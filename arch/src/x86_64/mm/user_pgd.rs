//! x86_64 用户 PGD 生命周期与页表操作。

use alloc::boxed::Box;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use general::mm::{PgdHandle, UserPgdOps};
use general::{
    MapBatchResult, PagingArch, PhysPageTableRoot, VirtAddr, find_leaf, protect_range_entries,
    unmap_range_entries, walk_and_map, walk_and_map_pages,
};
use mm::VmFlags;
use spin::Mutex;

use crate::x86_64::paging::X86_64Paging;
use crate::x86_64::specific::phys_to_virt;

const USER_HALF_ENTRIES: usize = 256;
const CPU_MASK_BITS: usize = usize::BITS as usize;

static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_USER_PGD: [AtomicUsize; sched::NR_CPUS] =
    [const { AtomicUsize::new(0) }; sched::NR_CPUS];

pub(super) fn set_kernel_root(root: usize) {
    if root != 0 {
        assert_eq!(root & (allocator::PAGE_SIZE - 1), 0);
    }
    KERNEL_ROOT.store(root, Ordering::Release);
}

struct UserPgdInner {
    root_phys: usize,
    root_virt: usize,
    active_cpus: AtomicUsize,
    update_lock: Mutex<()>,
}

impl UserPgdInner {
    fn new() -> Option<Box<Self>> {
        let request =
            allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
        let allocation = allocator::KERNEL_ALLOCATOR
            .allocate_physical(request)
            .ok()?;
        let root_phys = allocation.paddr;
        let root_virt = phys_to_virt(root_phys);
        unsafe { core::ptr::write_bytes(root_virt as *mut u8, 0, allocator::PAGE_SIZE) };

        // Copy only the kernel half.  A zero current CR3 is valid during the very
        // early loader phase; the formal kernel page-table initializer publishes
        // KERNEL_ROOT before the first user address space is created.
        // CR3[11:0] carries PCID and CR3[63] is the no-flush hint.  Neither
        // bit belongs to the physical page-table address (Linux's
        // `__read_cr3()` callers apply the same mask before walking it).
        let current_root = crate::x86_64::paging::read_cr3() & !(0xfff | (1usize << 63));
        if current_root != 0 {
            let current_virt = phys_to_virt(current_root);
            unsafe {
                let src = current_virt as *const usize;
                let dst = root_virt as *mut usize;
                for index in USER_HALF_ENTRIES..X86_64Paging::ENTRIES_PER_TABLE {
                    core::ptr::write_volatile(
                        dst.add(index),
                        core::ptr::read_volatile(src.add(index)),
                    );
                }
            }
        } else if let Some(kernel_root) = nonzero_root() {
            let src = phys_to_virt(kernel_root) as *const usize;
            unsafe {
                let dst = root_virt as *mut usize;
                for index in USER_HALF_ENTRIES..X86_64Paging::ENTRIES_PER_TABLE {
                    core::ptr::write_volatile(
                        dst.add(index),
                        core::ptr::read_volatile(src.add(index)),
                    );
                }
            }
        }

        Some(Box::new(Self {
            root_phys,
            root_virt,
            active_cpus: AtomicUsize::new(0),
            update_lock: Mutex::new(()),
        }))
    }
}

fn nonzero_root() -> Option<usize> {
    let root = KERNEL_ROOT.load(Ordering::Acquire) & !(0xfff | (1usize << 63));
    (root != 0).then_some(root)
}

unsafe fn inner<'a>(handle: PgdHandle) -> &'a UserPgdInner {
    unsafe { &*(handle.as_usize() as *const UserPgdInner) }
}

fn allocate_table_page() -> Result<usize, general::MapError> {
    let request = allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
    allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map(|allocation| allocation.paddr)
        .map_err(|_| general::MapError::OutOfMemory)
}

fn free_table_page(paddr: usize) {
    if let Err(error) = allocator::KERNEL_ALLOCATOR.try_free_physical_addr(paddr) {
        log::error!("[x86][mm] failed to release page-table page {paddr:#x}: {error:?}");
    }
}

fn free_user_subtree(table: usize, level: usize) {
    if level >= X86_64Paging::LEVELS - 1 {
        return;
    }
    // Only the top-level PML4 has a user/kernel split.  Every lower-level
    // page-table page has all 512 entries; recursively limiting those pages to
    // 256 leaks half of a subtree and can leave owned table pages dangling.
    let entry_count = if level == 0 {
        USER_HALF_ENTRIES
    } else {
        X86_64Paging::ENTRIES_PER_TABLE
    };
    let entries = unsafe { core::slice::from_raw_parts(table as *const usize, entry_count) };
    for bits in entries.iter().copied() {
        let pte = X86_64Paging::pte_from_usize(bits);
        if !X86_64Paging::pte_is_valid(pte) || X86_64Paging::pte_is_leaf(pte) {
            continue;
        }
        let child = X86_64Paging::pte_addr(pte);
        free_user_subtree(phys_to_virt(child), level + 1);
        free_table_page(child);
    }
}

fn new_pgd_for_user() -> PgdHandle {
    let inner = UserPgdInner::new().expect("[x86][mm] unable to allocate user PGD");
    let raw = Box::into_raw(inner) as *mut ();
    PgdHandle::from_raw(unsafe { NonNull::new_unchecked(raw) })
}

unsafe fn drop_pgd(handle: PgdHandle) {
    let raw = handle.as_usize();
    let object = unsafe { Box::from_raw(raw as *mut UserPgdInner) };
    assert_eq!(
        object.active_cpus.load(Ordering::Acquire),
        0,
        "[x86][mm] user PGD still active"
    );
    free_user_subtree(object.root_virt, 0);
    free_table_page(object.root_phys);
}

unsafe fn map_user(
    handle: PgdHandle,
    vaddr: usize,
    paddr: usize,
    flags: VmFlags,
) -> Result<(), general::MapError> {
    let object = unsafe { inner(handle) };
    let _guard = object.update_lock.lock();
    let result = walk_and_map::<X86_64Paging>(
        object.root_virt,
        vaddr,
        paddr,
        X86_64Paging::LEVELS - 1,
        flags.has(VmFlags::READ),
        flags.has(VmFlags::WRITE),
        flags.has(VmFlags::EXEC),
        flags.has(VmFlags::USER),
        false,
        phys_to_virt,
        allocate_table_page,
    );
    result
}

unsafe fn map_user_pages(
    handle: PgdHandle,
    vaddr: usize,
    paddrs: &[usize],
    flags: VmFlags,
) -> MapBatchResult {
    let object = unsafe { inner(handle) };
    let _guard = object.update_lock.lock();
    walk_and_map_pages::<X86_64Paging>(
        object.root_virt,
        vaddr,
        paddrs,
        X86_64Paging::LEVELS - 1,
        flags.has(VmFlags::READ),
        flags.has(VmFlags::WRITE),
        flags.has(VmFlags::EXEC),
        flags.has(VmFlags::USER),
        false,
        phys_to_virt,
        allocate_table_page,
        true,
    )
}

unsafe fn publish_new_mapping(handle: PgdHandle, vaddr: usize, len: usize) {
    let _ = handle;
    if len == 0 {
        return;
    }
    let start = vaddr & !(X86_64Paging::PAGE_SIZE - 1);
    let end = vaddr.saturating_add(len);
    let mut address = start;
    while address < end {
        unsafe { X86_64Paging::flush_tlb(Some(VirtAddr::new(address))) };
        address = address.saturating_add(X86_64Paging::PAGE_SIZE);
    }
}

unsafe fn unmap_user(handle: PgdHandle, vaddr: usize, len: usize) {
    let object = unsafe { inner(handle) };
    let _guard = object.update_lock.lock();
    let _ = unmap_range_entries::<X86_64Paging>(object.root_virt, vaddr, len, true, phys_to_virt);
}

unsafe fn protect_user(handle: PgdHandle, vaddr: usize, len: usize, flags: VmFlags) {
    let object = unsafe { inner(handle) };
    let _guard = object.update_lock.lock();
    let _ = protect_range_entries::<X86_64Paging>(
        object.root_virt,
        vaddr,
        len,
        flags.has(VmFlags::READ),
        flags.has(VmFlags::WRITE),
        flags.has(VmFlags::EXEC),
        flags.has(VmFlags::USER),
        false,
        phys_to_virt,
    );
}

unsafe fn clone_for_fork(src: PgdHandle, dst: PgdHandle, range: core::ops::Range<usize>) {
    let source = unsafe { inner(src) };
    let target = unsafe { inner(dst) };
    let mut address = range.start & !(X86_64Paging::PAGE_SIZE - 1);
    while address < range.end {
        let Ok((level, _, pte)) =
            find_leaf::<X86_64Paging>(source.root_virt, address, phys_to_virt)
        else {
            address = address.saturating_add(X86_64Paging::PAGE_SIZE);
            continue;
        };
        let page_size = X86_64Paging::leaf_page_size(level).unwrap_or(X86_64Paging::PAGE_SIZE);
        let source_page = X86_64Paging::pte_addr(pte) + (address & (page_size - 1));
        let request =
            allocator::PhysicalAllocRequest::new(allocator::PAGE_SIZE, allocator::PAGE_SIZE);
        let allocation = allocator::KERNEL_ALLOCATOR
            .allocate_physical(request)
            .expect("[x86][mm] fork page allocation failed");
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_to_virt(source_page) as *const u8,
                phys_to_virt(allocation.paddr) as *mut u8,
                allocator::PAGE_SIZE,
            );
        }
        let f = X86_64Paging::pte_flags(pte);
        let result = walk_and_map::<X86_64Paging>(
            target.root_virt,
            address,
            allocation.paddr,
            X86_64Paging::LEVELS - 1,
            X86_64Paging::flags_readable(f),
            X86_64Paging::flags_writable(f),
            X86_64Paging::flags_executable(f),
            X86_64Paging::flags_user_accessible(f),
            false,
            phys_to_virt,
            allocate_table_page,
        );
        if result.is_err() {
            free_table_page(allocation.paddr);
            break;
        }
        address = address.saturating_add(X86_64Paging::PAGE_SIZE);
    }
}

unsafe fn activate(handle: PgdHandle) {
    let object = unsafe { inner(handle) };
    let cpu = crate::x86_64::specific::current_cpu_id().min(sched::NR_CPUS - 1);
    let bit = 1usize.checked_shl(cpu as u32).unwrap_or(0);
    // Publish residency before loading CR3.  A concurrent updater may then
    // send a harmless early shootdown; the following CR3 load provides the
    // required flush before this CPU can use the new address space.  Keep the
    // previous PGD resident until after hardware has stopped walking it.
    object.active_cpus.fetch_or(bit, Ordering::SeqCst);
    let previous = CURRENT_USER_PGD[cpu].swap(handle.as_usize(), Ordering::AcqRel);
    unsafe { X86_64Paging::activate(PhysPageTableRoot::new(object.root_phys)) };
    if previous != 0 && previous != handle.as_usize() {
        let previous_object = unsafe { &*(previous as *const UserPgdInner) };
        previous_object
            .active_cpus
            .fetch_and(!bit, Ordering::AcqRel);
    }
}

unsafe fn activate_kernel() {
    if let Some(root) = nonzero_root() {
        unsafe { X86_64Paging::activate(PhysPageTableRoot::new(root)) };
    }
    let cpu = crate::x86_64::specific::current_cpu_id().min(sched::NR_CPUS - 1);
    let previous = CURRENT_USER_PGD[cpu].swap(0, Ordering::AcqRel);
    if previous != 0 {
        let bit = 1usize.checked_shl(cpu as u32).unwrap_or(0);
        let previous_object = unsafe { &*(previous as *const UserPgdInner) };
        previous_object
            .active_cpus
            .fetch_and(!bit, Ordering::AcqRel);
    }
}

pub(super) unsafe fn activate_kernel_for_arch() {
    unsafe { activate_kernel() };
}

unsafe fn invalidate(handle: PgdHandle, vaddr: usize, len: usize) {
    let _ = vaddr;
    if len == 0 {
        return;
    }
    let object = unsafe { inner(handle) };
    crate::x86_64::smp::shootdown_user_tlb(object.active_cpus.load(Ordering::Acquire));
}

unsafe fn count_mapped(handle: PgdHandle, vaddr: usize, len: usize) -> usize {
    let object = unsafe { inner(handle) };
    let mut address = vaddr & !(X86_64Paging::PAGE_SIZE - 1);
    let end = vaddr.saturating_add(len);
    let mut count = 0;
    while address < end {
        if find_leaf::<X86_64Paging>(object.root_virt, address, phys_to_virt).is_ok() {
            count += 1;
        }
        address = address.saturating_add(X86_64Paging::PAGE_SIZE);
    }
    count
}

pub(super) static USER_PGD_OPS: UserPgdOps = UserPgdOps {
    new_pgd_for_user,
    drop_pgd,
    zero_user_pages: zero_user_pages,
    map: map_user,
    map_pages: map_user_pages,
    publish_new_mapping,
    unmap: unmap_user,
    protect: protect_user,
    clone_for_fork,
    activate,
    activate_kernel,
    invalidate_range: invalidate,
    count_mapped,
};

unsafe fn zero_user_pages(vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, len) };
}

const _: () = assert!(CPU_MASK_BITS >= 32);
