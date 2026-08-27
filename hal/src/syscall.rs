//! 当前目标架构的 Linux syscall ABI 选择层。
//!
//! 具体编号和架构私有实现均位于 arch；HAL 只做条件重导出。

#[cfg(target_arch = "loongarch64")]
pub use arch::loongarch64::syscall::{USER_BREAK_HOOK_REGISTRAR, nr, register_linux_extensions};

#[cfg(target_arch = "riscv64")]
pub use arch::riscv64::syscall::{USER_BREAK_HOOK_REGISTRAR, nr, register_linux_extensions};

#[cfg(target_arch = "x86_64")]
pub use arch::x86_64::syscall::{USER_BREAK_HOOK_REGISTRAR, nr, register_linux_extensions};
