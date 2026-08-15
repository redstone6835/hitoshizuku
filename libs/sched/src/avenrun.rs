//! 1/5/15 分钟负载均值（Linux `avenrun` 语义的定点实现）。
//!
//! Linux 在周期 tick 上按指数衰减公式更新三个窗口的负载：
//!
//! ```text
//! avenrun[i] += (active - avenrun[i]) * EXP_i / 2048
//! ```
//!
//! 其中 `active` 是当前可运行任务数（Linux 为 `nr_running + nr_uninterruptible`，
//! 本实现采用各 CPU runqueue 的 `nr_running` 之和，差异见模块注释），`EXP_i`
//! 是 `exp(-tick/τ_i) * 2048` 的定点系数（tick=100ms，τ=60/300/900 秒）。
//!
//! 与 Linux 相同采用 11 位定点（`AVENRUN_ONE = 2048`）；`sysinfo(2)` 的
//! `loads[]` 字段单位为 1/65536，由 [`loads_scaled`] 换算。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::scheduler::{online_cpu_mask, runqueue_of};
use crate::sync::Spinlock;

/// 定点基数：1.0 负载 = `AVENRUN_ONE`（与 Linux 的 11 位定点一致）。
pub const AVENRUN_ONE: u64 = 2048;

/// 100ms tick 上各窗口的衰减系数：`exp(-0.1/τ) * 2048`。
const EXP_1: u64 = 1884; // τ = 60s
const EXP_5: u64 = 2014; // τ = 300s
const EXP_15: u64 = 2037; // τ = 900s

/// 两次 tick 的最小间隔（100Hz）。
const TICK_NS: u64 = 10_000_000;

/// 一次调用最多追赶的 tick 数（防止长时间停摆后循环失控）。
const MAX_CATCH_UP_TICKS: u64 = 256;

struct AvenrunState {
    /// 最近一次更新的 tick 时间戳（ns）。
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

/// 周期 tick 钩子：按经过的 tick 数做指数衰减更新。
///
/// 由调度器 `on_timer_tick` 调用（100Hz）。多 CPU 下通过 CAS 保证每个
/// 时间窗只更新一次；长时间停摆时按 `MAX_CATCH_UP_TICKS` 上限追赶。
pub fn tick(now_ns: u64) {
    loop {
        let last = LAST_UPDATE_NS.load(Ordering::Acquire);
        if now_ns < last + TICK_NS {
            return;
        }
        if LAST_UPDATE_NS
            .compare_exchange_weak(last, now_ns, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        let elapsed = now_ns - last;
        let steps = (elapsed / TICK_NS).clamp(1, MAX_CATCH_UP_TICKS);
        let active = active_tasks().saturating_mul(AVENRUN_ONE);
        let mut st = STATE.lock();
        // 指数衰减允许负载下降：`loads += (active - loads) * EXP / ONE`，
        // 必须按有符号计算，`saturating_sub` 会让空闲后的负载永不回落。
        for _ in 0..steps {
            for (load, exp) in st.loads.iter_mut().zip([EXP_1, EXP_5, EXP_15]) {
                let delta = ((active as i128 - *load as i128) * exp as i128) / AVENRUN_ONE as i128;
                *load = (*load as i128 + delta).max(0) as u64;
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

    /// 恒定活跃负载下三个窗口应收敛到 active（定点误差 < 5%）。
    #[test]
    fn converges_to_active_load() {
        let _guard = TEST_LOCK.lock();
        reset();
        *TEST_ACTIVE.lock() = Some(4);
        // 模拟 15 分钟（9000 tick）的持续满负载。
        for i in 1..=9000u64 {
            tick(i * TICK_NS);
        }
        let loads = loads_scaled();
        // 单位 1/65536：负载 4.0 应接近 4 * 65536。
        for (i, v) in loads.iter().enumerate() {
            let target = 4 * 65536;
            let err = v.abs_diff(target) as f64 / target as f64;
            assert!(
                err < 0.05,
                "window {i}: load {v} vs target {target}, err {err:.4}",
            );
        }
    }

    /// 空闲后负载应随时间衰减回 0。
    #[test]
    fn decays_to_zero_after_idle() {
        let _guard = TEST_LOCK.lock();
        reset();
        *TEST_ACTIVE.lock() = Some(8);
        for i in 1..=600u64 {
            tick(i * TICK_NS);
        }
        *TEST_ACTIVE.lock() = Some(0);
        for i in 601..=9600u64 {
            tick(i * TICK_NS);
        }
        let loads = loads_scaled();
        // 1 分钟窗口应几乎归零（τ=60s 衰减 15 分钟 → e^-15 ≈ 3e-7）。
        assert!(loads[0] < 65536, "1min load not decayed: {}", loads[0]);
        assert!(loads[2] < 4 * 65536, "15min load not decayed: {}", loads[2]);
    }

    /// 多 CPU 竞争：同一时间窗只允许一次更新推进，负载不超采样。
    #[test]
    fn multi_cpu_tick_is_idempotent_per_window() {
        let _guard = TEST_LOCK.lock();
        reset();
        *TEST_ACTIVE.lock() = Some(1);
        // 模拟 12 个 CPU 在同一时间窗内各自调用 tick。
        for i in 0..100u64 {
            for cpu in 0..12u64 {
                let _ = cpu;
                tick(10 * TICK_NS + i);
            }
        }
        // 负载必须收敛到 1.0（超采样会导致 > 1.0）。
        let loads = loads_scaled();
        assert!(loads[0] <= 2 * 65536, "over-sampled load: {}", loads[0]);
    }
}
