//! ext 超级块解析 + feature 门禁。
//!
//! 从磁盘第 1024 字节开始读 1024 字节超级块;按 magic、feature_compat /
//! feature_incompat / feature_ro_compat 判别 ext2/3/4;任何未知 incompat
//! 位都会拒绝挂载。

use alloc::vec;

use crate::crc;
use crate::layout::*;
use crate::state::{BlockBackend, BlockBackendError};

/// 从超级块提取并规范化后的运行时信息。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct Superblock {
    pub kind: ExtKind,

    pub inodes_count: u32,
    pub blocks_count: u64,
    pub first_data_block: u32,
    pub block_size: u32,
    /// s_log_cluster_size:BIGALLOC 时分配单元 = block_size << log_cluster_size。
    pub log_cluster_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u32,
    pub desc_size: u32, // 块组描述符大小:32 或 64
    pub first_ino: u32,
    pub s_magic: u16,
    /// s_state(挂载时快照;VALID_FS/ERROR_FS 管理见 alloc_mod)。
    pub state: u16,

    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,

    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],

    /// 是否启用 METADATA_CSUM(读路径要校验)。
    pub metadata_csum: bool,
    /// CSUM seed:当 INCOMPAT_CSUM_SEED 置位时直接取 s_checksum_seed,
    /// 否则以 UUID 计算得到。
    pub csum_seed: u32,

    /// 当前已用 + 空闲 inode / block 计数(初始化 FsState 时用)。
    pub free_blocks_count: u64,
    /// 仅特权进程可使用的保留块总数。
    pub reserved_blocks_count: u64,
    pub free_inodes_count: u32,

    /// ext4 orphan_file inode 号(COMPAT_ORPHAN_FILE 时有效)。
    pub orphan_file_inum: u32,
    /// s_journal_inum:内部日志 inode 号(通常为 [`EXT4_JOURNAL_INO`])。
    pub journal_inum: u32,
    /// s_journal_dev:外部日志设备号(非 0 表示外部日志,本驱动不支持)。
    pub journal_dev: u32,
    /// s_last_orphan:旧式孤儿 inode 链表头。
    pub last_orphan: u32,

    /// casefold 文件名编码(s_encoding),非 casefold 文件系统为 0。
    pub encoding: u16,

    /// MMP 块位置与检查间隔(s_mmp_block / s_mmp_update_interval)。
    pub mmp_block: u64,
    pub mmp_update_interval: u16,

    /// 因 ro_compat 语义必须退化为只读挂载(READONLY/BIGALLOC/
    /// HAS_SNAPSHOT/SHARED_BLOCKS 或未知 ro_compat 位)。
    pub force_read_only: bool,

    /// 总块组数。
    pub groups_count: u32,
}

/// 从设备读取并解析超级块。
pub(crate) fn load(backend: &dyn BlockBackend) -> Result<Superblock, BlockBackendError> {
    let sector_size = backend.sector_size() as u64;
    if sector_size < 512 || !sector_size.is_power_of_two() {
        return Err(BlockBackendError::OutOfRange);
    }
    // 超级块固定起始于字节 1024,跨若干扇区。
    let start_sector = SUPERBLOCK_OFFSET / sector_size;
    let in_sector = (SUPERBLOCK_OFFSET % sector_size) as usize;
    let sectors_needed =
        ((SUPERBLOCK_SIZE + in_sector) + sector_size as usize - 1) / sector_size as usize;
    let mut raw = vec![0u8; sectors_needed * sector_size as usize];
    backend.read_sectors(start_sector, sectors_needed as u32, &mut raw)?;
    let sb = &raw[in_sector..in_sector + SUPERBLOCK_SIZE];
    parse(sb)
}

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

fn parse(sb: &[u8]) -> Result<Superblock, BlockBackendError> {
    assert_eq!(sb.len(), SUPERBLOCK_SIZE);

    let magic = le16(sb, 56);
    if magic != SUPERBLOCK_MAGIC {
        return Err(BlockBackendError::OutOfRange);
    }

    let s_log_block_size = le32(sb, 24);
    if s_log_block_size > 6 {
        // 块大小 1KiB<<6 = 64KiB 上限,再大拒绝
        return Err(BlockBackendError::OutOfRange);
    }
    let block_size = 1024u32 << s_log_block_size;

    let inodes_count = le32(sb, 0);
    let blocks_count_lo = le32(sb, 4);
    let blocks_count_hi = le32(sb, 0x150); // s_blocks_count_hi
    let first_data_block = le32(sb, 20);
    let blocks_per_group = le32(sb, 32);
    let inodes_per_group = le32(sb, 40);
    let rev_level = le32(sb, 76);

    let feature_compat = le32(sb, 92);
    let feature_incompat = le32(sb, 96);
    let feature_ro_compat = le32(sb, 100);

    // inode_size: rev 0 默认 128;rev 1+ 用 s_inode_size(off 88)
    let inode_size = if rev_level == 0 {
        128
    } else {
        le16(sb, 88) as u32
    };
    if inode_size < 128 || !inode_size.is_power_of_two() {
        return Err(BlockBackendError::OutOfRange);
    }

    // 描述符大小:ext4 64bit 时 s_desc_size(off 254)
    let desc_size = if feature_incompat & INCOMPAT_64BIT != 0 {
        let v = le16(sb, 254) as u32;
        if v == 0 { 32 } else { v }
    } else {
        32
    };
    if desc_size != 32 && desc_size != 64 {
        return Err(BlockBackendError::OutOfRange);
    }

    let first_ino = if rev_level == 0 { 11 } else { le32(sb, 84) };

    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&sb[104..120]);
    let mut volume_name = [0u8; 16];
    volume_name.copy_from_slice(&sb[120..136]);

    // 判别变体
    let kind = if feature_incompat & INCOMPAT_EXTENTS != 0
        || feature_ro_compat & RO_COMPAT_HUGE_FILE != 0
        || desc_size == 64
    {
        ExtKind::Ext4
    } else if feature_compat & COMPAT_HAS_JOURNAL != 0 {
        ExtKind::Ext3
    } else {
        ExtKind::Ext2
    };

    // 日志设备本身不是文件系统,永远拒绝。
    if feature_incompat & INCOMPAT_JOURNAL_DEV != 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    // 连 Linux 也未实现的位(COMPRESSION/DIRDATA 是历史占位),
    // 以及已被 64BIT/FLEX_BG 取代的 META_BG:与 Linux 一样无法处理,拒绝。
    const UNSUPPORTED_INCOMPAT: u32 = INCOMPAT_COMPRESSION | INCOMPAT_META_BG | INCOMPAT_DIRDATA;
    if feature_incompat & UNSUPPORTED_INCOMPAT != 0 {
        return Err(BlockBackendError::Unsupported);
    }
    // 支持位白名单:任何超出的位一律拒绝。
    //
    // - RECOVER:挂载时回放日志(见 [`crate::journal`]);
    // - MMP:挂载时做 CLEAN 序列检查(见 state 模块);
    // - ENCRYPT:可挂载,访问加密 inode 时再报错(与无密钥 Linux 一致);
    // - CASEFOLD:可挂载,带 CASEFOLD 标志目录按 ASCII 大小写不敏感匹配
    //   (完整 Unicode casefold 未实现);
    // - LARGEDIR:读路径为线性扫描,天然兼容。
    const SUPPORTED_INCOMPAT: u32 = INCOMPAT_FILETYPE
        | INCOMPAT_RECOVER
        | INCOMPAT_EXTENTS
        | INCOMPAT_64BIT
        | INCOMPAT_MMP
        | INCOMPAT_FLEX_BG
        | INCOMPAT_EA_INODE
        | INCOMPAT_CSUM_SEED
        | INCOMPAT_LARGEDIR
        | INCOMPAT_INLINE_DATA
        | INCOMPAT_ENCRYPT
        | INCOMPAT_CASEFOLD;
    if feature_incompat & !SUPPORTED_INCOMPAT != 0 {
        return Err(BlockBackendError::Unsupported);
    }

    // ro_compat 门禁(对齐 Linux 语义):
    // - 已知且只读/读写都安全:SPARSE_SUPER/LARGE_FILE/HUGE_FILE/GDT_CSUM/
    //   DIR_NLINK/EXTRA_ISIZE/QUOTA/METADATA_CSUM/PROJECT/VERITY/ORPHAN_PRESENT;
    // - 只允许只读挂载的语义位:READONLY/BIGALLOC(簇分配写路径不支持)/
    //   HAS_SNAPSHOT(快照由外部维护)/SHARED_BLOCKS(共享块不可写);
    // - 未知位:Linux 拒绝读写挂载,我们退化为只读。
    const KNOWN_RO_COMPAT: u32 = RO_COMPAT_SPARSE_SUPER
        | RO_COMPAT_LARGE_FILE
        | RO_COMPAT_BTREE_DIR
        | RO_COMPAT_HUGE_FILE
        | RO_COMPAT_GDT_CSUM
        | RO_COMPAT_DIR_NLINK
        | RO_COMPAT_EXTRA_ISIZE
        | RO_COMPAT_HAS_SNAPSHOT
        | RO_COMPAT_QUOTA
        | RO_COMPAT_BIGALLOC
        | RO_COMPAT_METADATA_CSUM
        | RO_COMPAT_READONLY
        | RO_COMPAT_PROJECT
        | RO_COMPAT_SHARED_BLOCKS
        | RO_COMPAT_VERITY
        | RO_COMPAT_ORPHAN_PRESENT;
    const FORCE_RO_COMPAT: u32 =
        RO_COMPAT_READONLY | RO_COMPAT_BIGALLOC | RO_COMPAT_HAS_SNAPSHOT | RO_COMPAT_SHARED_BLOCKS;
    let force_read_only =
        feature_ro_compat & FORCE_RO_COMPAT != 0 || feature_ro_compat & !KNOWN_RO_COMPAT != 0;

    let metadata_csum = feature_ro_compat & RO_COMPAT_METADATA_CSUM != 0;
    let csum_seed = if feature_incompat & INCOMPAT_CSUM_SEED != 0 {
        le32(sb, sb_off::CHECKSUM_SEED)
    } else if metadata_csum {
        crc::crc32c(&uuid)
    } else {
        0
    };

    // 若启用 METADATA_CSUM,校验超级块自身:字段 0x3fc..0x400 是 s_checksum
    if metadata_csum {
        let expect = le32(sb, SUPERBLOCK_CHECKSUM_OFFSET);
        let actual = crc::crc32c(&sb[..SUPERBLOCK_CHECKSUM_OFFSET]);
        if actual != expect {
            return Err(BlockBackendError::OutOfRange);
        }
    }

    // blocks_count:64bit 时高 32 位在 s_blocks_count_hi
    let blocks_count = if feature_incompat & INCOMPAT_64BIT != 0 {
        ((blocks_count_hi as u64) << 32) | blocks_count_lo as u64
    } else {
        blocks_count_lo as u64
    };

    if blocks_per_group == 0 || inodes_per_group == 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    let groups_count = (((blocks_count - first_data_block as u64) + blocks_per_group as u64 - 1)
        / blocks_per_group as u64) as u32;

    // reserved_blocks / free_blocks / free_inodes
    let reserved_blocks_lo = le32(sb, 8);
    let free_blocks_lo = le32(sb, 12);
    let (reserved_blocks_hi, free_blocks_hi) = if feature_incompat & INCOMPAT_64BIT != 0 {
        (le32(sb, 0x154), le32(sb, 0x158))
    } else {
        (0, 0)
    };
    let reserved_blocks_count = ((reserved_blocks_hi as u64) << 32) | reserved_blocks_lo as u64;
    let free_blocks_count = ((free_blocks_hi as u64) << 32) | free_blocks_lo as u64;
    let free_inodes_count = le32(sb, 16);
    let orphan_file_inum = le32(sb, sb_off::ORPHAN_FILE_INUM);
    let journal_inum = le32(sb, sb_off::JOURNAL_INUM);
    let journal_dev = le32(sb, sb_off::JOURNAL_DEV);
    let last_orphan = le32(sb, sb_off::LAST_ORPHAN);
    let encoding = le16(sb, sb_off::ENCODING);
    let mmp_block = le64(sb, sb_off::MMP_BLOCK);
    let mmp_update_interval = le16(sb, sb_off::MMP_UPDATE_INTERVAL);
    let state = le16(sb, sb_off::STATE);

    Ok(Superblock {
        kind,
        inodes_count,
        blocks_count,
        first_data_block,
        block_size,
        log_cluster_size: le32(sb, 0x1c),
        blocks_per_group,
        inodes_per_group,
        inode_size,
        desc_size,
        first_ino,
        s_magic: magic,
        state,
        feature_compat,
        feature_incompat,
        feature_ro_compat,
        uuid,
        volume_name,
        metadata_csum,
        csum_seed,
        free_blocks_count,
        reserved_blocks_count,
        free_inodes_count,
        orphan_file_inum,
        journal_inum,
        journal_dev,
        last_orphan,
        encoding,
        mmp_block,
        mmp_update_interval,
        force_read_only,
        groups_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u16(raw: &mut [u8], offset: usize, value: u16) {
        raw[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(raw: &mut [u8], offset: usize, value: u32) {
        raw[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn parse_64bit_space_counters_uses_distinct_high_fields() {
        let mut raw = [0u8; SUPERBLOCK_SIZE];
        put_u32(&mut raw, 0, 1024);
        put_u32(&mut raw, 4, 11);
        put_u32(&mut raw, 8, 22);
        put_u32(&mut raw, 12, 33);
        put_u32(&mut raw, 32, u32::MAX);
        put_u32(&mut raw, 40, 1024);
        put_u16(&mut raw, 56, SUPERBLOCK_MAGIC);
        put_u32(&mut raw, 76, 1);
        put_u16(&mut raw, 88, 128);
        put_u32(&mut raw, 96, INCOMPAT_64BIT);
        put_u16(&mut raw, 254, 64);
        put_u32(&mut raw, 0x150, 5);
        put_u32(&mut raw, 0x154, 2);
        put_u32(&mut raw, 0x158, 3);

        let parsed = parse(&raw).expect("解析 64 位 ext 超级块");
        assert_eq!(parsed.blocks_count, (5u64 << 32) | 11);
        assert_eq!(parsed.reserved_blocks_count, (2u64 << 32) | 22);
        assert_eq!(parsed.free_blocks_count, (3u64 << 32) | 33);
    }
}
