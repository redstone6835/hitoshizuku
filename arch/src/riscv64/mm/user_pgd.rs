//! RISC-V64 用户态页表能力 → 注入到 [`general::mm::UserPgdOps`]。
//!
//! **唯一对外符号**是 `static USER_PGD_OPS`，由 `arch::riscv64::mm::register`
//! 在启动期注入到 general 一侧。
//!
//! 内部 `UserPgdInner` 是 arch 私有结构，由 `Box::leak` 后用 `NonNull<()>`
//! 套进 [`PgdHandle`] 传给上层；`drop_pgd` 反向还原。
//!
//! 物理页表分配 / 释放走 [`allocator::KERNEL_ALLOCATOR`]；映射走泛型
//! `general::page_walk::walk_and_map::<Riscv64Paging>`。

use alloc::boxed::Box;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use general::mm::{PgdHandle, UserPgdOps};
use general::{
    MapBatchResult, PagingArch, PhysPageTableRoot, VirtAddr, find_leaf, unmap_range_entries,
    walk_and_map, walk_and_map_pages,
};
use mm::VmFlags;

use crate::riscv64::paging::Riscv64Paging;
use crate::riscv64::specific::phys_to_virt;
use spin::Mutex;

const SATP_ASID_SHIFT: usize = 44;
const SATP_ASID_MASK: usize = 0xffff;
const ASID_TAG_BITS: usize = 16;

/// 硬件实际实现的 ASID 掩码。0 表示没有 ASID 支持。
static MAX_ASID: AtomicUsize = AtomicUsize::new(0xFFFF);

#[derive(Clone, Copy)]
struct AsidAllocator {
    next: usize,
    generation: usize,
}

static ASID_ALLOCATOR: Mutex<AsidAllocator> = Mutex::new(AsidAllocator {
    next: 1,
    generation: 1,
});
static CURRENT_ASID_GENERATION: AtomicUsize = AtomicUsize::new(1);

#[inline]
const fn make_asid_tag(generation: usize, asid: usize) -> usize {
    (generation << ASID_TAG_BITS) | asid
}

#[inline]
const fn tag_asid(tag: usize) -> usize {
    tag & SATP_ASID_MASK
}

#[inline]
const fn tag_generation(tag: usize) -> usize {
    tag >> ASID_TAG_BITS
}

/// 探测 satp.ASID 的 WARL 位宽，并初始化全局 generation allocator。
///
/// 临时写入全 1 ASID 后立即恢复原 satp；调用发生在第一个用户地址空间创建前。
pub(super) fn init_asid_allocator() {
    let old_satp = crate::read_csr!(satp);
    let probe_satp = old_satp | (SATP_ASID_MASK << SATP_ASID_SHIFT);
    crate::write_csr!(satp, probe_satp);
    let implemented = (crate::read_csr!(satp) >> SATP_ASID_SHIFT) & SATP_ASID_MASK;
    crate::write_csr!(satp, old_satp);
    unsafe { Riscv64Paging::flush_tlb_global(None) };

    MAX_ASID.store(implemented, Ordering::Release);
    let mut allocator = ASID_ALLOCATOR.lock();
    allocator.next = 1;
    allocator.generation = 1;
    CURRENT_ASID_GENERATION.store(1, Ordering::Release);
    log::info!(
        "[arch][mm] satp ASID probe: mask={:#x} bits={}",
        implemented,
        implemented.count_ones()
    );
}

/// 在当前 generation 中分配 ASID。回绕时只做一次全局 flush；旧地址空间在
/// 下一次 activate 时 lazy 获取新一代 ASID，因此不会与新地址空间共享 tag。
fn alloc_asid_tag() -> usize {
    let max = MAX_ASID.load(Ordering::Acquire);
    if max == 0 {
        return 0;
    }

    let mut allocator = ASID_ALLOCATOR.lock();
    if allocator.next > max {
        allocator.generation = allocator.generation.wrapping_add(1).max(1);
        allocator.next = 1;
        unsafe { Riscv64Paging::flush_tlb_global(None) };
        CURRENT_ASID_GENERATION.store(allocator.generation, Ordering::Release);
    }

    let asid = allocator.next;
    allocator.next += 1;
    make_asid_tag(allocator.generation, asid)
}

struct UserPgdInner {
    pgd_phys: usize,
    pgd_virt: usize,
    asid_tag: AtomicUsize,
    /// 曾经激活过本地址空间的逻辑 CPU。位图在 PGD 生命周期内单调增长，
    /// 保证已经缓存过该 ASID translation 的 hart 不会被后续 shootdown 遗漏。
    active_cpus: AtomicUsize,
    needs_page_table_fence: AtomicBool,
    needs_asid_fence: AtomicBool,
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
        unsafe {
            crate::riscv64::specific::zero_memory_fast(pgd_virt, allocator::PAGE_SIZE);
        }

        let cur_satp: usize = read_csr!(satp);
        let cur_root_ppn = cur_satp & 0xFFF_FFFF_FFFF;
        let cur_root_phys = cur_root_ppn << 12;
        let cur_root_virt = phys_to_virt(cur_root_phys);

        // 只复制高半区（内核空间）entries 到用户页表
        // 低半区（用户空间，entries 0~255）保持全零，由 ELF loader 按需映射
        let entries = Riscv64Paging::ENTRIES_PER_TABLE;
        let half = entries / 2;
        unsafe {
            let src = cur_root_virt as *const usize;
            let dst = pgd_virt as *mut usize;
            for i in half..entries {
                let entry = core::ptr::read_volatile(src.add(i));
                core::ptr::write_volatile(dst.add(i), entry);
            }
        }

        let asid_tag = alloc_asid_tag();
        Some(Box::new(Self {
            pgd_phys,
            pgd_virt,
            asid_tag: AtomicUsize::new(asid_tag),
            active_cpus: AtomicUsize::new(0),
            needs_page_table_fence: AtomicBool::new(true),
            needs_asid_fence: AtomicBool::new(false),
        }))
    }

    fn pgd_phys(&self) -> usize {
        self.pgd_phys
    }

    fn pgd_virt(&self) -> usize {
        self.pgd_virt
    }

    fn asid(&self) -> usize {
        let tag = self.asid_tag.load(Ordering::Acquire);
        if tag == 0 {
            return 0;
        }

        let generation = CURRENT_ASID_GENERATION.load(Ordering::Acquire);
        if tag_generation(tag) == generation {
            return tag_asid(tag);
        }

        let new_tag = alloc_asid_tag();
        self.asid_tag.store(new_tag, Ordering::Release);
        self.needs_asid_fence.store(true, Ordering::Release);
        tag_asid(new_tag)
    }
}

impl Drop for UserPgdInner {
    fn drop(&mut self) {
        free_user_page_table_pages(self.pgd_virt);
        free_page_table_page(self.pgd_phys);
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
    let entries = Riscv64Paging::ENTRIES_PER_TABLE / 2;
    free_table_entries(root_vaddr, 0, entries);
}

fn free_table_entries(table_vaddr: usize, level: usize, entries: usize) {
    for i in 0..entries {
        let pte_ptr = (table_vaddr + i * core::mem::size_of::<usize>()) as *mut usize;
        let bits = unsafe { core::ptr::read_volatile(pte_ptr) };
        let pte = Riscv64Paging::pte_from_usize(bits);
        if !Riscv64Paging::pte_is_valid(pte) || Riscv64Paging::pte_is_leaf(pte) {
            continue;
        }
        if level + 1 >= Riscv64Paging::LEVELS {
            continue;
        }
        let child_paddr = Riscv64Paging::pte_addr(pte);
        let child_vaddr = phys_to_virt(child_paddr);
        free_table_entries(child_vaddr, level + 1, Riscv64Paging::ENTRIES_PER_TABLE);
        unsafe {
            core::ptr::write_volatile(
                pte_ptr,
                Riscv64Paging::pte_to_usize(Riscv64Paging::invalid_pte()),
            )
        };
        free_page_table_page(child_paddr);
    }
}

fn new_pgd_for_user() -> PgdHandle {
    let inner = UserPgdInner::new().expect("[arch][mm] user PGD allocation failed");
    let raw = Box::into_raw(inner) as *mut ();
    let nn = unsafe { NonNull::new_unchecked(raw) };
    PgdHandle::from_raw(nn)
}

unsafe fn inner_ref<'a>(handle: PgdHandle) -> &'a UserPgdInner {
    let raw = handle.as_raw().as_ptr() as *const UserPgdInner;
    unsafe { &*raw }
}

unsafe fn drop_pgd(pgd: PgdHandle) {
    let raw = pgd.as_raw().as_ptr() as *mut UserPgdInner;
    let _ = unsafe { Box::from_raw(raw) };
}

unsafe fn activate(pgd: PgdHandle) {
    let inner = unsafe { inner_ref(pgd) };
    let asid = inner.asid();
    let cpu = crate::riscv64::specific::current_cpu_id();
    let cpu_bit = 1usize
        .checked_shl(cpu as u32)
        .expect("[arch][mm] logical CPU exceeds active mask width");
    let first_activation = inner.active_cpus.fetch_or(cpu_bit, Ordering::AcqRel) & cpu_bit == 0;
    let needs_page_table_fence = inner.needs_page_table_fence.swap(false, Ordering::AcqRel);
    let needs_asid_fence = inner.needs_asid_fence.swap(false, Ordering::AcqRel);
    unsafe {
        Riscv64Paging::activate_with_asid(
            PhysPageTableRoot::new(inner.pgd_phys()),
            asid,
            first_activation || needs_page_table_fence || needs_asid_fence,
        );
    }
}

unsafe fn activate_kernel() {
    let root = crate::riscv64::heap_vm::kernel_page_table_root();
    unsafe {
        // ASID 0 专用于内核页表；activate_with_asid 会在切根后刷新
        // 本地 ASID 0，避免 idle 沿用已经回收的用户 PGD。
        Riscv64Paging::activate_with_asid(PhysPageTableRoot::new(root), 0, false);
    }
}

fn allocate_page_table_page() -> Result<usize, general::MapError> {
    let request = allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
    allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map(|a| a.paddr)
        .map_err(|_| general::MapError::OutOfMemory)
}

fn flush_user_tlb_range(asid: usize, active_cpus: usize, vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    let page_size = Riscv64Paging::PAGE_SIZE;
    const PAGE_THRESHOLD: usize = 64;
    let Some(end) = vaddr.checked_add(len) else {
        unsafe { Riscv64Paging::flush_tlb_with_asid_on_cpus(asid, None, active_cpus) };
        return;
    };
    let aligned_start = vaddr & !(page_size - 1);
    let pages = end.saturating_sub(aligned_start).div_ceil(page_size);
    if pages > PAGE_THRESHOLD {
        unsafe { Riscv64Paging::flush_tlb_with_asid_on_cpus(asid, None, active_cpus) };
        return;
    }
    let Some(range_size) = pages.checked_mul(page_size) else {
        unsafe { Riscv64Paging::flush_tlb_with_asid_on_cpus(asid, None, active_cpus) };
        return;
    };
    if aligned_start.checked_add(range_size).is_none() {
        unsafe { Riscv64Paging::flush_tlb_with_asid_on_cpus(asid, None, active_cpus) };
        return;
    }

    // 本地 sfence.vma 只能按地址或整个 ASID 失效，因此仍按页执行。SBI RFENCE
    // 原生接受连续范围，远端只需一次 M-mode 往返，避免把常见的多页 mprotect
    // 和 munmap 放大为每页一次同步调用。
    let current = crate::riscv64::specific::current_cpu_id();
    let flush_local = current < usize::BITS as usize && active_cpus & (1usize << current) != 0;
    let mut va = aligned_start;
    while va < end {
        if flush_local {
            unsafe { Riscv64Paging::flush_tlb_local_with_asid(asid, Some(VirtAddr::new(va))) };
        }
        let Some(next) = va.checked_add(page_size) else {
            unsafe { Riscv64Paging::flush_tlb_with_asid_on_cpus(asid, None, active_cpus) };
            return;
        };
        va = next;
    }
    crate::riscv64::smp::remote_sfence_vma_range_on(
        active_cpus,
        Some(asid),
        aligned_start,
        range_size,
    );
}

unsafe fn map_user_pages(
    pgd: PgdHandle,
    vaddr: usize,
    paddr: usize,
    flags: VmFlags,
) -> Result<(), general::MapError> {
    let inner = unsafe { inner_ref(pgd) };
    let root_virt = inner.pgd_virt();
    let read = flags.has(VmFlags::READ);
    let write = flags.has(VmFlags::WRITE);
    let execute = flags.has(VmFlags::EXEC);
    let user = flags.has(VmFlags::USER);
    let result = walk_and_map::<Riscv64Paging>(
        root_virt,
        vaddr,
        paddr,
        Riscv64Paging::LEVELS - 1,
        read,
        write,
        execute,
        user,
        false,
        phys_to_virt,
        allocate_page_table_page,
    );
    // 即使中途 OOM，walk 也可能已经发布新的中间页表；首次激活仍须 fence。
    inner.needs_page_table_fence.store(true, Ordering::Release);
    result
}

unsafe fn map_user_page_batch(
    pgd: PgdHandle,
    vaddr: usize,
    paddrs: &[usize],
    flags: VmFlags,
) -> MapBatchResult {
    // Safety: 由 UserPgdOps 契约保证 handle、地址、权限和空目标 PTE 合法。
    let inner = unsafe { inner_ref(pgd) };
    let result = walk_and_map_pages::<Riscv64Paging>(
        inner.pgd_virt(),
        vaddr,
        paddrs,
        Riscv64Paging::LEVELS - 1,
        flags.has(VmFlags::READ),
        flags.has(VmFlags::WRITE),
        flags.has(VmFlags::EXEC),
        flags.has(VmFlags::USER),
        false,
        phys_to_virt,
        allocate_page_table_page,
        true, // fresh_range=true: pages are freshly allocated, skip intermediate fences
    );
    // 批次即使只安装了前缀，也可能发布新的中间页表；首次激活仍需 fence。
    inner.needs_page_table_fence.store(true, Ordering::Release);
    result
}

unsafe fn publish_new_mapping(pgd: PgdHandle, vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    // Safety: 由 UserPgdOps 契约保证 handle 与范围合法。
    let inner = unsafe { inner_ref(pgd) };
    let page_size = Riscv64Paging::PAGE_SIZE;
    let aligned_start = vaddr & !(page_size - 1);
    let targeted = vaddr
        .checked_add(len)
        .is_some_and(|end| end.saturating_sub(aligned_start) <= page_size);
    let address = targeted.then(|| VirtAddr::new(aligned_start));
    // Safety: sfence.vma 同时排序先前 PTE store 并清除本 hart 的旧无效状态；
    // needs_page_table_fence 保留为 true，使以后首次激活该 PGD 的其它 hart 仍会 fence。
    unsafe { Riscv64Paging::flush_tlb_local_with_asid(inner.asid(), address) };
}

unsafe fn unmap_user_pages(pgd: PgdHandle, vaddr: usize, len: usize) {
    let inner = unsafe { inner_ref(pgd) };
    let _ = unmap_range_entries::<Riscv64Paging>(inner.pgd_virt(), vaddr, len, true, phys_to_virt);
    inner.needs_page_table_fence.store(true, Ordering::Release);
}

unsafe fn protect_user_pages(pgd: PgdHandle, vaddr: usize, len: usize, flags: VmFlags) {
    let inner = unsafe { inner_ref(pgd) };
    let read = flags.has(VmFlags::READ);
    let write = flags.has(VmFlags::WRITE);
    let exec = flags.has(VmFlags::EXEC);
    let user = flags.has(VmFlags::USER);
    let mut va = vaddr & !(Riscv64Paging::PAGE_SIZE - 1);
    let end = vaddr.saturating_add(len);
    let base_level = Riscv64Paging::LEVELS - 1;
    let mut leaf_table_vaddr = 0usize;
    let mut leaf_table_end = va;
    while va < end {
        let cached = if leaf_table_vaddr != 0 && va < leaf_table_end {
            let index = Riscv64Paging::level_index(va, base_level);
            let pte_ptr = (leaf_table_vaddr + index * core::mem::size_of::<usize>()) as *mut usize;
            let old_pte =
                Riscv64Paging::pte_from_usize(unsafe { core::ptr::read_volatile(pte_ptr) });
            (Riscv64Paging::pte_is_valid(old_pte) && Riscv64Paging::pte_is_leaf(old_pte))
                .then_some((base_level, pte_ptr, old_pte))
        } else {
            None
        };
        if let Ok((level, pte_ptr, old_pte)) = cached.ok_or(()).or_else(|_| {
            let found =
                find_leaf::<Riscv64Paging>(inner.pgd_virt(), va, phys_to_virt).map_err(|_| ())?;
            if found.0 == base_level {
                let index = Riscv64Paging::level_index(va, base_level);
                leaf_table_vaddr = found.1 as usize - index * core::mem::size_of::<usize>();
                let entries_left = Riscv64Paging::ENTRIES_PER_TABLE - index;
                leaf_table_end = va
                    .saturating_add(entries_left * Riscv64Paging::PAGE_SIZE)
                    .min(end);
            } else {
                leaf_table_vaddr = 0;
                leaf_table_end = va;
            }
            Ok::<_, ()>(found)
        }) {
            let old_flags = Riscv64Paging::pte_flags(old_pte);
            let new_pte = Riscv64Paging::make_leaf_pte_for_level(
                level,
                Riscv64Paging::pte_addr(old_pte),
                read,
                write,
                exec,
                user,
                Riscv64Paging::flags_global(old_flags),
            )
            .expect("[arch][mm] protect_user_pages: invalid leaf permission");
            unsafe { core::ptr::write_volatile(pte_ptr, Riscv64Paging::pte_to_usize(new_pte)) };
        }
        va += Riscv64Paging::PAGE_SIZE;
    }
    inner.needs_page_table_fence.store(true, Ordering::Release);
}

unsafe fn clone_for_fork_user_pages(
    src: PgdHandle,
    dst: PgdHandle,
    range: core::ops::Range<usize>,
) {
    let src_inner = unsafe { inner_ref(src) };
    let dst_inner = unsafe { inner_ref(dst) };
    let mut va = range.start & !(Riscv64Paging::PAGE_SIZE - 1);
    while va < range.end {
        let Ok((level, _, src_pte)) =
            find_leaf::<Riscv64Paging>(src_inner.pgd_virt(), va, phys_to_virt)
        else {
            va += Riscv64Paging::PAGE_SIZE;
            continue;
        };
        let page_size = Riscv64Paging::leaf_page_size(level)
            .expect("[arch][mm] clone_for_fork_user_pages: unsupported leaf level");
        let src_paddr = Riscv64Paging::pte_addr(src_pte) + (va & (page_size - 1));
        let request =
            allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
        let new_alloc = match allocator::KERNEL_ALLOCATOR.allocate_physical(request) {
            Ok(a) => a,
            Err(_) => panic!("[arch][mm] clone_for_fork_user_pages: OOM"),
        };
        let new_paddr = new_alloc.paddr;
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_to_virt(src_paddr) as *const u8,
                phys_to_virt(new_paddr) as *mut u8,
                Riscv64Paging::PAGE_SIZE,
            );
        }
        let f = Riscv64Paging::pte_flags(src_pte);
        if let Err(err) = walk_and_map::<Riscv64Paging>(
            dst_inner.pgd_virt(),
            va,
            new_paddr,
            Riscv64Paging::LEVELS - 1,
            Riscv64Paging::flags_readable(f),
            Riscv64Paging::flags_writable(f),
            Riscv64Paging::flags_executable(f),
            Riscv64Paging::flags_user_accessible(f),
            Riscv64Paging::flags_global(f),
            phys_to_virt,
            allocate_page_table_page,
        ) {
            let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(new_alloc);
            panic!(
                "[arch][mm] clone_for_fork_user_pages: dst map failed: {:?}",
                err
            );
        }
        va += Riscv64Paging::PAGE_SIZE;
    }
    dst_inner
        .needs_page_table_fence
        .store(true, Ordering::Release);
}

unsafe fn invalidate_range(pgd: PgdHandle, vaddr: usize, len: usize) {
    let inner = unsafe { inner_ref(pgd) };
    let active_cpus = inner.active_cpus.load(Ordering::Acquire);
    flush_user_tlb_range(inner.asid(), active_cpus, vaddr, len);
    // 本次定向 fence 已覆盖相应页表修改，但不能消费新一代 ASID 的首次激活
    // 标记；后者要求 activate() 在安装该 ASID 时完成一次完整 ASID fence。
    inner.needs_page_table_fence.store(false, Ordering::Release);
}

unsafe fn count_mapped_pages(pgd: PgdHandle, vaddr: usize, len: usize) -> usize {
    let inner = unsafe { inner_ref(pgd) };
    let mut va = vaddr & !(Riscv64Paging::PAGE_SIZE - 1);
    let end = vaddr.saturating_add(len);
    let mut count = 0usize;
    while va < end {
        if find_leaf::<Riscv64Paging>(inner.pgd_virt(), va, phys_to_virt).is_ok() {
            count += 1;
        }
        va += Riscv64Paging::PAGE_SIZE;
    }
    count
}

unsafe fn zero_user_pages(vaddr: usize, len: usize) {
    // Safety: UserPgdOps 契约保证 direct-map 范围独占、可写且按页对齐。
    unsafe { crate::riscv64::specific::zero_memory_fast(vaddr, len) };
}

pub(super) static USER_PGD_OPS: UserPgdOps = UserPgdOps {
    new_pgd_for_user,
    drop_pgd,
    zero_user_pages,
    map: map_user_pages,
    map_pages: map_user_page_batch,
    publish_new_mapping,
    unmap: unmap_user_pages,
    protect: protect_user_pages,
    clone_for_fork: clone_for_fork_user_pages,
    activate,
    activate_kernel,
    invalidate_range,
    count_mapped: count_mapped_pages,
};
