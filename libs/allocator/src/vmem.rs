//! vmem arena 虚拟地址空间区间管理器
//!
//! 通过边界标签 (boundary tag) 管理线性资源区间。vmem 用于管理虚拟地址空间、
//! 设备 I/O 空间等线性资源。
//!
//! 核心特性:
//! - 基于边界标签的区间管理
//! - 支持 quantum 对齐分配
//! - 分离的空闲链表 (power-of-2 大小桶) 加速最佳匹配查找
//! - 即时合并 (instant coalescing) 机制
//! - 使用自举感知的动态标签池消除固定上限

use core::alloc::Layout;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::error::VmemError;

const INVALID_TAG: usize = usize::MAX;
/// 空闲链表的桶数 (按大小的 log2 分桶)
const VMEM_FREELISTS: usize = 32;

/// 默认 quantum (最小分配单位)
pub const VMEM_DEFAULT_QUANTUM: usize = 4096;

/// 边界标签类型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum BtType {
    /// 空闲段
    Free = 0,
    /// 已分配段
    Allocated = 1,
    /// span 标记（从父 arena 或初始导入的区间起始）
    Span = 2,
    /// span 标记（区间内部，不参与分配）
    #[allow(dead_code)]
    SpanInternal = 3,
}

/// 边界标签 (Boundary Tag)
///
/// 每个标签描述一段连续的地址区间及其状态。
/// 所有标签按地址顺序组成一个双向链表（段列表），
/// 空闲标签额外组织在按大小分桶的空闲链表中。
///
/// vmem 的核心思想就是“地址空间也是一种可切分、可合并的线性资源”，而 boundary tag
/// 正是这种资源模型的最小状态单元。
#[derive(Clone, Copy)]
pub struct BoundaryTag {
    /// 段起始地址
    pub base: usize,
    /// 段大小
    pub size: usize,
    /// 段类型
    pub bt_type: BtType,
    /// 段列表：下一个标签索引
    pub seg_next: usize,
    /// 段列表：上一个标签索引
    pub seg_prev: usize,
    /// 空闲链表：下一个同桶空闲标签索引
    pub free_next: usize,
    /// 空闲链表：上一个同桶空闲标签索引
    pub free_prev: usize,
    /// 是否在使用中
    pub in_use: bool,
}

impl BoundaryTag {
    pub const fn empty() -> Self {
        Self {
            base: 0,
            size: 0,
            bt_type: BtType::Free,
            seg_next: INVALID_TAG,
            seg_prev: INVALID_TAG,
            free_next: INVALID_TAG,
            free_prev: INVALID_TAG,
            in_use: false,
        }
    }
}

/// vmem 分配策略
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmemAllocPolicy {
    /// 最佳匹配 (Best Fit)
    BestFit,
    /// 即时匹配 (Instant Fit) - 优先从精确桶分配
    InstantFit,
    /// 首次匹配 (First Fit)
    FirstFit,
    /// 下次匹配 (Next Fit) - 从上次分配位置继续搜索
    NextFit,
}

/// vmem arena 统计信息
#[derive(Clone, Copy, Debug, Default)]
pub struct VmemStats {
    /// 总管理大小
    pub total_size: usize,
    /// 已分配大小
    pub allocated_size: usize,
    /// 空闲大小
    pub free_size: usize,
    /// 导入(span)计数
    pub span_count: usize,
    /// 分配请求总数
    pub alloc_count: u64,
    /// 释放请求总数
    pub free_count: u64,
    /// 分配失败数
    pub alloc_failures: u64,
    /// 活跃边界标签数
    pub active_tags: usize,
    /// 最大连续空闲区间
    pub largest_free_size: usize,
    /// 空闲段数量
    pub free_segments: usize,
    /// 分裂次数
    pub split_count: u64,
    /// 合并次数
    pub coalesce_count: u64,
    /// 释放大小不匹配次数
    pub size_mismatch_failures: u64,
    /// 无效释放次数
    pub invalid_free_failures: u64,
    /// span 重叠拒绝次数
    pub overlap_failures: u64,
    /// 元数据分配失败次数
    pub metadata_failures: u64,
}

impl VmemStats {
    pub const fn new() -> Self {
        Self {
            total_size: 0,
            allocated_size: 0,
            free_size: 0,
            span_count: 0,
            alloc_count: 0,
            free_count: 0,
            alloc_failures: 0,
            active_tags: 0,
            largest_free_size: 0,
            free_segments: 0,
            split_count: 0,
            coalesce_count: 0,
            size_mismatch_failures: 0,
            invalid_free_failures: 0,
            overlap_failures: 0,
            metadata_failures: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VmemValidationStats {
    pub segment_count: usize,
    pub free_segments: usize,
    pub allocated_size: usize,
    pub free_size: usize,
    pub largest_free_size: usize,
}

/// vmem arena
///
/// 管理线性地址区间资源的分配器:
/// - 边界标签池管理所有区间段
/// - 按大小分桶的空闲链表加速查找
/// - 支持 quantum 对齐
/// - 即时合并相邻空闲段
///
/// 和 buddy 管“物理页块”不同，vmem 管的是“地址区间语义”。它不关心页里装的是什么，
/// 只关心哪段地址已经被保留、哪段地址空闲、以及能否把相邻空闲段重新拼回去。
pub struct VmemArena {
    /// arena 名称 (调试用)
    name: [u8; 32],
    /// quantum: 最小分配/对齐单位
    quantum: usize,
    /// quantum 的 log2 值
    quantum_shift: usize,
    /// 边界标签空闲池头
    tag_free_head: usize,
    /// 段列表头（按地址排序的所有标签）
    seg_list_head: usize,
    /// 按大小分桶的空闲链表头
    free_lists: [usize; VMEM_FREELISTS],
    /// 分配策略
    policy: VmemAllocPolicy,
    /// 统计信息
    stats: VmemStats,
    /// 是否已初始化
    initialized: bool,
}

impl VmemArena {
    /// 创建未初始化的 vmem arena
    pub const fn new() -> Self {
        Self {
            name: [0u8; 32],
            quantum: VMEM_DEFAULT_QUANTUM,
            quantum_shift: 12,
            tag_free_head: INVALID_TAG,
            seg_list_head: INVALID_TAG,
            free_lists: [INVALID_TAG; VMEM_FREELISTS],
            policy: VmemAllocPolicy::BestFit,
            stats: VmemStats::new(),
            initialized: false,
        }
    }

    /// 初始化 vmem arena
    ///
    /// # 参数
    /// - `name`: arena 名称
    /// - `base`: 初始区间起始地址
    /// - `size`: 初始区间大小
    /// - `quantum`: 最小分配单位 (必须是 2 的幂)
    /// - `policy`: 分配策略
    pub fn init(
        &mut self,
        name: &[u8],
        base: usize,
        size: usize,
        quantum: usize,
        policy: VmemAllocPolicy,
    ) -> bool {
        // init 只建立 arena 自身的元数据和初始 span，不触碰任何物理页；因此它可以在
        // 比正式映射更早的阶段运行。
        // 验证 quantum 是 2 的幂
        if quantum == 0 || (quantum & (quantum - 1)) != 0 {
            return false;
        }

        self.name = [0u8; 32];
        // 设置名称
        let len = name.len().min(self.name.len() - 1);
        self.name[..len].copy_from_slice(&name[..len]);

        self.quantum = quantum;
        self.quantum_shift = quantum.trailing_zeros() as usize;
        self.policy = policy;
        self.tag_free_head = INVALID_TAG;
        self.seg_list_head = INVALID_TAG;
        self.stats = VmemStats::new();

        // 重置空闲链表
        for i in 0..VMEM_FREELISTS {
            self.free_lists[i] = INVALID_TAG;
        }

        self.initialized = true;

        // 添加初始 span
        if size > 0 {
            return self.add_span(base, size);
        }
        true
    }

    /// 从标签空闲池获取一个标签
    fn alloc_tag(&mut self) -> Option<usize> {
        // tag 本身也是元数据资源，所以这里优先复用 free tag；没有可复用的 tag 时，
        // 才去向 crate 级 metadata allocator 要新的标签存储。
        if self.tag_free_head != INVALID_TAG {
            let addr = self.tag_free_head;
            self.tag_free_head = read_tag(addr).free_next;
            let mut tag = BoundaryTag::empty();
            tag.in_use = true;
            write_tag(addr, tag);
            self.stats.active_tags += 1;
            return Some(addr);
        }

        let addr = crate::alloc_internal_metadata(Layout::new::<BoundaryTag>()) as usize;
        if addr == 0 {
            return None;
        }
        let mut tag = BoundaryTag::empty();
        tag.in_use = true;
        write_tag(addr, tag);
        self.stats.active_tags += 1;
        Some(addr)
    }

    /// 归还标签到空闲池
    fn free_tag(&mut self, addr: usize) {
        // 标签本身是 arena 的元数据对象。释放标签不是把内存还给底层分配器，而是把它放回
        // arena 私有的 tag freelist，供下一次区间切分或合并复用，避免频繁重新申请元数据。
        let mut tag = BoundaryTag::empty();
        tag.free_next = self.tag_free_head;
        write_tag(addr, tag);
        self.tag_free_head = addr;
        self.stats.active_tags = self.stats.active_tags.saturating_sub(1);
    }

    /// 计算大小所属的空闲链表桶索引
    fn size_to_bucket(&self, size: usize) -> usize {
        // vmem 用“按 quantum 归一化后的对数量级”做分桶，而不是精确大小索引。这样做的
        // 目的是用较少桶数覆盖很宽的区间范围，在查找复杂度和桶内离散度之间取平衡。
        if size == 0 {
            return 0;
        }
        let quantums = size.div_ceil(self.quantum);
        let bits = usize::BITS - quantums.leading_zeros();
        (bits as usize).min(VMEM_FREELISTS - 1)
    }

    /// 将空闲标签加入对应大小桶的空闲链表
    fn add_to_freelist(&mut self, tag_idx: usize) {
        // 只有 `BtType::Free` 的标签才参与按大小索引的空闲链表。段列表负责维持地址顺序，
        // 而 free list 负责加速分配搜索；同一个标签同时存在于两套结构中，但服务的语义不同。
        let mut tag = read_tag(tag_idx);
        let size = tag.size;
        let bucket = self.size_to_bucket(size);

        tag.free_prev = INVALID_TAG;
        tag.free_next = self.free_lists[bucket];
        tag.bt_type = BtType::Free;
        write_tag(tag_idx, tag);

        if self.free_lists[bucket] != INVALID_TAG {
            let mut head = read_tag(self.free_lists[bucket]);
            head.free_prev = tag_idx;
            write_tag(self.free_lists[bucket], head);
        }
        self.free_lists[bucket] = tag_idx;
    }

    /// 从空闲链表中移除标签
    fn remove_from_freelist(&mut self, tag_idx: usize) {
        // 这里不动段列表，只摘掉“按大小检索”这一层链接。无论后续是切分、保留还是释放，
        // 标签仍然按地址顺序留在 seg list 中，直到显式合并或删除。
        let tag = read_tag(tag_idx);
        let size = tag.size;
        let bucket = self.size_to_bucket(size);

        let prev = tag.free_prev;
        let next = tag.free_next;

        if prev != INVALID_TAG {
            let mut prev_tag = read_tag(prev);
            prev_tag.free_next = next;
            write_tag(prev, prev_tag);
        } else {
            self.free_lists[bucket] = next;
        }
        if next != INVALID_TAG {
            let mut next_tag = read_tag(next);
            next_tag.free_prev = prev;
            write_tag(next, next_tag);
        }

        let mut cleared = tag;
        cleared.free_prev = INVALID_TAG;
        cleared.free_next = INVALID_TAG;
        write_tag(tag_idx, cleared);
    }

    /// 在段列表中 `after` 标签之后插入新标签
    fn insert_seg_after(&mut self, after: usize, new_idx: usize) {
        // 段列表是 arena 的“地址真相”。任何前后切分产生的新标签，都必须先正确接入这条
        // 地址有序双向链表，后续相邻合并、按地址查找和调试输出才有可靠依据。
        let mut after_tag = read_tag(after);
        let next = after_tag.seg_next;
        after_tag.seg_next = new_idx;
        write_tag(after, after_tag);

        let mut new_tag = read_tag(new_idx);
        new_tag.seg_prev = after;
        new_tag.seg_next = next;
        write_tag(new_idx, new_tag);

        if next != INVALID_TAG {
            let mut next_tag = read_tag(next);
            next_tag.seg_prev = new_idx;
            write_tag(next, next_tag);
        }
    }

    fn insert_seg_before(&mut self, before: usize, new_idx: usize) {
        // 与 `insert_seg_after` 对称，这里主要服务前部对齐切分和显式 reserve 前缀切分。
        // 若插入到表头，还要同步更新 `seg_list_head`，否则整个 arena 的地址遍历入口会丢失。
        let before_tag = read_tag(before);
        let prev = before_tag.seg_prev;

        let mut new_tag = read_tag(new_idx);
        new_tag.seg_prev = prev;
        new_tag.seg_next = before;
        write_tag(new_idx, new_tag);

        let mut updated_before = before_tag;
        updated_before.seg_prev = new_idx;
        write_tag(before, updated_before);

        if prev != INVALID_TAG {
            let mut prev_tag = read_tag(prev);
            prev_tag.seg_next = new_idx;
            write_tag(prev, prev_tag);
        } else {
            self.seg_list_head = new_idx;
        }
    }

    /// 从段列表中移除标签
    fn remove_seg(&mut self, idx: usize) {
        // 只有在“标签已失去独立地址语义”时才会从段列表删除，例如与前后空闲段合并之后，
        // 被吃掉的那个标签就不应该再出现在地址视图中。
        let tag = read_tag(idx);
        let prev = tag.seg_prev;
        let next = tag.seg_next;

        if prev != INVALID_TAG {
            let mut prev_tag = read_tag(prev);
            prev_tag.seg_next = next;
            write_tag(prev, prev_tag);
        } else {
            self.seg_list_head = next;
        }
        if next != INVALID_TAG {
            let mut next_tag = read_tag(next);
            next_tag.seg_prev = prev;
            write_tag(next, next_tag);
        }
    }

    /// 向 arena 添加一个新的 span (地址区间)
    ///
    /// 创建一个 Span 标签标记区间来源，然后创建对应的 Free 标签供分配使用
    pub fn add_span(&mut self, base: usize, size: usize) -> bool {
        self.add_span_result(base, size).is_ok()
    }

    pub fn add_span_result(&mut self, base: usize, size: usize) -> Result<(), VmemError> {
        // “span 标签 + free 标签”分开存在，是为了同时表达两层信息：
        // 这段地址来自哪里，以及这段地址当前是否空闲可分配。
        if !self.initialized || size == 0 {
            return if !self.initialized {
                Err(VmemError::NotInitialized)
            } else {
                Err(VmemError::InvalidSize)
            };
        }
        if range_overlaps_existing_span(self.seg_list_head, base, size) {
            self.stats.overlap_failures += 1;
            return Err(VmemError::Overlap);
        }

        // 创建 Span 标记标签
        let span_tag = match self.alloc_tag() {
            Some(idx) => idx,
            None => {
                self.stats.metadata_failures += 1;
                return Err(VmemError::MetadataOutOfMemory);
            }
        };
        let mut span = BoundaryTag::empty();
        span.base = base;
        span.size = size;
        span.bt_type = BtType::Span;
        span.in_use = true;
        write_tag(span_tag, span);

        // 插入段列表（按地址排序）
        if self.seg_list_head == INVALID_TAG {
            self.seg_list_head = span_tag;
        } else {
            // 找到合适的插入位置
            let mut pos = self.seg_list_head;
            let mut prev_pos = INVALID_TAG;
            while pos != INVALID_TAG && read_tag(pos).base < base {
                prev_pos = pos;
                pos = read_tag(pos).seg_next;
            }
            if prev_pos == INVALID_TAG {
                // 插入头部
                let mut span = read_tag(span_tag);
                span.seg_next = self.seg_list_head;
                write_tag(span_tag, span);
                let mut head = read_tag(self.seg_list_head);
                head.seg_prev = span_tag;
                write_tag(self.seg_list_head, head);
                self.seg_list_head = span_tag;
            } else {
                self.insert_seg_after(prev_pos, span_tag);
            }
        }

        // 创建空闲标签
        let free_tag = match self.alloc_tag() {
            Some(idx) => idx,
            None => {
                self.remove_seg(span_tag);
                self.free_tag(span_tag);
                self.stats.metadata_failures += 1;
                return Err(VmemError::MetadataOutOfMemory);
            }
        };
        let mut free = BoundaryTag::empty();
        free.base = base;
        free.size = size;
        free.bt_type = BtType::Free;
        free.in_use = true;
        write_tag(free_tag, free);

        // 在 span 标签后插入空闲标签
        self.insert_seg_after(span_tag, free_tag);

        // 加入空闲链表
        self.add_to_freelist(free_tag);

        self.stats.total_size += size;
        self.stats.free_size += size;
        self.stats.span_count += 1;
        Ok(())
    }

    /// 从 arena 分配指定大小的地址区间
    ///
    /// 按策略搜索合适的空闲段，必要时进行分裂
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<usize> {
        self.alloc_result(size, align).ok()
    }

    pub fn alloc_result(&mut self, size: usize, align: usize) -> Result<usize, VmemError> {
        // vmem 的分配对象不是页，而是“满足对齐要求的一段线性地址”。一旦找到合适 free
        // tag，后续就是围绕这个 tag 做前部对齐切分和尾部分裂。
        if !self.initialized || size == 0 {
            return if !self.initialized {
                Err(VmemError::NotInitialized)
            } else {
                Err(VmemError::InvalidSize)
            };
        }

        self.stats.alloc_count += 1;
        VMEM_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);

        // quantum 对齐
        let aligned_size = match align_up(size, self.quantum) {
            Some(size) => size,
            None => {
                self.stats.alloc_failures += 1;
                return Err(VmemError::InvalidSize);
            }
        };
        let align = align.max(self.quantum);
        if (align & (align - 1)) != 0 {
            self.stats.alloc_failures += 1;
            return Err(VmemError::InvalidAlignment);
        }

        let result = match self.policy {
            VmemAllocPolicy::BestFit => self.alloc_best_fit(aligned_size, align),
            VmemAllocPolicy::InstantFit => self.alloc_instant_fit(aligned_size, align),
            VmemAllocPolicy::FirstFit => self.alloc_first_fit(aligned_size, align),
            VmemAllocPolicy::NextFit => self.alloc_first_fit(aligned_size, align),
        };
        result.ok_or(VmemError::OutOfAddressSpace)
    }

    pub fn alloc_in_range_result(
        &mut self,
        range_start: usize,
        range_end: usize,
        size: usize,
        align: usize,
    ) -> Result<usize, VmemError> {
        if !self.initialized {
            return Err(VmemError::NotInitialized);
        }
        if size == 0 {
            return Err(VmemError::InvalidSize);
        }
        if range_start >= range_end {
            self.stats.alloc_failures += 1;
            return Err(VmemError::InvalidRange);
        }

        self.stats.alloc_count += 1;
        VMEM_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);

        let aligned_size = match align_up(size, self.quantum) {
            Some(size) => size,
            None => {
                self.stats.alloc_failures += 1;
                return Err(VmemError::InvalidSize);
            }
        };
        let align = align.max(self.quantum);
        if (align & (align - 1)) != 0 {
            self.stats.alloc_failures += 1;
            return Err(VmemError::InvalidAlignment);
        }

        let mut idx = self.seg_list_head;
        while idx != INVALID_TAG {
            let tag = read_tag(idx);
            if tag.bt_type != BtType::Free {
                idx = tag.seg_next;
                continue;
            }
            let Some(tag_end) = tag.base.checked_add(tag.size) else {
                self.stats.alloc_failures += 1;
                return Err(VmemError::InvalidRange);
            };
            let candidate_start = tag.base.max(range_start);
            let candidate_end = tag_end.min(range_end);
            if candidate_start >= candidate_end {
                idx = tag.seg_next;
                continue;
            }
            let Some(base) = align_up(candidate_start, align) else {
                idx = tag.seg_next;
                continue;
            };
            let Some(end) = base.checked_add(aligned_size) else {
                self.stats.alloc_failures += 1;
                return Err(VmemError::InvalidRange);
            };
            if end > candidate_end {
                idx = tag.seg_next;
                continue;
            }
            return if self.reserve_from_tag(idx, base, aligned_size) {
                Ok(base)
            } else {
                self.stats.alloc_failures += 1;
                Err(VmemError::MetadataOutOfMemory)
            };
        }

        self.stats.alloc_failures += 1;
        Err(VmemError::OutOfAddressSpace)
    }

    pub fn reserve_result(&mut self, base: usize, size: usize) -> Result<(), VmemError> {
        // reserve 与普通 alloc 的区别在于：区间位置已经由上层决定，vmem 这里只是把这段
        // 地址从 free 视图中精准扣除，并把相邻剩余空间重新组织成新的 free tag。
        if !self.initialized || size == 0 || (base & (self.quantum - 1)) != 0 {
            return if !self.initialized {
                Err(VmemError::NotInitialized)
            } else if size == 0 {
                Err(VmemError::InvalidSize)
            } else {
                Err(VmemError::InvalidAlignment)
            };
        }

        self.stats.alloc_count += 1;
        VMEM_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);

        let aligned_size = match align_up(size, self.quantum) {
            Some(size) => size,
            None => {
                self.stats.alloc_failures += 1;
                return Err(VmemError::InvalidSize);
            }
        };
        let end = match base.checked_add(aligned_size) {
            Some(end) => end,
            None => {
                self.stats.alloc_failures += 1;
                return Err(VmemError::InvalidRange);
            }
        };

        let mut idx = self.seg_list_head;
        while idx != INVALID_TAG {
            let tag = read_tag(idx);
            if tag.bt_type == BtType::Free {
                let tag_base = tag.base;
                let Some(tag_end) = tag_base.checked_add(tag.size) else {
                    self.stats.alloc_failures += 1;
                    return Err(VmemError::InvalidRange);
                };
                if base >= tag_base && end <= tag_end {
                    return if self.reserve_from_tag(idx, base, aligned_size) {
                        Ok(())
                    } else {
                        Err(VmemError::MetadataOutOfMemory)
                    };
                }
            }
            idx = tag.seg_next;
        }

        self.stats.alloc_failures += 1;
        Err(VmemError::OutOfAddressSpace)
    }

    /// 最佳匹配分配
    fn alloc_best_fit(&mut self, size: usize, align: usize) -> Option<usize> {
        // best-fit 倾向于选择"扣掉对齐浪费后剩余最少"的空闲块，以降低外部碎片。不过这也
        // 意味着需要扫描更多候选块，时间开销通常高于 first-fit。
        const BEST_FIT_MAX_SCAN: usize = 64;

        let start_bucket = self.size_to_bucket(size);
        let mut best_idx = usize::MAX;
        let mut best_waste = usize::MAX;
        let mut scanned = 0;

        for bucket in start_bucket..VMEM_FREELISTS {
            let mut idx = self.free_lists[bucket];
            while idx != INVALID_TAG {
                if scanned >= BEST_FIT_MAX_SCAN {
                    if best_idx != usize::MAX {
                        return self.allocate_from_tag(best_idx, size, align);
                    }
                    return self.alloc_first_fit(size, align);
                }
                scanned += 1;

                let tag = read_tag(idx);
                let tag_size = tag.size;
                let tag_base = tag.base;
                let Some(aligned_base) = align_up(tag_base, align) else {
                    idx = tag.free_next;
                    continue;
                };
                let waste = aligned_base - tag_base;
                let Some(needed) = size.checked_add(waste) else {
                    idx = tag.free_next;
                    continue;
                };

                if tag_size >= needed && (tag_size - needed) < best_waste {
                    best_waste = tag_size - needed;
                    best_idx = idx;
                    if best_waste == 0 {
                        break;
                    }
                }
                idx = tag.free_next;
            }
            if best_idx != usize::MAX && best_waste == 0 {
                break;
            }
        }

        if best_idx == usize::MAX {
            self.stats.alloc_failures += 1;
            return None;
        }

        self.allocate_from_tag(best_idx, size, align)
    }

    /// 即时匹配分配（优先从精确大小桶查找）
    fn alloc_instant_fit(&mut self, size: usize, align: usize) -> Option<usize> {
        // instant-fit 的思路是先赌“同量级桶里就能找到足够合适的块”，命中时可减少跨桶
        // 扫描成本；若精确桶找不到，再逐级向更大桶回退。
        let bucket = self.size_to_bucket(size);

        // 先搜索精确桶
        let mut idx = self.free_lists[bucket];
        while idx != INVALID_TAG {
            let tag = read_tag(idx);
            let tag_base = tag.base;
            let Some(aligned_base) = align_up(tag_base, align) else {
                idx = tag.free_next;
                continue;
            };
            let waste = aligned_base - tag_base;
            let Some(needed) = size.checked_add(waste) else {
                idx = tag.free_next;
                continue;
            };
            if tag.size >= needed {
                return self.allocate_from_tag(idx, size, align);
            }
            idx = tag.free_next;
        }

        // 向上搜索更大的桶
        for b in (bucket + 1)..VMEM_FREELISTS {
            let mut idx = self.free_lists[b];
            while idx != INVALID_TAG {
                let tag = read_tag(idx);
                let tag_base = tag.base;
                let Some(aligned_base) = align_up(tag_base, align) else {
                    idx = tag.free_next;
                    continue;
                };
                let waste = aligned_base - tag_base;
                let Some(needed) = size.checked_add(waste) else {
                    idx = tag.free_next;
                    continue;
                };
                if tag.size >= needed {
                    return self.allocate_from_tag(idx, size, align);
                }
                idx = tag.free_next;
            }
        }

        self.stats.alloc_failures += 1;
        None
    }

    /// 首次匹配分配
    fn alloc_first_fit(&mut self, size: usize, align: usize) -> Option<usize> {
        // first-fit 只要看到第一个可容纳目标区间的 free tag 就停，搜索成本低，但更容易
        // 在表头附近反复切碎大块，碎片分布通常不如 best-fit 平滑。
        let start_bucket = self.size_to_bucket(size);

        for bucket in start_bucket..VMEM_FREELISTS {
            let mut idx = self.free_lists[bucket];
            while idx != INVALID_TAG {
                let tag = read_tag(idx);
                let tag_base = tag.base;
                let Some(aligned_base) = align_up(tag_base, align) else {
                    idx = tag.free_next;
                    continue;
                };
                let waste = aligned_base - tag_base;
                let Some(needed) = size.checked_add(waste) else {
                    idx = tag.free_next;
                    continue;
                };
                if tag.size >= needed {
                    return self.allocate_from_tag(idx, size, align);
                }
                idx = tag.free_next;
            }
        }

        self.stats.alloc_failures += 1;
        None
    }

    /// 从指定空闲标签中分配
    ///
    /// 处理对齐浪费和尾部剩余的分裂。
    ///
    /// 实现上统一委托给 [`vmem.reserve_from_tag()`]，避免普通分配路径和显式 reserve 路径
    /// 各自维护一套切分逻辑而产生段列表不一致问题。
    fn allocate_from_tag(&mut self, tag_idx: usize, size: usize, align: usize) -> Option<usize> {
        let tag = read_tag(tag_idx);
        let tag_base = tag.base;
        let tag_size = tag.size;
        let aligned_base = align_up(tag_base, align)?;
        let front_waste = aligned_base - tag_base;
        let total_needed = size.checked_add(front_waste)?;
        if tag_size < total_needed {
            return None;
        }
        if !self.reserve_from_tag(tag_idx, aligned_base, size) {
            return None;
        }

        Some(aligned_base)
    }

    fn reserve_from_tag(&mut self, tag_idx: usize, base: usize, size: usize) -> bool {
        // 这条路径和 `allocate_from_tag` 很像，但输入区间是外部指定的。它要做的是把一个
        // free tag 切成“前缀空闲 + 指定保留区 + 尾部空闲”三部分，其中保留区不做搜索，
        // 直接转成 Allocated 语义，作为地址所有权的既成事实。
        let tag = read_tag(tag_idx);
        let tag_base = tag.base;
        let tag_size = tag.size;
        let Some(tag_end) = tag_base.checked_add(tag_size) else {
            return false;
        };
        let Some(end) = base.checked_add(size) else {
            return false;
        };

        if base < tag_base || end > tag_end {
            return false;
        }

        let needs_front = base > tag_base;
        let remaining = tag_end - end;
        let needs_tail = remaining >= self.quantum;

        // 先拿齐切分需要的元数据标签，再修改 arena。这样元数据耗尽时可以无副作用失败，
        // 不会把 requested range 隐式扩大成整个 free tag，也不会留下临时碎片。
        let front_tag = if needs_front {
            match self.alloc_tag() {
                Some(idx) => Some(idx),
                None => {
                    self.stats.metadata_failures += 1;
                    self.stats.alloc_failures += 1;
                    return false;
                }
            }
        } else {
            None
        };
        let tail_tag = if needs_tail {
            match self.alloc_tag() {
                Some(idx) => Some(idx),
                None => {
                    if let Some(idx) = front_tag {
                        self.free_tag(idx);
                    }
                    self.stats.metadata_failures += 1;
                    self.stats.alloc_failures += 1;
                    return false;
                }
            }
        } else {
            None
        };

        self.remove_from_freelist(tag_idx);

        if let Some(front_tag) = front_tag {
            let mut front = BoundaryTag::empty();
            front.base = tag_base;
            front.size = base - tag_base;
            front.bt_type = BtType::Free;
            front.in_use = true;
            write_tag(front_tag, front);

            self.insert_seg_before(tag_idx, front_tag);

            self.add_to_freelist(front_tag);
            self.stats.split_count += 1;
            let mut current = read_tag(tag_idx);
            current.base = base;
            current.size = tag_end - base;
            write_tag(tag_idx, current);
        }

        if let Some(tail_tag) = tail_tag {
            let mut tail = BoundaryTag::empty();
            tail.base = end;
            tail.size = remaining;
            tail.bt_type = BtType::Free;
            tail.in_use = true;
            write_tag(tail_tag, tail);
            self.insert_seg_after(tag_idx, tail_tag);
            self.add_to_freelist(tail_tag);
            self.stats.split_count += 1;
            let mut allocated = read_tag(tag_idx);
            allocated.size = size;
            write_tag(tag_idx, allocated);
        }

        let mut allocated = read_tag(tag_idx);
        allocated.bt_type = BtType::Allocated;
        write_tag(tag_idx, allocated);
        self.stats.allocated_size += allocated.size;
        self.stats.free_size = self.stats.free_size.saturating_sub(allocated.size);

        true
    }

    /// 释放之前分配的地址区间
    ///
    /// 标记为空闲并尝试与相邻空闲段合并
    pub fn free(&mut self, addr: usize, size: usize) -> bool {
        self.free_result(addr, size).is_ok()
    }

    pub fn free_result(&mut self, addr: usize, size: usize) -> Result<(), VmemError> {
        // 释放路径的重点是“按地址和大小找到对应 allocated tag”，然后把它标成
        // free，并与左右相邻空闲段即时合并。即时合并能显著降低地址空间碎片化速度。
        if !self.initialized {
            return Err(VmemError::NotInitialized);
        }
        let Some(expected_size) = align_up(size, self.quantum) else {
            self.stats.invalid_free_failures += 1;
            return Err(VmemError::InvalidSize);
        };

        self.stats.free_count += 1;
        VMEM_FREE_COUNT.fetch_add(1, Ordering::Relaxed);

        // 在段列表中查找对应标签
        let mut idx = self.seg_list_head;
        while idx != INVALID_TAG {
            let tag = read_tag(idx);
            if tag.base == addr && tag.bt_type == BtType::Allocated {
                break;
            }
            idx = tag.seg_next;
        }

        if idx == INVALID_TAG {
            self.stats.invalid_free_failures += 1;
            return Err(VmemError::NotAllocated);
        }

        let mut current = read_tag(idx);
        let tag_size = current.size;
        if tag_size != expected_size {
            self.stats.size_mismatch_failures += 1;
            return Err(VmemError::SizeMismatch {
                expected: expected_size,
                actual: tag_size,
            });
        }
        current.bt_type = BtType::Free;
        write_tag(idx, current);
        self.stats.allocated_size = self.stats.allocated_size.saturating_sub(tag_size);
        self.stats.free_size += tag_size;

        // 尝试与后继空闲段合并
        let next = read_tag(idx).seg_next;
        if next != INVALID_TAG && read_tag(next).bt_type == BtType::Free {
            self.remove_from_freelist(next);
            let mut current = read_tag(idx);
            current.size += read_tag(next).size;
            write_tag(idx, current);
            self.remove_seg(next);
            self.free_tag(next);
            self.stats.coalesce_count += 1;
        }

        // 尝试与前驱空闲段合并
        let prev = read_tag(idx).seg_prev;
        if prev != INVALID_TAG && read_tag(prev).bt_type == BtType::Free {
            self.remove_from_freelist(prev);
            let mut prev_tag = read_tag(prev);
            prev_tag.size += read_tag(idx).size;
            write_tag(prev, prev_tag);
            self.remove_seg(idx);
            self.free_tag(idx);
            idx = prev;
            self.stats.coalesce_count += 1;
        }

        // 将合并后的空闲标签加入空闲链表
        self.add_to_freelist(idx);
        Ok(())
    }

    /// 获取统计信息
    pub fn stats(&self) -> VmemStats {
        let mut stats = self.stats;
        stats.free_segments = 0;
        stats.largest_free_size = 0;
        let mut idx = self.seg_list_head;
        while idx != INVALID_TAG {
            let tag = read_tag(idx);
            if tag.bt_type == BtType::Free {
                stats.free_segments += 1;
                stats.largest_free_size = stats.largest_free_size.max(tag.size);
            }
            idx = tag.seg_next;
        }
        stats
    }

    /// 检查地址当前是否落在已分配区间中。
    pub fn is_allocated(&self, addr: usize) -> bool {
        let mut idx = self.seg_list_head;
        while idx != INVALID_TAG {
            let tag = read_tag(idx);
            if tag.bt_type == BtType::Allocated {
                let Some(end) = tag.base.checked_add(tag.size) else {
                    return false;
                };
                if addr >= tag.base && addr < end {
                    return true;
                }
            }
            idx = tag.seg_next;
        }
        false
    }
}

#[inline]
fn read_tag(addr: usize) -> BoundaryTag {
    unsafe { *(addr as *const BoundaryTag) }
}

#[inline]
fn write_tag(addr: usize, tag: BoundaryTag) {
    unsafe {
        (addr as *mut BoundaryTag).write(tag);
    }
}

fn range_overlaps_existing_span(mut head: usize, base: usize, size: usize) -> bool {
    let Some(end) = base.checked_add(size) else {
        return true;
    };
    while head != INVALID_TAG {
        let tag = read_tag(head);
        if matches!(tag.bt_type, BtType::Span) {
            if let Some(tag_end) = tag.base.checked_add(tag.size) {
                if base < tag_end && end > tag.base {
                    return true;
                }
            } else {
                return true;
            }
        }
        head = tag.seg_next;
    }
    false
}

#[inline]
fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || (align & (align - 1)) != 0 {
        return None;
    }
    Some(value.checked_add(align - 1)? & !(align - 1))
}

/// 全局 vmem 操作计数器
pub static VMEM_ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
pub static VMEM_FREE_COUNT: AtomicU64 = AtomicU64::new(0);
