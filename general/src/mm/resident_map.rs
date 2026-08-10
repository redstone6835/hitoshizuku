use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::ops::Range;

const RADIX_BITS: usize = 6;
const RADIX_SLOTS: usize = 1 << RADIX_BITS;
const RADIX_MASK: usize = RADIX_SLOTS - 1;

enum RadixNode<T> {
    Branch([Option<Box<RadixNode<T>>>; RADIX_SLOTS]),
    Leaf(RadixSlots<T>),
}

struct RadixSlots<T> {
    occupied: u64,
    entries: [MaybeUninit<T>; RADIX_SLOTS],
}

impl<T> RadixSlots<T> {
    const fn new() -> Self {
        Self {
            occupied: 0,
            entries: [const { MaybeUninit::uninit() }; RADIX_SLOTS],
        }
    }

    fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    fn contains(&self, slot: usize) -> bool {
        debug_assert!(slot < RADIX_SLOTS);
        self.occupied & (1u64 << slot) != 0
    }

    fn get(&self, slot: usize) -> Option<&T> {
        self.contains(slot).then(|| {
            // Safety: 占用位只在对应槽位完成初始化后设置。
            unsafe { self.entries[slot].assume_init_ref() }
        })
    }

    fn get_mut(&mut self, slot: usize) -> Option<&mut T> {
        if !self.contains(slot) {
            return None;
        }
        // Safety: 占用位保证该槽位已初始化，独占借用保证不会制造别名。
        Some(unsafe { self.entries[slot].assume_init_mut() })
    }

    fn insert(&mut self, slot: usize, value: T) -> Option<T> {
        debug_assert!(slot < RADIX_SLOTS);
        if self.contains(slot) {
            // Safety: 占用位保证该槽位已初始化。
            return Some(core::mem::replace(
                unsafe { self.entries[slot].assume_init_mut() },
                value,
            ));
        }
        self.entries[slot].write(value);
        self.occupied |= 1u64 << slot;
        None
    }

    fn take(&mut self, slot: usize) -> Option<T> {
        if !self.contains(slot) {
            return None;
        }
        self.occupied &= !(1u64 << slot);
        // Safety: 清除占用位前该槽位已初始化，此后所有路径都不会再次读取它。
        Some(unsafe { self.entries[slot].assume_init_read() })
    }

    fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        let occupied = self.occupied;
        self.entries
            .iter()
            .enumerate()
            .filter_map(move |(slot, entry)| {
                (occupied & (1u64 << slot) != 0)
                    .then(|| {
                        // Safety: 迭代使用创建时的占用位快照，只暴露已初始化槽位。
                        unsafe { entry.assume_init_ref() }
                    })
                    .map(|value| (slot, value))
            })
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut T)> {
        let occupied = self.occupied;
        self.entries
            .iter_mut()
            .enumerate()
            .filter_map(move |(slot, entry)| {
                (occupied & (1u64 << slot) != 0)
                    .then(|| {
                        // Safety: 每个槽位只迭代一次，且调用方持有整个槽位表的独占借用。
                        unsafe { entry.assume_init_mut() }
                    })
                    .map(|value| (slot, value))
            })
    }
}

impl<T> Drop for RadixSlots<T> {
    fn drop(&mut self) {
        for slot in 0..RADIX_SLOTS {
            if self.contains(slot) {
                // Safety: 占用位保证该槽位已初始化，析构期间每个槽位只访问一次。
                unsafe { self.entries[slot].assume_init_drop() };
            }
        }
    }
}

impl<T> RadixNode<T> {
    fn new(level: usize) -> Self {
        if level == 0 {
            Self::Leaf(RadixSlots::new())
        } else {
            Self::Branch(core::array::from_fn(|_| None))
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Branch(children) => children.iter().all(Option::is_none),
            Self::Leaf(entries) => entries.is_empty(),
        }
    }
}

/// 以虚拟页号为键的动态高度基数树映射。
///
/// 根高度随最高存活页号增长和收缩，分支与叶节点按需分配。精确查询只访问当前
/// 地址跨度所需的槽位；范围遍历按槽位顺序递归，并用页号前缀跳过无关子树。
pub(super) struct RadixPageMap<T> {
    root: RadixNode<T>,
    root_level: usize,
    page_shift: usize,
    len: usize,
}

impl<T> RadixPageMap<T> {
    pub(super) fn new(page_size: usize) -> Self {
        assert!(page_size >= allocator::PAGE_SIZE);
        assert!(page_size.is_power_of_two());
        Self {
            root: RadixNode::new(0),
            root_level: 0,
            page_shift: page_size.trailing_zeros() as usize,
            len: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn contains_key(&self, address: usize) -> bool {
        self.get(address).is_some()
    }

    pub(super) fn get(&self, address: usize) -> Option<&T> {
        let page_index = self.page_index(address);
        if required_root_level(page_index) > self.root_level {
            return None;
        }
        get_node(&self.root, self.root_level, page_index)
    }

    pub(super) fn get_mut(&mut self, address: usize) -> Option<&mut T> {
        let page_index = self.page_index(address);
        if required_root_level(page_index) > self.root_level {
            return None;
        }
        get_node_mut(&mut self.root, self.root_level, page_index)
    }

    pub(super) fn insert(&mut self, address: usize, value: T) -> Option<T> {
        let page_index = self.page_index(address);
        self.grow_root(required_root_level(page_index));
        let previous = insert_node(&mut self.root, self.root_level, page_index, value);
        if previous.is_none() {
            self.len += 1;
        }
        previous
    }

    pub(super) fn insert_contiguous<I>(&mut self, start: usize, values: I) -> usize
    where
        I: ExactSizeIterator<Item = T>,
    {
        let mut values = values;
        let value_count = values.len();
        if value_count == 0 {
            return 0;
        }

        let start_page = self.page_index(start);
        let last_page = start_page
            .checked_add(value_count - 1)
            .expect("连续 resident 页范围溢出");
        self.grow_root(required_root_level(last_page));

        let mut page_index = start_page;
        let mut inserted = 0usize;
        let mut replaced = 0usize;
        while inserted < value_count {
            let leaf_slot = page_index & RADIX_MASK;
            let chunk_len = (RADIX_SLOTS - leaf_slot).min(value_count - inserted);
            let entries = leaf_entries_mut(&mut self.root, self.root_level, page_index);
            for slot in leaf_slot..leaf_slot + chunk_len {
                let value = values.next().expect("连续 resident 页迭代器提前结束");
                if entries.insert(slot, value).is_some() {
                    replaced += 1;
                } else {
                    self.len += 1;
                }
            }
            inserted += chunk_len;
            page_index += chunk_len;
        }
        debug_assert_eq!(values.len(), 0);
        replaced
    }

    pub(super) fn remove(&mut self, address: usize) -> Option<T> {
        let page_index = self.page_index(address);
        if required_root_level(page_index) > self.root_level {
            return None;
        }
        let removed = remove_node(&mut self.root, self.root_level, page_index);
        if removed.is_some() {
            self.len -= 1;
            self.shrink_root();
        }
        removed
    }

    pub(super) fn clear(&mut self) {
        self.root = RadixNode::new(0);
        self.root_level = 0;
        self.len = 0;
    }

    pub(super) fn for_each_mut(&mut self, mut visit: impl FnMut(usize, &mut T)) {
        for_each_node_mut(
            &mut self.root,
            self.root_level,
            0,
            self.page_shift,
            &mut visit,
        );
    }

    pub(super) fn for_each_range(&self, range: Range<usize>, mut visit: impl FnMut(usize, &T)) {
        let page_range = self.page_range(range);
        for_each_range_node(
            &self.root,
            self.root_level,
            0,
            &page_range,
            self.page_shift,
            &mut visit,
        );
    }

    pub(super) fn for_each_range_mut(
        &mut self,
        range: Range<usize>,
        mut visit: impl FnMut(usize, &mut T),
    ) {
        let page_range = self.page_range(range);
        for_each_range_node_mut(
            &mut self.root,
            self.root_level,
            0,
            &page_range,
            self.page_shift,
            &mut visit,
        );
    }

    #[cfg(any(test, feature = "performance-profile"))]
    pub(super) fn count_range(&self, range: Range<usize>) -> usize {
        let mut count = 0usize;
        self.for_each_range(range, |_address, _value| count += 1);
        count
    }

    pub(super) fn keys_in_range(&self, range: Range<usize>) -> Vec<usize> {
        let mut keys = Vec::new();
        self.for_each_range(range, |address, _value| keys.push(address));
        keys
    }

    fn page_index(&self, address: usize) -> usize {
        debug_assert_eq!(address & ((1usize << self.page_shift) - 1), 0);
        address >> self.page_shift
    }

    fn page_range(&self, range: Range<usize>) -> Range<usize> {
        debug_assert_eq!(range.start & ((1usize << self.page_shift) - 1), 0);
        debug_assert_eq!(range.end & ((1usize << self.page_shift) - 1), 0);
        (range.start >> self.page_shift)..(range.end >> self.page_shift)
    }

    fn grow_root(&mut self, target_level: usize) {
        if target_level <= self.root_level {
            return;
        }
        if self.len == 0 {
            self.root = RadixNode::new(target_level);
            self.root_level = target_level;
            return;
        }
        while self.root_level < target_level {
            let old_root = core::mem::replace(&mut self.root, RadixNode::new(0));
            let mut new_root = RadixNode::new(self.root_level + 1);
            let RadixNode::Branch(children) = &mut new_root else {
                unreachable!("radix 根扩展必须创建分支节点");
            };
            children[0] = Some(Box::new(old_root));
            self.root = new_root;
            self.root_level += 1;
        }
    }

    fn shrink_root(&mut self) {
        if self.len == 0 {
            self.clear();
            return;
        }
        while self.root_level != 0 {
            let next = match &mut self.root {
                RadixNode::Branch(children)
                    if children[0].is_some() && children[1..].iter().all(Option::is_none) =>
                {
                    children[0].take()
                }
                _ => None,
            };
            let Some(next) = next else {
                break;
            };
            self.root = *next;
            self.root_level -= 1;
        }
    }
}

fn required_root_level(page_index: usize) -> usize {
    let significant_bits = usize::BITS as usize - page_index.leading_zeros() as usize;
    significant_bits.saturating_sub(1) / RADIX_BITS
}

fn radix_slot(page_index: usize, level: usize) -> usize {
    (page_index >> (level * RADIX_BITS)) & RADIX_MASK
}

fn get_node<T>(node: &RadixNode<T>, level: usize, page_index: usize) -> Option<&T> {
    match node {
        RadixNode::Branch(children) => {
            debug_assert!(level != 0);
            let child = children[radix_slot(page_index, level)].as_deref()?;
            get_node(child, level - 1, page_index)
        }
        RadixNode::Leaf(entries) => {
            debug_assert_eq!(level, 0);
            entries.get(page_index & RADIX_MASK)
        }
    }
}

fn get_node_mut<T>(node: &mut RadixNode<T>, level: usize, page_index: usize) -> Option<&mut T> {
    match node {
        RadixNode::Branch(children) => {
            debug_assert!(level != 0);
            let child = children[radix_slot(page_index, level)].as_deref_mut()?;
            get_node_mut(child, level - 1, page_index)
        }
        RadixNode::Leaf(entries) => {
            debug_assert_eq!(level, 0);
            entries.get_mut(page_index & RADIX_MASK)
        }
    }
}

fn insert_node<T>(node: &mut RadixNode<T>, level: usize, page_index: usize, value: T) -> Option<T> {
    match node {
        RadixNode::Branch(children) => {
            debug_assert!(level != 0);
            let child = children[radix_slot(page_index, level)]
                .get_or_insert_with(|| Box::new(RadixNode::new(level - 1)));
            insert_node(child, level - 1, page_index, value)
        }
        RadixNode::Leaf(entries) => {
            debug_assert_eq!(level, 0);
            entries.insert(page_index & RADIX_MASK, value)
        }
    }
}

fn leaf_entries_mut<T>(
    node: &mut RadixNode<T>,
    level: usize,
    page_index: usize,
) -> &mut RadixSlots<T> {
    match node {
        RadixNode::Branch(children) => {
            debug_assert!(level != 0);
            let child = children[radix_slot(page_index, level)]
                .get_or_insert_with(|| Box::new(RadixNode::new(level - 1)));
            leaf_entries_mut(child, level - 1, page_index)
        }
        RadixNode::Leaf(entries) => {
            debug_assert_eq!(level, 0);
            entries
        }
    }
}

fn remove_node<T>(node: &mut RadixNode<T>, level: usize, page_index: usize) -> Option<T> {
    match node {
        RadixNode::Branch(children) => {
            debug_assert!(level != 0);
            let slot = radix_slot(page_index, level);
            let (removed, prune) = {
                let child = children[slot].as_deref_mut()?;
                let removed = remove_node(child, level - 1, page_index);
                let prune = removed.is_some() && child.is_empty();
                (removed, prune)
            };
            if prune {
                children[slot] = None;
            }
            removed
        }
        RadixNode::Leaf(entries) => {
            debug_assert_eq!(level, 0);
            entries.take(page_index & RADIX_MASK)
        }
    }
}

fn for_each_node_mut<T>(
    node: &mut RadixNode<T>,
    level: usize,
    prefix: usize,
    page_shift: usize,
    visit: &mut impl FnMut(usize, &mut T),
) {
    match node {
        RadixNode::Branch(children) => {
            let shift = level * RADIX_BITS;
            for (slot, child) in children.iter_mut().enumerate() {
                let Some(child) = child.as_deref_mut() else {
                    continue;
                };
                for_each_node_mut(
                    child,
                    level - 1,
                    prefix | (slot << shift),
                    page_shift,
                    visit,
                );
            }
        }
        RadixNode::Leaf(entries) => {
            for (slot, value) in entries.iter_mut() {
                visit((prefix | slot) << page_shift, value);
            }
        }
    }
}

fn for_each_range_node<T>(
    node: &RadixNode<T>,
    level: usize,
    prefix: usize,
    range: &Range<usize>,
    page_shift: usize,
    visit: &mut impl FnMut(usize, &T),
) {
    match node {
        RadixNode::Branch(children) => {
            let shift = level * RADIX_BITS;
            let span = 1usize << shift;
            for (slot, child) in children.iter().enumerate() {
                let Some(child) = child.as_deref() else {
                    continue;
                };
                let child_start = prefix | (slot << shift);
                let child_end = child_start.saturating_add(span);
                if child_start >= range.end || child_end <= range.start {
                    continue;
                }
                for_each_range_node(child, level - 1, child_start, range, page_shift, visit);
            }
        }
        RadixNode::Leaf(entries) => {
            for (slot, value) in entries.iter() {
                let page_index = prefix | slot;
                if page_index >= range.end {
                    break;
                }
                if page_index < range.start {
                    continue;
                }
                visit(page_index << page_shift, value);
            }
        }
    }
}

fn for_each_range_node_mut<T>(
    node: &mut RadixNode<T>,
    level: usize,
    prefix: usize,
    range: &Range<usize>,
    page_shift: usize,
    visit: &mut impl FnMut(usize, &mut T),
) {
    match node {
        RadixNode::Branch(children) => {
            let shift = level * RADIX_BITS;
            let span = 1usize << shift;
            for (slot, child) in children.iter_mut().enumerate() {
                let Some(child) = child.as_deref_mut() else {
                    continue;
                };
                let child_start = prefix | (slot << shift);
                let child_end = child_start.saturating_add(span);
                if child_start >= range.end || child_end <= range.start {
                    continue;
                }
                for_each_range_node_mut(child, level - 1, child_start, range, page_shift, visit);
            }
        }
        RadixNode::Leaf(entries) => {
            for (slot, value) in entries.iter_mut() {
                let page_index = prefix | slot;
                if page_index >= range.end {
                    break;
                }
                if page_index < range.start {
                    continue;
                }
                visit(page_index << page_shift, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::Cell;

    use super::{RadixPageMap, RadixSlots};

    const PAGE_SIZE: usize = 4096;

    struct DropCounter(Rc<Cell<usize>>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn radix_slots_drop_each_initialized_value_once() {
        let drops = Rc::new(Cell::new(0));
        let mut slots = RadixSlots::new();
        assert!(slots.insert(3, DropCounter(Rc::clone(&drops))).is_none());
        let replaced = slots
            .insert(3, DropCounter(Rc::clone(&drops)))
            .expect("占用槽位必须返回旧值");
        drop(replaced);
        assert_eq!(drops.get(), 1);
        drop(slots.take(3));
        assert_eq!(drops.get(), 2);
        slots.insert(7, DropCounter(Rc::clone(&drops)));
        drop(slots);
        assert_eq!(drops.get(), 3);
    }

    #[test]
    fn exact_operations_cross_radix_boundaries() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);
        let addresses = [
            0,
            63 * PAGE_SIZE,
            64 * PAGE_SIZE,
            4095 * PAGE_SIZE,
            4096 * PAGE_SIZE,
            0x7fff_ffff_f000,
        ];

        for (value, address) in addresses.into_iter().enumerate() {
            assert_eq!(pages.insert(address, value), None);
        }
        assert_eq!(pages.len(), addresses.len());
        for (value, address) in addresses.into_iter().enumerate() {
            assert_eq!(pages.get(address), Some(&value));
        }

        assert_eq!(pages.insert(64 * PAGE_SIZE, 99), Some(2));
        assert_eq!(pages.len(), addresses.len());
        assert_eq!(pages.remove(64 * PAGE_SIZE), Some(99));
        assert_eq!(pages.get(64 * PAGE_SIZE), None);
        assert_eq!(pages.len(), addresses.len() - 1);
    }

    #[test]
    fn contiguous_insert_fills_one_leaf() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);

        assert_eq!(
            pages.insert_contiguous(8 * PAGE_SIZE, [10usize, 11, 12].into_iter()),
            0
        );
        assert_eq!(pages.len(), 3);
        assert_eq!(pages.get(8 * PAGE_SIZE), Some(&10));
        assert_eq!(pages.get(9 * PAGE_SIZE), Some(&11));
        assert_eq!(pages.get(10 * PAGE_SIZE), Some(&12));
    }

    #[test]
    fn contiguous_insert_crosses_leaf_boundary() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);

        assert_eq!(
            pages.insert_contiguous(62 * PAGE_SIZE, [62usize, 63, 64, 65].into_iter()),
            0
        );
        assert_eq!(pages.len(), 4);
        for page in 62usize..=65 {
            assert_eq!(pages.get(page * PAGE_SIZE), Some(&page));
        }
    }

    #[test]
    fn contiguous_insert_grows_root_for_high_pages() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);
        let start = 4095 * PAGE_SIZE;

        assert_eq!(pages.insert_contiguous(start, [1usize, 2].into_iter()), 0);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages.get(start), Some(&1));
        assert_eq!(pages.get(start + PAGE_SIZE), Some(&2));
        assert!(pages.root_level >= 2);
    }

    #[test]
    fn contiguous_insert_counts_replacements() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);
        pages.insert(64 * PAGE_SIZE, 100usize);
        pages.insert(66 * PAGE_SIZE, 300usize);

        assert_eq!(
            pages.insert_contiguous(63 * PAGE_SIZE, [1usize, 2, 3, 4].into_iter()),
            2
        );
        assert_eq!(pages.len(), 4);
        for (offset, value) in [1usize, 2, 3, 4].into_iter().enumerate() {
            assert_eq!(pages.get((63 + offset) * PAGE_SIZE), Some(&value));
        }
    }

    #[test]
    fn range_walk_is_ordered_and_bounded() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);
        for page in [130usize, 1, 129, 64, 128, 63, 65] {
            pages.insert(page * PAGE_SIZE, page);
        }

        let mut visited = Vec::new();
        pages.for_each_range(64 * PAGE_SIZE..130 * PAGE_SIZE, |address, value| {
            visited.push((address / PAGE_SIZE, *value));
        });

        assert_eq!(visited, vec![(64, 64), (65, 65), (128, 128), (129, 129)]);
        assert_eq!(pages.count_range(64 * PAGE_SIZE..130 * PAGE_SIZE), 4);
        assert_eq!(
            pages.keys_in_range(64 * PAGE_SIZE..130 * PAGE_SIZE),
            vec![
                64 * PAGE_SIZE,
                65 * PAGE_SIZE,
                128 * PAGE_SIZE,
                129 * PAGE_SIZE
            ]
        );
    }

    #[test]
    fn mutable_walk_and_clear_preserve_length() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);
        for page in [2usize, 66, 130] {
            pages.insert(page * PAGE_SIZE, page);
        }

        pages.for_each_range_mut(64 * PAGE_SIZE..131 * PAGE_SIZE, |_address, value| {
            *value += 1000;
        });
        pages.for_each_mut(|_address, value| *value += 1);

        assert_eq!(pages.get(2 * PAGE_SIZE), Some(&3));
        assert_eq!(pages.get(66 * PAGE_SIZE), Some(&1067));
        assert_eq!(pages.get(130 * PAGE_SIZE), Some(&1131));
        assert_eq!(pages.len(), 3);

        pages.clear();
        assert!(pages.is_empty());
        assert_eq!(pages.len(), 0);
    }

    #[test]
    fn root_height_tracks_highest_live_page() {
        let mut pages = RadixPageMap::new(PAGE_SIZE);
        assert_eq!(pages.root_level, 0);

        pages.insert(2 * PAGE_SIZE, 2);
        pages.insert(0x7fff_ffff_f000, 3);
        assert!(pages.root_level > 0);

        assert_eq!(pages.remove(0x7fff_ffff_f000), Some(3));
        assert_eq!(pages.root_level, 0);
        assert_eq!(pages.get(2 * PAGE_SIZE), Some(&2));
    }
}
