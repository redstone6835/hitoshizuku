//! ext inode 辅助函数测试。

extern crate std;

use crate::inode;
use ktest::ktest;
use std::vec;
use vfs::stat::{DevId, FileType};

/// S_IFREG (0x8000) 映射为 FileType::Regular。
#[ktest]
fn file_type_from_mode_regular() {
    assert_eq!(
        inode::file_type_from_mode(0x8000),
        vfs::stat::FileType::Regular
    );
}

/// S_IFDIR (0x4000) 映射为 FileType::Directory。
#[ktest]
fn file_type_from_mode_directory() {
    assert_eq!(
        inode::file_type_from_mode(0x4000),
        vfs::stat::FileType::Directory
    );
}

/// S_IFLNK (0xA000) 映射为 FileType::Symlink。
#[ktest]
fn file_type_from_mode_symlink() {
    assert_eq!(
        inode::file_type_from_mode(0xa000),
        vfs::stat::FileType::Symlink
    );
}

/// i_block_slice 从 inode 128 字节结构的 0x28 偏移处提取 60 字节块指针数组。
#[ktest]
fn i_block_slice_offsets() {
    let raw = vec![0u8; 128];
    let block = inode::i_block_slice(&raw);
    assert_eq!(block.len(), 60);
    assert_eq!(block.as_ptr() as usize - raw.as_ptr() as usize, 0x28);
}

/// 特殊文件类型必须同时映射到正确的 ext inode 模式和目录项类型。
#[ktest]
fn special_file_layout_matches_ext_encoding() {
    assert_eq!(
        inode::special_file_layout(FileType::CharDevice),
        Some((0x2000, 3))
    );
    assert_eq!(
        inode::special_file_layout(FileType::BlockDevice),
        Some((0x6000, 4))
    );
    assert_eq!(
        inode::special_file_layout(FileType::Fifo),
        Some((0x1000, 5))
    );
    assert_eq!(
        inode::special_file_layout(FileType::Socket),
        Some((0xc000, 6))
    );
    assert_eq!(inode::special_file_layout(FileType::Regular), None);
}

/// 传统 8:8 设备号和扩展 12:20 设备号都必须可逆。
#[ktest]
fn special_device_encoding_roundtrips() {
    for dev in [DevId::new(8, 1), DevId::new(259, 0x4_5678)] {
        let (old, new) = inode::encode_special_device(dev).expect("encode device");
        assert_eq!(inode::decode_special_device(old, new), dev);
    }
    assert!(inode::encode_special_device(DevId::new(0x1000, 0)).is_err());
    assert!(inode::encode_special_device(DevId::new(0, 0x10_0000)).is_err());
}
