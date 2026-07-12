//! 不依赖堆分配的 ELM 公共线类型。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmMixinMode {
    Chain = 1,
    Observer = 2,
    Exclusive = 3,
}

impl ElmMixinMode {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Chain),
            2 => Some(Self::Observer),
            3 => Some(Self::Exclusive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmPortAccessPolicy {
    Internal = 1,
    Public = 2,
    ExtensionOnly = 3,
}

impl ElmPortAccessPolicy {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Internal),
            2 => Some(Self::Public),
            3 => Some(Self::ExtensionOnly),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FlowDirection {
    Source = 1,
    Sink = 2,
    Duplex = 3,
    Control = 4,
}

impl FlowDirection {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Source),
            2 => Some(Self::Sink),
            3 => Some(Self::Duplex),
            4 => Some(Self::Control),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FlowMode {
    Exclusive = 1,
    Shared = 2,
    Ordered = 3,
    Pipeline = 4,
    Broadcast = 5,
}

impl FlowMode {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Exclusive),
            2 => Some(Self::Shared),
            3 => Some(Self::Ordered),
            4 => Some(Self::Pipeline),
            5 => Some(Self::Broadcast),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmPrincipalKind {
    Kernel = 1,
    UserAdmin = 2,
    ElmCell = 3,
}
