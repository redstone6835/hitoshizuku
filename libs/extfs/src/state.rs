//! 驱动对外暴露的类型 + 共享状态。
//!
//! [`BlockBackend`] 是 ext 驱动对块设备的同步 I/O 契约。[`ExtFsDriver`]
//! 实现 [`vfs::superblock::FsDriver`],挂载时产生一个持有 [`FsState`]
//! 的 [`Superblock`](vfs::superblock::Superblock)。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vfs::cred::{Gid, Uid};
use vfs::dentry::Dentry;
use vfs::error::{VfsError, VfsResult};
use vfs::inode::{Inode, InodeId, InodeMeta};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{
    FsDriver, FsDriverFlags, InodeCache, Superblock as VfsSuperblock, SuperblockOps,
};
use vfs::sync::Spinlock;

use crate::bgd::{self, GroupDesc};
use crate::inode::{ExtInodeOps, load_inode};
use crate::layout::{EXT4_ROOT_INO, ExtKind};
use crate::sb::{self, Superblock as ExtSb};

const BLOCK_CACHE_CAP: usize = 128;

struct BlockCacheEntry {
    block: u64,
    data: Vec<u8>,
    last_used: u64,
}

struct BlockCache {
    entries: Vec<BlockCacheEntry>,
    tick: u64,
    block_size: usize,
}

impl BlockCache {
    fn new(block_size: u32) -> Self {
        Self {
            entries: Vec::new(),
            tick: 0,
            block_size: block_size as usize,
        }
    }

    fn read(&mut self, block: u64, out: &mut [u8]) -> bool {
        if out.len() != self.block_size {
            return false;
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.block == block) {
            self.tick = self.tick.wrapping_add(1);
            entry.last_used = self.tick;
            out.copy_from_slice(&entry.data);
            return true;
        }
        false
    }

    fn insert(&mut self, block: u64, data: &[u8]) {
        if data.len() != self.block_size {
            return;
        }
        self.tick = self.tick.wrapping_add(1);
        if let Some(entry) = self.entries.iter_mut().find(|e| e.block == block) {
            entry.data.copy_from_slice(data);
            entry.last_used = self.tick;
            return;
        }
        if self.entries.len() < BLOCK_CACHE_CAP {
            self.entries.push(BlockCacheEntry {
                block,
                data: Vec::from(data),
                last_used: self.tick,
            });
            return;
        }
        if let Some(victim) = self.entries.iter_mut().min_by_key(|e| e.last_used) {
            victim.block = block;
            victim.data.copy_from_slice(data);
            victim.last_used = self.tick;
        }
    }
}

/// 块设备同步 I/O 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockBackendError {
    Io,
    OutOfRange,
    Unsupported,
}

/// 文件系统与块设备之间的同步契约。`read_sectors` / `write_sectors` 按扇区
/// 粒度工作:调用方保证 `buf.len() == sector_size * count`。只读驱动中
/// `write_sectors` 不会被 inode/file 调用,但仍保留在 trait 里以便未来扩展。
pub trait BlockBackend: Send + Sync {
    fn sector_size(&self) -> u32;
    fn sector_count(&self) -> u64;
    fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockBackendError>;
    fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError>;
}

static EXTFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 单个块组的运行时计数(独立于描述符的 bitmap 布局,便于原子调整)。
#[derive(Debug, Clone, Copy)]
pub(crate) struct GroupCounts {
    pub free_blocks: u32,
    pub free_inodes: u32,
    pub used_dirs: u32,
}

/// 已挂载 ext FS 的共享状态。
pub(crate) struct FsState {
    pub(crate) backend: Arc<dyn BlockBackend>,
    pub(crate) ext_sb: ExtSb,
    pub(crate) group_desc: Spinlock<alloc::vec::Vec<GroupDesc>>,
    pub(crate) group_counts: Spinlock<alloc::vec::Vec<GroupCounts>>,
    block_cache: Spinlock<BlockCache>,
    block_cache_epoch: core::sync::atomic::AtomicU64,
    pub(crate) sb_free_blocks: core::sync::atomic::AtomicU64,
    pub(crate) sb_free_inodes: core::sync::atomic::AtomicU32,
    pub(crate) block_alloc_hint: core::sync::atomic::AtomicU64,
    pub(crate) inode_alloc_hint: core::sync::atomic::AtomicU32,
    pub(crate) alloc_meta_dirty: AtomicBool,
    /// 只读挂载标志(由驱动 flags 或 remount 控制)。
    pub(crate) read_only: core::sync::atomic::AtomicBool,
}

impl FsState {
    /// 只读地取一个块组描述符副本。
    pub(crate) fn group_desc_ref(&self, group: u32) -> Result<GroupDesc, BlockBackendError> {
        self.group_desc
            .lock()
            .get(group as usize)
            .copied()
            .ok_or(BlockBackendError::OutOfRange)
    }
    /// 与 `group_desc_ref` 一样但供分配路径调用(语义保留)。
    pub(crate) fn group_desc_mut(&self, group: u32) -> Result<GroupDesc, BlockBackendError> {
        self.group_desc_ref(group)
    }

    pub(crate) fn group_counts(&self, group: u32) -> Result<GroupCounts, BlockBackendError> {
        self.group_counts
            .lock()
            .get(group as usize)
            .copied()
            .ok_or(BlockBackendError::OutOfRange)
    }

    pub(crate) fn adjust_group_free_blocks(
        &self,
        group: u32,
        delta: i32,
    ) -> Result<(), BlockBackendError> {
        {
            let mut g = self.group_counts.lock();
            let c = g
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            c.free_blocks = apply_delta(c.free_blocks, delta);
        }
        let _ = group;
        self.alloc_meta_dirty.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn adjust_group_free_inodes(
        &self,
        group: u32,
        delta: i32,
    ) -> Result<(), BlockBackendError> {
        {
            let mut g = self.group_counts.lock();
            let c = g
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            c.free_inodes = apply_delta(c.free_inodes, delta);
        }
        let _ = group;
        self.alloc_meta_dirty.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn adjust_group_used_dirs(
        &self,
        group: u32,
        delta: i32,
    ) -> Result<(), BlockBackendError> {
        {
            let mut g = self.group_counts.lock();
            let c = g
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            c.used_dirs = apply_delta(c.used_dirs, delta);
        }
        let _ = group;
        self.alloc_meta_dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn adjust_sb_free_blocks(&self, delta: i64) -> Result<(), BlockBackendError> {
        let prev = self
            .sb_free_blocks
            .load(core::sync::atomic::Ordering::Acquire);
        let next = if delta < 0 {
            prev.saturating_sub((-delta) as u64)
        } else {
            prev + delta as u64
        };
        self.sb_free_blocks
            .store(next, core::sync::atomic::Ordering::Release);
        self.alloc_meta_dirty.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn adjust_sb_free_inodes(&self, delta: i32) -> Result<(), BlockBackendError> {
        let prev = self
            .sb_free_inodes
            .load(core::sync::atomic::Ordering::Acquire);
        let next = apply_delta(prev, delta);
        self.sb_free_inodes
            .store(next, core::sync::atomic::Ordering::Release);
        self.alloc_meta_dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn ext_sb_free_blocks(&self) -> u64 {
        self.sb_free_blocks
            .load(core::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn ext_sb_free_inodes(&self) -> u32 {
        self.sb_free_inodes
            .load(core::sync::atomic::Ordering::Acquire)
    }
    #[inline]
    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only.load(core::sync::atomic::Ordering::Acquire)
    }

    /// 以块为单位读取。
    #[inline]
    pub(crate) fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), BlockBackendError> {
        if out.len() != self.ext_sb.block_size as usize {
            return Err(BlockBackendError::OutOfRange);
        }
        {
            let mut cache = self.block_cache.lock();
            if cache.read(block, out) {
                return Ok(());
            }
        }
        let epoch = self.block_cache_epoch.load(Ordering::Acquire);
        bgd::read_blocks(self.backend.as_ref(), &self.ext_sb, block, 1, out)?;
        if self.block_cache_epoch.load(Ordering::Acquire) == epoch {
            self.block_cache.lock().insert(block, out);
        }
        Ok(())
    }

    /// 以块为单位写入。
    pub(crate) fn write_block(&self, block: u64, data: &[u8]) -> Result<(), BlockBackendError> {
        if data.len() != self.ext_sb.block_size as usize {
            return Err(BlockBackendError::OutOfRange);
        }
        bgd::write_blocks(self.backend.as_ref(), &self.ext_sb, block, 1, data)?;
        self.block_cache_epoch.fetch_add(1, Ordering::AcqRel);
        self.block_cache.lock().insert(block, data);
        Ok(())
    }

    /// 批量读块。
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn read_blocks(
        &self,
        start_block: u64,
        count: u32,
        out: &mut [u8],
    ) -> Result<(), BlockBackendError> {
        let bs = self.ext_sb.block_size as usize;
        let expected = bs * count as usize;
        if out.len() != expected {
            return Err(BlockBackendError::OutOfRange);
        }
        for i in 0..count {
            let off = i as usize * bs;
            self.read_block(start_block + i as u64, &mut out[off..off + bs])?;
        }
        Ok(())
    }

    pub(crate) fn write_blocks(
        &self,
        start_block: u64,
        count: u32,
        data: &[u8],
    ) -> Result<(), BlockBackendError> {
        let expected = self.ext_sb.block_size as usize * count as usize;
        if data.len() != expected {
            return Err(BlockBackendError::OutOfRange);
        }
        if count == 1 {
            return self.write_block(start_block, data);
        }
        bgd::write_blocks(self.backend.as_ref(), &self.ext_sb, start_block, count, data)?;
        self.block_cache_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    pub(crate) fn flush_alloc_metadata(&self) -> Result<(), BlockBackendError> {
        if !self.alloc_meta_dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        for group in 0..self.ext_sb.groups_count {
            crate::alloc_mod::flush_group_desc(self, group)?;
        }
        crate::alloc_mod::write_superblock(self)
    }

    #[inline]
    pub(crate) fn io_epoch(&self) -> u64 {
        self.block_cache_epoch.load(Ordering::Acquire)
    }

    /// 定位一个 inode 号所在的块号与块内字节偏移。
    pub(crate) fn inode_location(&self, ino: u32) -> Result<(u64, u32), BlockBackendError> {
        if ino == 0 || ino > self.ext_sb.inodes_count {
            return Err(BlockBackendError::OutOfRange);
        }
        let per_group = self.ext_sb.inodes_per_group;
        let inode_size = self.ext_sb.inode_size;
        let block_size = self.ext_sb.block_size;
        let group = (ino - 1) / per_group;
        let offset_in_group = (ino - 1) % per_group;
        let gd = self
            .group_desc
            .lock()
            .get(group as usize)
            .copied()
            .ok_or(BlockBackendError::OutOfRange)?;
        let byte_off = offset_in_group as u64 * inode_size as u64;
        let block = gd.inode_table + byte_off / block_size as u64;
        let in_block = (byte_off % block_size as u64) as u32;
        Ok((block, in_block))
    }
}

#[inline]
fn apply_delta(cur: u32, delta: i32) -> u32 {
    if delta < 0 {
        cur.saturating_sub((-delta) as u32)
    } else {
        cur + delta as u32
    }
}

/// 对外的驱动句柄。
pub struct ExtFsDriver {
    backend: Spinlock<Option<Arc<dyn BlockBackend>>>,
}

impl ExtFsDriver {
    pub const fn new() -> Self {
        Self {
            backend: Spinlock::new(None),
        }
    }

    pub fn bind_backend(&self, backend: Arc<dyn BlockBackend>) {
        *self.backend.lock() = Some(backend);
    }
}

impl Default for ExtFsDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl FsDriver for ExtFsDriver {
    fn name(&self) -> &'static str {
        "extfs"
    }

    fn flags(&self) -> FsDriverFlags {
        // ext 驱动是只读,强制只读挂载由驱动自己拦截。
        FsDriverFlags::default()
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<VfsSuperblock>> {
        let backend = self
            .backend
            .lock()
            .as_ref()
            .map(Arc::clone)
            .ok_or(VfsError::NoDevice)?;
        mount_impl(backend)
    }

    fn kill_sb(&self, _sb: Arc<VfsSuperblock>) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) struct ExtFsSuperblockOps {
    pub(crate) state: Arc<FsState>,
}

impl SuperblockOps for ExtFsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<VfsSuperblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }
    fn statfs(&self, sb: &Arc<VfsSuperblock>) -> VfsResult<FsStat> {
        let s = &self.state.ext_sb;
        Ok(FsStat {
            fs_type: 0xef53_0000,
            block_size: s.block_size as u64,
            total_blocks: s.blocks_count,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: s.inodes_count as u64,
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: 255,
        })
    }
    fn sync_fs(&self, _sb: &Arc<VfsSuperblock>) -> VfsResult<()> {
        self.state.flush_alloc_metadata().map_err(map_err)
    }
    fn remount(&self, _sb: &Arc<VfsSuperblock>, _flags: MountFlags) -> VfsResult<()> {
        // 只读驱动,remount 忽略写标志
        Ok(())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn mount_impl(backend: Arc<dyn BlockBackend>) -> VfsResult<Arc<VfsSuperblock>> {
    let ext_sb = sb::load(backend.as_ref()).map_err(map_err)?;
    let group_desc = bgd::load_all(backend.as_ref(), &ext_sb).map_err(map_err)?;
    let group_counts = group_desc
        .iter()
        .map(|g| GroupCounts {
            free_blocks: g.free_blocks_count,
            free_inodes: g.free_inodes_count,
            used_dirs: g.used_dirs_count,
        })
        .collect::<alloc::vec::Vec<_>>();
    let free_blocks = ext_sb.free_blocks_count;
    let free_inodes = ext_sb.free_inodes_count;
    let block_size = ext_sb.block_size;
    let state = Arc::new(FsState {
        backend: Arc::clone(&backend),
        ext_sb,
        group_desc: Spinlock::new(group_desc),
        group_counts: Spinlock::new(group_counts),
        block_cache: Spinlock::new(BlockCache::new(block_size)),
        block_cache_epoch: core::sync::atomic::AtomicU64::new(0),
        sb_free_blocks: core::sync::atomic::AtomicU64::new(free_blocks),
        sb_free_inodes: core::sync::atomic::AtomicU32::new(free_inodes),
        block_alloc_hint: core::sync::atomic::AtomicU64::new(0),
        inode_alloc_hint: core::sync::atomic::AtomicU32::new(0),
        alloc_meta_dirty: AtomicBool::new(false),
        read_only: core::sync::atomic::AtomicBool::new(false),
    });

    // 加载根 inode(2 号)
    let (root_meta_on_disk, root_raw) = load_inode(&state, EXT4_ROOT_INO).map_err(map_err)?;
    let fs_id = FsId::new(EXTFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

    let kind_hint = match state.ext_sb.kind {
        ExtKind::Ext2 => "ext2",
        ExtKind::Ext3 => "ext3",
        ExtKind::Ext4 => "ext4",
    };

    let sb = VfsSuperblock::new(|weak_sb| {
        let now = Timespec::ZERO;
        let meta = InodeMeta {
            size: root_meta_on_disk.size,
            nlink: root_meta_on_disk.nlink as u32,
            mode: FileMode::new((root_meta_on_disk.mode & 0o7777) as u16),
            uid: Uid(root_meta_on_disk.uid),
            gid: Gid(root_meta_on_disk.gid),
            atime: now,
            mtime: now,
            ctime: now,
            blocks: root_meta_on_disk.blocks_512,
        };
        let ops = ExtInodeOps::new(Arc::clone(&state), EXT4_ROOT_INO, root_raw.clone());
        let root_inode = Inode::new(
            InodeId {
                fs_id,
                ino: EXT4_ROOT_INO as u64,
            },
            FileType::Directory,
            DevId::new(0, 0),
            block_size,
            None,
            meta,
            Arc::new(ops) as Arc<dyn vfs::inode::InodeOps + Send + Sync>,
            weak_sb.clone(),
        );
        let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
        VfsSuperblock {
            fs_type: kind_hint,
            fs_id,
            dev_id: None,
            block_size,
            name_max: 255,
            root_inode,
            root_dentry,
            inode_cache: InodeCache::new(),
            ops: Box::new(ExtFsSuperblockOps {
                state: Arc::clone(&state),
            }),
            self_weak: weak_sb,
        }
    });

    Ok(sb)
}

/// 将 [`BlockBackendError`] 映射到 VFS 错误。
pub(crate) fn map_err(e: BlockBackendError) -> VfsError {
    match e {
        BlockBackendError::Io => VfsError::Io,
        BlockBackendError::OutOfRange => VfsError::InvalidArgument,
        BlockBackendError::Unsupported => VfsError::NotSupported,
    }
}
