//! 信号相关 syscall：rt_sigaction / rt_sigprocmask。

use errno::Errno;
use general::mm::{copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use sched::process_ops::UserContextRef;
use sched::task::TaskState;
use sched::{SigAction, SigActionFlags, SigHandler, SigProcMaskHow, SigSet, SignalNumber};

const SIGSET_SIZE: usize = 8;
const SIG_BLOCK: usize = 0;
const SIG_UNBLOCK: usize = 1;
const SIG_SETMASK: usize = 2;

pub(super) fn sys_rt_sigaction(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let sig = SignalNumber::from_raw(ctx.args[0] as i32).ok_or(Errno::EINVAL)?;
    let act_user = ctx.args[1];
    let old_user = ctx.args[2];
    let sigset_size = ctx.args[3];
    if sigset_size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }

    let old = ctx.task.shared_signal().get_action(sig);
    if old_user != 0 {
        write_sigaction(old_user, old)?;
    }
    if act_user != 0 {
        let new = read_sigaction(act_user)?;
        sched::operation::sigaction(sig, new)?;
    }
    Ok(0)
}

pub(super) fn sys_rt_sigprocmask(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let how_raw = ctx.args[0];
    let set_user = ctx.args[1];
    let old_user = ctx.args[2];
    let sigset_size = ctx.args[3];
    if sigset_size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }

    let old = ctx.task.signal.blocked_snapshot();
    if old_user != 0 {
        copy_to_user(old_user, &old.raw().to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    if set_user == 0 {
        return Ok(0);
    }

    let mut raw = [0u8; 8];
    copy_from_user(set_user, &mut raw).map_err(|e| e.as_errno())?;
    let set = SigSet::from_raw(u64::from_le_bytes(raw));
    let how = match how_raw {
        SIG_BLOCK => SigProcMaskHow::Block,
        SIG_UNBLOCK => SigProcMaskHow::Unblock,
        SIG_SETMASK => SigProcMaskHow::SetMask,
        _ => return Err(Errno::EINVAL),
    };
    sched::operation::sigprocmask(how, set)?;
    Ok(0)
}

pub(super) fn sys_rt_sigpending(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let set_user = ctx.args[0];
    let sigset_size = ctx.args[1];
    if sigset_size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let pending = sched::operation::sigpending()?;
    copy_to_user(set_user, &pending.raw().to_le_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_rt_sigreturn(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sched::operation::sigreturn_with_context(UserContextRef::new(ctx.tf.as_usize()))?;
    ctx.finalize_frame();
    Ok(0)
}

pub(super) fn sys_rt_sigsuspend(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let set_user = ctx.args[0];
    let sigset_size = ctx.args[1];
    if sigset_size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 8];
    copy_from_user(set_user, &mut raw).map_err(|e| e.as_errno())?;
    let mask = SigSet::from_raw(u64::from_le_bytes(raw));
    ctx.task.signal.save_blocked(mask);
    loop {
        let pending = sched::operation::sigpending()?;
        if pending.raw() != 0 {
            break;
        }
        if !ctx.task.cas_state(TaskState::Running, TaskState::Sleeping) {
            continue;
        }
        sched::operation::sched_yield()?;
    }
    ctx.task.signal.restore_blocked();
    Err(Errno::EINTR)
}

pub(super) fn sys_rt_sigtimedwait(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Linux ABI:
    //   long sys_rt_sigtimedwait(
    //       const sigset_t *uthese,    // a0
    //       siginfo_t *uinfo,          // a1
    //       const struct __kernel_timespec *uts,  // a2
    //       size_t sigsetsize          // a3
    //   );
    let uthese = ctx.args[0];
    let uinfo_user = ctx.args[1];
    let uts = ctx.args[2];
    let sigset_size = ctx.args[3];
    if sigset_size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    if uthese == 0 {
        return Err(Errno::EFAULT);
    }

    // 1. 读 these 信号集。
    let mut raw = [0u8; 8];
    copy_from_user(uthese, &mut raw).map_err(|e| e.as_errno())?;
    let these = SigSet::from_raw(u64::from_le_bytes(raw));
    if these.0 == 0 {
        return Err(Errno::EINVAL);
    }

    // 2. 解析 timeout。NULL 表示永久等待；其它按 timespec 解释。
    let timeout_ns: Option<u64> = if uts == 0 {
        None
    } else {
        let mut ts = [0u8; 16];
        copy_from_user(uts, &mut ts).map_err(|e| e.as_errno())?;
        let sec = i64::from_le_bytes(ts[0..8].try_into().unwrap());
        let nsec = i64::from_le_bytes(ts[8..16].try_into().unwrap());
        if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
            return Err(Errno::EINVAL);
        }
        // {0,0} → 永久等待；其它 → ns 总数。
        if sec == 0 && nsec == 0 {
            None
        } else {
            Some(
                (sec as u64)
                    .saturating_mul(1_000_000_000)
                    .saturating_add(nsec as u64),
            )
        }
    };

    // 3. 先非阻塞轮询；命中则直接返回。
    if let Some(info) = sched::operation::sigtimedwait_poll(these) {
        if uinfo_user != 0 {
            write_siginfo(uinfo_user, &info)?;
        }
        return Ok(info.sig.as_usize());
    }

    // 4. 没命中 → 让出调度等待，限时 timeout_ns。
    let got = sched::operation::sigtimedwait_wait(these, timeout_ns);
    if !got {
        return Err(Errno::EAGAIN);
    }
    // 再次轮询；理论上 wait 出来应该命中。
    let info = sched::operation::sigtimedwait_poll(these)
        .expect("[sigtimedwait] wait returned but poll found nothing");
    if uinfo_user != 0 {
        write_siginfo(uinfo_user, &info)?;
    }
    Ok(info.sig.as_usize())
}

pub(super) fn sys_sigaltstack(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_restart_syscall(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::EINTR)
}

pub(super) fn sys_rt_sigqueueinfo(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_rt_sigtimedwait_time64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_pidfd_send_signal(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

fn read_sigaction(user: usize) -> Result<SigAction, Errno> {
    let mut raw = [0u8; 32];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let handler = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
    let flags = u64::from_le_bytes(raw[8..16].try_into().unwrap()) as u32;
    let restorer = u64::from_le_bytes(raw[16..24].try_into().unwrap()) as usize;
    let mask = u64::from_le_bytes(raw[24..32].try_into().unwrap());
    let handler = match handler {
        0 => SigHandler::Default,
        1 => SigHandler::Ignore,
        addr => SigHandler::Handler(addr),
    };
    Ok(SigAction {
        handler,
        mask: SigSet::from_raw(mask),
        flags: SigActionFlags(flags),
        restorer,
    })
}

fn write_sigaction(user: usize, action: SigAction) -> Result<(), Errno> {
    let mut raw = [0u8; 32];
    let handler = match action.handler {
        SigHandler::Default => 0usize,
        SigHandler::Ignore => 1usize,
        SigHandler::Handler(addr) => addr,
    };
    raw[0..8].copy_from_slice(&(handler as u64).to_le_bytes());
    raw[8..16].copy_from_slice(&(action.flags.raw() as u64).to_le_bytes());
    raw[16..24].copy_from_slice(&(action.restorer as u64).to_le_bytes());
    raw[24..32].copy_from_slice(&action.mask.raw().to_le_bytes());
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn write_siginfo(user: usize, info: &sched::SigInfo) -> Result<(), Errno> {
    if let Some(raw) = info.raw {
        return copy_to_user(user, &raw).map_err(|e| e.as_errno());
    }
    let mut raw = [0u8; 128];
    put_i32(&mut raw, 0, info.sig.raw() as i32);
    put_i32(&mut raw, 8, info.code);
    put_i32(&mut raw, 12, info.sender_pid);
    put_u32(&mut raw, 16, info.sender_uid.0);
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn put_i32(out: &mut [u8], off: usize, v: i32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut [u8], off: usize, v: u32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
