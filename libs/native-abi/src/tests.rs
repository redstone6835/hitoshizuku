use sha2::{Digest, Sha256};

use crate::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, AbiImportRecord, CapabilityRequirementRecord,
    IncompatibleKind, InitialHandleRecord, MalformedKind, NativeAbiError, NativeAbiPolicy,
    NativeHandle, ObjectInterface, OperationId, RequirementId, Rights, StartInfoBuildError,
    StartInfoInput, TargetArch, UnsupportedKind, VmMapFlags, VmProtections, bind_native_abi,
    build_start_info, operation, requirement, status, wire,
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
    assert_eq!(RequirementId::from_raw(4), Some(RequirementId::Stdout));
    assert_eq!(RequirementId::from_raw(0), None);
    assert_eq!(RequirementId::from_raw(7), None);
}

#[test]
fn status_and_vm_flag_registries_preserve_wire_values() {
    assert_eq!(status::OK, 0x0000_0000);
    assert_eq!(status::CORE_INVALID_ARGUMENT, 0x0100_0001);
    assert_eq!(status::ABI_BAD_SLOT, 0x0200_0001);
    assert_eq!(status::HANDLE_STALE, 0x0300_0002);
    assert_eq!(status::SECURITY_RIGHTS_DENIED, 0x0400_0001);
    assert_eq!(status::IO_WOULD_BLOCK, 0x0500_0002);
    assert_eq!(status::IO_CLOSED, 0x0500_0003);
    assert_eq!(status::IO_ERROR, 0x0500_0004);
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
fn start_info_builder_preserves_bytes_and_emits_canonical_layout() {
    let argv = alloc::vec![b"prog".to_vec(), alloc::vec![0xff, b'a', b'r', b'g']];
    let env = alloc::vec![alloc::vec![], b"A=B".to_vec()];
    let handles = [
        InitialHandleRecord {
            requirement_id: RequirementId::SelfProcess,
            object_interface: ObjectInterface::Process,
            handle: NativeHandle::from_parts(1, 1),
            granted_rights: Rights::TERMINATE_SELF,
        },
        InitialHandleRecord {
            requirement_id: RequirementId::Stdout,
            object_interface: ObjectInterface::Stream,
            handle: NativeHandle::from_parts(2, 3),
            granted_rights: Rights::WRITE,
        },
    ];
    let random_seed = core::array::from_fn(|index| index as u8 + 1);

    let image = build_start_info(StartInfoInput {
        target_arch: TargetArch::Riscv64,
        enabled_features: 0x11,
        image_base: 0x4000_0000,
        initial_tls_base: 0x7fff_0000,
        initial_tls_size: 0x1000,
        initial_thread_pointer: 0x7fff_0000,
        argv: &argv,
        env: &env,
        initial_handles: &handles,
        call_slot_count: 3,
        random_seed,
        runtime_flags: 1,
        max_size: 4096,
    })
    .expect("合法 StartInfo 应完成编码");
    let bytes = image.as_bytes();

    assert_eq!(bytes.len(), 304);
    assert_eq!(&bytes[0x00..0x04], b"syst");
    assert_eq!(u16::from_le_bytes(bytes[0x04..0x06].try_into().unwrap()), 1);
    assert_eq!(
        u16::from_le_bytes(bytes[0x06..0x08].try_into().unwrap()),
        192
    );
    assert_eq!(
        u32::from_le_bytes(bytes[0x08..0x0c].try_into().unwrap()),
        304
    );
    assert_eq!(u16::from_le_bytes(bytes[0x10..0x12].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(bytes[0x12..0x14].try_into().unwrap()), 1);
    assert_eq!(
        u64::from_le_bytes(bytes[0x18..0x20].try_into().unwrap()),
        0x11
    );
    assert_eq!(
        u64::from_le_bytes(bytes[0x20..0x28].try_into().unwrap()),
        0x4000_0000
    );
    assert_eq!(
        u64::from_le_bytes(bytes[0x30..0x38].try_into().unwrap()),
        0x7fff_0000
    );
    assert_eq!(u32::from_le_bytes(bytes[0x48..0x4c].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(bytes[0x4c..0x50].try_into().unwrap()), 2);
    assert_eq!(
        u32::from_le_bytes(bytes[0x50..0x54].try_into().unwrap()),
        192
    );
    assert_eq!(
        u32::from_le_bytes(bytes[0x54..0x58].try_into().unwrap()),
        208
    );
    assert_eq!(
        u32::from_le_bytes(bytes[0x58..0x5c].try_into().unwrap()),
        288
    );
    assert_eq!(
        u32::from_le_bytes(bytes[0x5c..0x60].try_into().unwrap()),
        15
    );
    assert_eq!(u32::from_le_bytes(bytes[0x60..0x64].try_into().unwrap()), 2);
    assert_eq!(
        u32::from_le_bytes(bytes[0x68..0x6c].try_into().unwrap()),
        224
    );
    assert_eq!(u32::from_le_bytes(bytes[0x6c..0x70].try_into().unwrap()), 3);
    assert_eq!(&bytes[0x70..0x90], &random_seed);

    assert_eq!(&bytes[288..293], b"prog\0");
    assert_eq!(&bytes[293..298], &[0xff, b'a', b'r', b'g', 0]);
    assert_eq!(&bytes[298..303], b"\0A=B\0");
    assert_eq!(bytes[303], 0);
    assert_eq!(u32::from_le_bytes(bytes[192..196].try_into().unwrap()), 288);
    assert_eq!(u32::from_le_bytes(bytes[196..200].try_into().unwrap()), 4);
    assert_eq!(u32::from_le_bytes(bytes[200..204].try_into().unwrap()), 293);
    assert_eq!(u32::from_le_bytes(bytes[204..208].try_into().unwrap()), 4);
    assert_eq!(u32::from_le_bytes(bytes[208..212].try_into().unwrap()), 298);
    assert_eq!(u32::from_le_bytes(bytes[212..216].try_into().unwrap()), 0);

    assert_eq!(u32::from_le_bytes(bytes[224..228].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(bytes[228..230].try_into().unwrap()), 1);
    assert_eq!(
        u64::from_le_bytes(bytes[232..240].try_into().unwrap()),
        0x0000_0001_0000_0001
    );
    assert_eq!(
        u64::from_le_bytes(bytes[240..248].try_into().unwrap()),
        1 << 4
    );
    assert_eq!(u32::from_le_bytes(bytes[256..260].try_into().unwrap()), 4);
    assert_eq!(
        u64::from_le_bytes(bytes[264..272].try_into().unwrap()),
        0x0000_0002_0000_0003
    );
    assert_eq!(
        u64::from_le_bytes(bytes[272..280].try_into().unwrap()),
        1 << 1
    );
}

#[test]
fn start_info_builder_rejects_oversize_or_noncanonical_input() {
    let argv = alloc::vec![alloc::vec![b'x'; 193]];
    let no_handles = [];
    let input = StartInfoInput {
        target_arch: TargetArch::LoongArch64,
        enabled_features: 0,
        image_base: 0x4000_0000,
        initial_tls_base: 0,
        initial_tls_size: 0,
        initial_thread_pointer: 0,
        argv: &argv,
        env: &[],
        initial_handles: &no_handles,
        call_slot_count: 0,
        random_seed: [1; 32],
        runtime_flags: 0,
        max_size: 192,
    };
    assert_eq!(build_start_info(input), Err(StartInfoBuildError::TooLarge));

    let with_nul = alloc::vec![b"bad\0arg".to_vec()];
    assert_eq!(
        build_start_info(StartInfoInput {
            argv: &with_nul,
            max_size: 4096,
            ..input
        }),
        Err(StartInfoBuildError::InvalidInput)
    );

    let duplicate = [
        InitialHandleRecord {
            requirement_id: RequirementId::Stdout,
            object_interface: ObjectInterface::Stream,
            handle: NativeHandle::from_parts(1, 1),
            granted_rights: Rights::WRITE,
        },
        InitialHandleRecord {
            requirement_id: RequirementId::Stdout,
            object_interface: ObjectInterface::Stream,
            handle: NativeHandle::from_parts(1, 2),
            granted_rights: Rights::WRITE,
        },
    ];
    assert_eq!(
        build_start_info(StartInfoInput {
            argv: &[],
            initial_handles: &duplicate,
            max_size: 4096,
            ..input
        }),
        Err(StartInfoBuildError::InvalidInput)
    );
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
