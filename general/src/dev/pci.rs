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

use vfs::sync::Spinlock;

use super::dma::{DmaBouncePolicy, DmaConstraints, DmaContext};
use super::irq::IrqLine;
use super::msi;
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

#[derive(Clone, Copy, Debug)]
pub enum PciBarType {
    Memory,
    Io,
}

#[derive(Clone, Copy, Debug)]
pub struct PciBar {
    pub idx: usize,
    pub bar_type: PciBarType,
    pub prefetchable: bool,
    pub phys_addr: u64,
    pub size: u64,
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

/// PCI host bridge 的标准化描述。
///
/// 该结构是设备层认识 host bridge 的统一入口：ECAM 范围、bus-range、地址窗口、
/// DMA 一致性以及固件路由规模都会被保存下来，供 sysfs/诊断/后续热插拔或 DMA
/// 策略查询。具体的配置空间读写回调仍由 [`PciConfigAccess`] 单独安装。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciHostBridgeInfo {
    pub name: Box<str>,
    pub firmware_path: Option<Box<str>>,
    pub domain: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    pub ecam_phys: usize,
    pub ecam_size: usize,
    pub dma_coherent: bool,
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

struct PciHostBridgeRegistration {
    handle: PciHostBridgeHandle,
    info: PciHostBridgeInfo,
    pnp: Option<Arc<PnpDevice>>,
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
    if info.bus_start > info.bus_end || info.ecam_size == 0 {
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
    registry
        .bridges
        .try_reserve(1)
        .map_err(|_| PciHostBridgeError::OutOfMemory)?;
    let id = registry_id::alloc_locked_id(&mut registry.next_id)
        .map_err(|_| PciHostBridgeError::OutOfMemory)?;
    // host bridge 句柄可能被启动期回滚或热移除路径保存；编号只增长不复用，
    // 旧句柄就不会误注销后来重新登记的同一 domain/bus-range。
    let handle = PciHostBridgeHandle { id };
    registry
        .bridges
        .push(PciHostBridgeRegistration { handle, info, pnp });
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
    registry.bridges.swap_remove(index);
    drop(registry);
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

fn host_bridge_pnp_for(segment: u16, bus: u8) -> Option<Arc<PnpDevice>> {
    PCI_HOST_BRIDGES
        .lock()
        .bridges
        .iter()
        .find(|bridge| pci_host_bridge_covers(bridge, segment, bus))
        .and_then(|bridge| bridge.pnp.as_ref().map(Arc::clone))
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
/// PCI 常规配置空间每条 bus 最多有 32 个 device 编号。
pub const PCI_DEVICES_PER_BUS: u8 = 32;
/// 每个 PCI device 最多有 8 个 function，0 号 function 必须先探测。
pub const PCI_FUNCTIONS_PER_DEVICE: u8 = 8;

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

pub(crate) fn replace_pci_config_access(
    access: Option<PciConfigAccess>,
) -> Option<PciConfigAccess> {
    let mut current = PCI_CONFIG.lock();
    core::mem::replace(&mut *current, access)
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
    let guard = PCI_CONFIG.lock();
    let resolver = guard.as_ref()?.resolve_irq?;
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
    let guard = PCI_CONFIG.lock();
    let allocator = guard.as_ref()?.allocate_msi?;
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

impl PciMsiPnpResource {
    pub const fn new(pci: PciDevice, handle: PciMsiHandle, label: &'static str) -> Self {
        Self { pci, handle, label }
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
        self.pci.release_configured_msi(self.handle);
        Ok(())
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

impl PciDevice {
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

    /// 返回该 PCI function 的 DMA 上下文。
    ///
    /// DMA 能力来自设备所在 host bridge，而不是全局静态假设。当前固件只提供
    /// coherent 属性时，地址窗口仍采用 identity 兼容策略；后续接入 `dma-ranges`
    /// 或 IOMMU 后只需要在这里构造更精确的 constraints/mapper。
    pub fn dma_context(&self) -> DmaContext {
        let Some((segment, bus, _, _)) = self.bdf() else {
            return DmaContext::default_coherent();
        };
        let Some(host) = host_bridge_for_bus(segment, bus) else {
            return DmaContext::default_coherent();
        };
        DmaContext::with_constraints(DmaConstraints {
            address_mask: usize::MAX,
            max_segment_size: usize::MAX,
            max_segments: 1,
            coherent: host.info.dma_coherent,
            supports_scatter_gather: false,
            bounce: DmaBouncePolicy::Disabled,
        })
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
        let bar_count = self.bar_count();
        if idx >= bar_count {
            return None;
        }
        let offset = PCI_BAR0_OFFSET + (idx as u16) * PCI_BAR_STRIDE;
        let bar_val = self.try_read_config_u32(offset).ok()?;

        if bar_val == 0 {
            return None;
        }

        let is_mmio = bar_val & PCI_BAR_IO_SPACE == 0;
        let prefetchable = is_mmio && (bar_val & PCI_BAR_MEM_PREFETCHABLE) != 0;

        let (bar_type, phys_addr, size) = if is_mmio {
            let is_64 = match (bar_val >> PCI_BAR_MEM_TYPE_SHIFT) & PCI_BAR_MEM_TYPE_MASK {
                PCI_BAR_MEM_TYPE_32 => false,
                PCI_BAR_MEM_TYPE_64 if idx + 1 < bar_count => true,
                _ => return None,
            };
            let high_offset = offset + PCI_BAR_STRIDE;
            let high_val = if is_64 {
                self.try_read_config_u32(high_offset).ok()?
            } else {
                0
            };
            let phys_addr = ((high_val as u64) << 32) | ((bar_val & PCI_BAR_MEM_ADDR_MASK) as u64);

            let cmd = self.try_read_config_u16(PCI_COMMAND_OFFSET).ok()?;
            self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_MEMORY_SPACE)
                .ok()?;

            let size_bits = (|| -> Option<u64> {
                if is_64 {
                    self.try_write_config_u32(high_offset, u32::MAX).ok()?;
                }
                self.try_write_config_u32(offset, PCI_BAR_MEM_ADDR_MASK)
                    .ok()?;
                let size_lo = self.try_read_config_u32(offset).ok()? & PCI_BAR_MEM_ADDR_MASK;
                let size_hi = if is_64 {
                    self.try_read_config_u32(high_offset).ok()?
                } else {
                    0
                };
                Some(((size_hi as u64) << 32) | size_lo as u64)
            })();

            let _ = self.try_write_config_u32(offset, bar_val);
            if is_64 {
                let _ = self.try_write_config_u32(high_offset, high_val);
            }
            let _ = self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd);

            let size_bits = size_bits?;
            if size_bits == 0 {
                return None;
            }
            (PciBarType::Memory, phys_addr, (!size_bits).wrapping_add(1))
        } else {
            let phys_addr = (bar_val & PCI_BAR_IO_ADDR_MASK) as u64;
            let cmd = self.try_read_config_u16(PCI_COMMAND_OFFSET).ok()?;
            self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_IO_SPACE)
                .ok()?;

            let size_bits = (|| -> Option<u32> {
                self.try_write_config_u32(offset, PCI_BAR_IO_ADDR_MASK)
                    .ok()?;
                Some(self.try_read_config_u32(offset).ok()? & PCI_BAR_IO_ADDR_MASK)
            })();

            let _ = self.try_write_config_u32(offset, bar_val);
            let _ = self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd);

            let size_bits = size_bits?;
            if size_bits == 0 {
                return None;
            }
            (
                PciBarType::Io,
                phys_addr,
                (!size_bits).wrapping_add(1) as u64,
            )
        };

        Some(PciBar {
            idx,
            bar_type,
            prefetchable,
            phys_addr,
            size,
        })
    }

    pub fn map_bar_virt(&self, idx: usize) -> Option<(PciBar, usize)> {
        let bar = self.map_bar(idx)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref()?;
        let vaddr = match bar.bar_type {
            PciBarType::Memory => (cfg.device_mmio_to_virt)(bar.phys_addr as usize),
            PciBarType::Io => bar.phys_addr as usize,
        };
        Some((bar, vaddr))
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

    pub fn try_enable_bus_master(&self) -> Result<(), PciConfigError> {
        self.try_update_command(PCI_COMMAND_BUS_MASTER, 0)
    }

    pub fn enable_bus_master(&self) {
        let _ = self.try_enable_bus_master();
    }

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

    pub fn try_enable_mmio(&self) -> Result<(), PciConfigError> {
        self.try_update_command(PCI_COMMAND_MEMORY_SPACE, 0)
    }

    pub fn enable_mmio(&self) {
        let _ = self.try_enable_mmio();
    }

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

    pub fn disable_interrupts(&self) {
        let _ = self.try_disable_interrupts();
    }

    pub fn try_enable_interrupts(&self) -> Result<(), PciConfigError> {
        self.try_update_command(0, PCI_COMMAND_INTERRUPT_DISABLE)
    }

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
    pub fn routed_irq_line(&self) -> Option<IrqLine> {
        let (segment, bus, device, function) = self.bdf()?;
        let pin = self.irq_pin();
        let line = self.irq_line();
        let guard = PCI_CONFIG.lock();
        let resolver = guard.as_ref()?.resolve_irq?;
        resolver(segment, bus, device, function, pin, line)
    }

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
        if let Err(err) = self.program_single_msi(cap_offset, allocation.message()) {
            let _ = msi::free_msi(allocation);
            return Err(err);
        }
        Ok(PciMsiHandle {
            cap_offset,
            allocation,
        })
    }

    pub fn configure_single_msi(&self) -> Option<PciMsiHandle> {
        self.try_configure_single_msi().ok()
    }

    pub fn release_configured_msi(&self, handle: PciMsiHandle) {
        let _ = self.try_msi_disable(handle.cap_offset);
        let _ = msi::free_msi(handle.allocation);
    }

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

fn rollback_pci_registration(dev: &Arc<PnpDevice>, inserted: bool) {
    if !inserted {
        return;
    }
    if let Some(parent) = dev.parent() {
        parent.detach_child(dev);
    }
    PNP_DEVICES.remove_exact(dev);
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

        match pnp.state() {
            PnpState::Bound => Ok(PciRegistration::new(pnp, PciProbeStatus::Bound)),
            PnpState::Discovered => match PNP_DRIVERS.probe_device(&pnp) {
                Ok(()) => Ok(PciRegistration::new(pnp, PciProbeStatus::Bound)),
                Err(PnpError::NoDriver) => Ok(PciRegistration::new(pnp, PciProbeStatus::NoDriver)),
                Err(err) if err.is_deferred() => {
                    Ok(PciRegistration::new(pnp, PciProbeStatus::Deferred))
                }
                Err(err) => {
                    rollback_pci_registration(&pnp, registration.inserted);
                    Err(PciRegisterError::Pnp(err))
                }
            },
            PnpState::Probing | PnpState::Removing | PnpState::Gone => {
                rollback_pci_registration(&pnp, registration.inserted);
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
    let guard = PCI_CONFIG.lock();
    let cfg = match guard.as_ref() {
        Some(cfg) => cfg,
        None => return 0,
    };

    let read_u16 = cfg.read_u16;
    let read_u8 = cfg.read_u8;
    drop(guard);

    let mut count = 0usize;

    for bus in start_bus..=end_bus {
        for device in 0u8..PCI_DEVICES_PER_BUS {
            let vendor = match read_u16(segment, bus, device, 0, 0x00) {
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

            let header_type = match read_u8(segment, bus, device, 0, 0x0E) {
                Ok(header_type) => header_type,
                Err(_) => continue,
            };
            if header_type & PCI_HEADER_TYPE_MULTI_FUNCTION == 0 {
                continue;
            }

            for function in 1u8..PCI_FUNCTIONS_PER_DEVICE {
                let vendor = match read_u16(segment, bus, device, function, 0x00) {
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

    let guard = PCI_CONFIG.lock();
    let cfg = match guard.as_ref() {
        Some(cfg) => cfg,
        None => return devices,
    };

    let read_u16 = cfg.read_u16;
    let read_u32 = cfg.read_u32;
    let read_u8 = cfg.read_u8;
    drop(guard);

    for bus in start_bus..=end_bus {
        for device in 0u8..PCI_DEVICES_PER_BUS {
            let vendor = match read_u16(segment, bus, device, 0, 0x00) {
                Ok(vendor) => vendor,
                Err(_) => continue,
            };
            if vendor == PCI_INVALID_VENDOR_ID {
                continue;
            }

            let device_id = match read_u16(segment, bus, device, 0, 0x02) {
                Ok(device_id) => device_id,
                Err(_) => continue,
            };
            let class_raw = match read_u32(segment, bus, device, 0, 0x08) {
                Ok(class_raw) => class_raw,
                Err(_) => continue,
            };
            let header_type_raw = match read_u8(segment, bus, device, 0, 0x0E) {
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
                let vendor = match read_u16(segment, bus, device, function, 0x00) {
                    Ok(vendor) => vendor,
                    Err(_) => continue,
                };
                if vendor == PCI_INVALID_VENDOR_ID {
                    continue;
                }

                let device_id = match read_u16(segment, bus, device, function, 0x02) {
                    Ok(device_id) => device_id,
                    Err(_) => continue,
                };
                let class_raw = match read_u32(segment, bus, device, function, 0x08) {
                    Ok(class_raw) => class_raw,
                    Err(_) => continue,
                };
                let header_type = match read_u8(segment, bus, device, function, 0x0E) {
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
