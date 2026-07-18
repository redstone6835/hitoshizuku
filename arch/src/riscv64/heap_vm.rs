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
//!   PUD[2]:    kernel code direct map（1GiB，512×2MiB，PA 0x8000_0000 起）
//!   PUD[3]:    kernel direct map 扩展（1GiB leaf，PA 0xC000_0000 起）
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

use allocator::{PAGE_SIZE, PagePolicy, PhysicalAllocRequest, PhysicalAllocation};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};
use general::{MapError, PagingArch, find_leaf, unmap_range_entries, validate_range_permissions};
use spin::Mutex;

use crate::riscv64::paging::{Riscv64Paging, Riscv64Pte};
use crate::riscv64::specific::{KERNEL_VA_OFFSET, SATP_MODE_SV48, phys_to_virt};
use crate::riscv64::trap::LocalIrqGuard;

// ── 常量与静态 ──────────────────────────────────────────────────────────────────

/// 内核堆虚拟地址起始（PGD[511]→PUD[0]）。
pub const KERNEL_HEAP_BASE: usize = 0xFFFF_FF80_0000_0000;

/// 内核堆虚拟地址范围大小（2 GiB）。
///
/// 对应页表布局中 PGD[511]→PUD[0..1]，每 PUD 覆盖 1 GiB。
pub const KERNEL_HEAP_SIZE: usize = 2 * 1024 * 1024 * 1024;

/// MMIO 直接映射基址（PGD[510]，独立于 kernel heap/code）。
///
/// `device_mmio_to_virt(paddr) = paddr + MMIO_VIRT_BASE`。
pub const MMIO_VIRT_BASE: usize = 0xFFFF_FF00_0000_0000;

/// 内核 direct map 覆盖物理 RAM 的基址和大小（QEMU virt 默认从 0x80000000 开始）。
const KERNEL_PHYS_BASE: usize = 0x8000_0000;
const KERNEL_DIRECT_MAP_SIZE: usize = 0x4000_0000; // 1 GiB
const HEAP_PMD_SIZE: usize = 2 * 1024 * 1024;
const HEAP_PUD_SIZE: usize = 1024 * 1024 * 1024;
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
    }
}

pub fn kernel_heap_region() -> (usize, usize) {
    (KERNEL_HEAP_BASE, KERNEL_HEAP_SIZE)
}

pub fn kernel_virt_to_phys(vaddr: usize) -> Result<usize, MapError> {
    if !Riscv64Paging::is_canonical_vaddr(vaddr) {
        return Err(MapError::NotMapped);
    }

    // heap 的下级页表可能在 unmap 后被立即回收。查询必须与 map/unmap 使用同一把
    // 结构锁，否则 find_leaf() 可能沿着刚被摘除并释放的页表页继续遍历。
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
    let request = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map_err(|_| MapError::OutOfMemory)
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
            table_vaddr = phys_to_virt(Riscv64Paging::pte_addr(pte));
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
    pud_kernel: *mut usize,
}

/// 定位并验证 boot 汇编建立的 Sv48 根页表与 kernel PUD。
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

    for index in 2..=3usize {
        let direct_map_pte = Riscv64Pte(unsafe { core::ptr::read_volatile(pud_kernel.add(index)) });
        assert!(
            Riscv64Paging::pte_is_valid(direct_map_pte)
                && Riscv64Paging::pte_is_leaf(direct_map_pte),
            "[arch][heap_vm] early PUD[{index}] is not the expected 1 GiB leaf"
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
        pud_kernel,
    }
}

/// 构造 kernel direct-map 的 PMD 页，并按链接段边界设置 W^X 权限。
fn build_direct_map_pmd() -> PhysicalAllocation {
    let allocation = alloc_page_table_allocation()
        .expect("[arch][heap_vm] failed to allocate direct-map PMD page");
    let pmd = phys_to_virt(allocation.paddr) as *mut usize;

    unsafe extern "C" {
        fn stext();
        fn etext();
        fn erodata();
    }

    let direct_map_entries = KERNEL_DIRECT_MAP_SIZE / HEAP_PMD_SIZE;
    let direct_map_end = KERNEL_VIRT_BASE + KERNEL_DIRECT_MAP_SIZE;
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

    // `stext` 所在的整个 2 MiB leaf 必须可执行，因此向下取整；release linker 中
    // `stext` 本身已对齐，debug linker 的前置 padding 则会成为只读可执行区域。
    let text_start_index = (text_start - KERNEL_VIRT_BASE) / HEAP_PMD_SIZE;
    let text_end_index = (text_end - KERNEL_VIRT_BASE) / HEAP_PMD_SIZE;
    let rodata_end_index = (rodata_end - KERNEL_VIRT_BASE) / HEAP_PMD_SIZE;

    for index in 0..direct_map_entries {
        let paddr = KERNEL_PHYS_BASE + index * HEAP_PMD_SIZE;
        // 根据段归属设置权限（linker 脚本保证 etext/erodata 在 2MiB 边界上）：
        //   [0, text leaf)      → R+W（固件/保留区，不允许执行）
        //   [text leaf, etext)  → R+X（代码段及可能的前置 padding）
        //   [etext, erodata)    → R  （只读数据段，不可写）
        //   [erodata, ...)      → R+W（可写数据段和 BSS，不可执行）
        let (read, write, execute) = if index < text_start_index {
            (true, true, false)
        } else if index < text_end_index {
            (true, false, true)
        } else if index < rodata_end_index {
            (true, false, false)
        } else {
            (true, true, false)
        };
        let leaf = Riscv64Paging::make_leaf_pte(paddr, read, write, execute, false, true);
        unsafe { core::ptr::write_volatile(pmd.add(index), leaf.bits()) };
    }

    allocation
}

/// 构造 PGD[510] 下的 MMIO 与 PCI 32-bit BAR 直接映射窗口。
fn build_mmio_pud() -> Result<PhysicalAllocation, MapError> {
    let allocation = alloc_page_table_allocation()?;
    let pud = phys_to_virt(allocation.paddr) as *mut usize;
    unsafe { core::ptr::write_bytes(pud, 0, PAGE_SIZE / core::mem::size_of::<usize>()) };

    let mmio_leaf = Riscv64Paging::make_leaf_pte(0, true, true, false, false, true);
    unsafe { core::ptr::write_volatile(pud, mmio_leaf.bits()) };
    let pci32_leaf = Riscv64Paging::make_leaf_pte(0x4000_0000, true, true, false, false, true);
    unsafe { core::ptr::write_volatile(pud.add(1), pci32_leaf.bits()) };
    Ok(allocation)
}

/// 在所有子页表构造完成后发布父 PTE，并完成全局 TLB/取指同步。
fn publish_kernel_page_tables(
    layout: &EarlyPageTableLayout,
    direct_map_pmd_paddr: usize,
    mmio_pud_paddr: usize,
) {
    let direct_map_pte = Riscv64Paging::make_table_pte(direct_map_pmd_paddr);
    let mmio_pgd_pte = Riscv64Paging::make_table_pte(mmio_pud_paddr);
    let upper_ram_paddr = KERNEL_PHYS_BASE + KERNEL_DIRECT_MAP_SIZE;
    let upper_ram_leaf =
        Riscv64Paging::make_leaf_pte(upper_ram_paddr, true, true, false, false, true);

    unsafe {
        // 子页表初始化必须先于父 PTE 发布对硬件 page walker 可见。
        core::arch::asm!("fence w, w");
        core::ptr::write_volatile(layout.pgd.add(510), mmio_pgd_pte.bits());
        // PUD[3] 不包含内核代码，启动完成后收敛为 RW+NX。
        core::ptr::write_volatile(layout.pud_kernel.add(3), upper_ram_leaf.bits());
        // 最后替换当前正在执行代码所属的 PUD[2]。
        core::ptr::write_volatile(layout.pud_kernel.add(2), direct_map_pte.bits());

        // 确保 PTE store 到达内存后再刷 TLB/page-walk cache 和指令流。
        core::arch::asm!("fence rw, rw");
        Riscv64Paging::flush_tlb_global(None);
        core::arch::asm!("fence.i");
    }
}

/// UART 切到高半区后移除 boot 阶段使用的 PGD[0] identity mapping。
fn remove_identity_mapping(layout: &EarlyPageTableLayout) {
    unsafe {
        core::ptr::write_volatile(layout.pgd, 0);
        Riscv64Paging::flush_tlb_global(None);
    }
}

/// 原地修改早期页表 PUD_kernel[2] 从 1GiB leaf → table PTE
/// → PMD（512×2MiB leaves）。用全局 sfence.vma 冲刷一切 TLB/page-walk cache，
/// 保证后续指令取指走新映射。
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

    let layout = locate_early_page_tables();
    let direct_map_pmd = build_direct_map_pmd();
    let mmio_pud = match build_mmio_pud() {
        Ok(allocation) => allocation,
        Err(err) => {
            free_page_table_allocation(direct_map_pmd);
            panic!("[arch][heap_vm] failed to allocate MMIO PUD page: {err:?}");
        }
    };

    publish_kernel_page_tables(&layout, direct_map_pmd.paddr, mmio_pud.paddr);
    log::info!(
        "[arch][heap_vm] PUD[2] converted to {} x 2MiB leaves; PUD[0..1] reserved for heap",
        KERNEL_DIRECT_MAP_SIZE / HEAP_PMD_SIZE
    );

    KERNEL_PAGE_TABLE_ROOT.store(layout.root_paddr, Ordering::Release);
    verify_kernel_segments(layout.root_paddr);

    // MMIO 映射就绪后 UART 不再依赖低地址，identity mapping 才可安全拆除。
    crate::riscv64::early_console::switch_to_virtual();
    remove_identity_mapping(&layout);

    PAGE_TABLE_INIT_STATE.store(PAGE_TABLE_INITIALIZED, Ordering::Release);
    log::info!("[arch][heap_vm] identity mapping (PGD[0]) removed");
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
            let (level, _ptr, pte) = find_leaf::<Riscv64Paging>(root, va, phys_to_virt)
                .unwrap_or_else(|err| {
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
            let actual_paddr = Riscv64Paging::pte_addr(pte)
                .checked_add(va & (page_size - 1))
                .expect("[heap_vm] mapped physical address overflow");
            let expected_paddr = va.wrapping_sub(virt_offset);
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
    let first_direct_map_end = KERNEL_VIRT_BASE + KERNEL_DIRECT_MAP_SIZE;
    let full_direct_map_end = first_direct_map_end + 1024 * 1024 * 1024;
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
        "PUD[3] upper direct map",
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

fn validate_heap_range(vaddr: usize, paddr: Option<usize>, size: usize) -> Result<(), MapError> {
    if size == 0 || vaddr % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 {
        return Err(MapError::Misaligned);
    }
    if paddr.is_some_and(|addr| addr % PAGE_SIZE != 0) {
        return Err(MapError::Misaligned);
    }

    let end_vaddr = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let heap_end = KERNEL_HEAP_BASE
        .checked_add(KERNEL_HEAP_SIZE)
        .ok_or(MapError::NotMapped)?;
    if vaddr < KERNEL_HEAP_BASE || end_vaddr > heap_end {
        return Err(MapError::NotMapped);
    }
    if let Some(addr) = paddr {
        addr.checked_add(size).ok_or(MapError::NotMapped)?;
    }
    Ok(())
}

#[inline]
fn flush_kernel_tlb_all() {
    unsafe { Riscv64Paging::flush_tlb_global(None) };
    TLB_GLOBAL_FLUSHES.fetch_add(1, Ordering::Relaxed);
}

fn execute_kernel_tlb_flush_plan(plan: &KernelTlbFlushPlan) {
    if plan.global {
        flush_kernel_tlb_all();
        return;
    }

    for &vaddr in &plan.addresses[..plan.count] {
        flush_kernel_tlb_addr(vaddr);
        TLB_ADDRESS_FLUSHES.fetch_add(1, Ordering::Relaxed);
    }
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

fn free_page_table_page(paddr: usize) {
    if let Err(err) = allocator::KERNEL_ALLOCATOR.try_free_physical_addr(paddr) {
        log::error!(
            "[arch][heap_vm] failed to free page-table page paddr={:#x}: {:?}",
            paddr,
            err
        );
    } else {
        PAGE_TABLE_PAGES_RECLAIMED.fetch_add(1, Ordering::Relaxed);
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
) -> Result<(), MapError> {
    validate_heap_range(vaddr, Some(paddr), size)?;

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
            return map_range_with_policy(vaddr, paddr, size, PagePolicy::BaseOnly);
        }
    }

    let end_vaddr = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let mut current_vaddr = vaddr;
    let mut current_paddr = paddr;

    while current_vaddr < end_vaddr {
        if let Err(err) = walk_and_map_heap(root_vaddr, current_vaddr, current_paddr, target_level)
        {
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
                    execute_kernel_tlb_flush_plan(&plan);
                }
            }

            if page_policy == PagePolicy::PreferLarge && matches!(err, MapError::AlreadyMapped) {
                LARGE_PAGE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                return map_range_with_policy(vaddr, paddr, size, PagePolicy::BaseOnly);
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
    execute_kernel_tlb_flush_plan(&plan);
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

pub(crate) fn flush_kernel_tlb_addr(vaddr: usize) {
    unsafe {
        Riscv64Paging::flush_tlb_global(Some(general::VirtAddr::new(vaddr)));
    }
}

pub fn map_kernel_heap_range(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
) -> Result<(), MapError> {
    validate_heap_range(vaddr, Some(paddr), size)?;
    with_kernel_heap_page_table_lock(|| map_range_with_policy(vaddr, paddr, size, page_policy))
}

pub fn unmap_kernel_heap_range(vaddr: usize, size: usize) -> Result<(), MapError> {
    validate_heap_range(vaddr, None, size)?;

    with_kernel_heap_page_table_lock(|| unmap_kernel_heap_range_locked(vaddr, size))
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

    // 若摘除了中间页表，reclaim 内的一次全局 flush 已同时覆盖叶 translation；
    // 否则按预先保存的实际 leaf 地址做精确刷新。
    if !reclaim_empty_heap_page_tables(root_vaddr, vaddr, size) {
        execute_kernel_tlb_flush_plan(&flush_plan);
    }
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
    execute_kernel_tlb_flush_plan(&flush_plan);
    Ok(())
}

pub fn protect_kernel_heap_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> Result<(), MapError> {
    validate_heap_range(vaddr, None, size)?;
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
    validate_heap_range(vaddr, None, size)?;
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
