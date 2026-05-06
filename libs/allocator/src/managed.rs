//! 受管堆与 GC 集成层 —— 高度成熟版
//!
//! 提供：
//! - 精确对象分配 (exact‑only)，支持自动 GC 重试
//! - 显式释放（强引用保护）
//! - 撤离式 minor / major GC，新生代半区复制 + evacuation failure 保护
//! - 碎片感知的老年代压缩决策
//! - 动态精确根注册与自动精确根提供者接口
//! - 增量/并发标记入口（safepoint 外可调用）
//! - 终结器、句柄、根帧管理
//! - 类型化字段访问 (GcRefSlot / GcWeakRefSlot)
//! - 丰富的可观测性

use core::alloc::Layout;
use core::ptr::write_bytes;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, compiler_fence};

use spin::mutex::Mutex;

use crate::error::{AllocationError, DeallocationError, ManagedHandleError};
use crate::gc::{
    self, CARD_TABLE_SIZE, EXACT_NO_REFERENCES_DESCRIPTOR, FinalizerFn, GC_FLAG_CARD_DIRTY,
    GC_FLAG_EVACUATING, GC_FLAG_FINALIZED, GC_FLAG_FORWARDED, GC_FLAG_HAS_FINALIZER,
    GC_FLAG_OLD_GEN, GC_FLAG_PINNED, GC_FLAG_REMEMBERED, GC_FLAG_WEAK_REF, GarbageCollector,
    GcCollectionKind, GcMode, GcObjectHeader, GcRef, GcRefSlot, GcRootFrame, GcRootHandle, GcStats,
    GcWeakRef, GcWeakRefSlot, MAX_PENDING_FINALIZERS, PROMOTION_THRESHOLD, PendingFinalizer,
    RootType,
};
use crate::request::{
    AllocationArena, AllocationKind, AllocationRecord, ManagedAllocFlags, MemoryDomain, Zeroing,
};
use crate::space::KernelAddressSpace;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 自动启用 managed heap 时使用的轻量 bootstrap 容量。
pub const DEFAULT_MANAGED_HEAP_ORDER: usize = 9; // 2 MiB
/// 推荐的大 managed heap 配置。
pub const LARGE_MANAGED_HEAP_ORDER: usize = 14; // 64 MiB

/// 动态 exact root 槽位上限。
const MAX_DYNAMIC_EXACT_ROOT_SLOTS: usize = 128;

/// 自动精确根提供者：返回一组 (指针, 根类型, 来源 ID) 的切片
pub type ExactRootProviderFn = fn() -> &'static [(usize, RootType, usize)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExactRootSlotEntry {
    slot_addr: usize,
    root_type: RootType,
    active: bool,
}

impl ExactRootSlotEntry {
    const fn empty() -> Self {
        Self {
            slot_addr: 0,
            root_type: RootType::Global,
            active: false,
        }
    }
}

struct ExactRootRegistry {
    slots: [ExactRootSlotEntry; MAX_DYNAMIC_EXACT_ROOT_SLOTS],
}

impl ExactRootRegistry {
    const fn new() -> Self {
        Self {
            slots: [ExactRootSlotEntry::empty(); MAX_DYNAMIC_EXACT_ROOT_SLOTS],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedFailurePolicy {
    PanicOnFailure,
    ReturnError,
}

#[derive(Clone, Copy, Debug)]
pub struct ManagedHeapConfig {
    pub order: usize,
    pub mode: GcMode,
    pub failure_policy: ManagedFailurePolicy,
    pub external_free_callback: Option<fn(ptr: usize, size: usize)>,
    pub timestamp_ns: Option<fn() -> u64>,
}

impl ManagedHeapConfig {
    pub const fn default_kernel() -> Self {
        Self {
            order: DEFAULT_MANAGED_HEAP_ORDER,
            mode: GcMode::MarkSweep,
            failure_policy: ManagedFailurePolicy::PanicOnFailure,
            external_free_callback: None,
            timestamp_ns: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ManagedStats {
    pub enabled: bool,
    pub heap_start: usize,
    pub heap_size: usize,
    pub alloc_requests: u64,
    pub free_requests: u64,
    pub active_objects: u64,
    pub active_bytes: usize,
    pub alloc_failures: u64,
    pub gc: GcStats,
    pub gc_control: Option<gc::GcControlSnapshot>,
}

// ---------------------------------------------------------------------------
// ManagedAllocator 主结构体
// ---------------------------------------------------------------------------

pub struct ManagedAllocator {
    pub gc: Mutex<GarbageCollector>,
    enabled: AtomicBool,
    heap_start: AtomicUsize,
    heap_size: AtomicUsize,
    alloc_requests: AtomicU64,
    free_requests: AtomicU64,
    active_objects: AtomicU64,
    active_bytes: AtomicUsize,
    alloc_failures: AtomicU64,
    external_free_callback: AtomicUsize,
    relocation_observer: AtomicUsize,
    vmem_ptr: AtomicUsize,
    exact_root_provider: AtomicUsize,
    exact_root_registry: Mutex<ExactRootRegistry>,
    gc_enter_critical: AtomicUsize,
    gc_leave_critical: AtomicUsize,
    gc_safepoint_requested: AtomicBool,
}

impl ManagedAllocator {
    pub const fn new() -> Self {
        Self {
            gc: Mutex::new(GarbageCollector::new()),
            enabled: AtomicBool::new(false),
            heap_start: AtomicUsize::new(0),
            heap_size: AtomicUsize::new(0),
            alloc_requests: AtomicU64::new(0),
            free_requests: AtomicU64::new(0),
            active_objects: AtomicU64::new(0),
            active_bytes: AtomicUsize::new(0),
            alloc_failures: AtomicU64::new(0),
            external_free_callback: AtomicUsize::new(0),
            relocation_observer: AtomicUsize::new(0),
            vmem_ptr: AtomicUsize::new(0),
            exact_root_provider: AtomicUsize::new(0),
            exact_root_registry: Mutex::new(ExactRootRegistry::new()),
            gc_enter_critical: AtomicUsize::new(0),
            gc_leave_critical: AtomicUsize::new(0),
            gc_safepoint_requested: AtomicBool::new(false),
        }
    }

    // ========================================================================
    // 初始化
    // ========================================================================
    pub fn init(
        &self,
        heap_start: usize,
        heap_size: usize,
        mode: GcMode,
        vmem: *const KernelAddressSpace,
        reclaim_callback: fn(ptr: usize, size: usize),
        external_free_callback: Option<fn(ptr: usize, size: usize)>,
        timestamp_ns: Option<fn() -> u64>,
    ) {
        self.heap_start.store(heap_start, Ordering::Release);
        self.heap_size.store(heap_size, Ordering::Release);
        self.alloc_requests.store(0, Ordering::Release);
        self.free_requests.store(0, Ordering::Release);
        self.active_objects.store(0, Ordering::Release);
        self.active_bytes.store(0, Ordering::Release);
        self.alloc_failures.store(0, Ordering::Release);
        self.external_free_callback.store(
            external_free_callback.map_or(0, |f| f as usize),
            Ordering::Release,
        );
        self.vmem_ptr.store(vmem as usize, Ordering::Release);

        let mut gc = self.gc.lock();
        gc.init(heap_start, heap_size, mode, reclaim_callback, timestamp_ns);
        self.enabled.store(true, Ordering::Release);
    }

    pub fn extend_heap_contiguous(&self, added_base: usize, added_size: usize) -> bool {
        if added_size == 0 || !self.is_enabled() {
            return false;
        }
        let heap_start = self.heap_start.load(Ordering::Acquire);
        let heap_size = self.heap_size.load(Ordering::Acquire);
        let expected_base = heap_start.saturating_add(heap_size);
        if added_base != expected_base {
            return false;
        }

        let mut gc = self.gc.lock();
        if gc.heap_end != expected_base {
            return false;
        }
        gc.heap_end = gc.heap_end.saturating_add(added_size);
        self.heap_size
            .store(heap_size.saturating_add(added_size), Ordering::Release);
        true
    }

    // ========================================================================
    // 自动精确根
    // ========================================================================
    pub fn register_exact_root_slot(
        &self,
        slot: &'static AtomicUsize,
        root_type: RootType,
    ) -> bool {
        let slot_addr = slot as *const AtomicUsize as usize;
        let mut registry = self.exact_root_registry.lock();

        for entry in &registry.slots {
            if entry.active && entry.slot_addr == slot_addr {
                return false;
            }
        }

        for entry in &mut registry.slots {
            if !entry.active {
                *entry = ExactRootSlotEntry {
                    slot_addr,
                    root_type,
                    active: true,
                };
                return true;
            }
        }

        false
    }

    pub fn unregister_exact_root_slot(&self, slot: &'static AtomicUsize) -> bool {
        let slot_addr = slot as *const AtomicUsize as usize;
        let mut registry = self.exact_root_registry.lock();

        for entry in &mut registry.slots {
            if entry.active && entry.slot_addr == slot_addr {
                *entry = ExactRootSlotEntry::empty();
                return true;
            }
        }

        false
    }

    pub fn register_exact_root_provider(&self, provider: ExactRootProviderFn) {
        self.exact_root_provider
            .store(provider as usize, Ordering::Release);
    }

    fn collect_registered_exact_root_slots(&self, gc: &mut GarbageCollector) {
        let registry = self.exact_root_registry.lock();

        for entry in &registry.slots {
            if !entry.active {
                continue;
            }
            let slot = entry.slot_addr as *const AtomicUsize;
            let ptr = unsafe { (*slot).load(Ordering::Acquire) };
            if ptr == 0 {
                continue;
            }
            gc.add_automatic_root(ptr, entry.root_type, entry.slot_addr);
        }
    }

    fn collect_automatic_roots(&self, gc: &mut GarbageCollector) {
        gc.clear_automatic_roots();
        self.collect_registered_exact_root_slots(gc);
        let raw = self.exact_root_provider.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let provider: ExactRootProviderFn = unsafe { core::mem::transmute(raw) };
        for &(ptr, root_type, source_id) in provider() {
            gc.add_automatic_root(ptr, root_type, source_id);
        }
    }

    // ========================================================================
    // 分配
    // ========================================================================
    pub fn alloc(
        &self,
        layout: Layout,
        vmem: &KernelAddressSpace,
        flags: ManagedAllocFlags,
        zeroing: Zeroing,
    ) -> Result<AllocationRecord, AllocationError> {
        self.alloc_internal(layout, vmem, flags, zeroing, true)
    }

    fn alloc_internal(
        &self,
        layout: Layout,
        vmem: &KernelAddressSpace,
        flags: ManagedAllocFlags,
        zeroing: Zeroing,
        count_request: bool,
    ) -> Result<AllocationRecord, AllocationError> {
        self.alloc_internal_in_range(layout, vmem, flags, zeroing, count_request, 0, 0)
    }

    fn alloc_internal_in_range(
        &self,
        layout: Layout,
        vmem: &KernelAddressSpace,
        flags: ManagedAllocFlags,
        zeroing: Zeroing,
        count_request: bool,
        range_start: usize,
        range_end: usize,
    ) -> Result<AllocationRecord, AllocationError> {
        if count_request {
            self.alloc_requests.fetch_add(1, Ordering::Relaxed);
        }
        let aligned = layout.pad_to_align();
        log::debug!(
            "[alloc][managed] request size={} align={} flags={:?} zeroing={:?}",
            aligned.size(),
            aligned.align(),
            flags,
            zeroing,
        );

        if !self.is_enabled() {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::NotInitialized);
        }

        let object_size = aligned.size().max(1);
        let object_align = layout.align().max(1);
        let trace_descriptor = flags
            .trace_descriptor
            .unwrap_or(&EXACT_NO_REFERENCES_DESCRIPTOR);
        if object_size > u32::MAX as usize || object_align > u16::MAX as usize {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::InvalidLayout);
        }
        if !trace_descriptor.is_exact() {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::InvalidLayout);
        }
        if !trace_descriptor.matches_layout(object_size, object_align) {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::InvalidLayout);
        }

        let reserve_size = GcObjectHeader::HEADER_SIZE
            .saturating_add(object_size)
            .saturating_add(object_align.saturating_sub(1));
        if reserve_size > u32::MAX as usize {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::InvalidLayout);
        }

        // 最多重试 2 次：分配失败时自动触发 GC 后再试
        const MAX_RETRIES: u32 = 2;
        let mut last_err = AllocationError::OutOfMemory;
        for _retry in 0..=MAX_RETRIES {
            let raw_base = match if range_start < range_end {
                vmem.alloc_managed_range_in(range_start, range_end, reserve_size, object_align)
            } else {
                vmem.alloc_managed_range(reserve_size, object_align)
            } {
                Ok(addr) => addr,
                Err(err) => {
                    last_err = AllocationError::AddressSpace(err);
                    if !count_request {
                        return Err(last_err);
                    }
                    // 触发一次 GC 后继续重试
                    self.collect_with_policy(1);
                    continue;
                }
            };

            let object_addr = align_up(raw_base + GcObjectHeader::HEADER_SIZE, object_align);

            if object_addr < GcObjectHeader::HEADER_SIZE {
                let _ = vmem.free_managed_range(raw_base, reserve_size);
                self.alloc_failures.fetch_add(1, Ordering::Relaxed);
                return Err(AllocationError::InvalidLayout);
            }

            let header_addr = object_addr - GcObjectHeader::HEADER_SIZE;
            let prefix = header_addr.saturating_sub(raw_base);
            if prefix > u16::MAX as usize {
                let _ = vmem.free_managed_range(raw_base, reserve_size);
                self.alloc_failures.fetch_add(1, Ordering::Relaxed);
                return Err(AllocationError::InvalidLayout);
            }

            // 清零保留区
            unsafe {
                write_bytes(raw_base as *mut u8, 0, reserve_size);
            }
            compiler_fence(Ordering::SeqCst);

            let mut header = GcObjectHeader::new(object_size as u32);
            if flags.pinned {
                header.flags |= GC_FLAG_PINNED;
            }
            if let Some(finalizer_id) = flags.finalizer_id {
                header.flags |= GC_FLAG_HAS_FINALIZER;
                header.finalizer_id = finalizer_id;
            }
            header.prefix_bytes = prefix as u16;
            header.set_trace_descriptor(trace_descriptor);
            encode_reserve_size(&mut header, reserve_size);

            unsafe {
                (header_addr as *mut GcObjectHeader).write(header);
                if matches!(zeroing, Zeroing::Zeroed) {
                    write_bytes(object_addr as *mut u8, 0, object_size);
                }
            }

            if !self.gc.lock().register_object(
                header_addr,
                object_addr,
                raw_base,
                reserve_size,
                object_size,
                object_align,
            ) {
                let mut gc = self.gc.lock();
                if let Some(idx) = gc.find_object_by_object_addr(object_addr) {
                    gc.deactivate_object(idx);
                }
                drop(gc);

                unsafe {
                    write_bytes(raw_base as *mut u8, 0, reserve_size);
                }
                let _ = vmem.free_managed_range(raw_base, reserve_size);
                if !count_request {
                    return Err(AllocationError::OutOfMemory);
                }
                self.collect_with_policy(2); // 中度压力，可能触发 major
                continue;
            }

            self.active_objects.fetch_add(1, Ordering::Relaxed);
            self.active_bytes.fetch_add(object_size, Ordering::Relaxed);

            let record =
                AllocationRecord::new(AllocationKind::Managed, MemoryDomain::Managed, object_addr)
                    .with_arena(AllocationArena::Managed)
                    .with_sizes(object_size, object_size, object_align);
            log::debug!(
                "[alloc][managed] success raw_base={:#x} header={:#x} object={:#x} size={} reserve_size={} align={}",
                raw_base,
                header_addr,
                object_addr,
                object_size,
                reserve_size,
                object_align,
            );
            return Ok(record);
        }

        self.alloc_failures.fetch_add(1, Ordering::Relaxed);
        Err(last_err)
    }

    // ========================================================================
    // 显式释放 (带强引用保护)
    // ========================================================================
    pub fn free(&self, ptr: usize, vmem: &KernelAddressSpace) -> Result<(), DeallocationError> {
        let Some(allocation) = self.read_allocation(ptr) else {
            return Err(DeallocationError::UnknownPointer);
        };
        log::debug!(
            "[alloc][managed] free ptr={:#x} raw_base={:#x} reserve_size={} object_size={}",
            ptr,
            allocation.raw_base,
            allocation.reserve_size,
            allocation.object_size,
        );

        {
            let mut gc = self.gc.lock();
            for idx in 0..gc.handle_slot_count {
                let slot = gc.handle_slots[idx];
                if slot.active && slot.strong_refs > 0 && slot.object_addr == allocation.object_addr
                {
                    return Err(DeallocationError::ObjectStillReferenced);
                }
            }
            // 检查是否有活跃根引用
            for idx in 0..gc.root_count {
                let root = gc.roots[idx];
                if !root.active {
                    continue;
                }
                let root_ptr = if root.handle_slot != u16::MAX {
                    let slot_idx = root.handle_slot as usize;
                    if slot_idx < gc.handle_slot_count
                        && gc.handle_slots[slot_idx].active
                        && gc.handle_slots[slot_idx].generation == root.handle_generation
                    {
                        gc.handle_slots[slot_idx].object_addr
                    } else {
                        0
                    }
                } else {
                    root.ptr
                };
                if root_ptr != 0 && root_ptr == allocation.object_addr {
                    return Err(DeallocationError::ObjectStillReferenced);
                }
            }

            if object_has_strong_managed_reference_to(&gc, allocation.object_addr) {
                return Err(DeallocationError::ObjectStillReferenced);
            }

            if !gc.unregister_object(allocation.object_addr) {
                return Err(DeallocationError::UnknownPointer);
            }
        }

        self.reclaim_allocation(allocation, vmem);
        Ok(())
    }

    // ========================================================================
    // 复活对象 (用于终结器)
    // ========================================================================
    pub fn revive_object(&self, ptr: usize) -> bool {
        if !self.is_enabled() {
            return false;
        }
        self.gc.lock().revive_object(ptr)
    }

    // ========================================================================
    // GC 收集接口
    // ========================================================================
    pub fn collect_minor(&self) {
        self.collect_explicit_kind(GcCollectionKind::Minor);
    }

    pub fn collect_major(&self) {
        self.collect_explicit_kind(GcCollectionKind::Major);
    }

    pub fn collect_explicit_on_pressure(&self, pressure_level: u8) {
        self.collect_with_policy(pressure_level);
    }

    pub fn collect_on_pressure(&self, pressure_level: u8) {
        self.collect_with_policy(pressure_level);
    }

    pub fn collect_explicit_kind(&self, kind: GcCollectionKind) {
        self.collect_kind_internal(kind);
    }

    fn collect_with_policy(&self, pressure_level: u8) {
        match pressure_level {
            0 => {}
            1 => self.collect_kind_internal(GcCollectionKind::Minor),
            2 => {
                let before = self.stats().gc.bytes_reclaimed;
                self.collect_kind_internal(GcCollectionKind::Minor);
                if self.stats().gc.bytes_reclaimed == before {
                    self.collect_kind_internal(GcCollectionKind::Major);
                }
            }
            _ => self.collect_kind_internal(GcCollectionKind::Major),
        }
    }

    fn collect_kind_internal(&self, kind: GcCollectionKind) {
        if !self.is_enabled() {
            return;
        }
        let Some(vmem) = self.load_vmem() else {
            return;
        };
        let mut pending = [PendingFinalizer::empty(); MAX_PENDING_FINALIZERS];
        let start_time = self.gc_timestamp_now();
        let critical_state = self.enter_gc_safepoint();

        // 在安全点内收集自动精确根
        {
            let mut gc = self.gc.lock();
            self.collect_automatic_roots(&mut gc);
        }

        let pending_count = match kind {
            GcCollectionKind::Minor => self.collect_minor_evacuation(vmem, &mut pending),
            GcCollectionKind::Major => {
                // 碎片感知决策
                let mode = {
                    let gc = self.gc.lock();
                    if gc.stats.fragmentation_ratio > 300 {
                        GcMode::MarkCompact
                    } else {
                        GcMode::MarkSweep
                    }
                };
                match mode {
                    GcMode::MarkSweep => {
                        let mut gc = self.gc.lock();
                        gc.begin_major_mark_phase();
                        gc.remark_roots();
                        gc.sweep_phase();
                        gc.finish_collection_cycle();
                        let count = gc.drain_pending_finalizers(&mut pending);
                        gc.stats.finalizers_run += count as u64;
                        count
                    }
                    GcMode::MarkCompact => self.collect_major_compaction(vmem, &mut pending),
                }
            }
            _ => 0,
        };
        self.update_gc_pause_stats(start_time);
        self.leave_gc_safepoint(critical_state);
        self.run_drained_finalizers(&mut pending, pending_count);
    }

    fn collect_minor_evacuation(
        &self,
        vmem: &KernelAddressSpace,
        pending: &mut [PendingFinalizer; MAX_PENDING_FINALIZERS],
    ) -> usize {
        let mut next_card_table = [0u8; CARD_TABLE_SIZE];
        {
            let mut gc = self.gc.lock();
            gc.begin_minor_mark_phase(&mut next_card_table);
            self.mark_relocation_candidates(&mut gc, CollectionScope::YoungOnly);
        }
        self.evacuate_candidates(vmem, CollectionScope::YoungOnly);
        {
            let mut gc = self.gc.lock();
            gc.retarget_forwarded_roots();
            gc.retarget_forwarded_references();
        }
        self.cleanup_collection_scope(vmem, CollectionScope::YoungOnly);
        let mut gc = self.gc.lock();
        self.reset_live_objects_after_collection(&mut gc, CollectionScope::YoungOnly);
        gc.rebuild_remembered_set(&mut next_card_table);
        gc.card_table = next_card_table;
        // 切换 survivor 空间
        gc.switch_survivors();
        gc.finish_collection_cycle();
        let count = gc.drain_pending_finalizers(pending);
        gc.stats.finalizers_run += count as u64;
        count
    }

    fn collect_major_compaction(
        &self,
        vmem: &KernelAddressSpace,
        pending: &mut [PendingFinalizer; MAX_PENDING_FINALIZERS],
    ) -> usize {
        let mut next_card_table = [0u8; CARD_TABLE_SIZE];
        {
            let mut gc = self.gc.lock();
            gc.begin_major_mark_phase();
            gc.remark_roots();
            gc.phase = gc::GcPhase::Compact;
            self.mark_relocation_candidates(&mut gc, CollectionScope::FullHeap);
        }
        self.evacuate_candidates(vmem, CollectionScope::FullHeap);
        {
            let mut gc = self.gc.lock();
            gc.retarget_forwarded_roots();
            gc.retarget_forwarded_references();
        }
        self.cleanup_collection_scope(vmem, CollectionScope::FullHeap);
        let mut gc = self.gc.lock();
        self.reset_live_objects_after_collection(&mut gc, CollectionScope::FullHeap);
        gc.rebuild_remembered_set(&mut next_card_table);
        gc.card_table = next_card_table;
        gc.finish_collection_cycle();
        let count = gc.drain_pending_finalizers(pending);
        gc.stats.finalizers_run += count as u64;
        count
    }

    // ---- 撤离与清理辅助 ----
    fn mark_relocation_candidates(&self, gc: &mut GarbageCollector, scope: CollectionScope) {
        for idx in 0..gc.object_count {
            let entry = gc.objects[idx];
            if !entry.active {
                continue;
            }
            let header_addr = entry.header_addr;
            let mut header = unsafe { *(header_addr as *const GcObjectHeader) };
            header.flags &= !GC_FLAG_EVACUATING;
            if header.color != gc::GcColor::White as u8 && scope.contains(header.flags) {
                header.flags |= GC_FLAG_EVACUATING;
            }
            unsafe {
                *(header_addr as *mut GcObjectHeader) = header;
            }
        }
    }

    fn next_relocation_candidate(
        &self,
        gc: &GarbageCollector,
        scope: CollectionScope,
    ) -> Option<(usize, GcObjectHeader)> {
        for idx in 0..gc.object_count {
            let entry = gc.objects[idx];
            if !entry.active {
                continue;
            }
            let header = unsafe { *(entry.header_addr as *const GcObjectHeader) };
            if !scope.contains(header.flags) {
                continue;
            }
            if header.flags & GC_FLAG_EVACUATING == 0 || header.flags & GC_FLAG_FORWARDED != 0 {
                continue;
            }
            return Some((idx, header));
        }
        None
    }

    fn rewrite_relocated_header(
        &self,
        gc: &mut GarbageCollector,
        source_header: GcObjectHeader,
        target_ptr: usize,
        promote_to_old: bool,
    ) {
        let Some(target_idx) = gc.find_object_by_object_addr(target_ptr) else {
            return;
        };
        let target_entry = gc.objects[target_idx];
        let header_addr = target_entry.header_addr;
        let mut target_header = unsafe { *(header_addr as *const GcObjectHeader) };
        let mut flags =
            source_header.flags & (GC_FLAG_HAS_FINALIZER | GC_FLAG_FINALIZED | GC_FLAG_WEAK_REF);
        if promote_to_old {
            flags |= GC_FLAG_OLD_GEN;
        }
        flags &=
            !(GC_FLAG_CARD_DIRTY | GC_FLAG_REMEMBERED | GC_FLAG_FORWARDED | GC_FLAG_EVACUATING);
        target_header.size = source_header.size;
        target_header.color = source_header.color;
        target_header.generation = source_header.generation;
        target_header.flags = flags;
        target_header.finalizer_id = source_header.finalizer_id;
        target_header.set_trace_descriptor(source_header.trace_descriptor());
        target_header.set_forwarding(0);
        unsafe {
            *(header_addr as *mut GcObjectHeader) = target_header;
        }
        if promote_to_old {
            gc.stats.young_gen_objects = gc.stats.young_gen_objects.saturating_sub(1);
            gc.stats.old_gen_objects = gc.stats.old_gen_objects.saturating_add(1);
        }
    }

    fn relocation_target_range(
        &self,
        gc: &GarbageCollector,
        header: GcObjectHeader,
        scope: CollectionScope,
    ) -> Option<(usize, usize, bool)> {
        let promote_to_old = header.flags & GC_FLAG_OLD_GEN != 0
            || header.generation.saturating_add(1) >= PROMOTION_THRESHOLD;
        let range = if promote_to_old {
            (gc.young_gen_end, gc.heap_end)
        } else {
            (gc.survivor_to_start, gc.survivor_to_end)
        };
        if range.0 >= range.1 {
            return None;
        }
        match scope {
            CollectionScope::YoungOnly | CollectionScope::FullHeap => {
                Some((range.0, range.1, promote_to_old))
            }
        }
    }

    fn evacuate_candidates(&self, vmem: &KernelAddressSpace, scope: CollectionScope) {
        loop {
            let candidate = {
                let gc = self.gc.lock();
                self.next_relocation_candidate(&gc, scope)
            };
            let Some((idx, header)) = candidate else {
                break;
            };

            // 跳过被 pin 的对象（evacuation failure 的一种）
            if header.flags & GC_FLAG_PINNED != 0 {
                let mut gc = self.gc.lock();
                if idx < gc.object_count && gc.objects[idx].active {
                    let header_addr = gc.objects[idx].header_addr;
                    let mut updated = unsafe { *(header_addr as *const GcObjectHeader) };
                    updated.flags &= !GC_FLAG_EVACUATING;
                    unsafe {
                        *(header_addr as *mut GcObjectHeader) = updated;
                    }
                    gc.stats.evacuation_failures += 1;
                }
                continue;
            }

            let source_entry = {
                let gc = self.gc.lock();
                if idx >= gc.object_count || !gc.objects[idx].active {
                    continue;
                }
                gc.objects[idx]
            };
            let layout = match Layout::from_size_align(
                source_entry.object_size,
                source_entry.object_align,
            ) {
                Ok(layout) => layout,
                Err(_) => {
                    let mut gc = self.gc.lock();
                    if idx < gc.object_count && gc.objects[idx].active {
                        let header_addr = gc.objects[idx].header_addr;
                        let mut updated = unsafe { *(header_addr as *const GcObjectHeader) };
                        updated.flags &= !GC_FLAG_EVACUATING;
                        unsafe {
                            *(header_addr as *mut GcObjectHeader) = updated;
                        }
                        gc.stats.evacuation_failures += 1;
                    }
                    continue;
                }
            };
            let mut flags =
                ManagedAllocFlags::new().with_trace_descriptor(header.trace_descriptor());
            if header.flags & GC_FLAG_HAS_FINALIZER != 0 {
                flags = flags.with_finalizer(header.finalizer_id);
            }
            let res = {
                let gc = self.gc.lock();
                self.relocation_target_range(&gc, header, scope)
            };
            let (range_start, range_end, mut promote_to_old) = match res {
                Some(range) => range,
                None => {
                    let mut gc = self.gc.lock();
                    if idx < gc.object_count && gc.objects[idx].active {
                        let header_addr = gc.objects[idx].header_addr;
                        let mut updated = unsafe { *(header_addr as *const GcObjectHeader) };
                        updated.flags &= !GC_FLAG_EVACUATING;
                        unsafe {
                            *(header_addr as *mut GcObjectHeader) = updated;
                        }
                        gc.stats.evacuation_failures += 1;
                    }
                    continue;
                }
            };
            let target = match self.alloc_internal_in_range(
                layout,
                vmem,
                flags,
                Zeroing::Uninitialized,
                false,
                range_start,
                range_end,
            ) {
                Ok(record) => record,
                Err(_) if !promote_to_old => {
                    let (old_start, old_end) = {
                        let gc = self.gc.lock();
                        (gc.young_gen_end, gc.heap_end)
                    };
                    match self.alloc_internal_in_range(
                        layout,
                        vmem,
                        flags,
                        Zeroing::Uninitialized,
                        false,
                        old_start,
                        old_end,
                    ) {
                        Ok(record) => {
                            promote_to_old = true;
                            record
                        }
                        Err(_) => {
                            let mut gc = self.gc.lock();
                            if idx < gc.object_count && gc.objects[idx].active {
                                let header_addr = gc.objects[idx].header_addr;
                                let mut updated =
                                    unsafe { *(header_addr as *const GcObjectHeader) };
                                updated.flags &= !GC_FLAG_EVACUATING;
                                unsafe {
                                    *(header_addr as *mut GcObjectHeader) = updated;
                                }
                                gc.stats.evacuation_failures += 1;
                            }
                            continue;
                        }
                    }
                }
                Err(_) => {
                    let mut gc = self.gc.lock();
                    if idx < gc.object_count && gc.objects[idx].active {
                        let header_addr = gc.objects[idx].header_addr;
                        let mut updated = unsafe { *(header_addr as *const GcObjectHeader) };
                        updated.flags &= !GC_FLAG_EVACUATING;
                        unsafe {
                            *(header_addr as *mut GcObjectHeader) = updated;
                        }
                        gc.stats.evacuation_failures += 1;
                    }
                    continue;
                }
            };

            unsafe {
                core::ptr::copy_nonoverlapping(
                    source_entry.object_addr as *const u8,
                    target.ptr as *mut u8,
                    source_entry.object_size,
                );
            }

            let source_still_valid = {
                let gc = self.gc.lock();
                idx < gc.object_count
                    && gc.objects[idx].active
                    && gc.objects[idx].object_addr == source_entry.object_addr
            };
            if !source_still_valid {
                if let Some(allocation) = self.read_allocation(target.ptr) {
                    self.reclaim_relocated_allocation(allocation, vmem);
                }
                continue;
            }

            if !self.observe_relocation(source_entry.object_addr, target) {
                if let Some(allocation) = self.read_allocation(target.ptr) {
                    self.reclaim_relocated_allocation(allocation, vmem);
                }
                let mut gc = self.gc.lock();
                if idx < gc.object_count
                    && gc.objects[idx].active
                    && gc.objects[idx].object_addr == source_entry.object_addr
                {
                    let header_addr = gc.objects[idx].header_addr;
                    let mut updated = unsafe { *(header_addr as *const GcObjectHeader) };
                    updated.flags &= !GC_FLAG_EVACUATING;
                    unsafe {
                        *(header_addr as *mut GcObjectHeader) = updated;
                    }
                    gc.stats.evacuation_failures += 1;
                }
                continue;
            }

            {
                let mut gc = self.gc.lock();
                if idx >= gc.object_count
                    || !gc.objects[idx].active
                    || gc.objects[idx].object_addr != source_entry.object_addr
                {
                    panic!(
                        "[alloc][managed][invariant] relocation source changed after registry retarget old={:#x} new={:#x}",
                        source_entry.object_addr, target.ptr
                    );
                }
                gc.stats.relocated_bytes += source_entry.object_size as u64;
                self.rewrite_relocated_header(&mut gc, header, target.ptr, promote_to_old);
                if !gc.install_forwarding(source_entry.object_addr, target.ptr) {
                    panic!(
                        "[alloc][managed][invariant] relocation forwarding failed after registry retarget old={:#x} new={:#x}",
                        source_entry.object_addr, target.ptr
                    );
                }
                gc.stats.objects_compacted = gc.stats.objects_compacted.saturating_add(1);
                // 统计 survivor/promoted 字节
                if scope == CollectionScope::YoungOnly {
                    if promote_to_old {
                        gc.stats.promoted_bytes += source_entry.object_size as u64;
                    } else {
                        gc.stats.survivor_bytes += source_entry.object_size as u64;
                    }
                }
            }
        }
    }

    fn next_cleanup_action(
        &self,
        gc: &mut GarbageCollector,
        scope: CollectionScope,
    ) -> Option<CleanupAction> {
        for idx in 0..gc.object_count {
            let entry = gc.objects[idx];
            if !entry.active {
                continue;
            }
            let header = unsafe { *(entry.header_addr as *const GcObjectHeader) };
            let touched = scope.contains(header.flags)
                || (scope == CollectionScope::YoungOnly
                    && header.color != gc::GcColor::White as u8);
            if !touched {
                continue;
            }
            if header.flags & GC_FLAG_FORWARDED != 0 {
                gc.deactivate_object(idx);
                return Some(CleanupAction::Moved(self.allocation_from_entry(entry)));
            }
            if header.color != gc::GcColor::White as u8 {
                continue;
            }
            if header.flags & GC_FLAG_HAS_FINALIZER != 0 && header.flags & GC_FLAG_FINALIZED == 0 {
                gc.queue_finalizer(entry.object_addr, header.finalizer_id, entry.object_size);
                let mut updated = header;
                updated.flags |= GC_FLAG_FINALIZED;
                unsafe {
                    *(entry.header_addr as *mut GcObjectHeader) = updated;
                }
                continue;
            }
            let callback = gc.free_callback;
            gc.deactivate_object(idx);
            gc.stats.objects_swept = gc.stats.objects_swept.saturating_add(1);
            gc.stats.bytes_reclaimed = gc
                .stats
                .bytes_reclaimed
                .saturating_add(entry.reserve_size as u64);
            return Some(CleanupAction::Dead {
                allocation: self.allocation_from_entry(entry),
                reclaim_callback: callback,
            });
        }
        None
    }

    fn cleanup_collection_scope(&self, vmem: &KernelAddressSpace, scope: CollectionScope) {
        loop {
            let action = {
                let mut gc = self.gc.lock();
                gc.phase = gc::GcPhase::Sweep;
                self.next_cleanup_action(&mut gc, scope)
            };
            match action {
                Some(CleanupAction::Moved(allocation)) => {
                    self.reclaim_relocated_allocation(allocation, vmem);
                }
                Some(CleanupAction::Dead {
                    allocation,
                    reclaim_callback,
                }) => {
                    if let Some(callback) = reclaim_callback {
                        callback(allocation.header_addr, allocation.reserve_size);
                    } else {
                        self.reclaim_allocation(allocation, vmem);
                    }
                }
                None => break,
            }
        }
    }

    fn reset_live_objects_after_collection(
        &self,
        gc: &mut GarbageCollector,
        scope: CollectionScope,
    ) {
        for idx in 0..gc.object_count {
            let entry = gc.objects[idx];
            if !entry.active {
                continue;
            }
            let header_addr = entry.header_addr;
            let mut header = unsafe { *(header_addr as *const GcObjectHeader) };
            if !scope.contains(header.flags) {
                continue;
            }
            header.set_forwarding(0);
            header.flags &= !(GC_FLAG_EVACUATING | GC_FLAG_CARD_DIRTY | GC_FLAG_REMEMBERED);
            if header.color != gc::GcColor::White as u8 {
                header.color = gc::GcColor::White as u8;
                header.generation = header.generation.saturating_add(1);
                if header.generation >= PROMOTION_THRESHOLD && header.flags & GC_FLAG_OLD_GEN == 0 {
                    header.flags |= GC_FLAG_OLD_GEN;
                    gc.account_promotion();
                }
            }
            unsafe {
                *(header_addr as *mut GcObjectHeader) = header;
            }
        }
    }

    /// 从 GC 回收一个对象（由外部 free callback 唤起）
    pub fn reclaim_from_gc(&self, header_addr: usize, vmem: &KernelAddressSpace) -> Option<usize> {
        let allocation = self.read_allocation_from_header(header_addr)?;
        let ptr = allocation.object_addr;
        self.reclaim_allocation(allocation, vmem);
        Some(ptr)
    }

    // ========================================================================
    // 句柄与根管理
    // ========================================================================
    pub fn create_handle(&self, ptr: usize) -> Option<crate::GcHandle> {
        if !self.is_enabled() {
            return None;
        }
        self.gc.lock().create_handle(ptr)
    }

    pub fn retain_handle(&self, handle: &crate::GcHandle) -> Option<crate::GcHandle> {
        if !self.is_enabled() {
            return None;
        }
        self.gc.lock().retain_handle(handle)
    }

    pub fn release_handle(&self, handle: crate::GcHandle) {
        if !self.is_enabled() {
            return;
        }
        self.gc.lock().release_handle(handle);
    }

    pub fn downgrade_handle(&self, handle: &crate::GcHandle) -> Option<crate::GcWeakHandle> {
        if !self.is_enabled() {
            return None;
        }
        self.gc.lock().downgrade_handle(handle)
    }

    pub fn upgrade_weak_handle(&self, handle: &crate::GcWeakHandle) -> Option<crate::GcHandle> {
        if !self.is_enabled() {
            return None;
        }
        self.gc.lock().upgrade_weak_handle(handle)
    }

    pub fn release_weak_handle(&self, handle: crate::GcWeakHandle) {
        if !self.is_enabled() {
            return;
        }
        self.gc.lock().release_weak_handle(handle);
    }

    pub fn resolve_handle(&self, handle: &crate::GcHandle) -> Option<usize> {
        if !self.is_enabled() {
            return None;
        }
        self.gc.lock().resolve_handle(handle)
    }

    pub fn install_forwarding(
        &self,
        from: &crate::GcHandle,
        to: &crate::GcHandle,
    ) -> Result<(), ManagedHandleError> {
        if !self.is_enabled() {
            return Err(ManagedHandleError::NotInitialized);
        }
        let (from_ptr, new_record) = {
            let mut gc = self.gc.lock();
            let from_ptr = gc
                .resolve_handle(from)
                .ok_or(ManagedHandleError::InvalidHandle)?;
            let to_ptr = gc
                .resolve_handle(to)
                .ok_or(ManagedHandleError::InvalidHandle)?;
            let Some(target_idx) = gc.find_object_by_object_addr(to_ptr) else {
                return Err(ManagedHandleError::InvalidStoredReference);
            };
            let target_entry = gc.objects[target_idx];
            if !gc.install_forwarding(from_ptr, to_ptr) {
                return Err(ManagedHandleError::InvalidStoredReference);
            }
            let new_record = AllocationRecord::new(
                AllocationKind::Managed,
                MemoryDomain::Managed,
                target_entry.object_addr,
            )
            .with_arena(AllocationArena::Managed)
            .with_sizes(
                target_entry.object_size,
                target_entry.object_size,
                target_entry.object_align,
            );
            (from_ptr, new_record)
        };
        self.observe_relocation(from_ptr, new_record);
        Ok(())
    }

    pub fn pin_handle(&self, handle: &crate::GcHandle) -> Result<(), ManagedHandleError> {
        if !self.is_enabled() {
            return Err(ManagedHandleError::NotInitialized);
        }
        if self.gc.lock().pin_handle(handle) {
            Ok(())
        } else {
            Err(ManagedHandleError::InvalidHandle)
        }
    }

    pub fn unpin_handle(&self, handle: &crate::GcHandle) -> Result<(), ManagedHandleError> {
        if !self.is_enabled() {
            return Err(ManagedHandleError::NotInitialized);
        }
        let mut gc = self.gc.lock();
        if gc.is_handle_pinned(handle) && gc.unpin_handle(handle) {
            Ok(())
        } else if gc.resolve_handle(handle).is_none() {
            Err(ManagedHandleError::InvalidHandle)
        } else {
            Err(ManagedHandleError::NotPinned)
        }
    }

    pub fn is_handle_pinned(&self, handle: &crate::GcHandle) -> Result<bool, ManagedHandleError> {
        if !self.is_enabled() {
            return Err(ManagedHandleError::NotInitialized);
        }
        let gc = self.gc.lock();
        if gc.resolve_handle(handle).is_none() {
            return Err(ManagedHandleError::InvalidHandle);
        }
        Ok(gc.is_handle_pinned(handle))
    }

    pub fn root_handle(
        &self,
        handle: &crate::GcHandle,
        root_type: RootType,
        source_id: usize,
    ) -> Option<crate::GcRootHandle> {
        if !self.is_enabled() {
            return None;
        }
        self.gc.lock().add_handle_root(handle, root_type, source_id)
    }

    pub fn update_root_handle(&self, root: &crate::GcRootHandle, handle: &crate::GcHandle) -> bool {
        if !self.is_enabled() {
            return false;
        }
        self.gc.lock().update_handle_root(root, handle)
    }

    pub fn release_root_handle(&self, root: crate::GcRootHandle) {
        if !self.is_enabled() {
            return;
        }
        self.gc.lock().remove_handle_root(root);
    }

    pub fn clear_roots(&self) {
        if !self.is_enabled() {
            return;
        }
        self.gc.lock().clear_roots();
    }

    pub fn set_root_frame_slot<const N: usize>(
        &self,
        frame: &mut GcRootFrame<N>,
        slot: usize,
        handle: &crate::GcHandle,
    ) -> Result<(), ManagedHandleError> {
        if !self.is_enabled() {
            return Err(ManagedHandleError::NotInitialized);
        }
        if slot >= N {
            return Err(ManagedHandleError::SlotOutOfRange);
        }
        let mut gc = self.gc.lock();
        if gc.resolve_handle(handle).is_none() {
            return Err(ManagedHandleError::InvalidHandle);
        }
        let existing = frame.slots[slot];
        if existing != u16::MAX {
            let root = GcRootHandle { idx: existing };
            if gc.update_handle_root(&root, handle) {
                return Ok(());
            }
            gc.remove_root(existing as usize);
            frame.slots[slot] = u16::MAX;
        }
        let source_id = frame.source_base.saturating_add(slot);
        let Some(root) = gc.add_handle_root(handle, frame.root_type, source_id) else {
            return Err(ManagedHandleError::RootTableFull);
        };
        frame.slots[slot] = root.idx;
        Ok(())
    }

    pub fn clear_root_frame_slot<const N: usize>(
        &self,
        frame: &mut GcRootFrame<N>,
        slot: usize,
    ) -> Result<(), ManagedHandleError> {
        if !self.is_enabled() {
            return Err(ManagedHandleError::NotInitialized);
        }
        if slot >= N {
            return Err(ManagedHandleError::SlotOutOfRange);
        }
        let root_idx = frame.slots[slot];
        if root_idx != u16::MAX {
            self.gc.lock().remove_root(root_idx as usize);
            frame.slots[slot] = u16::MAX;
        }
        Ok(())
    }

    pub fn clear_root_frame<const N: usize>(&self, frame: &mut GcRootFrame<N>) {
        if !self.is_enabled() {
            return;
        }
        let mut gc = self.gc.lock();
        for root_idx in &mut frame.slots {
            if *root_idx != u16::MAX {
                gc.remove_root(*root_idx as usize);
                *root_idx = u16::MAX;
            }
        }
    }

    // ========================================================================
    // 类型化字段访问 API
    // ========================================================================
    pub fn load_ref<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcRefSlot<T>,
    ) -> Result<GcRef<T>, ManagedHandleError> {
        let raw_ref = self.load_ref_raw(owner, slot.offset(), ManagedSlotKind::Strong)?;
        Ok(GcRef::from_raw(raw_ref))
    }

    pub fn load_ref_handle<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcRefSlot<T>,
    ) -> Result<Option<crate::GcHandle>, ManagedHandleError> {
        let raw_ref = self.load_ref(owner, slot)?.as_raw();
        if raw_ref == 0 {
            return Ok(None);
        }
        self.create_handle(raw_ref)
            .map(Some)
            .ok_or(ManagedHandleError::InvalidStoredReference)
    }

    pub fn store_ref<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcRefSlot<T>,
        target: GcRef<T>,
    ) -> Result<(), ManagedHandleError> {
        let new_ref = self.resolve_stored_reference(target.as_raw())?;
        self.store_ref_raw(owner, slot.offset(), new_ref, ManagedSlotKind::Strong)
    }

    pub fn store_ref_handle<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcRefSlot<T>,
        target: Option<&crate::GcHandle>,
    ) -> Result<(), ManagedHandleError> {
        let new_ref = match target {
            Some(handle) => self
                .resolve_handle(handle)
                .ok_or(ManagedHandleError::InvalidHandle)?,
            None => 0,
        };
        self.store_ref(owner, slot, GcRef::from_raw(new_ref))
    }

    pub fn load_weak_ref<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcWeakRefSlot<T>,
    ) -> Result<GcWeakRef<T>, ManagedHandleError> {
        let raw_ref = self.load_ref_raw(owner, slot.offset(), ManagedSlotKind::Weak)?;
        Ok(GcWeakRef::from_raw(raw_ref))
    }

    pub fn load_weak_handle<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcWeakRefSlot<T>,
    ) -> Result<Option<crate::GcWeakHandle>, ManagedHandleError> {
        let raw_ref = self.load_weak_ref(owner, slot)?.as_raw();
        if raw_ref == 0 {
            return Ok(None);
        }
        let Some(strong) = self.create_handle(raw_ref) else {
            return Err(ManagedHandleError::InvalidStoredReference);
        };
        let weak = self
            .downgrade_handle(&strong)
            .ok_or(ManagedHandleError::InvalidStoredReference)?;
        self.release_handle(strong);
        Ok(Some(weak))
    }

    pub fn store_weak_ref<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcWeakRefSlot<T>,
        target: GcWeakRef<T>,
    ) -> Result<(), ManagedHandleError> {
        let new_ref = self.resolve_stored_reference(target.as_raw())?;
        self.store_ref_raw(owner, slot.offset(), new_ref, ManagedSlotKind::Weak)
    }

    pub fn store_weak_handle<T>(
        &self,
        owner: &crate::GcHandle,
        slot: GcWeakRefSlot<T>,
        target: Option<&crate::GcWeakHandle>,
    ) -> Result<(), ManagedHandleError> {
        let new_ref = match target {
            Some(handle) => match self.upgrade_weak_handle(handle) {
                Some(strong) => {
                    let resolved = self
                        .resolve_handle(&strong)
                        .ok_or(ManagedHandleError::InvalidStoredReference)?;
                    self.release_handle(strong);
                    resolved
                }
                None => 0,
            },
            None => 0,
        };
        self.store_weak_ref(owner, slot, GcWeakRef::from_raw(new_ref))
    }

    pub fn register_finalizer(&self, callback: FinalizerFn) -> Option<u16> {
        if !self.is_enabled() {
            return None;
        }
        self.gc.lock().register_finalizer(callback)
    }

    pub fn write_barrier(
        &self,
        obj_addr: usize,
        field_offset: usize,
        new_ref: usize,
        old_ref: usize,
    ) {
        if !self.is_enabled() {
            return;
        }
        self.gc
            .lock()
            .write_barrier(obj_addr, field_offset, new_ref, old_ref);
    }

    // ========================================================================
    // 回调绑定
    // ========================================================================
    pub fn bind_gc_critical_section(
        &self,
        enter: crate::GcEnterCriticalFn,
        leave: crate::GcLeaveCriticalFn,
    ) {
        self.gc_enter_critical
            .store(enter as usize, Ordering::Release);
        self.gc_leave_critical
            .store(leave as usize, Ordering::Release);
    }

    pub fn bind_relocation_observer(&self, callback: crate::ManagedGcMoveCallbackFn) {
        self.relocation_observer
            .store(callback as usize, Ordering::Release);
    }

    // ========================================================================
    // 统计与控制
    // ========================================================================
    pub fn stats(&self) -> ManagedStats {
        let gc = self.gc.lock();
        ManagedStats {
            enabled: self.is_enabled(),
            heap_start: self.heap_start.load(Ordering::Acquire),
            heap_size: self.heap_size.load(Ordering::Acquire),
            alloc_requests: self.alloc_requests.load(Ordering::Acquire),
            free_requests: self.free_requests.load(Ordering::Acquire),
            active_objects: self.active_objects.load(Ordering::Acquire),
            active_bytes: self.active_bytes.load(Ordering::Acquire),
            alloc_failures: self.alloc_failures.load(Ordering::Acquire),
            gc: gc.stats(),
            gc_control: Some(
                gc.control_snapshot(self.gc_safepoint_requested.load(Ordering::Acquire)),
            ),
        }
    }

    pub fn set_mode(&self, mode: GcMode) {
        if !self.is_enabled() {
            return;
        }
        self.gc.lock().set_mode(mode);
    }

    pub fn mode(&self) -> Option<GcMode> {
        if !self.is_enabled() {
            return None;
        }
        Some(self.gc.lock().mode())
    }

    pub fn gc_control_snapshot(&self) -> Option<gc::GcControlSnapshot> {
        if !self.is_enabled() {
            return None;
        }
        Some(
            self.gc
                .lock()
                .control_snapshot(self.gc_safepoint_requested.load(Ordering::Acquire)),
        )
    }

    // ========================================================================
    // 增量/并发标记
    // ========================================================================
    pub fn incremental_mark_step(&self, batch_size: usize) -> bool {
        if !self.is_enabled() {
            return false;
        }
        let critical_state = self.enter_gc_safepoint();
        // 在安全点内刷新自动根并推进标记
        {
            let mut gc = self.gc.lock();
            if gc.phase == gc::GcPhase::Idle {
                self.collect_automatic_roots(&mut gc);
            }
        }
        let more_work = self.gc.lock().incremental_mark(batch_size);
        self.leave_gc_safepoint(critical_state);
        more_work
    }

    pub fn continue_concurrent_mark(&self) -> bool {
        if !self.is_enabled() {
            return false;
        }
        self.gc.lock().continue_concurrent_mark()
    }

    pub fn finish_incremental_cycle(&self) {
        if !self.is_enabled() {
            return;
        }
        let mut pending = [PendingFinalizer::empty(); MAX_PENDING_FINALIZERS];
        let start_time = self.gc_timestamp_now();
        let critical_state = self.enter_gc_safepoint();
        let pending_count = {
            let mut gc = self.gc.lock();
            match gc.mode() {
                GcMode::MarkSweep | GcMode::MarkCompact => {
                    gc.finish_incremental_cycle();
                    let count = gc.drain_pending_finalizers(&mut pending);
                    gc.stats.finalizers_run += count as u64;
                    count
                }
            }
        };
        self.update_gc_pause_stats(start_time);
        self.leave_gc_safepoint(critical_state);
        self.run_drained_finalizers(&mut pending, pending_count);
    }

    // ========================================================================
    // 内部辅助函数
    // ========================================================================
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn contains(&self, addr: usize) -> bool {
        if !self.is_enabled() {
            return false;
        }
        let start = self.heap_start.load(Ordering::Acquire);
        let end = start.saturating_add(self.heap_size.load(Ordering::Acquire));
        addr >= start && addr < end
    }

    pub fn owns(&self, ptr: usize) -> bool {
        self.read_allocation(ptr).is_some()
    }

    fn read_allocation(&self, ptr: usize) -> Option<ManagedAllocation> {
        if !self.contains(ptr)
            || ptr < self.heap_start.load(Ordering::Acquire) + GcObjectHeader::HEADER_SIZE
        {
            return None;
        }
        self.read_allocation_from_header(ptr - GcObjectHeader::HEADER_SIZE)
            .filter(|allocation| allocation.object_addr == ptr)
    }

    fn read_allocation_from_header(&self, header_addr: usize) -> Option<ManagedAllocation> {
        if !self.contains(header_addr) {
            return None;
        }
        let header = unsafe { *(header_addr as *const GcObjectHeader) };
        if header.size == 0 {
            return None;
        }
        let raw_base = header_addr.checked_sub(header.prefix_bytes as usize)?;
        let reserve_size = decode_reserve_size(header) as usize;
        let object_addr = header_addr.checked_add(GcObjectHeader::HEADER_SIZE)?;
        let object_end = object_addr.checked_add(header.size as usize)?;
        let heap_end =
            self.heap_start.load(Ordering::Acquire) + self.heap_size.load(Ordering::Acquire);
        if raw_base < self.heap_start.load(Ordering::Acquire)
            || object_end > heap_end
            || reserve_size < GcObjectHeader::HEADER_SIZE + header.size as usize
        {
            return None;
        }
        Some(ManagedAllocation {
            header_addr,
            raw_base,
            reserve_size,
            object_addr,
            object_size: header.size as usize,
        })
    }

    fn validate_managed_field(
        &self,
        owner_ptr: usize,
        field_offset: usize,
        slot_kind: ManagedSlotKind,
    ) -> Result<(), ManagedHandleError> {
        let Some(allocation) = self.read_allocation(owner_ptr) else {
            return Err(ManagedHandleError::InvalidHandle);
        };
        let header = unsafe { *(allocation.header_addr as *const GcObjectHeader) };
        let descriptor = header.trace_descriptor();
        let valid = match slot_kind {
            ManagedSlotKind::Strong => {
                descriptor.allows_ref_offset(allocation.object_size, field_offset)
            }
            ManagedSlotKind::Weak => {
                descriptor.allows_weak_ref_offset(allocation.object_size, field_offset)
            }
        };
        if !valid {
            return Err(ManagedHandleError::InvalidFieldOffset);
        }
        Ok(())
    }

    fn resolve_field_slot(
        &self,
        owner: &crate::GcHandle,
        field_offset: usize,
        slot_kind: ManagedSlotKind,
    ) -> Result<(usize, usize), ManagedHandleError> {
        if !self.is_enabled() {
            return Err(ManagedHandleError::NotInitialized);
        }
        let owner_ptr = self
            .resolve_handle(owner)
            .ok_or(ManagedHandleError::InvalidHandle)?;
        self.validate_managed_field(owner_ptr, field_offset, slot_kind)?;
        Ok((owner_ptr, owner_ptr + field_offset))
    }

    fn resolve_stored_reference(&self, raw_ref: usize) -> Result<usize, ManagedHandleError> {
        if raw_ref == 0 {
            return Ok(0);
        }
        let resolved = {
            let gc = self.gc.lock();
            gc.resolve_forwarding_addr(raw_ref)
        };
        if !self.owns(resolved) && self.read_allocation(resolved).is_none() {
            return Err(ManagedHandleError::InvalidStoredReference);
        }
        Ok(resolved)
    }

    fn load_ref_raw(
        &self,
        owner: &crate::GcHandle,
        field_offset: usize,
        slot_kind: ManagedSlotKind,
    ) -> Result<usize, ManagedHandleError> {
        let (_owner_ptr, slot_addr) = self.resolve_field_slot(owner, field_offset, slot_kind)?;
        let raw_ref = unsafe { *(slot_addr as *const usize) };
        if raw_ref == 0 {
            return Ok(0);
        }
        let resolved = {
            let gc = self.gc.lock();
            gc.resolve_forwarding_addr(raw_ref)
        };
        if self.owns(resolved) || self.read_allocation(resolved).is_some() {
            Ok(resolved)
        } else if slot_kind == ManagedSlotKind::Weak {
            Ok(0)
        } else {
            Err(ManagedHandleError::InvalidStoredReference)
        }
    }

    fn store_ref_raw(
        &self,
        owner: &crate::GcHandle,
        field_offset: usize,
        new_ref: usize,
        slot_kind: ManagedSlotKind,
    ) -> Result<(), ManagedHandleError> {
        let (owner_ptr, slot_addr) = self.resolve_field_slot(owner, field_offset, slot_kind)?;
        let old_ref = unsafe { *(slot_addr as *const usize) };
        unsafe {
            *(slot_addr as *mut usize) = new_ref;
        }
        if slot_kind == ManagedSlotKind::Strong {
            self.write_barrier(owner_ptr, field_offset, new_ref, old_ref);
        }
        Ok(())
    }

    fn reclaim_allocation(&self, allocation: ManagedAllocation, vmem: &KernelAddressSpace) {
        self.free_requests.fetch_add(1, Ordering::Relaxed);
        self.active_objects.fetch_sub(1, Ordering::Relaxed);
        self.active_bytes
            .fetch_sub(allocation.object_size, Ordering::Relaxed);
        unsafe {
            write_bytes(allocation.raw_base as *mut u8, 0, allocation.reserve_size);
        }
        compiler_fence(Ordering::SeqCst);
        let _ = vmem.free_managed_range(allocation.raw_base, allocation.reserve_size);
        if let Some(callback) = self.load_external_free_callback() {
            callback(allocation.object_addr, allocation.object_size);
        }
    }

    fn reclaim_relocated_allocation(
        &self,
        allocation: ManagedAllocation,
        vmem: &KernelAddressSpace,
    ) {
        self.free_requests.fetch_add(1, Ordering::Relaxed);
        self.active_objects.fetch_sub(1, Ordering::Relaxed);
        self.active_bytes
            .fetch_sub(allocation.object_size, Ordering::Relaxed);
        unsafe {
            write_bytes(allocation.raw_base as *mut u8, 0, allocation.reserve_size);
        }
        compiler_fence(Ordering::SeqCst);
        let _ = vmem.free_managed_range(allocation.raw_base, allocation.reserve_size);
    }

    fn allocation_from_entry(&self, entry: crate::gc::GcObjectEntry) -> ManagedAllocation {
        ManagedAllocation {
            header_addr: entry.header_addr,
            raw_base: entry.raw_base,
            reserve_size: entry.reserve_size,
            object_addr: entry.object_addr,
            object_size: entry.object_size,
        }
    }

    fn load_vmem(&self) -> Option<&KernelAddressSpace> {
        let raw = self.vmem_ptr.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { &*(raw as *const KernelAddressSpace) })
        }
    }

    fn load_external_free_callback(&self) -> Option<fn(ptr: usize, size: usize)> {
        let raw = self.external_free_callback.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, fn(usize, usize)>(raw) })
        }
    }

    fn load_relocation_observer(&self) -> Option<crate::ManagedGcMoveCallbackFn> {
        let raw = self.relocation_observer.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, crate::ManagedGcMoveCallbackFn>(raw) })
        }
    }

    fn observe_relocation(&self, old_ptr: usize, new_record: AllocationRecord) -> bool {
        if let Some(callback) = self.load_relocation_observer() {
            callback(old_ptr, new_record)
        } else {
            true
        }
    }

    fn gc_timestamp_now(&self) -> u64 {
        let callback = { self.gc.lock().timestamp_ns };
        match callback {
            Some(f) => f(),
            None => 0,
        }
    }

    fn update_gc_pause_stats(&self, start_time: u64) {
        let end_time = self.gc_timestamp_now();
        let pause = end_time.saturating_sub(start_time);
        let mut gc = self.gc.lock();
        gc.stats.last_pause_ns = pause;
        gc.stats.total_pause_ns = gc.stats.total_pause_ns.saturating_add(pause);
    }

    fn enter_gc_safepoint(&self) -> usize {
        self.gc_safepoint_requested.store(true, Ordering::Release);
        self.enter_gc_critical()
    }

    fn leave_gc_safepoint(&self, state: usize) {
        self.leave_gc_critical(state);
        self.gc_safepoint_requested.store(false, Ordering::Release);
    }

    fn enter_gc_critical(&self) -> usize {
        let raw = self.gc_enter_critical.load(Ordering::Acquire);
        if raw == 0 {
            0
        } else {
            let callback: crate::GcEnterCriticalFn =
                unsafe { core::mem::transmute::<usize, crate::GcEnterCriticalFn>(raw) };
            callback()
        }
    }

    fn leave_gc_critical(&self, state: usize) {
        let raw = self.gc_leave_critical.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let callback: crate::GcLeaveCriticalFn =
            unsafe { core::mem::transmute::<usize, crate::GcLeaveCriticalFn>(raw) };
        callback(state);
    }

    fn run_drained_finalizers(
        &self,
        pending: &mut [PendingFinalizer; MAX_PENDING_FINALIZERS],
        pending_count: usize,
    ) {
        let tmp = 0..pending_count;
        for idx in tmp {
            let mut callback = pending[idx].callback;
            let mut obj_addr = pending[idx].obj_addr;
            let mut obj_size = pending[idx].obj_size;
            scrub_pending_finalizer_slot(&mut pending[idx]);
            if let Some(func) = callback {
                func(obj_addr, obj_size);
            }
            scrub_pending_finalizer_locals(&mut callback, &mut obj_addr, &mut obj_size);
        }
        scrub_pending_finalizer_slice(pending);
    }
}

impl Default for ManagedAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 内部辅助类型与函数
// ============================================================================

#[derive(Clone, Copy)]
struct ManagedAllocation {
    header_addr: usize,
    raw_base: usize,
    reserve_size: usize,
    object_addr: usize,
    object_size: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CollectionScope {
    YoungOnly,
    FullHeap,
}

impl CollectionScope {
    const fn contains(self, flags: u16) -> bool {
        match self {
            Self::YoungOnly => flags & GC_FLAG_OLD_GEN == 0,
            Self::FullHeap => true,
        }
    }
}

enum CleanupAction {
    Moved(ManagedAllocation),
    Dead {
        allocation: ManagedAllocation,
        reclaim_callback: Option<fn(ptr: usize, size: usize)>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagedSlotKind {
    Strong,
    Weak,
}

fn object_has_strong_managed_reference_to(gc: &GarbageCollector, target: usize) -> bool {
    for idx in 0..gc.object_count {
        let entry = gc.objects[idx];
        if !entry.active || entry.object_addr == target {
            continue;
        }

        let header = unsafe { *(entry.header_addr as *const GcObjectHeader) };
        let descriptor = header.trace_descriptor();
        if !descriptor.is_exact() {
            continue;
        }

        for &offset in descriptor.reference_offsets {
            if !descriptor.allows_ref_offset(entry.object_size, offset) {
                continue;
            }
            let raw_ref = unsafe { *((entry.object_addr + offset) as *const usize) };
            if raw_ref == 0 {
                continue;
            }
            let resolved = gc.resolve_forwarding_addr(raw_ref);
            if resolved == target {
                return true;
            }
            if let Some(ref_idx) = gc.find_object_containing(resolved)
                && gc.objects[ref_idx].object_addr == target
            {
                return true;
            }
        }
    }
    false
}

fn encode_reserve_size(header: &mut GcObjectHeader, reserve_size: usize) {
    header.reserve_size_lo = (reserve_size & 0xffff) as u16;
    header.reserve_size_hi = ((reserve_size >> 16) & 0xffff) as u16;
}

fn decode_reserve_size(header: GcObjectHeader) -> u32 {
    header.reserve_size_lo as u32 | ((header.reserve_size_hi as u32) << 16)
}

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

#[inline(never)]
fn scrub_pending_finalizer_slot(slot: &mut PendingFinalizer) {
    unsafe {
        core::ptr::write_volatile(slot, PendingFinalizer::empty());
    }
    compiler_fence(Ordering::SeqCst);
}

#[inline(never)]
fn scrub_pending_finalizer_locals(
    callback: &mut Option<FinalizerFn>,
    obj_addr: &mut usize,
    obj_size: &mut usize,
) {
    unsafe {
        core::ptr::write_volatile(callback, None);
        core::ptr::write_volatile(obj_addr, 0);
        core::ptr::write_volatile(obj_size, 0);
    }
    compiler_fence(Ordering::SeqCst);
}

#[inline(never)]
fn scrub_pending_finalizer_slice(pending: &mut [PendingFinalizer]) {
    for slot in pending {
        scrub_pending_finalizer_slot(slot);
    }
}
