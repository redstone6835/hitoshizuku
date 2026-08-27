//! RISC-V64 syscall 注入、Linux ABI 编号与架构私有 syscall。
mod frame_ops;
pub mod nr;

use errno::Errno;
use general::TaskOps;
use general::syscall::{SyscallContext, SyscallFn};

/// 由 `arch::riscv64::sched_ctx::register` 启动期调用一次。
pub fn register() {
    general::syscall::register_frame_ops(&frame_ops::SYSCALL_FRAME_OPS);
}

const SYS_RISCV_FLUSH_ICACHE_LOCAL: usize = 1;

fn validate_flush_icache_flags(flags: usize) -> Result<(), Errno> {
    if flags & !SYS_RISCV_FLUSH_ICACHE_LOCAL != 0 {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

fn sys_riscv_flush_icache(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    validate_flush_icache_flags(ctx.args[2])?;

    // Linux 保留并忽略 start/end；RISC-V 当前按全局 I-cache 同步实现。
    let _start = ctx.args[0];
    let _end = ctx.args[1];
    <crate::riscv64::Riscv64TaskOps as TaskOps>::sync_icache();
    Ok(0)
}

pub fn register_linux_extensions(register: fn(usize, SyscallFn)) {
    register(
        nr::SYS_RISCV_FLUSH_ICACHE.expect("RISC-V flush_icache 编号缺失"),
        sys_riscv_flush_icache,
    );
}

pub const USER_BREAK_HOOK_REGISTRAR: Option<fn(fn(usize) -> bool)> =
    Some(crate::riscv64::trap::exception::register_user_break_hook);

#[cfg(test)]
mod tests {
    use super::validate_flush_icache_flags;

    #[test]
    fn validates_linux_flush_icache_flags() {
        assert!(validate_flush_icache_flags(0).is_ok());
        assert!(validate_flush_icache_flags(1).is_ok());
        assert!(validate_flush_icache_flags(2).is_err());
        assert!(validate_flush_icache_flags(usize::MAX).is_err());
    }
}
