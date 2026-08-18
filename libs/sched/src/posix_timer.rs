//! POSIX per-process 定时器（`timer_create` 族系统调用的实现核心）。
//!
//! 壁钟定时器（`CLOCK_REALTIME`/`CLOCK_MONOTONIC`/`CLOCK_BOOTTIME`）复用调度器的
//! `DeadlineObserver` 机制（与 timerfd 相同的注册/取消/周期追赶语义）；CPU 时钟
//! 定时器（`CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID`）在周期 tick 上
//! 按 CPU 时间域检查（Linux 同样在 scheduler tick 检查 CPU 定时器）。
//!
//! 信号投递支持 `SIGEV_NONE`/`SIGEV_SIGNAL`/`SIGEV_THREAD_ID`；按 Linux 语义做
//! pending 合并（同信号已排队时只累计 overrun）与 overrun 计数（追赶周期）。
//!
//! 所有时间量内部以 ns 表示：壁钟定时器为单调域，CPU 时钟定时器为 CPU 时间域；
//! 时钟域换算由内核 syscall 胶水层完成（REALTIME 绝对时间需要 `REALTIME_OFFSET`）。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;

use crate::pid::PidT;
use crate::scheduler::{
    DeadlineObserver, cancel_deadline_observer, deliver_shared_signal_to_group, now_ns_public,
    register_deadline_observer, reserve_deadline_observer_id, signal_wakeup,
};
use crate::signal::{SigInfo, SignalNumber};
use crate::sync::Spinlock;
use crate::task::Task;

/// 每个线程组最多允许的定时器数（超出返回 `EAGAIN`）。
pub const MAX_TIMERS_PER_GROUP: usize = 64;

/// `timer_t` 编码：高 8 位时钟 id、低 24 位序号（与 Linux `(clockid << 24) | id` 一致）。
const TIMER_ID_CLOCK_SHIFT: u32 = 24;
const TIMER_ID_MASK: u32 = 0x00ff_ffff;

/// `SI_TIMER` 的 si_code。
const SI_TIMER_CODE: i32 = -2;
/// siginfo_t 中 `_timer` 成员的字段偏移（Linux UAPI 与 musl 布局一致）。
const SI_TIMERID_OFF: usize = 16;
const SI_OVERRUN_OFF: usize = 20;
const SI_SIGVAL_OFF: usize = 24;
const SI_SIGNAL_OFF: usize = 0;
const SI_CODE_OFF: usize = 8;

/// 支持的定时器时钟。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimerClock {
    Realtime,
    Monotonic,
    Boottime,
    ProcessCpu,
    ThreadCpu,
}

impl TimerClock {
    /// 按 Linux `clockid` 值解析（0/1/2/3/7）。
    pub fn from_clockid(clockid: i32) -> Option<Self> {
        match clockid {
            0 => Some(Self::Realtime),
            1 => Some(Self::Monotonic),
            2 => Some(Self::ProcessCpu),
            3 => Some(Self::ThreadCpu),
            7 => Some(Self::Boottime),
            _ => None,
        }
    }

    fn clockid(self) -> i32 {
        match self {
            Self::Realtime => 0,
            Self::Monotonic => 1,
            Self::ProcessCpu => 2,
            Self::ThreadCpu => 3,
            Self::Boottime => 7,
        }
    }

    pub fn is_cpu_clock(self) -> bool {
        matches!(self, Self::ProcessCpu | Self::ThreadCpu)
    }
}

/// 信号投递方式（`sigevent.sigev_notify`）。
#[derive(Clone, Copy, Debug)]
pub enum SigevNotify {
    /// `SIGEV_NONE`：到期不发信号。
    None,
    /// `SIGEV_SIGNAL` / `SIGEV_THREAD_ID`：投递 `signo`，`si_value` = `value`。
    Signal { signo: SignalNumber, value: u64 },
}

/// 定时器规格（内部以 ns 表示；壁钟为单调域，CPU 时钟为 CPU 时间域）。
///
/// `deadline_ns == 0` 表示解除定时器。
#[derive(Clone, Copy, Debug)]
pub struct TimerSpec {
    pub deadline_ns: u64,
    pub interval_ns: u64,
}

struct TimerInner {
    clock: TimerClock,
    /// 创建者任务（生命周期归属 + `SIGEV_SIGNAL` 的组投递依据）。
    owner: Weak<Task>,
    /// `SIGEV_THREAD_ID` 的投递目标线程；`None` = 投递到线程组。
    target_tid: Option<PidT>,
    notify: SigevNotify,
    deadline: Option<u64>,
    interval_ns: u64,
    /// 距上次排队信号以来错过的到期次数（Linux `it_overrun` 语义）。
    overrun: i64,
}

/// 一个 POSIX 定时器。由全局 [`TIMERS`] 表持有 `Arc`，同时是自身的
/// `DeadlineObserver`（壁钟定时器经调度器注册）。
pub struct PosixTimer {
    /// 编码后的 `timer_t`（对用户态可见的句柄）。
    timer_t: u32,
    inner: Spinlock<TimerInner>,
    /// DeadlineObserver 注册号（0 = 未注册）。独立于 `inner` 以便无锁交换。
    registration: AtomicU64,
    self_weak: Spinlock<Weak<PosixTimer>>,
}

static TIMERS: Spinlock<Vec<Arc<PosixTimer>>> = Spinlock::new(Vec::new());
static NEXT_TIMER_SEQ: AtomicU64 = AtomicU64::new(1);

fn encode_timer_t(clock: TimerClock, id: u32) -> u32 {
    ((clock.clockid() as u32) << TIMER_ID_CLOCK_SHIFT) | (id & TIMER_ID_MASK)
}

/// 按 tid 查找任务（SIGEV_THREAD_ID 目标解析；pid 注册表弱引用升级失败视为不存在）。
pub fn lookup_task(pid: PidT) -> Option<Arc<Task>> {
    crate::scheduler::root_pid_ns()
        .registry()
        .lookup(pid)?
        .upgrade()
}

/// 按 `timer_t` 查找定时器；顺带清理 owner 已销毁的僵尸条目。
fn find(timer_t: u32) -> Option<Arc<PosixTimer>> {
    let mut timers = TIMERS.lock();
    let mut i = 0;
    while i < timers.len() {
        let dead = timers[i].inner.lock().owner.upgrade().is_none();
        if dead {
            timers.swap_remove(i);
        } else {
            i += 1;
        }
    }
    timers.iter().find(|t| t.timer_t == timer_t).cloned()
}

impl PosixTimer {
    /// 组装 SI_TIMER 的 128 字节 siginfo_t（musl/Linux UAPI 布局）。
    fn build_siginfo_raw(&self, signo: SignalNumber, overrun: i64, value: u64) -> [u8; 128] {
        let mut raw = [0u8; 128];
        raw[SI_SIGNAL_OFF..SI_SIGNAL_OFF + 4].copy_from_slice(&(signo.raw() as i32).to_le_bytes());
        raw[SI_CODE_OFF..SI_CODE_OFF + 4].copy_from_slice(&SI_TIMER_CODE.to_le_bytes());
        raw[SI_TIMERID_OFF..SI_TIMERID_OFF + 4].copy_from_slice(&self.timer_t.to_le_bytes());
        raw[SI_OVERRUN_OFF..SI_OVERRUN_OFF + 4].copy_from_slice(&(overrun as i32).to_le_bytes());
        raw[SI_SIGVAL_OFF..SI_SIGVAL_OFF + 8].copy_from_slice(&value.to_le_bytes());
        raw
    }

    /// 到期触发：累计 overrun；按 Linux 合并语义排队信号（同信号已 pending 时
    /// 只累计不排队）。
    fn fire(&self, overruns: i64) {
        let mut inner = self.inner.lock();
        inner.overrun = inner.overrun.saturating_add(overruns);
        let SigevNotify::Signal { signo, value } = inner.notify else {
            return; // SIGEV_NONE
        };
        let Some(owner) = inner.owner.upgrade() else {
            return;
        };
        let target_tid = inner.target_tid;
        let overrun_now = inner.overrun;
        let pending = match target_tid {
            Some(tid) => lookup_task(tid)
                .map(|t| t.signal.has_pending_in(signo.bit()))
                .unwrap_or(true),
            None => owner.shared_signal().has_pending_in(signo.bit()),
        };
        if pending {
            inner.overrun = inner.overrun.saturating_add(1);
            return;
        }
        inner.overrun = 0;
        drop(inner);
        let raw = self.build_siginfo_raw(signo, overrun_now, value);
        let info = SigInfo {
            sig: signo,
            code: SI_TIMER_CODE,
            sender_pid: 0,
            sender_uid: crate::ids::Uid::ROOT,
            raw: Some(raw),
        };
        match target_tid {
            Some(tid) => {
                if let Some(target) = lookup_task(tid) {
                    if target.is_kernel_task() {
                        return;
                    }
                    target.signal.deliver(info.clone());
                    signal_wakeup(&target, &info);
                }
            }
            None => {
                deliver_shared_signal_to_group(&owner.thread_group(), info);
            }
        }
    }

    /// 注册（或重装）壁钟 deadline observer；已过期时立即触发并追赶。
    fn arm_observer(&self, deadline_ns: u64) {
        let now = now_ns_public();
        if deadline_ns <= now {
            self.deadline_expired(0, now);
            return;
        }
        let old = self.registration.swap(0, Ordering::AcqRel);
        if old != 0 {
            cancel_deadline_observer(old);
        }
        let registration = reserve_deadline_observer_id();
        self.registration.store(registration, Ordering::Release);
        let Some(this) = self.self_weak.lock().upgrade() else {
            return;
        };
        let observer: Arc<dyn DeadlineObserver> = this;
        if !register_deadline_observer(registration, deadline_ns, Arc::downgrade(&observer)) {
            if self
                .registration
                .compare_exchange(registration, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.deadline_expired(0, now_ns_public());
            }
        }
    }

    fn cancel_observer(&self) {
        let registration = self.registration.swap(0, Ordering::AcqRel);
        if registration != 0 {
            cancel_deadline_observer(registration);
        }
    }
}

impl DeadlineObserver for PosixTimer {
    /// 壁钟到期回调。返回下一次到期时刻（周期定时器）或 `None`（停止）。
    fn deadline_expired(&self, registration: u64, now_ns: u64) -> Option<u64> {
        if registration != 0 && self.registration.load(Ordering::Acquire) != registration {
            return None; // 已被 delete/disarm 取代的陈旧回调
        }
        let mut inner = self.inner.lock();
        let Some(deadline) = inner.deadline else {
            return None;
        };
        if inner.interval_ns == 0 {
            inner.deadline = None;
            drop(inner);
            self.cancel_observer();
            self.fire(0);
            return None;
        }
        let missed = now_ns.saturating_sub(deadline) / inner.interval_ns;
        let next = deadline + (missed + 1) * inner.interval_ns;
        inner.deadline = Some(next);
        let overruns = missed as i64;
        drop(inner);
        self.fire(overruns);
        Some(next)
    }
}

impl Drop for PosixTimer {
    fn drop(&mut self) {
        self.cancel_observer();
    }
}

/// 创建定时器并返回编码后的 `timer_t`。
pub fn create(
    clock: TimerClock,
    owner: &Arc<Task>,
    notify: SigevNotify,
    target_tid: Option<PidT>,
) -> Result<u32, Errno> {
    {
        let timers = TIMERS.lock();
        let owner_group = owner.thread_group();
        let count = timers
            .iter()
            .filter(|t| {
                t.inner
                    .lock()
                    .owner
                    .upgrade()
                    .map(|o| Arc::ptr_eq(&o.thread_group(), &owner_group))
                    .unwrap_or(false)
            })
            .count();
        if count >= MAX_TIMERS_PER_GROUP {
            return Err(Errno::EAGAIN);
        }
    }
    let id = (NEXT_TIMER_SEQ.fetch_add(1, Ordering::Relaxed) as u32) & TIMER_ID_MASK;
    let timer_t = encode_timer_t(clock, id);
    let timer = Arc::new(PosixTimer {
        timer_t,
        inner: Spinlock::new(TimerInner {
            clock,
            owner: Arc::downgrade(owner),
            target_tid,
            notify,
            deadline: None,
            interval_ns: 0,
            overrun: 0,
        }),
        registration: AtomicU64::new(0),
        self_weak: Spinlock::new(Weak::new()),
    });
    *timer.self_weak.lock() = Arc::downgrade(&timer);
    TIMERS.lock().push(timer);
    Ok(timer_t)
}

/// 查询定时器的时钟域（解码 `timer_t` 高 8 位）。
pub fn clock_of(timer_t: u32) -> Option<TimerClock> {
    TimerClock::from_clockid((timer_t >> TIMER_ID_CLOCK_SHIFT) as i32)
}

/// 当前"现在"（按时钟域）：壁钟为单调时间，CPU 时钟为 owner 任务已消耗 CPU 时间。
pub fn now_in_domain(clock: TimerClock, owner: &Task) -> u64 {
    match clock {
        TimerClock::Realtime | TimerClock::Monotonic | TimerClock::Boottime => now_ns_public(),
        TimerClock::ThreadCpu => owner.cpu_runtime_ns(now_ns_public()),
        TimerClock::ProcessCpu => owner.thread_group().cpu_runtime_ns(),
    }
}

/// 挂载/解除定时器。`spec.deadline_ns == 0` 时解除。
///
/// 返回 `false` 表示 `timer_t` 无效。
pub fn arm(timer_t: u32, spec: TimerSpec) -> bool {
    let Some(timer) = find(timer_t) else {
        return false;
    };
    let mut inner = timer.inner.lock();
    inner.overrun = 0;
    if spec.deadline_ns == 0 {
        inner.deadline = None;
        inner.interval_ns = 0;
        drop(inner);
        timer.cancel_observer();
        return true;
    }
    inner.deadline = Some(spec.deadline_ns);
    inner.interval_ns = spec.interval_ns;
    let is_cpu = inner.clock.is_cpu_clock();
    drop(inner);
    if !is_cpu {
        timer.arm_observer(spec.deadline_ns);
    }
    true
}

/// 读取剩余时间与周期（ns）。`None` = `timer_t` 无效。
pub fn gettime(timer_t: u32) -> Option<(u64, u64)> {
    let timer = find(timer_t)?;
    let inner = timer.inner.lock();
    let interval_ns = inner.interval_ns;
    let Some(deadline) = inner.deadline else {
        return Some((0, 0));
    };
    let now = match inner.clock {
        TimerClock::Realtime | TimerClock::Monotonic | TimerClock::Boottime => now_ns_public(),
        TimerClock::ThreadCpu => inner
            .owner
            .upgrade()
            .map(|o| o.cpu_runtime_ns(now_ns_public()))
            .unwrap_or(u64::MAX),
        TimerClock::ProcessCpu => inner
            .owner
            .upgrade()
            .map(|o| o.thread_group().cpu_runtime_ns())
            .unwrap_or(u64::MAX),
    };
    Some((deadline.saturating_sub(now), interval_ns))
}

/// 读取 overrun 计数（`i64`，恒非负）。
pub fn getoverrun(timer_t: u32) -> Option<i64> {
    find(timer_t).map(|t| t.inner.lock().overrun)
}

/// 删除定时器。返回 `false` 表示 `timer_t` 无效。
pub fn delete(timer_t: u32) -> bool {
    let mut timers = TIMERS.lock();
    let Some(idx) = timers.iter().position(|t| t.timer_t == timer_t) else {
        return false;
    };
    let timer = timers.swap_remove(idx);
    drop(timers);
    timer.inner.lock().deadline = None;
    timer.cancel_observer();
    true
}

/// 周期 tick 钩子：检查当前 CPU 任务的 CPU 时钟定时器（Linux 在 scheduler tick
/// 检查 `current` 的 CPU 定时器，休眠任务不消耗 CPU 时间故无需检查）。
pub(crate) fn fire_expired_cpu_timers(now_ns: u64, cpu_id: usize) {
    let Some(current) = crate::scheduler::runqueue_of(cpu_id).current() else {
        return;
    };
    let current_group = current.thread_group();
    let mut fired = Vec::new();
    {
        let mut timers = TIMERS.lock();
        let mut i = 0;
        while i < timers.len() {
            let timer = timers[i].clone();
            let mut inner = timer.inner.lock();
            // 顺带清理 owner 已销毁的僵尸条目。
            let Some(owner) = inner.owner.upgrade() else {
                drop(inner);
                timers.swap_remove(i);
                continue;
            };
            i += 1;
            if !inner.clock.is_cpu_clock() {
                continue;
            }
            let Some(deadline) = inner.deadline else {
                continue;
            };
            // 只检查与当前任务相关的定时器（THREAD → 本任务；PROCESS → 本组）。
            let (relevant, cpu_now) = match inner.clock {
                TimerClock::ThreadCpu => (
                    Arc::ptr_eq(&owner, &current),
                    current.cpu_runtime_ns(now_ns),
                ),
                TimerClock::ProcessCpu => {
                    let group = owner.thread_group();
                    (Arc::ptr_eq(&group, &current_group), group.cpu_runtime_ns())
                }
                _ => (false, 0),
            };
            if !relevant || cpu_now < deadline {
                continue;
            }
            if inner.interval_ns == 0 {
                inner.deadline = None;
                drop(inner);
                timer.cancel_observer();
                fired.push((timer, 0));
            } else {
                let missed = (cpu_now - deadline) / inner.interval_ns;
                inner.deadline = Some(deadline + (missed + 1) * inner.interval_ns);
                drop(inner);
                fired.push((timer, missed as i64));
            }
        }
    }
    for (timer, overruns) in fired {
        timer.fire(overruns);
    }
}

/// 线程退出清理：`SIGEV_THREAD_ID` 定时器在目标线程终止时自动删除（Linux 语义）。
pub fn release_timers_of_thread(tid: PidT) {
    let mut timers = TIMERS.lock();
    let mut i = 0;
    while i < timers.len() {
        let remove = timers[i]
            .inner
            .lock()
            .target_tid
            .map(|t| t == tid)
            .unwrap_or(false);
        if remove {
            let timer = timers.swap_remove(i);
            timer.inner.lock().deadline = None;
            timer.cancel_observer();
        } else {
            i += 1;
        }
    }
}

/// 线程组退出清理：删除该组创建的全部定时器（POSIX 定时器属于进程）。
pub fn release_timers_of_group(group: &Arc<crate::group::ThreadGroup>) {
    let mut timers = TIMERS.lock();
    let mut i = 0;
    while i < timers.len() {
        let remove = timers[i]
            .inner
            .lock()
            .owner
            .upgrade()
            .map(|o| Arc::ptr_eq(&o.thread_group(), group))
            .unwrap_or(true);
        if remove {
            let timer = timers.swap_remove(i);
            timer.inner.lock().deadline = None;
            timer.cancel_observer();
        } else {
            i += 1;
        }
    }
}
