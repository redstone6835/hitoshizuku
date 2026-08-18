//! `wait4` / `waitid` 的标志、目标选择、状态编码。
//!
//! POSIX `wstatus` 的 32 位编码：
//!
//! | 字段 | 位 | 含义 |
//! |---|---|---|
//! | 退出码 | bits 8..15 | 正常 exit(code) 时填 `(code & 0xff) << 8` |
//! | 终止信号 | bits 0..6 | 被信号 N 杀死时填 `N` |
//! | 核心转储位 | bit 7 | core dump 标志 |
//! | 0x7f | bits 0..7 全 1 | WIFSTOPPED 标识 |
//! | 0xffff | 全位 | WIFCONTINUED |
//!
//! 调度器内部使用 `WaitStatus(i32)`，构造器封装上述位运算；外部按需提供解码。

use alloc::sync::Arc;
use core::fmt;

use crate::group::ThreadGroup;
use crate::ids::Uid;
use crate::pid::PidT;
use crate::signal::SignalNumber;
use crate::task::TaskUsage;

/// `wait4` / `waitid` 的标志位。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaitOptions(pub u32);

impl WaitOptions {
    pub const WNOHANG: u32 = 0x00000001;
    pub const WUNTRACED: u32 = 0x00000002;
    pub const WSTOPPED: u32 = Self::WUNTRACED;
    pub const WEXITED: u32 = 0x00000004;
    pub const WCONTINUED: u32 = 0x00000008;
    pub const WNOWAIT: u32 = 0x01000000;
    pub const __WCLONE: u32 = 0x80000000;
    pub const __WALL: u32 = 0x40000000;
    pub const __WNOTHREAD: u32 = 0x20000000;

    pub const EMPTY: Self = Self(0);
    pub const fn from_raw(bits: u32) -> Self {
        Self(bits)
    }
    pub const fn raw(self) -> u32 {
        self.0
    }
    pub const fn has(self, bit: u32) -> bool {
        (self.0 & bit) != 0
    }
}

/// `wait4(pid, ...)` 的 pid 参数解释。
#[derive(Clone)]
pub enum WaitId {
    /// pid > 0：精确匹配。
    Pid(PidT),
    /// pid == -1：任意子。
    All,
    /// pid == 0：与调用者同 pgroup 的子。
    SameGroup,
    /// pid < -1：pgid == -pid 的子。
    Pgid(PidT),
    /// `waitid` 的 P_PIDFD：syscall 层已经把 fd 解成稳定线程组身份。
    Pidfd(Arc<ThreadGroup>),
}

impl fmt::Debug for WaitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pid(pid) => f.debug_tuple("Pid").field(pid).finish(),
            Self::All => f.write_str("All"),
            Self::SameGroup => f.write_str("SameGroup"),
            Self::Pgid(pgid) => f.debug_tuple("Pgid").field(pgid).finish(),
            Self::Pidfd(task) => f.debug_tuple("Pidfd").field(&Arc::as_ptr(task)).finish(),
        }
    }
}

impl PartialEq for WaitId {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Pid(lhs), Self::Pid(rhs)) => lhs == rhs,
            (Self::All, Self::All) => true,
            (Self::SameGroup, Self::SameGroup) => true,
            (Self::Pgid(lhs), Self::Pgid(rhs)) => lhs == rhs,
            (Self::Pidfd(lhs), Self::Pidfd(rhs)) => Arc::ptr_eq(lhs, rhs),
            _ => false,
        }
    }
}

impl Eq for WaitId {}

impl WaitId {
    /// 把 `wait4` 风格的 `pid` 参数解析成 [`WaitId`]。
    pub const fn from_wait4_pid(pid: PidT) -> Self {
        if pid > 0 {
            Self::Pid(pid)
        } else if pid == 0 {
            Self::SameGroup
        } else if pid == -1 {
            Self::All
        } else {
            Self::Pgid(-pid)
        }
    }
}

/// POSIX `wstatus` 编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitStatus(pub i32);

impl WaitStatus {
    pub const fn from_raw(v: i32) -> Self {
        Self(v)
    }
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// 子进程正常 exit。
    pub const fn from_exit(code: i32) -> Self {
        Self((code & 0xff) << 8)
    }

    /// 子进程被信号 N 终止（不带 core）。
    pub const fn from_signal(sig: SignalNumber) -> Self {
        Self(sig.raw() as i32 & 0x7f)
    }

    /// 子进程被信号 N 终止并 core。
    pub const fn from_signal_core(sig: SignalNumber) -> Self {
        Self((sig.raw() as i32 & 0x7f) | 0x80)
    }

    /// 子进程被信号 N 停止（WIFSTOPPED）。
    pub const fn from_stop(sig: SignalNumber) -> Self {
        Self(((sig.raw() as i32) << 8) | 0x7f)
    }

    /// 原始停止信号编码（支持 `PTRACE_O_TRACESYSGOOD` 的 `0x80|SIGTRAP`）。
    pub const fn from_stop_raw(raw_sig: i32) -> Self {
        Self(((raw_sig & 0xff) << 8) | 0x7f)
    }

    /// 子进程的 `PTRACE_EVENT_*` 停止：`(sig<<8) | (event<<16) | 0x7f`。
    pub const fn from_stop_event(sig: SignalNumber, event: u16) -> Self {
        Self(((sig.raw() as i32) << 8) | ((event as i32) << 16) | 0x7f)
    }

    /// 子进程从停止状态恢复。
    pub const fn continued() -> Self {
        Self(0xffff)
    }

    pub const fn wifexited(self) -> bool {
        self.0 != 0xffff && (self.0 & 0x7f) == 0
    }
    pub const fn wexitstatus(self) -> i32 {
        (self.0 >> 8) & 0xff
    }
    pub const fn wifsignaled(self) -> bool {
        let lo = self.0 & 0x7f;
        lo != 0 && lo != 0x7f
    }
    pub const fn wtermsig(self) -> i32 {
        self.0 & 0x7f
    }
    pub const fn wcoredump(self) -> bool {
        (self.0 & 0x80) != 0
    }
    pub const fn wifstopped(self) -> bool {
        (self.0 & 0xff) == 0x7f
    }
    pub const fn wstopsig(self) -> i32 {
        (self.0 >> 8) & 0xff
    }
    pub const fn wifcontinued(self) -> bool {
        self.0 == 0xffff
    }
}

/// `wait4` / `waitid` 的返回值。
#[derive(Debug, Clone, Copy)]
pub struct WaitResult {
    pub pid: PidT,
    pub status: WaitStatus,
    pub usage: TaskUsage,
    /// 被等待子进程的真实 uid。`waitid` 的 `si_uid` 需要 child 的 uid 而非
    /// waiter 的 uid，因此必须把 child 的凭据透出到 syscall 层（child 在返回前
    /// 已被 reap、pid 已释放，无法再由 pid 反查）。`pid == 0`（WNOHANG 无结果）
    /// 时无 child，置为 `Uid::ROOT`。
    pub child_uid: Uid,
}
