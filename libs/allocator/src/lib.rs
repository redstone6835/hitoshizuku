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
//! - `registry` / `metadata` 负责内部记账与自举支持。
//!
//! 因为它是分层系统，所以这里最重要的不是某一个算法，而是层之间的调用顺序、
//! 锁顺序和职责边界。文件头下面的 lock ordering 说明，就是为了保证这些层在组合
//! 起来以后仍然能稳定工作。
//!
//! TODO(alloc-stabilization): 本轮收口后仍需继续完成以下事项：
//! 1. 把 `loongarch64-unknown-none` QEMU allocator-bench 的关键 ns/op 数据纳入持续
//!    回归，避免后续优化只看单次日志；
//! 2. 引入多核压力测试，验证 registry shard、slab per-CPU cache 和回收路径无竞态；
//! 3. 评估 NUMA/per-CPU page cache、自定义 arena/policy 注册等扩展 API 是否需要进入
//!    对外稳定接口。
//!
//! # 锁顺序 (Lock Ordering)
//!
//! 为防止死锁，必须严格按照以下顺序获取锁：
//!
//! 1. `init_lock` — 初始化专用，正常运行期间绝不持有
//! 2. `vmem` arena 锁 (direct_map, kernel) — 虚拟地址空间
//! 3. `metadata.inner` — 元数据分配器状态
//! 4. `phys` (BuddyAllocator) — 物理内存分配器
//! 5. `registry.shard.inner` — 分片分配注册表
//! 6. `slab.state` — Slab 分配器状态
//! 7. `slab.cache.inner` — Per-CPU 缓存
//! 8. `kheap.inner` — 大对象分配器
//!
//! ## 关键规则
//!
//! - **调用回调 (map_fn, unmap_fn) 时绝不持有 allocator 内部锁**
//! - **持有 `phys` 时不要进入 vmem；vmem 元数据扩容可能需要临时申请物理页**
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
//!     let mut phys = self.phys.lock();
//!     phys.alloc_pages(order)
//! }; // 锁已释放
//!
//! // 此时可在不持有任何锁的情况下调用 map_fn
//! map_fn(vaddr, paddr, size);
//! ```
//!
//! 错误做法：
//! ```rust
//! let phys = self.phys.lock();
//! let arena = self.arena.lock(); // 持有 phys 时进入 vmem，可能与 metadata 扩容死锁
//! let paddr = phys.alloc_pages(order);
//! map_fn(vaddr, paddr, size); // 持锁时调用回调，也会放大死锁窗口
//! ```

mod boot;
mod buddy;
#[path = "kernel_symbols.rs"]
mod direct_symbols;
mod error;
#[doc(hidden)]
pub use direct_symbols::catalog_anchor as kernel_symbol_catalog_anchor;
mod kheap;
mod metadata;
mod owner_index;
mod registry;
mod request;
mod slab;
mod space;
pub mod stats;
mod vmem;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use spin::relax::RelaxStrategy;

/// allocator 内部锁竞争时的架构紧急工作轮询器。
///
/// 内核堆回收会在释放 allocator 锁后同步等待全核 TLB 失效；另一 CPU 若正等待
/// allocator 锁且本地中断暂时关闭，纯自旋会形成锁环。自定义 relax 在每次竞争
/// 迭代处理一次已注册的无分配回调，使 shootdown 可以完成。
pub struct AllocatorRelax;

pub(crate) type Mutex<T> = spin::mutex::Mutex<T, AllocatorRelax>;

static URGENT_POLL_FN: AtomicUsize = AtomicUsize::new(0);
static URGENT_PENDING_PTR: AtomicUsize = AtomicUsize::new(0);
static URGENT_PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);

impl RelaxStrategy for AllocatorRelax {
    #[inline]
    fn relax() {
        let pending_ptr = URGENT_PENDING_PTR.load(Ordering::Acquire);
        let pending_count = URGENT_PENDING_COUNT.load(Ordering::Relaxed);
        if pending_ptr != 0 && pending_count != 0 {
            let cpu = KERNEL_ALLOCATOR.current_cpu_id().min(pending_count - 1);
            // Safety: bind_urgent_poll 保存的是静态 AtomicBool slice，长度单独发布。
            let pending = unsafe { &*(pending_ptr as *const AtomicBool).add(cpu) };
            if !pending.load(Ordering::Acquire) {
                core::hint::spin_loop();
                return;
            }
            let raw = URGENT_POLL_FN.load(Ordering::Acquire);
            if raw == 0 {
                core::hint::spin_loop();
                return;
            }
            // Safety: `bind_urgent_poll` 只接受静态函数地址，注册后不再撤销。
            let poll = unsafe { core::mem::transmute::<usize, UrgentPollFn>(raw) };
            poll();
        }
        core::hint::spin_loop();
    }
}

const OWNED_ALLOCATION_LOCK_COUNT: usize = 64;
static OWNED_ALLOCATION_LOCKS: [Mutex<()>; OWNED_ALLOCATION_LOCK_COUNT] =
    [const { Mutex::new(()) }; OWNED_ALLOCATION_LOCK_COUNT];

use boot::BootAllocator;
use buddy::BuddyAllocator;
use kheap::KernelHeap;
use metadata::MetadataAllocator;
use owner_index::OwnerAllocationIndex;
use registry::AllocationRegistry;
use slab::SlabAllocator;

pub use buddy::{
    BuddyAllocError as PhysicalAllocError, BuddyAllocator as PhysicalAllocator, BuddyAudit,
    BuddyAuditFlags, BuddyReclaimStats, BuddySnapshot, BuddyStats, MemorySegment, PAGE_SIZE,
};
pub use error::{
    AddressSpaceError, AllocationError, DeallocationError, InitError, OwnedAllocationError,
    OwnershipError, PhysicalFreeError, RegistryError, VmemError,
};
pub use kheap::{
    KernelHeap as LargeObjectAllocator, KernelHeapAudit, KernelHeapAuditFlags,
    KernelHeapReclaimStats, KernelHeapStats,
};
pub use metadata::MetadataStats;
pub use owner_index::{
    OwnerAllocationIndex as AllocationOwnerIndex, OwnerIndexAudit, OwnerIndexAuditFlags,
    OwnerIndexError,
};
pub use registry::{
    AllocationOwnerStats, AllocationRegistryAudit, AllocationRegistryAuditFlags,
    AllocationRegistrySnapshot, AllocationRegistryStats, RegistryPathCounters,
};
pub use request::{
    AllocationArena, AllocationKind, AllocationRecord, AllocationRequestError, MemoryDomain,
    MemoryPlacement, MemoryRequest, PagePolicy, PhysicalAllocRequest, PhysicalAllocation,
    ReclaimPolicy, Zeroing,
};
#[cfg(feature = "performance-profile")]
pub use slab::SlabProfileCounter;
pub use slab::{
    MAX_CPUS, MAX_SMALL_SIZE, SLAB_SIZE_CLASS_COUNT, SlabAllocator as ZoneAllocator, SlabAudit,
    SlabAuditFlags, SlabClassStat, SlabReclaimStats, SlabStats,
};
pub use space::{AddressSpaceStats, ArenaKind, BackedRange, KernelAddressSpace};
pub use stats::{
    ALLOCATOR_API_VERSION, AllocatorAudit, AllocatorAuditFlags, AllocatorAuditScope,
    AllocatorCapabilities, AllocatorCapabilityFlags, AllocatorHotspotSummary,
    AllocatorReclaimRequest, AllocatorReclaimStats,
};
pub use vmem::{VmemAllocPolicy, VmemStats, VmemValidationStats};

pub type PhysicalMemoryManager = BuddyAllocator;
pub type AddressSpaceManager = KernelAddressSpace;
pub type MappedRange = BackedRange;

pub type PhysToVirtFn = fn(paddr: usize) -> usize;
pub type VirtToPhysFn = fn(vaddr: usize) -> usize;
pub type CpuIdFn = fn() -> usize;
pub type UrgentPollFn = fn();
pub type KernelHeapRegionFn = fn() -> (usize, usize);
pub type MapKernelHeapRangeFn =
    fn(vaddr: usize, paddr: usize, size: usize, page_policy: PagePolicy) -> bool;
pub type UnmapKernelHeapRangeFn = fn(vaddr: usize, size: usize) -> bool;

/// 可选的外部分配计量后端。
///
/// allocator 只传递无语义的所有者编号和字节数，不依赖任何上层扩展框架。所有回调都
/// 可能出现在分配热路径或 allocator 内部锁附近，因此实现不得分配内存、阻塞或重入
/// allocator。
#[derive(Clone, Copy)]
pub struct AllocationAccountingOps {
    pub current_owner: fn() -> u64,
    pub try_reserve: fn(owner: u64, bytes: u64) -> bool,
    pub try_resize: fn(owner: u64, old_bytes: u64, new_bytes: u64) -> bool,
    pub release: fn(owner: u64, bytes: u64),
}

static ALLOCATION_ACCOUNTING_OPS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATION_ACCOUNTING_SUSPEND_DEPTH: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];

/// 暂停当前 CPU 上由执行上下文推导出的隐式分配计量。
///
/// trap、故障恢复和其它内核基础设施路径可以使用该守卫，避免把内核自身的延迟分配
/// 错记到被中断的扩展单元。显式指定 `accounting_owner` 的请求不受影响。
#[must_use = "计量暂停守卫必须保持到内核基础设施路径结束"]
pub struct AllocationAccountingSuspension {
    cpu_id: usize,
}

impl Drop for AllocationAccountingSuspension {
    fn drop(&mut self) {
        let result = ALLOCATION_ACCOUNTING_SUSPEND_DEPTH[self.cpu_id].fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |depth| depth.checked_sub(1),
        );
        debug_assert!(result.is_ok(), "隐式分配计量暂停深度发生下溢");
    }
}

/// 在当前 CPU 上暂停隐式分配计量。
///
/// 该守卫只适合不会迁移 CPU 的短临界路径；架构 trap 入口满足这一约束。
pub fn suspend_implicit_allocation_accounting() -> Option<AllocationAccountingSuspension> {
    let cpu_id = KERNEL_ALLOCATOR.current_cpu_id().min(MAX_CPUS - 1);
    ALLOCATION_ACCOUNTING_SUSPEND_DEPTH[cpu_id]
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
            depth.checked_add(1)
        })
        .ok()?;
    Some(AllocationAccountingSuspension { cpu_id })
}

/// 安装唯一的外部分配计量后端。
///
/// 重复安装同一个静态表是幂等操作；尝试替换已经生效的后端会失败，避免活跃分配在
/// 两套账本之间失去归属。
pub fn register_allocation_accounting_ops(ops: &'static AllocationAccountingOps) -> bool {
    let address = ops as *const AllocationAccountingOps as usize;
    match ALLOCATION_ACCOUNTING_OPS.compare_exchange(
        0,
        address,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => true,
        Err(current) => current == address,
    }
}

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
}

pub struct KernelMemorySubsystem {
    boot: BootAllocator,
    phys: Mutex<BuddyAllocator>,
    vmem: KernelAddressSpace,
    kheap: KernelHeap,
    tracked_kheap: KernelHeap,
    slab: SlabAllocator,
    tracked_slab: SlabAllocator,
    metadata: MetadataAllocator,
    registry: AllocationRegistry,
    owner_index: OwnerAllocationIndex,
    init_lock: Mutex<()>,
    active: AtomicBool,
    phys_to_virt: AtomicUsize,
    virt_to_phys: AtomicUsize,
    cpu_id_fn: AtomicUsize,
    kernel_heap_region_fn: AtomicUsize,
    tracked_heap_region_fn: AtomicUsize,
    tracked_heap_start: AtomicUsize,
    tracked_heap_size: AtomicUsize,
    kernel_heap_map_fn: AtomicUsize,
    kernel_heap_unmap_fn: AtomicUsize,
    total_allocs: AtomicU64,
    total_deallocs: AtomicU64,
    total_reallocs: AtomicU64,
    total_bytes_allocated: AtomicU64,
    total_bytes_freed: AtomicU64,
    oom_count: AtomicU64,
    ownership_failures: AtomicU64,
}

pub type KernelAllocator = KernelMemorySubsystem;

unsafe impl Sync for KernelMemorySubsystem {}

enum TrackedReallocProbe {
    Updated {
        old_size: usize,
        record: AllocationRecord,
    },
    NeedsMove(AllocationRecord),
    Untracked,
    QuotaDenied,
}

fn allocation_accounting_ops() -> Option<&'static AllocationAccountingOps> {
    let address = ALLOCATION_ACCOUNTING_OPS.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }
    // 安全性：该原子只保存 `register_allocation_accounting_ops` 接收的静态表地址，
    // 注册后永不替换或释放。
    Some(unsafe { &*(address as *const AllocationAccountingOps) })
}

fn resolve_accounting_owner(explicit: Option<u64>) -> u64 {
    if let Some(owner) = explicit {
        return owner;
    }
    let cpu_id = KERNEL_ALLOCATOR.current_cpu_id().min(MAX_CPUS - 1);
    if ALLOCATION_ACCOUNTING_SUSPEND_DEPTH[cpu_id].load(Ordering::Acquire) != 0 {
        return 0;
    }
    allocation_accounting_ops()
        .map(|ops| (ops.current_owner)())
        .unwrap_or(0)
}

#[inline]
fn owned_allocation_lock_index(ptr: usize) -> usize {
    (ptr >> 4) & (OWNED_ALLOCATION_LOCK_COUNT - 1)
}

#[inline]
fn ranges_overlap(
    first_start: usize,
    first_len: usize,
    second_start: usize,
    second_len: usize,
) -> bool {
    let Some(first_end) = first_start.checked_add(first_len) else {
        return true;
    };
    let Some(second_end) = second_start.checked_add(second_len) else {
        return true;
    };
    first_start < second_end && second_start < first_end
}

fn map_owned_allocation_error(error: AllocationError) -> OwnedAllocationError {
    match error {
        AllocationError::NotInitialized => OwnedAllocationError::Unavailable,
        AllocationError::InvalidLayout => OwnedAllocationError::InvalidRequest,
        AllocationError::OutOfMemory => OwnedAllocationError::OutOfMemory,
        AllocationError::AddressSpace(_) => OwnedAllocationError::BackendFailure,
    }
}

fn try_reserve_accounting(owner: u64, bytes: usize) -> bool {
    owner == 0
        || allocation_accounting_ops()
            .map(|ops| (ops.try_reserve)(owner, bytes as u64))
            .unwrap_or(true)
}

fn try_resize_accounting(owner: u64, old_bytes: usize, new_bytes: usize) -> bool {
    owner == 0
        || allocation_accounting_ops()
            .map(|ops| (ops.try_resize)(owner, old_bytes as u64, new_bytes as u64))
            .unwrap_or(true)
}

fn release_accounting(owner: u64, bytes: usize) {
    if owner != 0
        && let Some(ops) = allocation_accounting_ops()
    {
        (ops.release)(owner, bytes as u64);
    }
}

impl KernelMemorySubsystem {
    pub const fn new() -> Self {
        Self {
            boot: BootAllocator::new(),
            phys: Mutex::new(BuddyAllocator::new()),
            vmem: KernelAddressSpace::new(),
            kheap: KernelHeap::new(crate::space::ArenaKind::Kernel),
            tracked_kheap: KernelHeap::new(crate::space::ArenaKind::Tracked),
            slab: SlabAllocator::new(crate::space::ArenaKind::Kernel),
            tracked_slab: SlabAllocator::new(crate::space::ArenaKind::Tracked),
            metadata: MetadataAllocator::new(),
            registry: AllocationRegistry::new(),
            owner_index: OwnerAllocationIndex::new(),
            init_lock: Mutex::new(()),
            active: AtomicBool::new(false),
            phys_to_virt: AtomicUsize::new(0),
            virt_to_phys: AtomicUsize::new(0),
            cpu_id_fn: AtomicUsize::new(0),
            kernel_heap_region_fn: AtomicUsize::new(0),
            tracked_heap_region_fn: AtomicUsize::new(0),
            tracked_heap_start: AtomicUsize::new(0),
            tracked_heap_size: AtomicUsize::new(0),
            kernel_heap_map_fn: AtomicUsize::new(0),
            kernel_heap_unmap_fn: AtomicUsize::new(0),
            total_allocs: AtomicU64::new(0),
            total_deallocs: AtomicU64::new(0),
            total_reallocs: AtomicU64::new(0),
            total_bytes_allocated: AtomicU64::new(0),
            total_bytes_freed: AtomicU64::new(0),
            oom_count: AtomicU64::new(0),
            ownership_failures: AtomicU64::new(0),
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

    /// 注册 allocator 自旋锁竞争时使用的无分配紧急工作回调。
    ///
    /// 该回调可能在任意 allocator 锁的等待路径执行，必须只使用原子状态或架构
    /// 指令，不能分配、阻塞或再次获取 allocator 锁。
    pub fn bind_urgent_poll(&self, pending: &'static [AtomicBool], poll: UrgentPollFn) {
        assert!(
            !pending.is_empty(),
            "allocator urgent pending slice is empty"
        );
        let address = poll as usize;
        match URGENT_POLL_FN.compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {}
            Err(existing) => {
                assert_eq!(existing, address, "allocator urgent poll hook replaced");
            }
        }
        let pending_address = pending.as_ptr() as usize;
        match URGENT_PENDING_PTR.compare_exchange(
            0,
            pending_address,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => URGENT_PENDING_COUNT.store(pending.len(), Ordering::Release),
            Err(existing) => {
                assert_eq!(
                    existing, pending_address,
                    "allocator urgent pending source replaced"
                );
                assert_eq!(
                    URGENT_PENDING_COUNT.load(Ordering::Acquire),
                    pending.len(),
                    "allocator urgent pending length changed"
                );
            }
        }
    }

    pub fn bind_kernel_heap_ops(
        &self,
        region_fn: KernelHeapRegionFn,
        tracked_region_fn: KernelHeapRegionFn,
        map_fn: MapKernelHeapRangeFn,
        unmap_fn: UnmapKernelHeapRangeFn,
    ) {
        let (tracked_heap_start, tracked_heap_size) = tracked_region_fn();
        self.kernel_heap_region_fn
            .store(region_fn as usize, Ordering::Release);
        self.tracked_heap_start
            .store(tracked_heap_start, Ordering::Relaxed);
        self.tracked_heap_size
            .store(tracked_heap_size, Ordering::Relaxed);
        self.tracked_heap_region_fn
            .store(tracked_region_fn as usize, Ordering::Release);
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
        let Some(_) = self.load_tracked_heap_region_fn() else {
            return Err(InitError::MissingKernelHeapRegion);
        };
        let tracked_heap_region = self.cached_tracked_heap_region();

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
                tracked_heap_region,
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
        self.tracked_kheap.init();
    }

    pub fn init_slab(&self, cpu_count: usize) {
        let _guard = self.init_lock.lock();
        let kernel_region = self
            .load_kernel_heap_region_fn()
            .map(|region| region())
            .unwrap_or((0, 0));
        let tracked_region = self
            .load_tracked_heap_region_fn()
            .map(|region| region())
            .unwrap_or((0, 0));
        self.slab.init(cpu_count, kernel_region);
        self.tracked_slab.init(cpu_count, tracked_region);
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
        if !self.tracked_kheap.is_initialized() {
            return Err(InitError::LargeAllocatorNotInitialized);
        }
        if !self.slab.is_initialized() {
            return Err(InitError::ZoneNotInitialized);
        }
        if !self.tracked_slab.is_initialized() {
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
        if !self.owner_index.init() {
            return Err(InitError::MetadataOutOfMemory);
        }
        if let Some(range) = self.boot.seal_and_take_free_tail(PAGE_SIZE) {
            if let Some(virt_to_phys) = self.load_virt_to_phys() {
                let paddr = virt_to_phys(range.start);
                match self.phys.lock().release_reserved_range(paddr, range.size) {
                    Ok(pages) => {
                        log::info!(
                            "[alloc][boot] released boot tail vaddr={:#x}..{:#x} paddr={:#x} pages={} bytes={}",
                            range.start,
                            range.end(),
                            paddr,
                            pages,
                            pages * PAGE_SIZE
                        );
                    }
                    Err(err) => {
                        log::warning!(
                            "[alloc][boot] failed to release boot tail vaddr={:#x}..{:#x} paddr={:#x} size={} err={:?}",
                            range.start,
                            range.end(),
                            paddr,
                            range.size,
                            err
                        );
                    }
                }
            } else {
                log::warning!(
                    "[alloc][boot] failed to release boot tail vaddr={:#x}..{:#x}: missing virt_to_phys",
                    range.start,
                    range.end()
                );
            }
        }
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// 返回 allocator 对外稳定能力快照。
    ///
    /// 该接口不扫描内部结构、不分配内存，适合 LKM/外部子系统在初始化时判断当前内核是否
    /// 支持 typed physical API、结构审计、cache reclaim 等能力。功能新增时
    /// 应增加 capability bit；破坏性 ABI 变化才递增 [`ALLOCATOR_API_VERSION`]。
    pub fn capabilities(&self) -> AllocatorCapabilities {
        AllocatorCapabilities {
            api_version: ALLOCATOR_API_VERSION,
            flags: AllocatorCapabilityFlags::stable_kernel(),
            max_small_size: MAX_SMALL_SIZE,
            max_cpus: MAX_CPUS,
            page_size: PAGE_SIZE,
        }
    }

    pub fn stats(&self) -> AllocStats {
        let boot = self.boot.snapshot();
        let address_space = self.vmem.snapshot();
        let slab = self.combined_slab_stats();
        let kheap = self.combined_kheap_stats();
        AllocStats {
            total_allocs: self.total_allocs.load(Ordering::Acquire),
            total_deallocs: self.total_deallocs.load(Ordering::Acquire),
            total_reallocs: self.total_reallocs.load(Ordering::Acquire),
            total_bytes_allocated: self.total_bytes_allocated.load(Ordering::Acquire),
            total_bytes_freed: self.total_bytes_freed.load(Ordering::Acquire),
            oom_count: self.oom_count.load(Ordering::Acquire),
            ownership_failures: self.ownership_failures.load(Ordering::Acquire),
            boot_used_bytes: boot.used_bytes,
            vmem_used_bytes: address_space
                .kernel
                .allocated_size
                .saturating_add(address_space.tracked.allocated_size),
            active_small_allocs: slab.active_objects,
            active_large_allocs: kheap.active_allocs,
        }
    }

    pub fn detailed_stats(&self) -> stats::MemoryOverview {
        let boot = self.boot.snapshot();
        let phys = self.buddy_stats();
        let address_space = self.address_space_stats();
        let kheap = self.combined_kheap_stats();
        let slab = self.combined_slab_stats();
        stats::build_overview(boot, phys, address_space, kheap, slab)
    }

    fn combined_kheap_stats(&self) -> KernelHeapStats {
        let mut stats = self.kheap.snapshot();
        stats.merge(self.tracked_kheap.snapshot());
        stats
    }

    fn combined_slab_stats(&self) -> SlabStats {
        let mut stats = self.slab.snapshot();
        stats.merge(self.tracked_slab.snapshot());
        stats
    }

    fn combined_kheap_audit(&self) -> KernelHeapAudit {
        let mut audit = self.kheap.audit();
        audit.merge(self.tracked_kheap.audit());
        audit
    }

    fn combined_slab_audit(&self) -> SlabAudit {
        let mut audit = self.slab.audit();
        audit.merge(self.tracked_slab.audit());
        audit
    }

    pub fn layer_stats(&self) -> stats::AllocatorLayerStats {
        self.layer_stats_with_physical_registry(self.buddy_stats(), self.registry.stats())
    }

    fn layer_stats_with_registry(
        &self,
        registry: AllocationRegistryStats,
    ) -> stats::AllocatorLayerStats {
        self.layer_stats_with_physical_registry(self.buddy_stats(), registry)
    }

    fn layer_stats_with_physical_registry(
        &self,
        phys: BuddyStats,
        registry: AllocationRegistryStats,
    ) -> stats::AllocatorLayerStats {
        stats::AllocatorLayerStats {
            phys,
            address_space: self.address_space_stats(),
            kheap: self.combined_kheap_stats(),
            slab: self.combined_slab_stats(),
            tracked_kheap: self.tracked_kheap.snapshot(),
            tracked_slab: self.tracked_slab.snapshot(),
            metadata: self.metadata.stats(),
            registry,
        }
    }

    pub fn pressure_level(&self) -> u8 {
        self.detailed_stats().pressure_level
    }

    /// 返回 allocator 当前热点摘要。
    ///
    /// 这个接口复用各层现有计数器，不做对象扫描，也不生成文本。bench、调试命令和未来
    /// LKM 风格扩展可以通过它稳定读取 cache 命中率、registry 链长、vmem 碎片等成本来源，
    /// 不需要解析 `format_diagnostic()` 的输出。
    pub fn hotspot_summary(&self) -> AllocatorHotspotSummary {
        let layers = self.layer_stats_with_registry(self.registry.stats());
        stats::build_hotspot_summary(&layers)
    }

    pub fn format_diagnostic(&self, buf: &mut [u8]) -> usize {
        self.format_diagnostic_with_scope(buf, AllocatorAuditScope::FullRegistry)
    }

    /// 使用指定自检范围格式化 allocator 诊断文本。
    ///
    /// `FullRegistry` 保留旧接口语义，会扫描 registry 链表并输出 `reg_struct/scan/chain`；
    /// `CountersOnly` 只读取各层 O(1) 计数器，适合高频日志、监控和未来外部扩展的低扰动
    /// 快照。诊断文本里的 `mode=` 字段会明确标明本次采样范围。
    pub fn format_diagnostic_with_scope(
        &self,
        buf: &mut [u8],
        scope: AllocatorAuditScope,
    ) -> usize {
        match scope {
            AllocatorAuditScope::FullRegistry => {
                let registry_snapshot = self.registry.snapshot();
                let phys_snapshot = self.phys.lock().snapshot();
                let layers = self.layer_stats_with_physical_registry(
                    phys_snapshot.stats,
                    registry_snapshot.stats,
                );
                let overview = stats::build_overview_from_layers(self.boot.snapshot(), &layers);
                let slab_audit = self.combined_slab_audit();
                let kheap_audit = self.combined_kheap_audit();
                stats::format_diagnostic(
                    buf,
                    &overview,
                    &layers,
                    &registry_snapshot.audit,
                    &phys_snapshot.audit,
                    &slab_audit,
                    &kheap_audit,
                )
            }
            AllocatorAuditScope::CountersOnly => {
                let layers = self.layer_stats_with_registry(self.registry.stats());
                let overview = stats::build_overview_from_layers(self.boot.snapshot(), &layers);
                stats::format_diagnostic_counters(buf, &overview, &layers)
            }
        }
    }

    /// 格式化只基于计数器的轻量诊断文本，不扫描 registry 链表。
    pub fn format_diagnostic_counters(&self, buf: &mut [u8]) -> usize {
        self.format_diagnostic_with_scope(buf, AllocatorAuditScope::CountersOnly)
    }

    /// 显式格式化带 registry 结构扫描的完整诊断文本。
    pub fn format_diagnostic_full(&self, buf: &mut [u8]) -> usize {
        self.format_diagnostic_with_scope(buf, AllocatorAuditScope::FullRegistry)
    }

    /// 返回 allocator 分层账本的一致性审计快照。
    ///
    /// 这个接口只读取统计信息，不扫描对象内容，也不会修复状态；它用于测试、benchmark 和
    /// 故障日志确认 registry 与 slab/kheap 等后端的计数是否仍然一致。并发运行时
    /// 可能观察到短暂中间态，严格断言应在没有其它 CPU 同时 alloc/free 的自检阶段执行。
    pub fn audit(&self) -> AllocatorAudit {
        self.audit_with_scope(AllocatorAuditScope::FullRegistry)
    }

    /// 按采样范围返回 allocator 审计快照。
    ///
    /// `CountersOnly` 可以证明各层 O(1) 账本之间是否一致，但不会扫描 registry 结构；返回值
    /// 中的 `registry_structure_scanned=false` 用来防止调用方把轻量结果误当成完整结构审计。
    pub fn audit_with_scope(&self, scope: AllocatorAuditScope) -> AllocatorAudit {
        match scope {
            AllocatorAuditScope::FullRegistry => {
                let registry_snapshot = self.registry.snapshot();
                let phys_snapshot = self.phys.lock().snapshot();
                let layers = self.layer_stats_with_physical_registry(
                    phys_snapshot.stats,
                    registry_snapshot.stats,
                );
                let slab_audit = self.combined_slab_audit();
                let kheap_audit = self.combined_kheap_audit();
                stats::build_audit_with_structures(
                    &layers,
                    registry_snapshot.audit,
                    phys_snapshot.audit,
                    slab_audit,
                    kheap_audit,
                )
            }
            AllocatorAuditScope::CountersOnly => {
                let layers = self.layer_stats_with_registry(self.registry.stats());
                stats::build_counter_audit(&layers)
            }
        }
    }

    /// 返回不扫描 registry 链表的轻量审计快照。
    pub fn audit_counters(&self) -> AllocatorAudit {
        self.audit_with_scope(AllocatorAuditScope::CountersOnly)
    }

    /// 返回 slab 内部链表和位图的一致性审计结果。
    ///
    /// 该接口只在每个 size class 内做有界扫描，不会修复状态。它比普通 `SlabStats`
    /// 更适合在 allocator 自检和 panic 前诊断里确认 slab node 链、alloc/cache 位图与
    /// O(1) 统计是否仍然一致。
    pub fn slab_audit(&self) -> SlabAudit {
        self.combined_slab_audit()
    }

    /// 返回每个 slab size class 的轻量计数器快照。
    ///
    /// 该接口只读取各 zone 的 O(1) 统计，不扫描对象内容，用于定位小对象泄漏集中在哪个
    /// 尺寸层级。
    pub fn slab_class_stats(&self) -> [SlabClassStat; SLAB_SIZE_CLASS_COUNT] {
        let mut stats = self.slab.class_stats();
        let tracked = self.tracked_slab.class_stats();
        for (combined, tracked) in stats.iter_mut().zip(tracked) {
            combined.merge(tracked);
        }
        stats
    }

    /// 返回 kheap 大对象缓存 ring 和活跃页账本的一致性审计结果。
    ///
    /// kheap 的活跃对象所有权由 registry 证明；这里重点扫描缓存 ring，确认 cached range
    /// 没有错阶、坏槽位或统计漂移。
    pub fn kheap_audit(&self) -> KernelHeapAudit {
        self.combined_kheap_audit()
    }

    pub fn reclaim(
        &self,
        request: AllocatorReclaimRequest,
    ) -> Result<AllocatorReclaimStats, AllocationError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(AllocationError::NotInitialized);
        }

        let mut kheap = if request.kheap_cached_ranges == 0 {
            KernelHeapReclaimStats::default()
        } else {
            self.kheap
                .reclaim_cached_ranges(request.kheap_cached_ranges, &self.phys, &self.vmem)
        };
        if request.kheap_cached_ranges != 0 {
            kheap.merge(self.tracked_kheap.reclaim_cached_ranges(
                request.kheap_cached_ranges,
                &self.phys,
                &self.vmem,
            ));
        }
        let mut slab = if request.flush_slab_cpu_caches || request.reclaim_slab_empty {
            self.slab.reclaim(
                request.flush_slab_cpu_caches,
                request.reclaim_slab_empty,
                &self.phys,
                &self.vmem,
            )
        } else {
            SlabReclaimStats::default()
        };
        if request.flush_slab_cpu_caches || request.reclaim_slab_empty {
            slab.merge(self.tracked_slab.reclaim(
                request.flush_slab_cpu_caches,
                request.reclaim_slab_empty,
                &self.phys,
                &self.vmem,
            ));
        }
        let phys = if request.reclaim_physical_deferred {
            self.phys.lock().reclaim_deferred()
        } else {
            BuddyReclaimStats::default()
        };

        Ok(AllocatorReclaimStats { kheap, slab, phys })
    }

    pub fn reclaim_caches(&self) -> Result<AllocatorReclaimStats, AllocationError> {
        self.reclaim(AllocatorReclaimRequest::caches())
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

    pub fn buddy_stats(&self) -> BuddyStats {
        self.phys.lock().stats()
    }

    /// 扫描 buddy hash/free-list/node freelist 并返回物理页结构审计结果。
    ///
    /// 这是冷路径接口，用于 allocator 自检、benchmark 和 panic 前日志。热路径只应读取
    /// [`KernelMemorySubsystem::buddy_stats`]，避免把全量物理页结构扫描放进 alloc/free。
    pub fn buddy_audit(&self) -> BuddyAudit {
        self.phys.lock().audit()
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

    pub fn registry_path_counters(&self) -> RegistryPathCounters {
        self.registry.path_counters()
    }

    #[cfg(feature = "performance-profile")]
    pub fn slab_profile_counter(&self, cpu: usize, counter: SlabProfileCounter) -> u64 {
        self.slab
            .profile_counter(cpu, counter)
            .saturating_add(self.tracked_slab.profile_counter(cpu, counter))
    }

    /// 返回指定非零外部所有者当前仍存活的 allocator 分配摘要。
    ///
    /// `owner=0` 表示普通内核分配，不属于外部所有者；此时直接返回空摘要，不访问
    /// registry 或 owner index。
    pub fn owner_allocation_stats(&self, owner: u64) -> AllocationOwnerStats {
        if owner == 0 {
            return AllocationOwnerStats::default();
        }
        self.owner_index.stats(owner)
    }

    pub fn owner_index_audit(&self) -> OwnerIndexAudit {
        self.owner_index.audit()
    }

    /// 扫描 registry 内部链表并返回结构审计结果。
    ///
    /// 这是冷路径自检接口，会遍历所有 shard 的 bucket 链和 freelist；热路径只应使用
    /// [`KernelMemorySubsystem::registry_stats`] 读取 O(1) 计数器，避免把完整扫描放进
    /// alloc/free 的临界路径。
    pub fn registry_audit(&self) -> AllocationRegistryAudit {
        self.registry.audit()
    }

    /// 在同一次 shard 加锁窗口中同时取得 registry 计数器和结构审计结果。
    ///
    /// 诊断和 benchmark 同时需要两类数据时应优先使用该接口，避免先 `stats()` 再
    /// `audit()` 造成重复锁 shard，也让两份数据来自更接近的采样窗口。
    pub fn registry_snapshot(&self) -> AllocationRegistrySnapshot {
        self.registry.snapshot()
    }

    pub fn metadata_stats(&self) -> MetadataStats {
        self.metadata.stats()
    }

    pub fn allocate_physical(
        &self,
        request: PhysicalAllocRequest,
    ) -> Result<PhysicalAllocation, buddy::BuddyAllocError> {
        let request = request.validate().map_err(buddy_alloc_error_from_request)?;
        // active is a one-way latch; Relaxed is safe here — the Release/Acquire
        // pair at init already establishes happens-before, and allocate_physical
        // is called on every page allocation, making Acquire overhead significant
        // on LoongArch (dbar instruction).
        let active = self.active.load(Ordering::Relaxed);
        let accounting_owner = if active {
            resolve_accounting_owner(request.accounting_owner())
        } else {
            0
        };
        if !try_reserve_accounting(accounting_owner, request.size) {
            return Err(buddy::BuddyAllocError::Fragmented);
        }
        let request = request.with_accounting_owner(accounting_owner);
        let mut allocation = self.allocate_physical_raw(request);
        if allocation.is_err() && active {
            // 显式物理页与普通内核堆共享 buddy。用户页、页表页等 typed 请求在
            // 宣告 OOM 前也必须归还 allocator 自身缓存，否则短生命周期堆对象
            // 留下的空 slab/kheap range 会对物理页调用方表现成不可用内存。
            let _ = self.reclaim_allocator_caches_for_retry();
            allocation = self.allocate_physical_raw(request);
        }
        let allocation = match allocation {
            Ok(allocation) => allocation,
            Err(err) => {
                release_accounting(accounting_owner, request.size);
                return Err(err);
            }
        };
        if !active {
            return Ok(allocation);
        }

        {
            let record = physical_record_from_allocation(request, allocation, accounting_owner);
            match self.registry.register_result(&self.boot, record) {
                Ok(()) => match self.owner_index.track(record) {
                    Ok(()) => Ok(allocation),
                    Err(owner_err) => {
                        match self.registry.remove_result(record.ptr) {
                            Ok(removed) if removed == record => {}
                            Ok(removed) => panic!(
                                "[alloc][invariant] physical owner index rollback removed unexpected record: expected={:?} removed={:?}",
                                record, removed
                            ),
                            Err(registry_err) => panic!(
                                "[alloc][invariant] physical owner index rollback lost registry record: paddr={:#x} owner_err={:?} registry_err={:?}",
                                record.ptr, owner_err, registry_err
                            ),
                        }
                        if !self.free_physical_raw(allocation) {
                            panic!(
                                "[alloc][invariant] physical owner index rollback failed to release buddy block: paddr={:#x} owner_err={:?}",
                                record.ptr, owner_err
                            );
                        }
                        release_accounting(accounting_owner, request.size);
                        Err(buddy_alloc_error_from_owner_index(owner_err))
                    }
                },
                Err(err) => {
                    let _ = self.free_physical_raw(allocation);
                    release_accounting(accounting_owner, request.size);
                    Err(match err {
                        RegistryError::NotInitialized => buddy::BuddyAllocError::NotInitialized,
                        RegistryError::InvalidRecord => buddy::BuddyAllocError::InvalidAddress,
                        RegistryError::UnknownPointer => buddy::BuddyAllocError::InvalidAddress,
                        RegistryError::DuplicatePointer => buddy::BuddyAllocError::BlockNotFree,
                        RegistryError::MetadataOutOfMemory => {
                            buddy::BuddyAllocError::MetadataOutOfMemory
                        }
                    })
                }
            }
        }
    }

    /// 分配由内核内部完整句柄独占管理的物理页，不写入通用逐对象注册表。
    ///
    /// 该入口只适用于 `owner=0` 且调用方会一直保留精确 [`PhysicalAllocation`]
    /// 生命周期的对象，例如用户常驻页。外部所有者、DMA 和只保存裸物理
    /// 地址的子系统仍必须使用 [`KernelMemorySubsystem::allocate_physical`]。
    pub fn allocate_untracked_physical(
        &self,
        request: PhysicalAllocRequest,
    ) -> Result<PhysicalAllocation, buddy::BuddyAllocError> {
        let request = request
            .validate()
            .map_err(buddy_alloc_error_from_request)?
            .without_external_accounting();
        let active = self.active.load(Ordering::Relaxed);
        let mut allocation = self.allocate_physical_raw(request);
        if allocation.is_err() && active {
            let _ = self.reclaim_allocator_caches_for_retry();
            allocation = self.allocate_physical_raw(request);
        }
        allocation
    }

    /// 释放由 [`KernelMemorySubsystem::allocate_untracked_physical`] 返回的完整句柄。
    ///
    /// 此路径直接按句柄中的物理地址和阶数归还伙伴分配器，不查询或修改通用
    /// 注册表。调用方必须保留准确的分配几何；不得传入受追踪物理页的句柄，
    /// 否则会留下失真的账本记录。
    pub fn try_free_untracked_physical(
        &self,
        allocation: PhysicalAllocation,
    ) -> Result<(), PhysicalFreeError> {
        self.try_free_physical_raw(allocation)
            .map_err(PhysicalFreeError::Buddy)
    }

    pub fn free_physical(&self, allocation: PhysicalAllocation) -> bool {
        self.try_free_physical(allocation).is_ok()
    }

    /// 按物理地址查询已经进入 registry 的显式物理页句柄。
    ///
    /// 这个接口面向只保存 `paddr` 的外部子系统。它把 registry 中的真实
    /// size/order/page_size 恢复成 [`PhysicalAllocation`]，避免调用方为了日志、校验或
    /// 延迟释放而手工拼装句柄。
    pub fn query_physical_allocation(
        &self,
        paddr: usize,
    ) -> Result<PhysicalAllocation, PhysicalFreeError> {
        let record = self
            .query_tracked_allocation(paddr)
            .map_err(|_| PhysicalFreeError::UnknownPointer)?;
        if record.kind != AllocationKind::Physical {
            return Err(PhysicalFreeError::InvalidRecordKind {
                actual: record.kind,
            });
        }
        Ok(physical_allocation_from_record(record))
    }

    /// 按物理地址释放一个已经进入 registry 的显式物理页。
    ///
    /// 外部 MM/页表代码经常只保存 `paddr`，如果让它们手工重建
    /// [`PhysicalAllocation`]，很容易把 size/order/page_size 填错。这个接口直接移除
    /// registry 中的 `Physical` 记录并释放对应 buddy 块，只做一次 shard 查找；若后端释放
    /// 失败会恢复原记录，保持和 [`KernelMemorySubsystem::try_free_physical`] 相同的回滚语义。
    pub fn try_free_physical_addr(&self, paddr: usize) -> Result<(), PhysicalFreeError> {
        if !self.active.load(Ordering::Acquire) {
            // 按地址释放依赖 registry 恢复 order/size/page_size。allocator 尚未 active 时
            // 没有逐对象物理页记录，不能把裸 paddr 猜成单页释放；早期路径必须保留完整
            // [`PhysicalAllocation`] 并调用 `try_free_physical()`。
            return Err(PhysicalFreeError::Registry(RegistryError::NotInitialized));
        }

        let record = match self.registry.get_result(paddr) {
            Ok(record) => record,
            Err(RegistryError::UnknownPointer) => return Err(PhysicalFreeError::UnknownPointer),
            Err(err) => return Err(PhysicalFreeError::Registry(err)),
        };
        if let Err(err) = self.owner_index.untrack(record) {
            panic!(
                "[alloc][invariant] owner index rejected physical free: paddr={:#x} owner={} err={:?}",
                paddr,
                record.accounting_owner(),
                err
            );
        }
        let record = match self.registry.remove_result(paddr) {
            Ok(record) => record,
            Err(err) => {
                self.restore_owner_index_or_panic(record, "physical registry remove failure");
                return Err(PhysicalFreeError::Registry(err));
            }
        };
        if record.kind != AllocationKind::Physical {
            self.restore_tracked_record_or_panic(record, "physical kind validation failure");
            return Err(PhysicalFreeError::InvalidRecordKind {
                actual: record.kind,
            });
        }

        let allocation = physical_allocation_from_record(record);
        if allocation.paddr != paddr {
            self.restore_tracked_record_or_panic(record, "physical address validation failure");
            return Err(PhysicalFreeError::AddressMismatch {
                expected: allocation.paddr,
                actual: paddr,
            });
        }

        match self.try_free_physical_raw(allocation) {
            Ok(()) => {
                release_accounting(record.accounting_owner(), record.size);
                Ok(())
            }
            Err(err) => {
                self.restore_tracked_record_or_panic(record, "physical buddy release failure");
                Err(PhysicalFreeError::Buddy(err))
            }
        }
    }

    /// 释放由 [`KernelMemorySubsystem::allocate_physical`] 返回的显式物理页。
    ///
    /// 与旧的布尔接口相比，这个接口会保留失败原因：active 后先校验 registry 中的
    /// `Physical` 记录，确认地址、order 和保留大小都与调用方传入的句柄一致，然后才
    /// 进入 buddy 释放。任何校验失败都会把刚移除的 registry 记录恢复回去，避免一次
    /// 错误释放尝试破坏后续所有权判断。
    pub fn try_free_physical(
        &self,
        allocation: PhysicalAllocation,
    ) -> Result<(), PhysicalFreeError> {
        if !self.active.load(Ordering::Acquire) {
            return self
                .try_free_physical_raw(allocation)
                .map_err(PhysicalFreeError::Buddy);
        }

        let record = match self.registry.get_result(allocation.paddr) {
            Ok(record) => record,
            Err(RegistryError::UnknownPointer) => return Err(PhysicalFreeError::UnknownPointer),
            Err(err) => return Err(PhysicalFreeError::Registry(err)),
        };

        if let Err(err) = self.owner_index.untrack(record) {
            panic!(
                "[alloc][invariant] owner index rejected physical free: paddr={:#x} owner={} err={:?}",
                allocation.paddr,
                record.accounting_owner(),
                err
            );
        }
        let record = match self.registry.remove_result(allocation.paddr) {
            Ok(record) => record,
            Err(err) => {
                self.restore_owner_index_or_panic(record, "physical registry remove failure");
                return Err(PhysicalFreeError::Registry(err));
            }
        };

        if let Err(err) = validate_physical_free_record(record, allocation) {
            // 调用方传入的句柄和 registry 中活跃记录不一致，说明这不是一次合法的
            // 所有权释放。物理页仍由原记录持有，必须先恢复账本再返回类型化错误。
            self.restore_tracked_record_or_panic(record, "physical handle validation failure");
            return Err(err);
        }

        match self.try_free_physical_raw(allocation) {
            Ok(()) => {
                release_accounting(record.accounting_owner(), record.size);
                Ok(())
            }
            Err(err) => {
                // buddy 拒绝释放时，物理页实际仍由调用方持有；必须恢复 registry
                // 账本，否则下一次释放会变成未知指针，审计也会漏掉该页。
                self.restore_tracked_record_or_panic(record, "physical buddy release failure");
                Err(PhysicalFreeError::Buddy(err))
            }
        }
    }

    /// 直接从 buddy 分配裸物理页，不写入 allocator registry。
    ///
    /// 这个接口只保留给极早期 bring-up 或 allocator 内部兼容路径。正常驱动、DMA、
    /// 用户页和未来 LKM 风格扩展都应使用 [`KernelMemorySubsystem::allocate_physical`]
    /// / [`KernelMemorySubsystem::try_free_physical`]，否则审计无法发现泄漏、重复释放或
    /// 句柄参数不匹配。
    #[deprecated(
        since = "0.1.0",
        note = "use allocate_physical/try_free_physical so physical pages are tracked in allocator registry"
    )]
    pub fn buddy_alloc_pages(&self, order: usize) -> Option<usize> {
        let mut phys = self.phys.lock();
        phys.alloc_pages(order)
    }

    /// 释放由 [`KernelMemorySubsystem::buddy_alloc_pages`] 返回的裸物理页。
    ///
    /// 与 [`KernelMemorySubsystem::try_free_physical`] 不同，这里不会检查 registry 记录，
    /// 也不会产生结构化错误。除非调用方明确处在 allocator 自举阶段，否则不应使用它。
    #[deprecated(
        since = "0.1.0",
        note = "use try_free_physical so ownership is validated against allocator registry"
    )]
    pub fn buddy_free_pages(&self, addr: usize, order: usize) -> bool {
        let mut phys = self.phys.lock();
        phys.free_pages(addr, order).is_ok()
    }

    pub fn allocate(&self, request: MemoryRequest) -> Result<AllocationRecord, AllocationError> {
        // active is a one-way latch; Relaxed is safe after init establishes
        // happens-before via the Release store. Avoids a dbar on every allocation
        // on LoongArch while preserving the boot-phase fallback.
        let active = self.active.load(Ordering::Relaxed);
        let request = request.validate()?;
        let accounting_owner = if active {
            resolve_accounting_owner(request.accounting_owner())
        } else {
            0
        };
        if !try_reserve_accounting(accounting_owner, request.size) {
            return Err(AllocationError::OutOfMemory);
        }
        let request = request.with_accounting_owner(accounting_owner);

        if !active {
            let result = self.allocate_boot(request);
            if result.is_err() {
                release_accounting(accounting_owner, request.size);
            }
            return result;
        }

        let mut allocation = self.allocate_active_once(request, crate::space::ArenaKind::Tracked);
        if allocation.is_err() && !matches!(request.reclaim, ReclaimPolicy::NoReclaim) {
            match request.reclaim {
                ReclaimPolicy::NoReclaim => {}
                ReclaimPolicy::TryAllocatorReclaim => {
                    self.reclaim_allocator_caches_for_retry();
                    allocation =
                        self.allocate_active_once(request, crate::space::ArenaKind::Tracked);
                }
            }
        }
        match allocation {
            Ok(record) => Ok(record),
            Err(err) => {
                release_accounting(accounting_owner, request.size);
                Err(err)
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

    /// 查询逐对象注册表中的分配记录。
    ///
    /// 与 [`KernelMemorySubsystem::query_allocation`] 不同，这个 API 不把 boot bump
    /// 区间作为 fallback。外部子系统需要判断“这个裸指针是否仍是一个可操作的对象”
    /// 时应使用这里，因为 active 后 boot 分配没有逐对象边界，不能安全地从区间包含关系
    /// 推导出对象所有权。
    pub fn query_tracked_allocation(&self, ptr: usize) -> Result<AllocationRecord, OwnershipError> {
        self.registry.get(ptr).ok_or(OwnershipError::UnknownPointer)
    }

    /// 查询完整覆盖给定范围的逐对象分配记录。
    ///
    /// 这是旧 ELM ABI 的兼容冷路径，会扫描 registry。新调用方应携带 owner 并使用
    /// [`KernelMemorySubsystem::query_owned_range`]，避免全表扫描。
    pub fn query_containing_allocation(
        &self,
        ptr: usize,
        len: usize,
    ) -> Result<AllocationRecord, OwnershipError> {
        self.registry
            .find_containing(ptr, len)
            .ok_or(OwnershipError::UnknownPointer)
    }

    /// 查询指定 owner 是否完整拥有一个 tracked 动态内存范围。
    ///
    /// 查询只访问该 owner 的有序范围树，不扫描其它 owner 或普通内核对象。
    pub fn query_owned_range(&self, owner: u64, ptr: usize, len: usize) -> bool {
        self.owner_index.contains(owner, ptr, len)
    }

    /// 为一个非零外部所有者创建普通 Kernel 域分配。
    ///
    /// 该入口强制覆盖请求中的计量 owner，不能借助调用方构造的 `MemoryRequest` 把资源
    /// 记到内核或其它 cell。它只接受 slab/kheap 可处理的普通 Kernel 域请求。
    pub fn allocate_owned(
        &self,
        owner: u64,
        request: MemoryRequest,
    ) -> Result<AllocationRecord, OwnedAllocationError> {
        if owner == 0 {
            return Err(OwnedAllocationError::InvalidOwner);
        }
        if !self.active.load(Ordering::Acquire) {
            return Err(OwnedAllocationError::Unavailable);
        }
        if request.domain != MemoryDomain::Kernel || request.validate().is_err() {
            return Err(OwnedAllocationError::InvalidRequest);
        }
        let record = self
            .allocate(request.with_accounting_owner(owner))
            .map_err(map_owned_allocation_error)?;
        if record.accounting_owner() != owner
            || !matches!(record.kind, AllocationKind::Small | AllocationKind::Large)
        {
            let _ = self.deallocate(record.ptr);
            return Err(OwnedAllocationError::BackendFailure);
        }
        Ok(record)
    }

    /// 查询一个由指定非零所有者持有的普通 Kernel 域分配。
    pub fn query_owned_allocation(
        &self,
        owner: u64,
        ptr: usize,
    ) -> Result<AllocationRecord, OwnedAllocationError> {
        if owner == 0 {
            return Err(OwnedAllocationError::InvalidOwner);
        }
        if ptr == 0 {
            return Err(OwnedAllocationError::UnknownPointer);
        }
        let _operation = OWNED_ALLOCATION_LOCKS[owned_allocation_lock_index(ptr)].lock();
        self.query_owned_allocation_unlocked(owner, ptr)
    }

    /// 释放一个由指定非零所有者持有的普通 Kernel 域分配。
    ///
    /// 同一地址的外部操作由分片锁串行化，使 owner 查询与注册表移除之间不会被另一条
    /// ELM 直接符号调用插入。普通内核代码仍必须遵守对象自身的独占释放规则。
    pub fn deallocate_owned(&self, owner: u64, ptr: usize) -> Result<(), OwnedAllocationError> {
        if owner == 0 {
            return Err(OwnedAllocationError::InvalidOwner);
        }
        if ptr == 0 {
            return Err(OwnedAllocationError::UnknownPointer);
        }
        if !self.active.load(Ordering::Acquire) {
            return Err(OwnedAllocationError::Unavailable);
        }
        let _operation = OWNED_ALLOCATION_LOCKS[owned_allocation_lock_index(ptr)].lock();
        self.query_owned_allocation_unlocked(owner, ptr)?;
        self.deallocate(ptr).map_err(|error| match error {
            DeallocationError::UnknownPointer => OwnedAllocationError::UnknownPointer,
            _ => OwnedAllocationError::BackendFailure,
        })
    }

    /// 调整一个由指定非零所有者持有的普通 Kernel 域分配。
    ///
    /// 返回记录始终继续绑定原 owner。调用方必须把旧地址视为已经失效，即使本次调整
    /// 恰好原地完成。
    pub fn reallocate_owned(
        &self,
        owner: u64,
        ptr: usize,
        request: MemoryRequest,
    ) -> Result<AllocationRecord, OwnedAllocationError> {
        self.reallocate_owned_inner(owner, ptr, request, None)
    }

    /// 调整外部所有者持有的普通分配，同时保证指定区间不属于待调整对象。
    ///
    /// 该接口用于跨 ABI 调用中的结果槽保护：若结果槽位于旧对象内部，移动式重分配会先
    /// 释放旧对象，再向已经失效的结果槽写入记录。排除区间检查与 owner 校验、重分配由
    /// 同一地址分片锁串行化，因此其它受约束操作不能在检查和提交之间制造地址复用。
    pub fn reallocate_owned_excluding_range(
        &self,
        owner: u64,
        ptr: usize,
        request: MemoryRequest,
        excluded_start: usize,
        excluded_len: usize,
    ) -> Result<AllocationRecord, OwnedAllocationError> {
        if excluded_start == 0
            || excluded_len == 0
            || excluded_start.checked_add(excluded_len).is_none()
        {
            return Err(OwnedAllocationError::InvalidRequest);
        }
        self.reallocate_owned_inner(owner, ptr, request, Some((excluded_start, excluded_len)))
    }

    fn reallocate_owned_inner(
        &self,
        owner: u64,
        ptr: usize,
        request: MemoryRequest,
        excluded: Option<(usize, usize)>,
    ) -> Result<AllocationRecord, OwnedAllocationError> {
        if owner == 0 {
            return Err(OwnedAllocationError::InvalidOwner);
        }
        if ptr == 0 {
            return Err(OwnedAllocationError::UnknownPointer);
        }
        if !self.active.load(Ordering::Acquire) {
            return Err(OwnedAllocationError::Unavailable);
        }
        if request.domain != MemoryDomain::Kernel || request.validate().is_err() {
            return Err(OwnedAllocationError::InvalidRequest);
        }
        let _operation = OWNED_ALLOCATION_LOCKS[owned_allocation_lock_index(ptr)].lock();
        let old_record = self.query_owned_allocation_unlocked(owner, ptr)?;
        if let Some((excluded_start, excluded_len)) = excluded
            && ranges_overlap(
                old_record.ptr,
                old_record.usable_size.max(old_record.size),
                excluded_start,
                excluded_len,
            )
        {
            return Err(OwnedAllocationError::AliasedRange);
        }
        let record = self
            .reallocate(ptr, request.with_accounting_owner(owner))
            .map_err(map_owned_allocation_error)?;
        if record.accounting_owner() != owner
            || !matches!(record.kind, AllocationKind::Small | AllocationKind::Large)
        {
            return Err(OwnedAllocationError::BackendFailure);
        }
        Ok(record)
    }

    fn query_owned_allocation_unlocked(
        &self,
        owner: u64,
        ptr: usize,
    ) -> Result<AllocationRecord, OwnedAllocationError> {
        let record = self
            .query_tracked_allocation(ptr)
            .map_err(|_| OwnedAllocationError::UnknownPointer)?;
        if record.accounting_owner() != owner {
            return Err(OwnedAllocationError::PermissionDenied);
        }
        if record.domain != MemoryDomain::Kernel
            || !matches!(record.kind, AllocationKind::Small | AllocationKind::Large)
        {
            return Err(OwnedAllocationError::InvalidRequest);
        }
        Ok(record)
    }

    /// 判断裸指针是否属于当前 allocator 逐对象跟踪的活跃分配。
    ///
    /// 这是给外部子系统做防御性检查的窄 API。需要释放或调整大小时仍应调用
    /// [`KernelMemorySubsystem::deallocate`] / [`KernelMemorySubsystem::reallocate`]，
    /// 不能因为这里返回 `true` 就绕过 allocator 的账本更新。
    ///
    /// active 之后 boot 区域没有逐对象元数据，不能把“地址落在 boot bump 区间内”
    /// 等价成“这是一个仍可操作的分配起始指针”；因此这里只信任注册表。
    pub fn owns_allocation(&self, ptr: usize) -> bool {
        ptr != 0 && self.query_tracked_allocation(ptr).is_ok()
    }

    pub fn allocation_kind(&self, ptr: usize) -> Result<AllocationKind, OwnershipError> {
        self.query_tracked_allocation(ptr).map(|record| record.kind)
    }

    /// 返回调用方请求的逻辑大小。
    ///
    /// 该值不一定等于底层可用空间；slab 对象通常会向上取整到 size class。
    pub fn allocation_size(&self, ptr: usize) -> Result<usize, OwnershipError> {
        self.query_tracked_allocation(ptr).map(|record| record.size)
    }

    /// 返回 allocator 为该分配实际保留的可用大小。
    ///
    /// 调用方只能在自己持有对象所有权时使用这个值做容量优化，不能把它当作越界访问
    /// 的授权；对象语义上的有效长度仍由上层数据结构维护。
    pub fn allocation_usable_size(&self, ptr: usize) -> Result<usize, OwnershipError> {
        self.query_tracked_allocation(ptr)
            .map(|record| record.usable_size.max(record.size))
    }

    /// 返回分配记录中的对齐要求。
    pub fn allocation_alignment(&self, ptr: usize) -> Result<usize, OwnershipError> {
        self.query_tracked_allocation(ptr)
            .map(|record| record.align)
    }

    /// 判断指定分配能否在不移动指针的前提下调整到新请求。
    ///
    /// 这个 API 只做账本和 size-class/order 判断，不会修改 allocator 状态。需要稳定
    /// 指针的外部子系统可以先调用它：返回 `Ok(true)` 时 [`reallocate`] 会走原地更新，
    /// 返回 `Ok(false)` 时则表示需要新分配、复制和释放旧对象。
    pub fn can_reallocate_in_place(
        &self,
        ptr: usize,
        request: MemoryRequest,
    ) -> Result<bool, AllocationError> {
        if ptr == 0
            || !self.active.load(Ordering::Acquire)
            || !matches!(request.domain, MemoryDomain::Kernel)
        {
            return Err(AllocationError::InvalidLayout);
        }

        let new_layout = request.layout()?;
        let record = self
            .query_tracked_allocation(ptr)
            .map_err(|_| AllocationError::InvalidLayout)?;
        if !matches!(record.kind, AllocationKind::Small | AllocationKind::Large) {
            return Err(AllocationError::InvalidLayout);
        }
        Ok(self.can_reuse_allocation(record, new_layout))
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
            .get(ptr)
            .ok_or(DeallocationError::UnknownPointer)?;
        if let Err(err) = self.owner_index.untrack(record) {
            panic!(
                "[alloc][invariant] owner index rejected tracked free: ptr={:#x} owner={} err={:?}",
                ptr,
                record.accounting_owner(),
                err
            );
        }
        let record = match self.registry.remove_result(ptr) {
            Ok(record) => record,
            Err(err) => {
                if let Err(restore_err) = self.owner_index.track(record) {
                    panic!(
                        "[alloc][invariant] failed to restore owner index after registry remove failure: ptr={:#x} remove={:?} restore={:?}",
                        ptr, err, restore_err
                    );
                }
                return Err(DeallocationError::UnknownPointer);
            }
        };
        let (slab, kheap) = match record.arena {
            Some(AllocationArena::Tracked) => (&self.tracked_slab, &self.tracked_kheap),
            Some(AllocationArena::Kernel) => (&self.slab, &self.kheap),
            _ => (&self.slab, &self.kheap),
        };
        let result = match record.kind {
            AllocationKind::Boot => Ok(()),
            AllocationKind::Small => {
                if slab.free_record(record, self.current_cpu_id()) {
                    Ok(())
                } else {
                    panic!(
                        "[alloc][invariant] registry owned small allocation but slab rejected free: ptr={:#x} size={} usable={}",
                        ptr, record.size, record.usable_size
                    )
                }
            }
            AllocationKind::Large => {
                if let Err(err) = kheap.free_record(record, &self.phys, &self.vmem) {
                    panic!(
                        "[alloc][invariant] registry owned large allocation but kheap rejected free: ptr={:#x} paddr={:?} order={} err={:?}",
                        ptr, record.paddr, record.order, err
                    )
                }
                Ok(())
            }
            AllocationKind::Physical => {
                let allocation = physical_allocation_from_record(record);
                match self.try_free_physical_raw(allocation) {
                    Ok(()) => Ok(()),
                    Err(err) => panic!(
                        "[alloc][invariant] registry owned physical allocation but buddy rejected free: ptr={:#x} paddr={:#x} order={} err={:?}",
                        ptr, allocation.paddr, allocation.order, err
                    ),
                }
            }
        };
        if result.is_ok() {
            release_accounting(record.accounting_owner(), record.size);
        }
        result
    }

    /// 调整一个普通内核分配的大小，并返回更新后的分配记录。
    ///
    /// 这是给内核其它子系统使用的安全 resize API。它只支持 `Kernel` 域中由
    /// slab/kheap 管理的对象；物理页应使用 `allocate_physical/free_physical` 明确表达
    /// 所有权。
    ///
    /// 和 `GlobalAlloc::realloc` 不同，这里不要求调用方提供旧 `Layout`，复制长度
    /// 直接来自 allocator 注册表中的真实记录，避免外部 API 误传旧尺寸导致越界复制。
    pub fn reallocate(
        &self,
        ptr: usize,
        request: MemoryRequest,
    ) -> Result<AllocationRecord, AllocationError> {
        self.total_reallocs.fetch_add(1, Ordering::Relaxed);
        self.tracked_kheap.record_realloc();

        if ptr == 0 {
            return self.allocate(request);
        }
        if !self.active.load(Ordering::Acquire) || !matches!(request.domain, MemoryDomain::Kernel) {
            return Err(AllocationError::InvalidLayout);
        }

        let request = request.validate()?;
        let new_layout = request.layout()?;
        let old_record = match self.probe_tracked_realloc(ptr, new_layout, request.size) {
            Ok(TrackedReallocProbe::Updated { old_size, record }) => {
                if matches!(request.zeroing, Zeroing::Zeroed) && request.size > old_size {
                    let start = ptr
                        .checked_add(old_size)
                        .ok_or(AllocationError::InvalidLayout)?;
                    let len = request.size - old_size;
                    #[cfg(feature = "performance-profile")]
                    let _profile = match record.kind {
                        AllocationKind::Small => {
                            profiling::scope(profiling::Event::MemZeroAllocatorSmall).bytes(len)
                        }
                        _ => profiling::scope(profiling::Event::MemZeroAllocatorLarge).bytes(len),
                    };
                    unsafe { core::ptr::write_bytes(start as *mut u8, 0, len) };
                }
                return Ok(record);
            }
            Ok(TrackedReallocProbe::NeedsMove(record)) => record,
            Ok(TrackedReallocProbe::QuotaDenied) => return Err(AllocationError::OutOfMemory),
            Ok(TrackedReallocProbe::Untracked) | Err(_) => {
                return Err(AllocationError::InvalidLayout);
            }
        };
        if !matches!(
            old_record.kind,
            AllocationKind::Small | AllocationKind::Large
        ) {
            return Err(AllocationError::InvalidLayout);
        }

        let caller = resolve_accounting_owner(request.accounting_owner());
        if caller != 0
            && old_record.accounting_owner() != 0
            && caller != old_record.accounting_owner()
        {
            return Err(AllocationError::InvalidLayout);
        }
        let new_record =
            self.allocate(request.with_accounting_owner(old_record.accounting_owner()))?;
        let copy_len = old_record.size.min(new_record.size);
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::MemCopyRealloc).bytes(copy_len);
        unsafe {
            core::ptr::copy_nonoverlapping(ptr as *const u8, new_record.ptr as *mut u8, copy_len);
        }
        self.retire_moved_kernel_allocation(ptr, old_record, || {
            let _ = self.deallocate(new_record.ptr);
        });
        Ok(new_record)
    }

    fn allocate_boot(&self, request: MemoryRequest) -> Result<AllocationRecord, AllocationError> {
        if !matches!(request.domain, MemoryDomain::Kernel) {
            return Err(AllocationError::NotInitialized);
        }
        let layout = request.layout()?;
        let ptr = self.boot.alloc(layout) as usize;
        if ptr == 0 {
            return Err(AllocationError::OutOfMemory);
        }
        if matches!(request.zeroing, Zeroing::Zeroed) {
            unsafe {
                core::ptr::write_bytes(ptr as *mut u8, 0, request.size);
            }
        }
        let record = AllocationRecord::new(AllocationKind::Boot, MemoryDomain::Kernel, ptr)
            .with_sizes(request.size, request.size, request.align)
            .with_accounting_owner(request.accounting_owner().unwrap_or(0));
        Ok(record)
    }

    fn alloc_active(&self, layout: Layout, zeroing: Zeroing) -> *mut u8 {
        let accounting_owner = resolve_accounting_owner(None);
        let request = MemoryRequest::for_kernel_layout(layout)
            .with_zeroing(zeroing)
            .with_reclaim(ReclaimPolicy::TryAllocatorReclaim)
            .with_accounting_owner(accounting_owner);
        let result = if accounting_owner == 0 {
            self.allocate_untracked_global_ptr(request)
        } else {
            self.allocate(request).map(|record| record.ptr)
        };
        match result {
            Ok(ptr) => ptr as *mut u8,
            Err(error) => {
                log::error!(
                    "[alloc] global allocation failed: size={} align={} zeroing={:?} owner={} error={:?}",
                    layout.size(),
                    layout.align(),
                    zeroing,
                    accounting_owner,
                    error,
                );
                null_mut()
            }
        }
    }

    fn allocate_untracked_global_ptr(
        &self,
        request: MemoryRequest,
    ) -> Result<usize, AllocationError> {
        let request = request.validate()?.with_accounting_owner(0);
        let mut allocation = self.allocate_untracked_global_ptr_once(request);
        if allocation.is_err() && !matches!(request.reclaim, ReclaimPolicy::NoReclaim) {
            let _ = self.reclaim_allocator_caches_for_retry();
            allocation = self.allocate_untracked_global_ptr_once(request);
        }
        allocation
    }

    fn allocate_untracked_global_ptr_once(
        &self,
        request: MemoryRequest,
    ) -> Result<usize, AllocationError> {
        if !matches!(request.domain, MemoryDomain::Kernel) {
            return Err(AllocationError::InvalidLayout);
        }
        let layout = request.layout()?;
        let force_large = matches!(request.page_policy, PagePolicy::RequireLarge);
        let alloc_large = || -> Result<usize, AllocationError> {
            let range =
                self.kheap
                    .alloc_range(layout, request.page_policy, &self.phys, &self.vmem)?;
            if matches!(request.zeroing, Zeroing::Zeroed) {
                #[cfg(feature = "performance-profile")]
                let _profile =
                    profiling::scope(profiling::Event::MemZeroAllocatorLarge).bytes(request.size);
                // Safety: kheap 返回当前调用方独占且至少覆盖请求长度的有效范围。
                unsafe { core::ptr::write_bytes(range.vaddr as *mut u8, 0, request.size) };
            }
            Ok(range.vaddr)
        };

        if is_small_request(request) && !force_large {
            if let Some(zone_idx) = SlabAllocator::class_index_for(layout) {
                let allocation =
                    self.slab
                        .alloc_class(zone_idx, self.current_cpu_id(), &self.phys, &self.vmem);
                if allocation.is_null() {
                    return Err(AllocationError::OutOfMemory);
                }
                if matches!(request.zeroing, Zeroing::Zeroed) {
                    #[cfg(feature = "performance-profile")]
                    let _profile = profiling::scope(profiling::Event::MemZeroAllocatorSmall)
                        .bytes(request.size);
                    // Safety: slab 返回当前调用方独占且至少覆盖请求长度的有效对象。
                    unsafe { core::ptr::write_bytes(allocation.ptr as *mut u8, 0, request.size) };
                }
                Ok(allocation.ptr)
            } else {
                alloc_large()
            }
        } else {
            alloc_large()
        }
    }

    fn is_tracked_heap_pointer(&self, ptr: usize) -> bool {
        let (start, size) = self.cached_tracked_heap_region();
        start
            .checked_add(size)
            .is_some_and(|end| ptr >= start && ptr < end)
    }

    #[inline]
    fn cached_tracked_heap_region(&self) -> (usize, usize) {
        (
            self.tracked_heap_start.load(Ordering::Relaxed),
            self.tracked_heap_size.load(Ordering::Relaxed),
        )
    }

    fn deallocate_untracked_global(
        &self,
        ptr: usize,
        layout: Layout,
    ) -> Result<(), DeallocationError> {
        if SlabAllocator::class_index_for(layout).is_some() {
            if self
                .slab
                .free_reclaiming(ptr, layout, self.current_cpu_id(), &self.phys, &self.vmem)
            {
                return Ok(());
            }
            return Err(DeallocationError::UnknownPointer);
        }
        self.kheap.free_layout(ptr, layout, &self.phys, &self.vmem)
    }

    unsafe fn reallocate_untracked_global(
        &self,
        ptr: *mut u8,
        old_layout: Layout,
        new_layout: Layout,
    ) -> *mut u8 {
        let old_small = SlabAllocator::class_index_for(old_layout);
        let new_small = SlabAllocator::class_index_for(new_layout);
        if let Some(old_zone_idx) = old_small {
            if old_small == new_small && self.slab.owns_in_class(old_zone_idx, ptr as usize) {
                return ptr;
            }
        }
        if old_small.is_none()
            && new_small.is_none()
            && self
                .kheap
                .can_reuse_layout(ptr as usize, old_layout, new_layout, &self.vmem)
        {
            return ptr;
        }

        let old_valid = if let Some(old_zone_idx) = old_small {
            self.slab.owns_in_class(old_zone_idx, ptr as usize)
        } else {
            self.kheap
                .can_reuse_layout(ptr as usize, old_layout, old_layout, &self.vmem)
        };
        if !old_valid {
            self.record_ownership_failure();
            return null_mut();
        }

        let request = MemoryRequest::for_kernel_layout(new_layout)
            .with_reclaim(ReclaimPolicy::TryAllocatorReclaim)
            .with_accounting_owner(0);
        self.total_allocs.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_allocated
            .fetch_add(new_layout.size() as u64, Ordering::Relaxed);
        let new_ptr = match self.allocate_untracked_global_ptr(request) {
            Ok(ptr) => ptr as *mut u8,
            Err(_) => {
                self.record_oom();
                return null_mut();
            }
        };
        let copy_len = old_layout.size().min(new_layout.size());
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::MemCopyRealloc).bytes(copy_len);
        unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr, copy_len) };

        if self
            .deallocate_untracked_global(ptr as usize, old_layout)
            .is_err()
        {
            let _ = self.deallocate_untracked_global(new_ptr as usize, new_layout);
            self.record_ownership_failure();
            return null_mut();
        }
        self.record_global_dealloc_stats(old_layout);
        new_ptr
    }

    fn allocate_active_once(
        &self,
        request: MemoryRequest,
        arena: crate::space::ArenaKind,
    ) -> Result<AllocationRecord, AllocationError> {
        match request.domain {
            MemoryDomain::Kernel => {
                let cpu = self.current_cpu_id();
                let layout = request.layout()?;
                let force_large = matches!(request.page_policy, PagePolicy::RequireLarge);
                let kheap = if arena == crate::space::ArenaKind::Tracked {
                    &self.tracked_kheap
                } else {
                    &self.kheap
                };
                let slab = if arena == crate::space::ArenaKind::Tracked {
                    &self.tracked_slab
                } else {
                    &self.slab
                };
                let allocation_arena = match arena {
                    crate::space::ArenaKind::Tracked => AllocationArena::Tracked,
                    crate::space::ArenaKind::Kernel => AllocationArena::Kernel,
                    crate::space::ArenaKind::DirectMap => AllocationArena::DirectMap,
                };
                let alloc_large = || -> Result<AllocationRecord, AllocationError> {
                    let range = match kheap.alloc_range(
                        layout,
                        request.page_policy,
                        &self.phys,
                        &self.vmem,
                    ) {
                        Ok(range) => range,
                        Err(err) => {
                            return Err(err);
                        }
                    };
                    if matches!(request.zeroing, Zeroing::Zeroed) {
                        #[cfg(feature = "performance-profile")]
                        let _profile = profiling::scope(profiling::Event::MemZeroAllocatorLarge)
                            .bytes(request.size);
                        unsafe {
                            core::ptr::write_bytes(range.vaddr as *mut u8, 0, request.size);
                        }
                    }
                    let record = AllocationRecord::new(
                        AllocationKind::Large,
                        MemoryDomain::Kernel,
                        range.vaddr,
                    )
                    .with_arena(allocation_arena)
                    .with_sizes(request.size, range.size, request.align)
                    .with_accounting_owner(request.accounting_owner().unwrap_or(0))
                    .with_physical(
                        range.paddr,
                        range.order,
                        (1usize << range.order) * PAGE_SIZE,
                    )
                    .with_backend_cookie(KernelHeap::backend_cookie_for(
                        range,
                        request.page_policy,
                    ));
                    if arena == crate::space::ArenaKind::Tracked {
                        self.register_allocation(record, || {
                            let _ = kheap.free_record_uncached(record, &self.phys, &self.vmem);
                        })?;
                    }
                    Ok(record)
                };
                if is_small_request(request) && !force_large {
                    // 先尝试 slab，但前提是它能满足对齐要求
                    let zone_idx_opt = SlabAllocator::class_index_for(layout);

                    if let Some(zone_idx) = zone_idx_opt {
                        let usable_size = slab.zone_size_class(zone_idx);
                        let allocation = slab.alloc_class(zone_idx, cpu, &self.phys, &self.vmem);
                        if allocation.is_null() {
                            return Err(AllocationError::OutOfMemory);
                        }
                        if matches!(request.zeroing, Zeroing::Zeroed) {
                            #[cfg(feature = "performance-profile")]
                            let _profile =
                                profiling::scope(profiling::Event::MemZeroAllocatorSmall)
                                    .bytes(request.size);
                            unsafe {
                                core::ptr::write_bytes(allocation.ptr as *mut u8, 0, request.size);
                            }
                        }
                        let record = AllocationRecord::new(
                            AllocationKind::Small,
                            MemoryDomain::Kernel,
                            allocation.ptr,
                        )
                        .with_arena(allocation_arena)
                        .with_sizes(request.size, usable_size, request.align)
                        .with_accounting_owner(request.accounting_owner().unwrap_or(0))
                        .with_backend_cookie(allocation.backend_cookie);
                        if arena == crate::space::ArenaKind::Tracked {
                            self.register_allocation(record, || {
                                slab.free_record(record, cpu);
                            })?;
                        }
                        Ok(record)
                    } else {
                        // slab 无法满足此对齐要求，回退到 kheap
                        alloc_large()
                    }
                } else {
                    alloc_large()
                }
            }
            MemoryDomain::Physical => {
                let physical_request = PhysicalAllocRequest::new(request.size, request.align)
                    .with_page_policy(request.page_policy)
                    .with_placement(request.placement)
                    .with_accounting_owner(request.accounting_owner().unwrap_or(0));
                let allocation = self
                    .allocate_physical_raw(physical_request)
                    .map_err(AllocationError::from)?;
                let record = physical_record_from_allocation(
                    physical_request,
                    allocation,
                    request.accounting_owner().unwrap_or(0),
                );
                self.register_allocation(record, || {
                    let _ = self.free_physical_raw(allocation);
                })?;
                Ok(record)
            }
        }
    }

    fn allocate_physical_raw(
        &self,
        request: PhysicalAllocRequest,
    ) -> Result<PhysicalAllocation, buddy::BuddyAllocError> {
        let mut phys = self.phys.lock();
        phys.alloc_pages_with(&request)
    }

    fn free_physical_raw(&self, allocation: PhysicalAllocation) -> bool {
        self.try_free_physical_raw(allocation).is_ok()
    }

    fn try_free_physical_raw(
        &self,
        allocation: PhysicalAllocation,
    ) -> Result<(), buddy::BuddyFreeError> {
        let mut phys = self.phys.lock();
        phys.free_allocation(allocation)
    }

    fn current_cpu_id(&self) -> usize {
        match self.load_cpu_id_fn() {
            Some(f) => f(),
            None => 0,
        }
    }

    fn allocate_internal_metadata(&self, layout: Layout) -> *mut u8 {
        self.metadata
            .alloc(layout, &self.phys, self.load_phys_to_virt())
    }

    fn reclaim_allocator_caches_for_retry(&self) -> AllocatorReclaimStats {
        // 分配失败后的重试路径必须先归还 allocator 自己持有的可回收内存：
        // kheap range cache、slab per-CPU cache/空 slab、buddy order-0 延迟合并页。
        // 这不是周期性整理，而是 OOM 前的最后防线，避免大规模短生命周期任务结束后
        // 大量页仍停在内部缓存里，后续请求却继续向 buddy 要新页。
        self.reclaim(AllocatorReclaimRequest::caches())
            .unwrap_or_default()
    }

    pub fn load_phys_to_virt(&self) -> Option<PhysToVirtFn> {
        let raw = self.phys_to_virt.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, PhysToVirtFn>(raw) })
        }
    }

    /// 使用当前已绑定的地址转换规则把物理地址转换为内核虚拟地址。
    pub fn physical_to_virtual(&self, physical_address: usize) -> Option<usize> {
        self.load_phys_to_virt()
            .map(|translate| translate(physical_address))
    }

    pub fn load_virt_to_phys(&self) -> Option<VirtToPhysFn> {
        let raw = self.virt_to_phys.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, VirtToPhysFn>(raw) })
        }
    }

    /// 使用当前已绑定的地址转换规则把内核虚拟地址转换为物理地址。
    pub fn virtual_to_physical(&self, virtual_address: usize) -> Option<usize> {
        self.load_virt_to_phys()
            .map(|translate| translate(virtual_address))
    }

    fn load_cpu_id_fn(&self) -> Option<CpuIdFn> {
        let raw = self.cpu_id_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, CpuIdFn>(raw) })
        }
    }

    fn load_kernel_heap_region_fn(&self) -> Option<KernelHeapRegionFn> {
        let raw = self.kernel_heap_region_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, KernelHeapRegionFn>(raw) })
        }
    }

    fn load_tracked_heap_region_fn(&self) -> Option<KernelHeapRegionFn> {
        let raw = self.tracked_heap_region_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, KernelHeapRegionFn>(raw) })
        }
    }

    fn load_kernel_heap_map_fn(&self) -> Option<MapKernelHeapRangeFn> {
        let raw = self.kernel_heap_map_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, MapKernelHeapRangeFn>(raw) })
        }
    }

    fn load_kernel_heap_unmap_fn(&self) -> Option<UnmapKernelHeapRangeFn> {
        let raw = self.kernel_heap_unmap_fn.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, UnmapKernelHeapRangeFn>(raw) })
        }
    }

    fn can_reuse_allocation(&self, record: AllocationRecord, new_layout: Layout) -> bool {
        if !ptr_satisfies_align(record.ptr, new_layout.align()) {
            return false;
        }
        match record.kind {
            AllocationKind::Small => {
                is_small(new_layout)
                    && new_layout.pad_to_align().size() <= record.usable_size
                    && new_layout.align() <= PAGE_SIZE
            }
            AllocationKind::Large => {
                !is_small(new_layout) && KernelHeap::required_order_for(new_layout) == record.order
            }
            AllocationKind::Boot | AllocationKind::Physical => false,
        }
    }

    fn release_moved_kernel_record(&self, record: AllocationRecord) {
        // `reallocate` 迁移路径已经把旧对象从 registry 移除。这里直接释放对应后端，避免
        // 再进入通用 `deallocate` 做一次查账和分派；如果后端拒绝释放，说明 registry 与
        // 后端状态已经不一致，必须作为 allocator invariant 暴露。
        let (slab, kheap) = match record.arena {
            Some(AllocationArena::Tracked) => (&self.tracked_slab, &self.tracked_kheap),
            Some(AllocationArena::Kernel) => (&self.slab, &self.kheap),
            _ => panic!(
                "[alloc][invariant] movable allocation has invalid arena: {:?}",
                record
            ),
        };
        match record.kind {
            AllocationKind::Small => {
                if !slab.free_record(record, self.current_cpu_id()) {
                    panic!(
                        "[alloc][invariant] moved small allocation release failed: ptr={:#x} size={} usable={}",
                        record.ptr, record.size, record.usable_size
                    );
                }
            }
            AllocationKind::Large => {
                if let Err(err) = kheap.free_record(record, &self.phys, &self.vmem) {
                    panic!(
                        "[alloc][invariant] moved large allocation release failed: ptr={:#x} paddr={:?} order={} err={:?}",
                        record.ptr, record.paddr, record.order, err
                    );
                }
            }
            _ => panic!(
                "[alloc][invariant] reallocate tried to release non-kernel movable record: {:?}",
                record
            ),
        }
        release_accounting(record.accounting_owner(), record.size);
    }

    fn retire_moved_kernel_allocation<F>(
        &self,
        ptr: usize,
        expected: AllocationRecord,
        cleanup_new: F,
    ) where
        F: FnOnce(),
    {
        if let Err(err) = self.owner_index.untrack(expected) {
            cleanup_new();
            panic!(
                "[alloc][invariant] reallocate could not remove old owner range: ptr={:#x} err={:?}",
                ptr, err
            );
        }
        let removed = match self.registry.remove_result(ptr) {
            Ok(record) => record,
            Err(err) => {
                if let Err(restore_err) = self.owner_index.track(expected) {
                    cleanup_new();
                    panic!(
                        "[alloc][invariant] reallocate owner range restore failed: ptr={:#x} remove={:?} restore={:?}",
                        ptr, err, restore_err
                    );
                }
                cleanup_new();
                panic!(
                    "[alloc][invariant] reallocate lost old registry record: ptr={:#x} err={:?}",
                    ptr, err
                );
            }
        };
        if removed != expected {
            self.restore_owner_index_or_panic(removed, "reallocate removed unexpected record");
            cleanup_new();
            panic!(
                "[alloc][invariant] reallocate removed unexpected record: ptr={:#x} expected={:?} removed={:?}",
                ptr, expected, removed
            );
        }
        self.release_moved_kernel_record(removed);
    }

    fn probe_tracked_realloc(
        &self,
        ptr: usize,
        new_layout: Layout,
        new_size: usize,
    ) -> Result<TrackedReallocProbe, RegistryError> {
        let record = match self.registry.get_result(ptr) {
            Ok(record) => record,
            Err(RegistryError::UnknownPointer) => return Ok(TrackedReallocProbe::Untracked),
            Err(err) => return Err(err),
        };
        if !self.can_reuse_allocation(record, new_layout) {
            return Ok(TrackedReallocProbe::NeedsMove(record));
        }
        if !try_resize_accounting(record.accounting_owner(), record.size, new_size) {
            return Ok(TrackedReallocProbe::QuotaDenied);
        }

        let mut updated = record;
        updated.size = new_size;
        updated.align = new_layout.align();
        if let Err(err) = self.registry.update_existing_result(ptr, updated) {
            let _ = try_resize_accounting(record.accounting_owner(), new_size, record.size);
            return Err(err);
        }
        if let Err(err) = self.owner_index.update(record, updated) {
            let _ = self.registry.update_existing_result(ptr, record);
            let _ = try_resize_accounting(record.accounting_owner(), new_size, record.size);
            panic!(
                "[alloc][invariant] owner range update failed after registry resize: ptr={:#x} err={:?}",
                ptr, err
            );
        }
        Ok(TrackedReallocProbe::Updated {
            old_size: record.size,
            record: updated,
        })
    }

    fn record_oom(&self) {
        self.oom_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_ownership_failure(&self) {
        self.ownership_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn record_global_dealloc_stats(&self, layout: Layout) {
        // `total_*` 统计的是 Rust `GlobalAlloc` 前端看到的请求，而不是所有 typed
        // allocator API。`realloc` 搬迁路径绕过通用 `dealloc` 释放旧对象时，也必须
        // 维持同一口径，benchmark 才能稳定拆分 alloc/free/realloc 成本。
        self.total_deallocs.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_freed
            .fetch_add(layout.size() as u64, Ordering::Relaxed);
    }

    fn restore_owner_index_or_panic(&self, record: AllocationRecord, context: &str) {
        if let Err(err) = self.owner_index.track(record) {
            panic!(
                "[alloc][invariant] {}: owner index restore failed ptr={:#x} owner={} err={:?}",
                context,
                record.ptr,
                record.accounting_owner(),
                err
            );
        }
    }

    fn restore_tracked_record_or_panic(&self, record: AllocationRecord, context: &str) {
        self.restore_owner_index_or_panic(record, context);
        if let Err(err) = self.registry.register_result(&self.boot, record) {
            panic!(
                "[alloc][invariant] {}: registry restore failed ptr={:#x} err={:?}",
                context, record.ptr, err
            );
        }
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
            Ok(()) => match self.owner_index.track(record) {
                Ok(()) => Ok(()),
                Err(err) => {
                    if let Err(remove_err) = self.registry.remove_result(record.ptr) {
                        panic!(
                            "[alloc][invariant] owner index failure could not roll back registry: ptr={:#x} owner_err={:?} registry_err={:?}",
                            record.ptr, err, remove_err
                        );
                    }
                    rollback();
                    Err(allocation_error_from_owner_index(err))
                }
            },
            Err(RegistryError::DuplicatePointer) => {
                rollback();
                Err(allocation_error_from_registry(
                    RegistryError::DuplicatePointer,
                ))
            }
            Err(err) => {
                rollback();
                Err(allocation_error_from_registry(err))
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

fn physical_record_from_allocation(
    request: PhysicalAllocRequest,
    allocation: PhysicalAllocation,
    accounting_owner: u64,
) -> AllocationRecord {
    AllocationRecord::new(
        AllocationKind::Physical,
        MemoryDomain::Physical,
        allocation.paddr,
    )
    .with_physical(allocation.paddr, allocation.order, allocation.page_size)
    .with_sizes(request.size, allocation.size, request.align)
    .with_accounting_owner(accounting_owner)
}

fn physical_allocation_from_record(record: AllocationRecord) -> PhysicalAllocation {
    PhysicalAllocation {
        paddr: record.paddr.unwrap_or(record.ptr),
        size: record.usable_size.max(record.size),
        order: record.order,
        page_size: record.page_size,
    }
}

fn validate_physical_free_record(
    record: AllocationRecord,
    allocation: PhysicalAllocation,
) -> Result<(), PhysicalFreeError> {
    if record.kind != AllocationKind::Physical {
        return Err(PhysicalFreeError::InvalidRecordKind {
            actual: record.kind,
        });
    }

    let expected_paddr = record.paddr.unwrap_or(record.ptr);
    if expected_paddr != allocation.paddr {
        return Err(PhysicalFreeError::AddressMismatch {
            expected: expected_paddr,
            actual: allocation.paddr,
        });
    }
    if record.order != allocation.order {
        return Err(PhysicalFreeError::OrderMismatch {
            expected: record.order,
            actual: allocation.order,
        });
    }

    if record.page_size != allocation.page_size {
        return Err(PhysicalFreeError::PageSizeMismatch {
            expected: record.page_size,
            actual: allocation.page_size,
        });
    }

    let expected_size = record.usable_size.max(record.size);
    if expected_size != allocation.size {
        return Err(PhysicalFreeError::SizeMismatch {
            expected: expected_size,
            actual: allocation.size,
        });
    }
    Ok(())
}

fn allocation_error_from_registry(err: RegistryError) -> AllocationError {
    match err {
        RegistryError::NotInitialized => AllocationError::NotInitialized,
        RegistryError::MetadataOutOfMemory => AllocationError::OutOfMemory,
        RegistryError::InvalidRecord
        | RegistryError::DuplicatePointer
        | RegistryError::UnknownPointer => AllocationError::InvalidLayout,
    }
}

fn allocation_error_from_owner_index(err: OwnerIndexError) -> AllocationError {
    match err {
        OwnerIndexError::NotInitialized => AllocationError::NotInitialized,
        OwnerIndexError::MetadataOutOfMemory => AllocationError::OutOfMemory,
        OwnerIndexError::InvalidOwner
        | OwnerIndexError::InvalidRange
        | OwnerIndexError::UnknownOwner
        | OwnerIndexError::UnknownRange
        | OwnerIndexError::Overlap
        | OwnerIndexError::Corrupt => AllocationError::InvalidLayout,
    }
}

fn buddy_alloc_error_from_owner_index(err: OwnerIndexError) -> buddy::BuddyAllocError {
    match err {
        OwnerIndexError::NotInitialized => buddy::BuddyAllocError::NotInitialized,
        OwnerIndexError::MetadataOutOfMemory => buddy::BuddyAllocError::MetadataOutOfMemory,
        OwnerIndexError::Overlap => buddy::BuddyAllocError::BlockNotFree,
        OwnerIndexError::InvalidOwner
        | OwnerIndexError::InvalidRange
        | OwnerIndexError::UnknownOwner
        | OwnerIndexError::UnknownRange
        | OwnerIndexError::Corrupt => buddy::BuddyAllocError::InvalidAddress,
    }
}

fn buddy_alloc_error_from_request(err: AllocationRequestError) -> buddy::BuddyAllocError {
    match err {
        AllocationRequestError::InvalidSize
        | AllocationRequestError::InvalidAlignment
        | AllocationRequestError::SizeOverflow
        | AllocationRequestError::UnsupportedOrder => buddy::BuddyAllocError::InvalidOrder,
        AllocationRequestError::InvalidPlacement => buddy::BuddyAllocError::InvalidAddress,
    }
}

fn ptr_satisfies_align(ptr: usize, align: usize) -> bool {
    align != 0 && align.is_power_of_two() && (ptr & (align - 1)) == 0
}

fn realloc_copy_source_size(record: AllocationRecord, fallback_layout_size: usize) -> usize {
    // boot 分配没有逐对象账本，active 后只能合成 size=0 的记录；realloc 仍需按
    // 调用方传入的旧 Layout 保留内容，其它路径则以 registry 真实大小为准。
    if matches!(record.kind, AllocationKind::Boot) && record.size == 0 {
        fallback_layout_size
    } else {
        record.size
    }
}

pub(crate) fn alloc_internal_metadata(layout: Layout) -> *mut u8 {
    KERNEL_ALLOCATOR.allocate_internal_metadata(layout)
}

#[kernel_symbols::export]
unsafe impl GlobalAlloc for KernelMemorySubsystem {
    #[kernel_symbols::export(
        name = "allocator.GlobalAlloc.alloc",
        contract = "kernel.allocator.global-alloc@1",
        version = 1,
        capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
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

    #[kernel_symbols::export(
        name = "allocator.GlobalAlloc.dealloc",
        contract = "kernel.allocator.global-alloc@1",
        version = 1,
        capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
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

        let result = if self.is_tracked_heap_pointer(ptr as usize) {
            self.deallocate(ptr as usize)
        } else {
            self.deallocate_untracked_global(ptr as usize, layout)
        };
        if result.is_err() {
            self.record_ownership_failure();
        }
    }

    #[kernel_symbols::export(
        name = "allocator.GlobalAlloc.realloc",
        contract = "kernel.allocator.global-alloc@1",
        version = 1,
        capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.total_reallocs.fetch_add(1, Ordering::Relaxed);
        if self.is_tracked_heap_pointer(ptr as usize) {
            self.tracked_kheap.record_realloc();
        } else {
            self.kheap.record_realloc();
        }

        if ptr.is_null() {
            let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
                self.record_oom();
                return null_mut();
            };
            return unsafe { self.alloc(new_layout) };
        }

        if new_size == 0 {
            unsafe { self.dealloc(ptr, layout) };
            return null_mut();
        }

        let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
            self.record_oom();
            return null_mut();
        };
        let active = self.active.load(Ordering::Acquire);
        if active
            && !self.boot.contains(ptr as usize)
            && !self.is_tracked_heap_pointer(ptr as usize)
        {
            return unsafe { self.reallocate_untracked_global(ptr, layout, new_layout) };
        }
        let owner = if active {
            match self.probe_tracked_realloc(ptr as usize, new_layout, new_size) {
                Ok(TrackedReallocProbe::Updated { .. }) => return ptr,
                Ok(TrackedReallocProbe::NeedsMove(record)) => Some(record),
                Ok(TrackedReallocProbe::QuotaDenied) => {
                    self.record_oom();
                    return null_mut();
                }
                Ok(TrackedReallocProbe::Untracked) if self.boot.contains(ptr as usize) => Some(
                    AllocationRecord::new(AllocationKind::Boot, MemoryDomain::Kernel, ptr as usize)
                        .with_sizes(layout.size(), layout.size(), layout.align()),
                ),
                Ok(TrackedReallocProbe::Untracked) => None,
                Err(_) => {
                    self.record_ownership_failure();
                    return null_mut();
                }
            }
        } else if self.boot.contains(ptr as usize) {
            Some(
                AllocationRecord::new(AllocationKind::Boot, MemoryDomain::Kernel, ptr as usize)
                    .with_sizes(layout.size(), layout.size(), layout.align()),
            )
        } else {
            None
        };

        let Some(owner) = owner else {
            if active {
                self.record_ownership_failure();
            }
            return null_mut();
        };
        if active
            && !matches!(
                owner.kind,
                AllocationKind::Boot | AllocationKind::Small | AllocationKind::Large
            )
        {
            self.record_ownership_failure();
            return null_mut();
        }

        let request = MemoryRequest::for_kernel_layout(new_layout)
            .with_reclaim(ReclaimPolicy::TryAllocatorReclaim)
            .with_accounting_owner(owner.accounting_owner());
        self.total_allocs.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_allocated
            .fetch_add(new_layout.size() as u64, Ordering::Relaxed);
        let new_record = match self.allocate(request) {
            Ok(record) => record,
            Err(_) => {
                self.record_oom();
                return null_mut();
            }
        };
        let new_ptr = new_record.ptr as *mut u8;

        let old_size = realloc_copy_source_size(owner, layout.size());
        let copy_len = old_size.min(new_size);
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::MemCopyRealloc).bytes(copy_len);
        unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr, copy_len) };

        if active {
            match owner.kind {
                AllocationKind::Boot => {}
                AllocationKind::Small | AllocationKind::Large => {
                    self.retire_moved_kernel_allocation(ptr as usize, owner, || unsafe {
                        self.dealloc(new_ptr, new_layout)
                    });
                    self.record_global_dealloc_stats(layout);
                }
                AllocationKind::Physical => {
                    self.record_ownership_failure();
                    unsafe { self.dealloc(new_ptr, new_layout) };
                    return null_mut();
                }
            }
        }

        new_ptr
    }

    #[kernel_symbols::export(
        name = "allocator.GlobalAlloc.alloc_zeroed",
        contract = "kernel.allocator.global-alloc@1",
        version = 1,
        capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
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

#[cfg(feature = "ktest-kernel")]
mod tests;

#[cfg(test)]
mod host_tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::{KernelMemorySubsystem, PagePolicy};

    static TRACKED_REGION_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TRACKED_REGION_START: AtomicUsize = AtomicUsize::new(0x4000);
    static TRACKED_REGION_SIZE: AtomicUsize = AtomicUsize::new(0x2000);
    static TEST_ALLOCATOR: KernelMemorySubsystem = KernelMemorySubsystem::new();

    fn kernel_region() -> (usize, usize) {
        (0x1000, 0x1000)
    }

    fn tracked_region() -> (usize, usize) {
        TRACKED_REGION_CALLS.fetch_add(1, Ordering::Relaxed);
        (
            TRACKED_REGION_START.load(Ordering::Relaxed),
            TRACKED_REGION_SIZE.load(Ordering::Relaxed),
        )
    }

    fn map_range(_vaddr: usize, _paddr: usize, _size: usize, _page_policy: PagePolicy) -> bool {
        true
    }

    fn unmap_range(_vaddr: usize, _size: usize) -> bool {
        true
    }

    #[test]
    fn tracked_heap_range_is_cached_with_exact_boundaries() {
        TRACKED_REGION_CALLS.store(0, Ordering::Relaxed);
        TRACKED_REGION_START.store(0x4000, Ordering::Relaxed);
        TRACKED_REGION_SIZE.store(0x2000, Ordering::Relaxed);
        TEST_ALLOCATOR.bind_kernel_heap_ops(kernel_region, tracked_region, map_range, unmap_range);

        assert!(!TEST_ALLOCATOR.is_tracked_heap_pointer(0x3fff));
        assert!(TEST_ALLOCATOR.is_tracked_heap_pointer(0x4000));
        assert!(TEST_ALLOCATOR.is_tracked_heap_pointer(0x5fff));
        assert!(!TEST_ALLOCATOR.is_tracked_heap_pointer(0x6000));
        assert_eq!(TRACKED_REGION_CALLS.load(Ordering::Relaxed), 1);

        TRACKED_REGION_START.store(0x7000, Ordering::Relaxed);
        TRACKED_REGION_SIZE.store(0, Ordering::Relaxed);
        TEST_ALLOCATOR.bind_kernel_heap_ops(kernel_region, tracked_region, map_range, unmap_range);
        assert!(!TEST_ALLOCATOR.is_tracked_heap_pointer(0x7000));
        assert_eq!(TRACKED_REGION_CALLS.load(Ordering::Relaxed), 2);

        TRACKED_REGION_START.store(usize::MAX - 1, Ordering::Relaxed);
        TRACKED_REGION_SIZE.store(4, Ordering::Relaxed);
        TEST_ALLOCATOR.bind_kernel_heap_ops(kernel_region, tracked_region, map_range, unmap_range);
        assert!(!TEST_ALLOCATOR.is_tracked_heap_pointer(usize::MAX - 1));
        assert_eq!(TRACKED_REGION_CALLS.load(Ordering::Relaxed), 3);
    }
}

/// 内核内存子系统的唯一状态实例；最终二进制自行选择是否把它安装为全局分配器。
#[kernel_symbols::export(
    name = "allocator.KERNEL_ALLOCATOR",
    contract = "kernel.allocator.root@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub static KERNEL_ALLOCATOR: KernelMemorySubsystem = KernelMemorySubsystem::new();
