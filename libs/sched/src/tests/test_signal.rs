//! POSIX 信号集与信号号码测试。
//!
//! 验证 SigSet 的位操作（has/with/without/union/intersection）与
//! SignalNumber 的合法性检查和位转换。位 0 保留，信号 N 使用 bit N。

extern crate std;

use ktest::ktest;
use crate::signal::{SigSet, SignalNumber};

/// 空集不包含任何信号。
#[ktest]
fn sigset_empty() {
    assert!(!SigSet::EMPTY.has(SignalNumber::SIGKILL));
    assert!(!SigSet::EMPTY.has(SignalNumber::SIGTERM));
}

/// with 添加信号后 has 返回 true。
#[ktest]
fn sigset_with_has() {
    let s = SigSet::EMPTY.with(SignalNumber::SIGKILL);
    assert!(s.has(SignalNumber::SIGKILL));
    assert!(!s.has(SignalNumber::SIGTERM));
}

/// without 移除信号后 has 返回 false。
#[ktest]
fn sigset_without() {
    let s = SigSet::EMPTY
        .with(SignalNumber::SIGKILL)
        .without(SignalNumber::SIGKILL);
    assert!(!s.has(SignalNumber::SIGKILL));
}

/// union 合并两个集合，保留双方的信号。
#[ktest]
fn sigset_union() {
    let a = SigSet::EMPTY.with(SignalNumber::SIGKILL);
    let b = SigSet::EMPTY.with(SignalNumber::SIGTERM);
    let u = a.union(b);
    assert!(u.has(SignalNumber::SIGKILL));
    assert!(u.has(SignalNumber::SIGTERM));
}

/// intersection 仅保留双方共有的信号。
#[ktest]
fn sigset_intersection() {
    let a = SigSet::EMPTY.with(SignalNumber::SIGKILL).with(SignalNumber::SIGTERM);
    let b = SigSet::EMPTY.with(SignalNumber::SIGKILL);
    let i = a.intersection(b);
    assert!(i.has(SignalNumber::SIGKILL));
    assert!(!i.has(SignalNumber::SIGTERM));
}

/// from_raw 构造后 raw() 往返一致。
#[ktest]
fn sigset_raw_roundtrip() {
    let s = SigSet::from_raw(0xdead_beef);
    assert_eq!(s.raw(), 0xdead_beef);
}

/// 信号编号 1..=64 为合法，构造成功。
#[ktest]
fn signal_number_from_raw_valid() {
    assert!(SignalNumber::from_raw(1).is_some());
    assert!(SignalNumber::from_raw(31).is_some());
    assert!(SignalNumber::from_raw(64).is_some());
}

/// 0、65、-1 均为非法信号编号，返回 None。
#[ktest]
fn signal_number_from_raw_invalid() {
    assert!(SignalNumber::from_raw(0).is_none());
    assert!(SignalNumber::from_raw(65).is_none());
    assert!(SignalNumber::from_raw(-1).is_none());
}

/// 信号 N 的位掩码为 1 << N（位 0 保留不用）。
#[ktest]
fn signal_number_bit() {
    assert_eq!(SignalNumber::SIGKILL.bit(), 1 << 9);
    assert_eq!(SignalNumber::SIGHUP.bit(), 1 << 1);
}

/// as_usize 返回信号编号的数值。
#[ktest]
fn signal_number_as_usize() {
    assert_eq!(SignalNumber::SIGKILL.as_usize(), 9);
}
