//! Native ABI 绑定阶段的稳定错误分类。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAbiErrorCategory {
    Malformed,
    Unsupported,
    Incompatible,
    ResourceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedKind {
    Import,
    Capability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedKind {
    AbiFamily(u16),
    RequiredRequirement(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompatibleKind {
    AbiEpoch(u16),
    Operation(u32),
    Signature(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    CallSlots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAbiError {
    Malformed(MalformedKind),
    Unsupported(UnsupportedKind),
    Incompatible(IncompatibleKind),
    ResourceExhausted(ResourceKind),
}

impl NativeAbiError {
    pub const fn category(self) -> NativeAbiErrorCategory {
        match self {
            Self::Malformed(_) => NativeAbiErrorCategory::Malformed,
            Self::Unsupported(_) => NativeAbiErrorCategory::Unsupported,
            Self::Incompatible(_) => NativeAbiErrorCategory::Incompatible,
            Self::ResourceExhausted(_) => NativeAbiErrorCategory::ResourceExhausted,
        }
    }
}
