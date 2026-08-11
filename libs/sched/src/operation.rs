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
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;

use crate::clone_flags::{CloneArgs, CloneFlags};
use crate::cpu::{CpuId, CpuMask};
use crate::eevdf::SchedParams;
use crate::group::{GroupExitStatus, ProcessGroup, Session, ThreadGroup};
use crate::ids::{Capability, Gid, Uid};
use crate::pid::PidT;
use crate::process_ops::{ExecRequest, UserContextRef, process_image_ops};
use crate::rlimit::{Resource, RlimitError, RlimitPair, Rlimits};
use crate::rseq::RseqEvent;
use crate::sched_class::{SchedAttr, SchedPolicy};
use crate::scheduler::{
    NR_CPUS, continue_task, current_cpu_id, current_task, deliver_shared_signal_to_group,
    enqueue_task_deferred, mark_task_stopped, migrate_task, now_ns_public, online_cpu_mask,
    request_balance, request_post_syscall_handoff, request_resched, root_pid_ns, runqueue_of,
    schedule_once, select_cpu_for_mask, signal_wakeup, task_runqueue_cpu,
};
use crate::signal::{
    DefaultAction, SigAction, SigActionFlags, SigHandler, SigInfo, SigProcMaskHow, SigSet,
    SignalNumber, default_action,
};
use crate::spawn::{abort_new_task, activate_task, clone_task, exit_task, reap_matching};
use crate::task::{Task, TaskUsage};
use crate::wait_flags::{WaitId, WaitOptions, WaitResult, WaitStatus};
use crate::{ExitCode, TaskState};

// ── 无副作用查询 ──────────────────────────────────────────────────────────────

/// 当前进程 pid（Linux 语义：线程组 leader 的 pid）。
#[kernel_symbols::export(name = "sched.operation.getpid", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn getpid() -> PidT {
    let me = current_task();
    me.thread_group()
        .leader()
        .and_then(|l| l.pid_root())
        .or_else(|| me.pid_root())
        .unwrap_or(0)
}

/// 当前线程 tid。
#[kernel_symbols::export(name = "sched.operation.gettid", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn gettid() -> PidT {
    current_task().pid_root().unwrap_or(0)
}

/// 父进程 pid。父若不在根 ns 或已被 reparent，会返回 reparent 后的新父。
#[kernel_symbols::export(name = "sched.operation.getppid", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
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
    new_pg.set_pgid(my_pid);
    new_session.set_sid(my_pid);
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
            let _ = deliver_to_thread_group(&m, info);
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
    let code = tg.request_group_exit(code);
    let members = tg.snapshot();
    for m in members.iter() {
        if Arc::ptr_eq(m, &me) {
            continue;
        }
        if m.is_kernel_task() {
            continue;
        }
        if m.state() != TaskState::Zombie && m.state() != TaskState::Dead {
            // 不能从发送者上下文直接 exit：目标可能停在 futex/poll 内核栈上，
            // 栈上的 Arc<VmSpace> 只有在线程自行展开时才会析构。
            crate::scheduler::group_exit_wakeup(m);
        }
    }
    drop(members);
    drop(tg);
    drop(me);
    exit(code);
}

/// Native 同步用户异常的终止入口。它不构造 Linux signal frame，而是记录
/// 进程级诊断并复用现有组退出清理。
pub fn terminate_native_fault(
    task: &Arc<Task>,
    kind: u32,
    exception_code: u64,
    address: u64,
    exit_code: i32,
) -> ! {
    let group = task.thread_group();
    group.record_native_fault(crate::group::NativeFaultInfo {
        kind,
        exception_code,
        address,
    });
    let code = group.request_group_exit(exit_code);
    for member in group.snapshot() {
        if !Arc::ptr_eq(&member, task)
            && !member.is_kernel_task()
            && !matches!(member.state(), TaskState::Zombie | TaskState::Dead)
        {
            crate::scheduler::group_exit_wakeup(&member);
        }
    }
    drop(group);
    exit(code)
}

/// 在目标线程自己的安全边界完成已经发布的 exit_group 请求。
pub fn complete_group_exit_if_requested(task: &Arc<Task>) -> bool {
    // 正常 syscall/返回路径绝大多数没有协作退出请求；先读每任务原子标志，
    // 避免为这条快路径获取 `rel` 锁并克隆线程组 Arc。
    if !task.group_exit_boundary_pending() {
        return false;
    }
    complete_group_exit_slow(task)
}

#[cold]
#[inline(never)]
fn complete_group_exit_slow(task: &Arc<Task>) -> bool {
    let Some(status) = task.thread_group().group_exit_status() else {
        return false;
    };
    if !matches!(task.state(), TaskState::Zombie | TaskState::Dead) {
        if let GroupExitStatus::Signaled {
            signal,
            core_dumped,
        } = status
        {
            task.mark_signaled_exit(signal, core_dumped);
        }
        exit_task(task, ExitCode(status.exit_code()));
    }
    true
}

/// 在目标线程自己的安全边界完成 Native Thread capability 发布的退出请求。
pub fn complete_native_thread_exit_if_requested(task: &Arc<Task>) -> bool {
    let Some(code) = task.native_thread_exit_boundary_pending() else {
        return false;
    };
    if !matches!(task.state(), TaskState::Zombie | TaskState::Dead) {
        exit_task(task, ExitCode(code));
    }
    true
}

/// 要求 exec 发起者之外的线程在自己的安全边界退出。
pub fn request_exec_sibling_exit(target: &Arc<Task>, preserve_leader_identity: bool) {
    crate::scheduler::exec_sibling_exit_wakeup(target, preserve_leader_identity);
}

/// 在目标线程自己的 syscall/用户返回边界完成 exec 协作退出。
pub fn complete_exec_sibling_exit_if_requested(task: &Arc<Task>) -> bool {
    if !task.exec_sibling_exit_boundary_pending() {
        return false;
    }
    match task.state() {
        // 保留 leader 身份的 exec 退出必须停在 Zombie，直到执行线程完成 PID、
        // 父子关系和 children 的事务迁移；提前置 Dead 会让失败回滚无处可收。
        TaskState::Zombie => {}
        TaskState::Dead => {}
        _ => exit_task(task, ExitCode(0)),
    }
    true
}

/// 在请求旧 leader 退出前预留身份迁移所需的 children 容量。
pub fn prepare_exec_leader_identity(
    executor: &Arc<Task>,
    old_leader: &Arc<Task>,
) -> Result<(), Errno> {
    if Arc::ptr_eq(executor, old_leader) {
        return Err(Errno::EINVAL);
    }
    let group = executor.thread_group();
    if !Arc::ptr_eq(&group, &old_leader.thread_group())
        || !group
            .leader()
            .is_some_and(|leader| Arc::ptr_eq(&leader, old_leader))
    {
        return Err(Errno::EBUSY);
    }
    executor
        .try_reserve_children_for_exec(old_leader.child_count())
        .map_err(|_| Errno::ENOMEM)
}

/// 非 leader 线程执行 exec 后，接管已经停止的旧 leader 进程身份。
pub fn adopt_exec_leader_identity(
    executor: &Arc<Task>,
    old_leader: &Arc<Task>,
) -> Result<(), Errno> {
    let identity = crate::pid::lock_process_identity();
    if Arc::ptr_eq(executor, old_leader) {
        return Err(Errno::EINVAL);
    }
    let group = executor.thread_group();
    if !Arc::ptr_eq(&group, &old_leader.thread_group())
        || old_leader.state() != TaskState::Zombie
        || !old_leader.exec_sibling_exit_preserves_identity()
        || !group.has_only_exec_members(executor, old_leader)
        || !group
            .leader()
            .is_some_and(|leader| Arc::ptr_eq(&leader, old_leader))
    {
        return Err(Errno::EBUSY);
    }

    let parent = old_leader.parent_in(&identity);
    if parent
        .as_ref()
        .is_some_and(|parent| !parent.has_child(old_leader))
        || old_leader.pid_root_in(&identity).is_none()
        || executor.pid_root_in(&identity).is_none()
        || !old_leader.pid_registrations_owned_by_self_in(&identity)
        || !executor.pid_registrations_owned_by_self_in(&identity)
    {
        return Err(Errno::EAGAIN);
    }
    if !executor.children_capacity_for_exec(old_leader.child_count()) {
        return Err(Errno::ENOMEM);
    }

    let leader_registrations = old_leader.take_pid_registrations_for_exec_in(&identity);
    let executor_registrations = executor.take_pid_registrations_for_exec_in(&identity);
    let mut replaced = 0;
    for (namespace, pid) in leader_registrations.iter() {
        if !namespace
            .registry()
            .replace_owner_in(&identity, *pid, old_leader, executor)
        {
            for (rollback_namespace, rollback_pid) in leader_registrations[..replaced].iter() {
                let restored = rollback_namespace.registry().replace_owner_in(
                    &identity,
                    *rollback_pid,
                    executor,
                    old_leader,
                );
                debug_assert!(restored, "exec leader PID owner 回滚失败");
            }
            old_leader.install_pid_registrations_for_exec_in(&identity, leader_registrations);
            executor.install_pid_registrations_for_exec_in(&identity, executor_registrations);
            return Err(Errno::EAGAIN);
        }
        replaced += 1;
    }

    if let Some(parent) = parent.as_ref() {
        if !parent.replace_child_for_exec_in(&identity, old_leader, executor) {
            for (namespace, pid) in leader_registrations.iter() {
                let restored = namespace
                    .registry()
                    .replace_owner_in(&identity, *pid, executor, old_leader);
                debug_assert!(restored, "exec leader PID owner 回滚失败");
            }
            old_leader.install_pid_registrations_for_exec_in(&identity, leader_registrations);
            executor.install_pid_registrations_for_exec_in(&identity, executor_registrations);
            return Err(Errno::EAGAIN);
        }
        executor.reparent_to_in(&identity, parent);
    } else {
        executor.clear_parent_for_exec_in(&identity);
    }
    old_leader.clear_parent_for_exec_in(&identity);

    for (namespace, pid) in executor_registrations.iter() {
        namespace.registry().release_in(&identity, *pid);
    }
    executor.install_pid_registrations_for_exec_in(&identity, leader_registrations);
    if let Some(leader_pid) = executor.pid_root_in(&identity) {
        executor.set_tgid_cache(leader_pid);
    }

    let children = old_leader.take_children_for_exec_in(&identity);
    for child in children.iter() {
        child.reparent_to_in(&identity, executor);
    }
    executor.append_children_for_exec_in(&identity, children);

    executor.set_exit_signal(old_leader.exit_signal());
    old_leader.set_exit_signal(0);
    group.set_leader_in(&identity, executor);
    let removed = group.remove_member(old_leader);
    debug_assert!(removed, "exec leader 身份迁移后旧 leader 应仍在成员表中");
    old_leader.process_group().remove_member(old_leader);
    old_leader.set_state(TaskState::Dead);
    old_leader.exit_waiters.wake_all();
    old_leader.clear_exec_sibling_exit_request();
    Ok(())
}

// ── 调度器相关 ────────────────────────────────────────────────────────────────

#[kernel_symbols::export(name = "sched.operation.sched_yield", contract = "kernel.sched.process-control@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn sched_yield() -> Result<(), Errno> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::SchedYield);
    current_task().record_voluntary_context_switch();
    // 主动让出仍然消耗了当前任务的实际 CPU 时间。传入 0 会跳过本次
    // vruntime 记账，使持续运行的内核 worker 在多个任务之间反复让出时
    // 永远保持过低的虚拟时间，最终饿死用户任务。
    schedule_once(now_ns_public());
    Ok(())
}

/// `nice(inc)`：相对当前 nice 调整，结果钳到 [-20, 19]。返回新 nice 值。
pub fn nice(inc: i32) -> Result<i32, Errno> {
    let me = current_task();
    let cur_nice = me.pi_base_attr().nice as i32;
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
    let now_ns = crate::scheduler::now_ns_public();
    let owner = task_runqueue_cpu(task).unwrap_or_else(CpuId::boot);
    let capacity = crate::scheduler::cpu_capacity(owner);
    crate::scheduler::deadline_admission().update_attr(task, owner, attr, capacity, || {
        runqueue_of(owner.get()).update_sched_attr(task, attr, now_ns)
    })
}

/// 应用 PI 子系统已经计算出的有效调度属性。
///
/// 该入口不改写用户设置的基础属性，也不创建新的 Deadline 带宽 reservation；
/// PI 解除后调用方必须再次传入 `Task::pi_remove_donation` 返回的属性。
pub fn pi_apply_effective_attr(task: &Arc<Task>, attr: SchedAttr) -> Result<(), Errno> {
    let owner = task_runqueue_cpu(task).unwrap_or_else(CpuId::boot).get();
    let result = update_task_sched_entity(task, |cpu_id, task, now_ns| {
        runqueue_of(cpu_id).update_sched_attr_raw(task, attr.normalized(), now_ns)
    });
    if result.is_ok() {
        request_resched(owner);
    }
    result
}

fn update_task_sched_entity(
    task: &Arc<Task>,
    mut update: impl FnMut(usize, &Arc<Task>, u64) -> bool,
) -> Result<(), Errno> {
    let now_ns = crate::scheduler::now_ns_public();
    let owner = task_runqueue_cpu(task).map_or(0, CpuId::get);
    if update(owner, task, now_ns) {
        Ok(())
    } else {
        Err(Errno::EBUSY)
    }
}

#[kernel_symbols::export(name = "sched.operation.sched_getattr", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn sched_getattr(pid: PidT) -> Result<SchedAttr, Errno> {
    let task = lookup_pid(pid)?;
    Ok(sched_getattr_for_task(&task))
}

pub fn sched_getattr_for_task(task: &Arc<Task>) -> SchedAttr {
    task.pi_base_attr()
}

pub fn set_sched_reset_on_fork(pid: PidT, enabled: bool) -> Result<(), Errno> {
    lookup_pid(pid)?.set_sched_reset_on_fork(enabled);
    Ok(())
}

pub fn sched_reset_on_fork(pid: PidT) -> Result<bool, Errno> {
    Ok(lookup_pid(pid)?.sched_reset_on_fork())
}

pub fn set_task_nice(task: &Arc<Task>, nice: i8) {
    let mut attr = task.pi_base_attr();
    attr.nice = nice.clamp(crate::eevdf::NICE_MIN, crate::eevdf::NICE_MAX);
    let owner = task_runqueue_cpu(task).map_or(0, CpuId::get);
    runqueue_of(owner).update_sched_attr(task, attr, crate::scheduler::now_ns_public());
}

#[kernel_symbols::export(name = "sched.operation.task_usage", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn task_usage(pid: PidT) -> Result<TaskUsage, Errno> {
    Ok(lookup_pid(pid)?.usage_snapshot(crate::scheduler::now_ns_public()))
}

pub fn children_usage(pid: PidT) -> Result<TaskUsage, Errno> {
    Ok(lookup_pid(pid)?.child_usage_snapshot())
}

#[kernel_symbols::export(name = "sched.operation.all_tasks_snapshot", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED | kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
pub fn all_tasks_snapshot() -> Vec<Arc<Task>> {
    root_pid_ns()
        .registry()
        .snapshot()
        .into_iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .collect()
}

/// 为 PONR 前资源准备提供可失败的全局任务快照。
pub fn try_all_tasks_snapshot() -> Result<Vec<Arc<Task>>, alloc::collections::TryReserveError> {
    root_pid_ns().registry().try_snapshot_tasks()
}

#[kernel_symbols::export(name = "sched.operation.sched_getaffinity", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn sched_getaffinity(pid: PidT) -> Result<u64, Errno> {
    let task = lookup_pid(pid)?;
    Ok(sched_getaffinity_for_task(&task))
}

pub fn sched_getaffinity_for_task(task: &Arc<Task>) -> u64 {
    task.cpu_affinity() & online_cpu_mask()
}

#[kernel_symbols::export(name = "sched.operation.sched_setaffinity", contract = "kernel.sched.process-control@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
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
    let old_affinity = task.cpu_affinity();
    task.set_cpu_affinity(requested.bits());

    let current_cpu = task.current_cpu();
    let current = CpuId::new(current_cpu);
    if current.is_some_and(|cpu| requested.contains(cpu) && online.contains(cpu)) {
        return Ok(());
    }

    let target = if task.sched.policy() == SchedPolicy::Deadline {
        requested
            .intersection(online)
            .iter()
            .filter(|cpu| {
                crate::scheduler::deadline_admission().can_migrate(
                    task,
                    *cpu,
                    crate::scheduler::cpu_capacity(*cpu),
                )
            })
            .min_by_key(|cpu| crate::scheduler::deadline_admission().reserved(*cpu))
            .map(CpuId::get)
    } else {
        select_cpu_for_mask(requested, current, false).map(CpuId::get)
    };

    if let Some(target_cpu) = target {
        // migrate_task 在持有 CPU_HOTPLUG_LOCK 时无法等待迁移事务，遇到迁移中的
        // 任务只会返回 EBUSY。这里在锁外等它落地后重试一次，避免把一次正常的
        // 并发负载均衡当成 setaffinity 失败。
        crate::scheduler::wait_for_migration_to_settle(task);
        let mut result = migrate_task(task, target_cpu);
        if result.is_err() && task.sched.is_migrating() {
            crate::scheduler::wait_for_migration_to_settle(task);
            result = migrate_task(task, target_cpu);
        }
        if let Err(error) = result {
            if task.sched.policy() == SchedPolicy::Deadline {
                task.set_cpu_affinity(old_affinity);
                return Err(error);
            }
            request_balance(target_cpu);
            if current_cpu < NR_CPUS {
                request_resched(current_cpu);
            }
        }
    } else if task.sched.policy() == SchedPolicy::Deadline {
        task.set_cpu_affinity(old_affinity);
        return Err(Errno::EBUSY);
    } else if current_cpu < NR_CPUS {
        request_resched(current_cpu);
    }
    Ok(())
}

/// getcpu：返回当前调度 CPU；节点编号由兼容层保持 UMA 语义。
#[kernel_symbols::export(name = "sched.operation.getcpu", contract = "kernel.sched.process-query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn getcpu() -> Result<(u32, u32), Errno> {
    Ok((current_cpu_id() as u32, 0))
}

// ── execve / sigreturn ──────────────────────────────────────────────

/// 从内核路径创建并启动一个新的用户进程。
///
/// 本函数建立 fork 语义的任务关系和子系统扩展，再把镜像装载与架构用户上下文
/// 安装交给 [`crate::process_ops::ProcessImageOps`]。返回的任务已经进入运行队列；
/// 任一步骤失败都会回滚尚未运行的子任务。
#[kernel_symbols::export(
    name = "sched.operation.spawn_user_process",
    contract = "kernel.sched.user-process@1",
    version = 1,
    capabilities = kernel_symbols::capability::SCHED_TASK
        | kernel_symbols::capability::VFS_IO
        | kernel_symbols::capability::MM_MEMORY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn spawn_user_process(
    parent: &Arc<Task>,
    path: &str,
    argv: &[String],
    envp: &[String],
) -> Result<Arc<Task>, Errno> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(Errno::EINVAL);
    }

    let child = clone_task(
        parent,
        CloneArgs::fork_default(),
        SchedParams::default_fair(),
    );
    if child.state() == TaskState::Dead || child.pid_root().is_none() {
        abort_new_task(&child);
        return Err(Errno::EAGAIN);
    }

    let Some(ops) = process_image_ops() else {
        abort_new_task(&child);
        return Err(Errno::ENOSYS);
    };
    if let Err(error) = (ops.spawn_user_process)(parent, &child, path, argv, envp) {
        abort_new_task(&child);
        return Err(error);
    }
    if let Err(error) = activate_task(&child) {
        abort_new_task(&child);
        return Err(error);
    }
    Ok(child)
}

pub fn execve(request: ExecRequest) -> Result<(), Errno> {
    execve_with_context(request, UserContextRef::NONE)
}

pub fn execve_with_context(request: ExecRequest, user_ctx: UserContextRef) -> Result<(), Errno> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::ProcessExec);
    let me = current_task();
    let ops = process_image_ops().ok_or(Errno::ENOSYS)?;
    (ops.execve)(&me, request, user_ctx)?;
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

/// 已完成任务图和用户上下文构造、但尚未进入运行队列的 clone 事务。
///
/// syscall personality 可在 [`Self::activate`] 前完成 pidfd 等可失败输出；若
/// 提前返回，`Drop` 会回滚尚未对调度器可见的 child。
pub struct PreparedClone {
    parent: Option<Arc<Task>>,
    child: Option<Arc<Task>>,
    pid: PidT,
    args: CloneArgs,
}

impl PreparedClone {
    fn from_parts(parent: Arc<Task>, child: Arc<Task>, pid: PidT, args: CloneArgs) -> Self {
        Self {
            parent: Some(parent),
            child: Some(child),
            pid,
            args,
        }
    }

    pub fn pid(&self) -> PidT {
        self.pid
    }

    pub fn child(&self) -> &Arc<Task> {
        self.child.as_ref().expect("prepared clone child 已被消费")
    }

    /// 发布 child 并完成 vfork / 调度交接语义。
    pub fn activate(mut self) -> Result<CloneOutcome, Errno> {
        let child = self.child.take().expect("prepared clone 只能激活一次");
        let parent = self.parent.take().expect("prepared clone 缺少父任务");
        if let Err(err) = activate_task(&child) {
            abort_new_task(&child);
            return Err(err);
        }

        if self.args.flags.has(CloneFlags::CLONE_VFORK) {
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
                if parent.group_exit_pending() {
                    break;
                }
                let entry = wait_child
                    .vfork_done
                    .prepare_to_wait(&parent, TaskState::Sleeping);
                if !wait_child.is_vforking() || parent.group_exit_pending() {
                    wait_child.vfork_done.finish_wait(&entry);
                    break;
                }
                drop(wait_child);
                drop(parent);
                schedule_once(crate::scheduler::now_ns_public());
                if let Some(wait_child) = child_wait.upgrade() {
                    wait_child.vfork_done.finish_wait(&entry);
                }
            }
        } else if !self.args.flags.has(CloneFlags::CLONE_THREAD) {
            // 不在 clone syscall 尚未返回时直接重入调度：父进程的 trap frame
            // 仍由 syscall 出口负责写返回值和推进 PC。这里只登记一次收尾后的
            // 启动交接，由 syscall dispatcher 在安全边界切给新子进程。
            request_post_syscall_handoff();
        }

        Ok(CloneOutcome {
            pid: self.pid,
            child,
        })
    }
}

impl Drop for PreparedClone {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            abort_new_task(&child);
        }
    }
}

pub fn prepare_clone_with_context(
    args: CloneArgs,
    user_ctx: UserContextRef,
) -> Result<PreparedClone, Errno> {
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

    Ok(PreparedClone::from_parts(parent, child, pid, args))
}

pub fn clone_with_context_outcome(
    args: CloneArgs,
    user_ctx: UserContextRef,
) -> Result<CloneOutcome, Errno> {
    prepare_clone_with_context(args, user_ctx)?.activate()
}

pub(crate) fn validate_clone_args(args: CloneArgs) -> Result<(), Errno> {
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
        | CloneFlags::CLONE_IO
        | CloneFlags::CLONE_CLEAR_SIGHAND;
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
    if flags.has(CloneFlags::CLONE_SIGHAND) && flags.has(CloneFlags::CLONE_CLEAR_SIGHAND) {
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
    // clone3 的 pidfd 字段只有在 CLONE_PIDFD 置位时才有意义。glibc 的
    // pthread_create() 会复用 clone_args 缓冲，把该字段填成 TID 地址但不置
    // CLONE_PIDFD；Linux 对这种无效字段是忽略而不是返回 EINVAL，否则用户态
    // 不会 fallback 到传统 clone，线程创建会直接失败。
    if flags.has(CloneFlags::CLONE_PIDFD) {
        if args.pidfd == 0 || flags.has(CloneFlags::CLONE_THREAD) {
            return Err(Errno::EINVAL);
        }
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
    if flags.has(CloneFlags::CLONE_CLEAR_SIGHAND) && flags.has(CloneFlags::CLONE_SIGHAND) {
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
        WaitId::Pgid(pgid) => child.process_group().pgid() == *pgid,
        WaitId::SameGroup => Arc::ptr_eq(&child.process_group(), &parent.process_group()),
        WaitId::Pidfd(group) => Arc::ptr_eq(&child.thread_group(), group),
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
        if wait_exited && child.is_waitable_zombie() {
            return true;
        }
        if (wait_stopped || child.is_ptrace_traced()) && child.wait_stopped_status(true).is_some() {
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
#[kernel_symbols::export(name = "sched.operation.wait4", contract = "kernel.sched.process-control@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
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
                    .find(|c| c.is_waitable_zombie() && pred(c))
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
                #[cfg(feature = "trace-task-lifecycle")]
                log::info!(
                    "[sched][wait] reap parent={:?} child={:?} target={:?}",
                    me.pid_root(),
                    child.pid_root(),
                    target,
                );
                return Ok(WaitResult {
                    pid: child.pid_root().unwrap_or(0),
                    status: child_exit_status(&child, code),
                    usage: child.usage_snapshot(crate::scheduler::now_ns_public()),
                });
            }
        }

        // 2. stopped / continued 是父侧可消费的状态变化事件，不会 reap child。
        let children = me.snapshot_children();
        for child in children.iter().filter(|c| matches_waitid(c, &target, &me)) {
            if wait_stopped || child.is_ptrace_traced() {
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
        // 若已有可投递 handler，必须先回到 syscall 分发器构造 sigframe；
        // SA_RESTART 的重启处理也依赖原始 syscall 参数尚未被返回值覆盖。
        if has_interrupting_signal(&me) {
            return Err(Errno::EINTR);
        }
        let entry = me.exit_waiters.prepare_to_wait(&me, TaskState::Sleeping);
        if has_interrupting_signal(&me) {
            me.exit_waiters.finish_wait(&entry);
            return Err(Errno::EINTR);
        }
        if wait_child_observable(
            &me,
            target.clone(),
            wait_exited,
            wait_stopped,
            wait_continued,
        ) {
            me.exit_waiters.finish_wait(&entry);
            continue;
        }
        #[cfg(feature = "trace-task-lifecycle")]
        {
            log::info!(
                "[sched][wait] block parent={:?} target={:?} children={}",
                me.pid_root(),
                target,
                children.len(),
            );
            for child in children
                .iter()
                .filter(|child| matches_waitid(child, &target, &me))
            {
                log::info!(
                    "[sched][wait] child={:?} state={:?} exit_ready={} threads={}",
                    child.pid_root(),
                    child.state(),
                    child.exit_event_ready(),
                    child.thread_group().snapshot().len(),
                );
            }
        }
        drop(me);
        schedule_once(crate::scheduler::now_ns_public());
        me = current_task();
        #[cfg(feature = "trace-task-lifecycle")]
        log::info!(
            "[sched][wait] resume parent={:?} target={:?}",
            me.pid_root(),
            target
        );
        me.exit_waiters.finish_wait(&entry);
        // 子退出和信号可能同时唤醒等待者。先回到循环顶部消费已可观察的
        // 子状态；只有仍无结果时，下一轮才按信号语义返回 EINTR。
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
    make_siginfo_with_code(sig, 0)
}

fn make_siginfo_with_code(sig: SignalNumber, code: i32) -> SigInfo {
    let me = current_task();
    let sender_pid = me
        .thread_group()
        .leader()
        .and_then(|leader| leader.pid_root())
        .or_else(|| me.pid_root())
        .unwrap_or(0);
    SigInfo {
        sig,
        code,
        sender_pid,
        sender_uid: me.credentials().uid,
        raw: None,
    }
}

fn terminate_thread_group_by_signal(target: &Arc<Task>, info: SigInfo) -> bool {
    let current = current_task();
    terminate_thread_group_by_signal_from(target, info, &current)
}

fn terminate_thread_group_by_signal_from(
    target: &Arc<Task>,
    info: SigInfo,
    current: &Arc<Task>,
) -> bool {
    if target.is_kernel_task() {
        return false;
    }

    terminate_thread_group_identity_by_signal_from(&target.thread_group(), info, current)
}

fn terminate_thread_group_identity_by_signal_from(
    group: &Arc<ThreadGroup>,
    info: SigInfo,
    current: &Arc<Task>,
) -> bool {
    let core_dumped = matches!(default_action(info.sig), DefaultAction::Core);
    // 与 CLONE_THREAD 登记通过 members 锁排序：先登记者会进入本次 snapshot，
    // 后到者被拒绝，避免 SIGKILL 之后仍产生未收到终止请求的新线程。
    let _ = group.request_group_signal(info.sig, core_dumped);
    let members = group.snapshot();
    let mut terminated = false;
    let mut need_handoff = false;

    for member in members.iter() {
        if member.is_kernel_task() {
            continue;
        }
        if matches!(member.state(), TaskState::Zombie | TaskState::Dead) {
            continue;
        }
        // 不在发送者上下文直接 exit 目标任务。目标恢复自己的
        // 阻塞调用栈，再在 syscall/用户返回边界消费权威组状态。
        crate::scheduler::group_exit_wakeup(member);
        if !Arc::ptr_eq(member, current) {
            need_handoff = true;
        }
        terminated = true;
    }

    if need_handoff {
        request_post_syscall_handoff();
    }
    terminated
}

fn deliver_to_thread_group_identity(group: &Arc<ThreadGroup>, info: SigInfo) -> bool {
    if info.sig == SignalNumber::SIGKILL {
        let current = current_task();
        return terminate_thread_group_identity_by_signal_from(group, info, &current);
    }

    deliver_shared_signal_to_group(group, info);
    true
}

fn deliver_to_thread_group(target: &Arc<Task>, info: SigInfo) -> bool {
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[sched][signal] group-deliver-enter target={:?} signal={:?} state={:?}",
        target.pid_root(),
        info.sig,
        target.state(),
    );
    if target.is_kernel_task() {
        return false;
    }
    let delivered = deliver_to_thread_group_identity(&target.thread_group(), info);
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[sched][signal] group-deliver-leave target={:?} signal={:?} delivered={}",
        target.pid_root(),
        info.sig,
        delivered,
    );
    delivered
}

fn check_pidfd_group_permission(group: &Arc<ThreadGroup>) -> Result<(), Errno> {
    if group.is_terminated() {
        return Err(Errno::ESRCH);
    }
    let target = group.leader().ok_or(Errno::ESRCH)?;
    check_kill_permission(&target)
}

/// pidfd 的普通信号入口：直接使用稳定线程组身份，禁止经由可重用的 TGID 回查。
pub fn pidfd_kill(group: &Arc<ThreadGroup>, sig: Option<SignalNumber>) -> Result<(), Errno> {
    check_pidfd_group_permission(group)?;
    let Some(sig) = sig else { return Ok(()) };
    let _ = deliver_to_thread_group_identity(group, make_siginfo(sig));
    Ok(())
}

/// pidfd 的排队信号入口，保留用户提供的 siginfo，同时绑定稳定线程组身份。
pub fn pidfd_queueinfo(group: &Arc<ThreadGroup>, info: SigInfo) -> Result<(), Errno> {
    check_pidfd_group_permission(group)?;
    let _ = deliver_to_thread_group_identity(group, info);
    Ok(())
}

/// `kill(pid, sig)`：按 POSIX pid 语义投递信号。
/// - pid > 0：送到整 thread-group（共享 pending）。
/// - pid == 0：送到调用者同 pgroup 的所有进程。
/// - pid == -1：送到 init 外的所有进程（精简实现：枚举当前 ns 所有 pid）。
/// - pid < -1：送到 pgid==-pid 的所有进程。
#[kernel_symbols::export(name = "sched.operation.kill", contract = "kernel.sched.signal@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
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
        let _ = deliver_to_thread_group(&target, info);
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
            delivered |= deliver_to_thread_group(&t, info);
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
    let info = make_siginfo_with_code(sig, -6);
    if sig == SignalNumber::SIGKILL {
        let _ = terminate_thread_group_by_signal(&target, info);
    } else {
        target.signal.deliver(info);
        signal_wakeup(&target, &info);
    }
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
    let info = make_siginfo_with_code(sig, -6);
    if sig == SignalNumber::SIGKILL {
        let _ = terminate_thread_group_by_signal(&target, info);
    } else {
        target.signal.deliver(info);
        signal_wakeup(&target, &info);
    }
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
    if info.sig == SignalNumber::SIGKILL {
        let _ = terminate_thread_group_by_signal(&target, info);
    } else {
        target.signal.deliver(info);
        signal_wakeup(&target, &info);
    }
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
    let _ = deliver_to_thread_group(&target, info);
    Ok(())
}

// ── ptrace 最小兼容层 ───────────────────────────────────────────────────────

pub fn ptrace_traceme() -> Result<(), Errno> {
    let me = current_task();
    if me.is_kernel_task() || me.parent().is_none() {
        return Err(Errno::EPERM);
    }
    if !me.enable_ptrace_traced() {
        return Err(Errno::EPERM);
    }
    Ok(())
}

pub fn ptrace_attach(pid: PidT) -> Result<(), Errno> {
    let target = lookup_pid(pid)?;
    let me = current_task();
    if target.is_kernel_task() || Arc::ptr_eq(&target, &me) {
        return Err(Errno::EPERM);
    }
    check_kill_permission(&target)?;
    if !target.enable_ptrace_traced() {
        return Err(Errno::EPERM);
    }
    let _ = mark_task_stopped(&target, SignalNumber::SIGSTOP);
    Ok(())
}

pub fn ptrace_seize(pid: PidT) -> Result<(), Errno> {
    let target = lookup_pid(pid)?;
    let me = current_task();
    if target.is_kernel_task() || Arc::ptr_eq(&target, &me) {
        return Err(Errno::EPERM);
    }
    check_kill_permission(&target)?;
    if !target.enable_ptrace_traced() {
        return Err(Errno::EPERM);
    }
    Ok(())
}

pub fn ptrace_interrupt(pid: PidT) -> Result<(), Errno> {
    let target = lookup_pid(pid)?;
    if target.is_kernel_task() || !target.is_ptrace_traced() {
        return Err(Errno::ESRCH);
    }
    check_kill_permission(&target)?;
    let _ = mark_task_stopped(&target, SignalNumber::SIGTRAP);
    Ok(())
}

pub fn ptrace_cont(pid: PidT, sig: Option<SignalNumber>) -> Result<(), Errno> {
    let target = lookup_pid(pid)?;
    if target.is_kernel_task() || !target.is_ptrace_traced() {
        return Err(Errno::ESRCH);
    }
    check_kill_permission(&target)?;
    let _ = continue_task(&target);
    if let Some(sig) = sig {
        tkill(pid, Some(sig))?;
    }
    Ok(())
}

pub fn ptrace_detach(pid: PidT, sig: Option<SignalNumber>) -> Result<(), Errno> {
    let target = lookup_pid(pid)?;
    if target.is_kernel_task() || !target.is_ptrace_traced() {
        return Err(Errno::ESRCH);
    }
    check_kill_permission(&target)?;
    target.clear_ptrace_traced();
    let _ = continue_task(&target);
    if let Some(sig) = sig {
        tkill(pid, Some(sig))?;
    }
    Ok(())
}

pub fn ptrace_kill(pid: PidT) -> Result<(), Errno> {
    let target = lookup_pid(pid)?;
    if target.is_kernel_task() || !target.is_ptrace_traced() {
        return Err(Errno::ESRCH);
    }
    tkill(pid, Some(SignalNumber::SIGKILL))
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
#[kernel_symbols::export(name = "sched.operation.sigpending", contract = "kernel.sched.signal@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
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
#[kernel_symbols::export(
    name = "sched.operation.has_interrupting_signal",
    contract = "kernel.sched.signal@1",
    version = 1,
    capabilities = kernel_symbols::capability::SCHED_QUERY
)]
pub fn has_interrupting_signal(task: &Arc<Task>) -> bool {
    if task.group_exit_pending() {
        return true;
    }
    let group = task.thread_group();
    let Some(consumer) = group.lock_signal_consumer() else {
        return false;
    };
    let shared = task.shared_signal();
    let blocked = task.signal.blocked_snapshot().raw();
    let mut pending =
        (task.signal.pending_snapshot().raw() | shared.pending_snapshot().raw()) & !blocked;
    while let Some(sig) = take_next_pending_signal(&mut pending) {
        let action = shared.get_action(sig);
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
                            .or_else(|| shared.dequeue_one_in(sig.bit()))
                            .unwrap_or_else(|| make_siginfo(sig));
                        drop(consumer);
                        apply_default_action_for_task(task, info);
                    }
                    return true;
                }
            },
        }
    }
    false
}

#[inline]
pub(crate) fn take_next_pending_signal(pending: &mut u64) -> Option<SignalNumber> {
    while *pending != 0 {
        let bit = pending.trailing_zeros();
        *pending &= pending.wrapping_sub(1);
        if let Some(signal) = SignalNumber::from_raw(bit as i32 + 1) {
            return Some(signal);
        }
    }
    None
}

/// syscall 返回 `EINTR` 前尝试消费一条可自动重启的用户 handler 信号。
///
/// `SA_RESTART` 要求 handler 返回后重新执行被打断的 syscall。分发器在写返回值、
/// 推进 PC 之前调用这里，因此原始参数寄存器仍然完整。
pub fn consume_restartable_signal() -> Option<(SigInfo, SigAction)> {
    let me = current_task();
    let group = me.thread_group();
    let _consumer = group.lock_signal_consumer()?;
    let shared = me.shared_signal();
    let blocked = me.signal.blocked_snapshot().raw();
    let mut pending =
        (me.signal.pending_snapshot().raw() | shared.pending_snapshot().raw()) & !blocked;

    while let Some(sig) = take_next_pending_signal(&mut pending) {
        let action = shared.get_action(sig);
        if !matches!(action.handler, SigHandler::Handler(_)) {
            continue;
        }
        if !action.flags.has(SigActionFlags::SA_RESTART) {
            continue;
        }

        let info = me
            .signal
            .dequeue_one_in(sig.bit())
            .or_else(|| shared.dequeue_one_in(sig.bit()))?;
        return Some((info, action));
    }

    None
}

/// `sigtimedwait(these)` 在不阻塞的情况下尝试消费一条属于 `these` 的信号。
/// 命中即返回 Some(SigInfo)，无命中返回 None（不进入等待）。
///
/// 不匹配 `these` 的 pending 会保留在原队列中，等待常规投递或其它 sigwait。
pub fn sigtimedwait_poll(these: SigSet) -> Option<SigInfo> {
    let me = current_task();
    me.dequeue_pending_signal_in(these)
}

fn sigtimedwait_pending(these: SigSet) -> bool {
    let me = current_task();
    me.has_pending_signal_in(these)
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
        if me.group_exit_pending() {
            me.signal.end_sigtimedwait();
            return false;
        }
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
#[kernel_symbols::export(name = "sched.operation.get_rlimit", contract = "kernel.sched.rlimit@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn get_rlimit(resource: Resource) -> Result<RlimitPair, Errno> {
    let me = current_task();
    let pair = me.thread_group().rlimits().lock().get(resource);
    Ok(pair)
}

/// `setrlimit(resource, new)` 写调用者 tg 的 rlimit。
///
/// 校验规则照搬 Linux 6.x `kernel/sys.c::do_prlimit`：
///   1. `new.soft ≤ new.hard`（基础不变量）
///   2. `new.hard ≤ cur.hard`（无 CAP_SYS_RESOURCE 时硬限制不可逆降不到
///      "原值以下"——但硬限制是允许降到任何更小的值，包括小于当前 soft）
///   3. `new.soft ≤ cur.hard`（软限制不能超过当前硬限制）
///
/// 关键点：**不检查** `new.hard < cur.soft`。POSIX 允许"硬限制降到
/// ≥ 0 的任意值"，glibc 旧 ABI setrlimit 也支持；libctest 的
/// `setrlim.c:21` 典型用法 `setrlimit(RLIMIT_STACK, 102400)` 会把
/// `rlim_max = 102400`，当前 soft 可能是 8MB，老式"hard 不能降到
/// 软以下"校验会误返 EINVAL。
#[kernel_symbols::export(name = "sched.operation.set_rlimit", contract = "kernel.sched.rlimit@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn set_rlimit(resource: Resource, new: RlimitPair) -> Result<RlimitPair, Errno> {
    let me = current_task();
    let tg = me.thread_group();
    let mut guard = tg.rlimits().lock();
    let cur = guard.get(resource);

    // (1) 软限制必须 ≤ 硬限制。
    if new.soft.0 > new.hard.0 {
        return Err(rlimit_err_to_errno(RlimitError::ExceedsHard));
    }
    // (2) 无 CAP 时硬限制不可调高（只能降或保留）。
    if new.hard.0 > cur.hard.0 {
        return Err(rlimit_err_to_errno(RlimitError::ExceedsHard));
    }
    // (3) 软限制不能超当前硬限制。
    if new.soft.0 > cur.hard.0 {
        return Err(rlimit_err_to_errno(RlimitError::ExceedsHard));
    }
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
    let target_tg = if pid == 0 {
        me.thread_group()
    } else if pid > 0 {
        let root = root_pid_ns();
        let Some(weak) = root.registry().lookup(pid) else {
            return Err(Errno::ESRCH);
        };
        let Some(task) = weak.upgrade() else {
            return Err(Errno::ESRCH);
        };
        task.thread_group()
    } else {
        return Err(Errno::EINVAL);
    };
    // 权限：调用方需是同 uid 或具 CAP_SYS_RESOURCE。本仓库尚未实现
    // capability 模型，按"同进程/同 uid 直通"处理。
    if let Some(n) = new {
        let mut guard = target_tg.rlimits().lock();
        let cur = guard.get(resource);
        if n.soft.0 > n.hard.0 {
            return Err(rlimit_err_to_errno(RlimitError::ExceedsHard));
        }
        if n.hard.0 > cur.hard.0 {
            return Err(rlimit_err_to_errno(RlimitError::ExceedsHard));
        }
        if n.soft.0 > cur.hard.0 {
            return Err(rlimit_err_to_errno(RlimitError::ExceedsHard));
        }
        let old = cur;
        guard.set(resource, n);
        Ok(old)
    } else {
        Ok(target_tg.rlimits().lock().get(resource))
    }
}

/// 返回整个 rlimit 表（用于调试/procfs）。
#[kernel_symbols::export(name = "sched.operation.rlimits_snapshot", contract = "kernel.sched.rlimit@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn rlimits_snapshot() -> Rlimits {
    let me = current_task();
    *me.thread_group().rlimits().lock()
}

// ── 信号投递在内核边界的默认动作处理 ─────────────────────────────────────────

/// Native 用户返回边界消费外部进程控制后的调度决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeExternalControl {
    /// 可以继续完成当前 Native call 或返回用户态。
    Continue,
    /// 任务已停止或刚被继续，需要先经过一次调度边界。
    Reschedule,
    /// 任务已经进入线程组退出，禁止返回用户态。
    Terminate,
}

/// 在 Native 安全边界消费一条外部进程控制事件。
///
/// Native 不暴露 Unix signal frame。显式忽略仍然保留；默认动作和异常残留的
/// Linux handler 都按内核默认动作执行，确保不会把 Linux 用户上下文带入 Native。
pub fn consume_native_external_control_for_task(task: &Arc<Task>) -> NativeExternalControl {
    if complete_group_exit_if_requested(task) {
        return NativeExternalControl::Terminate;
    }
    if complete_native_thread_exit_if_requested(task) {
        return NativeExternalControl::Terminate;
    }

    let _ = task.consume_pending_signal(|info| {
        if task.is_ptrace_traced() && info.sig != SignalNumber::SIGKILL {
            let _ = mark_task_stopped(task, info.sig);
            return;
        }
        let action = task.shared_signal().get_action(info.sig);
        match action.handler {
            SigHandler::Ignore => {}
            SigHandler::Default | SigHandler::Handler(_) => {
                apply_default_action_for_task(task, info);
            }
        }
    });

    if complete_group_exit_if_requested(task) || complete_native_thread_exit_if_requested(task) {
        return NativeExternalControl::Terminate;
    }
    match task.state() {
        TaskState::Zombie | TaskState::Dead => NativeExternalControl::Terminate,
        TaskState::Stopped | TaskState::Continued => NativeExternalControl::Reschedule,
        _ => NativeExternalControl::Continue,
    }
}

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
    deliver_pending_signals_for_task(&me, user_ctx)
}

pub fn deliver_pending_signals_for_task(
    me: &Arc<Task>,
    user_ctx: UserContextRef,
) -> Option<SigInfo> {
    if me.is_kernel_task() {
        return None;
    }
    if me.user_abi_kind() == crate::UserAbiKind::MygoNative {
        let _ = consume_native_external_control_for_task(me);
        return None;
    }
    me.consume_pending_signal(|info| {
        if me.is_ptrace_traced() && info.sig != SignalNumber::SIGKILL {
            let _ = mark_task_stopped(me, info.sig);
            return None;
        }
        let action = me.shared_signal().get_action(info.sig);
        use crate::signal::SigHandler;
        match action.handler {
            SigHandler::Default => {
                apply_default_action_for_task(me, info);
                None
            }
            SigHandler::Ignore => None,
            SigHandler::Handler(_) => {
                if process_image_ops().is_none() {
                    me.signal.deliver(info);
                    return Some(info);
                }
                match setup_user_signal_frame_for_task(me, info, action, user_ctx) {
                    Ok(()) => None,
                    Err(Errno::ENOSYS) => {
                        me.signal.deliver(info);
                        Some(info)
                    }
                    Err(_) => None,
                }
            }
        }
    })
    .flatten()
}

/// 在保存 signal frame 前先应用 rseq 的 SIGNAL 事件。
pub fn setup_user_signal_frame_for_task(
    task: &Arc<Task>,
    info: SigInfo,
    action: crate::signal::SigAction,
    user_ctx: UserContextRef,
) -> Result<(), Errno> {
    if user_ctx.is_none() {
        return Err(Errno::ENOSYS);
    }
    let ops = process_image_ops().ok_or(Errno::ENOSYS)?;
    task.mark_rseq_event(RseqEvent::Signal);
    if (ops.prepare_user_return)(task, user_ctx).is_err() {
        task.clear_rseq_registration();
        apply_default_action_for_task(
            task,
            SigInfo {
                sig: SignalNumber::SIGSEGV,
                code: 0,
                sender_pid: 0,
                sender_uid: crate::ids::Uid::ROOT,
                raw: None,
            },
        );
        return Err(Errno::EFAULT);
    }
    match (ops.setup_signal_frame)(task, info, action, user_ctx) {
        Ok(()) => Ok(()),
        Err(Errno::ENOSYS) => Err(Errno::ENOSYS),
        Err(error) => {
            apply_default_action_for_task(
                task,
                SigInfo {
                    sig: SignalNumber::SIGSEGV,
                    code: 0,
                    sender_pid: 0,
                    sender_uid: crate::ids::Uid::ROOT,
                    raw: None,
                },
            );
            Err(error)
        }
    }
}

/// 在即将恢复用户态时处理依赖当前 trap frame 的线程状态。
#[inline]
pub fn prepare_user_return_for_task(
    task: &Arc<Task>,
    user_ctx: UserContextRef,
) -> Result<(), Errno> {
    if task.is_kernel_task() || user_ctx.is_none() {
        return Ok(());
    }
    if !task.exec_sibling_exit_boundary_pending()
        && !task.group_exit_boundary_pending()
        && task.native_thread_exit_boundary_pending().is_none()
        && task.rseq_events().is_empty()
    {
        return Ok(());
    }
    prepare_user_return_slow(task, user_ctx)
}

#[cold]
#[inline(never)]
fn prepare_user_return_slow(task: &Arc<Task>, user_ctx: UserContextRef) -> Result<(), Errno> {
    if complete_exec_sibling_exit_if_requested(task)
        || complete_group_exit_if_requested(task)
        || complete_native_thread_exit_if_requested(task)
    {
        return Ok(());
    }
    let Some(ops) = process_image_ops() else {
        return Ok(());
    };
    let result = (ops.prepare_user_return)(task, user_ctx);
    if result.is_err() {
        task.clear_rseq_registration();
        apply_default_action_for_task(
            task,
            SigInfo {
                sig: SignalNumber::SIGSEGV,
                code: 0,
                sender_pid: 0,
                sender_uid: crate::ids::Uid::ROOT,
                raw: None,
            },
        );
    }
    result
}

pub fn apply_default_action(info: SigInfo) {
    let task = current_task();
    apply_default_action_for_task(&task, info);
}

pub(crate) fn apply_default_action_for_task(task: &Arc<Task>, info: SigInfo) {
    match default_action(info.sig) {
        DefaultAction::Term | DefaultAction::Core => {
            let _ = terminate_thread_group_by_signal_from(task, info, task);
            let _ = complete_group_exit_if_requested(task);
        }
        DefaultAction::Stop => {
            let _ = mark_task_stopped(task, info.sig);
        }
        DefaultAction::Cont => {
            let _ = continue_task(task);
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

#[cfg(test)]
mod pidfd_signal_tests {
    use alloc::sync::Weak;

    use super::*;

    #[test]
    fn process_signal_delivery_uses_stable_thread_group_identity() {
        let group = crate::ThreadGroup::new();
        let info = SigInfo {
            sig: SignalNumber::SIGUSR1,
            code: 0,
            sender_pid: 7,
            sender_uid: Uid(0),
            raw: None,
        };

        assert!(deliver_to_thread_group_identity(&group, info));
        assert!(
            group
                .shared_signal()
                .pending_snapshot()
                .has(SignalNumber::SIGUSR1)
        );
    }

    #[test]
    fn dropping_prepared_clone_rolls_back_unactivated_child() {
        let session = crate::Session::new();
        let process_group = crate::ProcessGroup::new(&session);
        session.register_group(&process_group);
        let parent = Task::new(
            SchedParams::default_fair(),
            Weak::new(),
            crate::ThreadGroup::new(),
            Arc::clone(&process_group),
        );
        let child_group = crate::ThreadGroup::new();
        let child = Task::new(
            SchedParams::default_fair(),
            Arc::downgrade(&parent),
            Arc::clone(&child_group),
            Arc::clone(&process_group),
        );
        child_group.set_leader(&child);
        child_group.add_member(&child);
        process_group.add_member(&child);
        parent.add_child(Arc::clone(&child));

        drop(PreparedClone::from_parts(
            Arc::clone(&parent),
            Arc::clone(&child),
            7,
            CloneArgs::fork_default(),
        ));

        assert_eq!(child.state(), TaskState::Dead);
        assert!(parent.snapshot_children().is_empty());
        assert!(child_group.snapshot().is_empty());
    }
}
