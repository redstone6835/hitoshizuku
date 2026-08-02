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
        if has_filetype && block[off + 6] == 0 && block[off + 7] == DIR_TAIL_MARKER {
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
    csum_ctx: Option<(u32, u32)>,
) -> Result<Vec<(u32, u32, u64)>, BlockBackendError> {
    if total_blocks > u32::MAX as u64 {
        return Err(BlockBackendError::OutOfRange);
    }
    let count = total_blocks as u32;
    if flags & crate::layout::EXT4_EXTENTS_FL != 0 {
        crate::extent::map_contiguous(state, i_block, 0, count, csum_ctx)
    } else {
        crate::map::map_contiguous(state, i_block, 0, count)
    }
}

/// HTree 目录在线性扫描时需要特殊处理的逻辑块集合。
///
/// - 根块(逻辑块 0)是 dx_root:`..` 的 rec_len 覆盖整块,扫描安全,
///   但块尾是 dx tail 而非 dir tail,必须跳过 dir 尾校验;
/// - 二级索引时,dx_root 的 entries 指向若干 dx_node 块:它们不是叶块,
///   必须整体跳过(扫描与校验都跳过)。
#[derive(Default)]
struct DxSkip {
    indexed: bool,
    blocks: Vec<u32>,
}

impl DxSkip {
    #[inline]
    fn skip_block(&self, lb: u32) -> bool {
        self.blocks.contains(&lb)
    }
    /// 该块是否需要做 dir 尾校验(叶块才校验)。
    #[inline]
    fn verify_tail(&self, lb: u32) -> bool {
        !(self.indexed && lb == 0) && !self.skip_block(lb)
    }
}

/// 解析 HTree 根块,构造 [`DxSkip`]。非索引目录返回默认(全不跳过)。
fn load_dx_skip(state: &FsState, i_block: &[u8], flags: u32) -> Result<DxSkip, BlockBackendError> {
    if flags & crate::layout::EXT4_INDEX_FL == 0 {
        return Ok(DxSkip::default());
    }
    let mut skip = DxSkip {
        indexed: true,
        blocks: Vec::new(),
    };
    let Some(phys) = resolve_block(state, i_block, flags, 0)? else {
        return Ok(skip);
    };
    let bs = state.ext_sb.block_size as usize;
    let mut block = vec![0u8; bs];
    state.read_block(phys, &mut block)?;
    // dx_root 布局:dirent(".") 12B + dirent("..") 12B + dx_root_info 8B +
    // dx_countlimit 4B + dx_entry[count]。
    if bs < 36 {
        return Ok(skip);
    }
    let indirect_levels = block[30];
    let count = u16::from_le_bytes([block[34], block[35]]) as usize;
    if indirect_levels == 0 {
        // 一级索引:没有独立 dx_node 块。
        return Ok(skip);
    }
    let max_entries = (bs - 36) / 8;
    for i in 0..count.min(max_entries) {
        let node = u32::from_le_bytes([
            block[36 + i * 8 + 4],
            block[36 + i * 8 + 5],
            block[36 + i * 8 + 6],
            block[36 + i * 8 + 7],
        ]);
        skip.blocks.push(node);
    }
    Ok(skip)
}

/// 目录叶块尾部校验(`ext4_dir_entry_tail`,与 Linux `ext4_dirblock_csum_verify` 一致)。
fn verify_dir_tail(
    state: &FsState,
    block: &[u8],
    csum_ctx: Option<(u32, u32)>,
) -> Result<(), BlockBackendError> {
    let Some((ino, generation)) = csum_ctx else {
        return Ok(());
    };
    if !state.ext_sb.metadata_csum {
        return Ok(());
    }
    if state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE == 0 {
        return Ok(());
    }
    let bs = block.len();
    if bs < 12 {
        return Err(BlockBackendError::Io);
    }
    let off = bs - 12;
    let zero1 = u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
    let rec_len = u16::from_le_bytes([block[off + 4], block[off + 5]]);
    if zero1 != 0 || rec_len != 12 || block[off + 6] != 0 || block[off + 7] != 0xde {
        return Err(BlockBackendError::Io);
    }
    let provided = u32::from_le_bytes([
        block[off + 8],
        block[off + 9],
        block[off + 10],
        block[off + 11],
    ]);
    let mut seed = state.ext_sb.csum_seed;
    seed = crate::crc::update(seed, &ino.to_le_bytes());
    seed = crate::crc::update(seed, &generation.to_le_bytes());
    let sum = crate::crc::update(seed, &block[..off]);
    if sum != provided {
        return Err(BlockBackendError::Io);
    }
    Ok(())
}

fn visit_mapped_dir_blocks<F: FnMut(&[u8]) -> bool>(
    state: &FsState,
    ranges: &[(u32, u32, u64)],
    dx_skip: &DxSkip,
    csum_ctx: Option<(u32, u32)>,
    mut f: F,
) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let mut buf = vec![0u8; bs * DIR_READ_CHUNK_BLOCKS as usize];
    for (range_lb, range_count, phys_start) in ranges {
        let mut done = 0u32;
        while done < *range_count {
            let chunk = (*range_count - done).min(DIR_READ_CHUNK_BLOCKS);
            let bytes = chunk as usize * bs;
            state.read_blocks(*phys_start + done as u64, chunk, &mut buf[..bytes])?;
            for idx in 0..chunk as usize {
                let lb = *range_lb + done + idx as u32;
                if dx_skip.skip_block(lb) {
                    continue;
                }
                let start = idx * bs;
                let block = &buf[start..start + bs];
                if dx_skip.verify_tail(lb) {
                    verify_dir_tail(state, block, csum_ctx)?;
                }
                if !f(block) {
                    return Ok(());
                }
            }
            done += chunk;
        }
    }
    Ok(())
}

/// 遍历目录条目,由调用方决定是否构造集合或提前停止。
///
/// 目录 open 只需要生成 VFS 快照,不需要先构造 `Vec<DirEntryRaw>`；该 visitor
/// 保留原有扫描顺序,同时让调用方少一次中间集合分配。
/// `csum_ctx = Some((ino, generation))` 时对目录叶块做 METADATA_CSUM 校验。
pub(crate) fn visit_entries<F>(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
    csum_ctx: Option<(u32, u32)>,
    mut f: F,
) -> Result<(), BlockBackendError>
where
    F: FnMut(&DirEntryRaw) -> bool,
{
    let block_size = state.ext_sb.block_size as u64;
    let total_blocks = (size + block_size - 1) / block_size;
    let has_ft = state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE != 0;
    let ranges = mapped_ranges(state, i_block, flags, total_blocks, csum_ctx)?;
    let dx_skip = load_dx_skip(state, i_block, flags)?;
    visit_mapped_dir_blocks(state, &ranges, &dx_skip, csum_ctx, |block| {
        scan_dir_block(block, has_ft, |e| f(e))
    })
}

/// 流式判断目录是否为空,只忽略 `.` / `..` 两个固定项。
///
/// `rmdir` 和 rename 覆盖空目录只需要布尔结果。直接在目录块内比较原始名称,
/// 可以避免为整个目录构造 `Vec<DirEntryRaw>` 和 `String`。
pub(crate) fn is_dir_empty(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
    csum_ctx: Option<(u32, u32)>,
) -> Result<bool, BlockBackendError> {
    let block_size = state.ext_sb.block_size as u64;
    let total_blocks = (size + block_size - 1) / block_size;
    let has_ft = state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE != 0;
    let ranges = mapped_ranges(state, i_block, flags, total_blocks, csum_ctx)?;
    let dx_skip = load_dx_skip(state, i_block, flags)?;
    let mut empty = true;
    visit_mapped_dir_blocks(state, &ranges, &dx_skip, csum_ctx, |block| {
        scan_dir_block_bytes(block, has_ft, |_, _, name_bytes| {
            if name_bytes == b"." || name_bytes == b".." {
                return true;
            }
            empty = false;
            false
        })
    })?;
    Ok(empty)
}

/// 在目录中查找一个名字。成功时提前停止扫描,避免为整目录构造 `Vec`。
pub(crate) fn find_entry(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
    name: &str,
    csum_ctx: Option<(u32, u32)>,
    casefold: bool,
) -> Result<Option<DirEntryRaw>, BlockBackendError> {
    find_entry_impl(
        state,
        i_block,
        flags,
        size,
        name.as_bytes(),
        csum_ctx,
        casefold,
    )
    .map(|hit| {
        hit.map(|(ino, file_type)| DirEntryRaw {
            ino,
            file_type,
            name: String::from(name),
        })
    })
}

/// 字节级 [`find_entry`]:fast-commit 回放里的文件名来自日志原始字节,
/// 不做 UTF-8 有损转换。返回 `(ino, file_type)`。
pub(crate) fn find_entry_bytes(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
    name: &[u8],
    csum_ctx: Option<(u32, u32)>,
) -> Result<Option<(u32, u8)>, BlockBackendError> {
    find_entry_impl(state, i_block, flags, size, name, csum_ctx, false)
}

#[allow(clippy::too_many_arguments)]
fn find_entry_impl(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
    name: &[u8],
    csum_ctx: Option<(u32, u32)>,
    casefold: bool,
) -> Result<Option<(u32, u8)>, BlockBackendError> {
    let block_size = state.ext_sb.block_size as u64;
    let total_blocks = (size + block_size - 1) / block_size;
    let has_ft = state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE != 0;
    let ranges = mapped_ranges(state, i_block, flags, total_blocks, csum_ctx)?;
    let dx_skip = load_dx_skip(state, i_block, flags)?;
    let mut found: Option<(u32, u8)> = None;
    visit_mapped_dir_blocks(state, &ranges, &dx_skip, csum_ctx, |block| {
        scan_dir_block_bytes(block, has_ft, |ino, file_type, name_bytes| {
            let hit = if casefold {
                name_bytes.eq_ignore_ascii_case(name)
            } else {
                name_bytes == name
            };
            if hit {
                found = Some((ino, file_type));
                return false;
            }
            true
        })
    })?;
    Ok(found)
}

/// 逻辑块号 -> 物理块号:ext4 用 extent,否则走间接块。inline_data 已在
/// 调用方提前处理。本函数不做 extent 尾校验(写路径/恢复路径使用)。
pub(crate) fn resolve_block(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    logical_block: u32,
) -> Result<Option<u64>, BlockBackendError> {
    if flags & crate::layout::EXT4_EXTENTS_FL != 0 {
        crate::extent::map_block_unverified(state, i_block, logical_block)
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
