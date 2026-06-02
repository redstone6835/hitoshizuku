//! 文件类型模式位与 FileMode 权限位操作测试。
//!
//! 验证 7 种 FileType 的 st_mode 高位编码与 FileMode 的 Unix 权限位操作。

extern crate std;

use crate::stat::{FileMode, FileType};
use ktest::ktest;

/// Regular 文件的模式位为 0o100000 (S_IFREG)。
#[ktest]
fn file_type_regular() {
    assert_eq!(FileType::Regular.to_mode_bits(), 0o100000);
}

/// Directory 的模式位为 0o040000 (S_IFDIR)。
#[ktest]
fn file_type_directory() {
    assert_eq!(FileType::Directory.to_mode_bits(), 0o040000);
}

/// Symlink 的模式位为 0o120000 (S_IFLNK)。
#[ktest]
fn file_type_symlink() {
    assert_eq!(FileType::Symlink.to_mode_bits(), 0o120000);
}

/// CharDevice 的模式位为 0o020000 (S_IFCHR)。
#[ktest]
fn file_type_char_device() {
    assert_eq!(FileType::CharDevice.to_mode_bits(), 0o020000);
}

/// BlockDevice 的模式位为 0o060000 (S_IFBLK)。
#[ktest]
fn file_type_block_device() {
    assert_eq!(FileType::BlockDevice.to_mode_bits(), 0o060000);
}

/// Fifo 的模式位为 0o010000 (S_IFIFO)。
#[ktest]
fn file_type_fifo() {
    assert_eq!(FileType::Fifo.to_mode_bits(), 0o010000);
}

/// Socket 的模式位为 0o140000 (S_IFSOCK)。
#[ktest]
fn file_type_socket() {
    assert_eq!(FileType::Socket.to_mode_bits(), 0o140000);
}

/// 各权限常量位值与 POSIX 对齐。
#[ktest]
fn file_mode_constants() {
    assert_eq!(FileMode::IRUSR.bits(), 0o400);
    assert_eq!(FileMode::IWUSR.bits(), 0o200);
    assert_eq!(FileMode::IXUSR.bits(), 0o100);
    assert_eq!(FileMode::ISUID.bits(), 0o4000);
}

/// PERM_MASK 覆盖低 9 位权限（rwxrwxrwx）。
#[ktest]
fn file_mode_perm_mask() {
    assert_eq!(FileMode::PERM_MASK.bits(), 0o777);
}

/// has 正确检测权限位是否置位。
#[ktest]
fn file_mode_has() {
    let m = FileMode::new(0o644);
    assert!(m.has(FileMode::IRUSR));
    assert!(m.has(FileMode::IRGRP));
    assert!(!m.has(FileMode::IXUSR));
}

/// with 添加权限后 has 返回 true，without 移除后 has 返回 false。
#[ktest]
fn file_mode_with_without() {
    let m = FileMode::new(0).with(FileMode::IRUSR).with(FileMode::IWUSR);
    assert!(m.has(FileMode::IRUSR));
    let m = m.without(FileMode::IRUSR);
    assert!(!m.has(FileMode::IRUSR));
    assert!(m.has(FileMode::IWUSR));
}

/// mask 按位与保留对应权限，用于 umask 运算。
#[ktest]
fn file_mode_mask() {
    let m = FileMode::new(0o7644).mask(FileMode::PERM_MASK);
    assert_eq!(m.bits(), 0o644);
}

/// is_empty 对空权限返回 true。
#[ktest]
fn file_mode_is_empty() {
    assert!(FileMode::new(0).is_empty());
    assert!(!FileMode::new(0o644).is_empty());
}

/// owner_read/write/exec 分别检测所有者的 r/w/x 位。
#[ktest]
fn file_mode_owner_predicates() {
    let m = FileMode::new(0o700);
    assert!(m.owner_read());
    assert!(m.owner_write());
    assert!(m.owner_exec());
}

/// group_read/write/exec 分别检测所属组的 r/w/x 位。
#[ktest]
fn file_mode_group_predicates() {
    let m = FileMode::new(0o070);
    assert!(m.group_read());
    assert!(m.group_write());
    assert!(m.group_exec());
}
