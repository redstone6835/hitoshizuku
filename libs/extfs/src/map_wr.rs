//! 间接块写路径:扩容(按需分配)/ 截断(释放)。
//!
//! `i_block[0..60]` 布局与 [`map`](crate::map) 相同。所有修改在内存副本 `i_block`
//! 上完成,由调用方负责把它写回 inode。

use alloc::vec;

use crate::alloc_mod;
use crate::state::{BlockBackendError, FsState};

const DIRECT_COUNT: u32 = 12;

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
    let p = ppb(state.ext_sb.block_size);
    if logical < DIRECT_COUNT {
        let cur = read_u32(i_block, logical);
        if cur != 0 {
            return Ok(cur as u64);
        }
        let new = alloc_mod::alloc_block(state)?;
        zero_block(state, new)?;
        write_u32(i_block, logical, new as u32);
        return Ok(new);
    }

    let bs = state.ext_sb.block_size as usize;
    // 各级间接块共用同一个 scratch buffer,避免重复堆分配。
    let mut buf = vec![0u8; bs];
    let rem = logical - DIRECT_COUNT;
    if rem < p {
        let new_data = alloc_or_walk_l1(state, i_block, 12, rem, &mut buf)?;
        return Ok(new_data);
    }

    let rem = rem - p;
    if rem < p * p {
        // 二级间接:先确保 L2 索引块存在,把它读到 buf
        let l2 = ensure_indirect_slot(state, i_block, 13, &mut buf)?;
        let mid_idx = rem / p;
        let mut mid = read_u32(&buf, mid_idx);
        if mid == 0 {
            mid = alloc_mod::alloc_block(state)? as u32;
            // 新 mid 块逻辑上为零,直接写零到磁盘后续不再 read_block
            write_u32(&mut buf, mid_idx, mid);
            state.write_block(l2, &buf)?;
            // mid 块需要置零 (因为我们随后会读它),走 cache insert
            cache_zero_block(state, mid as u64, &mut buf)?;
        } else {
            state.read_block(mid as u64, &mut buf)?;
        }
        let inner = rem % p;
        let cur = read_u32(&buf, inner);
        if cur != 0 {
            return Ok(cur as u64);
        }
        let new = alloc_mod::alloc_block(state)?;
        zero_block(state, new)?;
        write_u32(&mut buf, inner, new as u32);
        state.write_block(mid as u64, &buf)?;
        return Ok(new);
    }

    // 三级间接
    let rem = rem - p * p;
    let a = p * p;
    let l3 = ensure_indirect_slot(state, i_block, 14, &mut buf)?;
    let top_idx = rem / a;
    let mut top = read_u32(&buf, top_idx);
    if top == 0 {
        top = alloc_mod::alloc_block(state)? as u32;
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
        write_u32(&mut buf, mid_idx, mid);
        state.write_block(top as u64, &buf)?;
        cache_zero_block(state, mid as u64, &mut buf)?;
    } else {
        state.read_block(mid as u64, &mut buf)?;
    }
    let inner = rem % p;
    let cur = read_u32(&buf, inner);
    if cur != 0 {
        return Ok(cur as u64);
    }
    let new = alloc_mod::alloc_block(state)?;
    zero_block(state, new)?;
    write_u32(&mut buf, inner, new as u32);
    state.write_block(mid as u64, &buf)?;
    Ok(new)
}

/// 处理一级间接块的分配/查找。`slot` 是 i_block 中索引(12 = 一级)。
/// 返回数据块物理号。`buf` 长度必须等于 block_size,内容会被覆盖。
fn alloc_or_walk_l1(
    state: &FsState,
    i_block: &mut [u8],
    slot: u32,
    inner: u32,
    buf: &mut [u8],
) -> Result<u64, BlockBackendError> {
    let l1 = ensure_indirect_slot(state, i_block, slot, buf)?;
    let cur = read_u32(buf, inner);
    if cur != 0 {
        return Ok(cur as u64);
    }
    let new = alloc_mod::alloc_block(state)?;
    zero_block(state, new)?;
    write_u32(buf, inner, new as u32);
    state.write_block(l1, buf)?;
    Ok(new)
}

/// 确保 i_block[slot] 指向一个存在的间接块。`buf` 出参为该间接块的内容。
/// 若新分配则写零并把 buf 设为全零。
fn ensure_indirect_slot(
    state: &FsState,
    i_block: &mut [u8],
    slot: u32,
    buf: &mut [u8],
) -> Result<u64, BlockBackendError> {
    let cur = read_u32(i_block, slot);
    if cur != 0 {
        state.read_block(cur as u64, buf)?;
        return Ok(cur as u64);
    }
    let new = alloc_mod::alloc_block(state)?;
    write_u32(i_block, slot, new as u32);
    // 新块全零;直接把 buf 清零并写回磁盘
    for b in buf.iter_mut() {
        *b = 0;
    }
    state.write_block(new, buf)?;
    Ok(new)
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

/// 释放逻辑块号 ≥ `from_lb` 的所有间接指针对应的物理块。用于 partial truncate。
/// 处理完成后,间接块本身若变空(没有任何指向非零的项)也被释放。
pub(crate) fn free_blocks_from(
    state: &FsState,
    i_block: &mut [u8],
    from_lb: u32,
) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let p = ppb(state.ext_sb.block_size);

    // 直接块
    let direct_count = DIRECT_COUNT;
    for i in from_lb..direct_count {
        let b = read_u32(i_block, i);
        if b != 0 {
            crate::alloc_mod::free_block(state, b as u64)?;
            write_u32(i_block, i, 0);
        }
    }
    if from_lb >= direct_count && from_lb < direct_count + p {
        // 一级间接里从某个索引开始释放
        let rel = from_lb - direct_count;
        let l1 = read_u32(i_block, 12);
        if l1 != 0 {
            let mut blk = vec![0u8; bs];
            state.read_block(l1 as u64, &mut blk)?;
            for i in rel..p {
                let b = read_u32(&blk, i);
                if b != 0 {
                    crate::alloc_mod::free_block(state, b as u64)?;
                    write_u32(&mut blk, i, 0);
                }
            }
            if rel == 0 {
                // 整块都空了,连间接块自身也释放
                crate::alloc_mod::free_block(state, l1 as u64)?;
                write_u32(i_block, 12, 0);
            } else {
                state.write_block(l1 as u64, &blk)?;
            }
        }
    } else if from_lb < direct_count {
        // 完全释放一级间接
        let l1 = read_u32(i_block, 12);
        if l1 != 0 {
            let mut blk = vec![0u8; bs];
            state.read_block(l1 as u64, &mut blk)?;
            for i in 0..p {
                let b = read_u32(&blk, i);
                if b != 0 {
                    crate::alloc_mod::free_block(state, b as u64)?;
                }
            }
            crate::alloc_mod::free_block(state, l1 as u64)?;
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
            let mut l2_blk = vec![0u8; bs];
            state.read_block(l2 as u64, &mut l2_blk)?;
            for i in mid_idx_start..p {
                let mid = read_u32(&l2_blk, i);
                if mid == 0 {
                    continue;
                }
                let start_in_mid = if i == mid_idx_start { inner_start } else { 0 };
                let mut mid_blk = vec![0u8; bs];
                state.read_block(mid as u64, &mut mid_blk)?;
                for j in start_in_mid..p {
                    let b = read_u32(&mid_blk, j);
                    if b != 0 {
                        crate::alloc_mod::free_block(state, b as u64)?;
                        write_u32(&mut mid_blk, j, 0);
                    }
                }
                if start_in_mid == 0 {
                    crate::alloc_mod::free_block(state, mid as u64)?;
                    write_u32(&mut l2_blk, i, 0);
                } else {
                    state.write_block(mid as u64, &mid_blk)?;
                }
            }
            if mid_idx_start == 0 && inner_start == 0 {
                crate::alloc_mod::free_block(state, l2 as u64)?;
                write_u32(i_block, 13, 0);
            } else {
                state.write_block(l2 as u64, &l2_blk)?;
            }
        }
    } else if from_lb < l2_base {
        // 完全释放二级
        let l2 = read_u32(i_block, 13);
        if l2 != 0 {
            let mut l2_blk = vec![0u8; bs];
            state.read_block(l2 as u64, &mut l2_blk)?;
            for i in 0..p {
                let mid = read_u32(&l2_blk, i);
                if mid == 0 {
                    continue;
                }
                let mut mid_blk = vec![0u8; bs];
                state.read_block(mid as u64, &mut mid_blk)?;
                for j in 0..p {
                    let b = read_u32(&mid_blk, j);
                    if b != 0 {
                        crate::alloc_mod::free_block(state, b as u64)?;
                    }
                }
                crate::alloc_mod::free_block(state, mid as u64)?;
            }
            crate::alloc_mod::free_block(state, l2 as u64)?;
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
            let mut l3_blk = vec![0u8; bs];
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
                let mut top_blk = vec![0u8; bs];
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
                    let mut mid_blk = vec![0u8; bs];
                    state.read_block(mid as u64, &mut mid_blk)?;
                    for k in inner_start_here..p {
                        let b = read_u32(&mid_blk, k);
                        if b != 0 {
                            crate::alloc_mod::free_block(state, b as u64)?;
                            write_u32(&mut mid_blk, k, 0);
                        }
                    }
                    if inner_start_here == 0 {
                        crate::alloc_mod::free_block(state, mid as u64)?;
                        write_u32(&mut top_blk, mi, 0);
                    } else {
                        state.write_block(mid as u64, &mid_blk)?;
                    }
                }
                if mid_start_here == 0 && inner_start_here_base == 0 {
                    crate::alloc_mod::free_block(state, top as u64)?;
                    write_u32(&mut l3_blk, ti, 0);
                } else {
                    state.write_block(top as u64, &top_blk)?;
                }
            }
            if top_idx_start == 0 && mid_idx_start == 0 && inner_start == 0 {
                crate::alloc_mod::free_block(state, l3 as u64)?;
                write_u32(i_block, 14, 0);
            } else {
                state.write_block(l3 as u64, &l3_blk)?;
            }
        }
    } else {
        // from_lb 在三级前:整个三级间接释放
        let l3 = read_u32(i_block, 14);
        if l3 != 0 {
            let mut l3_blk = vec![0u8; bs];
            state.read_block(l3 as u64, &mut l3_blk)?;
            for i in 0..p {
                let top = read_u32(&l3_blk, i);
                if top == 0 {
                    continue;
                }
                let mut top_blk = vec![0u8; bs];
                state.read_block(top as u64, &mut top_blk)?;
                for j in 0..p {
                    let mid = read_u32(&top_blk, j);
                    if mid == 0 {
                        continue;
                    }
                    let mut mid_blk = vec![0u8; bs];
                    state.read_block(mid as u64, &mut mid_blk)?;
                    for k in 0..p {
                        let b = read_u32(&mid_blk, k);
                        if b != 0 {
                            crate::alloc_mod::free_block(state, b as u64)?;
                        }
                    }
                    crate::alloc_mod::free_block(state, mid as u64)?;
                }
                crate::alloc_mod::free_block(state, top as u64)?;
            }
            crate::alloc_mod::free_block(state, l3 as u64)?;
            write_u32(i_block, 14, 0);
        }
    }
    Ok(())
}

/// 释放 `i_block` 描述的整个文件的所有数据块(含间接块本身)。
pub(crate) fn free_all_blocks(
    state: &FsState,
    i_block: &mut [u8],
) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let p = ppb(state.ext_sb.block_size);

    // 直接块
    for i in 0..DIRECT_COUNT {
        let b = read_u32(i_block, i);
        if b != 0 {
            alloc_mod::free_block(state, b as u64)?;
            write_u32(i_block, i, 0);
        }
    }
    // 一级
    let l1 = read_u32(i_block, 12);
    if l1 != 0 {
        let mut blk = vec![0u8; bs];
        state.read_block(l1 as u64, &mut blk)?;
        for i in 0..p {
            let b = read_u32(&blk, i);
            if b != 0 {
                alloc_mod::free_block(state, b as u64)?;
            }
        }
        alloc_mod::free_block(state, l1 as u64)?;
        write_u32(i_block, 12, 0);
    }
    // 二级
    let l2 = read_u32(i_block, 13);
    if l2 != 0 {
        let mut l2_blk = vec![0u8; bs];
        state.read_block(l2 as u64, &mut l2_blk)?;
        for i in 0..p {
            let mid = read_u32(&l2_blk, i);
            if mid == 0 {
                continue;
            }
            let mut mid_blk = vec![0u8; bs];
            state.read_block(mid as u64, &mut mid_blk)?;
            for j in 0..p {
                let b = read_u32(&mid_blk, j);
                if b != 0 {
                    alloc_mod::free_block(state, b as u64)?;
                }
            }
            alloc_mod::free_block(state, mid as u64)?;
        }
        alloc_mod::free_block(state, l2 as u64)?;
        write_u32(i_block, 13, 0);
    }
    // 三级
    let l3 = read_u32(i_block, 14);
    if l3 != 0 {
        let mut l3_blk = vec![0u8; bs];
        state.read_block(l3 as u64, &mut l3_blk)?;
        for i in 0..p {
            let top = read_u32(&l3_blk, i);
            if top == 0 {
                continue;
            }
            let mut top_blk = vec![0u8; bs];
            state.read_block(top as u64, &mut top_blk)?;
            for j in 0..p {
                let mid = read_u32(&top_blk, j);
                if mid == 0 {
                    continue;
                }
                let mut mid_blk = vec![0u8; bs];
                state.read_block(mid as u64, &mut mid_blk)?;
                for k in 0..p {
                    let b = read_u32(&mid_blk, k);
                    if b != 0 {
                        alloc_mod::free_block(state, b as u64)?;
                    }
                }
                alloc_mod::free_block(state, mid as u64)?;
            }
            alloc_mod::free_block(state, top as u64)?;
        }
        alloc_mod::free_block(state, l3 as u64)?;
        write_u32(i_block, 14, 0);
    }
    Ok(())
}
