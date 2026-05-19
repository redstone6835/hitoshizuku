//! 对平台相关基本操作（如CPU、内存等）的抽象接口定义。
//!
//! 这些 trait 定义了平台相关的操作接口，规范化平台具体操作实现的功能和语义，以
//! 便其他上层接口直接调用，而不需要使用平台特定如 `[cfg(target_arch = "...")]`
//! 的条件编译。

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
