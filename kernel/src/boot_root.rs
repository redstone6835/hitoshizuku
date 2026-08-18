//! Linux 风格的启动根文件系统选择。
//!
//! DTB 与 ACPI 只负责发现固件和设备；initramfs、`rdinit=`、`root=` 以及最终
//! VFS 根的选择集中在这里，避免两条启动路径产生不同的 PID 1 语义。

use alloc::sync::Arc;

use general::dev::block::BlockDevice;
use general::dev::enumerate::DEVICES;
use general::vfs::device_files::projection::{active_block_devices, lookup_block_device_by_node};
use general::vfs::path::{self, Dirfd, LookupFlags};
use general::vfs::superblock::Superblock;
use general::vfs::{
    Credentials, FS_REGISTRY, FileMode, Mount, MountFlags, MountNamespace, VfsContext, VfsLimits,
    VfsRoot,
};
use log::printk;

use crate::initramfs::{InitramfsImage, InitramfsSource};

pub(crate) struct BootRoot {
    pub(crate) superblock: Arc<Superblock>,
    pub(crate) root_mount: Arc<Mount>,
    pub(crate) mount_ns: Arc<MountNamespace>,
    pub(crate) vfs_ctx: VfsContext,
    pub(crate) cred: Arc<Credentials>,
    pub(crate) source: &'static str,
    pub(crate) is_initramfs: bool,
}

/// 建立 Linux `kernel_init_freeable()` 风格的启动根。
///
/// 有 initramfs 时先解包并检查 `rdinit=`（默认 `/init`）是否可达；不可达时按
/// `root=` 切到真实根。没有显式 `root=` 时保留项目原有的单盘自动探测行为。
pub(crate) fn prepare(
    tag: &'static str,
    command_line: Option<&[u8]>,
    embedded_initramfs: Option<InitramfsImage>,
    external_initramfs: Option<InitramfsImage>,
) -> BootRoot {
    if embedded_initramfs.is_some() || external_initramfs.is_some() {
        let source = initramfs_source(embedded_initramfs, external_initramfs);
        let root = new_root(mount_tmpfs_superblock(tag), source, true);
        for image in [embedded_initramfs, external_initramfs]
            .into_iter()
            .flatten()
        {
            let archive_source = match image.source {
                InitramfsSource::Embedded => "embedded",
                InitramfsSource::External => "external",
            };
            printk!(
                "[kernel-start][{}] unpacking {} initramfs",
                tag,
                archive_source
            );
            crate::initramfs::unpack_newc(image, &root.vfs_ctx).unwrap_or_else(|err| {
                panic!(
                    "[kernel-start][{}] failed to unpack initramfs: {:?}",
                    tag, err
                )
            });
        }
        printk!("[kernel-start][{}] initramfs unpacked into root", tag);

        let ramdisk_init = crate::sched::ramdisk_init_command(command_line);
        if path::lookup(
            &root.vfs_ctx,
            &Dirfd::Cwd,
            ramdisk_init,
            LookupFlags::default(),
        )
        .is_ok()
        {
            return root;
        }

        if crate::sched::parse_init_command_line(command_line)
            .rdinit
            .is_some()
        {
            log::warning!(
                "[kernel-start][{}] check access for rdinit='{}' failed; ignoring it",
                tag,
                ramdisk_init
            );
        }
    }

    // 实机无串口验证：cmdline mygo.mmclog=<start_lba>[,count] 时，把内核启动
    // 日志写到块设备指定扇区（eMMC FAT 分区尾部未分配区，Debian 侧 dd 读取）。
    export_boot_log_to_mmc(command_line);

    // 根文件系统可能位于分区上（如 mmcblk0p3 / vda4）：先为所有 whole 盘
    // 扫描 MBR/GPT 分区表并注册分区设备，再解析 root= 或自动探测。
    let partitions = general::dev::partition::scan_and_register_all();
    if partitions != 0 {
        printk!(
            "[kernel-start][{}] registered {} partition device(s)",
            tag,
            partitions
        );
    }

    let (superblock, source) = mount_real_root(tag, command_line);
    new_root(superblock, source, false)
}

fn initramfs_source(
    embedded: Option<InitramfsImage>,
    external: Option<InitramfsImage>,
) -> &'static str {
    match (embedded.is_some(), external.is_some()) {
        (true, true) => "embedded + external initramfs",
        (true, false) => "embedded initramfs",
        (false, true) => "external initramfs",
        (false, false) => unreachable!(),
    }
}

pub(crate) fn root_command(command_line: Option<&[u8]>) -> Option<&str> {
    command_line
        .map(general::cmdline::Cmdline::new)
        .and_then(|cmdline| cmdline.find("root"))
}

/// 当前启动期设备命名只支持 devtmpfs 的直接块设备节点。
pub(crate) fn root_device_node(source: &str) -> Option<&str> {
    let node = source.strip_prefix("/dev/").unwrap_or(source);
    (!node.is_empty() && !node.contains('/') && !node.contains('\0')).then_some(node)
}

fn mount_real_root(
    tag: &'static str,
    command_line: Option<&[u8]>,
) -> (Arc<Superblock>, &'static str) {
    if let Some(source) = root_command(command_line) {
        let node = root_device_node(source).unwrap_or_else(|| {
            panic!(
                "[kernel-start][{}] unsupported root source '{}'; expected /dev/<device>",
                tag, source
            )
        });
        let dev = lookup_block_device_by_node(&DEVICES.functions, node)
            .or_else(|| {
                active_block_devices(&DEVICES.functions)
                    .into_iter()
                    .find(|dev| dev.name() == node)
            })
            .unwrap_or_else(|| {
                panic!(
                    "[kernel-start][{}] root device '{}' was not found",
                    tag, source
                )
            });
        return mount_block_root(dev).unwrap_or_else(|err| {
            panic!(
                "[kernel-start][{}] failed to mount root '{}': {}",
                tag, source, err
            )
        });
    }

    mount_first_block_root(tag).unwrap_or_else(|err| {
        panic!(
            "[kernel-start][{}] failed to mount block root: {}",
            tag, err
        )
    })
}

fn mount_tmpfs_superblock(tag: &str) -> Arc<Superblock> {
    FS_REGISTRY
        .find("tmpfs")
        .unwrap_or_else(|| panic!("[kernel-start][{}] tmpfs driver not found", tag))
        .mount(None, "")
        .unwrap_or_else(|err| {
            panic!(
                "[kernel-start][{}] failed to mount tmpfs root: {:?}",
                tag, err
            )
        })
}

fn new_root(superblock: Arc<Superblock>, source: &'static str, is_initramfs: bool) -> BootRoot {
    let root_mount = Mount::new(
        Arc::clone(&superblock),
        Arc::clone(&superblock.root_dentry),
        Arc::clone(&superblock.root_dentry),
        MountFlags::default(),
        None,
    );
    let mount_ns = MountNamespace::new(1, Arc::clone(&root_mount));
    let cred = Arc::new(Credentials::root());
    let vfs_ctx = VfsContext::new(
        Arc::clone(&superblock.root_dentry),
        Arc::clone(&root_mount),
        VfsRoot::new(Arc::clone(&superblock.root_dentry), Arc::clone(&root_mount)),
        Arc::clone(&mount_ns),
        Arc::clone(&cred),
        FileMode::new(0),
        VfsLimits::default_arc(),
    );
    BootRoot {
        superblock,
        root_mount,
        mount_ns,
        vfs_ctx,
        cred,
        source,
        is_initramfs,
    }
}

fn mount_block_root(
    dev: Arc<BlockDevice>,
) -> Result<(Arc<Superblock>, &'static str), &'static str> {
    general::vfs::mount_block_device_auto(dev, "")
        .map_err(|_| "unsupported or invalid root filesystem")
}

fn mount_first_block_root(tag: &str) -> Result<(Arc<Superblock>, &'static str), &'static str> {
    let devices = active_block_devices(&DEVICES.functions);
    if devices.is_empty() {
        return Err("no initramfs and no active block device found");
    }

    for dev in devices {
        match mount_block_root(Arc::clone(&dev)) {
            Ok(root) => return Ok(root),
            Err(err) => log::debug!(
                "[kernel-start][{}] block device {} is not root candidate: {}",
                tag,
                dev.name(),
                err
            ),
        }
    }

    Err("no active block device contains a supported root filesystem")
}

/// 把内核启动日志写入第一个可用块设备的指定扇区区间。
///
/// 用途：实机 bringup 无串口时的 SSH 侧信道验证——MyGO 原生引导后把日志写到
/// eMMC 的 FAT 分区尾部未分配扇区，回到 Debian 后用 dd 读取。
/// 命令行格式：mygo.mmclog=<start_lba>[,<sector_count>]（默认 64 扇区 = 32 KiB）。
fn export_boot_log_to_mmc(command_line: Option<&[u8]>) {
    use general::dev::block_sync::FsBlockAdapter;

    let cmdline = general::cmdline::Cmdline::new(command_line.unwrap_or_default());
    let Some(spec) = cmdline.find("mygo.mmclog") else {
        return;
    };
    let mut parts = spec.split(',');
    let Some(lba_text) = parts.next() else {
        return;
    };
    let Ok(start_lba) = lba_text.trim().parse::<u64>() else {
        log::warning!("[mygo-mmclog] invalid start lba: {}", lba_text);
        return;
    };
    let sector_count = parts
        .next()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(64)
        .clamp(1, 256);

    let text = log::LOGGER.export_text_limited(false, sector_count.saturating_mul(512));
    let mut buffer = alloc::vec![0u8; sector_count * 512];
    let copy_len = text.len().min(buffer.len());
    buffer[..copy_len].copy_from_slice(&text[..copy_len]);

    for dev in active_block_devices(&DEVICES.functions) {
        let adapter = match FsBlockAdapter::new(Arc::clone(&dev)) {
            Ok(adapter) => adapter,
            Err(_) => continue,
        };
        let blocks = u32::try_from(sector_count).unwrap_or(64);
        match adapter.write(start_lba, blocks, &buffer) {
            Ok(()) => {
                log::printk!(
                    "[mygo-mmclog] boot log written to {} at lba={} sectors={} bytes={}",
                    dev.name(),
                    start_lba,
                    sector_count,
                    copy_len
                );
                return;
            }
            Err(error) => {
                log::debug!("[mygo-mmclog] write to {} failed: {:?}", dev.name(), error);
            }
        }
    }
    log::warning!("[mygo-mmclog] no writable block device for boot log");
}
