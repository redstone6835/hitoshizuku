#![no_std]

//! MyGO Native ABI 的机器身份、启动契约与格式无关绑定模型。

extern crate alloc;

mod bind;
mod error;
mod model;
pub mod registry;
pub mod status;
pub mod wire;

pub use bind::{NativeAbiPolicy, bind_native_abi};
pub use error::{
    IncompatibleKind, MalformedKind, NativeAbiError, NativeAbiErrorCategory, ResourceKind,
    UnsupportedKind,
};
pub use model::{AbiImportRecord, BoundCallSlot, CapabilityRequirementRecord, NativeBindingPlan};
pub use registry::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, OPERATIONS, ObjectInterface, OperationId, OperationSpec,
    PAGE_SIZE, REQUIREMENTS, RequirementId, RequirementSpec, Rights, TargetArch, VmMapFlags,
    VmProtections, operation, operation_by_id, requirement, requirement_by_id,
};

#[cfg(test)]
mod tests;
