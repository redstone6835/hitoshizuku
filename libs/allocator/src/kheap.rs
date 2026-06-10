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

use spin::mutex::Mutex;

use crate::buddy::{BuddyAllocator, MAX_TRACKED_ORDER, PAGE_SIZE};
use crate::error::{AllocationError, DeallocationError};
use crate::request::AllocationRecord;
use crate::request::PagePolicy;
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
    pub cache_release_failures: u64,
    pub cached_ranges: usize,
    pub cached_pages: usize,
    pub cached_bytes: usize,
}

const KHEAP_CACHE_MAX_ORDER: usize = 1;
const KHEAP_CACHE_ORDER_COUNT: usize = KHEAP_CACHE_MAX_ORDER + 1;
const KHEAP_CACHE_CAPACITY_PER_ORDER: usize = 128;
const KHEAP_CACHEABLE_BACKEND_COOKIE: usize = 1;

#[derive(Clone, Copy)]
struct CachedOrderRanges {
    ranges: [Option<BackedRange>; KHEAP_CACHE_CAPACITY_PER_ORDER],
    len: usize,
}

impl CachedOrderRanges {
    const fn new() -> Self {
        Self {
            ranges: [None; KHEAP_CACHE_CAPACITY_PER_ORDER],
            len: 0,
        }
    }

    fn push(&mut self, range: BackedRange) -> bool {
        if self.len == self.ranges.len() {
            return false;
        }
        self.ranges[self.len] = Some(range);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<BackedRange> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let range = self.ranges[self.len];
        self.ranges[self.len] = None;
        range
    }
}

struct KernelHeapRangeCache {
    orders: [CachedOrderRanges; KHEAP_CACHE_ORDER_COUNT],
}

impl KernelHeapRangeCache {
    const fn new() -> Self {
        Self {
            orders: [CachedOrderRanges::new(); KHEAP_CACHE_ORDER_COUNT],
        }
    }

    fn push(&mut self, range: BackedRange) -> bool {
        let Some(idx) = cache_order_index(range.order) else {
            return false;
        };
        self.orders[idx].push(range)
    }

    fn pop(&mut self, order: usize) -> Option<BackedRange> {
        let idx = cache_order_index(order)?;
        self.orders[idx].pop()
    }

    fn pop_any(&mut self) -> Option<BackedRange> {
        for idx in (0..self.orders.len()).rev() {
            if let Some(range) = self.orders[idx].pop() {
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
}

pub struct KernelHeap {
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
    cache_release_failures: AtomicU64,
    cache: Mutex<KernelHeapRangeCache>,
}

impl KernelHeap {
    pub const fn new() -> Self {
        Self {
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
            cache_release_failures: self.cache_release_failures.load(Ordering::Acquire),
            cached_ranges,
            cached_pages,
            cached_bytes,
        }
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

        let range = BackedRange {
            arena: ArenaKind::Kernel,
            vaddr: record.ptr,
            paddr,
            size: block_bytes,
            order,
        };

        if !allow_cache || !self.try_cache_freed_range(record, range) {
            vmem.free_kernel_backed_range(range, phys).map_err(|err| {
                self.invalid_frees.fetch_add(1, Ordering::Relaxed);
                DeallocationError::AddressSpace(err)
            })?;
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

    fn alloc_range_uncached(
        &self,
        order: usize,
        page_policy: PagePolicy,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<BackedRange, crate::AddressSpaceError> {
        vmem.alloc_kernel_backed_range(order, phys, page_policy)
    }

    fn pop_cached_range(&self, order: usize) -> Option<BackedRange> {
        let range = self.cache.lock().pop(order);
        if range.is_some() {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        }
        range
    }

    fn try_cache_freed_range(&self, record: AllocationRecord, range: BackedRange) -> bool {
        if record.backend_cookie != KHEAP_CACHEABLE_BACKEND_COOKIE || !is_cacheable_range(range) {
            return false;
        }

        if self.cache.lock().push(range) {
            self.cache_inserts.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.cache_full_releases.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    fn flush_cache_to_backend(
        &self,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> usize {
        let mut released = 0usize;
        loop {
            let range = self.cache.lock().pop_any();
            let Some(range) = range else {
                break;
            };
            if let Err(err) = vmem.free_kernel_backed_range(range, phys) {
                self.cache_release_failures.fetch_add(1, Ordering::Relaxed);
                panic!(
                    "[alloc][invariant] kheap cache release failed: vaddr={:#x} paddr={:#x} order={} err={:?}",
                    range.vaddr, range.paddr, range.order, err
                );
            }
            released += 1;
        }
        released
    }
}

impl Default for KernelHeap {
    fn default() -> Self {
        Self::new()
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
fn is_cacheable_policy(order: usize, page_policy: PagePolicy) -> bool {
    cache_order_index(order).is_some() && matches!(page_policy, PagePolicy::BaseOnly)
}

#[inline]
fn is_cacheable_range(range: BackedRange) -> bool {
    if range.arena != ArenaKind::Kernel || cache_order_index(range.order).is_none() {
        return false;
    }
    range.size == (1usize << range.order) * PAGE_SIZE
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
