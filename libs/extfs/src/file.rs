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

use vfs::dentry::SmallStr;
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, PollEvents};
use vfs::stat::FileType;
use vfs::superblock::Superblock as VfsSuperblock;
use vfs::sync::Spinlock;

use crate::dir::DirEntryRaw;
use crate::inode::ExtInodeOps;
use crate::inode_wr::write_raw;
use crate::layout::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, EXT4_INLINE_DATA_FL,
};
use crate::state::{FsState, map_err};
use crate::{extent_wr, map_wr};

const I_BLOCK_BYTES: usize = 60;
const MAP_CACHE_MAX_BLOCKS: u32 = 256 * 1024;

// ── 目录 ────────────────────────────────────────────────────────────────

pub struct ExtDirFileOps {
    snapshot: Spinlock<Vec<DirEntry>>,
}

impl ExtDirFileOps {
    /// 带 FsState 的构造:file_type 为 DT_UNKNOWN 时,按目标 inode 的 i_mode
    /// 回填 kind。用于 ext2 无 INCOMPAT_FILETYPE 的卷。
    pub(crate) fn new_with_state(entries: Vec<DirEntryRaw>, state: &FsState) -> Self {
        let snapshot = entries
            .into_iter()
            .map(|e| {
                let kind = match e.file_type {
                    DT_REG => FileType::Regular,
                    DT_DIR => FileType::Directory,
                    DT_LNK => FileType::Symlink,
                    DT_CHR => FileType::CharDevice,
                    DT_BLK => FileType::BlockDevice,
                    DT_FIFO => FileType::Fifo,
                    DT_SOCK => FileType::Socket,
                    _ => {
                        // DT_UNKNOWN:读目标 inode 的 mode 判类型
                        match crate::inode_wr::read_raw(state, e.ino) {
                            Ok(ri) => {
                                let mode = u16::from_le_bytes([ri.bytes[0], ri.bytes[1]]);
                                crate::inode::file_type_from_mode(mode)
                            }
                            Err(_) => FileType::Regular,
                        }
                    }
                };
                DirEntry {
                    ino: e.ino as u64,
                    name: SmallStr::new(&e.name),
                    kind,
                }
            })
            .collect();
        Self {
            snapshot: Spinlock::new(snapshot),
        }
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
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
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
    map_cache: Spinlock<BlockMapCache>,
    /// 串行化 append / 扩容。
    io_mu: Spinlock<()>,
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
    pub(crate) fn new(state: Arc<FsState>, sb: Arc<VfsSuperblock>, ino: u32) -> Self {
        Self {
            state,
            sb,
            ino,
            map_cache: Spinlock::new(BlockMapCache::default()),
            io_mu: Spinlock::new(()),
        }
    }

    pub(crate) fn new_empty(state: Arc<FsState>, sb: Arc<VfsSuperblock>, ino: u32) -> Self {
        Self::new(state, sb, ino)
    }

    /// 找回 Inode 与 ExtInodeOps(每次 I/O 都现取,状态是最新的)。
    fn with_ops<R>(
        &self,
        f: impl FnOnce(&vfs::inode::Inode, &ExtInodeOps) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let inode = self
            .sb
            .find_inode(self.ino as u64)
            .ok_or(VfsError::NotFound)?;
        let ops = inode
            .downcast_ops::<ExtInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        f(&inode, ops)
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
        phys_start: u64,
        read_start: u64,
        read_end: u64,
        request_offset: u64,
        dst: &mut [u8],
    ) -> VfsResult<()> {
        let block_size = self.state.ext_sb.block_size as u64;
        let mut cur = read_start;

        if cur % block_size != 0 {
            let block_off = (cur % block_size) as usize;
            let take = ((block_size as usize - block_off) as u64).min(read_end - cur) as usize;
            let phys = phys_start + (cur - range_byte_start) / block_size;
            read_partial_block(
                &self.state,
                scratch,
                phys,
                block_off,
                take,
                cur,
                request_offset,
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
                .read_blocks(
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
            read_partial_block(
                &self.state,
                scratch,
                phys,
                0,
                take,
                cur,
                request_offset,
                dst,
            )
            .map_err(map_err)?;
        }

        Ok(())
    }
}

impl FileOps for ExtRegFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.with_ops(|_inode, ops| {
            let (flags, size, i_block) = {
                let g = ops.raw.lock();
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
                let raw_bytes = ops.raw.lock().bytes.clone();
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

            let ranges = self
                .map_ranges(flags, size, &i_block, first_lb, lb_count)
                .map_err(map_err)?;

            // if ranges.is_empty() && offset == 0 {
            //     log::info!("[extfs] read_at offset=0 size={} flags={:#x} ranges EMPTY first_lb={} lb_count={}", size, flags, first_lb, lb_count);
            // } else if offset == 0 {
            //     log::info!("[extfs] read_at offset=0 size={} ranges={} first=({},{},{:#x})", size, ranges.len(), ranges[0].0, ranges[0].1, ranges[0].2);
            // }

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
                    *phys_start,
                    read_start,
                    read_end,
                    offset,
                    buf,
                )?;
                filled_until = filled_until.max(buf_pos + overlap_bytes);
            }
            if filled_until < remaining {
                zero_bytes(&mut buf[filled_until..remaining]);
            }
            Ok(remaining)
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let _io = self.io_mu.lock();
        self.with_ops(|inode, ops| {
            let mut raw_guard = ops.raw.lock();
            let old_size = raw_guard.size();
            let start = if offset == u64::MAX { old_size } else { offset };
            let end = start
                .checked_add(buf.len() as u64)
                .ok_or(VfsError::FileTooLarge)?;

            let old_flags = raw_guard.flags();
            let old_blocks_lo = raw_guard.blocks_lo();
            let mut flags = old_flags;
            let mut i_block = raw_guard.i_block().to_vec();
            let old_i_block = i_block.clone();

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
                i_block = raw_guard.i_block().to_vec();
                raw_guard.set_size(0);
                recovered
            } else {
                None
            };
            // 不强行 demote — 如果是 extent 文件且能原地 append 就留着;
            // 真正无法容纳时再退化成间接布局。
            let block_size = self.state.ext_sb.block_size as u64;

            // 把旧 inline 内容(如果有)写回为数据块
            if let Some(old) = inline_recovered {
                if !old.is_empty() {
                    let mut written = 0usize;
                    let total = old.len();
                    while written < total {
                        let lb = (written as u64 / block_size) as u32;
                        let in_block = (written as u64 % block_size) as usize;
                        let want = ((block_size - in_block as u64) as usize).min(total - written);
                        let phys = ensure_block_any(&self.state, &mut flags, &mut i_block, lb)
                            .map_err(map_err)?;
                        let mut blk = vec![0u8; block_size as usize];
                        if in_block != 0 || want < block_size as usize {
                            self.state.read_block(phys, &mut blk).map_err(map_err)?;
                        }
                        blk[in_block..in_block + want]
                            .copy_from_slice(&old[written..written + want]);
                        self.state.write_block(phys, &blk).map_err(map_err)?;
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
                    let Some((phys, run_blocks)) = extent_wr::ensure_extent_run(
                        &self.state,
                        &mut i_block,
                        lb,
                        remain_blocks,
                    )
                    .map_err(map_err)?
                    else {
                        break;
                    };
                    let bytes = run_blocks as usize * block_size as usize;
                    self.state
                        .write_blocks(phys, run_blocks, &buf[written..written + bytes])
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
                    let phys = ensure_block_any(&self.state, &mut flags, &mut i_block, lb)
                        .map_err(map_err)?;
                    if let Some(prev_lb) = cur_lb {
                        if prev_lb != lb {
                            self.state
                                .write_block(cur_phys, &cur_blk_buf)
                                .map_err(map_err)?;
                        }
                    }
                    if cur_lb != Some(lb) {
                        if in_block != 0 || want < block_size as usize {
                            self.state
                                .read_block(phys, &mut cur_blk_buf)
                                .map_err(map_err)?;
                        }
                        // 整块覆盖时 buf 内容会被随后的 copy_from_slice 完整改写,无需清零
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
                        .write_block(cur_phys, &cur_blk_buf)
                        .map_err(map_err)?;
                }
            }

            let new_size = raw_guard.size().max(end);
            let blocks512 = (new_size + 511) / 512;
            let new_blocks_lo = blocks512 as u32;
            let metadata_changed = new_size != old_size
                || new_blocks_lo != old_blocks_lo
                || flags != old_flags
                || i_block != old_i_block;
            if metadata_changed {
                raw_guard.i_block_mut().copy_from_slice(&i_block);
                raw_guard.set_flags(flags);
                raw_guard.set_size(new_size);
                raw_guard.set_blocks_lo(new_blocks_lo);
                write_raw(&self.state, &raw_guard).map_err(map_err)?;
                inode.set_size(new_size);
                self.invalidate_map_cache();
            }

            Ok(written)
        })
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
    }
    fn release(&self) {}
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
/// - 若文件使用 extent 且根节点能容纳下新叶子,原地追加并返回新分配物理块;
/// - 若 extent 根已满或为索引节点,fallback 到 `demote_if_extent` + indirect;
/// - 若文件本就是 indirect,直接走 `map_wr::ensure_block`。
fn ensure_block_any(
    state: &FsState,
    flags: &mut u32,
    i_block: &mut [u8],
    lb: u32,
) -> Result<u64, crate::state::BlockBackendError> {
    if *flags & crate::layout::EXT4_EXTENTS_FL != 0 {
        if let Some(phys) = extent_wr::ensure_block_in_extent(state, i_block, lb)? {
            return Ok(phys);
        }
        // 原地追加失败 → 降级
        extent_wr::demote_if_extent(state, flags, i_block)?;
    }
    map_wr::ensure_block(state, i_block, lb)
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
    dst: &mut [u8],
) -> Result<(), crate::state::BlockBackendError> {
    let block_size = state.ext_sb.block_size as usize;
    if scratch.len() != block_size {
        scratch.resize(block_size, 0);
    }
    state.read_block(phys, scratch)?;
    let dst_pos = (cur - request_offset) as usize;
    dst[dst_pos..dst_pos + take].copy_from_slice(&scratch[block_off..block_off + take]);
    Ok(())
}

fn clip_ranges(ranges: &[(u32, u32, u64)], first_lb: u32, lb_count: u32) -> Vec<(u32, u32, u64)> {
    if lb_count == 0 {
        return Vec::new();
    }
    let end_lb = first_lb.saturating_add(lb_count);
    let mut out = Vec::new();
    for &(range_lb, range_count, phys_start) in ranges {
        let range_end = range_lb.saturating_add(range_count);
        if range_end <= first_lb || range_lb >= end_lb {
            continue;
        }
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
