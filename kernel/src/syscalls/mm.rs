//! 内存相关 syscall。

use alloc::sync::Arc;

use errno::Errno;
use general::mm::{VmSpace, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::current_fdtable;
use mm::VmFlags;
use sched::operation::get_rlimit;
use sched::rlimit::Resource;
use vfs::fdtable::Fd;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;
const PROT_GROWSDOWN: usize = 0x0100_0000;
const PROT_GROWSUP: usize = 0x0200_0000;
const PROT_SUPPORTED: usize = PROT_READ | PROT_WRITE | PROT_EXEC | PROT_GROWSDOWN | PROT_GROWSUP;

const MAP_SHARED: usize = 0x01;
const MAP_PRIVATE: usize = 0x02;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_GROWSDOWN: usize = 0x00100;
const MAP_DENYWRITE: usize = 0x00800;
const MAP_EXECUTABLE: usize = 0x01000;
const MAP_LOCKED: usize = 0x02000;
const MAP_NORESERVE: usize = 0x04000;
const MAP_POPULATE: usize = 0x08000;
const MAP_STACK: usize = 0x20000;
const MAP_HUGETLB: usize = 0x40000;
const MAP_SYNC: usize = 0x80000;
const MAP_FIXED_NOREPLACE: usize = 0x100000;
const MAP_UNINITIALIZED: usize = 0x4000000;
const MAP_DROPPABLE: usize = 0x800000;

/// Linux 接受的全部 mmap flag 位。未知位必须返回 EINVAL，不能静默忽略。
const MAP_KNOWN: usize = MAP_SHARED
    | MAP_PRIVATE
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_GROWSDOWN
    | MAP_DENYWRITE
    | MAP_EXECUTABLE
    | MAP_LOCKED
    | MAP_NORESERVE
    | MAP_POPULATE
    | MAP_STACK
    | MAP_HUGETLB
    | MAP_SYNC
    | MAP_FIXED_NOREPLACE
    | MAP_UNINITIALIZED
    | MAP_DROPPABLE;

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

// ── madvise advice 全集（与 Linux <linux/madvise.h> 对齐） ───────────────────
const MADV_NORMAL: usize = 0;
const MADV_RANDOM: usize = 1;
const MADV_SEQUENTIAL: usize = 2;
const MADV_WILLNEED: usize = 3;
const MADV_DONTNEED: usize = 4;
const MADV_FREE: usize = 8;
const MADV_REMOVE: usize = 9;
const MADV_DONTFORK: usize = 10;
const MADV_DOFORK: usize = 11;
const MADV_MERGEABLE: usize = 12;
const MADV_UNMERGEABLE: usize = 13;
const MADV_HUGEPAGE: usize = 14;
const MADV_NOHUGEPAGE: usize = 15;
const MADV_DONTDUMP: usize = 16;
const MADV_DODUMP: usize = 17;
const MADV_WIPEONFORK: usize = 18;
const MADV_KEEPONFORK: usize = 19;
const MADV_COLD: usize = 20;
const MADV_PAGEOUT: usize = 21;
const MADV_POPULATE_READ: usize = 22;
const MADV_POPULATE_WRITE: usize = 23;
const MADV_DONTNEED_LOCKED: usize = 24;
const MADV_HWPOISON: usize = 100;
const MADV_SOFT_OFFLINE: usize = 101;

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
    // 未知 flag 位必须显式拒绝，不能静默忽略（Linux 语义）。
    if flags & !MAP_KNOWN != 0 {
        return Err(Errno::EINVAL);
    }
    // prot 未知位（如 PROT_GROWSUP / PROT_SAO 之类）同样拒绝。
    if prot & !PROT_SUPPORTED != 0 {
        return Err(Errno::EINVAL);
    }
    // hugetlb 未实现：等价 !CONFIG_HUGETLBFS 的内核，返回 ENODEV。
    if flags & MAP_HUGETLB != 0 {
        return Err(Errno::ENODEV);
    }
    // MAP_SYNC 需要 DAX 文件系统支持；本内核无 DAX，返回 EOPNOTSUPP。
    if flags & MAP_SYNC != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let is_private = (flags & MAP_PRIVATE) != 0;
    let is_shared = (flags & MAP_SHARED) != 0;
    if is_private == is_shared {
        return Err(Errno::EINVAL);
    }
    // MAP_DROPPABLE 只允许匿名私有映射（Linux 6.6+ 语义）。
    if flags & MAP_DROPPABLE != 0 && (is_shared || flags & MAP_ANONYMOUS == 0) {
        return Err(Errno::EINVAL);
    }

    let mut vm_flags = prot_to_vm_flags(prot).with(VmFlags::USER);
    if is_shared {
        vm_flags = vm_flags.with(VmFlags::SHARED);
    }
    if flags & MAP_GROWSDOWN != 0 {
        vm_flags = vm_flags.with(VmFlags::GROWS_DOWN);
    }
    if flags & MAP_NORESERVE != 0 {
        vm_flags = vm_flags.with(VmFlags::NORESERVE);
    }
    if flags & MAP_DROPPABLE != 0 {
        vm_flags = vm_flags.with(VmFlags::DROPPABLE);
    }
    let lock_pages = flags & MAP_LOCKED != 0;
    if lock_pages {
        check_memlock_limit(ctx, len / page_size)?;
        vm_flags = vm_flags.with(VmFlags::LOCKED);
    }
    let fixed = (flags & MAP_FIXED) != 0;
    let fixed_noreplace = (flags & MAP_FIXED_NOREPLACE) != 0;
    let anonymous = (flags & MAP_ANONYMOUS) != 0;

    if (fixed || fixed_noreplace) && req_addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }

    let result = if fixed || fixed_noreplace {
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
                if !file.flags().readable() {
                    return Err(Errno::EACCES);
                }
                if is_shared && (prot & PROT_WRITE) != 0 && !file.flags().writable() {
                    return Err(Errno::EACCES);
                }
                #[cfg(feature = "performance-profile")]
                let profile_image = profile_file_mapping(&file, req_addr, offset);
                let backing: Arc<dyn mm::FileLike> = file;
                vm.map_fixed_file(req_addr..end, backing, offset, vm_flags)?;
                #[cfg(feature = "performance-profile")]
                if let Some((image_id, load_base)) = profile_image {
                    ctx.task()
                        .register_profile_mapped_image(image_id, req_addr, end, load_base);
                } else {
                    ctx.task().clear_profile_mapped_images(req_addr, end);
                }
            } else {
                return Err(Errno::EBADF);
            }
            #[cfg(feature = "performance-profile")]
            if anonymous {
                ctx.task().clear_profile_mapped_images(req_addr, end);
            }
            Ok(req_addr)
        } else {
            map_range(
                ctx.task(),
                &vm,
                req_addr..end,
                anonymous,
                fd_raw,
                offset,
                vm_flags,
                is_shared,
                prot,
            )?;
            Ok(req_addr)
        }
    } else {
        let mut result = Err(Errno::ENOMEM);
        for _ in 0..32 {
            let range = vm.alloc_mmap_range(len)?;
            match map_range(
                ctx.task(),
                &vm,
                range.clone(),
                anonymous,
                fd_raw,
                offset,
                vm_flags,
                is_shared,
                prot,
            ) {
                Ok(()) => {
                    result = Ok(range.start);
                    break;
                }
                Err(Errno::EEXIST) => continue,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        result
    };

    let mapped_addr = result?;
    // MAP_POPULATE / MAP_LOCKED 需要立即填充页表。失败按 Linux 语义回滚映射
    // 并返回 ENOMEM（MAP_LOCKED 时"无法锁定"同样报 ENOMEM）。
    if flags & (MAP_POPULATE | MAP_LOCKED) != 0 {
        let range = mapped_addr..mapped_addr.checked_add(len).ok_or(Errno::EINVAL)?;
        let write = flags & MAP_LOCKED != 0;
        if vm.prefault_user_range(range.clone(), write).is_err() {
            let _ = vm.unmap(range);
            return Err(Errno::ENOMEM);
        }
    }
    Ok(mapped_addr)
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
    #[cfg(feature = "performance-profile")]
    ctx.task().clear_profile_mapped_images(addr, end);
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

pub(super) fn sys_madvise(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let advice = ctx.args[2];

    let page_size = hal::memory::page_size();
    if addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }

    let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    let range = addr..end;
    match advice {
        // 访问策略类：当前 VM 无策略状态，完成可见校验即可（同原有语义）。
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL => {
            vm.contains_user_range(range)?;
            Ok(())
        }
        // 尽力预读/预填充；范围未映射返回 ENOMEM（Linux 2.6.16+ 语义）。
        MADV_WILLNEED => vm.madvise_populate(range, false, false),
        // DONTNEED：丢弃驻留页；命中 mlock 区域返回 EINVAL（Linux 语义）。
        MADV_DONTNEED => vm.discard_resident_range(range),
        // DONTNEED_LOCKED：允许命中已锁区域（Linux 5.18+）。
        MADV_DONTNEED_LOCKED => vm.discard_resident_range_locked(range),
        // FREE：页"可释放"但内容保留到回收发生。本内核无匿名页回收，内容
        // 始终保留——与无内存压力的 Linux 可观测行为一致。
        MADV_FREE => {
            vm.contains_user_range(range)?;
            Ok(())
        }
        // REMOVE：仅 tmpfs/shmem 文件（等价 fallocate(PUNCH_HOLE|KEEP_SIZE)）。
        MADV_REMOVE => vm.madvise_remove(range),
        MADV_DONTFORK => vm.update_area_flags(range, |flags| flags.with(VmFlags::DONTFORK)),
        MADV_DOFORK => vm.update_area_flags(range, |flags| flags.without(VmFlags::DONTFORK)),
        MADV_MERGEABLE => vm.update_area_flags(range, |flags| flags.with(VmFlags::MERGEABLE)),
        MADV_UNMERGEABLE => vm.update_area_flags(range, |flags| flags.without(VmFlags::MERGEABLE)),
        MADV_HUGEPAGE => vm.update_area_flags(range, |flags| flags.with(VmFlags::HUGEPAGE)),
        MADV_NOHUGEPAGE => vm.update_area_flags(range, |flags| flags.without(VmFlags::HUGEPAGE)),
        MADV_DONTDUMP => vm.update_area_flags(range, |flags| flags.with(VmFlags::DONTDUMP)),
        MADV_DODUMP => vm.update_area_flags(range, |flags| flags.without(VmFlags::DONTDUMP)),
        MADV_WIPEONFORK => vm.update_area_flags(range, |flags| flags.with(VmFlags::WIPEONFORK)),
        MADV_KEEPONFORK => vm.update_area_flags(range, |flags| flags.without(VmFlags::WIPEONFORK)),
        // COLD：无 LRU/回收器，仅校验（语义上"标记冷"无可观测效果）。
        MADV_COLD => {
            vm.contains_user_range(range)?;
            Ok(())
        }
        MADV_PAGEOUT => vm.madvise_pagout(range),
        MADV_POPULATE_READ => vm.madvise_populate(range, false, true),
        MADV_POPULATE_WRITE => vm.madvise_populate(range, true, true),
        // HWPOISON / SOFT_OFFLINE：需要 CAP_SYS_ADMIN 且本内核无故障注入能力。
        // 无能力返回 EPERM；有能力也无法执行，返回 EINVAL。
        MADV_HWPOISON | MADV_SOFT_OFFLINE => {
            if ctx
                .task()
                .credentials()
                .has_cap(sched::Capability::SysAdmin)
            {
                Err(Errno::EINVAL)
            } else {
                Err(Errno::EPERM)
            }
        }
        _ => Err(Errno::EINVAL),
    }
    .map(|_| 0)
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
    let mapped = vm.mremap(
        old_addr..old_end,
        new_len,
        (flags & MREMAP_MAYMOVE) != 0,
        fixed,
    )?;
    #[cfg(feature = "performance-profile")]
    {
        let mapped_end = mapped.checked_add(new_len).ok_or(Errno::EINVAL)?;
        if mapped != old_addr {
            ctx.task().clear_profile_mapped_images(mapped, mapped_end);
        }
        ctx.task()
            .remap_profile_mapped_images(old_addr, old_end, mapped, mapped_end);
    }
    Ok(mapped)
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
    let Some(range) = rounded_page_range(ctx.args[0], ctx.args[1])? else {
        return Ok(0);
    };
    check_memlock_limit(ctx, vm.would_lock_pages(&range))?;
    vm.mlock_range(range, true)?;
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
        check_memlock_limit(ctx, vm.would_lock_all_pages())?;
        vm.mlock_all_current((flags & MCL_ONFAULT) == 0)?;
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

/// `RLIMIT_MEMLOCK` 检查：超出软上限时返回 `ENOMEM`（Linux 语义）；持有
/// `CAP_IPC_LOCK` 的进程不受限制。
fn check_memlock_limit(ctx: &SyscallContext<'_>, additional_pages: usize) -> Result<(), Errno> {
    if ctx.task().credentials().has_cap(sched::Capability::IpcLock) {
        return Ok(());
    }
    let pair = get_rlimit(Resource::Memlock).map_err(|_| Errno::ENOMEM)?;
    let limit_pages = (pair.soft.0 / hal::memory::page_size() as u64) as usize;
    let locked = task_vm(ctx).map(|vm| vm.locked_pages()).unwrap_or(0);
    if locked.saturating_add(additional_pages) > limit_pages {
        return Err(Errno::ENOMEM);
    }
    Ok(())
}

pub(super) fn sys_mlock2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[2];
    if (flags & !MLOCK_ONFAULT) != 0 {
        return Err(Errno::EINVAL);
    }
    sys_mlock(ctx)
}

pub(super) fn sys_mincore(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let vec_user = ctx.args[2];
    let page_size = hal::memory::page_size();
    if addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    if vec_user == 0 {
        return Err(Errno::EFAULT);
    }

    let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    let range = addr..end;
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
    task: &sched::Task,
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
        #[cfg(not(feature = "performance-profile"))]
        let _ = task;
        return vm.map_anon(range, flags);
    }
    if fd_raw < 0 {
        return Err(Errno::EBADF);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt
        .get_file(Fd::from_raw(fd_raw as u32))
        .ok_or(Errno::EBADF)?;
    if !file.flags().readable() {
        return Err(Errno::EACCES);
    }
    if shared && (prot & PROT_WRITE) != 0 && !file.flags().writable() {
        return Err(Errno::EACCES);
    }
    #[cfg(feature = "performance-profile")]
    let profile_image = profile_file_mapping(&file, range.start, offset);
    #[cfg(not(feature = "performance-profile"))]
    let _ = task;
    let image_start = range.start;
    let image_end = range.end;
    let backing: Arc<dyn mm::FileLike> = file;
    vm.map_file(range, backing, offset, flags.with(VmFlags::USER))?;
    #[cfg(feature = "performance-profile")]
    if let Some((image_id, load_base)) = profile_image {
        task.register_profile_mapped_image(image_id, image_start, image_end, load_base);
    }
    Ok(())
}

#[cfg(feature = "performance-profile")]
fn profile_file_mapping(
    file: &Arc<vfs::file::File>,
    start: usize,
    offset: u64,
) -> Option<(u64, usize)> {
    let path = file.dentry().full_path(&file.mount().mount_root)?;
    let offset = usize::try_from(offset).ok()?;
    let load_base = start.checked_sub(offset)?;
    Some((crate::sched::profile_image_id(&path), load_base))
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
