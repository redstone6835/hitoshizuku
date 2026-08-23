extern crate std;

use core::mem::{align_of, size_of};

use crate::*;

const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(11, 3);
const HANDLE: LanguageHandle = LanguageHandle {
    slot: 7,
    generation: 2,
};

#[test]
fn fixed_layouts_are_architecture_independent() {
    assert_eq!(size_of::<LanguageHandle>(), 8);
    assert_eq!(size_of::<LanguageOwnerV1>(), 16);
    assert_eq!(size_of::<LanguageOwnedHandleV1>(), 24);
    assert_eq!(size_of::<LanguageBackendDescriptorV1>(), 80);
    assert_eq!(size_of::<LanguageInstanceDescriptorV1>(), 64);
    assert_eq!(size_of::<LanguageRuntimeCatalogV1>(), 32);
    assert_eq!(size_of::<LanguageBackendRequestV1>(), 40);
    assert_eq!(size_of::<LanguageInstanceCloseRequestV1>(), 48);
    assert_eq!(size_of::<LanguageRequestV1>(), 248);
    assert_eq!(size_of::<LanguagePollRequestV1>(), 40);
    assert_eq!(size_of::<LanguageCancelRequestV1>(), 48);
    assert_eq!(size_of::<LanguageDrainRequestV1>(), 32);
    assert_eq!(size_of::<LanguageRequestSubmitResponseV1>(), 32);
    assert_eq!(
        size_of::<LanguagePollResponseV1>(),
        LANGUAGE_MANAGED_FRAME_LEN
    );
    assert_eq!(size_of::<LanguageDrainResponseV1>(), 32);
    assert_eq!(align_of::<LanguageRequestV1>(), 8);
    assert_eq!(align_of::<LanguagePollResponseV1>(), 8);
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
    let poll = LanguagePollRequestV1::new(11, 3, 41);
    let cancel = LanguageCancelRequestV1::new(11, 3, 41, 1);
    let drain = LanguageDrainRequestV1::new(11, 3);
    let drain_reply = LanguageDrainResponseV1::new(1, 2, 3);
    let submit_reply = LanguageRequestSubmitResponseV1::queued(41);
    let poll_reply = LanguagePollResponseV1::pending(11, 3, 2, HANDLE, 41);

    round_trip(LanguageId::from_raw(1));
    round_trip(BackendId::from_raw(2));
    round_trip(InstanceId::from_raw(3));
    round_trip(RequestId::from_raw(41));
    round_trip(HANDLE);
    round_trip(OWNER);
    round_trip(LanguageOwnedHandleV1::new(HANDLE, OWNER));
    round_trip(backend);
    round_trip(instance);
    round_trip(catalog);
    round_trip(backend_request);
    round_trip(next_request);
    round_trip(close_request);
    round_trip(request);
    round_trip(work);
    round_trip(complete);
    round_trip(poll);
    round_trip(cancel);
    round_trip(drain);
    round_trip(drain_reply);
    round_trip(submit_reply);
    round_trip(poll_reply);
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
}
