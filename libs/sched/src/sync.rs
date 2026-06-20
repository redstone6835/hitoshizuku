//! sched 子系统内部自旋锁。
//!
//! 直接复用 VFS 中的设计思路：基于 `AtomicBool` 的 TATAS 自旋锁，足以保护
//! 调度器、等待队列、亲子关系等短临界区。调度器核心路径（`pick_next` /
//! `update_curr`）持锁时间极短；若将来需要长临界区，再引入睡眠锁。
//!
//! 使用约束：
//!
//! - 临界区内禁止触发可能再次取同一把锁的调用；
//! - 持有 runqueue 锁时不得调用可能分配 / 唤醒的函数；
//! - 中断上下文再持锁，需调用方自行关中断。

use core::cell::UnsafeCell;
use core::hint;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
    _not_send: PhantomData<*mut ()>,
}

impl<T> Deref for SpinlockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: 持有守卫即持有锁，当前核独占数据访问权。
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: 持有可变守卫即独占锁，可安全取可变引用。
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// Safety: T: Send 时 Spinlock<T> 对 data 的访问被 locked 严格序列化。
unsafe impl<T: Send> Sync for Spinlock<T> {}
unsafe impl<T: Send> Send for Spinlock<T> {}

impl<T> Spinlock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// 不获取锁直接读内部数据的引用。
    /// Safety: 调用方确保数据在此期间不会被其他线程修改。
    #[inline]
    pub unsafe fn get_unchecked(&self) -> &T {
        unsafe { &*self.data.get() }
    }

    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                hint::spin_loop();
            }
        }
        SpinlockGuard {
            lock: self,
            _not_send: PhantomData,
        }
    }

    pub fn try_lock(&self) -> Option<SpinlockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinlockGuard {
                lock: self,
                _not_send: PhantomData,
            })
    }
}
