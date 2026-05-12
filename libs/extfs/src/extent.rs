//! ext4 extent tree 遍历(只读)。
//!
//! Extent 是 ext4 的稀疏稀释映射:每 12 字节描述一段连续逻辑块 → 物理块。
//! `i_block[0..60]` 先放一个 `ext4_extent_header`,后面按情况紧接叶节点
//! (ext4_extent)或内节点(ext4_extent_idx)。磁盘上深度 > 0 的子节点各占一整块。
//!
//! 布局:
//!
//! ```text
//! +----------- ext4_extent_header (12 bytes) -----------+
//! | magic(2) | entries(2) | max(2) | depth(2) | gen(4) |
//! +-----------------------------------------------------+
//! ```
//!
//! depth == 0:后面跟 entries 个 ext4_extent(12 字节)
//! depth  > 0:后面跟 entries 个 ext4_extent_idx(12 字节)

use alloc::vec;
use alloc::vec::Vec;

use crate::layout::EXT4_EXT_MAGIC;
use crate::state::{BlockBackendError, FsState};

const EXT_HEADER_SIZE: usize = 12;
const EXT_ENTRY_SIZE: usize = 12;

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

#[inline(always)]
fn read_u16_le(p: *const u8, off: usize) -> u16 {
    u16::from_le_bytes(unsafe { [*p.add(off), *p.add(off + 1)] })
}

#[derive(Debug, Clone, Copy)]
struct ExtHeader {
    entries: u16,
    depth: u16,
}

#[inline]
fn parse_header(buf: &[u8]) -> Option<ExtHeader> {
    if buf.len() < EXT_HEADER_SIZE {
        return None;
    }
    let p = buf.as_ptr();
    let magic = read_u16_le(p, 0);
    if magic != EXT4_EXT_MAGIC {
        return None;
    }
    let entries = read_u16_le(p, 2);
    let depth = read_u16_le(p, 6);
    Some(ExtHeader { entries, depth })
}

/// 根据逻辑块号 `logical_block` 找对应的物理块号。若存在"洞",返回 `Ok(None)`。
///
/// `root` 是 `inode.i_block[0..60]`(或任何内节点块的前 60 字节)的副本;
/// 递归下钻时会从 `state` 按需读取子节点块。
#[inline]
pub(crate) fn map_block(
    state: &FsState,
    root: &[u8],
    logical_block: u32,
) -> Result<Option<u64>, BlockBackendError> {
    let mut current: alloc::vec::Vec<u8> = alloc::vec::Vec::from(root);
    loop {
        let hdr = match parse_header(&current) {
            Some(h) => h,
            None => return Ok(None),
        };
        let p = current.as_ptr();
        if hdr.depth == 0 {
            for i in 0..hdr.entries as usize {
                let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
                if off + EXT_ENTRY_SIZE > current.len() {
                    break;
                }
                let ee_block = read_u32_le(p, off);
                let ee_len = read_u16_le(p, off + 4);
                let uninit = ee_len > 0x8000;
                let real_len = if uninit { ee_len - 0x8000 } else { ee_len };
                let ee_start_hi = read_u16_le(p, off + 6) as u64;
                let ee_start_lo = read_u32_le(p, off + 8) as u64;
                let start = (ee_start_hi << 32) | ee_start_lo;
                if logical_block >= ee_block
                    && (logical_block as u64) < ee_block as u64 + real_len as u64
                {
                    if uninit {
                        return Ok(None);
                    }
                    let delta = logical_block - ee_block;
                    return Ok(Some(start + delta as u64));
                }
            }
            return Ok(None);
        } else {
            let mut best: Option<(u32, u64)> = None;
            for i in 0..hdr.entries as usize {
                let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
                if off + EXT_ENTRY_SIZE > current.len() {
                    break;
                }
                let ei_block = read_u32_le(p, off);
                let ei_leaf_lo = read_u32_le(p, off + 4) as u64;
                let ei_leaf_hi = read_u16_le(p, off + 8) as u64;
                let child = (ei_leaf_hi << 32) | ei_leaf_lo;
                if ei_block <= logical_block {
                    match best {
                        Some((b, _)) if b >= ei_block => {}
                        _ => best = Some((ei_block, child)),
                    }
                }
            }
            match best {
                Some((_, child_block)) => {
                    let block_size = state.ext_sb.block_size as usize;
                    let mut next = vec![0u8; block_size];
                    state.read_block(child_block, &mut next)?;
                    current = next;
                }
                None => return Ok(None),
            }
        }
    }
}

#[inline]
fn collect_extents(
    state: &FsState,
    current: &[u8],
    start_lb: u32,
    end_lb: u32,
    out: &mut Vec<(u32, u32, u64)>,
) -> Result<(), BlockBackendError> {
    let hdr = match parse_header(current) {
        Some(h) => h,
        None => return Ok(()),
    };
    let p = current.as_ptr();
    if hdr.depth == 0 {
        for i in 0..hdr.entries as usize {
            let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
            if off + EXT_ENTRY_SIZE > current.len() {
                break;
            }
            let ee_block = read_u32_le(p, off);
            let ee_len = read_u16_le(p, off + 4);
            let uninit = ee_len > 0x8000;
            if uninit {
                continue;
            }
            let real_len = ee_len as u32;
            let ee_start_hi = read_u16_le(p, off + 6) as u64;
            let ee_start_lo = read_u32_le(p, off + 8) as u64;
            let ee_start = (ee_start_hi << 32) | ee_start_lo;
            let ee_end = ee_block.saturating_add(real_len);
            if ee_end <= start_lb || ee_block >= end_lb {
                continue;
            }
            let overlap_start = ee_block.max(start_lb);
            let overlap_end = ee_end.min(end_lb);
            let overlap_count = overlap_end - overlap_start;
            let phys_start = ee_start + (overlap_start - ee_block) as u64;
            out.push((overlap_start, overlap_count, phys_start));
        }
    } else {
        let mut best_idx: Option<usize> = None;
        for i in 0..hdr.entries as usize {
            let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
            if off + EXT_ENTRY_SIZE > current.len() {
                break;
            }
            let ei_block = read_u32_le(p, off);
            if ei_block <= start_lb {
                best_idx = Some(i);
            }
        }
        let start_idx = best_idx.unwrap_or(0);
        let block_size = state.ext_sb.block_size as usize;
        for i in start_idx..hdr.entries as usize {
            let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
            if off + EXT_ENTRY_SIZE > current.len() {
                break;
            }
            let ei_block = read_u32_le(p, off);
            if ei_block >= end_lb {
                break;
            }
            let ei_leaf_lo = read_u32_le(p, off + 4) as u64;
            let ei_leaf_hi = read_u16_le(p, off + 8) as u64;
            let child = (ei_leaf_hi << 32) | ei_leaf_lo;
            let mut next = vec![0u8; block_size];
            state.read_block(child, &mut next)?;
            collect_extents(state, &next, start_lb, end_lb, out)?;
        }
    }
    Ok(())
}

pub(crate) fn map_contiguous(
    state: &FsState,
    root: &[u8],
    start_lb: u32,
    count: u32,
) -> Result<Vec<(u32, u32, u64)>, BlockBackendError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let end_lb = start_lb.saturating_add(count);
    let mut result = Vec::new();
    collect_extents(state, root, start_lb, end_lb, &mut result)?;
    Ok(result)
}
