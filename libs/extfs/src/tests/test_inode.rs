//! ext inode 辅助函数测试。

extern crate std;

use crate::inode;
use ktest::ktest;
use std::vec;

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
