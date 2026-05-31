//! 文件描述符与 FdFlags 测试。
//!
//! 验证标准流编号、Fd raw 转换、FdFlags CLOEXEC 位操作。

extern crate std;

use ktest::ktest;
use crate::fdtable::{Fd, FdFlags};

/// 标准流 STDIN=0, STDOUT=1, STDERR=2 与 POSIX 一致。
#[ktest]
fn fd_stdin_stdout_stderr() {
    assert_eq!(Fd::STDIN.as_raw(), 0);
    assert_eq!(Fd::STDOUT.as_raw(), 1);
    assert_eq!(Fd::STDERR.as_raw(), 2);
}

/// from_raw 构造后 as_raw 往返一致。
#[ktest]
fn fd_from_raw_as_raw_roundtrip() {
    for n in [0, 1, 2, 42, 1023] {
        assert_eq!(Fd::from_raw(n).as_raw(), n);
    }
}

/// 默认 FdFlags 的 raw 值为 0。
#[ktest]
fn fdflags_default_raw_zero() {
    assert_eq!(FdFlags::default().raw(), 0);
}

/// CLOEXEC 标志使 has(CLOEXEC) 返回 true。
#[ktest]
fn fdflags_cloexec_has() {
    assert!(FdFlags::CLOEXEC.has(FdFlags::CLOEXEC));
}

/// with 添加后 without 移除，has 返回 false。
#[ktest]
fn fdflags_with_without() {
    let f = FdFlags::default()
        .with(FdFlags::CLOEXEC)
        .without(FdFlags::CLOEXEC);
    assert!(!f.has(FdFlags::CLOEXEC));
}
