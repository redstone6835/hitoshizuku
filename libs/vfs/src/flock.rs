//! BSD `flock(2)` 风格的 advisory lock 管理器。
//!
//! 这里实现的是 VFS 级的“协作锁”：只有同样调用 `flock` 的进程会被这套
//! 状态约束，普通 read/write/open 不会被强制拦截。锁的 owner 采用打开文件
//! 描述（`Arc<File>` 指针）而不是 fd 号，这样 `dup` 出来的多个 fd 共享同一把
//! 锁，符合用户态对 `flock` 的常见预期。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

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
    fn from_file(file: &Arc<File>) -> Self {
        let inode = file.inode();
        Self {
            fs_id: inode.fs_id().raw(),
            ino: inode.ino(),
        }
    }

    fn from_file_ref(file: &File) -> Self {
        let inode = file.inode();
        Self {
            fs_id: inode.fs_id().raw(),
            ino: inode.ino(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockKind {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug)]
struct LockOwner {
    ptr: usize,
    kind: LockKind,
}

struct LockState {
    owners: Vec<LockOwner>,
    waiters: Arc<WaitQueue>,
}

impl LockState {
    fn new() -> Self {
        Self {
            owners: Vec::new(),
            waiters: Arc::new(WaitQueue::new()),
        }
    }

    fn compatible(&self, owner: usize, wanted: LockKind) -> bool {
        self.owners.iter().all(|entry| {
            entry.ptr == owner
                || matches!((entry.kind, wanted), (LockKind::Shared, LockKind::Shared))
        })
    }

    fn set_owner(&mut self, owner: usize, wanted: LockKind) {
        if let Some(entry) = self.owners.iter_mut().find(|entry| entry.ptr == owner) {
            entry.kind = wanted;
            return;
        }
        self.owners.push(LockOwner {
            ptr: owner,
            kind: wanted,
        });
    }

    fn remove_owner(&mut self, owner: usize) -> bool {
        let before = self.owners.len();
        self.owners.retain(|entry| entry.ptr != owner);
        before != self.owners.len()
    }
}

static FLOCKS: Spinlock<BTreeMap<LockKey, LockState>> = Spinlock::new(BTreeMap::new());

/// 申请或释放一个 advisory flock。
///
/// `exclusive=false` 表示共享锁，`nonblock=true` 时遇到冲突立即返回 `EAGAIN`。
/// 阻塞路径遵循 WaitQueue 的 prepare/recheck/sleep 协议，避免“检查后刚好释放”
/// 造成的丢唤醒。
pub fn flock(file: &Arc<File>, exclusive: bool, nonblock: bool) -> Result<(), Errno> {
    let key = LockKey::from_file(file);
    let owner = Arc::as_ptr(file) as usize;
    let wanted = if exclusive {
        LockKind::Exclusive
    } else {
        LockKind::Shared
    };
    let task = sched::current_task();

    loop {
        let should_sleep = {
            let mut table = FLOCKS.lock();
            let state = table.entry(key).or_insert_with(LockState::new);
            if state.compatible(owner, wanted) {
                state.set_owner(owner, wanted);
                false
            } else if nonblock {
                return Err(Errno::EAGAIN);
            } else {
                state.waiters.prepare_to_wait(&task, TaskState::Sleeping);
                true
            }
        };

        if !should_sleep {
            return Ok(());
        }

        let retry_without_sleep = {
            let table = FLOCKS.lock();
            table
                .get(&key)
                .map(|state| state.compatible(owner, wanted))
                .unwrap_or(true)
        };
        if retry_without_sleep {
            finish_wait_for(key, &task);
            continue;
        }
        if sched::operation::has_interrupting_signal(&task) {
            finish_wait_for(key, &task);
            return Err(Errno::EINTR);
        }

        sched::schedule_once(sched::now_ns_public());
        finish_wait_for(key, &task);
    }
}

fn finish_wait_for(key: LockKey, task: &Arc<sched::Task>) {
    let waiters = FLOCKS
        .lock()
        .get(&key)
        .map(|state| Arc::clone(&state.waiters));
    if let Some(waiters) = waiters {
        waiters.finish_wait(task);
    }
}

/// 释放指定打开文件描述拥有的 flock。
pub fn unlock(file: &Arc<File>) {
    let key = LockKey::from_file(file);
    let owner = Arc::as_ptr(file) as usize;
    unlock_key_owner(key, owner);
}

/// `File::drop` 路径没有 `Arc<File>` 句柄，只能使用对象自身地址作为 owner。
pub(crate) fn unlock_file_ref(file: &File) {
    let key = LockKey::from_file_ref(file);
    let owner = file as *const File as usize;
    unlock_key_owner(key, owner);
}

fn unlock_key_owner(key: LockKey, owner: usize) {
    let waiters = {
        let mut table = FLOCKS.lock();
        let Some(state) = table.get_mut(&key) else {
            return;
        };
        if !state.remove_owner(owner) {
            return;
        }
        let should_remove = state.owners.is_empty() && state.waiters.len_hint() == 0;
        let wake = if state.waiters.len_hint() != 0 {
            Some(Arc::clone(&state.waiters))
        } else {
            None
        };
        if should_remove {
            table.remove(&key);
        }
        wake
    };
    if let Some(waiters) = waiters {
        waiters.wake_all();
    }
}
