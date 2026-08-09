use sha2::{Digest, Sha256};

use crate::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, AbiImportRecord, CapabilityRequirementRecord,
    IncompatibleKind, InitialHandleRecord, MalformedKind, NativeAbiError, NativeAbiPolicy,
    NativeHandle, ObjectInterface, OperationId, RequirementId, Rights, RuntimeArrayInfo,
    StartInfoBuildError, StartInfoInput, TargetArch, UnsupportedKind, bind_native_abi,
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
    let exit = operation(OperationId::ProcessExit).expect("process.exit 必须注册");
    assert_eq!(exit.id as u32, 1);
    assert_eq!(exit.interface, Some(ObjectInterface::Process));
    assert_eq!(exit.required_rights.bits(), 1 << 5);
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

    let write = operation(OperationId::StreamWrite).expect("stream.write 必须注册");
    assert_eq!(write.id as u32, 6);
    assert_eq!(write.interface, Some(ObjectInterface::Stream));
    assert_eq!(write.required_rights.bits(), 1 << 1);
    assert_eq!(
        write.signature,
        "epoch=1;operation=6;object=3;args=user_const_ptr,u64,zero,zero,zero;result=u64"
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
fn current_thread_control_has_direct_operations() {
    let exit = crate::OPERATIONS
        .iter()
        .find(|spec| spec.name == "thread.exit")
        .expect("当前线程退出必须有独立 operation");
    assert_eq!(exit.interface, Some(ObjectInterface::Process));
    assert_eq!(exit.required_rights, Rights::NONE);
    assert!(exit.signature.ends_with("result=noreturn"));

    let yield_now = crate::OPERATIONS
        .iter()
        .find(|spec| spec.name == "thread.yield")
        .expect("主动让出调度器必须有独立 operation");
    assert_eq!(yield_now.interface, Some(ObjectInterface::Process));
    assert_eq!(yield_now.required_rights, Rights::NONE);
    assert!(yield_now.signature.ends_with("result=status"));
}

#[test]
fn memory_revocation_has_a_distinct_operation_and_statuses() {
    let revoke = crate::OPERATIONS
        .iter()
        .find(|spec| spec.name == "memory.revoke")
        .expect("MemoryObject 必须支持显式撤销");
    assert_eq!(revoke.interface, Some(ObjectInterface::MemoryObject));
    assert_eq!(revoke.required_rights, Rights::MODIFY);
    assert!(revoke.signature.ends_with("result=u64"));

    for name in ["memory.revoked", "memory.poisoned"] {
        assert!(
            crate::status::STATUS_CODES
                .iter()
                .any(|status| status.name == name),
            "缺少稳定状态 {name}"
        );
    }
}

#[test]
fn memory_statistics_is_a_read_only_snapshot() {
    let statistics =
        operation(OperationId::MemoryStatistics).expect("MemoryObject 必须提供只读统计快照");
    assert_eq!(statistics.id as u32, 70);
    assert_eq!(statistics.name, "memory.statistics");
    assert_eq!(statistics.interface, Some(ObjectInterface::MemoryObject));
    assert_eq!(statistics.required_rights, Rights::INSPECT);
    assert_eq!(statistics.submission(), crate::SubmissionMode::DirectOnly);
    assert_eq!(wire::MEMORY_STATISTICS_SIZE, 80);
    assert_eq!(
        core::mem::offset_of!(wire::MemoryStatistics, shared_resident_mappings),
        24
    );
    assert_eq!(
        core::mem::offset_of!(wire::MemoryStatistics, writeback_operations),
        64
    );
}

#[test]
fn shared_ring_state_has_stable_layout_and_wrapping_counts() {
    assert_eq!(core::mem::size_of::<wire::RingSharedState>(), 64);
    assert_eq!(core::mem::align_of::<wire::RingSharedState>(), 8);
    assert_eq!(core::mem::offset_of!(wire::RingSharedState, magic), 0x00);
    assert_eq!(core::mem::offset_of!(wire::RingSharedState, entries), 0x08);
    assert_eq!(core::mem::offset_of!(wire::RingSharedState, sq_head), 0x10);
    assert_eq!(core::mem::offset_of!(wire::RingSharedState, sq_tail), 0x14);
    assert_eq!(core::mem::offset_of!(wire::RingSharedState, cq_head), 0x18);
    assert_eq!(core::mem::offset_of!(wire::RingSharedState, cq_tail), 0x1c);
    assert_eq!(
        core::mem::offset_of!(wire::RingSharedState, sq_offset),
        0x20
    );
    assert_eq!(
        core::mem::offset_of!(wire::RingSharedState, cq_offset),
        0x28
    );
    assert_eq!(
        core::mem::offset_of!(wire::RingSharedState, generation),
        0x30
    );
    assert_eq!(core::mem::offset_of!(wire::RingSharedState, reserved), 0x38);

    assert_eq!(wire::ring_queue_len(4, 4, 8), Some(0));
    assert_eq!(wire::ring_queue_len(u32::MAX - 1, 1, 8), Some(3));
    assert_eq!(wire::ring_queue_len(10, 18, 8), Some(8));
    assert_eq!(wire::ring_queue_len(10, 19, 8), None);
}

#[test]
fn submission_modes_distinguish_scalar_memory_and_control_operations() {
    assert_eq!(
        operation(OperationId::ClockRead).unwrap().submission(),
        crate::SubmissionMode::Inline
    );
    assert_eq!(
        operation(OperationId::StreamRead).unwrap().submission(),
        crate::SubmissionMode::MemoryRegion
    );
    assert_eq!(
        operation(OperationId::ProcessExit).unwrap().submission(),
        crate::SubmissionMode::DirectOnly
    );
}

#[test]
fn requirement_registry_limits_interface_and_granted_rights() {
    let stdout = requirement(RequirementId::Stdout).expect("STDOUT 必须注册");
    assert_eq!(stdout.id as u32, 4);
    assert_eq!(stdout.interface, ObjectInterface::Stream);
    assert_eq!(
        stdout.max_rights,
        Rights::WRITE | Rights::DUPLICATE | Rights::OBSERVE
    );

    let clock = requirement(RequirementId::MonotonicClock).expect("时钟必须注册");
    assert_eq!(clock.id as u32, 6);
    assert_eq!(clock.interface, ObjectInterface::Clock);
    assert_eq!(clock.max_rights, Rights::READ);
    assert_eq!(RequirementId::from_raw(4), Some(RequirementId::Stdout));
    assert_eq!(RequirementId::from_raw(0), None);
    assert_eq!(RequirementId::from_raw(10), None);
    assert_eq!(crate::right_by_name("write").unwrap().right, Rights::WRITE);
    assert!(crate::right_by_name("WRITE").is_none());
}

#[test]
fn service_channel_requirement_only_grants_channel_messaging_rights() {
    let service = requirement(RequirementId::ServiceChannel).expect("服务通道必须注册");
    assert_eq!(service.id as u32, 9);
    assert_eq!(service.name, "service_channel");
    assert_eq!(service.interface, ObjectInterface::Channel);
    assert_eq!(
        service.max_rights,
        Rights::SEND | Rights::RECEIVE | Rights::DUPLICATE | Rights::OBSERVE
    );
    assert_eq!(
        RequirementId::from_raw(9),
        Some(RequirementId::ServiceChannel)
    );
}

#[test]
fn status_registry_preserves_wire_values() {
    assert_eq!(status::OK, 0x0000_0000);
    assert_eq!(status::CORE_INVALID_ARGUMENT, 0x0100_0001);
    assert_eq!(status::ABI_BAD_SLOT, 0x0200_0001);
    assert_eq!(status::HANDLE_STALE, 0x0300_0002);
    assert_eq!(status::SECURITY_RIGHTS_DENIED, 0x0400_0001);
    assert_eq!(status::STREAM_WOULD_BLOCK, 0x0500_0002);
    assert_eq!(status::STREAM_CLOSED, 0x0500_0004);
    assert_eq!(status::STREAM_ERROR, 0x0500_0005);
    assert_eq!(status::MEMORY_INVALID_ALIGNMENT, 0x0600_0002);
    assert_eq!(status::PROCESS_WAIT_IN_PROGRESS, 0x0700_0005);
    assert_eq!(status::IMAGE_NOT_EXECUTABLE, 0x0800_0003);
    assert_eq!(status::EVENT_CANCELLED, 0x0900_0006);
}

#[test]
fn process_and_event_operations_preserve_contracts() {
    let spawn = operation(OperationId::ProcessSpawn).expect("process.spawn 必须注册");
    assert_eq!(spawn.id as u32, 11);
    assert_eq!(spawn.interface, Some(ObjectInterface::Process));
    assert_eq!(spawn.required_rights, Rights::SPAWN);
    assert_eq!(
        spawn.signature,
        "epoch=1;operation=11;object=1;args=user_const_ptr,u64,zero,zero,zero;result=handle"
    );

    let wait = operation(OperationId::EventWait).expect("event.wait 必须注册");
    assert_eq!(wait.id as u32, 20);
    assert_eq!(wait.interface, Some(ObjectInterface::EventPort));
    assert_eq!(wait.required_rights, Rights::OBSERVE);
}

#[test]
fn component_registry_preserves_append_only_ids_and_rights() {
    assert_eq!(ObjectInterface::Image as u16, 5);
    assert_eq!(ObjectInterface::Component as u16, 7);
    assert_eq!(ObjectInterface::ComponentTransaction as u16, 8);
    assert_eq!(ObjectInterface::Interface as u16, 9);
    assert_eq!(Rights::LOAD.bits(), 1 << 15);
    assert_eq!(Rights::UNLOAD.bits(), 1 << 16);
    assert_eq!(crate::right_by_name("load").unwrap().right, Rights::LOAD);
    assert_eq!(
        crate::right_by_name("unload").unwrap().right,
        Rights::UNLOAD
    );
    assert!(
        requirement(RequirementId::SelfProcess)
            .unwrap()
            .max_rights
            .is_subset_of(Rights::from_bits(u64::MAX))
    );
    assert!(Rights::LOAD.is_subset_of(requirement(RequirementId::SelfProcess).unwrap().max_rights));
}

#[test]
fn component_operations_preserve_contracts() {
    let expected = [
        (
            OperationId::ComponentLoad,
            21,
            "component.load",
            Some(ObjectInterface::Process),
            Rights::LOAD,
            "epoch=1;operation=21;object=1;args=user_const_ptr,user_mut_ptr,zero,zero,zero;result=handle",
        ),
        (
            OperationId::ComponentActivate,
            22,
            "component.activate",
            Some(ObjectInterface::ComponentTransaction),
            Rights::LOAD,
            "epoch=1;operation=22;object=8;args=u32,user_mut_ptr,zero,zero,zero;result=handle",
        ),
        (
            OperationId::ComponentQuery,
            23,
            "component.query",
            Some(ObjectInterface::Component),
            Rights::INSPECT,
            "epoch=1;operation=23;object=7;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::ComponentInterface,
            24,
            "component.interface",
            Some(ObjectInterface::Component),
            Rights::BIND,
            "epoch=1;operation=24;object=7;args=user_const_ptr,zero,zero,zero,zero;result=handle,u64",
        ),
        (
            OperationId::ComponentUnload,
            25,
            "component.unload",
            Some(ObjectInterface::Component),
            Rights::UNLOAD,
            "epoch=1;operation=25;object=7;args=u64,user_mut_ptr,u64,zero,zero;result=handle",
        ),
        (
            OperationId::ComponentFinish,
            26,
            "component.finish",
            Some(ObjectInterface::ComponentTransaction),
            Rights::UNLOAD,
            "epoch=1;operation=26;object=8;args=u32,user_mut_ptr,zero,zero,zero;result=status",
        ),
        (
            OperationId::ComponentWake,
            27,
            "component.wake",
            Some(ObjectInterface::Component),
            Rights::NONE,
            "epoch=1;operation=27;object=7;args=u64,zero,zero,zero,zero;result=status",
        ),
    ];

    for (id, raw, name, interface, rights, signature) in expected {
        let spec = operation(id).expect("组件 operation 必须注册");
        assert_eq!(spec.id as u32, raw);
        assert_eq!(spec.name, name);
        assert_eq!(spec.interface, interface);
        assert_eq!(spec.required_rights, rights);
        assert_eq!(spec.signature, signature);
        assert_eq!(spec.submission(), crate::SubmissionMode::DirectOnly);
    }
}

#[test]
fn component_statuses_preserve_wire_values() {
    assert_eq!(status::COMPONENT_INVALID_IMAGE, 0x0a00_0001);
    assert_eq!(status::COMPONENT_DEPENDENCY_MISSING, 0x0a00_0002);
    assert_eq!(status::COMPONENT_DEPENDENCY_CONFLICT, 0x0a00_0003);
    assert_eq!(status::COMPONENT_DEPENDENCY_CYCLE, 0x0a00_0004);
    assert_eq!(status::COMPONENT_INITIALIZING, 0x0a00_0005);
    assert_eq!(status::COMPONENT_ACTIVE, 0x0a00_0006);
    assert_eq!(status::COMPONENT_IN_USE, 0x0a00_0007);
    assert_eq!(status::COMPONENT_DRAINING, 0x0a00_0008);
    assert_eq!(status::COMPONENT_TIMEOUT, 0x0a00_0009);
    assert_eq!(status::COMPONENT_UNLOADED, 0x0a00_000a);
    assert_eq!(status::COMPONENT_SELF_UNLOAD, 0x0a00_000b);
    assert_eq!(status::COMPONENT_LIFECYCLE_FAILED, 0x0a00_000c);
    assert_eq!(status::COMPONENT_INVALID_TRANSACTION, 0x0a00_000d);
}

#[test]
fn component_wire_layouts_are_frozen() {
    assert_eq!(wire::COMPONENT_LOAD_REQUEST_SIZE, 64);
    assert_eq!(wire::COMPONENT_LIFECYCLE_SIZE, 64);
    assert_eq!(wire::COMPONENT_QUERY_SIZE, 64);
    assert_eq!(wire::INTERFACE_REQUEST_SIZE, 48);
    assert_eq!(wire::COMPONENT_CALL_STATE_SIZE, 64);
    assert_eq!(wire::COMPONENT_CONTEXT_SIZE, 64);
    assert_eq!(wire::COMPONENT_INTERFACE_GATE_SIZE, 32);
    assert_eq!(core::mem::align_of::<wire::ComponentLoadRequest>(), 8);
    assert_eq!(core::mem::offset_of!(wire::ComponentLoadRequest, images), 8);
    assert_eq!(
        core::mem::offset_of!(wire::ComponentLoadRequest, bindings),
        24
    );
    assert_eq!(core::mem::offset_of!(wire::ComponentLifecycle, entry), 16);
    assert_eq!(core::mem::offset_of!(wire::ComponentLifecycle, context), 24);
    assert_eq!(
        core::mem::offset_of!(wire::ComponentQuery, component_identity),
        16
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentQuery, active_calls),
        48
    );
    assert_eq!(
        core::mem::offset_of!(wire::InterfaceRequest, signature_hash),
        16
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentCallState, active_calls),
        16
    );
    assert_eq!(core::mem::offset_of!(wire::ComponentContext, call_state), 8);
    assert_eq!(
        core::mem::offset_of!(wire::ComponentContext, call_slot_count),
        32
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentContext, capability_count),
        40
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentContext, capabilities),
        48
    );
    assert_eq!(
        core::mem::size_of::<wire::ComponentCapabilityRecord>(),
        wire::COMPONENT_CAPABILITY_RECORD_SIZE
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentCapabilityRecord, handle),
        8
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentCapabilityRecord, granted_rights),
        16
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentInterfaceGate, target),
        8
    );
    assert_eq!(
        core::mem::offset_of!(wire::ComponentInterfaceGate, component),
        16
    );
    assert_eq!(wire::COMPONENT_ACTION_NONE, 0);
    assert_eq!(wire::COMPONENT_ACTION_INITIALIZE, 1);
    assert_eq!(wire::COMPONENT_ACTION_FINALIZE, 2);
    assert_eq!(wire::COMPONENT_STATE_PREPARING, 1);
    assert_eq!(wire::COMPONENT_STATE_FAILED, 7);
    assert_eq!(wire::MAX_COMPONENT_IMAGES, 256);
    assert_eq!(wire::MAX_COMPONENT_BINDINGS, 4096);
}

#[test]
fn image_query_exposes_only_verified_identity() {
    let query = operation(OperationId::ImageQuery).expect("image.query 必须注册");
    assert_eq!(query.id as u32, 66);
    assert_eq!(query.name, "image.query");
    assert_eq!(query.interface, Some(ObjectInterface::Image));
    assert_eq!(query.required_rights, Rights::INSPECT);
    assert_eq!(
        query.signature,
        "epoch=1;operation=66;object=5;args=user_mut_ptr,zero,zero,zero,zero;result=status"
    );
    assert_eq!(query.submission(), crate::SubmissionMode::DirectOnly);

    assert_eq!(wire::IMAGE_INFO_SIZE, 144);
    assert_eq!(wire::IMAGE_ARTIFACT_EXECUTABLE, 1);
    assert_eq!(wire::IMAGE_ARTIFACT_SHARED_COMPONENT, 2);
    assert_eq!(core::mem::size_of::<wire::ImageInfo>(), 144);
    assert_eq!(core::mem::align_of::<wire::ImageInfo>(), 8);
    assert_eq!(core::mem::offset_of!(wire::ImageInfo, artifact_kind), 0);
    assert_eq!(core::mem::offset_of!(wire::ImageInfo, enabled_features), 8);
    assert_eq!(
        core::mem::offset_of!(wire::ImageInfo, component_identity),
        32
    );
    assert_eq!(core::mem::offset_of!(wire::ImageInfo, build_id), 64);
    assert_eq!(core::mem::offset_of!(wire::ImageInfo, content_hash), 96);
    assert_eq!(core::mem::offset_of!(wire::ImageInfo, reserved), 128);
}

#[test]
fn image_trust_rejections_keep_distinct_statuses() {
    assert_eq!(status::IMAGE_UNSIGNED, 0x0800_0004);
    assert_eq!(status::IMAGE_UNKNOWN_KEY, 0x0800_0005);
    assert_eq!(status::IMAGE_BAD_SIGNATURE, 0x0800_0006);
    assert_eq!(status::IMAGE_REVOKED, 0x0800_0007);
    assert_eq!(status::IMAGE_ROLLBACK, 0x0800_0008);
    for name in [
        "image.unsigned",
        "image.unknown_key",
        "image.bad_signature",
        "image.revoked",
        "image.rollback",
    ] {
        assert!(
            crate::status::STATUS_CODES
                .iter()
                .any(|status| status.name == name),
            "缺少 {name}"
        );
    }
}

#[test]
fn native_foundation_interfaces_and_rights_are_append_only() {
    assert_eq!(ObjectInterface::Thread as u16, 10);
    assert_eq!(ObjectInterface::MemoryObject as u16, 11);
    assert_eq!(ObjectInterface::Directory as u16, 12);
    assert_eq!(ObjectInterface::File as u16, 13);
    assert_eq!(ObjectInterface::Channel as u16, 14);
    assert_eq!(Rights::MAP.bits(), 1 << 17);
    assert_eq!(Rights::RESIZE.bits(), 1 << 18);
    assert_eq!(Rights::OPEN.bits(), 1 << 19);
    assert_eq!(Rights::MODIFY.bits(), 1 << 20);
    assert_eq!(Rights::SEND.bits(), 1 << 21);
    assert_eq!(Rights::RECEIVE.bits(), 1 << 22);
    assert_eq!(crate::right_by_name("map").unwrap().right, Rights::MAP);
    assert_eq!(
        crate::right_by_name("receive").unwrap().right,
        Rights::RECEIVE
    );
}

#[test]
fn native_foundation_status_families_are_append_only() {
    assert_eq!(status::THREAD_INVALID, 0x0b00_0001);
    assert_eq!(status::THREAD_WOULD_BLOCK, 0x0b00_0002);
    assert_eq!(status::THREAD_TIMEOUT, 0x0b00_0003);
    assert_eq!(status::THREAD_ALREADY_EXITED, 0x0b00_0004);
    assert_eq!(status::THREAD_SELF, 0x0b00_0005);

    assert_eq!(status::FILESYSTEM_INVALID_PATH, 0x0c00_0001);
    assert_eq!(status::FILESYSTEM_NOT_FOUND, 0x0c00_0002);
    assert_eq!(status::FILESYSTEM_ALREADY_EXISTS, 0x0c00_0003);
    assert_eq!(status::FILESYSTEM_NOT_DIRECTORY, 0x0c00_0004);
    assert_eq!(status::FILESYSTEM_IS_DIRECTORY, 0x0c00_0005);
    assert_eq!(status::FILESYSTEM_NOT_EMPTY, 0x0c00_0006);
    assert_eq!(status::FILESYSTEM_READ_ONLY, 0x0c00_0007);
    assert_eq!(status::FILESYSTEM_END, 0x0c00_0008);
    assert_eq!(status::FILESYSTEM_CHANGED, 0x0c00_0009);
    assert_eq!(status::FILESYSTEM_ERROR, 0x0c00_000a);

    assert_eq!(status::CHANNEL_FULL, 0x0d00_0001);
    assert_eq!(status::CHANNEL_EMPTY, 0x0d00_0002);
    assert_eq!(status::CHANNEL_PEER_CLOSED, 0x0d00_0003);
    assert_eq!(status::CHANNEL_MESSAGE_TOO_LARGE, 0x0d00_0004);
    assert_eq!(status::CHANNEL_BUFFER_TOO_SMALL, 0x0d00_0005);
    assert_eq!(status::CHANNEL_TRANSFER_INVALID, 0x0d00_0006);
}

#[test]
fn native_foundation_operations_preserve_contracts() {
    let expected = [
        (
            OperationId::ThreadCreate,
            28,
            "thread.create",
            Some(ObjectInterface::Process),
            Rights::CREATE,
            "epoch=1;operation=28;object=1;args=user_const_ptr,handle,zero,zero,zero;result=handle",
        ),
        (
            OperationId::ThreadJoin,
            29,
            "thread.join",
            Some(ObjectInterface::Thread),
            Rights::WAIT,
            "epoch=1;operation=29;object=10;args=user_mut_ptr,u64,zero,zero,zero;result=status",
        ),
        (
            OperationId::ThreadTerminate,
            30,
            "thread.terminate",
            Some(ObjectInterface::Thread),
            Rights::TERMINATE,
            "epoch=1;operation=30;object=10;args=u32,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::ThreadQuery,
            31,
            "thread.query",
            Some(ObjectInterface::Thread),
            Rights::INSPECT,
            "epoch=1;operation=31;object=10;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::MemoryCreate,
            32,
            "memory.create",
            Some(ObjectInterface::Process),
            Rights::CREATE,
            "epoch=1;operation=32;object=1;args=user_const_ptr,zero,zero,zero,zero;result=handle",
        ),
        (
            OperationId::MemoryMap,
            33,
            "memory.map",
            Some(ObjectInterface::MemoryObject),
            Rights::MAP,
            "epoch=1;operation=33;object=11;args=user_const_ptr,zero,zero,zero,zero;result=u64,u64",
        ),
        (
            OperationId::MemoryUnmap,
            34,
            "memory.unmap",
            Some(ObjectInterface::AddressSpace),
            Rights::FREE,
            "epoch=1;operation=34;object=2;args=u64,u64,zero,zero,zero;result=status",
        ),
        (
            OperationId::MemoryQuery,
            35,
            "memory.query",
            Some(ObjectInterface::MemoryObject),
            Rights::INSPECT,
            "epoch=1;operation=35;object=11;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::DirectoryOpen,
            36,
            "directory.open",
            Some(ObjectInterface::Directory),
            Rights::OPEN,
            "epoch=1;operation=36;object=12;args=user_const_ptr,zero,zero,zero,zero;result=handle",
        ),
        (
            OperationId::DirectoryCreate,
            37,
            "directory.create",
            Some(ObjectInterface::Directory),
            Rights::MODIFY,
            "epoch=1;operation=37;object=12;args=user_const_ptr,zero,zero,zero,zero;result=handle",
        ),
        (
            OperationId::DirectoryRemove,
            38,
            "directory.remove",
            Some(ObjectInterface::Directory),
            Rights::MODIFY,
            "epoch=1;operation=38;object=12;args=user_const_ptr,u32,zero,zero,zero;result=status",
        ),
        (
            OperationId::DirectoryQuery,
            39,
            "directory.query",
            Some(ObjectInterface::Directory),
            Rights::INSPECT,
            "epoch=1;operation=39;object=12;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::FileRead,
            40,
            "file.read",
            Some(ObjectInterface::File),
            Rights::READ,
            "epoch=1;operation=40;object=13;args=user_mut_ptr,u64,u64,u32,zero;result=u64",
        ),
        (
            OperationId::FileWrite,
            41,
            "file.write",
            Some(ObjectInterface::File),
            Rights::WRITE,
            "epoch=1;operation=41;object=13;args=user_const_ptr,u64,u64,u32,zero;result=u64",
        ),
        (
            OperationId::FileResize,
            42,
            "file.resize",
            Some(ObjectInterface::File),
            Rights::RESIZE,
            "epoch=1;operation=42;object=13;args=u64,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::FileQuery,
            43,
            "file.query",
            Some(ObjectInterface::File),
            Rights::INSPECT,
            "epoch=1;operation=43;object=13;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::FileMap,
            44,
            "file.map",
            Some(ObjectInterface::File),
            Rights::MAP,
            "epoch=1;operation=44;object=13;args=u64,u64,u32,zero,zero;result=handle",
        ),
        (
            OperationId::ChannelCreate,
            45,
            "channel.create",
            Some(ObjectInterface::Process),
            Rights::CREATE,
            "epoch=1;operation=45;object=1;args=u32,zero,zero,zero,zero;result=handle,handle",
        ),
        (
            OperationId::ChannelSend,
            46,
            "channel.send",
            Some(ObjectInterface::Channel),
            Rights::SEND,
            "epoch=1;operation=46;object=14;args=user_const_ptr,zero,zero,zero,zero;result=status",
        ),
        (
            OperationId::ChannelReceive,
            47,
            "channel.receive",
            Some(ObjectInterface::Channel),
            Rights::RECEIVE,
            "epoch=1;operation=47;object=14;args=user_mut_ptr,u64,zero,zero,zero;result=u64,u64",
        ),
    ];
    for (id, raw, name, interface, rights, signature) in expected {
        let spec = operation(id).expect("Native 基础对象 operation 必须注册");
        assert_eq!(spec.id as u32, raw);
        assert_eq!(spec.name, name);
        assert_eq!(spec.interface, interface);
        assert_eq!(spec.required_rights, rights);
        assert_eq!(spec.signature, signature);
        let submission = match id {
            OperationId::FileRead
            | OperationId::FileWrite
            | OperationId::ChannelSend
            | OperationId::ChannelReceive => crate::SubmissionMode::MemoryRegion,
            _ => crate::SubmissionMode::DirectOnly,
        };
        assert_eq!(spec.submission(), submission);
    }
}

#[test]
fn native_foundation_wire_layouts_are_frozen() {
    assert_eq!(wire::THREAD_CREATE_REQUEST_SIZE, 64);
    assert_eq!(wire::THREAD_RESULT_SIZE, 32);
    assert_eq!(wire::THREAD_INFO_SIZE, 48);
    assert_eq!(wire::MEMORY_CREATE_REQUEST_SIZE, 64);
    assert_eq!(wire::MEMORY_MAP_REQUEST_SIZE, 64);
    assert_eq!(wire::MEMORY_INFO_SIZE, 64);
    assert_eq!(wire::MEMORY_REGION_SIZE, 32);
    assert_eq!(wire::PATH_REF_SIZE, 16);
    assert_eq!(wire::DIRECTORY_REQUEST_SIZE, 64);
    assert_eq!(wire::DIRECTORY_INFO_SIZE, 64);
    assert_eq!(wire::FILE_INFO_SIZE, 64);
    assert_eq!(wire::CHANNEL_HANDLE_TRANSFER_SIZE, 32);
    assert_eq!(wire::CHANNEL_MESSAGE_SIZE, 64);
    assert_eq!(
        core::mem::offset_of!(wire::ThreadCreateRequest, stack_memory),
        8
    );
    assert_eq!(
        core::mem::offset_of!(wire::ThreadCreateRequest, argument),
        48
    );
    assert_eq!(
        core::mem::offset_of!(wire::MemoryMapRequest, permissions),
        40
    );
    assert_eq!(core::mem::offset_of!(wire::MemoryRegion, generation), 24);
    assert_eq!(
        core::mem::offset_of!(wire::DirectoryRequest, requested_rights),
        24
    );
    assert_eq!(core::mem::offset_of!(wire::ChannelMessage, handles_ptr), 16);
    assert_eq!(core::mem::offset_of!(wire::ChannelMessage, flags), 32);
    assert_eq!(wire::MAX_PATH_BYTES, 4096);
    assert_eq!(wire::MAX_CHANNEL_MESSAGE_BYTES, 1024 * 1024);
    assert_eq!(wire::MAX_CHANNEL_MESSAGE_HANDLES, 64);
}

#[test]
fn component_lifecycle_is_monotonic_and_unload_is_irreversible() {
    let mut lifecycle = crate::ComponentLifecycleMachine::new();
    assert_eq!(lifecycle.state(), crate::ComponentState::Preparing);
    assert_eq!(lifecycle.begin_initialization(), Ok(()));
    assert_eq!(lifecycle.state(), crate::ComponentState::Initializing);
    assert_eq!(lifecycle.activate(status::OK), Ok(()));
    assert_eq!(lifecycle.state(), crate::ComponentState::Active);
    assert_eq!(lifecycle.generation(), 1);

    assert_eq!(
        lifecycle.begin_unload(1, false, 0),
        Err(status::COMPONENT_IN_USE)
    );
    assert_eq!(lifecycle.state(), crate::ComponentState::Active);
    assert_eq!(lifecycle.begin_unload(0, false, 2), Ok(false));
    assert_eq!(lifecycle.state(), crate::ComponentState::Draining);
    assert_eq!(lifecycle.timeout(), status::COMPONENT_TIMEOUT);
    assert_eq!(lifecycle.state(), crate::ComponentState::Draining);
    assert_eq!(lifecycle.calls_drained(1), Err(status::COMPONENT_DRAINING));
    assert_eq!(lifecycle.calls_drained(0), Ok(()));
    assert_eq!(lifecycle.state(), crate::ComponentState::Finalizing);
    assert_eq!(
        lifecycle.finish(status::CORE_INVALID_ARGUMENT),
        status::COMPONENT_LIFECYCLE_FAILED
    );
    assert_eq!(lifecycle.state(), crate::ComponentState::Unloaded);
    assert_eq!(lifecycle.generation(), 2);
    assert_eq!(
        lifecycle.begin_unload(0, false, 0),
        Err(status::COMPONENT_UNLOADED)
    );
}

#[test]
fn component_lifecycle_rejects_self_unload_and_failed_init() {
    let mut lifecycle = crate::ComponentLifecycleMachine::new();
    lifecycle.begin_initialization().unwrap();
    assert_eq!(
        lifecycle.activate(status::CORE_INVALID_ARGUMENT),
        Err(status::COMPONENT_LIFECYCLE_FAILED)
    );
    assert_eq!(lifecycle.state(), crate::ComponentState::Failed);

    let mut active = crate::ComponentLifecycleMachine::new();
    active.begin_initialization().unwrap();
    active.activate(status::OK).unwrap();
    assert_eq!(
        active.begin_unload(0, true, 0),
        Err(status::COMPONENT_SELF_UNLOAD)
    );
    assert_eq!(active.state(), crate::ComponentState::Active);
}

#[test]
fn component_tls_allocator_honors_alignment_and_rolls_back_unpublished_tail() {
    let mut arena =
        crate::ComponentTlsAllocator::new(16 * 1024 * 1024, 4096).expect("合法 TLS arena 应建立");

    let reservation = arena
        .reserve(32, 2 * 1024 * 1024)
        .expect("高对齐 TLS 模板应分配");
    assert_eq!(reservation.offset(), 2 * 1024 * 1024);
    assert_eq!(reservation.size(), 4096);
    assert_eq!(reservation.identity(), 1);

    assert!(arena.rollback(reservation));
    let reused = arena
        .reserve(32, 2 * 1024 * 1024)
        .expect("未发布尾部回滚后应可复用");
    assert_eq!(reused.offset(), 2 * 1024 * 1024);
    assert_eq!(reused.identity(), 1);
}

#[test]
fn component_tls_allocator_reuses_non_lifo_reservations() {
    let mut arena =
        crate::ComponentTlsAllocator::new(4 * 4096, 4096).expect("合法 TLS arena 应建立");

    let first = arena.reserve(32, 16).expect("第一个 TLS 模板应分配");
    let second = arena.reserve(32, 16).expect("第二个 TLS 模板应分配");
    assert_eq!(first.offset(), 4096);
    assert_eq!(second.offset(), 8192);

    assert!(arena.rollback(first), "非尾部 TLS reservation 也必须可释放");
    let reused = arena.reserve(32, 16).expect("释放的非尾部空洞应可复用");
    assert_eq!(reused.offset(), first.offset());
    assert_ne!(reused.identity(), first.identity());

    assert!(arena.rollback(second));
    assert!(arena.rollback(reused));
}

#[test]
fn process_and_event_wire_layouts_are_frozen() {
    assert_eq!(wire::PROCESS_STRING_REF_SIZE, 16);
    assert_eq!(wire::PROCESS_ARRAY_REF_SIZE, 16);
    assert_eq!(wire::HANDLE_TRANSFER_SIZE, 32);
    assert_eq!(wire::SPAWN_REQUEST_SIZE, 64);
    assert_eq!(wire::PROCESS_RESULT_SIZE, 32);
    assert_eq!(wire::EVENT_RECORD_SIZE, 40);
    assert_eq!(core::mem::offset_of!(wire::SpawnRequest, argv), 8);
    assert_eq!(core::mem::offset_of!(wire::SpawnRequest, transfers), 40);
    assert_eq!(core::mem::offset_of!(wire::ProcessResult, detail0), 16);
    assert_eq!(core::mem::offset_of!(wire::EventRecord, sequence), 16);
    assert_eq!(wire::process_string_ref::LEN, 8);
    assert_eq!(wire::process_array_ref::RESERVED, 12);
    assert_eq!(wire::handle_transfer::SOURCE_HANDLE, 8);
    assert_eq!(wire::handle_transfer::FLAGS, 24);
    assert_eq!(wire::spawn_request::RESOURCE_POLICY, 56);
    assert_eq!(wire::process_result::FAULT_KIND, 12);
    assert_eq!(wire::process_result::DETAIL1, 24);
    assert_eq!(wire::event_record::VALUE1, 32);
    assert_eq!(wire::HANDLE_TRANSFER_MOVE, 1);
    assert_eq!(wire::MAX_EVENT_PORT_CAPACITY, 4096);
    assert_eq!(wire::MAX_EVENT_BATCH, 64);
}

#[test]
fn native_startup_wire_layout_is_frozen() {
    assert_eq!(wire::START_INFO_SIZE, 192);
    assert_eq!(wire::INITIAL_HANDLE_SIZE, 32);
    assert_eq!(wire::start_info::MAGIC, 0x00);
    assert_eq!(wire::start_info::ABI_EPOCH, 0x10);
    assert_eq!(wire::start_info::INITIAL_HANDLE_OFFSET, 0x68);
    assert_eq!(wire::start_info::RANDOM_SEED, 0x70);
    assert_eq!(wire::start_info::INIT_ARRAY_OFFSET, 0x98);
    assert_eq!(wire::start_info::INIT_ARRAY_COUNT, 0xa0);
    assert_eq!(wire::start_info::INIT_ARRAY_ENTRY_SIZE, 0xa4);
    assert_eq!(wire::start_info::RESERVED2, 0xa6);
    assert_eq!(wire::start_info::FINI_ARRAY_OFFSET, 0xa8);
    assert_eq!(wire::start_info::FINI_ARRAY_COUNT, 0xb0);
    assert_eq!(wire::start_info::FINI_ARRAY_ENTRY_SIZE, 0xb4);
    assert_eq!(wire::start_info::RESERVED3, 0xb6);
    assert_eq!(wire::start_info::RESERVED4, 0xb8);
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
            granted_rights: Rights::EXIT,
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
        enabled_features: 0x13,
        image_base: 0x4000_0000,
        initial_tls_base: 0x7fff_0000,
        initial_tls_size: 0x1000,
        initial_thread_pointer: 0x7fff_0000,
        argv: &argv,
        env: &env,
        initial_handles: &handles,
        call_slot_count: 3,
        random_seed,
        runtime_flags: 0b11,
        init_array: RuntimeArrayInfo {
            offset: 0x2000,
            count: 2,
            entry_size: 8,
        },
        fini_array: RuntimeArrayInfo {
            offset: 0x3000,
            count: 1,
            entry_size: 8,
        },
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
        0x13
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
    assert_eq!(
        u64::from_le_bytes(bytes[0x98..0xa0].try_into().unwrap()),
        0x2000
    );
    assert_eq!(u32::from_le_bytes(bytes[0xa0..0xa4].try_into().unwrap()), 2);
    assert_eq!(u16::from_le_bytes(bytes[0xa4..0xa6].try_into().unwrap()), 8);
    assert_eq!(
        u64::from_le_bytes(bytes[0xa8..0xb0].try_into().unwrap()),
        0x3000
    );
    assert_eq!(u32::from_le_bytes(bytes[0xb0..0xb4].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(bytes[0xb4..0xb6].try_into().unwrap()), 8);
    assert!(bytes[0xa6..0xa8].iter().all(|byte| *byte == 0));
    assert!(bytes[0xb6..0xc0].iter().all(|byte| *byte == 0));

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
        1 << 5
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
        init_array: RuntimeArrayInfo::EMPTY,
        fini_array: RuntimeArrayInfo::EMPTY,
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

    assert_eq!(
        build_start_info(StartInfoInput {
            argv: &[],
            init_array: RuntimeArrayInfo {
                offset: 0x2000,
                count: 0,
                entry_size: 8,
            },
            max_size: 4096,
            ..input
        }),
        Err(StartInfoBuildError::InvalidInput)
    );

    assert_eq!(
        build_start_info(StartInfoInput {
            enabled_features: 1,
            argv: &[],
            max_size: 4096,
            ..input
        }),
        Err(StartInfoBuildError::InvalidInput)
    );

    assert_eq!(
        build_start_info(StartInfoInput {
            initial_tls_base: 0x7000_0000,
            initial_tls_size: 32,
            initial_thread_pointer: 0x7000_0000,
            argv: &[],
            max_size: 4096,
            ..input
        }),
        Err(StartInfoBuildError::InvalidInput)
    );

    assert_eq!(
        build_start_info(StartInfoInput {
            runtime_flags: 1 << 63,
            argv: &[],
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
fn optional_unknown_operation_stays_unbound() {
    let mut unknown = TestImport::known(0, OperationId::ProcessExit, false);
    unknown.operation_id = 100;

    let plan = bind_native_abi(
        ABI_FAMILY_MYGO_NATIVE,
        ABI_EPOCH,
        &[unknown],
        &[] as &[TestCapability],
        NativeAbiPolicy::for_kernel(),
    )
    .expect("optional unknown operation 应产生未绑定 slot");
    assert_eq!(plan.call_slots[0].operation, None);
}

#[test]
fn required_unknown_operation_is_incompatible() {
    let mut unknown = TestImport::known(0, OperationId::ProcessExit, true);
    unknown.operation_id = 100;

    assert_eq!(
        bind_native_abi(
            ABI_FAMILY_MYGO_NATIVE,
            ABI_EPOCH,
            &[unknown],
            &[] as &[TestCapability],
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Incompatible(IncompatibleKind::Operation(
            100
        )))
    );
}

#[test]
fn stream_read_is_bound_for_kernel() {
    let import = TestImport::known(0, OperationId::StreamRead, true);
    let plan = bind_native_abi(
        ABI_FAMILY_MYGO_NATIVE,
        ABI_EPOCH,
        &[import],
        &[] as &[TestCapability],
        NativeAbiPolicy::for_kernel(),
    )
    .expect("stream.read 应属于当前内核 Native operation");
    assert_eq!(plan.call_slots[0].operation, Some(OperationId::StreamRead));
}

#[test]
fn address_space_operations_are_bound_for_kernel() {
    let imports = [
        TestImport::known(0, OperationId::MemoryAllocate, true),
        TestImport::known(1, OperationId::MemoryFree, true),
    ];
    let plan = bind_native_abi(
        ABI_FAMILY_MYGO_NATIVE,
        ABI_EPOCH,
        &imports,
        &[] as &[TestCapability],
        NativeAbiPolicy::for_kernel(),
    )
    .expect("VM operation 应属于当前内核 Native ABI");

    assert_eq!(
        plan.call_slots[0].operation,
        Some(OperationId::MemoryAllocate)
    );
    assert_eq!(plan.call_slots[1].operation, Some(OperationId::MemoryFree));
}

#[test]
fn registered_kernel_operations_are_all_bindable() {
    let imports: alloc::vec::Vec<_> = crate::OPERATIONS
        .iter()
        .enumerate()
        .map(|(slot, spec)| TestImport::known(slot as u32, spec.id, true))
        .collect();

    let plan = bind_native_abi(
        ABI_FAMILY_MYGO_NATIVE,
        ABI_EPOCH,
        &imports,
        &[] as &[TestCapability],
        NativeAbiPolicy::for_kernel(),
    )
    .expect("内核已注册并分发的 operation 必须全部可绑定");

    assert_eq!(plan.call_slots.len(), imports.len());
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
