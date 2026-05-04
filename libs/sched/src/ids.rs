//! sched 自带的 POSIX ABI 身份类型。
//!
//! 与 `vfs::cred` 独立 —— sched crate 不依赖 vfs。跨层传递时由上层做 `From`。

use alloc::vec::Vec;

/// 用户 ID，对应 POSIX `uid_t`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Uid(pub u32);

impl Uid {
    pub const ROOT: Self = Self(0);
    pub const NOBODY: Self = Self(65534);
    pub const fn is_root(self) -> bool {
        self.0 == 0
    }
}

/// 组 ID，对应 POSIX `gid_t`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gid(pub u32);

impl Gid {
    pub const ROOT: Self = Self(0);
    pub const NOBODY: Self = Self(65534);
}

/// Linux capability 位集。见 `Capability` 枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapSet(pub u64);

impl CapSet {
    pub const EMPTY: Self = Self(0);
    pub const FULL: Self = Self(u64::MAX);

    pub const fn single(cap: Capability) -> Self {
        Self(1u64 << (cap as u32))
    }
    pub const fn has(self, cap: Capability) -> bool {
        (self.0 & (1u64 << (cap as u32))) != 0
    }
    pub const fn with(self, cap: Capability) -> Self {
        Self(self.0 | (1u64 << (cap as u32)))
    }
    pub const fn without(self, cap: Capability) -> Self {
        Self(self.0 & !(1u64 << (cap as u32)))
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    pub const fn from_raw(bits: u64) -> Self {
        Self(bits)
    }
}

/// Linux capability 子集。本 crate 只枚举调度/信号相关条目，其它由 vfs 侧定义。
/// 数值与 Linux UAPI 对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Capability {
    Chown = 0,
    DacOverride = 1,
    DacReadSearch = 2,
    Fowner = 3,
    Fsetid = 4,
    Kill = 5,
    Setgid = 6,
    Setuid = 7,
    Setpcap = 8,
    SysNice = 23,
    SysResource = 24,
}

/// 进程凭据快照。写时复制——每次 setuid/setgid/capset 替换整个 `Arc<Credentials>`。
#[derive(Debug, Clone)]
pub struct Credentials {
    pub uid: Uid,
    pub euid: Uid,
    pub suid: Uid,
    pub gid: Gid,
    pub egid: Gid,
    pub sgid: Gid,
    pub groups: Vec<Gid>,
    pub caps: CapSet,
}

impl Credentials {
    /// 超级用户凭据（uid=0、全能力）。init 专用。
    pub fn root() -> Self {
        Self {
            uid: Uid::ROOT,
            euid: Uid::ROOT,
            suid: Uid::ROOT,
            gid: Gid::ROOT,
            egid: Gid::ROOT,
            sgid: Gid::ROOT,
            groups: Vec::new(),
            caps: CapSet::FULL,
        }
    }

    /// 无特权用户凭据。
    pub fn unprivileged(uid: Uid, gid: Gid) -> Self {
        Self {
            uid,
            euid: uid,
            suid: uid,
            gid,
            egid: gid,
            sgid: gid,
            groups: Vec::new(),
            caps: CapSet::EMPTY,
        }
    }

    pub fn has_cap(&self, cap: Capability) -> bool {
        self.caps.has(cap)
    }
}
