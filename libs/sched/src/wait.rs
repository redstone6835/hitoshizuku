//! 通用等待队列。
//!
//! [`WaitQueue`] 是调度器级别的通知原语：任务通过 [`WaitQueueEntry`] 注册到
//! 某个事件源上，事件发生时等待队列将对应任务从 `Sleeping` 推回 `Runnable`。
//! 默认唤醒路径会走 sched 的统一入队入口；需要绑定特殊事件源时也可以通过
//! `wake_fn` 注入额外动作。
//!
//! 用 `Weak<Task>` 而非 `Arc<Task>` 的原因：
//!
//! - 等待队列不应保活任务，否则 `exit_waiters` 这类"任务自己等自己"的场景
//!   会永远无法 drop；
//! - 已死任务的 upgrade 会自动失败，遍历时顺手清掉即可，不需要显式 "unregister"。

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU8, Ordering};

use crate::sync::Spinlock;
use crate::task::{Task, TaskState};

/// 唤醒回调。调用方负责把 task 加入某个 runqueue 并可能触发 IPI。
///
/// 回调在**不持 WaitQueue 锁**时被调用，避免把 rq 锁反向耦合进来。
pub type WakeFn = fn(&Arc<Task>);

const ENTRY_WAITING: u8 = 0;
const ENTRY_WOKEN: u8 = 1;
const ENTRY_FINISHED: u8 = 2;

/// 等待队列中的单个任务登记项。
pub struct WaitQueueEntry {
    task: Weak<Task>,
    state: AtomicU8,
}

impl WaitQueueEntry {
    fn new(task: &Arc<Task>) -> Arc<Self> {
        Arc::new(Self {
            task: Arc::downgrade(task),
            state: AtomicU8::new(ENTRY_WAITING),
        })
    }

    fn task(&self) -> Option<Arc<Task>> {
        self.task.upgrade()
    }

    fn is_waiting(&self) -> bool {
        self.state.load(Ordering::Acquire) == ENTRY_WAITING
    }

    fn mark_woken(&self) -> bool {
        self.state
            .compare_exchange(
                ENTRY_WAITING,
                ENTRY_WOKEN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish(&self) {
        self.state.store(ENTRY_FINISHED, Ordering::Release);
    }
}

/// 等待队列。
pub struct WaitQueue {
    waiters: Spinlock<VecDeque<Arc<WaitQueueEntry>>>,
}

#[kernel_symbols::export]
impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            waiters: Spinlock::new(VecDeque::new()),
        }
    }

    /// 把任务登记到队列上，不改变任务状态。
    ///
    /// 该接口用于 `poll` 等只登记通知对象的路径。阻塞当前任务应使用
    /// [`prepare_to_wait`](Self::prepare_to_wait)。同一任务可以登记到多个队列，
    /// 但在同一个队列中只保留一个有效条目。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.enqueue",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_TASK,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn enqueue(&self, task: &Arc<Task>) -> Arc<WaitQueueEntry> {
        let mut waiters = self.waiters.lock();
        waiters.retain(|entry| entry.is_waiting() && entry.task().is_some());
        if let Some(entry) = waiters.iter().find(|entry| {
            entry
                .task()
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, task))
        }) {
            return Arc::clone(entry);
        }
        let entry = WaitQueueEntry::new(task);
        waiters.push_back(Arc::clone(&entry));
        entry
    }

    /// 准备进入等待：登记任务并把它切换到指定睡眠态。
    ///
    /// 调用方必须在 prepare 后重新检查条件；若条件已经满足，应立即
    /// [`finish_wait`]，不要调度出去。这个协议覆盖"事件发生在首次检查和
    /// 真正睡眠之间"的窗口。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.prepare_to_wait",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_TASK,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn prepare_to_wait(&self, task: &Arc<Task>, state: TaskState) -> Arc<WaitQueueEntry> {
        debug_assert!(matches!(
            state,
            TaskState::Sleeping | TaskState::Uninterruptible
        ));
        let mut waiters = self.waiters.lock();
        waiters.retain(|entry| entry.is_waiting() && entry.task().is_some());
        let entry = waiters
            .iter()
            .find(|entry| {
                entry
                    .task()
                    .as_ref()
                    .is_some_and(|queued| Arc::ptr_eq(queued, task))
            })
            .cloned()
            .unwrap_or_else(|| {
                let entry = WaitQueueEntry::new(task);
                waiters.push_back(Arc::clone(&entry));
                entry
            });
        // 状态切换与登记由同一把锁序列化。waker 要么先完成且调用方随后重新
        // 检查条件，要么在这里之后看到 Sleeping/Uninterruptible。
        task.set_state(state);
        entry
    }

    /// 结束等待：从队列移除，并把仍处于睡眠态的任务恢复为可运行态。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.finish_wait",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_TASK,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn finish_wait(&self, entry: &Arc<WaitQueueEntry>) {
        {
            let mut waiters = self.waiters.lock();
            waiters.retain(|queued| !Arc::ptr_eq(queued, entry));
            entry.finish();
        }
        if let Some(task) = entry.task() {
            transition_from_wait(&task);
        }
    }

    /// 等到 `condition` 为真。每次真正让出 CPU 前都会在已经登记到队列、
    /// 且任务处于 Sleeping 状态后重新检查一次条件。
    pub fn wait_event(&self, task: &Arc<Task>, mut condition: impl FnMut() -> bool) {
        while !condition() {
            let entry = self.prepare_to_wait(task, TaskState::Sleeping);
            if condition() {
                self.finish_wait(&entry);
                return;
            }
            crate::scheduler::schedule_once(crate::scheduler::now_ns_public());
            self.finish_wait(&entry);
        }
    }

    /// 从队列中显式移除某个任务。常见于被信号打断、提前超时等场景。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.remove",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_TASK,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn remove(&self, task: &Arc<Task>) {
        let mut w = self.waiters.lock();
        w.retain(|entry| match entry.task() {
            Some(queued) if Arc::ptr_eq(&queued, task) => {
                entry.finish();
                false
            }
            Some(_) => entry.is_waiting(),
            None => false,
        });
    }

    /// 唤醒一个等待者。用 VecDeque 从队头取元素，避免 Vec::remove(0)
    /// 在 pipe/select 等高频等待路径上反复搬移整段数组。
    /// 返回被唤醒任务的 `Arc`，便于上层决定是否直接转入 runqueue。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.wake_one",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_TASK,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn wake_one(&self, wake: WakeFn) -> Option<Arc<Task>> {
        let picked = {
            let mut w = self.waiters.lock();
            let initial_len = w.len();
            let mut picked = None;
            for _ in 0..initial_len {
                let Some(entry) = w.pop_front() else {
                    break;
                };
                let Some(task) = entry.task() else {
                    continue;
                };
                let st = task.state();
                if st != TaskState::Sleeping && st != TaskState::Uninterruptible {
                    w.push_back(entry);
                    continue;
                }
                if entry.mark_woken() {
                    picked = Some(task);
                    break;
                }
            }
            picked
        };
        if let Some(ref task) = picked {
            transition_to_runnable(task);
            wake(task);
        }
        picked
    }

    /// 唤醒所有等待者。先把列表整取出来，锁外逐个调用 wake，防止反序。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.wake_all",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_TASK,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn wake_all(&self) {
        self.wake_all_with(default_wake);
    }

    /// 使用默认调度器入口唤醒一个等待者。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.wake_one_default",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_TASK,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn wake_one_default(&self) -> Option<Arc<Task>> {
        self.wake_one(default_wake)
    }

    /// 带回调的全量唤醒。
    pub fn wake_all_with(&self, wake: impl Fn(&Arc<Task>)) {
        let tasks: VecDeque<Arc<Task>> = {
            let mut w = self.waiters.lock();
            let drained = core::mem::take(&mut *w);
            drained
                .into_iter()
                .filter_map(|entry| {
                    let task = entry.task()?;
                    entry.mark_woken().then_some(task)
                })
                .collect()
        };
        for task in tasks {
            transition_to_runnable(&task);
            wake(&task);
        }
    }

    /// 唤醒至多 `n` 个等待者；返回实际成功唤醒的数量。
    pub fn wake_n(&self, n: usize, wake: impl Fn(&Arc<Task>)) -> usize {
        if n == 0 {
            return 0;
        }
        let tasks: VecDeque<Arc<Task>> = {
            let mut w = self.waiters.lock();
            let mut tasks = VecDeque::new();
            while tasks.len() < n && !w.is_empty() {
                if let Some(entry) = w.pop_front() {
                    let Some(task) = entry.task() else {
                        continue;
                    };
                    if entry.mark_woken() {
                        tasks.push_back(task);
                    }
                }
            }
            tasks
        };
        let mut woken = 0usize;
        for task in tasks {
            transition_to_runnable(&task);
            wake(&task);
            woken += 1;
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
                    Some(entry) => {
                        if entry.mark_woken()
                            && let Some(task) = entry.task()
                        {
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

    /// 当前队列中的登记项数量。该值只用于诊断和测试。
    #[kernel_symbols::export(
        name = "sched.wait.WaitQueue.len_hint",
        contract = "kernel.sched.wait@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_QUERY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
    )]
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
