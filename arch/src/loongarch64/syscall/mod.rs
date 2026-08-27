//! LoongArch64 syscall 注入与 Linux ABI 编号。
mod frame_ops;
pub mod nr;

/// 由 `arch::loongarch64::sched_ctx::register` 启动期调用一次。
pub fn register() {
    general::syscall::register_frame_ops(&frame_ops::SYSCALL_FRAME_OPS);
}

/// LoongArch64 当前没有独立于通用表的架构私有 Linux syscall。
pub fn register_linux_extensions(_: fn(usize, general::syscall::SyscallFn)) {}

pub const USER_BREAK_HOOK_REGISTRAR: Option<fn(fn(usize) -> bool)> =
    Some(crate::loongarch64::trap::exception::register_user_break_hook);
