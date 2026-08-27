//! 基于 ACPI 的内核初始化逻辑。
//!
//! 启动路径先完整枚举并校验 RSDT/XSDT 中的静态表，再初始化内存分配器并只构建一次
//! DSDT/SSDT AML namespace。静态表、AML 解析和 AML Host I/O 能力彼此独立：缺失可选
//! Host 后端不会阻止表清单与静态对象解析，也不会用伪造值继续执行固件方法。

mod pci;
mod platform;
mod static_tables;

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, Ordering};
use core::{mem, slice};

use acpi::spcr::SpcrInterfaceType;
use acpi::{AcpiHandler, PhysicalMapping};
use aml::value::{Args, StatusObject};
use aml::{AmlContext, AmlError, AmlName, AmlValue, DebugVerbosity, LevelType};

use allocator::KERNEL_ALLOCATOR;
use general::dev::dma::DmaContext;
use general::dev::platform::{
    DeviceMatchId, DeviceProperties, DeviceResource, FirmwareProperty, IrqPolarity,
    IrqResourceAttributes, IrqSharing, IrqTrigger, PlatformDeviceInfo, PlatformProbeStatus,
    register_and_probe_platform_device,
};
use general::dev::pnp::DevInitContext;
use general::firmware::power::{
    PowerAccessWidth, PowerControlInfo, PowerControlMethod, PowerRegister, PowerRegisterSpace,
};
use general::firmware::{FirmwareRegisterSpace, FirmwareTableMapping, SerialPortInfo};
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
const ACPI_HID_PNP0A03: &str = "PNP0A03";
const ACPI_HID_PNP0A08: &str = "PNP0A08";
const ACPI_HID_PNP0C0F: &str = "PNP0C0F";
const ACPI_HID_VIRTIO_MMIO: &str = "LNRO0005";
const EFI_MEMORY_WC: u64 = 1 << 1;
const EFI_MEMORY_ATTRIBUTES_ALLOWED: u64 =
    0x0000_0000_0000_001f | 0x0000_0000_000f_f000 | 0x8000_0000_0000_0000;
const AML_HOST_FAULT_INVALID_MMIO: u32 = 1 << 0;
const AML_HOST_FAULT_SYSTEM_IO: u32 = 1 << 1;
const AML_HOST_FAULT_PCI_CONFIG: u32 = 1 << 2;

static AML_HOST_FAULTS: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy)]
struct AcpiMapper {
    device_mmio_to_virt: fn(usize) -> usize,
    copied_tables: &'static [FirmwareTableMapping],
    host_ops: StartAcpiHostOps,
}

struct AmlRuntime {
    context: AmlContext,
}

impl AcpiMapper {
    const fn new(
        device_mmio_to_virt: fn(usize) -> usize,
        copied_tables: &'static [FirmwareTableMapping],
        host_ops: StartAcpiHostOps,
    ) -> Self {
        Self {
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
        panic!(
            "[kernel-start][acpi] table range {:#x}+{:#x} is absent from the immutable snapshot",
            physical_address, size
        )
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

    fn read_aml_mmio<const N: usize>(&self, physical_address: usize) -> Option<[u8; N]> {
        physical_address.checked_add(N)?;
        let mut bytes = [0u8; N];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let virtual_address = (self.device_mmio_to_virt)(physical_address + index);
            if virtual_address == 0 {
                record_aml_host_fault(AML_HOST_FAULT_INVALID_MMIO);
                return None;
            }
            // Safety: each address is mapped by the architecture callback and a byte
            // access has no alignment requirement. AML SystemMemory fields may be unaligned.
            *byte = unsafe { ptr::read_volatile(virtual_address as *const u8) };
        }
        Some(bytes)
    }

    fn write_aml_mmio<const N: usize>(&self, physical_address: usize, bytes: [u8; N]) -> bool {
        if physical_address.checked_add(N).is_none() {
            record_aml_host_fault(AML_HOST_FAULT_INVALID_MMIO);
            return false;
        }
        for (index, byte) in bytes.into_iter().enumerate() {
            let virtual_address = (self.device_mmio_to_virt)(physical_address + index);
            if virtual_address == 0 {
                record_aml_host_fault(AML_HOST_FAULT_INVALID_MMIO);
                return false;
            }
            // Safety: same mapping and byte-alignment contract as `read_aml_mmio`.
            unsafe { ptr::write_volatile(virtual_address as *mut u8, byte) };
        }
        true
    }

    #[cfg(feature = "kernel-tests")]
    fn pci_backend_available(self) -> bool {
        self.host_ops.pci.is_some() || self.mcfg_entries().next().is_some()
    }

    fn mcfg_entries(self) -> impl Iterator<Item = &'static [u8]> {
        self.copied_tables.iter().flat_map(|mapping| {
            let bytes: &'static [u8] = if mapping.virtual_start != 0
                && mapping.length >= 44
                && mapping.virtual_start.checked_add(mapping.length).is_some()
            {
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
                .checked_add(u64::from(bus) << 20)?
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
        self.read_aml_mmio::<1>(address)
            .map_or(u8::MAX, |bytes| bytes[0])
    }

    fn read_u16(&self, address: usize) -> u16 {
        self.read_aml_mmio(address)
            .map_or(u16::MAX, u16::from_le_bytes)
    }

    fn read_u32(&self, address: usize) -> u32 {
        self.read_aml_mmio(address)
            .map_or(u32::MAX, u32::from_le_bytes)
    }

    fn read_u64(&self, address: usize) -> u64 {
        self.read_aml_mmio(address)
            .map_or(u64::MAX, u64::from_le_bytes)
    }

    fn write_u8(&mut self, address: usize, value: u8) {
        let _ = self.write_aml_mmio(address, [value]);
    }

    fn write_u16(&mut self, address: usize, value: u16) {
        let _ = self.write_aml_mmio(address, value.to_le_bytes());
    }

    fn write_u32(&mut self, address: usize, value: u32) {
        let _ = self.write_aml_mmio(address, value.to_le_bytes());
    }

    fn write_u64(&mut self, address: usize, value: u64) {
        let _ = self.write_aml_mmio(address, value.to_le_bytes());
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

fn serial_register_properties(port: &SerialPortInfo) -> Vec<FirmwareProperty> {
    let mut properties = Vec::new();
    if let Some(reg_shift) = port.reg_shift {
        properties.push(FirmwareProperty::new(
            "reg-shift".into(),
            reg_shift.to_be_bytes().into(),
        ));
    }
    if let Some(reg_io_width) = port.reg_io_width {
        properties.push(FirmwareProperty::new(
            "reg-io-width".into(),
            reg_io_width.to_be_bytes().into(),
        ));
    }
    properties
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

    let platform_info = platform::parse_and_publish(
        acpi.mappings,
        context.boot.boot_cpu_id,
        &memory_segments,
        context.memory.boot_map.regions(),
    );
    let serial_device = serial_device_from_spcr(acpi.mappings, platform_info.madt.as_ref());
    let console_serial_port = serial_device
        .as_ref()
        .map(|device| (device.port.register_space, device.port.phys_addr));
    let mcfg_installed = match pci::install_mcfg_backend(
        &platform_info.pci_config_regions,
        context.address.device_mmio_to_virt,
    ) {
        Ok(summary) => summary.regions != 0,
        Err(error) => {
            printk!(
                "[kernel-start][acpi] failed to install MCFG PCI backend: {:?}",
                error
            );
            false
        }
    };

    // AML interpretation allocates namespace objects. Build and initialize one namespace after
    // the allocator is ready, then reuse it for power and device discovery so that table loading
    // and `_INI` side effects happen exactly once.
    let mut aml_runtime = build_aml_runtime(mapper, &tables);
    let acpi_mode_ready = ensure_acpi_mode(
        platform_info.fadt.as_ref(),
        context.address.device_mmio_to_virt,
        acpi.host_ops.io,
    );
    let power_controls = parse_power_controls(
        platform_info.fadt.as_ref(),
        aml_runtime.as_mut(),
        acpi_mode_ready,
    );
    general::firmware::power::install_with_acpi_ops(
        power_controls,
        context.address.device_mmio_to_virt,
        acpi.host_ops.io,
        acpi.host_ops.pci,
    );

    let cmdline = context
        .boot
        .command_line
        .map(general::cmdline::Cmdline::new);

    // ── 阶段 4：发现并登记 ACPI 设备描述 ──────────────────────────────────

    let mut serial_devices: Vec<FirmwareSerialDevice> = Vec::new();
    let mut virtio_mmio_devices: Vec<FirmwareMmioDevice> = Vec::new();
    let mut pci_roots: Vec<pci::AcpiPciRootBridge> = Vec::new();
    discover_acpi_namespace_devices(
        aml_runtime.as_mut(),
        &mut serial_devices,
        &mut virtio_mmio_devices,
        &mut pci_roots,
        &platform_info.pci_config_regions,
    );
    let pci_roots_published = if pci_roots.is_empty() {
        false
    } else if !mcfg_installed {
        printk!(
            "[kernel-start][acpi] ignored {} PCI root bridge(s) without a usable MCFG backend",
            pci_roots.len()
        );
        false
    } else {
        match pci::publish_root_bridges(&pci_roots) {
            Ok(summary) => summary.registered_hosts != 0,
            Err(error) => {
                printk!(
                    "[kernel-start][acpi] failed to publish PCI root bridges: {:?}",
                    error
                );
                false
            }
        }
    };
    if let Some(device) = serial_device
        && !serial_devices.iter().any(|existing| {
            existing.port.register_space == device.port.register_space
                && existing.port.phys_addr == device.port.phys_addr
        })
    {
        serial_devices.push(device);
    }
    let console_serial_port_index = console_serial_port.and_then(|key| {
        serial_devices
            .iter()
            .position(|device| (device.port.register_space, device.port.phys_addr) == key)
    });

    printk!(
        "[kernel-start][acpi] device discovery complete: {} uart(s), {} virtio block candidate(s), {} PCI root(s)",
        serial_devices.len(),
        virtio_mmio_devices.len(),
        pci_roots.len()
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
            .with_system_io(acpi.host_ops.io)
            .with_boot_cpu_id(context.boot.boot_cpu_id)
            .with_realtime_clock(crate::vdso::set_realtime_ns)
            .with_realtime_source_hooks(
                crate::vdso::install_realtime_source,
                crate::vdso::unregister_realtime_source,
            ),
        None,
    );
    if pci_roots_published && let Err(error) = pci::scan_root_bridges() {
        printk!(
            "[kernel-start][acpi] failed to scan PCI root bridges: {:?}",
            error
        );
    }

    let stdout_port = console_serial_port;
    let mut platform_bound = 0usize;
    for device in &serial_devices {
        let port = &device.port;
        let mut ids = Vec::new();
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_PNP0500.into()));
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_PNP0501.into()));
        let info = PlatformDeviceInfo {
            fw_name: port.name.clone(),
            fw_path: None,
            fw_parent_path: None,
            ids,
            resources: device.resources.clone(),
            irq_names: Vec::new(),
            properties: DeviceProperties {
                clock_hz: port.clock_hz,
                baud: port.baud,
                numa_node_id: None,
                fw_phandle: None,
                fw_interrupt_parent: None,
                interrupt_controller: false,
                fw_address_cells: None,
                fw_size_cells: None,
                fw_parent_address_cells: None,
                fw_parent_size_cells: None,
                stdout: stdout_port == Some((port.register_space, port.phys_addr)),
            },
            fw_properties: serial_register_properties(port),
            dma: DmaContext::default_coherent(),
            dtb_bindings: None,
            dtb_pcie_host: None,
            dtb_owned_nodes: None,
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
            irq_names: Vec::new(),
            properties: DeviceProperties::default(),
            fw_properties: Vec::new(),
            dma: DmaContext::default_coherent(),
            dtb_bindings: None,
            dtb_pcie_host: None,
            dtb_owned_nodes: None,
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
        false,
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
            String::from(device.port.name.as_ref()),
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
    pci_roots: &mut Vec<pci::AcpiPciRootBridge>,
    mcfg_regions: &[general::firmware::acpi::AcpiPciConfigRegion],
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
        let is_pci_root = ids
            .iter()
            .any(|id| id == ACPI_HID_PNP0A03 || id == ACPI_HID_PNP0A08);
        if !is_serial && !is_virtio_mmio && !is_pci_root {
            continue;
        }

        if is_pci_root {
            let Some(root) = acpi_pci_root_bridge(runtime, &path) else {
                continue;
            };
            if !pci::mcfg_covers_root(mcfg_regions, root.segment, root.bus_start, root.bus_end) {
                printk!(
                    "[kernel-start][acpi] PCI root {} segment={} buses={:#x}..={:#x} is not covered by MCFG",
                    path_string,
                    root.segment,
                    root.bus_start,
                    root.bus_end
                );
                continue;
            }
            if pci_roots.iter().any(|existing| {
                existing.segment == root.segment
                    && root.bus_start <= existing.bus_end
                    && existing.bus_start <= root.bus_end
            }) {
                printk!(
                    "[kernel-start][acpi] rejected overlapping PCI root {} segment={} buses={:#x}..={:#x}",
                    path_string,
                    root.segment,
                    root.bus_start,
                    root.bus_end
                );
                continue;
            }
            printk!(
                "[kernel-start][acpi] namespace PCI root: {} segment={} buses={:#x}..={:#x} windows={} _PRT={}",
                path_string,
                root.segment,
                root.bus_start,
                root.bus_end,
                root.windows.len(),
                root.irq_routes.len()
            );
            pci_roots.push(root);
            continue;
        }

        let resources = acpi_device_resources(runtime, &path);

        let name = alloc::format!("{}", path).leak();
        if is_serial {
            let location = first_mmio_resource(&resources)
                .filter(|&(_, size)| size != 0)
                .map(|(address, size)| (FirmwareRegisterSpace::SystemMemory, address, size))
                .or_else(|| {
                    resources
                        .iter()
                        .find_map(DeviceResource::as_io_port)
                        .map(|(base, size)| {
                            (
                                FirmwareRegisterSpace::SystemIo,
                                usize::from(base),
                                usize::from(size),
                            )
                        })
                });
            let Some((register_space, address, size)) = location else {
                printk!(
                    "[kernel-start][acpi] ACPI serial {} has no MMIO or SystemIO resource",
                    path_string
                );
                continue;
            };
            if !serial_devices.iter().any(|device| {
                device.port.register_space == register_space && device.port.phys_addr == address
            }) {
                printk!(
                    "[kernel-start][acpi] namespace serial: {} space={:?} address={:#x} size={:#x}",
                    name,
                    register_space,
                    address,
                    size
                );
                serial_devices.push(FirmwareSerialDevice {
                    port: SerialPortInfo {
                        name: name.into(),
                        register_space,
                        phys_addr: address,
                        reg_size: Some(size),
                        reg_shift: None,
                        reg_io_width: None,
                        clock_hz: None,
                        baud: None,
                    },
                    resources,
                });
            }
        } else if let Some((phys_addr, size)) =
            first_mmio_resource(&resources).filter(|&(_, size)| size != 0)
            && !virtio_mmio_devices
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

    let mut runtime = AmlRuntime { context };
    initialize_aml_namespace(&mut runtime);
    Some(runtime)
}

fn initialize_aml_namespace(runtime: &mut AmlRuntime) {
    let sb_ini = AmlName::from_str("\\_SB._INI").expect("valid static AML name");
    match evaluate_aml_object(runtime, &sb_ini) {
        Ok(_) | Err(AmlError::ValueDoesNotExist(_)) => {}
        Err(error) => printk!(
            "[kernel-start][acpi] AML \\_SB._INI initialization failed: {:?}",
            error
        ),
    }

    let mut device_paths = Vec::new();
    let mut namespace = runtime.context.namespace.clone();
    if let Err(error) = namespace.traverse(|path, level| {
        if level.typ == LevelType::Device {
            device_paths.push(path.clone());
        }
        Ok(true)
    }) {
        printk!(
            "[kernel-start][acpi] failed to enumerate AML _INI methods: {:?}",
            error
        );
        return;
    }

    let mut initialized = 0usize;
    let mut failed = 0usize;
    for path in device_paths {
        let status = match evaluate_acpi_status(runtime, &path) {
            Ok(status) => status,
            Err(error) => {
                failed += 1;
                printk!(
                    "[kernel-start][acpi] skipped _INI for {} after _STA failure: {:?}",
                    path,
                    error
                );
                continue;
            }
        };
        if !status.present {
            continue;
        }
        let ini = acpi_child_name(&path, "_INI");
        match evaluate_aml_object(runtime, &ini) {
            Ok(_) => initialized += 1,
            Err(AmlError::ValueDoesNotExist(_)) => {}
            Err(error) => {
                failed += 1;
                printk!(
                    "[kernel-start][acpi] AML _INI failed for {}: {:?}",
                    path,
                    error
                );
            }
        }
    }
    printk!(
        "[kernel-start][acpi] AML _INI initialization complete: initialized={} failed={}",
        initialized,
        failed
    );
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
    let status = match evaluate_acpi_status(runtime, path) {
        Ok(status) => status,
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

fn evaluate_acpi_status(
    runtime: &mut AmlRuntime,
    path: &AmlName,
) -> Result<StatusObject, AmlError> {
    match evaluate_aml_object(runtime, &acpi_child_name(path, "_STA")) {
        Ok(value) => value.as_status(),
        Err(error) if aml_error_is_missing(&error) => Ok(StatusObject::default()),
        Err(error) => Err(error),
    }
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
            "[kernel-start][acpi] malformed or unsupported _CRS for {}; ignoring template",
            path
        );
        return Vec::new();
    }
    resources
}

fn acpi_pci_root_bridge(
    runtime: &mut AmlRuntime,
    path: &AmlName,
) -> Option<pci::AcpiPciRootBridge> {
    let segment = match acpi_eval_optional_integer(runtime, path, "_SEG") {
        Ok(value) => u16::try_from(value.unwrap_or(0)).ok(),
        Err(error) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate _SEG for {}: {:?}",
                path,
                error
            );
            return None;
        }
    };
    let Some(segment) = segment else {
        printk!("[kernel-start][acpi] invalid _SEG for {}", path);
        return None;
    };
    let base_bus = match acpi_eval_optional_integer(runtime, path, "_BBN") {
        Ok(value) => u8::try_from(value.unwrap_or(0)).ok(),
        Err(error) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate _BBN for {}: {:?}",
                path,
                error
            );
            return None;
        }
    };
    let Some(base_bus) = base_bus else {
        printk!("[kernel-start][acpi] invalid _BBN for {}", path);
        return None;
    };
    let numa_node_id = match acpi_eval_optional_integer(runtime, path, "_PXM") {
        Ok(value) => match value.map(u32::try_from).transpose() {
            Ok(value) => value,
            Err(_) => {
                printk!("[kernel-start][acpi] invalid _PXM for {}", path);
                None
            }
        },
        Err(error) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate _PXM for {}: {:?}",
                path,
                error
            );
            None
        }
    };

    let crs = match evaluate_aml_object(runtime, &acpi_child_name(path, "_CRS")) {
        Ok(AmlValue::Buffer(bytes)) => bytes,
        Ok(_) => {
            printk!(
                "[kernel-start][acpi] PCI root _CRS for {} is not a buffer",
                path
            );
            return None;
        }
        Err(error) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate PCI root _CRS for {}: {:?}",
                path,
                error
            );
            return None;
        }
    };
    let resources = {
        let bytes = crs.lock();
        parse_pci_root_resource_template(&bytes)
    };
    let Some(resources) = resources else {
        printk!(
            "[kernel-start][acpi] malformed or unsupported PCI root _CRS for {}",
            path
        );
        return None;
    };
    if resources.bus_start != base_bus {
        printk!(
            "[kernel-start][acpi] PCI root {} _BBN={:#x} disagrees with _CRS bus start={:#x}",
            path,
            base_bus,
            resources.bus_start
        );
        return None;
    }

    let irq_routes = acpi_pci_prt_routes(runtime, path);
    Some(pci::AcpiPciRootBridge {
        firmware_path: path.as_string().into_boxed_str(),
        segment,
        bus_start: resources.bus_start,
        bus_end: resources.bus_end,
        numa_node_id,
        windows: resources.windows,
        dma_coherent: hal::platform::acpi_pci_dma_coherent_default(),
        identity_dma: hal::platform::acpi_pci_identity_dma_default(),
        irq_routes,
    })
}

fn acpi_eval_optional_integer(
    runtime: &mut AmlRuntime,
    path: &AmlName,
    name: &str,
) -> Result<Option<u64>, AmlError> {
    match evaluate_aml_object(runtime, &acpi_child_name(path, name)) {
        Ok(value) => {
            begin_aml_host_access();
            let result = value.as_integer(&runtime.context);
            if !finish_aml_host_access("PCI root integer conversion") {
                return Err(AmlError::Unimplemented);
            }
            result.map(Some)
        }
        Err(error) if aml_error_is_missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

struct AcpiPciRootResources {
    bus_start: u8,
    bus_end: u8,
    windows: Vec<pci::AcpiPciRootWindow>,
}

fn parse_pci_root_resource_template(bytes: &[u8]) -> Option<AcpiPciRootResources> {
    let mut bus_range = None;
    let mut windows = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let (kind, body_start, body_len) = if tag & 0x80 != 0 {
            let length = bytes.get(offset + 1..offset + 3)?;
            (
                tag & 0x7f,
                offset.checked_add(3)?,
                usize::from(u16::from_le_bytes([length[0], length[1]])),
            )
        } else {
            (
                (tag >> 3) & 0x0f,
                offset.checked_add(1)?,
                usize::from(tag & 0x07),
            )
        };
        let body_end = body_start.checked_add(body_len)?;
        let body = bytes.get(body_start..body_end)?;
        if tag & 0x80 != 0 {
            if !parse_pci_root_large_resource(kind, body, &mut bus_range, &mut windows) {
                return None;
            }
        } else {
            if kind == 0x0f {
                let checksum_valid = body.len() == 1
                    && body_end == bytes.len()
                    && (body[0] == 0
                        || bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0);
                let (bus_start, bus_end) = bus_range?;
                return checksum_valid.then_some(AcpiPciRootResources {
                    bus_start,
                    bus_end,
                    windows,
                });
            }
            let mut ignored: Vec<DeviceResource> = Vec::new();
            let valid = match kind {
                // These are resources consumed by the root itself, not downstream windows.
                0x04 => parse_small_irq_resource(body, &mut ignored),
                0x08 => parse_io_resource(body, &mut ignored),
                0x09 => parse_fixed_io_resource(body, &mut ignored),
                0x0e => !body.is_empty(),
                _ => false,
            };
            if !valid {
                return None;
            }
        }
        offset = body_end;
    }
    None
}

fn parse_pci_root_large_resource(
    kind: u8,
    body: &[u8],
    bus_range: &mut Option<(u8, u8)>,
    windows: &mut Vec<pci::AcpiPciRootWindow>,
) -> bool {
    match kind {
        0x01 => {
            let mut ignored = Vec::new();
            parse_memory24_resource(body, &mut ignored)
        }
        0x04 => true,
        0x05 => {
            let mut ignored = Vec::new();
            parse_memory_range_resource(body, 4, &mut ignored)
        }
        0x06 => {
            let mut ignored = Vec::new();
            parse_large_resource(kind, body, &mut ignored)
        }
        0x07 => parse_pci_root_address_resource(body, 4, bus_range, windows),
        0x08 => parse_pci_root_address_resource(body, 2, bus_range, windows),
        0x09 => {
            let mut ignored = Vec::new();
            parse_extended_irq_resource(body, &mut ignored)
        }
        0x0a => parse_pci_root_address_resource(body, 8, bus_range, windows),
        0x0b => parse_pci_root_extended_address_resource(body, bus_range, windows),
        _ => false,
    }
}

fn parse_pci_root_address_resource(
    body: &[u8],
    field_width: usize,
    bus_range: &mut Option<(u8, u8)>,
    windows: &mut Vec<pci::AcpiPciRootWindow>,
) -> bool {
    let fixed_len = match field_width
        .checked_mul(5)
        .and_then(|size| size.checked_add(3))
    {
        Some(length) => length,
        None => return false,
    };
    if body.len() < fixed_len || !valid_resource_source(&body[fixed_len..]) {
        return false;
    }
    parse_pci_root_address_fields(
        false,
        field_width,
        body[0],
        body[1],
        body[2],
        read_uint_le(body, 3, field_width),
        read_uint_le(body, 3 + field_width, field_width),
        read_uint_le(body, 3 + field_width * 2, field_width),
        read_uint_le(body, 3 + field_width * 3, field_width),
        read_uint_le(body, 3 + field_width * 4, field_width),
        Some(0),
        bus_range,
        windows,
    )
}

fn parse_pci_root_extended_address_resource(
    body: &[u8],
    bus_range: &mut Option<(u8, u8)>,
    windows: &mut Vec<pci::AcpiPciRootWindow>,
) -> bool {
    if body.len() != 53 || body[3] != 1 || body[4] != 0 {
        return false;
    }
    parse_pci_root_address_fields(
        true,
        8,
        body[0],
        body[1],
        body[2],
        read_uint_le(body, 5, 8),
        read_uint_le(body, 13, 8),
        read_uint_le(body, 21, 8),
        read_uint_le(body, 29, 8),
        read_uint_le(body, 37, 8),
        read_uint_le(body, 45, 8),
        bus_range,
        windows,
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_pci_root_address_fields(
    extended: bool,
    field_width: usize,
    resource_type: u8,
    general_flags: u8,
    type_flags: u8,
    granularity: Option<u64>,
    minimum: Option<u64>,
    maximum: Option<u64>,
    translation: Option<u64>,
    length: Option<u64>,
    type_attributes: Option<u64>,
    bus_range: &mut Option<(u8, u8)>,
    windows: &mut Vec<pci::AcpiPciRootWindow>,
) -> bool {
    let Some(type_attributes) = type_attributes else {
        return false;
    };
    if resource_type >= 0xc0 {
        return general_flags & 0xf0 == 0 && (!extended || type_attributes == 0);
    }
    if resource_type > 2
        || general_flags & 0xf0 != 0
        || (extended && resource_type != 0 && type_attributes != 0)
        || (extended && resource_type == 0 && type_attributes & !EFI_MEMORY_ATTRIBUTES_ALLOWED != 0)
    {
        return false;
    }
    let (Some(granularity), Some(minimum), Some(maximum), Some(translation), Some(length)) =
        (granularity, minimum, maximum, translation, length)
    else {
        return false;
    };
    if granularity != 0
        || length == 0
        || general_flags & 0x0c != 0x0c
        || maximum
            .checked_sub(minimum)
            .and_then(|span| span.checked_add(1))
            != Some(length)
    {
        return false;
    }
    if !address_type_flags_valid(resource_type, type_flags) {
        return false;
    }
    // Ordinary Word/DWord/QWord descriptors define bit 0 as ignored. Only the
    // Extended Address Space descriptor gives it Consumer/Producer semantics.
    if extended && general_flags & 1 != 0 {
        return true;
    }
    // Legal subtractive, memory-to-I/O, sparse-I/O, and special-memory descriptors cannot
    // be projected into the generic finite window model. Preserve the rest of this root's
    // `_CRS` rather than treating an unsupported-but-valid descriptor as malformed.
    if general_flags & 2 != 0 {
        return true;
    }

    if resource_type == 2 {
        if translation != 0 || bus_range.is_some() {
            return false;
        }
        let (Ok(start), Ok(end)) = (u8::try_from(minimum), u8::try_from(maximum)) else {
            return false;
        };
        *bus_range = Some((start, end));
        return true;
    }

    if !address_space_is_representable(resource_type, type_flags) {
        return true;
    }

    let Some(cpu_start) = translated_address_start(minimum, translation, length, field_width)
    else {
        return false;
    };
    let (Ok(cpu_start), Ok(size)) = (usize::try_from(cpu_start), usize::try_from(length)) else {
        return false;
    };
    let space = match resource_type {
        0 => {
            // Extended descriptors require `_MEM` cacheability bits to be ignored.
            if (!extended && type_flags & 0x06 == 0x06)
                || (extended && type_attributes & EFI_MEMORY_WC != 0)
            {
                pci::AcpiPciWindowSpace::PrefetchableMemory
            } else {
                pci::AcpiPciWindowSpace::Memory
            }
        }
        1 => pci::AcpiPciWindowSpace::Io,
        _ => return false,
    };
    let candidate = pci::AcpiPciRootWindow {
        space,
        pci_start: minimum,
        cpu_start,
        size,
    };
    if pci_root_window_overlaps(windows, candidate) {
        return false;
    }
    windows.push(candidate);
    true
}

fn pci_root_window_overlaps(
    windows: &[pci::AcpiPciRootWindow],
    candidate: pci::AcpiPciRootWindow,
) -> bool {
    windows.iter().copied().any(|existing| {
        let same_space = matches!(
            (existing.space, candidate.space),
            (pci::AcpiPciWindowSpace::Io, pci::AcpiPciWindowSpace::Io)
                | (
                    pci::AcpiPciWindowSpace::Memory | pci::AcpiPciWindowSpace::PrefetchableMemory,
                    pci::AcpiPciWindowSpace::Memory | pci::AcpiPciWindowSpace::PrefetchableMemory
                )
        );
        same_space
            && (range_u64_overlaps(
                existing.pci_start,
                existing.size,
                candidate.pci_start,
                candidate.size,
            ) || range_usize_overlaps(
                existing.cpu_start,
                existing.size,
                candidate.cpu_start,
                candidate.size,
            ))
    })
}

fn range_u64_overlaps(
    left_start: u64,
    left_size: usize,
    right_start: u64,
    right_size: usize,
) -> bool {
    let (Ok(left_size), Ok(right_size)) = (u64::try_from(left_size), u64::try_from(right_size))
    else {
        return true;
    };
    let (Some(left_end), Some(right_end)) = (
        left_start.checked_add(left_size),
        right_start.checked_add(right_size),
    ) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

fn range_usize_overlaps(
    left_start: usize,
    left_size: usize,
    right_start: usize,
    right_size: usize,
) -> bool {
    let (Some(left_end), Some(right_end)) = (
        left_start.checked_add(left_size),
        right_start.checked_add(right_size),
    ) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

fn acpi_pci_prt_routes(runtime: &mut AmlRuntime, path: &AmlName) -> Vec<pci::AcpiPciIrqRoute> {
    let prt_path = acpi_child_name(path, "_PRT");
    let object = match evaluate_aml_object(runtime, &prt_path) {
        Ok(object) => object,
        Err(error) if aml_error_is_missing(&error) => return Vec::new(),
        Err(error) => {
            printk!(
                "[kernel-start][acpi] failed to evaluate _PRT for {}: {:?}",
                path,
                error
            );
            return Vec::new();
        }
    };
    let routes = parse_pci_prt(&object, |source, source_index| {
        let relative = AmlName::from_str(source).ok()?;
        let resolved = runtime
            .context
            .namespace
            .search_for_level(&relative, &prt_path)
            .ok()?;
        if !matches!(
            runtime.context.namespace.get_by_path(&resolved),
            Ok(AmlValue::Device)
        ) {
            return None;
        }
        acpi_pci_link_current_irq(runtime, &resolved, source_index)
    });
    if routes.is_none() {
        printk!("[kernel-start][acpi] malformed _PRT for {}", path);
    }
    routes.unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AcpiPciIrqTarget {
    gsi: u32,
    trigger: general::dev::irq::IrqTrigger,
    polarity: general::dev::irq::IrqPolarity,
    sharing: general::dev::irq::IrqSharing,
}

const fn direct_pci_intx_target(gsi: u32) -> AcpiPciIrqTarget {
    AcpiPciIrqTarget {
        gsi,
        trigger: general::dev::irq::IrqTrigger::Level,
        polarity: general::dev::irq::IrqPolarity::Low,
        sharing: general::dev::irq::IrqSharing::Shared,
    }
}

fn parse_pci_prt(
    object: &AmlValue,
    mut resolve_link: impl FnMut(&str, u32) -> Option<AcpiPciIrqTarget>,
) -> Option<Vec<pci::AcpiPciIrqRoute>> {
    let AmlValue::Package(entries) = object else {
        return None;
    };
    let mut keys = Vec::new();
    keys.try_reserve(entries.len()).ok()?;
    let mut routes = Vec::new();
    routes.try_reserve(entries.len()).ok()?;
    for entry in entries {
        let AmlValue::Package(fields) = entry else {
            return None;
        };
        let [address, pin, source, source_index] = fields.as_slice() else {
            return None;
        };
        let address = aml_literal_integer(address)?;
        let pin = u8::try_from(aml_literal_integer(pin)?).ok()?;
        let source_index = u32::try_from(aml_literal_integer(source_index)?).ok()?;
        if address > u64::from(u32::MAX) || pin > 3 {
            return None;
        }
        let device = ((address >> 16) & 0xffff) as u16;
        let function = (address & 0xffff) as u16;
        if device >= u16::from(general::dev::pci::PCI_DEVICES_PER_BUS)
            || (function != 0xffff
                && function >= u16::from(general::dev::pci::PCI_FUNCTIONS_PER_DEVICE))
        {
            return None;
        }
        let target = match source {
            AmlValue::Integer(0) => direct_pci_intx_target(source_index),
            AmlValue::String(source) if !source.is_empty() => resolve_link(source, source_index)?,
            _ => return None,
        };
        let key = (device, function, pin);
        if keys
            .iter()
            .any(|&(existing_device, existing_function, existing_pin)| {
                existing_device == device
                    && existing_pin == pin
                    && (existing_function == function
                        || existing_function == 0xffff
                        || function == 0xffff)
            })
        {
            return None;
        }
        keys.push(key);
        routes.push(pci::AcpiPciIrqRoute {
            device: u8::try_from(device).ok()?,
            function: (function != 0xffff)
                .then(|| u8::try_from(function).ok())
                .flatten(),
            pin,
            gsi: target.gsi,
            trigger: target.trigger,
            polarity: target.polarity,
            sharing: target.sharing,
        });
    }
    Some(routes)
}

fn acpi_pci_link_current_irq(
    runtime: &mut AmlRuntime,
    path: &AmlName,
    resource_index: u32,
) -> Option<AcpiPciIrqTarget> {
    if !acpi_device_is_usable(runtime, path)
        || !acpi_device_ids(runtime, path)
            .iter()
            .any(|id| id == ACPI_HID_PNP0C0F)
    {
        return None;
    }
    let AmlValue::Buffer(bytes) =
        evaluate_aml_object(runtime, &acpi_child_name(path, "_CRS")).ok()?
    else {
        return None;
    };
    parse_pci_link_current_irq(&bytes.lock(), resource_index)
}

fn parse_pci_link_current_irq(bytes: &[u8], resource_index: u32) -> Option<AcpiPciIrqTarget> {
    let mut offset = 0usize;
    let mut index = 0u32;
    let mut selected = None;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let (kind, body_start, body_len) = if tag & 0x80 != 0 {
            let length = bytes.get(offset + 1..offset + 3)?;
            (
                tag & 0x7f,
                offset.checked_add(3)?,
                usize::from(u16::from_le_bytes([length[0], length[1]])),
            )
        } else {
            (
                (tag >> 3) & 0x0f,
                offset.checked_add(1)?,
                usize::from(tag & 0x07),
            )
        };
        let body_end = body_start.checked_add(body_len)?;
        let body = bytes.get(body_start..body_end)?;
        if tag & 0x80 == 0 && kind == 0x0f {
            let checksum_valid = body.len() == 1
                && body_end == bytes.len()
                && (body[0] == 0
                    || bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0);
            return if checksum_valid { selected } else { None };
        }
        if index == resource_index {
            let mut resources = Vec::new();
            let valid = if tag & 0x80 != 0 {
                kind == 0x09 && parse_extended_irq_resource(body, &mut resources)
            } else {
                kind == 0x04 && parse_small_irq_resource(body, &mut resources)
            };
            if !valid || resources.len() != 1 {
                return None;
            }
            let irq = resources[0].as_irq()?;
            let [gsi] = irq.cells() else {
                return None;
            };
            let attributes = irq.attributes();
            selected = Some(AcpiPciIrqTarget {
                gsi: *gsi,
                trigger: match attributes.trigger? {
                    IrqTrigger::Edge => general::dev::irq::IrqTrigger::Edge,
                    IrqTrigger::Level => general::dev::irq::IrqTrigger::Level,
                },
                polarity: match attributes.polarity? {
                    IrqPolarity::ActiveHigh => general::dev::irq::IrqPolarity::High,
                    IrqPolarity::ActiveLow => general::dev::irq::IrqPolarity::Low,
                },
                sharing: match attributes.sharing? {
                    IrqSharing::Exclusive => general::dev::irq::IrqSharing::Exclusive,
                    IrqSharing::Shared => general::dev::irq::IrqSharing::Shared,
                },
            });
        }
        index = index.checked_add(1)?;
        offset = body_end;
    }
    None
}

fn aml_literal_integer(value: &AmlValue) -> Option<u64> {
    match value {
        AmlValue::Integer(value) => Some(*value),
        AmlValue::Boolean(value) => Some(if *value { u64::MAX } else { 0 }),
        _ => None,
    }
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
                return (Vec::new(), false);
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
            return (Vec::new(), false);
        };
        let Some(body) = bytes.get(body_start..body_end) else {
            return (Vec::new(), false);
        };

        if tag & 0x80 != 0 {
            if !parse_large_resource(kind, body, &mut resources) {
                return (Vec::new(), false);
            }
        } else {
            if kind == 0x0f {
                let checksum_valid = body.len() == 1
                    && body_end == bytes.len()
                    && (body[0] == 0
                        || bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0);
                return if checksum_valid {
                    (resources, true)
                } else {
                    (Vec::new(), false)
                };
            }
            let valid = match kind {
                0x04 => parse_small_irq_resource(body, &mut resources),
                0x08 => parse_io_resource(body, &mut resources),
                0x09 => parse_fixed_io_resource(body, &mut resources),
                // Vendor-defined descriptors are opaque but do not alter the meaning of the
                // resources we publish. Reserved and dependency/DMA descriptors cannot be
                // represented by DeviceResource without losing allocation semantics.
                0x0e => !body.is_empty(),
                _ => false,
            };
            if !valid {
                return (Vec::new(), false);
            }
        }
        offset = body_end;
    }
    // A resource template is complete only after its EndTag descriptor.  An
    // otherwise valid prefix must not make a truncated firmware buffer look
    // usable to device discovery.
    (Vec::new(), false)
}

fn parse_large_resource(kind: u8, body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    match kind {
        // Memory24 stores address bits 23:8 and lengths in 256-byte blocks.
        0x01 => parse_memory24_resource(body, resources),
        // Generic register, GPIO, serial-bus and pin descriptors need richer typed resources.
        // Reject them instead of silently binding a device with an incomplete _CRS.
        0x02 | 0x0c..=0x12 => false,
        // Vendor-defined descriptor.
        0x04 => true,
        // 32-bit memory range.
        0x05 => parse_memory_range_resource(body, 4, resources),
        // 32-bit fixed memory range: information byte, base, length.
        0x06 => {
            if body.len() != 9 {
                return false;
            }
            if let (Some(base), Some(length)) = (read_u32_le(body, 1), read_u32_le(body, 5)) {
                if length == 0 || base.checked_add(length - 1).is_none() {
                    return false;
                }
                resources.push(DeviceResource::mmio(base as usize, length as usize));
                return true;
            }
            false
        }
        // DWord, Word, and QWord address-space descriptors respectively.
        0x07 => parse_address_space_resource(body, 4, resources),
        0x08 => parse_address_space_resource(body, 2, resources),
        0x0a => parse_address_space_resource(body, 8, resources),
        0x0b => parse_extended_address_space_resource(body, resources),
        0x09 => parse_extended_irq_resource(body, resources),
        _ => false,
    }
}

fn parse_address_space_resource(
    body: &[u8],
    field_width: usize,
    resources: &mut Vec<DeviceResource>,
) -> bool {
    let fixed_len = 3 + field_width * 5;
    if body.len() < fixed_len || !valid_resource_source(&body[fixed_len..]) {
        return false;
    }
    let resource_type = body[0];
    let general_flags = body[1];
    if general_flags & 0xf0 != 0 {
        return false;
    }
    if resource_type > 2 {
        return resource_type >= 0xc0;
    }
    let type_flags = body[2];
    if !address_type_flags_valid(resource_type, type_flags) {
        return false;
    }
    let granularity = read_uint_le(body, 3, field_width);
    let minimum = read_uint_le(body, 3 + field_width, field_width);
    let maximum = read_uint_le(body, 3 + field_width * 2, field_width);
    let translation = read_uint_le(body, 3 + field_width * 3, field_width);
    let length = read_uint_le(body, 3 + field_width * 4, field_width);
    let (Some(granularity), Some(minimum), Some(maximum), Some(translation), Some(length)) =
        (granularity, minimum, maximum, translation, length)
    else {
        return false;
    };
    if length == 0
        || general_flags & 0x0c != 0x0c
        || granularity != 0
        || maximum
            .checked_sub(minimum)
            .and_then(|span| span.checked_add(1))
            != Some(length)
    {
        return false;
    }
    if general_flags & 2 != 0 || !address_space_is_representable(resource_type, type_flags) {
        return true;
    }
    let Some(base) = translated_address_start(minimum, translation, length, field_width) else {
        return false;
    };
    match resource_type {
        0 => match (usize::try_from(base), usize::try_from(length)) {
            (Ok(base), Ok(length)) => {
                resources.push(DeviceResource::mmio(base, length));
                true
            }
            _ => false,
        },
        1 => match (u16::try_from(base), u16::try_from(length)) {
            (Ok(base), Ok(length)) if length != 0 && base.checked_add(length - 1).is_some() => {
                resources.push(DeviceResource::io_port(base, length));
                true
            }
            _ => false,
        },
        2 => true,
        _ => false,
    }
}

fn parse_extended_address_space_resource(body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    if body.len() != 53 || body[3] != 1 || body[4] != 0 {
        return false;
    }
    let resource_type = body[0];
    let general_flags = body[1];
    let type_flags = body[2];
    let Some(type_attributes) = read_uint_le(body, 45, 8) else {
        return false;
    };
    if resource_type > 2 || general_flags & 0xf0 != 0 {
        return resource_type >= 0xc0 && general_flags & 0xf0 == 0 && type_attributes == 0;
    }
    if !address_type_flags_valid(resource_type, type_flags)
        || (resource_type == 0 && type_attributes & !EFI_MEMORY_ATTRIBUTES_ALLOWED != 0)
        || (resource_type != 0 && type_attributes != 0)
    {
        return false;
    }
    let Some(granularity) = read_uint_le(body, 5, 8) else {
        return false;
    };
    let Some(minimum) = read_uint_le(body, 13, 8) else {
        return false;
    };
    let Some(maximum) = read_uint_le(body, 21, 8) else {
        return false;
    };
    let Some(translation) = read_uint_le(body, 29, 8) else {
        return false;
    };
    let Some(length) = read_uint_le(body, 37, 8) else {
        return false;
    };
    if length == 0
        || general_flags & 0x0c != 0x0c
        || granularity != 0
        || maximum
            .checked_sub(minimum)
            .and_then(|span| span.checked_add(1))
            != Some(length)
    {
        return false;
    }
    if general_flags & 1 == 0 {
        return true;
    }
    if general_flags & 2 != 0 {
        return true;
    }
    if !address_space_is_representable(resource_type, type_flags) {
        return true;
    }
    let Some(base) = translated_address_start(minimum, translation, length, 8) else {
        return false;
    };
    match resource_type {
        0 => match (usize::try_from(base), usize::try_from(length)) {
            (Ok(base), Ok(length)) => {
                resources.push(DeviceResource::mmio(base, length));
                true
            }
            _ => false,
        },
        1 => match (u16::try_from(base), u16::try_from(length)) {
            (Ok(base), Ok(length)) if base.checked_add(length - 1).is_some() => {
                resources.push(DeviceResource::io_port(base, length));
                true
            }
            _ => false,
        },
        2 => true,
        _ => false,
    }
}

fn address_type_flags_valid(resource_type: u8, type_flags: u8) -> bool {
    match resource_type {
        // Memory bits 7:6 are reserved. The lower bits describe writeability,
        // cacheability, range type, and memory-to-I/O translation.
        0 => type_flags & 0xc0 == 0,
        // I/O bits 7:6 and 3:2 are reserved; `_RNG=0` is reserved as well.
        1 => type_flags & 0xcc == 0 && type_flags & 0x03 != 0,
        2 => type_flags == 0,
        _ => false,
    }
}

fn address_space_is_representable(resource_type: u8, type_flags: u8) -> bool {
    match resource_type {
        // DeviceResource::Mmio cannot express reserved/ACPI/NVS ranges or
        // memory-to-I/O translation.
        0 => type_flags & 0x38 == 0,
        // DeviceResource::IoPort cannot preserve translation or sparse decoding.
        1 => type_flags & 0x30 == 0,
        2 => true,
        _ => false,
    }
}

fn valid_resource_source(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let Some(source) = bytes.get(1..) else {
        return false;
    };
    source.len() >= 2
        && source.last() == Some(&0)
        && !source[..source.len() - 1].contains(&0)
        && core::str::from_utf8(&source[..source.len() - 1]).is_ok()
}

fn parse_memory24_resource(body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    if body.len() != 9 {
        return false;
    }
    let Some(minimum) = read_u16_le(body, 1).map(|value| usize::from(value) << 8) else {
        return false;
    };
    let Some(maximum) = read_u16_le(body, 3).map(|value| usize::from(value) << 8) else {
        return false;
    };
    let Some(length) = read_u16_le(body, 7).map(|value| usize::from(value) << 8) else {
        return false;
    };
    if length == 0
        || minimum != maximum
        || maximum
            .checked_add(length - 1)
            .is_none_or(|end| end > 0x00ff_ffff)
    {
        return false;
    }
    resources.push(DeviceResource::mmio(minimum, length));
    true
}

fn parse_memory_range_resource(
    body: &[u8],
    field_width: usize,
    resources: &mut Vec<DeviceResource>,
) -> bool {
    if body.len() != 1 + field_width * 4 {
        return false;
    }
    let minimum = read_uint_le(body, 1, field_width);
    let maximum = read_uint_le(body, 1 + field_width, field_width);
    let length = read_uint_le(body, 1 + field_width * 3, field_width);
    match (minimum, maximum, length) {
        (Some(minimum), Some(maximum), Some(length))
            if length != 0
                && minimum == maximum
                && maximum
                    .checked_add(length - 1)
                    .is_some_and(|end| end <= u64::from(u32::MAX)) =>
        {
            match (usize::try_from(minimum), usize::try_from(length)) {
                (Ok(base), Ok(length)) => {
                    resources.push(DeviceResource::mmio(base, length));
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn parse_io_resource(body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    // Decode10 aliases every 0x400 bytes and cannot be represented by a single IoPort range.
    if body.len() != 7 || body[0] != 1 {
        return false;
    }
    let Some(minimum) = read_u16_le(body, 1) else {
        return false;
    };
    let Some(maximum) = read_u16_le(body, 3) else {
        return false;
    };
    let length = u16::from(body[6]);
    if length == 0 || minimum != maximum || minimum.checked_add(length - 1).is_none() {
        return false;
    }
    resources.push(DeviceResource::io_port(minimum, length));
    true
}

fn parse_fixed_io_resource(body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    if body.len() != 3 || body[1] & 0xfc != 0 {
        return false;
    }
    let Some(base) = read_u16_le(body, 0) else {
        return false;
    };
    let length = u16::from(body[2]);
    if length == 0 || base.checked_add(length - 1).is_none() {
        return false;
    }
    resources.push(DeviceResource::io_port(base, length));
    true
}

fn parse_small_irq_resource(body: &[u8], resources: &mut Vec<DeviceResource>) -> bool {
    if !matches!(body.len(), 2 | 3) {
        return false;
    }
    let Some(mask_bytes) = body.get(..2) else {
        return false;
    };
    let mask = u16::from_le_bytes([mask_bytes[0], mask_bytes[1]]);
    let information = body.get(2).copied().unwrap_or(0x01);
    let edge = information & 0x01 != 0;
    let active_low = information & 0x08 != 0;
    if mask.count_ones() != 1 || information & 0xc0 != 0 || edge == active_low {
        return false;
    }
    for irq in 0..16 {
        if mask & (1 << irq) != 0 {
            resources.push(acpi_irq_resource(AcpiIrqDescriptor {
                irq,
                trigger: if edge {
                    IrqTrigger::Edge
                } else {
                    IrqTrigger::Level
                },
                polarity: if active_low {
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
    let Some(fixed_len) = usize::from(count)
        .checked_mul(4)
        .and_then(|size| size.checked_add(2))
    else {
        return false;
    };
    if count != 1
        || flags & 0xe0 != 0
        || body.len() < fixed_len
        || !valid_resource_source(&body[fixed_len..])
    {
        return false;
    }
    // ResourceSource selects a named interrupt controller. The generic ACPI GSI domain
    // cannot represent that namespace reference, so retain descriptor validity but do not
    // publish its number as a global GSI.
    if body.len() != fixed_len {
        return true;
    }
    if flags & 1 == 0 {
        return true;
    }
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

/// Address Space `_TRA` is added at the descriptor's N-bit width. Two's-complement
/// subtraction and high-bit positive offsets therefore share the same modulo-N operation.
fn translated_address_start(
    secondary_start: u64,
    translation: u64,
    length: u64,
    field_width: usize,
) -> Option<u64> {
    if length == 0 {
        return None;
    }
    let primary_max = match field_width {
        2 => u64::from(u16::MAX),
        4 => u64::from(u32::MAX),
        8 => u64::MAX,
        _ => return None,
    };
    if secondary_start > primary_max || translation > primary_max {
        return None;
    }
    let primary_start = secondary_start.wrapping_add(translation) & primary_max;
    let primary_end = primary_start.checked_add(length - 1)?;
    (primary_end <= primary_max).then_some(primary_start)
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

fn ensure_acpi_mode(
    fadt: Option<&general::firmware::acpi::AcpiFadtInfo>,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<general::StartAcpiIoOps>,
) -> bool {
    let Some(fadt) = fadt else {
        return false;
    };
    if fadt.flags & (1 << 20) != 0 {
        return true;
    }
    let Some(pm1a_control) = fadt.pm1a_control else {
        return false;
    };
    if read_acpi_fixed_register(pm1a_control, device_mmio_to_virt, io_ops)
        .is_some_and(|value| value & 1 != 0)
    {
        return true;
    }
    let (Some(io_ops), Ok(smi_port)) = (io_ops, u16::try_from(fadt.smi_command_port)) else {
        return false;
    };
    if smi_port == 0 || fadt.acpi_enable == 0 {
        return false;
    }
    (io_ops.write_u8)(smi_port, fadt.acpi_enable);
    for _ in 0..100_000 {
        if read_acpi_fixed_register(pm1a_control, device_mmio_to_virt, Some(io_ops))
            .is_some_and(|value| value & 1 != 0)
        {
            printk!("[kernel-start][acpi] firmware entered ACPI mode");
            return true;
        }
        core::hint::spin_loop();
    }
    printk!("[kernel-start][acpi] timed out waiting for PM1 SCI_EN");
    false
}

fn read_acpi_fixed_register(
    gas: general::firmware::acpi::AcpiGenericAddress,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<general::StartAcpiIoOps>,
) -> Option<u64> {
    let width = PowerAccessWidth::from_bits(gas.bit_width, gas.access_size)?;
    match gas.address_space {
        general::firmware::acpi::AcpiAddressSpace::SystemMemory => {
            let physical = usize::try_from(gas.address).ok()?;
            let virtual_address = device_mmio_to_virt(physical);
            if virtual_address == 0 || virtual_address % width.bytes() != 0 {
                return None;
            }
            // Safety: the validated FADT GAS names a naturally aligned fixed-hardware
            // register and the architecture supplied its device mapping.
            Some(unsafe {
                match width {
                    PowerAccessWidth::U8 => {
                        core::ptr::read_volatile(virtual_address as *const u8) as u64
                    }
                    PowerAccessWidth::U16 => {
                        core::ptr::read_volatile(virtual_address as *const u16) as u64
                    }
                    PowerAccessWidth::U32 => {
                        core::ptr::read_volatile(virtual_address as *const u32) as u64
                    }
                    PowerAccessWidth::U64 => {
                        core::ptr::read_volatile(virtual_address as *const u64)
                    }
                }
            })
        }
        general::firmware::acpi::AcpiAddressSpace::SystemIo => {
            let port = u16::try_from(gas.address).ok()?;
            let io_ops = io_ops?;
            Some(match width {
                PowerAccessWidth::U8 => (io_ops.read_u8)(port) as u64,
                PowerAccessWidth::U16 => (io_ops.read_u16)(port) as u64,
                PowerAccessWidth::U32 => (io_ops.read_u32)(port) as u64,
                PowerAccessWidth::U64 => return None,
            })
        }
        _ => None,
    }
}

fn parse_power_controls(
    fadt: Option<&general::firmware::acpi::AcpiFadtInfo>,
    aml_runtime: Option<&mut AmlRuntime>,
    acpi_mode_ready: bool,
) -> PowerControlInfo {
    let shutdown = acpi_shutdown_control(fadt, aml_runtime, acpi_mode_ready);
    let reboot = acpi_reboot_control(fadt);

    printk!(
        "[kernel-start][acpi] power controls: shutdown={} reboot={}",
        shutdown.is_some() as usize,
        reboot.is_some() as usize
    );

    PowerControlInfo { shutdown, reboot }
}

fn acpi_reboot_control(
    fadt: Option<&general::firmware::acpi::AcpiFadtInfo>,
) -> Option<PowerControlMethod> {
    let fadt = fadt?;
    let register = match fadt
        .reset_register
        .and_then(|gas| power_register_from_acpi_gas(gas, true))
    {
        Some(register) => register,
        None => {
            printk!("[kernel-start][acpi] unsupported FADT reset register");
            return None;
        }
    };
    let value = u64::from(fadt.reset_value);

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
    fadt: Option<&general::firmware::acpi::AcpiFadtInfo>,
    aml_runtime: Option<&mut AmlRuntime>,
    acpi_mode_ready: bool,
) -> Option<PowerControlMethod> {
    let fadt = fadt?;

    let (sleep_type_a, sleep_type_b) = match aml_runtime.and_then(acpi_s5_sleep_types) {
        Some(types) => types,
        None => {
            printk!("[kernel-start][acpi] ACPI _S5 sleep type not found");
            return None;
        }
    };

    if fadt.flags & (1 << 20) != 0 {
        let sleep_control = match fadt
            .sleep_control
            .and_then(|gas| power_register_from_acpi_gas(gas, false))
        {
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

    if !acpi_mode_ready {
        printk!("[kernel-start][acpi] legacy shutdown disabled because SCI_EN is clear");
        return None;
    }

    let pm1a_control = match fadt
        .pm1a_control
        .and_then(|gas| power_register_from_acpi_gas(gas, false))
    {
        Some(register) => register,
        None => {
            printk!("[kernel-start][acpi] PM1a control register unavailable");
            return None;
        }
    };
    let pm1b_control = fadt
        .pm1b_control
        .and_then(|gas| power_register_from_acpi_gas(gas, false));

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

fn power_register_from_acpi_gas(
    gas: general::firmware::acpi::AcpiGenericAddress,
    allow_pci_config: bool,
) -> Option<PowerRegister> {
    if gas.address == 0 || gas.bit_offset != 0 {
        return None;
    }
    let space = match gas.address_space {
        general::firmware::acpi::AcpiAddressSpace::SystemMemory => PowerRegisterSpace::SystemMemory,
        general::firmware::acpi::AcpiAddressSpace::SystemIo => PowerRegisterSpace::SystemIo,
        general::firmware::acpi::AcpiAddressSpace::PciConfig if allow_pci_config => {
            let encoded = gas.address;
            if encoded >> 48 != 0 {
                return None;
            }
            let device = u8::try_from((encoded >> 32) & 0xffff).ok()?;
            let function = u8::try_from((encoded >> 16) & 0xffff).ok()?;
            if device >= 32 || function >= 8 {
                return None;
            }
            PowerRegisterSpace::PciConfig {
                segment: 0,
                bus: 0,
                device,
                function,
            }
        }
        _ => return None,
    };
    let access_width = PowerAccessWidth::from_bits(gas.bit_width, gas.access_size)?;
    let address = match space {
        PowerRegisterSpace::PciConfig { .. } => usize::from(gas.address as u16),
        _ => usize::try_from(gas.address).ok()?,
    };
    Some(PowerRegister {
        space,
        address,
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
    if sleep_type_a > 7 || sleep_type_b > 7 {
        printk!("[kernel-start][acpi] _S5 sleep type exceeds the 3-bit register field");
        return None;
    }
    printk!(
        "[kernel-start][acpi] _S5 sleep types: a={} b={}",
        sleep_type_a,
        sleep_type_b
    );
    Some((sleep_type_a, sleep_type_b))
}

fn serial_device_from_spcr(
    mappings: &'static [FirmwareTableMapping],
    madt: Option<&general::firmware::acpi::AcpiMadtInfo>,
) -> Option<FirmwareSerialDevice> {
    let mut parsed = None;
    for mapping in mappings {
        if mapping.virtual_start == 0
            || mapping.length < 36
            || mapping.virtual_start.checked_add(mapping.length).is_none()
        {
            continue;
        }
        // SAFETY: `StartContext::validate` verifies every snapshot mapping before ACPI
        // initialization. This local check keeps the parser safe when called by tests directly.
        let bytes =
            unsafe { slice::from_raw_parts(mapping.virtual_start as *const u8, mapping.length) };
        if bytes.get(..4) != Some(b"SPCR") {
            continue;
        }
        if parsed.is_some() {
            printk!("[kernel-start][acpi] duplicate SPCR table ignored");
            continue;
        }
        match general::firmware::acpi::parse_spcr(bytes) {
            Ok(info) => parsed = Some(info),
            Err(error) => printk!("[kernel-start][acpi] rejected SPCR table: {:?}", error),
        }
    }
    let spcr = parsed?;
    let interface = SpcrInterfaceType::from(spcr.interface_type);
    if !spcr_interface_is_16550_compatible(interface) {
        printk!(
            "[kernel-start][acpi] SPCR interface {:?} is not ns16550-compatible",
            interface,
        );
        return None;
    }
    let base_address = spcr.base;
    if !matches!(
        base_address.address_space,
        general::firmware::acpi::AcpiAddressSpace::SystemMemory
            | general::firmware::acpi::AcpiAddressSpace::SystemIo
    ) || base_address.address == 0
    {
        printk!(
            "[kernel-start][acpi] SPCR base address uses an unsupported space: {:?}",
            base_address,
        );
        return None;
    }

    let clock_hz = spcr.clock_hz;
    let baud = spcr.baud;
    let phys_addr = usize::try_from(base_address.address).ok()?;
    let register_space = match base_address.address_space {
        general::firmware::acpi::AcpiAddressSpace::SystemMemory => {
            FirmwareRegisterSpace::SystemMemory
        }
        general::firmware::acpi::AcpiAddressSpace::SystemIo
            if u16::try_from(base_address.address).is_ok() =>
        {
            FirmwareRegisterSpace::SystemIo
        }
        _ => return None,
    };
    let (reg_shift, reg_io_width, reg_size) = spcr_16550_register_layout(interface, base_address)?;
    let name = spcr
        .namespace
        .unwrap_or_else(|| alloc::format!("serial@{:#x}", phys_addr).into_boxed_str());

    if let Some(clock_hz) = clock_hz {
        printk!(
            "[kernel-start][acpi] SPCR serial: {} space={:?} address={:#x} clock={}Hz",
            name,
            register_space,
            phys_addr,
            clock_hz,
        );
    } else {
        printk!(
            "[kernel-start][acpi] SPCR serial: {} space={:?} address={:#x} clock=<firmware-configured>",
            name,
            register_space,
            phys_addr,
        );
    }

    let mut resources = Vec::new();
    match register_space {
        FirmwareRegisterSpace::SystemMemory => {
            resources.push(DeviceResource::mmio(phys_addr, reg_size))
        }
        FirmwareRegisterSpace::SystemIo => resources.push(DeviceResource::io_port(
            u16::try_from(phys_addr).ok()?,
            u16::try_from(reg_size).ok()?,
        )),
    }
    if let Some(gsi) = spcr.global_system_interrupt {
        resources.push(acpi_gsi_resource(gsi));
    } else if let Some(irq) = spcr.legacy_irq {
        resources.push(spcr_legacy_irq_resource(irq, madt));
    }

    Some(FirmwareSerialDevice {
        port: SerialPortInfo {
            name,
            register_space,
            phys_addr,
            reg_size: Some(reg_size),
            reg_shift: Some(reg_shift),
            reg_io_width: Some(reg_io_width),
            clock_hz,
            baud,
        },
        resources,
    })
}

fn spcr_16550_register_layout(
    interface: SpcrInterfaceType,
    gas: general::firmware::acpi::AcpiGenericAddress,
) -> Option<(u32, u32, usize)> {
    if gas.bit_offset != 0 {
        printk!(
            "[kernel-start][acpi] SPCR 16550 GAS has non-zero bit offset {}",
            gas.bit_offset
        );
        return None;
    }

    let (stride, access_width) = match interface {
        SpcrInterfaceType::Generic16550 => {
            let stride = usize::from(gas.bit_width).checked_div(8)?;
            if gas.bit_width == 0 || !gas.bit_width.is_multiple_of(8) || !stride.is_power_of_two() {
                printk!(
                    "[kernel-start][acpi] Generic16550 SPCR has invalid register width {}",
                    gas.bit_width
                );
                return None;
            }
            let access_width = match gas.access_size {
                1 => 1,
                2 => 2,
                3 => 4,
                _ => {
                    printk!(
                        "[kernel-start][acpi] Generic16550 SPCR has unsupported access size {}",
                        gas.access_size
                    );
                    return None;
                }
            };
            if access_width > stride {
                printk!(
                    "[kernel-start][acpi] Generic16550 SPCR access width {} exceeds stride {}",
                    access_width,
                    stride
                );
                return None;
            }
            (stride, access_width)
        }
        SpcrInterfaceType::Full16550 | SpcrInterfaceType::Full16450 => (1, 1),
        _ => return None,
    };
    let reg_shift = stride.trailing_zeros();
    let reg_size = 7usize.checked_mul(stride)?.checked_add(access_width)?;
    Some((reg_shift, access_width as u32, reg_size))
}

fn spcr_interface_is_16550_compatible(interface: SpcrInterfaceType) -> bool {
    matches!(
        interface,
        SpcrInterfaceType::Full16550
            | SpcrInterfaceType::Full16450
            | SpcrInterfaceType::Generic16550
    )
}

fn spcr_legacy_irq_resource(
    irq: u8,
    madt: Option<&general::firmware::acpi::AcpiMadtInfo>,
) -> DeviceResource {
    let Some(override_entry) = madt.and_then(|madt| {
        madt.interrupt_overrides
            .iter()
            .find(|entry| entry.bus == 0 && entry.source == irq)
    }) else {
        return acpi_gsi_resource(u32::from(irq));
    };
    use general::firmware::acpi::{AcpiInterruptPolarity, AcpiInterruptTrigger};
    acpi_irq_resource(AcpiIrqDescriptor {
        irq: override_entry.global_system_interrupt,
        trigger: match override_entry.attributes.trigger {
            AcpiInterruptTrigger::Level => IrqTrigger::Level,
            AcpiInterruptTrigger::Conforms | AcpiInterruptTrigger::Edge => IrqTrigger::Edge,
        },
        polarity: match override_entry.attributes.polarity {
            AcpiInterruptPolarity::ActiveLow => IrqPolarity::ActiveLow,
            AcpiInterruptPolarity::Conforms | AcpiInterruptPolarity::ActiveHigh => {
                IrqPolarity::ActiveHigh
            }
        },
        is_shared: false,
        is_wake_capable: false,
    })
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

    fn test_aml_runtime() -> AmlRuntime {
        AmlRuntime {
            context: AmlContext::new(
                Box::new(AcpiMapper::new(
                    identity_address,
                    &[],
                    StartAcpiHostOps::NONE,
                )),
                DebugVerbosity::None,
            ),
        }
    }

    #[ktest]
    fn aml_methods_are_evaluated_independently() {
        let mut runtime = test_aml_runtime();
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
            Ok(AmlValue::Integer(1))
        ));
    }

    #[ktest]
    fn static_aml_names_remain_available() {
        let mut runtime = test_aml_runtime();
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
        template.extend_from_slice(&[0x8a, 0x2b, 0x00, 0x00, 0x0c, 0x00]);
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
    fn malformed_resource_discards_valid_prefix() {
        let template = [
            0x86, 0x09, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x8a, 0x2b,
            0x00, 0x00,
        ];
        let (resources, complete) = parse_resource_template(&template);
        assert!(!complete);
        assert!(resources.is_empty());
    }

    #[ktest]
    fn resource_without_end_tag_is_incomplete() {
        let template = [
            0x86, 0x09, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
        ];
        let (resources, complete) = parse_resource_template(&template);
        assert!(!complete);
        assert!(resources.is_empty());
    }

    #[ktest]
    fn parses_extended_irq_polarity() {
        // Two edge-triggered Extended IRQs: bit 2 clear is active-high and set is active-low.
        let template = [
            0x89, 0x06, 0x00, 0x03, 0x01, 0x21, 0x00, 0x00, 0x00, 0x89, 0x06, 0x00, 0x07, 0x01,
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
    fn rejects_unrepresentable_irq_io_and_vendor_resources() {
        let mut resources = Vec::new();
        assert!(!parse_small_irq_resource(
            &[0x20, 0x00, 0x09],
            &mut resources,
        ));
        assert!(!parse_small_irq_resource(
            &[0x20, 0x00, 0x00],
            &mut resources,
        ));
        assert!(parse_small_irq_resource(
            &[0x20, 0x00, 0x08],
            &mut resources,
        ));
        resources.clear();
        assert!(parse_small_irq_resource(
            &[0x20, 0x00, 0x0e],
            &mut resources,
        ));
        assert_eq!(resources.len(), 1);

        resources.clear();
        assert!(!parse_extended_irq_resource(
            &[0x01, 0x02, 1, 0, 0, 0, 2, 0, 0, 0],
            &mut resources,
        ));
        assert!(parse_extended_irq_resource(
            &[0x01, 0x01, 1, 0, 0, 0, 0, b'X', 0],
            &mut resources,
        ));
        assert!(resources.is_empty());
        assert!(!parse_extended_irq_resource(
            &[0x01, 0x01, 1, 0, 0, 0, 0, b'X'],
            &mut resources,
        ));
        assert!(!parse_io_resource(&[0, 0, 0, 0, 0, 1, 1], &mut resources,));

        let (resources, complete) = parse_resource_template(&[0x70, 0x79, 0x00]);
        assert!(!complete);
        assert!(resources.is_empty());
    }

    #[ktest]
    fn rejects_unrepresentable_generic_address_type_flags() {
        let mut body = [0u8; 23];
        body[0] = 1;
        body[1] = 0x0c;
        body[7..11].copy_from_slice(&0u32.to_le_bytes());
        body[11..15].copy_from_slice(&0xffu32.to_le_bytes());
        body[19..23].copy_from_slice(&0x100u32.to_le_bytes());
        let mut resources = Vec::new();
        assert!(!parse_address_space_resource(&body, 4, &mut resources));

        body[0] = 0;
        body[2] = 0x40;
        assert!(!parse_address_space_resource(&body, 4, &mut resources));
        assert!(!parse_address_space_resource(&[0xc0], 4, &mut resources));

        body[2] = 0x18;
        resources.clear();
        assert!(parse_address_space_resource(&body, 4, &mut resources));
        assert!(resources.is_empty());

        body[0] = 1;
        body[2] = 0x31;
        assert!(parse_address_space_resource(&body, 4, &mut resources));
        assert!(resources.is_empty());
    }

    #[ktest]
    fn generic_extended_memory_accepts_known_uefi_attributes() {
        let mut body = [0u8; 53];
        body[0] = 0;
        body[1] = 0x0d;
        body[3] = 1;
        body[13..21].copy_from_slice(&0x1000u64.to_le_bytes());
        body[21..29].copy_from_slice(&0x1fffu64.to_le_bytes());
        body[37..45].copy_from_slice(&0x1000u64.to_le_bytes());
        body[45..53].copy_from_slice(&(0x08u64 | 0x4000).to_le_bytes());
        let mut resources = Vec::new();
        assert!(parse_extended_address_space_resource(&body, &mut resources));
        assert_eq!(resources[0].as_mmio(), Some((0x1000, 0x1000)));

        body[45..53].copy_from_slice(&(1u64 << 32).to_le_bytes());
        resources.clear();
        assert!(!parse_extended_address_space_resource(
            &body,
            &mut resources
        ));
    }

    #[ktest]
    fn parses_fixed_io_range_with_equal_minimum_and_maximum() {
        let template = [0x47, 0x01, 0xf8, 0x03, 0xf8, 0x03, 0x01, 0x08, 0x79, 0x00];
        let (resources, complete) = parse_resource_template(&template);
        assert!(complete);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].as_io_port(), Some((0x03f8, 8)));
    }

    #[ktest]
    fn scales_memory24_fields_to_bytes() {
        let template = [
            0x81, 0x09, 0x00, 0x00, 0x00, 0xd0, 0x00, 0xd0, 0x00, 0x00, 0x10, 0x00, 0x79, 0x00,
        ];
        let (resources, complete) = parse_resource_template(&template);
        assert!(complete);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].as_mmio(), Some((0x00d0_0000, 0x1000)));
    }

    #[ktest]
    fn parses_pci_root_bus_and_translated_windows() {
        let mut template = Vec::new();
        // WordBusNumber(MinFixed, MaxFixed, PosDecode), 0x20..=0x2f. Bit 0 is ignored
        // for ordinary Address Space descriptors and must not turn this into a consumer.
        template.extend_from_slice(&[
            0x88, 0x0d, 0x00, 0x02, 0x0d, 0x00, 0x00, 0x00, 0x20, 0x00, 0x2f, 0x00, 0x00, 0x00,
            0x10, 0x00,
        ]);
        // Root-consumed CF8 config port is validated but must not become a PCI child window.
        template.extend_from_slice(&[0x47, 0x01, 0xf8, 0x0c, 0xf8, 0x0c, 0x01, 0x08]);
        // WordIO producer window 0x0000..=0x0cf7.
        template.extend_from_slice(&[
            0x88, 0x0d, 0x00, 0x01, 0x0c, 0x03, 0x00, 0x00, 0x00, 0x00, 0xf7, 0x0c, 0x00, 0x00,
            0xf8, 0x0c,
        ]);
        // Prefetchable DWordMemory child 0x40000000 translated to CPU 0x80000000.
        template.extend_from_slice(&[0x87, 0x17, 0x00, 0x00, 0x0c, 0x07]);
        template.extend_from_slice(&0u32.to_le_bytes());
        template.extend_from_slice(&0x4000_0000u32.to_le_bytes());
        template.extend_from_slice(&0x4fff_ffffu32.to_le_bytes());
        template.extend_from_slice(&0x4000_0000u32.to_le_bytes());
        template.extend_from_slice(&0x1000_0000u32.to_le_bytes());
        template.extend_from_slice(&[0x79, 0x00]);

        let resources = parse_pci_root_resource_template(&template).expect("valid PCI root CRS");
        assert_eq!((resources.bus_start, resources.bus_end), (0x20, 0x2f));
        assert_eq!(resources.windows.len(), 2);
        assert_eq!(
            resources.windows[0],
            pci::AcpiPciRootWindow {
                space: pci::AcpiPciWindowSpace::Io,
                pci_start: 0,
                cpu_start: 0,
                size: 0x0cf8,
            }
        );
        assert_eq!(
            resources.windows[1],
            pci::AcpiPciRootWindow {
                space: pci::AcpiPciWindowSpace::PrefetchableMemory,
                pci_start: 0x4000_0000,
                cpu_start: 0x8000_0000,
                size: 0x1000_0000,
            }
        );
    }

    #[ktest]
    fn rejects_incomplete_or_overlapping_pci_root_crs() {
        let no_bus = [0x79, 0x00];
        assert!(parse_pci_root_resource_template(&no_bus).is_none());

        let mut overlapping = Vec::new();
        overlapping.extend_from_slice(&[
            0x88, 0x0d, 0x00, 0x02, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ]);
        for (minimum, maximum) in [(0x1000u32, 0x1fffu32), (0x1800, 0x27ff)] {
            overlapping.extend_from_slice(&[0x87, 0x17, 0x00, 0x00, 0x0c, 0x01]);
            overlapping.extend_from_slice(&0u32.to_le_bytes());
            overlapping.extend_from_slice(&minimum.to_le_bytes());
            overlapping.extend_from_slice(&maximum.to_le_bytes());
            overlapping.extend_from_slice(&0u32.to_le_bytes());
            overlapping.extend_from_slice(&0x1000u32.to_le_bytes());
        }
        overlapping.extend_from_slice(&[0x79, 0x00]);
        assert!(parse_pci_root_resource_template(&overlapping).is_none());
    }

    #[ktest]
    fn validates_pci_root_address_space_attributes() {
        let mut bus_range = None;
        let mut windows = Vec::new();
        assert!(!parse_pci_root_address_fields(
            false,
            2,
            2,
            0x0c,
            0,
            Some(1),
            Some(0),
            Some(0xff),
            Some(0),
            Some(0x100),
            Some(0),
            &mut bus_range,
            &mut windows,
        ));
        assert!(!parse_pci_root_address_fields(
            false,
            2,
            1,
            0x0c,
            0,
            Some(0),
            Some(0),
            Some(0xff),
            Some(0),
            Some(0x100),
            Some(0),
            &mut bus_range,
            &mut windows,
        ));
        assert!(parse_pci_root_address_fields(
            false,
            4,
            0,
            0x0c,
            0x20,
            Some(0),
            Some(0x1000),
            Some(0x1fff),
            Some(0),
            Some(0x1000),
            Some(0),
            &mut bus_range,
            &mut windows,
        ));
        assert!(windows.is_empty());
        assert!(parse_pci_root_address_fields(
            false,
            2,
            1,
            0x0c,
            0x31,
            Some(0),
            Some(0),
            Some(0xff),
            Some(0),
            Some(0x100),
            Some(0),
            &mut bus_range,
            &mut windows,
        ));
        assert!(windows.is_empty());
    }

    #[ktest]
    fn extended_pci_memory_uses_wc_att_and_validates_uefi_attributes() {
        let mut body = [0u8; 53];
        body[0] = 0;
        body[1] = 0x0c;
        body[2] = 0x07;
        body[3] = 1;
        body[13..21].copy_from_slice(&0x4000_0000u64.to_le_bytes());
        body[21..29].copy_from_slice(&0x4fff_ffffu64.to_le_bytes());
        body[29..37].copy_from_slice(&0x4000_0000u64.to_le_bytes());
        body[37..45].copy_from_slice(&0x1000_0000u64.to_le_bytes());
        let mut bus_range = Some((0, 0xff));
        let mut windows = Vec::new();
        assert!(parse_pci_root_extended_address_resource(
            &body,
            &mut bus_range,
            &mut windows,
        ));
        assert_eq!(windows[0].space, pci::AcpiPciWindowSpace::Memory);

        body[45..53].copy_from_slice(&EFI_MEMORY_WC.to_le_bytes());
        windows.clear();
        assert!(parse_pci_root_extended_address_resource(
            &body,
            &mut bus_range,
            &mut windows,
        ));
        assert_eq!(
            windows[0].space,
            pci::AcpiPciWindowSpace::PrefetchableMemory
        );

        let wb_xp_nv = (0x08u64 | 0x4000 | 0x8000).to_le_bytes();
        body[45..53].copy_from_slice(&wb_xp_nv);
        windows.clear();
        assert!(parse_pci_root_extended_address_resource(
            &body,
            &mut bus_range,
            &mut windows,
        ));
        assert_eq!(windows[0].space, pci::AcpiPciWindowSpace::Memory);

        body[45..53].copy_from_slice(&(1u64 << 32).to_le_bytes());
        windows.clear();
        assert!(!parse_pci_root_extended_address_resource(
            &body,
            &mut bus_range,
            &mut windows,
        ));

        body[0] = 1;
        body[2] = 3;
        windows.clear();
        assert!(!parse_pci_root_extended_address_resource(
            &body,
            &mut bus_range,
            &mut windows,
        ));
    }

    #[ktest]
    fn translates_address_space_offsets_at_descriptor_width() {
        for (width, negative) in [(2, 0xf000), (4, 0xffff_f000), (8, 0xffff_ffff_ffff_f000)] {
            assert_eq!(
                translated_address_start(0x1000, 0x20, 0x100, width),
                Some(0x1020)
            );
            assert_eq!(
                translated_address_start(0x1000, negative, 0x100, width),
                Some(0)
            );
        }
        assert_eq!(
            translated_address_start(0x1000_0000, 0x8000_0000, 0x1000, 4),
            Some(0x9000_0000)
        );
        assert_eq!(translated_address_start(0xfff0, 0, 0x20, 2), None);
        assert_eq!(translated_address_start(0, 0xffff, 1, 2), Some(0xffff));
    }

    #[ktest]
    fn accepts_adjacent_mcfg_allocations_covering_one_root() {
        let regions = [
            general::firmware::acpi::AcpiPciConfigRegion {
                segment: 3,
                bus_start: 0x20,
                bus_end: 0x27,
                segment_base_address: 0x8000_0000,
                physical_address: 0x8200_0000,
                size: 8 << 20,
            },
            general::firmware::acpi::AcpiPciConfigRegion {
                segment: 3,
                bus_start: 0x28,
                bus_end: 0x2f,
                segment_base_address: 0x9000_0000,
                physical_address: 0x9280_0000,
                size: 8 << 20,
            },
        ];
        assert!(pci::mcfg_covers_root(&regions, 3, 0x20, 0x2f));
        assert!(!pci::mcfg_covers_root(&regions, 3, 0x20, 0x30));
        assert!(!pci::mcfg_covers_root(&regions, 2, 0x20, 0x2f));
    }

    #[ktest]
    fn validates_pci_prt_entries_and_rejects_ambiguity() {
        let direct = AmlValue::Package(vec![
            AmlValue::Integer((3u64 << 16) | 0xffff),
            AmlValue::Integer(0),
            AmlValue::Integer(0),
            AmlValue::Integer(32),
        ]);
        let linked = AmlValue::Package(vec![
            AmlValue::Integer(4u64 << 16),
            AmlValue::Integer(1),
            AmlValue::String("\\_SB.LNKA".into()),
            AmlValue::Integer(0),
        ]);
        let table = AmlValue::Package(vec![direct, linked]);
        let routes = parse_pci_prt(&table, |source, index| {
            (source == "\\_SB.LNKA" && index == 0).then_some(AcpiPciIrqTarget {
                gsi: 48,
                trigger: general::dev::irq::IrqTrigger::Edge,
                polarity: general::dev::irq::IrqPolarity::High,
                sharing: general::dev::irq::IrqSharing::Exclusive,
            })
        })
        .expect("valid _PRT");
        assert_eq!(
            routes,
            vec![
                pci::AcpiPciIrqRoute {
                    device: 3,
                    function: None,
                    pin: 0,
                    gsi: 32,
                    trigger: general::dev::irq::IrqTrigger::Level,
                    polarity: general::dev::irq::IrqPolarity::Low,
                    sharing: general::dev::irq::IrqSharing::Shared,
                },
                pci::AcpiPciIrqRoute {
                    device: 4,
                    function: Some(0),
                    pin: 1,
                    gsi: 48,
                    trigger: general::dev::irq::IrqTrigger::Edge,
                    polarity: general::dev::irq::IrqPolarity::High,
                    sharing: general::dev::irq::IrqSharing::Exclusive,
                },
            ]
        );

        let duplicate = AmlValue::Package(vec![
            AmlValue::Package(vec![
                AmlValue::Integer(3u64 << 16),
                AmlValue::Integer(0),
                AmlValue::Integer(0),
                AmlValue::Integer(32),
            ]),
            AmlValue::Package(vec![
                AmlValue::Integer((3u64 << 16) | 0xffff),
                AmlValue::Integer(0),
                AmlValue::Integer(0),
                AmlValue::Integer(33),
            ]),
        ]);
        assert_eq!(
            parse_pci_prt(&duplicate, |_, _| Some(direct_pci_intx_target(0))),
            None
        );
        let short = AmlValue::Package(vec![AmlValue::Package(vec![
            AmlValue::Integer(0),
            AmlValue::Integer(0),
            AmlValue::Integer(0),
        ])]);
        assert_eq!(
            parse_pci_prt(&short, |_, _| Some(direct_pci_intx_target(0))),
            None
        );
    }

    #[ktest]
    fn parses_pci_link_current_irq_descriptor_by_source_index() {
        let small_irq = [0x22, 0x20, 0x00, 0x79, 0x00];
        assert_eq!(
            parse_pci_link_current_irq(&small_irq, 0),
            Some(AcpiPciIrqTarget {
                gsi: 5,
                trigger: general::dev::irq::IrqTrigger::Edge,
                polarity: general::dev::irq::IrqPolarity::High,
                sharing: general::dev::irq::IrqSharing::Exclusive,
            })
        );
        assert_eq!(parse_pci_link_current_irq(&small_irq, 1), None);

        let extended_irq = [
            0x89, 0x06, 0x00, 0x0f, 0x01, 0x21, 0x00, 0x00, 0x00, 0x79, 0x00,
        ];
        assert_eq!(
            parse_pci_link_current_irq(&extended_irq, 0),
            Some(AcpiPciIrqTarget {
                gsi: 33,
                trigger: general::dev::irq::IrqTrigger::Edge,
                polarity: general::dev::irq::IrqPolarity::Low,
                sharing: general::dev::irq::IrqSharing::Shared,
            })
        );

        let ambiguous = [0x22, 0x30, 0x00, 0x79, 0x00];
        assert_eq!(parse_pci_link_current_irq(&ambiguous, 0), None);
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
        let mapper = AcpiMapper::new(|physical| physical, mappings, StartAcpiHostOps::NONE);

        assert!(mapper.pci_backend_available());
        assert_eq!(
            mapper.pci_ecam_address(2, 0x21, 3, 2, 0x120),
            Some(0x8211_a120)
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
        let mapper = AcpiMapper::new(identity_address, mappings, StartAcpiHostOps::NONE);

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
        let mapper = AcpiMapper::new(offset_device_address, mappings, StartAcpiHostOps::NONE);

        assert_eq!(mapper.resolve_table(0x1004, 4), table.as_ptr() as usize + 4);
        assert_eq!(mapper.resolve_mmio::<u32>(0x1004), Some(0x5004));
        assert_eq!(mapper.resolve_mmio::<u32>(0x1002), None);
    }
}
