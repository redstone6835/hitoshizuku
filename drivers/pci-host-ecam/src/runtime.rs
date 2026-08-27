//! PCI ECAM host 的配置空间、固件路由与 BAR 资源运行时。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use general::dev::dma::DmaWindow;
use general::dev::irq;
use general::dev::msi;
use general::dev::pci::{
    PCI_DEVICES_PER_BUS, PCI_EXTENDED_CONFIG_SPACE_SIZE, PCI_FUNCTIONS_PER_DEVICE, PciBarMapping,
    PciBarType, PciConfigAccess, PciConfigError, PciConfigSpaceKind, PciDevice, PciFunctionDmaInfo,
    PciFunctionFirmwareInfo, PciHostAddressSpace, PciHostBridgeError, PciHostBridgeHandle,
    PciHostBridgeInfo, PciHostBridgeWindow, PciHostBusRange, PciHostConfigRegion, PciHostDmaInfo,
    PciHostTable, PciIommuReference, PciProbeStatus, PciRegisterError, PciRequesterIdMap,
    PciRequesterIdMapEntry, PciResolvedIrq, PciScanRegisterSummary, host_bridge_pnp_resource,
    register_host_bridge, try_install_pci_access_pair, unregister_host_bridge,
};
use general::dev::platform::FirmwareProperty;
use general::dev::pnp::{PnpDevice, PnpError, PnpResourceKind};
use general::firmware::dtb::{
    DtbPciAddressSpace, DtbPciConfigSpace, DtbPciInterruptResolver, DtbPciRangeInfo,
    DtbPcieHostInfo, resolve_pci_interrupt as resolve_dtb_pci_interrupt,
};
use vfs::sync::Spinlock;

use crate::bar::{
    PciBarAddressWidth, PciBarAllocation, PciBarKind, PciBarRuntimeWindow, PciBarWindowAllocator,
    PciBarWindowSpace, probed_20bit_memory_bar_size,
};
use crate::ls2k_config::{Ls2kConfigWindow, Ls2kRootIrqRoute, Ls2kRootIrqTable};
use crate::routing::{
    PciIntxBridgeRoute, PciIntxRouting, PciMsiMapRoute, PciMsiParentRoute, PciMsiRoutingMode,
    allocate_first_available,
};
use crate::topology::{
    PciBridgeApertureTracker, PciBridgeIoWindow, PciBridgeMemoryWindow, PciBridgePrefetchWindow,
    PciBusNumberAllocator, PciResourceEnvelope, PciResourceSpace, encode_bridge_io_window,
    encode_bridge_memory_window, encode_bridge_prefetch_window,
};

/// 一段按 PCI segment 与 bus-range 索引的 ECAM 窗口。
#[derive(Clone, Copy)]
struct PciEcamRegion {
    phys_base: usize,
    bus_start: u8,
    bus_end: u8,
    bus_shift: u8,
    function_shift: u8,
    function_size: u16,
    vbase: usize,
    size: usize,
    config_space: DtbPciConfigSpace,
}

/// DT 可以描述多个互不重叠的 host bridge；配置空间回调必须按每次访问携带的
/// segment/bus 选择 ECAM，而不能让后安装的 host 覆盖前一个全局 base。
static PCI_ECAM_REGIONS: Spinlock<PciHostTable<PciEcamRegion>> = Spinlock::new(PciHostTable::new());
static PCI_ACCESS_INIT: Spinlock<Option<usize>> = Spinlock::new(None);

struct PciIrqRouting {
    backend: PciIrqRoutingBackend,
    intx: Spinlock<PciIntxRouting>,
}

enum PciIrqRoutingBackend {
    Dtb {
        address_cells: usize,
        interrupt_cells: usize,
        resolver: DtbPciInterruptResolver,
    },
    Ls2k1000(Ls2kRootIrqTable),
}

static PCI_IRQ_ROUTING: Spinlock<PciHostTable<Arc<PciIrqRouting>>> =
    Spinlock::new(PciHostTable::new());

struct PciMsiRouting {
    mode: PciMsiRoutingMode,
}

static PCI_MSI_ROUTING: Spinlock<PciHostTable<PciMsiRouting>> = Spinlock::new(PciHostTable::new());

struct PciBarRouting {
    windows: Vec<PciBarRuntimeWindow>,
}

static PCI_BAR_ROUTING: Spinlock<PciHostTable<PciBarRouting>> = Spinlock::new(PciHostTable::new());

fn register_pci_host_bridge(
    host: &DtbPcieHostInfo,
    pnp: Option<Arc<PnpDevice>>,
    irq_route_count: usize,
    msi_route_count: usize,
) -> Result<PciHostBridgeHandle, PciHostBridgeError> {
    let info = PciHostBridgeInfo {
        name: host.name.clone(),
        firmware_path: Some(host.path.clone()),
        numa_node_id: host.numa_node_id,
        domain: host.domain,
        bus_start: host.bus_start,
        bus_end: host.bus_end,
        config_regions: vec![PciHostConfigRegion {
            bus_start: host.bus_start,
            bus_end: host.bus_end,
            physical_start: host.ecam_phys,
            size: host.ecam_size,
        }],
        config_space: pci_config_space_kind(host.config_space),
        dma_coherent: host.effective_dma.coherent,
        dma: pci_host_dma_info(host),
        firmware_functions: host
            .children
            .iter()
            .map(|child| PciFunctionFirmwareInfo {
                firmware_path: child.path.clone(),
                bus: child.bus,
                device: child.device,
                function: child.function,
                phandle: child.phandle,
                compatible: child.compatible.clone(),
                properties: child
                    .properties
                    .iter()
                    .map(|property| {
                        FirmwareProperty::new(property.name.clone(), property.value.clone())
                    })
                    .collect(),
            })
            .collect(),
        windows: host.ranges.iter().map(pci_host_window).collect(),
        irq_route_count,
        msi_route_count,
    };
    match register_host_bridge(info, pnp) {
        Ok(handle) => {
            log::printk!(
                "[pci-host-ecam] registered {} handle={} windows={} irq-routes={} msi-routes={}",
                host.path,
                handle.id(),
                host.ranges.len(),
                irq_route_count,
                msi_route_count
            );
            Ok(handle)
        }
        Err(PciHostBridgeError::AlreadyRegistered) => {
            log::printk!(
                "[kernel-start][dtb] PCI host bridge domain {} bus=[{:#x},{:#x}] already registered",
                host.domain,
                host.bus_start,
                host.bus_end
            );
            Err(PciHostBridgeError::AlreadyRegistered)
        }
        Err(err) => {
            log::printk!(
                "[kernel-start][dtb] failed to register PCI host bridge {}: {:?}",
                host.path,
                err
            );
            Err(err)
        }
    }
}

/// 由 ELM platform probe 完整激活一个规范化 DT PCI host。
pub(crate) fn probe_host(
    host: &DtbPcieHostInfo,
    dev: &Arc<PnpDevice>,
    device_mmio_to_virt: fn(usize) -> usize,
) -> Result<PciScanRegisterSummary, PnpError> {
    log::printk!(
        "[pci-host-ecam] probing {} domain={} ecam={:#x}+{:#x} bus=[{:#x},{:#x}] ranges={} msi-map={} msi-parent={} coherent={}",
        host.path,
        host.domain,
        host.ecam_phys,
        host.ecam_size,
        host.bus_start,
        host.bus_end,
        host.ranges.len(),
        host.msi_map.len(),
        host.msi_parents.len(),
        host.dma_coherent as usize
    );

    let runtime =
        begin_host_runtime_transaction(host, device_mmio_to_virt).map_err(|error| match error {
            PciHostRuntimeInstallError::BarWindows => PnpError::malformed(
                PnpResourceKind::PciHostBridge,
                "invalid or overlapping PCI address windows",
            ),
            PciHostRuntimeInstallError::Ecam => PnpError::malformed(
                PnpResourceKind::PciHostBridge,
                "overlapping, undersized or unrepresentable ECAM window",
            ),
        })?;

    let irq_route_count = if install_irq_routing(host.domain, host) {
        usable_irq_route_count(host)
    } else if !host.interrupt_map.is_empty() {
        log::printk!(
            "[pci-host-ecam] rejected IRQ routing for {}: unresolved or malformed route",
            host.path
        );
        0
    } else {
        0
    };
    let msi_route_count = if install_msi_routing(host.domain, host) {
        msi_route_count(host)
    } else if host.msi_map_present || !host.msi_parents.is_empty() {
        log::printk!(
            "[pci-host-ecam] rejected MSI routing for {}: unsupported or invalid specifier",
            host.path
        );
        0
    } else {
        0
    };

    dev.reserve_owned_resources(1)?;
    let handle = register_pci_host_bridge(
        host,
        Some(Arc::clone(dev)),
        irq_route_count,
        msi_route_count,
    )
    .map_err(|error| match error {
        PciHostBridgeError::OutOfMemory => PnpError::OutOfMemory,
        PciHostBridgeError::AlreadyRegistered => PnpError::registration_failed(
            PnpResourceKind::PciHostBridge,
            "PCI domain and bus range overlaps an active host",
        ),
        PciHostBridgeError::Invalid | PciHostBridgeError::NotFound => {
            PnpError::registration_failed(
                PnpResourceKind::PciHostBridge,
                "PCI host registry rejected descriptor",
            )
        }
    })?;
    if let Err(error) = dev.own_resource(host_bridge_pnp_resource(handle, "pci-host-ecam")) {
        let _ = unregister_host_bridge(handle);
        return Err(error);
    }
    runtime.commit();

    let topology = configure_pci_hierarchy(host);
    if irq_route_count != 0 && !publish_intx_topology(host.domain, &topology) {
        log::printk!(
            "[pci-host-ecam] failed to publish bridge INTx topology for {}",
            host.path
        );
    }
    let summary = register_pci_topology(&topology);
    log::printk!(
        "[pci-host-ecam] scan domain={} bus=[{:#x},{:#x}] registered={} bound={} no-driver={} deferred={} failed={}",
        host.domain,
        host.bus_start,
        host.bus_end,
        summary.registered,
        summary.bound,
        summary.no_driver,
        summary.deferred,
        summary.failed
    );
    Ok(summary)
}

/// PnP core 已经先移除所有 endpoint，此时可以精确撤销 host 私有运行时表。
pub(crate) fn remove_host(host: &DtbPcieHostInfo) {
    let Some(key) = PciHostBusRange::new(host.domain, host.bus_start, host.bus_end) else {
        return;
    };
    remove_host_runtime_state(key);
}

/// driver 注销全部 host 后清空模块静态状态。
///
/// ELM owned-resource drain 会随后恢复先前的全局 PCI 回调；这里先释放各路由表
/// 保留的容量并重置安装门闩，使同一 integrated component 再次 initialize 时
/// 能重新发布回调，而不会沿用上一代实例的状态。
pub(crate) fn reset_after_driver_unregister() -> bool {
    if !PCI_ECAM_REGIONS.lock().is_empty()
        || !PCI_IRQ_ROUTING.lock().is_empty()
        || !PCI_MSI_ROUTING.lock().is_empty()
        || !PCI_BAR_ROUTING.lock().is_empty()
    {
        return false;
    }

    *PCI_ECAM_REGIONS.lock() = PciHostTable::new();
    *PCI_IRQ_ROUTING.lock() = PciHostTable::new();
    *PCI_MSI_ROUTING.lock() = PciHostTable::new();
    *PCI_BAR_ROUTING.lock() = PciHostTable::new();
    *PCI_ACCESS_INIT.lock() = None;
    DEVICE_MMIO_TO_VIRT.store(0, Ordering::Release);
    true
}

fn pci_host_dma_info(host: &DtbPcieHostInfo) -> PciHostDmaInfo {
    let windows = host.effective_dma.windows.as_ref().map(|windows| {
        windows
            .iter()
            .map(|window| DmaWindow {
                cpu_start: window.cpu_start,
                dma_start: window.dma_start,
                size: window.size,
            })
            .collect()
    });
    let unsupported = host.effective_dma.unsupported;
    let iommus = iommu_references(&host.iommus);
    let iommu_map = host.iommu_map.as_ref().and_then(|map| {
        let entries = map
            .entries
            .iter()
            .filter(|entry| entry.provider_available)
            .map(|entry| PciRequesterIdMapEntry {
                input_base: entry.input_base,
                provider_path: entry.provider_path.clone(),
                provider_phandle: entry.provider_phandle,
                output_base: entry.output_base.clone(),
                length: entry.length,
            })
            .collect::<Vec<_>>();
        (!entries.is_empty()).then_some(PciRequesterIdMap {
            mask: map.mask,
            entries,
        })
    });
    let functions = host
        .children
        .iter()
        .map(|child| {
            let child_windows = child
                .bindings
                .effective_dma
                .windows
                .as_ref()
                .map(|windows| {
                    windows
                        .iter()
                        .map(|window| DmaWindow {
                            cpu_start: window.cpu_start,
                            dma_start: window.dma_start,
                            size: window.size,
                        })
                        .collect()
                });
            let iommus = iommu_references(
                &child
                    .bindings
                    .references
                    .iter()
                    .filter(|reference| reference.property.as_ref() == "iommus")
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            PciFunctionDmaInfo {
                firmware_path: child.path.clone(),
                bus: child.bus,
                device: child.device,
                function: child.function,
                coherent: child.bindings.effective_dma.coherent,
                windows: child_windows,
                iommus,
                unsupported: child.bindings.effective_dma.unsupported,
            }
        })
        .collect();
    PciHostDmaInfo {
        windows,
        iommu_map,
        iommus,
        unsupported,
        functions,
    }
}

fn iommu_references(
    references: &[general::firmware::dtb::DtbProviderReference],
) -> Vec<PciIommuReference> {
    references
        .iter()
        .filter(|reference| {
            reference.property.as_ref() == "iommus"
                && reference.provider_available == Some(true)
                && reference.phandle != 0
        })
        .filter_map(|reference| {
            Some(PciIommuReference {
                provider_path: reference.provider_path.clone()?,
                provider_phandle: reference.phandle,
                args: reference.args.clone(),
            })
        })
        .collect()
}

fn pci_host_window(range: &DtbPciRangeInfo) -> PciHostBridgeWindow {
    PciHostBridgeWindow {
        space: match range.space {
            DtbPciAddressSpace::Io => PciHostAddressSpace::Io,
            DtbPciAddressSpace::Memory => PciHostAddressSpace::Memory,
            DtbPciAddressSpace::PrefetchableMemory => PciHostAddressSpace::PrefetchableMemory,
            DtbPciAddressSpace::Unknown(value) => PciHostAddressSpace::Unknown(value),
        },
        pci_start: range.child_addr,
        cpu_start: range.parent_addr,
        size: range.size,
    }
}

const fn pci_config_space_kind(config_space: DtbPciConfigSpace) -> PciConfigSpaceKind {
    match config_space {
        DtbPciConfigSpace::Cam => PciConfigSpaceKind::Cam,
        DtbPciConfigSpace::Ecam => PciConfigSpaceKind::Ecam,
        DtbPciConfigSpace::Ls2k1000 => PciConfigSpaceKind::Ls2k1000,
    }
}

fn pci_runtime_window(range: &DtbPciRangeInfo) -> Option<PciBarRuntimeWindow> {
    let space = match range.space {
        DtbPciAddressSpace::Io => PciBarWindowSpace::Io,
        DtbPciAddressSpace::Memory => PciBarWindowSpace::Memory,
        DtbPciAddressSpace::PrefetchableMemory => PciBarWindowSpace::PrefetchableMemory,
        DtbPciAddressSpace::Unknown(_) => return None,
    };
    PciBarRuntimeWindow::new(space, range.child_addr, range.parent_addr, range.size)
}

fn pci_windows_overlap(left: PciBarRuntimeWindow, right: PciBarRuntimeWindow) -> bool {
    let same_address_space = matches!(
        (left.space, right.space),
        (PciBarWindowSpace::Io, PciBarWindowSpace::Io)
            | (
                PciBarWindowSpace::Memory | PciBarWindowSpace::PrefetchableMemory,
                PciBarWindowSpace::Memory | PciBarWindowSpace::PrefetchableMemory,
            )
    );
    same_address_space && left.pci_start < right.pci_end && right.pci_start < left.pci_end
}

/// 安装一段按 segment 与 bus-range 分派的 PCI 子地址运行时窗口。
///
/// BAR 中保存的是 PCI 子地址；设备驱动实际访问前必须依据当前 host 的 `ranges`
/// 转换成 CPU 物理地址。多 host 不能共享一个无 BDF 上下文的全局基址。
pub(crate) fn install_bar_windows(segment: u16, host: &DtbPcieHostInfo) -> bool {
    let mut windows = Vec::new();
    for range in &host.ranges {
        if matches!(range.space, DtbPciAddressSpace::Unknown(_)) {
            continue;
        }
        let Some(window) = pci_runtime_window(range) else {
            return false;
        };
        if windows
            .iter()
            .copied()
            .any(|existing| pci_windows_overlap(existing, window))
        {
            return false;
        }
        windows.push(window);
    }
    if windows.is_empty() {
        return false;
    }

    let Some(key) = PciHostBusRange::new(segment, host.bus_start, host.bus_end) else {
        return false;
    };
    PCI_BAR_ROUTING
        .lock()
        .insert(key, PciBarRouting { windows })
        .is_ok()
}

pub(crate) fn remove_bar_windows(segment: u16, bus_start: u8, bus_end: u8) {
    let Some(key) = PciHostBusRange::new(segment, bus_start, bus_end) else {
        return;
    };
    let _ = PCI_BAR_ROUTING.lock().remove_exact(key);
}

/// 回滚尚未发布到 PCI host registry 的运行时地址窗口。
///
/// 全局 config/BAR 回调本身可以常驻；删除对应 segment+bus-range 后，任何后续
/// BDF 访问都会按“未找到 host”失败，不会落到已经拒绝的固件窗口。
pub(crate) fn remove_runtime_windows(segment: u16, bus_start: u8, bus_end: u8) {
    remove_bar_windows(segment, bus_start, bus_end);
    let Some(key) = PciHostBusRange::new(segment, bus_start, bus_end) else {
        return;
    };
    let _ = PCI_ECAM_REGIONS.lock().remove_exact(key);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PciHostRuntimeInstallError {
    BarWindows,
    Ecam,
}

/// PCI host 发布前的运行时状态事务。
///
/// BAR、ECAM 与可选 IRQ/MSI 表先于 typed host registry 安装，以保证依赖通知
/// 唤醒驱动时后端已经可用；若 host 注册失败，析构会按精确键撤销本次全部状态。
pub(crate) struct PciHostRuntimeTransaction {
    key: PciHostBusRange,
    committed: bool,
}

impl PciHostRuntimeTransaction {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PciHostRuntimeTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        remove_host_runtime_state(self.key);
    }
}

fn remove_host_runtime_state(key: PciHostBusRange) {
    remove_runtime_windows(key.segment, key.bus_start, key.bus_end);
    remove_irq_routing(key.segment, key.bus_start, key.bus_end);
    remove_msi_routing(key.segment, key.bus_start, key.bus_end);
}

pub(crate) fn begin_host_runtime_transaction(
    host: &DtbPcieHostInfo,
    device_mmio_to_virt: fn(usize) -> usize,
) -> Result<PciHostRuntimeTransaction, PciHostRuntimeInstallError> {
    let key = PciHostBusRange::new(host.domain, host.bus_start, host.bus_end)
        .ok_or(PciHostRuntimeInstallError::BarWindows)?;
    if !install_bar_windows(host.domain, host) {
        return Err(PciHostRuntimeInstallError::BarWindows);
    }
    if !install_ecam(
        host.domain,
        host.ecam_phys as u64,
        host.ecam_size as u64,
        host.bus_start,
        host.bus_end,
        host.config_space,
        device_mmio_to_virt,
    ) {
        remove_bar_windows(host.domain, host.bus_start, host.bus_end);
        return Err(PciHostRuntimeInstallError::Ecam);
    }
    Ok(PciHostRuntimeTransaction {
        key,
        committed: false,
    })
}

fn resolve_pci_bar_mapping(
    segment: u16,
    bus: u8,
    _device: u8,
    _function: u8,
    bar_type: PciBarType,
    prefetchable: bool,
    pci_address: u64,
    size: u64,
) -> Option<PciBarMapping> {
    let kind = match bar_type {
        PciBarType::Io => PciBarKind::Io,
        PciBarType::Memory => PciBarKind::Memory { prefetchable },
    };
    let routings = PCI_BAR_ROUTING.lock();
    let routing = routings.get(segment, bus)?;

    let exact_prefetchable = matches!(kind, PciBarKind::Memory { prefetchable: true });
    let window = routing
        .windows
        .iter()
        .copied()
        .filter(|window| window.accepts(kind))
        .filter(|window| window.cpu_address(pci_address, size).is_some())
        .min_by_key(|window| {
            if exact_prefetchable && window.space == PciBarWindowSpace::PrefetchableMemory {
                0
            } else {
                1
            }
        })?;
    let cpu_phys = window.cpu_address(pci_address, size)?;
    drop(routings);
    Some(PciBarMapping {
        cpu_phys,
        virt_addr: mmio_to_virt_via_stored(cpu_phys),
    })
}

/// ECAM 配置空间地址计算。
///
/// 未安装 ECAM 与 BDF/offset 越界属于不同错误：前者表示平台初始化顺序错误，
/// 后者表示调用方访问了当前 host bridge 不覆盖的 function 或寄存器。
#[inline]
fn ecam_addr(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    width: usize,
) -> Result<usize, PciConfigError> {
    if device >= PCI_DEVICES_PER_BUS
        || function >= PCI_FUNCTIONS_PER_DEVICE
        || offset >= PCI_EXTENDED_CONFIG_SPACE_SIZE
    {
        return Err(PciConfigError::InvalidOffset);
    }
    let regions = PCI_ECAM_REGIONS.lock();
    if regions.is_empty() {
        return Err(PciConfigError::Uninitialized);
    }
    let region = regions
        .get(segment, bus)
        .copied()
        .ok_or(PciConfigError::InvalidOffset)?;
    if offset >= region.function_size {
        return Err(PciConfigError::InvalidOffset);
    }
    drop(regions);

    if region.config_space == DtbPciConfigSpace::Ls2k1000 {
        return Ls2kConfigWindow::new(region.vbase, region.size, region.bus_start, region.bus_end)
            .and_then(|window| window.address(bus, device, function, offset, width))
            .map_err(|_| PciConfigError::InvalidOffset);
    }

    let rel_bus = usize::from(bus - region.bus_start);
    let off = (rel_bus << region.bus_shift)
        | (usize::from(device) << (region.function_shift + 3))
        | (usize::from(function) << region.function_shift)
        | usize::from(offset);
    if off >= region.size {
        return Err(PciConfigError::InvalidOffset);
    }
    region
        .vbase
        .checked_add(off)
        .ok_or(PciConfigError::InvalidOffset)
}

fn ecam_read_u8(seg: u16, bus: u8, dev: u8, func: u8, offset: u16) -> Result<u8, PciConfigError> {
    let a = ecam_addr(seg, bus, dev, func, offset, 1)?;
    // Safety: `ecam_addr` 已验证 BDF、偏移和已映射 ECAM 窗口边界，地址按 u8 对齐。
    Ok(unsafe { core::ptr::read_volatile(a as *const u8) })
}
fn ecam_read_u16(seg: u16, bus: u8, dev: u8, func: u8, offset: u16) -> Result<u16, PciConfigError> {
    let a = ecam_addr(seg, bus, dev, func, offset, 2)?;
    // Safety: 调用者的 config API 已校验 2 字节对齐，`ecam_addr` 已验证窗口边界。
    Ok(unsafe { core::ptr::read_volatile(a as *const u16) })
}
fn ecam_read_u32(seg: u16, bus: u8, dev: u8, func: u8, offset: u16) -> Result<u32, PciConfigError> {
    let a = ecam_addr(seg, bus, dev, func, offset, 4)?;
    // Safety: 调用者的 config API 已校验 4 字节对齐，`ecam_addr` 已验证窗口边界。
    Ok(unsafe { core::ptr::read_volatile(a as *const u32) })
}
fn ecam_write_u8(
    seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
    v: u8,
) -> Result<(), PciConfigError> {
    let a = ecam_addr(seg, bus, dev, func, offset, 1)?;
    // Safety: `ecam_addr` 已验证 BDF、偏移和已映射 ECAM 窗口边界，地址按 u8 对齐。
    unsafe { core::ptr::write_volatile(a as *mut u8, v) };
    Ok(())
}
fn ecam_write_u16(
    seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
    v: u16,
) -> Result<(), PciConfigError> {
    let a = ecam_addr(seg, bus, dev, func, offset, 2)?;
    // Safety: 调用者的 config API 已校验 2 字节对齐，`ecam_addr` 已验证窗口边界。
    unsafe { core::ptr::write_volatile(a as *mut u16, v) };
    Ok(())
}
fn ecam_write_u32(
    seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
    v: u32,
) -> Result<(), PciConfigError> {
    let a = ecam_addr(seg, bus, dev, func, offset, 4)?;
    // Safety: 调用者的 config API 已校验 4 字节对齐，`ecam_addr` 已验证窗口边界。
    unsafe { core::ptr::write_volatile(a as *mut u32, v) };
    Ok(())
}

// ECAM config access 转发的 device_mmio_to_virt —— 装载时由启动上下文传入,
// 用于把 BAR 物理地址转成当前平台可访问的内核虚拟地址。
// 默认 identity(装载前不会被用到,只是为了让类型检查通过)。
static DEVICE_MMIO_TO_VIRT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

const PCI_BAR0_OFFSET: u16 = 0x10;
const PCI_BAR_STRIDE: u16 = 4;
const PCI_BAR_IO_SPACE: u32 = 0x1;
const PCI_BAR_IO_ADDR_MASK: u32 = 0xffff_fffc;
const PCI_BAR_MEM_TYPE_MASK: u32 = 0x6;
const PCI_BAR_MEM_TYPE_20BIT: u32 = 0x2;
const PCI_BAR_MEM_TYPE_64: u32 = 0x4;
const PCI_BAR_MEM_TYPE_RESERVED: u32 = 0x6;
const PCI_BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;
const PCI_BAR_MEM_20BIT_ADDR_MASK: u32 = 0x000f_fff0;
const PCI_BAR_MEM_PREFETCHABLE: u32 = 0x8;
const PCI_BAR_PROBE_VALUE: u32 = 0xffff_ffff;
const PCI_BAR_MIN_ALIGN: u64 = 0x10;
const PCI_BAR_IO_MIN_ALIGN: u64 = 0x4;

const PCI_COMMAND_IO_SPACE: u16 = 0x0001;
const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;

const PCI_HEADER_TYPE_BRIDGE: u8 = 0x01;
const PCI_CLASS_BRIDGE: u8 = 0x06;
const PCI_SUBCLASS_PCI_BRIDGE: u8 = 0x04;

const PCI_BRIDGE_BUS_NUMBERS: u16 = 0x18;
const PCI_BRIDGE_IO_BASE_LIMIT: u16 = 0x1c;
const PCI_BRIDGE_MEMORY_BASE_LIMIT: u16 = 0x20;
const PCI_BRIDGE_PREFETCH_BASE_LIMIT: u16 = 0x24;
const PCI_BRIDGE_PREFETCH_BASE_UPPER: u16 = 0x28;
const PCI_BRIDGE_PREFETCH_LIMIT_UPPER: u16 = 0x2c;
const PCI_BRIDGE_IO_BASE_LIMIT_UPPER: u16 = 0x30;

const PCI_BRIDGE_IO_TYPE_32: u8 = 0x1;
const PCI_BRIDGE_PREFETCH_TYPE_64: u16 = 0x1;

fn write_bar_u64_transactional<E>(
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

fn mmio_to_virt_via_stored(phys: usize) -> usize {
    let f = DEVICE_MMIO_TO_VIRT.load(Ordering::Acquire);
    if f == 0 {
        return phys;
    }
    // Safety: 存入的是合法的函数指针，由 `install_ecam` 发布且模块存活期间不会失效。
    let f: fn(usize) -> usize = unsafe { core::mem::transmute(f) };
    f(phys)
}

fn pci_child_interrupt_key(
    bus: u8,
    device: u8,
    function: u8,
    interrupt_pin: u8,
    address_cells: usize,
    interrupt_cells: usize,
) -> Option<Box<[u32]>> {
    let total = address_cells.checked_add(interrupt_cells)?;
    if address_cells == 0 || interrupt_cells == 0 {
        return None;
    }
    let mut cells = Vec::new();
    cells.resize(total, 0);
    // Open Firmware PCI child address 的第一个 cell 保存 bus/device/function。
    // 其余地址 cell 对 INTx 路由通常为 0；interrupt-map-mask 会决定参与匹配的位。
    cells[0] = ((bus as u32) << 16) | ((device as u32) << 11) | ((function as u32) << 8);
    cells[address_cells] = interrupt_pin as u32;
    Some(cells.into_boxed_slice())
}

fn resolve_pci_irq(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    interrupt_pin: Option<u8>,
    _interrupt_line: Option<u8>,
) -> Option<PciResolvedIrq> {
    let interrupt_pin = interrupt_pin?;
    let routing = {
        let routings = PCI_IRQ_ROUTING.lock();
        Arc::clone(routings.get(segment, bus)?)
    };
    let route_key = {
        let intx = routing.intx.lock();
        intx.resolve(bus, device, function, interrupt_pin)?
    };
    let line = match &routing.backend {
        PciIrqRoutingBackend::Dtb {
            address_cells,
            interrupt_cells,
            resolver,
        } => {
            let key = pci_child_interrupt_key(
                route_key.bus,
                route_key.device,
                route_key.function,
                route_key.pin,
                *address_cells,
                *interrupt_cells,
            )?;
            let (child_address, child_interrupt) = key.split_at(*address_cells);
            let route = resolve_dtb_pci_interrupt(resolver, child_address, child_interrupt)
                .ok()
                .flatten()?;
            irq::translate_firmware_irq(Some(route.parent), &route.parent_specifier)?
        }
        PciIrqRoutingBackend::Ls2k1000(table) => {
            let (parent, specifier) =
                table.resolve(route_key.bus, route_key.device, route_key.function)?;
            irq::translate_firmware_irq(Some(parent), &[specifier])?
        }
    };
    Some(PciResolvedIrq::shared(line))
}

fn pci_requester_id(bus: u8, device: u8, function: u8) -> u32 {
    ((bus as u32) << 8) | ((device as u32) << 3) | function as u32
}

fn resolve_pci_msi(segment: u16, bus: u8, device: u8, function: u8) -> Option<msi::MsiHandle> {
    let routings = PCI_MSI_ROUTING.lock();
    let routing = routings.get(segment, bus)?;
    let requester = pci_requester_id(bus, device, function);
    let targets = routing.mode.allocation_targets(requester);
    drop(routings);
    allocate_first_available(&targets, |target| {
        msi::allocate_msi(target.controller, target.device_id)
    })
}

pub(crate) fn install_irq_routing(segment: u16, host: &DtbPcieHostInfo) -> bool {
    if host.config_space == DtbPciConfigSpace::Ls2k1000 {
        let Some(table) = ls2k_irq_table(host) else {
            return false;
        };
        let Some(key) = PciHostBusRange::new(segment, host.bus_start, host.bus_end) else {
            return false;
        };
        let Some(intx) = PciIntxRouting::new(host.bus_start, Vec::new()) else {
            return false;
        };
        return PCI_IRQ_ROUTING
            .lock()
            .insert(
                key,
                Arc::new(PciIrqRouting {
                    backend: PciIrqRoutingBackend::Ls2k1000(table),
                    intx: Spinlock::new(intx),
                }),
            )
            .is_ok();
    }

    // 所有行先在局部内存中构造完整候选；只有全部可用才发布，
    // 因此验证或插入失败时不会留下半张表，也不会破坏旧表。
    let expected = match host.address_cells.checked_add(host.interrupt_cells) {
        Some(expected) if expected != 0 => expected,
        _ => return false,
    };
    let Some(mask) = host.interrupt_map_mask.as_ref() else {
        return false;
    };
    let Some(resolver) = host.interrupt_resolver.as_ref() else {
        return false;
    };
    if mask.len() != expected
        || host
            .interrupt_map_pass_thru
            .as_ref()
            .is_some_and(|pass_thru| pass_thru.len() != expected)
        || host.interrupt_map.is_empty()
    {
        return false;
    }
    for entry in &host.interrupt_map {
        if entry.child_address.len() != host.address_cells
            || entry.child_interrupt.len() != host.interrupt_cells
            || entry.child_address.len() + entry.child_interrupt.len() != expected
        {
            return false;
        }
    }

    let Some(key) = PciHostBusRange::new(segment, host.bus_start, host.bus_end) else {
        return false;
    };
    let Some(intx) = PciIntxRouting::new(host.bus_start, Vec::new()) else {
        return false;
    };
    // kernel resolver 的 clone 在表锁外执行；额外保留 candidate Arc，保证插入失败时
    // 锁内释放的 published Arc 不是最后一个引用，不会跨 ELM 调用 resolver drop。
    let candidate = Arc::new(PciIrqRouting {
        backend: PciIrqRoutingBackend::Dtb {
            address_cells: host.address_cells,
            interrupt_cells: host.interrupt_cells,
            resolver: resolver.clone_owned(),
        },
        intx: Spinlock::new(intx),
    });
    let published = Arc::clone(&candidate);
    let installed = {
        let mut routings = PCI_IRQ_ROUTING.lock();
        routings.insert(key, published).is_ok()
    };
    drop(candidate);
    installed
}

pub(crate) fn install_msi_routing(segment: u16, host: &DtbPcieHostInfo) -> bool {
    let mode = if host.msi_map_present {
        let mut routes = Vec::new();
        for entry in &host.msi_map {
            let Some(route) = PciMsiMapRoute::new(
                entry.requester_base,
                entry.controller,
                &entry.msi_specifier,
                entry.length,
            ) else {
                return false;
            };
            routes.push(route);
        }
        PciMsiRoutingMode::Map {
            mask: host.msi_map_mask,
            routes,
        }
    } else {
        let mut parents = Vec::new();
        for parent in &host.msi_parents {
            let Some(route) = PciMsiParentRoute::new(parent.controller, &parent.msi_specifier)
            else {
                return false;
            };
            parents.push(route);
        }
        PciMsiRoutingMode::Parents(parents)
    };
    let route_count = match &mode {
        PciMsiRoutingMode::Map { routes, .. } => routes.len(),
        PciMsiRoutingMode::Parents(parents) => parents.len(),
    };
    if route_count == 0 {
        return false;
    }
    let Some(key) = PciHostBusRange::new(segment, host.bus_start, host.bus_end) else {
        return false;
    };
    PCI_MSI_ROUTING
        .lock()
        .insert(key, PciMsiRouting { mode })
        .is_ok()
}

fn remove_irq_routing(segment: u16, bus_start: u8, bus_end: u8) {
    let Some(key) = PciHostBusRange::new(segment, bus_start, bus_end) else {
        return;
    };
    let removed = {
        let mut routings = PCI_IRQ_ROUTING.lock();
        routings.remove_exact(key)
    };
    // 最后一个 Arc 可能调用 kernel 导出的 resolver drop，必须在表锁外释放。
    drop(removed);
}

fn remove_msi_routing(segment: u16, bus_start: u8, bus_end: u8) {
    let Some(key) = PciHostBusRange::new(segment, bus_start, bus_end) else {
        return;
    };
    let _ = PCI_MSI_ROUTING.lock().remove_exact(key);
}

pub(crate) fn msi_route_count(host: &DtbPcieHostInfo) -> usize {
    if host.msi_map_present {
        host.msi_map.len()
    } else {
        host.msi_parents.len()
    }
}

pub(crate) fn usable_irq_route_count(host: &DtbPcieHostInfo) -> usize {
    if host.config_space == DtbPciConfigSpace::Ls2k1000 {
        ls2k_irq_table(host).map_or(0, Ls2kRootIrqTable::len)
    } else {
        host.interrupt_map.len()
    }
}

fn ls2k_irq_table(host: &DtbPcieHostInfo) -> Option<Ls2kRootIrqTable> {
    let mut routes = Vec::new();
    for child in &host.children {
        if child.bus != host.bus_start {
            continue;
        }
        let [interrupt] = child.interrupts.as_slice() else {
            return None;
        };
        let parent = interrupt.parent?;
        let [specifier] = interrupt.specifier.as_ref() else {
            return None;
        };
        routes.push(Ls2kRootIrqRoute::new(
            child.device,
            child.function,
            parent,
            *specifier,
        ));
    }
    Ls2kRootIrqTable::new(host.bus_start, &routes).ok()
}

fn usize_ranges_overlap(
    left_start: usize,
    left_size: usize,
    right_start: usize,
    right_size: usize,
) -> bool {
    let Some(left_end) = left_start.checked_add(left_size) else {
        return true;
    };
    let Some(right_end) = right_start.checked_add(right_size) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

fn ensure_pci_access_callbacks(device_mmio_to_virt: fn(usize) -> usize) -> bool {
    let callback = device_mmio_to_virt as usize;
    let mut installed = PCI_ACCESS_INIT.lock();
    if let Some(existing) = *installed {
        return existing == callback;
    }

    // 回调一经发布即可被 PCI/ELM 路径调用，因此先准备地址转换，再把 config 与
    // BAR mapper 作为同一个 owned-resource 原子安装。
    DEVICE_MMIO_TO_VIRT.store(callback, Ordering::Release);
    let access = PciConfigAccess {
        read_u8: ecam_read_u8,
        read_u16: ecam_read_u16,
        read_u32: ecam_read_u32,
        write_u8: ecam_write_u8,
        write_u16: ecam_write_u16,
        write_u32: ecam_write_u32,
        device_mmio_to_virt: mmio_to_virt_via_stored,
        resolve_irq: Some(resolve_pci_irq),
        allocate_msi: Some(resolve_pci_msi),
    };
    if try_install_pci_access_pair(access, resolve_pci_bar_mapping).is_err() {
        DEVICE_MMIO_TO_VIRT.store(0, Ordering::Release);
        return false;
    }
    *installed = Some(callback);
    true
}

/// 装载 ECAM 访问。`phys_base` 是物理地址，`device_mmio_to_virt` 负责转虚拟。
///
/// 候选窗口先完成大小、地址溢出、host key 与物理/虚拟别名检查，再一次发布；
/// 任何失败都不会替换先前合法 host 的 ECAM。
pub(crate) fn install_ecam(
    segment: u16,
    phys_base: u64,
    size: u64,
    bus_start: u8,
    bus_end: u8,
    config_space: DtbPciConfigSpace,
    device_mmio_to_virt: fn(usize) -> usize,
) -> bool {
    let Ok(phys_base) = usize::try_from(phys_base) else {
        return false;
    };
    let Ok(size) = usize::try_from(size) else {
        return false;
    };
    let Some(key) = PciHostBusRange::new(segment, bus_start, bus_end) else {
        return false;
    };
    let Some(bus_count) = usize::from(bus_end)
        .checked_sub(usize::from(bus_start))
        .and_then(|count| count.checked_add(1))
    else {
        return false;
    };
    let config_kind = pci_config_space_kind(config_space);
    let bus_shift = config_kind.bus_shift();
    let function_shift = config_kind.function_shift();
    let function_size = config_kind.bytes_per_function();
    let linear_size = bus_count.checked_mul(config_kind.bytes_per_bus());
    let valid_size = match config_space {
        DtbPciConfigSpace::Ls2k1000 => {
            Ls2kConfigWindow::new(phys_base, size, bus_start, bus_end).is_ok()
        }
        DtbPciConfigSpace::Cam | DtbPciConfigSpace::Ecam => {
            linear_size.is_some_and(|required| size >= required)
        }
    };
    if !valid_size || phys_base.checked_add(size).is_none() {
        return false;
    }
    let vbase = device_mmio_to_virt(phys_base);
    if vbase.checked_add(size).is_none() {
        return false;
    }
    if config_space == DtbPciConfigSpace::Ls2k1000
        && Ls2kConfigWindow::new(vbase, size, bus_start, bus_end).is_err()
    {
        return false;
    }
    let candidate = PciEcamRegion {
        phys_base,
        bus_start,
        bus_end,
        bus_shift,
        function_shift,
        function_size,
        vbase,
        size,
        config_space,
    };
    if !ensure_pci_access_callbacks(device_mmio_to_virt) {
        return false;
    }
    let mut regions = PCI_ECAM_REGIONS.lock();
    if regions.values().any(|existing| {
        usize_ranges_overlap(
            existing.phys_base,
            existing.size,
            candidate.phys_base,
            candidate.size,
        ) || usize_ranges_overlap(
            existing.vbase,
            existing.size,
            candidate.vbase,
            candidate.size,
        )
    }) {
        return false;
    }
    regions.insert(key, candidate).is_ok()
}

// 这一段负责标准 PCI 拓扑枚举与资源配置。桥按深度优先顺序临时打开完整
// subordinate 范围，回溯时收紧到真实后代；BAR 则先全局保留固件地址，再只给
// 缺失或冲突的资源分配新地址。

#[derive(Clone, Copy)]
struct PciBdf {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

struct PciTopology {
    root: PciBusNode,
}

struct PciBusNode {
    bus: u8,
    functions: Vec<PciFunctionNode>,
}

struct PciFunctionNode {
    bdf: PciBdf,
    pci: PciDevice,
    original_command: Option<u16>,
    bars: Vec<PciBarResource>,
    saw_io_bar: bool,
    saw_memory_bar: bool,
    io_failed: bool,
    memory_failed: bool,
    bridge: Option<PciBridgeNode>,
}

struct PciBridgeNode {
    io_32bit: bool,
    prefetch_64bit: bool,
    downstream: Box<PciBusNode>,
}

#[derive(Clone, Copy)]
struct PciBridgeDiscovery {
    function_index: usize,
    bus_latency: u32,
    io_32bit: bool,
    prefetch_64bit: bool,
}

#[derive(Clone, Copy)]
struct PciIntxUpstream {
    root_device: u8,
    root_function: u8,
    swizzle_offset: u8,
}

struct PciBarResource {
    index: u16,
    kind: PciBarKind,
    width: PciBarAddressWidth,
    size: u64,
    original_low: u32,
    original_high: Option<u32>,
    original_address: u64,
    allocation: Option<PciBarAllocation>,
}

/// 从已成功配置的 bridge 树生成每条下游 bus 的根桥 key 与累计 swizzle。
fn intx_topology(topology: &PciTopology) -> Option<PciIntxRouting> {
    let mut routes = Vec::new();
    collect_intx_bridge_routes(&topology.root, None, &mut routes)?;
    PciIntxRouting::new(topology.root.bus, routes)
}

fn collect_intx_bridge_routes(
    bus: &PciBusNode,
    upstream: Option<PciIntxUpstream>,
    routes: &mut Vec<PciIntxBridgeRoute>,
) -> Option<()> {
    for function in &bus.functions {
        let Some(bridge) = &function.bridge else {
            continue;
        };
        if function.bdf.bus != bus.bus {
            return None;
        }
        let next = match upstream {
            None => PciIntxUpstream {
                root_device: function.bdf.device,
                root_function: function.bdf.function,
                swizzle_offset: 0,
            },
            Some(upstream) => PciIntxUpstream {
                root_device: upstream.root_device,
                root_function: upstream.root_function,
                swizzle_offset: (upstream.swizzle_offset + function.bdf.device % 4) % 4,
            },
        };
        routes.push(PciIntxBridgeRoute::new(
            bridge.downstream.bus,
            next.root_device,
            next.root_function,
            next.swizzle_offset,
        )?);
        collect_intx_bridge_routes(&bridge.downstream, Some(next), routes)?;
    }
    Some(())
}

/// 在 endpoint 注册前原子替换 host 的纯拓扑快照。
fn publish_intx_topology(segment: u16, topology: &PciTopology) -> bool {
    let Some(candidate) = intx_topology(topology) else {
        return false;
    };
    let routing = {
        let routings = PCI_IRQ_ROUTING.lock();
        let Some(routing) = routings.get(segment, topology.root.bus) else {
            return false;
        };
        Arc::clone(routing)
    };
    *routing.intx.lock() = candidate;
    true
}

fn configure_pci_hierarchy(host: &DtbPcieHostInfo) -> PciTopology {
    let windows = host
        .ranges
        .iter()
        .filter_map(pci_runtime_window)
        .collect::<Vec<_>>();
    for window in &windows {
        log::printk!(
            "[pci-host-ecam] resource {:?} pci={:#x}..{:#x} cpu={:#x}..{:#x}",
            window.space,
            window.pci_start,
            window.pci_end,
            window.cpu_start,
            window.cpu_start + window.size() as usize
        );
    }

    let mut buses = PciBusNumberAllocator::new(host.bus_start, host.bus_end);
    let mut root = scan_pci_bus(host.domain, host.bus_start, host.bus_end, &mut buses);
    let mut allocator = PciBarWindowAllocator::new(&windows);
    prepare_bar_requirements(&mut root);
    reserve_firmware_bars(&mut root, &mut allocator);
    assign_missing_bars(&mut root, &mut allocator);
    let _ = finalize_pci_bus(&mut root);
    PciTopology { root }
}

fn scan_pci_bus(
    segment: u16,
    bus: u8,
    bus_end: u8,
    buses: &mut PciBusNumberAllocator,
) -> PciBusNode {
    let mut functions = scan_pci_functions(segment, bus);
    let mut bridges = Vec::new();
    // 先关闭当前 bus 上的全部桥，再逐一临时打开。否则某个尚未处理的固件
    // subordinate 范围可能与正在递归扫描的兄弟桥重叠，导致配置事务被错误转发。
    for (function_index, function) in functions.iter_mut().enumerate() {
        if !is_standard_pci_bridge(function) {
            continue;
        }
        let Some(original_command) = function.original_command else {
            let _ = function.pci.try_set_command(0);
            disable_bridge_forwarding(&function.pci);
            disable_bridge_bus_numbers(&function.pci, bus);
            log::printk!(
                "[pci-host-ecam] bridge {} has unreadable command register; downstream remains closed",
                function.pci.pnp_id()
            );
            continue;
        };
        // type 位是只读能力位，必须在把旧 forwarding window 关闭前采样。
        let io_base_limit = function
            .pci
            .try_read_config_u16(PCI_BRIDGE_IO_BASE_LIMIT)
            .unwrap_or(0);
        let prefetch_base_limit = function
            .pci
            .try_read_config_u32(PCI_BRIDGE_PREFETCH_BASE_LIMIT)
            .unwrap_or(0);
        let bus_latency = match function.pci.try_read_config_u32(PCI_BRIDGE_BUS_NUMBERS) {
            Ok(value) => value & 0xff00_0000,
            Err(error) => {
                disable_bridge_bus_numbers(&function.pci, bus);
                log::printk!(
                    "[pci-host-ecam] bridge {} bus register read failed: {:?}",
                    function.pci.pnp_id(),
                    error
                );
                continue;
            }
        };
        if let Err(error) = function
            .pci
            .try_set_command(original_command & !(PCI_COMMAND_IO_SPACE | PCI_COMMAND_MEMORY_SPACE))
        {
            disable_bridge_forwarding(&function.pci);
            disable_bridge_bus_numbers(&function.pci, bus);
            log::printk!(
                "[pci-host-ecam] bridge {} cannot disable forwarding: {:?}",
                function.pci.pnp_id(),
                error
            );
            continue;
        }
        let io_32bit = io_base_limit as u8 & 0xf == PCI_BRIDGE_IO_TYPE_32
            && (io_base_limit >> 8) as u8 & 0xf == PCI_BRIDGE_IO_TYPE_32;
        let prefetch_64bit = prefetch_base_limit as u16 & 0xf == PCI_BRIDGE_PREFETCH_TYPE_64
            && (prefetch_base_limit >> 16) as u16 & 0xf == PCI_BRIDGE_PREFETCH_TYPE_64;
        disable_bridge_forwarding(&function.pci);
        disable_bridge_bus_numbers(&function.pci, bus);
        bridges.push(PciBridgeDiscovery {
            function_index,
            bus_latency,
            io_32bit,
            prefetch_64bit,
        });
    }

    for discovery in bridges {
        let function = &mut functions[discovery.function_index];
        let Some(secondary) = buses.allocate() else {
            log::printk!(
                "[pci-host-ecam] bus-range exhausted at bridge {}; downstream disabled",
                function.pci.pnp_id()
            );
            continue;
        };
        if let Err(error) = write_bridge_bus_numbers(
            &function.pci,
            discovery.bus_latency,
            bus,
            secondary,
            bus_end,
        ) {
            log::printk!(
                "[pci-host-ecam] bridge {} temporary bus assignment failed: {:?}",
                function.pci.pnp_id(),
                error
            );
            disable_bridge_bus_numbers(&function.pci, bus);
            continue;
        }

        let downstream = scan_pci_bus(segment, secondary, bus_end, buses);
        let subordinate = buses.last_allocated(secondary);
        if let Err(error) = write_bridge_bus_numbers(
            &function.pci,
            discovery.bus_latency,
            bus,
            secondary,
            subordinate,
        ) {
            log::printk!(
                "[pci-host-ecam] bridge {} final subordinate write failed: {:?}; downstream disabled",
                function.pci.pnp_id(),
                error
            );
            disable_bridge_bus_numbers(&function.pci, bus);
            continue;
        }

        log::printk!(
            "[pci-host-ecam] bridge {} primary={:#x} secondary={:#x} subordinate={:#x}",
            function.pci.pnp_id(),
            bus,
            secondary,
            subordinate
        );
        function.bridge = Some(PciBridgeNode {
            io_32bit: discovery.io_32bit,
            prefetch_64bit: discovery.prefetch_64bit,
            downstream: Box::new(downstream),
        });
    }
    PciBusNode { bus, functions }
}

fn scan_pci_functions(segment: u16, bus: u8) -> Vec<PciFunctionNode> {
    let mut functions = Vec::new();
    for device in 0u8..PCI_DEVICES_PER_BUS {
        let Some(function_zero) = PciDevice::new_unregistered(segment, bus, device, 0) else {
            continue;
        };
        let multi_function = function_zero.info().is_some_and(|info| info.multi_function);
        functions.push(new_function_node(segment, bus, device, 0, function_zero));
        if !multi_function {
            continue;
        }
        for function in 1u8..PCI_FUNCTIONS_PER_DEVICE {
            let Some(pci) = PciDevice::new_unregistered(segment, bus, device, function) else {
                continue;
            };
            functions.push(new_function_node(segment, bus, device, function, pci));
        }
    }
    functions
}

fn new_function_node(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    pci: PciDevice,
) -> PciFunctionNode {
    let original_command = pci.try_command().ok();
    PciFunctionNode {
        bdf: PciBdf {
            segment,
            bus,
            device,
            function,
        },
        pci,
        original_command,
        bars: Vec::new(),
        saw_io_bar: false,
        saw_memory_bar: false,
        io_failed: original_command.is_none(),
        memory_failed: original_command.is_none(),
        bridge: None,
    }
}

fn is_standard_pci_bridge(function: &PciFunctionNode) -> bool {
    function.pci.info().is_some_and(|info| {
        let (class, subclass, _) = info.class_code();
        info.header_type == PCI_HEADER_TYPE_BRIDGE
            && class == PCI_CLASS_BRIDGE
            && subclass == PCI_SUBCLASS_PCI_BRIDGE
    })
}

fn write_bridge_bus_numbers(
    pci: &PciDevice,
    latency: u32,
    primary: u8,
    secondary: u8,
    subordinate: u8,
) -> Result<(), PciConfigError> {
    pci.try_write_config_u32(
        PCI_BRIDGE_BUS_NUMBERS,
        latency | u32::from(primary) | (u32::from(secondary) << 8) | (u32::from(subordinate) << 16),
    )
}

fn disable_bridge_bus_numbers(pci: &PciDevice, primary: u8) {
    let latency = pci.try_read_config_u32(PCI_BRIDGE_BUS_NUMBERS).unwrap_or(0) & 0xff00_0000;
    let _ = write_bridge_bus_numbers(pci, latency, primary, u8::MAX, 0);
}

fn prepare_bar_requirements(bus: &mut PciBusNode) {
    for function in &mut bus.functions {
        probe_function_bars(function);
        if let Some(bridge) = &mut function.bridge {
            prepare_bar_requirements(&mut bridge.downstream);
        }
    }
}

fn probe_function_bars(function: &mut PciFunctionNode) {
    let Some(original_command) = function.original_command else {
        let _ = function.pci.try_set_command(0);
        return;
    };
    if let Err(error) = function
        .pci
        .try_set_command(original_command & !(PCI_COMMAND_IO_SPACE | PCI_COMMAND_MEMORY_SPACE))
    {
        function.io_failed = true;
        function.memory_failed = true;
        log::printk!(
            "[pci-host-ecam] {} cannot quiesce BAR decode: {:?}",
            function.pci.pnp_id(),
            error
        );
        return;
    }

    let bar_count = function.pci.bar_count();
    let mut index = 0u16;
    while usize::from(index) < bar_count {
        let offset = PCI_BAR0_OFFSET + index * PCI_BAR_STRIDE;
        let original_low = match function.pci.try_read_config_u32(offset) {
            Ok(value) => value,
            Err(error) => {
                function.io_failed = true;
                function.memory_failed = true;
                log::printk!(
                    "[pci-host-ecam] {} BAR{} read failed: {:?}",
                    function.pci.pnp_id(),
                    index,
                    error
                );
                break;
            }
        };

        if original_low & PCI_BAR_IO_SPACE != 0 {
            function.saw_io_bar = true;
            match probe_io_bar(&function.pci, offset, original_low) {
                Ok(Some((size, address))) if size.is_power_of_two() => {
                    function.bars.push(PciBarResource {
                        index,
                        kind: PciBarKind::Io,
                        width: PciBarAddressWidth::Bits32,
                        size,
                        original_low,
                        original_high: None,
                        original_address: address,
                        allocation: None,
                    });
                }
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => {
                    function.io_failed = true;
                    log::printk!(
                        "[pci-host-ecam] {} I/O BAR{} is malformed or not probeable",
                        function.pci.pnp_id(),
                        index
                    );
                }
            }
            index += 1;
            continue;
        }

        function.saw_memory_bar = true;
        let width = match original_low & PCI_BAR_MEM_TYPE_MASK {
            PCI_BAR_MEM_TYPE_20BIT => PciBarAddressWidth::Bits20,
            PCI_BAR_MEM_TYPE_64 => PciBarAddressWidth::Bits64,
            PCI_BAR_MEM_TYPE_RESERVED => {
                function.memory_failed = true;
                log::printk!(
                    "[pci-host-ecam] {} BAR{} uses reserved memory type",
                    function.pci.pnp_id(),
                    index
                );
                index += 1;
                continue;
            }
            _ => PciBarAddressWidth::Bits32,
        };
        let is_64bit = width == PciBarAddressWidth::Bits64;
        if is_64bit && usize::from(index + 1) >= bar_count {
            function.memory_failed = true;
            log::printk!(
                "[pci-host-ecam] {} BAR{} is a truncated 64-bit pair",
                function.pci.pnp_id(),
                index
            );
            break;
        }
        match probe_memory_bar(&function.pci, offset, original_low, width) {
            Ok(Some((size, address, original_high))) if size.is_power_of_two() => {
                function.bars.push(PciBarResource {
                    index,
                    kind: PciBarKind::Memory {
                        prefetchable: original_low & PCI_BAR_MEM_PREFETCHABLE != 0,
                    },
                    width,
                    size,
                    original_low,
                    original_high,
                    original_address: address,
                    allocation: None,
                });
            }
            Ok(None) => {}
            Ok(Some(_)) | Err(_) => {
                function.memory_failed = true;
                log::printk!(
                    "[pci-host-ecam] {} BAR{} is malformed or not probeable",
                    function.pci.pnp_id(),
                    index
                );
            }
        }
        index += if is_64bit { 2 } else { 1 };
    }
}

fn probe_io_bar(
    pci: &PciDevice,
    offset: u16,
    original: u32,
) -> Result<Option<(u64, u64)>, PciConfigError> {
    pci.try_write_config_u32(offset, PCI_BAR_PROBE_VALUE)?;
    let size_raw = pci.try_read_config_u32(offset);
    let restore = pci.try_write_config_u32(offset, original);
    let size_raw = size_raw?;
    restore?;
    let size_bits = size_raw & PCI_BAR_IO_ADDR_MASK;
    let size = (!size_bits).wrapping_add(1) as u64;
    if size == 0 {
        return Ok(None);
    }
    Ok(Some((
        size.max(PCI_BAR_IO_MIN_ALIGN),
        u64::from(original & PCI_BAR_IO_ADDR_MASK),
    )))
}

fn probe_memory_bar(
    pci: &PciDevice,
    offset: u16,
    original_low: u32,
    width: PciBarAddressWidth,
) -> Result<Option<(u64, u64, Option<u32>)>, PciConfigError> {
    let is_64bit = width == PciBarAddressWidth::Bits64;
    let high_offset = offset + PCI_BAR_STRIDE;
    let original_high = if is_64bit {
        Some(pci.try_read_config_u32(high_offset)?)
    } else {
        None
    };
    pci.try_write_config_u32(offset, PCI_BAR_PROBE_VALUE)?;
    if is_64bit {
        if let Err(error) = pci.try_write_config_u32(high_offset, PCI_BAR_PROBE_VALUE) {
            let _ = pci.try_write_config_u32(offset, original_low);
            return Err(error);
        }
    }
    let size_low = pci.try_read_config_u32(offset);
    let size_high = if is_64bit {
        pci.try_read_config_u32(high_offset).map(Some)
    } else {
        Ok(None)
    };
    let restore_low = pci.try_write_config_u32(offset, original_low);
    let restore_high = original_high.map(|high| pci.try_write_config_u32(high_offset, high));
    let size_low = size_low?;
    let size_high = size_high?;
    restore_low?;
    if let Some(restore_high) = restore_high {
        restore_high?;
    }

    let size = if let Some(high) = size_high {
        let bits = (u64::from(high) << 32) | u64::from(size_low & PCI_BAR_MEM_ADDR_MASK);
        (!bits).wrapping_add(1)
    } else if width == PciBarAddressWidth::Bits20 {
        probed_20bit_memory_bar_size(size_low)
    } else {
        u64::from((!(size_low & PCI_BAR_MEM_ADDR_MASK)).wrapping_add(1))
    };
    if size == 0 {
        return Ok(None);
    }
    let low_address = if width == PciBarAddressWidth::Bits20 {
        original_low & PCI_BAR_MEM_20BIT_ADDR_MASK
    } else {
        original_low & PCI_BAR_MEM_ADDR_MASK
    };
    let address = (u64::from(original_high.unwrap_or(0)) << 32) | u64::from(low_address);
    Ok(Some((size.max(PCI_BAR_MIN_ALIGN), address, original_high)))
}

fn reserve_firmware_bars(bus: &mut PciBusNode, allocator: &mut PciBarWindowAllocator) {
    for function in &mut bus.functions {
        for bar in &mut function.bars {
            if bar.original_address == 0 {
                continue;
            }
            let Some(allocation) = allocator.reserve(
                bar.kind,
                bar.width,
                bar.original_address,
                bar.size,
                bar.size,
            ) else {
                continue;
            };
            bar.allocation = Some(allocation);
            log::printk!(
                "[pci-host-ecam] preserve BAR{} @ {} pci={:#x} cpu={:#x} size={:#x}",
                bar.index,
                function.pci.pnp_id(),
                allocation.pci_address,
                allocation.cpu_address,
                bar.size
            );
        }
        if let Some(bridge) = &mut function.bridge {
            reserve_firmware_bars(&mut bridge.downstream, allocator);
        }
    }
}

fn assign_missing_bars(bus: &mut PciBusNode, allocator: &mut PciBarWindowAllocator) {
    for function in &mut bus.functions {
        let mut io_failed = false;
        let mut memory_failed = false;
        for bar in &mut function.bars {
            if bar.allocation.is_some() {
                continue;
            }
            let Some(allocation) = allocator.allocate(bar.kind, bar.width, bar.size, bar.size)
            else {
                match bar.kind {
                    PciBarKind::Io => io_failed = true,
                    PciBarKind::Memory { .. } => memory_failed = true,
                }
                clear_bar_address(&function.pci, bar);
                log::printk!(
                    "[pci-host-ecam] no host window can place BAR{} @ {} size={:#x}; decode remains disabled",
                    bar.index,
                    function.pci.pnp_id(),
                    bar.size
                );
                continue;
            };
            if let Err(error) = write_bar_address(&function.pci, bar, allocation.pci_address) {
                match bar.kind {
                    PciBarKind::Io => io_failed = true,
                    PciBarKind::Memory { .. } => memory_failed = true,
                }
                clear_bar_address(&function.pci, bar);
                log::printk!(
                    "[pci-host-ecam] BAR{} write failed @ {}: {:?}; decode remains disabled",
                    bar.index,
                    function.pci.pnp_id(),
                    error
                );
                continue;
            }
            bar.allocation = Some(allocation);
            log::printk!(
                "[pci-host-ecam] assign BAR{} @ {} pci={:#x} cpu={:#x} size={:#x} window={:?}",
                bar.index,
                function.pci.pnp_id(),
                allocation.pci_address,
                allocation.cpu_address,
                bar.size,
                allocation.space
            );
        }
        function.io_failed |= io_failed;
        function.memory_failed |= memory_failed;
        if let Some(bridge) = &mut function.bridge {
            allocator.begin_bridge_group();
            assign_missing_bars(&mut bridge.downstream, allocator);
            allocator.end_bridge_group();
        }
    }
}

fn write_bar_address(
    pci: &PciDevice,
    bar: &PciBarResource,
    address: u64,
) -> Result<(), PciConfigError> {
    let offset = PCI_BAR0_OFFSET + bar.index * PCI_BAR_STRIDE;
    let (address_mask, type_mask) = match bar.kind {
        PciBarKind::Io => (PCI_BAR_IO_ADDR_MASK, !PCI_BAR_IO_ADDR_MASK),
        PciBarKind::Memory { .. } if bar.width == PciBarAddressWidth::Bits20 => {
            (PCI_BAR_MEM_20BIT_ADDR_MASK, !PCI_BAR_MEM_ADDR_MASK)
        }
        PciBarKind::Memory { .. } => (PCI_BAR_MEM_ADDR_MASK, !PCI_BAR_MEM_ADDR_MASK),
    };
    let low = (address as u32 & address_mask) | (bar.original_low & type_mask);
    if let Some(original_high) = bar.original_high {
        write_bar_u64_transactional(
            bar.original_low,
            original_high,
            low,
            (address >> 32) as u32,
            |high, value| {
                pci.try_write_config_u32(
                    if high {
                        offset + PCI_BAR_STRIDE
                    } else {
                        offset
                    },
                    value,
                )
            },
        )
    } else {
        pci.try_write_config_u32(offset, low)
    }
}

fn clear_bar_address(pci: &PciDevice, bar: &PciBarResource) {
    let _ = write_bar_address(pci, bar, 0);
}

fn finalize_pci_bus(bus: &mut PciBusNode) -> PciResourceEnvelope {
    let mut bus_resources = PciResourceEnvelope::default();
    let mut apertures = PciBridgeApertureTracker::new();
    // 本 bus function 自身的 BAR 与下游 forwarding window 处在同一 PCI 地址空间；
    // 先登记本地资源，避免桥窗口按粒度向外舍入后把同级 endpoint 一并转发下去。
    for function in &bus.functions {
        let mut local = function_resource_envelope(function);
        if function.io_failed {
            local.clear(PciResourceSpace::Io);
        }
        if function.memory_failed {
            local.clear(PciResourceSpace::Memory);
            local.clear(PciResourceSpace::PrefetchableMemory);
        }
        for space in [
            PciResourceSpace::Io,
            PciResourceSpace::Memory,
            PciResourceSpace::PrefetchableMemory,
        ] {
            if let Some(range) = local.range(space) {
                let _ = apertures.reserve(space, range);
            }
        }
    }
    for function in &mut bus.functions {
        let mut downstream = if let Some(bridge) = &mut function.bridge {
            finalize_pci_bus(&mut bridge.downstream)
        } else {
            PciResourceEnvelope::default()
        };

        if let Some(bridge) = &function.bridge {
            downstream = configure_bridge_forwarding(
                &function.pci,
                bridge,
                downstream,
                &mut apertures,
                function.io_failed,
                function.memory_failed,
            );
        }

        let own = function_resource_envelope(function);
        let io_needed = own.range(PciResourceSpace::Io).is_some()
            || downstream.range(PciResourceSpace::Io).is_some();
        let memory_needed = own.range(PciResourceSpace::Memory).is_some()
            || own.range(PciResourceSpace::PrefetchableMemory).is_some()
            || downstream.range(PciResourceSpace::Memory).is_some()
            || downstream
                .range(PciResourceSpace::PrefetchableMemory)
                .is_some();
        let Some(original_command) = function.original_command else {
            continue;
        };
        let mut command = original_command & !(PCI_COMMAND_IO_SPACE | PCI_COMMAND_MEMORY_SPACE);
        if !function.io_failed
            && (io_needed || (!function.saw_io_bar && original_command & PCI_COMMAND_IO_SPACE != 0))
        {
            command |= PCI_COMMAND_IO_SPACE;
        }
        if !function.memory_failed
            && (memory_needed
                || (!function.saw_memory_bar && original_command & PCI_COMMAND_MEMORY_SPACE != 0))
        {
            command |= PCI_COMMAND_MEMORY_SPACE;
        }
        if let Err(error) = function.pci.try_set_command(command) {
            if function.bridge.is_some() {
                disable_bridge_forwarding(&function.pci);
            }
            log::printk!(
                "[pci-host-ecam] {} command restore failed: {:?}; resources omitted",
                function.pci.pnp_id(),
                error
            );
            continue;
        }

        if command & PCI_COMMAND_IO_SPACE != 0 {
            if let Some(range) = own.range(PciResourceSpace::Io) {
                bus_resources.include_range(PciResourceSpace::Io, range);
            }
            if let Some(range) = downstream.range(PciResourceSpace::Io) {
                bus_resources.include_range(PciResourceSpace::Io, range);
            }
        }
        if command & PCI_COMMAND_MEMORY_SPACE != 0 {
            for space in [
                PciResourceSpace::Memory,
                PciResourceSpace::PrefetchableMemory,
            ] {
                if let Some(range) = own.range(space) {
                    bus_resources.include_range(space, range);
                }
                if let Some(range) = downstream.range(space) {
                    bus_resources.include_range(space, range);
                }
            }
        }
    }
    bus_resources
}

fn function_resource_envelope(function: &PciFunctionNode) -> PciResourceEnvelope {
    let mut resources = PciResourceEnvelope::default();
    for bar in &function.bars {
        let Some(allocation) = bar.allocation else {
            continue;
        };
        let space = match bar.kind {
            PciBarKind::Io => PciResourceSpace::Io,
            PciBarKind::Memory {
                prefetchable: false,
            } => PciResourceSpace::Memory,
            PciBarKind::Memory { prefetchable: true } => PciResourceSpace::PrefetchableMemory,
        };
        let _ = resources.include(space, allocation.pci_address, bar.size);
    }
    resources
}

fn configure_bridge_forwarding(
    pci: &PciDevice,
    bridge: &PciBridgeNode,
    mut resources: PciResourceEnvelope,
    apertures: &mut PciBridgeApertureTracker,
    io_blocked: bool,
    memory_blocked: bool,
) -> PciResourceEnvelope {
    let io_ok = !io_blocked
        && match resources.range(PciResourceSpace::Io) {
            None => program_io_aperture(pci, None, apertures),
            Some(range) => match encode_bridge_io_window(range, bridge.io_32bit) {
                Ok(window) => program_io_aperture(pci, Some(window), apertures),
                Err(error) => {
                    log::printk!(
                        "[pci-host-ecam] bridge {} cannot encode I/O aperture: {:?}",
                        pci.pnp_id(),
                        error
                    );
                    false
                }
            },
        };
    if io_blocked || !io_ok {
        disable_bridge_io_window(pci);
        resources.clear(PciResourceSpace::Io);
    }

    let memory_ok = !memory_blocked
        && match resources.range(PciResourceSpace::Memory) {
            None => program_memory_aperture(pci, None, apertures),
            Some(range) => match encode_bridge_memory_window(range) {
                Ok(window) => program_memory_aperture(pci, Some(window), apertures),
                Err(error) => {
                    log::printk!(
                        "[pci-host-ecam] bridge {} cannot encode memory aperture: {:?}",
                        pci.pnp_id(),
                        error
                    );
                    false
                }
            },
        };
    if memory_blocked || !memory_ok {
        disable_bridge_memory_window(pci);
        resources.clear(PciResourceSpace::Memory);
    }

    let prefetch_ok = !memory_blocked
        && match resources.range(PciResourceSpace::PrefetchableMemory) {
            None => program_prefetch_aperture(pci, None, apertures),
            Some(range) => match encode_bridge_prefetch_window(range, bridge.prefetch_64bit) {
                Ok(window) => program_prefetch_aperture(pci, Some(window), apertures),
                Err(error) => {
                    log::printk!(
                        "[pci-host-ecam] bridge {} cannot encode prefetch aperture: {:?}",
                        pci.pnp_id(),
                        error
                    );
                    false
                }
            },
        };
    if memory_blocked || !prefetch_ok {
        disable_bridge_prefetch_window(pci);
        resources.clear(PciResourceSpace::PrefetchableMemory);
    }
    resources
}

fn program_io_aperture(
    pci: &PciDevice,
    window: Option<PciBridgeIoWindow>,
    apertures: &mut PciBridgeApertureTracker,
) -> bool {
    let Some(window) = window else {
        disable_bridge_io_window(pci);
        return true;
    };
    if !apertures.reserve(PciResourceSpace::Io, window.forwarded) {
        log::printk!(
            "[pci-host-ecam] bridge {} I/O aperture overlaps a sibling; disabled",
            pci.pnp_id()
        );
        return false;
    }
    let lower = u16::from(window.base_low) | (u16::from(window.limit_low) << 8);
    let upper = u32::from(window.base_upper) | (u32::from(window.limit_upper) << 16);
    pci.try_write_config_u16(PCI_BRIDGE_IO_BASE_LIMIT, lower)
        .and_then(|_| pci.try_write_config_u32(PCI_BRIDGE_IO_BASE_LIMIT_UPPER, upper))
        .is_ok()
}

fn program_memory_aperture(
    pci: &PciDevice,
    window: Option<PciBridgeMemoryWindow>,
    apertures: &mut PciBridgeApertureTracker,
) -> bool {
    let Some(window) = window else {
        disable_bridge_memory_window(pci);
        return true;
    };
    if !apertures.reserve(PciResourceSpace::Memory, window.forwarded) {
        log::printk!(
            "[pci-host-ecam] bridge {} memory aperture overlaps a sibling; disabled",
            pci.pnp_id()
        );
        return false;
    }
    let value = u32::from(window.base) | (u32::from(window.limit) << 16);
    pci.try_write_config_u32(PCI_BRIDGE_MEMORY_BASE_LIMIT, value)
        .is_ok()
}

fn program_prefetch_aperture(
    pci: &PciDevice,
    window: Option<PciBridgePrefetchWindow>,
    apertures: &mut PciBridgeApertureTracker,
) -> bool {
    let Some(window) = window else {
        disable_bridge_prefetch_window(pci);
        return true;
    };
    if !apertures.reserve(PciResourceSpace::PrefetchableMemory, window.forwarded) {
        log::printk!(
            "[pci-host-ecam] bridge {} prefetch aperture overlaps a sibling; disabled",
            pci.pnp_id()
        );
        return false;
    }
    let lower = u32::from(window.base) | (u32::from(window.limit) << 16);
    pci.try_write_config_u32(PCI_BRIDGE_PREFETCH_BASE_LIMIT, lower)
        .and_then(|_| pci.try_write_config_u32(PCI_BRIDGE_PREFETCH_BASE_UPPER, window.base_upper))
        .and_then(|_| pci.try_write_config_u32(PCI_BRIDGE_PREFETCH_LIMIT_UPPER, window.limit_upper))
        .is_ok()
}

fn disable_bridge_forwarding(pci: &PciDevice) {
    disable_bridge_io_window(pci);
    disable_bridge_memory_window(pci);
    disable_bridge_prefetch_window(pci);
}

fn disable_bridge_io_window(pci: &PciDevice) {
    // base > limit 是 PCI-to-PCI bridge 规范定义的关闭表示。
    let _ = pci.try_write_config_u16(PCI_BRIDGE_IO_BASE_LIMIT, 0x00f0);
    let _ = pci.try_write_config_u32(PCI_BRIDGE_IO_BASE_LIMIT_UPPER, 0);
}

fn disable_bridge_memory_window(pci: &PciDevice) {
    let _ = pci.try_write_config_u32(PCI_BRIDGE_MEMORY_BASE_LIMIT, 0x0000_fff0);
}

fn disable_bridge_prefetch_window(pci: &PciDevice) {
    let _ = pci.try_write_config_u32(PCI_BRIDGE_PREFETCH_BASE_LIMIT, 0x0000_fff0);
    let _ = pci.try_write_config_u32(PCI_BRIDGE_PREFETCH_BASE_UPPER, 0);
    let _ = pci.try_write_config_u32(PCI_BRIDGE_PREFETCH_LIMIT_UPPER, 0);
}

fn register_pci_topology(topology: &PciTopology) -> PciScanRegisterSummary {
    let mut summary = PciScanRegisterSummary::default();
    register_pci_bus(&topology.root, &mut summary);
    summary
}

fn register_pci_bus(bus: &PciBusNode, summary: &mut PciScanRegisterSummary) {
    log::debug!("[pci-host-ecam] registering reachable bus {:#x}", bus.bus);
    for function in &bus.functions {
        let bdf = function.bdf;
        match PciDevice::register_and_probe(bdf.segment, bdf.bus, bdf.device, bdf.function) {
            Ok(registration) => {
                summary.registered += 1;
                match registration.status {
                    PciProbeStatus::Bound => summary.bound += 1,
                    PciProbeStatus::NoDriver => summary.no_driver += 1,
                    PciProbeStatus::Deferred => summary.deferred += 1,
                }
            }
            Err(PciRegisterError::NotPresent) => {}
            Err(error) => {
                summary.failed += 1;
                log::printk!(
                    "[pci-host-ecam] failed to register {:04x}:{:02x}:{:02x}.{}: {:?}",
                    bdf.segment,
                    bdf.bus,
                    bdf.device,
                    bdf.function,
                    error
                );
            }
        }
        if let Some(bridge) = &function.bridge {
            register_pci_bus(&bridge.downstream, summary);
        }
    }
}
