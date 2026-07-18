//! 按起址排序的 VMA 集合。
//!
//! 核心数据结构：`BTreeMap<usize /* start */, VmArea>`。选 BTreeMap 而不是
//! 侵入式 RB-tree 有两个理由：(1) 依赖 `alloc::collections` 就够，不必重写
//! 平衡树；(2) 我们的 VMA 数量级在百级，常数差异远小于可读性收益。未来若
//! 真有瓶颈，可无痛换成自写树 —— 这层接口不暴露 BTreeMap 细节。
//!
//! ## 不变式
//!
//! - 任意两条 VMA 的 `range` 不相交（严格 disjoint）。
//! - key 等于 `area.range.start`；`range.start < range.end`（空 range 不得插入）。
//! - backing 的 offset / direct paddr 能覆盖整段长度，计算一段末尾不溢出。
//!
//! ## 操作风格
//!
//! `unmap_range` / `protect_range` 走"先摘出要改的 VMAs → 分裂裁剪 → 重新插入"
//! 的拷贝-修改-写回路径。分裂 / 合并的粒度都以 4K 对齐为前提，但本层不强制
//! 检查对齐——调用方（VmSpace）在 range 送进来前做一次规整。

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::ops::Range;

use errno::Errno;

use crate::area::{VmArea, VmBacking};
use crate::flags::VmFlags;

/// VMA 集合。线程安全由外层（VmSpace）决定；本结构不自带锁。
#[derive(Default, Clone)]
pub struct VmaSet {
    tree: BTreeMap<usize, VmArea>,
}

/// `VmaSet::find_mut` 返回的受限可变视图。
///
/// 它刻意不暴露 `&mut VmArea`，避免外部修改 `range.start` 后破坏
/// `BTreeMap` key 与 VMA range 的一致性。
pub struct VmAreaMut<'a> {
    area: &'a mut VmArea,
}

impl<'a> VmAreaMut<'a> {
    pub fn as_ref(&self) -> &VmArea {
        self.area
    }

    pub fn flags_mut(&mut self) -> &mut VmFlags {
        &mut self.area.flags
    }

    pub fn set_flags(&mut self, flags: VmFlags) {
        self.area.flags = flags;
    }
}

impl<'a> core::ops::Deref for VmAreaMut<'a> {
    type Target = VmArea;

    fn deref(&self) -> &Self::Target {
        self.area
    }
}

#[kernel_symbols::export]
impl VmaSet {
    pub const fn new() -> Self {
        Self {
            tree: BTreeMap::new(),
        }
    }

    #[kernel_symbols::export(name = "mm.set.VmaSet.len", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    #[kernel_symbols::export(name = "mm.set.VmaSet.is_empty", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// 插入新 VMA。与任何已有 VMA 重叠返 `EEXIST`；空 range 返 `EINVAL`。
    #[kernel_symbols::export(name = "mm.set.VmaSet.insert", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn insert(&mut self, area: VmArea) -> Result<(), Errno> {
        if !area.is_well_formed() {
            return Err(Errno::EINVAL);
        }
        if self.iter_overlap(&area.range).next().is_some() {
            return Err(Errno::EEXIST);
        }
        self.tree.insert(area.range.start, area);
        self.merge_neighbors();
        Ok(())
    }

    /// 查 `addr` 所在 VMA。
    #[kernel_symbols::export(name = "mm.set.VmaSet.find", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW)]
    pub fn find(&self, addr: usize) -> Option<&VmArea> {
        // 最靠后且 start <= addr 的那一条。
        let (key, area) = self.tree.range(..=addr).next_back()?;
        if area.range.start == *key && area.is_well_formed() && area.contains(addr) {
            Some(area)
        } else {
            None
        }
    }

    /// 同上的受限可变版。只允许改 flags，不允许改 range/backing。
    pub fn find_mut(&mut self, addr: usize) -> Option<VmAreaMut<'_>> {
        let key = *self.tree.range(..=addr).next_back()?.0;
        let area = self.tree.get_mut(&key)?;
        if area.range.start == key && area.is_well_formed() && area.contains(addr) {
            Some(VmAreaMut { area })
        } else {
            None
        }
    }

    /// 若 `page` 落在某个 `GROWS_DOWN` 匿名 VMA 的下方且仍在允许增长窗口内，
    /// 把该 VMA 起点扩到 `page`，并返回新增页应该使用的 flags。
    #[kernel_symbols::export(name = "mm.set.VmaSet.grow_down_to", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn grow_down_to(
        &mut self,
        page: usize,
        max_growth: usize,
    ) -> Option<(Range<usize>, VmFlags)> {
        let key = *self.tree.range(page.checked_add(1)?..).next()?.0;
        let area = self.tree.get(&key)?;
        if area.range.start != key || !area.is_well_formed() {
            return None;
        }
        if page >= area.range.start || !area.flags.has(VmFlags::GROWS_DOWN) {
            return None;
        }
        if !matches!(area.backing, VmBacking::Anon) {
            return None;
        }
        if let Some((_, prev)) = self.tree.range(..key).next_back() {
            if prev.range.end > page {
                return None;
            }
        }
        let lowest = area.range.end.saturating_sub(max_growth);
        if page < lowest {
            return None;
        }
        let mut area = self.tree.remove(&key).expect("key from tree");
        area.range.start = page;
        let added = page..key;
        let flags = area.flags;
        self.tree.insert(area.range.start, area);
        Some((added, flags))
    }

    /// 与 `range` 有交集的全部 VMA。迭代期间禁止增删。
    pub fn iter_overlap<'a>(
        &'a self,
        range: &'a Range<usize>,
    ) -> impl Iterator<Item = &'a VmArea> + 'a {
        // 两部分合集：start < range.end 的全部，过滤掉 end <= range.start 的。
        self.tree.range(..range.end).map(|(_, v)| v).filter(|v| {
            range.start < range.end && v.range.start < v.range.end && v.range.end > range.start
        })
    }

    /// `range` 是否完全没有被任何 VMA 占用。
    #[kernel_symbols::export(name = "mm.set.VmaSet.is_range_free", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn is_range_free(&self, range: &Range<usize>) -> bool {
        range.start < range.end && self.iter_overlap(range).next().is_none()
    }

    /// `range` 是否被现有 VMA 连续覆盖，中间不允许有洞。
    #[kernel_symbols::export(name = "mm.set.VmaSet.contains_range", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn contains_range(&self, range: &Range<usize>) -> bool {
        if range.start >= range.end {
            return false;
        }
        let mut cursor = range.start;
        for area in self.iter_overlap(range) {
            if !area.is_well_formed() {
                return false;
            }
            if area.range.start > cursor {
                return false;
            }
            cursor = cursor.max(area.range.end);
            if cursor >= range.end {
                return true;
            }
        }
        false
    }

    /// 在 `search` 内找一段长度为 `len` 的空洞。调用方负责页对齐。
    #[kernel_symbols::export(name = "mm.set.VmaSet.find_gap", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn find_gap(&self, search: Range<usize>, len: usize) -> Option<Range<usize>> {
        if len == 0 || search.start >= search.end {
            return None;
        }
        let mut cursor = search.start;
        for area in self.tree.range(..search.end).map(|(_, v)| v) {
            if !area.is_well_formed() {
                return None;
            }
            if area.range.end <= search.start {
                continue;
            }
            let gap_end = area.range.start.min(search.end);
            if gap_end >= cursor && gap_end - cursor >= len {
                let end = cursor.checked_add(len)?;
                return Some(cursor..end);
            }
            cursor = cursor.max(area.range.end);
            if cursor >= search.end {
                return None;
            }
        }
        if search.end >= cursor && search.end - cursor >= len {
            let end = cursor.checked_add(len)?;
            Some(cursor..end)
        } else {
            None
        }
    }

    /// 取消 `range` 内的所有映射，返回被摘掉的 VMA 片段列表（已按 range 裁剪）。
    /// 上层据此对每个片段下发 `UserPgdOps::unmap`。跨 VMA 边界时自动 split。
    #[kernel_symbols::export(name = "mm.set.VmaSet.unmap_range", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn unmap_range(&mut self, range: &Range<usize>) -> Vec<VmArea> {
        if range.start >= range.end {
            return Vec::new();
        }
        // 先收集要动的 key（避免边迭代边改）。
        let keys: Vec<usize> = self
            .tree
            .range(..range.end)
            .filter(|(_, v)| v.range.end > range.start)
            .map(|(k, _)| *k)
            .collect();

        let mut removed = Vec::with_capacity(keys.len());
        for k in keys {
            let area = self.tree.remove(&k).expect("key from just-collected set");
            if area.range.start != k || !area.is_well_formed() {
                continue;
            }
            // 左残：area.start..range.start
            let (mid_start, carry_left) = if area.range.start < range.start {
                let (left, rest) = area
                    .split_at(range.start)
                    .expect("split at strictly-interior point");
                (rest.range.start, Some(left))
            } else {
                (area.range.start, None)
            };
            // 剩余部分从 mid_start 到 area.range.end 拆中间 vs 右残
            let area_rest = area
                .clip_to(&(mid_start..area.range.end))
                .expect("non-empty rest after left clip");
            let (mid_part, right_residual) = if area_rest.range.end > range.end {
                let (mid, right) = area_rest
                    .split_at(range.end)
                    .expect("split at strictly-interior point");
                (mid, Some(right))
            } else {
                (area_rest, None)
            };
            if let Some(l) = carry_left {
                self.tree.insert(l.range.start, l);
            }
            if let Some(r) = right_residual {
                self.tree.insert(r.range.start, r);
            }
            removed.push(mid_part);
        }
        removed
    }

    /// 修改 `range` 内全部 VMA 的 flags。跨边界时自动 split；返回被改过片段的
    /// (range, new_flags) 清单，供上层下发页表 protect。
    #[kernel_symbols::export(name = "mm.set.VmaSet.protect_range", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn protect_range(
        &mut self,
        range: &Range<usize>,
        new_flags: VmFlags,
    ) -> Vec<(Range<usize>, VmFlags)> {
        let cut_pieces = self.unmap_range(range);
        let mut out = Vec::with_capacity(cut_pieces.len());
        for mut p in cut_pieces {
            p.flags = p.flags.with_permissions(new_flags.permissions());
            out.push((p.range.clone(), p.flags));
            // insert 不会重叠（来自 unmap_range 的结果本身不相交、且已把对应段挖掉）。
            let _ = self.insert(p);
        }
        self.merge_neighbors();
        out
    }

    /// 修改 `range` 内全部 VMA 的非几何属性。跨边界时自动 split，backing 不变。
    pub fn update_flags_range(
        &mut self,
        range: &Range<usize>,
        update: impl Fn(VmFlags) -> VmFlags,
    ) -> Vec<(Range<usize>, VmFlags)> {
        let cut_pieces = self.unmap_range(range);
        let mut out = Vec::with_capacity(cut_pieces.len());
        for mut p in cut_pieces {
            p.flags = update(p.flags);
            out.push((p.range.clone(), p.flags));
            let _ = self.insert(p);
        }
        self.merge_neighbors();
        out
    }

    /// 合并相邻且 flags/backing 兼容的 VMA（Anon、SharedAnon/File 偏移衔接、
    /// Direct 物理地址衔接）。
    /// 典型调用点：insert 之后的紧邻合并；批量 unmap 之后的收尾。
    #[kernel_symbols::export(name = "mm.set.VmaSet.merge_neighbors", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn merge_neighbors(&mut self) {
        loop {
            let keys: Vec<usize> = self.tree.keys().copied().collect();
            let mut merged = false;
            for pair in keys.windows(2) {
                let k_left = pair[0];
                let k_right = pair[1];
                let (can_merge, new_end) = match (self.tree.get(&k_left), self.tree.get(&k_right)) {
                    (Some(l), Some(r)) => {
                        if !l.is_well_formed() || !r.is_well_formed() {
                            (false, 0)
                        } else {
                            let left_len = l.range.end - l.range.start;
                            let adjacent = l.range.end == r.range.start;
                            let same_flags = l.flags == r.flags;
                            let same_backing = match (&l.backing, &r.backing) {
                                (crate::area::VmBacking::Anon, crate::area::VmBacking::Anon) => {
                                    true
                                }
                                (
                                    crate::area::VmBacking::SharedAnon { id: il, offset: ol },
                                    crate::area::VmBacking::SharedAnon { id: ir, offset: or },
                                ) => il == ir && checked_offset_after(*ol, left_len) == Some(*or),
                                (
                                    crate::area::VmBacking::File {
                                        file: fl,
                                        offset: ol,
                                    },
                                    crate::area::VmBacking::File {
                                        file: fr,
                                        offset: or,
                                    },
                                ) => {
                                    alloc::sync::Arc::ptr_eq(fl, fr)
                                        && checked_offset_after(*ol, left_len) == Some(*or)
                                }
                                (
                                    crate::area::VmBacking::Direct(bl),
                                    crate::area::VmBacking::Direct(br),
                                ) => bl.checked_add(left_len) == Some(*br),
                                _ => false,
                            };
                            (adjacent && same_flags && same_backing, r.range.end)
                        }
                    }
                    _ => (false, 0),
                };
                if can_merge {
                    let _right = self.tree.remove(&k_right);
                    let left = self.tree.get_mut(&k_left).unwrap();
                    left.range.end = new_end;
                    merged = true;
                    break;
                }
            }
            if !merged {
                break;
            }
        }
    }

    /// 深拷贝 VMA 元数据（不触物理页；Arc<dyn FileLike> 共享）。
    /// 典型调用：`VmSpace::fork` 的第一步，拿到同构的 VmaSet 之后再逐页复制。
    #[kernel_symbols::export(name = "mm.set.VmaSet.deep_clone_metadata", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn deep_clone_metadata(&self) -> Self {
        Self {
            tree: self.tree.clone(),
        }
    }

    /// 只读迭代全部 VMA，按起址升序。
    pub fn iter(&self) -> impl Iterator<Item = &VmArea> + '_ {
        self.tree.values()
    }
}

fn checked_offset_after(offset: u64, len: usize) -> Option<u64> {
    offset.checked_add(u64::try_from(len).ok()?)
}
