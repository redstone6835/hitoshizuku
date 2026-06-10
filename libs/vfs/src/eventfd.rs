//! eventfd-backed anonymous file.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::vfs::anon;
use crate::vfs::cred::Credentials;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{AccessMode, DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::sync::Spinlock;

const EVENTFD_COUNTER_MAX: u64 = u64::MAX - 1;

struct EventfdState {
    counter: u64,
}

pub struct EventfdFileOps {
    state: Spinlock<EventfdState>,
    semaphore: bool,
    read_wait: WaitQueue,
    write_wait: WaitQueue,
}

impl EventfdFileOps {
    pub fn new(initval: u64, semaphore: bool) -> Self {
        Self {
            state: Spinlock::new(EventfdState { counter: initval }),
            semaphore,
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
        }
    }
}

impl FileOps for EventfdFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::InvalidArgument);
        }
        let value = {
            let mut state = self.state.lock();
            if state.counter == 0 {
                return Err(VfsError::WouldBlock);
            }
            if self.semaphore {
                state.counter -= 1;
                1
            } else {
                let value = state.counter;
                state.counter = 0;
                value
            }
        };
        buf[..8].copy_from_slice(&value.to_ne_bytes());
        self.write_wait.wake_all();
        Ok(8)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::InvalidArgument);
        }
        let value = u64::from_ne_bytes(buf[..8].try_into().unwrap());
        if value == u64::MAX {
            return Err(VfsError::InvalidArgument);
        }
        {
            let mut state = self.state.lock();
            if state.counter > EVENTFD_COUNTER_MAX.saturating_sub(value) {
                return Err(VfsError::WouldBlock);
            }
            state.counter += value;
        }
        self.read_wait.wake_all();
        Ok(8)
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
        let state = self.state.lock();
        let mut ready = PollEvents::default();
        if state.counter > 0 {
            ready = ready.with(PollEvents::POLLIN);
        }
        if state.counter < EVENTFD_COUNTER_MAX {
            ready = ready.with(PollEvents::POLLOUT);
        }
        ready.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) {
            self.read_wait.enqueue(task);
        }
        if interest.has(PollEvents::POLLOUT) {
            self.write_wait.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.read_wait.remove(task);
        self.write_wait.remove(task);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.read_wait.wake_all();
        self.write_wait.wake_all();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn create(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    initval: u64,
    semaphore: bool,
    nonblock: bool,
    cloexec: bool,
) -> Result<Fd, Errno> {
    let file_flags = OpenOptions {
        access: AccessMode::ReadWrite,
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
        Box::new(EventfdFileOps::new(initval, semaphore)),
    )
    .map_err(|err| err.to_errno())
}
