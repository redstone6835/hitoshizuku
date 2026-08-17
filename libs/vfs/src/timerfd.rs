//! timerfd-backed anonymous files.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use errno::Errno;
use sched::{DeadlineObserver, Task, WaitQueue};

use crate::poll_source::PollSource;
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

/// `timerfd_settime` 标志（Linux UAPI 值）。
pub const TFD_TIMER_ABSTIME: usize = 1;
pub const TFD_TIMER_CANCEL_ON_SET: usize = 2;

/// CLOCK_REALTIME 的 clockid 值。
const CLOCK_REALTIME_ID: usize = 0;

/// 登记了 `TFD_TIMER_CANCEL_ON_SET` 的 timerfd（Linux `cancel_list` 语义）。
///
/// 弱引用持有：fd 关闭后条目在下次遍历时自然清理。
static CANCEL_REGISTRY: Spinlock<Vec<Weak<TimerfdShared>>> = Spinlock::new(Vec::new());

struct TimerfdState {
    next_expiry_ns: Option<u64>,
    interval_ns: u64,
    expirations: u64,
}

pub struct TimerfdFileOps {
    shared: Arc<TimerfdShared>,
    clock_id: usize,
}

struct TimerfdShared {
    state: Spinlock<TimerfdState>,
    waiters: WaitQueue,
    poll_source: PollSource,
    registration: AtomicU64,
    self_weak: Weak<TimerfdShared>,
    /// 已登记 CANCEL_ON_SET（在 CANCEL_REGISTRY 中）。
    might_cancel: AtomicBool,
    /// 实时钟被设置后置位；下一次 read/settime 消费并返回 ECANCELED。
    cancelled: AtomicBool,
}

impl TimerfdFileOps {
    pub fn new(clock_id: usize) -> Self {
        let shared = Arc::new_cyclic(|self_weak| TimerfdShared {
            state: Spinlock::new(TimerfdState {
                next_expiry_ns: None,
                interval_ns: 0,
                expirations: 0,
            }),
            waiters: WaitQueue::new(),
            poll_source: PollSource::new(PollEvents::default()),
            registration: AtomicU64::new(0),
            self_weak: self_weak.clone(),
            might_cancel: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        Self { shared, clock_id }
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

    pub fn set_time(&self, now_ns: u64, new_value: TimerSpec) -> VfsResult<TimerSpec> {
        let mut state = self.shared.state.lock();
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
        self.shared.refresh_and_arm(now_ns);
        self.shared.waiters.wake_all();
        // Linux：定时器照常 arm，但若此前已被时钟设置取消，本次调用返回
        // ECANCELED（一次性，随后取消状态复位）。
        if self.shared.take_cancelled() {
            return Err(VfsError::Canceled);
        }
        Ok(old)
    }

    pub fn set_deadline(
        &self,
        now_ns: u64,
        deadline_ns: Option<u64>,
        interval_ns: u64,
    ) -> VfsResult<TimerSpec> {
        let mut state = self.shared.state.lock();
        Self::refresh_locked(&mut state, now_ns);
        let old = Self::remaining_locked(&state, now_ns);
        state.interval_ns = interval_ns;
        state.expirations = 0;
        state.next_expiry_ns = deadline_ns;
        drop(state);
        self.shared.refresh_and_arm(now_ns);
        self.shared.waiters.wake_all();
        if self.shared.take_cancelled() {
            return Err(VfsError::Canceled);
        }
        Ok(old)
    }

    /// 按 Linux 语义登记/注销 CANCEL_ON_SET：仅当 clockid 为 CLOCK_REALTIME
    /// 且本次 settime 同时带 `TFD_TIMER_ABSTIME` 与 `TFD_TIMER_CANCEL_ON_SET`。
    pub fn update_cancel_registration(&self, flags: usize) {
        let want = self.clock_id == CLOCK_REALTIME_ID
            && (flags & TFD_TIMER_ABSTIME) != 0
            && (flags & TFD_TIMER_CANCEL_ON_SET) != 0;
        self.shared.might_cancel.store(want, Ordering::Release);
        let mut registry = CANCEL_REGISTRY.lock();
        registry.retain(|weak| {
            weak.upgrade()
                .map(|shared| !Arc::ptr_eq(&shared, &self.shared))
                .unwrap_or(false)
        });
        if want {
            registry.push(Arc::downgrade(&self.shared));
        }
    }

    pub fn get_time(&self, now_ns: u64) -> TimerSpec {
        let mut state = self.shared.state.lock();
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

impl TimerfdShared {
    /// 消费取消标记（一次性；Linux `timerfd_canceled` 复位 moffs 的等价物）。
    fn take_cancelled(&self) -> bool {
        self.cancelled.swap(false, Ordering::AcqRel)
    }

    fn publish_readiness(&self, now_ns: u64) -> Option<u64> {
        let (ready, next, version) = {
            let mut state = self.state.lock();
            TimerfdFileOps::refresh_locked(&mut state, now_ns);
            (
                if state.expirations != 0 {
                    PollEvents::POLLIN
                } else {
                    PollEvents::default()
                },
                state.next_expiry_ns,
                self.poll_source.reserve_version(),
            )
        };
        self.poll_source.publish_versioned(ready, version);
        next
    }

    fn refresh_and_arm(&self, now_ns: u64) {
        let next = self.publish_readiness(now_ns);
        self.arm(next, now_ns);
    }

    fn arm(&self, deadline: Option<u64>, now_ns: u64) {
        let old = self.registration.swap(0, Ordering::AcqRel);
        if old != 0 {
            sched::cancel_deadline_observer(old);
        }
        let Some(deadline) = deadline else {
            return;
        };
        if deadline <= now_ns {
            if let Some(next) = self.deadline_expired(0, now_ns) {
                self.arm(Some(next), now_ns);
            }
            return;
        }
        let Some(this) = self.self_weak.upgrade() else {
            return;
        };
        let observer: Arc<dyn DeadlineObserver> = this;
        let registration = sched::reserve_deadline_observer_id();
        self.registration.store(registration, Ordering::Release);
        if !sched::register_deadline_observer(registration, deadline, Arc::downgrade(&observer)) {
            if self
                .registration
                .compare_exchange(registration, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let now_ns = sched::now_ns_public();
                if let Some(next) = self.deadline_expired(0, now_ns) {
                    self.arm(Some(next), now_ns);
                }
            }
        }
    }
}

/// 实时钟被设置后调用：取消所有登记的 `TFD_TIMER_CANCEL_ON_SET` timerfd，
/// 并唤醒阻塞的读者（Linux `timerfd_clock_was_set`：ticks++ + wake，读者
/// 醒来后发现取消标记而返回 ECANCELED）。
pub fn cancel_timers_on_clock_set() {
    let now_ns = sched::now_ns_public();
    let mut affected = Vec::new();
    {
        let mut registry = CANCEL_REGISTRY.lock();
        let mut i = 0;
        while i < registry.len() {
            match registry[i].upgrade() {
                None => {
                    registry.swap_remove(i);
                }
                Some(shared) => {
                    if shared.might_cancel.load(Ordering::Acquire) {
                        shared.cancelled.store(true, Ordering::Release);
                        let mut state = shared.state.lock();
                        state.expirations = state.expirations.saturating_add(1);
                        drop(state);
                        affected.push(shared);
                    }
                    i += 1;
                }
            }
        }
    }
    for shared in affected {
        shared.publish_readiness(now_ns);
        shared.waiters.wake_all();
    }
}

impl DeadlineObserver for TimerfdShared {
    fn deadline_expired(&self, registration: u64, now_ns: u64) -> Option<u64> {
        if registration != 0 && self.registration.load(Ordering::Acquire) != registration {
            return None;
        }
        let was_ready = self.poll_source.snapshot().0.has(PollEvents::POLLIN);
        let next = self.publish_readiness(now_ns);
        if !was_ready && self.poll_source.snapshot().0.has(PollEvents::POLLIN) {
            self.waiters.wake_all();
        }
        if next.is_none() && registration != 0 {
            let _ = self.registration.compare_exchange(
                registration,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        next
    }
}

impl Drop for TimerfdShared {
    fn drop(&mut self) {
        let registration = self.registration.swap(0, Ordering::AcqRel);
        if registration != 0 {
            sched::cancel_deadline_observer(registration);
        }
    }
}

impl FileOps for TimerfdFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::InvalidArgument);
        }
        // 实时钟被设置后取消：清空累计到期数并返回 ECANCELED（Linux 语义）。
        if self.shared.take_cancelled() {
            self.shared.state.lock().expirations = 0;
            return Err(VfsError::Canceled);
        }
        let value = {
            let mut state = self.shared.state.lock();
            Self::refresh_locked(&mut state, sched::now_ns_public());
            if state.expirations == 0 {
                return Err(VfsError::WouldBlock);
            }
            let value = state.expirations;
            state.expirations = 0;
            value
        };
        buf[..8].copy_from_slice(&value.to_ne_bytes());
        self.shared.publish_readiness(sched::now_ns_public());
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
        self.shared.publish_readiness(sched::now_ns_public());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 实时钟设置后：登记了 (REALTIME+ABSTIME+CANCEL_ON_SET) 的 timerfd
    /// 被取消，read 返回 ECANCELED（一次性）；未登记的不受影响。
    #[test]
    fn clock_set_cancels_registered_timer() {
        let timer = TimerfdFileOps::new(0); // CLOCK_REALTIME
        timer.update_cancel_registration(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET);
        let now = sched::now_ns_public();
        let _ = timer.set_deadline(now, Some(now.saturating_add(1_000_000_000)), 0);
        cancel_timers_on_clock_set();
        let mut value = [0u8; 8];
        assert_eq!(timer.read_at(&mut value, 0), Err(VfsError::Canceled));

        // 未带 ABSTIME 的 settime 不登记 → 时钟设置不影响它。
        let other = TimerfdFileOps::new(0);
        other.update_cancel_registration(TFD_TIMER_CANCEL_ON_SET);
        let _ = other.set_deadline(now, Some(now.saturating_add(1_000_000_000)), 0);
        cancel_timers_on_clock_set();
        assert!(!other.shared.cancelled.load(Ordering::Acquire));

        // 已取消的 fd 上再次 settime：定时器照常 arm，但返回 ECANCELED 一次。
        let revived = TimerfdFileOps::new(0);
        revived.update_cancel_registration(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET);
        let _ = revived.set_deadline(now, Some(now.saturating_add(1_000_000_000)), 0);
        cancel_timers_on_clock_set();
        assert_eq!(
            revived.set_deadline(now, Some(now.saturating_add(1_000_000_000)), 0),
            Err(VfsError::Canceled)
        );
        // 取消状态一次性：下一次 settime 正常。
        assert!(revived
            .set_deadline(now, Some(now.saturating_add(1_000_000_000)), 0)
            .is_ok());
    }

    #[test]
    fn expiry_publishes_readiness_without_epoll_scanning() {
        let timer = TimerfdFileOps::new(1);
        let now = sched::now_ns_public();
        let _ = timer.set_deadline(now, Some(now.saturating_add(10)), 0);
        assert!(timer.shared.poll_source.snapshot().0.is_empty());
        let registration = timer.shared.registration.load(Ordering::Acquire);
        let _ = timer
            .shared
            .deadline_expired(registration, now.saturating_add(10));
        assert!(
            timer
                .shared
                .poll_source
                .snapshot()
                .0
                .has(PollEvents::POLLIN)
        );
        let mut value = [0u8; 8];
        assert_eq!(timer.read_at(&mut value, 0), Ok(8));
        assert_eq!(u64::from_ne_bytes(value), 1);
        assert!(timer.shared.poll_source.snapshot().0.is_empty());
    }
}
