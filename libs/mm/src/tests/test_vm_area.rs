//! VmArea 几何运算测试。
//!
//! 验证单个虚拟内存区域的地址包含、区间重叠、以及 split_at 分裂操作。
//! 所有测试使用匿名映射构造，不依赖物理页或文件。

#[cfg(feature = "ktest-kernel")]
extern crate alloc;
#[cfg(not(feature = "ktest-kernel"))]
extern crate std;

use alloc::sync::Arc;

use crate::{SharedAnonObject, VmArea, VmBacking, VmFlags};
use ktest::ktest;

fn anon_area(start: usize, end: usize) -> VmArea {
    VmArea {
        range: start..end,
        flags: VmFlags::from_bits(VmFlags::READ | VmFlags::WRITE | VmFlags::USER),
        backing: VmBacking::anonymous(),
    }
}

/// contains 对 range 内地址（含起点、不含终点）返回 true。
#[ktest]
fn contains_addr_in_range() {
    let area = anon_area(0x1000, 0x2000);
    assert!(area.contains(0x1000));
    assert!(area.contains(0x1fff));
}

/// contains 对 range 外地址返回 false。
#[ktest]
fn contains_addr_out_of_range() {
    let area = anon_area(0x1000, 0x2000);
    assert!(!area.contains(0xfff));
    assert!(!area.contains(0x2000));
}

/// overlap 对存在交集的区间返回 true。
#[ktest]
fn overlap_when_overlapping() {
    let area = anon_area(0x1000, 0x3000);
    assert!(area.overlap(&(0x2000..0x4000)));
}

/// overlap 对相邻但不相交的区间返回 false。
#[ktest]
fn overlap_when_adjacent_no_overlap() {
    let area = anon_area(0x1000, 0x2000);
    assert!(!area.overlap(&(0x2000..0x3000)));
}

/// split_at 在中间点将 VMA 正确分裂为左右两段。
#[ktest]
fn split_at_midpoint() {
    let area = anon_area(0x1000, 0x3000);
    let (left, right) = area.split_at(0x2000).expect("valid split point");
    assert_eq!(left.range, 0x1000..0x2000);
    assert_eq!(right.range, 0x2000..0x3000);
}

/// split_at 在起点或终点处返回 None（不变式：start < split < end）。
#[ktest]
fn split_at_boundary_returns_none() {
    let area = anon_area(0x1000, 0x3000);
    assert!(area.split_at(0x1000).is_none());
    assert!(area.split_at(0x3000).is_none());
}

/// backing 的末地址溢出时必须在进入 VmaSet 前被拒绝。
#[ktest]
fn well_formed_rejects_backing_overflow() {
    let direct = VmArea {
        range: 0x1000..0x3000,
        flags: VmFlags::from_bits(VmFlags::READ | VmFlags::USER),
        backing: VmBacking::Direct(usize::MAX - 0x1000),
    };
    assert!(!direct.is_well_formed());

    let shared = VmArea {
        range: 0x1000..0x3000,
        flags: VmFlags::from_bits(VmFlags::READ | VmFlags::USER),
        backing: VmBacking::SharedAnon {
            object: Arc::new(SharedAnonObject::new()),
            offset: u64::MAX - 0x1000,
        },
    };
    assert!(!shared.is_well_formed());
}

/// backing 末地址恰好可表示时仍是合法 VMA。
#[ktest]
fn well_formed_accepts_backing_boundary() {
    let area = VmArea {
        range: 0x1000..0x3000,
        flags: VmFlags::from_bits(VmFlags::READ | VmFlags::USER),
        backing: VmBacking::Direct(usize::MAX - 0x2000),
    };
    assert!(area.is_well_formed());
}
