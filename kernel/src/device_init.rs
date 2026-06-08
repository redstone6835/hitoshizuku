//! 设备子系统启动期公共流程。
//!
//! DTB 与 ACPI 的固件解析策略不同，但设备抽象层的启动顺序必须一致：
//! 先准备 VFS 投影载体，再安装 PnP bridge，最后注册内建设备驱动。把这段逻辑
//! 集中在这里，可以避免两个启动路径各自复制顺序并留下不一致的边界条件。

use alloc::boxed::Box;
use alloc::sync::Arc;

use general::dev::drivers;
use general::dev::pnp::{DevInitContext, set_dev_init_context};
use general::vfs::FS_REGISTRY;
use general::vfs::devtmpfs::DevTmpfsSuperblockOps;
use general::vfs::superblock::Superblock;
use log::printk;

// TODO(dev-init): DTB/ACPI 仍各自挂载 `/dev`、`/dev/shm` 和 `/sys`。
// 后续应把标准设备/兼容层挂载收敛到本模块，并改为返回结构化启动错误，避免
// 固件路径复制顺序约束。

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
    general::vfs::register_block_filesystems();
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
            "[kernel-start][{}] failed to register built-in PnP drivers: {:?}",
            tag, err
        )
    });
    printk!("[kernel-start][{}] registered built-in PnP drivers", tag);
}
