//! 目录写路径:在最后一个目录块末尾插入一条新 `dir_entry_2`,或者
//! 把某条旧 entry 标为空(`ino=0`)并合并 rec_len 到前一条。
//!
//! ext 目录特点:每条 entry 的 `rec_len` 覆盖到下一条开头,最后一条
//! rec_len 扩到块尾。插入时找某条 "slack" 足够装下新 entry,就地分裂;
//! 否则走"向文件追加一个新目录块"。

use alloc::vec;

use crate::layout::EXT4_EXTENTS_FL;
use crate::state::{BlockBackendError, FsState};
use crate::{extent_wr, map_wr};

#[inline]
fn write_entry(
    dst: &mut [u8],
    rec_len: u16,
    new_ino: u32,
    file_type: u8,
    name: &str,
    has_filetype: bool,
) {
    let name_bytes = name.as_bytes();
    dst.fill(0);
    dst[0..4].copy_from_slice(&new_ino.to_le_bytes());
    dst[4..6].copy_from_slice(&rec_len.to_le_bytes());
    if has_filetype {
        dst[6] = name_bytes.len() as u8;
        dst[7] = file_type;
    } else {
        let nl = name_bytes.len() as u16;
        dst[6..8].copy_from_slice(&nl.to_le_bytes());
    }
    dst[8..8 + name_bytes.len()].copy_from_slice(name_bytes);
}

#[inline]
fn needed(name_len: usize) -> u16 {
    ((8 + name_len + 3) & !3) as u16
}

/// 扫一个目录块,尝试在里面就地插入一条新 entry。成功返回 `true`,
/// 失败(没 slack)返回 `false`。
fn try_insert_in_block(
    block: &mut [u8],
    new_ino: u32,
    file_type: u8,
    name: &str,
    has_filetype: bool,
) -> bool {
    let need = needed(name.len());
    let mut off = 0usize;
    while off + 8 <= block.len() {
        let entry_ino =
            u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        let rec_len = u16::from_le_bytes([block[off + 4], block[off + 5]]) as usize;
        if rec_len < 8 || off + rec_len > block.len() {
            return false;
        }
        let name_len = if has_filetype {
            block[off + 6] as usize
        } else {
            u16::from_le_bytes([block[off + 6], block[off + 7]]) as usize
        };
        let real = if entry_ino == 0 {
            0u16
        } else {
            needed(name_len)
        };
        if real as usize > rec_len {
            return false;
        }
        let slack = rec_len as u16 - real;
        if slack >= need {
            // 在当前条目后插入
            // 1) 把当前条目的 rec_len 缩到 real(或如果 ino==0,则完全替换)
            if entry_ino == 0 {
                // 空洞整条替换,rec_len 继承原 slot,避免额外分配临时 Vec。
                write_entry(
                    &mut block[off..off + rec_len],
                    rec_len as u16,
                    new_ino,
                    file_type,
                    name,
                    has_filetype,
                );
                return true;
            } else {
                block[off + 4..off + 6].copy_from_slice(&real.to_le_bytes());
                // 2) 在当前条目后写新条目,rec_len 扩大到吃掉 slack。
                let start = off + real as usize;
                write_entry(
                    &mut block[start..start + slack as usize],
                    slack,
                    new_ino,
                    file_type,
                    name,
                    has_filetype,
                );
                return true;
            }
        }
        off += rec_len;
    }
    false
}

/// 在目录 inode 里插入一条 entry。`i_block` 为 inode 的 60 字节 i_block 区
/// 的可变引用;`size` 为当前目录 i_size,也会按需更新(通过返回值)。
///
/// 返回新的目录大小(调用方要写回 inode.i_size)。
pub(crate) fn insert_entry(
    state: &FsState,
    i_block: &mut [u8],
    flags: &mut u32,
    size: u64,
    ino: u32,
    file_type: u8,
    name: &str,
) -> Result<u64, BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let total_blocks = (size + bs as u64 - 1) / bs as u64;
    let has_filetype = state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE != 0;

    let mut buf = vec![0u8; bs];
    for lb in 0..total_blocks {
        let phys = crate::dir::resolve_block(state, i_block, *flags, lb as u32)?;
        match phys {
            Some(p) => {
                state.read_block(p, &mut buf)?;
                if try_insert_in_block(&mut buf, ino, file_type, name, has_filetype) {
                    state.write_block(p, &buf)?;
                    return Ok(size);
                }
            }
            None => {}
        }
    }

    // 需要追加一个新目录块
    let new_lb = total_blocks as u32;
    let new_phys = if *flags & EXT4_EXTENTS_FL != 0 {
        extent_wr::ensure_block_in_extent(state, i_block, new_lb)?
            .ok_or(BlockBackendError::Unsupported)?
    } else {
        map_wr::ensure_block(state, i_block, new_lb)?
    };
    let mut blk = vec![0u8; bs];
    // 新块里先放一条占满整个块的 entry
    let rec_len = bs as u16;
    write_entry(
        &mut blk[..rec_len as usize],
        rec_len,
        ino,
        file_type,
        name,
        has_filetype,
    );
    state.write_block(new_phys, &blk)?;
    Ok(size + bs as u64)
}

/// 从目录里按名字删除一条 entry。返回 `true` 表示找到并删除。
pub(crate) fn remove_entry(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    size: u64,
    name: &str,
) -> Result<bool, BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    let total_blocks = (size + bs as u64 - 1) / bs as u64;
    let has_filetype = state.ext_sb.feature_incompat & crate::layout::INCOMPAT_FILETYPE != 0;
    let mut buf = vec![0u8; bs];
    for lb in 0..total_blocks {
        let phys = crate::dir::resolve_block(state, i_block, flags, lb as u32)?;
        let p = match phys {
            Some(p) => p,
            None => continue,
        };
        state.read_block(p, &mut buf)?;
        if remove_in_block(&mut buf, name, has_filetype) {
            state.write_block(p, &buf)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_in_block(block: &mut [u8], name: &str, has_filetype: bool) -> bool {
    let mut off = 0usize;
    let mut prev: Option<usize> = None;
    while off + 8 <= block.len() {
        let ino = u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        let rec_len = u16::from_le_bytes([block[off + 4], block[off + 5]]) as usize;
        let name_len = if has_filetype {
            block[off + 6] as usize
        } else {
            u16::from_le_bytes([block[off + 6], block[off + 7]]) as usize
        };
        if rec_len < 8 || off + rec_len > block.len() {
            return false;
        }
        if ino != 0 && name_len == name.len() {
            let n = &block[off + 8..off + 8 + name_len];
            if n == name.as_bytes() {
                // 两种删法:若有 prev,把 prev 的 rec_len 加上本条;否则把 ino=0 置空
                if let Some(p) = prev {
                    let prev_rec = u16::from_le_bytes([block[p + 4], block[p + 5]]) as usize;
                    let merged = (prev_rec + rec_len) as u16;
                    block[p + 4..p + 6].copy_from_slice(&merged.to_le_bytes());
                } else {
                    block[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                }
                return true;
            }
        }
        prev = Some(off);
        off += rec_len;
    }
    false
}

/// 初始化一个新目录的第一个块:写入 "." 和 ".."。返回块的字节数组(调用方
/// 自己 `write_block`)。
///
/// `has_filetype` 控制 byte 7:true 时填 DT_DIR (2);false 时填 0
/// (byte 7 是 name_len 高字节,name_len 在 {1,2} 时必为 0)。
pub(crate) fn make_init_dir_block(
    block_size: u32,
    self_ino: u32,
    parent_ino: u32,
    has_filetype: bool,
) -> alloc::vec::Vec<u8> {
    let bs = block_size as usize;
    let mut blk = alloc::vec![0u8; bs];
    let ft_byte: u8 = if has_filetype { 2 } else { 0 };
    // "." entry: ino=self_ino, rec_len=12, name_len=1, file_type=DT_DIR
    blk[0..4].copy_from_slice(&self_ino.to_le_bytes());
    blk[4..6].copy_from_slice(&12u16.to_le_bytes());
    blk[6] = 1;
    blk[7] = ft_byte;
    blk[8] = b'.';
    // ".." entry: ino=parent, rec_len=(bs-12), name_len=2, file_type=DT_DIR
    let rec2 = (bs - 12) as u16;
    blk[12..16].copy_from_slice(&parent_ino.to_le_bytes());
    blk[16..18].copy_from_slice(&rec2.to_le_bytes());
    blk[18] = 2;
    blk[19] = ft_byte;
    blk[20] = b'.';
    blk[21] = b'.';
    blk
}

/// 把一个目录的 `..` entry 的 ino 字段改写为 `new_parent_ino`。
/// 用于目录被 rename 到新父目录的场景。
///
/// 要求目录的第一个数据块包含 "." 和 "..";普通 mkdir 创建的目录都符合。
pub(crate) fn update_dotdot(
    state: &FsState,
    i_block: &[u8],
    flags: u32,
    new_parent_ino: u32,
) -> Result<(), BlockBackendError> {
    let bs = state.ext_sb.block_size as usize;
    // 第一个逻辑块号(目录起始块)
    let phys = crate::dir::resolve_block(state, i_block, flags, 0)?;
    let phys = match phys {
        Some(p) => p,
        None => return Err(BlockBackendError::OutOfRange),
    };
    let mut blk = vec![0u8; bs];
    state.read_block(phys, &mut blk)?;

    // 线性扫描找 name == ".."
    let mut off = 0usize;
    while off + 8 <= blk.len() {
        let rec_len = u16::from_le_bytes([blk[off + 4], blk[off + 5]]) as usize;
        let name_len = blk[off + 6] as usize;
        if rec_len < 8 || off + rec_len > blk.len() {
            return Err(BlockBackendError::OutOfRange);
        }
        if name_len == 2 && off + 8 + 2 <= blk.len() && &blk[off + 8..off + 10] == b".." {
            blk[off..off + 4].copy_from_slice(&new_parent_ino.to_le_bytes());
            state.write_block(phys, &blk)?;
            return Ok(());
        }
        off += rec_len;
    }
    // 没找到就不动(极少见 — 只发生在人为损坏目录)
    Ok(())
}
