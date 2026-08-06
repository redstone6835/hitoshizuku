use sha2::{Digest, Sha256};

use crate::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, AbiImportRecord, CapabilityRequirementRecord,
    IncompatibleKind, MalformedKind, NativeAbiError, NativeAbiPolicy, ObjectInterface, OperationId,
    RequirementId, Rights, UnsupportedKind, VmMapFlags, VmProtections, bind_native_abi, operation,
    requirement, status, wire,
};

#[derive(Clone, Copy)]
struct TestImport {
    slot: u32,
    operation_id: u32,
    required: bool,
    signature_hash: [u8; 32],
}

impl TestImport {
    fn known(slot: u32, operation_id: OperationId, required: bool) -> Self {
        Self {
            slot,
            operation_id: operation_id as u32,
            required,
            signature_hash: operation(operation_id)
                .expect("测试 operation 必须注册")
                .signature_hash,
        }
    }
}

impl AbiImportRecord for TestImport {
    fn slot(&self) -> u32 {
        self.slot
    }

    fn operation_id(&self) -> u32 {
        self.operation_id
    }

    fn required(&self) -> bool {
        self.required
    }

    fn signature_hash(&self) -> &[u8; 32] {
        &self.signature_hash
    }
}

#[derive(Clone, Copy)]
struct TestCapability {
    requirement_id: u32,
    object_interface: u16,
    required: bool,
    required_rights: u64,
}

impl CapabilityRequirementRecord for TestCapability {
    fn requirement_id(&self) -> u32 {
        self.requirement_id
    }

    fn object_interface(&self) -> u16 {
        self.object_interface
    }

    fn required(&self) -> bool {
        self.required
    }

    fn required_rights(&self) -> u64 {
        self.required_rights
    }
}

#[test]
fn operation_registry_preserves_ids_signatures_and_hashes() {
    let exit = operation(OperationId::ProcessExit).expect("PROCESS_EXIT 必须注册");
    assert_eq!(exit.id as u32, 1);
    assert_eq!(exit.interface, Some(ObjectInterface::Process));
    assert_eq!(exit.required_rights.bits(), 1 << 4);
    assert_eq!(
        exit.signature,
        "epoch=1;operation=1;object=1;args=u32,zero,zero,zero,zero;result=noreturn"
    );
    assert_eq!(
        exit.signature_hash,
        [
            0xa6, 0xc1, 0xfb, 0x70, 0xa1, 0x0c, 0x4b, 0x82, 0xa6, 0x32, 0xa3, 0x02, 0x75, 0xef,
            0x98, 0xd9, 0x62, 0x4e, 0x46, 0xee, 0x8c, 0x3b, 0x2e, 0xdf, 0x02, 0xac, 0xb4, 0xe9,
            0xef, 0x83, 0x44, 0x2b,
        ]
    );

    let write = operation(OperationId::StreamWrite).expect("STREAM_WRITE 必须注册");
    assert_eq!(write.id as u32, 6);
    assert_eq!(write.interface, Some(ObjectInterface::Stream));
    assert_eq!(write.required_rights.bits(), 1 << 1);
    assert_eq!(
        write.signature,
        "epoch=1;operation=6;object=3;args=user_const_ptr,u64,u32,zero,zero;result=u64"
    );
}

#[test]
fn every_canonical_signature_matches_its_frozen_hash() {
    for spec in crate::OPERATIONS {
        let actual: [u8; 32] = Sha256::digest(spec.signature.as_bytes()).into();
        assert_eq!(actual, spec.signature_hash, "{} signature hash", spec.name);
    }
}

#[test]
fn requirement_registry_limits_interface_and_granted_rights() {
    let stdout = requirement(RequirementId::Stdout).expect("STDOUT 必须注册");
    assert_eq!(stdout.id as u32, 4);
    assert_eq!(stdout.interface, ObjectInterface::Stream);
    assert_eq!(stdout.max_rights, Rights::WRITE | Rights::DUPLICATE);

    let clock = requirement(RequirementId::MonotonicClock).expect("时钟必须注册");
    assert_eq!(clock.id as u32, 6);
    assert_eq!(clock.interface, ObjectInterface::Clock);
    assert_eq!(clock.max_rights, Rights::READ);
}

#[test]
fn status_and_vm_flag_registries_preserve_wire_values() {
    assert_eq!(status::OK, 0x0000_0000);
    assert_eq!(status::CORE_INVALID_ARGUMENT, 0x0100_0001);
    assert_eq!(status::ABI_BAD_SLOT, 0x0200_0001);
    assert_eq!(status::HANDLE_STALE, 0x0300_0002);
    assert_eq!(status::SECURITY_RIGHTS_DENIED, 0x0400_0001);
    assert_eq!(status::IO_WOULD_BLOCK, 0x0500_0002);
    assert_eq!(status::VM_ADDRESS_CONFLICT, 0x0600_0002);
    assert_eq!(VmProtections::READ.bits(), 1);
    assert_eq!(VmProtections::WRITE.bits(), 2);
    assert_eq!(VmProtections::EXECUTE.bits(), 4);
    assert_eq!(VmMapFlags::FIXED.bits(), 1);
    assert_eq!(VmMapFlags::ZEROED.bits(), 2);
}

#[test]
fn native_startup_wire_layout_is_frozen() {
    assert_eq!(wire::START_INFO_SIZE, 192);
    assert_eq!(wire::INITIAL_HANDLE_SIZE, 32);
    assert_eq!(wire::start_info::MAGIC, 0x00);
    assert_eq!(wire::start_info::ABI_EPOCH, 0x10);
    assert_eq!(wire::start_info::INITIAL_HANDLE_OFFSET, 0x68);
    assert_eq!(wire::start_info::RANDOM_SEED, 0x70);
    assert_eq!(wire::start_info::RESERVED2, 0x98);
    assert_eq!(wire::initial_handle::REQUIREMENT_ID, 0x00);
    assert_eq!(wire::initial_handle::GRANTED_RIGHTS, 0x10);
    assert_eq!(wire::initial_handle::RESERVED, 0x18);
}

#[test]
fn required_operation_binds_without_soyo_types() {
    let imports = [TestImport::known(0, OperationId::ProcessExit, true)];
    let plan = bind_native_abi(
        ABI_FAMILY_MYGO_NATIVE,
        ABI_EPOCH,
        &imports,
        &[] as &[TestCapability],
        NativeAbiPolicy::for_kernel(),
    )
    .expect("required operation 应绑定");

    assert_eq!(plan.call_slots.len(), 1);
    assert_eq!(plan.call_slots[0].slot, 0);
    assert_eq!(plan.call_slots[0].operation, Some(OperationId::ProcessExit));
}

#[test]
fn binding_rejects_wrong_family_or_epoch() {
    assert_eq!(
        bind_native_abi(
            2,
            ABI_EPOCH,
            &[] as &[TestImport],
            &[] as &[TestCapability],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Unsupported(UnsupportedKind::AbiFamily(2)))
    );
    assert_eq!(
        bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            2,
            &[] as &[TestImport],
            &[] as &[TestCapability],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Incompatible(IncompatibleKind::AbiEpoch(2)))
    );
}

#[test]
fn optional_unknown_or_unimplemented_operation_stays_unbound() {
    let mut unknown = TestImport::known(0, OperationId::ProcessExit, false);
    unknown.operation_id = 100;
    let stream_read = TestImport::known(0, OperationId::StreamRead, false);

    for import in [unknown, stream_read] {
        let plan = bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            ABI_EPOCH,
            &[import],
            &[] as &[TestCapability],
            NativeAbiPolicy::for_kernel(),
        )
        .expect("optional operation 应产生未绑定 slot");
        assert_eq!(plan.call_slots[0].operation, None);
    }
}

#[test]
fn required_unknown_or_unimplemented_operation_is_incompatible() {
    let mut unknown = TestImport::known(0, OperationId::ProcessExit, true);
    unknown.operation_id = 100;
    let stream_read = TestImport::known(0, OperationId::StreamRead, true);

    for (import, operation_id) in [(unknown, 100), (stream_read, 5)] {
        assert_eq!(
            bind_native_abi(
                ABI_FAMILY_MYGO_NATIVE,
                ABI_EPOCH,
                &[import],
                &[] as &[TestCapability],
                NativeAbiPolicy::for_kernel(),
            ),
            Err(NativeAbiError::Incompatible(IncompatibleKind::Operation(
                operation_id
            )))
        );
    }
}

#[test]
fn known_optional_operation_still_checks_signature() {
    let mut import = TestImport::known(0, OperationId::StreamRead, false);
    import.signature_hash = [0; 32];

    assert_eq!(
        bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            ABI_EPOCH,
            &[import],
            &[] as &[TestCapability],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Incompatible(IncompatibleKind::Signature(5)))
    );
}

#[test]
fn malformed_slot_sequence_is_rejected_by_the_abi_boundary() {
    let imports = [TestImport::known(1, OperationId::ProcessExit, true)];

    assert_eq!(
        bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            ABI_EPOCH,
            &imports,
            &[] as &[TestCapability],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Malformed(MalformedKind::Import))
    );
}

#[test]
fn duplicate_operation_imports_are_rejected_by_the_abi_boundary() {
    let imports = [
        TestImport::known(0, OperationId::ProcessExit, true),
        TestImport::known(1, OperationId::ProcessExit, true),
    ];

    assert_eq!(
        bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            ABI_EPOCH,
            &imports,
            &[] as &[TestCapability],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Malformed(MalformedKind::Import))
    );
}

#[test]
fn duplicate_capability_requirements_are_rejected_by_the_abi_boundary() {
    let capability = TestCapability {
        requirement_id: RequirementId::Stdout as u32,
        object_interface: ObjectInterface::Stream as u16,
        required: true,
        required_rights: Rights::WRITE.bits(),
    };

    assert_eq!(
        bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            ABI_EPOCH,
            &[] as &[TestImport],
            &[capability, capability],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Malformed(MalformedKind::Capability))
    );
}

#[test]
fn capability_binding_checks_requirement_interface_and_rights() {
    let valid = TestCapability {
        requirement_id: RequirementId::Stdout as u32,
        object_interface: ObjectInterface::Stream as u16,
        required: true,
        required_rights: Rights::WRITE.bits(),
    };
    bind_native_abi(
        ABI_FAMILY_MYGO_NATIVE,
        ABI_EPOCH,
        &[] as &[TestImport],
        &[valid],
        NativeAbiPolicy::for_kernel(),
    )
    .expect("权限子集应通过");

    for capability in [
        TestCapability {
            object_interface: ObjectInterface::Clock as u16,
            ..valid
        },
        TestCapability {
            required_rights: Rights::READ.bits(),
            ..valid
        },
    ] {
        assert_eq!(
            bind_native_abi(
                ABI_FAMILY_MYGO_NATIVE,
                ABI_EPOCH,
                &[] as &[TestImport],
                &[capability],
                NativeAbiPolicy::for_kernel(),
            ),
            Err(NativeAbiError::Malformed(MalformedKind::Capability))
        );
    }
}

#[test]
fn unknown_requirement_preserves_required_optional_semantics() {
    let optional = TestCapability {
        requirement_id: 100,
        object_interface: 0x8000,
        required: false,
        required_rights: 0,
    };
    bind_native_abi(
        ABI_FAMILY_MYGO_NATIVE,
        ABI_EPOCH,
        &[] as &[TestImport],
        &[optional],
        NativeAbiPolicy::for_kernel(),
    )
    .expect("未知 optional requirement 应忽略");

    assert_eq!(
        bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            ABI_EPOCH,
            &[] as &[TestImport],
            &[TestCapability {
                required: true,
                ..optional
            }],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Unsupported(
            UnsupportedKind::RequiredRequirement(100)
        ))
    );
}
