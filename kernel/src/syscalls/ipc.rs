//! SysV IPC syscall glue.
//!
//! SysV shm 和 semaphore 的真实对象由 `general::ipc` 管理；本文件只做 Linux
//! asm-generic ABI 编解码、当前任务凭据转换、阻塞调度和 VM 映射操作。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;
use general::ipc::keys::{
    KEY_DEFAULT_PERM, KEY_REQKEY_DEFL_DEFAULT, KEY_REQKEY_DEFL_NO_CHANGE,
    KEY_REQKEY_DEFL_PROCESS_KEYRING, KEY_REQKEY_DEFL_REQUESTOR_KEYRING,
    KEY_REQKEY_DEFL_SESSION_KEYRING, KEY_REQKEY_DEFL_THREAD_KEYRING, KEY_REQKEY_DEFL_USER_KEYRING,
    KEY_REQKEY_DEFL_USER_SESSION_KEYRING, KEY_SPEC_REQKEY_AUTH_KEY, KEY_SPEC_SESSION_KEYRING,
    KeyId, KeyManager, KeyState, KeyType, ProcessKeyrings,
};
use general::ipc::mqueue::{
    MQ_ATTR_CURMSGS, MQ_ATTR_FLAGS, MQ_ATTR_MAXMSG, MQ_ATTR_MSGSIZE, MQ_ATTR_SIZE, MQ_NAME_MAX,
    MqAttr, MqNotifyKind, SI_MESGQ, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD,
};
use general::ipc::msg::{
    MSG_COPY, MSG_EXCEPT, MSG_INFO, MSG_NOERROR, MSG_STAT, MSG_STAT_ANY, MSG_TRUNC, MSGMAX, MSGMNB,
    MSGMNI, MsgId, MsgKey, MsgManager, MsgMetadata, MsgOpAttempt, MsgRecvOutcome, MsgSystemInfo,
};
use general::ipc::sem::{
    SEM_INFO, SEM_STAT, SEM_STAT_ANY, SEM_UNDO, SEMCTL_GETALL, SEMCTL_GETNCNT, SEMCTL_GETPID,
    SEMCTL_GETVAL, SEMCTL_GETZCNT, SEMCTL_SETALL, SEMCTL_SETVAL, SEMOPM, SemBlockKind, SemId,
    SemKey, SemManager, SemMetadata, SemOpAttempt, SemOperation, SemSystemInfo,
};
use general::ipc::sem_undo::SemUndoTable;
use general::ipc::shm::{
    IPC_64, IPC_INFO, IPC_RMID, IPC_SET, IPC_STAT, SHM_EXEC, SHM_INFO, SHM_LOCK, SHM_LOCKED,
    SHM_RDONLY, SHM_REMAP, SHM_RND, SHM_STAT, SHM_STAT_ANY, SHM_UNLOCK, ShmId, ShmKey, ShmManager,
    ShmMetadata, ShmMetadataUpdate, ShmSystemInfo,
};
use general::mm::{VmSpace, copy_cstr_from_user, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::current_fdtable;
use general::vfs::mqueue::{MqFileOps, dispatch_mq_notification, mq_registry, open_mq_fd};
use mm::{FileLike, VmFlags};
use sched::sync::Spinlock;
use vfs::cred::{Credentials as VfsCredentials, Gid as VfsGid, Uid as VfsUid};
use vfs::fdtable::FdFlags;
use vfs::file::{AccessMode, OpenOptions};
use vfs::stat::FileMode;

use super::vfs_cred_from_sched;

const MODE_MASK: u16 = 0o777;
const SHMAT_KNOWN_FLAGS: u32 = SHM_RDONLY | SHM_RND | SHM_REMAP | SHM_EXEC;
const SEMBUF_SIZE: usize = 6;

// asm-generic 64-bit ABI:
// - `struct ipc64_perm` is 48 bytes.
// - `struct shmid64_ds` is 112 bytes.
// - `struct msqid64_ds` is 120 bytes.
// - `struct semid64_ds` is 96 bytes.
// The kernel stores typed metadata in `general::ipc`; only this ABI edge
// packs/unpacks the Linux byte layout.
const IPC64_PERM_SIZE: usize = 48;
const SHMID64_DS_SIZE: usize = 112;
const MSQID64_DS_SIZE: usize = 120;
const SEMID64_DS_SIZE: usize = 96;
const MSGINFO_SIZE: usize = 32;
const SEMINFO_SIZE: usize = 40;

// Linux `ipc/sem.c` 默认限制，用于 `IPC_INFO` 的 `struct seminfo`。
const SEMMNI_LIMIT: i32 = 32_000;
const SEMMSL_LIMIT: i32 = 32_000;
const SEMMNS_LIMIT: i32 = SEMMNI_LIMIT * SEMMSL_LIMIT;
const SEMOPM_LIMIT: i32 = 500;
const SEMVMX_LIMIT: i32 = 32_767;
const SEMAEM_LIMIT: i32 = SEMVMX_LIMIT;
const SEMUME_LIMIT: i32 = SEMOPM_LIMIT;
const SEMUSZ_LIMIT: i32 = 20;

// Linux `ipc/msg.c` 默认限制，用于 `IPC_INFO` 的 `struct msginfo`。
const MSGPOOL: i32 = 8192;
const MSGMAP: i32 = 8192;
const MSGSSZ: i32 = 8;
const MSGTQL: i32 = 16384;
const MSGSEG: u16 = 0xffff;

/// 每个任务/进程的 `SEM_UNDO` 撤销表（`Arc<SemUndoTable>`）。
pub(crate) const TASKEXT_SEM_UNDO: sched::TaskExtKey = 0x0004_0001;

/// 每个任务/进程的 keyring 引用（`Arc<ProcessKeyrings>`）。
pub(crate) const TASKEXT_KEYRINGS: sched::TaskExtKey = 0x0004_0002;

/// key 描述符/类型的最大长度。
const KEY_DESC_MAX: usize = 4096;

static SYSV_SHM_MANAGER: Spinlock<Option<Arc<ShmManager>>> = Spinlock::new(None);
static SYSV_SEM_MANAGER: Spinlock<Option<Arc<SemManager>>> = Spinlock::new(None);
static SYSV_MSG_MANAGER: Spinlock<Option<Arc<MsgManager>>> = Spinlock::new(None);
static SYSV_KEYS_MANAGER: Spinlock<Option<Arc<KeyManager>>> = Spinlock::new(None);

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

    let manager = Arc::clone(&task_ipc(ctx).shm);
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
    let manager = Arc::clone(&task_ipc(ctx).shm);
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
    Arc::clone(&task_ipc(ctx).shm).note_detach(ShmId(shmid_raw), task_pid(ctx), now_sec());
    Ok(0)
}

pub(super) fn sys_shmctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let shmid = ShmId(ctx.args[0] as i32);
    let raw_cmd = ctx.args[1] as u32;
    let cmd = raw_cmd & !IPC_64;
    let buf = ctx.args[2];
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let manager = Arc::clone(&task_ipc(ctx).shm);

    match cmd {
        IPC_STAT => {
            let meta = manager.stat(shmid, &cred)?;
            let raw = encode_shmid64_ds(&meta);
            copy_to_user(buf, &raw).map_err(|e| e.as_errno())?;
            Ok(0)
        }
        SHM_STAT | SHM_STAT_ANY => {
            let (found_id, meta) = manager.stat_by_index(shmid.0, &cred, cmd == SHM_STAT)?;
            let raw = encode_shmid64_ds(&meta);
            copy_to_user(buf, &raw).map_err(|e| e.as_errno())?;
            Ok(found_id.0 as usize)
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
        SHM_LOCK | SHM_UNLOCK => {
            let lock = cmd == SHM_LOCK;
            if lock {
                // Linux `user_shm_lock`：锁定新增页数不得超出 `RLIMIT_MEMLOCK`
                //（`CAP_IPC_LOCK` 或 `RLIM_INFINITY` 豁免）。超限返回 `ENOMEM`。
                let additional = manager.would_lock_pages(shmid)?;
                check_shm_memlock_limit(ctx, manager.locked_pages(), additional)?;
            }
            manager.lock(shmid, lock, &cred)?;
            Ok(0)
        }
        IPC_INFO => {
            let info = manager.info();
            let raw = encode_shminfo(&info);
            copy_to_user(buf, &raw).map_err(|e| e.as_errno())?;
            Ok(info.max_index as usize)
        }
        SHM_INFO => {
            let info = manager.info();
            let raw = encode_shm_info(&info);
            copy_to_user(buf, &raw).map_err(|e| e.as_errno())?;
            Ok(info.max_index as usize)
        }
        _ => Err(Errno::EINVAL),
    }
}

// ── libaio（最小同步实现）──────────────────────────────────────────────────
//
// 本内核没有真正的异步 I/O 执行队列：`io_submit` 同步执行每个 iocb
// （pread/pwrite/fsync/fdatasync 子集），并把完成事件写入 `io_getevents` 可读
// 的事件队列。ABI 对齐 asm-generic 的 `struct iocb`（64 字节）与
// `struct io_event`（32 字节）。

const IOCB_SIZE: usize = 64;
const IOEVENT_SIZE: usize = 32;
const AIO_MAX_EVENTS_PER_CTX: usize = 4096;
const AIO_MAX_CONTEXTS: usize = 64;
/// 单个 iocb 的最大负载字节数（Linux `MAX_RW_COUNT` 的保守近似）。
const AIO_MAX_NBYTES: usize = 32 * 1024 * 1024;

// `struct iocb` 字段偏移（asm-generic 64 位布局）。
const IOCB_AIO_DATA: usize = 0;
const IOCB_AIO_LIO_OPCODE: usize = 16;
const IOCB_AIO_FILDES: usize = 20;
const IOCB_AIO_BUF: usize = 24;
const IOCB_AIO_NBYTES: usize = 32;
const IOCB_AIO_OFFSET: usize = 40;

// `struct io_event` 字段偏移。
const IOEVENT_DATA: usize = 0;
const IOEVENT_OBJ: usize = 8;
const IOEVENT_RES: usize = 16;
const IOEVENT_RES2: usize = 24;

// `aio_lio_opcode`（Linux `linux/aio_abi.h` 子集）。
const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_FSYNC: u16 = 2;
const IOCB_CMD_FDSYNC: u16 = 3;

/// 一条已完成的 AIO 事件（`struct io_event`）。
#[derive(Clone, Copy)]
struct AioEvent {
    data: u64,
    obj: u64,
    res: i64,
    res2: i64,
}

struct AioContext {
    completed: Spinlock<VecDeque<AioEvent>>,
}

static AIO_CONTEXTS: Spinlock<Option<BTreeMap<u64, Arc<AioContext>>>> = Spinlock::new(None);
static AIO_NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn aio_context(id: u64) -> Result<Arc<AioContext>, Errno> {
    let slot = AIO_CONTEXTS.lock();
    let Some(map) = slot.as_ref() else {
        return Err(Errno::EINVAL);
    };
    map.get(&id).cloned().ok_or(Errno::EINVAL)
}

fn aio_err(data: u64, obj: u64, error: Errno) -> AioEvent {
    AioEvent {
        data,
        obj,
        res: -(error.as_i32_internal() as i64),
        res2: 0,
    }
}

fn aio_file(fd: u32) -> Result<Arc<vfs::file::File>, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    fdt.get_file(vfs::fdtable::Fd::from_raw(fd))
        .ok_or(Errno::EBADF)
}

pub(super) fn sys_io_setup(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let nr_events = ctx.args[0];
    let ctx_idp = ctx.args[1];
    if nr_events == 0 || nr_events > AIO_MAX_EVENTS_PER_CTX {
        return Err(Errno::EINVAL);
    }
    if ctx_idp == 0 {
        return Err(Errno::EFAULT);
    }
    let id = AIO_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let context = Arc::new(AioContext {
        completed: Spinlock::new(VecDeque::new()),
    });
    {
        let mut slot = AIO_CONTEXTS.lock();
        if slot.is_none() {
            *slot = Some(BTreeMap::new());
        }
        let map = slot.as_mut().unwrap();
        if map.len() >= AIO_MAX_CONTEXTS {
            return Err(Errno::EAGAIN);
        }
        map.insert(id, Arc::clone(&context));
    }
    copy_to_user(ctx_idp, &id.to_le_bytes()).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_io_destroy(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let id = ctx.args[0] as u64;
    let mut slot = AIO_CONTEXTS.lock();
    let Some(map) = slot.as_mut() else {
        return Err(Errno::EINVAL);
    };
    if map.remove(&id).is_none() {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

pub(super) fn sys_io_submit(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let id = ctx.args[0] as u64;
    let nr = ctx.args[1];
    let iocbpp = ctx.args[2];
    let context = aio_context(id)?;
    if nr == 0 {
        return Ok(0);
    }
    if nr > AIO_MAX_EVENTS_PER_CTX {
        return Err(Errno::EINVAL);
    }
    if iocbpp == 0 {
        return Err(Errno::EFAULT);
    }
    let mut submitted = 0usize;
    for index in 0..nr {
        match submit_one(&context, iocbpp, index) {
            Ok(()) => submitted += 1,
            Err(error) => {
                return if submitted > 0 {
                    Ok(submitted)
                } else {
                    Err(error)
                };
            }
        }
    }
    Ok(submitted)
}

pub(super) fn sys_io_cancel(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // 同步执行：iocb 在 io_submit 内立即完成，永远没有"进行中"的请求可取消。
    let id = ctx.args[0] as u64;
    let _ = aio_context(id)?;
    Err(Errno::EAGAIN)
}

pub(super) fn sys_io_getevents(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_io_getevents_common(ctx)
}

/// `io_getevents`/`io_pgetevents` 共用：从完成事件队列取出最多 `nr` 条事件。
///
/// 由于本内核的 iocb 在 `io_submit` 内同步完成，事件提交后立即可读；若可用
/// 事件少于 `min_nr`，这里直接返回当前可用数（不阻塞），这是最小同步实现与
/// Linux 异步等待语义的取舍。
fn sys_io_getevents_common(ctx: &SyscallContext<'_>) -> Result<usize, Errno> {
    let id = ctx.args[0] as u64;
    let nr = ctx.args[2];
    let events_user = ctx.args[3];
    let context = aio_context(id)?;
    if nr == 0 {
        return Ok(0);
    }
    if events_user == 0 {
        return Err(Errno::EFAULT);
    }
    let drained: Vec<AioEvent> = {
        let mut completed = context.completed.lock();
        let count = nr.min(completed.len());
        completed.drain(..count).collect()
    };
    for (index, event) in drained.iter().enumerate() {
        let mut raw = [0u8; IOEVENT_SIZE];
        write_u64(&mut raw, IOEVENT_DATA, event.data);
        write_u64(&mut raw, IOEVENT_OBJ, event.obj);
        write_i64(&mut raw, IOEVENT_RES, event.res);
        write_i64(&mut raw, IOEVENT_RES2, event.res2);
        let addr = events_user
            .checked_add(index * IOEVENT_SIZE)
            .ok_or(Errno::EFAULT)?;
        copy_to_user(addr, &raw).map_err(|e| e.as_errno())?;
    }
    Ok(drained.len())
}

/// 解析并同步执行一条 iocb，把完成事件写入所属 context。
fn submit_one(context: &Arc<AioContext>, iocbpp: usize, index: usize) -> Result<(), Errno> {
    let ptr_addr = iocbpp
        .checked_add(index * size_of::<usize>())
        .ok_or(Errno::EFAULT)?;
    let mut ptr_raw = [0u8; size_of::<usize>()];
    copy_from_user(ptr_addr, &mut ptr_raw).map_err(|e| e.as_errno())?;
    let iocb_user = usize::from_le_bytes(ptr_raw);
    if iocb_user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; IOCB_SIZE];
    copy_from_user(iocb_user, &mut raw).map_err(|e| e.as_errno())?;
    let event = aio_execute(iocb_user, &raw);
    context.completed.lock().push_back(event);
    Ok(())
}

fn aio_execute(iocb_user: usize, raw: &[u8; IOCB_SIZE]) -> AioEvent {
    let data = read_u64(raw, IOCB_AIO_DATA);
    let obj = iocb_user as u64;
    let opcode = read_u16(raw, IOCB_AIO_LIO_OPCODE);
    let fd = read_u32(raw, IOCB_AIO_FILDES);
    let buf = read_u64(raw, IOCB_AIO_BUF) as usize;
    let nbytes = read_u64(raw, IOCB_AIO_NBYTES) as usize;
    let offset = read_i64(raw, IOCB_AIO_OFFSET);

    match opcode {
        IOCB_CMD_PREAD => aio_pread(data, obj, fd, buf, nbytes, offset),
        IOCB_CMD_PWRITE => aio_pwrite(data, obj, fd, buf, nbytes, offset),
        IOCB_CMD_FSYNC => aio_fsync(data, obj, fd, false),
        IOCB_CMD_FDSYNC => aio_fsync(data, obj, fd, true),
        _ => aio_err(data, obj, Errno::EINVAL),
    }
}

fn aio_pread(data: u64, obj: u64, fd: u32, buf: usize, nbytes: usize, offset: i64) -> AioEvent {
    if offset < 0 || nbytes > AIO_MAX_NBYTES {
        return aio_err(data, obj, Errno::EINVAL);
    }
    let file = match aio_file(fd) {
        Ok(file) => file,
        Err(error) => return aio_err(data, obj, error),
    };
    let mut tmp = vec![0u8; nbytes];
    let res = match file.read_at(&mut tmp, offset as u64) {
        Ok(n) => n as i64,
        Err(error) => -(error.to_errno().as_i32_internal() as i64),
    };
    if res > 0 {
        if let Err(error) = copy_to_user(buf, &tmp[..res as usize]) {
            return aio_err(data, obj, error.as_errno());
        }
    }
    AioEvent {
        data,
        obj,
        res,
        res2: 0,
    }
}

fn aio_pwrite(data: u64, obj: u64, fd: u32, buf: usize, nbytes: usize, offset: i64) -> AioEvent {
    if offset < 0 || nbytes > AIO_MAX_NBYTES {
        return aio_err(data, obj, Errno::EINVAL);
    }
    let file = match aio_file(fd) {
        Ok(file) => file,
        Err(error) => return aio_err(data, obj, error),
    };
    let mut tmp = vec![0u8; nbytes];
    if nbytes > 0 {
        if let Err(error) = copy_from_user(buf, &mut tmp) {
            return aio_err(data, obj, error.as_errno());
        }
    }
    let res = match file.write_at(&tmp, offset as u64) {
        Ok(n) => n as i64,
        Err(error) => -(error.to_errno().as_i32_internal() as i64),
    };
    AioEvent {
        data,
        obj,
        res,
        res2: 0,
    }
}

fn aio_fsync(data: u64, obj: u64, fd: u32, fdatasync: bool) -> AioEvent {
    let file = match aio_file(fd) {
        Ok(file) => file,
        Err(error) => return aio_err(data, obj, error),
    };
    let result = if fdatasync {
        file.datasync()
    } else {
        file.sync()
    };
    let res = match result {
        Ok(()) => 0,
        Err(error) => -(error.to_errno().as_i32_internal() as i64),
    };
    AioEvent {
        data,
        obj,
        res,
        res2: 0,
    }
}

pub(super) fn sys_mq_open(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const O_ACCMODE: usize = 0o3;
    const O_CREAT: usize = 0o100;
    const O_EXCL: usize = 0o200;
    const O_NONBLOCK: usize = 0o4000;
    const O_CLOEXEC: usize = 0o2000000;

    let name_user = ctx.args[0];
    let oflag = ctx.args[1];
    let mode = ctx.args[2] as u16;
    let attr_user = ctx.args[3];
    if name_user == 0 {
        return Err(Errno::EFAULT);
    }
    let name = copy_cstr_from_user(name_user, MQ_NAME_MAX).map_err(|e| e.as_errno())?;
    if oflag & !(O_ACCMODE | O_CREAT | O_EXCL | O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let access = match oflag & O_ACCMODE {
        0 => AccessMode::ReadOnly,
        1 => AccessMode::WriteOnly,
        2 => AccessMode::ReadWrite,
        _ => return Err(Errno::EINVAL),
    };
    let attr = if attr_user != 0 {
        let mut raw = [0u8; MQ_ATTR_SIZE];
        copy_from_user(attr_user, &mut raw).map_err(|e| e.as_errno())?;
        Some(MqAttr {
            maxmsg: read_i64(&raw, MQ_ATTR_MAXMSG),
            msgsize: read_i64(&raw, MQ_ATTR_MSGSIZE),
            curmsgs: 0,
        })
    } else {
        None
    };
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    // Linux `mq_open`：创建队列时用 `mode` 经 current umask 掩码作为权限；
    // 查找已有队列时 mode 被忽略。
    let perm_mode = general::vfs::current_vfs_context()
        .map(|ctx| ctx.apply_umask(FileMode::new(mode & MODE_MASK)))
        .unwrap_or_else(|| FileMode::new(mode & MODE_MASK));
    let queue = mq_registry().open(
        &name,
        oflag & O_CREAT != 0,
        oflag & O_EXCL != 0,
        attr.as_ref(),
        perm_mode,
        &cred,
    )?;
    if access != AccessMode::WriteOnly {
        queue.check_access(false, &cred)?;
    }
    if access != AccessMode::ReadOnly {
        queue.check_access(true, &cred)?;
    }

    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file_flags = OpenOptions {
        access,
        nonblock: oflag & O_NONBLOCK != 0,
        ..Default::default()
    };
    let fd_flags = if oflag & O_CLOEXEC != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    vfs::anon::create_fd(
        &fdt,
        Arc::new(cred),
        file_flags,
        fd_flags,
        Box::new(open_mq_fd(queue, oflag & O_NONBLOCK != 0)),
    )
    .map_err(|e| e.to_errno())
    .map(|fd| fd.as_raw() as usize)
}

pub(super) fn sys_mq_unlink(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let name_user = ctx.args[0];
    if name_user == 0 {
        return Err(Errno::EFAULT);
    }
    let name = copy_cstr_from_user(name_user, MQ_NAME_MAX).map_err(|e| e.as_errno())?;
    mq_registry().unlink(&name)?;
    Ok(0)
}

pub(super) fn sys_mq_timedsend(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let mqdes = vfs::fdtable::Fd::from_raw(ctx.args[0] as u32);
    let msg_ptr = ctx.args[1];
    let msg_len = ctx.args[2];
    let msg_prio = ctx.args[3] as u32;
    let timeout = ctx.args[4];
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt.get_file(mqdes).ok_or(Errno::EBADF)?;
    let ops = file.downcast_ops::<MqFileOps>().ok_or(Errno::EBADF)?;
    let queue: Arc<general::ipc::mqueue::MqObject> = Arc::clone(ops.queue());

    let mut data = vec![0u8; msg_len];
    if msg_len > 0 {
        copy_from_user(msg_ptr, &mut data).map_err(|e| e.as_errno())?;
    }
    let deadline = read_mq_deadline(timeout)?;
    let nonblock = file.flags().nonblock;
    let task = Arc::clone(ctx.task());
    let pid = task_pid(ctx);

    loop {
        match queue.try_send(msg_prio, &data, pid, nonblock) {
            Ok((true, notify)) => {
                if let Some(notification) = notify {
                    dispatch_mq_notification(&notification);
                }
                return Ok(0);
            }
            Ok((false, _)) => {}
            Err(error) => return Err(error),
        }
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            return Err(Errno::ETIMEDOUT);
        }

        let entry = queue
            .senders()
            .prepare_to_wait(&task, sched::TaskState::Sleeping);
        let deadline_armed = match deadline {
            Some(deadline) => {
                if !sched::register_sleep_deadline(&task, deadline) {
                    queue.senders().finish_wait(&entry);
                    return Err(Errno::ETIMEDOUT);
                }
                true
            }
            None => false,
        };
        match queue.try_send(msg_prio, &data, pid, nonblock) {
            Ok((true, notify)) => {
                queue.senders().finish_wait(&entry);
                if deadline_armed {
                    sched::cancel_sleep_deadline(&task);
                }
                if let Some(notification) = notify {
                    dispatch_mq_notification(&notification);
                }
                return Ok(0);
            }
            Ok((false, _)) => {}
            Err(error) => {
                queue.senders().finish_wait(&entry);
                if deadline_armed {
                    sched::cancel_sleep_deadline(&task);
                }
                return Err(error);
            }
        }
        if sched::operation::has_interrupting_signal(&task) {
            queue.senders().finish_wait(&entry);
            if deadline_armed {
                sched::cancel_sleep_deadline(&task);
            }
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            queue.senders().finish_wait(&entry);
            if deadline_armed {
                sched::cancel_sleep_deadline(&task);
            }
            return Err(Errno::ETIMEDOUT);
        }
        sched::schedule_once(sched::now_ns_direct());
        queue.senders().finish_wait(&entry);
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
    }
}

pub(super) fn sys_mq_timedreceive(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let mqdes = vfs::fdtable::Fd::from_raw(ctx.args[0] as u32);
    let msg_ptr = ctx.args[1];
    let msg_len = ctx.args[2];
    let msg_prio_user = ctx.args[3];
    let timeout = ctx.args[4];
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt.get_file(mqdes).ok_or(Errno::EBADF)?;
    let ops = file.downcast_ops::<MqFileOps>().ok_or(Errno::EBADF)?;
    let queue: Arc<general::ipc::mqueue::MqObject> = Arc::clone(ops.queue());

    let deadline = read_mq_deadline(timeout)?;
    let nonblock = file.flags().nonblock;
    let task = Arc::clone(ctx.task());

    loop {
        match queue.try_receive(msg_len, nonblock) {
            Ok(Some(message)) => {
                copy_to_user(msg_ptr, &message.data).map_err(|e| e.as_errno())?;
                if msg_prio_user != 0 {
                    copy_to_user(msg_prio_user, &message.priority.to_le_bytes())
                        .map_err(|e| e.as_errno())?;
                }
                return Ok(message.data.len());
            }
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            return Err(Errno::ETIMEDOUT);
        }

        let entry = queue
            .receivers()
            .prepare_to_wait(&task, sched::TaskState::Sleeping);
        let deadline_armed = match deadline {
            Some(deadline) => {
                if !sched::register_sleep_deadline(&task, deadline) {
                    queue.receivers().finish_wait(&entry);
                    return Err(Errno::ETIMEDOUT);
                }
                true
            }
            None => false,
        };
        match queue.try_receive(msg_len, nonblock) {
            Ok(Some(message)) => {
                queue.receivers().finish_wait(&entry);
                if deadline_armed {
                    sched::cancel_sleep_deadline(&task);
                }
                copy_to_user(msg_ptr, &message.data).map_err(|e| e.as_errno())?;
                if msg_prio_user != 0 {
                    copy_to_user(msg_prio_user, &message.priority.to_le_bytes())
                        .map_err(|e| e.as_errno())?;
                }
                return Ok(message.data.len());
            }
            Ok(None) => {}
            Err(error) => {
                queue.receivers().finish_wait(&entry);
                if deadline_armed {
                    sched::cancel_sleep_deadline(&task);
                }
                return Err(error);
            }
        }
        if sched::operation::has_interrupting_signal(&task) {
            queue.receivers().finish_wait(&entry);
            if deadline_armed {
                sched::cancel_sleep_deadline(&task);
            }
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            queue.receivers().finish_wait(&entry);
            if deadline_armed {
                sched::cancel_sleep_deadline(&task);
            }
            return Err(Errno::ETIMEDOUT);
        }
        sched::schedule_once(sched::now_ns_direct());
        queue.receivers().finish_wait(&entry);
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
    }
}

pub(super) fn sys_mq_notify(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const SIGEV_SIZE: usize = 48;
    let mqdes = vfs::fdtable::Fd::from_raw(ctx.args[0] as u32);
    let notification = ctx.args[1];
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt.get_file(mqdes).ok_or(Errno::EBADF)?;
    let ops = file.downcast_ops::<MqFileOps>().ok_or(Errno::EBADF)?;
    let queue: Arc<general::ipc::mqueue::MqObject> = Arc::clone(ops.queue());
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let pid = task_pid(ctx);

    if notification == 0 {
        // 取消注册（SIGEV_NONE 语义）。
        return queue.register_notify(MqNotifyKind::None, 0, 0).map(|_| 0);
    }

    let mut raw = [0u8; SIGEV_SIZE];
    copy_from_user(notification, &mut raw).map_err(|e| e.as_errno())?;
    let sigev_value = read_i64(&raw, 0) as usize;
    let sigev_signo = read_i32(&raw, 8);
    let sigev_notify = read_i32(&raw, 12);
    let kind = match sigev_notify {
        SIGEV_NONE => MqNotifyKind::None,
        SIGEV_SIGNAL => MqNotifyKind::Signal {
            signo: sigev_signo,
            value: sigev_value,
        },
        SIGEV_THREAD => {
            // `_sigev_un._sigev_thread._function` 在 union 起始（offset 16）。
            let function = read_u64(&raw, 16) as usize;
            MqNotifyKind::Thread {
                function,
                value: sigev_value,
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    if kind == MqNotifyKind::None {
        return queue.register_notify(MqNotifyKind::None, 0, 0).map(|_| 0);
    }
    // Linux `ipc/mqueue.c`：注册通知要求读权限（ipcperms）。
    queue.check_access(false, &cred)?;
    queue.register_notify(kind, pid, cred.uid.0).map(|_| 0)
}

pub(super) fn sys_mq_getsetattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const O_NONBLOCK: i64 = 0o4000;
    let mqdes = vfs::fdtable::Fd::from_raw(ctx.args[0] as u32);
    let newattr_user = ctx.args[1];
    let oldattr_user = ctx.args[2];
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let file = fdt.get_file(mqdes).ok_or(Errno::EBADF)?;
    let ops = file.downcast_ops::<MqFileOps>().ok_or(Errno::EBADF)?;
    let queue: Arc<general::ipc::mqueue::MqObject> = Arc::clone(ops.queue());
    let cred = vfs_cred_from_sched(&ctx.task().credentials());

    let old = queue.attr();
    let old_flags = if file.flags().nonblock { O_NONBLOCK } else { 0 };

    if newattr_user != 0 {
        queue.check_access(false, &cred)?;
        let mut raw = [0u8; MQ_ATTR_SIZE];
        copy_from_user(newattr_user, &mut raw).map_err(|e| e.as_errno())?;
        let flags = read_i64(&raw, MQ_ATTR_FLAGS);
        let maxmsg = read_i64(&raw, MQ_ATTR_MAXMSG);
        let msgsize = read_i64(&raw, MQ_ATTR_MSGSIZE);
        if flags & !O_NONBLOCK != 0 {
            return Err(Errno::EINVAL);
        }
        if maxmsg != old.maxmsg || msgsize != old.msgsize {
            queue.set_attr(&MqAttr {
                maxmsg,
                msgsize,
                curmsgs: 0,
            })?;
        }
        if flags != old_flags {
            file.set_status_flags(false, flags & O_NONBLOCK != 0, false, false);
        }
    }

    if oldattr_user != 0 {
        let current = queue.attr();
        let flags = if file.flags().nonblock { O_NONBLOCK } else { 0 };
        let mut raw = [0u8; MQ_ATTR_SIZE];
        write_i64(&mut raw, MQ_ATTR_FLAGS, flags);
        write_i64(&mut raw, MQ_ATTR_MAXMSG, current.maxmsg);
        write_i64(&mut raw, MQ_ATTR_MSGSIZE, current.msgsize);
        write_i64(&mut raw, MQ_ATTR_CURMSGS, current.curmsgs);
        copy_to_user(oldattr_user, &raw).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_msgget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let key = MsgKey(ctx.args[0] as i32);
    let flags = ctx.args[1] as u32;
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let id = Arc::clone(&task_ipc(ctx).msg).msgget(key, flags, &cred)?;
    Ok(id.0 as usize)
}

pub(super) fn sys_msgctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let raw_id = ctx.args[0] as i32;
    let raw_cmd = ctx.args[1] as u32;
    let cmd = raw_cmd & !IPC_64;
    let buf = ctx.args[2];
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let manager = Arc::clone(&task_ipc(ctx).msg);

    match cmd {
        IPC_STAT => {
            let queue = manager.queue_for_operation(MsgId(raw_id))?;
            let meta = queue.stat(&cred)?;
            copy_to_user(buf, &encode_msqid64_ds(&meta)).map_err(|e| e.as_errno())?;
            Ok(0)
        }
        MSG_STAT | MSG_STAT_ANY => {
            let (id, queue) = manager.queue_by_index(raw_id)?;
            let meta = if cmd == MSG_STAT_ANY {
                queue.stat_any()?
            } else {
                queue.stat(&cred)?
            };
            copy_to_user(buf, &encode_msqid64_ds(&meta)).map_err(|e| e.as_errno())?;
            Ok(id.0 as usize)
        }
        IPC_SET => {
            let mut raw = [0u8; MSQID64_DS_SIZE];
            copy_from_user(buf, &mut raw).map_err(|e| e.as_errno())?;
            let qbytes = read_u64(&raw, 88) as usize;
            let queue = manager.queue_for_operation(MsgId(raw_id))?;
            queue.set(
                Some(VfsUid(read_u32(&raw, 4))),
                Some(VfsGid(read_u32(&raw, 8))),
                Some(FileMode::new((read_u32(&raw, 20) as u16) & MODE_MASK)),
                Some(qbytes),
                &cred,
                now_sec(),
            )?;
            Ok(0)
        }
        IPC_RMID => {
            let queue = manager.remove(MsgId(raw_id), &cred)?;
            queue.waiters().wake_all();
            Ok(0)
        }
        IPC_INFO | MSG_INFO => {
            let info = manager.info();
            let raw = encode_msginfo(&info, cmd == IPC_INFO);
            copy_to_user(buf, &raw).map_err(|e| e.as_errno())?;
            Ok(info.max_index as usize)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sys_msgrcv(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let id = MsgId(ctx.args[0] as i32);
    let msgp = ctx.args[1];
    let msgsz = ctx.args[2];
    let msgtyp = ctx.args[3] as i64;
    let flags = ctx.args[4] as u32;
    if msgp == 0 {
        return Err(Errno::EFAULT);
    }
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let queue = Arc::clone(&task_ipc(ctx).msg).queue_for_operation(id)?;
    let task = Arc::clone(ctx.task());
    let pid = task_pid(ctx);

    loop {
        match queue.try_receive(msgtyp, msgsz, flags, &cred, pid, now_sec()) {
            Ok(MsgRecvOutcome::Received(received)) => {
                copy_to_user(msgp, &received.mtype.to_le_bytes()).map_err(|e| e.as_errno())?;
                if !received.data.is_empty() {
                    copy_to_user(msgp + 8, &received.data).map_err(|e| e.as_errno())?;
                }
                queue.waiters().wake_all();
                // `MSG_COPY` + `MSG_TRUNC` 返回消息完整长度；其余返回拷贝字节数。
                let returned = if received.copied && flags & MSG_TRUNC != 0 {
                    received.full_size
                } else {
                    received.data.len()
                };
                return Ok(returned);
            }
            Ok(MsgRecvOutcome::WouldBlock) => {}
            Err(error) => return Err(error),
        }
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }

        let entry = queue
            .waiters()
            .prepare_to_wait(&task, sched::TaskState::Sleeping);
        match queue.try_receive(msgtyp, msgsz, flags, &cred, pid, now_sec()) {
            Ok(MsgRecvOutcome::Received(received)) => {
                queue.waiters().finish_wait(&entry);
                copy_to_user(msgp, &received.mtype.to_le_bytes()).map_err(|e| e.as_errno())?;
                if !received.data.is_empty() {
                    copy_to_user(msgp + 8, &received.data).map_err(|e| e.as_errno())?;
                }
                queue.waiters().wake_all();
                let returned = if received.copied && flags & MSG_TRUNC != 0 {
                    received.full_size
                } else {
                    received.data.len()
                };
                return Ok(returned);
            }
            Ok(MsgRecvOutcome::WouldBlock) => {}
            Err(error) => {
                queue.waiters().finish_wait(&entry);
                return Err(error);
            }
        }
        if sched::operation::has_interrupting_signal(&task) {
            queue.waiters().finish_wait(&entry);
            return Err(Errno::EINTR);
        }
        sched::schedule_once(sched::now_ns_direct());
        queue.waiters().finish_wait(&entry);
    }
}

pub(super) fn sys_msgsnd(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let id = MsgId(ctx.args[0] as i32);
    let msgp = ctx.args[1];
    let msgsz = ctx.args[2];
    let flags = ctx.args[3] as u32;
    if msgp == 0 {
        return Err(Errno::EFAULT);
    }
    let mut mtype_raw = [0u8; 8];
    copy_from_user(msgp, &mut mtype_raw).map_err(|e| e.as_errno())?;
    let mtype = i64::from_le_bytes(mtype_raw);
    // 对齐 Linux `do_msgsnd`：先校验 `msgsz`/`mtype`（EINVAL），再读 data。
    // 否则"坏指针 + 超限"同时发生时，会先因 data 的 `copy_from_user` 返回
    // EFAULT，而 Linux 此处返回 EINVAL。
    if msgsz > MSGMAX {
        return Err(Errno::EINVAL);
    }
    if mtype < 1 {
        return Err(Errno::EINVAL);
    }
    let mut data = vec![0u8; msgsz];
    if msgsz > 0 {
        copy_from_user(msgp + 8, &mut data).map_err(|e| e.as_errno())?;
    }
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let queue = Arc::clone(&task_ipc(ctx).msg).queue_for_operation(id)?;
    let task = Arc::clone(ctx.task());
    let pid = task_pid(ctx);

    loop {
        match queue.try_send(mtype, &data, flags, &cred, pid, now_sec()) {
            Ok(MsgOpAttempt::Done) => {
                queue.waiters().wake_all();
                return Ok(0);
            }
            Ok(MsgOpAttempt::WouldBlock) => {}
            Err(error) => return Err(error),
        }
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }

        let entry = queue
            .waiters()
            .prepare_to_wait(&task, sched::TaskState::Sleeping);
        match queue.try_send(mtype, &data, flags, &cred, pid, now_sec()) {
            Ok(MsgOpAttempt::Done) => {
                queue.waiters().finish_wait(&entry);
                queue.waiters().wake_all();
                return Ok(0);
            }
            Ok(MsgOpAttempt::WouldBlock) => {}
            Err(error) => {
                queue.waiters().finish_wait(&entry);
                return Err(error);
            }
        }
        if sched::operation::has_interrupting_signal(&task) {
            queue.waiters().finish_wait(&entry);
            return Err(Errno::EINTR);
        }
        sched::schedule_once(sched::now_ns_direct());
        queue.waiters().finish_wait(&entry);
    }
}

pub(super) fn sys_semget(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let key = SemKey(ctx.args[0] as i32);
    let nsems = ctx.args[1];
    let flags = ctx.args[2] as u32;
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let id = Arc::clone(&task_ipc(ctx).sem).semget(key, nsems, flags, &cred, now_sec())?;
    Ok(id.0 as usize)
}

pub(super) fn sys_semctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let id = SemId(ctx.args[0] as i32);
    let sem_num = ctx.args[1];
    let cmd = (ctx.args[2] as u32) & !IPC_64;
    let arg = ctx.args[3];
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let manager = Arc::clone(&task_ipc(ctx).sem);
    let pid = task_pid(ctx);
    let now = now_sec();

    match cmd {
        IPC_RMID => {
            let set = manager.remove(id, &cred)?;
            sem_undo_table(ctx).clear(id);
            set.waiters().wake_all();
            Ok(0)
        }
        SEMCTL_GETVAL => Ok(manager.get_value(id, sem_num, &cred)? as usize),
        SEMCTL_SETVAL => {
            let value = ctx.args[3] as i32;
            let set = manager.set_value(id, sem_num, value, &cred, pid, now)?;
            sem_undo_table(ctx).clear(id);
            set.waiters().wake_all();
            Ok(0)
        }
        SEMCTL_GETPID => Ok(manager.get_pid(id, sem_num, &cred)? as usize),
        SEMCTL_GETNCNT => Ok(manager.get_ncnt(id, sem_num, &cred)? as usize),
        SEMCTL_GETZCNT => Ok(manager.get_zcnt(id, sem_num, &cred)? as usize),
        SEMCTL_GETALL => {
            if arg == 0 {
                return Err(Errno::EFAULT);
            }
            let values = manager.get_all(id, &cred)?;
            // `semun.array` 是 `unsigned short *`（2 字节/元素，Linux 语义）：
            // 每个值按 2 字节小端写出（内部值经 `0..=SEMVMX` 校验，恰好 fit u16）。
            for (index, value) in values.iter().enumerate() {
                let address = arg
                    .checked_add(index * size_of::<u16>())
                    .ok_or(Errno::EFAULT)?;
                copy_to_user(address, &(*value as u16).to_le_bytes()).map_err(|e| e.as_errno())?;
            }
            Ok(0)
        }
        SEMCTL_SETALL => {
            if arg == 0 {
                return Err(Errno::EFAULT);
            }
            let nsems = manager.stat(id, &cred)?.nsems;
            let mut values = vec![0i32; nsems];
            // `semun.array` 是 `unsigned short *`（2 字节/元素，Linux 语义）：
            // 每个元素读 2 字节小端，汇入 `Vec<i32>`（范围校验由 `set_all` 保留）。
            for (index, slot) in values.iter_mut().enumerate() {
                let address = arg
                    .checked_add(index * size_of::<u16>())
                    .ok_or(Errno::EFAULT)?;
                let mut raw = [0u8; 2];
                copy_from_user(address, &mut raw).map_err(|e| e.as_errno())?;
                *slot = u16::from_le_bytes(raw) as i32;
            }
            let set = manager.set_all(id, &values, &cred, pid, now)?;
            sem_undo_table(ctx).clear(id);
            set.waiters().wake_all();
            Ok(0)
        }
        IPC_STAT => {
            let meta = manager.stat(id, &cred)?;
            copy_to_user(arg, &encode_semid64_ds(&meta)).map_err(|e| e.as_errno())?;
            Ok(0)
        }
        SEM_STAT | SEM_STAT_ANY => {
            let (found_id, set) = manager.set_by_index(id.0)?;
            let meta = if cmd == SEM_STAT_ANY {
                set.stat_any()?
            } else {
                set.stat(&cred)?
            };
            copy_to_user(arg, &encode_semid64_ds(&meta)).map_err(|e| e.as_errno())?;
            Ok(found_id.0 as usize)
        }
        IPC_SET => {
            let mut raw = [0u8; SEMID64_DS_SIZE];
            copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
            manager.set_perm(
                id,
                Some(VfsUid(read_u32(&raw, 4))),
                Some(VfsGid(read_u32(&raw, 8))),
                Some(FileMode::new((read_u32(&raw, 20) as u16) & MODE_MASK)),
                &cred,
                now,
            )?;
            Ok(0)
        }
        IPC_INFO | SEM_INFO => {
            let info = manager.info();
            let raw = encode_seminfo(&info, cmd == IPC_INFO);
            copy_to_user(arg, &raw).map_err(|e| e.as_errno())?;
            Ok(info.max_index as usize)
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

/// 注册 mqueue 通知分发器（`SIGEV_SIGNAL`/`SIGEV_THREAD` 触发动作）。
pub(super) fn register_mq_notify_dispatcher_once() {
    general::vfs::register_mq_notify_dispatcher(mq_notify_dispatcher);
}

/// 启动期把 SysV shm/sem/msg 与 key 的快照接入 /proc 渲染（安装一次）。
///
/// 与 [`register_mq_notify_dispatcher_once`] 一样由 `syscalls::register_all`
/// 在启动期调用。provider 无任务上下文，读取 /proc 的进程即"当前任务"，因此
/// SysV 快照按当前任务所在 IPC 命名空间返回（与 Linux /proc/sysvipc 的命名
/// 空间语义一致）；key 非命名空间隔离，走全局 [`keys_manager`]。
pub(super) fn register_proc_ipc_providers_once() {
    general::vfs::procfs::install_proc_sysvipc_shm_provider(proc_sysvipc_shm_snapshot);
    general::vfs::procfs::install_proc_sysvipc_sem_provider(proc_sysvipc_sem_snapshot);
    general::vfs::procfs::install_proc_sysvipc_msg_provider(proc_sysvipc_msg_snapshot);
    general::vfs::procfs::install_proc_keys_provider(proc_keys_snapshot);
    general::vfs::procfs::install_proc_key_users_provider(proc_key_users_snapshot);
}

/// `/proc/sysvipc/shm`：当前 IPC 命名空间内全部 shm 段的快照。
fn proc_sysvipc_shm_snapshot() -> Vec<general::vfs::procfs::ProcSysvShmEntry> {
    let manager = Arc::clone(&crate::ns::current_ns().ipc.shm);
    let used = manager.info().used_segments;
    let page = general::mm::page_size() as u64;
    let mut out = Vec::with_capacity(used);
    for index in 0..used {
        let Ok((_id, meta)) = manager.stat_by_index(index as i32, &VfsCredentials::root(), false)
        else {
            continue;
        };
        out.push(general::vfs::procfs::ProcSysvShmEntry {
            id: meta.id.0,
            key: meta.key.0,
            size_bytes: meta.size,
            nattch: meta.nattch as u32,
            uid: meta.perm.uid.0,
            gid: meta.perm.gid.0,
            cuid: meta.perm.cuid.0,
            cgid: meta.perm.cgid.0,
            mode: meta.perm.mode.bits(),
            cpid: meta.cpid,
            lpid: meta.lpid,
            pages: meta.size.div_ceil(page),
            locked: meta.locked,
            marked_for_removal: meta.marked_for_removal,
            atime: meta.atime,
            dtime: meta.dtime,
            ctime: meta.ctime,
        });
    }
    out
}

/// `/proc/sysvipc/sem`：当前 IPC 命名空间内全部 semaphore 集合的快照。
fn proc_sysvipc_sem_snapshot() -> Vec<general::vfs::procfs::ProcSysvSemEntry> {
    let manager = Arc::clone(&crate::ns::current_ns().ipc.sem);
    let used = manager.info().sets;
    let mut out = Vec::with_capacity(used);
    for index in 0..used {
        let Ok((id, set)) = manager.set_by_index(index as i32) else {
            continue;
        };
        let Ok(meta) = set.stat_any() else {
            continue;
        };
        out.push(general::vfs::procfs::ProcSysvSemEntry {
            id: id.0,
            key: meta.key().0,
            nsems: meta.nsems as u32,
            uid: meta.uid().0,
            gid: meta.gid().0,
            cuid: meta.cuid().0,
            cgid: meta.cgid().0,
            mode: meta.mode().bits(),
            otime: meta.otime,
            ctime: meta.ctime,
        });
    }
    out
}

/// `/proc/sysvipc/msg`：当前 IPC 命名空间内全部消息队列的快照。
fn proc_sysvipc_msg_snapshot() -> Vec<general::vfs::procfs::ProcSysvMsgEntry> {
    let manager = Arc::clone(&crate::ns::current_ns().ipc.msg);
    let used = manager.info().queues;
    let mut out = Vec::with_capacity(used);
    for index in 0..used {
        let Ok((id, queue)) = manager.queue_by_index(index as i32) else {
            continue;
        };
        let Ok(meta) = queue.stat_any() else {
            continue;
        };
        out.push(general::vfs::procfs::ProcSysvMsgEntry {
            id: id.0,
            key: meta.key().0,
            qbytes: meta.qbytes as u64,
            qnum: meta.qnum as u64,
            uid: meta.uid().0,
            gid: meta.gid().0,
            cuid: meta.cuid().0,
            cgid: meta.cgid().0,
            mode: meta.mode().bits(),
            lspid: meta.lspid,
            lrpid: meta.lrpid,
            stime: meta.stime,
            rtime: meta.rtime,
            ctime: meta.ctime,
        });
    }
    out
}

/// `/proc/keys`：全局 keyring 系统的 key 快照。
fn proc_keys_snapshot() -> Vec<general::vfs::procfs::ProcKeyEntry> {
    keys_manager()
        .snapshot_all()
        .into_iter()
        .map(|snapshot| general::vfs::procfs::ProcKeyEntry {
            id: snapshot.id.0,
            type_name: snapshot.key_type.name(),
            description: snapshot.description,
            uid: snapshot.uid,
            gid: snapshot.gid,
            perm: snapshot.perm,
            state: key_state_flag(snapshot.state),
            expiry: snapshot.expiry,
            payload_len: snapshot.payload_len,
            nkeys: snapshot.nkeys,
        })
        .collect()
}

/// `/proc/key-users`：每 uid 的 key 配额快照。
fn proc_key_users_snapshot() -> Vec<(u32, usize, usize)> {
    keys_manager().quota_snapshot()
}

/// `/proc/keys` flags 列的 key 状态编码（简化自 Linux `KEY_FLAG_*`）。
fn key_state_flag(state: KeyState) -> &'static str {
    match state {
        KeyState::Instantiated => "I",
        KeyState::Uninstantiated => "U",
        KeyState::Negative => "N",
        KeyState::Revoked => "R",
    }
}

/// `SIGEV_SIGNAL`：向注册者投递信号（`si_code = SI_MESGQ`，携带 `si_value`）；
/// `SIGEV_THREAD`：克隆注册者线程执行通知函数。
fn mq_notify_dispatcher(notification: &general::ipc::mqueue::MqNotification) {
    use sched::ids::Uid;
    use sched::signal::SigInfo;

    match notification.kind {
        MqNotifyKind::None => {}
        MqNotifyKind::Signal { signo, value } => {
            let Some(sig) = sched::SignalNumber::from_raw(signo) else {
                return;
            };
            // 构造完整的 128 字节 siginfo（64 位布局：signo@0、code@8、pid@12、
            // uid@16、si_value@24），使用户 handler 能读到 si_value。
            let mut raw = [0u8; 128];
            raw[0..4].copy_from_slice(&(sig.raw() as i32).to_le_bytes());
            raw[8..12].copy_from_slice(&SI_MESGQ.to_le_bytes());
            raw[12..16].copy_from_slice(&notification.sender_pid.to_le_bytes());
            raw[16..20].copy_from_slice(&notification.sender_uid.to_le_bytes());
            raw[24..32].copy_from_slice(&(value as u64).to_le_bytes());
            let info = SigInfo {
                sig,
                code: SI_MESGQ,
                sender_pid: notification.sender_pid,
                sender_uid: Uid(notification.sender_uid),
                raw: Some(raw),
            };
            let _ = sched::operation::queueinfo(notification.sender_pid, info);
        }
        MqNotifyKind::Thread { function, value } => {
            // 注册者可能已退出；查找失败时静默丢弃（Linux 同样如此）。
            let Ok(target) = sched::operation::lookup_pid(notification.sender_pid) else {
                return;
            };
            crate::sched::spawn_mq_notify_thread(&target, function, value);
        }
    }
}

pub(super) fn sys_add_key(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let type_user = ctx.args[0];
    let desc_user = ctx.args[1];
    let payload_user = ctx.args[2];
    let plen = ctx.args[3];
    let keyring_arg = ctx.args[4] as i32;
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let manager = keys_manager();
    let now = now_sec_u64();

    let key_type_name = copy_cstr_from_user(type_user, 32).map_err(|e| e.as_errno())?;
    let key_type = KeyType::parse(&key_type_name).ok_or(Errno::ENODEV)?;
    let description = copy_cstr_from_user(desc_user, KEY_DESC_MAX).map_err(|e| e.as_errno())?;
    let mut payload = vec![0u8; plen];
    if plen > 0 {
        copy_from_user(payload_user, &mut payload).map_err(|e| e.as_errno())?;
    }
    let keyring = resolve_keyring(&manager, &process_keyrings(ctx), keyring_arg, &cred, now)?;
    let id = manager.add_key(key_type, &description, payload, keyring, &cred, now)?;
    Ok(id.0 as usize)
}

pub(super) fn sys_request_key(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let type_user = ctx.args[0];
    let desc_user = ctx.args[1];
    let info_user = ctx.args[2];
    let keyring_arg = ctx.args[3] as i32;
    let dest_keyring_arg = ctx.args[4] as i32;
    let task = Arc::clone(ctx.task());
    let cred = vfs_cred_from_sched(&task.credentials());
    let manager = keys_manager();
    let now = now_sec_u64();
    let process = process_keyrings(ctx);

    let key_type_name = copy_cstr_from_user(type_user, 32).map_err(|e| e.as_errno())?;
    let key_type = KeyType::parse(&key_type_name).ok_or(Errno::ENODEV)?;
    let description = copy_cstr_from_user(desc_user, KEY_DESC_MAX).map_err(|e| e.as_errno())?;
    let info = if info_user != 0 {
        copy_cstr_from_user(info_user, KEY_DESC_MAX).map_err(|e| e.as_errno())?
    } else {
        alloc::string::String::new()
    };

    let search_keyring = resolve_keyring(&manager, &process, keyring_arg, &cred, now)?;
    if let Ok(key) = manager.search(search_keyring, key_type, &description, &cred, now) {
        return Ok(key.id.0 as usize);
    }
    // 未命中：创建未实例化 key + 授权 key，spawn `/sbin/request-key` upcall。
    let dest_keyring = resolve_keyring(&manager, &process, dest_keyring_arg, &cred, now)?;
    let key = manager.create_uninstantiated(
        key_type,
        &description,
        cred.euid.0,
        cred.egid.0,
        KEY_DEFAULT_PERM,
    )?;
    let auth = manager.create_uninstantiated(
        KeyType::User,
        &format!("_reqkey_auth.{}", key.id.0),
        cred.euid.0,
        cred.egid.0,
        KEY_DEFAULT_PERM,
    )?;
    *process.reqkey_auth.lock() = Some(auth.id);
    if let Ok(dest) = manager.key(dest_keyring) {
        dest.add_member(key.id, key_type.name(), &description);
    }

    // Linux 布局：/sbin/request-key <op> <key> <uid> <gid> <keyring> <type> <desc> <info>
    let argv = vec![
        "/sbin/request-key".to_string(),
        "create".to_string(),
        key.id.0.to_string(),
        cred.euid.0.to_string(),
        cred.egid.0.to_string(),
        keyring_arg.to_string(),
        key_type_name,
        description.clone(),
        info,
    ];
    let spawned =
        sched::operation::spawn_user_process(&task, "/sbin/request-key", &argv, &[]).is_ok();
    if !spawned {
        *process.reqkey_auth.lock() = None;
        return Err(Errno::ENOKEY);
    }

    // 等待实例化（最多 60 秒，可被信号打断）。
    let deadline = sched::now_ns_direct().saturating_add(60_000_000_000);
    loop {
        match key.state() {
            KeyState::Instantiated => return Ok(key.id.0 as usize),
            KeyState::Negative => return Err(Errno::ENOKEY),
            KeyState::Revoked => return Err(Errno::ENOKEY),
            KeyState::Uninstantiated => {}
        }
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }
        if sched::now_ns_direct() >= deadline {
            return Err(Errno::ENOKEY);
        }
        sched::schedule_once(sched::now_ns_direct());
    }
}

pub(super) fn sys_keyctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const KEYCTL_GET_KEYRING_ID: usize = 0;
    const KEYCTL_JOIN_SESSION_KEYRING: usize = 1;
    const KEYCTL_UPDATE: usize = 2;
    const KEYCTL_REVOKE: usize = 3;
    const KEYCTL_CHOWN: usize = 4;
    const KEYCTL_SETPERM: usize = 5;
    const KEYCTL_DESCRIBE: usize = 6;
    const KEYCTL_CLEAR: usize = 7;
    const KEYCTL_LINK: usize = 8;
    const KEYCTL_UNLINK: usize = 9;
    const KEYCTL_SEARCH: usize = 10;
    const KEYCTL_READ: usize = 11;
    const KEYCTL_INSTANTIATE: usize = 12;
    const KEYCTL_NEGATE: usize = 13;
    const KEYCTL_SET_REQKEY_KEYRING: usize = 14;
    const KEYCTL_SET_TIMEOUT: usize = 15;
    const KEYCTL_ASSUME_AUTHORITY: usize = 16;
    const KEYCTL_GET_SECURITY: usize = 17;
    const KEYCTL_SESSION_TO_PARENT: usize = 18;
    const KEYCTL_REJECT: usize = 19;
    const KEYCTL_INSTANTIATE_IOV: usize = 20;
    const KEYCTL_INVALIDATE: usize = 21;
    const KEYCTL_GET_PERSISTENT: usize = 22;
    const KEYCTL_RESTRICT_KEYRING: usize = 29;
    const KEYCTL_SUPPORTS_ENCRYPT: usize = 32;
    const KEYCTL_SUPPORTS_DECRYPT: usize = 33;
    const KEYCTL_SUPPORTS_SIGN: usize = 34;
    const KEYCTL_SUPPORTS_VERIFY: usize = 35;
    const KEYCTL_CAPABILITIES: usize = 36;

    let cmd = ctx.args[0];
    let cred = vfs_cred_from_sched(&ctx.task().credentials());
    let manager = keys_manager();
    let now = now_sec_u64();

    match cmd {
        KEYCTL_GET_KEYRING_ID => {
            // arg2: keyring（KEY_SPEC 或 id）；arg3: create 标志。
            let spec = ctx.args[1] as i32;
            let create = ctx.args[2] != 0;
            if create && spec == KEY_SPEC_SESSION_KEYRING {
                let name = format!("_ses.{}", cred.euid.0);
                let key = manager.create_uninstantiated(
                    KeyType::Keyring,
                    &name,
                    cred.euid.0,
                    cred.egid.0,
                    KEY_DEFAULT_PERM,
                )?;
                key.set_state(KeyState::Instantiated);
                *process_keyrings(ctx).session.lock() = Some(key.id);
                return Ok(key.id.0 as usize);
            }
            let keyring = resolve_keyring(&manager, &process_keyrings(ctx), spec, &cred, now)?;
            Ok(keyring.0 as usize)
        }
        KEYCTL_JOIN_SESSION_KEYRING => {
            let name_user = ctx.args[1];
            let name = if name_user != 0 {
                copy_cstr_from_user(name_user, KEY_DESC_MAX).map_err(|e| e.as_errno())?
            } else {
                format!("_ses.{}", cred.euid.0)
            };
            let key = manager.create_uninstantiated(
                KeyType::Keyring,
                &name,
                cred.euid.0,
                cred.egid.0,
                KEY_DEFAULT_PERM,
            )?;
            key.set_state(KeyState::Instantiated);
            *process_keyrings(ctx).session.lock() = Some(key.id);
            Ok(key.id.0 as usize)
        }
        KEYCTL_UPDATE => {
            let key_id = KeyId(ctx.args[1] as i32);
            let payload_user = ctx.args[2];
            let plen = ctx.args[3];
            let mut payload = vec![0u8; plen];
            if plen > 0 {
                copy_from_user(payload_user, &mut payload).map_err(|e| e.as_errno())?;
            }
            manager.update(key_id, payload, &cred)?;
            Ok(0)
        }
        KEYCTL_REVOKE => {
            manager.revoke(KeyId(ctx.args[1] as i32), &cred)?;
            Ok(0)
        }
        KEYCTL_CHOWN => {
            let key_id = KeyId(ctx.args[1] as i32);
            let uid = ctx.args[2] as u32;
            let gid = ctx.args[3] as u32;
            manager.chown(
                key_id,
                (uid != u32::MAX).then_some(uid),
                (gid != u32::MAX).then_some(gid),
                &cred,
            )?;
            Ok(0)
        }
        KEYCTL_SETPERM => {
            manager.setperm(KeyId(ctx.args[1] as i32), ctx.args[2] as u32, &cred)?;
            Ok(0)
        }
        KEYCTL_DESCRIBE => {
            let key_id = KeyId(ctx.args[1] as i32);
            let buffer = ctx.args[2];
            let buflen = ctx.args[3];
            let description = manager.describe(key_id, &cred)?;
            keyctl_copy_string(buffer, buflen, &description)
        }
        KEYCTL_CLEAR => {
            manager.clear(KeyId(ctx.args[1] as i32), &cred)?;
            Ok(0)
        }
        KEYCTL_LINK => {
            let key_id = KeyId(ctx.args[1] as i32);
            // keyring 参数支持 KEY_SPEC_*（-1..-8）与显式 id（Linux 语义）。
            let keyring = resolve_keyring(
                &manager,
                &process_keyrings(ctx),
                ctx.args[2] as i32,
                &cred,
                now,
            )?;
            manager.link(keyring, key_id, &cred)?;
            Ok(0)
        }
        KEYCTL_UNLINK => {
            let key_id = KeyId(ctx.args[1] as i32);
            let keyring = resolve_keyring(
                &manager,
                &process_keyrings(ctx),
                ctx.args[2] as i32,
                &cred,
                now,
            )?;
            manager.unlink(keyring, key_id, &cred)?;
            Ok(0)
        }
        KEYCTL_SEARCH => {
            let keyring_arg = ctx.args[1] as i32;
            let type_user = ctx.args[2];
            let desc_user = ctx.args[3];
            let dest_keyring_arg = ctx.args[4] as i32;
            let key_type_name = copy_cstr_from_user(type_user, 32).map_err(|e| e.as_errno())?;
            let key_type = KeyType::parse(&key_type_name).ok_or(Errno::ENODEV)?;
            let description =
                copy_cstr_from_user(desc_user, KEY_DESC_MAX).map_err(|e| e.as_errno())?;
            let search_keyring =
                resolve_keyring(&manager, &process_keyrings(ctx), keyring_arg, &cred, now)?;
            let key = manager.search(search_keyring, key_type, &description, &cred, now)?;
            if dest_keyring_arg != 0 {
                let dest = resolve_keyring(
                    &manager,
                    &process_keyrings(ctx),
                    dest_keyring_arg,
                    &cred,
                    now,
                )?;
                if let Ok(dest_keyring) = manager.key(dest) {
                    dest_keyring.add_member(key.id, key_type.name(), &description);
                }
            }
            Ok(key.id.0 as usize)
        }
        KEYCTL_READ => {
            let key_id = KeyId(ctx.args[1] as i32);
            let buffer = ctx.args[2];
            let buflen = ctx.args[3];
            let data = manager.read(key_id, &cred)?;
            if buffer == 0 || buflen == 0 {
                return Ok(data.len());
            }
            let n = data.len().min(buflen);
            copy_to_user(buffer, &data[..n]).map_err(|e| e.as_errno())?;
            Ok(n)
        }
        KEYCTL_INSTANTIATE => {
            let key_id = KeyId(ctx.args[1] as i32);
            let payload_user = ctx.args[2];
            let plen = ctx.args[3];
            let keyring_id = KeyId(ctx.args[4] as i32);
            let mut payload = vec![0u8; plen];
            if plen > 0 {
                copy_from_user(payload_user, &mut payload).map_err(|e| e.as_errno())?;
            }
            manager.instantiate(key_id, payload, true, keyring_id, None, now)?;
            Ok(0)
        }
        KEYCTL_NEGATE => {
            let key_id = KeyId(ctx.args[1] as i32);
            let timeout = ctx.args[2] as u64;
            let keyring_id = KeyId(ctx.args[3] as i32);
            manager.instantiate(key_id, Vec::new(), false, keyring_id, Some(timeout), now)?;
            Ok(0)
        }
        KEYCTL_REJECT => {
            let key_id = KeyId(ctx.args[1] as i32);
            let timeout = ctx.args[2] as u64;
            let keyring_id = KeyId(ctx.args[4] as i32);
            manager.instantiate(key_id, Vec::new(), false, keyring_id, Some(timeout), now)?;
            Ok(0)
        }
        KEYCTL_SET_REQKEY_KEYRING => {
            // 设置默认请求 keyring 偏好（Linux `jit_keyring`）。返回旧偏好；
            // `KEY_REQKEY_DEFL_NO_CHANGE` 只查询不修改。
            let reqkey = ctx.args[1] as i32;
            let process = process_keyrings(ctx);
            let mut guard = process.reqkey_default.lock();
            let old = *guard;
            if reqkey != KEY_REQKEY_DEFL_NO_CHANGE {
                match reqkey {
                    KEY_REQKEY_DEFL_DEFAULT
                    | KEY_REQKEY_DEFL_THREAD_KEYRING
                    | KEY_REQKEY_DEFL_PROCESS_KEYRING
                    | KEY_REQKEY_DEFL_SESSION_KEYRING
                    | KEY_REQKEY_DEFL_USER_KEYRING
                    | KEY_REQKEY_DEFL_USER_SESSION_KEYRING
                    | KEY_REQKEY_DEFL_REQUESTOR_KEYRING => *guard = reqkey,
                    _ => return Err(Errno::EINVAL),
                }
            }
            Ok(old as usize)
        }
        KEYCTL_SET_TIMEOUT => {
            let key_id = KeyId(ctx.args[1] as i32);
            manager.set_timeout(key_id, ctx.args[2] as u64, &cred, now)?;
            Ok(0)
        }
        KEYCTL_ASSUME_AUTHORITY => {
            // 置/清 reqkey_auth（upcall 进程用它认领授权 key）。
            let key_arg = ctx.args[1] as i32;
            let process = process_keyrings(ctx);
            if key_arg == 0 {
                *process.reqkey_auth.lock() = None;
                Ok(0)
            } else if key_arg == KEY_SPEC_REQKEY_AUTH_KEY {
                Ok(process
                    .reqkey_auth
                    .lock()
                    .map(|id| id.0 as usize)
                    .unwrap_or(0))
            } else {
                *process.reqkey_auth.lock() = Some(KeyId(key_arg));
                Ok(0)
            }
        }
        KEYCTL_GET_SECURITY => {
            let key_id = KeyId(ctx.args[1] as i32);
            // 无 LSM：返回空的安全标签（长度 0，Linux 无 LSM 时
            // `security_key_getsecurity` 返回 0 的行为）。
            let _ = manager.describe(key_id, &cred)?;
            Ok(0)
        }
        KEYCTL_SESSION_TO_PARENT => {
            // 把当前进程的 session keyring 安装到父进程（Linux 语义）。
            let session = *process_keyrings(ctx).session.lock();
            let Some(parent) = ctx.task().parent() else {
                return Err(Errno::ENOKEY);
            };
            *process_keyrings_of(&parent).session.lock() = session;
            Ok(0)
        }
        KEYCTL_INSTANTIATE_IOV => {
            // iovec 版 instantiate：聚合成单个负载。
            let key_id = KeyId(ctx.args[1] as i32);
            let iov_user = ctx.args[2];
            let iovcnt = ctx.args[3];
            let keyring_id = KeyId(ctx.args[4] as i32);
            let payload = read_iovec_payload(iov_user, iovcnt)?;
            manager.instantiate(key_id, payload, true, keyring_id, None, now)?;
            Ok(0)
        }
        KEYCTL_INVALIDATE => {
            manager.invalidate(KeyId(ctx.args[1] as i32), &cred)?;
            Ok(0)
        }
        KEYCTL_GET_PERSISTENT => {
            let uid = ctx.args[1] as u32;
            let _keyring = ctx.args[2] as i32;
            let id = manager.user_keyring(uid, &cred)?;
            Ok(id.0 as usize)
        }
        KEYCTL_RESTRICT_KEYRING => {
            // 限制 keyring 可链接的 key 类型；restriction 字符串（LSM 风格）
            // 本内核不解析，仅在非空时校验可读性后忽略。
            let keyring_id = KeyId(ctx.args[1] as i32);
            let type_user = ctx.args[2];
            let restriction_user = ctx.args[3];
            let restriction = if type_user != 0 {
                let name = copy_cstr_from_user(type_user, 32).map_err(|e| e.as_errno())?;
                Some(KeyType::parse(&name).ok_or(Errno::ENODEV)?)
            } else {
                None
            };
            if restriction_user != 0 {
                let _ = copy_cstr_from_user(restriction_user, KEY_DESC_MAX)
                    .map_err(|e| e.as_errno())?;
            }
            manager.restrict_keyring(keyring_id, restriction, &cred)?;
            Ok(0)
        }
        KEYCTL_SUPPORTS_ENCRYPT
        | KEYCTL_SUPPORTS_DECRYPT
        | KEYCTL_SUPPORTS_SIGN
        | KEYCTL_SUPPORTS_VERIFY => {
            // 本内核不提供 key 加密/解密/签名/验签（无 crypto key 类型）。
            Ok(0)
        }
        KEYCTL_CAPABILITIES => {
            let buffer = ctx.args[1];
            let buflen = ctx.args[2];
            // Linux `keyctl_capabilities`：bit0 = capabilities 命令可用。
            let caps: [u8; 1] = [1];
            if buffer == 0 || buflen == 0 {
                return Ok(caps.len());
            }
            let n = caps.len().min(buflen);
            copy_to_user(buffer, &caps[..n]).map_err(|e| e.as_errno())?;
            Ok(n)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// 从 `KEY_SPEC_*` 或序列号解析 keyring；`0` 使用默认链。
fn resolve_keyring(
    manager: &KeyManager,
    process: &ProcessKeyrings,
    spec: i32,
    cred: &vfs::cred::Credentials,
    now: u64,
) -> Result<KeyId, Errno> {
    if spec == 0 {
        return Ok(general::ipc::keys::default_keyring_chain(
            process, cred, manager, now,
        ));
    }
    manager
        .resolve_spec(spec, process, cred, now)
        .map(|key| key.id)
}

/// 复制以 NUL 结尾的字符串到用户缓冲区；返回包含 NUL 的长度（Linux 语义）。
fn keyctl_copy_string(buffer: usize, buflen: usize, value: &str) -> Result<usize, Errno> {
    if buffer == 0 || buflen == 0 {
        return Ok(value.len() + 1);
    }
    let bytes = value.as_bytes();
    let n = bytes.len().min(buflen.saturating_sub(1));
    copy_to_user(buffer, &bytes[..n]).map_err(|e| e.as_errno())?;
    let nul = [0u8];
    copy_to_user(buffer + n, &nul).map_err(|e| e.as_errno())?;
    Ok(n + 1)
}

/// 读取 `struct iovec` 数组并聚合成一个负载缓冲区。
fn read_iovec_payload(iov_user: usize, iovcnt: usize) -> Result<Vec<u8>, Errno> {
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let mut total = 0usize;
    let mut payload = Vec::new();
    for index in 0..iovcnt {
        let address = iov_user.checked_add(index * 16).ok_or(Errno::EFAULT)?;
        let mut raw = [0u8; 16];
        copy_from_user(address, &mut raw).map_err(|e| e.as_errno())?;
        let base = read_u64(&raw, 0) as usize;
        let len = read_u64(&raw, 8) as usize;
        if base == 0 && len != 0 {
            return Err(Errno::EINVAL);
        }
        total = total.checked_add(len).ok_or(Errno::EINVAL)?;
        if total > 1024 * 1024 {
            return Err(Errno::E2BIG);
        }
        let start = payload.len();
        payload.resize(total, 0);
        if len > 0 {
            copy_from_user(base, &mut payload[start..start + len]).map_err(|e| e.as_errno())?;
        }
    }
    Ok(payload)
}

pub(super) fn sys_io_pgetevents(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // sigmask（args[5]）本内核不处理（无阻塞等待），事件读取语义与
    // `io_getevents` 一致。
    sys_io_getevents_common(ctx)
}

pub(super) fn sys_io_pgetevents_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_io_getevents_common(ctx)
}

pub(super) fn sys_mq_timedsend_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_mq_timedsend(ctx)
}

pub(super) fn sys_mq_timedreceive_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_mq_timedreceive(ctx)
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

/// 当前任务的 SysV IPC 命名空间（shm/sem/msg 管理器）。
fn task_ipc(ctx: &SyscallContext<'_>) -> Arc<crate::ns::IpcNamespace> {
    Arc::clone(&crate::ns::task_ns(ctx.task()).ipc)
}

fn keys_manager() -> Arc<KeyManager> {
    let mut slot = SYSV_KEYS_MANAGER.lock();
    if let Some(manager) = slot.as_ref() {
        return Arc::clone(manager);
    }
    let manager = Arc::new(KeyManager::default());
    *slot = Some(Arc::clone(&manager));
    manager
}

/// 取当前任务的 keyring 引用集；不存在时惰性创建并挂载。
fn process_keyrings(ctx: &SyscallContext<'_>) -> Arc<ProcessKeyrings> {
    process_keyrings_of(ctx.task())
}

/// 取任意任务的 keyring 引用集（`KEYCTL_SESSION_TO_PARENT` 需操作父任务）。
fn process_keyrings_of(task: &Arc<sched::Task>) -> Arc<ProcessKeyrings> {
    if let Some(process) = task
        .ext_lookup(TASKEXT_KEYRINGS)
        .and_then(|payload| payload.downcast::<ProcessKeyrings>().ok())
    {
        return process;
    }
    let process = Arc::new(ProcessKeyrings::new());
    let erased: Arc<dyn core::any::Any + Send + Sync> = process.clone();
    task.ext_install(TASKEXT_KEYRINGS, erased);
    process
}

/// 单调时钟的当前秒数（key 到期时间戳使用）。
fn now_sec_u64() -> u64 {
    crate::vdso::clock_time_ns(1).unwrap_or(0) as u64 / 1_000_000_000
}

fn msg_manager() -> Arc<MsgManager> {
    let mut slot = SYSV_MSG_MANAGER.lock();
    if let Some(manager) = slot.as_ref() {
        return Arc::clone(manager);
    }
    let manager = Arc::new(MsgManager::default());
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
    let set = Arc::clone(&task_ipc(ctx).sem).set_for_operation(id)?;
    let task = Arc::clone(ctx.task());
    let pid = task_pid(ctx);
    // 当前等待周期内登记的阻塞统计；每个周期结束注销，重新登记时可能指向
    // 批次中的不同操作。
    let mut registered: Option<(usize, SemBlockKind)> = None;

    loop {
        match set.try_apply(&operations, &cred, pid, now_sec()) {
            Ok(SemOpAttempt::Applied) => {
                record_sem_undo(ctx, id, &operations);
                set.waiters().wake_all();
                return Ok(0);
            }
            Ok(SemOpAttempt::WouldBlock { sem_num, kind }) => {
                if registered.is_none() {
                    set.register_blocked(sem_num, kind)?;
                    registered = Some((sem_num, kind));
                }
            }
            Err(error) => return Err(error),
        }
        if sched::operation::has_interrupting_signal(&task) {
            unregister_sem_blocked(&set, registered.take());
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            unregister_sem_blocked(&set, registered.take());
            return Err(Errno::EAGAIN);
        }

        let entry = set
            .waiters()
            .prepare_to_wait(&task, sched::TaskState::Sleeping);
        let deadline_armed = match deadline {
            Some(deadline) => {
                if !sched::register_sleep_deadline(&task, deadline) {
                    set.waiters().finish_wait(&entry);
                    unregister_sem_blocked(&set, registered.take());
                    return Err(Errno::EAGAIN);
                }
                true
            }
            None => false,
        };

        match set.try_apply(&operations, &cred, pid, now_sec()) {
            Ok(SemOpAttempt::Applied) => {
                finish_sem_wait(&set, &entry, &task, deadline_armed);
                unregister_sem_blocked(&set, registered.take());
                record_sem_undo(ctx, id, &operations);
                set.waiters().wake_all();
                return Ok(0);
            }
            Ok(SemOpAttempt::WouldBlock { .. }) => {}
            Err(error) => {
                finish_sem_wait(&set, &entry, &task, deadline_armed);
                unregister_sem_blocked(&set, registered.take());
                return Err(error);
            }
        }
        if sched::operation::has_interrupting_signal(&task) {
            finish_sem_wait(&set, &entry, &task, deadline_armed);
            unregister_sem_blocked(&set, registered.take());
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            finish_sem_wait(&set, &entry, &task, deadline_armed);
            unregister_sem_blocked(&set, registered.take());
            return Err(Errno::EAGAIN);
        }

        sched::schedule_once(sched::now_ns_direct());
        finish_sem_wait(&set, &entry, &task, deadline_armed);
        unregister_sem_blocked(&set, registered.take());
        if sched::operation::has_interrupting_signal(&task) {
            return Err(Errno::EINTR);
        }
        if deadline.is_some_and(|deadline| sched::now_ns_direct() >= deadline) {
            return Err(Errno::EAGAIN);
        }
    }
}

/// 成功提交一批 `semop` 后，把带 `SEM_UNDO` 标志的操作登记进撤销表。
///
/// 表不存在时惰性创建并挂载（Linux `find_alloc_undo`：首次 SEM_UNDO 操作
/// 建立进程自己的撤销表）。
fn record_sem_undo(ctx: &SyscallContext<'_>, id: SemId, operations: &[SemOperation]) {
    if !operations.iter().any(|op| op.sem_flg & SEM_UNDO != 0) {
        return;
    }
    sem_undo_table(ctx).record(id, operations);
}

/// 取当前任务的 `SEM_UNDO` 表；不存在时惰性创建并挂载。
fn sem_undo_table(ctx: &SyscallContext<'_>) -> Arc<SemUndoTable> {
    if let Some(table) = sem_undo_table_opt(ctx) {
        return table;
    }
    let table = Arc::new(SemUndoTable::new());
    let erased: Arc<dyn core::any::Any + Send + Sync> = table.clone();
    ctx.task().ext_install(TASKEXT_SEM_UNDO, erased);
    table
}

fn sem_undo_table_opt(ctx: &SyscallContext<'_>) -> Option<Arc<SemUndoTable>> {
    ctx.task()
        .ext_lookup(TASKEXT_SEM_UNDO)
        .and_then(|payload| payload.downcast::<SemUndoTable>().ok())
}

/// 退出清理：应用并移除任务的 `SEM_UNDO` 表（Linux `exit_sem`）。
///
/// `CLONE_SYSVSEM` 共享的表由多个任务持有；按 Linux 语义只有最后一个持有者
/// 应用撤销项。`ext_remove` 把本任务的引用从扩展表取出，因此 `strong_count`
/// 等于 1 时本任务就是最后一个持有者。
pub(super) fn apply_sem_undo_on_exit(task: &Arc<sched::Task>) {
    let Some(table) = task
        .ext_remove(TASKEXT_SEM_UNDO)
        .and_then(|payload| payload.downcast::<SemUndoTable>().ok())
    else {
        return;
    };
    if Arc::strong_count(&table) > 1 {
        return;
    }
    if table.is_empty() {
        return;
    }
    let cred = vfs_cred_from_sched(&task.credentials());
    let pid = task.pid_root().unwrap_or(0);
    let manager = Arc::clone(&crate::ns::task_ns(task).ipc.sem);
    table.apply_on_exit(&manager, &cred, pid, now_sec(), task);
}

/// 注销本等待周期的阻塞统计登记。
fn unregister_sem_blocked(
    set: &general::ipc::sem::SemSet,
    registered: Option<(usize, SemBlockKind)>,
) {
    if let Some((sem_num, kind)) = registered {
        set.unregister_blocked(sem_num, kind);
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
    let abs_realtime = (seconds as u64)
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds as u64))
        .ok_or(Errno::EINVAL)?;
    // mq_timedsend/mq_timedreceive 的 timeout 是 CLOCK_REALTIME 绝对时间；
    // 换算成单调时钟的 deadline：delta = 目标 - 当前 realtime。
    let now_realtime = crate::vdso::realtime_ns();
    let delta = abs_realtime.saturating_sub(now_realtime);
    Ok(Some(sched::now_ns_direct().saturating_add(delta)))
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

/// `shmctl(SHM_LOCK)` 的 `RLIMIT_MEMLOCK` 检查（Linux `user_shm_lock` 语义）。
///
/// `locked_pages` 是 shm 管理器已锁页数，`additional_pages` 是本次新增页数；
/// 持有 `CAP_IPC_LOCK` 或软上限为 `RLIM_INFINITY` 时豁免。超限返回 `ENOMEM`。
fn check_shm_memlock_limit(
    ctx: &SyscallContext<'_>,
    locked_pages: usize,
    additional_pages: usize,
) -> Result<(), Errno> {
    if ctx.task().credentials().has_cap(sched::Capability::IpcLock) {
        return Ok(());
    }
    let pair = sched::operation::get_rlimit(sched::rlimit::Resource::Memlock)
        .map_err(|_| Errno::ENOMEM)?;
    let limit_pages = (pair.soft.0 / hal::memory::page_size() as u64) as usize;
    if locked_pages.saturating_add(additional_pages) > limit_pages {
        return Err(Errno::ENOMEM);
    }
    Ok(())
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    Some(value.checked_add(align - 1)? & !(align - 1))
}

fn read_u16(raw: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(raw[off..off + 2].try_into().unwrap())
}

fn read_u32(raw: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(raw[off..off + 4].try_into().unwrap())
}

fn read_i32(raw: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(raw[off..off + 4].try_into().unwrap())
}

fn read_u64(raw: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(raw[off..off + 8].try_into().unwrap())
}

fn read_i64(raw: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(raw[off..off + 8].try_into().unwrap())
}

/// 解析 `mq_timedsend`/`mq_timedreceive` 的绝对超时 `timespec`（16 字节）。
fn read_mq_deadline(user: usize) -> Result<Option<u64>, Errno> {
    if user == 0 {
        return Ok(None);
    }
    let mut raw = [0u8; 16];
    copy_from_user(user, &mut raw).map_err(|error| error.as_errno())?;
    let seconds = read_i64(&raw, 0);
    let nanoseconds = read_i64(&raw, 8);
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(Errno::EINVAL);
    }
    let abs_realtime = (seconds as u64)
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds as u64))
        .ok_or(Errno::EINVAL)?;
    // mq_timedsend/mq_timedreceive 的 timeout 是 CLOCK_REALTIME 绝对时间；
    // 换算成单调时钟的 deadline：delta = 目标 - 当前 realtime。
    let now_realtime = crate::vdso::realtime_ns();
    let delta = abs_realtime.saturating_sub(now_realtime);
    Ok(Some(sched::now_ns_direct().saturating_add(delta)))
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
    let mode = if meta.locked {
        meta.perm.mode.bits() as u32 | SHM_LOCKED
    } else {
        meta.perm.mode.bits() as u32
    };
    write_u32(&mut raw, 20, mode);
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

/// 编码 `struct msqid64_ds`（120 字节，asm-generic 64 位布局）。
fn encode_msqid64_ds(meta: &MsgMetadata) -> [u8; MSQID64_DS_SIZE] {
    let mut raw = [0u8; MSQID64_DS_SIZE];
    write_i32(&mut raw, 0, meta.key().0);
    write_u32(&mut raw, 4, meta.uid().0);
    write_u32(&mut raw, 8, meta.gid().0);
    write_u32(&mut raw, 12, meta.cuid().0);
    write_u32(&mut raw, 16, meta.cgid().0);
    write_u32(&mut raw, 20, meta.mode().bits() as u32);
    write_u16(&mut raw, 24, 0);

    // 48 字节 ipc64_perm 之后：stime/rtime/ctime（time64）、cbytes/qnum/qbytes、
    // lspid/lrpid、两个保留字段。
    write_i64(&mut raw, 48, meta.stime);
    write_i64(&mut raw, 56, meta.rtime);
    write_i64(&mut raw, 64, meta.ctime);
    write_u64(&mut raw, 72, meta.bytes as u64);
    write_u64(&mut raw, 80, meta.qnum as u64);
    write_u64(&mut raw, 88, meta.qbytes as u64);
    write_i32(&mut raw, 96, meta.lspid);
    write_i32(&mut raw, 100, meta.lrpid);
    raw
}

/// 编码 `struct semid64_ds`（96 字节，asm-generic 64 位布局）。
fn encode_semid64_ds(meta: &SemMetadata) -> [u8; SEMID64_DS_SIZE] {
    let mut raw = [0u8; SEMID64_DS_SIZE];
    write_i32(&mut raw, 0, meta.key().0);
    write_u32(&mut raw, 4, meta.uid().0);
    write_u32(&mut raw, 8, meta.gid().0);
    write_u32(&mut raw, 12, meta.cuid().0);
    write_u32(&mut raw, 16, meta.cgid().0);
    write_u32(&mut raw, 20, meta.mode().bits() as u32);
    write_u16(&mut raw, 24, 0);

    // 48 字节 ipc64_perm 之后：otime/ctime（time64）、nsems、三个保留字段。
    write_i64(&mut raw, 48, meta.otime);
    write_i64(&mut raw, 56, meta.ctime);
    write_u64(&mut raw, 64, meta.nsems as u64);
    raw
}

/// 编码 `struct seminfo`（40 字节）。`limits` 为真时填 `IPC_INFO` 的系统限制，
/// 否则填 `SEM_INFO` 的当前用量（Linux `ipc/sem.c` 语义）。
fn encode_seminfo(info: &SemSystemInfo, limits: bool) -> [u8; SEMINFO_SIZE] {
    let mut raw = [0u8; SEMINFO_SIZE];
    let (semmap, semmni, semmns, semmnu) = if limits {
        (SEMMNS_LIMIT, SEMMNI_LIMIT, SEMMNS_LIMIT, SEMMNS_LIMIT)
    } else {
        (
            info.sems as i32,
            info.sets as i32,
            info.sems as i32,
            info.sets as i32,
        )
    };
    write_i32(&mut raw, 0, semmap);
    write_i32(&mut raw, 4, semmni);
    write_i32(&mut raw, 8, semmns);
    write_i32(&mut raw, 12, semmnu);
    write_i32(&mut raw, 16, SEMMSL_LIMIT);
    write_i32(&mut raw, 20, SEMOPM_LIMIT);
    write_i32(&mut raw, 24, SEMUME_LIMIT);
    write_i32(&mut raw, 28, SEMUSZ_LIMIT);
    write_i32(&mut raw, 32, SEMVMX_LIMIT);
    write_i32(&mut raw, 36, SEMAEM_LIMIT);
    raw
}

/// 编码 `struct shminfo`（40 字节，`IPC_INFO` 的系统限制）。
fn encode_shminfo(info: &ShmSystemInfo) -> [u8; 40] {
    let mut raw = [0u8; 40];
    write_u64(&mut raw, 0, info.limits.max_segment_size);
    write_u64(&mut raw, 8, info.limits.min_segment_size);
    write_u64(&mut raw, 16, info.limits.max_segments as u64);
    write_u64(&mut raw, 24, info.limits.max_segments as u64);
    write_u64(&mut raw, 32, info.limits.max_total_pages as u64);
    raw
}

/// 编码 `struct shm_info`（48 字节，asm-generic 64 位布局）。
///
/// Linux `shmctl(SHM_INFO)` 返回 `struct shm_info{int used_ids; ulong shm_tot,
/// shm_rss, shm_swp, swap_attempts, swap_successes;}`，与 `IPC_INFO` 的
/// `struct shminfo` 布局不同；`shm_tot`/`shm_rss`/`shm_swp` 以页为单位
/// （`ipcs -u` 打印 "pages allocated/resident/swapped"）。本内核无 swap，
/// 因此 resident == total、swapped 与 swap 尝试/成功恒 0。
fn encode_shm_info(info: &ShmSystemInfo) -> [u8; 48] {
    let mut raw = [0u8; 48];
    let pages = info.total_pages as u64;
    write_i32(&mut raw, 0, info.used_segments as i32);
    write_u64(&mut raw, 8, pages); // shm_tot
    write_u64(&mut raw, 16, pages); // shm_rss
    write_u64(&mut raw, 24, 0); // shm_swp
    write_u64(&mut raw, 32, 0); // swap_attempts
    write_u64(&mut raw, 40, 0); // swap_successes
    raw
}

/// 编码 `struct msginfo`（32 字节）。`limits` 为真时填 `IPC_INFO` 的系统限制，
/// 否则填 `MSG_INFO` 的当前用量（Linux `ipc/msg.c` 语义）。
fn encode_msginfo(info: &MsgSystemInfo, limits: bool) -> [u8; MSGINFO_SIZE] {
    let mut raw = [0u8; MSGINFO_SIZE];
    let (msgpool, msgmap, msgmax) = if limits {
        (MSGPOOL, MSGMAP, MSGMAX as i32)
    } else {
        (info.queues as i32, info.messages as i32, info.bytes as i32)
    };
    write_i32(&mut raw, 0, msgpool);
    write_i32(&mut raw, 4, msgmap);
    write_i32(&mut raw, 8, msgmax);
    write_i32(&mut raw, 12, MSGMNB as i32);
    write_i32(&mut raw, 16, MSGMNI as i32);
    write_i32(&mut raw, 20, MSGSSZ);
    write_i32(&mut raw, 24, MSGTQL);
    write_u16(&mut raw, 28, MSGSEG);
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
