//! ELM 所有者范围索引。
//!
//! tracked small/large 对象同时保留精确 registry 记录和按 owner 排序的范围节点。
//! 范围树只服务非零 owner，普通内核分配不会触碰这里的锁或节点。

use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::Mutex;
use crate::registry::AllocationOwnerStats;
use crate::request::{AllocationKind, AllocationRecord};

const OWNER_SHARDS: usize = 64;
const OWNER_BUCKETS_PER_SHARD: usize = 64;
const OWNER_BUCKETS: usize = OWNER_SHARDS * OWNER_BUCKETS_PER_SHARD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerIndexError {
    NotInitialized,
    InvalidOwner,
    InvalidRange,
    UnknownOwner,
    UnknownRange,
    Overlap,
    MetadataOutOfMemory,
    Corrupt,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerIndexAuditFlags(u32);

impl OwnerIndexAuditFlags {
    pub const UNINITIALIZED: Self = Self(1 << 0);
    pub const DIRECTORY_CORRUPTION: Self = Self(1 << 1);
    pub const TREE_CORRUPTION: Self = Self(1 << 2);
    pub const STATS_MISMATCH: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerIndexAudit {
    pub flags: OwnerIndexAuditFlags,
    pub owners: usize,
    pub ranges: usize,
    pub scanned_nodes: usize,
}

impl OwnerIndexAudit {
    pub const fn is_consistent(self) -> bool {
        self.flags.is_empty()
    }
}

#[derive(Clone, Copy)]
struct OwnerDirectoryEntry {
    owner: u64,
    root: usize,
    non_range_head: usize,
    next_bucket: usize,
    free_next: usize,
    stats: AllocationOwnerStats,
}

impl OwnerDirectoryEntry {
    const fn empty() -> Self {
        Self {
            owner: 0,
            root: 0,
            non_range_head: 0,
            next_bucket: 0,
            free_next: 0,
            stats: AllocationOwnerStats {
                records: 0,
                requested_bytes: 0,
                usable_bytes: 0,
                boot_records: 0,
                small_records: 0,
                large_records: 0,
                physical_records: 0,
                largest_requested_bytes: 0,
                largest_usable_bytes: 0,
                scan_errors: 0,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct OwnerRangeNode {
    start: usize,
    end: usize,
    owner: u64,
    requested: usize,
    usable: usize,
    kind: AllocationKind,
    left: usize,
    right: usize,
    parent: usize,
    height: u8,
    next_non_range: usize,
    free_next: usize,
}

impl OwnerRangeNode {
    const fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            owner: 0,
            requested: 0,
            usable: 0,
            kind: AllocationKind::Small,
            left: 0,
            right: 0,
            parent: 0,
            height: 1,
            next_non_range: 0,
            free_next: 0,
        }
    }

    fn from_record(record: &AllocationRecord, end: usize) -> Self {
        Self {
            start: record.ptr,
            end,
            owner: record.accounting_owner(),
            requested: record.size,
            usable: record.usable_size.max(record.size),
            kind: record.kind,
            ..Self::empty()
        }
    }
}

struct OwnerShardInner {
    free_entries: usize,
    free_nodes: usize,
    entries_allocated: usize,
    nodes_allocated: usize,
}

impl OwnerShardInner {
    const fn new() -> Self {
        Self {
            free_entries: 0,
            free_nodes: 0,
            entries_allocated: 0,
            nodes_allocated: 0,
        }
    }
}

struct OwnerShard {
    inner: Mutex<OwnerShardInner>,
}

impl OwnerShard {
    const fn new() -> Self {
        Self {
            inner: Mutex::new(OwnerShardInner::new()),
        }
    }
}

pub struct OwnerAllocationIndex {
    buckets: AtomicUsize,
    initialized: AtomicBool,
    shards: [OwnerShard; OWNER_SHARDS],
}

impl OwnerAllocationIndex {
    pub const fn new() -> Self {
        Self {
            buckets: AtomicUsize::new(0),
            initialized: AtomicBool::new(false),
            shards: [const { OwnerShard::new() }; OWNER_SHARDS],
        }
    }

    pub fn init(&self) -> bool {
        if self.initialized.load(Ordering::Acquire) {
            return true;
        }
        let layout = match Layout::array::<usize>(OWNER_BUCKETS) {
            Ok(layout) => layout,
            Err(_) => return false,
        };
        let buckets = crate::alloc_internal_metadata(layout) as *mut usize;
        if buckets.is_null() {
            return false;
        }
        for index in 0..OWNER_BUCKETS {
            unsafe {
                // Safety: 元数据分配器返回了可写的 OWNER_BUCKETS 个 usize 数组。
                buckets.add(index).write(0);
            }
        }
        match self.buckets.compare_exchange(
            0,
            buckets as usize,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.initialized.store(true, Ordering::Release);
                true
            }
            Err(existing) => {
                self.buckets.store(existing, Ordering::Release);
                self.initialized.store(true, Ordering::Release);
                true
            }
        }
    }

    #[inline]
    fn shard_for(&self, owner: u64) -> usize {
        ((owner ^ (owner >> 33) ^ (owner >> 17)) as usize) & (OWNER_SHARDS - 1)
    }

    #[inline]
    fn bucket_for(&self, owner: u64) -> usize {
        let shard = self.shard_for(owner);
        let local_bucket =
            ((owner ^ (owner >> 29) ^ (owner >> 13)) as usize) & (OWNER_BUCKETS_PER_SHARD - 1);
        shard * OWNER_BUCKETS_PER_SHARD + local_bucket
    }

    fn bucket_head(&self, bucket: usize) -> usize {
        let base = self.buckets.load(Ordering::Acquire) as *mut usize;
        if base.is_null() {
            0
        } else {
            unsafe {
                // Safety: init 发布了固定大小为 OWNER_BUCKETS 的元数据数组。
                base.add(bucket).read()
            }
        }
    }

    fn set_bucket_head(&self, bucket: usize, value: usize) {
        let base = self.buckets.load(Ordering::Acquire) as *mut usize;
        unsafe {
            // Safety: 调用方只会传入 bucket_for 生成的桶下标。
            base.add(bucket).write(value);
        }
    }

    fn find_entry_locked(
        &self,
        inner: &OwnerShardInner,
        bucket: usize,
        owner: u64,
    ) -> Result<(usize, usize), OwnerIndexError> {
        let mut previous = 0;
        let mut current = self.bucket_head(bucket);
        let mut visited = 0;
        while current != 0 {
            if visited >= inner.entries_allocated {
                return Err(OwnerIndexError::Corrupt);
            }
            let entry = read_entry(current);
            if entry.owner == owner {
                return Ok((current, previous));
            }
            previous = current;
            current = entry.next_bucket;
            visited += 1;
        }
        Ok((0, previous))
    }

    fn alloc_entry_locked(&self, inner: &mut OwnerShardInner) -> Option<usize> {
        if inner.free_entries != 0 {
            let address = inner.free_entries;
            inner.free_entries = read_entry(address).free_next;
            return Some(address);
        }
        let address = crate::alloc_internal_metadata(Layout::new::<OwnerDirectoryEntry>()) as usize;
        if address == 0 {
            return None;
        }
        inner.entries_allocated = inner.entries_allocated.saturating_add(1);
        Some(address)
    }

    fn alloc_node_locked(&self, inner: &mut OwnerShardInner) -> Option<usize> {
        if inner.free_nodes != 0 {
            let address = inner.free_nodes;
            inner.free_nodes = read_node(address).free_next;
            return Some(address);
        }
        let address = crate::alloc_internal_metadata(Layout::new::<OwnerRangeNode>()) as usize;
        if address == 0 {
            return None;
        }
        inner.nodes_allocated = inner.nodes_allocated.saturating_add(1);
        Some(address)
    }

    fn recycle_entry_locked(&self, inner: &mut OwnerShardInner, address: usize) {
        let mut entry = OwnerDirectoryEntry::empty();
        entry.free_next = inner.free_entries;
        write_entry(address, entry);
        inner.free_entries = address;
    }

    fn recycle_node_locked(&self, inner: &mut OwnerShardInner, address: usize) {
        let mut node = OwnerRangeNode::empty();
        node.free_next = inner.free_nodes;
        write_node(address, node);
        inner.free_nodes = address;
    }

    pub fn track(&self, record: &AllocationRecord) -> Result<(), OwnerIndexError> {
        let owner = record.accounting_owner();
        if owner == 0 {
            return Ok(());
        }
        if !self.initialized.load(Ordering::Acquire) {
            return Err(OwnerIndexError::NotInitialized);
        }
        let shard_index = self.shard_for(owner);
        let bucket = self.bucket_for(owner);
        let mut inner = self.shards[shard_index].inner.lock();
        let (mut entry_address, _) = self.find_entry_locked(&inner, bucket, owner)?;
        let mut new_entry = false;
        if entry_address == 0 {
            entry_address = self
                .alloc_entry_locked(&mut inner)
                .ok_or(OwnerIndexError::MetadataOutOfMemory)?;
            let entry = OwnerDirectoryEntry {
                owner,
                next_bucket: self.bucket_head(bucket),
                ..OwnerDirectoryEntry::empty()
            };
            write_entry(entry_address, entry);
            self.set_bucket_head(bucket, entry_address);
            new_entry = true;
        }
        let mut entry = read_entry(entry_address);
        let usable = record.usable_size.max(record.size);
        let Some(end) = record.ptr.checked_add(usable) else {
            if new_entry {
                self.set_bucket_head(bucket, entry.next_bucket);
                self.recycle_entry_locked(&mut inner, entry_address);
            }
            return Err(OwnerIndexError::InvalidRange);
        };
        if !matches!(record.kind, AllocationKind::Small | AllocationKind::Large)
            || record.domain != crate::request::MemoryDomain::Kernel
        {
            let node_address = self.alloc_node_locked(&mut inner).ok_or_else(|| {
                if new_entry {
                    self.set_bucket_head(bucket, entry.next_bucket);
                    self.recycle_entry_locked(&mut inner, entry_address);
                }
                OwnerIndexError::MetadataOutOfMemory
            })?;
            let mut node = OwnerRangeNode::from_record(record, end);
            node.next_non_range = entry.non_range_head;
            write_node(node_address, node);
            entry.non_range_head = node_address;
            add_stats(&mut entry.stats, record);
            write_entry(entry_address, entry);
            return Ok(());
        }
        if has_overlap(entry.root, record.ptr, end) {
            if new_entry {
                self.set_bucket_head(bucket, entry.next_bucket);
                self.recycle_entry_locked(&mut inner, entry_address);
            }
            return Err(OwnerIndexError::Overlap);
        }
        let node_address = self.alloc_node_locked(&mut inner).ok_or_else(|| {
            if new_entry {
                self.set_bucket_head(bucket, entry.next_bucket);
                self.recycle_entry_locked(&mut inner, entry_address);
            }
            OwnerIndexError::MetadataOutOfMemory
        })?;
        write_node(node_address, OwnerRangeNode::from_record(record, end));
        entry.root = insert_node(entry.root, node_address);
        add_stats(&mut entry.stats, record);
        write_entry(entry_address, entry);
        Ok(())
    }

    pub fn untrack(&self, record: &AllocationRecord) -> Result<(), OwnerIndexError> {
        let owner = record.accounting_owner();
        if owner == 0 {
            return Ok(());
        }
        if !self.initialized.load(Ordering::Acquire) {
            return Err(OwnerIndexError::NotInitialized);
        }
        let shard_index = self.shard_for(owner);
        let bucket = self.bucket_for(owner);
        let mut inner = self.shards[shard_index].inner.lock();
        let (entry_address, previous) = self.find_entry_locked(&inner, bucket, owner)?;
        if entry_address == 0 {
            return Err(OwnerIndexError::UnknownOwner);
        }
        let mut entry = read_entry(entry_address);
        if matches!(record.kind, AllocationKind::Small | AllocationKind::Large)
            && record.domain == crate::request::MemoryDomain::Kernel
        {
            let Some(node_address) = find_node(entry.root, record.ptr) else {
                return Err(OwnerIndexError::UnknownRange);
            };
            let node = read_node(node_address);
            if node.owner != owner {
                return Err(OwnerIndexError::Corrupt);
            }
            let (root, removed) = remove_node(entry.root, record.ptr);
            if removed == 0 {
                return Err(OwnerIndexError::UnknownRange);
            }
            entry.root = root;
            sub_stats(&mut entry.stats, node.requested, node.usable, node.kind);
            self.recycle_node_locked(&mut inner, removed);
            if node.requested >= entry.stats.largest_requested_bytes
                || node.usable >= entry.stats.largest_usable_bytes
            {
                if !recompute_largest(&mut entry, inner.nodes_allocated) {
                    return Err(OwnerIndexError::Corrupt);
                }
            }
        } else {
            let mut previous = 0;
            let mut current = entry.non_range_head;
            let mut visited = 0;
            while current != 0 {
                if visited >= inner.nodes_allocated {
                    return Err(OwnerIndexError::Corrupt);
                }
                let node = read_node(current);
                if node.start == record.ptr {
                    if node.owner != owner || node.kind != record.kind {
                        return Err(OwnerIndexError::Corrupt);
                    }
                    if previous == 0 {
                        entry.non_range_head = node.next_non_range;
                    } else {
                        let mut previous_node = read_node(previous);
                        previous_node.next_non_range = node.next_non_range;
                        write_node(previous, previous_node);
                    }
                    sub_stats(&mut entry.stats, node.requested, node.usable, node.kind);
                    self.recycle_node_locked(&mut inner, current);
                    if node.requested >= entry.stats.largest_requested_bytes
                        || node.usable >= entry.stats.largest_usable_bytes
                    {
                        if !recompute_largest(&mut entry, inner.nodes_allocated) {
                            return Err(OwnerIndexError::Corrupt);
                        }
                    }
                    break;
                }
                previous = current;
                current = node.next_non_range;
                visited += 1;
            }
            if current == 0 {
                return Err(OwnerIndexError::UnknownRange);
            }
        }
        write_entry(entry_address, entry);
        if entry.stats.records == 0 {
            if previous == 0 {
                self.set_bucket_head(bucket, entry.next_bucket);
            } else {
                let mut previous_entry = read_entry(previous);
                previous_entry.next_bucket = entry.next_bucket;
                write_entry(previous, previous_entry);
            }
            self.recycle_entry_locked(&mut inner, entry_address);
        }
        Ok(())
    }

    pub fn update(
        &self,
        old: &AllocationRecord,
        new: &AllocationRecord,
    ) -> Result<(), OwnerIndexError> {
        let owner = old.accounting_owner();
        if owner == 0 || new.accounting_owner() == 0 {
            return Ok(());
        }
        if owner != new.accounting_owner() {
            return Err(OwnerIndexError::InvalidOwner);
        }
        if old.ptr != new.ptr {
            return Err(OwnerIndexError::InvalidRange);
        }
        if !matches!(old.kind, AllocationKind::Small | AllocationKind::Large)
            || !matches!(new.kind, AllocationKind::Small | AllocationKind::Large)
        {
            return Ok(());
        }
        let Some(end) = new.ptr.checked_add(new.usable_size.max(new.size)) else {
            return Err(OwnerIndexError::InvalidRange);
        };
        let shard_index = self.shard_for(owner);
        let bucket = self.bucket_for(owner);
        let inner = self.shards[shard_index].inner.lock();
        let (entry_address, _) = self.find_entry_locked(&inner, bucket, owner)?;
        if entry_address == 0 {
            return Err(OwnerIndexError::UnknownOwner);
        }
        let mut entry = read_entry(entry_address);
        let Some(node_address) = find_node(entry.root, old.ptr) else {
            return Err(OwnerIndexError::UnknownRange);
        };
        if has_overlap_except(entry.root, old.ptr, new.ptr, end) {
            return Err(OwnerIndexError::Overlap);
        }
        let mut node = read_node(node_address);
        let largest_invalidated = node.requested == entry.stats.largest_requested_bytes
            || node.usable == entry.stats.largest_usable_bytes;
        sub_stats(&mut entry.stats, node.requested, node.usable, node.kind);
        node.start = new.ptr;
        node.end = end;
        node.requested = new.size;
        node.usable = new.usable_size.max(new.size);
        node.kind = new.kind;
        add_stats(&mut entry.stats, new);
        write_node(node_address, node);
        if largest_invalidated && !recompute_largest(&mut entry, inner.nodes_allocated) {
            return Err(OwnerIndexError::Corrupt);
        }
        write_entry(entry_address, entry);
        Ok(())
    }

    pub fn contains(&self, owner: u64, address: usize, len: usize) -> bool {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::AllocOwnerRangeLookup);
        if owner == 0 || address == 0 || len == 0 {
            return false;
        }
        let Some(end) = address.checked_add(len) else {
            return false;
        };
        if !self.initialized.load(Ordering::Acquire) {
            return false;
        }
        let shard_index = self.shard_for(owner);
        let bucket = self.bucket_for(owner);
        let inner = self.shards[shard_index].inner.lock();
        let Ok((entry_address, _)) = self.find_entry_locked(&inner, bucket, owner) else {
            return false;
        };
        if entry_address == 0 {
            return false;
        }
        let entry = read_entry(entry_address);
        let Some(node_address) = predecessor(entry.root, address) else {
            return false;
        };
        let node = read_node(node_address);
        node.owner == owner && node.start <= address && end <= node.end
    }

    pub fn stats(&self, owner: u64) -> AllocationOwnerStats {
        if owner == 0 || !self.initialized.load(Ordering::Acquire) {
            return AllocationOwnerStats::default();
        }
        let bucket = self.bucket_for(owner);
        let shard_index = self.shard_for(owner);
        let inner = self.shards[shard_index].inner.lock();
        let Ok((entry_address, _)) = self.find_entry_locked(&inner, bucket, owner) else {
            return AllocationOwnerStats::default();
        };
        if entry_address == 0 {
            AllocationOwnerStats::default()
        } else {
            read_entry(entry_address).stats
        }
    }

    pub fn audit(&self) -> OwnerIndexAudit {
        if !self.initialized.load(Ordering::Acquire) {
            return OwnerIndexAudit {
                flags: OwnerIndexAuditFlags::UNINITIALIZED,
                ..OwnerIndexAudit::default()
            };
        }
        let mut audit = OwnerIndexAudit::default();
        for shard_index in 0..OWNER_SHARDS {
            let shard = &self.shards[shard_index];
            let inner = shard.inner.lock();
            let bucket_start = shard_index * OWNER_BUCKETS_PER_SHARD;
            let bucket_end = bucket_start + OWNER_BUCKETS_PER_SHARD;
            for bucket in bucket_start..bucket_end {
                let mut current = self.bucket_head(bucket);
                let mut visited = 0;
                while current != 0 {
                    if visited >= inner.entries_allocated {
                        audit
                            .flags
                            .insert(OwnerIndexAuditFlags::DIRECTORY_CORRUPTION);
                        break;
                    }
                    visited += 1;
                    let entry = read_entry(current);
                    if entry.owner == 0
                        || self.shard_for(entry.owner) != shard_index
                        || self.bucket_for(entry.owner) != bucket
                    {
                        audit
                            .flags
                            .insert(OwnerIndexAuditFlags::DIRECTORY_CORRUPTION);
                    }
                    audit.owners += 1;
                    let mut range_nodes = 0;
                    let mut non_range_nodes = 0;
                    let mut seen_nodes = 0;
                    let mut expected_stats = AllocationOwnerStats::default();
                    if audit_tree(
                        entry.root,
                        entry.owner,
                        0,
                        None,
                        None,
                        &mut expected_stats,
                        &mut range_nodes,
                        &mut seen_nodes,
                        inner.nodes_allocated,
                    )
                    .is_none()
                    {
                        audit.flags.insert(OwnerIndexAuditFlags::TREE_CORRUPTION);
                    }
                    let mut non_range = entry.non_range_head;
                    let mut non_range_visited = 0;
                    while non_range != 0 {
                        if non_range_visited >= inner.nodes_allocated {
                            audit.flags.insert(OwnerIndexAuditFlags::TREE_CORRUPTION);
                            break;
                        }
                        let node = read_node(non_range);
                        if node.owner != entry.owner
                            || node.left != 0
                            || node.right != 0
                            || node.parent != 0
                        {
                            audit.flags.insert(OwnerIndexAuditFlags::TREE_CORRUPTION);
                        }
                        add_node_stats(&mut expected_stats, node);
                        non_range_nodes += 1;
                        seen_nodes += 1;
                        non_range = node.next_non_range;
                        non_range_visited += 1;
                    }
                    if expected_stats != entry.stats {
                        audit.flags.insert(OwnerIndexAuditFlags::STATS_MISMATCH);
                    }
                    audit.ranges += range_nodes;
                    audit.scanned_nodes += range_nodes + non_range_nodes;
                    current = entry.next_bucket;
                }
            }
        }
        audit
    }
}

impl Default for OwnerAllocationIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn add_stats(stats: &mut AllocationOwnerStats, record: &AllocationRecord) {
    let usable = record.usable_size.max(record.size);
    stats.records = stats.records.saturating_add(1);
    stats.requested_bytes = stats.requested_bytes.saturating_add(record.size);
    stats.usable_bytes = stats.usable_bytes.saturating_add(usable);
    stats.largest_requested_bytes = stats.largest_requested_bytes.max(record.size);
    stats.largest_usable_bytes = stats.largest_usable_bytes.max(usable);
    match record.kind {
        AllocationKind::Boot => stats.boot_records = stats.boot_records.saturating_add(1),
        AllocationKind::Small => stats.small_records = stats.small_records.saturating_add(1),
        AllocationKind::Large => stats.large_records = stats.large_records.saturating_add(1),
        AllocationKind::Physical => {
            stats.physical_records = stats.physical_records.saturating_add(1)
        }
    }
}

fn sub_stats(
    stats: &mut AllocationOwnerStats,
    requested: usize,
    usable: usize,
    kind: AllocationKind,
) {
    stats.records = stats.records.saturating_sub(1);
    stats.requested_bytes = stats.requested_bytes.saturating_sub(requested);
    stats.usable_bytes = stats.usable_bytes.saturating_sub(usable);
    match kind {
        AllocationKind::Boot => stats.boot_records = stats.boot_records.saturating_sub(1),
        AllocationKind::Small => stats.small_records = stats.small_records.saturating_sub(1),
        AllocationKind::Large => stats.large_records = stats.large_records.saturating_sub(1),
        AllocationKind::Physical => {
            stats.physical_records = stats.physical_records.saturating_sub(1)
        }
    }
    if stats.records == 0 {
        stats.largest_requested_bytes = 0;
        stats.largest_usable_bytes = 0;
    }
}

fn recompute_largest(entry: &mut OwnerDirectoryEntry, node_limit: usize) -> bool {
    let mut largest_requested = 0;
    let mut largest_usable = 0;
    let mut scanned = 0;
    if !scan_tree_largest(
        entry.root,
        &mut largest_requested,
        &mut largest_usable,
        &mut scanned,
        node_limit,
    ) {
        return false;
    }
    let mut current = entry.non_range_head;
    while current != 0 {
        if scanned >= node_limit {
            return false;
        }
        let node = read_node(current);
        largest_requested = largest_requested.max(node.requested);
        largest_usable = largest_usable.max(node.usable);
        current = node.next_non_range;
        scanned += 1;
    }
    entry.stats.largest_requested_bytes = largest_requested;
    entry.stats.largest_usable_bytes = largest_usable;
    true
}

fn scan_tree_largest(
    root: usize,
    largest_requested: &mut usize,
    largest_usable: &mut usize,
    scanned: &mut usize,
    node_limit: usize,
) -> bool {
    if root == 0 {
        return true;
    }
    if *scanned >= node_limit {
        return false;
    }
    let node = read_node(root);
    if !scan_tree_largest(
        node.left,
        largest_requested,
        largest_usable,
        scanned,
        node_limit,
    ) {
        return false;
    }
    *largest_requested = (*largest_requested).max(node.requested);
    *largest_usable = (*largest_usable).max(node.usable);
    *scanned += 1;
    scan_tree_largest(
        node.right,
        largest_requested,
        largest_usable,
        scanned,
        node_limit,
    )
}

fn read_entry(address: usize) -> OwnerDirectoryEntry {
    unsafe {
        // Safety: 地址来自元数据分配器，并在分配器生命周期内保持有效。
        (address as *const OwnerDirectoryEntry).read()
    }
}

fn write_entry(address: usize, entry: OwnerDirectoryEntry) {
    unsafe {
        // Safety: 地址来自元数据分配器，并在分配器生命周期内保持有效。
        (address as *mut OwnerDirectoryEntry).write(entry);
    }
}

fn read_node(address: usize) -> OwnerRangeNode {
    unsafe {
        // Safety: 地址来自元数据分配器，并在分配器生命周期内保持有效。
        (address as *const OwnerRangeNode).read()
    }
}

fn write_node(address: usize, node: OwnerRangeNode) {
    unsafe {
        // Safety: 地址来自元数据分配器，并在分配器生命周期内保持有效。
        (address as *mut OwnerRangeNode).write(node);
    }
}

fn node_height(address: usize) -> i16 {
    if address == 0 {
        0
    } else {
        read_node(address).height as i16
    }
}

fn refresh_height(address: usize) {
    let mut node = read_node(address);
    node.height = (1 + node_height(node.left).max(node_height(node.right))) as u8;
    write_node(address, node);
}

fn balance_factor(address: usize) -> i16 {
    let node = read_node(address);
    node_height(node.left) - node_height(node.right)
}

fn rotate_left(address: usize) -> usize {
    let mut root = read_node(address);
    let child_address = root.right;
    let mut child = read_node(child_address);
    let parent = root.parent;
    root.right = child.left;
    if root.right != 0 {
        let mut beta = read_node(root.right);
        beta.parent = address;
        write_node(root.right, beta);
    }
    child.left = address;
    child.parent = parent;
    root.parent = child_address;
    write_node(address, root);
    write_node(child_address, child);
    refresh_height(address);
    refresh_height(child_address);
    child_address
}

fn rotate_right(address: usize) -> usize {
    let mut root = read_node(address);
    let child_address = root.left;
    let mut child = read_node(child_address);
    let parent = root.parent;
    root.left = child.right;
    if root.left != 0 {
        let mut beta = read_node(root.left);
        beta.parent = address;
        write_node(root.left, beta);
    }
    child.right = address;
    child.parent = parent;
    root.parent = child_address;
    write_node(address, root);
    write_node(child_address, child);
    refresh_height(address);
    refresh_height(child_address);
    child_address
}

fn rebalance(address: usize) -> usize {
    refresh_height(address);
    let balance = balance_factor(address);
    if balance > 1 {
        let left = read_node(address).left;
        if balance_factor(left) < 0 {
            let new_left = rotate_left(left);
            let mut node = read_node(address);
            node.left = new_left;
            write_node(address, node);
        }
        return rotate_right(address);
    }
    if balance < -1 {
        let right = read_node(address).right;
        if balance_factor(right) > 0 {
            let new_right = rotate_right(right);
            let mut node = read_node(address);
            node.right = new_right;
            write_node(address, node);
        }
        return rotate_left(address);
    }
    address
}

fn insert_node(root: usize, node_address: usize) -> usize {
    if root == 0 {
        return node_address;
    }
    let node = read_node(node_address);
    let mut current = read_node(root);
    if node.start < current.start {
        current.left = insert_node(current.left, node_address);
        let mut child = read_node(current.left);
        child.parent = root;
        write_node(current.left, child);
    } else {
        current.right = insert_node(current.right, node_address);
        let mut child = read_node(current.right);
        child.parent = root;
        write_node(current.right, child);
    }
    write_node(root, current);
    rebalance(root)
}

fn find_node(root: usize, start: usize) -> Option<usize> {
    let mut current = root;
    while current != 0 {
        let node = read_node(current);
        if start == node.start {
            return Some(current);
        }
        current = if start < node.start {
            node.left
        } else {
            node.right
        };
    }
    None
}

fn minimum_node(mut address: usize) -> usize {
    while address != 0 {
        let left = read_node(address).left;
        if left == 0 {
            return address;
        }
        address = left;
    }
    0
}

fn remove_node(root: usize, start: usize) -> (usize, usize) {
    if root == 0 {
        return (0, 0);
    }
    let mut node = read_node(root);
    if start < node.start {
        let (left, removed) = remove_node(node.left, start);
        if removed == 0 {
            return (root, 0);
        }
        node.left = left;
        if left != 0 {
            let mut child = read_node(left);
            child.parent = root;
            write_node(left, child);
        }
        write_node(root, node);
        return (rebalance(root), removed);
    }
    if start > node.start {
        let (right, removed) = remove_node(node.right, start);
        if removed == 0 {
            return (root, 0);
        }
        node.right = right;
        if right != 0 {
            let mut child = read_node(right);
            child.parent = root;
            write_node(right, child);
        }
        write_node(root, node);
        return (rebalance(root), removed);
    }
    if node.left == 0 {
        if node.right != 0 {
            let mut child = read_node(node.right);
            child.parent = node.parent;
            write_node(node.right, child);
        }
        return (node.right, root);
    }
    if node.right == 0 {
        let mut child = read_node(node.left);
        child.parent = node.parent;
        write_node(node.left, child);
        return (node.left, root);
    }
    let successor = minimum_node(node.right);
    let successor_node = read_node(successor);
    node.start = successor_node.start;
    node.end = successor_node.end;
    node.owner = successor_node.owner;
    node.requested = successor_node.requested;
    node.usable = successor_node.usable;
    node.kind = successor_node.kind;
    let (right, removed) = remove_node(node.right, successor_node.start);
    node.right = right;
    write_node(root, node);
    if right != 0 {
        let mut child = read_node(right);
        child.parent = root;
        write_node(right, child);
    }
    (rebalance(root), removed)
}

fn predecessor(root: usize, address: usize) -> Option<usize> {
    let mut current = root;
    let mut result = 0;
    while current != 0 {
        let node = read_node(current);
        if node.start <= address {
            result = current;
            current = node.right;
        } else {
            current = node.left;
        }
    }
    (result != 0).then_some(result)
}

fn has_overlap(root: usize, start: usize, end: usize) -> bool {
    if let Some(previous) = predecessor(root, start) {
        if read_node(previous).end > start {
            return true;
        }
    }
    let mut current = root;
    let mut successor = 0;
    while current != 0 {
        let node = read_node(current);
        if node.start >= start {
            successor = current;
            current = node.left;
        } else {
            current = node.right;
        }
    }
    successor != 0 && read_node(successor).start < end
}

fn has_overlap_except(root: usize, old_start: usize, start: usize, end: usize) -> bool {
    if let Some(previous) = predecessor(root, start) {
        let node = read_node(previous);
        if node.start != old_start && node.end > start {
            return true;
        }
    }
    let mut current = root;
    let mut successor = 0;
    while current != 0 {
        let node = read_node(current);
        if node.start >= start {
            successor = current;
            current = node.left;
        } else {
            current = node.right;
        }
    }
    successor != 0 && read_node(successor).start != old_start && read_node(successor).start < end
}

fn add_node_stats(stats: &mut AllocationOwnerStats, node: OwnerRangeNode) {
    stats.records = stats.records.saturating_add(1);
    stats.requested_bytes = stats.requested_bytes.saturating_add(node.requested);
    stats.usable_bytes = stats.usable_bytes.saturating_add(node.usable);
    stats.largest_requested_bytes = stats.largest_requested_bytes.max(node.requested);
    stats.largest_usable_bytes = stats.largest_usable_bytes.max(node.usable);
    match node.kind {
        AllocationKind::Boot => stats.boot_records = stats.boot_records.saturating_add(1),
        AllocationKind::Small => stats.small_records = stats.small_records.saturating_add(1),
        AllocationKind::Large => stats.large_records = stats.large_records.saturating_add(1),
        AllocationKind::Physical => {
            stats.physical_records = stats.physical_records.saturating_add(1)
        }
    }
}

fn audit_tree(
    root: usize,
    owner: u64,
    expected_parent: usize,
    lower_bound: Option<usize>,
    upper_bound: Option<usize>,
    stats: &mut AllocationOwnerStats,
    count: &mut usize,
    seen: &mut usize,
    node_limit: usize,
) -> Option<u8> {
    if root == 0 {
        return Some(0);
    }
    if *seen >= node_limit {
        return None;
    }
    *seen += 1;
    let node = read_node(root);
    if node.owner != owner
        || node.parent != expected_parent
        || node.start == 0
        || node.start >= node.end
        || lower_bound.is_some_and(|lower| node.start <= lower)
        || upper_bound.is_some_and(|upper| node.start >= upper)
    {
        return None;
    }
    let left_height = audit_tree(
        node.left,
        owner,
        root,
        lower_bound,
        Some(node.start),
        stats,
        count,
        seen,
        node_limit,
    )?;
    let right_height = audit_tree(
        node.right,
        owner,
        root,
        Some(node.start),
        upper_bound,
        stats,
        count,
        seen,
        node_limit,
    )?;
    let expected_height = 1 + left_height.max(right_height);
    if node.height != expected_height as u8
        || (i16::from(left_height) - i16::from(right_height)).unsigned_abs() > 1
    {
        return None;
    }
    add_node_stats(stats, node);
    *count = count.saturating_add(1);
    Some(expected_height as u8)
}
