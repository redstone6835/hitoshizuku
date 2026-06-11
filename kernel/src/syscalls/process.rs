//! 进程与 libc 初始化相关 syscall。

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use errno::Errno;
use general::firmware::power;
use general::mm::{VmSpace, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use sched::clone_flags::{CloneArgs, CloneFlags};
use sched::ids::{Capability, Gid, Uid};
use sched::process_ops::{ExecRequest, UserContextRef};
use sched::sync::Spinlock;
use sched::task::{Task, TaskState};
use sched::{SchedAttr, SchedPolicy, SignalNumber, WaitId, WaitOptions, WaitStatus};

const MAX_CPUSET_BYTES: usize = 1024;

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
    futex_remove_task_waiters(&task);
    exit_robust_list(&task);
    clear_child_tid_and_wake(&task);
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
        futex_remove_task_waiters(&member);
        exit_robust_list(&member);
        clear_child_tid_and_wake(&member);
    }
    ctx.release_task_ref();
    drop(task);
    sched::operation::exit_group(code);
}

pub(super) fn sys_clone(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let regs = hal::user::decode_clone_register_args(ctx.args);
    let args = CloneArgs {
        flags: CloneFlags::from_raw(regs.flags),
        pidfd: 0,
        stack: regs.stack,
        stack_size: 0,
        parent_tid: regs.parent_tid,
        child_tid: regs.child_tid,
        tls: regs.tls,
        exit_signal: regs.flags & 0xff,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: 0,
    };
    let pid = sched::operation::clone_with_context(args, UserContextRef::new(ctx.tf.as_usize()))?;
    Ok(pid as usize)
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
    let args = CloneArgs {
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
        cgroup: read_u64(10) as usize,
    };
    if args.pidfd != 0 || args.set_tid != 0 || args.set_tid_size != 0 || args.cgroup != 0 {
        // TODO(threading): clone3 pidfd/set_tid/cgroup need pidfd file
        // objects, namespace-aware TID allocation and cgroup membership state.
        return Err(Errno::EOPNOTSUPP);
    }
    let pid = sched::operation::clone_with_context(args, UserContextRef::new(ctx.tf.as_usize()))?;
    Ok(pid as usize)
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
        let zero_rusage = [0u8; 144];
        copy_to_user(rusage_user, &zero_rusage).map_err(|e| e.as_errno())?;
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
        P_PIDFD => WaitId::Pidfd(id),
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
        let zero_rusage = [0u8; 144];
        copy_to_user(rusage, &zero_rusage).map_err(|e| e.as_errno())?;
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
    let _uinfo = ctx.args[3];
    // TODO(threading): preserve user-provided siginfo payload instead of
    // degrading this ABI to tgkill-style delivery.
    sched::operation::tgkill(tgid, tid, sig)?;
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
    let robust = task.robust_list();
    copy_to_user(head_user, &robust.head.to_ne_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(len_user, &robust.len.to_ne_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_rseq(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // TODO(threading): rseq cannot be safely advertised until context switch,
    // signal delivery, preemption and future cross-CPU migration paths all abort
    // active user critical sections. Returning ENOSYS makes libc disable rseq.
    Err(Errno::ENOSYS)
}

pub(super) fn sys_membarrier(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const MEMBARRIER_CMD_QUERY: usize = 0;
    match ctx.args[0] {
        MEMBARRIER_CMD_QUERY => Ok(0),
        _ => {
            // TODO(threading): expedited/global membarrier needs cross-CPU
            // rendezvous/IPI support after AP startup lands.
            Err(Errno::EOPNOTSUPP)
        }
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
    write_uts_field(&mut out, 1, b"mygo");
    write_uts_field(&mut out, 2, b"0.1.0");
    write_uts_field(&mut out, 3, b"MyGo kernel");
    write_uts_field(&mut out, 4, hal::platform::arch_name().as_bytes());
    write_uts_field(&mut out, 5, b"localdomain");
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
    loop {
        if sched::now_ns_public() >= deadline {
            return Ok(0);
        }
        sched::operation::sched_yield()?;
        let pending = sched::operation::sigpending()?;
        if pending.raw() != 0 {
            if rem_user != 0 {
                let now = sched::now_ns_public();
                let remaining_ns = deadline.saturating_sub(now) as i64;
                let rem_sec = remaining_ns / 1_000_000_000;
                let rem_nsec = remaining_ns % 1_000_000_000;
                let rem_buf = rem_sec
                    .to_le_bytes()
                    .into_iter()
                    .chain(rem_nsec.to_le_bytes())
                    .collect::<Vec<_>>();
                let _ = copy_to_user(rem_user, &rem_buf);
            }
            return Err(Errno::EINTR);
        }
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
    loop {
        let now = if absolute {
            crate::vdso::clock_time_ns(clock_id as usize).ok_or(Errno::EINVAL)?
        } else {
            sched::now_ns_public()
        };
        if now >= deadline {
            return Ok(0);
        }
        sched::operation::sched_yield()?;
        let pending = sched::operation::sigpending()?;
        if pending.raw() != 0 {
            if !absolute && rem_user != 0 {
                let now = sched::now_ns_public();
                let remaining_ns = deadline.saturating_sub(now) as i64;
                let rem_sec = remaining_ns / 1_000_000_000;
                let rem_nsec = remaining_ns % 1_000_000_000;
                let rem_buf = rem_sec
                    .to_le_bytes()
                    .into_iter()
                    .chain(rem_nsec.to_le_bytes())
                    .collect::<Vec<_>>();
                let _ = copy_to_user(rem_user, &rem_buf);
            }
            return Err(Errno::EINTR);
        }
    }
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
    if buf != 0 {
        let zero = [0u8; 32];
        copy_to_user(buf, &zero).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_getrusage(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let _who = ctx.args[0];
    let usage = ctx.args[1];
    if usage != 0 {
        let zero = [0u8; 144];
        copy_to_user(usage, &zero).map_err(|e| e.as_errno())?;
    }
    Ok(0)
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
    let _which = ctx.args[0];
    let _who = ctx.args[1];
    Ok(20)
}

pub(super) fn sys_setpriority(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(0)
}

pub(super) fn sys_sched_getparam(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let param_user = ctx.args[1];
    let attr = sched::operation::sched_getattr(pid)?;
    if param_user != 0 {
        let mut out = [0u8; 4];
        out[0..4].copy_from_slice(&(attr.priority as i32).to_le_bytes());
        copy_to_user(param_user, &out).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_sched_setparam(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let param_user = ctx.args[1];
    if param_user == 0 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 4];
    copy_from_user(param_user, &mut raw).map_err(|e| e.as_errno())?;
    let priority = i32::from_le_bytes(raw);
    let mut attr = sched::operation::sched_getattr(pid)?;
    match attr.policy {
        SchedPolicy::Fair | SchedPolicy::Idle => {
            if priority != 0 {
                return Err(Errno::EINVAL);
            }
        }
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => {
            if !(1..=99).contains(&priority) {
                return Err(Errno::EINVAL);
            }
            attr.priority = priority as u8;
        }
        SchedPolicy::Deadline => return Err(Errno::EINVAL),
    }
    sched::operation::sched_setattr(pid, attr)?;
    Ok(0)
}

pub(super) fn sys_sched_getscheduler(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let attr = sched::operation::sched_getattr(pid)?;
    Ok(encode_linux_sched_policy(attr.policy))
}

pub(super) fn sys_sched_setscheduler(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let policy = decode_linux_sched_policy(ctx.args[1])?;
    let param_user = ctx.args[2];
    if param_user == 0 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 4];
    copy_from_user(param_user, &mut raw).map_err(|e| e.as_errno())?;
    let priority = i32::from_le_bytes(raw);
    if matches!(policy, SchedPolicy::Fair | SchedPolicy::Idle) && priority != 0 {
        return Err(Errno::EINVAL);
    }
    if matches!(policy, SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin)
        && !(1..=99).contains(&priority)
    {
        return Err(Errno::EINVAL);
    }
    let attr = SchedAttr {
        policy,
        nice: 0,
        slice_ns: 0,
        priority: priority as u8,
        runtime_ns: 0,
        deadline_ns: 0,
        period_ns: 0,
    };
    sched::operation::sched_setattr(pid, attr)?;
    Ok(0)
}

pub(super) fn sys_sched_getaffinity(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let cpusetsize = ctx.args[1];
    let mask_user = ctx.args[2];
    let kernel_bytes = kernel_cpuset_bytes();
    if cpusetsize < kernel_bytes || cpusetsize > MAX_CPUSET_BYTES || mask_user == 0 {
        return Err(Errno::EINVAL);
    }

    let affinity = sched::operation::sched_getaffinity(pid)?;
    let mut mask = Vec::new();
    mask.resize(cpusetsize, 0);
    write_cpuset_mask(&mut mask, affinity);
    copy_to_user(mask_user, &mask).map_err(|e| e.as_errno())?;
    Ok(kernel_bytes)
}

pub(super) fn sys_sched_setaffinity(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let cpusetsize = ctx.args[1];
    let mask_user = ctx.args[2];
    let kernel_bytes = kernel_cpuset_bytes();
    if cpusetsize < kernel_bytes || cpusetsize > MAX_CPUSET_BYTES || mask_user == 0 {
        return Err(Errno::EINVAL);
    }
    let mut mask = Vec::new();
    mask.resize(cpusetsize, 0);
    copy_from_user(mask_user, &mut mask).map_err(|e| e.as_errno())?;
    sched::operation::sched_setaffinity(pid, read_cpuset_mask(&mask))?;
    Ok(0)
}

fn kernel_cpuset_bytes() -> usize {
    sched::NR_CPUS.div_ceil(8).max(1)
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

pub(super) fn sys_sched_get_priority_max(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    match decode_linux_sched_policy(ctx.args[0])? {
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => Ok(99),
        _ => Ok(0),
    }
}

pub(super) fn sys_sched_get_priority_min(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    match decode_linux_sched_policy(ctx.args[0])? {
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => Ok(1),
        _ => Ok(0),
    }
}

pub(super) fn sys_sched_rr_get_interval(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let tp = ctx.args[1];
    if tp == 0 {
        return Err(Errno::EFAULT);
    }
    let attr = sched::operation::sched_getattr(pid)?;
    let interval_ns = if attr.slice_ns == 0 {
        100_000_000
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
    let attr = read_linux_sched_attr(attr_user)?;
    sched::operation::sched_setattr(pid, attr)?;
    Ok(0)
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
    let attr = sched::operation::sched_getattr(pid)?;
    write_linux_sched_attr(attr_user, size, attr)?;
    Ok(0)
}

fn decode_linux_sched_policy(raw: usize) -> Result<SchedPolicy, Errno> {
    const SCHED_RESET_ON_FORK: usize = 0x4000_0000;
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

fn read_linux_sched_attr(user: usize) -> Result<SchedAttr, Errno> {
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
    if flags != 0 {
        // TODO(threading): sched_attr flags such as RESET_ON_FORK need per-task
        // inheritance state. Reject them explicitly until that state exists.
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(SchedAttr {
        policy,
        nice: i32::from_le_bytes(raw[16..20].try_into().unwrap()) as i8,
        slice_ns: 0,
        priority: u32::from_le_bytes(raw[20..24].try_into().unwrap()) as u8,
        runtime_ns: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
        deadline_ns: u64::from_le_bytes(raw[32..40].try_into().unwrap()),
        period_ns: u64::from_le_bytes(raw[40..48].try_into().unwrap()),
    })
}

fn write_linux_sched_attr(user: usize, size: usize, attr: SchedAttr) -> Result<(), Errno> {
    if size < LINUX_SCHED_ATTR_BASE_SIZE {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; LINUX_SCHED_ATTR_SIZE];
    raw[0..4].copy_from_slice(&(LINUX_SCHED_ATTR_SIZE as u32).to_le_bytes());
    raw[4..8].copy_from_slice(&(encode_linux_sched_policy(attr.policy) as u32).to_le_bytes());
    raw[8..16].copy_from_slice(&0u64.to_le_bytes());
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
        _ => Ok(0),
    }
}

pub(super) fn sys_capget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let hdrp = ctx.args[0];
    let datap = ctx.args[1];
    if hdrp != 0 {
        let mut hdr = [0u8; 16];
        copy_from_user(hdrp, &mut hdr).map_err(|e| e.as_errno())?;
        hdr[0..4].copy_from_slice(&0x20080522u32.to_le_bytes());
        hdr[4..8].copy_from_slice(&0u32.to_le_bytes());
        copy_to_user(hdrp, &hdr).map_err(|e| e.as_errno())?;
    }
    if datap != 0 {
        let data = [0u64; 2];
        let bytes = unsafe {
            core::slice::from_raw_parts(data.as_ptr() as *const u8, core::mem::size_of_val(&data))
        };
        copy_to_user(datap, bytes).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_capset(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let _hdrp = ctx.args[0];
    let _datap = ctx.args[1];
    Ok(0)
}

pub(super) fn sys_setuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uid = Uid(ctx.args[0] as u32);
    let creds = ctx.task().credentials();
    if creds.euid == Uid::ROOT || creds.uid == uid || creds.euid == uid || creds.suid == uid {
        let mut new = (*creds).clone();
        new.uid = uid;
        new.euid = uid;
        new.suid = uid;
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
        new.euid = Uid(euid);
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
        new.egid = Gid(egid);
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
        new.euid = Uid(euid);
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
        new.egid = Gid(egid);
    }
    if sgid != u32::MAX {
        new.sgid = Gid(sgid);
    }
    ctx.task().set_credentials(Arc::new(new));
    Ok(0)
}

/// FIXME: setfsuid/setfsgid 目前为安全空操作，始终返回当前 uid/gid，
/// 不修改任何凭据字段。POSIX 要求 fsuid/fsgid 是独立于 uid/gid 的字段，
/// 用于文件系统权限检查；正确实现需在 Credentials 中新增 fsuid/fsgid
/// 字段并修改 VFS 权限检查逻辑。
pub(super) fn sys_setfsuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task().credentials().uid.0 as usize)
}

pub(super) fn sys_setfsgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task().credentials().gid.0 as usize)
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
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_PRIVATE_FLAG: u32 = 128;
const FUTEX_CLOCK_REALTIME: u32 = 256;
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
const ROBUST_LIST_HEAD_SIZE: usize = 24;
const ROBUST_LIST_LIMIT: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FutexKey {
    owner: usize,
    uaddr: usize,
    shared: bool,
}

struct FutexWaiter {
    task: Weak<sched::Task>,
    bitset: u32,
}

struct FutexBucket {
    waiters: Vec<FutexWaiter>,
}

static FUTEX_TABLE: Spinlock<BTreeMap<FutexKey, FutexBucket>> = Spinlock::new(BTreeMap::new());

fn futex_cmd(futex_op: u32) -> u32 {
    futex_op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME)
}

fn task_vm_identity(task: &Arc<Task>) -> Option<usize> {
    let payload = task.ext_lookup(sched::TASKEXT_VM_SPACE)?;
    let vm = payload.downcast::<VmSpace>().ok()?;
    Some(Arc::as_ptr(&vm) as usize)
}

fn futex_key(task: &Arc<Task>, uaddr: usize, private: bool) -> FutexKey {
    if private {
        FutexKey {
            owner: task_vm_identity(task).unwrap_or_else(|| Arc::as_ptr(task) as usize),
            uaddr,
            shared: false,
        }
    } else {
        // TODO(threading): shared futex keys should be based on the mapped file
        // object or physical page, not the numeric user VA. This placeholder
        // keeps the ABI visible while private pthread futexes use precise keys.
        FutexKey {
            owner: 0,
            uaddr,
            shared: true,
        }
    }
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

fn futex_wake_addr(task: &Arc<Task>, uaddr: usize, count: usize) -> usize {
    futex_wake_key(futex_key(task, uaddr, true), count, FUTEX_BITSET_MATCH_ANY)
}

fn clear_child_tid_and_wake(task: &Arc<Task>) {
    let tid_addr = task.clear_child_tid();
    if tid_addr == 0 {
        return;
    }
    let zero = 0usize;
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
            futex_key(ctx.task(), uaddr, private),
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
                futex_key(ctx.task(), uaddr, private),
                uaddr,
                val,
                val3,
                futex_wait_deadline(futex_op, cmd, timeout)?,
            )
        }
        FUTEX_WAKE => Ok(futex_wake_key(
            futex_key(ctx.task(), uaddr, private),
            val as usize,
            FUTEX_BITSET_MATCH_ANY,
        )),
        FUTEX_WAKE_BITSET => {
            if val3 == 0 {
                return Err(Errno::EINVAL);
            }
            Ok(futex_wake_key(
                futex_key(ctx.task(), uaddr, private),
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
                futex_key(ctx.task(), uaddr, private),
                futex_key(ctx.task(), uaddr2, private),
                val as usize,
                timeout,
                FUTEX_BITSET_MATCH_ANY,
            ))
        }
        _ => {
            // TODO(threading): PI futexes, FUTEX_WAKE_OP and requeue_pi need
            // priority inheritance state attached to scheduler wait queues.
            Err(Errno::EOPNOTSUPP)
        }
    }
}

pub(super) fn sys_futex_waitv(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // TODO(threading): futex_waitv needs vector validation and shared futex key
    // support. Keep the syscall registered so userspace probing is explicit.
    Err(Errno::EOPNOTSUPP)
}

pub(super) fn sys_futex_wake(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // TODO(threading): new futex2 wake ABI has a different argument layout from
    // futex(2); implement it after the shared-key backend exists.
    Err(Errno::EOPNOTSUPP)
}

pub(super) fn sys_futex_wait(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // TODO(threading): new futex2 wait ABI is intentionally not aliased to old
    // futex because flags and timeout layout differ.
    Err(Errno::EOPNOTSUPP)
}

pub(super) fn sys_futex_requeue(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // TODO(threading): futex2 requeue should share the same future keyed wait
    // queue backend as futex_waitv/futex_wake.
    Err(Errno::EOPNOTSUPP)
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
            });
    }

    loop {
        if let Some(deadline) = deadline_ns {
            if sched::now_ns_public() >= deadline {
                futex_remove_waiter(key, &me);
                let _ = task.cas_state(TaskState::Sleeping, TaskState::Runnable);
                sched::cancel_sleep_deadline(task);
                return Err(Errno::ETIMEDOUT);
            }
        }
        if sched::operation::sigpending()?.raw() != 0 {
            futex_remove_waiter(key, &me);
            let _ = task.cas_state(TaskState::Sleeping, TaskState::Runnable);
            if deadline_ns.is_some() {
                sched::cancel_sleep_deadline(task);
            }
            return Err(Errno::EINTR);
        }
        if read_user_u32(uaddr).map_or(true, |cur| cur != expected) {
            futex_remove_waiter(key, &me);
            let _ = task.cas_state(TaskState::Sleeping, TaskState::Runnable);
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
            return Ok(0);
        }
    }
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

fn exit_robust_list(task: &Arc<Task>) {
    let robust = task.robust_list();
    if robust.head == 0 || robust.len != ROBUST_LIST_HEAD_SIZE {
        return;
    }
    let tid = task.pid_root().unwrap_or(0) as u32;
    let Ok(futex_offset) = read_user_isize(robust.head + 8) else {
        return;
    };
    let pending = read_user_usize(robust.head + 16).unwrap_or(0);
    let mut next = read_user_usize(robust.head).unwrap_or(0);
    let mut walked = 0usize;
    while next != 0 && next != robust.head && walked < ROBUST_LIST_LIMIT {
        handle_robust_node(task, next, futex_offset, tid);
        next = read_user_usize(next).unwrap_or(0);
        walked += 1;
    }
    if walked == ROBUST_LIST_LIMIT {
        log::warning!(
            "[syscall][robust] pid={:?} robust list walk hit limit; TODO fault-safe cycle detection",
            task.pid_root(),
        );
    }
    if pending != 0 {
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
    let Ok(cur) = read_user_u32(uaddr) else {
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

fn write_uts_field(out: &mut [u8], index: usize, value: &[u8]) {
    let start = index * 65;
    let n = value.len().min(64);
    out[start..start + n].copy_from_slice(&value[..n]);
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
