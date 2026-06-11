//! 内存相关 syscall。

use alloc::sync::Arc;

use errno::Errno;
use general::mm::{VmSpace, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::current_fdtable;
use mm::VmFlags;
use vfs::fdtable::Fd;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;

const MAP_SHARED: usize = 0x01;
const MAP_PRIVATE: usize = 0x02;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_FIXED_NOREPLACE: usize = 0x100000;

const MS_ASYNC: usize = 1;
const MS_INVALIDATE: usize = 2;
const MS_SYNC: usize = 4;
const MS_SUPPORTED: usize = MS_ASYNC | MS_INVALIDATE | MS_SYNC;

const MCL_CURRENT: usize = 1;
const MCL_FUTURE: usize = 2;
const MCL_ONFAULT: usize = 4;
const MCL_SUPPORTED: usize = MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT;
const MLOCK_ONFAULT: usize = 1;

const MREMAP_MAYMOVE: usize = 1;
const MREMAP_FIXED: usize = 2;
const MREMAP_DONTUNMAP: usize = 4;

pub(super) fn sys_brk(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let Some(vm) = task_vm(ctx) else {
        return Err(Errno::ENOMEM);
    };
    Ok(vm.set_brk(ctx.args[0]))
}

pub(super) fn sys_mmap(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let req_addr = ctx.args[0];
    let page_size = hal::memory::page_size();
    let len = align_up(ctx.args[1], page_size).ok_or(Errno::EINVAL)?;
    let prot = ctx.args[2];
    let flags = ctx.args[3];
    let fd_raw = ctx.args[4] as isize;
    let offset = ctx.args[5] as u64;

    if len == 0 || (offset as usize) % page_size != 0 {
        return Err(Errno::EINVAL);
    }
    let is_private = (flags & MAP_PRIVATE) != 0;
    let is_shared = (flags & MAP_SHARED) != 0;
    if is_private == is_shared {
        return Err(Errno::EINVAL);
    }

    let mut vm_flags = prot_to_vm_flags(prot).with(VmFlags::USER);
    if is_shared {
        vm_flags = vm_flags.with(VmFlags::SHARED);
    }
    let fixed = (flags & MAP_FIXED) != 0;
    let fixed_noreplace = (flags & MAP_FIXED_NOREPLACE) != 0;
    let anonymous = (flags & MAP_ANONYMOUS) != 0;

    if (fixed || fixed_noreplace) && req_addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }

    if fixed || fixed_noreplace {
        let end = req_addr.checked_add(len).ok_or(Errno::EINVAL)?;
        if fixed_noreplace && !vm.is_range_free(req_addr..end) {
            return Err(Errno::EEXIST);
        }
        if fixed && !fixed_noreplace {
            if anonymous {
                vm.map_fixed_anon(req_addr..end, vm_flags)?;
            } else if fd_raw >= 0 {
                let fdt = current_fdtable().ok_or(Errno::EBADF)?;
                let file = fdt
                    .get_file(Fd::from_raw(fd_raw as u32))
                    .ok_or(Errno::EBADF)?;
                if is_shared && (prot & PROT_WRITE) != 0 && !file.flags().writable() {
                    return Err(Errno::EACCES);
                }
                let backing: Arc<dyn mm::FileLike> = file;
                vm.map_fixed_file(req_addr..end, backing, offset, vm_flags.with(VmFlags::USER))?;
            } else {
                return Err(Errno::EBADF);
            }
            return Ok(req_addr);
        }
        map_range(
            &vm,
            req_addr..end,
            anonymous,
            fd_raw,
            offset,
            vm_flags,
            is_shared,
            prot,
        )?;
        return Ok(req_addr);
    }

    for _ in 0..32 {
        let range = vm.alloc_mmap_range(len)?;
        match map_range(
            &vm,
            range.clone(),
            anonymous,
            fd_raw,
            offset,
            vm_flags,
            is_shared,
            prot,
        ) {
            Ok(()) => return Ok(range.start),
            Err(Errno::EEXIST) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Errno::ENOMEM)
}

pub(super) fn sys_munmap(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let page_size = hal::memory::page_size();
    let len = align_up(ctx.args[1], page_size).ok_or(Errno::EINVAL)?;
    if addr % page_size != 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    vm.unmap(addr..end)?;
    Ok(0)
}

pub(super) fn sys_mprotect(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let page_size = hal::memory::page_size();
    let len = align_up(ctx.args[1], page_size).ok_or(Errno::EINVAL)?;
    let prot = ctx.args[2];
    if addr % page_size != 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    vm.mprotect(addr..end, prot_to_vm_flags(prot).with(VmFlags::USER))?;
    Ok(0)
}

pub(super) fn sys_madvise(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(0)
}

pub(super) fn sys_mremap(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let old_addr = ctx.args[0];
    let old_size = ctx.args[1];
    let new_size = ctx.args[2];
    let flags = ctx.args[3];
    let new_addr = ctx.args[4];
    let page_size = hal::memory::page_size();
    if old_addr % page_size != 0 || old_size == 0 || new_size == 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP)) != 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & MREMAP_DONTUNMAP) != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if (flags & MREMAP_FIXED) != 0 && (flags & MREMAP_MAYMOVE) == 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & MREMAP_FIXED) != 0 && new_addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }
    let old_len = align_up(old_size, page_size).ok_or(Errno::EINVAL)?;
    let new_len = align_up(new_size, page_size).ok_or(Errno::EINVAL)?;
    let old_end = old_addr.checked_add(old_len).ok_or(Errno::EINVAL)?;
    let fixed = if (flags & MREMAP_FIXED) != 0 {
        Some(new_addr)
    } else {
        None
    };
    vm.mremap(
        old_addr..old_end,
        new_len,
        (flags & MREMAP_MAYMOVE) != 0,
        fixed,
    )
}

pub(super) fn sys_swapon(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_swapoff(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_msync(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let flags = ctx.args[2];
    if (flags & !MS_SUPPORTED) != 0 || (flags & MS_ASYNC) != 0 && (flags & MS_SYNC) != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    let range = page_aligned_range(addr, len)?;
    if (flags & MS_ASYNC) == 0 {
        vm.sync_range(range)?;
    }
    Ok(0)
}

pub(super) fn sys_mlock(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    if let Some(range) = rounded_page_range(ctx.args[0], ctx.args[1])? {
        vm.mlock_range(range)?;
    }
    Ok(0)
}

pub(super) fn sys_munlock(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    if let Some(range) = rounded_page_range(ctx.args[0], ctx.args[1])? {
        vm.munlock_range(range)?;
    }
    Ok(0)
}

pub(super) fn sys_mlockall(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let flags = ctx.args[0];
    if flags == 0 || (flags & !MCL_SUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & MCL_ONFAULT) != 0 && (flags & (MCL_CURRENT | MCL_FUTURE)) == 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & MCL_CURRENT) != 0 {
        vm.mlock_all_current();
    }
    if (flags & MCL_FUTURE) != 0 {
        vm.set_mlock_future(true);
    }
    Ok(0)
}

pub(super) fn sys_munlockall(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    vm.munlock_all();
    Ok(0)
}

pub(super) fn sys_mincore(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let vec_user = ctx.args[2];
    if vec_user == 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    let range = page_aligned_range(addr, len)?;
    let bitmap = vm.resident_bitmap(range)?;
    copy_to_user(vec_user, &bitmap).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_remap_file_pages(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mbind(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_get_mempolicy(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_set_mempolicy(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_migrate_pages(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_move_pages(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_process_vm_readv(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_process_vm_writev(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_userfaultfd(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mlock2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[2];
    if (flags & !MLOCK_ONFAULT) != 0 {
        return Err(Errno::EINVAL);
    }
    sys_mlock(ctx)
}

pub(super) fn sys_pkey_mprotect(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_pkey_alloc(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_pkey_free(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_process_madvise(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_memfd_secret(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_set_mempolicy_home_node(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_cachestat(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_map_shadow_stack(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mseal(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

fn task_vm(ctx: &SyscallContext<'_>) -> Option<Arc<VmSpace>> {
    let payload = ctx.task().ext_lookup(sched::TASKEXT_VM_SPACE)?;
    payload.downcast::<VmSpace>().ok()
}

fn map_range(
    vm: &VmSpace,
    range: core::ops::Range<usize>,
    anonymous: bool,
    fd_raw: isize,
    offset: u64,
    flags: VmFlags,
    shared: bool,
    prot: usize,
) -> Result<(), Errno> {
    if anonymous {
        return vm.map_anon(range, flags);
    }
    if fd_raw < 0 {
        return Err(Errno::EBADF);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt
        .get_file(Fd::from_raw(fd_raw as u32))
        .ok_or(Errno::EBADF)?;
    if shared && (prot & PROT_WRITE) != 0 && !file.flags().writable() {
        return Err(Errno::EACCES);
    }
    let backing: Arc<dyn mm::FileLike> = file;
    vm.map_file(range, backing, offset, flags.with(VmFlags::USER))
}

fn prot_to_vm_flags(prot: usize) -> VmFlags {
    let mut flags = VmFlags::EMPTY;
    if (prot & PROT_READ) != 0 {
        flags = flags.with(VmFlags::READ);
    }
    if (prot & PROT_WRITE) != 0 {
        flags = flags.with(VmFlags::WRITE);
    }
    if (prot & PROT_EXEC) != 0 {
        flags = flags.with(VmFlags::EXEC);
    }
    flags
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    Some(value.checked_add(align - 1)? & !(align - 1))
}

fn page_aligned_range(addr: usize, len: usize) -> Result<core::ops::Range<usize>, Errno> {
    let page_size = hal::memory::page_size();
    if addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }
    let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    Ok(addr..end)
}

fn rounded_page_range(addr: usize, len: usize) -> Result<Option<core::ops::Range<usize>>, Errno> {
    if len == 0 {
        return Ok(None);
    }
    let page_size = hal::memory::page_size();
    let start = addr & !(page_size - 1);
    let raw_end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    let end = align_up(raw_end, page_size).ok_or(Errno::EINVAL)?;
    if start >= end {
        return Ok(None);
    }
    Ok(Some(start..end))
}
