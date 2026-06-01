//! clone(2) 标志位测试。
//!
//! 验证 CloneFlags 的位检测、退出信号提取以及 fork/vfork 默认标志。
//! 标志位数值与 Linux UAPI 严格对齐。

extern crate std;

use ktest::ktest;
use crate::clone_flags::CloneFlags;

/// CLONE_THREAD 位被正确检测。
#[ktest]
fn has_thread_flag() {
    let f = CloneFlags::from_raw(CloneFlags::CLONE_THREAD);
    assert!(f.has(CloneFlags::CLONE_THREAD));
    assert!(!f.has(CloneFlags::CLONE_VM));
}

/// CLONE_VM 位被正确检测。
#[ktest]
fn has_vm_flag() {
    let f = CloneFlags::from_raw(CloneFlags::CLONE_VM);
    assert!(f.has(CloneFlags::CLONE_VM));
}

/// fork 默认标志仅包含 SIGCHLD (17)，不含 CLONE_VM/CLONE_THREAD。
#[ktest]
fn fork_default_flags() {
    let f = CloneFlags::fork_default();
    assert_eq!(f.exit_signal(), 17);
    assert!(!f.has(CloneFlags::CLONE_VM));
    assert!(!f.has(CloneFlags::CLONE_THREAD));
}

/// vfork 默认标志包含 CLONE_VFORK | CLONE_VM | SIGCHLD。
#[ktest]
fn vfork_default_flags() {
    let f = CloneFlags::vfork_default();
    assert!(f.has(CloneFlags::CLONE_VFORK));
    assert!(f.has(CloneFlags::CLONE_VM));
    assert_eq!(f.exit_signal(), 17);
}

/// exit_signal 从低 8 位（CSIGNAL 掩码）提取退出信号编号。
#[ktest]
fn exit_signal_extract() {
    let f = CloneFlags::from_raw(0x0000_0009);
    assert_eq!(f.exit_signal(), 9);
}
