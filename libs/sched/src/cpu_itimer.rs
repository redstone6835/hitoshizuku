//! `ITIMER_VIRTUAL` / `ITIMER_PROF`（进程 CPU 时间域定时器）与 tick 级
//! user/system CPU 时间记账。
//!
//! 记账模型：每个 CPU 在周期 tick 中断时记录"被中断上下文是否为用户态"
//! （由架构 trap 层传入 `from_user`），把自上次 tick 以来的整段增量计入
//! 当前任务的线程组用户时间（`ThreadGroup::user_cpu_ns`）；总 CPU 时间由
//! runqueue 的 `cpu_runtime_ns` 提供，故：
//!
//! - `ITIMER_VIRTUAL`（进程用户态 CPU 时间）以组用户时间为域；
//! - `ITIMER_PROF`（用户态 + 内核态 CPU 时间）以组总 CPU 时间为域。
//!
//! 精度与 Linux 相同为 tick 粒度（100Hz）；tick 落在用户态时整段按用户计，
//! 属经典的粗粒度记账，误差 ≤ 一个 tick 周期。定时器检查同样挂在周期 tick
//! 上（Linux 在 scheduler tick 检查 `current` 的进程 CPU 定时器）。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::cpu::MAX_CPUS;
use crate::group::ThreadGroup;
use crate::signal::{SigInfo, SignalNumber};
use crate::task::Task;

/// 每 CPU 上次记账 tick 时刻（0 = 尚未记账）。
static LAST_TICK_ACCOUNT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// tick 中断记账：`from_user` 为真时把本段增量计入当前任务的线程组用户时间。
///
/// 由架构 timer ISR 在 `on_timer_tick` / `defer_timer_tick` 之前调用。
pub fn account_tick_user_system(now_ns: u64, from_user: bool) {
    let cpu_id = crate::scheduler::current_cpu_id();
    let last = LAST_TICK_ACCOUNT[cpu_id].swap(now_ns, Ordering::Relaxed);
    if last == 0 || !from_user {
        return;
    }
    let delta = now_ns.saturating_sub(last);
    if delta == 0 {
        return;
    }
    let Some(current) = crate::scheduler::runqueue_of(cpu_id).current() else {
        return;
    };
    current
        .thread_group()
        .user_cpu_ns
        .fetch_add(delta, Ordering::Relaxed);
}

/// CPU 时间域定时器种类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CpuItimerKind {
    /// `ITIMER_VIRTUAL`：进程用户态 CPU 时间。
    Virtual,
    /// `ITIMER_PROF`：进程用户态 + 内核态 CPU 时间。
    Prof,
}

/// `value_ns == 0` 表示未 armed；`interval_ns != 0` 表示到期后按该周期重装。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuItimerSpec {
    pub value_ns: u64,
    pub interval_ns: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CpuItimer {
    /// 到期时刻（各自时间域）。
    pub(crate) deadline_ns: u64,
    pub(crate) interval_ns: u64,
}

/// 查询线程组 CPU 时间域定时器剩余值。
pub fn get_cpu_itimer(task: &Arc<Task>, kind: CpuItimerKind) -> CpuItimerSpec {
    let group = task.thread_group();
    let now_ns = group_user_time(group.as_ref(), kind);
    let slot = match kind {
        CpuItimerKind::Virtual => &group.itimer_virtual,
        CpuItimerKind::Prof => &group.itimer_prof,
    };
    slot.lock()
        .as_ref()
        .map(|t| CpuItimerSpec {
            value_ns: t.deadline_ns.saturating_sub(now_ns),
            interval_ns: t.interval_ns,
        })
        .unwrap_or_default()
}

/// 设置线程组 CPU 时间域定时器，返回旧值。`value_ns == 0` 解除。
pub fn set_cpu_itimer(
    task: &Arc<Task>,
    kind: CpuItimerKind,
    value_ns: u64,
    interval_ns: u64,
) -> CpuItimerSpec {
    let group = task.thread_group();
    let now_ns = group_user_time(group.as_ref(), kind);
    let slot = match kind {
        CpuItimerKind::Virtual => &group.itimer_virtual,
        CpuItimerKind::Prof => &group.itimer_prof,
    };
    let mut guard = slot.lock();
    let old = guard
        .as_ref()
        .map(|t| CpuItimerSpec {
            value_ns: t.deadline_ns.saturating_sub(now_ns),
            interval_ns: t.interval_ns,
        })
        .unwrap_or_default();
    *guard = if value_ns == 0 {
        None
    } else {
        Some(CpuItimer {
            deadline_ns: now_ns.saturating_add(value_ns),
            interval_ns,
        })
    };
    old
}

/// 线程组在指定 CPU 时间域上的当前值。
fn group_user_time(group: &ThreadGroup, kind: CpuItimerKind) -> u64 {
    match kind {
        CpuItimerKind::Virtual => group.user_cpu_ns(),
        CpuItimerKind::Prof => group.cpu_runtime_ns(),
    }
}

/// 周期 tick 检查当前 CPU 任务的线程组 CPU 时间域定时器。
pub(crate) fn fire_expired_cpu_itimers(_now_ns: u64, cpu_id: usize) {
    let Some(current) = crate::scheduler::runqueue_of(cpu_id).current() else {
        return;
    };
    let group = current.thread_group();
    let mut fired = Vec::new();
    for kind in [CpuItimerKind::Virtual, CpuItimerKind::Prof] {
        let slot = match kind {
            CpuItimerKind::Virtual => &group.itimer_virtual,
            CpuItimerKind::Prof => &group.itimer_prof,
        };
        let mut guard = slot.lock();
        let Some(timer) = guard.as_mut() else {
            continue;
        };
        let now = group_user_time(group.as_ref(), kind);
        if now < timer.deadline_ns {
            continue;
        }
        if timer.interval_ns == 0 {
            guard.take();
        } else {
            let missed = (now - timer.deadline_ns) / timer.interval_ns;
            timer.deadline_ns += (missed + 1) * timer.interval_ns;
        }
        drop(guard);
        fired.push(match kind {
            CpuItimerKind::Virtual => SignalNumber::SIGVTALRM,
            CpuItimerKind::Prof => SignalNumber::SIGPROF,
        });
    }
    for signo in fired {
        let info = SigInfo {
            sig: signo,
            code: 128, // SI_KERNEL 精简编码（与 ITIMER_REAL 的 SIGALRM 一致）
            sender_pid: 0,
            sender_uid: crate::ids::Uid::ROOT,
            raw: None,
        };
        crate::scheduler::deliver_shared_signal_to_group(&group, info);
    }
}
