//! 垃圾回收核心 —— 高度成熟版
//!
//! 特性：
//! - 精确追踪 (exact‑first)，基于 TraceDescriptor
//! - 移动式 evacuation (young / old compact)
//! - 半区新生代 (eden + survivor) + evacuation failure 处理
//! - 碎片感知的老年代压缩决策
//! - 增量标记与并发标记框架 (ConcurrentMarkState)
//! - 完整观测：阶段耗时、碎片率、promotion/evacuation 统计
//! - 自动精确根提供者接口（替代保守扫描）
//!
//! 并发安全：所有可变操作要求 `&mut self`；多核 safepoint 由 `ManagedAllocator` 提供。

use core::{cell::UnsafeCell, marker::PhantomData};

/// 三色标记颜色
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum GcColor {
    White = 0,
    Gray = 1,
    Black = 2,
}

// ---------------------------------------------------------------------------
// 精确追踪描述符
// ---------------------------------------------------------------------------

/// 精确追踪描述符 – 所有 managed 对象必须携带。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceDescriptor {
    pub object_size: u32,
    pub object_align: u16,
    pub flags: u16,
    pub reference_offsets: &'static [usize],
    pub weak_reference_offsets: &'static [usize],
}

pub const TRACE_KIND_EXACT: u16 = 1 << 0;
pub const TRACE_FLAG_HAS_WEAK_REFS: u16 = 1 << 1;
pub const TRACE_FLAG_HAS_FINALIZER: u16 = 1 << 2;
pub const TRACE_FLAG_PINNED_LAYOUT: u16 = 1 << 3;

impl TraceDescriptor {
    /// 精确追踪 – 无引用字段
    pub const fn exact_no_references() -> Self {
        Self {
            object_size: 0,
            object_align: 0,
            flags: TRACE_KIND_EXACT,
            reference_offsets: &[],
            weak_reference_offsets: &[],
        }
    }

    /// 精确追踪 – 指定强引用偏移列表
    pub const fn exact(reference_offsets: &'static [usize]) -> Self {
        Self {
            object_size: 0,
            object_align: 0,
            flags: TRACE_KIND_EXACT,
            reference_offsets,
            weak_reference_offsets: &[],
        }
    }

    /// 精确追踪 – 同时指定布局尺寸与对齐
    pub const fn exact_layout(
        object_size: usize,
        object_align: usize,
        reference_offsets: &'static [usize],
    ) -> Self {
        Self {
            object_size: object_size as u32,
            object_align: object_align as u16,
            flags: TRACE_KIND_EXACT,
            reference_offsets,
            weak_reference_offsets: &[],
        }
    }

    pub const fn with_weak_references(mut self, weak_reference_offsets: &'static [usize]) -> Self {
        self.weak_reference_offsets = weak_reference_offsets;
        if !weak_reference_offsets.is_empty() {
            self.flags |= TRACE_FLAG_HAS_WEAK_REFS;
        }
        self
    }

    pub const fn with_flags(mut self, flags: u16) -> Self {
        self.flags |= flags;
        self
    }

    pub const fn is_exact(self) -> bool {
        self.flags & TRACE_KIND_EXACT != 0
    }

    pub fn matches_layout(self, object_size: usize, object_align: usize) -> bool {
        if !self.is_exact() {
            return false;
        }
        if self.object_size != 0 && self.object_size as usize != object_size {
            return false;
        }
        if self.object_align != 0 && self.object_align as usize != object_align {
            return false;
        }
        self.reference_offsets
            .iter()
            .copied()
            .all(|off| self.offset_is_valid(object_size, off))
            && self
                .weak_reference_offsets
                .iter()
                .copied()
                .all(|off| self.offset_is_valid(object_size, off))
    }

    pub fn allows_ref_offset(self, object_size: usize, field_offset: usize) -> bool {
        self.offset_is_valid(object_size, field_offset)
            && (!self.is_exact() || self.reference_offsets.contains(&field_offset))
    }

    pub fn allows_weak_ref_offset(self, object_size: usize, field_offset: usize) -> bool {
        self.offset_is_valid(object_size, field_offset)
            && (!self.is_exact() || self.weak_reference_offsets.contains(&field_offset))
    }

    fn offset_is_valid(self, object_size: usize, field_offset: usize) -> bool {
        let word_size = core::mem::size_of::<usize>();
        field_offset.is_multiple_of(word_size)
            && field_offset
                .checked_add(word_size)
                .is_some_and(|end| end <= object_size)
    }
}

/// 默认精确无引用描述符（公共常量）
pub static EXACT_NO_REFERENCES_DESCRIPTOR: TraceDescriptor = TraceDescriptor::exact_no_references();

// ---------------------------------------------------------------------------
// 受管引用包装类型 – 仅用于对象布局，实际写屏障由 managed 层驱动
// ---------------------------------------------------------------------------

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcRef<T> {
    raw: usize,
    _marker: PhantomData<*const T>,
}

impl<T> GcRef<T> {
    pub const fn null() -> Self {
        Self {
            raw: 0,
            _marker: PhantomData,
        }
    }
    pub const fn from_raw(raw: usize) -> Self {
        Self {
            raw,
            _marker: PhantomData,
        }
    }
    pub const fn as_raw(self) -> usize {
        self.raw
    }
    pub const fn is_null(self) -> bool {
        self.raw == 0
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcWeakRef<T> {
    raw: usize,
    _marker: PhantomData<*const T>,
}

impl<T> GcWeakRef<T> {
    pub const fn null() -> Self {
        Self {
            raw: 0,
            _marker: PhantomData,
        }
    }
    pub const fn from_raw(raw: usize) -> Self {
        Self {
            raw,
            _marker: PhantomData,
        }
    }
    pub const fn as_raw(self) -> usize {
        self.raw
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcRefSlot<T> {
    offset: usize,
    _marker: PhantomData<fn() -> T>,
}

impl<T> GcRefSlot<T> {
    pub const fn new(offset: usize) -> Self {
        Self {
            offset,
            _marker: PhantomData,
        }
    }

    pub const fn offset(self) -> usize {
        self.offset
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcWeakRefSlot<T> {
    offset: usize,
    _marker: PhantomData<fn() -> T>,
}

impl<T> GcWeakRefSlot<T> {
    pub const fn new(offset: usize) -> Self {
        Self {
            offset,
            _marker: PhantomData,
        }
    }

    pub const fn offset(self) -> usize {
        self.offset
    }
}

/// 受管字段槽位 – 显式标记这里是一个 GC 字段，写屏障由上层在修改时触发。
#[repr(transparent)]
pub struct GcCell<T> {
    value: UnsafeCell<T>,
}

impl<T> GcCell<T> {
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
        }
    }
}

// ---------------------------------------------------------------------------
// 对象头
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GcObjectHeader {
    pub size: u32,
    pub color: u8,
    pub generation: u8,
    pub flags: u16,
    pub finalizer_id: u16,
    pub prefix_bytes: u16,
    pub reserve_size_lo: u16,
    pub reserve_size_hi: u16,
    pub trace_descriptor_ptr: usize,
    pub forwarding_ptr: usize,
}

pub const GC_FLAG_PINNED: u16 = 1 << 0;
pub const GC_FLAG_WEAK_REF: u16 = 1 << 1;
pub const GC_FLAG_HAS_FINALIZER: u16 = 1 << 2;
pub const GC_FLAG_FINALIZED: u16 = 1 << 3;
pub const GC_FLAG_OLD_GEN: u16 = 1 << 4;
pub const GC_FLAG_CARD_DIRTY: u16 = 1 << 5;
pub const GC_FLAG_REMEMBERED: u16 = 1 << 6;
pub const GC_FLAG_EVACUATING: u16 = 1 << 7;
pub const GC_FLAG_FORWARDED: u16 = 1 << 8;

impl GcObjectHeader {
    pub const fn new(size: u32) -> Self {
        Self {
            size,
            color: GcColor::White as u8,
            generation: 0,
            flags: 0,
            finalizer_id: 0,
            prefix_bytes: 0,
            reserve_size_lo: 0,
            reserve_size_hi: 0,
            trace_descriptor_ptr: 0,
            forwarding_ptr: 0,
        }
    }

    pub const HEADER_SIZE: usize = core::mem::size_of::<Self>();

    pub fn set_trace_descriptor(&mut self, descriptor: &'static TraceDescriptor) {
        self.trace_descriptor_ptr = descriptor as *const TraceDescriptor as usize;
    }

    pub fn trace_descriptor(self) -> &'static TraceDescriptor {
        if self.trace_descriptor_ptr == 0 {
            &EXACT_NO_REFERENCES_DESCRIPTOR
        } else {
            unsafe { &*(self.trace_descriptor_ptr as *const TraceDescriptor) }
        }
    }

    pub const fn forwarded(self) -> Option<usize> {
        if self.flags & GC_FLAG_FORWARDED != 0 && self.forwarding_ptr != 0 {
            Some(self.forwarding_ptr)
        } else {
            None
        }
    }

    pub fn set_forwarding(&mut self, forwarding_ptr: usize) {
        self.forwarding_ptr = forwarding_ptr;
        if forwarding_ptr == 0 {
            self.flags &= !GC_FLAG_FORWARDED;
            self.flags &= !GC_FLAG_EVACUATING;
        } else {
            self.flags |= GC_FLAG_FORWARDED;
            self.flags |= GC_FLAG_EVACUATING;
        }
    }
}

// ---------------------------------------------------------------------------
// 根、句柄与终结器基础类型
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootType {
    Stack,
    Register,
    Global,
    KernelRef,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GcHandle {
    pub(crate) slot: u16,
    pub(crate) generation: u32,
}

impl GcHandle {
    pub const fn new(slot: u16, generation: u32) -> Self {
        Self { slot, generation }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct GcWeakHandle {
    pub(crate) slot: u16,
    pub(crate) generation: u32,
}

impl GcWeakHandle {
    pub const fn new(slot: u16, generation: u32) -> Self {
        Self { slot, generation }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct GcRootHandle {
    pub(crate) idx: u16,
}

impl GcRootHandle {
    pub const fn new(idx: u16) -> Self {
        Self { idx }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct GcRootFrame<const N: usize> {
    pub(crate) slots: [u16; N],
    pub(crate) root_type: RootType,
    pub(crate) source_base: usize,
}

impl<const N: usize> GcRootFrame<N> {
    pub const fn new(root_type: RootType, source_base: usize) -> Self {
        Self {
            slots: [u16::MAX; N],
            root_type,
            source_base,
        }
    }
    pub const fn slot_count(&self) -> usize {
        N
    }
}

pub type FinalizerFn = fn(obj_ptr: usize, obj_size: usize);

#[derive(Clone, Copy)]
pub struct FinalizerEntry {
    pub callback: Option<FinalizerFn>,
    pub active: bool,
}

impl FinalizerEntry {
    pub const fn empty() -> Self {
        Self {
            callback: None,
            active: false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct WriteBarrierEvent {
    pub obj_addr: usize,
    pub field_offset: usize,
    pub new_ref: usize,
    pub old_ref: usize,
}

#[derive(Clone, Copy)]
pub struct PendingFinalizer {
    pub callback: Option<FinalizerFn>,
    pub obj_addr: usize,
    pub obj_size: usize,
}

impl PendingFinalizer {
    pub const fn empty() -> Self {
        Self {
            callback: None,
            obj_addr: 0,
            obj_size: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub struct GcHandleSlot {
    pub object_addr: usize,
    pub generation: u32,
    pub strong_refs: u32,
    pub weak_refs: u32,
    pub root_refs: u32,
    pub pin_refs: u32,
    pub active: bool,
}

impl GcHandleSlot {
    pub const fn empty() -> Self {
        Self {
            object_addr: 0,
            generation: 0,
            strong_refs: 0,
            weak_refs: 0,
            root_refs: 0,
            pin_refs: 0,
            active: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcMode {
    MarkSweep,
    MarkCompact,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcPhase {
    Idle,
    RootScan,
    InitialMark,
    MarkPropagate,
    Remark,
    Sweep,
    Compact,
    Finalize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcCollectionKind {
    None,
    IncrementalMark,
    Minor,
    Major,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcControlSnapshot {
    pub mode: GcMode,
    pub phase: GcPhase,
    pub running: bool,
    pub safepoint_requested: bool,
    pub last_collection_kind: GcCollectionKind,
}

// ---------------------------------------------------------------------------
// 增强统计信息
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, Debug)]
pub struct GcStats {
    pub minor_gc_count: u64,
    pub major_gc_count: u64,
    pub incremental_mark_steps: u64,
    pub objects_marked: u64,
    pub objects_swept: u64,
    pub objects_compacted: u64,
    pub finalizers_run: u64,
    pub last_pause_ns: u64,
    pub total_pause_ns: u64,
    pub bytes_reclaimed: u64,
    pub young_gen_objects: u64,
    pub old_gen_objects: u64,
    pub promoted_objects: u64,
    pub write_barrier_count: u64,
    pub dirty_cards: usize,
    pub remembered_objects: usize,
    pub strong_handle_slots: usize,
    pub weak_handle_slots: usize,
    pub pinned_handle_slots: usize,
    pub object_table_entries: usize,
    pub object_table_capacity: usize,
    pub object_table_failures: u64,
    pub automatic_root_entries: usize,
    pub pending_finalizers: usize,
    pub last_collection_kind: GcCollectionKind,
    pub mark_time_ns: u64,
    pub sweep_time_ns: u64,
    pub compact_time_ns: u64,
    pub evacuation_failures: u64,
    pub survivor_bytes: u64,
    pub promoted_bytes: u64,
    pub relocated_bytes: u64,
    pub fragmentation_ratio: u32,
}

impl GcStats {
    pub const fn new() -> Self {
        Self {
            minor_gc_count: 0,
            major_gc_count: 0,
            incremental_mark_steps: 0,
            objects_marked: 0,
            objects_swept: 0,
            objects_compacted: 0,
            finalizers_run: 0,
            last_pause_ns: 0,
            total_pause_ns: 0,
            bytes_reclaimed: 0,
            young_gen_objects: 0,
            old_gen_objects: 0,
            promoted_objects: 0,
            write_barrier_count: 0,
            dirty_cards: 0,
            remembered_objects: 0,
            strong_handle_slots: 0,
            weak_handle_slots: 0,
            pinned_handle_slots: 0,
            object_table_entries: 0,
            object_table_capacity: MAX_OBJECTS,
            object_table_failures: 0,
            automatic_root_entries: 0,
            pending_finalizers: 0,
            last_collection_kind: GcCollectionKind::None,
            mark_time_ns: 0,
            sweep_time_ns: 0,
            compact_time_ns: 0,
            evacuation_failures: 0,
            survivor_bytes: 0,
            promoted_bytes: 0,
            relocated_bytes: 0,
            fragmentation_ratio: 0,
        }
    }
}

impl Default for GcStats {
    fn default() -> Self {
        Self::new()
    }
}

// 容量常量
const MAX_ROOTS: usize = 4096;
const MAX_FINALIZERS: usize = 256;
const MARK_STACK_SIZE: usize = 4096;
const WRITE_BARRIER_BUFFER_SIZE: usize = 1024;
pub(crate) const MAX_PENDING_FINALIZERS: usize = 256;
pub(crate) const CARD_TABLE_SIZE: usize = 8192;
const CARD_SIZE: usize = 512;
const MAX_OBJECTS: usize = 8192;
const MAX_HANDLE_SLOTS: usize = 8192;
const INVALID_HANDLE_SLOT: u16 = u16::MAX;
pub const PROMOTION_THRESHOLD: u8 = 3;

#[derive(Clone, Copy)]
pub struct GcObjectEntry {
    pub header_addr: usize,
    pub object_addr: usize,
    pub raw_base: usize,
    pub reserve_size: usize,
    pub object_size: usize,
    pub object_align: usize,
    pub trace_descriptor_ptr: usize,
    pub active: bool,
}

impl GcObjectEntry {
    pub const fn empty() -> Self {
        Self {
            header_addr: 0,
            object_addr: 0,
            raw_base: 0,
            reserve_size: 0,
            object_size: 0,
            object_align: 0,
            trace_descriptor_ptr: 0,
            active: false,
        }
    }

    pub(crate) fn trace_descriptor(self) -> &'static TraceDescriptor {
        if self.trace_descriptor_ptr == 0 {
            &EXACT_NO_REFERENCES_DESCRIPTOR
        } else {
            unsafe { &*(self.trace_descriptor_ptr as *const TraceDescriptor) }
        }
    }

    fn contains(self, ptr: usize) -> bool {
        self.active
            && ptr >= self.object_addr
            && ptr < self.object_addr.saturating_add(self.object_size)
    }
}

#[derive(Clone, Copy)]
pub struct GcRoot {
    pub ptr: usize,
    pub root_type: RootType,
    pub source_id: usize,
    pub active: bool,
    pub automatic: bool,
    pub handle_slot: u16,
    pub handle_generation: u32,
}

impl GcRoot {
    pub const fn empty() -> Self {
        Self {
            ptr: 0,
            root_type: RootType::Global,
            source_id: 0,
            active: false,
            automatic: false,
            handle_slot: INVALID_HANDLE_SLOT,
            handle_generation: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// 并发标记状态
// ---------------------------------------------------------------------------
pub struct ConcurrentMarkState {
    pub background_worker_active: bool,
    pub mark_queue: [usize; 1024],
    pub queue_head: usize,
    pub queue_tail: usize,
}

impl ConcurrentMarkState {
    pub const fn new() -> Self {
        Self {
            background_worker_active: false,
            mark_queue: [0; 1024],
            queue_head: 0,
            queue_tail: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// 垃圾回收器核心实现
// ---------------------------------------------------------------------------

pub struct GarbageCollector {
    pub mode: GcMode,
    pub phase: GcPhase,
    pub roots: [GcRoot; MAX_ROOTS],
    pub root_count: usize,
    pub finalizers: [FinalizerEntry; MAX_FINALIZERS],
    pub finalizer_count: usize,
    pub mark_stack: [usize; MARK_STACK_SIZE],
    pub mark_stack_top: usize,
    pub write_barrier_buffer: [WriteBarrierEvent; WRITE_BARRIER_BUFFER_SIZE],
    pub write_barrier_count: usize,
    pub card_table: [u8; CARD_TABLE_SIZE],
    pub card_table_base: usize,
    pub objects: [GcObjectEntry; MAX_OBJECTS],
    pub object_count: usize,
    pub live_object_count: usize,
    pub handle_slots: [GcHandleSlot; MAX_HANDLE_SLOTS],
    pub handle_slot_count: usize,
    pub heap_start: usize,
    pub heap_end: usize,
    pub young_gen_start: usize,
    pub young_gen_end: usize,
    pub eden_start: usize,
    pub eden_end: usize,
    pub survivor_from_start: usize,
    pub survivor_from_end: usize,
    pub survivor_to_start: usize,
    pub survivor_to_end: usize,
    pub stats: GcStats,
    pub initialized: bool,
    pub running: bool,
    pub free_callback: Option<fn(ptr: usize, size: usize)>,
    pub timestamp_ns: Option<fn() -> u64>,
    pub pending_finalizers: [PendingFinalizer; MAX_PENDING_FINALIZERS],
    pub pending_finalizer_count: usize,
    pub concurrent_mark: ConcurrentMarkState,
}

impl GarbageCollector {
    pub const fn new() -> Self {
        Self {
            mode: GcMode::MarkSweep,
            phase: GcPhase::Idle,
            roots: [GcRoot::empty(); MAX_ROOTS],
            root_count: 0,
            finalizers: [FinalizerEntry::empty(); MAX_FINALIZERS],
            finalizer_count: 0,
            mark_stack: [0; MARK_STACK_SIZE],
            mark_stack_top: 0,
            write_barrier_buffer: [WriteBarrierEvent {
                obj_addr: 0,
                field_offset: 0,
                new_ref: 0,
                old_ref: 0,
            }; WRITE_BARRIER_BUFFER_SIZE],
            write_barrier_count: 0,
            card_table: [0; CARD_TABLE_SIZE],
            card_table_base: 0,
            objects: [GcObjectEntry::empty(); MAX_OBJECTS],
            object_count: 0,
            live_object_count: 0,
            handle_slots: [GcHandleSlot::empty(); MAX_HANDLE_SLOTS],
            handle_slot_count: 0,
            heap_start: 0,
            heap_end: 0,
            young_gen_start: 0,
            young_gen_end: 0,
            eden_start: 0,
            eden_end: 0,
            survivor_from_start: 0,
            survivor_from_end: 0,
            survivor_to_start: 0,
            survivor_to_end: 0,
            stats: GcStats::new(),
            initialized: false,
            running: false,
            free_callback: None,
            timestamp_ns: None,
            pending_finalizers: [PendingFinalizer::empty(); MAX_PENDING_FINALIZERS],
            pending_finalizer_count: 0,
            concurrent_mark: ConcurrentMarkState::new(),
        }
    }

    pub fn init(
        &mut self,
        heap_start: usize,
        heap_size: usize,
        mode: GcMode,
        free_callback: fn(ptr: usize, size: usize),
        timestamp_ns: Option<fn() -> u64>,
    ) {
        self.heap_start = heap_start;
        self.heap_end = heap_start + heap_size;
        self.mode = mode;
        self.free_callback = Some(free_callback);
        self.timestamp_ns = timestamp_ns;

        let young_gen_size = heap_size / 4;
        self.young_gen_start = heap_start;
        self.young_gen_end = heap_start + young_gen_size;

        // 新生代分区：eden 占 young 的 60%，两个 survivor 各 20%
        let eden_size = young_gen_size * 3 / 5;
        let surv_size = (young_gen_size - eden_size) / 2;
        self.eden_start = heap_start;
        self.eden_end = heap_start + eden_size;
        self.survivor_from_start = self.eden_end;
        self.survivor_from_end = self.survivor_from_start + surv_size;
        self.survivor_to_start = self.survivor_from_end;
        self.survivor_to_end = self.survivor_to_start + surv_size;

        self.card_table_base = heap_start;
        self.objects = [GcObjectEntry::empty(); MAX_OBJECTS];
        self.object_count = 0;
        self.live_object_count = 0;
        self.handle_slots = [GcHandleSlot::empty(); MAX_HANDLE_SLOTS];
        self.handle_slot_count = 0;
        self.stats.object_table_entries = 0;
        self.stats.object_table_capacity = MAX_OBJECTS;
        self.stats.object_table_failures = 0;
        self.stats.automatic_root_entries = 0;
        self.stats.pending_finalizers = 0;
        self.pending_finalizers = [PendingFinalizer::empty(); MAX_PENDING_FINALIZERS];
        self.pending_finalizer_count = 0;
        self.concurrent_mark = ConcurrentMarkState::new();
        self.initialized = true;
    }

    // ========================================================================
    // 根管理
    // ========================================================================

    pub fn add_automatic_root(
        &mut self,
        ptr: usize,
        root_type: RootType,
        source_id: usize,
    ) -> Option<usize> {
        if ptr == 0 || !self.is_in_heap(ptr) {
            return None;
        }
        // 查找已有
        for idx in 0..self.root_count {
            let root = self.roots[idx];
            if root.active
                && root.automatic
                && root.ptr == ptr
                && root.root_type == root_type
                && root.source_id == source_id
            {
                return Some(idx);
            }
        }
        let idx = match self.find_free_root_slot() {
            Some(idx) => idx,
            None if self.root_count < MAX_ROOTS => {
                let idx = self.root_count;
                self.root_count += 1;
                idx
            }
            None => return None,
        };
        self.roots[idx] = GcRoot {
            ptr,
            root_type,
            source_id,
            active: true,
            automatic: true,
            handle_slot: INVALID_HANDLE_SLOT,
            handle_generation: 0,
        };
        self.stats.automatic_root_entries += 1;
        Some(idx)
    }

    pub fn add_handle_root(
        &mut self,
        handle: &GcHandle,
        root_type: RootType,
        source_id: usize,
    ) -> Option<GcRootHandle> {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation)
            || self.handle_slots[slot_idx].object_addr == 0
        {
            return None;
        }
        for idx in 0..self.root_count {
            let root = self.roots[idx];
            if root.active
                && !root.automatic
                && root.handle_slot == handle.slot
                && root.handle_generation == handle.generation
                && root.root_type == root_type
                && root.source_id == source_id
            {
                return Some(GcRootHandle::new(idx as u16));
            }
        }
        let idx = match self.find_free_root_slot() {
            Some(idx) => idx,
            None if self.root_count < MAX_ROOTS => {
                let idx = self.root_count;
                self.root_count += 1;
                idx
            }
            None => return None,
        };
        self.roots[idx] = GcRoot {
            ptr: 0,
            root_type,
            source_id,
            active: true,
            automatic: false,
            handle_slot: handle.slot,
            handle_generation: handle.generation,
        };
        self.handle_slots[slot_idx].root_refs =
            self.handle_slots[slot_idx].root_refs.saturating_add(1);
        Some(GcRootHandle::new(idx as u16))
    }

    pub fn update_handle_root(&mut self, root: &GcRootHandle, handle: &GcHandle) -> bool {
        let idx = root.idx as usize;
        let slot_idx = handle.slot as usize;
        if idx >= self.root_count
            || !self.roots[idx].active
            || !self.handle_slot_matches(slot_idx, handle.generation)
            || self.handle_slots[slot_idx].object_addr == 0
        {
            return false;
        }
        let previous = self.roots[idx];
        if previous.handle_slot != INVALID_HANDLE_SLOT {
            self.release_root_slot_ref(previous.handle_slot as usize);
        }
        self.roots[idx].ptr = 0;
        self.roots[idx].handle_slot = handle.slot;
        self.roots[idx].handle_generation = handle.generation;
        self.handle_slots[slot_idx].root_refs =
            self.handle_slots[slot_idx].root_refs.saturating_add(1);
        true
    }

    pub(crate) fn remove_root(&mut self, idx: usize) {
        if idx < self.root_count {
            let root = self.roots[idx];
            if root.active && root.handle_slot != INVALID_HANDLE_SLOT {
                self.release_root_slot_ref(root.handle_slot as usize);
            }
            if root.active && root.automatic {
                self.stats.automatic_root_entries =
                    self.stats.automatic_root_entries.saturating_sub(1);
            }
            self.roots[idx] = GcRoot::empty();
        }
    }

    pub fn remove_handle_root(&mut self, root: GcRootHandle) {
        self.remove_root(root.idx as usize);
    }

    pub(crate) fn clear_automatic_roots(&mut self) {
        for idx in 0..self.root_count {
            let root = self.roots[idx];
            if !root.active || !root.automatic {
                continue;
            }
            if root.handle_slot != INVALID_HANDLE_SLOT {
                self.release_root_slot_ref(root.handle_slot as usize);
            }
            self.roots[idx] = GcRoot::empty();
        }
        self.stats.automatic_root_entries = 0;
    }

    fn find_free_root_slot(&self) -> Option<usize> {
        (0..self.root_count).find(|&idx| !self.roots[idx].active)
    }

    fn handle_slot_matches(&self, slot_idx: usize, generation: u32) -> bool {
        slot_idx < self.handle_slot_count
            && self.handle_slots[slot_idx].active
            && self.handle_slots[slot_idx].generation == generation
    }

    fn release_root_slot_ref(&mut self, slot_idx: usize) {
        if slot_idx >= self.handle_slot_count {
            return;
        }
        self.handle_slots[slot_idx].root_refs =
            self.handle_slots[slot_idx].root_refs.saturating_sub(1);
        self.recycle_handle_slot_if_dead(slot_idx);
    }

    fn resolve_root_ptr(&self, root: GcRoot) -> Option<usize> {
        if root.handle_slot == INVALID_HANDLE_SLOT {
            if root.ptr == 0 {
                None
            } else {
                Some(self.resolve_forwarding_addr(root.ptr))
            }
        } else {
            let slot_idx = root.handle_slot as usize;
            if !self.handle_slot_matches(slot_idx, root.handle_generation) {
                return None;
            }
            let object_addr = self.handle_slots[slot_idx].object_addr;
            (object_addr != 0).then_some(self.resolve_forwarding_addr(object_addr))
        }
    }

    // ========================================================================
    // 句柄管理
    // ========================================================================

    /// 创建新句柄 – 总是分配全新槽位，确保句柄唯一性。
    pub fn create_handle(&mut self, object_addr: usize) -> Option<GcHandle> {
        let object_addr = self.resolve_forwarding_addr(object_addr);
        let object_idx = self.find_object_by_object_addr(object_addr)?;
        let header = unsafe { *(self.objects[object_idx].header_addr as *const GcObjectHeader) };

        // 寻找空闲槽位
        let slot_idx = match (0..self.handle_slot_count).find(|&idx| {
            !self.handle_slots[idx].active || self.slot_can_recycle(self.handle_slots[idx])
        }) {
            Some(idx) => idx,
            None if self.handle_slot_count < MAX_HANDLE_SLOTS => {
                let idx = self.handle_slot_count;
                self.handle_slot_count += 1;
                idx
            }
            None => return None,
        };

        let generation = self.handle_slots[slot_idx]
            .generation
            .wrapping_add(1)
            .max(1);
        self.handle_slots[slot_idx] = GcHandleSlot {
            object_addr,
            generation,
            strong_refs: 1,
            weak_refs: 0,
            root_refs: 0,
            pin_refs: if header.flags & GC_FLAG_PINNED != 0 {
                1
            } else {
                0
            },
            active: true,
        };
        Some(GcHandle::new(slot_idx as u16, generation))
    }

    pub fn retain_handle(&mut self, handle: &GcHandle) -> Option<GcHandle> {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation)
            || self.handle_slots[slot_idx].object_addr == 0
        {
            return None;
        }
        self.handle_slots[slot_idx].strong_refs =
            self.handle_slots[slot_idx].strong_refs.saturating_add(1);
        Some(GcHandle::new(handle.slot, handle.generation))
    }

    pub fn release_handle(&mut self, handle: GcHandle) {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation) {
            return;
        }
        self.handle_slots[slot_idx].strong_refs =
            self.handle_slots[slot_idx].strong_refs.saturating_sub(1);
        self.recycle_handle_slot_if_dead(slot_idx);
    }

    pub fn downgrade_handle(&mut self, handle: &GcHandle) -> Option<GcWeakHandle> {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation) {
            return None;
        }
        self.handle_slots[slot_idx].weak_refs =
            self.handle_slots[slot_idx].weak_refs.saturating_add(1);
        let object_addr = self.handle_slots[slot_idx].object_addr;
        if object_addr != 0 {
            self.set_object_flag(object_addr, GC_FLAG_WEAK_REF, true);
        }
        Some(GcWeakHandle::new(handle.slot, handle.generation))
    }

    /// 升级弱句柄为强句柄，并确保对象在标记周期内不会被清扫。
    pub fn upgrade_weak_handle(&mut self, handle: &GcWeakHandle) -> Option<GcHandle> {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation)
            || self.handle_slots[slot_idx].object_addr == 0
        {
            return None;
        }
        self.handle_slots[slot_idx].strong_refs =
            self.handle_slots[slot_idx].strong_refs.saturating_add(1);
        // 若 GC 处于标记阶段，确保对象被标记
        self.ensure_marked(self.handle_slots[slot_idx].object_addr);
        Some(GcHandle::new(handle.slot, handle.generation))
    }

    pub fn release_weak_handle(&mut self, handle: GcWeakHandle) {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation) {
            return;
        }
        self.handle_slots[slot_idx].weak_refs =
            self.handle_slots[slot_idx].weak_refs.saturating_sub(1);
        let object_addr = self.handle_slots[slot_idx].object_addr;
        if self.handle_slots[slot_idx].weak_refs == 0 && object_addr != 0 {
            self.set_object_flag(object_addr, GC_FLAG_WEAK_REF, false);
        }
        self.recycle_handle_slot_if_dead(slot_idx);
    }

    pub fn resolve_handle(&self, handle: &GcHandle) -> Option<usize> {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation) {
            return None;
        }
        let object_addr = self.handle_slots[slot_idx].object_addr;
        (object_addr != 0).then_some(self.resolve_forwarding_addr(object_addr))
    }

    /// 复活对象（用于终结器内）——清除终结标志并重新加入标记。
    pub fn revive_object(&mut self, object_addr: usize) -> bool {
        let Some(idx) = self.find_object_by_object_addr(object_addr) else {
            return false;
        };
        let header_addr = self.objects[idx].header_addr;
        let mut header = unsafe { *(header_addr as *const GcObjectHeader) };
        if header.flags & GC_FLAG_FINALIZED == 0 {
            return false;
        }
        header.flags &= !GC_FLAG_FINALIZED;
        header.color = GcColor::Gray as u8;
        unsafe {
            *(header_addr as *mut GcObjectHeader) = header;
        }
        self.push_mark_stack(object_addr);
        true
    }

    pub fn install_forwarding(&mut self, from_object_addr: usize, to_object_addr: usize) -> bool {
        if from_object_addr == to_object_addr {
            return false;
        }
        let Some(from_idx) = self.find_object_by_object_addr(from_object_addr) else {
            return false;
        };
        let Some(to_idx) = self.find_object_by_object_addr(to_object_addr) else {
            return false;
        };
        if self.objects[from_idx].object_size != self.objects[to_idx].object_size {
            return false;
        }

        let from_entry = self.objects[from_idx];
        let from_header_addr = from_entry.header_addr;
        let mut header = unsafe { *(from_header_addr as *const GcObjectHeader) };
        header.set_forwarding(to_object_addr);
        unsafe {
            *(from_header_addr as *mut GcObjectHeader) = header;
        }

        for idx in 0..self.handle_slot_count {
            if self.handle_slots[idx].active
                && self.handle_slots[idx].object_addr == from_object_addr
            {
                self.handle_slots[idx].object_addr = to_object_addr;
            }
        }

        for idx in 0..self.root_count {
            if !self.roots[idx].active || self.roots[idx].handle_slot != INVALID_HANDLE_SLOT {
                continue;
            }
            let ptr = self.roots[idx].ptr;
            if !from_entry.contains(ptr) {
                continue;
            }
            let offset = ptr.saturating_sub(from_object_addr);
            self.roots[idx].ptr = to_object_addr.saturating_add(offset.min(from_entry.object_size));
        }

        true
    }

    pub fn pin_handle(&mut self, handle: &GcHandle) -> bool {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation) {
            return false;
        }
        let object_addr = self.handle_slots[slot_idx].object_addr;
        let Some(object_idx) = self.find_object_by_object_addr(object_addr) else {
            return false;
        };
        let header_addr = self.objects[object_idx].header_addr;
        let mut header = unsafe { *(header_addr as *const GcObjectHeader) };
        header.flags |= GC_FLAG_PINNED;
        unsafe {
            *(header_addr as *mut GcObjectHeader) = header;
        }
        self.handle_slots[slot_idx].pin_refs = self.handle_slots[slot_idx]
            .pin_refs
            .saturating_add(1)
            .max(1);
        true
    }

    pub fn unpin_handle(&mut self, handle: &GcHandle) -> bool {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation)
            || self.handle_slots[slot_idx].pin_refs == 0
        {
            return false;
        }
        self.handle_slots[slot_idx].pin_refs -= 1;
        let object_addr = self.handle_slots[slot_idx].object_addr;
        let Some(object_idx) = self.find_object_by_object_addr(object_addr) else {
            return true;
        };
        let header_addr = self.objects[object_idx].header_addr;
        let mut header = unsafe { *(header_addr as *const GcObjectHeader) };
        if self.handle_slots[slot_idx].pin_refs == 0 {
            header.flags &= !GC_FLAG_PINNED;
        } else {
            header.flags |= GC_FLAG_PINNED;
        }
        unsafe {
            *(header_addr as *mut GcObjectHeader) = header;
        }
        true
    }

    pub fn is_handle_pinned(&self, handle: &GcHandle) -> bool {
        let slot_idx = handle.slot as usize;
        if !self.handle_slot_matches(slot_idx, handle.generation) {
            return false;
        }
        let object_addr = self.handle_slots[slot_idx].object_addr;
        let Some(object_idx) = self.find_object_by_object_addr(object_addr) else {
            return false;
        };
        let header_addr = self.objects[object_idx].header_addr;
        let header = unsafe { *(header_addr as *const GcObjectHeader) };
        header.flags & GC_FLAG_PINNED != 0
    }

    // ========================================================================
    // 对象表
    // ========================================================================

    pub fn register_object(
        &mut self,
        header_addr: usize,
        object_addr: usize,
        raw_base: usize,
        reserve_size: usize,
        object_size: usize,
        object_align: usize,
        trace_descriptor_ptr: usize,
    ) -> bool {
        if !self.initialized
            || !self.is_in_heap(header_addr)
            || !self.is_in_heap(object_addr)
            || object_size == 0
            || self.find_object_by_object_addr(object_addr).is_some()
            || self
                .find_object_overlapping(object_addr, object_size)
                .is_some()
        {
            self.stats.object_table_failures += 1;
            return false;
        }

        let slot = match (0..self.object_count).find(|&idx| !self.objects[idx].active) {
            Some(idx) => idx,
            None if self.object_count < MAX_OBJECTS => {
                let idx = self.object_count;
                self.object_count += 1;
                idx
            }
            None => {
                self.stats.object_table_failures += 1;
                return false;
            }
        };

        self.objects[slot] = GcObjectEntry {
            header_addr,
            object_addr,
            raw_base,
            reserve_size,
            object_size,
            object_align,
            trace_descriptor_ptr,
            active: true,
        };
        self.live_object_count += 1;
        self.stats.object_table_entries = self.live_object_count;
        if self.is_in_young_gen(object_addr) {
            self.stats.young_gen_objects += 1;
        } else {
            self.stats.old_gen_objects += 1;
        }
        true
    }

    pub fn unregister_object(&mut self, object_addr: usize) -> bool {
        if let Some(idx) = self.find_object_by_object_addr(object_addr) {
            self.deactivate_object(idx);
            true
        } else {
            false
        }
    }

    pub(crate) fn deactivate_object(&mut self, idx: usize) {
        if idx >= self.object_count || !self.objects[idx].active {
            return;
        }
        let entry = self.objects[idx];
        let was_young = self.is_in_young_gen(entry.object_addr);
        self.invalidate_handles_for_object(entry.object_addr);
        self.objects[idx].active = false;
        self.live_object_count = self.live_object_count.saturating_sub(1);
        self.stats.object_table_entries = self.live_object_count;
        if was_young {
            self.stats.young_gen_objects = self.stats.young_gen_objects.saturating_sub(1);
        } else {
            self.stats.old_gen_objects = self.stats.old_gen_objects.saturating_sub(1);
        }
    }

    fn invalidate_handles_for_object(&mut self, object_addr: usize) {
        for idx in 0..self.handle_slot_count {
            if self.handle_slots[idx].active && self.handle_slots[idx].object_addr == object_addr {
                self.handle_slots[idx].object_addr = 0;
                self.handle_slots[idx].pin_refs = 0;
                self.recycle_handle_slot_if_dead(idx);
            }
        }
    }

    pub(crate) fn find_object_by_object_addr(&self, object_addr: usize) -> Option<usize> {
        (0..self.object_count)
            .find(|&idx| self.objects[idx].active && self.objects[idx].object_addr == object_addr)
    }

    pub(crate) fn find_object_containing(&self, ptr: usize) -> Option<usize> {
        (0..self.object_count).find(|&idx| self.objects[idx].contains(ptr))
    }

    fn find_object_overlapping(&self, object_addr: usize, object_size: usize) -> Option<usize> {
        let object_end = object_addr.checked_add(object_size)?;
        for idx in 0..self.object_count {
            let entry = self.objects[idx];
            if !entry.active {
                continue;
            }
            let Some(entry_end) = entry.object_addr.checked_add(entry.object_size) else {
                continue;
            };
            if object_addr < entry_end && entry.object_addr < object_end {
                return Some(idx);
            }
        }
        None
    }

    // ========================================================================
    // 内部辅助：堆范围与分代判断
    // ========================================================================

    fn is_in_heap(&self, addr: usize) -> bool {
        addr >= self.heap_start && addr < self.heap_end
    }

    pub(crate) fn is_in_young_gen(&self, addr: usize) -> bool {
        let addr = self.resolve_forwarding_addr(addr);
        if let Some(idx) = self.find_object_containing(addr) {
            let header = unsafe { *(self.objects[idx].header_addr as *const GcObjectHeader) };
            header.flags & GC_FLAG_OLD_GEN == 0
        } else {
            false
        }
    }

    fn is_in_old_gen(&self, addr: usize) -> bool {
        let addr = self.resolve_forwarding_addr(addr);
        if let Some(idx) = self.find_object_containing(addr) {
            let header = unsafe { *(self.objects[idx].header_addr as *const GcObjectHeader) };
            header.flags & GC_FLAG_OLD_GEN != 0
        } else {
            false
        }
    }

    // ========================================================================
    // 转发链解析
    // ========================================================================
    pub(crate) fn resolve_forwarding_addr(&self, ptr: usize) -> usize {
        let mut current = ptr;
        let mut hops = 0usize;
        while hops < 8 {
            let Some(idx) = self.find_object_containing(current) else {
                break;
            };
            let entry = self.objects[idx];
            let header = unsafe { *(entry.header_addr as *const GcObjectHeader) };
            let Some(forwarded) = header.forwarded() else {
                break;
            };
            if forwarded == entry.object_addr {
                break;
            }
            let offset = current.saturating_sub(entry.object_addr);
            current = forwarded.saturating_add(offset.min(entry.object_size));
            hops += 1;
        }
        current
    }

    // ========================================================================
    // 标记栈操作
    // ========================================================================
    fn push_mark_stack(&mut self, addr: usize) {
        if self.mark_stack_top < MARK_STACK_SIZE {
            self.mark_stack[self.mark_stack_top] = addr;
            self.mark_stack_top += 1;
        }
    }

    fn pop_mark_stack(&mut self) -> Option<usize> {
        if self.mark_stack_top == 0 {
            return None;
        }
        self.mark_stack_top -= 1;
        Some(self.mark_stack[self.mark_stack_top])
    }

    // ========================================================================
    // 对象头读写
    // ========================================================================
    fn read_object_header(&self, addr: usize) -> Option<GcObjectHeader> {
        let idx = self.find_object_containing(addr)?;
        let header_addr = self.objects[idx].header_addr;
        let header = unsafe { *(header_addr as *const GcObjectHeader) };
        Some(header)
    }

    fn write_object_header(&mut self, addr: usize, header: &GcObjectHeader) {
        let Some(idx) = self.find_object_containing(addr) else {
            return;
        };
        let header_addr = self.objects[idx].header_addr;
        unsafe {
            *(header_addr as *mut GcObjectHeader) = *header;
        }
    }

    pub(crate) fn set_object_flag(&mut self, addr: usize, flag: u16, enabled: bool) {
        let Some(idx) = self.find_object_containing(addr) else {
            return;
        };
        let header_addr = self.objects[idx].header_addr;
        let mut header = unsafe { *(header_addr as *const GcObjectHeader) };
        if enabled {
            header.flags |= flag;
        } else {
            header.flags &= !flag;
        }
        unsafe {
            *(header_addr as *mut GcObjectHeader) = header;
        }
    }

    // ========================================================================
    // 标记与清扫核心
    // ========================================================================

    /// 将对象标记为灰色（若之前为白色），并推入标记栈。
    fn mark_gray(&mut self, addr: usize) {
        let addr = self.resolve_forwarding_addr(addr);
        if let Some(idx) = self.find_object_containing(addr) {
            let object_addr = self.objects[idx].object_addr;
            let mut header = unsafe { *(self.objects[idx].header_addr as *const GcObjectHeader) };
            if header.color == GcColor::White as u8 {
                header.color = GcColor::Gray as u8;
                unsafe {
                    *(self.objects[idx].header_addr as *mut GcObjectHeader) = header;
                }
                self.push_mark_stack(object_addr);
                self.stats.objects_marked += 1;
            }
        }
    }

    /// 确保对象在标记传播阶段不会被漏标（用于弱引用升级等场景）。
    pub(crate) fn ensure_marked(&mut self, addr: usize) {
        if self.phase != GcPhase::MarkPropagate && self.phase != GcPhase::Remark {
            return;
        }
        if let Some(idx) = self.find_object_containing(addr) {
            let header = unsafe { *(self.objects[idx].header_addr as *const GcObjectHeader) };
            if header.color == GcColor::White as u8 {
                self.mark_gray(addr);
            }
        }
    }

    /// 将对象标记为黑色，并扫描其精确引用字段，将引用的白色对象推入栈。
    fn mark_black(&mut self, addr: usize) {
        if let Some(mut header) = self.read_object_header(addr) {
            header.color = GcColor::Black as u8;
            self.write_object_header(addr, &header);

            let Some(idx) = self.find_object_containing(addr) else {
                return;
            };
            let entry = self.objects[idx];
            let descriptor = self.trace_descriptor_for_entry(entry);
            if !descriptor.is_exact() {
                return;
            }
            for &offset in descriptor.reference_offsets {
                if !descriptor.allows_ref_offset(entry.object_size, offset) {
                    continue;
                }
                let ref_val = unsafe { *((entry.object_addr + offset) as *const usize) };
                if self.is_in_heap(ref_val) {
                    self.mark_gray(ref_val);
                }
            }
        }
    }

    pub(crate) fn mark_young_refs_from_object(&mut self, addr: usize) {
        let Some(idx) = self.find_object_containing(addr) else {
            return;
        };
        let entry = self.objects[idx];
        let descriptor = self.trace_descriptor_for_entry(entry);
        if !descriptor.is_exact() {
            return;
        }
        for &offset in descriptor.reference_offsets {
            if !descriptor.allows_ref_offset(entry.object_size, offset) {
                continue;
            }
            let ref_val = unsafe { *((entry.object_addr + offset) as *const usize) };
            if self.is_in_young_gen(ref_val) {
                self.mark_gray(ref_val);
            }
        }
    }

    fn mark_black_young_only(&mut self, addr: usize) {
        if let Some(mut header) = self.read_object_header(addr) {
            if header.flags & GC_FLAG_OLD_GEN != 0 {
                return;
            }
            header.color = GcColor::Black as u8;
            self.write_object_header(addr, &header);
            self.mark_young_refs_from_object(addr);
        }
    }

    pub(crate) fn object_contains_young_ref(&self, addr: usize) -> bool {
        let Some(idx) = self.find_object_containing(addr) else {
            return false;
        };
        let entry = self.objects[idx];
        let descriptor = self.trace_descriptor_for_entry(entry);
        if !descriptor.is_exact() {
            return false;
        }
        for &offset in descriptor.reference_offsets {
            if !descriptor.allows_ref_offset(entry.object_size, offset) {
                continue;
            }
            let ref_val = unsafe { *((entry.object_addr + offset) as *const usize) };
            if self.is_in_young_gen(ref_val) {
                return true;
            }
        }
        false
    }

    pub(crate) fn trace_descriptor_for_entry(&self, entry: GcObjectEntry) -> TraceDescriptor {
        *entry.trace_descriptor()
    }

    // ---- 标记阶段 ----
    pub(crate) fn mark_phase(&mut self) {
        let start = self.now();
        self.phase = GcPhase::RootScan;

        for i in 0..self.root_count {
            if self.roots[i].active
                && let Some(ptr) = self.resolve_root_ptr(self.roots[i])
            {
                self.mark_gray(ptr);
            }
        }

        self.phase = GcPhase::InitialMark;
        self.phase = GcPhase::MarkPropagate;

        while let Some(addr) = self.pop_mark_stack() {
            self.mark_black(addr);
        }
        self.stats.mark_time_ns += self.now() - start;
    }

    // ---- 清扫阶段 ----
    pub(crate) fn sweep_phase(&mut self) {
        let start = self.now();
        self.phase = GcPhase::Sweep;

        let free_fn = match self.free_callback {
            Some(f) => f,
            None => return,
        };

        for idx in 0..self.object_count {
            if !self.objects[idx].active {
                continue;
            }
            let entry = self.objects[idx];
            if !self.is_in_heap(entry.object_addr) {
                continue;
            }
            if entry.raw_base > entry.header_addr
                || entry.reserve_size < GcObjectHeader::HEADER_SIZE + entry.object_size
            {
                self.deactivate_object(idx);
                continue;
            }
            let header_addr = entry.header_addr;
            let header = unsafe { *(header_addr as *const GcObjectHeader) };
            if header.size == 0 {
                self.deactivate_object(idx);
                continue;
            }
            let obj_addr = entry.object_addr;
            let obj_size = entry.object_size;
            let total_size = entry.reserve_size;

            if header.color == GcColor::White as u8 {
                if header.flags & GC_FLAG_HAS_FINALIZER != 0
                    && header.flags & GC_FLAG_FINALIZED == 0
                {
                    self.queue_finalizer(obj_addr, header.finalizer_id, obj_size);
                    let mut updated = header;
                    updated.flags |= GC_FLAG_FINALIZED;
                    unsafe {
                        *(header_addr as *mut GcObjectHeader) = updated;
                    }
                } else {
                    self.deactivate_object(idx);
                    free_fn(header_addr, total_size);
                    self.stats.objects_swept += 1;
                    self.stats.bytes_reclaimed += total_size as u64;
                }
            } else {
                let mut updated = header;
                updated.color = GcColor::White as u8;

                updated.generation = updated.generation.saturating_add(1);
                if updated.generation >= PROMOTION_THRESHOLD && updated.flags & GC_FLAG_OLD_GEN == 0
                {
                    updated.flags |= GC_FLAG_OLD_GEN;
                    self.account_promotion();
                }

                unsafe {
                    *(header_addr as *mut GcObjectHeader) = updated;
                }
                if updated.flags & GC_FLAG_OLD_GEN != 0 {
                    if self.object_contains_young_ref(obj_addr) {
                        self.mark_cards_for_range(obj_addr, obj_size);
                    } else {
                        self.set_object_flag(obj_addr, GC_FLAG_REMEMBERED, false);
                    }
                }
            }
        }
        self.stats.sweep_time_ns += self.now() - start;
        self.update_fragmentation_ratio();
    }

    fn update_fragmentation_ratio(&mut self) {
        // 计算老年代碎片率（空闲空间/总空间）
        let old_total = self.old_gen_bytes();
        if old_total == 0 {
            self.stats.fragmentation_ratio = 0;
            return;
        }
        let mut used = 0usize;
        for idx in 0..self.object_count {
            let e = self.objects[idx];
            if e.active && self.is_in_old_gen(e.object_addr) {
                used += e.reserve_size;
            }
        }
        let free = old_total.saturating_sub(used);
        self.stats.fragmentation_ratio = ((free as u64) * 1000 / old_total as u64) as u32;
    }

    fn old_gen_bytes(&self) -> usize {
        self.heap_end.saturating_sub(self.young_gen_end)
    }

    // ---- 分代晋升统计 ----
    pub(crate) fn account_promotion(&mut self) {
        self.stats.promoted_objects += 1;
        self.stats.young_gen_objects = self.stats.young_gen_objects.saturating_sub(1);
        self.stats.old_gen_objects += 1;
    }

    // ========================================================================
    // 增量标记与并发标记
    // ========================================================================
    pub fn incremental_mark(&mut self, batch_size: usize) -> bool {
        self.stats.last_collection_kind = GcCollectionKind::IncrementalMark;
        self.stats.incremental_mark_steps = self.stats.incremental_mark_steps.saturating_add(1);
        if self.phase == GcPhase::Idle {
            self.running = true;
            self.mark_stack_top = 0;
            self.concurrent_mark.queue_head = 0;
            self.concurrent_mark.queue_tail = 0;
            self.concurrent_mark.background_worker_active = true;
            self.phase = GcPhase::RootScan;
            for i in 0..self.root_count {
                if self.roots[i].active
                    && let Some(ptr) = self.resolve_root_ptr(self.roots[i])
                {
                    self.mark_gray(ptr);
                }
            }
            self.phase = GcPhase::InitialMark;
            self.phase = GcPhase::MarkPropagate;
        }

        if self.phase != GcPhase::MarkPropagate {
            self.concurrent_mark.background_worker_active = false;
            return false;
        }

        let mut processed = 0;
        while processed < batch_size {
            let next = self.concurrent_mark_pop().or_else(|| self.pop_mark_stack());
            match next {
                Some(addr) => {
                    self.mark_black(addr);
                    processed += 1;
                }
                None => {
                    self.phase = GcPhase::Remark;
                    self.concurrent_mark.background_worker_active = false;
                    return false;
                }
            }
        }

        true
    }

    pub fn continue_concurrent_mark(&mut self) -> bool {
        if self.phase != GcPhase::MarkPropagate || !self.concurrent_mark.background_worker_active {
            return false;
        }

        let mut processed = 0usize;
        while processed < 64 {
            let next = self.concurrent_mark_pop().or_else(|| self.pop_mark_stack());
            match next {
                Some(addr) => {
                    self.mark_black(addr);
                    processed += 1;
                }
                None => {
                    self.phase = GcPhase::Remark;
                    self.concurrent_mark.background_worker_active = false;
                    return false;
                }
            }
        }
        true
    }

    pub fn concurrent_mark_queue_push(&mut self, addr: usize) {
        let next = (self.concurrent_mark.queue_tail + 1) % 1024;
        if next == self.concurrent_mark.queue_head {
            return; // 队列满
        }
        self.concurrent_mark.mark_queue[self.concurrent_mark.queue_tail] = addr;
        self.concurrent_mark.queue_tail = next;
    }

    fn concurrent_mark_pop(&mut self) -> Option<usize> {
        if self.concurrent_mark.queue_head == self.concurrent_mark.queue_tail {
            return None;
        }
        let addr = self.concurrent_mark.mark_queue[self.concurrent_mark.queue_head];
        self.concurrent_mark.queue_head = (self.concurrent_mark.queue_head + 1) % 1024;
        Some(addr)
    }

    pub fn finish_incremental_cycle(&mut self) {
        if !self.initialized {
            return;
        }
        if self.phase == GcPhase::MarkPropagate
            && self.mark_stack_top == 0
            && self.concurrent_mark.queue_head == self.concurrent_mark.queue_tail
        {
            self.phase = GcPhase::Remark;
            self.concurrent_mark.background_worker_active = false;
        }
        if self.phase != GcPhase::Remark {
            return;
        }
        self.remark_roots();

        match self.mode {
            GcMode::MarkSweep => self.sweep_phase(),
            GcMode::MarkCompact => self.sweep_phase(), // 实际移动由上层负责
        }

        self.finish_collection_cycle();
    }

    // ========================================================================
    // 写屏障
    // ========================================================================
    pub fn write_barrier(
        &mut self,
        obj_addr: usize,
        field_offset: usize,
        new_ref: usize,
        old_ref: usize,
    ) {
        if !self.initialized {
            return;
        }

        if self.write_barrier_count < WRITE_BARRIER_BUFFER_SIZE {
            self.write_barrier_buffer[self.write_barrier_count] = WriteBarrierEvent {
                obj_addr,
                field_offset,
                new_ref,
                old_ref,
            };
            self.write_barrier_count += 1;
        }

        if self.is_in_old_gen(obj_addr) && self.is_in_young_gen(new_ref) {
            self.mark_card_dirty_addr(obj_addr.saturating_add(field_offset));
            self.set_object_flag(obj_addr, GC_FLAG_REMEMBERED, true);
        }

        if self.phase == GcPhase::MarkPropagate
            && self.is_in_heap(old_ref)
            && let Some(header) = self.read_object_header(old_ref)
            && header.color == GcColor::White as u8
        {
            let resolved = self.resolve_forwarding_addr(old_ref);
            self.push_mark_stack(resolved);
            // 同时推入并发队列，让后台也能处理
            self.concurrent_mark_queue_push(resolved);
        }
    }

    fn mark_card_dirty_addr(&mut self, addr: usize) {
        if addr < self.card_table_base {
            return;
        }
        let heap_end = self.card_table_base + CARD_TABLE_SIZE * CARD_SIZE;
        if addr >= heap_end {
            return;
        }
        let card_idx = (addr - self.card_table_base) / CARD_SIZE;
        if card_idx < CARD_TABLE_SIZE {
            self.card_table[card_idx] = 1;
            self.set_object_flag(addr, GC_FLAG_CARD_DIRTY, true);
        }
    }

    fn mark_cards_for_range(&mut self, addr: usize, size: usize) {
        if size == 0 || addr < self.card_table_base {
            return;
        }
        self.set_object_flag(addr, GC_FLAG_REMEMBERED, true);
        let range_end = addr.saturating_add(size.saturating_sub(1));
        let start_idx = (addr - self.card_table_base) / CARD_SIZE;
        let end_idx = (range_end - self.card_table_base) / CARD_SIZE;
        let upper = end_idx.min(CARD_TABLE_SIZE.saturating_sub(1));
        for card_idx in start_idx..=upper {
            self.card_table[card_idx] = 1;
        }
    }

    pub(crate) fn mark_cards_for_range_in_table(
        &self,
        table: &mut [u8; CARD_TABLE_SIZE],
        addr: usize,
        size: usize,
    ) {
        if size == 0 || addr < self.card_table_base {
            return;
        }
        let range_end = addr.saturating_add(size.saturating_sub(1));
        let start_idx = (addr - self.card_table_base) / CARD_SIZE;
        let end_idx = (range_end - self.card_table_base) / CARD_SIZE;
        let upper = end_idx.min(CARD_TABLE_SIZE.saturating_sub(1));
        (start_idx..=upper).for_each(|card_idx| {
            table[card_idx] = 1;
        });
    }

    pub(crate) fn clear_card_table(&mut self) {
        for i in 0..CARD_TABLE_SIZE {
            self.card_table[i] = 0;
        }
        for idx in 0..self.object_count {
            let entry = self.objects[idx];
            if entry.active {
                self.set_object_flag(entry.object_addr, GC_FLAG_CARD_DIRTY, false);
            }
        }
    }

    fn object_has_dirty_card(&self, addr: usize, size: usize) -> bool {
        if size == 0 || addr < self.card_table_base {
            return false;
        }
        let range_end = addr.saturating_add(size.saturating_sub(1));
        let start_idx = (addr - self.card_table_base) / CARD_SIZE;
        let end_idx = (range_end - self.card_table_base) / CARD_SIZE;
        let upper = end_idx.min(CARD_TABLE_SIZE.saturating_sub(1));
        for card_idx in start_idx..=upper {
            if self.card_table[card_idx] != 0 {
                return true;
            }
        }
        false
    }

    // ========================================================================
    // 终结器
    // ========================================================================
    pub fn register_finalizer(&mut self, callback: FinalizerFn) -> Option<u16> {
        if self.finalizer_count >= MAX_FINALIZERS {
            return None;
        }
        let idx = self.finalizer_count;
        self.finalizers[idx] = FinalizerEntry {
            callback: Some(callback),
            active: true,
        };
        self.finalizer_count += 1;
        Some(idx as u16)
    }

    pub(crate) fn queue_finalizer(&mut self, obj_addr: usize, finalizer_id: u16, obj_size: usize) {
        let idx = finalizer_id as usize;
        if idx >= self.finalizer_count
            || !self.finalizers[idx].active
            || self.pending_finalizer_count >= MAX_PENDING_FINALIZERS
        {
            return;
        }
        let callback = self.finalizers[idx].callback;
        if callback.is_none() {
            return;
        }
        self.pending_finalizers[self.pending_finalizer_count] = PendingFinalizer {
            callback,
            obj_addr,
            obj_size,
        };
        self.pending_finalizer_count += 1;
        self.stats.pending_finalizers = self.pending_finalizer_count;
    }

    pub(crate) fn drain_pending_finalizers(&mut self, out: &mut [PendingFinalizer]) -> usize {
        let count = self.pending_finalizer_count.min(out.len());
        for (dst, src) in out
            .iter_mut()
            .zip(self.pending_finalizers.iter())
            .take(count)
        {
            *dst = *src;
        }
        for idx in 0..count {
            self.pending_finalizers[idx] = PendingFinalizer::empty();
        }
        self.pending_finalizer_count = self.pending_finalizer_count.saturating_sub(count);
        self.stats.pending_finalizers = self.pending_finalizer_count;
        count
    }

    // ========================================================================
    // 内部公用函数
    // ========================================================================
    fn slot_can_recycle(&self, slot: GcHandleSlot) -> bool {
        !slot.active
            || (slot.object_addr == 0
                && slot.strong_refs == 0
                && slot.weak_refs == 0
                && slot.root_refs == 0
                && slot.pin_refs == 0)
    }

    fn recycle_handle_slot_if_dead(&mut self, slot_idx: usize) {
        if slot_idx < self.handle_slot_count && self.slot_can_recycle(self.handle_slots[slot_idx]) {
            self.handle_slots[slot_idx].active = false;
        }
    }

    pub(crate) fn remark_roots(&mut self) {
        self.phase = GcPhase::Remark;
        for i in 0..self.root_count {
            if self.roots[i].active
                && let Some(ptr) = self.resolve_root_ptr(self.roots[i])
            {
                self.mark_gray(ptr);
            }
        }
        while let Some(addr) = self.pop_mark_stack() {
            self.mark_black(addr);
        }
        self.phase = GcPhase::Remark;
    }

    pub(crate) fn retarget_forwarded_roots(&mut self) {
        for idx in 0..self.root_count {
            if !self.roots[idx].active || self.roots[idx].handle_slot != INVALID_HANDLE_SLOT {
                continue;
            }
            let ptr = self.roots[idx].ptr;
            if ptr == 0 {
                continue;
            }
            self.roots[idx].ptr = self.resolve_forwarding_addr(ptr);
        }
    }

    pub(crate) fn retarget_forwarded_references(&mut self) {
        for idx in 0..self.object_count {
            let entry = self.objects[idx];
            if !entry.active {
                continue;
            }
            let descriptor = self.trace_descriptor_for_entry(entry);
            if !descriptor.is_exact() {
                continue;
            }
            for &offset in descriptor.reference_offsets {
                if !descriptor.allows_ref_offset(entry.object_size, offset) {
                    continue;
                }
                let slot_addr = entry.object_addr + offset;
                let current = unsafe { *(slot_addr as *const usize) };
                if current == 0 {
                    continue;
                }
                let resolved = self.resolve_forwarding_addr(current);
                if resolved != current {
                    unsafe {
                        *(slot_addr as *mut usize) = resolved;
                    }
                }
            }
            for &offset in descriptor.weak_reference_offsets {
                if !descriptor.allows_weak_ref_offset(entry.object_size, offset) {
                    continue;
                }
                let slot_addr = entry.object_addr + offset;
                let current = unsafe { *(slot_addr as *const usize) };
                if current == 0 {
                    continue;
                }
                let resolved = self.resolve_forwarding_addr(current);
                if self.find_object_containing(resolved).is_some() {
                    unsafe {
                        *(slot_addr as *mut usize) = resolved;
                    }
                } else {
                    unsafe {
                        *(slot_addr as *mut usize) = 0;
                    }
                }
            }
        }
    }

    pub(crate) fn rebuild_remembered_set(&mut self, next_card_table: &mut [u8; CARD_TABLE_SIZE]) {
        *next_card_table = [0; CARD_TABLE_SIZE];
        self.clear_card_table();
        for idx in 0..self.object_count {
            let entry = self.objects[idx];
            if !entry.active || !self.is_in_old_gen(entry.object_addr) {
                continue;
            }
            if self.object_contains_young_ref(entry.object_addr) {
                self.mark_cards_for_range_in_table(
                    next_card_table,
                    entry.object_addr,
                    entry.object_size,
                );
                self.set_object_flag(entry.object_addr, GC_FLAG_REMEMBERED, true);
            } else {
                self.set_object_flag(entry.object_addr, GC_FLAG_REMEMBERED, false);
            }
        }
    }

    pub(crate) fn begin_minor_mark_phase(&mut self, next_card_table: &mut [u8; CARD_TABLE_SIZE]) {
        if !self.initialized {
            return;
        }
        self.running = true;
        self.stats.last_collection_kind = GcCollectionKind::Minor;
        self.stats.minor_gc_count = self.stats.minor_gc_count.saturating_add(1);
        self.mark_stack_top = 0;
        *next_card_table = [0; CARD_TABLE_SIZE];

        for i in 0..self.root_count {
            if !self.roots[i].active {
                continue;
            }
            let Some(ptr) = self.resolve_root_ptr(self.roots[i]) else {
                continue;
            };
            if self.is_in_young_gen(ptr) {
                self.mark_gray(ptr);
            } else if self.is_in_old_gen(ptr) {
                self.mark_young_refs_from_object(ptr);
                if let Some(idx) = self.find_object_containing(ptr) {
                    let entry = self.objects[idx];
                    if self.object_contains_young_ref(entry.object_addr) {
                        self.mark_cards_for_range_in_table(
                            next_card_table,
                            entry.object_addr,
                            entry.object_size,
                        );
                    }
                }
            }
        }

        for idx in 0..self.object_count {
            let entry = self.objects[idx];
            if !entry.active || !self.is_in_old_gen(entry.object_addr) {
                continue;
            }
            if !self.object_has_dirty_card(entry.object_addr, entry.object_size) {
                continue;
            }
            self.mark_young_refs_from_object(entry.object_addr);
            if self.object_contains_young_ref(entry.object_addr) {
                self.mark_cards_for_range_in_table(
                    next_card_table,
                    entry.object_addr,
                    entry.object_size,
                );
            }
        }

        while let Some(addr) = self.pop_mark_stack() {
            if self.is_in_young_gen(addr) {
                self.mark_black_young_only(addr);
            }
        }
        self.phase = GcPhase::Remark;
    }

    pub(crate) fn begin_major_mark_phase(&mut self) {
        if !self.initialized {
            return;
        }
        self.running = true;
        self.stats.last_collection_kind = GcCollectionKind::Major;
        self.stats.major_gc_count = self.stats.major_gc_count.saturating_add(1);
        self.clear_card_table();
        self.mark_stack_top = 0;
        let _ = self.now(); // mark time captured in mark_phase
        self.mark_phase();
        self.phase = GcPhase::Remark;
    }

    pub(crate) fn finish_collection_cycle(&mut self) {
        self.phase = GcPhase::Finalize;
        self.write_barrier_count = 0;
        self.running = false;
        self.phase = GcPhase::Idle;
    }

    // ========================================================================
    // 新生代 survivor 切换
    // ========================================================================
    pub fn switch_survivors(&mut self) {
        core::mem::swap(&mut self.survivor_from_start, &mut self.survivor_to_start);
        core::mem::swap(&mut self.survivor_from_end, &mut self.survivor_to_end);
    }

    // ========================================================================
    // 统计与控制
    // ========================================================================
    pub fn stats(&self) -> GcStats {
        let mut stats = self.stats;
        stats.object_table_entries = self.live_object_count;
        stats.object_table_capacity = MAX_OBJECTS;
        stats.pending_finalizers = self.pending_finalizer_count;
        stats.dirty_cards = self.card_table.iter().filter(|&&c| c != 0).count();
        stats.remembered_objects = self.count_remembered_objects();
        stats.strong_handle_slots = self.handle_slots[..self.handle_slot_count]
            .iter()
            .filter(|s| s.active && s.strong_refs > 0 && s.object_addr != 0)
            .count();
        stats.weak_handle_slots = self.handle_slots[..self.handle_slot_count]
            .iter()
            .filter(|s| s.active && s.weak_refs > 0)
            .count();
        stats.pinned_handle_slots = self.handle_slots[..self.handle_slot_count]
            .iter()
            .filter(|s| s.active && s.pin_refs > 0 && s.object_addr != 0)
            .count();
        stats
    }

    fn count_remembered_objects(&self) -> usize {
        let mut count = 0;
        for idx in 0..self.object_count {
            let entry = self.objects[idx];
            if !entry.active {
                continue;
            }
            let header = unsafe { *(entry.header_addr as *const GcObjectHeader) };
            if header.flags & GC_FLAG_REMEMBERED != 0 {
                count += 1;
            }
        }
        count
    }

    pub fn set_mode(&mut self, mode: GcMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> GcMode {
        self.mode
    }

    pub fn control_snapshot(&self, safepoint_requested: bool) -> GcControlSnapshot {
        GcControlSnapshot {
            mode: self.mode,
            phase: self.phase,
            running: self.running,
            safepoint_requested,
            last_collection_kind: self.stats.last_collection_kind,
        }
    }

    pub(crate) fn clear_roots(&mut self) {
        for i in 0..self.root_count {
            if self.roots[i].active && self.roots[i].handle_slot != INVALID_HANDLE_SLOT {
                self.release_root_slot_ref(self.roots[i].handle_slot as usize);
            }
            self.roots[i].active = false;
        }
        self.root_count = 0;
        self.stats.automatic_root_entries = 0;
        self.write_barrier_count = 0;
    }

    fn now(&self) -> u64 {
        self.timestamp_ns.map_or(0, |f| f())
    }
}
