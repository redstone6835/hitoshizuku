//! Linux-style boot root selection shared by every firmware path.
//!
//! DTB and ACPI are responsible for firmware and device discovery. Initramfs
//! layering, `rdinit=`, `root=` and the final VFS root are resolved here so all
//! boot protocols give PID 1 the same semantics.

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

/// Prepare the boot root with Linux `kernel_init_freeable()`-style semantics.
///
/// Embedded and firmware-provided initramfs archives are layered in that order.
/// The ramdisk stays as `/` only when `rdinit=` (or the default `/init`) exists;
/// otherwise the selected `root=` device, or the first mountable block device,
/// becomes the real root.
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

    // `root=` commonly names a partition rather than a whole disk. Publish all
    // discoverable MBR/GPT partitions before resolving the requested devtmpfs node.
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

/// Boot-time source resolution currently accepts direct devtmpfs block nodes.
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
