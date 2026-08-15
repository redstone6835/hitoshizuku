//! PCI 设备抽象层。
//!
//! 将 PCI/PCIe 设备封装为类型安全的 `PciDevice`，驱动通过它访问
//! config space、BAR 和 MSI/MSI-X，而不是直接操作裸寄存器。
//!
//! # 架构
//!
//! ```text
//! PciDevice  → 持有 Arc<PnpDevice>，提供类型安全访问
//! PciInfo    →  实现 PnpBusInfo，存储 vendor/device/class 等
//! PciBar     →  BAR 描述符
//! ```
//!
//! # 用法
//!
//! ```rust,ignore
//! // Bus 层：扫描到设备后创建 PnpDevice + PciDevice
//! let info = Box::new(PciInfo { vendor: 0x8086, device_id: 0x100e, ... });
//! let id = PnpId::Pci { segment: 0, bus: 1, device: 0, function: 0 };
//! let pnp = PnpDevice::new(id, "pci-0000:01:00.0".into(), info)?;
//! let pci_dev = PciDevice::from_pnp(&pnp).unwrap();
//!
//! // 驱动 probe 中使用 PciDevice
//! let bar0 = pci_dev.map_bar(0).unwrap();
//! pci_dev.enable_bus_master();
//! ```

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::mem::size_of;
use core::ptr::{read_volatile, write_volatile};

use vfs::sync::Spinlock;

use super::dma::{DmaAddressMapping, DmaBouncePolicy, DmaConstraints, DmaContext, DmaWindow};
use super::iommu::{self, IommuAttachment, IommuRequester};
use super::irq::IrqLine;
use super::msi;
use super::platform::FirmwareProperty;
use super::pnp::{
    self as pnp_core, BusType, PNP_DEVICES, PNP_DRIVERS, PnpBusInfo, PnpDependency, PnpDevice,
    PnpError, PnpHandleResource, PnpId, PnpResource, PnpResourceKind, PnpResourceReleaseError,
    PnpState,
};
use super::registry_id;

// ── PciInfo ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct PciInfo {
    pub vendor: u16,
    pub device_id: u16,
    pub revision: u8,
    pub class: u32,
    pub subclass: u8,
    pub prog_if: u8,
    pub subsystem_vendor: u16,
    pub subsystem_id: u16,
    pub header_type: u8,
    pub multi_function: bool,
}

impl PciInfo {
    pub const fn class_code(self) -> (u8, u8, u8) {
        ((self.class >> 16) as u8, self.subclass, self.prog_if)
    }

    pub fn is_storage_controller(&self) -> bool {
        (self.class >> 16) as u8 == 0x01
    }

    pub fn is_network_controller(&self) -> bool {
        (self.class >> 16) as u8 == 0x02
    }

    pub fn is_display_controller(&self) -> bool {
        (self.class >> 16) as u8 == 0x03
    }

    pub fn is_bridge(&self) -> bool {
        (self.class >> 16) as u8 == 0x06
    }

    /// 当前 header 类型实际提供的 BAR 槽数量。
    ///
    /// endpoint function 有 6 个普通 BAR；桥设备只有前 2 个 BAR，其余配置空间
    /// 字段有桥接专用含义。未知 header 类型不参与通用 BAR 枚举，避免把其它字段
    /// 误当成资源寄存器。
    pub const fn bar_count(&self) -> usize {
        match self.header_type {
            PCI_HEADER_TYPE_ENDPOINT => PCI_ENDPOINT_BAR_COUNT,
            PCI_HEADER_TYPE_BRIDGE => PCI_BRIDGE_BAR_COUNT,
            _ => 0,
        }
    }
}

impl PnpBusInfo for PciInfo {
    fn bus_type(&self) -> BusType {
        BusType::PCI
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── PciBar ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciBarType {
    Memory,
    Io,
}

#[derive(Clone, Copy, Debug)]
pub struct PciBar {
    pub idx: usize,
    pub bar_type: PciBarType,
    pub prefetchable: bool,
    /// 经 host bridge `ranges` 翻译后的 CPU 物理地址。
    pub phys_addr: u64,
    pub size: u64,
}

/// 事务性写入 64-bit BAR 的低/高 dword。
///
/// 高 dword 写入失败时，回调会被继续用于恢复两个原值。这是启动期
/// BAR fallback 与测试共享的纯事务原语，不属于 ELM 导出 ABI。
#[doc(hidden)]
pub fn write_pci_bar_u64_transactional<E>(
    original_low: u32,
    original_high: u32,
    new_low: u32,
    new_high: u32,
    mut write: impl FnMut(bool, u32) -> Result<(), E>,
) -> Result<(), E> {
    write(false, new_low)?;
    if let Err(error) = write(true, new_high) {
        let _ = write(false, original_low);
        let _ = write(true, original_high);
        return Err(error);
    }
    Ok(())
}

// ── PCI host bridge ─────────────────────────────────────────────────────

/// PCI host bridge 的地址窗口类型。
///
/// 这里保存的是 host bridge 对 PCI 子地址空间的公开能力，不把 DTB `ranges`
/// cell 或其它固件编码形式泄露给 PCI 设备层。不同固件来源只需要在启动阶段把
/// 自己的格式转换成这个枚举即可。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciHostAddressSpace {
    Io,
    Memory,
    PrefetchableMemory,
    Unknown(u32),
}

/// PCI host bridge 暴露的一段子地址空间到 CPU 物理地址的映射。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciHostBridgeWindow {
    pub space: PciHostAddressSpace,
    pub pci_start: u64,
    pub cpu_start: usize,
    pub size: usize,
}

/// host bridge `iommu-map` 的一项固件无关 requester-ID 映射。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciRequesterIdMapEntry {
    pub input_base: u32,
    pub provider_path: Box<str>,
    pub provider_phandle: u32,
    pub output_base: Box<[u32]>,
    pub length: u32,
}

/// 已规范化的 PCI requester-ID -> IOMMU stream-ID 映射。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciRequesterIdMap {
    pub mask: u32,
    pub entries: Vec<PciRequesterIdMapEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciRequesterIdMapMatch<'a> {
    pub entry: &'a PciRequesterIdMapEntry,
    pub offset: u32,
}

/// requester-ID 映射后的稳定借用视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciMappedRequesterId<'a> {
    pub provider_path: &'a str,
    pub provider_phandle: u32,
    pub args: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PciRequesterIdMapError {
    OutputOverflow {
        provider_phandle: u32,
        output_base: u32,
        offset: u32,
    },
    AmbiguousMultiCellRange {
        provider_phandle: u32,
        cells: usize,
        length: u32,
        offset: u32,
    },
}

impl PciRequesterIdMap {
    pub fn match_id(&self, requester_id: u32) -> Option<PciRequesterIdMapMatch<'_>> {
        let requester_id = requester_id & self.mask;
        self.entries.iter().find_map(|entry| {
            let offset = requester_id.checked_sub(entry.input_base)?;
            if offset >= entry.length {
                return None;
            }
            Some(PciRequesterIdMapMatch { entry, offset })
        })
    }

    pub fn map_id(
        &self,
        requester_id: u32,
    ) -> Result<Option<PciMappedRequesterId<'_>>, PciRequesterIdMapError> {
        let Some(matched) = self.match_id(requester_id) else {
            return Ok(None);
        };
        let entry = matched.entry;
        let args = match entry.output_base.as_ref() {
            [] => Vec::new(),
            &[output_base] => Vec::from([output_base.checked_add(matched.offset).ok_or(
                PciRequesterIdMapError::OutputOverflow {
                    provider_phandle: entry.provider_phandle,
                    output_base,
                    offset: matched.offset,
                },
            )?]),
            _ if entry.length == 1 => entry.output_base.to_vec(),
            output_base => {
                return Err(PciRequesterIdMapError::AmbiguousMultiCellRange {
                    provider_phandle: entry.provider_phandle,
                    cells: output_base.len(),
                    length: entry.length,
                    offset: matched.offset,
                });
            }
        };
        Ok(Some(PciMappedRequesterId {
            provider_path: &entry.provider_path,
            provider_phandle: entry.provider_phandle,
            args,
        }))
    }
}

/// host bridge 的有效 DMA 策略。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PciHostDmaInfo {
    /// `None` 表示沿用平台默认映射；`Some([])` 表示固件显式 identity。
    pub windows: Option<Vec<DmaWindow>>,
    /// 只包含可用 provider 的 requester-ID 映射。
    pub iommu_map: Option<PciRequesterIdMap>,
    /// host 节点直接声明、由全部 function 继承的 `iommus`。
    pub iommus: Vec<PciIommuReference>,
    /// 固件地址宽度或拓扑无法安全投影到当前内核 DMA 抽象。
    pub unsupported: bool,
    /// DT 中按 BDF 描述的 function 级覆盖。
    pub functions: Vec<PciFunctionDmaInfo>,
}

/// 固件为一个可枚举 PCI function 提供的稳定元数据快照。
///
/// 配置空间仍是 vendor/device/class 等 PCI 身份的权威来源；本结构只保存 DT、
/// ACPI 等固件额外提供的匹配字符串、句柄和原始属性。驱动因此不需要回读 DTB，
/// 也不会把 QEMU 或具体板级的 BDF 硬编码成设备身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciFunctionFirmwareInfo {
    pub firmware_path: Box<str>,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub phandle: Option<u32>,
    pub compatible: Vec<Box<str>>,
    pub properties: Vec<FirmwareProperty>,
}

#[kernel_symbols::export]
impl PciFunctionFirmwareInfo {
    /// 判断固件 compatible/ID 列表是否包含精确匹配项。
    #[kernel_symbols::export(
        name = "general.dev.pci.PciFunctionFirmwareInfo.has_compatible",
        contract = "kernel.general.pci-firmware@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn has_compatible(&self, expected: &str) -> bool {
        self.compatible
            .iter()
            .any(|compatible| compatible.as_ref() == expected)
    }

    /// 按属性名读取一个严格的单 cell、大端 `u32` 值。
    #[kernel_symbols::export(
        name = "general.dev.pci.PciFunctionFirmwareInfo.u32_property",
        contract = "kernel.general.pci-firmware@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn u32_property(&self, name: &str) -> Option<u32> {
        self.properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(FirmwareProperty::as_u32)
    }

    const fn matches_bdf(&self, bus: u8, device: u8, function: u8) -> bool {
        self.bus == bus && self.device == device && self.function == function
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciFunctionDmaInfo {
    pub firmware_path: Box<str>,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub coherent: bool,
    pub windows: Option<Vec<DmaWindow>>,
    /// function 节点直接声明的 `iommus`，优先于 host 级 direct 引用和 map。
    pub iommus: Vec<PciIommuReference>,
    pub unsupported: bool,
}

/// 一个已经按 provider `#iommu-cells` 切分的 direct IOMMU 引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciIommuReference {
    pub provider_path: Box<str>,
    pub provider_phandle: u32,
    pub args: Box<[u32]>,
}

/// Generic PCI host 的配置空间布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciConfigSpaceKind {
    /// Conventional PCI CAM：每条 bus 64 KiB，每个 function 256 bytes。
    Cam,
    /// PCIe ECAM：每条 bus 1 MiB，每个 function 4 KiB。
    Ecam,
    /// LS2K1000 厂商 CFG1 编码，配置函数仍暴露完整 4 KiB 空间。
    Ls2k1000,
}

impl PciConfigSpaceKind {
    pub const fn bytes_per_bus(self) -> usize {
        match self {
            Self::Cam => 1 << 16,
            Self::Ecam => 1 << 20,
            Self::Ls2k1000 => 1 << 16,
        }
    }

    pub const fn bytes_per_function(self) -> u16 {
        match self {
            Self::Cam => 0x100,
            Self::Ecam | Self::Ls2k1000 => PCI_EXTENDED_CONFIG_SPACE_SIZE,
        }
    }

    pub const fn bus_shift(self) -> u8 {
        match self {
            Self::Cam | Self::Ls2k1000 => 16,
            Self::Ecam => 20,
        }
    }

    pub const fn function_shift(self) -> u8 {
        match self {
            Self::Cam | Self::Ls2k1000 => 8,
            Self::Ecam => 12,
        }
    }
}

/// PCI host bridge 的标准化描述。
///
/// 该结构是设备层认识 host bridge 的统一入口：配置空间范围、bus-range、地址窗口、
/// DMA 一致性以及固件路由规模都会被保存下来，供 sysfs/诊断/后续热插拔或 DMA
/// 策略查询。具体的配置空间读写回调仍由 [`PciConfigAccess`] 单独安装。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciHostBridgeInfo {
    pub name: Box<str>,
    pub firmware_path: Option<Box<str>>,
    /// host bridge 及其 functions 的 DMA 分配亲和节点。
    pub numa_node_id: Option<u32>,
    pub domain: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    pub ecam_phys: usize,
    pub ecam_size: usize,
    pub config_space: PciConfigSpaceKind,
    pub dma_coherent: bool,
    pub dma: PciHostDmaInfo,
    /// 固件中按 BDF 描述的 function 级属性；未描述的可枚举 function 不产生条目。
    pub firmware_functions: Vec<PciFunctionFirmwareInfo>,
    pub windows: Vec<PciHostBridgeWindow>,
    pub irq_route_count: usize,
    pub msi_route_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciHostBridgeHandle {
    id: u64,
}

impl PciHostBridgeHandle {
    pub const fn id(self) -> u64 {
        self.id
    }
}

#[derive(Clone, Debug)]
pub struct PciHostBridgeSnapshot {
    pub handle: PciHostBridgeHandle,
    pub info: PciHostBridgeInfo,
    pub pnp: Option<Arc<PnpDevice>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciHostBridgeError {
    Invalid,
    AlreadyRegistered,
    NotFound,
    OutOfMemory,
}

/// PCI host 运行时状态的 segment+bus-range 键。
///
/// 这是 DT/ACPI 启动适配层共享的内部构件，不属于 ELM 导出 ABI。精确键用于
/// 回滚单次 host 初始化，避免按“任意重叠”清理时误删先前已经发布的 host。
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciHostBusRange {
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
}

impl PciHostBusRange {
    pub const fn new(segment: u16, bus_start: u8, bus_end: u8) -> Option<Self> {
        if bus_start > bus_end {
            return None;
        }
        Some(Self {
            segment,
            bus_start,
            bus_end,
        })
    }

    pub const fn contains(self, segment: u16, bus: u8) -> bool {
        self.segment == segment && bus >= self.bus_start && bus <= self.bus_end
    }

    pub const fn overlaps(self, other: Self) -> bool {
        self.segment == other.segment
            && self.bus_start <= other.bus_end
            && other.bus_start <= self.bus_end
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciHostTableError {
    Overlap,
    OutOfMemory,
}

/// 先完整构造候选、再按 host 键一次发布的运行时表。
///
/// 插入失败不会修改表；删除只接受精确键。该约束让 BAR、ECAM 与中断路由的
/// 启动期事务可以安全回滚，而不会覆盖或删除另一个合法 host。
#[doc(hidden)]
pub struct PciHostTable<T> {
    entries: Vec<(PciHostBusRange, T)>,
}

impl<T> PciHostTable<T> {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn insert(&mut self, key: PciHostBusRange, value: T) -> Result<(), PciHostTableError> {
        if self
            .entries
            .iter()
            .any(|(existing, _)| existing.overlaps(key))
        {
            return Err(PciHostTableError::Overlap);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| PciHostTableError::OutOfMemory)?;
        self.entries.push((key, value));
        Ok(())
    }

    pub fn get(&self, segment: u16, bus: u8) -> Option<&T> {
        self.entries
            .iter()
            .find(|(key, _)| key.contains(segment, bus))
            .map(|(_, value)| value)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|(_, value)| value)
    }

    pub fn remove_exact(&mut self, key: PciHostBusRange) -> Option<T> {
        let index = self
            .entries
            .iter()
            .position(|(existing, _)| *existing == key)?;
        Some(self.entries.swap_remove(index).1)
    }
}

struct PciHostBridgeRegistration {
    handle: PciHostBridgeHandle,
    info: PciHostBridgeInfo,
    fallback_dma_context: DmaContext,
    function_dma_contexts: Vec<PciFunctionDmaRegistration>,
    pnp: Option<Arc<PnpDevice>>,
}

struct PciFunctionDmaRegistration {
    bus: u8,
    device: u8,
    function: u8,
    context: DmaContext,
    iommu_context: Option<DmaContext>,
}

struct PciHostBridgeRegistry {
    next_id: u64,
    bridges: Vec<PciHostBridgeRegistration>,
}

impl PciHostBridgeRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            bridges: Vec::new(),
        }
    }
}

static PCI_HOST_BRIDGES: Spinlock<PciHostBridgeRegistry> =
    Spinlock::new(PciHostBridgeRegistry::new());

/// 登记一个固件枚举出的 PCI host bridge。
///
/// `pnp` 是可选的固件节点对象；存在时，后续扫描到的 PCI function 会自动挂到
/// 该节点下形成拓扑树。没有固件节点的早期平台仍可只登记 typed host 描述。
#[kernel_symbols::export(
    name = "general.dev.pci.register_host_bridge",
    contract = "kernel.general.pci-host@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_host_bridge(
    info: PciHostBridgeInfo,
    pnp: Option<Arc<PnpDevice>>,
) -> Result<PciHostBridgeHandle, PciHostBridgeError> {
    let bus_count = usize::from(info.bus_end)
        .checked_sub(usize::from(info.bus_start))
        .and_then(|count| count.checked_add(1));
    let required_config_size =
        bus_count.and_then(|count| count.checked_mul(info.config_space.bytes_per_bus()));
    if info.bus_start > info.bus_end
        || required_config_size.is_none_or(|required| info.ecam_size < required)
    {
        return Err(PciHostBridgeError::Invalid);
    }

    let domain = info.domain;
    let mut registry = PCI_HOST_BRIDGES.lock();
    if registry.bridges.iter().any(|bridge| {
        bridge.info.domain == info.domain
            && pci_bus_ranges_overlap(
                bridge.info.bus_start,
                bridge.info.bus_end,
                info.bus_start,
                info.bus_end,
            )
    }) {
        return Err(PciHostBridgeError::AlreadyRegistered);
    }
    let fallback_dma_context = pci_host_fallback_dma_context(&info)?;
    let function_dma_contexts = info
        .dma
        .functions
        .iter()
        .map(|function| {
            Ok(PciFunctionDmaRegistration {
                bus: function.bus,
                device: function.device,
                function: function.function,
                context: pci_dma_context(
                    function.coherent,
                    function.windows.as_ref(),
                    function.unsupported,
                )?,
                iommu_context: None,
            })
        })
        .collect::<Result<Vec<_>, PciHostBridgeError>>()?;
    registry
        .bridges
        .try_reserve(1)
        .map_err(|_| PciHostBridgeError::OutOfMemory)?;
    let id = registry_id::alloc_locked_id(&mut registry.next_id)
        .map_err(|_| PciHostBridgeError::OutOfMemory)?;
    // host bridge 句柄可能被启动期回滚或热移除路径保存；编号只增长不复用，
    // 旧句柄就不会误注销后来重新登记的同一 domain/bus-range。
    let handle = PciHostBridgeHandle { id };
    registry.bridges.push(PciHostBridgeRegistration {
        handle,
        info,
        fallback_dma_context,
        function_dma_contexts,
        pnp,
    });
    drop(registry);
    if super::elm_lifecycle::track_pci_host_bridge(handle).is_err() {
        let _ = unregister_host_bridge(handle);
        return Err(PciHostBridgeError::OutOfMemory);
    }
    pnp_core::notify_dependency_ready(PnpDependency::PciHostBridge(domain));
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.pci.unregister_host_bridge",
    contract = "kernel.general.pci-host@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_host_bridge(handle: PciHostBridgeHandle) -> Result<(), PciHostBridgeError> {
    let mut registry = PCI_HOST_BRIDGES.lock();
    let Some(index) = registry
        .bridges
        .iter()
        .position(|bridge| bridge.handle == handle)
    else {
        return Err(PciHostBridgeError::NotFound);
    };
    let bridge = registry.bridges.swap_remove(index);
    drop(registry);
    drop(bridge);
    super::elm_lifecycle::forget_pci_host_bridge(handle);
    Ok(())
}

fn release_host_bridge_resource(handle: PciHostBridgeHandle) -> bool {
    unregister_host_bridge(handle).is_ok()
}

/// 将 PCI host bridge 登记 handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.pci.host_bridge_pnp_resource",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn host_bridge_pnp_resource(
    handle: PciHostBridgeHandle,
    label: &'static str,
) -> PnpHandleResource<PciHostBridgeHandle> {
    PnpHandleResource::new(
        PnpResourceKind::PciHostBridge,
        label,
        handle,
        release_host_bridge_resource,
    )
}

#[kernel_symbols::export(
    name = "general.dev.pci.host_bridge_snapshot",
    contract = "kernel.general.pci-host@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn host_bridge_snapshot() -> Vec<PciHostBridgeSnapshot> {
    PCI_HOST_BRIDGES
        .lock()
        .bridges
        .iter()
        .map(snapshot_host_bridge)
        .collect()
}

/// 查询覆盖指定 segment/bus 的 PCI host bridge。
///
/// 调用方只需要知道 PCI function 所在的 segment/bus，即可拿到 host bridge 的
/// ECAM、地址窗口和 DMA 属性；不需要理解 DTB `ranges`、ACPI 资源模板或其它
/// 固件编码细节。
#[kernel_symbols::export(
    name = "general.dev.pci.host_bridge_for_bus",
    contract = "kernel.general.pci-host@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn host_bridge_for_bus(segment: u16, bus: u8) -> Option<PciHostBridgeSnapshot> {
    PCI_HOST_BRIDGES
        .lock()
        .bridges
        .iter()
        .find(|bridge| pci_host_bridge_covers(bridge, segment, bus))
        .map(snapshot_host_bridge)
}

fn find_function_firmware_info(
    functions: &[PciFunctionFirmwareInfo],
    bus: u8,
    device: u8,
    function: u8,
) -> Option<&PciFunctionFirmwareInfo> {
    functions
        .iter()
        .find(|entry| entry.matches_bdf(bus, device, function))
}

/// 查询一个 PCI function 的固件附加元数据。
///
/// 返回拥有型快照，调用方不会跨越 host bridge 热移除边界持有 registry 借用。
/// 固件没有描述该 BDF 时返回 `None`，这与 function 不存在是两个独立事实。
#[kernel_symbols::export(
    name = "general.dev.pci.pci_function_firmware_info",
    contract = "kernel.general.pci-firmware@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
        | kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pci_function_firmware_info(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
) -> Option<PciFunctionFirmwareInfo> {
    let registry = PCI_HOST_BRIDGES.lock();
    let bridge = registry
        .bridges
        .iter()
        .find(|bridge| pci_host_bridge_covers(bridge, segment, bus))?;
    find_function_firmware_info(&bridge.info.firmware_functions, bus, device, function).cloned()
}

fn host_bridge_pnp_for(segment: u16, bus: u8) -> Option<Arc<PnpDevice>> {
    PCI_HOST_BRIDGES
        .lock()
        .bridges
        .iter()
        .find(|bridge| pci_host_bridge_covers(bridge, segment, bus))
        .and_then(|bridge| bridge.pnp.as_ref().map(Arc::clone))
}

fn host_bridge_dma_context_for_bdf(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
) -> Option<DmaContext> {
    let requester_id = (u32::from(bus) << 8) | (u32::from(device) << 3) | u32::from(function);
    let (bridge_handle, context, references) = {
        let registry = PCI_HOST_BRIDGES.lock();
        let bridge = registry
            .bridges
            .iter()
            .find(|bridge| pci_host_bridge_covers(bridge, segment, bus))?;
        let function_context = bridge
            .function_dma_contexts
            .iter()
            .find(|entry| entry.bus == bus && entry.device == device && entry.function == function);
        if let Some(resolved) = function_context.and_then(|entry| entry.iommu_context.as_ref())
            && !resolved.iommu_consumer_released()
        {
            return Some(resolved.clone());
        }
        let context = function_context.map_or_else(
            || bridge.fallback_dma_context.clone(),
            |entry| entry.context.clone(),
        );
        let function_iommus = bridge
            .info
            .dma
            .functions
            .iter()
            .find(|entry| entry.bus == bus && entry.device == device && entry.function == function)
            .map(|entry| entry.iommus.as_slice())
            .unwrap_or(&[]);
        let references = if !function_iommus.is_empty() {
            function_iommus.to_vec()
        } else if !bridge.info.dma.iommus.is_empty() {
            bridge.info.dma.iommus.clone()
        } else if let Some(map) = bridge.info.dma.iommu_map.as_ref() {
            match map.map_id(requester_id) {
                Ok(Some(mapped)) => alloc::vec![PciIommuReference {
                    provider_path: mapped.provider_path.into(),
                    provider_phandle: mapped.provider_phandle,
                    args: mapped.args.into_boxed_slice(),
                }],
                Ok(None) => return Some(context),
                Err(_) => return Some(DmaContext::blocked(context.constraints())),
            }
        } else {
            return Some(context);
        };
        (bridge.handle, context, references)
    };

    let resolved = match pci_iommu_context(context.clone(), segment, requester_id, &references) {
        Ok(context) => context,
        Err(()) => return Some(DmaContext::blocked(context.constraints())),
    };

    let mut registry = PCI_HOST_BRIDGES.lock();
    let bridge = registry
        .bridges
        .iter_mut()
        .find(|bridge| bridge.handle == bridge_handle)?;
    if let Some(entry) = bridge
        .function_dma_contexts
        .iter_mut()
        .find(|entry| entry.bus == bus && entry.device == device && entry.function == function)
    {
        if let Some(existing) = entry.iommu_context.as_ref()
            && !existing.iommu_consumer_released()
        {
            return Some(existing.clone());
        }
        entry.iommu_context = Some(resolved.clone());
        return Some(resolved);
    }
    if bridge.function_dma_contexts.try_reserve(1).is_err() {
        // 未缓存的 lazy consumer 会让注册阶段认领 context A，而驱动下一次查询
        // 又创建 context B；B 没有 PnP lease。OOM 时必须 fail closed。
        return Some(
            DmaContext::blocked(context.constraints())
                .with_preferred_numa_node(context.preferred_numa_node()),
        );
    }
    bridge
        .function_dma_contexts
        .push(PciFunctionDmaRegistration {
            bus,
            device,
            function,
            context,
            iommu_context: Some(resolved.clone()),
        });
    Some(resolved)
}

fn pci_iommu_context(
    fallback: DmaContext,
    segment: u16,
    requester_id: u32,
    references: &[PciIommuReference],
) -> Result<DmaContext, ()> {
    let constraints = fallback.constraints();
    let preferred_numa_node = fallback.preferred_numa_node();
    let Ok(requester_id) = u16::try_from(requester_id) else {
        return Err(());
    };
    let attachments = references
        .iter()
        .map(|reference| IommuAttachment::new(reference.provider_phandle, reference.args.clone()))
        .collect();
    iommu::lazy_iommu_context(
        constraints,
        IommuRequester::pci(segment, requester_id),
        attachments,
    )
    .map(|context| context.with_preferred_numa_node(preferred_numa_node))
    .map_err(|_| ())
}

fn pci_host_fallback_dma_context(
    info: &PciHostBridgeInfo,
) -> Result<DmaContext, PciHostBridgeError> {
    pci_dma_context(
        info.dma_coherent,
        info.dma.windows.as_ref(),
        info.dma.unsupported,
    )
    .map(|context| context.with_preferred_numa_node(info.numa_node_id))
}

fn pci_dma_context(
    coherent: bool,
    windows: Option<&Vec<DmaWindow>>,
    unsupported: bool,
) -> Result<DmaContext, PciHostBridgeError> {
    let constraints = DmaConstraints {
        address_mask: usize::MAX,
        max_segment_size: usize::MAX,
        max_segments: 1,
        coherent,
        supports_scatter_gather: false,
        bounce: DmaBouncePolicy::Disabled,
    };
    if unsupported {
        return Ok(DmaContext::blocked(constraints));
    }
    let Some(windows) = windows else {
        return Ok(DmaContext::with_constraints(constraints));
    };
    if windows.is_empty() {
        return Ok(DmaContext::with_constraints(constraints));
    }
    if !valid_dma_windows(windows) {
        return Err(PciHostBridgeError::Invalid);
    }
    // DmaContext 共享拥有窗口；host 热移除后既有设备上下文仍保持有效，live overlay
    // 重建则可在最后一个上下文释放后回收旧窗口。
    Ok(DmaContext::with_owned_windows(
        constraints,
        Arc::from(windows.clone().into_boxed_slice()),
    ))
}

fn valid_dma_windows(windows: &[DmaWindow]) -> bool {
    for (index, window) in windows.iter().enumerate() {
        if window.size == 0
            || window.cpu_start.checked_add(window.size).is_none()
            || window.dma_start.checked_add(window.size).is_none()
        {
            return false;
        }
        for previous in &windows[..index] {
            let Some(previous_end) = previous.cpu_start.checked_add(previous.size) else {
                return false;
            };
            let Some(window_end) = window.cpu_start.checked_add(window.size) else {
                return false;
            };
            if previous.cpu_start < window_end && window.cpu_start < previous_end {
                return false;
            }
        }
    }
    true
}

const fn pci_bus_ranges_overlap(a_start: u8, a_end: u8, b_start: u8, b_end: u8) -> bool {
    a_start <= b_end && b_start <= a_end
}

fn pci_host_bridge_covers(bridge: &PciHostBridgeRegistration, segment: u16, bus: u8) -> bool {
    bridge.info.domain == segment && bus >= bridge.info.bus_start && bus <= bridge.info.bus_end
}

fn snapshot_host_bridge(bridge: &PciHostBridgeRegistration) -> PciHostBridgeSnapshot {
    PciHostBridgeSnapshot {
        handle: bridge.handle,
        info: bridge.info.clone(),
        pnp: bridge.pnp.as_ref().map(Arc::clone),
    }
}

fn attach_to_host_bridge(dev: &Arc<PnpDevice>) {
    let (segment, bus) = match &dev.id {
        PnpId::Pci { segment, bus, .. } => (*segment, *bus),
        _ => return,
    };
    let Some(host) = host_bridge_pnp_for(segment, bus) else {
        return;
    };
    if dev.parent().is_some() || Arc::ptr_eq(&host, dev) {
        return;
    }
    // 拓扑挂接失败只表示当前设备已经被其它总线关系占用或正处于生命周期转换中；
    // PCI function 的发现与驱动 probe 不应因此被撤销。
    let _ = host.attach_child(dev);
}

// ── PCI config space 常量 ────────────────────────────────────────────────

const PCI_COMMAND_OFFSET: u16 = 0x04;
const PCI_STATUS_OFFSET: u16 = 0x06;
const PCI_CAPABILITY_LIST_OFFSET: u16 = 0x34;
const PCI_INTERRUPT_LINE_OFFSET: u16 = 0x3c;
const PCI_INTERRUPT_PIN_OFFSET: u16 = 0x3d;

const PCI_COMMAND_IO_SPACE: u16 = 0x0001;
const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;
const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;
const PCI_COMMAND_INTERRUPT_DISABLE: u16 = 0x0400;
const PCI_STATUS_CAPABILITIES_LIST: u16 = 0x0010;

/// PCIe ECAM 下每个 function 暴露的完整配置空间大小。
pub const PCI_EXTENDED_CONFIG_SPACE_SIZE: u16 = 0x1000;
const PCI_CAPABILITY_MIN_OFFSET: u16 = 0x40;
const PCI_CAPABILITY_MAX_OFFSET: u16 = 0xFC;
const PCI_CAPABILITY_HEADER_SIZE: u16 = 2;
const PCI_CAPABILITY_MAX_STEPS: usize =
    ((PCI_CAPABILITY_MAX_OFFSET - PCI_CAPABILITY_MIN_OFFSET) / 4 + 1) as usize;
const PCI_HEADER_TYPE_ENDPOINT: u8 = 0x00;
const PCI_HEADER_TYPE_BRIDGE: u8 = 0x01;
const PCI_ENDPOINT_BAR_COUNT: usize = 6;
const PCI_BRIDGE_BAR_COUNT: usize = 2;
const PCI_BAR0_OFFSET: u16 = 0x10;
const PCI_BAR_STRIDE: u16 = 4;
const PCI_BAR_IO_SPACE: u32 = 0x1;
const PCI_BAR_IO_ADDR_MASK: u32 = 0xffff_fffc;
const PCI_BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;
const PCI_BAR_MEM_PREFETCHABLE: u32 = 0x8;
const PCI_BAR_MEM_TYPE_SHIFT: u32 = 1;
const PCI_BAR_MEM_TYPE_MASK: u32 = 0x3;
const PCI_BAR_MEM_TYPE_32: u32 = 0;
const PCI_BAR_MEM_TYPE_64: u32 = 2;
const PCI_HEADER_TYPE_MULTI_FUNCTION: u8 = 0x80;
const PCI_INVALID_VENDOR_ID: u16 = 0xffff;
const PCI_INVALID_INTERRUPT_LINE: u8 = 0xff;
const PCI_MSI_CAPABILITY_ID: u8 = 0x05;
const PCI_MSIX_CAPABILITY_ID: u8 = 0x11;
const PCI_MSI_CONTROL_OFFSET: u16 = 2;
const PCI_MSI_CONTROL_ENABLE: u16 = 0x0001;
const PCI_MSI_CONTROL_MULTI_MESSAGE_ENABLE_MASK: u16 = 0x0070;
const PCI_MSI_CONTROL_64BIT_CAPABLE: u16 = 0x0080;
const PCI_MSI_MESSAGE_ADDRESS_LO_OFFSET: u16 = 0x04;
const PCI_MSI_MESSAGE_ADDRESS_HI_OFFSET: u16 = 0x08;
const PCI_MSI_MESSAGE_DATA_32_OFFSET: u16 = 0x08;
const PCI_MSI_MESSAGE_DATA_64_OFFSET: u16 = 0x0c;
const PCI_MSIX_CONTROL_OFFSET: u16 = 0x02;
const PCI_MSIX_TABLE_OFFSET: u16 = 0x04;
const PCI_MSIX_CONTROL_TABLE_SIZE_MASK: u16 = 0x07ff;
const PCI_MSIX_CONTROL_FUNCTION_MASK: u16 = 0x4000;
const PCI_MSIX_CONTROL_ENABLE: u16 = 0x8000;
const PCI_MSIX_BIR_MASK: u32 = 0x7;
const PCI_MSIX_TABLE_ADDR_MASK: u32 = !PCI_MSIX_BIR_MASK;
const PCI_MSIX_ENTRY_SIZE: usize = 16;
const PCI_MSIX_ENTRY_ADDR_LO: usize = 0;
const PCI_MSIX_ENTRY_ADDR_HI: usize = 4;
const PCI_MSIX_ENTRY_DATA: usize = 8;
const PCI_MSIX_ENTRY_VECTOR_CONTROL: usize = 12;
const PCI_MSIX_ENTRY_MASKED: u32 = 1;
/// PCI 常规配置空间每条 bus 最多有 32 个 device 编号。
pub const PCI_DEVICES_PER_BUS: u8 = 32;
/// 每个 PCI device 最多有 8 个 function，0 号 function 必须先探测。
pub const PCI_FUNCTIONS_PER_DEVICE: u8 = 8;

/// 根据 BAR 探测阶段返回的地址掩码计算资源长度。
///
/// 32 位 BAR 必须先在 32 位宽度内取反；若先提升为 `u64`，高 32 位会被错误地
/// 一并取反，从而把例如 `0xffff_f000` 误解为一个接近 16 EiB 的资源窗口。
fn pci_bar_size_from_mask(mask: u64, is_64: bool) -> Option<u64> {
    let size = if is_64 {
        if mask == 0 {
            return None;
        }
        (!mask).wrapping_add(1)
    } else {
        let mask = u32::try_from(mask).ok()?;
        if mask == 0 {
            return None;
        }
        u64::from((!mask).wrapping_add(1))
    };

    size.is_power_of_two().then_some(size)
}

// ── PCI config space 访问回调 ────────────────────────────────────────────

/// PCI endpoint 中断路由解析回调。
///
/// PCI 驱动只知道 function 自身的 interrupt pin/line 配置；真正应注册到哪条
/// 规范化 IRQ line 由 host bridge 或固件路由决定。平台层可在安装 config
/// access 时提供该回调，设备驱动只消费解析后的 [`IrqLine`]。
pub type PciIrqResolver = fn(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    interrupt_pin: Option<u8>,
    interrupt_line: Option<u8>,
) -> Option<IrqLine>;

/// PCI endpoint MSI 分配回调。
///
/// Host bridge 负责把 PCI requester id 经固件 `msi-map` 路由到具体 MSI
/// controller。PCI 设备层只拿到已经分配好的 message/line，不理解平台 MSI
/// doorbell 地址或 vector 池。
pub type PciMsiAllocator =
    fn(segment: u16, bus: u8, device: u8, function: u8) -> Option<msi::MsiHandle>;

/// 一段 BAR 经 host bridge 地址窗口翻译后的运行时映射。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciBarMapping {
    pub cpu_phys: usize,
    pub virt_addr: usize,
}

/// PCI BAR 子地址翻译回调。
///
/// DT/ACPI host bridge 可以同时存在多个 segment 与 bus-range，因此翻译必须携带
/// 完整 BDF，不能只依赖一个全局 MMIO base。`pci_addr` 是配置空间 BAR 中的原值。
pub type PciBarMapper = fn(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    bar_type: PciBarType,
    prefetchable: bool,
    pci_addr: u64,
    size: u64,
) -> Option<PciBarMapping>;

#[derive(Clone, Copy)]
pub struct PciConfigAccess {
    pub read_u8: fn(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
    ) -> Result<u8, PciConfigError>,
    pub read_u16: fn(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
    ) -> Result<u16, PciConfigError>,
    pub read_u32: fn(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
    ) -> Result<u32, PciConfigError>,
    pub write_u8: fn(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u8,
    ) -> Result<(), PciConfigError>,
    pub write_u16: fn(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u16,
    ) -> Result<(), PciConfigError>,
    pub write_u32: fn(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) -> Result<(), PciConfigError>,
    pub device_mmio_to_virt: fn(phys_addr: usize) -> usize,
    pub resolve_irq: Option<PciIrqResolver>,
    pub allocate_msi: Option<PciMsiAllocator>,
}

static PCI_CONFIG: Spinlock<Option<PciConfigAccess>> = Spinlock::new(None);
static PCI_BAR_MAPPER: Spinlock<Option<PciBarMapper>> = Spinlock::new(None);

#[kernel_symbols::export(
    name = "general.dev.pci.set_pci_config_access",
    contract = "kernel.general.pci-admin@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn set_pci_config_access(access: PciConfigAccess) {
    if super::elm_lifecycle::install_pci_config_access(access).is_err() {
        log::error!("[pci] ELM PCI config 操作安装失败，原操作保持不变");
    }
}

/// 安装 PCI BAR 子地址到 CPU 地址的扩展回调。
///
/// 该入口属于 `@2` 扩展，避免把 `map_bar` 字段追加到既有
/// `PciConfigAccess@1` 结构而破坏旧 ELM 模块的 Rust 布局 ABI。
#[kernel_symbols::export(
    name = "general.dev.pci.set_pci_bar_mapper",
    contract = "kernel.general.pci-admin@2",
    version = 2,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn set_pci_bar_mapper(mapper: Option<PciBarMapper>) {
    if super::elm_lifecycle::install_pci_bar_mapper(mapper).is_err() {
        log::error!("[pci] ELM PCI BAR 映射回调安装失败，原回调保持不变");
    }
}

/// 供内建固件后端原子安装 config access 与 BAR mapper。
///
/// 这两组回调必须同时可用；ELM 拒绝 owned-resource 跟踪时会一起回滚。
/// 对外 ELM ABI 仍保留上面的 `@1`/`@2` 单项安装入口。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pci.try_install_pci_access_pair",
    contract = "kernel.general.pci-admin@2",
    version = 2,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn try_install_pci_access_pair(
    access: PciConfigAccess,
    mapper: PciBarMapper,
) -> Result<(), ()> {
    super::elm_lifecycle::install_pci_access_pair(access, mapper)
}

pub(crate) fn replace_pci_config_access(
    access: Option<PciConfigAccess>,
) -> Option<PciConfigAccess> {
    let mut current = PCI_CONFIG.lock();
    core::mem::replace(&mut *current, access)
}

pub(crate) fn replace_pci_bar_mapper(mapper: Option<PciBarMapper>) -> Option<PciBarMapper> {
    let mut current = PCI_BAR_MAPPER.lock();
    core::mem::replace(&mut *current, mapper)
}

pub(crate) fn replace_pci_access_pair(
    access: Option<PciConfigAccess>,
    mapper: Option<PciBarMapper>,
) -> (Option<PciConfigAccess>, Option<PciBarMapper>) {
    // 所有双后端读写都按 config -> mapper 的顺序持锁。config guard 最后释放，
    // 因此任何随后观察到新 config 的调用也一定能观察到配套的新 mapper。
    let mut current_access = PCI_CONFIG.lock();
    let mut current_mapper = PCI_BAR_MAPPER.lock();
    let previous_access = core::mem::replace(&mut *current_access, access);
    let previous_mapper = core::mem::replace(&mut *current_mapper, mapper);
    (previous_access, previous_mapper)
}

fn with_pci_access_pair<R>(
    callback: impl FnOnce(&PciConfigAccess, Option<PciBarMapper>) -> R,
) -> Option<R> {
    let current_access = PCI_CONFIG.lock();
    let access = current_access.as_ref()?;
    let current_mapper = PCI_BAR_MAPPER.lock();
    Some(callback(access, *current_mapper))
}

#[kernel_symbols::export(
    name = "general.dev.pci.config_read_u8",
    contract = "kernel.general.pci-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
)]
pub fn config_read_u8(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> Result<u8, PciConfigError> {
    let guard = PCI_CONFIG.lock();
    let config = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
    (config.read_u8)(segment, bus, device, function, offset)
}

#[kernel_symbols::export(
    name = "general.dev.pci.config_read_u16",
    contract = "kernel.general.pci-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
)]
pub fn config_read_u16(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> Result<u16, PciConfigError> {
    let guard = PCI_CONFIG.lock();
    let config = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
    (config.read_u16)(segment, bus, device, function, offset)
}

#[kernel_symbols::export(
    name = "general.dev.pci.config_read_u32",
    contract = "kernel.general.pci-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
)]
pub fn config_read_u32(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> Result<u32, PciConfigError> {
    let guard = PCI_CONFIG.lock();
    let config = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
    (config.read_u32)(segment, bus, device, function, offset)
}

#[kernel_symbols::export(
    name = "general.dev.pci.config_write_u8",
    contract = "kernel.general.pci-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn config_write_u8(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    value: u8,
) -> Result<(), PciConfigError> {
    let guard = PCI_CONFIG.lock();
    let config = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
    (config.write_u8)(segment, bus, device, function, offset, value)
}

#[kernel_symbols::export(
    name = "general.dev.pci.config_write_u16",
    contract = "kernel.general.pci-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn config_write_u16(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    value: u16,
) -> Result<(), PciConfigError> {
    let guard = PCI_CONFIG.lock();
    let config = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
    (config.write_u16)(segment, bus, device, function, offset, value)
}

#[kernel_symbols::export(
    name = "general.dev.pci.config_write_u32",
    contract = "kernel.general.pci-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn config_write_u32(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    value: u32,
) -> Result<(), PciConfigError> {
    let guard = PCI_CONFIG.lock();
    let config = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
    (config.write_u32)(segment, bus, device, function, offset, value)
}

#[kernel_symbols::export(
    name = "general.dev.pci.device_mmio_to_virt",
    contract = "kernel.general.pci-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn device_mmio_to_virt(physical_address: usize) -> Result<usize, PciConfigError> {
    let guard = PCI_CONFIG.lock();
    let config = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
    Ok((config.device_mmio_to_virt)(physical_address))
}

#[kernel_symbols::export(
    name = "general.dev.pci.resolve_irq",
    contract = "kernel.general.pci-route@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
)]
pub fn resolve_irq(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    interrupt_pin: Option<u8>,
    interrupt_line: Option<u8>,
) -> Option<IrqLine> {
    let resolver = {
        let guard = PCI_CONFIG.lock();
        guard.as_ref()?.resolve_irq?
    };
    resolver(
        segment,
        bus,
        device,
        function,
        interrupt_pin,
        interrupt_line,
    )
}

#[kernel_symbols::export(
    name = "general.dev.pci.allocate_msi",
    contract = "kernel.general.pci-route@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn allocate_msi(segment: u16, bus: u8, device: u8, function: u8) -> Option<msi::MsiHandle> {
    let allocator = {
        let guard = PCI_CONFIG.lock();
        guard.as_ref()?.allocate_msi?
    };
    allocator(segment, bus, device, function)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciConfigError {
    InvalidDevice,
    InvalidOffset,
    Uninitialized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciMsiError {
    NotSupported,
    NoAllocator,
    AllocationFailed,
    AddressUnsupported,
    DoorbellUnmappable,
    DataUnsupported,
    Config(PciConfigError),
}

impl From<PciConfigError> for PciMsiError {
    fn from(err: PciConfigError) -> Self {
        Self::Config(err)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciMsiHandle {
    cap_offset: u16,
    allocation: msi::MsiHandle,
}

struct PciMsiDoorbellRegistration {
    handle: PciMsiHandle,
    mapping: DmaAddressMapping,
}

static PCI_MSI_DOORBELLS: Spinlock<Vec<PciMsiDoorbellRegistration>> = Spinlock::new(Vec::new());

impl PciMsiHandle {
    pub const fn line(self) -> IrqLine {
        self.allocation.line()
    }

    pub const fn message(self) -> msi::MsiMessage {
        self.allocation.message()
    }
}

/// PCI MSI 配置资源。
///
/// 释放 MSI vector 前必须先清设备配置空间中的 MSI enable 位；因此它不能只保存
/// 底层 MSI handle，而要同时持有对应的 PCI function 访问对象。
pub struct PciMsiPnpResource {
    pci: PciDevice,
    handle: PciMsiHandle,
    label: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciMsixError {
    NotSupported,
    InvalidCount,
    InvalidTable,
    BarUnavailable,
    BarTooSmall,
    NoAllocator,
    AllocationFailed,
    DoorbellUnmappable,
    Config(PciConfigError),
}

impl From<PciConfigError> for PciMsixError {
    fn from(error: PciConfigError) -> Self {
        Self::Config(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PciMsixVector {
    table_index: u16,
    allocation: msi::MsiHandle,
}

pub struct PciMsixSet {
    cap_offset: u16,
    table_vaddr: usize,
    vectors: Box<[PciMsixVector]>,
    doorbells: Box<[DmaAddressMapping]>,
}

#[kernel_symbols::export]
impl PciMsixSet {
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciMsixSet.line",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn line(&self, index: usize) -> Option<IrqLine> {
        self.vectors
            .get(index)
            .map(|vector| vector.allocation.line())
    }
}

struct PciMsixPnpResource {
    pci: PciDevice,
    set: PciMsixSet,
    label: &'static str,
}

impl PciMsixPnpResource {
    const fn new(pci: PciDevice, set: PciMsixSet, label: &'static str) -> Self {
        Self { pci, set, label }
    }
}

impl PnpResource for PciMsixPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Msi
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        let kind = self.kind();
        let label = self.label;
        if self.pci.release_configured_msix_inner(self.set) {
            Ok(())
        } else {
            Err(PnpResourceReleaseError::new(
                kind,
                label,
                "MSI-X doorbell IOMMU mapping could not be revoked",
            ))
        }
    }
}

/// 将已配置的 MSI-X set 交给 PnP 设备管理。
///
/// 资源对象及其 trait vtable 均在常驻内核侧构造；登记失败时会先关闭 MSI-X、
/// 释放 vector，再把错误返回给动态 ELM。
#[kernel_symbols::export(
    name = "general.dev.pci.attach_msix_pnp_resource",
    contract = "kernel.general.pci-route@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
        | kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
    retained_args = 1u64 << 3
)]
pub fn attach_msix_pnp_resource(
    dev: &Arc<PnpDevice>,
    pci: PciDevice,
    set: PciMsixSet,
    label: &'static str,
) -> Result<(), PnpError> {
    dev.own_boxed_resource_or_release(Box::new(PciMsixPnpResource::new(pci, set, label)))
}

#[kernel_symbols::export]
impl PciMsiPnpResource {
    pub const fn new(pci: PciDevice, handle: PciMsiHandle, label: &'static str) -> Self {
        Self { pci, handle, label }
    }

    /// 在常驻内核侧构造完成类型擦除的 MSI 资源。
    ///
    /// 这样动态 ELM 不需要链接 `PciMsiPnpResource` 的私有 trait vtable，资源仍由
    /// PnP 设备按统一逆序释放规则管理。
    #[kernel_symbols::export(
        name = "general.dev.pci.PciMsiPnpResource.boxed",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
            | kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn boxed(
        pci: PciDevice,
        handle: PciMsiHandle,
        label: &'static str,
    ) -> Box<dyn PnpResource> {
        Box::new(Self::new(pci, handle, label))
    }
}

impl PnpResource for PciMsiPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Msi
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        if self.pci.release_configured_msi_inner(self.handle) {
            Ok(())
        } else {
            Err(PnpResourceReleaseError::new(
                self.kind(),
                self.label,
                "MSI doorbell IOMMU mapping could not be revoked",
            ))
        }
    }
}

// ── PCI capability 遍历 ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciCapability {
    pub offset: u16,
    pub id: u8,
    pub next_offset: Option<u16>,
}

pub struct PciCapabilityIter<'a> {
    device: &'a PciDevice,
    next_offset: Option<u16>,
    remaining: usize,
    visited: u64,
}

fn pci_config_range_valid(offset: u16, len: u16) -> bool {
    offset <= PCI_EXTENDED_CONFIG_SPACE_SIZE
        && len <= PCI_EXTENDED_CONFIG_SPACE_SIZE.saturating_sub(offset)
}

fn pci_config_access_valid(offset: u16, len: u16, align: u16) -> bool {
    pci_config_range_valid(offset, len) && (offset & (align - 1)) == 0
}

fn valid_capability_offset(offset: u16) -> bool {
    offset >= PCI_CAPABILITY_MIN_OFFSET
        && offset <= PCI_CAPABILITY_MAX_OFFSET
        && offset & 0x3 == 0
        && pci_config_range_valid(offset, PCI_CAPABILITY_HEADER_SIZE)
}

fn valid_capability_pointer(raw: u8) -> Option<u16> {
    let offset = raw as u16;
    if offset == 0 || !valid_capability_offset(offset) {
        return None;
    }
    Some(offset)
}

fn capability_visited_bit(offset: u16) -> Option<u64> {
    if !valid_capability_offset(offset) {
        return None;
    }
    let idx = (offset - PCI_CAPABILITY_MIN_OFFSET) / 4;
    Some(1u64 << idx as u32)
}

// ── PciDevice ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct PciDevice {
    pnp: Arc<PnpDevice>,
}

impl fmt::Debug for PciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PciDevice")
            .field("pnp_id", &self.pnp.id)
            .field("name", &self.pnp.name)
            .finish()
    }
}

#[kernel_symbols::export]
impl PciDevice {
    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.from_pnp",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn from_pnp(pnp: &Arc<PnpDevice>) -> Option<Self> {
        let PnpId::Pci { .. } = pnp.id else {
            return None;
        };

        pnp.info.as_any().downcast_ref::<PciInfo>()?;

        Some(Self {
            pnp: Arc::clone(pnp),
        })
    }

    pub fn pnp(&self) -> &Arc<PnpDevice> {
        &self.pnp
    }

    pub fn pnp_id(&self) -> &PnpId {
        &self.pnp.id
    }

    pub fn info(&self) -> Option<&PciInfo> {
        self.pnp.info.as_any().downcast_ref::<PciInfo>()
    }

    /// 返回总线枚举阶段固化的 function 级固件元数据。
    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.firmware_info",
        contract = "kernel.general.pci-firmware@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
            | kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn firmware_info(&self) -> Option<PciFunctionFirmwareInfo> {
        let (segment, bus, device, function) = self.bdf()?;
        pci_function_firmware_info(segment, bus, device, function)
    }

    /// 返回该 PCI function 的 DMA 上下文。
    ///
    /// DMA 能力来自设备所在 host bridge，而不是全局静态假设。`dma-ranges` 已转换
    /// 为 per-host 窗口；IOMMU domain 在首次 DMA map 时建立，provider 未就绪时
    /// fail closed，并由同一个缓存上下文在后续映射时重试。
    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.dma_context",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA
    )]
    pub fn dma_context(&self) -> DmaContext {
        let Some((segment, bus, device, function)) = self.bdf() else {
            return DmaContext::default_coherent();
        };
        host_bridge_dma_context_for_bdf(segment, bus, device, function)
            .unwrap_or_else(|| DmaContext::blocked(DmaConstraints::coherent_identity()))
    }

    fn bdf(&self) -> Option<(u16, u8, u8, u8)> {
        match self.pnp.id {
            PnpId::Pci {
                segment,
                bus,
                device,
                function,
            } => Some((segment, bus, device, function)),
            _ => None,
        }
    }

    // ── Config space 访问 ──

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_read_config_u8",
        contract = "kernel.general.pci-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_BUS
    )]
    pub fn try_read_config_u8(&self, offset: u16) -> Result<u8, PciConfigError> {
        if !pci_config_access_valid(offset, 1, 1) {
            return Err(PciConfigError::InvalidOffset);
        }
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.read_u8)(seg, bus, dev, func, offset)
    }

    pub fn try_read_config_u16(&self, offset: u16) -> Result<u16, PciConfigError> {
        if !pci_config_access_valid(offset, 2, 2) {
            return Err(PciConfigError::InvalidOffset);
        }
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.read_u16)(seg, bus, dev, func, offset)
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_read_config_u32",
        contract = "kernel.general.pci-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_BUS
    )]
    pub fn try_read_config_u32(&self, offset: u16) -> Result<u32, PciConfigError> {
        if !pci_config_access_valid(offset, 4, 4) {
            return Err(PciConfigError::InvalidOffset);
        }
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.read_u32)(seg, bus, dev, func, offset)
    }

    pub fn try_write_config_u8(&self, offset: u16, value: u8) -> Result<(), PciConfigError> {
        if !pci_config_access_valid(offset, 1, 1) {
            return Err(PciConfigError::InvalidOffset);
        }
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.write_u8)(seg, bus, dev, func, offset, value)
    }

    pub fn try_write_config_u16(&self, offset: u16, value: u16) -> Result<(), PciConfigError> {
        if !pci_config_access_valid(offset, 2, 2) {
            return Err(PciConfigError::InvalidOffset);
        }
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.write_u16)(seg, bus, dev, func, offset, value)
    }

    pub fn try_write_config_u32(&self, offset: u16, value: u32) -> Result<(), PciConfigError> {
        if !pci_config_access_valid(offset, 4, 4) {
            return Err(PciConfigError::InvalidOffset);
        }
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.write_u32)(seg, bus, dev, func, offset, value)
    }

    // 这些兼容读取只用于容错查询；probe/初始化路径需要区分错误时应使用 try_*。
    pub fn read_config_u8(&self, offset: u16) -> u8 {
        self.try_read_config_u8(offset).unwrap_or(0)
    }

    pub fn read_config_u16(&self, offset: u16) -> u16 {
        self.try_read_config_u16(offset).unwrap_or(0)
    }

    pub fn read_config_u32(&self, offset: u16) -> u32 {
        self.try_read_config_u32(offset).unwrap_or(0)
    }

    pub fn write_config_u8(&self, offset: u16, value: u8) {
        let _ = self.try_write_config_u8(offset, value);
    }

    pub fn write_config_u16(&self, offset: u16, value: u16) {
        let _ = self.try_write_config_u16(offset, value);
    }

    pub fn write_config_u32(&self, offset: u16, value: u32) {
        let _ = self.try_write_config_u32(offset, value);
    }

    // ── BAR ──

    pub fn map_bar(&self, idx: usize) -> Option<PciBar> {
        self.map_bar_with_virt(idx).map(|(bar, _)| bar)
    }

    fn map_bar_with_virt(&self, idx: usize) -> Option<(PciBar, usize)> {
        let bar_count = self.bar_count();
        if idx >= bar_count {
            return None;
        }
        let (segment, bus, device, function) = self.bdf()?;
        with_pci_access_pair(|cfg, mapper| {
            let read_u16 = |offset| (cfg.read_u16)(segment, bus, device, function, offset).ok();
            let read_u32 = |offset| (cfg.read_u32)(segment, bus, device, function, offset).ok();
            let write_u16 =
                |offset, value| (cfg.write_u16)(segment, bus, device, function, offset, value).ok();
            let write_u32 =
                |offset, value| (cfg.write_u32)(segment, bus, device, function, offset, value).ok();

            let offset = PCI_BAR0_OFFSET + (idx as u16) * PCI_BAR_STRIDE;
            let bar_val = read_u32(offset)?;
            if bar_val == 0 {
                return None;
            }

            let is_mmio = bar_val & PCI_BAR_IO_SPACE == 0;
            let prefetchable = is_mmio && (bar_val & PCI_BAR_MEM_PREFETCHABLE) != 0;
            let (bar_type, pci_addr, size) = if is_mmio {
                let is_64 = match (bar_val >> PCI_BAR_MEM_TYPE_SHIFT) & PCI_BAR_MEM_TYPE_MASK {
                    PCI_BAR_MEM_TYPE_32 => false,
                    PCI_BAR_MEM_TYPE_64 if idx + 1 < bar_count => true,
                    _ => return None,
                };
                let high_offset = offset + PCI_BAR_STRIDE;
                let high_val = if is_64 { read_u32(high_offset)? } else { 0 };
                let pci_addr =
                    ((high_val as u64) << 32) | ((bar_val & PCI_BAR_MEM_ADDR_MASK) as u64);

                let cmd = read_u16(PCI_COMMAND_OFFSET)?;
                write_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_MEMORY_SPACE)?;

                let size_bits = (|| -> Option<u64> {
                    if is_64 {
                        write_u32(high_offset, u32::MAX)?;
                    }
                    write_u32(offset, PCI_BAR_MEM_ADDR_MASK)?;
                    let size_lo = read_u32(offset)? & PCI_BAR_MEM_ADDR_MASK;
                    let size_hi = if is_64 { read_u32(high_offset)? } else { 0 };
                    Some(((size_hi as u64) << 32) | size_lo as u64)
                })();

                let _ = write_u32(offset, bar_val);
                if is_64 {
                    let _ = write_u32(high_offset, high_val);
                }
                let _ = write_u16(PCI_COMMAND_OFFSET, cmd);

                let size = pci_bar_size_from_mask(size_bits?, is_64)?;
                (PciBarType::Memory, pci_addr, size)
            } else {
                let pci_addr = (bar_val & PCI_BAR_IO_ADDR_MASK) as u64;
                let cmd = read_u16(PCI_COMMAND_OFFSET)?;
                write_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_IO_SPACE)?;

                let size_bits = (|| -> Option<u32> {
                    write_u32(offset, PCI_BAR_IO_ADDR_MASK)?;
                    Some(read_u32(offset)? & PCI_BAR_IO_ADDR_MASK)
                })();

                let _ = write_u32(offset, bar_val);
                let _ = write_u16(PCI_COMMAND_OFFSET, cmd);

                let size = pci_bar_size_from_mask(u64::from(size_bits?), false)?;
                (PciBarType::Io, pci_addr, size)
            };

            let mapping = if let Some(mapper) = mapper {
                mapper(
                    segment,
                    bus,
                    device,
                    function,
                    bar_type,
                    prefetchable,
                    pci_addr,
                    size,
                )?
            } else {
                let cpu_phys = usize::try_from(pci_addr).ok()?;
                PciBarMapping {
                    cpu_phys,
                    virt_addr: match bar_type {
                        PciBarType::Memory => (cfg.device_mmio_to_virt)(cpu_phys),
                        PciBarType::Io => cpu_phys,
                    },
                }
            };

            Some((
                PciBar {
                    idx,
                    bar_type,
                    prefetchable,
                    phys_addr: mapping.cpu_phys as u64,
                    size,
                },
                mapping.virt_addr,
            ))
        })
        .flatten()
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.map_bar_virt",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn map_bar_virt(&self, idx: usize) -> Option<(PciBar, usize)> {
        self.map_bar_with_virt(idx)
    }

    // ── 设备控制 ──

    pub fn try_command(&self) -> Result<u16, PciConfigError> {
        self.try_read_config_u16(PCI_COMMAND_OFFSET)
    }

    pub fn command(&self) -> u16 {
        self.try_command().unwrap_or(0)
    }

    pub fn try_set_command(&self, command: u16) -> Result<(), PciConfigError> {
        self.try_write_config_u16(PCI_COMMAND_OFFSET, command)
    }

    pub fn set_command(&self, command: u16) {
        let _ = self.try_set_command(command);
    }

    fn try_update_command(&self, set: u16, clear: u16) -> Result<(), PciConfigError> {
        let cmd = self.try_command()?;
        self.try_set_command((cmd | set) & !clear)
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_enable_bus_master",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_BUS,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn try_enable_bus_master(&self) -> Result<(), PciConfigError> {
        self.try_update_command(PCI_COMMAND_BUS_MASTER, 0)
    }

    pub fn enable_bus_master(&self) {
        let _ = self.try_enable_bus_master();
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_disable_bus_master",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_BUS,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn try_disable_bus_master(&self) -> Result<(), PciConfigError> {
        self.try_update_command(0, PCI_COMMAND_BUS_MASTER)
    }

    pub fn disable_bus_master(&self) {
        let _ = self.try_disable_bus_master();
    }

    pub fn bus_master_enabled(&self) -> bool {
        self.try_command()
            .is_ok_and(|cmd| cmd & PCI_COMMAND_BUS_MASTER != 0)
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_enable_mmio",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_BUS,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn try_enable_mmio(&self) -> Result<(), PciConfigError> {
        self.try_update_command(PCI_COMMAND_MEMORY_SPACE, 0)
    }

    pub fn enable_mmio(&self) {
        let _ = self.try_enable_mmio();
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_disable_mmio",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_BUS,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn try_disable_mmio(&self) -> Result<(), PciConfigError> {
        self.try_update_command(0, PCI_COMMAND_MEMORY_SPACE)
    }

    pub fn disable_mmio(&self) {
        let _ = self.try_disable_mmio();
    }

    pub fn try_enable_io(&self) -> Result<(), PciConfigError> {
        self.try_update_command(PCI_COMMAND_IO_SPACE, 0)
    }

    pub fn enable_io(&self) {
        let _ = self.try_enable_io();
    }

    pub fn try_disable_io(&self) -> Result<(), PciConfigError> {
        self.try_update_command(0, PCI_COMMAND_IO_SPACE)
    }

    pub fn disable_io(&self) {
        let _ = self.try_disable_io();
    }

    pub fn try_disable_interrupts(&self) -> Result<(), PciConfigError> {
        self.try_update_command(PCI_COMMAND_INTERRUPT_DISABLE, 0)
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.disable_interrupts",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn disable_interrupts(&self) {
        let _ = self.try_disable_interrupts();
    }

    pub fn try_enable_interrupts(&self) -> Result<(), PciConfigError> {
        self.try_update_command(0, PCI_COMMAND_INTERRUPT_DISABLE)
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.enable_interrupts",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn enable_interrupts(&self) {
        let _ = self.try_enable_interrupts();
    }

    // ── IRQ ──

    pub fn irq_line(&self) -> Option<u8> {
        let irq = self.try_read_config_u8(PCI_INTERRUPT_LINE_OFFSET).ok()?;
        if irq == 0 || irq == PCI_INVALID_INTERRUPT_LINE {
            None
        } else {
            Some(irq)
        }
    }

    pub fn irq_pin(&self) -> Option<u8> {
        let pin = self.try_read_config_u8(PCI_INTERRUPT_PIN_OFFSET).ok()?;
        if pin == 0 { None } else { Some(pin) }
    }

    /// 返回平台已解析好的规范化 IRQ line。
    ///
    /// 这里不把 PCI config space 的 interrupt line 字节直接解释为 CPU 中断号；
    /// 该字节只是一段桥接路由输入，具体含义必须由 host bridge/固件层解析。
    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.routed_irq_line",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn routed_irq_line(&self) -> Option<IrqLine> {
        let (segment, bus, device, function) = self.bdf()?;
        let pin = self.irq_pin();
        let line = self.irq_line();
        // 路由后端会跨 ELM 并进入 IRQ domain；只在锁内复制函数指针，允许回调重入 PCI 查询。
        let resolver = {
            let guard = PCI_CONFIG.lock();
            guard.as_ref()?.resolve_irq?
        };
        resolver(segment, bus, device, function, pin, line)
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_configure_single_msi",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn try_configure_single_msi(&self) -> Result<PciMsiHandle, PciMsiError> {
        let cap_offset = self.msi_capability().ok_or(PciMsiError::NotSupported)?;
        let (segment, bus, device, function) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let allocator = {
            let guard = PCI_CONFIG.lock();
            guard
                .as_ref()
                .and_then(|config| config.allocate_msi)
                .ok_or(PciMsiError::NoAllocator)?
        };
        let allocation =
            allocator(segment, bus, device, function).ok_or(PciMsiError::AllocationFailed)?;
        let (message, mapping) = self
            .map_msi_doorbell(allocation.message())
            .ok_or(PciMsiError::DoorbellUnmappable)
            .inspect_err(|_| {
                let _ = msi::free_msi(allocation);
            })?;
        if let Err(err) = self.program_single_msi(cap_offset, message) {
            drop(mapping);
            let _ = msi::free_msi(allocation);
            return Err(err);
        }
        let handle = PciMsiHandle {
            cap_offset,
            allocation,
        };
        let mut doorbells = PCI_MSI_DOORBELLS.lock();
        if doorbells.try_reserve(1).is_err() {
            drop(doorbells);
            let _ = self.try_msi_disable(cap_offset);
            drop(mapping);
            let _ = msi::free_msi(allocation);
            return Err(PciMsiError::AllocationFailed);
        }
        doorbells.push(PciMsiDoorbellRegistration { handle, mapping });
        Ok(handle)
    }

    pub fn configure_single_msi(&self) -> Option<PciMsiHandle> {
        self.try_configure_single_msi().ok()
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.release_configured_msi",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn release_configured_msi(&self, handle: PciMsiHandle) {
        if !self.release_configured_msi_inner(handle) {
            log::error!(
                "[pci] failed to revoke MSI doorbell mapping for {}",
                self.pnp.id
            );
        }
    }

    fn release_configured_msi_inner(&self, handle: PciMsiHandle) -> bool {
        // 只有确认设备侧 MSI 已关闭，才能撤销 doorbell 并释放可能被复用的 vector。
        if self.try_msi_disable(handle.cap_offset).is_err() {
            return false;
        }
        let registration = {
            let mut doorbells = PCI_MSI_DOORBELLS.lock();
            doorbells
                .iter()
                .position(|entry| entry.handle == handle)
                .map(|index| doorbells.swap_remove(index))
        };
        let Some(mut registration) = registration else {
            return false;
        };
        match registration.mapping.unmap() {
            Ok(()) => msi::free_msi(handle.allocation).is_ok(),
            Err(mapping) => {
                registration.mapping = mapping;
                // swap_remove 已留下至少一个空闲 capacity，恢复记录不再分配。
                PCI_MSI_DOORBELLS.lock().push(registration);
                false
            }
        }
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_enable_configured_msi",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn try_enable_configured_msi(&self, handle: PciMsiHandle) -> Result<(), PciMsiError> {
        let ctrl = self.try_read_config_u16(handle.cap_offset + PCI_MSI_CONTROL_OFFSET)?;
        self.try_write_config_u16(
            handle.cap_offset + PCI_MSI_CONTROL_OFFSET,
            (ctrl & !PCI_MSI_CONTROL_MULTI_MESSAGE_ENABLE_MASK) | PCI_MSI_CONTROL_ENABLE,
        )?;
        Ok(())
    }

    /// 禁用已经完成 message 编程、但仍保留 vector 所有权的 MSI。
    pub fn try_disable_configured_msi(&self, handle: PciMsiHandle) -> Result<(), PciMsiError> {
        self.try_msi_disable(handle.cap_offset)?;
        Ok(())
    }

    fn map_msi_doorbell(
        &self,
        message: msi::MsiMessage,
    ) -> Option<(msi::MsiMessage, DmaAddressMapping)> {
        let paddr = usize::try_from(message.address).ok()?;
        let mapping = self
            .dma_context()
            .map_identity_mmio(paddr, size_of::<u32>())?;
        let address = mapping.translated_addr(paddr, size_of::<u32>())? as u64;
        Some((
            msi::MsiMessage {
                address,
                data: message.data,
            },
            mapping,
        ))
    }

    pub fn bar_count(&self) -> usize {
        self.info().map(PciInfo::bar_count).unwrap_or(0)
    }

    // ── capability 遍历 ──

    pub fn capabilities_offset(&self) -> Option<u16> {
        let status = self.try_read_config_u16(PCI_STATUS_OFFSET).ok()?;
        if status & PCI_STATUS_CAPABILITIES_LIST == 0 {
            return None;
        }
        valid_capability_pointer(self.try_read_config_u8(PCI_CAPABILITY_LIST_OFFSET).ok()?)
    }

    pub fn capabilities(&self) -> PciCapabilityIter<'_> {
        PciCapabilityIter::new(self)
    }

    /// 返回 capability chain 的已校验快照。
    ///
    /// 快照在常驻 PCI 子系统内完成迭代，动态 ELM 不需要把
    /// `PciCapabilityIter::next` 的私有 trait 实现当成链接 ABI。
    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.capabilities_snapshot",
        contract = "kernel.general.pci-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn capabilities_snapshot(&self) -> Vec<PciCapability> {
        self.capabilities().collect()
    }

    pub fn find_capability(&self, cap_id: u8) -> Option<u16> {
        self.capabilities()
            .find(|cap| cap.id == cap_id)
            .map(|cap| cap.offset)
    }

    // ── MSI ──

    pub fn msi_capability(&self) -> Option<u16> {
        self.find_capability(PCI_MSI_CAPABILITY_ID)
    }

    pub fn try_msi_enable(&self, cap_offset: u16) -> Result<(), PciConfigError> {
        let msg_ctrl = self.try_read_config_u16(cap_offset + PCI_MSI_CONTROL_OFFSET)?;
        self.try_write_config_u16(
            cap_offset + PCI_MSI_CONTROL_OFFSET,
            msg_ctrl | PCI_MSI_CONTROL_ENABLE,
        )
    }

    pub fn msi_enable(&self, cap_offset: u16) {
        let _ = self.try_msi_enable(cap_offset);
    }

    pub fn try_msi_disable(&self, cap_offset: u16) -> Result<(), PciConfigError> {
        let msg_ctrl = self.try_read_config_u16(cap_offset + PCI_MSI_CONTROL_OFFSET)?;
        self.try_write_config_u16(
            cap_offset + PCI_MSI_CONTROL_OFFSET,
            msg_ctrl & !PCI_MSI_CONTROL_ENABLE,
        )
    }

    pub fn msi_disable(&self, cap_offset: u16) {
        let _ = self.try_msi_disable(cap_offset);
    }

    fn program_single_msi(
        &self,
        cap_offset: u16,
        message: msi::MsiMessage,
    ) -> Result<(), PciMsiError> {
        if message.data > u16::MAX as u32 {
            return Err(PciMsiError::DataUnsupported);
        }
        let ctrl = self.try_read_config_u16(cap_offset + PCI_MSI_CONTROL_OFFSET)?;
        let supports_64 = ctrl & PCI_MSI_CONTROL_64BIT_CAPABLE != 0;
        if !supports_64 && message.address > u32::MAX as u64 {
            return Err(PciMsiError::AddressUnsupported);
        }

        // 编程 MSI message 前先关 MSI enable，避免设备在 address/data 半更新时
        // 发出旧/新混合消息。这里只启用单 vector，multiple message enable 清零。
        self.try_write_config_u16(
            cap_offset + PCI_MSI_CONTROL_OFFSET,
            ctrl & !PCI_MSI_CONTROL_ENABLE,
        )?;
        self.try_write_config_u32(
            cap_offset + PCI_MSI_MESSAGE_ADDRESS_LO_OFFSET,
            message.address as u32,
        )?;
        let data_offset = if supports_64 {
            self.try_write_config_u32(
                cap_offset + PCI_MSI_MESSAGE_ADDRESS_HI_OFFSET,
                (message.address >> 32) as u32,
            )?;
            PCI_MSI_MESSAGE_DATA_64_OFFSET
        } else {
            PCI_MSI_MESSAGE_DATA_32_OFFSET
        };
        self.try_write_config_u16(cap_offset + data_offset, message.data as u16)?;
        self.try_write_config_u16(
            cap_offset + PCI_MSI_CONTROL_OFFSET,
            ctrl & !(PCI_MSI_CONTROL_MULTI_MESSAGE_ENABLE_MASK | PCI_MSI_CONTROL_ENABLE),
        )?;
        Ok(())
    }

    // ── MSI-X ──

    pub fn msix_capability(&self) -> Option<u16> {
        self.find_capability(PCI_MSIX_CAPABILITY_ID)
    }

    pub(crate) fn msix_table_size(&self) -> Option<u16> {
        let capability = self.msix_capability()?;
        let control = self
            .try_read_config_u16(capability + PCI_MSIX_CONTROL_OFFSET)
            .ok()?;
        Some((control & PCI_MSIX_CONTROL_TABLE_SIZE_MASK) + 1)
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_configure_msix",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn try_configure_msix(&self, count: u16) -> Result<PciMsixSet, PciMsixError> {
        let capability = self.msix_capability().ok_or(PciMsixError::NotSupported)?;
        let control = self.try_read_config_u16(capability + PCI_MSIX_CONTROL_OFFSET)?;
        let table_size = (control & PCI_MSIX_CONTROL_TABLE_SIZE_MASK) + 1;
        if count == 0 || count > table_size {
            return Err(PciMsixError::InvalidCount);
        }
        let table = self.try_read_config_u32(capability + PCI_MSIX_TABLE_OFFSET)?;
        let bir = (table & PCI_MSIX_BIR_MASK) as usize;
        let table_offset = (table & PCI_MSIX_TABLE_ADDR_MASK) as usize;
        let (bar, bar_vaddr) = self.map_bar_virt(bir).ok_or(PciMsixError::BarUnavailable)?;
        if !bar.is_memory() {
            return Err(PciMsixError::InvalidTable);
        }
        let table_bytes = usize::from(count)
            .checked_mul(PCI_MSIX_ENTRY_SIZE)
            .ok_or(PciMsixError::InvalidTable)?;
        if table_offset
            .checked_add(table_bytes)
            .is_none_or(|end| end > bar.size as usize)
        {
            return Err(PciMsixError::BarTooSmall);
        }
        let table_vaddr = bar_vaddr
            .checked_add(table_offset)
            .ok_or(PciMsixError::InvalidTable)?;
        let (segment, bus, device, function) = self.bdf().ok_or(PciMsixError::InvalidTable)?;
        let allocator = {
            let guard = PCI_CONFIG.lock();
            guard
                .as_ref()
                .and_then(|config| config.allocate_msi)
                .ok_or(PciMsixError::NoAllocator)?
        };

        self.try_write_config_u16(
            capability + PCI_MSIX_CONTROL_OFFSET,
            control | PCI_MSIX_CONTROL_ENABLE | PCI_MSIX_CONTROL_FUNCTION_MASK,
        )?;

        let mut vectors: Vec<PciMsixVector> = Vec::new();
        let mut doorbells: Vec<DmaAddressMapping> = Vec::new();
        if vectors.try_reserve_exact(count as usize).is_err() {
            let _ = self.try_write_config_u16(
                capability + PCI_MSIX_CONTROL_OFFSET,
                (control | PCI_MSIX_CONTROL_FUNCTION_MASK) & !PCI_MSIX_CONTROL_ENABLE,
            );
            return Err(PciMsixError::AllocationFailed);
        }
        if doorbells.try_reserve_exact(count as usize).is_err() {
            let _ = self.try_write_config_u16(
                capability + PCI_MSIX_CONTROL_OFFSET,
                (control | PCI_MSIX_CONTROL_FUNCTION_MASK) & !PCI_MSIX_CONTROL_ENABLE,
            );
            return Err(PciMsixError::AllocationFailed);
        }
        for table_index in 0..count {
            let Some(allocation) = allocator(segment, bus, device, function) else {
                for vector in vectors.drain(..) {
                    let _ = msi::free_msi(vector.allocation);
                }
                let _ = self.try_write_config_u16(
                    capability + PCI_MSIX_CONTROL_OFFSET,
                    (control | PCI_MSIX_CONTROL_FUNCTION_MASK) & !PCI_MSIX_CONTROL_ENABLE,
                );
                return Err(PciMsixError::AllocationFailed);
            };
            let original = allocation.message();
            let Some(paddr) = usize::try_from(original.address).ok() else {
                let _ = msi::free_msi(allocation);
                for vector in vectors.drain(..) {
                    let _ = msi::free_msi(vector.allocation);
                }
                let _ = self.try_write_config_u16(
                    capability + PCI_MSIX_CONTROL_OFFSET,
                    (control | PCI_MSIX_CONTROL_FUNCTION_MASK) & !PCI_MSIX_CONTROL_ENABLE,
                );
                drop(doorbells);
                return Err(PciMsixError::DoorbellUnmappable);
            };
            let translated = doorbells
                .iter()
                .find_map(|mapping| mapping.translated_addr(paddr, size_of::<u32>()));
            let address = if let Some(address) = translated {
                address
            } else {
                let Some(mapping) = self
                    .dma_context()
                    .map_identity_mmio(paddr, size_of::<u32>())
                else {
                    let _ = msi::free_msi(allocation);
                    for vector in vectors.drain(..) {
                        let _ = msi::free_msi(vector.allocation);
                    }
                    let _ = self.try_write_config_u16(
                        capability + PCI_MSIX_CONTROL_OFFSET,
                        (control | PCI_MSIX_CONTROL_FUNCTION_MASK) & !PCI_MSIX_CONTROL_ENABLE,
                    );
                    drop(doorbells);
                    return Err(PciMsixError::DoorbellUnmappable);
                };
                let Some(address) = mapping.translated_addr(paddr, size_of::<u32>()) else {
                    let _ = msi::free_msi(allocation);
                    for vector in vectors.drain(..) {
                        let _ = msi::free_msi(vector.allocation);
                    }
                    let _ = self.try_write_config_u16(
                        capability + PCI_MSIX_CONTROL_OFFSET,
                        (control | PCI_MSIX_CONTROL_FUNCTION_MASK) & !PCI_MSIX_CONTROL_ENABLE,
                    );
                    drop(mapping);
                    drop(doorbells);
                    return Err(PciMsixError::DoorbellUnmappable);
                };
                doorbells.push(mapping);
                address
            };
            let entry = table_vaddr + usize::from(table_index) * PCI_MSIX_ENTRY_SIZE;
            program_msix_entry(
                entry,
                msi::MsiMessage {
                    address: address as u64,
                    data: original.data,
                },
                true,
            );
            vectors.push(PciMsixVector {
                table_index,
                allocation,
            });
        }
        Ok(PciMsixSet {
            cap_offset: capability,
            table_vaddr,
            vectors: vectors.into_boxed_slice(),
            doorbells: doorbells.into_boxed_slice(),
        })
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.try_enable_configured_msix",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn try_enable_configured_msix(&self, set: &PciMsixSet) -> Result<(), PciMsixError> {
        for vector in set.vectors.iter().copied() {
            set_msix_entry_mask(set.table_vaddr, vector.table_index, false);
        }
        let control = self.try_read_config_u16(set.cap_offset + PCI_MSIX_CONTROL_OFFSET)?;
        self.try_write_config_u16(
            set.cap_offset + PCI_MSIX_CONTROL_OFFSET,
            (control | PCI_MSIX_CONTROL_ENABLE) & !PCI_MSIX_CONTROL_FUNCTION_MASK,
        )?;
        Ok(())
    }

    #[kernel_symbols::export(
        name = "general.dev.pci.PciDevice.release_configured_msix",
        contract = "kernel.general.pci-route@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn release_configured_msix(&self, set: PciMsixSet) {
        if !self.release_configured_msix_inner(set) {
            log::error!(
                "[pci] failed to revoke MSI-X doorbell mapping for {}",
                self.pnp.id
            );
        }
    }

    fn release_configured_msix_inner(&self, set: PciMsixSet) -> bool {
        let Ok(control) = self.try_read_config_u16(set.cap_offset + PCI_MSIX_CONTROL_OFFSET) else {
            // 无法确认设备停止写 doorbell；保留全部映射和 vector 比释放后被 DMA
            // 命中更安全。PnP commit 会把该失败视为 tainted。
            core::mem::forget(set);
            return false;
        };
        if self
            .try_write_config_u16(
                set.cap_offset + PCI_MSIX_CONTROL_OFFSET,
                (control | PCI_MSIX_CONTROL_FUNCTION_MASK) & !PCI_MSIX_CONTROL_ENABLE,
            )
            .is_err()
        {
            core::mem::forget(set);
            return false;
        }
        for vector in set.vectors.iter().copied() {
            set_msix_entry_mask(set.table_vaddr, vector.table_index, true);
        }
        // 先关闭并屏蔽设备侧 MSI-X，再撤销 IOMMU doorbell，防止 unmap 后仍有写入。
        let PciMsixSet {
            vectors, doorbells, ..
        } = set;
        let mut doorbells = doorbells.into_vec();
        while let Some(mapping) = doorbells.pop() {
            if let Err(mapping) = mapping.unmap() {
                // 当前 PnpResource::release 契约不能把资源对象交回；显式泄漏仍存活
                // 的 mapping/vector，确保不会丢 token 或复用仍可能被写的 vector。
                core::mem::forget(mapping);
                for mapping in doorbells {
                    core::mem::forget(mapping);
                }
                core::mem::forget(vectors);
                return false;
            }
        }
        let mut released = true;
        for vector in vectors.iter().copied() {
            released &= msi::free_msi(vector.allocation).is_ok();
        }
        released
    }
}

fn program_msix_entry(entry: usize, message: msi::MsiMessage, masked: bool) {
    // SAFETY: entry 已由 MSI-X capability、BAR 大小和 table index 共同校验。
    unsafe {
        write_volatile(
            (entry + PCI_MSIX_ENTRY_VECTOR_CONTROL) as *mut u32,
            PCI_MSIX_ENTRY_MASKED,
        );
        write_volatile(
            (entry + PCI_MSIX_ENTRY_ADDR_LO) as *mut u32,
            message.address as u32,
        );
        write_volatile(
            (entry + PCI_MSIX_ENTRY_ADDR_HI) as *mut u32,
            (message.address >> 32) as u32,
        );
        write_volatile((entry + PCI_MSIX_ENTRY_DATA) as *mut u32, message.data);
        write_volatile(
            (entry + PCI_MSIX_ENTRY_VECTOR_CONTROL) as *mut u32,
            if masked { PCI_MSIX_ENTRY_MASKED } else { 0 },
        );
    }
}

fn set_msix_entry_mask(table_vaddr: usize, table_index: u16, masked: bool) {
    let entry = table_vaddr + usize::from(table_index) * PCI_MSIX_ENTRY_SIZE;
    // SAFETY: table_vaddr 和 index 来自已配置的 PciMsixSet。
    unsafe {
        let control = (entry + PCI_MSIX_ENTRY_VECTOR_CONTROL) as *mut u32;
        let current = read_volatile(control);
        write_volatile(
            control,
            if masked {
                current | PCI_MSIX_ENTRY_MASKED
            } else {
                current & !PCI_MSIX_ENTRY_MASKED
            },
        );
    }
}

impl<'a> PciCapabilityIter<'a> {
    fn new(device: &'a PciDevice) -> Self {
        Self {
            device,
            next_offset: device.capabilities_offset(),
            remaining: PCI_CAPABILITY_MAX_STEPS,
            visited: 0,
        }
    }
}

impl<'a> Iterator for PciCapabilityIter<'a> {
    type Item = PciCapability;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next_offset?;
        if self.remaining == 0 {
            self.next_offset = None;
            return None;
        }
        self.remaining -= 1;

        let visited_bit = match capability_visited_bit(offset) {
            Some(bit) => bit,
            None => {
                self.next_offset = None;
                return None;
            }
        };
        if self.visited & visited_bit != 0 {
            self.next_offset = None;
            return None;
        }
        self.visited |= visited_bit;

        let id = match self.device.try_read_config_u8(offset) {
            Ok(id) => id,
            Err(_) => {
                self.next_offset = None;
                return None;
            }
        };
        let next_offset = match self.device.try_read_config_u8(offset + 1) {
            Ok(raw) => valid_capability_pointer(raw),
            Err(_) => None,
        };

        self.next_offset = next_offset;

        Some(PciCapability {
            offset,
            id,
            next_offset,
        })
    }
}

// ── 动态设备管理 ────────────────────────────────────────────────────────

fn pci_hardware_name(segment: u16, bus: u8, device: u8, function: u8) -> Box<str> {
    alloc::format!("pci-{segment:04x}:{bus:02x}:{device:02x}.{function}").into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciProbeStatus {
    Bound,
    NoDriver,
    Deferred,
}

#[derive(Clone)]
pub struct PciRegistration {
    pub device: Arc<PnpDevice>,
    pub status: PciProbeStatus,
}

impl PciRegistration {
    const fn new(device: Arc<PnpDevice>, status: PciProbeStatus) -> Self {
        Self { device, status }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciRegisterError {
    NotPresent,
    Pnp(PnpError),
}

fn rollback_pci_registration(dev: &Arc<PnpDevice>, inserted: bool) -> Result<(), PnpError> {
    if !inserted {
        return Ok(());
    }
    // IOMMU consumer lease 等 bus resource 必须走完整 PnP remove 事务；直接从
    // registry/父节点摘除只会 Drop 包装对象，无法 detach 已建立的 domain。
    dev.try_remove_device()
}

impl PciDevice {
    /// 从 config space 读取 PCI 信息，构造 PnpDevice 并注册到全局列表，
    /// 然后自动 probe 驱动。一步完成设备的完整发现-绑定流程。
    ///
    /// PnP 设备名是稳定硬件名 `pci-{seg:04x}:{bus:02x}:{dev:02x}.{func}`，
    /// 不承载 `/dev` 节点命名前缀语义。
    pub fn register_and_probe(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    ) -> Result<PciRegistration, PciRegisterError> {
        let id = PnpId::Pci {
            segment,
            bus,
            device,
            function,
        };

        let info = PciDevice::read_device_info(segment, bus, device, function)
            .ok_or(PciRegisterError::NotPresent)?;

        let name = pci_hardware_name(segment, bus, device, function);
        let new_dev = PnpDevice::new(id, name, Box::new(info)).map_err(PciRegisterError::Pnp)?;
        let registration = PNP_DEVICES
            .get_or_insert(Arc::clone(&new_dev))
            .map_err(PciRegisterError::Pnp)?;
        let pnp = registration.device;
        attach_to_host_bridge(&pnp);
        if registration.inserted
            && let Some(context) = host_bridge_dma_context_for_bdf(segment, bus, device, function)
            && context.has_iommu_consumer()
        {
            let Some(resource) = context.claim_iommu_pnp_resource("pci-iommu-consumer") else {
                rollback_pci_registration(&pnp, true).map_err(PciRegisterError::Pnp)?;
                return Err(PciRegisterError::Pnp(PnpError::InvalidState));
            };
            if let Err(error) = pnp.own_bus_resource(resource) {
                rollback_pci_registration(&pnp, true).map_err(PciRegisterError::Pnp)?;
                return Err(PciRegisterError::Pnp(error));
            }
        }

        match pnp.state() {
            PnpState::Bound => Ok(PciRegistration::new(pnp, PciProbeStatus::Bound)),
            PnpState::Discovered => match PNP_DRIVERS.probe_device(&pnp) {
                Ok(()) => Ok(PciRegistration::new(pnp, PciProbeStatus::Bound)),
                Err(PnpError::NoDriver) => Ok(PciRegistration::new(pnp, PciProbeStatus::NoDriver)),
                Err(err) if err.is_deferred() => {
                    Ok(PciRegistration::new(pnp, PciProbeStatus::Deferred))
                }
                Err(err) => {
                    rollback_pci_registration(&pnp, registration.inserted)
                        .map_err(PciRegisterError::Pnp)?;
                    Err(PciRegisterError::Pnp(err))
                }
            },
            PnpState::Probing | PnpState::Removing | PnpState::Gone => {
                rollback_pci_registration(&pnp, registration.inserted)
                    .map_err(PciRegisterError::Pnp)?;
                Err(PciRegisterError::Pnp(PnpError::InvalidState))
            }
        }
    }

    /// 从 config space 读取完整的 [`PciInfo`]。
    ///
    /// 需要 `PCI_CONFIG` 已设置。返回 `None` 表示设备不存在
    /// （vendor == 0xFFFF）或无法访问 config space。
    pub fn read_device_info(segment: u16, bus: u8, device: u8, function: u8) -> Option<PciInfo> {
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref()?;

        let vendor = (cfg.read_u16)(segment, bus, device, function, 0x00).ok()?;
        if vendor == PCI_INVALID_VENDOR_ID {
            return None;
        }

        let device_id = (cfg.read_u16)(segment, bus, device, function, 0x02).ok()?;
        let class_raw = (cfg.read_u32)(segment, bus, device, function, 0x08).ok()?;
        let revision = (cfg.read_u8)(segment, bus, device, function, 0x08).ok()?;
        let header_type = (cfg.read_u8)(segment, bus, device, function, 0x0E).ok()?;
        let subsystem_vendor = (cfg.read_u16)(segment, bus, device, function, 0x2C).ok()?;
        let subsystem_id = (cfg.read_u16)(segment, bus, device, function, 0x2E).ok()?;

        let class = (class_raw >> 8) & 0x00FF_FFFF;
        let subclass = (class_raw >> 16) as u8;
        let prog_if = (class_raw >> 8) as u8;
        let multi_function = header_type & PCI_HEADER_TYPE_MULTI_FUNCTION != 0;

        Some(PciInfo {
            vendor,
            device_id,
            revision,
            class,
            subclass,
            prog_if,
            subsystem_vendor,
            subsystem_id,
            header_type: header_type & 0x7F,
            multi_function,
        })
    }

    /// 创建一个不注册到全局 PnP 表的 PCI 访问句柄。
    ///
    /// 启动早期可能需要在正式 probe 前先整理 BAR 等资源。该句柄仍通过
    /// [`PciInfo`] 保存真实 config-space 信息，但不会出现在设备树、驱动匹配或
    /// devtmpfs 中；完成资源整理后应继续走 [`register_and_probe`](Self::register_and_probe)。
    pub fn new_unregistered(segment: u16, bus: u8, device: u8, function: u8) -> Option<Self> {
        let id = PnpId::Pci {
            segment,
            bus,
            device,
            function,
        };
        let info = Self::read_device_info(segment, bus, device, function)?;
        let pnp = PnpDevice::new(
            id,
            pci_hardware_name(segment, bus, device, function),
            Box::new(info),
        )
        .ok()?;
        Some(Self { pnp })
    }

    /// 触发设备热拔移除。
    ///
    /// 等效于 `self.pnp().remove_device()`。
    pub fn remove_from_bus(&self) {
        self.pnp.remove_device();
    }
}

#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pci.PciDevice.register_and_probe",
    contract = "kernel.general.pci-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn direct_pci_register_and_probe(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
) -> Result<PciRegistration, PciRegisterError> {
    PciDevice::register_and_probe(segment, bus, device, function)
}

#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pci.PciDevice.read_device_info",
    contract = "kernel.general.pci-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn direct_pci_read_device_info(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
) -> Option<PciInfo> {
    PciDevice::read_device_info(segment, bus, device, function)
}

#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pci.PciDevice.new_unregistered",
    contract = "kernel.general.pci-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn direct_pci_new_unregistered(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
) -> Option<PciDevice> {
    PciDevice::new_unregistered(segment, bus, device, function)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    static TEST_PCI_ACCESS_STATE: Spinlock<()> = Spinlock::new(());

    fn test_config_read_u8(
        _segment: u16,
        _bus: u8,
        _device: u8,
        _function: u8,
        _offset: u16,
    ) -> Result<u8, PciConfigError> {
        Ok(0x5a)
    }

    fn test_config_read_u16(
        _segment: u16,
        _bus: u8,
        _device: u8,
        _function: u8,
        _offset: u16,
    ) -> Result<u16, PciConfigError> {
        Ok(0x5aa5)
    }

    fn test_config_read_u32(
        _segment: u16,
        _bus: u8,
        _device: u8,
        _function: u8,
        _offset: u16,
    ) -> Result<u32, PciConfigError> {
        Ok(0x5aa5_a55a)
    }

    fn test_config_write<T>(
        _segment: u16,
        _bus: u8,
        _device: u8,
        _function: u8,
        _offset: u16,
        _value: T,
    ) -> Result<(), PciConfigError> {
        Ok(())
    }

    fn test_mmio_to_virt(phys: usize) -> usize {
        phys.wrapping_add(0x1000)
    }

    fn test_bar_mapper(
        _segment: u16,
        _bus: u8,
        _device: u8,
        _function: u8,
        _bar_type: PciBarType,
        _prefetchable: bool,
        pci_addr: u64,
        _size: u64,
    ) -> Option<PciBarMapping> {
        Some(PciBarMapping {
            cpu_phys: pci_addr as usize,
            virt_addr: pci_addr as usize + 0x1000,
        })
    }

    fn test_irq_resolver_requires_unlocked_config(
        _segment: u16,
        _bus: u8,
        _device: u8,
        _function: u8,
        _interrupt_pin: Option<u8>,
        _interrupt_line: Option<u8>,
    ) -> Option<IrqLine> {
        PCI_CONFIG.try_lock().map(|_guard| IrqLine::Hardware(17))
    }

    #[test]
    fn config_space_kinds_publish_standard_geometry() {
        assert_eq!(PciConfigSpaceKind::Cam.bytes_per_bus(), 1 << 16);
        assert_eq!(PciConfigSpaceKind::Cam.bytes_per_function(), 0x100);
        assert_eq!(PciConfigSpaceKind::Cam.bus_shift(), 16);
        assert_eq!(PciConfigSpaceKind::Cam.function_shift(), 8);

        assert_eq!(PciConfigSpaceKind::Ecam.bytes_per_bus(), 1 << 20);
        assert_eq!(
            PciConfigSpaceKind::Ecam.bytes_per_function(),
            PCI_EXTENDED_CONFIG_SPACE_SIZE
        );
        assert_eq!(PciConfigSpaceKind::Ecam.bus_shift(), 20);
        assert_eq!(PciConfigSpaceKind::Ecam.function_shift(), 12);
    }

    #[test]
    fn bar_size_decode_respects_register_width_and_rejects_invalid_masks() {
        assert_eq!(pci_bar_size_from_mask(0xffff_f000, false), Some(0x1000));
        assert_eq!(
            pci_bar_size_from_mask(0xffff_ffff_ffff_c000, true),
            Some(0x4000)
        );

        assert_eq!(pci_bar_size_from_mask(0, false), None);
        assert_eq!(pci_bar_size_from_mask(0xffff_d000, false), None);
        assert_eq!(pci_bar_size_from_mask(0x1_ffff_f000, false), None);
    }

    #[test]
    fn config_and_bar_backends_are_invoked_with_pair_locks_held() {
        let _test_state = TEST_PCI_ACCESS_STATE.lock();
        let access = PciConfigAccess {
            read_u8: test_config_read_u8,
            read_u16: test_config_read_u16,
            read_u32: test_config_read_u32,
            write_u8: test_config_write::<u8>,
            write_u16: test_config_write::<u16>,
            write_u32: test_config_write::<u32>,
            device_mmio_to_virt: test_mmio_to_virt,
            resolve_irq: None,
            allocate_msi: None,
        };
        let previous = replace_pci_access_pair(Some(access), Some(test_bar_mapper));
        let observed_pair = with_pci_access_pair(|current, mapper| {
            let pair_locked =
                PCI_CONFIG.try_lock().is_none() && PCI_BAR_MAPPER.try_lock().is_none();
            let config_invoked = (current.read_u32)(0, 0, 0, 0, 0) == Ok(0x5aa5_a55a);
            let mapper_invoked = mapper
                .and_then(|mapper| mapper(0, 0, 0, 0, PciBarType::Memory, false, 0x2000, 0x1000))
                == Some(PciBarMapping {
                    cpu_phys: 0x2000,
                    virt_addr: 0x3000,
                });
            pair_locked && config_invoked && mapper_invoked
        })
        .unwrap_or(false);
        let _ = replace_pci_access_pair(previous.0, previous.1);

        assert!(observed_pair);
    }

    #[test]
    fn device_irq_resolver_is_invoked_without_the_config_lock() {
        let _test_state = TEST_PCI_ACCESS_STATE.lock();
        let access = PciConfigAccess {
            read_u8: test_config_read_u8,
            read_u16: test_config_read_u16,
            read_u32: test_config_read_u32,
            write_u8: test_config_write::<u8>,
            write_u16: test_config_write::<u16>,
            write_u32: test_config_write::<u32>,
            device_mmio_to_virt: test_mmio_to_virt,
            resolve_irq: Some(test_irq_resolver_requires_unlocked_config),
            allocate_msi: None,
        };
        let previous = replace_pci_access_pair(Some(access), None);
        let routed_via_free_function = resolve_irq(0, 0, 1, 0, Some(1), None);
        let device = PciDevice::new_unregistered(0, 0, 1, 0).unwrap();
        let routed = device.routed_irq_line();
        let _ = replace_pci_access_pair(previous.0, previous.1);

        assert_eq!(routed_via_free_function, Some(IrqLine::Hardware(17)));
        assert_eq!(routed, Some(IrqLine::Hardware(17)));
    }

    #[test]
    fn host_table_rejects_overlap_without_replacing_published_entry() {
        let mut table = PciHostTable::new();
        let published = PciHostBusRange::new(0, 0x20, 0x2f).unwrap();
        let overlapping = PciHostBusRange::new(0, 0x28, 0x3f).unwrap();
        table.insert(published, 11).unwrap();

        assert_eq!(table.insert(published, 33), Err(PciHostTableError::Overlap));
        assert_eq!(
            table.insert(overlapping, 22),
            Err(PciHostTableError::Overlap)
        );
        assert_eq!(table.get(0, 0x20), Some(&11));
        assert_eq!(table.get(0, 0x2f), Some(&11));
        assert_eq!(table.get(0, 0x30), None);
    }

    #[test]
    fn host_table_rollback_removes_only_the_exact_transaction_key() {
        let mut table = PciHostTable::new();
        let first = PciHostBusRange::new(0, 0, 0x0f).unwrap();
        let second = PciHostBusRange::new(0, 0x10, 0x1f).unwrap();
        let other_segment = PciHostBusRange::new(1, 0, 0xff).unwrap();
        table.insert(first, 1).unwrap();
        table.insert(second, 2).unwrap();
        table.insert(other_segment, 3).unwrap();

        assert_eq!(table.remove_exact(second), Some(2));
        assert_eq!(table.get(0, 1), Some(&1));
        assert_eq!(table.get(0, 0x10), None);
        assert_eq!(table.get(1, 0x10), Some(&3));
        assert_eq!(table.remove_exact(second), None);
    }

    #[test]
    fn u64_bar_high_write_failure_restores_both_dwords() {
        let original_low = 0x1234_0004;
        let original_high = 0x0000_0001;
        let new_low = 0x9000_0004;
        let new_high = 0x0000_0002;
        let mut low = original_low;
        let mut high = original_high;
        let mut writes = Vec::new();

        let result = write_pci_bar_u64_transactional(
            original_low,
            original_high,
            new_low,
            new_high,
            |is_high, value| {
                writes.push((is_high, value));
                if is_high && value == new_high {
                    return Err("injected high dword failure");
                }
                if is_high {
                    high = value;
                } else {
                    low = value;
                }
                Ok(())
            },
        );

        assert_eq!(result, Err("injected high dword failure"));
        assert_eq!((low, high), (original_low, original_high));
        assert_eq!(
            writes,
            vec![
                (false, new_low),
                (true, new_high),
                (false, original_low),
                (true, original_high),
            ]
        );
    }

    #[test]
    fn requester_id_map_applies_mask_range_and_output_offset() {
        let map = PciRequesterIdMap {
            mask: 0xffff,
            entries: vec![PciRequesterIdMapEntry {
                input_base: 0x100,
                provider_path: "/iommu".into(),
                provider_phandle: 7,
                output_base: vec![0x40].into_boxed_slice(),
                length: 0x20,
            }],
        };
        assert_eq!(
            map.map_id(0xabcd_0110),
            Ok(Some(PciMappedRequesterId {
                provider_path: "/iommu",
                provider_phandle: 7,
                args: vec![0x50],
            }))
        );
        assert_eq!(map.map_id(0x120), Ok(None));

        let zero_cell = PciRequesterIdMap {
            mask: u32::MAX,
            entries: vec![PciRequesterIdMapEntry {
                input_base: 0x200,
                provider_path: "/zero-iommu".into(),
                provider_phandle: 8,
                output_base: Box::new([]),
                length: 2,
            }],
        };
        assert_eq!(
            zero_cell.map_id(0x201).unwrap().unwrap().args,
            Vec::<u32>::new()
        );

        let multi_cell = PciRequesterIdMap {
            mask: u32::MAX,
            entries: vec![PciRequesterIdMapEntry {
                input_base: 0x300,
                provider_path: "/wide-iommu".into(),
                provider_phandle: 9,
                output_base: vec![1, 2].into_boxed_slice(),
                length: 2,
            }],
        };
        assert!(multi_cell.match_id(0x301).is_some());
        assert!(matches!(
            multi_cell.map_id(0x301),
            Err(PciRequesterIdMapError::AmbiguousMultiCellRange { .. })
        ));
    }

    #[test]
    fn function_firmware_lookup_preserves_exact_bdf_and_typed_properties() {
        let functions = vec![
            PciFunctionFirmwareInfo {
                firmware_path: "/soc/pci@30000000/iommu@8".into(),
                bus: 0,
                device: 1,
                function: 0,
                phandle: Some(0x8000),
                compatible: vec!["riscv,pci-iommu".into()],
                properties: vec![FirmwareProperty::new(
                    "#iommu-cells".into(),
                    1u32.to_be_bytes().to_vec().into_boxed_slice(),
                )],
            },
            PciFunctionFirmwareInfo {
                firmware_path: "/soc/pci@30000000/virtio@10".into(),
                bus: 0,
                device: 2,
                function: 0,
                phandle: None,
                compatible: vec!["virtio,pci".into()],
                properties: Vec::new(),
            },
        ];

        let iommu = find_function_firmware_info(&functions, 0, 1, 0).unwrap();
        assert_eq!(iommu.phandle, Some(0x8000));
        assert!(iommu.has_compatible("riscv,pci-iommu"));
        assert!(!iommu.has_compatible("riscv,iommu"));
        assert_eq!(iommu.u32_property("#iommu-cells"), Some(1));
        assert!(find_function_firmware_info(&functions, 0, 1, 1).is_none());
        assert!(find_function_firmware_info(&functions, 1, 1, 0).is_none());
    }
}

#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pci.PciDevice.remove_from_bus",
    contract = "kernel.general.pci-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn direct_pci_remove_from_bus(device: &PciDevice) {
    device.remove_from_bus();
}

// ── PCI Bus 扫描器 ──────────────────────────────────────────────────────

/// 对指定 segment 内的 bus 范围进行 PCI 设备扫描。
///
/// 对每个存在且有效的 PCI function 调用 `on_device` 回调。
/// 回调返回 `true` 继续扫描下一设备，`false` 提前终止。
///
/// # 参数
/// - `segment`: PCI segment group（通常为 0）
/// - `start_bus`: 起始 bus 号（含）
/// - `end_bus`: 结束 bus 号（含）
/// - `on_device`: 设备发现回调
///
/// # 返回
/// 扫描到的设备数量。
#[kernel_symbols::export(
    name = "general.dev.pci.pci_scan_bus_range",
    contract = "kernel.general.pci-scan@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn pci_scan_bus_range(
    segment: u16,
    start_bus: u8,
    end_bus: u8,
    on_device: &mut dyn FnMut(u16, u8, u8, u8) -> bool,
) -> usize {
    let mut count = 0usize;

    for bus in start_bus..=end_bus {
        for device in 0u8..PCI_DEVICES_PER_BUS {
            let vendor = match config_read_u16(segment, bus, device, 0, 0x00) {
                Ok(vendor) => vendor,
                Err(_) => continue,
            };
            if vendor == PCI_INVALID_VENDOR_ID {
                continue;
            }

            if !on_device(segment, bus, device, 0) {
                return count + 1;
            }
            count += 1;

            let header_type = match config_read_u8(segment, bus, device, 0, 0x0E) {
                Ok(header_type) => header_type,
                Err(_) => continue,
            };
            if header_type & PCI_HEADER_TYPE_MULTI_FUNCTION == 0 {
                continue;
            }

            for function in 1u8..PCI_FUNCTIONS_PER_DEVICE {
                let vendor = match config_read_u16(segment, bus, device, function, 0x00) {
                    Ok(vendor) => vendor,
                    Err(_) => continue,
                };
                if vendor == PCI_INVALID_VENDOR_ID {
                    continue;
                }
                if !on_device(segment, bus, device, function) {
                    return count + 1;
                }
                count += 1;
            }
        }
    }

    count
}

/// 扫描 PCI bus 并以 PnP 之名注册所有发现的设备。
///
/// 这是 [`pci_scan_bus_range`] 的便捷封装：自动调用
/// [`PciDevice::register_and_probe`] 完成发现→注册→probe 流程。
///
/// 返回成功注册的设备数量。
#[kernel_symbols::export(
    name = "general.dev.pci.pci_scan_and_register",
    contract = "kernel.general.pci-scan@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn pci_scan_and_register(segment: u16, start_bus: u8, end_bus: u8) -> usize {
    let mut count = 0usize;
    pci_scan_bus_range(segment, start_bus, end_bus, &mut |seg, bus, dev, func| {
        match PciDevice::register_and_probe(seg, bus, dev, func) {
            Ok(_) => {
                count += 1;
            }
            Err(PciRegisterError::NotPresent) => {}
            Err(err) => {
                log::debug!(
                    "[pci] failed to register {seg:04x}:{bus:02x}:{dev:02x}.{func}: {:?}",
                    err
                );
            }
        }
        true
    });
    count
}

/// 扫描 PCI bus 并统计注册/probe 的结构化结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciScanRegisterSummary {
    pub registered: usize,
    pub bound: usize,
    pub no_driver: usize,
    pub deferred: usize,
    pub failed: usize,
}

#[kernel_symbols::export(
    name = "general.dev.pci.pci_scan_and_register_summary",
    contract = "kernel.general.pci-scan@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn pci_scan_and_register_summary(
    segment: u16,
    start_bus: u8,
    end_bus: u8,
) -> PciScanRegisterSummary {
    let mut summary = PciScanRegisterSummary::default();
    pci_scan_bus_range(segment, start_bus, end_bus, &mut |seg, bus, dev, func| {
        match PciDevice::register_and_probe(seg, bus, dev, func) {
            Ok(registration) => {
                summary.registered += 1;
                match registration.status {
                    PciProbeStatus::Bound => summary.bound += 1,
                    PciProbeStatus::NoDriver => summary.no_driver += 1,
                    PciProbeStatus::Deferred => summary.deferred += 1,
                }
            }
            Err(PciRegisterError::NotPresent) => {}
            Err(err) => {
                summary.failed += 1;
                log::debug!(
                    "[pci] failed to register {seg:04x}:{bus:02x}:{dev:02x}.{func}: {:?}",
                    err
                );
            }
        }
        true
    });
    summary
}

/// 扫描总线上的设备并只返回 BDF + vendor/device 的原始信息，
/// 不执行 PnP 注册。
///
/// 用于固件早期阶段仅需检查是否有特定设备存在的场景。
#[derive(Clone, Copy, Debug)]
pub struct PciRawDevice {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
    pub class: u32,
    pub header_type: u8,
    pub multi_function: bool,
}

impl PciRawDevice {
    /// 原始扫描结果对应的 BAR 槽数量。
    ///
    /// 该方法只依据 header type 判断资源寄存器范围，供早期资源分配阶段在
    /// 尚未注册 PnP 设备时使用。
    pub const fn bar_count(&self) -> usize {
        match self.header_type {
            PCI_HEADER_TYPE_ENDPOINT => PCI_ENDPOINT_BAR_COUNT,
            PCI_HEADER_TYPE_BRIDGE => PCI_BRIDGE_BAR_COUNT,
            _ => 0,
        }
    }
}

#[kernel_symbols::export(
    name = "general.dev.pci.pci_scan_raw",
    contract = "kernel.general.pci-scan@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn pci_scan_raw(segment: u16, start_bus: u8, end_bus: u8) -> alloc::vec::Vec<PciRawDevice> {
    let mut devices = alloc::vec::Vec::new();

    for bus in start_bus..=end_bus {
        for device in 0u8..PCI_DEVICES_PER_BUS {
            let vendor = match config_read_u16(segment, bus, device, 0, 0x00) {
                Ok(vendor) => vendor,
                Err(_) => continue,
            };
            if vendor == PCI_INVALID_VENDOR_ID {
                continue;
            }

            let device_id = match config_read_u16(segment, bus, device, 0, 0x02) {
                Ok(device_id) => device_id,
                Err(_) => continue,
            };
            let class_raw = match config_read_u32(segment, bus, device, 0, 0x08) {
                Ok(class_raw) => class_raw,
                Err(_) => continue,
            };
            let header_type_raw = match config_read_u8(segment, bus, device, 0, 0x0E) {
                Ok(header_type_raw) => header_type_raw,
                Err(_) => continue,
            };

            devices.push(PciRawDevice {
                segment,
                bus,
                device,
                function: 0,
                vendor,
                device_id,
                class: (class_raw >> 8) & 0x00FF_FFFF,
                header_type: header_type_raw & 0x7F,
                multi_function: header_type_raw & PCI_HEADER_TYPE_MULTI_FUNCTION != 0,
            });

            if header_type_raw & PCI_HEADER_TYPE_MULTI_FUNCTION == 0 {
                continue;
            }

            for function in 1u8..PCI_FUNCTIONS_PER_DEVICE {
                let vendor = match config_read_u16(segment, bus, device, function, 0x00) {
                    Ok(vendor) => vendor,
                    Err(_) => continue,
                };
                if vendor == PCI_INVALID_VENDOR_ID {
                    continue;
                }

                let device_id = match config_read_u16(segment, bus, device, function, 0x02) {
                    Ok(device_id) => device_id,
                    Err(_) => continue,
                };
                let class_raw = match config_read_u32(segment, bus, device, function, 0x08) {
                    Ok(class_raw) => class_raw,
                    Err(_) => continue,
                };
                let header_type = match config_read_u8(segment, bus, device, function, 0x0E) {
                    Ok(header_type) => header_type,
                    Err(_) => continue,
                };

                devices.push(PciRawDevice {
                    segment,
                    bus,
                    device,
                    function,
                    vendor,
                    device_id,
                    class: (class_raw >> 8) & 0x00FF_FFFF,
                    header_type: header_type & 0x7F,
                    multi_function: false,
                });
            }
        }
    }

    devices
}

// ── PciBar helpers ───────────────────────────────────────────────────────

impl PciBar {
    pub fn is_memory(&self) -> bool {
        matches!(self.bar_type, PciBarType::Memory)
    }

    pub fn is_io(&self) -> bool {
        matches!(self.bar_type, PciBarType::Io)
    }
}
