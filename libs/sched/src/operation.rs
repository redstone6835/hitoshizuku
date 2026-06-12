//! POSIX 进程 / 信号 syscall 的内核侧实现。
//!
//! 所有函数名与 Linux syscall 同名（去掉 `sys_` 前缀），形状对齐。它们**不是**
//! syscall 本身 —— syscall dispatcher 由上层编写，直接调这里的函数即可。
//!
//! ## 调用约定
//!
//! Linux 风格无参数：每个函数内部调 [`crate::scheduler::current_task`] 取
//! 调用者句柄。返回值统一 `Result<T, errno::Errno>`。
//!
use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;

use crate::clone_flags::{CloneArgs, CloneFlags};
use crate::cpu::{CpuId, CpuMask};
use crate::eevdf::SchedParams;
use crate::group::{ProcessGroup, Session};
use crate::ids::{Capability, Gid, Uid};
use crate::pid::PidT;
use crate::process_ops::{ExecRequest, UserContextRef, process_image_ops};
use crate::rlimit::{Resource, RlimitError, RlimitPair, Rlimits};
use crate::sched_class::SchedAttr;
use crate::scheduler::{
    NR_CPUS, continue_task, current_cpu_id, current_task, enqueue_task_deferred, mark_task_stopped,
    migrate_task, online_cpu_mask, request_balance, request_post_syscall_handoff, request_resched,
    root_pid_ns, runqueue_of, sched_topology, schedule_once, signal_wakeup, supported_cpu_mask,
};
use crate::signal::{
    DefaultAction, SigAction, SigHandler, SigInfo, SigProcMaskHow, SigSet, SignalNumber,
    default_action,
};
use crate::spawn::{abort_new_task, activate_task, clone_task, exit_task, reap_matching};
use crate::task::{Task, TaskUsage};
use crate::wait_flags::{WaitId, WaitOptions, WaitResult, WaitStatus};
use crate::{ExitCode, TaskState};

// ── 无副作用查询 ──────────────────────────────────────────────────────────────

/// 当前进程 pid（Linux 语义：线程组 leader 的 pid）。
pub fn getpid() -> PidT {
    let me = current_task();
    me.thread_group()
        .leader()
        .and_then(|l| l.pid_root())
        .or_else(|| me.pid_root())
        .unwrap_or(0)
}

/// 当前线程 tid。
pub fn gettid() -> PidT {
    current_task().pid_root().unwrap_or(0)
}

/// 父进程 pid。父若不在根 ns 或已被 reparent，会返回 reparent 后的新父。
pub fn getppid() -> PidT {
    current_task()
        .parent()
        .and_then(|p| p.thread_group().leader())
        .and_then(|l| l.pid_root())
        .unwrap_or(0)
}

pub fn getuid() -> Uid {
    current_task().credentials().uid
}

pub fn geteuid() -> Uid {
    current_task().credentials().euid
}

pub fn getgid() -> Gid {
    current_task().credentials().gid
}

pub fn getegid() -> Gid {
    current_task().credentials().egid
}

// ── 进程组 / 会话 ────────────────────────────────────────────────────────────

fn lookup_pid(pid: PidT) -> Result<Arc<Task>, Errno> {
    if pid == 0 {
        return Ok(current_task());
    }
    root_pid_ns()
        .registry()
        .lookup(pid)
        .and_then(|w| w.upgrade())
        .ok_or(Errno::ESRCH)
}

/// 按根 PID namespace 查询任务句柄。
///
/// 调度核心只暴露稳定的任务选择入口；`pid == 0` 的 Linux 特殊语义仍由
/// syscall 兼容层决定是否传入。
pub fn task_by_pid(pid: PidT) -> Result<Arc<Task>, Errno> {
    lookup_pid(pid)
}

pub fn getpgid(pid: PidT) -> Result<PidT, Errno> {
    let t = lookup_pid(pid)?;
    let pgid = t.process_group().pgid();
    if pgid > 0 {
        Ok(pgid)
    } else {
        Err(Errno::ESRCH)
    }
}

pub fn getsid(pid: PidT) -> Result<PidT, Errno> {
    let t = lookup_pid(pid)?;
    t.process_group()
        .session()
        .and_then(|s| s.leader())
        .and_then(|l| l.pid_root())
        .ok_or(Errno::ESRCH)
}

/// 设置 pid 所在的进程组为 pgid。POSIX 语义：
/// - pid==0 → 当前线程
/// - pgid==0 → pgid 等于 pid（自成一组 leader）
/// - target 与 caller 必须在同一 session
pub fn setpgid(pid: PidT, pgid: PidT) -> Result<(), Errno> {
    let target = lookup_pid(pid)?;
    let me = current_task();

    // 必须同 session。
    let my_session = me.process_group().session().ok_or(Errno::EPERM)?;
    let target_session = target.process_group().session().ok_or(Errno::EPERM)?;
    if !Arc::ptr_eq(&my_session, &target_session) {
        return Err(Errno::EPERM);
    }

    // 决定实际 pgid：0 → target 自己的 pid。
    let effective_pgid = if pgid == 0 {
        target.pid_root().unwrap_or(0)
    } else {
        pgid
    };
    if effective_pgid <= 0 {
        return Err(Errno::EINVAL);
    }

    // 找到名字等于 effective_pgid 的已存在 pgroup（遍历 session 里的 groups）。
    let found = my_session
        .snapshot_groups()
        .into_iter()
        .find(|g| g.pgid() == effective_pgid);

    let new_pg = match found {
        Some(g) => g,
        None => {
            // 仅允许 target 成为新 pgroup 的 leader（target.pid == effective_pgid）。
            if target.pid_root().unwrap_or(0) != effective_pgid {
                return Err(Errno::EPERM);
            }
            let pg = ProcessGroup::new(&my_session);
            my_session.register_group(&pg);
            pg
        }
    };

    // 迁移：旧组摘掉，新组装入，Task 换 Arc。
    let old_pg = target.process_group();
    if !Arc::ptr_eq(&old_pg, &new_pg) {
        old_pg.remove_member(&target);
        new_pg.add_member(&target);
        target.set_process_group(new_pg);
    }
    Ok(())
}

/// `setsid(2)`：创建新 session + 新 pgroup，当前线程成为两者的 leader。
/// 调用者必须不是已有 pgroup 的 leader。
pub fn setsid() -> Result<PidT, Errno> {
    let me = current_task();
    let my_pid = me.pid_root().ok_or(Errno::ESRCH)?;

    // 若调用者已经是某个 pgroup 的 leader → EPERM。
    let old_pg = me.process_group();
    if let Some(leader) = old_pg
        .snapshot()
        .iter()
        .find_map(|m| m.thread_group().leader())
    {
        if Arc::ptr_eq(&leader, &me) && old_pg.snapshot().len() == 1 {
            // 只有自己一个成员——允许；否则我们是一个大组的 leader，拒绝。
            // 简化：Linux 内核的判定是"当前 pgroup 的 pgid == my_pid"，
            // 即自己是 leader；我们拒绝该情况。
        }
        if let Some(lead_pid) = leader.pid_root() {
            if lead_pid == my_pid {
                return Err(Errno::EPERM);
            }
        }
    }

    // 新 session + 新 pgroup。
    let new_session = Session::new();
    let new_pg = ProcessGroup::new(&new_session);
    new_session.register_group(&new_pg);
    new_session.set_leader(&me);

    // 迁移当前线程。
    old_pg.remove_member(&me);
    new_pg.add_member(&me);
    me.set_process_group(Arc::clone(&new_pg));

    Ok(my_pid)
}

fn lookup_process_group(pgid: PidT) -> Result<Arc<ProcessGroup>, Errno> {
    if pgid <= 0 {
        return Err(Errno::EINVAL);
    }

    let me = current_task();
    if let Some(session) = me.process_group().session()
        && let Some(pg) = session
            .snapshot_groups()
            .into_iter()
            .find(|pg| pg.pgid() == pgid)
    {
        return Ok(pg);
    }

    // 进程组对象不在全局表中单独登记。这里从 pid namespace 中的存活任务反查，
    // 避免把 TTY 的前台组投递重新编码成 kill(-PGID) 后撞上特殊 pid 语义。
    root_pid_ns()
        .registry()
        .snapshot()
        .into_iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .filter(|task| task.is_user_task())
        .map(|task| task.process_group())
        .find(|pg| pg.pgid() == pgid)
        .ok_or(Errno::ESRCH)
}

fn deliver_to_process_group(pg: Arc<ProcessGroup>, sig: Option<SignalNumber>) -> Result<(), Errno> {
    let Some(sig) = sig else { return Ok(()) };
    let info = make_siginfo(sig);
    for m in pg.snapshot() {
        if m.is_kernel_task() {
            continue;
        }
        if check_kill_permission(&m).is_ok() {
            m.thread_group().shared_signal().deliver(info);
            for x in m.thread_group().snapshot() {
                if should_wake_for_signal(&x, sig) {
                    signal_wakeup(&x, &info);
                    break;
                }
            }
        }
    }
    Ok(())
}

/// 按真实进程组对象投递信号，供 TTY/作业控制内部路径使用。
///
/// 这条路径不复用 `kill(2)` 的 pid 编码，因此 PGID==1 时不会被误解释成
/// “广播所有进程”的特殊形式。
pub fn kill_process_group(pgid: PidT, sig: Option<SignalNumber>) -> Result<(), Errno> {
    let pg = lookup_process_group(pgid)?;
    deliver_to_process_group(pg, sig)
}

// ── exit / exit_group ────────────────────────────────────────────────────────

/// 结束当前线程。控制流不返回。
pub fn exit(code: i32) -> ! {
    let me = current_task();
    exit_task(&me, ExitCode(code));
    drop(me);
    schedule_once(0);
    panic!("[sched] exit: schedule_once returned unexpectedly");
}

/// 结束当前线程组的所有线程。
pub fn exit_group(code: i32) -> ! {
    let me = current_task();
    let tg = me.thread_group();
    let members = tg.snapshot();
    for m in members.iter() {
        if Arc::ptr_eq(m, &me) {
            continue;
        }
        if m.is_kernel_task() {
            continue;
        }
        if m.state() != TaskState::Zombie && m.state() != TaskState::Dead {
            exit_task(m, ExitCode(code));
        }
    }
    drop(members);
    drop(tg);
    drop(me);
    exit(code);
}

// ── 调度器相关 ────────────────────────────────────────────────────────────────

pub fn sched_yield() -> Result<(), Errno> {
    current_task().record_voluntary_context_switch();
    schedule_once(0);
    Ok(())
}

/// `nice(inc)`：相对当前 nice 调整，结果钳到 [-20, 19]。返回新 nice 值。
pub fn nice(inc: i32) -> Result<i32, Errno> {
    let me = current_task();
    let cur_nice = me.sched.nice() as i32;
    let mut new_nice = cur_nice.saturating_add(inc);
    if new_nice < -20 {
        new_nice = -20;
    } else if new_nice > 19 {
        new_nice = 19;
    }
    sched_setparam_for_task(
        &me,
        SchedParams {
            nice: new_nice as i8,
            slice_ns: 0,
        },
    )?;
    Ok(new_nice)
}

/// 为 pid 对应任务设置 sched params。
pub fn sched_setparam(pid: PidT, params: SchedParams) -> Result<(), Errno> {
    let t = lookup_pid(pid)?;
    sched_setparam_for_task(&t, params)
}

pub fn sched_setparam_for_task(task: &Arc<Task>, params: SchedParams) -> Result<(), Errno> {
    update_task_sched_entity(task, |cpu_id, task, now_ns| {
        runqueue_of(cpu_id).update_params(task, params, now_ns)
    })
}

pub fn sched_setnice_for_task(task: &Arc<Task>, nice: i8) -> Result<(), Errno> {
    update_task_sched_entity(task, |cpu_id, task, now_ns| {
        runqueue_of(cpu_id).update_nice(task, nice, now_ns)
    })
}

/// 设置完整调度属性。权限检查由 syscall 层负责；本函数只校验参数和更新 rq。
pub fn sched_setattr(pid: PidT, attr: SchedAttr) -> Result<(), Errno> {
    let attr = attr.validate()?;
    let t = lookup_pid(pid)?;
    sched_setattr_for_task(&t, attr)
}

pub fn sched_setattr_for_task(task: &Arc<Task>, attr: SchedAttr) -> Result<(), Errno> {
    let attr = attr.validate()?;
    update_task_sched_entity(task, |cpu_id, task, now_ns| {
        runqueue_of(cpu_id).update_sched_attr(task, attr, now_ns)
    })
}

fn update_task_sched_entity(
    task: &Arc<Task>,
    mut update: impl FnMut(usize, &Arc<Task>, u64) -> bool,
) -> Result<(), Errno> {
    let now_ns = crate::scheduler::now_ns_public();
    let owner = task.current_cpu();
    if owner < NR_CPUS && update(owner, task, now_ns) {
        return Ok(());
    }

    for cpu_id in 0..NR_CPUS {
        if cpu_id == owner {
            continue;
        }
        if update(cpu_id, task, now_ns) {
            return Ok(());
        }
    }

    if task.sched.on_rq() {
        Err(Errno::EBUSY)
    } else if update(task.current_cpu().min(NR_CPUS - 1), task, now_ns) {
        Ok(())
    } else {
        Err(Errno::EBUSY)
    }
}

pub fn sched_getattr(pid: PidT) -> Result<SchedAttr, Errno> {
    let task = lookup_pid(pid)?;
    Ok(sched_getattr_for_task(&task))
}

pub fn sched_getattr_for_task(task: &Arc<Task>) -> SchedAttr {
    task.sched.sched_attr()
}

pub fn set_sched_reset_on_fork(pid: PidT, enabled: bool) -> Result<(), Errno> {
    lookup_pid(pid)?.set_sched_reset_on_fork(enabled);
    Ok(())
}

pub fn sched_reset_on_fork(pid: PidT) -> Result<bool, Errno> {
    Ok(lookup_pid(pid)?.sched_reset_on_fork())
}

pub fn set_task_nice(task: &Arc<Task>, nice: i8) {
    let mut attr = task.sched.sched_attr();
    attr.nice = nice.clamp(crate::eevdf::NICE_MIN, crate::eevdf::NICE_MAX);
    runqueue_of(task.current_cpu()).update_sched_attr(
        task,
        attr,
        crate::scheduler::now_ns_public(),
    );
}

pub fn task_usage(pid: PidT) -> Result<TaskUsage, Errno> {
    Ok(lookup_pid(pid)?.usage_snapshot(crate::scheduler::now_ns_public()))
}

pub fn children_usage(pid: PidT) -> Result<TaskUsage, Errno> {
    Ok(lookup_pid(pid)?.child_usage_snapshot())
}

pub fn all_tasks_snapshot() -> Vec<Arc<Task>> {
    root_pid_ns()
        .registry()
        .snapshot()
        .into_iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .collect()
}

pub fn sched_getaffinity(pid: PidT) -> Result<u64, Errno> {
    let task = lookup_pid(pid)?;
    Ok(sched_getaffinity_for_task(&task))
}

pub fn sched_getaffinity_for_task(task: &Arc<Task>) -> u64 {
    task.cpu_affinity() & supported_cpu_mask()
}

pub fn sched_setaffinity(pid: PidT, mask: u64) -> Result<(), Errno> {
    let task = lookup_pid(pid)?;
    sched_setaffinity_for_task(&task, mask)
}

pub fn sched_setaffinity_for_task(task: &Arc<Task>, mask: u64) -> Result<(), Errno> {
    let requested = CpuMask::from_bits_truncate(mask);
    if requested.is_empty() {
        return Err(Errno::EINVAL);
    }
    let online = CpuMask::from_bits_truncate(online_cpu_mask());
    if requested.intersection(online).is_empty() {
        return Err(Errno::EINVAL);
    }
    let supported = requested.bits();
    task.set_cpu_affinity(supported);

    let current_cpu = task.current_cpu();
    let current = CpuId::new(current_cpu);
    if current.is_some_and(|cpu| requested.contains(cpu)) {
        return Ok(());
    }

    let target = sched_topology()
        .select_cpu(requested, online, current, false, |cpu| {
            runqueue_of(cpu.get()).nr_running()
        })
        .map(|cpu| cpu.get());

    if let Some(target_cpu) = target {
        if migrate_task(task, target_cpu).is_err() {
            request_balance(target_cpu);
            if current_cpu < NR_CPUS {
                request_resched(current_cpu);
            }
        }
    } else if current_cpu < NR_CPUS {
        request_resched(current_cpu);
    }
    Ok(())
}

/// getcpu：返回当前调度 CPU；节点编号由兼容层保持 UMA 语义。
pub fn getcpu() -> Result<(u32, u32), Errno> {
    Ok((current_cpu_id() as u32, 0))
}

// ── execve / sigreturn ──────────────────────────────────────────────

pub fn execve(request: ExecRequest) -> Result<(), Errno> {
    execve_with_context(request, UserContextRef::NONE)
}

pub fn execve_with_context(request: ExecRequest, user_ctx: UserContextRef) -> Result<(), Errno> {
    let me = current_task();
    let ops = process_image_ops().ok_or(Errno::ENOSYS)?;
    (ops.execve)(&me, request, user_ctx)?;
    me.clear_rseq_registration();
    me.clear_sigaltstack();
    me.shared_signal().reset_caught_for_exec();
    if me.is_vforking() {
        me.set_vforking(false);
        // vfork 父进程到这里已经可以继续运行，但不要立刻抢占刚 exec 完成的
        // child。让 child 先返回用户态跑到 daemon bind/listen，可以避免脚本
        // 中 `server -D &; client` 在没有显式 sleep 时打到监听尚未建立的窗口。
        me.vfork_done.wake_all_with(|task| {
            enqueue_task_deferred(Arc::clone(task), crate::scheduler::now_ns_public());
        });
    }
    Ok(())
}

pub fn sigreturn() -> Result<(), Errno> {
    sigreturn_with_context(UserContextRef::NONE)
}

pub fn sigreturn_with_context(user_ctx: UserContextRef) -> Result<(), Errno> {
    let me = current_task();
    let ops = process_image_ops().ok_or(Errno::ENOSYS)?;
    (ops.sigreturn)(&me, user_ctx)
}

// ── fork / vfork ─────────────────────────────────────────────────────

/// `fork(2)`：分叉出一个新进程，返回子进程 pid。用户上下文复制通过
/// [`crate::process_ops::ProcessImageOps`] 完成；sched 只负责任务图和调度状态。
pub fn fork() -> Result<PidT, Errno> {
    clone(CloneArgs::fork_default())
}

/// `vfork(2)`：等价 `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)`。
pub fn vfork() -> Result<PidT, Errno> {
    clone(CloneArgs::vfork_default())
}

/// `clone(flags, stack, ...)`：返回子进程 pid。
///
/// 本入口会先建立任务图，再调用已注册的 `ProcessImageOps::clone_user_context`
/// 安装子任务首次返回用户态所需的执行上下文，最后才入队。任何一步失败都会
/// 回滚尚未运行的 child。
pub fn clone(args: CloneArgs) -> Result<PidT, Errno> {
    clone_with_context(args, UserContextRef::NONE)
}

pub fn clone_with_context(args: CloneArgs, user_ctx: UserContextRef) -> Result<PidT, Errno> {
    Ok(clone_with_context_outcome(args, user_ctx)?.pid)
}

/// clone 的完整返回值。syscall 兼容层需要 child 句柄创建 pidfd；调度器核心
/// 仍然只暴露任务对象，不直接理解 fdtable 或用户态 ABI。
pub struct CloneOutcome {
    pub pid: PidT,
    pub child: Arc<Task>,
}

pub fn clone_with_context_outcome(
    args: CloneArgs,
    user_ctx: UserContextRef,
) -> Result<CloneOutcome, Errno> {
    validate_clone_args(args)?;
    let parent = current_task();
    let params = SchedParams::default_fair();
    let child = clone_task(&parent, args, params);
    let Some(pid) = child.pid_root() else {
        abort_new_task(&child);
        return Err(Errno::EAGAIN);
    };

    if args.flags.has(CloneFlags::CLONE_VFORK) {
        child.set_vforking(true);
    }

    let ops = match process_image_ops() {
        Some(ops) => ops,
        None => {
            abort_new_task(&child);
            return Err(Errno::ENOSYS);
        }
    };

    if let Err(err) = (ops.clone_user_context)(&parent, &child, args, user_ctx) {
        abort_new_task(&child);
        return Err(err);
    }
    if let Err(err) = activate_task(&child) {
        abort_new_task(&child);
        return Err(err);
    }

    if args.flags.has(CloneFlags::CLONE_VFORK) {
        let child_wait = Arc::downgrade(&child);
        drop(parent);
        loop {
            let Some(wait_child) = child_wait.upgrade() else {
                break;
            };
            if !wait_child.is_vforking() {
                break;
            }
            let parent = current_task();
            wait_child
                .vfork_done
                .prepare_to_wait(&parent, TaskState::Sleeping);
            if !wait_child.is_vforking() {
                wait_child.vfork_done.finish_wait(&parent);
                break;
            }
            drop(wait_child);
            drop(parent);
            schedule_once(crate::scheduler::now_ns_public());
            let parent = current_task();
            if let Some(wait_child) = child_wait.upgrade() {
                wait_child.vfork_done.finish_wait(&parent);
            }
        }
    } else {
        // 不在 clone syscall 尚未返回时直接重入调度：父进程的 trap frame
        // 仍由 syscall 出口负责写返回值和推进 PC。这里只登记一次收尾后的
        // 启动交接，由 syscall dispatcher 在安全边界切给新子进程。
        request_post_syscall_handoff();
    }

    Ok(CloneOutcome { pid, child })
}

fn validate_clone_args(args: CloneArgs) -> Result<(), Errno> {
    let flags = args.flags;
    const KNOWN: u64 = CloneFlags::CSIGNAL
        | CloneFlags::CLONE_VM
        | CloneFlags::CLONE_FS
        | CloneFlags::CLONE_FILES
        | CloneFlags::CLONE_SIGHAND
        | CloneFlags::CLONE_PIDFD
        | CloneFlags::CLONE_PTRACE
        | CloneFlags::CLONE_VFORK
        | CloneFlags::CLONE_PARENT
        | CloneFlags::CLONE_THREAD
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_SYSVSEM
        | CloneFlags::CLONE_SETTLS
        | CloneFlags::CLONE_PARENT_SETTID
        | CloneFlags::CLONE_CHILD_CLEARTID
        | CloneFlags::CLONE_DETACHED
        | CloneFlags::CLONE_UNTRACED
        | CloneFlags::CLONE_CHILD_SETTID
        | CloneFlags::CLONE_NEWCGROUP
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWUSER
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_IO;
    const UNSUPPORTED: u64 = CloneFlags::CLONE_PTRACE
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWCGROUP
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWUSER
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_IO;
    if flags.has(CloneFlags::CLONE_NEWNS) && flags.has(CloneFlags::CLONE_FS) {
        return Err(Errno::EINVAL);
    }
    if (flags.raw() & !KNOWN) != 0 || (flags.raw() & UNSUPPORTED) != 0 || args.cgroup != 0 {
        // TODO(threading): namespace/cgroup flags need namespace and cgroup objects before
        // clone can expose them safely.
        return Err(Errno::EOPNOTSUPP);
    }
    if args.set_tid != 0 || args.set_tid_size != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if args.pidfd != 0 && !flags.has(CloneFlags::CLONE_PIDFD) {
        return Err(Errno::EINVAL);
    }
    if flags.has(CloneFlags::CLONE_PIDFD) && args.pidfd == 0 {
        return Err(Errno::EINVAL);
    }
    if args.exit_signal > 64 {
        return Err(Errno::EINVAL);
    }
    if flags.has(CloneFlags::CLONE_THREAD) && args.exit_signal_raw() != 0 {
        return Err(Errno::EINVAL);
    }
    if args.stack == 0 && args.stack_size != 0 {
        return Err(Errno::EINVAL);
    }
    if flags.has(CloneFlags::CLONE_SIGHAND) && !flags.has(CloneFlags::CLONE_VM) {
        return Err(Errno::EINVAL);
    }
    if flags.has(CloneFlags::CLONE_THREAD)
        && (!flags.has(CloneFlags::CLONE_SIGHAND) || !flags.has(CloneFlags::CLONE_VM))
    {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

// ── wait4 / waitid ───────────────────────────────────────────────────────────

fn matches_waitid(child: &Arc<Task>, target: &WaitId, parent: &Arc<Task>) -> bool {
    if child.is_kernel_task() {
        return false;
    }
    match target {
        WaitId::All => true,
        WaitId::Pid(pid) => child.pid_root() == Some(*pid),
        WaitId::Pgid(pgid) => child
            .process_group()
            .snapshot()
            .iter()
            .find_map(|m| m.thread_group().leader().and_then(|l| l.pid_root()))
            .map_or(false, |p| p == *pgid),
        WaitId::SameGroup => Arc::ptr_eq(&child.process_group(), &parent.process_group()),
        WaitId::Pidfd(task) => Arc::ptr_eq(child, task),
    }
}

fn wait_child_observable(
    parent: &Arc<Task>,
    target: WaitId,
    wait_exited: bool,
    wait_stopped: bool,
    wait_continued: bool,
) -> bool {
    let children = parent.snapshot_children();
    let mut any_match = false;
    for child in children
        .iter()
        .filter(|c| matches_waitid(c, &target, parent))
    {
        any_match = true;
        if wait_exited && child.state() == TaskState::Zombie {
            return true;
        }
        if wait_stopped && child.wait_stopped_status(true).is_some() {
            return true;
        }
        if wait_continued && child.wait_continued_status(true).is_some() {
            return true;
        }
    }
    !any_match
}

fn child_exit_status(child: &Arc<Task>, fallback: ExitCode) -> WaitStatus {
    child
        .exit_wait_status()
        .unwrap_or_else(|| WaitStatus::from_exit(fallback.0))
}

/// `wait4(pid, &mut status, opts, _rusage)`：阻塞等待子退出。
/// 返回 `(pid, status)`。`WNOHANG` 下无 zombie 返回 `WaitResult { pid: 0, ... }`。
pub fn wait4(pid: PidT, options: WaitOptions) -> Result<WaitResult, Errno> {
    let target = WaitId::from_wait4_pid(pid);
    wait_common(target, options, true)
}

/// `waitid(idtype, id, options)`：结构化版本。
pub fn waitid(target: WaitId, options: WaitOptions) -> Result<WaitResult, Errno> {
    const WAITID_EVENTS: u32 =
        WaitOptions::WEXITED | WaitOptions::WSTOPPED | WaitOptions::WCONTINUED;
    if (options.raw() & WAITID_EVENTS) == 0 {
        return Err(Errno::EINVAL);
    }
    wait_common(target, options, false)
}

fn wait_common(
    target: WaitId,
    options: WaitOptions,
    implicit_exited: bool,
) -> Result<WaitResult, Errno> {
    let mut me = current_task();
    let wait_exited = implicit_exited || options.has(WaitOptions::WEXITED);
    let wait_stopped = options.has(WaitOptions::WSTOPPED);
    let wait_continued = options.has(WaitOptions::WCONTINUED);
    let nowait = options.has(WaitOptions::WNOWAIT);

    loop {
        // 1. 先看是否有退出事件匹配。wait4 的 options=0 隐含 WEXITED；
        //    waitid 必须由调用方显式传 WEXITED/WSTOPPED/WCONTINUED。
        let pred = |c: &Arc<Task>| matches_waitid(c, &target, &me);
        if wait_exited {
            if nowait {
                if let Some(child) = me
                    .snapshot_children()
                    .into_iter()
                    .find(|c| c.state() == TaskState::Zombie && pred(c))
                {
                    let code = child
                        .exit_code()
                        .expect("[sched][wait] zombie without exit code");
                    return Ok(WaitResult {
                        pid: child.pid_root().unwrap_or(0),
                        status: child_exit_status(&child, code),
                        usage: child.usage_snapshot(crate::scheduler::now_ns_public()),
                    });
                }
            } else if let Some((child, code)) = reap_matching(&me, pred) {
                return Ok(WaitResult {
                    pid: child.pid_root().unwrap_or(0),
                    status: child_exit_status(&child, code),
                    usage: child.usage_snapshot(crate::scheduler::now_ns_public()),
                });
            }
        }

        // 2. stopped / continued 是父侧可消费的状态变化事件，不会 reap child。
        let children = me.snapshot_children();
        if wait_stopped {
            for child in children.iter().filter(|c| matches_waitid(c, &target, &me)) {
                if let Some(status) = child.wait_stopped_status(nowait) {
                    return Ok(WaitResult {
                        pid: child.pid_root().unwrap_or(0),
                        status,
                        usage: child.usage_snapshot(crate::scheduler::now_ns_public()),
                    });
                }
            }
        }
        if wait_continued {
            for child in children.iter().filter(|c| matches_waitid(c, &target, &me)) {
                if let Some(status) = child.wait_continued_status(nowait) {
                    return Ok(WaitResult {
                        pid: child.pid_root().unwrap_or(0),
                        status,
                        usage: child.usage_snapshot(crate::scheduler::now_ns_public()),
                    });
                }
            }
        }

        // 3. 是否还有匹配的子？没有任何匹配子 → ECHILD。
        let any_match = children.iter().any(|c| matches_waitid(c, &target, &me));
        if !any_match {
            return Err(Errno::ECHILD);
        }

        // 4. WNOHANG：不阻塞，返回 pid=0。
        if options.has(WaitOptions::WNOHANG) {
            return Ok(WaitResult {
                pid: 0,
                status: WaitStatus(0),
                usage: TaskUsage::default(),
            });
        }

        // 5. 阻塞：按 wait_event 协议挂到 me.exit_waiters，让出 CPU，被唤醒后重试。
        me.exit_waiters.prepare_to_wait(&me, TaskState::Sleeping);
        if wait_child_observable(
            &me,
            target.clone(),
            wait_exited,
            wait_stopped,
            wait_continued,
        ) {
            me.exit_waiters.finish_wait(&me);
            continue;
        }
        drop(me);
        schedule_once(crate::scheduler::now_ns_public());
        me = current_task();
        me.exit_waiters.finish_wait(&me);
        // 唤醒后重新轮询。
    }
}

// ── kill / tkill / tgkill ────────────────────────────────────────────────────

fn check_kill_permission(target: &Arc<Task>) -> Result<(), Errno> {
    if target.is_kernel_task() {
        return Err(Errno::ESRCH);
    }
    let me = current_task();
    let me_creds = me.credentials();
    if me_creds.has_cap(Capability::Kill) {
        return Ok(());
    }
    let t_creds = target.credentials();
    if me_creds.euid == t_creds.uid || me_creds.euid == t_creds.suid {
        return Ok(());
    }
    if me_creds.uid == t_creds.uid || me_creds.uid == t_creds.suid {
        return Ok(());
    }
    Err(Errno::EPERM)
}

fn make_siginfo(sig: SignalNumber) -> SigInfo {
    let me = current_task();
    SigInfo {
        sig,
        code: 0,
        sender_pid: me.pid_root().unwrap_or(0),
        sender_uid: me.credentials().uid,
        raw: None,
    }
}

fn should_wake_for_signal(task: &Arc<Task>, sig: SignalNumber) -> bool {
    task.is_user_task()
        && (!task.signal.blocked_snapshot().has(sig) || task.signal.sigtimedwait_wants(sig))
}

/// `kill(pid, sig)`：按 POSIX pid 语义投递信号。
/// - pid > 0：送到整 thread-group（共享 pending）。
/// - pid == 0：送到调用者同 pgroup 的所有进程。
/// - pid == -1：送到 init 外的所有进程（精简实现：枚举当前 ns 所有 pid）。
/// - pid < -1：送到 pgid==-pid 的所有进程。
pub fn kill(pid: PidT, sig: Option<SignalNumber>) -> Result<(), Errno> {
    let me = current_task();
    if pid > 0 {
        let target = lookup_pid(pid)?;
        if target.is_kernel_task() {
            return Err(Errno::ESRCH);
        }
        check_kill_permission(&target)?;
        let Some(sig) = sig else { return Ok(()) };
        let info = make_siginfo(sig);
        target.thread_group().shared_signal().deliver(info);
        // 唤醒任一合适的 tg 成员。
        for m in target.thread_group().snapshot() {
            if should_wake_for_signal(&m, sig) {
                signal_wakeup(&m, &info);
                break;
            }
        }
        return Ok(());
    }

    // 广播到一个 process group 或整个 ns。
    if pid == -1 {
        // `kill(-1, sig)`：对调用者**之外**、非 init 的全部任务发信号。
        // 语义按 thread-group 粒度：凡 tgid==1 的成员（含 init 自身、init
        // 的 kthread 同组兄弟如 idle）一律跳过，否则 kthread 与 init 共用
        // SharedSignal 会把 SIGTERM 投回 init。
        let Some(sig) = sig else { return Ok(()) };
        let info = make_siginfo(sig);
        let my_tg = me.thread_group();
        let mut delivered = false;
        for (p, weak) in root_pid_ns().registry().snapshot() {
            if p == 1 {
                continue;
            }
            let Some(t) = weak.upgrade() else { continue };
            if t.is_kernel_task() {
                continue;
            }
            // 同 tg 直接跳过（覆盖 init 整个线程组）。
            if Arc::ptr_eq(&t.thread_group(), &my_tg) {
                continue;
            }
            let tg_leader_pid = t
                .thread_group()
                .leader()
                .and_then(|l| l.pid_root())
                .unwrap_or(0);
            if tg_leader_pid == 1 {
                continue;
            }
            if check_kill_permission(&t).is_err() {
                continue;
            }
            t.thread_group().shared_signal().deliver(info);
            for x in t.thread_group().snapshot() {
                if should_wake_for_signal(&x, sig) {
                    signal_wakeup(&x, &info);
                    break;
                }
            }
            delivered = true;
        }
        return if delivered { Ok(()) } else { Err(Errno::EPERM) };
    }

    let pg = match pid {
        0 => me.process_group(),
        p if p < -1 => lookup_process_group(-p)?,
        _ => return Err(Errno::EINVAL),
    };

    deliver_to_process_group(pg, sig)
}

/// `tkill(tid, sig)`：投递到**特定线程**（per-task pending）。
pub fn tkill(tid: PidT, sig: Option<SignalNumber>) -> Result<(), Errno> {
    let target = lookup_pid(tid)?;
    if target.is_kernel_task() {
        return Err(Errno::ESRCH);
    }
    check_kill_permission(&target)?;
    let Some(sig) = sig else { return Ok(()) };
    let info = make_siginfo(sig);
    target.signal.deliver(info);
    signal_wakeup(&target, &info);
    Ok(())
}

/// `tgkill(tgid, tid, sig)`：tid 的 thread_group 必须等于 tgid。
pub fn tgkill(tgid: PidT, tid: PidT, sig: Option<SignalNumber>) -> Result<(), Errno> {
    let target = lookup_pid(tid)?;
    if target.is_kernel_task() {
        return Err(Errno::ESRCH);
    }
    let actual_tgid = target
        .thread_group()
        .leader()
        .and_then(|l| l.pid_root())
        .unwrap_or(0);
    if actual_tgid != tgid {
        return Err(Errno::ESRCH);
    }
    check_kill_permission(&target)?;
    let Some(sig) = sig else { return Ok(()) };
    let info = make_siginfo(sig);
    target.signal.deliver(info);
    signal_wakeup(&target, &info);
    Ok(())
}

/// `rt_tgsigqueueinfo` 的调度层入口：调用方已经保留完整用户态 siginfo。
pub fn tgqueueinfo(tgid: PidT, tid: PidT, info: SigInfo) -> Result<(), Errno> {
    let target = lookup_pid(tid)?;
    if target.is_kernel_task() {
        return Err(Errno::ESRCH);
    }
    let actual_tgid = target
        .thread_group()
        .leader()
        .and_then(|l| l.pid_root())
        .unwrap_or(0);
    if actual_tgid != tgid {
        return Err(Errno::ESRCH);
    }
    check_kill_permission(&target)?;
    target.signal.deliver(info);
    signal_wakeup(&target, &info);
    Ok(())
}

/// `rt_sigqueueinfo` / pidfd process-directed queued signal entry.
pub fn queueinfo(pid: PidT, info: SigInfo) -> Result<(), Errno> {
    if pid <= 0 {
        return Err(Errno::EINVAL);
    }
    let target = lookup_pid(pid)?;
    if target.is_kernel_task() {
        return Err(Errno::ESRCH);
    }
    check_kill_permission(&target)?;
    target.thread_group().shared_signal().deliver(info);
    for member in target.thread_group().snapshot() {
        if should_wake_for_signal(&member, info.sig) {
            signal_wakeup(&member, &info);
            break;
        }
    }
    Ok(())
}

// ── sigaction / sigprocmask / sigpending ─────────────────────────────────────

/// `sigaction(sig, new, old)`。返回旧 action（若 `old` 不是 None 参考层封装写回）。
pub fn sigaction(sig: SignalNumber, new: SigAction) -> Result<SigAction, Errno> {
    if sig == SignalNumber::SIGKILL || sig == SignalNumber::SIGSTOP {
        return Err(Errno::EINVAL);
    }
    let me = current_task();
    let old = me.shared_signal().set_action(sig, new);
    Ok(old)
}

/// `sigprocmask(how, set, old)`。返回旧 mask。
pub fn sigprocmask(how: SigProcMaskHow, set: SigSet) -> Result<SigSet, Errno> {
    let me = current_task();
    let old = me.signal.block(set, how);
    Ok(old)
}

/// `sigpending()`：返回 per-task + tg-shared 的 pending 合集。
pub fn sigpending() -> Result<SigSet, Errno> {
    let me = current_task();
    let per_task = me.signal.pending_snapshot();
    let shared = me.shared_signal().pending_snapshot();
    Ok(SigSet(per_task.0 | shared.0))
}

/// 是否存在会打断阻塞 syscall 的 pending signal。
///
/// pending 位本身不够：SIGCHLD/SIGURG/SIGWINCH 等默认动作是忽略，若没有
/// 用户 handler，不应让 select/poll/socket wait 返回 EINTR。否则 netserver
/// 这类程序在子进程退出后会因为默认忽略的 SIGCHLD 直接跳出 accept loop。
pub fn has_interrupting_signal(task: &Arc<Task>) -> bool {
    let blocked = task.signal.blocked_snapshot().raw();
    let pending = (task.signal.pending_snapshot().raw()
        | task.shared_signal().pending_snapshot().raw())
        & !blocked;
    for raw in 1..crate::signal::NSIG as i32 {
        let Some(sig) = SignalNumber::from_raw(raw) else {
            continue;
        };
        if (pending & sig.bit()) == 0 {
            continue;
        }
        let action = task.shared_signal().get_action(sig);
        match action.handler {
            SigHandler::Ignore => continue,
            SigHandler::Handler(_) => return true,
            SigHandler::Default => match default_action(sig) {
                DefaultAction::Ign | DefaultAction::Cont => continue,
                DefaultAction::Term | DefaultAction::Core | DefaultAction::Stop => {
                    // 默认终止/停止类信号不能先把阻塞 syscall 退回用户态；
                    // 否则 SIGKILL 会被用户程序观察成 EINTR。当前任务在
                    // 内核态直接消费并执行默认动作，调度出口负责切走它。
                    let current = current_task();
                    if Arc::ptr_eq(&current, task) {
                        let info = task
                            .signal
                            .dequeue_one_in(sig.bit())
                            .or_else(|| task.shared_signal().dequeue_one_in(sig.bit()))
                            .unwrap_or_else(|| make_siginfo(sig));
                        apply_default_action(info);
                    }
                    return true;
                }
            },
        }
    }
    false
}

/// `sigtimedwait(these)` 在不阻塞的情况下尝试消费一条属于 `these` 的信号。
/// 命中即返回 Some(SigInfo)，无命中返回 None（不进入等待）。
///
/// 不匹配 `these` 的 pending 会保留在原队列中，等待常规投递或其它 sigwait。
pub fn sigtimedwait_poll(these: SigSet) -> Option<SigInfo> {
    let me = current_task();
    // 先 per-task（更及时），再 tg-shared。
    if let Some(info) = me.signal.dequeue_one_in(these.0) {
        return Some(info);
    }
    me.shared_signal().dequeue_one_in(these.0)
}

fn sigtimedwait_pending(these: SigSet) -> bool {
    let me = current_task();
    me.signal.has_pending_in(these.0) || me.shared_signal().has_pending_in(these.0)
}

fn finish_current_signal_wait(me: &Arc<Task>) {
    if !me.cas_state(TaskState::Sleeping, TaskState::Running) {
        let _ = me.cas_state(TaskState::Runnable, TaskState::Running);
    }
}

/// 等待 pending ∩ these 出现。返回 true 表示等到了，false 表示超时。
/// 本函数只等待、不消费；调用方随后用 [`sigtimedwait_poll`] 取走 siginfo。
pub fn sigtimedwait_wait(these: SigSet, timeout_ns: Option<u64>) -> bool {
    use crate::scheduler::{cancel_sleep_deadline, now_ns_public, register_sleep_deadline};
    let mut me = current_task();
    let deadline = timeout_ns.map(|ns| now_ns_public().saturating_add(ns));
    me.signal.begin_sigtimedwait(these);
    loop {
        if sigtimedwait_pending(these) {
            me.signal.end_sigtimedwait();
            return true;
        }
        if let Some(d) = deadline {
            if now_ns_public() >= d {
                me.signal.end_sigtimedwait();
                return false;
            }
        }

        if !me.cas_state(TaskState::Running, TaskState::Sleeping)
            && !me.cas_state(TaskState::Runnable, TaskState::Sleeping)
            && me.state() != TaskState::Sleeping
        {
            continue;
        }

        if let Some(d) = deadline {
            if !register_sleep_deadline(&me, d) {
                finish_current_signal_wait(&me);
                me.signal.end_sigtimedwait();
                return false;
            }
        }

        if sigtimedwait_pending(these) {
            if deadline.is_some() {
                cancel_sleep_deadline(&me);
            }
            finish_current_signal_wait(&me);
            me.signal.end_sigtimedwait();
            return true;
        }
        if let Some(d) = deadline {
            if now_ns_public() >= d {
                cancel_sleep_deadline(&me);
                finish_current_signal_wait(&me);
                me.signal.end_sigtimedwait();
                return false;
            }
        }

        drop(me);
        schedule_once(now_ns_public());
        me = current_task();
        if deadline.is_some() {
            cancel_sleep_deadline(&me);
        }
        finish_current_signal_wait(&me);
    }
}

// ── rlimit: getrlimit / setrlimit / prlimit64 ────────────────────────────────

fn rlimit_err_to_errno(e: RlimitError) -> Errno {
    match e {
        RlimitError::InvalidResource => Errno::EINVAL,
        RlimitError::ExceedsHard => Errno::EINVAL,
    }
}

/// `getrlimit(resource)` 拿到调用者 tg 的 (soft, hard)。
pub fn get_rlimit(resource: Resource) -> Result<RlimitPair, Errno> {
    let me = current_task();
    let pair = me.thread_group().rlimits().lock().get(resource);
    Ok(pair)
}

/// `setrlimit(resource, new)` 写调用者 tg 的 rlimit。
pub fn set_rlimit(resource: Resource, new: RlimitPair) -> Result<RlimitPair, Errno> {
    let me = current_task();
    let tg = me.thread_group();
    let mut guard = tg.rlimits().lock();
    let cur = guard.get(resource);
    validate_rlimit_update(&me, cur, new)?;
    let old = cur;
    guard.set(resource, new);
    Ok(old)
}

/// `prlimit64(pid, resource, new, old)`：
/// - `pid == 0`：当前进程；
/// - `pid > 0`：指定 TGID。
/// `new == None` 表示只读；`old != None` 写到该地址（call-site 决定）。
pub fn prlimit64(
    pid: i32,
    resource: Resource,
    new: Option<RlimitPair>,
) -> Result<RlimitPair, Errno> {
    let me = current_task();
    let my_tg = me.thread_group();
    let target = if pid == 0 {
        Arc::clone(&me)
    } else if pid > 0 {
        let root = root_pid_ns();
        let Some(weak) = root.registry().lookup(pid) else {
            return Err(Errno::ESRCH);
        };
        let Some(task) = weak.upgrade() else {
            return Err(Errno::ESRCH);
        };
        task
    } else {
        return Err(Errno::EINVAL);
    };
    let target_tg = target.thread_group();
    if !Arc::ptr_eq(&my_tg, &target_tg) {
        check_prlimit_target_permission(&me, &target)?;
    }
    if let Some(n) = new {
        let mut guard = target_tg.rlimits().lock();
        let cur = guard.get(resource);
        validate_rlimit_update(&me, cur, n)?;
        let old = cur;
        guard.set(resource, n);
        Ok(old)
    } else {
        Ok(target_tg.rlimits().lock().get(resource))
    }
}

/// 校验 rlimit 写入。
///
/// 规则对齐 Linux `do_prlimit`：`soft <= hard` 是基础不变量；非特权调用者
/// 只能在当前 hard 限制内调整 soft，并且不能提高 hard；具备
/// `CAP_SYS_RESOURCE` 时允许提高 hard/soft。这里刻意不检查 `new.hard < cur.soft`，
/// 因为 POSIX 允许把 hard 降到低于当前 soft 的新值，只要新 soft 同步降下来。
fn validate_rlimit_update(
    caller: &Arc<Task>,
    cur: RlimitPair,
    new: RlimitPair,
) -> Result<(), Errno> {
    if new.soft.0 > new.hard.0 {
        return Err(rlimit_err_to_errno(RlimitError::ExceedsHard));
    }
    if caller.credentials().has_cap(Capability::SysResource) {
        return Ok(());
    }
    if new.hard.0 > cur.hard.0 || new.soft.0 > cur.hard.0 {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// prlimit 读写其它进程时需要同属主，或者具备资源管理能力。
fn check_prlimit_target_permission(caller: &Arc<Task>, target: &Arc<Task>) -> Result<(), Errno> {
    let caller_creds = caller.credentials();
    if caller_creds.has_cap(Capability::SysResource) {
        return Ok(());
    }
    let target_creds = target.credentials();
    if caller_creds.uid == target_creds.uid
        && caller_creds.uid == target_creds.euid
        && caller_creds.uid == target_creds.suid
        && caller_creds.gid == target_creds.gid
        && caller_creds.gid == target_creds.egid
        && caller_creds.gid == target_creds.sgid
    {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

/// 返回整个 rlimit 表（用于调试/procfs）。
pub fn rlimits_snapshot() -> Rlimits {
    let me = current_task();
    *me.thread_group().rlimits().lock()
}

#[cfg(test)]
mod tests {
    use alloc::sync::{Arc, Weak};

    use super::*;
    use crate::group::ThreadGroup;
    use crate::ids::{CapSet, Credentials, Gid, Uid};
    use crate::rlimit::Rlim;

    fn task_with_credentials(creds: Credentials) -> Arc<Task> {
        let session = Session::new();
        let pg = ProcessGroup::new(&session);
        let tg = ThreadGroup::new();
        let task = Task::new(SchedParams::default_fair(), Weak::new(), tg, pg);
        task.set_credentials(Arc::new(creds));
        task
    }

    fn unprivileged_task(uid: u32, gid: u32) -> Arc<Task> {
        task_with_credentials(Credentials::unprivileged(Uid(uid), Gid(gid)))
    }

    #[test]
    fn rlimit_update_rejects_unprivileged_raise() {
        let caller = unprivileged_task(1000, 1000);
        let cur = RlimitPair::new(Rlim(100), Rlim(200));

        assert_eq!(
            validate_rlimit_update(&caller, cur, RlimitPair::new(Rlim(100), Rlim(300))),
            Err(Errno::EPERM)
        );
        assert_eq!(
            validate_rlimit_update(&caller, cur, RlimitPair::new(Rlim(250), Rlim(250))),
            Err(Errno::EPERM)
        );
        assert_eq!(
            validate_rlimit_update(&caller, cur, RlimitPair::new(Rlim(300), Rlim(200))),
            Err(Errno::EINVAL)
        );
        assert!(validate_rlimit_update(&caller, cur, RlimitPair::new(Rlim(50), Rlim(100))).is_ok());
    }

    #[test]
    fn rlimit_update_allows_sysresource_raise() {
        let mut creds = Credentials::unprivileged(Uid(1000), Gid(1000));
        creds.caps = CapSet::single(Capability::SysResource);
        let caller = task_with_credentials(creds);
        let cur = RlimitPair::new(Rlim(0), Rlim(0));

        assert!(validate_rlimit_update(&caller, cur, RlimitPair::new(Rlim(40), Rlim(40))).is_ok());
        assert_eq!(
            validate_rlimit_update(&caller, cur, RlimitPair::new(Rlim(41), Rlim(40))),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn prlimit_target_permission_requires_ids_or_sysresource() {
        let caller = unprivileged_task(1000, 1000);
        let same_owner = unprivileged_task(1000, 1000);
        assert!(check_prlimit_target_permission(&caller, &same_owner).is_ok());

        let mut different_saved_uid = Credentials::unprivileged(Uid(1000), Gid(1000));
        different_saved_uid.suid = Uid(1001);
        let different_owner = task_with_credentials(different_saved_uid);
        assert_eq!(
            check_prlimit_target_permission(&caller, &different_owner),
            Err(Errno::EPERM)
        );

        let mut resource_creds = Credentials::unprivileged(Uid(2000), Gid(2000));
        resource_creds.caps = CapSet::single(Capability::SysResource);
        let resource_caller = task_with_credentials(resource_creds);
        assert!(check_prlimit_target_permission(&resource_caller, &different_owner).is_ok());
    }
}

// ── 信号投递在内核边界的默认动作处理 ─────────────────────────────────────────

/// 消费当前任务的一条可投递信号；若其 action 是 Default，按 [`default_action`]
/// 施加副作用（Term 立刻走 exit）。如果 action 是 Handler（用户态），暂时
/// 返回 Some 供上层组装 sigframe（本实现暂无 userspace 不会触发）。
///
/// 在 [`schedule_once`] 入口调用可实现"调度边界检查信号"。
pub fn deliver_pending_signals() -> Option<SigInfo> {
    deliver_pending_signals_with_context(UserContextRef::NONE)
}

pub fn deliver_pending_signals_with_context(user_ctx: UserContextRef) -> Option<SigInfo> {
    let me = current_task();
    if me.is_kernel_task() {
        return None;
    }
    let info = me.signal.dequeue_one().or_else(|| {
        me.shared_signal()
            .dequeue_one(me.signal.blocked_snapshot().raw())
    })?;
    let action = me.shared_signal().get_action(info.sig);
    use crate::signal::SigHandler;
    match action.handler {
        SigHandler::Default => {
            apply_default_action(info);
            None
        }
        SigHandler::Ignore => None,
        SigHandler::Handler(_) => {
            let Some(ops) = process_image_ops() else {
                me.signal.deliver(info);
                return Some(info);
            };
            match (ops.setup_signal_frame)(&me, info, action, user_ctx) {
                Ok(()) => None,
                Err(Errno::ENOSYS) => {
                    me.signal.deliver(info);
                    Some(info)
                }
                Err(_) => {
                    apply_default_action(SigInfo {
                        sig: SignalNumber::SIGSEGV,
                        code: 0,
                        sender_pid: 0,
                        sender_uid: crate::ids::Uid::ROOT,
                        raw: None,
                    });
                    None
                }
            }
        }
    }
}

pub fn apply_default_action(info: SigInfo) {
    match default_action(info.sig) {
        DefaultAction::Term => {
            let me = current_task();
            me.mark_signaled_exit(info.sig, false);
            exit_task(&me, ExitCode(info.sig.raw() as i32));
        }
        DefaultAction::Core => {
            let me = current_task();
            me.mark_signaled_exit(info.sig, true);
            exit_task(&me, ExitCode(info.sig.raw() as i32));
        }
        DefaultAction::Stop => {
            let me = current_task();
            let _ = mark_task_stopped(&me, info.sig);
        }
        DefaultAction::Cont => {
            let me = current_task();
            let _ = continue_task(&me);
        }
        DefaultAction::Ign => {}
    }
}

// ── smoketest ────────────────────────────────────────────────────────────────

#[cfg(debug_assertions)]
pub mod smoketest {
    //! 启动期 sched 自检：覆盖 spawn / exit / reap / 上下文切换 / 多项 POSIX 动词。

    use super::*;
    use crate::arch_hooks::KernelEntry;
    use crate::scheduler::{init_task, runqueue};
    use crate::spawn::{SpawnKind, kthread_finish, kthread_spawn, spawn_child};
    use crate::wait::WaitQueue;

    unsafe extern "C" fn entry_finish(arg: usize) -> ! {
        log::info!(
            "[sched][smoke] kthread arg={} running, about to finish",
            arg
        );
        kthread_finish(ExitCode(arg as i32));
    }

    fn ke() -> KernelEntry {
        entry_finish
    }

    fn t1_basic() {
        let init = init_task();
        let rq_before = runqueue().nr_running();
        let pids_before = crate::scheduler::pid_count();
        let p = SchedParams::default_fair();
        let a = spawn_child(&init, SpawnKind::Process, p);
        let b = spawn_child(&init, SpawnKind::Thread, p);
        assert_eq!(runqueue().nr_running(), rq_before);
        assert!(Arc::ptr_eq(&b.thread_group(), &init.thread_group()));
        assert!(!Arc::ptr_eq(&a.thread_group(), &init.thread_group()));
        exit_task(&a, ExitCode(42));
        assert_eq!(a.state(), TaskState::Zombie);
        let (reaped, code) = crate::spawn::reap_child(&init).expect("reap a");
        assert!(Arc::ptr_eq(&reaped, &a));
        assert_eq!(code, ExitCode(42));
        assert!(
            b.cas_state(TaskState::Runnable, TaskState::Sleeping)
                || b.cas_state(TaskState::New, TaskState::Sleeping)
        );
        let wq = WaitQueue::new();
        wq.enqueue(&b);
        wq.wake_all();
        assert_eq!(b.state(), TaskState::Runnable);
        exit_task(&b, ExitCode(0));
        crate::spawn::reap_child(&init).expect("reap b");
        assert_eq!(runqueue().nr_running(), rq_before);
        assert_eq!(crate::scheduler::pid_count(), pids_before);
        drop(reaped);
        drop(a);
        drop(b);
        let kt = kthread_spawn(ke(), 99, SchedParams::default_fair());
        schedule_once(0);
        assert_eq!(kt.state(), TaskState::Zombie);
        let (rk, ck) = crate::spawn::reap_child(&init).expect("reap kt");
        assert_eq!(ck, ExitCode(99));
        drop(rk);
        drop(kt);
        log::info!("[sched][smoke] t1 PASS");
    }

    fn t2_kill_pending() {
        let init = init_task();
        let kt = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let pid = kt.pid_root().unwrap();
        super::tkill(pid, Some(SignalNumber::SIGUSR1)).expect("tkill");
        assert!(kt.signal.pending_snapshot().has(SignalNumber::SIGUSR1));
        exit_task(&kt, ExitCode(0));
        crate::spawn::reap_child(&init).expect("reap kt");
        log::info!("[sched][smoke] t2 PASS");
    }

    fn t3_setpgid() {
        let init = init_task();
        let a = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let b = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let a_pid = a.pid_root().unwrap();
        let b_pid = b.pid_root().unwrap();
        super::setpgid(a_pid, a_pid).expect("setpgid a");
        super::setpgid(b_pid, a_pid).expect("setpgid b->a");
        assert_eq!(super::getpgid(b_pid).unwrap(), a_pid);
        assert!(Arc::ptr_eq(&a.process_group(), &b.process_group()));
        exit_task(&a, ExitCode(0));
        exit_task(&b, ExitCode(0));
        crate::spawn::reap_child(&init).expect("reap a");
        crate::spawn::reap_child(&init).expect("reap b");
        drop(a);
        drop(b);
        log::info!("[sched][smoke] t3 PASS");
    }

    fn t4_signal_management() {
        let me = current_task();
        let new = SigAction {
            handler: crate::signal::SigHandler::Ignore,
            mask: SigSet::EMPTY,
            flags: crate::signal::SigActionFlags(0),
            restorer: 0,
        };
        let _old = super::sigaction(SignalNumber::SIGUSR2, new).expect("sigaction");
        let cur = me.shared_signal().get_action(SignalNumber::SIGUSR2);
        assert!(matches!(cur.handler, crate::signal::SigHandler::Ignore));
        let block = SigSet::EMPTY.with(SignalNumber::SIGUSR1);
        let _old = super::sigprocmask(SigProcMaskHow::Block, block).expect("sigprocmask");
        assert!(me.signal.blocked_snapshot().has(SignalNumber::SIGUSR1));
        super::tkill(me.pid_root().unwrap(), Some(SignalNumber::SIGUSR1)).expect("tkill self");
        assert!(super::sigpending().unwrap().has(SignalNumber::SIGUSR1));
        let _ = super::sigprocmask(SigProcMaskHow::Unblock, block).expect("unblock");
        let ign = SigAction {
            handler: crate::signal::SigHandler::Ignore,
            mask: SigSet::EMPTY,
            flags: crate::signal::SigActionFlags(0),
            restorer: 0,
        };
        super::sigaction(SignalNumber::SIGUSR1, ign).expect("ignore sigusr1");
        let _ = super::deliver_pending_signals();
        log::info!("[sched][smoke] t4 PASS");
    }

    fn t5_nice_yield() {
        let me = current_task();
        let before = me.sched.weight();
        let new_n = super::nice(5).expect("nice +5");
        assert_eq!(new_n, 5);
        assert_ne!(before, me.sched.weight());
        let _ = super::nice(-5);
        super::sched_yield().expect("yield");
        log::info!("[sched][smoke] t5 PASS");
    }

    fn t6_exit_group_simulated() {
        let init = init_task();
        let a = spawn_child(&init, SpawnKind::Thread, SchedParams::default_fair());
        let b = spawn_child(&init, SpawnKind::Thread, SchedParams::default_fair());
        assert!(Arc::ptr_eq(&a.shared_signal(), &init.shared_signal()));
        assert!(Arc::ptr_eq(&b.shared_signal(), &init.shared_signal()));
        exit_task(&a, ExitCode(7));
        exit_task(&b, ExitCode(7));
        let (_, ca) = crate::spawn::reap_child(&init).expect("reap a");
        let (_, cb) = crate::spawn::reap_child(&init).expect("reap b");
        assert_eq!(ca, ExitCode(7));
        assert_eq!(cb, ExitCode(7));
        drop(a);
        drop(b);
        log::info!("[sched][smoke] t6 PASS");
    }

    fn t7_clone_basic() {
        let init = init_task();
        let pid_before = crate::scheduler::pid_count();
        let _child = clone_task(
            &init,
            CloneArgs::fork_default(),
            SchedParams::default_fair(),
        );
        assert_eq!(crate::scheduler::pid_count(), pid_before + 1);
        let child = init.snapshot_children().last().cloned().expect("child arc");
        exit_task(&child, ExitCode(0));
        crate::spawn::reap_child(&init).expect("reap clone child");
        drop(child);
        log::info!("[sched][smoke] t7 PASS");
    }

    fn t8_orphan_reparent() {
        // A 派生 B；A 先退出并被 init reap；B 应被 reparent 到 init，
        // 最后 exit B → init 能 reap 到它。
        let init = init_task();
        let a = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let b = spawn_child(&a, SpawnKind::Process, SchedParams::default_fair());
        assert!(Arc::ptr_eq(&b.parent().expect("b has parent"), &a));
        exit_task(&a, ExitCode(0));
        // A 退出时把 B 交给 init。
        assert!(Arc::ptr_eq(&b.parent().expect("b reparented"), &init));
        crate::spawn::reap_child(&init).expect("reap a");
        exit_task(&b, ExitCode(0));
        crate::spawn::reap_child(&init).expect("reap b via init");
        drop(a);
        drop(b);
        log::info!("[sched][smoke] t8 PASS");
    }

    fn t9_kill_minus_one() {
        let init = init_task();
        let a = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let b = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let c = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        // init 就是当前任务；kill(-1) 应跳过 init 与自己（这里等价）。
        super::kill(-1, Some(SignalNumber::SIGTERM)).expect("kill -1");
        for t in [&a, &b, &c] {
            assert!(
                t.thread_group()
                    .shared_signal()
                    .pending_snapshot()
                    .has(SignalNumber::SIGTERM),
                "target missing SIGTERM"
            );
        }
        assert!(
            !init
                .thread_group()
                .shared_signal()
                .pending_snapshot()
                .has(SignalNumber::SIGTERM),
            "init must not be hit by kill(-1)"
        );
        for t in [&a, &b, &c] {
            exit_task(t, ExitCode(0));
            crate::spawn::reap_child(&init).expect("reap kill-1 child");
        }
        drop(a);
        drop(b);
        drop(c);
        log::info!("[sched][smoke] t9 PASS");
    }

    fn t10_idle_pick() {
        // kernel 层 boot_init 已经为 CPU 0 装了 idle；这里只断言槽位已填，
        // 且 idle 的亲和性限定在 CPU 0（bit 0）。不改动槽位，避免影响后续 bench。
        let idle = crate::scheduler::idle_task(0).expect("idle must be installed");
        assert_eq!(
            idle.cpu_affinity() & 0x1,
            0x1,
            "idle affinity must include cpu 0"
        );
        let _ = idle;
        log::info!("[sched][smoke] t10 PASS");
    }

    fn t11_clone_fs_files() {
        // 上层（kernel）已注册 TaskExtCloneHook，并给 init 装了 VfsContext +
        // FdTable。这里只验证 CloneFlags 行为：带 CLONE_FS|CLONE_FILES 时两侧
        // ext payload 的 Arc 必须 ptr_eq；不带时必须 !ptr_eq。
        let init = init_task();
        let vfs = init.ext_lookup(crate::task::TASKEXT_VFS_CONTEXT);
        let fdt = init.ext_lookup(crate::task::TASKEXT_VFS_FDTABLE);
        if vfs.is_none() || fdt.is_none() {
            log::info!("[sched][smoke] t11 SKIP (kernel ext not installed)");
            return;
        }
        let parent_vfs = vfs.unwrap();
        let parent_fdt = fdt.unwrap();

        // 共享路径：CLONE_FS | CLONE_FILES | SIGCHLD
        let share_flags = crate::clone_flags::CloneFlags(
            crate::clone_flags::CloneFlags::CLONE_FS
                | crate::clone_flags::CloneFlags::CLONE_FILES
                | 17, // SIGCHLD
        );
        let shared_args = CloneArgs {
            flags: share_flags,
            pidfd: 0,
            stack: 0,
            stack_size: 0,
            tls: 0,
            parent_tid: 0,
            child_tid: 0,
            exit_signal: 0,
            set_tid: 0,
            set_tid_size: 0,
            requested_pid: 0,
            cgroup: 0,
        };
        let shared_child = clone_task(&init, shared_args, SchedParams::default_fair());
        let sv = shared_child
            .ext_lookup(crate::task::TASKEXT_VFS_CONTEXT)
            .unwrap();
        let sf = shared_child
            .ext_lookup(crate::task::TASKEXT_VFS_FDTABLE)
            .unwrap();
        assert!(Arc::ptr_eq(&parent_vfs, &sv), "CLONE_FS must share vfs");
        assert!(
            Arc::ptr_eq(&parent_fdt, &sf),
            "CLONE_FILES must share fdtable"
        );

        // 深拷路径：fork 默认 flags（只 SIGCHLD）
        let fork_child = clone_task(
            &init,
            CloneArgs::fork_default(),
            SchedParams::default_fair(),
        );
        let fv = fork_child
            .ext_lookup(crate::task::TASKEXT_VFS_CONTEXT)
            .unwrap();
        let ff = fork_child
            .ext_lookup(crate::task::TASKEXT_VFS_FDTABLE)
            .unwrap();
        assert!(!Arc::ptr_eq(&parent_vfs, &fv), "fork must deep-copy vfs");
        assert!(
            !Arc::ptr_eq(&parent_fdt, &ff),
            "fork must deep-copy fdtable"
        );

        for c in [&shared_child, &fork_child] {
            exit_task(c, ExitCode(0));
            crate::spawn::reap_child(&init).expect("reap t11 child");
        }
        drop(shared_child);
        drop(fork_child);
        log::info!("[sched][smoke] t11 PASS");
    }

    fn t12_timer_tick() {
        // 直接打一次 tick，观察 NEED_RESCHED 能否被 preempt_if_needed 消费。
        // 具体切换行为依赖 rq 状态，这里只验证两个入口能无恐慌走完。
        crate::scheduler::on_timer_tick(u64::MAX / 2);
        crate::scheduler::preempt_if_needed(u64::MAX / 2);
        log::info!("[sched][smoke] t12 PASS");
    }

    fn t13_sched_classes() {
        let init = init_task();
        let local = crate::runqueue::Runqueue::new();

        let fair = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let rt = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let dl = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let rr1 = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let rr2 = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());

        super::sched_setattr(rt.pid_root().unwrap(), crate::SchedAttr::rt_fifo(20))
            .expect("set rt");
        super::sched_setattr(
            dl.pid_root().unwrap(),
            crate::SchedAttr::deadline(1_000_000, 2_000_000, 2_000_000),
        )
        .expect("set deadline");
        super::sched_setattr(
            rr1.pid_root().unwrap(),
            crate::SchedAttr::rt_round_robin(10, 1_000_000),
        )
        .expect("set rr1");
        super::sched_setattr(
            rr2.pid_root().unwrap(),
            crate::SchedAttr::rt_round_robin(10, 1_000_000),
        )
        .expect("set rr2");
        assert!(
            crate::SchedAttr::deadline(3, 2, 2).validate().is_err(),
            "deadline runtime > deadline must be rejected"
        );
        assert!(
            crate::SchedAttr::rt_fifo(0).validate().is_err(),
            "rt priority 0 must be rejected"
        );

        local.enqueue(Arc::clone(&fair), 1);
        local.enqueue(Arc::clone(&rt), 1);
        local.enqueue(Arc::clone(&dl), 1);
        let first = local.pick_next(2).expect("pick deadline");
        assert!(Arc::ptr_eq(&first, &dl), "deadline class must win");
        assert!(local.dequeue(&dl, 3));
        let second = local.pick_next(4).expect("pick rt");
        assert!(Arc::ptr_eq(&second, &rt), "rt class must outrank fair");
        assert!(local.dequeue(&rt, 5));
        assert!(local.dequeue(&fair, 6));

        local.enqueue(Arc::clone(&rr1), 10);
        local.enqueue(Arc::clone(&rr2), 10);
        let first_rr = local.pick_next(11).expect("pick rr1");
        assert!(Arc::ptr_eq(&first_rr, &rr1));
        assert!(local.tick(11 + 2_000_000), "RR slice expiry must resched");
        let second_rr = local.pick_next(11 + 2_000_000).expect("pick rr2");
        assert!(Arc::ptr_eq(&second_rr, &rr2), "RR peers must rotate");
        assert!(local.dequeue(&rr1, 14 + 2_000_000));
        assert!(local.dequeue(&rr2, 14 + 2_000_000));

        let weight_a = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        let weight_b = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        local.enqueue(Arc::clone(&weight_a), 20);
        local.enqueue(Arc::clone(&weight_b), 20);
        let ran = local.pick_next(20).expect("pick fair weight task");
        local
            .pick_next(20 + crate::eevdf::DEFAULT_BASE_SLICE_NS)
            .expect("rotate fair weight task");
        let before = local.avg_vruntime();
        local.update_params(
            &ran,
            SchedParams {
                nice: -20,
                slice_ns: 0,
            },
            20 + crate::eevdf::DEFAULT_BASE_SLICE_NS,
        );
        let after = local.avg_vruntime();
        assert!(
            after > before,
            "queued fair weight update must keep rq accounting coherent"
        );
        assert!(local.dequeue(&weight_a, 21 + crate::eevdf::DEFAULT_BASE_SLICE_NS));
        assert!(local.dequeue(&weight_b, 21 + crate::eevdf::DEFAULT_BASE_SLICE_NS));

        assert!(crate::scheduler::is_cpu_online(0));
        assert!(!crate::scheduler::is_cpu_online(crate::scheduler::NR_CPUS));
        assert!(crate::scheduler::register_cpu(crate::scheduler::NR_CPUS).is_err());

        for task in [&fair, &rt, &dl, &rr1, &rr2, &weight_a, &weight_b] {
            exit_task(task, ExitCode(0));
            crate::spawn::reap_child(&init).expect("reap sched class child");
        }
        log::info!("[sched][smoke] t13 PASS");
    }

    fn t14_stop_continue_wait() {
        let init = init_task();
        let child = spawn_child(&init, SpawnKind::Process, SchedParams::default_fair());
        child.into_kernel_thread(ke(), 0);
        activate_task(&child).expect("activate stop/continue child");
        let pid = child.pid_root().unwrap();

        assert!(crate::scheduler::mark_task_stopped(
            &child,
            SignalNumber::SIGSTOP
        ));
        assert_eq!(child.state(), TaskState::Stopped);

        let stop_peek = super::wait4(
            pid,
            WaitOptions::from_raw(
                WaitOptions::WUNTRACED | WaitOptions::WNOHANG | WaitOptions::WNOWAIT,
            ),
        )
        .expect("peek stopped child");
        assert_eq!(stop_peek.pid, pid);
        assert!(stop_peek.status.wifstopped());
        assert_eq!(
            stop_peek.status.wstopsig(),
            SignalNumber::SIGSTOP.raw() as i32
        );

        let stopped = super::wait4(
            pid,
            WaitOptions::from_raw(WaitOptions::WUNTRACED | WaitOptions::WNOHANG),
        )
        .expect("consume stopped child");
        assert_eq!(stopped.pid, pid);
        assert!(stopped.status.wifstopped());

        let no_stopped = super::wait4(
            pid,
            WaitOptions::from_raw(WaitOptions::WUNTRACED | WaitOptions::WNOHANG),
        )
        .expect("stopped event consumed");
        assert_eq!(no_stopped.pid, 0);

        assert!(crate::scheduler::continue_task(&child));
        assert_eq!(child.state(), TaskState::Runnable);

        let cont_peek = super::wait4(
            pid,
            WaitOptions::from_raw(
                WaitOptions::WCONTINUED | WaitOptions::WNOHANG | WaitOptions::WNOWAIT,
            ),
        )
        .expect("peek continued child");
        assert_eq!(cont_peek.pid, pid);
        assert!(cont_peek.status.wifcontinued());

        let continued = super::waitid(
            WaitId::Pid(pid),
            WaitOptions::from_raw(WaitOptions::WCONTINUED | WaitOptions::WNOHANG),
        )
        .expect("consume continued child");
        assert_eq!(continued.pid, pid);
        assert!(continued.status.wifcontinued());

        let no_continued = super::wait4(
            pid,
            WaitOptions::from_raw(WaitOptions::WCONTINUED | WaitOptions::WNOHANG),
        )
        .expect("continued event consumed");
        assert_eq!(no_continued.pid, 0);

        exit_task(&child, ExitCode(0));
        let exited =
            super::wait4(pid, WaitOptions::from_raw(WaitOptions::WNOHANG)).expect("reap child");
        assert_eq!(exited.pid, pid);
        assert!(exited.status.wifexited());
        assert_eq!(exited.status.wexitstatus(), 0);
        drop(child);
        log::info!("[sched][smoke] t14 PASS");
    }

    pub fn run() {
        log::info!("[sched][smoke] start");
        t1_basic();
        t2_kill_pending();
        t3_setpgid();
        t4_signal_management();
        t5_nice_yield();
        t6_exit_group_simulated();
        t7_clone_basic();
        t8_orphan_reparent();
        t9_kill_minus_one();
        t10_idle_pick();
        t11_clone_fs_files();
        t12_timer_tick();
        t13_sched_classes();
        t14_stop_continue_wait();
        log::info!(
            "[sched][smoke] ALL PASS rq={} pids={}",
            runqueue().nr_running(),
            crate::scheduler::pid_count(),
        );
    }
}
