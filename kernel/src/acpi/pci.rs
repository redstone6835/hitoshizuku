//! ACPI PCI root bridge 到通用 PCI 子系统的启动期适配。
//!
//! MCFG 只证明 segment/bus 对应的 ECAM 配置空间地址，不能单独证明 root bridge
//! 的存在。本模块先安装 MCFG 后端，再接收 AML 枚举出的 root bridge、`_CRS`
//! 地址窗口和已验证的 `_PRT` 路由。设备子系统就绪后由调用方显式触发扫描。

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

use general::dev::irq::{self, IrqPolarity, IrqSharing, IrqTrigger};
use general::dev::pci::{
    PCI_DEVICES_PER_BUS, PCI_EXTENDED_CONFIG_SPACE_SIZE, PCI_FUNCTIONS_PER_DEVICE, PciBarMapping,
    PciBarType, PciConfigAccess, PciConfigError, PciConfigSpaceKind, PciHostAddressSpace,
    PciHostBridgeError, PciHostBridgeHandle, PciHostBridgeInfo, PciHostBridgeWindow,
    PciHostBusRange, PciHostConfigRegion, PciHostDmaInfo, PciHostTable, PciHostTableError,
    PciIntxTopology, PciResolvedIrq, PciScanRegisterSummary, pci_intx_topology_snapshot,
    pci_scan_and_register_summary, register_host_bridge, try_install_pci_access_pair,
    unregister_host_bridge,
};
use general::firmware::acpi::AcpiPciConfigRegion;
use vfs::sync::Spinlock;

const ECAM_BYTES_PER_BUS: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AcpiPciWindowSpace {
    Io,
    Memory,
    PrefetchableMemory,
}

/// AML root bridge `_CRS` 中一段 PCI 子地址到 CPU 地址的转换窗口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AcpiPciRootWindow {
    pub space: AcpiPciWindowSpace,
    pub pci_start: u64,
    pub cpu_start: usize,
    pub size: usize,
}

/// AML namespace 中已经确认存在的 PCI root bridge。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AcpiPciRootBridge {
    pub firmware_path: Box<str>,
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    pub numa_node_id: Option<u32>,
    pub windows: Vec<AcpiPciRootWindow>,
    /// 由架构启动策略确认的 PCI DMA 一致性；ACPI/MCFG 本身不能推导此值。
    pub dma_coherent: bool,
    /// 由架构启动策略确认 CPU 与 PCI DMA 地址恒等；未知时保持 `false` 并阻断 DMA。
    pub identity_dma: bool,
    /// `_PRT` 中已经归一化为当前 ACPI GSI 的 root-bus 路由。
    pub irq_routes: Vec<AcpiPciIrqRoute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AcpiPciIrqRoute {
    pub device: u8,
    /// `None` 表示 `_PRT` 的 wildcard function (`0xffff`)。
    pub function: Option<u8>,
    /// `_PRT` pin 编号，范围为 `0..=3`。
    pub pin: u8,
    pub gsi: u32,
    pub trigger: IrqTrigger,
    pub polarity: IrqPolarity,
    pub sharing: IrqSharing,
}

/// 判断 MCFG allocation 的同 segment bus-range 并集是否无缝覆盖一个 AML root。
pub(super) fn mcfg_covers_root(
    regions: &[AcpiPciConfigRegion],
    segment: u16,
    bus_start: u8,
    bus_end: u8,
) -> bool {
    bus_range_is_covered(bus_start, bus_end, |next_bus| {
        regions
            .iter()
            .filter(|region| {
                region.segment == segment
                    && region.bus_start <= next_bus
                    && next_bus <= region.bus_end
            })
            .map(|region| region.bus_end)
            .max()
    })
}

#[derive(Clone, Copy)]
struct AcpiEcamRegion {
    segment: u16,
    physical_start: usize,
    virtual_start: usize,
    size: usize,
    bus_start: u8,
    bus_end: u8,
}

impl AcpiEcamRegion {
    fn physical_start_for(self, bus: u8) -> Option<usize> {
        if !(self.bus_start..=self.bus_end).contains(&bus) {
            return None;
        }
        self.physical_start
            .checked_add(usize::from(bus - self.bus_start) << 20)
    }

    fn address(
        self,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        width: usize,
    ) -> Result<usize, PciConfigError> {
        if device >= PCI_DEVICES_PER_BUS || function >= PCI_FUNCTIONS_PER_DEVICE {
            return Err(PciConfigError::InvalidDevice);
        }
        if !matches!(width, 1 | 2 | 4)
            || !(self.bus_start..=self.bus_end).contains(&bus)
            || usize::from(offset) % width != 0
            || usize::from(offset)
                .checked_add(width)
                .is_none_or(|end| end > usize::from(PCI_EXTENDED_CONFIG_SPACE_SIZE))
        {
            return Err(PciConfigError::InvalidOffset);
        }

        let relative = (usize::from(bus - self.bus_start) << 20)
            | (usize::from(device) << 15)
            | (usize::from(function) << 12)
            | usize::from(offset);
        if relative
            .checked_add(width)
            .is_none_or(|end| end > self.size)
        {
            return Err(PciConfigError::InvalidOffset);
        }
        self.virtual_start
            .checked_add(relative)
            .ok_or(PciConfigError::InvalidOffset)
    }
}

struct AcpiPciRootRuntime {
    segment: u16,
    bus_start: u8,
    bus_end: u8,
    windows: Vec<AcpiPciRootWindow>,
    irq_routes: Vec<AcpiPciIrqRoute>,
    intx_topology: Option<PciIntxTopology>,
}

struct AcpiPciRuntime {
    ecam: PciHostTable<AcpiEcamRegion>,
    mcfg_regions: Vec<AcpiEcamRegion>,
    roots: PciHostTable<AcpiPciRootRuntime>,
    device_mmio_to_virt: Option<fn(usize) -> usize>,
    roots_published: bool,
    scan_started: bool,
}

impl AcpiPciRuntime {
    const fn new() -> Self {
        Self {
            ecam: PciHostTable::new(),
            mcfg_regions: Vec::new(),
            roots: PciHostTable::new(),
            device_mmio_to_virt: None,
            roots_published: false,
            scan_started: false,
        }
    }

    fn clear_mcfg(&mut self) {
        self.ecam = PciHostTable::new();
        self.mcfg_regions.clear();
        self.roots = PciHostTable::new();
        self.device_mmio_to_virt = None;
        self.roots_published = false;
        self.scan_started = false;
    }

    fn clear_roots(&mut self) {
        self.roots = PciHostTable::new();
        self.roots_published = false;
        self.scan_started = false;
    }
}

static ACPI_PCI_RUNTIME: Spinlock<AcpiPciRuntime> = Spinlock::new(AcpiPciRuntime::new());

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AcpiMcfgInstallSummary {
    pub regions: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AcpiPciRootPublishSummary {
    pub registered_hosts: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AcpiPciError {
    McfgAlreadyInstalled,
    RootsAlreadyPublished,
    RootBridgesNotPublished,
    ScanAlreadyStarted,
    InvalidMcfg {
        index: usize,
    },
    OverlappingMcfg {
        index: usize,
    },
    InvalidRoot {
        index: usize,
    },
    RootNotCoveredByMcfg {
        index: usize,
    },
    OverlappingRoot {
        index: usize,
    },
    InvalidWindow {
        root_index: usize,
        window_index: usize,
    },
    OverlappingWindow {
        root_index: usize,
        window_index: usize,
    },
    OutOfMemory,
    BackendUnavailable,
    HostRegistration {
        index: usize,
        error: PciHostBridgeError,
    },
}

struct PreparedMcfg {
    table: PciHostTable<AcpiEcamRegion>,
    regions: Vec<AcpiEcamRegion>,
}

/// 安装 MCFG 配置空间后端，但不据此虚构 PCI root bridge。
pub(super) fn install_mcfg_backend(
    regions: &[AcpiPciConfigRegion],
    device_mmio_to_virt: fn(usize) -> usize,
) -> Result<AcpiMcfgInstallSummary, AcpiPciError> {
    if regions.is_empty() {
        return Ok(AcpiMcfgInstallSummary::default());
    }
    let prepared = prepare_mcfg(regions, device_mmio_to_virt)?;

    {
        let mut runtime = ACPI_PCI_RUNTIME.lock();
        if !runtime.ecam.is_empty() || runtime.device_mmio_to_virt.is_some() {
            return Err(AcpiPciError::McfgAlreadyInstalled);
        }
        runtime.ecam = prepared.table;
        runtime.mcfg_regions = prepared.regions;
        runtime.device_mmio_to_virt = Some(device_mmio_to_virt);
    }

    let access = PciConfigAccess {
        read_u8: ecam_read_u8,
        read_u16: ecam_read_u16,
        read_u32: ecam_read_u32,
        write_u8: ecam_write_u8,
        write_u16: ecam_write_u16,
        write_u32: ecam_write_u32,
        device_mmio_to_virt: runtime_device_mmio_to_virt,
        resolve_irq: Some(resolve_acpi_pci_irq),
        allocate_msi: None,
    };
    if try_install_pci_access_pair(access, resolve_bar_mapping).is_err() {
        ACPI_PCI_RUNTIME.lock().clear_mcfg();
        return Err(AcpiPciError::BackendUnavailable);
    }

    log::printk!(
        "[kernel-start][acpi] installed {} MCFG ECAM region(s)",
        regions.len()
    );
    Ok(AcpiMcfgInstallSummary {
        regions: regions.len(),
    })
}

/// 将 AML 枚举出的 root bridge 事务性发布到通用 PCI host registry。
pub(super) fn publish_root_bridges(
    roots: &[AcpiPciRootBridge],
) -> Result<AcpiPciRootPublishSummary, AcpiPciError> {
    let mcfg_regions = {
        let runtime = ACPI_PCI_RUNTIME.lock();
        if runtime.ecam.is_empty() || runtime.device_mmio_to_virt.is_none() {
            return Err(AcpiPciError::BackendUnavailable);
        }
        if runtime.roots_published || !runtime.roots.is_empty() {
            return Err(AcpiPciError::RootsAlreadyPublished);
        }
        let mut regions = Vec::new();
        regions
            .try_reserve(runtime.mcfg_regions.len())
            .map_err(|_| AcpiPciError::OutOfMemory)?;
        regions.extend_from_slice(&runtime.mcfg_regions);
        regions
    };
    let prepared = prepare_publishable_roots(roots, &mcfg_regions)?;
    let mut handles = Vec::new();
    handles
        .try_reserve(prepared.hosts.len())
        .map_err(|_| AcpiPciError::OutOfMemory)?;

    {
        let mut runtime = ACPI_PCI_RUNTIME.lock();
        if runtime.roots_published || !runtime.roots.is_empty() {
            return Err(AcpiPciError::RootsAlreadyPublished);
        }
        runtime.roots = prepared.table;
    }

    for (index, info) in prepared.hosts.into_iter().enumerate() {
        match register_host_bridge(info, None) {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                rollback_hosts(&mut handles);
                ACPI_PCI_RUNTIME.lock().clear_roots();
                return Err(AcpiPciError::HostRegistration { index, error });
            }
        }
    }
    ACPI_PCI_RUNTIME.lock().roots_published = true;

    log::printk!(
        "[kernel-start][acpi] published {} AML PCI root bridge(s)",
        handles.len()
    );
    Ok(AcpiPciRootPublishSummary {
        registered_hosts: handles.len(),
    })
}

/// 在 PnP/驱动子系统就绪后扫描全部已经发布的 ACPI PCI root bridge。
pub(super) fn scan_root_bridges() -> Result<PciScanRegisterSummary, AcpiPciError> {
    let roots = {
        let mut runtime = ACPI_PCI_RUNTIME.lock();
        if !runtime.roots_published {
            return Err(AcpiPciError::RootBridgesNotPublished);
        }
        if runtime.scan_started {
            return Err(AcpiPciError::ScanAlreadyStarted);
        }
        let mut roots = Vec::new();
        roots
            .try_reserve(runtime.roots.values().count())
            .map_err(|_| AcpiPciError::OutOfMemory)?;
        roots.extend(
            runtime
                .roots
                .values()
                .map(|root| (root.segment, root.bus_start, root.bus_end)),
        );
        runtime.scan_started = true;
        roots
    };

    let mut total = PciScanRegisterSummary::default();
    for (segment, bus_start, bus_end) in roots {
        let topology = pci_intx_topology_snapshot(segment, bus_start, bus_end);
        {
            let mut runtime = ACPI_PCI_RUNTIME.lock();
            let root = runtime
                .roots
                .get_mut(segment, bus_start)
                .ok_or(AcpiPciError::RootBridgesNotPublished)?;
            if !root.irq_routes.is_empty() && topology.is_none() {
                log::error!(
                    "[kernel-start][acpi] rejected INTx topology for segment={} buses={:#x}..={:#x}",
                    segment,
                    bus_start,
                    bus_end
                );
            }
            root.intx_topology = topology;
        }
        let summary = pci_scan_and_register_summary(segment, bus_start, bus_end);
        total.registered += summary.registered;
        total.bound += summary.bound;
        total.no_driver += summary.no_driver;
        total.deferred += summary.deferred;
        total.failed += summary.failed;
    }
    log::printk!(
        "[kernel-start][acpi] PCI scan registered={} bound={} no-driver={} deferred={} failed={}",
        total.registered,
        total.bound,
        total.no_driver,
        total.deferred,
        total.failed
    );
    Ok(total)
}

fn prepare_mcfg(
    regions: &[AcpiPciConfigRegion],
    device_mmio_to_virt: fn(usize) -> usize,
) -> Result<PreparedMcfg, AcpiPciError> {
    let mut table = PciHostTable::new();
    let mut validated: Vec<AcpiEcamRegion> = Vec::new();
    validated
        .try_reserve(regions.len())
        .map_err(|_| AcpiPciError::OutOfMemory)?;

    for (index, source) in regions.iter().copied().enumerate() {
        let key = PciHostBusRange::new(source.segment, source.bus_start, source.bus_end)
            .ok_or(AcpiPciError::InvalidMcfg { index })?;
        let bus_count = usize::from(source.bus_end - source.bus_start) + 1;
        let required_size = bus_count
            .checked_mul(ECAM_BYTES_PER_BUS)
            .ok_or(AcpiPciError::InvalidMcfg { index })?;
        let expected_start = source
            .segment_base_address
            .checked_add(usize::from(source.bus_start) << 20)
            .ok_or(AcpiPciError::InvalidMcfg { index })?;
        let physical_end = source
            .physical_address
            .checked_add(source.size)
            .ok_or(AcpiPciError::InvalidMcfg { index })?;
        let last_byte = source
            .address(
                source.bus_end,
                PCI_DEVICES_PER_BUS - 1,
                PCI_FUNCTIONS_PER_DEVICE - 1,
                0x0fff,
            )
            .and_then(|address| address.checked_add(1))
            .ok_or(AcpiPciError::InvalidMcfg { index })?;
        if source.segment_base_address == 0
            || !source
                .segment_base_address
                .is_multiple_of(ECAM_BYTES_PER_BUS)
            || source.physical_address != expected_start
            || source.size != required_size
            || last_byte != physical_end
        {
            return Err(AcpiPciError::InvalidMcfg { index });
        }

        let virtual_start = device_mmio_to_virt(source.physical_address);
        if virtual_start == 0
            || !virtual_start.is_multiple_of(core::mem::align_of::<u32>())
            || virtual_start.checked_add(source.size).is_none()
        {
            return Err(AcpiPciError::InvalidMcfg { index });
        }
        let candidate = AcpiEcamRegion {
            segment: source.segment,
            physical_start: source.physical_address,
            virtual_start,
            size: source.size,
            bus_start: source.bus_start,
            bus_end: source.bus_end,
        };
        if validated.iter().copied().any(|existing| {
            ranges_overlap(
                existing.physical_start,
                existing.size,
                candidate.physical_start,
                candidate.size,
            ) || ranges_overlap(
                existing.virtual_start,
                existing.size,
                candidate.virtual_start,
                candidate.size,
            )
        }) {
            return Err(AcpiPciError::OverlappingMcfg { index });
        }
        table.insert(key, candidate).map_err(|error| match error {
            PciHostTableError::Overlap => AcpiPciError::OverlappingMcfg { index },
            PciHostTableError::OutOfMemory => AcpiPciError::OutOfMemory,
        })?;
        validated.push(candidate);
    }
    Ok(PreparedMcfg {
        table,
        regions: validated,
    })
}

struct PreparedRoots {
    table: PciHostTable<AcpiPciRootRuntime>,
    hosts: Vec<PciHostBridgeInfo>,
}

#[cfg(feature = "kernel-tests")]
fn prepare_roots(
    roots: &[AcpiPciRootBridge],
    mcfg_regions: &[AcpiEcamRegion],
) -> Result<PreparedRoots, AcpiPciError> {
    let mut table = PciHostTable::new();
    let mut hosts = Vec::new();
    hosts
        .try_reserve(roots.len())
        .map_err(|_| AcpiPciError::OutOfMemory)?;

    for (index, root) in roots.iter().enumerate() {
        prepare_root(index, root, mcfg_regions, &mut table, &mut hosts)?;
    }
    Ok(PreparedRoots { table, hosts })
}

fn prepare_publishable_roots(
    roots: &[AcpiPciRootBridge],
    mcfg_regions: &[AcpiEcamRegion],
) -> Result<PreparedRoots, AcpiPciError> {
    let mut table = PciHostTable::new();
    let mut hosts = Vec::new();
    hosts
        .try_reserve(roots.len())
        .map_err(|_| AcpiPciError::OutOfMemory)?;
    for (index, root) in roots.iter().enumerate() {
        match prepare_root(index, root, mcfg_regions, &mut table, &mut hosts) {
            Ok(()) => {}
            Err(AcpiPciError::OutOfMemory) => return Err(AcpiPciError::OutOfMemory),
            Err(error) => log::error!(
                "[kernel-start][acpi] rejected PCI root {}: {:?}",
                root.firmware_path,
                error
            ),
        }
    }
    Ok(PreparedRoots { table, hosts })
}

fn prepare_root(
    index: usize,
    root: &AcpiPciRootBridge,
    mcfg_regions: &[AcpiEcamRegion],
    table: &mut PciHostTable<AcpiPciRootRuntime>,
    hosts: &mut Vec<PciHostBridgeInfo>,
) -> Result<(), AcpiPciError> {
    if root.firmware_path.is_empty() || root.bus_start > root.bus_end {
        return Err(AcpiPciError::InvalidRoot { index });
    }
    let key = PciHostBusRange::new(root.segment, root.bus_start, root.bus_end)
        .ok_or(AcpiPciError::InvalidRoot { index })?;
    let config_regions = prepare_host_config_regions(index, root, mcfg_regions)?;
    let windows = prepare_root_windows(index, &root.windows)?;
    let mut runtime_windows = Vec::new();
    runtime_windows
        .try_reserve(windows.len())
        .map_err(|_| AcpiPciError::OutOfMemory)?;
    runtime_windows.extend_from_slice(&windows);
    let mut runtime_irq_routes = Vec::new();
    runtime_irq_routes
        .try_reserve(root.irq_routes.len())
        .map_err(|_| AcpiPciError::OutOfMemory)?;
    runtime_irq_routes.extend_from_slice(&root.irq_routes);
    table
        .insert(
            key,
            AcpiPciRootRuntime {
                segment: root.segment,
                bus_start: root.bus_start,
                bus_end: root.bus_end,
                windows: runtime_windows,
                irq_routes: runtime_irq_routes,
                intx_topology: None,
            },
        )
        .map_err(|error| match error {
            PciHostTableError::Overlap => AcpiPciError::OverlappingRoot { index },
            PciHostTableError::OutOfMemory => AcpiPciError::OutOfMemory,
        })?;
    hosts.push(host_bridge_info(root, config_regions, windows));
    Ok(())
}

fn prepare_host_config_regions(
    index: usize,
    root: &AcpiPciRootBridge,
    regions: &[AcpiEcamRegion],
) -> Result<Vec<PciHostConfigRegion>, AcpiPciError> {
    if !bus_range_is_covered(root.bus_start, root.bus_end, |next_bus| {
        regions
            .iter()
            .filter(|region| {
                region.segment == root.segment
                    && region.bus_start <= next_bus
                    && next_bus <= region.bus_end
            })
            .map(|region| region.bus_end)
            .max()
    }) {
        return Err(AcpiPciError::RootNotCoveredByMcfg { index });
    }

    let region_count = regions
        .iter()
        .filter(|region| {
            region.segment == root.segment
                && region.bus_start <= root.bus_end
                && root.bus_start <= region.bus_end
        })
        .count();
    let mut prepared = Vec::new();
    prepared
        .try_reserve(region_count)
        .map_err(|_| AcpiPciError::OutOfMemory)?;
    for region in regions.iter().copied().filter(|region| {
        region.segment == root.segment
            && region.bus_start <= root.bus_end
            && root.bus_start <= region.bus_end
    }) {
        let bus_start = region.bus_start.max(root.bus_start);
        let bus_end = region.bus_end.min(root.bus_end);
        let physical_start = region
            .physical_start_for(bus_start)
            .ok_or(AcpiPciError::InvalidRoot { index })?;
        let size = (usize::from(bus_end - bus_start) + 1)
            .checked_mul(ECAM_BYTES_PER_BUS)
            .ok_or(AcpiPciError::InvalidRoot { index })?;
        prepared.push(PciHostConfigRegion {
            bus_start,
            bus_end,
            physical_start,
            size,
        });
    }
    prepared.sort_unstable_by_key(|region| region.bus_start);
    Ok(prepared)
}

fn bus_range_is_covered(
    bus_start: u8,
    bus_end: u8,
    mut covering_end: impl FnMut(u8) -> Option<u8>,
) -> bool {
    if bus_start > bus_end {
        return false;
    }
    let mut next_bus = bus_start;
    loop {
        let Some(covered_end) = covering_end(next_bus) else {
            return false;
        };
        if covered_end >= bus_end {
            return true;
        }
        let Some(next) = covered_end.checked_add(1) else {
            return false;
        };
        next_bus = next;
    }
}

fn prepare_root_windows(
    root_index: usize,
    windows: &[AcpiPciRootWindow],
) -> Result<Vec<AcpiPciRootWindow>, AcpiPciError> {
    let mut validated: Vec<AcpiPciRootWindow> = Vec::new();
    validated
        .try_reserve(windows.len())
        .map_err(|_| AcpiPciError::OutOfMemory)?;
    for (window_index, window) in windows.iter().copied().enumerate() {
        let size_u64 = u64::try_from(window.size).map_err(|_| AcpiPciError::InvalidWindow {
            root_index,
            window_index,
        })?;
        if window.size == 0
            || window.pci_start.checked_add(size_u64).is_none()
            || window.cpu_start.checked_add(window.size).is_none()
        {
            return Err(AcpiPciError::InvalidWindow {
                root_index,
                window_index,
            });
        }
        if validated.iter().copied().any(|existing| {
            let existing_size =
                u64::try_from(existing.size).expect("previous ACPI PCI window size was validated");
            windows_share_space(existing.space, window.space)
                && (u64_ranges_overlap(
                    existing.pci_start,
                    existing_size,
                    window.pci_start,
                    size_u64,
                ) || ranges_overlap(
                    existing.cpu_start,
                    existing.size,
                    window.cpu_start,
                    window.size,
                ))
        }) {
            return Err(AcpiPciError::OverlappingWindow {
                root_index,
                window_index,
            });
        }
        validated.push(window);
    }
    Ok(validated)
}

fn host_bridge_info(
    root: &AcpiPciRootBridge,
    config_regions: Vec<PciHostConfigRegion>,
    windows: Vec<AcpiPciRootWindow>,
) -> PciHostBridgeInfo {
    PciHostBridgeInfo {
        name: format!("ACPI PCI root {}", root.firmware_path).into_boxed_str(),
        firmware_path: Some(root.firmware_path.clone()),
        numa_node_id: root.numa_node_id,
        domain: root.segment,
        bus_start: root.bus_start,
        bus_end: root.bus_end,
        config_regions,
        config_space: PciConfigSpaceKind::Ecam,
        dma_coherent: root.dma_coherent,
        dma: if root.identity_dma {
            PciHostDmaInfo {
                windows: Some(Vec::new()),
                ..PciHostDmaInfo::default()
            }
        } else {
            PciHostDmaInfo {
                unsupported: true,
                ..PciHostDmaInfo::default()
            }
        },
        firmware_functions: Vec::new(),
        windows: windows.into_iter().map(general_host_window).collect(),
        irq_route_count: root.irq_routes.len(),
        msi_route_count: 0,
    }
}

fn general_host_window(window: AcpiPciRootWindow) -> PciHostBridgeWindow {
    PciHostBridgeWindow {
        space: match window.space {
            AcpiPciWindowSpace::Io => PciHostAddressSpace::Io,
            AcpiPciWindowSpace::Memory => PciHostAddressSpace::Memory,
            AcpiPciWindowSpace::PrefetchableMemory => PciHostAddressSpace::PrefetchableMemory,
        },
        pci_start: window.pci_start,
        cpu_start: window.cpu_start,
        size: window.size,
    }
}

fn rollback_hosts(handles: &mut Vec<PciHostBridgeHandle>) {
    while let Some(handle) = handles.pop() {
        if let Err(error) = unregister_host_bridge(handle) {
            log::error!(
                "[kernel-start][acpi] failed to roll back PCI host handle {}: {:?}",
                handle.id(),
                error
            );
        }
    }
}

fn windows_share_space(left: AcpiPciWindowSpace, right: AcpiPciWindowSpace) -> bool {
    matches!(
        (left, right),
        (AcpiPciWindowSpace::Io, AcpiPciWindowSpace::Io)
            | (
                AcpiPciWindowSpace::Memory | AcpiPciWindowSpace::PrefetchableMemory,
                AcpiPciWindowSpace::Memory | AcpiPciWindowSpace::PrefetchableMemory
            )
    )
}

fn ranges_overlap(
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

fn u64_ranges_overlap(left_start: u64, left_size: u64, right_start: u64, right_size: u64) -> bool {
    let Some(left_end) = left_start.checked_add(left_size) else {
        return true;
    };
    let Some(right_end) = right_start.checked_add(right_size) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

fn ecam_address(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    width: usize,
) -> Result<usize, PciConfigError> {
    let runtime = ACPI_PCI_RUNTIME.lock();
    if runtime.ecam.is_empty() {
        return Err(PciConfigError::Uninitialized);
    }
    runtime
        .ecam
        .get(segment, bus)
        .copied()
        .ok_or(PciConfigError::InvalidDevice)?
        .address(bus, device, function, offset, width)
}

fn ecam_read_u8(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> Result<u8, PciConfigError> {
    let address = ecam_address(segment, bus, device, function, offset, 1)?;
    // Safety: `ecam_address` validated the mapped window and u8 access range.
    Ok(unsafe { core::ptr::read_volatile(address as *const u8) })
}

fn ecam_read_u16(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> Result<u16, PciConfigError> {
    let address = ecam_address(segment, bus, device, function, offset, 2)?;
    // Safety: `ecam_address` validated the mapped window and u16 alignment/range.
    Ok(u16::from_le(unsafe {
        core::ptr::read_volatile(address as *const u16)
    }))
}

fn ecam_read_u32(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> Result<u32, PciConfigError> {
    let address = ecam_address(segment, bus, device, function, offset, 4)?;
    // Safety: `ecam_address` validated the mapped window and u32 alignment/range.
    Ok(u32::from_le(unsafe {
        core::ptr::read_volatile(address as *const u32)
    }))
}

fn ecam_write_u8(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    value: u8,
) -> Result<(), PciConfigError> {
    let address = ecam_address(segment, bus, device, function, offset, 1)?;
    // Safety: `ecam_address` validated the mapped window and u8 access range.
    unsafe { core::ptr::write_volatile(address as *mut u8, value) };
    Ok(())
}

fn ecam_write_u16(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    value: u16,
) -> Result<(), PciConfigError> {
    let address = ecam_address(segment, bus, device, function, offset, 2)?;
    // Safety: `ecam_address` validated the mapped window and u16 alignment/range.
    unsafe { core::ptr::write_volatile(address as *mut u16, value.to_le()) };
    Ok(())
}

fn ecam_write_u32(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    value: u32,
) -> Result<(), PciConfigError> {
    let address = ecam_address(segment, bus, device, function, offset, 4)?;
    // Safety: `ecam_address` validated the mapped window and u32 alignment/range.
    unsafe { core::ptr::write_volatile(address as *mut u32, value.to_le()) };
    Ok(())
}

fn runtime_device_mmio_to_virt(physical_address: usize) -> usize {
    let mapper = ACPI_PCI_RUNTIME.lock().device_mmio_to_virt;
    mapper.map_or(0, |mapper| mapper(physical_address))
}

fn resolve_acpi_pci_irq(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    interrupt_pin: Option<u8>,
    _interrupt_line: Option<u8>,
) -> Option<PciResolvedIrq> {
    let interrupt_pin = interrupt_pin?;
    let route = {
        let runtime = ACPI_PCI_RUNTIME.lock();
        let root = runtime.roots.get(segment, bus)?;
        let key = root
            .intx_topology
            .as_ref()?
            .resolve(bus, device, function, interrupt_pin)?;
        *root.irq_routes.iter().find(|route| {
            route.device == key.device
                && route
                    .function
                    .is_none_or(|function| function == key.function)
                && route.pin.checked_add(1) == Some(key.pin)
        })?
    };
    Some(PciResolvedIrq {
        line: irq::translate_firmware_irq(None, &[route.gsi])?,
        trigger: Some(route.trigger),
        polarity: Some(route.polarity),
        sharing: route.sharing,
    })
}

fn resolve_bar_mapping(
    segment: u16,
    bus: u8,
    _device: u8,
    _function: u8,
    bar_type: PciBarType,
    prefetchable: bool,
    pci_address: u64,
    size: u64,
) -> Option<PciBarMapping> {
    if pci_address == 0 || !size.is_power_of_two() || !pci_address.is_multiple_of(size) {
        return None;
    }
    let (window, mapper) = {
        let runtime = ACPI_PCI_RUNTIME.lock();
        let root = runtime.roots.get(segment, bus)?;
        let preferred_prefetch = bar_type == PciBarType::Memory && prefetchable;
        let window = root
            .windows
            .iter()
            .copied()
            .filter(|window| window_accepts(*window, bar_type, prefetchable))
            .filter(|window| window_cpu_address(*window, pci_address, size).is_some())
            .min_by_key(|window| {
                usize::from(
                    preferred_prefetch && window.space != AcpiPciWindowSpace::PrefetchableMemory,
                )
            })?;
        (window, runtime.device_mmio_to_virt?)
    };
    let cpu_phys = window_cpu_address(window, pci_address, size)?;
    let virt_addr = match bar_type {
        PciBarType::Io => cpu_phys,
        PciBarType::Memory => mapper(cpu_phys),
    };
    (virt_addr != 0 || matches!(bar_type, PciBarType::Io)).then_some(PciBarMapping {
        cpu_phys,
        virt_addr,
    })
}

fn window_accepts(window: AcpiPciRootWindow, bar_type: PciBarType, prefetchable: bool) -> bool {
    match (window.space, bar_type, prefetchable) {
        (AcpiPciWindowSpace::Io, PciBarType::Io, _) => true,
        (AcpiPciWindowSpace::Memory, PciBarType::Memory, _) => true,
        (AcpiPciWindowSpace::PrefetchableMemory, PciBarType::Memory, true) => true,
        _ => false,
    }
}

fn window_cpu_address(window: AcpiPciRootWindow, pci_address: u64, size: u64) -> Option<usize> {
    let window_size = u64::try_from(window.size).ok()?;
    let window_end = window.pci_start.checked_add(window_size)?;
    let end = pci_address.checked_add(size)?;
    if pci_address < window.pci_start || end > window_end {
        return None;
    }
    window
        .cpu_start
        .checked_add(usize::try_from(pci_address - window.pci_start).ok()?)
}

#[cfg(feature = "kernel-tests")]
mod tests {
    use alloc::vec;

    use ktest::ktest;

    use super::*;

    fn identity(address: usize) -> usize {
        address
    }

    fn offset_mapping(address: usize) -> usize {
        address + 0x1000_0000
    }

    fn region(segment: u16, bus_start: u8, bus_end: u8, base: usize) -> AcpiPciConfigRegion {
        let size = (usize::from(bus_end - bus_start) + 1) * ECAM_BYTES_PER_BUS;
        AcpiPciConfigRegion {
            segment,
            bus_start,
            bus_end,
            segment_base_address: base,
            physical_address: base + (usize::from(bus_start) << 20),
            size,
        }
    }

    fn root(segment: u16, bus_start: u8, bus_end: u8) -> AcpiPciRootBridge {
        AcpiPciRootBridge {
            firmware_path: "\\_SB.PCI0".into(),
            segment,
            bus_start,
            bus_end,
            numa_node_id: Some(1),
            windows: vec![AcpiPciRootWindow {
                space: AcpiPciWindowSpace::Memory,
                pci_start: 0x4000_0000,
                cpu_start: 0x8000_0000,
                size: 0x1000_0000,
            }],
            dma_coherent: true,
            identity_dma: true,
            irq_routes: vec![AcpiPciIrqRoute {
                device: 3,
                function: None,
                pin: 0,
                gsi: 32,
                trigger: IrqTrigger::Level,
                polarity: IrqPolarity::Low,
                sharing: IrqSharing::Shared,
            }],
        }
    }

    #[ktest]
    fn prepares_multiple_segments_and_nonzero_bus_ranges() {
        let sources = [
            region(0, 0, 0x1f, 0x8000_0000),
            region(2, 0x20, 0x2f, 0xa000_0000),
        ];
        let prepared = prepare_mcfg(&sources, offset_mapping).expect("valid MCFG regions");
        let first = prepared.table.get(0, 0x10).expect("segment zero");
        let second = prepared.table.get(2, 0x21).expect("segment two");
        assert_eq!(first.physical_start, 0x8000_0000);
        assert_eq!(second.address(0x21, 3, 2, 0x120, 4), Ok(0xb211_a120));
        assert!(prepared.table.get(1, 0x21).is_none());
    }

    #[ktest]
    fn rejects_malformed_and_aliased_mcfg_regions() {
        let mut malformed = region(0, 0x20, 0x2f, 0x8000_0000);
        malformed.physical_address += ECAM_BYTES_PER_BUS;
        assert_eq!(
            prepare_mcfg(&[malformed], identity).err(),
            Some(AcpiPciError::InvalidMcfg { index: 0 })
        );

        let aliased = [
            region(0, 0, 0x0f, 0x8000_0000),
            region(1, 0, 0x0f, 0x8000_0000),
        ];
        assert_eq!(
            prepare_mcfg(&aliased, identity).err(),
            Some(AcpiPciError::OverlappingMcfg { index: 1 })
        );
    }

    #[ktest]
    fn root_requires_covering_mcfg_allocation() {
        let mcfg =
            prepare_mcfg(&[region(0, 0x20, 0x2f, 0x8000_0000)], identity).expect("valid MCFG");
        let valid = root(0, 0x20, 0x27);
        let prepared = prepare_roots(&[valid], &mcfg.regions).expect("covered root");
        let info = &prepared.hosts[0];
        assert_eq!(
            info.config_regions,
            vec![PciHostConfigRegion {
                bus_start: 0x20,
                bus_end: 0x27,
                physical_start: 0x8200_0000,
                size: 8 * ECAM_BYTES_PER_BUS,
            }]
        );
        assert!(info.dma_coherent);
        assert!(!info.dma.unsupported);
        assert_eq!(info.dma.windows, Some(Vec::new()));
        assert_eq!(info.irq_route_count, 1);

        assert_eq!(
            prepare_roots(&[root(0, 0x20, 0x30)], &mcfg.regions).err(),
            Some(AcpiPciError::RootNotCoveredByMcfg { index: 0 })
        );
        assert_eq!(
            prepare_roots(&[root(1, 0x20, 0x27)], &mcfg.regions).err(),
            Some(AcpiPciError::RootNotCoveredByMcfg { index: 0 })
        );

        let split = prepare_mcfg(
            &[
                region(0, 0x20, 0x27, 0x8000_0000),
                region(0, 0x28, 0x2f, 0x9000_0000),
            ],
            identity,
        )
        .expect("valid split MCFG");
        let prepared =
            prepare_roots(&[root(0, 0x20, 0x2f)], &split.regions).expect("union coverage");
        assert_eq!(
            prepared.hosts[0].config_regions,
            vec![
                PciHostConfigRegion {
                    bus_start: 0x20,
                    bus_end: 0x27,
                    physical_start: 0x8200_0000,
                    size: 8 * ECAM_BYTES_PER_BUS,
                },
                PciHostConfigRegion {
                    bus_start: 0x28,
                    bus_end: 0x2f,
                    physical_start: 0x9280_0000,
                    size: 8 * ECAM_BYTES_PER_BUS,
                },
            ]
        );

        let gap = prepare_mcfg(
            &[
                region(0, 0x20, 0x27, 0x8000_0000),
                region(0, 0x29, 0x2f, 0x9000_0000),
            ],
            identity,
        )
        .expect("individually valid MCFG ranges");
        assert_eq!(
            prepare_roots(&[root(0, 0x20, 0x2f)], &gap.regions).err(),
            Some(AcpiPciError::RootNotCoveredByMcfg { index: 0 })
        );
    }

    #[ktest]
    fn invalid_root_does_not_hide_other_publishable_roots() {
        let mcfg = prepare_mcfg(
            &[
                region(0, 0x20, 0x2f, 0x8000_0000),
                region(2, 0x40, 0x4f, 0xa000_0000),
            ],
            identity,
        )
        .expect("valid MCFG");
        let prepared =
            prepare_publishable_roots(&[root(1, 0, 0xff), root(2, 0x40, 0x47)], &mcfg.regions)
                .expect("invalid root is isolated");
        assert_eq!(prepared.hosts.len(), 1);
        assert_eq!(prepared.hosts[0].domain, 2);
        assert!(prepared.table.get(1, 0).is_none());
        assert!(prepared.table.get(2, 0x40).is_some());
    }

    #[ktest]
    fn validates_root_windows_and_bar_translation() {
        let window = AcpiPciRootWindow {
            space: AcpiPciWindowSpace::PrefetchableMemory,
            pci_start: 0x4000_0000,
            cpu_start: 0x9000_0000,
            size: 0x1000_0000,
        };
        assert_eq!(
            window_cpu_address(window, 0x4100_0000, 0x2000),
            Some(0x9100_0000)
        );
        assert!(window_accepts(window, PciBarType::Memory, true));
        assert!(!window_accepts(window, PciBarType::Memory, false));
        assert!(!window_accepts(window, PciBarType::Io, false));

        let overlapping = [
            AcpiPciRootWindow {
                space: AcpiPciWindowSpace::Memory,
                pci_start: 0x1000,
                cpu_start: 0x2000,
                size: 0x1000,
            },
            AcpiPciRootWindow {
                space: AcpiPciWindowSpace::PrefetchableMemory,
                pci_start: 0x1800,
                cpu_start: 0x5000,
                size: 0x1000,
            },
        ];
        assert_eq!(
            prepare_root_windows(3, &overlapping).err(),
            Some(AcpiPciError::OverlappingWindow {
                root_index: 3,
                window_index: 1,
            })
        );
    }

    #[ktest]
    fn validates_bdf_offsets_alignment_and_window_bounds() {
        let source = region(2, 0x20, 0x2f, 0x8000_0000);
        let prepared = prepare_mcfg(&[source], offset_mapping).expect("valid MCFG region");
        let ecam = *prepared.table.get(2, 0x20).expect("published region");
        assert_eq!(ecam.address(0x20, 0, 0, 0, 4), Ok(0x9200_0000));
        assert_eq!(ecam.address(0x2f, 31, 7, 0x0ffc, 4), Ok(0x92ff_fffc));
        assert_eq!(
            ecam.address(0x20, 32, 0, 0, 4),
            Err(PciConfigError::InvalidDevice)
        );
        assert_eq!(
            ecam.address(0x20, 0, 8, 0, 4),
            Err(PciConfigError::InvalidDevice)
        );
        assert_eq!(
            ecam.address(0x20, 0, 0, 2, 4),
            Err(PciConfigError::InvalidOffset)
        );
        assert_eq!(
            ecam.address(0x20, 0, 0, 0x0fff, 2),
            Err(PciConfigError::InvalidOffset)
        );
    }
}
