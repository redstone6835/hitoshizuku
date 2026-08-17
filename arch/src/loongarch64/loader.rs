//! 内核初始化逻辑。
//!
//! 本模块实现 LoongArch64 平台的内核早期初始化流程，由汇编引导代码在完成
//! BSS 清零和临时栈建立之后跳转至此。整个初始化过程分为若干阶段，按顺序
//! 依次执行：安装异常入口、建立早期日志、初始化引导分配器、解析启动来源
//! （U-Boot / QEMU 直启）并快照 DTB，最后构造一次性
//! 的 `StartContext` 并跳转到内核主函数 `__kernel_start_init`。
//!
//! 所有初始化步骤均为一次性执行，且控制权不会返回到此模块。全局状态通过
//! `KERNEL_FIRMWARE_STATE` 和 `KERNEL_FIRMWARE_BUFFERS` 保存，供后续
//! 构建 `StartContext` 时读取。
//!
//! 启动协议：只支持 U-Boot / 传统引导器直启。主线 LoongArch Linux 协议使用
//! `$a0..$a2`，UHI 直启把 FDT 放在 `$a1`；2K1000 板载 Loongson U-Boot 的
//! legacy ABI 则使用 `$a0=argc/$a1=argv/$a2=bootparam/$a3=FDT`。loader 对
//! 这些通道做协议识别和 FDT 校验。EFI Boot Services / ACPI 路径已删除。

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, compiler_fence};
use core::{fmt, fmt::Write};

use super::early_console::configure_early_console;
use crate::boot_protocol::FirmwareSnapshot;
use crate::*;
use efi::{EfiSystemTable, EfiSystemTableView};
use fdt::Fdt;
use general::{
    StartAddressOps, StartAllocatorOps, StartArchitecture, StartBootInfo, StartContext,
    StartFirmware, StartFirmwareSource, StartMemory, StartMemoryMap, StartMemoryRegion,
    StartMemoryRegionKind, StartNoMapSupport, StartPhysRange,
};
use log::printk;

// ─────────────────────────── 内部常量 ────────────────────────────────

/// 日志行缓冲区大小，足够容纳一条完整的日志（含时间戳和换行）。
const SINK_LINE_BUFFER_SIZE: usize = 1280;
/// 内核命令行字符串的最大长度（含终止 NUL）。
const CMDLINE_BUF_SIZE: usize = 4096;
/// DTB 快照缓冲区的最大容量（4 MiB）。
const DTB_BUF_SIZE: usize = 4096 * 1024;
/// Loongson U-Boot legacy ABI 支持的最大 argv 项数。
const LEGACY_ARGV_MAX_COUNT: usize = 256;
/// 2K1000 板载 U-Boot 为 legacy argv 指针表和字符串预留的内存大小。
const LEGACY_ARGV_AREA_SIZE: usize = 64 * 1024;
/// Loongson Boot Parameter Interface 签名前缀；末字节是版本 `0` 或 `1`。
const LOONGSON_BPI_SIGNATURE_PREFIX: &[u8; 7] = b"BPI0100";

/// 板级内置 DTB（2K1000LA 板 fork U-Boot 不通过寄存器传 DTB 时使用）。
///
/// 对应厂内核 CONFIG_BUILTIN_DTB 的同一机制：板载 fork U-Boot 的 `bootm`
/// 不显式传 fdt（a1/a2 均非 DTB），内核必须自带设备树。该 DTB 提取自
/// 厂内核 5.10 镜像内嵌的 LS2K1000-DP-V10 设备树。
#[cfg(mygo_la_board_ls2k1000)]
#[used]
#[unsafe(link_section = ".builtin_dtb")]
static BUILTIN_BOARD_DTB: &[u8] = include_bytes!("builtin/ls2k1000-dp.dtb");
/// 启动期可保留的最大板级内存区域数（2K1000 两段 DDR）。
const BOARD_MEMORY_REGION_CAPACITY: usize = 4;
/// 空的启动内存区域，占位用于静态数组初始化。
const EMPTY_BOOT_MEMORY_REGION: StartMemoryRegion = StartMemoryRegion::new(
    StartPhysRange::new(0, 0),
    StartMemoryRegionKind::Reserved,
    0,
);

/// 2K1000LA 开发板内存（fork U-Boot bdinfo 实测，工厂 DTB 无 /memory 节点）：
///
/// - bank0：物理 0x0000_0000 .. 0x1000_0000（低 256 MiB，内核装载于 0x200000）；
/// - bank1：物理 0x9000_0000 .. 0x1_0000_0000（高 1.75 GiB）。
///
/// DTB 无 /memory 节点时作为直启内存回退；QEMU virt 的 DTB 自带 /memory，
/// 不受影响。这是板级平台常量，不是 DTB 硬编码。
const BOARD_MEMORY_RANGES: &[StartPhysRange] = &[
    StartPhysRange::new(0x0000_0000, 0x1000_0000),
    StartPhysRange::new(0x9000_0000, 0x1_0000_0000),
];

/// 由 loader 持有的固件快照引擎。
///
/// 实现 [`FirmwareSnapshot`]，把启动协议适配器所需的快照原语桥接到 loader
/// 私有的静态缓冲区。适配器只定义"该做什么"，本引擎负责"怎么做"。
struct LoaderFirmwareSnapshot {
    /// EFI system table 指针（可能为 0；QEMU 伪 EFI 交接会提供，仅用于
    /// 读取配置表中的 FDT）。
    system_table: *mut EfiSystemTable,
    /// 最近一次从配置表读取的 FDT 物理地址。
    fdt_paddr: Option<usize>,
    /// 交接寄存器直传的 DTB 地址（Loongson legacy `$a3` 或 UHI `$a1` 等）。
    handoff_fdt: Option<usize>,
}

impl LoaderFirmwareSnapshot {
    /// 从 EFI 配置表采集 FDT 指针。
    fn collect_config_tables(&mut self) {
        if self.system_table.is_null() {
            return;
        }
        // Safety: system table 已在调用点通过 EfiSystemTableView 完整校验。
        let st = unsafe { &*self.system_table };
        self.fdt_paddr = unsafe { st.find_fdt() }.map(|p| p as usize);
    }
}

impl FirmwareSnapshot for LoaderFirmwareSnapshot {
    fn snapshot_dtb_from_paddr(&mut self, paddr: usize) -> Result<(), &'static str> {
        match store_kernel_dtb_from_address(paddr) {
            Ok(_) => Ok(()),
            Err(primary_error) => {
                let fallback = (self.fdt_paddr == Some(paddr))
                    .then_some(self.handoff_fdt)
                    .flatten()
                    .filter(|candidate| *candidate != paddr);
                let Some(fallback) = fallback else {
                    return Err(primary_error);
                };
                printk!(
                    "[loader][dtb] EFI DTB rejected: {}; trying register handoff at {:#x}",
                    primary_error,
                    fallback,
                );
                store_kernel_dtb_from_address(fallback).map(|_| ())
            }
        }
    }

    fn efi_fdt_paddr(&self) -> Option<usize> {
        // 优先 EFI 配置表暴露的 FDT（QEMU `-kernel` 直启）；否则使用启动
        // 寄存器中按协议识别出的 DTB。
        self.fdt_paddr.or(self.handoff_fdt)
    }

    fn select_firmware_source(&self, source: StartFirmwareSource) {
        let dtb_enabled = matches!(source, StartFirmwareSource::Dtb);
        KERNEL_FIRMWARE_STATE
            .dtb_enabled
            .store(dtb_enabled, Ordering::Release);
    }
}

/// 判断早期可访问地址处是否是一个有效 FDT 前缀（magic + 最小长度）。
///
/// 用于从启动交接参数中区分 DTB 与 cmdline / EFI system table：LoongArch 直启
/// 协议本身没有 DTB 通道，但板载 fork U-Boot 的 `bootm` 显式传入 fdt 参数时
/// 会把 DTB 放在 `$a1` 或 `$a2`；这里用 magic 做确定性识别，不做任何硬编码。
///
/// 早期处于 DMW 直映窗口，不会发生页表缺页；调用方仍须先按启动 ABI 限定候选
/// 寄存器，因为 DMW 映射本身不保证任意物理地址都由可读 RAM 支撑。
fn fdt_prefix_valid(vaddr: usize) -> bool {
    if vaddr == 0 {
        return false;
    }
    // Safety: DMW 窗口覆盖整个物理地址空间，前 8 字节读取不会 fault；
    // 只有 magic 完全匹配才继续按 FDT 解释。
    let prefix = unsafe { core::slice::from_raw_parts(vaddr as *const u8, 8) };
    let magic = u32::from_be_bytes(prefix[..4].try_into().unwrap_or([0u8; 4]));
    magic == fdt::DTB_MAGIC
}

/// 判断地址是否指向 Loongson BPI boot_params 头。
///
/// 厂商 legacy ABI 的 `$a2` 不是 EFI system table，而是以 `BPI01000` 或
/// `BPI01001` 开头的 boot_params；识别它后才能正确解释 `$a1/$a3`。
fn loongson_bpi_header_valid(vaddr: usize) -> bool {
    if vaddr == 0 {
        return false;
    }
    // Safety: 地址来自固件交接的 `$a2`，DMW1 已建立；这里只读取固定 8 字节
    // 签名，不跟随其中的任何指针。
    let signature = unsafe { core::slice::from_raw_parts(vaddr as *const u8, 8) };
    signature.starts_with(LOONGSON_BPI_SIGNATURE_PREFIX) && matches!(signature[7], b'0' | b'1')
}

/// 从 Loongson BPI boot_params 取得嵌入的 EFI system table 指针。
fn loongson_bpi_efi_system_table(vaddr: usize) -> Option<usize> {
    if !loongson_bpi_header_valid(vaddr) {
        return None;
    }
    // Safety: BPI 结构的偏移 8 是一个可能未对齐的 64 位 efitab 指针；头部已
    // 校验，使用 read_unaligned 避免对固件结构施加 Rust 对齐要求。
    let table = unsafe { (vaddr.wrapping_add(8) as *const usize).read_unaligned() };
    (table != 0).then_some(table)
}

/// 对固件启动寄存器做一次统一分类，避免早期控制台与正式快照采用不同规则。
#[derive(Clone, Copy)]
struct FirmwareHandoffProbe {
    raw_arg0: usize,
    raw_arg1: usize,
    raw_arg2: usize,
    raw_arg3: usize,
    canonical_arg1: usize,
    canonical_arg2: usize,
    canonical_arg3: usize,
    bpi_handoff: bool,
    legacy_argv: bool,
    a1_is_fdt: bool,
    a2_is_fdt: bool,
    a3_is_fdt: bool,
    raw_system_table: Option<usize>,
    handoff_fdt: Option<usize>,
}

/// 统一命令行快照的诊断状态。
#[derive(Clone, Copy)]
struct CommandLineSnapshotStatus {
    source: &'static str,
    truncated: bool,
    malformed: bool,
}

/// 识别 QEMU/Linux、UHI 与 Loongson legacy 三种启动寄存器布局。
///
/// 外部 DTB 的优先级固定为 EFI 配置表、legacy `$a3`、UHI `$a1`、兼容
/// `$a2`。EFI 配置表在后续解析，此处只选择寄存器直传候选。
fn probe_firmware_handoff() -> FirmwareHandoffProbe {
    let raw_arg0 = EFI_BOOT.load(Ordering::Acquire);
    let raw_arg1 = CMDLINE_PTR.load(Ordering::Acquire);
    let raw_arg2 = EFI_SYSTEM_TABLE_PTR.load(Ordering::Acquire);
    let raw_arg3 = FIRMWARE_ARG3.load(Ordering::Acquire);

    let canonical_arg1 = reset_to_virt(raw_arg1);
    let canonical_arg2 = if raw_arg2 == 0 {
        0
    } else {
        reset_to_virt(raw_arg2)
    };
    let canonical_arg3 = if raw_arg3 == 0 {
        0
    } else {
        reset_to_virt(raw_arg3)
    };

    let bpi_handoff = loongson_bpi_header_valid(canonical_arg2);
    let legacy_argv =
        bpi_handoff && raw_arg1 != 0 && (1..=LEGACY_ARGV_MAX_COUNT).contains(&raw_arg0);
    let a1_is_fdt = !bpi_handoff && fdt_prefix_valid(canonical_arg1);
    let a2_is_fdt = !bpi_handoff && fdt_prefix_valid(canonical_arg2);
    let a3_is_fdt = bpi_handoff && fdt_prefix_valid(canonical_arg3);

    let raw_system_table = if bpi_handoff {
        loongson_bpi_efi_system_table(canonical_arg2)
    } else if raw_arg2 != 0 && !a2_is_fdt {
        Some(raw_arg2)
    } else {
        None
    };
    let handoff_fdt = a3_is_fdt
        .then_some(raw_arg3)
        .or_else(|| a1_is_fdt.then_some(raw_arg1))
        .or_else(|| a2_is_fdt.then_some(raw_arg2));

    FirmwareHandoffProbe {
        raw_arg0,
        raw_arg1,
        raw_arg2,
        raw_arg3,
        canonical_arg1,
        canonical_arg2,
        canonical_arg3,
        bpi_handoff,
        legacy_argv,
        a1_is_fdt,
        a2_is_fdt,
        a3_is_fdt,
        raw_system_table,
        handoff_fdt,
    }
}

// ─────────────────────── 固件状态与缓冲区 ─────────────────────────────

/// 全局固件状态，使用原子变量存储，可在启动早期安全访问。
///
/// 包含 DTB 是否启用、命令行长度、DTB 快照长度。所有字段的写入均使用
/// `Release` 排序，读取使用 `Acquire` 排序，以保证在跳转到内核启动代码
/// 之前对这些字段的修改可见。
struct KernelFirmwareState {
    /// DTB 是否被选为固件解析来源。
    dtb_enabled: AtomicBool,
    /// 内核命令行的有效长度（不含终止 NUL）。
    cmdline_valid_len: AtomicUsize,
    /// DTB 快照的有效字节数。
    dtb_valid_len: AtomicUsize,
}

impl KernelFirmwareState {
    /// 使用默认值（全部清零）创建实例。
    const fn new() -> Self {
        Self {
            dtb_enabled: AtomicBool::new(false),
            cmdline_valid_len: AtomicUsize::new(0),
            dtb_valid_len: AtomicUsize::new(0),
        }
    }

    /// 重置固件选择状态和快照长度，为重新选择做准备。
    fn reset_selection(&self) {
        self.dtb_enabled.store(false, Ordering::Release);
        self.dtb_valid_len.store(0, Ordering::Release);
    }
}

/// 全局固件缓冲区，存储命令行和 DTB 快照。
///
/// 直接定义为静态数组；`unsafe` 访问通过指针进行，但仅在初始化阶段
/// 单线程执行，无竞争。
struct KernelFirmwareBuffers {
    /// 内核命令行缓冲区，以 NUL 终止。
    command_line: [u8; CMDLINE_BUF_SIZE],
    /// DTB 快照缓冲区。
    dtb: [u8; DTB_BUF_SIZE],
}

impl KernelFirmwareBuffers {
    /// 创建所有缓冲区的零初始化实例。
    const fn new() -> Self {
        Self {
            command_line: [0u8; CMDLINE_BUF_SIZE],
            dtb: [0u8; DTB_BUF_SIZE],
        }
    }
}

/// 内核固件状态的全局实例。
static KERNEL_FIRMWARE_STATE: KernelFirmwareState = KernelFirmwareState::new();
/// 内核固件缓冲区的可变全局实例，仅在初始化阶段访问。
static mut KERNEL_FIRMWARE_BUFFERS: KernelFirmwareBuffers = KernelFirmwareBuffers::new();
/// 板级内存回退的归一化启动内存区域。
static mut BOARD_MEMORY_REGIONS: [StartMemoryRegion; BOARD_MEMORY_REGION_CAPACITY] =
    [EMPTY_BOOT_MEMORY_REGION; BOARD_MEMORY_REGION_CAPACITY];

// ─────────────────────── 辅助函数 ─────────────────────────────────────// ─────────────────────── 辅助函数 ─────────────────────────────────────

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

/// 判断 DTB 是否自带可用的 /memory 描述。
///
/// 2K1000LA 工厂 DTB 没有 memory 节点（内存信息只存在于 bootloader）；
/// QEMU virt 的 DTB 自带。与 fdt 解析器 `memory_banks` 的语义一致：遍历根
/// 节点直接子节点中 `device_type` 精确为 `memory` 的节点（QEMU LA64 有两个
/// memory 节点，因此不能用 `find_node("/memory")` 的消歧匹配），并要求 `reg`
/// 长度至少覆盖地址+大小各一个 cell（2×64 位 = 16 字节）。
fn dtb_describes_memory(dtb: &Fdt<'_>) -> bool {
    for node in dtb.root().children() {
        let Some(device_type) = node.property("device_type") else {
            continue;
        };
        if device_type.as_str().is_ok_and(|value| value == "memory")
            && node
                .property("reg")
                .is_some_and(|reg| reg.value().len() >= 16)
        {
            return true;
        }
    }
    false
}

/// 构造板级内存回退映射：DTB 无 /memory 节点（2K1000 工厂 DTB）时，使用
/// fork U-Boot bdinfo 实测的 DDR 布局（`BOARD_MEMORY_RANGES`）。
fn board_boot_memory_map() -> StartMemoryMap {
    let regions_ptr = addr_of_mut!(BOARD_MEMORY_REGIONS).cast::<StartMemoryRegion>();
    let mut count = 0usize;
    for range in BOARD_MEMORY_RANGES {
        if count >= BOARD_MEMORY_REGION_CAPACITY {
            break;
        }
        // Safety: count < BOARD_MEMORY_REGION_CAPACITY 保证写入在数组内；
        // 单线程启动阶段没有其他访问者，先填满全部元素再发布切片。
        unsafe {
            regions_ptr.add(count).write(StartMemoryRegion::new(
                *range,
                StartMemoryRegionKind::UsableRam,
                0,
            ));
        }
        count += 1;
    }
    // Safety: 已填写的 count 个元素都是完整初始化的 StartMemoryRegion，且
    // 静态数组具有 'static 生命周期。
    let slice = unsafe {
        core::slice::from_raw_parts(
            addr_of!(BOARD_MEMORY_REGIONS).cast::<StartMemoryRegion>(),
            count,
        )
    };
    let total_mib = BOARD_MEMORY_RANGES
        .iter()
        .map(|range| (range.end - range.start) / (1024 * 1024))
        .sum::<usize>();
    printk!(
        "[loader] DTB has no /memory; using 2K1000 board memory: {} regions ({} MiB)",
        count,
        total_mib,
    );
    StartMemoryMap::Regions(slice)
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

/// 在堆初始化之前从固件交接借用 FDT，仅用于选择最早期控制台。
///
/// 该视图不会被保存；正式固件选择仍在退出 Boot Services 前完成私有快照。
fn early_firmware_dtb(probe: &FirmwareHandoffProbe) -> Option<Fdt<'static>> {
    let (fdt, _) = early_firmware_dtb_with_addr(probe)?;
    Some(fdt)
}

/// 从固件交接取 FDT 视图及其原始地址。
///
/// 优先使用 EFI 配置表；随后检查 Loongson legacy `$a3`、UHI `$a1` 和兼容
/// `$a2` 通道。返回的地址用于正式路径把 FDT 快照进内核缓冲区。
fn early_firmware_dtb_with_addr(probe: &FirmwareHandoffProbe) -> Option<(Fdt<'static>, usize)> {
    fn parse_at(raw_fdt: usize) -> Option<(Fdt<'static>, usize)> {
        let bytes = firmware_dtb_bytes(reset_to_virt(raw_fdt)).ok()?;
        Fdt::parse(bytes).ok().map(|fdt| (fdt, raw_fdt))
    }

    if let Some(raw_system_table) = probe.raw_system_table {
        let canonical_system_table = reset_to_virt(raw_system_table);
        // Safety: DMW 已由入口汇编建立；`from_ptr` 会校验 EFI system table
        // 头部、对齐和签名，失败时不解引用其配置表。
        if let Some(view) = unsafe { EfiSystemTableView::from_ptr(canonical_system_table) } {
            // Safety: system table 已通过完整视图校验；这里只提取标准 FDT GUID
            // 对应的指针。
            if let Some(raw_fdt) = unsafe { view.table().find_fdt() }.map(|ptr| ptr as usize)
                && let Some(fdt) = parse_at(raw_fdt)
            {
                return Some(fdt);
            }
        }
    }

    probe.handoff_fdt.and_then(parse_at)
}

/// 清空内核命令行快照。
fn clear_kernel_command_line() {
    // Safety: 命令行缓冲区只在单线程启动阶段写入；先清首字节，再以 Release
    // 发布零长度，后续读者不会看到旧内容。
    unsafe {
        addr_of_mut!(KERNEL_FIRMWARE_BUFFERS.command_line)
            .cast::<u8>()
            .write(0);
    }
    KERNEL_FIRMWARE_STATE
        .cmdline_valid_len
        .store(0, Ordering::Release);
}

/// 将原始命令行地址复制到内核命令行缓冲区，并返回是否发生截断。
///
/// 复制时会扫描到 NUL 终止符或达到最大长度为止，并保证以 NUL 结尾。
///
/// 在当前平台上，`raw_ptr == 0` 仍可能是有效的命令行地址，因此这里
/// 不能把 0 直接解释为“没有命令行”。
fn store_kernel_command_line(raw_ptr: usize) -> bool {
    clear_kernel_command_line();

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

    len + 1 == CMDLINE_BUF_SIZE
}

/// 按 Loongson legacy ABI 把 `$a1=argv` 的 `argv[1..argc]` 拼成标准命令行。
///
/// 返回 `(truncated, malformed)`。指针必须落在该板 U-Boot 为 boot parameters
/// 分配的 64 KiB 区域内，避免把损坏的 argv 项继续解释成任意物理地址。
fn store_legacy_kernel_command_line(argc: usize, raw_argv: usize) -> (bool, bool) {
    clear_kernel_command_line();
    if raw_argv == 0 {
        return (false, true);
    }
    let argv = reset_to_virt(raw_argv) as *const usize;
    // Safety: 命令行缓冲区只在单线程启动阶段写入，地址在内核生命周期内稳定。
    let dst = unsafe { addr_of_mut!(KERNEL_FIRMWARE_BUFFERS.command_line).cast::<u8>() };
    let mut len = 0usize;
    let mut truncated = false;
    let mut malformed = false;

    let argv_phys = virt_to_phys(raw_argv);
    let Some(pointer_table_size) = LEGACY_ARGV_MAX_COUNT.checked_mul(core::mem::size_of::<usize>())
    else {
        return (false, true);
    };
    let Some(strings_start) = argv_phys.checked_add(pointer_table_size) else {
        return (false, true);
    };
    let Some(argv_area_end) = argv_phys.checked_add(LEGACY_ARGV_AREA_SIZE) else {
        return (false, true);
    };
    // Safety: legacy ABI 保证 `$a1` 指向 256 项 argv 表，且 argv[0] 固定为 NULL。
    if unsafe { argv.read_unaligned() } != 0 {
        return (false, true);
    }

    'arguments: for index in 1..argc.min(LEGACY_ARGV_MAX_COUNT) {
        // Safety: `$a1` 来自经 BPI + argc 范围确认的 Loongson U-Boot legacy
        // 交接，指向至少 argc 个 argv 指针；使用未对齐读取兼容固件布局。
        let raw_argument = unsafe { argv.add(index).read_unaligned() };
        if raw_argument == 0 {
            malformed = true;
            break;
        }
        let argument_phys = virt_to_phys(raw_argument);
        if !(strings_start..argv_area_end).contains(&argument_phys) {
            malformed = true;
            break;
        }
        let argument = reset_to_virt(raw_argument) as *const u8;
        let source_capacity = argv_area_end - argument_phys;
        let mut offset = 0usize;
        let mut wrote_argument = false;
        let mut terminated = false;
        while offset < source_capacity {
            // Safety: argv 项由 U-Boot 构造为 NUL 结尾字符串，且只在启动交接
            // 内存仍有效时读取；输出始终保留一个字节写终止 NUL。
            let byte = unsafe { argument.add(offset).read() };
            if byte == 0 {
                terminated = true;
                break;
            }
            if !wrote_argument && len != 0 {
                if len + 2 >= CMDLINE_BUF_SIZE {
                    truncated = true;
                    break 'arguments;
                }
                unsafe { dst.add(len).write(b' ') };
                len += 1;
            }
            if len + 1 >= CMDLINE_BUF_SIZE {
                truncated = true;
                break 'arguments;
            }
            unsafe { dst.add(len).write(byte) };
            len += 1;
            offset += 1;
            wrote_argument = true;
        }
        if !terminated {
            malformed = true;
            break;
        }
    }

    if malformed {
        len = 0;
    }
    // Safety: len 始终小于 CMDLINE_BUF_SIZE，目标是启动期独占的静态缓冲区；
    // malformed 时写回首字节 NUL，确保不会采用仅部分验证的启动参数。
    unsafe { dst.add(len).write(0) };
    KERNEL_FIRMWARE_STATE
        .cmdline_valid_len
        .store(len, Ordering::Release);
    (truncated, malformed)
}

/// 按探测到的启动 ABI 建立统一命令行快照。
fn snapshot_handoff_command_line(probe: &FirmwareHandoffProbe) -> CommandLineSnapshotStatus {
    if probe.bpi_handoff {
        if probe.legacy_argv {
            let (truncated, malformed) =
                store_legacy_kernel_command_line(probe.raw_arg0, probe.raw_arg1);
            CommandLineSnapshotStatus {
                source: "loongson-legacy-argv",
                truncated,
                malformed,
            }
        } else {
            clear_kernel_command_line();
            CommandLineSnapshotStatus {
                source: "loongson-legacy-invalid",
                truncated: false,
                malformed: true,
            }
        }
    } else if probe.a1_is_fdt {
        clear_kernel_command_line();
        CommandLineSnapshotStatus {
            source: "none (a1 is FDT)",
            truncated: false,
            malformed: false,
        }
    } else {
        CommandLineSnapshotStatus {
            source: "flat-a1",
            truncated: store_kernel_command_line(probe.raw_arg1),
            malformed: false,
        }
    }
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

/// 把板级内置 DTB 快照进内核缓冲区（ls2k1000 板专用回退）。
///
/// 板载 fork U-Boot 的 `bootm` 不通过寄存器或 EFI 配置表传递 DTB；厂内核
/// 以 CONFIG_BUILTIN_DTB 解决同一问题。这里在无任何交接 DTB 时把内置
/// 设备树复制进内核 DTB 缓冲区，供后续协议与启动上下文使用。
#[cfg(mygo_la_board_ls2k1000)]
fn snapshot_builtin_board_dtb() -> Option<Fdt<'static>> {
    let fdt = Fdt::parse(BUILTIN_BOARD_DTB).ok()?;
    let dtb_bytes = fdt.as_bytes();
    let dtb_size = dtb_bytes.len();
    if dtb_size > DTB_BUF_SIZE {
        return None;
    }
    // Safety: 内置 DTB 是编译期常量，长度不超过目标静态缓冲区；该复制发生在
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
        "[loader][dtb] builtin board DTB copied to kernel buffer: size={} bytes",
        dtb_size,
    );
    kernel_dtb()
}

/// 非板级构建：无内置 DTB 回退。
#[cfg(not(mygo_la_board_ls2k1000))]
fn snapshot_builtin_board_dtb() -> Option<Fdt<'static>> {
    None
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
/// 板级调试标记（MYGO_BOARD_DEBUG_UART=1 构建时启用）：向 2K1000LA UART0
/// （物理 0x1fe20000，经 DMW0 非缓存窗口 0x8000_0000_0000_0000）裸写一个
/// 字符，用于定位启动链卡点。非调试构建为空操作。仅调试用途。
pub(crate) fn debug_mark(byte: u8) {
    #[cfg(mygo_board_debug_uart)]
    {
        const DEBUG_UART: usize = 0x8000_0000_1fe2_0000usize;
        // Safety: UART0 是板级固定 MMIO 外设，DMW0 直映窗口保证可访问；
        // 轮询 LSR[5]（THRE）避免覆盖未发送完的字符。
        while unsafe { core::ptr::read_volatile((DEBUG_UART + 5) as *const u8) } & 0x20 == 0 {}
        unsafe { core::ptr::write_volatile((DEBUG_UART + 0) as *mut u8, byte) };
    }
    #[cfg(not(mygo_board_debug_uart))]
    {
        let _ = byte;
    }
}

pub unsafe extern "C" fn __kernel_arch_loader() {
    // 内存原语默认使用启动安全的字节路径；读取能力后再开放非对齐快路径。
    super::mem::init_ual();
    debug_mark(b'1'); // loader 入口

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
    debug_mark(b'2'); // 异常入口安装

    // BSS、DMW 和临时栈此时已经可用，但堆尚未建立。先统一识别启动寄存器布局，
    // 并把 flat cmdline 或 legacy argv 快照到内核缓冲区。earlycon、timer_hz 与最终
    // StartContext 都只消费这份快照，避免把 legacy `$a1=argv` 误当字符串。
    let handoff_probe = probe_firmware_handoff();
    let command_line_snapshot = snapshot_handoff_command_line(&handoff_probe);
    let cmdline_earlycon = kernel_command_line()
        .and_then(|bytes| general::cmdline::Cmdline::new(bytes).find("earlycon"));
    let early_console =
        configure_early_console(early_firmware_dtb(&handoff_probe), cmdline_earlycon);
    debug_mark(b'3'); // early console 配置

    e_print(format_args!(
        "[{:6}.{:06}] [boot] Entering kernel loader...\n",
        0, 0
    ));
    e_print(format_args!(
        "[{:6}.{:06}] [boot] early console: source={} phys={:#x} clock={} baud={} reg-offset={:#x} reg-shift={} reg-io-width={} endian={:?}{}\n",
        0,
        0,
        early_console.source.name(),
        early_console.config.phys_base,
        early_console.config.clock_hz,
        early_console.config.baud,
        early_console.config.reg_offset,
        early_console.config.reg_shift,
        early_console.config.io_width.bytes(),
        early_console.config.endian,
        if early_console.dt_error.is_some() {
            " (DT rejected)"
        } else {
            ""
        },
    ));
    if let Some(error) = early_console.dt_error {
        e_print(format_args!(
            "[{:6}.{:06}] [boot] early console candidate rejected: {:?}\n",
            0, 0, error
        ));
    }

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
        //
        // 与 earlycon 解析相同，只读取按启动 ABI 归一化后的内核命令行快照。
        let timer_hz = {
            let mut hz = DEFAULT_TIMER_HZ;
            let configured = kernel_command_line()
                .and_then(|bytes| general::cmdline::Cmdline::new(bytes).find("timer_hz"))
                .and_then(|value| value.parse().ok());
            if let Some(val) = configured {
                hz = val;
            }
            hz.max(1).min(10000)
        };
        configure_local_timer(timer_hz);
        debug_mark(b'4'); // 定时器配置
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
        debug_mark(b'5'); // 日志绑定
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
        allocator::KERNEL_ALLOCATOR.init_boot(heap_start, heap_size);
        debug_mark(b'6'); // boot allocator

        printk!(
            "[loader] boot allocator: {:#x}..{:#x} ({} MiB)",
            heap_start,
            heap_end,
            heap_size / (1024 * 1024),
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 3：采集启动来源参数并探测交接 DTB
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 探测已在堆初始化前完成，此处只记录结果并验证选出的 EFI system table。
    // Loongson legacy 模式下 `$a2` 是 BPI 结构而不是系统表，真正的系统表地址
    // 来自 BPI 偏移 8；DTB 则优先从该表的配置表取得，再回退到 `$a3`。
    debug_mark(b'7'); // handoff 探测
    printk!(
        "[loader] handoff probe: a0={} a1={:#x}->{:#x} a2={:#x}->{:#x} a3={:#x}->{:#x} bpi_handoff={} legacy_argv={} a1_is_fdt={} a2_is_fdt={} a3_is_fdt={}",
        handoff_probe.raw_arg0,
        handoff_probe.raw_arg1,
        handoff_probe.canonical_arg1,
        handoff_probe.raw_arg2,
        handoff_probe.canonical_arg2,
        handoff_probe.raw_arg3,
        handoff_probe.canonical_arg3,
        handoff_probe.bpi_handoff,
        handoff_probe.legacy_argv,
        handoff_probe.a1_is_fdt,
        handoff_probe.a2_is_fdt,
        handoff_probe.a3_is_fdt,
    );
    if command_line_snapshot.truncated {
        printk!(
            "[loader] boot command line truncated to {} bytes",
            CMDLINE_BUF_SIZE - 1,
        );
    }
    if command_line_snapshot.malformed {
        printk!("[loader] malformed legacy argv; command line discarded");
    }
    printk!(
        "[loader] boot args: source={} cmdline={:?}",
        command_line_snapshot.source,
        kernel_command_line().map(|bytes| core::str::from_utf8(bytes).unwrap_or("<invalid UTF-8>")),
    );

    let raw_system_table = handoff_probe.raw_system_table;
    let canonical_system_table = raw_system_table.map(reset_to_virt);
    let fw_view = canonical_system_table.and_then(|address| {
        // Safety: 候选地址来自已识别的 `$a2` 或 BPI efitab；DMW 已建立，
        // `from_ptr` 会在产生视图前校验 EFI system table 头部与签名。
        unsafe { EfiSystemTableView::from_ptr(address) }
    });
    if let Some(raw_system_table) = raw_system_table
        && fw_view.is_none()
    {
        printk!(
            "[loader] EFI system table pointer invalid: raw={:#x} canonical={:#x}",
            raw_system_table,
            canonical_system_table.unwrap_or(0),
        );
    }

    let fw_table = if let Some(fw_view) = fw_view {
        printk!(
            "[loader] EFI system table address accepted: raw={:#x} canonical={:#x} via={}",
            raw_system_table.unwrap_or(0),
            canonical_system_table.unwrap_or(0),
            if handoff_probe.bpi_handoff {
                "bpi"
            } else {
                "a2"
            },
        );
        Some(fw_view.as_ptr() as *mut EfiSystemTable)
    } else {
        None
    };

    // ═══════════════════════════════════════════════════════════════════════════
    // 步骤 3.1：固件表选择、完成 DTB 私有快照
    // ═══════════════════════════════════════════════════════════════════════════
    //
    // 本内核只支持 DTB 固件路径（U-Boot / QEMU 直启；ACPI 与 EFI Boot Services
    // 已随 U-Boot 直启改造删除）。`BootProtocolDispatcher` 依据 `_start` 原始
    // 参数做协议分类，快照引擎把固件暴露的 DTB 复制进内核私有缓冲区：
    //   - QEMU `-kernel` 直启：FDT 经伪 EFI 配置表暴露（fw_table 非空）；
    //   - 板载 fork U-Boot：先查 BPI EFI 配置表，再查 legacy `$a3`；
    //   - UHI/兼容直启：DTB 经 `$a1`/`$a2` 直传（handoff_fdt 命中）。
    // 两类来源都只读交接，快照由 dispatcher 驱动，不依赖任何 DTB 硬编码。
    KERNEL_FIRMWARE_STATE.reset_selection();
    let dispatcher =
        crate::boot_protocol::BootProtocolDispatcher::new(crate::loongarch64::boot_registers());
    let mut snapshot_engine = LoaderFirmwareSnapshot {
        system_table: fw_table.unwrap_or(core::ptr::null_mut()),
        fdt_paddr: None,
        handoff_fdt: handoff_probe.handoff_fdt,
    };
    snapshot_engine.collect_config_tables();
    debug_mark(b'8'); // 固件快照
    let firmware_handoff = match dispatcher.dispatch(&mut snapshot_engine) {
        Ok(handoff) => handoff,
        Err(err) => {
            // 外部候选可能只有正确 magic、但完整 FDT 结构已损坏。清除任何未完成
            // 的快照并保留 DTB 固件模式，让板级内置 DTB 成为最终回退。
            KERNEL_FIRMWARE_STATE
                .dtb_valid_len
                .store(0, Ordering::Release);
            snapshot_engine.select_firmware_source(StartFirmwareSource::Dtb);
            printk!(
                "[loader][dtb] all external DTB candidates rejected: {}; using board fallback",
                err,
            );
            crate::boot_protocol::FirmwareHandoff::dtb_source()
        }
    };
    printk!(
        "[loader] firmware selection: DTB via {} adapter",
        firmware_handoff.adapter,
    );
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
    //   - firmware      : 选定的固件表格（本路径固定为 DTB 及其私有快照）。
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
        // DTB 私有快照：dispatcher 已把固件暴露的 DTB 复制进内核缓冲区。若
        // 两个交接通道都未命中（例如板载 U-Boot 的 bootm 未显式传 fdt），
        // ls2k1000 板回退到内置设备树（厂内核 CONFIG_BUILTIN_DTB 的同一机制）；
        // 其他平台直接失败并给出可操作的修复提示。
        let dtb = kernel_dtb()
            .or_else(snapshot_builtin_board_dtb)
            .unwrap_or_else(|| {
                panic!(
                    "[loader] no device tree in handoff (bpi={} a1_fdt={} a2_fdt={} a3_fdt={}); \
                 load a valid DTB at U-Boot fdt_addr before bootm",
                    handoff_probe.bpi_handoff,
                    handoff_probe.a1_is_fdt,
                    handoff_probe.a2_is_fdt,
                    handoff_probe.a3_is_fdt,
                )
            });
        // 板级内存回退：2K1000 工厂 DTB 没有 /memory 节点（内存信息只存在于
        // bootloader），此时以 fork U-Boot bdinfo 实测的板级 DDR 布局
        // （BOARD_MEMORY_RANGES）作为启动内存映射；DTB 自带 /memory（QEMU
        // virt）时交给内核按 DTB 描述建立（boot_map=None）。
        let boot_map = if dtb_describes_memory(&dtb) {
            printk!("[loader] DTB describes /memory; kernel will use the DT memory map");
            StartMemoryMap::None
        } else {
            board_boot_memory_map()
        };
        let dtb_command_line = dtb
            .chosen_bootargs()
            .unwrap_or_else(|error| panic!("[loader] invalid /chosen/bootargs: {:?}", error))
            .map(str::as_bytes);
        let command_line = kernel_command_line().or(dtb_command_line);
        if kernel_command_line().is_none()
            && let Some(command_line) = dtb_command_line
        {
            printk!(
                "[loader] command line from DTB: {}",
                core::str::from_utf8(command_line).unwrap_or("<invalid UTF-8>")
            );
        }
        let context = StartContext {
            boot: StartBootInfo {
                architecture: StartArchitecture::new("loongarch64"),
                protocol: firmware_handoff.protocol,
                boot_cpu_id: LoongArch64MessageInterruptOps::current_cpu_id(),
                command_line,
            },
            firmware: StartFirmware::Dtb(dtb),
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
                tracked_heap_region,
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
    debug_mark(b'9'); // StartContext 就绪
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
