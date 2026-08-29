//! x86_64 的 Multiboot2/EFI 启动入口。
//!
//! Multiboot2 入口由 bootloader 以 32-bit protected mode（EAX=magic、
//! EBX=information pointer）调用。本文件建立一组很小的临时页表：低端
//! identity 映射供 bootloader 结构读取，`DIRECT_MAP_BASE` 映射供 ACPI/内存
//! 代码使用，`KERNEL_VA_OFFSET` 映射供链接后的内核代码执行。早期页表覆盖
//! 低端 4 GiB，以包含 QEMU/Q35 常见的 HPET、PCI MMIO 和 ACPI 固件区域；
//! 正式 x86 页表接管后再按内存图建立完整映射。
//!
//! UEFI 的 PE/COFF 包装和高半区重定位仍属于外部镜像适配。这里导出
//! ABI 正确、返回 EFI 状态的入口符号；入口会先完成 checked preflight，
//! 在当前 ELF 尚未具备 PE/COFF 重定位能力时明确返回 `EFI_UNSUPPORTED`，
//! 不把一个不完整的 EFI 路径伪装成成功启动或把固件停在半交接状态。

#![allow(clippy::missing_safety_doc)]

#[cfg(target_os = "none")]
use core::arch::global_asm;
#[cfg(target_os = "none")]
use core::ptr::{addr_of, addr_of_mut};
#[cfg(target_os = "none")]
use core::slice;

#[cfg(target_os = "none")]
use general::firmware::FirmwareTableMapping;
#[cfg(target_os = "none")]
use general::{
    StartAcpiHostOps, StartAcpiIoOps, StartAcpiTables, StartAddressOps, StartAllocatorOps,
    StartBootInfo, StartContext, StartFirmware, StartMemory, StartMemoryMap, StartMemoryRegion,
    StartMemoryRegionKind, StartNoMapSupport, StartPhysRange,
};

#[cfg(target_os = "none")]
use super::boot_protocol::{BootProtocolError, MULTIBOOT2_BOOTLOADER_MAGIC, Multiboot2Info};
#[cfg(target_os = "none")]
use super::early_console;
#[cfg(target_os = "none")]
use super::efi_stub::{self, EfiHandoffError};
#[cfg(target_os = "none")]
use super::io;
#[cfg(target_os = "none")]
use super::mm::heap_vm;
#[cfg(target_os = "none")]
use super::specific::{
    DIRECT_MAP_BASE, current_cpu_id, phys_to_virt, set_direct_map_base, virt_to_phys,
};
#[cfg(target_os = "none")]
use crate::clear_bss;

/// The temporary page-table window intentionally has a hard, explicit bound.
/// Four GiB covers the conventional ACPI/PCI MMIO aperture while keeping all
/// bootstrap physical addresses representable by the 32-bit Multiboot entry.
const EARLY_MAP_LIMIT: usize = 0x1_0000_0000;
const EARLY_PD_ENTRIES: usize = EARLY_MAP_LIMIT / (2 * 1024 * 1024);
const EARLY_PAGE_TABLE_BYTES: usize = 0x8000;
const MAX_MULTIBOOT_INFO: usize = 1024 * 1024;
const MAX_CMDLINE: usize = 4096;
const MAX_MEMORY_REGIONS: usize = 256;
const RSDP_V1_LEN: usize = 20;
const RSDP_V2_LEN: usize = 36;
// ACPI 2.0 currently defines a 36-byte RSDP, but retaining a bounded tail
// keeps this loader forward-compatible with firmware extensions without ever
// accepting an unbounded length from a boot tag/configuration table.
const RSDP_COPY_LEN: usize = 4096;
const EFI_MEMORY_MAP_BYTES: usize = 512 * 1024;
const MAX_EFI_MEMORY_REGIONS: usize = 512;
/// Maximum number of immutable ACPI objects retained by the early handoff.
/// This covers the root table, all entries in a normal XSDT, and FADT-linked
/// DSDT/FACS objects without allocating before the kernel heap exists.
const MAX_ACPI_MAPPINGS: usize = 256;

const _: () = assert!(EARLY_MAP_LIMIT == 2 * 1024 * 1024 * EARLY_PD_ENTRIES);
const _: () = assert!(EARLY_PAGE_TABLE_BYTES == 8 * 4096);

#[cfg(target_os = "none")]
fn early_device_mmio_to_virt(physical: usize) -> usize {
    // The bootstrap direct map is deliberately limited to four GiB.  Returning
    // zero makes ACPI/APIC/PCI consumers reject an unmapped high MMIO aperture
    // instead of manufacturing a canonical virtual address that will fault on
    // its first volatile access.
    if physical == 0 || physical >= EARLY_MAP_LIMIT {
        0
    } else {
        phys_to_virt(physical)
    }
}

/// Four-level tables used before the normal x86 MM implementation is installed.
///
/// The three PDPTs have different virtual aliases.  The identity and direct
/// aliases cover four GiB with four page-directory pages; the kernel alias
/// reuses the first two because the linked image is below the first 2 GiB.
/// Every leaf is a 2 MiB RWX mapping during this short transition; the regular
/// MM backend is responsible for enforcing final W^X permissions.
#[cfg(target_os = "none")]
#[repr(C, align(4096))]
struct EarlyPageTables {
    pml4: [u64; 512],
    identity_pdpt: [u64; 512],
    direct_pdpt: [u64; 512],
    kernel_pdpt: [u64; 512],
    pd: [[u64; 512]; 4],
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data.prepage")]
static mut EARLY_PAGE_TABLES: EarlyPageTables = EarlyPageTables {
    pml4: [0; 512],
    identity_pdpt: [0; 512],
    direct_pdpt: [0; 512],
    kernel_pdpt: [0; 512],
    pd: [[0; 512]; 4],
};

// These snapshots are written before paging is enabled, hence they have to be
// addressable through a low physical alias and must not be cleared with BSS.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data.prepage")]
static mut __x86_boot_magic: u32 = 0;

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data.prepage")]
static mut __x86_boot_info: u32 = 0;

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data.prepage")]
static mut MB2_REGIONS: [StartMemoryRegion; MAX_MEMORY_REGIONS] = [StartMemoryRegion::new(
    StartPhysRange::new(0, 1),
    StartMemoryRegionKind::Reserved,
    0,
); MAX_MEMORY_REGIONS];

#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut MB2_CMDLINE: [u8; MAX_CMDLINE] = [0; MAX_CMDLINE];

#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut MB2_CMDLINE_LEN: usize = 0;

#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut RSDP_COPY: [u8; RSDP_COPY_LEN] = [0; RSDP_COPY_LEN];

#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut RSDP_COPY_LEN_USED: usize = 0;

#[cfg(target_os = "none")]
#[repr(C, align(8))]
struct EfiMemoryMapStorage([u8; EFI_MEMORY_MAP_BYTES]);

#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut EFI_MEMORY_MAP: EfiMemoryMapStorage = EfiMemoryMapStorage([0; EFI_MEMORY_MAP_BYTES]);

#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut EFI_REGIONS: [StartMemoryRegion; MAX_EFI_MEMORY_REGIONS] = [StartMemoryRegion::new(
    StartPhysRange::new(0, 1),
    StartMemoryRegionKind::Reserved,
    0,
); MAX_EFI_MEMORY_REGIONS];

/// The early handoff publishes one mapping per validated ACPI object. Keeping
/// the list object-granular is important: the kernel's platform parser uses
/// the mapping start as the SDT header, while `AcpiMapper` uses the same list
/// to reject accesses outside the immutable handoff view. The bytes are
/// firmware-backed through the early direct map until the normal memory
/// manager takes ownership; no unbounded physical fallback is permitted.
#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut ACPI_MAPPINGS: [FirmwareTableMapping; MAX_ACPI_MAPPINGS] =
    [FirmwareTableMapping::EMPTY; MAX_ACPI_MAPPINGS];

#[cfg(target_os = "none")]
#[unsafe(link_section = ".data.prepage")]
static mut ACPI_MAPPING_COUNT: usize = 0;

// A Multiboot2 header is kept in the first load segment by the linker. The
// address fields are emitted by assembly so linker-provided physical aliases
// are resolved without forming invalid Rust compile-time pointers.
#[cfg(target_os = "none")]
global_asm!(
    r#"
    .section .multiboot2.header,"a",@progbits
    .balign 8
    .globl __x86_mb2_header
    .type __x86_mb2_header,@object
__x86_mb2_header:
    .long 0xe85250d6                 /* magic */
    .long 0                           /* architecture = i386 */
    .long 40                          /* header length */
    .long 0x17adaf02                  /* -(magic + architecture + length) */
    /* Do not emit the optional address tag.  Its presence makes GRUB's
       multiboot2 loader flatten the image at load_addr + file_offset,
       bypassing the ELF PT_LOAD p_paddr values used by the higher-half
       linker script.  The ELF program headers already describe every
       physical load address; retain only the explicit entry tag. */
    .long 3                           /* entry-address tag */
    .long 12
    .long __x86_start_phys
    .balign 8                         /* tag sizes exclude padding */
    .long 0                           /* end tag */
    .long 8
    .size __x86_mb2_header, .-__x86_mb2_header

    .section .data.prepage,"aw",@progbits
    .balign 8
    .globl __x86_gdt
    .type __x86_gdt,@object
__x86_gdt:
    .quad 0x0000000000000000
    .quad 0x00af9a000000ffff              /* bootstrap code (0x08) */
    .quad 0x00af9a000000ffff              /* kernel code (0x10) */
    .quad 0x00cf92000000ffff              /* kernel data (0x18) */
    .quad 0x0000000000000000              /* reserved (0x20) */
    .quad 0x00cff2000000ffff              /* user data (0x2b, RPL=3) */
    .quad 0x00affa000000ffff              /* user code (0x33, RPL=3) */
    .quad 0x0000000000000000              /* reserved (0x38) */
    .quad 0x0000000000000000              /* TSS low (0x40), filled later */
    .quad 0x0000000000000000              /* TSS high (0x48), filled later */
__x86_gdt_end:
    .balign 8
    .globl __x86_gdt_ptr
    .type __x86_gdt_ptr,@object
__x86_gdt_ptr:
    .word __x86_gdt_end - __x86_gdt - 1
    .long __x86_gdt_phys
    .size __x86_gdt_ptr, .-__x86_gdt_ptr

    .section .text.entry.boot,"ax",@progbits
    .code32
    .globl _start
    .type _start,@function
_start:
    cli
    cld
    /* Multiboot2: EAX=magic, EBX=info physical address. */
    movl %eax, %edx
    movl $__x86_boot_magic_phys, %edi
    movl %edx, (%edi)
    movl $__x86_boot_info_phys, %edi
    movl %ebx, (%edi)

    /* Clear the eight 4 KiB pages occupied by the temporary tables. */
    movl $__x86_early_page_tables_phys, %edi
    xorl %eax, %eax
    movl $0x2000, %ecx              /* 0x8000 bytes / sizeof(long) */
    rep stosl

    /* PML4[0] -> identity PDPT, PML4[256] -> direct map, PML4[511] -> kernel. */
    movl $__x86_early_page_tables_phys, %edi
    leal 0x1000(%edi), %eax
    orl $0x3, %eax
    movl %eax, 0(%edi)
    leal 0x2000(%edi), %eax
    orl $0x3, %eax
    movl %eax, 0x800(%edi)
    leal 0x3000(%edi), %eax
    orl $0x3, %eax
    movl %eax, 0xff8(%edi)

    /* identity/direct PDPTs cover four GiB; the kernel alias only needs the
       first two directory pages because the linked image is below 2 GiB. */
    leal 0x4000(%edi), %eax
    orl $0x3, %eax
    movl %eax, 0x1000(%edi)
    movl %eax, 0x2000(%edi)
    movl %eax, 0x3ff0(%edi)
    leal 0x5000(%edi), %eax
    orl $0x3, %eax
    movl %eax, 0x1008(%edi)
    movl %eax, 0x2008(%edi)
    movl %eax, 0x3ff8(%edi)
    leal 0x6000(%edi), %eax
    orl $0x3, %eax
    movl %eax, 0x1010(%edi)
    movl %eax, 0x2010(%edi)
    leal 0x7000(%edi), %eax
    orl $0x3, %eax
    movl %eax, 0x1018(%edi)
    movl %eax, 0x2018(%edi)

    /* 2048 two-MiB leaves map physical 0..4 GiB. */
    leal 0x4000(%edi), %edi
    xorl %ecx, %ecx
1:
    movl %ecx, %eax
    shll $21, %eax
    orl $0x83, %eax                 /* present | writable | huge */
    movl %eax, (%edi)
    addl $8, %edi
    incl %ecx
    cmpl $2048, %ecx
    jne 1b

    /* NX is a required kernel paging capability: the runtime page-table
       backend emits bit 63 for every non-executable leaf.  Fail closed before
       setting EFER.NXE instead of letting the first data mapping fault with a
       reserved-bit violation on an older CPU. */
    movl $0x80000000, %eax
    cpuid
    cmpl $0x80000001, %eax
    jb 2f
    movl $0x80000001, %eax
    cpuid
    btl $20, %edx
    jnc 2f

    /* Load GDT while still in protected mode, then enable PAE and long mode. */
    lgdt (__x86_gdt_ptr_phys)
    movl %cr4, %eax
    orl $0x20, %eax                  /* CR4.PAE */
    movl %eax, %cr4
    movl $__x86_early_page_tables_phys, %eax
    movl %eax, %cr3
    movl $0xc0000080, %ecx           /* IA32_EFER */
    rdmsr
    orl $0x900, %eax                 /* EFER.LME | EFER.NXE */
    wrmsr
    movl %cr0, %eax
    orl $0x80010001, %eax            /* CR0.PG | WP | PE */
    movl %eax, %cr0
    /* Use the same kernel-code selector as the runtime descriptor loader. */
    ljmp $0x10, $__x86_long_mode_phys

    .code64
    .globl __x86_long_mode_low
    .type __x86_long_mode_low,@function
__x86_long_mode_low:
    /* The far jump lands through identity; continue through the high alias. */
    movabs $__x86_long_mode, %rax
    jmp *%rax

    .globl __x86_long_mode
    .type __x86_long_mode,@function
__x86_long_mode:
    movw $0x18, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    xorl %ebp, %ebp
    movabs $__tmp_stack_top, %rsp
    andq $-16, %rsp
    movabs $__kernel_arch_loader, %rax
    call *%rax
2:
    cli
    hlt
    jmp 2b
    .size _start, .- _start

    "#,
    options(att_syntax)
);

// EFI's Windows x64 entry uses RCX/RDX, while Rust's C ABI uses RDI/RSI on
// this target. Capture and translate the registers before entering the Rust
// status-returning entry. The 32-byte shadow space and return address keep the
// Windows caller's stack contract intact while the Rust callee sees SysV
// alignment.
#[cfg(target_os = "none")]
global_asm!(
    r#"
    .section .text.efi.entry,"ax",@progbits
    .globl efi_pe_entry_trampoline
    .type efi_pe_entry_trampoline,@function
efi_pe_entry_trampoline:
    movq %rcx, %rdi
    movq %rdx, %rsi
    subq $40, %rsp                  /* shadow space + SysV call alignment */
    movabs $efi_entry, %rax
    call *%rax
    addq $40, %rsp
    ret
    .size efi_pe_entry_trampoline, .-efi_pe_entry_trampoline
    "#,
    options(att_syntax)
);

#[cfg(target_os = "none")]
#[inline(never)]
fn halt_with_code(code: u8, detail: &'static str) -> ! {
    early_console::write_bytes(b"\n[x86 boot halted] ");
    early_console::write_bytes(detail.as_bytes());
    early_console::write_bytes(b" code=");
    early_console::write_hex16(code as usize);
    early_console::write_bytes(b"\n");
    unsafe {
        core::arch::asm!("cli", "2:", "hlt", "jmp 2b", options(noreturn, nostack));
    }
}

#[cfg(target_os = "none")]
fn halt_protocol(error: BootProtocolError) -> ! {
    let code = match error {
        BootProtocolError::Truncated(_) => 1,
        BootProtocolError::Invalid(_) => 2,
        BootProtocolError::Overflow(_) => 3,
        BootProtocolError::Unsupported(_) => 4,
    };
    halt_with_code(code, "invalid Multiboot2 handoff")
}

#[cfg(target_os = "none")]
fn copy_cmdline(info: Multiboot2Info<'_>) -> Result<Option<&'static [u8]>, BootProtocolError> {
    let Some(command_line) = info.command_line() else {
        unsafe { MB2_CMDLINE_LEN = 0 };
        return Ok(None);
    };
    if command_line.len() >= MAX_CMDLINE {
        return Err(BootProtocolError::Unsupported(
            "Multiboot2 command line too long",
        ));
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            command_line.as_ptr(),
            addr_of_mut!(MB2_CMDLINE).cast::<u8>(),
            command_line.len(),
        );
        MB2_CMDLINE[command_line.len()] = 0;
        MB2_CMDLINE_LEN = command_line.len();
        Ok(Some(core::slice::from_raw_parts(
            addr_of!(MB2_CMDLINE).cast::<u8>(),
            MB2_CMDLINE_LEN,
        )))
    }
}

#[cfg(target_os = "none")]
fn checksum_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

#[cfg(target_os = "none")]
fn copy_rsdp(info: Multiboot2Info<'_>) -> Result<usize, BootProtocolError> {
    let bytes = info.acpi_rsdp().ok_or(BootProtocolError::Invalid(
        "Multiboot2 ACPI RSDP tag missing",
    ))?;
    copy_rsdp_bytes(bytes)
}

#[cfg(target_os = "none")]
fn copy_rsdp_bytes(bytes: &[u8]) -> Result<usize, BootProtocolError> {
    if bytes.len() < RSDP_V1_LEN || bytes.get(..8) != Some(b"RSD PTR ") {
        return Err(BootProtocolError::Invalid("Multiboot2 ACPI RSDP signature"));
    }
    if !checksum_valid(&bytes[..RSDP_V1_LEN]) {
        return Err(BootProtocolError::Invalid("Multiboot2 ACPI RSDP checksum"));
    }
    // The revision byte is part of the 20-byte ACPI 1.0 structure.  Do not
    // infer an extended RSDP merely from the amount of padding supplied by a
    // boot tag; old firmware is commonly followed by unrelated bytes.
    let revision = bytes[15];
    if revision == 1 {
        return Err(BootProtocolError::Unsupported(
            "reserved ACPI RSDP revision",
        ));
    }
    let copy_len = if revision == 0 {
        RSDP_V1_LEN
    } else {
        if bytes.len() < RSDP_V2_LEN {
            return Err(BootProtocolError::Truncated("extended ACPI RSDP header"));
        }
        let length = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
        if !(RSDP_V2_LEN..=RSDP_COPY_LEN).contains(&length) || length > bytes.len() {
            return Err(BootProtocolError::Invalid("Multiboot2 ACPI RSDP length"));
        }
        if !checksum_valid(&bytes[..length]) {
            return Err(BootProtocolError::Invalid(
                "Multiboot2 ACPI extended checksum",
            ));
        }
        length
    };
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr_of_mut!(RSDP_COPY).cast(), copy_len);
        // A v1 copy is exposed through the same fixed mapping, with the tail
        // zeroed so the ACPI crate never observes stale extended fields.
        if copy_len < RSDP_COPY_LEN {
            core::ptr::write_bytes(
                addr_of_mut!(RSDP_COPY).cast::<u8>().add(copy_len),
                0,
                RSDP_COPY_LEN - copy_len,
            );
        }
        RSDP_COPY_LEN_USED = copy_len;
    }
    Ok(copy_len)
}

/// Copy an EFI configuration-table RSDP only after checking the address and
/// the self-described length. The early page tables expose the conventional
/// low 4-GiB firmware/MMIO window; a table outside it is reported as an
/// unsupported handoff instead of being dereferenced through a guessed alias.
#[cfg(target_os = "none")]
fn copy_rsdp_pointer(pointer: usize) -> Result<usize, BootProtocolError> {
    if pointer == 0 || pointer & 7 != 0 {
        return Err(BootProtocolError::Invalid("EFI ACPI RSDP pointer"));
    }
    let v1_end = pointer
        .checked_add(RSDP_V1_LEN)
        .ok_or(BootProtocolError::Overflow("EFI ACPI RSDP address"))?;
    if v1_end > EARLY_MAP_LIMIT {
        return Err(BootProtocolError::Unsupported(
            "EFI ACPI RSDP outside early physical map",
        ));
    }
    // Read and validate the fixed ACPI 1.0 portion first.  This makes a
    // revision-0 RSDP safe even when the final bytes of the physical page are
    // not mapped as an extended structure.
    let v1 = unsafe { core::slice::from_raw_parts(pointer as *const u8, RSDP_V1_LEN) };
    if v1.get(..8) != Some(b"RSD PTR ") {
        return Err(BootProtocolError::Invalid("EFI ACPI RSDP signature"));
    }
    if !checksum_valid(v1) {
        return Err(BootProtocolError::Invalid("EFI ACPI RSDP checksum"));
    }
    let revision = v1[15];
    if revision == 0 {
        return copy_rsdp_bytes(v1);
    }
    if revision == 1 {
        return Err(BootProtocolError::Unsupported(
            "reserved ACPI RSDP revision",
        ));
    }
    let v2_end = pointer
        .checked_add(RSDP_V2_LEN)
        .ok_or(BootProtocolError::Overflow("EFI ACPI RSDP address"))?;
    if v2_end > EARLY_MAP_LIMIT {
        return Err(BootProtocolError::Unsupported(
            "EFI ACPI RSDP extended header outside early map",
        ));
    }
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, RSDP_V2_LEN) };
    let length = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
    if !(RSDP_V2_LEN..=RSDP_COPY_LEN).contains(&length) {
        return Err(BootProtocolError::Invalid("EFI ACPI RSDP length"));
    }
    let end = pointer
        .checked_add(length)
        .ok_or(BootProtocolError::Overflow("EFI ACPI RSDP length"))?;
    if length > RSDP_COPY_LEN || end > EARLY_MAP_LIMIT {
        return Err(BootProtocolError::Unsupported(
            "EFI ACPI RSDP exceeds early snapshot window",
        ));
    }
    let full = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    copy_rsdp_bytes(full)
}

#[cfg(target_os = "none")]
fn acpi_read_physical(physical_address: usize, size: usize) -> &'static [u8] {
    let Some(end) = physical_address.checked_add(size) else {
        return &[];
    };
    if size == 0 || end > EARLY_MAP_LIMIT {
        return &[];
    }
    let virtual_address = phys_to_virt(physical_address);
    if virtual_address.checked_add(size).is_none() {
        return &[];
    }
    // SAFETY: the early page tables establish a read/write direct alias for
    // every physical address below EARLY_MAP_LIMIT. The caller receives only
    // a bounded immutable view and all firmware lengths are checked before it
    // is used by a parser.
    unsafe { slice::from_raw_parts(virtual_address as *const u8, size) }
}

#[cfg(target_os = "none")]
fn add_acpi_mapping(physical_start: usize, length: usize) -> Result<(), &'static str> {
    let physical_end = physical_start
        .checked_add(length)
        .ok_or("[loader][acpi] table mapping address overflow")?;
    if physical_start == 0 || length == 0 || physical_end > EARLY_MAP_LIMIT {
        return Err("[loader][acpi] ACPI object lies outside the early map");
    }
    let virtual_start = phys_to_virt(physical_start);
    virtual_start
        .checked_add(length)
        .ok_or("[loader][acpi] table virtual mapping address overflow")?;

    unsafe {
        let count = ACPI_MAPPING_COUNT;
        let mappings = addr_of_mut!(ACPI_MAPPINGS).cast::<FirmwareTableMapping>();
        for index in 0..count {
            let existing = *mappings.add(index);
            if existing.physical_start == physical_start && existing.length == length {
                return Ok(());
            }
            let existing_end = existing
                .physical_start
                .checked_add(existing.length)
                .ok_or("[loader][acpi] existing mapping overflow")?;
            if physical_start < existing_end && existing.physical_start < physical_end {
                return Err("[loader][acpi] overlapping ACPI table mappings");
            }
        }
        if count >= MAX_ACPI_MAPPINGS {
            return Err("[loader][acpi] too many ACPI table mappings");
        }
        *mappings.add(count) = FirmwareTableMapping {
            physical_start,
            virtual_start,
            length,
        };
        ACPI_MAPPING_COUNT = count + 1;
    }
    Ok(())
}

#[cfg(target_os = "none")]
fn add_acpi_object(physical_address: usize) -> Result<(), &'static str> {
    let signature = acpi_read_physical(physical_address, 4);
    if signature == b"FACS" {
        let length = general::firmware::acpi::facs_length(physical_address, acpi_read_physical)?;
        return add_acpi_mapping(physical_address, length);
    }

    let length = general::firmware::acpi::table_length(physical_address, acpi_read_physical)?;
    let table = acpi_read_physical(physical_address, length);
    if table.len() != length {
        return Err("[loader][acpi] ACPI table is outside the early map");
    }
    general::firmware::acpi::validate_sdt(table)?;
    add_acpi_mapping(physical_address, length)?;

    // DSDT and FACS are not entries in the XSDT/RSDT, but AML and power
    // management consume them through the FADT. Add them to the same checked
    // mapping set before handing control to the kernel.
    if signature == b"FACP" {
        if let Some(closure) = general::firmware::acpi::fadt_closure(table)? {
            if let Some(dsdt) = closure.dsdt_phys {
                add_acpi_object(dsdt)?;
            }
            if let Some(facs) = closure.facs_phys {
                add_acpi_object(facs)?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "none")]
fn build_acpi_mappings(rsdp_phys: usize, rsdp_length: usize) -> Result<usize, &'static str> {
    unsafe {
        ACPI_MAPPING_COUNT = 0;
    }

    // `acpi::AcpiTables::from_rsdp` maps the fixed 36-byte Rust view even for
    // an ACPI 1.0 RSDP.  The tail of RSDP_COPY is zero-filled, so register
    // that compatibility view while retaining the real source length in
    // StartAcpiTables for validation and consumers.
    add_acpi_mapping(rsdp_phys, rsdp_length.max(RSDP_V2_LEN))?;

    let root = general::firmware::acpi::snapshot_root_info(rsdp_phys, acpi_read_physical)?;
    let root_length = general::firmware::acpi::table_length(root.root_phys, acpi_read_physical)?;
    let root_bytes = acpi_read_physical(root.root_phys, root_length);
    if root_bytes.len() != root_length {
        return Err("[loader][acpi] ACPI root table is outside the early map");
    }
    general::firmware::acpi::validate_root_table(root_bytes, root.root_kind)?;
    add_acpi_mapping(root.root_phys, root_length)?;
    general::firmware::acpi::for_each_root_table_entry(
        root_bytes,
        root.root_kind,
        add_acpi_object,
    )?;

    unsafe { Ok(ACPI_MAPPING_COUNT) }
}

#[cfg(target_os = "none")]
fn clip_memory_map(count: usize) -> usize {
    let mut write = 0;
    unsafe {
        for index in 0..count {
            let mut region = MB2_REGIONS[index];
            if region.range.start >= EARLY_MAP_LIMIT {
                continue;
            }
            region.range.end = region.range.end.min(EARLY_MAP_LIMIT);
            if region.range.end <= region.range.start {
                continue;
            }
            MB2_REGIONS[write] = region;
            write += 1;
        }
    }
    write
}

#[cfg(target_os = "none")]
fn init_boot_allocator() {
    unsafe extern "C" {
        fn sheap();
        fn eheap();
    }
    let heap_start = sheap as *const () as usize;
    let heap_end = eheap as *const () as usize;
    if heap_end <= heap_start {
        halt_with_code(5, "invalid boot heap");
    }
    allocator::KERNEL_ALLOCATOR.bind_address_translation(phys_to_virt, virt_to_phys);
    allocator::KERNEL_ALLOCATOR.bind_cpu_id(current_cpu_id);
    allocator::KERNEL_ALLOCATOR.init_boot(heap_start, heap_end - heap_start);
}

#[cfg(target_os = "none")]
fn acpi_read_u8(port: u16) -> u8 {
    unsafe { io::inb(port) }
}

#[cfg(target_os = "none")]
fn acpi_read_u16(port: u16) -> u16 {
    unsafe { io::inw(port) }
}

#[cfg(target_os = "none")]
fn acpi_read_u32(port: u16) -> u32 {
    unsafe { io::inl(port) }
}

#[cfg(target_os = "none")]
fn acpi_write_u8(port: u16, value: u8) {
    unsafe { io::outb(port, value) }
}

#[cfg(target_os = "none")]
fn acpi_write_u16(port: u16, value: u16) {
    unsafe { io::outw(port, value) }
}

#[cfg(target_os = "none")]
fn acpi_write_u32(port: u16, value: u32) {
    unsafe { io::outl(port, value) }
}

#[cfg(target_os = "none")]
const ACPI_IO_OPS: StartAcpiIoOps = StartAcpiIoOps {
    read_u8: acpi_read_u8,
    read_u16: acpi_read_u16,
    read_u32: acpi_read_u32,
    write_u8: acpi_write_u8,
    write_u16: acpi_write_u16,
    write_u32: acpi_write_u32,
};

#[cfg(target_os = "none")]
fn map_kernel_heap(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: allocator::PagePolicy,
) -> bool {
    heap_vm::map_kernel_heap_range(vaddr, paddr, size, page_policy).is_ok()
}

#[cfg(target_os = "none")]
fn unmap_kernel_heap(vaddr: usize, size: usize) -> bool {
    heap_vm::unmap_kernel_heap_range(vaddr, size).is_ok()
}

#[cfg(target_os = "none")]
fn protect_kernel_heap(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool {
    heap_vm::protect_kernel_heap_range(vaddr, size, read, write, execute).is_ok()
}

#[cfg(target_os = "none")]
fn validate_kernel_heap(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool {
    heap_vm::validate_kernel_heap_range(vaddr, size, read, write, execute).is_ok()
}

#[cfg(target_os = "none")]
fn sync_icache() {
    crate::x86_64::sync_icache();
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[inline(never)]
unsafe extern "C" fn __kernel_arch_loader() -> ! {
    // The page-table transition itself is complete before this function runs;
    // now install the direct-map address policy and clear only ordinary BSS.
    set_direct_map_base(DIRECT_MAP_BASE);
    unsafe { clear_bss() };
    early_console::set_port(0x3f8);

    let magic = unsafe { core::ptr::read_volatile(addr_of!(__x86_boot_magic)) };
    if magic != MULTIBOOT2_BOOTLOADER_MAGIC {
        halt_with_code(6, "Multiboot2 magic mismatch");
    }
    let info_phys = unsafe { core::ptr::read_volatile(addr_of!(__x86_boot_info)) as usize };
    if info_phys == 0 || info_phys >= EARLY_MAP_LIMIT || info_phys & 7 != 0 {
        halt_with_code(7, "Multiboot2 information pointer outside identity map");
    }
    let total_size = unsafe { core::ptr::read_volatile(info_phys as *const u32) as usize };
    if !(16..=MAX_MULTIBOOT_INFO).contains(&total_size)
        || info_phys
            .checked_add(total_size)
            .is_none_or(|end| end > EARLY_MAP_LIMIT)
    {
        halt_with_code(8, "Multiboot2 information size outside identity map");
    }

    let info_bytes = unsafe { core::slice::from_raw_parts(info_phys as *const u8, total_size) };
    let info = match Multiboot2Info::parse(info_bytes) {
        Ok(info) => info,
        Err(error) => halt_protocol(error),
    };
    let command_line = match copy_cmdline(info) {
        Ok(command_line) => command_line,
        Err(error) => halt_protocol(error),
    };
    if let Err(error) = copy_rsdp(info) {
        halt_protocol(error);
    }
    let region_count = match info.memory_regions_into(unsafe {
        core::slice::from_raw_parts_mut(addr_of_mut!(MB2_REGIONS).cast(), MAX_MEMORY_REGIONS)
    }) {
        Ok(count) => clip_memory_map(count),
        Err(error) => halt_protocol(error),
    };
    let low_regions: &[StartMemoryRegion] =
        unsafe { core::slice::from_raw_parts(addr_of!(MB2_REGIONS).cast(), region_count) };
    if region_count == 0
        || !low_regions
            .iter()
            .any(|region| region.kind.is_usable_after_handoff())
    {
        halt_with_code(9, "Multiboot2 map has no usable low memory");
    }

    init_boot_allocator();

    unsafe extern "C" {
        fn skernel();
        fn ekernel();
    }
    let kernel_image = StartPhysRange::new(
        virt_to_phys(skernel as *const () as usize),
        virt_to_phys(ekernel as *const () as usize),
    );
    if kernel_image.end <= kernel_image.start || kernel_image.end > EARLY_MAP_LIMIT {
        halt_with_code(10, "kernel image outside early map");
    }

    let rsdp_phys = virt_to_phys(addr_of!(RSDP_COPY) as usize);
    let rsdp_length = unsafe { RSDP_COPY_LEN_USED };
    let acpi_mapping_count = match build_acpi_mappings(rsdp_phys, rsdp_length) {
        Ok(count) => count,
        Err(error) => halt_with_code(12, error),
    };
    let acpi_mappings: &'static [FirmwareTableMapping] =
        unsafe { slice::from_raw_parts(addr_of!(ACPI_MAPPINGS).cast(), acpi_mapping_count) };
    let command_line = command_line;
    let context = StartContext {
        boot: StartBootInfo {
            architecture: general::ArchitectureId::X86_64,
            protocol: general::StartBootProtocol::Multiboot2,
            boot_cpu_id: current_cpu_id(),
            command_line,
        },
        firmware: StartFirmware::Acpi(StartAcpiTables {
            rsdp_phys,
            rsdp_length,
            mappings: acpi_mappings,
            host_ops: StartAcpiHostOps {
                io: Some(ACPI_IO_OPS),
                pci: None,
            },
        }),
        memory: StartMemory {
            kernel_image,
            boot_map: StartMemoryMap::Regions(low_regions),
        },
        address: StartAddressOps {
            phys_to_virt,
            virt_to_phys,
            device_mmio_to_virt: early_device_mmio_to_virt,
        },
        allocator: Some(StartAllocatorOps {
            kernel_heap_region: heap_vm::kernel_heap_region,
            tracked_heap_region: heap_vm::tracked_heap_region,
            map_kernel_heap_range: map_kernel_heap,
            unmap_kernel_heap_range: unmap_kernel_heap,
            protect_kernel_heap_range: protect_kernel_heap,
            validate_kernel_heap_range: validate_kernel_heap,
            sync_icache,
            init_kernel_page_table: heap_vm::init_kernel_page_table,
            no_map: StartNoMapSupport::ReservedOnly {
                granule: allocator::PAGE_SIZE,
                mechanism: "x86_64 early direct-map aliases remain fixed until a full sparse direct map is installed",
            },
        }),
    };
    if let Err(error) = context.validate() {
        halt_with_code(11, error);
    }

    unsafe extern "C" {
        fn __kernel_start_init(context: *const core::ffi::c_void) -> !;
    }
    unsafe { __kernel_start_init(core::ptr::from_ref(&context).cast()) };
}

/// EFI image entry used by a PE/COFF wrapper.
///
/// The current higher-half ELF linker image cannot be entered directly by a
/// firmware PE loader: it still needs a relocation-aware wrapper and a page
/// table transition. We nevertheless validate the real system table and take a
/// real `GetMemoryMap` snapshot here, then return `EFI_UNSUPPORTED` while
/// leaving Boot Services active. A future wrapper can call
/// [`efi_stub::exit_boot_services_checked`] after it owns that transition.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn efi_entry(image_handle: usize, system_table: usize) -> efi::EfiStatus {
    let map = unsafe {
        core::slice::from_raw_parts_mut(
            addr_of_mut!(EFI_MEMORY_MAP.0).cast::<u8>(),
            EFI_MEMORY_MAP_BYTES,
        )
    };
    let snapshot = match unsafe { efi_stub::preflight_memory_map(image_handle, system_table, map) }
    {
        Ok(snapshot) => snapshot,
        Err(error) => return error.status(),
    };

    let rsdp = unsafe { efi::find_acpi_rsdp(snapshot.system_table as *const efi::EfiSystemTable) };
    let Some(rsdp) = rsdp else {
        return efi::status_not_found();
    };
    if let Err(error) = copy_rsdp_pointer(rsdp as usize) {
        return EfiHandoffError::Protocol(error).status();
    }

    let regions = unsafe {
        core::slice::from_raw_parts_mut(addr_of_mut!(EFI_REGIONS).cast(), MAX_EFI_MEMORY_REGIONS)
    };
    let region_count = match snapshot.memory_map.regions_into(regions) {
        Ok(count) => count,
        Err(error) => return EfiHandoffError::Protocol(error).status(),
    };
    if region_count == 0
        || !regions[..region_count]
            .iter()
            .any(|region| region.kind.is_usable_after_handoff())
    {
        return efi::status_unsupported();
    }

    // Do not call ExitBootServices until a PE/COFF wrapper has installed the
    // higher-half page tables and a stable kernel stack. Returning this status
    // keeps firmware ownership coherent and lets that wrapper retry through
    // `exit_boot_services_checked` at the correct point in its sequence.
    efi::status_unsupported()
}

/// Expose constants for link/protocol tests without exposing mutable snapshots.
pub const EARLY_PHYSICAL_MAP_LIMIT: usize = EARLY_MAP_LIMIT;
pub const MULTIBOOT2_HEADER_LENGTH: usize = 40;
pub const MULTIBOOT2_HEADER_CHECKSUM: u32 = 0x17adaf02;
