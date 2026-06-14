//! RISC-V64 时钟源：rdtime 读取与 ns 转换。

use core::sync::atomic::{AtomicUsize, Ordering};

/// 稳定计时器频率（Hz），由 DTB timebase-frequency 填充。
pub static STABLE_TIMER_HZ: AtomicUsize = AtomicUsize::new(10_000_000);

#[inline]
pub fn stable_counter_raw() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("rdtime {}", out(reg) v, options(nomem, nostack)) };
    v
}

#[inline]
pub fn stable_counter_hz() -> u64 {
    STABLE_TIMER_HZ.load(Ordering::Relaxed) as u64
}

#[inline]
pub fn stable_counter_to_ns(cnt: u64) -> u64 {
    let hz = stable_counter_hz();
    if hz == 0 { return 0; }
    // 用 u128 中间值避免 (cnt % hz) * 1e9 溢出
    let secs = cnt / hz;
    let rem = cnt % hz;
    secs * 1_000_000_000 + (rem as u128 * 1_000_000_000 / hz as u128) as u64
}

#[inline]
pub fn kernel_timestamp_ns() -> u64 {
    stable_counter_to_ns(stable_counter_raw())
}
