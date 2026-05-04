//! 通用等待队列。
//!
//! [`WaitQueue`] 是调度器级别的通知原语：任务把自身的 [`Weak<Task>`] 注册到
//! 某个事件源上，事件发生时 upgrade 这些弱引用并将对应任务从 `Sleeping`
//! 推回 `Runnable`。默认唤醒路径会走 sched 的统一入队入口；需要绑定特殊事件
//! 源时也可以通过 `wake_fn` 注入额外动作。
//!
//! 用 `Weak<Task>` 而非 `Arc<Task>` 的原因：
//!
//! - 等待队列不应保活任务，否则 `exit_waiters` 这类"任务自己等自己"的场景
//!   会永远无法 drop；
//! - 已死任务的 upgrade 会自动失败，遍历时顺手清掉即可，不需要显式 "unregister"。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::sync::Spinlock;
use crate::task::{Task, TaskState};

/// 唤醒回调。调用方负责把 task 加入某个 runqueue 并可能触发 IPI。
///
/// 回调在**不持 WaitQueue 锁**时被调用，避免把 rq 锁反向耦合进来。
pub type WakeFn = fn(&Arc<Task>);

/// 等待队列。
pub struct WaitQueue {
    waiters: Spinlock<Vec<Weak<Task>>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            waiters: Spinlock::new(Vec::new()),
        }
    }

    /// 把任务挂到队列上，调用方随后应把自己置 `Sleeping` 并让出 CPU。
    ///
    /// 允许同一任务多次挂入多个队列（典型：`poll`）；每个队列只记一条 Weak。
    pub fn enqueue(&self, task: &Arc<Task>) {
        let mut waiters = self.waiters.lock();
        waiters.retain(|weak| weak.upgrade().is_some());
        if waiters.iter().any(|weak| {
            weak.upgrade()
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, task))
        }) {
            return;
        }
        waiters.push(Arc::downgrade(task));
    }

    /// 从队列中显式移除某个任务。常见于被信号打断、提前超时等场景。
    pub fn remove(&self, task: &Arc<Task>) {
        let mut w = self.waiters.lock();
        w.retain(|weak| match weak.upgrade() {
            Some(t) => !Arc::ptr_eq(&t, task),
            None => false,
        });
    }

    /// 唤醒一个等待者。清理 upgrade 失败的条目后，取出首个有效 Weak。
    /// 返回被唤醒任务的 `Arc`，便于上层决定是否直接转入 runqueue。
    pub fn wake_one(&self, wake: WakeFn) -> Option<Arc<Task>> {
        let picked = {
            let mut w = self.waiters.lock();
            loop {
                let front = w.first().cloned();
                match front {
                    None => break None,
                    Some(weak) => {
                        w.remove(0);
                        if let Some(task) = weak.upgrade() {
                            break Some(task);
                        }
                        // upgrade 失败：任务已死，继续找下一个。
                    }
                }
            }
        };
        if let Some(ref task) = picked {
            transition_to_runnable(task);
            wake(task);
        }
        picked
    }

    /// 唤醒所有等待者。先把列表整取出来，锁外逐个调用 wake，防止反序。
    pub fn wake_all(&self) {
        self.wake_all_with(default_wake);
    }

    /// 带回调的全量唤醒。
    pub fn wake_all_with(&self, wake: impl Fn(&Arc<Task>)) {
        let drained: Vec<Weak<Task>> = {
            let mut w = self.waiters.lock();
            core::mem::take(&mut *w)
        };
        for weak in drained {
            if let Some(task) = weak.upgrade() {
                transition_to_runnable(&task);
                wake(&task);
            }
        }
    }

    /// 唤醒至多 `n` 个等待者；返回实际成功唤醒的数量。
    pub fn wake_n(&self, n: usize, wake: impl Fn(&Arc<Task>)) -> usize {
        if n == 0 {
            return 0;
        }
        let mut taken: Vec<Weak<Task>> = Vec::new();
        {
            let mut w = self.waiters.lock();
            while taken.len() < n && !w.is_empty() {
                taken.push(w.remove(0));
            }
        }
        let mut woken = 0usize;
        for weak in taken {
            if let Some(task) = weak.upgrade() {
                transition_to_runnable(&task);
                wake(&task);
                woken += 1;
            }
        }
        woken
    }

    /// 唤醒一个等待者，调用 `wake` 回调（区别于 [`wake_one`] 接受 `fn` 的版本）。
    pub fn wake_one_with(&self, wake: impl Fn(&Arc<Task>)) -> Option<Arc<Task>> {
        let picked = {
            let mut w = self.waiters.lock();
            loop {
                let front = w.first().cloned();
                match front {
                    None => break None,
                    Some(weak) => {
                        w.remove(0);
                        if let Some(task) = weak.upgrade() {
                            break Some(task);
                        }
                    }
                }
            }
        };
        if let Some(ref task) = picked {
            transition_to_runnable(task);
            wake(task);
        }
        picked
    }

    /// 当前等待者数量（含已死 Weak，读路径不做清理避免写锁）。
    pub fn len_hint(&self) -> usize {
        self.waiters.lock().len()
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 Sleeping / Uninterruptible 切回 Runnable。CAS 失败说明任务已经
/// 处于其他状态（例如刚被另一路径唤醒），跳过即可。
fn transition_to_runnable(task: &Arc<Task>) {
    if !task.cas_state(TaskState::Sleeping, TaskState::Runnable) {
        let _ = task.cas_state(TaskState::Uninterruptible, TaskState::Runnable);
    }
}

fn default_wake(task: &Arc<Task>) {
    if crate::scheduler::is_ready() && task.state() == TaskState::Runnable {
        crate::scheduler::enqueue_task(Arc::clone(task), crate::scheduler::now_ns_public());
    }
}
