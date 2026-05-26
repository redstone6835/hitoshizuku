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
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u32,
    pub desc_size: u32, // 块组描述符大小:32 或 64
    pub first_ino: u32,
    pub s_magic: u16,

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
    pub free_inodes_count: u32,

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

    // 拒绝:未回放的日志要求先 clean umount / fsck
    if feature_incompat & INCOMPAT_RECOVER != 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    // 日志设备本身不是文件系统
    if feature_incompat & INCOMPAT_JOURNAL_DEV != 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    // 我们不支持的 incompat 位
    const UNSUPPORTED_INCOMPAT: u32 = INCOMPAT_COMPRESSION
        | INCOMPAT_META_BG
        | INCOMPAT_MMP
        | INCOMPAT_ENCRYPT
        | INCOMPAT_CASEFOLD
        | INCOMPAT_DIRDATA
        | INCOMPAT_LARGEDIR;
    if feature_incompat & UNSUPPORTED_INCOMPAT != 0 {
        return Err(BlockBackendError::Unsupported);
    }
    // 支持位白名单:任何超出的位一律拒绝
    const SUPPORTED_INCOMPAT: u32 = INCOMPAT_FILETYPE
        | INCOMPAT_EXTENTS
        | INCOMPAT_64BIT
        | INCOMPAT_FLEX_BG
        | INCOMPAT_EA_INODE
        | INCOMPAT_INLINE_DATA
        | INCOMPAT_CSUM_SEED;
    if feature_incompat & !SUPPORTED_INCOMPAT != 0 && feature_incompat & UNSUPPORTED_INCOMPAT == 0 {
        // 存在我们既不支持也不显式拒绝的位,保守拒绝
        return Err(BlockBackendError::Unsupported);
    }

    let metadata_csum = feature_ro_compat & RO_COMPAT_METADATA_CSUM != 0;
    let csum_seed = if feature_incompat & INCOMPAT_CSUM_SEED != 0 {
        le32(sb, 0x270)
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

    // free_blocks / free_inodes
    let free_blocks_lo = le32(sb, 12);
    let free_blocks_hi = if feature_incompat & INCOMPAT_64BIT != 0 {
        le32(sb, 0x154)
    } else {
        0
    };
    let free_blocks_count = ((free_blocks_hi as u64) << 32) | free_blocks_lo as u64;
    let free_inodes_count = le32(sb, 16);

    Ok(Superblock {
        kind,
        inodes_count,
        blocks_count,
        first_data_block,
        block_size,
        blocks_per_group,
        inodes_per_group,
        inode_size,
        desc_size,
        first_ino,
        s_magic: magic,
        feature_compat,
        feature_incompat,
        feature_ro_compat,
        uuid,
        volume_name,
        metadata_csum,
        csum_seed,
        free_blocks_count,
        free_inodes_count,
        groups_count,
    })
}
