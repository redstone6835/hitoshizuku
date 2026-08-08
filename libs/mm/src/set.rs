//! 按起址排序的 VMA 集合。
//!
//! 核心数据结构：`Vec<VmArea>`，按 `range.start` 升序排列。选 Vec 而非
//! BTreeMap 的理由：(1) 我们的 VMA 数量级在百级，Vec 的缓存友好性优于树；
//! (2) 二分查找的常数远小于 BTree 路径遍历；(3) 实现更直白。未来若真有
//! 瓶颈，可无痛换成自写树——这层接口不暴露内部细节。
//!
//! ## 不变式
//!
//! - 任意两条 VMA 的 `range` 不相交（严格 disjoint）。
//! - `areas` 按 `range.start` 严格升序排列；`range.start < range.end`（空 range 不得插入）。
//! - backing 的 offset / direct paddr 能覆盖整段长度，计算一段末尾不溢出。
//!
//! ## 操作风格
//!
//! `unmap_range` / `protect_range` 走"先摘出要改的 VMAs → 分裂裁剪 → 重新插入"
//! 的拷贝-修改-写回路径。分裂 / 合并的粒度都以 4K 对齐为前提，但本层不强制
//! 检查对齐——调用方（VmSpace）在 range 送进来前做一次规整。

use alloc::vec::Vec;
use core::convert::TryFrom;
use core::ops::Range;

use errno::Errno;

use crate::area::{VmArea, VmBacking};
use crate::flags::VmFlags;

/// VMA 集合。线程安全由外层（VmSpace）决定；本结构不自带锁。
#[derive(Default)]
pub struct VmaSet {
    areas: Vec<VmArea>,
}

/// `VmaSet::find_mut` 返回的受限可变视图。
///
/// 它刻意不暴露 `&mut VmArea`，避免外部修改 `range.start` 后破坏
/// 排序不变式。
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
        Self { areas: Vec::new() }
    }

    #[kernel_symbols::export(name = "mm.set.VmaSet.len", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn len(&self) -> usize {
        self.areas.len()
    }

    #[kernel_symbols::export(name = "mm.set.VmaSet.is_empty", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn is_empty(&self) -> bool {
        self.areas.is_empty()
    }

    /// 插入新 VMA。与任何已有 VMA 重叠返 `EEXIST`；空 range 返 `EINVAL`。
    #[kernel_symbols::export(name = "mm.set.VmaSet.insert", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn insert(&mut self, area: VmArea) -> Result<(), Errno> {
        if !area.is_well_formed() {
            return Err(Errno::EINVAL);
        }
        if self.overlaps_range(&area.range) {
            return Err(Errno::EEXIST);
        }
        let pos = self
            .areas
            .partition_point(|a| a.range.start < area.range.start);
        self.areas.insert(pos, area);
        self.merge_around(pos);
        Ok(())
    }

    /// 查 `addr` 所在 VMA。
    #[kernel_symbols::export(name = "mm.set.VmaSet.find", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW)]
    pub fn find(&self, addr: usize) -> Option<&VmArea> {
        // 最靠后且 start <= addr 的那一条。
        let idx = self.areas.partition_point(|a| a.range.start <= addr);
        if idx == 0 {
            return None;
        }
        let area = &self.areas[idx - 1];
        if area.is_well_formed() && area.contains(addr) {
            Some(area)
        } else {
            None
        }
    }

    /// 同上的受限可变版。只允许改 flags，不允许改 range/backing。
    pub fn find_mut(&mut self, addr: usize) -> Option<VmAreaMut<'_>> {
        let idx = self.areas.partition_point(|a| a.range.start <= addr);
        if idx == 0 {
            return None;
        }
        let area = &mut self.areas[idx - 1];
        if area.is_well_formed() && area.contains(addr) {
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
        // First area with start > page (i.e., start >= page + 1).
        let threshold = page.checked_add(1)?;
        let idx = self.areas.partition_point(|a| a.range.start < threshold);
        if idx >= self.areas.len() {
            return None;
        }
        let key = self.areas[idx].range.start;
        {
            let area = &self.areas[idx];
            if !area.is_well_formed() {
                return None;
            }
            if page >= area.range.start || !area.flags.has(VmFlags::GROWS_DOWN) {
                return None;
            }
            if !matches!(area.backing, VmBacking::Anon { .. }) {
                return None;
            }
        }
        if idx > 0 {
            let prev = &self.areas[idx - 1];
            if prev.range.end > page {
                return None;
            }
        }
        {
            let area = &self.areas[idx];
            let lowest = area.range.end.saturating_sub(max_growth);
            if page < lowest {
                return None;
            }
        }
        // Shrink start in-place; sorted invariant is maintained because
        // prev.range.start < prev.range.end <= page = new_start.
        let flags = self.areas[idx].flags;
        let added = page..key;
        self.areas[idx].range.start = page;
        Some((added, flags))
    }

    /// 与 `range` 有交集的全部 VMA。迭代期间禁止增删。
    pub fn iter_overlap<'a>(
        &'a self,
        range: &'a Range<usize>,
    ) -> impl Iterator<Item = &'a VmArea> + 'a {
        // All areas with start < range.end, filtered to those that actually overlap.
        let end_bound = self.areas.partition_point(|a| a.range.start < range.end);
        self.areas[..end_bound].iter().filter(move |v| {
            range.start < range.end && v.range.start < v.range.end && v.range.end > range.start
        })
    }

    /// `range` 是否完全没有被任何 VMA 占用。
    #[kernel_symbols::export(name = "mm.set.VmaSet.is_range_free", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn is_range_free(&self, range: &Range<usize>) -> bool {
        range.start < range.end && !self.overlaps_range(range)
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
        let end_bound = self.areas.partition_point(|a| a.range.start < search.end);
        for area in &self.areas[..end_bound] {
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

    /// 在 `search` 内查找长度足够且起点满足 `alignment` 的空洞。
    #[kernel_symbols::export(name = "mm.set.VmaSet.find_aligned_gap", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn find_aligned_gap(
        &self,
        search: Range<usize>,
        len: usize,
        alignment: usize,
    ) -> Option<Range<usize>> {
        if len == 0 || search.start >= search.end || alignment == 0 || !alignment.is_power_of_two()
        {
            return None;
        }
        let align_up = |value: usize| {
            value
                .checked_add(alignment - 1)
                .map(|aligned| aligned & !(alignment - 1))
        };
        let mut cursor = search.start;
        let end_bound = self
            .areas
            .partition_point(|area| area.range.start < search.end);
        for area in &self.areas[..end_bound] {
            if !area.is_well_formed() {
                return None;
            }
            if area.range.end <= search.start {
                continue;
            }
            let gap_end = area.range.start.min(search.end);
            if gap_end > cursor {
                let aligned = align_up(cursor)?;
                let end = aligned.checked_add(len)?;
                if end <= gap_end {
                    return Some(aligned..end);
                }
            }
            cursor = cursor.max(area.range.end);
            if cursor >= search.end {
                return None;
            }
        }
        let aligned = align_up(cursor)?;
        let end = aligned.checked_add(len)?;
        (end <= search.end).then_some(aligned..end)
    }

    /// 取消 `range` 内的所有映射，返回被摘掉的 VMA 片段列表（已按 range 裁剪）。
    /// 上层据此对每个片段下发 `UserPgdOps::unmap`。跨 VMA 边界时自动 split。
    #[kernel_symbols::export(name = "mm.set.VmaSet.unmap_range", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn unmap_range(&mut self, range: &Range<usize>) -> Vec<VmArea> {
        if range.start >= range.end {
            return Vec::new();
        }
        // Drain all areas that overlap [range.start, range.end) at once.
        let first = self.areas.partition_point(|a| a.range.end <= range.start);
        let last = self.areas.partition_point(|a| a.range.start < range.end);
        if first >= last {
            return Vec::new();
        }

        let overlapping: Vec<VmArea> = self.areas.drain(first..last).collect();

        let mut removed = Vec::with_capacity(overlapping.len());
        let mut residuals: Vec<VmArea> = Vec::new();

        for area in &overlapping {
            if !area.is_well_formed() {
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
                residuals.push(l);
            }
            if let Some(r) = right_residual {
                residuals.push(r);
            }
            removed.push(mid_part);
        }

        // Re-insert residuals at their correct sorted positions.
        for r in residuals {
            let pos = self
                .areas
                .partition_point(|a| a.range.start < r.range.start);
            self.areas.insert(pos, r);
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

    /// 当 `range` 完全位于单个 VMA 内时直接修改权限。
    ///
    /// 返回 `Some(true)` 表示发生了权限变化，`Some(false)` 表示权限本来就一致；跨
    /// VMA 或范围无效时返回 `None`，由调用方继续使用通用范围更新路径。子区间最多
    /// 原地分裂成三段，不构造摘取列表，也不进行全表重新插入。
    pub fn protect_single_area(
        &mut self,
        range: &Range<usize>,
        new_flags: VmFlags,
    ) -> Option<bool> {
        if range.start >= range.end {
            return None;
        }
        let next = self
            .areas
            .partition_point(|area| area.range.start <= range.start);
        let idx = next.checked_sub(1)?;
        let area = self.areas.get(idx)?;
        if !area.is_well_formed() || range.start < area.range.start || range.end > area.range.end {
            return None;
        };
        let permissions = new_flags.permissions();
        if area.flags.permissions() == permissions {
            return Some(false);
        }

        let mut replacement = area.clone();
        match (
            range.start == replacement.range.start,
            range.end == replacement.range.end,
        ) {
            (true, true) => {
                replacement.flags = replacement.flags.with_permissions(permissions);
                self.areas[idx] = replacement;
                self.merge_around(idx);
            }
            (true, false) => {
                let (mut protected, right) = replacement.split_at(range.end)?;
                protected.flags = protected.flags.with_permissions(permissions);
                self.areas[idx] = protected;
                self.areas.insert(idx + 1, right);
                self.merge_around(idx);
            }
            (false, true) => {
                let (left, mut protected) = replacement.split_at(range.start)?;
                protected.flags = protected.flags.with_permissions(permissions);
                self.areas[idx] = left;
                self.areas.insert(idx + 1, protected);
                self.merge_around(idx + 1);
            }
            (false, false) => {
                let (left, remainder) = replacement.split_at(range.start)?;
                let (mut protected, right) = remainder.split_at(range.end)?;
                protected.flags = protected.flags.with_permissions(permissions);
                self.areas[idx] = left;
                self.areas.insert(idx + 1, protected);
                self.areas.insert(idx + 2, right);
                self.merge_around(idx + 1);
            }
        }
        Some(true)
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
        if self.areas.is_empty() {
            return;
        }
        let mut pos = 0;
        while self.next_idx(pos).is_some() {
            if !self.try_merge_pair(pos) {
                pos += 1;
            }
        }
    }

    /// 为 `fork` 克隆 VMA 元数据，并封存父子双方既有的私有匿名合并来源。
    ///
    /// 文件对象和共享匿名对象仍由 `Arc` 共享；本方法不复制或修改物理页。调用后
    /// 任一地址空间新建的匿名映射都不会与继承区域跨边界合并。
    #[kernel_symbols::export(name = "mm.set.VmaSet.fork_clone_metadata", contract = "kernel.mm.vma-set@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn fork_clone_metadata(&mut self) -> Self {
        for area in &mut self.areas {
            area.backing.mark_fork_inherited();
        }
        Self {
            areas: self.areas.clone(),
        }
    }

    /// 摘出全部 VMA，供地址空间销毁路径先结束 backing 生命周期再回收页缓存。
    pub fn take_all(&mut self) -> Vec<VmArea> {
        core::mem::take(&mut self.areas)
    }

    /// 只读迭代全部 VMA，按起址升序。
    pub fn iter(&self) -> impl Iterator<Item = &VmArea> + '_ {
        self.areas.iter()
    }

    /// 只检查目标区间的即时前驱与首个区间内起点，避免线性扫描。
    fn overlaps_range(&self, range: &Range<usize>) -> bool {
        // predecessor: last area with start < range.start, check if end > range.start
        let idx = self.areas.partition_point(|a| a.range.start < range.start);
        if idx > 0 && self.areas[idx - 1].range.end > range.start {
            return true;
        }
        // any area with start in [range.start, range.end)
        idx < self.areas.len() && self.areas[idx].range.start < range.end
    }

    /// 找 `range.start == start` 的元素下标（精确匹配）。内部 merge 辅助用。
    #[allow(dead_code)]
    fn find_idx_by_start(&self, start: usize) -> Option<usize> {
        let idx = self.areas.partition_point(|a| a.range.start < start);
        if idx < self.areas.len() && self.areas[idx].range.start == start {
            Some(idx)
        } else {
            None
        }
    }

    fn next_idx(&self, pos: usize) -> Option<usize> {
        let next = pos + 1;
        if next < self.areas.len() {
            Some(next)
        } else {
            None
        }
    }

    /// 尝试合并 `areas[i]` 和 `areas[i+1]`；成功时移除 `areas[i+1]`，返回 true。
    fn try_merge_pair(&mut self, i: usize) -> bool {
        let new_end = match (self.areas.get(i), self.areas.get(i + 1)) {
            (Some(left), Some(right)) if can_merge_areas(left, right) => right.range.end,
            _ => return false,
        };
        self.areas.remove(i + 1);
        self.areas[i].range.end = new_end;
        true
    }

    /// 插入后只规整与新节点相连的局部邻居。`pos` 是刚插入元素的下标。
    fn merge_around(&mut self, mut pos: usize) {
        // Try to merge with the left neighbor first.
        if pos > 0 && self.try_merge_pair(pos - 1) {
            pos -= 1;
        }
        // Then merge right neighbors as long as possible.
        while self.next_idx(pos).is_some() {
            if !self.try_merge_pair(pos) {
                break;
            }
        }
    }
}

fn can_merge_areas(left: &VmArea, right: &VmArea) -> bool {
    if !left.is_well_formed()
        || !right.is_well_formed()
        || left.range.end != right.range.start
        || left.flags != right.flags
    {
        return false;
    }

    let left_len = left.range.end - left.range.start;
    match (&left.backing, &right.backing) {
        (
            VmBacking::Anon {
                merge_domain: left_domain,
            },
            VmBacking::Anon {
                merge_domain: right_domain,
            },
        ) => left_domain.can_merge(*right_domain),
        (
            VmBacking::SharedAnon {
                object: left_object,
                offset: left_offset,
            },
            VmBacking::SharedAnon {
                object: right_object,
                offset: right_offset,
            },
        ) => {
            alloc::sync::Arc::ptr_eq(left_object, right_object)
                && checked_offset_after(*left_offset, left_len) == Some(*right_offset)
        }
        (
            VmBacking::File {
                file: left_file,
                offset: left_offset,
            },
            VmBacking::File {
                file: right_file,
                offset: right_offset,
            },
        ) => {
            alloc::sync::Arc::ptr_eq(left_file, right_file)
                && checked_offset_after(*left_offset, left_len) == Some(*right_offset)
        }
        (VmBacking::Direct(left_base), VmBacking::Direct(right_base)) => {
            left_base.checked_add(left_len) == Some(*right_base)
        }
        _ => false,
    }
}

fn checked_offset_after(offset: u64, len: usize) -> Option<u64> {
    offset.checked_add(u64::try_from(len).ok()?)
}
