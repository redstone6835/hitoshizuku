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

use allocator::KERNEL_ALLOCATOR;
use general::dev::block::BlockDevice;
use general::dev::char::CharDevice;
use general::dev::drivers;
use general::dev::drivers::{RANDOM_DRIVER, URANDOM_DRIVER};
use general::dev::enumerate::DEVICES;
use general::dev::function::{active_block_devices, find_char_device_by_fw_name};
use general::dev::pci::pci_scan_and_register;
use general::dev::platform::{
    DeviceMatchId, DeviceProperties, DeviceResource, PlatformDeviceInfo,
    register_and_probe_platform_device,
};
use general::dev::pnp::{DevInitContext, set_dev_init_context};
use general::firmware::dtb as firmware_dtb;
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
use general::vfs::superblock::Superblock;
use general::{StartContext, StartFirmware};
use log::{LogRecord, LogSink, printk};

use crate::start;

mod pci;

/// DTB 启动路径的主入口。
///
/// 从 `StartContext` 中提取 DTB 固件视图，依次完成平台基础信息解析、
/// 内存分配器初始化、设备枚举和注册、tmpfs/devtmpfs 文件系统挂载、
/// 控制台注册以及日志系统绑定。
pub fn kernel_start_init(context: &StartContext) {
    log::debug!("[kernel-start][dtb] jumped into kernel_start_init()");

    // 步骤 0 先确认本次启动确实走的是 DTB 固件路径，并拿到稳定 DTB 视图。
    // 只有在这里把 DTB 视图固定下来，后续所有解析步骤才有共同的语义起点。
    let dtb = match context.firmware {
        StartFirmware::Dtb(dtb) => dtb,
        StartFirmware::Acpi(_) => {
            panic!("[kernel-start][dtb] StartContext firmware does not match DTB path")
        }
    };
    // 步骤 1 先把原始 DTB 交给平台无关的固件解析器。解析器会统一建立全树索引、
    // 处理 aliases/phandle/status/reg/ranges，并产出内核启动需要的标准描述符；
    // 这里不再假设设备一定挂在根节点或 `/soc` 下。
    let firmware = firmware_dtb::parse(dtb).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][dtb] failed to parse DTB firmware info: {:?}",
            err
        )
    });
    let firmware_dtb::DtbFirmwareInfo {
        cpu_count,
        mut memory_segments,
        reserved_segments,
        external_initramfs_range,
        stdout_serial,
        power_controls,
        serial_ports,
        platform_devices,
        pcie_hosts,
    } = firmware;

    printk!(
        "[kernel-start][dtb] firmware parsed: cpu={} memory={} reserved={} serial={} platform={} pcie-host={}",
        cpu_count,
        memory_segments.len(),
        reserved_segments.len(),
        serial_ports.len(),
        platform_devices.len(),
        pcie_hosts.len()
    );

    // 如果引导器额外提供了可用内存图，这里再做一次交叉过滤。
    if let Some(boot_segments) = context.memory.boot_map.usable_segments() {
        memory_segments = start::intersect_memory_segments(&memory_segments, &boot_segments)
            .unwrap_or_else(|| {
                panic!(
                    "[kernel-start][dtb] DTB memory description does not overlap usable boot memory"
                )
            });
    }

    // 步骤 2 把刚刚解析好的电源控制信息安装到固件抽象层。这样内核后续无论是
    // 正常关机、重启还是错误路径上的兜底退出，都能通过统一接口回到本平台提供的
    // syscon 寄存器写入方案，而不需要再次接触 DTB 原始节点。

    general::firmware::power::install(power_controls, context.address.phys_to_virt);

    // 步骤 3 初始化分层分配器。这个阶段会消费上面整理好的内存段、内核镜像占用区
    // 以及可选的外部 initramfs 范围。这里先建立物理地址与虚拟地址转换关系，再
    // 逐层启用物理页、内核虚拟内存、堆和 slab，使后续驱动注册与 VFS 挂载都能在
    // 同一套分配框架中进行。

    // 小步骤 3.1 先绑定平台提供的物理地址与虚拟地址转换函数。
    KERNEL_ALLOCATOR
        .bind_address_translation(context.address.phys_to_virt, context.address.virt_to_phys);

    // 小步骤 3.2 然后整理启动早期必须避开的保留区，包括内核镜像本身以及可选的
    // 外部 initramfs 地址范围。
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

    // 小步骤 3.3 先初始化物理页分配器，使之后的页级资源请求可以建立在 DTB 解析出的
    // 可用 RAM 之上。
    KERNEL_ALLOCATOR
        .init_phys(&memory_segments, &kernel_reserved)
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][dtb] failed to init physical allocator: {:?}",
                err
            )
        });

    // 小步骤 3.4 如果当前平台提供了完整的虚拟内存初始化回调，这里继续把内核页表、
    // 虚拟内存、堆和 slab 一次性拉起来，并在最后切换全局分配器。
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

    let cmdline = context
        .boot
        .command_line
        .map(general::cmdline::Cmdline::new);
    let external_initramfs = external_initramfs_range.map(|(start, end)| {
        let virt = (context.address.phys_to_virt)(start);
        let len = end - start;
        let bytes = unsafe { core::slice::from_raw_parts(virt as *const u8, len) };
        crate::initramfs::InitramfsImage {
            bytes,
            source: crate::initramfs::InitramfsSource::External,
        }
    });

    // 步骤 4 进入文件系统准备阶段。启动代码会先准备好 tmpfs、devtmpfs、procfs
    // 和 sysfs 所需的驱动与 superblock，然后根据是否存在 initramfs 或块设备根盘
    // 来决定最终 `/` 的来源。devtmpfs 会被提前建立，以便总线扫描和设备注册期间
    // 就能为后续 `/dev` 挂载准备好节点。

    // 小步骤 5.1 先注册启动阶段一定会用到的文件系统驱动。
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::TmpfsDriver)))
        .expect("[kernel-start][dtb] failed to register tmpfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::DevTmpfsDriver)))
        .expect("[kernel-start][dtb] failed to register devtmpfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::ProcFsDriver)))
        .expect("[kernel-start][dtb] failed to register procfs driver");
    FS_REGISTRY
        .register(Box::leak(Box::new(general::vfs::SysFsDriver)))
        .expect("[kernel-start][dtb] failed to register sysfs driver");
    general::vfs::register_block_filesystems();

    let dev_sb = FS_REGISTRY
        .find("devtmpfs")
        .expect("[kernel-start][dtb] devtmpfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][dtb] failed to mount devtmpfs");
    let dev_ops = dev_sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .expect("[kernel-start][dtb] failed to downcast devtmpfs ops");

    // 小步骤 5.2 接着把 devtmpfs 与 PnP 层连接起来。这样 PCI 扫描过程中一旦发现
    // 可驱动设备，对应的字符设备或块设备节点就能直接写入 devtmpfs；等最终根文件
    // 系统选定之后，再把这份已经填充好的 devtmpfs 挂到 `/dev` 即可。
    general::vfs::devtmpfs::install_pnp_bridge(Arc::clone(&dev_sb))
        .expect("[kernel-start][dtb] failed to install PnP devtmpfs bridge");
    printk!("[kernel-start][dtb] PnP devtmpfs callbacks installed");

    set_dev_init_context(
        DevInitContext::new(
            context.address.device_mmio_to_virt,
            context.address.virt_to_phys,
        )
        .with_realtime_clock(crate::vdso::set_realtime_ns),
    );

    // 安装架构无关的熵源：random 子系统需要时间戳 / 栈指针等
    // 启动期熵，这些只能由 arch 层提供。
    hal::random::register_arch_hooks();
    printk!("[kernel-start][dtb] registered arch entropy source for random subsystem");

    drivers::register_builtin_drivers()
        .expect("[kernel-start][dtb] failed to register built-in PnP drivers");
    printk!("[kernel-start][dtb] registered built-in PnP drivers");

    // 把 random / urandom 字符设备绑定到 devtmpfs。
    // 这两个节点不通过 PnP 枚举发现（既无 DTB compatible 节点，
    // 也不是 PCI 设备），而是在启动时静态挂载：
    //   - /dev/random   阻塞读、熵不足时 spin/yield
    //   - /dev/urandom  永不阻塞、CSPRNG 持续可读
    let _ = dev_ops.bind_char("random", CharDevice::new("random", &RANDOM_DRIVER));
    let _ = dev_ops.bind_char("urandom", CharDevice::new("urandom", &URANDOM_DRIVER));
    printk!("[kernel-start][dtb] bound /dev/random and /dev/urandom");

    let stdout_phys = stdout_serial.as_ref().map(|port| port.phys_addr);
    let mut platform_bound = 0usize;
    for device in &platform_devices {
        let info = platform_device_info_from_dtb(device, stdout_phys);
        if register_platform_device(info, "dtb") {
            platform_bound += 1;
        }
    }
    printk!(
        "[kernel-start][dtb] platform PnP discovery complete: {} candidate(s), {} bound",
        platform_devices.len(),
        platform_bound
    );

    // 小步骤 5.3 然后尝试使用标准化后的 PCIe host bridge 描述完成 ECAM 安装、
    // virtio-pci 驱动注册、BAR 分配以及总线扫描。
    if let Some(host) = pcie_hosts.first() {
        if pcie_hosts.len() > 1 {
            printk!(
                "[kernel-start][dtb] multiple pcie hosts found ({}); using first ECAM host {}",
                pcie_hosts.len(),
                host.path
            );
        }
        printk!(
            "[kernel-start][dtb] pcie ECAM {} phys={:#x} size={:#x} bus=[{:#x},{:#x}]",
            host.path,
            host.ecam_phys,
            host.ecam_size,
            host.bus_start,
            host.bus_end
        );
        pci::install_ecam(
            host.ecam_phys as u64,
            host.ecam_size as u64,
            host.bus_start,
            host.bus_end,
            context.address.device_mmio_to_virt,
        );

        pci::assign_bars(host.bus_start, host.bus_end);

        let count = pci_scan_and_register(0, host.bus_start, host.bus_end, "pci-");
        printk!(
            "[kernel-start][dtb] pci_scan_and_register probed {} device(s)",
            count
        );
    } else {
        printk!("[kernel-start][dtb] no pcie node in DTB; skipping PCI init");
    }

    // 小步骤 5.4 再决定根文件系统的来源。优先级是外部/内建 initramfs，其次才是
    // 已经注册好的块设备根盘。
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
        mount_first_block_root()
            .unwrap_or_else(|err| panic!("[kernel-start][dtb] failed to mount block root: {}", err))
    };
    printk!("[kernel-start][dtb] root source selected: {}", root_source);

    // 小步骤 5.5 在确定根 superblock 之后，创建 mount namespace 和启动期 VFS
    // 上下文，并在需要时把 initramfs 内容解包到最终根目录。
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

    // 小步骤 5.6 最后把 devtmpfs 和 sysfs 挂到标准路径，同时把之前登记好的启动
    // 设备节点批量同步进 `/dev`。
    ensure_dir(&vfs_ctx, "/dev", FileMode::new(0o755))
        .expect("[kernel-start][dtb] failed to ensure /dev directory");
    let dev_dentry = path::lookup(&vfs_ctx, &Dirfd::Cwd, "/dev", LookupFlags::DIRECTORY)
        .expect("[kernel-start][dtb] failed to resolve /dev")
        .dentry;
    mount_ns
        .mount(dev_dentry, Arc::clone(&dev_sb), MountFlags::default())
        .expect("[kernel-start][dtb] failed to mount devtmpfs on /dev");

    ensure_dir(&vfs_ctx, "/sys", FileMode::new(0o555))
        .expect("[kernel-start][dtb] failed to ensure /sys directory");
    let sys_dentry = path::lookup(&vfs_ctx, &Dirfd::Cwd, "/sys", LookupFlags::DIRECTORY)
        .expect("[kernel-start][dtb] failed to resolve /sys")
        .dentry;
    let sys_sb = FS_REGISTRY
        .find("sysfs")
        .expect("[kernel-start][dtb] sysfs driver not found")
        .mount(None, "")
        .expect("[kernel-start][dtb] failed to mount sysfs");
    mount_ns
        .mount(
            Arc::clone(&sys_dentry),
            Arc::clone(&sys_sb),
            MountFlags::default(),
        )
        .expect("[kernel-start][dtb] failed to mount sysfs on /sys");

    printk!(
        "[kernel-start][dtb] VFS ready: '{}' mounted as '/' + devtmpfs '/dev' + sysfs '/sys'",
        root_source
    );

    // 步骤 6 最后确定启动期控制台，并把日志出口绑定到已经就绪的字符设备上。
    // 这里优先尊重命令行显式指定的 console，其次回退到 DTB 的 stdout-path 结果。
    // 一旦控制台建立成功，后续 printk 与日志系统都会共享同一条稳定的输出路径。

    // 小步骤 6.1 先把根目录、挂载点和凭据等 VFS 组件交给 sched shim 保管；随后
    // sched::boot_init 会据此给 init 任务挂上 TASKEXT_VFS_CONTEXT / TASKEXT_VFS_FDTABLE。
    crate::sched::stash_boot_vfs_parts(
        Arc::clone(&root_sb.root_dentry),
        Arc::clone(&root_mount),
        Arc::clone(&mount_ns),
        Arc::new(cred.clone()),
    );

    // 小步骤 6.2 然后解析控制台来源。这里优先看命令行 console 参数，找不到时再
    // 用 stdout-path 反查已经注册好的串口驱动实例。
    let console_registered = {
        let cmdline_dev = cmdline
            .as_ref()
            .and_then(|cl| {
                cl.find("console")
                    .map(|v| v.split_once(',').map_or(v, |(d, _)| d))
            })
            .and_then(|name| resolve_cmdline_console(&vfs_ctx, dev_ops, name));

        let dev = if let Some(dev) = cmdline_dev {
            printk!(
                "[kernel-start][dtb] console from cmdline: {}",
                dev.fw_name()
            );
            Some(dev)
        } else if let Some(port) = stdout_serial.as_ref() {
            let found = lookup_char_fw_name(port.name);
            if let Some(dev) = found.as_ref() {
                printk!(
                    "[kernel-start][dtb] console from stdout-path: {}",
                    dev.fw_name()
                );
            }
            found
        } else {
            None
        };

        if let Some(dev) = dev {
            general::console::register_console(dev.clone());
            // 小步骤 6.2.1 在 devtmpfs 里把当前 console 重定向为固定路径
            // `/dev/console`，让用户态进程通过稳定路径打开它。
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

    // 小步骤 6.3 如果控制台已经可用，就把日志系统的 sink 也切换到这条设备路径上。
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
    lookup_char_fw_name(name)
}

fn lookup_char_fw_name(name: &str) -> Option<CharDevice> {
    find_char_device_by_fw_name(&DEVICES.functions, name)
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
    general::vfs::mount_block_device_auto(dev, "")
        .map_err(|_| "unsupported or invalid root filesystem")
}

fn mount_first_block_root() -> Result<(Arc<Superblock>, &'static str), &'static str> {
    let devices = active_block_devices(&DEVICES.functions);
    if devices.is_empty() {
        return Err("no initramfs and no active block device found");
    }

    for dev in devices {
        match mount_block_root(Arc::clone(&dev)) {
            Ok(root) => return Ok(root),
            Err(err) => {
                log::debug!(
                    "[kernel-start][dtb] block device {} is not root candidate: {}",
                    dev.name(),
                    err
                );
            }
        }
    }

    Err("no active block device contains a supported root filesystem")
}

fn platform_device_info_from_dtb(
    device: &firmware_dtb::DtbPlatformDeviceInfo,
    stdout_phys: Option<usize>,
) -> PlatformDeviceInfo {
    let ids = device
        .compatible
        .iter()
        .map(|compatible| DeviceMatchId::DtbCompatible((*compatible).into()))
        .collect();
    let resources = device
        .reg_ranges
        .iter()
        .map(|range| DeviceResource::Mmio {
            phys: range.phys_addr,
            size: range.size,
        })
        .collect();
    let first_phys = device.reg_ranges.first().map(|range| range.phys_addr);

    PlatformDeviceInfo {
        fw_name: device.name.into(),
        ids,
        resources,
        properties: DeviceProperties {
            clock_hz: device.clock_hz,
            baud: None,
            stdout: first_phys == stdout_phys,
        },
    }
}

fn register_platform_device(info: PlatformDeviceInfo, tag: &str) -> bool {
    match register_and_probe_platform_device(info) {
        Ok(reg) if reg.bound => true,
        Ok(reg) => {
            log::debug!(
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

pub(crate) fn ensure_dir(
    ctx: &VfsContext,
    path: &str,
    mode: FileMode,
) -> general::vfs::error::VfsResult<()> {
    match path::lookup(ctx, &Dirfd::Cwd, path, LookupFlags::DIRECTORY) {
        Ok(_) => Ok(()),
        Err(VfsError::NotFound) => general::vfs::operation::mkdirat(ctx, &Dirfd::Cwd, path, mode),
        Err(err) => Err(err),
    }
}
