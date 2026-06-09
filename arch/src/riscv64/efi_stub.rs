//! RISC-V64 UEFI EFI Stub。
//!
//! RISC-V 目标仅支持 QEMU 直启（DTB）模式。若固件以 UEFI 方式加载本镜像，
//! 入口将输出提示信息后 panic 退出。

use core::arch::naked_asm;

/// UEFI PE 入口跳板——RISC-V 不支持 UEFI 启动，触发 panic。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.efi.entry")]
pub unsafe extern "C" fn efi_pe_entry_trampoline(
    _image_handle: usize,
    _system_table: usize,
) {
    naked_asm!(
        "la a0, {msg}",
        "j {panic_fn}",
        msg = sym EFI_UNSUPPORTED_MSG,
        panic_fn = sym efi_unsupported_panic,
    );
}

#[unsafe(link_section = ".rodata")]
static EFI_UNSUPPORTED_MSG: &str = "暂不支持 UEFI 启动，请使用 QEMU 直启（DTB）模式。";

fn efi_unsupported_panic(_msg: &str) -> ! {
    panic!("暂不支持 UEFI 启动，请使用 QEMU 直启（DTB）模式。");
}

/// 内存映射快照——RISC-V 不走 EFI，始终返回 None。
pub(crate) fn memory_map_snapshot() -> Option<()> {
    None
}
