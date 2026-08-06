//! SOYO 解析与绑定的稳定错误分类。

use native_abi::{NativeAbiError, NativeAbiErrorCategory};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoyoErrorCategory {
    Malformed,
    Unsupported,
    Incompatible,
    Untrusted,
    ResourceExhausted,
    AllocationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedKind {
    Header,
    Reserved,
    Range,
    Alignment,
    Ordering,
    Overlap,
    Padding,
    String,
    Segment,
    Import,
    Capability,
    Relocation,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedKind {
    FormatVersion(u16),
    ArtifactKind(u16),
    TargetArch(u16),
    Endian(u8),
    PointerWidth(u8),
    HashAlgorithm(u16),
    RequiredFeature(u64),
    RequiredTable(u16),
    InitFini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompatibleKind {
    TargetArch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrustedKind {
    BuildIdMismatch,
    ContentHashMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    FileSize,
    ImageSize,
    DirectoryCount,
    TableBytes,
    StringBytes,
    StringLength,
    Segments,
    Imports,
    Capabilities,
    Relocations,
    TlsSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoyoError {
    Malformed(MalformedKind),
    Unsupported(UnsupportedKind),
    Incompatible(IncompatibleKind),
    Untrusted(UntrustedKind),
    ResourceExhausted(ResourceKind),
    AllocationFailed(ResourceKind),
    NativeAbi(NativeAbiError),
}

impl SoyoError {
    pub const fn category(self) -> SoyoErrorCategory {
        match self {
            Self::Malformed(_) => SoyoErrorCategory::Malformed,
            Self::Unsupported(_) => SoyoErrorCategory::Unsupported,
            Self::Incompatible(_) => SoyoErrorCategory::Incompatible,
            Self::Untrusted(_) => SoyoErrorCategory::Untrusted,
            Self::ResourceExhausted(_) => SoyoErrorCategory::ResourceExhausted,
            Self::AllocationFailed(_) => SoyoErrorCategory::AllocationFailed,
            Self::NativeAbi(error) => match error.category() {
                NativeAbiErrorCategory::Malformed => SoyoErrorCategory::Malformed,
                NativeAbiErrorCategory::Unsupported => SoyoErrorCategory::Unsupported,
                NativeAbiErrorCategory::Incompatible => SoyoErrorCategory::Incompatible,
                NativeAbiErrorCategory::ResourceExhausted => SoyoErrorCategory::ResourceExhausted,
            },
        }
    }
}
