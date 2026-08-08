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

use allocator::{
    KernelHeapRegionFn, MapKernelHeapRangeFn, MemorySegment, PhysToVirtFn, UnmapKernelHeapRangeFn,
    VirtToPhysFn,
};

use crate::dtb::Dtb;
use crate::firmware::{FirmwareTableMapping, normalize_segments};

/// 将设备 MMIO 物理地址转换为内核虚拟地址，该虚拟地址用于
/// 易失性寄存器访问。
///
/// 有意不采用任何具体架构早期映射或 ioremap 等机制来命名。
/// 各架构自行选择实现方式。
pub type DeviceMmioToVirtFn = fn(phys_addr: usize) -> usize;

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

/// 当前正在运行的内核镜像所属的指令集架构与 ABI 家族。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartArchitecture {
    name: &'static str,
}

impl StartArchitecture {
    pub const UNKNOWN: Self = Self { name: "" };

    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    pub const fn is_unknown(self) -> bool {
        self.name.is_empty()
    }
}

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

/// 从架构初始化传递给内核启动代码的稳定 ACPI 快照。
#[derive(Clone, Copy)]
pub struct StartAcpiTables {
    /// 已复制的 RSDP 视图的物理地址。ACPI 库使用物理地址
    /// 作为表的标识，因此即便复制后仍保持为物理地址。
    pub rsdp_phys: usize,
    /// 已复制的 ACPI 表的物理到虚拟地址映射。
    pub mappings: &'static [FirmwareTableMapping],
}

/// 由架构初始化选定并交给内核的固件表视图。
///
/// 该枚举保证上下文中始终只有一种被选中的固件格式，避免再由
/// `selected + Option` 的组合在运行期维持不变量。
#[derive(Clone, Copy)]
pub enum StartFirmware {
    /// 稳定的 DTB 视图。
    Dtb(Dtb<'static>),
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
}

impl StartMemoryRegion {
    /// 创建一条内存映射条目。
    pub const fn new(range: StartPhysRange, kind: StartMemoryRegionKind, attributes: u64) -> Self {
        Self {
            range,
            kind,
            attributes,
        }
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

        if let StartMemoryMap::Regions(regions) = self.memory.boot_map {
            if regions.is_empty() {
                return Err("[start-context] boot memory map is empty");
            }
            if regions
                .iter()
                .any(|region| region.range.end <= region.range.start)
            {
                return Err("[start-context] boot memory map contains empty or inverted ranges");
            }
        }

        if let StartFirmware::Acpi(acpi) = self.firmware {
            if acpi.rsdp_phys == 0 {
                return Err("[start-context] ACPI firmware is missing the copied RSDP");
            }
            if acpi.mappings.is_empty() {
                return Err("[start-context] ACPI firmware is missing copied table mappings");
            }
            if !self.memory.boot_map.has_usable_region() {
                return Err("[start-context] ACPI firmware requires usable boot memory segments");
            }
        }

        Ok(())
    }
}

/// 安装运行期启动命令行快照。
pub fn set_start_cmdline(cmdline: Option<&'static [u8]>) {
    unsafe {
        START_CMDLINE = cmdline;
    }
}

/// 读取运行期启动命令行快照。
pub fn start_cmdline() -> Option<&'static [u8]> {
    unsafe { START_CMDLINE }
}
