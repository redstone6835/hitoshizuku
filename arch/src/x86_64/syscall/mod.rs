//! x86_64 Linux syscall ABI。

pub mod nr;

mod frame_ops;

use errno::Errno;
use general::syscall::{SyscallContext, SyscallFn};

use super::paging;
use super::trap_frame::TrapFrame;

const ARCH_SET_GS: usize = 0x1001;
const ARCH_SET_FS: usize = 0x1002;
const ARCH_GET_FS: usize = 0x1003;
const ARCH_GET_GS: usize = 0x1004;
const USER_SPACE_TOP: usize = 0x0000_8000_0000_0000;

/// 注册 x86_64 的 syscall trap-frame 适配器。
pub fn register() {
    general::syscall::register_frame_ops(&frame_ops::SYSCALL_FRAME_OPS);
}

#[inline]
fn valid_user_segment_base(base: usize) -> bool {
    base < USER_SPACE_TOP && paging::is_canonical(base as u64, false)
}

/// Apply an `arch_prctl(2)` operation to the software-owned user segment bases.
///
/// The trap return path publishes these fields to `MSR_FS_BASE` and
/// `MSR_KERNEL_GS_BASE`, so changing the live frame is sufficient and keeps the
/// values attached to the task across scheduling.
fn apply_arch_prctl(
    frame: &mut TrapFrame,
    operation: usize,
    argument: usize,
) -> Result<Option<usize>, Errno> {
    match operation {
        ARCH_SET_FS | ARCH_SET_GS => {
            if !valid_user_segment_base(argument) {
                return Err(Errno::EPERM);
            }
            if operation == ARCH_SET_FS {
                frame.fs_base = argument;
            } else {
                frame.gs_base = argument;
            }
            Ok(None)
        }
        ARCH_GET_FS => Ok(Some(frame.fs_base)),
        ARCH_GET_GS => Ok(Some(frame.gs_base)),
        _ => Err(Errno::EINVAL),
    }
}

fn sys_arch_prctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let operation = ctx.args[0];
    let argument = ctx.args[1];
    let output = {
        // Safety: the syscall dispatcher guarantees that `ctx.tf` points to the
        // current task's uniquely borrowed live x86 trap frame.
        let frame = unsafe { &mut *(ctx.tf.as_usize() as *mut TrapFrame) };
        apply_arch_prctl(frame, operation, argument)?
    };
    if let Some(value) = output {
        general::mm::copy_to_user(argument, &value.to_ne_bytes())
            .map_err(|error| error.as_errno())?;
    }
    Ok(0)
}

/// 注册 x86_64 架构私有的 Linux syscall。
pub fn register_linux_extensions(register: fn(usize, SyscallFn)) {
    register(nr::SYS_ARCH_PRCTL, sys_arch_prctl);
}

/// x86_64 trap 后端接入后由其提供断点钩子注册器。
pub const USER_BREAK_HOOK_REGISTRAR: Option<fn(fn(usize) -> bool)> =
    Some(crate::x86_64::trap::register_user_break_hook);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_bases_roundtrip_through_arch_prctl_operations() {
        let mut frame = TrapFrame::default();
        let fs = 0x0000_1234_5678_9000;
        let gs = 0x0000_2345_6789_a000;

        assert_eq!(apply_arch_prctl(&mut frame, ARCH_SET_FS, fs), Ok(None));
        assert_eq!(apply_arch_prctl(&mut frame, ARCH_SET_GS, gs), Ok(None));
        assert_eq!(apply_arch_prctl(&mut frame, ARCH_GET_FS, 0), Ok(Some(fs)));
        assert_eq!(apply_arch_prctl(&mut frame, ARCH_GET_GS, 0), Ok(Some(gs)));
    }

    #[test]
    fn arch_prctl_rejects_kernel_and_unknown_operations() {
        let mut frame = TrapFrame::default();

        assert_eq!(
            apply_arch_prctl(&mut frame, ARCH_SET_FS, USER_SPACE_TOP),
            Err(Errno::EPERM)
        );
        assert_eq!(
            apply_arch_prctl(&mut frame, ARCH_SET_GS, usize::MAX),
            Err(Errno::EPERM)
        );
        assert_eq!(apply_arch_prctl(&mut frame, 0, 0), Err(Errno::EINVAL));
        assert_eq!(frame.fs_base, 0);
        assert_eq!(frame.gs_base, 0);
    }
}
