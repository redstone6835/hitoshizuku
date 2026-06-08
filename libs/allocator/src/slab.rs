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
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use spin::mutex::Mutex;

use crate::buddy::{BuddyAllocator, PAGE_SIZE};
use crate::request::AllocationRecord;
use crate::space::{BackedRange, KernelAddressSpace};

pub const MAX_SMALL_SIZE: usize = 2048;
pub const MAX_CPUS: usize = 64;

const SIZE_CLASSES: [usize; 14] = [
    8, 16, 32, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048,
];
const SIZE_CLASS_COUNT: usize = SIZE_CLASSES.len();
const CACHE_CAPACITY: usize = 32;
const REFILL_BATCH: usize = 8;
const BITMAP_WORDS: usize = 8;
const INVALID_SLAB_NODE: usize = 0;
const MAX_GROW_ATTEMPTS: usize = 3;
const MAX_EMPTY_SLABS_PER_ZONE: usize = 1;

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
    pub reclaimed_slabs: u64,
    pub free_slab_nodes: usize,
}

/// 每 CPU 缓存中的一个槽位。
///
/// 除了对象指针本身，还记录其所属 slab 节点，以便缓存命中时能快速把状态从“cached”
/// 切回“allocated”。
#[derive(Clone, Copy)]
struct CacheEntry {
    ptr: usize,
    slab_node: usize,
}

impl CacheEntry {
    const fn empty() -> Self {
        Self {
            ptr: 0,
            slab_node: INVALID_SLAB_NODE,
        }
    }
}

/// 某个 CPU 在某个 size class 下的本地缓存状态。
///
/// 它的目标是把最热的小对象分配/释放留在本地 CPU 上完成，尽量少碰全局 slab 状态。
struct PerCpuCacheState {
    entries: [CacheEntry; CACHE_CAPACITY],
    count: usize,
}

impl PerCpuCacheState {
    const fn new() -> Self {
        Self {
            entries: [CacheEntry::empty(); CACHE_CAPACITY],
            count: 0,
        }
    }

    fn pop(&mut self) -> Option<CacheEntry> {
        if self.count == 0 {
            return None;
        }
        self.count -= 1;
        let entry = self.entries[self.count];
        self.entries[self.count] = CacheEntry::empty();
        Some(entry)
    }

    fn push(&mut self, entry: CacheEntry) -> bool {
        if self.count >= CACHE_CAPACITY {
            return false;
        }
        self.entries[self.count] = entry;
        self.count += 1;
        true
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

#[derive(Clone, Copy)]
/// 单个 slab 的运行时状态。
///
/// 一个 slab 对应一批连续页和固定大小对象槽，`alloc_bitmap`/`cache_bitmap` 共同描述
/// 每个槽当前是空闲、已分配还是暂存在 per-CPU cache 中。
struct Slab {
    base_addr: usize,
    paddr: usize,
    page_count: u16,
    total_objects: u16,
    allocated_objects: u16,
    cached_objects: u16,
    /// 下次位图扫描的起点。
    ///
    /// slab 对象通常按低地址递增分配；如果每次 cache miss 都从 0 开始扫描，活跃 slab
    /// 越满，前缀已分配位带来的重复检查越多。这个 hint 让分配路径从上次命中位置之后
    /// 继续，并在 flush 释放真实空槽时回退到被释放槽位。
    next_free_hint: u16,
    alloc_bitmap: [u64; BITMAP_WORDS],
    cache_bitmap: [u64; BITMAP_WORDS],
    active: bool,
}

impl Slab {
    const fn empty() -> Self {
        Self {
            base_addr: 0,
            paddr: 0,
            page_count: 0,
            total_objects: 0,
            allocated_objects: 0,
            cached_objects: 0,
            next_free_hint: 0,
            alloc_bitmap: [0; BITMAP_WORDS],
            cache_bitmap: [0; BITMAP_WORDS],
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
        self.cached_objects = 0;
        self.next_free_hint = 0;
        self.alloc_bitmap = [0; BITMAP_WORDS];
        self.cache_bitmap = [0; BITMAP_WORDS];
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

    fn allocate(&mut self, obj_size: usize, cached: bool) -> Option<usize> {
        // slab 内部的分配只是位图扫描，不做任何跨 slab 策略判断。`cached=true` 时，
        // 对象会以“已占用但暂存在 per-CPU cache”的状态返回给上层缓存补货逻辑。
        if !self.active || self.allocated_objects >= self.total_objects {
            return None;
        }

        let Some(idx) = self.find_free_slot() else {
            self.next_free_hint = self.total_objects;
            return None;
        };

        self.set_alloc_bit(idx, true);
        self.set_cache_bit(idx, cached);
        self.allocated_objects += 1;
        if cached {
            self.cached_objects += 1;
        }
        self.next_free_hint = next_hint_after(idx, self.total_objects as usize) as u16;
        Some(self.base_addr + idx * obj_size)
    }

    fn stage_cached_free(&mut self, ptr: usize, obj_size: usize) -> bool {
        // staged free 不会立刻把对象变回真正空槽，而是先打上 cache 位，表示这个对象被
        // 回收到本地 cache，可供同 CPU 后续快速复用。
        let Some(idx) = self.object_index(ptr, obj_size) else {
            return false;
        };
        if !self.alloc_bit(idx) || self.cache_bit(idx) {
            return false;
        }
        self.set_cache_bit(idx, true);
        self.cached_objects += 1;
        true
    }

    fn commit_cache_alloc(&mut self, ptr: usize, obj_size: usize) -> bool {
        // per-CPU cache 命中时，对象其实一直处于“allocated + cached”状态。这里把
        // cache 位清掉，完成从“缓存持有”到“重新借出给调用者”的语义切换。
        let Some(idx) = self.object_index(ptr, obj_size) else {
            return false;
        };
        if !self.alloc_bit(idx) || !self.cache_bit(idx) {
            return false;
        }
        self.set_cache_bit(idx, false);
        self.cached_objects = self.cached_objects.saturating_sub(1);
        true
    }

    fn flush_cached(&mut self, ptr: usize, obj_size: usize) -> bool {
        // flush 是把 cache 中的对象真正还回 slab，自此该槽重新可分配。因此 alloc 位和
        // cache 位都要被清掉，计数也要同步回退。
        let Some(idx) = self.object_index(ptr, obj_size) else {
            return false;
        };
        if !self.alloc_bit(idx) || !self.cache_bit(idx) {
            return false;
        }
        self.set_cache_bit(idx, false);
        self.set_alloc_bit(idx, false);
        self.allocated_objects = self.allocated_objects.saturating_sub(1);
        self.cached_objects = self.cached_objects.saturating_sub(1);
        self.next_free_hint = idx.min(self.next_free_hint as usize) as u16;
        true
    }

    fn is_empty(&self) -> bool {
        self.active && self.allocated_objects == 0 && self.cached_objects == 0
    }

    fn alloc_bit(&self, idx: usize) -> bool {
        bit_is_set(&self.alloc_bitmap, idx)
    }

    fn cache_bit(&self, idx: usize) -> bool {
        bit_is_set(&self.cache_bitmap, idx)
    }

    fn set_alloc_bit(&mut self, idx: usize, set: bool) {
        set_bit(&mut self.alloc_bitmap, idx, set);
    }

    fn set_cache_bit(&mut self, idx: usize, set: bool) {
        set_bit(&mut self.cache_bitmap, idx, set);
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
}

struct ZoneState {
    slab_head: usize,
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
                reclaimed_slabs: 0,
                free_slab_nodes: 0,
            },
        }
    }

    fn on_alloc(&mut self, obj_size: usize) {
        self.stats.active_objects += 1;
        self.stats.active_bytes += obj_size;
    }

    fn on_free(&mut self, obj_size: usize) {
        self.stats.active_objects = self.stats.active_objects.saturating_sub(1);
        self.stats.active_bytes = self.stats.active_bytes.saturating_sub(obj_size);
    }

    fn try_allocate_user_object(&mut self, obj_size: usize) -> Option<usize> {
        let mut node_addr = self.slab_head;
        while node_addr != 0 {
            let node = slab_node_mut(node_addr);
            if let Some(ptr) = node.slab.allocate(obj_size, false) {
                self.on_alloc(obj_size);
                return Some(ptr);
            }
            node_addr = node.next;
        }
        None
    }

    fn allocate_cached_batch(&mut self, obj_size: usize, out: &mut [CacheEntry]) -> usize {
        // 批量补货的目标是提前准备一组 cached 对象，降低后续热路径反复拿全局锁的次数。
        let mut produced = 0;

        while produced < out.len() {
            let mut node_addr = self.slab_head;
            let mut entry = None;
            while node_addr != 0 {
                let node = slab_node_mut(node_addr);
                if let Some(ptr) = node.slab.allocate(obj_size, true) {
                    entry = Some(CacheEntry {
                        ptr,
                        slab_node: node_addr,
                    });
                    break;
                }
                node_addr = node.next;
            }
            let Some(entry) = entry else {
                break;
            };
            out[produced] = entry;
            produced += 1;
        }
        if produced > 0 {
            self.stats.cache_refills += 1;
        }
        produced
    }

    fn insert_slab_node(&mut self, node_addr: usize) {
        let node = slab_node_mut(node_addr);
        let block_pages = node.slab.page_count as usize;
        node.next = self.slab_head;
        self.slab_head = node_addr;
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
        node.next = self.free_node_head;
        self.free_node_head = node_addr;
        self.free_node_count += 1;
        self.stats.free_slab_nodes = self.free_node_count;
    }

    fn stage_cached_free(&mut self, ptr: usize, obj_size: usize) -> Option<usize> {
        // ZoneState 级别的 staged free 负责在本 zone 的所有 slab 中定位归属对象，并把
        // “用户已释放”的事实同步到统计信息上；位图状态细节仍由具体 slab 执行。
        let mut node_addr = self.slab_head;
        while node_addr != 0 {
            let node = slab_node_mut(node_addr);
            if node.slab.stage_cached_free(ptr, obj_size) {
                self.on_free(obj_size);
                return Some(node_addr);
            }
            node_addr = node.next;
        }
        self.stats.invalid_frees += 1;
        None
    }

    fn commit_cache_alloc(&mut self, entry: CacheEntry, obj_size: usize) -> bool {
        // cache entry 里保存了对象所属 slab 节点，因此命中后可以直接回到原 slab 撤销
        // cache 标记，而无需再次全表搜索。
        if entry.slab_node == INVALID_SLAB_NODE {
            return false;
        }
        if slab_node_mut(entry.slab_node)
            .slab
            .commit_cache_alloc(entry.ptr, obj_size)
        {
            self.on_alloc(obj_size);
            return true;
        }
        false
    }

    fn flush_cached_entries(&mut self, entries: &[CacheEntry], obj_size: usize) {
        // 当本地 cache 过满时，批量冲刷一批对象回 slab。这里故意保持顺序、朴素的实现，
        // 因为它走的是冷一些的回压路径，稳定性比极致优化更重要。
        let mut flushed = 0;
        for entry in entries {
            if entry.slab_node == INVALID_SLAB_NODE {
                continue;
            }
            if slab_node_mut(entry.slab_node)
                .slab
                .flush_cached(entry.ptr, obj_size)
            {
                flushed += 1;
            }
        }
        if flushed > 0 {
            self.stats.cache_flushes += 1;
        }
    }

    fn note_grow_failure(&mut self, reason: SlabGrowError) {
        self.stats.grow_failures += 1;
        if matches!(
            reason,
            SlabGrowError::BackedRange | SlabGrowError::InvalidBacking
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
                if prev == 0 {
                    self.slab_head = next;
                } else {
                    slab_node_mut(prev).next = next;
                }
                let node = slab_node_mut(current);
                node.slab.active = false;
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
}

#[derive(Clone, Copy, Debug)]
enum SlabGrowError {
    Metadata,
    BackedRange,
    InvalidBacking,
    Inactive,
}

fn allocate_slab_node(
    obj_size: usize,
    pages_per_slab: usize,
    phys: &Mutex<BuddyAllocator>,
    vmem: &KernelAddressSpace,
    reusable_node: Option<usize>,
) -> Result<usize, SlabGrowError> {
    let order = pages_to_order(pages_per_slab);
    let range = vmem
        .alloc_kernel_backed_range(order, phys, crate::PagePolicy::BaseOnly)
        .map_err(|_| SlabGrowError::BackedRange)?;
    let block_pages = 1usize << order;
    if range.paddr & (PAGE_SIZE - 1) != 0 {
        let _ = vmem.free_kernel_backed_range(range, phys);
        return Err(SlabGrowError::InvalidBacking);
    }

    let node_addr = match reusable_node {
        Some(node_addr) => node_addr,
        None => {
            let node_addr = crate::alloc_internal_metadata(Layout::new::<SlabNode>()) as usize;
            if node_addr == 0 {
                let _ = vmem.free_kernel_backed_range(range, phys);
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
    if !node.slab.active {
        let _ = vmem.free_kernel_backed_range(range, phys);
        return Err(SlabGrowError::Inactive);
    }

    Ok(node_addr)
}

struct Zone {
    size_class: usize,
    pages_per_slab: usize,
    state: Mutex<ZoneState>,
    caches: [PerCpuCache; MAX_CPUS],
}

impl Zone {
    const fn new(size_class: usize) -> Self {
        Self {
            size_class,
            pages_per_slab: pages_per_slab(size_class),
            state: Mutex::new(ZoneState::new()),
            caches: [const { PerCpuCache::new() }; MAX_CPUS],
        }
    }

    fn alloc(
        &self,
        cpu: usize,
        phys: &Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> *mut u8 {
        // Zone 的热路径是三段式：
        // 1. 先看 per-CPU cache；
        // 2. miss 后再看全局 slab 状态；
        // 3. 成功后顺便为本地 cache 批量补货。
        let cache = &self.caches[cpu];

        // 第一步：尝试从本地缓存获取（短暂持有 cache 锁）
        let cache_entry = {
            let mut cache_guard = cache.inner.lock();
            cache_guard.pop()
        }; // cache lock released here

        // 第二步：现在只持有 state 锁
        let mut state = self.state.lock();
        state.stats.alloc_requests += 1;

        // 尝试使用缓存的条目
        if let Some(entry) = cache_entry {
            if state.commit_cache_alloc(entry, self.size_class) {
                state.stats.cache_hits += 1;
                return entry.ptr as *mut u8;
            }
            state.stats.invalid_frees += 1;
            panic!(
                "[alloc][invariant] slab cache entry invalid class={} cpu={} ptr={:#x}",
                self.size_class, cpu, entry.ptr,
            );
        }

        state.stats.cache_misses += 1;
        let ptr = if let Some(ptr) = state.try_allocate_user_object(self.size_class) {
            drop(state);
            ptr
        } else {
            drop(state);
            let mut grow_attempts = 0;
            loop {
                if grow_attempts >= MAX_GROW_ATTEMPTS {
                    let mut state = self.state.lock();
                    state.stats.grow_failures += 1;
                    drop(state);
                    self.reclaim_empty_slabs(Some((phys, vmem)));
                    return null_mut();
                }

                let reusable_node = {
                    let mut state = self.state.lock();
                    state.pop_reusable_slab_node()
                };

                match allocate_slab_node(
                    self.size_class,
                    self.pages_per_slab,
                    phys,
                    vmem,
                    reusable_node,
                ) {
                    Ok(node_addr) => {
                        let mut state = self.state.lock();
                        state.insert_slab_node(node_addr);
                        if let Some(ptr) = state.try_allocate_user_object(self.size_class) {
                            break ptr;
                        }
                    }
                    Err(err) => {
                        let mut state = self.state.lock();
                        if let Some(node_addr) = reusable_node {
                            state.push_reusable_slab_node(node_addr);
                        }
                        state.note_grow_failure(err);
                        return null_mut();
                    }
                }
                grow_attempts += 1;
            }
        };

        // 第三步：为缓存补货（重新获取 cache 锁）
        let cache_slots = {
            let cache_guard = cache.inner.lock();
            CACHE_CAPACITY
                .saturating_sub(cache_guard.count)
                .min(REFILL_BATCH)
        };

        if cache_slots > 0 {
            let mut refill = [CacheEntry::empty(); REFILL_BATCH];
            let produced = {
                let mut state = self.state.lock();
                state.allocate_cached_batch(self.size_class, &mut refill[..cache_slots])
            };

            // 重新获取 cache 锁以推入条目
            let mut overflow = [CacheEntry::empty(); REFILL_BATCH];
            let mut overflow_count = 0usize;
            {
                let mut cache_guard = cache.inner.lock();
                for entry in refill.into_iter().take(produced) {
                    if !cache_guard.push(entry) {
                        overflow[overflow_count] = entry;
                        overflow_count += 1;
                    }
                }
            }
            if overflow_count > 0 {
                let mut state = self.state.lock();
                state.flush_cached_entries(&overflow[..overflow_count], self.size_class);
            }
        }

        ptr as *mut u8
    }

    fn free(
        &self,
        ptr: usize,
        cpu: usize,
        backing: Option<(&Mutex<BuddyAllocator>, &KernelAddressSpace)>,
    ) -> bool {
        // 释放时优先回收到本地 cache；只有 cache 太满时，才把部分 cached 对象真正
        // 冲刷回 slab 位图状态，从而尽量让热对象停留在本地 CPU。
        let cache = &self.caches[cpu];
        let mut drained = [CacheEntry::empty(); CACHE_CAPACITY / 2];
        let drained_count = {
            let mut cache_guard = cache.inner.lock();
            if cache_guard.count < CACHE_CAPACITY {
                0
            } else {
                let mut drained_count = 0usize;
                for slot in drained.iter_mut() {
                    let Some(entry) = cache_guard.pop() else {
                        break;
                    };
                    *slot = entry;
                    drained_count += 1;
                }
                drained_count
            }
        };
        if drained_count > 0 {
            let mut state = self.state.lock();
            state.flush_cached_entries(&drained[..drained_count], self.size_class);
        }

        let staged = {
            let mut state = self.state.lock();
            state.stats.free_requests += 1;
            state.stage_cached_free(ptr, self.size_class)
        };

        let Some(slab_node) = staged else {
            self.reclaim_empty_slabs(backing);
            return false;
        };

        let entry = CacheEntry { ptr, slab_node };
        let pushed = {
            let mut cache_guard = cache.inner.lock();
            cache_guard.push(entry)
        };
        if !pushed {
            let mut state = self.state.lock();
            state.flush_cached_entries(&[entry], self.size_class);
        }
        self.reclaim_empty_slabs(backing);
        true
    }

    fn reclaim_empty_slabs(&self, backing: Option<(&Mutex<BuddyAllocator>, &KernelAddressSpace)>) {
        let Some((phys, vmem)) = backing else {
            return;
        };
        loop {
            let range = {
                let mut state = self.state.lock();
                state.take_reclaimable_empty_slab()
            };
            let Some(range) = range else {
                break;
            };
            if let Err(err) = vmem.free_kernel_backed_range(range, phys) {
                panic!(
                    "[alloc][invariant] slab empty range reclaim failed class={} vaddr={:#x} paddr={:#x} size={} err={:?}",
                    self.size_class, range.vaddr, range.paddr, range.size, err
                );
            }
        }
    }

    fn snapshot(&self) -> SlabStats {
        self.state.lock().stats
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
}

pub struct SlabAllocator {
    zones: [Zone; SIZE_CLASS_COUNT],
    cpu_count: AtomicUsize,
    initialized: AtomicBool,
}

impl SlabAllocator {
    pub const fn new() -> Self {
        Self {
            zones: [
                Zone::new(SIZE_CLASSES[0]),
                Zone::new(SIZE_CLASSES[1]),
                Zone::new(SIZE_CLASSES[2]),
                Zone::new(SIZE_CLASSES[3]),
                Zone::new(SIZE_CLASSES[4]),
                Zone::new(SIZE_CLASSES[5]),
                Zone::new(SIZE_CLASSES[6]),
                Zone::new(SIZE_CLASSES[7]),
                Zone::new(SIZE_CLASSES[8]),
                Zone::new(SIZE_CLASSES[9]),
                Zone::new(SIZE_CLASSES[10]),
                Zone::new(SIZE_CLASSES[11]),
                Zone::new(SIZE_CLASSES[12]),
                Zone::new(SIZE_CLASSES[13]),
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

    pub fn same_size_class(old_layout: Layout, new_layout: Layout) -> bool {
        Self::class_index_for(old_layout) == Self::class_index_for(new_layout)
    }

    pub fn owns(&self, ptr: usize) -> bool {
        self.zone_index_for_ptr(ptr).is_some()
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
        self.zones[zone_idx].free(record.ptr, cpu, backing)
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
            out.reclaimed_slabs += stats.reclaimed_slabs;
            out.free_slab_nodes += stats.free_slab_nodes;
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
        Self::new()
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
    for (idx, class) in SIZE_CLASSES.iter().enumerate() {
        if size <= *class {
            return Some(idx);
        }
    }
    None
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
    // slab 的分配态和缓存态都压在位图里。热点路径直接操作 bit，是为了把每次对象状态
    // 切换控制在非常低的常数开销内。
    let word = idx / 64;
    if word >= BITMAP_WORDS {
        return false;
    }
    let bit = idx % 64;
    (bits[word] & (1u64 << bit)) != 0
}

#[inline]
fn set_bit(bits: &mut [u64; BITMAP_WORDS], idx: usize, set: bool) {
    // 所有对象槽位状态迁移最终都会收敛到“某一位设为 1 或清为 0”。把它集中到这里，
    // 可以保证 alloc/cache 两套位图操作拥有一致的边界检查与写入语义。
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
fn pages_to_order(pages: usize) -> usize {
    // slab 想申请任意页数，但 buddy 只接受 order。这里做的就是把需求向上折算成最小
    // 覆盖该页数的 2^order 块，作为下层物理页和虚拟区间申请的共同粒度。
    let mut order = 0;
    let mut block = 1;
    while block < pages {
        block <<= 1;
        order += 1;
    }
    order
}
