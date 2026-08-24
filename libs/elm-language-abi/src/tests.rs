extern crate std;

use core::mem::{align_of, size_of};

use crate::*;

const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(11, 3);
const HANDLE: LanguageHandle = LanguageHandle {
    slot: 7,
    generation: 2,
};
const CAPABILITY_HANDLE: LanguageHandle = LanguageHandle {
    slot: 8,
    generation: 4,
};
const RESOURCE_HANDLE: LanguageHandle = LanguageHandle {
    slot: 9,
    generation: 5,
};

#[test]
fn fixed_layouts_are_architecture_independent() {
    assert_eq!(size_of::<LanguageHandle>(), 8);
    assert_eq!(size_of::<LanguageOwnerV1>(), 16);
    assert_eq!(size_of::<LanguageOwnedHandleV1>(), 24);
    assert_eq!(size_of::<LanguageBackendDescriptorV1>(), 80);
    assert_eq!(size_of::<LanguageInstanceDescriptorV1>(), 64);
    assert_eq!(size_of::<LanguageArtifactIdentityV2>(), 128);
    assert_eq!(size_of::<LanguageInstanceOpenRequestV2>(), 168);
    assert_eq!(size_of::<LanguageInstanceDescriptorV2>(), 192);
    assert_eq!(size_of::<LanguageRuntimeCatalogV1>(), 32);
    assert_eq!(size_of::<LanguageBackendRequestV1>(), 40);
    assert_eq!(size_of::<LanguageInstanceCloseRequestV1>(), 48);
    assert_eq!(size_of::<LanguageRequestV1>(), 248);
    assert_eq!(size_of::<LanguageDelegationPolicyV1>(), 32);
    assert_eq!(size_of::<LanguageRequestV2>(), LANGUAGE_MANAGED_FRAME_LEN);
    assert_eq!(
        size_of::<LanguageBackendWorkV2>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(size_of::<LanguagePollRequestV1>(), 40);
    assert_eq!(size_of::<LanguageCancelRequestV1>(), 48);
    assert_eq!(size_of::<LanguageBackendCancelWorkV1>(), 64);
    assert_eq!(size_of::<LanguageBackendCancelAckV1>(), 64);
    assert_eq!(size_of::<LanguageDrainRequestV1>(), 32);
    assert_eq!(size_of::<LanguageRequestSubmitResponseV1>(), 32);
    assert_eq!(
        size_of::<LanguagePollResponseV1>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(size_of::<LanguageDrainResponseV1>(), 32);
    assert_eq!(align_of::<LanguageRequestV1>(), 8);
    assert_eq!(align_of::<LanguagePollResponseV1>(), 8);
    assert_eq!(size_of::<LanguageCapabilityV1>(), 32);
    assert_eq!(size_of::<LanguageResourceHandleV1>(), 32);
    assert_eq!(size_of::<LanguageMmioMapPayloadV1>(), 32);
    assert_eq!(size_of::<LanguageMmioAccessPayloadV1>(), 32);
    assert_eq!(size_of::<LanguageDmaAllocatePayloadV1>(), 24);
    assert_eq!(size_of::<LanguageDmaSyncPayloadV1>(), 24);
    assert_eq!(size_of::<LanguageBufferLeasePayloadV1>(), 32);
    assert_eq!(
        size_of::<LanguageBufferIoPayloadV1>(),
        LANGUAGE_FRAME_PAYLOAD_LEN
    );
    assert_eq!(size_of::<LanguageIrqSubscribePayloadV1>(), 32);
    assert_eq!(size_of::<LanguageIrqPollPayloadV1>(), 16);
    assert_eq!(size_of::<LanguageIrqEventStateV1>(), 40);
    assert_eq!(
        size_of::<LanguageResourceRequestV1>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(
        size_of::<LanguageResourceResponseV1>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(
        size_of::<LanguageDelegatedResourceRequestV2>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(
        size_of::<LanguageKernelCallRequestV1>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(
        size_of::<LanguageKernelCallResponseV1>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(
        size_of::<LanguageDelegatedKernelCallRequestV2>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
}

#[test]
fn resource_capability_and_handles_validate_owner_and_rights() {
    let rights = LANGUAGE_CAPABILITY_MMIO_MAP | LANGUAGE_CAPABILITY_MMIO_READ;
    let capability = LanguageCapabilityV1::new(CAPABILITY_HANDLE, rights, OWNER);
    assert_eq!(capability.validate(), Ok(()));
    assert!(capability.grants(LANGUAGE_CAPABILITY_MMIO_READ));
    assert!(!capability.grants(LANGUAGE_CAPABILITY_MMIO_WRITE));
    assert_eq!(capability.validate_for_owner(OWNER), Ok(()));

    let mut invalid = capability;
    invalid.rights = 1 << 63;
    assert_eq!(invalid.validate(), Err(LanguageValidationError::Capability));

    let resource = LanguageResourceHandleV1::new(
        RESOURCE_HANDLE,
        LanguageResourceKind::Mmio,
        LANGUAGE_RESOURCE_FLAG_OWNED | LANGUAGE_RESOURCE_FLAG_READ,
        OWNER,
    );
    assert_eq!(resource.validate(), Ok(()));
    assert_eq!(resource.kind(), Some(LanguageResourceKind::Mmio));

    let mut invalid_kind = resource;
    invalid_kind.kind = 99;
    assert_eq!(
        invalid_kind.validate(),
        Err(LanguageValidationError::ResourceKind)
    );
}

#[test]
fn resource_payloads_reject_empty_overflow_and_unknown_modes() {
    let map = LanguageMmioMapPayloadV1 {
        physical_base: 0x1000,
        length: 0x1000,
        access_flags: LANGUAGE_MMIO_ACCESS_READ | LANGUAGE_MMIO_ACCESS_VOLATILE,
        cache_mode: LanguageMmioCacheMode::Device as u32,
        reserved: 0,
    };
    assert_eq!(map.validate(), Ok(()));

    let mut map_overflow = map;
    map_overflow.physical_base = u64::MAX;
    assert_eq!(map_overflow.validate(), Err(LanguageValidationError::Range));
    let mut map_mode = map;
    map_mode.cache_mode = 99;
    assert_eq!(map_mode.validate(), Err(LanguageValidationError::CacheMode));

    let access = LanguageMmioAccessPayloadV1 {
        offset: 8,
        value: 0,
        width: 4,
        flags: 0,
        reserved: 0,
    };
    assert_eq!(access.validate(), Ok(()));
    let mut unaligned = access;
    unaligned.offset = 2;
    assert_eq!(
        unaligned.validate(),
        Err(LanguageValidationError::Alignment)
    );

    let dma = LanguageDmaAllocatePayloadV1 {
        length: 4096,
        alignment: 4096,
        direction: LanguageDmaDirection::Bidirectional as u32,
        flags: LANGUAGE_DMA_FLAG_COHERENT,
        reserved: 0,
    };
    assert_eq!(dma.validate(), Ok(()));
    let mut bad_alignment = dma;
    bad_alignment.alignment = 3;
    assert_eq!(
        bad_alignment.validate(),
        Err(LanguageValidationError::Alignment)
    );
    let mut bad_direction = dma;
    bad_direction.direction = 0;
    assert_eq!(
        bad_direction.validate(),
        Err(LanguageValidationError::Direction)
    );

    let sync = LanguageDmaSyncPayloadV1 {
        offset: 16,
        length: 64,
        direction: LanguageDmaDirection::ToDevice as u32,
        reserved: 0,
    };
    assert_eq!(sync.validate(), Ok(()));
    let mut sync_overflow = sync;
    sync_overflow.offset = u64::MAX;
    assert_eq!(
        sync_overflow.validate(),
        Err(LanguageValidationError::Range)
    );

    let lease = LanguageBufferLeasePayloadV1 {
        buffer_handle: RESOURCE_HANDLE,
        offset: 0,
        length: 128,
        access_flags: LANGUAGE_BUFFER_LEASE_READ | LANGUAGE_BUFFER_LEASE_WRITE,
        reserved: 0,
    };
    assert_eq!(lease.validate(), Ok(()));
    let mut no_access = lease;
    no_access.access_flags = 0;
    assert_eq!(no_access.validate(), Err(LanguageValidationError::Access));

    let io = LanguageBufferIoPayloadV1::new(4, b"hello").unwrap();
    assert_eq!(io.data().unwrap(), b"hello");
    let mut io_overflow = io;
    io_overflow.offset = u64::MAX;
    assert_eq!(io_overflow.validate(), Err(LanguageValidationError::Range));
    let oversized = [0_u8; LanguageBufferIoPayloadV1::DATA_CAPACITY + 1];
    assert_eq!(
        LanguageBufferIoPayloadV1::new(0, &oversized),
        Err(LanguageValidationError::PayloadLength)
    );
}

#[test]
fn irq_payloads_are_bounded_and_never_describe_a_hardware_line() {
    let subscribe = LanguageIrqSubscribePayloadV1 {
        source_id: 0x42,
        max_pending: 32,
        flags: LANGUAGE_IRQ_SUBSCRIBE_FLAG_NONE,
        reserved0: 0,
        reserved1: 0,
    };
    assert_eq!(subscribe.validate(), Ok(()));

    let mut missing_source = subscribe;
    missing_source.source_id = 0;
    assert_eq!(
        missing_source.validate(),
        Err(LanguageValidationError::Identifier)
    );
    let mut unbounded = subscribe;
    unbounded.max_pending = LANGUAGE_IRQ_MAX_PENDING_LIMIT + 1;
    assert_eq!(unbounded.validate(), Err(LanguageValidationError::Capacity));

    assert!(!LanguageIrqPollPayloadV1::poll().is_take());
    assert!(LanguageIrqPollPayloadV1::take().is_take());
    let mut invalid_poll = LanguageIrqPollPayloadV1::poll();
    invalid_poll.flags = 1 << 31;
    assert_eq!(invalid_poll.validate(), Err(LanguageValidationError::Flags));

    let state = LanguageIrqEventStateV1 {
        source_id: subscribe.source_id,
        sequence: 35,
        pending: 32,
        overflow: 3,
        capacity: subscribe.max_pending,
        flags: LANGUAGE_IRQ_EVENT_FLAG_ACTIVE | LANGUAGE_IRQ_EVENT_FLAG_OVERFLOW,
        reserved: 0,
    };
    assert_eq!(state.validate(), Ok(()));
    let mut impossible_pending = state;
    impossible_pending.pending = state.capacity + 1;
    assert_eq!(
        impossible_pending.validate(),
        Err(LanguageValidationError::Capacity)
    );
    let mut missing_overflow_flag = state;
    missing_overflow_flag.flags &= !LANGUAGE_IRQ_EVENT_FLAG_OVERFLOW;
    assert_eq!(
        missing_overflow_flag.validate(),
        Err(LanguageValidationError::Flags)
    );
}

#[test]
fn resource_frames_bind_optional_handles_and_owner() {
    let payload = [0xa5; 32];
    let request = LanguageResourceRequestV1::new(
        OWNER,
        CAPABILITY_HANDLE,
        RESOURCE_HANDLE,
        41,
        LANGUAGE_RESOURCE_OPCODE_MMIO_READ,
        &payload,
    )
    .unwrap();
    assert_eq!(request.validate(), Ok(()));
    assert_eq!(request.validate_for_owner(OWNER), Ok(()));
    assert_eq!(request.payload().unwrap(), payload);

    let mut missing_flag = request;
    missing_flag.flags &= !LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_RESOURCE;
    assert_eq!(missing_flag.validate(), Err(LanguageValidationError::Flags));

    let resource = LanguageResourceHandleV1::new(
        RESOURCE_HANDLE,
        LanguageResourceKind::Mmio,
        LANGUAGE_RESOURCE_FLAG_OWNED | LANGUAGE_RESOURCE_FLAG_READ,
        OWNER,
    );
    let response = LanguageResourceResponseV1::with_resource(
        OWNER,
        41,
        LanguageRuntimeStatus::OK,
        resource,
        b"ok",
    )
    .unwrap();
    assert_eq!(response.validate(), Ok(()));
    assert_eq!(response.payload().unwrap(), b"ok");
    assert_eq!(response.validate_for_owner(OWNER), Ok(()));

    let mut bad_response = response;
    bad_response.resource_kind = 99;
    assert_eq!(
        bad_response.validate(),
        Err(LanguageValidationError::ResourceKind)
    );
}

#[test]
fn kernel_call_frames_validate_operation_identity_and_owner() {
    let request =
        LanguageKernelCallRequestV1::new(OWNER, CAPABILITY_HANDLE, 0x1234, 41, b"input").unwrap();
    assert_eq!(request.validate(), Ok(()));
    assert_eq!(request.validate_for_owner(OWNER), Ok(()));
    assert_eq!(request.input().unwrap(), b"input");

    let mut bad_flags = request;
    bad_flags.flags = 1 << 31;
    assert_eq!(bad_flags.validate(), Err(LanguageValidationError::Flags));
    let mut bad_operation = request;
    bad_operation.operation_id = 0;
    assert_eq!(
        bad_operation.validate(),
        Err(LanguageValidationError::Identifier)
    );

    let response =
        LanguageKernelCallResponseV1::new(OWNER, 0x1234, 41, LanguageRuntimeStatus::OK, b"output")
            .unwrap();
    assert_eq!(response.validate(), Ok(()));
    assert_eq!(response.validate_for_owner(OWNER), Ok(()));
    assert_eq!(response.output().unwrap(), b"output");
    let mut bad_owner = response;
    bad_owner.owner_generation += 1;
    assert_eq!(
        bad_owner.validate_for_owner(OWNER),
        Err(LanguageValidationError::Owner)
    );
}

#[test]
fn delegation_frames_are_scoped_tokenized_and_wire_stable() {
    let opcode_mask =
        language_delegation_resource_opcode_bit(LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE).unwrap();
    let policy = LanguageDelegationPolicyV1::new(
        LANGUAGE_DELEGATION_FLAG_RESOURCE | LANGUAGE_DELEGATION_FLAG_KERNEL_CALL,
        LANGUAGE_CAPABILITY_BUFFER_READ | LANGUAGE_CAPABILITY_BUFFER_WRITE,
        opcode_mask,
        0x1234,
    );
    assert_eq!(policy.validate(), Ok(()));
    assert!(policy.allows_resource(
        LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
        LANGUAGE_CAPABILITY_BUFFER_READ | LANGUAGE_CAPABILITY_BUFFER_WRITE,
    ));
    assert!(!policy.allows_resource(
        LANGUAGE_RESOURCE_OPCODE_MMIO_READ,
        LANGUAGE_CAPABILITY_MMIO_READ,
    ));
    assert!(policy.allows_kernel_operation(0x1234));
    assert!(!policy.allows_kernel_operation(0x1235));

    let request = LanguageRequestV2::new(OWNER, 2, HANDLE, 41, 7, policy, b"work").unwrap();
    let token = LanguageHandle::new(0x55, 0xaa).unwrap();
    let work = LanguageBackendWorkV2::from_request(&request, token).unwrap();
    assert_eq!(work.owner(), OWNER);
    assert_eq!(work.delegation_handle, token);
    assert_eq!(work.payload().unwrap(), b"work");

    let resource = LanguageResourceRequestV1::new(
        OWNER,
        CAPABILITY_HANDLE,
        LanguageHandle::INVALID,
        1,
        LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
        &64_u64.to_le_bytes(),
    )
    .unwrap();
    let delegated_resource =
        LanguageDelegatedResourceRequestV2::from_request(resource, token).unwrap();
    assert_eq!(delegated_resource.consumer_request(), resource);

    let kernel =
        LanguageKernelCallRequestV1::new(OWNER, CAPABILITY_HANDLE, 0x1234, 1, b"input").unwrap();
    let delegated_kernel =
        LanguageDelegatedKernelCallRequestV2::from_request(kernel, token).unwrap();
    assert_eq!(delegated_kernel.consumer_request(), kernel);

    for (wire_size, encoded_len) in [
        (
            <LanguageRequestV2 as LanguageWire>::WIRE_SIZE,
            encode(&request, &mut [0; LANGUAGE_MANAGED_FRAME_LEN]).unwrap(),
        ),
        (
            <LanguageBackendWorkV2 as LanguageWire>::WIRE_SIZE,
            encode(&work, &mut [0; LANGUAGE_MANAGED_FRAME_LEN]).unwrap(),
        ),
        (
            <LanguageDelegatedResourceRequestV2 as LanguageWire>::WIRE_SIZE,
            encode(&delegated_resource, &mut [0; LANGUAGE_MANAGED_FRAME_LEN]).unwrap(),
        ),
        (
            <LanguageDelegatedKernelCallRequestV2 as LanguageWire>::WIRE_SIZE,
            encode(&delegated_kernel, &mut [0; LANGUAGE_MANAGED_FRAME_LEN]).unwrap(),
        ),
    ] {
        assert_eq!(wire_size, LANGUAGE_MANAGED_FRAME_LEN);
        assert_eq!(encoded_len, wire_size);
    }
    let mut bytes = [0; LANGUAGE_MANAGED_FRAME_LEN];
    encode(&delegated_resource, &mut bytes).unwrap();
    assert_eq!(decode(&bytes), Ok(delegated_resource));
    encode(&delegated_kernel, &mut bytes).unwrap();
    assert_eq!(decode(&bytes), Ok(delegated_kernel));

    let mut empty = LanguageDelegationPolicyV1::new(0, 0, 0, 0);
    assert_eq!(empty.validate(), Err(LanguageValidationError::Flags));
    empty.flags = LANGUAGE_DELEGATION_FLAG_RESOURCE;
    assert_eq!(empty.validate(), Err(LanguageValidationError::Capability));
    let mut forged = delegated_resource;
    forged.delegation_handle = LanguageHandle::INVALID;
    assert_eq!(forged.validate(), Err(LanguageValidationError::Handle));
}

#[test]
fn typed_ids_and_handles_reject_zero() {
    assert!(LanguageId::new(0).is_none());
    assert!(BackendId::new(0).is_none());
    assert!(InstanceId::new(0).is_none());
    assert!(RequestId::new(0).is_none());
    assert!(LanguageHandle::new(0, 1).is_none());
    assert!(LanguageHandle::new(1, 0).is_none());

    let handle = LanguageHandle::new(17, 9).unwrap();
    assert_eq!(LanguageHandle::from_raw(handle.raw()), handle);
    assert!(LanguageOwnedHandleV1::new(handle, OWNER).is_valid());
    assert!(!LanguageOwnedHandleV1::new(handle, LanguageOwnerV1::new(0, 3)).is_valid());
    let owned = LanguageOwnedHandleV1::new(handle, OWNER);
    assert_eq!(owned.validate_for(OWNER), Ok(()));
    assert_eq!(
        owned.validate_for(LanguageOwnerV1::new(12, OWNER.generation)),
        Err(LanguageValidationError::Owner)
    );
    assert_eq!(
        owned.validate_for(LanguageOwnerV1::new(OWNER.cell_id, OWNER.generation + 1)),
        Err(LanguageValidationError::Owner)
    );
}

#[test]
fn backend_descriptor_validates_every_header_field() {
    let valid = LanguageBackendDescriptorV1::new(
        1,
        2,
        LanguageBackendFlags::ASYNC.bits() | LanguageBackendFlags::CANCEL.bits(),
        0x55,
        8,
        32,
        b"csharp.aot",
    )
    .unwrap();
    assert_eq!(valid.validate(), Ok(()));

    let mut value = valid;
    value.abi_version += 1;
    assert_eq!(value.validate(), Err(LanguageValidationError::AbiVersion));

    let mut value = valid;
    value.struct_size -= 1;
    assert_eq!(value.validate(), Err(LanguageValidationError::StructSize));

    let mut value = valid;
    value.flags = 1 << 31;
    assert_eq!(value.validate(), Err(LanguageValidationError::Flags));

    let mut value = valid;
    value.backend_id = 0;
    assert_eq!(value.validate(), Err(LanguageValidationError::Identifier));

    let mut value = valid;
    value.max_requests = 0;
    assert_eq!(value.validate(), Err(LanguageValidationError::Capacity));

    let mut value = valid;
    value.name[value.name_len as usize] = b'x';
    assert_eq!(value.validate(), Err(LanguageValidationError::Name));

    let mut value = valid;
    value.reserved1 = 1;
    assert_eq!(value.validate(), Err(LanguageValidationError::Reserved));
}

#[test]
fn catalog_and_instance_bind_capacity_and_owner() {
    let catalog = LanguageRuntimeCatalogV1::new(16, 128, 64);
    assert_eq!(catalog.validate(), Ok(()));
    assert_eq!(catalog.contract_count, LANGUAGE_RUNTIME_CONTRACT_COUNT);
    assert_eq!(
        catalog.max_inline_payload,
        LANGUAGE_FRAME_PAYLOAD_LEN as u32
    );

    let instance = LanguageInstanceDescriptorV1::new(1, 2, 3, OWNER, HANDLE);
    assert_eq!(instance.validate(), Ok(()));
    assert_eq!(instance.owner(), OWNER);

    let mut stale_owner = instance;
    stale_owner.owner_generation = 0;
    assert_eq!(stale_owner.validate(), Err(LanguageValidationError::Owner));

    let mut bad_handle = instance;
    bad_handle.handle.generation = 0;
    assert_eq!(bad_handle.validate(), Err(LanguageValidationError::Handle));

    let mut conflicting_state = instance;
    conflicting_state.flags =
        LanguageInstanceFlags::ACTIVE.bits() | LanguageInstanceFlags::DRAINING.bits();
    assert_eq!(
        conflicting_state.validate(),
        Err(LanguageValidationError::State)
    );
}

#[test]
fn v2_instance_binds_package_artifact_and_interface_identity() {
    let artifact = LanguageArtifactIdentityV2::new(17, 19, [1; 32], [2; 32], [3; 32]);
    assert_eq!(artifact.validate(), Ok(()));
    let request = LanguageInstanceOpenRequestV2::new(OWNER, 2, artifact);
    assert_eq!(request.validate_for_owner(OWNER), Ok(()));
    let v1 = LanguageInstanceDescriptorV1::new(1, 2, 3, OWNER, HANDLE);
    let descriptor = LanguageInstanceDescriptorV2::from_v1(v1, artifact);
    assert_eq!(descriptor.validate_for_owner(OWNER), Ok(()));
    assert_eq!(descriptor.artifact, artifact);

    let mut missing_digest = artifact;
    missing_digest.artifact_digest = [0; 32];
    assert_eq!(
        missing_digest.validate(),
        Err(LanguageValidationError::Identifier)
    );
}

#[test]
fn backend_and_instance_control_requests_bind_trusted_owner() {
    let backend = LanguageBackendRequestV1::new(OWNER, 2);
    assert_eq!(backend.validate_for_owner(OWNER), Ok(()));
    assert_eq!(
        backend.validate_for_owner(LanguageOwnerV1::new(OWNER.cell_id + 1, OWNER.generation)),
        Err(LanguageValidationError::Owner)
    );

    let close = LanguageInstanceCloseRequestV1::new(OWNER, 2, HANDLE);
    assert_eq!(close.validate_for_owner(OWNER), Ok(()));
    assert_eq!(
        close.validate_for_owner(LanguageOwnerV1::new(OWNER.cell_id, OWNER.generation + 1)),
        Err(LanguageValidationError::Owner)
    );
}

#[test]
fn request_constructor_is_bounded_and_strict() {
    let payload = [0xa5; LANGUAGE_FRAME_PAYLOAD_LEN];
    let request = LanguageRequestV1::new(11, 3, 2, HANDLE, 41, 7, &payload).unwrap();
    assert_eq!(request.validate(), Ok(()));
    assert_eq!(request.validate_for_owner(11, 3), Ok(()));
    assert_eq!(
        request.validate_for_owner(12, 3),
        Err(LanguageValidationError::Owner)
    );
    assert_eq!(
        request.validate_for_owner(11, 4),
        Err(LanguageValidationError::Owner)
    );
    assert_eq!(request.payload().unwrap(), payload);

    let oversized = [0; LANGUAGE_FRAME_PAYLOAD_LEN + 1];
    assert_eq!(
        LanguageRequestV1::new(11, 3, 2, HANDLE, 41, 7, &oversized),
        Err(LanguageValidationError::PayloadLength)
    );

    let mut bad = request;
    bad.flags = 1 << 7;
    assert_eq!(bad.validate(), Err(LanguageValidationError::Flags));

    let mut bad = request;
    bad.payload_len = LANGUAGE_FRAME_PAYLOAD_LEN as u16 + 1;
    assert_eq!(bad.validate(), Err(LanguageValidationError::PayloadLength));

    let mut bad = request;
    bad.reserved0 = 1;
    assert_eq!(bad.validate(), Err(LanguageValidationError::Reserved));
}

#[test]
fn request_state_machine_only_allows_documented_edges() {
    assert_eq!(LANGUAGE_REQUEST_OPCODE_REQUEST_RELEASE, 10);
    assert_eq!(LANGUAGE_REQUEST_OPCODE_BACKEND_NEXT, 11);
    assert_eq!(LANGUAGE_REQUEST_OPCODE_BACKEND_COMPLETE, 12);
    assert!(LanguageRequestState::Queued.can_transition_to(LanguageRequestState::Running));
    assert!(LanguageRequestState::Queued.can_transition_to(LanguageRequestState::Canceled));
    assert!(LanguageRequestState::Running.can_transition_to(LanguageRequestState::Completed));
    assert!(LanguageRequestState::Running.can_transition_to(LanguageRequestState::Failed));
    assert!(!LanguageRequestState::Queued.can_transition_to(LanguageRequestState::Completed));
    assert!(!LanguageRequestState::Completed.can_transition_to(LanguageRequestState::Running));
    assert!(LanguageRequestState::Expired.is_terminal());
    assert_eq!(LanguageRequestState::from_raw(0), None);
    assert_eq!(LanguageRequestState::from_raw(7), None);
}

#[test]
fn poll_cancel_and_drain_validate_owner_and_reserved_fields() {
    let poll = LanguagePollRequestV1::new(OWNER.cell_id, OWNER.generation, 19);
    assert_eq!(poll.validate(), Ok(()));
    let release: LanguageRequestReleaseV1 = poll;
    assert_eq!(
        release.validate_for_owner(OWNER.cell_id, OWNER.generation),
        Ok(())
    );
    assert_eq!(
        size_of::<LanguageRequestReleaseV1>(),
        size_of::<LanguagePollRequestV1>()
    );

    let cancel = LanguageCancelRequestV1::new(OWNER.cell_id, OWNER.generation, 19, 4);
    assert_eq!(cancel.validate(), Ok(()));

    let drain = LanguageDrainRequestV1::new(OWNER.cell_id, OWNER.generation);
    assert_eq!(drain.validate(), Ok(()));
    let drain_response = LanguageDrainResponseV1::new(1, 2, 3);
    assert_eq!(drain_response.validate(), Ok(()));

    let mut wrong_generation = poll;
    wrong_generation.owner_generation = 0;
    assert_eq!(
        wrong_generation.validate(),
        Err(LanguageValidationError::Owner)
    );

    let mut reserved = cancel;
    reserved.reserved1 = 1;
    assert_eq!(reserved.validate(), Err(LanguageValidationError::Reserved));
}

#[test]
fn poll_response_checks_result_and_unknown_state() {
    let response = LanguagePollResponseV1::pending(11, 3, 2, HANDLE, 19);
    assert_eq!(response.validate(), Ok(()));
    assert_eq!(response.result().unwrap(), &[]);

    let mut unknown = response;
    unknown.state = 999;
    assert_eq!(unknown.validate(), Err(LanguageValidationError::State));

    let mut too_long = response;
    too_long.result_len = LANGUAGE_FRAME_PAYLOAD_LEN as u16 + 1;
    assert_eq!(
        too_long.validate(),
        Err(LanguageValidationError::PayloadLength)
    );
}

#[test]
fn backend_work_and_complete_frames_reject_invalid_terminal_semantics() {
    let request = LanguageRequestV1::new(11, 3, 2, HANDLE, 41, 7, b"hello").unwrap();
    let work = LanguageBackendWorkV1::from_request(&request).unwrap();
    assert_eq!(work.validate(), Ok(()));

    let mut work_reserved = work;
    work_reserved.reserved1 = 1;
    assert_eq!(
        work_reserved.validate(),
        Err(LanguageValidationError::Reserved)
    );

    let complete = LanguageBackendCompleteRequestV1::new(
        11,
        3,
        2,
        HANDLE,
        41,
        LanguageRequestState::Completed,
        LanguageRuntimeStatus::OK,
        b"done",
    )
    .unwrap();
    assert_eq!(complete.validate(), Ok(()));

    let mut running = complete;
    running.state = LanguageRequestState::Running as u32;
    assert_eq!(running.validate(), Err(LanguageValidationError::State));

    let mut canceled = complete;
    canceled.state = LanguageRequestState::Canceled as u32;
    assert_eq!(canceled.validate(), Err(LanguageValidationError::State));

    let mut completed_with_error = complete;
    completed_with_error.status = LanguageRuntimeStatus::FAULT.raw();
    assert_eq!(
        completed_with_error.validate(),
        Err(LanguageValidationError::State)
    );

    let mut complete_reserved = complete;
    complete_reserved.reserved1 = 1;
    assert_eq!(
        complete_reserved.validate(),
        Err(LanguageValidationError::Reserved)
    );
}

#[test]
fn backend_cancel_notice_requires_matching_terminal_ack() {
    let notice = LanguageBackendCancelWorkV1::new(
        OWNER.cell_id,
        OWNER.generation,
        2,
        HANDLE,
        41,
        LANGUAGE_CANCEL_REASON_DRAIN,
        LanguageRequestState::Expired,
    );
    assert_eq!(notice.validate(), Ok(()));
    let backend_owner = LanguageOwnerV1::new(21, 4);
    let acknowledgement = LanguageBackendCancelAckV1::new(backend_owner, notice);
    assert_eq!(
        acknowledgement.validate_for_owner(backend_owner.cell_id, backend_owner.generation),
        Ok(())
    );

    let mut wrong_terminal = acknowledgement;
    wrong_terminal.terminal_state = LanguageRequestState::Completed as u32;
    assert_eq!(
        wrong_terminal.validate(),
        Err(LanguageValidationError::State)
    );
}

#[test]
fn submit_success_is_always_initially_queued() {
    let response = LanguageRequestSubmitResponseV1::queued(19);
    assert_eq!(response.validate(), Ok(()));

    let mut impossible = response;
    impossible.state = LanguageRequestState::Completed as u32;
    assert_eq!(impossible.validate(), Err(LanguageValidationError::State));
}

#[test]
fn validation_errors_have_stable_status_mapping() {
    assert_eq!(
        LanguageValidationError::AbiVersion.status(),
        LanguageRuntimeStatus::ABI_MISMATCH
    );
    assert_eq!(
        LanguageValidationError::Owner.status(),
        LanguageRuntimeStatus::OWNER_MISMATCH
    );
    assert!(LanguageRuntimeStatus::OK.is_ok());
    assert!(!LanguageRuntimeStatus::FAULT.is_ok());
}

#[test]
fn backend_modes_are_explicit_and_cancel_requires_async() {
    assert_eq!(
        LanguageBackendDescriptorV1::new(1, 2, 0, 0, 1, 1, b"invalid"),
        Err(LanguageValidationError::Flags)
    );
    assert_eq!(
        LanguageBackendDescriptorV1::new(
            1,
            2,
            LANGUAGE_BACKEND_FLAG_SYNC | LANGUAGE_BACKEND_FLAG_CANCEL,
            0,
            1,
            1,
            b"invalid",
        ),
        Err(LanguageValidationError::Flags)
    );
}

#[test]
fn every_v1_wire_structure_round_trips_without_host_layout_copy() {
    fn round_trip<T>(value: T)
    where
        T: LanguageWire + PartialEq + Copy + core::fmt::Debug,
    {
        let mut encoded = std::vec![0_u8; T::WIRE_SIZE];
        assert_eq!(value.encode_wire(&mut encoded), Ok(T::WIRE_SIZE));
        assert_eq!(T::decode_wire(&encoded), Ok(value));

        let mut larger = std::vec![0_u8; T::WIRE_SIZE + 4];
        assert_eq!(value.encode_wire(&mut larger), Ok(T::WIRE_SIZE));
        assert_eq!(T::decode_wire(&larger[..T::WIRE_SIZE]), Ok(value));
    }

    let backend = LanguageBackendDescriptorV1::new(
        1,
        2,
        LANGUAGE_BACKEND_FLAG_ASYNC | LANGUAGE_BACKEND_FLAG_CANCEL,
        0x55,
        8,
        32,
        b"test.backend",
    )
    .unwrap();
    let instance = LanguageInstanceDescriptorV1::new(1, 2, 3, OWNER, HANDLE);
    let artifact = LanguageArtifactIdentityV2::new(17, 19, [1; 32], [2; 32], [3; 32]);
    let open_v2 = LanguageInstanceOpenRequestV2::new(OWNER, 2, artifact);
    let instance_v2 = LanguageInstanceDescriptorV2::from_v1(instance, artifact);
    let catalog = LanguageRuntimeCatalogV1::new(16, 128, 64);
    let backend_request = LanguageBackendRequestV1::new(OWNER, 2);
    let next_request = LanguageBackendNextRequestV1::new(OWNER, 2);
    let close_request = LanguageInstanceCloseRequestV1::new(OWNER, 2, HANDLE);
    let request = LanguageRequestV1::new(11, 3, 2, HANDLE, 41, 7, b"hello").unwrap();
    let work = LanguageBackendWorkV1::from_request(&request).unwrap();
    let complete = LanguageBackendCompleteRequestV1::new(
        11,
        3,
        2,
        HANDLE,
        41,
        LanguageRequestState::Completed,
        LanguageRuntimeStatus::OK,
        b"done",
    )
    .unwrap();
    let cancel_notice = LanguageBackendCancelWorkV1::new(
        11,
        3,
        2,
        HANDLE,
        41,
        LANGUAGE_CANCEL_REASON_REQUESTED,
        LanguageRequestState::Canceled,
    );
    let cancel_ack = LanguageBackendCancelAckV1::new(OWNER, cancel_notice);
    let poll = LanguagePollRequestV1::new(11, 3, 41);
    let cancel = LanguageCancelRequestV1::new(11, 3, 41, 1);
    let drain = LanguageDrainRequestV1::new(11, 3);
    let drain_reply = LanguageDrainResponseV1::new(1, 2, 3);
    let submit_reply = LanguageRequestSubmitResponseV1::queued(41);
    let poll_reply = LanguagePollResponseV1::pending(11, 3, 2, HANDLE, 41);
    let capability = LanguageCapabilityV1::new(
        CAPABILITY_HANDLE,
        LANGUAGE_CAPABILITY_MMIO_MAP | LANGUAGE_CAPABILITY_MMIO_READ,
        OWNER,
    );
    let resource = LanguageResourceHandleV1::new(
        RESOURCE_HANDLE,
        LanguageResourceKind::Mmio,
        LANGUAGE_RESOURCE_FLAG_OWNED | LANGUAGE_RESOURCE_FLAG_READ,
        OWNER,
    );
    let mmio_map = LanguageMmioMapPayloadV1 {
        physical_base: 0x1000,
        length: 0x1000,
        access_flags: LANGUAGE_MMIO_ACCESS_READ,
        cache_mode: LanguageMmioCacheMode::Uncached as u32,
        reserved: 0,
    };
    let mmio_access = LanguageMmioAccessPayloadV1 {
        offset: 0,
        value: 0x55,
        width: 4,
        flags: 0,
        reserved: 0,
    };
    let dma_allocate = LanguageDmaAllocatePayloadV1 {
        length: 4096,
        alignment: 4096,
        direction: LanguageDmaDirection::ToDevice as u32,
        flags: 0,
        reserved: 0,
    };
    let dma_sync = LanguageDmaSyncPayloadV1 {
        offset: 0,
        length: 64,
        direction: LanguageDmaDirection::ToDevice as u32,
        reserved: 0,
    };
    let lease = LanguageBufferLeasePayloadV1 {
        buffer_handle: RESOURCE_HANDLE,
        offset: 0,
        length: 64,
        access_flags: LANGUAGE_BUFFER_LEASE_READ,
        reserved: 0,
    };
    let buffer_io = LanguageBufferIoPayloadV1::new(0, b"payload").unwrap();
    let irq_subscribe = LanguageIrqSubscribePayloadV1 {
        source_id: 0x42,
        max_pending: 32,
        flags: LANGUAGE_IRQ_SUBSCRIBE_FLAG_NONE,
        reserved0: 0,
        reserved1: 0,
    };
    let irq_poll = LanguageIrqPollPayloadV1::take();
    let irq_state = LanguageIrqEventStateV1 {
        source_id: irq_subscribe.source_id,
        sequence: 3,
        pending: 2,
        overflow: 1,
        capacity: irq_subscribe.max_pending,
        flags: LANGUAGE_IRQ_EVENT_FLAG_ACTIVE
            | LANGUAGE_IRQ_EVENT_FLAG_TAKEN
            | LANGUAGE_IRQ_EVENT_FLAG_OVERFLOW,
        reserved: 0,
    };
    let resource_request = LanguageResourceRequestV1::new(
        OWNER,
        CAPABILITY_HANDLE,
        RESOURCE_HANDLE,
        41,
        LANGUAGE_RESOURCE_OPCODE_MMIO_READ,
        b"request",
    )
    .unwrap();
    let resource_response = LanguageResourceResponseV1::with_resource(
        OWNER,
        41,
        LanguageRuntimeStatus::OK,
        resource,
        b"ok",
    )
    .unwrap();
    let kernel_request =
        LanguageKernelCallRequestV1::new(OWNER, CAPABILITY_HANDLE, 0x1234, 41, b"input").unwrap();
    let kernel_response =
        LanguageKernelCallResponseV1::new(OWNER, 0x1234, 41, LanguageRuntimeStatus::OK, b"output")
            .unwrap();

    round_trip(LanguageId::from_raw(1));
    round_trip(BackendId::from_raw(2));
    round_trip(InstanceId::from_raw(3));
    round_trip(RequestId::from_raw(41));
    round_trip(HANDLE);
    round_trip(OWNER);
    round_trip(LanguageOwnedHandleV1::new(HANDLE, OWNER));
    round_trip(backend);
    round_trip(instance);
    round_trip(artifact);
    round_trip(open_v2);
    round_trip(instance_v2);
    round_trip(catalog);
    round_trip(backend_request);
    round_trip(next_request);
    round_trip(close_request);
    round_trip(request);
    round_trip(work);
    round_trip(complete);
    round_trip(cancel_notice);
    round_trip(cancel_ack);
    round_trip(poll);
    round_trip(cancel);
    round_trip(drain);
    round_trip(drain_reply);
    round_trip(submit_reply);
    round_trip(poll_reply);
    round_trip(capability);
    round_trip(resource);
    round_trip(mmio_map);
    round_trip(mmio_access);
    round_trip(dma_allocate);
    round_trip(dma_sync);
    round_trip(lease);
    round_trip(buffer_io);
    round_trip(irq_subscribe);
    round_trip(irq_poll);
    round_trip(irq_state);
    round_trip(resource_request);
    round_trip(resource_response);
    round_trip(kernel_request);
    round_trip(kernel_response);
}

#[test]
fn wire_rejects_short_long_and_semantically_malformed_input() {
    let request = LanguageRequestV1::new(11, 3, 2, HANDLE, 41, 7, b"payload").unwrap();
    let mut encoded = [0_u8; <LanguageRequestV1 as LanguageWire>::WIRE_SIZE];
    assert_eq!(encode(&request, &mut encoded), Ok(encoded.len()));
    assert_eq!(decode::<LanguageRequestV1>(&encoded), Ok(request));

    assert!(matches!(
        LanguageRequestV1::decode_wire(&encoded[..encoded.len() - 1]),
        Err(LanguageWireError::LengthMismatch { .. })
    ));
    let mut long = [0_u8; <LanguageRequestV1 as LanguageWire>::WIRE_SIZE + 1];
    long[..encoded.len()].copy_from_slice(&encoded);
    assert!(matches!(
        LanguageRequestV1::decode_wire(&long),
        Err(LanguageWireError::LengthMismatch { .. })
    ));
    let mut too_small = [0_u8; <LanguageRequestV1 as LanguageWire>::WIRE_SIZE - 1];
    assert!(matches!(
        request.encode_wire(&mut too_small),
        Err(LanguageWireError::OutputTooSmall { .. })
    ));

    let backend =
        LanguageBackendDescriptorV1::new(1, 2, LANGUAGE_BACKEND_FLAG_ASYNC, 0, 1, 1, b"x").unwrap();
    let mut backend_wire = [0_u8; <LanguageBackendDescriptorV1 as LanguageWire>::WIRE_SIZE];
    backend.encode_wire(&mut backend_wire).unwrap();
    backend_wire[backend_wire.len() - 1] = 1;
    assert_eq!(
        LanguageBackendDescriptorV1::decode_wire(&backend_wire),
        Err(LanguageWireError::Invalid(
            LanguageValidationError::Reserved
        ))
    );

    let kernel =
        LanguageKernelCallRequestV1::new(OWNER, CAPABILITY_HANDLE, 0x1234, 41, b"input").unwrap();
    let mut kernel_wire = [0_u8; <LanguageKernelCallRequestV1 as LanguageWire>::WIRE_SIZE];
    kernel.encode_wire(&mut kernel_wire).unwrap();
    kernel_wire[244] = 1;
    assert_eq!(
        LanguageKernelCallRequestV1::decode_wire(&kernel_wire),
        Err(LanguageWireError::Invalid(
            LanguageValidationError::Reserved
        ))
    );

    let irq = LanguageIrqSubscribePayloadV1 {
        source_id: 1,
        max_pending: 8,
        flags: 0,
        reserved0: 0,
        reserved1: 0,
    };
    let mut irq_wire = [0_u8; <LanguageIrqSubscribePayloadV1 as LanguageWire>::WIRE_SIZE];
    irq.encode_wire(&mut irq_wire).unwrap();
    irq_wire[24] = 1;
    assert_eq!(
        LanguageIrqSubscribePayloadV1::decode_wire(&irq_wire),
        Err(LanguageWireError::Invalid(
            LanguageValidationError::Reserved
        ))
    );
}
