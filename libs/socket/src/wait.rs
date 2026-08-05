//! 套接字阻塞等待原语。
//!
//! 提供条件等待 (`wait_while`) 和唤醒 (`wake_task`) 操作,
//! 支持超时截止时间和信号中断。

use alloc::sync::{Arc, Weak};

use sched::sync::Spinlock;
use sched::{
    Task, TaskState, WaitQueue, WaitQueueEntry, WaitReason, cancel_sleep_deadline, current_task,
    enqueue_task, is_ready, now_ns_direct, register_sleep_deadline, schedule_once,
};

use crate::types::SocketError;

/// Unix socket 状态变化时的 readiness 通知边界。
pub trait SocketReadinessObserver: Send + Sync {
    fn readiness_changed(&self);
}

pub(crate) struct SocketWaitQueue {
    queue: WaitQueue,
    observer: Spinlock<Option<Weak<dyn SocketReadinessObserver>>>,
}

impl SocketWaitQueue {
    pub const fn new() -> Self {
        Self {
            queue: WaitQueue::new_with_reason(WaitReason::SocketRead),
            observer: Spinlock::new(None),
        }
    }

    pub fn set_observer(&self, observer: Weak<dyn SocketReadinessObserver>) {
        *self.observer.lock() = Some(observer);
    }

    pub fn enqueue(&self, task: &Arc<Task>) {
        self.queue.enqueue(task);
    }

    pub fn prepare_to_wait(&self, task: &Arc<Task>) -> Arc<WaitQueueEntry> {
        self.queue.prepare_to_wait(task, TaskState::Sleeping)
    }

    pub fn finish_wait(&self, entry: &Arc<WaitQueueEntry>) {
        self.queue.finish_wait(entry);
    }

    pub fn remove(&self, task: &Arc<Task>) {
        self.queue.remove(task);
    }

    pub fn wake_one_with(&self, wake: impl Fn(&Arc<Task>)) -> Option<Arc<Task>> {
        self.notify_observer();
        self.queue.wake_one_with(wake)
    }

    pub fn wake_all_with(&self, wake: impl Fn(&Arc<Task>)) {
        self.notify_observer();
        self.queue.wake_all_with(wake);
    }

    fn notify_observer(&self) {
        let observer = self.observer.lock().as_ref().and_then(Weak::upgrade);
        if let Some(observer) = observer {
            observer.readiness_changed();
        }
    }
}

/// 唤醒等待队列中的一个任务(将其重新入调度器就绪队列)。
pub(crate) fn wake_task(task: &Arc<Task>) {
    if is_ready() && task.state() == TaskState::Runnable {
        enqueue_task(Arc::clone(task), now_ns_direct());
    }
}

/// 检查任务是否有待处理的非阻塞信号。
fn has_pending_signal(task: &Arc<Task>) -> bool {
    sched::operation::has_interrupting_signal(task)
}

/// 检查超时截止时间是否已过期。
fn deadline_expired(deadline: Option<u64>) -> bool {
    deadline.is_some_and(|dl| now_ns_direct() >= dl)
}

/// 条件等待:阻塞当前任务直到 `predicate` 返回 false、超时或收到信号。
///
/// 返回值:
/// - `Ok(())` — 条件已满足
/// - `Err(TemporaryUnavailable)` — 超时
/// - `Err(Interrupted)` — 被信号中断
pub(crate) fn wait_while(
    queue: &SocketWaitQueue,
    predicate: impl Fn() -> bool,
    deadline: Option<u64>,
) -> Result<(), SocketError> {
    loop {
        if !predicate() {
            return Ok(());
        }
        if deadline_expired(deadline) {
            return Err(SocketError::TemporaryUnavailable);
        }
        let task = current_task();
        let entry = queue.prepare_to_wait(&task);
        let deadline_armed =
            deadline.is_some_and(|deadline| register_sleep_deadline(&task, deadline));
        if !predicate() {
            if deadline_armed {
                cancel_sleep_deadline(&task);
            }
            queue.finish_wait(&entry);
            return Ok(());
        }
        if deadline_expired(deadline) {
            if deadline_armed {
                cancel_sleep_deadline(&task);
            }
            queue.finish_wait(&entry);
            return Err(SocketError::TemporaryUnavailable);
        }
        schedule_once(now_ns_direct());
        if deadline_armed {
            cancel_sleep_deadline(&task);
        }
        queue.finish_wait(&entry);
        if has_pending_signal(&task) {
            return Err(SocketError::Interrupted);
        }
        if deadline_expired(deadline) {
            return Err(SocketError::TemporaryUnavailable);
        }
    }
}
