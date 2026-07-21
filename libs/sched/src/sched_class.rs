//! 调度策略与调度类的稳定公共模型。
//!
//! `sched` 的核心数据结构只依赖这些抽象，不把 Linux syscall 参数或 kernel
//! 权限检查混进 runqueue。上层可以把 `sched_setattr` / `sched_setscheduler`
//! 一类 ABI 翻译成 [`SchedAttr`] 后交给本 crate。

use errno::Errno;

use crate::eevdf::{DEFAULT_BASE_SLICE_NS, NICE_MAX, NICE_MIN};

/// 调度类。runqueue 按 `Deadline > Realtime > Fair > Idle` 的顺序挑选。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum SchedClass {
    Deadline = 0,
    Realtime = 1,
    Fair = 2,
    Idle = 3,
}

/// 任务调度策略。数值不承诺等于 Linux UAPI，由 syscall 层负责翻译。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SchedPolicy {
    Fair = 0,
    RtFifo = 1,
    RtRoundRobin = 2,
    Deadline = 3,
    Idle = 4,
}

impl SchedPolicy {
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Fair),
            1 => Some(Self::RtFifo),
            2 => Some(Self::RtRoundRobin),
            3 => Some(Self::Deadline),
            4 => Some(Self::Idle),
            _ => None,
        }
    }

    pub const fn class(self) -> SchedClass {
        match self {
            Self::Deadline => SchedClass::Deadline,
            Self::RtFifo | Self::RtRoundRobin => SchedClass::Realtime,
            Self::Fair => SchedClass::Fair,
            Self::Idle => SchedClass::Idle,
        }
    }
}

/// RT 优先级范围。数值越大优先级越高。
pub const RT_PRIO_MIN: u8 = 1;
pub const RT_PRIO_MAX: u8 = 99;

/// 默认 RR 时间片：100ms，接近 Linux 默认 `sched_rr_timeslice_ms`。
pub const DEFAULT_RR_SLICE_NS: u64 = 100_000_000;

/// RT bandwidth 周期：每个 CPU 独立记账。
pub const DEFAULT_RT_PERIOD_NS: u64 = 1_000_000_000;
/// 默认 RT bandwidth 预算：保留 5% 给 fair/idle 任务和内核工作线程。
pub const DEFAULT_RT_RUNTIME_NS: u64 = 950_000_000;

/// deadline class 的保守默认参数。
pub const DEFAULT_DL_RUNTIME_NS: u64 = 4_000_000;
pub const DEFAULT_DL_DEADLINE_NS: u64 = 16_000_000;
pub const DEFAULT_DL_PERIOD_NS: u64 = 16_000_000;

/// 调度属性。所有策略共用一套结构，避免后续扩展公共 API。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedAttr {
    pub policy: SchedPolicy,
    pub nice: i8,
    pub slice_ns: u64,
    pub priority: u8,
    pub runtime_ns: u64,
    pub deadline_ns: u64,
    pub period_ns: u64,
}

impl SchedAttr {
    pub const fn fair(nice: i8, slice_ns: u64) -> Self {
        Self {
            policy: SchedPolicy::Fair,
            nice,
            slice_ns,
            priority: 0,
            runtime_ns: 0,
            deadline_ns: 0,
            period_ns: 0,
        }
    }

    pub const fn idle() -> Self {
        Self {
            policy: SchedPolicy::Idle,
            nice: NICE_MAX,
            slice_ns: DEFAULT_BASE_SLICE_NS,
            priority: 0,
            runtime_ns: 0,
            deadline_ns: 0,
            period_ns: 0,
        }
    }

    pub const fn rt_fifo(priority: u8) -> Self {
        Self {
            policy: SchedPolicy::RtFifo,
            nice: 0,
            slice_ns: 0,
            priority,
            runtime_ns: 0,
            deadline_ns: 0,
            period_ns: 0,
        }
    }

    pub const fn rt_round_robin(priority: u8, slice_ns: u64) -> Self {
        Self {
            policy: SchedPolicy::RtRoundRobin,
            nice: 0,
            slice_ns,
            priority,
            runtime_ns: 0,
            deadline_ns: 0,
            period_ns: 0,
        }
    }

    pub const fn deadline(runtime_ns: u64, deadline_ns: u64, period_ns: u64) -> Self {
        Self {
            policy: SchedPolicy::Deadline,
            nice: 0,
            slice_ns: 0,
            priority: 0,
            runtime_ns,
            deadline_ns,
            period_ns,
        }
    }

    pub fn normalized(mut self) -> Self {
        self.nice = self.nice.clamp(NICE_MIN, NICE_MAX);
        match self.policy {
            SchedPolicy::Fair => {
                if self.slice_ns == 0 {
                    self.slice_ns = DEFAULT_BASE_SLICE_NS;
                }
                self.priority = 0;
                self.runtime_ns = 0;
                self.deadline_ns = 0;
                self.period_ns = 0;
            }
            SchedPolicy::Idle => {
                self.nice = NICE_MAX;
                if self.slice_ns == 0 {
                    self.slice_ns = DEFAULT_BASE_SLICE_NS;
                }
                self.priority = 0;
                self.runtime_ns = 0;
                self.deadline_ns = 0;
                self.period_ns = 0;
            }
            SchedPolicy::RtFifo => {
                self.priority = self.priority.clamp(RT_PRIO_MIN, RT_PRIO_MAX);
                self.slice_ns = 0;
                self.runtime_ns = 0;
                self.deadline_ns = 0;
                self.period_ns = 0;
            }
            SchedPolicy::RtRoundRobin => {
                self.priority = self.priority.clamp(RT_PRIO_MIN, RT_PRIO_MAX);
                if self.slice_ns == 0 {
                    self.slice_ns = DEFAULT_RR_SLICE_NS;
                }
                self.runtime_ns = 0;
                self.deadline_ns = 0;
                self.period_ns = 0;
            }
            SchedPolicy::Deadline => {
                if self.runtime_ns == 0 {
                    self.runtime_ns = DEFAULT_DL_RUNTIME_NS;
                }
                if self.deadline_ns == 0 {
                    self.deadline_ns = DEFAULT_DL_DEADLINE_NS;
                }
                if self.period_ns == 0 {
                    self.period_ns = self.deadline_ns.max(DEFAULT_DL_PERIOD_NS);
                }
                self.priority = 0;
            }
        }
        self
    }

    pub fn validate(self) -> Result<Self, Errno> {
        let raw = self;
        let attr = self.normalized();
        match attr.policy {
            SchedPolicy::Fair | SchedPolicy::Idle => Ok(attr),
            SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => {
                if raw.priority < RT_PRIO_MIN || raw.priority > RT_PRIO_MAX {
                    Err(Errno::EINVAL)
                } else {
                    Ok(attr)
                }
            }
            SchedPolicy::Deadline => {
                if attr.runtime_ns == 0
                    || attr.deadline_ns == 0
                    || attr.period_ns == 0
                    || attr.runtime_ns > attr.deadline_ns
                    || attr.deadline_ns > attr.period_ns
                {
                    Err(Errno::EINVAL)
                } else {
                    Ok(attr)
                }
            }
        }
    }
}
