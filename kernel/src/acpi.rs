//! 基于 ACPI 的内核初始化逻辑。
//!
//! 当前 ACPI 的实现只是一个最小的 AIGC 实现，因为工程目前的重心不在于 ACPI，而
//! 在于 DTB。在决赛的时候可能会对 ACPI 的实现进行充分完善。

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::str::FromStr;
use core::{mem, slice};

use acpi::address::{AddressSpace, GenericAddress};
use acpi::aml::namespace::{AmlName, NamespaceLevelKind};
use acpi::aml::object::{Object, WrappedObject};
use acpi::aml::resource::{
    AddressSpaceResourceType, MemoryRangeDescriptor, Resource, resource_descriptor_list,
};
use acpi::aml::{AmlError, Interpreter};
use acpi::sdt::SdtHeader;
use acpi::sdt::fadt::Fadt;
use acpi::sdt::madt::Madt;
use acpi::sdt::spcr::{Spcr, SpcrInterfaceType};
use acpi::{AcpiTable, AmlHandler, Handle, Handler, PhysicalMapping};

use allocator::KERNEL_ALLOCATOR;
use general::dev::char::CharDevice;
use general::dev::drivers;
use general::dev::enumerate::DEVICES;
use general::dev::function::find_char_device_by_fw_name;
use general::dev::platform::{
    DeviceMatchId, DeviceProperties, DeviceResource, PlatformDeviceInfo,
    register_and_probe_platform_device,
};
use general::dev::pnp::{DevInitContext, set_dev_init_context};
use general::firmware::power::{
    PowerAccessWidth, PowerControlInfo, PowerControlMethod, PowerRegister, PowerRegisterSpace,
};
use general::firmware::{FirmwareTableMapping, SerialPortInfo};
use general::vfs::FS_REGISTRY;
use general::vfs::VfsContext;
use general::vfs::cred::Credentials;
use general::vfs::dentry::{Dentry, VfsRoot};
use general::vfs::devtmpfs::DevTmpfsSuperblockOps;
use general::vfs::error::VfsError;
use general::vfs::limits::VfsLimits;
use general::vfs::mount::{Mount, MountFlags, MountNamespace};
use general::vfs::path::{self, Dirfd, LookupFlags};
use general::vfs::stat::FileMode;
use general::{StartContext, StartFirmware};
use log::{LogRecord, LogSink, printk};

use crate::dtb::ensure_dir;
use crate::start;

const ACPI_MADT_TYPE_LOCAL_APIC: u8 = 0;
const ACPI_MADT_TYPE_GENERIC_INTERRUPT: u8 = 11;
const ACPI_MADT_TYPE_CORE_PIC: u8 = 17;
const ACPI_MADT_ENABLED: u32 = 1;
const ACPI_MADT_ONLINE_CAPABLE: u32 = 2;
const ACPI_HID_PNP0500: &str = "PNP0500";
const ACPI_HID_PNP0501: &str = "PNP0501";
const ACPI_HID_VIRTIO_MMIO: &str = "LNRO0005";

#[derive(Clone, Copy)]
struct AcpiMapper {
    phys_to_virt: fn(usize) -> usize,
    copied_tables: &'static [FirmwareTableMapping],
}

impl AcpiMapper {
    const fn new(
        phys_to_virt: fn(usize) -> usize,
        copied_tables: &'static [FirmwareTableMapping],
    ) -> Self {
        Self {
            phys_to_virt,
            copied_tables,
        }
    }

    #[inline]
    fn resolve(&self, physical_address: usize, size: usize) -> usize {
        for mapping in self.copied_tables {
            if let Some(virtual_address) = mapping.resolve(physical_address, size) {
                return virtual_address;
            }
        }
        (self.phys_to_virt)(physical_address)
    }

    #[inline]
    fn read_memory<T: Copy>(&self, physical_address: usize) -> T {
        let virtual_address = self.resolve(physical_address, mem::size_of::<T>());
        unsafe { ptr::read_volatile(virtual_address as *const T) }
    }

    #[inline]
    fn write_memory<T>(&self, physical_address: usize, value: T) {
        let virtual_address = self.resolve(physical_address, mem::size_of::<T>());
        unsafe { ptr::write_volatile(virtual_address as *mut T, value) };
    }
}

impl Handler for AcpiMapper {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let virtual_address = self.resolve(physical_address, size);
        let virtual_start = NonNull::new(virtual_address as *mut T)
            .expect("[kernel-start][acpi] null ACPI mapping");
        PhysicalMapping {
            physical_start: physical_address,
            virtual_start,
            region_length: size,
            mapped_length: size,
            handler: *self,
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        self.read_memory(address)
    }

    fn read_u16(&self, address: usize) -> u16 {
        self.read_memory(address)
    }

    fn read_u32(&self, address: usize) -> u32 {
        self.read_memory(address)
    }

    fn read_u64(&self, address: usize) -> u64 {
        self.read_memory(address)
    }

    fn write_u8(&self, address: usize, value: u8) {
        self.write_memory(address, value);
    }

    fn write_u16(&self, address: usize, value: u16) {
        self.write_memory(address, value);
    }

    fn write_u32(&self, address: usize, value: u32) {
        self.write_memory(address, value);
    }

    fn write_u64(&self, address: usize, value: u64) {
        self.write_memory(address, value);
    }

    fn read_io_u8(&self, _port: u16) -> u8 {
        unsupported_host_operation("read_io_u8")
    }

    fn read_io_u16(&self, _port: u16) -> u16 {
        unsupported_host_operation("read_io_u16")
    }

    fn read_io_u32(&self, _port: u16) -> u32 {
        unsupported_host_operation("read_io_u32")
    }

    fn write_io_u8(&self, _port: u16, _value: u8) {
        unsupported_host_operation("write_io_u8")
    }

    fn write_io_u16(&self, _port: u16, _value: u16) {
        unsupported_host_operation("write_io_u16")
    }

    fn write_io_u32(&self, _port: u16, _value: u32) {
        unsupported_host_operation("write_io_u32")
    }

    fn read_pci_u8(&self, _address: acpi::PciAddress, _offset: u16) -> u8 {
        unsupported_host_operation("read_pci_u8")
    }

    fn read_pci_u16(&self, _address: acpi::PciAddress, _offset: u16) -> u16 {
        unsupported_host_operation("read_pci_u16")
    }

    fn read_pci_u32(&self, _address: acpi::PciAddress, _offset: u16) -> u32 {
        unsupported_host_operation("read_pci_u32")
    }

    fn write_pci_u8(&self, _address: acpi::PciAddress, _offset: u16, _value: u8) {
        unsupported_host_operation("write_pci_u8")
    }

    fn write_pci_u16(&self, _address: acpi::PciAddress, _offset: u16, _value: u16) {
        unsupported_host_operation("write_pci_u16")
    }

    fn write_pci_u32(&self, _address: acpi::PciAddress, _offset: u16, _value: u32) {
        unsupported_host_operation("write_pci_u32")
    }

    fn nanos_since_boot(&self) -> u64 {
        0
    }

    fn stall(&self, microseconds: u64) {
        for _ in 0..microseconds {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        self.stall(milliseconds.saturating_mul(1000));
    }
}

impl AmlHandler for AcpiMapper {
    fn create_mutex(&self) -> Handle {
        Handle(0)
    }

    fn acquire(&self, _mutex: Handle, _timeout: u16) -> Result<(), AmlError> {
        Ok(())
    }

    fn release(&self, _mutex: Handle) {}
}

#[derive(Clone)]
struct FirmwareMmioDevice {
    name: &'static str,
    phys_addr: usize,
    size: usize,
}

pub fn kernel_start_init(context: &StartContext) {
    log::debug!("[kernel-start][acpi] jumped into kernel_start_init()");

    let acpi = match context.firmware {
        StartFirmware::Acpi(acpi) => acpi,
        StartFirmware::Dtb(_) => {
            panic!("[kernel-start][acpi] StartContext firmware does not match ACPI path")
        }
    };

    let mapper = AcpiMapper::new(context.address.phys_to_virt, acpi.mappings);
    let tables = unsafe { acpi::AcpiTables::from_rsdp(mapper, acpi.rsdp_phys) }
        .map_err(|_| "[kernel-start][acpi] invalid RSDP/table tree")
        .unwrap_or_else(|err| panic!("{}", err));

    printk!(
        "[kernel-start][acpi] RSDP={:#x} revision={}",
        acpi.rsdp_phys,
        tables.rsdp_revision,
    );

    // ── 阶段 1：解析 ACPI 固件表 ───────────────────────────────────────────

    let cpu_count = cpu_count_from_madt(&tables).unwrap_or(1).max(1);
    let serial_port = serial_port_from_spcr(&tables);
    let console_serial_port_phys = serial_port.map(|port| port.phys_addr);
    let power_controls = parse_power_controls(&tables, mapper);
    let memory_segments = context
        .memory
        .boot_map
        .usable_segments()
        .unwrap_or_else(|| {
            panic!("[kernel-start][acpi] ACPI firmware requires usable boot memory segments")
        });

    // ── 阶段 2：安装电源控制入口 ───────────────────────────────────────────

    general::firmware::power::install(power_controls, context.address.phys_to_virt);

    // ── 阶段 3：初始化分层内存分配器 ───────────────────────────────────────

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
            alloc_ops.map_kernel_heap_range,
            alloc_ops.unmap_kernel_heap_range,
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

    let cmdline = context
        .boot
        .command_line
        .map(general::cmdline::Cmdline::new);

    // ── 阶段 4：发现并登记 ACPI 设备描述 ──────────────────────────────────

    let mut serial_ports: Vec<SerialPortInfo> = Vec::new();
    let mut virtio_mmio_devices: Vec<FirmwareMmioDevice> = Vec::new();
    discover_acpi_namespace_devices(mapper, &tables, &mut serial_ports, &mut virtio_mmio_devices);
    if let Some(port) = serial_port
        && !serial_ports
            .iter()
            .any(|existing| existing.phys_addr == port.phys_addr)
    {
        serial_ports.push(port);
    }
    let console_serial_port_index = console_serial_port_phys
        .and_then(|phys| serial_ports.iter().position(|port| port.phys_addr == phys));

    printk!(
        "[kernel-start][acpi] device discovery complete: {} uart(s), {} virtio block candidate(s)",
        serial_ports.len(),
        virtio_mmio_devices.len()
    );

    // ── 阶段 5：挂载根文件系统并准备 /dev ─────────────────────────────────

    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::TmpfsDriver)))
        .expect("[kernel-start][acpi] failed to register tmpfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::DevTmpfsDriver)))
        .expect("[kernel-start][acpi] failed to register devtmpfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::ProcFsDriver)))
        .expect("[kernel-start][acpi] failed to register procfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::SysFsDriver)))
        .expect("[kernel-start][acpi] failed to register sysfs driver");
    general::vfs::register_block_filesystems();

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

    let dev_inode = root_sb
        .root_inode
        .mkdir("dev", FileMode::new(0o755), &cred)
        .expect("[kernel-start][acpi] failed to create /dev directory");
    let dev_dentry = general::vfs::DCACHE.insert(Dentry::new_positive(
        "dev",
        Some(Arc::clone(&root_sb.root_dentry)),
        Arc::clone(&dev_inode),
    ));

    let dev_sb = FS_REGISTRY
        .find("devtmpfs")
        .expect("[kernel-start][acpi] devtmpfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][acpi] failed to mount devtmpfs");

    mount_ns
        .mount(
            Arc::clone(&dev_dentry),
            Arc::clone(&dev_sb),
            MountFlags::default(),
        )
        .expect("[kernel-start][acpi] failed to mount devtmpfs on /dev");

    let dev_ops = dev_sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .expect("[kernel-start][acpi] failed to downcast devtmpfs ops");

    general::vfs::devtmpfs::install_pnp_bridge(Arc::clone(&dev_sb))
        .expect("[kernel-start][acpi] failed to install PnP devtmpfs bridge");
    printk!("[kernel-start][acpi] PnP devtmpfs callbacks installed");

    set_dev_init_context(
        DevInitContext::new(
            context.address.device_mmio_to_virt,
            context.address.virt_to_phys,
        )
        .with_realtime_clock(crate::vdso::set_realtime_ns),
    );
    drivers::register_builtin_drivers()
        .expect("[kernel-start][acpi] failed to register built-in PnP drivers");
    printk!("[kernel-start][acpi] registered built-in PnP drivers");

    let stdout_phys = console_serial_port_phys;
    let mut platform_bound = 0usize;
    for port in &serial_ports {
        let mut ids = Vec::new();
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_PNP0500.into()));
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_PNP0501.into()));
        let mut resources = Vec::new();
        resources.push(DeviceResource::Mmio {
            phys: port.phys_addr,
            size: 0x100,
        });
        let info = PlatformDeviceInfo {
            fw_name: port.name.into(),
            ids,
            resources,
            properties: DeviceProperties {
                clock_hz: port.clock_hz,
                baud: Some(115_200),
                stdout: stdout_phys == Some(port.phys_addr),
            },
        };
        if register_platform_device(info, "acpi") {
            platform_bound += 1;
        }
    }
    for device in &virtio_mmio_devices {
        let mut ids = Vec::new();
        ids.push(DeviceMatchId::AcpiHid(ACPI_HID_VIRTIO_MMIO.into()));
        let mut resources = Vec::new();
        resources.push(DeviceResource::Mmio {
            phys: device.phys_addr,
            size: device.size,
        });
        let info = PlatformDeviceInfo {
            fw_name: device.name.into(),
            ids,
            resources,
            properties: DeviceProperties::default(),
        };
        if register_platform_device(info, "acpi") {
            platform_bound += 1;
        }
    }
    printk!(
        "[kernel-start][acpi] platform PnP discovery complete: {} candidate(s), {} bound",
        serial_ports.len() + virtio_mmio_devices.len(),
        platform_bound
    );

    printk!("[kernel-start][acpi] VFS ready: tmpfs '/' + devtmpfs '/dev'");

    // ── 阶段 6：注册控制台并绑定日志输出 ──────────────────────────────────

    let vfs_ctx = VfsContext::new(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        VfsRoot::new(Arc::clone(&root_sb.root_dentry), Arc::clone(&root_mount)),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
        FileMode::new(0),
        VfsLimits::default_arc(),
    );

    // 把同一套部件交给 sched shim 保管：随后 sched::boot_init 会据此给 init
    // 任务挂上 TASKEXT_VFS_CONTEXT / TASKEXT_VFS_FDTABLE。
    crate::sched::stash_boot_vfs_parts(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
    );

    ensure_dir(&vfs_ctx, "/sys", FileMode::new(0o555))
        .expect("[kernel-start][acpi] failed to ensure /sys directory");
    let sys_dentry = path::lookup(&vfs_ctx, &Dirfd::Cwd, "/sys", LookupFlags::DIRECTORY)
        .expect("[kernel-start][acpi] failed to resolve /sys")
        .dentry;
    let sys_sb = FS_REGISTRY
        .find("sysfs")
        .expect("[kernel-start][acpi] sysfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][acpi] failed to mount sysfs");
    mount_ns
        .mount(
            Arc::clone(&sys_dentry),
            Arc::clone(&sys_sb),
            MountFlags::default(),
        )
        .expect("[kernel-start][acpi] failed to mount sysfs on /sys");

    let console_registered = {
        let cmdline_dev = cmdline
            .as_ref()
            .and_then(|cl| {
                cl.find("console")
                    .map(|v| v.split_once(',').map_or(v, |(d, _)| d))
            })
            .and_then(|name| resolve_cmdline_console(&vfs_ctx, dev_ops, name));

        printk!(
            "[kernel-start][acpi] console device from cmdline: {:?}",
            cmdline_dev.as_ref().map(|dev| dev.fw_name())
        );

        let dev = if let Some(dev) = cmdline_dev {
            printk!(
                "[kernel-start][acpi] console from cmdline: {}",
                dev.fw_name()
            );
            Some(dev)
        } else if let Some(port) = console_serial_port_index.and_then(|i| serial_ports.get(i)) {
            let found = lookup_char_fw_name(port.name);
            if let Some(dev) = found.as_ref() {
                printk!(
                    "[kernel-start][acpi] console from firmware: {}",
                    dev.fw_name()
                );
            }
            found
        } else {
            None
        };

        if let Some(dev) = dev {
            general::console::register_console(dev.clone());
            match dev_ops.bind_char("console", dev.clone()) {
                Ok(()) => {
                    printk!(
                        "[kernel-start][acpi] bound /dev/console -> {}",
                        dev.fw_name()
                    );
                    crate::sched::stash_boot_console_name(alloc::string::String::from(
                        "/dev/console",
                    ));
                }
                Err(VfsError::AlreadyExists) => {
                    printk!("[kernel-start][acpi] /dev/console already exists; using it for stdio");
                    crate::sched::stash_boot_console_name(alloc::string::String::from(
                        "/dev/console",
                    ));
                }
                Err(err) => {
                    printk!(
                        "[kernel-start][acpi] failed to bind /dev/console: {:?}",
                        err
                    );
                }
            }
            true
        } else {
            printk!("[kernel-start][acpi] no console registered");
            false
        }
    };

    if console_registered {
        static LOG_SINK: LogSink = LogSink {
            write_record: write_log_record_to_console,
        };
        log::bind_log_sink(&LOG_SINK);
    }

    printk!("[kernel-start][acpi] kernel initialization complete, jumping to main entry");
}

fn write_log_record_to_console(record: &LogRecord<'_>) {
    let line = start::format_log_record_line(record);
    general::console::console_write(line.as_bytes());
}

fn resolve_cmdline_console(
    vfs_ctx: &VfsContext,
    dev_ops: &DevTmpfsSuperblockOps,
    name: &str,
) -> Option<CharDevice> {
    if let Some(dev_name) = name.strip_prefix("/dev/")
        && let Some(dev) = dev_ops.char_dev(dev_name)
    {
        return Some(dev);
    }
    if !name.starts_with('/')
        && let Some(dev) = dev_ops.char_dev(name)
    {
        return Some(dev);
    }
    if name.starts_with('/') {
        return path::lookup(vfs_ctx, &Dirfd::Cwd, name, LookupFlags::default())
            .ok()
            .and_then(|lookup| lookup.dentry.full_path(&vfs_ctx.root.root()))
            .and_then(|resolved| {
                resolved
                    .strip_prefix("/dev/")
                    .and_then(|dev_name| dev_ops.char_dev(dev_name))
            });
    }
    lookup_char_fw_name(name)
}

fn lookup_char_fw_name(name: &str) -> Option<CharDevice> {
    find_char_device_by_fw_name(&DEVICES.functions, name)
}

fn register_platform_device(info: PlatformDeviceInfo, tag: &str) -> bool {
    match register_and_probe_platform_device(info) {
        Ok(reg) if reg.bound => true,
        Ok(reg) => {
            printk!(
                "[kernel-start][{}] no platform driver for {}",
                tag,
                reg.device.id
            );
            false
        }
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

fn unsupported_host_operation(operation: &str) -> ! {
    panic!(
        "[kernel-start][acpi] ACPI host operation `{}` is not wired for the static table parser",
        operation
    )
}

fn discover_acpi_namespace_devices(
    mapper: AcpiMapper,
    tables: &acpi::AcpiTables<AcpiMapper>,
    serial_ports: &mut Vec<SerialPortInfo>,
    virtio_mmio_devices: &mut Vec<FirmwareMmioDevice>,
) {
    let interpreter = match build_acpi_interpreter(mapper, tables) {
        Some(interpreter) => interpreter,
        None => return,
    };

    interpreter.initialize_namespace();

    let mut paths = Vec::new();
    let mut namespace = interpreter.namespace.lock().clone();
    if let Err(err) = namespace.traverse(|path, level| {
        if level.kind == NamespaceLevelKind::Device {
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
        let ids = acpi_device_ids(&interpreter, &path);
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

        let resources = acpi_device_resources(&interpreter, &path);
        let Some((phys_addr, size)) = resources.into_iter().find(|&(_, size)| size != 0) else {
            printk!(
                "[kernel-start][acpi] ACPI device {} has supported id but no MMIO resource",
                path_string
            );
            continue;
        };

        let name = alloc::format!("{}", path).leak();
        if is_serial {
            if !serial_ports.iter().any(|port| port.phys_addr == phys_addr) {
                printk!(
                    "[kernel-start][acpi] namespace serial: {} phys={:#x}",
                    name,
                    phys_addr
                );
                serial_ports.push(SerialPortInfo {
                    name,
                    phys_addr,
                    clock_hz: None,
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
                size,
            });
        }
    }
}

fn build_acpi_interpreter(
    mapper: AcpiMapper,
    tables: &acpi::AcpiTables<AcpiMapper>,
) -> Option<Interpreter<AcpiMapper>> {
    let dsdt = match tables.dsdt() {
        Ok(dsdt) => dsdt,
        Err(err) => {
            printk!(
                "[kernel-start][acpi] DSDT unavailable for device discovery: {:?}",
                err
            );
            return None;
        }
    };

    let interpreter = Interpreter::new(mapper, dsdt.revision, empty_fixed_registers(), None);
    if let Err(err) = load_aml_table(&interpreter, mapper, dsdt) {
        printk!(
            "[kernel-start][acpi] failed to load DSDT for device discovery: {:?}",
            err
        );
        return None;
    }

    for ssdt in tables.ssdts() {
        if let Err(err) = load_aml_table(&interpreter, mapper, ssdt) {
            printk!(
                "[kernel-start][acpi] failed to load SSDT {:#x} for device discovery: {:?}",
                ssdt.phys_address,
                err
            );
        }
    }

    Some(interpreter)
}

fn load_aml_table(
    interpreter: &Interpreter<AcpiMapper>,
    mapper: AcpiMapper,
    table: acpi::AmlTable,
) -> Result<(), AmlError> {
    let header_size = mem::size_of::<SdtHeader>();
    let length = table.length as usize;
    if length <= header_size {
        return Ok(());
    }
    let aml_phys = table.phys_address + header_size;
    let aml_len = length - header_size;
    let aml_virt = mapper.resolve(aml_phys, aml_len);
    let aml = unsafe { slice::from_raw_parts(aml_virt as *const u8, aml_len) };
    interpreter.load_table(aml)
}

fn empty_fixed_registers() -> Arc<acpi::registers::FixedRegisters<AcpiMapper>> {
    use acpi::address::{GenericAddress, MappedGas};
    use acpi::registers::{FixedRegisters, Pm1ControlRegisterBlock, Pm1EventRegisterBlock};

    let mapper = AcpiMapper {
        phys_to_virt: |phys| phys,
        copied_tables: &[],
    };
    let gas = GenericAddress {
        address_space: AddressSpace::SystemIo,
        bit_width: 8,
        bit_offset: 0,
        access_size: 1,
        address: 0,
    };
    let pm1a_event = unsafe { MappedGas::map_gas(gas, &mapper) }
        .expect("[kernel-start][acpi] failed to construct dummy PM1 event GAS");
    let pm1a_control = unsafe { MappedGas::map_gas(gas, &mapper) }
        .expect("[kernel-start][acpi] failed to construct dummy PM1 control GAS");

    Arc::new(FixedRegisters {
        pm1_event_registers: Pm1EventRegisterBlock {
            pm1_event_length: 4,
            pm1a: pm1a_event,
            pm1b: None,
        },
        pm1_control_registers: Pm1ControlRegisterBlock {
            pm1a: pm1a_control,
            pm1b: None,
        },
    })
}

fn acpi_device_ids(interpreter: &Interpreter<AcpiMapper>, path: &AmlName) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = acpi_eval_string(interpreter, path, "_HID") {
        ids.push(id);
    }
    if let Ok(Some(cid)) =
        interpreter.evaluate_if_present(acpi_child_name(path, "_CID"), Vec::new())
    {
        collect_acpi_id_object(cid, &mut ids);
    }
    ids
}

fn collect_acpi_id_object(object: WrappedObject, ids: &mut Vec<String>) {
    let object = object.unwrap_transparent_reference();
    match &*object {
        Object::Integer(value) => {
            ids.push(eisa_id_to_string(*value as u32));
        }
        Object::String(value) => {
            ids.push(value.clone());
        }
        Object::Buffer(bytes) => {
            if let Ok(value) = core::str::from_utf8(bytes) {
                ids.push(value.trim_end_matches('\0').to_string());
            }
        }
        Object::Package(elements) => {
            for element in elements {
                collect_acpi_id_object(element.clone(), ids);
            }
        }
        _ => {}
    }
}

fn acpi_eval_string(
    interpreter: &Interpreter<AcpiMapper>,
    path: &AmlName,
    name: &str,
) -> Option<String> {
    let object = interpreter
        .evaluate_if_present(acpi_child_name(path, name), Vec::new())
        .ok()
        .flatten()?
        .unwrap_transparent_reference();
    match &*object {
        Object::Integer(value) => Some(eisa_id_to_string(*value as u32)),
        Object::String(value) => Some(value.clone()),
        Object::Buffer(bytes) => core::str::from_utf8(bytes)
            .ok()
            .map(|value| value.trim_end_matches('\0').to_string()),
        _ => None,
    }
}

fn acpi_device_resources(
    interpreter: &Interpreter<AcpiMapper>,
    path: &AmlName,
) -> Vec<(usize, usize)> {
    let object = match interpreter.evaluate_if_present(acpi_child_name(path, "_CRS"), Vec::new()) {
        Ok(Some(object)) => object,
        _ => return Vec::new(),
    };
    let resources = match resource_descriptor_list(object.unwrap_transparent_reference()) {
        Ok(resources) => resources,
        Err(err) => {
            printk!(
                "[kernel-start][acpi] failed to parse _CRS for {}: {:?}",
                path,
                err
            );
            return Vec::new();
        }
    };

    let mut ranges = Vec::new();
    for resource in resources {
        match resource {
            Resource::MemoryRange(MemoryRangeDescriptor::FixedLocation {
                base_address,
                range_length,
                ..
            }) => ranges.push((base_address as usize, range_length as usize)),
            Resource::AddressSpace(desc)
                if desc.resource_type == AddressSpaceResourceType::MemoryRange =>
            {
                ranges.push((desc.address_range.0 as usize, desc.length as usize));
            }
            _ => {}
        }
    }
    ranges
}

fn acpi_child_name(path: &AmlName, name: &str) -> AmlName {
    AmlName::from_str(name)
        .and_then(|child| child.resolve(path))
        .expect("[kernel-start][acpi] invalid static AML child name")
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
    mapper: AcpiMapper,
) -> PowerControlInfo {
    let shutdown = acpi_shutdown_control(tables, mapper);
    let reboot = acpi_reboot_control(tables);

    printk!(
        "[kernel-start][acpi] power controls: shutdown={} reboot={}",
        shutdown.is_some() as usize,
        reboot.is_some() as usize
    );

    PowerControlInfo { shutdown, reboot }
}

fn acpi_reboot_control(tables: &acpi::AcpiTables<AcpiMapper>) -> Option<PowerControlMethod> {
    let fadt = tables.find_table::<Fadt>()?;
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
    mapper: AcpiMapper,
) -> Option<PowerControlMethod> {
    let fadt = tables.find_table::<Fadt>()?;
    if let Err(err) = fadt.validate() {
        printk!("[kernel-start][acpi] invalid FADT for shutdown: {:?}", err);
        return None;
    }

    let (sleep_type_a, sleep_type_b) = match acpi_s5_sleep_types(tables, mapper) {
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
    let access_width = PowerAccessWidth::from_bits(gas.bit_width, gas.access_size)?;
    Some(PowerRegister {
        space,
        address: gas.address as usize,
        access_width,
    })
}

fn acpi_s5_sleep_types(
    tables: &acpi::AcpiTables<AcpiMapper>,
    mapper: AcpiMapper,
) -> Option<(u8, u8)> {
    if let Ok(dsdt) = tables.dsdt()
        && let Some(types) = acpi_s5_sleep_types_from_table(mapper, dsdt)
    {
        return Some(types);
    }

    for ssdt in tables.ssdts() {
        if let Some(types) = acpi_s5_sleep_types_from_table(mapper, ssdt) {
            return Some(types);
        }
    }
    None
}

fn acpi_s5_sleep_types_from_table(mapper: AcpiMapper, table: acpi::AmlTable) -> Option<(u8, u8)> {
    let header_size = mem::size_of::<SdtHeader>();
    let length = table.length as usize;
    if length <= header_size {
        return None;
    }
    let aml_phys = table.phys_address.checked_add(header_size)?;
    let aml_len = length - header_size;
    let aml_virt = mapper.resolve(aml_phys, aml_len);
    let aml = unsafe { slice::from_raw_parts(aml_virt as *const u8, aml_len) };
    let result = scan_aml_for_s5(aml);
    if let Some((sleep_type_a, sleep_type_b)) = result {
        printk!(
            "[kernel-start][acpi] _S5 sleep types: a={} b={} table={:#x}",
            sleep_type_a,
            sleep_type_b,
            table.phys_address
        );
    }
    result
}

fn scan_aml_for_s5(aml: &[u8]) -> Option<(u8, u8)> {
    let mut offset = 0usize;
    while offset + 6 <= aml.len() {
        if aml[offset] == 0x08
            && let Some(object_offset) = aml_name_s5_end(aml, offset + 1)
            && let Some(types) = parse_s5_package(aml, object_offset)
        {
            return Some(types);
        }
        offset += 1;
    }
    None
}

fn aml_name_s5_end(aml: &[u8], mut offset: usize) -> Option<usize> {
    while aml.get(offset) == Some(&b'^') {
        offset += 1;
    }
    if aml.get(offset) == Some(&b'\\') {
        offset += 1;
    }

    match *aml.get(offset)? {
        0x2e => {
            let first = aml.get(offset + 1..offset + 5)?;
            let second = aml.get(offset + 5..offset + 9)?;
            (first == b"_S5_" || second == b"_S5_").then_some(offset + 9)
        }
        0x2f => {
            let count = *aml.get(offset + 1)? as usize;
            let names_start = offset + 2;
            let names_end = names_start.checked_add(count.checked_mul(4)?)?;
            let names = aml.get(names_start..names_end)?;
            names
                .chunks_exact(4)
                .any(|name| name == b"_S5_")
                .then_some(names_end)
        }
        _ => (aml.get(offset..offset + 4)? == b"_S5_").then_some(offset + 4),
    }
}

fn parse_s5_package(aml: &[u8], offset: usize) -> Option<(u8, u8)> {
    if *aml.get(offset)? != 0x12 {
        return None;
    }
    let (_pkg_len, pkg_len_bytes) = parse_aml_pkg_len(aml, offset + 1)?;
    let mut cursor = offset + 1 + pkg_len_bytes;
    let element_count = *aml.get(cursor)? as usize;
    cursor += 1;
    if element_count < 1 {
        return None;
    }

    let (sleep_type_a, next) = parse_aml_integer(aml, cursor)?;
    cursor = next;
    let sleep_type_b = if element_count >= 2 {
        parse_aml_integer(aml, cursor)
            .map(|(value, _next)| value)
            .unwrap_or(sleep_type_a)
    } else {
        sleep_type_a
    };

    Some((sleep_type_a as u8, sleep_type_b as u8))
}

fn parse_aml_pkg_len(aml: &[u8], offset: usize) -> Option<(usize, usize)> {
    let lead = *aml.get(offset)?;
    let follow_count = (lead >> 6) as usize;
    if follow_count == 0 {
        return Some(((lead & 0x3f) as usize, 1));
    }

    let mut length = (lead & 0x0f) as usize;
    for index in 0..follow_count {
        let byte = *aml.get(offset + 1 + index)? as usize;
        length |= byte << (4 + index * 8);
    }
    Some((length, 1 + follow_count))
}

fn parse_aml_integer(aml: &[u8], offset: usize) -> Option<(u64, usize)> {
    match *aml.get(offset)? {
        0x00 => Some((0, offset + 1)),
        0x01 => Some((1, offset + 1)),
        0xff => Some((u64::MAX, offset + 1)),
        0x0a => Some((*aml.get(offset + 1)? as u64, offset + 2)),
        0x0b => Some((
            u16::from_le_bytes(aml.get(offset + 1..offset + 3)?.try_into().ok()?) as u64,
            offset + 3,
        )),
        0x0c => Some((
            u32::from_le_bytes(aml.get(offset + 1..offset + 5)?.try_into().ok()?) as u64,
            offset + 5,
        )),
        0x0e => Some((
            u64::from_le_bytes(aml.get(offset + 1..offset + 9)?.try_into().ok()?),
            offset + 9,
        )),
        _ => None,
    }
}

fn cpu_count_from_madt(tables: &acpi::AcpiTables<AcpiMapper>) -> Option<usize> {
    let madt = tables.find_table::<Madt>()?;
    if let Err(err) = madt.get().validate() {
        printk!("[kernel-start][acpi] invalid MADT: {:?}", err);
        return None;
    }

    let madt_bytes = unsafe {
        slice::from_raw_parts(madt.virtual_start.as_ptr().cast::<u8>(), madt.region_length)
    };
    let count = count_enabled_processors_from_madt(madt_bytes);
    if count != 0 {
        printk!("[kernel-start][acpi] MADT CPU count: {}", count);
        Some(count)
    } else {
        None
    }
}

fn count_enabled_processors_from_madt(madt: &[u8]) -> usize {
    let header_size = mem::size_of::<Madt>();
    if madt.len() < header_size {
        return 0;
    }

    let mut count = 0usize;
    let mut offset = header_size;
    while offset + 2 <= madt.len() {
        let entry_type = madt[offset];
        let entry_len = madt[offset + 1] as usize;
        if entry_len < 2 || offset + entry_len > madt.len() {
            break;
        }
        let entry = &madt[offset..offset + entry_len];
        if madt_processor_enabled(entry_type, entry) {
            count += 1;
        }
        offset += entry_len;
    }
    count
}

fn madt_processor_enabled(entry_type: u8, entry: &[u8]) -> bool {
    match entry_type {
        ACPI_MADT_TYPE_LOCAL_APIC => read_u32_le(entry, 4)
            .map(|flags| flags & ACPI_MADT_ENABLED != 0)
            .unwrap_or(false),
        ACPI_MADT_TYPE_GENERIC_INTERRUPT => read_u32_le(entry, 20)
            .map(|flags| flags & (ACPI_MADT_ENABLED | ACPI_MADT_ONLINE_CAPABLE) != 0)
            .unwrap_or(false),
        ACPI_MADT_TYPE_CORE_PIC => read_u32_le(entry, 11)
            .map(|flags| flags & ACPI_MADT_ENABLED != 0)
            .unwrap_or(false),
        _ => false,
    }
}

fn serial_port_from_spcr(tables: &acpi::AcpiTables<AcpiMapper>) -> Option<SerialPortInfo> {
    let spcr = tables.find_table::<Spcr>()?;
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

    Some(SerialPortInfo {
        name,
        phys_addr,
        clock_hz,
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

fn spcr_namespace_name(spcr: &Spcr) -> Option<&'static str> {
    let name = spcr.namespace_string().ok()?;
    let name = name.trim_matches('\0').trim();
    if name.is_empty() || name == "." {
        return None;
    }
    Some(name.to_string().leak())
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
