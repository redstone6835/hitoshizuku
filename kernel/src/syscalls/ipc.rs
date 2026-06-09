//! SysV IPC syscall glue.
//!
//! SysV shm 的真实对象和生命周期由 `general::ipc::shm` 管理；本文件只做
//! Linux asm-generic ABI 编解码、当前任务凭据转换和 VM 映射操作。这样 syscall
//! 层不会再持有第二份 shm 表，也不会把 attach 计数硬编码到地址记录里。

use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;
use general::ipc::shm::{
    IPC_64, IPC_RMID, IPC_SET, IPC_STAT, SHM_EXEC, SHM_RDONLY, SHM_REMAP, SHM_RND, ShmId, ShmKey,
    ShmManager, ShmMetadata, ShmMetadataUpdate,
};
use general::mm::{VmSpace, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use mm::{FileLike, VmFlags};
use sched::ids::{Capability as SchedCapability, Credentials as SchedCredentials};
use sched::sync::Spinlock;
use vfs::cred::{
    CapSet as VfsCapSet, Capability as VfsCapability, Credentials as VfsCredentials, Gid as VfsGid,
    Uid as VfsUid,
};
use vfs::stat::FileMode;

const MODE_MASK: u16 = 0o777;
const SHMAT_KNOWN_FLAGS: u32 = SHM_RDONLY | SHM_RND | SHM_REMAP | SHM_EXEC;

// asm-generic 64-bit ABI:
// - `struct ipc64_perm` is 48 bytes.
// - `struct shmid64_ds` is 112 bytes.
// The kernel stores typed metadata in `general::ipc::shm`; only this ABI edge
// packs/unpacks the Linux byte layout.
const IPC64_PERM_SIZE: usize = 48;
const SHMID64_DS_SIZE: usize = 112;

static SYSV_SHM_MANAGER: Spinlock<Option<Arc<ShmManager>>> = Spinlock::new(None);

pub(super) fn sys_shmget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let key = ShmKey(ctx.args[0] as i32);
    let size = ctx.args[1] as u64;
    let flags = ctx.args[2] as u32;
    let cred = vfs_cred_from_sched(&ctx.task.credentials());

    let manager = shm_manager();
    let id = manager.shmget(key, size, flags, &cred)?;
    Ok(id.0 as usize)
}

pub(super) fn sys_shmat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let shmid = ShmId(ctx.args[0] as i32);
    let mut req_addr = ctx.args[1];
    let flags = ctx.args[2] as u32;
    let page_size = hal::memory::page_size();

    if flags & !SHMAT_KNOWN_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & SHM_REMAP != 0 && req_addr == 0 {
        return Err(Errno::EINVAL);
    }
    if req_addr != 0 {
        if flags & SHM_RND != 0 {
            req_addr &= !(page_size - 1);
        } else if req_addr % page_size != 0 {
            return Err(Errno::EINVAL);
        }
    }

    let cred = vfs_cred_from_sched(&ctx.task.credentials());
    let manager = shm_manager();
    let object = manager.attach(shmid, flags, &cred)?;
    let size = usize::try_from(object.len()).map_err(|_| Errno::EINVAL)?;
    let len = align_up(size, page_size).ok_or(Errno::EINVAL)?;
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let vm_flags = shmat_vm_flags(flags);
    let backing: Arc<dyn FileLike> = object;

    let range = if req_addr == 0 {
        let range = vm.alloc_mmap_range(len)?;
        vm.map_file(range.clone(), backing, 0, vm_flags)?;
        range
    } else {
        let end = req_addr.checked_add(len).ok_or(Errno::EINVAL)?;
        let range = req_addr..end;
        if flags & SHM_REMAP != 0 {
            vm.map_fixed_file(range.clone(), backing, 0, vm_flags)?;
        } else {
            vm.map_file(range.clone(), backing, 0, vm_flags)
                .map_err(|err| {
                    if err == Errno::EEXIST {
                        Errno::EINVAL
                    } else {
                        err
                    }
                })?;
        }
        range
    };

    manager.note_attach(shmid, task_pid(ctx), now_sec());
    Ok(range.start)
}

pub(super) fn sys_shmdt(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let addr = ctx.args[0];
    let vm = task_vm(ctx).ok_or(Errno::ENOMEM)?;
    let (range, shmid_raw) = vm.sysv_shm_mapping_at(addr).ok_or(Errno::EINVAL)?;
    vm.unmap(range)?;
    shm_manager().note_detach(ShmId(shmid_raw), task_pid(ctx), now_sec());
    Ok(0)
}

pub(super) fn sys_shmctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let shmid = ShmId(ctx.args[0] as i32);
    let raw_cmd = ctx.args[1] as u32;
    let cmd = raw_cmd & !IPC_64;
    let buf = ctx.args[2];
    let cred = vfs_cred_from_sched(&ctx.task.credentials());
    let manager = shm_manager();

    match cmd {
        IPC_STAT => {
            let meta = manager.stat(shmid, &cred)?;
            let raw = encode_shmid64_ds(&meta);
            copy_to_user(buf, &raw).map_err(|e| e.as_errno())?;
            Ok(0)
        }
        IPC_SET => {
            let mut raw = [0u8; SHMID64_DS_SIZE];
            copy_from_user(buf, &mut raw).map_err(|e| e.as_errno())?;
            let update = ShmMetadataUpdate {
                uid: Some(VfsUid(read_u32(&raw, 4))),
                gid: Some(VfsGid(read_u32(&raw, 8))),
                mode: Some(FileMode::new((read_u32(&raw, 20) as u16) & MODE_MASK)),
            };
            manager.set(shmid, update, &cred)?;
            manager.note_change(shmid, now_sec());
            Ok(0)
        }
        IPC_RMID => {
            manager.remove(shmid, &cred)?;
            manager.note_change(shmid, now_sec());
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

fn shm_manager() -> Arc<ShmManager> {
    let mut slot = SYSV_SHM_MANAGER.lock();
    if let Some(manager) = slot.as_ref() {
        return Arc::clone(manager);
    }
    let manager = Arc::new(ShmManager::default());
    *slot = Some(Arc::clone(&manager));
    manager
}

fn task_vm(ctx: &SyscallContext<'_>) -> Option<Arc<VmSpace>> {
    let payload = ctx.task.ext_lookup(sched::TASKEXT_VM_SPACE)?;
    payload.downcast::<VmSpace>().ok()
}

fn task_pid(ctx: &SyscallContext<'_>) -> i32 {
    ctx.task.pid_root().unwrap_or(0)
}

fn shmat_vm_flags(flags: u32) -> VmFlags {
    let mut vm_flags = VmFlags::EMPTY
        .with(VmFlags::READ)
        .with(VmFlags::SHARED)
        .with(VmFlags::USER);
    if flags & SHM_RDONLY == 0 {
        vm_flags = vm_flags.with(VmFlags::WRITE);
    }
    if flags & SHM_EXEC != 0 {
        vm_flags = vm_flags.with(VmFlags::EXEC);
    }
    vm_flags
}

fn vfs_cred_from_sched(src: &SchedCredentials) -> VfsCredentials {
    let mut caps = VfsCapSet::EMPTY;
    for (sched_cap, vfs_cap) in [
        (SchedCapability::Chown, VfsCapability::Chown),
        (SchedCapability::DacOverride, VfsCapability::DacOverride),
        (SchedCapability::DacReadSearch, VfsCapability::DacReadSearch),
        (SchedCapability::Fowner, VfsCapability::FOwner),
        (SchedCapability::Fsetid, VfsCapability::FSetId),
        (SchedCapability::SysBoot, VfsCapability::SysAdmin),
        (SchedCapability::SysResource, VfsCapability::SysAdmin),
    ] {
        if src.has_cap(sched_cap) {
            caps = caps.with(vfs_cap);
        }
    }

    VfsCredentials {
        uid: VfsUid(src.uid.0),
        euid: VfsUid(src.euid.0),
        suid: VfsUid(src.suid.0),
        fsuid: VfsUid(src.fsuid.0),
        gid: VfsGid(src.gid.0),
        egid: VfsGid(src.egid.0),
        sgid: VfsGid(src.sgid.0),
        fsgid: VfsGid(src.fsgid.0),
        groups: src
            .groups
            .iter()
            .map(|gid| VfsGid(gid.0))
            .collect::<Vec<_>>(),
        caps,
    }
}

fn now_sec() -> i64 {
    crate::vdso::clock_time_ns(0).unwrap_or(0) as i64 / 1_000_000_000
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    Some(value.checked_add(align - 1)? & !(align - 1))
}

fn read_u32(raw: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(raw[off..off + 4].try_into().unwrap())
}

fn write_u16(raw: &mut [u8], off: usize, val: u16) {
    raw[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn write_u32(raw: &mut [u8], off: usize, val: u32) {
    raw[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn write_i32(raw: &mut [u8], off: usize, val: i32) {
    raw[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn write_u64(raw: &mut [u8], off: usize, val: u64) {
    raw[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

fn write_i64(raw: &mut [u8], off: usize, val: i64) {
    raw[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

fn encode_shmid64_ds(meta: &ShmMetadata) -> [u8; SHMID64_DS_SIZE] {
    let mut raw = [0u8; SHMID64_DS_SIZE];

    // ipc64_perm：key/uid/gid/cuid/cgid/mode/seq 后接保留字段。字段偏移遵循
    // asm-generic 64 位布局，避免把 Rust 结构体 repr 泄漏到用户 ABI。
    write_i32(&mut raw, 0, meta.key.0);
    write_u32(&mut raw, 4, meta.perm.uid.0);
    write_u32(&mut raw, 8, meta.perm.gid.0);
    write_u32(&mut raw, 12, meta.perm.cuid.0);
    write_u32(&mut raw, 16, meta.perm.cgid.0);
    write_u32(&mut raw, 20, meta.perm.mode.bits() as u32);
    write_u16(&mut raw, 24, 0);

    // shmid64_ds：ipc64_perm 后依次是 size_t、三个 time64、两个 pid_t、
    // nattch 和保留字段。当前 manager 已维护有语义的字段，其余保持 0。
    write_u64(&mut raw, IPC64_PERM_SIZE, meta.size);
    write_i64(&mut raw, 56, meta.atime);
    write_i64(&mut raw, 64, meta.dtime);
    write_i64(&mut raw, 72, meta.ctime);
    write_i32(&mut raw, 80, meta.cpid);
    write_i32(&mut raw, 84, meta.lpid);
    write_u64(&mut raw, 88, meta.nattch as u64);
    raw
}
