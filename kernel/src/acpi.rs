//! 基于 ACPI 的内核初始化逻辑。
//!
//! 启动路径先完整枚举并校验 RSDT/XSDT 中的静态表，再初始化内存分配器并只构建一次
//! DSDT/SSDT AML namespace。静态表、AML 解析和 AML Host I/O 能力彼此独立：缺失可选
//! Host 后端不会阻止表清单与静态对象解析，也不会用伪造值继续执行固件方法。

mod static_tables;

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, Ordering};
use core::{mem, slice};

use acpi::address::{AccessSize, AddressSpace, GenericAddress};
use acpi::spcr::{Spcr, SpcrInterfaceType};
use acpi::{AcpiHandler, AcpiTable, PhysicalMapping};
use aml::value::{Args, RegionSpace, StatusObject};
use aml::{AmlContext, AmlError, AmlName, AmlValue, DebugVerbosity, LevelType};

use allocator::KERNEL_ALLOCATOR;
use general::dev::platform::{
    DeviceMatchId, DeviceProperties, DeviceResource, IrqPolarity, IrqResourceAttributes,
    IrqSharing, IrqTrigger, PlatformDeviceInfo, PlatformProbeStatus,
    register_and_probe_platform_device,
};
use general::dev::pnp::DevInitContext;
use general::firmware::power::{
    PowerAccessWidth, PowerControlInfo, PowerControlMethod, PowerRegister, PowerRegisterSpace,
};
use general::firmware::{FirmwareTableMapping, SerialPortInfo};
use general::vfs::FS_REGISTRY;
use general::vfs::VfsContext;
use general::vfs::cred::Credentials;
use general::vfs::dentry::VfsRoot;
use general::vfs::limits::VfsLimits;
use general::vfs::mount::{Mount, MountFlags, MountNamespace};
use general::vfs::stat::FileMode;
use general::{StartAcpiHostOps, StartContext, StartFirmware};
use log::printk;

const ACPI_HID_PNP0500: &str = "PNP0500";
const ACPI_HID_PNP0501: &str = "PNP0501";
const ACPI_HID_VIRTIO_MMIO: &str = "LNRO0005";
const AML_HOST_FAULT_INVALID_MMIO: u32 = 1 << 0;
const AML_HOST_FAULT_SYSTEM_IO: u32 = 1 << 1;
const AML_HOST_FAULT_PCI_CONFIG: u32 = 1 << 2;

static AML_HOST_FAULTS: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy)]
struct AcpiMapper {
    phys_to_virt: fn(usize) -> usize,
    device_mmio_to_virt: fn(usize) -> usize,
    copied_tables: &'static [FirmwareTableMapping],
    host_ops: StartAcpiHostOps,
}

#[derive(Clone, Copy, Debug, Default)]
struct AmlRegionRequirements {
    system_io: usize,
    pci_config: usize,
    pci_config_unavailable: usize,
    unsupported: usize,
}

impl AmlRegionRequirements {
    fn requires_unavailable_host_ops(self, mapper: AcpiMapper) -> bool {
        (self.system_io != 0 && mapper.host_ops.io.is_none())
            || self.pci_config_unavailable != 0
            || self.unsupported != 0
    }
}

struct AmlRuntime {
    context: AmlContext,
    methods_enabled: bool,
}

impl AcpiMapper {
    const fn new(
        phys_to_virt: fn(usize) -> usize,
        device_mmio_to_virt: fn(usize) -> usize,
        copied_tables: &'static [FirmwareTableMapping],
        host_ops: StartAcpiHostOps,
    ) -> Self {
        Self {
            phys_to_virt,
            device_mmio_to_virt,
            copied_tables,
            host_ops,
        }
    }

    #[inline]
    fn resolve_table(&self, physical_address: usize, size: usize) -> usize {
        for mapping in self.copied_tables {
            if let Some(virtual_address) = mapping.resolve(physical_address, size) {
                return virtual_address;
            }
        }
        (self.phys_to_virt)(physical_address)
    }

    #[inline]
    fn resolve_mmio<T>(&self, physical_address: usize) -> Option<usize> {
        let size = mem::size_of::<T>();
        if physical_address.checked_add(size).is_none()
            || !physical_address.is_multiple_of(mem::align_of::<T>())
        {
            record_aml_host_fault(AML_HOST_FAULT_INVALID_MMIO);
            return None;
        }
        let virtual_address = (self.device_mmio_to_virt)(physical_address);
        if virtual_address == 0 || !virtual_address.is_multiple_of(mem::align_of::<T>()) {
            record_aml_host_fault(AML_HOST_FAULT_INVALID_MMIO);
            return None;
        }
        Some(virtual_address)
    }

    #[inline]
    fn read_mmio<T: Copy>(&self, physical_address: usize) -> Option<T> {
        let virtual_address = self.resolve_mmio::<T>(physical_address)?;
        Some(unsafe { ptr::read_volatile(virtual_address as *const T) })
    }

    #[inline]
    fn write_mmio<T>(&self, physical_address: usize, value: T) -> bool {
        let Some(virtual_address) = self.resolve_mmio::<T>(physical_address) else {
            return false;
        };
        unsafe { ptr::write_volatile(virtual_address as *mut T, value) };
        true
    }

    fn pci_backend_available(self) -> bool {
        self.host_ops.pci.is_some() || self.mcfg_entries().next().is_some()
    }

    fn mcfg_entries(self) -> impl Iterator<Item = &'static [u8]> {
        self.copied_tables.iter().flat_map(|mapping| {
            let bytes: &'static [u8] = if mapping.length >= 44 {
                // SAFETY: firmware mappings refer to immutable loader-owned snapshots that
                // outlive StartContext and the ACPI interpreter.
                unsafe { slice::from_raw_parts(mapping.virtual_start as *const u8, mapping.length) }
            } else {
                &[]
            };
            let table_len = read_u32_le(bytes, 4).map(|length| length as usize);
            let valid = bytes.get(..4) == Some(b"MCFG")
                && table_len.is_some_and(|length| (44..=bytes.len()).contains(&length))
                && bytes
                    .get(36..44)
                    .is_some_and(|reserved| reserved.iter().all(|byte| *byte == 0))
                && table_len.is_some_and(|length| (length - 44).is_multiple_of(16))
                && table_len.is_some_and(|length| {
                    bytes[..length]
                        .iter()
                        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
                        == 0
                });
            let payload = if valid {
                &bytes[44..table_len.unwrap_or(44)]
            } else {
                &[]
            };
            payload.chunks_exact(16).filter(|entry| {
                read_uint_le(entry, 0, 8).is_some_and(|base| {
                    base != 0 && base & ((1 << 20) - 1) == 0 && usize::try_from(base).is_ok()
                }) && entry
                    .get(10)
                    .zip(entry.get(11))
                    .is_some_and(|(start, end)| start <= end)
                    && entry
                        .get(12..16)
                        .is_some_and(|reserved| reserved.iter().all(|byte| *byte == 0))
            })
        })
    }

    fn pci_segment_bus_available(self, segment: u16, bus: u8) -> bool {
        self.pci_ecam_address(segment, bus, 0, 0, 0).is_some()
    }

    fn pci_ecam_address(
        self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
    ) -> Option<usize> {
        if device >= 32 || function >= 8 || offset >= 4096 {
            return None;
        }
        for entry in self.mcfg_entries() {
            let base = read_uint_le(entry, 0, 8)?;
            let entry_segment = read_uint_le(entry, 8, 2)? as u16;
            let bus_start = *entry.get(10)?;
            let bus_end = *entry.get(11)?;
            if base == 0
                || base & ((1 << 20) - 1) != 0
                || bus_start > bus_end
                || entry_segment != segment
                || !(bus_start..=bus_end).contains(&bus)
            {
                continue;
            }
            let address = base
                .checked_add(u64::from(bus - bus_start) << 20)?
                .checked_add(u64::from(device) << 15)?
                .checked_add(u64::from(function) << 12)?
                .checked_add(u64::from(offset))?;
            return usize::try_from(address).ok();
        }
        None
    }

    fn pci_ecam_range_available(
        self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        width: u16,
    ) -> bool {
        matches!(width, 1 | 2 | 4)
            && offset.is_multiple_of(width)
            && offset.checked_add(width).is_some_and(|end| end <= 4096)
            && self
                .pci_ecam_address(segment, bus, device, function, offset)
                .is_some()
    }

    fn read_pci_ecam_u8(
        self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
    ) -> Option<u8> {
        self.read_mmio(self.pci_ecam_address(segment, bus, device, function, offset)?)
    }

    fn write_pci_ecam_u8(
        self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u8,
    ) -> bool {
        let Some(address) = self.pci_ecam_address(segment, bus, device, function, offset) else {
            return false;
        };
        self.write_mmio(address, value)
    }
}

impl AcpiHandler for AcpiMapper {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let virtual_address = self.resolve_table(physical_address, size);
        let virtual_start = NonNull::new(virtual_address as *mut T)
            .expect("[kernel-start][acpi] null ACPI mapping");
        unsafe { PhysicalMapping::new(physical_address, virtual_start, size, size, *self) }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}
}

impl aml::Handler for AcpiMapper {
    fn read_u8(&self, address: usize) -> u8 {
        self.read_mmio(address).unwrap_or(u8::MAX)
    }

    fn read_u16(&self, address: usize) -> u16 {
        self.read_mmio(address).unwrap_or(u16::MAX)
    }

    fn read_u32(&self, address: usize) -> u32 {
        self.read_mmio(address).unwrap_or(u32::MAX)
    }

    fn read_u64(&self, address: usize) -> u64 {
        self.read_mmio(address).unwrap_or(u64::MAX)
    }

    fn write_u8(&mut self, address: usize, value: u8) {
        let _ = self.write_mmio(address, value);
    }

    fn write_u16(&mut self, address: usize, value: u16) {
        let _ = self.write_mmio(address, value);
    }

    fn write_u32(&mut self, address: usize, value: u32) {
        let _ = self.write_mmio(address, value);
    }

    fn write_u64(&mut self, address: usize, value: u64) {
        let _ = self.write_mmio(address, value);
    }

    fn read_io_u8(&self, _port: u16) -> u8 {
        match self.host_ops.io {
            Some(ops) => (ops.read_u8)(_port),
            None => {
                record_aml_host_fault(AML_HOST_FAULT_SYSTEM_IO);
                u8::MAX
            }
        }
    }

    fn read_io_u16(&self, _port: u16) -> u16 {
        match self.host_ops.io {
            Some(ops) => (ops.read_u16)(_port),
            None => {
                record_aml_host_fault(AML_HOST_FAULT_SYSTEM_IO);
                u16::MAX
            }
        }
    }

    fn read_io_u32(&self, _port: u16) -> u32 {
        match self.host_ops.io {
            Some(ops) => (ops.read_u32)(_port),
            None => {
                record_aml_host_fault(AML_HOST_FAULT_SYSTEM_IO);
                u32::MAX
            }
        }
    }

    fn write_io_u8(&self, _port: u16, _value: u8) {
        match self.host_ops.io {
            Some(ops) => (ops.write_u8)(_port, _value),
            None => record_aml_host_fault(AML_HOST_FAULT_SYSTEM_IO),
        }
    }

    fn write_io_u16(&self, _port: u16, _value: u16) {
        match self.host_ops.io {
            Some(ops) => (ops.write_u16)(_port, _value),
            None => record_aml_host_fault(AML_HOST_FAULT_SYSTEM_IO),
        }
    }

    fn write_io_u32(&self, _port: u16, _value: u32) {
        match self.host_ops.io {
            Some(ops) => (ops.write_u32)(_port, _value),
            None => record_aml_host_fault(AML_HOST_FAULT_SYSTEM_IO),
        }
    }

    fn read_pci_u8(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
        self.read_pci_ecam_u8(segment, bus, device, function, offset)
            .or_else(|| {
                self.host_ops
                    .pci
                    .map(|ops| (ops.read_u8)(segment, bus, device, function, offset))
            })
            .unwrap_or_else(|| {
                record_aml_host_fault(AML_HOST_FAULT_PCI_CONFIG);
                u8::MAX
            })
    }

    fn read_pci_u16(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
        if self.pci_ecam_range_available(segment, bus, device, function, offset, 2) {
            return self
                .read_mmio(
                    self.pci_ecam_address(segment, bus, device, function, offset)
                        .expect("validated PCI ECAM address"),
                )
                .unwrap_or(u16::MAX);
        }
        self.host_ops
            .pci
            .map(|ops| (ops.read_u16)(segment, bus, device, function, offset))
            .unwrap_or_else(|| {
                record_aml_host_fault(AML_HOST_FAULT_PCI_CONFIG);
                u16::MAX
            })
    }

    fn read_pci_u32(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        if self.pci_ecam_range_available(segment, bus, device, function, offset, 4) {
            return self
                .read_mmio(
                    self.pci_ecam_address(segment, bus, device, function, offset)
                        .expect("validated PCI ECAM address"),
                )
                .unwrap_or(u32::MAX);
        }
        self.host_ops
            .pci
            .map(|ops| (ops.read_u32)(segment, bus, device, function, offset))
            .unwrap_or_else(|| {
                record_aml_host_fault(AML_HOST_FAULT_PCI_CONFIG);
                u32::MAX
            })
    }

    fn write_pci_u8(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u8,
    ) {
        if self.write_pci_ecam_u8(segment, bus, device, function, offset, value) {
            return;
        }
        match self.host_ops.pci {
            Some(ops) => (ops.write_u8)(segment, bus, device, function, offset, value),
            None => record_aml_host_fault(AML_HOST_FAULT_PCI_CONFIG),
        }
    }

    fn write_pci_u16(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u16,
    ) {
        if self.pci_ecam_range_available(segment, bus, device, function, offset, 2) {
            let _ = self.write_mmio(
                self.pci_ecam_address(segment, bus, device, function, offset)
                    .expect("validated PCI ECAM address"),
                value,
            );
            return;
        }
        match self.host_ops.pci {
            Some(ops) => (ops.write_u16)(segment, bus, device, function, offset, value),
            None => record_aml_host_fault(AML_HOST_FAULT_PCI_CONFIG),
        }
    }

    fn write_pci_u32(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) {
        if self.pci_ecam_range_available(segment, bus, device, function, offset, 4) {
            let _ = self.write_mmio(
                self.pci_ecam_address(segment, bus, device, function, offset)
                    .expect("validated PCI ECAM address"),
                value,
            );
            return;
        }
        match self.host_ops.pci {
            Some(ops) => (ops.write_u32)(segment, bus, device, function, offset, value),
            None => record_aml_host_fault(AML_HOST_FAULT_PCI_CONFIG),
        }
    }
}

#[derive(Clone)]
struct FirmwareSerialDevice {
    port: SerialPortInfo,
    resources: Vec<DeviceResource>,
}

#[derive(Clone)]
struct FirmwareMmioDevice {
    name: &'static str,
    phys_addr: usize,
    resources: Vec<DeviceResource>,
}

pub fn kernel_start_init(context: &StartContext) {
    log::debug!("[kernel-start][acpi] jumped into kernel_start_init()");

    let acpi = match context.firmware {
        StartFirmware::Acpi(acpi) => acpi,
        StartFirmware::Dtb(_) => {
            panic!("[kernel-start][acpi] StartContext firmware does not match ACPI path")
        }
    };

    let mapper = AcpiMapper::new(
        context.address.phys_to_virt,
        context.address.device_mmio_to_virt,
        acpi.mappings,
        acpi.host_ops,
    );
    let tables = unsafe { acpi::AcpiTables::from_rsdp(mapper, acpi.rsdp_phys) }
        .map_err(|_| "[kernel-start][acpi] invalid RSDP/table tree")
        .unwrap_or_else(|err| panic!("{}", err));

    printk!(
        "[kernel-start][acpi] RSDP={:#x} revision={}",
        acpi.rsdp_phys,
        tables.revision(),
    );

    // ── 阶段 1：解析 ACPI 固件表 ───────────────────────────────────────────

    let static_summary = static_tables::inspect(&tables, acpi.mappings);
    let cpu_count = static_summary.cpu_count;
    let serial_device = serial_device_from_spcr(&tables);
    let console_serial_port_phys = serial_device.as_ref().map(|device| device.port.phys_addr);
    let memory_segments = context
        .memory
        .boot_map
        .usable_segments()
        .unwrap_or_else(|| {
            panic!("[kernel-start][acpi] ACPI firmware requires usable boot memory segments")
        });

    // ── 阶段 2：初始化分层内存分配器 ───────────────────────────────────────

    KERNEL_ALLOCATOR
        .bind_address_translation(context.address.phys_to_virt, context.address.virt_to_phys);

    let kernel_image = context.memory.kernel_image;
    let kernel_reserved = [(kernel_image.start, kernel_image.end)];

    KERNEL_ALLOCATOR
        .init_phys(&memory_segments, &kernel_reserved)
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][acpi] failed to init physical allocator: {:?}",
                err
            )
        });

    if let Some(alloc_ops) = context.allocator {
        KERNEL_ALLOCATOR.bind_kernel_heap_ops(
            alloc_ops.kernel_heap_region,
            alloc_ops.tracked_heap_region,
            alloc_ops.map_kernel_heap_range,
            alloc_ops.unmap_kernel_heap_range,
        );
        general::elm_image::register_elm_image_ops(
            alloc_ops.protect_kernel_heap_range,
            alloc_ops.validate_kernel_heap_range,
            alloc_ops.sync_icache,
        );
        (alloc_ops.init_kernel_page_table)();
        KERNEL_ALLOCATOR
            .init_vmem(&kernel_reserved)
            .unwrap_or_else(|err| panic!("[kernel-start][acpi] failed to init vmem: {:?}", err));
        KERNEL_ALLOCATOR.init_kheap();
        KERNEL_ALLOCATOR.init_slab(cpu_count);
        KERNEL_ALLOCATOR.activate_global().unwrap_or_else(|err| {
            panic!(
                "[kernel-start][acpi] failed to activate global allocator: {:?}",
                err
            )
        });
    }

    printk!(
        "[kernel-start][acpi] memory allocator ready: {} RAM segment(s)",
        memory_segments.len()
    );

    // AML interpretation allocates namespace objects. Build and initialize one namespace after
    // the allocator is ready, then reuse it for power and device discovery so that table loading
    // and `_INI` side effects happen exactly once.
    let mut aml_runtime = build_aml_runtime(mapper, &tables);
    let power_controls = parse_power_controls(&tables, aml_runtime.as_mut());
    general::firmware::power::install_with_platform_ops(
        power_controls,
        context.address.device_mmio_to_virt,
        acpi.host_ops.io,
    );

    let cmdline = context
        .boot
        .command_line
        .map(general::cmdline::Cmdline::new);

    // ── 阶段 4：发现并登记 ACPI 设备描述 ──────────────────────────────────

    let mut serial_devices: Vec<FirmwareSerialDevice> = Vec::new();
    let mut virtio_mmio_devices: Vec<FirmwareMmioDevice> = Vec::new();
    discover_acpi_namespace_devices(
        aml_runtime.as_mut(),
        &mut serial_devices,
        &mut virtio_mmio_devices,
    );
    if let Some(device) = serial_device
        && !serial_devices
            .iter()
            .any(|existing| existing.port.phys_addr == device.port.phys_addr)
    {
        serial_devices.push(device);
    }
    let console_serial_port_index = console_serial_port_phys.and_then(|phys| {
        serial_devices
            .iter()
            .position(|device| device.port.phys_addr == phys)
    });

    printk!(
        "[kernel-start][acpi] device discovery complete: {} uart(s), {} virtio block candidate(s)",
        serial_devices.len(),
        virtio_mmio_devices.len()
    );

    // ── 阶段 5：挂载根文件系统并准备 /dev ─────────────────────────────────

    crate::device_init::register_core_filesystems("acpi");

    let root_sb = FS_REGISTRY
        .find("tmpfs")
        .expect("[kernel-start][acpi] tmpfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][acpi] failed to mount tmpfs root");

    let root_mount = Mount::new(
        Arc::clone(&root_sb),
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_sb.root_dentry),
        MountFlags::default(),
        None,
    );

    let mount_ns = MountNamespace::new(1, Arc::clone(&root_mount));

    let cred = Credentials::root();

    let vfs_ctx = VfsContext::new(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        VfsRoot::new(Arc::clone(&root_sb.root_dentry), Arc::clone(&root_mount)),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
        FileMode::new(0),
        VfsLimits::default_arc(),
    );

    let dev_sb = crate::device_init::mount_devtmpfs("acpi");
    crate::device_init::mount_devtmpfs_on_dev("acpi", &vfs_ctx, Arc::clone(&dev_sb));

    crate::device_init::activate_device_subsystem(
        "acpi",
        Arc::clone(&dev_sb),
        DevInitContext::new(context.address.device_mmio_to_virt)
            .with_boot_cpu_id(context.boot.boot_cpu_id)
            .with_realtime_clock(crate::vdso::set_realtime_ns)
            .with_realtime_source_hooks(
                crate::vdso::install_realtime_source,
                crate::vdso::unregister_realtime_source,
            ),
        None,
    );

    let stdout_phys = console_serial_port_phys;
    let mut platform_bound = 0usize;
    for device in &serial_devices {
        let port = device.port;
        let mut ids = Vec::new();
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_PNP0500.into()));
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_PNP0501.into()));
        let info = PlatformDeviceInfo {
            fw_name: port.name.into(),
            fw_path: None,
            fw_parent_path: None,
            ids,
            resources: device.resources.clone(),
            properties: DeviceProperties {
                clock_hz: port.clock_hz,
                baud: port.baud,
                fw_phandle: None,
                fw_interrupt_parent: None,
                interrupt_controller: false,
                fw_address_cells: None,
                fw_size_cells: None,
                fw_parent_address_cells: None,
                fw_parent_size_cells: None,
                stdout: stdout_phys == Some(port.phys_addr),
            },
            fw_properties: Vec::new(),
        };
        if register_platform_device(info, "acpi") {
            platform_bound += 1;
        }
    }
    for device in &virtio_mmio_devices {
        let mut ids = Vec::new();
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_VIRTIO_MMIO.into()));
        let info = PlatformDeviceInfo {
            fw_name: device.name.into(),
            fw_path: None,
            fw_parent_path: None,
            ids,
            resources: device.resources.clone(),
            properties: DeviceProperties::default(),
            fw_properties: Vec::new(),
        };
        if register_platform_device(info, "acpi") {
            platform_bound += 1;
        }
    }
    printk!(
        "[kernel-start][acpi] platform PnP discovery complete: {} candidate(s), {} bound",
        serial_devices.len() + virtio_mmio_devices.len(),
        platform_bound
    );

    // ── 阶段 6：注册控制台并绑定日志输出 ──────────────────────────────────

    crate::device_init::mount_standard_user_api_filesystems("acpi", &vfs_ctx);

    // 把同一套部件交给 sched shim 保管：随后 sched::boot_init 会据此给 init
    // 任务挂上 TASKEXT_VFS_CONTEXT / TASKEXT_VFS_FDTABLE。
    crate::sched::stash_boot_vfs_parts(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
    );

    printk!(
        "[kernel-start][acpi] VFS ready: tmpfs '/' + devtmpfs '/dev' + tmpfs '/dev/shm' + sysfs '/sys'"
    );

    let console_selector = if let Some(name) = cmdline.as_ref().and_then(|cl| {
        cl.find("console")
            .map(|value| value.split_once(',').map_or(value, |(device, _)| device))
    }) {
        printk!(
            "[kernel-start][acpi] console requested by cmdline: {}",
            name
        );
        Some(crate::device_init::BootConsoleSelector::DeviceName(
            String::from(name),
        ))
    } else if let Some(device) = console_serial_port_index.and_then(|i| serial_devices.get(i)) {
        printk!(
            "[kernel-start][acpi] console requested by firmware: {}",
            device.port.name
        );
        Some(crate::device_init::BootConsoleSelector::FirmwareName(
            String::from(device.port.name),
        ))
    } else {
        None
    };
    if let Some(selector) = console_selector {
        let _ = crate::device_init::bind_or_defer_boot_console(
            "acpi",
            &vfs_ctx,
            Arc::clone(&dev_sb),
            selector,
        );
    } else {
        printk!("[kernel-start][acpi] no console selected");
    }

    printk!("[kernel-start][acpi] kernel initialization complete, jumping to main entry");
}

fn register_platform_device(info: PlatformDeviceInfo, tag: &str) -> bool {
    match register_and_probe_platform_device(info) {
        Ok(reg) => match reg.status {
            PlatformProbeStatus::Bound => true,
            PlatformProbeStatus::NoDriver => {
                printk!(
                    "[kernel-start][{}] no platform driver for {}",
                    tag,
                    reg.device.id
                );
                false
            }
            PlatformProbeStatus::Deferred => {
                log::debug!(
                    "[kernel-start][{}] deferred platform probe for {}",
                    tag,
                    reg.device.id
                );
                false
            }
        },
        Err(err) => {
            printk!(
                "[kernel-start][{}] failed to register/probe platform device: {:?}",
                tag,
                err
            );
            false
        }
    }
}

fn record_aml_host_fault(fault: u32) {
    AML_HOST_FAULTS.fetch_or(fault, Ordering::Relaxed);
}

fn begin_aml_host_access() {
    AML_HOST_FAULTS.store(0, Ordering::Release);
}

fn finish_aml_host_access(stage: &str) -> bool {
    let faults = AML_HOST_FAULTS.swap(0, Ordering::AcqRel);
    if faults == 0 {
        return true;
    }
    printk!(
        "[kernel-start][acpi] rejected AML {} after host-access failure: invalid-mmio={} \
         system-io={} pci-config={}",
        stage,
        usize::from(faults & AML_HOST_FAULT_INVALID_MMIO != 0),
        usize::from(faults & AML_HOST_FAULT_SYSTEM_IO != 0),
        usize::from(faults & AML_HOST_FAULT_PCI_CONFIG != 0),
    );
    false
}

fn discover_acpi_namespace_devices(
    runtime: Option<&mut AmlRuntime>,
    serial_devices: &mut Vec<FirmwareSerialDevice>,
    virtio_mmio_devices: &mut Vec<FirmwareMmioDevice>,
) {
    let Some(runtime) = runtime else {
        return;
    };

    let mut paths = Vec::new();
    let mut namespace = runtime.context.namespace.clone();
    if let Err(err) = namespace.traverse(|path, level| {
        if level.typ == LevelType::Device {
            paths.push(path.clone());
        }
        Ok(true)
    }) {
        printk!(
            "[kernel-start][acpi] failed to traverse ACPI namespace: {:?}",
            err
        );
        return;
    }

    for path in paths {
        let path_string = path.as_string();
        if !acpi_device_is_usable(runtime, &path) {
            continue;
        }
        let ids = acpi_device_ids(runtime, &path);
        if ids.is_empty() {
            continue;
        }

        let is_serial = ids
            .iter()
            .any(|id| id == ACPI_HID_PNP0500 || id == ACPI_HID_PNP0501);
        let is_virtio_mmio = ids.iter().any(|id| id == ACPI_HID_VIRTIO_MMIO);
        if !is_serial && !is_virtio_mmio {
            continue;
        }

        let resources = acpi_device_resources(runtime, &path);
        let Some((phys_addr, size)) =
            first_mmio_resource(&resources).filter(|&(_, size)| size != 0)
        else {
            printk!(
                "[kernel-start][acpi] ACPI device {} has supported id but no MMIO resource",
                path_string
            );
            continue;
        };

        let name = alloc::format!("{}", path).leak();
        if is_serial {
            if !serial_devices
                .iter()
                .any(|device| device.port.phys_addr == phys_addr)
            {
                printk!(
                    "[kernel-start][acpi] namespace serial: {} phys={:#x} size={:#x}",
                    name,
                    phys_addr,
                    size
                );
                serial_devices.push(FirmwareSerialDevice {
                    port: SerialPortInfo {
                        name,
                        phys_addr,
                        reg_size: Some(size),
                        clock_hz: None,
                        baud: None,
                    },
                    resources,
                });
            }
        } else if !virtio_mmio_devices
            .iter()
            .any(|dev| dev.phys_addr == phys_addr)
        {
            printk!(
                "[kernel-start][acpi] namespace virtio-mmio: {} phys={:#x} size={:#x}",
                name,
                phys_addr,
                size
            );
            virtio_mmio_devices.push(FirmwareMmioDevice {
                name,
                phys_addr,
                resources,
            });
        }
    }
}

fn build_aml_runtime(
    mapper: AcpiMapper,
    tables: &acpi::AcpiTables<AcpiMapper>,
) -> Option<AmlRuntime> {
    let dsdt = match tables.dsdt() {
        Ok(dsdt) => dsdt,
        Err(err) => {
            printk!("[kernel-start][acpi] DSDT unavailable: {:?}", err);
            return None;
        }
    };

    let mut context = AmlContext::new(Box::new(mapper), DebugVerbosity::None);
    if let Err(err) = load_aml_table(&mut context, mapper, &dsdt) {
        printk!("[kernel-start][acpi] failed to load DSDT: {:?}", err);
        return None;
    }

    let mut loaded_ssdts = 0usize;
    let mut failed_ssdts = 0usize;
    for ssdt in tables.ssdts() {
        if let Err(err) = load_aml_table(&mut context, mapper, &ssdt) {
            failed_ssdts += 1;
            printk!(
                "[kernel-start][acpi] failed to load SSDT AML at {:#x}: {:?}",
                ssdt.address,
                err
            );
        } else {
            loaded_ssdts += 1;
        }
    }

    printk!(
        "[kernel-start][acpi] AML namespace loaded: DSDT=1 SSDT={} failed={}",
        loaded_ssdts,
        failed_ssdts
    );

    let requirements = match aml_region_requirements(&context, mapper) {
        Ok(requirements) => requirements,
        Err(err) => {
            printk!(
                "[kernel-start][acpi] failed to inspect AML OperationRegion capabilities: {:?}",
                err
            );
            return Some(AmlRuntime {
                context,
                methods_enabled: false,
            });
        }
    };

    // The external `aml` crate exposes infallible host callbacks. Returning a made-up zero on an
    // unavailable bus would let firmware methods make unsafe decisions, while panicking would turn
    // one optional device into an unexplained boot failure. Fail closed before invoking methods and
    // continue to expose static Name objects from the already-parsed namespace.
    let unavailable_host_ops = requirements.requires_unavailable_host_ops(mapper);
    let mut methods_enabled = failed_ssdts == 0 && !unavailable_host_ops;
    if unavailable_host_ops {
        printk!(
            "[kernel-start][acpi] AML dynamic methods disabled: unavailable OperationRegion \
             backends SystemIO={} PCIConfig={}/{} other={}; static namespace objects remain usable",
            if mapper.host_ops.io.is_some() {
                0
            } else {
                requirements.system_io
            },
            requirements.pci_config_unavailable,
            requirements.pci_config,
            requirements.unsupported
        );
    } else if failed_ssdts != 0 {
        printk!(
            "[kernel-start][acpi] AML dynamic methods disabled because the namespace is incomplete"
        );
    }

    if methods_enabled {
        begin_aml_host_access();
        let initialize_result = context.initialize_objects();
        let host_access_valid = finish_aml_host_access("_INI initialization");
        if !host_access_valid {
            methods_enabled = false;
        } else if let Err(err) = initialize_result {
            methods_enabled = false;
            printk!(
                "[kernel-start][acpi] AML _INI initialization failed; subsequent dynamic method \
                 evaluation is disabled: {:?}",
                err
            );
        } else {
            printk!("[kernel-start][acpi] AML _INI initialization complete");
        }
    }

    Some(AmlRuntime {
        context,
        methods_enabled,
    })
}

fn aml_region_requirements(
    context: &AmlContext,
    mapper: AcpiMapper,
) -> Result<AmlRegionRequirements, AmlError> {
    let mut requirements = AmlRegionRequirements::default();
    let mut namespace = context.namespace.clone();
    namespace.traverse(|_, level| {
        for handle in level.values.values() {
            let AmlValue::OpRegion {
                region,
                parent_device,
                ..
            } = context.namespace.get(*handle)?
            else {
                continue;
            };
            match region {
                RegionSpace::SystemMemory => {}
                RegionSpace::SystemIo => requirements.system_io += 1,
                RegionSpace::PciConfig => {
                    requirements.pci_config += 1;
                    if mapper.host_ops.pci.is_none()
                        && !parent_device.as_ref().is_some_and(|parent| {
                            aml_pci_region_covered_by_mcfg(context, mapper, parent)
                        })
                    {
                        requirements.pci_config_unavailable += 1;
                    }
                }
                _ => requirements.unsupported += 1,
            }
        }
        Ok(true)
    })?;
    Ok(requirements)
}

fn aml_pci_region_covered_by_mcfg(
    context: &AmlContext,
    mapper: AcpiMapper,
    parent_device: &AmlName,
) -> bool {
    let Some(segment) = aml_static_integer(context, parent_device, "_SEG", Some(0))
        .and_then(|value| u16::try_from(value).ok())
    else {
        return false;
    };
    let Some(bus) = aml_static_integer(context, parent_device, "_BBN", Some(0))
        .and_then(|value| u8::try_from(value).ok())
    else {
        return false;
    };
    let Some(address) = aml_static_integer(context, parent_device, "_ADR", None) else {
        return false;
    };
    let device = (address >> 16) & 0xffff;
    let function = address & 0xffff;
    device < 32 && function < 8 && mapper.pci_segment_bus_available(segment, bus)
}

fn aml_static_integer(
    context: &AmlContext,
    scope: &AmlName,
    name: &str,
    missing: Option<u64>,
) -> Option<u64> {
    let name = AmlName::from_str(name).ok()?;
    match context.namespace.search(&name, scope) {
        Ok((_, handle)) => match context.namespace.get(handle).ok()? {
            AmlValue::Integer(value) => Some(*value),
            AmlValue::Boolean(value) => Some(if *value { u64::MAX } else { 0 }),
            _ => None,
        },
        Err(err) if aml_error_is_missing(&err) => missing,
        Err(_) => None,
    }
}

fn load_aml_table(
    context: &mut AmlContext,
    mapper: AcpiMapper,
    table: &acpi::AmlTable,
) -> Result<(), AmlError> {
    let length = table.length as usize;
    if length == 0 {
        return Ok(());
    }
    let aml_virt = mapper.resolve_table(table.address, length);
    let aml = unsafe { slice::from_raw_parts(aml_virt as *const u8, length) };
    begin_aml_host_access();
    let result = context.parse_table(aml);
    if !finish_aml_host_access("table parse") {
        return Err(AmlError::Unimplemented);
    }
    result
}

fn acpi_device_is_usable(runtime: &mut AmlRuntime, path: &AmlName) -> bool {
    let status = match evaluate_aml_object(runtime, &acpi_child_name(path, "_STA")) {
        Ok(value) => match value.as_status() {
            Ok(status) => status,
            Err(err) => {
                printk!("[kernel-start][acpi] invalid _STA for {}: {:?}", path, err);
                return false;
            }
        },
        Err(err) if aml_error_is_missing(&err) => StatusObject::default(),
        Err(err) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate _STA for {}: {:?}",
                path,
                err
            );
            return false;
        }
    };
    status.present && status.enabled && status.functional
}

fn acpi_device_ids(runtime: &mut AmlRuntime, path: &AmlName) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = acpi_eval_string(runtime, path, "_HID") {
        ids.push(id);
    }
    match evaluate_aml_object(runtime, &acpi_child_name(path, "_CID")) {
        Ok(cid) => collect_acpi_id_object(cid, &mut ids),
        Err(err) if aml_error_is_missing(&err) => {}
        Err(err) => printk!(
            "[kernel-start][acpi] failed to evaluate _CID for {}: {:?}",
            path,
            err
        ),
    }
    ids
}

fn collect_acpi_id_object(object: AmlValue, ids: &mut Vec<String>) {
    match object {
        AmlValue::Integer(value) => {
            ids.push(eisa_id_to_string(value as u32));
        }
        AmlValue::String(value) => {
            ids.push(value);
        }
        AmlValue::Buffer(bytes) => {
            let bytes = bytes.lock();
            if let Ok(value) = core::str::from_utf8(&bytes) {
                ids.push(value.trim_end_matches('\0').to_string());
            }
        }
        AmlValue::Package(elements) => {
            for element in elements {
                collect_acpi_id_object(element, ids);
            }
        }
        _ => {}
    }
}

fn acpi_eval_string(runtime: &mut AmlRuntime, path: &AmlName, name: &str) -> Option<String> {
    let object = match evaluate_aml_object(runtime, &acpi_child_name(path, name)) {
        Ok(object) => object,
        Err(err) if aml_error_is_missing(&err) => return None,
        Err(err) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate {} for {}: {:?}",
                name,
                path,
                err
            );
            return None;
        }
    };
    match object {
        AmlValue::Integer(value) => Some(eisa_id_to_string(value as u32)),
        AmlValue::String(value) => Some(value),
        AmlValue::Buffer(bytes) => {
            let bytes = bytes.lock();
            core::str::from_utf8(&bytes)
                .ok()
                .map(|value| value.trim_end_matches('\0').to_string())
        }
        _ => None,
    }
}

fn acpi_device_resources(runtime: &mut AmlRuntime, path: &AmlName) -> Vec<DeviceResource> {
    let object = match evaluate_aml_object(runtime, &acpi_child_name(path, "_CRS")) {
        Ok(object) => object,
        Err(err) if aml_error_is_missing(&err) => return Vec::new(),
        Err(err) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate _CRS for {}: {:?}",
                path,
                err
            );
            return Vec::new();
        }
    };
    let AmlValue::Buffer(bytes) = object else {
        printk!(
            "[kernel-start][acpi] _CRS for {} did not return a resource buffer",
            path
        );
        return Vec::new();
    };
    let bytes = bytes.lock();
    let (resources, complete) = parse_resource_template(&bytes);
    if !complete {
        printk!(
            "[kernel-start][acpi] malformed _CRS for {}; keeping valid prefix",
            path
        );
    }
    resources
}

fn first_mmio_resource(resources: &[DeviceResource]) -> Option<(usize, usize)> {
    resources.iter().find_map(DeviceResource::as_mmio)
}

fn acpi_gsi_resource(gsi: u32) -> DeviceResource {
    let mut cells = Vec::new();
    cells.push(gsi);
    DeviceResource::irq(None, cells.into_boxed_slice())
}

#[derive(Clone, Copy)]
struct AcpiIrqDescriptor {
    irq: u32,
    trigger: IrqTrigger,
    polarity: IrqPolarity,
    is_shared: bool,
    is_wake_capable: bool,
}

fn acpi_irq_resource(irq: AcpiIrqDescriptor) -> DeviceResource {
    let mut cells = Vec::new();
    cells.push(irq.irq);
    DeviceResource::irq_with_attributes(
        None,
        cells.into_boxed_slice(),
        IrqResourceAttributes {
            trigger: Some(irq.trigger),
            polarity: Some(irq.polarity),
            sharing: Some(if irq.is_shared {
                IrqSharing::Shared
            } else {
                IrqSharing::Exclusive
            }),
            wake_capable: irq.is_wake_capable,
        },
    )
}

fn parse_resource_template(bytes: &[u8]) -> (Vec<DeviceResource>, bool) {
    let mut resources = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let (kind, body_start, body_len) = if tag & 0x80 != 0 {
            let Some(length_bytes) = bytes.get(offset + 1..offset + 3) else {
                return (resources, false);
            };
            (
                tag & 0x7f,
                offset + 3,
                u16::from_le_bytes([length_bytes[0], length_bytes[1]]) as usize,
            )
        } else {
            ((tag >> 3) & 0x0f, offset + 1, (tag & 0x07) as usize)
        };
        let Some(body_end) = body_start.checked_add(body_len) else {
            return (resources, false);
        };
        let Some(body) = bytes.get(body_start..body_end) else {
            return (resources, false);
        };

        if tag & 0x80 != 0 {
            if !parse_large_resource(kind, body, &mut resources) {
                return (resources, false);
            }
        } else {
            if kind == 0x0f {
                return (resources, true);
            }
            if kind == 0x04 && !parse_small_irq_resource(body, &mut resources) {
                return (resources, false);
            }
        }
        offset = body_end;
    }
    // A resource template is complete only after its EndTag descriptor.  An
    // otherwise valid prefix must not make a truncated firmware buffer look
    // usable to device discovery.
    (resources, false)
}

fn parse_large_resource(kind: u8, body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    match kind {
        // 32-bit fixed memory range: information byte, base, length.
        0x06 => {
            if body.len() < 9 {
                return false;
            }
            if let (Some(base), Some(length)) = (read_u32_le(body, 1), read_u32_le(body, 5)) {
                resources.push(DeviceResource::mmio(base as usize, length as usize));
            }
            true
        }
        // DWord, Word, and QWord address-space descriptors respectively.
        0x07 => parse_address_space_resource(body, 4, resources),
        0x08 => parse_address_space_resource(body, 2, resources),
        0x0a => parse_address_space_resource(body, 8, resources),
        0x09 => parse_extended_irq_resource(body, resources),
        // Other resource kinds are valid but irrelevant to the devices registered here.
        _ => true,
    }
}

fn parse_address_space_resource(
    body: &[u8],
    field_width: usize,
    resources: &mut Vec<DeviceResource>,
) -> bool {
    let Some(&resource_type) = body.first() else {
        return false;
    };
    if resource_type != 0 {
        return true;
    }
    if body.len() < 3 + field_width * 5 {
        return false;
    }
    let minimum = read_uint_le(body, 3 + field_width, field_width);
    let translation = read_uint_le(body, 3 + field_width * 3, field_width);
    let length = read_uint_le(body, 3 + field_width * 4, field_width);
    let (Some(minimum), Some(translation), Some(length)) = (minimum, translation, length) else {
        return false;
    };
    let Some(base) = minimum.checked_add(translation) else {
        return false;
    };
    if let (Ok(base), Ok(length)) = (usize::try_from(base), usize::try_from(length)) {
        resources.push(DeviceResource::mmio(base, length));
        true
    } else {
        false
    }
}

fn parse_small_irq_resource(body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    let Some(mask_bytes) = body.get(..2) else {
        return false;
    };
    let mask = u16::from_le_bytes([mask_bytes[0], mask_bytes[1]]);
    let information = body.get(2).copied().unwrap_or(0x01);
    for irq in 0..16 {
        if mask & (1 << irq) != 0 {
            resources.push(acpi_irq_resource(AcpiIrqDescriptor {
                irq,
                trigger: if information & 0x01 != 0 {
                    IrqTrigger::Edge
                } else {
                    IrqTrigger::Level
                },
                polarity: if information & 0x08 != 0 {
                    IrqPolarity::ActiveLow
                } else {
                    IrqPolarity::ActiveHigh
                },
                is_shared: information & 0x10 != 0,
                is_wake_capable: information & 0x20 != 0,
            }));
        }
    }
    true
}

fn parse_extended_irq_resource(body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    let (Some(&flags), Some(&count)) = (body.first(), body.get(1)) else {
        return false;
    };
    for index in 0..usize::from(count) {
        let Some(offset) = 2usize.checked_add(index * 4) else {
            return false;
        };
        let Some(irq) = read_u32_le(body, offset) else {
            return false;
        };
        resources.push(acpi_irq_resource(AcpiIrqDescriptor {
            irq,
            trigger: if flags & 0x02 != 0 {
                IrqTrigger::Edge
            } else {
                IrqTrigger::Level
            },
            // Extended IRQ flags encode bit 2 as 0 for active-high and 1 for
            // active-low.
            polarity: if flags & 0x04 != 0 {
                IrqPolarity::ActiveLow
            } else {
                IrqPolarity::ActiveHigh
            },
            is_shared: flags & 0x08 != 0,
            is_wake_capable: flags & 0x10 != 0,
        }));
    }
    true
}

fn read_uint_le(bytes: &[u8], offset: usize, width: usize) -> Option<u64> {
    let value = bytes.get(offset..offset.checked_add(width)?)?;
    match width {
        2 => Some(u16::from_le_bytes(value.try_into().ok()?) as u64),
        4 => Some(u32::from_le_bytes(value.try_into().ok()?) as u64),
        8 => Some(u64::from_le_bytes(value.try_into().ok()?)),
        _ => None,
    }
}

fn acpi_child_name(path: &AmlName, name: &str) -> AmlName {
    AmlName::from_str(name)
        .and_then(|child| child.resolve(path))
        .expect("[kernel-start][acpi] invalid static AML child name")
}

fn aml_error_is_missing(error: &AmlError) -> bool {
    matches!(error, AmlError::ValueDoesNotExist(_))
}

fn evaluate_aml_object(runtime: &mut AmlRuntime, path: &AmlName) -> Result<AmlValue, AmlError> {
    let is_method = matches!(
        runtime.context.namespace.get_by_path(path)?,
        AmlValue::Method { .. }
    );
    if is_method && !runtime.methods_enabled {
        return Err(AmlError::Unimplemented);
    }
    begin_aml_host_access();
    let result = if is_method {
        runtime.context.invoke_method(path, Args::default())
    } else {
        runtime.context.namespace.get_by_path(path).cloned()
    };
    if !finish_aml_host_access("object evaluation") {
        return Err(AmlError::Unimplemented);
    }
    result
}

fn eisa_id_to_string(raw: u32) -> String {
    let id = raw.swap_bytes();
    let c0 = (((id >> 26) & 0x1f) as u8 + b'@') as char;
    let c1 = (((id >> 21) & 0x1f) as u8 + b'@') as char;
    let c2 = (((id >> 16) & 0x1f) as u8 + b'@') as char;
    let mut result = String::new();
    result.push(c0);
    result.push(c1);
    result.push(c2);
    for shift in [12, 8, 4, 0] {
        let digit = ((id >> shift) & 0x0f) as u8;
        result.push(if digit < 10 {
            (b'0' + digit) as char
        } else {
            (b'A' + digit - 10) as char
        });
    }
    result
}

fn parse_power_controls(
    tables: &acpi::AcpiTables<AcpiMapper>,
    aml_runtime: Option<&mut AmlRuntime>,
) -> PowerControlInfo {
    let shutdown = acpi_shutdown_control(tables, aml_runtime);
    let reboot = acpi_reboot_control(tables);

    printk!(
        "[kernel-start][acpi] power controls: shutdown={} reboot={}",
        shutdown.is_some() as usize,
        reboot.is_some() as usize
    );

    PowerControlInfo { shutdown, reboot }
}

fn acpi_reboot_control(tables: &acpi::AcpiTables<AcpiMapper>) -> Option<PowerControlMethod> {
    let fadt = static_tables::find_supported_fadt(tables)?;
    if let Err(err) = fadt.validate() {
        printk!("[kernel-start][acpi] invalid FADT for reset: {:?}", err);
        return None;
    }

    let flags = unsafe { core::ptr::addr_of!(fadt.flags).read_unaligned() };
    if !flags.supports_system_reset_via_fadt() {
        printk!("[kernel-start][acpi] FADT reset register is not advertised");
        return None;
    }

    let register = match fadt.reset_register() {
        Ok(register) => register,
        Err(err) => {
            printk!(
                "[kernel-start][acpi] invalid FADT reset register: {:?}",
                err
            );
            return None;
        }
    };
    let register = match power_register_from_gas(register) {
        Some(register) => register,
        None => {
            printk!("[kernel-start][acpi] unsupported FADT reset register");
            return None;
        }
    };
    let value = unsafe { core::ptr::addr_of!(fadt.reset_value).read_unaligned() } as u64;

    printk!(
        "[kernel-start][acpi] reboot: FADT reset {:?} addr={:#x} value={:#x} width={:?}",
        register.space,
        register.address,
        value,
        register.access_width
    );

    Some(PowerControlMethod::RegisterWrite { register, value })
}

fn acpi_shutdown_control(
    tables: &acpi::AcpiTables<AcpiMapper>,
    aml_runtime: Option<&mut AmlRuntime>,
) -> Option<PowerControlMethod> {
    let fadt = static_tables::find_supported_fadt(tables)?;
    if let Err(err) = fadt.validate() {
        printk!("[kernel-start][acpi] invalid FADT for shutdown: {:?}", err);
        return None;
    }

    let (sleep_type_a, sleep_type_b) = match aml_runtime.and_then(acpi_s5_sleep_types) {
        Some(types) => types,
        None => {
            printk!("[kernel-start][acpi] ACPI _S5 sleep type not found");
            return None;
        }
    };

    let flags = unsafe { core::ptr::addr_of!(fadt.flags).read_unaligned() };
    if flags.system_is_hw_reduced_acpi() {
        let register = match fadt.sleep_control_register() {
            Ok(Some(register)) => register,
            Ok(None) => {
                printk!("[kernel-start][acpi] hardware-reduced FADT lacks sleep control register");
                return None;
            }
            Err(err) => {
                printk!(
                    "[kernel-start][acpi] invalid sleep control register: {:?}",
                    err
                );
                return None;
            }
        };
        let sleep_control = match power_register_from_gas(register) {
            Some(register) => register,
            None => {
                printk!("[kernel-start][acpi] unsupported sleep control register");
                return None;
            }
        };
        printk!(
            "[kernel-start][acpi] shutdown: HW-reduced sleep_control {:?} addr={:#x} S5={}",
            sleep_control.space,
            sleep_control.address,
            sleep_type_a
        );
        return Some(PowerControlMethod::AcpiSleepControl {
            sleep_control,
            sleep_type: sleep_type_a,
        });
    }

    let pm1a_control = match fadt
        .pm1a_control_block()
        .ok()
        .and_then(power_register_from_gas)
    {
        Some(register) => register,
        None => {
            printk!("[kernel-start][acpi] PM1a control register unavailable");
            return None;
        }
    };
    let pm1b_control = fadt
        .pm1b_control_block()
        .ok()
        .flatten()
        .and_then(power_register_from_gas);

    printk!(
        "[kernel-start][acpi] shutdown: PM1a {:?} addr={:#x} S5a={} S5b={}",
        pm1a_control.space,
        pm1a_control.address,
        sleep_type_a,
        sleep_type_b
    );

    Some(PowerControlMethod::AcpiPm1Sleep {
        pm1a_control,
        pm1b_control,
        sleep_type_a,
        sleep_type_b,
    })
}

fn power_register_from_gas(gas: GenericAddress) -> Option<PowerRegister> {
    let space = match gas.address_space {
        AddressSpace::SystemMemory => PowerRegisterSpace::SystemMemory,
        AddressSpace::SystemIo => PowerRegisterSpace::SystemIo,
        _ => return None,
    };
    let access_size = match gas.access_size {
        AccessSize::Undefined => 0,
        AccessSize::ByteAccess => 1,
        AccessSize::WordAccess => 2,
        AccessSize::DWordAccess => 3,
        AccessSize::QWordAccess => 4,
    };
    let access_width = PowerAccessWidth::from_bits(gas.bit_width, access_size)?;
    Some(PowerRegister {
        space,
        address: gas.address as usize,
        access_width,
    })
}

fn acpi_s5_sleep_types(runtime: &mut AmlRuntime) -> Option<(u8, u8)> {
    let value = match evaluate_aml_object(
        runtime,
        &AmlName::from_str("\\_S5").expect("valid static AML name"),
    ) {
        Ok(value) => value,
        Err(err) => {
            printk!("[kernel-start][acpi] failed to evaluate _S5: {:?}", err);
            return None;
        }
    };
    let AmlValue::Package(elements) = value else {
        printk!("[kernel-start][acpi] _S5 did not evaluate to a package");
        return None;
    };
    let sleep_type_a = elements.first()?.as_integer(&runtime.context).ok()?;
    let sleep_type_b = elements
        .get(1)
        .and_then(|value| value.as_integer(&runtime.context).ok())
        .unwrap_or(sleep_type_a);
    let (Ok(sleep_type_a), Ok(sleep_type_b)) =
        (u8::try_from(sleep_type_a), u8::try_from(sleep_type_b))
    else {
        printk!("[kernel-start][acpi] _S5 contains an invalid sleep type");
        return None;
    };
    printk!(
        "[kernel-start][acpi] _S5 sleep types: a={} b={}",
        sleep_type_a,
        sleep_type_b
    );
    Some((sleep_type_a, sleep_type_b))
}

fn serial_device_from_spcr(tables: &acpi::AcpiTables<AcpiMapper>) -> Option<FirmwareSerialDevice> {
    let spcr = static_tables::find_supported_spcr(tables)?;
    if let Err(err) = spcr.validate() {
        printk!("[kernel-start][acpi] invalid SPCR: {:?}", err);
        return None;
    }

    if !spcr_interface_is_16550_compatible(spcr.interface_type()) {
        printk!(
            "[kernel-start][acpi] SPCR interface {:?} is not ns16550-compatible",
            spcr.interface_type(),
        );
        return None;
    }

    let base_address = match spcr.base_address()? {
        Ok(address) => address,
        Err(err) => {
            printk!("[kernel-start][acpi] invalid SPCR base address: {:?}", err);
            return None;
        }
    };
    if base_address.address_space != AddressSpace::SystemMemory || base_address.address == 0 {
        printk!(
            "[kernel-start][acpi] SPCR base address is not system memory: {:?}",
            base_address,
        );
        return None;
    }

    let clock_hz = spcr.uart_clock_frequency().map(|clock| clock.get());
    let baud = spcr.baud_rate().map(|baud| baud.get());
    let phys_addr = base_address.address as usize;
    let name = spcr_namespace_name(&spcr)
        .unwrap_or_else(|| alloc::format!("serial@{:#x}", phys_addr).leak());

    if let Some(clock_hz) = clock_hz {
        printk!(
            "[kernel-start][acpi] SPCR serial: {} phys={:#x} clock={}Hz",
            name,
            phys_addr,
            clock_hz,
        );
    } else {
        printk!(
            "[kernel-start][acpi] SPCR serial: {} phys={:#x} clock=<firmware-configured>",
            name,
            phys_addr,
        );
    }

    let mut resources = Vec::new();
    resources.push(DeviceResource::mmio(phys_addr, 0));
    if let Some(gsi) = spcr.global_system_interrupt() {
        resources.push(acpi_gsi_resource(gsi));
    } else if let Some(irq) = spcr.irq() {
        resources.push(acpi_gsi_resource(u32::from(irq)));
    }

    Some(FirmwareSerialDevice {
        port: SerialPortInfo {
            name,
            phys_addr,
            reg_size: None,
            clock_hz,
            baud,
        },
        resources,
    })
}

fn spcr_interface_is_16550_compatible(interface: SpcrInterfaceType) -> bool {
    matches!(
        interface,
        SpcrInterfaceType::Full16550
            | SpcrInterfaceType::Full16450
            | SpcrInterfaceType::Nvidia16550
            | SpcrInterfaceType::Generic16550
    )
}

fn spcr_namespace_name(spcr: &PhysicalMapping<AcpiMapper, Spcr>) -> Option<&'static str> {
    // `acpi::Spcr::namespace_string` trusts firmware offsets. Keep the crate for the
    // fixed fields, but validate this variable-length tail against the mapped SDT.
    let bytes = unsafe {
        slice::from_raw_parts(
            spcr.virtual_start().as_ptr().cast::<u8>(),
            spcr.region_length(),
        )
    };
    let length = usize::from(read_u16_le(bytes, 84)?);
    let offset = usize::from(read_u16_le(bytes, 86)?);
    if length == 0 || offset < mem::size_of::<Spcr>() {
        return None;
    }
    let name = core::str::from_utf8(bytes.get(offset..offset.checked_add(length)?)?).ok()?;
    let name = name.trim_matches('\0').trim();
    if name.is_empty() || name == "." {
        return None;
    }
    Some(name.to_string().leak())
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

#[cfg(feature = "kernel-tests")]
mod tests {
    use alloc::vec;

    use ktest::ktest;

    use super::*;

    fn identity_address(address: usize) -> usize {
        address
    }

    fn offset_device_address(address: usize) -> usize {
        address + 0x4000
    }

    fn test_aml_runtime(methods_enabled: bool) -> AmlRuntime {
        AmlRuntime {
            context: AmlContext::new(
                Box::new(AcpiMapper::new(
                    identity_address,
                    identity_address,
                    &[],
                    StartAcpiHostOps::NONE,
                )),
                DebugVerbosity::None,
            ),
            methods_enabled,
        }
    }

    #[ktest]
    fn disabled_aml_methods_fail_without_invoking_backend() {
        let mut runtime = test_aml_runtime(false);
        let path = AmlName::from_str("\\MTHD").expect("valid AML test name");
        runtime
            .context
            .namespace
            .add_value(
                path.clone(),
                AmlValue::native_method(0, false, 0, |_| Ok(AmlValue::Integer(1))),
            )
            .expect("insert AML test method");

        assert!(matches!(
            evaluate_aml_object(&mut runtime, &path),
            Err(AmlError::Unimplemented)
        ));
    }

    #[ktest]
    fn static_aml_names_remain_available_when_methods_are_disabled() {
        let mut runtime = test_aml_runtime(false);
        let path = AmlName::from_str("\\STAT").expect("valid AML test name");
        runtime
            .context
            .namespace
            .add_value(path.clone(), AmlValue::Integer(7))
            .expect("insert AML static test object");

        assert!(matches!(
            evaluate_aml_object(&mut runtime, &path),
            Ok(AmlValue::Integer(7))
        ));
    }

    #[ktest]
    fn parses_qword_memory_resource_and_skips_unknown_descriptor() {
        let mut template = Vec::new();
        // Unknown large descriptor with a one-byte payload.
        template.extend_from_slice(&[0x84, 0x01, 0x00, 0xaa]);
        // QWord address-space descriptor, memory type, minimum 0x1000, translation 0x20,
        // length 0x200. The maximum and granularity fields are irrelevant to registration.
        template.extend_from_slice(&[0x8a, 0x2b, 0x00, 0x00, 0x00, 0x00]);
        template.extend_from_slice(&0u64.to_le_bytes());
        template.extend_from_slice(&0x1000u64.to_le_bytes());
        template.extend_from_slice(&0x11ffu64.to_le_bytes());
        template.extend_from_slice(&0x20u64.to_le_bytes());
        template.extend_from_slice(&0x200u64.to_le_bytes());
        template.extend_from_slice(&[0x79, 0x00]);

        let (resources, complete) = parse_resource_template(&template);
        assert!(complete);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].as_mmio(), Some((0x1020, 0x200)));
    }

    #[ktest]
    fn malformed_resource_keeps_valid_prefix() {
        let template = [
            0x86, 0x09, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x8a, 0x2b,
            0x00, 0x00,
        ];
        let (resources, complete) = parse_resource_template(&template);
        assert!(!complete);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].as_mmio(), Some((0x2000, 0x1000)));
    }

    #[ktest]
    fn resource_without_end_tag_is_incomplete() {
        let template = [
            0x86, 0x09, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
        ];
        let (resources, complete) = parse_resource_template(&template);
        assert!(!complete);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].as_mmio(), Some((0x2000, 0x1000)));
    }

    #[ktest]
    fn parses_extended_irq_polarity() {
        // Two edge-triggered Extended IRQs: bit 2 clear is active-high and set is active-low.
        let template = [
            0x89, 0x06, 0x00, 0x02, 0x01, 0x21, 0x00, 0x00, 0x00, 0x89, 0x06, 0x00, 0x06, 0x01,
            0x22, 0x00, 0x00, 0x00, 0x79, 0x00,
        ];
        let (resources, complete) = parse_resource_template(&template);
        assert!(complete);
        let high = resources[0].as_irq().expect("active-high IRQ resource");
        assert_eq!(high.cells(), &[33]);
        assert_eq!(high.attributes().trigger, Some(IrqTrigger::Edge));
        assert_eq!(high.attributes().polarity, Some(IrqPolarity::ActiveHigh));
        let low = resources[1].as_irq().expect("active-low IRQ resource");
        assert_eq!(low.cells(), &[34]);
        assert_eq!(low.attributes().trigger, Some(IrqTrigger::Edge));
        assert_eq!(low.attributes().polarity, Some(IrqPolarity::ActiveLow));
    }

    #[ktest]
    fn converts_integer_eisa_id() {
        let encoded = ((16u32 << 26) | (14 << 21) | (16 << 16) | 0x0501).swap_bytes();
        assert_eq!(eisa_id_to_string(encoded), "PNP0501");
    }

    #[ktest]
    fn resolves_pci_config_space_from_valid_mcfg() {
        let mut table = vec![0u8; 60];
        table[..4].copy_from_slice(b"MCFG");
        table[4..8].copy_from_slice(&60u32.to_le_bytes());
        table[44..52].copy_from_slice(&0x8000_0000u64.to_le_bytes());
        table[52..54].copy_from_slice(&2u16.to_le_bytes());
        table[54] = 0x20;
        table[55] = 0x2f;
        table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
        let table = Box::leak(table.into_boxed_slice());
        let mappings = Box::leak(
            vec![FirmwareTableMapping {
                physical_start: 0x1000,
                virtual_start: table.as_ptr() as usize,
                length: table.len(),
            }]
            .into_boxed_slice(),
        );
        let mapper = AcpiMapper::new(
            |physical| physical,
            |physical| physical,
            mappings,
            StartAcpiHostOps::NONE,
        );

        assert!(mapper.pci_backend_available());
        assert_eq!(
            mapper.pci_ecam_address(2, 0x21, 3, 2, 0x120),
            Some(0x8011_a120)
        );
        assert_eq!(mapper.pci_ecam_address(1, 0x21, 3, 2, 0x120), None);
        assert_eq!(mapper.pci_ecam_address(2, 0x30, 3, 2, 0x120), None);
        assert!(!mapper.pci_ecam_range_available(2, 0x21, 3, 2, 0x0fff, 2));
        assert!(!mapper.pci_ecam_range_available(2, 0x21, 3, 2, 0x0121, 2));
        assert!(!mapper.pci_ecam_range_available(2, 0x21, 3, 2, 0x0122, 4));
    }

    #[ktest]
    fn rejects_malformed_mcfg_entries() {
        let mut table = vec![0u8; 60];
        table[..4].copy_from_slice(b"MCFG");
        table[4..8].copy_from_slice(&60u32.to_le_bytes());
        table[44..52].copy_from_slice(&0x8000_0000u64.to_le_bytes());
        table[54] = 0x20;
        table[55] = 0x2f;
        table[56] = 1;
        table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
        let table = Box::leak(table.into_boxed_slice());
        let mappings = Box::leak(
            vec![FirmwareTableMapping {
                physical_start: 0x1000,
                virtual_start: table.as_ptr() as usize,
                length: table.len(),
            }]
            .into_boxed_slice(),
        );
        let mapper = AcpiMapper::new(
            identity_address,
            identity_address,
            mappings,
            StartAcpiHostOps::NONE,
        );

        assert!(!mapper.pci_backend_available());
        assert_eq!(mapper.pci_ecam_address(0, 0x20, 0, 0, 0), None);
    }

    #[ktest]
    fn separates_table_snapshots_from_device_mmio() {
        let table = Box::leak(vec![0u8; 16].into_boxed_slice());
        let mappings = Box::leak(
            vec![FirmwareTableMapping {
                physical_start: 0x1000,
                virtual_start: table.as_ptr() as usize,
                length: table.len(),
            }]
            .into_boxed_slice(),
        );
        let mapper = AcpiMapper::new(
            identity_address,
            offset_device_address,
            mappings,
            StartAcpiHostOps::NONE,
        );

        assert_eq!(mapper.resolve_table(0x1004, 4), table.as_ptr() as usize + 4);
        assert_eq!(mapper.resolve_mmio::<u32>(0x1004), Some(0x5004));
        assert_eq!(mapper.resolve_mmio::<u32>(0x1002), None);
    }
}
