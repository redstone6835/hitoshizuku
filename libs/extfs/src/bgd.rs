//! 块组描述符(32/64 bit)加载与访问。
//!
//! 32 位描述符 32 字节一条,仅使用低 32 位块号;64 位描述符 64 字节,
//! 新增 `*_hi` 字段存放高 32 位,以支持 `INCOMPAT_64BIT`。
//!
//! 只读驱动里我们主要用到 `bg_inode_table{_lo,_hi}` 来定位 inode 表。

use alloc::vec;
use alloc::vec::Vec;

use crate::crc;
use crate::sb::Superblock;
use crate::state::{BlockBackend, BlockBackendError};

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct GroupDesc {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub flags: u16,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub used_dirs_count: u32,
}

/// 读取所有块组描述符到内存;小文件系统(~GB)下总量很小,无需缓存。
pub(crate) fn load_all(
    backend: &dyn BlockBackend,
    sb: &Superblock,
) -> Result<Vec<GroupDesc>, BlockBackendError> {
    let group_count = sb.groups_count as usize;
    let desc_size = sb.desc_size as usize;
    let total_bytes = group_count * desc_size;

    // 描述符表从 block `first_data_block + 1` 开始。
    let first_gdt_block = sb.first_data_block as u64 + 1;
    let gdt_blocks = ((total_bytes as u64) + sb.block_size as u64 - 1) / sb.block_size as u64;

    let mut buf = vec![0u8; (gdt_blocks * sb.block_size as u64) as usize];
    read_blocks(backend, sb, first_gdt_block, gdt_blocks as u32, &mut buf)?;

    let mut out = Vec::with_capacity(group_count);
    for i in 0..group_count {
        let raw = &buf[i * desc_size..i * desc_size + desc_size];
        let block_bitmap_lo = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as u64;
        let inode_bitmap_lo = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as u64;
        let inode_table_lo = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as u64;
        let flags = u16::from_le_bytes([raw[18], raw[19]]);

        let (block_bitmap, inode_bitmap, inode_table) = if desc_size == 64 {
            let block_hi = u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]) as u64;
            let inode_hi = u32::from_le_bytes([raw[36], raw[37], raw[38], raw[39]]) as u64;
            let table_hi = u32::from_le_bytes([raw[40], raw[41], raw[42], raw[43]]) as u64;
            (
                (block_hi << 32) | block_bitmap_lo,
                (inode_hi << 32) | inode_bitmap_lo,
                (table_hi << 32) | inode_table_lo,
            )
        } else {
            (block_bitmap_lo, inode_bitmap_lo, inode_table_lo)
        };

        // METADATA_CSUM 下,bg_checksum 覆盖 (group_nr, gd 内容除 csum 外)
        if sb.metadata_csum {
            let expect = u16::from_le_bytes([raw[0x1e], raw[0x1f]]);
            let mut tmp = [0u8; 64];
            let n = desc_size.min(64);
            tmp[..n].copy_from_slice(&raw[..n]);
            tmp[0x1e] = 0;
            tmp[0x1f] = 0;
            let mut seed = sb.csum_seed;
            let gnr = i as u32;
            seed = crc::update(seed, &gnr.to_le_bytes());
            let actual = (crc::update(seed, &tmp[..n]) & 0xffff) as u16;
            if actual != expect {
                return Err(BlockBackendError::OutOfRange);
            }
        }

        // 读取 free / used 计数(lo + hi,hi 仅 64bit 描述符有)
        let free_blocks_lo = u16::from_le_bytes([raw[12], raw[13]]) as u32;
        let free_inodes_lo = u16::from_le_bytes([raw[14], raw[15]]) as u32;
        let used_dirs_lo = u16::from_le_bytes([raw[16], raw[17]]) as u32;
        let (free_blocks_count, free_inodes_count, used_dirs_count) = if desc_size == 64 {
            let fb_hi = u16::from_le_bytes([raw[44], raw[45]]) as u32;
            let fi_hi = u16::from_le_bytes([raw[46], raw[47]]) as u32;
            let ud_hi = u16::from_le_bytes([raw[48], raw[49]]) as u32;
            (
                (fb_hi << 16) | free_blocks_lo,
                (fi_hi << 16) | free_inodes_lo,
                (ud_hi << 16) | used_dirs_lo,
            )
        } else {
            (free_blocks_lo, free_inodes_lo, used_dirs_lo)
        };

        out.push(GroupDesc {
            block_bitmap,
            inode_bitmap,
            inode_table,
            flags,
            free_blocks_count,
            free_inodes_count,
            used_dirs_count,
        });
    }
    Ok(out)
}

/// 以 `sb.block_size` 为单位从 `start_block` 起读 `count` 个块。
pub(crate) fn read_blocks(
    backend: &dyn BlockBackend,
    sb: &Superblock,
    start_block: u64,
    count: u32,
    out: &mut [u8],
) -> Result<(), BlockBackendError> {
    let ss = backend.sector_size() as u64;
    let per_block = sb.block_size as u64 / ss;
    if per_block == 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    let lba = start_block * per_block;
    let sectors = (count as u64) * per_block;
    backend.read_sectors(lba, sectors as u32, out)
}

/// 同上,写路径。
pub(crate) fn write_blocks(
    backend: &dyn BlockBackend,
    sb: &Superblock,
    start_block: u64,
    count: u32,
    buf: &[u8],
) -> Result<(), BlockBackendError> {
    let ss = backend.sector_size() as u64;
    let per_block = sb.block_size as u64 / ss;
    if per_block == 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    let lba = start_block * per_block;
    let sectors = (count as u64) * per_block;
    backend.write_sectors(lba, sectors as u32, buf)
}

/// 把第 `group` 个块组描述符(内容已由调用方在 `GroupDesc` 里更新)写回磁盘。
/// 如果启用了 METADATA_CSUM 则重算 `bg_checksum`。
pub(crate) fn write_desc(
    backend: &dyn BlockBackend,
    sb: &Superblock,
    group: u32,
    desc: &GroupDesc,
    free_blocks_count: u32,
    free_inodes_count: u32,
    used_dirs_count: u32,
) -> Result<(), BlockBackendError> {
    let desc_size = sb.desc_size as usize;
    let first_gdt_block = sb.first_data_block as u64 + 1;
    let per_block = sb.block_size as usize / desc_size;
    if per_block == 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    let blk_index = group as u64 / per_block as u64;
    let in_blk = (group as usize % per_block) * desc_size;
    let blk_lba = first_gdt_block + blk_index;

    let mut blk = vec![0u8; sb.block_size as usize];
    read_blocks(backend, sb, blk_lba, 1, &mut blk)?;

    let raw = &mut blk[in_blk..in_blk + desc_size];
    // lo 32bit of bitmap/inode_table
    raw[0..4].copy_from_slice(&(desc.block_bitmap as u32).to_le_bytes());
    raw[4..8].copy_from_slice(&(desc.inode_bitmap as u32).to_le_bytes());
    raw[8..12].copy_from_slice(&(desc.inode_table as u32).to_le_bytes());
    raw[12..14].copy_from_slice(&(free_blocks_count as u16).to_le_bytes());
    raw[14..16].copy_from_slice(&(free_inodes_count as u16).to_le_bytes());
    raw[16..18].copy_from_slice(&(used_dirs_count as u16).to_le_bytes());
    raw[18..20].copy_from_slice(&desc.flags.to_le_bytes());

    if desc_size == 64 {
        raw[32..36].copy_from_slice(&((desc.block_bitmap >> 32) as u32).to_le_bytes());
        raw[36..40].copy_from_slice(&((desc.inode_bitmap >> 32) as u32).to_le_bytes());
        raw[40..44].copy_from_slice(&((desc.inode_table >> 32) as u32).to_le_bytes());
        let free_blk_hi = (free_blocks_count >> 16) as u16;
        let free_ino_hi = (free_inodes_count >> 16) as u16;
        let used_dirs_hi = (used_dirs_count >> 16) as u16;
        raw[44..46].copy_from_slice(&free_blk_hi.to_le_bytes());
        raw[46..48].copy_from_slice(&free_ino_hi.to_le_bytes());
        raw[48..50].copy_from_slice(&used_dirs_hi.to_le_bytes());
    }

    // 重算 checksum
    if sb.metadata_csum {
        raw[0x1e] = 0;
        raw[0x1f] = 0;
        let mut tmp = [0u8; 64];
        let n = desc_size.min(64);
        tmp[..n].copy_from_slice(&raw[..n]);
        let mut seed = sb.csum_seed;
        seed = crc::update(seed, &group.to_le_bytes());
        let sum = (crc::update(seed, &tmp[..n]) & 0xffff) as u16;
        raw[0x1e..0x20].copy_from_slice(&sum.to_le_bytes());
    }

    write_blocks(backend, sb, blk_lba, 1, &blk)?;
    Ok(())
}
