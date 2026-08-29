#![no_std]
#![no_main]

use uefi_loader::{EfiStatus, Handle, SystemTable};

#[unsafe(no_mangle)]
unsafe extern "efiapi" fn efi_main(image: Handle, system_table: *mut SystemTable) -> EfiStatus {
    unsafe { uefi_loader::run(image, system_table) }
}
