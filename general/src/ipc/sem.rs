//! System V semaphore 的通用对象管理器。
//!
//! 本模块维护 semaphore set 的身份、权限、值和删除状态。syscall 层负责用户
//! ABI 编解码以及阻塞调度；这里提供可原子重试的批量操作接口。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use errno::Errno;
use sched::WaitQueue;
use spin::Mutex;
use vfs::cred::{Capability, Credentials, Gid, Uid};
use vfs::stat::FileMode;

use super::shm::{IPC_CREAT, IPC_EXCL, IPC_PRIVATE};

/// `semop` 非阻塞标志。
pub const IPC_NOWAIT: u16 = 0o4000;
/// 请求进程退出时回滚操作。当前实现不维护进程级 undo 表，因此明确拒绝。
pub const SEM_UNDO: u16 = 0o10000;
/// 单个 semaphore 的最大值。
pub const SEMVMX: i32 = 32767;
/// 单次 `semop` 允许的最大操作数。
pub const SEMOPM: usize = 500;

const FIRST_SEM_ID: i32 = 1;
const SEMMSL: usize = 32_000;
const SEMMNI: usize = 32_000;

/// SysV semaphore set id。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemId(pub i32);

/// SysV semaphore key。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemKey(pub i32);

impl SemKey {
    /// `IPC_PRIVATE` 每次都创建新集合，不进入 key 查找表。
    pub const PRIVATE: Self = Self(IPC_PRIVATE);
}

/// 一项 `struct sembuf` 操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemOperation {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: u16,
}

/// 一次原子批量操作的尝试结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemOpAttempt {
    Applied,
    WouldBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemPerm {
    key: SemKey,
    uid: Uid,
    gid: Gid,
    cuid: Uid,
    mode: FileMode,
}

impl SemPerm {
    fn new(key: SemKey, flags: u32, cred: &Credentials) -> Self {
        Self {
            key,
            uid: cred.euid,
            gid: cred.egid,
            cuid: cred.euid,
            mode: mode_from_flags(flags),
        }
    }
}

struct SemSetInner {
    perm: SemPerm,
    values: Vec<i32>,
    removed: bool,
}

/// 单个 semaphore set。阻塞者持有该对象的 `Arc`，因此集合删除后仍能观察到
/// `EIDRM`，而不会误认成后来复用同一整数 id 的新集合。
pub struct SemSet {
    inner: Mutex<SemSetInner>,
    waiters: WaitQueue,
}

impl SemSet {
    fn new(key: SemKey, nsems: usize, flags: u32, cred: &Credentials) -> Self {
        Self {
            inner: Mutex::new(SemSetInner {
                perm: SemPerm::new(key, flags, cred),
                values: vec![0; nsems],
                removed: false,
            }),
            waiters: WaitQueue::new(),
        }
    }

    /// 返回该集合的等待队列，供 syscall 层执行 prepare/recheck/sleep 协议。
    pub fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    /// 原子尝试一批操作。只有所有条件均满足时才提交临时值。
    pub fn try_apply(
        &self,
        operations: &[SemOperation],
        cred: &Credentials,
    ) -> Result<SemOpAttempt, Errno> {
        if operations.is_empty() {
            return Err(Errno::EINVAL);
        }
        if operations.len() > SEMOPM {
            return Err(Errno::E2BIG);
        }

        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        check_operation_permissions(cred, &inner.perm, operations)?;

        let mut next_values = inner.values.clone();
        for operation in operations {
            if operation.sem_flg & SEM_UNDO != 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            if operation.sem_flg & !(IPC_NOWAIT | SEM_UNDO) != 0 {
                return Err(Errno::EINVAL);
            }

            let value = next_values
                .get_mut(operation.sem_num as usize)
                .ok_or(Errno::EFBIG)?;
            let delta = i32::from(operation.sem_op);
            if delta > 0 {
                let updated = value.checked_add(delta).ok_or(Errno::ERANGE)?;
                if updated > SEMVMX {
                    return Err(Errno::ERANGE);
                }
                *value = updated;
            } else if delta < 0 {
                let needed = -delta;
                if *value < needed {
                    if operation.sem_flg & IPC_NOWAIT != 0 {
                        return Err(Errno::EAGAIN);
                    }
                    return Ok(SemOpAttempt::WouldBlock);
                }
                *value -= needed;
            } else if *value != 0 {
                if operation.sem_flg & IPC_NOWAIT != 0 {
                    return Err(Errno::EAGAIN);
                }
                return Ok(SemOpAttempt::WouldBlock);
            }
        }

        inner.values = next_values;
        Ok(SemOpAttempt::Applied)
    }

    fn nsems(&self) -> usize {
        self.inner.lock().values.len()
    }

    fn check_requested_mode(&self, flags: u32, cred: &Credentials) -> Result<(), Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        check_mode_request(cred, &inner.perm, flags)
    }

    fn get_value(&self, sem_num: usize, cred: &Credentials) -> Result<i32, Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if !cred.can_read(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        inner.values.get(sem_num).copied().ok_or(Errno::EINVAL)
    }

    fn set_value(&self, sem_num: usize, value: i32, cred: &Credentials) -> Result<(), Errno> {
        if !(0..=SEMVMX).contains(&value) {
            return Err(Errno::ERANGE);
        }
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if !cred.can_write(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        *inner.values.get_mut(sem_num).ok_or(Errno::EINVAL)? = value;
        Ok(())
    }
}

struct SemManagerState {
    by_id: BTreeMap<SemId, Arc<SemSet>>,
    by_key: BTreeMap<SemKey, SemId>,
    next_id: i32,
}

impl SemManagerState {
    fn new() -> Self {
        Self {
            by_id: BTreeMap::new(),
            by_key: BTreeMap::new(),
            next_id: FIRST_SEM_ID,
        }
    }
}

/// SysV semaphore 全局管理器。
pub struct SemManager {
    state: Mutex<SemManagerState>,
}

impl SemManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SemManagerState::new()),
        }
    }

    /// `IPC_PRIVATE` 总是创建；普通 key 按 `IPC_CREAT/IPC_EXCL` 查找或创建。
    pub fn semget(
        &self,
        key: SemKey,
        nsems: usize,
        flags: u32,
        cred: &Credentials,
    ) -> Result<SemId, Errno> {
        let mut state = self.state.lock();
        if key != SemKey::PRIVATE {
            if let Some(id) = state.by_key.get(&key).copied() {
                let set = state.by_id.get(&id).ok_or(Errno::EINVAL)?;
                if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                    return Err(Errno::EEXIST);
                }
                if nsems > set.nsems() {
                    return Err(Errno::EINVAL);
                }
                set.check_requested_mode(flags, cred)?;
                return Ok(id);
            }
            if flags & IPC_CREAT == 0 {
                return Err(Errno::ENOENT);
            }
        }

        if nsems == 0 || nsems > SEMMSL {
            return Err(Errno::EINVAL);
        }
        if state.by_id.len() >= SEMMNI {
            return Err(Errno::ENOSPC);
        }
        let id = allocate_id(&mut state)?;
        let set = Arc::new(SemSet::new(key, nsems, flags, cred));
        state.by_id.insert(id, set);
        if key != SemKey::PRIVATE {
            state.by_key.insert(key, id);
        }
        Ok(id)
    }

    /// 获取一次 `semop` 使用的稳定对象引用。
    pub fn set_for_operation(&self, id: SemId) -> Result<Arc<SemSet>, Errno> {
        self.state
            .lock()
            .by_id
            .get(&id)
            .cloned()
            .ok_or(Errno::EINVAL)
    }

    pub fn get_value(&self, id: SemId, sem_num: usize, cred: &Credentials) -> Result<i32, Errno> {
        self.set_for_operation(id)?.get_value(sem_num, cred)
    }

    /// 设置单个值。调用方成功后必须唤醒该集合的等待者。
    pub fn set_value(
        &self,
        id: SemId,
        sem_num: usize,
        value: i32,
        cred: &Credentials,
    ) -> Result<Arc<SemSet>, Errno> {
        let set = self.set_for_operation(id)?;
        set.set_value(sem_num, value, cred)?;
        Ok(set)
    }

    /// 删除集合并标记稳定对象，使已阻塞任务返回 `EIDRM`。
    pub fn remove(&self, id: SemId, cred: &Credentials) -> Result<Arc<SemSet>, Errno> {
        let mut state = self.state.lock();
        let set = state.by_id.get(&id).cloned().ok_or(Errno::EINVAL)?;
        let key = {
            let mut inner = set.inner.lock();
            check_control_owner(cred, &inner.perm)?;
            inner.removed = true;
            inner.perm.key
        };
        state.by_id.remove(&id);
        if key != SemKey::PRIVATE {
            state.by_key.remove(&key);
        }
        Ok(set)
    }
}

impl Default for SemManager {
    fn default() -> Self {
        Self::new()
    }
}

fn allocate_id(state: &mut SemManagerState) -> Result<SemId, Errno> {
    for _ in 0..SEMMNI {
        let raw = state.next_id;
        state.next_id = if raw == i32::MAX {
            FIRST_SEM_ID
        } else {
            raw + 1
        };
        let id = SemId(raw);
        if !state.by_id.contains_key(&id) {
            return Ok(id);
        }
    }
    Err(Errno::ENOSPC)
}

fn mode_from_flags(flags: u32) -> FileMode {
    FileMode::new((flags as u16) & FileMode::PERM_MASK.bits())
}

fn check_mode_request(cred: &Credentials, perm: &SemPerm, flags: u32) -> Result<(), Errno> {
    let requested = mode_from_flags(flags);
    if requested.has_any(FileMode::IRUSR.with(FileMode::IRGRP).with(FileMode::IROTH))
        && !cred.can_read(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    if requested.has_any(FileMode::IWUSR.with(FileMode::IWGRP).with(FileMode::IWOTH))
        && !cred.can_write(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    Ok(())
}

fn check_operation_permissions(
    cred: &Credentials,
    perm: &SemPerm,
    operations: &[SemOperation],
) -> Result<(), Errno> {
    if operations.iter().any(|operation| operation.sem_op == 0)
        && !cred.can_read(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    if operations.iter().any(|operation| operation.sem_op != 0)
        && !cred.can_write(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    Ok(())
}

fn check_control_owner(cred: &Credentials, perm: &SemPerm) -> Result<(), Errno> {
    if cred.is_owner(perm.uid) || cred.is_owner(perm.cuid) || cred.has_cap(Capability::SysAdmin) {
        return Ok(());
    }
    Err(Errno::EPERM)
}
