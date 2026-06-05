//! VmFlags 位操作测试。
//!
//! 验证自定义 u32 位域的基本操作：构造、has/with/without、权限掩码、子集检查。

#[cfg(feature = "ktest-kernel")]
extern crate alloc;
#[cfg(not(feature = "ktest-kernel"))]
extern crate std;

use crate::VmFlags;
use ktest::ktest;

/// 位构造后 bits() 往返一致。
#[ktest]
fn from_bits_roundtrip() {
    for bits in [0, VmFlags::READ, VmFlags::READ | VmFlags::WRITE] {
        assert_eq!(VmFlags::from_bits(bits).bits(), bits);
    }
}

/// Default 为空标志集。
#[ktest]
fn empty_is_default() {
    let f = VmFlags::default();
    assert_eq!(f.bits(), 0);
}

/// has 正确检测 READ/WRITE/EXEC 各位。
#[ktest]
fn has_read_write_exec() {
    let f = VmFlags::from_bits(VmFlags::READ | VmFlags::WRITE | VmFlags::EXEC);
    assert!(f.has(VmFlags::READ));
    assert!(f.has(VmFlags::WRITE));
    assert!(f.has(VmFlags::EXEC));
    assert!(!f.has(VmFlags::USER));
}

/// with 向空集添加标志后 has 返回 true。
#[ktest]
fn with_adds_flag() {
    let f = VmFlags::EMPTY.with(VmFlags::WRITE);
    assert!(f.has(VmFlags::WRITE));
    assert!(!f.has(VmFlags::READ));
}

/// without 移除标志后 has 返回 false。
#[ktest]
fn without_removes_flag() {
    let f = VmFlags::from_bits(VmFlags::READ | VmFlags::WRITE);
    let g = f.without(VmFlags::WRITE);
    assert!(g.has(VmFlags::READ));
    assert!(!g.has(VmFlags::WRITE));
}

/// permissions() 仅保留低 3 位（RWX），过滤 USER/SHARED 等区域类型标志。
#[ktest]
fn permissions_masks_non_perm_bits() {
    let f = VmFlags::from_bits(VmFlags::READ | VmFlags::WRITE | VmFlags::USER);
    let perms = f.permissions();
    assert_eq!(perms.bits(), VmFlags::READ | VmFlags::WRITE);
}

/// contains_all 验证给定标志位是否全部置位。
#[ktest]
fn contains_all_subset() {
    let f = VmFlags::from_bits(VmFlags::READ | VmFlags::WRITE | VmFlags::EXEC);
    assert!(f.contains_all(VmFlags::READ | VmFlags::WRITE));
    assert!(!f.contains_all(VmFlags::READ | VmFlags::USER));
}

/// has(0) 始终返回 false，避免空标志误判。
#[ktest]
fn has_flag_zero_always_false() {
    let f = VmFlags::from_bits(VmFlags::READ);
    assert!(!f.has(0));
}
