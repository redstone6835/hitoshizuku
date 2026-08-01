//! 内存中的 ext4 镜像构造器(测试基础设施)。
//!
//! 手工布局一个 4K 块、单块组、metadata_csum 的 ext4 镜像:
//!
//! ```text
//! 块 0      : boot 区 + 超级块(@1024)
//! 块 1      : 块组描述符(64 字节,单组)
//! 块 2      : 块位图
//! 块 3      : inode 位图
//! 块 4..11  : inode 表(128 inode × 256B)
//! 块 12     : 根目录数据块
//! 块 16..47 : 日志 inode 数据(32 块,逻辑 0..31)
//! 块 48..   : 测试文件数据区
//! ```

extern crate std;

use std::vec;
use std::vec::Vec;

use crate::crc;

pub const BS: usize = 4096;
pub const BLOCKS_COUNT: u32 = 1024;
pub const INODES_COUNT: u32 = 128;
pub const INODES_PER_GROUP: u32 = 128;
pub const INODE_SIZE: usize = 256;

pub const GDT_BLOCK: u32 = 1;
pub const BLOCK_BITMAP: u32 = 2;
pub const INODE_BITMAP: u32 = 3;
pub const INODE_TABLE: u32 = 4;
pub const ROOT_DIR_BLOCK: u32 = 12;
pub const JOURNAL_BLOCK: u32 = 16;
pub const JOURNAL_BLOCKS: u32 = 32;
pub const FILE_DATA_BLOCK: u32 = 48;

pub const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
pub const EXT4_INDEX_FL: u32 = 0x0000_1000;
pub const EXT4_CASEFOLD_FL: u32 = 0x4000_0000;
pub const EXT4_ENCRYPT_FL: u32 = 0x0000_0800;
pub const EXT4_VERITY_FL: u32 = 0x0010_0000;

pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFLNK: u16 = 0xa000;

pub const COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub const COMPAT_ORPHAN_FILE: u32 = 0x1000;
pub const COMPAT_FAST_COMMIT: u32 = 0x0400;
pub const INCOMPAT_FILETYPE: u32 = 0x0002;
pub const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const RO_COMPAT_HUGE_FILE: u32 = 0x0008;
const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
const RO_COMPAT_DIR_NLINK: u32 = 0x0020;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
pub const RO_COMPAT_ORPHAN_PRESENT: u32 = 0x10000;

const EXT2_STATE_VALID_FS: u16 = 0x0001;

pub const JBD2_MAGIC: u32 = 0xc03b_3998;
pub const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
pub const JBD2_COMMIT_BLOCK: u32 = 2;
pub const JBD2_SUPERBLOCK_V2: u32 = 4;
pub const JBD2_REVOKE_BLOCK: u32 = 5;
pub const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0001;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x0010;
pub const JBD2_FEATURE_INCOMPAT_FAST_COMMIT: u32 = 0x0020;

pub const JBD2_FLAG_ESCAPE: u16 = 1;
pub const JBD2_FLAG_LAST_TAG: u16 = 8;

#[inline]
pub fn le16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn le32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn le64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn be16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
}
#[inline]
pub fn be32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}
#[inline]
pub fn be64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}
#[inline]
pub fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
#[inline]
pub fn rdb32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// 构造 extent 根(60 字节 i_block)。
pub fn extent_root(entries: &[(u32, u32, u64)]) -> [u8; 60] {
    let mut ib = [0u8; 60];
    le16(&mut ib, 0, 0xf30a);
    le16(&mut ib, 2, entries.len() as u16);
    le16(&mut ib, 4, 4);
    le16(&mut ib, 6, 0);
    for (i, &(ee_block, ee_len, ee_start)) in entries.iter().enumerate() {
        let off = 12 + i * 12;
        le32(&mut ib, off, ee_block);
        le16(&mut ib, off + 4, ee_len as u16);
        le16(&mut ib, off + 6, (ee_start >> 32) as u16);
        le32(&mut ib, off + 8, ee_start as u32);
    }
    ib
}

/// 计算并写入 inode checksum(0x7c lo,可选 0x82 hi)。
pub fn inode_csum(csum_seed: u32, ino: u32, generation: u32, raw: &mut [u8; INODE_SIZE]) {
    raw[0x7c] = 0;
    raw[0x7d] = 0;
    let extra_isize = u16::from_le_bytes([raw[0x80], raw[0x81]]) as usize;
    if extra_isize >= 4 {
        raw[0x82] = 0;
        raw[0x83] = 0;
    }
    let mut seed = csum_seed;
    seed = crc::update(seed, &ino.to_le_bytes());
    seed = crc::update(seed, &generation.to_le_bytes());
    let sum = crc::update(seed, raw);
    raw[0x7c..0x7e].copy_from_slice(&(sum as u16).to_le_bytes());
    if extra_isize >= 4 {
        raw[0x82..0x84].copy_from_slice(&((sum >> 16) as u16).to_le_bytes());
    }
}

/// 计算并写入目录块尾 checksum(metadata_csum 目录)。
pub fn dir_tail_csum(csum_seed: u32, ino: u32, generation: u32, block: &mut [u8]) {
    let off = block.len() - 12;
    let mut seed = csum_seed;
    seed = crc::update(seed, &ino.to_le_bytes());
    seed = crc::update(seed, &generation.to_le_bytes());
    let sum = crc::update(seed, &block[..off]);
    le32(block, off, 0);
    le16(block, off + 4, 12);
    block[off + 6] = 0;
    block[off + 7] = 0xde;
    le32(block, off + 8, sum);
}

/// 位图 checksum(bgd 里的 `bg_*_bitmap_csum`,只覆盖有效位图字节)。
pub fn bitmap_csum(csum_seed: u32, bitmap: &[u8]) -> u32 {
    crc::update(csum_seed, bitmap)
}

/// 计算并写入 64 字节块组描述符的 bg_checksum。
pub fn gdt_csum(csum_seed: u32, group: u32, desc: &mut [u8; 64]) {
    desc[0x1e] = 0;
    desc[0x1f] = 0;
    let mut seed = csum_seed;
    seed = crc::update(seed, &group.to_le_bytes());
    let sum = (crc::update(seed, desc) & 0xffff) as u16;
    desc[0x1e..0x20].copy_from_slice(&sum.to_le_bytes());
}

/// 内存镜像。
pub struct ExtImg {
    pub data: Vec<u8>,
    pub uuid: [u8; 16],
    pub csum_seed: u32,
}

impl ExtImg {
    /// 只携带 uuid 的轻量实例(复用 journal 构造器,用于真实镜像测试)。
    pub fn for_uuid(uuid: [u8; 16]) -> Self {
        Self {
            data: Vec::new(),
            uuid,
            csum_seed: crc::crc32c(&uuid),
        }
    }
}

impl ExtImg {
    /// 构造一个干净、带空日志的 ext4 镜像(默认特性:has_journal +
    /// filetype + extents + 64bit + metadata_csum 等)。
    pub fn new() -> Self {
        let mut img = Self {
            data: vec![0u8; BLOCKS_COUNT as usize * BS],
            uuid: [0x5a; 16],
            csum_seed: 0,
        };
        img.csum_seed = crc::crc32c(&img.uuid);
        img.write_superblock(0);
        img.write_gdt();
        img.init_bitmaps();
        img.write_root_dir();
        img.write_journal_inode();
        img.write_jsb(
            1,
            0,
            JBD2_FEATURE_INCOMPAT_REVOKE | JBD2_FEATURE_INCOMPAT_CSUM_V3,
        );
        img
    }

    pub fn block_mut(&mut self, blk: u32) -> &mut [u8] {
        let start = blk as usize * BS;
        &mut self.data[start..start + BS]
    }
    pub fn block(&self, blk: u32) -> &[u8] {
        let start = blk as usize * BS;
        &self.data[start..start + BS]
    }

    fn sb_mut(&mut self) -> &mut [u8] {
        &mut self.data[1024..2048]
    }

    fn write_superblock(&mut self, feature_incompat_extra: u32) {
        let _ = self.csum_seed;
        let uuid = self.uuid;
        let sb = self.sb_mut();
        sb.fill(0);
        le32(sb, 0x00, INODES_COUNT); // s_inodes_count
        le32(sb, 0x04, BLOCKS_COUNT); // s_blocks_count_lo
        le32(sb, 0x08, 0); // s_r_blocks_count_lo
        le32(sb, 0x0c, BLOCKS_COUNT - 100); // s_free_blocks_count_lo
        le32(sb, 0x10, INODES_COUNT - 16); // s_free_inodes_count
        le32(sb, 0x14, 0); // s_first_data_block
        le32(sb, 0x18, 2); // s_log_block_size (4K)
        le32(sb, 0x1c, 0); // s_log_cluster_size
        le32(sb, 0x20, 8 * BS as u32); // s_blocks_per_group
        le32(sb, 0x24, 8 * BS as u32); // s_clusters_per_group
        le32(sb, 0x28, INODES_PER_GROUP); // s_inodes_per_group
        le16(sb, 0x34, 0); // s_mnt_count
        le16(sb, 0x36, 30); // s_max_mnt_count
        le16(sb, 0x38, 0xef53); // s_magic
        le16(sb, 0x3a, EXT2_STATE_VALID_FS); // s_state
        le16(sb, 0x3c, 1); // s_errors
        le32(sb, 0x40, 1_700_000_000); // s_lastcheck
        le32(sb, 0x4c, 1); // s_rev_level
        le32(sb, 0x54, 11); // s_first_ino
        le16(sb, 0x58, INODE_SIZE as u16); // s_inode_size
        le32(sb, 0x5c, COMPAT_HAS_JOURNAL); // s_feature_compat
        le32(
            sb,
            0x60,
            INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | feature_incompat_extra,
        );
        le32(
            sb,
            0x64,
            RO_COMPAT_SPARSE_SUPER
                | RO_COMPAT_LARGE_FILE
                | RO_COMPAT_HUGE_FILE
                | RO_COMPAT_GDT_CSUM
                | RO_COMPAT_DIR_NLINK
                | RO_COMPAT_EXTRA_ISIZE
                | RO_COMPAT_METADATA_CSUM,
        );
        sb[0x68..0x78].copy_from_slice(&uuid);
        le32(sb, 0xe0, 8); // s_journal_inum
        le32(sb, 0xe8, 0); // s_last_orphan
        le16(sb, 0xfe, 64); // s_desc_size
        let sum = crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    /// 切换 INCOMPAT_RECOVER 并重算超级块校验。
    pub fn set_recover(&mut self, on: bool) {
        let _ = self.csum_seed;
        let sb = self.sb_mut();
        let mut incompat = rd32(sb, 0x60);
        if on {
            incompat |= INCOMPAT_RECOVER;
        } else {
            incompat &= !INCOMPAT_RECOVER;
        }
        le32(sb, 0x60, incompat);
        let sum = crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    /// 追加 compat 特性位(如 FAST_COMMIT / ORPHAN_FILE)。
    pub fn add_compat(&mut self, bits: u32) {
        let _ = self.csum_seed;
        let sb = self.sb_mut();
        let compat = rd32(sb, 0x5c) | bits;
        le32(sb, 0x5c, compat);
        let sum = crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    /// 追加 ro_compat 特性位(如 ORPHAN_PRESENT)。
    pub fn add_ro_compat(&mut self, bits: u32) {
        let _ = self.csum_seed;
        let sb = self.sb_mut();
        let ro = rd32(sb, 0x64) | bits;
        le32(sb, 0x64, ro);
        let sum = crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    /// 追加 incompat 特性位。
    pub fn add_incompat(&mut self, bits: u32) {
        let _ = self.csum_seed;
        let sb = self.sb_mut();
        let v = rd32(sb, 0x60) | bits;
        le32(sb, 0x60, v);
        let sum = crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    pub fn set_last_orphan(&mut self, ino: u32) {
        let _ = self.csum_seed;
        let sb = self.sb_mut();
        le32(sb, 0xe8, ino);
        let sum = crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    pub fn set_orphan_file_inum(&mut self, ino: u32) {
        let _ = self.csum_seed;
        let sb = self.sb_mut();
        le32(sb, 0x280, ino);
        let sum = crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    pub fn state_flags(&self) -> u16 {
        u16::from_le_bytes([self.data[1024 + 0x3a], self.data[1024 + 0x3b]])
    }
    pub fn incompat_features(&self) -> u32 {
        rd32(&self.data[1024..2048], 0x60)
    }

    fn write_gdt(&mut self) {
        let _ = self.csum_seed;
        let mut desc = [0u8; 64];
        le32(&mut desc, 0, BLOCK_BITMAP);
        le32(&mut desc, 4, INODE_BITMAP);
        le32(&mut desc, 8, INODE_TABLE);
        le16(&mut desc, 12, (BLOCKS_COUNT - 100) as u16); // free blocks
        le16(&mut desc, 14, (INODES_COUNT - 16) as u16); // free inodes
        le16(&mut desc, 16, 2); // used dirs
        le16(&mut desc, 18, 0); // flags
        let mut seed = self.csum_seed;
        seed = crc::update(seed, &0u32.to_le_bytes());
        let mut tmp = [0u8; 64];
        tmp.copy_from_slice(&desc);
        tmp[0x1e] = 0;
        tmp[0x1f] = 0;
        let sum = (crc::update(seed, &tmp) & 0xffff) as u16;
        le16(&mut desc, 0x1e, sum);
        self.block_mut(GDT_BLOCK)[..64].copy_from_slice(&desc);
    }

    fn init_bitmaps(&mut self) {
        // 块位图:0..16 已用(元数据),其余空闲。
        for blk in 0..JOURNAL_BLOCK {
            self.set_block_used(blk, true);
        }
        // inode 位图:1..=10 保留 + 2(根) + 8(日志)。
        for ino in 1..=10u32 {
            self.set_inode_used(ino, true);
        }
    }

    pub fn set_block_used(&mut self, blk: u32, used: bool) {
        let byte = (blk / 8) as usize;
        let mask = 1u8 << (blk % 8);
        let bm = self.block_mut(BLOCK_BITMAP);
        if used {
            bm[byte] |= mask;
        } else {
            bm[byte] &= !mask;
        }
    }

    pub fn set_inode_used(&mut self, ino: u32, used: bool) {
        let bit = ino - 1;
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        let bm = self.block_mut(INODE_BITMAP);
        if used {
            bm[byte] |= mask;
        } else {
            bm[byte] &= !mask;
        }
    }

    pub fn block_used(&self, blk: u32) -> bool {
        self.block(BLOCK_BITMAP)[(blk / 8) as usize] & (1u8 << (blk % 8)) != 0
    }
    pub fn inode_used(&self, ino: u32) -> bool {
        let bit = ino - 1;
        self.block(INODE_BITMAP)[(bit / 8) as usize] & (1u8 << (bit % 8)) != 0
    }

    /// 写一个 inode(自动处理 METADATA_CSUM)。
    pub fn write_inode(&mut self, ino: u32, raw: &[u8; INODE_SIZE]) {
        let generation = rd32(raw, 100);
        let mut tmp = *raw;
        // csum 域清零后重算
        tmp[0x7c] = 0;
        tmp[0x7d] = 0;
        let extra_isize = u16::from_le_bytes([tmp[0x80], tmp[0x81]]) as usize;
        if extra_isize >= 4 {
            tmp[0x82] = 0;
            tmp[0x83] = 0;
        }
        let mut seed = self.csum_seed;
        seed = crc::update(seed, &ino.to_le_bytes());
        seed = crc::update(seed, &generation.to_le_bytes());
        let sum = crc::update(seed, &tmp);
        tmp[0x7c..0x7e].copy_from_slice(&(sum as u16).to_le_bytes());
        if extra_isize >= 4 {
            tmp[0x82..0x84].copy_from_slice(&((sum >> 16) as u16).to_le_bytes());
        }
        let idx = (ino - 1) as usize;
        let table_off = INODE_TABLE as usize * BS + idx * INODE_SIZE;
        self.data[table_off..table_off + INODE_SIZE].copy_from_slice(&tmp);
    }

    /// 构造一个 inode 内存镜像(调用方自行填字段后 write_inode)。
    pub fn make_inode(
        mode: u16,
        nlink: u16,
        size: u64,
        i_block: &[u8; 60],
        flags: u32,
    ) -> [u8; INODE_SIZE] {
        let mut raw = [0u8; INODE_SIZE];
        le16(&mut raw, 0, mode);
        le32(&mut raw, 4, size as u32);
        le32(&mut raw, 108, (size >> 32) as u32);
        le16(&mut raw, 26, nlink);
        le32(&mut raw, 32, flags);
        raw[0x28..0x28 + 60].copy_from_slice(i_block);
        le16(&mut raw, 0x80, 32); // i_extra_isize
        // atime/ctime/mtime 给固定值
        le32(&mut raw, 8, 1_700_000_000);
        le32(&mut raw, 12, 1_700_000_000);
        le32(&mut raw, 16, 1_700_000_000);
        raw
    }

    fn write_root_dir(&mut self) {
        let mut ib = [0u8; 60];
        le32(&mut ib, 0, ROOT_DIR_BLOCK);
        let raw = Self::make_inode(S_IFDIR | 0o755, 2, BS as u64, &ib, 0);
        le32(&mut { raw }, 28, 0); // blocks_lo 稍后覆盖
        let mut raw = raw;
        le32(&mut raw, 28, 8); // i_blocks = 4096/512
        self.write_inode(2, &raw);

        // 根目录块:. / ..
        let has_tail = true;
        let mut blk = vec![0u8; BS];
        le32(&mut blk, 0, 2);
        le16(&mut blk, 4, 12);
        blk[6] = 1;
        blk[7] = 2;
        blk[8] = b'.';
        let rec2 = BS as u16 - 12 - if has_tail { 12 } else { 0 };
        le32(&mut blk, 12, 2);
        le16(&mut blk, 16, rec2);
        blk[18] = 2;
        blk[19] = 2;
        blk[20] = b'.';
        blk[21] = b'.';
        self.block_mut(ROOT_DIR_BLOCK).copy_from_slice(&blk);
        self.write_dir_tail(ROOT_DIR_BLOCK, 2, 0);
    }

    /// 给目录块写 ext4_dir_entry_tail(metadata_csum)。
    pub fn write_dir_tail(&mut self, blk: u32, ino: u32, generation: u32) {
        let off = BS - 12;
        let mut seed = self.csum_seed;
        seed = crc::update(seed, &ino.to_le_bytes());
        seed = crc::update(seed, &generation.to_le_bytes());
        let block = self.block(blk).to_vec();
        let sum = crc::update(seed, &block[..off]);
        let block = self.block_mut(blk);
        le32(block, off, 0);
        le16(block, off + 4, 12);
        block[off + 6] = 0;
        block[off + 7] = 0xde;
        le32(block, off + 8, sum);
    }

    /// 在根目录追加一条 entry(就地分裂 "..")。
    pub fn add_root_entry(&mut self, ino: u32, file_type: u8, name: &[u8]) {
        let needed = ((8 + name.len() + 3) & !3) as u16;
        let (dotdot_off, rec2) = {
            let block = self.block(ROOT_DIR_BLOCK);
            // 找 ".."(第二个 entry)
            let rec1 = u16::from_le_bytes([block[4], block[5]]) as usize;
            let rec2 = u16::from_le_bytes([block[12 + 4], block[12 + 5]]) as usize;
            (12, (rec1, rec2))
        };
        let _ = dotdot_off;
        let block = self.block_mut(ROOT_DIR_BLOCK);
        let prev_rec = rec2.1;
        // ".." 缩到 12,新 entry 占剩余
        le16(block, 12 + 4, 12);
        let new_off = 12 + 12;
        let new_rec = (prev_rec - 12) as u16;
        le32(block, new_off, ino);
        le16(block, new_off + 4, new_rec);
        block[new_off + 6] = name.len() as u8;
        block[new_off + 7] = file_type;
        block[new_off + 8..new_off + 8 + name.len()].copy_from_slice(name);
        let _ = needed;
        self.write_dir_tail(ROOT_DIR_BLOCK, 2, 0);
    }

    fn write_journal_inode(&mut self) {
        let ib = extent_root(&[(0, JOURNAL_BLOCKS as u16 as u32, JOURNAL_BLOCK as u64)]);
        let mut raw = Self::make_inode(
            S_IFREG | 0o600,
            1,
            (JOURNAL_BLOCKS as usize * BS) as u64,
            &ib,
            EXT4_EXTENTS_FL,
        );
        le32(&mut raw, 28, (JOURNAL_BLOCKS * 8) as u32);
        self.write_inode(8, &raw);
    }

    /// 日志逻辑块 → 物理块(extent: 0..32 → 16..47)。
    pub fn journal_phys(journal_block: u32) -> u32 {
        JOURNAL_BLOCK + journal_block
    }

    /// 写日志超级块(日志逻辑块 0)。
    pub fn write_jsb(&mut self, sequence: u32, start: u32, features: u32) {
        let mut jsb = vec![0u8; BS];
        be32(&mut jsb, 0, JBD2_MAGIC);
        be32(&mut jsb, 4, JBD2_SUPERBLOCK_V2);
        be32(&mut jsb, 8, 0);
        be32(&mut jsb, 0x0c, BS as u32);
        be32(&mut jsb, 0x10, JOURNAL_BLOCKS);
        be32(&mut jsb, 0x14, 1); // s_first
        be32(&mut jsb, 0x18, sequence);
        be32(&mut jsb, 0x1c, start);
        be32(&mut jsb, 0x24, 0); // compat
        be32(&mut jsb, 0x28, features);
        be32(&mut jsb, 0x2c, 0); // ro_compat
        jsb[0x30..0x40].copy_from_slice(&self.uuid);
        be32(&mut jsb, 0x40, 1); // nr_users
        be32(&mut jsb, 0x48, 1024); // max_transaction
        be32(&mut jsb, 0x4c, 0); // max_trans_data
        jsb[0x50] = 4; // checksum type = crc32c
        be32(&mut jsb, 0x54, 0); // num_fc_blks(0 → 默认 256,我们镜像小,用不到)
        // v3 校验:crc32c(前 1024 字节,校验域清零)
        let mut tmp = jsb[..1024].to_vec();
        tmp[0xfc..0x100].fill(0);
        let sum = crc::crc32c(&tmp);
        be32(&mut jsb, 0xfc, sum);
        self.block_mut(Self::journal_phys(0)).copy_from_slice(&jsb);
    }

    /// 设置 jsb 中的 s_num_fc_blks。
    pub fn set_jsb_num_fc(&mut self, num_fc: u32) {
        let mut jsb = self.block(Self::journal_phys(0)).to_vec();
        be32(&mut jsb, 0x54, num_fc);
        let mut tmp = jsb[..1024].to_vec();
        tmp[0xfc..0x100].fill(0);
        let sum = crc::crc32c(&tmp);
        be32(&mut jsb, 0xfc, sum);
        self.block_mut(Self::journal_phys(0)).copy_from_slice(&jsb);
    }

    /// 设置 jsb 的 start/sequence(重算 csum)。
    pub fn set_jsb_start(&mut self, sequence: u32, start: u32) {
        let mut jsb = self.block(Self::journal_phys(0)).to_vec();
        be32(&mut jsb, 0x18, sequence);
        be32(&mut jsb, 0x1c, start);
        let mut tmp = jsb[..1024].to_vec();
        tmp[0xfc..0x100].fill(0);
        let sum = crc::crc32c(&tmp);
        be32(&mut jsb, 0xfc, sum);
        self.block_mut(Self::journal_phys(0)).copy_from_slice(&jsb);
    }

    pub fn jsb_start(&self) -> u32 {
        rdb32(self.block(Self::journal_phys(0)), 0x1c)
    }
    pub fn jsb_sequence(&self) -> u32 {
        rdb32(self.block(Self::journal_phys(0)), 0x18)
    }

    fn journal_csum_seed(&self) -> u32 {
        crc::crc32c(&self.uuid)
    }

    /// v3 描述符块:`tags` 为 (目标块号, flags, 数据块内容)。
    pub fn journal_descriptor(&self, seq: u32, tags: &[(u64, u16, Vec<u8>)]) -> Vec<u8> {
        let seed = self.journal_csum_seed();
        let mut blk = vec![0u8; BS];
        be32(&mut blk, 0, JBD2_MAGIC);
        be32(&mut blk, 4, JBD2_DESCRIPTOR_BLOCK);
        be32(&mut blk, 8, seq);
        let mut off = 12usize;
        let last = tags.len() - 1;
        for (i, (blocknr, flags, data)) in tags.iter().enumerate() {
            let mut fl = *flags;
            if i == last {
                fl |= JBD2_FLAG_LAST_TAG;
            }
            be32(&mut blk, off, *blocknr as u32); // t_blocknr
            be32(&mut blk, off + 4, fl as u32); // t_flags (tag3 为 32 位)
            be32(&mut blk, off + 8, (blocknr >> 32) as u32); // t_blocknr_high
            let mut csum = crc::update(seed, &seq.to_be_bytes());
            csum = crc::update(csum, data);
            be32(&mut blk, off + 12, csum); // t_checksum
            off += 16;
            // 非 SAME_UUID:跟 16 字节 uuid
            blk[off..off + 16].copy_from_slice(&self.uuid);
            off += 16;
        }
        let sum = crc::update(seed, &{
            let mut t = blk.clone();
            t[BS - 4..].fill(0);
            t
        });
        be32(&mut blk, BS - 4, sum);
        blk
    }

    /// v3 revoke 块(record_len=4,日志不带 64BIT)。
    pub fn journal_revoke(&self, seq: u32, blocks: &[u64]) -> Vec<u8> {
        let seed = self.journal_csum_seed();
        let mut blk = vec![0u8; BS];
        be32(&mut blk, 0, JBD2_MAGIC);
        be32(&mut blk, 4, JBD2_REVOKE_BLOCK);
        be32(&mut blk, 8, seq);
        let r_count = 16 + 4 * blocks.len() as u32;
        be32(&mut blk, 12, r_count);
        let mut off = 16usize;
        for b in blocks {
            be32(&mut blk, off, *b as u32);
            off += 4;
        }
        let sum = crc::update(seed, &{
            let mut t = blk.clone();
            t[BS - 4..].fill(0);
            t
        });
        be32(&mut blk, BS - 4, sum);
        blk
    }

    /// v3 提交块。`corrupt = true` 时故意写错校验和(模拟撕裂提交)。
    pub fn journal_commit(&self, seq: u32, sec: u64, corrupt: bool) -> Vec<u8> {
        let seed = self.journal_csum_seed();
        let mut blk = vec![0u8; BS];
        be32(&mut blk, 0, JBD2_MAGIC);
        be32(&mut blk, 4, JBD2_COMMIT_BLOCK);
        be32(&mut blk, 8, seq);
        blk[12] = 4; // h_chksum_type = crc32c
        blk[13] = 4; // h_chksum_size
        be64(&mut blk, 48, sec);
        let sum = crc::update(seed, &{
            let mut t = blk.clone();
            t[16..20].fill(0);
            t
        });
        be32(&mut blk, 16, if corrupt { sum ^ 0xdead_beef } else { sum });
        blk
    }

    /// 把一组 (日志逻辑块号, 内容) 写进镜像。
    pub fn inject_journal(&mut self, blocks: &[(u32, Vec<u8>)]) {
        for (lb, content) in blocks {
            self.block_mut(Self::journal_phys(*lb))
                .copy_from_slice(content);
        }
    }

    // ── fast commit 构造 ────────────────────────────────────────────────

    /// FC HEAD tag。
    pub fn fc_head(tid: u32) -> Vec<u8> {
        let mut v = vec![0u8; 12];
        le16(&mut v, 0, 9); // EXT4_FC_TAG_HEAD
        le16(&mut v, 2, 8);
        le32(&mut v, 4, 0); // features
        le32(&mut v, 8, tid);
        v
    }
    /// FC TAIL tag(crc 由调用方算)。
    pub fn fc_tail(tid: u32, crc: u32) -> Vec<u8> {
        let mut v = vec![0u8; 12];
        le16(&mut v, 0, 8); // EXT4_FC_TAG_TAIL
        le16(&mut v, 2, 8);
        le32(&mut v, 4, tid);
        le32(&mut v, 8, crc);
        v
    }
    /// FC INODE tag(完整 inode 镜像)。
    pub fn fc_inode(ino: u32, raw: &[u8; INODE_SIZE]) -> Vec<u8> {
        let mut v = vec![0u8; 4 + 4 + INODE_SIZE];
        le16(&mut v, 0, 6); // EXT4_FC_TAG_INODE
        le16(&mut v, 2, (4 + INODE_SIZE) as u16);
        le32(&mut v, 4, ino);
        v[8..8 + INODE_SIZE].copy_from_slice(raw);
        v
    }
    /// FC CREAT/LINK/UNLINK tag。
    pub fn fc_dentry(tag: u16, parent: u32, ino: u32, name: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 4 + 8 + name.len()];
        le16(&mut v, 0, tag);
        le16(&mut v, 2, (8 + name.len()) as u16);
        le32(&mut v, 4, parent);
        le32(&mut v, 8, ino);
        v[12..12 + name.len()].copy_from_slice(name);
        v
    }
    /// FC ADD_RANGE tag。
    pub fn fc_add_range(ino: u32, lblk: u32, len: u16, pblk: u64) -> Vec<u8> {
        let mut v = vec![0u8; 4 + 16];
        le16(&mut v, 0, 1); // EXT4_FC_TAG_ADD_RANGE
        le16(&mut v, 2, 16);
        le32(&mut v, 4, ino);
        le32(&mut v, 8, lblk);
        le16(&mut v, 12, len);
        le16(&mut v, 14, (pblk >> 32) as u16);
        le32(&mut v, 16, pblk as u32);
        v
    }
    /// FC DEL_RANGE tag。
    pub fn fc_del_range(ino: u32, lblk: u32, len: u32) -> Vec<u8> {
        let mut v = vec![0u8; 4 + 12];
        le16(&mut v, 0, 2); // EXT4_FC_TAG_DEL_RANGE
        le16(&mut v, 2, 12);
        le32(&mut v, 4, ino);
        le32(&mut v, 8, lblk);
        le32(&mut v, 12, len);
        v
    }

    /// 把一串 FC tag(不含 TAIL)打包进一个 fc 块并自动追加合法 TAIL。
    /// 返回块内容。`tid` 同时用于 HEAD 与 TAIL。
    pub fn fc_block(&self, tid: u32, tags: &[Vec<u8>]) -> Vec<u8> {
        let mut blk = vec![0u8; BS];
        let head = Self::fc_head(tid);
        let mut crc = crc::update(0, &head);
        blk[..head.len()].copy_from_slice(&head);
        let mut off = head.len();
        for tag in tags {
            crc = crc::update(crc, tag);
            blk[off..off + tag.len()].copy_from_slice(tag);
            off += tag.len();
        }
        // TAIL 的 crc 覆盖 (tl + fc_tid) = 8 字节
        let mut tail_partial = vec![0u8; 8];
        le16(&mut tail_partial, 0, 8);
        le16(&mut tail_partial, 2, 8);
        le32(&mut tail_partial, 4, tid);
        crc = crc::update(crc, &tail_partial);
        let tail = Self::fc_tail(tid, crc);
        blk[off..off + tail.len()].copy_from_slice(&tail);
        blk
    }
}
