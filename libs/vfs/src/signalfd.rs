//! signalfd-backed anonymous files.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{SigInfo, SigSet, SignalObserver, Task, WaitQueue};

use crate::poll_source::PollSource;
use crate::vfs::anon;
use crate::vfs::cred::Credentials;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{AccessMode, DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::sync::Spinlock;

pub struct SignalfdFileOps {
    shared: Arc<SignalfdShared>,
}

struct SignalfdShared {
    mask: Spinlock<SigSet>,
    waiters: WaitQueue,
    poll_source: PollSource,
    owner: Weak<Task>,
}

impl SignalfdFileOps {
    pub fn new(mask: SigSet) -> Self {
        let owner = if sched::is_ready() {
            Some(sched::current_task())
        } else {
            None
        };
        let shared = Arc::new(SignalfdShared {
            mask: Spinlock::new(mask.sanitized()),
            waiters: WaitQueue::new(),
            poll_source: PollSource::new(PollEvents::default()),
            owner: owner.as_ref().map_or_else(Weak::new, Arc::downgrade),
        });
        if let Some(owner) = owner {
            let observer: Arc<dyn SignalObserver> = shared.clone();
            let weak = Arc::downgrade(&observer);
            owner.signal.subscribe(weak.clone());
            owner.shared_signal().subscribe(weak);
        }
        shared.publish_readiness();
        Self { shared }
    }

    pub fn set_mask(&self, mask: SigSet) {
        *self.shared.mask.lock() = mask.sanitized();
        self.refresh_readiness();
        self.shared.waiters.wake_all();
    }

    fn refresh_readiness(&self) {
        self.shared.publish_readiness();
    }
}

impl SignalfdShared {
    fn pending(&self) -> bool {
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        let mask = self.mask.lock().raw();
        owner.signal.has_pending_in(mask) || owner.shared_signal().has_pending_in(mask)
    }

    fn publish_readiness(&self) {
        let version = self.poll_source.reserve_version();
        let readiness = if self.pending() {
            PollEvents::POLLIN
        } else {
            PollEvents::default()
        };
        self.poll_source.publish_versioned(readiness, version);
    }
}

impl SignalObserver for SignalfdShared {
    fn signal_state_changed(&self) {
        self.publish_readiness();
        if self.pending() {
            self.waiters.wake_all();
        }
    }
}

impl FileOps for SignalfdFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < SIGNALFD_SIGINFO_SIZE {
            return Err(VfsError::InvalidArgument);
        }
        let mask = *self.shared.mask.lock();
        let mut written = 0usize;
        while written + SIGNALFD_SIGINFO_SIZE <= buf.len() {
            let Some(info) = sched::operation::sigtimedwait_poll(mask) else {
                break;
            };
            write_signalfd_siginfo(&mut buf[written..written + SIGNALFD_SIGINFO_SIZE], info);
            written += SIGNALFD_SIGINFO_SIZE;
        }
        if written == 0 {
            return Err(VfsError::WouldBlock);
        }
        self.refresh_readiness();
        Ok(written)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidArgument)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        self.refresh_readiness();
        self.shared.poll_source.snapshot().0.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) {
            self.shared.waiters.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.shared.waiters.remove(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.shared.poll_source)
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.shared.waiters.wake_all();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub const SIGNALFD_SIGINFO_SIZE: usize = 128;

fn put_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put_i32(buf: &mut [u8], off: usize, value: i32) {
    buf[off..off + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_signalfd_siginfo(buf: &mut [u8], info: SigInfo) {
    buf.fill(0);
    put_u32(buf, 0, info.sig.raw() as u32);
    put_i32(buf, 4, info.code);
    put_u32(buf, 12, info.sender_pid as u32);
    put_u32(buf, 16, info.sender_uid.0);
    if let Some(raw) = info.raw {
        // 用户态排队信号的完整 siginfo 已在调度层保留。signalfd 的 ABI 不是
        // siginfo_t 的逐字节别名，这里只提取通用头部字段，其余扩展字段保持零。
        let signo = i32::from_ne_bytes(raw[0..4].try_into().unwrap_or([0; 4]));
        if signo > 0 {
            put_u32(buf, 0, signo as u32);
        }
        put_i32(
            buf,
            4,
            i32::from_ne_bytes(raw[8..12].try_into().unwrap_or([0; 4])),
        );
    }
}

pub fn create(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    mask: SigSet,
    nonblock: bool,
    cloexec: bool,
) -> Result<Fd, Errno> {
    let file_flags = OpenOptions {
        access: AccessMode::ReadOnly,
        nonblock,
        ..Default::default()
    };
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    anon::create_fd(
        fdt,
        cred,
        file_flags,
        fd_flags,
        Box::new(SignalfdFileOps::new(mask)),
    )
    .map_err(|err| err.to_errno())
}
