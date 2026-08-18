//! 进程与 libc 初始化相关 syscall。

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use errno::Errno;
use general::firmware::power;
use general::mm::{VmFutexKey, VmSpace, copy_cstr_from_user, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::nsfs::ProcNsKind;
use general::vfs::pidfd;
use general::vfs::{self, fdtable::Fd};
use ns::UtsNamespace;
use sched::clone_flags::{CloneArgs, CloneFlags};
use sched::ids::{CapSet, Capability, Credentials, Gid, Uid};
use sched::pid::PidT;
use sched::process_ops::{ExecRequest, UserContextRef};
use sched::sync::Spinlock;
use sched::task::{RseqRegistration, Task, TaskState};
use sched::{SchedAttr, SchedPolicy, SignalNumber, WaitId, WaitOptions, WaitStatus};

use super::vfs_cred_from_sched;

// getpriority/setpriority 的 Linux 兼容层编码。调度核心只接收 Task 和 nice，
// 不理解 which/who 这组用户态选择语义。
const PRIO_PROCESS: usize = 0;
const PRIO_PGRP: usize = 1;
const PRIO_USER: usize = 2;
const MIN_NICE: i32 = -20;
const MAX_NICE: i32 = 19;

const LINUX_REBOOT_MAGIC1: u32 = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2: u32 = 672_274_793;
const LINUX_REBOOT_MAGIC2A: u32 = 85_072_278;
const LINUX_REBOOT_MAGIC2B: u32 = 369_367_448;
const LINUX_REBOOT_MAGIC2C: u32 = 537_993_216;

const LINUX_REBOOT_CMD_RESTART: u32 = 0x0123_4567;
const LINUX_REBOOT_CMD_HALT: u32 = 0xcdef_0123;
const LINUX_REBOOT_CMD_CAD_ON: u32 = 0x89ab_cdef;
const LINUX_REBOOT_CMD_CAD_OFF: u32 = 0x0000_0000;
const LINUX_REBOOT_CMD_POWER_OFF: u32 = 0x4321_fedc;
const LINUX_REBOOT_CMD_RESTART2: u32 = 0xa1b2_c3d4;
const LINUX_REBOOT_CMD_SW_SUSPEND: u32 = 0xd000_fce2;
const LINUX_REBOOT_CMD_KEXEC: u32 = 0x4558_4543;
const UTS_FIELD_LEN: usize = 65;
const UTS_NAME_MAX: usize = UTS_FIELD_LEN - 1;
const EXEC_PATH_MAX: usize = 4096;

#[cfg(target_arch = "riscv64")]
const SYS_RISCV_FLUSH_ICACHE_LOCAL: usize = 1;

const PTRACE_TRACEME: usize = 0;
const PTRACE_PEEKTEXT: usize = 1;
const PTRACE_PEEKDATA: usize = 2;
const PTRACE_PEEKUSR: usize = 3;
const PTRACE_POKETEXT: usize = 4;
const PTRACE_POKEDATA: usize = 5;
const PTRACE_POKEUSR: usize = 6;
const PTRACE_CONT: usize = 7;
const PTRACE_KILL: usize = 8;
const PTRACE_SINGLESTEP: usize = 9;
const PTRACE_ATTACH: usize = 16;
const PTRACE_DETACH: usize = 17;
const PTRACE_OLDSETOPTIONS: usize = 21;
const PTRACE_SYSCALL: usize = 24;
const PTRACE_GETFDPIC: usize = 33;
const PTRACE_SETOPTIONS: usize = 0x4200;
const PTRACE_GETEVENTMSG: usize = 0x4201;
const PTRACE_GETSIGINFO: usize = 0x4202;
const PTRACE_SETSIGINFO: usize = 0x4203;
const PTRACE_GETREGSET: usize = 0x4204;
const PTRACE_SETREGSET: usize = 0x4205;
const PTRACE_SEIZE: usize = 0x4206;
const PTRACE_INTERRUPT: usize = 0x4207;
const PTRACE_LISTEN: usize = 0x4208;
const PTRACE_PEEKSIGINFO: usize = 0x4209;
const PTRACE_GETSIGMASK: usize = 0x420a;
const PTRACE_SETSIGMASK: usize = 0x420b;
const PTRACE_SECCOMP_GET_FILTER: usize = 0x420c;
const PTRACE_SECCOMP_GET_METADATA: usize = 0x420d;
const PTRACE_GET_SYSCALL_INFO: usize = 0x420e;
const PTRACE_GET_RSEQ_CONFIGURATION: usize = 0x420f;
const PTRACE_SET_SYSCALL_USER_DISPATCH_CONFIG: usize = 0x4210;
const PTRACE_GET_SYSCALL_USER_DISPATCH_CONFIG: usize = 0x4211;
const PTRACE_SET_SYSCALL_INFO: usize = 0x4212;

static UTS_HOSTNAME: Spinlock<[u8; UTS_FIELD_LEN]> = Spinlock::new([0u8; UTS_FIELD_LEN]);
static UTS_DOMAINNAME: Spinlock<[u8; UTS_FIELD_LEN]> = Spinlock::new([0u8; UTS_FIELD_LEN]);

#[cfg(target_arch = "riscv64")]
fn validate_riscv_flush_icache_flags(flags: usize) -> Result<(), Errno> {
    if flags & !SYS_RISCV_FLUSH_ICACHE_LOCAL != 0 {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sys_riscv_flush_icache(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    validate_riscv_flush_icache_flags(ctx.args[2])?;

    // Linux 当前保留并忽略 start/end，因为 RISC-V 不支持按范围失效 I-cache。
    let _start = ctx.args[0];
    let _end = ctx.args[1];

    // TODO(riscv64): 为 LOCAL=1 增加仅本地执行 fence.i 的路径。当前全局同步
    // 在语义上正确，但会产生不必要的远端 hart RFENCE 开销。
    <arch::CurrentTaskOps as general::TaskOps>::sync_icache();
    Ok(0)
}

pub(super) fn sys_getpid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let task = ctx.task();
    // Linux：getpid 返回调用者 pid 命名空间中的 pid。任务的权威 ns 是
    // sched 侧 `task.pid_ns()`（spawn 时由 register_pid_chain 按
    // pending/父 ns 设置）；NsProxy.pid 只反映 fork 时的快照，unshare
    // (CLONE_NEWPID) 后子进程会得到新 ns，必须用任务自身 ns 查询。
    let ns = task.pid_ns();
    if let Some(pid) = task.pid_in(&ns) {
        return Ok(pid as usize);
    }
    Ok(task
        .tgid_cached()
        .or_else(|| task.pid_root_cached())
        .or_else(|| {
            let tgid = task.thread_group().tgid();
            if tgid > 0 {
                Some(tgid)
            } else {
                task.pid_root()
            }
        })
        .unwrap_or(0) as usize)
}

pub(super) fn sys_gettid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx
        .task()
        .pid_root_cached()
        .or_else(|| ctx.task().pid_root())
        .unwrap_or(0) as usize)
}

pub(super) fn sys_getppid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx
        .task()
        .parent()
        .and_then(|p| {
            p.tgid_cached().or_else(|| {
                let tgid = p.thread_group().tgid();
                if tgid > 0 { Some(tgid) } else { p.pid_root() }
            })
        })
        .unwrap_or(0) as usize)
}

pub(super) fn sys_getuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task().credentials().uid.0 as usize)
}

pub(super) fn sys_geteuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task().credentials().euid.0 as usize)
}

pub(super) fn sys_getgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task().credentials().gid.0 as usize)
}

pub(super) fn sys_getegid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task().credentials().egid.0 as usize)
}

pub(super) fn sys_exit(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let code = ctx.args[0] as i32;
    let task = Arc::clone(ctx.task());
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!("[syscall][exit] pid={:?} code={}", task.pid_root(), code);
    // Safety: sched::operation::exit 不返回，不会再访问本 syscall context。
    unsafe { ctx.release_task_ref() };
    drop(task);
    sched::operation::exit(code);
}

pub(super) fn sys_exit_group(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let code = ctx.args[0] as i32;
    let task = Arc::clone(ctx.task());
    #[cfg(any(feature = "trace-task-lifecycle", feature = "trace-signal-wait"))]
    log::info!(
        "[syscall][exit_group] pid={:?} code={}",
        task.pid_root(),
        code
    );
    // Safety: sched::operation::exit_group 不返回，不会再访问本 syscall context。
    unsafe { ctx.release_task_ref() };
    drop(task);
    sched::operation::exit_group(code);
}

fn release_exit_files(task: &Arc<Task>) {
    // A zombie must not keep pipe/socket/file endpoints alive until its parent
    // reaps it.  Shell pipelines rely on writer fd release at process exit to
    // deliver EOF to readers such as wc or command substitution.
    if let Some(payload) = task.ext_lookup(sched::TASKEXT_VFS_FDTABLE)
        && let Ok(fdt) = Arc::downcast::<vfs::fdtable::FdTable>(payload)
    {
        let owner_pid = task
            .thread_group()
            .leader()
            .and_then(|leader| leader.pid_root())
            .or_else(|| task.pid_root())
            .unwrap_or(0);
        let shared = fdtable_has_other_live_owner(task, &fdt);
        #[cfg(feature = "trace-task-lifecycle")]
        log::info!(
            "[syscall][exit-cleanup] files pid={:?} fds={} shared={}",
            task.pid_root(),
            fdt.len(),
            shared,
        );
        if shared {
            fdt.release_all_record_locks_for_owner(owner_pid);
        } else {
            fdt.close_all_for_owner(owner_pid);
        }
        #[cfg(feature = "trace-task-lifecycle")]
        log::info!(
            "[syscall][exit-cleanup] files-done pid={:?}",
            task.pid_root(),
        );
    }
    let _ = task.ext_remove(sched::TASKEXT_VFS_FDTABLE);
}

pub(crate) fn fdtable_has_other_live_owner(
    task: &Arc<Task>,
    fdt: &Arc<vfs::fdtable::FdTable>,
) -> bool {
    try_fdtable_has_other_live_owner(task, fdt).unwrap_or(true)
}

pub(crate) fn try_fdtable_has_other_live_owner(
    task: &Arc<Task>,
    fdt: &Arc<vfs::fdtable::FdTable>,
) -> Result<bool, Errno> {
    let tasks = sched::operation::try_all_tasks_snapshot().map_err(|_| Errno::ENOMEM)?;
    Ok(fdtable_has_other_live_owner_in(task, fdt, tasks.iter()))
}

pub(crate) fn fdtable_has_other_live_owner_in<'a>(
    task: &Arc<Task>,
    fdt: &Arc<vfs::fdtable::FdTable>,
    tasks: impl IntoIterator<Item = &'a Arc<Task>>,
) -> bool {
    for other in tasks {
        if Arc::ptr_eq(&other, task) || other.is_kernel_task() {
            continue;
        }
        if matches!(other.state(), TaskState::Zombie | TaskState::Dead) {
            continue;
        }
        let Some(payload) = other.ext_lookup(sched::TASKEXT_VFS_FDTABLE) else {
            continue;
        };
        let Ok(other_fdt) = Arc::downcast::<vfs::fdtable::FdTable>(payload) else {
            continue;
        };
        if Arc::ptr_eq(&other_fdt, fdt) {
            return true;
        }
    }
    false
}

pub(super) fn sys_clone(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let regs = hal::user::decode_clone_register_args(ctx.args);
    let mut raw_flags = regs.flags;
    if CloneFlags::from_raw(raw_flags).has(CloneFlags::CLONE_THREAD) {
        // 传统 clone ABI 把退出信号编码在 flags 低位；Linux 对线程克隆忽略该值。
        // clone3 的 exit_signal 是独立字段，仍由通用校验严格要求为零。
        raw_flags &= !CloneFlags::CSIGNAL;
    }
    let flags = CloneFlags::from_raw(raw_flags);
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][clone] flags={:#x} stack={:#x} parent_tid={:#x} child_tid={:#x} tls={:#x}",
        regs.flags,
        regs.stack,
        regs.parent_tid,
        regs.child_tid,
        regs.tls,
    );
    let args = CloneArgs {
        flags,
        pidfd: if flags.has(CloneFlags::CLONE_PIDFD) {
            regs.parent_tid
        } else {
            0
        },
        stack: regs.stack,
        stack_size: 0,
        parent_tid: regs.parent_tid,
        child_tid: regs.child_tid,
        tls: regs.tls,
        exit_signal: raw_flags & CloneFlags::CSIGNAL,
        set_tid: 0,
        set_tid_size: 0,
        requested_pid: 0,
        cgroup: 0,
    };
    if flags.has(CloneFlags::CLONE_PIDFD) && flags.has(CloneFlags::CLONE_PARENT_SETTID) {
        return Err(Errno::EINVAL);
    }
    let prepared =
        sched::operation::prepare_clone_with_context(args, UserContextRef::new(ctx.tf.as_usize()))?;
    let installed_pidfd = install_clone_pidfd(args, prepared.child())?;
    let outcome = prepared.activate()?;
    if let Some(installed) = installed_pidfd {
        installed.commit();
    }
    ptrace_notify_fork(ctx.task(), flags, outcome.pid);
    clone_install_namespaces(ctx.task(), &outcome.child, flags)?;
    Ok(outcome.pid as usize)
}

pub(super) fn sys_clone3(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let user = ctx.args[0];
    let size = ctx.args[1];
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    if size < LINUX_CLONE_ARGS_MIN_SIZE {
        return Err(Errno::EINVAL);
    }
    if size > LINUX_CLONE_ARGS_MAX_SIZE {
        return Err(Errno::E2BIG);
    }
    let mut raw = [0u8; LINUX_CLONE_ARGS_SIZE];
    let n = size.min(raw.len());
    copy_from_user(user, &mut raw[..n]).map_err(|e| e.as_errno())?;
    validate_clone3_tail(user, size)?;
    let read_u64 = |idx: usize| -> u64 {
        let start = idx * 8;
        u64::from_le_bytes(raw[start..start + 8].try_into().unwrap())
    };
    let mut args = CloneArgs {
        flags: CloneFlags::from_raw(read_u64(0)),
        pidfd: read_u64(1) as usize,
        child_tid: read_u64(2) as usize,
        parent_tid: read_u64(3) as usize,
        exit_signal: read_u64(4),
        stack: read_u64(5) as usize,
        stack_size: read_u64(6) as usize,
        tls: read_u64(7) as usize,
        set_tid: read_u64(8) as usize,
        set_tid_size: read_u64(9) as usize,
        requested_pid: 0,
        cgroup: read_u64(10) as usize,
    };
    if (args.stack == 0) != (args.stack_size == 0) {
        return Err(Errno::EINVAL);
    }
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][clone3] flags={:#x} stack={:#x} stack_size={:#x} parent_tid={:#x} child_tid={:#x} tls={:#x}",
        args.flags.raw(),
        args.stack,
        args.stack_size,
        args.parent_tid,
        args.child_tid,
        args.tls,
    );
    prepare_clone3_set_tid(&mut args, ctx.task())?;
    if args.cgroup != 0 {
        // TODO(threading): cgroup 需要成员状态与迁移事务；当前阶段不把 fd 当作占位接受。
        return Err(Errno::EOPNOTSUPP);
    }
    let prepared =
        sched::operation::prepare_clone_with_context(args, UserContextRef::new(ctx.tf.as_usize()))?;
    let installed_pidfd = install_clone_pidfd(args, prepared.child())?;
    let outcome = prepared.activate()?;
    if let Some(installed) = installed_pidfd {
        installed.commit();
    }
    ptrace_notify_fork(ctx.task(), args.flags, outcome.pid);
    clone_install_namespaces(ctx.task(), &outcome.child, args.flags)?;
    Ok(outcome.pid as usize)
}

struct InstalledClonePidfd {
    fdt: Arc<vfs::fdtable::FdTable>,
    fd: Option<Fd>,
}

impl InstalledClonePidfd {
    fn commit(mut self) {
        self.fd.take();
    }
}

impl Drop for InstalledClonePidfd {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            let _ = self.fdt.close_fd(fd);
        }
    }
}

fn install_clone_pidfd(
    args: CloneArgs,
    child: &Arc<Task>,
) -> Result<Option<InstalledClonePidfd>, Errno> {
    if !args.flags.has(CloneFlags::CLONE_PIDFD) {
        return Ok(None);
    }
    let fdt = vfs::current_fdtable().ok_or(Errno::ENOSYS)?;
    let cred = vfs::current_vfs_context()
        .map(|ctx| ctx.cred())
        .ok_or(Errno::ENOSYS)?;
    let fd = pidfd::create(&fdt, cred, child.thread_group(), false)?;
    let installed = InstalledClonePidfd {
        fdt: Arc::clone(&fdt),
        fd: Some(fd),
    };
    if let Err(err) = copy_to_user(args.pidfd, &(fd.as_raw() as i32).to_le_bytes()) {
        return Err(err.as_errno());
    }
    Ok(Some(installed))
}

fn prepare_clone3_set_tid(args: &mut CloneArgs, task: &Arc<Task>) -> Result<(), Errno> {
    if args.set_tid == 0 && args.set_tid_size == 0 {
        return Ok(());
    }
    if args.set_tid == 0 || args.set_tid_size == 0 {
        return Err(Errno::EINVAL);
    }
    if args.set_tid_size != 1 {
        // 多层 namespace 的 set_tid 数组需要完整 PID namespace 栈；当前 PID
        // 模型只有 root namespace 指定分配，因此只接受单元素数组。
        return Err(Errno::EOPNOTSUPP);
    }
    let creds = task.credentials();
    if !creds.euid.is_root()
        && !creds.has_cap(Capability::SysAdmin)
        && !creds.has_cap(Capability::CheckpointRestore)
    {
        return Err(Errno::EPERM);
    }
    let mut raw = [0u8; 4];
    copy_from_user(args.set_tid, &mut raw).map_err(|e| e.as_errno())?;
    let requested = i32::from_le_bytes(raw);
    if requested <= 0 || requested >= sched::pid::DEFAULT_PID_MAX {
        return Err(Errno::EINVAL);
    }
    if sched::root_pid_ns().registry().lookup(requested).is_some() {
        return Err(Errno::EEXIST);
    }
    args.requested_pid = requested;
    args.set_tid = 0;
    args.set_tid_size = 0;
    Ok(())
}

pub(super) fn sys_execve(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let request = ExecRequest::new(ctx.args[0], ctx.args[1], ctx.args[2]);
    sched::operation::execve_with_context(request, UserContextRef::new(ctx.tf.as_usize()))?;
    ctx.finalize_frame();
    Ok(0)
}

pub(super) fn sys_reboot(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let magic1 = ctx.args[0] as u32;
    let magic2 = ctx.args[1] as u32;
    let cmd = ctx.args[2] as u32;

    if magic1 != LINUX_REBOOT_MAGIC1
        || !matches!(
            magic2,
            LINUX_REBOOT_MAGIC2
                | LINUX_REBOOT_MAGIC2A
                | LINUX_REBOOT_MAGIC2B
                | LINUX_REBOOT_MAGIC2C
        )
    {
        return Err(Errno::EINVAL);
    }

    let creds = ctx.task().credentials();
    if !creds.has_cap(Capability::SysBoot) {
        return Err(Errno::EPERM);
    }

    match cmd {
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => Ok(0),
        LINUX_REBOOT_CMD_RESTART | LINUX_REBOOT_CMD_RESTART2 => {
            log::emergency!("[syscall][reboot] restart requested");
            let _ = crate::integrated_components::finalize_all();
            power::reboot().map_err(map_power_error)?;
            halt_after_power_request()
        }
        LINUX_REBOOT_CMD_HALT => {
            log::emergency!("[syscall][reboot] halt requested");
            let _ = crate::integrated_components::finalize_all();
            power::shutdown().map_err(map_power_error)?;
            halt_after_power_request()
        }
        LINUX_REBOOT_CMD_POWER_OFF => {
            log::emergency!("[syscall][reboot] poweroff requested");
            let _ = crate::integrated_components::finalize_all();
            power::shutdown().map_err(map_power_error)?;
            halt_after_power_request()
        }
        // SW_SUSPEND：无休眠（hibernation）支持，Linux 同样返回 EOPNOTSUPP。
        LINUX_REBOOT_CMD_SW_SUSPEND => Err(Errno::EOPNOTSUPP),
        // KEXEC：无 kexec 内核重载机制，与 sys_kexec_load/file_load 一致返回
        // ENOSYS（Linux 未启用 CONFIG_KEXEC 时的等价行为）。
        LINUX_REBOOT_CMD_KEXEC => Err(Errno::ENOSYS),
        _ => Err(Errno::EINVAL),
    }
}

fn map_power_error(err: power::PowerError) -> Errno {
    log::warning!("[syscall][reboot] power control failed: {:?}", err);
    Errno::EOPNOTSUPP
}

fn halt_after_power_request() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

pub(super) fn sys_wait4(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::ProcessWait);
    let pid = ctx.args[0] as i32;
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][wait4] enter parent={:?} target={} options={:#x}",
        ctx.task().pid_root(),
        pid,
        ctx.args[2],
    );
    let status_user = ctx.args[1];
    let options = WaitOptions::from_raw(ctx.args[2] as u32);
    let rusage_user = ctx.args[3];

    let result = sched::operation::wait4(pid, options)?;
    #[cfg(feature = "trace-signal-wait")]
    log::info!(
        "[syscall][wait4] leave parent={:?} target={} result={}",
        ctx.task().pid_root(),
        pid,
        result.pid,
    );
    if status_user != 0 {
        copy_to_user(status_user, &result.status.raw().to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    if rusage_user != 0 {
        write_rusage(rusage_user, result.usage, 0)?;
    }
    Ok(result.pid as usize)
}

pub(super) fn sys_waitid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::ProcessWait);
    const P_ALL: usize = 0;
    const P_PID: usize = 1;
    const P_PGID: usize = 2;
    const P_PIDFD: usize = 3;
    const WAITID_EVENTS: u32 =
        WaitOptions::WEXITED | WaitOptions::WSTOPPED | WaitOptions::WCONTINUED;

    let idtype = ctx.args[0];
    let id = ctx.args[1] as i32;
    let infop = ctx.args[2];
    let options = WaitOptions::from_raw(ctx.args[3] as u32);
    let rusage = ctx.args[4];

    if (options.raw() & WAITID_EVENTS) == 0 {
        return Err(Errno::EINVAL);
    }
    let target = match idtype {
        P_ALL => WaitId::All,
        P_PID => WaitId::Pid(id),
        P_PGID => {
            if id == 0 {
                WaitId::SameGroup
            } else {
                WaitId::Pgid(id)
            }
        }
        P_PIDFD => {
            let fdt = vfs::current_fdtable().ok_or(Errno::ENOSYS)?;
            let file = fdt.get_file(Fd::from_raw(id as u32)).ok_or(Errno::EBADF)?;
            let nonblock_pidfd = file.flags().nonblock;
            let group = pidfd::group_from_file(&file).ok_or(Errno::EINVAL)?;
            if nonblock_pidfd && !options.has(WaitOptions::WNOHANG) {
                let probe_options = WaitOptions::from_raw(options.raw() | WaitOptions::WNOHANG);
                let probe =
                    sched::operation::waitid(WaitId::Pidfd(Arc::clone(&group)), probe_options)?;
                if probe.pid == 0 {
                    return Err(Errno::EAGAIN);
                }
                if infop != 0 {
                    let mut raw = [0u8; 128];
                    write_i32(&mut raw, 0, SignalNumber::SIGCHLD.raw() as i32);
                    write_i32(&mut raw, 4, 0);
                    write_i32(&mut raw, 8, waitid_code(probe.status));
                    write_i32(&mut raw, 16, probe.pid);
                    write_u32(&mut raw, 20, ctx.task().credentials().uid.0);
                    write_i32(&mut raw, 24, waitid_status(probe.status));
                    copy_to_user(infop, &raw).map_err(|e| e.as_errno())?;
                }
                if rusage != 0 {
                    write_rusage(rusage, probe.usage, 0)?;
                }
                return Ok(0);
            }
            WaitId::Pidfd(group)
        }
        _ => return Err(Errno::EINVAL),
    };
    let result = sched::operation::waitid(target, options)?;
    if infop != 0 {
        let mut raw = [0u8; 128];
        if result.pid != 0 {
            write_i32(&mut raw, 0, SignalNumber::SIGCHLD.raw() as i32);
            write_i32(&mut raw, 4, 0);
            write_i32(&mut raw, 8, waitid_code(result.status));
            write_i32(&mut raw, 16, result.pid);
            write_u32(&mut raw, 20, ctx.task().credentials().uid.0);
            write_i32(&mut raw, 24, waitid_status(result.status));
        }
        copy_to_user(infop, &raw).map_err(|e| e.as_errno())?;
    }
    if rusage != 0 {
        write_rusage(rusage, result.usage, 0)?;
    }
    Ok(0)
}

pub(super) fn sys_sched_yield(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sched::operation::sched_yield()?;
    Ok(0)
}

pub(super) fn sys_kill(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let sig = signal_arg(ctx.args[1])?;
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][kill] enter sender={:?} target={} signal={:?}",
        ctx.task().pid_root(),
        pid,
        sig,
    );
    sched::operation::kill(pid, sig)?;
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][kill] leave sender={:?} target={} signal={:?}",
        ctx.task().pid_root(),
        pid,
        sig,
    );
    Ok(0)
}

pub(super) fn sys_tkill(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let tid = ctx.args[0] as i32;
    let sig = signal_arg(ctx.args[1])?;
    sched::operation::tkill(tid, sig)?;
    Ok(0)
}

pub(super) fn sys_tgkill(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let tgid = ctx.args[0] as i32;
    let tid = ctx.args[1] as i32;
    let sig = signal_arg(ctx.args[2])?;
    sched::operation::tgkill(tgid, tid, sig)?;
    Ok(0)
}

pub(super) fn sys_rt_tgsigqueueinfo(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let tgid = ctx.args[0] as i32;
    let tid = ctx.args[1] as i32;
    let sig = signal_arg(ctx.args[2])?;
    let uinfo = ctx.args[3];
    let Some(sig) = sig else {
        sched::operation::tgkill(tgid, tid, None)?;
        return Ok(0);
    };
    if uinfo == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 128];
    copy_from_user(uinfo, &mut raw).map_err(|e| e.as_errno())?;
    let signo = i32::from_le_bytes(raw[0..4].try_into().unwrap());
    if signo != sig.raw() as i32 {
        return Err(Errno::EINVAL);
    }
    let info = sched::SigInfo {
        sig,
        code: i32::from_le_bytes(raw[8..12].try_into().unwrap()),
        sender_pid: i32::from_le_bytes(raw[12..16].try_into().unwrap()),
        sender_uid: Uid(u32::from_le_bytes(raw[16..20].try_into().unwrap())),
        raw: Some(raw),
    };
    sched::operation::tgqueueinfo(tgid, tid, info)?;
    Ok(0)
}

pub(super) fn sys_setpgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sched::operation::setpgid(ctx.args[0] as i32, ctx.args[1] as i32)?;
    Ok(0)
}

pub(super) fn sys_getpgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::getpgid(ctx.args[0] as i32)? as usize)
}

pub(super) fn sys_getsid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::getsid(ctx.args[0] as i32)? as usize)
}

pub(super) fn sys_setsid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::setsid()? as usize)
}

pub(super) fn sys_set_tid_address(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    ctx.task().set_clear_child_tid(ctx.args[0]);
    Ok(sched::operation::gettid() as usize)
}

pub(super) fn sys_set_robust_list(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let head = ctx.args[0];
    let len = ctx.args[1];
    if len != ROBUST_LIST_HEAD_SIZE {
        return Err(Errno::EINVAL);
    }
    ctx.task().set_robust_list(head, len);
    Ok(0)
}

pub(super) fn sys_get_robust_list(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let head_user = ctx.args[1];
    let len_user = ctx.args[2];
    if head_user == 0 || len_user == 0 {
        return Err(Errno::EFAULT);
    }
    let task = lookup_task_for_thread_syscall(pid, ctx.task())?;
    require_task_access(ctx.task(), &task)?;
    let robust = task.robust_list();
    copy_to_user(head_user, &robust.head.to_ne_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(len_user, &robust.len.to_ne_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_rseq(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const RSEQ_SIZE: usize = 32;
    const RSEQ_ALIGN: usize = 32;
    const RSEQ_FLAG_UNREGISTER: usize = 1;
    let ptr = ctx.args[0];
    let len = ctx.args[1];
    let flags = ctx.args[2];
    let signature = ctx.args[3] as u32;

    if (flags & !RSEQ_FLAG_UNREGISTER) != 0 {
        return Err(Errno::EINVAL);
    }
    if ptr == 0 || len != RSEQ_SIZE || ptr & (RSEQ_ALIGN - 1) != 0 {
        return Err(Errno::EINVAL);
    }

    let current = ctx.task().rseq_registration();
    if (flags & RSEQ_FLAG_UNREGISTER) != 0 {
        if !current.registered {
            return Err(Errno::EINVAL);
        }
        if current.ptr != ptr || current.len as usize != len {
            return Err(Errno::EINVAL);
        }
        if current.signature != signature {
            return Err(Errno::EPERM);
        }
        reset_rseq_cpu_fields(ptr)?;
        ctx.task().clear_rseq_registration();
        return Ok(0);
    }

    if current.registered {
        if current.ptr != ptr || current.len as usize != len {
            return Err(Errno::EINVAL);
        }
        if current.signature != signature {
            return Err(Errno::EPERM);
        }
        return Err(Errno::EBUSY);
    }

    // 注册成功前先确认用户区可写，并把当前 CPU 写入 rseq 的两个 CPU 字段。
    // 后续迁移/切换由 kernel::sched 的 TaskCpuStateOps hook 继续刷新。
    let cpu_id = sched::current_cpu_id();
    let cpu = u32::try_from(cpu_id).map_err(|_| Errno::EINVAL)?;
    write_rseq_cpu_fields(ptr, cpu)?;
    ctx.task().set_rseq_registration(RseqRegistration {
        ptr,
        len: len as u32,
        signature,
        registered: true,
    });
    ctx.task().publish_rseq_cpu(cpu_id);
    Ok(0)
}

const RSEQ_CPU_ID_START_OFFSET: usize = 0;
const RSEQ_CPU_ID_OFFSET: usize = 4;

fn write_rseq_cpu_fields(ptr: usize, cpu: u32) -> Result<(), Errno> {
    let start_addr = ptr
        .checked_add(RSEQ_CPU_ID_START_OFFSET)
        .ok_or(Errno::EFAULT)?;
    let current_addr = ptr.checked_add(RSEQ_CPU_ID_OFFSET).ok_or(Errno::EFAULT)?;
    write_user_u32(start_addr, cpu)?;
    write_user_u32(current_addr, cpu)
}

fn reset_rseq_cpu_fields(ptr: usize) -> Result<(), Errno> {
    let start_addr = ptr
        .checked_add(RSEQ_CPU_ID_START_OFFSET)
        .ok_or(Errno::EFAULT)?;
    let current_addr = ptr.checked_add(RSEQ_CPU_ID_OFFSET).ok_or(Errno::EFAULT)?;
    write_user_u32(start_addr, 0)?;
    write_user_u32(current_addr, u32::MAX)
}

pub(super) fn sys_membarrier(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const MEMBARRIER_CMD_QUERY: usize = 0;
    const MEMBARRIER_CMD_GLOBAL: usize = 1 << 0;
    const MEMBARRIER_CMD_GLOBAL_EXPEDITED: usize = 1 << 1;
    const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: usize = 1 << 2;
    const MEMBARRIER_CMD_PRIVATE_EXPEDITED: usize = 1 << 3;
    const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: usize = 1 << 4;
    const MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE: usize = 1 << 5;
    const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: usize = 1 << 6;
    const MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ: usize = 1 << 7;
    const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ: usize = 1 << 8;
    const MEMBARRIER_CMD_GET_REGISTRATIONS: usize = 1 << 9;
    // 取舍：SYNC_CORE / RSEQ 变体在目标 CPU 上还须执行序列化指令 / 打断 rseq
    // 临界区；本内核 rendezvous 只做全内存屏障，二者按 PRIVATE_EXPEDITED 等价
    // 处理并如实声明支持位。
    let supported = MEMBARRIER_CMD_GLOBAL
        | MEMBARRIER_CMD_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ
        | MEMBARRIER_CMD_GET_REGISTRATIONS;

    let cmd = ctx.args[0];
    let flags = ctx.args[1];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    match cmd {
        MEMBARRIER_CMD_QUERY => Ok(supported),
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ => {
            let vm = task_vm_space(ctx.task()).ok_or(Errno::EINVAL)?;
            vm.register_membarrier(cmd);
            Ok(0)
        }
        MEMBARRIER_CMD_GLOBAL => {
            sched::synchronize_cpus()?;
            Ok(0)
        }
        MEMBARRIER_CMD_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ => {
            let required_registration = match cmd {
                MEMBARRIER_CMD_GLOBAL_EXPEDITED => MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED,
                MEMBARRIER_CMD_PRIVATE_EXPEDITED => MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
                MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => {
                    MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
                }
                MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ => {
                    MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ
                }
                _ => unreachable!(),
            };
            let vm = task_vm_space(ctx.task()).ok_or(Errno::EINVAL)?;
            if vm.membarrier_registration() & required_registration == 0 {
                return Err(Errno::EPERM);
            }
            sched::synchronize_cpus()?;
            Ok(0)
        }
        MEMBARRIER_CMD_GET_REGISTRATIONS => {
            let vm = task_vm_space(ctx.task()).ok_or(Errno::EINVAL)?;
            let mask = vm.membarrier_registration();
            // Linux 返回注册位图（低 32 位），高位须为 0。
            let out = ctx.args[2];
            if out != 0 {
                copy_to_user(out, &((mask & 0xffff_ffff) as u32).to_le_bytes())
                    .map_err(|e| e.as_errno())?;
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sys_prlimit64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Linux ABI:
    //   long prlimit64(
    //       pid_t pid,                    // a0
    //       int resource,                 // a1
    //       const struct rlimit64 *new,   // a2  (NULL 表示不改)
    //       struct rlimit64 *old          // a3  (NULL 表示不读)
    //   );
    //
    // struct rlimit64 {
    //   uint64_t rlim_cur;   /* soft */
    //   uint64_t rlim_max;   /* hard */
    // };
    let pid = ctx.args[0] as i32;
    let resource_raw = ctx.args[1] as u32;
    let new_user = ctx.args[2];
    let old_user = ctx.args[3];

    let resource = match sched::Resource::from_raw(resource_raw) {
        Some(r) => r,
        None => return Err(Errno::EINVAL),
    };

    // 读 new
    let new_pair = if new_user != 0 {
        let mut raw = [0u8; 16];
        copy_from_user(new_user, &mut raw).map_err(|e| e.as_errno())?;
        let cur = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let max = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        Some(sched::RlimitPair::new(
            sched::Rlim::from_raw(cur),
            sched::Rlim::from_raw(max),
        ))
    } else {
        None
    };

    let old = sched::operation::prlimit64(pid, resource, new_pair)?;
    if resource == sched::Resource::Nofile
        && let Some(new) = new_pair
    {
        let target = sched_task_from_pid(pid, ctx.task())?;
        sync_thread_group_fdtable_nofile_limit(&target, new)?;
    }

    if old_user != 0 {
        let mut raw = [0u8; 16];
        raw[0..8].copy_from_slice(&old.soft.raw().to_le_bytes());
        raw[8..16].copy_from_slice(&old.hard.raw().to_le_bytes());
        copy_to_user(old_user, &raw).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

/// `getrlimit(resource, rlim *)`—— 老 ABI，rlimit 结构体 16 字节。
///
/// struct rlimit {
///     unsigned long rlim_cur;   /* soft */
///     unsigned long rlim_max;   /* hard */
/// };
///
/// 与 prlimit64 不同，old 结构体是 8 字节 rlim（无 64 扩展）。
pub(super) fn sys_getrlimit(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let resource_raw = ctx.args[0] as u32;
    let rlim_user = ctx.args[1];
    let resource = match sched::Resource::from_raw(resource_raw) {
        Some(r) => r,
        None => return Err(Errno::EINVAL),
    };
    if rlim_user == 0 {
        return Err(Errno::EFAULT);
    }
    let pair = sched::operation::get_rlimit(resource)?;
    // 老 rlimit 是 8 字节字段。在 64-bit 内核上 rlim_t == u64；本仓库
    // 的硬件是 loongarch64/riscv64，所以使用 8 字节 rlim 与 16 字节结构。
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&pair.soft.raw().to_le_bytes());
    raw[8..16].copy_from_slice(&pair.hard.raw().to_le_bytes());
    copy_to_user(rlim_user, &raw).map_err(|e| e.as_errno())?;
    Ok(0)
}

/// `setrlimit(resource, const rlim *)`—— 老 ABI，写 new 同时也读 old 写回。
pub(super) fn sys_setrlimit(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let resource_raw = ctx.args[0] as u32;
    let rlim_user = ctx.args[1];
    let resource = match sched::Resource::from_raw(resource_raw) {
        Some(r) => r,
        None => return Err(Errno::EINVAL),
    };
    if rlim_user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 16];
    copy_from_user(rlim_user, &mut raw).map_err(|e| e.as_errno())?;
    let cur = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    let max = u64::from_le_bytes(raw[8..16].try_into().unwrap());
    let new = sched::RlimitPair::new(sched::Rlim::from_raw(cur), sched::Rlim::from_raw(max));
    let _old = sched::operation::set_rlimit(resource, new)?;
    if resource == sched::Resource::Nofile {
        sync_thread_group_fdtable_nofile_limit(ctx.task(), new)?;
    }
    Ok(0)
}

pub(super) fn sys_getrandom(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let buf_user = ctx.args[0];
    let size = ctx.args[1];
    let flags = ctx.args[2];
    const GRND_NONBLOCK: usize = 0x0001;
    const GRND_RANDOM: usize = 0x0002;
    const GRND_INSECURE: usize = 0x0004;
    const RANDOM_COPY_CHUNK: usize = 256;

    // 用户态标志只在 syscall 兼容层解释，底层随机源只提供字节填充能力。
    let known_flags = GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE;
    if (flags & !known_flags) != 0
        || (flags & (GRND_RANDOM | GRND_INSECURE)) == (GRND_RANDOM | GRND_INSECURE)
    {
        return Err(Errno::EINVAL);
    }

    let blocking = flags & GRND_NONBLOCK == 0;
    let mode = if flags & GRND_INSECURE != 0 {
        general::dev::random::RandomReadMode::Insecure
    } else if flags & GRND_RANDOM != 0 {
        general::dev::random::RandomReadMode::Entropy { blocking }
    } else {
        general::dev::random::RandomReadMode::Secure { blocking }
    };

    let mut done = 0usize;
    let mut chunk = [0u8; RANDOM_COPY_CHUNK];
    while done < size {
        let n = (size - done).min(chunk.len());
        // 256 字节只作为内核栈上临时缓冲大小，不限制用户态请求长度。
        let produced = match general::dev::random::fill(&mut chunk[..n], mode) {
            Ok(produced) if produced <= n => produced,
            Ok(_) if done != 0 => break,
            Ok(_) => return Err(Errno::EIO),
            Err(_) if done != 0 => break,
            Err(error) => {
                return Err(match error {
                    general::dev::char::CharIoError::Unavailable => Errno::ENODEV,
                    general::dev::char::CharIoError::Interrupted => Errno::EINTR,
                    general::dev::char::CharIoError::Timeout => Errno::EAGAIN,
                    general::dev::char::CharIoError::NoSpace => Errno::ENOSPC,
                    general::dev::char::CharIoError::HardwareError => Errno::EIO,
                });
            }
        };
        if produced == 0 {
            if done == 0 {
                return Err(Errno::EAGAIN);
            }
            break;
        }
        let dst = buf_user.checked_add(done).ok_or(Errno::EFAULT)?;
        copy_to_user(dst, &chunk[..produced]).map_err(|e| e.as_errno())?;
        done += produced;
        if produced < n {
            break;
        }
    }

    Ok(done)
}

pub(super) fn sys_clock_gettime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clock_id = ctx.args[0];
    let tp = ctx.args[1];
    let ns = clock_time_ns_for_task(ctx.task(), clock_id).ok_or(Errno::EINVAL)?;
    let sec = (ns / 1_000_000_000) as i64;
    let nsec = (ns % 1_000_000_000) as i64;
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&sec.to_le_bytes());
    out[8..16].copy_from_slice(&nsec.to_le_bytes());
    copy_to_user(tp, &out).map_err(|e| e.as_errno())?;
    Ok(0)
}

const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
const CLOCK_THREAD_CPUTIME_ID: usize = 3;
/// Linux `CLOCK_TAI`：国际原子时，= `CLOCK_REALTIME` + TAI 偏移。
const CLOCK_TAI: usize = 11;

fn clock_time_ns_for_task(task: &Arc<Task>, clock_id: usize) -> Option<u64> {
    match clock_id {
        CLOCK_PROCESS_CPUTIME_ID => {
            let now_ns = sched::now_ns_direct();
            let mut total = 0u64;
            for member in task.thread_group().snapshot() {
                let usage = member.usage_snapshot(now_ns);
                total = total
                    .saturating_add(usage.user_ns)
                    .saturating_add(usage.system_ns);
            }
            Some(total)
        }
        CLOCK_THREAD_CPUTIME_ID => {
            let usage = task.usage_snapshot(sched::now_ns_direct());
            Some(usage.user_ns.saturating_add(usage.system_ns))
        }
        CLOCK_TAI => {
            let realtime = crate::vdso::realtime_ns();
            Some((realtime as i64).saturating_add(crate::adjtimex::tai_offset_ns()) as u64)
        }
        _ => crate::vdso::clock_time_ns(clock_id),
    }
}

pub(super) fn sys_uname(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uts = Arc::clone(&crate::ns::task_ns(ctx.task()).uts);
    let mut out = [0u8; 65 * 6];
    write_uts_field(&mut out, 0, b"MyGo");
    let hostname = uts.hostname();
    write_uts_ns_field(&mut out, 1, &hostname, b"mygo");
    // release/version 由编译期信息驱动（Linux 由 uname_release/version 编译宏驱动）。
    write_uts_field(&mut out, 2, env!("CARGO_PKG_VERSION").as_bytes());
    write_uts_field(
        &mut out,
        3,
        concat!("MyGo kernel ", env!("CARGO_PKG_VERSION")).as_bytes(),
    );
    write_uts_field(&mut out, 4, hal::platform::arch_name().as_bytes());
    let domainname = uts.domainname();
    write_uts_ns_field(&mut out, 5, &domainname, b"localdomain");
    copy_to_user(ctx.args[0], &out).map_err(|e| e.as_errno())?;
    Ok(0)
}

/// 写 `utsname` 字段：命名空间值非空时优先。
fn write_uts_ns_field(out: &mut [u8], index: usize, value: &[u8], default: &[u8]) {
    if value.iter().all(|byte| *byte == 0) {
        write_uts_field(out, index, default);
    } else {
        write_uts_field(out, index, value);
    }
}

pub(super) fn sys_getcpu(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let cpu_user = ctx.args[0];
    let node_user = ctx.args[1];
    let (cpu, node) = sched::operation::getcpu()?;
    if cpu_user != 0 {
        copy_to_user(cpu_user, &cpu.to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    if node_user != 0 {
        copy_to_user(node_user, &node.to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_nanosleep(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    ctx.disable_restart();
    let req_user = ctx.args[0];
    let rem_user = ctx.args[1];
    if req_user == 0 {
        // 空指针是用户内存访问失败；只有读取到的 timespec 内容非法才返回 EINVAL。
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 16];
    copy_from_user(req_user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let ns_total = sec.saturating_mul(1_000_000_000i64).saturating_add(nsec);
    if ns_total == 0 {
        return Ok(0);
    }
    let deadline = sched::now_ns_direct().saturating_add(ns_total as u64);
    match sleep_until_deadline(ctx.task(), deadline, || Ok(sched::now_ns_direct())) {
        Ok(()) => Ok(0),
        Err(Errno::EINTR) => {
            write_remaining_timespec(rem_user, deadline.saturating_sub(sched::now_ns_direct()))?;
            Err(Errno::EINTR)
        }
        Err(err) => Err(err),
    }
}

pub(super) fn sys_getitimer(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let which = ctx.args[0];
    let curr_value = ctx.args[1];
    if curr_value == 0 {
        return Err(Errno::EFAULT);
    }
    let spec = match which {
        ITIMER_REAL => sched::get_realtime_itimer(ctx.task()),
        ITIMER_VIRTUAL => {
            let s = sched::cpu_itimer::get_cpu_itimer(
                &ctx.task(),
                sched::cpu_itimer::CpuItimerKind::Virtual,
            );
            sched::RealtimeItimerSpec {
                value_ns: s.value_ns,
                interval_ns: s.interval_ns,
            }
        }
        ITIMER_PROF => {
            let s = sched::cpu_itimer::get_cpu_itimer(
                &ctx.task(),
                sched::cpu_itimer::CpuItimerKind::Prof,
            );
            sched::RealtimeItimerSpec {
                value_ns: s.value_ns,
                interval_ns: s.interval_ns,
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    write_itimerval(curr_value, spec)?;
    Ok(0)
}

pub(super) fn sys_setitimer(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let which = ctx.args[0];
    let new_value = ctx.args[1];
    let old_value = ctx.args[2];

    let new_spec = if new_value == 0 {
        sched::RealtimeItimerSpec::default()
    } else {
        read_itimerval(new_value)?
    };
    let old_spec = match which {
        ITIMER_REAL => {
            sched::set_realtime_itimer(ctx.task(), new_spec.value_ns, new_spec.interval_ns)
        }
        ITIMER_VIRTUAL => {
            let old = sched::cpu_itimer::set_cpu_itimer(
                &ctx.task(),
                sched::cpu_itimer::CpuItimerKind::Virtual,
                new_spec.value_ns,
                new_spec.interval_ns,
            );
            sched::RealtimeItimerSpec {
                value_ns: old.value_ns,
                interval_ns: old.interval_ns,
            }
        }
        ITIMER_PROF => {
            let old = sched::cpu_itimer::set_cpu_itimer(
                &ctx.task(),
                sched::cpu_itimer::CpuItimerKind::Prof,
                new_spec.value_ns,
                new_spec.interval_ns,
            );
            sched::RealtimeItimerSpec {
                value_ns: old.value_ns,
                interval_ns: old.interval_ns,
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    if old_value != 0 {
        write_itimerval(old_value, old_spec)?;
    }
    Ok(0)
}

pub(super) fn sys_clock_nanosleep(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    ctx.disable_restart();
    let clock_id = ctx.args[0] as i32;
    let flags = ctx.args[1];
    let req_user = ctx.args[2];
    let rem_user = ctx.args[3];
    const TIMER_ABSTIME: usize = 1;
    if flags & !TIMER_ABSTIME != 0 {
        return Err(Errno::EINVAL);
    }
    if clock_id == CLOCK_THREAD_CPUTIME_ID as i32 {
        return Err(Errno::EOPNOTSUPP);
    }
    if clock_id != crate::vdso::CLOCK_REALTIME as i32
        && clock_id != crate::vdso::CLOCK_MONOTONIC as i32
        && clock_id != crate::vdso::CLOCK_MONOTONIC_RAW as i32
    {
        return Err(Errno::EINVAL);
    }
    if req_user == 0 {
        // 空指针是用户内存访问失败；只有读取到的 timespec 内容非法才返回 EINVAL。
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 16];
    copy_from_user(req_user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let absolute = (flags & TIMER_ABSTIME) != 0;
    let deadline = if absolute {
        sec.saturating_mul(1_000_000_000i64).saturating_add(nsec) as u64
    } else {
        let ns_total = sec.saturating_mul(1_000_000_000i64).saturating_add(nsec);
        sched::now_ns_direct().saturating_add(ns_total as u64)
    };
    if !absolute && sec == 0 && nsec == 0 {
        return Ok(0);
    }
    let now_fn = || {
        if absolute {
            crate::vdso::clock_time_ns(clock_id as usize).ok_or(Errno::EINVAL)
        } else {
            Ok(sched::now_ns_direct())
        }
    };
    match sleep_until_deadline(ctx.task(), deadline, now_fn) {
        Ok(()) => Ok(0),
        Err(Errno::EINTR) => {
            if !absolute {
                write_remaining_timespec(
                    rem_user,
                    deadline.saturating_sub(sched::now_ns_direct()),
                )?;
            }
            Err(Errno::EINTR)
        }
        Err(err) => Err(err),
    }
}

fn sleep_until_deadline(
    task: &Arc<Task>,
    deadline: u64,
    mut now_fn: impl FnMut() -> Result<u64, Errno>,
) -> Result<(), Errno> {
    loop {
        if now_fn()? >= deadline {
            return Ok(());
        }
        if sched::operation::has_interrupting_signal(task) {
            return Err(Errno::EINTR);
        }

        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(sched::WaitReason::Timer, sched::now_ns_direct());
        if !task.cas_state(TaskState::Running, TaskState::Sleeping)
            && !task.cas_state(TaskState::Runnable, TaskState::Sleeping)
            && task.state() != TaskState::Sleeping
        {
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            sched::operation::sched_yield()?;
            continue;
        }

        let now = now_fn()?;
        if now >= deadline {
            restore_current_task_after_sleep(task);
            return Ok(());
        }
        if sched::operation::has_interrupting_signal(task) {
            restore_current_task_after_sleep(task);
            return Err(Errno::EINTR);
        }

        let sleep_deadline = sched::now_ns_direct().saturating_add(deadline.saturating_sub(now));
        if !sched::register_sleep_deadline(task, sleep_deadline) {
            restore_current_task_after_sleep(task);
            return Ok(());
        }
        sched::schedule_once(sched::now_ns_direct());
        sched::cancel_sleep_deadline(task);
        restore_current_task_after_sleep(task);
    }
}

fn restore_current_task_after_sleep(task: &Arc<Task>) {
    if !task.cas_state(TaskState::Sleeping, TaskState::Running) {
        let _ = task.cas_state(TaskState::Runnable, TaskState::Running);
    }
    #[cfg(feature = "performance-profile")]
    task.cancel_profile_wait();
}

fn write_remaining_timespec(rem_user: usize, remaining_ns: u64) -> Result<(), Errno> {
    if rem_user == 0 {
        return Ok(());
    }
    let remaining_ns = remaining_ns.min(i64::MAX as u64) as i64;
    let rem_sec = remaining_ns / 1_000_000_000;
    let rem_nsec = remaining_ns % 1_000_000_000;
    let mut rem_buf = [0u8; 16];
    rem_buf[0..8].copy_from_slice(&rem_sec.to_le_bytes());
    rem_buf[8..16].copy_from_slice(&rem_nsec.to_le_bytes());
    copy_to_user(rem_user, &rem_buf).map_err(|e| e.as_errno())
}

pub(super) fn sys_clock_getres(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clock_id = ctx.args[0];
    let tp = ctx.args[1];
    let res_ns = match clock_id {
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => 1,
        CLOCK_TAI => {
            crate::vdso::clock_getres_ns(crate::vdso::CLOCK_REALTIME).ok_or(Errno::EINVAL)?
        }
        _ => crate::vdso::clock_getres_ns(clock_id).ok_or(Errno::EINVAL)?,
    } as i64;
    if tp != 0 {
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&0i64.to_le_bytes());
        out[8..16].copy_from_slice(&res_ns.to_le_bytes());
        copy_to_user(tp, &out).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_times(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let buf = ctx.args[0];
    let self_usage = ctx.task().usage_snapshot(sched::now_ns_direct());
    let child_usage = ctx.task().child_usage_snapshot();
    let ticks = ns_to_clock_ticks(sched::now_ns_direct());
    if buf != 0 {
        let mut raw = [0u8; 32];
        put_i64(&mut raw, 0, ns_to_clock_ticks(self_usage.user_ns));
        put_i64(&mut raw, 8, ns_to_clock_ticks(self_usage.system_ns));
        put_i64(&mut raw, 16, ns_to_clock_ticks(child_usage.user_ns));
        put_i64(&mut raw, 24, ns_to_clock_ticks(child_usage.system_ns));
        copy_to_user(buf, &raw).map_err(|e| e.as_errno())?;
    }
    Ok(ticks as usize)
}

pub(super) fn sys_getrusage(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const RUSAGE_SELF: i32 = 0;
    const RUSAGE_CHILDREN: i32 = -1;
    const RUSAGE_THREAD: i32 = 1;

    let who = ctx.args[0] as i32;
    let usage = ctx.args[1];
    if usage == 0 {
        return Err(Errno::EFAULT);
    }
    let task = ctx.task();
    let (snapshot, maxrss_kb) = match who {
        // RUSAGE_THREAD 只统计调用线程；RUSAGE_SELF 统计整个线程组
        // （Linux 语义：线程组累计）。RUSAGE_CHILDREN 是已 reap 子进程累计，
        // 子进程峰值驻留未记账，maxrss 记 0。
        RUSAGE_THREAD => (
            task.usage_snapshot(sched::now_ns_direct()),
            task_rss_kb(task),
        ),
        RUSAGE_SELF => (
            aggregate_thread_group_usage(task, sched::now_ns_direct()),
            task_rss_kb(task),
        ),
        RUSAGE_CHILDREN => (task.child_usage_snapshot(), 0),
        _ => return Err(Errno::EINVAL),
    };
    write_rusage(usage, snapshot, maxrss_kb)?;
    Ok(0)
}

/// 当前驻留页数折算为 KB（`ru_maxrss` best-effort）。
fn task_rss_kb(task: &Arc<Task>) -> u64 {
    task_vm_space(task)
        .map(|vm| (vm.mapped_pages() as u64).saturating_mul(hal::memory::page_size() as u64) / 1024)
        .unwrap_or(0)
}

/// 累加整个线程组的 usage（`RUSAGE_SELF`）。
fn aggregate_thread_group_usage(task: &Arc<Task>, now_ns: u64) -> sched::TaskUsage {
    let mut total = sched::TaskUsage {
        user_ns: 0,
        system_ns: 0,
        minflt: 0,
        majflt: 0,
        voluntary_ctxt_switches: 0,
        involuntary_ctxt_switches: 0,
    };
    for member in task.thread_group().snapshot() {
        total.add_assign(member.usage_snapshot(now_ns));
    }
    total
}

const USER_HZ: u64 = 100;

fn ns_to_clock_ticks(ns: u64) -> i64 {
    (ns / (1_000_000_000 / USER_HZ)).min(i64::MAX as u64) as i64
}

fn write_rusage(user: usize, usage: sched::TaskUsage, maxrss_kb: u64) -> Result<(), Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 144];
    write_timeval_pair(&mut raw, 0, usage.user_ns);
    write_timeval_pair(&mut raw, 16, usage.system_ns);
    // ru_maxrss：Linux 报告峰值驻留集（KB）。本内核无峰值记账，取当前
    // 驻留页数折算为 KB（best-effort，见任务书取舍说明）。
    put_i64(&mut raw, 32, maxrss_kb.min(i64::MAX as u64) as i64);
    put_i64(&mut raw, 64, usage.minflt.min(i64::MAX as u64) as i64);
    put_i64(&mut raw, 72, usage.majflt.min(i64::MAX as u64) as i64);
    // 块 I/O（inblock/oublock）与 IPC/信号计数当前无记账，保持 0。
    put_i64(
        &mut raw,
        128,
        usage.voluntary_ctxt_switches.min(i64::MAX as u64) as i64,
    );
    put_i64(
        &mut raw,
        136,
        usage.involuntary_ctxt_switches.min(i64::MAX as u64) as i64,
    );
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn write_timeval_pair(out: &mut [u8], off: usize, ns: u64) {
    let sec = (ns / 1_000_000_000).min(i64::MAX as u64) as i64;
    let usec = ((ns % 1_000_000_000) / 1_000) as i64;
    put_i64(out, off, sec);
    put_i64(out, off + 8, usec);
}

fn encode_sysinfo(uptime: i64, totalram: u64, freeram: u64) -> [u8; 112] {
    let mut out = [0u8; 112];
    put_i64(&mut out, 0, uptime);
    put_u64(&mut out, 32, totalram);
    put_u64(&mut out, 40, freeram);
    // 暂无可靠统计来源的共享内存、缓存、swap 和 highmem 字段保持为零。
    put_u16(&mut out, 80, 0);
    put_u64(&mut out, 88, 0);
    put_u64(&mut out, 96, 0);
    // RISC-V 与 LoongArch64 的用户态 ABI 使用 64 位 struct sysinfo。
    put_u32(&mut out, 104, 1);
    out
}

pub(super) fn sys_sysinfo(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let info = ctx.args[0];
    if info != 0 {
        // Linux struct sysinfo（64 位布局，sizeof == 112）：
        // uptime(0) loads[3](8,16,24) totalram(32) freeram(40) sharedram(48)
        // bufferram(56) totalswap(64) freeswap(72) procs(u16,80) pad(82)
        // totalhigh(88) freehigh(96) mem_unit(u32,104) _f(108..112)。
        let overview = allocator::KERNEL_ALLOCATOR.detailed_stats();
        let page_size = hal::memory::page_size() as u64;
        let totalram = overview.total_physical as u64;
        let freeram = overview.free_physical as u64;
        let sharedram = general::mm::memstat::SHARED_ANON_PAGES
            .load(core::sync::atomic::Ordering::Relaxed)
            .saturating_mul(page_size)
            .saturating_add(super::ipc::sysv_shm_total_bytes());
        let (total_swap_pages, free_swap_pages) = general::mm::swap::swap_totals();
        let mut procs = 0u16;
        if sched::is_ready() {
            for (_, weak) in sched::root_pid_ns().registry().snapshot() {
                if weak.upgrade().is_some() {
                    procs = procs.saturating_add(1);
                }
            }
        }
        let mut out = encode_sysinfo(
            (sched::now_ns_direct() / 1_000_000_000) as i64,
            totalram,
            freeram,
        );
        let loads = sched::avenrun::loads_scaled();
        put_u64(&mut out, 8, loads[0]);
        put_u64(&mut out, 16, loads[1]);
        put_u64(&mut out, 24, loads[2]);
        put_u64(&mut out, 48, sharedram);
        put_u64(&mut out, 56, 0); // 无块设备 buffer 记账。
        put_u64(&mut out, 64, total_swap_pages.saturating_mul(page_size));
        put_u64(&mut out, 72, free_swap_pages.saturating_mul(page_size));
        put_u16(&mut out, 80, procs);
        copy_to_user(info, &out).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_gettimeofday(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let tv = ctx.args[0];
    let _tz = ctx.args[1];
    if tv != 0 {
        let ns = crate::vdso::realtime_ns();
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&((ns / 1_000_000_000) as i64).to_le_bytes());
        out[8..16].copy_from_slice(&((ns % 1_000_000_000 / 1000) as i64).to_le_bytes());
        copy_to_user(tv, &out).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_getpriority(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let targets = priority_targets(ctx.args[0], ctx.args[1], ctx.task())?;
    let best = targets
        .iter()
        .map(|task| task.pi_base_attr().nice as i32)
        .min()
        .ok_or(Errno::ESRCH)?;
    Ok(linux_priority_from_nice(best))
}

pub(super) fn sys_setpriority(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let targets = priority_targets(ctx.args[0], ctx.args[1], ctx.task())?;
    let nice = (ctx.args[2] as isize as i32).clamp(MIN_NICE, MAX_NICE);
    for task in &targets {
        check_setpriority_permission(ctx.task(), task, nice)?;
    }
    for task in targets {
        sched::operation::sched_setnice_for_task(&task, nice as i8)?;
    }
    Ok(0)
}

fn linux_priority_from_nice(nice: i32) -> usize {
    (20 - nice.clamp(MIN_NICE, MAX_NICE)) as usize
}

/// 按 Linux `which/who` 选择目标任务集合。
///
/// `who == 0` 的含义依赖 `which`，所以只在兼容层展开；返回集合为空时按
/// syscall 语义报 `ESRCH`。
fn priority_targets(which: usize, who: usize, caller: &Arc<Task>) -> Result<Vec<Arc<Task>>, Errno> {
    let tasks = match which {
        PRIO_PROCESS => {
            let task = if who == 0 {
                Arc::clone(caller)
            } else {
                lookup_root_task(who as i32)?
            };
            alloc::vec![task]
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                caller.process_group().pgid()
            } else {
                who as i32
            };
            if pgid <= 0 {
                return Err(Errno::ESRCH);
            }
            tasks_by_process_group(pgid)
        }
        PRIO_USER => {
            let uid = if who == 0 {
                caller.credentials().uid
            } else {
                Uid(who as u32)
            };
            tasks_by_real_uid(uid)
        }
        _ => return Err(Errno::EINVAL),
    };
    if tasks.is_empty() {
        Err(Errno::ESRCH)
    } else {
        Ok(tasks)
    }
}

/// 遍历根 PID namespace，收集指定进程组中的任务。
fn tasks_by_process_group(pgid: i32) -> Vec<Arc<Task>> {
    sched::root_pid_ns()
        .registry()
        .snapshot()
        .into_iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .filter(|task| task.process_group().pgid() == pgid)
        .collect()
}

/// 遍历根 PID namespace，收集真实 UID 匹配的任务。
fn tasks_by_real_uid(uid: Uid) -> Vec<Arc<Task>> {
    sched::root_pid_ns()
        .registry()
        .snapshot()
        .into_iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .filter(|task| task.credentials().uid == uid)
        .collect()
}

/// 校验 setpriority 的目标权限与优先级提升权限。
///
/// 普通调用者只能修改同属主任务；降低 nice（提高优先级）时，还必须满足
/// `CAP_SYS_NICE` 或当前线程组的 `RLIMIT_NICE` 下限。
fn check_setpriority_permission(
    caller: &Arc<Task>,
    target: &Arc<Task>,
    requested_nice: i32,
) -> Result<(), Errno> {
    let caller_creds = caller.credentials();
    if caller_creds.has_cap(Capability::SysNice) {
        return Ok(());
    }
    check_sched_target_owner(&caller_creds, target)?;

    let current = target.pi_base_attr().nice as i32;
    if requested_nice < current && requested_nice < nice_floor_from_rlimit(caller) {
        return Err(Errno::EACCES);
    }
    Ok(())
}

/// 把 `RLIMIT_NICE` 的 soft limit 转成允许设置的最低 nice 值。
fn nice_floor_from_rlimit(task: &Arc<Task>) -> i32 {
    let limit = task
        .thread_group()
        .rlimits()
        .lock()
        .get(sched::Resource::Nice)
        .soft
        .raw()
        .min(40) as i32;
    (20 - limit).clamp(MIN_NICE, MAX_NICE)
}

pub(super) fn sys_sched_getparam(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let param_user = ctx.args[1];
    if param_user == 0 {
        return Err(Errno::EFAULT);
    }
    let task = sched_task_from_pid(pid, ctx.task())?;
    let attr = sched::operation::sched_getattr_for_task(&task);
    let mut out = [0u8; 4];
    out[0..4].copy_from_slice(&(attr.priority as i32).to_le_bytes());
    copy_to_user(param_user, &out).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_sched_setparam(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let param_user = ctx.args[1];
    if param_user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 4];
    copy_from_user(param_user, &mut raw).map_err(|e| e.as_errno())?;
    let priority = i32::from_le_bytes(raw);
    let task = sched_task_from_pid(pid, ctx.task())?;
    let old = task.pi_base_attr();
    let mut attr = old;
    match attr.policy {
        SchedPolicy::Fair | SchedPolicy::Batch | SchedPolicy::Idle => {
            if priority != 0 {
                return Err(Errno::EINVAL);
            }
        }
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => {
            attr.priority = linux_rt_priority_from_param(priority)?;
        }
        SchedPolicy::Deadline => return Err(Errno::EINVAL),
    }
    check_sched_attr_permission(ctx.task(), &task, old, attr)?;
    sched::operation::sched_setattr_for_task(&task, attr)?;
    Ok(0)
}

pub(super) fn sys_sched_getscheduler(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let task = sched_task_from_pid(pid, ctx.task())?;
    let attr = sched::operation::sched_getattr_for_task(&task);
    let mut raw = encode_linux_sched_policy(attr.policy);
    if task.sched_reset_on_fork() {
        raw |= SCHED_RESET_ON_FORK;
    }
    Ok(raw)
}

pub(super) fn sys_sched_setscheduler(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let raw_policy = ctx.args[1];
    let reset_on_fork = (raw_policy & SCHED_RESET_ON_FORK) != 0;
    let policy = decode_linux_sched_policy(raw_policy)?;
    let param_user = ctx.args[2];
    if param_user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 4];
    copy_from_user(param_user, &mut raw).map_err(|e| e.as_errno())?;
    let priority = i32::from_le_bytes(raw);
    if matches!(
        policy,
        SchedPolicy::Fair | SchedPolicy::Batch | SchedPolicy::Idle
    ) && priority != 0
    {
        return Err(Errno::EINVAL);
    }
    let rt_priority = if matches!(policy, SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin) {
        linux_rt_priority_from_param(priority)?
    } else {
        0
    };
    let task = sched_task_from_pid(pid, ctx.task())?;
    let old = task.pi_base_attr();
    let attr = SchedAttr {
        policy,
        nice: old.nice,
        slice_ns: if policy == SchedPolicy::RtRoundRobin {
            sched::sched_rr_timeslice_ns()
        } else {
            old.slice_ns
        },
        priority: rt_priority,
        runtime_ns: 0,
        deadline_ns: 0,
        period_ns: 0,
    };
    check_sched_attr_permission(ctx.task(), &task, old, attr)?;
    sched::operation::sched_setattr_for_task(&task, attr)?;
    task.set_sched_reset_on_fork(reset_on_fork);
    Ok(0)
}

pub(super) fn sys_sched_getaffinity(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let cpusetsize = ctx.args[1];
    let mask_user = ctx.args[2];
    let kernel_bytes = kernel_cpuset_bytes();
    if mask_user == 0 {
        return Err(Errno::EFAULT);
    }
    if cpusetsize < kernel_bytes {
        return Err(Errno::EINVAL);
    }

    let task = sched_task_from_pid(pid, ctx.task())?;
    let affinity = sched::operation::sched_getaffinity_for_task(&task);
    let mut mask = Vec::new();
    mask.resize(kernel_bytes, 0);
    write_cpuset_mask(&mut mask, affinity);
    copy_to_user(mask_user, &mask).map_err(|e| e.as_errno())?;
    Ok(kernel_bytes)
}

pub(super) fn sys_sched_setaffinity(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let cpusetsize = ctx.args[1];
    let mask_user = ctx.args[2];
    let kernel_bytes = kernel_cpuset_bytes();
    if mask_user == 0 {
        return Err(Errno::EFAULT);
    }
    if cpusetsize < kernel_bytes {
        return Err(Errno::EINVAL);
    }
    let mut mask = Vec::new();
    mask.resize(kernel_bytes, 0);
    copy_from_user(mask_user, &mut mask).map_err(|e| e.as_errno())?;
    let task = sched_task_from_pid(pid, ctx.task())?;
    check_sched_target_permission(ctx.task(), &task)?;
    sched::operation::sched_setaffinity_for_task(&task, read_cpuset_mask(&mask))?;
    Ok(0)
}

fn kernel_cpuset_bytes() -> usize {
    sched::CpuMask::supported_storage_bytes()
}

fn read_cpuset_mask(raw: &[u8]) -> u64 {
    let mut mask = 0u64;
    for (idx, byte) in raw.iter().take(8).enumerate() {
        mask |= (*byte as u64) << (idx * 8);
    }
    mask
}

fn write_cpuset_mask(out: &mut [u8], mask: u64) {
    let raw = mask.to_le_bytes();
    let n = out.len().min(raw.len());
    out[..n].copy_from_slice(&raw[..n]);
}

const MYGO_SCHED_INFO_VERSION: u32 = 2;
const MYGO_SCHED_INFO_HEADER_SIZE: usize = 80;
const MYGO_SCHED_DOMAIN_ENTRY_SIZE: usize = 32;
const MYGO_SCHED_DOMAIN_PARENT_NONE: u32 = u32::MAX;
const MYGO_SCHED_INFO_F_TARGET_PID: usize = 1 << 0;
const MYGO_SCHED_INFO_INVALID_U32: u32 = u32::MAX;

pub(super) fn sys_mygo_sched_info(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let user = ctx.args[0];
    let size = ctx.args[1];
    let flags = ctx.args[2];
    if flags & !MYGO_SCHED_INFO_F_TARGET_PID != 0 {
        return Err(Errno::EINVAL);
    }
    let target = if flags & MYGO_SCHED_INFO_F_TARGET_PID != 0 {
        sched_task_from_pid(ctx.args[3] as i32, ctx.task())?
    } else {
        Arc::clone(ctx.task())
    };
    check_sched_target_permission(ctx.task(), &target)?;

    let topology = sched::sched_topology();
    let placement = sched::task_sched_placement(&target);
    let domain_count = topology.len();
    let required = MYGO_SCHED_INFO_HEADER_SIZE
        .checked_add(
            domain_count
                .checked_mul(MYGO_SCHED_DOMAIN_ENTRY_SIZE)
                .ok_or(Errno::EINVAL)?,
        )
        .ok_or(Errno::EINVAL)?;
    if user == 0 || size == 0 {
        return Ok(required);
    }
    if size < required {
        return Err(Errno::EINVAL);
    }

    let mut out = Vec::new();
    out.resize(required, 0);

    // 头部只定义 MyGo 私有调度查询格式；具体偏移不进入 sched 核心，避免
    // 底层调度模型被用户态 ABI 绑死。
    write_u32(&mut out, 0, MYGO_SCHED_INFO_VERSION);
    write_u32(&mut out, 4, MYGO_SCHED_INFO_HEADER_SIZE as u32);
    write_u32(&mut out, 8, MYGO_SCHED_DOMAIN_ENTRY_SIZE as u32);
    write_u32(&mut out, 12, domain_count as u32);
    write_u64(&mut out, 16, sched::supported_cpu_mask());
    write_u64(&mut out, 24, sched::online_cpu_mask());
    let current_cpu = sched::current_cpu_id();
    write_u32(&mut out, 32, current_cpu as u32);
    write_u32(
        &mut out,
        36,
        sched::current_sched_domain_id(current_cpu).unwrap_or(0) as u32,
    );
    write_u64(&mut out, 40, placement.affinity.bits());
    write_u64(&mut out, 48, placement.effective.bits());
    write_u32(
        &mut out,
        56,
        placement
            .current_cpu
            .map(|cpu| cpu.get() as u32)
            .unwrap_or(MYGO_SCHED_INFO_INVALID_U32),
    );
    write_u32(
        &mut out,
        60,
        placement
            .current_domain
            .map(|id| id as u32)
            .unwrap_or(MYGO_SCHED_INFO_INVALID_U32),
    );
    write_u32(
        &mut out,
        64,
        placement
            .preferred_cpu
            .map(|cpu| cpu.get() as u32)
            .unwrap_or(MYGO_SCHED_INFO_INVALID_U32),
    );
    write_u32(&mut out, 68, target.pid_root().unwrap_or(0) as u32);

    for idx in 0..domain_count {
        let Some(domain) = topology.domain(idx) else {
            return Err(Errno::EINVAL);
        };
        let off = MYGO_SCHED_INFO_HEADER_SIZE + idx * MYGO_SCHED_DOMAIN_ENTRY_SIZE;
        write_u32(&mut out, off, domain.id() as u32);
        write_u32(
            &mut out,
            off + 4,
            domain
                .parent()
                .map(|id| id as u32)
                .unwrap_or(MYGO_SCHED_DOMAIN_PARENT_NONE),
        );
        write_u32(&mut out, off + 8, domain.level() as u32);
        write_u64(&mut out, off + 16, domain.span().bits());
    }

    copy_to_user(user, &out).map_err(|e| e.as_errno())?;
    Ok(required)
}

pub(super) fn sys_sched_get_priority_max(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    match decode_linux_sched_policy(ctx.args[0])? {
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => Ok(sched::RT_PRIO_MAX as usize),
        _ => Ok(0),
    }
}

pub(super) fn sys_sched_get_priority_min(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    match decode_linux_sched_policy(ctx.args[0])? {
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => Ok(sched::RT_PRIO_MIN as usize),
        _ => Ok(0),
    }
}

pub(super) fn sys_sched_rr_get_interval(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let tp = ctx.args[1];
    if tp == 0 {
        return Err(Errno::EFAULT);
    }
    let task = sched_task_from_pid(pid, ctx.task())?;
    let attr = sched::operation::sched_getattr_for_task(&task);
    let interval_ns = if attr.policy == SchedPolicy::RtRoundRobin {
        sched::sched_rr_timeslice_ns()
    } else if attr.slice_ns == 0 {
        sched::DEFAULT_RR_SLICE_NS
    } else {
        attr.slice_ns
    };
    write_timespec_ns(tp, interval_ns)?;
    Ok(0)
}

pub(super) fn sys_sched_setattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let attr_user = ctx.args[1];
    let flags = ctx.args[2];
    if attr_user == 0 {
        return Err(Errno::EFAULT);
    }
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let (mut attr, reset_on_fork) = read_linux_sched_attr(attr_user)?;
    if attr.policy == SchedPolicy::RtRoundRobin {
        attr.slice_ns = sched::sched_rr_timeslice_ns();
    }
    let task = sched_task_from_pid(pid, ctx.task())?;
    let old = task.pi_base_attr();
    check_sched_attr_permission(ctx.task(), &task, old, attr)?;
    sched::operation::sched_setattr_for_task(&task, attr)?;
    task.set_sched_reset_on_fork(reset_on_fork);
    Ok(0)
}

/// 校验调度属性修改权限。
///
/// syscall 层负责用户态权限和资源限制；sched 核心只接收已经授权的内部属性。
fn check_sched_attr_permission(
    caller: &Arc<Task>,
    target: &Arc<Task>,
    old: SchedAttr,
    requested: SchedAttr,
) -> Result<(), Errno> {
    let caller_creds = caller.credentials();
    if caller_creds.has_cap(Capability::SysNice) {
        return Ok(());
    }
    check_sched_target_owner(&caller_creds, target)?;

    let requested = requested.validate()?;
    match requested.policy {
        SchedPolicy::Fair | SchedPolicy::Batch => {
            if requested.nice < old.nice && (requested.nice as i32) < nice_floor_from_rlimit(caller)
            {
                return Err(Errno::EACCES);
            }
        }
        SchedPolicy::Idle => {}
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => {
            if old.policy != requested.policy || requested.priority > old.priority {
                check_rtprio_limit(caller, requested.priority)?;
            }
        }
        SchedPolicy::Deadline => return Err(Errno::EPERM),
    }
    Ok(())
}

/// 校验调度类系统调用的目标权限：同属主或具备 `CAP_SYS_NICE`。
fn check_sched_target_permission(caller: &Arc<Task>, target: &Arc<Task>) -> Result<(), Errno> {
    let caller_creds = caller.credentials();
    if caller_creds.has_cap(Capability::SysNice) {
        return Ok(());
    }
    check_sched_target_owner(&caller_creds, target)
}

/// Linux 调度权限使用调用者 euid 匹配目标 real/effective uid。
fn check_sched_target_owner(caller_creds: &Credentials, target: &Arc<Task>) -> Result<(), Errno> {
    let target_creds = target.credentials();
    if caller_creds.euid == target_creds.uid || caller_creds.euid == target_creds.euid {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

fn check_rtprio_limit(caller: &Arc<Task>, priority: u8) -> Result<(), Errno> {
    let limit = caller
        .thread_group()
        .rlimits()
        .lock()
        .get(sched::Resource::RtPrio)
        .soft
        .raw()
        .min(sched::RT_PRIO_MAX as u64) as u8;
    if priority <= limit {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

pub(super) fn sys_sched_getattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let attr_user = ctx.args[1];
    let size = ctx.args[2];
    let flags = ctx.args[3];
    if attr_user == 0 {
        return Err(Errno::EFAULT);
    }
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let task = sched_task_from_pid(pid, ctx.task())?;
    let attr = sched::operation::sched_getattr_for_task(&task);
    write_linux_sched_attr(attr_user, size, attr, task.sched_reset_on_fork())?;
    Ok(0)
}

const SCHED_RESET_ON_FORK: usize = 0x4000_0000;
const SCHED_FLAG_RESET_ON_FORK: u64 = 0x01;

/// 解析调度 syscall 的 `pid` 参数：0 表示当前线程，负数按 Linux 返回 EINVAL。
fn sched_task_from_pid(pid: i32, caller: &Arc<Task>) -> Result<Arc<Task>, Errno> {
    if pid == 0 {
        Ok(Arc::clone(caller))
    } else if pid > 0 {
        lookup_root_task(pid)
    } else {
        Err(Errno::EINVAL)
    }
}

fn lookup_root_task(pid: i32) -> Result<Arc<Task>, Errno> {
    sched::root_pid_ns()
        .registry()
        .lookup(pid)
        .and_then(|weak| weak.upgrade())
        .ok_or(Errno::ESRCH)
}

/// Linux `sched_param.sched_priority` 转内部 RT 优先级。
fn linux_rt_priority_from_param(priority: i32) -> Result<u8, Errno> {
    if (sched::RT_PRIO_MIN as i32..=sched::RT_PRIO_MAX as i32).contains(&priority) {
        Ok(priority as u8)
    } else {
        Err(Errno::EINVAL)
    }
}

fn decode_linux_sched_policy(raw: usize) -> Result<SchedPolicy, Errno> {
    match raw & !SCHED_RESET_ON_FORK {
        0 => Ok(SchedPolicy::Fair),
        1 => Ok(SchedPolicy::RtFifo),
        2 => Ok(SchedPolicy::RtRoundRobin),
        3 => Ok(SchedPolicy::Batch),
        5 => Ok(SchedPolicy::Idle),
        6 => Ok(SchedPolicy::Deadline),
        _ => Err(Errno::EINVAL),
    }
}

fn encode_linux_sched_policy(policy: SchedPolicy) -> usize {
    match policy {
        SchedPolicy::Fair => 0,
        SchedPolicy::RtFifo => 1,
        SchedPolicy::RtRoundRobin => 2,
        SchedPolicy::Batch => 3,
        SchedPolicy::Idle => 5,
        SchedPolicy::Deadline => 6,
    }
}

const LINUX_SCHED_ATTR_SIZE: usize = 56;
const LINUX_SCHED_ATTR_BASE_SIZE: usize = 48;
const LINUX_SCHED_ATTR_MAX_SIZE: usize = 4096;
const LINUX_CLONE_ARGS_SIZE: usize = 88;
const LINUX_CLONE_ARGS_MIN_SIZE: usize = 64;
const LINUX_CLONE_ARGS_MAX_SIZE: usize = 4096;

fn validate_clone3_tail(user: usize, size: usize) -> Result<(), Errno> {
    if size <= LINUX_CLONE_ARGS_SIZE {
        return Ok(());
    }
    // TODO(threading): accept future clone_args fields once pidfd/set_tid/cgroup
    // backing state exists. For now, Linux-compatible probing requires unknown
    // extension bytes to be zero.
    validate_user_tail_zero(user, LINUX_CLONE_ARGS_SIZE, size - LINUX_CLONE_ARGS_SIZE)
}

fn validate_user_tail_zero(user: usize, offset: usize, len: usize) -> Result<(), Errno> {
    let mut checked = 0usize;
    let mut chunk = [0u8; 64];
    while checked < len {
        let n = (len - checked).min(chunk.len());
        let addr = user
            .checked_add(offset)
            .and_then(|addr| addr.checked_add(checked))
            .ok_or(Errno::EFAULT)?;
        copy_from_user(addr, &mut chunk[..n]).map_err(|e| e.as_errno())?;
        if chunk[..n].iter().any(|byte| *byte != 0) {
            return Err(Errno::E2BIG);
        }
        checked += n;
    }
    Ok(())
}

fn read_linux_sched_attr(user: usize) -> Result<(SchedAttr, bool), Errno> {
    let mut raw = [0u8; LINUX_SCHED_ATTR_SIZE];
    copy_from_user(user, &mut raw[..4]).map_err(|e| e.as_errno())?;
    let size = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
    if size < LINUX_SCHED_ATTR_BASE_SIZE {
        return Err(Errno::EINVAL);
    }
    if size > LINUX_SCHED_ATTR_MAX_SIZE {
        return Err(Errno::E2BIG);
    }
    let n = size.min(LINUX_SCHED_ATTR_SIZE);
    copy_from_user(user, &mut raw[..n]).map_err(|e| e.as_errno())?;
    if size > LINUX_SCHED_ATTR_SIZE {
        // TODO(threading): newer sched_attr extensions must be modelled in
        // SchedAttr before non-zero extension fields can be accepted.
        validate_user_tail_zero(user, LINUX_SCHED_ATTR_SIZE, size - LINUX_SCHED_ATTR_SIZE)?;
    }

    let policy =
        decode_linux_sched_policy(u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize)?;
    let flags = u64::from_le_bytes(raw[8..16].try_into().unwrap());
    if (flags & !SCHED_FLAG_RESET_ON_FORK) != 0 {
        // TODO(threading): 其它 sched_attr flags 需要调度器提供对应的继承或
        // admission-control 状态，不能在 ABI 层静默吞掉。
        return Err(Errno::EOPNOTSUPP);
    }
    Ok((
        SchedAttr {
            policy,
            nice: i32::from_le_bytes(raw[16..20].try_into().unwrap()) as i8,
            slice_ns: 0,
            priority: u32::from_le_bytes(raw[20..24].try_into().unwrap()) as u8,
            runtime_ns: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
            deadline_ns: u64::from_le_bytes(raw[32..40].try_into().unwrap()),
            period_ns: u64::from_le_bytes(raw[40..48].try_into().unwrap()),
        },
        (flags & SCHED_FLAG_RESET_ON_FORK) != 0,
    ))
}

fn write_linux_sched_attr(
    user: usize,
    size: usize,
    attr: SchedAttr,
    reset_on_fork: bool,
) -> Result<(), Errno> {
    if size < LINUX_SCHED_ATTR_BASE_SIZE {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; LINUX_SCHED_ATTR_SIZE];
    raw[0..4].copy_from_slice(&(LINUX_SCHED_ATTR_SIZE as u32).to_le_bytes());
    raw[4..8].copy_from_slice(&(encode_linux_sched_policy(attr.policy) as u32).to_le_bytes());
    let flags = if reset_on_fork {
        SCHED_FLAG_RESET_ON_FORK
    } else {
        0
    };
    raw[8..16].copy_from_slice(&flags.to_le_bytes());
    raw[16..20].copy_from_slice(&(attr.nice as i32).to_le_bytes());
    raw[20..24].copy_from_slice(&(attr.priority as u32).to_le_bytes());
    raw[24..32].copy_from_slice(&attr.runtime_ns.to_le_bytes());
    raw[32..40].copy_from_slice(&attr.deadline_ns.to_le_bytes());
    raw[40..48].copy_from_slice(&attr.period_ns.to_le_bytes());
    let n = size.min(LINUX_SCHED_ATTR_SIZE);
    copy_to_user(user, &raw[..n]).map_err(|e| e.as_errno())
}

pub(super) fn sys_personality(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Linux `personality(2)`：
    // - `persona == 0xffffffff`：只读当前 persona，不修改；
    // - 否则写入新 persona 并返回旧值。
    // `PER_*` 位（`ADDR_NO_RANDOMIZE` 等）随 fork 继承、exec 保留。
    // 取舍：本内核尚未实现用户态 ASLR（mmap 基址由确定布局决定），因此
    // `ADDR_NO_RANDOMIZE`/`ADDR_COMPAT_LAYOUT` 等位只做持久化与回读，
    // 不会改变地址空间布局（见 general::mm 的 user_mmap_base 固定布局）。
    let persona = ctx.args[0] as u64;
    let task = ctx.task();
    let old = task.personality();
    if persona != 0xffff_ffff {
        task.set_personality(persona);
    }
    Ok(old as usize)
}

pub(super) fn sys_prctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const PR_SET_PDEATHSIG: usize = 1;
    const PR_GET_PDEATHSIG: usize = 2;
    const PR_GET_DUMPABLE: usize = 3;
    const PR_SET_DUMPABLE: usize = 4;
    const PR_GET_KEEPCAPS: usize = 7;
    const PR_SET_KEEPCAPS: usize = 8;
    const PR_SET_NAME: usize = 15;
    const PR_GET_NAME: usize = 16;
    const PR_GET_SECCOMP: usize = 21;
    const PR_SET_SECCOMP: usize = 22;
    const PR_CAPBSET_READ: usize = 23;
    const PR_CAPBSET_DROP: usize = 24;
    const PR_GET_TSC: usize = 25;
    const PR_SET_TSC: usize = 26;
    const PR_GET_SECUREBITS: usize = 27;
    const PR_SET_SECUREBITS: usize = 28;
    const PR_SET_TIMERSLACK: usize = 29;
    const PR_GET_TIMERSLACK: usize = 30;
    const PR_SET_MM: usize = 35;
    const PR_SET_CHILD_SUBREAPER: usize = 36;
    const PR_GET_CHILD_SUBREAPER: usize = 37;
    const PR_SET_NO_NEW_PRIVS: usize = 38;
    const PR_GET_NO_NEW_PRIVS: usize = 39;
    const PR_SET_THP_DISABLE: usize = 41;
    const PR_GET_THP_DISABLE: usize = 42;
    const PR_GET_TID_ADDRESS: usize = 40;
    const PR_CAP_AMBIENT: usize = 47;
    const PR_GET_SPECULATION_CTRL: usize = 52;
    const PR_SET_SPECULATION_CTRL: usize = 53;
    const PR_SET_PTRACER: usize = 0x5961_6d61;
    const PR_GET_PTRACER: usize = 0x5961_6d62;
    const PR_SET_VMA: usize = 0x5356_4d41;
    const PR_GET_AUXV: usize = 0x4155_5856;

    const SECBIT_KEEP_CAPS: u32 = 1 << 0;
    const SECBIT_KEEP_CAPS_LOCKED: u32 = 1 << 1;
    const SECBIT_NO_SETUID_FIXUP: u32 = 1 << 2;
    const SECBIT_NO_SETUID_FIXUP_LOCKED: u32 = 1 << 3;
    const SECBIT_NOROOT: u32 = 1 << 4;
    const SECBIT_NOROOT_LOCKED: u32 = 1 << 5;
    const SECBIT_NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
    const SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED: u32 = 1 << 7;
    const SECBIT_LOCKED: u32 = SECBIT_KEEP_CAPS_LOCKED
        | SECBIT_NO_SETUID_FIXUP_LOCKED
        | SECBIT_NOROOT_LOCKED
        | SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;
    const SECBIT_ALL: u32 = SECBIT_KEEP_CAPS
        | SECBIT_KEEP_CAPS_LOCKED
        | SECBIT_NO_SETUID_FIXUP
        | SECBIT_NO_SETUID_FIXUP_LOCKED
        | SECBIT_NOROOT
        | SECBIT_NOROOT_LOCKED
        | SECBIT_NO_CAP_AMBIENT_RAISE
        | SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;

    let task = ctx.task();
    match ctx.args[0] {
        PR_SET_PDEATHSIG => {
            let raw = ctx.args[1] as i32;
            if raw >= 0 && raw > 64 {
                return Err(Errno::EINVAL);
            }
            task.set_pdeathsig(raw);
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            let addr = ctx.args[1];
            if addr == 0 {
                return Err(Errno::EFAULT);
            }
            copy_to_user(addr, &(task.pdeathsig() as u32).to_le_bytes())
                .map_err(|e| e.as_errno())?;
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(task.dumpable() as usize),
        PR_SET_DUMPABLE => {
            let value = ctx.args[1];
            if value > 2 {
                return Err(Errno::EINVAL);
            }
            task.set_dumpable(value as u8);
            Ok(0)
        }
        PR_GET_KEEPCAPS => Ok(task.keepcaps() as usize),
        PR_SET_KEEPCAPS => {
            let enabled = ctx.args[1] != 0;
            let creds = task.credentials();
            if enabled && creds.euid != Uid::ROOT {
                return Err(Errno::EPERM);
            }
            task.set_keepcaps(enabled);
            Ok(0)
        }
        PR_SET_NAME => {
            let name_user = ctx.args[1];
            if name_user == 0 {
                return Err(Errno::EFAULT);
            }
            let mut raw = [0u8; sched::TASK_COMM_LEN];
            copy_from_user(name_user, &mut raw).map_err(|e| e.as_errno())?;
            task.set_comm(&raw);
            Ok(0)
        }
        PR_GET_NAME => {
            let buf = ctx.args[1];
            if buf != 0 {
                copy_to_user(buf, &task.comm()).map_err(|e| e.as_errno())?;
            }
            Ok(0)
        }
        PR_CAPBSET_READ => {
            let cap = ctx.args[1] as u32;
            if !linux_cap_valid(cap) {
                return Err(Errno::EINVAL);
            }
            let creds = task.credentials();
            Ok(((creds.cap_bset.raw() >> cap) & 1) as usize)
        }
        PR_CAPBSET_DROP => {
            let cap = ctx.args[1] as u32;
            if !linux_cap_valid(cap) {
                return Err(Errno::EINVAL);
            }
            let current = task.credentials();
            if !current.has_cap(Capability::Setpcap) {
                return Err(Errno::EPERM);
            }
            let mut new = (*current).clone();
            new.cap_bset = CapSet::from_raw(new.cap_bset.raw() & !(1u64 << cap));
            install_credentials(task, new);
            Ok(0)
        }
        PR_GET_TSC => Ok(prctl_misc(ctx.task()).tsc_mode.load(Ordering::Acquire) as usize),
        PR_SET_TSC => {
            let flag = ctx.args[1];
            // PR_TSC_ENABLE=1 / PR_TSC_SIGSEGV=2。
            if flag != 1 && flag != 2 {
                return Err(Errno::EINVAL);
            }
            prctl_misc(ctx.task())
                .tsc_mode
                .store(flag as u8, Ordering::Release);
            Ok(0)
        }
        PR_GET_SECUREBITS => Ok(task.credentials().securebits as usize),
        PR_SET_SECUREBITS => {
            let bits = ctx.args[1] as u32;
            if bits & !SECBIT_ALL != 0 {
                return Err(Errno::EINVAL);
            }
            let current = task.credentials();
            if current.securebits & SECBIT_LOCKED != 0 {
                // 已锁定位不可再改。
                if current.securebits & SECBIT_LOCKED != bits & SECBIT_LOCKED {
                    return Err(Errno::EPERM);
                }
            }
            if bits & SECBIT_LOCKED != 0 && !current.has_cap(Capability::Setpcap) {
                return Err(Errno::EPERM);
            }
            let mut new = (*current).clone();
            new.securebits = bits;
            install_credentials(task, new);
            Ok(0)
        }
        PR_SET_TIMERSLACK => {
            task.set_timer_slack_ns(ctx.args[1] as u64);
            Ok(0)
        }
        PR_GET_TIMERSLACK => Ok(task.timer_slack_ns().min(usize::MAX as u64) as usize),
        PR_SET_MM => prctl_set_mm(ctx.task(), ctx.args[1], ctx.args[2]),
        PR_SET_CHILD_SUBREAPER => {
            task.set_subreaper(ctx.args[1] != 0);
            Ok(0)
        }
        PR_GET_CHILD_SUBREAPER => {
            let addr = ctx.args[1];
            if addr == 0 {
                return Err(Errno::EFAULT);
            }
            copy_to_user(addr, &(task.is_subreaper() as u32).to_le_bytes())
                .map_err(|e| e.as_errno())?;
            Ok(0)
        }
        PR_SET_NO_NEW_PRIVS => {
            if ctx.args[1] != 0 && ctx.args[1] != 1 {
                return Err(Errno::EINVAL);
            }
            if ctx.args[1] == 1 {
                task.set_no_new_privs(true);
            }
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => Ok(task.no_new_privs() as usize),
        PR_SET_THP_DISABLE => {
            let value = ctx.args[1];
            if value > 1 {
                return Err(Errno::EINVAL);
            }
            prctl_misc(ctx.task())
                .thp_disable
                .store(value as u8, Ordering::Release);
            Ok(0)
        }
        PR_GET_THP_DISABLE => {
            Ok(prctl_misc(ctx.task()).thp_disable.load(Ordering::Acquire) as usize)
        }
        PR_GET_SECCOMP => Ok(seccomp_state(task).mode() as usize),
        PR_SET_SECCOMP => {
            // 老式 PR_SET_SECCOMP：arg2 是 SECCOMP_MODE_*（STRICT=1 / FILTER=2），
            // 与 seccomp(2) 的 SECCOMP_SET_MODE_*（STRICT=0 / FILTER=1）不同，
            // 先换算成 SET 语义再走公共安装路径。
            use general::seccomp::{SECCOMP_MODE_FILTER, SECCOMP_MODE_STRICT};
            let mode = match ctx.args[1] as i32 {
                SECCOMP_MODE_STRICT => general::seccomp::SECCOMP_SET_MODE_STRICT as usize,
                SECCOMP_MODE_FILTER => general::seccomp::SECCOMP_SET_MODE_FILTER as usize,
                _ => return Err(Errno::EINVAL),
            };
            crate::syscalls::process::seccomp_filter_setup(ctx.task(), mode, ctx.args[2])?;
            Ok(0)
        }
        PR_GET_TID_ADDRESS => {
            let addr = ctx.args[1];
            if addr == 0 {
                return Err(Errno::EFAULT);
            }
            copy_to_user(addr, &(task.clear_child_tid() as u64).to_le_bytes())
                .map_err(|e| e.as_errno())?;
            Ok(0)
        }
        PR_SET_PTRACER => {
            // yama LSM 的追踪者作用域：0 = PR_SET_PTRACER_ANY，其余为指定 pid。
            // 取舍：持久化作用域值；ptrace_may_access 目前只做 uid/dumpable 检查，
            // 未按作用域过滤（见 sched::operation::ptrace_may_access）。
            task.set_ptracer_scope(ctx.args[1] as i32);
            Ok(0)
        }
        PR_GET_PTRACER => {
            let addr = ctx.args[1];
            if addr == 0 {
                return Err(Errno::EFAULT);
            }
            copy_to_user(addr, &(task.ptracer_scope() as u32).to_le_bytes())
                .map_err(|e| e.as_errno())?;
            Ok(0)
        }
        PR_CAP_AMBIENT => prctl_cap_ambient(task, ctx.args[1], ctx.args[2]),
        PR_SET_VMA => prctl_set_vma(task, ctx.args[1], ctx.args[2], ctx.args[3]),
        PR_GET_AUXV => prctl_get_auxv(task, ctx.args[1], ctx.args[2]),
        PR_GET_SPECULATION_CTRL => prctl_get_speculation_ctrl(task, ctx.args[1], ctx.args[2]),
        PR_SET_SPECULATION_CTRL => prctl_set_speculation_ctrl(task, ctx.args[1], ctx.args[2]),
        _ => Err(Errno::EINVAL),
    }
}

/// 每个任务/进程的 `prctl` 杂项状态（TSC 模式、THP 开关）。
pub(crate) const TASKEXT_PRCTL_MISC: sched::TaskExtKey = 0x0004_0005;

/// `prctl` 持久化的进程级杂项状态（Linux `PR_SET_TSC`/`PR_SET_THP_DISABLE`）。
pub(crate) struct PrctlMiscState {
    /// `PR_TSC_ENABLE=1` / `PR_TSC_SIGSEGV=2`。
    pub(crate) tsc_mode: AtomicU8,
    /// `PR_SET_THP_DISABLE` 的 0/1。
    pub(crate) thp_disable: AtomicU8,
}

impl PrctlMiscState {
    pub(crate) fn new() -> Self {
        Self {
            tsc_mode: AtomicU8::new(1), // 默认 PR_TSC_ENABLE
            thp_disable: AtomicU8::new(0),
        }
    }
}

/// 取任务的 prctl 杂项状态（惰性创建并挂载）。
fn prctl_misc(task: &Arc<Task>) -> Arc<PrctlMiscState> {
    if let Some(state) = task
        .ext_lookup(TASKEXT_PRCTL_MISC)
        .and_then(|payload| payload.downcast::<PrctlMiscState>().ok())
    {
        return state;
    }
    let state = Arc::new(PrctlMiscState::new());
    let erased: Arc<dyn core::any::Any + Send + Sync> = state.clone();
    task.ext_install(TASKEXT_PRCTL_MISC, erased);
    state
}

/// exec 时捕获的 auxv（`PR_GET_AUXV` 读取）。由 `crate::exec` 安装，
/// 与 `TASKEXT_EXEC_PATH/ARGS/ENVP` 同代。
pub(crate) const TASKEXT_EXEC_AUXV: sched::TaskExtKey = 0x0002_0004;
/// `PR_SET_MM` 持久化的 mm 边界字段（CRIU 恢复用）。
pub(crate) const TASKEXT_PRCTL_MM: sched::TaskExtKey = 0x0004_0006;

/// `PR_SET_MM` 的 mm 边界字段。Linux 把这些值存在 `mm_struct` 上；
/// 本内核无独立 `mm_struct`，用任务扩展保存。fork 时扩展被共享（同 Arc），
/// 但 CRIU 恢复在 exec 后、fork 前完成，实际路径不受影响。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PrctlMmFields {
    pub start_code: u64,
    pub end_code: u64,
    pub start_data: u64,
    pub end_data: u64,
    pub start_stack: u64,
    pub start_brk: u64,
    pub brk: u64,
    pub arg_start: u64,
    pub arg_end: u64,
    pub env_start: u64,
    pub env_end: u64,
}

fn prctl_mm_state(task: &Arc<Task>) -> Arc<Spinlock<PrctlMmFields>> {
    if let Some(state) = task
        .ext_lookup(TASKEXT_PRCTL_MM)
        .and_then(|payload| payload.downcast::<Spinlock<PrctlMmFields>>().ok())
    {
        return state;
    }
    let state = Arc::new(Spinlock::new(PrctlMmFields::default()));
    let erased: Arc<dyn core::any::Any + Send + Sync> = state.clone();
    task.ext_install(TASKEXT_PRCTL_MM, erased);
    state
}

/// `PR_SET_MM`：CRIU 依赖的 mm 边界字段恢复。
///
/// Linux 要求 `CAP_SYS_RESOURCE`。实现 `PR_SET_MM_MAP`（CRIU 主路径）与
/// `PR_SET_MM_{START,END}_{CODE,DATA,STACK,BRK}`、`PR_SET_MM_ARG_*`、
/// `PR_SET_MM_ENV_*` 标量项，并把 `PR_SET_MM_MAP_SIZE` 报告为 `sizeof(prctl_mm_map)`。
/// 取舍：`PR_SET_MM_AUXV`/`PR_SET_MM_EXE_FILE` 只做基本校验不落地（auxv 由
/// exec 捕获、exe 由 `TASKEXT_EXEC_PATH` 维护），且这些字段不参与实际 mm 布局，
/// 仅持久化供 CRIU 回读。
fn prctl_set_mm(task: &Arc<Task>, option: usize, value: usize) -> Result<usize, Errno> {
    const PR_SET_MM_START_CODE: usize = 1;
    const PR_SET_MM_END_CODE: usize = 2;
    const PR_SET_MM_START_DATA: usize = 3;
    const PR_SET_MM_END_DATA: usize = 4;
    const PR_SET_MM_START_STACK: usize = 5;
    const PR_SET_MM_START_BRK: usize = 6;
    const PR_SET_MM_BRK: usize = 7;
    const PR_SET_MM_ARG_START: usize = 8;
    const PR_SET_MM_ARG_END: usize = 9;
    const PR_SET_MM_ENV_START: usize = 10;
    const PR_SET_MM_ENV_END: usize = 11;
    const PR_SET_MM_AUXV: usize = 12;
    const PR_SET_MM_EXE_FILE: usize = 13;
    const PR_SET_MM_MAP: usize = 14;
    const PR_SET_MM_MAP_SIZE: usize = 15;

    if !task.credentials().has_cap(Capability::SysResource) {
        return Err(Errno::EPERM);
    }
    let state = prctl_mm_state(task);
    let mut fields = state.lock();
    match option {
        PR_SET_MM_START_CODE => fields.start_code = value as u64,
        PR_SET_MM_END_CODE => fields.end_code = value as u64,
        PR_SET_MM_START_DATA => fields.start_data = value as u64,
        PR_SET_MM_END_DATA => fields.end_data = value as u64,
        PR_SET_MM_START_STACK => fields.start_stack = value as u64,
        PR_SET_MM_START_BRK => fields.start_brk = value as u64,
        PR_SET_MM_BRK => fields.brk = value as u64,
        PR_SET_MM_ARG_START => fields.arg_start = value as u64,
        PR_SET_MM_ARG_END => fields.arg_end = value as u64,
        PR_SET_MM_ENV_START => fields.env_start = value as u64,
        PR_SET_MM_ENV_END => fields.env_end = value as u64,
        PR_SET_MM_AUXV => {
            // CRIU 用它传自定义 auxv；本内核 auxv 由 exec 捕获，只校验地址
            // 非空，不回写 mm 布局。
            if value == 0 {
                return Err(Errno::EINVAL);
            }
        }
        PR_SET_MM_EXE_FILE => {
            // 校验 fd 有效即可；不替换 TASKEXT_EXEC_PATH（见取舍注释）。
            if value != usize::MAX && task_fdtable(task).is_none() {
                return Err(Errno::EBADF);
            }
        }
        PR_SET_MM_MAP => {
            if value == 0 {
                return Err(Errno::EFAULT);
            }
            let mut raw = [0u8; 88];
            copy_from_user(value, &mut raw).map_err(|e| e.as_errno())?;
            let read = |off: usize| u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
            let start_code = read(0);
            let end_code = read(8);
            let start_data = read(16);
            let end_data = read(24);
            let start_brk = read(32);
            let brk = read(40);
            let start_stack = read(48);
            let arg_start = read(56);
            let arg_end = read(64);
            let env_start = read(72);
            let env_end = read(80);
            // Linux 校验各区间单调不重叠（`validate_prctl_map_addr`）。
            if start_code > end_code
                || end_code > start_data
                || start_data > end_data
                || end_data > start_brk
                || start_brk > brk
                || brk > start_stack
                || start_stack > arg_start
                || arg_start > arg_end
                || arg_end > env_start
                || env_start > env_end
            {
                return Err(Errno::EINVAL);
            }
            fields.start_code = start_code;
            fields.end_code = end_code;
            fields.start_data = start_data;
            fields.end_data = end_data;
            fields.start_brk = start_brk;
            fields.brk = brk;
            fields.start_stack = start_stack;
            fields.arg_start = arg_start;
            fields.arg_end = arg_end;
            fields.env_start = env_start;
            fields.env_end = env_end;
        }
        PR_SET_MM_MAP_SIZE => {
            return Ok(core::mem::size_of::<[u64; 11]>() + 8); // 11 * u64 + auxv_size/exe_fd
        }
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}

/// `PR_CAP_AMBIENT`：ambient capability 集管理。
fn prctl_cap_ambient(task: &Arc<Task>, sub: usize, cap_raw: usize) -> Result<usize, Errno> {
    const PR_CAP_AMBIENT_IS_SET: usize = 1;
    const PR_CAP_AMBIENT_RAISE: usize = 2;
    const PR_CAP_AMBIENT_LOWER: usize = 3;
    const PR_CAP_AMBIENT_CLEAR_ALL: usize = 4;

    let cap = cap_raw as u32;
    let current = task.credentials();
    let mut new = (*current).clone();
    match sub {
        PR_CAP_AMBIENT_IS_SET => {
            if !linux_cap_valid(cap) {
                return Err(Errno::EINVAL);
            }
            Ok((new.cap_ambient.raw() >> cap & 1) as usize)
        }
        PR_CAP_AMBIENT_RAISE => {
            if !linux_cap_valid(cap) {
                return Err(Errno::EINVAL);
            }
            // Linux：能力必须已在 inheritable 与 permitted 中；无 CAP_SETPCAP
            // 时只能提升已经同时存在于两者中的能力。
            let bit = 1u64 << cap;
            if new.cap_inheritable.raw() & bit == 0 || new.cap_permitted.raw() & bit == 0 {
                return Err(Errno::EPERM);
            }
            if !new.has_cap(Capability::Setpcap) {
                return Err(Errno::EPERM);
            }
            new.cap_ambient = CapSet::from_raw(new.cap_ambient.raw() | bit);
            install_credentials(task, new);
            Ok(0)
        }
        PR_CAP_AMBIENT_LOWER => {
            if !linux_cap_valid(cap) {
                return Err(Errno::EINVAL);
            }
            new.cap_ambient = CapSet::from_raw(new.cap_ambient.raw() & !(1u64 << cap));
            install_credentials(task, new);
            Ok(0)
        }
        PR_CAP_AMBIENT_CLEAR_ALL => {
            new.cap_ambient = CapSet::EMPTY;
            install_credentials(task, new);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `PR_SET_VMA`：给匿名 VMA 区间命名（`PR_SET_VMA_ANON_NAME`）。
///
/// 取舍：本内核 VMA 无 anon_name 存储（`general::mm::VmSpace` 只读边界），
/// 因此只校验区间与名称，不落地；返回 0 以兼容依赖该 prctl 的运行时。
fn prctl_set_vma(
    _task: &Arc<Task>,
    addr: usize,
    size: usize,
    name_user: usize,
) -> Result<usize, Errno> {
    if size == 0 || addr > usize::MAX - size {
        return Err(Errno::EINVAL);
    }
    let _name = if name_user == 0 {
        return Err(Errno::EFAULT);
    } else {
        copy_cstr_from_user(name_user, 256).map_err(|e| e.as_errno())?
    };
    Ok(0)
}

/// `PR_GET_AUXV`：把 exec 捕获的 auxv 拷回用户态。
fn prctl_get_auxv(task: &Arc<Task>, user: usize, size: usize) -> Result<usize, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let auxv = task
        .ext_lookup(TASKEXT_EXEC_AUXV)
        .and_then(|payload| payload.downcast::<Spinlock<Vec<u64>>>().ok());
    let Some(auxv) = auxv else {
        // 尚未 exec（或非 Tomori 进程）：无 auxv，按空返回。
        return Ok(0);
    };
    let bytes = auxv.lock();
    let total = bytes.len() * 8;
    let n = size.min(total);
    if n > 0 {
        let mut out = alloc::vec![0u8; n];
        for (index, chunk) in out.chunks_mut(8).enumerate() {
            chunk.copy_from_slice(&bytes[index].to_le_bytes());
        }
        copy_to_user(user, &out).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

/// `PR_GET_SPECULATION_CTRL`：读回投机执行控制位（按 feature 3 位打包）。
fn prctl_get_speculation_ctrl(
    task: &Arc<Task>,
    feature: usize,
    out_user: usize,
) -> Result<usize, Errno> {
    const PR_SPEC_STORE_BYPASS: usize = 0;
    const PR_SPEC_INDIRECT_BRANCH: usize = 1;
    const PR_SPEC_L1D_FLUSH: usize = 2;
    if feature > PR_SPEC_L1D_FLUSH || out_user == 0 {
        return Err(Errno::EINVAL);
    }
    let value = (task.speculation_ctrl() >> (feature * 3)) & 0b111;
    copy_to_user(out_user, &(value as u32).to_le_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

/// `PR_SET_SPECULATION_CTRL`：记录投机执行控制位（不施加实际硬件缓解）。
fn prctl_set_speculation_ctrl(
    task: &Arc<Task>,
    feature: usize,
    value: usize,
) -> Result<usize, Errno> {
    const PR_SPEC_STORE_BYPASS: usize = 0;
    const PR_SPEC_INDIRECT_BRANCH: usize = 1;
    const PR_SPEC_L1D_FLUSH: usize = 2;
    const PR_SPEC_ENABLE: usize = 0;
    const PR_SPEC_DISABLE: usize = 1;
    const PR_SPEC_FORCE_DISABLE: usize = 2;
    const PR_SPEC_DISABLE_NOEXEC: usize = 4;
    if feature > PR_SPEC_L1D_FLUSH {
        return Err(Errno::EINVAL);
    }
    if value & !0b111 != 0
        || !matches!(
            value & 0b111,
            PR_SPEC_ENABLE | PR_SPEC_DISABLE | PR_SPEC_FORCE_DISABLE | PR_SPEC_DISABLE_NOEXEC
        )
    {
        return Err(Errno::EINVAL);
    }
    // 取舍：内核未实现投机执行缓解，仅持久化位图供回读（见 process.rs 顶部说明）。
    let shift = feature * 3;
    let mask = 0b111u64 << shift;
    let current = task.speculation_ctrl();
    task.set_speculation_ctrl((current & !mask) | ((value as u64 & 0b111) << shift));
    Ok(0)
}

/// `PR_SET_SECCOMP`：老式 seccomp 入口，等价 `seccomp(2)`。
pub(super) fn seccomp_filter_setup(
    task: &Arc<Task>,
    mode: usize,
    filter_user: usize,
) -> Result<(), Errno> {
    use general::seccomp::*;
    match mode as u32 {
        SECCOMP_SET_MODE_STRICT => {
            seccomp_state(task).set_strict();
            Ok(())
        }
        SECCOMP_SET_MODE_FILTER => {
            let cred = vfs_cred_from_sched(&task.credentials());
            if !filter_install_allowed(task.no_new_privs(), &cred) {
                return Err(Errno::EACCES);
            }
            let mut fprog = [0u8; 16];
            copy_from_user(filter_user, &mut fprog).map_err(|e| e.as_errno())?;
            let len = u16::from_le_bytes(fprog[0..2].try_into().unwrap()) as usize;
            let ptr = u64::from_le_bytes(fprog[8..16].try_into().unwrap()) as usize;
            let mut bytes = vec![0u8; len * 8];
            if len > 0 {
                copy_from_user(ptr, &mut bytes).map_err(|e| e.as_errno())?;
            }
            let insns = parse_program(&bytes)?;
            let filter = SeccompFilter::new(insns, 0)?;
            seccomp_state(task).push_filter(filter);
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sys_capget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let hdrp = ctx.args[0];
    let datap = ctx.args[1];
    let (version, pid) = read_cap_header(hdrp)?;
    let words = cap_version_words(version)?;
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    if pid != 0 && Some(pid) != ctx.task().pid_root() {
        return Err(Errno::ESRCH);
    }
    if datap != 0 {
        let creds = ctx.task().credentials();
        write_cap_data(datap, words, &creds)?;
    }
    Ok(0)
}

pub(super) fn sys_capset(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let hdrp = ctx.args[0];
    let datap = ctx.args[1];
    if datap == 0 {
        return Err(Errno::EFAULT);
    }
    let (version, pid) = read_cap_header(hdrp)?;
    let words = cap_version_words(version)?;
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    if pid != 0 && Some(pid) != ctx.task().pid_root() {
        return Err(Errno::EPERM);
    }
    let requested = read_cap_data(datap, words)?;
    let current = ctx.task().credentials();
    validate_capset(requested, &current)?;
    let mut new = (*current).clone();
    new.caps = requested.effective;
    new.cap_permitted = requested.permitted;
    new.cap_inheritable = requested.inheritable;
    install_credentials(ctx.task(), new);
    Ok(0)
}

const LINUX_CAPABILITY_VERSION_1: u32 = 0x1998_0330;
const LINUX_CAPABILITY_VERSION_2: u32 = 0x2007_1026;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const LINUX_CAP_LAST_CAP: u32 = 40;
const LINUX_CAP_VALID_MASK: u64 = (1u64 << (LINUX_CAP_LAST_CAP + 1)) - 1;

#[derive(Clone, Copy)]
struct LinuxCapData {
    effective: CapSet,
    permitted: CapSet,
    inheritable: CapSet,
}

fn linux_cap_valid(cap: u32) -> bool {
    cap <= LINUX_CAP_LAST_CAP
}

fn valid_linux_caps() -> CapSet {
    CapSet::from_raw(LINUX_CAP_VALID_MASK)
}

fn read_cap_header(user: usize) -> Result<(u32, i32), Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 8];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let version = u32::from_le_bytes(raw[0..4].try_into().unwrap());
    let pid = i32::from_le_bytes(raw[4..8].try_into().unwrap());
    if cap_version_words(version).is_err() {
        raw[0..4].copy_from_slice(&LINUX_CAPABILITY_VERSION_3.to_le_bytes());
        raw[4..8].copy_from_slice(&0i32.to_le_bytes());
        let _ = copy_to_user(user, &raw);
        return Err(Errno::EINVAL);
    }
    Ok((version, pid))
}

fn cap_version_words(version: u32) -> Result<usize, Errno> {
    match version {
        LINUX_CAPABILITY_VERSION_1 => Ok(1),
        LINUX_CAPABILITY_VERSION_2 | LINUX_CAPABILITY_VERSION_3 => Ok(2),
        _ => Err(Errno::EINVAL),
    }
}

fn write_cap_data(user: usize, words: usize, creds: &Credentials) -> Result<(), Errno> {
    let mut raw = [0u8; 24];
    let valid = LINUX_CAP_VALID_MASK;
    let caps = [
        creds.caps.raw() & valid,
        creds.cap_permitted.raw() & valid,
        creds.cap_inheritable.raw() & valid,
    ];
    for i in 0..words {
        let shift = i * 32;
        let off = i * 12;
        for (j, value) in caps.iter().enumerate() {
            let word = ((value >> shift) & u32::MAX as u64) as u32;
            raw[off + j * 4..off + j * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    copy_to_user(user, &raw[..words * 12]).map_err(|e| e.as_errno())
}

fn read_cap_data(user: usize, words: usize) -> Result<LinuxCapData, Errno> {
    let mut raw = [0u8; 24];
    copy_from_user(user, &mut raw[..words * 12]).map_err(|e| e.as_errno())?;
    let mut effective = 0u64;
    let mut permitted = 0u64;
    let mut inheritable = 0u64;
    for i in 0..words {
        let off = i * 12;
        let eff = u32::from_le_bytes(raw[off..off + 4].try_into().unwrap()) as u64;
        let prm = u32::from_le_bytes(raw[off + 4..off + 8].try_into().unwrap()) as u64;
        let inh = u32::from_le_bytes(raw[off + 8..off + 12].try_into().unwrap()) as u64;
        effective |= eff << (i * 32);
        permitted |= prm << (i * 32);
        inheritable |= inh << (i * 32);
    }
    Ok(LinuxCapData {
        effective: CapSet::from_raw(effective),
        permitted: CapSet::from_raw(permitted),
        inheritable: CapSet::from_raw(inheritable),
    })
}

fn validate_capset(requested: LinuxCapData, current: &Credentials) -> Result<(), Errno> {
    let valid = valid_linux_caps();
    let requested_all =
        requested.effective.raw() | requested.permitted.raw() | requested.inheritable.raw();
    if (requested_all & !valid.raw()) != 0 {
        return Err(Errno::EPERM);
    }

    if !requested.permitted.contains_all(requested.effective) {
        return Err(Errno::EPERM);
    }

    let old_permitted = current.cap_permitted.mask(valid);
    if !old_permitted.contains_all(requested.permitted) {
        return Err(Errno::EPERM);
    }

    let old_inheritable = current.cap_inheritable.mask(valid);
    let inheritable_from = if current.has_cap(Capability::Setpcap) {
        old_inheritable.raw() | current.cap_bset.mask(valid).raw()
    } else {
        old_inheritable.raw() | old_permitted.raw()
    };
    if (requested.inheritable.raw() & !inheritable_from) != 0 {
        return Err(Errno::EPERM);
    }

    Ok(())
}

fn install_credentials(task: &Arc<Task>, new: Credentials) {
    let sched_cred = Arc::new(new);
    task.set_credentials(Arc::clone(&sched_cred));
    if let Some(vfs_ctx) = vfs::current_vfs_context() {
        vfs_ctx.set_cred(Arc::new(vfs_cred_from_sched(&sched_cred)));
    }
}

const LINUX_FS_CAP_MASK: u64 = (1u64 << Capability::Chown as u32)
    | (1u64 << Capability::DacOverride as u32)
    | (1u64 << Capability::DacReadSearch as u32)
    | (1u64 << Capability::Fowner as u32)
    | (1u64 << Capability::Fsetid as u32)
    | (1u64 << 9) // CAP_LINUX_IMMUTABLE
    | (1u64 << 27) // CAP_MKNOD
    | (1u64 << 32); // CAP_MAC_OVERRIDE

fn drop_caps_after_uid_gid_change(old: &Credentials, new: &mut Credentials) {
    let lost_root_uid = (old.uid == Uid::ROOT || old.euid == Uid::ROOT || old.suid == Uid::ROOT)
        && new.uid != Uid::ROOT
        && new.euid != Uid::ROOT
        && new.suid != Uid::ROOT;
    let lost_effective_root = old.euid == Uid::ROOT && new.euid != Uid::ROOT;
    let gained_effective_root = old.euid != Uid::ROOT && new.euid == Uid::ROOT;
    if lost_root_uid {
        new.caps = CapSet::EMPTY;
        new.cap_permitted = CapSet::EMPTY;
        new.cap_inheritable = CapSet::EMPTY;
    } else if lost_effective_root {
        new.caps = CapSet::EMPTY;
    } else if gained_effective_root {
        new.caps = new.cap_permitted;
    }

    // Linux 将 fsuid=0 视为文件系统能力的开关，但不改变 permitted 集合。
    if old.fsuid == Uid::ROOT && new.fsuid != Uid::ROOT {
        new.caps = CapSet::from_raw(new.caps.raw() & !LINUX_FS_CAP_MASK);
    } else if old.fsuid != Uid::ROOT && new.fsuid == Uid::ROOT {
        new.caps = CapSet::from_raw(new.caps.raw() | (new.cap_permitted.raw() & LINUX_FS_CAP_MASK));
    }
}

pub(super) fn sys_setuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uid = Uid(ctx.args[0] as u32);
    if uid.0 == u32::MAX {
        return Err(Errno::EINVAL);
    }
    let creds = ctx.task().credentials();
    let mut new = (*creds).clone();
    if creds.has_cap(Capability::Setuid) {
        new.uid = uid;
        new.euid = uid;
        new.suid = uid;
        new.fsuid = uid;
    } else if uid == creds.uid || uid == creds.suid {
        new.euid = uid;
        new.fsuid = uid;
    } else {
        return Err(Errno::EPERM);
    }
    drop_caps_after_uid_gid_change(&creds, &mut new);
    install_credentials(ctx.task(), new);
    Ok(0)
}

pub(super) fn sys_setgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let gid = Gid(ctx.args[0] as u32);
    if gid.0 == u32::MAX {
        return Err(Errno::EINVAL);
    }
    let creds = ctx.task().credentials();
    let mut new = (*creds).clone();
    if creds.has_cap(Capability::Setgid) {
        new.gid = gid;
        new.egid = gid;
        new.sgid = gid;
        new.fsgid = gid;
    } else if gid == creds.gid || gid == creds.sgid {
        new.egid = gid;
        new.fsgid = gid;
    } else {
        return Err(Errno::EPERM);
    }
    drop_caps_after_uid_gid_change(&creds, &mut new);
    install_credentials(ctx.task(), new);
    Ok(0)
}

pub(super) fn sys_setreuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let ruid = ctx.args[0] as u32;
    let euid = ctx.args[1] as u32;
    let creds = ctx.task().credentials();
    let privileged = creds.has_cap(Capability::Setuid);
    let mut new = (*creds).clone();
    if ruid != u32::MAX {
        if !privileged && ruid != creds.uid.0 && ruid != creds.euid.0 {
            return Err(Errno::EPERM);
        }
        new.uid = Uid(ruid);
    }
    if euid != u32::MAX {
        if !privileged && euid != creds.uid.0 && euid != creds.euid.0 && euid != creds.suid.0 {
            return Err(Errno::EPERM);
        }
        let new_euid = Uid(euid);
        new.euid = new_euid;
        new.fsuid = new_euid;
    }
    if ruid != u32::MAX || (euid != u32::MAX && euid != creds.uid.0) {
        new.suid = new.euid;
    }
    drop_caps_after_uid_gid_change(&creds, &mut new);
    install_credentials(ctx.task(), new);
    Ok(0)
}

pub(super) fn sys_setregid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let rgid = ctx.args[0] as u32;
    let egid = ctx.args[1] as u32;
    let creds = ctx.task().credentials();
    let privileged = creds.has_cap(Capability::Setgid);
    let mut new = (*creds).clone();
    if rgid != u32::MAX {
        if !privileged && rgid != creds.gid.0 && rgid != creds.egid.0 {
            return Err(Errno::EPERM);
        }
        new.gid = Gid(rgid);
    }
    if egid != u32::MAX {
        if !privileged && egid != creds.gid.0 && egid != creds.egid.0 && egid != creds.sgid.0 {
            return Err(Errno::EPERM);
        }
        let new_egid = Gid(egid);
        new.egid = new_egid;
        new.fsgid = new_egid;
    }
    if rgid != u32::MAX || (egid != u32::MAX && egid != creds.gid.0) {
        new.sgid = new.egid;
    }
    drop_caps_after_uid_gid_change(&creds, &mut new);
    install_credentials(ctx.task(), new);
    Ok(0)
}

pub(super) fn sys_setresuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let ruid = ctx.args[0] as u32;
    let euid = ctx.args[1] as u32;
    let suid = ctx.args[2] as u32;
    let creds = ctx.task().credentials();
    if !creds.has_cap(Capability::Setuid) {
        if ruid != u32::MAX && ruid != creds.uid.0 && ruid != creds.euid.0 && ruid != creds.suid.0 {
            return Err(Errno::EPERM);
        }
        if euid != u32::MAX && euid != creds.uid.0 && euid != creds.euid.0 && euid != creds.suid.0 {
            return Err(Errno::EPERM);
        }
        if suid != u32::MAX && suid != creds.uid.0 && suid != creds.euid.0 && suid != creds.suid.0 {
            return Err(Errno::EPERM);
        }
    }
    let mut new = (*creds).clone();
    if ruid != u32::MAX {
        new.uid = Uid(ruid);
    }
    if euid != u32::MAX {
        new.euid = Uid(euid);
    }
    if suid != u32::MAX {
        new.suid = Uid(suid);
    }
    new.fsuid = new.euid;
    drop_caps_after_uid_gid_change(&creds, &mut new);
    install_credentials(ctx.task(), new);
    Ok(0)
}

pub(super) fn sys_setresgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let rgid = ctx.args[0] as u32;
    let egid = ctx.args[1] as u32;
    let sgid = ctx.args[2] as u32;
    let creds = ctx.task().credentials();
    if !creds.has_cap(Capability::Setgid) {
        if rgid != u32::MAX && rgid != creds.gid.0 && rgid != creds.egid.0 && rgid != creds.sgid.0 {
            return Err(Errno::EPERM);
        }
        if egid != u32::MAX && egid != creds.gid.0 && egid != creds.egid.0 && egid != creds.sgid.0 {
            return Err(Errno::EPERM);
        }
        if sgid != u32::MAX && sgid != creds.gid.0 && sgid != creds.egid.0 && sgid != creds.sgid.0 {
            return Err(Errno::EPERM);
        }
    }
    let mut new = (*creds).clone();
    if rgid != u32::MAX {
        new.gid = Gid(rgid);
    }
    if egid != u32::MAX {
        new.egid = Gid(egid);
    }
    if sgid != u32::MAX {
        new.sgid = Gid(sgid);
    }
    new.fsgid = new.egid;
    drop_caps_after_uid_gid_change(&creds, &mut new);
    install_credentials(ctx.task(), new);
    Ok(0)
}

pub(super) fn sys_setfsuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uid = Uid(ctx.args[0] as u32);
    let creds = ctx.task().credentials();
    let old = creds.fsuid;
    if uid.0 != u32::MAX
        && (creds.has_cap(Capability::Setuid)
            || uid == creds.uid
            || uid == creds.euid
            || uid == creds.suid
            || uid == creds.fsuid)
    {
        let mut new = (*creds).clone();
        new.fsuid = uid;
        drop_caps_after_uid_gid_change(&creds, &mut new);
        install_credentials(ctx.task(), new);
    }
    Ok(old.0 as usize)
}

pub(super) fn sys_setfsgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let gid = Gid(ctx.args[0] as u32);
    let creds = ctx.task().credentials();
    let old = creds.fsgid;
    if gid.0 != u32::MAX
        && (creds.has_cap(Capability::Setgid)
            || gid == creds.gid
            || gid == creds.egid
            || gid == creds.sgid
            || gid == creds.fsgid)
    {
        let mut new = (*creds).clone();
        new.fsgid = gid;
        install_credentials(ctx.task(), new);
    }
    Ok(old.0 as usize)
}

pub(super) fn sys_getgroups(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let size = ctx.args[0];
    let list = ctx.args[1];
    let creds = ctx.task().credentials();
    if size == 0 {
        return Ok(creds.groups.len());
    }
    if (size as usize) < creds.groups.len() {
        return Err(Errno::EINVAL);
    }
    for (i, g) in creds.groups.iter().enumerate() {
        let off = list + i * 4;
        copy_to_user(off, &g.0.to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    Ok(creds.groups.len())
}

pub(super) fn sys_setgroups(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let size = ctx.args[0];
    let list = ctx.args[1];
    let creds = ctx.task().credentials();
    if creds.euid != Uid::ROOT {
        return Err(Errno::EPERM);
    }
    const NGROUPS_MAX: usize = 65536;
    if size > NGROUPS_MAX {
        return Err(Errno::EINVAL);
    }
    let mut groups = Vec::new();
    for i in 0..size {
        let mut raw = [0u8; 4];
        copy_from_user(list + i * 4, &mut raw).map_err(|e| e.as_errno())?;
        groups.push(Gid(u32::from_le_bytes(raw)));
    }
    let mut new = (*creds).clone();
    new.groups = groups;
    install_credentials(ctx.task(), new);
    Ok(0)
}

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_REQUEUE: u32 = 3;
const FUTEX_CMP_REQUEUE: u32 = 4;
const FUTEX_WAKE_OP: u32 = 5;
const FUTEX_LOCK_PI: u32 = 6;
const FUTEX_UNLOCK_PI: u32 = 7;
const FUTEX_TRYLOCK_PI: u32 = 8;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_WAIT_REQUEUE_PI: u32 = 11;
const FUTEX_CMP_REQUEUE_PI: u32 = 12;
const FUTEX_LOCK_PI2: u32 = 13;
const FUTEX_PRIVATE_FLAG: u32 = 128;
const FUTEX_CLOCK_REALTIME: u32 = 256;
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
const ROBUST_LIST_HEAD_SIZE: usize = 24;
const ROBUST_LIST_LIMIT: usize = 2048;

/// exec 在 PONR 前预分配的 robust futex 遍历空间。
pub(crate) struct ExecCleanupScratch {
    robust_visited: Vec<usize>,
    pi_handoffs: Vec<ExecPiHandoff>,
    pi_handoff_overflow: bool,
}

struct ExecPiHandoff {
    task: Arc<Task>,
    token: usize,
    attr: SchedAttr,
}

impl ExecCleanupScratch {
    pub(crate) fn prepare() -> Result<Self, Errno> {
        let mut robust_visited = Vec::new();
        robust_visited
            .try_reserve_exact(ROBUST_LIST_LIMIT)
            .map_err(|_| Errno::ENOMEM)?;
        let mut pi_handoffs = Vec::new();
        let reserve = ROBUST_LIST_LIMIT.saturating_add(PI_FUTEX_TABLE.lock().len());
        pi_handoffs
            .try_reserve_exact(reserve)
            .map_err(|_| Errno::ENOMEM)?;
        Ok(Self {
            robust_visited,
            pi_handoffs,
            pi_handoff_overflow: false,
        })
    }

    pub(crate) fn has_pi_handoff_overflow(&self) -> bool {
        self.pi_handoff_overflow
    }

    pub(crate) fn apply_pi_handoffs(&mut self) -> bool {
        let mut applied = true;
        for handoff in self.pi_handoffs.drain(..) {
            if handoff
                .task
                .pi_try_add_donation(handoff.token, handoff.attr)
                .is_some()
            {
                sched::defer_pi_effective_update(&handoff.task);
            } else {
                applied = false;
                log::emergency!(
                    "[syscall][futex] exec PI donation capacity exhausted token={}",
                    handoff.token
                );
            }
        }
        applied
    }
}

type FutexKey = VmFutexKey;

struct FutexWaiter {
    task: Weak<sched::Task>,
    bitset: u32,
    waitv_index: Option<usize>,
    pi_target: Option<(FutexKey, usize)>,
    state: Arc<FutexWaitState>,
}

struct FutexWaitState {
    /// futex 等待状态机：
    /// - ARMED：waiter 已进入 futex 表，但还没有真正睡下；
    /// - SLEEPING：waiter 已把 task 状态切到 Sleeping，可以由 wake 入队；
    /// - WOKEN：wake 事件已经到达，waiter 自己或调度器会负责收尾。
    state: core::sync::atomic::AtomicU8,
}

impl FutexWaitState {
    fn new() -> Self {
        Self {
            state: core::sync::atomic::AtomicU8::new(FUTEX_WAIT_ARMED),
        }
    }

    fn is_woken(&self) -> bool {
        self.state.load(Ordering::Acquire) == FUTEX_WAIT_WOKEN
    }

    fn mark_sleeping(&self) -> bool {
        self.state
            .compare_exchange(
                FUTEX_WAIT_ARMED,
                FUTEX_WAIT_SLEEPING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn rearm_after_non_futex_wakeup(&self) -> bool {
        self.state
            .compare_exchange(
                FUTEX_WAIT_SLEEPING,
                FUTEX_WAIT_ARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn mark_woken(&self) -> u8 {
        self.state.swap(FUTEX_WAIT_WOKEN, Ordering::AcqRel)
    }
}

struct FutexBucket {
    waiters: Vec<FutexWaiter>,
}

struct PiFutexWaiter {
    task: Weak<sched::Task>,
    state: Arc<FutexWaitState>,
    seq: usize,
}

struct PiFutexState {
    token: usize,
    owner: Weak<sched::Task>,
    vm: Weak<VmSpace>,
    uaddr: usize,
    waiters: Vec<PiFutexWaiter>,
}

const FUTEX_WAIT_ARMED: u8 = 0;
const FUTEX_WAIT_SLEEPING: u8 = 1;
const FUTEX_WAIT_WOKEN: u8 = 2;

static FUTEX_TABLE: Spinlock<BTreeMap<FutexKey, FutexBucket>> = Spinlock::new(BTreeMap::new());
static PI_FUTEX_TABLE: Spinlock<BTreeMap<FutexKey, PiFutexState>> = Spinlock::new(BTreeMap::new());
static NEXT_PI_FUTEX_TOKEN: AtomicUsize = AtomicUsize::new(1);
static NEXT_PI_WAITER_SEQ: AtomicUsize = AtomicUsize::new(1);
const PI_CHAIN_MAX: usize = 32;

fn futex_cmd(futex_op: u32) -> u32 {
    futex_op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME)
}

#[cfg(feature = "trace-task-lifecycle")]
fn trace_futex_task(task: &Task) -> bool {
    #[cfg(feature = "performance-profile")]
    {
        // 诊断范围由 profile_control 的 root=<pid> 定义，并随子进程继承；
        // 不依赖特定程序名，任意工作负载都能得到同样的 futex 因果链。
        let session = task.profile_session_id();
        session != 0 && session == profiling::session_id()
    }
    #[cfg(not(feature = "performance-profile"))]
    {
        let _ = task;
        false
    }
}

fn task_vm_space_for_futex(task: &Arc<Task>) -> Result<Arc<VmSpace>, Errno> {
    let payload = task
        .ext_lookup(sched::TASKEXT_VM_SPACE)
        .ok_or(Errno::EFAULT)?;
    payload.downcast::<VmSpace>().map_err(|_| Errno::EFAULT)
}

fn futex_key(task: &Arc<Task>, uaddr: usize, private: bool) -> Result<FutexKey, Errno> {
    task_vm_space_for_futex(task)?.futex_key_for(uaddr, private)
}

fn next_pi_token() -> usize {
    NEXT_PI_FUTEX_TOKEN.fetch_add(1, Ordering::Relaxed).max(1)
}

fn pi_urgency(attr: SchedAttr) -> u16 {
    match attr.policy {
        SchedPolicy::Deadline => 400,
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => 200 + attr.priority as u16,
        SchedPolicy::Fair | SchedPolicy::Batch => 100 + (19i16 - attr.nice as i16).max(0) as u16,
        SchedPolicy::Idle => 0,
    }
}

fn pi_best_waiter(state: &PiFutexState) -> Option<(usize, Arc<Task>)> {
    state
        .waiters
        .iter()
        .enumerate()
        .filter_map(|(index, waiter)| {
            waiter
                .task
                .upgrade()
                .filter(|task| task.pid_root().is_some_and(|pid| pid > 0))
                .map(|task| {
                    let urgency = pi_urgency(task.sched.sched_attr());
                    (index, task, urgency, usize::MAX - waiter.seq)
                })
        })
        .max_by_key(|(_, _, urgency, fifo)| (*urgency, *fifo))
        .map(|(index, task, _, _)| (index, task))
}

fn pi_owner_update(owner: &Arc<Task>, token: usize, donated: Option<SchedAttr>) {
    let effective = match donated {
        Some(attr) => owner.pi_add_donation(token, attr),
        None => owner.pi_remove_donation(token),
    };
    let _ = sched::operation::pi_apply_effective_attr(owner, effective);
    pi_propagate_from(owner);
}

fn pi_top_donation(state: &PiFutexState) -> Option<SchedAttr> {
    pi_best_waiter(state).map(|(_, task)| task.sched.sched_attr())
}

fn pi_refresh_owner(key: FutexKey) {
    let update = {
        let table = PI_FUTEX_TABLE.lock();
        let Some(state) = table.get(&key) else {
            return;
        };
        state
            .owner
            .upgrade()
            .map(|owner| (owner, state.token, pi_top_donation(state)))
    };
    if let Some((owner, token, donation)) = update {
        pi_owner_update(&owner, token, donation);
    }
}

fn pi_waiting_owner_locked(
    table: &BTreeMap<FutexKey, PiFutexState>,
    waiter: &Arc<Task>,
) -> Option<Arc<Task>> {
    table.values().find_map(|state| {
        state
            .waiters
            .iter()
            .any(|entry| {
                entry
                    .task
                    .upgrade()
                    .is_some_and(|task| Arc::ptr_eq(&task, waiter))
            })
            .then(|| state.owner.upgrade())
            .flatten()
    })
}

fn pi_chain_reaches_locked(
    table: &BTreeMap<FutexKey, PiFutexState>,
    start: &Arc<Task>,
    target: &Arc<Task>,
) -> bool {
    let mut current = Arc::clone(start);
    let mut visited = [0usize; PI_CHAIN_MAX];
    for depth in 0..PI_CHAIN_MAX {
        if Arc::ptr_eq(&current, target) {
            return true;
        }
        let address = Arc::as_ptr(&current) as usize;
        if visited[..depth].contains(&address) {
            return false;
        }
        visited[depth] = address;
        let Some(next) = pi_waiting_owner_locked(table, &current) else {
            return false;
        };
        current = next;
    }
    false
}

fn pi_propagate_from(waiter: &Arc<Task>) {
    let mut current = Arc::clone(waiter);
    let mut visited = [0usize; PI_CHAIN_MAX];
    for depth in 0..PI_CHAIN_MAX {
        let address = Arc::as_ptr(&current) as usize;
        if visited[..depth].contains(&address) {
            return;
        }
        visited[depth] = address;
        let update = {
            let table = PI_FUTEX_TABLE.lock();
            table.values().find_map(|state| {
                state
                    .waiters
                    .iter()
                    .any(|entry| {
                        entry
                            .task
                            .upgrade()
                            .is_some_and(|task| Arc::ptr_eq(&task, &current))
                    })
                    .then(|| {
                        state
                            .owner
                            .upgrade()
                            .map(|owner| (owner, state.token, pi_top_donation(state)))
                    })
                    .flatten()
            })
        };
        let Some((owner, token, donation)) = update else {
            return;
        };
        let effective = match donation {
            Some(attr) => owner.pi_add_donation(token, attr),
            None => owner.pi_remove_donation(token),
        };
        let _ = sched::operation::pi_apply_effective_attr(&owner, effective);
        current = owner;
    }
}

fn futex_wake_key(key: FutexKey, count: usize, bitset: u32) -> usize {
    if count == 1 {
        return futex_wake_key_one(key, bitset);
    }
    futex_wake_key_inner(key, count, bitset, false)
}

/// 只取出一个 waiter 的无分配唤醒路径，供退出清理中的 robust/clear-child-tid 使用。
fn futex_wake_key_one(key: FutexKey, bitset: u32) -> usize {
    futex_wake_key_one_with_mode(key, bitset, false)
}

fn futex_wake_key_one_with_mode(key: FutexKey, bitset: u32, deferred: bool) -> usize {
    let waiter = {
        let mut table = FUTEX_TABLE.lock();
        let mut selected = None;
        loop {
            let Some(bucket) = table.get_mut(&key) else {
                break;
            };
            let Some(index) = bucket
                .waiters
                .iter()
                .position(|waiter| waiter.bitset & bitset != 0)
            else {
                break;
            };
            let waiter = bucket.waiters.remove(index);
            if let Some(task) = waiter.task.upgrade() {
                selected = Some((task, waiter.state));
                break;
            }
            if bucket.waiters.is_empty() {
                break;
            }
        }
        if table
            .get(&key)
            .is_some_and(|bucket| bucket.waiters.is_empty())
        {
            table.remove(&key);
        }
        selected
    };
    waiter.map_or(0, |(task, state)| {
        if deferred {
            wake_futex_waiter_deferred(task, state)
        } else {
            wake_futex_waiter(task, state)
        }
    })
}

fn futex_wake_key_inner(key: FutexKey, count: usize, bitset: u32, trace: bool) -> usize {
    #[cfg(not(feature = "trace-task-lifecycle"))]
    let _ = trace;
    let waiters = {
        let mut table = FUTEX_TABLE.lock();
        let Some(bucket) = table.get_mut(&key) else {
            #[cfg(feature = "trace-task-lifecycle")]
            if trace {
                log::info!("[syscall][futex] wake-bucket-miss key={key:?}");
            }
            return 0;
        };
        #[cfg(feature = "trace-task-lifecycle")]
        let initial_len = bucket.waiters.len();
        #[cfg(feature = "trace-task-lifecycle")]
        let mut dead = 0usize;
        let mut waiters = Vec::new();
        let mut idx = 0;
        while idx < bucket.waiters.len() && waiters.len() < count {
            if (bucket.waiters[idx].bitset & bitset) != 0 {
                let waiter = bucket.waiters.remove(idx);
                if let Some(task) = waiter.task.upgrade() {
                    waiters.push((task, waiter.state));
                } else {
                    #[cfg(feature = "trace-task-lifecycle")]
                    {
                        dead += 1;
                    }
                }
            } else {
                idx += 1;
            }
        }
        #[cfg(feature = "trace-task-lifecycle")]
        if trace {
            log::info!(
                "[syscall][futex] wake-bucket key={key:?} before={initial_len} selected={} dead={dead} remain={}",
                waiters.len(),
                bucket.waiters.len(),
            );
        }
        if bucket.waiters.is_empty() {
            table.remove(&key);
        }
        waiters
    };
    wake_futex_waiters(waiters)
}

fn wake_futex_waiters(waiters: Vec<(Arc<sched::Task>, Arc<FutexWaitState>)>) -> usize {
    let mut woken = 0usize;
    for (waiter, state) in waiters {
        woken += wake_futex_waiter(waiter, state);
    }
    woken
}

fn wake_futex_waiter(waiter: Arc<sched::Task>, state: Arc<FutexWaitState>) -> usize {
    match state.mark_woken() {
        FUTEX_WAIT_SLEEPING => {
            if waiter.cas_state(TaskState::Sleeping, TaskState::Runnable) {
                let now_ns = sched::now_ns_public();
                #[cfg(feature = "performance-profile")]
                waiter.mark_profile_woken(now_ns);
                // futex 唤醒可以跨越任意用户态同步原语；只按常规首选队列入队，
                // 不依赖当前 syscall 的返回边界完成交接。
                sched::enqueue_task_preferred(waiter, now_ns);
            }
            1
        }
        FUTEX_WAIT_ARMED | FUTEX_WAIT_WOKEN => 1,
        _ => 0,
    }
}

/// exec 清理使用无锁 deferred 链发布唤醒，避免在 PONR 后触碰 runqueue 分配。
fn wake_futex_waiter_deferred(waiter: Arc<sched::Task>, state: Arc<FutexWaitState>) -> usize {
    match state.mark_woken() {
        FUTEX_WAIT_SLEEPING => {
            if waiter.cas_state(TaskState::Sleeping, TaskState::Runnable) {
                sched::defer_task_wake(&waiter);
            }
            1
        }
        FUTEX_WAIT_ARMED | FUTEX_WAIT_WOKEN => 1,
        _ => 0,
    }
}

fn futex_remove_waiter(key: FutexKey, task: &Arc<Task>) -> bool {
    let mut table = FUTEX_TABLE.lock();
    let Some(bucket) = table.get_mut(&key) else {
        return false;
    };
    let before = bucket.waiters.len();
    bucket.waiters.retain(|w| match w.task.upgrade() {
        Some(waiter) => !Arc::ptr_eq(&waiter, task),
        None => false,
    });
    let removed = before != bucket.waiters.len();
    if bucket.waiters.is_empty() {
        table.remove(&key);
    }
    removed
}

fn futex_remove_task_waiters(task: &Arc<Task>) -> usize {
    let mut table = FUTEX_TABLE.lock();
    let mut empty_keys = Vec::new();
    let mut removed = 0usize;
    for (key, bucket) in table.iter_mut() {
        let before = bucket.waiters.len();
        bucket.waiters.retain(|waiter| match waiter.task.upgrade() {
            Some(waiter_task) => !Arc::ptr_eq(&waiter_task, task),
            None => false,
        });
        removed = removed.saturating_add(before.saturating_sub(bucket.waiters.len()));
        if bucket.waiters.is_empty() {
            empty_keys.push(*key);
        }
    }
    for key in empty_keys {
        table.remove(&key);
    }
    removed
}

fn pi_remove_task_waiters(task: &Arc<Task>) {
    let mut updates = Vec::new();
    {
        let mut table = PI_FUTEX_TABLE.lock();
        for state in table.values_mut() {
            let before = state.waiters.len();
            state.waiters.retain(|waiter| {
                !waiter
                    .task
                    .upgrade()
                    .is_some_and(|queued| Arc::ptr_eq(&queued, task))
            });
            if before != state.waiters.len() {
                if let Some(owner) = state.owner.upgrade() {
                    updates.push((owner, state.token, pi_top_donation(state)));
                }
            }
        }
    }
    for (owner, token, donation) in updates {
        pi_owner_update(&owner, token, donation);
    }
}

fn pi_release_owned_futexes(task: &Arc<Task>) {
    let owned = {
        let table = PI_FUTEX_TABLE.lock();
        table
            .iter()
            .filter_map(|(key, state)| {
                state
                    .owner
                    .upgrade()
                    .filter(|owner| Arc::ptr_eq(owner, task))
                    .and_then(|_| state.vm.upgrade().map(|vm| (*key, vm, state.uaddr)))
            })
            .collect::<Vec<_>>()
    };
    let mut no_handoffs = Vec::new();
    let mut overflow = false;
    for (key, vm, uaddr) in owned {
        let _ = pi_owner_died_key(
            &vm,
            key,
            uaddr,
            task,
            false,
            &mut no_handoffs,
            &mut overflow,
        );
    }
}

/// exec 清理专用：不创建临时 Vec 地移除当前任务在普通 futex 表中的 waiter。
fn futex_remove_task_waiters_for_exec(task: &Arc<Task>) -> usize {
    let mut removed = 0usize;
    loop {
        let empty_key = {
            let mut table = FUTEX_TABLE.lock();
            let mut empty_key = None;
            for (key, bucket) in table.iter_mut() {
                let before = bucket.waiters.len();
                bucket.waiters.retain(|waiter| {
                    !waiter
                        .task
                        .upgrade()
                        .is_some_and(|queued| Arc::ptr_eq(&queued, task))
                });
                removed = removed.saturating_add(before.saturating_sub(bucket.waiters.len()));
                if bucket.waiters.is_empty() {
                    empty_key = Some(*key);
                    break;
                }
            }
            if let Some(key) = empty_key {
                table.remove(&key);
            }
            empty_key
        };
        if empty_key.is_none() {
            return removed;
        }
    }
}

/// exec 清理专用：逐项更新已有 PI donation，避免在不可回退阶段收集更新 Vec。
fn pi_remove_task_waiters_for_exec(task: &Arc<Task>) {
    loop {
        let update = {
            let mut table = PI_FUTEX_TABLE.lock();
            table.values_mut().find_map(|state| {
                let before = state.waiters.len();
                state.waiters.retain(|waiter| {
                    !waiter
                        .task
                        .upgrade()
                        .is_some_and(|queued| Arc::ptr_eq(&queued, task))
                });
                (before != state.waiters.len()).then(|| {
                    state
                        .owner
                        .upgrade()
                        .map(|owner| (owner, state.token, pi_top_donation(state)))
                })
            })
        };
        match update {
            None => return,
            Some(None) => continue,
            Some(Some((owner, token, donation))) => {
                let _ = owner.pi_update_existing_donation(token, donation);
                sched::defer_pi_effective_update(&owner);
            }
        }
    }
}

/// exec 清理专用：沿用标准 PI owner-death handoff，只把调度器重排和 waiter
/// 唤醒推迟到安全调度边界。
fn pi_release_owned_futexes_for_exec(task: &Arc<Task>, scratch: &mut ExecCleanupScratch) {
    loop {
        let owned = {
            let table = PI_FUTEX_TABLE.lock();
            table.iter().find_map(|(key, state)| {
                state
                    .owner
                    .upgrade()
                    .filter(|owner| Arc::ptr_eq(owner, task))
                    .and_then(|_| state.vm.upgrade().map(|vm| (*key, vm, state.uaddr)))
            })
        };
        let Some((key, vm, uaddr)) = owned else {
            return;
        };
        let _ = pi_owner_died_key(
            &vm,
            key,
            uaddr,
            task,
            true,
            &mut scratch.pi_handoffs,
            &mut scratch.pi_handoff_overflow,
        );
    }
}

fn futex_enqueue_waiter_if_equal(
    vm: &VmSpace,
    key: FutexKey,
    uaddr: usize,
    expected: u32,
    waiter: FutexWaiter,
) -> Result<(), Errno> {
    let mut table = FUTEX_TABLE.lock();
    let observed = vm.read_user_u32_nofault(uaddr)?;
    if observed != expected {
        #[cfg(feature = "trace-task-lifecycle")]
        if waiter
            .task
            .upgrade()
            .as_ref()
            .is_some_and(|task| trace_futex_task(task))
        {
            log::info!(
                "[syscall][futex] enqueue-recheck key={key:?} expected={expected} observed={observed}"
            );
        }
        return Err(Errno::EAGAIN);
    }
    let bucket = table.entry(key).or_insert(FutexBucket {
        waiters: Vec::new(),
    });
    #[cfg(feature = "trace-task-lifecycle")]
    if waiter
        .task
        .upgrade()
        .as_ref()
        .is_some_and(|task| trace_futex_task(task))
    {
        log::info!(
            "[syscall][futex] enqueue key={key:?} expected={expected} observed={observed} before={}",
            bucket.waiters.len(),
        );
    }
    bucket.waiters.push(waiter);
    Ok(())
}

fn futex_requeue_key(
    src: FutexKey,
    dst: FutexKey,
    wake_count: usize,
    requeue_count: usize,
    bitset: u32,
) -> usize {
    let (wake, requeued) = {
        let mut table = FUTEX_TABLE.lock();
        futex_requeue_locked(&mut table, src, dst, wake_count, requeue_count, bitset)
    };
    wake_futex_waiters(wake) + requeued
}

fn futex_cmp_requeue_key(
    vm: &VmSpace,
    uaddr: usize,
    expected: u32,
    src: FutexKey,
    dst: FutexKey,
    wake_count: usize,
    requeue_count: usize,
    bitset: u32,
) -> Result<usize, Errno> {
    // 先在表锁外触发可能需要的 lazy fault；自旋锁临界区内只允许 nofault 读取。
    vm.prefault_user_u32(uaddr, false)?;
    futex_cmp_requeue_after_prefault(
        vm,
        uaddr,
        expected,
        src,
        dst,
        wake_count,
        requeue_count,
        bitset,
    )
}

fn futex_cmp_requeue_after_prefault(
    vm: &VmSpace,
    uaddr: usize,
    expected: u32,
    src: FutexKey,
    dst: FutexKey,
    wake_count: usize,
    requeue_count: usize,
    bitset: u32,
) -> Result<usize, Errno> {
    let (wake, requeued) = {
        let mut table = FUTEX_TABLE.lock();
        if vm.read_user_u32_nofault(uaddr)? != expected {
            return Err(Errno::EAGAIN);
        }
        futex_requeue_locked(&mut table, src, dst, wake_count, requeue_count, bitset)
    };
    Ok(wake_futex_waiters(wake) + requeued)
}

fn futex_requeue_locked(
    table: &mut BTreeMap<FutexKey, FutexBucket>,
    src: FutexKey,
    dst: FutexKey,
    wake_count: usize,
    requeue_count: usize,
    bitset: u32,
) -> (Vec<(Arc<Task>, Arc<FutexWaitState>)>, usize) {
    let mut wake = Vec::new();
    let mut requeue = Vec::new();
    if let Some(bucket) = table.get_mut(&src) {
        let mut idx = 0;
        while idx < bucket.waiters.len()
            && (wake.len() < wake_count || requeue.len() < requeue_count)
        {
            if bucket.waiters[idx].pi_target.is_some() {
                idx += 1;
                continue;
            }
            if (bucket.waiters[idx].bitset & bitset) == 0 {
                idx += 1;
                continue;
            }
            let waiter = bucket.waiters.remove(idx);
            if wake.len() < wake_count {
                if let Some(task) = waiter.task.upgrade() {
                    wake.push((task, waiter.state));
                }
            } else if requeue.len() < requeue_count && waiter.task.strong_count() != 0 {
                requeue.push(waiter);
            }
        }
        if bucket.waiters.is_empty() {
            table.remove(&src);
        }
    }
    let requeued = requeue.len();
    if !requeue.is_empty() {
        table
            .entry(dst)
            .or_insert(FutexBucket {
                waiters: Vec::new(),
            })
            .waiters
            .extend(requeue);
    }
    (wake, requeued)
}

#[cfg(any(feature = "kernel-tests", feature = "smp-tests"))]
#[path = "../tests/futex.rs"]
mod futex_tests;

fn futex_atomic_update_user(
    vm: &VmSpace,
    uaddr: usize,
    update: impl Fn(u32) -> Result<u32, Errno>,
) -> Result<u32, Errno> {
    vm.prefault_user_u32(uaddr, true)?;
    let mut current = vm.read_user_u32_nofault(uaddr)?;
    loop {
        let new = update(current)?;
        let observed = vm.compare_exchange_user_u32_nofault(uaddr, current, new)?;
        if observed == current {
            return Ok(current);
        }
        current = observed;
    }
}

fn futex_wake_op(
    task: &Arc<Task>,
    uaddr: usize,
    uaddr2: usize,
    private: bool,
    wake_count: usize,
    wake_count2: usize,
    encoded: u32,
) -> Result<usize, Errno> {
    let key1 = futex_key(task, uaddr, private)?;
    let key2 = futex_key(task, uaddr2, private)?;
    let vm = task_vm_space_for_futex(task)?;
    let old = futex_atomic_update_user(&vm, uaddr2, |old| futex_apply_wake_op(old, encoded))?;
    let mut woken = futex_wake_key(key1, wake_count, FUTEX_BITSET_MATCH_ANY);
    if futex_wake_op_cmp(old, encoded)? {
        woken += futex_wake_key(key2, wake_count2, FUTEX_BITSET_MATCH_ANY);
    }
    Ok(woken)
}

fn futex_apply_wake_op(old: u32, encoded: u32) -> Result<u32, Errno> {
    const FUTEX_OP_SET: u32 = 0;
    const FUTEX_OP_ADD: u32 = 1;
    const FUTEX_OP_OR: u32 = 2;
    const FUTEX_OP_ANDN: u32 = 3;
    const FUTEX_OP_XOR: u32 = 4;
    const FUTEX_OP_OPARG_SHIFT: u32 = 8;

    let mut op = (encoded >> 28) & 0xf;
    let mut arg = (encoded >> 12) & 0xfff;
    if (op & FUTEX_OP_OPARG_SHIFT) != 0 {
        op &= !FUTEX_OP_OPARG_SHIFT;
        if arg >= 32 {
            return Err(Errno::EINVAL);
        }
        arg = 1u32 << arg;
    }
    match op {
        FUTEX_OP_SET => Ok(arg),
        FUTEX_OP_ADD => Ok(old.wrapping_add(arg)),
        FUTEX_OP_OR => Ok(old | arg),
        FUTEX_OP_ANDN => Ok(old & !arg),
        FUTEX_OP_XOR => Ok(old ^ arg),
        _ => Err(Errno::EINVAL),
    }
}

fn futex_wake_op_cmp(old: u32, encoded: u32) -> Result<bool, Errno> {
    let cmp = (encoded >> 24) & 0xf;
    let rhs = sign_extend_12(encoded & 0xfff);
    let lhs = old as i32;
    match cmp {
        0 => Ok(lhs == rhs),
        1 => Ok(lhs != rhs),
        2 => Ok(lhs < rhs),
        3 => Ok(lhs <= rhs),
        4 => Ok(lhs > rhs),
        5 => Ok(lhs >= rhs),
        _ => Err(Errno::EINVAL),
    }
}

fn sign_extend_12(value: u32) -> i32 {
    ((value << 20) as i32) >> 20
}

fn pi_register_owner(key: FutexKey, vm: &Arc<VmSpace>, uaddr: usize, owner: &Arc<Task>) {
    let mut table = PI_FUTEX_TABLE.lock();
    let state = table.entry(key).or_insert_with(|| PiFutexState {
        token: next_pi_token(),
        owner: Arc::downgrade(owner),
        vm: Arc::downgrade(vm),
        uaddr,
        waiters: Vec::new(),
    });
    state.owner = Arc::downgrade(owner);
    state.vm = Arc::downgrade(vm);
    state.uaddr = uaddr;
    drop(table);
    pi_refresh_owner(key);
}

fn pi_enqueue_waiter(
    vm: &Arc<VmSpace>,
    key: FutexKey,
    uaddr: usize,
    task: &Arc<Task>,
    wait_state: &Arc<FutexWaitState>,
) -> Result<(), Errno> {
    let tid = task.pid_root().unwrap_or(0) as u32;
    loop {
        let observed = vm.read_user_u32_nofault(uaddr)?;
        let owner_tid = observed & FUTEX_TID_MASK;
        if owner_tid == 0 {
            return Err(Errno::EAGAIN);
        }
        if owner_tid == tid {
            return Err(Errno::EDEADLK);
        }
        let owner = lookup_root_task(owner_tid as i32)?;
        let mut table = PI_FUTEX_TABLE.lock();
        let current = vm.read_user_u32_nofault(uaddr)?;
        if (current & FUTEX_TID_MASK) != owner_tid {
            continue;
        }
        if pi_chain_reaches_locked(&table, &owner, task) {
            return Err(Errno::EDEADLK);
        }
        let waiting = current | FUTEX_WAITERS;
        if waiting != current
            && vm.compare_exchange_user_u32_nofault(uaddr, current, waiting)? != current
        {
            continue;
        }
        let state = table.entry(key).or_insert_with(|| PiFutexState {
            token: next_pi_token(),
            owner: Arc::downgrade(&owner),
            vm: Arc::downgrade(vm),
            uaddr,
            waiters: Vec::new(),
        });
        state.owner = Arc::downgrade(&owner);
        state.vm = Arc::downgrade(vm);
        state.uaddr = uaddr;
        state
            .waiters
            .retain(|waiter| waiter.task.strong_count() != 0);
        if state.waiters.iter().any(|waiter| {
            waiter
                .task
                .upgrade()
                .is_some_and(|queued| Arc::ptr_eq(&queued, task))
        }) {
            return Err(Errno::EDEADLK);
        }
        state.waiters.push(PiFutexWaiter {
            task: Arc::downgrade(task),
            state: Arc::clone(wait_state),
            seq: NEXT_PI_WAITER_SEQ.fetch_add(1, Ordering::Relaxed),
        });
        drop(table);
        pi_refresh_owner(key);
        return Ok(());
    }
}

fn pi_remove_waiter(
    vm: &VmSpace,
    key: FutexKey,
    uaddr: usize,
    task: &Arc<Task>,
    wait_state: &Arc<FutexWaitState>,
) -> Result<bool, Errno> {
    let update = {
        let mut table = PI_FUTEX_TABLE.lock();
        let Some(state) = table.get_mut(&key) else {
            return Ok(false);
        };
        let Some(index) = state.waiters.iter().position(|waiter| {
            Arc::ptr_eq(&waiter.state, wait_state)
                && waiter
                    .task
                    .upgrade()
                    .is_some_and(|queued| Arc::ptr_eq(&queued, task))
        }) else {
            return Ok(false);
        };
        state.waiters.remove(index);
        state
            .waiters
            .retain(|waiter| waiter.task.strong_count() != 0);
        let owner = state.owner.upgrade();
        let token = state.token;
        let donation = pi_top_donation(state);
        if state.waiters.is_empty() {
            loop {
                let current = vm.read_user_u32_nofault(uaddr)?;
                if (current & FUTEX_WAITERS) == 0 {
                    break;
                }
                let new = current & !FUTEX_WAITERS;
                if vm.compare_exchange_user_u32_nofault(uaddr, current, new)? == current {
                    break;
                }
            }
        }
        owner.map(|owner| (owner, token, donation))
    };
    if let Some((owner, token, donation)) = update {
        pi_owner_update(&owner, token, donation);
    }
    Ok(true)
}

fn pi_wait_registered(
    vm: &VmSpace,
    key: FutexKey,
    uaddr: usize,
    task: &Arc<Task>,
    wait_state: &Arc<FutexWaitState>,
    deadline_ns: Option<u64>,
) -> Result<usize, Errno> {
    loop {
        if wait_state.is_woken() {
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            restore_current_task_after_sleep(task);
            return Ok(0);
        }
        if let Some(deadline) = deadline_ns
            && sched::now_ns_direct() >= deadline
            && pi_remove_waiter(vm, key, uaddr, task, wait_state)?
        {
            restore_current_task_after_sleep(task);
            sched::cancel_sleep_deadline(task);
            return Err(Errno::ETIMEDOUT);
        }
        if sched::operation::has_interrupting_signal(task)
            && pi_remove_waiter(vm, key, uaddr, task, wait_state)?
        {
            restore_current_task_after_sleep(task);
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            return Err(Errno::EINTR);
        }
        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(sched::WaitReason::Futex, sched::now_ns_direct());
        let _ = task.cas_state(TaskState::Running, TaskState::Sleeping);
        if !wait_state.mark_sleeping() {
            restore_current_task_after_sleep(task);
            continue;
        }
        if wait_state.is_woken() {
            restore_current_task_after_sleep(task);
            continue;
        }
        sched::operation::sched_yield()?;
        let _ = wait_state.rearm_after_non_futex_wakeup();
        restore_current_task_after_sleep(task);
    }
}

fn futex_lock_pi(
    task: &Arc<Task>,
    uaddr: usize,
    private: bool,
    try_only: bool,
    deadline_ns: Option<u64>,
) -> Result<usize, Errno> {
    let tid = task.pid_root().unwrap_or(0) as u32;
    if tid == 0 {
        return Err(Errno::ESRCH);
    }
    let key = futex_key(task, uaddr, private)?;
    let vm = task_vm_space_for_futex(task)?;
    vm.prefault_user_u32(uaddr, true)?;
    loop {
        let cur = vm.read_user_u32_nofault(uaddr)?;
        let owner = cur & FUTEX_TID_MASK;
        if owner == 0 {
            let new = (cur & FUTEX_OWNER_DIED) | tid;
            if vm.compare_exchange_user_u32_nofault(uaddr, cur, new)? == cur {
                pi_register_owner(key, &vm, uaddr, task);
                return Ok(0);
            }
            continue;
        }
        if owner == tid {
            return Err(Errno::EDEADLK);
        }
        if try_only {
            return Err(Errno::EAGAIN);
        }
        if let Some(deadline) = deadline_ns
            && !sched::register_sleep_deadline(task, deadline)
        {
            return Err(Errno::ETIMEDOUT);
        }
        let wait_state = Arc::new(FutexWaitState::new());
        match pi_enqueue_waiter(&vm, key, uaddr, task, &wait_state) {
            Ok(()) => return pi_wait_registered(&vm, key, uaddr, task, &wait_state, deadline_ns),
            Err(Errno::EAGAIN) => {
                if deadline_ns.is_some() {
                    sched::cancel_sleep_deadline(task);
                }
            }
            Err(err) => {
                if deadline_ns.is_some() {
                    sched::cancel_sleep_deadline(task);
                }
                return Err(err);
            }
        }
    }
}

fn futex_unlock_pi(task: &Arc<Task>, uaddr: usize, private: bool) -> Result<usize, Errno> {
    let tid = task.pid_root().unwrap_or(0) as u32;
    if tid == 0 {
        return Err(Errno::ESRCH);
    }
    let key = futex_key(task, uaddr, private)?;
    let vm = task_vm_space_for_futex(task)?;
    vm.prefault_user_u32(uaddr, true)?;
    loop {
        let mut table = PI_FUTEX_TABLE.lock();
        let cur = vm.read_user_u32_nofault(uaddr)?;
        if (cur & FUTEX_TID_MASK) != tid {
            return Err(Errno::EPERM);
        }
        let Some(state) = table.get_mut(&key) else {
            if vm.compare_exchange_user_u32_nofault(uaddr, cur, 0)? == cur {
                return Ok(0);
            }
            continue;
        };
        state
            .waiters
            .retain(|waiter| waiter.task.strong_count() != 0);
        let token = state.token;
        let old_owner = state.owner.upgrade().unwrap_or_else(|| Arc::clone(task));
        let Some((next_index, next_owner)) = pi_best_waiter(state) else {
            if vm.compare_exchange_user_u32_nofault(uaddr, cur, 0)? != cur {
                continue;
            }
            table.remove(&key);
            drop(table);
            pi_owner_update(&old_owner, token, None);
            return Ok(0);
        };
        let next_tid = next_owner.pid_root().unwrap_or(0) as u32;
        if next_tid == 0 {
            state.waiters.remove(next_index);
            continue;
        }
        let remaining = state.waiters.len().saturating_sub(1);
        let mut new = (cur & FUTEX_OWNER_DIED) | next_tid;
        if remaining != 0 {
            new |= FUTEX_WAITERS;
        }
        if vm.compare_exchange_user_u32_nofault(uaddr, cur, new)? != cur {
            continue;
        }
        let handed = state.waiters.remove(next_index);
        state.owner = Arc::downgrade(&next_owner);
        let next_donation = pi_top_donation(state);
        drop(table);

        pi_owner_update(&old_owner, token, None);
        pi_owner_update(&next_owner, token, next_donation);
        let _ = wake_futex_waiter(next_owner, handed.state);
        return Ok(0);
    }
}

fn pi_owner_died_key(
    vm: &VmSpace,
    key: FutexKey,
    uaddr: usize,
    task: &Arc<Task>,
    deferred: bool,
    pi_handoffs: &mut Vec<ExecPiHandoff>,
    pi_handoff_overflow: &mut bool,
) -> bool {
    let update = {
        let mut table = PI_FUTEX_TABLE.lock();
        let Some(state) = table.get_mut(&key) else {
            return false;
        };
        if !state
            .owner
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, task))
        {
            return false;
        }
        let Ok(cur) = vm.read_user_u32_nofault(uaddr) else {
            return false;
        };
        if (cur & FUTEX_TID_MASK) != task.pid_root().unwrap_or(0) as u32 {
            return false;
        }
        state
            .waiters
            .retain(|waiter| waiter.task.strong_count() != 0);
        let token = state.token;
        if let Some((index, next)) = pi_best_waiter(state) {
            let next_tid = next.pid_root().unwrap_or(0) as u32;
            if next_tid == 0 {
                return false;
            }
            let remaining = state.waiters.len().saturating_sub(1);
            let mut new = FUTEX_OWNER_DIED | next_tid;
            if remaining != 0 {
                new |= FUTEX_WAITERS;
            }
            if vm.compare_exchange_user_u32_nofault(uaddr, cur, new).ok() != Some(cur) {
                return false;
            }
            let waiter = state.waiters.remove(index);
            state.owner = Arc::downgrade(&next);
            let donation = pi_top_donation(state);
            Some((token, Some((next, donation, waiter.state))))
        } else {
            if vm
                .compare_exchange_user_u32_nofault(uaddr, cur, FUTEX_OWNER_DIED)
                .ok()
                != Some(cur)
            {
                return false;
            }
            table.remove(&key);
            Some((token, None))
        }
    };
    if let Some((token, handoff)) = update {
        if deferred {
            let _ = task.pi_update_existing_donation(token, None);
            sched::defer_pi_effective_update(task);
        } else {
            pi_owner_update(task, token, None);
        }
        if let Some((next, donation, state)) = handoff {
            if deferred {
                if let Some(donation) = donation {
                    if !pi_handoffs
                        .iter()
                        .any(|handoff| handoff.token == token && Arc::ptr_eq(&handoff.task, &next))
                    {
                        if pi_handoffs.len() == pi_handoffs.capacity() {
                            *pi_handoff_overflow = true;
                            log::emergency!(
                                "[syscall][futex] exec PI handoff scratch exhausted token={token}"
                            );
                        } else {
                            pi_handoffs.push(ExecPiHandoff {
                                task: Arc::clone(&next),
                                token,
                                attr: donation,
                            });
                        }
                    }
                }
                let _ = wake_futex_waiter_deferred(next, state);
            } else {
                pi_owner_update(&next, token, donation);
                let _ = wake_futex_waiter(next, state);
            }
        }
    }
    true
}

fn futex_wait_requeue_pi(
    task: &Arc<Task>,
    src_uaddr: usize,
    expected: u32,
    dst_uaddr: usize,
    private: bool,
    deadline_ns: Option<u64>,
) -> Result<usize, Errno> {
    if dst_uaddr == 0 || dst_uaddr % 4 != 0 || src_uaddr == dst_uaddr {
        return Err(Errno::EINVAL);
    }
    let vm = task_vm_space_for_futex(task)?;
    let src = vm.futex_key_for(src_uaddr, private)?;
    let dst = vm.futex_key_for(dst_uaddr, private)?;
    if src == dst {
        return Err(Errno::EINVAL);
    }
    vm.prefault_user_u32(src_uaddr, false)?;
    vm.prefault_user_u32(dst_uaddr, true)?;
    if let Some(deadline) = deadline_ns
        && !sched::register_sleep_deadline(task, deadline)
    {
        return Err(Errno::ETIMEDOUT);
    }
    let wait_state = Arc::new(FutexWaitState::new());
    if let Err(err) = futex_enqueue_waiter_if_equal(
        &vm,
        src,
        src_uaddr,
        expected,
        FutexWaiter {
            task: Arc::downgrade(task),
            bitset: FUTEX_BITSET_MATCH_ANY,
            waitv_index: None,
            pi_target: Some((dst, dst_uaddr)),
            state: Arc::clone(&wait_state),
        },
    ) {
        if deadline_ns.is_some() {
            sched::cancel_sleep_deadline(task);
        }
        return Err(err);
    }

    loop {
        if wait_state.is_woken() {
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            restore_current_task_after_sleep(task);
            return Ok(0);
        }
        let timed_out = deadline_ns.is_some_and(|deadline| sched::now_ns_direct() >= deadline);
        let interrupted = sched::operation::has_interrupting_signal(task);
        if timed_out || interrupted {
            let removed = futex_remove_waiter(src, task)
                || pi_remove_waiter(&vm, dst, dst_uaddr, task, &wait_state)?;
            if removed {
                restore_current_task_after_sleep(task);
                if deadline_ns.is_some() {
                    sched::cancel_sleep_deadline(task);
                }
                return Err(if timed_out {
                    Errno::ETIMEDOUT
                } else {
                    Errno::EINTR
                });
            }
            continue;
        }
        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(sched::WaitReason::Futex, sched::now_ns_direct());
        let _ = task.cas_state(TaskState::Running, TaskState::Sleeping);
        if !wait_state.mark_sleeping() {
            restore_current_task_after_sleep(task);
            continue;
        }
        if wait_state.is_woken() {
            restore_current_task_after_sleep(task);
            continue;
        }
        sched::operation::sched_yield()?;
        let _ = wait_state.rearm_after_non_futex_wakeup();
        restore_current_task_after_sleep(task);
    }
}

fn futex_cmp_requeue_pi(
    task: &Arc<Task>,
    src_uaddr: usize,
    expected: u32,
    dst_uaddr: usize,
    private: bool,
    wake_count: usize,
    requeue_count: usize,
) -> Result<usize, Errno> {
    if wake_count != 1 || dst_uaddr == 0 || dst_uaddr % 4 != 0 || src_uaddr == dst_uaddr {
        return Err(Errno::EINVAL);
    }
    let vm = task_vm_space_for_futex(task)?;
    let src = vm.futex_key_for(src_uaddr, private)?;
    let dst = vm.futex_key_for(dst_uaddr, private)?;
    if src == dst {
        return Err(Errno::EINVAL);
    }
    vm.prefault_user_u32(src_uaddr, false)?;
    vm.prefault_user_u32(dst_uaddr, true)?;

    loop {
        let dst_observed = vm.read_user_u32_nofault(dst_uaddr)?;
        let owner_tid = dst_observed & FUTEX_TID_MASK;
        let owner = if owner_tid == 0 {
            None
        } else {
            Some(lookup_root_task(owner_tid as i32)?)
        };
        let (moved, acquired, donation_update) = {
            let mut futex_table = FUTEX_TABLE.lock();
            let mut pi_table = PI_FUTEX_TABLE.lock();
            if vm.read_user_u32_nofault(src_uaddr)? != expected {
                return Err(Errno::EAGAIN);
            }
            let dst_current = vm.read_user_u32_nofault(dst_uaddr)?;
            if (dst_current & FUTEX_TID_MASK) != owner_tid {
                continue;
            }
            let Some(bucket) = futex_table.get_mut(&src) else {
                return Ok(0);
            };
            let limit = requeue_count.saturating_add(1);
            let eligible = bucket
                .waiters
                .iter()
                .filter(|waiter| waiter.pi_target == Some((dst, dst_uaddr)))
                .filter(|waiter| waiter.task.strong_count() != 0)
                .take(limit)
                .count();
            if eligible == 0 {
                return Ok(0);
            }

            let direct_owner = if owner_tid == 0 {
                bucket
                    .waiters
                    .iter()
                    .find(|waiter| waiter.pi_target == Some((dst, dst_uaddr)))
                    .filter(|waiter| waiter.task.strong_count() != 0)
                    .and_then(|waiter| waiter.task.upgrade())
            } else {
                None
            };
            if let Some(pi_owner) = owner.as_ref()
                && bucket
                    .waiters
                    .iter()
                    .filter(|waiter| waiter.pi_target == Some((dst, dst_uaddr)))
                    .filter_map(|waiter| waiter.task.upgrade())
                    .take(limit)
                    .any(|waiter| pi_chain_reaches_locked(&pi_table, pi_owner, &waiter))
            {
                return Err(Errno::EDEADLK);
            }
            let queued = eligible.saturating_sub(usize::from(direct_owner.is_some()));
            let new_word = if let Some(next) = direct_owner.as_ref() {
                let next_tid = next.pid_root().unwrap_or(0) as u32;
                if next_tid == 0 {
                    return Err(Errno::ESRCH);
                }
                next_tid | if queued != 0 { FUTEX_WAITERS } else { 0 }
            } else {
                dst_current | FUTEX_WAITERS
            };
            if vm.compare_exchange_user_u32_nofault(dst_uaddr, dst_current, new_word)?
                != dst_current
            {
                continue;
            }

            let mut selected = Vec::new();
            let mut index = 0;
            while index < bucket.waiters.len() && selected.len() < limit {
                if bucket.waiters[index].pi_target == Some((dst, dst_uaddr)) {
                    let waiter = bucket.waiters.remove(index);
                    if waiter.task.strong_count() != 0 {
                        selected.push(waiter);
                    }
                } else {
                    index += 1;
                }
            }
            let source_empty = bucket.waiters.is_empty();
            if source_empty {
                futex_table.remove(&src);
            }

            let acquired_waiter = if direct_owner.is_some() {
                Some(selected.remove(0))
            } else {
                None
            };
            let pi_owner = direct_owner.or(owner).ok_or(Errno::ESRCH)?;
            let state = pi_table.entry(dst).or_insert_with(|| PiFutexState {
                token: next_pi_token(),
                owner: Arc::downgrade(&pi_owner),
                vm: Arc::downgrade(&vm),
                uaddr: dst_uaddr,
                waiters: Vec::new(),
            });
            state.owner = Arc::downgrade(&pi_owner);
            state.vm = Arc::downgrade(&vm);
            state.uaddr = dst_uaddr;
            state
                .waiters
                .extend(selected.into_iter().filter_map(|waiter| {
                    let task = waiter.task.upgrade()?;
                    Some(PiFutexWaiter {
                        task: Arc::downgrade(&task),
                        state: waiter.state,
                        seq: NEXT_PI_WAITER_SEQ.fetch_add(1, Ordering::Relaxed),
                    })
                }));
            let donation = pi_top_donation(state);
            let token = state.token;
            (
                eligible,
                acquired_waiter
                    .and_then(|waiter| waiter.task.upgrade().map(|task| (task, waiter.state))),
                Some((pi_owner, token, donation)),
            )
        };

        if let Some((owner, token, donation)) = donation_update {
            pi_owner_update(&owner, token, donation);
        }
        if let Some((task, state)) = acquired {
            let _ = wake_futex_waiter(task, state);
        }
        return Ok(moved);
    }
}

fn futex_wake_addr(task: &Arc<Task>, uaddr: usize, count: usize) -> usize {
    if count == 1 {
        return futex_wake_addr_one(task, uaddr);
    }
    let mut woken = 0usize;
    if let Ok(key) = futex_key(task, uaddr, true) {
        woken += futex_wake_key(key, count, FUTEX_BITSET_MATCH_ANY);
    }
    if let Ok(key) = futex_key(task, uaddr, false) {
        woken += futex_wake_key(key, count, FUTEX_BITSET_MATCH_ANY);
    }
    woken
}

fn futex_wake_addr_one(task: &Arc<Task>, uaddr: usize) -> usize {
    let mut woken = 0usize;
    if let Ok(key) = futex_key(task, uaddr, true) {
        woken += futex_wake_key_one(key, FUTEX_BITSET_MATCH_ANY);
    }
    if let Ok(key) = futex_key(task, uaddr, false) {
        woken += futex_wake_key_one(key, FUTEX_BITSET_MATCH_ANY);
    }
    woken
}

fn futex_wake_addr_one_deferred(task: &Arc<Task>, uaddr: usize) -> usize {
    let mut woken = 0usize;
    if let Ok(key) = futex_key(task, uaddr, true) {
        woken += futex_wake_key_one_with_mode(key, FUTEX_BITSET_MATCH_ANY, true);
    }
    if let Ok(key) = futex_key(task, uaddr, false) {
        woken += futex_wake_key_one_with_mode(key, FUTEX_BITSET_MATCH_ANY, true);
    }
    woken
}

fn clear_child_tid_and_wake(task: &Arc<Task>) {
    clear_child_tid_and_wake_with_mode(task, false);
}

fn clear_child_tid_and_wake_with_mode(task: &Arc<Task>, deferred: bool) {
    let tid_addr = task.clear_child_tid();
    if tid_addr == 0 {
        return;
    }
    let written = task_vm_space(task).is_some_and(|vm| {
        vm.read_user_u32_nofault(tid_addr)
            .ok()
            .and_then(|current| {
                vm.compare_exchange_user_u32_nofault(tid_addr, current, 0)
                    .ok()
                    .filter(|previous| *previous == current)
            })
            .is_some()
    });
    task.set_clear_child_tid(0);
    let woken = written
        .then(|| {
            if deferred {
                futex_wake_addr_one_deferred(task, tid_addr)
            } else {
                futex_wake_addr(task, tid_addr, 1)
            }
        })
        .unwrap_or(0);
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][clear-child-tid] pid={:?} addr={:#x} written={} woken={}",
        task.pid_root(),
        tid_addr,
        written,
        woken,
    );
    #[cfg(not(feature = "trace-task-lifecycle"))]
    let _ = (written, woken);
}

pub(super) fn sys_futex(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uaddr = ctx.args[0];
    let futex_op = ctx.args[1] as u32;
    let val = ctx.args[2] as u32;
    let timeout = ctx.args[3];
    let uaddr2 = ctx.args[4];
    let val3 = ctx.args[5] as u32;
    let cmd = futex_cmd(futex_op);
    let private = (futex_op & FUTEX_PRIVATE_FLAG) != 0;

    #[cfg(feature = "trace-task-lifecycle")]
    if trace_futex_task(ctx.task()) {
        log::info!(
            "[syscall][futex] enter pid={:?} comm={:?} cmd={} private={} addr={:#x} val={} addr2={:#x} val3={}",
            ctx.task().pid_root(),
            ctx.task().comm(),
            cmd,
            private,
            uaddr,
            val,
            uaddr2,
            val3,
        );
    }

    if uaddr % 4 != 0 {
        return Err(Errno::EINVAL);
    }

    match cmd {
        FUTEX_WAIT => futex_wait(
            ctx.task(),
            futex_key(ctx.task(), uaddr, private)?,
            uaddr,
            val,
            FUTEX_BITSET_MATCH_ANY,
            futex_wait_deadline(futex_op, cmd, timeout)?,
        ),
        FUTEX_WAIT_BITSET => {
            if val3 == 0 {
                return Err(Errno::EINVAL);
            }
            futex_wait(
                ctx.task(),
                futex_key(ctx.task(), uaddr, private)?,
                uaddr,
                val,
                val3,
                futex_wait_deadline(futex_op, cmd, timeout)?,
            )
        }
        FUTEX_WAKE => {
            let key = futex_key(ctx.task(), uaddr, private)?;
            let traced = {
                #[cfg(feature = "trace-task-lifecycle")]
                {
                    trace_futex_task(ctx.task())
                }
                #[cfg(not(feature = "trace-task-lifecycle"))]
                {
                    false
                }
            };
            let woken = futex_wake_key_inner(key, val as usize, FUTEX_BITSET_MATCH_ANY, traced);
            #[cfg(feature = "trace-task-lifecycle")]
            if trace_futex_task(ctx.task()) {
                log::info!(
                    "[syscall][futex] wake pid={:?} addr={:#x} key={:?} requested={} woken={}",
                    ctx.task().pid_root(),
                    uaddr,
                    key,
                    val,
                    woken,
                );
            }
            Ok(woken)
        }
        FUTEX_WAKE_BITSET => {
            if val3 == 0 {
                return Err(Errno::EINVAL);
            }
            Ok(futex_wake_key(
                futex_key(ctx.task(), uaddr, private)?,
                val as usize,
                val3,
            ))
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            if uaddr2 == 0 || uaddr2 % 4 != 0 {
                return Err(Errno::EINVAL);
            }
            let vm = task_vm_space_for_futex(ctx.task())?;
            let src = vm.futex_key_for(uaddr, private)?;
            let dst = vm.futex_key_for(uaddr2, private)?;
            if cmd == FUTEX_CMP_REQUEUE {
                return futex_cmp_requeue_key(
                    &vm,
                    uaddr,
                    val3,
                    src,
                    dst,
                    val as usize,
                    timeout,
                    FUTEX_BITSET_MATCH_ANY,
                );
            }
            Ok(futex_requeue_key(
                src,
                dst,
                val as usize,
                timeout,
                FUTEX_BITSET_MATCH_ANY,
            ))
        }
        FUTEX_WAKE_OP => {
            if uaddr2 == 0 || uaddr2 % 4 != 0 {
                return Err(Errno::EINVAL);
            }
            futex_wake_op(
                ctx.task(),
                uaddr,
                uaddr2,
                private,
                val as usize,
                timeout,
                val3,
            )
        }
        FUTEX_LOCK_PI | FUTEX_LOCK_PI2 => futex_lock_pi(
            ctx.task(),
            uaddr,
            private,
            false,
            (timeout != 0)
                .then(|| futex_wait_deadline(futex_op, cmd, timeout))
                .transpose()?
                .flatten(),
        ),
        FUTEX_TRYLOCK_PI => futex_lock_pi(ctx.task(), uaddr, private, true, None),
        FUTEX_UNLOCK_PI => futex_unlock_pi(ctx.task(), uaddr, private),
        FUTEX_WAIT_REQUEUE_PI => futex_wait_requeue_pi(
            ctx.task(),
            uaddr,
            val,
            uaddr2,
            private,
            (timeout != 0)
                .then(|| futex_wait_deadline(futex_op, cmd, timeout))
                .transpose()?
                .flatten(),
        ),
        FUTEX_CMP_REQUEUE_PI => futex_cmp_requeue_pi(
            ctx.task(),
            uaddr,
            val3,
            uaddr2,
            private,
            val as usize,
            timeout,
        ),
        _ => {
            // TODO(threading): 其它 futex 操作需要扩展独立的等待队列状态。
            Err(Errno::EOPNOTSUPP)
        }
    }
}

pub(super) fn sys_futex_waitv(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const FUTEX_WAITV_MAX: usize = 128;
    const FUTEX_WAITV_ENTRY_SIZE: usize = 24;

    let waiters = ctx.args[0];
    let nr = ctx.args[1];
    let flags = ctx.args[2];
    let timeout = ctx.args[3];
    let clockid = ctx.args[4];
    if waiters == 0 || nr == 0 || nr > FUTEX_WAITV_MAX || flags != 0 {
        return Err(Errno::EINVAL);
    }
    let deadline = futex2_abs_deadline(timeout, clockid)?;
    let vm = task_vm_space_for_futex(ctx.task())?;
    let mut entries = Vec::new();
    for index in 0..nr {
        let entry = read_futex_waitv_entry(waiters + index * FUTEX_WAITV_ENTRY_SIZE, index)?;
        vm.prefault_user_u32(entry.uaddr, false)?;
        if vm.read_user_u32_nofault(entry.uaddr)? != entry.expected {
            return Err(Errno::EAGAIN);
        }
        entries.push(entry);
    }
    if let Some(deadline) = deadline
        && sched::now_ns_direct() >= deadline
    {
        return Err(Errno::ETIMEDOUT);
    }
    futex_waitv_enqueue_if_equal(&vm, &entries, ctx.task())?;

    loop {
        if sched::operation::has_interrupting_signal(ctx.task()) {
            futex_waitv_remove_all(&entries, ctx.task());
            restore_current_task_after_sleep(ctx.task());
            if deadline.is_some() {
                sched::cancel_sleep_deadline(ctx.task());
            }
            return Err(Errno::EINTR);
        }
        if let Some(index) = futex_waitv_woken_index(&entries) {
            futex_waitv_remove_all(&entries, ctx.task());
            restore_current_task_after_sleep(ctx.task());
            if deadline.is_some() {
                sched::cancel_sleep_deadline(ctx.task());
            }
            return Ok(index);
        }
        if futex_waitv_value_mismatch(&entries)? {
            futex_waitv_remove_all(&entries, ctx.task());
            restore_current_task_after_sleep(ctx.task());
            if deadline.is_some() {
                sched::cancel_sleep_deadline(ctx.task());
            }
            return Err(Errno::EAGAIN);
        }
        if let Some(deadline) = deadline {
            if sched::now_ns_direct() >= deadline {
                futex_waitv_remove_all(&entries, ctx.task());
                restore_current_task_after_sleep(ctx.task());
                sched::cancel_sleep_deadline(ctx.task());
                return Err(Errno::ETIMEDOUT);
            }
            if !sched::register_sleep_deadline(ctx.task(), deadline) {
                futex_waitv_remove_all(&entries, ctx.task());
                restore_current_task_after_sleep(ctx.task());
                return Err(Errno::ETIMEDOUT);
            }
        }
        #[cfg(feature = "performance-profile")]
        ctx.task()
            .begin_profile_wait(sched::WaitReason::Futex, sched::now_ns_direct());
        let _ = ctx
            .task()
            .cas_state(TaskState::Running, TaskState::Sleeping);
        let mut already_woken = None;
        for entry in &entries {
            if !entry.wait_state.mark_sleeping() && entry.wait_state.is_woken() {
                already_woken = Some(entry.index);
                break;
            }
        }
        // 与 FUTEX_WAIT 一样，wake 可能正好发生在“检查 woken_index”和
        // “真正让出 CPU”之间。这里在进入调度器前再查一次，避免事件已到达
        // 但当前任务仍睡下去。
        if let Some(index) = already_woken.or_else(|| futex_waitv_woken_index(&entries)) {
            futex_waitv_remove_all(&entries, ctx.task());
            restore_current_task_after_sleep(ctx.task());
            if deadline.is_some() {
                sched::cancel_sleep_deadline(ctx.task());
            }
            return Ok(index);
        }
        sched::operation::sched_yield()?;
        for entry in &entries {
            let _ = entry.wait_state.rearm_after_non_futex_wakeup();
        }
        if deadline.is_some() {
            sched::cancel_sleep_deadline(ctx.task());
        }
        restore_current_task_after_sleep(ctx.task());
    }
}

pub(super) fn sys_futex_wake(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uaddr = ctx.args[0];
    let mask = ctx.args[1] as u32;
    let nr = ctx.args[2] as isize;
    let flags = ctx.args[3] as u32;
    if uaddr == 0 {
        return Err(Errno::EFAULT);
    }
    if uaddr % 4 != 0 || mask == 0 || nr < 0 {
        return Err(Errno::EINVAL);
    }
    let private = futex2_private(flags)?;
    Ok(futex_wake_key(
        futex_key(ctx.task(), uaddr, private)?,
        nr as usize,
        mask,
    ))
}

pub(super) fn sys_futex_wait(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uaddr = ctx.args[0];
    let val = ctx.args[1] as u32;
    let mask = ctx.args[2] as u32;
    let flags = ctx.args[3] as u32;
    let timeout = ctx.args[4];
    let clockid = ctx.args[5];
    if uaddr == 0 {
        return Err(Errno::EFAULT);
    }
    if uaddr % 4 != 0 || mask == 0 {
        return Err(Errno::EINVAL);
    }
    let private = futex2_private(flags)?;
    futex_wait(
        ctx.task(),
        futex_key(ctx.task(), uaddr, private)?,
        uaddr,
        val,
        mask,
        futex2_abs_deadline(timeout, clockid)?,
    )
}

pub(super) fn sys_futex_requeue(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const FUTEX_WAITV_ENTRY_SIZE: usize = 24;

    let waiters = ctx.args[0];
    let flags = ctx.args[1];
    let nr_wake = ctx.args[2] as isize;
    let nr_requeue = ctx.args[3] as isize;
    if waiters == 0 {
        return Err(Errno::EFAULT);
    }
    if flags != 0 || nr_wake < 0 || nr_requeue < 0 {
        return Err(Errno::EINVAL);
    }
    let src = read_futex_waitv_entry(waiters, 0)?;
    let dst = read_futex_waitv_entry(waiters + FUTEX_WAITV_ENTRY_SIZE, 1)?;
    let vm = task_vm_space_for_futex(ctx.task())?;
    futex_cmp_requeue_key(
        &vm,
        src.uaddr,
        src.expected,
        src.key,
        dst.key,
        nr_wake as usize,
        nr_requeue as usize,
        FUTEX_BITSET_MATCH_ANY,
    )
}

pub(super) fn sys_unshare(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    crate::ns::unshare(ctx.task(), ctx.args[0] as u64)?;
    Ok(0)
}

pub(super) fn sys_kexec_load(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // kexec 内核重载机制整体未实现。Linux 在未启用 CONFIG_KEXEC 时同样返回
    // ENOSYS，此处语义对齐，属正确桩而非缺口。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_init_module(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_delete_module(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timer_create(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timer_create_common(ctx, false)
}

pub(super) fn sys_timer_gettime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timer_gettime_common(ctx)
}

pub(super) fn sys_timer_getoverrun(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timer_getoverrun_common(ctx)
}

pub(super) fn sys_timer_settime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timer_settime_common(ctx)
}

pub(super) fn sys_timer_delete(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timer_delete_common(ctx)
}

/// Linux `sigevent`（64 位）前 20 字节：`sigev_value`(8) + `sigev_signo`(4)
/// + `sigev_notify`(4) + `sigev_notify_thread_id`(4)。
const SIGEV_HEADER_SIZE: usize = 20;
const SIGEV_VALUE_OFF: usize = 0;
const SIGEV_SIGNO_OFF: usize = 8;
const SIGEV_NOTIFY_OFF: usize = 12;
const SIGEV_TID_OFF: usize = 16;

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
// SIGEV_THREAD(2) 由 glibc 用 SIGEV_THREAD_ID + 自建线程翻译，内核不接受。
const SIGEV_THREAD_ID: i32 = 4;

const TIMER_ABSTIME: i32 = 1;

/// Linux `struct itimerspec`（64 位，32 字节）。
const ITIMERSPEC_SIZE: usize = 32;
const ITIMERSPEC_INTERVAL_OFF: usize = 0;
const ITIMERSPEC_VALUE_OFF: usize = 16;

fn timer_create_common(ctx: &mut SyscallContext<'_>, _time64: bool) -> Result<usize, Errno> {
    let clockid = ctx.args[0] as i32;
    let sevp = ctx.args[1];
    let timeridp = ctx.args[2];
    let clock = sched::posix_timer::TimerClock::from_clockid(clockid).ok_or(Errno::EINVAL)?;

    // sigevent 默认：SIGEV_SIGNAL + SIGALRM（sevp == NULL）。
    let mut sigev_value = 0u64;
    let mut signo = SignalNumber::SIGALRM;
    let mut notify = SIGEV_SIGNAL;
    let mut thread_id = 0i32;
    if sevp != 0 {
        let mut buf = [0u8; SIGEV_HEADER_SIZE];
        copy_from_user(sevp, &mut buf).map_err(|e| e.as_errno())?;
        sigev_value = u64::from_le_bytes(
            buf[SIGEV_VALUE_OFF..SIGEV_VALUE_OFF + 8]
                .try_into()
                .unwrap(),
        );
        signo = SignalNumber::from_raw(i32::from_le_bytes(
            buf[SIGEV_SIGNO_OFF..SIGEV_SIGNO_OFF + 4]
                .try_into()
                .unwrap(),
        ))
        .ok_or(Errno::EINVAL)?;
        notify = i32::from_le_bytes(
            buf[SIGEV_NOTIFY_OFF..SIGEV_NOTIFY_OFF + 4]
                .try_into()
                .unwrap(),
        );
        thread_id = i32::from_le_bytes(buf[SIGEV_TID_OFF..SIGEV_TID_OFF + 4].try_into().unwrap());
    }

    let caller = ctx.task();
    let caller_tgid = caller.thread_group().tgid();
    let target_tid = match notify {
        SIGEV_NONE => {
            let _ = sigev_value;
            let _ = signo;
            None
        }
        SIGEV_SIGNAL => {
            let _ = sigev_value;
            let _ = signo;
            None
        }
        SIGEV_THREAD_ID => {
            if thread_id <= 0 {
                return Err(Errno::EINVAL);
            }
            // 目标线程必须属于调用者线程组（Linux 语义）。
            let tid = thread_id as PidT;
            let target = sched::posix_timer::lookup_task(tid).ok_or(Errno::EINVAL)?;
            if target.is_kernel_task() || target.thread_group().tgid() != caller_tgid {
                return Err(Errno::EINVAL);
            }
            Some(tid)
        }
        _ => return Err(Errno::EINVAL),
    };
    let sigev = match notify {
        SIGEV_NONE => sched::posix_timer::SigevNotify::None,
        _ => sched::posix_timer::SigevNotify::Signal {
            signo,
            value: sigev_value,
        },
    };
    let timer_t = sched::posix_timer::create(clock, &caller, sigev, target_tid)?;
    copy_to_user(timeridp, &timer_t.to_le_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

fn timer_settime_common(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timer_t = ctx.args[0] as u32;
    let flags = ctx.args[1] as i32;
    let new_value = ctx.args[2];
    let old_value = ctx.args[3];
    if flags & !TIMER_ABSTIME != 0 {
        return Err(Errno::EINVAL);
    }
    let mut buf = [0u8; ITIMERSPEC_SIZE];
    copy_from_user(new_value, &mut buf).map_err(|e| e.as_errno())?;
    let read_timespec = |off: usize| -> Result<(i64, i64), Errno> {
        let sec = i64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        let nsec = i64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return Err(Errno::EINVAL);
        }
        Ok((sec, nsec))
    };
    let (interval_sec, interval_nsec) = read_timespec(ITIMERSPEC_INTERVAL_OFF)?;
    let (value_sec, value_nsec) = read_timespec(ITIMERSPEC_VALUE_OFF)?;
    let interval_ns = (interval_sec as u64) * 1_000_000_000 + interval_nsec as u64;
    let value_ns = (value_sec as u64) * 1_000_000_000 + value_nsec as u64;

    // 旧值快照（timer_settime 的 old_value 输出上一次挂载的剩余时间）。
    if old_value != 0 {
        let (remaining_ns, old_interval_ns) =
            sched::posix_timer::gettime(timer_t).unwrap_or((0, 0));
        let mut old = [0u8; ITIMERSPEC_SIZE];
        write_timespec_pair(&mut old, ITIMERSPEC_INTERVAL_OFF, old_interval_ns);
        write_timespec_pair(&mut old, ITIMERSPEC_VALUE_OFF, remaining_ns);
        copy_to_user(old_value, &old).map_err(|e| e.as_errno())?;
    }

    if value_ns == 0 {
        // 解除定时器。
        if !sched::posix_timer::arm(
            timer_t,
            sched::posix_timer::TimerSpec {
                deadline_ns: 0,
                interval_ns: 0,
            },
        ) {
            return Err(Errno::EINVAL);
        }
        return Ok(0);
    }

    let clock = sched::posix_timer::clock_of(timer_t).ok_or(Errno::EINVAL)?;
    let absolute = flags & TIMER_ABSTIME != 0;
    if absolute && clock.is_cpu_clock() {
        // Linux：CPU 时钟定时器不支持 TIMER_ABSTIME。
        return Err(Errno::EINVAL);
    }
    let now = sched::posix_timer::now_in_domain(clock, &ctx.task());
    let deadline = match (clock, absolute) {
        (sched::posix_timer::TimerClock::Realtime, true) => {
            // 绝对 REALTIME：换算到单调域。
            let offset = crate::vdso::realtime_ns().saturating_sub(crate::vdso::monotonic_ns());
            value_ns.saturating_sub(offset)
        }
        (_, true) => value_ns,
        (_, false) => now.saturating_add(value_ns),
    };
    if !sched::posix_timer::arm(
        timer_t,
        sched::posix_timer::TimerSpec {
            deadline_ns: deadline,
            interval_ns: interval_ns,
        },
    ) {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

fn timer_gettime_common(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timer_t = ctx.args[0] as u32;
    let curr = ctx.args[1];
    let (remaining_ns, interval_ns) = sched::posix_timer::gettime(timer_t).ok_or(Errno::EINVAL)?;
    let mut out = [0u8; ITIMERSPEC_SIZE];
    write_timespec_pair(&mut out, ITIMERSPEC_INTERVAL_OFF, interval_ns);
    write_timespec_pair(&mut out, ITIMERSPEC_VALUE_OFF, remaining_ns);
    copy_to_user(curr, &mut out).map_err(|e| e.as_errno())?;
    Ok(0)
}

fn timer_getoverrun_common(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timer_t = ctx.args[0] as u32;
    let overrun = sched::posix_timer::getoverrun(timer_t).ok_or(Errno::EINVAL)?;
    Ok(overrun as usize)
}

fn timer_delete_common(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timer_t = ctx.args[0] as u32;
    if !sched::posix_timer::delete(timer_t) {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// 把 ns 写成一对 `struct timespec`（sec i64 @off，nsec i64 @off+8）。
fn write_timespec_pair(out: &mut [u8], off: usize, ns: u64) {
    put_i64(out, off, (ns / 1_000_000_000) as i64);
    put_i64(out, off + 8, (ns % 1_000_000_000) as i64);
}

pub(super) fn sys_clock_settime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    clock_settime_common(ctx)
}

pub(super) fn sys_ptrace(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let request = ctx.args[0];
    let pid = ctx.args[1] as i32;
    let addr = ctx.args[2];
    let data = ctx.args[3];

    match request {
        PTRACE_TRACEME => {
            sched::operation::ptrace_traceme()?;
            Ok(0)
        }
        PTRACE_ATTACH => {
            sched::operation::ptrace_attach(pid)?;
            Ok(0)
        }
        PTRACE_SEIZE => {
            sched::operation::ptrace_seize(pid)?;
            Ok(0)
        }
        PTRACE_CONT => {
            sched::operation::ptrace_cont(pid, ptrace_signal_arg(data)?)?;
            Ok(0)
        }
        PTRACE_SYSCALL => {
            sched::operation::ptrace_syscall(pid, ptrace_signal_arg(data)?)?;
            Ok(0)
        }
        PTRACE_SINGLESTEP => {
            let target = ptrace_target_task(pid)?;
            // 指令补丁法单步：把目标 PC 处的指令替换为断点指令。
            arm_singlestep(&target)?;
            sched::operation::ptrace_singlestep(pid, ptrace_signal_arg(data)?)?;
            Ok(0)
        }
        PTRACE_KILL => {
            sched::operation::ptrace_kill(pid)?;
            Ok(0)
        }
        PTRACE_DETACH => {
            sched::operation::ptrace_detach(pid, ptrace_signal_arg(data)?)?;
            Ok(0)
        }
        PTRACE_SETOPTIONS | PTRACE_OLDSETOPTIONS => {
            ptrace_set_options(pid, data as u64)?;
            Ok(0)
        }
        PTRACE_INTERRUPT => {
            sched::operation::ptrace_interrupt(pid)?;
            Ok(0)
        }
        PTRACE_LISTEN => {
            sched::operation::ptrace_cont(pid, None)?;
            Ok(0)
        }
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            // Linux：PEEKDATA/POKEDATA 的第四个参数直接携带数据（word），
            // 不做指针解引用——PEEK 以系统调用返回值回传。
            let target = ptrace_target_task(pid)?;
            let vm = ptrace_target_vm(&target)?;
            let mut raw = [0u8; 8];
            vm.copy_user_bytes_in(addr, &mut raw)?;
            Ok(usize::from_ne_bytes(raw))
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            let target = ptrace_target_task(pid)?;
            let vm = ptrace_target_vm(&target)?;
            let raw = (data as u64).to_ne_bytes();
            vm.copy_user_bytes_out(addr, &raw)?;
            if request == PTRACE_POKETEXT {
                <arch::CurrentTaskOps as general::TaskOps>::sync_icache();
            }
            Ok(0)
        }
        PTRACE_PEEKUSR => {
            let target = ptrace_target_task(pid)?;
            let value = ptrace_peek_usr(&target, addr)?;
            Ok(value)
        }
        PTRACE_POKEUSR => {
            let target = ptrace_target_task(pid)?;
            ptrace_poke_usr(&target, addr, data)?;
            Ok(0)
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => {
            let target = ptrace_target_task(pid)?;
            let note_type = addr;
            let iov_user = data;
            ptrace_regset(&target, note_type, request == PTRACE_SETREGSET, iov_user)?;
            Ok(0)
        }
        PTRACE_GETSIGINFO => {
            let info = sched::operation::ptrace_getsiginfo(pid)?;
            super::signal::write_siginfo(data, &info)?;
            Ok(0)
        }
        PTRACE_SETSIGINFO => {
            let info = super::signal::read_queued_siginfo_raw(data)?;
            sched::operation::ptrace_setsiginfo(pid, info)?;
            Ok(0)
        }
        PTRACE_GETEVENTMSG => {
            let message = sched::operation::ptrace_geteventmsg(pid)?;
            copy_to_user(data, &(message as u64).to_ne_bytes()).map_err(|e| e.as_errno())?;
            Ok(0)
        }
        PTRACE_GETSIGMASK => {
            let mask = sched::operation::ptrace_get_sigmask(pid)?;
            copy_to_user(data, &mask.raw().to_ne_bytes()).map_err(|e| e.as_errno())?;
            Ok(0)
        }
        PTRACE_SETSIGMASK => {
            let mut raw = [0u8; 8];
            copy_from_user(data, &mut raw).map_err(|e| e.as_errno())?;
            let mask = sched::SigSet::from_raw(u64::from_ne_bytes(raw));
            sched::operation::ptrace_set_sigmask(pid, mask)?;
            Ok(0)
        }
        PTRACE_PEEKSIGINFO => {
            // 返回目标排队中的 pending siginfo（非破坏性快照）。
            // struct ptrace_peeksiginfo_args { u64 off; u32 flags; s32 nr; }。
            let target = ptrace_target_task(pid)?;
            let mut args = [0u8; 16];
            copy_from_user(addr, &mut args).map_err(|e| e.as_errno())?;
            let off = u64::from_le_bytes(args[0..8].try_into().unwrap()) as usize;
            let flags = u32::from_le_bytes(args[8..12].try_into().unwrap());
            let nr = i32::from_le_bytes(args[12..16].try_into().unwrap());
            const PTRACE_PEEKSIGINFO_SHARED: u32 = 1;
            // 取舍：负 nr 的倒序读取（Linux 反向遍历）未实现，按 EINVAL 拒绝。
            if flags & !PTRACE_PEEKSIGINFO_SHARED != 0 || nr < 0 {
                return Err(Errno::EINVAL);
            }
            let infos = if flags & PTRACE_PEEKSIGINFO_SHARED != 0 {
                target.shared_signal().shared_pending_infos_snapshot()
            } else {
                target.signal.pending_infos_snapshot()
            };
            let mut copied = 0usize;
            for (index, info) in infos.iter().skip(off).enumerate() {
                if copied >= nr as usize {
                    break;
                }
                let slot = data.checked_add(index * 128).ok_or(Errno::EFAULT)?;
                super::signal::write_siginfo(slot, info)?;
                copied += 1;
            }
            Ok(copied)
        }
        PTRACE_GET_SYSCALL_INFO => {
            let target = ptrace_target_task(pid)?;
            write_ptrace_syscall_info(&target, data)?;
            Ok(0)
        }
        PTRACE_GET_RSEQ_CONFIGURATION => {
            let target = ptrace_target_task(pid)?;
            write_ptrace_rseq_configuration(&target, data)?;
            Ok(0)
        }
        // PTRACE_GETFDPIC 只在无 MMU 的 FDPIC ABI 上有意义，LA/RV 均为
        // ELF + MMU，Linux 同架构同样返回 EINVAL。
        PTRACE_GETFDPIC => Err(Errno::EINVAL),
        // 取舍：SECCOMP_GET_FILTER/GET_METADATA 需反射 seccomp 过滤器 BPF
        // 程序（general::seccomp 属只读边界，未暴露过滤器镜像接口），暂保持
        // EINVAL；SYSCALL_USER_DISPATCH / SET_SYSCALL_INFO 依赖 seccomp 调度
        // 路径，未实现。
        PTRACE_SECCOMP_GET_FILTER | PTRACE_SECCOMP_GET_METADATA => Err(Errno::EINVAL),
        PTRACE_SET_SYSCALL_USER_DISPATCH_CONFIG
        | PTRACE_GET_SYSCALL_USER_DISPATCH_CONFIG
        | PTRACE_SET_SYSCALL_INFO => Err(Errno::EINVAL),
        _ => Err(Errno::EIO),
    }
}

/// 校验 `PTRACE_SETOPTIONS` 的选项位并安装到目标。
fn ptrace_set_options(pid: i32, options: u64) -> Result<(), Errno> {
    const PTRACE_O_TRACESYSGOOD: u64 = 0x0000_0001;
    const PTRACE_O_TRACEFORK: u64 = 0x0000_0002;
    const PTRACE_O_TRACEVFORK: u64 = 0x0000_0004;
    const PTRACE_O_TRACECLONE: u64 = 0x0000_0008;
    const PTRACE_O_TRACEEXEC: u64 = 0x0000_0010;
    const PTRACE_O_TRACEEXIT: u64 = 0x0000_0040;
    const PTRACE_O_TRACESECCOMP: u64 = 0x0000_0080;
    const PTRACE_O_EXITKILL: u64 = 0x0010_0000;
    const PTRACE_O_SUSPEND_SECCOMP: u64 = 0x0020_0000;
    const KNOWN: u64 = PTRACE_O_TRACESYSGOOD
        | PTRACE_O_TRACEFORK
        | PTRACE_O_TRACEVFORK
        | PTRACE_O_TRACECLONE
        | PTRACE_O_TRACEEXEC
        | PTRACE_O_TRACEEXIT
        | PTRACE_O_TRACESECCOMP
        | PTRACE_O_EXITKILL
        | PTRACE_O_SUSPEND_SECCOMP;
    if options & !KNOWN != 0 {
        return Err(Errno::EINVAL);
    }
    sched::operation::ptrace_set_options(pid, options)?;
    Ok(())
}

/// 取 ptrace 目标（已跟踪 + 权限）。
fn ptrace_target_task(pid: i32) -> Result<Arc<Task>, Errno> {
    let target = sched::operation::lookup_pid(pid)?;
    if target.is_kernel_task() || !target.is_ptrace_traced() {
        return Err(Errno::ESRCH);
    }
    let me = sched::current_task_direct();
    if !sched::operation::ptrace_may_access(&me, &target) {
        return Err(Errno::EPERM);
    }
    Ok(target)
}

fn ptrace_target_vm(target: &Arc<Task>) -> Result<Arc<VmSpace>, Errno> {
    task_vm_space(target).ok_or(Errno::EIO)
}

/// 取目标停止时保存的用户 trap frame。
fn ptrace_target_frame(target: &Arc<Task>) -> Result<hal::user_context::UserTrapFrame, Errno> {
    // 优先取 syscall 入口的 arch 快照（arch 在 syscall 分发前保存）；
    // 回退到恢复/写回路径保存的 UserTrapFrame（POKEUSR 写回后、以及
    // 信号/单步场景）。
    #[cfg(target_arch = "loongarch64")]
    let arch_frame = target
        .ext_lookup(sched::TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<arch::loongarch64::TrapFrame>().ok());
    #[cfg(target_arch = "riscv64")]
    let arch_frame = target
        .ext_lookup(sched::TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<arch::riscv64::TrapFrame>().ok());
    if let Some(frame) = arch_frame {
        let raw = Arc::as_ptr(&frame) as usize;
        return Ok(hal::user_context::UserTrapFrame::from_context(raw));
    }
    let frame = target
        .ext_lookup(sched::TASKEXT_USER_TRAP_FRAME)
        .and_then(|payload| payload.downcast::<hal::user_context::UserTrapFrame>().ok())
        .ok_or(Errno::EIO)?;
    Ok(*frame)
}

/// 写回目标 trap frame（POKEUSR 使用）。
fn ptrace_store_frame(target: &Arc<Task>, frame: hal::user_context::UserTrapFrame) {
    let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(frame);
    target.ext_install(sched::TASKEXT_USER_TRAP_FRAME, erased);
}

/// `PTRACE_PEEKUSR`：按 mcontext 布局读寄存器字（pc@0、regs@8、每项 8 字节）。
fn ptrace_peek_usr(target: &Arc<Task>, offset: usize) -> Result<usize, Errno> {
    let frame = ptrace_target_frame(target)?;
    let mut mcontext = [0u8; 1024];
    if !frame.write_linux_mcontext(&mut mcontext) {
        return Err(Errno::EIO);
    }
    // mcontext 布局：pc@0；regs@8（32 项）；可能还有 flags 等尾部。
    let regs_len = 8 + 32 * 8;
    if offset < 8 {
        return Ok(u64::from_ne_bytes(mcontext[0..8].try_into().unwrap()) as usize);
    }
    if offset < regs_len && (offset - 8) % 8 == 0 {
        let index = (offset - 8) / 8;
        let start = 8 + index * 8;
        return Ok(u64::from_ne_bytes(mcontext[start..start + 8].try_into().unwrap()) as usize);
    }
    Err(Errno::EIO)
}

/// `PTRACE_POKEUSR`：按 mcontext 布局写寄存器字。
fn ptrace_poke_usr(target: &Arc<Task>, offset: usize, value: usize) -> Result<(), Errno> {
    let mut frame = ptrace_target_frame(target)?;
    let mut mcontext = [0u8; 1024];
    if !frame.write_linux_mcontext(&mut mcontext) {
        return Err(Errno::EIO);
    }
    let regs_len = 8 + 32 * 8;
    if offset < 8 {
        mcontext[0..8].copy_from_slice(&(value as u64).to_ne_bytes());
    } else if offset < regs_len && (offset - 8) % 8 == 0 {
        let index = (offset - 8) / 8;
        let start = 8 + index * 8;
        mcontext[start..start + 8].copy_from_slice(&(value as u64).to_ne_bytes());
    } else {
        return Err(Errno::EIO);
    }
    if !frame.apply_linux_mcontext(&mcontext) {
        return Err(Errno::EIO);
    }
    ptrace_store_frame(target, frame);
    Ok(())
}

/// `PTRACE_GETREGSET`/`SETREGSET`：`struct iovec { base, len }` 指向的数据区。
fn ptrace_regset(
    target: &Arc<Task>,
    note_type: usize,
    set: bool,
    iov_user: usize,
) -> Result<(), Errno> {
    let mut iov = [0u8; 16];
    copy_from_user(iov_user, &mut iov).map_err(|e| e.as_errno())?;
    let base = u64::from_le_bytes(iov[0..8].try_into().unwrap()) as usize;
    let mut len = u64::from_le_bytes(iov[8..16].try_into().unwrap()) as usize;

    const NT_PRSTATUS: usize = 1;
    const NT_FPREGSET: usize = 2;

    match note_type {
        NT_PRSTATUS => {
            let frame = ptrace_target_frame(target)?;
            let mut mcontext = [0u8; 1024];
            if !frame.write_linux_mcontext(&mut mcontext) {
                return Err(Errno::EIO);
            }
            let size = linux_mcontext_size();
            if set {
                if len < size {
                    return Err(Errno::EIO);
                }
                let mut input = [0u8; 1024];
                copy_from_user(base, &mut input[..size]).map_err(|e| e.as_errno())?;
                let mut frame = ptrace_target_frame(target)?;
                if !frame.apply_linux_mcontext(&input) {
                    return Err(Errno::EIO);
                }
                ptrace_store_frame(target, frame);
            } else {
                if len < size {
                    len = size;
                    write_iov_len(iov_user, len)?;
                    return Ok(());
                }
                copy_to_user(base, &mcontext[..size]).map_err(|e| e.as_errno())?;
            }
            Ok(())
        }
        NT_FPREGSET => {
            // 架构 trap frame 在 syscall 入口保存了浮点寄存器
            // （LA：f0-f31+fcsr+fcc；RV：f0-f31+fcsr），这里按 Linux
            // `struct user_fpregs_struct` 布局读写，供 gdb/CRIU 使用。
            let size = linux_fpregset_size();
            if set {
                if len < size {
                    return Err(Errno::EIO);
                }
                let mut input = [0u8; 272];
                copy_from_user(base, &mut input[..size]).map_err(|e| e.as_errno())?;
                ptrace_write_arch_fpregs(target, &input[..size])?;
            } else {
                if len < size {
                    len = size;
                    write_iov_len(iov_user, len)?;
                    return Ok(());
                }
                let out = ptrace_read_arch_fpregs(target)?;
                copy_to_user(base, &out).map_err(|e| e.as_errno())?;
            }
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}

fn write_iov_len(iov_user: usize, len: usize) -> Result<(), Errno> {
    copy_to_user(iov_user + 8, &(len as u64).to_ne_bytes()).map_err(|e| e.as_errno())
}

/// Linux `struct user_regs_struct`（mcontext 布局）的大小。
fn linux_mcontext_size() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        8 + 32 * 8 + 4 // pc + regs + flags
    }
    #[cfg(target_arch = "riscv64")]
    {
        8 + 32 * 8 // pc + regs
    }
}

/// Linux `struct user_fpregs_struct` 的大小。
///
/// - loongarch64：`{ u64 fpr[32]; u64 fcc; u32 fcsr; }`，8 字节对齐后 272；
/// - riscv64：`{ u64 f[32]; u64 fcsr; }`，264。
fn linux_fpregset_size() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        32 * 8 + 8 + 4 + 4
    }
    #[cfg(target_arch = "riscv64")]
    {
        32 * 8 + 8
    }
}

/// 从架构 trap frame 读出浮点寄存器，按 `struct user_fpregs_struct` 布局编码。
fn ptrace_read_arch_fpregs(target: &Arc<Task>) -> Result<Vec<u8>, Errno> {
    #[cfg(target_arch = "loongarch64")]
    {
        let frame = target
            .ext_lookup(sched::TASKEXT_PTRACE_FRAME)
            .and_then(|payload| payload.downcast::<arch::loongarch64::TrapFrame>().ok())
            .ok_or(Errno::EIO)?;
        let mut out = vec![0u8; linux_fpregset_size()];
        for (index, reg) in frame.f.iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&reg.to_le_bytes());
        }
        out[256..264].copy_from_slice(&frame.fcc.to_le_bytes());
        out[264..268].copy_from_slice(&(frame.fcsr as u32).to_le_bytes());
        Ok(out)
    }
    #[cfg(target_arch = "riscv64")]
    {
        let frame = target
            .ext_lookup(sched::TASKEXT_PTRACE_FRAME)
            .and_then(|payload| payload.downcast::<arch::riscv64::TrapFrame>().ok())
            .ok_or(Errno::EIO)?;
        let mut out = vec![0u8; linux_fpregset_size()];
        for (index, reg) in frame.f.iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&reg.to_le_bytes());
        }
        out[256..264].copy_from_slice(&(frame.fcsr as u64).to_le_bytes());
        Ok(out)
    }
}

/// 把 `struct user_fpregs_struct` 布局的字节写回架构 trap frame。
fn ptrace_write_arch_fpregs(target: &Arc<Task>, bytes: &[u8]) -> Result<(), Errno> {
    let size = linux_fpregset_size();
    if bytes.len() < size {
        return Err(Errno::EIO);
    }
    #[cfg(target_arch = "loongarch64")]
    {
        let frame = target
            .ext_lookup(sched::TASKEXT_PTRACE_FRAME)
            .and_then(|payload| payload.downcast::<arch::loongarch64::TrapFrame>().ok())
            .ok_or(Errno::EIO)?;
        let mut new = *frame;
        for (index, reg) in new.f.iter_mut().enumerate() {
            *reg = u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().unwrap());
        }
        new.fcc = u64::from_le_bytes(bytes[256..264].try_into().unwrap());
        new.fcsr = u32::from_le_bytes(bytes[264..268].try_into().unwrap()) as u64;
        let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(new);
        target
            .ext_replace(sched::TASKEXT_PTRACE_FRAME, erased)
            .map_err(|_| Errno::EIO)?;
        Ok(())
    }
    #[cfg(target_arch = "riscv64")]
    {
        let frame = target
            .ext_lookup(sched::TASKEXT_PTRACE_FRAME)
            .and_then(|payload| payload.downcast::<arch::riscv64::TrapFrame>().ok())
            .ok_or(Errno::EIO)?;
        let mut new = *frame;
        for (index, reg) in new.f.iter_mut().enumerate() {
            *reg = u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().unwrap());
        }
        new.fcsr = u32::from_le_bytes(bytes[256..260].try_into().unwrap());
        let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(new);
        target
            .ext_replace(sched::TASKEXT_PTRACE_FRAME, erased)
            .map_err(|_| Errno::EIO)?;
        Ok(())
    }
}

/// 单步断点：把目标 PC 处的指令替换为断点指令。
fn arm_singlestep(target: &Arc<Task>) -> Result<(), Errno> {
    if target.singlestep_armed() {
        return Ok(());
    }
    let frame = ptrace_target_frame(target)?;
    let pc = frame.pc();
    let vm = ptrace_target_vm(target)?;
    let mut insn = [0u8; 4];
    vm.copy_user_bytes_in(pc, &mut insn)?;
    let original = u32::from_ne_bytes(insn);
    let breakpoint = breakpoint_insn();
    vm.copy_user_bytes_out(pc, &breakpoint.to_ne_bytes())?;
    <arch::CurrentTaskOps as general::TaskOps>::sync_icache();
    target.arm_singlestep(pc, original);
    Ok(())
}

/// 架构断点指令（loongarch64 `break 0` / riscv64 `ebreak`）。
fn breakpoint_insn() -> u32 {
    #[cfg(target_arch = "loongarch64")]
    {
        0x2a000000 // break 0
    }
    #[cfg(target_arch = "riscv64")]
    {
        0x00100073 // ebreak
    }
}

/// 单步断点陷阱钩子（arch 的 break trap 在用户态调用）：命中则恢复原指令
/// 并把目标置为 `SIGTRAP` stop。
pub(crate) fn ptrace_singlestep_trap_hook(pc: usize) -> bool {
    let me = sched::current_task_direct();
    if !me.singlestep_armed() {
        return false;
    }
    if me.singlestep_addr() != pc {
        me.clear_singlestep();
        return false;
    }
    let insn = me.take_singlestep_insn();
    me.clear_singlestep();
    if let Some(insn) = insn {
        if let Some(vm) = task_vm_space(&me) {
            let _ = vm.copy_user_bytes_out(pc, &insn.to_ne_bytes());
        }
        <arch::CurrentTaskOps as general::TaskOps>::sync_icache();
    }
    me.set_ptrace_stop_event(0);
    me.clear_ptrace_last_siginfo();
    sched::operation::ptrace_mark_stopped(&me, sched::SignalNumber::SIGTRAP);
    true
}

/// clone 后安装命名空间：`CLONE_NEWUTS/NEWIPC/NEWTIME/NEWCGROUP` 换子进程
/// 的引用集，`CLONE_NEWNS` 换子进程的 VFS 上下文，`CLONE_NEWPID` 已在
/// clone 前经 pending 命名空间生效。
fn clone_install_namespaces(
    parent: &Arc<Task>,
    child: &Arc<Task>,
    flags: CloneFlags,
) -> Result<(), Errno> {
    use sched::clone_flags::CloneFlags as F;

    let ns_flags = flags.raw()
        & (F::CLONE_NEWNS
            | F::CLONE_NEWCGROUP
            | F::CLONE_NEWUTS
            | F::CLONE_NEWIPC
            | F::CLONE_NEWPID);
    if ns_flags == 0 {
        return Ok(());
    }
    if !parent
        .credentials()
        .has_cap(sched::ids::Capability::SysAdmin)
    {
        return Err(Errno::EPERM);
    }
    let parent_ns = crate::ns::task_ns(parent);
    if flags.has(F::CLONE_NEWPID) {
        // 子进程进新 pid 命名空间（sched 的 child_pid_ns hook 已消费）。
        // 无需额外动作；验证子进程确实注册进了新 ns。
    }
    if flags.has(F::CLONE_NEWNS)
        || flags.has(F::CLONE_NEWCGROUP)
        || flags.has(F::CLONE_NEWUTS)
        || flags.has(F::CLONE_NEWIPC)
    {
        let mut proxy = crate::ns::NsProxy {
            uts: Arc::clone(&parent_ns.uts),
            ipc: Arc::clone(&parent_ns.ipc),
            time: Arc::clone(&parent_ns.time),
            cgroup: Arc::clone(&parent_ns.cgroup),
            pid: crate::ns::child_pid_namespace(parent),
            pending_pid: sched::sync::Spinlock::new(None),
        };
        if flags.has(F::CLONE_NEWUTS) {
            proxy.uts =
                ns::UtsNamespace::new(&parent_ns.uts.hostname(), &parent_ns.uts.domainname());
        }
        if flags.has(F::CLONE_NEWIPC) {
            proxy.ipc = crate::ns::IpcNamespace::new();
        }
        if flags.has(F::CLONE_NEWCGROUP) {
            proxy.cgroup = ns::CgroupNamespace::new();
        }
        let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(proxy);
        child.ext_install(crate::ns::TASKEXT_NS, erased);
        if flags.has(F::CLONE_NEWNS) {
            if let Some(vfs_ctx) = child
                .ext_lookup(sched::TASKEXT_VFS_CONTEXT)
                .and_then(|payload| payload.downcast::<general::vfs::VfsContext>().ok())
            {
                if let Ok(forked) = vfs_ctx.clone_with_new_ns() {
                    let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(forked);
                    child.ext_install(sched::TASKEXT_VFS_CONTEXT, erased);
                }
            }
        }
    }
    Ok(())
}

/// 命名空间 provider（procfs `/proc/<pid>/ns/*` 使用）。
pub(crate) fn ns_provider(pid: i32, kind: ProcNsKind) -> Option<Arc<dyn ns::Namespace>> {
    let task = sched::operation::lookup_pid(pid).ok()?;
    let proxy = crate::ns::task_ns(&task);
    match kind {
        ProcNsKind::Uts => Some(proxy.uts.clone() as Arc<dyn ns::Namespace>),
        ProcNsKind::Ipc => Some(proxy.ipc.clone() as Arc<dyn ns::Namespace>),
        ProcNsKind::Time => Some(proxy.time.clone() as Arc<dyn ns::Namespace>),
        ProcNsKind::Cgroup => Some(proxy.cgroup.clone() as Arc<dyn ns::Namespace>),
        ProcNsKind::Pid => Some(proxy.pid.clone() as Arc<dyn ns::Namespace>),
        ProcNsKind::Mount => task
            .ext_lookup(sched::TASKEXT_VFS_CONTEXT)
            .and_then(|payload| payload.downcast::<general::vfs::VfsContext>().ok())
            .map(|ctx| Arc::clone(&ctx.mount_ns) as Arc<dyn ns::Namespace>),
        ProcNsKind::User | ProcNsKind::Net => None,
    }
}

/// `PTRACE_O_TRACEFORK/VFORK/CLONE`：克隆事件通知（父进程停止）。
fn ptrace_notify_fork(parent: &Arc<Task>, flags: CloneFlags, child_pid: i32) {
    const PTRACE_O_TRACEFORK: u64 = 0x0000_0002;
    const PTRACE_O_TRACEVFORK: u64 = 0x0000_0004;
    const PTRACE_O_TRACECLONE: u64 = 0x0000_0008;
    const PTRACE_EVENT_FORK: u16 = 1;
    const PTRACE_EVENT_VFORK: u16 = 2;
    const PTRACE_EVENT_CLONE: u16 = 3;

    if !parent.is_ptrace_traced() {
        return;
    }
    let options = parent.ptrace_options();
    let (event, mask) = if flags.has(CloneFlags::CLONE_VFORK) {
        (PTRACE_EVENT_VFORK, PTRACE_O_TRACEVFORK)
    } else if flags.has(CloneFlags::CLONE_THREAD) {
        (PTRACE_EVENT_CLONE, PTRACE_O_TRACECLONE)
    } else {
        (PTRACE_EVENT_FORK, PTRACE_O_TRACEFORK)
    };
    if options & mask == 0 {
        return;
    }
    parent.set_ptrace_event_msg(child_pid as i64);
    parent.set_ptrace_stop_event(event);
    parent.clear_ptrace_last_siginfo();
    sched::operation::ptrace_mark_stopped(parent, sched::SignalNumber::SIGTRAP);
}

/// `PTRACE_GET_SYSCALL_INFO`：`struct ptrace_syscall_info`（80 字节）。
fn write_ptrace_syscall_info(target: &Arc<Task>, user: usize) -> Result<(), Errno> {
    const PTRACE_SYSCALL_INFO_NONE: u8 = 0;
    const PTRACE_SYSCALL_INFO_ENTRY: u8 = 1;
    const PTRACE_SYSCALL_INFO_EXIT: u8 = 2;
    const PTRACE_SYSCALL_INFO_SECCOMP: u8 = 3;
    #[cfg(target_arch = "loongarch64")]
    const AUDIT_ARCH: u32 = 0x4000_0102; // AUDIT_ARCH_LOONGARCH64
    #[cfg(target_arch = "riscv64")]
    const AUDIT_ARCH: u32 = 0x4000_00f3; // AUDIT_ARCH_RISCV64

    let (state, nr, args, ret) = target.ptrace_syscall_info();
    let frame = ptrace_target_frame(target)?;
    let ip = frame.pc() as u64;
    let sp = frame.sp() as u64;
    let mut raw = [0u8; 80];
    match state {
        PTRACE_SYSCALL_INFO_ENTRY => {
            raw[0] = PTRACE_SYSCALL_INFO_ENTRY;
            raw[4..8].copy_from_slice(&AUDIT_ARCH.to_le_bytes());
            raw[8..16].copy_from_slice(&ip.to_le_bytes());
            raw[16..24].copy_from_slice(&sp.to_le_bytes());
            raw[24..32].copy_from_slice(&(nr as u64).to_le_bytes());
            for (index, arg) in args.iter().enumerate() {
                raw[32 + index * 8..40 + index * 8].copy_from_slice(&arg.to_le_bytes());
            }
        }
        PTRACE_SYSCALL_INFO_EXIT => {
            raw[0] = PTRACE_SYSCALL_INFO_EXIT;
            raw[4..8].copy_from_slice(&AUDIT_ARCH.to_le_bytes());
            raw[8..16].copy_from_slice(&ip.to_le_bytes());
            raw[16..24].copy_from_slice(&sp.to_le_bytes());
            raw[24..32].copy_from_slice(&(ret as u64).to_le_bytes());
            raw[32] = (ret < 0 && ret > -4096) as u8; // is_error
        }
        PTRACE_SYSCALL_INFO_SECCOMP => {
            raw[0] = PTRACE_SYSCALL_INFO_SECCOMP;
            raw[4..8].copy_from_slice(&AUDIT_ARCH.to_le_bytes());
            raw[8..16].copy_from_slice(&ip.to_le_bytes());
            raw[16..24].copy_from_slice(&sp.to_le_bytes());
            raw[24..32].copy_from_slice(&(nr as u64).to_le_bytes());
            for (index, arg) in args.iter().enumerate() {
                raw[32 + index * 8..40 + index * 8].copy_from_slice(&arg.to_le_bytes());
            }
        }
        _ => raw[0] = PTRACE_SYSCALL_INFO_NONE,
    }
    copy_to_user(user, &raw).map_err(|e| e.as_errno())?;
    Ok(())
}

/// `PTRACE_GET_RSEQ_CONFIGURATION`：`struct ptrace_rseq_configuration`（24 字节）。
fn write_ptrace_rseq_configuration(target: &Arc<Task>, user: usize) -> Result<(), Errno> {
    let mut raw = [0u8; 24];
    let (abi_pointer, abi_size, signature) = target
        .rseq_registration_if_registered()
        .map(|registration| (registration.ptr, registration.len, registration.signature))
        .unwrap_or((0, 0, 0));
    raw[0..8].copy_from_slice(&(abi_pointer as u64).to_le_bytes());
    raw[8..12].copy_from_slice(&abi_size.to_le_bytes());
    raw[12..16].copy_from_slice(&signature.to_le_bytes());
    copy_to_user(user, &raw).map_err(|e| e.as_errno())?;
    Ok(())
}

pub(super) fn sys_getresuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let creds = ctx.task().credentials();
    copy_to_user(ctx.args[0], &creds.uid.0.to_le_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(ctx.args[1], &creds.euid.0.to_le_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(ctx.args[2], &creds.suid.0.to_le_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_getresgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let creds = ctx.task().credentials();
    copy_to_user(ctx.args[0], &creds.gid.0.to_le_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(ctx.args[1], &creds.egid.0.to_le_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(ctx.args[2], &creds.sgid.0.to_le_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_sethostname(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    require_cap(ctx.task(), Capability::SysAdmin)?;
    let user = ctx.args[0];
    let len = ctx.args[1];
    if len > UTS_NAME_MAX {
        return Err(Errno::EINVAL);
    }
    let mut value = [0u8; UTS_FIELD_LEN];
    if len != 0 {
        copy_from_user(user, &mut value[..len]).map_err(|e| e.as_errno())?;
    }
    crate::ns::task_ns(ctx.task())
        .uts
        .set_hostname(&value[..len])
        .map(|_| 0)
}

pub(super) fn sys_setdomainname(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    require_cap(ctx.task(), Capability::SysAdmin)?;
    let user = ctx.args[0];
    let len = ctx.args[1];
    if len > UTS_NAME_MAX {
        return Err(Errno::EINVAL);
    }
    let mut value = [0u8; UTS_FIELD_LEN];
    if len != 0 {
        copy_from_user(user, &mut value[..len]).map_err(|e| e.as_errno())?;
    }
    crate::ns::task_ns(ctx.task())
        .uts
        .set_domainname(&value[..len])
        .map(|_| 0)
}

pub(super) fn sys_umask(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = vfs::current_vfs_context().ok_or(Errno::EBADF)?;
    let old = vfs_ctx.set_umask(vfs::FileMode::new((ctx.args[0] & 0o777) as u16));
    Ok(old.bits() as usize)
}

pub(super) fn sys_settimeofday(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let tv = ctx.args[0];
    if tv == 0 {
        return Ok(0);
    }
    require_cap(ctx.task(), Capability::SysTime)?;
    let mut raw = [0u8; TIMEVAL_SIZE];
    copy_from_user(tv, &mut raw).map_err(|e| e.as_errno())?;
    let new_ns = timeval_to_ns(&raw)?;
    let old_offset = crate::vdso::realtime_offset_ns();
    crate::vdso::set_realtime_ns(new_ns);
    // 实时钟被设置：取消登记了 TFD_TIMER_CANCEL_ON_SET 的 timerfd。
    if crate::vdso::realtime_offset_ns() != old_offset {
        vfs::timerfd::cancel_timers_on_clock_set();
    }
    Ok(0)
}

pub(super) fn sys_adjtimex(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    adjtimex_common(ctx, 0)
}

/// Linux `struct timex`（musl 64 位布局，208 字节）。
const TIMEX_SIZE: usize = 208;
const TIMEX_MODES_OFF: usize = 0;
const TIMEX_OFFSET_OFF: usize = 8;
const TIMEX_FREQ_OFF: usize = 16;
const TIMEX_MAXERROR_OFF: usize = 24;
const TIMEX_ESTERROR_OFF: usize = 32;
const TIMEX_STATUS_OFF: usize = 40;
const TIMEX_CONSTANT_OFF: usize = 48;
const TIMEX_PRECISION_OFF: usize = 56;
const TIMEX_TOLERANCE_OFF: usize = 64;
const TIMEX_TIME_OFF: usize = 72;
const TIMEX_TICK_OFF: usize = 88;

/// `adjtimex`（arg0 = timex*）与 `clock_adjtime`（arg1 = timex*）共用实现。
fn adjtimex_common(ctx: &mut SyscallContext<'_>, timex_arg: usize) -> Result<usize, Errno> {
    let ptr = ctx.args[timex_arg];
    if ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let mut buf = [0u8; TIMEX_SIZE];
    copy_from_user(ptr, &mut buf).map_err(|e| e.as_errno())?;

    let read_i64 = |off: usize| i64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    let read_i32 = |off: usize| i32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
    let modes = u32::from_le_bytes(
        buf[TIMEX_MODES_OFF..TIMEX_MODES_OFF + 4]
            .try_into()
            .unwrap(),
    );
    if modes & 0x8000 != 0
        && modes != crate::adjtimex::ADJ_OFFSET_SINGLESHOT
        && modes != crate::adjtimex::ADJ_OFFSET_SS_READ
    {
        return Err(Errno::EINVAL);
    }
    if modes != 0 && modes != crate::adjtimex::ADJ_OFFSET_SS_READ {
        require_cap(ctx.task(), Capability::SysTime)?;
    }
    let setoffset = modes & crate::adjtimex::ADJ_SETOFFSET != 0;
    let old_realtime_offset = setoffset.then(crate::vdso::realtime_offset_ns);
    let fields = crate::adjtimex::TimexFields {
        modes,
        offset: read_i64(TIMEX_OFFSET_OFF),
        freq: read_i64(TIMEX_FREQ_OFF),
        maxerror: read_i64(TIMEX_MAXERROR_OFF),
        esterror: read_i64(TIMEX_ESTERROR_OFF),
        status: read_i32(TIMEX_STATUS_OFF),
        constant: read_i64(TIMEX_CONSTANT_OFF),
        tick: read_i64(TIMEX_TICK_OFF),
        time_sec: read_i64(TIMEX_TIME_OFF),
        time_subsec: read_i64(TIMEX_TIME_OFF + 8),
        precision: 0,
        tolerance: 0,
    };

    if fields.modes != 0 && fields.modes != crate::adjtimex::ADJ_OFFSET_SS_READ {
        require_cap(ctx.task(), Capability::SysTime)?;
    }

    let out = crate::adjtimex::do_adjtimex(fields)?;
    if let Some(old_offset) = old_realtime_offset {
        if crate::vdso::realtime_offset_ns() != old_offset {
            vfs::timerfd::cancel_timers_on_clock_set();
        }
    }

    let write_i64 = |buf: &mut [u8; TIMEX_SIZE], off: usize, v: i64| {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    };
    buf[TIMEX_MODES_OFF..TIMEX_MODES_OFF + 4].copy_from_slice(&out.modes.to_le_bytes());
    write_i64(&mut buf, TIMEX_OFFSET_OFF, out.offset);
    write_i64(&mut buf, TIMEX_FREQ_OFF, out.freq);
    write_i64(&mut buf, TIMEX_MAXERROR_OFF, out.maxerror);
    write_i64(&mut buf, TIMEX_ESTERROR_OFF, out.esterror);
    buf[TIMEX_STATUS_OFF..TIMEX_STATUS_OFF + 4].copy_from_slice(&out.status.to_le_bytes());
    write_i64(&mut buf, TIMEX_CONSTANT_OFF, out.constant);
    write_i64(&mut buf, TIMEX_PRECISION_OFF, out.precision);
    write_i64(&mut buf, TIMEX_TOLERANCE_OFF, out.tolerance);
    write_i64(&mut buf, TIMEX_TICK_OFF, out.tick);
    // time 字段：当前 CLOCK_REALTIME；STA_NANO 决定亚秒字段单位。
    let realtime_ns = crate::vdso::realtime_ns();
    write_i64(
        &mut buf,
        TIMEX_TIME_OFF,
        (realtime_ns / 1_000_000_000) as i64,
    );
    let realtime_subsec = if out.status & crate::adjtimex::STA_NANO != 0 {
        realtime_ns % 1_000_000_000
    } else {
        (realtime_ns % 1_000_000_000) / 1_000
    };
    write_i64(&mut buf, TIMEX_TIME_OFF + 8, realtime_subsec as i64);
    copy_to_user(ptr, &buf).map_err(|e| e.as_errno())?;

    // 返回值 = 时钟状态（TIME_OK/TIME_INS/TIME_DEL/TIME_ERROR）。
    Ok(crate::adjtimex::clock_state(out.status) as usize)
}

pub(super) fn sys_perf_event_open(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_clock_adjtime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clockid = ctx.args[0] as i32;
    // Linux 的 clock_adjtime 支持 CLOCK_REALTIME 与 CLOCK_TAI。
    if clockid != crate::vdso::CLOCK_REALTIME as i32 && clockid != CLOCK_TAI as i32 {
        return Err(Errno::EINVAL);
    }
    adjtimex_common(ctx, 1)
}

pub(super) fn sys_setns(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd_raw = ctx.args[0] as isize;
    let nstype = ctx.args[1] as u64;
    if fd_raw < 0 {
        return Err(Errno::EBADF);
    }
    let fdt = general::vfs::current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt
        .get_file(vfs::fdtable::Fd::from_raw(fd_raw as u32))
        .ok_or(Errno::EBADF)?;
    let ns_file = file
        .downcast_ops::<general::vfs::nsfs::NsfsFileOps>()
        .ok_or(Errno::EBADF)?;
    crate::ns::setns(ctx.task(), Arc::clone(ns_file.namespace()), nstype)?;
    Ok(0)
}

pub(super) fn sys_kcmp(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const KCMP_FILE: usize = 0;
    const KCMP_VM: usize = 1;
    const KCMP_FILES: usize = 2;
    const KCMP_FS: usize = 3;
    const KCMP_SIGHAND: usize = 4;
    const KCMP_IO: usize = 5;
    const KCMP_SYSVSEM: usize = 6;
    const KCMP_EPOLL_TFD: usize = 7;

    let pid1 = ctx.args[0] as i32;
    let pid2 = ctx.args[1] as i32;
    let ty = ctx.args[2];
    let idx1 = ctx.args[3];
    let idx2 = ctx.args[4];
    let task1 = lookup_task_for_thread_syscall(pid1, ctx.task())?;
    let task2 = lookup_task_for_thread_syscall(pid2, ctx.task())?;
    require_task_access(ctx.task(), &task1)?;
    require_task_access(ctx.task(), &task2)?;

    match ty {
        KCMP_FILE => {
            let fd1 = kcmp_fd_arg(idx1)?;
            let fd2 = kcmp_fd_arg(idx2)?;
            let fdt1 = task_fdtable(&task1).ok_or(Errno::EBADF)?;
            let fdt2 = task_fdtable(&task2).ok_or(Errno::EBADF)?;
            let file1 = fdt1.get_file(fd1).ok_or(Errno::EBADF)?;
            let file2 = fdt2.get_file(fd2).ok_or(Errno::EBADF)?;
            Ok(kcmp_arc(&file1, &file2))
        }
        KCMP_VM => {
            let vm1 = task_vm_space(&task1).ok_or(Errno::EINVAL)?;
            let vm2 = task_vm_space(&task2).ok_or(Errno::EINVAL)?;
            Ok(kcmp_arc(&vm1, &vm2))
        }
        KCMP_FILES => {
            let fdt1 = task_fdtable(&task1).ok_or(Errno::EINVAL)?;
            let fdt2 = task_fdtable(&task2).ok_or(Errno::EINVAL)?;
            Ok(kcmp_arc(&fdt1, &fdt2))
        }
        KCMP_FS => {
            let fs1 = task_vfs_context(&task1).ok_or(Errno::EINVAL)?;
            let fs2 = task_vfs_context(&task2).ok_or(Errno::EINVAL)?;
            Ok(kcmp_arc(&fs1, &fs2))
        }
        KCMP_SIGHAND => {
            let sig1 = task1.shared_signal();
            let sig2 = task2.shared_signal();
            Ok(kcmp_arc(&sig1, &sig2))
        }
        KCMP_IO => {
            // 本内核无独立 io_context；同线程组（同进程）视为共享同一 I/O 上下文，
            // 与 Linux 对同进程线程的 KCMP_IO 返回 0 一致。
            Ok(kcmp_arc(&task1.thread_group(), &task2.thread_group()))
        }
        // 取舍：KCMP_SYSVSEM 需比对 SysV 信号量 undo 表（TASKEXT_SEM_UNDO 由
        // ipc.rs 维护，属只读边界），KCMP_EPOLL_TFD 需读取 epoll 目标文件
        // （libs/vfs epoll 只读边界）；二者保持 EOPNOTSUPP 并在注释说明。
        KCMP_SYSVSEM | KCMP_EPOLL_TFD => Err(Errno::EOPNOTSUPP),
        _ => Err(Errno::EOPNOTSUPP),
    }
}

pub(super) fn sys_finit_module(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_seccomp(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    use general::seccomp::*;

    const KNOWN_FLAGS: u32 = SECCOMP_FILTER_FLAG_TSYNC
        | SECCOMP_FILTER_FLAG_LOG
        | SECCOMP_FILTER_FLAG_SPEC_ALLOW
        | SECCOMP_FILTER_FLAG_NEW_LISTENER
        | SECCOMP_FILTER_FLAG_TSYNC_ESRCH;
    const KNOWN_ACTIONS: [u32; 8] = [
        SECCOMP_RET_KILL_PROCESS,
        SECCOMP_RET_KILL_THREAD,
        SECCOMP_RET_TRAP,
        SECCOMP_RET_ERRNO,
        SECCOMP_RET_USER_NOTIF,
        SECCOMP_RET_TRACE,
        SECCOMP_RET_LOG,
        SECCOMP_RET_ALLOW,
    ];

    let op = ctx.args[0] as u32;
    let flags = ctx.args[1] as u32;
    let filter_user = ctx.args[2];
    let task = ctx.task();

    match op {
        SECCOMP_SET_MODE_STRICT => {
            if flags != 0 || filter_user != 0 {
                return Err(Errno::EINVAL);
            }
            seccomp_state(task).set_strict();
            Ok(0)
        }
        SECCOMP_SET_MODE_FILTER => {
            if filter_user == 0 {
                return Err(Errno::EFAULT);
            }
            if flags & !KNOWN_FLAGS != 0 {
                return Err(Errno::EINVAL);
            }
            let cred = vfs_cred_from_sched(&task.credentials());
            if !filter_install_allowed(task.no_new_privs(), &cred) {
                return Err(Errno::EACCES);
            }
            // struct sock_fprog { u16 len; u16 pad; ... ptr }
            let mut fprog = [0u8; 16];
            copy_from_user(filter_user, &mut fprog).map_err(|e| e.as_errno())?;
            let len = u16::from_le_bytes(fprog[0..2].try_into().unwrap()) as usize;
            let ptr = u64::from_le_bytes(fprog[8..16].try_into().unwrap()) as usize;
            if ptr == 0 {
                return Err(Errno::EFAULT);
            }
            let mut bytes = vec![0u8; len * 8];
            if len > 0 {
                copy_from_user(ptr, &mut bytes).map_err(|e| e.as_errno())?;
            }
            let insns = parse_program(&bytes)?;
            let filter = SeccompFilter::new(insns, flags)?;
            seccomp_state(task).push_filter(filter);
            Ok(0)
        }
        SECCOMP_GET_ACTION_AVAIL => {
            let action = ctx.args[1] as u32;
            if action & !SECCOMP_RET_ACTION_FULL != 0
                || !KNOWN_ACTIONS.contains(&(action & SECCOMP_RET_ACTION_FULL))
            {
                return Err(Errno::EOPNOTSUPP);
            }
            Ok(0)
        }
        SECCOMP_GET_NOTIF_SIZES => {
            // struct seccomp_notif_sizes { u16 seccomp_notif; u16 seccomp_notif_resp; u16 seccomp_data; }
            if filter_user == 0 {
                return Err(Errno::EFAULT);
            }
            let mut sizes = [0u8; 8];
            sizes[0..2].copy_from_slice(&16u16.to_le_bytes());
            sizes[2..4].copy_from_slice(&16u16.to_le_bytes());
            sizes[4..6].copy_from_slice(&64u16.to_le_bytes());
            copy_to_user(filter_user, &sizes[..6]).map_err(|e| e.as_errno())?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// 取任务的 seccomp 状态（惰性创建并挂载）。
fn seccomp_state(task: &Arc<Task>) -> Arc<general::seccomp::SeccompState> {
    if let Some(state) = task
        .ext_lookup(general::syscall::TASKEXT_SECCOMP)
        .and_then(|payload| payload.downcast::<general::seccomp::SeccompState>().ok())
    {
        return state;
    }
    let state = general::seccomp::SeccompState::new();
    let erased: Arc<dyn core::any::Any + Send + Sync> = state.clone();
    task.ext_install(general::syscall::TASKEXT_SECCOMP, erased);
    state
}

pub(super) fn sys_bpf(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_execveat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const AT_FDCWD: i32 = -100;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_EMPTY_PATH: usize = 0x1000;

    let dirfd_raw = ctx.args[0];
    let path_user = ctx.args[1];
    let argv_user = ctx.args[2];
    let envp_user = ctx.args[3];
    let flags = ctx.args[4];
    if (flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH)) != 0 {
        return Err(Errno::EINVAL);
    }
    let vfs_ctx = vfs::current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = vfs::current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_cstr_from_user(path_user, EXEC_PATH_MAX).map_err(|e| e.as_errno())?;

    if path.is_empty() && dirfd_raw as i32 != AT_FDCWD {
        if (flags & AT_EMPTY_PATH) == 0 {
            return Err(Errno::ENOENT);
        }
        let fd_raw = u32::try_from(dirfd_raw).map_err(|_| Errno::EBADF)?;
        let fd = Fd::from_raw(fd_raw);
        let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
        if (flags & AT_SYMLINK_NOFOLLOW) != 0 && file.inode().kind() == vfs::stat::FileType::Symlink
        {
            return Err(Errno::ELOOP);
        }
        let request = ExecRequest::from_file_descriptor(fd_raw, argv_user, envp_user);
        sched::operation::execve_with_context(request, UserContextRef::new(ctx.tf.as_usize()))?;
        ctx.finalize_frame();
        return Ok(0);
    }

    let exec_path = if path.is_empty() {
        if (flags & AT_EMPTY_PATH) == 0 {
            return Err(Errno::ENOENT);
        }
        vfs::namespace_path(&vfs_ctx, &vfs_ctx.cwd(), &vfs_ctx.cwd_mount()).ok_or(Errno::ENOENT)?
    } else {
        let dirfd = if path.starts_with('/') || dirfd_raw as i32 == AT_FDCWD {
            vfs::path::Dirfd::Cwd
        } else {
            let file = fdt
                .get_file(Fd::from_raw(dirfd_raw as u32))
                .ok_or(Errno::EBADF)?;
            vfs::path::Dirfd::Fd(file)
        };
        let lookup_flags = if (flags & AT_SYMLINK_NOFOLLOW) != 0 {
            vfs::path::LookupFlags::NO_FOLLOW
        } else {
            vfs::path::LookupFlags::default()
        };
        let result =
            vfs::path::lookup(&vfs_ctx, &dirfd, &path, lookup_flags).map_err(|e| e.to_errno())?;
        if (flags & AT_SYMLINK_NOFOLLOW) != 0
            && result
                .dentry
                .inode()
                .is_some_and(|inode| inode.kind() == vfs::stat::FileType::Symlink)
        {
            return Err(Errno::ELOOP);
        }
        vfs::namespace_path(&vfs_ctx, &result.dentry, &result.mount).ok_or(Errno::ENOENT)?
    };

    let request = ExecRequest::from_kernel_path(exec_path, argv_user, envp_user);
    sched::operation::execve_with_context(request, UserContextRef::new(ctx.tf.as_usize()))?;
    ctx.finalize_frame();
    Ok(0)
}

pub(super) fn sys_kexec_file_load(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // kexec 内核文件加载未实现；Linux 无 CONFIG_KEXEC 时同样 ENOSYS。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_clock_gettime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_clock_gettime(ctx)
}

pub(super) fn sys_clock_settime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    clock_settime_common(ctx)
}

pub(super) fn sys_clock_adjtime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_clock_adjtime(ctx)
}

pub(super) fn sys_clock_getres_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_clock_getres(ctx)
}

pub(super) fn sys_clock_nanosleep_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_clock_nanosleep(ctx)
}

pub(super) fn sys_timer_gettime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timer_gettime_common(ctx)
}

pub(super) fn sys_timer_settime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timer_settime_common(ctx)
}

pub(super) fn sys_pidfd_open(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let flags = ctx.args[1];
    const PIDFD_NONBLOCK: usize = 0o00004000;
    if pid <= 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & !PIDFD_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let task = lookup_task_for_thread_syscall(pid, ctx.task())?;
    require_task_access(ctx.task(), &task)?;
    let group = pidfd::group_for_process_pid(pid, &task)?;
    let fdt = vfs::current_fdtable().ok_or(Errno::ENOSYS)?;
    let cred = vfs::current_vfs_context()
        .map(|ctx| ctx.cred())
        .ok_or(Errno::ENOSYS)?;
    let fd = pidfd::create(&fdt, cred, group, (flags & PIDFD_NONBLOCK) != 0)?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_landlock_create_ruleset(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Landlock LSM 整体未实现；Linux 未启用 CONFIG_SECURITY_LANDLOCK 时同样 ENOSYS。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_landlock_add_rule(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Landlock LSM 整体未实现（同上）。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_landlock_restrict_self(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Landlock LSM 整体未实现（同上）。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_process_mrelease(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Linux `process_mrelease(pidfd, flags)`：OOM killer 用它提前回收一个已退出
    // （僵尸）进程的地址空间，而无需等待父进程 reap。
    //
    // ABI：arg0 = pidfd，arg1 = flags（必须为 0）。
    let fd_raw = ctx.args[0] as isize;
    let flags = ctx.args[1];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    if fd_raw < 0 {
        return Err(Errno::EBADF);
    }
    let fdt = vfs::current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt
        .get_file(Fd::from_raw(fd_raw as u32))
        .ok_or(Errno::EBADF)?;
    let group = pidfd::group_from_file(&file).ok_or(Errno::EINVAL)?;

    // 仅对已退出/僵尸进程有效；仍存活的进程返回 EINVAL（Linux 语义）。
    if group.group_exit_status().is_none() {
        return Err(Errno::EINVAL);
    }

    // 强制回收：逐个成员移除 VmSpace 扩展槽，drop 掉其地址空间 Arc。
    // 这是 exit 路径（KernelExtExitHook::cleanup_on_exit）同款回收动作，对已
    // 回收过的任务为幂等 no-op；对尚未经 exit hook 回收的僵尸进程则立即
    // 释放其 PGD 与 resident 页引用。VmSpace 由 mm 内部引用计数管理，
    // 这里只借用 `ext_remove` 公共接口，不依赖 mm 内部字段。
    for member in group.snapshot() {
        let _ = member.ext_remove(sched::TASKEXT_VM_SPACE);
    }
    Ok(0)
}

pub(super) fn sys_lsm_get_self_attr(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // LSM 自属性查询未实现；除 capability 外无 LSM 模块。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_lsm_set_self_attr(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // LSM 自属性设置未实现；除 capability 外无 LSM 模块。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_lsm_list_modules(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Linux 至少返回 "capability"。ABI：
    //   sys_lsm_list_modules(ids, size, flags)
    // 返回填写的 id 数量；ids 为 u64 数组（LSM_ID_CAPABILITY = 100）。
    let ids_user = ctx.args[0];
    let size = ctx.args[1];
    let flags = ctx.args[2];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    const LSM_ID_CAPABILITY: u64 = 100;
    if size == 0 {
        return Ok(1);
    }
    if ids_user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut ids = [0u8; 8];
    ids.copy_from_slice(&LSM_ID_CAPABILITY.to_le_bytes());
    copy_to_user(ids_user, &ids).map_err(|e| e.as_errno())?;
    Ok(1)
}

fn futex_wait(
    task: &Arc<Task>,
    key: FutexKey,
    uaddr: usize,
    expected: u32,
    bitset: u32,
    deadline_ns: Option<u64>,
) -> Result<usize, Errno> {
    #[cfg(feature = "trace-task-lifecycle")]
    if trace_futex_task(task) {
        log::info!(
            "[syscall][futex] wait pid={:?} addr={:#x} expected={} bitset={:#x} deadline={:?}",
            task.pid_root(),
            uaddr,
            expected,
            bitset,
            deadline_ns,
        );
    }
    let me = Arc::clone(task);
    let wait_state = Arc::new(FutexWaitState::new());
    let vm = task_vm_space_for_futex(task)?;
    vm.prefault_user_u32(uaddr, false)?;
    if let Some(deadline) = deadline_ns {
        if !sched::register_sleep_deadline(task, deadline) {
            return Err(Errno::ETIMEDOUT);
        }
    }
    if let Err(err) = futex_enqueue_waiter_if_equal(
        &vm,
        key,
        uaddr,
        expected,
        FutexWaiter {
            task: Arc::downgrade(&me),
            bitset,
            waitv_index: None,
            pi_target: None,
            state: Arc::clone(&wait_state),
        },
    ) {
        if deadline_ns.is_some() {
            sched::cancel_sleep_deadline(task);
        }
        return Err(err);
    }
    loop {
        if let Some(deadline) = deadline_ns {
            if sched::now_ns_direct() >= deadline {
                futex_remove_waiter(key, &me);
                restore_current_task_after_sleep(task);
                sched::cancel_sleep_deadline(task);
                return Err(Errno::ETIMEDOUT);
            }
        }
        if sched::operation::has_interrupting_signal(task) {
            futex_remove_waiter(key, &me);
            restore_current_task_after_sleep(task);
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            return Err(Errno::EINTR);
        }
        if vm
            .read_user_u32_nofault(uaddr)
            .map_or(true, |cur| cur != expected)
        {
            #[cfg(feature = "trace-task-lifecycle")]
            if trace_futex_task(task) {
                log::info!(
                    "[syscall][futex] waiter-value-changed pid={:?} key={key:?}",
                    task.pid_root(),
                );
            }
            futex_remove_waiter(key, &me);
            restore_current_task_after_sleep(task);
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            return Err(Errno::EAGAIN);
        }
        if wait_state.is_woken() {
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            restore_current_task_after_sleep(task);
            return Ok(0);
        }
        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(sched::WaitReason::Futex, sched::now_ns_direct());
        let _ = task.cas_state(TaskState::Running, TaskState::Sleeping);
        if !wait_state.mark_sleeping() {
            if wait_state.is_woken() {
                if deadline_ns.is_some() {
                    sched::cancel_sleep_deadline(task);
                }
                restore_current_task_after_sleep(task);
                return Ok(0);
            }
            restore_current_task_after_sleep(task);
            continue;
        }
        // 关闭 futex lost-wakeup 窗口：
        // wake 可能发生在用户值二次检查之后、真正 schedule 之前。wake 会先置
        // woken；若 waiter 尚未提交睡眠，wake 不会入队，等待者必须在这里自行返回。
        if wait_state.is_woken() {
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            restore_current_task_after_sleep(task);
            return Ok(0);
        }
        sched::operation::sched_yield()?;
        let _ = wait_state.rearm_after_non_futex_wakeup();
        if wait_state.is_woken() {
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            restore_current_task_after_sleep(task);
            return Ok(0);
        }
    }
}

#[derive(Clone)]
struct FutexWaitvEntry {
    index: usize,
    uaddr: usize,
    expected: u32,
    key: FutexKey,
    wait_state: Arc<FutexWaitState>,
}

fn futex_waitv_enqueue_if_equal(
    vm: &VmSpace,
    entries: &[FutexWaitvEntry],
    task: &Arc<Task>,
) -> Result<(), Errno> {
    let mut table = FUTEX_TABLE.lock();
    for entry in entries {
        if vm.read_user_u32_nofault(entry.uaddr)? != entry.expected {
            return Err(Errno::EAGAIN);
        }
    }
    for entry in entries {
        table
            .entry(entry.key)
            .or_insert(FutexBucket {
                waiters: Vec::new(),
            })
            .waiters
            .push(FutexWaiter {
                task: Arc::downgrade(task),
                bitset: FUTEX_BITSET_MATCH_ANY,
                waitv_index: Some(entry.index),
                pi_target: None,
                state: Arc::clone(&entry.wait_state),
            });
    }
    Ok(())
}

const FUTEX2_SIZE_U32: u32 = 0x02;
const FUTEX2_SIZE_MASK: u32 = 0x03;
const FUTEX2_SUPPORTED_FLAGS: u32 = FUTEX2_SIZE_U32 | FUTEX_PRIVATE_FLAG;

fn futex2_private(flags: u32) -> Result<bool, Errno> {
    if (flags & FUTEX2_SIZE_MASK) != FUTEX2_SIZE_U32 || (flags & !FUTEX2_SUPPORTED_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok((flags & FUTEX_PRIVATE_FLAG) != 0)
}

fn read_futex_waitv_entry(user: usize, index: usize) -> Result<FutexWaitvEntry, Errno> {
    let mut raw = [0u8; 24];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let val = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    let uaddr = u64::from_le_bytes(raw[8..16].try_into().unwrap()) as usize;
    let flags = u32::from_le_bytes(raw[16..20].try_into().unwrap());
    let reserved = u32::from_le_bytes(raw[20..24].try_into().unwrap());
    if reserved != 0 || val > u32::MAX as u64 {
        return Err(Errno::EINVAL);
    }
    if uaddr == 0 {
        return Err(Errno::EFAULT);
    }
    if uaddr % 4 != 0 {
        return Err(Errno::EINVAL);
    }
    let private = futex2_private(flags)?;
    let task = sched::current_task_direct();
    Ok(FutexWaitvEntry {
        index,
        uaddr,
        expected: val as u32,
        key: futex_key(&task, uaddr, private)?,
        wait_state: Arc::new(FutexWaitState::new()),
    })
}

fn futex_waitv_woken_index(entries: &[FutexWaitvEntry]) -> Option<usize> {
    for entry in entries {
        if entry.wait_state.is_woken() {
            return Some(entry.index);
        }
    }
    None
}

fn futex_waitv_value_mismatch(entries: &[FutexWaitvEntry]) -> Result<bool, Errno> {
    for entry in entries {
        if read_user_u32(entry.uaddr)? != entry.expected {
            return Ok(true);
        }
    }
    Ok(false)
}

fn futex_waitv_remove_all(entries: &[FutexWaitvEntry], task: &Arc<Task>) {
    let mut table = FUTEX_TABLE.lock();
    for entry in entries {
        let remove_bucket = if let Some(bucket) = table.get_mut(&entry.key) {
            bucket.waiters.retain(|waiter| {
                let same_task = waiter
                    .task
                    .upgrade()
                    .as_ref()
                    .is_some_and(|waiter_task| Arc::ptr_eq(waiter_task, task));
                !(same_task && waiter.waitv_index == Some(entry.index))
            });
            bucket.waiters.is_empty()
        } else {
            false
        };
        if remove_bucket {
            table.remove(&entry.key);
        }
    }
}

fn futex2_abs_deadline(timeout_user: usize, clockid: usize) -> Result<Option<u64>, Errno> {
    if timeout_user == 0 {
        return Ok(None);
    }
    if clockid != crate::vdso::CLOCK_MONOTONIC && clockid != crate::vdso::CLOCK_REALTIME {
        return Err(Errno::EINVAL);
    }
    let abs_ns = read_timespec_ns(timeout_user)?;
    let sched_now = sched::now_ns_direct();
    let clock_now = crate::vdso::clock_time_ns(clockid).unwrap_or(sched_now);
    Ok(Some(if abs_ns <= clock_now {
        sched_now
    } else {
        sched_now.saturating_add(abs_ns - clock_now)
    }))
}

fn futex_wait_deadline(futex_op: u32, cmd: u32, timeout_user: usize) -> Result<Option<u64>, Errno> {
    if timeout_user == 0 {
        return Ok(None);
    }
    let timeout_ns = read_timespec_ns(timeout_user)?;
    let sched_now = sched::now_ns_direct();
    if cmd == FUTEX_WAIT {
        return Ok(Some(sched_now.saturating_add(timeout_ns)));
    }
    let clock_id = match cmd {
        // Linux 的旧 PI ABI 固定把 timeout 解释为绝对 CLOCK_REALTIME；
        // LOCK_PI2 才允许在 CLOCK_MONOTONIC 与 CLOCK_REALTIME 之间选择。
        FUTEX_LOCK_PI | FUTEX_WAIT_REQUEUE_PI => crate::vdso::CLOCK_REALTIME,
        _ if (futex_op & FUTEX_CLOCK_REALTIME) != 0 => crate::vdso::CLOCK_REALTIME,
        _ => crate::vdso::CLOCK_MONOTONIC,
    };
    let clock_now = crate::vdso::clock_time_ns(clock_id).unwrap_or(sched_now);
    Ok(Some(if timeout_ns <= clock_now {
        sched_now
    } else {
        sched_now.saturating_add(timeout_ns - clock_now)
    }))
}

fn read_timespec_ns(user: usize) -> Result<u64, Errno> {
    let mut raw = [0u8; 16];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok((sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u64))
}

fn write_timespec_ns(user: usize, ns: u64) -> Result<(), Errno> {
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&((ns / 1_000_000_000).min(i64::MAX as u64) as i64).to_le_bytes());
    raw[8..16].copy_from_slice(&((ns % 1_000_000_000) as i64).to_le_bytes());
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn lookup_task_for_thread_syscall(pid: i32, current: &Arc<Task>) -> Result<Arc<Task>, Errno> {
    if pid == 0 {
        return Ok(Arc::clone(current));
    }
    // TODO(threading): Linux applies ptrace permission checks here. The current
    // credential model lacks a common ptrace access helper, so we only validate
    // that the TID exists.
    sched::root_pid_ns()
        .registry()
        .lookup(pid)
        .and_then(|weak| weak.upgrade())
        .ok_or(Errno::ESRCH)
}

fn require_task_access(current: &Arc<Task>, target: &Arc<Task>) -> Result<(), Errno> {
    if Arc::ptr_eq(current, target) {
        return Ok(());
    }
    let current_creds = current.credentials();
    if current_creds.has_cap(Capability::SysAdmin) {
        return Ok(());
    }
    let target_creds = target.credentials();
    if current_creds.euid == target_creds.uid
        || current_creds.euid == target_creds.euid
        || current_creds.uid == target_creds.uid
        || current_creds.uid == target_creds.euid
    {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

fn task_fdtable(task: &Arc<Task>) -> Option<Arc<vfs::FdTable>> {
    task.ext_lookup(sched::TASKEXT_VFS_FDTABLE)?
        .downcast::<vfs::FdTable>()
        .ok()
}

fn sync_thread_group_fdtable_nofile_limit(
    task: &Arc<Task>,
    limit: sched::RlimitPair,
) -> Result<(), Errno> {
    let raw = limit.soft.raw();
    let soft = u32::try_from(raw).map_err(|_| Errno::EINVAL)?;
    let hard = u32::try_from(limit.hard.raw()).map_err(|_| Errno::EINVAL)?;
    let mut synced = false;
    if let Some(fdt) = task_fdtable(task) {
        fdt.set_limits(soft, hard).map_err(|e| e.to_errno())?;
        synced = true;
    }
    for member in task.thread_group().snapshot() {
        if let Some(fdt) = task_fdtable(&member) {
            fdt.set_limits(soft, hard).map_err(|e| e.to_errno())?;
            synced = true;
        }
    }
    if synced { Ok(()) } else { Err(Errno::EBADF) }
}

fn task_vfs_context(task: &Arc<Task>) -> Option<Arc<vfs::VfsContext>> {
    task.ext_lookup(sched::TASKEXT_VFS_CONTEXT)?
        .downcast::<vfs::VfsContext>()
        .ok()
}

fn task_vm_space(task: &Arc<Task>) -> Option<Arc<VmSpace>> {
    task.ext_lookup(sched::TASKEXT_VM_SPACE)?
        .downcast::<VmSpace>()
        .ok()
}

pub(crate) fn cleanup_task_before_exit(task: &Arc<Task>) {
    if task.is_kernel_task() {
        return;
    }
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!("[syscall][exit-cleanup] begin pid={:?}", task.pid_root(),);
    let _ = sched::cancel_sleep_deadline(task);
    release_exit_files(task);
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][exit-cleanup] futex-begin pid={:?}",
        task.pid_root(),
    );
    let current = sched::current_task_direct();
    if Arc::ptr_eq(&current, task) {
        cleanup_task_before_exit_in_active_vm(task);
        return;
    }
    let target_vm = task_vm_space(task);
    let current_vm = task_vm_space(&current);
    let switch_vm = match (&target_vm, &current_vm) {
        (Some(target), Some(current)) => !Arc::ptr_eq(target, current),
        (Some(_), None) => true,
        _ => false,
    };
    if switch_vm {
        let saved = current.ext_remove(sched::TASKEXT_VM_SPACE);
        if let Some(vm) = target_vm.as_ref() {
            current.ext_install(sched::TASKEXT_VM_SPACE, vm.clone());
            vm.activate();
        }
        cleanup_task_before_exit_in_active_vm(task);
        current.ext_remove(sched::TASKEXT_VM_SPACE);
        if let Some(saved) = saved {
            current.ext_install(sched::TASKEXT_VM_SPACE, saved);
        }
        if let Some(vm) = current_vm.as_ref() {
            vm.activate();
        }
    } else {
        cleanup_task_before_exit_in_active_vm(task);
    }
}

fn cleanup_task_before_exit_in_active_vm(task: &Arc<Task>) {
    let _ = futex_remove_task_waiters(task);
    pi_remove_task_waiters(task);
    pi_release_owned_futexes(task);
    exit_robust_list(task);
    clear_child_tid_and_wake(task);
    super::ipc::apply_sem_undo_on_exit(task);
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[syscall][exit-cleanup] futex-done pid={:?}",
        task.pid_root(),
    );
}

/// 在 exec 的旧地址空间仍激活时完成不可回退的用户 ABI 清理。
pub(crate) fn cleanup_task_for_exec(task: &Arc<Task>, scratch: &mut ExecCleanupScratch) {
    let _ = futex_remove_task_waiters_for_exec(task);
    pi_remove_task_waiters_for_exec(task);
    pi_release_owned_futexes_for_exec(task, scratch);
    exit_robust_list_with_scratch(
        task,
        &mut scratch.robust_visited,
        true,
        &mut scratch.pi_handoffs,
        &mut scratch.pi_handoff_overflow,
    );
    clear_child_tid_and_wake_with_mode(task, true);
}

fn kcmp_arc<T>(left: &Arc<T>, right: &Arc<T>) -> usize {
    if Arc::ptr_eq(left, right) {
        return 0;
    }
    if (Arc::as_ptr(left) as usize) < (Arc::as_ptr(right) as usize) {
        1
    } else {
        2
    }
}

fn kcmp_fd_arg(raw: usize) -> Result<Fd, Errno> {
    if raw > u32::MAX as usize {
        return Err(Errno::EBADF);
    }
    Ok(Fd::from_raw(raw as u32))
}

fn exit_robust_list(task: &Arc<Task>) {
    let mut visited = Vec::new();
    if visited.try_reserve_exact(ROBUST_LIST_LIMIT).is_err() {
        let _ = task.take_robust_list();
        return;
    }
    let mut pi_handoffs = Vec::new();
    let mut overflow = false;
    exit_robust_list_with_scratch(task, &mut visited, false, &mut pi_handoffs, &mut overflow);
}

fn exit_robust_list_with_scratch(
    task: &Arc<Task>,
    visited: &mut Vec<usize>,
    deferred_wake: bool,
    pi_handoffs: &mut Vec<ExecPiHandoff>,
    pi_handoff_overflow: &mut bool,
) {
    visited.clear();
    let robust = task.take_robust_list();
    let Some(vm) = task_vm_space(task) else {
        return;
    };
    if robust.head == 0 || robust.len != ROBUST_LIST_HEAD_SIZE {
        return;
    }
    if !robust_node_aligned(robust.head)
        || !vm.is_user_range_readable(robust.head, ROBUST_LIST_HEAD_SIZE)
    {
        return;
    }
    let tid = task.pid_root().unwrap_or(0) as u32;
    let Ok(futex_offset) = read_robust_isize(&vm, robust.head + 8) else {
        return;
    };
    let pending = read_robust_usize(&vm, robust.head + 16).unwrap_or(0);
    let mut next = read_robust_usize(&vm, robust.head).unwrap_or(0);
    let mut walked = 0usize;
    while next != 0 && next != robust.head && walked < ROBUST_LIST_LIMIT {
        if !robust_node_aligned(next) {
            log::debug!(
                "[syscall][robust] pid={:?} ignored unaligned robust node {:#x}",
                task.pid_root(),
                next,
            );
            break;
        }
        if visited.contains(&next) {
            log::warning!(
                "[syscall][robust] pid={:?} robust list cycle at {:#x}",
                task.pid_root(),
                next,
            );
            break;
        }
        visited.push(next);
        let Ok(next_link) = read_robust_usize(&vm, next) else {
            log::debug!(
                "[syscall][robust] pid={:?} stopped at unreadable robust node {:#x}",
                task.pid_root(),
                next,
            );
            break;
        };
        handle_robust_node(
            task,
            &vm,
            next,
            futex_offset,
            tid,
            deferred_wake,
            pi_handoffs,
            pi_handoff_overflow,
        );
        next = next_link;
        walked += 1;
    }
    if walked == ROBUST_LIST_LIMIT {
        log::warning!(
            "[syscall][robust] pid={:?} robust list walk hit limit",
            task.pid_root(),
        );
    }
    if pending != 0
        && pending != robust.head
        && robust_node_aligned(pending)
        && !visited.contains(&pending)
    {
        handle_robust_node(
            task,
            &vm,
            pending,
            futex_offset,
            tid,
            deferred_wake,
            pi_handoffs,
            pi_handoff_overflow,
        );
    }
}

fn handle_robust_node(
    task: &Arc<Task>,
    vm: &Arc<VmSpace>,
    node: usize,
    futex_offset: isize,
    tid: u32,
    deferred_wake: bool,
    pi_handoffs: &mut Vec<ExecPiHandoff>,
    pi_handoff_overflow: &mut bool,
) {
    let Some(uaddr) = robust_futex_addr(node, futex_offset) else {
        return;
    };
    if uaddr % 4 != 0 {
        return;
    }
    let Ok(cur) = vm.read_user_u32_nofault(uaddr) else {
        return;
    };
    if (cur & FUTEX_TID_MASK) != tid {
        return;
    }
    {
        let mut handed_off = false;
        for private in [true, false] {
            if let Ok(key) = vm.futex_key_for(uaddr, private) {
                handed_off |= pi_owner_died_key(
                    vm,
                    key,
                    uaddr,
                    task,
                    deferred_wake,
                    pi_handoffs,
                    pi_handoff_overflow,
                );
            }
        }
        if handed_off {
            return;
        }
    }
    let new = (cur & !FUTEX_TID_MASK) | FUTEX_OWNER_DIED;
    if vm.compare_exchange_user_u32_nofault(uaddr, cur, new).ok() == Some(cur) {
        if deferred_wake {
            let _ = futex_wake_addr_one_deferred(task, uaddr);
        } else {
            let _ = futex_wake_addr(task, uaddr, 1);
        }
    }
}

fn robust_futex_addr(node: usize, futex_offset: isize) -> Option<usize> {
    let addr = (node as isize).checked_add(futex_offset)?;
    if addr < 0 { None } else { Some(addr as usize) }
}

fn robust_node_aligned(node: usize) -> bool {
    node % core::mem::align_of::<usize>() == 0
}

fn read_robust_usize(vm: &VmSpace, user: usize) -> Result<usize, Errno> {
    if !vm.is_user_range_readable(user, core::mem::size_of::<usize>()) {
        return Err(Errno::EFAULT);
    }
    #[cfg(target_pointer_width = "64")]
    {
        let low = vm.read_user_u32_nofault(user)? as usize;
        let high = vm.read_user_u32_nofault(user + 4)? as usize;
        Ok(low | (high << 32))
    }
    #[cfg(target_pointer_width = "32")]
    {
        vm.read_user_u32_nofault(user).map(|value| value as usize)
    }
}

fn read_robust_isize(vm: &VmSpace, user: usize) -> Result<isize, Errno> {
    read_robust_usize(vm, user).map(|value| value as isize)
}

fn read_user_u32(user: usize) -> Result<u32, Errno> {
    let mut raw = [0u8; 4];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(u32::from_ne_bytes(raw))
}

fn write_user_u32(user: usize, value: u32) -> Result<(), Errno> {
    copy_to_user(user, &value.to_ne_bytes()).map_err(|e| e.as_errno())
}

const ITIMER_REAL: usize = 0;
const ITIMER_VIRTUAL: usize = 1;
const ITIMER_PROF: usize = 2;
const ITIMERVAL_SIZE: usize = 32;
const TIMEVAL_SIZE: usize = 16;
const USEC_PER_SEC: i64 = 1_000_000;

fn read_itimerval(user: usize) -> Result<sched::RealtimeItimerSpec, Errno> {
    let mut raw = [0u8; ITIMERVAL_SIZE];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(sched::RealtimeItimerSpec {
        interval_ns: timeval_to_ns(&raw[0..TIMEVAL_SIZE])?,
        value_ns: timeval_to_ns(&raw[TIMEVAL_SIZE..ITIMERVAL_SIZE])?,
    })
}

fn write_itimerval(user: usize, spec: sched::RealtimeItimerSpec) -> Result<(), Errno> {
    let mut raw = [0u8; ITIMERVAL_SIZE];
    write_timeval_ns(&mut raw[0..TIMEVAL_SIZE], spec.interval_ns);
    write_timeval_ns(&mut raw[TIMEVAL_SIZE..ITIMERVAL_SIZE], spec.value_ns);
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn timeval_to_ns(raw: &[u8]) -> Result<u64, Errno> {
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let usec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || !(0..USEC_PER_SEC).contains(&usec) {
        return Err(Errno::EINVAL);
    }
    Ok((sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add((usec as u64).saturating_mul(1_000)))
}

fn write_timeval_ns(out: &mut [u8], ns: u64) {
    let sec = (ns / 1_000_000_000).min(i64::MAX as u64) as i64;
    let usec = ((ns % 1_000_000_000) / 1_000) as i64;
    out[0..8].copy_from_slice(&sec.to_le_bytes());
    out[8..16].copy_from_slice(&usec.to_le_bytes());
}

fn require_cap(task: &Arc<Task>, cap: Capability) -> Result<(), Errno> {
    if task.credentials().has_cap(cap) {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

fn clock_settime_common(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clock_id = ctx.args[0];
    let tp = ctx.args[1];
    if clock_id != crate::vdso::CLOCK_REALTIME {
        return Err(Errno::EINVAL);
    }
    if tp == 0 {
        return Err(Errno::EFAULT);
    }
    require_cap(ctx.task(), Capability::SysTime)?;
    let new_ns = read_timespec_ns(tp)?;
    let old_offset = crate::vdso::realtime_offset_ns();
    crate::vdso::set_realtime_ns(new_ns);
    // 实时钟被设置：取消登记了 TFD_TIMER_CANCEL_ON_SET 的 timerfd。
    if crate::vdso::realtime_offset_ns() != old_offset {
        vfs::timerfd::cancel_timers_on_clock_set();
    }
    Ok(0)
}

fn set_uts_field(
    ctx: &mut SyscallContext<'_>,
    field: &Spinlock<[u8; UTS_FIELD_LEN]>,
) -> Result<usize, Errno> {
    let user = ctx.args[0];
    let len = ctx.args[1];
    if len > UTS_NAME_MAX {
        return Err(Errno::EINVAL);
    }
    require_cap(ctx.task(), Capability::SysAdmin)?;
    let mut value = [0u8; UTS_FIELD_LEN];
    if len != 0 {
        copy_from_user(user, &mut value[..len]).map_err(|e| e.as_errno())?;
    }
    value[UTS_NAME_MAX] = 1;
    *field.lock() = value;
    Ok(0)
}

fn write_uts_field(out: &mut [u8], index: usize, value: &[u8]) {
    let start = index * 65;
    let n = value.len().min(64);
    out[start..start + n].copy_from_slice(&value[..n]);
}

fn write_uts_dynamic_field(
    out: &mut [u8],
    index: usize,
    field: &Spinlock<[u8; UTS_FIELD_LEN]>,
    default: &[u8],
) {
    let guard = field.lock();
    if guard[UTS_NAME_MAX] == 0 {
        write_uts_field(out, index, default);
    } else {
        write_uts_field(out, index, &guard[..]);
    }
}

fn signal_arg(raw: usize) -> Result<Option<SignalNumber>, Errno> {
    if raw == 0 {
        Ok(None)
    } else {
        SignalNumber::from_raw(raw as i32)
            .map(Some)
            .ok_or(Errno::EINVAL)
    }
}

fn ptrace_signal_arg(raw: usize) -> Result<Option<SignalNumber>, Errno> {
    if raw == 0 {
        Ok(None)
    } else {
        SignalNumber::from_raw(raw as i32)
            .map(Some)
            .ok_or(Errno::EIO)
    }
}

fn waitid_status(status: WaitStatus) -> i32 {
    if status.wifexited() {
        status.wexitstatus()
    } else if status.wifsignaled() {
        status.wtermsig()
    } else if status.wifstopped() {
        status.wstopsig()
    } else if status.wifcontinued() {
        SignalNumber::SIGCONT.raw() as i32
    } else {
        status.raw()
    }
}

fn waitid_code(status: WaitStatus) -> i32 {
    if status.wifexited() {
        1 // CLD_EXITED
    } else if status.wifsignaled() {
        if status.wcoredump() {
            3 // CLD_DUMPED
        } else {
            2 // CLD_KILLED
        }
    } else if status.wifstopped() {
        5 // CLD_STOPPED
    } else if status.wifcontinued() {
        6 // CLD_CONTINUED
    } else {
        0
    }
}

fn write_i64(out: &mut [u8], off: usize, value: i64) {
    out[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(out: &mut [u8], off: usize, value: i32) {
    out[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], off: usize, value: u32) {
    out[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], off: usize, value: u64) {
    out[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_u16(out: &mut [u8], off: usize, v: u16) {
    out[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut [u8], off: usize, v: u32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut [u8], off: usize, v: u64) {
    out[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_i64(out: &mut [u8], off: usize, v: i64) {
    out[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[cfg(all(feature = "kernel-tests", target_arch = "riscv64"))]
mod riscv_flush_icache_tests {
    use errno::Errno;
    use ktest::ktest;

    use super::validate_riscv_flush_icache_flags;

    #[ktest]
    fn riscv_flush_icache_accepts_linux_flags() {
        assert!(validate_riscv_flush_icache_flags(0).is_ok());
        assert!(validate_riscv_flush_icache_flags(1).is_ok());
    }

    #[ktest]
    fn riscv_flush_icache_rejects_unknown_flag_bits() {
        assert!(matches!(
            validate_riscv_flush_icache_flags(2),
            Err(Errno::EINVAL)
        ));
        assert!(matches!(
            validate_riscv_flush_icache_flags(3),
            Err(Errno::EINVAL)
        ));
        assert!(matches!(
            validate_riscv_flush_icache_flags(usize::MAX),
            Err(Errno::EINVAL)
        ));
    }
}

#[cfg(feature = "kernel-tests")]
mod sysinfo_tests {
    use ktest::ktest;

    use super::encode_sysinfo;

    fn read_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn read_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    #[ktest]
    fn sysinfo_uses_linux_64bit_layout() {
        let bytes = encode_sysinfo(7, 0x1122_3344_5566_7788, 0x8877_6655_4433_2211);

        assert_eq!(read_u64(&bytes, 0), 7);
        assert_eq!(read_u64(&bytes, 32), 0x1122_3344_5566_7788);
        assert_eq!(read_u64(&bytes, 40), 0x8877_6655_4433_2211);
        assert_eq!(read_u64(&bytes, 48), 0);
        assert_eq!(read_u64(&bytes, 56), 0);
        assert_eq!(read_u64(&bytes, 64), 0);
        assert_eq!(read_u64(&bytes, 72), 0);
        assert_eq!(read_u16(&bytes, 80), 0);
        assert_eq!(read_u64(&bytes, 88), 0);
        assert_eq!(read_u64(&bytes, 96), 0);
        assert_eq!(read_u32(&bytes, 104), 1);
    }
}
