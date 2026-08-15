//! nsfs：命名空间文件（`/proc/self/ns/*` 与 `setns(2)` 的 fd 载体）。
//!
//! 文件打开时绑定一个具体的命名空间对象；`setns(fd)` 从文件操作层取回该
//! 对象。ioctl 支持 Linux `NS_GET_*` 系列（`NS_GET_NSTYPE` 等）。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;

use errno::Errno;
use ns::{Namespace, NsType};
use sched::Task;
use vfs::file::{FileOps, IoctlCmd, PollEvents};
use vfs::poll_source::PollSource;
use vfs::stat::FileType;

/// `setns(2)` 的 fd 校验：文件必须来自 nsfs。
pub const NSFS_IOCTL_BASE: u64 = 0xb7;

/// `NS_GET_NSTYPE`：返回命名空间类型（`CLONE_NEW*` 位）。
pub const NS_GET_NSTYPE: u64 = 0xb701;
/// `NS_GET_OWNER_UID`：返回属主 uid（本内核无用户命名空间 → 0）。
pub const NS_GET_OWNER_UID: u64 = 0xb702;
/// `NS_GET_PARENT`：返回父命名空间（无 → `ENOTTY`）。
pub const NS_GET_PARENT: u64 = 0xb703;
/// `NS_GET_USERNS`：返回用户命名空间（无 → `ENOTTY`）。
pub const NS_GET_USERNS: u64 = 0xb704;
/// `NS_GET_PID_IN_PIDNS` / `NS_GET_TGID_IN_PIDNS`。
pub const NS_GET_PID_IN_PIDNS: u64 = 0xb706;
pub const NS_GET_TGID_IN_PIDNS: u64 = 0xb707;

/// nsfs 文件的打开描述：绑定一个命名空间。
pub struct NsfsFileOps {
    namespace: Arc<dyn Namespace>,
    poll_source: PollSource,
}

impl NsfsFileOps {
    pub fn new(namespace: Arc<dyn Namespace>) -> Self {
        Self {
            namespace,
            poll_source: PollSource::new(vfs::file::PollEvents(0)),
        }
    }

    pub fn namespace(&self) -> &Arc<dyn Namespace> {
        &self.namespace
    }

    /// 供 procfs 构造 ns 文件时使用的类型判定。
    pub fn ns_type(&self) -> NsType {
        self.namespace.ns_type()
    }
}

impl FileOps for NsfsFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> vfs::error::VfsResult<usize> {
        Err(vfs::error::VfsError::InvalidArgument)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> vfs::error::VfsResult<usize> {
        Err(vfs::error::VfsError::InvalidArgument)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(vfs::file::DirEntry) -> core::ops::ControlFlow<()>,
    ) -> vfs::error::VfsResult<u64> {
        Err(vfs::error::VfsError::NotADirectory)
    }

    fn sync(&self) -> vfs::error::VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
    }

    fn is_epollable(&self) -> bool {
        false
    }

    fn release(&self) {}

    fn ioctl(&self, cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        let raw = cmd.raw() as u64;
        match raw {
            NS_GET_NSTYPE => Ok(self.namespace.ns_type() as usize),
            NS_GET_OWNER_UID => Ok(0),
            NS_GET_PARENT | NS_GET_USERNS => Err(Errno::ENOTTY),
            NS_GET_PID_IN_PIDNS | NS_GET_TGID_IN_PIDNS => {
                // 需要目标 pid 命名空间参数（见 ioctl 调用方）；当前参数为
                // 目标 ns 的 fd。简化：按 fd 解析由 syscall 层完成，这里
                // 只对 pid 类型命名空间生效。
                if self.namespace.ns_type() != NsType::Pid {
                    return Err(Errno::EINVAL);
                }
                Err(Errno::ENOTTY)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 命名空间文件类型（供 procfs 使用）。
pub const NS_FILE_TYPE: FileType = FileType::Regular;

/// 进程命名空间类型（procfs `/proc/<pid>/ns/<name>`）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProcNsKind {
    Uts,
    Ipc,
    Time,
    Cgroup,
    Pid,
    Mount,
    User,
    Net,
}

impl ProcNsKind {
    pub const ALL: [ProcNsKind; 8] = [
        ProcNsKind::Uts,
        ProcNsKind::Ipc,
        ProcNsKind::Time,
        ProcNsKind::Cgroup,
        ProcNsKind::Pid,
        ProcNsKind::Mount,
        ProcNsKind::User,
        ProcNsKind::Net,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Uts => "uts",
            Self::Ipc => "ipc",
            Self::Time => "time",
            Self::Cgroup => "cgroup",
            Self::Pid => "pid",
            Self::Mount => "mnt",
            Self::User => "user",
            Self::Net => "net",
        }
    }
}

/// 命名空间提供者（kernel 注册；procfs 的 ns 文件打开时调用）。
pub type NsProvider = fn(pid: i32, kind: ProcNsKind) -> Option<Arc<dyn Namespace>>;

static NS_PROVIDER: vfs::sync::Spinlock<Option<NsProvider>> = vfs::sync::Spinlock::new(None);

pub fn register_ns_provider(provider: NsProvider) {
    *NS_PROVIDER.lock() = Some(provider);
}

pub fn ns_provider() -> Option<NsProvider> {
    *NS_PROVIDER.lock()
}

/// ns 文件内容格式：`ns:[inum]`。
pub fn ns_file_content(namespace: &dyn Namespace) -> alloc::string::String {
    alloc::format!("ns:[{}]", namespace.inum())
}
