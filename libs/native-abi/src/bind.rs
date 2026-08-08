//! ABI family、epoch、operation 与 capability 的格式无关绑定。

use alloc::vec::Vec;

use crate::error::{
    IncompatibleKind, MalformedKind, NativeAbiError, ResourceKind, UnsupportedKind,
};
use crate::model::{
    AbiImportRecord, BoundCallSlot, CapabilityRequirementRecord, NativeBindingPlan,
};
use crate::registry::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, ObjectInterface, OperationId, Rights, operation_by_id,
    requirement_by_id,
};

const KERNEL_SUPPORTED_OPERATIONS: &[OperationId] = &[
    OperationId::ProcessExit,
    OperationId::HandleClose,
    OperationId::HandleDuplicate,
    OperationId::HandleRestrict,
    OperationId::StreamRead,
    OperationId::StreamWrite,
    OperationId::ClockRead,
    OperationId::MemoryAllocate,
    OperationId::MemoryFree,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeAbiPolicy {
    pub abi_family: u16,
    pub abi_epoch: u16,
    pub supported_operations: Option<&'static [OperationId]>,
}

impl NativeAbiPolicy {
    pub const fn for_kernel() -> Self {
        Self {
            abi_family: ABI_FAMILY_MYGO_NATIVE,
            abi_epoch: ABI_EPOCH,
            supported_operations: Some(KERNEL_SUPPORTED_OPERATIONS),
        }
    }

    pub const fn for_host() -> Self {
        Self {
            abi_family: ABI_FAMILY_MYGO_NATIVE,
            abi_epoch: ABI_EPOCH,
            supported_operations: None,
        }
    }

    fn supports_operation(self, operation: OperationId) -> bool {
        self.supported_operations
            .is_none_or(|operations| operations.contains(&operation))
    }
}

pub fn bind_native_abi<I, C>(
    abi_family: u16,
    abi_epoch: u16,
    imports: &[I],
    capabilities: &[C],
    policy: NativeAbiPolicy,
) -> Result<NativeBindingPlan, NativeAbiError>
where
    I: AbiImportRecord,
    C: CapabilityRequirementRecord,
{
    if abi_family != policy.abi_family {
        return Err(NativeAbiError::Unsupported(UnsupportedKind::AbiFamily(
            abi_family,
        )));
    }
    if abi_epoch != policy.abi_epoch {
        return Err(NativeAbiError::Incompatible(IncompatibleKind::AbiEpoch(
            abi_epoch,
        )));
    }

    let mut call_slots = Vec::new();
    call_slots
        .try_reserve_exact(imports.len())
        .map_err(|_| NativeAbiError::ResourceExhausted(ResourceKind::CallSlots))?;
    for (index, import) in imports.iter().enumerate() {
        if import.slot() != index as u32
            || import.operation_id() == 0
            || import.operation_id() == u32::MAX
        {
            return Err(NativeAbiError::Malformed(MalformedKind::Import));
        }
        if imports[..index]
            .iter()
            .any(|previous| previous.operation_id() == import.operation_id())
        {
            return Err(NativeAbiError::Malformed(MalformedKind::Import));
        }
        let Some(spec) = operation_by_id(import.operation_id()) else {
            if import.required() {
                return Err(NativeAbiError::Incompatible(IncompatibleKind::Operation(
                    import.operation_id(),
                )));
            }
            call_slots.push(unbound_slot(import.slot()));
            continue;
        };
        if import.signature_hash() != &spec.signature_hash {
            return Err(NativeAbiError::Incompatible(IncompatibleKind::Signature(
                import.operation_id(),
            )));
        }
        if !policy.supports_operation(spec.id) {
            if import.required() {
                return Err(NativeAbiError::Incompatible(IncompatibleKind::Operation(
                    import.operation_id(),
                )));
            }
            call_slots.push(unbound_slot(import.slot()));
            continue;
        }
        call_slots.push(BoundCallSlot {
            slot: import.slot(),
            operation: Some(spec.id),
            interface: spec.interface,
            required_rights: spec.required_rights,
        });
    }

    for (index, capability) in capabilities.iter().enumerate() {
        if capability.requirement_id() == 0
            || capability.requirement_id() == u32::MAX
            || capability.object_interface() == 0
            || capability.object_interface() == u16::MAX
        {
            return Err(NativeAbiError::Malformed(MalformedKind::Capability));
        }
        if capabilities[..index]
            .iter()
            .any(|previous| previous.requirement_id() == capability.requirement_id())
        {
            return Err(NativeAbiError::Malformed(MalformedKind::Capability));
        }
        let Some(spec) = requirement_by_id(capability.requirement_id()) else {
            if capability.required() {
                return Err(NativeAbiError::Unsupported(
                    UnsupportedKind::RequiredRequirement(capability.requirement_id()),
                ));
            }
            continue;
        };
        if capability.object_interface() != spec.interface as u16
            || !Rights::from_bits(capability.required_rights()).is_subset_of(spec.max_rights)
        {
            return Err(NativeAbiError::Malformed(MalformedKind::Capability));
        }
    }

    Ok(NativeBindingPlan { call_slots })
}

const fn unbound_slot(slot: u32) -> BoundCallSlot {
    BoundCallSlot {
        slot,
        operation: None,
        interface: None::<ObjectInterface>,
        required_rights: Rights::NONE,
    }
}
