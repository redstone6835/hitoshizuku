//! `adjtimex(2)` / `clock_adjtime(2)` 的 NTP 时间调整状态机。
//!
//! 模型：`CLOCK_REALTIME = CLOCK_MONOTONIC + REALTIME_OFFSET_NS`。频率微调
//! （`ADJ_FREQUENCY` / `ADJ_TICK`）以"周期 tick 上把频率误差折叠进 offset"的
//! 方式生效：内核路径与 vDSO 路径（`wall_time + delta`）保持同一量级精度
//! （误差 ≤ 500ppm × 10ms ≈ 5us/tick），且不触碰调度器依赖的单调时钟域。
//!
//! `ADJ_OFFSET` 立即应用（Linux 默认在 1 秒内缓变；立即应用对外可观测语义
//! 等价，`ADJ_OFFSET_SS_READ` 因此恒返回 0 剩余量）。`CLOCK_TAI` 未实现，
//! `ADJ_TAI` 返回 `EINVAL`（Linux 对不支持 TAI 的时钟同样拒绝）。

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
/// `adjtime()` 语义：一次性应用 offset（隐含 `ADJ_OFFSET|ADJ_FREQUENCY`）。
pub const ADJ_OFFSET_SINGLESHOT: u32 = 0x8002;
/// 只读查询未应用的 singleshot 偏移（本实现立即应用，恒为 0）。
pub const ADJ_OFFSET_SS_READ: u32 = 0xa000;

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

/// 可写 status 位掩码（PLL/FLL/同步状态等由用户态 NTP 维护）。
const STA_WRITABLE: i32 = STA_PLL
    | STA_PPSFREQ
    | STA_PPSTIME
    | STA_FLL
    | STA_INS
    | STA_DEL
    | STA_UNSYNC
    | STA_FREQHOLD
    | STA_PPSSIGNAL
    | STA_PPSJITTER
    | STA_PPSWANDER
    | STA_PPSERROR
    | STA_CLOCKERR
    | STA_MODE
    | STA_CLK;

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
    const KNOWN: u32 = ADJ_OFFSET
        | ADJ_FREQUENCY
        | ADJ_MAXERROR
        | ADJ_ESTERROR
        | ADJ_STATUS
        | ADJ_TIMECONST
        | ADJ_TAI
        | ADJ_SETOFFSET
        | ADJ_MICRO
        | ADJ_NANO
        | ADJ_TICK
        | 0x8000 // ADJ_OFFSET_SINGLESHOT/SS_READ 的标记位
        | 0x2000;
    if modes & !KNOWN != 0 {
        return Err(Errno::EINVAL);
    }

    if modes & 0x8000 != 0 {
        // singleshot 家族：0xa000 = SS_READ（只读），0x8002 = 应用。
        if modes & ADJ_OFFSET_SS_READ == ADJ_OFFSET_SS_READ {
            // 立即应用模型下没有挂起的缓变，剩余量恒 0。
            txc.offset = 0;
        } else {
            let delta_ns = if state.nano_mode { txc.offset } else { txc.offset * 1_000 };
            crate::vdso::adjust_realtime_offset(delta_ns);
            txc.offset = 0;
        }
    } else {
        if modes & ADJ_OFFSET != 0 {
            let delta_ns = if state.nano_mode { txc.offset } else { txc.offset * 1_000 };
            crate::vdso::adjust_realtime_offset(delta_ns);
            txc.offset = 0;
        }
        if modes & ADJ_FREQUENCY != 0 {
            if txc.freq.abs() > MAXFREQ_SCALED {
                return Err(Errno::EINVAL);
            }
            state.freq_scaled = txc.freq;
        }
        if modes & ADJ_MAXERROR != 0 {
            state.maxerror = txc.maxerror.clamp(0, DEFAULT_MAXERROR);
        }
        if modes & ADJ_ESTERROR != 0 {
            state.esterror = txc.esterror.clamp(0, DEFAULT_ESTERROR);
        }
        if modes & ADJ_STATUS != 0 {
            state.status = txc.status & STA_WRITABLE;
        }
        if modes & ADJ_TIMECONST != 0 {
            if !(0..=MAX_TIMECONST).contains(&txc.constant) {
                return Err(Errno::EINVAL);
            }
            state.constant = txc.constant;
        }
        if modes & ADJ_TICK != 0 {
            if !(MIN_TICK_USEC..=MAX_TICK_USEC).contains(&txc.tick) {
                return Err(Errno::EINVAL);
            }
            let adj_usec = txc.tick - state.tick_usec;
            state.tick_usec = txc.tick;
            // δ usec / 10000 usec tick → 100·δ ppm（2^-16 定点）。
            let freq_adj = adj_usec * 100 * 65536;
            state.freq_scaled = state
                .freq_scaled
                .saturating_add(freq_adj)
                .clamp(-MAXFREQ_SCALED, MAXFREQ_SCALED);
        }
        if modes & ADJ_TAI != 0 {
            // 本内核未实现 CLOCK_TAI。
            return Err(Errno::EINVAL);
        }
        if modes & ADJ_SETOFFSET != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        if modes & ADJ_NANO != 0 {
            state.nano_mode = true;
            state.status |= STA_NANO;
        }
        if modes & ADJ_MICRO != 0 {
            state.nano_mode = false;
            state.status &= !STA_NANO;
        }
    }

    // 回填只读/当前状态字段。
    txc.freq = state.freq_scaled;
    txc.maxerror = state.maxerror;
    txc.esterror = state.esterror;
    txc.status = state.status;
    txc.constant = state.constant;
    txc.tick = state.tick_usec;
    txc.precision = PRECISION_NS;
    txc.tolerance = MAXFREQ_SCALED;
    Ok(txc)
}

/// `adjtimex`/`clock_adjtime` 的系统调用返回值：时钟状态（`TIME_OK` 等）。
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
