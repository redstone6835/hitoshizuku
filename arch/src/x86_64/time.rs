//! x86_64 时间源与 TSC 校准。
//!
//! Linux 在 x86 上优先使用 invariant TSC，并通过 CPUID leaf 0x15/0x16 或
//! 固件提供的频率建立 `cyc2ns` 换算。这里保留相同的分层：原始计数器只在
//! arch 读取，通用调度器只看到单调纳秒和频率回调。没有可靠 CPUID 比率时
//! 使用保守的 1 GHz 软件默认值，并明确暴露 `calibration_source`，避免把
//! 猜测值误当成硬件校准结果。

use core::arch::x86_64::__cpuid_count;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use super::apic;
use super::specific::{rdtsc, rdtsc_ordered};
#[cfg(target_os = "none")]
use super::trap;

pub const DEFAULT_TSC_HZ: u64 = 1_000_000_000;
pub const MIN_TSC_HZ: u64 = 100_000;
pub const MAX_TSC_HZ: u64 = 10_000_000_000;

// LAPIC timer register offsets and encodings.  Both the hardware registers and
// the programming guard are per-CPU; sharing either state would let one CPU
// suppress another CPU's deadline update during concurrent scheduling.
const LAPIC_LVT_TIMER: usize = 0x320;
const LAPIC_TIMER_INITIAL_COUNT: usize = 0x380;
const LAPIC_TIMER_DIVIDE_CONFIG: usize = 0x3e0;
const LAPIC_TIMER_MASK: u32 = 1 << 16;
const LAPIC_TIMER_MODE_MASK: u32 = 0b11 << 17;
const LAPIC_TIMER_MODE_ONE_SHOT: u32 = 0;
const LAPIC_TIMER_MODE_TSC_DEADLINE: u32 = 0b10 << 17;
const LAPIC_TIMER_DIVIDE_BY_16: u32 = 0b0011;
// IA32_TSC_DEADLINE is in the architectural (non-C000) MSR range.
const MSR_TSC_DEADLINE: u32 = 0x0000_06e0;
const CPUID_TSC_DEADLINE: u32 = 1 << 24;
/// Keep a regular scheduler tick even when no software deadline is pending.
pub const DEFAULT_TIMER_PERIOD_NS: u64 = 10_000_000;
/// LAPIC bus frequency is not architecturally enumerated.  This is the
/// conservative QEMU/PC fallback; a platform may replace it through
/// [`set_lapic_timer_frequency`] after calibrating against HPET/PIT.
const DEFAULT_LAPIC_TICK_HZ: u64 = 6_250_000; // 100 MHz input / divide-by-16
const MIN_LAPIC_TICK_HZ: u64 = 1_000;
const MAX_LAPIC_TICK_HZ: u64 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LocalTimerBackend {
    /// No LAPIC mapping has been published yet.
    Uninitialized = 0,
    /// LAPIC TSC-deadline mode (CPUID.1:ECX.TSC_DEADLINE).
    TscDeadline = 1,
    /// LAPIC one-shot/countdown mode used when TSC deadline or calibration is
    /// unavailable.  The count is always bounded to a 32-bit initial value.
    LapicOneShot = 2,
    /// A mapped LAPIC was rejected or hardware programming failed.
    Unavailable = 3,
    /// Hosted builds deliberately never touch APIC/MSR state.
    Hosted = 4,
}

static LOCAL_TIMER_BACKEND: [AtomicU8; super::smp::MAX_CPUS] =
    [const { AtomicU8::new(LocalTimerBackend::Uninitialized as u8) }; super::smp::MAX_CPUS];
static LAPIC_TIMER_TICK_HZ: AtomicU64 = AtomicU64::new(DEFAULT_LAPIC_TICK_HZ);
static TIMER_PROGRAMMING: [AtomicBool; super::smp::MAX_CPUS] =
    [const { AtomicBool::new(false) }; super::smp::MAX_CPUS];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CalibrationSource {
    Default = 0,
    Cpuid15 = 1,
    Cpuid16 = 2,
    Firmware = 3,
}

static TSC_HZ: AtomicU64 = AtomicU64::new(DEFAULT_TSC_HZ);
static SOURCE: AtomicU8 = AtomicU8::new(CalibrationSource::Default as u8);
static INITIALIZED: AtomicU8 = AtomicU8::new(0);

#[inline]
fn valid_frequency(hz: u64) -> bool {
    (MIN_TSC_HZ..=MAX_TSC_HZ).contains(&hz)
}

/// Read the architectural CPUID 0x15 ratio, if the firmware/CPU publishes it.
pub fn cpuid15_hz() -> Option<u64> {
    let max = __cpuid_count(0, 0).eax;
    if max < 0x15 {
        return None;
    }
    let leaf = __cpuid_count(0x15, 0);
    if leaf.eax == 0 || leaf.ebx == 0 {
        return None;
    }
    let crystal = if leaf.ecx != 0 {
        u64::from(leaf.ecx)
    } else {
        // Intel specifies 24 MHz as the common fallback when ECX is zero.
        24_000_000
    };
    let hz = u128::from(crystal)
        .saturating_mul(u128::from(leaf.ebx))
        .checked_div(u128::from(leaf.eax))?;
    let hz = u64::try_from(hz).ok()?;
    valid_frequency(hz).then_some(hz)
}

/// Read the nominal MHz value from CPUID 0x16 as a fallback calibration.
pub fn cpuid16_hz() -> Option<u64> {
    let max = __cpuid_count(0, 0).eax;
    if max < 0x16 {
        return None;
    }
    let mhz = u64::from(__cpuid_count(0x16, 0).eax);
    let hz = mhz.checked_mul(1_000_000)?;
    valid_frequency(hz).then_some(hz)
}

/// Whether CPUID advertises an invariant (non-stop) TSC.
pub fn invariant_tsc() -> bool {
    let max_extended = __cpuid_count(0x8000_0000, 0).eax;
    max_extended >= 0x8000_0007 && (__cpuid_count(0x8000_0007, 0).edx & (1 << 8)) != 0
}

/// Select a frequency supplied by the loader/firmware. Invalid values are
/// rejected instead of allowing a divide-by-zero or an implausible clock.
pub fn set_frequency(hz: u64, source: CalibrationSource) -> bool {
    if !valid_frequency(hz) {
        return false;
    }
    TSC_HZ.store(hz, Ordering::Release);
    SOURCE.store(source as u8, Ordering::Release);
    INITIALIZED.store(1, Ordering::Release);
    true
}

/// Calibrate once using the architecturally documented CPUID leaves.
pub fn init() -> u64 {
    if INITIALIZED.load(Ordering::Acquire) == 0 {
        let selected = cpuid15_hz()
            .map(|hz| (hz, CalibrationSource::Cpuid15))
            .or_else(|| cpuid16_hz().map(|hz| (hz, CalibrationSource::Cpuid16)))
            .unwrap_or((DEFAULT_TSC_HZ, CalibrationSource::Default));
        let _ = set_frequency(selected.0, selected.1);
    }
    TSC_HZ.load(Ordering::Acquire)
}

#[inline]
pub fn stable_counter_raw() -> u64 {
    rdtsc()
}

#[inline]
pub fn stable_counter_raw_ordered() -> u64 {
    rdtsc_ordered()
}

#[inline]
pub fn stable_counter_hz() -> u64 {
    init()
}

#[inline]
pub fn stable_counter_to_ns(counter: u64) -> u64 {
    let hz = stable_counter_hz().max(1);
    ((u128::from(counter).saturating_mul(1_000_000_000)) / u128::from(hz)).min(u128::from(u64::MAX))
        as u64
}

#[inline]
pub fn kernel_timestamp_ns() -> u64 {
    stable_counter_to_ns(stable_counter_raw_ordered())
}

pub const fn calibration_source_from_raw(raw: u8) -> CalibrationSource {
    match raw {
        1 => CalibrationSource::Cpuid15,
        2 => CalibrationSource::Cpuid16,
        3 => CalibrationSource::Firmware,
        _ => CalibrationSource::Default,
    }
}

pub fn calibration_source() -> CalibrationSource {
    calibration_source_from_raw(SOURCE.load(Ordering::Acquire))
}

#[inline]
fn local_timer_backend_from_raw(raw: u8) -> LocalTimerBackend {
    match raw {
        1 => LocalTimerBackend::TscDeadline,
        2 => LocalTimerBackend::LapicOneShot,
        3 => LocalTimerBackend::Unavailable,
        4 => LocalTimerBackend::Hosted,
        _ => LocalTimerBackend::Uninitialized,
    }
}

#[inline]
const fn timer_cpu_index(cpu: usize) -> usize {
    if cpu < super::smp::MAX_CPUS {
        cpu
    } else {
        super::smp::MAX_CPUS - 1
    }
}

#[inline]
fn current_timer_cpu() -> usize {
    timer_cpu_index(super::smp::current_cpu_id())
}

#[inline]
fn local_timer_backend_slot() -> &'static AtomicU8 {
    &LOCAL_TIMER_BACKEND[current_timer_cpu()]
}

#[inline]
fn timer_programming_slot() -> &'static AtomicBool {
    &TIMER_PROGRAMMING[current_timer_cpu()]
}

/// Return the backend selected for the current CPU's local timer.
///
/// The value is deliberately a small diagnostic enum rather than a boolean:
/// callers can distinguish “the LAPIC has not been mapped yet” from a hosted
/// no-op or a hardware programming failure.
pub fn local_timer_backend() -> LocalTimerBackend {
    local_timer_backend_from_raw(local_timer_backend_slot().load(Ordering::Acquire))
}

/// Publish a calibrated post-divider LAPIC tick rate for one-shot mode.
///
/// LAPIC bus frequency is not architecturally enumerated.  Firmware/HPET/PIT
/// code may call this after measuring the counter; invalid values are rejected
/// so a bad calibration cannot wrap the 32-bit initial-count calculation.
pub fn set_lapic_timer_frequency(hz: u64) -> bool {
    if !(MIN_LAPIC_TICK_HZ..=MAX_LAPIC_TICK_HZ).contains(&hz) {
        return false;
    }
    LAPIC_TIMER_TICK_HZ.store(hz, Ordering::Release);
    true
}

#[inline]
fn cpuid_tsc_deadline_capable() -> bool {
    let max = __cpuid_count(0, 0).eax;
    max >= 1 && (__cpuid_count(1, 0).ecx & CPUID_TSC_DEADLINE) != 0
}

#[inline]
fn reliable_tsc_for_deadline() -> bool {
    // A scheduler deadline is an absolute value in the TSC-derived nanosecond
    // domain.  Do not select TSC-deadline mode when that domain is only the
    // software 1 GHz guess; one-shot mode remains explicit and bounded there.
    invariant_tsc()
        && valid_frequency(stable_counter_hz())
        && !matches!(calibration_source(), CalibrationSource::Default)
}

#[inline]
fn timer_delta_ns(deadline_ns: Option<u64>, now_ns: u64) -> u64 {
    deadline_ns
        .map(|deadline| deadline.saturating_sub(now_ns).min(DEFAULT_TIMER_PERIOD_NS))
        .unwrap_or(DEFAULT_TIMER_PERIOD_NS)
}

/// Convert a duration to timer ticks, rounding up and retaining at least one
/// tick.  The saturating arithmetic is intentional: malformed far-future
/// deadlines must produce a finite maximum count, never zero through wrap.
#[inline]
fn ns_to_ticks_ceil(delta_ns: u64, hz: u64) -> u64 {
    let product = u128::from(delta_ns).saturating_mul(u128::from(hz));
    let rounded = product.saturating_add(999_999_999) / 1_000_000_000;
    u64::try_from(rounded).unwrap_or(u64::MAX).max(1)
}

#[inline]
fn timer_lvt_value(mode: u32, masked: bool) -> u32 {
    let mut value = u32::from(apic::TIMER_VECTOR) | (mode & LAPIC_TIMER_MODE_MASK);
    if masked {
        value |= LAPIC_TIMER_MASK;
    }
    value
}

#[cfg(target_os = "none")]
#[inline]
fn write_tsc_deadline(value: u64) -> bool {
    // Safety: this path is selected only after CPUID advertises the MSR and
    // executes at CPL0.  The x86 facade keeps WRMSR in one privileged helper.
    unsafe { super::write_msr(MSR_TSC_DEADLINE, value as usize) };
    true
}

#[cfg(not(target_os = "none"))]
#[inline]
fn write_tsc_deadline(value: u64) -> bool {
    let _ = value;
    false
}

#[cfg(target_os = "none")]
fn configure_lapic_timer(backend: LocalTimerBackend) -> bool {
    let mode = match backend {
        LocalTimerBackend::TscDeadline => LAPIC_TIMER_MODE_TSC_DEADLINE,
        LocalTimerBackend::LapicOneShot => LAPIC_TIMER_MODE_ONE_SHOT,
        _ => return false,
    };

    // Mask before changing mode/count.  This also prevents a stale deadline
    // from firing while the scheduler hook is being installed.
    if !apic::write_local_apic(LAPIC_LVT_TIMER, timer_lvt_value(mode, true)) {
        return false;
    }
    if !apic::write_local_apic(LAPIC_TIMER_DIVIDE_CONFIG, LAPIC_TIMER_DIVIDE_BY_16) {
        return false;
    }
    if !apic::write_local_apic(LAPIC_TIMER_INITIAL_COUNT, 0) {
        return false;
    }
    if matches!(backend, LocalTimerBackend::TscDeadline) && !write_tsc_deadline(0) {
        return false;
    }
    true
}

/// Install the local timer mode once the LAPIC mapping and IDT are both ready.
///
/// MADT processing can precede scheduler registration, so this function is
/// intentionally retryable.  It never marks a missing mapping as success;
/// `sched_ctx::register` calls it again after installing the trap entry.
pub(crate) fn initialize_local_timer() {
    #[cfg(not(target_os = "none"))]
    {
        local_timer_backend_slot().store(LocalTimerBackend::Hosted as u8, Ordering::Release);
        return;
    }

    #[cfg(target_os = "none")]
    {
        if !matches!(local_timer_backend(), LocalTimerBackend::Uninitialized) {
            return;
        }
        if apic::local_apic_base().is_none()
            || !trap::is_installed()
            || sched::arch_hooks::deadline_timer().is_none()
        {
            return;
        }
        let programming = timer_programming_slot();
        if programming
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let backend = if cpuid_tsc_deadline_capable() && reliable_tsc_for_deadline() {
            LocalTimerBackend::TscDeadline
        } else {
            LocalTimerBackend::LapicOneShot
        };
        let configured = configure_lapic_timer(backend);
        local_timer_backend_slot().store(
            if configured {
                backend as u8
            } else {
                LocalTimerBackend::Unavailable as u8
            },
            Ordering::Release,
        );
        programming.store(false, Ordering::Release);

        // Arm a regular tick immediately.  Keep this outside the programming
        // guard because `rearm_local_timer` uses the same lock-free guard.
        if configured {
            rearm_local_timer(None);
        }
    }
}

#[cfg(target_os = "none")]
fn arm_tsc_deadline(deadline_ns: Option<u64>) -> bool {
    let now_ticks = stable_counter_raw_ordered();
    let now_ns = stable_counter_to_ns(now_ticks);
    let delta_ns = timer_delta_ns(deadline_ns, now_ns);
    let ticks = ns_to_ticks_ceil(delta_ns, stable_counter_hz());
    let deadline_ticks = now_ticks.saturating_add(ticks);

    if !apic::write_local_apic(
        LAPIC_LVT_TIMER,
        timer_lvt_value(LAPIC_TIMER_MODE_TSC_DEADLINE, true),
    ) {
        return false;
    }
    if !write_tsc_deadline(deadline_ticks) {
        return false;
    }
    apic::write_local_apic(
        LAPIC_LVT_TIMER,
        timer_lvt_value(LAPIC_TIMER_MODE_TSC_DEADLINE, false),
    )
}

#[cfg(target_os = "none")]
fn arm_lapic_oneshot(deadline_ns: Option<u64>) -> bool {
    let now_ns = stable_counter_to_ns(stable_counter_raw_ordered());
    let delta_ns = timer_delta_ns(deadline_ns, now_ns);
    let ticks = ns_to_ticks_ceil(delta_ns, LAPIC_TIMER_TICK_HZ.load(Ordering::Acquire))
        .min(u64::from(u32::MAX)) as u32;
    let ticks = ticks.max(1);

    if !apic::write_local_apic(
        LAPIC_LVT_TIMER,
        timer_lvt_value(LAPIC_TIMER_MODE_ONE_SHOT, true),
    ) {
        return false;
    }
    if !apic::write_local_apic(LAPIC_TIMER_DIVIDE_CONFIG, LAPIC_TIMER_DIVIDE_BY_16) {
        return false;
    }
    if !apic::write_local_apic(LAPIC_TIMER_INITIAL_COUNT, ticks) {
        return false;
    }
    apic::write_local_apic(
        LAPIC_LVT_TIMER,
        timer_lvt_value(LAPIC_TIMER_MODE_ONE_SHOT, false),
    )
}

/// Reprogram the current CPU's local deadline timer.
///
/// `Some` is an absolute scheduler nanosecond deadline; `None` restores the
/// bounded regular tick.  Hosted builds only publish the diagnostic backend
/// and never execute privileged instructions or dereference MMIO.
pub(crate) fn rearm_local_timer(deadline_ns: Option<u64>) {
    #[cfg(not(target_os = "none"))]
    {
        let _ = deadline_ns;
        local_timer_backend_slot().store(LocalTimerBackend::Hosted as u8, Ordering::Release);
        return;
    }

    #[cfg(target_os = "none")]
    {
        if matches!(local_timer_backend(), LocalTimerBackend::Uninitialized) {
            initialize_local_timer();
        }
        let backend = local_timer_backend();
        if !matches!(
            backend,
            LocalTimerBackend::TscDeadline | LocalTimerBackend::LapicOneShot
        ) {
            return;
        }
        let programming = timer_programming_slot();
        if programming
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let programmed = match backend {
            LocalTimerBackend::TscDeadline => arm_tsc_deadline(deadline_ns),
            LocalTimerBackend::LapicOneShot => arm_lapic_oneshot(deadline_ns),
            _ => false,
        };
        programming.store(false, Ordering::Release);
        if !programmed {
            // A failed volatile write is a hard failure for this boot.  Keep
            // the state explicit so the scheduler does not repeatedly issue
            // unsafe MMIO writes from every timer callback.
            local_timer_backend_slot()
                .store(LocalTimerBackend::Unavailable as u8, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_validation_rejects_wraparound_values() {
        assert!(!set_frequency(0, CalibrationSource::Firmware));
        assert!(!set_frequency(u64::MAX, CalibrationSource::Firmware));
        assert!(set_frequency(2_000_000_000, CalibrationSource::Firmware));
        assert_eq!(stable_counter_hz(), 2_000_000_000);
    }

    #[test]
    fn conversion_is_monotonic_and_saturating() {
        let first = stable_counter_to_ns(1);
        let second = stable_counter_to_ns(2);
        let last = stable_counter_to_ns(u64::MAX);
        assert!(first <= second);
        assert!(last >= second);
        assert!(last <= u64::MAX);
    }

    #[test]
    fn timer_tick_conversion_rounds_up_and_never_returns_zero() {
        assert_eq!(ns_to_ticks_ceil(0, 1_000_000_000), 1);
        assert_eq!(ns_to_ticks_ceil(1, 500_000_000), 1);
        assert_eq!(ns_to_ticks_ceil(2, 500_000_000), 1);
        assert_eq!(ns_to_ticks_ceil(2_000_000_000, 500_000_000), 1_000_000_000);
        assert_eq!(ns_to_ticks_ceil(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn timer_deadline_is_capped_to_regular_tick() {
        assert_eq!(timer_delta_ns(None, 100), DEFAULT_TIMER_PERIOD_NS);
        assert_eq!(timer_delta_ns(Some(105), 100), 5);
        assert_eq!(timer_delta_ns(Some(100), 105), 0);
        assert_eq!(timer_delta_ns(Some(u64::MAX), 0), DEFAULT_TIMER_PERIOD_NS);
    }

    #[test]
    fn lapic_frequency_validation_is_bounded() {
        assert!(!set_lapic_timer_frequency(0));
        assert!(!set_lapic_timer_frequency(u64::MAX));
        assert!(set_lapic_timer_frequency(DEFAULT_LAPIC_TICK_HZ));
    }

    #[test]
    fn timer_state_has_one_slot_per_scheduler_cpu() {
        assert_eq!(LOCAL_TIMER_BACKEND.len(), super::super::smp::MAX_CPUS);
        assert_eq!(TIMER_PROGRAMMING.len(), super::super::smp::MAX_CPUS);
        assert_eq!(timer_cpu_index(0), 0);
        assert_eq!(
            timer_cpu_index(super::super::smp::MAX_CPUS),
            super::super::smp::MAX_CPUS - 1
        );
    }
}
