//! RISC-V64 UEFI EFI Stub。
//!
//! RISC-V 目标仅支持 QEMU 直启（DTB）模式。若固件以 UEFI 方式加载本镜像，
//! 入口将输出提示信息后 panic 退出。

use core::arch::naked_asm;

/// UEFI PE 入口跳板——RISC-V 不支持 UEFI 启动，触发 panic。
///
/// # Safety
///
/// 只能由固件 PE loader 按 EFI 入口 ABI 跳入；它不是可从 Rust 调用的普通函数。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.efi.entry")]
pub unsafe extern "C" fn efi_pe_entry_trampoline(_image_handle: usize, _system_table: usize) {
    naked_asm!(
        "j {panic_fn}",
        panic_fn = sym efi_unsupported_panic,
    );
}

extern "C" fn efi_unsupported_panic() -> ! {
    panic!("暂不支持 UEFI 启动，请使用 QEMU 直启（DTB）模式。");
}
