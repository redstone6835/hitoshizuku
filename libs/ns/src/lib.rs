//! 命名空间框架。
//!
//! 提供 Linux 命名空间的内核侧对象模型：
//!
//! - [`Namespace`] trait：所有命名空间类型的统一接口（类型、不透明 inode
//!   号），供 nsfs 文件系统持有与 `setns(2)` 使用；
//! - [`UtsNamespace`]：hostname/domainname（`sethostname`/`setdomainname`）；
//! - [`TimeNamespace`]：realtime/monotonic/boottime 时钟偏移
//!   （`CLONE_NEWTIME`）；
//! - [`CgroupNamespace`]：cgroup 层级根视图（本内核无 cgroupfs，恒为根）；
//! - PID 命名空间复用 `sched::pid::PidNamespace`，并在本 crate 实现
//!   [`Namespace`]（trait 与类型分属两个 crate，不违反孤儿规则）；
//! - Mount 命名空间由 `vfs::MountNamespace` 实现 [`Namespace`]（见 vfs 侧
//!   `impl ns::Namespace for MountNamespace`）。

#![no_std]

extern crate alloc;

use alloc::sync::Arc;

pub mod cgroup;
pub mod time;
pub mod uts;

use core::sync::atomic::{AtomicU64, Ordering};

pub use cgroup::CgroupNamespace;
pub use sched::pid::PidNamespace;
pub use time::TimeNamespace;
pub use uts::UtsNamespace;

/// 命名空间类型（Linux `CLONE_NEW*` 位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NsType {
    Mount = 0x0002_0000,
    Uts = 0x0400_0000,
    Ipc = 0x0800_0000,
    User = 0x1000_0000,
    Pid = 0x2000_0000,
    Net = 0x4000_0000,
    Time = 0x0080_0000,
    Cgroup = 0x0200_0000,
}

impl NsType {
    /// nsfs 文件名（`/proc/self/ns/<name>`）。
    pub const fn proc_name(self) -> &'static str {
        match self {
            Self::Mount => "mnt",
            Self::Uts => "uts",
            Self::Ipc => "ipc",
            Self::User => "user",
            Self::Pid => "pid",
            Self::Net => "net",
            Self::Time => "time",
            Self::Cgroup => "cgroup",
        }
    }

    /// `CLONE_NEW*` 位 → [`NsType`]。
    pub const fn from_clone_flag(flag: u64) -> Option<Self> {
        match flag {
            0x0002_0000 => Some(Self::Mount),
            0x0400_0000 => Some(Self::Uts),
            0x0800_0000 => Some(Self::Ipc),
            0x1000_0000 => Some(Self::User),
            0x2000_0000 => Some(Self::Pid),
            0x4000_0000 => Some(Self::Net),
            0x0080_0000 => Some(Self::Time),
            0x0200_0000 => Some(Self::Cgroup),
            _ => None,
        }
    }
}

/// 命名空间对象的统一接口。
pub trait Namespace: core::any::Any + Send + Sync {
    fn ns_type(&self) -> NsType;
    /// 该命名空间实例的唯一不透明 inode 号（nsfs 显示为 `ns:[inum]`）。
    fn inum(&self) -> u64;
}

/// `Arc<dyn Namespace>` 的向下转型（经 `dyn Any` upcast coercion）。
pub fn downcast_arc<T: 'static + Send + Sync>(namespace: Arc<dyn Namespace>) -> Option<Arc<T>> {
    let any: Arc<dyn core::any::Any + Send + Sync> = namespace;
    any.downcast::<T>().ok()
}

static NEXT_NS_INUM: AtomicU64 = AtomicU64::new(0x4000_0000);

/// 分配一个全局唯一的不透明 inode 号。
pub fn allocate_ns_inum() -> u64 {
    NEXT_NS_INUM.fetch_add(1, Ordering::Relaxed)
}

impl Namespace for PidNamespace {
    fn ns_type(&self) -> NsType {
        NsType::Pid
    }

    fn inum(&self) -> u64 {
        // 用 pid 分配器的地址身份做稳定 inum（sched 未提供 inum 字段）。
        self as *const Self as usize as u64
    }
}
