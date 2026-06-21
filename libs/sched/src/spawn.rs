//! 任务派生 / 退出 / 回收，以及内核线程入口。
//!
//! 本模块覆盖 fork / clone / exit / wait 的"图操作"——更新 Task / Group /
//! Runqueue 的关系网，并通过注册的 `TaskExtCloneHook` 让上层（VFS 等）参与
//! fork 决策。具体的 syscall 接入由 [`crate::operation`] 调用本模块的函数。

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch_hooks::KernelEntry;
use crate::clone_flags::{CloneArgs, CloneFlags};
use crate::eevdf::SchedParams;
use crate::group::{ProcessGroup, Session, ThreadGroup};
use crate::sched_class::{SchedAttr, SchedPolicy};
use crate::scheduler::{
    current_task, enqueue_task, init_task, is_current_on_any_cpu, mark_task_exited, now_ns_public,
    root_pid_ns, schedule_once,
};
use crate::signal::SignalNumber;
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

// ── 简单 spawn（不带 CloneFlags） ────────────────────────────────────────────

/// 从 `parent` 派生一个新任务：分配 pid、登记亲缘 / 组关系，但不入 runqueue。
///
/// 调用方必须先安装执行上下文，再调用 [`activate_task`]。这样不会把半初始化
/// 任务暴露给调度器。
pub fn spawn_child(parent: &Arc<Task>, kind: SpawnKind, params: SchedParams) -> Arc<Task> {
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

    if matches!(kind, SpawnKind::Process) {
        tgroup.set_leader(&child);
    }
    tgroup.add_member(&child);
    pgroup.add_member(&child);

    parent.add_child(Arc::clone(&child));

    let Some(pid) = root_ns.registry().allocate(&child) else {
        log::warning!(
            "[sched][spawn] pid allocation failed kind={:?} parent_pid={:?}",
            kind,
            parent.pid_root(),
        );
        abort_new_task(&child);
        return child;
    };
    child.register_pid(Arc::clone(&root_ns), pid);
    if matches!(kind, SpawnKind::Process) {
        tgroup.set_tgid(pid);
    }
    if pgroup.pgid() <= 0 {
        pgroup.set_pgid(pid);
    }

    #[cfg(feature = "trace-task-lifecycle")]
    log::debug!(
        "[sched][spawn] kind={:?} pid={} parent_pid={:?}",
        kind,
        pid,
        parent.pid_root(),
    );
    child
}

/// 把已经安装执行上下文的任务放入合适的 runqueue。
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
    Ok(enqueue_task(Arc::clone(task), now_ns_public()))
}

/// 回滚尚未运行、尚未入队的新任务。用于 clone/exec 安装用户上下文失败的路径。
pub fn abort_new_task(task: &Arc<Task>) {
    if let Some(parent) = task.parent() {
        let _ = parent.remove_child(task);
    }
    for (ns, pid) in task.pid_namespaces_snapshot() {
        ns.registry().release(pid);
    }
    task.thread_group().remove_member(task);
    task.process_group().remove_member(task);
    task.set_state(TaskState::Dead);
}

// ── 完整 clone：处理 CLONE_* flags、ext hook、vfork ──────────────────────────

/// POSIX clone(2)：根据 `args.flags` 决定 ThreadGroup / SharedSignal / 父
/// 选择 / vfork 阻塞，并通过注册的 [`crate::task::TaskExtCloneHook`]
/// 让上层处理 VFS / FdTable 的拷贝。
pub fn clone_task(parent: &Arc<Task>, args: CloneArgs, params: SchedParams) -> Arc<Task> {
    let flags = args.flags;
    let root_ns = root_pid_ns();
    let parent_tg = parent.thread_group();

    // 1. 决定 thread group：CLONE_THREAD 共享，否则新建。
    let new_tg = if flags.has(CloneFlags::CLONE_THREAD) {
        parent_tg
    } else {
        // CLONE_SIGHAND 共享 SharedSignal（即便不 CLONE_THREAD）。
        let tg = if flags.has(CloneFlags::CLONE_SIGHAND) {
            ThreadGroup::new_sharing_signal(Arc::clone(parent_tg.shared_signal()))
        } else {
            // 不共享 → 深拷一份 sigaction。
            let copied = parent_tg.shared_signal().fork_copy();
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
    let real_parent = if flags.has(CloneFlags::CLONE_PARENT) {
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
        let parent_attr = parent.sched.sched_attr();
        let child_attr = match parent_attr.policy {
            SchedPolicy::Fair | SchedPolicy::Idle => SchedAttr::fair(parent_attr.nice.max(0), 0),
            SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin | SchedPolicy::Deadline => {
                SchedAttr::fair(0, 0)
            }
        };
        child.sched.set_sched_attr(child_attr);
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
    new_tg.add_member(&child);
    pg.add_member(&child);

    // 8. 父登记（亲缘图保活）。CLONE_THREAD 线程不进入普通 child/wait 模型。
    if !flags.has(CloneFlags::CLONE_THREAD) {
        real_parent.add_child(Arc::clone(&child));
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
        let Some(pid) = root_ns.registry().allocate(&child) else {
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
    child.register_pid(Arc::clone(&root_ns), pid);
    if !flags.has(CloneFlags::CLONE_THREAD) {
        new_tg.set_tgid(pid);
    }
    if pg.pgid() <= 0 {
        pg.set_pgid(pid);
    }

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

/// 标记任务退出：出 runqueue、置 Zombie、唤醒 `exit_waiters`，把退出信号
/// 投递给父，唤醒 vfork_done。**不**释放 pid 槽——zombie 期间父按 pid 仍能查到。
///
/// 不切换 CPU；调用方决定何时调 [`schedule_once`]。
pub fn exit_task(task: &Arc<Task>, code: ExitCode) {
    if task.is_idle_task() {
        log::error!(
            "[sched][exit] refusing to exit idle task pid={:?}",
            task.pid_root(),
        );
        return;
    }

    task.cleanup_before_exit();

    // 1) 先把自己的子任务托管给 init，让它们在父死后仍有 reaper。
    //    init 任务本身退出（正常情况下不会发生）时跳过，避免自引用成环。
    let children = task.snapshot_children();
    if !children.is_empty() {
        let init = init_task();
        if !Arc::ptr_eq(&init, task) {
            task.reparent_children_to(&init);
            // 已经是 Zombie 的子需要让 init 感知——否则它们的 SIGCHLD 之前投给
            // 了旧父，init 不会来 reap。这里只对刚被过继的 Zombie 重投一次。
            for c in children.iter() {
                if c.state() == TaskState::Zombie {
                    if let Some(sig) = SignalNumber::from_raw(SignalNumber::SIGCHLD.raw() as i32) {
                        let info = crate::signal::SigInfo {
                            sig,
                            code: 1, // CLD_EXITED
                            sender_pid: c.pid_root().unwrap_or(0),
                            sender_uid: crate::ids::Uid::ROOT,
                            raw: None,
                        };
                        init.thread_group().shared_signal().deliver(info);
                        crate::scheduler::signal_wakeup(&init, &info);
                    }
                }
            }
        }
    }

    mark_task_exited(task, code);
    if !is_current_on_any_cpu(task) {
        task.cleanup_exit_extensions();
    }

    // 唤醒 vfork 父。
    if task.is_vforking() {
        task.set_vforking(false);
        task.vfork_done.wake_all();
    }

    // 给父投递 exit_signal。
    let exit_sig = task.exit_signal();
    if exit_sig == 0 {
        for (ns, pid) in task.pid_namespaces_snapshot() {
            ns.registry().release(pid);
        }
        task.thread_group().remove_member(task);
        task.process_group().remove_member(task);
        task.set_state(TaskState::Dead);
        return;
    }
    if exit_sig > 0 {
        if let Some(parent) = task.parent() {
            if let Some(sig) = SignalNumber::from_raw(exit_sig) {
                let info = crate::signal::SigInfo {
                    sig,
                    code: 1, // CLD_EXITED
                    sender_pid: task.pid_root().unwrap_or(0),
                    sender_uid: crate::ids::Uid::ROOT,
                    raw: None,
                };
                // 共享投递（thread-group level）：parent 任意线程能收到。
                parent.thread_group().shared_signal().deliver(info);
                crate::scheduler::signal_wakeup(&parent, &info);
            }
        }
    }

    // 唤醒 parent 的 exit_waiters（wait4 / waitid 阻塞者）。
    if let Some(parent) = task.parent() {
        parent.exit_waiters.wake_all();
    }
}

/// 父侧 reap：从 children 取走首个 Zombie 子，释放其 pid，返回 (任务, 退出码)。
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

    // 进程已经不再运行；在父进程 wait 上下文释放 VM/FDT/VFS 等重量级资源。
    // wait 状态和 procfs 需要的轻量字段仍保留在 Task 本体中。
    zombie.cleanup_exit_extensions();
    zombie.retire_execution();

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

/// 列出 `parent` 当前所有 Zombie 子。便于 wait4(WNOHANG) 等不阻塞探查。
pub fn list_zombie_children(parent: &Arc<Task>) -> Vec<Arc<Task>> {
    parent
        .snapshot_children()
        .into_iter()
        .filter(|c| c.is_user_task() && c.state() == TaskState::Zombie)
        .collect()
}

// ── 内核线程 ──────────────────────────────────────────────────────────────────

/// 从 init 派生一个内核线程。线程入口签名：
/// `unsafe extern "C" fn(arg: usize) -> !`，内部以 [`kthread_finish`] 退出。
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

    let Some(pid) = root_ns.registry().allocate(&child) else {
        log::warning!("[sched][kthread] pid allocation failed");
        child.set_state(TaskState::Dead);
        return child;
    };
    child.register_pid(Arc::clone(&root_ns), pid);
    tgroup.set_leader(&child);
    tgroup.set_tgid(pid);
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
