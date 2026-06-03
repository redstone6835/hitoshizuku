//! VFS 运行时限制配置测试。
//!
//! 验证 VfsLimits 的 Linux 兼容默认值与自定义构造。

extern crate std;

use crate::limits::VfsLimits;
use ktest::ktest;

/// 默认符号链接深度限制为 40（Linux 默认值）。
#[ktest]
fn default_symlink_max_depth() {
    assert_eq!(VfsLimits::default().symlink_max_depth, 40);
}

/// 默认路径最大长度为 4096（Linux PATH_MAX）。
#[ktest]
fn default_path_max() {
    assert_eq!(VfsLimits::default().path_max, 4096);
}

/// 默认文件描述符软硬限制分别为 1024 和 4096。
#[ktest]
fn default_nofile_limits() {
    let l = VfsLimits::default();
    assert_eq!(l.nofile_default, 1024);
    assert_eq!(l.nofile_max, 4096);
}

/// VfsLimits::new 正确设置各字段。
#[ktest]
fn custom_limits() {
    let l = VfsLimits::new(8, 256, 64, 128);
    assert_eq!(l.symlink_max_depth, 8);
    assert_eq!(l.path_max, 256);
    assert_eq!(l.nofile_default, 64);
    assert_eq!(l.nofile_max, 128);
}
