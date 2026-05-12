//! ext 目录读取(线性扫描,兼容 HTree 目录)。
//!
//! 每个目录块由变长的 `ext4_dir_entry_2` 串成:
//!
//! ```text
//! +-----+-----+-----------+-----------+-----------+
//! | ino | rec | name_len  | file_type | name ...  |
//! |  4  |  2  |   1       |   1       |  name_len |
//! +-----+-----+-----------+-----------+-----------+
//! ```
//!
//! `rec_len` 跳到下一条。`ino == 0` 的条目是"已删除 slot"(仍占 rec_len 字节)。
//! 块尾可能有 12 字节的 `ext4_dir_entry_tail`(METADATA_CSUM 时),本实现
//! 遇到 name_len==0xDE 的 reserved tail 条目会跳过。
//!
//! HTree(`EXT4_INDEX_FL`)目录的索引块混在 i_block 里,但所有 *叶块* 仍然
//! 是标准的线性目录块。只读驱动只需按 `i_size` 遍历数据块的线性条目集合,
//! 忽略 HTree 索引;正确性不受影响,仅不享受 O(log n) 查找。

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::layout::{DT_DIR, DT_LNK, DT_REG};
use crate::state::{BlockBackendError, FsState};

#[derive(Debug, Clone)]
pub(crate) struct DirEntryRaw {
    pub ino: u32,
    pub file_type: u8,
    pub name: String,
}

const DIR_TAIL_MARKER: u8 = 0xde;
const DIR_READ_CHUNK_BLOCKS: u32 = 32;

fn scan_dir_block_bytes<F: FnMut(u32, u8, &[u8]) -> bool>(
    block: &[u8],
    has_filetype: bool,
    mut f: F,
) -> bool {
    let mut off = 0usize;
    while off + 8 <= block.len() {
        let ino = u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        let rec_len = u16::from_le_bytes([block[off + 4], block[off + 5]]) as usize;
        if rec_len < 8 || off + rec_len > block.len() {
            break;
        }
        // checksum tail 只在 INCOMPAT_FILETYPE 时才会出现(两字段布局才合法)
        if has_filetype && block[off + 6] == DIR_TAIL_MARKER {
            off += rec_len;
            continue;
        }
        let (name_len, file_type) = if has_filetype {
            (block[off + 6] as usize, block[off + 7])
        } else {
            // 经典 ext2:byte 6,7 合并成小端 u16 name_len;没 file_type 信息
            let nl = u16::from_le_bytes([block[off + 6], block[off + 7]]) as usize;
            (nl, crate::layout::DT_UNKNOWN)
        };
        if ino != 0 && name_len != 0 && (off + 8 + name_len) <= off + rec_len {
            let name_bytes = &block[off + 8..off + 8 + name_len];
            if !f(ino, file_type, name_bytes) {
                return false;
            }
        }
        off += rec_len;
    }
    true
}

/// 扫描一个目录数据块,回调每个活跃条目。若回调返回 `false` 则提前结束。
///
/// `has_filetype` 控制 byte 7 的解释:true 时是 file_type(DT_*),
/// false 时它是 name_len 的高字节(ext2 无 `INCOMPAT_FILETYPE` 时启用)。
pub(crate) fn scan_dir_block<F: FnMut(&DirEntryRaw) -> bool>(
    block: &[u8],
    has_filetype: bool,
    mut f: F,
) -> bool {
    scan_dir_block_bytes(block, has_filetype, |ino, file_type, name_bytes| {
        let name = core::str::from_utf8(name_bytes)
            .map(String::from)
            .unwrap_or_else(|_| {
                let mut s = String::with_capacity(name_bytes.len());
                for &b in name_bytes {
                    if b.is_ascii() {
                        s.push(b as char);
                    } else {
                        s.push('\u{fffd}');
                    }
                }
                s
            });
        let entry = DirEntryRaw {
            ino,
            file_type,
            name,
        };
        f(&entry)
    })
}

fn mapped_ranges(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    total_blocks: u64,
) -> Result<Vec<(u32, u32, u64)>, BlockBackendError> {
    if total_blocks > u32::MAX as u64 {
        return Err(BlockBackendError::OutOfRange);
    }
    let count = total_blocks as u32;
    if flags & crate::layout::EXT4_EXTENTS_FL != 0 {
        crate::extent::map_contiguous(state, i_block, 0, count)
    } else {
        crate::map::map_contiguous(state, i_block, 0, count)
    }
}

fn visit_mapped_dir_blocks<F: FnMut(&[u8]) -> bool>(
    state: &FsState,
    ranges: &[(u32, u32, u64)],
    mut f: F,
) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let mut buf = vec![0u8; bs * DIR_READ_CHUNK_BLOCKS as usize];
    for (_, range_count, phys_start) in ranges {
        let mut done = 0u32;
        while done < *range_count {
            let chunk = (*range_count - done).min(DIR_READ_CHUNK_BLOCKS);
            let bytes = chunk as usize * bs;
            state.read_blocks(*phys_start + done as u64, chunk, &mut buf[..bytes])?;
            for idx in 0..chunk as usize {
                let start = idx * bs;
                if !f(&buf[start..start + bs]) {
                    return Ok(());
                }
            }
            done += chunk;
        }
    }
    Ok(())
}

/// 读取一个文件所有已用数据块(根据 size + block_map 回调),并对每块调用
/// [`scan_dir_block`]。返回所有活跃条目的集合。
pub(crate) fn read_all_entries(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
) -> Result<Vec<DirEntryRaw>, BlockBackendError> {
    let block_size = state.ext_sb.block_size as u64;
    let total_blocks = (size + block_size - 1) / block_size;
    let has_ft = state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE != 0;
    let mut out: Vec<DirEntryRaw> = Vec::new();
    let ranges = mapped_ranges(state, i_block, flags, total_blocks)?;
    visit_mapped_dir_blocks(state, &ranges, |block| {
        scan_dir_block(block, has_ft, |e| {
            out.push(e.clone());
            true
        })
    })?;
    Ok(out)
}

/// 在目录中查找一个名字。成功时提前停止扫描,避免为整目录构造 `Vec`。
pub(crate) fn find_entry(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
    name: &str,
) -> Result<Option<DirEntryRaw>, BlockBackendError> {
    let block_size = state.ext_sb.block_size as u64;
    let total_blocks = (size + block_size - 1) / block_size;
    let has_ft = state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE != 0;
    let target = name.as_bytes();
    let ranges = mapped_ranges(state, i_block, flags, total_blocks)?;
    let mut found: Option<DirEntryRaw> = None;
    visit_mapped_dir_blocks(state, &ranges, |block| {
        scan_dir_block_bytes(block, has_ft, |ino, file_type, name_bytes| {
            if name_bytes == target {
                found = Some(DirEntryRaw {
                    ino,
                    file_type,
                    name: String::from(name),
                });
                return false;
            }
            true
        })
    })?;
    Ok(found)
}

/// 逻辑块号 -> 物理块号:ext4 用 extent,否则走间接块。inline_data 已在
/// 调用方提前处理。
pub(crate) fn resolve_block(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    logical_block: u32,
) -> Result<Option<u64>, BlockBackendError> {
    if flags & crate::layout::EXT4_EXTENTS_FL != 0 {
        crate::extent::map_block(state, i_block, logical_block)
    } else {
        crate::map::map_block(state, i_block, logical_block)
    }
}

#[allow(dead_code)]
pub(crate) fn file_type_of(ty: u8) -> Option<vfs::stat::FileType> {
    match ty {
        DT_REG => Some(vfs::stat::FileType::Regular),
        DT_DIR => Some(vfs::stat::FileType::Directory),
        DT_LNK => Some(vfs::stat::FileType::Symlink),
        _ => None,
    }
}
