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

use crate::arch_hooks::{self, LocalInterruptGuard};

const URGENT_POLL_INTERVAL: usize = 1024;

#[inline]
fn spin_until_unlocked(locked: &AtomicBool, mut poll_urgent: impl FnMut()) {
    let mut spins = 0usize;
    while locked.load(Ordering::Relaxed) {
        hint::spin_loop();
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(URGENT_POLL_INTERVAL) {
            poll_urgent();
        }
    }
    #[cfg(feature = "performance-profile")]
    {
        let checks = spins / URGENT_POLL_INTERVAL;
        if checks != 0 {
            crate::arch_hooks::record_urgent_spin_checks(
                crate::scheduler::current_cpu_id(),
                checks,
            );
        }
    }
}

pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
    _not_send: PhantomData<*mut ()>,
}

/// 持锁期间保持本地中断关闭，并在解锁后恢复进入前的中断状态。
pub struct IrqSpinlockGuard<'a, T> {
    // 字段按声明顺序析构：必须先释放数据锁，再恢复本地中断。
    guard: SpinlockGuard<'a, T>,
    _interrupt_guard: LocalInterruptGuard,
}

impl<T> Deref for IrqSpinlockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for IrqSpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
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

    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_until_unlocked(&self.locked, || {
                if crate::urgent_work_pending() {
                    crate::poll_urgent_work();
                }
            });
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

    fn lock_irqsave(&self) -> IrqSpinlockGuard<'_, T> {
        let interrupt_guard = arch_hooks::disable_local_interrupts();
        let guard = self.lock();
        IrqSpinlockGuard {
            guard,
            _interrupt_guard: interrupt_guard,
        }
    }

    #[cfg(test)]
    fn lock_irqsave_with(
        &self,
        ops: Option<&arch_hooks::ArchLocalInterruptOps>,
    ) -> IrqSpinlockGuard<'_, T> {
        let interrupt_guard = LocalInterruptGuard::with_ops(ops);
        let guard = self.lock();
        IrqSpinlockGuard {
            guard,
            _interrupt_guard: interrupt_guard,
        }
    }
}

/// 只能通过 irq-save 语义获取的自旋锁，供中断可达的数据结构使用。
pub struct IrqSpinlock<T> {
    inner: Spinlock<T>,
}

// Safety: 所有数据访问都由 inner 串行化，中断状态只影响当前 CPU。
unsafe impl<T: Send> Sync for IrqSpinlock<T> {}
unsafe impl<T: Send> Send for IrqSpinlock<T> {}

impl<T> IrqSpinlock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            inner: Spinlock::new(data),
        }
    }

    pub fn lock(&self) -> IrqSpinlockGuard<'_, T> {
        self.inner.lock_irqsave()
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::{Spinlock, spin_until_unlocked};
    use crate::arch_hooks::ArchLocalInterruptOps;

    static TEST_IRQ_LOCK: Spinlock<()> = Spinlock::new(());
    static INTERRUPTS_ENABLED: AtomicBool = AtomicBool::new(true);
    static LOCK_RELEASED_BEFORE_RESTORE: AtomicBool = AtomicBool::new(false);

    fn save_and_disable_interrupts() -> usize {
        usize::from(INTERRUPTS_ENABLED.swap(false, Ordering::AcqRel))
    }

    fn restore_interrupts(state: usize) {
        LOCK_RELEASED_BEFORE_RESTORE.store(TEST_IRQ_LOCK.try_lock().is_some(), Ordering::Release);
        INTERRUPTS_ENABLED.store(state != 0, Ordering::Release);
    }

    static TEST_INTERRUPT_OPS: ArchLocalInterruptOps = ArchLocalInterruptOps {
        save_and_disable: save_and_disable_interrupts,
        restore: restore_interrupts,
    };

    #[test]
    fn contended_wait_polls_urgent_work_periodically() {
        let locked = AtomicBool::new(true);
        let polls = AtomicUsize::new(0);

        spin_until_unlocked(&locked, || {
            if polls.fetch_add(1, Ordering::Relaxed) == 2 {
                locked.store(false, Ordering::Release);
            }
        });

        assert_eq!(polls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn irqsave_guard_restores_interrupts_after_unlock() {
        INTERRUPTS_ENABLED.store(true, Ordering::Release);
        LOCK_RELEASED_BEFORE_RESTORE.store(false, Ordering::Release);

        {
            let _guard = TEST_IRQ_LOCK.lock_irqsave_with(Some(&TEST_INTERRUPT_OPS));
            assert!(!INTERRUPTS_ENABLED.load(Ordering::Acquire));
            assert!(TEST_IRQ_LOCK.try_lock().is_none());
        }

        assert!(INTERRUPTS_ENABLED.load(Ordering::Acquire));
        assert!(LOCK_RELEASED_BEFORE_RESTORE.load(Ordering::Acquire));
    }
}
