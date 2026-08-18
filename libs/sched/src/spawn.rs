//! 任务派生 / 退出 / 回收，以及内核线程入口。
//!
//! 本模块覆盖 fork / clone / exit / wait 的"图操作"——更新 Task / Group /
//! Runqueue 的关系网，并通过注册的 `TaskExtCloneHook` 让上层（VFS 等）参与
//! fork 决策。具体的 syscall 接入由 [`crate::operation`] 调用本模块的函数。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::arch_hooks::KernelEntry;
use crate::clone_flags::{CloneArgs, CloneFlags};
use crate::eevdf::SchedParams;
use crate::group::{ProcessGroup, Session, ThreadGroup};
use crate::pid::{PidNamespace, PidT};
use crate::sched_class::{SchedAttr, SchedPolicy};
use crate::scheduler::{
    activate_task_on_cpu, current_task, deliver_shared_signal_to_group, enqueue_task,
    enqueue_task_with_hint, init_task, is_current_on_any_cpu, mark_task_exited, now_ns_public,
    root_pid_ns, schedule_once,
};
use crate::signal::SignalNumber;
use crate::sync::Spinlock;
use crate::task::{Task, ext_clone_hook};
use crate::{ExitCode, TaskState};

/// 派生类型：新进程 vs 新线程。
#[derive(Debug, Clone, Copy)]
pub enum SpawnKind {
    /// 新进程：独立 ThreadGroup，继承父的 ProcessGroup（fork 语义）。
    Process,
    /// 新线程：共享父的 ThreadGroup 与 ProcessGroup。
    Thread,
}

#[cfg(feature = "performance-profile")]
fn register_profile_child(parent: &Arc<Task>, child: &Arc<Task>, pid: crate::pid::PidT) {
    let Some(parent_pid) = parent.pid_root_cached() else {
        return;
    };
    profiling::trace_task_spawn(parent_pid as u64, pid as u64);
    let session = child.profile_session_id();
    if session == 0 {
        return;
    }
    profiling::register_task(
        session,
        pid as u64,
        parent_pid as u64,
        child.tgid_cached().unwrap_or(pid) as u64,
    );
    profiling::record_task_images(
        session,
        pid as u64,
        child.profile_main_image(),
        child.profile_interpreter_image(),
    );
}

// ── pid 命名空间 ─────────────────────────────────────────────────────────────

/// 子进程 pid 命名空间钩子：由 kernel 注册（读任务的 pending 命名空间）。
/// 返回 `None` 时使用父任务自身的 pid 命名空间。
static CHILD_PID_NS_HOOK: Spinlock<Option<fn(&Arc<Task>) -> Option<Arc<PidNamespace>>>> =
    Spinlock::new(None);

pub fn register_child_pid_ns_hook(hook: fn(&Arc<Task>) -> Option<Arc<PidNamespace>>) {
    *CHILD_PID_NS_HOOK.lock() = Some(hook);
}

fn child_pid_ns(parent: &Arc<Task>) -> Arc<PidNamespace> {
    let hook = *CHILD_PID_NS_HOOK.lock();
    if let Some(hook) = hook {
        if let Some(ns) = hook(parent) {
            return ns;
        }
    }
    parent.pid_ns()
}

/// 把任务注册进 pid 命名空间链（自身 ns 起，直到根），返回根 ns 的 pid。
pub(crate) fn register_pid_chain(task: &Arc<Task>) -> Result<PidT, ()> {
    let mut ns = task.pid_ns();
    let mut root_pid = None;
    loop {
        let pid = ns.registry().allocate(task).ok_or(())?;
        if ns.parent().is_none() {
            root_pid = Some(pid);
        }
        task.register_pid(Arc::clone(&ns), pid);
        if ns.parent().is_none() {
            break;
        }
        ns = Arc::clone(ns.parent().expect("parent() 已判 Some"));
    }
    root_pid.ok_or(())
}

// ── 简单 spawn（不带 CloneFlags） ────────────────────────────────────────────

/// 从 `parent` 派生一个新任务：分配 pid、登记亲缘 / 组关系，但不入 runqueue。
///
/// 调用方必须先安装执行上下文，再调用 [`activate_task`]。这样不会把半初始化
/// 任务暴露给调度器。
#[kernel_symbols::export(name = "sched.spawn.spawn_child", contract = "kernel.sched.task-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
pub fn spawn_child(parent: &Arc<Task>, kind: SpawnKind, params: SchedParams) -> Arc<Task> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::ProcessClone);
    let root_ns = root_pid_ns();

    let (tgroup, pgroup) = match kind {
        SpawnKind::Thread => (parent.thread_group(), parent.process_group()),
        SpawnKind::Process => {
            // 不共享：新建 TG，并拷贝父进程 rlimit。
            let parent_tg = parent.thread_group();
            let tg = ThreadGroup::new();
            {
                let src = parent_tg.rlimits().lock();
                let mut dst = tg.rlimits().lock();
                *dst = src.fork_copy();
            }
            (tg, parent.process_group())
        }
    };

    let child = Task::new(
        params,
        Arc::downgrade(parent),
        Arc::clone(&tgroup),
        Arc::clone(&pgroup),
    );
    child.inherit_timer_slack_from(parent);
    #[cfg(feature = "performance-profile")]
    child.inherit_profile_session_from(parent);
    child.set_pid_ns(child_pid_ns(parent));

    if matches!(kind, SpawnKind::Process) {
        tgroup.set_leader(&child);
    }
    if !tgroup.try_add_member(&child) {
        child.set_state(TaskState::Dead);
        return child;
    }
    pgroup.add_member(&child);

    parent.add_child(Arc::clone(&child));

    let Ok(pid) = register_pid_chain(&child) else {
        log::warning!(
            "[sched][spawn] pid allocation failed kind={:?} parent_pid={:?}",
            kind,
            parent.pid_root(),
        );
        abort_new_task(&child);
        return child;
    };
    if matches!(kind, SpawnKind::Process) {
        tgroup.set_tgid(pid);
        child.set_tgid_cache(pid);
    } else {
        child.set_tgid_cache(tgroup.tgid());
    }
    if pgroup.pgid() <= 0 {
        pgroup.set_pgid(pid);
    }

    #[cfg(feature = "performance-profile")]
    register_profile_child(parent, &child, pid);

    #[cfg(feature = "trace-task-lifecycle")]
    log::debug!(
        "[sched][spawn] kind={:?} pid={} parent_pid={:?}",
        kind,
        pid,
        parent.pid_root(),
    );
    child
}

/// 派生由整个线程组拥有的 Native child。
///
/// Native ABI 的 `process.spawn` 不把 child 绑定到发起调用的线程：所有权、wait
/// 与 reap 都归属于 `owner` 线程组。Task 的 POSIX parent 保持为空，避免某个
/// 非 leader 线程退出时把仍属存活线程组的 child 过继给 init。
pub fn spawn_native_child(
    parent: &Arc<Task>,
    params: SchedParams,
) -> Result<Arc<Task>, errno::Errno> {
    let root_ns = root_pid_ns();
    let owner = parent.thread_group();
    let child_group = ThreadGroup::new();
    {
        let src = owner.rlimits().lock();
        let mut dst = child_group.rlimits().lock();
        *dst = src.fork_copy();
    }
    let process_group = parent.process_group();
    let child = Task::new(
        params,
        Weak::new(),
        Arc::clone(&child_group),
        Arc::clone(&process_group),
    );
    child.set_native_owner(&owner);
    child.inherit_timer_slack_from(parent);
    child.set_credentials(parent.credentials());
    #[cfg(feature = "performance-profile")]
    child.inherit_profile_session_from(parent);

    child_group.set_leader(&child);
    if !child_group.try_add_member(&child) {
        child.set_state(TaskState::Dead);
        return Err(errno::Errno::EBUSY);
    }
    process_group.add_member(&child);
    match owner.try_add_native_child(Arc::clone(&child)) {
        Ok(true) => {}
        Ok(false) => {
            abort_new_task(&child);
            return Err(errno::Errno::EBUSY);
        }
        Err(_) => {
            abort_new_task(&child);
            return Err(errno::Errno::ENOMEM);
        }
    }

    let pid = register_pid_chain(&child).map_err(|_| {
        abort_new_task(&child);
        errno::Errno::ENOMEM
    })?;
    child_group.set_tgid(pid);
    child.set_tgid_cache(pid);
    if process_group.pgid() <= 0 {
        process_group.set_pgid(pid);
    }

    #[cfg(feature = "performance-profile")]
    register_profile_child(parent, &child, pid);

    Ok(child)
}

/// 创建一个尚未激活的 Native 用户线程。
///
/// 新线程共享调用者的线程组、进程组与 personality，但不进入 POSIX
/// parent/children 图，也不携带退出信号。调用方必须先安装 VM 和用户上下文，
/// 再调用 [`activate_task`]。
pub fn spawn_native_thread(
    parent: &Arc<Task>,
    params: SchedParams,
) -> Result<Arc<Task>, errno::Errno> {
    let root_ns = root_pid_ns();
    let group = parent.thread_group();
    let Some(exec) = group.lock_for_clone() else {
        return Err(errno::Errno::EBUSY);
    };
    if group.user_abi_kind() != native_abi::UserAbiKind::MygoNative
        || group.group_exit_status().is_some()
    {
        return Err(errno::Errno::EBUSY);
    }

    let process_group = parent.process_group();
    let child = Task::new(
        params,
        Weak::new(),
        Arc::clone(&group),
        Arc::clone(&process_group),
    );
    child.set_pid_ns(child_pid_ns(parent));
    child.set_exit_signal(0);
    child.set_credentials(parent.credentials());
    child.inherit_timer_slack_from(parent);
    #[cfg(feature = "performance-profile")]
    child.inherit_profile_session_from(parent);

    if !exec.try_add_member(&child) {
        child.set_state(TaskState::Dead);
        return Err(errno::Errno::EBUSY);
    }
    process_group.add_member(&child);

    if register_pid_chain(&child).is_err() {
        abort_new_task(&child);
        return Err(errno::Errno::ENOMEM);
    }
    child.set_tgid_cache(group.tgid());

    #[cfg(feature = "performance-profile")]
    register_profile_child(parent, &child, pid);
    Ok(child)
}

/// 把已经安装执行上下文的任务放入合适的 runqueue。
#[kernel_symbols::export(name = "sched.spawn.activate_task", contract = "kernel.sched.task-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn activate_task(task: &Arc<Task>) -> Result<usize, errno::Errno> {
    if task.arch_context().is_none() {
        return Err(errno::Errno::EINVAL);
    }
    match task.state() {
        TaskState::New | TaskState::Runnable | TaskState::Sleeping => {}
        TaskState::Running
        | TaskState::Uninterruptible
        | TaskState::Stopped
        | TaskState::Continued
        | TaskState::Zombie
        | TaskState::Dead => return Err(errno::Errno::EINVAL),
    }
    #[cfg(feature = "performance-profile")]
    if task.state() == TaskState::Sleeping {
        task.mark_profile_woken(now_ns_public());
    }
    Ok(enqueue_task(Arc::clone(task), now_ns_public()))
}

/// 唤醒任务并优先放到指定 CPU；提示不可用时仍会选择其它合法 CPU。
pub fn activate_task_with_cpu_hint(
    task: &Arc<Task>,
    cpu_hint: usize,
) -> Result<usize, errno::Errno> {
    if task.arch_context().is_none() {
        return Err(errno::Errno::EINVAL);
    }
    match task.state() {
        TaskState::New | TaskState::Runnable | TaskState::Sleeping => {}
        TaskState::Running
        | TaskState::Uninterruptible
        | TaskState::Stopped
        | TaskState::Continued
        | TaskState::Zombie
        | TaskState::Dead => return Err(errno::Errno::EINVAL),
    }
    #[cfg(feature = "performance-profile")]
    if task.state() == TaskState::Sleeping {
        task.mark_profile_woken(now_ns_public());
    }
    Ok(enqueue_task_with_hint(
        Arc::clone(task),
        cpu_hint,
        now_ns_public(),
    ))
}

/// 回滚尚未运行、尚未入队的新任务。用于 clone/exec 安装用户上下文失败的路径。
#[kernel_symbols::export(name = "sched.spawn.abort_new_task", contract = "kernel.sched.task-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn abort_new_task(task: &Arc<Task>) {
    crate::scheduler::deadline_admission().release(task);
    if let Some(owner) = task.native_owner() {
        owner.remove_native_child(task);
    }
    if let Some(parent) = task.parent() {
        let _ = parent.remove_child(task);
    }
    for (ns, pid) in task.pid_namespaces_snapshot() {
        ns.registry().release(pid);
    }
    let group = task.thread_group();
    if group.remove_member(task) {
        group.cancel_member_accounting();
    }
    task.process_group().remove_member(task);
    task.set_state(TaskState::Dead);
    if group.mark_terminated_if_all_members_terminal() {
        notify_terminated_thread_group(&group);
    }
}

// ── 完整 clone：处理 CLONE_* flags、ext hook、vfork ──────────────────────────

/// POSIX clone(2)：根据 `args.flags` 决定 ThreadGroup / SharedSignal / 父
/// 选择 / vfork 阻塞，并通过注册的 [`crate::task::TaskExtCloneHook`]
/// 让上层处理 VFS / FdTable 的拷贝。
#[kernel_symbols::export(name = "sched.spawn.clone_task", contract = "kernel.sched.task-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
pub fn clone_task(parent: &Arc<Task>, args: CloneArgs, params: SchedParams) -> Arc<Task> {
    let flags = args.flags;
    let root_ns = root_pid_ns();
    let parent_tg = parent.thread_group();
    let Some(parent_exec) = parent_tg.lock_for_clone() else {
        let rejected = Task::new(
            params,
            Arc::downgrade(parent),
            Arc::clone(&parent_tg),
            parent.process_group(),
        );
        rejected.set_state(TaskState::Dead);
        return rejected;
    };

    // 1. 决定 thread group：CLONE_THREAD 共享，否则新建。
    let new_tg = if flags.has(CloneFlags::CLONE_THREAD) {
        Arc::clone(&parent_tg)
    } else {
        // CLONE_SIGHAND 只共享 handler 表；独立线程组必须拥有自己的
        // 进程级 pending 队列。CLONE_THREAD 才复用完整 SharedSignal。
        let tg = if flags.has(CloneFlags::CLONE_SIGHAND) {
            ThreadGroup::new_sharing_signal(Arc::new(parent_tg.shared_signal().clone_sighand()))
        } else {
            // 不共享时深拷 sigaction；CLEAR_SIGHAND 只重置用户处理函数，父进程
            // 显式忽略的信号仍须保持 SIG_IGN。
            let copied = if flags.has(CloneFlags::CLONE_CLEAR_SIGHAND) {
                parent_tg.shared_signal().fork_copy_clearing_handlers()
            } else {
                parent_tg.shared_signal().fork_copy()
            };
            ThreadGroup::new_sharing_signal(Arc::new(copied))
        };
        {
            let src = parent_tg.rlimits().lock();
            let mut dst = tg.rlimits().lock();
            *dst = src.fork_copy();
        }
        tg
    };

    // 2. 进程组：clone 不引入新 pgroup（setpgid 才会改）。
    let pg = parent.process_group();

    // 3. 父选择：CLONE_PARENT → 新任务的父等于 parent.parent；否则 parent。
    let mut real_parent = if flags.has(CloneFlags::CLONE_PARENT) {
        parent.parent().unwrap_or_else(init_task)
    } else {
        Arc::clone(parent)
    };

    // 4. 创建任务。Task::new 已根据 thread_group 复制了 SharedSignal Arc。
    let child = Task::new(
        params,
        Arc::downgrade(&real_parent),
        Arc::clone(&new_tg),
        Arc::clone(&pg),
    );
    // 4.1) pid 命名空间：子进程按 pending/父进程 ns 链注册（CLONE_NEWPID
    //      在 fork 时对子进程生效）。
    child.set_pid_ns(child_pid_ns(parent));
    child.inherit_timer_slack_from(parent);
    #[cfg(feature = "performance-profile")]
    child.inherit_profile_session_from(parent);
    // 5. 凭据：所有 fork/clone 都拷贝父的当前凭据（写时复制）。
    child.set_credentials(parent.credentials());
    if flags.has(CloneFlags::CLONE_VM) && !flags.has(CloneFlags::CLONE_VFORK) {
        child.clear_sigaltstack();
    } else {
        child.set_sigaltstack(parent.sigaltstack());
    }
    if parent.sched_reset_on_fork() && !flags.has(CloneFlags::CLONE_THREAD) {
        // 父任务通过 SCHED_RESET_ON_FORK 要求子进程不能继承 RT/deadline
        // 或负 nice 权重；子任务自身不继续携带该继承标志。
        let parent_attr = parent.pi_base_attr();
        let child_attr = match parent_attr.policy {
            SchedPolicy::Fair | SchedPolicy::Idle => SchedAttr::fair(parent_attr.nice.max(0), 0),
            SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin | SchedPolicy::Deadline => {
                SchedAttr::fair(0, 0)
            }
        };
        child.set_sched_attr(child_attr);
        child.set_sched_reset_on_fork(false);
    }

    // 6. 退出信号：CLONE_THREAD 不发；否则取 flags 低 8 位（0 → SIGCHLD）。
    let Some(raw_exit_sig) = args.exit_signal_checked() else {
        log::warning!(
            "[sched][clone] invalid exit_signal={} flags={:#x}",
            args.exit_signal,
            flags.raw(),
        );
        child.set_state(TaskState::Dead);
        return child;
    };
    let exit_sig = if flags.has(CloneFlags::CLONE_THREAD) {
        if raw_exit_sig != 0 {
            log::warning!(
                "[sched][clone] CLONE_THREAD with non-zero exit_signal={} flags={:#x}",
                raw_exit_sig,
                flags.raw(),
            );
            child.set_state(TaskState::Dead);
            return child;
        }
        0
    } else if raw_exit_sig == 0 {
        SignalNumber::SIGCHLD.raw() as i32
    } else {
        raw_exit_sig as i32
    };
    child.set_exit_signal(exit_sig);

    // 7. tg / pg 登记。
    if !flags.has(CloneFlags::CLONE_THREAD) {
        new_tg.set_leader(&child);
    }
    let member_added = if flags.has(CloneFlags::CLONE_THREAD) {
        parent_exec.try_add_member(&child)
    } else {
        new_tg.try_add_member(&child)
    };
    if !member_added {
        child.set_state(TaskState::Dead);
        return child;
    }
    pg.add_member(&child);

    // 8. 父登记（亲缘图保活）。CLONE_THREAD 线程不进入普通 child/wait 模型。
    if !flags.has(CloneFlags::CLONE_THREAD) {
        let identity = crate::pid::lock_process_identity();
        // CLONE_PARENT 的 real_parent 可能属于另一个线程组；等待身份事务后
        // 必须重新读取，不能继续使用 exec 前缓存的旧 leader。
        if flags.has(CloneFlags::CLONE_PARENT) {
            real_parent = parent
                .parent_in(&identity)
                .filter(|candidate| {
                    !matches!(candidate.state(), TaskState::Zombie | TaskState::Dead)
                })
                .unwrap_or_else(init_task);
        }
        if real_parent.try_reserve_children_for_exec(1).is_err() {
            drop(identity);
            abort_new_task(&child);
            return child;
        }
        child.reparent_to_in(&identity, &real_parent);
        real_parent.add_child_in(&identity, Arc::clone(&child));
    }

    // 9. 分配 pid（根 ns 一次；多 ns 留待后续）。
    let pid = if args.requested_pid > 0 {
        match root_ns
            .registry()
            .allocate_specific(&child, args.requested_pid)
        {
            Ok(pid) => pid,
            Err(err) => {
                log::warning!(
                    "[sched][clone] requested pid allocation failed parent_pid={:?} requested_pid={} err={:?}",
                    real_parent.pid_root(),
                    args.requested_pid,
                    err,
                );
                abort_new_task(&child);
                return child;
            }
        }
    } else {
        let Ok(pid) = register_pid_chain(&child) else {
            log::warning!(
                "[sched][clone] pid allocation failed parent_pid={:?} flags={:#x}",
                real_parent.pid_root(),
                flags.raw(),
            );
            abort_new_task(&child);
            return child;
        };
        pid
    };
    if !flags.has(CloneFlags::CLONE_THREAD) {
        new_tg.set_tgid(pid);
        child.set_tgid_cache(pid);
    } else {
        child.set_tgid_cache(new_tg.tgid());
    }
    if pg.pgid() <= 0 {
        pg.set_pgid(pid);
    }

    #[cfg(feature = "performance-profile")]
    register_profile_child(&real_parent, &child, pid);

    // 10. ext clone hook：把上层注册的 VFS / fdtable 等子系统状态按 flags 拷贝。
    if let Some(hook) = ext_clone_hook() {
        for (key, src) in parent.ext_snapshot() {
            let dst = hook.clone_for(key, &src, flags);
            child.ext_install(key, dst);
        }
    } else {
        // 无 hook 时，按"全共享"复制 Arc，保持调用链可工作。
        for (key, src) in parent.ext_snapshot() {
            child.ext_install(key, src);
        }
    }

    #[cfg(feature = "trace-task-lifecycle")]
    log::debug!(
        "[sched][clone] pid={} parent_pid={:?} flags={:#x} exit_sig={}",
        pid,
        real_parent.pid_root(),
        flags.raw(),
        exit_sig,
    );

    child
}

// ── exit / reap ──────────────────────────────────────────────────────────────

/// 向单个可等待任务的父进程发布退出事件。
fn notify_task_parent(task: &Arc<Task>) {
    let Some(parent) = task.parent() else {
        return;
    };
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[sched][exit-notify] child={:?} parent={:?} child_state={:?}",
        task.pid_root(),
        parent.pid_root(),
        task.state(),
    );
    let exit_sig = task.exit_signal();
    if exit_sig > 0
        && let Some(sig) = SignalNumber::from_raw(exit_sig)
    {
        let info = crate::signal::SigInfo {
            sig,
            code: 1, // CLD_EXITED
            sender_pid: task.pid_root().unwrap_or(0),
            sender_uid: crate::ids::Uid::ROOT,
            raw: None,
        };
        deliver_shared_signal_to_group(&parent.thread_group(), info);
    }
    parent.exit_waiters.wake_all();
}

/// 最后一个线程完成退出后，重新发布此前被延迟的 leader 退出事件。
fn notify_terminated_thread_group(group: &Arc<ThreadGroup>) {
    let Some(leader) = group.leader() else {
        return;
    };
    if leader.state() == TaskState::Zombie {
        notify_task_parent(&leader);
    }
}

/// Native owner 线程组全部退出后，按普通进程亲缘模型把尚未回收的 child 交给
/// system reaper。迁移 parent 指针与 init 的 children 登记由身份事务一起保护，
/// 防止 child 的退出通知落在半迁移状态。
fn reparent_native_children_to_init(owner: &Arc<ThreadGroup>) {
    if !owner.has_native_children() {
        return;
    }
    let init = init_task();
    if Arc::ptr_eq(&init.thread_group(), owner) {
        return;
    }
    let children = owner.take_native_children_for_reparent();
    if children.is_empty() {
        return;
    }
    {
        let identity = crate::pid::lock_process_identity();
        for child in children.iter() {
            child.reparent_to_in(&identity, &init);
            init.add_child_in(&identity, Arc::clone(child));
        }
    }
    for child in children {
        if child.is_waitable_zombie() {
            notify_task_parent(&child);
        }
    }
}

/// 标记任务退出：出 runqueue、置 Zombie、唤醒 `exit_waiters`，把退出信号
/// 投递给父，唤醒 vfork_done。**不**释放 pid 槽——zombie 期间父按 pid 仍能查到。
///
/// 不切换 CPU；调用方决定何时调 [`schedule_once`]。
/// `PTRACE_O_TRACEEXIT` 选项位。
pub const PTRACE_O_TRACEEXIT: u64 = 0x0000_0040;
/// `PTRACE_EVENT_EXIT` 事件号。
pub const PTRACE_EVENT_EXIT: u16 = 6;

#[kernel_symbols::export(name = "sched.spawn.exit_task", contract = "kernel.sched.task-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn exit_task(task: &Arc<Task>, code: ExitCode) {
    if task.is_idle_task() {
        log::error!(
            "[sched][exit] refusing to exit idle task pid={:?}",
            task.pid_root(),
        );
        return;
    }

    let group = task.thread_group();
    let is_group_leader = task.is_thread_group_leader();
    let preserve_exec_identity = task.exec_sibling_exit_preserves_identity();

    task.cleanup_before_exit();

    // TRACEEXIT：退出前停一次，tracer 处理完（PTRACE_CONT）后才继续退出。
    if task.is_ptrace_traced() && task.ptrace_options() & PTRACE_O_TRACEEXIT != 0 {
        task.set_ptrace_event_msg(code.0 as i64);
        task.set_ptrace_stop_event(PTRACE_EVENT_EXIT);
        task.clear_ptrace_last_siginfo();
        let _ = task.mark_stopped_with_raw_sig(crate::signal::SignalNumber::SIGTRAP.raw() as i32);
        while task.state() == crate::task::TaskState::Stopped {
            crate::scheduler::schedule_once(crate::scheduler::now_ns_public());
        }
    }

    crate::scheduler::deadline_admission().release(task);

    // 1) 先把自己的子任务托管给 init，让它们在父死后仍有 reaper。
    //    init 任务本身退出（正常情况下不会发生）时跳过，避免自引用成环。
    let children = if preserve_exec_identity {
        Vec::new()
    } else {
        task.snapshot_children()
    };
    if !children.is_empty() {
        let init = init_task();
        if !Arc::ptr_eq(&init, task) {
            task.reparent_children_to(&init);
            // 已经是 Zombie 的子需要让 init 感知——否则它们的 SIGCHLD 之前投给
            // 了旧父，init 不会来 reap。这里只对刚被过继的 Zombie 重投一次。
            for c in children.iter() {
                if c.is_waitable_zombie() {
                    if let Some(sig) = SignalNumber::from_raw(SignalNumber::SIGCHLD.raw() as i32) {
                        let info = crate::signal::SigInfo {
                            sig,
                            code: 1, // CLD_EXITED
                            sender_pid: c.pid_root().unwrap_or(0),
                            sender_uid: crate::ids::Uid::ROOT,
                            raw: None,
                        };
                        deliver_shared_signal_to_group(&init.thread_group(), info);
                    }
                }
            }
        }
    }

    mark_task_exited(task, code);
    let group_terminated = group.mark_terminated_if_all_members_terminal();
    if group_terminated {
        reparent_native_children_to_init(&group);
    }
    if !is_current_on_any_cpu(task) {
        task.cleanup_exit_extensions();
    }

    // 唤醒 vfork 父。
    if task.is_vforking() {
        task.set_vforking(false);
        task.vfork_done.wake_all();
    }

    // CLONE_THREAD 成员的 exit_signal 为 0，直接释放独立 TID；线程组 leader
    // 必须保留 Zombie/PID，直到所有成员终止后才允许父进程观察和回收。
    let exit_sig = task.exit_signal();
    if preserve_exec_identity {
        // 非 leader 执行 exec 时，旧 leader 先停止执行并释放线程资源，但保持
        // Zombie、PID、父侧 child 项和组成员资格，直到身份迁移完整成功。
    } else if exit_sig == 0 {
        for (ns, pid) in task.pid_namespaces_snapshot() {
            ns.registry().release(pid);
        }
        group.remove_member(task);
        task.process_group().remove_member(task);
        task.set_state(TaskState::Dead);
        task.exit_waiters.wake_all();
    } else if !is_group_leader {
        // 非 leader 若显式携带退出信号，仍保持逐任务通知语义。
        notify_task_parent(task);
    }

    if group_terminated {
        #[cfg(feature = "trace-task-lifecycle")]
        log::info!(
            "[sched][group-terminated] pid={:?} leader={} state={:?}",
            task.pid_root(),
            is_group_leader,
            task.state(),
        );
        notify_terminated_thread_group(&group);
    }
}

/// 父侧 reap：从 children 取走首个 Zombie 子，释放其 pid，返回 (任务, 退出码)。
#[kernel_symbols::export(name = "sched.spawn.reap_child", contract = "kernel.sched.task-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
pub fn reap_child(parent: &Arc<Task>) -> Option<(Arc<Task>, ExitCode)> {
    reap_matching(parent, |_| true)
}

/// 带谓词的 reap：找出第一个匹配且 Zombie 的子。
pub fn reap_matching<F>(parent: &Arc<Task>, mut pred: F) -> Option<(Arc<Task>, ExitCode)>
where
    F: FnMut(&Arc<Task>) -> bool,
{
    let zombie = parent.reap_matching(|t| t.is_user_task() && pred(t))?;
    let code = zombie
        .exit_code()
        .expect("[sched][reap] zombie without exit code");
    let usage = zombie.usage_snapshot(now_ns_public());
    parent.add_child_usage(usage);

    // 归还 pid 槽。
    for (ns, pid) in zombie.pid_namespaces_snapshot() {
        ns.registry().release(pid);
    }

    // 从 tg / pg 索引清理。
    zombie.thread_group().remove_member(&zombie);
    zombie.process_group().remove_member(&zombie);

    // 执行体不能在 reap 这里释放：任务可能仍是某个 CPU 的 current，或者正处于
    // 调度器发布 next、尚未完成上下文切换的窗口。exit_task 已经负责非 current
    // 任务的扩展清理；current 任务会在最终切出后的 retired 队列中统一清理。
    // 这里只移除父子关系和 PID 所有权，剩余执行体由调度器或 Task 的 Arc 生命周期
    // 回收，避免父进程与远端调度器并发释放上下文/内核栈。

    debug_assert_eq!(zombie.state(), TaskState::Dead);
    #[cfg(feature = "trace-task-lifecycle")]
    log::debug!(
        "[sched][reap] parent_pid={:?} child_pid={:?} code={}",
        parent.pid_root(),
        zombie.pid_root(),
        code.0,
    );
    Some((zombie, code))
}

/// Native owner 线程组侧的 reap。Tomori 的 Task parent/children 表保持不变；
/// 该入口只消费 `ThreadGroup` 的 Native child 表。
pub fn reap_native_child<F>(owner: &Arc<ThreadGroup>, mut pred: F) -> Option<(Arc<Task>, ExitCode)>
where
    F: FnMut(&Arc<Task>) -> bool,
{
    let zombie = owner.reap_native_child(|task| pred(task))?;
    let code = zombie
        .exit_code()
        .expect("[sched][native-reap] zombie without exit code");

    for (ns, pid) in zombie.pid_namespaces_snapshot() {
        ns.registry().release(pid);
    }
    zombie.thread_group().remove_member(&zombie);
    zombie.process_group().remove_member(&zombie);

    debug_assert_eq!(zombie.state(), TaskState::Dead);
    Some((zombie, code))
}

/// 列出 `parent` 当前所有 Zombie 子。便于 wait4(WNOHANG) 等不阻塞探查。
#[kernel_symbols::export(name = "sched.spawn.list_zombie_children", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
pub fn list_zombie_children(parent: &Arc<Task>) -> Vec<Arc<Task>> {
    parent
        .snapshot_children()
        .into_iter()
        .filter(|c| c.is_user_task() && c.is_waitable_zombie())
        .collect()
}

// ── 内核线程 ──────────────────────────────────────────────────────────────────

/// 从 init 派生一个内核线程。线程入口签名：
/// `unsafe extern "C" fn(arg: usize) -> !`，内部以 [`kthread_finish`] 退出。
#[kernel_symbols::export(name = "sched.spawn.kthread_create", contract = "kernel.sched.kernel-thread@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED, retained_args = 1 << 0)]
pub fn kthread_create(entry: KernelEntry, arg: usize, params: SchedParams) -> Arc<Task> {
    let root_ns = root_pid_ns();
    let session = Session::new();
    let pgroup = ProcessGroup::new(&session);
    session.register_group(&pgroup);
    let tgroup = ThreadGroup::new();

    // 内核线程必须和 PID 1 的用户态进程彻底分离：不作为 init 的普通子线程，
    // 不共享 init 的 SharedSignal，也不进入 init 的进程组。否则 Ctrl-C /
    // exit_group / wait 这类 POSIX 路径会误伤 idle 和其它内核线程。
    let child = Task::new(
        params,
        alloc::sync::Weak::new(),
        Arc::clone(&tgroup),
        Arc::clone(&pgroup),
    );
    child.mark_kernel_thread();
    child.set_exit_signal(0);
    // 内核线程注册进根 pid 命名空间（kthread_create 开头已取 root_ns）。
    child.set_pid_ns(Arc::clone(&root_ns));

    let Ok(pid) = register_pid_chain(&child) else {
        log::warning!("[sched][kthread] pid allocation failed");
        child.set_state(TaskState::Dead);
        return child;
    };
    tgroup.set_leader(&child);
    tgroup.set_tgid(pid);
    child.set_tgid_cache(pid);
    tgroup.add_member(&child);
    pgroup.set_pgid(pid);
    pgroup.add_member(&child);
    session.set_leader(&child);
    session.set_sid(pid);

    child.into_kernel_thread(entry, arg);
    child
}

/// 从 init 派生并立即启动一个内核线程。线程入口签名：
/// `unsafe extern "C" fn(arg: usize) -> !`，内部以 [`kthread_finish`] 退出。
#[kernel_symbols::export(name = "sched.spawn.kthread_spawn", contract = "kernel.sched.kernel-thread@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED, retained_args = 1 << 0)]
pub fn kthread_spawn(entry: KernelEntry, arg: usize, params: SchedParams) -> Arc<Task> {
    let child = kthread_create(entry, arg, params);
    if let Err(err) = activate_task(&child) {
        log::warning!(
            "[sched][kthread] spawn activation failed pid={:?} err={:?}",
            child.pid_root(),
            err,
        );
        return child;
    }
    #[cfg(feature = "trace-task-lifecycle")]
    log::debug!(
        "[sched][kthread] spawned pid={:?} entry={:#x}",
        child.pid_root(),
        entry as *const () as usize,
    );
    child
}

/// 在指定活动 CPU 上创建并启动一个内核线程。
///
/// CPU 亲和性会在任务暴露给运行队列之前安装，因此不存在线程先在其它
/// CPU 上运行、随后再迁移的窗口。该接口不接受仅 online 但尚未 active 的 CPU。
#[kernel_symbols::export(name = "sched.spawn.kthread_spawn_on_cpu", contract = "kernel.sched.kernel-thread@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED, retained_args = 1 << 0)]
pub fn kthread_spawn_on_cpu(
    entry: KernelEntry,
    arg: usize,
    params: SchedParams,
    cpu_id: usize,
) -> Result<Arc<Task>, errno::Errno> {
    let child = kthread_create(entry, arg, params);
    if let Err(error) = activate_task_on_cpu(&child, cpu_id, now_ns_public()) {
        abort_new_task(&child);
        return Err(error);
    }
    Ok(child)
}

/// 内核线程的退出点：标记 Zombie、让出 CPU。控制流不返回。
pub fn kthread_finish(code: ExitCode) -> ! {
    let me = current_task();
    exit_task(&me, code);
    drop(me);
    schedule_once(0);
    panic!("[sched] kthread_finish: schedule_once returned unexpectedly");
}

/// 重新把任务 reparent 到 init。父先死时由更上层使用。
pub fn reparent_to_init(orphan: &Arc<Task>) {
    orphan.reparent_to(&init_task());
}
