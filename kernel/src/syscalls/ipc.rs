//! SysV IPC syscall glue.
//!
//! SysV shm 和 semaphore 的真实对象由 `general::ipc` 管理；本文件只做 Linux
//! asm-generic ABI 编解码、当前任务凭据转换、阻塞调度和 VM 映射操作。

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use errno::Errno;
use general::ipc::sem::{SEMOPM, SemId, SemKey, SemManager, SemOpAttempt, SemOperation};
use general::ipc::shm::{
    IPC_64, IPC_RMID, IPC_SET, IPC_STAT, SHM_EXEC, SHM_RDONLY, SHM_REMAP, SHM_RND, ShmId, ShmKey,
    ShmManager, ShmMetadata, ShmMetadataUpdate,
};
use general::mm::{VmSpace, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use mm::{FileLike, VmFlags};
use sched::sync::Spinlock;
use vfs::cred::{Gid as VfsGid, Uid as VfsUid};
use vfs::stat::FileMode;

use super::vfs_cred_from_sched;

const MODE_MASK: u16 = 0o777;
const SHMAT_KNOWN_FLAGS: u32 = SHM_RDONLY | SHM_RND | SHM_REMAP | SHM_EXEC;
const SEMBUF_SIZE: usize = 6;
const SEMCTL_GETVAL: u32 = 12;
const SEMCTL_SETVAL: u32 = 16;

// asm-generic 64-bit ABI:
// - `struct ipc64_perm` is 48 bytes.
// - `struct shmid64_ds` is 112 bytes.
// The kernel stores typed metadata in `general::ipc::shm`; only this ABI edge
// packs/unpacks the Linux byte layout.
const IPC64_PERM_SIZE: usize = 48;
const SHMID64_DS_SIZE: usize = 112;

static SYSV_SHM_MANAGER: Spinlock<Option<Arc<ShmManager>>> = Spinlock::new(None);
static SYSV_SEM_MANAGER: Spinlock<Option<Arc<SemManager>>> = Spinlock::new(None);

/// SysV shm 当前占用总字节数（`sysinfo` 的 `sharedram` 数据源）。
///
/// 管理器未初始化（尚未创建任何段）时返回 0。
pub(super) fn sysv_shm_total_bytes() -> u64 {
    let manager = {
        let guard = SYSV_SHM_MANAGER.lock();
        match guard.as_ref() {
            Some(m) => Arc::clone(m),
            None => return 0,
        }
    };
    let info = manager.info();
    info.total_pages as u64 * general::mm::page_size() as u64
}

pub(super) fn sys_shmget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let key = ShmKey(ctx.args[0] as i32);
    let size = ctx.args[1] as u64;
    let flags = ctx.args[2] as u32;
    let cred = vfs_cred_from_sched(&ctx.task().credentials());

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

    let cred = vfs_cred_from_sched(&ctx.task().credentials());
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
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
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

pub(super) fn sys_io_setup(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_io_destroy(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_io_submit(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_io_cancel(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_io_getevents(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_open(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_unlink(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_timedsend(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_timedreceive(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_notify(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_getsetattr(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_msgget(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_msgctl(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_msgrcv(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_msgsnd(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_semget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let key = SemKey(ctx.args[0] as i32);
    let nsems = ctx.args[1];
    let flags = ctx.args[2] as u32;
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let id = sem_manager().semget(key, nsems, flags, &cred)?;
    Ok(id.0 as usize)
}

pub(super) fn sys_semctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let id = SemId(ctx.args[0] as i32);
    let sem_num = ctx.args[1];
    let cmd = (ctx.args[2] as u32) & !IPC_64;
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let manager = sem_manager();

    match cmd {
        IPC_RMID => {
            let set = manager.remove(id, &cred)?;
            set.waiters().wake_all();
            Ok(0)
        }
        SEMCTL_GETVAL => Ok(manager.get_value(id, sem_num, &cred)? as usize),
        SEMCTL_SETVAL => {
            let value = ctx.args[3] as i32;
            let set = manager.set_value(id, sem_num, value, &cred)?;
            set.waiters().wake_all();
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sys_semtimedop(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timeout = ctx.args[3];
    sys_semop_common(ctx, Some(timeout))
}

pub(super) fn sys_semop(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_semop_common(ctx, None)
}

pub(super) fn sys_add_key(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_request_key(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_keyctl(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_io_pgetevents(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_io_pgetevents_time64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_timedsend_time64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_mq_timedreceive_time64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_semtimedop_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timeout = ctx.args[3];
    sys_semop_common(ctx, Some(timeout))
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

fn sem_manager() -> Arc<SemManager> {
    let mut slot = SYSV_SEM_MANAGER.lock();
    if let Some(manager) = slot.as_ref() {
        return Arc::clone(manager);
    }
    let manager = Arc::new(SemManager::default());
    *slot = Some(Arc::clone(&manager));
    manager
}

fn sys_semop_common(
    ctx: &mut SyscallContext<'_>,
    timeout_user: Option<usize>,
) -> Result<usize, Errno> {
    let id = SemId(ctx.args[0] as i32);
    let operations = read_sem_operations(ctx.args[1], ctx.args[2])?;
    let deadline = match timeout_user {
        Some(user) => read_sem_deadline(user)?,
        None => None,
    };
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let set = sem_manager().set_for_operation(id)?;
    let task = Arc::clone(ctx.task());

    loop {
        match set.try_apply(&operations, &cred)? {
            SemOpAttempt::Applied => {
                set.waiters().wake_all();
                return Ok(0);
            }
            SemOpAttempt::WouldBlock => {}
        }
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            return Err(Errno::EAGAIN);
        }

        let entry = set
            .waiters()
            .prepare_to_wait(&task, sched::TaskState::Sleeping);
        let deadline_armed = match deadline {
            Some(deadline) => {
                if !sched::register_sleep_deadline(&task, deadline) {
                    set.waiters().finish_wait(&entry);
                    return Err(Errno::EAGAIN);
                }
                true
            }
            None => false,
        };

        match set.try_apply(&operations, &cred) {
            Ok(SemOpAttempt::Applied) => {
                finish_sem_wait(&set, &entry, &task, deadline_armed);
                set.waiters().wake_all();
                return Ok(0);
            }
            Ok(SemOpAttempt::WouldBlock) => {}
            Err(error) => {
                finish_sem_wait(&set, &entry, &task, deadline_armed);
                return Err(error);
            }
        }
        if sched::operation::has_interrupting_signal(&task) {
            finish_sem_wait(&set, &entry, &task, deadline_armed);
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            finish_sem_wait(&set, &entry, &task, deadline_armed);
            return Err(Errno::EAGAIN);
        }

        sched::schedule_once(sched::now_ns_direct());
        finish_sem_wait(&set, &entry, &task, deadline_armed);
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            return Err(Errno::EAGAIN);
        }
    }
}

fn finish_sem_wait(
    set: &general::ipc::sem::SemSet,
    entry: &Arc<sched::WaitQueueEntry>,
    task: &Arc<sched::Task>,
    deadline_armed: bool,
) {
    if deadline_armed {
        sched::cancel_sleep_deadline(task);
    }
    set.waiters().finish_wait(entry);
}

fn read_sem_operations(user: usize, count: usize) -> Result<Vec<SemOperation>, Errno> {
    if count == 0 {
        return Err(Errno::EINVAL);
    }
    if count > SEMOPM {
        return Err(Errno::E2BIG);
    }
    let byte_len = count.checked_mul(SEMBUF_SIZE).ok_or(Errno::E2BIG)?;
    let mut raw = vec![0u8; byte_len];
    copy_from_user(user, &mut raw).map_err(|error| error.as_errno())?;

    let mut operations = Vec::with_capacity(count);
    for bytes in raw.chunks_exact(SEMBUF_SIZE) {
        operations.push(SemOperation {
            sem_num: u16::from_le_bytes(bytes[0..2].try_into().unwrap()),
            sem_op: i16::from_le_bytes(bytes[2..4].try_into().unwrap()),
            sem_flg: u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
        });
    }
    Ok(operations)
}

fn read_sem_deadline(user: usize) -> Result<Option<u64>, Errno> {
    if user == 0 {
        return Ok(None);
    }
    let mut raw = [0u8; 16];
    copy_from_user(user, &mut raw).map_err(|error| error.as_errno())?;
    let seconds = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nanoseconds = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(Errno::EINVAL);
    }
    let duration = (seconds as u64)
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds as u64))
        .ok_or(Errno::EINVAL)?;
    Ok(Some(sched::now_ns_direct().saturating_add(duration)))
}

fn task_vm(ctx: &SyscallContext<'_>) -> Option<Arc<VmSpace>> {
    let payload = ctx.task().ext_lookup(sched::TASKEXT_VM_SPACE)?;
    payload.downcast::<VmSpace>().ok()
}

fn task_pid(ctx: &SyscallContext<'_>) -> i32 {
    ctx.task().pid_root().unwrap_or(0)
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

#[cfg(feature = "kernel-tests")]
mod tests {
    use general::ipc::sem::{IPC_NOWAIT, SemManager, SemOpAttempt, SemOperation};
    use general::ipc::shm::{IPC_CREAT, IPC_EXCL};
    use ktest::ktest;
    use vfs::cred::Credentials;

    use super::SemKey;

    fn operation(sem_num: u16, sem_op: i16, sem_flg: u16) -> SemOperation {
        SemOperation {
            sem_num,
            sem_op,
            sem_flg,
        }
    }

    #[ktest]
    fn semaphore_batch_is_atomic() {
        let manager = SemManager::new();
        let cred = Credentials::root();
        let id = manager
            .semget(SemKey::PRIVATE, 2, IPC_CREAT | 0o700, &cred)
            .expect("创建 semaphore set");
        let set = manager.set_for_operation(id).expect("查找 semaphore set");

        assert_eq!(
            set.try_apply(&[operation(0, 1, 0), operation(0, -2, 0)], &cred),
            Ok(SemOpAttempt::WouldBlock)
        );
        assert_eq!(manager.get_value(id, 0, &cred), Ok(0));

        assert_eq!(
            set.try_apply(&[operation(0, 2, 0), operation(0, -1, 0)], &cred),
            Ok(SemOpAttempt::Applied)
        );
        assert_eq!(manager.get_value(id, 0, &cred), Ok(1));
    }

    #[ktest]
    fn semaphore_nowait_does_not_change_value() {
        let manager = SemManager::new();
        let cred = Credentials::root();
        let id = manager
            .semget(SemKey::PRIVATE, 1, IPC_CREAT | 0o700, &cred)
            .expect("创建 semaphore set");
        let set = manager.set_for_operation(id).expect("查找 semaphore set");

        assert_eq!(
            set.try_apply(&[operation(0, -1, IPC_NOWAIT)], &cred),
            Err(errno::Errno::EAGAIN)
        );
        assert_eq!(manager.get_value(id, 0, &cred), Ok(0));
    }

    #[ktest]
    fn semaphore_key_and_removal_lifecycle() {
        let manager = SemManager::new();
        let cred = Credentials::root();
        let key = SemKey(0x1234);
        let id = manager
            .semget(key, 2, IPC_CREAT | 0o700, &cred)
            .expect("创建 keyed semaphore set");

        assert_eq!(manager.semget(key, 0, 0, &cred), Ok(id));
        assert_eq!(
            manager.semget(key, 2, IPC_CREAT | IPC_EXCL | 0o700, &cred),
            Err(errno::Errno::EEXIST)
        );

        let stable_set = manager.set_for_operation(id).expect("保留稳定对象引用");
        manager.remove(id, &cred).expect("删除 semaphore set");
        assert!(matches!(
            stable_set.try_apply(&[operation(0, 1, 0)], &cred),
            Err(errno::Errno::EIDRM)
        ));
        assert!(matches!(
            manager.set_for_operation(id),
            Err(errno::Errno::EINVAL)
        ));
    }
}
