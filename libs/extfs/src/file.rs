//! VFS `FileOps` 实现:目录 readdir + 普通文件 read/write。
//!
//! `ExtRegFileOps` 只持 `Arc<Superblock>` + ino;每次 I/O 现查 Inode,读出
//! 当前的 `i_block`/size/flags。好处:写路径(`write_at` 要扩容)和只读
//! 路径共用同一套状态,且不用手动同步快照。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use sched::mutex::Mutex;
use smallvec::SmallVec;
use vfs::dentry::SmallStr;
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, PollEvents, read_pages_at_default};
use vfs::stat::{FileType, Timespec};
use vfs::superblock::Superblock as VfsSuperblock;
use vfs::sync::Spinlock;

use crate::inode_wr::RawInode;
use crate::layout::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, EXT4_ENCRYPT_FL, EXT4_INLINE_DATA_FL,
    EXT4_VERITY_FL,
};
use crate::map_wr::BlockAllocState;
use crate::state::{FsState, map_err};
use crate::{
    extent_wr,
    inode::{lock_raw, sync_vfs_meta, touch_content_times},
    map_wr,
};

const I_BLOCK_BYTES: usize = 60;
const MAP_CACHE_MAX_BLOCKS: u32 = 256 * 1024;
const READ_AHEAD_BLOCKS: u32 = 16;
const INLINE_BATCH_RANGES: usize = READ_AHEAD_BLOCKS as usize;

type BlockRanges = SmallVec<[(u32, u32, u64); INLINE_BATCH_RANGES]>;
type ScatterRanges<'a> = SmallVec<[&'a mut [u8]; INLINE_BATCH_RANGES]>;

// ── 目录 ────────────────────────────────────────────────────────────────

pub struct ExtDirFileOps {
    snapshot: Spinlock<Vec<DirEntry>>,
}

impl ExtDirFileOps {
    /// 生成打开目录时的快照:file_type 为 DT_UNKNOWN 时按目标 inode 的 i_mode 回填。
    /// `csum_ctx = Some((ino, generation))` 时对目录叶块做 METADATA_CSUM 校验。
    pub(crate) fn new_with_state(
        state: &FsState,
        i_block: &[u8],
        flags: u32,
        size: u64,
        csum_ctx: Option<(u32, u32)>,
    ) -> VfsResult<Self> {
        let mut snapshot = Vec::new();
        crate::dir::visit_entries(state, i_block, flags, size, csum_ctx, |e| {
            let kind = match e.file_type {
                DT_REG => FileType::Regular,
                DT_DIR => FileType::Directory,
                DT_LNK => FileType::Symlink,
                DT_CHR => FileType::CharDevice,
                DT_BLK => FileType::BlockDevice,
                DT_FIFO => FileType::Fifo,
                DT_SOCK => FileType::Socket,
                _ => {
                    // DT_UNKNOWN:读目标 inode 的 mode 判类型。
                    match crate::inode_wr::read_raw(state, e.ino) {
                        Ok(ri) => {
                            let mode = u16::from_le_bytes([ri.bytes[0], ri.bytes[1]]);
                            crate::inode::file_type_from_mode(mode)
                        }
                        Err(_) => FileType::Regular,
                    }
                }
            };
            snapshot.push(DirEntry {
                ino: e.ino as u64,
                name: SmallStr::new(&e.name),
                kind,
            });
            true
        })
        .map_err(map_err)?;
        Ok(Self {
            snapshot: Spinlock::new(snapshot),
        })
    }
}

impl FileOps for ExtDirFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        let snap = self.snapshot.lock();
        let mut idx = pos as usize;
        while idx < snap.len() {
            if sink(snap[idx].clone()).is_break() {
                return Ok(idx as u64);
            }
            idx += 1;
        }
        Ok(snap.len() as u64)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 目录枚举不会等待外部事件；readiness 表示可立即尝试 I/O。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── 普通文件 ────────────────────────────────────────────────────────────

pub struct ExtRegFileOps {
    state: Arc<FsState>,
    sb: Arc<VfsSuperblock>,
    ino: u32,
    /// 打开文件直接持有 inode raw 状态，保证 unlink 后仍可继续 I/O。
    ///
    /// VFS 在 nlink=0 后会把 inode 从 superblock cache 移除；如果这里每次
    /// I/O 都靠 ino 反查 cache，unlink-but-open 文件会立刻变成 ENOENT。
    raw: Arc<Spinlock<RawInode>>,
    /// 同一 inode 的所有打开句柄共享映射代际，避免间接块补洞后其它句柄
    /// 继续使用旧的 hole 结果。
    mapping_generation: Arc<AtomicU64>,
    map_cache: Spinlock<BlockMapCache>,
    #[cfg(test)]
    map_rebuilds: AtomicU64,
    read_ahead: Spinlock<ReadAheadState>,
    /// 串行化 append / 扩容。
    io_mu: Mutex<()>,
}

#[derive(Default)]
struct ReadAheadState {
    valid: bool,
    next_offset: u64,
}

struct BlockMapCache {
    valid: bool,
    generation: u64,
    flags: u32,
    size: u64,
    i_block: [u8; I_BLOCK_BYTES],
    ranges: Vec<(u32, u32, u64)>,
}

impl Default for BlockMapCache {
    fn default() -> Self {
        Self {
            valid: false,
            generation: 0,
            flags: 0,
            size: 0,
            i_block: [0u8; I_BLOCK_BYTES],
            ranges: Vec::new(),
        }
    }
}

impl BlockMapCache {
    fn matches(
        &self,
        generation: u64,
        flags: u32,
        size: u64,
        i_block: &[u8; I_BLOCK_BYTES],
    ) -> bool {
        self.valid
            && self.generation == generation
            && self.flags == flags
            && self.size == size
            && self.i_block == *i_block
    }
}

impl ExtRegFileOps {
    pub(crate) fn new(
        state: Arc<FsState>,
        sb: Arc<VfsSuperblock>,
        ino: u32,
        raw: Arc<Spinlock<RawInode>>,
        mapping_generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            state,
            sb,
            ino,
            raw,
            mapping_generation,
            map_cache: Spinlock::new(BlockMapCache::default()),
            #[cfg(test)]
            map_rebuilds: AtomicU64::new(0),
            read_ahead: Spinlock::new(ReadAheadState::default()),
            io_mu: Mutex::new(()),
        }
    }

    pub(crate) fn new_empty(
        state: Arc<FsState>,
        sb: Arc<VfsSuperblock>,
        ino: u32,
        raw: Arc<Spinlock<RawInode>>,
        mapping_generation: Arc<AtomicU64>,
    ) -> Self {
        Self::new(state, sb, ino, raw, mapping_generation)
    }

    /// extent 尾校验上下文:(ino, i_generation)。
    #[inline]
    fn csum_ctx(&self) -> Option<(u32, u32)> {
        let generation = lock_raw(&self.raw).generation();
        Some((self.ino, generation))
    }

    #[allow(clippy::too_many_arguments)]
    fn map_ranges(
        &self,
        flags: u32,
        size: u64,
        i_block: &[u8; I_BLOCK_BYTES],
        generation: u64,
        first_lb: u32,
        lb_count: u32,
        csum_ctx: Option<(u32, u32)>,
    ) -> Result<BlockRanges, crate::state::BlockBackendError> {
        if lb_count == 0 {
            return Ok(BlockRanges::new());
        }

        let block_size = self.state.ext_sb.block_size as u64;
        let total_blocks_u64 = size.div_ceil(block_size);
        if total_blocks_u64 > u32::MAX as u64 {
            return Err(crate::state::BlockBackendError::OutOfRange);
        }
        let total_blocks = total_blocks_u64 as u32;
        {
            let cache = self.map_cache.lock();
            if cache.matches(generation, flags, size, i_block) {
                return Ok(clip_ranges(&cache.ranges, first_lb, lb_count));
            }
        }

        let map_all = total_blocks != 0 && total_blocks <= MAP_CACHE_MAX_BLOCKS;
        #[cfg(test)]
        self.map_rebuilds.fetch_add(1, Ordering::Relaxed);
        let (map_start, map_count) = if map_all {
            (0, total_blocks)
        } else {
            (first_lb, lb_count)
        };
        let mapped = if flags & crate::layout::EXT4_EXTENTS_FL != 0 {
            crate::extent::map_contiguous(&self.state, i_block, map_start, map_count, csum_ctx)?
        } else {
            crate::map::map_contiguous(&self.state, i_block, map_start, map_count)?
        };

        if map_all && self.mapping_generation.load(Ordering::Acquire) == generation {
            let mut cache = self.map_cache.lock();
            cache.valid = true;
            cache.generation = generation;
            cache.flags = flags;
            cache.size = size;
            cache.i_block = *i_block;
            cache.ranges.clear();
            cache.ranges.extend_from_slice(&mapped);
        }

        Ok(if map_all {
            clip_ranges(&mapped, first_lb, lb_count)
        } else {
            BlockRanges::from_vec(mapped)
        })
    }

    fn invalidate_map_cache(&self) {
        self.map_cache.lock().valid = false;
    }

    #[cfg(test)]
    pub(crate) fn map_rebuilds(&self) -> u64 {
        self.map_rebuilds.load(Ordering::Relaxed)
    }

    fn read_mapped_bytes(
        &self,
        scratch: &mut Vec<u8>,
        range_byte_start: u64,
        range_byte_end: u64,
        phys_start: u64,
        read_start: u64,
        read_end: u64,
        request_offset: u64,
        allow_readahead: bool,
        dst: &mut [u8],
    ) -> VfsResult<()> {
        let block_size = self.state.ext_sb.block_size as u64;
        let mut cur = read_start;

        if cur % block_size != 0 {
            let block_off = (cur % block_size) as usize;
            let take = ((block_size as usize - block_off) as u64).min(read_end - cur) as usize;
            let phys = phys_start + (cur - range_byte_start) / block_size;
            let readahead_blocks =
                readahead_blocks_for(block_size, range_byte_end, cur, allow_readahead);
            read_partial_block(
                &self.state,
                scratch,
                phys,
                block_off,
                take,
                cur,
                request_offset,
                readahead_blocks,
                dst,
            )
            .map_err(map_err)?;
            cur += take as u64;
        }

        let aligned_bytes = ((read_end - cur) / block_size) * block_size;
        if aligned_bytes != 0 {
            let phys = phys_start + (cur - range_byte_start) / block_size;
            let dst_pos = (cur - request_offset) as usize;
            let requested_blocks = (aligned_bytes / block_size) as u32;
            let readahead_blocks =
                readahead_blocks_for(block_size, range_byte_end, cur, allow_readahead);
            read_aligned_blocks(
                &self.state,
                scratch,
                phys,
                requested_blocks,
                readahead_blocks,
                &mut dst[dst_pos..dst_pos + aligned_bytes as usize],
            )
            .map_err(map_err)?;
            cur += aligned_bytes;
        }

        if cur < read_end {
            let take = (read_end - cur) as usize;
            let phys = phys_start + (cur - range_byte_start) / block_size;
            let readahead_blocks =
                readahead_blocks_for(block_size, range_byte_end, cur, allow_readahead);
            read_partial_block(
                &self.state,
                scratch,
                phys,
                0,
                take,
                cur,
                request_offset,
                readahead_blocks,
                dst,
            )
            .map_err(map_err)?;
        }

        Ok(())
    }
}

impl FileOps for ExtRegFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let (flags, size, i_block, mapping_generation) = {
            let g = lock_raw(&self.raw);
            let mut i_block = [0u8; I_BLOCK_BYTES];
            i_block.copy_from_slice(crate::inode::i_block_slice(&g.bytes));
            (
                g.flags(),
                g.size(),
                i_block,
                self.mapping_generation.load(Ordering::Acquire),
            )
        };
        // fscrypt:无密钥时内容不可读(与 Linux 无密钥访问一致,报 EOPNOTSUPP)。
        if flags & EXT4_ENCRYPT_FL != 0 {
            return Err(VfsError::NotSupported);
        }
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let remaining = (size - offset).min(buf.len() as u64) as usize;

        // inline_data 路径
        if flags & EXT4_INLINE_DATA_FL != 0 {
            let raw_bytes = lock_raw(&self.raw).bytes.clone();
            if let Some(inline) =
                crate::inode::try_inline_data(&self.state, &raw_bytes, size, flags)
            {
                let start = offset as usize;
                let end = (start + remaining).min(inline.len());
                let take = end.saturating_sub(start);
                buf[..take].copy_from_slice(&inline[start..end]);
                return Ok(take);
            }
        }

        let block_size = self.state.ext_sb.block_size as u64;

        let first_lb = (offset / block_size) as u32;
        let last_lb = ((offset + remaining as u64 - 1) / block_size) as u32;
        let lb_count = last_lb - first_lb + 1;
        let allow_readahead = {
            let mut state = self.read_ahead.lock();
            let allow = offset == 0 || state.valid && offset == state.next_offset;
            state.valid = true;
            state.next_offset = offset.saturating_add(remaining as u64);
            allow
        };
        let map_lb_count = if allow_readahead {
            let total_blocks = size.div_ceil(block_size).min(u32::MAX as u64) as u32;
            total_blocks
                .saturating_sub(first_lb)
                .min(lb_count.max(READ_AHEAD_BLOCKS))
                .max(lb_count)
        } else {
            lb_count
        };

        let ranges = self
            .map_ranges(
                flags,
                size,
                &i_block,
                mapping_generation,
                first_lb,
                map_lb_count,
                self.csum_ctx(),
            )
            .map_err(map_err)?;

        let mut filled_until = 0usize;
        let mut scratch = Vec::new();
        for (range_lb, range_count, phys_start) in &ranges {
            let range_byte_start = *range_lb as u64 * block_size;
            let range_byte_end = range_byte_start + *range_count as u64 * block_size;
            let read_start = offset.max(range_byte_start);
            let read_end = (offset + remaining as u64).min(range_byte_end);
            if read_start >= read_end {
                continue;
            }
            let overlap_bytes = (read_end - read_start) as usize;
            let buf_pos = (read_start - offset) as usize;

            if filled_until < buf_pos {
                zero_bytes(&mut buf[filled_until..buf_pos]);
            }

            self.read_mapped_bytes(
                &mut scratch,
                range_byte_start,
                range_byte_end,
                *phys_start,
                read_start,
                read_end,
                offset,
                allow_readahead,
                buf,
            )?;
            filled_until = filled_until.max(buf_pos + overlap_bytes);
        }
        if filled_until < remaining {
            zero_bytes(&mut buf[filled_until..remaining]);
        }
        Ok(remaining)
    }

    fn read_pages_at(
        &self,
        offset: u64,
        pages: &mut [&mut [u8]],
        valid_len: usize,
    ) -> VfsResult<()> {
        let (capacity, request_end) = validate_page_request(pages, offset, valid_len)?;
        if valid_len == 0 {
            return zero_scatter_range(pages, 0, capacity);
        }

        let (flags, size, i_block, mapping_generation) = {
            let g = lock_raw(&self.raw);
            let mut i_block = [0u8; I_BLOCK_BYTES];
            i_block.copy_from_slice(crate::inode::i_block_slice(&g.bytes));
            (
                g.flags(),
                g.size(),
                i_block,
                self.mapping_generation.load(Ordering::Acquire),
            )
        };
        if request_end > size {
            return Err(VfsError::Io);
        }

        let block_size = self.state.ext_sb.block_size as usize;
        let aligned_layout = block_size != 0
            && offset % block_size as u64 == 0
            && pages.iter().all(|page| page.len() % block_size == 0);
        if flags & EXT4_INLINE_DATA_FL != 0 || !aligned_layout {
            return read_pages_at_default(self, offset, pages, valid_len);
        }

        zero_scatter_range(pages, valid_len, capacity - valid_len)?;

        let first_lb =
            u32::try_from(offset / block_size as u64).map_err(|_| VfsError::InvalidArgument)?;
        let requested_blocks =
            u32::try_from(valid_len.div_ceil(block_size)).map_err(|_| VfsError::InvalidArgument)?;
        let ranges = self
            .map_ranges(
                flags,
                size,
                &i_block,
                mapping_generation,
                first_lb,
                requested_blocks,
                self.csum_ctx(),
            )
            .map_err(map_err)?;

        let full_len = valid_len / block_size * block_size;
        let full_end = offset + full_len as u64;
        let mut filled_until = 0usize;
        for &(range_lb, range_count, phys_start) in &ranges {
            let range_start = range_lb as u64 * block_size as u64;
            let range_end = range_start + range_count as u64 * block_size as u64;
            let read_start = offset.max(range_start);
            let read_end = full_end.min(range_end);
            if read_start >= read_end {
                continue;
            }

            let dst_start = (read_start - offset) as usize;
            if filled_until < dst_start {
                zero_scatter_range(pages, filled_until, dst_start - filled_until)?;
            }
            let read_len = (read_end - read_start) as usize;
            let physical = phys_start + (read_start - range_start) / block_size as u64;
            let block_count =
                u32::try_from(read_len / block_size).map_err(|_| VfsError::InvalidArgument)?;
            let mut targets =
                scatter_range_mut(pages, dst_start, read_len).ok_or(VfsError::InvalidArgument)?;
            self.state
                .read_data_blocks_vectored(physical, block_count, &mut targets)
                .map_err(map_err)?;
            filled_until = filled_until.max(dst_start + read_len);
        }
        if filled_until < full_len {
            zero_scatter_range(pages, filled_until, full_len - filled_until)?;
        }

        let partial_len = valid_len - full_len;
        if partial_len != 0 {
            let partial_lb = first_lb
                .checked_add((full_len / block_size) as u32)
                .ok_or(VfsError::InvalidArgument)?;
            let physical = ranges
                .iter()
                .find_map(|&(range_lb, range_count, phys_start)| {
                    let range_end = range_lb.saturating_add(range_count);
                    (partial_lb >= range_lb && partial_lb < range_end)
                        .then_some(phys_start + (partial_lb - range_lb) as u64)
                });
            if let Some(physical) = physical {
                let mut block = vec![0u8; block_size];
                self.state
                    .read_block(physical, &mut block)
                    .map_err(map_err)?;
                copy_to_scatter(pages, full_len, &block[..partial_len])?;
            } else {
                zero_scatter_range(pages, full_len, partial_len)?;
            }
        }

        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let _io = self.io_mu.lock();
        {
            let mut raw_guard = lock_raw(&self.raw);
            {
                let flags = raw_guard.flags();
                // fscrypt:无密钥不可写;fs-verity:已启用校验的文件不可变(EROFS)。
                if flags & EXT4_ENCRYPT_FL != 0 {
                    return Err(VfsError::NotSupported);
                }
                if flags & EXT4_VERITY_FL != 0 {
                    return Err(VfsError::ReadOnlyFilesystem);
                }
            }
            let old_size = raw_guard.size();
            let start = if offset == u64::MAX { old_size } else { offset };
            let end = start
                .checked_add(buf.len() as u64)
                .ok_or(VfsError::FileTooLarge)?;

            let old_flags = raw_guard.flags();
            let old_blocks_lo = raw_guard.blocks_lo();
            let mut flags = old_flags;
            let mut i_block = [0u8; I_BLOCK_BYTES];
            i_block.copy_from_slice(raw_guard.i_block());
            let old_i_block = i_block;

            // inline_data 无损迁移:先把现有 inline 字节读出来,再清 flag,
            // 之后把旧内容作为普通数据块写回,最后执行用户写入。
            let inline_recovered: Option<Vec<u8>> = if flags & EXT4_INLINE_DATA_FL != 0 {
                let raw_bytes = raw_guard.bytes.clone();
                let recovered =
                    crate::inode::try_inline_data(&self.state, &raw_bytes, raw_guard.size(), flags);
                // 清 flag + 把 i_block 置零(供间接块布局使用)
                flags &= !EXT4_INLINE_DATA_FL;
                for b in raw_guard.i_block_mut().iter_mut() {
                    *b = 0;
                }
                i_block.copy_from_slice(raw_guard.i_block());
                raw_guard.set_size(0);
                recovered
            } else {
                None
            };
            // 不强行 demote — 如果是 extent 文件且能原地 append 就留着;
            // 真正无法容纳时再退化成间接布局。
            let block_size = self.state.ext_sb.block_size as u64;
            let mut map_scratch = Vec::new();
            let mut newly_allocated_blocks: u64 = 0;

            // 把旧 inline 内容(如果有)写回为数据块
            if let Some(old) = inline_recovered {
                if !old.is_empty() {
                    let mut written = 0usize;
                    let total = old.len();
                    while written < total {
                        let lb = (written as u64 / block_size) as u32;
                        let in_block = (written as u64 % block_size) as usize;
                        let want = ((block_size - in_block as u64) as usize).min(total - written);
                        let (block, new_metadata) = ensure_block_any(
                            &self.state,
                            &mut flags,
                            &mut i_block,
                            lb,
                            &mut map_scratch,
                        )
                        .map_err(map_err)?;
                        newly_allocated_blocks += new_metadata as u64;
                        if block.is_new() {
                            newly_allocated_blocks += 1;
                        }
                        let phys = block.phys();
                        let mut blk = vec![0u8; block_size as usize];
                        if !block.is_new() && (in_block != 0 || want < block_size as usize) {
                            self.state.read_block(phys, &mut blk).map_err(map_err)?;
                        }
                        blk[in_block..in_block + want]
                            .copy_from_slice(&old[written..written + want]);
                        self.state.write_data_block(phys, &blk).map_err(map_err)?;
                        written += want;
                    }
                    raw_guard.set_size(total as u64);
                }
            }

            // 执行用户数据块写入:先把覆盖/新增的块逐个落盘
            let first_lb = start / block_size;
            let last_lb = (end - 1) / block_size;
            let mut written = 0usize;
            let mut file_off = start;

            if flags & crate::layout::EXT4_EXTENTS_FL != 0
                && start.is_multiple_of(block_size)
                && buf.len().is_multiple_of(block_size as usize)
            {
                while written < buf.len() {
                    let lb = (file_off / block_size) as u32;
                    let remain_blocks = ((buf.len() - written) as u64 / block_size) as u32;
                    // Check if blocks exist before calling ensure_extent_run
                    let existing_run =
                        extent_wr::lookup_extent_run_pub(&i_block, lb, remain_blocks);
                    let Some((phys, run_blocks)) =
                        extent_wr::ensure_extent_run(&self.state, &mut i_block, lb, remain_blocks)
                            .map_err(map_err)?
                    else {
                        break;
                    };
                    if existing_run.is_none() {
                        newly_allocated_blocks += run_blocks as u64;
                    }
                    let bytes = run_blocks as usize * block_size as usize;
                    self.state
                        .write_data_blocks(phys, run_blocks, &buf[written..written + bytes])
                        .map_err(map_err)?;
                    written += bytes;
                    file_off += bytes as u64;
                }
            }

            if written != buf.len() {
                written = 0;
                file_off = start;
                let mut cur_blk_buf = vec![0u8; block_size as usize];
                let mut cur_lb: Option<u32> = None;
                let mut cur_phys: u64 = 0;
                for lb in first_lb..=last_lb {
                    let lb = lb as u32;
                    let in_block = (file_off % block_size) as usize;
                    let want = ((block_size - in_block as u64) as usize).min(buf.len() - written);
                    let (block, new_metadata) = ensure_block_any(
                        &self.state,
                        &mut flags,
                        &mut i_block,
                        lb,
                        &mut map_scratch,
                    )
                    .map_err(map_err)?;
                    newly_allocated_blocks += new_metadata as u64;
                    if block.is_new() {
                        newly_allocated_blocks += 1;
                    }
                    let phys = block.phys();

                    // 快速路径：块已在 cache 中且非新块，直接原地 partial write，
                    // 单次加锁替代 read_block + write_block 两次加锁。
                    if !block.is_new()
                        && cur_lb != Some(lb)
                        && self.state.modify_block_partial(
                            phys,
                            in_block,
                            &buf[written..written + want],
                        )
                    {
                        // 前一个块如果在暂存区还未写回，先 flush
                        if let Some(prev_lb) = cur_lb {
                            if prev_lb != lb {
                                self.state
                                    .write_data_block(cur_phys, &cur_blk_buf)
                                    .map_err(map_err)?;
                                cur_lb = None;
                            }
                        }
                        written += want;
                        file_off += want as u64;
                        continue;
                    }

                    // 慢速路径：块不在 cache 或新块，走传统 read-modify-write
                    if let Some(prev_lb) = cur_lb {
                        if prev_lb != lb {
                            self.state
                                .write_data_block(cur_phys, &cur_blk_buf)
                                .map_err(map_err)?;
                        }
                    }
                    if cur_lb != Some(lb) {
                        if block.is_new() {
                            cur_blk_buf.fill(0);
                        } else if in_block != 0 || want < block_size as usize {
                            self.state
                                .read_block(phys, &mut cur_blk_buf)
                                .map_err(map_err)?;
                        }
                        cur_lb = Some(lb);
                        cur_phys = phys;
                    }
                    cur_blk_buf[in_block..in_block + want]
                        .copy_from_slice(&buf[written..written + want]);
                    written += want;
                    file_off += want as u64;
                }
                if cur_lb.is_some() {
                    self.state
                        .write_data_block(cur_phys, &cur_blk_buf)
                        .map_err(map_err)?;
                }
            }

            let new_size = raw_guard.size().max(end);
            let mapping_changed =
                flags != old_flags || i_block != old_i_block || newly_allocated_blocks != 0;
            let size_changed = new_size != old_size;
            let new_blocks_lo = if newly_allocated_blocks > 0 {
                let sectors_per_block = (block_size / 512) as u64;
                let added_sectors = newly_allocated_blocks * sectors_per_block;
                (old_blocks_lo as u64 + added_sectors).min(u32::MAX as u64) as u32
            } else {
                old_blocks_lo
            };
            let metadata_changed =
                size_changed || new_blocks_lo != old_blocks_lo || mapping_changed;
            if metadata_changed {
                raw_guard.i_block_mut().copy_from_slice(&i_block);
                raw_guard.set_flags(flags);
                raw_guard.set_size(new_size);
                raw_guard.set_blocks_lo(new_blocks_lo);
                self.invalidate_map_cache();
            }

            if mapping_changed {
                // 必须在 raw 锁内、最新 i_block/间接块映射发布之后推进代际。
                // 这样其它句柄在 raw 锁下取得的快照能与代际属于同一版本。
                self.mapping_generation.fetch_add(1, Ordering::AcqRel);
            }

            // 非空写即使没有改变尺寸或块映射，也必须更新 mtime/ctime。
            touch_content_times(&mut raw_guard, Timespec::now());
            // 先在 raw 锁下发布最新快照，保证并发写入按元数据修改顺序进入
            // 版本化 writeback；真正的 block I/O 在释放 raw Spinlock 后进行。
            self.state.stage_inode_write(&raw_guard);
            if let Some(inode) = self.sb.find_inode(self.ino as u64) {
                sync_vfs_meta(&self.state, &inode, &raw_guard);
            }

            drop(raw_guard);
            // 若旧 owner 正在写同一 inode，本调用会等待或接管 pending；返回成功前，
            // 最新尺寸、块映射和时间戳都已进入共享 block cache。
            self.state.flush_inode_write(self.ino).map_err(map_err)?;

            Ok(written)
        }
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        let _io = self.io_mu.lock();
        self.state.sync_all().map_err(map_err)?;
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 普通文件不会阻塞等待设备事件；读写 readiness 应立即满足。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn release(&self) {
        let _ = self.ino;
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── Symlink 目标读取(供 InodeOps::readlink 使用) ──────────────────────

pub(crate) fn symlink_target(
    state: &FsState,
    flags: u32,
    size: u64,
    i_block: &[u8],
    csum_ctx: Option<(u32, u32)>,
) -> VfsResult<String> {
    crate::symlink::read_link(state, flags, size, i_block, csum_ctx).map_err(map_err)
}

/// 统一入口:根据 flags 选择 extent 原地追加或 indirect 分配。
///
/// - 若文件使用 extent 且根节点能容纳下新叶子,原地追加并返回物理块状态;
/// - 若 extent 根已满或为索引节点,fallback 到 `demote_if_extent` + indirect;
/// - 若文件本就是 indirect,直接走间接块写路径。
fn ensure_block_any(
    state: &FsState,
    flags: &mut u32,
    i_block: &mut [u8],
    lb: u32,
    scratch: &mut Vec<u8>,
) -> Result<(BlockAllocState, u32), crate::state::BlockBackendError> {
    let mut new_metadata = 0u32;
    if *flags & crate::layout::EXT4_EXTENTS_FL != 0 {
        if let Some(block) = extent_wr::ensure_block_in_extent_for_write(state, i_block, lb)? {
            return Ok((block, 0));
        }
        // 原地追加失败 → 保留已有数据地降级为间接块布局。
        let (converted, demoted_metadata) =
            extent_wr::demote_preserve_if_extent_count(state, flags, i_block)?;
        if !converted {
            return Err(crate::state::BlockBackendError::Unsupported);
        }
        new_metadata = demoted_metadata;
    }
    // 文件写路径会自行处理新块未覆盖区域，避免整块覆盖时先写零再写数据。
    let (block, mapped_metadata) =
        map_wr::ensure_block_for_write_with_scratch_count(state, i_block, lb, false, scratch)?;
    new_metadata = new_metadata
        .checked_add(mapped_metadata)
        .ok_or(crate::state::BlockBackendError::OutOfRange)?;
    Ok((block, new_metadata))
}

#[inline]
fn zero_bytes(buf: &mut [u8]) {
    if !buf.is_empty() {
        // Safety: buf 是当前调用独占的有效切片，write_bytes 长度严格等于切片长度。
        unsafe {
            core::ptr::write_bytes(buf.as_mut_ptr(), 0, buf.len());
        }
    }
}

fn validate_page_request(
    pages: &[&mut [u8]],
    offset: u64,
    valid_len: usize,
) -> VfsResult<(usize, u64)> {
    let mut capacity = 0usize;
    for page in pages {
        if page.is_empty() {
            return Err(VfsError::InvalidArgument);
        }
        capacity = capacity
            .checked_add(page.len())
            .ok_or(VfsError::FileTooLarge)?;
    }
    if valid_len > capacity {
        return Err(VfsError::InvalidArgument);
    }
    let valid_len_u64 = u64::try_from(valid_len).map_err(|_| VfsError::FileTooLarge)?;
    let end = offset
        .checked_add(valid_len_u64)
        .ok_or(VfsError::FileTooLarge)?;
    Ok((capacity, end))
}

fn scatter_range_mut<'a, 'page>(
    pages: &'a mut [&'page mut [u8]],
    start: usize,
    len: usize,
) -> Option<ScatterRanges<'a>>
where
    'page: 'a,
{
    if len == 0 {
        return Some(ScatterRanges::new());
    }
    let end = start.checked_add(len)?;
    let mut page_start = 0usize;
    let mut covered = 0usize;
    let mut out = ScatterRanges::new();
    for page in pages.iter_mut() {
        let page_end = page_start.checked_add(page.len())?;
        let overlap_start = start.max(page_start);
        let overlap_end = end.min(page_end);
        if overlap_start < overlap_end {
            let local_start = overlap_start - page_start;
            let local_end = overlap_end - page_start;
            covered = covered.checked_add(local_end - local_start)?;
            out.push(&mut page[local_start..local_end]);
        }
        page_start = page_end;
        if page_start >= end {
            break;
        }
    }
    (covered == len).then_some(out)
}

fn zero_scatter_range(pages: &mut [&mut [u8]], start: usize, len: usize) -> VfsResult<()> {
    let targets = scatter_range_mut(pages, start, len).ok_or(VfsError::InvalidArgument)?;
    for target in targets {
        target.fill(0);
    }
    Ok(())
}

fn copy_to_scatter(pages: &mut [&mut [u8]], start: usize, src: &[u8]) -> VfsResult<()> {
    let targets = scatter_range_mut(pages, start, src.len()).ok_or(VfsError::InvalidArgument)?;
    let mut copied = 0usize;
    for target in targets {
        let end = copied + target.len();
        target.copy_from_slice(&src[copied..end]);
        copied = end;
    }
    Ok(())
}

fn read_partial_block(
    state: &FsState,
    scratch: &mut Vec<u8>,
    phys: u64,
    block_off: usize,
    take: usize,
    cur: u64,
    request_offset: u64,
    readahead_blocks: u32,
    dst: &mut [u8],
) -> Result<(), crate::state::BlockBackendError> {
    let dst_pos = (cur - request_offset) as usize;
    // 快速路径：cache 命中时直接拷贝部分字节，无需读出整块
    {
        let mut cache = state.block_cache.lock();
        if cache.read_partial(phys, block_off, &mut dst[dst_pos..dst_pos + take]) {
            return Ok(());
        }
    }
    // 慢速路径：cache miss。顺序小读一次带上后续连续块，摊薄 virtio-mmio 门铃成本。
    let block_size = state.ext_sb.block_size as usize;
    let blocks = readahead_blocks.max(1) as usize;
    let bytes = block_size * blocks;
    if scratch.len() != bytes {
        scratch.resize(bytes, 0);
    }
    state.read_data_blocks(phys, blocks as u32, scratch)?;
    dst[dst_pos..dst_pos + take].copy_from_slice(&scratch[block_off..block_off + take]);
    Ok(())
}

pub(crate) fn read_aligned_blocks(
    state: &FsState,
    scratch: &mut Vec<u8>,
    phys: u64,
    requested_blocks: u32,
    readahead_blocks: u32,
    dst: &mut [u8],
) -> Result<(), crate::state::BlockBackendError> {
    let block_size = state.ext_sb.block_size as usize;
    let requested_bytes = block_size * requested_blocks as usize;
    if dst.len() != requested_bytes {
        return Err(crate::state::BlockBackendError::OutOfRange);
    }

    let blocks = requested_blocks.max(readahead_blocks.max(1));
    // 随机访问或大块请求没有额外窗口，沿用原批量读路径并直接写入目标缓冲区。
    if blocks == requested_blocks {
        return state.read_data_blocks(phys, requested_blocks, dst);
    }

    // 预读块已在 cache 时只拷贝调用方请求的前缀，避免为补下一窗口重复发 I/O。
    {
        let mut cache = state.block_cache.lock();
        let mut all_cached = true;
        for i in 0..requested_blocks {
            let start = block_size * i as usize;
            let end = start + block_size;
            if !cache.read_partial(phys + i as u64, 0, &mut dst[start..end]) {
                all_cached = false;
                break;
            }
        }
        if all_cached {
            return Ok(());
        }
    }

    // 顺序读把后端请求扩大到同一连续映射内最多 16 块；随机读的窗口仍为请求本身。
    let bytes = block_size * blocks as usize;
    if scratch.len() != bytes {
        scratch.resize(bytes, 0);
    }
    state.read_data_blocks(phys, blocks, scratch)?;
    dst.copy_from_slice(&scratch[..requested_bytes]);
    Ok(())
}

fn readahead_blocks_for(
    block_size: u64,
    range_byte_end: u64,
    cur: u64,
    allow_readahead: bool,
) -> u32 {
    if !allow_readahead || block_size == 0 {
        return 1;
    }
    let current_block_start = cur / block_size * block_size;
    let bytes_left = range_byte_end.saturating_sub(current_block_start);
    let blocks_left = bytes_left.div_ceil(block_size).min(u32::MAX as u64) as u32;
    blocks_left.clamp(1, READ_AHEAD_BLOCKS)
}

fn clip_ranges(ranges: &[(u32, u32, u64)], first_lb: u32, lb_count: u32) -> BlockRanges {
    if lb_count == 0 {
        return BlockRanges::new();
    }
    let end_lb = first_lb.saturating_add(lb_count);
    let start_idx = first_overlapping_range(ranges, first_lb);
    let mut out = BlockRanges::new();
    for &(range_lb, range_count, phys_start) in &ranges[start_idx..] {
        if range_lb >= end_lb {
            break;
        }
        let range_end = range_lb.saturating_add(range_count);
        let overlap_start = range_lb.max(first_lb);
        let overlap_end = range_end.min(end_lb);
        if overlap_start >= overlap_end {
            continue;
        }
        out.push((
            overlap_start,
            overlap_end - overlap_start,
            phys_start + (overlap_start - range_lb) as u64,
        ));
    }
    out
}

#[inline]
fn first_overlapping_range(ranges: &[(u32, u32, u64)], first_lb: u32) -> usize {
    let mut lo = 0usize;
    let mut hi = ranges.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (range_lb, range_count, _) = ranges[mid];
        if range_lb.saturating_add(range_count) <= first_lb {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::{
        INLINE_BATCH_RANGES, ScatterRanges, clip_ranges, first_overlapping_range,
        readahead_blocks_for, scatter_range_mut,
    };

    #[test]
    fn clip_ranges_starts_inside_existing_range() {
        let ranges = [(0, 8, 100), (16, 4, 200), (24, 2, 300)];

        assert_eq!(first_overlapping_range(&ranges, 3), 0);
        assert_eq!(clip_ranges(&ranges, 3, 4).as_slice(), &[(3, 4, 103)]);
    }

    #[test]
    fn clip_ranges_skips_before_window_and_stops_after() {
        let ranges = [(0, 2, 100), (4, 3, 200), (8, 4, 300), (20, 1, 400)];

        assert_eq!(first_overlapping_range(&ranges, 5), 1);
        assert_eq!(
            clip_ranges(&ranges, 5, 5).as_slice(),
            &[(5, 2, 201), (8, 2, 300)]
        );
    }

    #[test]
    fn batch_ranges_stay_inline_through_sixteen_items() {
        let ranges: [(u32, u32, u64); INLINE_BATCH_RANGES] =
            core::array::from_fn(|index| (index as u32, 1, 100 + index as u64));
        let clipped = clip_ranges(&ranges, 0, INLINE_BATCH_RANGES as u32);
        assert_eq!(clipped.len(), INLINE_BATCH_RANGES);
        assert!(!clipped.spilled());
        let empty_clipped = clip_ranges(&ranges, 0, 0);
        assert!(empty_clipped.is_empty());
        assert!(!empty_clipped.spilled());

        let mut storage = [[0u8; 1]; INLINE_BATCH_RANGES];
        let mut pages: ScatterRanges<'_> = storage.iter_mut().map(|page| &mut page[..]).collect();
        assert!(!pages.spilled());
        let scattered = scatter_range_mut(&mut pages, 0, INLINE_BATCH_RANGES).unwrap();
        assert_eq!(scattered.len(), INLINE_BATCH_RANGES);
        assert!(!scattered.spilled());
        drop(scattered);
        let empty_scattered = scatter_range_mut(&mut pages, usize::MAX, 0).unwrap();
        assert!(empty_scattered.is_empty());
        assert!(!empty_scattered.spilled());
    }

    #[test]
    fn readahead_window_respects_mapping_end_and_random_access() {
        let block_size = 4096;

        assert_eq!(readahead_blocks_for(block_size, 4 * block_size, 0, true), 4);
        assert_eq!(
            readahead_blocks_for(block_size, 64 * block_size, 0, false),
            1
        );
    }
}
