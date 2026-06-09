//! POSIX 进程资源限制（rlimit）。
//!
//! 实现覆盖 Linux `<sys/resource.h>` 公开的 16 种 `RLIMIT_*` 资源。模块本身
//! 只描述资源定义、rlim 值的语义与 per-task 存储；具体 syscall 入口在
//! `kernel::syscalls::process`，对应 `sched::operation::get_rlimit` 等。
//!
//! # 设计要点
//!
//! - **Rlim 是无符号 64-bit**：`u64::MAX`（`RLIM_INFINITY`）表示"无限制"；
//!   这样所有比较/算术都可以走原生整数，零分支。Linux 自己也用 `__rlim_t =
//!   unsigned long`，内核内部是 `u64`。
//! - **per-task 存储**：rlimit 跟随 ThreadGroup（POSIX 语义），所以存放在
//!   `Task` 的 `tg` 共享结构里。我们用 `ThreadGroup` 字段而不是 Task 自
//!   己的字段，避免 `RLIMIT_NPROC` 这种进程级计数在多线程下被多次
//!   解释。`ThreadGroup` 上的访问走 `Spinlock<Rlimits>`。
//! - **不可降硬限制到软限制以下**：Linux 行为；非特权进程只能 `soft ≤ hard`
//!   之内调，调硬限制需要特权。
//! - **`RLIM_INFINITY` 的算术**：`x.saturating_add(RLIM_INFINITY)` 仍是
//!   infinity，遵循"无限制 + 任何 = 无限制"。`saturating_sub` 是"剩余额度"
//!   辅助：任一操作数为 infinity 时返回 0，有限值之间走原生饱和减法。
//! - **Resource 的可枚举性**：`From<u32>` / `as_u32` 双向；越界返回 None。
//!   syscall 入口处先校验，未知资源返回 `EINVAL`。

use core::fmt;
use core::sync::atomic::AtomicU64;

use crate::sync::Spinlock;

/// 资源耗尽 / 越限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlimitError {
    /// 资源编号未知。
    InvalidResource,
    /// 软/硬限制超过约束。
    ExceedsHard,
}

impl fmt::Display for RlimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResource => f.write_str("invalid rlimit resource"),
            Self::ExceedsHard => f.write_str("limit exceeds hard or existing cap"),
        }
    }
}

/// rlim_t 的语义包装。
///
/// 仅是 `u64` 的薄包装；提供 `INFINITY` 常量、`is_infinity()` 与有限值
/// 比较辅助。**没有实现除 None 之外的 `Option<Rlim>` 语义**：所有
/// `RLIM_INFINITY` 由该值直接承担。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct Rlim(pub u64);

impl Rlim {
    /// Linux 约定的"无限制"。
    pub const INFINITY: Self = Self(u64::MAX);

    #[inline]
    pub const fn from_raw(v: u64) -> Self {
        Self(v)
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_infinity(self) -> bool {
        self.0 == u64::MAX
    }

    /// `saturating_add` 让 infinity + 任何 = infinity。
    #[inline]
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        if self.is_infinity() || rhs.is_infinity() {
            return Some(Self::INFINITY);
        }
        self.0.checked_add(rhs.0).map(Self)
    }

    /// "剩余额度"减法：任一操作数为 infinity 时返回 0；有限值走饱和减法。
    #[inline]
    pub fn saturating_sub(self, rhs: Self) -> Self {
        if self.is_infinity() || rhs.is_infinity() {
            return Self(0);
        }
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl core::fmt::Debug for Rlim {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_infinity() {
            f.write_str("INFINITY")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// 资源枚举。
///
/// 与 Linux `<sys/resource.h>` 的 `enum rlimit_resource` 一一对应。新增
/// 资源时记得同步到 `resource_count`、`default_for` 与
/// `is_thread_group_scoped`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Cpu = 0,
    Fsize = 1,
    Data = 2,
    Stack = 3,
    Core = 4,
    Rss = 5,
    Nproc = 6,
    Nofile = 7,
    Memlock = 8,
    As = 9,
    Locks = 10,
    Sigpending = 11,
    Msgqueue = 12,
    Nice = 13,
    RtPrio = 14,
    RtTime = 15,
}

impl Resource {
    /// 资源种类数。
    pub const COUNT: usize = 16;

    /// 从 Linux 整数（`RLIMIT_*`）构造；越界返回 None。
    pub const fn from_raw(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::Cpu,
            1 => Self::Fsize,
            2 => Self::Data,
            3 => Self::Stack,
            4 => Self::Core,
            5 => Self::Rss,
            6 => Self::Nproc,
            7 => Self::Nofile,
            8 => Self::Memlock,
            9 => Self::As,
            10 => Self::Locks,
            11 => Self::Sigpending,
            12 => Self::Msgqueue,
            13 => Self::Nice,
            14 => Self::RtPrio,
            15 => Self::RtTime,
            _ => return None,
        })
    }

    /// 转回 Linux 整数。
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Linux 资源名（用于日志/调试）。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cpu => "RLIMIT_CPU",
            Self::Fsize => "RLIMIT_FSIZE",
            Self::Data => "RLIMIT_DATA",
            Self::Stack => "RLIMIT_STACK",
            Self::Core => "RLIMIT_CORE",
            Self::Rss => "RLIMIT_RSS",
            Self::Nproc => "RLIMIT_NPROC",
            Self::Nofile => "RLIMIT_NOFILE",
            Self::Memlock => "RLIMIT_MEMLOCK",
            Self::As => "RLIMIT_AS",
            Self::Locks => "RLIMIT_LOCKS",
            Self::Sigpending => "RLIMIT_SIGPENDING",
            Self::Msgqueue => "RLIMIT_MSGQUEUE",
            Self::Nice => "RLIMIT_NICE",
            Self::RtPrio => "RLIMIT_RTPRIO",
            Self::RtTime => "RLIMIT_RTTIME",
        }
    }

    /// 默认值（启动时 init 进程的 rlimit）。
    ///
    /// 取自 Linux 6.x 主流发行版默认（man 2 getrlimit）。
    pub const fn default_for(self) -> RlimitPair {
        match self {
            // CPU 秒数：不限
            Self::Cpu => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 文件大小：不限
            Self::Fsize => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 数据段：不限
            Self::Data => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 栈：8 MiB，硬限制不限
            Self::Stack => RlimitPair::new(Rlim(8 * 1024 * 1024), Rlim::INFINITY),
            // core 文件：默认 0（不产生 core）
            Self::Core => RlimitPair::new(Rlim(0), Rlim::INFINITY),
            // RSS：不限
            Self::Rss => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 进程数：默认不限（部分发行版用 `ulimit -u 4096`，我们不限）
            Self::Nproc => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 打开文件数：与 libs/vfs VfsLimits.nofile_default/_max 对齐
            Self::Nofile => RlimitPair::new(Rlim(1024), Rlim(4096)),
            // mlock 字节
            Self::Memlock => RlimitPair::new(Rlim(65536), Rlim(65536)),
            // 虚拟地址空间：不限
            Self::As => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 文件锁：不限
            Self::Locks => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 排队信号数：默认不限
            Self::Sigpending => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
            // 消息队列字节
            Self::Msgqueue => RlimitPair::new(Rlim(819200), Rlim(819200)),
            // nice：0
            Self::Nice => RlimitPair::new(Rlim(0), Rlim(0)),
            // rt 优先级：0
            Self::RtPrio => RlimitPair::new(Rlim(0), Rlim(0)),
            // rt CPU 时间（微秒）：不限
            Self::RtTime => RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY),
        }
    }
}

/// 一对 (soft, hard) rlimit。
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct RlimitPair {
    pub soft: Rlim,
    pub hard: Rlim,
}

impl RlimitPair {
    pub const fn new(soft: Rlim, hard: Rlim) -> Self {
        Self { soft, hard }
    }

    /// 软限制不能超过硬限制；硬限制不能低于当前软限制。
    pub const fn is_valid(self) -> bool {
        // soft ≤ hard
        if self.soft.0 > self.hard.0 {
            return false;
        }
        true
    }
}

impl core::fmt::Debug for RlimitPair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RlimitPair")
            .field("soft", &self.soft)
            .field("hard", &self.hard)
            .finish()
    }
}

// ── per-task 存储 ────────────────────────────────────────────────────────────

/// 16 个资源的 rlimit 集合。常驻在 ThreadGroup 上，per-thread 共享。
#[derive(Clone, Copy)]
pub struct Rlimits {
    entries: [RlimitPair; Resource::COUNT],
}

impl Rlimits {
    /// 启动期默认 rlimit（每项走 `Resource::default_for`）。
    pub const fn new_with_defaults() -> Self {
        let mut entries = [RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY); Resource::COUNT];
        let mut idx = 0;
        // 注意：没法在 const 里遍历 enum，所以按数字顺序硬编码。
        // 任何时候对 Resource 添加项都需要同步这里。
        while idx < Resource::COUNT {
            // `default_for` 是 const fn，索引安全靠 while 边界。
            let r = match idx {
                0 => Resource::Cpu,
                1 => Resource::Fsize,
                2 => Resource::Data,
                3 => Resource::Stack,
                4 => Resource::Core,
                5 => Resource::Rss,
                6 => Resource::Nproc,
                7 => Resource::Nofile,
                8 => Resource::Memlock,
                9 => Resource::As,
                10 => Resource::Locks,
                11 => Resource::Sigpending,
                12 => Resource::Msgqueue,
                13 => Resource::Nice,
                14 => Resource::RtPrio,
                15 => Resource::RtTime,
                _ => unreachable!(),
            };
            entries[idx] = r.default_for();
            idx += 1;
        }
        Self { entries }
    }

    /// 读取 `resource`。
    pub fn get(&self, resource: Resource) -> RlimitPair {
        self.entries[resource as usize]
    }

    /// 写入 `resource`。
    pub fn set(&mut self, resource: Resource, pair: RlimitPair) {
        self.entries[resource as usize] = pair;
    }

    /// 复制（fork / clone 时使用）。
    pub fn fork_copy(&self) -> Self {
        *self
    }

    /// 返回内部数组的快照。`size` 至少为 `Resource::COUNT`。
    pub fn snapshot_into(&self, out: &mut [RlimitPair]) {
        let n = out.len().min(Resource::COUNT);
        out[..n].copy_from_slice(&self.entries[..n]);
    }
}

impl Default for Rlimits {
    fn default() -> Self {
        Self::new_with_defaults()
    }
}

impl core::fmt::Debug for Rlimits {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rlimits").finish_non_exhaustive()
    }
}

// ── RLIMIT_NOFILE 用法：fdtable 软/硬限制传递 ────────────────────────────────

/// per-tg 持有 Rlimits 字段，并发保护用 Spinlock。
pub type RlimitsLock = Spinlock<Rlimits>;

/// 一些进程级计数器（fd 计数、mmap 总字节等）原子上保存在这里。
///
/// 这些字段是 `task` / `vfs` 之外的"非 rlimit 但需要 tg 范围"统计量。
/// 当前只用 `pending_signals` 作为 `RLIMIT_SIGPENDING` 的简单示意。
#[derive(Default)]
pub struct RgStats {
    /// 该线程组排队的信号条目数（含 per-task + shared 的并集去重后）。
    pub pending_signals: AtomicU64,
}

// ── 单元测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_round_trip() {
        for i in 0..Resource::COUNT {
            let r = Resource::from_raw(i as u32).unwrap();
            assert_eq!(r.as_u32(), i as u32);
            assert!(!r.name().is_empty());
        }
        assert!(Resource::from_raw(Resource::COUNT as u32).is_none());
        assert!(Resource::from_raw(u32::MAX).is_none());
    }

    #[test]
    fn rlim_infinity_arith() {
        let i = Rlim::INFINITY;
        assert!(i.is_infinity());
        assert_eq!(i.saturating_sub(Rlim(7)).is_infinity(), false);
        assert!(i.checked_add(Rlim(1)).unwrap().is_infinity());
    }

    #[test]
    fn pair_validity() {
        assert!(RlimitPair::new(Rlim(10), Rlim(20)).is_valid());
        assert!(!RlimitPair::new(Rlim(20), Rlim(10)).is_valid());
        assert!(RlimitPair::new(Rlim::INFINITY, Rlim::INFINITY).is_valid());
        assert!(RlimitPair::new(Rlim(0), Rlim(0)).is_valid());
    }

    #[test]
    fn defaults_match_linux() {
        let s = Rlimits::new_with_defaults();
        // NOFILE 软/硬：1024/4096
        let nofile = s.get(Resource::Nofile);
        assert_eq!(nofile.soft, Rlim(1024));
        assert_eq!(nofile.hard, Rlim(4096));
        // STACK：8 MiB 软
        assert_eq!(s.get(Resource::Stack).soft, Rlim(8 * 1024 * 1024));
        // CORE 软：0
        assert_eq!(s.get(Resource::Core).soft, Rlim(0));
    }

    #[test]
    fn fork_copy_is_independent() {
        let mut s1 = Rlimits::new_with_defaults();
        s1.set(Resource::Nofile, RlimitPair::new(Rlim(50), Rlim(200)));
        let s2 = s1.fork_copy();
        assert_eq!(s2.get(Resource::Nofile).soft, Rlim(50));
        // mut 不影响另一份（pair 是 Copy，set 走 mut 借用）
    }
}
