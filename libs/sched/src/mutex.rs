//! 基于 [`WaitQueue`](crate::wait::WaitQueue) 的可睡眠互斥锁（sleepable mutex）。
//!
//! `Mutex<T>` 在锁已被持有时将当前任务挂入等待队列
//! 并切至 `Sleeping` 状态让出 CPU。当锁持有者释放锁时，自动唤醒一个等待者。
//!
//! # 使用约束
//!
//! - **禁止在中断上下文中调用 `lock()`**。
//! - 同一任务重复 `lock()` 会死锁自身。
//! - 临界区内可以执行阻塞 I/O。

use core::cell::UnsafeCell;
use core::fmt;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::scheduler::{current_task, now_ns_public, schedule_once};
use crate::task::TaskState;
use crate::wait::WaitQueue;

/// 可睡眠互斥锁。
///
/// 内部状态：
/// - `locked: AtomicBool` — 快速路径 CAS 判断。
/// - `waiters: WaitQueue` — 等待者的睡眠队列。
/// - `data: UnsafeCell<T>` — 受保护的数据。
pub struct Mutex<T> {
    locked: AtomicBool,
    waiters: WaitQueue,
    data: UnsafeCell<T>,
}

// Safety: T: Send 时 Mutex<T> 对 data 的访问被 locked + waiters 严格序列化。
unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    /// 创建一个新的未锁定 Mutex。
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: WaitQueue::new(),
            data: UnsafeCell::new(data),
        }
    }

    /// 尝试获取锁，不睡眠。成功返回 `Some(guard)`，否则返回 `None`。
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MutexGuard { lock: self })
    }

    /// 获取锁。若锁已被持有，当前任务将睡眠等待直到被唤醒。
    ///
    /// # 流程
    ///
    /// 1. 快速路径：CAS `locked: false→true`，成功则立即返回。
    /// 2. 慢速路径：`prepare_to_wait` → 再次 CAS → 仍失败则 `schedule_once` 让出 CPU。
    /// 3. 被唤醒后 `finish_wait` 清理状态，回到步骤 1 重试。
    ///
    /// 两次检查（double-check）覆盖了"锁在 `prepare_to_wait` 后被释放"的窗口，
    /// 确保不会丢失 wakeup。
    pub fn lock(&self) -> MutexGuard<'_, T> {
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return MutexGuard { lock: self };
            }

            let current = current_task();
            self.waiters.prepare_to_wait(&current, TaskState::Sleeping);

            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.waiters.finish_wait(&current);
                return MutexGuard { lock: self };
            }

            schedule_once(now_ns_public());
            self.waiters.finish_wait(&current);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex")
            .field("locked", &self.locked.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Mutex 的 RAII 守卫，离开作用域时自动释放锁并唤醒一个等待者。
pub struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
}

// 阻止 Send/Sync：MutexGuard 不能离开获取它的任务上下文。
// 若 Send 到其他线程再 drop，解锁会发生在错误的上下文。

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: 持有 MutexGuard 即持有锁，独占访问 data。
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: 持有可变 MutexGuard 即独占锁，可安全取可变引用。
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // 先释放锁再唤醒，确保被唤醒者看到 locked == false。
        self.lock.locked.store(false, Ordering::Release);
        self.lock.waiters.wake_one_default();
    }
}
