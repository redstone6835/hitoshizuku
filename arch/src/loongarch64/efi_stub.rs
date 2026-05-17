//! LoongArch64 UEFI EFI Stub 引导入口（使用统一 EFI 抽象）。
//!
//! 当内核被构建为 PE/COFF 格式时，UEFI 固件会加载此镜像并调用
//! [`efi_pe_entry`] 作为入口点。该函数负责完成固件到内核的交接：
//!
//! 1. 通过 EFI Boot Services 查找 `EFI_LOADED_IMAGE_PROTOCOL`，
//!    读取 `LoadOptions`（命令行），复制到 `.data` 段中的静态缓冲区；
//! 2. 保存 `image_handle` 等后续退出 Boot Services 必需的信息；
//! 3. 将 EFI 启动信息通过寄存器传递给内核入口 [`_start`]。
//!
//! 跳转到 `_start` 时，寄存器的语义与 QEMU 直启路径完全一致：
//!   - `$a0` = 1（标记 EFI 引导）
//!   - `$a1` = 命令行字符串指针（指向 `.data` 中的静态缓冲区，DMW0 可访问）
//!   - `$a2` = EFI 系统表指针
//!
//! ## 关于 `clear_bss()` 的影响
//!
//! `pre_boot_init` 在执行任何逻辑之前首先调用 `clear_bss()` 清零整个
//! `.bss` 段。因此，从 EFI Stub 传递到内核的任何数据**必须**存放在
//! `.data` 段（而非 `.bss`），否则会在 `clear_bss()` 中被擦除。
//!
//! 这也是为什么 `EFI_CMDLINE_BUF` 和 EFI 快照状态显式放在 `.data` 段中的原因。
//!
//! [`_start`] 的逻辑完全不需要任何修改。

use core::arch::naked_asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::loongarch64::*;
use efi::*;
use log::printk;

// ──────────────────── 命令行缓冲区（.data 段）────────────────────────

/// 存放从 EFI LoadOptions 复制而来的内核命令行字符串。
///
/// 必须放在 `.data` 段以避免被 `pre_boot_init` 中的 `clear_bss()` 清零。
#[unsafe(link_section = ".data")]
static mut EFI_CMDLINE_BUF: [u8; 1024] = [0u8; 1024];

/// EFI 命令行缓冲区的长度。
const EFI_CMDLINE_BUF_LEN: usize = 1024;

// ───────────────────── 内存映射缓冲区 ────────────────────────────────

/// GetMemoryMap 调用使用的保留缓冲区大小。
const MMAP_BUF_SIZE: usize = 128 * 1024;
pub(crate) const MEMORY_MAP_BUFFER_SIZE: usize = MMAP_BUF_SIZE;

#[repr(C, align(8))]
struct EfiAlignedBytes<const N: usize>([u8; N]);

/// 保留 EFI GetMemoryMap 的原始字节，供缺少 DTB 的 ACPI/EFI 路径初始化物理内存。
///
/// 这些状态必须放在 `.data`：`_start` 后的 `pre_boot_init` 会清零 `.bss`。
#[unsafe(link_section = ".data")]
static mut EFI_MMAP_BUF: EfiAlignedBytes<MMAP_BUF_SIZE> = EfiAlignedBytes([0u8; MMAP_BUF_SIZE]);

#[unsafe(link_section = ".data")]
static EFI_MMAP_SIZE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[unsafe(link_section = ".data")]
static EFI_MMAP_DESCRIPTOR_SIZE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[unsafe(link_section = ".data")]
static EFI_MMAP_EXITED_BOOT_SERVICES: AtomicBool = AtomicBool::new(false);

#[unsafe(link_section = ".data")]
static EFI_IMAGE_HANDLE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EfiMemoryMapSource {
    /// 该内存映射是在成功执行 `ExitBootServices` 之后取得的。
    BootServicesExited,
    /// 该内存映射仅用于观察，固件的 Boot Services 仍视为活跃。
    BootServicesActive,
}

#[derive(Clone, Copy)]
pub(crate) struct RawEfiMemoryMapSnapshot {
    pub bytes: &'static [u8],
    pub descriptor_size: usize,
    pub source: EfiMemoryMapSource,
}

pub(crate) fn memory_map_snapshot() -> Option<RawEfiMemoryMapSnapshot> {
    let len = EFI_MMAP_SIZE.load(Ordering::Acquire);
    let descriptor_size = EFI_MMAP_DESCRIPTOR_SIZE.load(Ordering::Acquire);
    if len == 0 || descriptor_size == 0 || len > MMAP_BUF_SIZE {
        return None;
    }
    let bytes = unsafe { core::slice::from_raw_parts((&raw const EFI_MMAP_BUF).cast::<u8>(), len) };
    Some(RawEfiMemoryMapSnapshot {
        bytes,
        descriptor_size,
        source: if EFI_MMAP_EXITED_BOOT_SERVICES.load(Ordering::Acquire) {
            EfiMemoryMapSource::BootServicesExited
        } else {
            EfiMemoryMapSource::BootServicesActive
        },
    })
}

fn clear_memory_map_snapshot() {
    EFI_MMAP_SIZE.store(0, Ordering::Release);
    EFI_MMAP_DESCRIPTOR_SIZE.store(0, Ordering::Release);
    EFI_MMAP_EXITED_BOOT_SERVICES.store(false, Ordering::Release);
}

fn store_memory_map_snapshot(
    memory_map_size: usize,
    descriptor_size: usize,
    source: EfiMemoryMapSource,
) {
    EFI_MMAP_EXITED_BOOT_SERVICES.store(
        matches!(source, EfiMemoryMapSource::BootServicesExited),
        Ordering::Release,
    );
    EFI_MMAP_DESCRIPTOR_SIZE.store(descriptor_size, Ordering::Release);
    EFI_MMAP_SIZE.store(memory_map_size, Ordering::Release);
}

pub(crate) fn snapshot_memory_map(system_table: *mut EfiSystemTable) -> Result<(), EfiStatus> {
    clear_memory_map_snapshot();

    let mmap_buf = unsafe {
        core::slice::from_raw_parts_mut((&raw mut EFI_MMAP_BUF).cast::<u8>(), MMAP_BUF_SIZE)
    };
    let mut map_size = mmap_buf.len();
    let mut map_key = 0usize;
    let mut descriptor_size = 0usize;
    let mut descriptor_version = 0u32;
    let status = unsafe {
        get_memory_map_retry(
            system_table,
            &mut map_size,
            mmap_buf.as_mut_ptr().cast::<EfiMemoryDescriptor>(),
            &mut map_key,
            &mut descriptor_size,
            &mut descriptor_version,
        )
    };
    if !status_is_success(status) {
        if status == status_buffer_too_small() {
            printk!(
                "[efi-stub] GetMemoryMap buffer exhausted: requested={} bytes capacity={} bytes descriptor_size={}",
                map_size,
                MMAP_BUF_SIZE,
                descriptor_size,
            );
        }
        return Err(status);
    }

    let _ = map_key;
    let _ = descriptor_version;
    store_memory_map_snapshot(
        map_size,
        descriptor_size,
        EfiMemoryMapSource::BootServicesActive,
    );
    Ok(())
}

pub(crate) fn exit_boot_services_with_memory_map_snapshot(
    system_table: *mut EfiSystemTable,
) -> Result<(), EfiStatus> {
    clear_memory_map_snapshot();

    let image_handle = EFI_IMAGE_HANDLE.load(Ordering::Acquire) as EfiHandle;
    let mmap_buf = unsafe {
        core::slice::from_raw_parts_mut((&raw mut EFI_MMAP_BUF).cast::<u8>(), MMAP_BUF_SIZE)
    };
    let mut handoff = EfiBootHandoff {
        system_table: core::ptr::null(),
        image_handle: core::ptr::null_mut(),
        cmdline: core::ptr::null(),
        cmdline_size: 0,
        memory_map_size: 0,
        map_key: 0,
        descriptor_size: 0,
        descriptor_version: 0,
    };
    let status = unsafe {
        exit_boot_services_with_memory_map(system_table, image_handle, mmap_buf, &mut handoff)
    };
    if !status_is_success(status) {
        if handoff.memory_map_size != 0 {
            printk!(
                "[efi-stub] ExitBootServices memory-map capture failed: requested={} bytes capacity={} bytes descriptor_size={}",
                handoff.memory_map_size,
                MMAP_BUF_SIZE,
                handoff.descriptor_size,
            );
        }
        return Err(status);
    }

    store_memory_map_snapshot(
        handoff.memory_map_size,
        handoff.descriptor_size,
        EfiMemoryMapSource::BootServicesExited,
    );
    Ok(())
}

// ──────────────────── EFI PE 入口点 ─────────────────────────────────

/// PE/COFF 物理入口跳板。
///
/// UEFI 固件按 PE 的 `ImageBase + AddressOfEntryPoint` 进入镜像，此时 PC 处于
/// 低物理地址；内核主体则按 DMW1 高半区地址链接。这里先建立 DMW1，再跳到真正的
/// Rust EFI stub，使后续全局变量和函数调用都能通过高半区地址访问当前镜像。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.efi.entry")]
pub unsafe extern "C" fn efi_pe_entry_trampoline(
    _image_handle: EfiHandle,
    _system_table: *mut EfiSystemTable,
) {
    naked_asm!(
        // 映射 0x9000_0000_0000_0000 高半区到物理内存，保持 $a0/$a1 不变。
        "ori $r12, $r0, 0x11",
        "lu52i.d $r12, $r12, -1792",
        "csrwr $r12, 0x181",

        "la.abs $r12, {entry}",
        "jirl $zero, $r12, 0",

        entry = sym efi_pe_entry,
    )
}

/// EFI PE/COFF 应用程序入口点。
///
/// 这是 UEFI 固件在加载内核 PE/COFF 镜像后调用的第一个函数。
/// 该函数**不返回到固件**，而是无条件跳转到内核入口 [`_start`]。
///
/// # 调用约定
///
/// UEFI 固件以标准 C 调用约定（LoongArch LP64D ABI）调用此函数：
///   - `$a0` = `image_handle`：当前已加载镜像的 UEFI 句柄
///   - `$a1` = `system_table`：EFI System Table 的指针
#[unsafe(no_mangle)]
pub unsafe extern "C" fn efi_pe_entry(
    image_handle: EfiHandle,
    system_table: *mut EfiSystemTable,
) -> EfiStatus {
    if system_table.is_null() {
        return status_load_error();
    }

    let cmdline_buf = unsafe {
        core::slice::from_raw_parts_mut(&raw mut EFI_CMDLINE_BUF as *mut u8, EFI_CMDLINE_BUF_LEN)
    };
    unsafe {
        let _ = disable_watchdog(system_table);
    }
    let loaded_image = match unsafe { loaded_image_protocol(system_table, image_handle) } {
        Ok(loaded_image) => loaded_image,
        Err(status) => return status,
    };
    let cmdline_len = match unsafe { copy_loaded_image_options_ascii(loaded_image, cmdline_buf) } {
        Ok(len) => len,
        Err(status) => return status,
    };
    let cmdline_ptr = cmdline_buf.as_ptr() as usize;
    EFI_IMAGE_HANDLE.store(image_handle as usize, Ordering::Release);

    EFI_BOOT.store(1, Ordering::Relaxed);
    CMDLINE_PTR.store(cmdline_ptr, Ordering::Relaxed);
    let _ = cmdline_len;
    EFI_SYSTEM_TABLE_PTR.store(system_table as usize, Ordering::Relaxed);

    // 跳转到内核 _start，传递寄存器参数：
    //    $a0 = 1, $a1 = cmdline_ptr, $a2 = system_table
    unsafe {
        enter_start(system_table as usize, cmdline_ptr);
        core::hint::unreachable_unchecked();
    }
}

/// 跳转到内核入口 [`_start`] 的汇编分支。
///
/// EFI stub 只负责完成早期 handoff，然后用 naked 汇编直接跳转到 `_start`，
/// 避开 Rust 的函数序言/尾声并保持 `_start` 入口约定不变。
///
/// # 寄存器约定
///
/// 入口时（LP64D ABI）：
///   - `$a0` = `system_table_addr`
///   - `$a1` = `cmdline_ptr`
///
/// 跳转前调整为：
///   - `$a0` = 1
///   - `$a1` = `cmdline_ptr`
///   - `$a2` = `system_table_addr`
#[unsafe(naked)]
unsafe extern "C" fn enter_start(system_table_addr: usize, cmdline_ptr: usize) {
    naked_asm!(
        // 将 system_table_addr 从 $a0 移到 $a2
        "or $a2, $a0, $zero",
        // 设置 $a0 = 1
        "ori $a0, $zero, 1",

        // 获取 _start 的绝对地址并跳转
        "la.abs $t0, {start}",
        "jirl $zero, $t0, 0",

        start = sym super::_start,
    )
}
