//! extent tree 写路径(最小实现):释放整棵树,回落为间接块。
//!
//! 理由:写入一个已存在的 extent 文件比较复杂(分裂、合并、索引节点下钻)。
//! 只要我们在 truncate-to-0 时把 `EXT4_EXTENTS_FL` 清掉并把 header 清零,
//! 之后就能像 ext2 文件一样用间接块继续写入。所有"读"仍然兼容原 extent,
//! 写入前必须至少经历一次 truncate=0。

use alloc::vec;
use alloc::vec::Vec;

use crate::alloc_mod;
use crate::layout::{EXT4_EXT_MAGIC, EXT4_EXTENTS_FL};
use crate::state::{BlockBackendError, FsState};

const EXT_HEADER_SIZE: usize = 12;
const EXT_ENTRY_SIZE: usize = 12;

/// 递归释放 extent tree 指向的所有数据块 + 子节点块。
/// `root` 是当前节点的前 (≥ header_size) 字节;对 inode 根只传 60 字节。
pub(crate) fn free_tree(state: &FsState, root: &[u8]) -> Result<(), BlockBackendError> {
    if root.len() < EXT_HEADER_SIZE {
        return Ok(());
    }
    let magic = u16::from_le_bytes([root[0], root[1]]);
    if magic != EXT4_EXT_MAGIC {
        return Ok(());
    }
    let entries = u16::from_le_bytes([root[2], root[3]]);
    let depth = u16::from_le_bytes([root[6], root[7]]);
    if depth == 0 {
        for i in 0..entries as usize {
            let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
            if off + EXT_ENTRY_SIZE > root.len() {
                break;
            }
            let ee_len = u16::from_le_bytes([root[off + 4], root[off + 5]]);
            let real_len = if ee_len > 0x8000 {
                ee_len - 0x8000
            } else {
                ee_len
            };
            let start_hi = u16::from_le_bytes([root[off + 6], root[off + 7]]) as u64;
            let start_lo =
                u32::from_le_bytes([root[off + 8], root[off + 9], root[off + 10], root[off + 11]])
                    as u64;
            let start = (start_hi << 32) | start_lo;
            for b in 0..real_len as u64 {
                alloc_mod::free_block(state, start + b)?;
            }
        }
    } else {
        for i in 0..entries as usize {
            let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
            if off + EXT_ENTRY_SIZE > root.len() {
                break;
            }
            let leaf_lo =
                u32::from_le_bytes([root[off + 4], root[off + 5], root[off + 6], root[off + 7]])
                    as u64;
            let leaf_hi = u16::from_le_bytes([root[off + 8], root[off + 9]]) as u64;
            let child = (leaf_hi << 32) | leaf_lo;
            let bs = state.ext_sb.block_size as usize;
            let mut blk = vec![0u8; bs];
            state.read_block(child, &mut blk)?;
            free_tree(state, &blk)?;
            alloc_mod::free_block(state, child)?;
        }
    }
    Ok(())
}

/// 把一个原来用 extent 的文件 i_block 重置为"空的间接块布局"。
/// 调用方负责清 `EXT4_EXTENTS_FL`。
pub(crate) fn reset_to_indirect(i_block: &mut [u8]) {
    for b in i_block.iter_mut() {
        *b = 0;
    }
}

/// 如果文件启用了 extent,清空整树 + 清 flag,回落为间接布局。
/// 调用方:先调用此函数再做写路径。
pub(crate) fn demote_if_extent(
    state: &FsState,
    flags: &mut u32,
    i_block: &mut [u8],
) -> Result<(), BlockBackendError> {
    if *flags & EXT4_EXTENTS_FL == 0 {
        return Ok(());
    }
    free_tree(state, i_block)?;
    reset_to_indirect(i_block);
    *flags &= !EXT4_EXTENTS_FL;
    Ok(())
}

/// 判断 i_block 是否已经是合法的 extent 根(简单看 magic)。
#[inline]
#[allow(dead_code)]
pub(crate) fn has_extent_magic(i_block: &[u8]) -> bool {
    i_block.len() >= 2 && u16::from_le_bytes([i_block[0], i_block[1]]) == EXT4_EXT_MAGIC
}

#[allow(dead_code)]
pub(crate) fn collect_entries(root: &[u8]) -> Vec<(u32, u16, u64)> {
    let mut out = Vec::new();
    if root.len() < EXT_HEADER_SIZE {
        return out;
    }
    let entries = u16::from_le_bytes([root[2], root[3]]);
    let depth = u16::from_le_bytes([root[6], root[7]]);
    if depth != 0 {
        return out;
    }
    for i in 0..entries as usize {
        let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
        if off + EXT_ENTRY_SIZE > root.len() {
            break;
        }
        let ee_block = u32::from_le_bytes([root[off], root[off + 1], root[off + 2], root[off + 3]]);
        let ee_len = u16::from_le_bytes([root[off + 4], root[off + 5]]);
        let start_hi = u16::from_le_bytes([root[off + 6], root[off + 7]]) as u64;
        let start_lo =
            u32::from_le_bytes([root[off + 8], root[off + 9], root[off + 10], root[off + 11]])
                as u64;
        let start = (start_hi << 32) | start_lo;
        out.push((ee_block, ee_len, start));
    }
    out
}

/// 尝试在 i_block 根节点里原地追加一个叶子 extent,覆盖逻辑块 `lb` 指向新分配
/// 的物理块 `phys`。仅当 depth=0 且 `entries < max` 时成功。
///
/// 返回 `true` 表示已追加。失败说明根已满或是索引节点,调用方应 fallback 到
/// `demote_if_extent` + 间接块。
pub(crate) fn try_append_leaf(i_block: &mut [u8], lb: u32, phys: u64, len: u16) -> bool {
    if i_block.len() < EXT_HEADER_SIZE {
        return false;
    }
    let magic = u16::from_le_bytes([i_block[0], i_block[1]]);
    if magic != EXT4_EXT_MAGIC {
        return false;
    }
    let entries = u16::from_le_bytes([i_block[2], i_block[3]]);
    let max = u16::from_le_bytes([i_block[4], i_block[5]]);
    let depth = u16::from_le_bytes([i_block[6], i_block[7]]);
    if depth != 0 || entries >= max {
        return false;
    }
    // 尝试合并:若新 extent 紧邻上一条(相同物理连续 + lb 连续 + 未越 16bit len),
    // 就把上一条 len 加 1 即可(一样的路径 ext4 的 extent_append 里做了 merge)。
    if entries > 0 {
        let last_off = EXT_HEADER_SIZE + (entries as usize - 1) * EXT_ENTRY_SIZE;
        let last_blk = u32::from_le_bytes([
            i_block[last_off],
            i_block[last_off + 1],
            i_block[last_off + 2],
            i_block[last_off + 3],
        ]);
        let last_len = u16::from_le_bytes([i_block[last_off + 4], i_block[last_off + 5]]);
        let last_hi = u16::from_le_bytes([i_block[last_off + 6], i_block[last_off + 7]]) as u64;
        let last_lo = u32::from_le_bytes([
            i_block[last_off + 8],
            i_block[last_off + 9],
            i_block[last_off + 10],
            i_block[last_off + 11],
        ]) as u64;
        let last_start = (last_hi << 32) | last_lo;
        let last_uninit = last_len > 0x8000;
        if !last_uninit {
            let real_last_len = last_len;
            if last_blk + real_last_len as u32 == lb
                && last_start + real_last_len as u64 == phys
                && (real_last_len as u32 + len as u32) < 0x8000
            {
                let new_len = real_last_len + len;
                i_block[last_off + 4..last_off + 6].copy_from_slice(&new_len.to_le_bytes());
                return true;
            }
        }
    }
    let off = EXT_HEADER_SIZE + entries as usize * EXT_ENTRY_SIZE;
    if off + EXT_ENTRY_SIZE > i_block.len() {
        return false;
    }
    i_block[off..off + 4].copy_from_slice(&lb.to_le_bytes());
    i_block[off + 4..off + 6].copy_from_slice(&len.to_le_bytes());
    let hi = (phys >> 32) as u16;
    let lo = phys as u32;
    i_block[off + 6..off + 8].copy_from_slice(&hi.to_le_bytes());
    i_block[off + 8..off + 12].copy_from_slice(&lo.to_le_bytes());
    let new_entries = entries + 1;
    i_block[2..4].copy_from_slice(&new_entries.to_le_bytes());
    true
}

/// 确保 i_block 里有一个合法的根节点(magic + entries=0 + max=4 + depth=0)。
/// 用在 extent 文件创建或降级回 extent 时。
#[allow(dead_code)]
pub(crate) fn init_empty_root(i_block: &mut [u8]) {
    for b in i_block.iter_mut() {
        *b = 0;
    }
    i_block[0..2].copy_from_slice(&EXT4_EXT_MAGIC.to_le_bytes());
    i_block[2..4].copy_from_slice(&0u16.to_le_bytes()); // entries
    i_block[4..6].copy_from_slice(&4u16.to_le_bytes()); // max
    i_block[6..8].copy_from_slice(&0u16.to_le_bytes()); // depth
    // generation 保持 0
}

/// 在 extent 文件里为逻辑块 `lb` 找/建物理块。仅支持深度 0 的根叶节点。
/// 若根已满/深度>0,返回 `Ok(None)`,调用方应 fallback。
pub(crate) fn ensure_block_in_extent(
    state: &crate::state::FsState,
    i_block: &mut [u8],
    lb: u32,
) -> Result<Option<u64>, BlockBackendError> {
    if i_block.len() < EXT_HEADER_SIZE {
        return Ok(None);
    }
    let magic = u16::from_le_bytes([i_block[0], i_block[1]]);
    if magic != EXT4_EXT_MAGIC {
        return Ok(None);
    }
    let depth = u16::from_le_bytes([i_block[6], i_block[7]]);
    if depth != 0 {
        return Ok(None);
    }
    // 查现有叶子看 lb 是否已覆盖
    let entries = u16::from_le_bytes([i_block[2], i_block[3]]);
    for i in 0..entries as usize {
        let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
        if off + EXT_ENTRY_SIZE > i_block.len() {
            break;
        }
        let ee_block = u32::from_le_bytes([
            i_block[off],
            i_block[off + 1],
            i_block[off + 2],
            i_block[off + 3],
        ]);
        let ee_len = u16::from_le_bytes([i_block[off + 4], i_block[off + 5]]);
        let real_len = if ee_len > 0x8000 {
            ee_len - 0x8000
        } else {
            ee_len
        };
        if lb >= ee_block && lb < ee_block + real_len as u32 {
            if ee_len > 0x8000 {
                // 未初始化 extent:写入时需要先标已初始化 + 清零,简化为
                // 作为"未覆盖"处理(让调用方 fallback 到 demote)
                return Ok(None);
            }
            let start_hi = u16::from_le_bytes([i_block[off + 6], i_block[off + 7]]) as u64;
            let start_lo = u32::from_le_bytes([
                i_block[off + 8],
                i_block[off + 9],
                i_block[off + 10],
                i_block[off + 11],
            ]) as u64;
            let start = (start_hi << 32) | start_lo;
            return Ok(Some(start + (lb - ee_block) as u64));
        }
    }
    // 没覆盖到:需要新开一条叶子。先分配一个物理块,再尝试 append。
    let new_phys = crate::alloc_mod::alloc_block(state)?;
    // 清零新块
    let bs = state.ext_sb.block_size as usize;
    let z = alloc::vec![0u8; bs];
    state.write_block(new_phys, &z)?;
    if try_append_leaf(i_block, lb, new_phys, 1) {
        Ok(Some(new_phys))
    } else {
        // 失败 —— 把块释放再回退,调用方会走 demote
        crate::alloc_mod::free_block(state, new_phys)?;
        Ok(None)
    }
}
