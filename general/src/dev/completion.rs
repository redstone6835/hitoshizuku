//! 通用完成变量。
//!
//! `Completion<T>` 是块设备同步/异步接口的核心枢纽：
//! - 同步路径调 `.wait()` 阻塞当前任务（调度器就绪时用 WaitQueue，否则 spin）。
//! - 异步路径注册 `Waker`，由 `.complete()` 触发唤醒。
//! - 驱动完成请求时先 drop 自己的锁，再调 `bio.complete()`，彻底避免重入死锁。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};

use sched::WaitQueue;
use vfs::sync::Spinlock;

/// 通用完成变量。
///
/// 生产者调用 `complete(value)` 存入结果并唤醒所有等待者；
/// 消费者通过 `wait()`（同步）或 `poll()`（Future）获取结果。
pub struct Completion<T> {
    done: AtomicBool,
    result: Spinlock<Option<T>>,
    waker: Spinlock<Option<Waker>>,
    wait_queue: WaitQueue,
}

impl<T> Completion<T> {
    pub const fn new_detached() -> Self {
        Self {
            done: AtomicBool::new(false),
            result: Spinlock::new(None),
            waker: Spinlock::new(None),
            wait_queue: WaitQueue::new(),
        }
    }

    pub fn new() -> Arc<Self> {
        Arc::new(Self::new_detached())
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// 标记完成，存储结果，唤醒所有等待者。
    pub fn complete(&self, value: T) {
        *self.result.lock() = Some(value);
        self.done.store(true, Ordering::Release);
        if let Some(w) = self.waker.lock().take() {
            w.wake();
        }
        // Completion<T> 当前是单消费者原语；T 不要求 Clone，不能把同一结果发给多个 waiter。
        self.wait_queue.wake_one_with(|task| {
            if sched::is_ready() && task.state() == sched::TaskState::Runnable {
                sched::enqueue_task(Arc::clone(task), sched::now_ns_public());
            }
        });
    }

    /// 同步阻塞等待结果。调度器就绪时用 WaitQueue 让出 CPU；否则 spin。
    pub fn wait(&self) -> T {
        if sched::is_ready() {
            self.wait_blocking()
        } else {
            self.wait_spinning()
        }
    }

    /// 供 Future::poll 使用。
    pub fn poll(&self, cx: &mut Context<'_>) -> Poll<T> {
        if self.done.load(Ordering::Acquire) {
            return self
                .result
                .lock()
                .take()
                .map(Poll::Ready)
                .unwrap_or(Poll::Pending);
        }
        *self.waker.lock() = Some(cx.waker().clone());
        // 二次检查：避免注册和完成之间的竞争
        if self.done.load(Ordering::Acquire) {
            self.result
                .lock()
                .take()
                .map(Poll::Ready)
                .unwrap_or(Poll::Pending)
        } else {
            Poll::Pending
        }
    }

    fn wait_blocking(&self) -> T {
        loop {
            if self.done.load(Ordering::Acquire) {
                if let Some(result) = self.result.lock().take() {
                    return result;
                }
            }
            let task = sched::current_task();
            let _ = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping);
            let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);
            self.wait_queue.enqueue(&task);
            if self.done.load(Ordering::Acquire) {
                self.wait_queue.remove(&task);
                if let Some(result) = self.result.lock().take() {
                    restore_current_after_wait(&task);
                    return result;
                }
            }
            drop(task);
            sched::schedule_once(sched::now_ns_public());
            let task = sched::current_task();
            self.wait_queue.remove(&task);
            restore_current_after_wait(&task);
        }
    }

    fn wait_spinning(&self) -> T {
        loop {
            if self.done.load(Ordering::Acquire) {
                if let Some(result) = self.result.lock().take() {
                    return result;
                }
            }
            core::hint::spin_loop();
        }
    }
}

fn restore_current_after_wait(task: &Arc<sched::Task>) {
    if !task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running) {
        let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
    }
}
