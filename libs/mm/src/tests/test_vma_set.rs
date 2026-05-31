//! VmaSet 集合操作测试。
//!
//! 验证按起址排序的 VMA 集合的插入、查找、空洞搜索、取消映射、范围检查与邻接合并。

extern crate std;

use errno::Errno;
use ktest::ktest;
use crate::{VmArea, VmBacking, VmFlags, VmaSet};

fn anon_area(start: usize, end: usize) -> VmArea {
    VmArea {
        range: start..end,
        flags: VmFlags::from_bits(VmFlags::READ | VmFlags::WRITE | VmFlags::USER),
        backing: VmBacking::Anon,
    }
}

/// 插入非重叠 VMA 成功，留 gap 避免 merge_neighbors 合并。
#[ktest]
fn insert_non_overlapping() {
    let mut vmas = VmaSet::new();
    assert!(vmas.insert(anon_area(0x1000, 0x2000)).is_ok());
    assert!(vmas.insert(anon_area(0x3000, 0x4000)).is_ok());
    assert_eq!(vmas.len(), 2);
}

/// 插入与已有 VMA 重叠的区域返回 EEXIST。
#[ktest]
fn insert_overlapping_returns_eexist() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x3000)).unwrap();
    assert_eq!(vmas.insert(anon_area(0x2000, 0x4000)), Err(Errno::EEXIST));
}

/// 插入空 range（start >= end）返回 EINVAL。
#[ktest]
fn insert_empty_range_returns_einval() {
    let mut vmas = VmaSet::new();
    assert_eq!(vmas.insert(anon_area(0x1000, 0x1000)), Err(Errno::EINVAL));
}

/// find 在包含 addr 的 VMA 内返回该 VMA。
#[ktest]
fn find_existing_vma() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x3000)).unwrap();
    let found = vmas.find(0x2000);
    assert!(found.is_some());
    assert_eq!(found.unwrap().range, 0x1000..0x3000);
}

/// find 在不属于任何 VMA 的地址处返回 None。
#[ktest]
fn find_hole_returns_none() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x2000)).unwrap();
    assert!(vmas.find(0x3000).is_none());
}

/// find_gap 在两个 VMA 之间找到足够大的空洞。
#[ktest]
fn find_gap_simple_hole() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x2000)).unwrap();
    vmas.insert(anon_area(0x3000, 0x4000)).unwrap();
    assert_eq!(vmas.find_gap(0x1500..0x5000, 0x1000), Some(0x2000..0x3000));
}

/// find_gap 在空洞不够大时返回 None。
#[ktest]
fn find_gap_no_space() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x2000)).unwrap();
    vmas.insert(anon_area(0x2000, 0x3000)).unwrap();
    assert!(vmas.find_gap(0x1000..0x3000, 0x500).is_none());
}

/// unmap_range 精确匹配一个 VMA 时移除该 VMA。
#[ktest]
fn unmap_range_exact_match() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x2000)).unwrap();
    let removed = vmas.unmap_range(&(0x1000..0x2000));
    assert_eq!(removed.len(), 1);
    assert!(vmas.is_empty());
}

/// unmap_range 在 VMA 中间穿过时自动 split，移除中间片段。
#[ktest]
fn unmap_range_splits_vma() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x4000)).unwrap();
    let removed = vmas.unmap_range(&(0x2000..0x3000));
    assert_eq!(removed.len(), 1);
    assert!(vmas.find(0x1500).is_some());
    assert!(vmas.find(0x3500).is_some());
}

/// contains_range 验证 VMA 连续覆盖整个目标区间。
#[ktest]
fn contains_range_true() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x4000)).unwrap();
    assert!(vmas.contains_range(&(0x2000..0x3000)));
}

/// contains_range 在部分覆盖时返回 false。
#[ktest]
fn contains_range_false_partial() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x3000)).unwrap();
    assert!(!vmas.contains_range(&(0x2000..0x4000)));
}

/// insert 内部调用 merge_neighbors，相邻同标志匿名区自动合并为一条。
#[ktest]
fn merge_neighbors_compatible() {
    let mut vmas = VmaSet::new();
    vmas.insert(anon_area(0x1000, 0x2000)).unwrap();
    vmas.insert(anon_area(0x2000, 0x3000)).unwrap();
    assert_eq!(vmas.len(), 1);
    assert_eq!(vmas.find(0x1500).unwrap().range, 0x1000..0x3000);
}
