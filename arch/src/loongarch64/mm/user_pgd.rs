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
use general::{PagingArch, PhysPageTableRoot, VirtAddr, find_leaf, walk_and_map};
use mm::VmFlags;

use crate::loongarch64::paging::LoongArch64Paging;
use crate::loongarch64::specific::phys_to_virt;

/// LoongArch64 ASID 单调发号；溢出后回到 1（0 留给内核）。
static NEXT_ASID: AtomicUsize = AtomicUsize::new(1);

/// arch 私有的 PGD 描述符。general 看到的只是它的 `NonNull<()>`，不解释字段。
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
        // Safety: 刚分配的物理页对应内核直映窗口，本次唯一写入者。
        unsafe { core::ptr::write_bytes(pgd_virt as *mut u8, 0, allocator::PAGE_SIZE) };

        // 复制内核侧 PGD 项，让用户进程能在 trap 进入内核态时访问内核映射。
        let kernel_pgd = super::super::heap_vm::KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
        if kernel_pgd != 0 {
            copy_kernel_pgd_entries(pgd_virt, kernel_pgd);
        }

        let asid = NEXT_ASID.fetch_add(1, Ordering::Relaxed);
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

fn copy_kernel_pgd_entries(dst_pgd_virt: usize, src_pgd_phys: usize) {
    let src_pgd_virt = phys_to_virt(src_pgd_phys);
    let entries = LoongArch64Paging::ENTRIES_PER_TABLE;
    let src = src_pgd_virt as *const usize;
    let dst = dst_pgd_virt as *mut usize;
    for i in 0..entries {
        // Safety: src/dst 指向 PAGE_SIZE 字节的 PGD 缓冲，按 usize 对齐；
        //         本次只读源、独占写目的，无并发访问者。
        let entry = unsafe { core::ptr::read_volatile(src.add(i)) };
        unsafe { core::ptr::write_volatile(dst.add(i), entry) };
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

unsafe fn map(handle: PgdHandle, vaddr: usize, paddr: usize, flags: VmFlags) {
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
    .expect("[arch][mm] walk_and_map failed");
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
    unsafe { invalidate_range(handle, vaddr, len) };
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
    // Safety: activate_with_asid 写 PGDL/ASID/PWCL/PWCH 与 CRMD，在调度器
    //         切换边界调用。
    unsafe {
        LoongArch64Paging::activate_with_asid(
            PhysPageTableRoot::new(inner.pgd_phys()),
            inner.asid(),
        );
    }
}

unsafe fn invalidate_range(handle: PgdHandle, vaddr: usize, len: usize) {
    // Safety: 同上。
    let inner = unsafe { inner_ref(handle) };
    let mut va = vaddr & !(LoongArch64Paging::PAGE_SIZE - 1);
    let end = vaddr + len;
    while va < end {
        // Safety: flush_tlb_with_asid 不解引用任何指针，仅发 invtlb。
        unsafe {
            LoongArch64Paging::flush_tlb_with_asid(inner.asid(), Some(VirtAddr::new(va)));
        }
        va += LoongArch64Paging::PAGE_SIZE;
    }
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
    unmap,
    protect,
    clone_for_fork,
    activate,
    invalidate_range,
    count_mapped,
};
