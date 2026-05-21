//! Block-backed filesystem registration.
//!
//! extfs/fatfs keep the on-disk filesystem logic in their own crates.  This
//! module adapts a VFS mount source such as `/dev/vd0` to the global block
//! device registry and binds the resulting device to `SyncBlockBackend`.

use alloc::boxed::Box;
use alloc::sync::Arc;

use vfs::FS_REGISTRY;
use vfs::error::{VfsError, VfsResult};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock};

use crate::dev::block::BlockDevice;
use crate::dev::block_sync::SyncBlockBackend;
use crate::dev::enumerate::DEVICES;

#[derive(Clone, Copy)]
enum BlockFsKind {
    Ext,
    Fat,
}

struct BlockFsDriver {
    name: &'static str,
    kind: BlockFsKind,
}

impl BlockFsDriver {
    const fn new(name: &'static str, kind: BlockFsKind) -> Self {
        Self { name, kind }
    }
}

impl FsDriver for BlockFsDriver {
    fn name(&self) -> &'static str {
        self.name
    }

    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::default()
    }

    fn mount(&self, dev: Option<&str>, data: &str) -> VfsResult<Arc<Superblock>> {
        let dev = resolve_block_device(dev.ok_or(VfsError::NoDevice)?)?;
        match self.kind {
            BlockFsKind::Ext => {
                let backend = Arc::new(SyncBlockBackend::new(dev));
                let driver = extfs::ExtFsDriver::new();
                driver.bind_backend(backend);
                driver.mount(None, data)
            }
            BlockFsKind::Fat => {
                let backend = Arc::new(SyncBlockBackend::new(dev));
                let driver = fatfs::FatFsDriver::new();
                driver.bind_backend(backend);
                driver.mount(None, data)
            }
        }
    }

    fn kill_sb(&self, sb: Arc<Superblock>) {
        match self.kind {
            BlockFsKind::Ext => {
                let driver = extfs::ExtFsDriver::new();
                driver.kill_sb(sb);
            }
            BlockFsKind::Fat => {
                let driver = fatfs::FatFsDriver::new();
                driver.kill_sb(sb);
            }
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub fn register_block_filesystems() {
    for (name, kind) in [
        ("extfs", BlockFsKind::Ext),
        ("ext2", BlockFsKind::Ext),
        ("ext3", BlockFsKind::Ext),
        ("ext4", BlockFsKind::Ext),
        ("fatfs", BlockFsKind::Fat),
        ("fat", BlockFsKind::Fat),
        ("msdos", BlockFsKind::Fat),
        ("vfat", BlockFsKind::Fat),
    ] {
        FS_REGISTRY
            .register(Box::leak(Box::new(BlockFsDriver::new(name, kind))))
            .expect("[kernel-start] failed to register block filesystem driver");
    }
}

fn resolve_block_device(source: &str) -> VfsResult<Arc<BlockDevice>> {
    let name = match source.strip_prefix("/dev/") {
        Some(name) => name,
        None if !source.starts_with('/') => source,
        None => return Err(VfsError::NotFound),
    };
    if name.is_empty() || name.contains('/') {
        return Err(VfsError::NotFound);
    }
    DEVICES.block_devs.lookup(name).ok_or(VfsError::NotFound)
}
