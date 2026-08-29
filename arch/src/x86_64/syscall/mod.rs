//! x86_64 Linux syscall ABI。

pub mod nr;

mod frame_ops;

/// 注册 x86_64 的 syscall trap-frame 适配器。
pub fn register() {
    general::syscall::register_frame_ops(&frame_ops::SYSCALL_FRAME_OPS);
}

/// x86_64 当前没有独立于主 syscall 表的架构私有注册项。
pub fn register_linux_extensions(_: fn(usize, general::syscall::SyscallFn)) {}

/// x86_64 trap 后端接入后由其提供断点钩子注册器。
pub const USER_BREAK_HOOK_REGISTRAR: Option<fn(fn(usize) -> bool)> =
    Some(crate::x86_64::trap::register_user_break_hook);
