//! memfd-backed anonymous regular files.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::vfs::anon;
use crate::vfs::cred::Credentials;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{AccessMode, DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::{Inode, InodeOps};
use crate::vfs::stat::{FileMode, FileType};
use crate::vfs::sync::Spinlock;

pub const F_SEAL_SEAL: u32 = 0x0001;
pub const F_SEAL_SHRINK: u32 = 0x0002;
pub const F_SEAL_GROW: u32 = 0x0004;
pub const F_SEAL_WRITE: u32 = 0x0008;
pub const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
pub const F_SEAL_ALL: u32 =
    F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

struct MemfdInner {
    data: Vec<u8>,
    seals: u32,
    inode: Option<Weak<Inode>>,
}

struct MemfdState {
    inner: Spinlock<MemfdInner>,
    allow_sealing: bool,
    waiters: WaitQueue,
}

impl MemfdState {
    fn new(allow_sealing: bool) -> Self {
        let seals = if allow_sealing { 0 } else { F_SEAL_SEAL };
        Self {
            inner: Spinlock::new(MemfdInner {
                data: Vec::new(),
                seals,
                inode: None,
            }),
            allow_sealing,
            waiters: WaitQueue::new(),
        }
    }

    fn bind_inode(&self, inode: &Arc<Inode>) {
        self.inner.lock().inode = Some(Arc::downgrade(inode));
    }

    fn update_inode_size(inner: &MemfdInner) {
        if let Some(inode) = inner.inode.as_ref().and_then(Weak::upgrade) {
            let size = inner.data.len() as u64;
            inode.set_size_and_blocks(size, size.div_ceil(512));
        }
    }

    fn add_seals(&self, seals: u32) -> Result<(), Errno> {
        if !self.allow_sealing || (seals & !F_SEAL_ALL) != 0 {
            return Err(Errno::EINVAL);
        }
        let mut inner = self.inner.lock();
        if (inner.seals & F_SEAL_SEAL) != 0 {
            return Err(Errno::EPERM);
        }
        inner.seals |= seals;
        Ok(())
    }

    fn seals(&self) -> u32 {
        self.inner.lock().seals
    }

    fn truncate(&self, size: u64) -> VfsResult<()> {
        let size = usize::try_from(size).map_err(|_| VfsError::FileTooLarge)?;
        let mut inner = self.inner.lock();
        let old_len = inner.data.len();
        if size < old_len && (inner.seals & F_SEAL_SHRINK) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        if size > old_len && (inner.seals & F_SEAL_GROW) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        inner.data.resize(size, 0);
        Self::update_inode_size(&inner);
        drop(inner);
        self.waiters.wake_all();
        Ok(())
    }
}

struct MemfdInodeOps {
    state: Arc<MemfdState>,
}

impl InodeOps for MemfdInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }

    fn truncate(&self, _inode: &Inode, size: u64) -> VfsResult<()> {
        self.state.truncate(size)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MemfdFileOps {
    state: Arc<MemfdState>,
}

impl MemfdFileOps {
    pub fn add_seals(&self, seals: u32) -> Result<(), Errno> {
        self.state.add_seals(seals)
    }

    pub fn seals(&self) -> u32 {
        self.state.seals()
    }
}

impl FileOps for MemfdFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let offset = usize::try_from(offset).map_err(|_| VfsError::InvalidArgument)?;
        let inner = self.state.inner.lock();
        if offset >= inner.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(inner.data.len() - offset);
        buf[..n].copy_from_slice(&inner.data[offset..offset + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let mut inner = self.state.inner.lock();
        if (inner.seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE)) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        let offset = if offset == u64::MAX {
            inner.data.len()
        } else {
            usize::try_from(offset).map_err(|_| VfsError::FileTooLarge)?
        };
        let end = offset
            .checked_add(buf.len())
            .ok_or(VfsError::FileTooLarge)?;
        if end > inner.data.len() && (inner.seals & F_SEAL_GROW) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        if end > inner.data.len() {
            inner.data.resize(end, 0);
        }
        inner.data[offset..end].copy_from_slice(buf);
        Self::state_update_inode_size(&inner);
        drop(inner);
        self.state.waiters.wake_all();
        Ok(buf.len())
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
        let mut ready = PollEvents::POLLIN.with(PollEvents::POLLOUT);
        if (self.state.inner.lock().seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE)) != 0 {
            ready = ready.without(PollEvents::POLLOUT);
        }
        ready.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, _interest: PollEvents) -> bool {
        self.state.waiters.enqueue(task);
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.state.waiters.remove(task);
    }

    fn fallocate(&self, offset: u64, len: u64) -> VfsResult<()> {
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        self.state.truncate(end)
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.state.waiters.wake_all();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl MemfdFileOps {
    fn state_update_inode_size(inner: &MemfdInner) {
        MemfdState::update_inode_size(inner);
    }
}

pub fn create(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    allow_sealing: bool,
    cloexec: bool,
) -> Result<Fd, Errno> {
    let state = Arc::new(MemfdState::new(allow_sealing));
    let inode_ops = Arc::new(MemfdInodeOps {
        state: Arc::clone(&state),
    });
    let file_ops = Box::new(MemfdFileOps {
        state: Arc::clone(&state),
    });
    let file_flags = OpenOptions {
        access: AccessMode::ReadWrite,
        ..Default::default()
    };
    let file = anon::new_private_file(
        cred,
        file_flags,
        FileType::Regular,
        FileMode::new(0o600),
        0,
        inode_ops,
        file_ops,
    );
    state.bind_inode(file.inode());
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    fdt.alloc_fd(file, fd_flags).map_err(|e| e.to_errno())
}
