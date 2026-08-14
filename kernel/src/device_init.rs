//! 设备子系统启动期公共流程。
//!
//! DTB 与 ACPI 的固件解析策略不同，但设备抽象层的启动顺序必须一致：
//! 先准备 VFS 投影载体，再安装 PnP bridge，最后注册内建设备驱动。把这段逻辑
//! 集中在这里，可以避免两个启动路径各自复制顺序并留下不一致的边界条件。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;

use general::dev::char::CharDevice;
use general::dev::enumerate::DEVICES;
use general::dev::pnp::{DevInitContext, set_dev_init_context};
use general::vfs::device_files::projection::find_char_device_by_fw_name;
use general::vfs::devtmpfs::DevTmpfsSuperblockOps;
use general::vfs::error::VfsError;
use general::vfs::fdtable::FdTable;
use general::vfs::mount::{Mount, MountFlags};
use general::vfs::path::{self, Dirfd, LookupFlags};
use general::vfs::stat::FileMode;
use general::vfs::superblock::Superblock;
use general::vfs::{FS_REGISTRY, VfsContext, ensure_dir, mount_standard_shm_tmpfs};
use log::{LogRecord, LogSink, printk};
use core::sync::atomic::{AtomicBool, Ordering};

use sched::sync::Spinlock;
use sched::{TASKEXT_VFS_CONTEXT, TASKEXT_VFS_FDTABLE, Task};

const SYSFS_DIR_PATH: &str = "/sys";
const SYSFS_DIR_MODE: FileMode = FileMode::new(0o555);
const SYSFS_FS_TYPE: &str = "sysfs";

/// 启动控制台的稳定选择方式。
pub enum BootConsoleSelector {
    /// 用户通过命令行指定的 devtmpfs 名称或绝对路径。
    DeviceName(String),
    /// 固件 stdout/SPCR 指向的设备固件名称。
    FirmwareName(String),
}

impl BootConsoleSelector {
    fn description(&self) -> &str {
        match self {
            Self::DeviceName(name) | Self::FirmwareName(name) => name,
        }
    }
}

struct DeferredBootConsole {
    tag: &'static str,
    dev_sb: Arc<Superblock>,
    selector: BootConsoleSelector,
}

static DEFERRED_BOOT_CONSOLE: Spinlock<Option<DeferredBootConsole>> = Spinlock::new(None);

static PTMX_DEVNODE_REGISTERED: AtomicBool = AtomicBool::new(false);

static EXTRA_STATIC_NODES_REGISTERED: AtomicBool = AtomicBool::new(false);

static CONSOLE_LOG_SINK: LogSink = LogSink {
    write_record: write_log_record_to_console,
};

/// 解析并绑定启动控制台；驱动尚未装载时保存请求，供 BuildBound ELM 装载后重试。
pub fn bind_or_defer_boot_console(
    tag: &'static str,
    vfs_ctx: &VfsContext,
    dev_sb: Arc<Superblock>,
    selector: BootConsoleSelector,
) -> bool {
    let dev_ops = devtmpfs_ops(&dev_sb, tag);
    let Some(device) = resolve_boot_console(vfs_ctx, dev_ops, &selector) else {
        printk!(
            "[kernel-start][{}] console {} deferred until managed drivers are loaded",
            tag,
            selector.description()
        );
        *DEFERRED_BOOT_CONSOLE.lock() = Some(DeferredBootConsole {
            tag,
            dev_sb,
            selector,
        });
        return false;
    };
    if selector_is_vt(&selector) {
        activate_vt_console(tag, dev_ops, device, true)
    } else {
        let bound = activate_console(tag, dev_ops, device.clone(), true);
        maybe_install_virtual_terminals(tag, dev_ops, &selector, device);
        bound
    }
}

/// 在 BuildBound 设备 ELM 装载完成后重试尚未解析的启动控制台。
pub fn retry_deferred_boot_console(init: &Arc<Task>) -> bool {
    let Some(request) = DEFERRED_BOOT_CONSOLE.lock().take() else {
        return false;
    };
    let Some(vfs_ctx) = task_vfs_context(init) else {
        log::error!(
            "[kernel-start][{}] deferred console lacks init VFS context",
            request.tag
        );
        *DEFERRED_BOOT_CONSOLE.lock() = Some(request);
        return false;
    };
    let Some(fdtable) = task_fdtable(init) else {
        log::error!(
            "[kernel-start][{}] deferred console lacks init fdtable",
            request.tag
        );
        *DEFERRED_BOOT_CONSOLE.lock() = Some(request);
        return false;
    };
    let dev_ops = devtmpfs_ops(&request.dev_sb, request.tag);
    let Some(device) = resolve_boot_console(&vfs_ctx, dev_ops, &request.selector) else {
        log::error!(
            "[kernel-start][{}] deferred console {} is still unavailable",
            request.tag,
            request.selector.description()
        );
        *DEFERRED_BOOT_CONSOLE.lock() = Some(request);
        return false;
    };
    if selector_is_vt(&request.selector) {
        if !activate_vt_console(request.tag, dev_ops, device, false) {
            *DEFERRED_BOOT_CONSOLE.lock() = Some(request);
            return false;
        }
    } else {
        if !activate_console(request.tag, dev_ops, device.clone(), false) {
            *DEFERRED_BOOT_CONSOLE.lock() = Some(request);
            return false;
        }
        maybe_install_virtual_terminals(request.tag, dev_ops, &request.selector, device);
    }
    crate::stdio::install_stdio(&vfs_ctx, &fdtable, "/dev/console");
    true
}

fn resolve_boot_console(
    vfs_ctx: &VfsContext,
    dev_ops: &DevTmpfsSuperblockOps,
    selector: &BootConsoleSelector,
) -> Option<CharDevice> {
    // VT 选择器(console=ttyN)没有自己的底层设备:物理控制台回退到
    // 固件 stdout 指向的 console 字符设备(uart),VT 管理器以它为渲染
    // 与输入目标。ttyN 节点由 install_virtual_terminal_nodes 在之后创建。
    if selector_is_vt(selector) {
        return general::vfs::device_files::projection::active_char_devices(&DEVICES.functions)
            .into_iter()
            .find(|dev| dev.is_console());
    }
    match selector {
        BootConsoleSelector::FirmwareName(name) => {
            find_char_device_by_fw_name(&DEVICES.functions, name)
        }
        BootConsoleSelector::DeviceName(name) => {
            if let Some(dev_name) = name.strip_prefix("/dev/")
                && let Some(device) = dev_ops.char_dev(dev_name)
            {
                return Some(device);
            }
            if !name.starts_with('/')
                && let Some(device) = dev_ops.char_dev(name)
            {
                return Some(device);
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
            find_char_device_by_fw_name(&DEVICES.functions, name)
        }
    }
}

fn activate_console(
    tag: &str,
    dev_ops: &DevTmpfsSuperblockOps,
    device: CharDevice,
    stash_for_boot_init: bool,
) -> bool {
    match dev_ops.bind_char("console", device.clone()) {
        Ok(()) => printk!(
            "[kernel-start][{}] bound /dev/console -> {}",
            tag,
            device.fw_name()
        ),
        Err(VfsError::AlreadyExists) => {
            printk!(
                "[kernel-start][{}] /dev/console already exists; using it for stdio",
                tag
            );
        }
        Err(error) => {
            printk!(
                "[kernel-start][{}] failed to bind /dev/console: {:?}",
                tag,
                error
            );
            return false;
        }
    }
    general::console::register_console(device);
    log::bind_log_sink(&CONSOLE_LOG_SINK);
    // 回放 console 就绪前进入 ring buffer 的早期日志（一条不丢）。
    replay_buffered_logs_to_console();
    if stash_for_boot_init {
        crate::sched::stash_boot_console_name(String::from("/dev/console"));
    }
    true
}

/// console= 选择器是否为虚拟终端(`tty0`..`ttyN`)。
///
/// 注意 `tty`(5:0,会话控制终端别名)与 `ttyS0` 等串口名不是 VT。
fn selector_is_vt(selector: &BootConsoleSelector) -> bool {
    let name = selector.description();
    let name = name.strip_prefix("/dev/").unwrap_or(name);
    name.strip_prefix("tty")
        .and_then(|digits| digits.parse::<u8>().ok())
        .is_some_and(|index| index < general::dev::tty::vt::VT_COUNT as u8)
}

/// 以虚拟终端为启动控制台(`console=ttyN`)。
///
/// 安装 VT 管理器并把串口输入路由到活动 VT;`/dev/console` 绑定为 tty0
/// 别名(活动 VT)。内核日志仍直接写物理串口,不经过 VT 行规程。
fn activate_vt_console(
    tag: &str,
    dev_ops: &DevTmpfsSuperblockOps,
    device: CharDevice,
    stash_for_boot_init: bool,
) -> bool {
    let manager = general::dev::tty::VtManager::install(device.clone(), true);
    if let Err(error) =
        general::vfs::devtmpfs::install_virtual_terminal_nodes(dev_ops, manager, true)
    {
        printk!(
            "[kernel-start][{}] failed to install virtual terminal nodes: {:?}",
            tag,
            error
        );
        return false;
    }
    general::console::register_console(device);
    log::bind_log_sink(&CONSOLE_LOG_SINK);
    replay_buffered_logs_to_console();
    if stash_for_boot_init {
        crate::sched::stash_boot_console_name(String::from("/dev/console"));
    }
    printk!("[kernel-start][{}] bound /dev/console -> tty0 (VT console)", tag);
    true
}

/// 非 VT 控制台下仍安装虚拟终端(与 Linux 一致,/dev/ttyN 始终存在),
/// 但串口输入不路由到 VT,console 保持指向物理串口。
fn maybe_install_virtual_terminals(
    tag: &str,
    dev_ops: &DevTmpfsSuperblockOps,
    selector: &BootConsoleSelector,
    device: CharDevice,
) {
    if selector_is_vt(selector) {
        return;
    }
    let manager = general::dev::tty::VtManager::install(device, false);
    match general::vfs::devtmpfs::install_virtual_terminal_nodes(dev_ops, manager, false) {
        Ok(()) => printk!(
            "[kernel-start][{}] installed virtual terminals tty0..tty{}",
            tag,
            general::dev::tty::vt::VT_COUNT - 1
        ),
        Err(error) => printk!(
            "[kernel-start][{}] virtual terminal node installation failed: {:?}",
            tag,
            error
        ),
    }
}

fn task_vfs_context(task: &Arc<Task>) -> Option<Arc<VfsContext>> {
    Arc::downcast::<VfsContext>(task.ext_lookup(TASKEXT_VFS_CONTEXT)?).ok()
}

fn task_fdtable(task: &Arc<Task>) -> Option<Arc<FdTable>> {
    Arc::downcast::<FdTable>(task.ext_lookup(TASKEXT_VFS_FDTABLE)?).ok()
}

fn write_log_record_to_console(record: &LogRecord<'_>) {
    let line = crate::start::format_log_record_line(record);
    general::console::console_write(line.as_bytes());
}

/// 回放 console 就绪前进入日志环形缓冲区的早期启动日志。
///
/// 启动早期没有 sink，所有日志仍写入 ring buffer；此处把未输出条目按与
/// 实时日志一致的格式写向已注册的 console，保证启动过程一条日志都不丢。
/// 消费语义保证回放后新日志不会被重复输出。
fn replay_buffered_logs_to_console() {
    log::replay_ready_logs(|level, timestamp, message| {
        let record = log::LogRecord {
            timestamp,
            level,
            seq: 0,
            message,
        };
        write_log_record_to_console(&record);
    });
}

/// 注册启动期核心文件系统驱动。
///
/// 这里按名字做幂等检查，避免未来固件路径或测试代码重复进入设备初始化时把同名
/// driver 重复塞进全局 registry。块文件系统的注册函数内部负责自己的适配器集合。
pub fn register_core_filesystems(tag: &str) {
    if let Err(err) = general::vfs::device_files::base::register_standard_node_policies() {
        panic!(
            "[kernel-start][{}] failed to register standard node policies: {:?}",
            tag, err
        );
    }
    if FS_REGISTRY.find("tmpfs").is_none() {
        FS_REGISTRY
            .register(Box::leak(Box::new(general::vfs::TmpfsDriver)))
            .unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][{}] failed to register tmpfs driver: {:?}",
                    tag, err
                )
            });
    }
    if FS_REGISTRY.find("devtmpfs").is_none() {
        FS_REGISTRY
            .register(Box::leak(Box::new(general::vfs::DevTmpfsDriver)))
            .unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][{}] failed to register devtmpfs driver: {:?}",
                    tag, err
                )
            });
    }
    if FS_REGISTRY.find("devpts").is_none() {
        FS_REGISTRY
            .register(Box::leak(Box::new(general::vfs::DevPtsDriver)))
            .unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][{}] failed to register devpts driver: {:?}",
                    tag, err
                )
            });
    }
    if FS_REGISTRY.find("procfs").is_none() {
        FS_REGISTRY
            .register(Box::leak(Box::new(general::vfs::ProcFsDriver)))
            .unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][{}] failed to register procfs driver: {:?}",
                    tag, err
                )
            });
    }
    if FS_REGISTRY.find("sysfs").is_none() {
        FS_REGISTRY
            .register(Box::leak(Box::new(general::vfs::SysFsDriver)))
            .unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][{}] failed to register sysfs driver: {:?}",
                    tag, err
                )
            });
    }
    general::vfs::user_api::standard_devices::register_standard_device_policies().unwrap_or_else(
        |err| {
            panic!(
                "[kernel-start][{}] failed to register standard device number policies: {:?}",
                tag, err
            )
        },
    );
    general::vfs::register_block_filesystems();
    general::vfs::device_files::projection::register_builtin_device_file_projectors()
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][{}] failed to register device file projectors: {:?}",
                tag, err
            )
        });
    general::vfs::device_files::rtc::register_devtmpfs_adapter().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register RTC devtmpfs adapter: {:?}",
            tag, err
        )
    });
    general::vfs::device_files::loop_device::register_devtmpfs_adapter().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register loop devtmpfs adapter: {:?}",
            tag, err
        )
    });
    general::vfs::device_files::cpu_dma_latency::register_devtmpfs_adapter().unwrap_or_else(
        |err| {
            panic!(
                "[kernel-start][{}] failed to register cpu_dma_latency devtmpfs adapter: {:?}",
                tag, err
            )
        },
    );
    general::vfs::device_files::loop_device::register_control_node().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register loop-control devtmpfs node: {:?}",
            tag, err
        )
    });
    general::vfs::device_files::cpu_dma_latency::register_static_node().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register cpu_dma_latency devtmpfs node: {:?}",
            tag, err
        )
    });
    general::vfs::device_files::base::register_static_nodes().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register base devtmpfs nodes: {:?}",
            tag, err
        )
    });
}

/// 创建并发布 devtmpfs 单例 superblock。
pub fn mount_devtmpfs(tag: &str) -> Arc<Superblock> {
    register_pty_devnode_if_needed(tag);
    register_extra_static_nodes_if_needed(tag);
    FS_REGISTRY
        .find("devtmpfs")
        .unwrap_or_else(|| panic!("[kernel-start][{}] devtmpfs driver not found", tag))
        .mount(None, "")
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][{}] failed to mount devtmpfs: {:?}",
                tag, err
            )
        })
}

/// 取回 devtmpfs 的 typed superblock ops。
pub fn devtmpfs_ops<'a>(dev_sb: &'a Arc<Superblock>, tag: &str) -> &'a DevTmpfsSuperblockOps {
    dev_sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .unwrap_or_else(|| panic!("[kernel-start][{}] failed to downcast devtmpfs ops", tag))
}

/// 把全局 devtmpfs superblock 挂载到标准 `/dev` 路径。
///
/// devtmpfs 是设备模型的 VFS 投影载体，具体节点已经由 PnP bridge 和静态节点注册表
/// 写入 superblock；这里仅负责把这份全局命名空间接到当前 mount namespace。
pub fn mount_devtmpfs_on_dev(tag: &str, ctx: &VfsContext, dev_sb: Arc<Superblock>) -> Arc<Mount> {
    ensure_dir(ctx, general::vfs::DEV_DIR_PATH, FileMode::new(0o755)).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to ensure /dev directory: {:?}",
            tag, err
        )
    });

    if let Ok(existing) = path::lookup(
        ctx,
        &Dirfd::Cwd,
        general::vfs::DEV_DIR_PATH,
        LookupFlags::DIRECTORY,
    ) && Arc::ptr_eq(&existing.mount.superblock, &dev_sb)
        && Arc::ptr_eq(&existing.dentry, &existing.mount.mount_root)
    {
        return existing.mount;
    }

    let mountpoint = path::lookup(
        ctx,
        &Dirfd::Cwd,
        general::vfs::DEV_DIR_PATH,
        LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
    )
    .unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to resolve /dev mountpoint: {:?}",
            tag, err
        )
    });
    ctx.mount_ns
        .mount(mountpoint.dentry, dev_sb, MountFlags::default())
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][{}] failed to mount devtmpfs on /dev: {:?}",
                tag, err
            )
        })
}

/// 挂载依赖 `/dev` 的标准兼容层伪文件系统。
///
/// `/dev/shm` 和 `/sys` 不是底层设备身份的一部分，但它们依赖启动期设备文件系统
/// 先就绪。集中到这里可以让 DTB/ACPI 等固件入口共享同一套顺序和幂等规则。
pub fn mount_standard_user_api_filesystems(tag: &str, ctx: &VfsContext) {
    mount_standard_shm_tmpfs(ctx).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to mount tmpfs on /dev/shm: {:?}",
            tag, err
        )
    });
    mount_devpts_on_pts(tag, ctx);
    mount_sysfs_on_sys(tag, ctx);
}

/// 挂载 devpts 到 `/dev/pts`(Linux 布局)。
fn mount_devpts_on_pts(tag: &str, ctx: &VfsContext) -> Arc<Mount> {
    ensure_dir(ctx, "/dev/pts", vfs::stat::FileMode::new(0o755)).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to ensure /dev/pts directory: {:?}",
            tag, err
        )
    });
    if let Ok(existing) = path::lookup(ctx, &Dirfd::Cwd, "/dev/pts", LookupFlags::DIRECTORY)
        && existing.mount.superblock.fs_type == "devpts"
        && Arc::ptr_eq(&existing.dentry, &existing.mount.mount_root)
    {
        return existing.mount;
    }
    let mountpoint = path::lookup(
        ctx,
        &Dirfd::Cwd,
        "/dev/pts",
        LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
    )
    .unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to resolve /dev/pts mountpoint: {:?}",
            tag, err
        )
    });
    let sb = FS_REGISTRY
        .find("devpts")
        .expect("[kernel-start] devpts driver not found")
        .mount(None, "mode=0620,ptmxmode=0666")
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][{}] failed to mount devpts: {:?}",
                tag, err
            )
        });
    let mount = ctx.mount_ns.mount_at(
        Arc::clone(&mountpoint.dentry),
        Arc::clone(&mountpoint.mount),
        sb,
        MountFlags::default(),
    );
    match mount {
        Ok(mount) => {
            printk!("[kernel-start][{}] devpts mounted on /dev/pts", tag);
            mount
        }
        Err(err) => panic!(
            "[kernel-start][{}] failed to mount devpts at /dev/pts: {:?}",
            tag, err
        ),
    }
}

fn register_extra_static_nodes_if_needed(tag: &str) {
    if EXTRA_STATIC_NODES_REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    match general::vfs::device_files::base::register_extra_static_nodes() {
        Ok(_) => printk!("[kernel-start][{}] registered /dev/full /dev/kmsg", tag),
        Err(err) => printk!(
            "[kernel-start][{}] failed to register full/kmsg nodes: {:?}",
            tag,
            err
        ),
    }
}

fn register_pty_devnode_if_needed(tag: &str) {
    if PTMX_DEVNODE_REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    match general::vfs::devtmpfs::register_pty_devnode() {
        Ok(_) => printk!("[kernel-start][{}] registered /dev/ptmx devnode", tag),
        Err(err) => printk!(
            "[kernel-start][{}] failed to register /dev/ptmx devnode: {:?}",
            tag,
            err
        ),
    }
}

fn mount_sysfs_on_sys(tag: &str, ctx: &VfsContext) -> Arc<Mount> {
    ensure_dir(ctx, SYSFS_DIR_PATH, SYSFS_DIR_MODE).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to ensure /sys directory: {:?}",
            tag, err
        )
    });
    if let Ok(existing) = path::lookup(ctx, &Dirfd::Cwd, SYSFS_DIR_PATH, LookupFlags::DIRECTORY)
        && existing.mount.superblock.fs_type == SYSFS_FS_TYPE
        && Arc::ptr_eq(&existing.dentry, &existing.mount.mount_root)
    {
        return existing.mount;
    }

    let mountpoint = path::lookup(
        ctx,
        &Dirfd::Cwd,
        SYSFS_DIR_PATH,
        LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
    )
    .unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to resolve /sys mountpoint: {:?}",
            tag, err
        )
    });
    let sys_sb = FS_REGISTRY
        .find(SYSFS_FS_TYPE)
        .unwrap_or_else(|| panic!("[kernel-start][{}] sysfs driver not found", tag))
        .mount(None, "")
        .unwrap_or_else(|err| panic!("[kernel-start][{}] failed to mount sysfs: {:?}", tag, err));
    ctx.mount_ns
        .mount(mountpoint.dentry, sys_sb, MountFlags::default())
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][{}] failed to mount sysfs on /sys: {:?}",
                tag, err
            )
        })
}

/// 安装 function 投影订阅、设备初始化上下文和内建驱动。
///
/// `ctx` 只携带底层设备驱动需要的内核能力；`/dev` 命名、设备号等用户接口层
/// 信息不进入这个上下文，仍由 devtmpfs/VFS 投影层处理。
pub fn activate_device_subsystem(
    tag: &str,
    dev_sb: Arc<Superblock>,
    ctx: DevInitContext,
    bootloader_seed: Option<&[u8]>,
) {
    general::vfs::devtmpfs::install_function_projection(Arc::clone(&dev_sb)).unwrap_or_else(
        |err| {
            panic!(
                "[kernel-start][{}] failed to install devtmpfs function projection: {:?}",
                tag, err
            )
        },
    );
    printk!(
        "[kernel-start][{}] devtmpfs function projection installed",
        tag
    );

    set_dev_init_context(ctx);

    // random 驱动的熵源 hook 来自 arch/hal；它是内建设备驱动初始化前的能力注入，
    // 不属于任何具体固件路径，DTB 和 ACPI 必须共享同一顺序。
    hal::random::register_arch_hooks();
    printk!(
        "[kernel-start][{}] registered arch entropy source for random subsystem",
        tag
    );

    if let Some(seed) = bootloader_seed {
        general::dev::random::add_bootloader_randomness(seed);
    }

    let integrated = crate::integrated_components::initialize_phase(
        crate::integrated_components::IntegratedPhase::Device,
    )
    .unwrap_or_else(|error| panic!("[kernel-start][{tag}] 设备阶段集成组件初始化失败: {error}"));
    if integrated != 0 {
        printk!(
            "[kernel-start][{}] initialized {} device-stage integrated component(s)",
            tag,
            integrated
        );
    }

    printk!(
        "[kernel-start][{}] registered configured ELM device drivers",
        tag
    );
}

/// 在辅助 CPU 启动完成后安装网络 host、driver 与 stack 的共享启动配置。
pub fn install_network_boot_config() {
    let mut material = [0u8; 112];
    general::dev::random::fill(
        &mut material,
        general::dev::random::RandomReadMode::Insecure,
    )
    .expect("random ELM 未提供网络启动密钥材料");
    let online_cpu_count = sched::online_cpu_mask().count_ones();
    let active_cpu_count =
        net::boot::select_protocol_shard_count(online_cpu_count).expect("网络启动时没有在线 CPU");
    let (host_config, driver_config, stack_config) =
        net::boot::NetBootConfigs::from_random_material(material, active_cpu_count)
            .expect("active CPU count 超出网络栈范围")
            .split();
    net::boot::install_host_boot_config(host_config).expect("网络 host 启动配置被重复安装");
    net::device::install_net_runtime(driver_config, crate::net_runtime::registrar())
        .expect("网络运行时被重复安装");
    net::stack::install_stack_runtime(stack_config, crate::net_stack::registrar())
        .expect("网络 stack broker 被重复安装");
    log::info!(
        "[kernel] installed network boot config: online_cpus={} protocol_shards={}",
        online_cpu_count,
        active_cpu_count
    );
}
