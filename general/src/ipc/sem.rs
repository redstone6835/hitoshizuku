//! System V semaphore 的通用对象管理器。
//!
//! 本模块维护 semaphore set 的身份、权限、值和删除状态。syscall 层负责用户
//! ABI 编解码以及阻塞调度；这里提供可原子重试的批量操作接口，语义对齐
//! Linux `ipc/sem.c`：
//!
//! - 每次 `semop` 整批原子提交，任一条件不满足则整体等待（`IPC_NOWAIT` →
//!   `EAGAIN`）；
//! - 每个 semaphore 维护 `sempid`（最近操作者）、`semncnt`（等待数值增加）、
//!   `semzcnt`（等待数值归零）三个统计字段，`semctl(GETPID/GETNCNT/GETZCNT)`
//!   直接读取；
//! - `SETVAL`/`SETALL`/`IPC_RMID` 后唤醒全部等待者，由等待协议在重试前注销
//!   自己的阻塞登记，计数自然归零；
//! - `SEM_UNDO` 撤销表由 syscall 层在成功提交后登记（见 `sem_undo` 模块），
//!   本模块只负责标志合法性。

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
/// 请求进程退出时回滚操作。撤销表语义见 `sem_undo` 模块。
pub const SEM_UNDO: u16 = 0o10000;
/// 单个 semaphore 的最大值。
pub const SEMVMX: i32 = 32767;
/// 单次 `semop` 允许的最大操作数。
pub const SEMOPM: usize = 500;

/// `semctl` 的查询/设置命令（值相关）。
pub const SEMCTL_GETPID: u32 = 11;
pub const SEMCTL_GETVAL: u32 = 12;
pub const SEMCTL_GETALL: u32 = 13;
pub const SEMCTL_GETNCNT: u32 = 14;
pub const SEMCTL_GETZCNT: u32 = 15;
pub const SEMCTL_SETVAL: u32 = 16;
pub const SEMCTL_SETALL: u32 = 17;
/// `semctl` 的枚举类命令（索引/信息查询族）。
pub const SEM_STAT: u32 = 18;
pub const SEM_INFO: u32 = 19;
pub const SEM_STAT_ANY: u32 = 20;

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

/// 阻塞等待的类别，对应 Linux `semncnt`/`semzcnt` 两种统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemBlockKind {
    /// 等待 `semval >= needed`（`sem_op < 0`）。
    Increment,
    /// 等待 `semval == 0`（`sem_op == 0`）。
    Zero,
}

/// 一次原子批量操作的尝试结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemOpAttempt {
    Applied,
    /// 首个不满足条件的操作；调用方据此登记阻塞统计并决定等待。
    WouldBlock {
        sem_num: usize,
        kind: SemBlockKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemPerm {
    key: SemKey,
    uid: Uid,
    gid: Gid,
    cuid: Uid,
    cgid: Gid,
    mode: FileMode,
}

impl SemPerm {
    fn new(key: SemKey, flags: u32, cred: &Credentials) -> Self {
        Self {
            key,
            uid: cred.euid,
            gid: cred.egid,
            cuid: cred.euid,
            cgid: cred.egid,
            mode: mode_from_flags(flags),
        }
    }
}

/// 单个 semaphore 的运行时状态。
#[derive(Debug, Clone, Copy)]
struct SemValue {
    val: i32,
    /// 最近一次 `semop`/`SETVAL`/`SETALL` 操作者的 pid。
    sempid: i32,
    /// 等待 `semval` 增加（`sem_op < 0` 被阻塞）的任务数。
    semncnt: u32,
    /// 等待 `semval == 0`（`sem_op == 0` 被阻塞）的任务数。
    semzcnt: u32,
}

struct SemSetInner {
    perm: SemPerm,
    values: Vec<SemValue>,
    /// 最近一次 `semop` 成功时间（秒）。
    otime: i64,
    /// 创建/`IPC_SET`/`SETVAL`/`SETALL` 时间（秒）。
    ctime: i64,
    removed: bool,
}

/// 单个 semaphore set。阻塞者持有该对象的 `Arc`，因此集合删除后仍能观察到
/// `EIDRM`，而不会误认成后来复用同一整数 id 的新集合。
pub struct SemSet {
    inner: Mutex<SemSetInner>,
    waiters: WaitQueue,
}

impl SemSet {
    fn new(key: SemKey, nsems: usize, flags: u32, cred: &Credentials, now_sec: i64) -> Self {
        Self {
            inner: Mutex::new(SemSetInner {
                perm: SemPerm::new(key, flags, cred),
                values: vec![
                    SemValue {
                        val: 0,
                        sempid: 0,
                        semncnt: 0,
                        semzcnt: 0
                    };
                    nsems
                ],
                otime: 0,
                ctime: now_sec,
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
    ///
    /// 成功时把涉及到的 semaphore 的 `sempid` 更新为 `pid`、`otime` 更新为
    /// `now_sec`。返回 [`SemOpAttempt::WouldBlock`] 时携带首个阻塞操作的位置，
    /// 供调用方登记 `semncnt`/`semzcnt`。
    pub fn try_apply(
        &self,
        operations: &[SemOperation],
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
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
        let mut first_blocked: Option<(usize, SemBlockKind)> = None;
        for operation in operations {
            if operation.sem_flg & !(IPC_NOWAIT | SEM_UNDO) != 0 {
                return Err(Errno::EINVAL);
            }

            let value = next_values
                .get_mut(operation.sem_num as usize)
                .ok_or(Errno::EFBIG)?;
            let delta = i32::from(operation.sem_op);
            if delta > 0 {
                let updated = value.val.checked_add(delta).ok_or(Errno::ERANGE)?;
                if updated > SEMVMX {
                    return Err(Errno::ERANGE);
                }
                value.val = updated;
            } else if delta < 0 {
                let needed = -delta;
                if value.val < needed {
                    if operation.sem_flg & IPC_NOWAIT != 0 {
                        return Err(Errno::EAGAIN);
                    }
                    first_blocked = first_blocked.or(Some((
                        operation.sem_num as usize,
                        SemBlockKind::Increment,
                    )));
                    break;
                }
                value.val -= needed;
            } else if value.val != 0 {
                if operation.sem_flg & IPC_NOWAIT != 0 {
                    return Err(Errno::EAGAIN);
                }
                first_blocked =
                    first_blocked.or(Some((operation.sem_num as usize, SemBlockKind::Zero)));
                break;
            }
        }

        let Some((sem_num, kind)) = first_blocked else {
            inner.values = next_values;
            for operation in operations {
                if let Some(value) = inner.values.get_mut(operation.sem_num as usize) {
                    value.sempid = pid;
                }
            }
            inner.otime = now_sec;
            return Ok(SemOpAttempt::Applied);
        };
        // 批量操作整体原子：任一条件不满足时整个批次都不提交（next_values 被丢弃）。
        Ok(SemOpAttempt::WouldBlock { sem_num, kind })
    }

    /// 登记一次阻塞等待（`semncnt`/`semzcnt` 统计）。
    ///
    /// 调用方在准备睡眠前调用；唤醒/出错离开等待循环时必须用
    /// [`SemSet::unregister_blocked`] 注销。集合已删除时返回 `EIDRM`。
    pub fn register_blocked(&self, sem_num: usize, kind: SemBlockKind) -> Result<(), Errno> {
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        let value = inner.values.get_mut(sem_num).ok_or(Errno::EFBIG)?;
        match kind {
            SemBlockKind::Increment => value.semncnt = value.semncnt.saturating_add(1),
            SemBlockKind::Zero => value.semzcnt = value.semzcnt.saturating_add(1),
        }
        Ok(())
    }

    /// 注销一次阻塞等待。幂等：计数不会低于 0。
    pub fn unregister_blocked(&self, sem_num: usize, kind: SemBlockKind) {
        let mut inner = self.inner.lock();
        if let Some(value) = inner.values.get_mut(sem_num) {
            match kind {
                SemBlockKind::Increment => value.semncnt = value.semncnt.saturating_sub(1),
                SemBlockKind::Zero => value.semzcnt = value.semzcnt.saturating_sub(1),
            }
        }
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
        inner
            .values
            .get(sem_num)
            .map(|value| value.val)
            .ok_or(Errno::EINVAL)
    }

    /// `GETALL`：读出整个集合（读权限）。
    pub fn get_all(&self, cred: &Credentials) -> Result<Vec<i32>, Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if !cred.can_read(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        Ok(inner.values.iter().map(|value| value.val).collect())
    }

    fn get_pid(&self, sem_num: usize, cred: &Credentials) -> Result<i32, Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if !cred.can_read(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        inner
            .values
            .get(sem_num)
            .map(|value| value.sempid)
            .ok_or(Errno::EINVAL)
    }

    fn get_ncnt(&self, sem_num: usize, cred: &Credentials) -> Result<u32, Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if !cred.can_read(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        inner
            .values
            .get(sem_num)
            .map(|value| value.semncnt)
            .ok_or(Errno::EINVAL)
    }

    fn get_zcnt(&self, sem_num: usize, cred: &Credentials) -> Result<u32, Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if !cred.can_read(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        inner
            .values
            .get(sem_num)
            .map(|value| value.semzcnt)
            .ok_or(Errno::EINVAL)
    }

    fn set_value(
        &self,
        sem_num: usize,
        value: i32,
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
    ) -> Result<(), Errno> {
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
        let slot = inner.values.get_mut(sem_num).ok_or(Errno::EINVAL)?;
        slot.val = value;
        slot.sempid = pid;
        inner.ctime = now_sec;
        Ok(())
    }

    /// `SETALL`：整体覆盖集合（写权限，逐值 `ERANGE` 校验）。
    pub fn set_all(
        &self,
        values: &[i32],
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
    ) -> Result<(), Errno> {
        if values.len() != self.nsems() {
            return Err(Errno::EINVAL);
        }
        if values.iter().any(|value| !(0..=SEMVMX).contains(value)) {
            return Err(Errno::ERANGE);
        }
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if !cred.can_write(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        for (slot, value) in inner.values.iter_mut().zip(values) {
            slot.val = *value;
            slot.sempid = pid;
        }
        inner.ctime = now_sec;
        Ok(())
    }

    /// `IPC_STAT`/`SEM_STAT` 快照（要求读权限）。
    pub fn stat(&self, cred: &Credentials) -> Result<SemMetadata, Errno> {
        self.stat_inner(cred, true)
    }

    /// `SEM_STAT_ANY` 快照（不检查读权限）。
    pub fn stat_any(&self) -> Result<SemMetadata, Errno> {
        self.stat_inner(&Credentials::root(), false)
    }

    fn stat_inner(&self, cred: &Credentials, check_perms: bool) -> Result<SemMetadata, Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if check_perms && !cred.can_read(inner.perm.uid, inner.perm.gid, inner.perm.mode) {
            return Err(Errno::EACCES);
        }
        Ok(SemMetadata {
            perm: inner.perm,
            otime: inner.otime,
            ctime: inner.ctime,
            nsems: inner.values.len(),
        })
    }

    /// `IPC_SET`：更新权限元数据。
    pub fn set_perm(
        &self,
        uid: Option<Uid>,
        gid: Option<Gid>,
        mode: Option<FileMode>,
        cred: &Credentials,
        now_sec: i64,
    ) -> Result<(), Errno> {
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        check_control_owner(cred, &inner.perm)?;
        if let Some(uid) = uid {
            inner.perm.uid = uid;
        }
        if let Some(gid) = gid {
            inner.perm.gid = gid;
        }
        if let Some(mode) = mode {
            inner.perm.mode = mode.mask(FileMode::PERM_MASK);
        }
        inner.ctime = now_sec;
        Ok(())
    }
}

/// `IPC_STAT`/`SEM_STAT` 快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemMetadata {
    pub perm: SemPerm,
    pub otime: i64,
    pub ctime: i64,
    pub nsems: usize,
}

impl SemMetadata {
    pub fn key(&self) -> SemKey {
        self.perm.key
    }
    pub fn uid(&self) -> Uid {
        self.perm.uid
    }
    pub fn gid(&self) -> Gid {
        self.perm.gid
    }
    pub fn cuid(&self) -> Uid {
        self.perm.cuid
    }
    pub fn cgid(&self) -> Gid {
        self.perm.cgid
    }
    pub fn mode(&self) -> FileMode {
        self.perm.mode
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
        now_sec: i64,
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
        let set = Arc::new(SemSet::new(key, nsems, flags, cred, now_sec));
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

    /// `GETALL`。
    pub fn get_all(&self, id: SemId, cred: &Credentials) -> Result<Vec<i32>, Errno> {
        self.set_for_operation(id)?.get_all(cred)
    }

    pub fn get_pid(&self, id: SemId, sem_num: usize, cred: &Credentials) -> Result<i32, Errno> {
        self.set_for_operation(id)?.get_pid(sem_num, cred)
    }

    pub fn get_ncnt(&self, id: SemId, sem_num: usize, cred: &Credentials) -> Result<u32, Errno> {
        self.set_for_operation(id)?.get_ncnt(sem_num, cred)
    }

    pub fn get_zcnt(&self, id: SemId, sem_num: usize, cred: &Credentials) -> Result<u32, Errno> {
        self.set_for_operation(id)?.get_zcnt(sem_num, cred)
    }

    /// 设置单个值。调用方成功后必须唤醒该集合的等待者。
    pub fn set_value(
        &self,
        id: SemId,
        sem_num: usize,
        value: i32,
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
    ) -> Result<Arc<SemSet>, Errno> {
        let set = self.set_for_operation(id)?;
        set.set_value(sem_num, value, cred, pid, now_sec)?;
        Ok(set)
    }

    /// `SETALL`。调用方成功后必须唤醒该集合的等待者。
    pub fn set_all(
        &self,
        id: SemId,
        values: &[i32],
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
    ) -> Result<Arc<SemSet>, Errno> {
        let set = self.set_for_operation(id)?;
        set.set_all(values, cred, pid, now_sec)?;
        Ok(set)
    }

    /// `IPC_STAT`/`SEM_STAT`。
    pub fn stat(&self, id: SemId, cred: &Credentials) -> Result<SemMetadata, Errno> {
        self.set_for_operation(id)?.stat(cred)
    }

    /// `SEM_STAT_ANY`（无权限检查）。
    pub fn stat_any(&self, id: SemId) -> Result<SemMetadata, Errno> {
        self.set_for_operation(id)?.stat_any()
    }

    /// `IPC_SET`。
    pub fn set_perm(
        &self,
        id: SemId,
        uid: Option<Uid>,
        gid: Option<Gid>,
        mode: Option<FileMode>,
        cred: &Credentials,
        now_sec: i64,
    ) -> Result<(), Errno> {
        self.set_for_operation(id)?
            .set_perm(uid, gid, mode, cred, now_sec)
    }

    /// `SEM_STAT`/`SEM_STAT_ANY`：按 id 排序后的序号取集合。
    pub fn set_by_index(&self, index: i32) -> Result<(SemId, Arc<SemSet>), Errno> {
        if index < 0 {
            return Err(Errno::EINVAL);
        }
        let state = self.state.lock();
        let Some((&id, set)) = state.by_id.iter().nth(index as usize) else {
            return Err(Errno::EINVAL);
        };
        Ok((id, Arc::clone(set)))
    }

    /// `IPC_INFO`/`SEM_INFO` 系统级诊断快照。
    pub fn info(&self) -> SemSystemInfo {
        let state = self.state.lock();
        let mut sems = 0usize;
        for set in state.by_id.values() {
            sems += set.nsems();
        }
        SemSystemInfo {
            sets: state.by_id.len(),
            sems,
            max_index: state.by_id.keys().next_back().map_or(0, |id| id.0),
        }
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

/// `IPC_INFO`/`SEM_INFO` 返回的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemSystemInfo {
    pub sets: usize,
    pub sems: usize,
    pub max_index: i32,
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
    // CAP_IPC_OWNER 允许绕过 SysV IPC 对象的权限检查。
    if cred.has_cap(Capability::IpcOwner) {
        return Ok(());
    }
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

#[cfg(feature = "kernel-tests")]
mod tests {
    use super::*;
    use ktest::ktest;

    fn operation(sem_num: u16, sem_op: i16, sem_flg: u16) -> SemOperation {
        SemOperation {
            sem_num,
            sem_op,
            sem_flg,
        }
    }

    fn create(manager: &SemManager, nsems: usize, key: SemKey) -> SemId {
        manager
            .semget(key, nsems, IPC_CREAT | 0o700, &Credentials::root(), 0)
            .expect("创建 semaphore set")
    }

    #[ktest]
    fn semaphore_batch_is_atomic() {
        let manager = SemManager::new();
        let cred = Credentials::root();
        let id = create(&manager, 2, SemKey::PRIVATE);
        let set = manager.set_for_operation(id).expect("查找 semaphore set");

        assert_eq!(
            set.try_apply(&[operation(0, 1, 0), operation(0, -2, 0)], &cred, 7, 0),
            Ok(SemOpAttempt::WouldBlock {
                sem_num: 1,
                kind: SemBlockKind::Increment
            })
        );
        assert_eq!(manager.get_value(id, 0, &cred), Ok(0));

        assert_eq!(
            set.try_apply(&[operation(0, 2, 0), operation(0, -1, 0)], &cred, 7, 0),
            Ok(SemOpAttempt::Applied)
        );
        assert_eq!(manager.get_value(id, 0, &cred), Ok(1));
        assert_eq!(manager.get_pid(id, 0, &cred), Ok(7));
    }

    #[ktest]
    fn semaphore_nowait_does_not_change_value() {
        let manager = SemManager::new();
        let cred = Credentials::root();
        let id = create(&manager, 1, SemKey::PRIVATE);
        let set = manager.set_for_operation(id).expect("查找 semaphore set");

        assert_eq!(
            set.try_apply(&[operation(0, -1, IPC_NOWAIT)], &cred, 7, 0),
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
            .semget(key, 2, IPC_CREAT | 0o700, &cred, 0)
            .expect("创建 keyed semaphore set");

        assert_eq!(manager.semget(key, 0, 0, &cred, 0), Ok(id));
        assert_eq!(
            manager.semget(key, 2, IPC_CREAT | IPC_EXCL | 0o700, &cred, 0),
            Err(errno::Errno::EEXIST)
        );

        let stable_set = manager.set_for_operation(id).expect("保留稳定对象引用");
        manager.remove(id, &cred).expect("删除 semaphore set");
        assert!(matches!(
            stable_set.try_apply(&[operation(0, 1, 0)], &cred, 7, 0),
            Err(errno::Errno::EIDRM)
        ));
        assert!(matches!(
            manager.set_for_operation(id),
            Err(errno::Errno::EINVAL)
        ));
    }

    #[ktest]
    fn semaphore_getall_setall_and_stat() {
        let manager = SemManager::new();
        let cred = Credentials::root();
        let id = create(&manager, 3, SemKey::PRIVATE);
        let set = manager.set_for_operation(id).expect("查找 semaphore set");

        manager
            .set_value(id, 0, 5, &cred, 11, 100)
            .expect("SETVAL");
        set.try_apply(&[operation(1, 3, 0)], &cred, 12, 200)
            .expect("semop");
        assert_eq!(manager.get_all(id, &cred), Ok(vec![5, 3, 0]));
        assert_eq!(manager.get_pid(id, 0, &cred), Ok(11));
        assert_eq!(manager.get_pid(id, 1, &cred), Ok(12));

        manager
            .set_all(id, &[9, 8, 7], &cred, 13, 300)
            .expect("SETALL");
        assert_eq!(manager.get_all(id, &cred), Ok(vec![9, 8, 7]));

        let stat = manager.stat(id, &cred).expect("IPC_STAT");
        assert_eq!(stat.nsems, 3);
        assert_eq!(stat.ctime, 300);
        assert_eq!(stat.otime, 200);
        assert_eq!(stat.uid(), Uid::ROOT);

        let (found_id, _) = manager.set_by_index(0).expect("SEM_STAT");
        assert_eq!(found_id, id);
        assert!(matches!(manager.set_by_index(5), Err(errno::Errno::EINVAL)));
    }

    #[ktest]
    fn semaphore_blocked_counters_are_maintained() {
        let manager = SemManager::new();
        let cred = Credentials::root();
        let id = create(&manager, 1, SemKey::PRIVATE);
        let set = manager.set_for_operation(id).expect("查找 semaphore set");

        // 模拟 syscall 层：尝试 -1 失败后登记阻塞，再注销。
        assert!(matches!(
            set.try_apply(&[operation(0, -1, 0)], &cred, 7, 0),
            Ok(SemOpAttempt::WouldBlock {
                sem_num: 0,
                kind: SemBlockKind::Increment
            })
        ));
        set.register_blocked(0, SemBlockKind::Increment)
            .expect("登记阻塞");
        assert_eq!(manager.get_ncnt(id, 0, &cred), Ok(1));
        assert_eq!(manager.get_zcnt(id, 0, &cred), Ok(0));

        // 第二个任务等待归零。
        assert!(matches!(
            set.try_apply(&[operation(0, 0, 0)], &cred, 8, 0),
            Ok(SemOpAttempt::WouldBlock {
                sem_num: 0,
                kind: SemBlockKind::Zero
            })
        ));
        set.register_blocked(0, SemBlockKind::Zero).expect("登记");
        assert_eq!(manager.get_zcnt(id, 0, &cred), Ok(1));

        set.unregister_blocked(0, SemBlockKind::Increment);
        set.unregister_blocked(0, SemBlockKind::Zero);
        assert_eq!(manager.get_ncnt(id, 0, &cred), Ok(0));
        assert_eq!(manager.get_zcnt(id, 0, &cred), Ok(0));
    }
}
