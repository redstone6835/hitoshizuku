//! 内核大对象分配层。
//!
//! slab 擅长处理大量小对象，但对于页级对齐需求更高、尺寸更大的对象，继续使用 slab
//! 会带来明显的内部碎片和管理复杂度。这个模块专门负责那部分"大对象"分配请求。
//!
//! 它本身并不直接操作页表，也不自己实现物理页算法，而是组合两部分现有能力：
//!
//! - 通过 `KernelAddressSpace` 预留一段虚拟地址；
//! - 通过 `BuddyAllocator` 获得对应的物理页块；
//! - 再由架构层回调把二者映射在一起。
//!
//! 因而 `KernelHeap` 更像是一个策略层：它决定何时走大对象路径、如何统计使用情况、
//! 如何在 free/realloc 时把请求反向拆回 address space 与 buddy 两层。
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::Mutex;

use crate::buddy::{BuddyAllocator, MAX_TRACKED_ORDER, PAGE_SIZE};
use crate::error::{AllocationError, DeallocationError};
use crate::request::PagePolicy;
use crate::request::{AllocationArena, AllocationKind, AllocationRecord, MemoryDomain};
use crate::space::{ArenaKind, BackedRange, KernelAddressSpace};

#[derive(Clone, Copy, Debug, Default)]
pub struct KernelHeapStats {
    pub alloc_requests: u64,
    pub free_requests: u64,
    pub realloc_requests: u64,
    pub active_allocs: u64,
    pub active_bytes: usize,
    pub active_pages: usize,
    pub alloc_failures: u64,
    pub address_reservation_failures: u64,
    pub invalid_frees: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_inserts: u64,
    pub cache_full_releases: u64,
    pub cache_pressure_flushes: u64,
    pub cache_pressure_releases: u64,
    pub cache_maintenance_flushes: u64,
    pub cache_maintenance_releases: u64,
    pub cache_release_failures: u64,
    pub cached_ranges: usize,
    pub cached_pages: usize,
    pub cached_bytes: usize,
}

impl KernelHeapStats {
    pub(crate) fn merge(&mut self, other: Self) {
        self.alloc_requests = self.alloc_requests.saturating_add(other.alloc_requests);
        self.free_requests = self.free_requests.saturating_add(other.free_requests);
        self.realloc_requests = self.realloc_requests.saturating_add(other.realloc_requests);
        self.active_allocs = self.active_allocs.saturating_add(other.active_allocs);
        self.active_bytes = self.active_bytes.saturating_add(other.active_bytes);
        self.active_pages = self.active_pages.saturating_add(other.active_pages);
        self.alloc_failures = self.alloc_failures.saturating_add(other.alloc_failures);
        self.address_reservation_failures = self
            .address_reservation_failures
            .saturating_add(other.address_reservation_failures);
        self.invalid_frees = self.invalid_frees.saturating_add(other.invalid_frees);
        self.cache_hits = self.cache_hits.saturating_add(other.cache_hits);
        self.cache_misses = self.cache_misses.saturating_add(other.cache_misses);
        self.cache_inserts = self.cache_inserts.saturating_add(other.cache_inserts);
        self.cache_full_releases = self
            .cache_full_releases
            .saturating_add(other.cache_full_releases);
        self.cache_pressure_flushes = self
            .cache_pressure_flushes
            .saturating_add(other.cache_pressure_flushes);
        self.cache_pressure_releases = self
            .cache_pressure_releases
            .saturating_add(other.cache_pressure_releases);
        self.cache_maintenance_flushes = self
            .cache_maintenance_flushes
            .saturating_add(other.cache_maintenance_flushes);
        self.cache_maintenance_releases = self
            .cache_maintenance_releases
            .saturating_add(other.cache_maintenance_releases);
        self.cache_release_failures = self
            .cache_release_failures
            .saturating_add(other.cache_release_failures);
        self.cached_ranges = self.cached_ranges.saturating_add(other.cached_ranges);
        self.cached_pages = self.cached_pages.saturating_add(other.cached_pages);
        self.cached_bytes = self.cached_bytes.saturating_add(other.cached_bytes);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelHeapReclaimStats {
    pub released_ranges: usize,
    pub released_pages: usize,
    pub released_bytes: usize,
}

impl KernelHeapReclaimStats {
    pub(crate) fn merge(&mut self, other: Self) {
        self.released_ranges = self.released_ranges.saturating_add(other.released_ranges);
        self.released_pages = self.released_pages.saturating_add(other.released_pages);
        self.released_bytes = self.released_bytes.saturating_add(other.released_bytes);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelHeapAudit {
    pub flags: KernelHeapAuditFlags,
    pub scanned_cached_ranges: usize,
    pub scanned_cached_pages: usize,
    pub scanned_cached_bytes: usize,
    pub scanned_active_allocs: u64,
    pub scanned_active_pages: usize,
    pub scanned_active_bytes: usize,
}

impl KernelHeapAudit {
    pub const fn is_consistent(self) -> bool {
        self.flags.is_empty()
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.flags.0 |= other.flags.0;
        self.scanned_cached_ranges = self
            .scanned_cached_ranges
            .saturating_add(other.scanned_cached_ranges);
        self.scanned_cached_pages = self
            .scanned_cached_pages
            .saturating_add(other.scanned_cached_pages);
        self.scanned_cached_bytes = self
            .scanned_cached_bytes
            .saturating_add(other.scanned_cached_bytes);
        self.scanned_active_allocs = self
            .scanned_active_allocs
            .saturating_add(other.scanned_active_allocs);
        self.scanned_active_pages = self
            .scanned_active_pages
            .saturating_add(other.scanned_active_pages);
        self.scanned_active_bytes = self
            .scanned_active_bytes
            .saturating_add(other.scanned_active_bytes);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelHeapAuditFlags(u32);

impl KernelHeapAuditFlags {
    pub const CACHE_RING_INVALID: Self = Self(1 << 0);
    pub const CACHE_RANGE_INVALID: Self = Self(1 << 1);
    pub const ACTIVE_ACCOUNTING_MISMATCH: Self = Self(1 << 2);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) != 0
    }

    fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }
}

// fork/exec 会高频创建 16 KiB 到 256 KiB 的 Vec、地址空间元数据和内核栈。
// 保留这些范围的虚拟映射可以避免每次释放都修改全局内核页表。高阶容量逐级收紧，
// 整个缓存填满时最多保留约 18 MiB 后备页。
const KHEAP_CACHE_MAX_ORDER: usize = 6;
const KHEAP_CACHE_ORDER_COUNT: usize = KHEAP_CACHE_MAX_ORDER + 1;
const KHEAP_CACHE_SLOT_COUNT: usize = 128;
const KHEAP_CACHE_CAPACITY_PER_ORDER: [usize; KHEAP_CACHE_ORDER_COUNT] =
    [128, 128, 128, 64, 64, 32, 16];
const KHEAP_CACHEABLE_BACKEND_COOKIE: usize = 1;

#[derive(Clone, Copy)]
struct CachedOrderRanges {
    ranges: [Option<BackedRange>; KHEAP_CACHE_SLOT_COUNT],
    head: usize,
    len: usize,
}

impl CachedOrderRanges {
    const fn new() -> Self {
        Self {
            ranges: [None; KHEAP_CACHE_SLOT_COUNT],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, range: BackedRange, capacity: usize) -> bool {
        if self.len == capacity {
            return false;
        }
        let tail = self.tail_index(capacity);
        self.ranges[tail] = Some(range);
        self.len += 1;
        true
    }

    fn pop(&mut self, capacity: usize) -> Option<BackedRange> {
        if self.len == 0 {
            return None;
        }
        let tail = self.newest_index(capacity);
        let range = self.ranges[tail].take();
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        range
    }

    fn push_or_evict_oldest(&mut self, range: BackedRange, capacity: usize) -> Option<BackedRange> {
        if self.push(range, capacity) {
            return None;
        }

        // cache 满时保留最新释放的 range。大对象路径最常见的模式是短时间内释放后再分配
        // 同阶对象；环形队列淘汰最旧元素并把当前 range 放到最新位置，避免满桶时搬移
        // 128 个槽位。
        let evicted = self.ranges[self.head].replace(range);
        self.head = self.next_index(self.head, capacity);
        evicted
    }

    fn tail_index(&self, capacity: usize) -> usize {
        (self.head + self.len) % capacity
    }

    fn newest_index(&self, capacity: usize) -> usize {
        (self.head + self.len - 1) % capacity
    }

    fn next_index(&self, index: usize, capacity: usize) -> usize {
        let next = index + 1;
        if next == capacity { 0 } else { next }
    }

    fn audit(&self, order: usize, arena: ArenaKind, audit: &mut KernelHeapAudit) {
        let capacity = cache_capacity_for_order(order);
        // kheap cache 是环形队列：active 窗口必须全部为合法 range，窗口外必须为空。
        // 这能发现 len/head 损坏、坏槽位残留，以及错误 order 的 range 被放入缓存。
        if capacity == 0
            || capacity > self.ranges.len()
            || self.len > capacity
            || self.head >= capacity
            || (self.len == 0 && self.head != 0)
        {
            audit.flags.insert(KernelHeapAuditFlags::CACHE_RING_INVALID);
        }

        let len = self.len.min(capacity);
        for slot in 0..self.ranges.len() {
            let offset = if slot >= capacity {
                capacity
            } else if slot >= self.head {
                slot - self.head
            } else {
                capacity - self.head + slot
            };
            let active_slot = slot < capacity && offset < len;
            match (active_slot, self.ranges[slot]) {
                (true, Some(range)) => {
                    if !is_expected_cached_range(range, order, arena) {
                        audit
                            .flags
                            .insert(KernelHeapAuditFlags::CACHE_RANGE_INVALID);
                        continue;
                    }
                    let pages = 1usize << range.order;
                    audit.scanned_cached_ranges += 1;
                    audit.scanned_cached_pages = audit.scanned_cached_pages.saturating_add(pages);
                    audit.scanned_cached_bytes =
                        audit.scanned_cached_bytes.saturating_add(range.size);
                }
                (true, None) | (false, Some(_)) => {
                    audit.flags.insert(KernelHeapAuditFlags::CACHE_RING_INVALID);
                }
                (false, None) => {}
            }
        }
    }
}

struct KernelHeapRangeCache {
    orders: [CachedOrderRanges; KHEAP_CACHE_ORDER_COUNT],
}

enum CacheFreeOutcome {
    Cached,
    ReleaseCurrent(BackedRange),
    ReleaseEvicted(BackedRange),
}

impl KernelHeapRangeCache {
    const fn new() -> Self {
        Self {
            orders: [CachedOrderRanges::new(); KHEAP_CACHE_ORDER_COUNT],
        }
    }

    fn push_or_evict_oldest(&mut self, range: BackedRange) -> Option<BackedRange> {
        let idx = cache_order_index(range.order)?;
        self.orders[idx].push_or_evict_oldest(range, cache_capacity_for_order(idx))
    }

    fn pop(&mut self, order: usize) -> Option<BackedRange> {
        let idx = cache_order_index(order)?;
        self.orders[idx].pop(cache_capacity_for_order(idx))
    }

    fn pop_any(&mut self) -> Option<BackedRange> {
        for idx in (0..self.orders.len()).rev() {
            if let Some(range) = self.orders[idx].pop(cache_capacity_for_order(idx)) {
                return Some(range);
            }
        }
        None
    }

    fn snapshot(&self) -> (usize, usize, usize) {
        let mut ranges = 0usize;
        let mut pages = 0usize;
        for order in 0..=KHEAP_CACHE_MAX_ORDER {
            let len = self.orders[order].len;
            ranges = ranges.saturating_add(len);
            pages = pages.saturating_add(len.saturating_mul(1usize << order));
        }
        (ranges, pages, pages.saturating_mul(PAGE_SIZE))
    }

    fn audit(&self, arena: ArenaKind) -> KernelHeapAudit {
        let mut audit = KernelHeapAudit::default();
        for order in 0..=KHEAP_CACHE_MAX_ORDER {
            self.orders[order].audit(order, arena, &mut audit);
        }
        audit
    }
}

pub struct KernelHeap {
    arena: ArenaKind,
    initialized: AtomicBool,
    alloc_requests: AtomicU64,
    free_requests: AtomicU64,
    realloc_requests: AtomicU64,
    active_allocs: AtomicU64,
    active_bytes: AtomicUsize,
    active_pages: AtomicUsize,
    alloc_failures: AtomicU64,
    address_reservation_failures: AtomicU64,
    invalid_frees: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    cache_inserts: AtomicU64,
    cache_full_releases: AtomicU64,
    cache_pressure_flushes: AtomicU64,
    cache_pressure_releases: AtomicU64,
    cache_maintenance_flushes: AtomicU64,
    cache_maintenance_releases: AtomicU64,
    cache_release_failures: AtomicU64,
    cache: Mutex<KernelHeapRangeCache>,
}

impl KernelHeap {
    pub const fn new(arena: ArenaKind) -> Self {
        Self {
            arena,
            initialized: AtomicBool::new(false),
            alloc_requests: AtomicU64::new(0),
            free_requests: AtomicU64::new(0),
            realloc_requests: AtomicU64::new(0),
            active_allocs: AtomicU64::new(0),
            active_bytes: AtomicUsize::new(0),
            active_pages: AtomicUsize::new(0),
            alloc_failures: AtomicU64::new(0),
            address_reservation_failures: AtomicU64::new(0),
            invalid_frees: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            cache_inserts: AtomicU64::new(0),
            cache_full_releases: AtomicU64::new(0),
            cache_pressure_flushes: AtomicU64::new(0),
            cache_pressure_releases: AtomicU64::new(0),
            cache_maintenance_flushes: AtomicU64::new(0),
            cache_maintenance_releases: AtomicU64::new(0),
            cache_release_failures: AtomicU64::new(0),
            cache: Mutex::new(KernelHeapRangeCache::new()),
        }
    }

    pub fn init(&self) {
        self.initialized.store(true, Ordering::Release);
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> KernelHeapStats {
        let (cached_ranges, cached_pages, cached_bytes) = self.cache.lock().snapshot();
        KernelHeapStats {
            alloc_requests: self.alloc_requests.load(Ordering::Acquire),
            free_requests: self.free_requests.load(Ordering::Acquire),
            realloc_requests: self.realloc_requests.load(Ordering::Acquire),
            active_allocs: self.active_allocs.load(Ordering::Acquire),
            active_bytes: self.active_bytes.load(Ordering::Acquire),
            active_pages: self.active_pages.load(Ordering::Acquire),
            alloc_failures: self.alloc_failures.load(Ordering::Acquire),
            address_reservation_failures: self.address_reservation_failures.load(Ordering::Acquire),
            invalid_frees: self.invalid_frees.load(Ordering::Acquire),
            cache_hits: self.cache_hits.load(Ordering::Acquire),
            cache_misses: self.cache_misses.load(Ordering::Acquire),
            cache_inserts: self.cache_inserts.load(Ordering::Acquire),
            cache_full_releases: self.cache_full_releases.load(Ordering::Acquire),
            cache_pressure_flushes: self.cache_pressure_flushes.load(Ordering::Acquire),
            cache_pressure_releases: self.cache_pressure_releases.load(Ordering::Acquire),
            cache_maintenance_flushes: self.cache_maintenance_flushes.load(Ordering::Acquire),
            cache_maintenance_releases: self.cache_maintenance_releases.load(Ordering::Acquire),
            cache_release_failures: self.cache_release_failures.load(Ordering::Acquire),
            cached_ranges,
            cached_pages,
            cached_bytes,
        }
    }

    pub fn audit(&self) -> KernelHeapAudit {
        let mut audit = self.cache.lock().audit(self.arena);
        audit.scanned_active_allocs = self.active_allocs.load(Ordering::Acquire);
        audit.scanned_active_pages = self.active_pages.load(Ordering::Acquire);
        audit.scanned_active_bytes = self.active_bytes.load(Ordering::Acquire);

        if audit.scanned_active_bytes != audit.scanned_active_pages.saturating_mul(PAGE_SIZE)
            || (audit.scanned_active_allocs == 0
                && (audit.scanned_active_pages != 0 || audit.scanned_active_bytes != 0))
        {
            audit
                .flags
                .insert(KernelHeapAuditFlags::ACTIVE_ACCOUNTING_MISMATCH);
        }

        audit
    }

    pub fn record_realloc(&self) {
        self.realloc_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn alloc_range(
        &self,
        layout: Layout,
        page_policy: PagePolicy,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<BackedRange, AllocationError> {
        if !self.is_initialized() {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::NotInitialized);
        }
        self.alloc_requests.fetch_add(1, Ordering::Relaxed);

        let Some((order, page_policy)) = effective_layout_policy(layout, page_policy) else {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::InvalidLayout);
        };
        let block_pages = 1usize << order;
        let block_bytes = block_pages * PAGE_SIZE;

        if is_cacheable_policy(order, page_policy) {
            if let Some(range) = self.pop_cached_range(order) {
                self.active_allocs.fetch_add(1, Ordering::Relaxed);
                self.active_pages.fetch_add(block_pages, Ordering::Relaxed);
                self.active_bytes.fetch_add(block_bytes, Ordering::Relaxed);
                return Ok(range);
            }
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }

        let range = match self.alloc_range_uncached(order, page_policy, phys, vmem) {
            Ok(range) => range,
            Err(err) => {
                let released = self.flush_cache_to_backend(phys, vmem);
                if released == 0 {
                    self.alloc_failures.fetch_add(1, Ordering::Relaxed);
                    self.address_reservation_failures
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(AllocationError::AddressSpace(err));
                }
                self.cache_pressure_flushes.fetch_add(1, Ordering::Relaxed);
                self.cache_pressure_releases
                    .fetch_add(released as u64, Ordering::Relaxed);
                match self.alloc_range_uncached(order, page_policy, phys, vmem) {
                    Ok(range) => range,
                    Err(err) => {
                        self.alloc_failures.fetch_add(1, Ordering::Relaxed);
                        self.address_reservation_failures
                            .fetch_add(1, Ordering::Relaxed);
                        return Err(AllocationError::AddressSpace(err));
                    }
                }
            }
        };

        self.active_allocs.fetch_add(1, Ordering::Relaxed);
        self.active_pages.fetch_add(block_pages, Ordering::Relaxed);
        self.active_bytes.fetch_add(block_bytes, Ordering::Relaxed);

        Ok(range)
    }

    pub fn free_record(
        &self,
        record: AllocationRecord,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<(), DeallocationError> {
        self.free_record_inner(record, phys, vmem, true)
    }

    pub fn free_layout(
        &self,
        ptr: usize,
        layout: Layout,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<(), DeallocationError> {
        let Some((order, page_policy)) = effective_layout_policy(layout, PagePolicy::BaseOnly)
        else {
            return Err(DeallocationError::InvalidPointer);
        };
        let Some(range) = vmem.backed_range(self.arena, ptr, phys) else {
            return Err(DeallocationError::UnknownPointer);
        };
        if range.order != order || range.size != (1usize << order) * PAGE_SIZE {
            return Err(DeallocationError::InvalidPointer);
        }
        let allocation_arena = match self.arena {
            ArenaKind::Kernel => AllocationArena::Kernel,
            ArenaKind::Tracked => AllocationArena::Tracked,
            ArenaKind::DirectMap => AllocationArena::DirectMap,
        };
        let record = AllocationRecord::new(AllocationKind::Large, MemoryDomain::Kernel, ptr)
            .with_arena(allocation_arena)
            .with_sizes(layout.size(), range.size, layout.align())
            .with_physical(range.paddr, range.order, range.size)
            .with_backend_cookie(Self::backend_cookie_for(range, page_policy));
        self.free_record(record, phys, vmem)
    }

    pub fn can_reuse_layout(
        &self,
        ptr: usize,
        old_layout: Layout,
        new_layout: Layout,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> bool {
        let Some((old_order, _)) = effective_layout_policy(old_layout, PagePolicy::BaseOnly) else {
            return false;
        };
        let Some((new_order, _)) = effective_layout_policy(new_layout, PagePolicy::BaseOnly) else {
            return false;
        };
        old_order == new_order
            && vmem
                .backed_range(self.arena, ptr, phys)
                .is_some_and(|range| range.order == old_order)
    }

    pub(crate) fn free_record_uncached(
        &self,
        record: AllocationRecord,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<(), DeallocationError> {
        self.free_record_inner(record, phys, vmem, false)
    }

    fn free_record_inner(
        &self,
        record: AllocationRecord,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
        allow_cache: bool,
    ) -> Result<(), DeallocationError> {
        self.free_requests.fetch_add(1, Ordering::Relaxed);
        if !self.is_initialized() {
            self.invalid_frees.fetch_add(1, Ordering::Relaxed);
            return Err(DeallocationError::UnknownPointer);
        }

        let paddr = match record.paddr {
            Some(paddr) => paddr,
            None => {
                self.invalid_frees.fetch_add(1, Ordering::Relaxed);
                return Err(DeallocationError::InvalidPointer);
            }
        };
        let order = record.order;
        if order > MAX_TRACKED_ORDER {
            self.invalid_frees.fetch_add(1, Ordering::Relaxed);
            return Err(DeallocationError::InvalidPointer);
        }
        let block_pages = 1usize << order;
        let block_bytes = block_pages * PAGE_SIZE;

        let expected_arena = match record.arena {
            Some(AllocationArena::Kernel) => ArenaKind::Kernel,
            Some(AllocationArena::Tracked) => ArenaKind::Tracked,
            _ => {
                self.invalid_frees.fetch_add(1, Ordering::Relaxed);
                return Err(DeallocationError::InvalidPointer);
            }
        };
        if expected_arena != self.arena {
            self.invalid_frees.fetch_add(1, Ordering::Relaxed);
            return Err(DeallocationError::InvalidPointer);
        }
        let range = BackedRange {
            arena: self.arena,
            vaddr: record.ptr,
            paddr,
            size: block_bytes,
            order,
        };

        let cache_outcome = if allow_cache {
            self.cache_freed_range(record, range)
        } else {
            CacheFreeOutcome::ReleaseCurrent(range)
        };
        match cache_outcome {
            CacheFreeOutcome::Cached => {}
            CacheFreeOutcome::ReleaseCurrent(range) => {
                vmem.free_backed_range(range, phys).map_err(|err| {
                    self.invalid_frees.fetch_add(1, Ordering::Relaxed);
                    DeallocationError::AddressSpace(err)
                })?;
            }
            CacheFreeOutcome::ReleaseEvicted(range) => {
                if let Err(err) = vmem.free_backed_range(range, phys) {
                    self.cache_release_failures.fetch_add(1, Ordering::Relaxed);
                    panic!(
                        "[alloc][invariant] kheap cache eviction release failed: vaddr={:#x} paddr={:#x} order={} err={:?}",
                        range.vaddr, range.paddr, range.order, err
                    );
                }
            }
        }

        self.active_allocs.fetch_sub(1, Ordering::Relaxed);
        self.active_pages.fetch_sub(block_pages, Ordering::Relaxed);
        self.active_bytes.fetch_sub(block_bytes, Ordering::Relaxed);
        Ok(())
    }

    pub fn required_order_for(layout: Layout) -> usize {
        required_order(layout).unwrap_or(MAX_TRACKED_ORDER + 1)
    }

    pub(crate) fn backend_cookie_for(range: BackedRange, page_policy: PagePolicy) -> usize {
        if is_cacheable_policy(range.order, page_policy) {
            KHEAP_CACHEABLE_BACKEND_COOKIE
        } else {
            0
        }
    }

    pub fn reclaim_cached_ranges(
        &self,
        max_ranges: usize,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> KernelHeapReclaimStats {
        let stats = self.release_cached_ranges(max_ranges, phys, vmem);
        if stats.released_ranges != 0 {
            self.cache_maintenance_flushes
                .fetch_add(1, Ordering::Relaxed);
            self.cache_maintenance_releases
                .fetch_add(stats.released_ranges as u64, Ordering::Relaxed);
        }
        stats
    }

    fn alloc_range_uncached(
        &self,
        order: usize,
        page_policy: PagePolicy,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<BackedRange, crate::AddressSpaceError> {
        vmem.alloc_backed_range(self.arena, order, page_policy, phys)
    }

    fn pop_cached_range(&self, order: usize) -> Option<BackedRange> {
        let range = self.cache.lock().pop(order);
        if range.is_some() {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        }
        range
    }

    fn cache_freed_range(&self, record: AllocationRecord, range: BackedRange) -> CacheFreeOutcome {
        if record.backend_cookie != KHEAP_CACHEABLE_BACKEND_COOKIE
            || !is_cacheable_range(range, self.arena)
        {
            return CacheFreeOutcome::ReleaseCurrent(range);
        }

        let evicted = self.cache.lock().push_or_evict_oldest(range);
        self.cache_inserts.fetch_add(1, Ordering::Relaxed);
        match evicted {
            Some(evicted) => {
                self.cache_full_releases.fetch_add(1, Ordering::Relaxed);
                CacheFreeOutcome::ReleaseEvicted(evicted)
            }
            None => CacheFreeOutcome::Cached,
        }
    }

    fn flush_cache_to_backend(
        &self,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> usize {
        self.release_cached_ranges(usize::MAX, phys, vmem)
            .released_ranges
    }

    fn release_cached_ranges(
        &self,
        max_ranges: usize,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> KernelHeapReclaimStats {
        let mut out = KernelHeapReclaimStats::default();
        while out.released_ranges < max_ranges {
            let range = self.cache.lock().pop_any();
            let Some(range) = range else {
                break;
            };
            if let Err(err) = vmem.free_backed_range(range, phys) {
                self.cache_release_failures.fetch_add(1, Ordering::Relaxed);
                panic!(
                    "[alloc][invariant] kheap cache release failed: vaddr={:#x} paddr={:#x} order={} err={:?}",
                    range.vaddr, range.paddr, range.order, err
                );
            }
            out.released_ranges += 1;
            out.released_pages = out.released_pages.saturating_add(1usize << range.order);
            out.released_bytes = out.released_bytes.saturating_add(range.size);
        }
        out
    }
}

impl Default for KernelHeap {
    fn default() -> Self {
        Self::new(ArenaKind::Kernel)
    }
}

fn required_order(layout: Layout) -> Option<usize> {
    let aligned = layout.pad_to_align();
    let size_pages = pages_for(aligned.size());
    let align_pages = pages_for(aligned.align().max(PAGE_SIZE));
    pages_to_order(size_pages.max(align_pages))
}

fn effective_layout_policy(layout: Layout, requested: PagePolicy) -> Option<(usize, PagePolicy)> {
    const MIN_LARGE_PAGE_ORDER: usize = 9; // 2 MiB

    let mut order = required_order(layout)?;
    let page_policy = match requested {
        PagePolicy::RequireLarge => {
            order = order.max(MIN_LARGE_PAGE_ORDER);
            PagePolicy::RequireLarge
        }
        PagePolicy::PreferLarge => PagePolicy::PreferLarge,
        PagePolicy::BaseOnly if order >= MIN_LARGE_PAGE_ORDER => PagePolicy::PreferLarge,
        PagePolicy::BaseOnly => PagePolicy::BaseOnly,
    };
    if order > MAX_TRACKED_ORDER {
        return None;
    }
    Some((order, page_policy))
}

#[inline]
fn cache_order_index(order: usize) -> Option<usize> {
    if order <= KHEAP_CACHE_MAX_ORDER {
        Some(order)
    } else {
        None
    }
}

#[inline]
fn cache_capacity_for_order(order: usize) -> usize {
    KHEAP_CACHE_CAPACITY_PER_ORDER
        .get(order)
        .copied()
        .unwrap_or(0)
}

#[inline]
fn is_cacheable_policy(order: usize, page_policy: PagePolicy) -> bool {
    cache_order_index(order).is_some() && matches!(page_policy, PagePolicy::BaseOnly)
}

#[inline]
fn is_cacheable_range(range: BackedRange, arena: ArenaKind) -> bool {
    if range.arena != arena || cache_order_index(range.order).is_none() {
        return false;
    }
    range.size == (1usize << range.order) * PAGE_SIZE
}

#[inline]
fn is_expected_cached_range(range: BackedRange, order: usize, arena: ArenaKind) -> bool {
    is_cacheable_range(range, arena)
        && range.order == order
        && range.vaddr.is_multiple_of(PAGE_SIZE)
        && range.paddr.is_multiple_of(PAGE_SIZE)
}

#[inline]
fn pages_for(bytes: usize) -> usize {
    bytes.max(1).div_ceil(PAGE_SIZE).max(1)
}

#[inline]
fn pages_to_order(pages: usize) -> Option<usize> {
    let mut order = 0;
    let mut block = 1;
    while block < pages {
        if order >= MAX_TRACKED_ORDER {
            return None;
        }
        block = block.checked_shl(1)?;
        order += 1;
    }
    Some(order)
}
