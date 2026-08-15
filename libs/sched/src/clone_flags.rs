//! `clone(2)` flag 位常量与 `CloneArgs` 结构。
//!
//! 数值与 Linux UAPI 严格对齐，这样上层 syscall 翻译层可以直接把 `flags: u64`
//! 透传进来，不必再做中间映射。

/// `clone()` 的 flag 位集，基于 Linux UAPI。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CloneFlags(pub u64);

impl CloneFlags {
    pub const CSIGNAL: u64 = 0x000000ff;
    pub const CLONE_VM: u64 = 0x00000100;
    pub const CLONE_FS: u64 = 0x00000200;
    pub const CLONE_FILES: u64 = 0x00000400;
    pub const CLONE_SIGHAND: u64 = 0x00000800;
    pub const CLONE_PIDFD: u64 = 0x00001000;
    pub const CLONE_PTRACE: u64 = 0x00002000;
    pub const CLONE_VFORK: u64 = 0x00004000;
    pub const CLONE_PARENT: u64 = 0x00008000;
    pub const CLONE_THREAD: u64 = 0x00010000;
    pub const CLONE_NEWNS: u64 = 0x00020000;
    pub const CLONE_SYSVSEM: u64 = 0x00040000;
    pub const CLONE_SETTLS: u64 = 0x00080000;
    pub const CLONE_PARENT_SETTID: u64 = 0x00100000;
    pub const CLONE_CHILD_CLEARTID: u64 = 0x00200000;
    pub const CLONE_DETACHED: u64 = 0x00400000;
    pub const CLONE_UNTRACED: u64 = 0x00800000;
    pub const CLONE_CHILD_SETTID: u64 = 0x01000000;
    pub const CLONE_NEWTIME: u64 = 0x00800000;
pub const CLONE_NEWCGROUP: u64 = 0x02000000;
    pub const CLONE_NEWUTS: u64 = 0x04000000;
    pub const CLONE_NEWIPC: u64 = 0x08000000;
    pub const CLONE_NEWUSER: u64 = 0x10000000;
    pub const CLONE_NEWPID: u64 = 0x20000000;
    pub const CLONE_NEWNET: u64 = 0x40000000;
    pub const CLONE_IO: u64 = 0x80000000;
    pub const CLONE_CLEAR_SIGHAND: u64 = 0x00000001_00000000;

    pub const fn from_raw(bits: u64) -> Self {
        Self(bits)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    pub const fn has(self, bit: u64) -> bool {
        (self.0 & bit) != 0
    }

    /// 取低 8 位作为退出信号号码（`CSIGNAL`）。0 表示无信号。
    pub const fn exit_signal(self) -> u8 {
        (self.0 & Self::CSIGNAL) as u8
    }

    /// fork(2) 等价 flags：仅 SIGCHLD。
    pub const fn fork_default() -> Self {
        Self(17 /* SIGCHLD */)
    }

    /// vfork(2) 等价 flags：CLONE_VFORK | CLONE_VM | SIGCHLD。
    pub const fn vfork_default() -> Self {
        Self(Self::CLONE_VFORK | Self::CLONE_VM | 17)
    }
}

/// clone()/clone3 的统一参数。老 clone ABI 由 syscall 层翻译进这套结构。
#[derive(Debug, Clone, Copy)]
pub struct CloneArgs {
    pub flags: CloneFlags,
    /// `CLONE_PIDFD` 写回地址或 clone3 的 pidfd 字段。
    pub pidfd: usize,
    /// 子进程用户栈顶。0 表示沿用父（fork 语义）。
    pub stack: usize,
    /// clone3 的 stack_size。老 clone 传 0。
    pub stack_size: usize,
    /// `CLONE_SETTLS` 时的 TLS 值。
    pub tls: usize,
    /// `CLONE_PARENT_SETTID` 写回地址（用户态指针，0 表示不需要）。
    pub parent_tid: usize,
    /// `CLONE_CHILD_SETTID` / `CLONE_CHILD_CLEARTID` 地址。
    pub child_tid: usize,
    /// clone3 独立 exit_signal。老 clone 使用 flags 低 8 位。
    pub exit_signal: u64,
    /// clone3 set_tid 数组用户指针。
    pub set_tid: usize,
    pub set_tid_size: usize,
    /// syscall 层解析后的根 namespace 指定 pid；0 表示由分配器自动选择。
    pub requested_pid: i32,
    /// clone3 cgroup fd。
    pub cgroup: usize,
}

impl CloneArgs {
    pub const fn exit_signal_checked(self) -> Option<u8> {
        let raw = if self.exit_signal != 0 {
            self.exit_signal
        } else {
            self.flags.exit_signal() as u64
        };
        if raw == 0 || raw <= 64 {
            Some(raw as u8)
        } else {
            None
        }
    }

    pub const fn exit_signal_raw(self) -> u8 {
        if self.exit_signal != 0 {
            self.exit_signal as u8
        } else {
            self.flags.exit_signal()
        }
    }

    pub const fn fork_default() -> Self {
        Self {
            flags: CloneFlags::fork_default(),
            pidfd: 0,
            stack: 0,
            stack_size: 0,
            tls: 0,
            parent_tid: 0,
            child_tid: 0,
            exit_signal: 0,
            set_tid: 0,
            set_tid_size: 0,
            requested_pid: 0,
            cgroup: 0,
        }
    }

    pub const fn vfork_default() -> Self {
        Self {
            flags: CloneFlags::vfork_default(),
            pidfd: 0,
            stack: 0,
            stack_size: 0,
            tls: 0,
            parent_tid: 0,
            child_tid: 0,
            exit_signal: 0,
            set_tid: 0,
            set_tid_size: 0,
            requested_pid: 0,
            cgroup: 0,
        }
    }
}
