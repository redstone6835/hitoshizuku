//! 内核虚拟地址空间管理层。
//!
//! 这个模块位于“物理页分配器”和“具体对象分配器”之间，负责解决一个核心问题：
//! **某段物理页拿到了之后，应该把它映射到哪段虚拟地址上？**
//!
//! 它把虚拟地址空间按用途拆成多个 arena：
//!
//! - `DirectMap`：直接映射区；
//! - `Kernel`：普通内核堆与大对象区域；
//! - `Managed`：受 GC 管理的虚拟区域。
//!
//! 每次“带后备页帧的分配”都要经过这里完成三步：
//!
//! 1. 在对应 arena 中保留一段虚拟地址范围；
//! 2. 向 buddy 申请与之匹配的物理页；
//! 3. 调用架构层提供的映射回调，把虚拟地址和物理地址绑定起来。
//!
//! 这个层次的意义在于解耦：
//!
//! - 上层 slab / kheap 不需要理解页表细节；
//! - 下层 buddy 不需要理解虚拟地址布局；
//! - 平台相关映射逻辑通过函数指针注入，不污染通用 allocator 代码。
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use spin::mutex::Mutex;

use crate::boot::BootAllocator;
use crate::buddy::{BuddyAllocError, BuddyAllocator, BuddyFreeError, PAGE_SIZE};
use crate::error::AddressSpaceError;
use crate::request::{MemoryPlacement, PagePolicy, PhysicalAllocRequest};
use crate::vmem::{VmemAllocPolicy, VmemArena, VmemStats};

#[inline]
fn now_ns() -> u64 {
    log::get_timestamp_ns()
}

#[inline]
fn elapsed_us(start_ns: u64) -> u64 {
    now_ns().saturating_sub(start_ns) / 1_000
}

/// 虚拟地址 arena 的逻辑分类。
///
/// 这里的分类反映的是“这段虚拟地址将被怎样使用”，而不是某个具体算法。
/// 各个 arena 彼此隔离，可以独立统计、独立保留地址，也便于未来扩展不同权限或策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArenaKind {
    DirectMap,
    Kernel,
    Managed,
}

/// 一段同时具备虚拟地址和物理后备页的区间。
///
/// 这是 `space` 层与更上层 `slab` / `kheap` 之间最核心的交付物：上层不关心页表细节，
/// 只需要知道“我拿到了一段已经有物理后备的可用地址范围”。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackedRange {
    pub arena: ArenaKind,
    pub vaddr: usize,
    pub paddr: usize,
    pub size: usize,
    pub order: usize,
}

/// 地址空间层统计信息。
///
/// 它分别描述 linear-map、kernel、managed 三个 arena 的使用状态，并额外给出
/// managed 子系统是否已经启用。
#[derive(Clone, Copy, Debug, Default)]
pub struct AddressSpaceStats {
    pub direct_map: VmemStats,
    pub kernel: VmemStats,
    pub managed: VmemStats,
    pub managed_enabled: bool,
}

/// 内核虚拟地址空间管理器。
///
/// 这个结构本身不拥有物理页，而是协调多个 `VmemArena`、记录与架构层交互所需的
/// 映射回调、并在分配/释放带后备页的范围时安排“保留虚拟地址 -> 获取物理页 ->
/// 建立映射”这一整套流程。
pub struct KernelAddressSpace {
    direct_map: Mutex<VmemArena>,
    kernel: Mutex<VmemArena>,
    managed: Mutex<VmemArena>,
    initialized: AtomicBool,
    managed_enabled: AtomicBool,
    kernel_direct_map: AtomicBool,
    kernel_virt_to_phys: AtomicUsize,
    kernel_heap_map: AtomicUsize,
    kernel_heap_unmap: AtomicUsize,
}

impl KernelAddressSpace {
    pub const fn new() -> Self {
        Self {
            direct_map: Mutex::new(VmemArena::new()),
            kernel: Mutex::new(VmemArena::new()),
            managed: Mutex::new(VmemArena::new()),
            initialized: AtomicBool::new(false),
            managed_enabled: AtomicBool::new(false),
            kernel_direct_map: AtomicBool::new(false),
            kernel_virt_to_phys: AtomicUsize::new(0),
            kernel_heap_map: AtomicUsize::new(0),
            kernel_heap_unmap: AtomicUsize::new(0),
        }
    }

    pub fn bind_kernel_heap_mapping(
        &self,
        map_fn: crate::MapKernelHeapRangeFn,
        unmap_fn: crate::UnmapKernelHeapRangeFn,
    ) {
        self.kernel_heap_map
            .store(map_fn as usize, Ordering::Release);
        self.kernel_heap_unmap
            .store(unmap_fn as usize, Ordering::Release);
    }

    pub fn init_from_phys(
        &self,
        phys: &BuddyAllocator,
        reserved_phys: &[(usize, usize)],
        phys_to_virt: fn(usize) -> usize,
        virt_to_phys: fn(usize) -> usize,
        kernel_heap_region: (usize, usize),
        _boot: &BootAllocator,
    ) -> Result<(), AddressSpaceError> {
        if !phys.is_initialized() {
            return Err(AddressSpaceError::NotInitialized);
        }

        let init_start_ns = now_ns();
        let direct_map_init_us;
        let direct_map_span_us;
        let mut direct_map_span_count = 0usize;
        let kernel_init_us;
        let mut kernel_span_us = 0u64;
        let mut kernel_span_count = 0usize;
        let managed_init_us;

        {
            let phase_start_ns = now_ns();
            let mut direct_map = self.direct_map.lock();
            if !direct_map.init(b"direct_map", 0, 0, PAGE_SIZE, VmemAllocPolicy::BestFit) {
                self.initialized.store(false, Ordering::Release);
                return Err(AddressSpaceError::MetadataOutOfMemory);
            }
            direct_map_init_us = elapsed_us(phase_start_ns);

            let span_start_ns = now_ns();
            for segment in phys.iter_segments() {
                direct_map_span_count += 1;
                if let Err(err) =
                    direct_map.add_span_result(phys_to_virt(segment.start), segment.size)
                {
                    log::info!(
                        "[alloc][vmem] direct_map add_span failed base={:#x} size={} err={:?}",
                        phys_to_virt(segment.start),
                        segment.size,
                        err,
                    );
                    self.initialized.store(false, Ordering::Release);
                    return Err(match err {
                        crate::error::VmemError::MetadataOutOfMemory => {
                            AddressSpaceError::MetadataOutOfMemory
                        }
                        crate::error::VmemError::Overlap
                        | crate::error::VmemError::InvalidRange => AddressSpaceError::InvalidRange,
                        _ => AddressSpaceError::InvalidRange,
                    });
                }
            }
            direct_map_span_us = elapsed_us(span_start_ns);
            // 当平台提供独立的高半区 kernel heap window 时，allocator 的真实可分配性
            // 由 `kernel` arena + buddy 共同决定，direct_map 只承担“物理 span 视图”
            // 与统计角色。此时再把 kernel/metadata carve-out 逐个同步到 direct_map，
            // 既不会提升分配正确性，反而会把启动路径重新拉回脆弱且高开销的
            // reserve-from-tag 逻辑。只有在 kernel arena 退化为 linear-map 模式时，
            // direct_map 的精确保留才真正影响地址分配结果。
            if kernel_heap_region.1 == 0 {
                for &(start, end) in reserved_phys {
                    if end <= start {
                        continue;
                    }
                    if let Err(err) = direct_map.reserve_result(phys_to_virt(start), end - start) {
                        log::info!(
                            "[alloc][vmem] direct_map reserve failed base={:#x} size={} err={:?}",
                            phys_to_virt(start),
                            end - start,
                            err,
                        );
                        self.initialized.store(false, Ordering::Release);
                        return Err(match err {
                            crate::error::VmemError::MetadataOutOfMemory => {
                                AddressSpaceError::MetadataOutOfMemory
                            }
                            _ => AddressSpaceError::InvalidRange,
                        });
                    }
                }
                for range in phys.iter_metadata_ranges() {
                    if let Err(err) =
                        direct_map.reserve_result(phys_to_virt(range.start), range.size)
                    {
                        log::info!(
                            "[alloc][vmem] direct_map metadata reserve failed base={:#x} size={} err={:?}",
                            phys_to_virt(range.start),
                            range.size,
                            err,
                        );
                        self.initialized.store(false, Ordering::Release);
                        return Err(match err {
                            crate::error::VmemError::MetadataOutOfMemory => {
                                AddressSpaceError::MetadataOutOfMemory
                            }
                            _ => AddressSpaceError::InvalidRange,
                        });
                    }
                }
            }
        }

        {
            let phase_start_ns = now_ns();
            let mut kernel = self.kernel.lock();
            if kernel_heap_region.1 == 0 {
                if !kernel.init(b"kernel_heap", 0, 0, PAGE_SIZE, VmemAllocPolicy::BestFit) {
                    self.initialized.store(false, Ordering::Release);
                    return Err(AddressSpaceError::MetadataOutOfMemory);
                }
                let span_start_ns = now_ns();
                for segment in phys.iter_segments() {
                    kernel_span_count += 1;
                    if let Err(err) =
                        kernel.add_span_result(phys_to_virt(segment.start), segment.size)
                    {
                        log::info!(
                            "[alloc][vmem] kernel add_span failed base={:#x} size={} err={:?}",
                            phys_to_virt(segment.start),
                            segment.size,
                            err,
                        );
                        self.initialized.store(false, Ordering::Release);
                        return Err(match err {
                            crate::error::VmemError::MetadataOutOfMemory => {
                                AddressSpaceError::MetadataOutOfMemory
                            }
                            crate::error::VmemError::Overlap
                            | crate::error::VmemError::InvalidRange => {
                                AddressSpaceError::InvalidRange
                            }
                            _ => AddressSpaceError::InvalidRange,
                        });
                    }
                }
                kernel_span_us = elapsed_us(span_start_ns);
                for &(start, end) in reserved_phys {
                    if end <= start {
                        continue;
                    }
                    if let Err(err) = kernel.reserve_result(phys_to_virt(start), end - start) {
                        log::info!(
                            "[alloc][vmem] kernel reserve failed base={:#x} size={} err={:?}",
                            phys_to_virt(start),
                            end - start,
                            err,
                        );
                        self.initialized.store(false, Ordering::Release);
                        return Err(match err {
                            crate::error::VmemError::MetadataOutOfMemory => {
                                AddressSpaceError::MetadataOutOfMemory
                            }
                            _ => AddressSpaceError::InvalidRange,
                        });
                    }
                }
                for range in phys.iter_metadata_ranges() {
                    if let Err(err) = kernel.reserve_result(phys_to_virt(range.start), range.size) {
                        log::info!(
                            "[alloc][vmem] kernel metadata reserve failed base={:#x} size={} err={:?}",
                            phys_to_virt(range.start),
                            range.size,
                            err,
                        );
                        self.initialized.store(false, Ordering::Release);
                        return Err(match err {
                            crate::error::VmemError::MetadataOutOfMemory => {
                                AddressSpaceError::MetadataOutOfMemory
                            }
                            _ => AddressSpaceError::InvalidRange,
                        });
                    }
                }
                self.kernel_direct_map.store(true, Ordering::Release);
                self.kernel_virt_to_phys
                    .store(virt_to_phys as usize, Ordering::Release);
            } else {
                if !kernel.init(
                    b"kernel_heap",
                    kernel_heap_region.0,
                    kernel_heap_region.1,
                    PAGE_SIZE,
                    VmemAllocPolicy::BestFit,
                ) {
                    self.initialized.store(false, Ordering::Release);
                    return Err(AddressSpaceError::MetadataOutOfMemory);
                }
                if (kernel_heap_region.0 & (PAGE_SIZE - 1)) != 0 {
                    self.initialized.store(false, Ordering::Release);
                    return Err(AddressSpaceError::InvalidRange);
                }
                self.kernel_direct_map.store(false, Ordering::Release);
                self.kernel_virt_to_phys.store(0, Ordering::Release);
            }
            kernel_init_us = elapsed_us(phase_start_ns);
        }

        {
            let phase_start_ns = now_ns();
            let mut managed = self.managed.lock();
            if !managed.init(b"managed_heap", 0, 0, PAGE_SIZE, VmemAllocPolicy::BestFit) {
                self.initialized.store(false, Ordering::Release);
                return Err(AddressSpaceError::MetadataOutOfMemory);
            }
            managed_init_us = elapsed_us(phase_start_ns);
        }

        self.managed_enabled.store(false, Ordering::Release);
        self.initialized.store(true, Ordering::Release);
        let stats = self.snapshot();
        log::info!(
            "[alloc][vmem][timing] total={} us direct_init={} us direct_spans={} us direct_count={} kernel_init={} us kernel_spans={} us kernel_count={} managed_init={} us kernel_window={} MiB",
            elapsed_us(init_start_ns),
            direct_map_init_us,
            direct_map_span_us,
            direct_map_span_count,
            kernel_init_us,
            kernel_span_us,
            kernel_span_count,
            managed_init_us,
            kernel_heap_region.1 / (1024 * 1024),
        );
        log::info!(
            "[alloc][vmem] initialized direct_map_total={} kernel_total={} managed_total={} managed_enabled={}",
            stats.direct_map.total_size,
            stats.kernel.total_size,
            stats.managed.total_size,
            stats.managed_enabled,
        );
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn managed_enabled(&self) -> bool {
        self.managed_enabled.load(Ordering::Acquire)
    }

    pub fn alloc_kernel_backed_range(
        &self,
        order: usize,
        phys: &Mutex<BuddyAllocator>,
        page_policy: PagePolicy,
    ) -> Result<BackedRange, AddressSpaceError> {
        self.alloc_backed_range(ArenaKind::Kernel, order, page_policy, phys)
    }

    pub fn free_kernel_backed_range(
        &self,
        range: BackedRange,
        phys: &Mutex<BuddyAllocator>,
    ) -> Result<(), AddressSpaceError> {
        self.free_backed_range(range, phys)
    }

    pub fn init_managed_heap(
        &self,
        order: usize,
        phys: &Mutex<BuddyAllocator>,
    ) -> Result<BackedRange, AddressSpaceError> {
        if !self.is_initialized() {
            return Err(AddressSpaceError::NotInitialized);
        }
        if self.managed_enabled() {
            return Err(AddressSpaceError::ManagedUnavailable);
        }

        let page_policy = if order >= 9 {
            PagePolicy::PreferLarge
        } else {
            PagePolicy::BaseOnly
        };
        let range = self.alloc_backed_range(ArenaKind::Kernel, order, page_policy, phys)?;
        let add_span_result = {
            let mut managed = self.managed.lock();
            managed.add_span_result(range.vaddr, range.size)
        };
        if let Err(err) = add_span_result {
            let _ = self.free_backed_range(range, phys);
            return Err(match err {
                crate::error::VmemError::MetadataOutOfMemory => {
                    AddressSpaceError::MetadataOutOfMemory
                }
                crate::error::VmemError::Overlap | crate::error::VmemError::InvalidRange => {
                    AddressSpaceError::InvalidRange
                }
                _ => AddressSpaceError::InvalidRange,
            });
        }
        self.managed_enabled.store(true, Ordering::Release);
        Ok(BackedRange {
            arena: ArenaKind::Managed,
            ..range
        })
    }

    pub fn grow_managed_heap_contiguous(
        &self,
        expected_base: usize,
        order: usize,
        phys: &Mutex<BuddyAllocator>,
    ) -> Result<BackedRange, AddressSpaceError> {
        if !self.is_initialized() {
            return Err(AddressSpaceError::NotInitialized);
        }
        if !self.managed_enabled() {
            return Err(AddressSpaceError::ManagedUnavailable);
        }

        let page_policy = if order >= 9 {
            PagePolicy::PreferLarge
        } else {
            PagePolicy::BaseOnly
        };
        let range =
            self.alloc_backed_range_at(ArenaKind::Kernel, expected_base, order, page_policy, phys)?;
        let add_span_result = {
            let mut managed = self.managed.lock();
            managed.add_span_result(range.vaddr, range.size)
        };
        if let Err(err) = add_span_result {
            let _ = self.free_backed_range(range, phys);
            return Err(match err {
                crate::error::VmemError::MetadataOutOfMemory => {
                    AddressSpaceError::MetadataOutOfMemory
                }
                crate::error::VmemError::Overlap | crate::error::VmemError::InvalidRange => {
                    AddressSpaceError::InvalidRange
                }
                _ => AddressSpaceError::InvalidRange,
            });
        }
        Ok(BackedRange {
            arena: ArenaKind::Managed,
            ..range
        })
    }

    pub fn alloc_managed_range(
        &self,
        size: usize,
        align: usize,
    ) -> Result<usize, AddressSpaceError> {
        if !self.managed_enabled() {
            return Err(AddressSpaceError::ManagedUnavailable);
        }
        let result = self
            .managed
            .lock()
            .alloc_result(size, align)
            .map_err(AddressSpaceError::from);
        match result {
            Ok(addr) => Ok(addr),
            Err(err) => Err(err),
        }
    }

    pub fn alloc_managed_range_in(
        &self,
        range_start: usize,
        range_end: usize,
        size: usize,
        align: usize,
    ) -> Result<usize, AddressSpaceError> {
        if !self.managed_enabled() {
            return Err(AddressSpaceError::ManagedUnavailable);
        }
        let result = self
            .managed
            .lock()
            .alloc_in_range_result(range_start, range_end, size, align)
            .map_err(AddressSpaceError::from);
        match result {
            Ok(addr) => Ok(addr),
            Err(err) => Err(err),
        }
    }

    pub fn free_managed_range(&self, addr: usize, size: usize) -> Result<(), AddressSpaceError> {
        if !self.managed_enabled() {
            return Err(AddressSpaceError::ManagedUnavailable);
        }
        self.managed
            .lock()
            .free_result(addr, size)
            .map_err(AddressSpaceError::from)?;
        Ok(())
    }

    pub fn kernel_range_allocated(&self, addr: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        self.kernel.lock().is_allocated(addr)
    }

    pub fn managed_range_allocated(&self, addr: usize) -> bool {
        if !self.managed_enabled() {
            return false;
        }
        self.managed.lock().is_allocated(addr)
    }

    pub fn snapshot(&self) -> AddressSpaceStats {
        AddressSpaceStats {
            direct_map: self.direct_map.lock().stats(),
            kernel: self.kernel.lock().stats(),
            managed: self.managed.lock().stats(),
            managed_enabled: self.managed_enabled(),
        }
    }

    pub fn kernel_stats(&self) -> VmemStats {
        self.kernel.lock().stats()
    }

    pub fn direct_map_stats(&self) -> VmemStats {
        self.direct_map.lock().stats()
    }

    pub fn managed_stats(&self) -> VmemStats {
        self.managed.lock().stats()
    }

    #[inline]
    fn arena_lock(&self, arena: ArenaKind) -> &Mutex<VmemArena> {
        match arena {
            ArenaKind::DirectMap => &self.direct_map,
            ArenaKind::Kernel => &self.kernel,
            ArenaKind::Managed => &self.managed,
        }
    }

    fn load_kernel_heap_map_fn(&self) -> Option<crate::MapKernelHeapRangeFn> {
        let raw = self.kernel_heap_map.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, crate::MapKernelHeapRangeFn>(raw) })
        }
    }

    fn load_kernel_heap_unmap_fn(&self) -> Option<crate::UnmapKernelHeapRangeFn> {
        let raw = self.kernel_heap_unmap.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, crate::UnmapKernelHeapRangeFn>(raw) })
        }
    }

    fn load_kernel_virt_to_phys_fn(&self) -> Option<fn(usize) -> usize> {
        let raw = self.kernel_virt_to_phys.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, fn(usize) -> usize>(raw) })
        }
    }

    fn alloc_backed_range(
        &self,
        arena: ArenaKind,
        order: usize,
        page_policy: PagePolicy,
        phys: &Mutex<BuddyAllocator>,
    ) -> Result<BackedRange, AddressSpaceError> {
        if !self.is_initialized() {
            return Err(AddressSpaceError::NotInitialized);
        }

        let order = effective_order_for_page_policy(order, page_policy);
        let block_pages = 1usize << order;
        let size = block_pages * PAGE_SIZE;

        // 第一步：分配虚拟地址（短暂持有 arena_lock）
        let vaddr = {
            let arena_lock = self.arena_lock(arena);
            let mut arena_state = arena_lock.lock();
            match arena_state.alloc(size, size) {
                Some(vaddr) => vaddr,
                None => {
                    return Err(AddressSpaceError::OutOfVirtualAddressSpace);
                }
            }
        }; // arena_lock released here

        let placement =
            if arena == ArenaKind::Kernel && self.kernel_direct_map.load(Ordering::Acquire) {
                let Some(virt_to_phys) = self.load_kernel_virt_to_phys_fn() else {
                    let _ = self.arena_lock(arena).lock().free(vaddr, size);
                    return Err(AddressSpaceError::MappingUnavailable);
                };
                MemoryPlacement::ExactPhys(virt_to_phys(vaddr))
            } else {
                MemoryPlacement::Any
            };

        // 第二步：分配物理页（短暂持有 phys 锁）
        let allocation = {
            let mut phys = phys.lock();
            phys.alloc_pages_with(
                &PhysicalAllocRequest::new(size, size)
                    .with_page_policy(page_policy)
                    .with_placement(placement),
            )
        }; // phys lock released here

        let allocation = match allocation {
            Ok(alloc) => alloc,
            Err(_) => {
                // 回滚：释放虚拟地址
                let _ = self.arena_lock(arena).lock().free(vaddr, size);
                return Err(AddressSpaceError::PhysicalRangeUnavailable);
            }
        };

        // 第三步：建立页映射（不持有任何锁 — 可安全调用回调）
        if arena != ArenaKind::DirectMap
            && !(arena == ArenaKind::Kernel && self.kernel_direct_map.load(Ordering::Acquire))
        {
            let Some(map_fn) = self.load_kernel_heap_map_fn() else {
                // 回滚：同时释放虚拟地址和物理页
                let _ = self.arena_lock(arena).lock().free(vaddr, size);
                let mut phys = phys.lock();
                let _ = phys.free_pages(allocation.paddr, allocation.order);
                return Err(AddressSpaceError::MappingUnavailable);
            };

            // 在没有任何 allocator 锁的情况下调用 map_fn
            if !map_fn(vaddr, allocation.paddr, allocation.size, page_policy) {
                // 回滚：同时释放虚拟地址和物理页
                let _ = self.arena_lock(arena).lock().free(vaddr, size);
                let mut phys = phys.lock();
                let _ = phys.free_pages(allocation.paddr, allocation.order);
                return Err(AddressSpaceError::MappingFailed);
            }
        }

        let backed = BackedRange {
            arena,
            vaddr,
            paddr: allocation.paddr,
            size: allocation.size, // 使用 buddy 分配器返回的 allocation.size
            order: allocation.order,
        };
        Ok(backed)
    }

    fn alloc_backed_range_at(
        &self,
        arena: ArenaKind,
        vaddr: usize,
        order: usize,
        page_policy: PagePolicy,
        phys: &Mutex<BuddyAllocator>,
    ) -> Result<BackedRange, AddressSpaceError> {
        if !self.is_initialized() {
            return Err(AddressSpaceError::NotInitialized);
        }

        let order = effective_order_for_page_policy(order, page_policy);
        let block_pages = 1usize << order;
        let size = block_pages * PAGE_SIZE;

        {
            let arena_lock = self.arena_lock(arena);
            let mut arena_state = arena_lock.lock();
            arena_state
                .reserve_result(vaddr, size)
                .map_err(AddressSpaceError::from)?;
        }

        let placement =
            if arena == ArenaKind::Kernel && self.kernel_direct_map.load(Ordering::Acquire) {
                let Some(virt_to_phys) = self.load_kernel_virt_to_phys_fn() else {
                    let _ = self.arena_lock(arena).lock().free(vaddr, size);
                    return Err(AddressSpaceError::MappingUnavailable);
                };
                MemoryPlacement::ExactPhys(virt_to_phys(vaddr))
            } else {
                MemoryPlacement::Any
            };

        let allocation = {
            let mut phys = phys.lock();
            phys.alloc_pages_with(
                &PhysicalAllocRequest::new(size, size)
                    .with_page_policy(page_policy)
                    .with_placement(placement),
            )
        };

        let allocation = match allocation {
            Ok(alloc) => alloc,
            Err(_) => {
                let _ = self.arena_lock(arena).lock().free(vaddr, size);
                return Err(AddressSpaceError::PhysicalRangeUnavailable);
            }
        };

        if arena != ArenaKind::DirectMap
            && !(arena == ArenaKind::Kernel && self.kernel_direct_map.load(Ordering::Acquire))
        {
            let Some(map_fn) = self.load_kernel_heap_map_fn() else {
                let _ = self.arena_lock(arena).lock().free(vaddr, size);
                let mut phys = phys.lock();
                let _ = phys.free_pages(allocation.paddr, allocation.order);
                return Err(AddressSpaceError::MappingUnavailable);
            };
            if !map_fn(vaddr, allocation.paddr, allocation.size, page_policy) {
                let _ = self.arena_lock(arena).lock().free(vaddr, size);
                let mut phys = phys.lock();
                let _ = phys.free_pages(allocation.paddr, allocation.order);
                return Err(AddressSpaceError::MappingFailed);
            }
        }

        let backed = BackedRange {
            arena,
            vaddr,
            paddr: allocation.paddr,
            size: allocation.size,
            order: allocation.order,
        };
        Ok(backed)
    }

    fn free_backed_range(
        &self,
        range: BackedRange,
        phys: &Mutex<BuddyAllocator>,
    ) -> Result<(), AddressSpaceError> {
        if !self.is_initialized() {
            return Err(AddressSpaceError::NotInitialized);
        }

        if range.arena != ArenaKind::DirectMap
            && !(range.arena == ArenaKind::Kernel && self.kernel_direct_map.load(Ordering::Acquire))
        {
            let Some(unmap_fn) = self.load_kernel_heap_unmap_fn() else {
                panic!(
                    "[alloc][vmem] backed free missing unmap arena={:?} vaddr={:#x} size={}",
                    range.arena, range.vaddr, range.size,
                );
            };
            if !unmap_fn(range.vaddr, range.size) {
                panic!(
                    "[alloc][invariant] backed free unmap failed arena={:?} vaddr={:#x} size={}",
                    range.arena, range.vaddr, range.size,
                );
            }
        }

        let arena_lock = self.arena_lock(range.arena);
        if !arena_lock.lock().free(range.vaddr, range.size) {
            panic!(
                "[alloc][invariant] backed free arena release failed arena={:?} vaddr={:#x} size={}",
                range.arena, range.vaddr, range.size,
            );
        }

        let phys_result = {
            let mut phys = phys.lock();
            phys.free_pages(range.paddr, range.order)
        };
        if phys_result.is_err() {
            panic!(
                "[alloc][invariant] backed free phys release failed arena={:?} vaddr={:#x} paddr={:#x} order={} size={}",
                range.arena, range.vaddr, range.paddr, range.order, range.size,
            );
        }

        Ok(())
    }
}

impl Default for KernelAddressSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl From<BuddyAllocError> for AddressSpaceError {
    fn from(_: BuddyAllocError) -> Self {
        AddressSpaceError::PhysicalRangeUnavailable
    }
}

impl From<BuddyFreeError> for AddressSpaceError {
    fn from(_: BuddyFreeError) -> Self {
        AddressSpaceError::PhysicalReleaseFailed
    }
}

fn effective_order_for_page_policy(order: usize, page_policy: PagePolicy) -> usize {
    const MIN_LARGE_PAGE_ORDER: usize = 9; // 2 MiB

    if matches!(page_policy, PagePolicy::RequireLarge) {
        order.max(MIN_LARGE_PAGE_ORDER)
    } else {
        order
    }
}
