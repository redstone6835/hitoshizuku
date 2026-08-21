#![no_std]

//! Hitoshizuku Native ABI 的机器身份、启动契约与格式无关绑定模型。

extern crate alloc;

mod bind;
mod component;
mod error;
mod handle;
mod model;
pub mod registry;
mod start_info;
pub mod status;
pub mod wire;

pub use bind::{NativeAbiPolicy, bind_native_abi};
pub use component::{
    ComponentLifecycleMachine, ComponentState, ComponentTlsAllocator, ComponentTlsReservation,
};
pub use error::{
    IncompatibleKind, MalformedKind, NativeAbiError, NativeAbiErrorCategory, ResourceKind,
    UnsupportedKind,
};
pub use handle::{
    HandleSlot, MAX_NATIVE_HANDLE_SLOTS, NativeHandle, NativeHandleRef, NativeHandleTable,
};
pub use model::{
    AbiImportRecord, BoundCallSlot, CapabilityRequirementRecord, ExecPhase, NativeBindingPlan,
    UserAbiKind,
};
pub use registry::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, INTERFACES, InterfaceSpec, OPERATIONS, ObjectInterface,
    OperationId, OperationSpec, PAGE_SIZE, REQUIREMENTS, RIGHTS, RequirementId, RequirementSpec,
    RightSpec, Rights, SubmissionMode, TargetArch, interface_spec, operation, operation_by_id,
    requirement, requirement_by_id, right_by_name,
};
pub use start_info::{
    InitialHandleRecord, RuntimeArrayInfo, StartInfoBuildError, StartInfoImage, StartInfoInput,
    build_start_info,
};

#[cfg(test)]
mod tests;
