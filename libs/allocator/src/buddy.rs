//! 基于伙伴算法的物理页分配器。
//!
//! 这个模块管理“真正的物理页帧”。如果把整个 allocator 看成分层系统，那么它位于
//! 最底层，向上给虚拟地址空间、kernel heap、slab 和其它组件提供页级内存来源。
//!
//! 当前实现的几个关键目标是：
//!
//! - 支持多个物理内存段，而不是假设整块连续 RAM；
//! - 支持按 order 管理和合并页块，降低外部碎片；
//! - 支持不同 free list / zone，例如默认区和低端 DMA 区；
//! - 支持精确物理地址放置与大页对齐策略，满足内核映射需求。
//!
//! “segment-aware” 的意思是，分配器不会把所有 RAM 简化成单一线性池，而是保留
//! 来自 DTB/固件解析的段信息，并在每个 zone 中分别维护伙伴结构。这一点对真实平台
//! 很重要，因为可用内存通常并不天然连续，而且还可能夹杂保留区、DMA 约束区和设备
//! 窗口。

use core::alloc::Layout;
use core::ptr::null_mut;

use crate::boot::BootAllocator;
use crate::request::{
    AllocationRequestError, MemoryPlacement, PhysicalAllocRequest, PhysicalAllocation,
};

#[inline]
fn now_ns() -> u64 {
    log::get_timestamp_ns()
}

#[inline]
fn elapsed_us(start_ns: u64) -> u64 {
    now_ns().saturating_sub(start_ns) / 1_000
}

/// 基本页大小：4 KiB。
pub const PAGE_SIZE: usize = 4096;
/// 基本页偏移位数。
#[allow(dead_code)]
pub const PAGE_SHIFT: usize = 12;
/// 当前机器字长下最大可追踪的 order 值。
///
/// `order` 会被频繁换算成字节大小 `(1 << order) * PAGE_SIZE`；保留最高位可以
/// 保证这个乘法始终落在 `usize` 可表示范围内，避免最大 order 在不同构建模式下
/// 触发溢出或回绕。
pub const MAX_TRACKED_ORDER: usize = usize::BITS as usize - PAGE_SHIFT - 1;
/// 按用途拆分的空闲链表数量。
pub const VM_NFREELIST: usize = 2;
/// 默认空闲链表索引。
pub const VM_FREELIST_DEFAULT: usize = 0;
/// 低端 DMA 空闲链表索引。
pub const VM_FREELIST_DMA: usize = 1;
/// DMA 区域边界（16 MiB）。
pub const DMA_ZONE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySegment {
    pub start: usize,
    pub size: usize,
}

impl MemorySegment {
    pub const fn end(self) -> usize {
        self.start.saturating_add(self.size)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PageState {
    Free = 0,
    Allocated = 1,
    BuddyTail = 2,
    Reserved = 3,
}

/// 兼容旧接口保留的页描述符类型。
///
/// 稀疏实现不再为整机 RAM 物化全局 `PageInfo[]`；因此 `page_info()/iter_pages()`
/// 这类“按页遍历”接口仅保留 ABI/编译兼容性，不再作为主路径数据源。
#[derive(Clone, Copy)]
pub struct PageInfo {
    pub free_next: u32,
    pub free_prev: u32,
    pub ref_count: u16,
    #[allow(dead_code)]
    pub slab_zone_id: u16,
    pub state: PageState,
    pub order: u8,
    #[allow(dead_code)]
    pub gc_mark: u8,
}

impl PageInfo {
    pub const fn empty() -> Self {
        Self {
            free_next: u32::MAX,
            free_prev: u32::MAX,
            ref_count: 0,
            slab_zone_id: 0,
            state: PageState::Reserved,
            order: 0,
            gc_mark: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BuddyStats {
    pub total_pages: usize,
    pub allocated_pages: usize,
    pub free_pages: usize,
    pub reserved_pages: usize,
    pub metadata_pages: usize,
    pub segment_count: usize,
    pub max_order: usize,
    pub hash_bucket_count: usize,
    pub node_capacity: usize,
    pub node_used: usize,
    pub free_count_per_order: [usize; MAX_TRACKED_ORDER + 1],
    pub alloc_requests: u64,
    pub free_requests: u64,
    pub split_count: u64,
    pub coalesce_count: u64,
    pub alloc_failures: u64,
}

impl BuddyStats {
    pub const fn new() -> Self {
        Self {
            total_pages: 0,
            allocated_pages: 0,
            free_pages: 0,
            reserved_pages: 0,
            metadata_pages: 0,
            segment_count: 0,
            max_order: 0,
            hash_bucket_count: 0,
            node_capacity: 0,
            node_used: 0,
            free_count_per_order: [0; MAX_TRACKED_ORDER + 1],
            alloc_requests: 0,
            free_requests: 0,
            split_count: 0,
            coalesce_count: 0,
            alloc_failures: 0,
        }
    }
}

impl Default for BuddyStats {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuddyInitError {
    EmptyMemoryMap,
    MetadataOutOfMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuddyAllocError {
    NotInitialized,
    InvalidOrder,
    InvalidAddress,
    BlockOutOfRange,
    BlockNotFree,
    Fragmented,
    MetadataOutOfMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuddyFreeError {
    NotInitialized,
    InvalidOrder,
    InvalidAddress,
    BlockOutOfRange,
    NotAllocated,
    OrderMismatch,
    CorruptTail,
}

#[derive(Clone, Copy)]
struct BuddySegment {
    range: MemorySegment,
    total_pages: usize,
    max_order: usize,
    fl_type: usize,
}

impl BuddySegment {
    const fn empty() -> Self {
        Self {
            range: MemorySegment { start: 0, size: 0 },
            total_pages: 0,
            max_order: 0,
            fl_type: VM_FREELIST_DEFAULT,
        }
    }
}

#[derive(Clone, Copy)]
struct BlockNode {
    start: usize,
    order: u8,
    seg_idx: u32,
    fl_type: u8,
    is_free: bool,
    ref_count: u16,
    slab_zone_id: u16,
    gc_mark: u8,
    free_next: usize,
    free_prev: usize,
    hash_next: usize,
}

impl BlockNode {
    const fn empty() -> Self {
        Self {
            start: 0,
            order: 0,
            seg_idx: 0,
            fl_type: 0,
            is_free: false,
            ref_count: 0,
            slab_zone_id: 0,
            gc_mark: 0,
            free_next: 0,
            free_prev: 0,
            hash_next: 0,
        }
    }
}

const FREE_HEADS: usize = VM_NFREELIST * (MAX_TRACKED_ORDER + 1);
const TARGET_HASH_CHAIN: usize = 4;
const MAX_HASH_BUCKETS: usize = 1_048_576;
const MIN_HASH_BUCKETS: usize = 1_024;
const METADATA_RANGE_COUNT: usize = 1;

struct MetadataBump {
    cursor: usize,
    end: usize,
}

impl MetadataBump {
    const fn new(base: usize, end: usize) -> Self {
        Self { cursor: base, end }
    }

    fn alloc_array<T>(&mut self, count: usize) -> Option<*mut T> {
        if count == 0 {
            return Some(null_mut());
        }

        let layout = Layout::array::<T>(count).ok()?;
        let aligned = align_up_checked(self.cursor, layout.align())?;
        let next = aligned.checked_add(layout.size())?;
        if next > self.end {
            return None;
        }
        self.cursor = next;
        Some(aligned as *mut T)
    }
}

pub struct BuddyAllocator {
    segments: *mut BuddySegment,
    segment_count: usize,
    reserved_ranges: *mut MemorySegment,
    reserved_range_count: usize,
    metadata_ranges: *mut MemorySegment,
    metadata_range_count: usize,
    hash_buckets: *mut usize,
    hash_bucket_count: usize,
    nodes: *mut BlockNode,
    node_capacity: usize,
    node_used: usize,
    free_heads: [usize; FREE_HEADS],
    node_freelist: usize,
    initialized: bool,
    stats: BuddyStats,
}

#[allow(dead_code)]
impl BuddyAllocator {
    pub const fn new() -> Self {
        Self {
            segments: null_mut(),
            segment_count: 0,
            reserved_ranges: null_mut(),
            reserved_range_count: 0,
            metadata_ranges: null_mut(),
            metadata_range_count: 0,
            hash_buckets: null_mut(),
            hash_bucket_count: 0,
            nodes: null_mut(),
            node_capacity: 0,
            node_used: 0,
            free_heads: [0; FREE_HEADS],
            node_freelist: 0,
            initialized: false,
            stats: BuddyStats::new(),
        }
    }

    pub fn init(
        &mut self,
        segments: &[MemorySegment],
        reserved_regions: &[(usize, usize)],
        phys_to_virt: fn(usize) -> usize,
        _boot: &BootAllocator,
    ) -> Result<(), BuddyInitError> {
        let init_start_ns = now_ns();
        self.reset();

        let effective_count = count_effective_segments(segments);
        if effective_count == 0 {
            return Err(BuddyInitError::EmptyMemoryMap);
        }

        let total_pages =
            estimate_total_pages(segments).ok_or(BuddyInitError::MetadataOutOfMemory)?;
        if total_pages == 0 {
            return Err(BuddyInitError::EmptyMemoryMap);
        }
        if total_pages > usize::MAX / PAGE_SIZE {
            return Err(BuddyInitError::MetadataOutOfMemory);
        }

        let stored_reserved = reserved_regions
            .iter()
            .filter(|(start, end)| end > start)
            .count();
        let bucket_count = choose_hash_bucket_count(total_pages);
        let node_capacity = total_pages;
        let metadata_bytes = buddy_metadata_bytes(
            effective_count,
            stored_reserved,
            bucket_count,
            node_capacity,
        )
        .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        let metadata_start_ns = now_ns();
        let metadata_range = carve_metadata_range(segments, reserved_regions, metadata_bytes)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        let metadata_base = phys_to_virt(metadata_range.start);
        let metadata_end = metadata_base
            .checked_add(metadata_range.size)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        unsafe {
            core::ptr::write_bytes(metadata_base as *mut u8, 0, metadata_range.size);
        }
        let mut metadata = MetadataBump::new(metadata_base, metadata_end);

        let segment_ptr = metadata
            .alloc_array::<BuddySegment>(effective_count)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        for idx in 0..effective_count {
            unsafe { segment_ptr.add(idx).write(BuddySegment::empty()) };
        }

        let reserved_ptr = metadata
            .alloc_array::<MemorySegment>(stored_reserved)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        let metadata_range_ptr = metadata
            .alloc_array::<MemorySegment>(1)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        unsafe {
            metadata_range_ptr.write(metadata_range);
        }
        let bucket_ptr = metadata
            .alloc_array::<usize>(bucket_count)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        let node_ptr = metadata
            .alloc_array::<BlockNode>(node_capacity)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        let metadata_us = elapsed_us(metadata_start_ns);

        self.segments = segment_ptr;
        self.hash_buckets = bucket_ptr;
        self.hash_bucket_count = bucket_count;
        self.reserved_ranges = reserved_ptr;
        self.metadata_ranges = metadata_range_ptr;
        self.metadata_range_count = 1;
        self.nodes = node_ptr;
        self.node_capacity = node_capacity;
        self.node_used = 0;

        let segment_start_ns = now_ns();
        let mut seg_out = 0usize;
        for &raw_segment in segments {
            for_each_effective_segment(raw_segment, |segment, fl_type| {
                let total_pages = segment.size / PAGE_SIZE;
                let seg = unsafe { &mut *segment_ptr.add(seg_out) };
                *seg = BuddySegment {
                    range: segment,
                    total_pages,
                    max_order: max_order_for_pages(total_pages),
                    fl_type,
                };
                self.stats.total_pages = self
                    .stats
                    .total_pages
                    .checked_add(total_pages)
                    .unwrap_or(usize::MAX);
                self.stats.max_order = self.stats.max_order.max(seg.max_order);
                seg_out += 1;
            });
        }
        self.segment_count = seg_out;
        self.stats.segment_count = seg_out;
        self.stats.metadata_pages = metadata_range.size / PAGE_SIZE;
        self.stats.hash_bucket_count = self.hash_bucket_count;
        self.stats.node_capacity = self.node_capacity;
        let segment_us = elapsed_us(segment_start_ns);

        let reserved_start_ns = now_ns();
        let mut reserved_out = 0usize;
        for &(start, end) in reserved_regions {
            if end <= start {
                continue;
            }
            unsafe {
                reserved_ptr.add(reserved_out).write(MemorySegment {
                    start,
                    size: end - start,
                });
            }
            reserved_out += 1;
        }
        self.reserved_range_count = reserved_out;
        let reserved_us = elapsed_us(reserved_start_ns);

        let seed_start_ns = now_ns();
        self.seed_initial_free_ranges(reserved_regions)?;
        let seed_us = elapsed_us(seed_start_ns);
        self.stats.reserved_pages = self
            .stats
            .total_pages
            .saturating_sub(self.stats.free_pages + self.stats.allocated_pages);
        self.initialized = true;

        log::info!(
            "[alloc][buddy] initialized sections={} total_ram={} MiB metadata={} KiB metadata_phys={:#x} nodes={} buckets={}",
            self.segment_count,
            (self.stats.total_pages * PAGE_SIZE) / (1024 * 1024),
            metadata_bytes / 1024,
            metadata_range.start,
            self.node_capacity,
            self.hash_bucket_count,
        );
        log::info!(
            "[alloc][buddy][timing] total={} us metadata={} us segments={} us reserved={} us seed={} us",
            elapsed_us(init_start_ns),
            metadata_us,
            segment_us,
            reserved_us,
            seed_us,
        );
        Ok(())
    }

    pub fn release_bootmem(&mut self) {
        // 兼容壳：稀疏实现已经在 `init()` 内完成全部播种。
    }

    pub fn alloc_pages(&mut self, order: usize) -> Option<usize> {
        if !self.initialized || order > MAX_TRACKED_ORDER {
            return None;
        }

        self.stats.alloc_requests += 1;

        if let Some(addr) = self.alloc_from_zone(order, VM_FREELIST_DEFAULT) {
            return Some(addr);
        }
        if let Some(addr) = self.alloc_from_zone(order, VM_FREELIST_DMA) {
            return Some(addr);
        }

        self.stats.alloc_failures += 1;
        None
    }

    pub fn alloc_pages_from_zone(&mut self, order: usize, fl_type: usize) -> Option<usize> {
        if !self.initialized || order > MAX_TRACKED_ORDER || fl_type >= VM_NFREELIST {
            return None;
        }

        self.stats.alloc_requests += 1;
        let result = self.alloc_from_zone(order, fl_type);
        if result.is_none() {
            self.stats.alloc_failures += 1;
        }
        result
    }

    pub fn alloc_pages_with(
        &mut self,
        request: &PhysicalAllocRequest,
    ) -> Result<PhysicalAllocation, BuddyAllocError> {
        if !self.initialized {
            return Err(BuddyAllocError::NotInitialized);
        }

        let order = required_order_for_request(request)?;
        let page_size = (1usize << order) * PAGE_SIZE;

        let paddr = match request.placement {
            MemoryPlacement::ExactPhys(addr) => self.alloc_pages_exact(addr, order)?,
            MemoryPlacement::LowMem => self
                .alloc_pages_from_zone(order, VM_FREELIST_DMA)
                .ok_or(BuddyAllocError::Fragmented)?,
            MemoryPlacement::Any => self.alloc_pages(order).ok_or(BuddyAllocError::Fragmented)?,
        };

        Ok(PhysicalAllocation {
            paddr,
            size: page_size,
            order,
            page_size,
        })
    }

    pub fn free_allocation(
        &mut self,
        allocation: PhysicalAllocation,
    ) -> Result<(), BuddyFreeError> {
        self.free_pages(allocation.paddr, allocation.order)
    }

    pub fn alloc_pages_exact(
        &mut self,
        addr: usize,
        order: usize,
    ) -> Result<usize, BuddyAllocError> {
        if !self.initialized {
            return Err(BuddyAllocError::NotInitialized);
        }
        if order > MAX_TRACKED_ORDER {
            return Err(BuddyAllocError::InvalidOrder);
        }

        let Some((seg_idx, page_idx)) = self.page_location(addr) else {
            return Err(BuddyAllocError::InvalidAddress);
        };
        let seg = *self
            .segment(seg_idx)
            .ok_or(BuddyAllocError::InvalidAddress)?;
        if order > seg.max_order {
            return Err(BuddyAllocError::InvalidOrder);
        }

        let block_pages = 1usize << order;
        if (addr & (block_pages * PAGE_SIZE - 1)) != 0 {
            return Err(BuddyAllocError::InvalidAddress);
        }
        if page_idx + block_pages > seg.total_pages {
            return Err(BuddyAllocError::BlockOutOfRange);
        }

        self.stats.alloc_requests += 1;

        let target_pfn = addr / PAGE_SIZE;
        for current_order in order..=seg.max_order {
            let current_pages = 1usize << current_order;
            let current_pfn = align_down(target_pfn, current_pages);
            let current_addr = current_pfn * PAGE_SIZE;
            if current_addr < seg.range.start || current_addr >= seg.range.end() {
                continue;
            }
            let current_page = (current_addr - seg.range.start) / PAGE_SIZE;
            if current_page + current_pages > seg.total_pages {
                continue;
            }
            let node_addr = self.hash_find(current_addr);
            if node_addr == 0 {
                continue;
            }
            let node = node_ref(node_addr);
            if !node.is_free
                || node.order as usize != current_order
                || node.seg_idx as usize != seg_idx
            {
                continue;
            }

            if let Some(result) = self.allocate_exact_from_node(node_addr, order, addr) {
                return Ok(result);
            }
        }

        self.stats.alloc_failures += 1;
        Err(BuddyAllocError::BlockNotFree)
    }

    pub fn free_pages(&mut self, addr: usize, order: usize) -> Result<(), BuddyFreeError> {
        if !self.initialized {
            return Err(BuddyFreeError::NotInitialized);
        }
        if order > MAX_TRACKED_ORDER {
            return Err(BuddyFreeError::InvalidOrder);
        }

        let node_addr = self.hash_find(addr);
        if node_addr == 0 {
            return Err(BuddyFreeError::NotAllocated);
        }
        let node = node_ref(node_addr);
        if node.is_free {
            return Err(BuddyFreeError::NotAllocated);
        }
        if node.order as usize != order {
            return Err(BuddyFreeError::OrderMismatch);
        }

        let seg_idx = node.seg_idx as usize;
        let seg = *self
            .segment(seg_idx)
            .ok_or(BuddyFreeError::InvalidAddress)?;
        if order > seg.max_order {
            return Err(BuddyFreeError::InvalidOrder);
        }

        self.stats.free_requests += 1;
        self.stats.allocated_pages = self.stats.allocated_pages.saturating_sub(1usize << order);
        self.stats.free_pages += 1usize << order;

        self.hash_remove(node_addr);

        let mut merged_addr = addr;
        let mut merged_order = order;
        let fl_type = seg.fl_type;

        while merged_order < seg.max_order {
            let block_pages = 1usize << merged_order;
            let pfn = merged_addr / PAGE_SIZE;
            let buddy_pfn = pfn ^ block_pages;
            let buddy_addr = buddy_pfn * PAGE_SIZE;
            if buddy_addr < seg.range.start {
                break;
            }
            let buddy_page = (buddy_addr - seg.range.start) / PAGE_SIZE;
            if buddy_page + block_pages > seg.total_pages {
                break;
            }

            let buddy_node_addr = self.hash_find(buddy_addr);
            if buddy_node_addr == 0 {
                break;
            }
            let buddy = node_ref(buddy_node_addr);
            if !buddy.is_free
                || buddy.order as usize != merged_order
                || buddy.seg_idx as usize != seg_idx
                || buddy.fl_type as usize != fl_type
            {
                break;
            }

            self.remove_from_free_list(buddy_node_addr);
            self.hash_remove(buddy_node_addr);
            self.recycle_node(buddy_node_addr);

            merged_addr = merged_addr.min(buddy_addr);
            merged_order += 1;
            self.stats.coalesce_count += 1;
        }

        let node = node_mut(node_addr);
        node.start = merged_addr;
        node.order = merged_order as u8;
        node.seg_idx = seg_idx as u32;
        node.fl_type = fl_type as u8;
        node.is_free = true;
        node.ref_count = 0;
        node.slab_zone_id = 0;
        node.gc_mark = 0;
        node.free_next = 0;
        node.free_prev = 0;
        node.hash_next = 0;

        self.hash_insert(node_addr);
        self.add_to_free_list(node_addr);
        Ok(())
    }

    pub fn alloc_frame(&mut self) -> Option<usize> {
        self.alloc_pages(0)
    }

    pub fn free_frame(&mut self, addr: usize) {
        let _ = self.free_pages(addr, 0);
    }

    pub fn page_info(&self, _addr: usize) -> Option<&PageInfo> {
        None
    }

    pub fn page_info_mut(&mut self, _addr: usize) -> Option<&mut PageInfo> {
        None
    }

    pub fn inc_ref(&mut self, addr: usize) {
        let node_addr = self.hash_find(addr);
        if node_addr == 0 {
            return;
        }
        let node = node_mut(node_addr);
        if !node.is_free {
            node.ref_count = node.ref_count.saturating_add(1);
        }
    }

    pub fn dec_ref(&mut self, addr: usize) -> bool {
        let node_addr = self.hash_find(addr);
        if node_addr == 0 {
            return false;
        }
        let node = node_mut(node_addr);
        if node.is_free || node.ref_count == 0 {
            return false;
        }
        node.ref_count -= 1;
        if node.ref_count == 0 {
            let order = node.order as usize;
            let _ = node;
            return self.free_pages(addr, order).is_ok();
        }
        false
    }

    pub fn stats(&self) -> BuddyStats {
        let mut stats = self.stats;
        stats.node_used = self.node_used;
        stats
    }

    pub fn free_bytes(&self) -> usize {
        self.stats.free_pages * PAGE_SIZE
    }

    pub fn allocated_bytes(&self) -> usize {
        self.stats.allocated_pages * PAGE_SIZE
    }

    pub fn get_total_pages(&self) -> usize {
        self.stats.total_pages
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn max_supported_order(&self) -> usize {
        self.stats.max_order
    }

    pub fn iter_segments(&self) -> BuddySegmentIter<'_> {
        BuddySegmentIter {
            allocator: self,
            index: 0,
        }
    }

    pub fn iter_reserved_ranges(&self) -> BuddyReservedRangeIter<'_> {
        BuddyReservedRangeIter {
            allocator: self,
            index: 0,
        }
    }

    pub fn iter_metadata_ranges(&self) -> BuddyMetadataRangeIter<'_> {
        BuddyMetadataRangeIter {
            allocator: self,
            index: 0,
        }
    }

    pub fn iter_pages(&self) -> BuddyPageIter<'_> {
        BuddyPageIter {
            allocator: self,
            done: true,
        }
    }

    pub fn iter_free_blocks(&self) -> BuddyFreeBlockIter<'_> {
        BuddyFreeBlockIter {
            allocator: self,
            fl_type: 0,
            order: 0,
            current: 0,
        }
    }

    pub fn set_gc_mark(&mut self, addr: usize, mark: u8) {
        let node_addr = self.hash_find(addr);
        if node_addr == 0 {
            return;
        }
        let node = node_mut(node_addr);
        if !node.is_free {
            node.gc_mark = mark;
        }
    }

    pub fn clear_all_gc_marks(&mut self) {
        for bucket in 0..self.hash_bucket_count {
            let mut node_addr = self.bucket_head(bucket);
            while node_addr != 0 {
                let node = node_mut(node_addr);
                if !node.is_free {
                    node.gc_mark = 0;
                }
                node_addr = node.hash_next;
            }
        }
    }

    pub fn set_slab_zone(&mut self, addr: usize, zone_id: u16) {
        let node_addr = self.hash_find(addr);
        if node_addr == 0 {
            return;
        }
        let node = node_mut(node_addr);
        if !node.is_free {
            node.slab_zone_id = zone_id;
        }
    }

    pub fn segment_index_for_addr(&self, addr: usize) -> Option<usize> {
        self.page_location(addr).map(|(segment_idx, _)| segment_idx)
    }

    fn reset(&mut self) {
        self.segments = null_mut();
        self.segment_count = 0;
        self.reserved_ranges = null_mut();
        self.reserved_range_count = 0;
        self.metadata_ranges = null_mut();
        self.metadata_range_count = 0;
        self.hash_buckets = null_mut();
        self.hash_bucket_count = 0;
        self.nodes = null_mut();
        self.node_capacity = 0;
        self.node_used = 0;
        self.free_heads = [0; FREE_HEADS];
        self.node_freelist = 0;
        self.initialized = false;
        self.stats = BuddyStats::new();
    }

    fn seed_initial_free_ranges(
        &mut self,
        reserved_regions: &[(usize, usize)],
    ) -> Result<(), BuddyInitError> {
        for seg_idx in 0..self.segment_count {
            let seg = *self
                .segment(seg_idx)
                .ok_or(BuddyInitError::MetadataOutOfMemory)?;
            let mut current_page = 0usize;

            while current_page < seg.total_pages {
                let Some((reserved_start, reserved_end)) =
                    self.next_reserved_page_run(&seg, current_page, reserved_regions)
                else {
                    self.seed_free_range(seg_idx, current_page, seg.total_pages)?;
                    break;
                };

                if current_page < reserved_start {
                    self.seed_free_range(seg_idx, current_page, reserved_start)?;
                }

                current_page = current_page.max(reserved_end);
                while let Some((overlap_start, overlap_end)) =
                    self.next_reserved_page_run(&seg, current_page, reserved_regions)
                {
                    if overlap_start > current_page {
                        break;
                    }
                    current_page = current_page.max(overlap_end);
                }
            }
        }
        Ok(())
    }

    fn next_reserved_page_run(
        &self,
        seg: &BuddySegment,
        current_page: usize,
        reserved_regions: &[(usize, usize)],
    ) -> Option<(usize, usize)> {
        let mut next = None;

        for &(start, end) in reserved_regions {
            if let Some(interval) = clipped_page_interval(seg, start, end) {
                if interval.1 > current_page {
                    next = choose_earlier_page_interval(next, interval);
                }
            }
        }

        for idx in 0..self.metadata_range_count {
            let range = unsafe { *self.metadata_ranges.add(idx) };
            if let Some(interval) = clipped_page_interval(seg, range.start, range.end()) {
                if interval.1 > current_page {
                    next = choose_earlier_page_interval(next, interval);
                }
            }
        }

        let (start, mut end) = next?;
        loop {
            let mut extended = false;

            for &(candidate_start, candidate_end) in reserved_regions {
                let Some((candidate_start, candidate_end)) =
                    clipped_page_interval(seg, candidate_start, candidate_end)
                else {
                    continue;
                };
                if candidate_start <= end && candidate_end > end {
                    end = candidate_end;
                    extended = true;
                }
            }

            for idx in 0..self.metadata_range_count {
                let range = unsafe { *self.metadata_ranges.add(idx) };
                let Some((candidate_start, candidate_end)) =
                    clipped_page_interval(seg, range.start, range.end())
                else {
                    continue;
                };
                if candidate_start <= end && candidate_end > end {
                    end = candidate_end;
                    extended = true;
                }
            }

            if !extended {
                break;
            }
        }

        Some((start, end.min(seg.total_pages)))
    }

    fn seed_free_range(
        &mut self,
        seg_idx: usize,
        range_start: usize,
        range_end: usize,
    ) -> Result<(), BuddyInitError> {
        let seg = *self
            .segment(seg_idx)
            .ok_or(BuddyInitError::MetadataOutOfMemory)?;
        let mut current = range_start;
        while current < range_end {
            let available = range_end - current;
            let start = seg.range.start + current * PAGE_SIZE;
            let pfn = start / PAGE_SIZE;
            let order = best_seed_order(seg.max_order, pfn, available);
            let block_pages = 1usize << order;
            let node_addr = self
                .new_node(start, order, seg_idx, seg.fl_type, true)
                .ok_or(BuddyInitError::MetadataOutOfMemory)?;
            self.hash_insert(node_addr);
            self.add_to_free_list(node_addr);
            self.stats.free_pages += block_pages;
            current += block_pages;
        }
        Ok(())
    }

    fn alloc_from_zone(&mut self, order: usize, fl_type: usize) -> Option<usize> {
        for current_order in order..=MAX_TRACKED_ORDER {
            let mut node_addr = self.free_head(fl_type, current_order);
            while node_addr != 0 {
                let next = node_ref(node_addr).free_next;
                if let Some(result) = self.allocate_from_free_node(node_addr, order) {
                    return Some(result);
                }
                node_addr = next;
            }
        }
        None
    }

    fn allocate_from_free_node(&mut self, node_addr: usize, target_order: usize) -> Option<usize> {
        let node = node_ref(node_addr);
        let current_order = node.order as usize;
        if current_order < target_order || !node.is_free {
            return None;
        }

        let needed = current_order - target_order;
        let mut split_nodes = [0usize; MAX_TRACKED_ORDER + 1];
        if !self.preallocate_nodes(needed, &mut split_nodes) {
            return None;
        }

        self.remove_from_free_list(node_addr);
        self.hash_remove(node_addr);

        let node = node_mut(node_addr);
        let seg_idx = node.seg_idx as usize;
        let fl_type = node.fl_type as usize;
        let current_start = node.start;

        (0..needed).for_each(|depth| {
            let split_order = current_order - depth - 1;
            let right_start = current_start + ((1usize << split_order) * PAGE_SIZE);
            let buddy_addr = split_nodes[depth];
            initialize_node(buddy_addr, right_start, split_order, seg_idx, fl_type, true);
            self.hash_insert(buddy_addr);
            self.add_to_free_list(buddy_addr);
            self.stats.split_count += 1;
            node_mut(node_addr).order = split_order as u8;
        });

        let node = node_mut(node_addr);
        node.start = current_start;
        node.order = target_order as u8;
        node.is_free = false;
        node.ref_count = 1;
        node.slab_zone_id = 0;
        node.gc_mark = 0;
        node.free_next = 0;
        node.free_prev = 0;
        node.hash_next = 0;
        self.hash_insert(node_addr);

        let block_pages = 1usize << target_order;
        self.stats.allocated_pages += block_pages;
        self.stats.free_pages = self.stats.free_pages.saturating_sub(block_pages);
        Some(node.start)
    }

    fn allocate_exact_from_node(
        &mut self,
        node_addr: usize,
        target_order: usize,
        target_addr: usize,
    ) -> Option<usize> {
        let node = node_ref(node_addr);
        let current_order = node.order as usize;
        let seg_idx = node.seg_idx as usize;
        let seg = *self.segment(seg_idx)?;
        let mut current_start = node.start;
        let needed = current_order.saturating_sub(target_order);

        let mut split_nodes = [0usize; MAX_TRACKED_ORDER + 1];
        if !self.preallocate_nodes(needed, &mut split_nodes) {
            return None;
        }

        self.remove_from_free_list(node_addr);
        self.hash_remove(node_addr);

        (0..needed).for_each(|depth| {
            let split_order = current_order - depth - 1;
            let half_size = (1usize << split_order) * PAGE_SIZE;
            let left_start = current_start;
            let right_start = left_start + half_size;

            if target_addr < right_start {
                let buddy_addr = split_nodes[depth];
                initialize_node(
                    buddy_addr,
                    right_start,
                    split_order,
                    seg_idx,
                    seg.fl_type,
                    true,
                );
                self.hash_insert(buddy_addr);
                self.add_to_free_list(buddy_addr);
            } else {
                let buddy_addr = split_nodes[depth];
                initialize_node(
                    buddy_addr,
                    left_start,
                    split_order,
                    seg_idx,
                    seg.fl_type,
                    true,
                );
                self.hash_insert(buddy_addr);
                self.add_to_free_list(buddy_addr);
                current_start = right_start;
            }
            self.stats.split_count += 1;
        });

        let node = node_mut(node_addr);
        node.start = current_start;
        node.order = target_order as u8;
        node.is_free = false;
        node.ref_count = 1;
        node.slab_zone_id = 0;
        node.gc_mark = 0;
        node.free_next = 0;
        node.free_prev = 0;
        node.hash_next = 0;
        self.hash_insert(node_addr);

        let block_pages = 1usize << target_order;
        self.stats.allocated_pages += block_pages;
        self.stats.free_pages = self.stats.free_pages.saturating_sub(block_pages);
        Some(node.start)
    }

    fn preallocate_nodes(
        &mut self,
        count: usize,
        out: &mut [usize; MAX_TRACKED_ORDER + 1],
    ) -> bool {
        if count == 0 {
            return true;
        }

        for idx in 0..count {
            let Some(node_addr) = self.alloc_node_raw() else {
                (0..idx).for_each(|rollback| {
                    self.recycle_node(out[rollback]);
                    out[rollback] = 0;
                });
                return false;
            };
            out[idx] = node_addr;
        }
        true
    }

    fn new_node(
        &mut self,
        start: usize,
        order: usize,
        seg_idx: usize,
        fl_type: usize,
        is_free: bool,
    ) -> Option<usize> {
        let node_addr = self.alloc_node_raw()?;
        initialize_node(node_addr, start, order, seg_idx, fl_type, is_free);
        Some(node_addr)
    }

    fn alloc_node_raw(&mut self) -> Option<usize> {
        if self.node_freelist != 0 {
            let node_addr = self.node_freelist;
            self.node_freelist = node_ref(node_addr).hash_next;
            let node = node_mut(node_addr);
            *node = BlockNode::empty();
            return Some(node_addr);
        }

        if self.nodes.is_null() || self.node_used >= self.node_capacity {
            None
        } else {
            let ptr = unsafe { self.nodes.add(self.node_used) } as usize;
            self.node_used += 1;
            unsafe {
                (ptr as *mut BlockNode).write(BlockNode::empty());
            }
            Some(ptr)
        }
    }

    fn recycle_node(&mut self, node_addr: usize) {
        let node = node_mut(node_addr);
        *node = BlockNode::empty();
        node.hash_next = self.node_freelist;
        self.node_freelist = node_addr;
    }

    fn add_to_free_list(&mut self, node_addr: usize) {
        let node = node_ref(node_addr);
        let fl_type = node.fl_type as usize;
        let order = node.order as usize;
        let head = self.free_head(fl_type, order);

        let node = node_mut(node_addr);
        node.is_free = true;
        node.free_prev = 0;
        node.free_next = head;

        if head != 0 {
            node_mut(head).free_prev = node_addr;
        }

        self.set_free_head(fl_type, order, node_addr);
        self.stats.free_count_per_order[order] += 1;
    }

    fn remove_from_free_list(&mut self, node_addr: usize) {
        let node = node_ref(node_addr);
        let fl_type = node.fl_type as usize;
        let order = node.order as usize;
        let prev = node.free_prev;
        let next = node.free_next;

        if prev != 0 {
            node_mut(prev).free_next = next;
        } else {
            self.set_free_head(fl_type, order, next);
        }
        if next != 0 {
            node_mut(next).free_prev = prev;
        }

        let node = node_mut(node_addr);
        node.free_prev = 0;
        node.free_next = 0;
        self.stats.free_count_per_order[order] =
            self.stats.free_count_per_order[order].saturating_sub(1);
    }

    fn hash_find(&self, start: usize) -> usize {
        if self.hash_bucket_count == 0 {
            return 0;
        }
        let bucket = hash_bucket(start, self.hash_bucket_count);
        let mut node_addr = self.bucket_head(bucket);
        while node_addr != 0 {
            let node = node_ref(node_addr);
            if node.start == start {
                return node_addr;
            }
            node_addr = node.hash_next;
        }
        0
    }

    fn hash_insert(&mut self, node_addr: usize) {
        let bucket = hash_bucket(node_ref(node_addr).start, self.hash_bucket_count);
        let head = self.bucket_head(bucket);
        let node = node_mut(node_addr);
        node.hash_next = head;
        self.set_bucket_head(bucket, node_addr);
    }

    fn hash_remove(&mut self, node_addr: usize) {
        if self.hash_bucket_count == 0 {
            return;
        }
        let start = node_ref(node_addr).start;
        let bucket = hash_bucket(start, self.hash_bucket_count);
        let mut current = self.bucket_head(bucket);
        let mut prev = 0usize;

        while current != 0 {
            if current == node_addr {
                let next = node_ref(current).hash_next;
                if prev == 0 {
                    self.set_bucket_head(bucket, next);
                } else {
                    node_mut(prev).hash_next = next;
                }
                node_mut(current).hash_next = 0;
                return;
            }
            prev = current;
            current = node_ref(current).hash_next;
        }
    }

    fn bucket_head(&self, bucket: usize) -> usize {
        if self.hash_buckets.is_null() {
            0
        } else {
            unsafe { *self.hash_buckets.add(bucket) }
        }
    }

    fn set_bucket_head(&mut self, bucket: usize, head: usize) {
        unsafe {
            *self.hash_buckets.add(bucket) = head;
        }
    }

    fn free_head(&self, fl_type: usize, order: usize) -> usize {
        self.free_heads[free_head_index(fl_type, order)]
    }

    fn set_free_head(&mut self, fl_type: usize, order: usize, head: usize) {
        self.free_heads[free_head_index(fl_type, order)] = head;
    }

    fn segment(&self, index: usize) -> Option<&BuddySegment> {
        if index >= self.segment_count {
            return None;
        }
        Some(unsafe { &*self.segments.add(index) })
    }

    fn page_location(&self, addr: usize) -> Option<(usize, usize)> {
        for seg_idx in 0..self.segment_count {
            let seg = self.segment(seg_idx)?;
            if addr < seg.range.start || addr >= seg.range.end() {
                continue;
            }
            let page_idx = (addr - seg.range.start) / PAGE_SIZE;
            if page_idx < seg.total_pages {
                return Some((seg_idx, page_idx));
            }
        }
        None
    }
}

impl Default for BuddyAllocator {
    fn default() -> Self {
        Self::new()
    }
}

pub struct BuddySegmentIter<'a> {
    allocator: &'a BuddyAllocator,
    index: usize,
}

impl<'a> Iterator for BuddySegmentIter<'a> {
    type Item = MemorySegment;

    fn next(&mut self) -> Option<Self::Item> {
        let segment = self.allocator.segment(self.index)?;
        let mut range = segment.range;
        self.index += 1;
        while let Some(next) = self.allocator.segment(self.index) {
            if range.end() != next.range.start {
                break;
            }
            range.size = next.range.end().saturating_sub(range.start);
            self.index += 1;
        }
        Some(range)
    }
}

pub struct BuddyReservedRangeIter<'a> {
    allocator: &'a BuddyAllocator,
    index: usize,
}

impl<'a> Iterator for BuddyReservedRangeIter<'a> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.allocator.reserved_range_count {
            return None;
        }
        let range = unsafe { *self.allocator.reserved_ranges.add(self.index) };
        self.index += 1;
        Some((range.start, range.end()))
    }
}

pub struct BuddyMetadataRangeIter<'a> {
    allocator: &'a BuddyAllocator,
    index: usize,
}

impl<'a> Iterator for BuddyMetadataRangeIter<'a> {
    type Item = MemorySegment;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.allocator.metadata_range_count {
            return None;
        }
        let range = unsafe { *self.allocator.metadata_ranges.add(self.index) };
        self.index += 1;
        Some(range)
    }
}

pub struct BuddyPageIter<'a> {
    allocator: &'a BuddyAllocator,
    done: bool,
}

impl<'a> Iterator for BuddyPageIter<'a> {
    type Item = (usize, usize, &'a PageInfo);

    fn next(&mut self) -> Option<Self::Item> {
        let _ = self.allocator;
        let _ = self.done;
        None
    }
}

pub struct BuddyFreeBlockIter<'a> {
    allocator: &'a BuddyAllocator,
    fl_type: usize,
    order: usize,
    current: usize,
}

impl<'a> Iterator for BuddyFreeBlockIter<'a> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current != 0 {
                let node_addr = self.current;
                let node = node_ref(node_addr);
                self.current = node.free_next;
                return Some((node.start, node.order as usize));
            }

            if self.fl_type >= VM_NFREELIST {
                return None;
            }

            while self.order <= MAX_TRACKED_ORDER {
                let head = self.allocator.free_head(self.fl_type, self.order);
                self.order += 1;
                if head != 0 {
                    let node = node_ref(head);
                    self.current = node.free_next;
                    return Some((node.start, node.order as usize));
                }
            }

            self.fl_type += 1;
            self.order = 0;
        }
    }
}

#[inline]
fn node_ref(addr: usize) -> &'static BlockNode {
    unsafe { &*(addr as *const BlockNode) }
}

#[inline]
fn node_mut(addr: usize) -> &'static mut BlockNode {
    unsafe { &mut *(addr as *mut BlockNode) }
}

#[inline]
fn initialize_node(
    node_addr: usize,
    start: usize,
    order: usize,
    seg_idx: usize,
    fl_type: usize,
    is_free: bool,
) {
    let node = node_mut(node_addr);
    *node = BlockNode {
        start,
        order: order as u8,
        seg_idx: seg_idx as u32,
        fl_type: fl_type as u8,
        is_free,
        ref_count: if is_free { 0 } else { 1 },
        slab_zone_id: 0,
        gc_mark: 0,
        free_next: 0,
        free_prev: 0,
        hash_next: 0,
    };
}

fn count_effective_segments(segments: &[MemorySegment]) -> usize {
    let mut count = 0usize;
    for &raw_segment in segments {
        for_each_effective_segment(raw_segment, |_segment, _fl_type| {
            count += 1;
        });
    }
    count
}

fn estimate_total_pages(segments: &[MemorySegment]) -> Option<usize> {
    let mut total = 0usize;
    for &raw_segment in segments {
        for_each_effective_segment(raw_segment, |segment, _fl_type| {
            total = total
                .checked_add(segment.size / PAGE_SIZE)
                .unwrap_or(usize::MAX);
        });
    }
    (total != usize::MAX).then_some(total)
}

fn for_each_effective_segment(raw_segment: MemorySegment, mut f: impl FnMut(MemorySegment, usize)) {
    let segment = normalize_segment(raw_segment);
    if segment.size < PAGE_SIZE {
        return;
    }

    let segment_end = segment.end();
    if segment.start < DMA_ZONE_LIMIT && segment_end > DMA_ZONE_LIMIT {
        let low = MemorySegment {
            start: segment.start,
            size: DMA_ZONE_LIMIT - segment.start,
        };
        let high = MemorySegment {
            start: DMA_ZONE_LIMIT,
            size: segment_end - DMA_ZONE_LIMIT,
        };
        if low.size >= PAGE_SIZE {
            f(low, VM_FREELIST_DMA);
        }
        if high.size >= PAGE_SIZE {
            f(high, VM_FREELIST_DEFAULT);
        }
    } else {
        let fl_type = if segment_end <= DMA_ZONE_LIMIT {
            VM_FREELIST_DMA
        } else {
            VM_FREELIST_DEFAULT
        };
        f(segment, fl_type);
    }
}

fn normalize_segment(segment: MemorySegment) -> MemorySegment {
    let Some(raw_end) = segment.start.checked_add(segment.size) else {
        return MemorySegment { start: 0, size: 0 };
    };
    let Some(start) = align_up_checked(segment.start, PAGE_SIZE) else {
        return MemorySegment { start: 0, size: 0 };
    };
    if raw_end <= start {
        return MemorySegment { start: 0, size: 0 };
    }
    let size = ((raw_end - start) / PAGE_SIZE) * PAGE_SIZE;
    MemorySegment { start, size }
}

fn buddy_metadata_bytes(
    segment_count: usize,
    reserved_count: usize,
    bucket_count: usize,
    node_capacity: usize,
) -> Option<usize> {
    let mut cursor = 0usize;
    bump_metadata_size::<BuddySegment>(&mut cursor, segment_count)?;
    bump_metadata_size::<MemorySegment>(&mut cursor, reserved_count)?;
    bump_metadata_size::<MemorySegment>(&mut cursor, METADATA_RANGE_COUNT)?;
    bump_metadata_size::<usize>(&mut cursor, bucket_count)?;
    bump_metadata_size::<BlockNode>(&mut cursor, node_capacity)?;
    align_up_checked(cursor, PAGE_SIZE)
}

fn bump_metadata_size<T>(cursor: &mut usize, count: usize) -> Option<()> {
    if count == 0 {
        return Some(());
    }
    let layout = Layout::array::<T>(count).ok()?;
    let aligned = align_up_checked(*cursor, layout.align())?;
    *cursor = aligned.checked_add(layout.size())?;
    Some(())
}

fn carve_metadata_range(
    segments: &[MemorySegment],
    reserved_regions: &[(usize, usize)],
    size: usize,
) -> Option<MemorySegment> {
    let size = align_up_checked(size.max(PAGE_SIZE), PAGE_SIZE)?;
    let mut best = None;
    let mut best_fl_type = VM_FREELIST_DMA;

    for &raw_segment in segments {
        for_each_effective_segment(raw_segment, |segment, fl_type| {
            let mut current = segment.start;
            while current < segment.end() {
                let reserved = next_reserved_phys_run(segment, current, reserved_regions);
                let gap_end = reserved
                    .map(|(start, _end)| start)
                    .unwrap_or_else(|| segment.end());
                consider_metadata_gap(
                    current,
                    gap_end,
                    size,
                    fl_type,
                    &mut best,
                    &mut best_fl_type,
                );

                match reserved {
                    Some((_start, end)) => current = current.max(end),
                    None => break,
                }
            }
        });
    }

    best
}

fn next_reserved_phys_run(
    segment: MemorySegment,
    current: usize,
    reserved_regions: &[(usize, usize)],
) -> Option<(usize, usize)> {
    let mut next = None;
    for &(start, end) in reserved_regions {
        let Some(interval) = clipped_phys_interval(segment, start, end) else {
            continue;
        };
        if interval.1 <= current {
            continue;
        }
        next = choose_earlier_phys_interval(next, interval);
    }

    let (start, mut end) = next?;
    loop {
        let mut extended = false;
        for &(candidate_start, candidate_end) in reserved_regions {
            let Some((candidate_start, candidate_end)) =
                clipped_phys_interval(segment, candidate_start, candidate_end)
            else {
                continue;
            };
            if candidate_start <= end && candidate_end > end {
                end = candidate_end;
                extended = true;
            }
        }
        if !extended {
            break;
        }
    }
    Some((start, end))
}

fn clipped_phys_interval(
    segment: MemorySegment,
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
    if end <= start {
        return None;
    }
    let overlap_start = start.max(segment.start);
    let overlap_end = end.min(segment.end());
    (overlap_end > overlap_start).then_some((overlap_start, overlap_end))
}

fn consider_metadata_gap(
    gap_start: usize,
    gap_end: usize,
    size: usize,
    fl_type: usize,
    best: &mut Option<MemorySegment>,
    best_fl_type: &mut usize,
) {
    if gap_end <= gap_start || gap_end - gap_start < size {
        return;
    }
    let Some(aligned_end) = align_down_checked(gap_end, PAGE_SIZE) else {
        return;
    };
    let Some(min_end) = gap_start.checked_add(size) else {
        return;
    };
    if aligned_end < min_end {
        return;
    }
    let start = aligned_end - size;
    let candidate = MemorySegment { start, size };
    if metadata_candidate_is_better(candidate, fl_type, *best, *best_fl_type) {
        *best = Some(candidate);
        *best_fl_type = fl_type;
    }
}

fn metadata_candidate_is_better(
    candidate: MemorySegment,
    fl_type: usize,
    best: Option<MemorySegment>,
    best_fl_type: usize,
) -> bool {
    let Some(best) = best else {
        return true;
    };
    if fl_type == VM_FREELIST_DEFAULT && best_fl_type != VM_FREELIST_DEFAULT {
        return true;
    }
    if fl_type != VM_FREELIST_DEFAULT && best_fl_type == VM_FREELIST_DEFAULT {
        return false;
    }
    candidate.start > best.start
}

#[inline]
fn choose_earlier_phys_interval(
    current: Option<(usize, usize)>,
    candidate: (usize, usize),
) -> Option<(usize, usize)> {
    match current {
        Some(existing) if existing.0 < candidate.0 => Some(existing),
        Some(existing) if existing.0 == candidate.0 && existing.1 >= candidate.1 => Some(existing),
        _ => Some(candidate),
    }
}

fn clipped_page_interval(seg: &BuddySegment, start: usize, end: usize) -> Option<(usize, usize)> {
    if end <= start {
        return None;
    }

    let overlap_start = start.max(seg.range.start);
    let overlap_end = end.min(seg.range.end());
    if overlap_end <= overlap_start {
        return None;
    }

    let start_page = (overlap_start - seg.range.start) / PAGE_SIZE;
    let end_page = (overlap_end - seg.range.start).div_ceil(PAGE_SIZE);
    (start_page < end_page).then_some((start_page, end_page.min(seg.total_pages)))
}

#[inline]
fn choose_earlier_page_interval(
    current: Option<(usize, usize)>,
    candidate: (usize, usize),
) -> Option<(usize, usize)> {
    match current {
        Some(existing) if existing.0 < candidate.0 => Some(existing),
        Some(existing) if existing.0 == candidate.0 && existing.1 >= candidate.1 => Some(existing),
        _ => Some(candidate),
    }
}

#[inline]
fn free_head_index(fl_type: usize, order: usize) -> usize {
    fl_type * (MAX_TRACKED_ORDER + 1) + order
}

#[inline]
fn hash_bucket(start: usize, bucket_count: usize) -> usize {
    let mut x = start >> PAGE_SHIFT;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccdusize);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53usize);
    x ^= x >> 33;
    x & (bucket_count - 1)
}

fn choose_hash_bucket_count(total_pages: usize) -> usize {
    let mut buckets = total_pages
        .div_ceil(TARGET_HASH_CHAIN)
        .clamp(MIN_HASH_BUCKETS, MAX_HASH_BUCKETS);
    if !buckets.is_power_of_two() {
        buckets = buckets.next_power_of_two();
    }
    buckets
}

#[inline]
fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

fn max_order_for_pages(total_pages: usize) -> usize {
    let mut order = 0usize;
    let mut block = 1usize;
    while block <= total_pages / 2 && order < MAX_TRACKED_ORDER {
        block <<= 1;
        order += 1;
    }
    order
}

fn best_seed_order(max_order: usize, page_idx: usize, available: usize) -> usize {
    let mut order = pages_to_order_floor(available).min(max_order);
    while order > 0 && (page_idx & ((1usize << order) - 1)) != 0 {
        order -= 1;
    }
    order
}

#[inline]
fn pages_to_order_floor(pages: usize) -> usize {
    let mut order = 0usize;
    let mut block = 1usize;
    while (block << 1) <= pages {
        block <<= 1;
        order += 1;
    }
    order
}

fn required_order_for_request(request: &PhysicalAllocRequest) -> Result<usize, BuddyAllocError> {
    request
        .required_order()
        .map_err(buddy_alloc_error_from_request)
}

fn buddy_alloc_error_from_request(err: AllocationRequestError) -> BuddyAllocError {
    match err {
        AllocationRequestError::InvalidSize
        | AllocationRequestError::InvalidAlignment
        | AllocationRequestError::SizeOverflow
        | AllocationRequestError::UnsupportedOrder => BuddyAllocError::InvalidOrder,
        AllocationRequestError::InvalidPlacement => BuddyAllocError::InvalidAddress,
    }
}

#[inline]
fn align_up_checked(value: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    Some(value.checked_add(align - 1)? & !(align - 1))
}

#[inline]
fn align_down_checked(value: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    Some(value & !(align - 1))
}
