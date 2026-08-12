//! 信号相关 syscall：rt_sigaction / rt_sigprocmask。

use errno::Errno;
use general::mm::{copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::{self, fdtable::Fd, pidfd};
use hal::user_context::UserTrapFrame;
use sched::ids::Uid;
use sched::process_ops::UserContextRef;
use sched::task::TaskState;
use sched::{
    SigAction, SigActionFlags, SigAltStack, SigHandler, SigProcMaskHow, SigSet, SignalNumber,
};

const SIGSET_SIZE: usize = 8;
const SIG_BLOCK: usize = 0;
const SIG_UNBLOCK: usize = 1;
const SIG_SETMASK: usize = 2;
const STACK_T_SIZE: usize = 24;
const SS_ONSTACK: u32 = 1;
const SS_DISABLE: u32 = 2;
const MINSIGSTKSZ: usize = 2048;

pub(super) fn sys_rt_sigaction(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let sig = SignalNumber::from_raw(ctx.args[0] as i32).ok_or(Errno::EINVAL)?;
    let act_user = ctx.args[1];
    let old_user = ctx.args[2];
    let sigset_size = ctx.args[3];
    if sigset_size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }

    let old = ctx.task().shared_signal().get_action(sig);
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

    let old = ctx.task().signal.blocked_snapshot();
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
    ctx.task().signal.save_blocked(mask);
    #[cfg(any(feature = "trace-task-lifecycle", feature = "trace-signal-wait"))]
    log::info!(
        "[syscall][sigsuspend] enter pid={:?} mask={:#x}",
        ctx.task().pid_root(),
        mask.raw(),
    );
    // sigsuspend 即使遇到带 SA_RESTART 的 handler 也必须以 EINTR 结束；
    // 普通返回边界负责推进 PC 并建立 handler frame。
    ctx.disable_restart();
    loop {
        if ctx.task().group_exit_pending() || sched::operation::has_interrupting_signal(ctx.task())
        {
            break;
        }
        if !ctx
            .task()
            .cas_state(TaskState::Running, TaskState::Sleeping)
        {
            sched::operation::sched_yield()?;
            continue;
        }

        // 信号可能在首次检查之后、睡眠状态发布之前到达。投递方此时看到的
        // 仍是 Running，不会负责唤醒；发布 Sleeping 后必须二次检查，确保
        // check-then-sleep 窗口内到达的信号不会永久丢失。
        if ctx.task().group_exit_pending() || sched::operation::has_interrupting_signal(ctx.task())
        {
            if !ctx
                .task()
                .cas_state(TaskState::Sleeping, TaskState::Running)
            {
                let _ = ctx
                    .task()
                    .cas_state(TaskState::Runnable, TaskState::Running);
            }
            #[cfg(any(feature = "trace-task-lifecycle", feature = "trace-signal-wait"))]
            log::info!(
                "[syscall][sigsuspend] sleep-race-closed pid={:?}",
                ctx.task().pid_root(),
            );
            break;
        }
        sched::operation::sched_yield()?;
    }
    #[cfg(any(feature = "trace-task-lifecycle", feature = "trace-signal-wait"))]
    log::info!(
        "[syscall][sigsuspend] leave pid={:?}",
        ctx.task().pid_root(),
    );
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
    #[cfg(feature = "trace-signal-wait")]
    log::info!(
        "[syscall][sigtimedwait] enter pid={:?} set={:#x} timeout_ptr={:#x}",
        ctx.task().pid_root(),
        these.raw(),
        uts,
    );

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
        // NULL timeout 才表示永久等待；非 NULL 的 {0,0} 是立即轮询。
        Some(
            (sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(nsec as u64),
        )
    };

    // 3. 先非阻塞轮询；命中则直接返回。
    if let Some(info) = sched::operation::sigtimedwait_poll(these) {
        if uinfo_user != 0 {
            write_siginfo(uinfo_user, &info)?;
        }
        return Ok(info.sig.as_usize());
    }

    // 4. 没命中 → 让出调度等待，限时 timeout_ns。
    #[cfg(feature = "trace-signal-wait")]
    log::info!(
        "[syscall][sigtimedwait] sleep pid={:?} timeout_ns={:?}",
        ctx.task().pid_root(),
        timeout_ns,
    );
    let got = sched::operation::sigtimedwait_wait(these, timeout_ns);
    #[cfg(feature = "trace-signal-wait")]
    log::info!(
        "[syscall][sigtimedwait] resume pid={:?} ready={}",
        ctx.task().pid_root(),
        got,
    );
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

pub(super) fn sys_sigaltstack(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let new_user = ctx.args[0];
    let old_user = ctx.args[1];
    let current_sp = UserTrapFrame::from_context(ctx.tf.as_usize()).sp();

    if old_user != 0 {
        write_stack_t(old_user, ctx.task().sigaltstack(), current_sp)?;
    }
    if new_user == 0 {
        return Ok(0);
    }
    if ctx.task().sigaltstack().contains(current_sp) {
        return Err(Errno::EPERM);
    }
    let new_stack = read_stack_t(new_user)?;
    ctx.task().set_sigaltstack(new_stack);
    Ok(0)
}

pub(super) fn sys_restart_syscall(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::EINTR)
}

pub(super) fn sys_rt_sigqueueinfo(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    if pid <= 0 {
        return Err(Errno::EINVAL);
    }
    let Some(sig) = signal_number(ctx.args[1])? else {
        sched::operation::kill(pid, None)?;
        return Ok(0);
    };
    let info = read_queued_siginfo(ctx.args[2], sig)?;
    sched::operation::queueinfo(pid, info)?;
    Ok(0)
}

pub(super) fn sys_rt_sigtimedwait_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_rt_sigtimedwait(ctx)
}

pub(super) fn sys_pidfd_send_signal(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let sig = signal_number(ctx.args[1])?;
    let uinfo = ctx.args[2];
    let flags = ctx.args[3];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = vfs::current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
    let group = pidfd::group_from_file(&file).ok_or(Errno::EINVAL)?;
    let Some(sig) = sig else {
        sched::operation::pidfd_kill(&group, None)?;
        return Ok(0);
    };
    if uinfo == 0 {
        sched::operation::pidfd_kill(&group, Some(sig))?;
    } else {
        let info = read_queued_siginfo(uinfo, sig)?;
        sched::operation::pidfd_queueinfo(&group, info)?;
    }
    Ok(0)
}

fn signal_number(raw: usize) -> Result<Option<SignalNumber>, Errno> {
    if raw == 0 {
        Ok(None)
    } else {
        SignalNumber::from_raw(raw as i32)
            .map(Some)
            .ok_or(Errno::EINVAL)
    }
}

fn read_queued_siginfo(user: usize, sig: SignalNumber) -> Result<sched::SigInfo, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 128];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let signo = i32::from_le_bytes(raw[0..4].try_into().unwrap());
    if signo != sig.raw() as i32 {
        return Err(Errno::EINVAL);
    }
    Ok(sched::SigInfo {
        sig,
        code: i32::from_le_bytes(raw[8..12].try_into().unwrap()),
        sender_pid: i32::from_le_bytes(raw[12..16].try_into().unwrap()),
        sender_uid: Uid(u32::from_le_bytes(raw[16..20].try_into().unwrap())),
        raw: Some(raw),
    })
}

fn fd_arg(raw: usize) -> Result<Fd, Errno> {
    let fd = raw as isize;
    if fd < 0 {
        return Err(Errno::EBADF);
    }
    Ok(Fd::from_raw(fd as u32))
}

fn read_sigaction(user: usize) -> Result<SigAction, Errno> {
    let mut raw = [0u8; 24];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let handler = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
    let flags = u64::from_le_bytes(raw[8..16].try_into().unwrap()) as u32;
    let mask = u64::from_le_bytes(raw[16..24].try_into().unwrap());
    let handler = match handler {
        0 => SigHandler::Default,
        1 => SigHandler::Ignore,
        addr => SigHandler::Handler(addr),
    };
    Ok(SigAction {
        handler,
        mask: SigSet::from_raw(mask),
        flags: SigActionFlags(flags),
        restorer: 0,
    })
}

fn write_sigaction(user: usize, action: SigAction) -> Result<(), Errno> {
    let mut raw = [0u8; 24];
    let handler = match action.handler {
        SigHandler::Default => 0usize,
        SigHandler::Ignore => 1usize,
        SigHandler::Handler(addr) => addr,
    };
    raw[0..8].copy_from_slice(&(handler as u64).to_le_bytes());
    raw[8..16].copy_from_slice(&(action.flags.raw() as u64).to_le_bytes());
    raw[16..24].copy_from_slice(&action.mask.raw().to_le_bytes());
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

fn read_stack_t(user: usize) -> Result<SigAltStack, Errno> {
    let mut raw = [0u8; STACK_T_SIZE];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let sp = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
    let flags = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    let size = u64::from_le_bytes(raw[16..24].try_into().unwrap()) as usize;
    if (flags & !(SS_DISABLE | SS_ONSTACK)) != 0 || (flags & SS_ONSTACK) != 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & SS_DISABLE) != 0 {
        return Ok(SigAltStack::disabled());
    }
    if size < MINSIGSTKSZ {
        return Err(Errno::ENOMEM);
    }
    sp.checked_add(size).ok_or(Errno::EINVAL)?;
    Ok(SigAltStack {
        sp,
        size,
        disabled: false,
    })
}

fn write_stack_t(user: usize, stack: SigAltStack, current_sp: usize) -> Result<(), Errno> {
    let mut raw = [0u8; STACK_T_SIZE];
    raw[0..8].copy_from_slice(&(stack.sp as u64).to_le_bytes());
    let flags = if stack.disabled {
        SS_DISABLE
    } else if stack.contains(current_sp) {
        SS_ONSTACK
    } else {
        0
    };
    raw[8..12].copy_from_slice(&flags.to_le_bytes());
    raw[16..24].copy_from_slice(&(stack.size as u64).to_le_bytes());
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn put_i32(out: &mut [u8], off: usize, v: i32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut [u8], off: usize, v: u32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
