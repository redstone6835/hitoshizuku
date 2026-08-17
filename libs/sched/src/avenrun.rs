//! 1/5/15 分钟负载均值（Linux `avenrun` 语义的定点实现）。
//!
//! Linux 每 5 秒按指数衰减公式更新三个窗口的负载：
//!
//! ```text
//! avenrun[i] = (avenrun[i] * EXP_i + active * (FIXED_1 - EXP_i)) / FIXED_1
//! ```
//!
//! 其中 `active` 是当前可运行任务数（Linux 为 `nr_running + nr_uninterruptible`，
//! 本实现采用各 CPU runqueue 的 `nr_running` 之和，差异见模块注释），`EXP_i`
//! 是 `exp(-5s/τ_i) * 2048` 的定点衰减系数（τ=60/300/900 秒）。
//!
//! 与 Linux 相同采用 11 位定点（`AVENRUN_ONE = 2048`）；`sysinfo(2)` 的
//! `loads[]` 字段单位为 1/65536，由 [`loads_scaled`] 换算。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::scheduler::{online_cpu_mask, runqueue_of};
use crate::sync::Spinlock;

/// 定点基数：1.0 负载 = `AVENRUN_ONE`（与 Linux 的 11 位定点一致）。
pub const AVENRUN_ONE: u64 = 2048;

/// Linux loadavg 定点基数。
const FIXED_1: u64 = AVENRUN_ONE;

/// 5 秒采样周期下各窗口的衰减系数：`exp(-5/τ) * FIXED_1`。
const EXP_1: u64 = 1884; // τ = 60s
const EXP_5: u64 = 2014; // τ = 300s
const EXP_15: u64 = 2037; // τ = 900s

/// Linux 每 5 秒采样一次全局活跃任务数。
const LOAD_FREQ_NS: u64 = 5_000_000_000;

/// 一次调用最多追赶的采样数（防止长时间停摆后循环失控）。
const MAX_CATCH_UP_SAMPLES: u64 = 256;

struct AvenrunState {
    /// 最近一次更新的采样时间戳（ns）。
    last_tick_ns: u64,
    /// 三个窗口的定点负载，单位 `AVENRUN_ONE`。
    loads: [u64; 3],
}

static STATE: Spinlock<AvenrunState> = Spinlock::new(AvenrunState {
    last_tick_ns: 0,
    loads: [0; 3],
});

/// 上一次实际执行更新时写入的时间戳；多 CPU 并发 tick 时只有观察到
/// 时间窗推进的那个 CPU 执行更新（与 Linux 在单个 CPU 上维护 avenrun 一致）。
static LAST_UPDATE_NS: AtomicU64 = AtomicU64::new(0);

/// 供单测注入的活跃任务数（`None` 表示使用实时采样）。
#[cfg(test)]
static TEST_ACTIVE: Spinlock<Option<u64>> = Spinlock::new(None);

/// 采样当前全局活跃任务数：各在线 CPU runqueue 的 `nr_running` 之和。
///
/// 说明：Linux 的 `active` 还包含不可中断睡眠任务（`nr_uninterruptible`）；
/// 本内核的不可中断任务比例极低（主要为块 I/O 等待），为保持 tick 路径
/// 无全局遍历，这里只统计可运行任务，负载数值在满负载时与 Linux 一致。
fn active_tasks() -> u64 {
    #[cfg(test)]
    if let Some(v) = *TEST_ACTIVE.lock() {
        return v;
    }
    let mut mask = online_cpu_mask();
    let mut active = 0u64;
    while mask != 0 {
        let cpu_id = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        active += runqueue_of(cpu_id).nr_running() as u64;
    }
    active
}

/// 周期 tick 钩子：每 5 秒按经过的采样数做指数衰减更新。
///
/// 由调度器 `on_timer_tick` 调用。多 CPU 下通过 CAS 保证每个 5 秒采样窗
/// 只更新一次；长时间停摆时按 `MAX_CATCH_UP_SAMPLES` 上限追赶。
pub fn tick(now_ns: u64) {
    loop {
        let last = LAST_UPDATE_NS.load(Ordering::Acquire);
        if now_ns.saturating_sub(last) < LOAD_FREQ_NS {
            return;
        }
        if LAST_UPDATE_NS
            .compare_exchange_weak(last, now_ns, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        let elapsed = now_ns - last;
        let steps = (elapsed / LOAD_FREQ_NS).clamp(1, MAX_CATCH_UP_SAMPLES);
        let active = active_tasks().saturating_mul(AVENRUN_ONE);
        let mut st = STATE.lock();
        // EXP 是旧负载的保留比例；新样本的权重必须是 FIXED_1 - EXP。
        for _ in 0..steps {
            for (load, exp) in st.loads.iter_mut().zip([EXP_1, EXP_5, EXP_15]) {
                let mut next =
                    *load as u128 * exp as u128 + active as u128 * (FIXED_1 - exp) as u128;
                // 与 Linux calc_load() 相同，负载上升时向上取整。
                if active >= *load {
                    next += (FIXED_1 - 1) as u128;
                }
                *load = (next / FIXED_1 as u128) as u64;
            }
        }
        st.last_tick_ns = now_ns;
        return;
    }
}

/// 当前负载均值，单位 1/65536（`sysinfo` 的 `loads[]` 字段单位）。
pub fn loads_scaled() -> [u64; 3] {
    let st = STATE.lock();
    [
        st.loads[0] * (65536 / AVENRUN_ONE),
        st.loads[1] * (65536 / AVENRUN_ONE),
        st.loads[2] * (65536 / AVENRUN_ONE),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试共享全局静态（STATE/LAST_UPDATE_NS/TEST_ACTIVE），必须串行执行。
    static TEST_LOCK: Spinlock<()> = Spinlock::new(());

    fn reset() {
        *STATE.lock() = AvenrunState {
            last_tick_ns: 0,
            loads: [0; 3],
        };
        LAST_UPDATE_NS.store(0, Ordering::SeqCst);
        *TEST_ACTIVE.lock() = None;
    }

    /// 第一个 5 秒采样使用 Linux 的 (FIXED_1 - EXP) 新样本权重。
    #[test]
    fn first_sample_uses_linux_weight() {
        let _guard = TEST_LOCK.lock();
        reset();
        *TEST_ACTIVE.lock() = Some(4);
        tick(LOAD_FREQ_NS - 1);
        assert_eq!(loads_scaled(), [0; 3]);

        tick(LOAD_FREQ_NS);
        assert_eq!(loads_scaled(), [20_992, 4_352, 1_408]);
    }

    /// 持续 1 分钟的 4 个活跃任务应按 1/5/15 分钟窗口产生不同响应。
    #[test]
    fn tracks_real_time_windows_after_one_minute() {
        let _guard = TEST_LOCK.lock();
        reset();
        *TEST_ACTIVE.lock() = Some(4);
        for sample in 1..=12u64 {
            tick(sample * LOAD_FREQ_NS);
        }

        let loads = loads_scaled();
        let unit = 65_536;
        assert!(
            (2 * unit..3 * unit).contains(&loads[0]),
            "1min load: {}",
            loads[0]
        );
        assert!(
            (unit / 2..unit).contains(&loads[1]),
            "5min load: {}",
            loads[1]
        );
        assert!(
            (unit / 8..unit / 2).contains(&loads[2]),
            "15min load: {}",
            loads[2]
        );
    }

    /// 运行 60 分钟后再空闲 15 分钟，负载按各自真实窗口衰减。
    #[test]
    fn decays_across_real_time_windows_after_idle() {
        let _guard = TEST_LOCK.lock();
        reset();
        *TEST_ACTIVE.lock() = Some(8);
        for sample in 1..=720u64 {
            tick(sample * LOAD_FREQ_NS);
        }
        *TEST_ACTIVE.lock() = Some(0);
        for sample in 721..=900u64 {
            tick(sample * LOAD_FREQ_NS);
        }

        let loads = loads_scaled();
        let unit = 65_536;
        assert!(loads[0] < unit / 20, "1min load not decayed: {}", loads[0]);
        assert!(
            (unit / 4..unit / 2).contains(&loads[1]),
            "5min load: {}",
            loads[1]
        );
        assert!(
            (2 * unit..4 * unit).contains(&loads[2]),
            "15min load: {}",
            loads[2]
        );
    }

    /// 多 CPU 竞争：同一时间窗只允许一次更新推进，负载不超采样。
    #[test]
    fn multi_cpu_tick_is_idempotent_per_window() {
        let _guard = TEST_LOCK.lock();
        reset();
        *TEST_ACTIVE.lock() = Some(1);
        tick(LOAD_FREQ_NS);
        let after_one_sample = loads_scaled();

        // 模拟 12 个 CPU 在同一个 5 秒采样窗内各自调用 tick。
        for offset in 0..100u64 {
            for _cpu in 0..12u64 {
                tick(LOAD_FREQ_NS + offset);
            }
        }
        assert_eq!(loads_scaled(), after_one_sample);
    }
}
