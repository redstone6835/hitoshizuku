//! 传统 ext2/3 间接块寻址(直接/单/双/三重间接)。
//!
//! inode 的 `i_block[0..15]` 布局:
//! - `[0..12]`:直接块指针(12 个)
//! - `[12]`:一级间接(块内存 `bs/4` 个块号)
//! - `[13]`:二级间接
//! - `[14]`:三级间接
//!
//! 块号是 32 位小端。若指针为 0,表示逻辑块号映射为"洞"(返回 0)。

use alloc::vec;
use alloc::vec::Vec;

use crate::state::{BlockBackendError, FsState};

const DIRECT_COUNT: u32 = 12;

#[inline(always)]
fn ptrs_per_block(block_size: u32) -> u32 {
    block_size / 4
}

#[inline(always)]
fn read_u32_le(p: *const u8, off: usize) -> u32 {
    u32::from_le_bytes(unsafe {
        [
            *p.add(off),
            *p.add(off + 1),
            *p.add(off + 2),
            *p.add(off + 3),
        ]
    })
}

/// 解析一个指向间接块的块号(0 表示洞)。
fn read_u32_from_block(state: &FsState, block: u64, index: u32) -> Result<u32, BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    if block == 0 {
        return Ok(0);
    }
    let mut buf = vec![0u8; bs];
    state.read_block(block, &mut buf)?;
    let off = (index as usize) * 4;
    if off + 4 > bs {
        return Err(BlockBackendError::OutOfRange);
    }
    Ok(read_u32_le(buf.as_ptr(), off))
}

/// 解析 `i_block[..]` 的直接指针(`logical_block < 12`)。
#[inline(always)]
fn direct_ptr(i_block: &[u8], idx: u32) -> u32 {
    let off = (idx as usize) * 4;
    read_u32_le(i_block.as_ptr(), off)
}

/// 将逻辑块号映射到物理块号。返回 `Ok(None)` 表示洞。
///
/// `i_block` 必须是 inode 的 60 字节 i_block 区域(或等价副本)。
#[inline]
pub(crate) fn map_block(
    state: &FsState,
    i_block: &[u8],
    logical_block: u32,
) -> Result<Option<u64>, BlockBackendError> {
    let ppb = ptrs_per_block(state.ext_sb.block_size);
    if logical_block < DIRECT_COUNT {
        let p = direct_ptr(i_block, logical_block);
        return Ok(if p == 0 { None } else { Some(p as u64) });
    }
    // 一级
    let rem = logical_block - DIRECT_COUNT;
    if rem < ppb {
        let l1 = direct_ptr(i_block, 12);
        if l1 == 0 {
            return Ok(None);
        }
        let p = read_u32_from_block(state, l1 as u64, rem)?;
        return Ok(if p == 0 { None } else { Some(p as u64) });
    }
    // 二级
    let rem = rem - ppb;
    if rem < ppb * ppb {
        let l2 = direct_ptr(i_block, 13);
        if l2 == 0 {
            return Ok(None);
        }
        let mid = read_u32_from_block(state, l2 as u64, rem / ppb)?;
        if mid == 0 {
            return Ok(None);
        }
        let p = read_u32_from_block(state, mid as u64, rem % ppb)?;
        return Ok(if p == 0 { None } else { Some(p as u64) });
    }
    // 三级
    let rem = rem - ppb * ppb;
    let l3 = direct_ptr(i_block, 14);
    if l3 == 0 {
        return Ok(None);
    }
    let a = ppb * ppb;
    let top = read_u32_from_block(state, l3 as u64, rem / a)?;
    if top == 0 {
        return Ok(None);
    }
    let mid = read_u32_from_block(state, top as u64, (rem % a) / ppb)?;
    if mid == 0 {
        return Ok(None);
    }
    let p = read_u32_from_block(state, mid as u64, rem % ppb)?;
    Ok(if p == 0 { None } else { Some(p as u64) })
}

#[inline]
fn read_indirect_block(state: &FsState, block: u64) -> Result<Vec<u32>, BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let mut buf = vec![0u8; bs];
    state.read_block(block, &mut buf)?;
    let count = bs / 4;
    let mut ptrs = Vec::with_capacity(count);
    let p = buf.as_ptr();
    for i in 0..count {
        ptrs.push(read_u32_le(p, i * 4));
    }
    Ok(ptrs)
}

#[inline]
fn push_range(
    ranges: &mut Vec<(u32, u32, u64)>,
    lb: u32,
    phys: u64,
    prev_lb: &mut Option<u32>,
    prev_phys: &mut Option<u64>,
    run_len: &mut u32,
) {
    match (*prev_lb, *prev_phys) {
        (Some(plb), Some(pphys)) if plb + *run_len == lb && pphys + *run_len as u64 == phys => {
            *run_len += 1;
        }
        _ => {
            if let (Some(plb), Some(pphys)) = (*prev_lb, *prev_phys) {
                ranges.push((plb, *run_len, pphys));
            }
            *prev_lb = Some(lb);
            *prev_phys = Some(phys);
            *run_len = 1;
        }
    }
}

#[inline]
fn flush_run(
    ranges: &mut Vec<(u32, u32, u64)>,
    prev_lb: &mut Option<u32>,
    prev_phys: &mut Option<u64>,
    run_len: &mut u32,
) {
    if let (Some(plb), Some(pphys)) = (*prev_lb, *prev_phys) {
        ranges.push((plb, *run_len, pphys));
    }
    *prev_lb = None;
    *prev_phys = None;
    *run_len = 0;
}

#[inline]
pub(crate) fn map_contiguous(
    state: &FsState,
    i_block: &[u8],
    start_lb: u32,
    count: u32,
) -> Result<Vec<(u32, u32, u64)>, BlockBackendError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let ppb = ptrs_per_block(state.ext_sb.block_size);
    let end_lb = start_lb + count;
    let mut ranges = Vec::new();

    let bound1 = DIRECT_COUNT;
    let bound2 = bound1 + ppb;
    let bound3 = bound2 + ppb * ppb;

    // 直接块范围:逐块比较连续性
    let direct_end = end_lb.min(bound1);
    if start_lb < direct_end {
        collect_direct_runs(&mut ranges, i_block, start_lb, direct_end);
    }

    // 一级间接
    if end_lb > bound1 && start_lb < bound2 {
        let l1 = direct_ptr(i_block, 12);
        let seg_start = start_lb.max(bound1);
        let seg_end = end_lb.min(bound2);
        if l1 != 0 {
            let indirect = read_indirect_block(state, l1 as u64)?;
            let start_idx = (seg_start - bound1) as usize;
            let end_idx = (seg_end - bound1) as usize;
            collect_ptr_runs(&mut ranges, &indirect, seg_start, start_idx, end_idx);
        }
    }

    // 二级间接
    if end_lb > bound2 && start_lb < bound3 {
        let l2 = direct_ptr(i_block, 13);
        let seg_start = start_lb.max(bound2);
        let seg_end = end_lb.min(bound3);
        if l2 != 0 {
            let l2_ptrs = read_indirect_block(state, l2 as u64)?;
            let first_mid = (seg_start - bound2) / ppb;
            let last_mid = ((seg_end - 1) - bound2) / ppb;
            for mid_idx in first_mid..=last_mid {
                let mid = l2_ptrs[mid_idx as usize];
                let lb_base = bound2 + mid_idx * ppb;
                let s = seg_start.max(lb_base);
                let e = seg_end.min(lb_base + ppb);
                let start_idx = (s - lb_base) as usize;
                let end_idx = (e - lb_base) as usize;
                if mid != 0 {
                    let indirect = read_indirect_block(state, mid as u64)?;
                    collect_ptr_runs(&mut ranges, &indirect, s, start_idx, end_idx);
                }
            }
        }
    }

    // 三级间接
    if end_lb > bound3 {
        let l3 = direct_ptr(i_block, 14);
        let seg_start = start_lb.max(bound3);
        if l3 != 0 {
            let a = ppb * ppb;
            let l3_ptrs = read_indirect_block(state, l3 as u64)?;
            let first_top = (seg_start - bound3) / a;
            let last_top = ((end_lb - 1) - bound3) / a;
            for top_idx in first_top..=last_top {
                let top = l3_ptrs[top_idx as usize];
                let top_base = bound3 + top_idx * a;
                let ts = seg_start.max(top_base);
                let te = end_lb.min(top_base + a);
                if top == 0 {
                    continue;
                }
                let top_ptrs = read_indirect_block(state, top as u64)?;
                let first_mid = (ts - top_base) / ppb;
                let last_mid = ((te - 1) - top_base) / ppb;
                for mid_idx in first_mid..=last_mid {
                    let mid = top_ptrs[mid_idx as usize];
                    let lb_base = top_base + mid_idx * ppb;
                    let s = ts.max(lb_base);
                    let e = te.min(lb_base + ppb);
                    let start_idx = (s - lb_base) as usize;
                    let end_idx = (e - lb_base) as usize;
                    if mid != 0 {
                        let indirect = read_indirect_block(state, mid as u64)?;
                        collect_ptr_runs(&mut ranges, &indirect, s, start_idx, end_idx);
                    }
                }
            }
        }
    }

    Ok(ranges)
}

#[inline]
fn collect_direct_runs(ranges: &mut Vec<(u32, u32, u64)>, i_block: &[u8], start: u32, end: u32) {
    let mut prev_lb: Option<u32> = None;
    let mut prev_phys: Option<u64> = None;
    let mut run_len: u32 = 0;
    for lb in start..end {
        let p = direct_ptr(i_block, lb);
        if p != 0 {
            push_range(
                ranges,
                lb,
                p as u64,
                &mut prev_lb,
                &mut prev_phys,
                &mut run_len,
            );
        } else {
            flush_run(ranges, &mut prev_lb, &mut prev_phys, &mut run_len);
        }
    }
    flush_run(ranges, &mut prev_lb, &mut prev_phys, &mut run_len);
}

#[inline]
fn collect_ptr_runs(
    ranges: &mut Vec<(u32, u32, u64)>,
    ptrs: &[u32],
    lb_origin: u32,
    start_idx: usize,
    end_idx: usize,
) {
    let mut prev_lb: Option<u32> = None;
    let mut prev_phys: Option<u64> = None;
    let mut run_len: u32 = 0;
    for idx in start_idx..end_idx {
        let p = ptrs[idx];
        let lb = lb_origin + (idx - start_idx) as u32;
        if p != 0 {
            push_range(
                ranges,
                lb,
                p as u64,
                &mut prev_lb,
                &mut prev_phys,
                &mut run_len,
            );
        } else {
            flush_run(ranges, &mut prev_lb, &mut prev_phys, &mut run_len);
        }
    }
    flush_run(ranges, &mut prev_lb, &mut prev_phys, &mut run_len);
}
