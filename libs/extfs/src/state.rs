//! 驱动对外暴露的类型 + 共享状态。
//!
//! [`BlockBackend`] 是 ext 驱动对块设备的同步 I/O 契约。[`ExtFsDriver`]
//! 实现 [`vfs::superblock::FsDriver`],挂载时产生一个持有 [`FsState`]
//! 的 [`Superblock`](vfs::superblock::Superblock)。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
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
use crate::inode_wr::RawInode;
use crate::layout::{EXT4_ROOT_INO, ExtKind};
use crate::sb::{self, Superblock as ExtSb};

const BLOCK_CACHE_CAP: usize = 512;

struct BlockCacheSlot {
    block: u64,
    data: Vec<u8>,
    referenced: bool,
    occupied: bool,
}

/// O(log n) 块缓存：BTreeMap 索引 + Clock eviction。
struct BlockCache {
    slots: Vec<BlockCacheSlot>,
    /// block_no → slot 索引。
    index: BTreeMap<u64, usize>,
    /// Clock eviction 指针（循环扫描）。
    hand: usize,
    block_size: usize,
}

impl BlockCache {
    fn new(block_size: u32) -> Self {
        Self {
            slots: Vec::new(),
            index: BTreeMap::new(),
            hand: 0,
            block_size: block_size as usize,
        }
    }

    fn read(&mut self, block: u64, out: &mut [u8]) -> bool {
        if out.len() != self.block_size {
            return false;
        }
        if let Some(&idx) = self.index.get(&block) {
            let slot = &mut self.slots[idx];
            slot.referenced = true;
            out.copy_from_slice(&slot.data);
            return true;
        }
        false
    }

    fn insert(&mut self, block: u64, data: &[u8]) {
        if data.len() != self.block_size {
            return;
        }
        // 命中：原地更新
        if let Some(&idx) = self.index.get(&block) {
            let slot = &mut self.slots[idx];
            slot.data.copy_from_slice(data);
            slot.referenced = true;
            return;
        }
        // 未满：直接 push
        if self.slots.len() < BLOCK_CACHE_CAP {
            let idx = self.slots.len();
            self.slots.push(BlockCacheSlot {
                block,
                data: Vec::from(data),
                referenced: true,
                occupied: true,
            });
            self.index.insert(block, idx);
            return;
        }
        // 已满：Clock eviction
        let cap = self.slots.len();
        let mut steps = 0usize;
        loop {
            let i = self.hand;
            self.hand = (self.hand + 1) % cap;
            let slot = &mut self.slots[i];
            if !slot.occupied {
                slot.block = block;
                slot.data.copy_from_slice(data);
                slot.referenced = true;
                slot.occupied = true;
                self.index.insert(block, i);
                return;
            }
            if slot.referenced {
                slot.referenced = false;
                steps += 1;
                if steps > cap * 2 {
                    // 保险：兜底 LRU 化淘汰
                    let old_block = slot.block;
                    self.index.remove(&old_block);
                    slot.block = block;
                    slot.data.copy_from_slice(data);
                    slot.referenced = true;
                    self.index.insert(block, i);
                    return;
                }
                continue;
            }
            // 命中淘汰
            let old_block = slot.block;
            self.index.remove(&old_block);
            slot.block = block;
            slot.data.copy_from_slice(data);
            slot.referenced = true;
            self.index.insert(block, i);
            return;
        }
    }

    fn invalidate_range(&mut self, start: u64, count: u32) {
        if count == 0 {
            return;
        }
        let end = start.saturating_add(count as u64);
        // 收集需要移除的 block 号（避免在借用 self.index 时修改它）
        let to_remove: Vec<u64> = self.index.range(start..end).map(|(&b, _)| b).collect();
        for block in to_remove {
            if let Some(idx) = self.index.remove(&block) {
                let slot = &mut self.slots[idx];
                slot.occupied = false;
                slot.referenced = false;
            }
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
    pub(crate) alloc_group_dirty: Spinlock<alloc::vec::Vec<u8>>,
    pub(crate) alloc_sb_dirty: AtomicBool,
    pub(crate) dirty_inodes: Spinlock<alloc::vec::Vec<Arc<Spinlock<RawInode>>>>,
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
        self.mark_group_dirty(group)?;
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
        self.mark_group_dirty(group)?;
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
        self.mark_group_dirty(group)?;
        Ok(())
    }

    fn mark_group_dirty(&self, group: u32) -> Result<(), BlockBackendError> {
        let mut dirty = self.alloc_group_dirty.lock();
        let slot = dirty
            .get_mut(group as usize)
            .ok_or(BlockBackendError::OutOfRange)?;
        *slot = 1;
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
        self.alloc_sb_dirty.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn adjust_sb_free_inodes(&self, delta: i32) -> Result<(), BlockBackendError> {
        let prev = self
            .sb_free_inodes
            .load(core::sync::atomic::Ordering::Acquire);
        let next = apply_delta(prev, delta);
        self.sb_free_inodes
            .store(next, core::sync::atomic::Ordering::Release);
        self.alloc_sb_dirty.store(true, Ordering::Release);
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
        if count == 1 {
            return self.read_block(start_block, out);
        }
        // 分批读取，每批不超过 MAX_CHUNK_BLOCKS，避免超出 VirtIO 队列限制
        const MAX_CHUNK_BLOCKS: u32 = 128; // 128×4KB=512KB，VirtIO 256 descriptor 安全范围内
        let mut off = 0usize;
        let mut remaining = count;
        let mut block = start_block;
        while remaining > 0 {
            let n = remaining.min(MAX_CHUNK_BLOCKS);
            bgd::read_blocks(
                self.backend.as_ref(),
                &self.ext_sb,
                block,
                n,
                &mut out[off..off + bs * n as usize],
            )?;
            off += bs * n as usize;
            block += n as u64;
            remaining -= n;
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
        if count == 0 {
            return Ok(());
        }
        bgd::write_blocks(
            self.backend.as_ref(),
            &self.ext_sb,
            start_block,
            count,
            data,
        )?;
        self.block_cache_epoch.fetch_add(1, Ordering::AcqRel);
        self.block_cache.lock().invalidate_range(start_block, count);
        Ok(())
    }

    pub(crate) fn flush_alloc_metadata(&self) -> Result<(), BlockBackendError> {
        let dirty_groups = {
            let mut dirty = self.alloc_group_dirty.lock();
            let mut groups = Vec::new();
            for (group, is_dirty) in dirty.iter_mut().enumerate() {
                if *is_dirty != 0 {
                    *is_dirty = 0;
                    groups.push(group as u32);
                }
            }
            groups
        };
        let sb_dirty = self.alloc_sb_dirty.swap(false, Ordering::AcqRel);

        for (idx, &group) in dirty_groups.iter().enumerate() {
            if let Err(err) = crate::alloc_mod::flush_group_desc(self, group) {
                let mut dirty = self.alloc_group_dirty.lock();
                for &pending_group in &dirty_groups[idx..] {
                    if let Some(slot) = dirty.get_mut(pending_group as usize) {
                        *slot = 1;
                    }
                }
                if sb_dirty {
                    self.alloc_sb_dirty.store(true, Ordering::Release);
                }
                return Err(err);
            }
        }

        if sb_dirty {
            if let Err(err) = crate::alloc_mod::write_superblock(self) {
                self.alloc_sb_dirty.store(true, Ordering::Release);
                return Err(err);
            }
        }
        Ok(())
    }

    pub(crate) fn mark_inode_dirty(&self, raw: &Arc<Spinlock<RawInode>>) {
        let mut dirty = self.dirty_inodes.lock();
        if dirty.iter().any(|entry| Arc::ptr_eq(entry, raw)) {
            return;
        }
        dirty.push(Arc::clone(raw));
    }

    pub(crate) fn flush_dirty_inodes(&self) -> Result<(), BlockBackendError> {
        let pending = {
            let mut dirty = self.dirty_inodes.lock();
            if dirty.is_empty() {
                return Ok(());
            }
            core::mem::take(&mut *dirty)
        };

        for (idx, raw) in pending.iter().enumerate() {
            let snapshot = loop {
                if let Some(guard) = raw.try_lock() {
                    break guard.clone();
                }
                if sched::is_ready() {
                    sched::schedule_once(sched::now_ns_public());
                } else {
                    core::hint::spin_loop();
                }
            };
            if let Err(err) = crate::inode_wr::write_raw(self, &snapshot) {
                let mut dirty = self.dirty_inodes.lock();
                for pending_raw in &pending[idx..] {
                    if !dirty.iter().any(|entry| Arc::ptr_eq(entry, pending_raw)) {
                        dirty.push(Arc::clone(pending_raw));
                    }
                }
                return Err(err);
            }
        }
        Ok(())
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
            free_inodes: s.free_inodes_count as u64,
            fs_id: sb.fs_id.raw(),
            name_max: 255,
        })
    }
    fn sync_fs(&self, _sb: &Arc<VfsSuperblock>) -> VfsResult<()> {
        self.state.flush_dirty_inodes().map_err(map_err)?;
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

fn discard_orphan_file(state: &Arc<FsState>) -> Result<(), BlockBackendError> {
    let ino = state.ext_sb.orphan_file_inum;
    if ino == 0 || ino > state.ext_sb.inodes_count {
        return Ok(());
    }

    let mut raw = crate::inode_wr::read_raw(state, ino)?;
    let mut i_block = [0u8; 60];
    i_block.copy_from_slice(raw.i_block());

    // 当前内核没有 ext4 orphan_file 维护逻辑。若直接只清 superblock feature,
    // 原 orphan file inode 会变成宿主 fsck 眼中的 unattached inode；因此挂载时
    // 主动丢弃它占用的数据块和 inode 位图，再把 superblock 指针清零。
    if raw.flags() & crate::layout::EXT4_EXTENTS_FL != 0 {
        crate::extent_wr::free_tree(state, &i_block)?;
    } else {
        crate::map_wr::free_all_blocks(state, &mut i_block)?;
    }

    raw.bytes.fill(0);
    // extfs 库层无法直接访问内核 realtime；写合法 epoch 秒即可避免
    // e2fsck 把过小 dtime 误判为损坏 orphan 链。
    raw.set_dtime(1_700_000_000);
    crate::inode_wr::write_raw(state, &raw)?;
    crate::alloc_mod::free_inode(state, ino, false)?;
    Ok(())
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
    let group_count = group_desc.len();
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
        alloc_group_dirty: Spinlock::new(vec![0u8; group_count]),
        alloc_sb_dirty: AtomicBool::new(false),
        dirty_inodes: Spinlock::new(Vec::new()),
        read_only: core::sync::atomic::AtomicBool::new(false),
    });

    // 当前驱动不维护 ext4 orphan_file。可写挂载后立即规范化 superblock，
    // 同时清理旧镜像可能残留的 feature 位、s_last_orphan 和 s_orphan_file_inum。
    discard_orphan_file(&state).map_err(map_err)?;
    crate::alloc_mod::write_superblock(&state).map_err(map_err)?;

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

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::bgd::GroupDesc;
    use crate::layout::ExtKind;
    use crate::map_wr::{self, BlockAllocState};
    use crate::sb::Superblock;

    struct CountingBackend {
        data: Spinlock<Vec<u8>>,
        sector_size: u32,
        reads: Spinlock<Vec<(u64, u32)>>,
        writes: Spinlock<Vec<(u64, u32)>>,
    }

    impl CountingBackend {
        fn new(sector_count: u32, sector_size: u32) -> Self {
            Self {
                data: Spinlock::new(vec![0; sector_count as usize * sector_size as usize]),
                sector_size,
                reads: Spinlock::new(Vec::new()),
                writes: Spinlock::new(Vec::new()),
            }
        }

        fn seed_block(&self, block: u64, block_size: usize, data: &[u8]) {
            let start = block as usize * block_size;
            self.data.lock()[start..start + data.len()].copy_from_slice(data);
        }

        fn writes(&self) -> Vec<(u64, u32)> {
            self.writes.lock().clone()
        }

        fn reads(&self) -> Vec<(u64, u32)> {
            self.reads.lock().clone()
        }
    }

    impl BlockBackend for CountingBackend {
        fn sector_size(&self) -> u32 {
            self.sector_size
        }

        fn sector_count(&self) -> u64 {
            (self.data.lock().len() / self.sector_size as usize) as u64
        }

        fn read_sectors(
            &self,
            lba: u64,
            count: u32,
            buf: &mut [u8],
        ) -> Result<(), BlockBackendError> {
            let len = self.sector_size as usize * count as usize;
            if buf.len() < len {
                return Err(BlockBackendError::OutOfRange);
            }
            let start = lba as usize * self.sector_size as usize;
            let end = start
                .checked_add(len)
                .ok_or(BlockBackendError::OutOfRange)?;
            let data = self.data.lock();
            if end > data.len() {
                return Err(BlockBackendError::OutOfRange);
            }
            buf[..len].copy_from_slice(&data[start..end]);
            self.reads.lock().push((lba, count));
            Ok(())
        }

        fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError> {
            let len = self.sector_size as usize * count as usize;
            if buf.len() < len {
                return Err(BlockBackendError::OutOfRange);
            }
            let start = lba as usize * self.sector_size as usize;
            let end = start
                .checked_add(len)
                .ok_or(BlockBackendError::OutOfRange)?;
            let mut data = self.data.lock();
            if end > data.len() {
                return Err(BlockBackendError::OutOfRange);
            }
            data[start..end].copy_from_slice(&buf[..len]);
            self.writes.lock().push((lba, count));
            Ok(())
        }
    }

    fn alloc_test_state(backend: Arc<CountingBackend>) -> FsState {
        let block_size = 1024;
        let free_blocks = 60;
        let ext_sb = Superblock {
            kind: ExtKind::Ext2,
            inodes_count: 16,
            blocks_count: 64,
            first_data_block: 0,
            block_size,
            blocks_per_group: 64,
            inodes_per_group: 16,
            inode_size: 128,
            desc_size: 32,
            first_ino: 11,
            s_magic: 0xef53,
            feature_compat: 0,
            feature_incompat: 0,
            feature_ro_compat: 0,
            uuid: [0; 16],
            volume_name: [0; 16],
            metadata_csum: false,
            csum_seed: 0,
            free_blocks_count: free_blocks,
            free_inodes_count: 16,
            orphan_file_inum: 0,
            groups_count: 1,
        };
        let group_desc = vec![GroupDesc {
            block_bitmap: 1,
            inode_bitmap: 2,
            inode_table: 3,
            flags: 0,
            free_blocks_count: free_blocks as u32,
            free_inodes_count: 16,
            used_dirs_count: 1,
        }];
        let group_counts = vec![GroupCounts {
            free_blocks: free_blocks as u32,
            free_inodes: 16,
            used_dirs: 1,
        }];
        let backend: Arc<dyn BlockBackend> = backend;
        FsState {
            backend,
            ext_sb,
            group_desc: Spinlock::new(group_desc),
            group_counts: Spinlock::new(group_counts),
            block_cache: Spinlock::new(BlockCache::new(block_size)),
            block_cache_epoch: AtomicU64::new(0),
            sb_free_blocks: AtomicU64::new(free_blocks),
            sb_free_inodes: core::sync::atomic::AtomicU32::new(16),
            block_alloc_hint: AtomicU64::new(0),
            inode_alloc_hint: core::sync::atomic::AtomicU32::new(0),
            alloc_group_dirty: Spinlock::new(vec![0; 1]),
            alloc_sb_dirty: AtomicBool::new(false),
            dirty_inodes: Spinlock::new(Vec::new()),
            read_only: AtomicBool::new(false),
        }
    }

    #[test]
    fn ensure_block_for_write_skips_new_direct_zero_write() {
        let backend = Arc::new(CountingBackend::new(128, 512));
        let mut bitmap = vec![0u8; 1024];
        // 前 4 个块视为元数据，首个可分配数据块为物理块 4。
        bitmap[0] = 0b0000_1111;
        backend.seed_block(1, 1024, &bitmap);

        let state = alloc_test_state(Arc::clone(&backend));
        let mut i_block = [0u8; 60];

        let block = map_wr::ensure_block_for_write(&state, &mut i_block, 0, false)
            .expect("allocate direct block");

        assert_eq!(block, BlockAllocState::NewlyAllocated(4));
        assert_eq!(
            u32::from_le_bytes([i_block[0], i_block[1], i_block[2], i_block[3]]),
            4
        );
        // direct 新块由文件写路径覆盖/补零；alloc 路径只允许写分配元数据，
        // 不能提前写物理数据块 4，否则小块覆盖写会被大量无意义清零拖慢。
        let writes = backend.writes();
        assert!(writes.contains(&(2, 2)));
        assert!(!writes.contains(&(8, 2)));
    }

    #[test]
    fn read_blocks_single_block_uses_block_cache() {
        let backend = Arc::new(CountingBackend::new(128, 512));
        let mut data = vec![0u8; 1024];
        data[0] = 0xaa;
        backend.seed_block(8, 1024, &data);

        let state = alloc_test_state(Arc::clone(&backend));
        let mut first = vec![0u8; 1024];
        let mut second = vec![0u8; 1024];

        state.read_blocks(8, 1, &mut first).expect("first read");
        state.read_blocks(8, 1, &mut second).expect("cached read");

        assert_eq!(first, data);
        assert_eq!(second, data);
        assert_eq!(backend.reads(), vec![(16, 2)]);
    }

    #[test]
    fn flush_alloc_metadata_writes_only_dirty_group() {
        let backend = Arc::new(CountingBackend::new(256, 512));
        let state = alloc_test_state(Arc::clone(&backend));
        state.adjust_group_free_blocks(0, -1).expect("mark dirty");
        backend.writes.lock().clear();

        state.flush_alloc_metadata().expect("flush metadata");

        assert_eq!(backend.writes(), vec![(2, 2)]);
    }
}
