//! RISC-V64 时钟源：rdtime 读取、ns 转换与 SBI 定时器重装载。

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::set_csr;

use super::csr::SIE_STIE;

/// 稳定计时器频率（Hz），由 DTB timebase-frequency 填充。
pub static STABLE_TIMER_HZ: AtomicUsize = AtomicUsize::new(10_000_000);

const NS_FACTOR_SHIFT: u32 = 32;
static NS_FACTOR: AtomicU64 =
    AtomicU64::new(((1_000_000_000u128 << NS_FACTOR_SHIFT) / 10_000_000) as u64);

/// 设置稳定计时器频率，同时刷新 ns 转换的预算因子。
pub fn set_stable_counter_hz(hz: usize) {
    STABLE_TIMER_HZ.store(hz, Ordering::Relaxed);
    let hz = hz.max(1) as u128;
    let factor = ((1_000_000_000u128 << NS_FACTOR_SHIFT) / hz) as u64;
    NS_FACTOR.store(factor, Ordering::Release);
}

/// 默认周期性调度 tick 频率。
pub const DEFAULT_TIMER_HZ: usize = 100;

static TIMER_HZ: AtomicUsize = AtomicUsize::new(DEFAULT_TIMER_HZ);
static TIMER_PERIOD_TICKS: AtomicU64 = AtomicU64::new(100_000);

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
    let factor = NS_FACTOR.load(Ordering::Relaxed);
    if factor == 0 {
        return 0;
    }
    ((cnt as u128 * factor as u128) >> NS_FACTOR_SHIFT) as u64
}

#[inline]
pub fn kernel_timestamp_ns() -> u64 {
    stable_counter_to_ns(stable_counter_raw())
}

#[inline]
fn sbi_set_timer(stime_value: u64) -> isize {
    const SBI_EXT_TIME: usize = 0x5449_4d45;
    const SBI_TIME_SET_TIMER: usize = 0;

    let error: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") stime_value as usize => error,
            in("a6") SBI_TIME_SET_TIMER,
            in("a7") SBI_EXT_TIME,
            lateout("a1") _,
            lateout("a2") _,
            lateout("a3") _,
            lateout("a4") _,
            lateout("a5") _,
            options(nostack)
        );
    }
    error as isize
}

#[inline]
fn arm_timer_at(deadline_ticks: u64) {
    let _ = sbi_set_timer(deadline_ticks);
}

/// 根据稳定计时器频率配置周期 tick。
///
/// 这里只设置 `sie.STIE` 和下一次 compare 值；全局 `sstatus.SIE` 仍由调度器
/// idle/用户返回路径控制，避免在启动早期未准备好时直接响应中断。
pub fn init_periodic_timer(timer_hz: usize) {
    let timer_hz = timer_hz.clamp(1, 10_000);
    let stable_hz = stable_counter_hz().max(1);
    let period = (stable_hz / timer_hz as u64).max(1);

    TIMER_HZ.store(timer_hz, Ordering::Relaxed);
    TIMER_PERIOD_TICKS.store(period, Ordering::Relaxed);
    set_csr!(sie, SIE_STIE);
    arm_timer_at(stable_counter_raw().saturating_add(period));
}

/// 在 timer interrupt handler 中重装下一次 tick。
pub fn rearm_periodic_timer() {
    let period = TIMER_PERIOD_TICKS.load(Ordering::Relaxed).max(1);
    arm_timer_at(stable_counter_raw().saturating_add(period));
}

#[inline]
pub fn timer_hz() -> usize {
    TIMER_HZ.load(Ordering::Relaxed)
}

#[inline]
pub fn timer_period_ticks() -> u64 {
    TIMER_PERIOD_TICKS.load(Ordering::Relaxed)
}
