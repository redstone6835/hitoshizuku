//! 进程与 libc 初始化相关 syscall。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use errno::Errno;
use general::firmware::power;
use general::mm::{copy_from_user, copy_to_user};
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
    log::debug!(
        "[syscall][exit] pid={:?} code={}",
        ctx.task.pid_root(),
        code
    );
    clear_child_tid_and_wake(&ctx.task);
    sched::operation::exit(code);
}

pub(super) fn sys_exit_group(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let code = ctx.args[0] as i32;
    log::debug!(
        "[syscall][exit_group] pid={:?} code={}",
        ctx.task.pid_root(),
        code
    );
    clear_child_tid_and_wake(&ctx.task);
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
    if user == 0 || size < 64 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 88];
    let n = size.min(raw.len());
    copy_from_user(user, &mut raw[..n]).map_err(|e| e.as_errno())?;
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

    let creds = ctx.task.credentials();
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
            write_u32(&mut raw, 20, ctx.task.credentials().uid.0);
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
    ctx.task.set_clear_child_tid(ctx.args[0]);
    Ok(sched::operation::gettid() as usize)
}

pub(super) fn sys_set_robust_list(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(0)
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
    let new = sched::RlimitPair::new(
        sched::Rlim::from_raw(cur),
        sched::Rlim::from_raw(max),
    );
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
        if sched::now_ns_public() >= deadline {
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
    sched::operation::sched_getattr(pid)?;
    if cpusetsize == 0 || cpusetsize > MAX_CPUSET_BYTES || mask_user == 0 {
        return Err(Errno::EINVAL);
    }

    let mut mask = Vec::new();
    mask.resize(cpusetsize, 0);
    mask[0] = 1;
    copy_to_user(mask_user, &mask).map_err(|e| e.as_errno())?;
    Ok(core::mem::size_of::<usize>())
}

pub(super) fn sys_sched_setaffinity(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let cpusetsize = ctx.args[1];
    let mask_user = ctx.args[2];
    sched::operation::sched_getattr(pid)?;
    if cpusetsize == 0 || cpusetsize > MAX_CPUSET_BYTES || mask_user == 0 {
        return Err(Errno::EINVAL);
    }
    let mut first = [0u8; 1];
    copy_from_user(mask_user, &mut first).map_err(|e| e.as_errno())?;
    if (first[0] & 1) == 0 {
        return Err(Errno::EINVAL);
    }
    Ok(0)
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

pub(super) fn sys_personality(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let _persona = ctx.args[0];
    Ok(0)
}

pub(super) fn sys_prctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const PR_SET_NAME: usize = 15;
    const PR_GET_NAME: usize = 16;
    match ctx.args[0] {
        PR_SET_NAME => Ok(0),
        PR_GET_NAME => {
            let buf = ctx.args[1];
            if buf != 0 {
                let name = b"mygo\0";
                copy_to_user(buf, name).map_err(|e| e.as_errno())?;
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
    let creds = ctx.task.credentials();
    if creds.euid == Uid::ROOT || creds.uid == uid || creds.euid == uid || creds.suid == uid {
        let mut new = (*creds).clone();
        new.uid = uid;
        new.euid = uid;
        new.suid = uid;
        ctx.task.set_credentials(Arc::new(new));
        Ok(0)
    } else {
        Err(Errno::EPERM)
    }
}

pub(super) fn sys_setgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let gid = Gid(ctx.args[0] as u32);
    let creds = ctx.task.credentials();
    if creds.euid == Uid::ROOT || creds.gid == gid || creds.egid == gid || creds.sgid == gid {
        let mut new = (*creds).clone();
        new.gid = gid;
        new.egid = gid;
        new.sgid = gid;
        ctx.task.set_credentials(Arc::new(new));
        Ok(0)
    } else {
        Err(Errno::EPERM)
    }
}

pub(super) fn sys_setreuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let ruid = ctx.args[0] as u32;
    let euid = ctx.args[1] as u32;
    let creds = ctx.task.credentials();
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
    ctx.task.set_credentials(Arc::new(new));
    Ok(0)
}

pub(super) fn sys_setregid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let rgid = ctx.args[0] as u32;
    let egid = ctx.args[1] as u32;
    let creds = ctx.task.credentials();
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
    ctx.task.set_credentials(Arc::new(new));
    Ok(0)
}

pub(super) fn sys_setresuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let ruid = ctx.args[0] as u32;
    let euid = ctx.args[1] as u32;
    let suid = ctx.args[2] as u32;
    let creds = ctx.task.credentials();
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
    ctx.task.set_credentials(Arc::new(new));
    Ok(0)
}

pub(super) fn sys_setresgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let rgid = ctx.args[0] as u32;
    let egid = ctx.args[1] as u32;
    let sgid = ctx.args[2] as u32;
    let creds = ctx.task.credentials();
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
    ctx.task.set_credentials(Arc::new(new));
    Ok(0)
}

/// FIXME: setfsuid/setfsgid 目前为安全空操作，始终返回当前 uid/gid，
/// 不修改任何凭据字段。POSIX 要求 fsuid/fsgid 是独立于 uid/gid 的字段，
/// 用于文件系统权限检查；正确实现需在 Credentials 中新增 fsuid/fsgid
/// 字段并修改 VFS 权限检查逻辑。
pub(super) fn sys_setfsuid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task.credentials().uid.0 as usize)
}

pub(super) fn sys_setfsgid(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(ctx.task.credentials().gid.0 as usize)
}

pub(super) fn sys_getgroups(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let size = ctx.args[0];
    let list = ctx.args[1];
    let creds = ctx.task.credentials();
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
    let creds = ctx.task.credentials();
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
    ctx.task.set_credentials(Arc::new(new));
    Ok(0)
}

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_PRIVATE_FLAG: u32 = 128;

struct FutexBucket {
    waiters: Vec<Arc<sched::Task>>,
}

static FUTEX_TABLE: Spinlock<BTreeMap<usize, FutexBucket>> = Spinlock::new(BTreeMap::new());

fn futex_wake_addr(uaddr: usize, count: usize) -> usize {
    let waiters = {
        let mut table = FUTEX_TABLE.lock();
        let Some(bucket) = table.get_mut(&uaddr) else {
            return 0;
        };
        let count = count.min(bucket.waiters.len());
        let waiters: Vec<_> = bucket.waiters.drain(..count).collect();
        if bucket.waiters.is_empty() {
            table.remove(&uaddr);
        }
        waiters
    };
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

fn clear_child_tid_and_wake(task: &Arc<Task>) {
    let tid_addr = task.clear_child_tid();
    if tid_addr == 0 {
        return;
    }
    let zero = 0usize;
    let _ = copy_to_user(tid_addr, &zero.to_ne_bytes());
    task.set_clear_child_tid(0);
    let _ = futex_wake_addr(tid_addr, 1);
}

pub(super) fn sys_futex(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let uaddr = ctx.args[0];
    let futex_op = ctx.args[1] as u32;
    let val = ctx.args[2] as u32;
    let _timeout = ctx.args[3];
    let _uaddr2 = ctx.args[4];
    let _val3 = ctx.args[5] as u32;

    let cmd = futex_op & !FUTEX_PRIVATE_FLAG;

    if uaddr % 4 != 0 {
        return Err(Errno::EINVAL);
    }

    match cmd {
        FUTEX_WAIT => {
            let me = Arc::clone(&ctx.task);
            {
                let mut table = FUTEX_TABLE.lock();
                let mut cur = [0u8; 4];
                copy_from_user(uaddr, &mut cur).map_err(|e| e.as_errno())?;
                let cur_val = u32::from_le_bytes(cur);
                if cur_val != val {
                    return Err(Errno::EAGAIN);
                }
                let bucket = table.entry(uaddr).or_insert(FutexBucket {
                    waiters: Vec::new(),
                });
                bucket.waiters.push(me.clone());
            }

            loop {
                if !ctx.task.cas_state(TaskState::Running, TaskState::Sleeping) {
                    continue;
                }
                let mut cur = [0u8; 4];
                if copy_from_user(uaddr, &mut cur).is_err() || u32::from_le_bytes(cur) != val {
                    let mut table = FUTEX_TABLE.lock();
                    if let Some(bucket) = table.get_mut(&uaddr) {
                        bucket.waiters.retain(|w| !Arc::ptr_eq(w, &me));
                        if bucket.waiters.is_empty() {
                            table.remove(&uaddr);
                        }
                    }
                    let _ = ctx.task.cas_state(TaskState::Sleeping, TaskState::Runnable);
                    return Err(Errno::EAGAIN);
                }
                sched::operation::sched_yield()?;
                let table = FUTEX_TABLE.lock();
                let still_waiting = table
                    .get(&uaddr)
                    .map(|b| b.waiters.iter().any(|w| Arc::ptr_eq(w, &me)))
                    .unwrap_or(false);
                if !still_waiting {
                    return Ok(0);
                }
            }
        }
        FUTEX_WAKE => {
            // val 是第 3 个参数，即 ctx.args[2]，不是 ctx.args[5]
            Ok(futex_wake_addr(uaddr, val as usize))
        }
        _ => Err(Errno::ENOSYS),
    }
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
