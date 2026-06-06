//! 块设备承载的文件系统注册。
//!
//! 本模块只负责把 VFS mount 源解析为块设备，并把块设备
//! 包装成同步后端。具体磁盘格式由注册进来的 [`BlockFsDriver`] 实现。

use alloc::boxed::Box;
use alloc::sync::Arc;

use vfs::FS_REGISTRY;
use vfs::error::{VfsError, VfsResult};
use vfs::superblock::{FsDriver, FsDriverFlags, FsProbe, Superblock};

use crate::dev::block::BlockDevice;
use crate::dev::block_sync::{SyncBlockBackend, SyncIoError};

use super::mount_source::resolve_block_mount_source;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BlockFsProbe {
    None,
    Weak,
    Strong,
}

impl From<BlockFsProbe> for FsProbe {
    fn from(value: BlockFsProbe) -> Self {
        match value {
            BlockFsProbe::None => Self::None,
            BlockFsProbe::Weak => Self::Weak,
            BlockFsProbe::Strong => Self::Strong,
        }
    }
}

pub trait BlockFsDriver: Send + Sync {
    fn name(&self) -> &'static str;

    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    fn probe(&self, backend: &SyncBlockBackend) -> BlockFsProbe;

    fn mount_block(&self, backend: Arc<SyncBlockBackend>, data: &str)
    -> VfsResult<Arc<Superblock>>;

    fn kill_sb(&self, sb: Arc<Superblock>);
}

struct BlockFsAdapter {
    name: &'static str,
    driver: &'static dyn BlockFsDriver,
    auto_detect: bool,
}

impl BlockFsAdapter {
    const fn new(
        name: &'static str,
        driver: &'static dyn BlockFsDriver,
        auto_detect: bool,
    ) -> Self {
        Self {
            name,
            driver,
            auto_detect,
        }
    }
}

impl FsDriver for BlockFsAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn flags(&self) -> FsDriverFlags {
        let flags = FsDriverFlags::default().with(FsDriverFlags::BLOCK);
        if self.auto_detect {
            flags.with(FsDriverFlags::AUTO_DETECT)
        } else {
            flags
        }
    }

    fn probe(&self, dev: Option<&str>) -> FsProbe {
        let Some(source) = dev else {
            return FsProbe::None;
        };
        let Ok(dev) = resolve_block_device(source) else {
            return FsProbe::None;
        };
        let backend = SyncBlockBackend::new(dev);
        self.driver.probe(&backend).into()
    }

    fn mount(&self, dev: Option<&str>, data: &str) -> VfsResult<Arc<Superblock>> {
        let dev = resolve_block_device(dev.ok_or(VfsError::NoDevice)?)?;
        let backend = Arc::new(SyncBlockBackend::new(dev));
        if self.driver.probe(backend.as_ref()) == BlockFsProbe::None {
            return Err(VfsError::InvalidArgument);
        }
        self.driver.mount_block(backend, data)
    }

    fn kill_sb(&self, sb: Arc<Superblock>) {
        self.driver.kill_sb(sb);
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub fn register_block_fs_driver(driver: &'static dyn BlockFsDriver) -> VfsResult<()> {
    register_block_fs_name(driver.name(), driver, true)?;
    for alias in driver.aliases() {
        if *alias != driver.name() {
            register_block_fs_name(alias, driver, false)?;
        }
    }
    Ok(())
}

fn register_block_fs_name(
    name: &'static str,
    driver: &'static dyn BlockFsDriver,
    auto_detect: bool,
) -> VfsResult<usize> {
    FS_REGISTRY.register(Box::leak(Box::new(BlockFsAdapter::new(
        name,
        driver,
        auto_detect,
    ))))
}

pub fn mount_block_source_auto(
    source: &str,
    data: &str,
) -> VfsResult<(Arc<Superblock>, &'static str)> {
    let dev = resolve_block_mount_source(source)?;
    mount_block_device_auto(dev, data)
}

fn mount_block_backend_auto(
    backend: Arc<SyncBlockBackend>,
    data: &str,
) -> VfsResult<(Arc<Superblock>, &'static str)> {
    let mut last_error = VfsError::NoDevice;
    for wanted_probe in [BlockFsProbe::Strong, BlockFsProbe::Weak] {
        for entry in FS_REGISTRY.iter() {
            let driver = entry.driver;
            let Some(adapter) = driver.as_any().downcast_ref::<BlockFsAdapter>() else {
                continue;
            };
            if !adapter.auto_detect || adapter.driver.probe(backend.as_ref()) != wanted_probe {
                continue;
            }
            match adapter.driver.mount_block(Arc::clone(&backend), data) {
                Ok(sb) => return Ok((sb, driver.name())),
                Err(err) => last_error = err,
            }
        }
    }
    Err(last_error)
}

pub fn mount_block_device_auto(
    dev: Arc<BlockDevice>,
    data: &str,
) -> VfsResult<(Arc<Superblock>, &'static str)> {
    let source = alloc::string::String::from(dev.name());
    let backend = Arc::new(SyncBlockBackend::new(dev));
    let (sb, fs_name) = mount_block_backend_auto(backend, data)?;
    let root_source = alloc::format!("{} ({})", source, fs_name).leak();
    Ok((sb, root_source))
}

pub fn register_block_filesystems() {
    register_block_fs_driver(&EXT_BLOCK_FS_DRIVER)
        .expect("[kernel-start] failed to register ext block filesystem driver");
    register_block_fs_driver(&FAT_BLOCK_FS_DRIVER)
        .expect("[kernel-start] failed to register fat block filesystem driver");
}

fn resolve_block_device(source: &str) -> VfsResult<Arc<BlockDevice>> {
    resolve_block_mount_source(source)
}

struct ExtBlockFsDriver;

static EXT_BLOCK_FS_DRIVER: ExtBlockFsDriver = ExtBlockFsDriver;

impl BlockFsDriver for ExtBlockFsDriver {
    fn name(&self) -> &'static str {
        "extfs"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["ext2", "ext3", "ext4"]
    }

    fn probe(&self, backend: &SyncBlockBackend) -> BlockFsProbe {
        let mut magic = [0u8; 2];
        match read_backend_bytes(backend, 1024 + 56, &mut magic) {
            Ok(()) if magic == [0x53, 0xef] => BlockFsProbe::Strong,
            _ => BlockFsProbe::None,
        }
    }

    fn mount_block(
        &self,
        backend: Arc<SyncBlockBackend>,
        data: &str,
    ) -> VfsResult<Arc<Superblock>> {
        let driver = extfs::ExtFsDriver::new();
        let backend: Arc<dyn extfs::BlockBackend> = backend;
        driver.bind_backend(backend);
        driver.mount(None, data)
    }

    fn kill_sb(&self, sb: Arc<Superblock>) {
        extfs::ExtFsDriver::new().kill_sb(sb);
    }
}

struct FatBlockFsDriver;

static FAT_BLOCK_FS_DRIVER: FatBlockFsDriver = FatBlockFsDriver;

impl BlockFsDriver for FatBlockFsDriver {
    fn name(&self) -> &'static str {
        "fatfs"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["fat", "msdos", "vfat"]
    }

    fn probe(&self, backend: &SyncBlockBackend) -> BlockFsProbe {
        let sector_size = backend.sector_size_bytes() as usize;
        if !(512..=4096).contains(&sector_size) || !sector_size.is_power_of_two() {
            return BlockFsProbe::None;
        }
        let mut boot = alloc::vec![0u8; sector_size];
        if backend.read(0, 1, &mut boot).is_err() {
            return BlockFsProbe::None;
        }
        if boot.get(510..512) != Some(&[0x55, 0xaa]) {
            return BlockFsProbe::None;
        }

        let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as usize;
        let sectors_per_cluster = boot[13] as usize;
        let reserved_sectors = u16::from_le_bytes([boot[14], boot[15]]);
        let num_fats = boot[16];
        let root_entries = u16::from_le_bytes([boot[17], boot[18]]);
        let total_sectors_16 = u16::from_le_bytes([boot[19], boot[20]]);
        let total_sectors_32 = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]);
        let fat_size_16 = u16::from_le_bytes([boot[22], boot[23]]);
        let fat_size_32 = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);
        let total_sectors = if total_sectors_16 != 0 {
            total_sectors_16 as u32
        } else {
            total_sectors_32
        };
        let fat_size = if fat_size_16 != 0 {
            fat_size_16 as u32
        } else {
            fat_size_32
        };

        if bytes_per_sector == sector_size
            && sectors_per_cluster != 0
            && sectors_per_cluster <= 128
            && sectors_per_cluster.is_power_of_two()
            && reserved_sectors != 0
            && num_fats != 0
            && num_fats <= 4
            && total_sectors != 0
            && fat_size != 0
            && (root_entries != 0 || fat_size_16 == 0)
        {
            BlockFsProbe::Strong
        } else {
            BlockFsProbe::Weak
        }
    }

    fn mount_block(
        &self,
        backend: Arc<SyncBlockBackend>,
        data: &str,
    ) -> VfsResult<Arc<Superblock>> {
        let driver = fatfs::FatFsDriver::new();
        let backend: Arc<dyn fatfs::BlockBackend> = backend;
        driver.bind_backend(backend);
        driver.mount(None, data)
    }

    fn kill_sb(&self, sb: Arc<Superblock>) {
        fatfs::FatFsDriver::new().kill_sb(sb);
    }
}

fn read_backend_bytes(
    backend: &SyncBlockBackend,
    byte_offset: u64,
    out: &mut [u8],
) -> VfsResult<()> {
    if out.is_empty() {
        return Ok(());
    }

    let sector_size = backend.sector_size_bytes() as usize;
    if sector_size == 0 {
        return Err(VfsError::InvalidArgument);
    }
    let in_sector = byte_offset as usize % sector_size;
    let start_lba = byte_offset / sector_size as u64;
    let total = in_sector
        .checked_add(out.len())
        .ok_or(VfsError::InvalidArgument)?;
    let sector_count = total.div_ceil(sector_size);
    let mut buffer = alloc::vec![0u8; sector_count * sector_size];
    backend
        .read(start_lba, sector_count as u32, &mut buffer)
        .map_err(sync_error_to_vfs)?;
    out.copy_from_slice(&buffer[in_sector..in_sector + out.len()]);
    Ok(())
}

fn sync_error_to_vfs(err: SyncIoError) -> VfsError {
    match err {
        SyncIoError::BufferTooSmall | SyncIoError::InvalidRange => VfsError::InvalidArgument,
        SyncIoError::Submit(_) | SyncIoError::Io(_) => VfsError::Io,
    }
}
