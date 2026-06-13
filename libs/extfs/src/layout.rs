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
pub(crate) const RO_COMPAT_QUOTA: u32 = 0x0100;
pub(crate) const RO_COMPAT_BIGALLOC: u32 = 0x0200;
pub(crate) const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
pub(crate) const RO_COMPAT_PROJECT: u32 = 0x2000;
pub(crate) const RO_COMPAT_VERITY: u32 = 0x8000;
pub(crate) const RO_COMPAT_ORPHAN_PRESENT: u32 = 0x10000;

/// Inode flags (i_flags)
pub(crate) const EXT4_EXTENTS_FL: u32 = 0x00080000;
pub(crate) const EXT4_INLINE_DATA_FL: u32 = 0x10000000;
pub(crate) const EXT4_ENCRYPT_FL: u32 = 0x00000800;

/// extent header magic
pub(crate) const EXT4_EXT_MAGIC: u16 = 0xf30a;

/// "fast symlink" 最大内联长度(在 i_block 里直接放文本)。
pub(crate) const FAST_SYMLINK_MAX: usize = 60;

/// 不同 ext 变体对同一套代码的需求提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtKind {
    Ext2,
    Ext3,
    Ext4,
}
