//! `adjtimex(2)` / `clock_adjtime(2)` 的 NTP 时间调整状态机。
//!
//! 模型：`CLOCK_REALTIME = CLOCK_MONOTONIC + REALTIME_OFFSET_NS`。频率微调
//! （`ADJ_FREQUENCY` / `ADJ_TICK`）以"周期 tick 上把频率误差折叠进 offset"的
//! 方式生效：内核路径与 vDSO 路径（`wall_time + delta`）保持同一量级精度
//! （误差 ≤ 500ppm × 10ms ≈ 5us/tick），且不触碰调度器依赖的单调时钟域。
//!
//! `ADJ_OFFSET` 立即应用（Linux 默认在 1 秒内缓变；立即应用对外可观测语义
//! 等价，`ADJ_OFFSET_SS_READ` 因此恒返回 0 剩余量）。`CLOCK_TAI = CLOCK_REALTIME
//! + TAI_OFFSET_NS`，`ADJ_TAI` 通过 `timex.constant`（秒）设置偏移。取舍：
//! TAI 偏移由本模块维护、clock_gettime 在 syscall 层叠加，vDSO 不导出 TAI。

use core::sync::atomic::{AtomicI64, Ordering};

use errno::Errno;
use sched::sync::Spinlock;

pub const ADJ_OFFSET: u32 = 0x0001;
pub const ADJ_FREQUENCY: u32 = 0x0002;
pub const ADJ_MAXERROR: u32 = 0x0004;
pub const ADJ_ESTERROR: u32 = 0x0008;
pub const ADJ_STATUS: u32 = 0x0010;
pub const ADJ_TIMECONST: u32 = 0x0020;
pub const ADJ_TAI: u32 = 0x0080;
pub const ADJ_SETOFFSET: u32 = 0x0100;
pub const ADJ_MICRO: u32 = 0x1000;
pub const ADJ_NANO: u32 = 0x2000;
pub const ADJ_TICK: u32 = 0x4000;
/// `adjtime()` 语义：一次性应用 offset。
pub const ADJ_OFFSET_SINGLESHOT: u32 = 0x8001;
/// 只读查询未应用的 singleshot 偏移（本实现立即应用，恒为 0）。
pub const ADJ_OFFSET_SS_READ: u32 = 0xa001;

pub const STA_PLL: i32 = 0x0001;
pub const STA_PPSFREQ: i32 = 0x0002;
pub const STA_PPSTIME: i32 = 0x0004;
pub const STA_FLL: i32 = 0x0008;
pub const STA_INS: i32 = 0x0010;
pub const STA_DEL: i32 = 0x0020;
pub const STA_UNSYNC: i32 = 0x0040;
pub const STA_FREQHOLD: i32 = 0x0080;
pub const STA_PPSSIGNAL: i32 = 0x0100;
pub const STA_PPSJITTER: i32 = 0x0200;
pub const STA_PPSWANDER: i32 = 0x0400;
pub const STA_PPSERROR: i32 = 0x0800;
pub const STA_CLOCKERR: i32 = 0x1000;
pub const STA_NANO: i32 = 0x2000;
pub const STA_MODE: i32 = 0x4000;
pub const STA_CLK: i32 = 0x8000;

/// Linux `STA_RONLY`：内核维护、用户态不能经 `ADJ_STATUS` 修改的状态位。
const STA_RONLY: i32 = STA_PPSSIGNAL
    | STA_PPSJITTER
    | STA_PPSWANDER
    | STA_PPSERROR
    | STA_CLOCKERR
    | STA_NANO
    | STA_MODE
    | STA_CLK;

/// 按 Linux `process_adjtimex_modes()` 语义合并 `ADJ_STATUS`：可写位来自请求，
/// `STA_RONLY` 位（特别是 `STA_NANO`）始终保留内核当前值。
const fn merge_status(current: i32, requested: i32) -> i32 {
    (current & STA_RONLY) | (requested & !STA_RONLY)
}

/// 频率上限 ±500 ppm（2^-16 ppm 定点，Linux `MAXFREQ_SCALED`）。
const MAXFREQ_SCALED: i64 = 500 * 65536;
/// 默认 maxerror：16 秒（usec，Linux `NTP_PHASE_LIMIT`）。
const DEFAULT_MAXERROR: i64 = 16_000_000;
/// 默认 esterror：0.5 秒（usec）。
const DEFAULT_ESTERROR: i64 = 500_000;
/// 默认 tick：10ms（usec）。
const DEFAULT_TICK_USEC: i64 = 10_000;
/// `ADJ_TICK` 合法范围（Linux：`900000/USER_HZ ..= 1100000/USER_HZ`，USER_HZ=100）。
const MIN_TICK_USEC: i64 = 9_000;
const MAX_TICK_USEC: i64 = 11_000;
/// `ADJ_TIMECONST` 合法范围（Linux `MAXTC = 10`）。
const MAX_TIMECONST: i64 = 10;
/// 精度报告（ns）：1us。
const PRECISION_NS: i64 = 1_000;

/// `struct timex` 的字段视图（内核内部表示；ABI 编解码在 syscall 胶水层）。
#[derive(Clone, Copy, Debug)]
pub struct TimexFields {
    pub modes: u32,
    pub offset: i64,
    pub freq: i64,
    pub maxerror: i64,
    pub esterror: i64,
    pub status: i32,
    pub constant: i64,
    pub tick: i64,
    /// `ADJ_SETOFFSET` 输入：相对调整量的秒部分。
    pub time_sec: i64,
    /// `ADJ_SETOFFSET` 输入：微秒部分；`ADJ_NANO` 模式下为纳秒。
    pub time_subsec: i64,
    /// 只读：实际精度（ns）。
    pub precision: i64,
    /// 只读：频率容限（2^-16 ppm）。
    pub tolerance: i64,
}

struct TimexState {
    /// 频率误差，2^-16 ppm（Linux `time_freq` 语义）。
    freq_scaled: i64,
    maxerror: i64,
    esterror: i64,
    status: i32,
    constant: i64,
    tick_usec: i64,
    /// `ADJ_NANO` 之后 `ADJ_OFFSET` 以 ns 为单位（Linux `STA_NANO` 语义）。
    nano_mode: bool,
    /// 上次把频率误差折叠进 `REALTIME_OFFSET_NS` 的单调时刻。
    last_fold_mono: u64,
}

static STATE: Spinlock<TimexState> = Spinlock::new(TimexState {
    freq_scaled: 0,
    maxerror: DEFAULT_MAXERROR,
    esterror: DEFAULT_ESTERROR,
    status: STA_UNSYNC,
    constant: 0,
    tick_usec: DEFAULT_TICK_USEC,
    nano_mode: false,
    last_fold_mono: 0,
});

/// `CLOCK_TAI` 相对 `CLOCK_REALTIME` 的偏移（ns）。`ADJ_TAI` 以秒为单位写入。
static TAI_OFFSET_NS: AtomicI64 = AtomicI64::new(0);

/// 读取当前 TAI 偏移（ns）。`CLOCK_TAI = CLOCK_REALTIME + tai_offset_ns()`。
pub fn tai_offset_ns() -> i64 {
    TAI_OFFSET_NS.load(Ordering::Relaxed)
}

/// 把自上次折叠以来的频率误差累加进 `REALTIME_OFFSET_NS`。
///
/// `drift_ns = elapsed_ns × freq_ppm / 1e6 = elapsed × freq_scaled / 65536 / 1e6`。
fn fold_locked(state: &mut TimexState, now_ns: u64) {
    let elapsed = now_ns.saturating_sub(state.last_fold_mono);
    state.last_fold_mono = now_ns;
    if elapsed == 0 {
        return;
    }
    let drift_ns = ((elapsed as i128) * (state.freq_scaled as i128) / (65536 * 1_000_000)) as i64;
    if drift_ns != 0 {
        crate::vdso::adjust_realtime_offset(drift_ns);
    }
}

/// 周期 tick 折叠（由 vdso 的 cpu0 tick 钩子调用，与 vdso 页刷新同步）。
pub fn on_timer_tick(now_ns: u64) {
    let mut state = STATE.lock();
    fold_locked(&mut state, now_ns);
}

/// 应用 `adjtimex` 请求并回填可读字段；`EINVAL` 表示参数非法。
pub fn do_adjtimex(mut txc: TimexFields) -> Result<TimexFields, Errno> {
    let now_ns = crate::vdso::monotonic_ns();
    let mut state = STATE.lock();
    fold_locked(&mut state, now_ns);

    let modes = txc.modes;
    // 未知模式位一律拒绝（Linux 语义）。
    const KNOWN_REGULAR: u32 = ADJ_OFFSET
        | ADJ_FREQUENCY
        | ADJ_MAXERROR
        | ADJ_ESTERROR
        | ADJ_STATUS
        | ADJ_TIMECONST
        | ADJ_TAI
        | ADJ_SETOFFSET
        | ADJ_MICRO
        | ADJ_NANO
        | ADJ_TICK;
    let singleshot = matches!(modes, ADJ_OFFSET_SINGLESHOT | ADJ_OFFSET_SS_READ);
    if !singleshot && modes & !KNOWN_REGULAR != 0 {
        return Err(Errno::EINVAL);
    }

    if modes == ADJ_OFFSET_SS_READ {
        // 立即应用模型下没有挂起的缓变，剩余量恒 0。
        txc.offset = 0;
    } else if modes == ADJ_OFFSET_SINGLESHOT {
        let delta_ns = txc.offset.checked_mul(1_000).ok_or(Errno::EINVAL)?;
        crate::vdso::adjust_realtime_offset(delta_ns);
        txc.offset = 0;
    } else {
        if modes & ADJ_NANO != 0 && modes & ADJ_MICRO != 0 {
            return Err(Errno::EINVAL);
        }
        if modes & ADJ_SETOFFSET != 0 && modes & ADJ_OFFSET != 0 {
            return Err(Errno::EINVAL);
        }
        if modes & ADJ_TAI != 0 {
            // Linux：ADJ_TAI 用 timex.constant 携带 TAI 偏移（秒）。范围上限
            // 取 Linux 的 CLOCK_TAI 合理域（±2^32 秒）防止 ns 溢出；负值
            // 合法并被接受（TAI 允许落后于 realtime）。
            let seconds = txc.constant;
            if !(-(1i64 << 32)..=(1i64 << 32)).contains(&seconds) {
                return Err(Errno::EINVAL);
            }
            let delta_ns = seconds.checked_mul(1_000_000_000).ok_or(Errno::EINVAL)?;
            TAI_OFFSET_NS.store(delta_ns, Ordering::Relaxed);
            txc.constant = seconds;
        }
        if modes & ADJ_FREQUENCY != 0 && !(-MAXFREQ_SCALED..=MAXFREQ_SCALED).contains(&txc.freq) {
            return Err(Errno::EINVAL);
        }
        if modes & ADJ_TIMECONST != 0 && !(0..=MAX_TIMECONST).contains(&txc.constant) {
            return Err(Errno::EINVAL);
        }
        if modes & ADJ_TICK != 0 && !(MIN_TICK_USEC..=MAX_TICK_USEC).contains(&txc.tick) {
            return Err(Errno::EINVAL);
        }

        let nano_mode = if modes & ADJ_NANO != 0 {
            true
        } else if modes & ADJ_MICRO != 0 {
            false
        } else {
            state.nano_mode
        };
        let offset_delta_ns = if modes & ADJ_OFFSET != 0 {
            let scale = if nano_mode { 1 } else { 1_000 };
            Some(txc.offset.checked_mul(scale).ok_or(Errno::EINVAL)?)
        } else {
            None
        };
        let setoffset_delta_ns = if modes & ADJ_SETOFFSET != 0 {
            let setoffset_nano = modes & ADJ_NANO != 0;
            let subsec_limit = if setoffset_nano {
                1_000_000_000
            } else {
                1_000_000
            };
            if !(0..subsec_limit).contains(&txc.time_subsec) {
                return Err(Errno::EINVAL);
            }
            let subsec_scale = if setoffset_nano { 1i128 } else { 1_000i128 };
            let delta_ns = (txc.time_sec as i128)
                .checked_mul(1_000_000_000)
                .and_then(|value| value.checked_add((txc.time_subsec as i128) * subsec_scale))
                .filter(|value| (i64::MIN as i128..=i64::MAX as i128).contains(value))
                .ok_or(Errno::EINVAL)? as i64;
            Some(delta_ns)
        } else {
            None
        };

        if modes & ADJ_STATUS != 0 {
            state.status = merge_status(state.status, txc.status);
        }
        if modes & (ADJ_NANO | ADJ_MICRO) != 0 {
            state.nano_mode = nano_mode;
            if nano_mode {
                state.status |= STA_NANO;
            } else {
                state.status &= !STA_NANO;
            }
        }
        if let Some(delta_ns) = offset_delta_ns {
            crate::vdso::adjust_realtime_offset(delta_ns);
            txc.offset = 0;
        }
        if modes & ADJ_FREQUENCY != 0 {
            state.freq_scaled = txc.freq;
        }
        if modes & ADJ_MAXERROR != 0 {
            state.maxerror = txc.maxerror.clamp(0, DEFAULT_MAXERROR);
        }
        if modes & ADJ_ESTERROR != 0 {
            state.esterror = txc.esterror.clamp(0, DEFAULT_ESTERROR);
        }
        if modes & ADJ_TIMECONST != 0 {
            state.constant = txc.constant;
        }
        if modes & ADJ_TICK != 0 {
            let adj_usec = txc.tick - state.tick_usec;
            state.tick_usec = txc.tick;
            // δ usec / 10000 usec tick → 100·δ ppm（2^-16 定点）。
            let freq_adj = adj_usec * 100 * 65536;
            state.freq_scaled = state
                .freq_scaled
                .saturating_add(freq_adj)
                .clamp(-MAXFREQ_SCALED, MAXFREQ_SCALED);
        }
        if let Some(delta_ns) = setoffset_delta_ns {
            crate::vdso::adjust_realtime_offset(delta_ns);
        }
    }

    // 回填只读/当前状态字段。
    txc.freq = state.freq_scaled;
    txc.maxerror = state.maxerror;
    txc.esterror = state.esterror;
    txc.status = state.status;
    // ADJ_TAI 复用 constant 字段返回 TAI 偏移（秒），否则返回 PLL 时间常数。
    txc.constant = if modes & ADJ_TAI != 0 {
        TAI_OFFSET_NS.load(Ordering::Relaxed) / 1_000_000_000
    } else {
        state.constant
    };
    txc.tick = state.tick_usec;
    txc.precision = PRECISION_NS;
    txc.tolerance = MAXFREQ_SCALED;
    Ok(txc)
}

/// `adjtimex`/`clock_adjtime` 的系统调用返回值：时钟状态（`TIME_OK` 等）。
///
/// 取舍：本实现不调度闰秒插入/删除（STA_INS/STA_DEL 只作标志位直接映射），
/// 也没有“进行中/已完成”的跃变阶段，因此不会返回 `TIME_OOP(3)`/`TIME_WAIT(4)`；
/// 返回值集合收敛为 `TIME_OK/TIME_ERROR/TIME_INS/TIME_DEL`。这是无闰秒状态机
/// 下的可接受简化。
pub fn clock_state(status: i32) -> i32 {
    // Linux kernel/time/time.c 的映射：UNSYNC/CLOCKERR → TIME_ERROR(5)，
    // INS → TIME_INS(1)，DEL → TIME_DEL(2)，否则 TIME_OK(0)。
    if status & (STA_UNSYNC | STA_CLOCKERR) != 0 {
        return 5; // TIME_ERROR
    }
    if status & STA_INS != 0 {
        return 1; // TIME_INS
    }
    if status & STA_DEL != 0 {
        return 2; // TIME_DEL
    }
    0 // TIME_OK
}
