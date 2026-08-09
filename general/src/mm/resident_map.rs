use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ops::Range;

const RADIX_BITS: usize = 6;
const RADIX_SLOTS: usize = 1 << RADIX_BITS;
const RADIX_MASK: usize = RADIX_SLOTS - 1;

enum RadixNode<T> {
    Branch([Option<Box<RadixNode<T>>>; RADIX_SLOTS]),
    Leaf([Option<T>; RADIX_SLOTS]),
}

impl<T> RadixNode<T> {
    fn new(level: usize) -> Self {
        if level == 0 {
            Self::Leaf(core::array::from_fn(|_| None))
        } else {
            Self::Branch(core::array::from_fn(|_| None))
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Branch(children) => children.iter().all(Option::is_none),
            Self::Leaf(entries) => entries.iter().all(Option::is_none),
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
            entries[page_index & RADIX_MASK].as_ref()
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
            entries[page_index & RADIX_MASK].as_mut()
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
            entries[page_index & RADIX_MASK].replace(value)
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
            entries[page_index & RADIX_MASK].take()
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
            for (slot, entry) in entries.iter_mut().enumerate() {
                if let Some(value) = entry.as_mut() {
                    visit((prefix | slot) << page_shift, value);
                }
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
            for (slot, entry) in entries.iter().enumerate() {
                let page_index = prefix | slot;
                if page_index >= range.end {
                    break;
                }
                if page_index < range.start {
                    continue;
                }
                if let Some(value) = entry.as_ref() {
                    visit(page_index << page_shift, value);
                }
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
            for (slot, entry) in entries.iter_mut().enumerate() {
                let page_index = prefix | slot;
                if page_index >= range.end {
                    break;
                }
                if page_index < range.start {
                    continue;
                }
                if let Some(value) = entry.as_mut() {
                    visit(page_index << page_shift, value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::RadixPageMap;

    const PAGE_SIZE: usize = 4096;

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
