use alloc::vec;
use alloc::vec::Vec;

use ed25519_dalek::{Signer, SigningKey};

use crate::{
    ActionId, BindingGraph, BindingId, ELM_ACTION_OPCODE_INVOKE, ELM_ACTION_RESULT_HEALTH,
    ELM_API_FEATURE_LOG, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
    ELM_EBI_EXPORT_FLAG_DIRECT_PINNED, ELM_EBI_EXPORT_FLAG_MANAGED, ELM_EBI_HOOK_ON_FINALIZE,
    ELM_EBI_HOOK_ON_INITIALIZE, ELM_EBI_HOOK_ON_MIGRATE_ABORT, ELM_EBI_HOOK_ON_MIGRATE_EXPORT,
    ELM_EBI_HOOK_ON_MIGRATE_IMPORT, ELM_EBI_IMPORT_FLAG_DIRECT_PINNED,
    ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL, ELM_EBI_IMPORT_FLAG_MANAGED, ELM_EBI_NAME_LEN,
    ELM_EBI_PROJECTION_SOURCE_ABI_VERSION, ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE,
    ELM_EBI_SEGMENT_FLAG_EXECUTE, ELM_EBI_SEGMENT_FLAG_READ, ELM_EBI_SEGMENT_FLAG_WRITE,
    ELM_EBI_SOURCE_ABI_VERSION, ELM_EBI_SYMBOL_NAME_LEN, ELM_EKI_BLOCK_DESC_SIZE,
    ELM_EKI_ELMAPI_BLOCK_SIZE, ELM_EKI_ELMAPI_BLOCK_VERSION, ELM_EKI_FORMAT_VERSION,
    ELM_EKI_HEADER_SIZE, ELM_EKI_IMAGE_HASH_SHA256_SIZE, ELM_EKI_MAGIC, ELM_EKI_MANIFEST_NAME_LEN,
    ELM_EKI_MANIFEST_VERSION_LEN, ELM_EKI_PROJECTION_SOURCE_ID, ELM_EKI_PROVIDER_PORT_RECORD_SIZE,
    ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_DETAIL_NONE, ELM_HEALTH_FLAG_HAS_FAILURES,
    ELM_LIFECYCLE_REASON_HAS_DEPENDENTS, ELM_LIFECYCLE_REASON_HAS_EXTENSIONS,
    ELM_LIFECYCLE_REASON_HOOK_FAILED, ELM_LIFECYCLE_REASON_NONE, ELM_MGR_ACTION_BIND,
    ELM_MGR_ACTION_DETACH, ELM_MGR_ACTION_EVENT_READ, ELM_MGR_ACTION_EVENT_SUBSCRIBE,
    ELM_MGR_ACTION_EXTENSION_ATTACH, ELM_MGR_ACTION_EXTENSION_DETACH,
    ELM_MGR_ACTION_EXTENSION_DISPATCH, ELM_MGR_ACTION_EXTENSION_QUERY, ELM_MGR_ACTION_HEALTH_QUERY,
    ELM_MGR_ACTION_NATIVE_CAPABILITY_QUERY, ELM_MGR_ACTION_PROVIDER_ASYNC, ELM_MGR_ACTION_REPLACE,
    ELM_MGR_ACTION_TODO_QUERY, ELM_MGR_ACTION_TRUST_QUERY, ELM_MGR_ACTION_UNBIND,
    ELM_MGR_API_CONTRACT_LEN, ELM_MGR_API_FLAG_STABLE, ELM_MGR_API_KIND_EVENT, ELM_MGR_BUILTIN_ID,
    ELM_MGR_EVENT_READ_FLAG_ADVANCE, ELM_MGR_EXTENSION_CONTRACT_LEN,
    ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN, ELM_MGR_EXTENSION_PAYLOAD_LEN,
    ELM_MGR_EXTENSION_POINT_LEN, ELM_MGR_MAX_INPUT, ELM_MGR_MAX_PAYLOAD,
    ELM_MGR_POLICY_API_REGISTRY, ELM_MGR_POLICY_AUDIT, ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS,
    ELM_MGR_POLICY_EXTENSION_RUNTIME, ELM_MGR_POLICY_HEALTH, ELM_MGR_POLICY_HOT_REPLACE,
    ELM_MGR_POLICY_MENU_BINDING, ELM_MGR_POLICY_NATIVE_CAPABILITIES,
    ELM_MGR_POLICY_NATIVE_LIFECYCLE, ELM_MGR_POLICY_NEXUS_BINDING, ELM_MGR_POLICY_PREFLIGHT,
    ELM_MGR_POLICY_PROVIDER_ASYNC, ELM_MGR_POLICY_PROVIDER_PORTS, ELM_MGR_POLICY_RESOURCE_BUDGET,
    ELM_MGR_POLICY_TODO_REGISTRY, ELM_MGR_POLICY_TRUST, ELM_MGR_RELATION_POINT_LEN,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_TODO, ELM_NATIVE_CAPABILITY_FLAG_KERNEL_SYMBOL,
    ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD, ELM_NATIVE_CAPABILITY_KIND_EXPORT,
    ELM_NATIVE_CAPABILITY_KIND_IMPORT, ELM_NATIVE_CAPABILITY_NAME_LEN,
    ELM_NATIVE_ENTRY_ABI_VERSION, ELM_NATIVE_PROVIDER_CALL_ABI_VERSION,
    ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK,
    ELM_NEXUS_CONTRACT_LEN, ELM_POLICY_BLOCK_CONTRACT_MISMATCH, ELM_POLICY_BLOCK_DUPLICATE_BINDING,
    ELM_POLICY_BLOCK_EXTENSION_DUPLICATE, ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND,
    ELM_POLICY_BLOCK_HAS_DEPENDENTS, ELM_POLICY_BLOCK_HAS_EXTENSIONS,
    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED, ELM_POLICY_BLOCK_PORT_TODO,
    ELM_POLICY_BLOCK_PROVIDER_BUSY, ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED,
    ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, ELM_POLICY_BLOCK_RESOURCE_QUOTA, ELM_PROOF_SHA256_LEN,
    ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS, ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS,
    ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS, ELM_PROVIDER_ASYNC_QUEUE_LIMIT, ELM_PROVIDER_FLAG_DYNAMIC,
    ELM_PROVIDER_FLAG_KERNEL_BACKEND, ELM_PROVIDER_FLAG_NATIVE_BACKEND,
    ELM_PROVIDER_FLAG_TODO_BACKEND, ELM_PROVIDER_PORT_FLAG_NONE,
    ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED, ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK,
    ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE, ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK,
    ELM_REPLACE_CELL_ABI_VERSION, ELM_REPLACE_MIGRATION_STATE_MAX, ELM_RUNTIME_LOG_MESSAGE_LEN,
    ELM_TODO_DETAIL_LEN, ELM_TODO_FLAG_ACTIVE, ELM_TODO_FLAG_STATIC, ELM_TODO_KIND_RUNTIME,
    ELM_TODO_NAME_LEN, ELM_TODO_REGISTRY_FLAG_TRUNCATED, ElmActionInvokeReply,
    ElmActionInvokeRequest, ElmCallFrame, ElmCellSnapshot, ElmContext, ElmCoreHealthHeader,
    ElmCoreHealthRecord, ElmCoreInfo, ElmCtlCommand, ElmEbiArch, ElmEbiDependencyDecl, ElmEbiEntry,
    ElmEbiExportDecl, ElmEbiExtensionDecl, ElmEbiExtensionPointDecl, ElmEbiImage, ElmEbiImportDecl,
    ElmEbiLifecycleHookDecl, ElmEbiLifecycleHookKind, ElmEbiLifecycleHooks, ElmEbiLoadStatus,
    ElmEbiMenuDecl, ElmEbiProofV1, ElmEbiProviderPortDecl, ElmEbiRelocationKind,
    ElmEbiRustHookSignature, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiSourceKind,
    ElmEbiSourceRequest, ElmEbiSymbolLocationDecl, ElmEbiTarget, ElmEbiUnit, ElmEkiBlockKind,
    ElmError, ElmEventRecord, ElmExtensionAttachRequest, ElmExtensionAttachResponse,
    ElmExtensionDetachRequest, ElmExtensionDispatchRequest, ElmExtensionDispatchResponse,
    ElmExtensionSnapshotHeader, ElmExtensionSnapshotRecord, ElmId, ElmKind, ElmLifecycleAction,
    ElmLifecyclePhase, ElmLifecyclePlanRequest, ElmLifecyclePlanResponse, ElmLifecycleRequest,
    ElmLifecycleResponse, ElmManifest, ElmMenuItemKind, ElmMenuItemSnapshot, ElmMenuSnapshotHeader,
    ElmMgrApiDescriptor, ElmMgrApiRegistryHeader, ElmMgrAuditHeader, ElmMgrAuditRecord,
    ElmMgrCallHeader, ElmMgrCallKind, ElmMgrEventSubscribeRequest, ElmMgrEventSubscribeResponse,
    ElmMgrEventSubscriptionHeader, ElmMgrEventSubscriptionRecord, ElmMgrEventUnsubscribeRequest,
    ElmMgrEventUnsubscribeResponse, ElmMgrPolicyInfo, ElmMgrRelationKind, ElmMgrRelationRecord,
    ElmMgrResponseHeader, ElmMgrSubscribedEventReadHeader, ElmMgrSubscribedEventReadRequest,
    ElmMgrTopologyHeader, ElmMixinMode, ElmName, ElmNativeCapabilityHeader,
    ElmNativeCapabilityRecord, ElmNativeEntryFrameV1, ElmNativeProviderCallV1,
    ElmNativeProviderSnapshotV1, ElmNexusBindPlanResponse, ElmNexusBindRequest,
    ElmNexusBindingRecord, ElmNexusBindingSnapshotHeader, ElmNexusUnbindRequest, ElmPanicStrategy,
    ElmPortAccessPolicy, ElmPortSnapshot, ElmProjectionSourceRequest,
    ElmProviderAsyncCancelRequest, ElmProviderAsyncCancelResponse, ElmProviderAsyncPollRequest,
    ElmProviderAsyncPollResponse, ElmProviderAsyncState, ElmProviderAsyncSubmitRequest,
    ElmProviderAsyncSubmitResponse, ElmProviderInvokeRequest, ElmProviderInvokeResponse,
    ElmProviderPortRecord, ElmProviderPortRegisterRequest, ElmProviderPortRegisterResponse,
    ElmProviderPortStatsHeader, ElmProviderPortStatsRecord, ElmProviderPortUnregisterRequest,
    ElmProviderQueueStatsHeader, ElmProviderQueueStatsRecord, ElmProviderSnapshotHeader,
    ElmProviderSnapshotRequest, ElmReplaceCellRequestV1, ElmReplaceCellResponseV1, ElmReplyFrame,
    ElmResourceBudget, ElmResourceKind, ElmResourceUsage, ElmRuntimeEventRequest,
    ElmRuntimeEventResponse, ElmRuntimeLogRequest, ElmRuntimeLogResponse,
    ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord, ElmRustAbiFingerprintV1,
    ElmSnapshotHeader, ElmState, ElmTodoRegistryHeader, ElmTodoRegistryRecord, ElmTrustAcceptance,
    ElmTrustAnchor, ElmTrustError, ElmTrustStore, ElmVersion, FlowContract, FlowDirection,
    FlowMode, Generation, LeaseId, LeaseKind, LeaseRegistry, LeaseRights, LeaseState, PortId,
    ResourceLease, TopologyEventKind, builtin_port_descriptors, canonical_ebi_digest,
    first_lifecycle_reason, parse_eki_ebi_unit, parse_eki_image, planned_final_state, sha256,
    sha256_with_zeroed_range, state_code, status_from_blockers,
};

fn manifest(name: &str) -> ElmManifest {
    ElmManifest::new(
        ElmName::new(name).unwrap(),
        ElmVersion::new("0.1.0").unwrap(),
        ElmKind::Service,
    )
}

fn contract(name: &str) -> FlowContract {
    FlowContract::new(name).unwrap()
}

fn lifecycle_hooks() -> ElmEbiLifecycleHooks {
    ElmEbiLifecycleHooks::rust_context_result_v1()
}

fn ebi_unit(name: &str) -> ElmEbiUnit {
    ElmEbiUnit::new(manifest(name), ElmEbiTarget::new(ElmEbiArch::Any))
        .with_lifecycle_hooks(lifecycle_hooks())
}

#[test]
fn ebi_protocol_rejects_kernel_mixin_without_native_code() {
    let mixin = crate::ElmEbiKernelMixinDecl::new(
        "allocator.GlobalAlloc.alloc",
        "head",
        "__elm_kernel_mixin_trace_alloc",
        crate::ElmKernelMixinKind::Inject,
        0,
    )
    .unwrap()
    .with_site_identity(0, [1; 32], [2; 32], [3; 32], [4; 32], [5; 32], [6; 32]);
    let unit = ebi_unit("kernel-mixin-without-code").with_kernel_mixin(mixin);
    assert_eq!(
        unit.validate(ElmEbiArch::Any),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
}

#[test]
fn ebi_direct_symbols_require_exact_rust_abi_hashes() {
    assert_eq!(
        ElmEbiExportDecl::new("test.legacy", "test.legacy@1", 1, 0, [0; 32]),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
    let legacy_import =
        ElmEbiImportDecl::new("test.legacy", "test.legacy@1", 1, 0, [0; 32]).unwrap();
    assert_eq!(
        ebi_unit("legacy-direct-import")
            .with_import(legacy_import)
            .validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
    assert_eq!(
        ElmEbiImportDecl::new(
            "test.direct",
            "test.direct@1",
            1,
            ELM_EBI_IMPORT_FLAG_DIRECT_PINNED,
            [0; 32],
        ),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
    assert!(
        ElmEbiImportDecl::new(
            "test.direct",
            "test.direct@1",
            1,
            ELM_EBI_IMPORT_FLAG_DIRECT_PINNED,
            [1; 32],
        )
        .is_ok()
    );
    assert_eq!(
        ElmEbiExportDecl::new(
            "test.direct",
            "test.direct@1",
            1,
            ELM_EBI_EXPORT_FLAG_DIRECT_PINNED,
            [0; 32],
        ),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
    assert!(
        ElmEbiExportDecl::new(
            "test.direct",
            "test.direct@1",
            1,
            ELM_EBI_EXPORT_FLAG_DIRECT_PINNED,
            [2; 32],
        )
        .is_ok()
    );
    assert_eq!(
        ElmEbiImportDecl::new(
            "test.managed",
            "test.managed@1",
            1,
            ELM_EBI_IMPORT_FLAG_MANAGED,
            [3; 32],
        ),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
    assert_eq!(
        ElmEbiExportDecl::new(
            "test.managed",
            "test.managed@1",
            1,
            ELM_EBI_EXPORT_FLAG_MANAGED,
            [4; 32],
        ),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_i64(out: &mut [u8], offset: usize, value: i64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn fixed_copy(out: &mut [u8], offset: usize, capacity: usize, value: &str) {
    let bytes = value.as_bytes();
    assert!(bytes.len() <= capacity);
    out[offset..offset + bytes.len()].copy_from_slice(bytes);
}

fn eki_manifest_block(name: &str, version: &str, kind: ElmKind) -> Vec<u8> {
    let mut out = vec![0; 16 + ELM_EKI_MANIFEST_NAME_LEN + ELM_EKI_MANIFEST_VERSION_LEN];
    write_u32(&mut out, 0, kind.as_raw());
    write_u16(&mut out, 8, name.len() as u16);
    write_u16(&mut out, 10, version.len() as u16);
    fixed_copy(&mut out, 16, ELM_EKI_MANIFEST_NAME_LEN, name);
    fixed_copy(
        &mut out,
        16 + ELM_EKI_MANIFEST_NAME_LEN,
        ELM_EKI_MANIFEST_VERSION_LEN,
        version,
    );
    out
}

fn eki_menu_block(label: &str, description: &str, route: &str) -> Vec<u8> {
    let mut out = vec![
        0;
        16 + crate::ELM_MENU_LABEL_LEN
            + crate::ELM_MENU_DESCRIPTION_LEN
            + crate::ELM_MENU_ROUTE_LEN
    ];
    write_u32(&mut out, 0, ElmMenuItemKind::Action as u32);
    write_u16(&mut out, 8, label.len() as u16);
    write_u16(&mut out, 10, description.len() as u16);
    write_u16(&mut out, 12, route.len() as u16);
    fixed_copy(&mut out, 16, crate::ELM_MENU_LABEL_LEN, label);
    fixed_copy(
        &mut out,
        16 + crate::ELM_MENU_LABEL_LEN,
        crate::ELM_MENU_DESCRIPTION_LEN,
        description,
    );
    fixed_copy(
        &mut out,
        16 + crate::ELM_MENU_LABEL_LEN + crate::ELM_MENU_DESCRIPTION_LEN,
        crate::ELM_MENU_ROUTE_LEN,
        route,
    );
    out
}

fn eki_abi_fingerprint_block(profile_hash: [u8; 32], bridge_abi_version: u16) -> Vec<u8> {
    let mut fingerprint = ElmRustAbiFingerprintV1::new(
        [0x31; 32],
        [0x32; 32],
        [0x33; 32],
        1,
        ElmPanicStrategy::AbortThroughRuntime,
        1,
        0,
    );
    fingerprint.kernel_api_profile_hash = profile_hash;
    fingerprint.kernel_api_bridge_abi_version = bridge_abi_version;
    let mut out = vec![0; crate::ELM_EKI_ABI_FINGERPRINT_BLOCK_SIZE];
    write_u16(&mut out, 0, crate::ELM_RUST_ABI_FINGERPRINT_VERSION);
    write_u16(&mut out, 2, fingerprint.elmapi_version);
    out[4] = fingerprint.panic_strategy as u8;
    out[5] = fingerprint.code_model;
    write_u64(&mut out, 8, fingerprint.target_features);
    write_u32(&mut out, 16, fingerprint.flags);
    out[24..56].copy_from_slice(&fingerprint.rustc_commit_hash);
    out[56..88].copy_from_slice(&fingerprint.target_spec_hash);
    out[88..120].copy_from_slice(&fingerprint.kernel_interface_hash);
    out[120..152].copy_from_slice(&fingerprint.kernel_api_profile_hash);
    write_u16(&mut out, 152, fingerprint.kernel_api_bridge_abi_version);
    out
}

fn eki_segments_block(
    kind: ElmEbiSegmentKind,
    flags: u32,
    file_size: u64,
    mem_size: u64,
) -> Vec<u8> {
    eki_segments_blocks(&[(kind, flags, file_size, mem_size)])
}

fn eki_segments_blocks(entries: &[(ElmEbiSegmentKind, u32, u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, entries.len() as u32);
    push_u32(&mut out, 0);
    for (kind, flags, file_size, mem_size) in entries {
        push_u32(&mut out, *kind as u32);
        push_u32(&mut out, *flags);
        push_u64(&mut out, *file_size);
        push_u64(&mut out, *mem_size);
        push_u64(&mut out, 0);
    }
    out
}

fn eki_symbol_block(name: &str, contract: &str, version: u32) -> Vec<u8> {
    eki_symbol_block_with_flags(name, contract, version, 0)
}

fn eki_symbol_block_with_flags(name: &str, contract: &str, version: u32, flags: u32) -> Vec<u8> {
    eki_symbol_block_with_flags_and_hash(name, contract, version, flags, [0; 32])
}

fn eki_symbol_block_with_flags_and_hash(
    name: &str,
    contract: &str,
    version: u32,
    flags: u32,
    rust_abi_hash: [u8; 32],
) -> Vec<u8> {
    let record = 8;
    let mut out = vec![0; record + 48 + ELM_EBI_SYMBOL_NAME_LEN + ELM_NEXUS_CONTRACT_LEN];
    write_u32(&mut out, 0, 1);
    let (min_version, max_version) = if version == 0 {
        (1, u32::MAX)
    } else {
        (version, version)
    };
    write_u32(&mut out, record, min_version);
    write_u32(&mut out, record + 4, flags);
    write_u16(&mut out, record + 8, name.len() as u16);
    write_u16(&mut out, record + 10, contract.len() as u16);
    write_u32(&mut out, record + 12, max_version);
    out[record + 16..record + 48].copy_from_slice(&rust_abi_hash);
    fixed_copy(&mut out, record + 48, ELM_EBI_SYMBOL_NAME_LEN, name);
    fixed_copy(
        &mut out,
        record + 48 + ELM_EBI_SYMBOL_NAME_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        contract,
    );
    out
}

fn eki_elmapi_block(root_import_index: u32, required_features: u64, versions: &[u16]) -> Vec<u8> {
    let mut out = vec![0; ELM_EKI_ELMAPI_BLOCK_SIZE];
    write_u16(&mut out, 0, ELM_EKI_ELMAPI_BLOCK_VERSION);
    write_u16(&mut out, 2, versions.len() as u16);
    write_u32(&mut out, 4, root_import_index);
    write_u64(&mut out, 8, required_features);
    for (index, version) in versions.iter().enumerate() {
        write_u16(&mut out, 16 + index * 2, *version);
    }
    out
}

fn eki_lifecycle_hooks_block() -> Vec<u8> {
    let record = 8;
    let mut out = vec![0; record + 2 * (20 + ELM_EBI_SYMBOL_NAME_LEN)];
    write_u32(&mut out, 0, 2);
    write_lifecycle_hook_record(
        &mut out,
        record,
        ElmEbiLifecycleHookKind::Initialize,
        ELM_EBI_HOOK_ON_INITIALIZE,
    );
    write_lifecycle_hook_record(
        &mut out,
        record + 20 + ELM_EBI_SYMBOL_NAME_LEN,
        ElmEbiLifecycleHookKind::Finalize,
        ELM_EBI_HOOK_ON_FINALIZE,
    );
    out
}

fn eki_lifecycle_hooks_block_with_migration() -> Vec<u8> {
    let record = 8;
    let hook_record_size = 20 + ELM_EBI_SYMBOL_NAME_LEN;
    let mut out = vec![0; record + 5 * hook_record_size];
    write_u32(&mut out, 0, 5);
    for (index, (kind, symbol)) in [
        (
            ElmEbiLifecycleHookKind::Initialize,
            ELM_EBI_HOOK_ON_INITIALIZE,
        ),
        (ElmEbiLifecycleHookKind::Finalize, ELM_EBI_HOOK_ON_FINALIZE),
        (
            ElmEbiLifecycleHookKind::MigrateExport,
            ELM_EBI_HOOK_ON_MIGRATE_EXPORT,
        ),
        (
            ElmEbiLifecycleHookKind::MigrateImport,
            ELM_EBI_HOOK_ON_MIGRATE_IMPORT,
        ),
        (
            ElmEbiLifecycleHookKind::MigrateAbort,
            ELM_EBI_HOOK_ON_MIGRATE_ABORT,
        ),
    ]
    .iter()
    .enumerate()
    {
        write_lifecycle_hook_record(&mut out, record + index * hook_record_size, *kind, symbol);
    }
    out
}

fn eki_symbol_locations_block(entries: &[(&str, u32, u64, u64)]) -> Vec<u8> {
    let record_size = crate::ELM_EKI_SYMBOL_LOCATION_RECORD_SIZE;
    let mut out = vec![0; 8 + entries.len() * record_size];
    write_u32(&mut out, 0, entries.len() as u32);
    for (index, (name, segment_index, offset, size)) in entries.iter().enumerate() {
        let record = 8 + index * record_size;
        write_u16(&mut out, record, name.len() as u16);
        write_u32(&mut out, record + 8, *segment_index);
        write_u64(&mut out, record + 16, *offset);
        write_u64(&mut out, record + 24, *size);
        fixed_copy(&mut out, record + 32, ELM_EBI_SYMBOL_NAME_LEN, name);
    }
    out
}

fn eki_relocations_block(entries: &[(ElmEbiRelocationKind, u32, u32, u64, i64)]) -> Vec<u8> {
    let record_size = crate::ELM_EKI_RELOCATION_RECORD_SIZE;
    let mut out = vec![0; 8 + entries.len() * record_size];
    write_u32(&mut out, 0, entries.len() as u32);
    for (index, (kind, target_segment, value_index, target_offset, addend)) in
        entries.iter().enumerate()
    {
        let record = 8 + index * record_size;
        write_u32(&mut out, record, *kind as u32);
        write_u32(&mut out, record + 8, *target_segment);
        write_u32(&mut out, record + 12, *value_index);
        write_u64(&mut out, record + 16, *target_offset);
        write_i64(&mut out, record + 24, *addend);
    }
    out
}

fn write_lifecycle_hook_record(
    out: &mut [u8],
    offset: usize,
    kind: ElmEbiLifecycleHookKind,
    symbol: &str,
) {
    write_u32(out, offset, kind as u32);
    write_u16(out, offset + 8, crate::ELM_EBI_RUST_ABI_VERSION);
    write_u16(
        out,
        offset + 10,
        ElmEbiRustHookSignature::ContextResult as u16,
    );
    write_u16(out, offset + 12, symbol.len() as u16);
    fixed_copy(out, offset + 20, ELM_EBI_SYMBOL_NAME_LEN, symbol);
}

fn eki_dependency_block(provider_name: &str, contract: &str) -> Vec<u8> {
    let record = 8;
    let mut out = vec![0; record + 8 + ELM_EBI_NAME_LEN + ELM_NEXUS_CONTRACT_LEN];
    write_u32(&mut out, 0, 1);
    write_u16(&mut out, record, provider_name.len() as u16);
    write_u16(&mut out, record + 2, contract.len() as u16);
    fixed_copy(&mut out, record + 8, ELM_EBI_NAME_LEN, provider_name);
    fixed_copy(
        &mut out,
        record + 8 + ELM_EBI_NAME_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        contract,
    );
    out
}

fn eki_extension_point_block_with_mode(point: &str, contract: &str, mode: ElmMixinMode) -> Vec<u8> {
    let record = 8;
    let mut out = vec![0; record + 16 + ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN];
    write_u32(&mut out, 0, 1);
    write_u16(&mut out, record, point.len() as u16);
    write_u16(&mut out, record + 2, contract.len() as u16);
    write_u32(&mut out, record + 4, mode as u32);
    fixed_copy(&mut out, record + 16, ELM_MGR_RELATION_POINT_LEN, point);
    fixed_copy(
        &mut out,
        record + 16 + ELM_MGR_RELATION_POINT_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        contract,
    );
    out
}

fn eki_extension_block_with_dispatch(
    target_name: &str,
    point: &str,
    contract: &str,
    handler_contract: &str,
    priority: i32,
) -> Vec<u8> {
    let record = 8;
    let mut out = vec![
        0;
        record
            + 24
            + ELM_EBI_NAME_LEN
            + ELM_MGR_RELATION_POINT_LEN
            + ELM_NEXUS_CONTRACT_LEN * 2
    ];
    write_u32(&mut out, 0, 1);
    write_u16(&mut out, record, target_name.len() as u16);
    write_u16(&mut out, record + 2, point.len() as u16);
    write_u16(&mut out, record + 4, contract.len() as u16);
    write_u16(&mut out, record + 6, handler_contract.len() as u16);
    write_u32(&mut out, record + 8, priority as u32);
    fixed_copy(&mut out, record + 24, ELM_EBI_NAME_LEN, target_name);
    fixed_copy(
        &mut out,
        record + 24 + ELM_EBI_NAME_LEN,
        ELM_MGR_RELATION_POINT_LEN,
        point,
    );
    fixed_copy(
        &mut out,
        record + 24 + ELM_EBI_NAME_LEN + ELM_MGR_RELATION_POINT_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        contract,
    );
    fixed_copy(
        &mut out,
        record + 24 + ELM_EBI_NAME_LEN + ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        handler_contract,
    );
    out
}

fn eki_provider_port_block(
    contract: &str,
    access: ElmPortAccessPolicy,
    direction: FlowDirection,
    mode: FlowMode,
) -> Vec<u8> {
    eki_provider_port_block_with_handler(contract, access, direction, mode, None)
}

fn eki_provider_port_block_with_handler(
    contract: &str,
    access: ElmPortAccessPolicy,
    direction: FlowDirection,
    mode: FlowMode,
    handler: Option<&str>,
) -> Vec<u8> {
    eki_provider_port_block_with_symbols(contract, access, direction, mode, handler, None)
}

fn eki_provider_port_block_with_symbols(
    contract: &str,
    access: ElmPortAccessPolicy,
    direction: FlowDirection,
    mode: FlowMode,
    handler: Option<&str>,
    snapshot: Option<&str>,
) -> Vec<u8> {
    let record = 8;
    let mut out = vec![0; record + ELM_EKI_PROVIDER_PORT_RECORD_SIZE];
    write_u32(&mut out, 0, 1);
    write_u32(&mut out, record, access as u32);
    write_u32(&mut out, record + 4, direction as u32);
    write_u32(&mut out, record + 8, mode as u32);
    write_u16(&mut out, record + 16, contract.len() as u16);
    if let Some(handler) = handler {
        write_u16(&mut out, record + 18, handler.len() as u16);
        fixed_copy(
            &mut out,
            record + 24 + ELM_NEXUS_CONTRACT_LEN,
            ELM_EBI_SYMBOL_NAME_LEN,
            handler,
        );
    }
    if let Some(snapshot) = snapshot {
        write_u16(&mut out, record + 20, snapshot.len() as u16);
        fixed_copy(
            &mut out,
            record + 24 + ELM_NEXUS_CONTRACT_LEN + ELM_EBI_SYMBOL_NAME_LEN,
            ELM_EBI_SYMBOL_NAME_LEN,
            snapshot,
        );
    }
    fixed_copy(&mut out, record + 24, ELM_NEXUS_CONTRACT_LEN, contract);
    out
}

fn eki_image(blocks: &[(ElmEkiBlockKind, Vec<u8>)]) -> Vec<u8> {
    let block_count = blocks.len();
    let mut out = vec![0; ELM_EKI_HEADER_SIZE + block_count * ELM_EKI_BLOCK_DESC_SIZE];
    let mut payload_offset = out.len();
    for (index, (kind, payload)) in blocks.iter().enumerate() {
        let desc = ELM_EKI_HEADER_SIZE + index * ELM_EKI_BLOCK_DESC_SIZE;
        write_u32(&mut out, desc, *kind as u32);
        write_u64(&mut out, desc + 8, payload_offset as u64);
        write_u64(&mut out, desc + 16, payload.len() as u64);
        write_u64(&mut out, desc + 24, payload.len() as u64);
        out.extend_from_slice(payload);
        payload_offset += payload.len();
    }
    out[0..8].copy_from_slice(&ELM_EKI_MAGIC);
    write_u16(&mut out, 8, ELM_EKI_FORMAT_VERSION);
    write_u16(&mut out, 10, crate::ELM_EBI_ABI_VERSION);
    write_u32(&mut out, 12, ELM_EKI_HEADER_SIZE as u32);
    let file_size = out.len() as u64;
    write_u64(&mut out, 16, file_size);
    write_u64(&mut out, 24, ELM_EKI_HEADER_SIZE as u64);
    write_u32(&mut out, 40, ElmEbiArch::Any as u32);
    write_u16(&mut out, 44, 1);
    write_u32(&mut out, 48, block_count as u32);
    out
}

fn eki_image_with_header_hash(blocks: &[(ElmEkiBlockKind, Vec<u8>)]) -> Vec<u8> {
    let mut out = eki_image(blocks);
    let hash_offset = out.len();
    out.extend_from_slice(&[0; ELM_PROOF_SHA256_LEN]);
    let file_size = out.len() as u64;
    write_u64(&mut out, 16, file_size);
    write_u64(&mut out, 32, hash_offset as u64);
    write_u32(&mut out, 52, ELM_EKI_IMAGE_HASH_SHA256_SIZE);
    let digest = sha256_with_zeroed_range(&out, hash_offset, ELM_PROOF_SHA256_LEN).unwrap();
    out[hash_offset..hash_offset + ELM_PROOF_SHA256_LEN].copy_from_slice(&digest);
    out
}

#[test]
fn proof_sha256_matches_standard_vectors() {
    assert_eq!(
        sha256(b""),
        [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ]
    );
    assert_eq!(
        sha256(b"abc"),
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ]
    );
}

#[test]
fn canonical_ebi_digest_ignores_container_block_layout() {
    let code = vec![0x13, 0, 0, 0];
    let manifest = (
        ElmEkiBlockKind::Manifest,
        eki_manifest_block("canonical-layout", "0.1.0", ElmKind::Service),
    );
    let segments = (
        ElmEkiBlockKind::Segments,
        eki_segments_block(
            ElmEbiSegmentKind::Code,
            0,
            code.len() as u64,
            code.len() as u64,
        ),
    );
    let lifecycle = (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block());
    let symbols = (
        ElmEkiBlockKind::SymbolLocations,
        eki_symbol_locations_block(&[
            (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 2),
            (ELM_EBI_HOOK_ON_FINALIZE, 0, 2, 2),
        ]),
    );
    let first = parse_eki_image(&eki_image(&[
        manifest.clone(),
        segments.clone(),
        (ElmEkiBlockKind::Code, code.clone()),
        lifecycle.clone(),
        symbols.clone(),
    ]))
    .unwrap();
    let second = parse_eki_image(&eki_image(&[
        manifest,
        lifecycle,
        segments,
        (ElmEkiBlockKind::Code, code),
        symbols,
    ]))
    .unwrap();

    assert_ne!(
        first.unit.segments[0].source_index,
        second.unit.segments[0].source_index
    );
    assert_ne!(
        first.unit.segments[0].source_offset,
        second.unit.segments[0].source_offset
    );
    assert_eq!(canonical_ebi_digest(&first), canonical_ebi_digest(&second));
}

fn trust_fixture(
    name: &str,
    release_epoch: u64,
    signing: &SigningKey,
) -> (ElmEbiImage, ElmRustAbiFingerprintV1, ElmEbiProofV1) {
    let fingerprint = ElmRustAbiFingerprintV1::new(
        sha256(b"rustc-test"),
        sha256(b"target-test"),
        sha256(b"kernel-interface-test"),
        1,
        ElmPanicStrategy::AbortThroughRuntime,
        1,
        0,
    );
    let unit = ElmEbiUnit::new(
        ElmManifest::new(
            ElmName::new(name).unwrap(),
            ElmVersion::new("1.0.0").unwrap(),
            ElmKind::Service,
        ),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks());
    let image = ElmEbiImage::new(unit).with_abi_fingerprint(fingerprint.clone());
    let public_key = signing.verifying_key().to_bytes();
    let mut proof = ElmEbiProofV1 {
        source_identifier: "test-fixture".into(),
        source_digest: sha256(b"test-source"),
        subject_digest: canonical_ebi_digest(&image),
        signer_key_id: sha256(&public_key),
        signer_public_key: public_key,
        release_epoch,
        flags: 0,
        signature: [1; 64],
    };
    proof.signature = signing
        .sign(&proof.unsigned_message(&fingerprint))
        .to_bytes();
    (image, fingerprint, proof)
}

#[test]
fn trust_store_commits_epoch_only_after_acceptance() {
    let signing = SigningKey::from_bytes(&[7; 32]);
    let (image, fingerprint, proof) = trust_fixture("signed-cell", 3, &signing);
    let mut store = ElmTrustStore::new();
    store
        .register_anchor(ElmTrustAnchor::new("test-root", proof.signer_public_key).unwrap())
        .unwrap();

    let acceptance = store.verify(&image, &proof, &fingerprint).unwrap();
    assert!(store.accepted_epochs().is_empty());
    store.try_accept(acceptance).unwrap();
    assert_eq!(store.accepted_epochs().len(), 1);
    assert_eq!(store.accepted_epochs()[0].release_epoch(), 3);
}

#[test]
fn proof_signature_message_binds_kernel_api_profile_and_bridge() {
    let signing = SigningKey::from_bytes(&[29; 32]);
    let (_, fingerprint, proof) = trust_fixture("profile-bound-proof", 1, &signing);
    let original = proof.unsigned_message(&fingerprint);
    let mut profile_changed = fingerprint.clone();
    profile_changed.kernel_api_profile_hash = [0x61; 32];
    profile_changed.kernel_api_bridge_abi_version = 1;
    let profile_message = proof.unsigned_message(&profile_changed);
    let mut bridge_changed = profile_changed;
    bridge_changed.kernel_api_bridge_abi_version = 2;

    assert_ne!(original, profile_message);
    assert_ne!(profile_message, proof.unsigned_message(&bridge_changed));
}

#[test]
fn trust_store_reservation_is_explicit_and_balanced() {
    let signing = SigningKey::from_bytes(&[8; 32]);
    let (image, fingerprint, proof) = trust_fixture("reserved-cell", 2, &signing);
    let mut store = ElmTrustStore::new();
    store
        .register_anchor(ElmTrustAnchor::new("reserved-root", proof.signer_public_key).unwrap())
        .unwrap();
    let acceptance = store.verify(&image, &proof, &fingerprint).unwrap();

    assert!(store.reserve_acceptance(&acceptance).unwrap());
    assert_eq!(store.pending_acceptance_slots(), 1);
    store.cancel_acceptance_reservation(true).unwrap();
    assert_eq!(store.pending_acceptance_slots(), 0);
    assert_eq!(
        store.cancel_acceptance_reservation(true),
        Err(ElmTrustError::ReservationMissing)
    );

    assert!(store.reserve_acceptance(&acceptance).unwrap());
    store.accept_reserved(acceptance.clone(), true).unwrap();
    assert_eq!(store.pending_acceptance_slots(), 0);
    assert_eq!(store.accepted_epochs().len(), 1);
    assert!(!store.reserve_acceptance(&acceptance).unwrap());
}

#[test]
fn trust_store_rejects_unknown_and_revoked_signers() {
    let signing = SigningKey::from_bytes(&[9; 32]);
    let (image, fingerprint, proof) = trust_fixture("signed-cell", 1, &signing);
    let mut unknown = ElmTrustStore::new();
    let other = SigningKey::from_bytes(&[10; 32]);
    unknown
        .register_anchor(
            ElmTrustAnchor::new("other-root", other.verifying_key().to_bytes()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        unknown.verify(&image, &proof, &fingerprint),
        Err(ElmTrustError::UnknownSigner)
    );

    let mut revoked = ElmTrustStore::new();
    revoked
        .register_anchor(ElmTrustAnchor::new("test-root", proof.signer_public_key).unwrap())
        .unwrap();
    revoked.revoke(proof.signer_key_id).unwrap();
    assert_eq!(
        revoked.verify(&image, &proof, &fingerprint),
        Err(ElmTrustError::Revoked)
    );
}

#[test]
fn trust_store_rejects_rollback_and_invalid_signature() {
    let signing = SigningKey::from_bytes(&[11; 32]);
    let (image, fingerprint, current) = trust_fixture("signed-cell", 5, &signing);
    let (_, _, previous) = trust_fixture("signed-cell", 4, &signing);
    let mut store = ElmTrustStore::new();
    store
        .register_anchor(ElmTrustAnchor::new("test-root", current.signer_public_key).unwrap())
        .unwrap();
    let acceptance = store.verify(&image, &current, &fingerprint).unwrap();
    store.try_accept(acceptance).unwrap();
    assert_eq!(
        store.verify(&image, &previous, &fingerprint),
        Err(ElmTrustError::Rollback)
    );

    let mut invalid = current.clone();
    invalid.signature[0] ^= 0x80;
    assert_eq!(
        store.verify(&image, &invalid, &fingerprint),
        Err(ElmTrustError::SignatureMismatch)
    );
}

#[test]
fn trust_store_rollback_authority_survives_key_rotation_and_restore() {
    let old_signing = SigningKey::from_bytes(&[12; 32]);
    let new_signing = SigningKey::from_bytes(&[13; 32]);
    let (current_image, fingerprint, current) =
        trust_fixture("rotated-signed-cell", 8, &old_signing);
    let (previous_image, _, previous) = trust_fixture("rotated-signed-cell", 7, &new_signing);
    let old_anchor = ElmTrustAnchor::new_with_rollback_authority(
        "vendor-key-2026-a",
        "vendor.example/kernel-modules",
        current.signer_public_key,
    )
    .unwrap();
    let new_anchor = ElmTrustAnchor::new_with_rollback_authority(
        "vendor-key-2026-b",
        "vendor.example/kernel-modules",
        previous.signer_public_key,
    )
    .unwrap();
    let rollback_authority_id = old_anchor.rollback_authority_id();
    assert_eq!(rollback_authority_id, new_anchor.rollback_authority_id());

    let mut store = ElmTrustStore::new();
    store.register_anchor(old_anchor).unwrap();
    store.register_anchor(new_anchor.clone()).unwrap();
    let acceptance = store
        .verify(&current_image, &current, &fingerprint)
        .unwrap();
    assert_eq!(acceptance.rollback_authority_id(), rollback_authority_id);
    store.try_accept(acceptance.clone()).unwrap();
    assert_eq!(
        store.verify(&previous_image, &previous, &fingerprint),
        Err(ElmTrustError::Rollback)
    );

    let mut restored = ElmTrustStore::new();
    restored.register_anchor(new_anchor).unwrap();
    restored
        .try_accept(
            ElmTrustAcceptance::from_persisted(
                acceptance.signer_key_id(),
                acceptance.rollback_authority_id(),
                acceptance.module_digest(),
                acceptance.release_epoch(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        restored.verify(&previous_image, &previous, &fingerprint),
        Err(ElmTrustError::Rollback)
    );
}

#[test]
fn eki_header_hash_verifies_whole_image() {
    let image = eki_image_with_header_hash(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("hashed-eki", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    assert!(parse_eki_image(&image).is_ok());

    let mut tampered = image.clone();
    let last = tampered.len() - ELM_PROOF_SHA256_LEN - 1;
    tampered[last] ^= 0x1;
    assert_eq!(
        parse_eki_image(&tampered),
        Err(ElmEbiLoadStatus::InvalidUnit)
    );
}

#[test]
fn state_machine_accepts_normal_start_path() {
    let transition = ElmState::Discovered
        .transition_to(ElmState::Verified)
        .unwrap();
    assert_eq!(transition.from, ElmState::Discovered);
    assert_eq!(transition.to, ElmState::Verified);
    assert!(ElmState::Ready.transition_to(ElmState::Active).is_ok());
}

#[test]
fn state_machine_rejects_skipping_binding() {
    assert!(matches!(
        ElmState::Loaded.transition_to(ElmState::Active),
        Err(ElmError::InvalidTransition)
    ));
}

#[test]
fn state_machine_accepts_unplug_paths() {
    assert!(ElmState::Loaded.transition_to(ElmState::Detached).is_ok());
    assert!(ElmState::Paused.transition_to(ElmState::Detached).is_ok());
    assert!(ElmState::Detached.transition_to(ElmState::Retired).is_ok());
}

#[test]
fn state_machine_accepts_lifecycle_fault_paths() {
    assert!(
        ElmState::Discovered
            .transition_to(ElmState::Faulted)
            .is_ok()
    );
    assert!(ElmState::Verified.transition_to(ElmState::Faulted).is_ok());
    assert!(ElmState::Loaded.transition_to(ElmState::Faulted).is_ok());
    assert!(ElmState::Linked.transition_to(ElmState::Faulted).is_ok());
    assert!(ElmState::Ready.transition_to(ElmState::Faulted).is_ok());
    assert!(ElmState::Active.transition_to(ElmState::Faulted).is_ok());
    assert!(ElmState::Quiescing.transition_to(ElmState::Active).is_ok());
    assert!(ElmState::Paused.transition_to(ElmState::Faulted).is_ok());
    assert!(
        ElmState::Faulted
            .transition_to(ElmState::Quarantined)
            .is_ok()
    );
    assert!(
        ElmState::Quarantined
            .transition_to(ElmState::Detached)
            .is_ok()
    );
}

#[test]
fn elm_context_reports_lifecycle_identity() {
    let mut context = ElmContext::new(
        ElmId(7),
        Some(ElmId(1)),
        Generation(3),
        ElmState::Loaded,
        ElmLifecyclePhase::Initialize,
        0,
    );

    assert_eq!(context.cell_id(), ElmId(7));
    assert_eq!(context.parent_id(), Some(ElmId(1)));
    assert_eq!(context.generation(), Generation(3));
    assert_eq!(context.state(), ElmState::Loaded);
    assert_eq!(context.phase(), ElmLifecyclePhase::Initialize);
    assert_eq!(context.flags(), 0);

    context.set_state(ElmState::Active);
    assert_eq!(context.state(), ElmState::Active);
}

#[test]
fn binding_graph_tracks_parent_dependency_and_extension() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("elm-mgr")).unwrap();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph.set_parent(ElmId(2), ElmId(1)).unwrap();
    graph
        .add_extension_point(ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    graph
        .add_extension(ElmId(2), ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    graph
        .add_dependency(ElmId(2), ElmId(1), contract("core.event@1"))
        .unwrap();

    let report = graph.validate().unwrap();
    assert_eq!(report.cells, 2);
    assert_eq!(report.parent_edges, 1);
    assert_eq!(report.dependency_edges, 1);
    assert_eq!(report.extension_edges, 1);

    let parent_edges = graph.parent_edges();
    assert_eq!(parent_edges[0].child, ElmId(2));
    assert_eq!(parent_edges[0].parent, ElmId(1));
    assert_eq!(graph.children_of(ElmId(1)), alloc::vec![ElmId(2)]);
    assert_eq!(graph.dependents_of(ElmId(1)), alloc::vec![ElmId(2)]);
    assert_eq!(graph.extensions_targeting(ElmId(1)), alloc::vec![ElmId(2)]);

    let extension_points = graph.extension_points();
    assert_eq!(extension_points[0].owner, ElmId(1));
    assert_eq!(extension_points[0].name, "menu.item");
}

#[test]
fn binding_graph_tracks_capability_bindings() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("elm-mgr")).unwrap();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph
        .add_capability_binding(
            BindingId(7),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(9)),
        )
        .unwrap();

    let report = graph.validate().unwrap();
    assert_eq!(report.capability_bindings, 1);

    let binding = graph.capability_binding(BindingId(7)).unwrap();
    assert_eq!(binding.consumer, ElmId(2));
    assert_eq!(binding.port, PortId(3));
    assert_eq!(binding.lease, Some(LeaseId(9)));
    assert!(binding.active);
    assert_eq!(
        graph.capability_bindings_for_cell(ElmId(2)),
        alloc::vec![BindingId(7)]
    );
    assert!(
        graph
            .capability_binding_for(ElmId(2), PortId(3), &contract("mgr.menu.item@1"))
            .is_some()
    );
}

#[test]
fn binding_graph_rejects_duplicate_capability_binding() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph
        .add_capability_binding(
            BindingId(7),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(9)),
        )
        .unwrap();

    assert!(matches!(
        graph.add_capability_binding(
            BindingId(8),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(10)),
        ),
        Err(ElmError::DuplicateBinding)
    ));
}

#[test]
fn binding_graph_rejects_parent_cycle() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("root")).unwrap();
    graph.insert_cell(ElmId(2), manifest("child")).unwrap();
    graph.set_parent(ElmId(2), ElmId(1)).unwrap();
    assert!(matches!(
        graph.set_parent(ElmId(1), ElmId(2)),
        Err(ElmError::ParentCycle)
    ));
}

#[test]
fn binding_graph_rejects_dependency_cycle() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("a")).unwrap();
    graph.insert_cell(ElmId(2), manifest("b")).unwrap();
    graph
        .add_dependency(ElmId(1), ElmId(2), contract("test.dep@1"))
        .unwrap();
    assert!(matches!(
        graph.add_dependency(ElmId(2), ElmId(1), contract("test.dep@1")),
        Err(ElmError::DependencyCycle)
    ));
}

#[test]
fn binding_graph_rejects_invalid_extension_contract() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("target")).unwrap();
    graph.insert_cell(ElmId(2), manifest("extension")).unwrap();
    graph
        .add_extension_point(ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    assert!(matches!(
        graph.add_extension(ElmId(2), ElmId(1), "menu.item", contract("mgr.menu.item@2")),
        Err(ElmError::ContractMismatch)
    ));
}

#[test]
fn binding_graph_removes_leaf_cell_edges() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("elm-mgr")).unwrap();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph.set_parent(ElmId(2), ElmId(1)).unwrap();
    graph
        .add_extension_point(ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    graph
        .add_extension(ElmId(2), ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();

    let report = graph.remove_cell(ElmId(2)).unwrap();
    assert_eq!(report.parent_edges, 1);
    assert_eq!(report.extension_edges, 1);
    assert!(graph.cell(ElmId(2)).is_none());
    assert!(graph.validate().is_ok());
}

#[test]
fn binding_graph_removes_capability_binding() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph
        .add_capability_binding(
            BindingId(7),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(9)),
        )
        .unwrap();

    let removed = graph.remove_capability_binding(BindingId(7)).unwrap();
    assert_eq!(removed.consumer, ElmId(2));
    assert!(graph.capability_binding(BindingId(7)).is_none());
    assert!(matches!(
        graph.remove_capability_binding(BindingId(7)),
        Err(ElmError::BindingNotFound)
    ));
}

#[test]
fn lease_registry_revokes_owned_leases() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(ResourceLease::new(
            LeaseId(1),
            ElmId(7),
            LeaseKind::MenuItem,
            LeaseRights::CONTROL,
            Generation::FIRST,
        ))
        .unwrap();
    assert_eq!(registry.revoke_all_owned_by(ElmId(7)).unwrap(), 1);
    assert_eq!(
        registry.get(LeaseId(1)).unwrap().state,
        LeaseState::Revoking
    );
}

#[test]
fn lease_revoke_waits_for_active_refs() {
    let mut lease = ResourceLease::new(
        LeaseId(1),
        ElmId(7),
        LeaseKind::Provider,
        LeaseRights::READ,
        Generation::FIRST,
    );
    lease.active_refs = 1;
    lease.begin_revoke().unwrap();
    assert!(matches!(lease.finish_revoke(), Err(ElmError::LeaseBusy)));
    lease.active_refs = 0;
    lease.finish_revoke().unwrap();
    assert_eq!(lease.state, LeaseState::Revoked);
}

#[test]
fn lease_active_reference_counter_never_saturates() {
    let mut registry = LeaseRegistry::new();
    let id = LeaseId(99);
    registry
        .insert(ResourceLease::new(
            id,
            ElmId(7),
            LeaseKind::Provider,
            LeaseRights::CONTROL,
            Generation::FIRST,
        ))
        .unwrap();
    registry
        .iter_mut()
        .find(|lease| lease.id == id)
        .unwrap()
        .active_refs = usize::MAX;
    assert_eq!(registry.add_active_ref(id), Err(ElmError::LeaseBusy));
    assert_eq!(registry.get(id).unwrap().active_refs, usize::MAX);
}

#[test]
fn lease_registry_revokes_and_removes_owned_leases() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(ResourceLease::new(
            LeaseId(1),
            ElmId(7),
            LeaseKind::MenuItem,
            LeaseRights::CONTROL,
            Generation::FIRST,
        ))
        .unwrap();

    let revoked = registry.revoke_and_remove_owned_by(ElmId(7)).unwrap();
    assert_eq!(revoked, alloc::vec![LeaseId(1)]);
    assert!(registry.get(LeaseId(1)).is_none());
    assert!(registry.is_empty());
}

#[test]
fn lease_registry_busy_batch_revoke_is_atomic() {
    let mut registry = LeaseRegistry::new();
    for id in [LeaseId(1), LeaseId(2)] {
        registry
            .insert(ResourceLease::new(
                id,
                ElmId(7),
                LeaseKind::Provider,
                LeaseRights::CONTROL,
                Generation::FIRST,
            ))
            .unwrap();
    }
    registry.get_mut(LeaseId(2)).unwrap().active_refs = 1;

    assert_eq!(
        registry.revoke_and_remove_owned_by(ElmId(7)),
        Err(ElmError::LeaseBusy)
    );
    assert_eq!(registry.get(LeaseId(1)).unwrap().state, LeaseState::Active);
    assert_eq!(registry.get(LeaseId(2)).unwrap().state, LeaseState::Active);
    assert_eq!(
        registry.revoke_and_remove(LeaseId(2)),
        Err(ElmError::LeaseBusy)
    );
    assert_eq!(registry.get(LeaseId(2)).unwrap().state, LeaseState::Active);
}

#[test]
fn lease_registry_reports_busy_owned_leases() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(ResourceLease::new(
            LeaseId(1),
            ElmId(7),
            LeaseKind::Provider,
            LeaseRights::READ,
            Generation::FIRST,
        ))
        .unwrap();

    assert_eq!(registry.busy_owned_by(ElmId(7)), 0);
    registry.get_mut(LeaseId(1)).unwrap().active_refs = 2;
    assert_eq!(registry.busy_owned_by(ElmId(7)), 1);
    assert_eq!(registry.busy_owned_by(ElmId(8)), 0);
}

#[test]
fn lease_registry_tracks_binding_owner() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(
            ResourceLease::new(
                LeaseId(1),
                ElmId(7),
                LeaseKind::MenuItem,
                LeaseRights::CONTROL,
                Generation::FIRST,
            )
            .with_binding(BindingId(9)),
        )
        .unwrap();

    assert_eq!(
        registry.get_by_binding(BindingId(9)).map(|lease| lease.id),
        Some(LeaseId(1))
    );
    assert_eq!(registry.revoke_and_remove(LeaseId(1)).unwrap(), LeaseId(1));
    assert!(registry.get(LeaseId(1)).is_none());
}

#[test]
fn runtime_port_lease_tracks_write_rights() {
    let lease = ResourceLease::new(
        LeaseId(5),
        ElmId(7),
        LeaseKind::RuntimePort,
        LeaseRights::WRITE,
        Generation::FIRST,
    )
    .with_binding(BindingId(9));

    assert_eq!(lease.kind, LeaseKind::RuntimePort);
    assert!(lease.rights.write);
    assert!(!lease.rights.control);
    assert_eq!(lease.binding, Some(BindingId(9)));
}

#[test]
fn ctl_command_rejects_unknown_command() {
    assert_eq!(ElmCtlCommand::from_raw(1), Some(ElmCtlCommand::CoreQuery));
    assert_eq!(ElmCtlCommand::from_raw(509), None);
}

#[test]
fn core_info_reports_capabilities_and_counts() {
    let info = ElmCoreInfo::new(1, 15, 0, 7);
    assert_eq!(info.cell_count, 1);
    assert_eq!(info.port_count, 15);
    assert_eq!(info.event_sequence, 7);
    assert_ne!(info.capabilities, 0);
}

#[test]
fn snapshot_entries_truncate_names_safely() {
    let cell = ElmCellSnapshot::new(
        ElmId(1),
        None,
        ElmState::Active,
        ElmKind::Manager,
        Generation::FIRST,
        "elm-mgr",
        ElmEbiArch::Any,
        ElmEbiLoadStatus::Ok,
        false,
        ElmEbiSourceKind::Builtin,
        0,
        0,
        0,
        true,
        true,
        true,
        false,
        ElmResourceBudget::ROOT,
        ElmResourceUsage::default(),
        false,
        0,
        0,
        false,
        [0; 32],
        0,
    );
    assert_eq!(cell.id, 1);
    assert_eq!(cell.parent, 0);
    assert_eq!(cell.name_len, 7);
    assert_eq!(&cell.name[..7], b"elm-mgr");
    assert_eq!(cell.ebi_source, ElmEbiSourceKind::Builtin as u32);
    assert_ne!(
        cell.lifecycle_flags & crate::ELM_CELL_LIFECYCLE_HOOKS_DECLARED,
        0
    );
    assert_eq!(
        cell.budget_max_provider_ports,
        ElmResourceBudget::ROOT.max_provider_ports
    );
    assert_eq!(cell.usage_provider_ports, 0);
    assert_eq!(cell.isolated, 0);
    assert_eq!(cell.trust_flags, crate::ELM_CELL_TRUST_INTERNAL);
    assert_eq!(cell.release_epoch, 0);

    let port = ElmPortSnapshot::new(
        crate::PortId(1),
        None,
        "core.log@1",
        crate::FlowDirection::Sink,
        FlowMode::Shared,
        true,
    );
    assert_eq!(port.contract_len, 10);
    assert_eq!(&port.contract[..10], b"core.log@1");
}

#[test]
fn snapshot_header_uses_fixed_entry_sizes() {
    let header = ElmSnapshotHeader::new(1, 15, 0, 9);
    assert_eq!(header.cell_count, 1);
    assert_eq!(header.port_count, 15);
    assert!(header.cell_entry_size as usize >= core::mem::size_of::<ElmCellSnapshot>());
    assert!(header.port_entry_size as usize >= core::mem::size_of::<ElmPortSnapshot>());
}

#[test]
fn builtin_ports_include_only_elm_owned_ports() {
    let ports = builtin_port_descriptors();
    assert_eq!(ports.len(), 4);
    assert!(ports.iter().any(|port| {
        port.contract == "core.log@1" && port.id == crate::PortId(1) && port.implemented
    }));
    assert!(ports.iter().any(|port| {
        port.contract == "core.event@1" && port.id == crate::PortId(2) && port.implemented
    }));
    assert!(ports.iter().any(|port| port.contract == "mgr.menu.item@1"));
    assert!(ports.iter().any(|port| {
        port.contract == "mgr.action.invoke@1"
            && port.id == crate::PortId(4)
            && port.owner == Some(ELM_MGR_BUILTIN_ID)
            && port.implemented
            && port.invokable
    }));
    assert!(ports.iter().all(|port| port.implemented));
    assert!(
        !ports
            .iter()
            .any(|port| port.contract == "device.discovered@1")
    );
}

#[test]
fn kernel_provider_spec_builds_api_and_unsupported_reply() {
    let spec = crate::ElmKernelProviderSpec::subsystem_todo(
        "elm.test",
        "provider",
        "elm.test.provider@1",
        "test.provider@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    );
    let api = spec.api_descriptor(100, ELM_MGR_BUILTIN_ID);
    assert_eq!(api.id, 100);
    assert_eq!(api.owner_cell_id, ELM_MGR_BUILTIN_ID.0);
    assert_ne!(api.flags & crate::ELM_MGR_API_FLAG_PROVIDER_OPS, 0);
    assert_ne!(api.flags & crate::ELM_MGR_API_FLAG_TODO, 0);

    let port = spec.port_descriptor(crate::PortId(100), ELM_MGR_BUILTIN_ID);
    assert_eq!(port.contract, "test.provider@1");
    assert_eq!(port.owner, Some(ELM_MGR_BUILTIN_ID));
    assert!(port.implemented);
    assert!(port.invokable);

    let frame = ElmCallFrame::empty(7, 9, 0);
    let reply = (spec.invoke)(frame);
    assert_eq!(reply.binding_id, 7);
    assert_eq!(reply.call_id, 9);
    assert_eq!(reply.status, crate::ELM_CALL_STATUS_UNSUPPORTED);
}

#[test]
fn event_record_uses_zero_for_absent_objects() {
    let event = ElmEventRecord::new(
        crate::ElmEventSequence(3),
        TopologyEventKind::CellAdded,
        Some(ElmId(9)),
        None,
        None,
        None,
    );
    assert_eq!(event.sequence, 3);
    assert_eq!(event.cell, 9);
    assert_eq!(event.port, 0);
    assert_eq!(event.binding, 0);
    assert_eq!(event.lease, 0);
}

#[test]
fn mgr_response_header_reports_payload_status() {
    assert_eq!(ELM_MGR_MAX_PAYLOAD, 256 * 1024);
    assert_eq!(
        ELM_MGR_MAX_INPUT,
        ELM_MGR_MAX_PAYLOAD + core::mem::size_of::<ElmMgrCallHeader>()
    );

    let query_policy = ElmMgrCallHeader::empty(ElmMgrCallKind::QueryPolicy);
    assert_eq!(query_policy.kind, ElmMgrCallKind::QueryPolicy as u32);
    assert_eq!(query_policy.flags, 0);
    assert_eq!(query_policy.payload_len, 0);
    assert_eq!(query_policy.reserved, 0);

    let preflight_bind = ElmMgrCallHeader::new(ElmMgrCallKind::PreflightBind, 88);
    assert_eq!(preflight_bind.kind, ElmMgrCallKind::PreflightBind as u32);
    assert_eq!(preflight_bind.flags, 0);
    assert_eq!(preflight_bind.payload_len, 88);
    assert_eq!(preflight_bind.reserved, 0);

    let ok = ElmMgrResponseHeader::ok(16);
    assert_eq!(ok.status, ELM_MGR_STATUS_OK);
    assert_eq!(ok.payload_len, 16);

    let invalid = ElmMgrResponseHeader::invalid();
    assert_eq!(invalid.status, ELM_MGR_STATUS_INVALID);
    assert_eq!(invalid.payload_len, 0);
}

#[test]
fn runtime_mgr_call_kinds_are_stable() {
    assert_eq!(
        ElmMgrCallKind::from_raw(16),
        Some(ElmMgrCallKind::SubmitRuntimeLog)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(17),
        Some(ElmMgrCallKind::ReadRuntimeEvent)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(18),
        Some(ElmMgrCallKind::AckRuntimeEvent)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(19),
        Some(ElmMgrCallKind::QueryRuntimePorts)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(26),
        Some(ElmMgrCallKind::SubmitProviderCall)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(27),
        Some(ElmMgrCallKind::PollProviderReply)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(28),
        Some(ElmMgrCallKind::CancelProviderCall)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(29),
        Some(ElmMgrCallKind::QueryProviderQueue)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(30),
        Some(ElmMgrCallKind::QueryApiRegistry)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(31),
        Some(ElmMgrCallKind::SubscribeEvent)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(32),
        Some(ElmMgrCallKind::UnsubscribeEvent)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(33),
        Some(ElmMgrCallKind::QueryEventSubscriptions)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(34),
        Some(ElmMgrCallKind::ReadSubscribedEvents)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(35),
        Some(ElmMgrCallKind::QueryProviderSnapshot)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(36),
        Some(ElmMgrCallKind::QueryNativeCapabilities)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(37),
        Some(ElmMgrCallKind::QueryTodoRegistry)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(56),
        Some(ElmMgrCallKind::BeginImageSession)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(57),
        Some(ElmMgrCallKind::WriteImageSession)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(58),
        Some(ElmMgrCallKind::SealImageSession)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(59),
        Some(ElmMgrCallKind::AbortImageSession)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(60),
        Some(ElmMgrCallKind::QueryImageSession)
    );
}

#[test]
fn image_session_abi_is_fixed_layout() {
    let digest = crate::sha256(b"image-session");
    let begin = crate::ElmImageSessionBeginRequestV1::new(4096, 5000, digest);
    assert_eq!(begin.abi_version, crate::ELM_IMAGE_SESSION_ABI_VERSION);
    assert_eq!(begin.hash_alg, crate::ELM_IMAGE_SESSION_HASH_SHA256);
    assert_eq!(core::mem::size_of_val(&begin), 64);

    let write = crate::ElmImageSessionWriteRequestV1::new(7, 64, 32);
    assert_eq!(write.session_id, 7);
    assert_eq!(core::mem::size_of_val(&write), 32);
    assert_eq!(core::mem::size_of::<crate::ElmImageSessionRequestV1>(), 16);
    assert_eq!(core::mem::size_of::<crate::ElmImageSessionInfoV1>(), 120);
    assert_eq!(
        core::mem::size_of::<crate::ElmImageSessionReferenceV1>(),
        16
    );
    assert_eq!(crate::ELM_IMAGE_SESSION_MAX_CHUNK, 64 * 1024);
}

#[test]
fn kernel_interface_manifest_tracks_layout_and_target() {
    let riscv = crate::kernel_interface_manifest_v1(crate::ElmEbiArch::Riscv64 as u32);
    let loongarch = crate::kernel_interface_manifest_v1(crate::ElmEbiArch::LoongArch64 as u32);
    assert_ne!(
        crate::sha256(riscv.as_bytes()),
        crate::sha256(loongarch.as_bytes())
    );
    assert!(riscv.contains("type=ElmApiRootV1 size="));
    assert!(riscv.contains("field=ElmRuntimeApiV1.invoke_managed offset="));
    assert!(riscv.contains("fn.abort-current=extern-C(u32)->never"));
    assert!(riscv.ends_with('\n'));
}

#[test]
fn runtime_log_request_truncates_fixed_message_buffer() {
    let long = "x".repeat(ELM_RUNTIME_LOG_MESSAGE_LEN + 7);
    let request = ElmRuntimeLogRequest::new(11, 6, &long);
    assert_eq!(request.binding_id, 11);
    assert_eq!(request.level, 6);
    assert_eq!(request.message_len, ELM_RUNTIME_LOG_MESSAGE_LEN as u16);
    assert_eq!(request.message[0], b'x');
    assert_eq!(
        core::mem::size_of::<ElmRuntimeLogRequest>(),
        24 + ELM_RUNTIME_LOG_MESSAGE_LEN
    );

    let response = ElmRuntimeLogResponse::new(11, request.message_len as u32, ELM_MGR_STATUS_OK, 1);
    assert_eq!(response.accepted_len, ELM_RUNTIME_LOG_MESSAGE_LEN as u32);
    assert_eq!(core::mem::size_of::<ElmRuntimeLogResponse>(), 32);
}

#[test]
fn runtime_event_and_stats_records_are_fixed_layout() {
    let request = ElmRuntimeEventRequest::new(12, 9);
    assert_eq!(request.binding_id, 12);
    assert_eq!(request.cursor, 9);
    assert_eq!(core::mem::size_of::<ElmRuntimeEventRequest>(), 24);

    let event = ElmEventRecord::new(
        crate::ElmEventSequence(10),
        TopologyEventKind::BindingAdded,
        None,
        None,
        Some(BindingId(12)),
        None,
    );
    let response = ElmRuntimeEventResponse::with_event(12, 9, event, 0, ELM_MGR_STATUS_OK);
    assert_eq!(response.has_event, 1);
    assert_eq!(response.next_cursor, 10);
    assert_eq!(core::mem::size_of::<ElmRuntimeEventResponse>(), 88);

    let empty = ElmRuntimeEventResponse::empty(12, 10, 0, ELM_MGR_STATUS_OK);
    assert_eq!(empty.has_event, 0);
    assert_eq!(empty.event.sequence, 0);

    let header = ElmRuntimePortStatsHeader::new(1, 10);
    assert_eq!(header.record_count, 1);
    assert_eq!(header.event_sequence, 10);
    assert_eq!(core::mem::size_of::<ElmRuntimePortStatsHeader>(), 16);

    let record = ElmRuntimePortStatsRecord::new(12, 7, 2, 13, 10, 3, 4, 1);
    assert_eq!(record.binding_id, 12);
    assert_eq!(record.port_id, 2);
    assert_eq!(record.cursor, 10);
    assert_eq!(core::mem::size_of::<ElmRuntimePortStatsRecord>(), 72);
}

#[test]
fn mgr_api_and_event_subscription_records_are_fixed_layout() {
    let descriptor = ElmMgrApiDescriptor::new(
        7,
        ELM_MGR_BUILTIN_ID.0,
        ELM_MGR_API_KIND_EVENT,
        ELM_MGR_API_FLAG_STABLE,
        ElmMgrCallKind::SubscribeEvent as u32,
        "elm.mgr",
        "event.subscribe",
        "elm.mgr.event.subscribe@1",
    );
    assert_eq!(descriptor.id, 7);
    assert_eq!(descriptor.owner_cell_id, ELM_MGR_BUILTIN_ID.0);
    assert_eq!(descriptor.call_kind, 31);
    assert_eq!(descriptor.contract_len as usize, 25);
    assert!(descriptor.contract_len as usize <= ELM_MGR_API_CONTRACT_LEN);
    assert_eq!(core::mem::size_of::<ElmMgrApiDescriptor>(), 176);

    let registry = ElmMgrApiRegistryHeader::new(3, 9);
    assert_eq!(registry.record_count, 3);
    assert_eq!(
        registry.record_entry_size as usize,
        core::mem::size_of::<ElmMgrApiDescriptor>()
    );
    assert_eq!(core::mem::size_of::<ElmMgrApiRegistryHeader>(), 24);

    let request = ElmMgrEventSubscribeRequest::new(ELM_MGR_BUILTIN_ID.0);
    assert_eq!(request.owner_cell_id, ELM_MGR_BUILTIN_ID.0);
    assert_eq!(request.kind_filter, 0);
    assert_eq!(core::mem::size_of::<ElmMgrEventSubscribeRequest>(), 48);

    let response = ElmMgrEventSubscribeResponse::new(1, 100, ELM_MGR_BUILTIN_ID.0, 8, 0, 0);
    assert_eq!(response.subscription_id, 1);
    assert_eq!(response.flags & 1, 1);
    assert_eq!(core::mem::size_of::<ElmMgrEventSubscribeResponse>(), 48);

    let unsubscribe = ElmMgrEventUnsubscribeRequest::new(1, ELM_MGR_BUILTIN_ID.0);
    assert_eq!(unsubscribe.subscription_id, 1);
    assert_eq!(core::mem::size_of::<ElmMgrEventUnsubscribeRequest>(), 24);

    let unsubscribe_response =
        ElmMgrEventUnsubscribeResponse::new(1, 100, ELM_MGR_BUILTIN_ID.0, 0, true, 2, 0);
    assert_eq!(unsubscribe_response.revoked, 1);
    assert_eq!(core::mem::size_of::<ElmMgrEventUnsubscribeResponse>(), 48);

    let sub_header = ElmMgrEventSubscriptionHeader::new(1, 8);
    assert_eq!(
        sub_header.record_entry_size as usize,
        core::mem::size_of::<ElmMgrEventSubscriptionRecord>()
    );
    assert_eq!(core::mem::size_of::<ElmMgrEventSubscriptionHeader>(), 16);

    let sub_record = ElmMgrEventSubscriptionRecord::new(
        1,
        ELM_MGR_BUILTIN_ID.0,
        100,
        8,
        2,
        true,
        ELM_MGR_BUILTIN_ID.0,
        0,
        0,
        0,
        3,
        0,
    );
    assert_eq!(sub_record.cursor, 8);
    assert_eq!(sub_record.flags & 1, 1);
    assert_eq!(core::mem::size_of::<ElmMgrEventSubscriptionRecord>(), 88);

    let read_request = ElmMgrSubscribedEventReadRequest::new(1, 8, 4);
    assert_eq!(read_request.flags, ELM_MGR_EVENT_READ_FLAG_ADVANCE);
    assert_eq!(core::mem::size_of::<ElmMgrSubscribedEventReadRequest>(), 24);

    let read_header = ElmMgrSubscribedEventReadHeader::new(0, 0, 0, 1, 8, 8, 0);
    assert_eq!(
        read_header.record_entry_size as usize,
        core::mem::size_of::<ElmEventRecord>()
    );
    assert_eq!(core::mem::size_of::<ElmMgrSubscribedEventReadHeader>(), 48);
}

#[test]
fn todo_registry_records_are_fixed_layout() {
    let header = ElmTodoRegistryHeader::new_with_flags(2, 1, ELM_TODO_REGISTRY_FLAG_TRUNCATED, 9);
    assert_eq!(header.record_count, 2);
    assert_eq!(header.active_count, 1);
    assert_eq!(header.flags, ELM_TODO_REGISTRY_FLAG_TRUNCATED);
    assert_eq!(
        header.record_entry_size as usize,
        core::mem::size_of::<ElmTodoRegistryRecord>()
    );
    assert_eq!(core::mem::size_of::<ElmTodoRegistryHeader>(), 24);

    let record = ElmTodoRegistryRecord::new(
        ELM_TODO_KIND_RUNTIME,
        ELM_TODO_FLAG_STATIC | ELM_TODO_FLAG_ACTIVE,
        ELM_POLICY_BLOCK_PROVIDER_BUSY,
        7,
        ELM_MGR_STATUS_BUSY,
        "runtime.provider_in_flight",
        "运行中的 provider 调用已支持取消意图",
    );
    assert_eq!(record.kind, ELM_TODO_KIND_RUNTIME);
    assert_eq!(record.flags & ELM_TODO_FLAG_ACTIVE, ELM_TODO_FLAG_ACTIVE);
    assert_eq!(record.subject_id, 7);
    assert_eq!(record.status, ELM_MGR_STATUS_BUSY);
    assert!(record.name_len as usize <= ELM_TODO_NAME_LEN);
    assert!(record.detail_len as usize <= ELM_TODO_DETAIL_LEN);
    assert_eq!(core::mem::size_of::<ElmTodoRegistryRecord>(), 232);
}

#[test]
fn lifecycle_request_and_response_are_fixed_layout() {
    let request = ElmLifecycleRequest::new(7);
    assert_eq!(request.cell_id, 7);
    assert_eq!(core::mem::size_of::<ElmLifecycleRequest>(), 16);

    let response = ElmLifecycleResponse::new(
        7,
        ELM_MGR_STATUS_OK,
        state_code(ElmState::Paused),
        1,
        2,
        ELM_LIFECYCLE_REASON_NONE,
    );
    assert_eq!(response.cell_id, 7);
    assert_eq!(response.final_state, state_code(ElmState::Paused));
    assert_eq!(response.revoked_leases, 1);
    assert_eq!(response.removed_menu_items, 2);
    assert_eq!(core::mem::size_of::<ElmLifecycleResponse>(), 32);
}

#[test]
fn lifecycle_plan_and_mgr_policy_are_fixed_layout() {
    assert_eq!(
        ElmLifecycleAction::from_raw(ElmLifecycleAction::Detach as u32),
        Some(ElmLifecycleAction::Detach)
    );
    assert_eq!(ElmLifecycleAction::Detach.bit(), ELM_MGR_ACTION_DETACH);

    let request = ElmLifecyclePlanRequest::new(7, ElmLifecycleAction::Pause);
    assert_eq!(request.cell_id, 7);
    assert_eq!(request.action, ElmLifecycleAction::Pause as u32);
    assert_eq!(core::mem::size_of::<ElmLifecyclePlanRequest>(), 16);

    let response = ElmLifecyclePlanResponse::new(
        7,
        ElmLifecycleAction::Detach,
        false,
        ELM_MGR_STATUS_BUSY,
        state_code(ElmState::Active),
        ELM_POLICY_BLOCK_HAS_DEPENDENTS,
    )
    .with_affected(0, 1, 0);
    assert_eq!(response.allowed, 0);
    assert_eq!(response.affected_dependents, 1);
    assert_eq!(
        first_lifecycle_reason(response.blockers),
        ELM_LIFECYCLE_REASON_HAS_DEPENDENTS
    );
    assert_eq!(status_from_blockers(response.blockers), ELM_MGR_STATUS_BUSY);
    assert_eq!(core::mem::size_of::<ElmLifecyclePlanResponse>(), 48);

    let policy = ElmMgrPolicyInfo::new(128);
    assert_eq!(policy.audit_capacity, 128);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_PREFLIGHT, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_AUDIT, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_NEXUS_BINDING, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_MENU_BINDING, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_PROVIDER_PORTS, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_HEALTH, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_API_REGISTRY, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_NATIVE_CAPABILITIES, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_TODO_REGISTRY, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_TRUST, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_RESOURCE_BUDGET, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_HOT_REPLACE, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_NATIVE_LIFECYCLE, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_BIND, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_UNBIND, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_REPLACE, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_HEALTH_QUERY, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_EVENT_SUBSCRIBE, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_EVENT_READ, 0);
    assert_ne!(
        policy.supported_actions & ELM_MGR_ACTION_NATIVE_CAPABILITY_QUERY,
        0
    );
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_TODO_QUERY, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_TRUST_QUERY, 0);
    assert_ne!(policy.blocker_mask & ELM_POLICY_BLOCK_RESOURCE_QUOTA, 0);

    let replace = ElmReplaceCellRequestV1::new(7, ElmEbiSourceKind::Projection as u16, 128);
    assert_eq!(replace.abi_version, ELM_REPLACE_CELL_ABI_VERSION);
    assert_eq!(replace.target_cell_id, 7);
    assert_eq!(replace.source_kind, ElmEbiSourceKind::Projection as u16);
    assert_eq!(replace.migration_limit, 0);
    assert_eq!(replace.source_payload_len, 128);
    let replace = replace.with_privileged_symbol_authorization();
    assert!(replace.authorizes_privileged_symbols());
    assert_eq!(
        replace.flags,
        crate::ELM_REPLACE_CELL_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS
    );
    assert_eq!(core::mem::size_of::<ElmReplaceCellRequestV1>(), 32);
    assert_eq!(ELM_REPLACE_MIGRATION_STATE_MAX, 64 * 1024);

    let replace_response = ElmReplaceCellResponseV1::new(
        7,
        ELM_MGR_STATUS_OK,
        state_code(ElmState::Active),
        2,
        16,
        ELM_LIFECYCLE_REASON_NONE,
        0,
    );
    assert_eq!(replace_response.cell_id, 7);
    assert_eq!(replace_response.generation, 2);
    assert_eq!(replace_response.migrated_len, 16);
    assert_eq!(core::mem::size_of::<ElmReplaceCellResponseV1>(), 40);
}

#[test]
fn resource_budget_model_is_stable() {
    assert!(
        ElmResourceBudget::ROOT.max_provider_ports > ElmResourceBudget::DEFAULT.max_provider_ports
    );
    assert!(ElmResourceBudget::DEFAULT.max_native_faults != 0);
    assert!(
        ElmResourceBudget::BUILD_BOUND.max_provider_queue
            < ElmResourceBudget::DEFAULT.max_provider_queue
    );
    assert!(
        u128::from(ElmResourceBudget::BUILD_BOUND.cpu_budget_ns_per_period)
            * u128::from(ElmResourceBudget::DEFAULT.cpu_period_ns)
            < u128::from(ElmResourceBudget::DEFAULT.cpu_budget_ns_per_period)
                * u128::from(ElmResourceBudget::BUILD_BOUND.cpu_period_ns)
    );

    let usage = ElmResourceUsage {
        provider_ports: 1,
        provider_queue: 2,
        event_subscriptions: 3,
        pending_loads: 4,
        native_images: 5,
        native_faults: 6,
        audit_records: 7,
        ..ElmResourceUsage::default()
    };
    assert_eq!(usage.provider_queue, 2);
    assert_eq!(ElmResourceKind::NativeFault, ElmResourceKind::NativeFault);
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_RESOURCE_QUOTA),
        ELM_MGR_STATUS_BUSY
    );
}

#[test]
fn call_frame_abi_is_fixed_layout() {
    assert_eq!(ELM_ACTION_OPCODE_INVOKE, 1);
    assert_eq!(ELM_ACTION_RESULT_HEALTH, 1);

    let action_request = ElmActionInvokeRequest::new(100);
    assert_eq!(action_request.action_id, 100);
    assert_eq!(action_request.flags, 0);
    assert_eq!(action_request.reserved, 0);
    assert_eq!(core::mem::size_of::<ElmActionInvokeRequest>(), 16);

    let action_reply = ElmActionInvokeReply::health(100, 101, 1, ELM_MGR_STATUS_OK, 9);
    assert_eq!(action_reply.action_id, 100);
    assert_eq!(action_reply.menu_item_id, 101);
    assert_eq!(action_reply.owner_cell_id, 1);
    assert_eq!(action_reply.result_kind, ELM_ACTION_RESULT_HEALTH);
    assert_eq!(action_reply.result_code, ELM_MGR_STATUS_OK);
    assert_eq!(action_reply.event_sequence, 9);
    assert_eq!(core::mem::size_of::<ElmActionInvokeReply>(), 48);

    let frame = ElmCallFrame::new(11, 9, 1, b"hello");
    assert_eq!(frame.binding_id, 11);
    assert_eq!(frame.call_id, 9);
    assert_eq!(frame.opcode, 1);
    assert_eq!(frame.payload_len, 5);
    assert_eq!(&frame.payload[..5], b"hello");
    assert_eq!(core::mem::size_of::<ElmCallFrame>(), 288);

    let reply = ElmReplyFrame::new(11, 9, ELM_MGR_STATUS_OK, b"world");
    assert_eq!(reply.binding_id, 11);
    assert_eq!(reply.call_id, 9);
    assert_eq!(reply.status, ELM_MGR_STATUS_OK);
    assert_eq!(reply.payload_len, 5);
    assert_eq!(&reply.payload[..5], b"world");
    assert_eq!(core::mem::size_of::<ElmReplyFrame>(), 288);

    let native_call = ElmNativeProviderCallV1::new(1, 2, 3, frame);
    assert_eq!(ELM_NATIVE_PROVIDER_CALL_ABI_VERSION, 1);
    assert_eq!(
        native_call.abi_version,
        ELM_NATIVE_PROVIDER_CALL_ABI_VERSION
    );
    assert_eq!(native_call.cell_id, 1);
    assert_eq!(native_call.port_id, 2);
    assert_eq!(native_call.lease_id, 3);
    assert_eq!(native_call.binding_id, 11);
    assert_eq!(
        native_call.reply.status,
        crate::ELM_CALL_STATUS_PROVIDER_FAULT
    );
    assert_eq!(core::mem::size_of::<ElmNativeProviderCallV1>(), 616);

    let entry =
        ElmNativeEntryFrameV1::new(7, ELM_MGR_BUILTIN_ID.0, 3, state_code(ElmState::Loaded));
    assert_eq!(entry.abi_version, ELM_NATIVE_ENTRY_ABI_VERSION);
    assert_eq!(entry.cell_id, 7);
    assert_eq!(entry.parent_id, ELM_MGR_BUILTIN_ID.0);
    assert_eq!(entry.exit_code, 0);
    assert_eq!(core::mem::size_of::<ElmNativeEntryFrameV1>(), 48);

    let snapshot = ElmNativeProviderSnapshotV1::new(7, 100, 11, 12, 0x1000, 256);
    assert_eq!(
        snapshot.abi_version,
        ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION
    );
    assert_eq!(snapshot.flags, 0);
    assert_eq!(snapshot.reserved2, 0);
    assert_eq!(snapshot.cell_id, 7);
    assert_eq!(snapshot.port_id, 100);
    assert_eq!(snapshot.binding_id, 11);
    assert_eq!(snapshot.payload_addr, 0x1000);
    assert_eq!(snapshot.capacity, 256);
    assert_eq!(ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED, 1);
    assert_eq!(ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE, 2);
    assert_eq!(ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK, 3);
    assert_eq!(core::mem::size_of::<ElmNativeProviderSnapshotV1>(), 72);
}

#[test]
fn nexus_binding_abi_records_are_fixed_layout() {
    let request = ElmNexusBindRequest::new(7, 3, "mgr.menu.item@1");
    assert_eq!(request.cell_id, 7);
    assert_eq!(request.port_id, 3);
    assert_eq!(request.contract_len, "mgr.menu.item@1".len() as u16);
    assert_eq!(request.contract.len(), ELM_NEXUS_CONTRACT_LEN);
    assert_eq!(core::mem::size_of::<ElmNexusBindRequest>(), 88);

    let response = ElmNexusBindPlanResponse::new(7, 3, 11, 12, 1, true, ELM_MGR_STATUS_OK, 0);
    assert_eq!(response.allowed, 1);
    assert_eq!(response.binding_id, 11);
    assert_eq!(response.lease_id, 12);
    assert_eq!(core::mem::size_of::<ElmNexusBindPlanResponse>(), 64);

    let unbind = ElmNexusUnbindRequest::new(11);
    assert_eq!(unbind.binding_id, 11);
    assert_eq!(core::mem::size_of::<ElmNexusUnbindRequest>(), 16);

    let header = ElmNexusBindingSnapshotHeader::new(1, 9);
    assert_eq!(header.binding_count, 1);
    assert_eq!(header.event_sequence, 9);
    assert_eq!(core::mem::size_of::<ElmNexusBindingSnapshotHeader>(), 16);

    let record = ElmNexusBindingRecord::new(11, 7, 3, 12, 1, true, "mgr.menu.item@1");
    assert_eq!(record.active, 1);
    assert_eq!(record.contract_len, "mgr.menu.item@1".len() as u16);
    assert_eq!(core::mem::size_of::<ElmNexusBindingRecord>(), 120);
}

#[test]
fn provider_port_abi_records_are_fixed_layout() {
    assert_eq!(
        ElmMgrCallKind::from_raw(20),
        Some(ElmMgrCallKind::RegisterProviderPort)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(24),
        Some(ElmMgrCallKind::QueryProviderStats)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(25),
        Some(ElmMgrCallKind::QueryHealth)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(35),
        Some(ElmMgrCallKind::QueryProviderSnapshot)
    );
    assert_eq!(ELM_PROVIDER_FLAG_DYNAMIC, 1);
    assert_eq!(ELM_PROVIDER_FLAG_KERNEL_BACKEND, 2);
    assert_eq!(ELM_PROVIDER_FLAG_TODO_BACKEND, 4);
    assert_eq!(ELM_PROVIDER_FLAG_NATIVE_BACKEND, 8);

    let request = ElmProviderPortRegisterRequest::new(
        1,
        "demo.provider@1",
        ElmPortAccessPolicy::ExtensionOnly,
        FlowDirection::Control,
        FlowMode::Shared,
        ELM_PROVIDER_PORT_FLAG_NONE,
    );
    assert_eq!(request.owner_cell_id, 1);
    assert_eq!(
        request.access_policy,
        ElmPortAccessPolicy::ExtensionOnly as u32
    );
    assert_eq!(request.direction, FlowDirection::Control as u32);
    assert_eq!(request.mode, FlowMode::Shared as u32);
    assert_eq!(request.contract_len, "demo.provider@1".len() as u16);
    assert_eq!(core::mem::size_of::<ElmProviderPortRegisterRequest>(), 96);

    let response = ElmProviderPortRegisterResponse::new(
        1,
        100,
        ELM_MGR_STATUS_OK,
        ElmPortAccessPolicy::ExtensionOnly as u32,
        0,
    );
    assert_eq!(response.port_id, 100);
    assert_eq!(core::mem::size_of::<ElmProviderPortRegisterResponse>(), 40);

    let unregister = ElmProviderPortUnregisterRequest::new(100);
    assert_eq!(unregister.port_id, 100);
    assert_eq!(core::mem::size_of::<ElmProviderPortUnregisterRequest>(), 16);

    let invoke = ElmProviderInvokeRequest::new(ElmCallFrame::new(7, 1, 1, b"abc"));
    assert_eq!(invoke.frame.binding_id, 7);
    assert_eq!(core::mem::size_of::<ElmProviderInvokeRequest>(), 288);

    let invoke_response =
        ElmProviderInvokeResponse::new(ElmReplyFrame::new(7, 1, ELM_MGR_STATUS_OK, b"abc"));
    assert_eq!(invoke_response.reply.status, ELM_MGR_STATUS_OK);
    assert_eq!(core::mem::size_of::<ElmProviderInvokeResponse>(), 288);

    let snapshot_request = ElmProviderSnapshotRequest::by_binding(7);
    assert_eq!(snapshot_request.binding_id, 7);
    assert_eq!(snapshot_request.port_id, 0);
    assert_eq!(snapshot_request.flags, 0);
    assert_eq!(snapshot_request.cursor(), 0);
    assert!(!snapshot_request.is_paged());
    let snapshot_request = ElmProviderSnapshotRequest::by_binding_paged(7, 3);
    assert_eq!(snapshot_request.binding_id, 7);
    assert_eq!(snapshot_request.port_id, 0);
    assert_eq!(
        snapshot_request.flags,
        ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED
    );
    assert_eq!(snapshot_request.cursor(), 3);
    assert!(snapshot_request.is_paged());
    let snapshot_request = ElmProviderSnapshotRequest::by_port_paged(100, 4);
    assert_eq!(snapshot_request.port_id, 100);
    assert_eq!(snapshot_request.binding_id, 0);
    assert_eq!(snapshot_request.cursor(), 4);
    assert_eq!(ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK, 1);
    assert_eq!(core::mem::size_of::<ElmProviderSnapshotRequest>(), 24);

    let snapshot_header = ElmProviderSnapshotHeader::new(ELM_MGR_STATUS_OK, 100, 7, 8, 1)
        .with_page(ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE, 9);
    assert_eq!(snapshot_header.status, ELM_MGR_STATUS_OK);
    assert_eq!(snapshot_header.port_id, 100);
    assert_eq!(snapshot_header.binding_id, 7);
    assert_eq!(snapshot_header.payload_len, 8);
    assert_eq!(snapshot_header.record_count, 1);
    assert_eq!(
        snapshot_header.flags,
        ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE
    );
    assert_eq!(snapshot_header.reserved, 9);
    assert_eq!(ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK, 1);
    assert_eq!(core::mem::size_of::<ElmProviderSnapshotHeader>(), 40);

    assert_eq!(
        ElmProviderAsyncState::from_raw(1),
        Some(ElmProviderAsyncState::Queued)
    );
    assert_eq!(
        ElmProviderAsyncState::from_raw(6),
        Some(ElmProviderAsyncState::Expired)
    );
    assert_eq!(ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS, 5_000);
    assert_eq!(ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS, 30_000);
    assert_eq!(ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS, 60_000);
    assert_eq!(ELM_PROVIDER_ASYNC_QUEUE_LIMIT, 64);

    let submit = ElmProviderAsyncSubmitRequest::new(
        ElmCallFrame::new(7, 2, ELM_ACTION_OPCODE_INVOKE, b"abc"),
        100,
        200,
    );
    assert_eq!(submit.frame.binding_id, 7);
    assert_eq!(submit.timeout_ms, 100);
    assert_eq!(submit.result_ttl_ms, 200);
    assert_eq!(core::mem::size_of::<ElmProviderAsyncSubmitRequest>(), 304);

    let submit_response = ElmProviderAsyncSubmitResponse::new(
        99,
        7,
        2,
        ELM_MGR_STATUS_OK,
        ElmProviderAsyncState::Queued,
        1,
        0,
    );
    assert_eq!(submit_response.ticket_id, 99);
    assert_eq!(submit_response.state, ElmProviderAsyncState::Queued as u32);
    assert_eq!(core::mem::size_of::<ElmProviderAsyncSubmitResponse>(), 48);

    let poll = ElmProviderAsyncPollRequest::new(99);
    assert_eq!(poll.ticket_id, 99);
    assert_eq!(core::mem::size_of::<ElmProviderAsyncPollRequest>(), 16);

    let poll_response = ElmProviderAsyncPollResponse::new(
        99,
        ElmProviderAsyncState::Completed,
        ELM_MGR_STATUS_OK,
        ElmReplyFrame::new(7, 2, ELM_MGR_STATUS_OK, b"abc"),
        0,
        123,
    );
    assert_eq!(poll_response.state, ElmProviderAsyncState::Completed as u32);
    assert_eq!(poll_response.expires_at_ns, 123);
    assert_eq!(core::mem::size_of::<ElmProviderAsyncPollResponse>(), 320);

    let cancel = ElmProviderAsyncCancelRequest::new(99);
    assert_eq!(cancel.ticket_id, 99);
    assert_eq!(core::mem::size_of::<ElmProviderAsyncCancelRequest>(), 16);

    let cancel_response = ElmProviderAsyncCancelResponse::new(
        99,
        ElmProviderAsyncState::Canceled,
        ELM_MGR_STATUS_OK,
        0,
    );
    assert_eq!(
        cancel_response.state,
        ElmProviderAsyncState::Canceled as u32
    );
    assert_eq!(core::mem::size_of::<ElmProviderAsyncCancelResponse>(), 24);

    let header = ElmProviderPortStatsHeader::new(1, 9);
    assert_eq!(header.record_count, 1);
    assert_eq!(header.event_sequence, 9);
    assert_eq!(core::mem::size_of::<ElmProviderPortStatsHeader>(), 16);
    let stats_header = ElmProviderPortStatsHeader::new_stats(1, 9);
    assert_eq!(
        stats_header.record_entry_size as usize,
        core::mem::size_of::<ElmProviderPortStatsRecord>()
    );

    let record = ElmProviderPortRecord::new(
        100,
        1,
        ElmPortAccessPolicy::ExtensionOnly as u32,
        FlowDirection::Control as u32,
        FlowMode::Shared as u32,
        true,
        true,
        2,
        ELM_PROVIDER_FLAG_DYNAMIC | ELM_PROVIDER_FLAG_TODO_BACKEND,
        3,
        1,
        1,
        "demo.provider@1",
    );
    assert_eq!(record.binding_count, 2);
    assert_eq!(
        record.flags,
        ELM_PROVIDER_FLAG_DYNAMIC | ELM_PROVIDER_FLAG_TODO_BACKEND
    );
    assert_eq!(record.calls, 3);
    assert_eq!(record.contract_len, "demo.provider@1".len() as u16);
    assert_eq!(core::mem::size_of::<ElmProviderPortRecord>(), 136);

    let stats = ElmProviderPortStatsRecord::new(
        100,
        1,
        2,
        u32::from(ELM_PROVIDER_FLAG_KERNEL_BACKEND),
        3,
        1,
        1,
    );
    assert_eq!(stats.flags, u32::from(ELM_PROVIDER_FLAG_KERNEL_BACKEND));
    assert_eq!(stats.failed_calls, 1);
    assert_eq!(core::mem::size_of::<ElmProviderPortStatsRecord>(), 48);

    let queue_header = ElmProviderQueueStatsHeader::new(1, 9);
    assert_eq!(
        queue_header.record_entry_size as usize,
        core::mem::size_of::<ElmProviderQueueStatsRecord>()
    );
    assert_eq!(core::mem::size_of::<ElmProviderQueueStatsHeader>(), 16);

    let queue_record = ElmProviderQueueStatsRecord::new(4, 1, 0, 2, 64, 4, 9, 8, 1, 2, 3);
    assert_eq!(queue_record.port_id, 4);
    assert_eq!(queue_record.queue_limit, 64);
    assert_eq!(queue_record.max_in_flight, 4);
    assert_eq!(queue_record.rejected, 3);
    assert_eq!(core::mem::size_of::<ElmProviderQueueStatsRecord>(), 72);

    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_PROVIDER_BUSY),
        ELM_MGR_STATUS_BUSY
    );
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL),
        ELM_MGR_STATUS_BUSY
    );
    assert_eq!(ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED, 1 << 19);
    assert_eq!(ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, 1 << 20);
    assert_eq!(ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, 1 << 21);
    assert_eq!(ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED, 1 << 22);
    assert_eq!(ELM_POLICY_BLOCK_RESOURCE_QUOTA, 1 << 24);
    let policy = ElmMgrPolicyInfo::new(128);
    assert_ne!(
        policy.blocker_mask & ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
        0
    );
    assert_ne!(
        policy.blocker_mask & ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL,
        0
    );
    assert_ne!(
        policy.blocker_mask & ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
        0
    );
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_PROVIDER_ASYNC, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_PROVIDER_ASYNC, 0);
}

#[test]
fn extension_mgr_abi_records_are_fixed_layout() {
    assert_eq!(
        ElmMgrCallKind::from_raw(38),
        Some(ElmMgrCallKind::QueryExtensions)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(39),
        Some(ElmMgrCallKind::PreflightExtensionAttach)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(40),
        Some(ElmMgrCallKind::CommitExtensionAttach)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(41),
        Some(ElmMgrCallKind::CommitExtensionDetach)
    );
    assert_eq!(
        ElmMgrCallKind::from_raw(42),
        Some(ElmMgrCallKind::DispatchExtension)
    );

    let header = ElmExtensionSnapshotHeader::new(1, 2, 9);
    assert_eq!(
        header.record_entry_size as usize,
        core::mem::size_of::<ElmExtensionSnapshotRecord>()
    );
    assert_eq!(core::mem::size_of::<ElmExtensionSnapshotHeader>(), 24);

    let point = ElmExtensionSnapshotRecord::point_with_mode(
        1,
        "menu.item",
        "mgr.menu.item@1",
        ElmMixinMode::Observer,
    );
    assert_eq!(point.point_len, "menu.item".len() as u16);
    assert_eq!(point.contract_len, "mgr.menu.item@1".len() as u16);
    assert_eq!(point.mode, ElmMixinMode::Observer as u32);
    assert_eq!(core::mem::size_of::<ElmExtensionSnapshotRecord>(), 208);

    let attach = ElmExtensionAttachRequest::new(100, 1, "menu.item", "mgr.menu.item@1")
        .with_dispatch("demo.handler@1", -10);
    assert_eq!(attach.point.len(), ELM_MGR_EXTENSION_POINT_LEN);
    assert_eq!(attach.contract.len(), ELM_MGR_EXTENSION_CONTRACT_LEN);
    assert_eq!(
        attach.handler_contract.len(),
        ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN
    );
    assert_eq!(attach.priority, -10);
    assert_eq!(attach.handler_contract_len, "demo.handler@1".len() as u16);
    assert_eq!(core::mem::size_of::<ElmExtensionAttachRequest>(), 192);

    let detach = ElmExtensionDetachRequest::new(100, 1, "menu.item");
    assert_eq!(detach.point_len, "menu.item".len() as u16);
    assert_eq!(core::mem::size_of::<ElmExtensionDetachRequest>(), 56);

    let response = ElmExtensionAttachResponse::new(100, 1, 1, true, ELM_MGR_STATUS_OK, 0);
    assert_eq!(response.allowed, 1);
    assert_eq!(core::mem::size_of::<ElmExtensionAttachResponse>(), 40);

    let dispatch = ElmExtensionDispatchRequest::new(1, 100, 7, "menu.item", "mgr.menu.item@1");
    assert_eq!(dispatch.payload.len(), ELM_MGR_EXTENSION_PAYLOAD_LEN);
    assert_eq!(core::mem::size_of::<ElmExtensionDispatchRequest>(), 392);

    let dispatch_response = ElmExtensionDispatchResponse::new(
        ELM_MGR_STATUS_OK,
        1,
        1,
        0,
        ElmReplyFrame::empty(0, 7, ELM_MGR_STATUS_OK),
    );
    assert_eq!(dispatch_response.matched_extensions, 1);
    assert_eq!(dispatch_response.mode, ElmMixinMode::Chain as u32);
    assert_eq!(core::mem::size_of::<ElmExtensionDispatchResponse>(), 312);

    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND),
        ELM_MGR_STATUS_NOT_FOUND
    );
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_EXTENSION_DUPLICATE),
        ELM_MGR_STATUS_BUSY
    );
    let policy = ElmMgrPolicyInfo::new(128);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_EXTENSION_RUNTIME, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_EXTENSION_QUERY, 0);
    assert_ne!(
        policy.supported_actions & ELM_MGR_ACTION_EXTENSION_ATTACH,
        0
    );
    assert_ne!(
        policy.supported_actions & ELM_MGR_ACTION_EXTENSION_DETACH,
        0
    );
    assert_ne!(
        policy.supported_actions & ELM_MGR_ACTION_EXTENSION_DISPATCH,
        0
    );
}

#[test]
fn native_capability_records_are_fixed_layout() {
    assert_eq!(ELM_NATIVE_CAPABILITY_FLAG_KERNEL_SYMBOL, 1 << 2);
    let header = ElmNativeCapabilityHeader::new(1, 0, 9);
    assert_eq!(
        header.record_entry_size as usize,
        core::mem::size_of::<ElmNativeCapabilityRecord>()
    );
    assert_eq!(header.record_count, 1);
    assert_eq!(header.event_sequence, 9);
    assert_eq!(core::mem::size_of::<ElmNativeCapabilityHeader>(), 24);

    let import = ElmNativeCapabilityRecord::new(
        ELM_NATIVE_CAPABILITY_KIND_IMPORT,
        ELM_MGR_STATUS_OK,
        7,
        3,
        0,
        2,
        ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD,
        "runtime.invoke",
        "mgr.action.invoke@1",
    );
    assert_eq!(import.kind, ELM_NATIVE_CAPABILITY_KIND_IMPORT);
    assert_eq!(import.owner_cell_id, 7);
    assert_eq!(import.peer_cell_id, 3);
    assert_eq!(import.requested_version, 0);
    assert_eq!(import.selected_version, 2);
    assert_eq!(import.name_len, "runtime.invoke".len() as u16);
    assert_eq!(import.contract_len, "mgr.action.invoke@1".len() as u16);
    assert_eq!(import.name.len(), ELM_NATIVE_CAPABILITY_NAME_LEN);

    let export = ElmNativeCapabilityRecord::new(
        ELM_NATIVE_CAPABILITY_KIND_EXPORT,
        ELM_MGR_STATUS_OK,
        3,
        0,
        1,
        1,
        0,
        "runtime.invoke",
        "mgr.action.invoke@1",
    );
    assert_eq!(export.kind, ELM_NATIVE_CAPABILITY_KIND_EXPORT);
    assert_eq!(export.peer_cell_id, 0);
    assert_eq!(core::mem::size_of::<ElmNativeCapabilityRecord>(), 240);
}

#[test]
fn core_health_abi_records_are_fixed_layout() {
    let header = ElmCoreHealthHeader::new(2, ELM_MGR_STATUS_OK, 9);
    assert_eq!(header.record_count, 2);
    assert_eq!(header.status, ELM_MGR_STATUS_OK);
    assert_eq!(header.flags, 0);
    assert_eq!(header.event_sequence, 9);
    assert_eq!(
        header.record_entry_size as usize,
        core::mem::size_of::<ElmCoreHealthRecord>()
    );
    assert_eq!(core::mem::size_of::<ElmCoreHealthHeader>(), 24);

    let failed = ElmCoreHealthHeader::new(1, ELM_MGR_STATUS_INVALID, 10);
    assert_ne!(failed.flags & ELM_HEALTH_FLAG_HAS_FAILURES, 0);

    let ok = ElmCoreHealthRecord::ok(ELM_HEALTH_CHECK_GRAPH);
    assert_eq!(ok.status, ELM_MGR_STATUS_OK);
    assert_eq!(ok.detail, ELM_HEALTH_DETAIL_NONE);

    let invalid = ElmCoreHealthRecord::invalid(ELM_HEALTH_CHECK_GRAPH, 7, 3);
    assert_eq!(invalid.status, ELM_MGR_STATUS_INVALID);
    assert_eq!(invalid.subject_id, 7);
    assert_eq!(core::mem::size_of::<ElmCoreHealthRecord>(), 24);
}

#[test]
fn mgr_topology_and_audit_records_are_fixed_layout() {
    let relation = ElmMgrRelationRecord::new(
        ElmMgrRelationKind::Extension,
        2,
        1,
        "mgr.menu.item@1",
        "menu.item",
    );
    assert_eq!(relation.source, 2);
    assert_eq!(relation.target, 1);
    assert_eq!(relation.contract_len, "mgr.menu.item@1".len() as u16);
    assert_eq!(relation.point_len, "menu.item".len() as u16);
    assert_eq!(core::mem::size_of::<ElmMgrRelationRecord>(), 128);

    let topology = ElmMgrTopologyHeader::new(1, 2, 9);
    assert_eq!(topology.relation_count, 1);
    assert_eq!(topology.cell_count, 2);
    assert_eq!(topology.event_sequence, 9);
    assert_eq!(core::mem::size_of::<ElmMgrTopologyHeader>(), 24);

    let audit = ElmMgrAuditRecord::new(
        3,
        ElmLifecycleAction::Detach as u32,
        ELM_MGR_STATUS_BUSY,
        7,
        ELM_POLICY_BLOCK_HAS_EXTENSIONS,
        state_code(ElmState::Active),
    );
    assert_eq!(audit.sequence, 3);
    assert_eq!(
        first_lifecycle_reason(audit.blockers),
        ELM_LIFECYCLE_REASON_HAS_EXTENSIONS
    );
    assert_eq!(audit.flags, crate::ELM_AUDIT_FLAG_OPERATION);
    assert_eq!(audit.authority, crate::ELM_AUDIT_AUTHORITY_KERNEL);
    assert_eq!(core::mem::size_of::<ElmMgrAuditRecord>(), 88);

    let audit_header = ElmMgrAuditHeader::new(1, 0, 3);
    assert_eq!(audit_header.record_count, 1);
    assert_eq!(audit_header.last_sequence, 3);
    assert_eq!(core::mem::size_of::<ElmMgrAuditHeader>(), 24);
}

#[test]
fn lifecycle_status_helpers_report_planned_result() {
    assert_eq!(
        planned_final_state(ElmLifecycleAction::Pause, ElmState::Active),
        state_code(ElmState::Paused)
    );
    assert_eq!(
        planned_final_state(ElmLifecycleAction::Replace, ElmState::Active),
        state_code(ElmState::Active)
    );
    assert_eq!(status_from_blockers(0), ELM_MGR_STATUS_OK);
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_PORT_TODO),
        ELM_MGR_STATUS_TODO
    );
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_DUPLICATE_BINDING),
        ELM_MGR_STATUS_BUSY
    );
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
        ELM_MGR_STATUS_INVALID
    );
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED),
        ELM_MGR_STATUS_INVALID
    );
    assert_eq!(
        first_lifecycle_reason(ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED),
        ELM_LIFECYCLE_REASON_HOOK_FAILED
    );
}

#[test]
fn menu_snapshot_entries_are_fixed_layout() {
    let header = ElmMenuSnapshotHeader::new(1, 3);
    assert_eq!(header.item_count, 1);
    assert_eq!(header.generation, 3);
    assert!(header.item_entry_size as usize >= core::mem::size_of::<ElmMenuItemSnapshot>());

    let item = ElmMenuItemSnapshot::new(
        7,
        ElmId(2),
        ActionId(9),
        ElmMenuItemKind::Action,
        crate::ELM_MENU_FLAG_TODO,
        "ELM 状态",
        "查看当前 ELM 拓扑",
        "elm/status",
    );
    assert_eq!(item.id, 7);
    assert_eq!(item.owner, 2);
    assert_eq!(item.action, 9);
    assert_eq!(item.label_len, "ELM 状态".len() as u16);
    assert_eq!(&item.route[..10], b"elm/status");
}

#[test]
fn ebi_protocol_accepts_menu_extension_unit() {
    let unit = ElmEbiUnit::new(
        ElmManifest::new(
            ElmName::new("demo-menu").unwrap(),
            ElmVersion::new("0.1.0").unwrap(),
            ElmKind::Extension,
        ),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_menu(ElmEbiMenuDecl::new(
        ElmMenuItemKind::Action,
        crate::ELM_MENU_FLAG_TODO,
        "Demo",
        "demo item",
        "demo/run",
    ));

    assert!(unit.validate(ElmEbiArch::Riscv64).is_ok());
    assert_eq!(unit.manifest.name.as_str(), "demo-menu");
    assert!(unit.menu.is_some());
    assert!(!unit.has_native_code());
}

#[test]
fn ebi_protocol_rejects_wrong_architecture() {
    let unit = ElmEbiUnit::new(
        manifest("wrong-arch"),
        ElmEbiTarget::new(ElmEbiArch::LoongArch64),
    );
    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::ArchMismatch)
    );
}

#[test]
fn ebi_protocol_marks_native_code_as_todo_boundary() {
    let unit =
        ebi_unit("native-cell").with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::Code, 4096, 0));

    assert!(unit.validate(ElmEbiArch::Riscv64).is_ok());
    assert!(unit.has_native_code());
    assert_eq!(
        unit.segments[0].flags,
        ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_EXECUTE
    );
}

#[test]
fn ebi_protocol_marks_entry_as_native_boundary() {
    let unit = ebi_unit("entry-cell").with_entry(ElmEbiEntry::new("elm_main"));

    assert!(unit.validate(ElmEbiArch::LoongArch64).is_ok());
    assert!(unit.has_native_code());
}

#[test]
fn ebi_image_accepts_code_entry_symbol() {
    let unit = ebi_unit("entry-image")
        .with_entry(ElmEbiEntry::new("elm_main"))
        .with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::Code, 16, 0));
    let image = ElmEbiImage::new(unit)
        .with_symbol_location(
            ElmEbiSymbolLocationDecl::new(ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4, 0).unwrap(),
        )
        .with_symbol_location(
            ElmEbiSymbolLocationDecl::new(ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4, 0).unwrap(),
        )
        .with_symbol_location(ElmEbiSymbolLocationDecl::new("elm_main", 0, 8, 4, 0).unwrap());

    assert!(image.validate(ElmEbiArch::Riscv64).is_ok());
}

#[test]
fn ebi_image_rejects_missing_entry_symbol() {
    let unit = ebi_unit("missing-entry-image")
        .with_entry(ElmEbiEntry::new("elm_main"))
        .with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::Code, 16, 0));
    let image = ElmEbiImage::new(unit)
        .with_symbol_location(
            ElmEbiSymbolLocationDecl::new(ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4, 0).unwrap(),
        )
        .with_symbol_location(
            ElmEbiSymbolLocationDecl::new(ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4, 0).unwrap(),
        );

    assert_eq!(
        image.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
}

#[test]
fn ebi_image_rejects_entry_symbol_outside_code() {
    let unit = ebi_unit("bad-entry-image")
        .with_entry(ElmEbiEntry::new("elm_main"))
        .with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::Code, 16, 0))
        .with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::ReadOnlyData, 16, 0));
    let image = ElmEbiImage::new(unit)
        .with_symbol_location(
            ElmEbiSymbolLocationDecl::new(ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4, 0).unwrap(),
        )
        .with_symbol_location(
            ElmEbiSymbolLocationDecl::new(ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4, 0).unwrap(),
        )
        .with_symbol_location(ElmEbiSymbolLocationDecl::new("elm_main", 1, 0, 4, 0).unwrap());

    assert_eq!(
        image.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
}

#[test]
fn ebi_protocol_accepts_payload_segments_and_symbols() {
    let unit = ebi_unit("payload-cell")
        .with_segment(ElmEbiSegment::from_payload(
            ElmEbiSegmentKind::Code,
            0,
            64,
            64,
            16,
            3,
            512,
            0x1234,
        ))
        .with_import(
            ElmEbiImportDecl::new(
                "runtime.invoke",
                "mgr.action.invoke@1",
                1,
                ELM_EBI_IMPORT_FLAG_MANAGED,
                [0; 32],
            )
            .unwrap(),
        )
        .with_export(
            ElmEbiExportDecl::new(
                "demo.provider",
                "demo.provider@1",
                1,
                ELM_EBI_EXPORT_FLAG_MANAGED,
                [0; 32],
            )
            .unwrap(),
        );

    assert!(unit.validate(ElmEbiArch::Riscv64).is_ok());
    assert_eq!(unit.segments[0].file_size, 64);
    assert_eq!(unit.segments[0].mem_size, 64);
    assert_eq!(unit.segments[0].source_index, 3);
    assert_eq!(unit.imports[0].name, "runtime.invoke");
    assert_eq!(unit.exports[0].contract.as_str(), "demo.provider@1");
}

#[test]
fn ebi_protocol_rejects_invalid_code_segment_flags() {
    let unit = ebi_unit("bad-code").with_segment(ElmEbiSegment::from_payload(
        ElmEbiSegmentKind::Code,
        ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_WRITE,
        64,
        64,
        0,
        1,
        0,
        1,
    ));

    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::InvalidSegment)
    );
}

#[test]
fn ebi_protocol_accepts_declarative_topology_unit() {
    let unit = ebi_unit("topology-cell")
        .with_dependency(ElmEbiDependencyDecl::new("elm-mgr", "core.event@1").unwrap())
        .with_extension_point(ElmEbiExtensionPointDecl::new("demo.point", "demo.point@1").unwrap())
        .with_extension(
            ElmEbiExtensionDecl::new("elm-mgr", "menu.item", "mgr.menu.item@1").unwrap(),
        )
        .with_provider_port(
            ElmEbiProviderPortDecl::new(
                "demo.provider@1",
                ElmPortAccessPolicy::Public,
                FlowDirection::Control,
                FlowMode::Shared,
                0,
            )
            .unwrap(),
        );

    assert!(unit.validate(ElmEbiArch::Riscv64).is_ok());
    assert_eq!(unit.dependencies.len(), 1);
    assert_eq!(unit.extension_points.len(), 1);
    assert_eq!(unit.extensions.len(), 1);
    assert_eq!(unit.provider_ports.len(), 1);
}

#[test]
fn ebi_protocol_accepts_lifecycle_less_declarative_unit() {
    let unit = ElmEbiUnit::new(
        manifest("missing-hooks"),
        ElmEbiTarget::new(ElmEbiArch::Any),
    );

    assert!(unit.validate(ElmEbiArch::Riscv64).is_ok());
    assert!(unit.lifecycle_hooks.is_none());
}

#[test]
fn ebi_protocol_rejects_wrong_lifecycle_hook_symbol() {
    let bad_hooks = ElmEbiLifecycleHooks::new(
        ElmEbiLifecycleHookDecl::new(
            ElmEbiLifecycleHookKind::Initialize,
            "init",
            crate::ELM_EBI_RUST_ABI_VERSION,
            ElmEbiRustHookSignature::ContextResult,
            0,
        )
        .unwrap(),
        ElmEbiLifecycleHookDecl::new(
            ElmEbiLifecycleHookKind::Finalize,
            ELM_EBI_HOOK_ON_FINALIZE,
            crate::ELM_EBI_RUST_ABI_VERSION,
            ElmEbiRustHookSignature::ContextResult,
            0,
        )
        .unwrap(),
    );

    assert_eq!(bad_hooks, Err(ElmEbiLoadStatus::InvalidManifest));
}

#[test]
fn ebi_protocol_rejects_invalid_provider_flags() {
    let unit = ElmEbiUnit::new(
        manifest("bad-provider-flags"),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_provider_port(
        ElmEbiProviderPortDecl::new(
            "bad.provider@1",
            ElmPortAccessPolicy::Public,
            FlowDirection::Control,
            FlowMode::Shared,
            1,
        )
        .unwrap(),
    );

    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
}

#[test]
fn ebi_protocol_rejects_invalid_menu_decl() {
    let unit = ElmEbiUnit::new(manifest("bad-menu"), ElmEbiTarget::new(ElmEbiArch::Any)).with_menu(
        ElmEbiMenuDecl::new(ElmMenuItemKind::Action, 0, "", "missing label", "bad/menu"),
    );

    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::InvalidMenu)
    );
}

#[test]
fn ebi_protocol_rejects_unsupported_abi() {
    let mut target = ElmEbiTarget::new(ElmEbiArch::Any);
    target.abi_version = crate::ELM_EBI_ABI_VERSION + 1;
    let unit = ElmEbiUnit::new(manifest("future-unit"), target);

    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::UnsupportedAbi)
    );
}

#[test]
fn ebi_source_request_is_fixed_layout() {
    let request = ElmEbiSourceRequest::new(ElmEbiSourceKind::Projection, 128);
    assert_eq!(core::mem::size_of::<ElmEbiSourceRequest>(), 96);
    assert_eq!(crate::ELM_EBI_SOURCE_REQUEST_SIZE, 96);
    assert_eq!(
        core::mem::size_of::<crate::ElmEkiHeader>(),
        ELM_EKI_HEADER_SIZE
    );
    assert_eq!(
        core::mem::size_of::<crate::ElmEkiBlockDesc>(),
        ELM_EKI_BLOCK_DESC_SIZE
    );
    assert_eq!(request.abi_version, ELM_EBI_SOURCE_ABI_VERSION);
    assert_eq!(request.source_kind, ElmEbiSourceKind::Projection as u16);
    assert_eq!(request.flags, 0);
    assert_eq!(request.parent_cell_id, crate::ELM_MGR_BUILTIN_ID.0);
    assert_eq!(request.budget, crate::ElmResourceBudget::DEFAULT);
    assert_eq!(request.reserved0, 0);
    assert_eq!(request.reserved1, 0);
    assert_eq!(request.payload_len, 128);
    assert_eq!(request.reserved2, 0);
    assert_eq!(request.reserved3, 0);
    let manager = request.with_management_grant();
    assert!(manager.grants_management());
    assert_eq!(manager.flags, crate::ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT);
    let kernel_symbols = request.with_privileged_symbol_authorization();
    assert!(kernel_symbols.authorizes_privileged_symbols());
    assert_eq!(
        kernel_symbols.flags,
        crate::ELM_EBI_SOURCE_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS
    );
    assert_eq!(
        ElmEbiSourceKind::from_raw(5),
        Some(ElmEbiSourceKind::BuildBound)
    );
    assert_eq!(ElmEbiSourceKind::from_raw(6), None);
}

#[test]
fn projection_source_request_is_fixed_layout() {
    let request = ElmProjectionSourceRequest::new(ELM_EKI_PROJECTION_SOURCE_ID, 128);
    assert_eq!(ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE, 24);
    assert_eq!(core::mem::size_of::<ElmProjectionSourceRequest>(), 24);
    assert_eq!(request.abi_version, ELM_EBI_PROJECTION_SOURCE_ABI_VERSION);
    assert_eq!(request.flags, 0);
    assert_eq!(request.reserved0, 0);
    assert_eq!(request.provider_id, ELM_EKI_PROJECTION_SOURCE_ID);
    assert_eq!(request.payload_len, 128);
    assert_eq!(request.reserved1, 0);
}

#[test]
fn eki_parser_accepts_menu_unit() {
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-menu", "0.1.0", ElmKind::Extension),
        ),
        (
            ElmEkiBlockKind::Menu,
            eki_menu_block("EKI 菜单", "来自 EKI 的菜单项", "eki/menu"),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    assert_eq!(unit.manifest.name.as_str(), "eki-menu");
    assert_eq!(unit.manifest.kind, ElmKind::Extension);
    assert!(unit.menu.is_some());
    assert!(!unit.has_native_code());
}

#[test]
fn eki_parser_selects_exact_profile_from_multi_variant_bundle() {
    let first_profile = [0x11; 32];
    let second_profile = [0x22; 32];
    let first = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("variant-first", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::AbiFingerprint,
            eki_abi_fingerprint_block(first_profile, 1),
        ),
    ]);
    let second = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("variant-second", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::AbiFingerprint,
            eki_abi_fingerprint_block(second_profile, 1),
        ),
    ]);
    let mut directory = vec![0u8; 8 + crate::ELM_EKI_VARIANT_RECORD_SIZE * 2];
    write_u16(&mut directory, 0, crate::ELM_EKI_VARIANT_DIRECTORY_VERSION);
    write_u16(&mut directory, 2, crate::ELM_EKI_VARIANT_RECORD_SIZE as u16);
    write_u32(&mut directory, 4, 2);
    for (index, (profile, image, priority)) in [
        (first_profile, first.as_slice(), 10u32),
        (second_profile, second.as_slice(), 20u32),
    ]
    .iter()
    .enumerate()
    {
        let offset = 8 + index * crate::ELM_EKI_VARIANT_RECORD_SIZE;
        write_u32(&mut directory, offset, (index + 1) as u32);
        write_u32(&mut directory, offset + 4, ElmEbiArch::Any as u32);
        write_u32(&mut directory, offset + 8, *priority);
        write_u16(&mut directory, offset + 12, 1);
        directory[offset + 16..offset + 48].copy_from_slice(profile);
        directory[offset + 48..offset + 80].copy_from_slice(&crate::sha256(image));
    }
    let bundle = eki_image(&[
        (ElmEkiBlockKind::VariantDirectory, directory),
        (ElmEkiBlockKind::VariantImage, first),
        (ElmEkiBlockKind::VariantImage, second),
    ]);

    let variants = crate::parse_eki_variants(&bundle).unwrap();
    assert_eq!(variants.len(), 2);
    let selected = crate::parse_eki_image_for(
        &bundle,
        crate::ElmEkiSelector {
            arch: ElmEbiArch::Riscv64,
            profile_hash: first_profile,
            bridge_abi_version: 1,
        },
    )
    .unwrap();
    assert_eq!(selected.unit.manifest.name.as_str(), "variant-first");
    assert_eq!(
        crate::parse_eki_image(&bundle),
        Err(ElmEbiLoadStatus::UnsupportedAbi)
    );
}

#[test]
fn eki_parser_rejects_variant_directory_profile_relabel() {
    let inner_profile = [0x41; 32];
    let directory_profile = [0x42; 32];
    let inner = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("variant-profile-relabel", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::AbiFingerprint,
            eki_abi_fingerprint_block(inner_profile, 1),
        ),
    ]);
    let mut directory = vec![0u8; 8 + crate::ELM_EKI_VARIANT_RECORD_SIZE];
    write_u16(&mut directory, 0, crate::ELM_EKI_VARIANT_DIRECTORY_VERSION);
    write_u16(&mut directory, 2, crate::ELM_EKI_VARIANT_RECORD_SIZE as u16);
    write_u32(&mut directory, 4, 1);
    write_u32(&mut directory, 8, 1);
    write_u32(&mut directory, 12, ElmEbiArch::Any as u32);
    write_u16(&mut directory, 20, 1);
    directory[24..56].copy_from_slice(&directory_profile);
    directory[56..88].copy_from_slice(&sha256(&inner));
    let bundle = eki_image(&[
        (ElmEkiBlockKind::VariantDirectory, directory),
        (ElmEkiBlockKind::VariantImage, inner),
    ]);

    assert_eq!(
        crate::parse_eki_variants(&bundle),
        Err(ElmEbiLoadStatus::AbiFingerprintRejected)
    );
}

#[test]
fn eki_parser_rejects_variant_directory_bridge_relabel() {
    let profile = [0x51; 32];
    let inner = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("variant-bridge-relabel", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::AbiFingerprint,
            eki_abi_fingerprint_block(profile, 1),
        ),
    ]);
    let mut directory = vec![0u8; 8 + crate::ELM_EKI_VARIANT_RECORD_SIZE];
    write_u16(&mut directory, 0, crate::ELM_EKI_VARIANT_DIRECTORY_VERSION);
    write_u16(&mut directory, 2, crate::ELM_EKI_VARIANT_RECORD_SIZE as u16);
    write_u32(&mut directory, 4, 1);
    write_u32(&mut directory, 8, 1);
    write_u32(&mut directory, 12, ElmEbiArch::Any as u32);
    write_u16(&mut directory, 20, 2);
    directory[24..56].copy_from_slice(&profile);
    directory[56..88].copy_from_slice(&sha256(&inner));
    let bundle = eki_image(&[
        (ElmEkiBlockKind::VariantDirectory, directory),
        (ElmEkiBlockKind::VariantImage, inner),
    ]);

    assert_eq!(
        crate::parse_eki_image(&bundle),
        Err(ElmEbiLoadStatus::AbiFingerprintRejected)
    );
}

#[test]
fn eki_parser_accepts_migration_lifecycle_hooks() {
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-migrate-hooks", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::LifecycleHooks,
            eki_lifecycle_hooks_block_with_migration(),
        ),
    ]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    let hooks = unit.lifecycle_hooks.unwrap();
    assert_eq!(hooks.initialize.symbol, ELM_EBI_HOOK_ON_INITIALIZE);
    assert_eq!(hooks.finalize.symbol, ELM_EBI_HOOK_ON_FINALIZE);
    assert_eq!(
        hooks.migrate_export.unwrap().symbol,
        ELM_EBI_HOOK_ON_MIGRATE_EXPORT
    );
    assert_eq!(
        hooks.migrate_import.unwrap().symbol,
        ELM_EBI_HOOK_ON_MIGRATE_IMPORT
    );
    assert_eq!(
        hooks.migrate_abort.unwrap().symbol,
        ELM_EBI_HOOK_ON_MIGRATE_ABORT
    );
}

#[test]
fn eki_parser_marks_native_segments() {
    let code = vec![0x13, 0, 0, 0];
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-native", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_block(
                ElmEbiSegmentKind::Code,
                0,
                code.len() as u64,
                code.len() as u64,
            ),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 2),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 2, 2),
            ]),
        ),
    ]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    assert_eq!(unit.segments.len(), 1);
    assert_eq!(unit.segments[0].kind, ElmEbiSegmentKind::Code);
    assert_eq!(unit.segments[0].file_size, 4);
    assert_eq!(unit.segments[0].mem_size, 4);
    assert_ne!(unit.segments[0].content_hash, 0);
    assert!(unit.has_native_code());
}

#[test]
fn eki_parser_accepts_symbol_locations_and_relocations() {
    let code = vec![0x13, 0, 0, 0, 0, 0, 0, 0];
    let relocs = eki_relocations_block(&[(ElmEbiRelocationKind::SymbolAbs64, 0, 0, 0, 8)]);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-reloc", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_blocks(&[
                (
                    ElmEbiSegmentKind::Code,
                    0,
                    code.len() as u64,
                    code.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Relocation,
                    0,
                    relocs.len() as u64,
                    relocs.len() as u64,
                ),
            ]),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::Relocation, relocs),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
            ]),
        ),
    ]);

    let image = parse_eki_image(&image).unwrap();
    assert_eq!(image.symbol_locations.len(), 2);
    assert_eq!(image.relocations.len(), 1);
    assert_eq!(image.relocations[0].kind, ElmEbiRelocationKind::SymbolAbs64);
    assert_eq!(image.relocations[0].addend, 8);
}

#[test]
fn eki_parser_accepts_import_relocations() {
    let code = vec![0; 40];
    let rust_abi_hash = crate::sha256(b"fn()->u64");
    let relocs = eki_relocations_block(&[
        (ElmEbiRelocationKind::ImportAbs64, 0, 0, 8, 0),
        (ElmEbiRelocationKind::ImportRel32, 0, 0, 16, 4),
        (ElmEbiRelocationKind::ImportRel64, 0, 0, 24, -4),
    ]);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-import-reloc", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Imports,
            eki_symbol_block_with_flags_and_hash(
                "runtime.invoke",
                "mgr.action.invoke@1",
                1,
                ELM_EBI_IMPORT_FLAG_DIRECT_PINNED,
                rust_abi_hash,
            ),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_blocks(&[
                (
                    ElmEbiSegmentKind::Code,
                    0,
                    code.len() as u64,
                    code.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Relocation,
                    0,
                    relocs.len() as u64,
                    relocs.len() as u64,
                ),
            ]),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::Relocation, relocs),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
            ]),
        ),
    ]);

    let image = parse_eki_image(&image).unwrap();
    assert_eq!(image.unit.imports.len(), 1);
    assert_eq!(image.unit.imports[0].rust_abi_hash, rust_abi_hash);
    assert_eq!(image.relocations.len(), 3);
    assert_eq!(image.relocations[0].kind, ElmEbiRelocationKind::ImportAbs64);
    assert_eq!(image.relocations[1].kind, ElmEbiRelocationKind::ImportRel32);
    assert_eq!(image.relocations[2].kind, ElmEbiRelocationKind::ImportRel64);
}

#[test]
fn eki_parser_rejects_callable_import_without_relocation() {
    for (name, contract, flags) in [
        (
            "runtime.direct",
            "runtime.direct@1",
            ELM_EBI_IMPORT_FLAG_DIRECT_PINNED,
        ),
        (
            "sched.now_ns_public",
            "kernel.sched.now-ns@1",
            ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL,
        ),
    ] {
        let image = eki_image(&[
            (
                ElmEkiBlockKind::Manifest,
                eki_manifest_block("eki-import-without-slot", "0.1.0", ElmKind::Service),
            ),
            (
                ElmEkiBlockKind::Imports,
                eki_symbol_block_with_flags_and_hash(
                    name,
                    contract,
                    1,
                    flags,
                    crate::sha256(b"fn()->u64"),
                ),
            ),
        ]);

        assert_eq!(
            parse_eki_image(&image),
            Err(ElmEbiLoadStatus::InvalidTarget)
        );
    }
}

#[test]
fn eki_parser_accepts_managed_import_only_in_aligned_data_slot() {
    let code = vec![0; 8];
    let data = vec![0; 16];
    let relocs = eki_relocations_block(&[(ElmEbiRelocationKind::ImportAbs64, 1, 0, 8, 0)]);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-managed-import", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Imports,
            eki_symbol_block_with_flags(
                "runtime.invoke",
                "mgr.action.invoke@1",
                1,
                ELM_EBI_IMPORT_FLAG_MANAGED,
            ),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_blocks(&[
                (
                    ElmEbiSegmentKind::Code,
                    0,
                    code.len() as u64,
                    code.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Data,
                    0,
                    data.len() as u64,
                    data.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Relocation,
                    0,
                    relocs.len() as u64,
                    relocs.len() as u64,
                ),
            ]),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::Data, data),
        (ElmEkiBlockKind::Relocation, relocs),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
            ]),
        ),
    ]);

    assert!(parse_eki_image(&image).is_ok());
}

#[test]
fn eki_parser_rejects_managed_import_outside_aligned_abs64_slot() {
    for (kind, segment_index, target_offset) in [
        (ElmEbiRelocationKind::ImportAbs64, 0, 0),
        (ElmEbiRelocationKind::ImportAbs64, 1, 1),
        (ElmEbiRelocationKind::ImportRel64, 1, 8),
    ] {
        let code = vec![0; 16];
        let data = vec![0; 16];
        let relocs = eki_relocations_block(&[(kind, segment_index, 0, target_offset, 0)]);
        let image = eki_image(&[
            (
                ElmEkiBlockKind::Manifest,
                eki_manifest_block("eki-bad-managed-import", "0.1.0", ElmKind::Service),
            ),
            (
                ElmEkiBlockKind::Imports,
                eki_symbol_block_with_flags(
                    "runtime.invoke",
                    "mgr.action.invoke@1",
                    1,
                    ELM_EBI_IMPORT_FLAG_MANAGED,
                ),
            ),
            (
                ElmEkiBlockKind::Segments,
                eki_segments_blocks(&[
                    (
                        ElmEbiSegmentKind::Code,
                        0,
                        code.len() as u64,
                        code.len() as u64,
                    ),
                    (
                        ElmEbiSegmentKind::Data,
                        0,
                        data.len() as u64,
                        data.len() as u64,
                    ),
                    (
                        ElmEbiSegmentKind::Relocation,
                        0,
                        relocs.len() as u64,
                        relocs.len() as u64,
                    ),
                ]),
            ),
            (ElmEkiBlockKind::Code, code),
            (ElmEkiBlockKind::Data, data),
            (ElmEkiBlockKind::Relocation, relocs),
            (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
            (
                ElmEkiBlockKind::SymbolLocations,
                eki_symbol_locations_block(&[
                    (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                    (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
                ]),
            ),
        ]);

        assert_eq!(
            parse_eki_image(&image),
            Err(ElmEbiLoadStatus::InvalidTarget)
        );
    }
}

#[test]
fn eki_parser_accepts_elmapi_compatibility_and_root_slot() {
    let code = vec![0; 8];
    let data = vec![0; 8];
    let relocs = eki_relocations_block(&[(ElmEbiRelocationKind::ImportAbs64, 1, 0, 0, 0)]);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-elmapi", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Imports,
            eki_symbol_block(ELM_API_ROOT_IMPORT_NAME, ELM_API_ROOT_IMPORT_CONTRACT, 0),
        ),
        (
            ElmEkiBlockKind::ApiCompatibility,
            eki_elmapi_block(0, ELM_API_FEATURE_LOG, &[1]),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_blocks(&[
                (
                    ElmEbiSegmentKind::Code,
                    0,
                    code.len() as u64,
                    code.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Data,
                    0,
                    data.len() as u64,
                    data.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Relocation,
                    0,
                    relocs.len() as u64,
                    relocs.len() as u64,
                ),
            ]),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::Data, data),
        (ElmEkiBlockKind::Relocation, relocs),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
            ]),
        ),
    ]);

    let image = parse_eki_image(&image).unwrap();
    let compatibility = image.unit.api_compatibility.unwrap();
    assert_eq!(compatibility.root_import_index, 0);
    assert_eq!(compatibility.required_features, ELM_API_FEATURE_LOG);
    assert_eq!(compatibility.compatible_versions, vec![1]);
}

#[test]
fn eki_parser_rejects_elmapi_root_relocation_in_code() {
    let code = vec![0; 16];
    let relocs = eki_relocations_block(&[(ElmEbiRelocationKind::ImportAbs64, 0, 0, 8, 0)]);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-elmapi-code-slot", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Imports,
            eki_symbol_block(ELM_API_ROOT_IMPORT_NAME, ELM_API_ROOT_IMPORT_CONTRACT, 0),
        ),
        (
            ElmEkiBlockKind::ApiCompatibility,
            eki_elmapi_block(0, 0, &[1]),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_blocks(&[
                (
                    ElmEbiSegmentKind::Code,
                    0,
                    code.len() as u64,
                    code.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Relocation,
                    0,
                    relocs.len() as u64,
                    relocs.len() as u64,
                ),
            ]),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::Relocation, relocs),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
            ]),
        ),
    ]);

    assert_eq!(
        parse_eki_image(&image),
        Err(ElmEbiLoadStatus::InvalidTarget)
    );
}

#[test]
fn eki_parser_rejects_unaligned_elmapi_root_slot() {
    let code = vec![0; 8];
    let data = vec![0; 16];
    let relocs = eki_relocations_block(&[(ElmEbiRelocationKind::ImportAbs64, 1, 0, 1, 0)]);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-elmapi-unaligned-slot", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Imports,
            eki_symbol_block(ELM_API_ROOT_IMPORT_NAME, ELM_API_ROOT_IMPORT_CONTRACT, 0),
        ),
        (
            ElmEkiBlockKind::ApiCompatibility,
            eki_elmapi_block(0, 0, &[1]),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_blocks(&[
                (
                    ElmEbiSegmentKind::Code,
                    0,
                    code.len() as u64,
                    code.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Data,
                    0,
                    data.len() as u64,
                    data.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Relocation,
                    0,
                    relocs.len() as u64,
                    relocs.len() as u64,
                ),
            ]),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::Data, data),
        (ElmEkiBlockKind::Relocation, relocs),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
            ]),
        ),
    ]);

    assert_eq!(
        parse_eki_image(&image),
        Err(ElmEbiLoadStatus::InvalidTarget)
    );
}

#[test]
fn eki_parser_rejects_payload_segment_without_declaration() {
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-native-no-decl", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::Code, vec![0x13, 0, 0, 0]),
    ]);

    assert_eq!(
        parse_eki_ebi_unit(&image),
        Err(ElmEbiLoadStatus::InvalidSegment)
    );
}

#[test]
fn eki_parser_accepts_imports_exports_metadata() {
    let code = vec![0; 16];
    let data = vec![0; 8];
    let relocs = eki_relocations_block(&[(ElmEbiRelocationKind::ImportAbs64, 1, 0, 0, 0)]);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-symbols", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Imports,
            eki_symbol_block_with_flags(
                "runtime.invoke",
                "mgr.action.invoke@1",
                1,
                ELM_EBI_IMPORT_FLAG_MANAGED,
            ),
        ),
        (
            ElmEkiBlockKind::Exports,
            eki_symbol_block_with_flags(
                "demo.provider",
                "demo.provider@1",
                1,
                ELM_EBI_EXPORT_FLAG_MANAGED,
            ),
        ),
        (
            ElmEkiBlockKind::Segments,
            eki_segments_blocks(&[
                (
                    ElmEbiSegmentKind::Code,
                    0,
                    code.len() as u64,
                    code.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Data,
                    0,
                    data.len() as u64,
                    data.len() as u64,
                ),
                (
                    ElmEbiSegmentKind::Relocation,
                    0,
                    relocs.len() as u64,
                    relocs.len() as u64,
                ),
            ]),
        ),
        (ElmEkiBlockKind::Code, code),
        (ElmEkiBlockKind::Data, data),
        (ElmEkiBlockKind::Relocation, relocs),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
        (
            ElmEkiBlockKind::SymbolLocations,
            eki_symbol_locations_block(&[
                (ELM_EBI_HOOK_ON_INITIALIZE, 0, 0, 4),
                (ELM_EBI_HOOK_ON_FINALIZE, 0, 4, 4),
                ("demo.provider", 0, 8, 4),
            ]),
        ),
    ]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    assert_eq!(unit.imports.len(), 1);
    assert_eq!(unit.exports.len(), 1);
    assert_eq!(unit.imports[0].name, "runtime.invoke");
    assert_eq!(unit.exports[0].contract.as_str(), "demo.provider@1");
    assert!(unit.has_native_code());
}

#[test]
fn eki_parser_accepts_declarative_topology_blocks() {
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-topology", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Dependencies,
            eki_dependency_block("elm-mgr", "core.event@1"),
        ),
        (
            ElmEkiBlockKind::ExtensionPoints,
            eki_extension_point_block_with_mode(
                "demo.point",
                "demo.point@1",
                ElmMixinMode::Observer,
            ),
        ),
        (
            ElmEkiBlockKind::Extensions,
            eki_extension_block_with_dispatch(
                "elm-mgr",
                "menu.item",
                "mgr.menu.item@1",
                "demo.provider@1",
                -7,
            ),
        ),
        (
            ElmEkiBlockKind::ProviderPorts,
            eki_provider_port_block(
                "demo.provider@1",
                ElmPortAccessPolicy::Public,
                FlowDirection::Control,
                FlowMode::Shared,
            ),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    assert_eq!(unit.manifest.name.as_str(), "eki-topology");
    assert_eq!(unit.dependencies[0].provider_name, "elm-mgr");
    assert_eq!(unit.extension_points[0].point, "demo.point");
    assert_eq!(unit.extension_points[0].mode, ElmMixinMode::Observer);
    assert_eq!(unit.extensions[0].target_name, "elm-mgr");
    assert_eq!(
        unit.extensions[0].handler_contract.as_str(),
        "demo.provider@1"
    );
    assert_eq!(unit.extensions[0].priority, -7);
    assert_eq!(unit.provider_ports[0].contract.as_str(), "demo.provider@1");
    assert_eq!(unit.provider_ports[0].handler_symbol, None);
}

#[test]
fn eki_parser_accepts_provider_port_handler_symbol() {
    let block = eki_provider_port_block_with_handler(
        "demo.native.provider@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        Some("demo_provider_call"),
    );
    assert_eq!(block.len(), 8 + ELM_EKI_PROVIDER_PORT_RECORD_SIZE);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-provider-handler", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::ProviderPorts, block),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    assert_eq!(unit.provider_ports.len(), 1);
    assert_eq!(
        unit.provider_ports[0].contract.as_str(),
        "demo.native.provider@1"
    );
    assert_eq!(
        unit.provider_ports[0].handler_symbol.as_deref(),
        Some("demo_provider_call")
    );
    assert_eq!(unit.provider_ports[0].snapshot_symbol, None);
}

#[test]
fn eki_parser_accepts_provider_port_snapshot_symbol() {
    let block = eki_provider_port_block_with_symbols(
        "demo.native.snapshot@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        Some("demo_provider_call"),
        Some("demo_provider_snapshot"),
    );
    assert_eq!(block.len(), 8 + ELM_EKI_PROVIDER_PORT_RECORD_SIZE);
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-provider-snapshot", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::ProviderPorts, block),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    assert_eq!(unit.provider_ports.len(), 1);
    assert_eq!(
        unit.provider_ports[0].handler_symbol.as_deref(),
        Some("demo_provider_call")
    );
    assert_eq!(
        unit.provider_ports[0].snapshot_symbol.as_deref(),
        Some("demo_provider_snapshot")
    );
}

#[test]
fn eki_parser_accepts_lifecycle_less_declarative_unit() {
    let image = eki_image(&[(
        ElmEkiBlockKind::Manifest,
        eki_manifest_block("missing-hooks", "0.1.0", ElmKind::Service),
    )]);

    let unit = parse_eki_ebi_unit(&image).unwrap();
    assert!(unit.lifecycle_hooks.is_none());
}

#[test]
fn eki_parser_rejects_wrong_lifecycle_hook_symbol() {
    let mut hooks = eki_lifecycle_hooks_block();
    let record = 8;
    let symbol_offset = record + 20;
    hooks[symbol_offset..symbol_offset + ELM_EBI_HOOK_ON_INITIALIZE.len()].fill(0);
    fixed_copy(&mut hooks, symbol_offset, ELM_EBI_SYMBOL_NAME_LEN, "init");
    write_u16(&mut hooks, record + 12, 4);

    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("bad-hooks", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, hooks),
    ]);

    assert_eq!(
        parse_eki_ebi_unit(&image),
        Err(ElmEbiLoadStatus::InvalidManifest)
    );
}

#[test]
fn eki_parser_rejects_bad_magic() {
    let mut image = eki_image(&[(
        ElmEkiBlockKind::Manifest,
        eki_manifest_block("bad-eki", "0.1.0", ElmKind::Service),
    )]);
    image[0] = b'X';

    assert_eq!(
        parse_eki_ebi_unit(&image),
        Err(ElmEbiLoadStatus::InvalidUnit)
    );
}

#[test]
fn current_context_policy_check_uses_entered_snapshot() {
    assert!(crate::current_context().is_none());
    assert!(crate::current_cell_allows(
        crate::ELM_CELL_POLICY_ALLOW_PROVIDER
    ));

    let context = crate::ElmContext::new(
        crate::ElmId(42),
        Some(crate::ELM_MGR_BUILTIN_ID),
        crate::Generation(7),
        crate::ElmState::Active,
        crate::ElmLifecyclePhase::Initialize,
        0,
    )
    .with_allowed_actions(crate::ELM_CELL_POLICY_ALLOW_EVENT);

    {
        let _guard = crate::enter_current_context(&context).expect("测试上下文必须可进入");
        assert_eq!(crate::current_cell(), Some(crate::ElmId(42)));
        assert!(crate::current_cell_allows(
            crate::ELM_CELL_POLICY_ALLOW_EVENT
        ));
        let denied = crate::check_current_cell(crate::ELM_CELL_POLICY_ALLOW_PROVIDER);
        assert!(!denied.allowed);
        assert_eq!(denied.status, crate::ELM_MGR_STATUS_PERMISSION);
    }

    assert!(crate::current_context().is_none());
}

#[test]
fn manager_api_descriptor_is_available_from_elm_root() {
    let record = crate::ElmMgrApiDescriptor::new(
        1,
        crate::ELM_MGR_BUILTIN_ID.0,
        crate::ELM_MGR_API_KIND_EVENT,
        crate::ELM_MGR_API_FLAG_STABLE,
        crate::ElmMgrCallKind::SubscribeEvent as u32,
        "elm.event",
        "subscribe",
        "elm.event.subscribe@1",
    );

    assert_eq!(record.kind, crate::ELM_MGR_API_KIND_EVENT);
    assert_ne!(record.flags & crate::ELM_MGR_API_FLAG_STABLE, 0);
}

#[test]
fn elmapi_compatibility_selects_highest_common_version() {
    let compatibility = crate::ElmEbiApiCompatibility::new(0, 0, [1]).unwrap();
    assert_eq!(compatibility.select_highest_common(&[1]), Some(1));
    assert_eq!(compatibility.select_highest_common(&[]), None);
    assert!(crate::ElmEbiApiCompatibility::new(0, 0, [1, 1]).is_err());
}

#[test]
fn elmapi_v1_tables_have_fixed_layout() {
    assert_eq!(core::mem::size_of::<crate::ElmApiContextV1>(), 56);
    assert_eq!(core::mem::size_of::<crate::ElmApiNamespaceV1>(), 40);
    assert_eq!(core::mem::size_of::<crate::ElmRuntimeApiV1>(), 56);
    assert_eq!(core::mem::size_of::<crate::ElmManagementApiV1>(), 16);
    assert_eq!(core::mem::size_of::<crate::ElmApiRootV1>(), 48);
}

#[test]
fn runtime_namespace_descriptor_requires_canonical_identifier_and_policy() {
    static TABLE: u64 = 0;
    let valid = crate::ElmApiNamespaceDescriptorV1::new(
        "elm.test",
        1,
        crate::ELM_API_NAMESPACE_FLAG_PUBLIC,
        1,
        &TABLE,
    );
    assert!(valid.validate());

    let invalid_name = crate::ElmApiNamespaceDescriptorV1::new(
        "kernel.test",
        1,
        crate::ELM_API_NAMESPACE_FLAG_PUBLIC,
        1,
        &TABLE,
    );
    assert!(!invalid_name.validate());

    let missing_policy = crate::ElmApiNamespaceDescriptorV1::new("elm.test", 1, 0, 1, &TABLE);
    assert!(!missing_policy.validate());

    let ambiguous_policy = crate::ElmApiNamespaceDescriptorV1::new(
        "elm.test",
        1,
        crate::ELM_API_NAMESPACE_FLAG_PUBLIC | crate::ELM_API_NAMESPACE_FLAG_MANAGEMENT,
        1,
        &TABLE,
    );
    assert!(!ambiguous_policy.validate());

    let malformed_namespace = crate::ElmApiNamespaceDescriptorV1::new(
        "elm..test",
        1,
        crate::ELM_API_NAMESPACE_FLAG_PUBLIC,
        1,
        &TABLE,
    );
    assert!(!malformed_namespace.validate());
}
