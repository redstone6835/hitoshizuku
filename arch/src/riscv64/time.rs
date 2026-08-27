//! RISC-V64 时钟源：rdtime 读取、ns 转换与 SBI 定时器重装载。

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use super::csr::SIE_STIE;
use super::sbi;

/// 稳定计时器频率（Hz），由 DTB timebase-frequency 填充。
pub static STABLE_TIMER_HZ: AtomicUsize = AtomicUsize::new(10_000_000);

const NS_FIXED_SHIFT: u32 = 32;
// QEMU virt 的常见 10 MHz timebase 可精确化为 ticks * 100，不需要 128 位乘法。
static NS_FACTOR: AtomicU64 = AtomicU64::new(100);
static NS_SHIFT: AtomicUsize = AtomicUsize::new(0);
static EXACT_NS_PER_TICK: AtomicU64 = AtomicU64::new(100);

/// 设置稳定计时器频率，同时刷新 ns 转换的预算因子。
pub fn set_stable_counter_hz(hz: usize) {
    STABLE_TIMER_HZ.store(hz, Ordering::Relaxed);
    let hz = hz.max(1) as u128;
    let exact_ns_per_tick = (1_000_000_000u128 % hz == 0)
        .then_some((1_000_000_000u128 / hz) as u64)
        .unwrap_or(0);
    let (factor, shift) = if 1_000_000_000u128 % hz == 0 {
        ((1_000_000_000u128 / hz) as u64, 0)
    } else {
        (
            ((1_000_000_000u128 << NS_FIXED_SHIFT) / hz) as u64,
            NS_FIXED_SHIFT as usize,
        )
    };
    NS_FACTOR.store(factor, Ordering::Release);
    NS_SHIFT.store(shift, Ordering::Release);
    EXACT_NS_PER_TICK.store(exact_ns_per_tick, Ordering::Release);
}

/// 默认周期性调度 tick 频率。
pub const DEFAULT_TIMER_HZ: usize = 100;

static TIMER_HZ: AtomicUsize = AtomicUsize::new(DEFAULT_TIMER_HZ);
static TIMER_PERIOD_TICKS: AtomicU64 = AtomicU64::new(100_000);
static NEXT_TIMER_DEADLINES: [AtomicU64; sched::NR_CPUS] =
    [const { AtomicU64::new(0) }; sched::NR_CPUS];
static HAS_SSTC: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum SbiTimerBackend {
    Unknown = 0,
    Time = 1,
    Legacy = 2,
}

impl SbiTimerBackend {
    #[inline]
    fn load() -> Self {
        match SBI_TIMER_BACKEND.load(Ordering::Acquire) {
            value if value == Self::Time as u8 => Self::Time,
            value if value == Self::Legacy as u8 => Self::Legacy,
            _ => Self::Unknown,
        }
    }

    #[inline]
    fn store(self) {
        SBI_TIMER_BACKEND.store(self as u8, Ordering::Release);
    }
}

static SBI_TIMER_BACKEND: AtomicU8 = AtomicU8::new(SbiTimerBackend::Unknown as u8);
static SBI_TIMER_FALLBACK_REPORTED: AtomicBool = AtomicBool::new(false);

/// 由 loader 根据 DTB ISA 字符串发布 Sstc 可用性。
pub fn set_sstc_available(available: bool) {
    HAS_SSTC.store(available, Ordering::Release);
}

#[inline]
pub fn has_sstc() -> bool {
    HAS_SSTC.load(Ordering::Acquire)
}

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
    let shift = NS_SHIFT.load(Ordering::Acquire);
    let factor = NS_FACTOR.load(Ordering::Relaxed);
    if factor == 0 {
        return 0;
    }
    if shift == 0 {
        cnt.wrapping_mul(factor)
    } else {
        ((cnt as u128 * factor as u128) >> shift) as u64
    }
}

#[inline]
pub fn kernel_timestamp_ns() -> u64 {
    stable_counter_to_ns(stable_counter_raw())
}

#[inline]
fn arm_timer_at(deadline_ticks: u64) {
    if has_sstc() {
        unsafe {
            // stimecmp CSR = 0x14d。使用数值 CSR，避免依赖外部汇编器的扩展名解析。
            core::arch::asm!(
                "csrw 0x14d, {deadline}",
                deadline = in(reg) deadline_ticks,
                options(nomem, nostack, preserves_flags)
            );
        }
        return;
    }

    if SbiTimerBackend::load() != SbiTimerBackend::Legacy {
        let ret = sbi::set_timer(deadline_ticks);
        if ret.is_ok() {
            SbiTimerBackend::Time.store();
            return;
        }

        SbiTimerBackend::Legacy.store();
        if !SBI_TIMER_FALLBACK_REPORTED.swap(true, Ordering::AcqRel) {
            log::warning!(
                "[timer] SBI TIME set_timer failed (error={}); falling back to legacy EID 0",
                ret.error
            );
        }
    }

    sbi::legacy_set_timer(deadline_ticks);
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
    crate::set_csr!(sie, SIE_STIE);
    let deadline = stable_counter_raw().saturating_add(period);
    NEXT_TIMER_DEADLINES[current_timer_cpu()].store(deadline, Ordering::Release);
    arm_timer_at(deadline);
}

/// 在 timer interrupt handler 中重装下一次 tick。
pub fn rearm_periodic_timer() -> u64 {
    let period = TIMER_PERIOD_TICKS.load(Ordering::Relaxed).max(1);
    let now = stable_counter_raw();
    let deadline = &NEXT_TIMER_DEADLINES[current_timer_cpu()];
    let previous = deadline.load(Ordering::Acquire);
    let mut next = if previous == 0 {
        now.saturating_add(period)
    } else {
        previous.saturating_add(period)
    };

    if next <= now {
        let missed = now.saturating_sub(previous) / period + 1;
        next = previous.saturating_add(missed.saturating_mul(period));
        if next <= now {
            next = now.saturating_add(period);
        }
    }

    deadline.store(next, Ordering::Release);
    arm_timer_at(next);
    now
}

/// 按绝对纳秒截止时间重编程当前 CPU 的本地 timer。
///
/// 临时截止时间不会晚于常规调度 tick；撤销临时约束时从当前时刻恢复一个完整周期。
pub fn rearm_local_timer(deadline_ns: Option<u64>) {
    let period = TIMER_PERIOD_TICKS.load(Ordering::Relaxed).max(1);
    let now_ticks = stable_counter_raw();
    let now_ns = stable_counter_to_ns(now_ticks);
    let deadline_ticks = deadline_ns.map_or_else(
        || now_ticks.saturating_add(period),
        |deadline_ns| {
            let delta_ns = deadline_ns.saturating_sub(now_ns);
            let delta_ticks = if delta_ns == 0 {
                1
            } else if let exact_ns_per_tick = EXACT_NS_PER_TICK.load(Ordering::Acquire)
                && exact_ns_per_tick != 0
            {
                delta_ns.div_ceil(exact_ns_per_tick).clamp(1, period)
            } else {
                let hz = stable_counter_hz().max(1) as u128;
                ((delta_ns as u128 * hz).saturating_add(999_999_999) / 1_000_000_000)
                    .clamp(1, period as u128) as u64
            };
            now_ticks.saturating_add(delta_ticks)
        },
    );
    let programmed = &NEXT_TIMER_DEADLINES[current_timer_cpu()];
    if programmed.swap(deadline_ticks, Ordering::AcqRel) != deadline_ticks {
        arm_timer_at(deadline_ticks);
    }
}

#[inline]
fn current_timer_cpu() -> usize {
    crate::riscv64::specific::current_cpu_id().min(sched::NR_CPUS - 1)
}

#[inline]
pub fn timer_hz() -> usize {
    TIMER_HZ.load(Ordering::Relaxed)
}

#[inline]
pub fn timer_period_ticks() -> u64 {
    TIMER_PERIOD_TICKS.load(Ordering::Relaxed)
}
