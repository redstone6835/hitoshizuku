#![no_std]
//!
//! 分层内核分配器总入口。
//!
//! 这个 crate 不是单一算法的封装，而是一套“按职责分层”的内核内存子系统。它把
//! 不同类型的内存问题拆成不同层处理：
//!
//! - `boot` 解决最早期临时分配；
//! - `buddy` 管理物理页帧；
//! - `vmem` / `space` 管理虚拟地址区间与映射协调；
//! - `slab` 处理高频小对象；
//! - `kheap` 处理大对象与页级对齐分配；
//! - `managed` / `gc` 为受管对象提供可选的自动回收能力；
//! - `registry` / `metadata` 负责内部记账与自举支持。
//!
//! 因为它是分层系统，所以这里最重要的不是某一个算法，而是层之间的调用顺序、
//! 锁顺序和职责边界。文件头下面的 lock ordering 说明，就是为了保证这些层在组合
//! 起来以后仍然能稳定工作。
//!
//! # 锁顺序 (Lock Ordering)
//!
//! 为防止死锁，必须严格按照以下顺序获取锁：
//!
//! 1. `init_lock` — 初始化专用，正常运行期间绝不持有
//! 2. `phys` (BuddyAllocator) — 物理内存分配器
//! 3. `vmem` arena 锁 (direct_map, kernel, managed) — 虚拟地址空间
//! 4. `metadata.inner` — 元数据分配器
//! 5. `registry.inner` — 分配注册表
//! 6. `slab.state` — Slab 分配器状态
//! 7. `slab.cache.inner` — Per-CPU 缓存
//! 8. `kheap.inner` — 大对象分配器
//! 9. `managed.gc` — 垃圾回收器
//! 10. `managed.exact_root_registry` — 动态精确根槽注册表
//!
//! ## 关键规则
//!
//! - **调用回调 (map_fn, unmap_fn) 时绝不持有 `phys` 锁**
//! - **在调用可能触发分配的函数前，先释放锁**
//! - **尽量避免同时持有多个锁**
//! - **绝不反序获取锁**
//!
//! ## 锁释放策略
//!
//! - 尽可能早地释放锁
//! - 使用作用域块 `{ let guard = lock(); ... }` 确保提前释放
//! - 对于多步操作，在各步骤之间释放锁
//!
//! ## 示例
//!
//! 正确做法：
//! ```rust
//! let vaddr = {
//!     let arena = self.arena.lock();
//!     arena.alloc(size)
//! }; // 锁已释放
//!
//! let paddr = {
//!     let phys = self.phys.lock();
//!     phys.alloc_pages(order)
//! }; // 锁已释放
//!
//! // 此时可在不持有任何锁的情况下调用 map_fn
//! map_fn(vaddr, paddr, size);
//! ```
//!
//! 错误做法：
//! ```rust
//! let arena = self.arena.lock();
//! let phys = self.phys.lock(); // 同时持有两把锁
//! let paddr = phys.alloc_pages(order);
//! map_fn(vaddr, paddr, size); // 持锁时调用回调 — 有死锁风险！
//! ```

mod boot;
mod buddy;
mod error;
mod gc;
mod kheap;
mod managed;
mod metadata;
mod registry;
mod request;
mod slab;
mod space;
pub mod stats;
mod vmem;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use spin::mutex::Mutex;

use boot::BootAllocator;
use buddy::BuddyAllocator;
use kheap::KernelHeap;
use managed::ManagedAllocator as ManagedRuntime;
use metadata::MetadataAllocator;
use registry::AllocationRegistry;
use slab::SlabAllocator;

pub use buddy::{BuddyAllocator as PhysicalAllocator, BuddyStats, MemorySegment, PAGE_SIZE};
pub use error::{
    AddressSpaceError, AllocationError, DeallocationError, InitError, ManagedHandleError,
    OwnershipError, RegistryError, VmemError,
};
pub use gc::{
    FinalizerFn, GcCell, GcCollectionKind, GcControlSnapshot, GcHandle, GcMode, GcObjectHeader,
    GcPhase, GcRef, GcRefSlot, GcRootFrame, GcRootHandle, GcStats, GcWeakHandle, GcWeakRef,
    GcWeakRefSlot, RootType, TRACE_FLAG_HAS_FINALIZER, TRACE_FLAG_HAS_WEAK_REFS,
    TRACE_FLAG_PINNED_LAYOUT, TraceDescriptor,
};
pub use kheap::{KernelHeap as LargeObjectAllocator, KernelHeapStats};
pub use managed::{
    DEFAULT_MANAGED_HEAP_ORDER, ExactRootProviderFn, LARGE_MANAGED_HEAP_ORDER, ManagedAllocator,
    ManagedFailurePolicy, ManagedHeapConfig, ManagedStats,
};
pub use metadata::MetadataStats;
pub use registry::AllocationRegistryStats;
pub use request::{
    AllocationArena, AllocationKind, AllocationRecord, ManagedAllocFlags, MemoryDomain,
    MemoryPlacement, MemoryRequest, PagePolicy, PhysicalAllocRequest, PhysicalAllocation,
    ReclaimPolicy, Zeroing,
};
pub use slab::{MAX_CPUS, MAX_SMALL_SIZE, SlabAllocator as ZoneAllocator, SlabStats};
pub use space::{AddressSpaceStats, ArenaKind, BackedRange, KernelAddressSpace};
pub use vmem::{VmemAllocPolicy, VmemStats, VmemValidationStats};

pub type PhysicalMemoryManager = BuddyAllocator;
pub type AddressSpaceManager = KernelAddressSpace;
pub type MappedRange = BackedRange;

pub type PhysToVirtFn = fn(paddr: usize) -> usize;
pub type VirtToPhysFn = fn(vaddr: usize) -> usize;
pub type CpuIdFn = fn() -> usize;
pub type GcEnterCriticalFn = fn() -> usize;
pub type GcLeaveCriticalFn = fn(state: usize);
pub type ManagedGcMoveCallbackFn = fn(old_ptr: usize, new_record: AllocationRecord) -> bool;
pub type KernelHeapRegionFn = fn() -> (usize, usize);
pub type MapKernelHeapRangeFn =
    fn(vaddr: usize, paddr: usize, size: usize, page_policy: PagePolicy) -> bool;
pub type UnmapKernelHeapRangeFn = fn(vaddr: usize, size: usize) -> bool;

#[derive(Clone, Copy, Debug)]
pub struct AllocStats {
    pub total_allocs: u64,
    pub total_deallocs: u64,
    pub total_reallocs: u64,
    pub total_bytes_allocated: u64,
    pub total_bytes_freed: u64,
    pub oom_count: u64,
    pub ownership_failures: u64,
    pub boot_used_bytes: usize,
    pub vmem_used_bytes: usize,
    pub active_small_allocs: u64,
    pub active_large_allocs: u64,
    pub active_managed_allocs: u64,
    pub active_managed_bytes: usize,
    pub managed_enabled: bool,
}

pub struct KernelMemorySubsystem {
    boot: BootAllocator,
    phys: Mutex<BuddyAllocator>,
    vmem: KernelAddressSpace,
    kheap: KernelHeap,
    slab: SlabAllocator,
    managed: ManagedRuntime,
    metadata: MetadataAllocator,
    registry: AllocationRegistry,
    init_lock: Mutex<()>,
    active: AtomicBool,
    phys_to_virt: AtomicUsize,
    virt_to_phys: AtomicUsize,
    cpu_id_fn: AtomicUsize,
    kernel_heap_region_fn: AtomicUsize,
    kernel_heap_map_fn: AtomicUsize,
    kernel_heap_unmap_fn: AtomicUsize,
    total_allocs: AtomicU64,
    total_deallocs: AtomicU64,
    total_reallocs: AtomicU64,
    total_bytes_allocated: AtomicU64,
    total_bytes_freed: AtomicU64,
    oom_count: AtomicU64,
    ownership_failures: AtomicU64,
    managed_growth_order: AtomicUsize,
}

pub type KernelAllocator = KernelMemorySubsystem;

unsafe impl Sync for KernelMemorySubsystem {}

impl KernelMemorySubsystem {
    pub const fn new() -> Self {
        Self {
            boot: BootAllocator::new(),
            phys: Mutex::new(BuddyAllocator::new()),
            vmem: KernelAddressSpace::new(),
            kheap: KernelHeap::new(),
            slab: SlabAllocator::new(),
            managed: ManagedRuntime::new(),
            metadata: MetadataAllocator::new(),
            registry: AllocationRegistry::new(),
            init_lock: Mutex::new(()),
            active: AtomicBool::new(false),
            phys_to_virt: AtomicUsize::new(0),
            virt_to_phys: AtomicUsize::new(0),
            cpu_id_fn: AtomicUsize::new(0),
            kernel_heap_region_fn: AtomicUsize::new(0),
            kernel_heap_map_fn: AtomicUsize::new(0),
            kernel_heap_unmap_fn: AtomicUsize::new(0),
            total_allocs: AtomicU64::new(0),
            total_deallocs: AtomicU64::new(0),
            total_reallocs: AtomicU64::new(0),
            total_bytes_allocated: AtomicU64::new(0),
            total_bytes_freed: AtomicU64::new(0),
            oom_count: AtomicU64::new(0),
            ownership_failures: AtomicU64::new(0),
            managed_growth_order: AtomicUsize::new(DEFAULT_MANAGED_HEAP_ORDER),
        }
    }

    pub fn bind_address_translation(&self, phys_to_virt: PhysToVirtFn, virt_to_phys: VirtToPhysFn) {
        self.phys_to_virt
            .store(phys_to_virt as usize, Ordering::Release);
        self.virt_to_phys
            .store(virt_to_phys as usize, Ordering::Release);
    }

    pub fn bind_cpu_id(&self, cpu_id_fn: CpuIdFn) {
        self.cpu_id_fn.store(cpu_id_fn as usize, Ordering::Release);
    }

    pub fn bind_gc_critical_section(&self, enter: GcEnterCriticalFn, leave: GcLeaveCriticalFn) {
        self.managed.bind_gc_critical_section(enter, leave);
    }

    pub fn register_gc_exact_root_slot(
        &self,
        slot: &'static AtomicUsize,
        root_type: RootType,
    ) -> bool {
        self.managed.register_exact_root_slot(slot, root_type)
    }

    pub fn unregister_gc_exact_root_slot(&self, slot: &'static AtomicUsize) -> bool {
        self.managed.unregister_exact_root_slot(slot)
    }

    pub fn bind_gc_exact_root_provider(&self, provider: ExactRootProviderFn) {
        self.managed.register_exact_root_provider(provider);
    }

    pub fn bind_kernel_heap_ops(
        &self,
        region_fn: KernelHeapRegionFn,
        map_fn: MapKernelHeapRangeFn,
        unmap_fn: UnmapKernelHeapRangeFn,
    ) {
        self.kernel_heap_region_fn
            .store(region_fn as usize, Ordering::Release);
        self.kernel_heap_map_fn
            .store(map_fn as usize, Ordering::Release);
        self.kernel_heap_unmap_fn
            .store(unmap_fn as usize, Ordering::Release);
        self.vmem.bind_kernel_heap_mapping(map_fn, unmap_fn);
    }

    pub fn init_boot(&self, start: usize, size: usize) {
        let _guard = self.init_lock.lock();
        self.boot.init(start, size);
        self.metadata.bind_boot_source(&self.boot);
    }

    pub fn init_phys(
        &self,
        segments: &[MemorySegment],
        reserved_regions: &[(usize, usize)],
    ) -> Result<(), InitError> {
        let _guard = self.init_lock.lock();
        if !self.boot.is_initialized() {
            return Err(InitError::BootNotInitialized);
        }
        let Some(phys_to_virt) = self.load_phys_to_virt() else {
            return Err(InitError::MissingPhysToVirt);
        };
        let mut phys = self.phys.lock();
        phys.init(segments, reserved_regions, phys_to_virt, &self.boot)
            .map_err(|err| match err {
                buddy::BuddyInitError::EmptyMemoryMap => InitError::InvalidMemoryMap,
                buddy::BuddyInitError::MetadataOutOfMemory => InitError::MetadataOutOfMemory,
            })?;
        Ok(())
    }

    pub fn init_vmem(&self, reserved_regions: &[(usize, usize)]) -> Result<(), InitError> {
        let _guard = self.init_lock.lock();
        let Some(phys_to_virt) = self.load_phys_to_virt() else {
            return Err(InitError::MissingPhysToVirt);
        };
        let Some(virt_to_phys) = self.load_virt_to_phys() else {
            return Err(InitError::MissingVirtToPhys);
        };
        let Some(region_fn) = self.load_kernel_heap_region_fn() else {
            return Err(InitError::MissingKernelHeapRegion);
        };
        let kernel_heap_region = region_fn();

        let init_result = {
            let phys = self.phys.lock();
            if !phys.is_initialized() {
                return Err(InitError::PhysNotInitialized);
            }

            // `vmem` 初始化期间会构造 boundary tags；这些 tag 通过 metadata allocator
            // 分配。若此时 metadata 已切到 dynamic 路径，则会再次进入 `phys.lock()`
            // 申请 backing pages，形成同核自旋锁重入死锁。因此必须在 `vmem`
            // 初始化完成并释放 `phys` 锁之后，才允许 metadata 切换到 dynamic。
            self.vmem.init_from_phys(
                &phys,
                reserved_regions,
                phys_to_virt,
                virt_to_phys,
                kernel_heap_region,
                &self.boot,
            )
        };

        match init_result {
            Ok(()) => {
                self.metadata.enable_dynamic();
                Ok(())
            }
            Err(err) => {
                log::info!("[alloc][init] init_vmem failed err={:?}", err);
                Err(match err {
                    AddressSpaceError::MetadataOutOfMemory => InitError::MetadataOutOfMemory,
                    _ => InitError::AddressSpaceInitFailed,
                })
            }
        }
    }

    pub fn init_kheap(&self) {
        let _guard = self.init_lock.lock();
        self.kheap.init();
    }

    pub fn init_slab(&self, cpu_count: usize) {
        let _guard = self.init_lock.lock();
        self.slab.bind_metadata_source(&self.boot);
        self.slab.init(cpu_count);
    }

    pub fn init_managed(
        &self,
        order: usize,
        mode: GcMode,
        free_callback: fn(ptr: usize, size: usize),
        timestamp_ns: Option<fn() -> u64>,
    ) -> Result<BackedRange, InitError> {
        let _guard = self.init_lock.lock();
        self.init_managed_locked(ManagedHeapConfig {
            order,
            mode,
            failure_policy: ManagedFailurePolicy::ReturnError,
            external_free_callback: Some(free_callback),
            timestamp_ns,
        })
    }

    pub fn init_managed_with_config(
        &self,
        config: ManagedHeapConfig,
    ) -> Result<BackedRange, InitError> {
        let _guard = self.init_lock.lock();
        self.init_managed_locked(config)
    }

    fn init_managed_locked(&self, config: ManagedHeapConfig) -> Result<BackedRange, InitError> {
        if self.managed.is_enabled() {
            return Err(InitError::ManagedAlreadyInitialized);
        }
        let mode = config.mode;

        let range = self
            .vmem
            .init_managed_heap(config.order, &self.phys)
            .map_err(|err| match err {
                AddressSpaceError::ManagedUnavailable => InitError::ManagedAlreadyInitialized,
                _ => InitError::ManagedRegionUnavailable,
            })?;

        self.managed.init(
            range.vaddr,
            range.size,
            mode,
            &self.vmem as *const KernelAddressSpace,
            managed_gc_reclaim,
            config.external_free_callback,
            config.timestamp_ns,
        );
        self.managed.bind_relocation_observer(managed_gc_retarget);
        self.managed_growth_order
            .store(config.order, Ordering::Release);
        log::info!(
            "[alloc][managed] default heap enabled base={:#x} size={} order={} mode={:?} failure_policy={:?}",
            range.vaddr,
            range.size,
            config.order,
            mode,
            config.failure_policy,
        );
        Ok(range)
    }

    fn grow_managed_locked(&self, order: usize) -> Result<BackedRange, InitError> {
        let managed = self.managed.stats();
        let expected_base = managed.heap_start.saturating_add(managed.heap_size);
        let range = self
            .vmem
            .grow_managed_heap_contiguous(expected_base, order, &self.phys)
            .map_err(|err| match err {
                AddressSpaceError::MetadataOutOfMemory => InitError::MetadataOutOfMemory,
                _ => InitError::ManagedRegionUnavailable,
            })?;
        if !self.managed.extend_heap_contiguous(range.vaddr, range.size) {
            panic!(
                "[alloc][managed][invariant] heap growth not contiguous base={:#x} size={}",
                range.vaddr, range.size,
            );
        }
        log::info!(
            "[alloc][managed] heap expanded base={:#x} size={} order={}",
            range.vaddr,
            range.size,
            order,
        );
        Ok(range)
    }

    fn maybe_grow_managed(&self) -> Result<(), InitError> {
        if !self.managed.is_enabled() {
            return self.ensure_default_managed();
        }

        let _guard = self.init_lock.lock();
        if !self.managed.is_enabled() {
            return self.ensure_default_managed();
        }

        let order = self.managed_growth_order.load(Ordering::Acquire);
        self.grow_managed_locked(order).map(|_| ())
    }

    pub fn init_gc(
        &self,
        order: usize,
        mode: GcMode,
        free_callback: fn(ptr: usize, size: usize),
        timestamp_ns: Option<fn() -> u64>,
    ) -> Result<BackedRange, InitError> {
        self.init_managed(order, mode, free_callback, timestamp_ns)
    }

    pub fn activate_global(&self) -> Result<(), InitError> {
        let _guard = self.init_lock.lock();
        if !self.boot.is_initialized() {
            return Err(InitError::BootNotInitialized);
        }
        if !self.phys.lock().is_initialized() {
            return Err(InitError::PhysNotInitialized);
        }
        if !self.vmem.is_initialized() {
            return Err(InitError::AddressSpaceInitFailed);
        }
        if !self.kheap.is_initialized() {
            return Err(InitError::LargeAllocatorNotInitialized);
        }
        if !self.slab.is_initialized() {
            return Err(InitError::ZoneNotInitialized);
        }
        let kernel_heap_uses_mapped_window = self
            .load_kernel_heap_region_fn()
            .map(|region_fn| region_fn().1 != 0)
            .unwrap_or(false);
        if kernel_heap_uses_mapped_window
            && (self.load_kernel_heap_map_fn().is_none()
                || self.load_kernel_heap_unmap_fn().is_none())
        {
            return Err(InitError::MissingKernelHeapMappingOps);
        }
        if !self.registry.init(&self.boot) {
            return Err(InitError::MetadataOutOfMemory);
        }
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> AllocStats {
        let boot = self.boot.snapshot();
        let address_space = self.vmem.snapshot();
        let slab = self.slab.snapshot();
        let kheap = self.kheap.snapshot();
        let managed = self.managed.stats();
        AllocStats {
            total_allocs: self.total_allocs.load(Ordering::Acquire),
            total_deallocs: self.total_deallocs.load(Ordering::Acquire),
            total_reallocs: self.total_reallocs.load(Ordering::Acquire),
            total_bytes_allocated: self.total_bytes_allocated.load(Ordering::Acquire),
            total_bytes_freed: self.total_bytes_freed.load(Ordering::Acquire),
            oom_count: self.oom_count.load(Ordering::Acquire),
            ownership_failures: self.ownership_failures.load(Ordering::Acquire),
            boot_used_bytes: boot.used_bytes,
            vmem_used_bytes: address_space.kernel.allocated_size,
            active_small_allocs: slab.active_objects,
            active_large_allocs: kheap.active_allocs,
            active_managed_allocs: managed.active_objects,
            active_managed_bytes: managed.active_bytes,
            managed_enabled: managed.enabled,
        }
    }

    pub fn detailed_stats(&self) -> stats::MemoryOverview {
        let boot = self.boot.snapshot();
        let phys = self.buddy_stats();
        let address_space = self.address_space_stats();
        let kheap = self.kheap.snapshot();
        let slab = self.slab.snapshot();
        let managed = self.managed.stats();
        stats::build_overview(boot, phys, address_space, kheap, slab, managed)
    }

    pub fn layer_stats(&self) -> stats::AllocatorLayerStats {
        stats::AllocatorLayerStats {
            phys: self.buddy_stats(),
            address_space: self.address_space_stats(),
            kheap: self.kheap.snapshot(),
            slab: self.slab.snapshot(),
            registry: self.registry.stats(),
            managed: self.managed.stats(),
        }
    }

    pub fn pressure_level(&self) -> u8 {
        self.detailed_stats().pressure_level
    }

    pub fn format_diagnostic(&self, buf: &mut [u8]) -> usize {
        let overview = self.detailed_stats();
        let layers = self.layer_stats();
        stats::format_diagnostic(buf, &overview, &layers)
    }

    pub fn address_space(&self) -> &KernelAddressSpace {
        &self.vmem
    }

    pub fn zone_allocator(&self) -> &SlabAllocator {
        &self.slab
    }

    pub fn large_allocator(&self) -> &KernelHeap {
        &self.kheap
    }

    pub fn managed_allocator(&self) -> &ManagedRuntime {
        &self.managed
    }

    pub fn buddy_stats(&self) -> BuddyStats {
        self.phys.lock().stats()
    }

    pub fn address_space_stats(&self) -> AddressSpaceStats {
        self.vmem.snapshot()
    }

    pub fn vmem_stats(&self) -> VmemStats {
        self.vmem.kernel_stats()
    }

    pub fn registry_stats(&self) -> AllocationRegistryStats {
        self.registry.stats()
    }

    pub fn allocate_physical(
        &self,
        request: PhysicalAllocRequest,
    ) -> Result<PhysicalAllocation, buddy::BuddyAllocError> {
        log::debug!(
            "[alloc][phys] request size={} align={} page_policy={:?} placement={:?}",
            request.size,
            request.align,
            request.page_policy,
            request.placement,
        );
        let mut phys = self.phys.lock();
        let result = phys.alloc_pages_with(&request);
        match result {
            Ok(allocation) => {
                log::debug!(
                    "[alloc][phys] success paddr={:#x} size={} order={} page_size={}",
                    allocation.paddr,
                    allocation.size,
                    allocation.order,
                    allocation.page_size,
                );
                Ok(allocation)
            }
            Err(err) => {
                log::debug!(
                    "[alloc][phys] failed size={} align={} page_policy={:?} placement={:?} err={:?}",
                    request.size,
                    request.align,
                    request.page_policy,
                    request.placement,
                    err,
                );
                Err(err)
            }
        }
    }

    pub fn free_physical(&self, allocation: PhysicalAllocation) -> bool {
        let mut phys = self.phys.lock();
        phys.free_allocation(allocation).is_ok()
    }

    pub fn buddy_alloc_pages(&self, order: usize) -> Option<usize> {
        let mut phys = self.phys.lock();
        phys.alloc_pages(order)
    }

    pub fn buddy_free_pages(&self, addr: usize, order: usize) -> bool {
        let mut phys = self.phys.lock();
        phys.free_pages(addr, order).is_ok()
    }

    pub fn allocate(&self, request: MemoryRequest) -> Result<AllocationRecord, AllocationError> {
        let active = self.active.load(Ordering::Acquire);
        log_request_phase("begin", request, active);

        if !active {
            return self.allocate_boot(request);
        }

        let mut allocation = self.allocate_active_once(request);
        if allocation.is_err()
            && !matches!(request.reclaim, ReclaimPolicy::NoReclaim)
            && self.managed.is_enabled()
        {
            let pressure = self.pressure_level();
            log::debug!(
                "[alloc][reclaim] trigger managed_gc pressure={} request_domain={:?} size={} align={}",
                pressure,
                request.domain,
                request.size,
                request.align,
            );
            self.managed.collect_on_pressure(pressure);
            allocation = self.allocate_active_once(request);
        }
        match allocation {
            Ok(record) => {
                log_record_phase("complete", record);
                Ok(record)
            }
            Err(err) => {
                log::debug!(
                    "[alloc][complete] failed domain={:?} size={} align={} err={:?}",
                    request.domain,
                    request.size,
                    request.align,
                    err,
                );
                Err(err)
            }
        }
    }

    pub fn allocate_managed(
        &self,
        layout: Layout,
        flags: ManagedAllocFlags,
    ) -> Result<AllocationRecord, AllocationError> {
        self.allocate(
            MemoryRequest::for_managed_layout(layout)
                .with_managed_flags(flags)
                .with_zeroing(Zeroing::Zeroed),
        )
    }

    pub fn allocate_managed_handle(
        &self,
        layout: Layout,
        flags: ManagedAllocFlags,
    ) -> Result<GcHandle, AllocationError> {
        let record = self.allocate_managed(layout, flags)?;
        match self.managed.create_handle(record.ptr) {
            Some(handle) => Ok(handle),
            None => {
                let _ = self.deallocate(record.ptr);
                Err(AllocationError::OutOfMemory)
            }
        }
    }

    pub fn query_allocation(&self, ptr: usize) -> Result<AllocationRecord, OwnershipError> {
        if self.boot.contains(ptr) {
            return Ok(
                AllocationRecord::new(AllocationKind::Boot, MemoryDomain::Kernel, ptr)
                    .with_sizes(0, 0, 1),
            );
        }
        if let Some(record) = self.registry.get(ptr) {
            return Ok(record);
        }
        Err(OwnershipError::UnknownPointer)
    }

    pub fn allocation_kind(&self, ptr: usize) -> Result<AllocationKind, OwnershipError> {
        self.query_allocation(ptr).map(|record| record.kind)
    }

    pub fn deallocate(&self, ptr: usize) -> Result<(), DeallocationError> {
        if ptr == 0 {
            return Ok(());
        }
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.boot.contains(ptr) {
            return Ok(());
        }

        let record = self
            .registry
            .remove(ptr)
            .ok_or(DeallocationError::UnknownPointer)?;
        match record.kind {
            AllocationKind::Boot => Ok(()),
            AllocationKind::Small => {
                if self.slab.free_record_reclaiming(
                    record,
                    self.current_cpu_id(),
                    Some((&self.phys, &self.vmem)),
                ) {
                    Ok(())
                } else {
                    panic!(
                        "[alloc][invariant] registry owned small allocation but slab rejected free: ptr={:#x} size={} usable={}",
                        ptr, record.size, record.usable_size
                    )
                }
            }
            AllocationKind::Large => {
                if let Err(err) = self.kheap.free_record(record, &self.phys, &self.vmem) {
                    panic!(
                        "[alloc][invariant] registry owned large allocation but kheap rejected free: ptr={:#x} paddr={:?} order={} err={:?}",
                        ptr, record.paddr, record.order, err
                    )
                }
                Ok(())
            }
            AllocationKind::Managed => {
                if let Err(err) = self.managed.free(ptr, &self.vmem) {
                    if let Err(rollback_err) = self.registry.register_result(&self.boot, record) {
                        panic!(
                            "[alloc][invariant] managed free failed and registry rollback failed: ptr={:#x} free_err={:?} rollback_err={:?}",
                            ptr, err, rollback_err
                        );
                    }
                    // 允许 ObjectStillReferenced 错误传播（例如由上层处理）
                    Err(err)
                } else {
                    Ok(())
                }
            }
            AllocationKind::Physical => {
                let allocation = PhysicalAllocation {
                    paddr: record.paddr.unwrap_or(record.ptr),
                    size: record.usable_size.max(record.size),
                    order: record.order,
                    page_size: record.page_size,
                };
                if self.free_physical(allocation) {
                    Ok(())
                } else {
                    panic!(
                        "[alloc][invariant] registry owned physical allocation but buddy rejected free: ptr={:#x} paddr={:#x} order={}",
                        ptr, allocation.paddr, allocation.order
                    )
                }
            }
        }
    }

    fn allocate_boot(&self, request: MemoryRequest) -> Result<AllocationRecord, AllocationError> {
        if !matches!(request.domain, MemoryDomain::Kernel) {
            log::debug!(
                "[alloc][boot] rejected non-kernel request domain={:?} size={} align={}",
                request.domain,
                request.size,
                request.align,
            );
            return Err(AllocationError::NotInitialized);
        }
        let layout = layout_from_request(request)?;
        let ptr = self.boot.alloc(layout) as usize;
        if ptr == 0 {
            log::debug!(
                "[alloc][boot] out of memory size={} align={}",
                request.size,
                request.align,
            );
            return Err(AllocationError::OutOfMemory);
        }
        if matches!(request.zeroing, Zeroing::Zeroed) {
            unsafe {
                core::ptr::write_bytes(ptr as *mut u8, 0, request.size);
            }
        }
        let record = AllocationRecord::new(AllocationKind::Boot, MemoryDomain::Kernel, ptr)
            .with_sizes(request.size, request.size, request.align);
        log_record_phase("boot", record);
        Ok(record)
    }

    fn alloc_active(&self, layout: Layout, zeroing: Zeroing) -> *mut u8 {
        let request = MemoryRequest::for_kernel_layout(layout).with_zeroing(zeroing);
        match self.allocate(request) {
            Ok(record) => record.ptr as *mut u8,
            Err(_) => null_mut(),
        }
    }

    fn allocate_active_once(
        &self,
        request: MemoryRequest,
    ) -> Result<AllocationRecord, AllocationError> {
        match request.domain {
            MemoryDomain::Kernel => {
                let cpu = self.current_cpu_id();
                let layout = layout_from_request(request)?;
                let force_large = matches!(request.page_policy, PagePolicy::RequireLarge);
                let alloc_large = || -> Result<AllocationRecord, AllocationError> {
                    log::debug!(
                        "[alloc][route] domain=Kernel path=kheap size={} align={} cpu={} page_policy={:?}",
                        request.size,
                        request.align,
                        cpu,
                        request.page_policy,
                    );
                    let range = match self.kheap.alloc_range(
                        layout,
                        request.page_policy,
                        &self.phys,
                        &self.vmem,
                    ) {
                        Ok(range) => range,
                        Err(err) => {
                            log::debug!(
                                "[alloc] kheap alloc_range failed size={} align={} err={:?}",
                                request.size,
                                request.align,
                                err,
                            );
                            return Err(err);
                        }
                    };
                    if matches!(request.zeroing, Zeroing::Zeroed) {
                        unsafe {
                            core::ptr::write_bytes(range.vaddr as *mut u8, 0, request.size);
                        }
                    }
                    let record = AllocationRecord::new(
                        AllocationKind::Large,
                        MemoryDomain::Kernel,
                        range.vaddr,
                    )
                    .with_arena(AllocationArena::Kernel)
                    .with_sizes(request.size, range.size, request.align)
                    .with_physical(
                        range.paddr,
                        range.order,
                        (1usize << range.order) * PAGE_SIZE,
                    );
                    self.register_allocation(record, || {
                        let _ = self.kheap.free_record(record, &self.phys, &self.vmem);
                    })?;
                    Ok(record)
                };
                if is_small_request(request) && !force_large {
                    // 先尝试 slab，但前提是它能满足对齐要求
                    let zone_idx_opt = {
                        let layout = layout_from_request(request)?;
                        SlabAllocator::class_index_for(layout)
                    };

                    if let Some(zone_idx) = zone_idx_opt {
                        let usable_size = self.slab.zone_size_class(zone_idx);
                        log::debug!(
                            "[alloc][route] domain=Kernel path=slab-first size={} align={} cpu={}",
                            request.size,
                            request.align,
                            cpu,
                        );
                        let ptr = self.slab.alloc(layout, cpu, &self.phys, &self.vmem);
                        if ptr.is_null() {
                            log::debug!(
                                "[alloc] slab returned null for size={} align={}, trying kheap fallback",
                                request.size,
                                request.align,
                            );
                            return alloc_large();
                        }
                        if matches!(request.zeroing, Zeroing::Zeroed) {
                            unsafe {
                                core::ptr::write_bytes(ptr, 0, request.size);
                            }
                        }
                        let record = AllocationRecord::new(
                            AllocationKind::Small,
                            MemoryDomain::Kernel,
                            ptr as usize,
                        )
                        .with_arena(AllocationArena::Kernel)
                        .with_sizes(
                            request.size,
                            usable_size,
                            request.align,
                        );
                        self.register_allocation(record, || {
                            self.slab.free_record_reclaiming(
                                record,
                                cpu,
                                Some((&self.phys, &self.vmem)),
                            );
                        })?;
                        Ok(record)
                    } else {
                        // slab 无法满足此对齐要求，回退到 kheap
                        log::debug!(
                            "[alloc][route] domain=Kernel path=kheap-fallback size={} align={} cpu={} reason=slab-alignment-unsupported",
                            request.size,
                            request.align,
                            cpu,
                        );
                        alloc_large()
                    }
                } else {
                    log::debug!(
                        "[alloc][route] domain=Kernel path=kheap-direct size={} align={} cpu={} page_policy={:?}",
                        request.size,
                        request.align,
                        cpu,
                        request.page_policy,
                    );
                    alloc_large()
                }
            }
            MemoryDomain::Managed => {
                log::debug!(
                    "[alloc][route] domain=Managed size={} align={} flags={:?} zeroing={:?}",
                    request.size,
                    request.align,
                    request.managed,
                    request.zeroing,
                );
                if let Err(err) = self.ensure_default_managed() {
                    log::debug!(
                        "[alloc][managed] lazy default heap init failed size={} align={} err={:?}",
                        request.size,
                        request.align,
                        err,
                    );
                    return Err(match err {
                        InitError::MetadataOutOfMemory | InitError::ManagedRegionUnavailable => {
                            AllocationError::OutOfMemory
                        }
                        _ => AllocationError::NotInitialized,
                    });
                }
                let layout = layout_from_request(request)?;
                let record =
                    match self
                        .managed
                        .alloc(layout, &self.vmem, request.managed, request.zeroing)
                    {
                        Ok(record) => record,
                        Err(AllocationError::AddressSpace(
                            AddressSpaceError::OutOfVirtualAddressSpace,
                        )) => {
                            if let Err(err) = self.maybe_grow_managed() {
                                log::debug!(
                                    "[alloc][managed] heap growth failed size={} align={} err={:?}",
                                    request.size,
                                    request.align,
                                    err,
                                );
                                return Err(AllocationError::OutOfMemory);
                            }
                            self.managed.alloc(
                                layout,
                                &self.vmem,
                                request.managed,
                                request.zeroing,
                            )?
                        }
                        Err(err) => return Err(err),
                    };
                self.register_allocation(record, || {
                    let _ = self.managed.free(record.ptr, &self.vmem);
                })?;
                Ok(record)
            }
            MemoryDomain::Physical => {
                log::debug!(
                    "[alloc][route] domain=Physical size={} align={} page_policy={:?} placement={:?}",
                    request.size,
                    request.align,
                    request.page_policy,
                    request.placement,
                );
                let allocation = self
                    .allocate_physical(
                        PhysicalAllocRequest::new(request.size, request.align)
                            .with_page_policy(request.page_policy)
                            .with_placement(request.placement),
                    )
                    .map_err(AllocationError::from)?;
                let record = AllocationRecord::new(
                    AllocationKind::Physical,
                    MemoryDomain::Physical,
                    allocation.paddr,
                )
                .with_physical(allocation.paddr, allocation.order, allocation.page_size)
                .with_sizes(request.size, allocation.size, request.align);
                self.register_allocation(record, || {
                    let _ = self.free_physical(allocation);
                })?;
                Ok(record)
            }
        }
    }

    fn current_cpu_id(&self) -> usize {
        match self.load_cpu_id_fn() {
            Some(f) => f(),
            None => 0,
        }
    }

    fn reclaim_managed_from_gc(&self, header_addr: usize, _size: usize) {
        if let Some(ptr) = self.managed.reclaim_from_gc(header_addr, &self.vmem) {
            let _ = self.registry.remove(ptr);
        }
    }

    fn retarget_managed_registry(&self, old_ptr: usize, new_record: AllocationRecord) -> bool {
        if old_ptr == 0 || new_record.ptr == 0 {
            return false;
        }
        if old_ptr == new_record.ptr {
            return self
                .registry
                .update_existing_result(old_ptr, new_record)
                .is_ok();
        }

        let old_record = match self.registry.remove_result(old_ptr) {
            Ok(record) => record,
            Err(_) => return false,
        };

        let result = match self.registry.get_result(new_record.ptr) {
            Ok(_) => self
                .registry
                .update_existing_result(new_record.ptr, new_record),
            Err(RegistryError::UnknownPointer) => {
                self.registry.register_result(&self.boot, new_record)
            }
            Err(err) => Err(err),
        };

        match result {
            Ok(()) => true,
            Err(_) => {
                let _ = self.registry.register_result(&self.boot, old_record);
                false
            }
        }
    }

    fn ensure_default_managed(&self) -> Result<(), InitError> {
        if self.managed.is_enabled() {
            return Ok(());
        }

        let _guard = self.init_lock.lock();
        if self.managed.is_enabled() {
            return Ok(());
        }

        let config = ManagedHeapConfig::default_kernel();
        self.init_managed_locked(config).map(|_| ())
    }

    fn allocate_internal_metadata(&self, layout: Layout) -> *mut u8 {
        self.metadata
            .alloc(layout, &self.phys, self.load_phys_to_virt())
    }

    pub fn load_phys_to_virt(&self) -> Option<PhysToVirtFn> {
        let raw = self.phys_to_virt.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // Safety: raw 来自有效的 PhysToVirtFn，通过 bind_address_translation 写入。
            // Acquire 加载与 Release 存储同步，保证函数指针值完整可见。
            // transmute 前的 null 检查（raw == 0）确保函数指针非空。
            Some(unsafe { core::mem::transmute::<usize, PhysToVirtFn>(raw) })
        }
    }

    fn load_virt_to_phys(&self) -> Option<VirtToPhysFn> {
        let raw = self.virt_to_phys.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // Safety: 与 load_phys_to_virt 模式相同——raw 是有效的 VirtToPhysFn，
            // 由 bind_address_translation 写入，通过 Acquire/Release 同步。
            Some(unsafe { core::mem::transmute::<usize, VirtToPhysFn>(raw) })
        }
    }

    fn load_cpu_id_fn(&self) -> Option<CpuIdFn> {
        let raw = self.cpu_id_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // Safety: raw 来自有效的 CpuIdFn，通过 bind_cpu_id 写入。
            // Acquire/Release 配对确保可见性。
            Some(unsafe { core::mem::transmute::<usize, CpuIdFn>(raw) })
        }
    }

    fn load_kernel_heap_region_fn(&self) -> Option<KernelHeapRegionFn> {
        let raw = self.kernel_heap_region_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // Safety: raw 来自有效的 KernelHeapRegionFn，通过 bind_kernel_heap_ops 写入。
            // Acquire/Release 配对确保可见性。
            Some(unsafe { core::mem::transmute::<usize, KernelHeapRegionFn>(raw) })
        }
    }

    fn load_kernel_heap_map_fn(&self) -> Option<MapKernelHeapRangeFn> {
        let raw = self.kernel_heap_map_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // Safety: raw 来自有效的 MapKernelHeapRangeFn，通过 bind_kernel_heap_ops 写入。
            // Acquire/Release 配对确保可见性。
            Some(unsafe { core::mem::transmute::<usize, MapKernelHeapRangeFn>(raw) })
        }
    }

    fn load_kernel_heap_unmap_fn(&self) -> Option<UnmapKernelHeapRangeFn> {
        let raw = self.kernel_heap_unmap_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // Safety: raw 来自有效的 UnmapKernelHeapRangeFn，通过 bind_kernel_heap_ops 写入。
            // Acquire/Release 配对确保可见性。
            Some(unsafe { core::mem::transmute::<usize, UnmapKernelHeapRangeFn>(raw) })
        }
    }

    fn can_reuse_allocation(&self, record: AllocationRecord, new_layout: Layout) -> bool {
        match record.kind {
            AllocationKind::Small => {
                is_small(new_layout)
                    && new_layout.pad_to_align().size() <= record.usable_size
                    && new_layout.align() <= PAGE_SIZE
            }
            AllocationKind::Large => {
                !is_small(new_layout) && KernelHeap::required_order_for(new_layout) == record.order
            }
            AllocationKind::Managed => false,
            AllocationKind::Boot | AllocationKind::Physical => false,
        }
    }

    fn record_oom(&self) {
        self.oom_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_ownership_failure(&self) {
        self.ownership_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn register_allocation<F>(
        &self,
        record: AllocationRecord,
        rollback: F,
    ) -> Result<(), AllocationError>
    where
        F: FnOnce(),
    {
        match self.registry.register_result(&self.boot, record) {
            Ok(()) => {
                /* ... */
                Ok(())
            }
            Err(RegistryError::DuplicatePointer) => {
                // 防御性清理：若旧记录对应的对象已不在 GC 对象表中，可安全移除
                let can_remove = match record.kind {
                    AllocationKind::Managed => {
                        let gc = self.managed.gc.lock();
                        gc.find_object_by_object_addr(record.ptr).is_none()
                    }
                    _ => false,
                };
                if can_remove {
                    let _ = self.registry.remove(record.ptr);
                    // 重试插入
                    match self.registry.register_result(&self.boot, record) {
                        Ok(()) => {
                            log::debug!(
                                "[alloc][registry] recovered from stale duplicate: ptr={:#x}",
                                record.ptr
                            );
                            return Ok(());
                        }
                        Err(err) => {
                            log::debug!(
                                "[alloc] registry re-insert after cleanup failed ptr={:#x} err={:?}",
                                record.ptr,
                                err
                            );
                        }
                    }
                }
                // 无法清除则回滚
                rollback();
                Err(AllocationError::OutOfMemory)
            }
            Err(_) => {
                rollback();
                Err(AllocationError::OutOfMemory)
            }
        }
    }
}

impl Default for KernelMemorySubsystem {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn is_small(layout: Layout) -> bool {
    let aligned = layout.pad_to_align();
    aligned.size() <= MAX_SMALL_SIZE && aligned.align() <= PAGE_SIZE
}

#[inline]
fn is_small_request(request: MemoryRequest) -> bool {
    request.size <= MAX_SMALL_SIZE && request.align <= PAGE_SIZE
}

fn layout_from_request(request: MemoryRequest) -> Result<Layout, AllocationError> {
    Layout::from_size_align(request.size.max(1), request.align.max(1))
        .map_err(|_| AllocationError::InvalidLayout)
}

fn log_request_phase(phase: &str, request: MemoryRequest, active: bool) {
    log::debug!(
        "[alloc][{}] active={} domain={:?} size={} align={} zeroing={:?} reclaim={:?} page_policy={:?} placement={:?} managed={:?}",
        phase,
        active,
        request.domain,
        request.size,
        request.align,
        request.zeroing,
        request.reclaim,
        request.page_policy,
        request.placement,
        request.managed,
    );
}

fn log_record_phase(phase: &str, record: AllocationRecord) {
    match record.paddr {
        Some(paddr) => {
            log::debug!(
                "[alloc][{}] kind={:?} domain={:?} arena={:?} ptr={:#x} paddr={:#x} size={} usable={} align={} order={} page_size={}",
                phase,
                record.kind,
                record.domain,
                record.arena,
                record.ptr,
                paddr,
                record.size,
                record.usable_size,
                record.align,
                record.order,
                record.page_size,
            );
        }
        None => {
            log::debug!(
                "[alloc][{}] kind={:?} domain={:?} arena={:?} ptr={:#x} size={} usable={} align={} order={} page_size={}",
                phase,
                record.kind,
                record.domain,
                record.arena,
                record.ptr,
                record.size,
                record.usable_size,
                record.align,
                record.order,
                record.page_size,
            );
        }
    }
}

fn managed_gc_reclaim(ptr: usize, size: usize) {
    KERNEL_ALLOCATOR.reclaim_managed_from_gc(ptr, size);
}

fn managed_gc_retarget(old_ptr: usize, new_record: AllocationRecord) -> bool {
    KERNEL_ALLOCATOR.retarget_managed_registry(old_ptr, new_record)
}

pub(crate) fn alloc_internal_metadata(layout: Layout) -> *mut u8 {
    KERNEL_ALLOCATOR.allocate_internal_metadata(layout)
}

unsafe impl GlobalAlloc for KernelMemorySubsystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.total_allocs.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_allocated
            .fetch_add(layout.size() as u64, Ordering::Relaxed);

        let ptr = if self.active.load(Ordering::Acquire) {
            self.alloc_active(layout, Zeroing::Uninitialized)
        } else {
            self.boot.alloc(layout)
        };

        if ptr.is_null() {
            self.record_oom();
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }

        self.total_deallocs.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_freed
            .fetch_add(layout.size() as u64, Ordering::Relaxed);

        if !self.active.load(Ordering::Acquire) {
            return;
        }

        if self.deallocate(ptr as usize).is_err() {
            self.record_ownership_failure();
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.total_reallocs.fetch_add(1, Ordering::Relaxed);
        self.kheap.record_realloc();

        if ptr.is_null() {
            let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            return unsafe { self.alloc(new_layout) };
        }

        if new_size == 0 {
            unsafe { self.dealloc(ptr, layout) };
            return null_mut();
        }

        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let active = self.active.load(Ordering::Acquire);
        let owner = if active {
            self.query_allocation(ptr as usize).ok()
        } else if self.boot.contains(ptr as usize) {
            Some(
                AllocationRecord::new(AllocationKind::Boot, MemoryDomain::Kernel, ptr as usize)
                    .with_sizes(layout.size(), layout.size(), layout.align()),
            )
        } else {
            None
        };

        if active
            && owner
                .map(|record| self.can_reuse_allocation(record, new_layout))
                .unwrap_or(false)
        {
            if let Some(mut record) = owner {
                record.size = new_size;
                record.align = new_layout.align();
                if !self.registry.update_existing(ptr as usize, record) {
                    self.record_ownership_failure();
                }
            }
            return ptr;
        }

        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return null_mut();
        }

        let copy_len = layout.size().min(new_size);
        unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr, copy_len) };

        if active {
            match owner {
                Some(record) if record.kind == AllocationKind::Boot => {}
                Some(_) => unsafe { self.dealloc(ptr, layout) },
                None => self.record_ownership_failure(),
            }
        }

        new_ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.total_allocs.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_allocated
            .fetch_add(layout.size() as u64, Ordering::Relaxed);

        let ptr = if self.active.load(Ordering::Acquire) {
            self.alloc_active(layout, Zeroing::Zeroed)
        } else {
            let ptr = self.boot.alloc(layout);
            if !ptr.is_null() {
                unsafe {
                    core::ptr::write_bytes(ptr, 0, layout.size());
                }
            }
            ptr
        };

        if ptr.is_null() {
            self.record_oom();
        }
        ptr
    }
}

#[global_allocator]
pub static KERNEL_ALLOCATOR: KernelMemorySubsystem = KernelMemorySubsystem::new();
