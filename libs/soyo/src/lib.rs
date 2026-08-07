#![no_std]

//! SOYO 直接可执行格式的 Wire 定义、解析与格式校验。

extern crate alloc;

mod decode;
mod error;
mod format;
mod layout;
mod metadata;
mod parse;
mod reader;
pub mod registry;
mod source;
mod validate;
pub mod wire;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::{
    IncompatibleKind, MalformedKind, ResourceKind, SoyoError, SoyoErrorCategory, UnsupportedKind,
    UntrustedKind,
};
pub use layout::{
    SoyoMappedSegment, SoyoProcessLayout, SoyoRuntimeLayoutInput, plan_mapped_segments,
    plan_runtime_layout,
};
pub use metadata::{
    AbiImport, CapabilityRequirement, DirectoryEntry, ImageSegment, Relocation, RuntimeInfo,
    SoyoHeader, SoyoMetadata,
};
pub use parse::read_soyo;
pub use reader::{SliceSoyoReader, SoyoReadAt, SoyoReadError, SoyoReadLimits};
pub use validate::{SoyoLoadPlan, SoyoTargetPolicy, validate_soyo};

#[cfg(test)]
mod tests;
