//! timerfd-backed anonymous files.

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerSpec {
    pub interval_ns: u64,
    pub value_ns: u64,
}

struct TimerfdState {
    next_expiry_ns: Option<u64>,
    interval_ns: u64,
    expirations: u64,
}

pub struct TimerfdFileOps {
    state: Spinlock<TimerfdState>,
    waiters: WaitQueue,
    clock_id: usize,
}

impl TimerfdFileOps {
    pub fn new(clock_id: usize) -> Self {
        Self {
            state: Spinlock::new(TimerfdState {
                next_expiry_ns: None,
                interval_ns: 0,
                expirations: 0,
            }),
            waiters: WaitQueue::new(),
            clock_id,
        }
    }

    pub fn clock_id(&self) -> usize {
        self.clock_id
    }

    fn refresh_locked(state: &mut TimerfdState, now_ns: u64) {
        let Some(next) = state.next_expiry_ns else {
            return;
        };
        if now_ns < next {
            return;
        }
        let count = if state.interval_ns == 0 {
            state.next_expiry_ns = None;
            1
        } else {
            let elapsed = now_ns.saturating_sub(next);
            let count = elapsed / state.interval_ns + 1;
            state.next_expiry_ns =
                Some(next.saturating_add(count.saturating_mul(state.interval_ns)));
            count
        };
        state.expirations = state.expirations.saturating_add(count);
    }

    pub fn set_time(&self, now_ns: u64, new_value: TimerSpec) -> TimerSpec {
        let mut state = self.state.lock();
        Self::refresh_locked(&mut state, now_ns);
        let old = Self::remaining_locked(&state, now_ns);
        state.interval_ns = new_value.interval_ns;
        state.expirations = 0;
        state.next_expiry_ns = if new_value.value_ns == 0 {
            None
        } else {
            Some(now_ns.saturating_add(new_value.value_ns))
        };
        drop(state);
        self.waiters.wake_all();
        old
    }

    pub fn set_deadline(
        &self,
        now_ns: u64,
        deadline_ns: Option<u64>,
        interval_ns: u64,
    ) -> TimerSpec {
        let mut state = self.state.lock();
        Self::refresh_locked(&mut state, now_ns);
        let old = Self::remaining_locked(&state, now_ns);
        state.interval_ns = interval_ns;
        state.expirations = 0;
        state.next_expiry_ns = deadline_ns;
        drop(state);
        self.waiters.wake_all();
        old
    }

    pub fn get_time(&self, now_ns: u64) -> TimerSpec {
        let mut state = self.state.lock();
        Self::refresh_locked(&mut state, now_ns);
        Self::remaining_locked(&state, now_ns)
    }

    fn remaining_locked(state: &TimerfdState, now_ns: u64) -> TimerSpec {
        let value_ns = state
            .next_expiry_ns
            .map(|next| next.saturating_sub(now_ns))
            .unwrap_or(0);
        TimerSpec {
            interval_ns: state.interval_ns,
            value_ns,
        }
    }
}

impl FileOps for TimerfdFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::InvalidArgument);
        }
        let value = {
            let mut state = self.state.lock();
            Self::refresh_locked(&mut state, sched::now_ns_public());
            if state.expirations == 0 {
                return Err(VfsError::WouldBlock);
            }
            let value = state.expirations;
            state.expirations = 0;
            value
        };
        buf[..8].copy_from_slice(&value.to_ne_bytes());
        Ok(8)
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
        let mut state = self.state.lock();
        Self::refresh_locked(&mut state, sched::now_ns_public());
        let ready = if state.expirations > 0 {
            PollEvents::POLLIN
        } else {
            PollEvents::default()
        };
        ready.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) {
            self.waiters.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.waiters.remove(task);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.waiters.wake_all();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn create(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    clock_id: usize,
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
        Box::new(TimerfdFileOps::new(clock_id)),
    )
    .map_err(|err| err.to_errno())
}
