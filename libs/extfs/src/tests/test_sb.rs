//! ext superblock 解析测试。

extern crate std;

use crate::layout::{ExtKind, INCOMPAT_EXTENTS};
use crate::sb;
use ktest::ktest;
use ktest_mock::MemDisk;
use std::sync::Arc;
use std::vec;
use std::vec::Vec;

fn make_sb_bytes(
    magic: u16,
    s_log_block_size: u32,
    inode_size: u32,
    feature_incompat: u32,
) -> Vec<u8> {
    let mut raw = vec![0u8; 1024];
    raw[0..4].copy_from_slice(&100u32.to_le_bytes()); // s_inodes_count
    raw[4..8].copy_from_slice(&2048u32.to_le_bytes()); // s_blocks_count_lo
    raw[24..28].copy_from_slice(&s_log_block_size.to_le_bytes());
    raw[32..36].copy_from_slice(&8192u32.to_le_bytes()); // s_blocks_per_group
    raw[40..44].copy_from_slice(&2048u32.to_le_bytes()); // s_inodes_per_group
    raw[56..58].copy_from_slice(&magic.to_le_bytes());
    raw[76..80].copy_from_slice(&1u32.to_le_bytes()); // s_rev_level
    raw[88..90].copy_from_slice(&(inode_size as u16).to_le_bytes());
    raw[96..100].copy_from_slice(&feature_incompat.to_le_bytes());
    if inode_size >= 256 {
        raw[254..256].copy_from_slice(&64u16.to_le_bytes()); // s_desc_size
    }
    raw
}

fn make_sb_disk(
    magic: u16,
    s_log_block_size: u32,
    inode_size: u32,
    feature_incompat: u32,
) -> Arc<MemDisk> {
    let sb = make_sb_bytes(magic, s_log_block_size, inode_size, feature_incompat);
    let sb_offset = 1024;
    let mut data = vec![0u8; sb_offset];
    data.extend_from_slice(&sb);
    Arc::new(MemDisk::from_bytes(data, 512))
}

/// 构造 ext4 superblock（INCOMPAT_EXTENTS 置位），验证魔数、块大小、inode 大小、文件系统类型。
#[ktest]
fn load_ext4_superblock() {
    let disk = make_sb_disk(0xef53, 0, 256, INCOMPAT_EXTENTS);
    let sb = sb::load(&*disk).expect("load ext4 superblock");
    assert_eq!(sb.s_magic, 0xef53);
    assert_eq!(sb.block_size, 1024);
    assert_eq!(sb.inode_size, 256);
    assert!(matches!(sb.kind, ExtKind::Ext4));
}

/// 错误的魔数导致解析失败。
#[ktest]
fn reject_bad_magic() {
    let disk = make_sb_disk(0x0000, 0, 128, 0);
    assert!(sb::load(&*disk).is_err());
}

/// s_log_block_size=2 时块大小为 4096（1024 << 2）。
#[ktest]
fn block_size_4096() {
    let disk = make_sb_disk(0xef53, 2, 256, INCOMPAT_EXTENTS);
    let sb = sb::load(&*disk).expect("load ext4 4K block SB");
    assert_eq!(sb.block_size, 4096);
}
