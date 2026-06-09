//! 设备子系统启动期公共流程。
//!
//! DTB 与 ACPI 的固件解析策略不同，但设备抽象层的启动顺序必须一致：
//! 先准备 VFS 投影载体，再安装 PnP bridge，最后注册内建设备驱动。把这段逻辑
//! 集中在这里，可以避免两个启动路径各自复制顺序并留下不一致的边界条件。

use alloc::boxed::Box;
use alloc::sync::Arc;

use general::dev::drivers;
use general::dev::pnp::{DevInitContext, set_dev_init_context};
use general::vfs::devtmpfs::DevTmpfsSuperblockOps;
use general::vfs::mount::{Mount, MountFlags};
use general::vfs::path::{self, Dirfd, LookupFlags};
use general::vfs::stat::FileMode;
use general::vfs::superblock::Superblock;
use general::vfs::{FS_REGISTRY, VfsContext, ensure_dir, mount_posix_shm_tmpfs};
use log::printk;

const SYSFS_DIR_PATH: &str = "/sys";
const SYSFS_DIR_MODE: FileMode = FileMode::new(0o555);
const SYSFS_FS_TYPE: &str = "sysfs";

/// 注册启动期核心文件系统驱动。
///
/// 这里按名字做幂等检查，避免未来固件路径或测试代码重复进入设备初始化时把同名
/// driver 重复塞进全局 registry。块文件系统的注册函数内部负责自己的适配器集合。
pub fn register_core_filesystems(tag: &str) {
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
    general::vfs::posix_compat::register_posix_device_policies().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register POSIX device number policies: {:?}",
            tag, err
        )
    });
    general::vfs::register_block_filesystems();
    general::vfs::rtc_devnode::register_devtmpfs_adapter().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register RTC devtmpfs adapter: {:?}",
            tag, err
        )
    });
    general::vfs::loop_devnode::register_devtmpfs_adapter().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register loop devtmpfs adapter: {:?}",
            tag, err
        )
    });
    general::vfs::loop_devnode::register_control_node().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register loop-control devtmpfs node: {:?}",
            tag, err
        )
    });
}

/// 创建并发布 devtmpfs 单例 superblock。
pub fn mount_devtmpfs(tag: &str) -> Arc<Superblock> {
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
pub fn mount_standard_compat_filesystems(tag: &str, ctx: &VfsContext) {
    general::vfs::net_ioctl::install_net_ioctl_compat();
    mount_posix_shm_tmpfs(ctx).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to mount tmpfs on /dev/shm: {:?}",
            tag, err
        )
    });
    mount_sysfs_on_sys(tag, ctx);
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

/// 安装 PnP bridge、设备初始化上下文和内建驱动。
///
/// `ctx` 只携带底层设备驱动需要的内核能力；POSIX `/dev` 命名、设备号等兼容层
/// 信息不进入这个上下文，仍由 devtmpfs/VFS 投影层处理。
pub fn activate_device_subsystem(tag: &str, dev_sb: Arc<Superblock>, ctx: DevInitContext) {
    general::vfs::devtmpfs::install_pnp_bridge(Arc::clone(&dev_sb)).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to install PnP devtmpfs bridge: {:?}",
            tag, err
        )
    });
    printk!("[kernel-start][{}] PnP devtmpfs callbacks installed", tag);

    set_dev_init_context(ctx);

    // random 驱动的熵源 hook 来自 arch/hal；它是内建设备驱动初始化前的能力注入，
    // 不属于任何具体固件路径，DTB 和 ACPI 必须共享同一顺序。
    hal::random::register_arch_hooks();
    printk!(
        "[kernel-start][{}] registered arch entropy source for random subsystem",
        tag
    );

    drivers::register_builtin_drivers().unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to register built-in PnP driver {}: {:?}",
            tag,
            err.driver(),
            err.error()
        )
    });
    printk!("[kernel-start][{}] registered built-in PnP drivers", tag);
}
