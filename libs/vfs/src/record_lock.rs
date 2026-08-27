//! POSIX / OFD byte-range advisory record lock 管理器。
//!
//! 这里实现 `fcntl(F_GETLK/F_SETLK/F_SETLKW)` 与 `fcntl(F_OFD_GETLK/F_OFD_SETLK/
//! F_OFD_SETLKW)` 背后的 VFS 级记录锁。它和 BSD `flock(2)` 是两套独立语义。
//!
//! 两类 record lock 的 owner 语义不同：
//! - **POSIX 锁**的 owner 是进程：同一进程在同一 inode 上的新锁会替换/合并旧锁，
//!   任意指向该 inode 的 fd 关闭时都要释放该进程的记录锁；`F_GETLK` 返回冲突
//!   进程 pid。
//! - **OFD 锁**的 owner 是打开文件描述（open file description）：`fork`/`dup`
//!   共享同一 OFD 的进程可以协同解锁，只有最后一个引用该 OFD 的 fd 关闭时才
//!   释放；`F_OFD_GETLK` 返回 `l_pid == -1`。

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::{max, min};
use core::ops::Range;

use errno::Errno;
use sched::{TaskState, WaitQueue};

use crate::vfs::file::File;
use crate::vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LockKey {
    fs_id: u64,
    ino: u64,
}

impl LockKey {
    fn from_file(file: &File) -> Self {
        let inode = file.inode();
        Self {
            fs_id: inode.fs_id().raw(),
            ino: inode.ino(),
        }
    }
}

/// POSIX/OFD record lock 类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordLockType {
    Read,
    Write,
    Unlock,
}

/// 用户态 `struct flock` 解析后的通用范围描述。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordLockRequest {
    pub lock_type: RecordLockType,
    pub start: u64,
    /// `None` 表示锁到 EOF。底层记录锁统一用半开区间，避免 `u64::MAX`
    /// 参与 `end + 1` 这类易溢出计算。
    pub end: Option<u64>,
}

impl RecordLockRequest {
    pub fn new(lock_type: RecordLockType, start: u64, len: u64) -> Self {
        let end = if len == 0 {
            None
        } else {
            Some(start.saturating_add(len))
        };
        Self {
            lock_type,
            start,
            end,
        }
    }
}

/// `F_GETLK` 返回给 ABI 层的冲突锁摘要。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordLockConflict {
    pub lock_type: RecordLockType,
    pub start: u64,
    pub end: Option<u64>,
    pub owner_pid: i32,
}

/// owner 种类：进程（POSIX）或打开文件描述（OFD）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum OwnerKind {
    Process,
    Ofd,
}

/// record lock 的 owner 身份。`id` 对 Process 是 pid，对 Ofd 是打开文件描述
/// （`Arc<File>` 数据块地址）的唯一标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RecordOwner {
    kind: OwnerKind,
    id: u64,
}

impl RecordOwner {
    fn process(pid: i32) -> Self {
        Self {
            kind: OwnerKind::Process,
            id: pid as u32 as u64,
        }
    }

    fn ofd(ptr: usize) -> Self {
        Self {
            kind: OwnerKind::Ofd,
            id: ptr as u64,
        }
    }

    fn pid_or_neg_one(self) -> i32 {
        match self.kind {
            OwnerKind::Process => self.id as u32 as i32,
            OwnerKind::Ofd => -1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RecordLock {
    start: u64,
    end: Option<u64>,
    lock_type: RecordLockType,
    owner: RecordOwner,
}

#[derive(Clone, Copy, Debug)]
struct PendingLock {
    key: LockKey,
    req: RecordLockRequest,
}

impl RecordLock {
    fn overlaps(self, req: &RecordLockRequest) -> bool {
        ranges_overlap(self.start, self.end, req.start, req.end)
    }

    fn conflicts(self, owner: RecordOwner, req: &RecordLockRequest) -> bool {
        self.owner != owner
            && self.lock_type != RecordLockType::Unlock
            && req.lock_type != RecordLockType::Unlock
            && (self.lock_type == RecordLockType::Write || req.lock_type == RecordLockType::Write)
            && self.overlaps(req)
    }

    fn conflict_view(self) -> RecordLockConflict {
        RecordLockConflict {
            lock_type: self.lock_type,
            start: self.start,
            end: self.end,
            owner_pid: self.owner.pid_or_neg_one(),
        }
    }
}

struct RecordLockState {
    locks: Vec<RecordLock>,
    waiters: Arc<WaitQueue>,
}

impl RecordLockState {
    fn new() -> Self {
        Self {
            locks: Vec::new(),
            waiters: Arc::new(WaitQueue::new()),
        }
    }

    fn first_conflict_owner(
        &self,
        owner: RecordOwner,
        req: &RecordLockRequest,
    ) -> Option<RecordOwner> {
        self.locks
            .iter()
            .copied()
            .filter(|lock| lock.conflicts(owner, req))
            .min_by_key(|lock| lock.start)
            .map(|lock| lock.owner)
    }

    fn first_conflict(
        &self,
        owner: RecordOwner,
        req: &RecordLockRequest,
    ) -> Option<RecordLockConflict> {
        self.locks
            .iter()
            .copied()
            .filter(|lock| lock.conflicts(owner, req))
            .min_by_key(|lock| lock.start)
            .map(RecordLock::conflict_view)
    }

    fn apply(&mut self, owner: RecordOwner, req: RecordLockRequest) {
        let mut out = Vec::with_capacity(self.locks.len().saturating_add(1));
        for lock in self.locks.drain(..) {
            if lock.owner != owner || !lock.overlaps(&req) {
                out.push(lock);
                continue;
            }

            // 同一 owner 的新锁会替换重叠区间。先把旧锁在请求区间两侧的残段保留，
            // 请求区间本身稍后按新类型插入；Unlock 则只保留残段。
            if let Some(left) = left_piece(lock, &req) {
                out.push(left);
            }
            if let Some(right) = right_piece(lock, &req) {
                out.push(right);
            }
        }

        if req.lock_type != RecordLockType::Unlock {
            out.push(RecordLock {
                start: req.start,
                end: req.end,
                lock_type: req.lock_type,
                owner,
            });
        }

        self.locks = normalize(out);
    }

    fn release_owner(&mut self, owner: RecordOwner) -> bool {
        let before = self.locks.len();
        self.locks.retain(|lock| lock.owner != owner);
        before != self.locks.len()
    }
}

static RECORD_LOCKS: Spinlock<BTreeMap<LockKey, RecordLockState>> = Spinlock::new(BTreeMap::new());
static PENDING_LOCKS: Spinlock<BTreeMap<RecordOwner, PendingLock>> = Spinlock::new(BTreeMap::new());

/// 查询一次 `F_GETLK` 冲突（POSIX owner）。
pub fn getlk(file: &File, owner_pid: i32, req: RecordLockRequest) -> Option<RecordLockConflict> {
    getlk_inner(file, RecordOwner::process(owner_pid), req)
}

/// 查询一次 `F_OFD_GETLK` 冲突（OFD owner；冲突 owner 以 `l_pid == -1` 返回）。
pub fn getlk_ofd(
    file: &File,
    ofd_ptr: usize,
    req: RecordLockRequest,
) -> Option<RecordLockConflict> {
    getlk_inner(file, RecordOwner::ofd(ofd_ptr), req)
}

fn getlk_inner(
    file: &File,
    owner: RecordOwner,
    req: RecordLockRequest,
) -> Option<RecordLockConflict> {
    if req.lock_type == RecordLockType::Unlock {
        return None;
    }
    let key = LockKey::from_file(file);
    RECORD_LOCKS
        .lock()
        .get(&key)
        .and_then(|state| state.first_conflict(owner, &req))
}

/// 执行 `F_SETLK/F_SETLKW`（POSIX owner）。
pub fn setlk(file: &File, owner_pid: i32, req: RecordLockRequest, wait: bool) -> Result<(), Errno> {
    setlk_inner(file, RecordOwner::process(owner_pid), req, wait)
}

/// 执行 `F_OFD_SETLK/F_OFD_SETLKW`（OFD owner）。
pub fn setlk_ofd(
    file: &File,
    ofd_ptr: usize,
    req: RecordLockRequest,
    wait: bool,
) -> Result<(), Errno> {
    setlk_inner(file, RecordOwner::ofd(ofd_ptr), req, wait)
}

/// `wait=false` 表示非阻塞；遇到冲突返回 `EAGAIN`。阻塞路径采用 prepare /
/// recheck / sleep 协议，避免释放者在检查后唤醒前到达造成丢唤醒。
fn setlk_inner(
    file: &File,
    owner: RecordOwner,
    req: RecordLockRequest,
    wait: bool,
) -> Result<(), Errno> {
    let key = LockKey::from_file(file);
    let task = sched::current_task();

    loop {
        let (wait_entry, wake) = {
            let mut table = RECORD_LOCKS.lock();
            let state = table.entry(key).or_insert_with(RecordLockState::new);
            if state.first_conflict(owner, &req).is_none() {
                state.apply(owner, req);
                let wake = if state.waiters.len_hint() != 0 {
                    Some(Arc::clone(&state.waiters))
                } else {
                    None
                };
                cleanup_empty_state(&mut table, key);
                (None, wake)
            } else if !wait {
                return Err(Errno::EAGAIN);
            } else {
                PENDING_LOCKS.lock().insert(owner, PendingLock { key, req });
                let entry = state.waiters.prepare_to_wait(&task, TaskState::Sleeping);
                (Some((Arc::clone(&state.waiters), entry)), None)
            }
        };
        if let Some(waiters) = wake {
            waiters.wake_all();
        }

        let Some((waiters, entry)) = wait_entry else {
            return Ok(());
        };

        let (retry_without_sleep, deadlock) = {
            let table = RECORD_LOCKS.lock();
            let retry = table
                .get(&key)
                .map(|state| state.first_conflict(owner, &req).is_none())
                .unwrap_or(true);
            let deadlock = !retry && would_deadlock(&table, owner, key, &req);
            (retry, deadlock)
        };
        if retry_without_sleep {
            waiters.finish_wait(&entry);
            PENDING_LOCKS.lock().remove(&owner);
            continue;
        }
        if deadlock {
            waiters.finish_wait(&entry);
            PENDING_LOCKS.lock().remove(&owner);
            return Err(Errno::EDEADLK);
        }
        if sched::operation::has_interrupting_signal(&task) {
            waiters.finish_wait(&entry);
            PENDING_LOCKS.lock().remove(&owner);
            return Err(Errno::EINTR);
        }

        sched::schedule_once(sched::now_ns_public());
        waiters.finish_wait(&entry);
        PENDING_LOCKS.lock().remove(&owner);
    }
}

/// 关闭 fd 时释放记录锁：POSIX 锁按 owner_pid 释放；OFD 锁仅在最后一个指向该
/// 打开文件描述的 fd 关闭时释放（用 `Arc` 强引用计数近似判断，见调用方注释）。
pub fn release_process_locks_for_file(file: &Arc<File>, owner_pid: i32) {
    let key = LockKey::from_file(file);
    let process_owner = RecordOwner::process(owner_pid);
    let ofd_owner = RecordOwner::ofd(Arc::as_ptr(file) as usize);
    // 关闭路径持有 `&Arc<File>`（RemovedFd 内部），强计数为 1 表示已无其余 fd
    // 引用该打开文件描述。若内核另有 Arc 引用（mmap/epoll 等）则保守地不释放
    // OFD 锁，避免与仍活跃的共享映射语义冲突。
    let last_reference = Arc::strong_count(file) <= 1;
    let waiters = {
        let mut table = RECORD_LOCKS.lock();
        let Some(state) = table.get_mut(&key) else {
            return;
        };
        let mut released = state.release_owner(process_owner);
        if last_reference {
            released |= state.release_owner(ofd_owner);
        }
        if !released {
            return;
        }
        let wake = if state.waiters.len_hint() != 0 {
            Some(Arc::clone(&state.waiters))
        } else {
            None
        };
        cleanup_empty_state(&mut table, key);
        wake
    };
    if let Some(waiters) = waiters {
        waiters.wake_all();
    }
}

fn cleanup_empty_state(table: &mut BTreeMap<LockKey, RecordLockState>, key: LockKey) {
    if table
        .get(&key)
        .is_some_and(|state| state.locks.is_empty() && state.waiters.len_hint() == 0)
    {
        table.remove(&key);
    }
}

fn would_deadlock(
    table: &BTreeMap<LockKey, RecordLockState>,
    waiter: RecordOwner,
    key: LockKey,
    req: &RecordLockRequest,
) -> bool {
    let pending = PENDING_LOCKS.lock();
    let Some(first_owner) = table
        .get(&key)
        .and_then(|state| state.first_conflict_owner(waiter, req))
    else {
        return false;
    };

    let mut stack = Vec::new();
    stack.push(first_owner);
    let mut seen = BTreeSet::new();
    while let Some(owner) = stack.pop() {
        if owner == waiter {
            return true;
        }
        if !seen.insert(owner) {
            continue;
        }
        let Some(waiting) = pending.get(&owner) else {
            continue;
        };
        let Some(next_owner) = table
            .get(&waiting.key)
            .and_then(|state| state.first_conflict_owner(owner, &waiting.req))
        else {
            continue;
        };
        stack.push(next_owner);
    }
    false
}

fn ranges_overlap(a_start: u64, a_end: Option<u64>, b_start: u64, b_end: Option<u64>) -> bool {
    match (a_end, b_end) {
        (Some(ae), Some(be)) => a_start < be && b_start < ae,
        (Some(ae), None) => b_start < ae,
        (None, Some(be)) => a_start < be,
        (None, None) => true,
    }
}

fn left_piece(lock: RecordLock, req: &RecordLockRequest) -> Option<RecordLock> {
    if lock.start >= req.start {
        return None;
    }
    Some(RecordLock {
        start: lock.start,
        end: Some(req.start),
        lock_type: lock.lock_type,
        owner: lock.owner,
    })
}

fn right_piece(lock: RecordLock, req: &RecordLockRequest) -> Option<RecordLock> {
    let right_start = req.end?;
    if end_le(lock.end, Some(right_start)) {
        return None;
    }
    Some(RecordLock {
        start: right_start,
        end: lock.end,
        lock_type: lock.lock_type,
        owner: lock.owner,
    })
}

fn normalize(mut locks: Vec<RecordLock>) -> Vec<RecordLock> {
    locks.retain(|lock| lock.end.is_none_or(|end| lock.start < end));
    locks.sort_by_key(|lock| (lock.owner, lock.lock_type_order(), lock.start));

    let mut out: Vec<RecordLock> = Vec::with_capacity(locks.len());
    for lock in locks {
        if let Some(last) = out.last_mut()
            && last.owner == lock.owner
            && last.lock_type == lock.lock_type
            && touches_or_overlaps(last.end, lock.start)
        {
            last.end = max_end(last.end, lock.end);
            continue;
        }
        out.push(lock);
    }
    out.sort_by_key(|lock| lock.start);
    out
}

impl RecordLock {
    fn lock_type_order(self) -> u8 {
        match self.lock_type {
            RecordLockType::Read => 0,
            RecordLockType::Write => 1,
            RecordLockType::Unlock => 2,
        }
    }
}

fn touches_or_overlaps(end: Option<u64>, next_start: u64) -> bool {
    end.is_none_or(|end| next_start <= end)
}

fn max_end(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, _) | (_, None) => None,
        (Some(a), Some(b)) => Some(max(a, b)),
    }
}

fn end_le(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(a), Some(b)) => a <= b,
    }
}

/// 把内部半开区间转换成 Linux `flock.l_len`。
pub fn len_from_range(start: u64, end: Option<u64>) -> u64 {
    end.map(|end| end.saturating_sub(start)).unwrap_or(0)
}

/// 根据 `whence/start/len` 计算绝对半开区间。
pub fn request_from_parts(
    file: &File,
    lock_type: RecordLockType,
    whence: i16,
    start: i64,
    len: i64,
) -> Result<RecordLockRequest, Errno> {
    let base = match whence {
        0 => 0i128,
        1 => i128::from(file.pos()),
        2 => i128::from(file.inode().size()),
        _ => return Err(Errno::EINVAL),
    };
    let raw_start = base.checked_add(i128::from(start)).ok_or(Errno::EINVAL)?;
    if raw_start < 0 {
        return Err(Errno::EINVAL);
    }

    let raw_end = if len == 0 {
        None
    } else {
        Some(
            raw_start
                .checked_add(i128::from(len))
                .ok_or(Errno::EINVAL)?,
        )
    };

    let (start, end) = match raw_end {
        Some(raw_end) if raw_end < 0 => return Err(Errno::EINVAL),
        Some(raw_end) if raw_end < raw_start => (raw_end as u64, Some(raw_start as u64)),
        Some(raw_end) => (raw_start as u64, Some(raw_end as u64)),
        None => (raw_start as u64, None),
    };

    Ok(RecordLockRequest {
        lock_type,
        start,
        end: end.map(|end| max(end, start)),
    })
}

/// 把锁范围限制在请求范围内，用于 `F_GETLK` 返回用户态期望的冲突片段。
pub fn clipped_conflict(
    conflict: RecordLockConflict,
    req: &RecordLockRequest,
) -> RecordLockConflict {
    let start = max(conflict.start, req.start);
    let end = match (conflict.end, req.end) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(min(a, b)),
    };
    RecordLockConflict {
        start,
        end,
        ..conflict
    }
}

#[allow(dead_code)]
fn _range_for_debug(start: u64, end: Option<u64>) -> Range<u64> {
    start..end.unwrap_or(u64::MAX)
}
