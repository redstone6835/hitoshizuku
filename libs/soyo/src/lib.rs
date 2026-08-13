#![no_std]

//! SOYO 直接可执行格式的 Wire 定义、解析与格式校验。

extern crate alloc;

pub mod component;
mod decode;
mod error;
mod format;
mod layout;
mod metadata;
mod parse;
mod reader;
pub mod registry;
mod source;
mod trust;
mod validate;
pub mod wire;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use component::{
    ComponentGraphError, ComponentGraphIdentity, ComponentGraphNode, ComponentGraphPlan,
    plan_component_graph,
};
pub use error::{
    IncompatibleKind, MalformedKind, ResourceKind, SoyoError, SoyoErrorCategory, UnsupportedKind,
    UntrustedKind,
};
pub use layout::{
    SoyoMappedSegment, SoyoProcessLayout, SoyoRuntimeLayoutInput, plan_mapped_segments,
    plan_runtime_layout,
};
pub use metadata::{
    AbiImport, CapabilityRequirement, ComponentDependency, ComponentInfo, ComponentMetadata,
    DirectoryEntry, DynamicRelocation, ImageSegment, Relocation, RuntimeInfo, SoyoHeader,
    SoyoMetadata, SoyoSignature, SymbolExport, SymbolImport,
};
pub use parse::read_soyo;
pub use reader::{SliceSoyoReader, SoyoReadAt, SoyoReadError, SoyoReadLimits};
pub use trust::{
    SignatureTrust, SignatureTrustError, SignatureTrustPolicy, TrustedPublicKey, signature_message,
    verify_metadata_signature,
};
pub use validate::{
    SoyoComponentPlan, SoyoLoadPlan, SoyoTargetPolicy, validate_component_soyo, validate_soyo,
};

#[cfg(test)]
mod tests;
