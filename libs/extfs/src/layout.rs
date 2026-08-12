//! ext2/3/4 磁盘布局常量:超级块/inode/目录项字段偏移、feature 位、
//! inode 模式位、extent tree magic,等等。
//!
//! 所有数字与 `include/linux/ext4_fs.h` / `e2fsprogs` 保持一致;本文件只放
//! **只读** 使用到的常量,写路径不会用到的(比如 `s_log_groups_per_flex` 等)
//! 视情况忽略。

#![allow(dead_code)]

/// 超级块起始字节(不随 block_size 变化)。
pub(crate) const SUPERBLOCK_OFFSET: u64 = 1024;
pub(crate) const SUPERBLOCK_SIZE: usize = 1024;
pub(crate) const SUPERBLOCK_CHECKSUM_OFFSET: usize = 0x3fc;
pub(crate) const SUPERBLOCK_MAGIC: u16 = 0xef53;

/// 根目录的 inode 号。
pub(crate) const EXT4_ROOT_INO: u32 = 2;

/// 目录项 file type(dirent::file_type 字段)。
pub(crate) const DT_UNKNOWN: u8 = 0;
pub(crate) const DT_REG: u8 = 1;
pub(crate) const DT_DIR: u8 = 2;
pub(crate) const DT_CHR: u8 = 3;
pub(crate) const DT_BLK: u8 = 4;
pub(crate) const DT_FIFO: u8 = 5;
pub(crate) const DT_SOCK: u8 = 6;
pub(crate) const DT_LNK: u8 = 7;

/// Mode 字段高位(i_mode 顶端 4 位决定文件类型)。
pub(crate) const S_IFMT: u16 = 0xf000;
pub(crate) const S_IFSOCK: u16 = 0xc000;
pub(crate) const S_IFLNK: u16 = 0xa000;
pub(crate) const S_IFREG: u16 = 0x8000;
pub(crate) const S_IFBLK: u16 = 0x6000;
pub(crate) const S_IFDIR: u16 = 0x4000;
pub(crate) const S_IFCHR: u16 = 0x2000;
pub(crate) const S_IFIFO: u16 = 0x1000;

/// feature_compat (s_feature_compat)
pub(crate) const COMPAT_DIR_PREALLOC: u32 = 0x0001;
pub(crate) const COMPAT_IMAGIC_INODES: u32 = 0x0002;
pub(crate) const COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub(crate) const COMPAT_EXT_ATTR: u32 = 0x0008;
pub(crate) const COMPAT_RESIZE_INODE: u32 = 0x0010;
pub(crate) const COMPAT_DIR_INDEX: u32 = 0x0020;
pub(crate) const COMPAT_SPARSE_SUPER2: u32 = 0x0200;
pub(crate) const COMPAT_FAST_COMMIT: u32 = 0x0400;
pub(crate) const COMPAT_ORPHAN_FILE: u32 = 0x1000;

/// feature_incompat (s_feature_incompat) —— 未知位必须拒绝挂载
pub(crate) const INCOMPAT_COMPRESSION: u32 = 0x0001;
pub(crate) const INCOMPAT_FILETYPE: u32 = 0x0002;
pub(crate) const INCOMPAT_RECOVER: u32 = 0x0004;
pub(crate) const INCOMPAT_JOURNAL_DEV: u32 = 0x0008;
pub(crate) const INCOMPAT_META_BG: u32 = 0x0010;
pub(crate) const INCOMPAT_EXTENTS: u32 = 0x0040;
pub(crate) const INCOMPAT_64BIT: u32 = 0x0080;
pub(crate) const INCOMPAT_MMP: u32 = 0x0100;
pub(crate) const INCOMPAT_FLEX_BG: u32 = 0x0200;
pub(crate) const INCOMPAT_EA_INODE: u32 = 0x0400;
pub(crate) const INCOMPAT_DIRDATA: u32 = 0x1000;
pub(crate) const INCOMPAT_CSUM_SEED: u32 = 0x2000;
pub(crate) const INCOMPAT_LARGEDIR: u32 = 0x4000;
pub(crate) const INCOMPAT_INLINE_DATA: u32 = 0x8000;
pub(crate) const INCOMPAT_ENCRYPT: u32 = 0x10000;
pub(crate) const INCOMPAT_CASEFOLD: u32 = 0x20000;

/// feature_ro_compat (s_feature_ro_compat) —— 只读挂载时可全部忽略,
/// 但其中 METADATA_CSUM 会启用读侧校验。
pub(crate) const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub(crate) const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
pub(crate) const RO_COMPAT_BTREE_DIR: u32 = 0x0004;
pub(crate) const RO_COMPAT_HUGE_FILE: u32 = 0x0008;
pub(crate) const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
pub(crate) const RO_COMPAT_DIR_NLINK: u32 = 0x0020;
pub(crate) const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
pub(crate) const RO_COMPAT_HAS_SNAPSHOT: u32 = 0x0080;
pub(crate) const RO_COMPAT_QUOTA: u32 = 0x0100;
pub(crate) const RO_COMPAT_BIGALLOC: u32 = 0x0200;
pub(crate) const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
pub(crate) const RO_COMPAT_READONLY: u32 = 0x1000;
pub(crate) const RO_COMPAT_PROJECT: u32 = 0x2000;
pub(crate) const RO_COMPAT_SHARED_BLOCKS: u32 = 0x4000;
pub(crate) const RO_COMPAT_VERITY: u32 = 0x8000;
pub(crate) const RO_COMPAT_ORPHAN_PRESENT: u32 = 0x10000;

/// Inode flags (i_flags)
pub(crate) const EXT4_INDEX_FL: u32 = 0x00001000;
pub(crate) const EXT4_HUGE_FILE_FL: u32 = 0x00040000;
pub(crate) const EXT4_EXTENTS_FL: u32 = 0x00080000;
pub(crate) const EXT4_VERITY_FL: u32 = 0x00100000;
pub(crate) const EXT4_INLINE_DATA_FL: u32 = 0x10000000;
pub(crate) const EXT4_CASEFOLD_FL: u32 = 0x40000000;
pub(crate) const EXT4_ENCRYPT_FL: u32 = 0x00000800;

/// 块组描述符标志（`bg_flags`）。
pub(crate) const EXT4_BG_INODE_UNINIT: u16 = 0x0001;
pub(crate) const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;
pub(crate) const EXT4_BG_INODE_ZEROED: u16 = 0x0004;

/// extent header magic
pub(crate) const EXT4_EXT_MAGIC: u16 = 0xf30a;

/// "fast symlink" 最大内联长度(在 i_block 里直接放文本)。
pub(crate) const FAST_SYMLINK_MAX: usize = 60;

/// 日志(journal) 专用 inode 号。
pub(crate) const EXT4_JOURNAL_INO: u32 = 8;

/// ext4 目录硬链接上限;超过后 DIR_NLINK 特性把 i_links_count 固定为 1。
pub(crate) const EXT4_LINK_MAX: u16 = 65000;

/// s_state:文件系统状态位。
pub(crate) const EXT2_STATE_VALID_FS: u16 = 0x0001;
pub(crate) const EXT2_STATE_ERROR_FS: u16 = 0x0002;

/// MMP(多挂载保护)块常量(`struct mmp_struct`)。
pub(crate) const EXT4_MMP_MAGIC: u32 = 0x004d_4d50;
pub(crate) const EXT4_MMP_SEQ_CLEAN: u32 = 0xff4d_4d50;
pub(crate) const EXT4_MMP_SEQ_FSCK: u32 = 0xe24d_4d50;
pub(crate) const EXT4_MMP_SEQ_MAX: u32 = 0xe24d_4d4f;

/// orphan file 每个数据块尾部的魔数(`ext4_orphan_block_tail.ob_magic`)。
pub(crate) const EXT4_ORPHAN_BLOCK_MAGIC: u32 = 0x0b10_ca04;

/// casefold 使用的文件名编码(s_encoding):UTF-8 12.1。
pub(crate) const EXT4_ENC_UTF8_12_1: u16 = 1;

/// 超级块内部分字段偏移(与 `struct ext4_super_block` 对齐,已经真实
/// mke2fs 镜像逐字段核对)。
pub(crate) mod sb_off {
    pub(crate) const MTIME: usize = 0x2c;
    pub(crate) const WTIME: usize = 0x30;
    pub(crate) const MNT_COUNT: usize = 0x34;
    pub(crate) const STATE: usize = 0x3a;
    pub(crate) const FEATURE_COMPAT: usize = 0x5c;
    pub(crate) const FEATURE_INCOMPAT: usize = 0x60;
    pub(crate) const FEATURE_RO_COMPAT: usize = 0x64;
    pub(crate) const JOURNAL_INUM: usize = 0xe0;
    pub(crate) const JOURNAL_DEV: usize = 0xe4;
    pub(crate) const LAST_ORPHAN: usize = 0xe8;
    pub(crate) const FIRST_META_BG: usize = 0x104;
    pub(crate) const MMP_UPDATE_INTERVAL: usize = 0x166;
    pub(crate) const MMP_BLOCK: usize = 0x168;
    pub(crate) const LPF_INO: usize = 0x268;
    pub(crate) const PRJ_QUOTA_INUM: usize = 0x26c;
    pub(crate) const CHECKSUM_SEED: usize = 0x270;
    pub(crate) const WTIME_HI: usize = 0x274;
    pub(crate) const MTIME_HI: usize = 0x275;
    pub(crate) const ENCODING: usize = 0x27c;
    pub(crate) const ENCODING_FLAGS: usize = 0x27e;
    pub(crate) const ORPHAN_FILE_INUM: usize = 0x280;
}

/// 不同 ext 变体对同一套代码的需求提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtKind {
    Ext2,
    Ext3,
    Ext4,
}
