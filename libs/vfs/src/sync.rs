//! VFS 层内部使用的自旋锁原语。
//!
//! 在 `no_std` 环境下，标准库的 `std::sync::Mutex` 不可用。这里提供一个基于
//! `AtomicBool` 的简单自旋锁（spinlock），足以保护 VFS 层的短临界区（如 inode
//! 元数据的读写、dentry 缓存的更新）。
//!
//! ### 使用注意
//!
//! 自旋锁在持锁期间会忙等（spin），不会主动让出 CPU。因此：
//! - **禁止在持锁期间休眠或阻塞**（如等待 I/O 完成）；
//! - **禁止在持锁期间调用可能再次获取同一把锁的函数**（死锁）；
//! - 对于需要长时间持锁的场景（如等待磁盘 I/O），应使用睡眠锁（sleep mutex），
//!   此处暂不提供，留待调度器就绪后扩展。
//!
//! 未来引入任务调度后，可将此模块替换为基于 futex 或信号量的睡眠锁，接口保持
//! 不变，上层代码无需修改。

use core::cell::UnsafeCell;
use core::hint;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// 基于 `AtomicBool` 的自旋锁。
///
/// `T` 是被保护的数据，存放在 [`UnsafeCell`] 中以允许通过共享引用修改其内容。
/// `Send + Sync` 约束：`T: Send` 保证数据可以在线程间转移；只要数据满足 `Send`，
/// 自旋锁本身也可以在多个核之间安全共享（`Sync`）。
///
/// # 中断安全
///
/// 此自旋锁不会自动禁用中断。若中断处理程序可能获取同一把锁，调用方必须
/// 在 `lock()` 前手动禁用中断，否则会导致死锁。
/// 对于仅在进程上下文中使用的锁（如 `FdTable`），无需额外处理。
///
/// # 公平性
///
/// 当前实现为非公平锁（test-and-test-and-set）。在高竞争场景下，某些核心
/// 可能长时间饥饿。若需公平性，应替换为 ticket lock 或 MCS lock。
pub struct Spinlock<T> {
    /// 锁状态：`false` = 未加锁，`true` = 已加锁。
    locked: AtomicBool,
    /// 受保护的数据。`UnsafeCell` 提供内部可变性，是 Rust 中构建锁原语的
    /// 标准方式，明确地向编译器声明"我知道这里可能存在别名，我自己保证安全"。
    data: UnsafeCell<T>,
}

// Safety: 只要 T 可以在线程间传递（Send），Spinlock<T> 就可以在多核间共享（Sync）。
// 对 data 的访问权被 locked 字段严格序列化，不存在数据竞争。
unsafe impl<T: Send> Sync for Spinlock<T> {}
unsafe impl<T: Send> Send for Spinlock<T> {}

impl<T> Spinlock<T> {
    /// 构造一个新的未加锁自旋锁，保护数据 `data`。
    ///
    /// 此函数为 `const fn`，可以用于初始化 `static` 变量（如 dentry 缓存）。
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// 获取锁，返回一个 [`SpinlockGuard`]，离开作用域时自动释放锁。
    ///
    /// 若锁已被其他核持有，当前核将在此自旋等待，直到锁被释放。
    /// 等待期间调用 `core::hint::spin_loop()`，在支持的架构上会发出
    /// pause/yield 提示，
    /// 降低总线争用，提高超线程系统的整体吞吐量。
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        // 使用 Acquire 语义获取锁：确保临界区内对受保护数据的读写操作，
        // 不会被编译器或 CPU 重排到锁获取之前。
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // 锁被其他核持有时，先以 Relaxed 轮询，避免频繁发出 LOCK 前缀
            // 指令（该指令会独占总线，影响其他核的缓存一致性）。
            while self.locked.load(Ordering::Relaxed) {
                hint::spin_loop();
            }
        }
        SpinlockGuard {
            lock: self,
            _not_send: PhantomData,
        }
    }

    /// 尝试获取锁，若锁已被占用则立即返回 `None`，不自旋等待。
    ///
    /// 适用于"有就取，没有就做其他事"的场景，避免在已知高竞争路径上浪费 CPU。
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

/// 自旋锁的 RAII 守卫，持有锁的期间提供对受保护数据的独占访问。
///
/// 实现 [`Deref`] 和 [`DerefMut`]，使调用方可以透明地访问受保护数据。
/// 实现 [`Drop`]，确保离开作用域时（包括 panic 展开时）自动释放锁。
pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
    /// `*mut ()` 不实现 `Send`，因此 `SpinlockGuard` 也不会自动实现 `Send`。
    /// 这防止锁守卫被转移到其他线程——若守卫被 Send 到另一个线程后 drop，
    /// 原线程可能仍在临界区内操作数据，导致数据竞争。
    _not_send: PhantomData<*mut ()>,
}

// SpinlockGuard 不能 Send（不能把守卫本身转移到另一个线程）。
// 也不能 Sync（&SpinlockGuard 不应跨线程共享，因为 deref_mut 通过 & 也可获取 &mut T）。
// *mut () 已经同时阻止了自动 Send 和 Sync 推导，无需额外标注。

impl<T> Deref for SpinlockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: 持有守卫即持有锁，当前只有一个核可以进入此处，不存在数据竞争。
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: 同上，持有可变守卫时独占锁，可以安全地获取可变引用。
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        // Release 语义：确保临界区内的所有写操作在释放锁之前对其他核可见，
        // 与 lock() 中的 Acquire 配对，形成完整的 acquire-release 同步对。
        self.lock.locked.store(false, Ordering::Release);
    }
}
