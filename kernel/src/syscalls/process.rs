//! 进程与 libc 初始化相关 syscall。

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use errno::Errno;
use general::firmware::power;
use general::mm::{VmFutexKey, VmSpace, copy_cstr_from_user, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::pidfd;
use general::vfs::{self, fdtable::Fd};
use sched::clone_flags::{CloneArgs, CloneFlags};
use sched::ids::{CapSet, Capability, Credentials, Gid, Uid};
use sched::process_ops::{ExecRequest, UserContextRef};
use sched::sync::Spinlock;
use sched::task::{RseqRegistration, Task, TaskState};
use sched::{SchedAttr, SchedPolicy, SignalNumber, WaitId, WaitOptions, WaitStatus};

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

static UTS_HOSTNAME: Spinlock<[u8; UTS_FIELD_LEN]> = Spinlock::new([0u8; UTS_FIELD_LEN]);
static UTS_DOMAINNAME: Spinlock<[u8; UTS_FIELD_LEN]> = Spinlock::new([0u8; UTS_FIELD_LEN]);

pub(super) fn sys_getpid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::getpid() as usize)
}

pub(super) fn sys_gettid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::gettid() as usize)
}

pub(super) fn sys_getppid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::getppid() as usize)
}

pub(super) fn sys_getuid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::getuid().0 as usize)
}

pub(super) fn sys_geteuid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::geteuid().0 as usize)
}

pub(super) fn sys_getgid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::getgid().0 as usize)
}

pub(super) fn sys_getegid(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(sched::operation::getegid().0 as usize)
}

pub(super) fn sys_exit(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let code = ctx.args[0] as i32;
    let task = Arc::clone(ctx.task());
    log::debug!("[syscall][exit] pid={:?} code={}", task.pid_root(), code);
    release_exit_files(&task);
    ctx.release_task_ref();
    drop(task);
    sched::operation::exit(code);
}

pub(super) fn sys_exit_group(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let code = ctx.args[0] as i32;
    let task = Arc::clone(ctx.task());
    log::debug!(
        "[syscall][exit_group] pid={:?} code={}",
        task.pid_root(),
        code
    );
    for member in task.thread_group().snapshot() {
        release_exit_files(&member);
    }
    ctx.release_task_ref();
    drop(task);
    sched::operation::exit_group(code);
}

fn release_exit_files(task: &Arc<Task>) {
    // A zombie must not keep pipe/socket/file endpoints alive until its parent
    // reaps it.  Shell pipelines rely on writer fd release at process exit to
    // deliver EOF to readers such as wc or command substitution.
    let _ = task.ext_remove(sched::TASKEXT_VFS_FDTABLE);
}

pub(super) fn sys_clone(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let regs = hal::user::decode_clone_register_args(ctx.args);
    let flags = CloneFlags::from_raw(regs.flags);
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
        exit_signal: regs.flags & 0xff,
        set_tid: 0,
        set_tid_size: 0,
        requested_pid: 0,
        cgroup: 0,
    };
    if flags.has(CloneFlags::CLONE_PIDFD) && flags.has(CloneFlags::CLONE_PARENT_SETTID) {
        return Err(Errno::EINVAL);
    }
    let outcome =
        sched::operation::clone_with_context_outcome(args, UserContextRef::new(ctx.tf.as_usize()))?;
    install_clone_pidfd(args, Arc::clone(&outcome.child))?;
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
    prepare_clone3_set_tid(&mut args, ctx.task())?;
    if args.cgroup != 0 {
        // TODO(threading): cgroup 需要成员状态与迁移事务；当前阶段不把 fd 当作占位接受。
        return Err(Errno::EOPNOTSUPP);
    }
    let outcome =
        sched::operation::clone_with_context_outcome(args, UserContextRef::new(ctx.tf.as_usize()))?;
    install_clone_pidfd(args, Arc::clone(&outcome.child))?;
    Ok(outcome.pid as usize)
}

fn install_clone_pidfd(args: CloneArgs, child: Arc<Task>) -> Result<(), Errno> {
    if !args.flags.has(CloneFlags::CLONE_PIDFD) {
        return Ok(());
    }
    let fdt = vfs::current_fdtable().ok_or(Errno::ENOSYS)?;
    let cred = vfs::current_vfs_context()
        .map(|ctx| Arc::clone(&ctx.cred))
        .ok_or(Errno::ENOSYS)?;
    let fd = pidfd::create(&fdt, cred, child, false)?;
    if let Err(err) = copy_to_user(args.pidfd, &(fd.as_raw() as i32).to_le_bytes()) {
        let _ = fdt.close_fd(fd);
        return Err(err.as_errno());
    }
    Ok(())
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
            power::reboot().map_err(map_power_error)?;
            halt_after_power_request()
        }
        LINUX_REBOOT_CMD_HALT => {
            log::emergency!("[syscall][reboot] halt requested");
            power::shutdown().map_err(map_power_error)?;
            halt_after_power_request()
        }
        LINUX_REBOOT_CMD_POWER_OFF => {
            log::emergency!("[syscall][reboot] poweroff requested");
            power::shutdown().map_err(map_power_error)?;
            halt_after_power_request()
        }
        LINUX_REBOOT_CMD_SW_SUSPEND | LINUX_REBOOT_CMD_KEXEC => Err(Errno::EOPNOTSUPP),
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
    let pid = ctx.args[0] as i32;
    let status_user = ctx.args[1];
    let options = WaitOptions::from_raw(ctx.args[2] as u32);
    let rusage_user = ctx.args[3];

    let result = sched::operation::wait4(pid, options)?;
    if status_user != 0 {
        copy_to_user(status_user, &result.status.raw().to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    if rusage_user != 0 {
        write_rusage(rusage_user, result.usage)?;
    }
    Ok(result.pid as usize)
}

pub(super) fn sys_waitid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
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
            let task = pidfd::task_from_file(&file).ok_or(Errno::EINVAL)?;
            if nonblock_pidfd && !options.has(WaitOptions::WNOHANG) {
                let probe_options = WaitOptions::from_raw(options.raw() | WaitOptions::WNOHANG);
                let probe =
                    sched::operation::waitid(WaitId::Pidfd(Arc::clone(&task)), probe_options)?;
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
                    write_rusage(rusage, probe.usage)?;
                }
                return Ok(0);
            }
            WaitId::Pidfd(task)
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
        write_rusage(rusage, result.usage)?;
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
    sched::operation::kill(pid, sig)?;
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
    const RSEQ_MIN_SIZE: usize = 32;
    const RSEQ_FLAG_UNREGISTER: usize = 1;
    let ptr = ctx.args[0];
    let len = ctx.args[1];
    let flags = ctx.args[2];
    let signature = ctx.args[3] as u32;

    if (flags & !RSEQ_FLAG_UNREGISTER) != 0 {
        return Err(Errno::EINVAL);
    }
    if ptr == 0 || len < RSEQ_MIN_SIZE || len > u32::MAX as usize {
        return Err(Errno::EINVAL);
    }

    let current = ctx.task().rseq_registration();
    if (flags & RSEQ_FLAG_UNREGISTER) != 0 {
        if !current.registered {
            return Err(Errno::EINVAL);
        }
        if current.ptr != ptr || current.len as usize != len || current.signature != signature {
            return Err(Errno::EINVAL);
        }
        ctx.task().clear_rseq_registration();
        return Ok(0);
    }

    if current.registered {
        return Err(Errno::EBUSY);
    }

    // 注册成功前先确认用户区可写，并把当前 CPU 写入 rseq 的两个 CPU 字段。
    // 后续迁移/切换由 kernel::sched 的 TaskCpuStateOps hook 继续刷新。
    let cpu = sched::current_cpu_id() as u32;
    write_rseq_cpu_fields(ptr, cpu)?;
    ctx.task().set_rseq_registration(RseqRegistration {
        ptr,
        len: len as u32,
        signature,
        registered: true,
    });
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

pub(super) fn sys_membarrier(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const MEMBARRIER_CMD_QUERY: usize = 0;
    const MEMBARRIER_CMD_GLOBAL: usize = 1 << 0;
    const MEMBARRIER_CMD_GLOBAL_EXPEDITED: usize = 1 << 1;
    const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: usize = 1 << 2;
    const MEMBARRIER_CMD_PRIVATE_EXPEDITED: usize = 1 << 3;
    const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: usize = 1 << 4;
    let single_cpu = sched::online_cpu_mask().count_ones() <= 1;
    let supported = if single_cpu {
        MEMBARRIER_CMD_GLOBAL
            | MEMBARRIER_CMD_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_PRIVATE_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
    } else {
        // TODO(smp): 多核 membarrier 需要 IPI rendezvous，确保目标 CPU 在
        // 返回前经过调度点或显式内存屏障；单 CPU fence 不能扩展成 SMP 语义。
        0
    };

    let cmd = ctx.args[0];
    let flags = ctx.args[1];
    if cmd != MEMBARRIER_CMD_QUERY && flags != 0 {
        return Err(Errno::EINVAL);
    }
    match cmd {
        MEMBARRIER_CMD_QUERY => Ok(supported),
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => {
            if (supported & cmd) != 0 {
                Ok(0)
            } else {
                Err(Errno::EOPNOTSUPP)
            }
        }
        MEMBARRIER_CMD_GLOBAL
        | MEMBARRIER_CMD_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED => {
            if (supported & cmd) == 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            // 当前内核只启动单 CPU；完整 SMP IPI rendezvous 接入前，SeqCst fence
            // 已足以满足本 CPU 上的 membarrier 可见性语义。
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
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
    Ok(0)
}

pub(super) fn sys_getrandom(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let buf_user = ctx.args[0];
    let size = ctx.args[1];
    let flags = ctx.args[2];
    const GRND_NONBLOCK: usize = 1;
    const GRND_RANDOM: usize = 2;
    if (flags & !(GRND_NONBLOCK | GRND_RANDOM)) != 0 {
        return Err(Errno::EINVAL);
    }
    if size > 256 {
        return Err(Errno::EINVAL);
    }
    let mut data = alloc::vec![0u8; size];
    prng_fill(&mut data);
    copy_to_user(buf_user, &data).map_err(|e| e.as_errno())?;
    Ok(size)
}

pub(super) fn sys_clock_gettime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clock_id = ctx.args[0];
    let tp = ctx.args[1];
    let ns = crate::vdso::clock_time_ns(clock_id).ok_or(Errno::EINVAL)?;
    let sec = (ns / 1_000_000_000) as i64;
    let nsec = (ns % 1_000_000_000) as i64;
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&sec.to_le_bytes());
    out[8..16].copy_from_slice(&nsec.to_le_bytes());
    copy_to_user(tp, &out).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_uname(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let mut out = [0u8; 65 * 6];
    write_uts_field(&mut out, 0, b"MyGo");
    write_uts_dynamic_field(&mut out, 1, &UTS_HOSTNAME, b"mygo");
    write_uts_field(&mut out, 2, b"0.1.0");
    write_uts_field(&mut out, 3, b"MyGo kernel");
    write_uts_field(&mut out, 4, hal::platform::arch_name().as_bytes());
    write_uts_dynamic_field(&mut out, 5, &UTS_DOMAINNAME, b"localdomain");
    copy_to_user(ctx.args[0], &out).map_err(|e| e.as_errno())?;
    Ok(0)
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
    let req_user = ctx.args[0];
    let rem_user = ctx.args[1];
    if req_user == 0 {
        return Err(Errno::EINVAL);
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
    let deadline = sched::now_ns_public().saturating_add(ns_total as u64);
    match sleep_until_deadline(ctx.task(), deadline, || Ok(sched::now_ns_public())) {
        Ok(()) => Ok(0),
        Err(Errno::EINTR) => {
            write_remaining_timespec(rem_user, deadline.saturating_sub(sched::now_ns_public()));
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
    if which != ITIMER_REAL {
        return Err(Errno::EINVAL);
    }
    let spec = sched::get_realtime_itimer(ctx.task());
    write_itimerval(curr_value, spec)?;
    Ok(0)
}

pub(super) fn sys_setitimer(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let which = ctx.args[0];
    let new_value = ctx.args[1];
    let old_value = ctx.args[2];
    if which != ITIMER_REAL {
        return Err(Errno::EINVAL);
    }

    let new_spec = if new_value == 0 {
        sched::RealtimeItimerSpec::default()
    } else {
        read_itimerval(new_value)?
    };
    let old_spec = sched::set_realtime_itimer(ctx.task(), new_spec.value_ns, new_spec.interval_ns);
    if old_value != 0 {
        write_itimerval(old_value, old_spec)?;
    }
    Ok(0)
}

pub(super) fn sys_clock_nanosleep(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clock_id = ctx.args[0] as i32;
    let flags = ctx.args[1];
    let req_user = ctx.args[2];
    let rem_user = ctx.args[3];
    if clock_id != crate::vdso::CLOCK_REALTIME as i32
        && clock_id != crate::vdso::CLOCK_MONOTONIC as i32
    {
        return Err(Errno::EINVAL);
    }
    if req_user == 0 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 16];
    copy_from_user(req_user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    const TIMER_ABSTIME: usize = 1;
    let absolute = (flags & TIMER_ABSTIME) != 0;
    let deadline = if absolute {
        sec.saturating_mul(1_000_000_000i64).saturating_add(nsec) as u64
    } else {
        let ns_total = sec.saturating_mul(1_000_000_000i64).saturating_add(nsec);
        sched::now_ns_public().saturating_add(ns_total as u64)
    };
    if !absolute && sec == 0 && nsec == 0 {
        return Ok(0);
    }
    let now_fn = || {
        if absolute {
            crate::vdso::clock_time_ns(clock_id as usize).ok_or(Errno::EINVAL)
        } else {
            Ok(sched::now_ns_public())
        }
    };
    match sleep_until_deadline(ctx.task(), deadline, now_fn) {
        Ok(()) => Ok(0),
        Err(Errno::EINTR) => {
            if !absolute {
                write_remaining_timespec(rem_user, deadline.saturating_sub(sched::now_ns_public()));
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

        if !task.cas_state(TaskState::Running, TaskState::Sleeping)
            && !task.cas_state(TaskState::Runnable, TaskState::Sleeping)
            && task.state() != TaskState::Sleeping
        {
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

        let sleep_deadline = sched::now_ns_public().saturating_add(deadline.saturating_sub(now));
        if !sched::register_sleep_deadline(task, sleep_deadline) {
            restore_current_task_after_sleep(task);
            return Ok(());
        }
        sched::schedule_once(sched::now_ns_public());
        sched::cancel_sleep_deadline(task);
        restore_current_task_after_sleep(task);
    }
}

fn restore_current_task_after_sleep(task: &Arc<Task>) {
    if !task.cas_state(TaskState::Sleeping, TaskState::Running) {
        let _ = task.cas_state(TaskState::Runnable, TaskState::Running);
    }
}

fn write_remaining_timespec(rem_user: usize, remaining_ns: u64) {
    if rem_user == 0 {
        return;
    }
    let remaining_ns = remaining_ns.min(i64::MAX as u64) as i64;
    let rem_sec = remaining_ns / 1_000_000_000;
    let rem_nsec = remaining_ns % 1_000_000_000;
    let mut rem_buf = [0u8; 16];
    rem_buf[0..8].copy_from_slice(&rem_sec.to_le_bytes());
    rem_buf[8..16].copy_from_slice(&rem_nsec.to_le_bytes());
    let _ = copy_to_user(rem_user, &rem_buf);
}

pub(super) fn sys_clock_getres(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clock_id = ctx.args[0];
    let tp = ctx.args[1];
    let res_ns = crate::vdso::clock_getres_ns(clock_id).ok_or(Errno::EINVAL)? as i64;
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
    let self_usage = ctx.task().usage_snapshot(sched::now_ns_public());
    let child_usage = ctx.task().child_usage_snapshot();
    let ticks = ns_to_clock_ticks(sched::now_ns_public());
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
    let snapshot = match who {
        RUSAGE_SELF | RUSAGE_THREAD => ctx.task().usage_snapshot(sched::now_ns_public()),
        RUSAGE_CHILDREN => ctx.task().child_usage_snapshot(),
        _ => return Err(Errno::EINVAL),
    };
    write_rusage(usage, snapshot)?;
    Ok(0)
}

const USER_HZ: u64 = 100;

fn ns_to_clock_ticks(ns: u64) -> i64 {
    (ns / (1_000_000_000 / USER_HZ)).min(i64::MAX as u64) as i64
}

fn write_rusage(user: usize, usage: sched::TaskUsage) -> Result<(), Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 144];
    write_timeval_pair(&mut raw, 0, usage.user_ns);
    write_timeval_pair(&mut raw, 16, usage.system_ns);
    put_i64(&mut raw, 64, usage.minflt.min(i64::MAX as u64) as i64);
    put_i64(&mut raw, 72, usage.majflt.min(i64::MAX as u64) as i64);
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

pub(super) fn sys_sysinfo(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let info = ctx.args[0];
    if info != 0 {
        let mut out = [0u8; 112];
        put_i64(&mut out, 0, sched::now_ns_public() as i64 / 1_000_000_000);
        put_u64(&mut out, 8, 0);
        put_u64(&mut out, 16, 0);
        put_u64(&mut out, 24, 0);
        put_u64(&mut out, 32, 256 * 1024 * 1024);
        put_u64(&mut out, 40, 128 * 1024 * 1024);
        put_u64(&mut out, 48, 256 * 1024 * 1024);
        put_u16(&mut out, 56, 0);
        put_u16(&mut out, 58, 0);
        put_u16(&mut out, 60, 0);
        put_u16(&mut out, 62, 0);
        put_u32(&mut out, 64, 1);
        put_u32(&mut out, 68, 65536);
        put_u32(&mut out, 72, 65536);
        put_u32(&mut out, 76, 0);
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
        .map(|task| task.sched.nice() as i32)
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

    let current = target.sched.nice() as i32;
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
    let old = task.sched.sched_attr();
    let mut attr = old;
    match attr.policy {
        SchedPolicy::Fair | SchedPolicy::Idle => {
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
    if matches!(policy, SchedPolicy::Fair | SchedPolicy::Idle) && priority != 0 {
        return Err(Errno::EINVAL);
    }
    let rt_priority = if matches!(policy, SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin) {
        linux_rt_priority_from_param(priority)?
    } else {
        0
    };
    let task = sched_task_from_pid(pid, ctx.task())?;
    let old = task.sched.sched_attr();
    let attr = SchedAttr {
        policy,
        nice: old.nice,
        slice_ns: old.slice_ns,
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
    let interval_ns = if attr.slice_ns == 0 {
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
    let (attr, reset_on_fork) = read_linux_sched_attr(attr_user)?;
    let task = sched_task_from_pid(pid, ctx.task())?;
    let old = task.sched.sched_attr();
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
        SchedPolicy::Fair => {
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
    let _persona = ctx.args[0];
    Ok(0)
}

pub(super) fn sys_prctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const PR_SET_NAME: usize = 15;
    const PR_GET_NAME: usize = 16;
    const PR_CAPBSET_READ: usize = 23;
    const PR_CAPBSET_DROP: usize = 24;
    match ctx.args[0] {
        PR_SET_NAME => {
            let name_user = ctx.args[1];
            if name_user == 0 {
                return Err(Errno::EFAULT);
            }
            let mut raw = [0u8; sched::TASK_COMM_LEN];
            copy_from_user(name_user, &mut raw).map_err(|e| e.as_errno())?;
            ctx.task().set_comm(&raw);
            Ok(0)
        }
        PR_GET_NAME => {
            let buf = ctx.args[1];
            if buf != 0 {
                copy_to_user(buf, &ctx.task().comm()).map_err(|e| e.as_errno())?;
            }
            Ok(0)
        }
        PR_CAPBSET_READ => {
            let cap = ctx.args[1] as u32;
            if !linux_cap_valid(cap) {
                return Err(Errno::EINVAL);
            }
            let creds = ctx.task().credentials();
            Ok(((creds.cap_bset.raw() >> cap) & 1) as usize)
        }
        PR_CAPBSET_DROP => {
            let cap = ctx.args[1] as u32;
            if !linux_cap_valid(cap) {
                return Err(Errno::EINVAL);
            }
            let current = ctx.task().credentials();
            if !current.has_cap(Capability::Setpcap) {
                return Err(Errno::EPERM);
            }
            let mut new = (*current).clone();
            new.cap_bset = CapSet::from_raw(new.cap_bset.raw() & !(1u64 << cap));
            ctx.task().set_credentials(Arc::new(new));
            Ok(0)
        }
        _ => Ok(0),
    }
}

pub(super) fn sys_capget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let hdrp = ctx.args[0];
    let datap = ctx.args[1];
    let (version, pid) = read_cap_header(hdrp)?;
    let words = cap_version_words(version)?;
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
    ctx.task().set_credentials(Arc::new(new));
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

pub(super) fn sys_setuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uid = Uid(ctx.args[0] as u32);
    let creds = ctx.task().credentials();
    if creds.euid == Uid::ROOT || creds.uid == uid || creds.euid == uid || creds.suid == uid {
        let mut new = (*creds).clone();
        new.uid = uid;
        new.euid = uid;
        new.suid = uid;
        new.fsuid = uid;
        ctx.task().set_credentials(Arc::new(new));
        Ok(0)
    } else {
        Err(Errno::EPERM)
    }
}

pub(super) fn sys_setgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let gid = Gid(ctx.args[0] as u32);
    let creds = ctx.task().credentials();
    if creds.euid == Uid::ROOT || creds.gid == gid || creds.egid == gid || creds.sgid == gid {
        let mut new = (*creds).clone();
        new.gid = gid;
        new.egid = gid;
        new.sgid = gid;
        new.fsgid = gid;
        ctx.task().set_credentials(Arc::new(new));
        Ok(0)
    } else {
        Err(Errno::EPERM)
    }
}

pub(super) fn sys_setreuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let ruid = ctx.args[0] as u32;
    let euid = ctx.args[1] as u32;
    let creds = ctx.task().credentials();
    let mut new = (*creds).clone();
    if ruid != u32::MAX {
        if creds.euid != Uid::ROOT && ruid != creds.uid.0 && ruid != creds.euid.0 {
            return Err(Errno::EPERM);
        }
        new.uid = Uid(ruid);
    }
    if euid != u32::MAX {
        if creds.euid != Uid::ROOT
            && euid != creds.uid.0
            && euid != creds.euid.0
            && euid != creds.suid.0
        {
            return Err(Errno::EPERM);
        }
        let new_euid = Uid(euid);
        new.euid = new_euid;
        new.fsuid = new_euid;
    }
    ctx.task().set_credentials(Arc::new(new));
    Ok(0)
}

pub(super) fn sys_setregid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let rgid = ctx.args[0] as u32;
    let egid = ctx.args[1] as u32;
    let creds = ctx.task().credentials();
    let mut new = (*creds).clone();
    if rgid != u32::MAX {
        if creds.euid != Uid::ROOT && rgid != creds.gid.0 && rgid != creds.egid.0 {
            return Err(Errno::EPERM);
        }
        new.gid = Gid(rgid);
    }
    if egid != u32::MAX {
        if creds.euid != Uid::ROOT
            && egid != creds.gid.0
            && egid != creds.egid.0
            && egid != creds.sgid.0
        {
            return Err(Errno::EPERM);
        }
        let new_egid = Gid(egid);
        new.egid = new_egid;
        new.fsgid = new_egid;
    }
    ctx.task().set_credentials(Arc::new(new));
    Ok(0)
}

pub(super) fn sys_setresuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let ruid = ctx.args[0] as u32;
    let euid = ctx.args[1] as u32;
    let suid = ctx.args[2] as u32;
    let creds = ctx.task().credentials();
    if creds.euid != Uid::ROOT {
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
        let new_euid = Uid(euid);
        new.euid = new_euid;
        new.fsuid = new_euid;
    }
    if suid != u32::MAX {
        new.suid = Uid(suid);
    }
    ctx.task().set_credentials(Arc::new(new));
    Ok(0)
}

pub(super) fn sys_setresgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let rgid = ctx.args[0] as u32;
    let egid = ctx.args[1] as u32;
    let sgid = ctx.args[2] as u32;
    let creds = ctx.task().credentials();
    if creds.euid != Uid::ROOT {
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
        let new_egid = Gid(egid);
        new.egid = new_egid;
        new.fsgid = new_egid;
    }
    if sgid != u32::MAX {
        new.sgid = Gid(sgid);
    }
    ctx.task().set_credentials(Arc::new(new));
    Ok(0)
}

pub(super) fn sys_setfsuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uid = Uid(ctx.args[0] as u32);
    let creds = ctx.task().credentials();
    let old = creds.fsuid;
    if creds.has_cap(Capability::Setuid)
        || uid == creds.uid
        || uid == creds.euid
        || uid == creds.suid
        || uid == creds.fsuid
    {
        let mut new = (*creds).clone();
        new.fsuid = uid;
        ctx.task().set_credentials(Arc::new(new));
    }
    Ok(old.0 as usize)
}

pub(super) fn sys_setfsgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let gid = Gid(ctx.args[0] as u32);
    let creds = ctx.task().credentials();
    let old = creds.fsgid;
    if creds.has_cap(Capability::Setgid)
        || gid == creds.gid
        || gid == creds.egid
        || gid == creds.sgid
        || gid == creds.fsgid
    {
        let mut new = (*creds).clone();
        new.fsgid = gid;
        ctx.task().set_credentials(Arc::new(new));
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
    ctx.task().set_credentials(Arc::new(new));
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

type FutexKey = VmFutexKey;

struct FutexWaiter {
    task: Weak<sched::Task>,
    bitset: u32,
    waitv_index: Option<usize>,
}

struct FutexBucket {
    waiters: Vec<FutexWaiter>,
}

static FUTEX_TABLE: Spinlock<BTreeMap<FutexKey, FutexBucket>> = Spinlock::new(BTreeMap::new());
static FUTEX_USER_OP_LOCK: Spinlock<()> = Spinlock::new(());

fn futex_cmd(futex_op: u32) -> u32 {
    futex_op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME)
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

fn futex_wake_key(key: FutexKey, count: usize, bitset: u32) -> usize {
    let waiters = {
        let mut table = FUTEX_TABLE.lock();
        let Some(bucket) = table.get_mut(&key) else {
            return 0;
        };
        let mut waiters = Vec::new();
        let mut idx = 0;
        while idx < bucket.waiters.len() && waiters.len() < count {
            if (bucket.waiters[idx].bitset & bitset) != 0 {
                let waiter = bucket.waiters.remove(idx);
                if let Some(task) = waiter.task.upgrade() {
                    waiters.push(task);
                }
            } else {
                idx += 1;
            }
        }
        if bucket.waiters.is_empty() {
            table.remove(&key);
        }
        waiters
    };
    wake_futex_waiters(waiters)
}

fn wake_futex_waiters(waiters: Vec<Arc<sched::Task>>) -> usize {
    let mut woken = 0usize;
    for waiter in waiters {
        if waiter.cas_state(TaskState::Sleeping, TaskState::Runnable) {
            sched::enqueue_task(waiter, sched::now_ns_public());
            woken += 1;
        } else if waiter.cas_state(TaskState::Running, TaskState::Runnable) {
            woken += 1;
        } else if waiter.state() == TaskState::Runnable {
            woken += 1;
        }
    }
    woken
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

fn futex_requeue_key(
    src: FutexKey,
    dst: FutexKey,
    wake_count: usize,
    requeue_count: usize,
    bitset: u32,
) -> usize {
    let (wake, requeue) = {
        let mut table = FUTEX_TABLE.lock();
        let mut wake = Vec::new();
        let mut requeue = Vec::new();
        if let Some(bucket) = table.get_mut(&src) {
            let mut idx = 0;
            while idx < bucket.waiters.len()
                && (wake.len() < wake_count || requeue.len() < requeue_count)
            {
                if (bucket.waiters[idx].bitset & bitset) == 0 {
                    idx += 1;
                    continue;
                }
                let waiter = bucket.waiters.remove(idx);
                if wake.len() < wake_count {
                    if let Some(task) = waiter.task.upgrade() {
                        wake.push(task);
                    }
                } else if requeue.len() < requeue_count {
                    if waiter.task.strong_count() != 0 {
                        requeue.push(waiter);
                    }
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
    };
    wake_futex_waiters(wake) + requeue
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
    let old = {
        let _guard = FUTEX_USER_OP_LOCK.lock();
        let old = read_user_u32(uaddr2)?;
        let new = futex_apply_wake_op(old, encoded)?;
        write_user_u32(uaddr2, new)?;
        old
    };
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
    loop {
        let expected = {
            let _guard = FUTEX_USER_OP_LOCK.lock();
            let cur = read_user_u32(uaddr)?;
            let owner = cur & FUTEX_TID_MASK;
            if owner == 0 {
                let new = (cur & FUTEX_OWNER_DIED) | tid;
                write_user_u32(uaddr, new)?;
                return Ok(0);
            }
            if owner == tid {
                return Err(Errno::EDEADLK);
            }
            if try_only {
                return Err(Errno::EAGAIN);
            }
            let waiting = cur | FUTEX_WAITERS;
            if waiting != cur {
                write_user_u32(uaddr, waiting)?;
            }
            waiting
        };
        match futex_wait(
            task,
            key,
            uaddr,
            expected,
            FUTEX_BITSET_MATCH_ANY,
            deadline_ns,
        ) {
            Ok(_) | Err(Errno::EAGAIN) => continue,
            Err(err) => return Err(err),
        }
    }
}

fn futex_unlock_pi(task: &Arc<Task>, uaddr: usize, private: bool) -> Result<usize, Errno> {
    let tid = task.pid_root().unwrap_or(0) as u32;
    if tid == 0 {
        return Err(Errno::ESRCH);
    }
    let key = futex_key(task, uaddr, private)?;
    let had_waiters = {
        let _guard = FUTEX_USER_OP_LOCK.lock();
        let cur = read_user_u32(uaddr)?;
        if (cur & FUTEX_TID_MASK) != tid {
            return Err(Errno::EPERM);
        }
        let had_waiters = (cur & FUTEX_WAITERS) != 0;
        write_user_u32(uaddr, 0)?;
        had_waiters
    };
    Ok(if had_waiters {
        futex_wake_key(key, 1, FUTEX_BITSET_MATCH_ANY)
    } else {
        0
    })
}

fn futex_wake_addr(task: &Arc<Task>, uaddr: usize, count: usize) -> usize {
    let mut woken = 0usize;
    if let Ok(key) = futex_key(task, uaddr, true) {
        woken += futex_wake_key(key, count, FUTEX_BITSET_MATCH_ANY);
    }
    if let Ok(key) = futex_key(task, uaddr, false) {
        woken += futex_wake_key(key, count, FUTEX_BITSET_MATCH_ANY);
    }
    woken
}

fn clear_child_tid_and_wake(task: &Arc<Task>) {
    let tid_addr = task.clear_child_tid();
    if tid_addr == 0 {
        return;
    }
    let zero = 0u32;
    let _ = copy_to_user(tid_addr, &zero.to_ne_bytes());
    task.set_clear_child_tid(0);
    let _ = futex_wake_addr(task, tid_addr, 1);
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
        FUTEX_WAKE => Ok(futex_wake_key(
            futex_key(ctx.task(), uaddr, private)?,
            val as usize,
            FUTEX_BITSET_MATCH_ANY,
        )),
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
            if cmd == FUTEX_CMP_REQUEUE && read_user_u32(uaddr)? != val3 {
                return Err(Errno::EAGAIN);
            }
            Ok(futex_requeue_key(
                futex_key(ctx.task(), uaddr, private)?,
                futex_key(ctx.task(), uaddr2, private)?,
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
        FUTEX_LOCK_PI | FUTEX_LOCK_PI2 => futex_lock_pi(ctx.task(), uaddr, private, false, None),
        FUTEX_TRYLOCK_PI => futex_lock_pi(ctx.task(), uaddr, private, true, None),
        FUTEX_UNLOCK_PI => futex_unlock_pi(ctx.task(), uaddr, private),
        FUTEX_WAIT_REQUEUE_PI | FUTEX_CMP_REQUEUE_PI => {
            // TODO(threading): requeue_pi 需要把普通等待者原子迁移到 PI owner
            // 队列，并在迁移期间维护 owner 继承关系；不能退化成普通 requeue。
            Err(Errno::EOPNOTSUPP)
        }
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
    let mut entries = Vec::new();
    for index in 0..nr {
        let entry = read_futex_waitv_entry(waiters + index * FUTEX_WAITV_ENTRY_SIZE, index)?;
        if read_user_u32(entry.uaddr)? != entry.expected {
            return Err(Errno::EAGAIN);
        }
        entries.push(entry);
    }
    if let Some(deadline) = deadline
        && sched::now_ns_public() >= deadline
    {
        return Err(Errno::ETIMEDOUT);
    }
    {
        let mut table = FUTEX_TABLE.lock();
        for entry in entries.iter() {
            table
                .entry(entry.key)
                .or_insert(FutexBucket {
                    waiters: Vec::new(),
                })
                .waiters
                .push(FutexWaiter {
                    task: Arc::downgrade(ctx.task()),
                    bitset: FUTEX_BITSET_MATCH_ANY,
                    waitv_index: Some(entry.index),
                });
        }
    }

    loop {
        if sched::operation::has_interrupting_signal(ctx.task()) {
            futex_waitv_remove_all(&entries, ctx.task());
            restore_current_task_after_sleep(ctx.task());
            if deadline.is_some() {
                sched::cancel_sleep_deadline(ctx.task());
            }
            return Err(Errno::EINTR);
        }
        if let Some(index) = futex_waitv_woken_index(&entries, ctx.task()) {
            futex_waitv_remove_all(&entries, ctx.task());
            restore_current_task_after_sleep(ctx.task());
            if deadline.is_some() {
                sched::cancel_sleep_deadline(ctx.task());
            }
            return Ok(index);
        }
        if let Some(deadline) = deadline {
            if sched::now_ns_public() >= deadline {
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
        let _ = ctx
            .task()
            .cas_state(TaskState::Running, TaskState::Sleeping);
        sched::operation::sched_yield()?;
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
    if read_user_u32(src.uaddr)? != src.expected {
        return Err(Errno::EAGAIN);
    }
    Ok(futex_requeue_key(
        src.key,
        dst.key,
        nr_wake as usize,
        nr_requeue as usize,
        FUTEX_BITSET_MATCH_ANY,
    ))
}

pub(super) fn sys_unshare(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_kexec_load(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_init_module(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_delete_module(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timer_create(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timer_gettime(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timer_getoverrun(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timer_settime(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timer_delete(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_clock_settime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    clock_settime_common(ctx)
}

pub(super) fn sys_ptrace(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
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
    set_uts_field(ctx, &UTS_HOSTNAME)
}

pub(super) fn sys_setdomainname(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    set_uts_field(ctx, &UTS_DOMAINNAME)
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
    crate::vdso::set_realtime_ns(timeval_to_ns(&raw)?);
    Ok(0)
}

pub(super) fn sys_adjtimex(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_perf_event_open(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_clock_adjtime(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_setns(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_kcmp(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const KCMP_FILE: usize = 0;
    const KCMP_VM: usize = 1;
    const KCMP_FILES: usize = 2;
    const KCMP_FS: usize = 3;
    const KCMP_SIGHAND: usize = 4;

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
        _ => Err(Errno::EOPNOTSUPP),
    }
}

pub(super) fn sys_finit_module(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_seccomp(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
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

    let exec_path = if path.is_empty() {
        if (flags & AT_EMPTY_PATH) == 0 {
            return Err(Errno::ENOENT);
        }
        if dirfd_raw as i32 == AT_FDCWD {
            vfs::namespace_path(&vfs_ctx, &vfs_ctx.cwd(), &vfs_ctx.cwd_mount())
                .ok_or(Errno::ENOENT)?
        } else {
            let fd = Fd::from_raw(dirfd_raw as u32);
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            vfs::namespace_path(&vfs_ctx, file.dentry(), file.mount()).ok_or(Errno::ENOENT)?
        }
    } else {
        let dirfd = if dirfd_raw as i32 == AT_FDCWD {
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
    Err(Errno::ENOSYS)
}

pub(super) fn sys_clock_gettime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_clock_gettime(ctx)
}

pub(super) fn sys_clock_settime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    clock_settime_common(ctx)
}

pub(super) fn sys_clock_adjtime64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_clock_getres_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_clock_getres(ctx)
}

pub(super) fn sys_clock_nanosleep_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_clock_nanosleep(ctx)
}

pub(super) fn sys_timer_gettime64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timer_settime64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
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
    let fdt = vfs::current_fdtable().ok_or(Errno::ENOSYS)?;
    let cred = vfs::current_vfs_context()
        .map(|ctx| Arc::clone(&ctx.cred))
        .ok_or(Errno::ENOSYS)?;
    let fd = pidfd::create(&fdt, cred, task, (flags & PIDFD_NONBLOCK) != 0)?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_landlock_create_ruleset(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_landlock_add_rule(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_landlock_restrict_self(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_process_mrelease(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_lsm_get_self_attr(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_lsm_set_self_attr(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_lsm_list_modules(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

fn futex_wait(
    task: &Arc<Task>,
    key: FutexKey,
    uaddr: usize,
    expected: u32,
    bitset: u32,
    deadline_ns: Option<u64>,
) -> Result<usize, Errno> {
    let me = Arc::clone(task);
    if read_user_u32(uaddr)? != expected {
        return Err(Errno::EAGAIN);
    }
    if let Some(deadline) = deadline_ns {
        if !sched::register_sleep_deadline(task, deadline) {
            return Err(Errno::ETIMEDOUT);
        }
    }
    {
        let mut table = FUTEX_TABLE.lock();
        table
            .entry(key)
            .or_insert(FutexBucket {
                waiters: Vec::new(),
            })
            .waiters
            .push(FutexWaiter {
                task: Arc::downgrade(&me),
                bitset,
                waitv_index: None,
            });
    }
    loop {
        if let Some(deadline) = deadline_ns {
            if sched::now_ns_public() >= deadline {
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
        if read_user_u32(uaddr).map_or(true, |cur| cur != expected) {
            futex_remove_waiter(key, &me);
            restore_current_task_after_sleep(task);
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            return Err(Errno::EAGAIN);
        }
        let _ = task.cas_state(TaskState::Running, TaskState::Sleeping);
        sched::operation::sched_yield()?;
        let table = FUTEX_TABLE.lock();
        let still_waiting = table
            .get(&key)
            .map(|bucket| {
                bucket.waiters.iter().any(|w| {
                    w.task
                        .upgrade()
                        .as_ref()
                        .is_some_and(|waiter| Arc::ptr_eq(waiter, &me))
                })
            })
            .unwrap_or(false);
        if !still_waiting {
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            restore_current_task_after_sleep(task);
            return Ok(0);
        }
    }
}

#[derive(Clone, Copy)]
struct FutexWaitvEntry {
    index: usize,
    uaddr: usize,
    expected: u32,
    key: FutexKey,
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
    let task = sched::current_task();
    Ok(FutexWaitvEntry {
        index,
        uaddr,
        expected: val as u32,
        key: futex_key(&task, uaddr, private)?,
    })
}

fn futex_waitv_contains(bucket: &FutexBucket, task: &Arc<Task>, waitv_index: usize) -> bool {
    bucket.waiters.iter().any(|waiter| {
        waiter
            .task
            .upgrade()
            .as_ref()
            .is_some_and(|waiter_task| Arc::ptr_eq(waiter_task, task))
            && waiter.waitv_index == Some(waitv_index)
    })
}

fn futex_waitv_woken_index(entries: &[FutexWaitvEntry], task: &Arc<Task>) -> Option<usize> {
    let table = FUTEX_TABLE.lock();
    for entry in entries {
        let still_waiting = table
            .get(&entry.key)
            .map(|bucket| futex_waitv_contains(bucket, task, entry.index))
            .unwrap_or(false);
        if !still_waiting {
            return Some(entry.index);
        }
    }
    None
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
    let sched_now = sched::now_ns_public();
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
    let sched_now = sched::now_ns_public();
    if cmd == FUTEX_WAIT {
        return Ok(Some(sched_now.saturating_add(timeout_ns)));
    }
    let clock_id = if (futex_op & FUTEX_CLOCK_REALTIME) != 0 {
        crate::vdso::CLOCK_REALTIME
    } else {
        crate::vdso::CLOCK_MONOTONIC
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
    let _ = sched::cancel_sleep_deadline(task);
    let current = sched::current_task();
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
    exit_robust_list(task);
    clear_child_tid_and_wake(task);
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
    let robust = task.robust_list();
    if robust.head == 0 || robust.len != ROBUST_LIST_HEAD_SIZE {
        return;
    }
    if !robust_node_aligned(robust.head)
        || !task_user_range_readable(task, robust.head, ROBUST_LIST_HEAD_SIZE)
    {
        task.set_robust_list(0, 0);
        return;
    }
    let tid = task.pid_root().unwrap_or(0) as u32;
    let Ok(futex_offset) = read_robust_isize(task, robust.head + 8) else {
        task.set_robust_list(0, 0);
        return;
    };
    let pending = read_robust_usize(task, robust.head + 16).unwrap_or(0);
    let mut next = read_robust_usize(task, robust.head).unwrap_or(0);
    let mut walked = 0usize;
    let mut visited = Vec::new();
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
        let Ok(next_link) = read_robust_usize(task, next) else {
            log::debug!(
                "[syscall][robust] pid={:?} stopped at unreadable robust node {:#x}",
                task.pid_root(),
                next,
            );
            break;
        };
        handle_robust_node(task, next, futex_offset, tid);
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
        handle_robust_node(task, pending, futex_offset, tid);
    }
    task.set_robust_list(0, 0);
}

fn handle_robust_node(task: &Arc<Task>, node: usize, futex_offset: isize, tid: u32) {
    let Some(uaddr) = robust_futex_addr(node, futex_offset) else {
        return;
    };
    if uaddr % 4 != 0 {
        return;
    }
    let Ok(cur) = read_robust_u32(task, uaddr) else {
        return;
    };
    if (cur & FUTEX_TID_MASK) != tid {
        return;
    }
    let new = (cur & !FUTEX_TID_MASK) | FUTEX_OWNER_DIED;
    if write_user_u32(uaddr, new).is_ok() {
        let _ = futex_wake_addr(task, uaddr, 1);
    }
}

fn robust_futex_addr(node: usize, futex_offset: isize) -> Option<usize> {
    let addr = (node as isize).checked_add(futex_offset)?;
    if addr < 0 { None } else { Some(addr as usize) }
}

fn robust_node_aligned(node: usize) -> bool {
    node % core::mem::align_of::<usize>() == 0
}

fn task_user_range_readable(task: &Arc<Task>, user: usize, len: usize) -> bool {
    task_vm_space(task).is_some_and(|vm| vm.is_user_range_readable(user, len))
}

fn read_robust_usize(task: &Arc<Task>, user: usize) -> Result<usize, Errno> {
    if !task_user_range_readable(task, user, core::mem::size_of::<usize>()) {
        return Err(Errno::EFAULT);
    }
    read_user_usize(user)
}

fn read_robust_isize(task: &Arc<Task>, user: usize) -> Result<isize, Errno> {
    if !task_user_range_readable(task, user, core::mem::size_of::<isize>()) {
        return Err(Errno::EFAULT);
    }
    read_user_isize(user)
}

fn read_robust_u32(task: &Arc<Task>, user: usize) -> Result<u32, Errno> {
    if !task_user_range_readable(task, user, core::mem::size_of::<u32>()) {
        return Err(Errno::EFAULT);
    }
    read_user_u32(user)
}

fn read_user_usize(user: usize) -> Result<usize, Errno> {
    let mut raw = [0u8; core::mem::size_of::<usize>()];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(usize::from_ne_bytes(raw))
}

fn read_user_isize(user: usize) -> Result<isize, Errno> {
    let mut raw = [0u8; core::mem::size_of::<isize>()];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(isize::from_ne_bytes(raw))
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
    crate::vdso::set_realtime_ns(read_timespec_ns(tp)?);
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

static PRNG_STATE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0xDEAD_BEEF_CAFE_BABEu64);

fn prng_fill(buf: &mut [u8]) {
    let mut state = PRNG_STATE.load(core::sync::atomic::Ordering::Relaxed);
    if state == 0 {
        state = sched::now_ns_public() | 1;
    }
    for chunk in buf.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    PRNG_STATE.store(state, core::sync::atomic::Ordering::Relaxed);
}
