//! 小对象 slab 分配器。
//!
//! 这个模块负责高频、小尺寸对象的分配与释放，是内核常规 `Box`、小型结构体、缓存
//! 节点等对象最常走的路径。它的设计目标是：
//!
//! - 小对象分配足够快，避免频繁落到页级分配；
//! - 同尺寸对象集中放置，减少内部碎片；
//! - 借助每 CPU 缓存降低锁竞争和跨核共享压力。
//!
//! 实现思路接近经典的 slab/UMA：
//!
//! - 先把常用小尺寸离散成若干 size class；
//! - 每个 size class 维护自己的 slab 链表；
//! - slab 由一批页组成，再切分成若干固定大小对象槽；
//! - 热路径优先命中 per-CPU cache，缓存失配时再回到全局状态补货。
//!
//! 它和 `kheap` 的分工边界很明确：不适合放进 slab 的对象，直接交给大对象路径。
use core::alloc::Layout;
use core::mem::MaybeUninit;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::Mutex;

use crate::buddy::{BuddyAllocator, MAX_TRACKED_ORDER, PAGE_SIZE};
use crate::request::{AllocationArena, AllocationKind, AllocationRecord, MemoryDomain};
use crate::space::{ArenaKind, BackedRange, KernelAddressSpace};

pub const MAX_SMALL_SIZE: usize = 2048;
pub const MAX_CPUS: usize = 64;

const SIZE_CLASSES: [usize; 14] = [
    8, 16, 32, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048,
];
const SIZE_CLASS_COUNT: usize = SIZE_CLASSES.len();
pub const SLAB_SIZE_CLASS_COUNT: usize = SIZE_CLASS_COUNT;
const CACHE_CAPACITY: usize = 64;
const REFILL_BATCH: usize = 32;
const FLUSH_BATCH: usize = CACHE_CAPACITY / 2;
const BITMAP_WORDS: usize = 8;
const INVALID_SLAB_NODE: usize = 0;
const INVALID_CACHED_INDEX: u16 = u16::MAX;
const SLAB_LOOKUP_BUCKETS: usize = 1024;
const SLAB_DIRECTORY_BITS: usize = 9;
const SLAB_DIRECTORY_ENTRIES: usize = 1 << SLAB_DIRECTORY_BITS;
const SLAB_DIRECTORY_MASK: usize = SLAB_DIRECTORY_ENTRIES - 1;
const SLAB_DIRECTORY_LEVELS: usize = 3;
const SLAB_DIRECTORY_MAX_PAGES: usize = 1 << (SLAB_DIRECTORY_BITS * SLAB_DIRECTORY_LEVELS);
const OBJECT_STATE_BITS: usize = 2;
const OBJECTS_PER_STATE_WORD: usize = usize::BITS as usize / OBJECT_STATE_BITS;
const STATE_WORDS: usize = (BITMAP_WORDS * 64).div_ceil(OBJECTS_PER_STATE_WORD);
const OBJECT_FREE: u8 = 0;
const OBJECT_ALLOCATED: u8 = 1;
const OBJECT_CACHED: u8 = 2;
const SLAB_NODE_RETIRED: usize = 1usize << (usize::BITS as usize - 1);
const SLAB_NODE_PIN_MASK: usize = SLAB_NODE_RETIRED - 1;
const MAX_GROW_ATTEMPTS: usize = 3;
const MAX_EMPTY_SLABS_PER_ZONE: usize = 4;
static NEXT_SLAB_COOKIE: AtomicUsize = AtomicUsize::new(1);

#[cfg(feature = "performance-profile")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlabProfileCounter {
    CacheHit,
    CacheMiss,
    Refill,
    Flush,
    SlowPath,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SlabStats {
    pub alloc_requests: u64,
    pub free_requests: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub grow_failures: u64,
    pub active_objects: u64,
    pub active_slabs: usize,
    pub active_pages: usize,
    pub active_bytes: usize,
    pub address_reservation_failures: u64,
    pub invalid_frees: u64,
    pub cache_refills: u64,
    pub cache_flushes: u64,
    pub fast_free_hits: u64,
    pub fast_free_fallbacks: u64,
    pub reclaimed_slabs: u64,
    pub free_slab_nodes: usize,
}

impl SlabStats {
    pub(crate) fn merge(&mut self, other: Self) {
        self.alloc_requests = self.alloc_requests.saturating_add(other.alloc_requests);
        self.free_requests = self.free_requests.saturating_add(other.free_requests);
        self.cache_hits = self.cache_hits.saturating_add(other.cache_hits);
        self.cache_misses = self.cache_misses.saturating_add(other.cache_misses);
        self.grow_failures = self.grow_failures.saturating_add(other.grow_failures);
        self.active_objects = self.active_objects.saturating_add(other.active_objects);
        self.active_slabs = self.active_slabs.saturating_add(other.active_slabs);
        self.active_pages = self.active_pages.saturating_add(other.active_pages);
        self.active_bytes = self.active_bytes.saturating_add(other.active_bytes);
        self.address_reservation_failures = self
            .address_reservation_failures
            .saturating_add(other.address_reservation_failures);
        self.invalid_frees = self.invalid_frees.saturating_add(other.invalid_frees);
        self.cache_refills = self.cache_refills.saturating_add(other.cache_refills);
        self.cache_flushes = self.cache_flushes.saturating_add(other.cache_flushes);
        self.fast_free_hits = self.fast_free_hits.saturating_add(other.fast_free_hits);
        self.fast_free_fallbacks = self
            .fast_free_fallbacks
            .saturating_add(other.fast_free_fallbacks);
        self.reclaimed_slabs = self.reclaimed_slabs.saturating_add(other.reclaimed_slabs);
        self.free_slab_nodes = self.free_slab_nodes.saturating_add(other.free_slab_nodes);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SlabClassStat {
    pub size_class: usize,
    pub active_objects: u64,
    pub active_bytes: usize,
    pub active_slabs: usize,
    pub active_pages: usize,
    pub empty_slabs: usize,
    pub empty_pages: usize,
    pub reclaimable_empty_pages: usize,
    pub free_slab_nodes: usize,
}

impl SlabClassStat {
    pub(crate) fn merge(&mut self, other: Self) {
        self.active_objects = self.active_objects.saturating_add(other.active_objects);
        self.active_bytes = self.active_bytes.saturating_add(other.active_bytes);
        self.active_slabs = self.active_slabs.saturating_add(other.active_slabs);
        self.active_pages = self.active_pages.saturating_add(other.active_pages);
        self.empty_slabs = self.empty_slabs.saturating_add(other.empty_slabs);
        self.empty_pages = self.empty_pages.saturating_add(other.empty_pages);
        self.reclaimable_empty_pages = self
            .reclaimable_empty_pages
            .saturating_add(other.reclaimable_empty_pages);
        self.free_slab_nodes = self.free_slab_nodes.saturating_add(other.free_slab_nodes);
    }
}

/// 每 CPU 缓存中的一个槽位。
///
/// 除了对象指针本身，还记录其所属 slab 节点，使批量 flush 能直接回到原 slab。
#[derive(Clone, Copy)]
struct CacheEntry {
    ptr: usize,
    slab_node: usize,
    cached_index: u16,
}

impl CacheEntry {
    const fn empty() -> Self {
        Self {
            ptr: 0,
            slab_node: INVALID_SLAB_NODE,
            cached_index: INVALID_CACHED_INDEX,
        }
    }
}

/// 稀疏页 owner 目录的一个中间页或叶页。
///
/// 三层目录以 9 bit 为一级，覆盖 2^27 个 4 KiB 页，即 512 GiB heap window。中间页
/// 仅在 slab 首次覆盖对应区间时分配，避免为 LoongArch64 的 32 GiB heap 预留完整数组。
#[repr(C, align(4096))]
struct SlabDirectoryPage {
    entries: [AtomicUsize; SLAB_DIRECTORY_ENTRIES],
}

impl SlabDirectoryPage {
    const fn new() -> Self {
        Self {
            entries: [const { AtomicUsize::new(0) }; SLAB_DIRECTORY_ENTRIES],
        }
    }
}

/// allocator 级的 `virtual page -> SlabNode` side metadata。
///
/// lookup 固定执行三次数组寻址，不随 slab 数量增长；这与 SLUB 通过 `struct page`
/// 实现 `virt_to_slab()` 的方式等价。目录页永不归还，SlabNode 也只复用不释放，故
/// Acquire 读者不会解引用已释放的 metadata。
struct SlabPageDirectory {
    base: AtomicUsize,
    page_count: AtomicUsize,
    grow_lock: Mutex<()>,
    root: SlabDirectoryPage,
}

impl SlabPageDirectory {
    const fn new() -> Self {
        Self {
            base: AtomicUsize::new(0),
            page_count: AtomicUsize::new(0),
            grow_lock: Mutex::new(()),
            root: SlabDirectoryPage::new(),
        }
    }

    fn init(&self, region: (usize, usize)) -> bool {
        let (base, size) = region;
        if size == 0
            || !base.is_multiple_of(PAGE_SIZE)
            || !size.is_multiple_of(PAGE_SIZE)
            || base.checked_add(size).is_none()
        {
            return false;
        }
        let pages = size / PAGE_SIZE;
        if pages == 0 || pages > SLAB_DIRECTORY_MAX_PAGES {
            return false;
        }
        self.base.store(base, Ordering::Release);
        self.page_count.store(pages, Ordering::Release);
        true
    }

    fn ensure_range(&self, base: usize, size: usize) -> bool {
        let Some((start, pages)) = self.range_pages(base, size) else {
            return false;
        };
        let _guard = self.grow_lock.lock();
        for page in start..start + pages {
            if self.ensure_leaf(page).is_none() {
                return false;
            }
        }
        true
    }

    fn publish_range(&self, base: usize, size: usize, node_addr: usize) -> bool {
        let Some((start, pages)) = self.range_pages(base, size) else {
            return false;
        };
        for page in start..start + pages {
            let Some((leaf, slot)) = self.leaf_entry(page) else {
                self.clear_range(base, (page - start) * PAGE_SIZE, node_addr);
                return false;
            };
            match leaf.entries[slot].compare_exchange(
                0,
                node_addr,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {}
                Err(existing) if existing == node_addr => {}
                Err(_) => {
                    self.clear_range(base, (page - start) * PAGE_SIZE, node_addr);
                    return false;
                }
            }
        }
        true
    }

    fn clear_range(&self, base: usize, size: usize, node_addr: usize) -> bool {
        if size == 0 {
            return true;
        }
        let Some((start, pages)) = self.range_pages(base, size) else {
            return false;
        };
        let mut cleared = true;
        for page in start..start + pages {
            let Some((leaf, slot)) = self.leaf_entry(page) else {
                cleared = false;
                continue;
            };
            if leaf.entries[slot]
                .compare_exchange(node_addr, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                cleared = false;
            }
        }
        cleared
    }

    fn owns_range(&self, base: usize, size: usize, node_addr: usize) -> bool {
        let Some((start, pages)) = self.range_pages(base, size) else {
            return false;
        };
        (start..start + pages).all(|page| {
            self.leaf_entry(page)
                .is_some_and(|(leaf, slot)| leaf.entries[slot].load(Ordering::Acquire) == node_addr)
        })
    }

    #[inline]
    fn lookup(&self, ptr: usize) -> Option<SlabOwner> {
        let page = self.page_index(ptr)?;
        let (leaf, slot) = self.leaf_entry(page)?;
        // 节点内容由 lifecycle 的 Release/Acquire 发布；owner 本身只负责提供永久有效的
        // metadata 地址，因此这里无需再为同一节点付一次 Acquire fence。
        let node_addr = leaf.entries[slot].load(Ordering::Relaxed);
        if node_addr == 0 {
            return None;
        }
        let node = slab_node(node_addr);
        if !node.try_pin() {
            return None;
        }
        let owner = SlabOwner { node_addr };
        if leaf.entries[slot].load(Ordering::Relaxed) != node_addr || !node.slab.contains(ptr) {
            return None;
        }
        Some(owner)
    }

    fn ensure_leaf(&self, page: usize) -> Option<&'static SlabDirectoryPage> {
        let root_slot = (page >> (SLAB_DIRECTORY_BITS * 2)) & SLAB_DIRECTORY_MASK;
        let middle_slot = (page >> SLAB_DIRECTORY_BITS) & SLAB_DIRECTORY_MASK;
        let middle = ensure_directory_page(&self.root.entries[root_slot])?;
        ensure_directory_page(&middle.entries[middle_slot])
    }

    #[inline]
    fn leaf_entry(&self, page: usize) -> Option<(&'static SlabDirectoryPage, usize)> {
        let root_slot = (page >> (SLAB_DIRECTORY_BITS * 2)) & SLAB_DIRECTORY_MASK;
        let middle_slot = (page >> SLAB_DIRECTORY_BITS) & SLAB_DIRECTORY_MASK;
        let leaf_slot = page & SLAB_DIRECTORY_MASK;
        let middle = directory_page(self.root.entries[root_slot].load(Ordering::Acquire))?;
        let leaf = directory_page(middle.entries[middle_slot].load(Ordering::Acquire))?;
        Some((leaf, leaf_slot))
    }

    #[inline]
    fn page_index(&self, ptr: usize) -> Option<usize> {
        // SlabAllocator::initialized 的 Acquire 已发布固定 heap window。
        let pages = self.page_count.load(Ordering::Relaxed);
        let base = self.base.load(Ordering::Relaxed);
        let offset = ptr.checked_sub(base)?;
        let page = offset / PAGE_SIZE;
        (page < pages).then_some(page)
    }

    fn range_pages(&self, base: usize, size: usize) -> Option<(usize, usize)> {
        if size == 0 || !base.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let start = self.page_index(base)?;
        let pages = size / PAGE_SIZE;
        let end = start.checked_add(pages)?;
        (end <= self.page_count.load(Ordering::Relaxed)).then_some((start, pages))
    }
}

/// 页目录查询取得的节点生命周期 pin。
///
/// 回收者先把节点置为 retiring，再等待已有 pin 清零，因而 guard 存活期间节点不会被
/// 重新初始化。pin 只覆盖 owner 校验和对象状态 CAS，进入 per-CPU cache 前就会释放。
struct SlabOwner {
    node_addr: usize,
}

impl SlabOwner {
    #[inline]
    fn node_addr(&self) -> usize {
        self.node_addr
    }

    #[inline]
    fn node(&self) -> &SlabNode {
        slab_node(self.node_addr)
    }
}

impl Drop for SlabOwner {
    fn drop(&mut self) {
        slab_node(self.node_addr).unpin();
    }
}

fn ensure_directory_page(slot: &AtomicUsize) -> Option<&'static SlabDirectoryPage> {
    if let Some(page) = directory_page(slot.load(Ordering::Acquire)) {
        return Some(page);
    }
    let raw = crate::alloc_internal_metadata(Layout::new::<SlabDirectoryPage>());
    if raw.is_null() {
        return None;
    }
    let page = raw.cast::<SlabDirectoryPage>();
    // Safety: metadata allocator 满足 Layout 的大小和 4 KiB 对齐；该区域尚未发布给读者。
    unsafe { page.write(SlabDirectoryPage::new()) };
    slot.store(page as usize, Ordering::Release);
    directory_page(page as usize)
}

#[inline]
fn directory_page(addr: usize) -> Option<&'static SlabDirectoryPage> {
    if addr == 0 {
        None
    } else {
        // Safety: 目录只保存 `alloc_internal_metadata` 返回且永不释放的 DirectoryPage。
        Some(unsafe { &*(addr as *const SlabDirectoryPage) })
    }
}

/// 只暴露实际写入前缀的 cache 排空缓冲区。
///
/// 满 cache 释放属于冷路径，但不能为了排出少量对象先初始化整个临时数组。
struct CacheDrainBuffer<const N: usize> {
    entries: [MaybeUninit<CacheEntry>; N],
    initialized: usize,
}

impl<const N: usize> CacheDrainBuffer<N> {
    const fn new() -> Self {
        Self {
            entries: [const { MaybeUninit::uninit() }; N],
            initialized: 0,
        }
    }

    fn push(&mut self, entry: CacheEntry) -> bool {
        let Some(slot) = self.entries.get_mut(self.initialized) else {
            return false;
        };
        slot.write(entry);
        self.initialized += 1;
        true
    }

    fn is_full(&self) -> bool {
        self.initialized == N
    }

    fn initialized(&self) -> &[CacheEntry] {
        // Safety: `initialized` 仅在对应槽位完成 write 后递增，因此此前缀全部有效。
        unsafe {
            core::slice::from_raw_parts(
                self.entries.as_ptr().cast::<CacheEntry>(),
                self.initialized,
            )
        }
    }
}

/// 某个 CPU 在某个 size class 下的本地缓存状态。
///
/// 它的目标是把最热的小对象分配/释放留在本地 CPU 上完成，尽量少碰全局 slab 状态。
struct PerCpuCacheState {
    entries: [CacheEntry; CACHE_CAPACITY],
    count: usize,
    stats: PerCpuCacheStats,
}

impl PerCpuCacheState {
    const fn new() -> Self {
        Self {
            entries: [CacheEntry::empty(); CACHE_CAPACITY],
            count: 0,
            stats: PerCpuCacheStats::new(),
        }
    }

    fn pop_entry(&mut self) -> Option<CacheEntry> {
        if self.count == 0 {
            return None;
        }
        self.count -= 1;
        let entry = self.entries[self.count];
        self.entries[self.count] = CacheEntry::empty();
        Some(entry)
    }

    fn pop_for_alloc(&mut self) -> Option<CacheEntry> {
        self.stats.alloc_requests = self.stats.alloc_requests.saturating_add(1);
        let entry = self.pop_entry();
        if entry.is_some() {
            self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
            self.stats.successful_allocations = self.stats.successful_allocations.saturating_add(1);
        } else {
            self.stats.cache_misses = self.stats.cache_misses.saturating_add(1);
            self.stats.note_slow_path();
        }
        entry
    }

    fn push(&mut self, entry: CacheEntry) -> bool {
        if self.count >= CACHE_CAPACITY {
            return false;
        }
        self.entries[self.count] = entry;
        self.count += 1;
        true
    }

    fn push_for_free<const N: usize>(
        &mut self,
        entry: CacheEntry,
        drained: &mut CacheDrainBuffer<N>,
        used_hint: bool,
    ) -> usize {
        self.stats.free_requests = self.stats.free_requests.saturating_add(1);
        self.stats.successful_frees = self.stats.successful_frees.saturating_add(1);
        if used_hint {
            self.stats.fast_free_hits = self.stats.fast_free_hits.saturating_add(1);
        } else {
            self.stats.fast_free_fallbacks = self.stats.fast_free_fallbacks.saturating_add(1);
            self.stats.note_slow_path();
        }
        if self.push(entry) {
            return 0;
        }
        while !drained.is_full() {
            let Some(entry) = self.pop_entry() else {
                break;
            };
            if !drained.push(entry) {
                panic!("[alloc][invariant] slab drain buffer rejected available slot");
            }
        }
        if !self.push(entry) {
            panic!("[alloc][invariant] slab cache remained full after drain");
        }
        self.stats.flushes = self.stats.flushes.saturating_add(1);
        drained.initialized().len()
    }

    fn push_refill(&mut self, entries: &[CacheEntry], overflow: &mut [CacheEntry]) -> usize {
        if entries.is_empty() {
            return 0;
        }
        self.stats.refills = self.stats.refills.saturating_add(1);
        let mut overflow_count = 0usize;
        for &entry in entries {
            if !self.push(entry) {
                overflow[overflow_count] = entry;
                overflow_count += 1;
            }
        }
        if overflow_count != 0 {
            self.stats.flushes = self.stats.flushes.saturating_add(1);
        }
        overflow_count
    }

    fn note_slow_allocation(&mut self) {
        self.stats.successful_allocations = self.stats.successful_allocations.saturating_add(1);
    }

    fn drain_into(&mut self, out: &mut [CacheEntry]) -> usize {
        let mut drained = 0usize;
        for slot in out {
            let Some(entry) = self.pop_entry() else {
                break;
            };
            *slot = entry;
            drained += 1;
        }
        drained
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PerCpuCacheStats {
    alloc_requests: u64,
    successful_allocations: u64,
    free_requests: u64,
    successful_frees: u64,
    cache_hits: u64,
    cache_misses: u64,
    refills: u64,
    flushes: u64,
    fast_free_hits: u64,
    fast_free_fallbacks: u64,
    #[cfg(feature = "performance-profile")]
    slow_paths: u64,
}

impl PerCpuCacheStats {
    const fn new() -> Self {
        Self {
            alloc_requests: 0,
            successful_allocations: 0,
            free_requests: 0,
            successful_frees: 0,
            cache_hits: 0,
            cache_misses: 0,
            refills: 0,
            flushes: 0,
            fast_free_hits: 0,
            fast_free_fallbacks: 0,
            #[cfg(feature = "performance-profile")]
            slow_paths: 0,
        }
    }

    #[inline]
    fn note_slow_path(&mut self) {
        #[cfg(feature = "performance-profile")]
        {
            self.slow_paths = self.slow_paths.saturating_add(1);
        }
    }
}

struct PerCpuCache {
    inner: Mutex<PerCpuCacheState>,
}

impl PerCpuCache {
    const fn new() -> Self {
        Self {
            inner: Mutex::new(PerCpuCacheState::new()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlabAllocation {
    pub ptr: usize,
    pub backend_cookie: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlabReclaimStats {
    pub flushed_cached_objects: usize,
    pub reclaimed_slabs: usize,
    pub reclaimed_pages: usize,
    pub reclaimed_bytes: usize,
}

impl SlabReclaimStats {
    pub(crate) fn merge(&mut self, other: Self) {
        self.flushed_cached_objects = self
            .flushed_cached_objects
            .saturating_add(other.flushed_cached_objects);
        self.reclaimed_slabs = self.reclaimed_slabs.saturating_add(other.reclaimed_slabs);
        self.reclaimed_pages = self.reclaimed_pages.saturating_add(other.reclaimed_pages);
        self.reclaimed_bytes = self.reclaimed_bytes.saturating_add(other.reclaimed_bytes);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlabAudit {
    pub flags: SlabAuditFlags,
    pub scanned_slabs: usize,
    pub scanned_active_objects: u64,
    pub scanned_cached_objects: u64,
    pub scanned_active_pages: usize,
    pub scanned_active_bytes: usize,
    pub scanned_free_nodes: usize,
}

impl SlabAudit {
    pub const fn is_consistent(self) -> bool {
        self.flags.is_empty()
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.flags.0 |= other.flags.0;
        self.scanned_slabs = self.scanned_slabs.saturating_add(other.scanned_slabs);
        self.scanned_active_objects = self
            .scanned_active_objects
            .saturating_add(other.scanned_active_objects);
        self.scanned_cached_objects = self
            .scanned_cached_objects
            .saturating_add(other.scanned_cached_objects);
        self.scanned_active_pages = self
            .scanned_active_pages
            .saturating_add(other.scanned_active_pages);
        self.scanned_active_bytes = self
            .scanned_active_bytes
            .saturating_add(other.scanned_active_bytes);
        self.scanned_free_nodes = self
            .scanned_free_nodes
            .saturating_add(other.scanned_free_nodes);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlabAuditFlags(u32);

impl SlabAuditFlags {
    pub const SLAB_CHAIN_LOOP: Self = Self(1 << 0);
    pub const FREE_NODE_LOOP: Self = Self(1 << 1);
    pub const OBJECT_COUNT_MISMATCH: Self = Self(1 << 2);
    pub const CACHE_COUNT_MISMATCH: Self = Self(1 << 3);
    pub const CACHE_WITHOUT_ALLOC: Self = Self(1 << 4);
    pub const UNUSED_BITS_SET: Self = Self(1 << 5);
    pub const INVALID_SLAB_RANGE: Self = Self(1 << 6);
    pub const STATS_MISMATCH: Self = Self(1 << 7);
    pub const SLAB_LOOKUP_MISMATCH: Self = Self(1 << 8);
    pub const SLAB_DIRECTORY_MISMATCH: Self = Self(1 << 9);

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

#[derive(Clone, Copy, Debug, Default)]
struct SlabFlushResult {
    flushed: usize,
}

impl SlabAllocation {
    const fn null() -> Self {
        Self {
            ptr: 0,
            backend_cookie: 0,
        }
    }

    pub const fn is_null(self) -> bool {
        self.ptr == 0
    }
}

/// 单个 slab 的运行时状态。
///
/// 一个 slab 对应一批连续页和固定大小对象槽。进入 magazine 的对象继续保持 alloc 位，
/// 只有批量 flush 才把它们变回空槽，因此 alloc/free 命中不需要同步共享位图。
struct Slab {
    /// 节点会在回收后复用；这些字段必须允许旧目录读者与重新初始化并发观察，而不能
    /// 通过覆盖整个 `Slab` 制造 `&mut` / `&` 别名。
    base_addr: AtomicUsize,
    paddr: AtomicUsize,
    page_count: AtomicUsize,
    object_size: AtomicUsize,
    total_objects: AtomicUsize,
    allocated_objects: AtomicUsize,
    /// 下次位图扫描的起点。
    ///
    /// slab 对象通常按低地址递增分配；如果每次 cache miss 都从 0 开始扫描，活跃 slab
    /// 越满，前缀已分配位带来的重复检查越多。这个 hint 让分配路径从上次命中位置之后
    /// 继续，并在 flush 释放真实空槽时回退到被释放槽位。
    next_free_hint: AtomicUsize,
    /// 每个对象使用两个原子位表示状态：FREE、ALLOCATED 或 CACHED。
    ///
    /// 释放和 cache refill 不再需要 ZoneState 锁，只对对应对象执行一次 CAS；ZoneState
    /// 仍负责批量分配、统计计数和 slab 链表维护。
    object_states: [AtomicU64; STATE_WORDS],
    active: AtomicBool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlabObjectState {
    Allocated,
    Cached,
}

impl Slab {
    const fn empty() -> Self {
        Self {
            base_addr: AtomicUsize::new(0),
            paddr: AtomicUsize::new(0),
            page_count: AtomicUsize::new(0),
            object_size: AtomicUsize::new(0),
            total_objects: AtomicUsize::new(0),
            allocated_objects: AtomicUsize::new(0),
            next_free_hint: AtomicUsize::new(0),
            object_states: [const { AtomicU64::new(0) }; STATE_WORDS],
            active: AtomicBool::new(false),
        }
    }

    /// 在节点尚未发布到页目录时准备一个 slab 生命周期。
    ///
    /// 所有字段都使用原子写入，因此旧的失败查询即使跨越节点回收，也不会与重新初始化
    /// 形成数据竞争。调用方必须在插入页目录后再调用 [`Slab::activate`]。
    fn prepare(&self, base_addr: usize, paddr: usize, page_count: usize, obj_size: usize) {
        self.active.store(false, Ordering::Release);
        self.base_addr.store(0, Ordering::Relaxed);
        self.paddr.store(0, Ordering::Relaxed);
        self.page_count.store(0, Ordering::Relaxed);
        self.object_size.store(0, Ordering::Relaxed);
        self.total_objects.store(0, Ordering::Relaxed);
        self.allocated_objects.store(0, Ordering::Relaxed);
        self.next_free_hint.store(0, Ordering::Relaxed);
        for word in &self.object_states {
            word.store(0, Ordering::Relaxed);
        }
        // 一个 slab 初始化时，核心工作不是“构造对象”，而是把一段连续页解释成固定大小的
        // 槽位数组，并把所有状态压进位图。后续对象生命周期全靠位图位翻转推进。
        if obj_size == 0 {
            return;
        }

        let Some(span_size) = page_count.checked_mul(PAGE_SIZE) else {
            return;
        };
        if page_count > u16::MAX as usize {
            return;
        }
        if obj_size > u16::MAX as usize {
            return;
        }

        let total_objects = (span_size / obj_size).min(BITMAP_WORDS * 64);
        self.base_addr.store(base_addr, Ordering::Relaxed);
        self.paddr.store(paddr, Ordering::Relaxed);
        self.page_count.store(page_count, Ordering::Relaxed);
        self.object_size.store(obj_size, Ordering::Relaxed);
        self.total_objects.store(total_objects, Ordering::Relaxed);
    }

    fn activate(&self) -> bool {
        let active = self.total_objects.load(Ordering::Relaxed) != 0;
        self.active.store(active, Ordering::Release);
        active
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    #[inline]
    fn base_addr(&self) -> usize {
        self.base_addr.load(Ordering::Relaxed)
    }

    #[inline]
    fn paddr(&self) -> usize {
        self.paddr.load(Ordering::Relaxed)
    }

    #[inline]
    fn page_count(&self) -> usize {
        self.page_count.load(Ordering::Relaxed)
    }

    #[inline]
    fn object_size(&self) -> usize {
        self.object_size.load(Ordering::Relaxed)
    }

    #[inline]
    fn total_objects(&self) -> usize {
        self.total_objects.load(Ordering::Relaxed)
    }

    #[inline]
    fn allocated_objects(&self) -> usize {
        self.allocated_objects.load(Ordering::Relaxed)
    }

    #[inline]
    fn span_size(&self) -> Option<usize> {
        self.page_count().checked_mul(PAGE_SIZE)
    }

    fn contains(&self, ptr: usize) -> bool {
        // 这里只判断地址是否落在 slab 覆盖范围内，不验证它是不是对象边界。更严格的槽位
        // 合法性检查交给 `object_index` 统一处理。
        if !self.active.load(Ordering::Relaxed) {
            return false;
        }
        let Some(span_size) = self.span_size() else {
            return false;
        };
        let base_addr = self.base_addr();
        let Some(end) = base_addr.checked_add(span_size) else {
            return false;
        };
        ptr >= base_addr && ptr < end
    }

    fn object_index(&self, ptr: usize, obj_size: usize) -> Option<usize> {
        // 指针要想映射成有效槽位，必须同时满足：落在 slab 范围内、位于对象边界上、索引
        // 不超过 `total_objects`。这三个条件一起定义了“一个对象指针在 slab 语义下成立”。
        if obj_size == 0 || obj_size != self.object_size() {
            return None;
        }

        if !self.contains(ptr) {
            return None;
        }
        let offset = ptr - self.base_addr();
        if !offset.is_multiple_of(obj_size) {
            return None;
        }
        let idx = offset / obj_size;
        (idx < self.total_objects()).then_some(idx)
    }

    fn allocate(&self, obj_size: usize) -> Option<usize> {
        // slab 内部只在批量补货路径扫描和修改位图。分配给调用者或暂存在 magazine
        // 对 slab 来说都是保留状态，两者的转换完全发生在 CPU 本地 cache 中。
        let total_objects = self.total_objects();
        if !self.active.load(Ordering::Acquire)
            || self.allocated_objects() >= total_objects
            || obj_size != self.object_size()
        {
            return None;
        }

        let Some(idx) = self.find_free_slot() else {
            self.next_free_hint.store(total_objects, Ordering::Relaxed);
            return None;
        };

        if !self.transition_state(idx, OBJECT_FREE, OBJECT_ALLOCATED) {
            return None;
        }
        self.allocated_objects.fetch_add(1, Ordering::Relaxed);
        self.next_free_hint
            .store(next_hint_after(idx, total_objects), Ordering::Relaxed);
        Some(self.base_addr() + idx * obj_size)
    }

    fn object_state(&self, ptr: usize, obj_size: usize) -> Option<SlabObjectState> {
        let idx = self.object_index(ptr, obj_size)?;
        match self.load_state(idx) {
            OBJECT_ALLOCATED => Some(SlabObjectState::Allocated),
            OBJECT_CACHED => Some(SlabObjectState::Cached),
            _ => None,
        }
    }

    fn release_reserved(&self, entry: CacheEntry, obj_size: usize) -> bool {
        // magazine flush 才会真正释放对象槽。此时持有 ZoneState 锁，可以直接修改
        // slab 计数；对象状态本身使用 CAS，避免与无锁 free 发生状态撕裂。
        let Some(idx) = self.object_index(entry.ptr, obj_size) else {
            return false;
        };
        let expected = if entry.cached_index == INVALID_CACHED_INDEX {
            OBJECT_ALLOCATED
        } else {
            OBJECT_CACHED
        };
        if !self.transition_state(idx, expected, OBJECT_FREE) {
            return false;
        }
        let allocated = self.allocated_objects();
        self.allocated_objects
            .store(allocated.saturating_sub(1), Ordering::Relaxed);
        let hint = self.next_free_hint.load(Ordering::Relaxed);
        self.next_free_hint.store(idx.min(hint), Ordering::Relaxed);
        true
    }

    fn is_empty(&self) -> bool {
        self.active.load(Ordering::Acquire) && self.allocated_objects() == 0
    }

    #[inline]
    fn load_state(&self, idx: usize) -> u8 {
        let word = idx / OBJECTS_PER_STATE_WORD;
        let shift = (idx % OBJECTS_PER_STATE_WORD) * OBJECT_STATE_BITS;
        self.object_states
            .get(word)
            .map(|bits| ((bits.load(Ordering::Relaxed) >> shift) & 0b11) as u8)
            .unwrap_or(OBJECT_FREE)
    }

    #[inline]
    fn transition_state(&self, idx: usize, expected: u8, next: u8) -> bool {
        let word = idx / OBJECTS_PER_STATE_WORD;
        let shift = (idx % OBJECTS_PER_STATE_WORD) * OBJECT_STATE_BITS;
        let Some(bits) = self.object_states.get(word) else {
            return false;
        };
        let mask = 0b11u64 << shift;
        let expected = (expected as u64) << shift;
        let next = (next as u64) << shift;
        let mut current = bits.load(Ordering::Relaxed);
        loop {
            if current & mask != expected {
                return false;
            }
            let updated = (current & !mask) | next;
            match bits.compare_exchange_weak(current, updated, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    #[inline]
    fn mark_cached(&self, ptr: usize, obj_size: usize) -> Option<u16> {
        let idx = self.object_index(ptr, obj_size)?;
        self.transition_state(idx, OBJECT_ALLOCATED, OBJECT_CACHED)
            .then_some(idx as u16)
    }

    #[inline]
    fn mark_allocated(&self, entry: CacheEntry) -> bool {
        if entry.cached_index == INVALID_CACHED_INDEX {
            return true;
        }
        self.transition_state(entry.cached_index as usize, OBJECT_CACHED, OBJECT_ALLOCATED)
    }

    fn find_free_slot(&self) -> Option<usize> {
        let total = self.total_objects();
        if total == 0 {
            return None;
        }

        let start = self
            .next_free_hint
            .load(Ordering::Relaxed)
            .min(total.saturating_sub(1));
        find_free_state_in_range(&self.object_states, start, total)
            .or_else(|| find_free_state_in_range(&self.object_states, 0, start))
    }
}

struct SlabNode {
    slab: Slab,
    backing: AtomicBackedRange,
    next: AtomicUsize,
    lookup_next: AtomicUsize,
    /// 最高位表示 retiring，其余位为无锁目录读者数量。
    lifecycle: AtomicUsize,
    /// 每次节点复用都会更换的后端身份，防止 registry cookie 跨生命周期 ABA。
    backend_cookie: AtomicUsize,
}

struct AtomicBackedRange {
    arena: AtomicUsize,
    vaddr: AtomicUsize,
    paddr: AtomicUsize,
    size: AtomicUsize,
    order: AtomicUsize,
}

impl AtomicBackedRange {
    const fn empty() -> Self {
        Self {
            arena: AtomicUsize::new(0),
            vaddr: AtomicUsize::new(0),
            paddr: AtomicUsize::new(0),
            size: AtomicUsize::new(0),
            order: AtomicUsize::new(0),
        }
    }

    fn store(&self, range: BackedRange) {
        self.arena
            .store(encode_arena(range.arena), Ordering::Relaxed);
        self.vaddr.store(range.vaddr, Ordering::Relaxed);
        self.paddr.store(range.paddr, Ordering::Relaxed);
        self.size.store(range.size, Ordering::Relaxed);
        self.order.store(range.order, Ordering::Relaxed);
    }

    fn load(&self) -> BackedRange {
        BackedRange {
            arena: decode_arena(self.arena.load(Ordering::Relaxed)),
            vaddr: self.vaddr.load(Ordering::Relaxed),
            paddr: self.paddr.load(Ordering::Relaxed),
            size: self.size.load(Ordering::Relaxed),
            order: self.order.load(Ordering::Relaxed),
        }
    }
}

const fn encode_arena(arena: ArenaKind) -> usize {
    match arena {
        ArenaKind::DirectMap => 0,
        ArenaKind::Kernel => 1,
        ArenaKind::Tracked => 2,
    }
}

fn decode_arena(encoded: usize) -> ArenaKind {
    match encoded {
        0 => ArenaKind::DirectMap,
        1 => ArenaKind::Kernel,
        2 => ArenaKind::Tracked,
        _ => panic!("[alloc][invariant] invalid slab backing arena"),
    }
}

impl SlabNode {
    const fn empty() -> Self {
        Self {
            slab: Slab::empty(),
            backing: AtomicBackedRange::empty(),
            next: AtomicUsize::new(0),
            lookup_next: AtomicUsize::new(0),
            lifecycle: AtomicUsize::new(SLAB_NODE_RETIRED),
            backend_cookie: AtomicUsize::new(0),
        }
    }

    fn prepare(&self, range: BackedRange, page_count: usize, obj_size: usize) {
        self.slab
            .prepare(range.vaddr, range.paddr, page_count, obj_size);
        self.backing.store(range);
        self.backend_cookie
            .store(next_slab_cookie(), Ordering::Relaxed);
        self.next.store(0, Ordering::Relaxed);
        self.lookup_next.store(0, Ordering::Relaxed);
    }

    #[inline]
    fn try_pin(&self) -> bool {
        let mut lifecycle = self.lifecycle.load(Ordering::Relaxed);
        loop {
            if lifecycle & SLAB_NODE_RETIRED != 0 || lifecycle == SLAB_NODE_PIN_MASK {
                return false;
            }
            match self.lifecycle.compare_exchange_weak(
                lifecycle,
                lifecycle + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => lifecycle = observed,
            }
        }
    }

    #[inline]
    fn unpin(&self) {
        let previous = self.lifecycle.fetch_sub(1, Ordering::Release);
        if previous & SLAB_NODE_PIN_MASK == 0 {
            panic!("[alloc][invariant] slab node pin underflow");
        }
    }

    fn begin_retire(&self) {
        let previous = self.lifecycle.fetch_or(SLAB_NODE_RETIRED, Ordering::AcqRel);
        if previous & SLAB_NODE_RETIRED != 0 {
            panic!("[alloc][invariant] slab node retired twice");
        }
    }

    #[inline]
    fn pins_drained(&self) -> bool {
        self.lifecycle.load(Ordering::Acquire) == SLAB_NODE_RETIRED
    }

    fn activate(&self) -> bool {
        if !self.slab.activate() {
            return false;
        }
        let previous = self.lifecycle.swap(0, Ordering::Release);
        if previous != SLAB_NODE_RETIRED {
            panic!("[alloc][invariant] slab node activated with live readers");
        }
        true
    }

    #[inline]
    fn backend_cookie(&self) -> usize {
        self.backend_cookie.load(Ordering::Relaxed)
    }
}

fn next_slab_cookie() -> usize {
    loop {
        let cookie = NEXT_SLAB_COOKIE.fetch_add(1, Ordering::Relaxed);
        if cookie != 0 {
            return cookie;
        }
    }
}

struct ZoneState {
    slab_head: usize,
    preferred_slab: usize,
    slab_lookup: [usize; SLAB_LOOKUP_BUCKETS],
    /// 回收空 slab 后留下的 SlabNode 元数据 freelist。
    ///
    /// metadata allocator 目前只支持分配、不支持归还；因此空 slab 被释放 backing
    /// range 后，节点本身必须在 zone 内复用，避免频繁 grow/reclaim 导致元数据单调膨胀。
    free_node_head: usize,
    free_node_count: usize,
    /// 已脱离目录但仍可能有旧读者 pin 的节点；冷路径观察到 pin 清零后才转入 freelist。
    retired_node_head: usize,
    retired_node_count: usize,
    slab_count: usize,
    stats: SlabStats,
}

impl ZoneState {
    const fn new() -> Self {
        Self {
            slab_head: 0,
            preferred_slab: 0,
            slab_lookup: [0; SLAB_LOOKUP_BUCKETS],
            free_node_head: 0,
            free_node_count: 0,
            retired_node_head: 0,
            retired_node_count: 0,
            slab_count: 0,
            stats: SlabStats {
                alloc_requests: 0,
                free_requests: 0,
                cache_hits: 0,
                cache_misses: 0,
                grow_failures: 0,
                active_objects: 0,
                active_slabs: 0,
                active_pages: 0,
                active_bytes: 0,
                address_reservation_failures: 0,
                invalid_frees: 0,
                cache_refills: 0,
                cache_flushes: 0,
                fast_free_hits: 0,
                fast_free_fallbacks: 0,
                reclaimed_slabs: 0,
                free_slab_nodes: 0,
            },
        }
    }

    fn allocate_batch(&mut self, obj_size: usize, out: &mut [CacheEntry]) -> usize {
        // 一次 ZoneState 临界区同时取得当前请求和后续 magazine 补货，避免 miss 路径
        // 先分配一个对象、再重新拿锁补货。所有对象在 slab 中都保持 reserved 状态。
        let mut produced = 0;
        // 同一批补货沿着 slab 链继续向前，不为每个对象重新从首个 slab 扫描。
        let start = if self.preferred_slab != 0 {
            self.preferred_slab
        } else {
            self.slab_head
        };
        let mut node_addr = start;
        let mut wrapped = false;

        while produced < out.len() {
            let mut entry = None;
            let mut selected = 0;
            loop {
                if node_addr == 0 {
                    if !wrapped && start != self.slab_head {
                        node_addr = self.slab_head;
                        wrapped = true;
                        continue;
                    }
                    break;
                }
                // 回绕后再次到达起始节点，说明整条链已经检查完毕。
                if wrapped && node_addr == start {
                    break;
                }
                let current = node_addr;
                let node = slab_node(current);
                if let Some(ptr) = node.slab.allocate(obj_size) {
                    entry = Some(CacheEntry {
                        ptr,
                        slab_node: current,
                        cached_index: INVALID_CACHED_INDEX,
                    });
                    selected = current;
                    break;
                }
                node_addr = node.next.load(Ordering::Relaxed);
            }
            let Some(entry) = entry else {
                break;
            };
            out[produced] = entry;
            produced += 1;
            // 当前 slab 还有空槽时继续使用它；耗尽后才转到下一个节点。
            let node = slab_node(selected);
            node_addr = if node.slab.allocated_objects() < node.slab.total_objects() {
                selected
            } else {
                node.next.load(Ordering::Relaxed)
            };
            self.preferred_slab = if node_addr != 0 {
                node_addr
            } else {
                // 当前节点耗尽后从链表头重新开始，下一批仍会覆盖整条链。
                self.slab_head
            };
        }
        produced
    }

    fn insert_slab_node(&mut self, node_addr: usize) {
        let node = slab_node(node_addr);
        let backing = node.backing.load();
        let block_pages = node.slab.page_count();
        let bucket = slab_lookup_bucket(node.slab.base_addr(), backing.size);
        node.lookup_next
            .store(self.slab_lookup[bucket], Ordering::Relaxed);
        self.slab_lookup[bucket] = node_addr;
        node.next.store(self.slab_head, Ordering::Relaxed);
        self.slab_head = node_addr;
        self.preferred_slab = node_addr;
        self.slab_count += 1;
        self.stats.active_slabs = self.slab_count;
        self.stats.active_pages += block_pages;
    }

    fn pop_reusable_slab_node(&mut self) -> Option<usize> {
        self.reap_retired_slab_nodes();
        if self.free_node_head == 0 {
            return None;
        }
        let node_addr = self.free_node_head;
        let node = slab_node(node_addr);
        self.free_node_head = node.next.load(Ordering::Relaxed);
        node.next.store(0, Ordering::Relaxed);
        self.free_node_count = self.free_node_count.saturating_sub(1);
        self.update_free_node_stats();
        Some(node_addr)
    }

    fn push_reusable_slab_node(&mut self, node_addr: usize) {
        let node = slab_node(node_addr);
        if !node.pins_drained() {
            panic!("[alloc][invariant] pinned slab node entered reusable list");
        }
        node.slab.deactivate();
        node.lookup_next.store(0, Ordering::Relaxed);
        node.next.store(self.free_node_head, Ordering::Relaxed);
        self.free_node_head = node_addr;
        self.free_node_count += 1;
        self.update_free_node_stats();
    }

    fn enqueue_retired_slab_node(&mut self, node_addr: usize) {
        let node = slab_node(node_addr);
        node.next.store(self.retired_node_head, Ordering::Relaxed);
        self.retired_node_head = node_addr;
        self.retired_node_count += 1;
        self.reap_retired_slab_nodes();
        self.update_free_node_stats();
    }

    fn reap_retired_slab_nodes(&mut self) {
        let mut previous = 0usize;
        let mut current = self.retired_node_head;
        while current != 0 {
            let node = slab_node(current);
            let next = node.next.load(Ordering::Relaxed);
            if node.pins_drained() {
                if previous == 0 {
                    self.retired_node_head = next;
                } else {
                    slab_node(previous).next.store(next, Ordering::Relaxed);
                }
                self.retired_node_count = self.retired_node_count.saturating_sub(1);
                node.next.store(0, Ordering::Relaxed);
                self.push_reusable_slab_node(current);
            } else {
                previous = current;
            }
            current = next;
        }
        self.update_free_node_stats();
    }

    fn update_free_node_stats(&mut self) {
        self.stats.free_slab_nodes = self.free_node_count.saturating_add(self.retired_node_count);
    }

    fn lookup_slab_node(&self, slab_base: usize, slab_span: usize) -> Option<usize> {
        if slab_span == 0 || !slab_span.is_power_of_two() {
            return None;
        }
        let bucket = slab_lookup_bucket(slab_base, slab_span);
        let mut current = self.slab_lookup[bucket];
        let mut visited = 0usize;
        while current != 0 && visited < self.slab_count {
            let node = slab_node(current);
            let backing = node.backing.load();
            if node.slab.active.load(Ordering::Acquire)
                && node.slab.base_addr() == slab_base
                && backing.size == slab_span
            {
                return Some(current);
            }
            current = node.lookup_next.load(Ordering::Relaxed);
            visited += 1;
        }
        None
    }

    fn unlink_slab_lookup(&mut self, node_addr: usize) -> bool {
        if node_addr == INVALID_SLAB_NODE {
            return false;
        }
        let node = slab_node(node_addr);
        let backing = node.backing.load();
        let bucket = slab_lookup_bucket(node.slab.base_addr(), backing.size);
        let mut previous = 0usize;
        let mut current = self.slab_lookup[bucket];
        let mut visited = 0usize;
        while current != 0 && visited < self.slab_count {
            let next = slab_node(current).lookup_next.load(Ordering::Relaxed);
            if current == node_addr {
                if previous == 0 {
                    self.slab_lookup[bucket] = next;
                } else {
                    slab_node(previous)
                        .lookup_next
                        .store(next, Ordering::Relaxed);
                }
                slab_node(current).lookup_next.store(0, Ordering::Relaxed);
                return true;
            }
            previous = current;
            current = next;
            visited += 1;
        }
        false
    }

    #[allow(dead_code)]
    fn find_allocated_node(
        &mut self,
        ptr: usize,
        obj_size: usize,
        slab_span: usize,
    ) -> Option<(usize, SlabObjectState)> {
        // backing 按 slab span 对齐，因此指针可直接归一化成目录键。释放路径只检查
        // 对应哈希桶，不再遍历该 size class 的完整 slab 链。
        if slab_span == 0 || !slab_span.is_power_of_two() {
            self.stats.invalid_frees += 1;
            return None;
        }
        let slab_base = ptr & !(slab_span - 1);
        if let Some(node_addr) = self.lookup_slab_node(slab_base, slab_span) {
            let node = slab_node(node_addr);
            if let Some(state) = node.slab.object_state(ptr, obj_size) {
                return Some((node_addr, state));
            }
        }
        self.stats.invalid_frees += 1;
        None
    }

    fn mark_allocated(entry: CacheEntry) -> bool {
        if entry.cached_index == INVALID_CACHED_INDEX || entry.slab_node == INVALID_SLAB_NODE {
            return false;
        }
        slab_node(entry.slab_node).slab.mark_allocated(entry)
    }

    fn flush_cached_entries(&mut self, entries: &[CacheEntry], obj_size: usize) -> SlabFlushResult {
        // 当本地 cache 过满时，批量冲刷一批对象回 slab。这里故意保持顺序、朴素的实现，
        // 因为它走的是冷一些的回压路径，稳定性比极致优化更重要。普通 free 只冲刷
        // 固定批量，不扫描或同步回收空 slab；页回收统一由 pressure/reclaim 冷路径完成。
        let mut flushed = 0;
        for entry in entries {
            if entry.slab_node == INVALID_SLAB_NODE {
                continue;
            }
            let slab = &slab_node(entry.slab_node).slab;
            if !slab.active.load(Ordering::Acquire) {
                self.stats.invalid_frees += 1;
                continue;
            }
            if slab.release_reserved(*entry, obj_size) {
                flushed += 1;
            }
        }
        SlabFlushResult { flushed }
    }

    fn note_grow_failure(&mut self, reason: SlabGrowError) {
        self.stats.grow_failures += 1;
        if matches!(
            reason,
            SlabGrowError::BackedRange
                | SlabGrowError::UnsupportedOrder
                | SlabGrowError::InvalidBacking
        ) {
            self.stats.address_reservation_failures += 1;
        }
    }

    fn take_reclaimable_empty_slab(
        &mut self,
        directory: &SlabPageDirectory,
    ) -> Option<(BackedRange, usize)> {
        let mut empty_count = 0usize;
        let mut node_addr = self.slab_head;
        while node_addr != 0 {
            let node = slab_node(node_addr);
            if node.slab.is_empty() {
                empty_count += 1;
            }
            node_addr = node.next.load(Ordering::Relaxed);
        }
        if empty_count <= MAX_EMPTY_SLABS_PER_ZONE {
            return None;
        }

        let mut prev = 0usize;
        let mut current = self.slab_head;
        while current != 0 {
            let node = slab_node(current);
            let next = node.next.load(Ordering::Relaxed);
            if node.slab.is_empty() {
                let backing = node.backing.load();
                if !self.unlink_slab_lookup(current) {
                    panic!("[alloc][invariant] active slab missing from lookup directory");
                }
                // 先阻止新的无锁读者进入，再清 owner 目录；节点只有在已有读者退出后
                // 才能放回复用链，避免旧查询与下一次生命周期的初始化发生 ABA。
                node.begin_retire();
                node.slab.deactivate();
                if !directory.clear_range(backing.vaddr, backing.size, current) {
                    panic!("[alloc][invariant] active slab missing from page owner directory");
                }
                if prev == 0 {
                    self.slab_head = next;
                } else {
                    slab_node(prev).next.store(next, Ordering::Relaxed);
                }
                if self.preferred_slab == current {
                    self.preferred_slab = next;
                }
                self.slab_count = self.slab_count.saturating_sub(1);
                self.stats.active_slabs = self.slab_count;
                self.stats.active_pages = self
                    .stats
                    .active_pages
                    .saturating_sub(backing.size / PAGE_SIZE);
                self.stats.reclaimed_slabs += 1;
                return Some((backing, current));
            }
            prev = current;
            current = next;
        }
        None
    }

    fn audit(&self, obj_size: usize) -> SlabAudit {
        // 这里做的是 slab 自身结构审计，不修复状态。链表遍历都以当前计数器为上界，
        // 这样即使链表被破坏成环，也只会报告错误，不会在诊断路径中死循环。
        let mut audit = SlabAudit::default();
        let mut node_addr = self.slab_head;

        while node_addr != 0 {
            if audit.scanned_slabs >= self.slab_count {
                audit.flags.insert(SlabAuditFlags::SLAB_CHAIN_LOOP);
                break;
            }

            let node = slab_node(node_addr);
            let backing = node.backing.load();
            audit.scanned_slabs += 1;
            audit_slab(node, obj_size, &mut audit);
            if self.lookup_slab_node(node.slab.base_addr(), backing.size) != Some(node_addr) {
                audit.flags.insert(SlabAuditFlags::SLAB_LOOKUP_MISMATCH);
            }
            node_addr = node.next.load(Ordering::Relaxed);
        }

        let mut lookup_nodes = 0usize;
        for (bucket, &head) in self.slab_lookup.iter().enumerate() {
            let mut lookup_node = head;
            let mut bucket_nodes = 0usize;
            while lookup_node != 0 {
                if bucket_nodes >= self.slab_count {
                    audit.flags.insert(SlabAuditFlags::SLAB_LOOKUP_MISMATCH);
                    break;
                }

                let node = slab_node(lookup_node);
                let backing = node.backing.load();
                if !node.slab.active.load(Ordering::Acquire)
                    || slab_lookup_bucket(node.slab.base_addr(), backing.size) != bucket
                    || !self.contains_active_node(lookup_node)
                {
                    audit.flags.insert(SlabAuditFlags::SLAB_LOOKUP_MISMATCH);
                }
                lookup_nodes = lookup_nodes.saturating_add(1);
                bucket_nodes += 1;
                lookup_node = node.lookup_next.load(Ordering::Relaxed);
            }
        }
        if lookup_nodes != self.slab_count {
            audit.flags.insert(SlabAuditFlags::SLAB_LOOKUP_MISMATCH);
        }

        let mut free_node_addr = self.free_node_head;
        let mut scanned_reusable = 0usize;
        while free_node_addr != 0 {
            if scanned_reusable >= self.free_node_count {
                audit.flags.insert(SlabAuditFlags::FREE_NODE_LOOP);
                break;
            }

            let node = slab_node(free_node_addr);
            if node.slab.active.load(Ordering::Acquire) {
                audit.flags.insert(SlabAuditFlags::INVALID_SLAB_RANGE);
            }
            scanned_reusable += 1;
            audit.scanned_free_nodes += 1;
            free_node_addr = node.next.load(Ordering::Relaxed);
        }

        let mut retired_node_addr = self.retired_node_head;
        let mut scanned_retired = 0usize;
        while retired_node_addr != 0 {
            if scanned_retired >= self.retired_node_count {
                audit.flags.insert(SlabAuditFlags::FREE_NODE_LOOP);
                break;
            }

            let node = slab_node(retired_node_addr);
            if node.slab.active.load(Ordering::Acquire)
                || node.lifecycle.load(Ordering::Acquire) & SLAB_NODE_RETIRED == 0
            {
                audit.flags.insert(SlabAuditFlags::INVALID_SLAB_RANGE);
            }
            scanned_retired += 1;
            audit.scanned_free_nodes += 1;
            retired_node_addr = node.next.load(Ordering::Relaxed);
        }

        if audit.scanned_slabs != self.slab_count
            || audit.scanned_slabs != self.stats.active_slabs
            || audit.scanned_active_pages != self.stats.active_pages
            || audit.scanned_free_nodes
                != self.free_node_count.saturating_add(self.retired_node_count)
            || audit.scanned_free_nodes != self.stats.free_slab_nodes
        {
            audit.flags.insert(SlabAuditFlags::STATS_MISMATCH);
        }

        audit
    }

    fn contains_active_node(&self, node_addr: usize) -> bool {
        let mut current = self.slab_head;
        let mut visited = 0usize;
        while current != 0 && visited < self.slab_count {
            if current == node_addr {
                return true;
            }
            current = slab_node(current).next.load(Ordering::Relaxed);
            visited += 1;
        }
        false
    }

    fn cached_entry_is_reserved(&self, entry: CacheEntry, obj_size: usize) -> bool {
        if entry.slab_node == INVALID_SLAB_NODE || !self.contains_active_node(entry.slab_node) {
            return false;
        }
        let state = slab_node(entry.slab_node)
            .slab
            .object_state(entry.ptr, obj_size);
        matches!(
            (entry.cached_index != INVALID_CACHED_INDEX, state),
            (true, Some(SlabObjectState::Cached)) | (false, Some(SlabObjectState::Allocated))
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum SlabGrowError {
    Metadata,
    BackedRange,
    UnsupportedOrder,
    InvalidBacking,
    Inactive,
}

fn allocate_slab_node(
    obj_size: usize,
    pages_per_slab: usize,
    arena: ArenaKind,
    phys: &Mutex<BuddyAllocator>,
    vmem: &KernelAddressSpace,
    reusable_node: Option<usize>,
) -> Result<usize, SlabGrowError> {
    let order = pages_to_order(pages_per_slab).ok_or(SlabGrowError::UnsupportedOrder)?;
    let range = vmem
        .alloc_backed_range(arena, order, crate::PagePolicy::BaseOnly, phys)
        .map_err(|_| SlabGrowError::BackedRange)?;
    let block_pages = pages_for_order(order).ok_or(SlabGrowError::UnsupportedOrder)?;
    if range.paddr & (PAGE_SIZE - 1) != 0 {
        let _ = vmem.free_backed_range(range, phys);
        return Err(SlabGrowError::InvalidBacking);
    }

    let node_addr = match reusable_node {
        Some(node_addr) => node_addr,
        None => {
            let node_addr = crate::alloc_internal_metadata(Layout::new::<SlabNode>()) as usize;
            if node_addr == 0 {
                let _ = vmem.free_backed_range(range, phys);
                return Err(SlabGrowError::Metadata);
            }
            // Safety: metadata allocator 返回满足 `SlabNode` Layout 的未发布存储。
            unsafe { (node_addr as *mut SlabNode).write(SlabNode::empty()) };
            node_addr
        }
    };

    let node = slab_node(node_addr);
    node.prepare(range, block_pages, obj_size);
    if node.slab.total_objects() == 0 {
        let _ = vmem.free_backed_range(range, phys);
        return Err(SlabGrowError::Inactive);
    }

    Ok(node_addr)
}

struct Zone {
    size_class: usize,
    pages_per_slab: usize,
    arena: ArenaKind,
    state: Mutex<ZoneState>,
    caches: [PerCpuCache; MAX_CPUS],
}

impl Zone {
    const fn new(size_class: usize, arena: ArenaKind) -> Self {
        Self {
            size_class,
            pages_per_slab: pages_per_slab(size_class),
            arena,
            state: Mutex::new(ZoneState::new()),
            caches: [const { PerCpuCache::new() }; MAX_CPUS],
        }
    }

    fn alloc(
        &self,
        cpu: usize,
        phys: &Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
        directory: &SlabPageDirectory,
    ) -> SlabAllocation {
        // Zone 的热路径只获取一次 CPU 本地 cache 锁。命中时直接返回 entry；miss
        // 才进入 ZoneState，一次批量取得当前对象和后续 magazine 补货。
        let cache = &self.caches[cpu];
        let cache_entry = {
            let mut cache_guard = cache.inner.lock();
            cache_guard.pop_for_alloc()
        };
        if let Some(entry) = cache_entry {
            if entry.cached_index != INVALID_CACHED_INDEX && !ZoneState::mark_allocated(entry) {
                panic!("[alloc][invariant] cached slab object lost reservation");
            }
            return SlabAllocation {
                ptr: entry.ptr,
                backend_cookie: slab_node(entry.slab_node).backend_cookie(),
            };
        }

        let mut batch = [CacheEntry::empty(); REFILL_BATCH + 1];
        let mut produced = {
            let mut state = self.state.lock();
            state.allocate_batch(self.size_class, &mut batch)
        };
        if produced == 0 {
            let mut grow_attempts = 0;
            while produced == 0 {
                if grow_attempts >= MAX_GROW_ATTEMPTS {
                    let mut state = self.state.lock();
                    state.stats.grow_failures += 1;
                    drop(state);
                    self.reclaim_empty_slabs(Some((phys, vmem)), directory);
                    return SlabAllocation::null();
                }
                let reusable_node = {
                    let mut state = self.state.lock();
                    state.pop_reusable_slab_node()
                };
                match allocate_slab_node(
                    self.size_class,
                    self.pages_per_slab,
                    self.arena,
                    phys,
                    vmem,
                    reusable_node,
                ) {
                    Ok(node_addr) => {
                        let node = slab_node(node_addr);
                        let backing = node.backing.load();
                        if !directory.ensure_range(backing.vaddr, backing.size) {
                            let range = backing;
                            if vmem.free_backed_range(range, phys).is_err() {
                                panic!("[alloc][invariant] slab directory grow rollback failed");
                            }
                            let mut state = self.state.lock();
                            state.push_reusable_slab_node(node_addr);
                            state.note_grow_failure(SlabGrowError::Metadata);
                            return SlabAllocation::null();
                        }
                        let mut state = self.state.lock();
                        state.insert_slab_node(node_addr);
                        if !directory.publish_range(node.slab.base_addr(), backing.size, node_addr)
                        {
                            panic!("[alloc][invariant] slab page owner directory overlap");
                        }
                        if !node.activate() {
                            panic!("[alloc][invariant] published slab cannot become active");
                        }
                        produced = state.allocate_batch(self.size_class, &mut batch);
                    }
                    Err(err) => {
                        let mut state = self.state.lock();
                        if let Some(node_addr) = reusable_node {
                            state.push_reusable_slab_node(node_addr);
                        }
                        state.note_grow_failure(err);
                        return SlabAllocation::null();
                    }
                }
                grow_attempts += 1;
            }
        }

        let current = batch[0];
        let mut overflow = [CacheEntry::empty(); REFILL_BATCH];
        let overflow_count = {
            let mut cache_guard = cache.inner.lock();
            let overflow_count = cache_guard.push_refill(&batch[1..produced], &mut overflow);
            cache_guard.note_slow_allocation();
            overflow_count
        };
        if overflow_count != 0 {
            let mut state = self.state.lock();
            state.flush_cached_entries(&overflow[..overflow_count], self.size_class);
        }
        SlabAllocation {
            ptr: current.ptr,
            backend_cookie: slab_node(current.slab_node).backend_cookie(),
        }
    }

    fn free(&self, ptr: usize, cpu: usize, directory: &SlabPageDirectory) -> bool {
        // GlobalAlloc 只提供 ptr + Layout，不能使用 registry cookie。页 owner 目录以
        // 固定三层寻址取得节点，再用对象状态 CAS 完成 ALLOCATED -> CACHED；正常释放
        // 路径不再获取 ZoneState 锁，也不再遍历 size class 或哈希冲突链。
        let slab_span = self.pages_per_slab.saturating_mul(PAGE_SIZE);
        let Some(owner) = directory.lookup(ptr) else {
            return false;
        };
        let node_addr = owner.node_addr();
        let node = owner.node();
        if node.slab.object_size() != self.size_class || node.slab.span_size() != Some(slab_span) {
            return false;
        }
        let Some(cached_index) = node.slab.mark_cached(ptr, self.size_class) else {
            return false;
        };
        drop(owner);
        let entry = CacheEntry {
            ptr,
            slab_node: node_addr,
            cached_index,
        };
        self.enqueue_free(entry, cpu, true)
    }

    fn free_with_hint(
        &self,
        ptr: usize,
        cpu: usize,
        backend_cookie_hint: usize,
        directory: &SlabPageDirectory,
    ) -> bool {
        if backend_cookie_hint == 0 {
            return false;
        }

        // cookie 只作为 owner 身份提示；仍通过页目录固定当前节点生命周期，避免节点
        // 回收复用后把陈旧 cookie 解释成新的 slab。
        let Some(owner) = directory.lookup(ptr) else {
            return false;
        };
        let node_addr = owner.node_addr();
        if owner.node().backend_cookie() != backend_cookie_hint {
            return false;
        }
        let Some(cached_index) = owner.node().slab.mark_cached(ptr, self.size_class) else {
            return false;
        };
        drop(owner);
        let entry = CacheEntry {
            ptr,
            slab_node: node_addr,
            cached_index,
        };
        self.enqueue_free(entry, cpu, true)
    }

    fn enqueue_free(&self, entry: CacheEntry, cpu: usize, used_hint: bool) -> bool {
        let cache = &self.caches[cpu];
        let mut drained = CacheDrainBuffer::<FLUSH_BATCH>::new();
        let drained_count = {
            let mut cache_guard = cache.inner.lock();
            cache_guard.push_for_free(entry, &mut drained, used_hint)
        };
        if drained_count != 0 {
            let mut state = self.state.lock();
            state.flush_cached_entries(drained.initialized(), self.size_class);
        }
        true
    }

    fn flush_cpu_caches(&self, cpu_count: usize) -> SlabReclaimStats {
        let mut out = SlabReclaimStats::default();
        for cpu in 0..cpu_count.min(MAX_CPUS) {
            let mut drained = [CacheEntry::empty(); CACHE_CAPACITY];
            let drained_count = {
                let mut cache = self.caches[cpu].inner.lock();
                let drained_count = cache.drain_into(&mut drained);
                if drained_count != 0 {
                    cache.stats.flushes = cache.stats.flushes.saturating_add(1);
                }
                drained_count
            };
            if drained_count == 0 {
                continue;
            }
            let result = {
                let mut state = self.state.lock();
                state.flush_cached_entries(&drained[..drained_count], self.size_class)
            };
            out.flushed_cached_objects = out.flushed_cached_objects.saturating_add(result.flushed);
        }
        out
    }

    fn reclaim_empty_slabs(
        &self,
        backing: Option<(&Mutex<BuddyAllocator>, &KernelAddressSpace)>,
        directory: &SlabPageDirectory,
    ) -> SlabReclaimStats {
        let mut out = SlabReclaimStats::default();
        let Some((phys, vmem)) = backing else {
            return out;
        };
        loop {
            let retired = {
                let mut state = self.state.lock();
                state.take_reclaimable_empty_slab(directory)
            };
            let Some((range, node_addr)) = retired else {
                break;
            };
            if let Err(err) = vmem.free_backed_range(range, phys) {
                panic!(
                    "[alloc][invariant] slab empty range reclaim failed class={} vaddr={:#x} paddr={:#x} size={} err={:?}",
                    self.size_class, range.vaddr, range.paddr, range.size, err
                );
            }
            self.state.lock().enqueue_retired_slab_node(node_addr);
            out.reclaimed_slabs += 1;
            out.reclaimed_pages = out.reclaimed_pages.saturating_add(range.size / PAGE_SIZE);
            out.reclaimed_bytes = out.reclaimed_bytes.saturating_add(range.size);
        }
        out
    }

    fn snapshot(&self) -> SlabStats {
        let mut out = self.state.lock().stats;
        out.alloc_requests = 0;
        out.free_requests = 0;
        out.cache_hits = 0;
        out.cache_misses = 0;
        out.cache_refills = 0;
        out.cache_flushes = 0;
        out.fast_free_hits = 0;
        out.fast_free_fallbacks = 0;
        let mut successful_allocations = 0u64;
        let mut successful_frees = 0u64;
        for cache in &self.caches {
            let stats = cache.inner.lock().stats;
            out.alloc_requests = out.alloc_requests.saturating_add(stats.alloc_requests);
            out.free_requests = out.free_requests.saturating_add(stats.free_requests);
            successful_allocations =
                successful_allocations.saturating_add(stats.successful_allocations);
            successful_frees = successful_frees.saturating_add(stats.successful_frees);
            out.cache_hits = out.cache_hits.saturating_add(stats.cache_hits);
            out.cache_misses = out.cache_misses.saturating_add(stats.cache_misses);
            out.cache_refills = out.cache_refills.saturating_add(stats.refills);
            out.cache_flushes = out.cache_flushes.saturating_add(stats.flushes);
            out.fast_free_hits = out.fast_free_hits.saturating_add(stats.fast_free_hits);
            out.fast_free_fallbacks = out
                .fast_free_fallbacks
                .saturating_add(stats.fast_free_fallbacks);
        }
        // 请求计数保留失败尝试用于诊断；存量只能由成功的生命周期事件推导。
        out.active_objects = successful_allocations.saturating_sub(successful_frees);
        out.active_bytes = (out.active_objects as usize).saturating_mul(self.size_class);
        out
    }

    fn class_stat(&self) -> SlabClassStat {
        let stats = self.snapshot();
        let state = self.state.lock();
        let mut empty_slabs = 0usize;
        let mut empty_pages = 0usize;
        let mut node_addr = state.slab_head;
        while node_addr != 0 {
            let node = slab_node(node_addr);
            if node.slab.is_empty() {
                empty_slabs = empty_slabs.saturating_add(1);
                empty_pages = empty_pages.saturating_add(node.backing.load().size / PAGE_SIZE);
            }
            node_addr = node.next.load(Ordering::Relaxed);
        }
        SlabClassStat {
            size_class: self.size_class,
            active_objects: stats.active_objects,
            active_bytes: stats.active_bytes,
            active_slabs: state.stats.active_slabs,
            active_pages: state.stats.active_pages,
            empty_slabs,
            empty_pages,
            reclaimable_empty_pages: empty_slabs
                .saturating_sub(MAX_EMPTY_SLABS_PER_ZONE)
                .saturating_mul(self.pages_per_slab),
            free_slab_nodes: state.stats.free_slab_nodes,
        }
    }

    fn audit(&self, directory: &SlabPageDirectory) -> SlabAudit {
        let mut latest = SlabAudit::default();
        for _ in 0..4 {
            latest = self.audit_once(directory);
            if latest.is_consistent() {
                break;
            }
            core::hint::spin_loop();
        }
        latest
    }

    fn audit_once(&self, directory: &SlabPageDirectory) -> SlabAudit {
        // 遵循 allocator 的 state -> cache 锁序，同时固定全部 magazine，使 entry 和
        // 普通计数来自同一时点。refill/flush 的交接窗口由 audit() 有限重试吸收。
        let state = self.state.lock();
        let caches = self.caches.each_ref().map(|cache| cache.inner.lock());
        let mut audit = state.audit(self.size_class);
        let mut node_addr = state.slab_head;
        while node_addr != INVALID_SLAB_NODE {
            let node = slab_node(node_addr);
            let backing = node.backing.load();
            if !directory.owns_range(backing.vaddr, backing.size, node_addr) {
                audit.flags.insert(SlabAuditFlags::SLAB_DIRECTORY_MISMATCH);
            }
            node_addr = node.next.load(Ordering::Relaxed);
        }
        let tracked_cached_bits = audit.scanned_cached_objects;
        let mut cached = 0usize;
        let mut tracked_cached = 0usize;
        let mut successful_allocations = 0u64;
        let mut successful_frees = 0u64;
        for (cpu, cache) in caches.iter().enumerate() {
            successful_allocations =
                successful_allocations.saturating_add(cache.stats.successful_allocations);
            successful_frees = successful_frees.saturating_add(cache.stats.successful_frees);
            for (slot, &entry) in cache.entries[..cache.count].iter().enumerate() {
                let duplicate_local = cache.entries[..slot]
                    .iter()
                    .any(|previous| previous.ptr == entry.ptr);
                let duplicate_remote = caches[..cpu].iter().any(|previous| {
                    previous.entries[..previous.count]
                        .iter()
                        .any(|candidate| candidate.ptr == entry.ptr)
                });
                let duplicate = duplicate_local || duplicate_remote;
                if !duplicate && state.cached_entry_is_reserved(entry, self.size_class) {
                    cached += 1;
                    tracked_cached += usize::from(entry.cached_index != INVALID_CACHED_INDEX);
                } else {
                    audit.flags.insert(SlabAuditFlags::CACHE_WITHOUT_ALLOC);
                }
            }
        }
        if tracked_cached_bits != tracked_cached as u64 {
            audit.flags.insert(SlabAuditFlags::CACHE_COUNT_MISMATCH);
        }
        audit.scanned_cached_objects = cached as u64;
        audit.scanned_active_objects = audit.scanned_active_objects.saturating_sub(cached as u64);
        audit.scanned_active_bytes = audit
            .scanned_active_bytes
            .saturating_sub(cached.saturating_mul(self.size_class));
        let active_objects = successful_allocations.saturating_sub(successful_frees);
        let active_bytes = (active_objects as usize).saturating_mul(self.size_class);
        if audit.scanned_active_objects != active_objects
            || audit.scanned_active_bytes != active_bytes
        {
            audit.flags.insert(SlabAuditFlags::STATS_MISMATCH);
        }
        audit
    }
}

pub struct SlabAllocator {
    zones: [Zone; SIZE_CLASS_COUNT],
    directory: SlabPageDirectory,
    cpu_count: AtomicUsize,
    initialized: AtomicBool,
}

impl SlabAllocator {
    pub const fn new(arena: ArenaKind) -> Self {
        Self {
            zones: [
                Zone::new(SIZE_CLASSES[0], arena),
                Zone::new(SIZE_CLASSES[1], arena),
                Zone::new(SIZE_CLASSES[2], arena),
                Zone::new(SIZE_CLASSES[3], arena),
                Zone::new(SIZE_CLASSES[4], arena),
                Zone::new(SIZE_CLASSES[5], arena),
                Zone::new(SIZE_CLASSES[6], arena),
                Zone::new(SIZE_CLASSES[7], arena),
                Zone::new(SIZE_CLASSES[8], arena),
                Zone::new(SIZE_CLASSES[9], arena),
                Zone::new(SIZE_CLASSES[10], arena),
                Zone::new(SIZE_CLASSES[11], arena),
                Zone::new(SIZE_CLASSES[12], arena),
                Zone::new(SIZE_CLASSES[13], arena),
            ],
            directory: SlabPageDirectory::new(),
            cpu_count: AtomicUsize::new(1),
            initialized: AtomicBool::new(false),
        }
    }

    pub fn init(&self, cpu_count: usize, heap_region: (usize, usize)) {
        self.cpu_count
            .store(cpu_count.clamp(1, MAX_CPUS), Ordering::Release);
        assert!(
            self.directory.init(heap_region),
            "[alloc][invariant] slab page owner directory requires a valid heap window"
        );
        self.initialized.store(true, Ordering::Release);
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn class_index_for(layout: Layout) -> Option<usize> {
        let aligned = layout.pad_to_align();
        let size = aligned.size();
        let align = aligned.align();
        if align > PAGE_SIZE {
            return None;
        }
        let start = class_index_for_size(size)?;
        for (idx, class) in SIZE_CLASSES.iter().enumerate().skip(start) {
            if class.is_multiple_of(align) {
                return Some(idx);
            }
        }
        None
    }

    pub fn zone_size_class(&self, zone_idx: usize) -> usize {
        self.zones[zone_idx].size_class
    }

    pub fn alloc(
        &self,
        layout: Layout,
        cpu_id: usize,
        phys: &Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> *mut u8 {
        // `SlabAllocator` 自己不处理对象位图细节，它更像总调度器：根据 layout 选择
        // size class，再把请求转交给对应 zone。
        if !self.is_initialized() {
            return null_mut();
        }
        let Some(zone_idx) = Self::class_index_for(layout) else {
            return null_mut();
        };
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx]
            .alloc(cpu, phys, vmem, &self.directory)
            .ptr as *mut u8
    }

    pub(crate) fn alloc_class(
        &self,
        zone_idx: usize,
        cpu_id: usize,
        phys: &Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> SlabAllocation {
        // 上层路由已经根据 Layout 做过 size-class 与对齐判定时，直接进入目标 zone，
        // 避免在 GlobalAlloc 热路径里重复计算一次 class_index_for(layout)。
        if !self.is_initialized() || zone_idx >= self.zones.len() {
            return SlabAllocation::null();
        }
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].alloc(cpu, phys, vmem, &self.directory)
    }

    pub fn free(&self, ptr: usize, layout: Layout, cpu_id: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let Some(zone_idx) = Self::class_index_for(layout) else {
            return false;
        };
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].free(ptr, cpu, &self.directory)
    }

    pub(crate) fn free_class(&self, ptr: usize, zone_idx: usize, cpu_id: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        if zone_idx >= self.zones.len() {
            return false;
        }
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].free(ptr, cpu, &self.directory)
    }

    pub fn same_size_class(old_layout: Layout, new_layout: Layout) -> bool {
        Self::class_index_for(old_layout) == Self::class_index_for(new_layout)
    }

    pub fn owns(&self, ptr: usize) -> bool {
        self.is_initialized() && self.zone_index_for_ptr(ptr).is_some()
    }

    pub(crate) fn owns_in_class(&self, zone_idx: usize, ptr: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let Some(zone) = self.zones.get(zone_idx) else {
            return false;
        };
        self.directory
            .lookup(ptr)
            .is_some_and(|owner| owner.node().slab.object_size() == zone.size_class)
    }

    pub fn free_ptr(&self, ptr: usize, cpu_id: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let Some(zone_idx) = self.zone_index_for_ptr(ptr) else {
            return false;
        };
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].free(ptr, cpu, &self.directory)
    }

    pub(crate) fn free_record(&self, record: AllocationRecord, cpu_id: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        if !matches!(record.kind, AllocationKind::Small)
            || !matches!(record.domain, MemoryDomain::Kernel)
            || record.ptr == 0
            || record.backend_cookie == 0
            || record.size == 0
            || record.size > record.usable_size
        {
            return false;
        }
        let Ok(layout) = Layout::from_size_align(record.size, record.align) else {
            return false;
        };
        let Some(zone_idx) = Self::class_index_for(layout) else {
            return false;
        };
        let zone = &self.zones[zone_idx];
        let expected_arena = match zone.arena {
            ArenaKind::DirectMap => AllocationArena::DirectMap,
            ArenaKind::Kernel => AllocationArena::Kernel,
            ArenaKind::Tracked => AllocationArena::Tracked,
        };
        if record.arena != Some(expected_arena)
            || record.usable_size != zone.size_class
            || !record.ptr.is_multiple_of(record.align)
        {
            return false;
        }
        let cpu = self.normalize_cpu(cpu_id);
        zone.free_with_hint(record.ptr, cpu, record.backend_cookie, &self.directory)
    }

    pub fn usable_size_for_ptr(&self, ptr: usize) -> Option<usize> {
        if !self.is_initialized() {
            return None;
        }
        self.zone_index_for_ptr(ptr)
            .map(|zone_idx| self.zones[zone_idx].size_class)
    }

    pub fn snapshot(&self) -> SlabStats {
        let mut out = SlabStats::default();
        for zone in &self.zones {
            let stats = zone.snapshot();
            out.alloc_requests += stats.alloc_requests;
            out.free_requests += stats.free_requests;
            out.cache_hits += stats.cache_hits;
            out.cache_misses += stats.cache_misses;
            out.grow_failures += stats.grow_failures;
            out.active_objects += stats.active_objects;
            out.active_slabs += stats.active_slabs;
            out.active_pages += stats.active_pages;
            out.active_bytes += stats.active_bytes;
            out.address_reservation_failures += stats.address_reservation_failures;
            out.invalid_frees += stats.invalid_frees;
            out.cache_refills += stats.cache_refills;
            out.cache_flushes += stats.cache_flushes;
            out.fast_free_hits += stats.fast_free_hits;
            out.fast_free_fallbacks += stats.fast_free_fallbacks;
            out.reclaimed_slabs += stats.reclaimed_slabs;
            out.free_slab_nodes += stats.free_slab_nodes;
        }
        out
    }

    pub fn class_stats(&self) -> [SlabClassStat; SIZE_CLASS_COUNT] {
        core::array::from_fn(|idx| self.zones[idx].class_stat())
    }

    #[cfg(feature = "performance-profile")]
    pub fn profile_counter(&self, cpu: usize, counter: SlabProfileCounter) -> u64 {
        if cpu >= MAX_CPUS {
            return 0;
        }
        self.zones
            .iter()
            .map(|zone| {
                let stats = zone.caches[cpu].inner.lock().stats;
                match counter {
                    SlabProfileCounter::CacheHit => stats.cache_hits,
                    SlabProfileCounter::CacheMiss => stats.cache_misses,
                    SlabProfileCounter::Refill => stats.refills,
                    SlabProfileCounter::Flush => stats.flushes,
                    SlabProfileCounter::SlowPath => stats.slow_paths,
                }
            })
            .sum()
    }

    pub fn reclaim(
        &self,
        flush_cpu_caches: bool,
        reclaim_empty_slabs: bool,
        phys: &Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> SlabReclaimStats {
        let mut out = SlabReclaimStats::default();
        if !self.is_initialized() {
            return out;
        }

        let cpu_count = self.cpu_count.load(Ordering::Acquire).clamp(1, MAX_CPUS);
        for zone in &self.zones {
            if flush_cpu_caches {
                let stats = zone.flush_cpu_caches(cpu_count);
                out.flushed_cached_objects = out
                    .flushed_cached_objects
                    .saturating_add(stats.flushed_cached_objects);
            }
            if reclaim_empty_slabs {
                let stats = zone.reclaim_empty_slabs(Some((phys, vmem)), &self.directory);
                out.reclaimed_slabs = out.reclaimed_slabs.saturating_add(stats.reclaimed_slabs);
                out.reclaimed_pages = out.reclaimed_pages.saturating_add(stats.reclaimed_pages);
                out.reclaimed_bytes = out.reclaimed_bytes.saturating_add(stats.reclaimed_bytes);
            }
        }
        out
    }

    pub fn audit(&self) -> SlabAudit {
        let mut out = SlabAudit::default();
        if !self.is_initialized() {
            return out;
        }

        for zone in &self.zones {
            out.merge(zone.audit(&self.directory));
        }
        out
    }

    fn normalize_cpu(&self, cpu_id: usize) -> usize {
        let limit = self.cpu_count.load(Ordering::Acquire).clamp(1, MAX_CPUS);
        cpu_id.min(limit - 1)
    }

    fn zone_index_for_ptr(&self, ptr: usize) -> Option<usize> {
        let owner = self.directory.lookup(ptr)?;
        let slab = &owner.node().slab;
        let size_class = slab.object_size();
        if slab.object_state(ptr, size_class) != Some(SlabObjectState::Allocated) {
            return None;
        }
        class_index_for_size(size_class)
    }
}

impl Default for SlabAllocator {
    fn default() -> Self {
        Self::new(ArenaKind::Kernel)
    }
}

const fn pages_per_slab(size_class: usize) -> usize {
    // 这里先按“约 32 个对象”估算 slab 容量，再向上对齐到 2 的幂页数，使 slab 的页需求
    // 能自然映射到 buddy 的 order 语义，减少底层页分配的额外碎片。
    let target_objects = 32usize;
    let min_pages = (size_class * target_objects).div_ceil(PAGE_SIZE);
    let min_pages = if min_pages == 0 { 1 } else { min_pages };
    min_pages.next_power_of_two()
}

/// 按大小把请求路由到最近可容纳的 size class。
///
/// 这是 slab 把“任意小对象请求”归一化成有限离散尺寸集合的关键步骤。
fn class_index_for_size(size: usize) -> Option<usize> {
    match size {
        0..=8 => Some(0),
        9..=16 => Some(1),
        17..=32 => Some(2),
        33..=64 => Some(3),
        65..=96 => Some(4),
        97..=128 => Some(5),
        129..=192 => Some(6),
        193..=256 => Some(7),
        257..=384 => Some(8),
        385..=512 => Some(9),
        513..=768 => Some(10),
        769..=1024 => Some(11),
        1025..=1536 => Some(12),
        1537..=2048 => Some(13),
        _ => None,
    }
}

#[inline]
fn slab_node(addr: usize) -> &'static SlabNode {
    // Safety: 非零地址只来自满足 Layout 的 metadata allocator 或测试中的存活 Box；节点
    // 存储永不释放，生命周期切换由 `SlabNode::lifecycle` 固定。
    unsafe { &*(addr as *const SlabNode) }
}

#[inline]
fn find_free_state_in_range(
    states: &[AtomicU64; STATE_WORDS],
    start: usize,
    end: usize,
) -> Option<usize> {
    const PAIR_LOW_BITS: u64 = 0x5555_5555_5555_5555;
    let end = end.min(BITMAP_WORDS * 64);
    if start >= end {
        return None;
    }
    let mut word = start / OBJECTS_PER_STATE_WORD;
    while word < STATE_WORDS {
        let word_start = word * OBJECTS_PER_STATE_WORD;
        if word_start >= end {
            break;
        }
        let from = start.saturating_sub(word_start).min(OBJECTS_PER_STATE_WORD);
        let to = (end - word_start).min(OBJECTS_PER_STATE_WORD);
        let raw = states[word].load(Ordering::Relaxed);
        let free_low_bits = !(raw | (raw >> 1)) & PAIR_LOW_BITS;
        let high_mask = if to == OBJECTS_PER_STATE_WORD {
            PAIR_LOW_BITS
        } else {
            ((1u64 << (to * OBJECT_STATE_BITS)) - 1) & PAIR_LOW_BITS
        };
        let low_mask = if from == 0 {
            0
        } else {
            ((1u64 << (from * OBJECT_STATE_BITS)) - 1) & PAIR_LOW_BITS
        };
        let available = free_low_bits & high_mask & !low_mask;
        if available != 0 {
            return Some(word_start + available.trailing_zeros() as usize / OBJECT_STATE_BITS);
        }
        word += 1;
    }
    None
}

#[inline]
fn slab_lookup_bucket(slab_base: usize, slab_span: usize) -> usize {
    debug_assert!(slab_span.is_power_of_two());
    let slot = slab_base / slab_span;
    slot.wrapping_mul(0x9e37_79b9_7f4a_7c15) & (SLAB_LOOKUP_BUCKETS - 1)
}

fn audit_slab(node: &SlabNode, obj_size: usize, audit: &mut SlabAudit) {
    let slab = &node.slab;
    let backing = node.backing.load();
    let total = slab.total_objects();
    let page_count = slab.page_count();
    let base_addr = slab.base_addr();
    let paddr = slab.paddr();
    let expected_objects = page_count
        .checked_mul(PAGE_SIZE)
        .map(|span| span / obj_size)
        .unwrap_or(0)
        .min(BITMAP_WORDS * 64);

    if !slab.active.load(Ordering::Acquire)
        || base_addr == 0
        || page_count == 0
        || slab.object_size() != obj_size
        || total == 0
        || total > BITMAP_WORDS * 64
        || expected_objects != total
        || backing.vaddr != base_addr
        || backing.paddr != paddr
        || backing.size != page_count.saturating_mul(PAGE_SIZE)
        || !base_addr.is_multiple_of(PAGE_SIZE)
        || !paddr.is_multiple_of(PAGE_SIZE)
    {
        audit.flags.insert(SlabAuditFlags::INVALID_SLAB_RANGE);
    }

    let (allocated, cached, invalid_states) = count_object_states(slab, total);
    if allocated != slab.allocated_objects() || invalid_states != 0 {
        audit.flags.insert(SlabAuditFlags::OBJECT_COUNT_MISMATCH);
    }
    if has_object_states_outside_range(slab, total) {
        audit.flags.insert(SlabAuditFlags::UNUSED_BITS_SET);
    }

    audit.scanned_active_objects = audit
        .scanned_active_objects
        .saturating_add(allocated as u64);
    audit.scanned_cached_objects = audit.scanned_cached_objects.saturating_add(cached as u64);
    audit.scanned_active_pages = audit.scanned_active_pages.saturating_add(page_count);
    audit.scanned_active_bytes = audit
        .scanned_active_bytes
        .saturating_add(allocated.saturating_mul(obj_size));
}

fn count_object_states(slab: &Slab, end: usize) -> (usize, usize, usize) {
    let end = end.min(BITMAP_WORDS * 64);
    let mut allocated = 0usize;
    let mut cached = 0usize;
    let mut invalid = 0usize;
    for idx in 0..end {
        match slab.load_state(idx) {
            OBJECT_FREE => {}
            OBJECT_ALLOCATED => allocated += 1,
            OBJECT_CACHED => {
                allocated += 1;
                cached += 1;
            }
            _ => invalid += 1,
        }
    }
    (allocated, cached, invalid)
}

fn has_object_states_outside_range(slab: &Slab, end: usize) -> bool {
    ((end.min(BITMAP_WORDS * 64))..(BITMAP_WORDS * 64))
        .any(|idx| slab.load_state(idx) != OBJECT_FREE)
}

#[cfg(test)]
mod slab_state_tests {
    extern crate alloc;
    extern crate std;

    use alloc::boxed::Box;
    use core::sync::atomic::Ordering;

    use super::{
        CACHE_CAPACITY, CacheDrainBuffer, CacheEntry, FLUSH_BATCH, INVALID_SLAB_NODE,
        PerCpuCacheState, SLAB_DIRECTORY_BITS, SLAB_DIRECTORY_MASK, Slab, SlabAllocator,
        SlabAuditFlags, SlabDirectoryPage, SlabNode, SlabObjectState, SlabPageDirectory, ZoneState,
        directory_page, slab_lookup_bucket,
    };
    use crate::buddy::PAGE_SIZE;
    use crate::space::{ArenaKind, BackedRange};

    fn test_slab_node(base: usize) -> Box<SlabNode> {
        test_slab_node_with_pages(base, 1)
    }

    fn test_slab_node_with_pages(base: usize, pages: usize) -> Box<SlabNode> {
        let node = Box::new(SlabNode::empty());
        node.prepare(
            BackedRange {
                arena: ArenaKind::Kernel,
                vaddr: base,
                paddr: base,
                size: pages * PAGE_SIZE,
                order: pages.trailing_zeros() as usize,
            },
            pages,
            64,
        );
        assert!(node.activate());
        node
    }

    fn prepare_test_directory_page(directory: &SlabPageDirectory, page: usize) {
        let root_slot = (page >> (SLAB_DIRECTORY_BITS * 2)) & SLAB_DIRECTORY_MASK;
        let middle_slot = (page >> SLAB_DIRECTORY_BITS) & SLAB_DIRECTORY_MASK;
        let middle = match directory_page(
            directory.root.entries[root_slot].load(core::sync::atomic::Ordering::Acquire),
        ) {
            Some(page) => page,
            None => {
                let page = Box::leak(Box::new(SlabDirectoryPage::new()));
                directory.root.entries[root_slot].store(
                    page as *mut SlabDirectoryPage as usize,
                    core::sync::atomic::Ordering::Release,
                );
                page
            }
        };
        if directory_page(middle.entries[middle_slot].load(core::sync::atomic::Ordering::Acquire))
            .is_none()
        {
            let leaf = Box::leak(Box::new(SlabDirectoryPage::new()));
            middle.entries[middle_slot].store(
                leaf as *mut SlabDirectoryPage as usize,
                core::sync::atomic::Ordering::Release,
            );
        }
    }

    fn test_cache_entry(ptr: usize) -> CacheEntry {
        CacheEntry {
            ptr,
            slab_node: ptr + PAGE_SIZE,
            cached_index: ptr as u16,
        }
    }

    #[test]
    fn non_full_cache_free_leaves_drain_prefix_empty() {
        let mut cache = PerCpuCacheState::new();
        let mut drained = CacheDrainBuffer::<FLUSH_BATCH>::new();
        let entry = test_cache_entry(1);

        assert_eq!(cache.push_for_free(entry, &mut drained, false), 0);
        assert!(drained.initialized().is_empty());
        assert_eq!(cache.count, 1);
        assert_eq!(cache.entries[0].ptr, entry.ptr);
    }

    #[test]
    fn full_cache_drain_exposes_only_the_written_prefix() {
        let mut cache = PerCpuCacheState::new();
        for ptr in 1..=CACHE_CAPACITY {
            assert!(cache.push(test_cache_entry(ptr)));
        }
        let mut drained = CacheDrainBuffer::<{ CACHE_CAPACITY + 1 }>::new();
        let incoming = test_cache_entry(CACHE_CAPACITY + 1);

        assert_eq!(
            cache.push_for_free(incoming, &mut drained, false),
            CACHE_CAPACITY
        );
        assert_eq!(drained.initialized().len(), CACHE_CAPACITY);
        assert_eq!(drained.initialized()[0].ptr, CACHE_CAPACITY);
        assert_eq!(drained.initialized()[CACHE_CAPACITY - 1].ptr, 1);
        assert_eq!(cache.count, 1);
        assert_eq!(cache.entries[0].ptr, incoming.ptr);
    }

    #[test]
    fn cached_object_state_round_trip() {
        let slab = Slab::empty();
        slab.prepare(0x1000, 0x2000, 1, 64);
        assert!(slab.activate());
        let ptr = slab.allocate(64).expect("分配 slab 对象");
        assert_eq!(slab.object_state(ptr, 64), Some(SlabObjectState::Allocated));

        let index = slab.mark_cached(ptr, 64).expect("缓存 slab 对象");
        assert_eq!(slab.object_state(ptr, 64), Some(SlabObjectState::Cached));
        assert!(slab.release_reserved(
            CacheEntry {
                ptr,
                slab_node: INVALID_SLAB_NODE,
                cached_index: index,
            },
            64,
        ));
        assert_eq!(slab.object_state(ptr, 64), None);
        assert_eq!(slab.allocated_objects(), 0);
    }

    #[test]
    fn repeated_free_state_transition_is_rejected() {
        let slab = Slab::empty();
        slab.prepare(0x1000, 0x2000, 1, 64);
        assert!(slab.activate());
        let ptr = slab.allocate(64).expect("分配 slab 对象");

        assert_eq!(slab.mark_cached(ptr, 32), None);
        assert_eq!(slab.object_state(ptr, 64), Some(SlabObjectState::Allocated));
        let index = slab.mark_cached(ptr, 64).expect("首次释放状态迁移");
        assert_eq!(slab.mark_cached(ptr, 64), None);
        assert!(slab.mark_allocated(CacheEntry {
            ptr,
            slab_node: INVALID_SLAB_NODE,
            cached_index: index,
        }));
        assert!(slab.mark_cached(ptr, 64).is_some());
    }

    #[test]
    fn concurrent_object_state_transitions_share_atomic_words() {
        let slab = Slab::empty();
        slab.prepare(0x10_0000, 0x20_0000, 1, 64);
        assert!(slab.activate());
        let mut entries = [CacheEntry::empty(); 64];
        for entry in &mut entries {
            let ptr = slab.allocate(64).expect("分配并发测试对象");
            *entry = CacheEntry {
                ptr,
                slab_node: INVALID_SLAB_NODE,
                cached_index: super::INVALID_CACHED_INDEX,
            };
        }

        std::thread::scope(|scope| {
            let (left, right) = entries.split_at_mut(32);
            scope.spawn(|| {
                for entry in left {
                    entry.cached_index = slab.mark_cached(entry.ptr, 64).expect("左半对象释放");
                }
            });
            scope.spawn(|| {
                for entry in right {
                    entry.cached_index = slab.mark_cached(entry.ptr, 64).expect("右半对象释放");
                }
            });
        });

        for entry in entries {
            assert!(slab.release_reserved(entry, 64));
        }
        assert!(slab.is_empty());
    }

    #[test]
    fn page_directory_maps_every_page_of_multi_page_slab() {
        const BASE: usize = 0x4000_0000;
        let directory = SlabPageDirectory::new();
        assert!(directory.init((BASE, 4 * 1024 * 1024)));
        let slab_base = BASE + 4 * PAGE_SIZE;
        let mut node = test_slab_node_with_pages(slab_base, 4);
        let node_addr = (&mut *node as *mut SlabNode) as usize;
        let first_page = (slab_base - BASE) / PAGE_SIZE;
        prepare_test_directory_page(&directory, first_page);

        assert!(directory.publish_range(slab_base, 4 * PAGE_SIZE, node_addr));
        assert!(directory.owns_range(slab_base, 4 * PAGE_SIZE, node_addr));
        for page in 0..4 {
            assert_eq!(
                directory
                    .lookup(slab_base + page * PAGE_SIZE)
                    .map(|owner| owner.node_addr()),
                Some(node_addr)
            );
        }
        assert!(directory.clear_range(slab_base, 4 * PAGE_SIZE, node_addr));
        assert!(!directory.owns_range(slab_base, 4 * PAGE_SIZE, node_addr));
        assert!(directory.lookup(slab_base).is_none());
    }

    #[test]
    fn page_directory_handles_adjacent_leaf_tables() {
        const BASE: usize = 0x8000_0000;
        let directory = SlabPageDirectory::new();
        assert!(directory.init((BASE, 4 * 1024 * 1024)));
        let left_base = BASE + (SLAB_DIRECTORY_MASK - 1) * PAGE_SIZE;
        let right_base = BASE + (SLAB_DIRECTORY_MASK + 1) * PAGE_SIZE;
        let mut left = test_slab_node(left_base);
        let mut right = test_slab_node(right_base);
        let left_addr = (&mut *left as *mut SlabNode) as usize;
        let right_addr = (&mut *right as *mut SlabNode) as usize;
        let left_page = (left_base - BASE) / PAGE_SIZE;
        let right_page = (right_base - BASE) / PAGE_SIZE;
        prepare_test_directory_page(&directory, left_page);
        prepare_test_directory_page(&directory, right_page);

        assert!(directory.publish_range(left_base, PAGE_SIZE, left_addr));
        assert!(directory.publish_range(right_base, PAGE_SIZE, right_addr));
        assert_eq!(
            directory.lookup(left_base).map(|owner| owner.node_addr()),
            Some(left_addr)
        );
        assert_eq!(
            directory.lookup(right_base).map(|owner| owner.node_addr()),
            Some(right_addr)
        );
    }

    #[test]
    fn retiring_node_defers_reuse_until_owner_drops_and_changes_cookie() {
        const BASE: usize = 0xc000_0000;
        let directory = SlabPageDirectory::new();
        assert!(directory.init((BASE, 4 * 1024 * 1024)));
        let mut node = test_slab_node(BASE);
        let node_addr = (&mut *node as *mut SlabNode) as usize;
        prepare_test_directory_page(&directory, 0);
        assert!(directory.publish_range(BASE, PAGE_SIZE, node_addr));
        let owner = directory.lookup(BASE).expect("固定目录读者");
        let old_cookie = node.backend_cookie();
        node.begin_retire();
        node.slab.deactivate();
        assert!(directory.clear_range(BASE, PAGE_SIZE, node_addr));
        let mut state = ZoneState::new();
        state.enqueue_retired_slab_node(node_addr);
        assert_eq!(state.pop_reusable_slab_node(), None);
        assert_eq!(state.retired_node_count, 1);

        drop(owner);
        assert_eq!(state.pop_reusable_slab_node(), Some(node_addr));
        assert_eq!(state.retired_node_count, 0);
        node.prepare(
            BackedRange {
                arena: ArenaKind::Kernel,
                vaddr: BASE,
                paddr: BASE,
                size: PAGE_SIZE,
                order: 0,
            },
            1,
            64,
        );
        assert_ne!(node.backend_cookie(), old_cookie);
        assert!(directory.publish_range(BASE, PAGE_SIZE, node_addr));
        assert!(node.activate());
        assert_eq!(
            directory.lookup(BASE).map(|owner| owner.node_addr()),
            Some(node_addr)
        );
    }

    #[test]
    fn slab_lookup_keeps_colliding_node_after_unlink() {
        let first_base = PAGE_SIZE;
        let first_bucket = slab_lookup_bucket(first_base, PAGE_SIZE);
        let second_base = (2..=(super::SLAB_LOOKUP_BUCKETS * 4))
            .map(|page| page * PAGE_SIZE)
            .find(|&base| slab_lookup_bucket(base, PAGE_SIZE) == first_bucket)
            .expect("构造哈希碰撞");

        let mut first = test_slab_node(first_base);
        let mut second = test_slab_node(second_base);
        let first_addr = (&mut *first as *mut SlabNode) as usize;
        let second_addr = (&mut *second as *mut SlabNode) as usize;
        let mut state = ZoneState::new();

        state.insert_slab_node(first_addr);
        state.insert_slab_node(second_addr);
        assert_eq!(
            state.lookup_slab_node(first_base, PAGE_SIZE),
            Some(first_addr)
        );
        assert_eq!(
            state.lookup_slab_node(second_base, PAGE_SIZE),
            Some(second_addr)
        );

        assert!(state.unlink_slab_lookup(second_addr));
        assert_eq!(state.lookup_slab_node(second_base, PAGE_SIZE), None);
        assert_eq!(
            state.lookup_slab_node(first_base, PAGE_SIZE),
            Some(first_addr)
        );
    }

    #[test]
    fn allocated_node_lookup_does_not_depend_on_active_list() {
        let base = PAGE_SIZE;
        let mut node = test_slab_node(base);
        let ptr = node.slab.allocate(64).expect("分配测试对象");
        let node_addr = (&mut *node as *mut SlabNode) as usize;
        let mut state = ZoneState::new();
        state.insert_slab_node(node_addr);

        state.slab_head = 0;
        assert_eq!(
            state.find_allocated_node(ptr, 64, PAGE_SIZE),
            Some((node_addr, SlabObjectState::Allocated))
        );
    }

    #[test]
    fn owns_in_class_uses_page_directory_owner() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                const BASE: usize = 0x1_0000_0000;
                let allocator = Box::new(SlabAllocator::new(ArenaKind::Kernel));
                assert!(allocator.directory.init((BASE, 4 * 1024 * 1024)));
                allocator.initialized.store(true, Ordering::Release);

                let mut node = test_slab_node(BASE);
                let node_addr = (&mut *node as *mut SlabNode) as usize;
                prepare_test_directory_page(&allocator.directory, 0);
                assert!(
                    allocator
                        .directory
                        .publish_range(BASE, PAGE_SIZE, node_addr)
                );

                let zone_idx = super::class_index_for_size(64).expect("64-byte size class");
                assert!(allocator.owns_in_class(zone_idx, BASE));
                assert!(!allocator.owns_in_class(zone_idx + 1, BASE));
                assert!(!allocator.owns_in_class(super::SIZE_CLASS_COUNT, BASE));
            })
            .expect("创建大栈测试线程")
            .join()
            .expect("size-class owner 测试线程失败");
    }

    #[test]
    fn audit_detects_slab_lookup_mismatch() {
        let base = PAGE_SIZE;
        let mut node = test_slab_node(base);
        let node_addr = (&mut *node as *mut SlabNode) as usize;
        let mut state = ZoneState::new();
        state.insert_slab_node(node_addr);
        assert!(state.audit(64).is_consistent());

        assert!(state.unlink_slab_lookup(node_addr));
        assert!(
            state
                .audit(64)
                .flags
                .contains(SlabAuditFlags::SLAB_LOOKUP_MISMATCH)
        );
    }
}

#[inline]
fn next_hint_after(idx: usize, total: usize) -> usize {
    if idx + 1 < total { idx + 1 } else { 0 }
}

#[inline]
fn pages_to_order(pages: usize) -> Option<usize> {
    // slab 想申请任意页数，但 buddy 只接受 order。这里做的就是把需求向上折算成最小
    // 覆盖该页数的 2^order 块，作为下层物理页和虚拟区间申请的共同粒度。这里必须
    // 使用有界左移：size class 目前是常量，但未来扩展 slab class 时不能让极端页数
    // 在 grow 冷路径中溢出或无限循环。
    if pages == 0 {
        return None;
    }
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

#[inline]
fn pages_for_order(order: usize) -> Option<usize> {
    if order > MAX_TRACKED_ORDER {
        return None;
    }
    1usize.checked_shl(order as u32)
}

#[cfg(feature = "ktest-kernel")]
mod tests {
    extern crate alloc;

    use alloc::boxed::Box;

    use super::*;
    use crate::space::ArenaKind;

    fn test_node(base_addr: usize, allocated: bool) -> Box<SlabNode> {
        let node = Box::new(SlabNode::empty());
        node.prepare(
            BackedRange {
                arena: ArenaKind::Kernel,
                vaddr: base_addr,
                paddr: base_addr,
                size: PAGE_SIZE,
                order: 0,
            },
            1,
            PAGE_SIZE,
        );
        assert!(node.activate());
        if allocated {
            assert_eq!(node.slab.allocate(PAGE_SIZE), Some(base_addr));
        }
        node
    }

    #[ktest::ktest]
    fn allocate_batch_wraps_before_growing() {
        let mut head = test_node(0x1000, false);
        let mut preferred = test_node(0x2000, true);
        let mut tail = test_node(0x3000, true);
        head.next.store(
            (&mut *preferred as *mut SlabNode) as usize,
            Ordering::Relaxed,
        );
        preferred
            .next
            .store((&mut *tail as *mut SlabNode) as usize, Ordering::Relaxed);

        let mut state = ZoneState::new();
        state.slab_head = (&mut *head as *mut SlabNode) as usize;
        state.preferred_slab = (&mut *preferred as *mut SlabNode) as usize;
        state.slab_count = 3;

        let mut out = [CacheEntry::empty(); 1];
        assert_eq!(state.allocate_batch(PAGE_SIZE, &mut out), 1);
        assert_eq!(out[0].ptr, 0x1000);
    }
}
