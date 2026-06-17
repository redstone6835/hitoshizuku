//! Linux file lease 的最小 VFS 状态管理。
//!
//! 目前只覆盖 `fcntl(F_SETLEASE/F_GETLEASE)` 的状态语义：同一 inode 同时只
//! 允许一个进程持有 lease，进程关闭对应文件或退出时释放。内核尚未实现
//! lease break、SIGIO 通知以及 open/truncate 时的强制冲突处理，因此这里不把
//! lease 接入文件访问路径，只作为兼容 LTP 基础用例的 advisory 状态。

use alloc::collections::BTreeMap;

use errno::Errno;

use crate::vfs::file::File;
use crate::vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LeaseKey {
    fs_id: u64,
    ino: u64,
}

impl LeaseKey {
    fn from_file(file: &File) -> Self {
        let inode = file.inode();
        Self {
            fs_id: inode.fs_id().raw(),
            ino: inode.ino(),
        }
    }
}

/// Linux lease 类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseType {
    Read,
    Write,
    Unlock,
}

#[derive(Clone, Copy, Debug)]
struct Lease {
    owner_pid: i32,
    lease_type: LeaseType,
}

struct LeaseState {
    lease: Lease,
}

static LEASES: Spinlock<BTreeMap<LeaseKey, LeaseState>> = Spinlock::new(BTreeMap::new());

/// 设置或释放当前进程在 inode 上的 lease。
pub fn setlease(file: &File, owner_pid: i32, lease_type: LeaseType) -> Result<(), Errno> {
    let key = LeaseKey::from_file(file);
    let mut leases = LEASES.lock();
    match lease_type {
        LeaseType::Unlock => {
            if leases
                .get(&key)
                .is_some_and(|state| state.lease.owner_pid == owner_pid)
            {
                leases.remove(&key);
            }
        }
        LeaseType::Read | LeaseType::Write => match leases.get_mut(&key) {
            Some(state) if state.lease.owner_pid == owner_pid => {
                state.lease.lease_type = lease_type;
            }
            Some(_) => return Err(Errno::EAGAIN),
            None => {
                leases.insert(
                    key,
                    LeaseState {
                        lease: Lease {
                            owner_pid,
                            lease_type,
                        },
                    },
                );
            }
        },
    }
    Ok(())
}

/// 查询当前进程在 inode 上持有的 lease。
pub fn getlease(file: &File, owner_pid: i32) -> LeaseType {
    let key = LeaseKey::from_file(file);
    LEASES
        .lock()
        .get(&key)
        .filter(|state| state.lease.owner_pid == owner_pid)
        .map(|state| state.lease.lease_type)
        .unwrap_or(LeaseType::Unlock)
}

/// 关闭指向 inode 的 fd 时释放当前进程持有的 lease。
pub fn release_process_lease_for_file(file: &File, owner_pid: i32) {
    let key = LeaseKey::from_file(file);
    let mut leases = LEASES.lock();
    if leases
        .get(&key)
        .is_some_and(|state| state.lease.owner_pid == owner_pid)
    {
        leases.remove(&key);
    }
}
