//! VFS 凭据与 DAC 权限检查测试。
//!
//! 验证 CapSet 位操作、Credentials::root/unprivileged 构造、
//! 以及 can_read/can_write/can_exec/is_owner 的三级匹配
//! （owner→group→other）与能力（DacOverride/DacReadSearch/FOwner）绕过逻辑。

extern crate std;

use crate::cred::{CapSet, Capability, Credentials, Gid, Uid};
use crate::stat::FileMode;
use ktest::ktest;
use std::vec;

/// 构造指定 uid/gid 和能力集的凭据，用于 DAC 测试。
fn cred_with_caps(uid: u32, gid: u32, caps: CapSet) -> Credentials {
    Credentials {
        uid: Uid(uid),
        euid: Uid(uid),
        suid: Uid(uid),
        gid: Gid(gid),
        egid: Gid(gid),
        sgid: Gid(gid),
        groups: vec![],
        caps,
    }
}

// ── CapSet ─────────────────────────────────────────────────────────

/// single 构造的集合包含该能力。
#[ktest]
fn caps_single_has() {
    assert!(CapSet::single(Capability::DacOverride).has(Capability::DacOverride));
}

/// 空集不包含任何能力。
#[ktest]
fn caps_empty_has_none() {
    assert!(!CapSet::EMPTY.has(Capability::DacOverride));
}

/// FULL 集包含任意能力。
#[ktest]
fn caps_full_has_all() {
    assert!(CapSet::FULL.has(Capability::DacOverride));
    assert!(CapSet::FULL.has(Capability::SysAdmin));
}

/// with 添加后再 without 移除，has 返回 false。
#[ktest]
fn caps_with_without() {
    let c = CapSet::EMPTY
        .with(Capability::DacOverride)
        .without(Capability::DacOverride);
    assert!(!c.has(Capability::DacOverride));
}

/// merge 合并两个集合，同时拥有双方的能力。
#[ktest]
fn caps_merge() {
    let a = CapSet::single(Capability::DacOverride);
    let b = CapSet::single(Capability::SysAdmin);
    let m = a.merge(b);
    assert!(m.has(Capability::DacOverride));
    assert!(m.has(Capability::SysAdmin));
}

// ── Credentials 构造 ───────────────────────────────────────────────

/// root 凭据拥有多种核心能力，通过 CapSet::FULL 体现而非 euid==0 特判。
#[ktest]
fn cred_root_has_multiple_caps() {
    let c = Credentials::root();
    assert!(c.has_cap(Capability::DacOverride));
    assert!(c.has_cap(Capability::DacReadSearch));
    assert!(c.has_cap(Capability::FOwner));
    assert!(c.has_cap(Capability::SysAdmin));
}

/// unprivileged 构造的凭据无任何能力。
#[ktest]
fn cred_unprivileged_no_caps() {
    let c = Credentials::unprivileged(Uid(1000), Gid(1000));
    assert!(!c.has_cap(Capability::DacOverride));
}

// ── DAC can_read ───────────────────────────────────────────────────

/// 文件 owner 匹配 euid 且有 IRUSR 时可读。
#[ktest]
fn can_read_owner_match() {
    let c = cred_with_caps(1000, 1000, CapSet::EMPTY);
    assert!(c.can_read(Uid(1000), Gid(0), FileMode::new(0o400)));
}

/// 文件 owner 不匹配 euid 且无能力时不可读。
#[ktest]
fn can_read_owner_mismatch() {
    let c = cred_with_caps(2000, 2000, CapSet::EMPTY);
    assert!(!c.can_read(Uid(1000), Gid(0), FileMode::new(0o400)));
}

/// 持有 DacOverride 能力者始终可读，无视权限位。
#[ktest]
fn can_read_dac_override() {
    let c = cred_with_caps(2000, 2000, CapSet::single(Capability::DacOverride));
    assert!(c.can_read(Uid(1000), Gid(0), FileMode::new(0)));
}

/// 持有 DacReadSearch 能力者始终可读。
#[ktest]
fn can_read_dac_read_search() {
    let c = cred_with_caps(2000, 2000, CapSet::single(Capability::DacReadSearch));
    assert!(c.can_read(Uid(1000), Gid(0), FileMode::new(0)));
}

/// egid 不匹配但文件 gid 在附加组中时可读。
#[ktest]
fn can_read_supplementary_group() {
    let c = Credentials {
        uid: Uid(2000),
        euid: Uid(2000),
        suid: Uid(2000),
        gid: Gid(2000),
        egid: Gid(2000),
        sgid: Gid(2000),
        groups: vec![Gid(3000)],
        caps: CapSet::EMPTY,
    };
    assert!(c.can_read(Uid(1000), Gid(3000), FileMode::new(0o040)));
}

// ── DAC can_write ──────────────────────────────────────────────────

/// 文件 owner 匹配且有 IWUSR 时可写。
#[ktest]
fn can_write_owner() {
    let c = cred_with_caps(1000, 1000, CapSet::EMPTY);
    assert!(c.can_write(Uid(1000), Gid(0), FileMode::new(0o200)));
}

/// 持有 DacOverride 能力时可写，无视权限位。
#[ktest]
fn can_write_dac_override() {
    let c = cred_with_caps(2000, 2000, CapSet::single(Capability::DacOverride));
    assert!(c.can_write(Uid(1000), Gid(0), FileMode::new(0)));
}

/// DacReadSearch 能力不绕过写权限检查。
#[ktest]
fn can_write_no_dac_read_search() {
    let c = cred_with_caps(2000, 2000, CapSet::single(Capability::DacReadSearch));
    assert!(!c.can_write(Uid(1000), Gid(0), FileMode::new(0o200)));
}

// ── DAC can_exec ───────────────────────────────────────────────────

/// owner 匹配且有 IXUSR 时可执行。
#[ktest]
fn can_exec_owner() {
    let c = cred_with_caps(1000, 1000, CapSet::EMPTY);
    assert!(c.can_exec(Uid(1000), Gid(0), FileMode::new(0o100), false));
}

/// egid 匹配文件 gid 且有 IXGRP 时可执行。
#[ktest]
fn can_exec_group() {
    let c = cred_with_caps(2000, 1000, CapSet::EMPTY);
    assert!(c.can_exec(Uid(1000), Gid(1000), FileMode::new(0o010), false));
}

/// 非 owner 非 group 时检查 IXOTH 权限位。
#[ktest]
fn can_exec_other() {
    let c = cred_with_caps(2000, 2000, CapSet::EMPTY);
    assert!(c.can_exec(Uid(1000), Gid(1000), FileMode::new(0o001), false));
}

/// DacOverride 对目录执行始终放行，即使没有任何执行位。
#[ktest]
fn can_exec_dac_override_dir_without_exec_bit() {
    let c = cred_with_caps(2000, 2000, CapSet::single(Capability::DacOverride));
    assert!(c.can_exec(Uid(1000), Gid(0), FileMode::new(0), true));
}

/// DacOverride 对普通文件执行要求文件至少有一个执行位。
#[ktest]
fn can_exec_dac_override_file_requires_any_exec() {
    let c = cred_with_caps(2000, 2000, CapSet::single(Capability::DacOverride));
    assert!(!c.can_exec(Uid(1000), Gid(0), FileMode::new(0), false));
    assert!(c.can_exec(Uid(1000), Gid(0), FileMode::new(0o001), false));
}

// ── is_owner ───────────────────────────────────────────────────────

/// euid == file_uid 时 is_owner 返回 true。
#[ktest]
fn is_owner_true() {
    let c = cred_with_caps(1000, 1000, CapSet::EMPTY);
    assert!(c.is_owner(Uid(1000)));
}

/// euid != file_uid 且无 FOwner 时 is_owner 返回 false。
#[ktest]
fn is_owner_false() {
    let c = cred_with_caps(2000, 2000, CapSet::EMPTY);
    assert!(!c.is_owner(Uid(1000)));
}

/// 持有 FOwner 能力时 is_owner 始终返回 true。
#[ktest]
fn is_owner_fowner_cap() {
    let c = cred_with_caps(2000, 2000, CapSet::single(Capability::FOwner));
    assert!(c.is_owner(Uid(1000)));
}
