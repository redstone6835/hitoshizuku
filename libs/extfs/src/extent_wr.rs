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
use crate::map_wr::BlockAllocState;
use crate::state::{BlockBackendError, FsState};

const EXT_HEADER_SIZE: usize = 12;
const EXT_ENTRY_SIZE: usize = 12;
const MAX_INITIALIZED_EXTENT_LEN: u32 = 0x8000;
const MAX_EXTENT_PHYSICAL_BLOCK: u64 = (1u64 << 48) - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LeafExtent {
    pub(crate) logical: u32,
    pub(crate) len: u32,
    pub(crate) physical: u64,
    pub(crate) initialized: bool,
}

impl LeafExtent {
    #[inline]
    pub(crate) fn logical_end(self) -> u64 {
        self.logical as u64 + self.len as u64
    }

    #[inline]
    fn physical_end(self) -> u64 {
        self.physical + self.len as u64
    }
}

pub(crate) fn read_leaf_extent(root: &[u8], index: usize) -> Option<LeafExtent> {
    let off = EXT_HEADER_SIZE.checked_add(index.checked_mul(EXT_ENTRY_SIZE)?)?;
    let entry = root.get(off..off + EXT_ENTRY_SIZE)?;
    let logical = u32::from_le_bytes(entry[0..4].try_into().ok()?);
    let encoded_len = u16::from_le_bytes(entry[4..6].try_into().ok()?);
    if encoded_len == 0 {
        return None;
    }
    let initialized = encoded_len <= 0x8000;
    let len = if initialized {
        encoded_len as u32
    } else {
        (encoded_len - 0x8000) as u32
    };
    let physical_hi = u16::from_le_bytes(entry[6..8].try_into().ok()?) as u64;
    let physical_lo = u32::from_le_bytes(entry[8..12].try_into().ok()?) as u64;
    let extent = LeafExtent {
        logical,
        len,
        physical: (physical_hi << 32) | physical_lo,
        initialized,
    };
    if extent.logical_end() > u32::MAX as u64 + 1
        || extent.physical_end().checked_sub(1)? > MAX_EXTENT_PHYSICAL_BLOCK
    {
        return None;
    }
    Some(extent)
}

fn write_leaf(root: &mut [u8], index: usize, extent: LeafExtent) {
    debug_assert!((1..=MAX_INITIALIZED_EXTENT_LEN).contains(&extent.len));
    let off = EXT_HEADER_SIZE + index * EXT_ENTRY_SIZE;
    let encoded_len = if extent.initialized {
        extent.len as u16
    } else {
        (extent.len as u16) | 0x8000
    };
    root[off..off + 4].copy_from_slice(&extent.logical.to_le_bytes());
    root[off + 4..off + 6].copy_from_slice(&encoded_len.to_le_bytes());
    root[off + 6..off + 8].copy_from_slice(&((extent.physical >> 32) as u16).to_le_bytes());
    root[off + 8..off + 12].copy_from_slice(&(extent.physical as u32).to_le_bytes());
}

/// 校验 inode 内嵌叶子根，并确保已有条目按逻辑块排序且互不重叠。
fn validated_leaf_root(root: &[u8]) -> Option<(usize, usize)> {
    if root.len() < EXT_HEADER_SIZE
        || u16::from_le_bytes(root[0..2].try_into().ok()?) != EXT4_EXT_MAGIC
        || u16::from_le_bytes(root[6..8].try_into().ok()?) != 0
    {
        return None;
    }
    let entries = u16::from_le_bytes(root[2..4].try_into().ok()?) as usize;
    let max = u16::from_le_bytes(root[4..6].try_into().ok()?) as usize;
    let capacity = (root.len() - EXT_HEADER_SIZE) / EXT_ENTRY_SIZE;
    if entries > max || max > capacity {
        return None;
    }

    let mut previous_end = 0u64;
    for index in 0..entries {
        let extent = read_leaf_extent(root, index)?;
        if index != 0 && (extent.logical as u64) < previous_end {
            return None;
        }
        previous_end = extent.logical_end();
    }
    Some((entries, max))
}

#[inline]
fn can_merge(left: LeafExtent, right: LeafExtent) -> bool {
    left.initialized == right.initialized
        && left.logical_end() == right.logical as u64
        && left.physical_end() == right.physical
        && left.len + right.len <= MAX_INITIALIZED_EXTENT_LEN
}

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
            alloc_mod::free_blocks_run(state, start, real_len as u32)?;
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

/// 统计 extent tree 实际占用的文件系统块数,包含数据块和外部 extent 索引块。
///
/// inode 内嵌的 extent root 不占磁盘块,所以 depth=0 时只累加叶子 extent 的
/// 数据块数;depth>0 时每个 child extent block 先计 1,再递归统计其子树。
pub(crate) fn count_tree_blocks(state: &FsState, root: &[u8]) -> Result<u64, BlockBackendError> {
    if root.len() < EXT_HEADER_SIZE {
        return Ok(0);
    }
    let magic = u16::from_le_bytes([root[0], root[1]]);
    if magic != EXT4_EXT_MAGIC {
        return Ok(0);
    }
    let entries = u16::from_le_bytes([root[2], root[3]]) as usize;
    let depth = u16::from_le_bytes([root[6], root[7]]);
    let mut total = 0u64;
    if depth == 0 {
        for i in 0..entries {
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
            total += real_len as u64;
        }
    } else {
        let bs = state.ext_sb.block_size as usize;
        let mut blk = vec![0u8; bs];
        for i in 0..entries {
            let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
            if off + EXT_ENTRY_SIZE > root.len() {
                break;
            }
            let leaf_lo =
                u32::from_le_bytes([root[off + 4], root[off + 5], root[off + 6], root[off + 7]])
                    as u64;
            let leaf_hi = u16::from_le_bytes([root[off + 8], root[off + 9]]) as u64;
            let child = (leaf_hi << 32) | leaf_lo;
            total += 1;
            state.read_block(child, &mut blk)?;
            total += count_tree_blocks(state, &blk)?;
        }
    }
    Ok(total)
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

/// 保留现有数据块,把 depth=0 的 extent 根转换成 direct/indirect 映射。
///
/// 写路径不能使用 [`demote_if_extent`],因为它会释放 extent 树下的数据块。
/// 当 extent 根已经无法继续追加时,这里把已有叶子映射搬到传统间接块布局,
/// 然后调用方可以继续用 [`crate::map_wr::ensure_block`] 扩容。
pub(crate) fn demote_preserve_if_extent(
    state: &FsState,
    flags: &mut u32,
    i_block: &mut [u8],
) -> Result<bool, BlockBackendError> {
    demote_preserve_if_extent_count(state, flags, i_block).map(|(converted, _)| converted)
}

/// 与 [`demote_preserve_if_extent`] 相同，并返回转换期间新分配的间接索引块数。
/// 原 extent 的数据块只是搬运映射，不计入该增量。
pub(crate) fn demote_preserve_if_extent_count(
    state: &FsState,
    flags: &mut u32,
    i_block: &mut [u8],
) -> Result<(bool, u32), BlockBackendError> {
    if *flags & EXT4_EXTENTS_FL == 0 {
        return Ok((true, 0));
    }
    if i_block.len() < EXT_HEADER_SIZE {
        return Ok((false, 0));
    }
    let magic = u16::from_le_bytes([i_block[0], i_block[1]]);
    if magic != EXT4_EXT_MAGIC {
        return Ok((false, 0));
    }
    let entries = u16::from_le_bytes([i_block[2], i_block[3]]) as usize;
    let depth = u16::from_le_bytes([i_block[6], i_block[7]]);
    if depth != 0 {
        return Ok((false, 0));
    }

    let old = i_block.to_vec();
    reset_to_indirect(i_block);
    let mut new_metadata = 0u32;
    for i in 0..entries {
        let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
        if off + EXT_ENTRY_SIZE > old.len() {
            return Ok((false, 0));
        }
        let ee_block = u32::from_le_bytes([old[off], old[off + 1], old[off + 2], old[off + 3]]);
        let ee_len = u16::from_le_bytes([old[off + 4], old[off + 5]]);
        if ee_len > 0x8000 {
            return Ok((false, 0));
        }
        let start_hi = u16::from_le_bytes([old[off + 6], old[off + 7]]) as u64;
        let start_lo =
            u32::from_le_bytes([old[off + 8], old[off + 9], old[off + 10], old[off + 11]]) as u64;
        let start = (start_hi << 32) | start_lo;
        for b in 0..ee_len as u32 {
            let allocated =
                crate::map_wr::set_existing_block(state, i_block, ee_block + b, start + b as u64)?;
            new_metadata = new_metadata
                .checked_add(allocated)
                .ok_or(BlockBackendError::OutOfRange)?;
        }
    }
    *flags &= !EXT4_EXTENTS_FL;
    Ok((true, new_metadata))
}

/// 判断 i_block 是否已经是合法的 extent 根(简单看 magic)。
#[inline]
#[allow(dead_code)]
pub(crate) fn has_extent_magic(i_block: &[u8]) -> bool {
    i_block.len() >= 2 && u16::from_le_bytes([i_block[0], i_block[1]]) == EXT4_EXT_MAGIC
}

/// extent 根的深度(非 extent 布局返回 None)。fast-commit 回放用它拒绝深树。
pub(crate) fn root_depth(i_block: &[u8]) -> Option<u16> {
    if i_block.len() < EXT_HEADER_SIZE {
        return None;
    }
    let magic = u16::from_le_bytes([i_block[0], i_block[1]]);
    if magic != EXT4_EXT_MAGIC {
        return None;
    }
    Some(u16::from_le_bytes([i_block[6], i_block[7]]))
}

/// depth-0 根的条目数与容量(结构不合法返回 None)。
pub(crate) fn leaf_root_shape(i_block: &[u8]) -> Option<(usize, usize)> {
    validated_leaf_root(i_block)
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

/// 尝试在 i_block 根节点里原地插入一个叶子 extent,覆盖逻辑块 `lb` 指向新分配
/// 的物理块 `phys`。条目始终按逻辑块排序，并尽量与物理、逻辑都连续的相邻项合并。
///
/// 返回 `true` 表示已插入或合并。失败说明根已满、映射重叠或是索引节点,调用方应 fallback 到
/// `demote_if_extent` + 间接块。
pub(crate) fn try_append_leaf(i_block: &mut [u8], lb: u32, phys: u64, len: u16) -> bool {
    try_append_leaf_impl(i_block, lb, phys, len as u32, true)
}

/// [`try_append_leaf`] 的推广:允许插入未初始化(unwritten) extent,
/// fast-commit ADD_RANGE 回放需要保留该状态位。
pub(crate) fn try_append_leaf_uninit(
    i_block: &mut [u8],
    lb: u32,
    phys: u64,
    len: u32,
    initialized: bool,
) -> bool {
    try_append_leaf_impl(i_block, lb, phys, len, initialized)
}

fn try_append_leaf_impl(
    i_block: &mut [u8],
    lb: u32,
    phys: u64,
    len: u32,
    initialized: bool,
) -> bool {
    if len == 0
        || len > MAX_INITIALIZED_EXTENT_LEN
        || lb as u64 + len as u64 > u32::MAX as u64 + 1
        || phys
            .checked_add(len as u64 - 1)
            .is_none_or(|last| last > MAX_EXTENT_PHYSICAL_BLOCK)
    {
        return false;
    }

    let Some((entries, max)) = validated_leaf_root(i_block) else {
        return false;
    };
    let new_extent = LeafExtent {
        logical: lb,
        len,
        physical: phys,
        initialized,
    };
    let insert_at = (0..entries)
        .find(|&index| read_leaf_extent(i_block, index).unwrap().logical >= lb)
        .unwrap_or(entries);
    let left = insert_at
        .checked_sub(1)
        .map(|index| read_leaf_extent(i_block, index).unwrap());
    let right = (insert_at < entries).then(|| read_leaf_extent(i_block, insert_at).unwrap());

    if left.is_some_and(|extent| extent.logical_end() > lb as u64)
        || right.is_some_and(|extent| new_extent.logical_end() > extent.logical as u64)
    {
        return false;
    }

    let merge_left = left.is_some_and(|extent| can_merge(extent, new_extent));
    let merge_right = right.is_some_and(|extent| can_merge(new_extent, extent));
    if merge_left && merge_right {
        let left_extent = left.unwrap();
        let right_extent = right.unwrap();
        if left_extent.len + new_extent.len + right_extent.len <= MAX_INITIALIZED_EXTENT_LEN {
            write_leaf(
                i_block,
                insert_at - 1,
                LeafExtent {
                    len: left_extent.len + new_extent.len + right_extent.len,
                    ..left_extent
                },
            );
            let entries_end = EXT_HEADER_SIZE + entries * EXT_ENTRY_SIZE;
            let right_off = EXT_HEADER_SIZE + insert_at * EXT_ENTRY_SIZE;
            i_block.copy_within(right_off + EXT_ENTRY_SIZE..entries_end, right_off);
            i_block[entries_end - EXT_ENTRY_SIZE..entries_end].fill(0);
            i_block[2..4].copy_from_slice(&((entries - 1) as u16).to_le_bytes());
            return true;
        }
    }
    if merge_left {
        let left_extent = left.unwrap();
        write_leaf(
            i_block,
            insert_at - 1,
            LeafExtent {
                len: left_extent.len + new_extent.len,
                ..left_extent
            },
        );
        return true;
    }
    if merge_right {
        let right_extent = right.unwrap();
        write_leaf(
            i_block,
            insert_at,
            LeafExtent {
                len: new_extent.len + right_extent.len,
                ..new_extent
            },
        );
        return true;
    }
    if entries >= max {
        return false;
    }

    let insert_off = EXT_HEADER_SIZE + insert_at * EXT_ENTRY_SIZE;
    let entries_end = EXT_HEADER_SIZE + entries * EXT_ENTRY_SIZE;
    i_block.copy_within(insert_off..entries_end, insert_off + EXT_ENTRY_SIZE);
    write_leaf(i_block, insert_at, new_extent);
    i_block[2..4].copy_from_slice(&((entries + 1) as u16).to_le_bytes());
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
    ensure_block_in_extent_for_write(state, i_block, lb).map(|res| res.map(BlockAllocState::phys))
}

/// 在 extent 根叶子中查找或分配一个逻辑块，并返回该数据块是否刚分配。
///
/// 新 extent 数据块不在这里清零；写路径会根据部分写/整块覆盖决定是否需要填零，
/// 这样可以避免新建文件顺序写时的重复写盘。
pub(crate) fn ensure_block_in_extent_for_write(
    state: &crate::state::FsState,
    i_block: &mut [u8],
    lb: u32,
) -> Result<Option<BlockAllocState>, BlockBackendError> {
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
            return Ok(Some(BlockAllocState::Existing(
                start + (lb - ee_block) as u64,
            )));
        }
    }
    // 没覆盖到:需要新开一条叶子。先分配一个物理块,再尝试 append。
    let new_phys = crate::alloc_mod::alloc_block(state)?;
    if try_append_leaf(i_block, lb, new_phys, 1) {
        Ok(Some(BlockAllocState::NewlyAllocated(new_phys)))
    } else {
        // 失败 —— 把块释放再回退,调用方会走 demote
        crate::alloc_mod::free_block(state, new_phys)?;
        Ok(None)
    }
}

pub(crate) fn ensure_extent_run(
    state: &crate::state::FsState,
    i_block: &mut [u8],
    start_lb: u32,
    count: u32,
) -> Result<Option<(u64, u32)>, BlockBackendError> {
    if count == 0 {
        return Ok(None);
    }
    if i_block.len() < EXT_HEADER_SIZE {
        return Ok(None);
    }
    let magic = u16::from_le_bytes([i_block[0], i_block[1]]);
    let depth = u16::from_le_bytes([i_block[6], i_block[7]]);
    if magic != EXT4_EXT_MAGIC || depth != 0 {
        return Ok(None);
    }

    // bench 的覆盖写会反复命中同一条 extent。直接从叶子条目计算 run，避免
    // 对每个逻辑块都重新扫描 extent 表。
    if let Some((phys, run)) = lookup_extent_run(i_block, start_lb, count) {
        return Ok(Some((phys, run)));
    }

    // 未映射：分配长度必须截断到下一条已有 extent 之前，否则随机 pwrite 会让
    // 新映射覆盖后面的有效映射。单条 initialized extent 也不能超过 0x8000 块。
    let Some(alloc_count) = clip_unmapped_run(i_block, start_lb, count) else {
        return Ok(None);
    };
    let (new_phys, got) = crate::alloc_mod::alloc_blocks_run(state, alloc_count)?;
    if try_append_leaf(i_block, start_lb, new_phys, got as u16) {
        Ok(Some((new_phys, got)))
    } else {
        // extent 叶子满了，尝试只插入 1 个块
        if got > 1 {
            // 释放多余的块
            for i in 1..got {
                let _ = crate::alloc_mod::free_block(state, new_phys + i as u64);
            }
        }
        if try_append_leaf(i_block, start_lb, new_phys, 1) {
            Ok(Some((new_phys, 1)))
        } else {
            crate::alloc_mod::free_block(state, new_phys)?;
            Ok(None)
        }
    }
}

/// 把一个 hole 中的分配请求裁剪到下一条 extent 的起点。
fn clip_unmapped_run(i_block: &[u8], start_lb: u32, count: u32) -> Option<u32> {
    if count == 0 {
        return None;
    }
    let (entries, _) = validated_leaf_root(i_block)?;
    let start = start_lb as u64;
    let logical_limit = u32::MAX as u64 + 1;
    let mut clipped = (count as u64)
        .min(MAX_INITIALIZED_EXTENT_LEN as u64)
        .min(logical_limit - start);
    for index in 0..entries {
        let extent = read_leaf_extent(i_block, index)?;
        if start >= extent.logical as u64 && start < extent.logical_end() {
            return None;
        }
        if start < extent.logical as u64 {
            clipped = clipped.min(extent.logical as u64 - start);
            break;
        }
    }
    u32::try_from(clipped).ok().filter(|len| *len != 0)
}

/// Public wrapper for `lookup_extent_run` — used by file write path to
/// determine if blocks are already mapped before calling `ensure_extent_run`.
pub(crate) fn lookup_extent_run_pub(i_block: &[u8], lb: u32, max_count: u32) -> Option<(u64, u32)> {
    lookup_extent_run(i_block, lb, max_count)
}

/// 在叶子 extent 中查找从 `lb` 开始的连续映射。
///
/// 返回的 run 长度不会超过 `max_count`，且只来自同一条 extent；extent 条目本身已
/// 表达物理块连续性，无需逐个逻辑块重复查找。
fn lookup_extent_run(i_block: &[u8], lb: u32, max_count: u32) -> Option<(u64, u32)> {
    if max_count == 0 || i_block.len() < EXT_HEADER_SIZE {
        return None;
    }
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
                return None; // uninitialized extent
            }
            let start_hi = u16::from_le_bytes([i_block[off + 6], i_block[off + 7]]) as u64;
            let start_lo = u32::from_le_bytes([
                i_block[off + 8],
                i_block[off + 9],
                i_block[off + 10],
                i_block[off + 11],
            ]) as u64;
            let in_extent = lb - ee_block;
            let run = (real_len as u32 - in_extent).min(max_count);
            return Some((((start_hi << 32) | start_lo) + in_extent as u64, run));
        }
    }
    None
}

/// 把 depth-0 extent 根中 `[lblk, lblk+len)` 的映射改写为 `(phys, initialized)`。
///
/// 用于 fast-commit ADD_RANGE 回放:完全覆盖的 extent 直接替换;部分覆盖时按
/// 左/中/右最多三段分裂。根容量不足(分裂后超过 eh_max)时返回 `false`
/// 且不修改,由调用方决定降级策略。
pub(crate) fn leaf_root_set_range(
    i_block: &mut [u8],
    lblk: u32,
    len: u32,
    phys: u64,
    initialized: bool,
) -> bool {
    if len == 0 {
        return true;
    }
    let Some((entries, max)) = validated_leaf_root(i_block) else {
        return false;
    };
    let rs = lblk as u64;
    let re = rs + len as u64;
    let mut out: Vec<LeafExtent> = Vec::with_capacity(entries + 2);
    let mut inserted = false;
    for index in 0..entries {
        let Some(extent) = read_leaf_extent(i_block, index) else {
            return false;
        };
        let es = extent.logical as u64;
        let ee = extent.logical_end();
        if ee <= rs || es >= re {
            out.push(extent);
            continue;
        }
        // 与目标区间相交:裁剪左右两侧,中间由目标条目替换。
        if es < rs {
            out.push(LeafExtent {
                logical: extent.logical,
                len: (rs - es) as u32,
                physical: extent.physical,
                initialized: extent.initialized,
            });
        }
        if !inserted {
            out.push(LeafExtent {
                logical: lblk,
                len,
                physical: phys,
                initialized,
            });
            inserted = true;
        }
        if ee > re {
            out.push(LeafExtent {
                logical: re as u32,
                len: (ee - re) as u32,
                physical: extent.physical + (re - es),
                initialized: extent.initialized,
            });
        }
    }
    if !inserted {
        out.push(LeafExtent {
            logical: lblk,
            len,
            physical: phys,
            initialized,
        });
    }
    if out.len() > max {
        return false;
    }
    rewrite_leaf_root(i_block, &out);
    true
}

/// 从 depth-0 extent 根中删除 `[lblk, lblk+len)` 的映射(fast-commit
/// DEL_RANGE 回放)。相交 extent 被裁剪或整条移除。返回 `false` 表示根不合法。
pub(crate) fn leaf_root_punch(i_block: &mut [u8], lblk: u32, len: u32) -> bool {
    if len == 0 {
        return true;
    }
    let Some((entries, _)) = validated_leaf_root(i_block) else {
        return false;
    };
    let rs = lblk as u64;
    let re = rs + len as u64;
    let mut out: Vec<LeafExtent> = Vec::with_capacity(entries + 1);
    for index in 0..entries {
        let Some(extent) = read_leaf_extent(i_block, index) else {
            return false;
        };
        let es = extent.logical as u64;
        let ee = extent.logical_end();
        if ee <= rs || es >= re {
            out.push(extent);
            continue;
        }
        if es < rs {
            out.push(LeafExtent {
                logical: extent.logical,
                len: (rs - es) as u32,
                physical: extent.physical,
                initialized: extent.initialized,
            });
        }
        if ee > re {
            out.push(LeafExtent {
                logical: re as u32,
                len: (ee - re) as u32,
                physical: extent.physical + (re - es),
                initialized: extent.initialized,
            });
        }
    }
    rewrite_leaf_root(i_block, &out);
    true
}

/// 用新条目表重写 depth-0 extent 根(条数、内容、尾部清零)。
fn rewrite_leaf_root(i_block: &mut [u8], extents: &[LeafExtent]) {
    let entries_end = EXT_HEADER_SIZE + extents.len() * EXT_ENTRY_SIZE;
    for (index, extent) in extents.iter().enumerate() {
        write_leaf(i_block, index, *extent);
    }
    let root_end = i_block.len();
    i_block[entries_end..root_end].fill(0);
    i_block[2..4].copy_from_slice(&(extents.len() as u16).to_le_bytes());
}

/// 递归遍历 extent 树,把所有叶映射的数据块与索引子块在位图中标记为
/// 已用,返回数据块总数。
///
/// fast-commit 回放收尾时,需要保证崩溃前已分配但位图未落盘的块被正确
/// 置位(对应 `ext4_fc_set_bitmaps_and_counters` 里对 `path[j].p_block`
/// 与数据范围的标记)。
pub(crate) fn mark_tree_blocks_used(
    state: &FsState,
    root: &[u8],
) -> Result<u64, BlockBackendError> {
    if root.len() < EXT_HEADER_SIZE {
        return Ok(0);
    }
    let magic = u16::from_le_bytes([root[0], root[1]]);
    if magic != EXT4_EXT_MAGIC {
        return Ok(0);
    }
    let entries = u16::from_le_bytes([root[2], root[3]]) as usize;
    let depth = u16::from_le_bytes([root[6], root[7]]);
    if depth == 0 {
        let mut total = 0u64;
        for index in 0..entries {
            let Some(ext) = read_leaf_extent(root, index) else {
                break;
            };
            crate::alloc_mod::mark_blocks_used(state, ext.physical, ext.len)?;
            total += ext.len as u64;
        }
        return Ok(total);
    }
    let bs = state.ext_sb.block_size as usize;
    let mut blk = vec![0u8; bs];
    let mut total = 0u64;
    for i in 0..entries {
        let off = EXT_HEADER_SIZE + i * EXT_ENTRY_SIZE;
        if off + EXT_ENTRY_SIZE > root.len() {
            break;
        }
        let leaf_lo =
            u32::from_le_bytes([root[off + 4], root[off + 5], root[off + 6], root[off + 7]]) as u64;
        let leaf_hi = u16::from_le_bytes([root[off + 8], root[off + 9]]) as u64;
        let child = (leaf_hi << 32) | leaf_lo;
        crate::alloc_mod::mark_blocks_used(state, child, 1)?;
        state.read_block(child, &mut blk)?;
        total += mark_tree_blocks_used(state, &blk)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::{
        clip_unmapped_run, collect_entries, init_empty_root, lookup_extent_run, try_append_leaf,
    };

    fn entries(root: &[u8]) -> Vec<(u32, u16, u64)> {
        collect_entries(root)
    }

    #[test]
    fn lookup_extent_run_clips_inside_single_extent() {
        let mut root = [0u8; 60];
        init_empty_root(&mut root);
        assert!(try_append_leaf(&mut root, 8, 1000, 16));

        assert_eq!(lookup_extent_run(&root, 12, 32), Some((1004, 12)));
        assert_eq!(lookup_extent_run(&root, 20, 2), Some((1012, 2)));
        assert_eq!(lookup_extent_run(&root, 24, 1), None);
    }

    #[test]
    fn out_of_order_writes_stay_sorted() {
        let mut root = [0u8; 60];
        init_empty_root(&mut root);
        assert!(try_append_leaf(&mut root, 0, 100, 1));
        assert!(try_append_leaf(&mut root, 16, 200, 1));
        assert!(try_append_leaf(&mut root, 15, 300, 1));

        assert_eq!(
            entries(&root),
            vec![(0, 1, 100), (15, 1, 300), (16, 1, 200)]
        );
    }

    #[test]
    fn inserts_between_existing_extents() {
        let mut root = [0u8; 60];
        init_empty_root(&mut root);
        assert!(try_append_leaf(&mut root, 0, 100, 2));
        assert!(try_append_leaf(&mut root, 10, 200, 2));
        assert!(try_append_leaf(&mut root, 5, 150, 2));

        assert_eq!(entries(&root), vec![(0, 2, 100), (5, 2, 150), (10, 2, 200)]);
    }

    #[test]
    fn insertion_merges_both_neighbors() {
        let mut root = [0u8; 60];
        init_empty_root(&mut root);
        assert!(try_append_leaf(&mut root, 0, 100, 2));
        assert!(try_append_leaf(&mut root, 4, 104, 2));
        assert!(try_append_leaf(&mut root, 2, 102, 2));

        assert_eq!(entries(&root), vec![(0, 6, 100)]);
    }

    #[test]
    fn full_root_can_merge_but_rejects_isolated_insertion() {
        let mut root = [0u8; 60];
        init_empty_root(&mut root);
        for (logical, physical) in [(0, 100), (10, 200), (20, 300), (30, 400)] {
            assert!(try_append_leaf(&mut root, logical, physical, 1));
        }

        assert!(try_append_leaf(&mut root, 1, 101, 1));
        assert_eq!(entries(&root)[0], (0, 2, 100));
        let unchanged = root;
        assert!(!try_append_leaf(&mut root, 5, 500, 1));
        assert_eq!(root, unchanged);
    }

    #[test]
    fn overlapping_insertion_is_rejected_without_changes() {
        let mut root = [0u8; 60];
        init_empty_root(&mut root);
        assert!(try_append_leaf(&mut root, 8, 100, 4));
        let unchanged = root;

        assert!(!try_append_leaf(&mut root, 7, 200, 2));
        assert!(!try_append_leaf(&mut root, 10, 300, 4));
        assert_eq!(root, unchanged);
    }

    #[test]
    fn allocation_run_stops_at_next_extent() {
        let mut root = [0u8; 60];
        init_empty_root(&mut root);
        assert!(try_append_leaf(&mut root, 0, 100, 2));
        assert!(try_append_leaf(&mut root, 16, 200, 4));

        assert_eq!(clip_unmapped_run(&root, 2, 100), Some(14));
        assert_eq!(clip_unmapped_run(&root, 15, 8), Some(1));
        assert_eq!(clip_unmapped_run(&root, 16, 8), None);
        assert_eq!(clip_unmapped_run(&root, 20, 8), Some(8));
    }
}
