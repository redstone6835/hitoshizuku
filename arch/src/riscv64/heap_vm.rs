//! RISC-V64 内核堆页表映射实现。
//!
//! 本模块负责把 allocator 提供的"内核堆虚拟地址范围"和"物理页帧分配结果"
//! 连接起来，使上层的大对象分配器、slab 扩容以及其它基于内核堆的设施能够
//! 真正访问到有效内存。
//!
//! ## 与 [`paging`](super::paging) 的分工
//!
//! - `paging` 负责描述 RISC-V64 页表项格式、层级、TLB 操作；
//! - 本模块负责在运行时"走页表、创建中间页表页、写入叶子映射、解除映射"。
//!
//! ## 虚拟地址布局（Sv48）
//!
//! ```text
//! PGD[511] (0xFFFF_FF80_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF)
//!   PUD[0]:    kernel heap（1GiB，按需映射 4K / 2M 页）
//!   PUD[1]:    kernel heap（1GiB，按需映射 4K / 2M 页）
//!   PUD[2]:    kernel code direct map（1GiB，2MiB leaf；no-map 边界按 4KiB 拆分）
//!   PUD[3..17]: kernel direct map 扩展（通常为 1GiB leaf，no-map 窗口按需拆分）
//!
//! PGD[510] (0xFFFF_FF00_0000_0000 .. 0xFFFF_FF80_0000_0000)
//!   PUD[0]:    MMIO 直接映射（1GiB leaf，PA 0x0..0x4000_0000）
//!   PUD[1]:    PCI 32-bit BAR 窗口（1GiB leaf，PA 0x4000_0000..0x8000_0000）
//! ```
//!
//! ## 大页策略
//!
//! 与 LoongArch 原型一致，支持三种策略：
//! - `BaseOnly`：强制 4KiB 基本页
//! - `PreferLarge`：优先 2MiB 大页，失败降级到 4KiB
//! - `RequireLarge`：强制 2MiB 大页，失败返回错误

use alloc::vec::Vec;
use allocator::{
    MemoryDomain, MemoryRequest, PAGE_SIZE, PagePolicy, PhysicalAllocRequest, PhysicalAllocation,
    Zeroing,
};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use general::{
    MapError, PagingArch, PhysPageTableRoot, StartNoMapError, StartPhysRange, find_leaf,
    replace_empty_table_with_leaf, unmap_range_entries, validate_range_permissions,
};
use spin::Mutex;

use crate::riscv64::paging::{Riscv64Paging, Riscv64Pte};
use crate::riscv64::specific::{
    IRQ_STACK_GUARD_SIZE, IRQ_STACK_SIZE, KERNEL_VA_OFFSET, SATP_MODE_SV48, phys_to_virt,
    virt_to_phys,
};
use crate::riscv64::trap::LocalIrqGuard;

// ── 常量与静态 ──────────────────────────────────────────────────────────────────

/// 内核堆虚拟地址起始（PGD[511]→PUD[0]）。
pub const KERNEL_HEAP_BASE: usize = 0xFFFF_FF80_0000_0000;

/// 内核堆虚拟地址范围大小（2 GiB）。
///
/// 对应页表布局中 PGD[511]→PUD[0..1]，每 PUD 覆盖 1 GiB。
pub const KERNEL_HEAP_SIZE: usize = 2 * 1024 * 1024 * 1024;

/// debug 构建为页表 map/unmap/rollback 自检保留一个不会交给 vmem 的窗口。
#[cfg(debug_assertions)]
const HEAP_VM_SELFTEST_SIZE: usize = 4 * 1024 * 1024;
#[cfg(not(debug_assertions))]
const HEAP_VM_SELFTEST_SIZE: usize = 0;

/// heap window 顶部依次保留 debug self-test、guard page 与 boot hart emergency stack，
/// 因此 allocator 可使用的 heap 区间必须在这些窗口之前结束。
const EMERGENCY_STACK_WINDOW_SIZE: usize = IRQ_STACK_GUARD_SIZE + IRQ_STACK_SIZE;
const KERNEL_HEAP_USABLE_SIZE: usize =
    KERNEL_HEAP_SIZE - HEAP_VM_SELFTEST_SIZE - EMERGENCY_STACK_WINDOW_SIZE;
const KERNEL_HEAP_USABLE_END: usize = KERNEL_HEAP_BASE + KERNEL_HEAP_USABLE_SIZE;
#[cfg(debug_assertions)]
const HEAP_VM_SELFTEST_BASE: usize = KERNEL_HEAP_USABLE_END;
#[cfg(debug_assertions)]
const HEAP_VM_SELFTEST_END: usize = HEAP_VM_SELFTEST_BASE + HEAP_VM_SELFTEST_SIZE;
const EMERGENCY_STACK_GUARD_BASE: usize = KERNEL_HEAP_USABLE_END + HEAP_VM_SELFTEST_SIZE;
const EMERGENCY_STACK_BASE: usize = EMERGENCY_STACK_GUARD_BASE + IRQ_STACK_GUARD_SIZE;
const EMERGENCY_STACK_END: usize = EMERGENCY_STACK_BASE + IRQ_STACK_SIZE;

/// MMIO 直接映射基址（PGD[510]，独立于 kernel heap/code）。
///
/// `device_mmio_to_virt(paddr) = paddr + MMIO_VIRT_BASE`。
pub const MMIO_VIRT_BASE: usize = 0xFFFF_FF00_0000_0000;

/// 内核 direct map 覆盖物理 RAM 的基址和大小（QEMU virt 默认从 0x80000000 开始）。
const KERNEL_PHYS_BASE: usize = 0x8000_0000;
const KERNEL_DIRECT_MAP_WINDOW_SIZE: usize = 0x4000_0000; // 1 GiB
const KERNEL_DIRECT_MAP_PUD_START: usize = 2;
/// 正式页表覆盖的 RAM 物理范围：从 PUD[2] 连续映射 16 个 1 GiB 窗口。
/// 这覆盖 QEMU 16 GiB 配置（扣除起始物理地址后的可用 RAM）并保留首窗的 W^X 细分。
const KERNEL_DIRECT_MAP_PUD_COUNT: usize = 16;
pub const KERNEL_DIRECT_MAP_PHYS_START: usize = KERNEL_PHYS_BASE;
pub const KERNEL_DIRECT_MAP_PHYS_END: usize =
    KERNEL_PHYS_BASE + KERNEL_DIRECT_MAP_PUD_COUNT * KERNEL_DIRECT_MAP_WINDOW_SIZE;
const HEAP_PMD_SIZE: usize = 2 * 1024 * 1024;
const HEAP_PUD_SIZE: usize = 1024 * 1024 * 1024;
/// RISC-V 正式线性映射落实 `no-map` 的最小粒度。
pub const NO_MAP_GRANULE: usize = PAGE_SIZE;
const PTE_VALID: usize = 1 << 0;
const PTE_SOFTWARE_LEVEL_SHIFT: usize = 8;
const PTE_SOFTWARE_LEVEL_MASK: usize = 0b11 << PTE_SOFTWARE_LEVEL_SHIFT;

/// 内核 direct map 对应的虚拟基址（高半区，独立于 identity phys_to_virt）。
const KERNEL_VIRT_BASE: usize = KERNEL_PHYS_BASE.wrapping_add(KERNEL_VA_OFFSET);

pub(crate) static KERNEL_PAGE_TABLE_ROOT: AtomicUsize = AtomicUsize::new(0);

const PAGE_TABLE_UNINITIALIZED: usize = 0;
const PAGE_TABLE_INITIALIZING: usize = 1;
const PAGE_TABLE_INITIALIZED: usize = 2;
static PAGE_TABLE_INIT_STATE: AtomicUsize = AtomicUsize::new(PAGE_TABLE_UNINITIALIZED);
static SECONDARY_IDENTITY_PUD: AtomicUsize = AtomicUsize::new(0);

const NO_MAP_UNPREPARED: usize = 0;
const NO_MAP_PREPARING: usize = 1;
const NO_MAP_RANGES_READY: usize = 2;
const NO_MAP_PREPARED: usize = 3;
static NO_MAP_PREPARE_STATE: AtomicUsize = AtomicUsize::new(NO_MAP_UNPREPARED);
static NO_MAP_SNAPSHOT: AtomicPtr<NoMapRangeSnapshot> = AtomicPtr::new(core::ptr::null_mut());
static DIRECT_MAP_SPLIT_PUD_WINDOWS: AtomicUsize = AtomicUsize::new(0);
static DIRECT_MAP_SPLIT_PMD_CHUNKS: AtomicUsize = AtomicUsize::new(0);
static DIRECT_MAP_UNMAPPED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// boot heap 中永久保存的 `no-map` 只读切片描述符。
struct NoMapRangeSnapshot {
    ranges: *const StartPhysRange,
    len: usize,
}

/// 动态 kernel heap 页表的结构锁。
///
/// allocator 会在不持有自身锁时调用映射回调，因此页表层必须自行序列化父 PTE
/// 的创建、叶映射更新和空页表回收。调用侧同时关闭本地中断，避免同一 hart
/// 在页表临界区内被可分配内存的中断处理路径重入。
static KERNEL_HEAP_PAGE_TABLE_LOCK: Mutex<()> = Mutex::new(());

static BASE_PAGE_MAPS: AtomicUsize = AtomicUsize::new(0);
static LARGE_PAGE_MAPS: AtomicUsize = AtomicUsize::new(0);
static LARGE_PAGE_FALLBACKS: AtomicUsize = AtomicUsize::new(0);
static MAP_ROLLBACKS: AtomicUsize = AtomicUsize::new(0);
static TLB_ADDRESS_FLUSHES: AtomicUsize = AtomicUsize::new(0);
static TLB_GLOBAL_FLUSHES: AtomicUsize = AtomicUsize::new(0);
static PAGE_TABLE_PAGES_RECLAIMED: AtomicUsize = AtomicUsize::new(0);
static PAGE_TABLE_ALLOCATION_FAILURES: AtomicUsize = AtomicUsize::new(0);
static PAGE_TABLE_CORRUPTIONS: AtomicUsize = AtomicUsize::new(0);

#[cfg(debug_assertions)]
const NO_PAGE_TABLE_ALLOCATION_FAILURE: usize = usize::MAX;
#[cfg(debug_assertions)]
static FAIL_PAGE_TABLE_ALLOCATION_AFTER: AtomicUsize =
    AtomicUsize::new(NO_PAGE_TABLE_ALLOCATION_FAILURE);

const TLB_ADDRESS_THRESHOLD: usize = 64;
const MAX_RECLAIMED_TABLES: usize = KERNEL_HEAP_SIZE / HEAP_PMD_SIZE + 2;

/// 页表回收不能在持有 heap 页表锁时再向 allocator 申请临时 Vec，否则可能递归进入
/// map 回调。该固定 scratch 由同一把全局锁和本地关中断共同保护。
struct ReclaimScratch(UnsafeCell<[usize; MAX_RECLAIMED_TABLES]>);
unsafe impl Sync for ReclaimScratch {}
static RECLAIM_SCRATCH: ReclaimScratch = ReclaimScratch(UnsafeCell::new([0; MAX_RECLAIMED_TABLES]));

#[derive(Clone, Copy)]
struct KernelTlbFlushPlan {
    addresses: [usize; TLB_ADDRESS_THRESHOLD],
    count: usize,
    global: bool,
}

impl KernelTlbFlushPlan {
    const fn new() -> Self {
        Self {
            addresses: [0; TLB_ADDRESS_THRESHOLD],
            count: 0,
            global: false,
        }
    }

    fn push(&mut self, vaddr: usize) {
        if self.global {
            return;
        }
        if self.count == self.addresses.len() {
            self.global = true;
            return;
        }
        self.addresses[self.count] = vaddr;
        self.count += 1;
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KernelHeapVmStats {
    pub base_page_maps: usize,
    pub large_page_maps: usize,
    pub large_page_fallbacks: usize,
    pub map_rollbacks: usize,
    pub tlb_address_flushes: usize,
    pub tlb_global_flushes: usize,
    pub page_table_pages_reclaimed: usize,
    pub page_table_allocation_failures: usize,
    pub page_table_corruptions: usize,
}

pub fn kernel_heap_vm_stats() -> KernelHeapVmStats {
    KernelHeapVmStats {
        base_page_maps: BASE_PAGE_MAPS.load(Ordering::Relaxed),
        large_page_maps: LARGE_PAGE_MAPS.load(Ordering::Relaxed),
        large_page_fallbacks: LARGE_PAGE_FALLBACKS.load(Ordering::Relaxed),
        map_rollbacks: MAP_ROLLBACKS.load(Ordering::Relaxed),
        tlb_address_flushes: TLB_ADDRESS_FLUSHES.load(Ordering::Relaxed),
        tlb_global_flushes: TLB_GLOBAL_FLUSHES.load(Ordering::Relaxed),
        page_table_pages_reclaimed: PAGE_TABLE_PAGES_RECLAIMED.load(Ordering::Relaxed),
        page_table_allocation_failures: PAGE_TABLE_ALLOCATION_FAILURES.load(Ordering::Relaxed),
        page_table_corruptions: PAGE_TABLE_CORRUPTIONS.load(Ordering::Relaxed),
    }
}

pub fn kernel_heap_region() -> (usize, usize) {
    (KERNEL_HEAP_BASE, KERNEL_HEAP_USABLE_SIZE)
}

/// 在物理 allocator 接管前登记 DT `no-map` 范围。
///
/// DT 范围按页向外扩展后裁剪到本架构实际建立的 RAM 线性映射。最终范围会排序、
/// 合并并作为 boot heap 只读快照发布；随后立即从 boot heap 构造正式直映。因此
/// buddy 在初始化任意高端 RAM 元数据之前，1GiB、2MiB 或 4KiB 叶项已精确留出
/// 全部空洞。
pub(crate) fn prepare_no_map(ranges: &[StartPhysRange]) -> Result<(), StartNoMapError> {
    if PAGE_TABLE_INIT_STATE.load(Ordering::Acquire) != PAGE_TABLE_UNINITIALIZED {
        return Err(StartNoMapError::InvalidRange);
    }
    if NO_MAP_PREPARE_STATE
        .compare_exchange(
            NO_MAP_UNPREPARED,
            NO_MAP_PREPARING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return Err(StartNoMapError::InvalidRange);
    }

    let normalized = match normalize_no_map_ranges(ranges) {
        Ok(normalized) => normalized,
        Err(err) => {
            rollback_no_map_preparation();
            return Err(err);
        }
    };
    let count = normalized.len();
    let snapshot = match allocate_no_map_snapshot(&normalized) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            rollback_no_map_preparation();
            return Err(err);
        }
    };
    NO_MAP_SNAPSHOT.store(snapshot, Ordering::Relaxed);
    NO_MAP_PREPARE_STATE.store(NO_MAP_RANGES_READY, Ordering::Release);

    let layout = locate_early_page_tables();
    validate_minimal_boot_direct_map(&layout);
    let direct_map = match build_direct_map_page_tables() {
        Ok(direct_map) => direct_map,
        Err(err) => {
            rollback_no_map_preparation();
            return Err(err);
        }
    };
    publish_boot_direct_map(&layout, &direct_map);
    DIRECT_MAP_SPLIT_PUD_WINDOWS.store(direct_map.split_pud_windows, Ordering::Release);
    DIRECT_MAP_SPLIT_PMD_CHUNKS.store(direct_map.split_pmd_chunks, Ordering::Release);
    DIRECT_MAP_UNMAPPED_PAGES.store(direct_map.unmapped_pages, Ordering::Release);
    core::mem::forget(normalized);
    NO_MAP_PREPARE_STATE.store(NO_MAP_PREPARED, Ordering::Release);
    log::info!(
        "[arch][heap_vm] published boot direct map: no-map={} split-pud={} split-pmd={} unmapped-pages={} granule={} KiB",
        count,
        direct_map.split_pud_windows,
        direct_map.split_pmd_chunks,
        direct_map.unmapped_pages,
        NO_MAP_GRANULE / 1024
    );
    Ok(())
}

fn normalize_no_map_ranges(
    ranges: &[StartPhysRange],
) -> Result<Vec<StartPhysRange>, StartNoMapError> {
    unsafe extern "C" {
        fn skernel();
        fn ekernel();
    }
    let kernel_start = crate::riscv64::specific::virt_to_phys(skernel as usize);
    let kernel_end = crate::riscv64::specific::virt_to_phys(ekernel as usize);
    if kernel_end <= kernel_start {
        return Err(StartNoMapError::InvalidRange);
    }

    // 先完整校验再触碰 boot heap，错误输入不会消耗不可回收的 bump 空间。
    let mut relevant_count = 0usize;
    for &range in ranges {
        if normalize_no_map_range(range, kernel_start, kernel_end)?.is_some() {
            relevant_count = relevant_count
                .checked_add(1)
                .ok_or(StartNoMapError::OutOfMemory)?;
        }
    }

    let mut normalized = Vec::new();
    normalized
        .try_reserve_exact(relevant_count)
        .map_err(|_| StartNoMapError::OutOfMemory)?;
    for &range in ranges {
        if let Some(range) = normalize_no_map_range(range, kernel_start, kernel_end)? {
            normalized.push(range);
        }
    }

    normalized.sort_unstable_by_key(|range| range.start);
    let mut merged = 0usize;
    for index in 0..normalized.len() {
        let range = normalized[index];
        if merged > 0 && range.start <= normalized[merged - 1].end {
            normalized[merged - 1].end = normalized[merged - 1].end.max(range.end);
        } else {
            normalized[merged] = range;
            merged += 1;
        }
    }
    normalized.truncate(merged);
    Ok(normalized)
}

fn normalize_no_map_range(
    range: StartPhysRange,
    kernel_start: usize,
    kernel_end: usize,
) -> Result<Option<StartPhysRange>, StartNoMapError> {
    if range.end <= range.start {
        return Err(StartNoMapError::InvalidRange);
    }

    let clipped_start = range.start.max(KERNEL_DIRECT_MAP_PHYS_START);
    let clipped_end = range.end.min(KERNEL_DIRECT_MAP_PHYS_END);
    if clipped_end <= clipped_start {
        return Ok(None);
    }
    let start = clipped_start & !(NO_MAP_GRANULE - 1);
    let end = clipped_end
        .checked_add(NO_MAP_GRANULE - 1)
        .ok_or(StartNoMapError::InvalidRange)?
        & !(NO_MAP_GRANULE - 1);
    if end <= start {
        return Err(StartNoMapError::InvalidRange);
    }
    if ranges_intersect(start, end, kernel_start, kernel_end) {
        return Err(StartNoMapError::OverlapsKernelImage);
    }
    Ok(Some(StartPhysRange::new(start, end)))
}

fn allocate_no_map_snapshot(
    ranges: &[StartPhysRange],
) -> Result<*mut NoMapRangeSnapshot, StartNoMapError> {
    let request = MemoryRequest::new(
        MemoryDomain::Kernel,
        core::mem::size_of::<NoMapRangeSnapshot>(),
        core::mem::align_of::<NoMapRangeSnapshot>(),
    );
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate(request)
        .map_err(|_| StartNoMapError::OutOfMemory)?;
    let snapshot = allocation.ptr as *mut NoMapRangeSnapshot;
    // Safety: allocation.ptr 是按 NoMapRangeSnapshot 大小和对齐从尚未封存的 boot heap
    // 取得的独占区域；描述符在状态发布前完整写入，此后不再修改。
    unsafe {
        snapshot.write(NoMapRangeSnapshot {
            ranges: ranges.as_ptr(),
            len: ranges.len(),
        });
    }
    Ok(snapshot)
}

fn rollback_no_map_preparation() {
    // 状态是快照可见性的唯一门闩。boot allocator 是 bump 模式，失败路径丢弃 Vec
    // 所有权不会复用底层区域；保留旧指针还可保证已观察到 READY 的读者不会悬空。
    NO_MAP_PREPARE_STATE.store(NO_MAP_PREPARING, Ordering::Release);
    NO_MAP_PREPARE_STATE.store(NO_MAP_UNPREPARED, Ordering::Release);
}

fn no_map_ranges() -> &'static [StartPhysRange] {
    match NO_MAP_PREPARE_STATE.load(Ordering::Acquire) {
        NO_MAP_UNPREPARED => &[],
        NO_MAP_RANGES_READY | NO_MAP_PREPARED => {
            let snapshot = NO_MAP_SNAPSHOT.load(Ordering::Acquire);
            assert!(
                !snapshot.is_null(),
                "[arch][heap_vm] missing published no-map snapshot"
            );
            // Safety: RANGES_READY/PREPARED 由写者在描述符和范围缓冲完成后以 Release
            // 发布。READY 阶段 Vec 仍由当前 boot hart 持有且不再修改，PREPARED 阶段
            // 所有权已永久泄漏；描述符及其范围缓冲均不会被复用。
            unsafe {
                let snapshot = &*snapshot;
                core::slice::from_raw_parts(snapshot.ranges, snapshot.len)
            }
        }
        _ => panic!("[arch][heap_vm] no-map preparation is still in progress"),
    }
}

#[inline]
const fn ranges_intersect(start: usize, end: usize, other_start: usize, other_end: usize) -> bool {
    start < other_end && other_start < end
}

fn no_map_intersects(start: usize, end: usize) -> bool {
    for range in no_map_ranges() {
        if range.start >= end {
            break;
        }
        if ranges_intersect(start, end, range.start, range.end) {
            return true;
        }
    }
    false
}

fn no_map_covers(start: usize, end: usize) -> bool {
    no_map_ranges()
        .iter()
        .take_while(|range| range.start <= start)
        .any(|range| range.end >= end)
}

fn no_map_range_containing(paddr: usize) -> Option<StartPhysRange> {
    for &range in no_map_ranges() {
        if paddr < range.start {
            return None;
        }
        if paddr < range.end {
            return Some(range);
        }
    }
    None
}

fn install_boot_emergency_stack() -> Result<(), MapError> {
    let request = PhysicalAllocRequest::new(IRQ_STACK_SIZE, PAGE_SIZE);
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map_err(|_| MapError::OutOfMemory)?;

    unsafe {
        crate::riscv64::specific::zero_memory_fast(phys_to_virt(allocation.paddr), IRQ_STACK_SIZE);
    }
    let map_result = map_kernel_range_in_window(
        EMERGENCY_STACK_BASE,
        allocation.paddr,
        IRQ_STACK_SIZE,
        PagePolicy::BaseOnly,
        EMERGENCY_STACK_BASE,
        EMERGENCY_STACK_END,
    );
    if let Err(err) = map_result {
        let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(allocation);
        return Err(err);
    }

    let top = EMERGENCY_STACK_END;
    let hart = crate::riscv64::specific::current_hart_ptr();
    unsafe {
        core::ptr::addr_of_mut!((*hart).irq_stack_top).write_volatile(top);
    }
    log::info!(
        "[arch][heap_vm] emergency stack mapped: guard={:#x} stack={:#x}..{:#x}",
        EMERGENCY_STACK_GUARD_BASE,
        EMERGENCY_STACK_BASE,
        top
    );
    Ok(())
}

#[cfg(debug_assertions)]
fn debug_verify_unpublished_page_table_transactions() {
    let test_vaddr = KERNEL_HEAP_BASE;
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .expect("[arch][heap_vm] debug transaction test page allocation failed");

    // heap 的 PUD 项初始为空，建立一个 4 KiB leaf 需要两张下级页表。分别在
    // 第一次和第二次分配处失败，验证私有子树不会被提前发布。
    for successful_allocations_before_failure in 0..=1usize {
        FAIL_PAGE_TABLE_ALLOCATION_AFTER
            .store(successful_allocations_before_failure, Ordering::Relaxed);
        let result = map_kernel_heap_range(
            test_vaddr,
            allocation.paddr,
            PAGE_SIZE,
            PagePolicy::BaseOnly,
        );
        assert!(
            matches!(result, Err(MapError::OutOfMemory)),
            "[arch][heap_vm] injected allocation failure returned {result:?}"
        );
        assert!(
            kernel_virt_to_phys(test_vaddr).is_err(),
            "[arch][heap_vm] failed private page-table branch became reachable"
        );
    }

    FAIL_PAGE_TABLE_ALLOCATION_AFTER.store(NO_PAGE_TABLE_ALLOCATION_FAILURE, Ordering::Relaxed);
    allocator::KERNEL_ALLOCATOR
        .try_free_physical(allocation)
        .expect("[arch][heap_vm] debug transaction test page free failed");
    log::info!("[arch][heap_vm] unpublished page-table fault injection passed");
}

/// allocator registry 激活后运行发布/回滚/回收自检。
///
/// 该函数只能从启动后期的 arch 注册点调用；专用虚拟窗口不会暴露给 vmem，避免
/// 自检踩到已经分配的 kernel heap range。
#[cfg(debug_assertions)]
pub(crate) fn debug_verify_heap_mapping_transactions() {
    assert!(
        allocator::KERNEL_ALLOCATOR.is_active(),
        "[arch][heap_vm] late transaction self-test ran before allocator activation"
    );
    let test_vaddr = (HEAP_VM_SELFTEST_BASE + HEAP_PMD_SIZE - 1) & !(HEAP_PMD_SIZE - 1);
    assert!(
        test_vaddr + HEAP_PMD_SIZE + PAGE_SIZE <= HEAP_VM_SELFTEST_END,
        "[arch][heap_vm] debug self-test window is too small"
    );
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .expect("[arch][heap_vm] debug reclaim test page allocation failed");

    // 重复发布、解除映射并回收空 PT。PUD/PMD 同时承载顶部 emergency stack，
    // 因此这里只要求每轮回收 self-test leaf 所属的 PT，并在释放前全局 flush。
    const RECLAIM_CYCLES: usize = 3;
    let reclaimed_before = PAGE_TABLE_PAGES_RECLAIMED.load(Ordering::Relaxed);
    let global_flushes_before = TLB_GLOBAL_FLUSHES.load(Ordering::Relaxed);
    for _ in 0..RECLAIM_CYCLES {
        map_kernel_range_in_window(
            test_vaddr,
            allocation.paddr,
            PAGE_SIZE,
            PagePolicy::BaseOnly,
            HEAP_VM_SELFTEST_BASE,
            HEAP_VM_SELFTEST_END,
        )
        .expect("[arch][heap_vm] debug transaction test map failed");
        let translated = kernel_virt_to_phys(test_vaddr)
            .expect("[arch][heap_vm] debug transaction test translation missing");
        assert_eq!(
            translated, allocation.paddr,
            "[arch][heap_vm] debug transaction test translation mismatch"
        );
        unmap_kernel_range_in_window(
            test_vaddr,
            PAGE_SIZE,
            HEAP_VM_SELFTEST_BASE,
            HEAP_VM_SELFTEST_END,
        )
        .expect("[arch][heap_vm] debug transaction test unmap failed");
        assert!(kernel_virt_to_phys(test_vaddr).is_err());
        // 正式 free 保留空下级页表；自检在专用窗口显式触发冷路径回收，继续验证
        // 摘除父 PTE、全局 fence 与物理页释放的事务顺序。
        assert!(with_kernel_heap_page_table_lock(|| {
            let root = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
            reclaim_empty_heap_page_tables(phys_to_virt(root), test_vaddr, PAGE_SIZE)
        }));
    }
    assert!(
        PAGE_TABLE_PAGES_RECLAIMED.load(Ordering::Relaxed) >= reclaimed_before + RECLAIM_CYCLES,
        "[arch][heap_vm] empty PT pages were not reclaimed"
    );
    assert!(
        TLB_GLOBAL_FLUSHES.load(Ordering::Relaxed) >= global_flushes_before + RECLAIM_CYCLES,
        "[arch][heap_vm] page-table reclaim skipped the global TLB flush"
    );
    allocator::KERNEL_ALLOCATOR
        .try_free_physical(allocation)
        .expect("[arch][heap_vm] debug transaction test page free failed");

    // 让连续两页跨过 2 MiB 边界：第一页发布完整 PMD+PT，第二页需要再分配一张
    // PT。第二次页表分配注入失败后，整段映射必须回滚，不能留下第一片 leaf。
    let rollback_vaddr = test_vaddr - PAGE_SIZE;
    let rollback_allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(2 * PAGE_SIZE, PAGE_SIZE))
        .expect("[arch][heap_vm] debug partial-rollback allocation failed");
    let rollbacks_before = MAP_ROLLBACKS.load(Ordering::Relaxed);
    FAIL_PAGE_TABLE_ALLOCATION_AFTER.store(1, Ordering::Relaxed);
    let result = map_kernel_range_in_window(
        rollback_vaddr,
        rollback_allocation.paddr,
        2 * PAGE_SIZE,
        PagePolicy::BaseOnly,
        HEAP_VM_SELFTEST_BASE,
        HEAP_VM_SELFTEST_END,
    );
    FAIL_PAGE_TABLE_ALLOCATION_AFTER.store(NO_PAGE_TABLE_ALLOCATION_FAILURE, Ordering::Relaxed);
    assert!(
        matches!(result, Err(MapError::OutOfMemory)),
        "[arch][heap_vm] partial-map failure injection returned {result:?}"
    );
    assert!(kernel_virt_to_phys(rollback_vaddr).is_err());
    assert!(kernel_virt_to_phys(rollback_vaddr + PAGE_SIZE).is_err());
    assert!(
        MAP_ROLLBACKS.load(Ordering::Relaxed) > rollbacks_before,
        "[arch][heap_vm] partial mapping did not record a rollback"
    );
    allocator::KERNEL_ALLOCATOR
        .try_free_physical(rollback_allocation)
        .expect("[arch][heap_vm] debug partial-rollback page free failed");
    log::info!("[arch][heap_vm] published map rollback/reclaim self-test passed");
}

pub fn kernel_virt_to_phys(vaddr: usize) -> Result<usize, MapError> {
    if !Riscv64Paging::is_canonical_vaddr(vaddr) {
        return Err(MapError::NotMapped);
    }

    // 查询与 map/unmap 使用同一把结构锁，避免在叶 PTE 更新中观察到部分状态；
    // 映射失败回滚和显式调试回收仍可能摘除下级页表。
    with_kernel_heap_page_table_lock(|| {
        let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
        if root_paddr == 0 {
            return Err(MapError::NotMapped);
        }
        let root_vaddr = phys_to_virt(root_paddr);
        let (level, _, pte) = find_leaf::<Riscv64Paging>(root_vaddr, vaddr, phys_to_virt)?;
        let page_size = Riscv64Paging::leaf_page_size(level).ok_or(MapError::UnsupportedLevel)?;
        Riscv64Paging::pte_addr(pte)
            .checked_add(vaddr & (page_size - 1))
            .ok_or(MapError::NotMapped)
    })
}

fn with_kernel_heap_page_table_lock<T>(f: impl FnOnce() -> T) -> T {
    let _interrupt_guard = LocalIrqGuard::acquire();
    let _page_table_guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    f()
}

fn alloc_page_table_allocation() -> Result<PhysicalAllocation, MapError> {
    #[cfg(debug_assertions)]
    {
        let remaining = FAIL_PAGE_TABLE_ALLOCATION_AFTER.load(Ordering::Relaxed);
        if remaining != NO_PAGE_TABLE_ALLOCATION_FAILURE {
            if remaining == 0 {
                PAGE_TABLE_ALLOCATION_FAILURES.fetch_add(1, Ordering::Relaxed);
                return Err(MapError::OutOfMemory);
            }
            FAIL_PAGE_TABLE_ALLOCATION_AFTER.store(remaining - 1, Ordering::Relaxed);
        }
    }

    let request = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map_err(|_| {
            PAGE_TABLE_ALLOCATION_FAILURES.fetch_add(1, Ordering::Relaxed);
            MapError::OutOfMemory
        })
}

fn checked_page_table_virt(paddr: usize) -> Result<usize, MapError> {
    let end = paddr.checked_add(PAGE_SIZE).ok_or(MapError::NotMapped)?;
    if paddr < KERNEL_DIRECT_MAP_PHYS_START
        || end > KERNEL_DIRECT_MAP_PHYS_END
        || paddr % PAGE_SIZE != 0
        || no_map_intersects(paddr, end)
    {
        PAGE_TABLE_CORRUPTIONS.fetch_add(1, Ordering::Relaxed);
        log::error!(
            "[arch][heap_vm] corrupt non-leaf PTE points outside usable direct map: {paddr:#x}"
        );
        return Err(MapError::NotMapped);
    }
    Ok(phys_to_virt(paddr))
}

fn free_page_table_allocation(allocation: PhysicalAllocation) {
    if let Err(err) = allocator::KERNEL_ALLOCATOR.try_free_physical(allocation) {
        log::error!(
            "[arch][heap_vm] failed to rollback page-table allocation paddr={:#x}: {:?}",
            allocation.paddr,
            err
        );
    } else {
        PAGE_TABLE_PAGES_RECLAIMED.fetch_add(1, Ordering::Relaxed);
    }
}

fn free_unpublished_page_tables(
    allocations: &mut [Option<PhysicalAllocation>; Riscv64Paging::LEVELS - 1],
    count: usize,
) {
    for slot in allocations[..count].iter_mut().rev() {
        if let Some(allocation) = slot.take() {
            free_page_table_allocation(allocation);
        }
    }
}

/// 为 kernel heap 建立一个叶映射。
///
/// 与通用 `walk_and_map` 不同，本实现直到整条缺失的页表链和叶 PTE 都准备完成后，
/// 才发布第一个可达的父 PTE。任何中间分配失败都只涉及尚未发布的页表页，可以
/// 完整释放，不会在正式页表中留下空分支。
fn walk_and_map_heap(
    root_vaddr: usize,
    vaddr: usize,
    paddr: usize,
    target_level: usize,
) -> Result<(), MapError> {
    if target_level >= Riscv64Paging::LEVELS
        || !Riscv64Paging::supported_leaf_levels().contains(&target_level)
    {
        return Err(MapError::UnsupportedLevel);
    }

    let mut table_vaddr = root_vaddr;

    for level in 0..target_level {
        let index = Riscv64Paging::level_index(vaddr, level);
        let pte_ptr = (table_vaddr + index * core::mem::size_of::<usize>()) as *mut usize;
        let pte = Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pte_ptr) });

        if Riscv64Paging::pte_is_valid(pte) {
            if Riscv64Paging::pte_is_leaf(pte) {
                return Err(MapError::AlreadyMapped);
            }
            table_vaddr = checked_page_table_virt(Riscv64Paging::pte_addr(pte))?;
            continue;
        }

        // 从第一个缺失层级开始，整条子树先在不可达状态下构造。
        let mut allocations: [Option<PhysicalAllocation>; Riscv64Paging::LEVELS - 1] =
            [None; Riscv64Paging::LEVELS - 1];
        let mut allocation_count = 0usize;

        let first = alloc_page_table_allocation()?;
        allocations[allocation_count] = Some(first);
        allocation_count += 1;
        let mut private_table_vaddr = phys_to_virt(first.paddr);
        unsafe { core::ptr::write_bytes(private_table_vaddr as *mut u8, 0, PAGE_SIZE) };

        for child_level in (level + 1)..target_level {
            let child = match alloc_page_table_allocation() {
                Ok(allocation) => allocation,
                Err(err) => {
                    free_unpublished_page_tables(&mut allocations, allocation_count);
                    return Err(err);
                }
            };
            allocations[allocation_count] = Some(child);
            allocation_count += 1;

            let child_vaddr = phys_to_virt(child.paddr);
            unsafe { core::ptr::write_bytes(child_vaddr as *mut u8, 0, PAGE_SIZE) };

            let child_index = Riscv64Paging::level_index(vaddr, child_level);
            let child_pte_ptr =
                (private_table_vaddr + child_index * core::mem::size_of::<usize>()) as *mut usize;
            let child_pte = Riscv64Paging::make_table_pte(child.paddr);
            unsafe {
                core::ptr::write_volatile(child_pte_ptr, Riscv64Paging::pte_to_usize(child_pte))
            };
            private_table_vaddr = child_vaddr;
        }

        let leaf = match Riscv64Paging::make_leaf_pte_for_level(
            target_level,
            paddr,
            true,
            true,
            false,
            false,
            true,
        ) {
            Some(leaf) => leaf,
            None => {
                free_unpublished_page_tables(&mut allocations, allocation_count);
                return Err(MapError::InvalidPermission);
            }
        };
        let leaf_index = Riscv64Paging::level_index(vaddr, target_level);
        let leaf_pte_ptr =
            (private_table_vaddr + leaf_index * core::mem::size_of::<usize>()) as *mut usize;
        unsafe {
            core::ptr::write_volatile(leaf_pte_ptr, Riscv64Paging::pte_to_usize(leaf));
            // 私有子树中的所有写入必须先于首个父 PTE 的发布对 page walker 可见。
            core::arch::asm!("fence w, w");
            let first_pte = Riscv64Paging::make_table_pte(first.paddr);
            core::ptr::write_volatile(pte_ptr, Riscv64Paging::pte_to_usize(first_pte));
        }
        return Ok(());
    }

    let index = Riscv64Paging::level_index(vaddr, target_level);
    let pte_ptr = (table_vaddr + index * core::mem::size_of::<usize>()) as *mut usize;
    let old_pte = Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pte_ptr) });
    if Riscv64Paging::pte_is_valid(old_pte) {
        return Err(MapError::AlreadyMapped);
    }

    let leaf =
        Riscv64Paging::make_leaf_pte_for_level(target_level, paddr, true, true, false, false, true)
            .ok_or(MapError::InvalidPermission)?;
    unsafe { core::ptr::write_volatile(pte_ptr, Riscv64Paging::pte_to_usize(leaf)) };
    Ok(())
}

struct EarlyPageTableLayout {
    root_paddr: usize,
    pgd: *mut usize,
    pud_identity: *mut usize,
    pud_kernel: *mut usize,
}

/// 定位并验证 boot 汇编建立的 Sv48 根页表与两张 PUD。
fn locate_early_page_tables() -> EarlyPageTableLayout {
    let satp: usize = read_csr!(satp);
    assert_eq!(
        satp & (0xFusize << 60),
        SATP_MODE_SV48,
        "[arch][heap_vm] expected Sv48 during page-table initialization"
    );
    let root_ppn = satp & 0xFFF_FFFF_FFFF;
    let root_paddr = root_ppn << 12;
    assert_ne!(
        root_paddr, 0,
        "[arch][heap_vm] missing early page-table root"
    );

    // 统一通过高半区 direct map 访问页表页，不依赖 identity mapping 的生命周期。
    let pgd = phys_to_virt(root_paddr) as *mut usize;
    let pud_identity_pte = Riscv64Pte(unsafe { core::ptr::read_volatile(pgd) });
    assert!(
        Riscv64Paging::pte_is_valid(pud_identity_pte)
            && !Riscv64Paging::pte_is_leaf(pud_identity_pte),
        "[arch][heap_vm] invalid early PGD[0]"
    );
    let pud_identity = phys_to_virt(Riscv64Paging::pte_addr(pud_identity_pte)) as *mut usize;
    let pud_kernel_pte = Riscv64Pte(unsafe { core::ptr::read_volatile(pgd.add(511)) });
    assert!(
        Riscv64Paging::pte_is_valid(pud_kernel_pte) && !Riscv64Paging::pte_is_leaf(pud_kernel_pte),
        "[arch][heap_vm] invalid early PGD[511]"
    );
    let pud_kernel = phys_to_virt(Riscv64Paging::pte_addr(pud_kernel_pte)) as *mut usize;

    for index in 0..2usize {
        let heap_pte = Riscv64Pte(unsafe { core::ptr::read_volatile(pud_kernel.add(index)) });
        assert!(
            !Riscv64Paging::pte_is_valid(heap_pte),
            "[arch][heap_vm] early heap PUD[{index}] unexpectedly occupied"
        );
    }

    let mmio_pgd = Riscv64Pte(unsafe { core::ptr::read_volatile(pgd.add(510)) });
    assert!(
        !Riscv64Paging::pte_is_valid(mmio_pgd),
        "[arch][heap_vm] early PGD[510] unexpectedly occupied"
    );

    EarlyPageTableLayout {
        root_paddr,
        pgd,
        pud_identity,
        pud_kernel,
    }
}

/// 确认尚未解析 DTB 时的页表只映射了内核镜像所在的首窗口。
fn validate_minimal_boot_direct_map(layout: &EarlyPageTableLayout) {
    let first = Riscv64Pte(unsafe {
        core::ptr::read_volatile(layout.pud_kernel.add(KERNEL_DIRECT_MAP_PUD_START))
    });
    assert!(
        Riscv64Paging::pte_is_valid(first) && !Riscv64Paging::pte_is_leaf(first),
        "[arch][heap_vm] minimal boot PUD[2] is not a PMD table"
    );
    let identity_first = Riscv64Pte(unsafe {
        core::ptr::read_volatile(layout.pud_identity.add(KERNEL_DIRECT_MAP_PUD_START))
    });
    assert_eq!(
        Riscv64Paging::pte_to_usize(identity_first),
        Riscv64Paging::pte_to_usize(first),
        "[arch][heap_vm] minimal identity and high-half mappings do not share the kernel PMD"
    );
    for index in (KERNEL_DIRECT_MAP_PUD_START + 1)
        ..(KERNEL_DIRECT_MAP_PUD_START + KERNEL_DIRECT_MAP_PUD_COUNT)
    {
        let pte = Riscv64Pte(unsafe { core::ptr::read_volatile(layout.pud_kernel.add(index)) });
        assert!(
            !Riscv64Paging::pte_is_valid(pte),
            "[arch][heap_vm] minimal boot PUD[{index}] maps RAM before DT no-map parsing"
        );
    }
}

struct DirectMapPageTables {
    pud_entries: [usize; KERNEL_DIRECT_MAP_PUD_COUNT],
    split_pud_windows: usize,
    split_pmd_chunks: usize,
    unmapped_pages: usize,
}

#[derive(Default)]
struct DirectMapBuildStats {
    split_pud_windows: usize,
    split_pmd_chunks: usize,
    unmapped_pages: usize,
}

#[inline]
fn direct_map_permissions(
    vaddr: usize,
    text_leaf_start: usize,
    text_end: usize,
    rodata_end: usize,
) -> (bool, bool, bool) {
    if vaddr < text_leaf_start {
        (true, true, false)
    } else if vaddr < text_end {
        (true, false, true)
    } else if vaddr < rodata_end {
        (true, false, false)
    } else {
        (true, true, false)
    }
}

fn write_fresh_page_table_entry(table: *mut usize, index: usize, bits: usize) {
    assert!(index < Riscv64Paging::ENTRIES_PER_TABLE);
    // Safety: 本函数只用于尚未发布且完整占有的单页页表分配；index 已限制在
    // 512 个 usize 表项内，父 PTE 直到全部子项写完后才会发布。
    unsafe { core::ptr::write_volatile(table.add(index), bits) };
}

/// 从尚未封存的 boot heap 分配一张永久页表页。
///
/// 这些页必须在 buddy 扫描高端 RAM 之前存在；boot allocator 的 used 前缀在
/// 正式 allocator 激活时不会回灌，所以页表生命期与内核一致。
fn alloc_boot_page_table() -> Result<usize, StartNoMapError> {
    let request = MemoryRequest::new(MemoryDomain::Kernel, PAGE_SIZE, PAGE_SIZE)
        .with_zeroing(Zeroing::Zeroed);
    let allocation = allocator::KERNEL_ALLOCATOR.allocate(request).map_err(|_| {
        PAGE_TABLE_ALLOCATION_FAILURES.fetch_add(1, Ordering::Relaxed);
        StartNoMapError::OutOfMemory
    })?;
    let paddr = virt_to_phys(allocation.ptr);
    let end = paddr
        .checked_add(PAGE_SIZE)
        .ok_or(StartNoMapError::InvalidRange)?;
    if paddr % PAGE_SIZE != 0
        || paddr < KERNEL_DIRECT_MAP_PHYS_START
        || end > KERNEL_DIRECT_MAP_PHYS_END
        || no_map_intersects(paddr, end)
    {
        return Err(StartNoMapError::InvalidRange);
    }
    Ok(paddr)
}

/// 为一个部分命中 `no-map` 的 2MiB 区块构造 4KiB 末级页表。
fn build_direct_map_pte(
    chunk_paddr: usize,
    permissions: (bool, bool, bool),
    stats: &mut DirectMapBuildStats,
) -> Result<usize, StartNoMapError> {
    let paddr = alloc_boot_page_table()?;
    let pte = phys_to_virt(paddr) as *mut usize;
    let (read, write, execute) = permissions;

    for index in 0..Riscv64Paging::ENTRIES_PER_TABLE {
        let paddr = chunk_paddr + index * PAGE_SIZE;
        let bits = if no_map_intersects(paddr, paddr + PAGE_SIZE) {
            stats.unmapped_pages += 1;
            0
        } else {
            Riscv64Paging::make_leaf_pte(paddr, read, write, execute, false, true).bits()
        };
        write_fresh_page_table_entry(pte, index, bits);
    }
    Ok(paddr)
}

/// 构造一个 1GiB direct-map 窗口的 PMD；仅边界 2MiB 区块需要继续拆成 4KiB。
fn build_direct_map_pmd(
    window_paddr: usize,
    text_leaf_start: usize,
    text_end: usize,
    rodata_end: usize,
    stats: &mut DirectMapBuildStats,
) -> Result<usize, StartNoMapError> {
    let paddr = alloc_boot_page_table()?;
    let pmd = phys_to_virt(paddr) as *mut usize;

    for index in 0..Riscv64Paging::ENTRIES_PER_TABLE {
        let paddr = window_paddr + index * HEAP_PMD_SIZE;
        let end = paddr + HEAP_PMD_SIZE;
        let vaddr = phys_to_virt(paddr);
        let permissions = direct_map_permissions(vaddr, text_leaf_start, text_end, rodata_end);
        let bits = if no_map_covers(paddr, end) {
            stats.unmapped_pages += HEAP_PMD_SIZE / PAGE_SIZE;
            0
        } else if no_map_intersects(paddr, end) {
            stats.split_pmd_chunks += 1;
            let pte_paddr = build_direct_map_pte(paddr, permissions, stats)?;
            Riscv64Paging::make_table_pte(pte_paddr).bits()
        } else {
            let (read, write, execute) = permissions;
            Riscv64Paging::make_leaf_pte(paddr, read, write, execute, false, true).bits()
        };
        write_fresh_page_table_entry(pmd, index, bits);
    }
    Ok(paddr)
}

/// 构造正式 RAM 线性映射。无 `no-map` 的高端窗口保留 1GiB 大页；命中范围的窗口
/// 先拆为 2MiB，只有范围边界所在区块继续拆为 4KiB，避免扩大不可访问区域。
fn build_direct_map_page_tables() -> Result<DirectMapPageTables, StartNoMapError> {
    unsafe extern "C" {
        fn stext();
        fn etext();
        fn erodata();
    }

    let direct_map_end = KERNEL_VIRT_BASE + KERNEL_DIRECT_MAP_WINDOW_SIZE;
    let text_start = stext as usize;
    let text_end = etext as usize;
    let rodata_end = erodata as usize;
    assert!(
        KERNEL_VIRT_BASE <= text_start && text_start < text_end && text_end <= rodata_end,
        "[arch][heap_vm] invalid kernel section ordering"
    );
    assert!(
        rodata_end <= direct_map_end,
        "[arch][heap_vm] kernel image exceeds the first 1 GiB direct-map window"
    );
    assert_eq!(
        text_end % HEAP_PMD_SIZE,
        0,
        "[arch][heap_vm] etext must be 2 MiB aligned"
    );
    assert_eq!(
        rodata_end % HEAP_PMD_SIZE,
        0,
        "[arch][heap_vm] erodata must be 2 MiB aligned"
    );

    // `stext` 所在的整个 2MiB leaf 必须可执行；debug 链接脚本的前置 padding
    // 因而与正文使用相同的只读可执行权限。
    let text_leaf_start = text_start & !(HEAP_PMD_SIZE - 1);
    let mut stats = DirectMapBuildStats::default();
    let mut pud_entries = [0usize; KERNEL_DIRECT_MAP_PUD_COUNT];

    let first_pmd_paddr = build_direct_map_pmd(
        KERNEL_PHYS_BASE,
        text_leaf_start,
        text_end,
        rodata_end,
        &mut stats,
    )?;
    pud_entries[0] = Riscv64Paging::make_table_pte(first_pmd_paddr).bits();
    stats.split_pud_windows += 1;

    for (window, entry) in pud_entries.iter_mut().enumerate().skip(1) {
        let paddr = KERNEL_PHYS_BASE + window * KERNEL_DIRECT_MAP_WINDOW_SIZE;
        let end = paddr + KERNEL_DIRECT_MAP_WINDOW_SIZE;
        *entry = if no_map_covers(paddr, end) {
            stats.unmapped_pages += KERNEL_DIRECT_MAP_WINDOW_SIZE / PAGE_SIZE;
            0
        } else if no_map_intersects(paddr, end) {
            stats.split_pud_windows += 1;
            let pmd_paddr =
                build_direct_map_pmd(paddr, text_leaf_start, text_end, rodata_end, &mut stats)?;
            Riscv64Paging::make_table_pte(pmd_paddr).bits()
        } else {
            Riscv64Paging::make_leaf_pte(paddr, true, true, false, false, true).bits()
        };
    }

    let expected_unmapped_pages: usize = no_map_ranges()
        .iter()
        .map(|range| (range.end - range.start) / PAGE_SIZE)
        .sum();
    assert_eq!(
        stats.unmapped_pages, expected_unmapped_pages,
        "[arch][heap_vm] no-map page-table coverage mismatch"
    );

    Ok(DirectMapPageTables {
        pud_entries,
        split_pud_windows: stats.split_pud_windows,
        split_pmd_chunks: stats.split_pmd_chunks,
        unmapped_pages: stats.unmapped_pages,
    })
}

/// 构造 PGD[510] 下的 MMIO 与 PCI 32-bit BAR 直接映射窗口。
fn build_mmio_pud() -> Result<PhysicalAllocation, MapError> {
    let allocation = alloc_page_table_allocation()?;
    let pud = phys_to_virt(allocation.paddr) as *mut usize;
    // Safety: pud 指向刚分配且尚未发布的完整页表页；清零覆盖整页，随后写入的两个
    // usize 表项都位于 512 项容量内，父 PGD 项会在函数返回后才发布。
    unsafe {
        core::ptr::write_bytes(pud, 0, PAGE_SIZE / core::mem::size_of::<usize>());
        let mmio_leaf = Riscv64Paging::make_leaf_pte(0, true, true, false, false, true);
        core::ptr::write_volatile(pud, mmio_leaf.bits());
        let pci32_leaf = Riscv64Paging::make_leaf_pte(0x4000_0000, true, true, false, false, true);
        core::ptr::write_volatile(pud.add(1), pci32_leaf.bits());
    }
    Ok(allocation)
}

/// 在 buddy 初始化之前发布完整 RAM 直映。
fn publish_boot_direct_map(layout: &EarlyPageTableLayout, direct_map: &DirectMapPageTables) {
    // Safety: layout 指向当前激活且由 boot hart 独占更新的早期页表；
    // 全部 boot-heap 子页表已初始化。当前指令流在高半区，因此可先替换
    // identity 项和高端窗口，最后原子地替换承载正文的首窗口。
    unsafe {
        core::arch::asm!("fence w, w");
        for index in (KERNEL_DIRECT_MAP_PUD_START + KERNEL_DIRECT_MAP_PUD_COUNT)
            ..Riscv64Paging::ENTRIES_PER_TABLE
        {
            core::ptr::write_volatile(layout.pud_kernel.add(index), 0);
        }
        for window in 1..KERNEL_DIRECT_MAP_PUD_COUNT {
            core::ptr::write_volatile(
                layout.pud_kernel.add(KERNEL_DIRECT_MAP_PUD_START + window),
                direct_map.pud_entries[window],
            );
        }
        // identity 只保留低端 UART/MMIO 与继承相同空洞的内核首窗口。
        for index in 1..Riscv64Paging::ENTRIES_PER_TABLE {
            core::ptr::write_volatile(layout.pud_identity.add(index), 0);
        }
        core::ptr::write_volatile(
            layout.pud_identity.add(KERNEL_DIRECT_MAP_PUD_START),
            direct_map.pud_entries[0],
        );
        core::ptr::write_volatile(
            layout.pud_kernel.add(KERNEL_DIRECT_MAP_PUD_START),
            direct_map.pud_entries[0],
        );
        core::arch::asm!("fence rw, rw");
        Riscv64Paging::flush_tlb_global(None);
        core::arch::asm!("fence.i");
    }
}

/// 发布独立的高半区 MMIO PUD；RAM 直映已在物理 allocator 之前完成。
fn publish_mmio_page_table(layout: &EarlyPageTableLayout, mmio_pud_paddr: usize) {
    let mmio_pgd_pte = Riscv64Paging::make_table_pte(mmio_pud_paddr);
    // Safety: 子 PUD 已完整初始化，PGD[510] 在启动 hart 发布前为空。
    unsafe {
        core::arch::asm!("fence w, w");
        core::ptr::write_volatile(layout.pgd.add(510), mmio_pgd_pte.bits());
        core::arch::asm!("fence rw, rw");
        Riscv64Paging::flush_tlb_global(None);
    }
}

/// UART 切到高半区后移除 boot 阶段使用的 PGD[0] identity mapping。
fn remove_identity_mapping(layout: &EarlyPageTableLayout) {
    unsafe {
        core::ptr::write_volatile(layout.pgd, 0);
        Riscv64Paging::flush_tlb_global(None);
    }
}

/// 在已提前发布的 RAM 直映上安装 MMIO 窗口与内核堆页表运行时。
pub fn init_kernel_page_table() {
    match PAGE_TABLE_INIT_STATE.compare_exchange(
        PAGE_TABLE_UNINITIALIZED,
        PAGE_TABLE_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(PAGE_TABLE_INITIALIZED) => return,
        Err(_) => panic!("[arch][heap_vm] recursive kernel page-table initialization"),
    }

    assert_eq!(
        NO_MAP_PREPARE_STATE.load(Ordering::Acquire),
        NO_MAP_PREPARED,
        "[arch][heap_vm] direct map was not published before physical allocator initialization"
    );
    let layout = locate_early_page_tables();
    let mmio_pud = match build_mmio_pud() {
        Ok(allocation) => allocation,
        Err(err) => panic!("[arch][heap_vm] failed to allocate MMIO PUD page: {err:?}"),
    };
    publish_mmio_page_table(&layout, mmio_pud.paddr);
    log::info!(
        "[arch][heap_vm] direct map ready: split-pud={} split-pmd={} no-map-pages={}; PUD[0..1] reserved for heap",
        DIRECT_MAP_SPLIT_PUD_WINDOWS.load(Ordering::Acquire),
        DIRECT_MAP_SPLIT_PMD_CHUNKS.load(Ordering::Acquire),
        DIRECT_MAP_UNMAPPED_PAGES.load(Ordering::Acquire)
    );

    KERNEL_PAGE_TABLE_ROOT.store(layout.root_paddr, Ordering::Release);
    #[cfg(debug_assertions)]
    debug_verify_unpublished_page_table_transactions();
    install_boot_emergency_stack()
        .unwrap_or_else(|err| panic!("[arch][heap_vm] emergency stack setup failed: {err:?}"));
    verify_kernel_segments(layout.root_paddr);

    // MMIO 映射就绪后 UART 不再依赖低地址，identity mapping 才可安全拆除。
    crate::riscv64::early_console::switch_to_virtual();
    remove_identity_mapping(&layout);

    PAGE_TABLE_INIT_STATE.store(PAGE_TABLE_INITIALIZED, Ordering::Release);
    log::info!("[arch][heap_vm] identity mapping (PGD[0]) removed");
}

/// 返回 boot hart 已发布的正式 Sv48 根页表物理地址。
pub(crate) fn kernel_page_table_root() -> usize {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    assert_ne!(root_paddr, 0, "[smp] kernel page table is not ready");
    root_paddr
}

/// 将当前 hart 切回内核地址空间。
///
/// 用户任务离开 CPU 后，调度器不能继续保留它的用户根页表；否则该任务的
/// `VmSpace` 释放后，下一次内核访问可能落到已经复用的物理页表页。内核根使用
/// ASID 0，`activate_with_asid` 会在根或 ASID 变化时执行本地失效。
pub fn activate_kernel_page_table() {
    let root_paddr = kernel_page_table_root();
    unsafe {
        Riscv64Paging::activate_with_asid(PhysPageTableRoot::new(root_paddr), 0, false);
    }
}

/// 为 SBI HSM 的物理入口临时恢复内核镜像所在 1 GiB 的 identity mapping。
pub(crate) fn install_secondary_identity_mapping() {
    let _guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    if SECONDARY_IDENTITY_PUD.load(Ordering::Acquire) != 0 {
        return;
    }
    let root_paddr = kernel_page_table_root();
    let pgd = phys_to_virt(root_paddr) as *mut usize;
    let kernel_pud_pte = Riscv64Pte(unsafe { core::ptr::read_volatile(pgd.add(511)) });
    assert!(
        Riscv64Paging::pte_is_valid(kernel_pud_pte) && !Riscv64Paging::pte_is_leaf(kernel_pud_pte)
    );
    let kernel_pud = phys_to_virt(Riscv64Paging::pte_addr(kernel_pud_pte)) as *mut usize;
    let identity_pud_paddr = alloc_page_table_allocation()
        .expect("[smp] identity PUD allocation failed")
        .paddr;
    let identity_pud = phys_to_virt(identity_pud_paddr) as *mut usize;
    unsafe {
        core::ptr::write_bytes(identity_pud, 0, PAGE_SIZE / core::mem::size_of::<usize>());
        let kernel_image_pte = core::ptr::read_volatile(kernel_pud.add(2));
        core::ptr::write_volatile(identity_pud.add(2), kernel_image_pte);
        core::arch::asm!("fence w, w", options(nostack));
        let identity_pgd_pte = Riscv64Paging::make_table_pte(identity_pud_paddr);
        core::ptr::write_volatile(pgd, identity_pgd_pte.bits());
        core::arch::asm!("fence rw, rw", options(nostack));
        Riscv64Paging::flush_tlb_global(None);
    }
    SECONDARY_IDENTITY_PUD.store(identity_pud_paddr, Ordering::Release);
}

/// 所有 AP 已跳入高半区后撤销临时 identity mapping。
pub(crate) fn remove_secondary_identity_mapping() {
    let _guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    let identity_pud_paddr = SECONDARY_IDENTITY_PUD.swap(0, Ordering::AcqRel);
    if identity_pud_paddr == 0 {
        return;
    }
    let root_paddr = kernel_page_table_root();
    let pgd = phys_to_virt(root_paddr) as *mut usize;
    unsafe {
        core::ptr::write_volatile(pgd, 0);
        core::arch::asm!("fence rw, rw", options(nostack));
        Riscv64Paging::flush_tlb_global(None);
    }
    free_page_table_page(identity_pud_paddr);
}

/// 验证内核关键段的页表权限。仅在 debug 构建中生效。
///
/// 确保 W^X（写或执行二选一）策略正确实施：
/// - .text: R+X（只读可执行）
/// - .rodata: R（只读不可执行）
/// - .data: R+W（可读可写不可执行）
fn verify_kernel_segments(root_paddr: usize) {
    if !cfg!(debug_assertions) {
        return;
    }

    unsafe extern "C" {
        fn stext();
        fn etext();
        fn srodata();
        fn erodata();
        fn sdata();
        fn ekernel();
    }

    let root = phys_to_virt(root_paddr);

    let check_range = |name: &str,
                       start: usize,
                       end: usize,
                       expect_r: bool,
                       expect_w: bool,
                       expect_x: bool,
                       virt_offset: usize| {
        if start == end {
            return;
        }
        assert!(start < end, "[heap_vm] invalid verification range '{name}'");
        let mut va = start;
        while va < end {
            let expected_paddr = va.wrapping_sub(virt_offset);
            let leaf = find_leaf::<Riscv64Paging>(root, va, phys_to_virt);
            if let Some(no_map) = no_map_range_containing(expected_paddr) {
                assert!(
                    leaf.is_err(),
                    "[heap_vm] no-map page remains mapped in '{}' at va={:#x} pa={:#x}",
                    name,
                    va,
                    expected_paddr
                );
                va = va
                    .checked_add(no_map.end - expected_paddr)
                    .expect("[heap_vm] no-map verification address overflow")
                    .min(end);
                continue;
            }

            let (level, _ptr, pte) = leaf.unwrap_or_else(|err| {
                panic!(
                    "[heap_vm] range '{}' is not mapped at {:#x}: {:?}",
                    name, va, err
                )
            });
            let flags = <Riscv64Paging as PagingArch>::pte_flags(pte);
            let r = <Riscv64Paging as PagingArch>::flags_readable(flags);
            let w = <Riscv64Paging as PagingArch>::flags_writable(flags);
            let x = <Riscv64Paging as PagingArch>::flags_executable(flags);
            let user = <Riscv64Paging as PagingArch>::flags_user_accessible(flags);
            let global = <Riscv64Paging as PagingArch>::flags_global(flags);
            assert!(
                r == expect_r && w == expect_w && x == expect_x,
                "[heap_vm] segment '{}' at {:#x}: perm R={} W={} X={}, expected R={} W={} X={}",
                name,
                va,
                r,
                w,
                x,
                expect_r,
                expect_w,
                expect_x
            );
            assert!(
                !user && global,
                "[heap_vm] invalid U/G flags for '{name}' at {va:#x}"
            );
            assert!(!(w && x), "[heap_vm] W^X violation for '{name}' at {va:#x}");

            let page_size = Riscv64Paging::leaf_page_size(level)
                .unwrap_or_else(|| panic!("[heap_vm] unsupported leaf level {level}"));
            let leaf_paddr = Riscv64Paging::pte_addr(pte);
            let leaf_end = leaf_paddr
                .checked_add(page_size)
                .expect("[heap_vm] leaf physical range overflow");
            assert!(
                !no_map_intersects(leaf_paddr, leaf_end),
                "[heap_vm] mapped leaf in '{}' crosses a no-map range: pa={:#x} size={:#x}",
                name,
                leaf_paddr,
                page_size
            );
            let actual_paddr = leaf_paddr
                .checked_add(va & (page_size - 1))
                .expect("[heap_vm] mapped physical address overflow");
            assert_eq!(
                actual_paddr, expected_paddr,
                "[heap_vm] physical mapping mismatch for '{name}' at {va:#x}"
            );
            let leaf_base = va & !(page_size - 1);
            let next = leaf_base
                .checked_add(page_size)
                .expect("[heap_vm] verification address overflow");
            assert!(next > va, "[heap_vm] verification walker made no progress");
            va = next.min(end);
        }
    };

    let text_start = stext as usize;
    let text_leaf_start = text_start & !(HEAP_PMD_SIZE - 1);
    let first_direct_map_end = KERNEL_VIRT_BASE + KERNEL_DIRECT_MAP_WINDOW_SIZE;
    let full_direct_map_end =
        KERNEL_VIRT_BASE + KERNEL_DIRECT_MAP_PUD_COUNT * KERNEL_DIRECT_MAP_WINDOW_SIZE;
    assert!(
        KERNEL_VIRT_BASE <= text_leaf_start
            && text_start < etext as usize
            && etext as usize == srodata as usize
            && erodata as usize <= sdata as usize
            && ekernel as usize <= first_direct_map_end,
        "[heap_vm] invalid linker section layout"
    );

    // 验证整个 direct map，而不是只检查每个段的第一个地址。
    check_range(
        "firmware/reserved",
        KERNEL_VIRT_BASE,
        text_leaf_start,
        true,
        true,
        false,
        KERNEL_VA_OFFSET,
    );
    check_range(
        "text alignment padding",
        text_leaf_start,
        text_start,
        true,
        false,
        true,
        KERNEL_VA_OFFSET,
    );
    check_range(
        ".text",
        text_start,
        etext as usize,
        true,
        false,
        true,
        KERNEL_VA_OFFSET,
    );
    check_range(
        ".rodata",
        srodata as usize,
        erodata as usize,
        true,
        false,
        false,
        KERNEL_VA_OFFSET,
    );
    check_range(
        ".data/.bss",
        sdata as usize,
        ekernel as usize,
        true,
        true,
        false,
        KERNEL_VA_OFFSET,
    );
    check_range(
        "PUD[2] direct-map tail",
        ekernel as usize,
        first_direct_map_end,
        true,
        true,
        false,
        KERNEL_VA_OFFSET,
    );
    check_range(
        "高端 direct map",
        first_direct_map_end,
        full_direct_map_end,
        true,
        true,
        false,
        KERNEL_VA_OFFSET,
    );
    check_range(
        "MMIO low",
        MMIO_VIRT_BASE,
        MMIO_VIRT_BASE + 0x4000_0000,
        true,
        true,
        false,
        MMIO_VIRT_BASE,
    );
    check_range(
        "MMIO PCI32",
        MMIO_VIRT_BASE + 0x4000_0000,
        MMIO_VIRT_BASE + 0x8000_0000,
        true,
        true,
        false,
        MMIO_VIRT_BASE,
    );
}

fn validate_mapping_range(
    vaddr: usize,
    paddr: Option<usize>,
    size: usize,
    allowed_start: usize,
    allowed_end: usize,
) -> Result<(), MapError> {
    if size == 0 || vaddr % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 {
        return Err(MapError::Misaligned);
    }
    if paddr.is_some_and(|addr| addr % PAGE_SIZE != 0) {
        return Err(MapError::Misaligned);
    }

    let end_vaddr = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    if vaddr < allowed_start || end_vaddr > allowed_end {
        return Err(MapError::NotMapped);
    }
    if let Some(addr) = paddr {
        let end = addr.checked_add(size).ok_or(MapError::NotMapped)?;
        if addr < KERNEL_DIRECT_MAP_PHYS_START
            || end > KERNEL_DIRECT_MAP_PHYS_END
            || no_map_intersects(addr, end)
        {
            return Err(MapError::NotMapped);
        }
    }
    Ok(())
}

#[inline]
fn flush_kernel_tlb_all() {
    unsafe { Riscv64Paging::flush_tlb_global(None) };
    TLB_GLOBAL_FLUSHES.fetch_add(1, Ordering::Relaxed);
}

fn execute_kernel_tlb_flush_plan(plan: &KernelTlbFlushPlan, range_start: usize, range_size: usize) {
    if plan.global {
        unsafe { Riscv64Paging::flush_tlb_global_local(None) };
        TLB_GLOBAL_FLUSHES.fetch_add(1, Ordering::Relaxed);
    } else {
        for &vaddr in &plan.addresses[..plan.count] {
            unsafe { Riscv64Paging::flush_tlb_global_local(Some(general::VirtAddr::new(vaddr))) };
            TLB_ADDRESS_FLUSHES.fetch_add(1, Ordering::Relaxed);
        }
    }

    // 本地 sfence 需要逐 leaf 执行，但 SBI RFENCE 原生接受连续范围。把原先最多
    // 64 次 M-mode 往返合并为一次，同时仍在返回前等待所有远端 hart 完成失效。
    crate::riscv64::smp::remote_sfence_vma_range_on(usize::MAX, None, range_start, range_size);
}

fn uniform_kernel_tlb_flush_plan(
    vaddr: usize,
    size: usize,
    leaf_size: usize,
) -> KernelTlbFlushPlan {
    let mut plan = KernelTlbFlushPlan::new();
    let Some(end_vaddr) = vaddr.checked_add(size) else {
        plan.global = true;
        return plan;
    };
    let mut current_vaddr = vaddr;
    while current_vaddr < end_vaddr {
        plan.push(current_vaddr);
        if plan.global {
            break;
        }
        let Some(next_vaddr) = current_vaddr.checked_add(leaf_size) else {
            plan.global = true;
            break;
        };
        current_vaddr = next_vaddr;
    }
    plan
}

/// 验证待解除映射的范围，同时按实际叶 PTE 粒度生成 TLB 刷新计划。
/// 一个 2 MiB leaf 只需要一个地址 fence，不再被错误地按 512 个 4 KiB 页计算。
fn existing_kernel_tlb_flush_plan(
    root_vaddr: usize,
    vaddr: usize,
    size: usize,
) -> Result<KernelTlbFlushPlan, MapError> {
    if size == 0 || vaddr % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 {
        return Err(MapError::Misaligned);
    }
    let end_vaddr = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let mut current_vaddr = vaddr;
    let mut plan = KernelTlbFlushPlan::new();

    while current_vaddr < end_vaddr {
        let leaf = find_kernel_heap_leaf_or_guard(root_vaddr, current_vaddr)?;
        let leaf_size =
            Riscv64Paging::leaf_page_size(leaf.level).ok_or(MapError::UnsupportedLevel)?;
        let leaf_base = current_vaddr & !(leaf_size - 1);
        let next_vaddr = current_vaddr
            .checked_add(leaf_size)
            .ok_or(MapError::NotMapped)?;
        if leaf_base != current_vaddr || next_vaddr > end_vaddr {
            return Err(MapError::Misaligned);
        }
        plan.push(current_vaddr);
        current_vaddr = next_vaddr;
    }
    Ok(plan)
}

fn page_table_is_empty(table_vaddr: usize) -> bool {
    for index in 0..Riscv64Paging::ENTRIES_PER_TABLE {
        let pte_ptr = (table_vaddr + index * core::mem::size_of::<usize>()) as *const usize;
        let pte = Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pte_ptr) });
        if Riscv64Paging::pte_is_valid(pte) || software_guard_level(pte).is_some() {
            return false;
        }
    }
    true
}

fn free_page_table_page(paddr: usize) -> bool {
    if let Err(err) = allocator::KERNEL_ALLOCATOR.try_free_physical_addr(paddr) {
        log::error!(
            "[arch][heap_vm] failed to free page-table page paddr={:#x}: {:?}",
            paddr,
            err
        );
        false
    } else {
        PAGE_TABLE_PAGES_RECLAIMED.fetch_add(1, Ordering::Relaxed);
        true
    }
}

/// 回收解除映射后变空的 PT/PMD 页，只处理内核堆 PUD[0..1]。
///
/// 根页表和启动阶段创建的 PUD 页由整个内核地址空间共享，绝不在这里释放。
/// 返回 true 表示至少摘除了一个非叶页表，并已执行一次覆盖叶映射的全局 TLB flush。
fn reclaim_empty_heap_page_tables(root_vaddr: usize, vaddr: usize, size: usize) -> bool {
    let Some(end_vaddr) = vaddr.checked_add(size) else {
        return false;
    };
    let invalid = Riscv64Paging::pte_to_usize(Riscv64Paging::invalid_pte());
    let reclaimed = unsafe { &mut *RECLAIM_SCRATCH.0.get() };
    let mut reclaimed_count = 0usize;
    let mut current_pud_base = vaddr & !(HEAP_PUD_SIZE - 1);

    while current_pud_base < end_vaddr {
        let next_pud_base = current_pud_base
            .checked_add(HEAP_PUD_SIZE)
            .unwrap_or(end_vaddr);
        let chunk_start = vaddr.max(current_pud_base);
        let chunk_end = end_vaddr.min(next_pud_base);

        let pgd_index = Riscv64Paging::level_index(chunk_start, 0);
        let pgd_pte_ptr = (root_vaddr + pgd_index * core::mem::size_of::<usize>()) as *const usize;
        let pgd_pte =
            Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pgd_pte_ptr) });
        if !Riscv64Paging::pte_is_valid(pgd_pte) || Riscv64Paging::pte_is_leaf(pgd_pte) {
            break;
        }

        let pud_vaddr = phys_to_virt(Riscv64Paging::pte_addr(pgd_pte));
        let pud_index = Riscv64Paging::level_index(chunk_start, 1);
        if pud_index > 1 {
            break;
        }
        let pud_pte_ptr = (pud_vaddr + pud_index * core::mem::size_of::<usize>()) as *mut usize;
        let pud_pte =
            Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pud_pte_ptr) });
        if !Riscv64Paging::pte_is_valid(pud_pte) || Riscv64Paging::pte_is_leaf(pud_pte) {
            current_pud_base = next_pud_base;
            continue;
        }

        let pmd_paddr = Riscv64Paging::pte_addr(pud_pte);
        let pmd_vaddr = phys_to_virt(pmd_paddr);
        let mut current_pmd_base = chunk_start & !(HEAP_PMD_SIZE - 1);
        while current_pmd_base < chunk_end {
            let pmd_index = Riscv64Paging::level_index(current_pmd_base, 2);
            let pmd_pte_ptr = (pmd_vaddr + pmd_index * core::mem::size_of::<usize>()) as *mut usize;
            let pmd_pte =
                Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pmd_pte_ptr) });

            if Riscv64Paging::pte_is_valid(pmd_pte) && !Riscv64Paging::pte_is_leaf(pmd_pte) {
                let pt_paddr = Riscv64Paging::pte_addr(pmd_pte);
                if page_table_is_empty(phys_to_virt(pt_paddr)) {
                    unsafe { core::ptr::write_volatile(pmd_pte_ptr, invalid) };
                    debug_assert!(reclaimed_count < reclaimed.len());
                    reclaimed[reclaimed_count] = pt_paddr;
                    reclaimed_count += 1;
                }
            }

            current_pmd_base = current_pmd_base
                .checked_add(HEAP_PMD_SIZE)
                .unwrap_or(chunk_end);
        }

        // 每个 PUD chunk 只扫描一次 PMD，避免大范围 unmap 时重复 512 次全表检查。
        if page_table_is_empty(pmd_vaddr) {
            unsafe { core::ptr::write_volatile(pud_pte_ptr, invalid) };
            debug_assert!(reclaimed_count < reclaimed.len());
            reclaimed[reclaimed_count] = pmd_paddr;
            reclaimed_count += 1;
        }

        current_pud_base = next_pud_base;
    }

    if reclaimed_count == 0 {
        return false;
    }

    // 所有父 PTE 一次性摘除后，用一个全局 fence 同时失效叶 translation 和
    // page-walk cache；完成前绝不覆盖或释放被摘除的页表页。
    unsafe { core::arch::asm!("fence rw, rw") };
    flush_kernel_tlb_all();
    for &paddr in &reclaimed[..reclaimed_count] {
        free_page_table_page(paddr);
    }
    true
}

/// 对标 LoongArch：动态搜索 2 MiB 叶子层级，不硬编码具体层级。
fn map_range_with_policy(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
    allowed_start: usize,
    allowed_end: usize,
) -> Result<(), MapError> {
    validate_mapping_range(vaddr, Some(paddr), size, allowed_start, allowed_end)?;

    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        return Err(MapError::NotMapped);
    }
    let root_vaddr = phys_to_virt(root_paddr);

    let (target_level, page_size) = find_leaf_level(page_policy, vaddr, paddr, size)?;
    if vaddr % page_size != 0 || paddr % page_size != 0 || size % page_size != 0 {
        if page_policy == PagePolicy::RequireLarge {
            return Err(MapError::Misaligned);
        }
        if page_policy == PagePolicy::PreferLarge {
            LARGE_PAGE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            return map_range_with_policy(
                vaddr,
                paddr,
                size,
                PagePolicy::BaseOnly,
                allowed_start,
                allowed_end,
            );
        }
    }

    let end_vaddr = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let mut current_vaddr = vaddr;
    let mut current_paddr = paddr;

    while current_vaddr < end_vaddr {
        if let Err(mut err) =
            walk_and_map_heap(root_vaddr, current_vaddr, current_paddr, target_level)
        {
            // 基础页解除映射后会保留空的下级页表。大页映射遇到这种非叶项时，先验证
            // 整棵子树为空，再将它提升为大页叶；存在活跃映射时仍按 AlreadyMapped 失败。
            if matches!(err, MapError::AlreadyMapped)
                && !matches!(page_policy, PagePolicy::BaseOnly)
            {
                match replace_empty_table_with_leaf::<Riscv64Paging>(
                    root_vaddr,
                    current_vaddr,
                    current_paddr,
                    target_level,
                    true,
                    true,
                    false,
                    false,
                    true,
                    phys_to_virt,
                    free_page_table_page,
                ) {
                    Ok(reclaim_failures) => {
                        if reclaim_failures != 0 {
                            log::error!(
                                "[arch][heap_vm] promoted empty page-table subtree with {} unreclaimed page(s): vaddr={:#x}",
                                reclaim_failures,
                                current_vaddr
                            );
                        }
                        current_vaddr += page_size;
                        current_paddr += page_size;
                        continue;
                    }
                    Err(promote_err) => err = promote_err,
                }
            }
            let mapped_size = current_vaddr - vaddr;
            if mapped_size != 0 {
                MAP_ROLLBACKS.fetch_add(1, Ordering::Relaxed);
                if let Err(rollback_err) = unmap_range_entries::<Riscv64Paging>(
                    root_vaddr,
                    vaddr,
                    mapped_size,
                    true,
                    phys_to_virt,
                ) {
                    panic!(
                        "[arch][heap_vm] partial mapping rollback failed: vaddr={:#x} size={:#x} error={:?}",
                        vaddr, mapped_size, rollback_err
                    );
                }
                if !reclaim_empty_heap_page_tables(root_vaddr, vaddr, mapped_size) {
                    let plan = uniform_kernel_tlb_flush_plan(vaddr, mapped_size, page_size);
                    execute_kernel_tlb_flush_plan(&plan, vaddr, mapped_size);
                }
            }

            if page_policy == PagePolicy::PreferLarge && matches!(err, MapError::AlreadyMapped) {
                LARGE_PAGE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                return map_range_with_policy(
                    vaddr,
                    paddr,
                    size,
                    PagePolicy::BaseOnly,
                    allowed_start,
                    allowed_end,
                );
            }
            return Err(err);
        }

        current_vaddr += page_size;
        current_paddr += page_size;
    }

    if page_size == PAGE_SIZE {
        BASE_PAGE_MAPS.fetch_add(size / PAGE_SIZE, Ordering::Relaxed);
    } else {
        LARGE_PAGE_MAPS.fetch_add(size / page_size, Ordering::Relaxed);
    }
    let plan = uniform_kernel_tlb_flush_plan(vaddr, size, page_size);
    execute_kernel_tlb_flush_plan(&plan, vaddr, size);
    Ok(())
}

/// 搜索 2 MiB 叶子层级。
fn find_2_mib_leaf_level() -> Option<(usize, usize)> {
    for &level in Riscv64Paging::supported_leaf_levels() {
        if let Some(size) = Riscv64Paging::leaf_page_size(level) {
            if size == HEAP_PMD_SIZE {
                return Some((level, size));
            }
        }
    }
    None
}

/// 搜索最小叶子层级（4 KiB）。
fn find_smallest_leaf_level() -> usize {
    let mut smallest: Option<(usize, usize)> = None;
    for &level in Riscv64Paging::supported_leaf_levels() {
        if let Some(size) = Riscv64Paging::leaf_page_size(level) {
            if smallest.is_none() || size < smallest.unwrap().1 {
                smallest = Some((level, size));
            }
        }
    }
    smallest.map(|(l, _)| l).unwrap_or(3)
}

/// 根据 page_policy 确定目标映射层级。
fn find_leaf_level(
    page_policy: PagePolicy,
    _vaddr: usize,
    _paddr: usize,
    size: usize,
) -> Result<(usize, usize), MapError> {
    match page_policy {
        PagePolicy::BaseOnly => {
            let level = find_smallest_leaf_level();
            let page_size = Riscv64Paging::leaf_page_size(level).unwrap_or(PAGE_SIZE);
            Ok((level, page_size))
        }
        PagePolicy::PreferLarge | PagePolicy::RequireLarge => {
            // 只有 >= 2MiB 的分配才尝试大页
            if size >= HEAP_PMD_SIZE {
                if let Some((level, ps)) = find_2_mib_leaf_level() {
                    return Ok((level, ps));
                }
            }
            if page_policy == PagePolicy::RequireLarge {
                return Err(MapError::UnsupportedLevel);
            }
            // 降级到 BaseOnly
            let level = find_smallest_leaf_level();
            let page_size = Riscv64Paging::leaf_page_size(level).unwrap_or(PAGE_SIZE);
            Ok((level, page_size))
        }
    }
}

fn map_kernel_range_in_window(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
    allowed_start: usize,
    allowed_end: usize,
) -> Result<(), MapError> {
    validate_mapping_range(vaddr, Some(paddr), size, allowed_start, allowed_end)?;
    with_kernel_heap_page_table_lock(|| {
        map_range_with_policy(vaddr, paddr, size, page_policy, allowed_start, allowed_end)
    })
}

fn unmap_kernel_range_in_window(
    vaddr: usize,
    size: usize,
    allowed_start: usize,
    allowed_end: usize,
) -> Result<(), MapError> {
    validate_mapping_range(vaddr, None, size, allowed_start, allowed_end)?;
    with_kernel_heap_page_table_lock(|| unmap_kernel_heap_range_locked(vaddr, size))
}

pub fn map_kernel_heap_range(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
) -> Result<(), MapError> {
    map_kernel_range_in_window(
        vaddr,
        paddr,
        size,
        page_policy,
        KERNEL_HEAP_BASE,
        KERNEL_HEAP_USABLE_END,
    )
}

pub fn unmap_kernel_heap_range(vaddr: usize, size: usize) -> Result<(), MapError> {
    unmap_kernel_range_in_window(vaddr, size, KERNEL_HEAP_BASE, KERNEL_HEAP_USABLE_END)
}

fn unmap_kernel_heap_range_locked(vaddr: usize, size: usize) -> Result<(), MapError> {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        return Err(MapError::NotMapped);
    }
    let root_vaddr = phys_to_virt(root_paddr);

    // 先完整验证范围并按实际 leaf 粒度保存刷新计划，避免后半段错误导致前半段
    // 已经被解除映射，也避免 2 MiB leaf 被按 512 个基本页计算。
    let flush_plan = existing_kernel_tlb_flush_plan(root_vaddr, vaddr, size)?;
    clear_kernel_heap_entries(root_vaddr, vaddr, size)?;

    // 内核堆完整覆盖 2 GiB 时，下级页表的物理上界约为 4 MiB。普通 free 保留
    // 已建立的空 PT/PMD，避免短生命周期大对象反复扫描 512 项、释放页表页并在
    // 下一次分配中重新建立同一层级。叶映射仍在返回前完成全 hart 失效。
    execute_kernel_tlb_flush_plan(&flush_plan, vaddr, size);
    Ok(())
}

#[derive(Clone, Copy)]
struct KernelHeapLeaf {
    level: usize,
    pte_ptr: *mut usize,
    pte: Riscv64Pte,
}

fn find_kernel_heap_leaf_or_guard(
    root_vaddr: usize,
    vaddr: usize,
) -> Result<KernelHeapLeaf, MapError> {
    let mut table_vaddr = root_vaddr;
    for level in 0..Riscv64Paging::LEVELS {
        let index = Riscv64Paging::level_index(vaddr, level);
        let pte_ptr = (table_vaddr + index * core::mem::size_of::<usize>()) as *mut usize;
        let pte = Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pte_ptr) });
        if !Riscv64Paging::pte_is_valid(pte) {
            if software_guard_level(pte) == Some(level) {
                return Ok(KernelHeapLeaf {
                    level,
                    pte_ptr,
                    pte,
                });
            }
            return Err(MapError::NotMapped);
        }
        if Riscv64Paging::pte_is_leaf(pte) {
            return Ok(KernelHeapLeaf {
                level,
                pte_ptr,
                pte,
            });
        }
        table_vaddr = phys_to_virt(Riscv64Paging::pte_addr(pte));
    }
    Err(MapError::NotMapped)
}

fn software_guard_pte(level: usize, paddr: usize) -> Result<Riscv64Pte, MapError> {
    if !Riscv64Paging::supported_leaf_levels().contains(&level)
        || level == 0
        || level > PTE_SOFTWARE_LEVEL_MASK >> PTE_SOFTWARE_LEVEL_SHIFT
        || paddr % PAGE_SIZE != 0
    {
        return Err(MapError::UnsupportedLevel);
    }
    Ok(Riscv64Pte(
        ((paddr >> 12) << 10) | (level << PTE_SOFTWARE_LEVEL_SHIFT),
    ))
}

fn software_guard_level(pte: Riscv64Pte) -> Option<usize> {
    if pte.bits() & PTE_VALID != 0 {
        return None;
    }
    let level = (pte.bits() & PTE_SOFTWARE_LEVEL_MASK) >> PTE_SOFTWARE_LEVEL_SHIFT;
    Riscv64Paging::supported_leaf_levels()
        .contains(&level)
        .then_some(level)
}

fn clear_kernel_heap_entries(root_vaddr: usize, vaddr: usize, size: usize) -> Result<(), MapError> {
    let end = vaddr.checked_add(size).ok_or(MapError::Misaligned)?;
    let mut current = vaddr;
    while current < end {
        let leaf = find_kernel_heap_leaf_or_guard(root_vaddr, current)?;
        let page_size =
            Riscv64Paging::leaf_page_size(leaf.level).ok_or(MapError::UnsupportedLevel)?;
        let next = current.checked_add(page_size).ok_or(MapError::Misaligned)?;
        if current & (page_size - 1) != 0 || next > end {
            return Err(MapError::Misaligned);
        }
        unsafe {
            core::ptr::write_volatile(
                leaf.pte_ptr,
                Riscv64Paging::pte_to_usize(Riscv64Paging::invalid_pte()),
            );
        }
        current = next;
    }
    Ok(())
}

fn protect_kernel_heap_range_locked(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> Result<(), MapError> {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        return Err(MapError::NotMapped);
    }
    if (read || write || execute)
        && !Riscv64Paging::is_valid_leaf_perm(read, write, execute, false, true)
    {
        return Err(MapError::InvalidPermission);
    }

    let root_vaddr = phys_to_virt(root_paddr);
    let flush_plan = existing_kernel_tlb_flush_plan(root_vaddr, vaddr, size)?;
    let end = vaddr.checked_add(size).ok_or(MapError::Misaligned)?;
    let mut current = vaddr;
    while current < end {
        let leaf = find_kernel_heap_leaf_or_guard(root_vaddr, current)?;
        let page_size =
            Riscv64Paging::leaf_page_size(leaf.level).ok_or(MapError::UnsupportedLevel)?;
        let next = current.checked_add(page_size).ok_or(MapError::Misaligned)?;
        let paddr = Riscv64Paging::pte_addr(leaf.pte);
        let new_pte = if !read && !write && !execute {
            software_guard_pte(leaf.level, paddr)?
        } else {
            Riscv64Paging::make_leaf_pte_for_level(
                leaf.level, paddr, read, write, execute, false, true,
            )
            .ok_or(MapError::InvalidPermission)?
        };
        unsafe {
            core::ptr::write_volatile(leaf.pte_ptr, Riscv64Paging::pte_to_usize(new_pte));
        }
        current = next;
    }
    execute_kernel_tlb_flush_plan(&flush_plan, vaddr, size);
    Ok(())
}

pub fn protect_kernel_heap_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> Result<(), MapError> {
    validate_mapping_range(vaddr, None, size, KERNEL_HEAP_BASE, KERNEL_HEAP_USABLE_END)?;
    with_kernel_heap_page_table_lock(|| {
        protect_kernel_heap_range_locked(vaddr, size, read, write, execute)
    })
}

pub fn validate_kernel_heap_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> Result<(), MapError> {
    validate_mapping_range(vaddr, None, size, KERNEL_HEAP_BASE, KERNEL_HEAP_USABLE_END)?;
    with_kernel_heap_page_table_lock(|| {
        let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
        if root_paddr == 0 {
            return Err(MapError::NotMapped);
        }
        validate_range_permissions::<Riscv64Paging>(
            phys_to_virt(root_paddr),
            vaddr,
            size,
            read,
            write,
            execute,
            phys_to_virt,
        )
    })
}
