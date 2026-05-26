//! 基于 DTB 的内核启动初始化逻辑。
//!
//! 本模块负责解析启动上下文中的 DTB，完成内存管理、设备发现与注册、
//! 文件系统挂载以及控制台绑定等核心启动流程。
//!
//! 驱动对象采用静态生命周期，通过 `Box::leak` 分配，因为内核启动阶段
//! 创建的资源将伴随系统整个生命周期，无需释放。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use allocator::{KERNEL_ALLOCATOR, MemorySegment};
use general::dev::block::{BlockDevice, BlockDeviceKind};
use general::dev::block_sync::SyncBlockBackend;
use general::dev::char::{CharDevice, CharDeviceKind};
use general::dev::drivers::{Uart16550, VirtioBlk, VirtioPciBlkDriver};
use general::dev::enumerate::DEVICES;
use general::dev::pci::pci_scan_and_register;
use general::dev::pnp::{PNP_DRIVERS, PnpDevtmpfsCallbacks, PnpError, set_devtmpfs_callbacks};
use general::dtb::{Dtb, DtbNode, DtbReserveEntry};
use general::firmware::SerialPortInfo;
use general::firmware::power::{
    PowerAccessWidth, PowerControlInfo, PowerControlMethod, PowerRegister, PowerRegisterSpace,
};
use general::vfs::FS_REGISTRY;
use general::vfs::VfsContext;
use general::vfs::cred::Credentials;
use general::vfs::dentry::VfsRoot;
use general::vfs::devtmpfs::DevTmpfsSuperblockOps;
use general::vfs::error::VfsError;
use general::vfs::limits::VfsLimits;
use general::vfs::mount::{Mount, MountFlags, MountNamespace};
use general::vfs::path::{self, Dirfd, LookupFlags};
use general::vfs::stat::FileMode;
use general::vfs::superblock::{FsDriver, Superblock};
use general::{StartContext, StartFirmware};
use log::{LogRecord, LogSink, printk};

use crate::start;

mod pci;

// DTB 兼容字符串常量
const DTB_COMPAT_SYSCON_POWEROFF: &[u8] = b"syscon-poweroff";
const DTB_COMPAT_SYSCON_REBOOT: &[u8] = b"syscon-reboot";
const DTB_COMPAT_VIRTIO_MMIO: &[u8] = b"virtio,mmio";

/// DTB 启动路径的主入口。
///
/// 从 `StartContext` 中提取 DTB 固件视图，依次完成平台基础信息解析、
/// 内存分配器初始化、设备枚举和注册、tmpfs/devtmpfs 文件系统挂载、
/// 控制台注册以及日志系统绑定。
pub fn kernel_start_init(context: &StartContext) {
    log::debug!("[kernel-start][dtb] jumped into kernel_start_init()");

    // 取出 DTB 视图
    let dtb = match context.firmware {
        StartFirmware::Dtb(dtb) => dtb,
        StartFirmware::Acpi(_) => {
            panic!("[kernel-start][dtb] StartContext firmware does not match DTB path")
        }
    };

    // ── 步骤 1：解析 DTB 平台信息 ────────────────────────────────────────

    let cpu_count = count_cpus(dtb);
    let (serial_ports, console_serial_port_index) = parse_stdout_serial(dtb);
    let power_controls = parse_power_controls(dtb);
    let external_initramfs_range = parse_initramfs_range(dtb);
    let mut memory_segments = parse_memory_segments(dtb)
        .unwrap_or_else(|err| panic!("[kernel-start][dtb] failed to parse DTB memory: {}", err));

    if let Some(boot_segments) = context.memory.boot_map.usable_segments() {
        memory_segments = start::intersect_memory_segments(&memory_segments, &boot_segments)
            .unwrap_or_else(|| {
                panic!(
                    "[kernel-start][dtb] DTB memory description does not overlap usable boot memory"
                )
            });
    }

    // ── 步骤 2：安装电源控制回调 ─────────────────────────────────────────

    general::firmware::power::install(power_controls, context.address.phys_to_virt);

    // ── 步骤 3：初始化分层内存分配器 ─────────────────────────────────────

    KERNEL_ALLOCATOR
        .bind_address_translation(context.address.phys_to_virt, context.address.virt_to_phys);

    let kernel_image = context.memory.kernel_image;
    let mut kernel_reserved = Vec::new();
    kernel_reserved.push((kernel_image.start, kernel_image.end));
    if let Some((start, end)) = external_initramfs_range {
        kernel_reserved.push((start, end));
        printk!(
            "[kernel-start][dtb] external initramfs reserved: phys={:#x}..{:#x} ({} bytes)",
            start,
            end,
            end - start
        );
    }

    KERNEL_ALLOCATOR
        .init_phys(&memory_segments, &kernel_reserved)
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][dtb] failed to init physical allocator: {:?}",
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
            .unwrap_or_else(|err| panic!("[kernel-start][dtb] failed to init vmem: {:?}", err));
        KERNEL_ALLOCATOR.init_kheap();
        KERNEL_ALLOCATOR.init_slab(cpu_count);
        KERNEL_ALLOCATOR.activate_global().unwrap_or_else(|err| {
            panic!(
                "[kernel-start][dtb] failed to activate global allocator: {:?}",
                err
            )
        });
    }

    printk!(
        "[kernel-start][dtb] memory allocator ready: {} RAM segment(s)",
        memory_segments.len()
    );

    let cmdline = context.boot.command_line.map(crate::cmdline::Cmdline::new);
    let external_initramfs = external_initramfs_range.map(|(start, end)| {
        let virt = (context.address.phys_to_virt)(start);
        let len = end - start;
        let bytes = unsafe { core::slice::from_raw_parts(virt as *const u8, len) };
        crate::initramfs::InitramfsImage {
            bytes,
            source: crate::initramfs::InitramfsSource::External,
        }
    });

    // ── 步骤 4：发现并注册 DTB 设备 ─────────────────────────────────────

    let mut uart_index = 0usize;
    let mut block_index = 0usize;
    let mut char_dev_bindings: Vec<(&'static str, CharDevice)> = Vec::new();

    for node in dtb.children().into_iter().flatten() {
        register_dtb_device_node(
            node,
            context.address.device_mmio_to_virt,
            context.address.virt_to_phys,
            &mut uart_index,
            &mut block_index,
            &mut char_dev_bindings,
        );
    }

    printk!(
        "[kernel-start][dtb] device discovery complete: {} uart(s), {} block device(s)",
        uart_index,
        block_index
    );

    // ── 步骤 5：先确认根目录来源，再挂载根文件系统与 /dev ─────────────

    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::TmpfsDriver)))
        .expect("[kernel-start][dtb] failed to register tmpfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::DevTmpfsDriver)))
        .expect("[kernel-start][dtb] failed to register devtmpfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::ProcFsDriver)))
        .expect("[kernel-start][dtb] failed to register procfs driver");
    general::vfs::register_block_filesystems();

    let dev_sb = FS_REGISTRY
        .find("devtmpfs")
        .expect("[kernel-start][dtb] devtmpfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][dtb] failed to mount devtmpfs");
    let dev_ops = dev_sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .expect("[kernel-start][dtb] failed to downcast devtmpfs ops");
    bind_standard_devtmpfs_nodes(dev_ops);

    // devtmpfs superblock 先建好并注入给 PnP。它不需要先挂载到某个临时根；
    // 总线扫描期间发现的设备节点会写入 devtmpfs，等根目录确定后再挂到 /dev。
    init_pnp_and_pci(
        dtb,
        &dev_sb,
        context.address.device_mmio_to_virt,
        context.address.virt_to_phys,
    );

    let selected_initramfs = external_initramfs.or_else(crate::initramfs::embedded_image);
    let cred = Credentials::root();
    let (root_sb, root_source) = if let Some(image) = selected_initramfs {
        let sb = mount_tmpfs_superblock();
        (
            sb,
            match image.source {
                crate::initramfs::InitramfsSource::Embedded => "embedded initramfs",
                crate::initramfs::InitramfsSource::External => "external initramfs",
            },
        )
    } else {
        let dev = DEVICES
            .block_devs
            .lookup("vd0")
            .unwrap_or_else(|| panic!("[kernel-start][dtb] no initramfs and /dev/vd0 not found"));
        mount_block_root(Arc::clone(&dev)).unwrap_or_else(|err| {
            panic!(
                "[kernel-start][dtb] failed to mount /dev/{} as root: {}",
                dev.name(),
                err
            )
        })
    };
    printk!("[kernel-start][dtb] root source selected: {}", root_source);

    let root_mount = Mount::new(
        Arc::clone(&root_sb),
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_sb.root_dentry),
        MountFlags::default(),
        None,
    );
    let mount_ns = MountNamespace::new(1, Arc::clone(&root_mount));

    let vfs_ctx = VfsContext::new(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        VfsRoot::new(Arc::clone(&root_sb.root_dentry), Arc::clone(&root_mount)),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
        FileMode::new(0),
        VfsLimits::default_arc(),
    );

    if let Some(image) = selected_initramfs {
        crate::initramfs::unpack_newc(image, &vfs_ctx).unwrap_or_else(|err| {
            panic!("[kernel-start][dtb] failed to unpack initramfs: {:?}", err)
        });
        printk!("[kernel-start][dtb] initramfs unpacked into root");
    }

    ensure_dir(&vfs_ctx, "/dev", FileMode::new(0o755))
        .expect("[kernel-start][dtb] failed to ensure /dev directory");
    let dev_dentry = path::lookup(&vfs_ctx, &Dirfd::Cwd, "/dev", LookupFlags::DIRECTORY)
        .expect("[kernel-start][dtb] failed to resolve /dev")
        .dentry;
    mount_ns
        .mount(dev_dentry, Arc::clone(&dev_sb), MountFlags::default())
        .expect("[kernel-start][dtb] failed to mount devtmpfs on /dev");

    bind_boot_devices_to_devtmpfs(dev_ops, &char_dev_bindings);

    printk!(
        "[kernel-start][dtb] VFS ready: '{}' mounted as '/' + devtmpfs '/dev'",
        root_source
    );

    // ── 步骤 6：注册控制台并绑定日志输出 ────────────────────────────────

    // 把同一套部件交给 sched shim 保管：随后 sched::boot_init 会据此给 init
    // 任务挂上 TASKEXT_VFS_CONTEXT / TASKEXT_VFS_FDTABLE。
    crate::sched::stash_boot_vfs_parts(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
    );

    let console_registered = {
        let cmdline_dev = cmdline
            .as_ref()
            .and_then(|cl| cl.console_device())
            .and_then(|name| resolve_cmdline_console(&vfs_ctx, dev_ops, name));

        let dev = if let Some(dev) = cmdline_dev {
            printk!(
                "[kernel-start][dtb] console from cmdline: {}",
                dev.fw_name()
            );
            Some(dev)
        } else if let Some(port) = console_serial_port_index.and_then(|i| serial_ports.get(i)) {
            let virt_base = (context.address.device_mmio_to_virt)(port.phys_addr);
            let found = DEVICES.char_devs.iter().find(|dev| {
                dev.downcast_driver::<Uart16550>()
                    .is_some_and(|uart| uart.base() == virt_base)
            });
            if let Some(dev) = found.as_ref() {
                printk!(
                    "[kernel-start][dtb] console from firmware: {}",
                    dev.fw_name()
                );
            }
            found
        } else {
            None
        };

        if let Some(dev) = dev {
            general::console::register_console(dev.clone());
            // 在 devtmpfs 里把当前 console 重定向为固定路径 /dev/console，让
            // 用户态进程通过稳定路径打开它。
            match dev_ops.bind_char("console", dev.clone()) {
                Ok(()) => {
                    printk!(
                        "[kernel-start][dtb] bound /dev/console -> {}",
                        dev.fw_name()
                    );
                    crate::sched::stash_boot_console_name(alloc::string::String::from(
                        "/dev/console",
                    ));
                }
                Err(VfsError::AlreadyExists) => {
                    printk!("[kernel-start][dtb] /dev/console already exists; using it for stdio");
                    crate::sched::stash_boot_console_name(alloc::string::String::from(
                        "/dev/console",
                    ));
                }
                Err(err) => {
                    printk!("[kernel-start][dtb] failed to bind /dev/console: {:?}", err);
                }
            }
            true
        } else {
            printk!("[kernel-start][dtb] no console registered");
            false
        }
    };

    if console_registered {
        static LOG_SINK: LogSink = LogSink {
            write_record: write_log_record_to_console,
        };
        log::bind_log_sink(&LOG_SINK);
    }

    printk!("[kernel-start][dtb] kernel initialization complete, jumping to main entry");
}

/// 日志记录回调：将格式化的日志行发送到控制台。
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
    DEVICES.char_devs.lookup(name)
}

fn mount_tmpfs_superblock() -> Arc<Superblock> {
    FS_REGISTRY
        .find("tmpfs")
        .expect("[kernel-start][dtb] tmpfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][dtb] failed to mount tmpfs root")
}

fn mount_block_root(
    dev: Arc<BlockDevice>,
) -> Result<(Arc<Superblock>, &'static str), &'static str> {
    let ext_driver = Box::leak(Box::new(extfs::ExtFsDriver::new()));
    ext_driver.bind_backend(Arc::new(SyncBlockBackend::new(Arc::clone(&dev))));
    if let Ok(sb) = ext_driver.mount(None, "") {
        return Ok((sb, "/dev/vd0 (extfs)"));
    }

    let fat_driver = Box::leak(Box::new(fatfs::FatFsDriver::new()));
    fat_driver.bind_backend(Arc::new(SyncBlockBackend::new(dev)));
    if let Ok(sb) = fat_driver.mount(None, "") {
        return Ok((sb, "/dev/vd0 (fatfs)"));
    }

    Err("unsupported or invalid root filesystem")
}

fn ensure_dir(ctx: &VfsContext, path: &str, mode: FileMode) -> general::vfs::error::VfsResult<()> {
    match path::lookup(ctx, &Dirfd::Cwd, path, LookupFlags::DIRECTORY) {
        Ok(_) => Ok(()),
        Err(VfsError::NotFound) => general::vfs::operation::mkdirat(ctx, &Dirfd::Cwd, path, mode),
        Err(err) => Err(err),
    }
}

fn bind_boot_devices_to_devtmpfs(
    dev_ops: &DevTmpfsSuperblockOps,
    char_dev_bindings: &[(&'static str, CharDevice)],
) {
    for (user_name, dev) in char_dev_bindings {
        match dev_ops.bind_char(user_name, dev.clone()) {
            Ok(()) | Err(VfsError::AlreadyExists) => {}
            Err(err) => {
                printk!(
                    "[kernel-start][dtb] failed to bind char dev '{}' (fw: {}) to /dev: {:?}",
                    user_name,
                    dev.fw_name(),
                    err
                );
            }
        }
    }

    match DEVICES.block_devs.list() {
        Ok(block_devs) => {
            for dev in block_devs {
                match dev_ops.bind_block(dev.name(), Arc::clone(&dev)) {
                    Ok(()) | Err(VfsError::AlreadyExists) => {}
                    Err(err) => {
                        printk!(
                            "[kernel-start][dtb] failed to bind block dev '{}' to /dev: {:?}",
                            dev.name(),
                            err
                        );
                    }
                }
            }
        }
        Err(err) => {
            printk!(
                "[kernel-start][dtb] failed to enumerate block devices for devtmpfs: {:?}",
                err
            );
        }
    }
}

fn bind_standard_devtmpfs_nodes(dev_ops: &DevTmpfsSuperblockOps) {
    for (name, dev) in [("null", CharDevice::null()), ("zero", CharDevice::zero())] {
        match dev_ops.bind_char(name, dev) {
            Ok(()) | Err(VfsError::AlreadyExists) => {}
            Err(err) => {
                printk!(
                    "[kernel-start][dtb] failed to bind standard /dev/{}: {:?}",
                    name,
                    err
                );
            }
        }
    }
}

fn parse_initramfs_range(dtb: Dtb<'static>) -> Option<(usize, usize)> {
    let chosen = dtb.find_child("chosen")?;
    let start = read_dtb_usize(chosen.find_property("linux,initrd-start")?.value())?;
    let end = read_dtb_usize(chosen.find_property("linux,initrd-end")?.value())?;
    (end > start).then_some((start, end))
}

fn read_dtb_usize(value: &[u8]) -> Option<usize> {
    match value.len() {
        4 => Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]) as usize),
        8 => {
            let raw = u64::from_be_bytes([
                value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
            ]);
            if raw > usize::MAX as u64 {
                None
            } else {
                Some(raw as usize)
            }
        }
        _ => None,
    }
}

/// 递归遍历 DTB 节点，注册串口和 virtio 块设备。
fn register_dtb_device_node(
    node: DtbNode<'static>,
    device_mmio_to_virt: fn(usize) -> usize,
    virt_to_phys: fn(usize) -> usize,
    uart_index: &mut usize,
    block_index: &mut usize,
    char_dev_bindings: &mut Vec<(&'static str, CharDevice)>,
) {
    register_dtb_serial_node(node, device_mmio_to_virt, uart_index, char_dev_bindings);
    register_dtb_virtio_mmio_node(node, device_mmio_to_virt, virt_to_phys, block_index);
    for child in node.children() {
        register_dtb_device_node(
            child,
            device_mmio_to_virt,
            virt_to_phys,
            uart_index,
            block_index,
            char_dev_bindings,
        );
    }
}

/// 注册一个 DTB 串口节点为 ns16550 字符设备。
fn register_dtb_serial_node(
    node: DtbNode<'static>,
    device_mmio_to_virt: fn(usize) -> usize,
    uart_index: &mut usize,
    char_dev_bindings: &mut Vec<(&'static str, CharDevice)>,
) {
    if node.base_name_bytes() != b"serial" {
        return;
    }
    let reg_prop = match node.find_property("reg") {
        Some(p) => p,
        None => return,
    };
    let phys_addr = match parse_reg_addr(reg_prop.value()) {
        Some(a) => a,
        None => return,
    };

    let clock_hz = node
        .find_property("clock-frequency")
        .and_then(|p| {
            let v = p.value();
            if v.len() >= 4 {
                Some(u32::from_be_bytes(v[0..4].try_into().ok()?))
            } else {
                None
            }
        })
        .unwrap_or(0);

    let virt_base = device_mmio_to_virt(phys_addr);

    // 通过 Box::leak 产生静态引用，伴随内核整个生命周期
    let uart: &'static Uart16550 = if clock_hz != 0 {
        Box::leak(Box::new(Uart16550::new(virt_base, clock_hz, 115_200)))
    } else {
        Box::leak(Box::new(Uart16550::new_preconfigured(virt_base)))
    };

    let idx = *uart_index;
    *uart_index += 1;

    let fw_name: &'static str = node
        .name()
        .unwrap_or_else(|| leak_str(&alloc::format!("serial@{:#x}", phys_addr)));
    let user_name: &'static str = leak_str(&alloc::format!("{}{}", CharDeviceKind::Ns16550.name(), idx));

    let dev = CharDevice::new(CharDeviceKind::Ns16550, fw_name, uart);
    if let Err(err) = DEVICES.char_devs.push(dev.clone()) {
        printk!(
            "[kernel-start][dtb] failed to register {} ({}{}) at {:#x}: {:?}",
            fw_name,
            CharDeviceKind::Ns16550.name(),
            idx,
            phys_addr,
            err
        );
    } else {
        char_dev_bindings.push((user_name, dev));
        printk!(
            "[kernel-start][dtb] registered {} -> /dev/{} at phys={:#x}",
            fw_name,
            user_name,
            phys_addr
        );
    }
}

/// 将字符串转换为 `&'static str`，用于内核中需要静态字符串的场合。
/// 实现采用 `Box::leak`，这会导致内存永久泄漏，仅用于启动阶段一次性分配。
fn leak_str(s: &str) -> &'static str {
    let boxed: Box<str> = s.into();
    Box::leak(boxed)
}

/// 注册 virtio-mmio 块设备。
fn register_dtb_virtio_mmio_node(
    node: DtbNode<'static>,
    device_mmio_to_virt: fn(usize) -> usize,
    virt_to_phys: fn(usize) -> usize,
    block_index: &mut usize,
) {
    if !node_compatible_contains(node, DTB_COMPAT_VIRTIO_MMIO) {
        return;
    }

    let reg_prop = match node.find_property("reg") {
        Some(prop) => prop,
        None => return,
    };
    let (phys_addr, size) = match parse_reg_addr_size(reg_prop.value()) {
        Some(range) => range,
        None => return,
    };
    if size == 0 {
        return;
    }

    let virt_base = device_mmio_to_virt(phys_addr);
    let driver = match VirtioBlk::new(virt_base, virt_to_phys) {
        Ok(driver) => driver,
        Err(err) => {
            printk!(
                "[kernel-start][dtb] skipped virtio-mmio {} at {:#x}: {}",
                node.name().unwrap_or("<unnamed>"),
                phys_addr,
                err
            );
            return;
        }
    };

    let user_name = alloc::format!("{}{}", BlockDeviceKind::VirtioBlk.name(), *block_index);
    let block_dev = match driver.into_block_dev(&user_name, virt_to_phys) {
        Ok(dev) => dev,
        Err(err) => {
            printk!(
                "[kernel-start][dtb] failed to create block dev for {} at {:#x}: {}",
                node.name().unwrap_or("<unnamed>"),
                phys_addr,
                err
            );
            return;
        }
    };

    match DEVICES.block_devs.push(&block_dev) {
        Ok(dev) => {
            printk!(
                "[kernel-start][dtb] registered virtio-blk {} -> /dev/{} at phys={:#x}",
                node.name().unwrap_or("<unnamed>"),
                dev.name(),
                phys_addr
            );
            *block_index += 1;
        }
        Err(err) => {
            printk!(
                "[kernel-start][dtb] failed to register virtio-blk {} at {:#x}: {:?}",
                node.name().unwrap_or("<unnamed>"),
                phys_addr,
                err
            );
        }
    }
}

// ───────────────────── 内存段辅助函数 ─────────────────────────────────

/// 合并、排序并去除零大小的内存段，返回规范化的段列表。
fn normalize_segments(mut segments: Vec<MemorySegment>) -> Option<Vec<MemorySegment>> {
    if segments.is_empty() {
        return None;
    }

    segments.sort_unstable_by_key(|segment| segment.start);
    let mut merged: Vec<MemorySegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        if segment.size == 0 {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            let last_end = last.start.saturating_add(last.size);
            if last_end >= segment.start {
                let merged_end = last_end.max(segment.start.saturating_add(segment.size));
                last.size = merged_end.saturating_sub(last.start);
                continue;
            }
        }
        merged.push(segment);
    }

    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// 根据 `reg` 属性的字节长度推断地址、大小单元数。
fn infer_reg_cells(reg: &[u8], addr_cells: usize, size_cells: usize) -> Option<(usize, usize)> {
    let entry_bytes = (addr_cells + size_cells) * 4;
    if entry_bytes != 0 && reg.len().is_multiple_of(entry_bytes) {
        return Some((addr_cells, size_cells));
    }
    if reg.len().is_multiple_of(16) {
        return Some((2, 2));
    }
    if reg.len().is_multiple_of(8) {
        return Some((1, 1));
    }
    None
}

/// 从字节切片中读取指定 cell 数量的值。
fn read_cells(bytes: &[u8], cells: usize) -> Option<usize> {
    match cells {
        1 => Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize),
        2 => Some(u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?) as usize),
        _ => None,
    }
}

/// 读取 DTB 节点中 `#address-cells` 或 `#size-cells` 的值。
fn read_cells_count(node: DtbNode<'static>, name: &str, default: usize) -> usize {
    node.find_property(name)
        .and_then(|prop| read_be_u32_prop(prop.value()))
        .map(|value| value as usize)
        .filter(|&value| matches!(value, 1 | 2))
        .unwrap_or(default)
}

/// 将 `reg` 属性追加为 `MemorySegment` 列表。
fn append_ranges_from_reg(
    reg: &[u8],
    default_addr_cells: usize,
    default_size_cells: usize,
    segments: &mut Vec<MemorySegment>,
) {
    let Some((addr_cells, size_cells)) =
        infer_reg_cells(reg, default_addr_cells, default_size_cells)
    else {
        return;
    };
    let entry_bytes = (addr_cells + size_cells) * 4;
    let mut cursor = 0usize;
    while cursor + entry_bytes <= reg.len() {
        let addr_end = cursor + addr_cells * 4;
        let size_end = addr_end + size_cells * 4;
        let Some(start) = read_cells(&reg[cursor..addr_end], addr_cells) else {
            break;
        };
        let Some(size) = read_cells(&reg[addr_end..size_end], size_cells) else {
            break;
        };
        cursor = size_end;

        if size != 0 {
            segments.push(MemorySegment { start, size });
        }
    }
}

/// 收集所有保留内存（memreserve 块 + reserved-memory 节点）。
fn collect_reserved_segments(
    dtb: Dtb<'static>,
    root: DtbNode<'static>,
    root_addr_cells: usize,
    root_size_cells: usize,
) -> Vec<MemorySegment> {
    let mut reserved = Vec::new();

    if let Some(entries) = dtb.mem_reservations() {
        for DtbReserveEntry { address, size } in entries {
            if size != 0 {
                reserved.push(MemorySegment {
                    start: address,
                    size,
                });
            }
        }
    }

    if let Some(reserved_memory) = root.find_child("reserved-memory") {
        let addr_cells = read_cells_count(reserved_memory, "#address-cells", root_addr_cells);
        let size_cells = read_cells_count(reserved_memory, "#size-cells", root_size_cells);
        for child in reserved_memory.children() {
            let Some(reg) = child.find_property("reg").map(|prop| prop.value()) else {
                continue;
            };
            append_ranges_from_reg(reg, addr_cells, size_cells, &mut reserved);
        }
    }

    normalize_segments(reserved).unwrap_or_default()
}

/// 从可用段中扣除保留段，返回剩余的可用内存段。
fn subtract_reserved_segments(
    segments: Vec<MemorySegment>,
    reserved: &[MemorySegment],
) -> Option<Vec<MemorySegment>> {
    let segments = normalize_segments(segments)?;
    if reserved.is_empty() {
        return Some(segments);
    }

    let mut result = Vec::new();
    for segment in segments {
        let mut cursor = segment.start;
        let segment_end = segment.end();

        for hole in reserved {
            let hole_start = hole.start;
            let hole_end = hole.end();
            if hole_end <= cursor {
                continue;
            }
            if hole_start >= segment_end {
                break;
            }
            if cursor < hole_start {
                result.push(MemorySegment {
                    start: cursor,
                    size: hole_start - cursor,
                });
            }
            cursor = cursor.max(hole_end.min(segment_end));
            if cursor >= segment_end {
                break;
            }
        }

        if cursor < segment_end {
            result.push(MemorySegment {
                start: cursor,
                size: segment_end - cursor,
            });
        }
    }

    normalize_segments(result)
}

/// 解析 DTB 的 `/memory` 节点，获得系统可用物理内存段。
fn parse_memory_segments(dtb: Dtb<'static>) -> Result<Vec<MemorySegment>, &'static str> {
    let root = dtb
        .root()
        .ok_or("[kernel-start][dtb] missing or invalid DTB root node")?;
    let root_addr_cells = read_cells_count(root, "#address-cells", 2);
    let root_size_cells = read_cells_count(root, "#size-cells", 2);
    let mut segments = Vec::new();

    for node in dtb
        .children()
        .ok_or("[kernel-start][dtb] failed to iterate DTB root children")?
    {
        if node.base_name_bytes() != b"memory" {
            continue;
        }

        let Some(reg) = node.find_property("reg").map(|prop| prop.value()) else {
            continue;
        };
        append_ranges_from_reg(reg, root_addr_cells, root_size_cells, &mut segments);
    }

    let Some(raw_segments) = normalize_segments(segments) else {
        return Err("[kernel-start][dtb] no usable memory node found");
    };
    let reserved = collect_reserved_segments(dtb, root, root_addr_cells, root_size_cells);
    let memory_segments = subtract_reserved_segments(raw_segments, &reserved)
        .ok_or("[kernel-start][dtb] no usable RAM remains after subtracting reserved regions")?;

    printk!(
        "[kernel-start][dtb] memory segments: usable={} reserved={} root-cells=({},{})",
        memory_segments.len(),
        reserved.len(),
        root_addr_cells,
        root_size_cells,
    );

    Ok(memory_segments)
}

/// 统计 CPU 核心数。
fn count_cpus(dtb: Dtb<'static>) -> usize {
    dtb.find_child("cpus")
        .map(|cpus| {
            cpus.children()
                .filter(|node| node.base_name_bytes() == b"cpu")
                .count()
        })
        .unwrap_or(1)
        .max(1)
}

/// 解析 `reg` 属性中的单个地址（大端序）。
fn parse_reg_addr(reg: &[u8]) -> Option<usize> {
    if reg.len() >= 16 {
        Some(u64::from_be_bytes(reg[0..8].try_into().ok()?) as usize)
    } else if reg.len() >= 4 {
        Some(u32::from_be_bytes(reg[0..4].try_into().ok()?) as usize)
    } else {
        None
    }
}

/// 解析 `reg` 属性中的地址/大小对。
fn parse_reg_addr_size(reg: &[u8]) -> Option<(usize, usize)> {
    if reg.len() >= 16 {
        Some((
            u64::from_be_bytes(reg[0..8].try_into().ok()?) as usize,
            u64::from_be_bytes(reg[8..16].try_into().ok()?) as usize,
        ))
    } else if reg.len() >= 8 {
        Some((
            u32::from_be_bytes(reg[0..4].try_into().ok()?) as usize,
            u32::from_be_bytes(reg[4..8].try_into().ok()?) as usize,
        ))
    } else {
        None
    }
}

/// 解析关机/重启等电源控制方法。
fn parse_power_controls(dtb: Dtb<'static>) -> PowerControlInfo {
    let shutdown = parse_syscon_power_action(dtb, DTB_COMPAT_SYSCON_POWEROFF);
    let reboot = parse_syscon_power_action(dtb, DTB_COMPAT_SYSCON_REBOOT);

    printk!(
        "[kernel-start][dtb] power controls: shutdown={} reboot={}",
        shutdown.is_some() as usize,
        reboot.is_some() as usize
    );

    PowerControlInfo { shutdown, reboot }
}

/// 解析 syscon 类型的电源控制（开关机/重启）。
fn parse_syscon_power_action(dtb: Dtb<'static>, compatible: &[u8]) -> Option<PowerControlMethod> {
    let action_node = find_node_by_compatible(dtb, compatible)?;
    let regmap_phandle = read_be_u32_prop(action_node.find_property("regmap")?.value())?;
    let offset = read_be_usize_prop(action_node.find_property("offset")?.value())?;
    let value = read_be_u64_prop(action_node.find_property("value")?.value())?;
    let regmap_node = find_node_by_phandle(dtb, regmap_phandle)?;
    let (base, size) = parse_reg_addr_size(regmap_node.find_property("reg")?.value())?;
    let width_bytes = regmap_node
        .find_property("reg-io-width")
        .and_then(|prop| read_be_usize_prop(prop.value()))
        .or_else(|| {
            if size != 0 && offset < size && size - offset < 4 {
                Some(1)
            } else {
                Some(4)
            }
        })?;
    let access_width = PowerAccessWidth::from_bytes(width_bytes)?;
    let address = base.checked_add(offset)?;

    printk!(
        "[kernel-start][dtb] power {:?}: syscon={} phys={:#x} offset={:#x} value={:#x} width={}B",
        core::str::from_utf8(compatible).unwrap_or("<invalid>"),
        regmap_node.name().unwrap_or("<unnamed>"),
        address,
        offset,
        value,
        width_bytes
    );

    Some(PowerControlMethod::RegisterWrite {
        register: PowerRegister {
            space: PowerRegisterSpace::SystemMemory,
            address,
            access_width,
        },
        value,
    })
}

/// 读大端 u32 属性值。
fn read_be_u32_prop(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.get(..4)?.try_into().ok()?))
}

/// 读大端 usize 属性值（兼容 u32 和 u64）。
fn read_be_usize_prop(value: &[u8]) -> Option<usize> {
    if value.len() < 4 {
        None
    } else if value.len() < 8 {
        read_be_u32_prop(value).map(|value| value as usize)
    } else {
        Some(u64::from_be_bytes(value.get(..8)?.try_into().ok()?) as usize)
    }
}

/// 读大端 u64 属性值。
fn read_be_u64_prop(value: &[u8]) -> Option<u64> {
    if value.len() < 4 {
        None
    } else if value.len() < 8 {
        read_be_u32_prop(value).map(|value| value as u64)
    } else {
        Some(u64::from_be_bytes(value.get(..8)?.try_into().ok()?))
    }
}

/// 按 compatible 字符串查找节点（深度优先）。
fn find_node_by_compatible(dtb: Dtb<'static>, compatible: &[u8]) -> Option<DtbNode<'static>> {
    for node in dtb.children()? {
        if node_compatible_contains(node, compatible) {
            return Some(node);
        }
        if let Some(found) = find_child_by_compatible(node, compatible) {
            return Some(found);
        }
    }
    None
}

fn find_child_by_compatible(node: DtbNode<'static>, compatible: &[u8]) -> Option<DtbNode<'static>> {
    for child in node.children() {
        if node_compatible_contains(child, compatible) {
            return Some(child);
        }
        if let Some(found) = find_child_by_compatible(child, compatible) {
            return Some(found);
        }
    }
    None
}

/// 检查节点的 compatible 属性是否包含指定的字符串。
fn node_compatible_contains(node: DtbNode<'static>, compatible: &[u8]) -> bool {
    let Some(prop) = node.find_property("compatible") else {
        return false;
    };
    prop.value()
        .split(|&byte| byte == 0)
        .any(|entry| entry == compatible)
}

/// 按 phandle 值查找节点。
fn find_node_by_phandle(dtb: Dtb<'static>, phandle: u32) -> Option<DtbNode<'static>> {
    for node in dtb.children()? {
        if node_phandle_matches(node, phandle) {
            return Some(node);
        }
        if let Some(found) = find_child_by_phandle(node, phandle) {
            return Some(found);
        }
    }
    None
}

fn find_child_by_phandle(node: DtbNode<'static>, phandle: u32) -> Option<DtbNode<'static>> {
    for child in node.children() {
        if node_phandle_matches(child, phandle) {
            return Some(child);
        }
        if let Some(found) = find_child_by_phandle(child, phandle) {
            return Some(found);
        }
    }
    None
}

fn node_phandle_matches(node: DtbNode<'static>, phandle: u32) -> bool {
    ["phandle", "linux,phandle"].iter().any(|name| {
        node.find_property(name)
            .and_then(|prop| read_be_u32_prop(prop.value()))
            == Some(phandle)
    })
}

/// 解析 stdout-path 获得控制台串口信息和索引。
fn parse_stdout_serial(dtb: Dtb<'static>) -> (Vec<SerialPortInfo>, Option<usize>) {
    let Some(console_path) = parse_console_path(dtb) else {
        printk!("[kernel-start][dtb] stdout-path not present; serial not registered");
        return (Vec::new(), None);
    };
    let Some(node) = find_node_by_path(dtb, console_path) else {
        printk!(
            "[kernel-start][dtb] stdout-path '{}' does not resolve to a node",
            console_path,
        );
        return (Vec::new(), None);
    };
    let Some(port) = serial_port_from_node(node, console_path) else {
        printk!(
            "[kernel-start][dtb] stdout-path '{}' is not a complete ns16550 node",
            console_path,
        );
        return (Vec::new(), None);
    };

    if let Some(clock_hz) = port.clock_hz {
        printk!(
            "[kernel-start][dtb] stdout serial: {} phys={:#x} clock={}Hz",
            port.name,
            port.phys_addr,
            clock_hz,
        );
    } else {
        printk!(
            "[kernel-start][dtb] stdout serial: {} phys={:#x} clock=<firmware-configured>",
            port.name,
            port.phys_addr,
        );
    }

    (alloc::vec![port], Some(0))
}

/// 从节点提取 SerialPortInfo。
fn serial_port_from_node(node: DtbNode<'static>, path: &'static str) -> Option<SerialPortInfo> {
    if node.base_name_bytes() != b"serial" {
        return None;
    }
    let reg_prop = node.find_property("reg")?;
    let phys_addr = parse_reg_addr(reg_prop.value())?;
    let clock_hz = node.find_property("clock-frequency").and_then(|p| {
        let v = p.value();
        if v.len() >= 4 {
            Some(u32::from_be_bytes(v[0..4].try_into().ok()?))
        } else {
            None
        }
    });

    Some(SerialPortInfo {
        name: node.name().unwrap_or(path),
        phys_addr,
        clock_hz,
    })
}

/// 解析 stdout-path 属性，去掉可能的冒号和参数部分。
fn parse_stdout_path(val: &'static [u8]) -> Option<&'static str> {
    let trimmed_end = val
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let s = core::str::from_utf8(&val[..trimmed_end]).ok()?;
    let s = s.trim_start_matches('/');
    Some(s.split(':').next().unwrap_or(s))
}

/// 从 DTB 的 chosen 节点或 aliases 中获取控制台路径。
fn parse_console_path(dtb: Dtb<'static>) -> Option<&'static str> {
    let chosen = dtb.find_child("chosen")?;
    let stdout_prop = chosen.find_property("stdout-path")?;
    let requested = parse_stdout_path(stdout_prop.value())?;

    if let Some(alias_node) = dtb.find_child("aliases")
        && let Some(alias_prop) = alias_node.find_property(requested)
        && let Some(resolved) = parse_stdout_path(alias_prop.value())
    {
        return Some(resolved);
    }

    Some(requested)
}

/// 根据路径字符串在 DTB 中查找节点。
fn find_node_by_path(dtb: Dtb<'static>, path: &str) -> Option<DtbNode<'static>> {
    let mut components = path
        .trim_start_matches('/')
        .split('/')
        .filter(|component| !component.is_empty());
    let first = components.next()?;
    let mut node = dtb.find_child(first)?;
    for component in components {
        node = node.find_child(component)?;
    }
    Some(node)
}

// ── PnP + PCI 初始化 ────────────────────────────────────────────────────

/// 装 devtmpfs 回调、解析 DTB pcie 节点、注册 virtio-pci 驱动,最后扫描总线。
///
/// 调用前提:
/// - devtmpfs superblock 已创建；它可以尚未挂到最终 `/dev`。
/// - 任何在 bus 扫描过程中被匹配上的驱动都会调用
///   [`PnpDevice::register_block_function`](general::dev::pnp::PnpDevice::register_block_function),
///   其中转发给 devtmpfs 的路径靠本函数注入的回调完成。
fn init_pnp_and_pci(
    dtb: Dtb<'static>,
    dev_sb: &Arc<general::vfs::superblock::Superblock>,
    device_mmio_to_virt: fn(usize) -> usize,
    virt_to_phys: fn(usize) -> usize,
) {
    // (a) 给 PnP 注入 devtmpfs 回调。DevTmpfsSuperblockOps 通过 Arc<Superblock>
    //     downcast 获取,所以先把 ops 指针以 'static 形式存到一个 static 里。
    //     这里用 Box::leak 的方式把一份克隆的 Arc 泄漏到静态生命周期。
    use general::vfs::devtmpfs::DevTmpfsSuperblockOps as DtOps;
    let sb_leaked: &'static Arc<general::vfs::superblock::Superblock> =
        Box::leak(Box::new(Arc::clone(dev_sb)));

    fn bind_block_cb(name: &str, dev: Arc<BlockDevice>) -> Result<(), PnpError> {
        let sb = devtmpfs_sb();
        let ops = sb.downcast_ops::<DtOps>().ok_or(PnpError::NoDevtmpfs)?;
        ops.bind_block(name, dev)
            .map_err(|_| PnpError::DevtmpfsError)
    }
    fn bind_char_cb(name: &str, dev: CharDevice) -> Result<(), PnpError> {
        let sb = devtmpfs_sb();
        let ops = sb.downcast_ops::<DtOps>().ok_or(PnpError::NoDevtmpfs)?;
        ops.bind_char(name, dev)
            .map_err(|_| PnpError::DevtmpfsError)
    }
    fn unbind_cb(name: &str) -> Result<(), PnpError> {
        let sb = devtmpfs_sb();
        let ops = sb.downcast_ops::<DtOps>().ok_or(PnpError::NoDevtmpfs)?;
        ops.unbind(name).map_err(|_| PnpError::DevtmpfsError)
    }

    // 用静态闭包无法捕获 sb,只能靠静态变量共享引用。
    set_pnp_devtmpfs_sb(sb_leaked);
    set_devtmpfs_callbacks(PnpDevtmpfsCallbacks {
        bind_block: bind_block_cb,
        bind_char: bind_char_cb,
        unbind: unbind_cb,
    });
    printk!("[kernel-start][dtb] PnP devtmpfs callbacks installed");

    // (b) 解析 pcie 节点。没有就跳过。
    let Some((phys, size, bus_s, bus_e)) = pci::parse_pcie_node(dtb) else {
        printk!("[kernel-start][dtb] no pcie node in DTB; skipping PCI init");
        return;
    };
    printk!(
        "[kernel-start][dtb] pcie ECAM phys={:#x} size={:#x} bus=[{:#x},{:#x}]",
        phys,
        size,
        bus_s,
        bus_e
    );
    pci::install_ecam(phys, size, bus_s, bus_e, device_mmio_to_virt);

    // (c) 注册 virtio-pci 块设备驱动。
    let drv = Box::leak(Box::new(VirtioPciBlkDriver::new(virt_to_phys)));
    PNP_DRIVERS.register(drv);
    printk!("[kernel-start][dtb] registered PnpDriver 'virtio-pci-blk'");

    // (c.5) 直接 `-kernel` 引导下 QEMU 不会为 PCI 设备分配 BAR,
    //       这里遍历一次,从 PCI MMIO 窗口按需切片。
    pci::assign_bars(bus_s, bus_e);

    // (d) 扫描总线。每个被匹配的设备会自动经 PnP → devtmpfs 挂到 /dev/vd*。
    let count = pci_scan_and_register(0, bus_s, bus_e, "pci-");
    printk!(
        "[kernel-start][dtb] pci_scan_and_register probed {} device(s)",
        count
    );
}

// devtmpfs superblock 全局桥梁,PnP 回调由 static fn 实现,需要从这里拿 sb。
static PNP_DEVTMPFS_SB: vfs::sync::Spinlock<
    Option<&'static Arc<general::vfs::superblock::Superblock>>,
> = vfs::sync::Spinlock::new(None);

fn set_pnp_devtmpfs_sb(sb: &'static Arc<general::vfs::superblock::Superblock>) {
    *PNP_DEVTMPFS_SB.lock() = Some(sb);
}

fn devtmpfs_sb() -> &'static Arc<general::vfs::superblock::Superblock> {
    PNP_DEVTMPFS_SB.lock().expect("devtmpfs sb not installed")
}
