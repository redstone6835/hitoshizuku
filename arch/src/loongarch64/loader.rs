//! 内核初始化逻辑。
//!
//! 本模块实现 LoongArch64 平台的内核早期初始化流程，由汇编引导代码在完成
//! BSS 清零和临时栈建立之后跳转至此。整个初始化过程分为若干阶段，按顺序
//! 依次执行：安装异常入口、建立早期日志、初始化引导分配器、按启动来源解析
//! EFI 系统表中的关键信息、快照固件表（ACPI/DTB）并退出 Boot Services，最后构造一次性
//! 的 `StartContext` 并跳转到内核主函数 `__kernel_start_init`。
//!
//! 所有初始化步骤均为一次性执行，且控制权不会返回到此模块。全局状态通过
//! `KERNEL_FIRMWARE_STATE` 和 `KERNEL_FIRMWARE_BUFFERS` 保存，供后续
//! 构建 `StartContext` 时读取。

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, compiler_fence};
use core::{fmt, fmt::Write};

use super::efi_stub;
use crate::*;
use efi::*;
use fdt::Fdt;
use general::firmware::{self, FirmwareTableMapping};
use general::{
    StartAcpiTables, StartAddressOps, StartAllocatorOps, StartArchitecture, StartBootInfo,
    StartBootProtocol, StartContext, StartFirmware, StartFirmwareSource, StartMemory,
    StartMemoryMap, StartMemoryRegion, StartMemoryRegionKind, StartNoMapSupport, StartPhysRange,
};
use log::printk;

// ─────────────────────────── 内部常量 ────────────────────────────────

/// 日志行缓冲区大小，足够容纳一条完整的日志（含时间戳和换行）。
const SINK_LINE_BUFFER_SIZE: usize = 1280;
/// 内核命令行字符串的最大长度（含终止 NUL）。
const CMDLINE_BUF_SIZE: usize = 4096;
/// DTB 快照缓冲区的最大容量（4 MiB）。
const DTB_BUF_SIZE: usize = 4096 * 1024;
/// ACPI 表快照缓冲区的最大容量（4 MiB）。
const ACPI_BUF_SIZE: usize = 4096 * 1024;
/// ACPI 表物理到虚拟地址映射表的最大条目数。
const ACPI_MAPPING_CAPACITY: usize = 128;
/// 启动期可保留的最大规范化内存区域数。
const BOOT_MEMORY_REGION_CAPACITY: usize =
    efi_stub::MEMORY_MAP_BUFFER_SIZE / core::mem::size_of::<EfiMemoryDescriptor>();
/// 空的启动内存区域，占位用于静态数组初始化。
const EMPTY_BOOT_MEMORY_REGION: StartMemoryRegion = StartMemoryRegion::new(
    StartPhysRange::new(0, 0),
    StartMemoryRegionKind::Reserved,
    0,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootProtocolSelectionError {
    AcpiWithoutCompletedHandoff,
    EfiStubWithoutCompletedHandoff,
}

impl BootProtocolSelectionError {
    const fn message(self) -> &'static str {
        match self {
            Self::AcpiWithoutCompletedHandoff => {
                "[loader] ACPI handoff requires a post-ExitBootServices EFI memory map"
            }
            Self::EfiStubWithoutCompletedHandoff => {
                "[loader] EFI stub did not complete ExitBootServices; refusing unsafe DT /memory fallback"
            }
        }
    }
}

/// 根据固件来源与 EFI 交接能力选择内核看到的有效启动协议。
///
/// 原始 `$a0` 标志只用于诊断：只有完成 ExitBootServices 后的 EFI 内存图才是
/// RAM 权威来源；其他情况仅允许未经过本内核 EFI stub 的 DTB 兼容交接退化为直启。
const fn select_effective_boot_protocol(
    _reported_efi_boot: bool,
    entered_via_efi_stub: bool,
    memory_map_source: Option<efi_stub::EfiMemoryMapSource>,
    firmware_source: StartFirmwareSource,
) -> Result<StartBootProtocol, BootProtocolSelectionError> {
    if matches!(
        memory_map_source,
        Some(efi_stub::EfiMemoryMapSource::BootServicesExited)
    ) {
        return Ok(StartBootProtocol::Efi);
    }
    match firmware_source {
        StartFirmwareSource::Acpi => Err(BootProtocolSelectionError::AcpiWithoutCompletedHandoff),
        StartFirmwareSource::Dtb if entered_via_efi_stub => {
            Err(BootProtocolSelectionError::EfiStubWithoutCompletedHandoff)
        }
        StartFirmwareSource::Dtb => Ok(StartBootProtocol::Direct),
    }
}

const _: () = {
    assert!(matches!(
        select_effective_boot_protocol(
            false,
            false,
            Some(efi_stub::EfiMemoryMapSource::BootServicesExited),
            StartFirmwareSource::Dtb,
        ),
        Ok(StartBootProtocol::Efi)
    ));
    assert!(matches!(
        select_effective_boot_protocol(
            true,
            true,
            Some(efi_stub::EfiMemoryMapSource::BootServicesExited),
            StartFirmwareSource::Acpi,
        ),
        Ok(StartBootProtocol::Efi)
    ));
    assert!(matches!(
        select_effective_boot_protocol(true, false, None, StartFirmwareSource::Dtb),
        Ok(StartBootProtocol::Direct)
    ));
    assert!(matches!(
        select_effective_boot_protocol(
            true,
            false,
            Some(efi_stub::EfiMemoryMapSource::BootServicesActive),
            StartFirmwareSource::Dtb,
        ),
        Ok(StartBootProtocol::Direct)
    ));
    assert!(matches!(
        select_effective_boot_protocol(
            true,
            true,
            Some(efi_stub::EfiMemoryMapSource::BootServicesActive),
            StartFirmwareSource::Dtb,
        ),
        Err(BootProtocolSelectionError::EfiStubWithoutCompletedHandoff)
    ));
    assert!(matches!(
        select_effective_boot_protocol(
            false,
            false,
            Some(efi_stub::EfiMemoryMapSource::BootServicesActive),
            StartFirmwareSource::Acpi,
        ),
        Err(BootProtocolSelectionError::AcpiWithoutCompletedHandoff)
    ));
};

// ─────────────────────── 固件状态与缓冲区 ─────────────────────────────

/// 全局固件状态，使用原子变量存储，可在卸载 Boot Services 前后安全访问。
///
/// 包含 ACPI/DTB 是否启用、命令行长度、DTB/ACPI 快照长度、ACPI 映射条目数
/// 以及 RSDP 物理地址。所有字段的写入均使用 `Release` 排序，读取使用
/// `Acquire` 排序，以保证在跳转到内核启动代码之前对这些字段的修改可见。
struct KernelFirmwareState {
    /// ACPI 是否被选为固件解析来源。
    acpi_enabled: AtomicBool,
    /// DTB 是否被选为固件解析来源。
    dtb_enabled: AtomicBool,
    /// 内核命令行的有效长度（不含终止 NUL）。
    cmdline_valid_len: AtomicUsize,
    /// DTB 快照的有效字节数。
    dtb_valid_len: AtomicUsize,
    /// ACPI 表快照的总字节数。
    acpi_valid_len: AtomicUsize,
    /// ACPI 映射表中已使用的条目数。
    acpi_mapping_count: AtomicUsize,
    /// ACPI RSDP 的物理地址，仅在 ACPI 启用时有效。
    acpi_rsdp_phys: AtomicUsize,
}

impl KernelFirmwareState {
    /// 使用默认值（全部清零）创建实例。
    const fn new() -> Self {
        Self {
            acpi_enabled: AtomicBool::new(false),
            dtb_enabled: AtomicBool::new(false),
            cmdline_valid_len: AtomicUsize::new(0),
            dtb_valid_len: AtomicUsize::new(0),
            acpi_valid_len: AtomicUsize::new(0),
            acpi_mapping_count: AtomicUsize::new(0),
            acpi_rsdp_phys: AtomicUsize::new(0),
        }
    }

    /// 重置固件选择状态和快照长度，为重新选择做准备。
    fn reset_selection(&self) {
        self.acpi_enabled.store(false, Ordering::Release);
        self.dtb_enabled.store(false, Ordering::Release);
        self.dtb_valid_len.store(0, Ordering::Release);
        self.acpi_valid_len.store(0, Ordering::Release);
        self.acpi_mapping_count.store(0, Ordering::Release);
        self.acpi_rsdp_phys.store(0, Ordering::Release);
    }
}

/// 全局固件缓冲区，存储命令行、DTB、ACPI 表和 ACPI 映射表。
///
/// 由于这些缓冲区必须在 Boot Services 退出前填充，并在退出后保持有效，
/// 因此直接定义为静态数组。`unsafe` 访问通过指针进行，但仅在初始化阶段
/// 单线程执行，无竞争。
struct KernelFirmwareBuffers {
    /// 内核命令行缓冲区，以 NUL 终止。
    command_line: [u8; CMDLINE_BUF_SIZE],
    /// DTB 快照缓冲区。
    dtb: [u8; DTB_BUF_SIZE],
    /// ACPI 表快照缓冲区，用于存放复制的所有 ACPI 表。
    acpi: [u8; ACPI_BUF_SIZE],
    /// ACPI 表物理到虚拟地址映射数组。
    acpi_mappings: [FirmwareTableMapping; ACPI_MAPPING_CAPACITY],
}

impl KernelFirmwareBuffers {
    /// 创建所有缓冲区的零初始化实例。
    const fn new() -> Self {
        Self {
            command_line: [0u8; CMDLINE_BUF_SIZE],
            dtb: [0u8; DTB_BUF_SIZE],
            acpi: [0u8; ACPI_BUF_SIZE],
            acpi_mappings: [FirmwareTableMapping::EMPTY; ACPI_MAPPING_CAPACITY],
        }
    }
}

/// 内核固件状态的全局实例。
static KERNEL_FIRMWARE_STATE: KernelFirmwareState = KernelFirmwareState::new();
/// 内核固件缓冲区的可变全局实例，仅在初始化阶段访问。
static mut KERNEL_FIRMWARE_BUFFERS: KernelFirmwareBuffers = KernelFirmwareBuffers::new();
/// 归一化后的启动内存映射，供 `StartContext` 以平台无关形式持有。
static mut BOOT_MEMORY_REGIONS: [StartMemoryRegion; BOOT_MEMORY_REGION_CAPACITY] =
    [EMPTY_BOOT_MEMORY_REGION; BOOT_MEMORY_REGION_CAPACITY];
/// `BOOT_MEMORY_REGIONS` 当前有效的条目数。
static BOOT_MEMORY_REGION_COUNT: AtomicUsize = AtomicUsize::new(0);

// ─────────────────────── 辅助函数 ─────────────────────────────────────

/// 返回当前存储的内核命令行切片（不包括终止 NUL）。
///
/// 如果没有命令行（长度为 0），则返回 `None`。
fn kernel_command_line() -> Option<&'static [u8]> {
    let len = KERNEL_FIRMWARE_STATE
        .cmdline_valid_len
        .load(Ordering::Acquire);
    if len == 0 {
        return None;
    }
    Some(unsafe {
        core::slice::from_raw_parts(
            addr_of!(KERNEL_FIRMWARE_BUFFERS.command_line).cast::<u8>(),
            len,
        )
    })
}

/// 返回当前存储的 DTB 视图，如果未存储或长度为零则返回 `None`。
fn kernel_dtb() -> Option<Fdt<'static>> {
    let len = KERNEL_FIRMWARE_STATE.dtb_valid_len.load(Ordering::Acquire);
    if len == 0 {
        return None;
    }
    // Safety: dtb_valid_len 只在完整快照复制完成后以 Release 发布；Acquire 读取到
    // 非零长度后，固定缓冲区在内核生命周期内保持只读，且长度受容量约束。
    let slice = unsafe {
        core::slice::from_raw_parts(addr_of!(KERNEL_FIRMWARE_BUFFERS.dtb).cast::<u8>(), len)
    };
    Fdt::parse(slice).ok()
}

/// 从固件虚拟地址取得受快照容量限制的 DTB 字节。
fn firmware_dtb_bytes(vaddr: usize) -> Result<&'static [u8], &'static str> {
    // Safety: EFI 配置表保证 vaddr 指向至少包含 magic/totalsize 的 FDT 前缀；
    // 完整视图只在 totalsize 通过固定容量检查后构造。
    let prefix = unsafe { core::slice::from_raw_parts(vaddr as *const u8, 8) };
    let magic = u32::from_be_bytes(prefix[..4].try_into().map_err(|_| "truncated DTB prefix")?);
    if magic != fdt::DTB_MAGIC {
        return Err("[loader][dtb] invalid DTB magic");
    }
    let total_size =
        u32::from_be_bytes(prefix[4..8].try_into().map_err(|_| "truncated DTB size")?) as usize;
    if total_size < 32 || total_size > DTB_BUF_SIZE {
        return Err("[loader][dtb] DTB size is outside the snapshot range");
    }
    // Safety: EFI/FDT 交接保证声明的 totalsize 范围可读，且长度已限制在静态
    // 快照缓冲区容量内。
    Ok(unsafe { core::slice::from_raw_parts(vaddr as *const u8, total_size) })
}

/// 返回当前有效的 ACPI 映射表切片。
fn kernel_acpi_mappings() -> &'static [FirmwareTableMapping] {
    let count = KERNEL_FIRMWARE_STATE
        .acpi_mapping_count
        .load(Ordering::Acquire);
    unsafe {
        core::slice::from_raw_parts(
            addr_of!(KERNEL_FIRMWARE_BUFFERS.acpi_mappings).cast::<FirmwareTableMapping>(),
            count,
        )
    }
}

/// 返回当前有效的归一化启动内存区域切片。
fn boot_memory_regions() -> &'static [StartMemoryRegion] {
    let count = BOOT_MEMORY_REGION_COUNT.load(Ordering::Acquire);
    unsafe {
        core::slice::from_raw_parts(
            addr_of!(BOOT_MEMORY_REGIONS).cast::<StartMemoryRegion>(),
            count,
        )
    }
}

/// 将 EFI 内存类型转换为平台无关的启动内存分类。
fn classify_efi_memory_type(
    type_: u32,
    source: efi_stub::EfiMemoryMapSource,
) -> StartMemoryRegionKind {
    match type_ {
        1 | 2 => StartMemoryRegionKind::BootloaderReclaimable,
        3 | 4 => {
            if matches!(source, efi_stub::EfiMemoryMapSource::BootServicesExited) {
                StartMemoryRegionKind::FirmwareReclaimable
            } else {
                StartMemoryRegionKind::Reserved
            }
        }
        5 | 6 => StartMemoryRegionKind::FirmwareRuntime,
        7 => StartMemoryRegionKind::UsableRam,
        8 => StartMemoryRegionKind::Unusable,
        9 => StartMemoryRegionKind::AcpiReclaimable,
        10 => StartMemoryRegionKind::AcpiNonVolatileStorage,
        11 | 12 => StartMemoryRegionKind::Mmio,
        _ => StartMemoryRegionKind::Reserved,
    }
}

/// 将 EFI 原始内存描述符转换为平台无关的启动内存区域切片。
fn snapshot_boot_memory_regions(
    snapshot: efi_stub::RawEfiMemoryMapSnapshot,
) -> Result<&'static [StartMemoryRegion], &'static str> {
    if snapshot.descriptor_size < core::mem::size_of::<EfiMemoryDescriptor>() {
        printk!(
            "[loader] EFI memory descriptor size too small: got={} expected_at_least={}",
            snapshot.descriptor_size,
            core::mem::size_of::<EfiMemoryDescriptor>(),
        );
        return Err("[loader] EFI memory descriptor size is smaller than the UEFI baseline");
    }
    if !snapshot
        .bytes
        .len()
        .is_multiple_of(snapshot.descriptor_size)
    {
        printk!(
            "[loader] EFI memory map has a partial descriptor: bytes={} descriptor_size={}",
            snapshot.bytes.len(),
            snapshot.descriptor_size,
        );
        return Err("[loader] EFI memory map length is not descriptor-aligned");
    }

    BOOT_MEMORY_REGION_COUNT.store(0, Ordering::Release);
    let mut count = 0usize;
    let mut offset = 0usize;
    while offset + snapshot.descriptor_size <= snapshot.bytes.len() {
        let descriptor = unsafe {
            snapshot.bytes[offset..offset + snapshot.descriptor_size]
                .as_ptr()
                .cast::<EfiMemoryDescriptor>()
                .read_unaligned()
        };
        offset += snapshot.descriptor_size;

        if descriptor.number_of_pages == 0 {
            continue;
        }

        let start = usize::try_from(descriptor.physical_start)
            .map_err(|_| "[loader] EFI memory descriptor start exceeds physical address width")?;
        let pages = usize::try_from(descriptor.number_of_pages)
            .map_err(|_| "[loader] EFI memory descriptor page count exceeds address width")?;
        let size = pages
            .checked_mul(4096)
            .ok_or("[loader] EFI memory descriptor page count overflowed")?;
        let end = start
            .checked_add(size)
            .ok_or("[loader] EFI memory descriptor range overflowed")?;
        if count >= BOOT_MEMORY_REGION_CAPACITY {
            printk!(
                "[loader] normalized boot memory region buffer exhausted: descriptors_seen={} capacity={} snapshot_bytes={} descriptor_size={}",
                count + 1,
                BOOT_MEMORY_REGION_CAPACITY,
                snapshot.bytes.len(),
                snapshot.descriptor_size,
            );
            return Err("[loader] normalized boot memory region buffer exhausted");
        }

        unsafe {
            addr_of_mut!(BOOT_MEMORY_REGIONS)
                .cast::<StartMemoryRegion>()
                .add(count)
                .write(
                    StartMemoryRegion::new(
                        StartPhysRange::new(start, end),
                        classify_efi_memory_type(descriptor.type_, snapshot.source),
                        descriptor.attribute,
                    )
                    .with_source_type(descriptor.type_),
                );
        }
        count += 1;
    }

    BOOT_MEMORY_REGION_COUNT.store(count, Ordering::Release);
    Ok(boot_memory_regions())
}

/// 将原始命令行地址复制到内核命令行缓冲区，并返回其切片。
///
/// 复制时会扫描到 NUL 终止符或达到最大长度为止，并保证以 NUL 结尾。
///
/// 在当前平台上，`raw_ptr == 0` 仍可能是有效的命令行地址，因此这里
/// 不能把 0 直接解释为“没有命令行”。
fn store_kernel_command_line(raw_ptr: usize) -> Option<&'static [u8]> {
    KERNEL_FIRMWARE_STATE
        .cmdline_valid_len
        .store(0, Ordering::Release);

    let src = reset_to_virt(raw_ptr) as *const u8;
    let mut len = 0usize;
    while len + 1 < CMDLINE_BUF_SIZE {
        let byte = unsafe { src.add(len).read() };
        if byte == 0 {
            break;
        }
        len += 1;
    }

    unsafe {
        let dst = addr_of_mut!(KERNEL_FIRMWARE_BUFFERS.command_line).cast::<u8>();
        core::ptr::copy_nonoverlapping(src, dst, len);
        dst.add(len).write(0);
        erase_original_cmdline(src.cast_mut(), len + 1, dst, CMDLINE_BUF_SIZE);
    }
    KERNEL_FIRMWARE_STATE
        .cmdline_valid_len
        .store(len, Ordering::Release);

    if len + 1 == CMDLINE_BUF_SIZE {
        printk!(
            "[loader] boot command line truncated to {} bytes",
            CMDLINE_BUF_SIZE - 1
        );
    }

    kernel_command_line()
}

fn ranges_overlap(a_start: usize, a_len: usize, b_start: usize, b_len: usize) -> bool {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

unsafe fn erase_original_cmdline(src: *mut u8, len: usize, saved: *const u8, saved_len: usize) {
    if len == 0 || ranges_overlap(src as usize, len, saved as usize, saved_len) {
        return;
    }

    for offset in 0..len {
        unsafe {
            src.add(offset).write_volatile(0);
        }
    }
    compiler_fence(Ordering::SeqCst);
}

/// 从指定的物理地址拷贝 DTB 到内核缓冲区，并返回 DTB 视图。
///
/// 成功时打印拷贝信息，失败时返回错误描述。
fn store_kernel_dtb_from_address(fdt_addr: usize) -> Result<Fdt<'static>, &'static str> {
    if fdt_addr == 0 {
        return Err("[loader][dtb] missing DTB address");
    }

    let fdt_addr = reset_to_virt(fdt_addr);
    let fw_bytes = firmware_dtb_bytes(fdt_addr)?;
    let fw_dtb = Fdt::parse(fw_bytes).map_err(|_| "[loader][dtb] invalid DTB layout")?;
    let dtb_bytes = fw_dtb.as_bytes();
    let dtb_size = dtb_bytes.len();
    if dtb_size > DTB_BUF_SIZE {
        printk!(
            "[loader][dtb] DTB snapshot buffer exhausted: size={} bytes capacity={} bytes src={:#x}",
            dtb_size,
            DTB_BUF_SIZE,
            fdt_addr,
        );
        return Err("[loader][dtb] DTB too large");
    }

    // Safety: 源 DTB 已通过 Fdt::parse 且长度不超过目标静态缓冲区；该复制发生在
    // 单线程启动阶段，发布有效长度前没有读者，源地址与内核缓冲区不重叠。
    unsafe {
        core::ptr::copy_nonoverlapping(
            dtb_bytes.as_ptr(),
            addr_of_mut!(KERNEL_FIRMWARE_BUFFERS.dtb).cast::<u8>(),
            dtb_size,
        );
        KERNEL_FIRMWARE_STATE
            .dtb_valid_len
            .store(dtb_size, Ordering::Release);
    }

    printk!(
        "[loader][dtb] DTB copied to kernel buffer: src={:#x} size={} bytes",
        fdt_addr,
        dtb_size,
    );
    kernel_dtb().ok_or("[loader][dtb] DTB copy verification failed")
}

/// 快照所有 ACPI 表（从 RSDP 开始，递归复制根表及其子表）到内核缓冲区。
///
/// 成功后返回 RSDP 的物理地址，并填充 ACPI 映射表。
fn snapshot_acpi_tables(rsdp_addr: usize) -> Result<usize, &'static str> {
    let rsdp_phys = virt_to_phys(rsdp_addr);
    let root_info: firmware::acpi::AcpiSnapshotRootInfo =
        firmware::acpi::snapshot_root_info(rsdp_phys, acpi_phys_bytes)?;
    copy_acpi_range(rsdp_phys, root_info.rsdp_copy_len)?;

    let root_len = firmware::acpi::table_length(root_info.root_phys, acpi_phys_bytes)?;
    let root = copy_acpi_range(root_info.root_phys, root_len)?;
    firmware::acpi::validate_root_table(root, root_info.root_kind)?;

    firmware::acpi::for_each_root_table_entry(root, root_info.root_kind, |table_phys| {
        let table_len = firmware::acpi::table_length(table_phys, acpi_phys_bytes)?;
        let table = copy_acpi_range(table_phys, table_len)?;
        firmware::acpi::validate_sdt(table)?;

        // 如果是 FADT，还需要复制 DSDT 和 FACS（如果存在）
        if let Some(closure) = firmware::acpi::fadt_closure(table)? {
            if let Some(dsdt_phys) = closure.dsdt_phys {
                let dsdt_len = firmware::acpi::table_length(dsdt_phys, acpi_phys_bytes)?;
                let dsdt = copy_acpi_range(dsdt_phys, dsdt_len)?;
                firmware::acpi::validate_sdt(dsdt)?;
            }
            if let Some(facs_phys) = closure.facs_phys {
                let facs_len = firmware::acpi::facs_length(facs_phys, acpi_phys_bytes)?;
                copy_acpi_range(facs_phys, facs_len)?;
            }
        }
        Ok(())
    })?;

    KERNEL_FIRMWARE_STATE
        .acpi_rsdp_phys
        .store(rsdp_phys, Ordering::Release);
    printk!(
        "[loader][acpi] ACPI tables copied: RSDP={:#x} tables={} bytes={}",
        rsdp_phys,
        KERNEL_FIRMWARE_STATE
            .acpi_mapping_count
            .load(Ordering::Acquire),
        KERNEL_FIRMWARE_STATE.acpi_valid_len.load(Ordering::Acquire),
    );
    Ok(rsdp_phys)
}

/// 将物理地址处长度为 `len` 的 ACPI 数据复制到内核 ACPI 缓冲区，并维护映射。
///
/// 如果该物理地址已存在映射且长度足够，直接返回已映射的虚拟切片；
/// 否则在缓冲区中分配新区域并复制。返回新分配或已存在的虚拟切片。
fn copy_acpi_range(phys_addr: usize, len: usize) -> Result<&'static [u8], &'static str> {
    if len == 0 {
        return Ok(&[]);
    }
    // 检查是否已有足够长的映射，避免重复复制
    if let Some(virt) = kernel_acpi_mappings()
        .iter()
        .find_map(|mapping| mapping.resolve(phys_addr, len))
    {
        return Ok(unsafe { core::slice::from_raw_parts(virt as *const u8, len) });
    }
    // 防止冲突：同一个物理地址已有更短的映射
    for mapping in kernel_acpi_mappings() {
        if mapping.physical_start == phys_addr && mapping.length < len {
            return Err("[loader][acpi] conflicting ACPI table length");
        }
    }

    let count = KERNEL_FIRMWARE_STATE
        .acpi_mapping_count
        .load(Ordering::Acquire);
    if count >= ACPI_MAPPING_CAPACITY {
        printk!(
            "[loader][acpi] ACPI mapping capacity exhausted: used={} capacity={} next_phys={:#x} next_len={}",
            count,
            ACPI_MAPPING_CAPACITY,
            phys_addr,
            len,
        );
        return Err("[loader][acpi] ACPI mapping table full");
    }

    // 在 ACPI 缓冲区中分配 8 字节对齐的空间
    let offset = KERNEL_FIRMWARE_STATE
        .acpi_valid_len
        .load(Ordering::Acquire)
        .checked_add(7)
        .map(|value| value & !7usize)
        .ok_or("[loader][acpi] ACPI buffer offset overflow")?;
    let end = offset
        .checked_add(len)
        .ok_or("[loader][acpi] ACPI buffer size overflow")?;
    if end > ACPI_BUF_SIZE {
        printk!(
            "[loader][acpi] ACPI snapshot buffer exhausted: used={} aligned_offset={} request={} capacity={} phys={:#x}",
            KERNEL_FIRMWARE_STATE.acpi_valid_len.load(Ordering::Acquire),
            offset,
            len,
            ACPI_BUF_SIZE,
            phys_addr,
        );
        return Err("[loader][acpi] ACPI table snapshot too large");
    }

    unsafe {
        let src = phys_to_virt(phys_addr) as *const u8;
        let dst = addr_of_mut!(KERNEL_FIRMWARE_BUFFERS.acpi)
            .cast::<u8>()
            .add(offset);
        core::ptr::copy_nonoverlapping(src, dst, len);
        addr_of_mut!(KERNEL_FIRMWARE_BUFFERS.acpi_mappings)
            .cast::<FirmwareTableMapping>()
            .add(count)
            .write(FirmwareTableMapping {
                physical_start: phys_addr,
                virtual_start: dst as usize,
                length: len,
            });
    }
    KERNEL_FIRMWARE_STATE
        .acpi_valid_len
        .store(end, Ordering::Release);
    KERNEL_FIRMWARE_STATE
        .acpi_mapping_count
        .store(count + 1, Ordering::Release);
    kernel_acpi_mappings()
        .iter()
        .find_map(|mapping| mapping.resolve(phys_addr, len))
        .map(|virt| unsafe { core::slice::from_raw_parts(virt as *const u8, len) })
        .ok_or("[loader][acpi] ACPI copy verification failed")
}

/// 辅助函数：根据物理地址读取指定长度的字节切片。
///
/// 在 ACPI 快照过程中用于临时读取固件表内容（尚未复制之前）。
fn acpi_phys_bytes(phys_addr: usize, len: usize) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(phys_to_virt(phys_addr) as *const u8, len) }
}

/// 简单的行日志缓冲区，实现 `fmt::Write`，用于格式化一条日志消息。
///
/// 当日志超过缓冲区长度时，多余部分被截断，不触发错误。
struct SinkLineBuffer {
    buf: [u8; SINK_LINE_BUFFER_SIZE],
    len: usize,
}

impl SinkLineBuffer {
    const fn new() -> Self {
        Self {
            buf: [0; SINK_LINE_BUFFER_SIZE],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl fmt::Write for SinkLineBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.len >= self.buf.len() {
            return Ok(());
        }

        let available = self.buf.len() - self.len;
        let copy_len = s.len().min(available);
        self.buf[self.len..self.len + copy_len].copy_from_slice(&s.as_bytes()[..copy_len]);
        self.len += copy_len;
        Ok(())
    }
}

/// 将一条日志记录格式化为带时间戳的单行文本，返回行缓冲区。
fn format_log_record_line(record: &log::LogRecord<'_>) -> SinkLineBuffer {
    let (secs, nanos) = log::format_timestamp(record.timestamp);
    let mut buf = SinkLineBuffer::new();
    let _ = writeln!(
        &mut buf,
        "[{:6}.{:06}] {}",
        secs,
        nanos / 1000,
        record.message
    );
    buf
}

// ── 稳定计时器频率（从 CPUCFG 读取，默认 100 MHz） ────────────────────────
//
// LoongArch64 提供了一个"稳定计数器"（stable counter），通过 rdtime.d 指令
// 读取。与 x86 的 TSC 不同，该计数器不随 CPU 变频而变化，是真正的单调时钟。
// 为了将计数值转换为纳秒时间戳，必须知道其底层振荡器的频率（单位 Hz）。
//
// 此频率可从两个来源获取：
//   1. CPUCFG 寄存器组（字 4 和字 5）：硬件直接告知，最权威；
//   2. DTB 的 cpus/cpu@0 节点的 timebase-frequency 属性：固件填写。
//
// 本实现优先使用 CPUCFG（步骤 1.1），默认值 100 MHz 是 QEMU LoongArch 虚拟
// 机的实际频率，也是大多数实物板卡的典型值。

/// 从 CPUCFG 或 DTB 中获得的稳定计时器频率（单位 Hz），全局共享。
///
/// 初始值 100_000_000（100 MHz）为 QEMU LoongArch64 默认值。
pub static STABLE_TIMER_HZ: AtomicUsize = AtomicUsize::new(100_000_000);
static TIMER_HZ: AtomicUsize = AtomicUsize::new(DEFAULT_TIMER_HZ);
static TIMER_PERIOD_TICKS: AtomicU64 = AtomicU64::new(1_000_000);

/// TCFG.InitVal 可写入位 47:2 的最大原始计数值。
const TCFG_MAX_TICKS: u64 = ((1u64 << 48) - 1) & !0b11;

/// 启动时刻的原始计时值，用于将后续时间戳归零到启动时刻。
static BOOT_TIMESTAMP_NS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn timer_hz() -> usize {
    TIMER_HZ.load(Ordering::Acquire)
}

pub(crate) fn configure_local_timer(timer_hz: usize) {
    let timer_hz = timer_hz.clamp(1, 10_000);
    TIMER_HZ.store(timer_hz, Ordering::Release);
    let stable_hz = STABLE_TIMER_HZ.load(Ordering::Relaxed) as u64;
    let period = (stable_hz / timer_hz as u64).max(1);
    TIMER_PERIOD_TICKS.store(period.min(TCFG_MAX_TICKS), Ordering::Release);
    const LIE_TIMER: usize = 1 << 11;
    const LIE_IPI: usize = 1 << 12;
    let lie_val = LIE_TIMER | LIE_IPI;
    let lie_mask = LIE_TIMER | LIE_IPI;
    let clear_timer = 1usize;
    unsafe {
        core::arch::asm!(
            "csrxchg {val}, {mask}, {csr_ecfg}",
            val = inout(reg) lie_val => _,
            mask = in(reg) lie_mask,
            csr_ecfg = const CSR_ECFG,
            options(nostack, preserves_flags)
        );
        core::arch::asm!(
            "csrwr {val}, {csr_ticlr}",
            val = inout(reg) clear_timer => _,
            csr_ticlr = const CSR_TICLR,
            options(nostack, preserves_flags)
        );
    }
    rearm_local_timer(None);
}

/// 按软件绝对截止时间重装当前 CPU 的 one-shot 定时器。
///
/// 无论是否存在软件 deadline，下一次中断都不会晚于常规调度 tick；这样短超时
/// 可以获得亚 tick 精度，同时 EEVDF、网络轮询等周期工作不会因 one-shot 模式
/// 而停止。所有纳秒到计数值的转换均向上取整，避免定时器早于请求时间触发。
pub(crate) fn rearm_local_timer(deadline_ns: Option<u64>) {
    let period = TIMER_PERIOD_TICKS
        .load(Ordering::Acquire)
        .clamp(1, TCFG_MAX_TICKS);
    let ticks = deadline_ns.map_or(period, |deadline| {
        let now_ns = kernel_timestamp_ns();
        let delta_ns = deadline.saturating_sub(now_ns);
        let stable_hz = STABLE_TIMER_HZ.load(Ordering::Relaxed).max(1) as u128;
        let ticks = if delta_ns == 0 {
            1
        } else {
            ((delta_ns as u128 * stable_hz).saturating_add(999_999_999) / 1_000_000_000)
                .clamp(1, TCFG_MAX_TICKS as u128) as u64
        };
        ticks.min(period)
    });
    // TCFG 的计数值直接占据位 47:2，硬件按 4 递减；它不是需要左移后再
    // 填入的普通整数位域。额外左移两位会把所有超时精确放大为四倍。
    let ticks = ticks.clamp(4, TCFG_MAX_TICKS).saturating_add(3) & !0b11;
    let tcfg_val = ticks | 1;
    unsafe {
        core::arch::asm!(
            "csrwr {val}, {csr_tcfg}",
            val = inout(reg) tcfg_val => _,
            csr_tcfg = const CSR_TCFG,
            options(nostack, preserves_flags)
        );
    }
}

// ── 分配器临界区辅助 ──────────────────────────────────────────────

/// 进入分配器临界区：保存当前中断状态并关闭中断。
///
/// 这确保分配器内部操作不被中断打断，避免重入导致的数据竞争。
#[inline]
fn gc_enter_critical() -> usize {
    let state = unsafe { LoongArch64InterruptOps::save_interrupt_state() };
    unsafe { LoongArch64InterruptOps::disable_interrupts() };
    state
}

/// 离开分配器临界区：恢复之前保存的中断状态。
///
/// 参数 `state` 来自 `gc_enter_critical` 的返回值。
fn gc_leave_critical(state: usize) {
    unsafe { LoongArch64InterruptOps::restore_interrupt_state(state) };
}

/// LoongArch64 平台内核架构加载器入口。
///
/// 本函数由汇编引导代码（`_start_virtualized`）在以下前置条件均满足后调用：
/// 1. DMW0/DMW1 已配置完毕，虚拟地址空间已建立；
/// 2. BSS 段已被清零（链接器脚本中 `sbss`…`ebss` 范围）；
/// 3. 临时内核栈（`__tmp_stack_bottom`…`__tmp_stack_top`）已就绪，
///    `$sp` 已指向栈顶；
/// 4. CPU 处于 PLV0（最高特权级），中断全局关闭（CRMD.IE=0）。
///
/// 函数内部按顺序执行早期架构初始化步骤，最终通过 `jirl $r0` 无返回跳转到
/// `__kernel_start_init`，不再返回到此函数。
///
/// # Safety
/// 此函数只能由 `_start_virtualized` 调用一次，不得从其他任何位置调用。
pub unsafe extern "C" fn __kernel_arch_loader() {
    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 1：安装异常/中断入口地址
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 处理器必须立即拥有一套可用的异常处理程序，否则任何意外故障都将导致
    // CPU 跳转到未初始化的地址（通常为 0x0），造成不可控的行为。
    //
    // `install_exception_entry` 同时将三个关键 CSR 设置为内核异常处理函数：
    //   - CSR_EENTRY     ：通用异常入口。凡 TLB 相关、系统调用、断点、非法
    //                      指令等异常，硬件都会将控制权交至此地址。
    //   - CSR_TLBRENTRY  ：TLB 重填快路径（用于硬件页表遍历，lddir/ldpte）。
    //   - CSR_MERRENTRY  ：机器错误入口，处理不可恢复的硬件错误。
    //
    // 这些入口函数均位于链接脚本设定的 `.text` 段中，且使用绝对地址，
    // 因此必须在 MMU 和 DMW 窗口稳定后安装。
    unsafe { install_exception_entry() };

    e_print(format_args!(
        "[{:6}.{:06}] [boot] Entering kernel loader...\n",
        0, 0
    ));

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 1.1：通过 CPUCFG 读取稳定计时器频率
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // LoongArch64 提供了一组 CPU 配置寄存器（CPUCFG），其中字 4 和字 5
    // 包含了稳定计时器（stable counter）的基准频率与倍频/分频系数。
    //
    // 寄存器布局（架构定义）：
    //   CPUCFG.4（CC_FREQ）  ：[31:0] 计时器基准振荡器频率 (Hz)，big-endian
    //   CPUCFG.5（CC_MUL/CC_DIV）：
    //       [15:0]   CC_MUL – 频率倍乘系数
    //       [31:16]  CC_DIV – 频率分频系数
    //
    // 若硬件未实现倍频/分频字段（全零），则直接使用 CC_FREQ 作为最终频率。
    // 否则频率 = CC_FREQ * CC_MUL / CC_DIV。
    //
    // 读取结果存入全局原子变量 `STABLE_TIMER_HZ`，供时间戳源使用。
    {
        let cc_freq: u32;
        let cc_mul_div: u32;
        unsafe {
            // rd = CPUCFG[rj] 指令，rj=4 读取 CC_FREQ
            core::arch::asm!(
                "cpucfg {rd}, {rj}",
                rd = out(reg) cc_freq,
                rj = in(reg) 4usize,
            );
            // rj=5 读取 CC_MUL/CC_DIV
            core::arch::asm!(
                "cpucfg {rd}, {rj}",
                rd = out(reg) cc_mul_div,
                rj = in(reg) 5usize,
            );
        }
        let cc_mul = (cc_mul_div & 0xFFFF) as u64; // 低 16 位
        let cc_div = ((cc_mul_div >> 16) & 0xFFFF) as u64; // 高 16 位
        let hz = if cc_mul == 0 || cc_div == 0 {
            cc_freq as usize // 硬件未实现分频，直接使用基准频率
        } else {
            (cc_freq as u64 * cc_mul / cc_div) as usize
        };
        if hz > 0 {
            STABLE_TIMER_HZ.store(hz, Ordering::Relaxed);
        }
        e_print(format_args!(
            "[{:6}.{:06}] [loader] stable timer frequency from cpucfg: CC_FREQ={} CC_MUL={} CC_DIV={} -> {} Hz\n",
            0,
            0,
            cc_freq,
            cc_mul,
            cc_div,
            STABLE_TIMER_HZ.load(Ordering::Relaxed),
        ));

        // 步骤 1.1b：配置定时器中断，使其按配置频率产生中断。
        // 默认 100 Hz；命令行 `timer_hz=N` 可覆盖。
        let timer_hz = {
            let mut hz = DEFAULT_TIMER_HZ;
            let raw = CMDLINE_PTR.load(Ordering::Acquire);
            if raw != 0 {
                let cmd = unsafe {
                    general::cmdline::Cmdline::from_raw_until_nul(
                        reset_to_virt(raw) as *const u8,
                        4096,
                    )
                };
                if let Some(val) = cmd.find("timer_hz").and_then(|v| v.parse().ok()) {
                    hz = val;
                }
            }
            hz.max(1).min(10000)
        };
        configure_local_timer(timer_hz);
        let period = stable_counter_hz() / timer_hz as u64;
        e_print(format_args!(
            "[{:6}.{:06}] [loader] timer configured: hz={} period={}\n",
            0, 0, timer_hz, period,
        ));
    }
    super::trap::install_loongarch_irq_line_ops();

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 1.2：注册时间戳源 (Timestamp Source)
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 日志系统需要从某个单调时钟获取纳秒级时间戳。这里使用 LoongArch64 的
    // `rdtime.d` 指令读取稳定计数器，并根据 `STABLE_TIMER_HZ` 将其转换为纳秒。
    //
    // 转换公式： ns = (cnt / hz) * 1_000_000_000 + (cnt % hz) * 1_000_000_000 / hz
    // 为避免中间乘积溢出 u64，分段计算。
    //
    // 同时记录启动时的计时器原始值到 `BOOT_TIMESTAMP_NS`，使得日志输出的时间戳
    // 从 0.000000 开始（即为从启动到当前的时间差）。
    {
        fn time() -> u64 {
            let cnt: u64;
            unsafe {
                // 读稳定计数器，$rj 为 $zero 时忽略
                core::arch::asm!(
                    "rdtime.d {cnt}, $zero",
                    cnt = out(reg) cnt,
                );
            }
            let hz = STABLE_TIMER_HZ.load(Ordering::Relaxed) as u64;
            if hz == 0 {
                return 0;
            }
            let secs = cnt / hz;
            let frac_ns = (cnt % hz) * 1_000_000_000 / hz;
            secs * 1_000_000_000 + frac_ns
        }
        fn timestamp_since_boot() -> u64 {
            time().saturating_sub(BOOT_TIMESTAMP_NS.load(Ordering::Relaxed))
        }

        BOOT_TIMESTAMP_NS.store(time(), Ordering::Relaxed);
        log::register_timestamp_source(timestamp_since_boot);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 1.3：绑定早期日志输出 (Early Log Sink)
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 在内核尚未建立完整的控制台框架（`general::console`）之前，所有 `printk!`
    // 等日志宏的输出需要一个临时出口。这里直接绑定到 `e_write_bytes`，该函数
    // 通过直接操作 UART 寄存器（MMIO）发送数据，完全不依赖任何内存分配或锁。
    //
    // 这个 early sink 将在 `kernel_start_init` 完成控制台注册后被替换。
    {
        fn early_sink_write(record: &log::LogRecord<'_>) {
            let line = format_log_record_line(record);
            crate::e_write_bytes(line.as_bytes());
        }
        static EARLY_LOG_SINK: log::LogSink = log::LogSink {
            write_record: early_sink_write,
        };
        log::bind_log_sink(&EARLY_LOG_SINK);
    }

    // 此时日志系统已可用，输出版本或状态信息（不占用串口直接输出的 e_print）
    {
        let (secs, nanos) = log::format_timestamp(log::get_timestamp_ns());
        e_print(format_args!(
            "[{:6}.{:06}] [loader] Successfully initialized early logger at early_console.\n",
            secs,
            nanos / 1000
        ));
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 2：初始化引导期分配器 (Boot Allocator)
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 此时 BSS 已清零，堆区域 (`sheap` .. `eheap`) 完全空闲。我们将其交给全局
    // 分配器 `allocator::KERNEL_ALLOCATOR` 作为初始物理内存池。
    //
    // 为了让分配器在未来能够处理物理地址与虚拟地址的转换、在多核环境下安全
    // 运行，我们在此同时注入：
    //   - 地址转换函数 (phys_to_virt / virt_to_phys)
    //   - 当前 CPU ID 获取函数（用于 per-CPU 缓存）
    //   - 临界区函数（关闭/恢复中断），防止重入
    //
    // 此步骤完成后，所有依赖 alloc 的代码（Box, Vec, String 等）均可正常使用。
    {
        unsafe extern "C" {
            fn sheap(); // 堆起始（链接脚本符号）
            fn eheap(); // 堆末尾
        }

        let heap_start = sheap as *const () as usize;
        let heap_end = eheap as *const () as usize;
        let heap_size = heap_end - heap_start;

        allocator::KERNEL_ALLOCATOR.bind_address_translation(phys_to_virt, virt_to_phys);
        allocator::KERNEL_ALLOCATOR.bind_cpu_id(LoongArch64MessageInterruptOps::current_cpu_id);
        allocator::KERNEL_ALLOCATOR.bind_gc_critical_section(gc_enter_critical, gc_leave_critical);
        allocator::KERNEL_ALLOCATOR.init_boot(heap_start, heap_size);

        printk!(
            "[loader] boot allocator: {:#x}..{:#x} ({} MiB)",
            heap_start,
            heap_end,
            heap_size / (1024 * 1024),
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 3：采集启动来源参数并选择固件表
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 从 `_start` 传入并由 `pre_boot_init` 保存在全局原子变量中的原始参数：
    //   - EFI_SYSTEM_TABLE_PTR  (可能为 0)
    //   - CMDLINE_PTR           (在当前平台上即使为 0 也可能是有效地址)
    // 这些值可能是固件传递的物理地址，也可能是 QEMU 直线路径下的物理地址，
    // 因此必须通过 `firmware_pointer_to_virt` 转换为当前地址空间可访问的
    // 虚拟地址（在 DMW 窗口内）。
    let raw_st_addr = EFI_SYSTEM_TABLE_PTR.load(Ordering::Acquire);
    let raw_cmdline_ptr = CMDLINE_PTR.load(Ordering::Acquire);
    let canonical_st_addr = if raw_st_addr == 0 {
        0
    } else {
        reset_to_virt(raw_st_addr)
    };
    let canonical_cmdline_ptr = reset_to_virt(raw_cmdline_ptr);

    // 将命令行复制到内核静态缓冲区（.data 或 .bss 中），避免后续被覆盖
    let _ = store_kernel_command_line(raw_cmdline_ptr);

    printk!(
        "[loader] boot args: efi_boot={} cmdline={:#x}->{:#x} efi_system_table={:#x}->{:#x}",
        EFI_BOOT.load(Ordering::Acquire),
        raw_cmdline_ptr,
        canonical_cmdline_ptr,
        raw_st_addr,
        canonical_st_addr,
    );

    // 如果原始系统表地址有效，尝试创建 EfiSystemTableView（非空校验+对齐校验）
    let fw_view = if raw_st_addr == 0 {
        None
    } else {
        unsafe { EfiSystemTableView::from_ptr(canonical_st_addr) }
    };
    if raw_st_addr != 0 && fw_view.is_none() {
        printk!(
            "[loader] EFI system table pointer invalid: raw={:#x} canonical={:#x}",
            raw_st_addr,
            canonical_st_addr,
        );
    }

    let fw_table = if let Some(fw_view) = fw_view {
        printk!(
            "[loader] EFI system table address accepted: raw={:#x} canonical={:#x}",
            raw_st_addr,
            canonical_st_addr,
        );
        Some(fw_view.as_ptr() as *mut EfiSystemTable)
    } else {
        None
    };

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 3.1：固件表选择、退出 Boot Services、完成私有快照
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 固件选择策略：优先使用 ACPI（若 EFI 配置表中有 RSDP），否则使用 DTB。
    // 这遵循 Linux/LoongArch 的默认行为。
    //
    // 在退出 Boot Services 之前，只从 EFI 系统表中提取后续真正需要的
    // ACPI/DTB 指针；退出后不再访问 EFI 系统表本体。
    //
    // 接着根据选择执行对应的快照操作：
    //   - ACPI：从 RSDP 出发递归发现并复制所有 ACPI 表到内核缓冲区，
    //           同时建立物理→虚拟地址映射表。
    //   - DTB ：直接将设备树数据复制到内核预留的 DTB 缓冲区中。
    //
    // 若 EFI 内存映射快照可用，后续 `StartMemoryMap` 将携带归一化后的平台区域；
    // 若无，则仅允许那些本身不依赖独立启动内存映射的固件路径继续。
    KERNEL_FIRMWARE_STATE.reset_selection();
    let mut acpi_rsdp_for_state = 0usize;
    if let Some(fw_table) = fw_table {
        let acpi_rsdp = unsafe { (*fw_table).find_acpi_rsdp() };
        let fdt = unsafe { (*fw_table).find_fdt() };
        match efi_stub::exit_boot_services_with_memory_map_snapshot(fw_table) {
            Ok(()) => {
                printk!("[loader] EFI Boot Services exited after extracting system table data")
            }
            Err(status) if status == status_unsupported() => {
                match efi_stub::snapshot_memory_map(fw_table) {
                    Ok(()) => printk!(
                        "[loader] EFI ExitBootServices hook unavailable; captured EFI memory map without exiting Boot Services"
                    ),
                    Err(map_status) if acpi_rsdp.is_some() => panic!(
                        "[loader] ACPI tables discovered but EFI memory map snapshot failed without ExitBootServices: {} ({:#x})",
                        status_name(map_status),
                        map_status,
                    ),
                    Err(map_status) => {
                        printk!(
                            "[loader] EFI ExitBootServices hook unavailable and EFI memory map snapshot failed: {} ({:#x}); continuing without EFI memory map",
                            status_name(map_status),
                            map_status,
                        );
                    }
                }
            }
            Err(status) => panic!(
                "[loader] failed to exit EFI Boot Services: {} ({:#x})",
                status_name(status),
                status
            ),
        }

        if let Some(rsdp) = acpi_rsdp {
            // ACPI 路径需要 EFI 内存映射来获取可用物理内存，否则无法初始化分配器
            if matches!(
                efi_stub::memory_map_snapshot().map(|snapshot| snapshot.source),
                Some(efi_stub::EfiMemoryMapSource::BootServicesExited)
            ) {
                let rsdp_phys = snapshot_acpi_tables(rsdp as usize).unwrap_or_else(|err| {
                    panic!("[loader] failed to snapshot ACPI tables: {}", err)
                });
                acpi_rsdp_for_state = rsdp_phys;
                KERNEL_FIRMWARE_STATE
                    .acpi_enabled
                    .store(true, Ordering::Release);
                KERNEL_FIRMWARE_STATE
                    .dtb_enabled
                    .store(false, Ordering::Release);
                printk!(
                    "[loader] firmware selection: ACPI enabled, RSDP={:#x}; DTB ignored",
                    rsdp_phys,
                );
            } else {
                panic!(
                    "[loader] ACPI tables discovered but ExitBootServices did not produce a usable memory map"
                );
            }
        } else if let Some(fdt) = fdt {
            store_kernel_dtb_from_address(fdt as usize)
                .unwrap_or_else(|err| panic!("[loader] failed to snapshot EFI DTB: {}", err));
            KERNEL_FIRMWARE_STATE
                .dtb_enabled
                .store(true, Ordering::Release);
            KERNEL_FIRMWARE_STATE
                .acpi_enabled
                .store(false, Ordering::Release);
            printk!("[loader] firmware selection: DTB enabled from EFI configuration table");
        } else {
            panic!("[loader] firmware selection failed: neither ACPI nor DTB found");
        }
    } else {
        panic!("[loader] EFI system table unavailable; cannot discover ACPI or DTB");
    }
    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 4：构造启动上下文 (StartContext) 并跳转到内核主函数
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 所有架构特定的初始化已经完成，剩下的工作将由平台无关的 `kernel_start_init`
    // 完成。这里我们构造一个 `StartContext` 结构体，它是一份只读的、一次性的
    // 交接文档，描述了内核启动的完整初始条件。
    //
    // 内容包括：
    //   - boot          : 架构类型、启动协议、boot CPU ID 以及命令行。
    //   - firmware      : 选定的固件表格（ACPI 或 DTB 及其私有快照）。
    //   - memory        : 内核镜像占用的物理范围，以及启动内存映射（如有）。
    //   - address       : 物理地址到虚拟地址的转换函数（普通 RAM 和 MMIO）。
    //   - allocator     : 可选的分配器回调（内核堆区域、映射/解映射等）。
    //
    // 构造完成后，将上下文指针放入寄存器 `$a0`，然后通过 `la.abs` + `jirl $r0`
    // 无返回跳转到 `__kernel_start_init`。此后，当前栈和代码段都可能被回收或
    // 覆盖，因此绝不可尝试返回到这个函数。
    unsafe extern "C" {
        fn skernel(); // 内核映像起始符号（链接脚本定义）
        fn ekernel(); // 内核映像结束符号
    }
    let start_context = {
        // 内核映像占用的物理地址范围（从虚拟地址反推物理地址）
        let kernel_phys_start = virt_to_phys(skernel as *const () as usize);
        let kernel_phys_end = virt_to_phys(ekernel as *const () as usize);
        let memory_map_snapshot = efi_stub::memory_map_snapshot();
        let memory_map_source = memory_map_snapshot.map(|snapshot| snapshot.source);
        let boot_map = if let Some(snapshot) = memory_map_snapshot {
            let regions = snapshot_boot_memory_regions(snapshot).unwrap_or_else(|err| {
                panic!("[loader] failed to normalize EFI memory map: {}", err)
            });
            printk!(
                "[loader] boot memory map normalized into {} platform regions ({})",
                regions.len(),
                match snapshot.source {
                    efi_stub::EfiMemoryMapSource::BootServicesExited => {
                        "post-ExitBootServices"
                    }
                    efi_stub::EfiMemoryMapSource::BootServicesActive => {
                        "observed-only; Boot Services still active"
                    }
                },
            );
            StartMemoryMap::Regions(regions)
        } else {
            StartMemoryMap::None
        };
        let acpi_enabled = KERNEL_FIRMWARE_STATE.acpi_enabled.load(Ordering::Acquire);
        let dtb_enabled = KERNEL_FIRMWARE_STATE.dtb_enabled.load(Ordering::Acquire);
        if acpi_enabled == dtb_enabled {
            panic!("[loader] expected exactly one firmware source to be selected");
        }

        // QEMU 的 LoongArch `-kernel` 直启路径会把 $a0 置为 1，并通过一个
        // EFI 兼容系统表暴露 FDT 配置表，但该伪交接没有 Boot Services，因而
        // 无法提供 GetMemoryMap。只有 ExitBootServices 成功后取得的内存图才是 RAM
        // 权威来源；BootServicesActive 图只能在非 stub DT 直启中作为额外白名单。
        let reported_efi_boot = EFI_BOOT.load(Ordering::Acquire) == 1;
        let entered_via_efi_stub = efi_stub::entered_via_efi_stub();
        let firmware_source = if acpi_enabled {
            StartFirmwareSource::Acpi
        } else {
            StartFirmwareSource::Dtb
        };
        let boot_protocol = select_effective_boot_protocol(
            reported_efi_boot,
            entered_via_efi_stub,
            memory_map_source,
            firmware_source,
        )
        .unwrap_or_else(|err| panic!("{}", err.message()));
        match memory_map_source {
            Some(efi_stub::EfiMemoryMapSource::BootServicesExited) if !reported_efi_boot => {
                printk!(
                    "[loader] post-ExitBootServices EFI memory map is present without the EFI boot flag; treating the handoff as EFI"
                );
            }
            Some(efi_stub::EfiMemoryMapSource::BootServicesActive) => {
                printk!(
                    "[loader] EFI memory map is observation-only because Boot Services remain active; treating DTB handoff as direct and constraining DT /memory with the observed map"
                );
            }
            None if reported_efi_boot => {
                printk!(
                    "[loader] EFI-compatible DTB handoff has no GetMemoryMap snapshot; treating boot as direct and using DT /memory"
                );
            }
            Some(efi_stub::EfiMemoryMapSource::BootServicesExited) | None => {}
        }

        let firmware = if acpi_enabled {
            if acpi_rsdp_for_state == 0 {
                panic!("[loader] ACPI selected but RSDP snapshot is missing");
            }
            StartFirmware::Acpi(StartAcpiTables {
                rsdp_phys: acpi_rsdp_for_state,
                mappings: kernel_acpi_mappings(),
            })
        } else {
            StartFirmware::Dtb(
                kernel_dtb()
                    .unwrap_or_else(|| panic!("[loader] DTB selected but snapshot missing")),
            )
        };
        let context = StartContext {
            boot: StartBootInfo {
                architecture: StartArchitecture::new("loongarch64"),
                protocol: boot_protocol,
                boot_cpu_id: LoongArch64MessageInterruptOps::current_cpu_id(),
                command_line: kernel_command_line(),
            },
            firmware,
            memory: StartMemory {
                kernel_image: StartPhysRange::new(kernel_phys_start, kernel_phys_end),
                boot_map,
            },
            address: StartAddressOps {
                phys_to_virt,
                virt_to_phys: kernel_virt_to_phys,
                device_mmio_to_virt: |phys_addr| DMW0_UNCACHED_BASE | phys_addr,
            },
            allocator: Some(StartAllocatorOps {
                kernel_heap_region,
                map_kernel_heap_range,
                unmap_kernel_heap_range,
                protect_kernel_heap_range,
                validate_kernel_heap_range,
                sync_icache,
                init_kernel_page_table,
                no_map: StartNoMapSupport::ReservedOnly {
                    granule: allocator::PAGE_SIZE,
                    mechanism: "LoongArch DMW0/DMW1 fixed windows cannot remove individual aliases",
                },
            }),
        };
        context
            .validate()
            .unwrap_or_else(|err| panic!("[loader] invalid StartContext: {}", err));
        context
    };
    let context_ptr = addr_of!(start_context) as *const StartContext as usize;
    printk!("[loader] Welcome to MyGO!!!!! OS");
    unsafe {
        // 将上下文指针放入 $a0 寄存器，然后绝对跳转到内核启动函数
        core::arch::asm!(
            "or $a0, {context}, $zero",
            "la.abs $r12, __kernel_start_init",
            "jirl $r0, $r12, 0",
            context = in(reg) context_ptr,
            options(noreturn),
        );
    }
}
