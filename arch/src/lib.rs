//! # Arch 层
//! Arch 层为在 general 中定义的各种接口提供平台特定实现，并提供一
//! 些平台相关的基本操作接口。

#![no_std]
extern crate alloc;

// 两个架构共享零分配的 16550 DT 配置解析；源码暂保留在原 LoongArch 路径，
// 避免启动期解析语义分叉。
#[cfg(any(test, target_arch = "riscv64", target_arch = "loongarch64"))]
#[path = "loongarch64/early_console_config.rs"]
pub(crate) mod early_console_config;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;
#[cfg(target_arch = "riscv64")]
pub type CurrentTaskOps = riscv64::Riscv64TaskOps;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;
#[cfg(target_arch = "loongarch64")]
pub type CurrentTaskOps = loongarch64::LoongArch64TaskOps;

/// 清零 BSS 段，确保未初始化的静态变量被正确设置为 0。
pub(crate) unsafe fn clear_bss() {
    unsafe extern "C" {
        fn sbss();
        fn ebss();
    }
    clear_bss_for_loop(sbss as *const () as usize, ebss as *const () as usize);
}

/// 使用循环的方式清零 BSS 段，适用于 BSS 较大的情况，避免栈溢出。
pub(crate) fn clear_bss_for_loop(begin: usize, end: usize) {
    unsafe {
        core::ptr::write_bytes(begin as *mut u8, 0, end - begin);
    }
}
