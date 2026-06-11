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
//! 1. `Runqueue::inner` —— 每 CPU 一把，严禁跨 rq 反序。
//! 2. `Task::rel` —— 亲子关系（parent / children / tg_link / pid_in_ns）。
//! 3. `Task::creds` / `Task::shared_signal` / `Task::kstack` / `Task::ctx`
//!    / `Task::ext` —— 同一 Task 内的次级字段锁，彼此独立，禁止互相嵌套。
//! 4. `ThreadGroup::members` / `ProcessGroup::members` / `Session::groups` ——
//!    组成员索引。
//! 5. `SharedSignal::actions` / `SharedSignal::shared_pending_infos` ——
//!    tg 共享信号表。
//! 6. `WaitQueue::waiters` —— 等待者列表。
//! 7. `SignalState::pending_infos` —— per-task 信号队列。
//!
//! 调用可能触发唤醒 / 分配的函数前必须释放所有 rq 锁。
//!
//! 依赖 `alloc` 的 `Arc` / `Vec` / `BTreeMap`；不做堆外分配。原子操作必须
//! 显式传入 [`core::sync::atomic::Ordering`]。

extern crate alloc;

pub mod arch_hooks;
pub mod clone_flags;
pub mod eevdf;
pub mod group;
pub mod ids;
pub mod operation;
pub mod pid;
pub mod process_ops;
pub mod rlimit;
pub mod runqueue;
pub mod sched_class;
pub mod scheduler;
pub mod signal;
pub mod spawn;
pub mod sync;
pub mod task;
pub mod wait;
pub mod wait_flags;

pub use arch_hooks::{ArchContextOps, CpuControlOps, KernelEntry};
pub use clone_flags::{CloneArgs, CloneFlags};
pub use eevdf::{SchedEntity, SchedParams, Weight};
pub use group::{ProcessGroup, Session, ThreadGroup};
pub use ids::{CapSet, Capability, Credentials, Gid, Uid};
pub use pid::{PidNamespace, PidRegistry, PidT};
pub use process_ops::{
    ExecRequest, ProcessImageOps, UserContextRef, process_image_ops, register_process_image_ops,
};
pub use rlimit::{Resource, Rlim, RlimitError, RlimitPair, Rlimits, RlimitsLock};
pub use runqueue::Runqueue;
pub use sched_class::{
    DEFAULT_DL_DEADLINE_NS, DEFAULT_DL_PERIOD_NS, DEFAULT_DL_RUNTIME_NS, DEFAULT_RR_SLICE_NS,
    RT_PRIO_MAX, RT_PRIO_MIN, SchedAttr, SchedClass, SchedPolicy,
};
pub use scheduler::cancel_sleep_deadline;
pub use scheduler::{
    NR_CPUS, balance_once, current_cpu_id, current_task, current_task_on, enqueue_task, idle_task,
    init, init_task, install_idle, is_cpu_online, is_ready, migrate_task, needs_resched,
    now_ns_public, on_timer_tick, online_cpu_mask, pid_count, preempt_if_needed, register_cpu,
    register_sleep_deadline, request_post_syscall_handoff, request_resched, root_pid_ns,
    run_post_syscall_handoff, runqueue, runqueue_of, schedule_once, set_realtime_itimer,
    signal_wakeup, spawn_idle_for, supported_cpu_mask,
};
pub use scheduler::{RealtimeItimerSpec, get_realtime_itimer};
pub use scheduler::{adopt_cpu_current, cpu_start_scheduling, spawn_idle_for_cpu};
pub use signal::{
    DefaultAction, SharedSignal, SigAction, SigActionFlags, SigHandler, SigInfo, SigProcMaskHow,
    SigSet, SignalNumber, SignalState,
};
pub use spawn::{
    SpawnKind, abort_new_task, activate_task, clone_task, exit_task, kthread_create,
    kthread_finish, kthread_spawn, list_zombie_children, reap_child, reap_matching,
    reparent_to_init, spawn_child,
};
pub use task::{
    ExitCode, RobustListState, RseqRegistration, TASK_COMM_LEN, TASKEXT_EXEC_ARGS,
    SigAltStack, TASKEXT_EXEC_ENVP, TASKEXT_EXEC_PATH, TASKEXT_USER_TRAP_FRAME,
    TASKEXT_VFS_CONTEXT, TASKEXT_VFS_FDTABLE, TASKEXT_VM_SPACE, Task, TaskExt, TaskExtCloneHook,
    TaskExtKey, TaskState, TaskUsage, ext_clone_hook, register_ext_clone_hook,
};
pub use wait::WaitQueue;
pub use wait_flags::{WaitId, WaitOptions, WaitResult, WaitStatus};

#[cfg(test)]
mod tests;
