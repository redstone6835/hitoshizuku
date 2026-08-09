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
use crate::request::AllocationRecord;
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
const MAX_GROW_ATTEMPTS: usize = 3;
const MAX_EMPTY_SLABS_PER_ZONE: usize = 4;

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
        // SAFETY: initialized 只会在对应槽位完成 MaybeUninit::write 后递增，因此
        // entries 的这个前缀全部有效；后缀不会被构造成引用。
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
        // free 热路径最常见的情况是 cache 还有空间。把“检查容量”和“push”合并到一次
        // cache 锁内，只有满桶时才排出一批对象交给 slab state 做真实释放；腾出空间后
        // 立即放入当前对象，避免满桶慢路径二次获取 cache 锁。
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
    pub slab_node: usize,
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
    made_empty: bool,
}

impl SlabAllocation {
    const fn null() -> Self {
        Self {
            ptr: 0,
            slab_node: INVALID_SLAB_NODE,
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
    base_addr: usize,
    paddr: usize,
    page_count: u16,
    total_objects: u16,
    allocated_objects: u16,
    /// 下次位图扫描的起点。
    ///
    /// slab 对象通常按低地址递增分配；如果每次 cache miss 都从 0 开始扫描，活跃 slab
    /// 越满，前缀已分配位带来的重复检查越多。这个 hint 让分配路径从上次命中位置之后
    /// 继续，并在 flush 释放真实空槽时回退到被释放槽位。
    next_free_hint: u16,
    alloc_bitmap: [u64; BITMAP_WORDS],
    cached_bitmap: [AtomicU64; BITMAP_WORDS],
    active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlabObjectState {
    Allocated,
    Cached,
}

impl Slab {
    const fn empty() -> Self {
        Self {
            base_addr: 0,
            paddr: 0,
            page_count: 0,
            total_objects: 0,
            allocated_objects: 0,
            next_free_hint: 0,
            alloc_bitmap: [0; BITMAP_WORDS],
            cached_bitmap: [const { AtomicU64::new(0) }; BITMAP_WORDS],
            active: false,
        }
    }

    fn init(&mut self, base_addr: usize, paddr: usize, page_count: usize, obj_size: usize) {
        // 一个 slab 初始化时，核心工作不是“构造对象”，而是把一段连续页解释成固定大小的
        // 槽位数组，并把所有状态压进位图。后续对象生命周期全靠位图位翻转推进。
        if obj_size == 0 {
            self.active = false;
            return;
        }

        let Some(span_size) = page_count.checked_mul(PAGE_SIZE) else {
            self.active = false;
            return;
        };
        if page_count > u16::MAX as usize {
            self.active = false;
            return;
        }

        let total_objects = (span_size / obj_size).min(BITMAP_WORDS * 64);
        self.base_addr = base_addr;
        self.paddr = paddr;
        self.page_count = page_count as u16;
        self.total_objects = total_objects as u16;
        self.allocated_objects = 0;
        self.next_free_hint = 0;
        self.alloc_bitmap = [0; BITMAP_WORDS];
        for word in &self.cached_bitmap {
            word.store(0, Ordering::Relaxed);
        }
        self.active = total_objects > 0;
    }

    fn contains(&self, ptr: usize) -> bool {
        // 这里只判断地址是否落在 slab 覆盖范围内，不验证它是不是对象边界。更严格的槽位
        // 合法性检查交给 `object_index` 统一处理。
        let Some(span_size) = (self.page_count as usize).checked_mul(PAGE_SIZE) else {
            return false;
        };
        let Some(end) = self.base_addr.checked_add(span_size) else {
            return false;
        };
        self.active && ptr >= self.base_addr && ptr < end
    }

    fn object_index(&self, ptr: usize, obj_size: usize) -> Option<usize> {
        // 指针要想映射成有效槽位，必须同时满足：落在 slab 范围内、位于对象边界上、索引
        // 不超过 `total_objects`。这三个条件一起定义了“一个对象指针在 slab 语义下成立”。
        if obj_size == 0 {
            return None;
        }

        if !self.contains(ptr) {
            return None;
        }
        let offset = ptr - self.base_addr;
        if !offset.is_multiple_of(obj_size) {
            return None;
        }
        let idx = offset / obj_size;
        (idx < self.total_objects as usize).then_some(idx)
    }

    fn allocate(&mut self, obj_size: usize) -> Option<usize> {
        // slab 内部只在批量补货路径扫描和修改位图。分配给调用者或暂存在 magazine
        // 对 slab 来说都是保留状态，两者的转换完全发生在 CPU 本地 cache 中。
        if !self.active || self.allocated_objects >= self.total_objects {
            return None;
        }

        let Some(idx) = self.find_free_slot() else {
            self.next_free_hint = self.total_objects;
            return None;
        };

        self.set_alloc_bit(idx, true);
        self.set_cached_bit(idx, false);
        self.allocated_objects += 1;
        self.next_free_hint = next_hint_after(idx, self.total_objects as usize) as u16;
        Some(self.base_addr + idx * obj_size)
    }

    fn object_state(&self, ptr: usize, obj_size: usize) -> Option<SlabObjectState> {
        let idx = self.object_index(ptr, obj_size)?;
        if !self.alloc_bit(idx) {
            return None;
        }
        Some(if self.cached_bit(idx) {
            SlabObjectState::Cached
        } else {
            SlabObjectState::Allocated
        })
    }

    fn release_reserved(&mut self, ptr: usize, obj_size: usize) -> bool {
        // magazine flush 才会真正释放对象槽。此时持有 ZoneState 锁，可以直接修改
        // 普通位图和 slab 计数，不需要任何原子读改写。
        let Some(idx) = self.object_index(ptr, obj_size) else {
            return false;
        };
        if !self.alloc_bit(idx) {
            return false;
        }
        self.set_alloc_bit(idx, false);
        self.set_cached_bit(idx, false);
        self.allocated_objects = self.allocated_objects.saturating_sub(1);
        self.next_free_hint = idx.min(self.next_free_hint as usize) as u16;
        true
    }

    fn is_empty(&self) -> bool {
        self.active && self.allocated_objects == 0
    }

    fn alloc_bit(&self, idx: usize) -> bool {
        bit_is_set(&self.alloc_bitmap, idx)
    }

    fn cached_bit(&self, idx: usize) -> bool {
        let word = idx / 64;
        let Some(bits) = self.cached_bitmap.get(word) else {
            return false;
        };
        bits.load(Ordering::Acquire) & (1u64 << (idx % 64)) != 0
    }

    fn set_alloc_bit(&mut self, idx: usize, set: bool) {
        set_bit(&mut self.alloc_bitmap, idx, set);
    }

    fn set_cached_bit(&self, idx: usize, set: bool) -> bool {
        let word = idx / 64;
        let Some(bits) = self.cached_bitmap.get(word) else {
            return false;
        };
        let mask = 1u64 << (idx % 64);
        let previous = if set {
            bits.fetch_or(mask, Ordering::AcqRel)
        } else {
            bits.fetch_and(!mask, Ordering::AcqRel)
        };
        previous & mask != 0
    }

    fn find_free_slot(&self) -> Option<usize> {
        let total = self.total_objects as usize;
        if total == 0 {
            return None;
        }

        let start = (self.next_free_hint as usize).min(total.saturating_sub(1));
        find_clear_bit_in_range(&self.alloc_bitmap, start, total)
            .or_else(|| find_clear_bit_in_range(&self.alloc_bitmap, 0, start))
    }
}

struct SlabNode {
    slab: Slab,
    backing: BackedRange,
    next: usize,
    lookup_next: usize,
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
                let node = slab_node_mut(current);
                if let Some(ptr) = node.slab.allocate(obj_size) {
                    entry = Some(CacheEntry {
                        ptr,
                        slab_node: current,
                        cached_index: INVALID_CACHED_INDEX,
                    });
                    selected = current;
                    break;
                }
                node_addr = node.next;
            }
            let Some(entry) = entry else {
                break;
            };
            out[produced] = entry;
            produced += 1;
            // 当前 slab 还有空槽时继续使用它；耗尽后才转到下一个节点。
            let node = slab_node(selected);
            node_addr = if node.slab.allocated_objects < node.slab.total_objects {
                selected
            } else {
                node.next
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
        let node = slab_node_mut(node_addr);
        let block_pages = node.slab.page_count as usize;
        let bucket = slab_lookup_bucket(node.slab.base_addr, node.backing.size);
        node.lookup_next = self.slab_lookup[bucket];
        self.slab_lookup[bucket] = node_addr;
        node.next = self.slab_head;
        self.slab_head = node_addr;
        self.preferred_slab = node_addr;
        self.slab_count += 1;
        self.stats.active_slabs = self.slab_count;
        self.stats.active_pages += block_pages;
    }

    fn pop_reusable_slab_node(&mut self) -> Option<usize> {
        if self.free_node_head == 0 {
            return None;
        }
        let node_addr = self.free_node_head;
        let node = slab_node_mut(node_addr);
        self.free_node_head = node.next;
        node.next = 0;
        self.free_node_count = self.free_node_count.saturating_sub(1);
        self.stats.free_slab_nodes = self.free_node_count;
        Some(node_addr)
    }

    fn push_reusable_slab_node(&mut self, node_addr: usize) {
        let node = slab_node_mut(node_addr);
        node.slab = Slab::empty();
        node.lookup_next = 0;
        node.next = self.free_node_head;
        self.free_node_head = node_addr;
        self.free_node_count += 1;
        self.stats.free_slab_nodes = self.free_node_count;
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
            if node.slab.active
                && node.slab.base_addr == slab_base
                && node.backing.size == slab_span
            {
                return Some(current);
            }
            current = node.lookup_next;
            visited += 1;
        }
        None
    }

    fn unlink_slab_lookup(&mut self, node_addr: usize) -> bool {
        if node_addr == INVALID_SLAB_NODE {
            return false;
        }
        let node = slab_node(node_addr);
        let bucket = slab_lookup_bucket(node.slab.base_addr, node.backing.size);
        let mut previous = 0usize;
        let mut current = self.slab_lookup[bucket];
        let mut visited = 0usize;
        while current != 0 && visited < self.slab_count {
            let next = slab_node(current).lookup_next;
            if current == node_addr {
                if previous == 0 {
                    self.slab_lookup[bucket] = next;
                } else {
                    slab_node_mut(previous).lookup_next = next;
                }
                slab_node_mut(current).lookup_next = 0;
                return true;
            }
            previous = current;
            current = next;
            visited += 1;
        }
        false
    }

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

    fn mark_cached(&mut self, node_addr: usize, ptr: usize, obj_size: usize) -> Option<u16> {
        let slab = &slab_node(node_addr).slab;
        let idx = slab.object_index(ptr, obj_size)?;
        if !slab.alloc_bit(idx) || slab.set_cached_bit(idx, true) {
            return None;
        }
        Some(idx as u16)
    }

    fn mark_allocated(entry: CacheEntry) -> bool {
        if entry.cached_index == INVALID_CACHED_INDEX || entry.slab_node == INVALID_SLAB_NODE {
            return false;
        }
        let idx = entry.cached_index as usize;
        let word = idx / 64;
        let mask = 1u64 << (idx % 64);
        let node = entry.slab_node as *const SlabNode;
        // SAFETY: slab 节点只复用不释放；cache entry 保留 alloc 位，因此节点在清位前
        // 不会进入回收路径。cached_index 也来自该节点的有效对象索引。
        let bits = unsafe { &*core::ptr::addr_of!((*node).slab.cached_bitmap[word]) };
        bits.fetch_and(!mask, Ordering::AcqRel) & mask != 0
    }

    fn flush_cached_entries(&mut self, entries: &[CacheEntry], obj_size: usize) -> SlabFlushResult {
        // 当本地 cache 过满时，批量冲刷一批对象回 slab。这里故意保持顺序、朴素的实现，
        // 因为它走的是冷一些的回压路径，稳定性比极致优化更重要。返回值只表示本轮
        // flush 是否可能制造了一个空 slab；普通 free 不再无条件扫描 slab 链。
        let mut flushed = 0;
        let mut made_empty = false;
        for entry in entries {
            if entry.slab_node == INVALID_SLAB_NODE {
                continue;
            }
            let slab = &mut slab_node_mut(entry.slab_node).slab;
            if !slab.active {
                self.stats.invalid_frees += 1;
                continue;
            }
            if slab.release_reserved(entry.ptr, obj_size) {
                flushed += 1;
                made_empty |= slab.is_empty();
            }
        }
        SlabFlushResult {
            flushed,
            made_empty,
        }
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

    fn take_reclaimable_empty_slab(&mut self) -> Option<BackedRange> {
        let mut empty_count = 0usize;
        let mut node_addr = self.slab_head;
        while node_addr != 0 {
            let node = slab_node(node_addr);
            if node.slab.is_empty() {
                empty_count += 1;
            }
            node_addr = node.next;
        }
        if empty_count <= MAX_EMPTY_SLABS_PER_ZONE {
            return None;
        }

        let mut prev = 0usize;
        let mut current = self.slab_head;
        while current != 0 {
            let node = slab_node(current);
            let next = node.next;
            if node.slab.is_empty() {
                let backing = node.backing;
                if !self.unlink_slab_lookup(current) {
                    panic!("[alloc][invariant] active slab missing from lookup directory");
                }
                if prev == 0 {
                    self.slab_head = next;
                } else {
                    slab_node_mut(prev).next = next;
                }
                let node = slab_node_mut(current);
                node.slab.active = false;
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
                self.push_reusable_slab_node(current);
                return Some(backing);
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
            audit.scanned_slabs += 1;
            audit_slab(node, obj_size, &mut audit);
            if self.lookup_slab_node(node.slab.base_addr, node.backing.size) != Some(node_addr) {
                audit.flags.insert(SlabAuditFlags::SLAB_LOOKUP_MISMATCH);
            }
            node_addr = node.next;
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
                if !node.slab.active
                    || slab_lookup_bucket(node.slab.base_addr, node.backing.size) != bucket
                    || !self.contains_active_node(lookup_node)
                {
                    audit.flags.insert(SlabAuditFlags::SLAB_LOOKUP_MISMATCH);
                }
                lookup_nodes = lookup_nodes.saturating_add(1);
                bucket_nodes += 1;
                lookup_node = node.lookup_next;
            }
        }
        if lookup_nodes != self.slab_count {
            audit.flags.insert(SlabAuditFlags::SLAB_LOOKUP_MISMATCH);
        }

        let mut free_node_addr = self.free_node_head;
        while free_node_addr != 0 {
            if audit.scanned_free_nodes >= self.free_node_count {
                audit.flags.insert(SlabAuditFlags::FREE_NODE_LOOP);
                break;
            }

            let node = slab_node(free_node_addr);
            if node.slab.active {
                audit.flags.insert(SlabAuditFlags::INVALID_SLAB_RANGE);
            }
            audit.scanned_free_nodes += 1;
            free_node_addr = node.next;
        }

        if audit.scanned_slabs != self.slab_count
            || audit.scanned_slabs != self.stats.active_slabs
            || audit.scanned_active_pages != self.stats.active_pages
            || audit.scanned_free_nodes != self.free_node_count
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
            current = slab_node(current).next;
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
            node_addr
        }
    };

    let node = slab_node_mut(node_addr);
    node.slab = Slab::empty();
    node.slab
        .init(range.vaddr, range.paddr, block_pages, obj_size);
    node.backing = range;
    node.next = 0;
    node.lookup_next = 0;
    if !node.slab.active {
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
                slab_node: entry.slab_node,
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
                    self.reclaim_empty_slabs(Some((phys, vmem)));
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
                        let mut state = self.state.lock();
                        state.insert_slab_node(node_addr);
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
            let flush = {
                let mut state = self.state.lock();
                state.flush_cached_entries(&overflow[..overflow_count], self.size_class)
            };
            if flush.made_empty {
                self.reclaim_empty_slabs(Some((phys, vmem)));
            }
        }
        SlabAllocation {
            ptr: current.ptr,
            slab_node: current.slab_node,
        }
    }

    fn free(
        &self,
        ptr: usize,
        cpu: usize,
        backing: Option<(&Mutex<BuddyAllocator>, &KernelAddressSpace)>,
    ) -> bool {
        // 没有 registry cookie 的路径在 ZoneState 中定位对象。缓存对象由独立位图标记，
        // 因而不再需要扫描全部 CPU cache 来判断重复释放。
        let cache = &self.caches[cpu];
        let mut drained = CacheDrainBuffer::<FLUSH_BATCH>::new();
        let mut should_reclaim = false;
        let (slab_node, cached_index) = {
            let mut state = self.state.lock();
            let slab_span = self.pages_per_slab.saturating_mul(PAGE_SIZE);
            let Some((slab_node, object_state)) =
                state.find_allocated_node(ptr, self.size_class, slab_span)
            else {
                return false;
            };
            if object_state == SlabObjectState::Cached {
                state.stats.invalid_frees += 1;
                return false;
            }
            let Some(cached_index) = state.mark_cached(slab_node, ptr, self.size_class) else {
                state.stats.invalid_frees += 1;
                return false;
            };
            (slab_node, cached_index)
        };
        let entry = CacheEntry {
            ptr,
            slab_node,
            cached_index,
        };
        let drained_count = {
            let mut cache_guard = cache.inner.lock();
            cache_guard.push_for_free(entry, &mut drained, false)
        };
        if drained_count != 0 {
            let mut state = self.state.lock();
            should_reclaim |= state
                .flush_cached_entries(drained.initialized(), self.size_class)
                .made_empty;
        }
        if should_reclaim {
            self.reclaim_empty_slabs(backing);
        }
        true
    }

    fn free_with_hint(
        &self,
        ptr: usize,
        cpu: usize,
        slab_node_hint: usize,
        backing: Option<(&Mutex<BuddyAllocator>, &KernelAddressSpace)>,
    ) -> bool {
        if slab_node_hint == INVALID_SLAB_NODE {
            return self.free(ptr, cpu, backing);
        }

        // backend cookie 由 allocator registry 生成且 SlabNode 只复用不释放；正常释放
        // 可以直接把 entry 放入本地 magazine，不读取 slab 元数据。
        let cache = &self.caches[cpu];
        let mut drained = CacheDrainBuffer::<FLUSH_BATCH>::new();
        let mut should_reclaim = false;
        let entry = CacheEntry {
            ptr,
            slab_node: slab_node_hint,
            cached_index: INVALID_CACHED_INDEX,
        };
        let drained_count = {
            let mut cache_guard = cache.inner.lock();
            cache_guard.push_for_free(entry, &mut drained, true)
        };
        if drained_count != 0 {
            let mut state = self.state.lock();
            should_reclaim |= state
                .flush_cached_entries(drained.initialized(), self.size_class)
                .made_empty;
        }
        if should_reclaim {
            self.reclaim_empty_slabs(backing);
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
    ) -> SlabReclaimStats {
        let mut out = SlabReclaimStats::default();
        let Some((phys, vmem)) = backing else {
            return out;
        };
        loop {
            let range = {
                let mut state = self.state.lock();
                state.take_reclaimable_empty_slab()
            };
            let Some(range) = range else {
                break;
            };
            if let Err(err) = vmem.free_backed_range(range, phys) {
                panic!(
                    "[alloc][invariant] slab empty range reclaim failed class={} vaddr={:#x} paddr={:#x} size={} err={:?}",
                    self.size_class, range.vaddr, range.paddr, range.size, err
                );
            }
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
                empty_pages = empty_pages.saturating_add(node.backing.size / PAGE_SIZE);
            }
            node_addr = node.next;
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

    fn contains(&self, ptr: usize) -> bool {
        let state = self.state.lock();
        let mut node_addr = state.slab_head;
        while node_addr != 0 {
            let node = slab_node(node_addr);
            if node.slab.contains(ptr) {
                return true;
            }
            node_addr = node.next;
        }
        false
    }

    fn audit(&self) -> SlabAudit {
        let mut latest = SlabAudit::default();
        for _ in 0..4 {
            latest = self.audit_once();
            if latest.is_consistent() {
                break;
            }
            core::hint::spin_loop();
        }
        latest
    }

    fn audit_once(&self) -> SlabAudit {
        // 同时固定全部 magazine，保证 entry 和普通计数来自同一时点。refill/flush 在
        // cache 与共享状态之间交接时可能留下极短窗口，audit() 会对此做有限重试。
        let caches = self.caches.each_ref().map(|cache| cache.inner.lock());
        let state = self.state.lock();
        let mut audit = state.audit(self.size_class);
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
            cpu_count: AtomicUsize::new(1),
            initialized: AtomicBool::new(false),
        }
    }

    pub fn init(&self, cpu_count: usize) {
        self.cpu_count
            .store(cpu_count.clamp(1, MAX_CPUS), Ordering::Release);
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
        self.zones[zone_idx].alloc(cpu, phys, vmem).ptr as *mut u8
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
        self.zones[zone_idx].alloc(cpu, phys, vmem)
    }

    pub fn free(&self, ptr: usize, layout: Layout, cpu_id: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let Some(zone_idx) = Self::class_index_for(layout) else {
            return false;
        };
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].free(ptr, cpu, None)
    }

    pub(crate) fn free_reclaiming(
        &self,
        ptr: usize,
        layout: Layout,
        cpu_id: usize,
        phys: &Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let Some(zone_idx) = Self::class_index_for(layout) else {
            return false;
        };
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].free(ptr, cpu, Some((phys, vmem)))
    }

    pub fn same_size_class(old_layout: Layout, new_layout: Layout) -> bool {
        Self::class_index_for(old_layout) == Self::class_index_for(new_layout)
    }

    pub fn owns(&self, ptr: usize) -> bool {
        self.zone_index_for_ptr(ptr).is_some()
    }

    pub(crate) fn owns_in_class(&self, zone_idx: usize, ptr: usize) -> bool {
        self.zones
            .get(zone_idx)
            .is_some_and(|zone| zone.contains(ptr))
    }

    pub fn free_ptr(&self, ptr: usize, cpu_id: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let Some(zone_idx) = self.zone_index_for_ptr(ptr) else {
            return false;
        };
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].free(ptr, cpu, None)
    }

    pub fn free_record(&self, record: AllocationRecord, cpu_id: usize) -> bool {
        self.free_record_reclaiming(record, cpu_id, None)
    }

    pub fn free_record_reclaiming(
        &self,
        record: AllocationRecord,
        cpu_id: usize,
        backing: Option<(&Mutex<BuddyAllocator>, &KernelAddressSpace)>,
    ) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let class_size = record.usable_size.max(record.size);
        let Some(zone_idx) = class_index_for_size(class_size) else {
            return false;
        };
        let cpu = self.normalize_cpu(cpu_id);
        self.zones[zone_idx].free_with_hint(record.ptr, cpu, record.backend_cookie, backing)
    }

    pub fn usable_size_for_ptr(&self, ptr: usize) -> Option<usize> {
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
                let stats = zone.reclaim_empty_slabs(Some((phys, vmem)));
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
            out.merge(zone.audit());
        }
        out
    }

    fn normalize_cpu(&self, cpu_id: usize) -> usize {
        let limit = self.cpu_count.load(Ordering::Acquire).clamp(1, MAX_CPUS);
        cpu_id.min(limit - 1)
    }

    fn zone_index_for_ptr(&self, ptr: usize) -> Option<usize> {
        for (idx, zone) in self.zones.iter().enumerate() {
            if zone.contains(ptr) {
                return Some(idx);
            }
        }
        None
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
    unsafe { &*(addr as *const SlabNode) }
}

#[inline]
fn slab_node_mut(addr: usize) -> &'static mut SlabNode {
    unsafe { &mut *(addr as *mut SlabNode) }
}

#[inline]
fn bit_is_set(bits: &[u64; BITMAP_WORDS], idx: usize) -> bool {
    // slab 只在 refill/flush 批次修改位图，热点 magazine 命中不进入这里。
    let word = idx / 64;
    if word >= BITMAP_WORDS {
        return false;
    }
    let bit = idx % 64;
    (bits[word] & (1u64 << bit)) != 0
}

#[inline]
fn set_bit(bits: &mut [u64; BITMAP_WORDS], idx: usize, set: bool) {
    // 所有对象槽位状态迁移最终都会收敛到“某一位设为 1 或清为 0”。
    let word = idx / 64;
    if word >= BITMAP_WORDS {
        return;
    }
    let bit = idx % 64;
    if set {
        bits[word] |= 1u64 << bit;
    } else {
        bits[word] &= !(1u64 << bit);
    }
}

#[inline]
fn slab_lookup_bucket(slab_base: usize, slab_span: usize) -> usize {
    debug_assert!(slab_span.is_power_of_two());
    let slot = slab_base / slab_span;
    slot.wrapping_mul(0x9e37_79b9_7f4a_7c15) & (SLAB_LOOKUP_BUCKETS - 1)
}

fn audit_slab(node: &SlabNode, obj_size: usize, audit: &mut SlabAudit) {
    let slab = &node.slab;
    let total = slab.total_objects as usize;
    let page_count = slab.page_count as usize;
    let expected_objects = page_count
        .checked_mul(PAGE_SIZE)
        .map(|span| span / obj_size)
        .unwrap_or(0)
        .min(BITMAP_WORDS * 64);

    if !slab.active
        || slab.base_addr == 0
        || page_count == 0
        || total == 0
        || total > BITMAP_WORDS * 64
        || expected_objects != total
        || node.backing.vaddr != slab.base_addr
        || node.backing.paddr != slab.paddr
        || node.backing.size != page_count.saturating_mul(PAGE_SIZE)
        || !slab.base_addr.is_multiple_of(PAGE_SIZE)
        || !slab.paddr.is_multiple_of(PAGE_SIZE)
    {
        audit.flags.insert(SlabAuditFlags::INVALID_SLAB_RANGE);
    }

    let allocated = count_set_bits_in_range(&slab.alloc_bitmap, total);
    let cached = count_atomic_bits_in_range(&slab.cached_bitmap, total);
    if allocated != slab.allocated_objects as usize
        || !atomic_bits_are_subset(&slab.cached_bitmap, &slab.alloc_bitmap, total)
    {
        audit.flags.insert(SlabAuditFlags::OBJECT_COUNT_MISMATCH);
    }
    if has_bits_outside_range(&slab.alloc_bitmap, total)
        || has_atomic_bits_outside_range(&slab.cached_bitmap, total)
    {
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

fn count_set_bits_in_range(bits: &[u64; BITMAP_WORDS], end: usize) -> usize {
    let end = end.min(BITMAP_WORDS * 64);
    let mut count = 0usize;
    let mut word = 0usize;
    while word < BITMAP_WORDS {
        let word_start = word * 64;
        if word_start >= end {
            break;
        }
        let word_end = (end - word_start).min(64);
        count =
            count.saturating_add((bits[word] & bit_range_mask(0, word_end)).count_ones() as usize);
        word += 1;
    }
    count
}

fn count_atomic_bits_in_range(bits: &[AtomicU64; BITMAP_WORDS], end: usize) -> usize {
    let end = end.min(BITMAP_WORDS * 64);
    let mut count = 0usize;
    let mut word = 0usize;
    while word < BITMAP_WORDS {
        let word_start = word * 64;
        if word_start >= end {
            break;
        }
        let word_end = (end - word_start).min(64);
        count = count.saturating_add(
            (bits[word].load(Ordering::Acquire) & bit_range_mask(0, word_end)).count_ones()
                as usize,
        );
        word += 1;
    }
    count
}

fn atomic_bits_are_subset(
    subset: &[AtomicU64; BITMAP_WORDS],
    superset: &[u64; BITMAP_WORDS],
    end: usize,
) -> bool {
    let end = end.min(BITMAP_WORDS * 64);
    for word in 0..BITMAP_WORDS {
        let word_start = word * 64;
        let valid_mask = if word_start >= end {
            0
        } else {
            bit_range_mask(0, (end - word_start).min(64))
        };
        if subset[word].load(Ordering::Acquire) & valid_mask & !superset[word] != 0 {
            return false;
        }
    }
    true
}

fn has_bits_outside_range(bits: &[u64; BITMAP_WORDS], end: usize) -> bool {
    let end = end.min(BITMAP_WORDS * 64);
    let mut word = 0usize;
    while word < BITMAP_WORDS {
        let word_start = word * 64;
        let valid_mask = if word_start >= end {
            0
        } else {
            bit_range_mask(0, (end - word_start).min(64))
        };
        if bits[word] & !valid_mask != 0 {
            return true;
        }
        word += 1;
    }
    false
}

fn has_atomic_bits_outside_range(bits: &[AtomicU64; BITMAP_WORDS], end: usize) -> bool {
    let end = end.min(BITMAP_WORDS * 64);
    for (word, bits) in bits.iter().enumerate() {
        let word_start = word * 64;
        let valid_mask = if word_start >= end {
            0
        } else {
            bit_range_mask(0, (end - word_start).min(64))
        };
        if bits.load(Ordering::Acquire) & !valid_mask != 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod slab_state_tests {
    extern crate alloc;
    extern crate std;

    use alloc::boxed::Box;

    use super::{
        CACHE_CAPACITY, CacheDrainBuffer, CacheEntry, FLUSH_BATCH, PerCpuCacheState, Slab,
        SlabAllocator, SlabAuditFlags, SlabNode, SlabObjectState, ZoneState, slab_lookup_bucket,
    };
    use crate::buddy::PAGE_SIZE;
    use crate::space::{ArenaKind, BackedRange};

    fn test_slab_node(base: usize) -> Box<SlabNode> {
        let mut slab = Slab::empty();
        slab.init(base, base, 1, 64);
        Box::new(SlabNode {
            slab,
            backing: BackedRange {
                arena: ArenaKind::Kernel,
                vaddr: base,
                paddr: base,
                size: PAGE_SIZE,
                order: 0,
            },
            next: 0,
            lookup_next: 0,
        })
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
        let mut slab = Slab::empty();
        slab.init(0x1000, 0x2000, 1, 64);
        let ptr = slab.allocate(64).expect("分配 slab 对象");
        assert_eq!(slab.object_state(ptr, 64), Some(SlabObjectState::Allocated));

        let index = slab.object_index(ptr, 64).expect("定位 slab 对象");
        slab.set_cached_bit(index, true);
        assert_eq!(slab.object_state(ptr, 64), Some(SlabObjectState::Cached));
        assert!(slab.release_reserved(ptr, 64));
        assert_eq!(slab.object_state(ptr, 64), None);
        assert_eq!(slab.allocated_objects, 0);
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
    fn owns_in_class_routes_to_only_the_requested_zone() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let allocator = Box::new(SlabAllocator::new(ArenaKind::Kernel));
                let zone_idx = 3;
                let mut node = test_slab_node(PAGE_SIZE);
                let ptr = node.slab.allocate(64).expect("分配测试对象");
                let node_addr = (&mut *node as *mut SlabNode) as usize;
                allocator.zones[zone_idx]
                    .state
                    .lock()
                    .insert_slab_node(node_addr);

                assert!(allocator.owns_in_class(zone_idx, ptr));
                assert!(!allocator.owns_in_class(zone_idx + 1, ptr));
                assert!(!allocator.owns_in_class(super::SIZE_CLASS_COUNT, ptr));
            })
            .expect("创建大栈测试线程")
            .join()
            .expect("ownership 路由测试线程失败");
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
fn find_clear_bit_in_range(bits: &[u64; BITMAP_WORDS], start: usize, end: usize) -> Option<usize> {
    // 对象位图最多 512 位。按 word 查找可以一次跳过 64 个已分配槽，避免 slab 越满时
    // cache miss 路径退化成从头逐位扫描。`end` 由 total_objects 传入，必须 mask 掉
    // slab 末尾未使用的位，防止把 bitmap 填充位误认为真实对象槽。
    let end = end.min(BITMAP_WORDS * 64);
    if start >= end {
        return None;
    }

    let mut word = start / 64;
    while word < BITMAP_WORDS {
        let word_start = word * 64;
        if word_start >= end {
            break;
        }
        let from = start.saturating_sub(word_start).min(64);
        let to = (end - word_start).min(64);
        let mask = bit_range_mask(from, to);
        let free = !bits[word] & mask;
        if free != 0 {
            return Some(word_start + free.trailing_zeros() as usize);
        }
        word += 1;
    }
    None
}

#[inline]
fn bit_range_mask(start: usize, end: usize) -> u64 {
    if start >= end {
        return 0;
    }
    let high = if end >= 64 {
        u64::MAX
    } else {
        (1u64 << end) - 1
    };
    let low = if start == 0 { 0 } else { (1u64 << start) - 1 };
    high & !low
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
        let mut slab = Slab::empty();
        slab.init(base_addr, base_addr, 1, PAGE_SIZE);
        if allocated {
            assert_eq!(slab.allocate(PAGE_SIZE), Some(base_addr));
        }
        Box::new(SlabNode {
            slab,
            backing: BackedRange {
                arena: ArenaKind::Kernel,
                vaddr: base_addr,
                paddr: base_addr,
                size: PAGE_SIZE,
                order: 0,
            },
            next: 0,
            lookup_next: 0,
        })
    }

    #[ktest::ktest]
    fn allocate_batch_wraps_before_growing() {
        let mut head = test_node(0x1000, false);
        let mut preferred = test_node(0x2000, true);
        let mut tail = test_node(0x3000, true);
        head.next = (&mut *preferred as *mut SlabNode) as usize;
        preferred.next = (&mut *tail as *mut SlabNode) as usize;

        let mut state = ZoneState::new();
        state.slab_head = (&mut *head as *mut SlabNode) as usize;
        state.preferred_slab = (&mut *preferred as *mut SlabNode) as usize;
        state.slab_count = 3;

        let mut out = [CacheEntry::empty(); 1];
        assert_eq!(state.allocate_batch(PAGE_SIZE, &mut out), 1);
        assert_eq!(out[0].ptr, 0x1000);
    }
}
