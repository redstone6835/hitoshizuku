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

use sched::mutex::Mutex;
use vfs::dentry::SmallStr;
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, PollEvents};
use vfs::stat::FileType;
use vfs::superblock::Superblock as VfsSuperblock;
use vfs::sync::Spinlock;

use crate::inode_wr::RawInode;
use crate::layout::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, EXT4_INLINE_DATA_FL,
};
use crate::map_wr::BlockAllocState;
use crate::state::{FsState, map_err};
use crate::{extent_wr, inode::lock_raw, map_wr};

const I_BLOCK_BYTES: usize = 60;
const MAP_CACHE_MAX_BLOCKS: u32 = 256 * 1024;
const READ_AHEAD_BLOCKS: u32 = 16;

// ── 目录 ────────────────────────────────────────────────────────────────

pub struct ExtDirFileOps {
    snapshot: Spinlock<Vec<DirEntry>>,
}

impl ExtDirFileOps {
    /// 生成打开目录时的快照:file_type 为 DT_UNKNOWN 时按目标 inode 的 i_mode 回填。
    pub(crate) fn new_with_state(
        state: &FsState,
        i_block: &[u8],
        flags: u32,
        size: u64,
    ) -> VfsResult<Self> {
        let mut snapshot = Vec::new();
        crate::dir::visit_entries(state, i_block, flags, size, |e| {
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
    map_cache: Spinlock<BlockMapCache>,
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
    epoch: u64,
    flags: u32,
    size: u64,
    i_block: [u8; I_BLOCK_BYTES],
    ranges: Vec<(u32, u32, u64)>,
}

impl Default for BlockMapCache {
    fn default() -> Self {
        Self {
            valid: false,
            epoch: 0,
            flags: 0,
            size: 0,
            i_block: [0u8; I_BLOCK_BYTES],
            ranges: Vec::new(),
        }
    }
}

impl BlockMapCache {
    fn matches(&self, epoch: u64, flags: u32, size: u64, i_block: &[u8; I_BLOCK_BYTES]) -> bool {
        self.valid
            && self.epoch == epoch
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
    ) -> Self {
        Self {
            state,
            sb,
            ino,
            raw,
            map_cache: Spinlock::new(BlockMapCache::default()),
            read_ahead: Spinlock::new(ReadAheadState::default()),
            io_mu: Mutex::new(()),
        }
    }

    pub(crate) fn new_empty(
        state: Arc<FsState>,
        sb: Arc<VfsSuperblock>,
        ino: u32,
        raw: Arc<Spinlock<RawInode>>,
    ) -> Self {
        Self::new(state, sb, ino, raw)
    }

    fn map_ranges(
        &self,
        flags: u32,
        size: u64,
        i_block: &[u8; I_BLOCK_BYTES],
        first_lb: u32,
        lb_count: u32,
    ) -> Result<Vec<(u32, u32, u64)>, crate::state::BlockBackendError> {
        if lb_count == 0 {
            return Ok(Vec::new());
        }

        let block_size = self.state.ext_sb.block_size as u64;
        let total_blocks_u64 = size.div_ceil(block_size);
        if total_blocks_u64 > u32::MAX as u64 {
            return Err(crate::state::BlockBackendError::OutOfRange);
        }
        let total_blocks = total_blocks_u64 as u32;
        let epoch = self.state.io_epoch();
        {
            let cache = self.map_cache.lock();
            if cache.matches(epoch, flags, size, i_block) {
                return Ok(clip_ranges(&cache.ranges, first_lb, lb_count));
            }
        }

        let map_all = total_blocks != 0 && total_blocks <= MAP_CACHE_MAX_BLOCKS;
        let (map_start, map_count) = if map_all {
            (0, total_blocks)
        } else {
            (first_lb, lb_count)
        };
        let mapped = if flags & crate::layout::EXT4_EXTENTS_FL != 0 {
            crate::extent::map_contiguous(&self.state, i_block, map_start, map_count)?
        } else {
            crate::map::map_contiguous(&self.state, i_block, map_start, map_count)?
        };

        if map_all && self.state.io_epoch() == epoch {
            let mut cache = self.map_cache.lock();
            cache.valid = true;
            cache.epoch = epoch;
            cache.flags = flags;
            cache.size = size;
            cache.i_block = *i_block;
            cache.ranges.clear();
            cache.ranges.extend_from_slice(&mapped);
        }

        Ok(if map_all {
            clip_ranges(&mapped, first_lb, lb_count)
        } else {
            mapped
        })
    }

    fn invalidate_map_cache(&self) {
        self.map_cache.lock().valid = false;
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
            self.state
                .read_data_blocks(
                    phys,
                    (aligned_bytes / block_size) as u32,
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
        let (flags, size, i_block) = {
            let g = lock_raw(&self.raw);
            let mut i_block = [0u8; I_BLOCK_BYTES];
            i_block.copy_from_slice(crate::inode::i_block_slice(&g.bytes));
            (g.flags(), g.size(), i_block)
        };
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
            .map_ranges(flags, size, &i_block, first_lb, map_lb_count)
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
                        let block = ensure_block_any(
                            &self.state,
                            &mut flags,
                            &mut i_block,
                            lb,
                            &mut map_scratch,
                        )
                        .map_err(map_err)?;
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
                    let block = ensure_block_any(
                        &self.state,
                        &mut flags,
                        &mut i_block,
                        lb,
                        &mut map_scratch,
                    )
                    .map_err(map_err)?;
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
            let mapping_changed = flags != old_flags || i_block != old_i_block;
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
                self.state.mark_inode_dirty(&self.raw);
                if let Some(inode) = self.sb.find_inode(self.ino as u64) {
                    inode.set_size_and_blocks(new_size, new_blocks_lo as u64);
                }
                self.invalidate_map_cache();
            }

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
) -> VfsResult<String> {
    crate::symlink::read_link(state, flags, size, i_block).map_err(map_err)
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
) -> Result<BlockAllocState, crate::state::BlockBackendError> {
    if *flags & crate::layout::EXT4_EXTENTS_FL != 0 {
        if let Some(block) = extent_wr::ensure_block_in_extent_for_write(state, i_block, lb)? {
            return Ok(block);
        }
        // 原地追加失败 → 保留已有数据地降级为间接块布局。
        if !extent_wr::demote_preserve_if_extent(state, flags, i_block)? {
            return Err(crate::state::BlockBackendError::Unsupported);
        }
    }
    // 文件写路径会自行处理新块未覆盖区域，避免整块覆盖时先写零再写数据。
    map_wr::ensure_block_for_write_with_scratch(state, i_block, lb, false, scratch)
}

#[inline]
fn zero_bytes(buf: &mut [u8]) {
    if !buf.is_empty() {
        unsafe {
            core::ptr::write_bytes(buf.as_mut_ptr(), 0, buf.len());
        }
    }
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

fn clip_ranges(ranges: &[(u32, u32, u64)], first_lb: u32, lb_count: u32) -> Vec<(u32, u32, u64)> {
    if lb_count == 0 {
        return Vec::new();
    }
    let end_lb = first_lb.saturating_add(lb_count);
    let start_idx = first_overlapping_range(ranges, first_lb);
    let mut out = Vec::with_capacity(ranges.len().saturating_sub(start_idx).min(4));
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
    use alloc::vec;

    use super::{clip_ranges, first_overlapping_range};

    #[test]
    fn clip_ranges_starts_inside_existing_range() {
        let ranges = [(0, 8, 100), (16, 4, 200), (24, 2, 300)];

        assert_eq!(first_overlapping_range(&ranges, 3), 0);
        assert_eq!(clip_ranges(&ranges, 3, 4), vec![(3, 4, 103)]);
    }

    #[test]
    fn clip_ranges_skips_before_window_and_stops_after() {
        let ranges = [(0, 2, 100), (4, 3, 200), (8, 4, 300), (20, 1, 400)];

        assert_eq!(first_overlapping_range(&ranges, 5), 1);
        assert_eq!(clip_ranges(&ranges, 5, 5), vec![(5, 2, 201), (8, 2, 300)]);
    }
}
