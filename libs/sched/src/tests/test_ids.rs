//! 用户凭据与 capability 测试。
//!
//! 验证 CapSet 位操作、Uid 特权检测、Credentials 的 root/unprivileged 构造
//! 与能力检查。Capability 枚举数值与 Linux UAPI 对齐。

extern crate std;

use ktest::ktest;
use crate::ids::{CapSet, Capability, Credentials, Gid, Uid};

/// single 构造的集合包含该能力。
#[ktest]
fn caps_has_true() {
    assert!(CapSet::single(Capability::Kill).has(Capability::Kill));
}

/// 空集不包含任何能力。
#[ktest]
fn caps_has_false() {
    assert!(!CapSet::EMPTY.has(Capability::Kill));
}

/// with 添加后再 without 移除，has 返回 false。
#[ktest]
fn caps_with_without() {
    let c = CapSet::EMPTY
        .with(Capability::Kill)
        .without(Capability::Kill);
    assert!(!c.has(Capability::Kill));
}

/// FULL 集包含任意能力。
#[ktest]
fn caps_full_has_all() {
    assert!(CapSet::FULL.has(Capability::Kill));
    assert!(CapSet::FULL.has(Capability::SysNice));
    assert!(CapSet::FULL.has(Capability::Chown));
}

/// is_empty 正确区分空集、单能力和全集。
#[ktest]
fn caps_is_empty() {
    assert!(CapSet::EMPTY.is_empty());
    assert!(!CapSet::FULL.is_empty());
    assert!(!CapSet::single(Capability::Kill).is_empty());
}

/// Uid(0) 为 root，is_root() 返回 true。
#[ktest]
fn uid_root_is_root() {
    assert!(Uid::ROOT.is_root());
}

/// 非零 uid 的 is_root() 返回 false。
#[ktest]
fn uid_non_root() {
    assert!(!Uid(1000).is_root());
}

/// root 凭据拥有全能力。
#[ktest]
fn creds_root_has_cap() {
    let creds = Credentials::root();
    assert!(creds.has_cap(Capability::Kill));
}

/// unprivileged 构造的凭据无 Kill 能力。
#[ktest]
fn creds_unprivileged_no_cap() {
    let creds = Credentials::unprivileged(Uid(1000), Gid(1000));
    assert!(!creds.has_cap(Capability::Kill));
}
