//! LoongArch64 用户态页表能力 → 注入到 [`general::mm::UserPgdOps`]。
//!
//! 内部 `UserPgdInner` 是 arch 私有结构，由 `Box::leak` 后用 `NonNull<()>`
//! 套进 [`PgdHandle`] 传给上层；`drop_pgd` 反向还原。
//!
//! 物理页表分配 / 释放走 [`allocator::KERNEL_ALLOCATOR`]；映射走泛型
//! `general::page_walk::walk_and_map::<LoongArch64Paging>`。

use alloc::boxed::Box;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use general::mm::{PgdHandle, UserPgdOps};
use general::{
    MapBatchResult, PagingArch, PhysPageTableRoot, VirtAddr, find_leaf, walk_and_map,
    walk_and_map_pages,
};
use mm::VmFlags;

use crate::loongarch64::paging::LoongArch64Paging;
use crate::loongarch64::specific::{CSR_ASID_ASID_MASK, phys_to_virt};

const HARDWARE_ASID_COUNT: usize = CSR_ASID_ASID_MASK + 1;
const FALLBACK_HARDWARE_ASID: usize = CSR_ASID_ASID_MASK;
const EXCLUSIVE_ASID_COUNT: usize = FALLBACK_HARDWARE_ASID - 1;
const ASID_WORD_BITS: usize = usize::BITS as usize;
const ASID_WORDS: usize = (HARDWARE_ASID_COUNT + ASID_WORD_BITS - 1) / ASID_WORD_BITS;

/// 0 留给内核，最后一个硬件 ASID 留给耗尽时的共享 fallback。
static ALLOCATED_HARDWARE_ASIDS: [AtomicUsize; ASID_WORDS] =
    [const { AtomicUsize::new(0) }; ASID_WORDS];
static HARDWARE_ASID_GENERATIONS: [AtomicUsize; HARDWARE_ASID_COUNT] =
    [const { AtomicUsize::new(0) }; HARDWARE_ASID_COUNT];
static NEXT_ASID_HINT: AtomicUsize = AtomicUsize::new(0);
static NEXT_FALLBACK_GENERATION: AtomicUsize = AtomicUsize::new(0);

fn advance_nonzero(counter: &AtomicUsize) -> usize {
    let mut current = counter.load(Ordering::SeqCst);
    loop {
        let next = current
            .checked_add(1)
            .expect("[arch][mm] ASID/TLB generation exhausted");
        match counter.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

const fn logical_asid(generation: usize, hardware_asid: usize) -> usize {
    assert!(
        generation <= (usize::MAX >> 10),
        "[arch][mm] logical ASID generation exhausted"
    );
    (generation << 10) | hardware_asid
}

fn allocate_user_asid() -> (usize, bool) {
    let start = NEXT_ASID_HINT.fetch_add(1, Ordering::Relaxed) % EXCLUSIVE_ASID_COUNT;
    for offset in 0..EXCLUSIVE_ASID_COUNT {
        let hardware_asid = 1 + (start + offset) % EXCLUSIVE_ASID_COUNT;
        let word = hardware_asid / ASID_WORD_BITS;
        let mask = 1usize << (hardware_asid % ASID_WORD_BITS);
        let mut allocated = ALLOCATED_HARDWARE_ASIDS[word].load(Ordering::Relaxed);
        loop {
            if allocated & mask != 0 {
                break;
            }
            match ALLOCATED_HARDWARE_ASIDS[word].compare_exchange_weak(
                allocated,
                allocated | mask,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let generation = advance_nonzero(&HARDWARE_ASID_GENERATIONS[hardware_asid]);
                    return (logical_asid(generation, hardware_asid), true);
                }
                Err(observed) => allocated = observed,
            }
        }
    }

    let generation = advance_nonzero(&NEXT_FALLBACK_GENERATION);
    (logical_asid(generation, FALLBACK_HARDWARE_ASID), false)
}

fn release_user_asid(asid: usize, exclusive: bool) {
    if !exclusive {
        return;
    }
    let hardware_asid = asid & CSR_ASID_ASID_MASK;
    let word = hardware_asid / ASID_WORD_BITS;
    let mask = 1usize << (hardware_asid % ASID_WORD_BITS);
    let old = ALLOCATED_HARDWARE_ASIDS[word].fetch_and(!mask, Ordering::Release);
    debug_assert_ne!(old & mask, 0, "[arch][mm] releasing an unallocated ASID");
}

const fn activation_requires_full_flush(
    exclusive: bool,
    observed_generation: usize,
    current_generation: usize,
) -> bool {
    !exclusive || observed_generation != current_generation
}

const _: () = {
    assert!(HARDWARE_ASID_COUNT == 1024);
    assert!(FALLBACK_HARDWARE_ASID == 1023);
    assert!(activation_requires_full_flush(true, 0, 1));
    assert!(!activation_requires_full_flush(true, 7, 7));
    assert!(activation_requires_full_flush(false, 7, 7));
};

/// arch 私有的 PGD 描述符。general 看到的只是它的 `NonNull<()>`，不解释字段。
struct UserPgdInner {
    pgd_phys: usize,
    pgd_virt: usize,
    asid: usize,
    exclusive_asid: bool,
    /// 现有映射被替换、撤销或收紧时递增。切换方以此发现扫描开始前已经切离、
    /// 因而没有收到 shootdown 的更新。
    tlb_generation: AtomicUsize,
    /// 每 CPU 最近完整同步到的本地址空间 TLB 代际。
    cpu_tlb_generations: [AtomicUsize; sched::NR_CPUS],
    /// 曾经激活过本地址空间的逻辑 CPU。位图在 PGD 生命周期内单调增长，确保
    /// 已经缓存过该 ASID translation 的 CPU 不会被后续 shootdown 遗漏。
    active_cpus: AtomicUsize,
}

impl UserPgdInner {
    fn new() -> Option<Box<Self>> {
        let request =
            allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
        let pgd_alloc = allocator::KERNEL_ALLOCATOR
            .allocate_physical(request)
            .ok()?;
        let pgd_phys = pgd_alloc.paddr;
        let pgd_virt = phys_to_virt(pgd_phys);
        // Safety: 刚分配的物理页对应内核直映窗口，本次唯一写入者。
        unsafe { core::ptr::write_bytes(pgd_virt as *mut u8, 0, allocator::PAGE_SIZE) };

        let (asid, exclusive_asid) = allocate_user_asid();
        Some(Box::new(Self {
            pgd_phys,
            pgd_virt,
            asid,
            exclusive_asid,
            tlb_generation: AtomicUsize::new(1),
            cpu_tlb_generations: [const { AtomicUsize::new(0) }; sched::NR_CPUS],
            active_cpus: AtomicUsize::new(0),
        }))
    }

    fn pgd_phys(&self) -> usize {
        self.pgd_phys
    }

    fn pgd_virt(&self) -> usize {
        self.pgd_virt
    }

    fn asid(&self) -> usize {
        self.asid
    }
}

impl Drop for UserPgdInner {
    fn drop(&mut self) {
        // VmSpace 的最后一个 Arc 只会在任务已切离或 exec 已激活新根后析构；
        // 因此释放独占 slot 时没有 CPU 仍能用本 logical ASID 访问旧 PGDL。
        // 各 CPU 上遗留的硬件 TLB 项由下一个 slot owner 的首次激活全刷清除。
        free_user_page_table_pages(self.pgd_virt);
        free_page_table_page(self.pgd_phys);
        release_user_asid(self.asid, self.exclusive_asid);
    }
}

fn free_page_table_page(paddr: usize) {
    if let Err(err) = allocator::KERNEL_ALLOCATOR.try_free_physical_addr(paddr) {
        log::error!(
            "[arch][mm] failed to free tracked page-table page paddr={:#x}: {:?}",
            paddr,
            err
        );
    }
}

fn free_user_page_table_pages(root_vaddr: usize) {
    let entries = LoongArch64Paging::ENTRIES_PER_TABLE / 2;
    free_table_entries(root_vaddr, 0, entries);
}

fn free_table_entries(table_vaddr: usize, level: usize, entries: usize) {
    for i in 0..entries {
        let pte_ptr = (table_vaddr + i * core::mem::size_of::<usize>()) as *mut usize;
        let bits = unsafe { core::ptr::read_volatile(pte_ptr) };
        let pte = LoongArch64Paging::pte_from_usize(bits);
        if !LoongArch64Paging::pte_is_valid(pte) || LoongArch64Paging::pte_is_leaf(pte) {
            continue;
        }
        if level + 1 >= LoongArch64Paging::LEVELS {
            continue;
        }
        let child_paddr = LoongArch64Paging::pte_addr(pte);
        let child_vaddr = phys_to_virt(child_paddr);
        free_table_entries(child_vaddr, level + 1, LoongArch64Paging::ENTRIES_PER_TABLE);
        unsafe {
            core::ptr::write_volatile(
                pte_ptr,
                LoongArch64Paging::pte_to_usize(LoongArch64Paging::invalid_pte()),
            )
        };
        free_page_table_page(child_paddr);
    }
}

fn allocate_page_table_page() -> Result<usize, general::MapError> {
    let request = allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map_err(|_| general::MapError::OutOfMemory)?;
    Ok(allocation.paddr)
}

fn flush_user_tlb_range(asid: usize, target_cpus: usize, vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    let page_size = LoongArch64Paging::PAGE_SIZE;
    let Some(end) = vaddr.checked_add(len) else {
        // Safety: 动态目标包含仍可能在下一次完整激活失效前执行该 ASID 的 CPU。
        unsafe { LoongArch64Paging::flush_tlb_with_asid_on_cpus(asid, None, target_cpus) };
        return;
    };
    let aligned_start = vaddr & !(page_size - 1);
    let pages = end.saturating_sub(aligned_start).div_ceil(page_size);
    if pages == 1 {
        // Safety: 动态目标包含仍可能在下一次完整激活失效前执行该 ASID 的 CPU。
        unsafe {
            LoongArch64Paging::flush_tlb_with_asid_on_cpus(
                asid,
                Some(VirtAddr::new(aligned_start)),
                target_cpus,
            )
        };
    } else {
        // 远端 LoongArch IPI 没有携带地址范围，接收端本来就执行完整失效。多页
        // 范围只发布一次完整请求，避免把一次 munmap 放大为最多 64 轮全核 IPI。
        // Safety: 动态目标包含仍可能在下一次完整激活失效前执行该 ASID 的 CPU。
        unsafe { LoongArch64Paging::flush_tlb_with_asid_on_cpus(asid, None, target_cpus) };
    }
}

// ── PgdHandle 与 UserPgdInner 的来回 ─────────────────────────────────────────

fn handle_from_inner(boxed: Box<UserPgdInner>) -> PgdHandle {
    let raw = Box::into_raw(boxed) as *mut ();
    // Safety: Box::into_raw 返回非空指针。
    let nn = unsafe { NonNull::new_unchecked(raw) };
    PgdHandle::from_raw(nn)
}

/// # Safety
/// `handle` 必须是 [`handle_from_inner`] 之前返回的；调用后 handle 不可再用。
unsafe fn inner_from_handle_drop(handle: PgdHandle) {
    let raw = handle.as_raw().as_ptr() as *mut UserPgdInner;
    // Safety: 由调用方保证。Box::from_raw 接管所有权并在离作用域时 Drop。
    let _ = unsafe { Box::from_raw(raw) };
}

/// # Safety
/// `handle` 必须是合法的（未释放的）handle。返回的引用生命周期不超过本调用。
unsafe fn inner_ref<'a>(handle: PgdHandle) -> &'a UserPgdInner {
    let raw = handle.as_raw().as_ptr() as *const UserPgdInner;
    // Safety: 由调用方保证 handle 仍合法；general 一侧 VmSpace 在 Drop 之前
    //         不会再调用任何方法。
    unsafe { &*raw }
}

// ── UserPgdOps 函数实现（裸函数指针填进 static） ─────────────────────────────

fn new_pgd_for_user() -> PgdHandle {
    let inner = UserPgdInner::new().expect("[arch][mm] user PGD allocation failed");
    handle_from_inner(inner)
}

unsafe fn drop_pgd(handle: PgdHandle) {
    // Safety: 由 UserPgdOps 契约保证。
    unsafe { inner_from_handle_drop(handle) };
}

unsafe fn map(
    handle: PgdHandle,
    vaddr: usize,
    paddr: usize,
    flags: VmFlags,
) -> Result<(), general::MapError> {
    // Safety: 由 UserPgdOps 契约保证 handle 合法；vaddr / paddr 在用户半空间且 4K 对齐。
    let inner = unsafe { inner_ref(handle) };
    let read = flags.has(VmFlags::READ);
    let write = flags.has(VmFlags::WRITE);
    let exec = flags.has(VmFlags::EXEC);
    walk_and_map::<LoongArch64Paging>(
        inner.pgd_virt(),
        vaddr,
        paddr,
        LoongArch64Paging::LEVELS - 1,
        read,
        write,
        exec,
        true,
        false,
        phys_to_virt,
        allocate_page_table_page,
    )
}

unsafe fn map_pages(
    handle: PgdHandle,
    vaddr: usize,
    paddrs: &[usize],
    flags: VmFlags,
) -> MapBatchResult {
    // Safety: 由 UserPgdOps 契约保证 handle、地址、权限和空目标 PTE 合法。
    let inner = unsafe { inner_ref(handle) };
    walk_and_map_pages::<LoongArch64Paging>(
        inner.pgd_virt(),
        vaddr,
        paddrs,
        LoongArch64Paging::LEVELS - 1,
        flags.has(VmFlags::READ),
        flags.has(VmFlags::WRITE),
        flags.has(VmFlags::EXEC),
        true,
        false,
        phys_to_virt,
        allocate_page_table_page,
        false, // fresh_range=false: conservative, keep per-table fences
    )
}

unsafe fn publish_new_mapping(handle: PgdHandle, vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    // Safety: 由 UserPgdOps 契约保证 handle 与范围合法。
    let inner = unsafe { inner_ref(handle) };
    let page_size = LoongArch64Paging::PAGE_SIZE;
    let aligned_start = vaddr & !(page_size - 1);
    let targeted = vaddr
        .checked_add(len)
        .is_some_and(|end| end.saturating_sub(aligned_start) <= page_size);
    let address = targeted.then(|| VirtAddr::new(aligned_start));
    // Safety: 调用方保证这些叶 PTE 此前无有效映射；这里只发布写入并收敛本核
    // 可能缓存的无效 translation，不承担旧映射回收同步。
    unsafe { LoongArch64Paging::flush_tlb_local_with_asid(inner.asid(), address) };
}

unsafe fn unmap(handle: PgdHandle, vaddr: usize, len: usize) {
    use general::unmap_range_entries;
    // Safety: 同上。
    let inner = unsafe { inner_ref(handle) };
    let _ =
        unmap_range_entries::<LoongArch64Paging>(inner.pgd_virt(), vaddr, len, true, phys_to_virt);
}

unsafe fn protect(handle: PgdHandle, vaddr: usize, len: usize, flags: VmFlags) {
    // Safety: 同 map。
    let inner = unsafe { inner_ref(handle) };
    let read = flags.has(VmFlags::READ);
    let write = flags.has(VmFlags::WRITE);
    let exec = flags.has(VmFlags::EXEC);
    let user = flags.has(VmFlags::USER);
    let mut va = vaddr & !(LoongArch64Paging::PAGE_SIZE - 1);
    let end = vaddr.saturating_add(len);
    while va < end {
        if let Ok((level, pte_ptr, old_pte)) =
            find_leaf::<LoongArch64Paging>(inner.pgd_virt(), va, phys_to_virt)
        {
            let old_flags = LoongArch64Paging::pte_flags(old_pte);
            let new_pte = LoongArch64Paging::make_leaf_pte_for_level(
                level,
                LoongArch64Paging::pte_addr(old_pte),
                read,
                write,
                exec,
                user,
                LoongArch64Paging::flags_global(old_flags),
            )
            .expect("[arch][mm] protect: invalid leaf permission");
            unsafe { core::ptr::write_volatile(pte_ptr, LoongArch64Paging::pte_to_usize(new_pte)) };
        }
        va += LoongArch64Paging::PAGE_SIZE;
    }
}

unsafe fn clone_for_fork(src: PgdHandle, dst: PgdHandle, range: core::ops::Range<usize>) {
    let src_inner = unsafe { inner_ref(src) };
    let dst_inner = unsafe { inner_ref(dst) };
    let virt = allocator::KERNEL_ALLOCATOR
        .load_phys_to_virt()
        .expect("[arch][mm] clone_for_fork without phys_to_virt");
    let mut va = range.start & !(LoongArch64Paging::PAGE_SIZE - 1);
    while va < range.end {
        let Ok((level, _pte_ptr, src_pte)) =
            find_leaf::<LoongArch64Paging>(src_inner.pgd_virt(), va, phys_to_virt)
        else {
            va += LoongArch64Paging::PAGE_SIZE;
            continue;
        };
        let page_size = LoongArch64Paging::leaf_page_size(level)
            .expect("[arch][mm] clone_for_fork: unsupported leaf level");
        let src_paddr = LoongArch64Paging::pte_addr(src_pte) + (va & (page_size - 1));
        let new_alloc = allocator::KERNEL_ALLOCATOR
            .allocate_physical(allocator::PhysicalAllocRequest::new(
                allocator::PAGE_SIZE,
                allocator::PAGE_SIZE,
            ))
            .expect("[arch][mm] clone_for_fork: OOM");
        let new_paddr = new_alloc.paddr;
        unsafe {
            core::ptr::copy_nonoverlapping(
                virt(src_paddr) as *const u8,
                virt(new_paddr) as *mut u8,
                LoongArch64Paging::PAGE_SIZE,
            );
        }
        let f = LoongArch64Paging::pte_flags(src_pte);
        if let Err(err) = walk_and_map::<LoongArch64Paging>(
            dst_inner.pgd_virt(),
            va,
            new_paddr,
            LoongArch64Paging::LEVELS - 1,
            LoongArch64Paging::flags_readable(f),
            LoongArch64Paging::flags_writable(f),
            LoongArch64Paging::flags_executable(f),
            LoongArch64Paging::flags_user_accessible(f),
            LoongArch64Paging::flags_global(f),
            phys_to_virt,
            allocate_page_table_page,
        ) {
            let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(new_alloc);
            panic!("[arch][mm] clone_for_fork: dst map failed: {:?}", err);
        }
        va += LoongArch64Paging::PAGE_SIZE;
    }
}

unsafe fn activate(handle: PgdHandle) {
    // Safety: 由 UserPgdOps 契约保证 handle 合法。
    let inner = unsafe { inner_ref(handle) };
    let cpu = crate::loongarch64::trap::LoongArch64MessageInterruptOps::current_cpu_id();
    let cpu_bit = 1usize
        .checked_shl(cpu as u32)
        .expect("[arch][mm] logical CPU exceeds active mask width");
    let kernel_pgd = super::super::heap_vm::KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    assert_ne!(
        kernel_pgd, 0,
        "[arch][mm] user address space activated before kernel PGDH"
    );
    let asid = inner.asid();
    // 历史位和当前逻辑 ASID 都先于代际读取发布。失效方若看到本 ASID 就发送
    // IPI；若扫描早于本次发布，则下面的 SeqCst 代际读取会观察到更新并要求完整
    // 本地失效。两侧与 PTE-write→generation→scan 形成全序闭环。
    inner.active_cpus.fetch_or(cpu_bit, Ordering::SeqCst);
    super::super::smp::publish_current_logical_asid(asid);
    let generation = inner.tlb_generation.load(Ordering::SeqCst);
    let observed_generation = inner.cpu_tlb_generations[cpu].load(Ordering::SeqCst);
    let flush_tlb =
        activation_requires_full_flush(inner.exclusive_asid, observed_generation, generation);
    // Safety: 两个根均在各自生命周期内有效；本函数只在调度器地址空间切换边界调用。
    unsafe {
        LoongArch64Paging::activate_with_asid_roots_cached(
            PhysPageTableRoot::new(inner.pgd_phys()),
            PhysPageTableRoot::new(kernel_pgd),
            asid,
            flush_tlb,
        );
    }
    if flush_tlb {
        inner.cpu_tlb_generations[cpu].store(generation, Ordering::SeqCst);
    }
}

unsafe fn invalidate_range(handle: PgdHandle, vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    // Safety: 同上。
    let inner = unsafe { inner_ref(handle) };
    let asid = inner.asid();
    // PTE 写发生在调用本回调之前。SeqCst 递增先于目标扫描，使并发激活要么
    // 被扫描命中并收到 IPI，要么在进入用户态前观察到新代际并完整本地失效。
    advance_nonzero(&inner.tlb_generation);
    let targets = super::super::smp::shootdown_targets_after_pte_update(&inner.active_cpus, asid);
    flush_user_tlb_range(asid, targets, vaddr, len);
}

unsafe fn count_mapped(handle: PgdHandle, vaddr: usize, len: usize) -> usize {
    let inner = unsafe { inner_ref(handle) };
    let mut va = vaddr & !(LoongArch64Paging::PAGE_SIZE - 1);
    let end = vaddr.saturating_add(len);
    let mut count = 0usize;
    while va < end {
        if find_leaf::<LoongArch64Paging>(inner.pgd_virt(), va, phys_to_virt).is_ok() {
            count += 1;
        }
        va += LoongArch64Paging::PAGE_SIZE;
    }
    count
}

/// 注入到 general 的 vtable。
pub(super) static USER_PGD_OPS: UserPgdOps = UserPgdOps {
    new_pgd_for_user,
    drop_pgd,
    map,
    map_pages,
    publish_new_mapping,
    unmap,
    protect,
    clone_for_fork,
    activate,
    invalidate_range,
    count_mapped,
};
