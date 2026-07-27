//! 间接块写路径:扩容(按需分配)/ 截断(释放)。
//!
//! `i_block[0..60]` 布局与 [`map`](crate::map) 相同。所有修改在内存副本 `i_block`
//! 上完成,由调用方负责把它写回 inode。

use alloc::vec;
use alloc::vec::Vec;

use crate::alloc_mod;
use crate::state::{BlockBackendError, FsState};

const DIRECT_COUNT: u32 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockAllocState {
    Existing(u64),
    NewlyAllocated(u64),
}

impl BlockAllocState {
    pub(crate) const fn phys(self) -> u64 {
        match self {
            Self::Existing(phys) | Self::NewlyAllocated(phys) => phys,
        }
    }

    pub(crate) const fn is_new(self) -> bool {
        matches!(self, Self::NewlyAllocated(_))
    }
}

#[inline]
fn ppb(block_size: u32) -> u32 {
    block_size / 4
}

fn read_u32(block: &[u8], idx: u32) -> u32 {
    let o = (idx as usize) * 4;
    u32::from_le_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]])
}

fn write_u32(block: &mut [u8], idx: u32, v: u32) {
    let o = (idx as usize) * 4;
    block[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// 给逻辑块号 `logical` 分配物理块,并把路径上的间接块补齐。返回物理块号。
/// 若物理块已存在则直接返回现有。
pub(crate) fn ensure_block(
    state: &FsState,
    i_block: &mut [u8],
    logical: u32,
) -> Result<u64, BlockBackendError> {
    ensure_block_for_write(state, i_block, logical, true).map(BlockAllocState::phys)
}

/// 给写路径查找或分配逻辑块，并返回该数据块是否刚分配。
///
/// `zero_new_data` 只控制新数据块是否立即写零；调用方若会整块覆盖或会自行填零，
/// 可以跳过这次写盘，避免 bench 写路径里的重复 I/O。间接块本身仍始终清零。
pub(crate) fn ensure_block_for_write(
    state: &FsState,
    i_block: &mut [u8],
    logical: u32,
    zero_new_data: bool,
) -> Result<BlockAllocState, BlockBackendError> {
    let mut scratch = Vec::new();
    ensure_block_for_write_with_scratch(state, i_block, logical, zero_new_data, &mut scratch)
}

/// 与 [`ensure_block_for_write`] 相同,但由调用方复用间接块 scratch。
///
/// 文件顺序写降级到 indirect 布局时会逐块调用这里；把 block-size 缓冲提升到
/// 调用层,可以避免每个逻辑块都重新分配一块临时内存。
pub(crate) fn ensure_block_for_write_with_scratch(
    state: &FsState,
    i_block: &mut [u8],
    logical: u32,
    zero_new_data: bool,
    scratch: &mut Vec<u8>,
) -> Result<BlockAllocState, BlockBackendError> {
    ensure_block_for_write_with_scratch_count(state, i_block, logical, zero_new_data, scratch)
        .map(|(block, _)| block)
}

/// 与 [`ensure_block_for_write_with_scratch`] 相同，并返回本次为间接映射新建的
/// 索引块数量。数据块是否新分配仍由 [`BlockAllocState`] 表达。
///
/// `i_blocks` 必须同时统计数据块和间接索引块；把增量从分配点向上传递可避免
/// 每次写入后重新遍历整棵映射树。
pub(crate) fn ensure_block_for_write_with_scratch_count(
    state: &FsState,
    i_block: &mut [u8],
    logical: u32,
    zero_new_data: bool,
    scratch: &mut Vec<u8>,
) -> Result<(BlockAllocState, u32), BlockBackendError> {
    let p = ppb(state.ext_sb.block_size);
    if logical < DIRECT_COUNT {
        let cur = read_u32(i_block, logical);
        if cur != 0 {
            return Ok((BlockAllocState::Existing(cur as u64), 0));
        }
        let new = alloc_mod::alloc_block(state)?;
        if zero_new_data {
            zero_block(state, new)?;
        }
        write_u32(i_block, logical, new as u32);
        return Ok((BlockAllocState::NewlyAllocated(new), 0));
    }

    let bs = state.ext_sb.block_size as usize;
    if scratch.len() != bs {
        scratch.resize(bs, 0);
    }
    // 各级间接块共用同一个 scratch buffer,避免重复堆分配。
    let buf = scratch.as_mut_slice();
    let rem = logical - DIRECT_COUNT;
    if rem < p {
        return alloc_or_walk_l1(state, i_block, 12, rem, buf, zero_new_data);
    }

    let rem = rem - p;
    if rem < p * p {
        // 二级间接:先确保 L2 索引块存在,把它读到 buf
        let (l2, mut new_metadata) = ensure_indirect_slot(state, i_block, 13, buf)?;
        let mid_idx = rem / p;
        let mut mid = read_u32(buf, mid_idx);
        if mid == 0 {
            mid = alloc_mod::alloc_block(state)? as u32;
            new_metadata += 1;
            // 新 mid 块逻辑上为零,直接写零到磁盘后续不再 read_block
            write_u32(buf, mid_idx, mid);
            state.write_block(l2, buf)?;
            // mid 块需要置零 (因为我们随后会读它),走 cache insert
            cache_zero_block(state, mid as u64, buf)?;
        } else {
            state.read_block(mid as u64, buf)?;
        }
        let inner = rem % p;
        let cur = read_u32(buf, inner);
        if cur != 0 {
            return Ok((BlockAllocState::Existing(cur as u64), new_metadata));
        }
        let new = alloc_mod::alloc_block(state)?;
        if zero_new_data {
            zero_block(state, new)?;
        }
        write_u32(buf, inner, new as u32);
        state.write_block(mid as u64, buf)?;
        return Ok((BlockAllocState::NewlyAllocated(new), new_metadata));
    }

    // 三级间接
    let rem = rem - p * p;
    let a = p * p;
    let (l3, mut new_metadata) = ensure_indirect_slot(state, i_block, 14, buf)?;
    let top_idx = rem / a;
    let mut top = read_u32(buf, top_idx);
    if top == 0 {
        top = alloc_mod::alloc_block(state)? as u32;
        new_metadata += 1;
        write_u32(buf, top_idx, top);
        state.write_block(l3, buf)?;
        cache_zero_block(state, top as u64, buf)?;
    } else {
        state.read_block(top as u64, buf)?;
    }
    let mid_idx = (rem % a) / p;
    let mut mid = read_u32(buf, mid_idx);
    if mid == 0 {
        mid = alloc_mod::alloc_block(state)? as u32;
        new_metadata += 1;
        write_u32(buf, mid_idx, mid);
        state.write_block(top as u64, buf)?;
        cache_zero_block(state, mid as u64, buf)?;
    } else {
        state.read_block(mid as u64, buf)?;
    }
    let inner = rem % p;
    let cur = read_u32(buf, inner);
    if cur != 0 {
        return Ok((BlockAllocState::Existing(cur as u64), new_metadata));
    }
    let new = alloc_mod::alloc_block(state)?;
    if zero_new_data {
        zero_block(state, new)?;
    }
    write_u32(buf, inner, new as u32);
    state.write_block(mid as u64, buf)?;
    Ok((BlockAllocState::NewlyAllocated(new), new_metadata))
}

/// 在间接块布局里写入一个既有数据块映射。
///
/// 补齐必要的索引块,用于从 extent
/// 布局保留数据地降级到传统 direct/indirect 布局。
pub(crate) fn set_existing_block(
    state: &FsState,
    i_block: &mut [u8],
    logical: u32,
    phys: u64,
) -> Result<u32, BlockBackendError> {
    let p = ppb(state.ext_sb.block_size);
    if logical < DIRECT_COUNT {
        write_u32(i_block, logical, phys as u32);
        return Ok(0);
    }

    let bs = state.ext_sb.block_size as usize;
    let mut buf = vec![0u8; bs];
    let rem = logical - DIRECT_COUNT;
    if rem < p {
        let (l1, new_metadata) = ensure_indirect_slot(state, i_block, 12, &mut buf)?;
        write_u32(&mut buf, rem, phys as u32);
        state.write_block(l1, &buf)?;
        return Ok(new_metadata);
    }

    let rem = rem - p;
    if rem < p * p {
        let (l2, mut new_metadata) = ensure_indirect_slot(state, i_block, 13, &mut buf)?;
        let mid_idx = rem / p;
        let mut mid = read_u32(&buf, mid_idx);
        if mid == 0 {
            mid = alloc_mod::alloc_block(state)? as u32;
            new_metadata += 1;
            write_u32(&mut buf, mid_idx, mid);
            state.write_block(l2, &buf)?;
            cache_zero_block(state, mid as u64, &mut buf)?;
        } else {
            state.read_block(mid as u64, &mut buf)?;
        }
        write_u32(&mut buf, rem % p, phys as u32);
        state.write_block(mid as u64, &buf)?;
        return Ok(new_metadata);
    }

    let rem = rem - p * p;
    let a = p * p;
    let (l3, mut new_metadata) = ensure_indirect_slot(state, i_block, 14, &mut buf)?;
    let top_idx = rem / a;
    let mut top = read_u32(&buf, top_idx);
    if top == 0 {
        top = alloc_mod::alloc_block(state)? as u32;
        new_metadata += 1;
        write_u32(&mut buf, top_idx, top);
        state.write_block(l3, &buf)?;
        cache_zero_block(state, top as u64, &mut buf)?;
    } else {
        state.read_block(top as u64, &mut buf)?;
    }
    let mid_idx = (rem % a) / p;
    let mut mid = read_u32(&buf, mid_idx);
    if mid == 0 {
        mid = alloc_mod::alloc_block(state)? as u32;
        new_metadata += 1;
        write_u32(&mut buf, mid_idx, mid);
        state.write_block(top as u64, &buf)?;
        cache_zero_block(state, mid as u64, &mut buf)?;
    } else {
        state.read_block(mid as u64, &mut buf)?;
    }
    write_u32(&mut buf, rem % p, phys as u32);
    state.write_block(mid as u64, &buf)?;
    Ok(new_metadata)
}

/// 处理一级间接块的分配/查找。`slot` 是 i_block 中索引(12 = 一级)。
/// 返回数据块物理号。`buf` 长度必须等于 block_size,内容会被覆盖。
fn alloc_or_walk_l1(
    state: &FsState,
    i_block: &mut [u8],
    slot: u32,
    inner: u32,
    buf: &mut [u8],
    zero_new_data: bool,
) -> Result<(BlockAllocState, u32), BlockBackendError> {
    let (l1, new_metadata) = ensure_indirect_slot(state, i_block, slot, buf)?;
    let cur = read_u32(buf, inner);
    if cur != 0 {
        return Ok((BlockAllocState::Existing(cur as u64), new_metadata));
    }
    let new = alloc_mod::alloc_block(state)?;
    if zero_new_data {
        zero_block(state, new)?;
    }
    write_u32(buf, inner, new as u32);
    state.write_block(l1, buf)?;
    Ok((BlockAllocState::NewlyAllocated(new), new_metadata))
}

/// 确保 i_block[slot] 指向一个存在的间接块。`buf` 出参为该间接块的内容。
/// 若新分配则写零并把 buf 设为全零。
fn ensure_indirect_slot(
    state: &FsState,
    i_block: &mut [u8],
    slot: u32,
    buf: &mut [u8],
) -> Result<(u64, u32), BlockBackendError> {
    let cur = read_u32(i_block, slot);
    if cur != 0 {
        state.read_block(cur as u64, buf)?;
        return Ok((cur as u64, 0));
    }
    let new = alloc_mod::alloc_block(state)?;
    write_u32(i_block, slot, new as u32);
    // 新块全零;直接把 buf 清零并写回磁盘
    for b in buf.iter_mut() {
        *b = 0;
    }
    state.write_block(new, buf)?;
    Ok((new, 1))
}

fn zero_block(state: &FsState, block: u64) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let z = vec![0u8; bs];
    state.write_block(block, &z)
}

/// 把 buf 清零并写回 block,同时填充缓存。供新分配的间接块走这条路径。
fn cache_zero_block(state: &FsState, block: u64, buf: &mut [u8]) -> Result<(), BlockBackendError> {
    for b in buf.iter_mut() {
        *b = 0;
    }
    state.write_block(block, buf)
}

#[inline]
fn resize_block_scratch(buf: &mut Vec<u8>, bs: usize) {
    if buf.len() != bs {
        buf.resize(bs, 0);
    }
}

/// 统计传统 direct/indirect 布局实际占用的文件系统块数。
///
/// ext4 的 `i_blocks` 记录的是已分配 512B sector 数,不是 `i_size` 四舍五入。
/// 因此 1 字节文件只要分配了一个 4KiB 数据块,也必须记 8 个 sector。
pub(crate) fn count_all_blocks(state: &FsState, i_block: &[u8]) -> Result<u64, BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let p = ppb(state.ext_sb.block_size);
    let mut total = 0u64;
    let mut l1_blk = Vec::new();
    let mut l2_blk = Vec::new();
    let mut l3_blk = Vec::new();
    let mut top_blk = Vec::new();
    let mut mid_blk = Vec::new();

    for i in 0..DIRECT_COUNT {
        if read_u32(i_block, i) != 0 {
            total += 1;
        }
    }

    let l1 = read_u32(i_block, 12);
    if l1 != 0 {
        total += 1; // 一级间接块本身
        resize_block_scratch(&mut l1_blk, bs);
        state.read_block(l1 as u64, &mut l1_blk)?;
        for i in 0..p {
            if read_u32(&l1_blk, i) != 0 {
                total += 1;
            }
        }
    }

    let l2 = read_u32(i_block, 13);
    if l2 != 0 {
        total += 1; // 二级间接块本身
        resize_block_scratch(&mut l2_blk, bs);
        resize_block_scratch(&mut mid_blk, bs);
        state.read_block(l2 as u64, &mut l2_blk)?;
        for i in 0..p {
            let mid = read_u32(&l2_blk, i);
            if mid == 0 {
                continue;
            }
            total += 1; // 中间一级间接块
            state.read_block(mid as u64, &mut mid_blk)?;
            for j in 0..p {
                if read_u32(&mid_blk, j) != 0 {
                    total += 1;
                }
            }
        }
    }

    let l3 = read_u32(i_block, 14);
    if l3 != 0 {
        total += 1; // 三级间接块本身
        resize_block_scratch(&mut l3_blk, bs);
        resize_block_scratch(&mut top_blk, bs);
        resize_block_scratch(&mut mid_blk, bs);
        state.read_block(l3 as u64, &mut l3_blk)?;
        for i in 0..p {
            let top = read_u32(&l3_blk, i);
            if top == 0 {
                continue;
            }
            total += 1; // 二级索引块
            state.read_block(top as u64, &mut top_blk)?;
            for j in 0..p {
                let mid = read_u32(&top_blk, j);
                if mid == 0 {
                    continue;
                }
                total += 1; // 一级索引块
                state.read_block(mid as u64, &mut mid_blk)?;
                for k in 0..p {
                    if read_u32(&mid_blk, k) != 0 {
                        total += 1;
                    }
                }
            }
        }
    }

    Ok(total)
}

/// 释放逻辑块号 ≥ `from_lb` 的所有间接指针对应的物理块。用于 partial truncate。
/// 处理完成后,间接块本身若变空(没有任何指向非零的项)也被释放。
pub(crate) fn free_blocks_from(
    state: &FsState,
    i_block: &mut [u8],
    from_lb: u32,
) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let p = ppb(state.ext_sb.block_size);
    let mut l1_blk = Vec::new();
    let mut l2_blk = Vec::new();
    let mut l3_blk = Vec::new();
    let mut top_blk = Vec::new();
    let mut mid_blk = Vec::new();
    let mut freed_blocks = Vec::new();

    // 直接块
    let direct_count = DIRECT_COUNT;
    for i in from_lb..direct_count {
        let b = read_u32(i_block, i);
        if b != 0 {
            freed_blocks.push(b as u64);
            write_u32(i_block, i, 0);
        }
    }
    if from_lb >= direct_count && from_lb < direct_count + p {
        // 一级间接里从某个索引开始释放
        let rel = from_lb - direct_count;
        let l1 = read_u32(i_block, 12);
        if l1 != 0 {
            resize_block_scratch(&mut l1_blk, bs);
            state.read_block(l1 as u64, &mut l1_blk)?;
            for i in rel..p {
                let b = read_u32(&l1_blk, i);
                if b != 0 {
                    freed_blocks.push(b as u64);
                    write_u32(&mut l1_blk, i, 0);
                }
            }
            if rel == 0 {
                // 整块都空了,连间接块自身也释放
                freed_blocks.push(l1 as u64);
                write_u32(i_block, 12, 0);
            } else {
                state.write_block(l1 as u64, &l1_blk)?;
            }
        }
    } else if from_lb < direct_count {
        // 完全释放一级间接
        let l1 = read_u32(i_block, 12);
        if l1 != 0 {
            resize_block_scratch(&mut l1_blk, bs);
            state.read_block(l1 as u64, &mut l1_blk)?;
            for i in 0..p {
                let b = read_u32(&l1_blk, i);
                if b != 0 {
                    freed_blocks.push(b as u64);
                }
            }
            freed_blocks.push(l1 as u64);
            write_u32(i_block, 12, 0);
        }
    }

    let l2_base = direct_count + p;
    let l2_end = l2_base + p * p;
    if from_lb >= l2_base && from_lb < l2_end {
        let rel = from_lb - l2_base;
        let mid_idx_start = rel / p;
        let inner_start = rel % p;
        let l2 = read_u32(i_block, 13);
        if l2 != 0 {
            resize_block_scratch(&mut l2_blk, bs);
            resize_block_scratch(&mut mid_blk, bs);
            state.read_block(l2 as u64, &mut l2_blk)?;
            for i in mid_idx_start..p {
                let mid = read_u32(&l2_blk, i);
                if mid == 0 {
                    continue;
                }
                let start_in_mid = if i == mid_idx_start { inner_start } else { 0 };
                state.read_block(mid as u64, &mut mid_blk)?;
                for j in start_in_mid..p {
                    let b = read_u32(&mid_blk, j);
                    if b != 0 {
                        freed_blocks.push(b as u64);
                        write_u32(&mut mid_blk, j, 0);
                    }
                }
                if start_in_mid == 0 {
                    freed_blocks.push(mid as u64);
                    write_u32(&mut l2_blk, i, 0);
                } else {
                    state.write_block(mid as u64, &mid_blk)?;
                }
            }
            if mid_idx_start == 0 && inner_start == 0 {
                freed_blocks.push(l2 as u64);
                write_u32(i_block, 13, 0);
            } else {
                state.write_block(l2 as u64, &l2_blk)?;
            }
        }
    } else if from_lb < l2_base {
        // 完全释放二级
        let l2 = read_u32(i_block, 13);
        if l2 != 0 {
            resize_block_scratch(&mut l2_blk, bs);
            resize_block_scratch(&mut mid_blk, bs);
            state.read_block(l2 as u64, &mut l2_blk)?;
            for i in 0..p {
                let mid = read_u32(&l2_blk, i);
                if mid == 0 {
                    continue;
                }
                state.read_block(mid as u64, &mut mid_blk)?;
                for j in 0..p {
                    let b = read_u32(&mid_blk, j);
                    if b != 0 {
                        freed_blocks.push(b as u64);
                    }
                }
                freed_blocks.push(mid as u64);
            }
            freed_blocks.push(l2 as u64);
            write_u32(i_block, 13, 0);
        }
    }

    // 三级间接超出范围极少见,partial truncate 覆盖此区仍做全量扫描
    let l3_base = l2_end;
    if from_lb >= l3_base {
        let rel = from_lb - l3_base;
        let a = p * p;
        let top_idx_start = rel / a;
        let mid_idx_start = (rel % a) / p;
        let inner_start = rel % p;
        let l3 = read_u32(i_block, 14);
        if l3 != 0 {
            resize_block_scratch(&mut l3_blk, bs);
            resize_block_scratch(&mut top_blk, bs);
            resize_block_scratch(&mut mid_blk, bs);
            state.read_block(l3 as u64, &mut l3_blk)?;
            for ti in top_idx_start..p {
                let top = read_u32(&l3_blk, ti);
                if top == 0 {
                    continue;
                }
                let mid_start_here = if ti == top_idx_start {
                    mid_idx_start
                } else {
                    0
                };
                let inner_start_here_base = if ti == top_idx_start { inner_start } else { 0 };
                state.read_block(top as u64, &mut top_blk)?;
                for mi in mid_start_here..p {
                    let mid = read_u32(&top_blk, mi);
                    if mid == 0 {
                        continue;
                    }
                    let inner_start_here = if ti == top_idx_start && mi == mid_idx_start {
                        inner_start_here_base
                    } else {
                        0
                    };
                    state.read_block(mid as u64, &mut mid_blk)?;
                    for k in inner_start_here..p {
                        let b = read_u32(&mid_blk, k);
                        if b != 0 {
                            freed_blocks.push(b as u64);
                            write_u32(&mut mid_blk, k, 0);
                        }
                    }
                    if inner_start_here == 0 {
                        freed_blocks.push(mid as u64);
                        write_u32(&mut top_blk, mi, 0);
                    } else {
                        state.write_block(mid as u64, &mid_blk)?;
                    }
                }
                if mid_start_here == 0 && inner_start_here_base == 0 {
                    freed_blocks.push(top as u64);
                    write_u32(&mut l3_blk, ti, 0);
                } else {
                    state.write_block(top as u64, &top_blk)?;
                }
            }
            if top_idx_start == 0 && mid_idx_start == 0 && inner_start == 0 {
                freed_blocks.push(l3 as u64);
                write_u32(i_block, 14, 0);
            } else {
                state.write_block(l3 as u64, &l3_blk)?;
            }
        }
    } else {
        // from_lb 在三级前:整个三级间接释放
        let l3 = read_u32(i_block, 14);
        if l3 != 0 {
            resize_block_scratch(&mut l3_blk, bs);
            resize_block_scratch(&mut top_blk, bs);
            resize_block_scratch(&mut mid_blk, bs);
            state.read_block(l3 as u64, &mut l3_blk)?;
            for i in 0..p {
                let top = read_u32(&l3_blk, i);
                if top == 0 {
                    continue;
                }
                state.read_block(top as u64, &mut top_blk)?;
                for j in 0..p {
                    let mid = read_u32(&top_blk, j);
                    if mid == 0 {
                        continue;
                    }
                    state.read_block(mid as u64, &mut mid_blk)?;
                    for k in 0..p {
                        let b = read_u32(&mid_blk, k);
                        if b != 0 {
                            freed_blocks.push(b as u64);
                        }
                    }
                    freed_blocks.push(mid as u64);
                }
                freed_blocks.push(top as u64);
            }
            freed_blocks.push(l3 as u64);
            write_u32(i_block, 14, 0);
        }
    }
    crate::alloc_mod::free_blocks_sparse(state, &freed_blocks)?;
    Ok(())
}

/// 释放 `i_block` 描述的整个文件的所有数据块(含间接块本身)。
pub(crate) fn free_all_blocks(
    state: &FsState,
    i_block: &mut [u8],
) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let p = ppb(state.ext_sb.block_size);
    let mut l1_blk = Vec::new();
    let mut l2_blk = Vec::new();
    let mut l3_blk = Vec::new();
    let mut top_blk = Vec::new();
    let mut mid_blk = Vec::new();
    let mut freed_blocks = Vec::new();

    // 直接块
    for i in 0..DIRECT_COUNT {
        let b = read_u32(i_block, i);
        if b != 0 {
            freed_blocks.push(b as u64);
            write_u32(i_block, i, 0);
        }
    }
    // 一级
    let l1 = read_u32(i_block, 12);
    if l1 != 0 {
        resize_block_scratch(&mut l1_blk, bs);
        state.read_block(l1 as u64, &mut l1_blk)?;
        for i in 0..p {
            let b = read_u32(&l1_blk, i);
            if b != 0 {
                freed_blocks.push(b as u64);
            }
        }
        freed_blocks.push(l1 as u64);
        write_u32(i_block, 12, 0);
    }
    // 二级
    let l2 = read_u32(i_block, 13);
    if l2 != 0 {
        resize_block_scratch(&mut l2_blk, bs);
        resize_block_scratch(&mut mid_blk, bs);
        state.read_block(l2 as u64, &mut l2_blk)?;
        for i in 0..p {
            let mid = read_u32(&l2_blk, i);
            if mid == 0 {
                continue;
            }
            state.read_block(mid as u64, &mut mid_blk)?;
            for j in 0..p {
                let b = read_u32(&mid_blk, j);
                if b != 0 {
                    freed_blocks.push(b as u64);
                }
            }
            freed_blocks.push(mid as u64);
        }
        freed_blocks.push(l2 as u64);
        write_u32(i_block, 13, 0);
    }
    // 三级
    let l3 = read_u32(i_block, 14);
    if l3 != 0 {
        resize_block_scratch(&mut l3_blk, bs);
        resize_block_scratch(&mut top_blk, bs);
        resize_block_scratch(&mut mid_blk, bs);
        state.read_block(l3 as u64, &mut l3_blk)?;
        for i in 0..p {
            let top = read_u32(&l3_blk, i);
            if top == 0 {
                continue;
            }
            state.read_block(top as u64, &mut top_blk)?;
            for j in 0..p {
                let mid = read_u32(&top_blk, j);
                if mid == 0 {
                    continue;
                }
                state.read_block(mid as u64, &mut mid_blk)?;
                for k in 0..p {
                    let b = read_u32(&mid_blk, k);
                    if b != 0 {
                        freed_blocks.push(b as u64);
                    }
                }
                freed_blocks.push(mid as u64);
            }
            freed_blocks.push(top as u64);
        }
        freed_blocks.push(l3 as u64);
        write_u32(i_block, 14, 0);
    }
    alloc_mod::free_blocks_sparse(state, &freed_blocks)?;
    Ok(())
}
