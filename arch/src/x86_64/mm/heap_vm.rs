//! x86_64 内核堆的页表后端。
//!
//! `allocator` 只负责分配物理页和虚拟区间，页表格式、页表页生命周期以及 TLB
//! 失效则由本模块完成。启动汇编已经建立了一个包含内核镜像和低端 direct-map 的
//! 四级根页表；正式初始化会复用该根，并在一个专用的 PML4 槽中按需建立 heap
//! 映射。这样切换到分层 allocator 时不会短暂丢失当前执行代码的映射。
//!
//! 这个模块刻意只实现内核堆窗口，不把用户地址空间逻辑混入其中。用户 PGD 仍由
//! [`super::user_pgd`] 负责，所有通用页表遍历规则通过 `general::page_walk` 的
//! `PagingArch` 接口复用。

use allocator::{PAGE_SIZE, PagePolicy};
#[cfg(target_os = "none")]
use allocator::{PhysicalAllocRequest, PhysicalAllocation};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use general::{MapError, PagingArch, VirtAddr};
use spin::Mutex;

use crate::x86_64::paging::{
    self, PTE_HUGE, PTE_PRESENT, PTE_SOFT_LEAF, PTE_WRITABLE, PageTableEntry,
};
use crate::x86_64::specific::phys_to_virt;

use super::super::paging::X86_64Paging;

/// 动态内核堆虚拟地址窗口。PML4[384] 在 x86 启动页表中保持为空，专供正式 MM 使用。
pub const KERNEL_HEAP_BASE: usize = 0xffff_c000_0000_0000;
/// 普通内核堆窗口大小（32 GiB）。
pub const KERNEL_HEAP_SIZE: usize = 32 * 1024 * 1024 * 1024;
/// 需要逐对象 registry 记账的独立窗口起点。
pub const TRACKED_HEAP_BASE: usize = KERNEL_HEAP_BASE + KERNEL_HEAP_SIZE;
/// tracked heap 窗口大小（2 GiB）。
pub const TRACKED_HEAP_SIZE: usize = 2 * 1024 * 1024 * 1024;

/// 早期 x86 页表和当前 direct-map 只覆盖低端 4 GiB。boot loader 已经把交接内存图
/// 裁剪到同一范围；在这里再次检查可以防止把一个没有可访问别名的物理页装入堆。
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub const KERNEL_DIRECT_MAP_PHYS_START: usize = 0;
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub const KERNEL_DIRECT_MAP_PHYS_END: usize = 0x1_0000_0000;

const LARGE_PAGE_SIZE: usize = 2 * 1024 * 1024;
const DYNAMIC_PML4_INDEX: usize = 384;
const DYNAMIC_PDPT_ENTRIES: usize = 34; // 32 GiB heap + 2 GiB tracked
const PAGE_TABLE_INIT_UNINITIALIZED: usize = 0;
const PAGE_TABLE_INIT_INITIALIZING: usize = 1;
const PAGE_TABLE_INIT_INITIALIZED: usize = 2;

/// 软件 guard 标记。硬件只检查 P bit，因此 non-present PTE 可以保留原物理地址和
/// 叶粒度，供解除映射/权限恢复路径识别；普通页表 walker 会把它视为未映射。
const PTE_SOFT_GUARD: u64 = 1 << 10;

const _: () =
    assert!(KERNEL_HEAP_SIZE / (1 << 30) + TRACKED_HEAP_SIZE / (1 << 30) == DYNAMIC_PDPT_ENTRIES);

static KERNEL_PAGE_TABLE_ROOT: AtomicUsize = AtomicUsize::new(0);
static PAGE_TABLE_INIT_STATE: AtomicUsize = AtomicUsize::new(PAGE_TABLE_INIT_UNINITIALIZED);
static DYNAMIC_PML4_OWNED: AtomicBool = AtomicBool::new(false);
static KERNEL_HEAP_PAGE_TABLE_LOCK: Mutex<()> = Mutex::new(());

/// 映射统计用于启动诊断和后续性能回归。
static BASE_PAGE_MAPS: AtomicUsize = AtomicUsize::new(0);
static LARGE_PAGE_MAPS: AtomicUsize = AtomicUsize::new(0);
static LARGE_PAGE_FALLBACKS: AtomicUsize = AtomicUsize::new(0);
static TLB_FLUSHES: AtomicUsize = AtomicUsize::new(0);

#[cfg(not(target_os = "none"))]
#[repr(C, align(4096))]
struct HostedPageTablePool {
    pages: [[u8; PAGE_SIZE]; 64],
}

#[cfg(not(target_os = "none"))]
static mut HOSTED_PAGE_TABLE_POOL: HostedPageTablePool = HostedPageTablePool {
    pages: [[0; PAGE_SIZE]; 64],
};
#[cfg(not(target_os = "none"))]
static HOSTED_PAGE_TABLE_NEXT: AtomicUsize = AtomicUsize::new(0);

/// x86 heap 页表统计快照。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelHeapVmStats {
    pub base_page_maps: usize,
    pub large_page_maps: usize,
    pub large_page_fallbacks: usize,
    pub tlb_flushes: usize,
}

pub fn kernel_heap_vm_stats() -> KernelHeapVmStats {
    KernelHeapVmStats {
        base_page_maps: BASE_PAGE_MAPS.load(Ordering::Relaxed),
        large_page_maps: LARGE_PAGE_MAPS.load(Ordering::Relaxed),
        large_page_fallbacks: LARGE_PAGE_FALLBACKS.load(Ordering::Relaxed),
        tlb_flushes: TLB_FLUSHES.load(Ordering::Relaxed),
    }
}

#[inline]
pub const fn kernel_heap_region() -> (usize, usize) {
    (KERNEL_HEAP_BASE, KERNEL_HEAP_SIZE)
}

#[inline]
pub const fn tracked_heap_region() -> (usize, usize) {
    (TRACKED_HEAP_BASE, TRACKED_HEAP_SIZE)
}

#[inline]
fn dynamic_window(vaddr: usize, size: usize) -> Option<(usize, usize)> {
    let end = vaddr.checked_add(size)?;
    let kernel_end = KERNEL_HEAP_BASE.checked_add(KERNEL_HEAP_SIZE)?;
    let tracked_end = TRACKED_HEAP_BASE.checked_add(TRACKED_HEAP_SIZE)?;
    if vaddr >= KERNEL_HEAP_BASE && end <= kernel_end {
        return Some((KERNEL_HEAP_BASE, kernel_end));
    }
    if vaddr >= TRACKED_HEAP_BASE && end <= tracked_end {
        return Some((TRACKED_HEAP_BASE, tracked_end));
    }
    None
}

#[inline]
fn physical_range_valid(paddr: usize, size: usize) -> bool {
    let Some(end) = paddr.checked_add(size) else {
        return false;
    };
    #[cfg(target_os = "none")]
    {
        paddr >= KERNEL_DIRECT_MAP_PHYS_START && end <= KERNEL_DIRECT_MAP_PHYS_END
    }
    #[cfg(not(target_os = "none"))]
    {
        // Hosted tests use process addresses as synthetic physical addresses. They still
        // must fit the x86 PTE address field and retain page alignment.
        (paddr as u64) <= paging::PTE_ADDR_MASK && (end as u64) <= paging::PTE_ADDR_MASK + 1
    }
}

#[inline]
fn root_phys() -> Option<usize> {
    let root = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    (root != 0).then_some(root)
}

#[inline]
fn root_virt() -> Result<usize, MapError> {
    let root = root_phys().ok_or(MapError::OutOfMemory)?;
    if root & (PAGE_SIZE - 1) != 0 || !X86_64Paging::physical_address_valid(root) {
        return Err(MapError::NotMapped);
    }
    let virt = phys_to_virt(root);
    (virt != 0).then_some(virt).ok_or(MapError::NotMapped)
}

/// 初始化正式内核页表。
///
/// Multiboot2 入口已经把当前 CR3 指向包含高半区内核映射的静态根；复用它是必要的，
/// 因为函数执行期间不能切换到一个尚未复制内核映射的新根。没有启动根时，host 单测
/// 使用静态页表池；裸机则立即停止在明确的初始化错误上。
pub fn init_kernel_page_table() {
    match PAGE_TABLE_INIT_STATE.compare_exchange(
        PAGE_TABLE_INIT_UNINITIALIZED,
        PAGE_TABLE_INIT_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(PAGE_TABLE_INIT_INITIALIZED) => return,
        Err(_) => panic!("[x86][heap_vm] recursive kernel page-table initialization"),
    }

    #[allow(unused_mut)]
    let mut root = paging::read_cr3() & !(0xfff | (1usize << 63));
    #[cfg(not(target_os = "none"))]
    if root == 0 {
        let index = HOSTED_PAGE_TABLE_NEXT.fetch_add(1, Ordering::AcqRel);
        if index >= 64 {
            panic!("[x86][heap_vm] hosted page-table pool exhausted");
        }
        root = unsafe { HOSTED_PAGE_TABLE_POOL.pages[index].as_ptr() as usize };
    }
    if root == 0 || root & (PAGE_SIZE - 1) != 0 || !X86_64Paging::physical_address_valid(root) {
        panic!("[x86][heap_vm] invalid boot CR3 root {root:#x}");
    }

    let root_vaddr = phys_to_virt(root);
    if root_vaddr == 0 {
        panic!("[x86][heap_vm] boot CR3 root has no virtual alias");
    }
    let pml4_ptr = (root_vaddr + DYNAMIC_PML4_INDEX * core::mem::size_of::<usize>()) as *mut usize;
    let existing = X86_64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pml4_ptr) });
    if X86_64Paging::pte_is_valid(existing) {
        panic!("[x86][heap_vm] dynamic heap PML4 slot is already occupied");
    }

    KERNEL_PAGE_TABLE_ROOT.store(root, Ordering::Release);
    crate::x86_64::mm::set_kernel_page_table(root);
    // Keep CR3 mirror synchronized on hosted builds. On bare targets this is the already active
    // root; rewriting it is unnecessary and would discard boot-time PCID policy.
    #[cfg(not(target_os = "none"))]
    unsafe {
        paging::write_cr3(root);
    }
    DYNAMIC_PML4_OWNED.store(true, Ordering::Release);
    PAGE_TABLE_INIT_STATE.store(PAGE_TABLE_INIT_INITIALIZED, Ordering::Release);
}

#[inline]
fn kernel_table_pte(paddr: usize) -> PageTableEntry {
    PageTableEntry::new(paddr as u64, PTE_PRESENT | PTE_WRITABLE)
}

fn allocate_page_table_page() -> Result<usize, MapError> {
    #[cfg(target_os = "none")]
    {
        let allocation = allocator::KERNEL_ALLOCATOR
            .allocate_untracked_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
            .map_err(|_| MapError::OutOfMemory)?;
        if !physical_range_valid(allocation.paddr, PAGE_SIZE) {
            let _ = allocator::KERNEL_ALLOCATOR.try_free_untracked_physical(allocation);
            return Err(MapError::NotMapped);
        }
        Ok(allocation.paddr)
    }
    #[cfg(not(target_os = "none"))]
    {
        let index = HOSTED_PAGE_TABLE_NEXT.fetch_add(1, Ordering::AcqRel);
        if index >= 64 {
            return Err(MapError::OutOfMemory);
        }
        let paddr = unsafe { HOSTED_PAGE_TABLE_POOL.pages[index].as_ptr() as usize };
        unsafe { core::ptr::write_bytes(paddr as *mut u8, 0, PAGE_SIZE) };
        Ok(paddr)
    }
}

fn free_page_table_page(paddr: usize) -> bool {
    #[cfg(target_os = "none")]
    {
        let allocation = PhysicalAllocation {
            paddr,
            size: PAGE_SIZE,
            order: 0,
            page_size: PAGE_SIZE,
        };
        allocator::KERNEL_ALLOCATOR
            .try_free_untracked_physical(allocation)
            .is_ok()
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = paddr;
        true
    }
}

#[inline]
fn table_from_pte(pte: PageTableEntry) -> Result<usize, MapError> {
    if !X86_64Paging::pte_is_valid(pte) || X86_64Paging::pte_is_leaf(pte) {
        return Err(MapError::NotMapped);
    }
    let paddr = X86_64Paging::pte_addr(pte);
    if !physical_range_valid(paddr, PAGE_SIZE) {
        return Err(MapError::NotMapped);
    }
    let virt = phys_to_virt(paddr);
    (virt != 0).then_some(virt).ok_or(MapError::NotMapped)
}

/// 在一个缺失层级上原子地构造页表链。新页表在父 PTE 发布前全部清零，失败时释放
/// 尚未可达的页表页，避免 allocator 看到半成品树。
fn walk_and_map_heap(
    root: usize,
    vaddr: usize,
    paddr: usize,
    target_level: usize,
) -> Result<(), MapError> {
    if target_level >= X86_64Paging::LEVELS || X86_64Paging::leaf_page_size(target_level).is_none()
    {
        return Err(MapError::UnsupportedLevel);
    }

    let mut table = root;
    for level in 0..target_level {
        let index = X86_64Paging::level_index(vaddr, level);
        let entry_ptr = (table + index * core::mem::size_of::<usize>()) as *mut usize;
        let entry = X86_64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(entry_ptr) });
        if X86_64Paging::pte_is_valid(entry) {
            if X86_64Paging::pte_is_leaf(entry) {
                return Err(MapError::AlreadyMapped);
            }
            table = table_from_pte(entry)?;
            continue;
        }

        let mut allocated = [0usize; X86_64Paging::LEVELS];
        let mut allocated_count = 0usize;
        let first = match allocate_page_table_page() {
            Ok(paddr) => paddr,
            Err(error) => return Err(error),
        };
        allocated[allocated_count] = first;
        allocated_count += 1;
        let mut private_table = phys_to_virt(first);
        unsafe { core::ptr::write_bytes(private_table as *mut u8, 0, PAGE_SIZE) };

        for child_level in (level + 1)..target_level {
            let child = match allocate_page_table_page() {
                Ok(paddr) => paddr,
                Err(error) => {
                    for paddr in allocated[..allocated_count].iter().copied().rev() {
                        let _ = free_page_table_page(paddr);
                    }
                    return Err(error);
                }
            };
            allocated[allocated_count] = child;
            allocated_count += 1;
            let child_vaddr = phys_to_virt(child);
            unsafe { core::ptr::write_bytes(child_vaddr as *mut u8, 0, PAGE_SIZE) };
            let child_index = X86_64Paging::level_index(vaddr, child_level);
            let child_ptr =
                (private_table + child_index * core::mem::size_of::<usize>()) as *mut usize;
            unsafe {
                core::ptr::write_volatile(child_ptr, kernel_table_pte(child).0 as usize);
            }
            private_table = child_vaddr;
        }

        let leaf = match X86_64Paging::make_leaf_pte_for_level(
            target_level,
            paddr,
            true,
            true,
            false,
            false,
            false,
        ) {
            Some(leaf) => leaf,
            None => {
                for paddr in allocated[..allocated_count].iter().copied().rev() {
                    let _ = free_page_table_page(paddr);
                }
                return Err(MapError::InvalidPermission);
            }
        };
        let leaf_index = X86_64Paging::level_index(vaddr, target_level);
        let leaf_ptr = (private_table + leaf_index * core::mem::size_of::<usize>()) as *mut usize;
        unsafe {
            core::ptr::write_volatile(leaf_ptr, leaf.0 as usize);
            core::sync::atomic::fence(Ordering::Release);
            core::ptr::write_volatile(entry_ptr, kernel_table_pte(first).0 as usize);
        }
        return Ok(());
    }

    let index = X86_64Paging::level_index(vaddr, target_level);
    let leaf_ptr = (table + index * core::mem::size_of::<usize>()) as *mut usize;
    let old = X86_64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(leaf_ptr) });
    // A software guard is an intentionally non-present leaf. Treat it as an
    // available slot when the allocator reuses the virtual range; only a
    // hardware-present mapping is a true collision.
    if X86_64Paging::pte_is_valid(old) {
        return Err(MapError::AlreadyMapped);
    }
    let leaf =
        X86_64Paging::make_leaf_pte_for_level(target_level, paddr, true, true, false, false, false)
            .ok_or(MapError::InvalidPermission)?;
    core::sync::atomic::fence(Ordering::Release);
    unsafe { core::ptr::write_volatile(leaf_ptr, leaf.0 as usize) };
    Ok(())
}

#[derive(Clone, Copy)]
struct HeapLeaf {
    level: usize,
    ptr: *mut usize,
    pte: PageTableEntry,
    guard: bool,
}

fn find_heap_leaf(root: usize, vaddr: usize) -> Result<HeapLeaf, MapError> {
    let mut table = root;
    for level in 0..X86_64Paging::LEVELS {
        let index = X86_64Paging::level_index(vaddr, level);
        let ptr = (table + index * core::mem::size_of::<usize>()) as *mut usize;
        let pte = X86_64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(ptr) });
        let guard = !pte.is_present() && pte.0 & PTE_SOFT_GUARD != 0;
        if guard {
            return Ok(HeapLeaf {
                level,
                ptr,
                pte,
                guard: true,
            });
        }
        if !pte.is_present() {
            return Err(MapError::NotMapped);
        }
        if X86_64Paging::pte_is_leaf(pte) {
            return Ok(HeapLeaf {
                level,
                ptr,
                pte,
                guard: false,
            });
        }
        table = table_from_pte(pte)?;
    }
    Err(MapError::NotMapped)
}

#[inline]
fn validate_range(
    vaddr: usize,
    size: usize,
    paddr: Option<usize>,
) -> Result<(usize, usize), MapError> {
    if size == 0 || vaddr % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 {
        return Err(MapError::Misaligned);
    }
    let end = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    if !paging::is_canonical(vaddr as u64, false) || !paging::is_canonical((end - 1) as u64, false)
    {
        return Err(MapError::NotMapped);
    }
    let window = dynamic_window(vaddr, size).ok_or(MapError::NotMapped)?;
    if let Some(paddr) = paddr {
        if paddr % PAGE_SIZE != 0 || !physical_range_valid(paddr, size) {
            return Err(MapError::NotMapped);
        }
    }
    Ok(window)
}

fn choose_leaf(
    page_policy: PagePolicy,
    vaddr: usize,
    paddr: usize,
    size: usize,
) -> Result<(usize, usize), MapError> {
    let aligned_large =
        vaddr % LARGE_PAGE_SIZE == 0 && paddr % LARGE_PAGE_SIZE == 0 && size % LARGE_PAGE_SIZE == 0;
    match page_policy {
        PagePolicy::BaseOnly => Ok((3, PAGE_SIZE)),
        PagePolicy::PreferLarge if size >= LARGE_PAGE_SIZE && aligned_large => {
            Ok((2, LARGE_PAGE_SIZE))
        }
        PagePolicy::PreferLarge => {
            LARGE_PAGE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            Ok((3, PAGE_SIZE))
        }
        PagePolicy::RequireLarge if size >= LARGE_PAGE_SIZE && aligned_large => {
            Ok((2, LARGE_PAGE_SIZE))
        }
        PagePolicy::RequireLarge if size < LARGE_PAGE_SIZE => Err(MapError::UnsupportedHugePage),
        PagePolicy::RequireLarge => Err(MapError::Misaligned),
    }
}

fn flush_range(vaddr: usize, size: usize) {
    // Dynamic leaves are deliberately non-global, so a CR3 reload is sufficient for large
    // ranges and avoids millions of INVLPG instructions. Small ranges use Linux-like INVLPG.
    let pages = size / PAGE_SIZE;
    if pages > 256 {
        unsafe { paging::flush_tlb() };
    } else {
        let mut current = vaddr;
        let end = vaddr.saturating_add(size);
        while current < end {
            unsafe { X86_64Paging::flush_tlb(Some(VirtAddr::new(current))) };
            current = current.saturating_add(PAGE_SIZE);
        }
    }
    TLB_FLUSHES.fetch_add(1, Ordering::Relaxed);
}

fn map_range_locked(
    vaddr: usize,
    paddr: usize,
    size: usize,
    policy: PagePolicy,
) -> Result<(), MapError> {
    validate_range(vaddr, size, Some(paddr))?;
    let root = root_virt()?;
    let (level, leaf_size) = choose_leaf(policy, vaddr, paddr, size)?;
    let end = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let mut current_vaddr = vaddr;
    let mut current_paddr = paddr;
    let mut mapped = 0usize;
    while current_vaddr < end {
        if let Err(error) = walk_and_map_heap(root, current_vaddr, current_paddr, level) {
            if mapped != 0 {
                // The transaction only publishes complete leaves; clear those leaves before
                // returning the physical block to allocator::space.
                let _ = clear_range_locked(root, vaddr, mapped);
                flush_range(vaddr, mapped);
            }
            return Err(error);
        }
        mapped = mapped.checked_add(leaf_size).ok_or(MapError::NotMapped)?;
        current_vaddr = current_vaddr
            .checked_add(leaf_size)
            .ok_or(MapError::NotMapped)?;
        current_paddr = current_paddr
            .checked_add(leaf_size)
            .ok_or(MapError::NotMapped)?;
    }
    if leaf_size == PAGE_SIZE {
        BASE_PAGE_MAPS.fetch_add(size / PAGE_SIZE, Ordering::Relaxed);
    } else {
        LARGE_PAGE_MAPS.fetch_add(size / leaf_size, Ordering::Relaxed);
    }
    flush_range(vaddr, size);
    Ok(())
}

fn validate_exact_leaves(root: usize, vaddr: usize, size: usize) -> Result<(), MapError> {
    let end = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let mut current = vaddr;
    while current < end {
        let leaf = find_heap_leaf(root, current)?;
        let leaf_size =
            X86_64Paging::leaf_page_size(leaf.level).ok_or(MapError::UnsupportedLevel)?;
        let base = current & !(leaf_size - 1);
        let next = base.checked_add(leaf_size).ok_or(MapError::NotMapped)?;
        if base != current || next > end {
            return Err(MapError::Misaligned);
        }
        current = next;
    }
    Ok(())
}

fn clear_range_locked(root: usize, vaddr: usize, size: usize) -> Result<(), MapError> {
    validate_exact_leaves(root, vaddr, size)?;
    let end = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let mut current = vaddr;
    while current < end {
        let leaf = find_heap_leaf(root, current)?;
        unsafe { core::ptr::write_volatile(leaf.ptr, 0) };
        let leaf_size =
            X86_64Paging::leaf_page_size(leaf.level).ok_or(MapError::UnsupportedLevel)?;
        current = current.checked_add(leaf_size).ok_or(MapError::NotMapped)?;
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    Ok(())
}

fn guard_pte(leaf: HeapLeaf) -> PageTableEntry {
    let mut flags = PTE_SOFT_GUARD;
    if leaf.level < 3 {
        flags |= PTE_HUGE;
    } else {
        flags |= PTE_SOFT_LEAF;
    }
    PageTableEntry::new(X86_64Paging::pte_addr(leaf.pte) as u64, flags)
}

fn protect_range_locked(
    root: usize,
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> Result<(), MapError> {
    if !read && !write && !execute {
        validate_exact_leaves(root, vaddr, size)?;
    } else if !X86_64Paging::is_valid_leaf_perm(read, write, execute, false, false) {
        return Err(MapError::InvalidPermission);
    } else {
        validate_exact_leaves(root, vaddr, size)?;
    }

    let end = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    let mut current = vaddr;
    while current < end {
        let leaf = find_heap_leaf(root, current)?;
        let leaf_size =
            X86_64Paging::leaf_page_size(leaf.level).ok_or(MapError::UnsupportedLevel)?;
        let replacement = if !read && !write && !execute {
            guard_pte(leaf)
        } else {
            X86_64Paging::make_leaf_pte_for_level(
                leaf.level,
                X86_64Paging::pte_addr(leaf.pte),
                read,
                write,
                execute,
                false,
                false,
            )
            .ok_or(MapError::InvalidPermission)?
        };
        unsafe { core::ptr::write_volatile(leaf.ptr, replacement.0 as usize) };
        current = current.checked_add(leaf_size).ok_or(MapError::NotMapped)?;
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    flush_range(vaddr, size);
    Ok(())
}

/// 为内核堆建立物理后备映射。
pub fn map_kernel_heap_range(
    vaddr: usize,
    paddr: usize,
    size: usize,
    policy: PagePolicy,
) -> Result<(), MapError> {
    let _guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    map_range_locked(vaddr, paddr, size, policy)
}

/// 解除内核堆映射。范围必须与当前叶粒度完整对齐，避免隐式拆分大页。
pub fn unmap_kernel_heap_range(vaddr: usize, size: usize) -> Result<(), MapError> {
    validate_range(vaddr, size, None)?;
    let _guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    let root = root_virt()?;
    clear_range_locked(root, vaddr, size)?;
    flush_range(vaddr, size);
    Ok(())
}

/// 修改内核堆页权限；三项权限均为 false 时安装软件 guard PTE。
pub fn protect_kernel_heap_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> Result<(), MapError> {
    validate_range(vaddr, size, None)?;
    let _guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    protect_range_locked(root_virt()?, vaddr, size, read, write, execute)
}

/// 校验范围内每个叶子映射的权限。允许起止地址落在同一大页内部。
pub fn validate_kernel_heap_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> Result<(), MapError> {
    if size == 0 {
        return Err(MapError::Misaligned);
    }
    let end = vaddr.checked_add(size).ok_or(MapError::NotMapped)?;
    if !paging::is_canonical(vaddr as u64, false) || !paging::is_canonical((end - 1) as u64, false)
    {
        return Err(MapError::NotMapped);
    }
    dynamic_window(vaddr, size).ok_or(MapError::NotMapped)?;
    let _guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    let root = root_virt()?;
    let mut current = vaddr;
    while current < end {
        let leaf = find_heap_leaf(root, current)?;
        if leaf.guard {
            return Err(MapError::NotMapped);
        }
        let flags = X86_64Paging::pte_flags(leaf.pte);
        if (read && !X86_64Paging::flags_readable(flags))
            || (write && !X86_64Paging::flags_writable(flags))
            || (execute && !X86_64Paging::flags_executable(flags))
        {
            return Err(MapError::InvalidPermission);
        }
        let leaf_size =
            X86_64Paging::leaf_page_size(leaf.level).ok_or(MapError::UnsupportedLevel)?;
        let base = current & !(leaf_size - 1);
        current = base
            .checked_add(leaf_size)
            .ok_or(MapError::NotMapped)?
            .min(end);
    }
    Ok(())
}

/// 为 `StartAddressOps::virt_to_phys` 提供动态 heap 窗口的反向转换。
#[cfg(target_os = "none")]
pub(crate) fn virt_to_phys(vaddr: usize) -> Option<usize> {
    if dynamic_window(vaddr, 1).is_none() {
        return None;
    }
    let _guard = KERNEL_HEAP_PAGE_TABLE_LOCK.lock();
    let root = root_virt().ok()?;
    let leaf = find_heap_leaf(root, vaddr).ok()?;
    if leaf.guard {
        return None;
    }
    let page_size = X86_64Paging::leaf_page_size(leaf.level)?;
    X86_64Paging::pte_addr(leaf.pte).checked_add(vaddr & (page_size - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_windows_are_canonical_and_disjoint() {
        assert!(paging::is_canonical(KERNEL_HEAP_BASE as u64, false));
        assert!(paging::is_canonical(
            (TRACKED_HEAP_BASE + TRACKED_HEAP_SIZE - 1) as u64,
            false
        ));
        assert!(KERNEL_HEAP_BASE + KERNEL_HEAP_SIZE <= TRACKED_HEAP_BASE);
    }

    #[test]
    fn policy_prefers_large_pages_only_when_fully_aligned() {
        assert_eq!(
            choose_leaf(
                PagePolicy::PreferLarge,
                KERNEL_HEAP_BASE,
                LARGE_PAGE_SIZE,
                LARGE_PAGE_SIZE
            ),
            Ok((2, LARGE_PAGE_SIZE))
        );
        assert_eq!(
            choose_leaf(
                PagePolicy::PreferLarge,
                KERNEL_HEAP_BASE + PAGE_SIZE,
                LARGE_PAGE_SIZE,
                LARGE_PAGE_SIZE
            ),
            Ok((3, PAGE_SIZE))
        );
        assert_eq!(
            choose_leaf(PagePolicy::RequireLarge, KERNEL_HEAP_BASE, 0, PAGE_SIZE),
            Err(MapError::UnsupportedHugePage)
        );
    }
}
