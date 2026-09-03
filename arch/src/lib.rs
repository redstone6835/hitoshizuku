//! # Arch 层
//! Arch 层为在 general 中定义的各种接口提供平台特定实现，并提供一
//! 些平台相关的基本操作接口。

#![no_std]
extern crate alloc;

// LoongArch 直启在堆初始化前使用的零分配 16550 DT 配置解析。
#[cfg(any(test, target_arch = "loongarch64"))]
#[path = "loongarch64/early_console_config.rs"]
pub(crate) mod early_console_config;

// LoongArch U-Boot 直启协议决策；host 单测可直接覆盖纯逻辑部分。
#[cfg(all(any(test, target_arch = "loongarch64"), not(target_arch = "x86_64")))]
pub(crate) mod boot_protocol;

// x86_64 的完整后端只在目标为 x86_64 时编译；这样其它 ISA 的交叉构建不会
// 解析 x86 专用内联汇编。x86_64 host 单测仍会自然进入该分支。
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;
#[cfg(target_arch = "riscv64")]
pub type CurrentTaskOps = riscv64::Riscv64TaskOps;

#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
#[cfg(target_arch = "x86_64")]
pub type CurrentTaskOps = x86_64::X86_64TaskOps;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;
#[cfg(target_arch = "loongarch64")]
pub type CurrentTaskOps = loongarch64::LoongArch64TaskOps;

/// 清零 BSS 段，确保未初始化的静态变量被正确设置为 0。
#[cfg(target_os = "none")]
pub(crate) unsafe fn clear_bss() {
    unsafe extern "C" {
        fn sbss();
        fn ebss();
    }
    clear_bss_for_loop(sbss as *const () as usize, ebss as *const () as usize);
}

/// 使用循环的方式清零 BSS 段，适用于 BSS 较大的情况，避免栈溢出。
#[cfg(target_os = "none")]
pub(crate) fn clear_bss_for_loop(begin: usize, end: usize) {
    unsafe {
        core::ptr::write_bytes(begin as *mut u8, 0, end - begin);
    }
}
