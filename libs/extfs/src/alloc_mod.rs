//! 位图分配/释放(块 + inode) + 超级块/块组计数回写。
//!
//! 所有分配都严格按顺序:**位图置位 → 更新计数 → 落盘**。没有日志兜底,
//! 中间崩溃会让 FS 损坏。mount 会因此拒绝 `NEEDS_RECOVERY`,要求 clean umount
//! 或 fsck。

use alloc::vec;
use core::sync::atomic::Ordering;

use crate::bgd;
use crate::crc;
use crate::layout::{
    COMPAT_ORPHAN_FILE, RO_COMPAT_ORPHAN_PRESENT, SUPERBLOCK_CHECKSUM_OFFSET, SUPERBLOCK_OFFSET,
};
use crate::state::{BlockBackendError, FsState};
use vfs::sync::{Spinlock, SpinlockGuard};

/// 互斥锁:整个 FS 串行化分配/释放路径。粒度粗但简单,不留 torn-state。
static ALLOC_LOCK: Spinlock<()> = Spinlock::new(());

fn lock_alloc() -> SpinlockGuard<'static, ()> {
    loop {
        if let Some(guard) = ALLOC_LOCK.try_lock() {
            return guard;
        }
        // extfs 分配临界区当前会触发块 I/O，等待方不能纯自旋，否则单核
        // 下持锁任务拿不到 CPU。这里必须用真实时间戳推进 fair 调度；
        // `schedule_once(0)` 不记账，在 iozone 多进程写入时会放大成饥饿。
        if sched::is_ready() {
            sched::schedule_once(sched::now_ns_public());
        } else {
            core::hint::spin_loop();
        }
    }
}

/// 在块位图中找一个空闲 bit 并置位,返回分配到的 0-based 位号(该组内部)。
fn alloc_bit_in_bitmap(
    state: &FsState,
    bitmap_block: u64,
    bits_in_group: u32,
    start_hint: u32,
    bm: &mut [u8],
) -> Result<Option<u32>, BlockBackendError> {
    state.read_block(bitmap_block, bm)?;

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
    bm: &mut [u8],
) -> Result<bool, BlockBackendError> {
    state.read_block(bitmap_block, bm)?;
    let byte_idx = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8) as u8;
    if bm[byte_idx] & mask == 0 {
        // 已经是空闲,不算错误(容忍重复 free)
        return Ok(false);
    }
    bm[byte_idx] &= !mask;
    state.write_block(bitmap_block, &bm)?;
    Ok(true)
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
    let _g = lock_alloc();
    let sb = &state.ext_sb;
    let mut bitmap_scratch = vec![0u8; sb.block_size as usize];
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
        let got = alloc_run_in_bitmap(
            state,
            bmap,
            bits_in_group,
            start_bit,
            want,
            &mut bitmap_scratch,
        )?;
        if let Some((nr, count)) = got {
            let phys =
                sb.first_data_block as u64 + group as u64 * sb.blocks_per_group as u64 + nr as u64;
            let next_rel = group as u64 * sb.blocks_per_group as u64 + nr as u64 + count as u64;
            state.block_alloc_hint.store(next_rel, Ordering::Relaxed);
            state.adjust_group_free_blocks(group, -(count as i32))?;
            state.adjust_sb_free_blocks(-(count as i64))?;
            state.flush_alloc_metadata()?;
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
    bm: &mut [u8],
) -> Result<Option<(u32, u32)>, BlockBackendError> {
    state.read_block(bitmap_block, bm)?;

    let start = start_hint.min(bits_in_group);
    let result = choose_zero_run(&bm, start, bits_in_group, want);
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

fn choose_zero_run(bm: &[u8], start: u32, end: u32, want: u32) -> Option<(u32, u32)> {
    let want = want.max(1);
    let primary = find_zero_run(bm, start, end, want);
    if primary.is_some_and(|(_, count)| count >= want) {
        return primary;
    }
    let wrapped = (start > 0)
        .then(|| find_zero_run(bm, 0, start, want))
        .flatten();
    if wrapped.is_some_and(|(_, count)| count >= want) {
        return wrapped;
    }
    match (primary, wrapped) {
        (Some(a), Some(b)) if b.1 > a.1 => Some(b),
        (Some(a), _) => Some(a),
        (None, b) => b,
    }
}

/// 在位图中找一段连续 0-bit。
///
/// 优先返回满足 `want` 的 run；如果本扫描区间内没有足够长的连续空间，则返回
/// 最长的短 run。bench 的新文件顺序写会直接受 extent 长度影响，不能只拿第一个
/// 零 bit 附近的短 run，否则会把一次大写拆成多条 extent 和多轮位图更新。
fn find_zero_run(bm: &[u8], start: u32, end: u32, want: u32) -> Option<(u32, u32)> {
    let want = want.max(1);
    let mut nr = start;
    let mut best: Option<(u32, u32)> = None;

    while nr < end {
        let Some(run_start) = find_zero_bit(bm, nr, end) else {
            break;
        };
        let mut count = 1u32;
        nr = run_start + 1;

        while count < want && nr < end {
            // 对齐后的全零 u64 可直接累加，减少大空闲区上的逐位判断。
            if nr % 64 == 0 && nr + 64 <= end && count + 64 <= want {
                let word = read_bitmap_u64(bm, (nr / 8) as usize);
                if word == 0 {
                    count += 64;
                    nr += 64;
                    continue;
                }
            }
            if !bit_is_zero(bm, nr) {
                nr += 1;
                break;
            }
            count += 1;
            nr += 1;
        }

        if count >= want {
            return Some((run_start, want));
        }
        match best {
            Some((_, best_count)) if best_count >= count => {}
            _ => best = Some((run_start, count)),
        }

        // 如果 run 是因为达到 end 结束，后面没有更多候选。
        if nr >= end {
            break;
        }
        // 如果 run 是因为达到 want 结束，上面已经返回；其余情况 nr 已跳过占用 bit。
        while nr < end && !bit_is_zero(bm, nr) {
            if nr % 64 == 0 && nr + 64 <= end {
                let word = read_bitmap_u64(bm, (nr / 8) as usize);
                if word == u64::MAX {
                    nr += 64;
                    continue;
                }
            }
            nr += 1;
        }
    }

    best
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;

    fn mark_used(bits: &mut [u8], bit: u32) {
        let byte_idx = (bit / 8) as usize;
        bits[byte_idx] |= 1 << ((bit % 8) as u8);
    }

    #[test]
    fn find_zero_run_prefers_requested_length_over_first_short_run() {
        let mut bits = vec![0xffu8; 16];
        // [2, 4) 是短 run，[8, 14) 能满足请求。
        for bit in 2..4 {
            bits[(bit / 8) as usize] &= !(1 << ((bit % 8) as u8));
        }
        for bit in 8..14 {
            bits[(bit / 8) as usize] &= !(1 << ((bit % 8) as u8));
        }

        assert_eq!(find_zero_run(&bits, 0, 32, 6), Some((8, 6)));
    }

    #[test]
    fn find_zero_run_returns_longest_short_run_when_request_cannot_fit() {
        let mut bits = vec![0xffu8; 16];
        for bit in 1..3 {
            bits[(bit / 8) as usize] &= !(1 << ((bit % 8) as u8));
        }
        for bit in 8..12 {
            bits[(bit / 8) as usize] &= !(1 << ((bit % 8) as u8));
        }
        mark_used(&mut bits, 12);

        assert_eq!(find_zero_run(&bits, 0, 32, 8), Some((8, 4)));
    }

    #[test]
    fn choose_zero_run_checks_wrapped_region_before_accepting_short_run() {
        let mut bits = vec![0xffu8; 16];
        for bit in 2..8 {
            bits[(bit / 8) as usize] &= !(1 << ((bit % 8) as u8));
        }
        for bit in 18..20 {
            bits[(bit / 8) as usize] &= !(1 << ((bit % 8) as u8));
        }

        assert_eq!(choose_zero_run(&bits, 16, 32, 6), Some((2, 6)));
    }
}

/// 释放一个数据块。宽松:允许重复释放(do nothing if bitmap bit 已 0)。
pub(crate) fn free_block(state: &FsState, block: u64) -> Result<(), BlockBackendError> {
    let _g = lock_alloc();
    let sb = &state.ext_sb;
    let mut bitmap_scratch = vec![0u8; sb.block_size as usize];
    if block < sb.first_data_block as u64 {
        return Err(BlockBackendError::OutOfRange);
    }
    let rel = block - sb.first_data_block as u64;
    let group = (rel / sb.blocks_per_group as u64) as u32;
    let in_group = (rel % sb.blocks_per_group as u64) as u32;
    let gd = state.group_desc_mut(group)?;
    if !clear_bit_in_bitmap(state, gd.block_bitmap, in_group, &mut bitmap_scratch)? {
        return Ok(());
    }
    state.block_alloc_hint.store(
        group as u64 * sb.blocks_per_group as u64 + in_group as u64,
        Ordering::Relaxed,
    );
    state.adjust_group_free_blocks(group, 1)?;
    state.adjust_sb_free_blocks(1)?;
    state.flush_alloc_metadata()?;
    Ok(())
}

/// 批量释放一段连续数据块，并把同一批次的 group/superblock 元数据合并刷新。
///
/// iozone 这类测试会频繁删除刚写完的大文件；extent 中通常是一段连续物理块。
/// 若逐块调用 `free_block()`，每个 4KiB 数据块都会触发一次 bitmap checksum、
/// group descriptor 和 superblock 写回，收尾阶段会被放大成近似卡死。
pub(crate) fn free_blocks_run(
    state: &FsState,
    start_block: u64,
    count: u32,
) -> Result<(), BlockBackendError> {
    if count == 0 {
        return Ok(());
    }

    let _g = lock_alloc();
    let sb = &state.ext_sb;
    if start_block < sb.first_data_block as u64 {
        return Err(BlockBackendError::OutOfRange);
    }

    let mut bitmap_scratch = vec![0u8; sb.block_size as usize];
    let first_rel = start_block - sb.first_data_block as u64;
    let mut rel = first_rel;
    let mut remaining = count;
    let mut total_cleared = 0u32;

    while remaining > 0 {
        let group = (rel / sb.blocks_per_group as u64) as u32;
        if group >= sb.groups_count {
            return Err(BlockBackendError::OutOfRange);
        }

        let in_group = (rel % sb.blocks_per_group as u64) as u32;
        let bits_in_group = if group == sb.groups_count - 1 {
            let used_before = group as u64 * sb.blocks_per_group as u64;
            let remain = sb.blocks_count.saturating_sub(used_before);
            remain.min(sb.blocks_per_group as u64) as u32
        } else {
            sb.blocks_per_group
        };
        if in_group >= bits_in_group {
            return Err(BlockBackendError::OutOfRange);
        }

        let run = remaining.min(bits_in_group - in_group);
        let gd = state.group_desc_mut(group)?;
        state.read_block(gd.block_bitmap, &mut bitmap_scratch)?;

        let mut cleared = 0u32;
        for bit in in_group..in_group + run {
            let byte_idx = (bit / 8) as usize;
            let mask = 1u8 << ((bit % 8) as u8);
            if bitmap_scratch[byte_idx] & mask != 0 {
                bitmap_scratch[byte_idx] &= !mask;
                cleared += 1;
            }
        }

        if cleared != 0 {
            state.write_block(gd.block_bitmap, &bitmap_scratch)?;
            state.adjust_group_free_blocks(group, cleared as i32)?;
            total_cleared += cleared;
        }

        rel += run as u64;
        remaining -= run;
    }

    if total_cleared != 0 {
        state.block_alloc_hint.store(first_rel, Ordering::Relaxed);
        state.adjust_sb_free_blocks(total_cleared as i64)?;
        state.flush_alloc_metadata()?;
    }
    Ok(())
}

/// 分配一个 inode(非目录)。`is_dir` 控制用于 `s_used_dirs` 计数。
pub(crate) fn alloc_inode(state: &FsState, is_dir: bool) -> Result<u32, BlockBackendError> {
    let _g = lock_alloc();
    let sb = &state.ext_sb;
    let mut bitmap_scratch = vec![0u8; sb.block_size as usize];
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
        let nr = alloc_bit_in_bitmap(
            state,
            gd.inode_bitmap,
            sb.inodes_per_group,
            start_bit,
            &mut bitmap_scratch,
        )?;
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
            state.flush_alloc_metadata()?;
            return Ok(ino);
        }
    }
    Err(BlockBackendError::OutOfRange)
}

pub(crate) fn free_inode(state: &FsState, ino: u32, is_dir: bool) -> Result<(), BlockBackendError> {
    let _g = lock_alloc();
    if ino == 0 {
        return Ok(());
    }
    let sb = &state.ext_sb;
    let mut bitmap_scratch = vec![0u8; sb.block_size as usize];
    let group = (ino - 1) / sb.inodes_per_group;
    let in_group = (ino - 1) % sb.inodes_per_group;
    let gd = state.group_desc_mut(group)?;
    if !clear_bit_in_bitmap(state, gd.inode_bitmap, in_group, &mut bitmap_scratch)? {
        return Ok(());
    }
    state
        .inode_alloc_hint
        .store(group * sb.inodes_per_group + in_group, Ordering::Relaxed);
    state.adjust_group_free_inodes(group, 1)?;
    if is_dir {
        state.adjust_group_used_dirs(group, -1)?;
    }
    state.adjust_sb_free_inodes(1)?;
    state.flush_alloc_metadata()?;
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
    // 当前写路径不维护 ext4 orphan_file。进入可写挂载后清掉该特性位,
    // 否则 e2fsck 会按 orphan file 扫描已释放 inode 并报告 corrupted orphan list。
    let compat = u32::from_le_bytes([sb_slice[92], sb_slice[93], sb_slice[94], sb_slice[95]])
        & !COMPAT_ORPHAN_FILE;
    sb_slice[92..96].copy_from_slice(&compat.to_le_bytes());
    let ro_compat =
        u32::from_le_bytes([sb_slice[100], sb_slice[101], sb_slice[102], sb_slice[103]])
            & !RO_COMPAT_ORPHAN_PRESENT;
    sb_slice[100..104].copy_from_slice(&ro_compat.to_le_bytes());
    sb_slice[0xe8..0xec].copy_from_slice(&0u32.to_le_bytes());
    sb_slice[0x280..0x284].copy_from_slice(&0u32.to_le_bytes());
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
