//! 内存相关 syscall。

use alloc::string::String;
use alloc::sync::Arc;

use errno::Errno;
use general::mm::{
    FaultKind, Mempolicy, VmSpace, copy_cstr_from_user, copy_from_user, copy_to_user,
    file_cache_stat, memstat,
};
use general::syscall::SyscallContext;
use general::vfs::{current_fdtable, current_vfs_context, pidfd};
use mm::UserAccessError;
use mm::VmFlags;
use sched::operation::get_rlimit;
use sched::rlimit::Resource;
use vfs::fdtable::Fd;
use vfs::file::{AccessMode, OpenOptions};
use vfs::operation;
use vfs::path::Dirfd;
use vfs::stat::FileMode;

/// 路径最大长度（与 Linux PATH_MAX 一致）。
const PATH_MAX: usize = 4096;

fn path_copy_errno(err: UserAccessError) -> Errno {
    match err {
        UserAccessError::TooLong => Errno::ENAMETOOLONG,
        _ => err.as_errno(),
    }
}

/// 读取一个 16 字节 iovec 条目，返回 (base, len)。
fn read_iovec(iov: usize, index: usize) -> Result<(usize, usize), Errno> {
    let mut raw = [0u8; 16];
    let ptr = iov
        .checked_add(index.checked_mul(16).ok_or(Errno::EFAULT)?)
        .ok_or(Errno::EFAULT)?;
    copy_from_user(ptr, &mut raw).map_err(|e| e.as_errno())?;
    let base = usize::from_le_bytes(raw[0..8].try_into().unwrap());
    let len = usize::from_le_bytes(raw[8..16].try_into().unwrap());
    Ok((base, len))
}

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
    let requested = ctx.args[0];
    // brk 增长按 `RLIMIT_AS`/`RLIMIT_DATA` 强制（Linux `check_data_rlimit`）。
    if requested != 0 {
        let old = vm.current_brk();
        if requested > old {
            let page_size = hal::memory::page_size();
            let grow = (requested - old).div_ceil(page_size);
            check_as_limit(ctx, grow)?;
            check_data_limit(ctx, grow)?;
        }
    }
    Ok(vm.set_brk(requested))
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

    // RLIMIT_AS / RLIMIT_DATA 强制（Linux `may_expand_vm` + `check_data_rlimit`）。
    let additional_pages = len / page_size;
    check_as_limit(ctx, additional_pages)?;
    let is_data = (prot & PROT_WRITE) != 0 && is_private && (flags & MAP_GROWSDOWN) == 0;
    if is_data {
        check_data_limit(ctx, additional_pages)?;
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
        // 非零 `req_addr` 是 Linux mmap 的地址提示，而不是只能用于
        // MAP_FIXED。若提示区间空闲，优先在该地址建立映射；HotSpot 的
        // compressed class space 会依赖返回地址精确匹配这个 hint。这里的
        // 查询与后续 VMA 登记之间允许有并发竞争，EEXIST 时回退到普通路径。
        if req_addr != 0 {
            let hint_addr = req_addr & !(page_size - 1);
            if hint_addr != 0
                && let Some(range) = vm.mmap_hint_range(hint_addr, len)
            {
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
                    Ok(()) => return Ok(range.start),
                    Err(Errno::EEXIST) => {}
                    Err(error) => return Err(error),
                }
            }
        }

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
    apply_madvise(&vm, ctx.args[0], ctx.args[1], ctx.args[2], ctx)?;
    Ok(0)
}

/// `madvise(2)` 与 `process_madvise(2)` 共用的 advice 应用逻辑。
///
/// 返回 `Err` 时语义与 Linux 一致；`process_madvise` 对每个 iovec 分别调用。
/// `ctx` 仅用于 HWPOISON/SOFT_OFFLINE 的能力检查；进程级调用不经过这两个
/// advice（在调用方先行拒绝）。
fn apply_madvise(
    vm: &VmSpace,
    addr: usize,
    len: usize,
    advice: usize,
    ctx: &SyscallContext<'_>,
) -> Result<(), Errno> {
    let page_size = hal::memory::page_size();
    if addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(());
    }

    let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    let range = addr..end;
    // mseal 语义：密封区域禁止"修改映射内容/驻留"的 madvise（Linux
    // `can_do_madvise`），命中任何密封区域即返回 EPERM。
    if matches!(
        advice,
        MADV_DONTNEED
            | MADV_DONTNEED_LOCKED
            | MADV_FREE
            | MADV_PAGEOUT
            | MADV_REMOVE
            | MADV_POPULATE_READ
            | MADV_POPULATE_WRITE
    ) && vm.has_sealed_in(&range)
    {
        return Err(Errno::EPERM);
    }
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
        // FREE：页"可释放"但内容保留到回收发生。本内核无匿名页回收，标记后可
        // 由 MADV_DONTNEED/MADV_PAGEOUT/显式回收点丢弃，无压力时内容保留。
        MADV_FREE => vm.madvise_free(range),
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
        // COLD：无 LRU/回收器，标记冷页供显式回收点（MADV_PAGEOUT）与观测使用。
        MADV_COLD => vm.madvise_cold(range),
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
    // MREMAP_DONTUNMAP 必须搭配 MREMAP_MAYMOVE（Linux 5.7+ 语义）。
    if (flags & MREMAP_DONTUNMAP) != 0 && (flags & MREMAP_MAYMOVE) == 0 {
        return Err(Errno::EINVAL);
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
    // 扩展时强制 RLIMIT_AS / RLIMIT_DATA（Linux 对 mremap 增长同样走
    // `may_expand_vm` + `check_data_rlimit`）。
    if new_len > old_len {
        let grow = (new_len - old_len) / page_size;
        check_as_limit(ctx, grow)?;
        if vm.vma_is_data(old_addr) {
            check_data_limit(ctx, grow)?;
        }
    }
    let fixed = if (flags & MREMAP_FIXED) != 0 {
        Some(new_addr)
    } else {
        None
    };
    let mapped = if (flags & MREMAP_DONTUNMAP) != 0 {
        vm.mremap_dontunmap(old_addr..old_end, new_len, fixed)?
    } else {
        vm.mremap(
            old_addr..old_end,
            new_len,
            (flags & MREMAP_MAYMOVE) != 0,
            fixed,
        )?
    };
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

pub(super) fn sys_swapon(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Linux: swapon 需要 CAP_SYS_ADMIN。
    if !ctx
        .task()
        .credentials()
        .has_cap(sched::Capability::SysAdmin)
    {
        return Err(Errno::EPERM);
    }
    const SWAP_FLAG_PREFER: usize = 0x8000;
    let path_user = ctx.args[0];
    let swapflags = ctx.args[1];
    if swapflags & !(SWAP_FLAG_PREFER | 0xffff) != 0 {
        return Err(Errno::EINVAL);
    }
    let priority = if swapflags & SWAP_FLAG_PREFER != 0 {
        (swapflags & 0xffff) as i32
    } else {
        -1
    };
    let path = copy_cstr_from_user(path_user, PATH_MAX).map_err(path_copy_errno)?;
    // 以读写方式打开 swap 文件/分区并登记；fd 由调用方自行关闭，swap 表持有
    // 自己的 Arc<File> 引用（Linux 同样在 swapoff 前持有文件引用）。换出需要
    // 写入 swap 空间，因此必须可写（Linux swapon 同样要求可写句柄）。
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let flags = OpenOptions {
        access: AccessMode::ReadWrite,
        ..OpenOptions::default()
    };
    let fd = operation::openat(&vfs_ctx, &fdt, &Dirfd::Cwd, &path, flags, FileMode::new(0))
        .map_err(|e| e.to_errno())?;
    let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
    general::mm::swap::swapon(file, path, priority)?;
    Ok(0)
}

pub(super) fn sys_swapoff(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    if !ctx
        .task()
        .credentials()
        .has_cap(sched::Capability::SysAdmin)
    {
        return Err(Errno::EPERM);
    }
    let path = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(path_copy_errno)?;
    general::mm::swap::swapoff(&path)?;
    Ok(0)
}

pub(super) fn sys_msync(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let flags = ctx.args[2];
    if (flags & !MS_SUPPORTED) != 0 || (flags & MS_ASYNC) != 0 && (flags & MS_SYNC) != 0 {
        return Err(Errno::EINVAL);
    }
    // Linux 要求 flags 至少含 MS_ASYNC 或 MS_SYNC 之一（MS_INVALIDATE 可单独
    // 搭配使用，但不能单独作为有效调用）。
    if (flags & (MS_ASYNC | MS_SYNC)) == 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    let range = page_aligned_range(addr, len)?;
    // MS_ASYNC 语义是"发起写回、不等待完成"。本内核无异步回写线程，脏
    // MAP_SHARED 页的回写退化为同步执行（保证发起写回、而非完全无动作）。
    vm.sync_range(range)?;
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

/// `RLIMIT_AS` 检查：地址空间总几何页数（含 `MAP_NORESERVE`）加新增页不得
/// 超过软上限，否则返回 `ENOMEM`（Linux `may_expand_vm` 语义）。
///
/// `MAP_FIXED` 覆盖已有映射时按完整长度保守检查，可能比 Linux 更严格（未做
/// 净增长核算）；软上限为 `RLIM_INFINITY` 时不受限制。
fn check_as_limit(ctx: &SyscallContext<'_>, additional_pages: usize) -> Result<(), Errno> {
    let pair = get_rlimit(Resource::As).map_err(|_| Errno::ENOMEM)?;
    if pair.soft.is_infinity() {
        return Ok(());
    }
    let limit_pages = (pair.soft.0 / hal::memory::page_size() as u64) as usize;
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    if vm.total_vm_pages().saturating_add(additional_pages) > limit_pages {
        return Err(Errno::ENOMEM);
    }
    Ok(())
}

/// `RLIMIT_DATA` 检查：私有可写非栈映射（Linux `is_data_mapping`）几何页数加
/// 新增页不得超过软上限，否则返回 `ENOMEM`（Linux `check_data_rlimit` 语义）。
fn check_data_limit(ctx: &SyscallContext<'_>, additional_pages: usize) -> Result<(), Errno> {
    let pair = get_rlimit(Resource::Data).map_err(|_| Errno::ENOMEM)?;
    if pair.soft.is_infinity() {
        return Ok(());
    }
    let limit_pages = (pair.soft.0 / hal::memory::page_size() as u64) as usize;
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    if vm.data_vm_pages().saturating_add(additional_pages) > limit_pages {
        return Err(Errno::ENOMEM);
    }
    Ok(())
}

pub(super) fn sys_mlock2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[2];
    if (flags & !MLOCK_ONFAULT) != 0 {
        return Err(Errno::EINVAL);
    }
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let Some(range) = rounded_page_range(ctx.args[0], ctx.args[1])? else {
        return Ok(0);
    };
    check_memlock_limit(ctx, vm.would_lock_pages(&range))?;
    // MLOCK_ONFAULT：只锁 VMA、不预读入物理页（缺页时才填充）；未置位时与
    // mlock(2) 一致，先同步 fault-in 全部页再锁。
    vm.mlock_range(range, (flags & MLOCK_ONFAULT) == 0)?;
    Ok(0)
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

pub(super) fn sys_remap_file_pages(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let addr = ctx.args[0];
    let size = ctx.args[1];
    let prot = ctx.args[2];
    let pgoff = ctx.args[3];
    let flags = ctx.args[4];
    // Linux: prot 与 flags 必须为 0。
    if prot != 0 || flags != 0 {
        return Err(Errno::EINVAL);
    }
    vm.remap_file_pages(addr, size, pgoff)?;
    Ok(0)
}

// ── NUMA 内存策略（单节点语义） ──────────────────────────────────────────────

const MPOL_DEFAULT: u32 = 0;
const MPOL_PREFERRED: u32 = 1;
const MPOL_BIND: u32 = 2;
const MPOL_INTERLEAVE: u32 = 3;
const MPOL_LOCAL: u32 = 4;

const MPOL_MF_STRICT: usize = 1;
const MPOL_MF_MOVE: usize = 2;
const MPOL_MF_MOVEALL: usize = 4;

const MPOL_F_NODE: usize = 1;
const MPOL_F_ADDR: usize = 2;
const MPOL_F_MEMS_ALLOWED: usize = 4;

/// 从用户态读取节点掩码（单节点系统：只有 node 0 合法）。
///
/// Linux `get_nodes` 语义：`maxnode` 以位为单位，`nmask` 指向
/// `ceil(maxnode/64)` 个 `unsigned long`；超过 `maxnode` 的位必须为零，否则
/// `EINVAL`。本内核只有 node 0，掩码中出现任何其它节点同样 `EINVAL`。
fn read_nodemask(nmask: usize, maxnode: usize) -> Result<u64, Errno> {
    if maxnode == 0 {
        return Err(Errno::EINVAL);
    }
    if nmask == 0 {
        return Ok(0);
    }
    let nlongs = maxnode.div_ceil(64);
    let mut mask = 0u64;
    for index in 0..nlongs {
        let mut word = [0u8; 8];
        copy_from_user(nmask + index * 8, &mut word).map_err(|e| e.as_errno())?;
        let word = u64::from_le_bytes(word);
        let valid_bits = (maxnode - index * 64).min(64) as u32;
        if valid_bits < 64 && word >> valid_bits != 0 {
            return Err(Errno::EINVAL);
        }
        if index == 0 {
            mask = word;
        } else if word != 0 {
            return Err(Errno::EINVAL);
        }
    }
    if mask & !1 != 0 {
        // 单节点系统不存在其它节点。
        return Err(Errno::EINVAL);
    }
    Ok(mask)
}

/// 校验 mempolicy 模式与掩码；返回要存储的策略（None = 默认/LOCAL）。
fn validate_mempolicy(mode: u32, mask: u64) -> Result<Option<Mempolicy>, Errno> {
    match mode {
        MPOL_DEFAULT | MPOL_LOCAL => Ok(None),
        MPOL_PREFERRED => {
            // 空掩码的 PREFERRED 等价 LOCAL（Linux 语义）。
            if mask == 0 {
                Ok(None)
            } else {
                Ok(Some(Mempolicy {
                    mode,
                    node_mask: mask,
                    home_node: 0,
                }))
            }
        }
        MPOL_BIND | MPOL_INTERLEAVE => {
            if mask == 0 {
                return Err(Errno::EINVAL);
            }
            Ok(Some(Mempolicy {
                mode,
                node_mask: mask,
                home_node: 0,
            }))
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sys_set_mempolicy(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let mode = ctx.args[0] as u32;
    let mask = read_nodemask(ctx.args[1], ctx.args[2])?;
    let policy = validate_mempolicy(mode, mask)?;
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    vm.set_task_mempolicy(policy);
    Ok(0)
}

pub(super) fn sys_get_mempolicy(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let policy_user = ctx.args[0];
    let nmask_user = ctx.args[1];
    let maxnode = ctx.args[2];
    let addr = ctx.args[3];
    let flags = ctx.args[4];
    if flags & !(MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) != 0 {
        return Err(Errno::EINVAL);
    }
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    if flags & MPOL_F_MEMS_ALLOWED != 0 {
        // 单节点系统允许的节点集合 = {0}。
        if maxnode == 0 {
            return Err(Errno::EINVAL);
        }
        if nmask_user != 0 {
            let word = 1u64.to_le_bytes();
            copy_to_user(nmask_user, &word).map_err(|e| e.as_errno())?;
        }
        return Ok(0);
    }
    let (mode, mask) = if flags & MPOL_F_ADDR != 0 {
        // 查询地址生效的策略；地址必须落在某个 VMA 内（Linux 返回 EFAULT）。
        if addr == 0 {
            return Err(Errno::EINVAL);
        }
        let page_size = hal::memory::page_size();
        let base = addr & !(page_size - 1);
        if vm
            .contains_user_range(base..base.checked_add(page_size).ok_or(Errno::EINVAL)?)
            .is_err()
        {
            return Err(Errno::EFAULT);
        }
        let (policy, _) = vm.mempolicy_at(addr);
        match policy {
            Some(policy) => (policy.mode, policy.node_mask),
            None => (MPOL_DEFAULT, 0),
        }
    } else {
        if addr != 0 {
            return Err(Errno::EINVAL);
        }
        match vm.task_mempolicy() {
            Some(policy) => (policy.mode, policy.node_mask),
            None => (MPOL_DEFAULT, 0),
        }
    };
    // MPOL_F_NODE：返回策略对应的节点（单节点系统恒为 0）。
    let reported = if flags & MPOL_F_NODE != 0 { 0u32 } else { mode };
    if policy_user != 0 {
        let raw = (reported as i32).to_le_bytes();
        copy_to_user(policy_user, &raw).map_err(|e| e.as_errno())?;
    }
    if nmask_user != 0 && maxnode != 0 {
        let word = mask.to_le_bytes();
        copy_to_user(nmask_user, &word).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_mbind(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let mode = ctx.args[2] as u32;
    let mask = read_nodemask(ctx.args[3], ctx.args[4])?;
    let flags = ctx.args[5];
    if flags & !(MPOL_MF_STRICT | MPOL_MF_MOVE | MPOL_MF_MOVEALL) != 0 {
        return Err(Errno::EINVAL);
    }
    let policy = validate_mempolicy(mode, mask)?;
    let page_size = hal::memory::page_size();
    if addr % page_size != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    // 单节点语义：所有页都在 node 0，STRICT/MOVE/MOVEALL 恒可满足；只登记
    // 区域策略（范围未映射时 mbind_range 返回 ENOMEM，与 Linux 一致）。
    let policy = policy.unwrap_or(Mempolicy {
        mode,
        node_mask: 0,
        home_node: 0,
    });
    vm.mbind_range(addr..end, policy)?;
    Ok(0)
}

pub(super) fn sys_migrate_pages(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let maxnode = ctx.args[1];
    let old_mask = read_nodemask(ctx.args[2], maxnode)?;
    let new_mask = read_nodemask(ctx.args[3], maxnode)?;
    let target = if pid == 0 {
        Arc::clone(ctx.task())
    } else {
        lookup_task_by_pid(pid)?
    };
    if !process_vm_may_access(ctx.task(), &target) {
        return Err(Errno::EPERM);
    }
    // 单节点系统没有可迁移的页；成功返回 0（Linux 返回"未能迁移的页数"）。
    let _ = (old_mask, new_mask);
    Ok(0)
}

pub(super) fn sys_move_pages(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let nr_pages = ctx.args[1];
    let pages_user = ctx.args[2];
    let nodes_user = ctx.args[3];
    let status_user = ctx.args[4];
    let flags = ctx.args[5];
    if flags & !(MPOL_MF_MOVE | MPOL_MF_MOVEALL) != 0 {
        return Err(Errno::EINVAL);
    }
    if nr_pages == 0 {
        return Ok(0);
    }
    let target = if pid == 0 {
        Arc::clone(ctx.task())
    } else {
        lookup_task_by_pid(pid)?
    };
    if !process_vm_may_access(ctx.task(), &target) {
        return Err(Errno::EPERM);
    }
    let vm = target_vm(&target).ok_or(Errno::ENOMEM)?;
    let page_size = hal::memory::page_size();
    let mut not_migrated = 0usize;
    for index in 0..nr_pages {
        let page_user = read_user_usize(pages_user + index * 8)?;
        if page_user % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        if nodes_user != 0 {
            // 节点数组：单节点系统只接受 node 0。
            let mut raw = [0u8; 4];
            copy_from_user(nodes_user + index * 4, &mut raw).map_err(|e| e.as_errno())?;
            let node = i32::from_le_bytes(raw);
            if node != 0 {
                return Err(Errno::EINVAL);
            }
        }
        // Linux `move_pages` 的 status 反映页迁移结果：页不存在 → -ENOENT；
        // 存在且已在目标节点（单节点系统恒在 node 0）→ 0（无需迁移）。
        // 本内核无 NUMA 迁移，以"页是否驻留"作为 status 判定。
        let status: i32 = if vm.is_page_resident(page_user) {
            0
        } else {
            -(Errno::ENOENT.as_i32_direct() as i32)
        };
        if status != 0 {
            not_migrated += 1;
        }
        if status_user != 0 {
            let raw = status.to_le_bytes();
            copy_to_user(status_user + index * 4, &raw).map_err(|e| e.as_errno())?;
        }
    }
    // 返回值 = 未能迁移（或不存在）的页数。
    Ok(not_migrated)
}

pub(super) fn sys_set_mempolicy_home_node(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let home_node = ctx.args[2] as i32;
    let flags = ctx.args[3];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let page_size = hal::memory::page_size();
    if addr % page_size != 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    // 单节点系统只有 node 0 是合法 home node。
    if home_node != 0 {
        return Err(Errno::EINVAL);
    }
    let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    // 范围必须被 `MPOL_BIND`/`MPOL_PREFERRED` 区域策略覆盖（Linux 语义），
    // 否则 EINVAL；成功时记录 home node 供 `get_mempolicy` 观测。
    vm.set_mempolicy_home_node(addr..end, home_node as u32)?;
    Ok(0)
}

// ── 跨进程内存访问 ───────────────────────────────────────────────────────────

/// `process_vm_readv`/`writev`/`process_madvise`/`move_pages` 的目标访问检查
/// （对应 Linux `ptrace_may_access(PTRACE_MODE_ATTACH_REALCREDS)`）。
fn process_vm_may_access(current: &Arc<sched::Task>, target: &Arc<sched::Task>) -> bool {
    if Arc::ptr_eq(current, target) {
        return true;
    }
    let current_creds = current.credentials();
    if current_creds.has_cap(sched::Capability::SysPtrace)
        || current_creds.has_cap(sched::Capability::SysAdmin)
    {
        return true;
    }
    let target_creds = target.credentials();
    current_creds.euid == target_creds.uid
        || current_creds.euid == target_creds.euid
        || current_creds.uid == target_creds.uid
        || current_creds.uid == target_creds.euid
}

/// 从任意任务取 VmSpace。
fn target_vm(task: &Arc<sched::Task>) -> Option<Arc<VmSpace>> {
    let payload = task.ext_lookup(sched::TASKEXT_VM_SPACE)?;
    payload.downcast::<VmSpace>().ok()
}

fn lookup_task_by_pid(pid: i32) -> Result<Arc<sched::Task>, Errno> {
    if pid <= 0 {
        return Err(Errno::EINVAL);
    }
    sched::root_pid_ns()
        .registry()
        .lookup(pid)
        .and_then(|weak| weak.upgrade())
        .ok_or(Errno::ESRCH)
}

fn read_user_usize(user: usize) -> Result<usize, Errno> {
    let mut raw = [0u8; 8];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(usize::from_le_bytes(raw))
}

fn read_user_i32(user: usize) -> Result<i32, Errno> {
    let mut raw = [0u8; 4];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(i32::from_le_bytes(raw))
}

pub(super) fn sys_process_vm_readv(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    process_vm_rw(ctx, false)
}

pub(super) fn sys_process_vm_writev(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    process_vm_rw(ctx, true)
}

fn process_vm_rw(ctx: &mut SyscallContext<'_>, write: bool) -> Result<usize, Errno> {
    let pid = ctx.args[0] as i32;
    let local_iov = ctx.args[1];
    let local_count = ctx.args[2];
    let remote_iov = ctx.args[3];
    let remote_count = ctx.args[4];
    let flags = ctx.args[5];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    if local_count > 1024 || remote_count > 1024 {
        return Err(Errno::EINVAL);
    }
    let target = if pid == 0 {
        Arc::clone(ctx.task())
    } else {
        lookup_task_by_pid(pid)?
    };
    if !process_vm_may_access(ctx.task(), &target) {
        return Err(Errno::EPERM);
    }
    let remote_vm = target_vm(&target).ok_or(Errno::ENOMEM)?;
    let mut total = 0usize;
    let mut local_index = 0usize;
    let mut remote_index = 0usize;
    let mut local = if local_count != 0 {
        Some(read_iovec(local_iov, 0)?)
    } else {
        None
    };
    let mut remote = if remote_count != 0 {
        Some(read_iovec(remote_iov, 0)?)
    } else {
        None
    };
    loop {
        // 跳过空 iovec，必要时取下一个。
        loop {
            let Some((_, len)) = local else { break };
            if len != 0 {
                break;
            }
            local_index += 1;
            local = if local_index < local_count {
                Some(read_iovec(local_iov, local_index)?)
            } else {
                None
            };
        }
        loop {
            let Some((_, len)) = remote else { break };
            if len != 0 {
                break;
            }
            remote_index += 1;
            remote = if remote_index < remote_count {
                Some(read_iovec(remote_iov, remote_index)?)
            } else {
                None
            };
        }
        let (Some((local_base, local_len)), Some((remote_base, remote_len))) = (&local, &remote)
        else {
            break;
        };
        let chunk = (*local_len).min(*remote_len);
        let copied =
            copy_process_vm_range(ctx, &remote_vm, *remote_base, *local_base, chunk, write)?;
        total = total.saturating_add(copied);
        if copied < chunk {
            // 目标页不可访问：已拷贝部分成功返回，未开始则报 EFAULT（Linux 语义）。
            if total == 0 {
                return Err(Errno::EFAULT);
            }
            break;
        }
        // 按实际拷贝字节推进两个 iovec 游标。
        local = Some((*local_base + copied, *local_len - copied));
        remote = Some((*remote_base + copied, *remote_len - copied));
    }
    Ok(total)
}

/// 单页内拷贝：先确保远程页驻留（含 COW），再在本地/远程用户地址与内核
/// 直映页之间搬运。
fn copy_process_vm_range(
    ctx: &mut SyscallContext<'_>,
    remote: &VmSpace,
    remote_addr: usize,
    local_addr: usize,
    len: usize,
    write: bool,
) -> Result<usize, Errno> {
    let page_size = hal::memory::page_size();
    let mut copied = 0usize;
    while copied < len {
        let ra = remote_addr + copied;
        let la = local_addr + copied;
        let offset = ra & (page_size - 1);
        let chunk = (page_size - offset).min(len - copied);
        let kind = if write {
            FaultKind::Store
        } else {
            FaultKind::Load
        };
        if remote
            .ensure_remote_page(ra & !(page_size - 1), kind)
            .is_err()
        {
            return Ok(copied);
        }
        let mut buf = [0u8; 4096];
        if write {
            if copy_from_user(la, &mut buf[..chunk]).is_err() {
                return Ok(copied);
            }
            if remote
                .copy_resident_bytes_in(ra..ra + chunk, &buf[..chunk])
                .is_err()
            {
                return Ok(copied);
            }
        } else {
            if remote
                .copy_resident_bytes_out(ra..ra + chunk, &mut buf[..chunk])
                .is_err()
            {
                return Ok(copied);
            }
            if copy_to_user(la, &buf[..chunk]).is_err() {
                return Ok(copied);
            }
        }
        copied += chunk;
    }
    Ok(copied)
}

pub(super) fn sys_process_madvise(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pidfd_raw = ctx.args[0] as i32;
    let iov = ctx.args[1];
    let vlen = ctx.args[2];
    let advice = ctx.args[3];
    let flags = ctx.args[4];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    // process_madvise 不接受特权类/文件类 advice（Linux 语义）。
    if !matches!(
        advice,
        MADV_NORMAL
            | MADV_RANDOM
            | MADV_SEQUENTIAL
            | MADV_WILLNEED
            | MADV_DONTNEED
            | MADV_FREE
            | MADV_COLD
            | MADV_PAGEOUT
            | MADV_POPULATE_READ
            | MADV_POPULATE_WRITE
    ) {
        return Err(Errno::EINVAL);
    }
    if vlen > 1024 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt
        .get_file(Fd::from_raw(pidfd_raw as u32))
        .ok_or(Errno::EBADF)?;
    let group = pidfd::group_from_file(&file).ok_or(Errno::EINVAL)?;
    let leader = group.leader().ok_or(Errno::ESRCH)?;
    if !process_vm_may_access(ctx.task(), &leader) {
        return Err(Errno::EPERM);
    }
    let vm = target_vm(&leader).ok_or(Errno::ENOMEM)?;
    // Linux 语义：返回已成功建议的字节数；某个 iovec 失败时返回此前累计的
    // 字节数，仅当尚未处理任何字节时才返回该错误。
    let mut total = 0usize;
    for index in 0..vlen {
        let (base, len) = read_iovec(iov, index)?;
        if len == 0 {
            continue;
        }
        match apply_madvise(&vm, base, len, advice, ctx) {
            Ok(()) => total = total.saturating_add(len),
            Err(err) => {
                if total != 0 {
                    return Ok(total);
                }
                return Err(err);
            }
        }
    }
    Ok(total)
}

// ── cachestat ────────────────────────────────────────────────────────────────

pub(super) fn sys_cachestat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd_raw = ctx.args[0] as i32;
    let range_user = ctx.args[1];
    let cstat_user = ctx.args[2];
    let flags = ctx.args[3];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let mut raw_range = [0u8; 16];
    copy_from_user(range_user, &mut raw_range).map_err(|e| e.as_errno())?;
    let off = u64::from_le_bytes(raw_range[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(raw_range[8..16].try_into().unwrap());
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    off.checked_add(len).ok_or(Errno::EINVAL)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt
        .get_file(Fd::from_raw(fd_raw as u32))
        .ok_or(Errno::EBADF)?;
    if !file.flags().readable() {
        return Err(Errno::EBADF);
    }
    let (shared_key, private_key) = {
        let file_like: &dyn mm::FileLike = file.as_ref();
        (file_like.cache_key(), file_like.private_page_cache_key())
    };
    let (nr_cache, nr_dirty, nr_writeback, nr_evicted, nr_recently_evicted) =
        file_cache_stat(shared_key, private_key, off, len);
    let mut out = [0u8; 40];
    for (slot, value) in [
        nr_cache,
        nr_dirty,
        nr_writeback,
        nr_evicted,
        nr_recently_evicted,
    ]
    .iter()
    .enumerate()
    {
        out[slot * 8..slot * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    copy_to_user(cstat_user, &out).map_err(|e| e.as_errno())?;
    Ok(0)
}

// ── mseal / pkey ─────────────────────────────────────────────────────────────

pub(super) fn sys_mseal(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let addr = ctx.args[0];
    let len = ctx.args[1];
    let flags = ctx.args[2];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let page_size = hal::memory::page_size();
    if addr % page_size != 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
    let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    vm.update_area_flags(addr..end, |flags| flags.with(VmFlags::SEALED))?;
    Ok(0)
}

pub(super) fn sys_pkey_mprotect(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // LoongArch/RISC-V 无 PKU；pkey == -1 时退化为普通 mprotect（Linux
    // glibc 兼容用法），否则按无 PKU 架构返回 ENOSYS。
    let pkey = ctx.args[3] as isize;
    if pkey != -1 {
        return Err(Errno::ENOSYS);
    }
    sys_mprotect(ctx)
}

pub(super) fn sys_pkey_alloc(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // 无 PKU 架构：Linux 返回 ENOSYS。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_pkey_free(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_memfd_secret(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // !CONFIG_MEMFD_SECRET 内核的 Linux 等价行为。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_map_shadow_stack(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // CET shadow stack 仅 x86；其它架构 Linux 不提供该 syscall。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_userfaultfd(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const O_CLOEXEC: usize = 0x80000;
    const O_NONBLOCK: usize = 0x800;
    // UFFD_USER_MODE_ONLY：仅拦截用户态缺页。本实现只服务于用户态访问，
    // 接受该标志（内核态 uaccess 命中的行为与未置位时一致）。
    const UFFD_USER_MODE_ONLY: usize = 1 << 0;
    let flags = ctx.args[0];
    if flags & !(O_CLOEXEC | O_NONBLOCK | UFFD_USER_MODE_ONLY) != 0 {
        return Err(Errno::EINVAL);
    }
    // vm.unprivileged_userfaultfd：非特权进程默认不允许（Linux 6.x 语义）。
    let creds = ctx.task().credentials();
    let privileged =
        creds.has_cap(sched::Capability::SysPtrace) || creds.has_cap(sched::Capability::SysAdmin);
    if !memstat::userfaultfd_allowed(privileged) {
        return Err(Errno::EPERM);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = general::mm::uffd::create_uffd_fd(
        &fdt,
        vfs_ctx.cred(),
        (flags & O_NONBLOCK) != 0,
        (flags & O_CLOEXEC) != 0,
    )?;
    Ok(fd.as_raw() as usize)
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
