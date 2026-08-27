//! 对平台相关基本操作（如CPU、内存等）的抽象接口定义。
//!
//! 这些 trait 定义了平台相关的操作接口，规范化平台具体操作实现的功能和语义，以
//! 便其他上层接口直接调用，而不需要在调用点散落平台特定条件编译。

/// 内核支持的指令集架构身份。
///
/// 这是内核内部的通用身份，不是任何外部镜像或固件协议的 wire 值。各协议应
/// 保留自己的稳定枚举，并在 HAL 胶水层完成显式转换。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ArchitectureId {
    Unknown = 0,
    Riscv64 = 1,
    LoongArch64 = 2,
    X86_64 = 3,
}

impl ArchitectureId {
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Unknown),
            1 => Some(Self::Riscv64),
            2 => Some(Self::LoongArch64),
            3 => Some(Self::X86_64),
            _ => None,
        }
    }

    /// Linux `utsname.machine` 风格的规范名称。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Riscv64 => "riscv64",
            Self::LoongArch64 => "loongarch64",
            Self::X86_64 => "x86_64",
        }
    }

    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Physical address value passed across HAL boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysAddr(usize);

impl PhysAddr {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Virtual address value passed across HAL boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct VirtAddr(usize);

impl VirtAddr {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysPageTableRoot(usize);

impl PhysPageTableRoot {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Raw trap frame pointer passed across HAL boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TrapFramePtr(usize);

impl TrapFramePtr {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }
}
