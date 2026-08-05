#![no_std]
//!
//! 进程/任务调度子系统。
//!
//! 本 crate 提供内核任务的核心抽象、EEVDF 调度算法、以及 POSIX 进程/信号 ABI
//! 的内核侧实现。整个内部模型**不依赖 PID 整数**：任务身份 = `Arc<Task>`，
//! 父子关系、线程组、等待队列、进程间通信全部通过 `Arc` / `Weak` 互引，
//! 生命周期由所有权表达，语义天然原子。
//!
//! ## 分层
//!
//! - [`task`] —— 单个任务的状态、亲子链接、退出通知、凭据、信号、VFS 侧表。
//! - [`group`] —— 线程组（TGID 等价物）、进程组、会话。
//! - [`wait`] —— 通用等待队列，基于 `Weak<Task>` 订阅。
//! - [`eevdf`] —— EEVDF 算法参数：权重表、虚拟时间推进、deadline 计算。
//! - [`runqueue`] —— 按虚拟 deadline 有序的运行队列与调度决策。
//! - [`pid`] —— PID namespace 与整数索引（上层 ABI 翻译使用）。
//! - [`ids`] —— sched 自有的 Uid/Gid/Credentials/CapSet（不依赖 vfs）。
//! - [`signal`] —— 信号号码、集合、动作、per-task / per-tg 状态。
//! - [`clone_flags`] —— `clone(2)` 位标志与 `CloneArgs`。
//! - [`wait_flags`] —— `wait4 / waitid` 标志、目标、wstatus 编码。
//! - [`scheduler`] —— 全局 runqueue、CURRENT_TASK、`init` / `schedule_once`。
//! - [`spawn`] —— SpawnKind、spawn/exit/reap、clone_task、kthread_*。
//! - [`operation`] —— POSIX 进程/信号动词（getpid / kill / fork / wait4 …）。
//!
//! ## 与 Linux ABI 的关系
//!
//! 数字化命名（pid_t / tid / pgid / sid）通过 [`pid`] 模块提供。`operation`
//! 模块内函数与 Linux syscall 同名（去掉 `sys_` 前缀），参数形状对齐，但**不**
//! 是 syscall 本身 —— syscall dispatcher 由更上层做，调用这些 operation 即可。
//!
//! ## 锁顺序 (Lock Ordering)
//!
//! 1. `RT_SCHEDULING_CONFIG` —— 仅 sysctl 更新路径持有；可依次进入各 CPU
//!    `Runqueue::inner`，反向获取禁止。
//! 2. `Runqueue::inner` —— 每 CPU 一把，严禁跨 rq 反序。
//! 3. `Task::rel` —— 亲子关系（parent / children / tg_link / pid_in_ns）。
//! 4. `Task::creds` / `Task::kstack` / `Task::ctx` / `Task::ext` —— 同一
//!    Task 内的次级字段锁，彼此独立，禁止互相嵌套。`Task::shared_signal`
//!    是稳定的 `Arc`，其内部状态按自身规则同步。
//! 5. `ThreadGroup::members` / `ProcessGroup::members` / `Session::groups` ——
//!    组成员索引。
//! 6. `SharedSignal::actions` / `SharedSignal::shared_pending_infos` ——
//!    tg 共享信号表。
//! 7. `WaitQueue::waiters` —— 等待者列表。
//! 8. `SignalState::pending_infos` —— per-task 信号队列。
//!
//! 调用可能触发唤醒 / 分配的函数前必须释放所有 rq 锁。
//!
//! 依赖 `alloc` 的 `Arc` / `Vec` / `BTreeMap`；不做堆外分配。原子操作必须
//! 显式传入 [`core::sync::atomic::Ordering`]。

extern crate alloc;

pub mod arch_hooks;
pub mod clone_flags;
pub mod cpu;
mod deadline_admission;
pub mod eevdf;
pub mod group;
pub mod ids;
pub mod membarrier;
pub mod migration;
pub mod mutex;
pub mod operation;
pub mod pid;
pub mod placement;
pub mod process_ops;
pub mod rlimit;
pub mod rseq;
mod runqueue;
pub mod sched_class;
pub mod scheduler;
pub mod scheduler_state;
pub mod signal;
pub mod spawn;
pub mod sync;
pub mod task;
pub mod wait;
pub mod wait_flags;

pub use arch_hooks::{
    ArchContextOps, ArchDeadlineTimerOps, CpuControlOps, KernelEntry, TaskCpuStateOps,
    mark_urgent_work, poll_urgent_work, urgent_pending_slots, urgent_work_pending,
};
pub use clone_flags::{CloneArgs, CloneFlags};
pub use cpu::{
    CpuId, CpuMask, MAX_SCHED_DOMAINS, SCHED_CAPACITY_SCALE, SchedDomain, SchedPlacement,
    SchedTopology,
};
pub use eevdf::{SchedEntity, SchedParams, Weight};
pub use group::{GroupExitStatus, ProcessGroup, Session, ThreadGroup};
pub use ids::{CapSet, Capability, Credentials, Gid, Uid};
pub use membarrier::{
    handle_ipi as handle_membarrier_ipi, handle_ipi_on as handle_membarrier_ipi_on,
    pending_on as membarrier_pending_on, synchronize_cpus,
};
pub use migration::MigrationContext;
pub use operation::spawn_user_process;
pub use pid::{PidNamespace, PidRegistry, PidT};
pub use placement::{PlacementSnapshot, PlacementState, TaskPlacement};
pub use process_ops::{
    ExecRequest, ProcessImageOps, UserContextRef, process_image_ops, register_process_image_ops,
};
pub use rlimit::{Resource, Rlim, RlimitError, RlimitPair, Rlimits, RlimitsLock};
pub use rseq::{RseqCs, RseqError, RseqEvent, RseqEvents, RseqResumeAction, validate_signature};
pub use runqueue::RunqueueClassLoad;
pub use sched_class::{
    DEFAULT_DL_DEADLINE_NS, DEFAULT_DL_PERIOD_NS, DEFAULT_DL_RUNTIME_NS, DEFAULT_RR_SLICE_NS,
    DEFAULT_RT_PERIOD_NS, DEFAULT_RT_RUNTIME_NS, RT_PRIO_MAX, RT_PRIO_MIN, SchedAttr, SchedClass,
    SchedPolicy,
};
#[cfg(feature = "performance-profile")]
pub use scheduler::current_task_epoch;
pub use scheduler::{
    DeadlineObserver, cancel_deadline_observer, cancel_sleep_deadline, register_deadline_observer,
    reserve_deadline_observer_id,
};
pub use scheduler::{
    NR_CPUS, acknowledge_resched_notification, activate_cpu, active_cpu_mask, balance_once,
    current_cpu_id, current_task, current_task_cpu_time_ns, current_task_direct, current_task_fast,
    current_task_fast_direct, current_task_handoff_target, current_task_id, current_task_on,
    current_task_ref,
    defer_task_wake, defer_timer_tick, drain_deferred_timer_tick, enqueue_task,
    enqueue_task_deferred, enqueue_task_preferred, enqueue_task_preferred_for_handoff,
    enqueue_task_with_hint, group_exit_wakeup, idle_task, init, init_task, install_idle,
    is_cpu_active, is_cpu_online, is_ready, is_ready_direct, mark_cpu_online, migrate_task,
    needs_resched, needs_resched_current, now_ns_direct, now_ns_public, offline_cpu, on_timer_tick,
    online_cpu_mask, pid_count,
    preempt_if_needed, register_cpu, register_sleep_deadline, reprogram_current_deadline,
    request_balance, request_post_syscall_handoff, request_post_syscall_handoff_to,
    request_resched, root_pid_ns, run_post_syscall_handoff, run_post_syscall_handoff_lazy,
    sched_rr_timeslice_ms, sched_rr_timeslice_ns, sched_rt_period_us, sched_rt_runtime_us,
    schedule_once, scheduler_diag, set_realtime_itimer, set_sched_rr_timeslice_ms,
    set_sched_rt_period_us, set_sched_rt_runtime_us, signal_wakeup, spawn_idle_for,
    supported_cpu_mask, try_current_task_ref,
};
pub use scheduler::{RealtimeItimerSpec, get_realtime_itimer};
pub use scheduler::{adopt_cpu_current, cpu_start_scheduling, spawn_idle_for_cpu};
#[cfg(feature = "performance-profile")]
pub use scheduler::{current_profile_image, current_profile_session_id};
#[cfg(feature = "performance-profile")]
pub use scheduler::{current_profile_span_id, set_current_profile_span_id};
pub use scheduler::{
    current_sched_domain_id, install_sched_topology, sched_domain_stats, sched_topology,
    task_sched_placement,
};
pub use scheduler_state::{
    CpuSchedState, HandoffReason, HandoffTarget, SchedDomainStats, Scheduler, TopologySnapshot,
};
pub use signal::{
    DefaultAction, SharedSignal, SigAction, SigActionFlags, SigHandler, SigInfo, SigProcMaskHow,
    SigSet, SignalNumber, SignalObserver, SignalState,
};
pub use spawn::activate_task_with_cpu_hint;
pub use spawn::{
    SpawnKind, abort_new_task, activate_task, clone_task, exit_task, kthread_create,
    kthread_finish, kthread_spawn, kthread_spawn_on_cpu, list_zombie_children, reap_child,
    reap_matching, reparent_to_init, spawn_child,
};
pub use task::{
    DEFAULT_TIMER_SLACK_NS, ExecutionActionClaim, ExecutionScopeKind, ExitCode, RobustListState,
    RseqRegistration, SigAltStack, TASK_COMM_LEN, TASKEXT_ELM_EXECUTION, TASKEXT_EXEC_ACCESS,
    TASKEXT_EXEC_ARGS, TASKEXT_EXEC_ENVP, TASKEXT_EXEC_PATH, TASKEXT_RISCV_VECTOR_SIGNAL_STACK,
    TASKEXT_RISCV_VECTOR_STATE, TASKEXT_USER_TRAP_FRAME, TASKEXT_VFS_CONTEXT, TASKEXT_VFS_FDTABLE,
    TASKEXT_VM_SPACE, Task, TaskDiag, TaskExitAccountingHook, TaskExt, TaskExtCloneHook,
    TaskExtExitHook, TaskExtKey, TaskKind, TaskPreExitHook, TaskState, TaskUsage, WaitReason,
    ext_clone_hook, ext_exit_hook, pre_exit_hook, register_exit_accounting_hook,
    register_ext_clone_hook, register_ext_exit_hook, register_pre_exit_hook, task_diag,
};
pub use wait::{WaitQueue, WaitQueueEntry};
pub use wait_flags::{WaitId, WaitOptions, WaitResult, WaitStatus};

/// 强制链接器保留调度子系统直接符号所在的代码生成单元。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    scheduler::current_cpu_id as usize
        ^ spawn::spawn_child as usize
        ^ operation::getpid as usize
        ^ operation::spawn_user_process as usize
}

#[cfg(test)]
mod tests;
