//! 面向内核调试构建的采样式数据竞争检测运行时。
//!
//! 编译侧借用 LLVM ThreadSanitizer pass 生成的普通读写 hook，但运行时采用
//! KCSAN 风格的随机 watchpoint，而不是用户态 TSan 的 shadow memory。原子操作
//! 和 memintrinsic 在编译侧保持原语义，不由本 crate 替换。

#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

mod detector;
mod hooks;
mod report;

pub use detector::{DisableGuard, configure, disable, enabled, force_sample, install, set_enabled};
pub use report::{report, report_window};

/// 一次被检测内存访问的类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessKind {
    /// 普通读取。
    Read = 1,
    /// 普通写入。
    Write = 2,
    /// 同一个指令序列中的读后写。
    ReadWrite = 3,
    /// 显式标注的原子读取。
    AtomicRead = 4,
    /// 显式标注的原子写入或读改写。
    AtomicWrite = 5,
    /// MMIO/volatile 读取；默认不参与竞争判断。
    VolatileRead = 6,
    /// MMIO/volatile 写入；默认不参与竞争判断。
    VolatileWrite = 7,
}

impl AccessKind {
    /// 返回日志中使用的稳定名称。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "read-write",
            Self::AtomicRead => "atomic-read",
            Self::AtomicWrite => "atomic-write",
            Self::VolatileRead => "volatile-read",
            Self::VolatileWrite => "volatile-write",
        }
    }

    pub(crate) const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Read,
            2 => Self::Write,
            3 => Self::ReadWrite,
            4 => Self::AtomicRead,
            5 => Self::AtomicWrite,
            6 => Self::VolatileRead,
            7 => Self::VolatileWrite,
            _ => Self::Read,
        }
    }

    pub(crate) const fn is_write(self) -> bool {
        matches!(
            self,
            Self::Write | Self::ReadWrite | Self::AtomicWrite | Self::VolatileWrite
        )
    }

    pub(crate) const fn is_atomic(self) -> bool {
        matches!(self, Self::AtomicRead | Self::AtomicWrite)
    }

    pub(crate) const fn is_volatile(self) -> bool {
        matches!(self, Self::VolatileRead | Self::VolatileWrite)
    }
}

/// 报告中保存的一侧访问。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// 访问起始虚拟地址。
    pub address: usize,
    /// 访问字节数。
    pub size: usize,
    /// 访问类型。
    pub kind: AccessKind,
    /// 执行访问的 CPU 槽。
    pub cpu: usize,
    /// 当时的根任务 ID；启动早期或中断上下文可能为 0。
    pub task: u64,
    /// hook 返回地址，可用同次构建的 ELF 做离线符号化。
    pub pc: usize,
    /// 平台稳定计数器原始值。
    pub timestamp: u64,
}

/// 一条已经确认的冲突报告。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// 报告 ring 中的单调序号。
    pub sequence: u64,
    /// 建立 watchpoint 的访问。
    pub first: Access,
    /// 命中 watchpoint 的访问。
    pub second: Access,
}

/// 报告 ring 当前可读取窗口。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportWindow {
    /// 仍被保留的第一个序号。
    pub first_sequence: u64,
    /// 下一条将分配的序号。
    pub next_sequence: u64,
    /// 因 ring 覆盖而不再保留的累计报告数。
    pub overwritten: u64,
}

/// 运行时依赖的只读平台回调。
#[derive(Clone, Copy)]
pub struct RuntimeHooks {
    /// 返回当前根任务 ID；无任务时返回 0。
    pub current_task: fn() -> u64,
    /// 返回平台稳定计数器原始值。
    pub timestamp: fn() -> u64,
}

/// 采样检测器配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// 每 CPU 平均多少次普通访问建立一次 watchpoint。
    pub sample_interval: u32,
    /// watchpoint 建立后执行的自旋迭代数。
    pub delay_iterations: u32,
    /// 是否重复报告同一地址与同一对调用点。
    pub report_repeated: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sample_interval: 4_096,
            delay_iterations: 4_096,
            report_repeated: false,
        }
    }
}

/// 检测器累计统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 成功建立的采样 watchpoint 数。
    pub samples: u64,
    /// 因哈希槽占用而放弃的采样数。
    pub watchpoint_misses: u64,
    /// 命中冲突的 watchpoint 数。
    pub conflicts: u64,
    /// 写入报告 ring 的报告数。
    pub reports: u64,
    /// 因报告发布器正忙而丢弃的报告数。
    pub dropped_reports: u64,
    /// 被调用点去重抑制的报告数。
    pub duplicate_reports: u64,
    /// 当前活动 watchpoint 数。
    pub active_watchpoints: usize,
}

/// 返回检测器累计统计。
pub fn stats() -> Stats {
    detector::stats()
}

/// 显式提交一次访问；自动插桩 hook 最终也进入该入口。
///
/// 大于 16 字节的范围会被拆成多个相邻 watchpoint 粒度。`pc` 应是调用点的
/// 返回地址；不需要定位时可以传 0。
pub fn check_access(address: usize, size: usize, kind: AccessKind, pc: usize) {
    detector::check_access(address, size, kind, pc);
}

/// 判断目标地址当前是否存在活动 watchpoint，主要供内核自测与诊断使用。
pub fn watchpoint_active_for(address: usize) -> bool {
    detector::watchpoint_active_for(address)
}
