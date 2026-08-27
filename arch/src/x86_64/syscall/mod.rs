//! x86_64 Linux syscall ABI。

pub mod nr;

/// x86_64 当前没有独立于主 syscall 表的架构私有注册项。
pub fn register_linux_extensions(_: fn(usize, general::syscall::SyscallFn)) {}

/// x86_64 trap 后端接入后由其提供断点钩子注册器。
pub const USER_BREAK_HOOK_REGISTRAR: Option<fn(fn(usize) -> bool)> = None;
