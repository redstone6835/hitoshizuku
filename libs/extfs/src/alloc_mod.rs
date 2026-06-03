//! 位图分配/释放(块 + inode) + 超级块/块组计数回写。
//!
//! 所有分配都严格按顺序:**位图置位 → 更新计数 → 落盘**。没有日志兜底,
//! 中间崩溃会让 FS 损坏。mount 会因此拒绝 `NEEDS_RECOVERY`,要求 clean umount
//! 或 fsck。

use alloc::vec;
use core::sync::atomic::Ordering;

use crate::bgd;
use crate::crc;
use crate::layout::{SUPERBLOCK_CHECKSUM_OFFSET, SUPERBLOCK_OFFSET};
use crate::state::{BlockBackendError, FsState};
use vfs::sync::Spinlock;

/// 互斥锁:整个 FS 串行化分配/释放路径。粒度粗但简单,不留 torn-state。
static ALLOC_LOCK: Spinlock<()> = Spinlock::new(());

/// 在块位图中找一个空闲 bit 并置位,返回分配到的 0-based 位号(该组内部)。
fn alloc_bit_in_bitmap(
    state: &FsState,
    bitmap_block: u64,
    bits_in_group: u32,
    start_hint: u32,
) -> Result<Option<u32>, BlockBackendError> {
    let mut bm = vec![0u8; state.ext_sb.block_size as usize];
    state.read_block(bitmap_block, &mut bm)?;

    let start = start_hint.min(bits_in_group);
    let nr = find_zero_bit(&bm, start, bits_in_group)
        .or_else(|| (start > 0).then(|| find_zero_bit(&bm, 0, start)).flatten());
    if let Some(nr) = nr {
        let byte_idx = (nr / 8) as usize;
        bm[byte_idx] |= 1 << ((nr % 8) as u8);
        state.write_block(bitmap_block, &bm)?;
        return Ok(Some(nr));
    }
    Ok(None)
}

/// 读取 bitmap 中以 `byte_off` 起始的 8 字节作为 u64 (LE)。越界字节按 0xff 处理。
#[inline]
fn read_bitmap_u64(bm: &[u8], byte_off: usize) -> u64 {
    let mut buf = [0xffu8; 8];
    let avail = bm.len().saturating_sub(byte_off).min(8);
    if avail > 0 {
        buf[..avail].copy_from_slice(&bm[byte_off..byte_off + avail]);
    }
    u64::from_le_bytes(buf)
}

#[inline]
fn bit_is_zero(bm: &[u8], nr: u32) -> bool {
    let byte_idx = (nr / 8) as usize;
    let mask = 1u8 << ((nr % 8) as u8);
    bm.get(byte_idx).copied().unwrap_or(0xff) & mask == 0
}

fn find_zero_bit(bm: &[u8], start: u32, end: u32) -> Option<u32> {
    let mut nr = start;
    // 对齐到 64-bit 字边界前逐位扫描
    while nr < end && nr % 64 != 0 {
        if bit_is_zero(bm, nr) {
            return Some(nr);
        }
        nr += 1;
    }
    // 主循环：按 u64 字扫描
    while nr + 64 <= end {
        let word = read_bitmap_u64(bm, (nr / 8) as usize);
        if word != u64::MAX {
            let bit = (!word).trailing_zeros();
            return Some(nr + bit);
        }
        nr += 64;
    }
    // 尾部逐位扫描
    while nr < end {
        if bit_is_zero(bm, nr) {
            return Some(nr);
        }
        nr += 1;
    }
    None
}

fn clear_bit_in_bitmap(
    state: &FsState,
    bitmap_block: u64,
    bit: u32,
) -> Result<(), BlockBackendError> {
    let mut bm = vec![0u8; state.ext_sb.block_size as usize];
    state.read_block(bitmap_block, &mut bm)?;
    let byte_idx = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8) as u8;
    if bm[byte_idx] & mask == 0 {
        // 已经是空闲,不算错误(容忍重复 free)
        return Ok(());
    }
    bm[byte_idx] &= !mask;
    state.write_block(bitmap_block, &bm)?;
    Ok(())
}

/// 分配一个数据块。按组顺序扫描 `s_free_blocks`;找到则更新 gd/sb 回写。
/// 返回物理块号(绝对)。
pub(crate) fn alloc_block(state: &FsState) -> Result<u64, BlockBackendError> {
    let r = alloc_blocks_run(state, 1)?;
    Ok(r.0)
}

/// 批量分配最多 `want` 个连续数据块。返回 (起始物理块号, 实际分配数量)。
/// 至少分配 1 个块，否则返回错误。
pub(crate) fn alloc_blocks_run(
    state: &FsState,
    want: u32,
) -> Result<(u64, u32), BlockBackendError> {
    let _g = ALLOC_LOCK.lock();
    let sb = &state.ext_sb;
    let start_rel = state.block_alloc_hint.load(Ordering::Relaxed);
    let start_group = if sb.groups_count == 0 {
        0
    } else {
        ((start_rel / sb.blocks_per_group as u64) as u32).min(sb.groups_count - 1)
    };
    for pass in 0..sb.groups_count {
        let group = (start_group + pass) % sb.groups_count;
        if state.group_counts(group)?.free_blocks == 0 {
            continue;
        }
        let gd = state.group_desc_mut(group)?;
        let bmap = gd.block_bitmap;
        let bits_in_group = if group == sb.groups_count - 1 {
            let used_before = group as u64 * sb.blocks_per_group as u64;
            let remain = sb.blocks_count.saturating_sub(used_before);
            remain.min(sb.blocks_per_group as u64) as u32
        } else {
            sb.blocks_per_group
        };
        let start_bit = if group == start_group {
            (start_rel % sb.blocks_per_group as u64) as u32
        } else {
            0
        };
        let got = alloc_run_in_bitmap(state, bmap, bits_in_group, start_bit, want)?;
        if let Some((nr, count)) = got {
            let phys =
                sb.first_data_block as u64 + group as u64 * sb.blocks_per_group as u64 + nr as u64;
            let next_rel = group as u64 * sb.blocks_per_group as u64 + nr as u64 + count as u64;
            state.block_alloc_hint.store(next_rel, Ordering::Relaxed);
            state.adjust_group_free_blocks(group, -(count as i32))?;
            state.adjust_sb_free_blocks(-(count as i64))?;
            return Ok((phys, count));
        }
    }
    Err(BlockBackendError::OutOfRange)
}

/// 在位图中找最多 `want` 个连续空闲 bit 并置位。返回 (起始位号, 数量)。
fn alloc_run_in_bitmap(
    state: &FsState,
    bitmap_block: u64,
    bits_in_group: u32,
    start_hint: u32,
    want: u32,
) -> Result<Option<(u32, u32)>, BlockBackendError> {
    let mut bm = vec![0u8; state.ext_sb.block_size as usize];
    state.read_block(bitmap_block, &mut bm)?;

    let start = start_hint.min(bits_in_group);
    let result = find_zero_run(&bm, start, bits_in_group, want).or_else(|| {
        (start > 0)
            .then(|| find_zero_run(&bm, 0, start, want))
            .flatten()
    });
    if let Some((nr, count)) = result {
        for i in 0..count {
            let bit = nr + i;
            let byte_idx = (bit / 8) as usize;
            bm[byte_idx] |= 1 << ((bit % 8) as u8);
        }
        state.write_block(bitmap_block, &bm)?;
        return Ok(Some((nr, count)));
    }
    Ok(None)
}

/// 在位图中找最多 `want` 个连续的 0-bit，至少找到 1 个才返回。
fn find_zero_run(bm: &[u8], start: u32, end: u32, want: u32) -> Option<(u32, u32)> {
    let first = find_zero_bit(bm, start, end)?;
    let run_start = first;
    let mut count = 1u32;
    let mut nr = first + 1;
    while count < want && nr < end {
        // 对齐后的全零 u64 可直接累加 64
        if nr % 64 == 0 && nr + 64 <= end && count + 64 <= want {
            let word = read_bitmap_u64(bm, (nr / 8) as usize);
            if word == 0 {
                count += 64;
                nr += 64;
                continue;
            }
        }
        if !bit_is_zero(bm, nr) {
            break;
        }
        count += 1;
        nr += 1;
    }
    Some((run_start, count))
}

/// 释放一个数据块。宽松:允许重复释放(do nothing if bitmap bit 已 0)。
pub(crate) fn free_block(state: &FsState, block: u64) -> Result<(), BlockBackendError> {
    let _g = ALLOC_LOCK.lock();
    let sb = &state.ext_sb;
    if block < sb.first_data_block as u64 {
        return Err(BlockBackendError::OutOfRange);
    }
    let rel = block - sb.first_data_block as u64;
    let group = (rel / sb.blocks_per_group as u64) as u32;
    let in_group = (rel % sb.blocks_per_group as u64) as u32;
    let gd = state.group_desc_mut(group)?;
    clear_bit_in_bitmap(state, gd.block_bitmap, in_group)?;
    state.block_alloc_hint.store(
        group as u64 * sb.blocks_per_group as u64 + in_group as u64,
        Ordering::Relaxed,
    );
    state.adjust_group_free_blocks(group, 1)?;
    state.adjust_sb_free_blocks(1)?;
    Ok(())
}

/// 分配一个 inode(非目录)。`is_dir` 控制用于 `s_used_dirs` 计数。
pub(crate) fn alloc_inode(state: &FsState, is_dir: bool) -> Result<u32, BlockBackendError> {
    let _g = ALLOC_LOCK.lock();
    let sb = &state.ext_sb;
    let start_rel = state.inode_alloc_hint.load(Ordering::Relaxed);
    let start_group = if sb.groups_count == 0 {
        0
    } else {
        (start_rel / sb.inodes_per_group).min(sb.groups_count - 1)
    };
    for pass in 0..sb.groups_count {
        let group = (start_group + pass) % sb.groups_count;
        if state.group_counts(group)?.free_inodes == 0 {
            continue;
        }
        let gd = state.group_desc_mut(group)?;
        let start_bit = if group == start_group {
            start_rel % sb.inodes_per_group
        } else {
            0
        };
        let nr = alloc_bit_in_bitmap(state, gd.inode_bitmap, sb.inodes_per_group, start_bit)?;
        if let Some(nr) = nr {
            let ino = group * sb.inodes_per_group + nr + 1;
            state
                .inode_alloc_hint
                .store(group * sb.inodes_per_group + nr + 1, Ordering::Relaxed);
            state.adjust_group_free_inodes(group, -1)?;
            if is_dir {
                state.adjust_group_used_dirs(group, 1)?;
            }
            state.adjust_sb_free_inodes(-1)?;
            return Ok(ino);
        }
    }
    Err(BlockBackendError::OutOfRange)
}

pub(crate) fn free_inode(state: &FsState, ino: u32, is_dir: bool) -> Result<(), BlockBackendError> {
    let _g = ALLOC_LOCK.lock();
    if ino == 0 {
        return Ok(());
    }
    let sb = &state.ext_sb;
    let group = (ino - 1) / sb.inodes_per_group;
    let in_group = (ino - 1) % sb.inodes_per_group;
    let gd = state.group_desc_mut(group)?;
    clear_bit_in_bitmap(state, gd.inode_bitmap, in_group)?;
    state
        .inode_alloc_hint
        .store(group * sb.inodes_per_group + in_group, Ordering::Relaxed);
    state.adjust_group_free_inodes(group, 1)?;
    if is_dir {
        state.adjust_group_used_dirs(group, -1)?;
    }
    state.adjust_sb_free_inodes(1)?;
    Ok(())
}

/// 写超级块的若干计数字段(free_blocks, free_inodes)并重算 checksum。
pub(crate) fn write_superblock(state: &FsState) -> Result<(), BlockBackendError> {
    let sector_size = state.backend.sector_size() as u64;
    let start_sector = SUPERBLOCK_OFFSET / sector_size;
    let in_sector = (SUPERBLOCK_OFFSET % sector_size) as usize;
    let sectors_needed = (1024 + in_sector + sector_size as usize - 1) / sector_size as usize;
    let mut raw = vec![0u8; sectors_needed * sector_size as usize];
    state
        .backend
        .read_sectors(start_sector, sectors_needed as u32, &mut raw)?;
    let sb_slice = &mut raw[in_sector..in_sector + 1024];

    // 更新 s_free_blocks_count_{lo,hi} 与 s_free_inodes_count
    let free_blocks = state.ext_sb_free_blocks();
    let free_inodes = state.ext_sb_free_inodes();
    sb_slice[12..16].copy_from_slice(&(free_blocks as u32).to_le_bytes());
    sb_slice[16..20].copy_from_slice(&free_inodes.to_le_bytes());
    if state.ext_sb.feature_incompat & crate::layout::INCOMPAT_64BIT != 0 {
        let hi = (free_blocks >> 32) as u32;
        sb_slice[0x154..0x158].copy_from_slice(&hi.to_le_bytes());
    }

    // Recompute checksum
    if state.ext_sb.metadata_csum {
        let sum = crc::crc32c(&sb_slice[..SUPERBLOCK_CHECKSUM_OFFSET]);
        sb_slice[SUPERBLOCK_CHECKSUM_OFFSET..SUPERBLOCK_CHECKSUM_OFFSET + 4]
            .copy_from_slice(&sum.to_le_bytes());
    }
    state
        .backend
        .write_sectors(start_sector, sectors_needed as u32, &raw)
}

/// 把状态里缓存的某个块组描述符写回磁盘。
pub(crate) fn flush_group_desc(state: &FsState, group: u32) -> Result<(), BlockBackendError> {
    let cached = state.group_desc_ref(group)?;
    let counts = state.group_counts(group)?;
    bgd::write_desc(
        state.backend.as_ref(),
        &state.ext_sb,
        group,
        &cached,
        counts.free_blocks,
        counts.free_inodes,
        counts.used_dirs,
    )
}
