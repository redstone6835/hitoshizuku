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
    let rem = logical - DIRECT_COUNT;
    if rem < p {
        let mut l1 = read_u32(i_block, 12);
        if l1 == 0 {
            l1 = alloc_mod::alloc_block(state)? as u32;
            zero_block(state, l1 as u64)?;
            write_u32(i_block, 12, l1);
        }
        let mut blk = vec![0u8; bs];
        state.read_block(l1 as u64, &mut blk)?;
        let cur = read_u32(&blk, rem);
        if cur != 0 {
            return Ok(cur as u64);
        }
        let new = alloc_mod::alloc_block(state)?;
        zero_block(state, new)?;
        write_u32(&mut blk, rem, new as u32);
        state.write_block(l1 as u64, &blk)?;
        return Ok(new);
    }

    let rem = rem - p;
    if rem < p * p {
        let mut l2 = read_u32(i_block, 13);
        if l2 == 0 {
            l2 = alloc_mod::alloc_block(state)? as u32;
            zero_block(state, l2 as u64)?;
            write_u32(i_block, 13, l2);
        }
        let mut l2_blk = vec![0u8; bs];
        state.read_block(l2 as u64, &mut l2_blk)?;
        let mid_idx = rem / p;
        let mut mid = read_u32(&l2_blk, mid_idx);
        if mid == 0 {
            mid = alloc_mod::alloc_block(state)? as u32;
            zero_block(state, mid as u64)?;
            write_u32(&mut l2_blk, mid_idx, mid);
            state.write_block(l2 as u64, &l2_blk)?;
        }
        let mut mid_blk = vec![0u8; bs];
        state.read_block(mid as u64, &mut mid_blk)?;
        let cur = read_u32(&mid_blk, rem % p);
        if cur != 0 {
            return Ok(cur as u64);
        }
        let new = alloc_mod::alloc_block(state)?;
        zero_block(state, new)?;
        write_u32(&mut mid_blk, rem % p, new as u32);
        state.write_block(mid as u64, &mid_blk)?;
        return Ok(new);
    }

    let rem = rem - p * p;
    let mut l3 = read_u32(i_block, 14);
    if l3 == 0 {
        l3 = alloc_mod::alloc_block(state)? as u32;
        zero_block(state, l3 as u64)?;
        write_u32(i_block, 14, l3);
    }
    let a = p * p;
    let mut l3_blk = vec![0u8; bs];
    state.read_block(l3 as u64, &mut l3_blk)?;
    let top_idx = rem / a;
    let mut top = read_u32(&l3_blk, top_idx);
    if top == 0 {
        top = alloc_mod::alloc_block(state)? as u32;
        zero_block(state, top as u64)?;
        write_u32(&mut l3_blk, top_idx, top);
        state.write_block(l3 as u64, &l3_blk)?;
    }
    let mut top_blk = vec![0u8; bs];
    state.read_block(top as u64, &mut top_blk)?;
    let mid_idx = (rem % a) / p;
    let mut mid = read_u32(&top_blk, mid_idx);
    if mid == 0 {
        mid = alloc_mod::alloc_block(state)? as u32;
        zero_block(state, mid as u64)?;
        write_u32(&mut top_blk, mid_idx, mid);
        state.write_block(top as u64, &top_blk)?;
    }
    let mut mid_blk = vec![0u8; bs];
    state.read_block(mid as u64, &mut mid_blk)?;
    let cur = read_u32(&mid_blk, rem % p);
    if cur != 0 {
        return Ok(cur as u64);
    }
    let new = alloc_mod::alloc_block(state)?;
    zero_block(state, new)?;
    write_u32(&mut mid_blk, rem % p, new as u32);
    state.write_block(mid as u64, &mid_blk)?;
    Ok(new)
}

fn zero_block(state: &FsState, block: u64) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let z = vec![0u8; bs];
    state.write_block(block, &z)
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
