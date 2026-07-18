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

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};

use crate::sync::Spinlock;
use crate::task::{Task, TaskState, WaitReason};

/// 唤醒回调。调用方负责把 task 加入某个 runqueue 并可能触发 IPI。
///
/// 回调在**不持 WaitQueue 锁**时被调用，避免把 rq 锁反向耦合进来。
pub type WakeFn = fn(&Arc<Task>);

/// 等待队列。
pub struct WaitQueue {
    waiters: Spinlock<VecDeque<Weak<Task>>>,
    #[cfg(feature = "performance-profile")]
    reason: WaitReason,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self::new_with_reason(WaitReason::Other)
    }

    pub const fn new_with_reason(reason: WaitReason) -> Self {
        #[cfg(not(feature = "performance-profile"))]
        let _ = reason;
        Self {
            waiters: Spinlock::new(VecDeque::new()),
            #[cfg(feature = "performance-profile")]
            reason,
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
        waiters.push_back(Arc::downgrade(task));
    }

    /// 准备进入等待：先把当前任务标成睡眠态，再挂入等待队列。
    ///
    /// 调用方必须在 prepare 后重新检查条件；若条件已经满足，应立即
    /// [`finish_wait`]，不要调度出去。这个协议覆盖"事件发生在首次检查和
    /// 真正睡眠之间"的窗口。
    pub fn prepare_to_wait(&self, task: &Arc<Task>, state: TaskState) {
        debug_assert!(matches!(
            state,
            TaskState::Sleeping | TaskState::Uninterruptible
        ));
        // 先入队再切状态：若先切 Sleeping 再入队，waker 可能在入队前
        // 检查队列（空），唤醒丢失。
        self.enqueue(task);
        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(self.reason, crate::scheduler::now_ns_public());
        task.set_state(state);
    }

    /// 结束等待：从队列移除，并把仍处于睡眠态的任务恢复为可运行态。
    pub fn finish_wait(&self, task: &Arc<Task>) {
        self.remove(task);
        transition_from_wait(task);
        #[cfg(feature = "performance-profile")]
        task.cancel_profile_wait();
    }

    /// 等到 `condition` 为真。每次真正让出 CPU 前都会在已经登记到队列、
    /// 且任务处于 Sleeping 状态后重新检查一次条件。
    pub fn wait_event(&self, task: &Arc<Task>, mut condition: impl FnMut() -> bool) {
        while !condition() {
            self.prepare_to_wait(task, TaskState::Sleeping);
            if condition() {
                self.finish_wait(task);
                return;
            }
            crate::scheduler::schedule_once(crate::scheduler::now_ns_public());
            self.finish_wait(task);
        }
    }

    /// 从队列中显式移除某个任务。常见于被信号打断、提前超时等场景。
    pub fn remove(&self, task: &Arc<Task>) {
        let mut w = self.waiters.lock();
        w.retain(|weak| match weak.upgrade() {
            Some(t) => !Arc::ptr_eq(&t, task),
            None => false,
        });
    }

    /// 唤醒一个等待者。用 VecDeque 从队头取元素，避免 Vec::remove(0)
    /// 在 pipe/select 等高频等待路径上反复搬移整段数组。
    /// 返回被唤醒任务的 `Arc`，便于上层决定是否直接转入 runqueue。
    pub fn wake_one(&self, wake: WakeFn) -> Option<Arc<Task>> {
        let picked = {
            let mut w = self.waiters.lock();
            let initial_len = w.len();
            let mut pushed_back = 0usize;
            loop {
                let weak = match w.pop_front() {
                    None => break None,
                    Some(wk) => wk,
                };
                let Some(task) = weak.upgrade() else {
                    continue;
                };
                let st = task.state();
                if st != TaskState::Sleeping && st != TaskState::Uninterruptible {
                    if pushed_back < initial_len {
                        w.push_back(weak);
                        pushed_back += 1;
                        continue;
                    }
                    break None;
                }
                break Some(task);
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

    /// 使用默认调度器入口唤醒一个等待者。
    pub fn wake_one_default(&self) -> Option<Arc<Task>> {
        self.wake_one(default_wake)
    }

    /// 带回调的全量唤醒。
    pub fn wake_all_with(&self, wake: impl Fn(&Arc<Task>)) {
        let drained: VecDeque<Weak<Task>> = {
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
        let mut taken: VecDeque<Weak<Task>> = VecDeque::new();
        {
            let mut w = self.waiters.lock();
            while taken.len() < n && !w.is_empty() {
                if let Some(weak) = w.pop_front() {
                    taken.push_back(weak);
                }
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
                match w.pop_front() {
                    None => break None,
                    Some(weak) => {
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
    let transitioned = task.cas_state(TaskState::Sleeping, TaskState::Runnable)
        || task.cas_state(TaskState::Uninterruptible, TaskState::Runnable);
    if transitioned {
        #[cfg(feature = "performance-profile")]
        task.mark_profile_woken(crate::scheduler::now_ns_public());
    }
}

fn transition_from_wait(task: &Arc<Task>) {
    if crate::scheduler::is_ready() {
        let current = crate::scheduler::current_task();
        if Arc::ptr_eq(&current, task) {
            if !task.cas_state(TaskState::Sleeping, TaskState::Running)
                && !task.cas_state(TaskState::Uninterruptible, TaskState::Running)
            {
                let _ = task.cas_state(TaskState::Runnable, TaskState::Running);
            }
            return;
        }
    }
    transition_to_runnable(task);
}

fn default_wake(task: &Arc<Task>) {
    if crate::scheduler::is_ready() && task.state() == TaskState::Runnable {
        crate::scheduler::enqueue_task(Arc::clone(task), crate::scheduler::now_ns_public());
    }
}
