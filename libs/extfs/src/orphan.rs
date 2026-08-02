//! 孤儿 inode 清理(Linux `ext4_orphan_cleanup` 的等价物)。
//!
//! 两类来源:
//! - `s_last_orphan` 旧式链表(经 `i_dtime` 串联);
//! - orphan file(`COMPAT_ORPHAN_FILE` + `s_orphan_file_inum`,
//!   块内 `u32` inode 号数组,块尾 `ext4_orphan_block_tail`)。
//!
//! 每个孤儿 inode 的处理与内核一致:`i_nlink > 0` 表示崩溃时正在截断,
//! 完成截断到 `i_size`;`i_nlink == 0` 表示已删除但仍被打开,释放全部
//! 数据块并回收 inode。

use alloc::vec;
use alloc::vec::Vec;

use crate::layout::*;
use crate::state::{BlockBackendError, FsState};

/// orphan file 块数的人为上限(与内核 `EXT4_MAX_ORPHAN_FILE_BLOCKS` 一致)。
const MAX_ORPHAN_FILE_BLOCKS: u64 = 512;

/// 入口:清理两类孤儿来源,并按需清除超级块里的相关指针。
pub(crate) fn cleanup(state: &FsState) -> Result<(), BlockBackendError> {
    let has_orphan_file =
        state.ext_sb.feature_compat & COMPAT_ORPHAN_FILE != 0 && state.ext_sb.orphan_file_inum != 0;
    let orphan_present = state.ext_sb.feature_ro_compat & RO_COMPAT_ORPHAN_PRESENT != 0;

    // 1) s_last_orphan 旧式链表。
    if state.ext_sb.last_orphan != 0 {
        cleanup_orphan_list(state)?;
    }

    // 2) orphan file。无论 ORPHAN_PRESENT 是否置位都扫描一遍:
    // 空文件扫描成本极低,而置位丢失时漏清会变成 inode 泄漏。
    if has_orphan_file {
        cleanup_orphan_file(state)?;
    }

    // 3) 清超级块指针(LAST_ORPHAN 与 ORPHAN_PRESENT 标记)。
    if state.ext_sb.last_orphan != 0 || orphan_present {
        crate::alloc_mod::patch_superblock(state, |sb| {
            sb[sb_off::LAST_ORPHAN..sb_off::LAST_ORPHAN + 4].copy_from_slice(&0u32.to_le_bytes());
            let ro = u32::from_le_bytes([
                sb[sb_off::FEATURE_RO_COMPAT],
                sb[sb_off::FEATURE_RO_COMPAT + 1],
                sb[sb_off::FEATURE_RO_COMPAT + 2],
                sb[sb_off::FEATURE_RO_COMPAT + 3],
            ]) & !RO_COMPAT_ORPHAN_PRESENT;
            sb[sb_off::FEATURE_RO_COMPAT..sb_off::FEATURE_RO_COMPAT + 4]
                .copy_from_slice(&ro.to_le_bytes());
        })?;
    }
    Ok(())
}

/// 旧式孤儿链表:head 在 `s_last_orphan`,后续经每个 inode 的 `i_dtime`。
fn cleanup_orphan_list(state: &FsState) -> Result<(), BlockBackendError> {
    let mut head = state.ext_sb.last_orphan;
    let mut guard: u64 = 0;
    while head != 0 {
        guard += 1;
        if guard > state.ext_sb.inodes_count as u64 {
            // 链表成环:文件系统损坏,拒绝继续。
            return Err(BlockBackendError::OutOfRange);
        }
        let raw = crate::inode_wr::read_raw(state, head)?;
        let next = raw.dtime();
        process_orphan(state, &raw)?;
        head = next;
    }
    Ok(())
}

/// orphan file:逐块读出 inode 号数组并处理,随后把条目区清零写回。
fn cleanup_orphan_file(state: &FsState) -> Result<(), BlockBackendError> {
    let orphan_inum = state.ext_sb.orphan_file_inum;
    if orphan_inum == 0 || orphan_inum > state.ext_sb.inodes_count {
        return Ok(());
    }
    let raw = crate::inode_wr::read_raw(state, orphan_inum)?;
    let bs = state.ext_sb.block_size as usize;
    let of_blocks = (raw.size() / state.ext_sb.block_size as u64).min(MAX_ORPHAN_FILE_BLOCKS);
    let inodes_per_ob = (bs - 8) / 4;
    let i_block: Vec<u8> = raw.i_block().to_vec();
    let flags = raw.flags();
    let generation = raw.generation();

    for blk_idx in 0..of_blocks {
        let Some(phys) = crate::dir::resolve_block(state, &i_block, flags, blk_idx as u32)? else {
            continue;
        };
        let mut block = vec![0u8; bs];
        state.read_block(phys, &mut block)?;
        // 坏 magic 的块不可信(内核视为 EIO):保守跳过而不是误删 inode。
        let tail_off = bs - 8;
        let magic = u32::from_le_bytes([
            block[tail_off],
            block[tail_off + 1],
            block[tail_off + 2],
            block[tail_off + 3],
        ]);
        if magic != EXT4_ORPHAN_BLOCK_MAGIC {
            continue;
        }
        let mut inos: Vec<u32> = Vec::new();
        for j in 0..inodes_per_ob {
            let ino = u32::from_le_bytes([
                block[j * 4],
                block[j * 4 + 1],
                block[j * 4 + 2],
                block[j * 4 + 3],
            ]);
            if ino != 0 {
                inos.push(ino);
            }
        }
        for ino in inos {
            if let Ok(orphan) = crate::inode_wr::read_raw(state, ino) {
                process_orphan(state, &orphan)?;
            }
        }
        // 条目区清零并写回,orphan file 回到"空"状态。
        block[..inodes_per_ob * 4].fill(0);
        if state.ext_sb.metadata_csum {
            let csum = orphan_block_csum(state, orphan_inum, generation, phys, &block);
            block[tail_off + 4..tail_off + 8].copy_from_slice(&csum.to_le_bytes());
        }
        state.write_block(phys, &block)?;
    }
    Ok(())
}

/// orphan file 块校验:seed = orphan inode 的 i_csum_seed,
/// 覆盖 `le64(物理块号) || 条目区`。
fn orphan_block_csum(
    state: &FsState,
    orphan_inum: u32,
    generation: u32,
    phys: u64,
    block: &[u8],
) -> u32 {
    let bs = state.ext_sb.block_size as usize;
    let inodes_per_ob = (bs - 8) / 4;
    let mut seed = state.ext_sb.csum_seed;
    seed = crate::crc::update(seed, &orphan_inum.to_le_bytes());
    seed = crate::crc::update(seed, &generation.to_le_bytes());
    let mut csum = crate::crc::update(seed, &phys.to_le_bytes());
    csum = crate::crc::update(csum, &block[..inodes_per_ob * 4]);
    csum
}

/// 单个孤儿 inode:nlink>0 完成截断;nlink==0 释放并回收。
fn process_orphan(
    state: &FsState,
    raw: &crate::inode_wr::RawInode,
) -> Result<(), BlockBackendError> {
    let ino = raw.ino;
    let mode = raw.mode();
    if mode == 0 {
        // 已被完全清掉的 inode(可能 fsck 已处理过):跳过。
        return Ok(());
    }
    let is_dir = mode & S_IFMT == S_IFDIR;

    if raw.nlink() != 0 {
        // 崩溃时正在 truncate:把 i_size 之后的块全部释放。
        truncate_to_size(state, raw)?;
        return Ok(());
    }

    // nlink == 0:等价于 evict 路径,释放整棵映射并回收 inode。
    let mut work = raw.clone();
    let mut ib = [0u8; 60];
    ib.copy_from_slice(work.i_block());
    let mut flags = work.flags();
    crate::extent_wr::demote_if_extent(state, &mut flags, &mut ib)?;
    crate::map_wr::free_all_blocks(state, &mut ib)?;
    work.bytes.fill(0);
    // 与 clear_deleted_inode 相同:留一个合法 dtime 避免 e2fsck 误报。
    work.set_dtime(1_700_000_000);
    crate::inode_wr::write_raw(state, &work)?;
    crate::alloc_mod::free_inode(state, ino, is_dir)?;
    Ok(())
}

/// 把孤儿 inode 截断到其当前 `i_size`(完成崩溃时未竟的 truncate)。
fn truncate_to_size(
    state: &FsState,
    raw: &crate::inode_wr::RawInode,
) -> Result<(), BlockBackendError> {
    let mut work = raw.clone();
    let size = work.size();
    let block_size = state.ext_sb.block_size as u64;
    let mut ib = [0u8; 60];
    ib.copy_from_slice(work.i_block());
    let mut flags = work.flags();

    if flags & EXT4_EXTENTS_FL != 0 {
        crate::extent_wr::demote_preserve_if_extent(state, &mut flags, &mut ib)?;
    }
    let first_free_lb = ((size + block_size - 1) / block_size) as u32;
    crate::map_wr::free_blocks_from(state, &mut ib, first_free_lb)?;

    // i_size 不在块边界时,最后一个保留块的尾部必须清零(读洞语义)。
    let tail = (size % block_size) as usize;
    if tail != 0 && size != 0 {
        let lb = (size / block_size) as u32;
        if let Some(phys) = crate::map::map_block(state, &ib, lb)? {
            let bs = block_size as usize;
            let mut blk = vec![0u8; bs];
            state.read_block(phys, &mut blk)?;
            blk[tail..].fill(0);
            state.write_block(phys, &blk)?;
        }
    }

    work.i_block_mut().copy_from_slice(&ib);
    work.set_flags(flags);
    crate::inode_wr::write_raw(state, &work)?;
    Ok(())
}
