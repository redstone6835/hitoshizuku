//! 内核启动上下文，供架构相关引导代码与 `kernel` 共享。
//!
//! `StartContext` 是架构加载器在 CPU、栈、早期分配器和固件快照均可用之后
//! 产出的交接对象。内核消费该对象以完成平台无关的后续启动工作：
//! 固件解析、内存子系统激活、设备注册以及控制台绑定。
//!
//! 设计规则：
//! - 上下文描述的是已固化的能力与固件视图，不得编码 ISA 特定的常量，
//!   比如具体架构的早期直映窗口前缀。
//! - 架构侧负责发现、复制及生命周期稳定化。
//!   内核侧负责解释与策略。
//! - 此处存储的指针与切片在整个启动过程中必须保持有效。
//!   当前加载器通过在跳转到 `__kernel_start_init` 之前将快照存入
//!   内核静态内存来满足这一要求。

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

use allocator::{
    KernelHeapRegionFn, MapKernelHeapRangeFn, MemorySegment, PhysToVirtFn, UnmapKernelHeapRangeFn,
    VirtToPhysFn,
};

use crate::ArchitectureId;
use crate::firmware::{FirmwareTableMapping, normalize_segments};
use fdt::Fdt;

/// 将设备 MMIO 物理地址转换为内核虚拟地址，该虚拟地址用于
/// 易失性寄存器访问。
///
/// 有意不采用任何具体架构早期映射或 ioremap 等机制来命名。
/// 各架构自行选择实现方式。
pub type DeviceMmioToVirtFn = fn(phys_addr: usize) -> usize;

/// 架构在线性内存映射中准备 `no-map` 洞的回调。
pub type PrepareNoMapFn = fn(&[StartPhysRange]) -> Result<(), StartNoMapError>;

/// 线性映射实现准备 `no-map` 范围时可能返回的错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartNoMapError {
    /// 范围不满足架构页表的地址/对齐不变量。
    InvalidRange,
    /// 范围覆盖了内核仍需访问的启动映射。
    OverlapsKernelImage,
    /// 启动堆无法保存范围快照或为正式线性映射分配页表页。
    OutOfMemory,
}

/// 架构对标准 RAM 线性映射的能力声明。
#[derive(Clone, Copy)]
pub enum StartNoMapSupport {
    /// 当前启动协议没有需要处理的标准线性映射约束。
    None,
    /// 架构可以按至少 `granule` 粒度落实范围；回调必须在物理 allocator
    /// 初始化前完成约束登记，正式页表建立时消费同一份登记。
    Enforced {
        granule: usize,
        prepare: PrepareNoMapFn,
    },
    /// 架构会按 `granule` 从物理 allocator 排除范围，但固定直映窗口无法移除
    /// 对应虚拟别名。这与 Linux LoongArch 的 DMW 内存保留模型一致；调用方必须
    /// 明确记录该限制，且普通 RAM API 不得主动访问这些范围。
    ReservedOnly {
        granule: usize,
        mechanism: &'static str,
    },
    /// 架构的固定窗口无法挖洞。遇到有效 `no-map` 时启动层必须 fail-closed。
    Unsupported { mechanism: &'static str },
}

impl StartNoMapSupport {
    /// 返回落实物理页保留所需的粒度。
    pub const fn granule(self) -> Option<usize> {
        match self {
            Self::Enforced { granule, .. } | Self::ReservedOnly { granule, .. } => Some(granule),
            Self::None | Self::Unsupported { .. } => None,
        }
    }
}

/// 安装内核堆所需的架构页表状态。
///
/// 该回调在 [`StartAllocatorOps`] 中是可选的，因为某些早期移植版本
/// 在启动阶段可能在没有可分页内核堆的情况下运行。成熟的平台
/// 应在固件暴露可用物理内存时提供该项。
pub type InitKernelPageTableFn = fn();
pub type ProtectKernelHeapRangeFn =
    fn(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool;
pub type ValidateKernelHeapRangeFn =
    fn(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool;
pub type SyncIcacheFn = fn();

/// 使用排他上界的物理地址范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartPhysRange {
    /// 物理起始地址（包含）。
    pub start: usize,
    /// 物理结束地址（不包含）。
    pub end: usize,
}

impl StartPhysRange {
    /// 创建一个物理地址范围。调用者负责传入满足
    /// `start <= end` 的有序范围。
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// 以分配器当前消费的元组形式返回该范围。
    pub const fn as_tuple(self) -> (usize, usize) {
        (self.start, self.end)
    }
}

/// 启动交接中的架构身份。保留旧名称以避免启动协议调用方重复定义一套类型。
pub type StartArchitecture = ArchitectureId;

/// 进入内核所使用的外部启动协议或加载器路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartBootProtocol {
    /// 通过 EFI/UEFI 镜像或 EFI 兼容的交接进入。
    Efi,
    /// 通过 Linux 启动协议或 Linux 兼容桩进入。
    LinuxBoot,
    /// 通过 Multiboot2 进入。
    Multiboot2,
    /// 通过 coreboot 表或 coreboot 载荷交接进入。
    Coreboot,
    /// 由虚拟机或固件提供的内核入口直接进入。
    Direct,
    /// 通过此处未建模的架构特定协议进入。
    Other,
    /// 加载器无法对启动协议进行分类。
    Unknown,
}

/// 在用户态或调度器启动之前有用的基本启动元数据。
#[derive(Clone, Copy, Debug)]
pub struct StartBootInfo {
    /// 正在运行的内核镜像的指令集架构。
    pub architecture: StartArchitecture,
    /// 本次启动所使用的加载器协议。
    pub protocol: StartBootProtocol,
    /// 启动处理器（boot processor）的硬件标识符。
    ///
    /// 该值由架构定义。内核启动代码可在固件表描述完整的 CPU 拓扑之前
    /// 将其用于早期 per-CPU 绑定。
    pub boot_cpu_id: usize,
    /// 可选的命令行字节序列，前提是架构加载器能提供稳定的副本。
    /// 字节序列不要求是 UTF-8 编码。
    pub command_line: Option<&'static [u8]>,
}

/// 架构初始化选择的固件源，用于平台解析。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFirmwareSource {
    /// 解析 DTB 视图。
    Dtb,
    /// 解析 ACPI 视图。
    Acpi,
}

impl StartFirmwareSource {
    /// 返回该固件来源的人类可读名称。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dtb => "dtb",
            Self::Acpi => "acpi",
        }
    }
}

/// 架构向 AML 解释器提供的 SystemIO 端口访问入口。
#[derive(Clone, Copy)]
pub struct StartAcpiIoOps {
    pub read_u8: fn(u16) -> u8,
    pub read_u16: fn(u16) -> u16,
    pub read_u32: fn(u16) -> u32,
    pub write_u8: fn(u16, u8),
    pub write_u16: fn(u16, u16),
    pub write_u32: fn(u16, u32),
}

/// 架构向 AML 解释器提供的 PCI 配置空间访问入口。
///
/// 参数依次为 segment、bus、device、function、register offset。支持 ECAM 的平台可以
/// 不提供本组回调，内核会优先从 MCFG 建立配置空间映射；没有 MCFG 的平台则必须提供
/// 回调后才能执行访问 PCI OperationRegion 的 AML 方法。
#[derive(Clone, Copy)]
pub struct StartAcpiPciOps {
    pub read_u8: fn(u16, u8, u8, u8, u16) -> u8,
    pub read_u16: fn(u16, u8, u8, u8, u16) -> u16,
    pub read_u32: fn(u16, u8, u8, u8, u16) -> u32,
    pub write_u8: fn(u16, u8, u8, u8, u16, u8),
    pub write_u16: fn(u16, u8, u8, u8, u16, u16),
    pub write_u32: fn(u16, u8, u8, u8, u16, u32),
}

/// AML Host I/O 能力。
///
/// ACPI 静态表和 AML 字节码的解析不依赖这些回调。只有执行访问 SystemIO 或传统 PCI
/// 配置空间的 AML 方法时才需要它们。架构层必须提供真实硬件访问，不能用固定返回值
/// 模拟成功；缺失能力时，内核会保留已解析的 AML namespace，并跳过方法执行。
#[derive(Clone, Copy)]
pub struct StartAcpiHostOps {
    pub io: Option<StartAcpiIoOps>,
    pub pci: Option<StartAcpiPciOps>,
}

impl StartAcpiHostOps {
    pub const NONE: Self = Self {
        io: None,
        pci: None,
    };
}

/// 从架构初始化传递给内核启动代码的稳定 ACPI 快照。
#[derive(Clone, Copy)]
pub struct StartAcpiTables {
    /// 已复制的 RSDP 视图的物理地址。ACPI 库使用物理地址
    /// 作为表的标识，因此即便复制后仍保持为物理地址。
    pub rsdp_phys: usize,
    /// 已复制的 ACPI 表的物理到虚拟地址映射。
    pub mappings: &'static [FirmwareTableMapping],
    /// AML 执行期可使用的架构 Host I/O 能力。
    pub host_ops: StartAcpiHostOps,
}

/// 由架构初始化选定并交给内核的固件表视图。
///
/// 该枚举保证上下文中始终只有一种被选中的固件格式，避免再由
/// `selected + Option` 的组合在运行期维持不变量。
#[derive(Clone, Copy)]
pub enum StartFirmware {
    /// 稳定的 DTB 视图。
    Dtb(Fdt<'static>),
    /// 稳定的 ACPI 视图。
    Acpi(StartAcpiTables),
}

impl StartFirmware {
    /// 返回当前上下文所选中的固件来源。
    pub const fn source(self) -> StartFirmwareSource {
        match self {
            Self::Dtb(_) => StartFirmwareSource::Dtb,
            Self::Acpi(_) => StartFirmwareSource::Acpi,
        }
    }
}

/// 在将分配器从引导分配器提升为分层运行期分配器之前
/// 必须知晓的内存范围。
#[derive(Clone, Copy, Debug)]
pub struct StartMemory {
    /// 内核镜像及其早期静态数据所占用的物理范围。
    ///
    /// 内核启动代码必须在将物理内存段交给分配器之前
    /// 保留该范围。
    pub kernel_image: StartPhysRange,
    /// 启动时的物理内存映射，前提是启动协议提供了独立于
    /// 所选固件表格式的映射信息。
    ///
    /// 这是有意不放在 [`StartAcpiTables`] 中的。ACPI 描述
    /// 设备和平台控制，但可用 RAM 的发现与启动协议相关：
    /// EFI 提供 EFI 内存映射，PC BIOS 通常提供 E820，
    /// Multiboot 提供带标签的范围，而仅使用 DTB 的系统
    /// 则常常直接在 `/memory` 节点中描述 RAM。将内存映射
    /// 放在此处，使得内核启动代码可以选择正确的解析器，
    /// 而不会让 ACPI 依赖于 EFI。
    pub boot_map: StartMemoryMap,
}

/// 与具体启动协议无关的物理内存映射。
#[derive(Clone, Copy, Debug)]
pub enum StartMemoryMap {
    /// 未提供独立的启动内存映射。
    None,
    /// 由架构加载器在进入 `kernel_start_init` 之前规范化好的物理内存范围。
    ///
    /// 这意味着协议特定的描述符布局已经在更低层被吸收；`StartContext`
    /// 只保留平台无关的结果，而不再暴露 EFI/E820/Multiboot 原始格式。
    Regions(&'static [StartMemoryRegion]),
}

impl StartMemoryMap {
    /// 返回启动协议规范化后的原始区域切片。
    pub const fn regions(self) -> Option<&'static [StartMemoryRegion]> {
        match self {
            Self::None => None,
            Self::Regions(regions) => Some(regions),
        }
    }

    /// 返回启动内存映射中是否至少存在一段在交接后仍可用的 RAM。
    pub fn has_usable_region(self) -> bool {
        match self {
            Self::None => false,
            Self::Regions(regions) => regions.iter().any(|region| {
                region.range.end > region.range.start && region.kind.is_usable_after_handoff()
            }),
        }
    }

    /// 提取在启动协议完成交接后可以交给物理分配器的 RAM 范围。
    ///
    /// 这是有意设计为上下文局部的转换。固件解析器不得依赖
    /// `StartMemoryMap`；`kernel_start_init` 从上下文中提取
    /// 所需的内存段，并将纯分配器范围传递给需要它们的解析器。
    pub fn usable_segments(self) -> Option<Vec<MemorySegment>> {
        match self {
            Self::None => None,
            Self::Regions(regions) => memory_segments_from_regions(regions),
        }
    }
}

/// 启动协议所提供的物理内存范围的分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartMemoryRegionKind {
    /// 在固件交接后可立即使用的普通 RAM。
    UsableRam,
    /// 由 bootloader 使用，一旦内核已复制所需的所有结构后
    /// 即可回收的内存。
    BootloaderReclaimable,
    /// 在 boot-services 阶段结束后可回收的固件 boot-services 内存。
    FirmwareReclaimable,
    /// 必须继续保留以用于运行时服务的固件运行时内存。
    FirmwareRuntime,
    /// ACPI 可回收内存。内核只有在已复制或解析所有需要的
    /// ACPI 表之后才能重用这些内存。
    AcpiReclaimable,
    /// ACPI NVS 内存。必须在休眠状态之间保持其内容。
    AcpiNonVolatileStorage,
    /// 设备 MMIO 或 MMIO 端口空间，绝不可作为通用 RAM。
    Mmio,
    /// 内核镜像、早期栈或永久的内核所有引导数据。
    KernelReserved,
    /// 不可使用或存在缺陷的 RAM。
    Unusable,
    /// 因未知原因而被保留。除非平台策略明确理解其用途，
    /// 否则应视为不可用。
    Reserved,
}

impl StartMemoryRegionKind {
    /// 返回该区域是否可以在启动协议完成交接后
    /// 立即交给早期物理分配器。
    pub const fn is_usable_after_handoff(self) -> bool {
        matches!(
            self,
            Self::UsableRam | Self::BootloaderReclaimable | Self::FirmwareReclaimable
        )
    }
}

/// 启动协议提供的一条物理内存映射条目。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartMemoryRegion {
    /// 该条目所覆盖的物理地址范围。
    pub range: StartPhysRange,
    /// 平台无关的内存分类。
    pub kind: StartMemoryRegionKind,
    /// 保留以供后续架构策略使用的原始协议属性。
    /// 对于 EFI，对应描述符属性；对于 E820/Multiboot，
    /// 可能为零或协议特定的位集合。
    pub attributes: u64,
    /// 协议原始区域类型。EFI 路径保存 descriptor `Type`，没有对应概念的
    /// 启动协议保持 `None`。
    pub source_type: Option<u32>,
}

impl StartMemoryRegion {
    /// 创建一条内存映射条目。
    pub const fn new(range: StartPhysRange, kind: StartMemoryRegionKind, attributes: u64) -> Self {
        Self {
            range,
            kind,
            attributes,
            source_type: None,
        }
    }

    /// 附加协议原始区域类型。
    pub const fn with_source_type(mut self, source_type: u32) -> Self {
        self.source_type = Some(source_type);
        self
    }
}

fn memory_segments_from_regions(
    regions: &'static [StartMemoryRegion],
) -> Option<Vec<MemorySegment>> {
    if regions.is_empty() {
        return None;
    }

    let mut segments = Vec::new();
    for region in regions {
        if region.range.end <= region.range.start {
            continue;
        }
        if region.kind.is_usable_after_handoff() {
            segments.push(MemorySegment {
                start: region.range.start,
                size: region.range.end - region.range.start,
            });
        }
    }

    normalize_memory_segments(segments)
}

fn normalize_memory_segments(segments: Vec<MemorySegment>) -> Option<Vec<MemorySegment>> {
    normalize_segments(segments)
}

/// 校验架构加载器发布的显式启动内存图。
///
/// `Regions` 表示加载器已经给出了 RAM 权威边界，因此即使条目本身格式完整，
/// 完全没有可交接 RAM 也必须失败；调用方不得把这种情况降级成 `None` 后改信 DT。
fn validate_start_memory_map(
    protocol: StartBootProtocol,
    boot_map: StartMemoryMap,
) -> Result<(), &'static str> {
    let StartMemoryMap::Regions(regions) = boot_map else {
        if matches!(protocol, StartBootProtocol::Efi) {
            return Err("[start-context] EFI boot requires a usable boot memory map");
        }
        return Ok(());
    };
    if regions.is_empty() {
        return Err("[start-context] boot memory map is empty");
    }
    if regions
        .iter()
        .any(|region| region.range.end <= region.range.start)
    {
        return Err("[start-context] boot memory map contains empty or inverted ranges");
    }
    if !boot_map.has_usable_region() {
        return Err("[start-context] boot memory map contains no usable RAM");
    }
    Ok(())
}

/// 由架构提供的地址转换能力。
#[derive(Clone, Copy)]
pub struct StartAddressOps {
    /// 将普通 RAM 物理地址转换为内核虚拟地址。
    pub phys_to_virt: PhysToVirtFn,
    /// 将内核虚拟地址转换为物理地址。该函数必须能处理架构早期直映地址；
    /// 若平台支持页表映射内核堆，也应覆盖内核堆地址。
    pub virt_to_phys: VirtToPhysFn,
    /// 将设备 MMIO 物理地址转换为内核虚拟地址。
    pub device_mmio_to_virt: DeviceMmioToVirtFn,
}

/// 由架构提供的可选分配器与分页回调。
#[derive(Clone, Copy)]
pub struct StartAllocatorOps {
    /// 返回为内核堆保留的虚拟地址范围。
    pub kernel_heap_region: KernelHeapRegionFn,
    /// 返回需要 registry 账本的分配使用的独立虚拟地址窗口。
    pub tracked_heap_region: KernelHeapRegionFn,
    /// 为内核堆虚拟范围映射物理后备范围。
    pub map_kernel_heap_range: MapKernelHeapRangeFn,
    /// 取消映射先前映射的内核堆虚拟范围。
    pub unmap_kernel_heap_range: UnmapKernelHeapRangeFn,
    /// 修改内核堆页权限，供 ELM 原生镜像完成 W^X 切换。
    pub protect_kernel_heap_range: ProtectKernelHeapRangeFn,
    /// 只读校验内核堆映射权限，供 ELM 原生 API 验证跨边界裸指针。
    pub validate_kernel_heap_range: ValidateKernelHeapRangeFn,
    /// 同步指令缓存，供 ELM 原生镜像完成代码发布。
    pub sync_icache: SyncIcacheFn,
    /// 安装映射的堆页所需的架构页表状态。
    pub init_kernel_page_table: InitKernelPageTableFn,
    /// 标准 RAM 线性映射的 `no-map` 能力。
    pub no_map: StartNoMapSupport,
}

/// 从架构初始化代码到内核启动代码的平台无关交接对象。
///
/// 此结构体刻意采用面向数据的设计：架构初始化代码一次性填充，
/// 随后内核启动代码按顺序消费。它应保持足够小，以便
/// 存放在静态内存中并通过指针传递。
#[derive(Clone, Copy)]
pub struct StartContext {
    /// 启动元数据，例如架构与入口协议。
    pub boot: StartBootInfo,
    /// 已被架构加载器选定的固件表视图。
    pub firmware: StartFirmware,
    /// 在 allocator 启动之前必须预留的物理内存范围。
    pub memory: StartMemory,
    /// 必须提供的地址转换回调。
    pub address: StartAddressOps,
    /// 可选的分层内核 allocator 支持回调，用于那些支持
    /// 分层内核分配器的平台。
    pub allocator: Option<StartAllocatorOps>,
}

/// 运行期保存的启动命令行快照。
static mut START_CMDLINE: Option<&'static [u8]> = None;
/// 运行期保存的已校验架构身份。
static START_ARCHITECTURE: AtomicU8 = AtomicU8::new(ArchitectureId::Unknown as u8);

impl StartContext {
    /// 返回当前启动上下文所选定的固件来源。
    pub const fn firmware_source(&self) -> StartFirmwareSource {
        self.firmware.source()
    }

    /// 校验启动上下文是否满足内核启动阶段依赖的基本不变量。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.boot.architecture.is_unknown() {
            return Err("[start-context] boot architecture is unknown");
        }

        if matches!(self.boot.protocol, StartBootProtocol::Unknown) {
            return Err("[start-context] boot protocol is unknown");
        }

        if self.memory.kernel_image.end <= self.memory.kernel_image.start {
            return Err("[start-context] kernel image range is empty or inverted");
        }

        validate_start_memory_map(self.boot.protocol, self.memory.boot_map)?;

        if let Some(allocator) = self.allocator
            && let Some(granule) = allocator.no_map.granule()
            && (granule == 0 || !granule.is_power_of_two())
        {
            return Err("[start-context] no-map granule must be a non-zero power of two");
        }

        if let StartFirmware::Acpi(acpi) = self.firmware {
            if acpi.rsdp_phys == 0 {
                return Err("[start-context] ACPI firmware is missing the copied RSDP");
            }
            validate_acpi_table_mappings(acpi.rsdp_phys, acpi.mappings)?;
            if !self.memory.boot_map.has_usable_region() {
                return Err("[start-context] ACPI firmware requires usable boot memory segments");
            }
        }

        Ok(())
    }
}

fn validate_acpi_table_mappings(
    rsdp_phys: usize,
    mappings: &[FirmwareTableMapping],
) -> Result<(), &'static str> {
    if mappings.is_empty() {
        return Err("[start-context] ACPI firmware is missing copied table mappings");
    }

    for (index, mapping) in mappings.iter().enumerate() {
        if mapping.length == 0 || mapping.virtual_start == 0 {
            return Err("[start-context] ACPI table mapping is empty or null");
        }
        if mapping.physical_start.checked_add(mapping.length).is_none()
            || mapping.virtual_start.checked_add(mapping.length).is_none()
        {
            return Err("[start-context] ACPI table mapping address overflows");
        }

        let physical_end = mapping.physical_start + mapping.length;
        if mappings[..index].iter().any(|previous| {
            let previous_end = previous.physical_start + previous.length;
            mapping.physical_start < previous_end && previous.physical_start < physical_end
        }) {
            return Err("[start-context] ACPI table mappings overlap");
        }
    }

    if !mappings
        .iter()
        .any(|mapping| mapping.resolve(rsdp_phys, 36).is_some())
    {
        return Err("[start-context] copied RSDP is outside ACPI table mappings");
    }

    Ok(())
}

/// 安装运行期启动命令行快照。
pub fn set_start_cmdline(cmdline: Option<&'static [u8]>) {
    unsafe {
        START_CMDLINE = cmdline;
    }
}

/// 安装运行期架构身份。重复安装同一身份是幂等的，冲突身份会立即失败。
pub fn set_start_architecture(architecture: ArchitectureId) {
    assert!(
        !architecture.is_unknown(),
        "[start-context] cannot install an unknown runtime architecture"
    );
    match START_ARCHITECTURE.compare_exchange(
        ArchitectureId::Unknown as u8,
        architecture as u8,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(previous) if previous == architecture as u8 => {}
        Err(_) => panic!("[start-context] runtime architecture already installed"),
    }
}

/// 读取运行期架构身份；启动上下文尚未安装时返回 [`ArchitectureId::Unknown`]。
pub fn start_architecture() -> ArchitectureId {
    ArchitectureId::from_raw(START_ARCHITECTURE.load(Ordering::Acquire))
        .unwrap_or(ArchitectureId::Unknown)
}

/// 读取运行期启动命令行快照。
pub fn start_cmdline() -> Option<&'static [u8]> {
    unsafe { START_CMDLINE }
}

#[cfg(test)]
mod tests {
    use super::{
        ArchitectureId, FirmwareTableMapping, StartBootProtocol, StartMemoryMap, StartMemoryRegion,
        StartMemoryRegionKind, StartPhysRange, validate_acpi_table_mappings,
        validate_start_memory_map,
    };

    static RESERVED_ONLY: [StartMemoryRegion; 1] = [StartMemoryRegion::new(
        StartPhysRange::new(0x1000, 0x2000),
        StartMemoryRegionKind::Reserved,
        0,
    )];
    static USABLE: [StartMemoryRegion; 1] = [StartMemoryRegion::new(
        StartPhysRange::new(0x2000, 0x4000),
        StartMemoryRegionKind::UsableRam,
        0,
    )];

    #[test]
    fn explicit_boot_memory_map_requires_usable_ram() {
        assert!(validate_start_memory_map(StartBootProtocol::Direct, StartMemoryMap::None).is_ok());
        assert_eq!(
            validate_start_memory_map(StartBootProtocol::Efi, StartMemoryMap::None),
            Err("[start-context] EFI boot requires a usable boot memory map")
        );
        assert_eq!(
            validate_start_memory_map(
                StartBootProtocol::Direct,
                StartMemoryMap::Regions(&RESERVED_ONLY),
            ),
            Err("[start-context] boot memory map contains no usable RAM")
        );
        assert!(
            validate_start_memory_map(StartBootProtocol::Efi, StartMemoryMap::Regions(&USABLE),)
                .is_ok()
        );
    }

    #[test]
    fn architecture_identity_has_canonical_names_and_values() {
        assert_eq!(ArchitectureId::Riscv64.name(), "riscv64");
        assert_eq!(ArchitectureId::LoongArch64.name(), "loongarch64");
        assert_eq!(ArchitectureId::X86_64.name(), "x86_64");
        assert_eq!(ArchitectureId::from_raw(3), Some(ArchitectureId::X86_64));
        assert!(ArchitectureId::Unknown.is_unknown());
    }

    #[test]
    fn acpi_snapshot_mappings_cover_rsdp_without_overlap_or_overflow() {
        let valid = [FirmwareTableMapping {
            physical_start: 0x1000,
            virtual_start: 0x8000,
            length: 0x100,
        }];
        assert!(validate_acpi_table_mappings(0x1020, &valid).is_ok());

        let null = [FirmwareTableMapping {
            virtual_start: 0,
            ..valid[0]
        }];
        assert_eq!(
            validate_acpi_table_mappings(0x1020, &null),
            Err("[start-context] ACPI table mapping is empty or null")
        );

        let overflow = [FirmwareTableMapping {
            virtual_start: usize::MAX - 8,
            length: 0x100,
            ..valid[0]
        }];
        assert_eq!(
            validate_acpi_table_mappings(0x1020, &overflow),
            Err("[start-context] ACPI table mapping address overflows")
        );

        let overlap = [
            valid[0],
            FirmwareTableMapping {
                physical_start: 0x1080,
                virtual_start: 0x9000,
                length: 0x100,
            },
        ];
        assert_eq!(
            validate_acpi_table_mappings(0x1020, &overlap),
            Err("[start-context] ACPI table mappings overlap")
        );
        assert_eq!(
            validate_acpi_table_mappings(0x2000, &valid),
            Err("[start-context] copied RSDP is outside ACPI table mappings")
        );
    }
}
