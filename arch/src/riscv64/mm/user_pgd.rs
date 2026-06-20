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
use core::sync::atomic::{AtomicUsize, Ordering};

use general::mm::{PgdHandle, UserPgdOps};
use general::{
    PagingArch, PhysPageTableRoot, VirtAddr, find_leaf, unmap_range_entries, walk_and_map,
};
use mm::VmFlags;

use crate::riscv64::paging::Riscv64Paging;
use crate::riscv64::specific::phys_to_virt;

static NEXT_ASID: AtomicUsize = AtomicUsize::new(1);
/// ASID 位宽上限（硬件相关，Sv48 通常 16 位 = 65535）。
/// 初始化时通过探测硬件获取实际值。
static MAX_ASID: AtomicUsize = AtomicUsize::new(0xFFFF);

/// 分配下一个 ASID。到达上限时回绕到 1 并 flush 全 TLB。
// TODO(SMP): 多核时需改为 generation-based 方案——回绕时递增 generation，
// 各核在 context switch 时 lazy 重分配，避免全核 IPI + 全 TLB flush。
fn alloc_asid() -> usize {
    loop {
        let cur = NEXT_ASID.load(Ordering::Relaxed);
        let max = MAX_ASID.load(Ordering::Relaxed);
        let next = if cur >= max { 1 } else { cur + 1 };
        if NEXT_ASID
            .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            if cur >= max {
                // 回绕：flush 全 TLB（所有 ASID）
                unsafe {
                    core::arch::asm!("sfence.vma zero, zero");
                }
            }
            return cur;
        }
    }
}

struct UserPgdInner {
    pgd_phys: usize,
    pgd_virt: usize,
    asid: usize,
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

        let asid = alloc_asid();
        Some(Box::new(Self {
            pgd_phys,
            pgd_virt,
            asid,
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

fn pgd_phys(pgd: PgdHandle) -> PhysPageTableRoot {
    let inner = unsafe { inner_ref(pgd) };
    PhysPageTableRoot::new(inner.pgd_phys())
}

unsafe fn activate(pgd: PgdHandle) {
    let inner = unsafe { inner_ref(pgd) };
    unsafe {
        Riscv64Paging::activate_with_asid(PhysPageTableRoot::new(inner.pgd_phys()), inner.asid());
    }
}

fn allocate_page_table_page() -> Result<usize, general::MapError> {
    let request = allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
    allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map(|a| a.paddr)
        .map_err(|_| general::MapError::OutOfMemory)
}

fn flush_user_tlb_range(asid: usize, vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    let page_size = Riscv64Paging::PAGE_SIZE;
    const PAGE_THRESHOLD: usize = 64;
    let Some(end) = vaddr.checked_add(len) else {
        unsafe { Riscv64Paging::flush_tlb_with_asid(asid, None) };
        return;
    };
    let aligned_start = vaddr & !(page_size - 1);
    let pages = end.saturating_sub(aligned_start).div_ceil(page_size);
    if pages > PAGE_THRESHOLD {
        unsafe { Riscv64Paging::flush_tlb_with_asid(asid, None) };
        return;
    }
    let mut va = aligned_start;
    while va < end {
        unsafe { Riscv64Paging::flush_tlb_with_asid(asid, Some(VirtAddr::new(va))) };
        let Some(next) = va.checked_add(page_size) else {
            unsafe { Riscv64Paging::flush_tlb_with_asid(asid, None) };
            return;
        };
        va = next;
    }
}

unsafe fn map_user_pages(pgd: PgdHandle, vaddr: usize, paddr: usize, flags: VmFlags) {
    let inner = unsafe { inner_ref(pgd) };
    let root_virt = inner.pgd_virt();
    let read = flags.has(VmFlags::READ);
    let write = flags.has(VmFlags::WRITE);
    let execute = flags.has(VmFlags::EXEC);
    let user = flags.has(VmFlags::USER);
    walk_and_map::<Riscv64Paging>(
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
    )
    .expect("[arch][mm] map_user_pages: walk_and_map failed");
}

unsafe fn unmap_user_pages(pgd: PgdHandle, vaddr: usize, len: usize) {
    let inner = unsafe { inner_ref(pgd) };
    let _ = unmap_range_entries::<Riscv64Paging>(inner.pgd_virt(), vaddr, len, true, phys_to_virt);
    flush_user_tlb_range(inner.asid(), vaddr, len);
}

unsafe fn protect_user_pages(pgd: PgdHandle, vaddr: usize, len: usize, flags: VmFlags) {
    let inner = unsafe { inner_ref(pgd) };
    let read = flags.has(VmFlags::READ);
    let write = flags.has(VmFlags::WRITE);
    let exec = flags.has(VmFlags::EXEC);
    let user = flags.has(VmFlags::USER);
    let mut va = vaddr & !(Riscv64Paging::PAGE_SIZE - 1);
    let end = vaddr.saturating_add(len);
    while va < end {
        if let Ok((level, pte_ptr, old_pte)) =
            find_leaf::<Riscv64Paging>(inner.pgd_virt(), va, phys_to_virt)
        {
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
    flush_user_tlb_range(inner.asid(), vaddr, len);
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
}

fn lookup_pgd(pgd: PgdHandle, vaddr: usize) -> Option<usize> {
    let inner = unsafe { inner_ref(pgd) };
    let root_virt = inner.pgd_virt();
    find_leaf::<Riscv64Paging>(root_virt, vaddr, phys_to_virt)
        .map(|(_, _, pte)| Riscv64Paging::pte_addr(pte))
        .ok()
}

#[allow(dead_code)]
fn get_asid(pgd: PgdHandle) -> usize {
    let inner = unsafe { inner_ref(pgd) };
    inner.asid()
}

unsafe fn invalidate_range(pgd: PgdHandle, vaddr: usize, len: usize) {
    let inner = unsafe { inner_ref(pgd) };
    flush_user_tlb_range(inner.asid(), vaddr, len);
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

pub(super) static USER_PGD_OPS: UserPgdOps = UserPgdOps {
    new_pgd_for_user,
    drop_pgd,
    map: map_user_pages,
    unmap: unmap_user_pages,
    protect: protect_user_pages,
    clone_for_fork: clone_for_fork_user_pages,
    activate,
    invalidate_range,
    count_mapped: count_mapped_pages,
};
