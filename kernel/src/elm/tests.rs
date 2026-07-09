use ktest::ktest;

use core::any::Any;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use elm_model::{
    ELM_ACTION_OPCODE_INVOKE, ELM_ACTION_RESULT_HEALTH, ELM_CALL_STATUS_BUSY,
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_NOT_FOUND, ELM_CALL_STATUS_OK,
    ELM_CALL_STATUS_UNSUPPORTED, ELM_EBI_HOOK_ON_FINALIZE, ELM_EBI_HOOK_ON_INITIALIZE,
    ELM_EBI_NAME_LEN, ELM_EBI_SYMBOL_NAME_LEN, ELM_EKI_BLOCK_DESC_SIZE, ELM_EKI_FORMAT_VERSION,
    ELM_EKI_HEADER_SIZE, ELM_EKI_MAGIC, ELM_EKI_MANIFEST_NAME_LEN, ELM_EKI_MANIFEST_VERSION_LEN,
    ELM_EKI_SYMBOL_LOCATION_RECORD_SIZE, ELM_HEALTH_CHECK_AUDITS, ELM_HEALTH_CHECK_BINDINGS,
    ELM_HEALTH_CHECK_CELLS, ELM_HEALTH_CHECK_EVENTS, ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_MENU,
    ELM_HEALTH_CHECK_NATIVE_CAPABILITIES, ELM_HEALTH_CHECK_PORTS, ELM_HEALTH_CHECK_PROVIDERS,
    ELM_HEALTH_CHECK_RUNTIME_PORTS, ELM_HEALTH_CHECK_TODO_REGISTRY, ELM_KERNEL_PROVIDER_FLAG_NONE,
    ELM_LIFECYCLE_REASON_HOOK_FAILED, ELM_MENU_FLAG_TODO, ELM_MGR_ACTION_PROVIDER_ASYNC,
    ELM_MGR_ACTION_PROVIDER_INVOKE, ELM_MGR_API_KIND_SUBSYSTEM, ELM_MGR_RELATION_POINT_LEN,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_TODO, ELM_MGR_STATUS_UNSUPPORTED, ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED,
    ELM_NEXUS_CONTRACT_LEN, ELM_POLICY_BLOCK_INVALID_STATE, ELM_POLICY_BLOCK_LEASE_BUSY,
    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED, ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
    ELM_POLICY_BLOCK_PORT_TODO, ELM_POLICY_BLOCK_PROVIDER_BUSY,
    ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, ELM_POLICY_BLOCK_RESOURCE_QUOTA,
    ELM_PROVIDER_ASYNC_QUEUE_LIMIT, ELM_PROVIDER_FLAG_DYNAMIC, ELM_PROVIDER_FLAG_KERNEL_BACKEND,
    ELM_PROVIDER_FLAG_NATIVE_BACKEND, ELM_PROVIDER_FLAG_TODO_BACKEND,
    ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED, ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE,
    ElmActionInvokeRequest, ElmCallFrame, ElmContext, ElmCoreHealthHeader, ElmCoreHealthRecord,
    ElmEbiArch, ElmEbiLifecycleHookKind, ElmEbiLifecycleHooks, ElmEbiLoadStatus, ElmEbiMenuDecl,
    ElmEbiRustHookSignature, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiSourceKind,
    ElmEbiSourceRequest, ElmEbiTarget, ElmEbiUnit, ElmEkiBlockKind, ElmError, ElmId,
    ElmKernelProviderSnapshotPage, ElmKernelProviderSpec, ElmKind, ElmLifecyclePhase, ElmManifest,
    ElmMenuItemKind, ElmMgrApiRegistryHeader, ElmMgrAuditHeader, ElmMgrCallHeader, ElmMgrCallKind,
    ElmMgrEventSubscribeRequest, ElmMgrEventUnsubscribeRequest, ElmMgrPolicyInfo,
    ElmMgrRelationKind, ElmMgrResponseHeader, ElmMgrSubscribedEventReadHeader,
    ElmMgrSubscribedEventReadRequest, ElmName, ElmNativeCapabilityHeader, ElmNativeEntryFrameV1,
    ElmNexusBindPlanResponse, ElmNexusBindRequest, ElmNexusUnbindRequest, ElmPortAccessPolicy,
    ElmProviderAsyncCancelRequest, ElmProviderAsyncPollRequest, ElmProviderAsyncState,
    ElmProviderAsyncSubmitRequest, ElmProviderInvokeRequest, ElmProviderInvokeResponse,
    ElmProviderPortRegisterRequest, ElmProviderPortRegisterResponse, ElmProviderPortStatsHeader,
    ElmProviderQueueStatsHeader, ElmProviderSnapshotHeader, ElmProviderSnapshotRequest,
    ElmReplaceCellRequestV1, ElmReplyFrame, ElmResult, ElmState, ElmTodoRegistryHeader,
    ElmTodoRegistryRecord, ElmVersion, FlowDirection, FlowMode, Generation, state_code,
};

use super::core::{ELM_MGR_ID, ElmCore, ElmLifecycleExecutor};
use super::mgr_channel::dispatch_mgr_call_on_core;

struct TestElmDeviceFunction;

impl general::dev::function::DeviceFunction for TestElmDeviceFunction {
    fn class_id(&self) -> general::dev::function::DeviceClassId {
        general::dev::function::DeviceClassId::new("elmtest")
    }

    fn dev_name(&self) -> &str {
        "elm-claim-test"
    }

    fn mark_gone(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn manifest(name: &str, kind: ElmKind) -> ElmManifest {
    ElmManifest::new(
        ElmName::new(name).unwrap(),
        ElmVersion::new("0.1.0").unwrap(),
        kind,
    )
}

fn lifecycle_hooks() -> ElmEbiLifecycleHooks {
    ElmEbiLifecycleHooks::rust_context_result_v1()
}

fn menu_unit(name: &str) -> ElmEbiUnit {
    ElmEbiUnit::new(
        manifest(name, ElmKind::Extension),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_menu(ElmEbiMenuDecl::new(
        ElmMenuItemKind::Action,
        0,
        "诊断入口",
        "ELM 第一阶段菜单单元",
        "elm/test/menu",
    ))
}

fn native_unit(name: &str) -> ElmEbiUnit {
    ElmEbiUnit::new(
        manifest(name, ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::Code, 4096, 0))
}

#[derive(Default)]
struct RecordingLifecycleExecutor {
    initialize_calls: u32,
    finalize_calls: u32,
    fail_initialize: bool,
    fail_finalize: bool,
    last_initialize_cell: u64,
    last_finalize_cell: u64,
}

impl RecordingLifecycleExecutor {
    fn fail_initialize() -> Self {
        Self {
            fail_initialize: true,
            ..Self::default()
        }
    }

    fn fail_finalize() -> Self {
        Self {
            fail_finalize: true,
            ..Self::default()
        }
    }
}

impl ElmLifecycleExecutor for RecordingLifecycleExecutor {
    fn on_initialize(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        assert_eq!(context.phase(), ElmLifecyclePhase::Initialize);
        assert_eq!(context.parent_id(), Some(ELM_MGR_ID));
        assert_eq!(context.state(), ElmState::Loaded);
        self.initialize_calls += 1;
        self.last_initialize_cell = context.cell_id().0;
        if self.fail_initialize {
            return Err(ElmError::PermissionDenied);
        }
        context.set_state(ElmState::Active);
        Ok(())
    }

    fn on_finalize(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        assert_eq!(context.phase(), ElmLifecyclePhase::Finalize);
        assert_eq!(context.parent_id(), Some(ELM_MGR_ID));
        self.finalize_calls += 1;
        self.last_finalize_cell = context.cell_id().0;
        if self.fail_finalize {
            return Err(ElmError::PermissionDenied);
        }
        Ok(())
    }
}

static TEST_PROVIDER_REVOKES: AtomicUsize = AtomicUsize::new(0);
static TEST_NATIVE_ENTRY_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn test_native_entry_ok(frame: *mut ElmNativeEntryFrameV1) -> i32 {
    if frame.is_null() {
        return -1;
    }
    // 安全性：调用方按照 ELM native entry v1 约定传入可写帧指针。
    let frame = unsafe { &mut *frame };
    if frame.cell_id == 7
        && frame.parent_id == ELM_MGR_ID.0
        && frame.generation == 3
        && frame.state == state_code(ElmState::Active)
        && frame.exit_code == 0
    {
        TEST_NATIVE_ENTRY_CALLS.fetch_add(1, Ordering::Relaxed);
        0
    } else {
        -1
    }
}

unsafe extern "C" fn test_native_entry_returns_error(frame: *mut ElmNativeEntryFrameV1) -> i32 {
    if !frame.is_null() {
        TEST_NATIVE_ENTRY_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    -1
}

unsafe extern "C" fn test_native_entry_mutates_frame(frame: *mut ElmNativeEntryFrameV1) -> i32 {
    if frame.is_null() {
        return -1;
    }
    // 安全性：调用方按照 ELM native entry v1 约定传入可写帧指针。
    unsafe {
        (*frame).cell_id = 0xdead;
    }
    TEST_NATIVE_ENTRY_CALLS.fetch_add(1, Ordering::Relaxed);
    0
}

fn test_revoke_provider_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_OK)
}

fn test_revoke_provider_on_revoke(
    binding: Option<elm_model::BindingId>,
    lease: Option<elm_model::LeaseId>,
) {
    if binding.is_some() && lease.is_some() {
        TEST_PROVIDER_REVOKES.fetch_add(1, Ordering::Relaxed);
    }
}

static TEST_REVOKE_PROVIDERS: [ElmKernelProviderSpec; 1] = [ElmKernelProviderSpec::new(
    "elm.test",
    "revoke",
    "elm.test.revoke@1",
    ELM_MGR_API_KIND_SUBSYSTEM,
    0,
    0,
    "test.revoke@1",
    FlowDirection::Control,
    FlowMode::Shared,
    ElmPortAccessPolicy::Internal,
    true,
    ELM_KERNEL_PROVIDER_FLAG_NONE,
    test_revoke_provider_invoke,
    None,
    Some(test_revoke_provider_on_revoke),
)];

fn test_snapshot_provider_snapshot(out: &mut [u8]) -> Result<usize, i32> {
    let payload = b"snapshot-ok";
    out[..payload.len()].copy_from_slice(payload);
    Ok(payload.len())
}

static TEST_SNAPSHOT_PROVIDERS: [ElmKernelProviderSpec; 1] = [ElmKernelProviderSpec::new(
    "elm.test",
    "snapshot",
    "elm.test.snapshot@1",
    ELM_MGR_API_KIND_SUBSYSTEM,
    0,
    0,
    "test.snapshot@1",
    FlowDirection::Control,
    FlowMode::Shared,
    ElmPortAccessPolicy::Internal,
    true,
    ELM_KERNEL_PROVIDER_FLAG_NONE,
    test_revoke_provider_invoke,
    Some(test_snapshot_provider_snapshot),
    None,
)];

fn test_paged_snapshot_provider_snapshot(
    cursor: u32,
    out: &mut [u8],
) -> Result<ElmKernelProviderSnapshotPage, i32> {
    let payload = match cursor {
        0 => b"page-a".as_slice(),
        1 => b"page-b".as_slice(),
        _ => return Err(ELM_MGR_STATUS_NOT_FOUND),
    };
    out[..payload.len()].copy_from_slice(payload);
    if cursor == 0 {
        Ok(ElmKernelProviderSnapshotPage::more(payload.len(), 1, 1))
    } else {
        Ok(ElmKernelProviderSnapshotPage::final_page(payload.len(), 1))
    }
}

static TEST_PAGED_SNAPSHOT_PROVIDERS: [ElmKernelProviderSpec; 1] = [ElmKernelProviderSpec::new(
    "elm.test",
    "snapshot-paged",
    "elm.test.snapshot.paged@1",
    ELM_MGR_API_KIND_SUBSYSTEM,
    0,
    0,
    "test.snapshot.paged@1",
    FlowDirection::Control,
    FlowMode::Shared,
    ElmPortAccessPolicy::Internal,
    true,
    ELM_KERNEL_PROVIDER_FLAG_NONE,
    test_revoke_provider_invoke,
    None,
    None,
)
.with_paged_snapshot(test_paged_snapshot_provider_snapshot)];

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
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
        16 + elm_model::ELM_MENU_LABEL_LEN
            + elm_model::ELM_MENU_DESCRIPTION_LEN
            + elm_model::ELM_MENU_ROUTE_LEN
    ];
    write_u32(&mut out, 0, ElmMenuItemKind::Action as u32);
    write_u16(&mut out, 8, label.len() as u16);
    write_u16(&mut out, 10, description.len() as u16);
    write_u16(&mut out, 12, route.len() as u16);
    fixed_copy(&mut out, 16, elm_model::ELM_MENU_LABEL_LEN, label);
    fixed_copy(
        &mut out,
        16 + elm_model::ELM_MENU_LABEL_LEN,
        elm_model::ELM_MENU_DESCRIPTION_LEN,
        description,
    );
    fixed_copy(
        &mut out,
        16 + elm_model::ELM_MENU_LABEL_LEN + elm_model::ELM_MENU_DESCRIPTION_LEN,
        elm_model::ELM_MENU_ROUTE_LEN,
        route,
    );
    out
}

fn eki_entry_block(symbol: &str) -> Vec<u8> {
    let mut out = vec![0; 8 + ELM_EBI_SYMBOL_NAME_LEN];
    write_u16(&mut out, 0, symbol.len() as u16);
    fixed_copy(&mut out, 8, ELM_EBI_SYMBOL_NAME_LEN, symbol);
    out
}

fn eki_segments_block(
    kind: ElmEbiSegmentKind,
    flags: u32,
    file_size: u64,
    mem_size: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, 1);
    push_u32(&mut out, 0);
    push_u32(&mut out, kind as u32);
    push_u32(&mut out, flags);
    push_u64(&mut out, file_size);
    push_u64(&mut out, mem_size);
    push_u64(&mut out, 0);
    out
}

fn eki_lifecycle_hooks_block() -> Vec<u8> {
    let record = 8;
    let hook_record_size = 20 + ELM_EBI_SYMBOL_NAME_LEN;
    let mut out = vec![0; record + 2 * hook_record_size];
    write_u32(&mut out, 0, 2);
    write_lifecycle_hook_record(
        &mut out,
        record,
        ElmEbiLifecycleHookKind::Initialize,
        ELM_EBI_HOOK_ON_INITIALIZE,
    );
    write_lifecycle_hook_record(
        &mut out,
        record + hook_record_size,
        ElmEbiLifecycleHookKind::Finalize,
        ELM_EBI_HOOK_ON_FINALIZE,
    );
    out
}

fn eki_symbol_locations_block(entries: &[(&str, u32, u64, u64)]) -> Vec<u8> {
    let record_size = ELM_EKI_SYMBOL_LOCATION_RECORD_SIZE;
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

fn write_lifecycle_hook_record(
    out: &mut [u8],
    offset: usize,
    kind: ElmEbiLifecycleHookKind,
    symbol: &str,
) {
    write_u32(out, offset, kind as u32);
    write_u16(out, offset + 8, elm_model::ELM_EBI_RUST_ABI_VERSION);
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

fn eki_extension_point_block(point: &str, contract: &str) -> Vec<u8> {
    let record = 8;
    let mut out = vec![0; record + 8 + ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN];
    write_u32(&mut out, 0, 1);
    write_u16(&mut out, record, point.len() as u16);
    write_u16(&mut out, record + 2, contract.len() as u16);
    fixed_copy(&mut out, record + 8, ELM_MGR_RELATION_POINT_LEN, point);
    fixed_copy(
        &mut out,
        record + 8 + ELM_MGR_RELATION_POINT_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        contract,
    );
    out
}

fn eki_extension_block(target_name: &str, point: &str, contract: &str) -> Vec<u8> {
    let record = 8;
    let mut out =
        vec![
            0;
            record + 8 + ELM_EBI_NAME_LEN + ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN
        ];
    write_u32(&mut out, 0, 1);
    write_u16(&mut out, record, target_name.len() as u16);
    write_u16(&mut out, record + 2, point.len() as u16);
    write_u16(&mut out, record + 4, contract.len() as u16);
    fixed_copy(&mut out, record + 8, ELM_EBI_NAME_LEN, target_name);
    fixed_copy(
        &mut out,
        record + 8 + ELM_EBI_NAME_LEN,
        ELM_MGR_RELATION_POINT_LEN,
        point,
    );
    fixed_copy(
        &mut out,
        record + 8 + ELM_EBI_NAME_LEN + ELM_MGR_RELATION_POINT_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        contract,
    );
    out
}

fn eki_provider_port_block(
    contract: &str,
    access: ElmPortAccessPolicy,
    direction: FlowDirection,
    mode: FlowMode,
) -> Vec<u8> {
    let record = 8;
    let mut out = vec![0; record + 24 + ELM_NEXUS_CONTRACT_LEN];
    write_u32(&mut out, 0, 1);
    write_u32(&mut out, record, access as u32);
    write_u32(&mut out, record + 4, direction as u32);
    write_u32(&mut out, record + 8, mode as u32);
    write_u16(&mut out, record + 16, contract.len() as u16);
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
    write_u16(&mut out, 10, elm_model::ELM_EBI_ABI_VERSION);
    write_u32(&mut out, 12, ELM_EKI_HEADER_SIZE as u32);
    let file_size = out.len() as u64;
    write_u64(&mut out, 16, file_size);
    write_u64(&mut out, 24, ELM_EKI_HEADER_SIZE as u64);
    write_u32(&mut out, 40, ElmEbiArch::Any as u32);
    write_u16(&mut out, 44, 1);
    write_u32(&mut out, 48, block_count as u32);
    out
}

fn ebi_source_payload(kind: ElmEbiSourceKind, payload: &[u8]) -> Vec<u8> {
    let request = ElmEbiSourceRequest::new(kind, payload.len() as u32);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.source_kind);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.payload_len);
    push_u32(&mut out, request.reserved);
    out.extend_from_slice(payload);
    out
}

fn raw_mgr_call(kind: u32, flags: u32, reserved: u32, payload: &[u8]) -> Vec<u8> {
    let header = ElmMgrCallHeader {
        kind,
        flags,
        payload_len: payload.len() as u32,
        reserved,
    };
    let mut out = Vec::new();
    push_u32(&mut out, header.kind);
    push_u32(&mut out, header.flags);
    push_u32(&mut out, header.payload_len);
    push_u32(&mut out, header.reserved);
    out.extend_from_slice(payload);
    out
}

fn mgr_call(kind: ElmMgrCallKind, payload: &[u8]) -> Vec<u8> {
    raw_mgr_call(kind as u32, 0, 0, payload)
}

fn provider_register_payload(request: &ElmProviderPortRegisterRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.owner_cell_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.access_policy);
    push_u32(&mut out, request.direction);
    push_u32(&mut out, request.mode);
    push_u16(&mut out, request.contract_len);
    push_u16(&mut out, request.reserved0);
    push_u32(&mut out, request.reserved1);
    out.extend_from_slice(&request.contract);
    out
}

fn nexus_bind_payload(request: &ElmNexusBindRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.cell_id);
    push_u64(&mut out, request.port_id);
    push_u32(&mut out, request.flags);
    push_u16(&mut out, request.contract_len);
    push_u16(&mut out, request.reserved);
    out.extend_from_slice(&request.contract);
    out
}

fn action_invoke_payload(request: &ElmActionInvokeRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.action_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn provider_invoke_payload(request: &ElmProviderInvokeRequest) -> Vec<u8> {
    let frame = request.frame;
    let mut out = Vec::new();
    push_u64(&mut out, frame.binding_id);
    push_u64(&mut out, frame.call_id);
    push_u32(&mut out, frame.opcode);
    push_u32(&mut out, frame.flags);
    push_u16(&mut out, frame.payload_len);
    push_u16(&mut out, frame.reserved0);
    push_u32(&mut out, frame.reserved1);
    out.extend_from_slice(&frame.payload);
    out
}

fn device_claim_payload(class_name: &str, dev_name: &str) -> Vec<u8> {
    let request = general::dev::elm::ElmDeviceClaimRequest::new(class_name, dev_name);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.flags);
    push_u16(&mut out, request.class_len);
    push_u16(&mut out, request.name_len);
    out.extend_from_slice(&request.class_name);
    out.extend_from_slice(&request.dev_name);
    out
}

fn vfs_lookup_payload(path: &str) -> Vec<u8> {
    let request = vfs::elm::ElmVfsLookupRequest::new(path);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.flags);
    push_u32(&mut out, request.dirfd_kind);
    push_u32(&mut out, request.lookup_flags);
    push_u16(&mut out, request.path_len);
    push_u16(&mut out, request.reserved);
    out.extend_from_slice(&request.path[..request.path_len as usize]);
    out
}

fn device_claim_snapshot_has_binding(payload: &[u8], binding_id: u64) -> bool {
    let header_size = core::mem::size_of::<general::dev::elm::ElmDeviceClaimSnapshotHeader>();
    if payload.len() < header_size {
        return false;
    }
    let record_size = read_u16(payload, 2) as usize;
    let record_count = read_u32(payload, 4) as usize;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if payload.len() >= offset + record_size && read_u64(payload, offset) == binding_id {
            return true;
        }
    }
    false
}

fn provider_snapshot_payload(request: &ElmProviderSnapshotRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.port_id);
    push_u64(&mut out, request.binding_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn event_subscribe_payload(request: &ElmMgrEventSubscribeRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.owner_cell_id);
    push_u32(&mut out, request.kind_filter);
    push_u32(&mut out, request.flags);
    push_u64(&mut out, request.cell_filter);
    push_u64(&mut out, request.port_filter);
    push_u64(&mut out, request.binding_filter);
    push_u64(&mut out, request.lease_filter);
    out
}

fn event_unsubscribe_payload(request: &ElmMgrEventUnsubscribeRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.subscription_id);
    push_u64(&mut out, request.owner_cell_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn subscribed_event_read_payload(request: &ElmMgrSubscribedEventReadRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.subscription_id);
    push_u64(&mut out, request.cursor);
    push_u32(&mut out, request.max_records);
    push_u32(&mut out, request.flags);
    out
}

fn provider_async_submit_payload(request: &ElmProviderAsyncSubmitRequest) -> Vec<u8> {
    let mut out = provider_invoke_payload(&ElmProviderInvokeRequest::new(request.frame));
    push_u32(&mut out, request.timeout_ms);
    push_u32(&mut out, request.result_ttl_ms);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn provider_async_poll_payload(request: &ElmProviderAsyncPollRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.ticket_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn provider_async_cancel_payload(request: &ElmProviderAsyncCancelRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.ticket_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn lifecycle_request_payload(request: &elm_model::ElmLifecycleRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.cell_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn replace_cell_payload(request: &ElmReplaceCellRequestV1, source_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.flags);
    push_u16(&mut out, request.source_kind);
    push_u16(&mut out, request.reserved0);
    push_u64(&mut out, request.target_cell_id);
    push_u32(&mut out, request.migration_limit);
    push_u32(&mut out, request.source_payload_len);
    push_u64(&mut out, request.reserved1);
    out.extend_from_slice(source_payload);
    out
}

fn response_status(bytes: &[u8]) -> i32 {
    read_i32(bytes, 0)
}

fn response_payload_len(bytes: &[u8]) -> usize {
    read_u32(bytes, 4) as usize
}

fn response_payload(bytes: &[u8]) -> &[u8] {
    let start = core::mem::size_of::<ElmMgrResponseHeader>();
    let end = start + response_payload_len(bytes);
    &bytes[start..end]
}

fn topology_has_relation(
    bytes: &[u8],
    kind: ElmMgrRelationKind,
    source: u64,
    target: u64,
    contract: &str,
    point: &str,
) -> bool {
    let header_size = core::mem::size_of::<elm_model::ElmMgrTopologyHeader>();
    let record_size = read_u16(bytes, 2) as usize;
    for index in 0..read_u32(bytes, 4) as usize {
        let offset = header_size + index * record_size;
        let contract_len = read_u16(bytes, offset + 24) as usize;
        let point_len = read_u16(bytes, offset + 26) as usize;
        if read_u32(bytes, offset) == kind as u32
            && read_u64(bytes, offset + 8) == source
            && read_u64(bytes, offset + 16) == target
            && contract_len == contract.len()
            && point_len == point.len()
            && &bytes[offset + 32..offset + 32 + contract_len] == contract.as_bytes()
            && &bytes[offset + 96..offset + 96 + point_len] == point.as_bytes()
        {
            return true;
        }
    }
    false
}

fn bind_mgr_action_provider(core: &mut ElmCore) -> (u64, ElmNexusBindPlanResponse) {
    let action = core
        .menu_items()
        .iter()
        .find(|item| item.route == "elm/mgr/health")
        .unwrap()
        .action;
    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, 4, "mgr.action.invoke@1");
    let response = core.commit_bind(bind);
    assert_eq!(response.allowed, 1);
    assert_eq!(response.status, ELM_MGR_STATUS_OK);
    (action.0, response)
}

fn provider_port_id_by_contract(core: &mut ElmCore, contract: &str) -> Option<u64> {
    let bytes = core.provider_ports_bytes();
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(&bytes, 2) as usize;
    let record_count = read_u32(&bytes, 4) as usize;
    let contract_offset = record_size.saturating_sub(ELM_NEXUS_CONTRACT_LEN);
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        let contract_len = read_u16(&bytes, offset + 40) as usize;
        if contract_len == contract.len()
            && &bytes[offset + contract_offset..offset + contract_offset + contract_len]
                == contract.as_bytes()
        {
            return Some(read_u64(&bytes, offset));
        }
    }
    None
}

fn provider_stats_by_port(core: &mut ElmCore, port_id: u64) -> Option<(u32, u64, u64, u64)> {
    let bytes = core.provider_stats_bytes();
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(&bytes, 2) as usize;
    let record_count = read_u32(&bytes, 4) as usize;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(&bytes, offset) == port_id {
            return Some((
                read_u32(&bytes, offset + 20),
                read_u64(&bytes, offset + 24),
                read_u64(&bytes, offset + 32),
                read_u64(&bytes, offset + 40),
            ));
        }
    }
    None
}

#[allow(clippy::type_complexity)]
fn provider_queue_stats_by_port(
    core: &mut ElmCore,
    port_id: u64,
    now_ns: u64,
) -> Option<(u32, u32, u32, u64, u64, u64, u64)> {
    let bytes = core.provider_queue_bytes(now_ns);
    let header_size = core::mem::size_of::<ElmProviderQueueStatsHeader>();
    let record_size = read_u16(&bytes, 2) as usize;
    let record_count = read_u32(&bytes, 4) as usize;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(&bytes, offset) == port_id {
            return Some((
                read_u32(&bytes, offset + 8),
                read_u32(&bytes, offset + 12),
                read_u32(&bytes, offset + 16),
                read_u64(&bytes, offset + 32),
                read_u64(&bytes, offset + 40),
                read_u64(&bytes, offset + 48),
                read_u64(&bytes, offset + 56),
            ));
        }
    }
    None
}

#[ktest]
fn elm_builtin_mgr_init_health_is_clean() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    assert_eq!(core.cells().len(), 1);
    assert_eq!(core.cells()[0].id, ELM_MGR_ID);
    assert_eq!(core.cells()[0].state, ElmState::Active);
    assert_eq!(core.cells()[0].ebi_source, ElmEbiSourceKind::Builtin);
    assert_eq!(core.cells()[0].resource_budget.max_provider_ports, 256);
    assert!(!core.cells()[0].isolated);
    assert_eq!(core.menu_items().len(), 1);
    assert_eq!(core.menu_items()[0].owner, ELM_MGR_ID);
    assert_eq!(core.menu_items()[0].route, "elm/mgr/health");
    assert_eq!(core.menu_items()[0].flags & ELM_MENU_FLAG_TODO, 0);

    let health = core.health_bytes();
    assert_eq!(read_i32(&health, 8), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(&health, 12), 0);
    assert_eq!(read_u32(&health, 4), 11);

    let record_size = read_u16(&health, 2) as usize;
    assert_eq!(record_size, core::mem::size_of::<ElmCoreHealthRecord>());
    let mut checks = 0u32;
    for index in 0..read_u32(&health, 4) as usize {
        let offset = core::mem::size_of::<elm_model::ElmCoreHealthHeader>() + index * record_size;
        let check_kind = read_u32(&health, offset);
        assert_eq!(read_i32(&health, offset + 4), ELM_MGR_STATUS_OK);
        checks |= 1 << check_kind;
    }
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_GRAPH), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_CELLS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_PORTS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_PROVIDERS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_BINDINGS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_RUNTIME_PORTS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_MENU), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_EVENTS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_AUDITS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_NATIVE_CAPABILITIES), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_TODO_REGISTRY), 0);
}

#[ktest]
fn elm_native_capability_snapshot_is_empty_after_builtin_init() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let bytes = core.native_capabilities_bytes();
    assert_eq!(
        read_u16(&bytes, 2) as usize,
        core::mem::size_of::<elm_model::ElmNativeCapabilityRecord>()
    );
    assert_eq!(read_u32(&bytes, 4), 0);
    assert_eq!(
        read_u32(&bytes, 8) & ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED,
        0
    );
    assert_eq!(read_u64(&bytes, 16), core.last_event_sequence());
    assert_eq!(
        bytes.len(),
        core::mem::size_of::<ElmNativeCapabilityHeader>()
    );
}

#[ktest]
fn elm_todo_registry_reports_static_and_dynamic_boundaries() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let bytes = core.todo_registry_bytes();
    assert_eq!(
        read_u16(&bytes, 2) as usize,
        core::mem::size_of::<ElmTodoRegistryRecord>()
    );
    assert!(read_u32(&bytes, 4) >= 4);
    assert!(read_u32(&bytes, 8) >= 4);
    for removed in [
        "runtime.elm_mgr_eki_boot",
        "runtime.resource_quota",
        "source.non_eki",
        "native.fault_isolation",
        "runtime.hot_replace_rebind",
    ] {
        assert!(
            !bytes
                .windows(removed.len())
                .any(|window| window == removed.as_bytes())
        );
    }
    assert!(
        bytes
            .windows("source.projection_remote".len())
            .any(|window| window == b"source.projection_remote")
    );
    assert!(
        bytes
            .windows("native.trap_recovery".len())
            .any(|window| window == b"native.trap_recovery")
    );
    assert!(
        bytes
            .windows("provider.snapshot_streaming".len())
            .any(|window| window == b"provider.snapshot_streaming")
    );
    assert!(
        !bytes
            .windows("runtime.running_call_cancel".len())
            .any(|window| window == b"runtime.running_call_cancel")
    );
    assert!(
        !bytes
            .windows("native.snapshot_paging".len())
            .any(|window| window == b"native.snapshot_paging")
    );
    assert_eq!(
        bytes.len(),
        core::mem::size_of::<ElmTodoRegistryHeader>()
            + read_u32(&bytes, 4) as usize * core::mem::size_of::<ElmTodoRegistryRecord>()
    );

    let register = ElmProviderPortRegisterRequest::new(
        ELM_MGR_ID.0,
        "elm.test.todo.dynamic@1",
        ElmPortAccessPolicy::Internal,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let response = core.register_provider_port(register);
    assert_eq!(response.status, ELM_MGR_STATUS_OK);

    let bytes =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryTodoRegistry, &[]));
    assert_eq!(response_status(&bytes), ELM_MGR_STATUS_OK);
    let payload = response_payload(&bytes);
    let record_size = read_u16(payload, 2) as usize;
    let record_count = read_u32(payload, 4) as usize;
    let mut found_dynamic_provider = false;
    for index in 0..record_count {
        let offset = core::mem::size_of::<ElmTodoRegistryHeader>() + index * record_size;
        let subject = read_u64(payload, offset + 16);
        if subject == response.port_id {
            found_dynamic_provider = true;
            assert_eq!(read_i32(payload, offset + 24), ELM_MGR_STATUS_TODO);
        }
    }
    assert!(found_dynamic_provider);

    let bad = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryTodoRegistry, &[1]),
    );
    assert_eq!(response_status(&bad), ELM_MGR_STATUS_INVALID);
}

#[ktest]
fn elm_menu_ebi_unit_waits_for_lifecycle_executor() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let response = core.load_ebi_unit(menu_unit("elm-test-menu"), ElmEbiArch::Riscv64);
    assert_eq!(response.status, ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(response.final_state, state_code(ElmState::Loaded));
    assert_eq!(core.menu_items().len(), 1);

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.parent, Some(ELM_MGR_ID));
    assert_eq!(cell.state, ElmState::Loaded);
    assert!(cell.lifecycle_hooks_declared);
    assert!(!cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);
}

#[ktest]
fn elm_menu_ebi_unit_activates_with_lifecycle_executor() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::default();

    let response = core.load_ebi_unit_with_lifecycle_executor(
        menu_unit("elm-test-menu-active"),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(response.final_state, state_code(ElmState::Active));
    assert_eq!(core.menu_items().len(), 2);
    assert_eq!(executor.initialize_calls, 1);
    assert_eq!(executor.finalize_calls, 0);

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Active);
    assert!(cell.lifecycle_executor_ready);
    assert!(cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);

    let detach = core.detach_cell_with_lifecycle_executor(ElmId(response.cell_id), &mut executor);
    assert_eq!(detach.status, ELM_MGR_STATUS_OK);
    assert_eq!(detach.final_state, state_code(ElmState::Retired));
    assert_eq!(executor.finalize_calls, 1);
    assert_eq!(executor.last_finalize_cell, response.cell_id);
    assert_eq!(core.menu_items().len(), 1);
    assert!(
        core.cells()
            .iter()
            .all(|cell| cell.id != ElmId(response.cell_id))
    );
}

#[ktest]
fn elm_menu_ebi_initialize_failure_quarantines_without_activation() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::fail_initialize();

    let response = core.load_ebi_unit_with_lifecycle_executor(
        menu_unit("elm-test-menu-init-fail"),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(response.status, ElmEbiLoadStatus::RuntimeRejected as i32);
    assert_eq!(response.final_state, state_code(ElmState::Quarantined));
    assert_eq!(response.reason, ELM_LIFECYCLE_REASON_HOOK_FAILED);
    assert_eq!(core.menu_items().len(), 1);
    assert_eq!(executor.initialize_calls, 1);
    assert_eq!(executor.finalize_calls, 0);

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Quarantined);
    assert!(!cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);
    assert!(cell.isolated);
    assert_eq!(cell.native_faults, 1);
    assert_ne!(
        cell.isolation_blocker & ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
        0
    );

    let detach = core.detach_cell_with_lifecycle_executor(ElmId(response.cell_id), &mut executor);
    assert_eq!(detach.status, ELM_MGR_STATUS_OK);
    assert_eq!(executor.finalize_calls, 0);
    assert_eq!(core.cells().len(), 1);
}

#[ktest]
fn elm_menu_ebi_finalize_failure_keeps_resources_for_diagnostics() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::fail_finalize();

    let response = core.load_ebi_unit_with_lifecycle_executor(
        menu_unit("elm-test-menu-finalize-fail"),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(core.menu_items().len(), 2);

    let detach = core.detach_cell_with_lifecycle_executor(ElmId(response.cell_id), &mut executor);
    assert_eq!(detach.status, ELM_MGR_STATUS_INVALID);
    assert_eq!(detach.reason, ELM_LIFECYCLE_REASON_HOOK_FAILED);
    assert_eq!(detach.final_state, state_code(ElmState::Quarantined));
    assert_eq!(executor.finalize_calls, 1);
    assert_eq!(core.menu_items().len(), 2);

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Quarantined);
    assert!(cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);
}

#[ktest]
fn elm_mgr_load_cell_empty_payload_reports_ebi_source_todo() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let response = dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &[]));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_TODO);

    let audits = core.audit_bytes();
    let audit_header_size = core::mem::size_of::<ElmMgrAuditHeader>();
    let audit_record_size = read_u16(&audits, 2) as usize;
    let audit_record_count = read_u32(&audits, 4) as usize;
    let last_audit = audit_header_size + (audit_record_count - 1) * audit_record_size;
    assert_ne!(
        read_u64(&audits, last_audit + 24) & ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
        0
    );
}

#[ktest]
fn elm_mgr_loads_menu_eki_source() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-menu-cell", "0.1.0", ElmKind::Extension),
        ),
        (
            ElmEkiBlockKind::Menu,
            eki_menu_block("EKI 菜单", "来自 EKI 的菜单项", "eki/menu"),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let payload = ebi_source_payload(ElmEbiSourceKind::Eki, &image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&response),
        core::mem::size_of::<elm_model::ElmLoadCellResponse>()
    );
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Loaded));
    assert_eq!(core.menu_items().len(), 1);
}

#[ktest]
fn elm_mgr_loads_native_eki_source_as_native_todo() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let code = vec![0x13, 0, 0, 0];
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-native-cell", "0.1.0", ElmKind::Service),
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
    let payload = ebi_source_payload(ElmEbiSourceKind::Eki, &image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Loaded));
    let cell_id = read_u64(load, 0);
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(cell_id))
        .unwrap();
    assert_eq!(cell.native_segment_count, 1);
    assert_eq!(cell.native_import_count, 0);
    assert_eq!(cell.native_export_count, 0);
    assert!(cell.lifecycle_hooks_declared);
    assert!(!cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);
}

#[ktest]
fn elm_mgr_keeps_eki_entry_pending_without_image_ops() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let code = vec![0x13, 0, 0, 0];
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-entry-cell", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::Entry, eki_entry_block("elm_main")),
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
                ("elm_main", 0, 0, 2),
            ]),
        ),
    ]);
    let payload = ebi_source_payload(ElmEbiSourceKind::Eki, &image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Loaded));
    let cell_id = read_u64(load, 0);
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Loaded);
    assert!(!cell.lifecycle_initialized);
}

#[ktest]
fn elm_native_entry_accepts_valid_frame() {
    TEST_NATIVE_ENTRY_CALLS.store(0, Ordering::Relaxed);

    let result = super::native::test_call_native_entry(
        test_native_entry_ok as usize,
        ElmId(7),
        Some(ELM_MGR_ID),
        Generation(3),
        ElmState::Active,
    );

    assert!(result.is_ok());
    assert_eq!(TEST_NATIVE_ENTRY_CALLS.load(Ordering::Relaxed), 1);
}

#[ktest]
fn elm_native_entry_rejects_error_return() {
    TEST_NATIVE_ENTRY_CALLS.store(0, Ordering::Relaxed);

    let result = super::native::test_call_native_entry(
        test_native_entry_returns_error as usize,
        ElmId(7),
        Some(ELM_MGR_ID),
        Generation(3),
        ElmState::Active,
    );

    assert!(result.is_err());
    assert_eq!(TEST_NATIVE_ENTRY_CALLS.load(Ordering::Relaxed), 1);
}

#[ktest]
fn elm_native_entry_rejects_frame_mutation() {
    TEST_NATIVE_ENTRY_CALLS.store(0, Ordering::Relaxed);

    let result = super::native::test_call_native_entry(
        test_native_entry_mutates_frame as usize,
        ElmId(7),
        Some(ELM_MGR_ID),
        Generation(3),
        ElmState::Active,
    );

    assert!(result.is_err());
    assert_eq!(TEST_NATIVE_ENTRY_CALLS.load(Ordering::Relaxed), 1);
}

#[ktest]
fn elm_native_import_rebind_rewrites_absolute_slot() {
    let mut slot = 0x1000_u64;
    let result = super::native::test_rewrite_import_abs64(&mut slot, 0xfeed_cafe);

    assert!(result.is_ok());
    assert_eq!(slot, 0xfeed_cafe);
}

#[ktest]
fn elm_mgr_rejects_corrupt_eki_source() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut image = eki_image(&[(
        ElmEkiBlockKind::Manifest,
        eki_manifest_block("bad-eki-cell", "0.1.0", ElmKind::Service),
    )]);
    image[0] = b'X';
    let payload = ebi_source_payload(ElmEbiSourceKind::Eki, &image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);
}

#[ktest]
fn elm_mgr_replace_cell_rejects_legacy_lifecycle_payload_shape() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let legacy = lifecycle_request_payload(&elm_model::ElmLifecycleRequest::new(ELM_MGR_ID.0));

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::ReplaceCell, &legacy));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);
}

#[ktest]
fn elm_mgr_replace_cell_non_eki_source_reports_source_todo() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let request =
        ElmReplaceCellRequestV1::new(ELM_MGR_ID.0, ElmEbiSourceKind::Projection as u16, 0);
    let payload = replace_cell_payload(&request, &[]);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::ReplaceCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_TODO);

    let audits = core.audit_bytes();
    let audit_header_size = core::mem::size_of::<ElmMgrAuditHeader>();
    let audit_record_size = read_u16(&audits, 2) as usize;
    let audit_record_count = read_u32(&audits, 4) as usize;
    let last_audit = audit_header_size + (audit_record_count - 1) * audit_record_size;
    assert_ne!(
        read_u64(&audits, last_audit + 24) & ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
        0
    );
}

#[ktest]
fn elm_mgr_replace_cell_eki_source_enters_real_preflight_path() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let original_image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("replace-target", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let original_payload = ebi_source_payload(ElmEbiSourceKind::Eki, &original_image);
    let load = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::LoadCell, &original_payload),
    );
    assert_eq!(response_status(&load), ELM_MGR_STATUS_OK);
    let load_payload = response_payload(&load);
    assert_eq!(
        read_i32(load_payload, 8),
        ElmEbiLoadStatus::NativeCodeTodo as i32
    );
    let cell_id = read_u64(load_payload, 0);

    let replacement_image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("replace-target", "0.1.1", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let request = ElmReplaceCellRequestV1::new(
        cell_id,
        ElmEbiSourceKind::Eki as u16,
        replacement_image.len() as u32,
    );
    let payload = replace_cell_payload(&request, &replacement_image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::ReplaceCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let replace = response_payload(&response);
    assert_eq!(read_i32(replace, 8), ELM_MGR_STATUS_INVALID);
    assert_ne!(read_u64(replace, 32) & ELM_POLICY_BLOCK_INVALID_STATE, 0);
}

#[ktest]
fn elm_mgr_loads_eki_declarative_topology() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-topology-cell", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Dependencies,
            eki_dependency_block("elm-mgr", "core.event@1"),
        ),
        (
            ElmEkiBlockKind::ExtensionPoints,
            eki_extension_point_block("demo.point", "demo.point@1"),
        ),
        (
            ElmEkiBlockKind::Extensions,
            eki_extension_block("elm-mgr", "menu.item", "mgr.menu.item@1"),
        ),
        (
            ElmEkiBlockKind::ProviderPorts,
            eki_provider_port_block(
                "eki.provider@1",
                ElmPortAccessPolicy::Public,
                FlowDirection::Control,
                FlowMode::Shared,
            ),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let payload = ebi_source_payload(ElmEbiSourceKind::Eki, &image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Loaded));
    let cell_id = read_u64(load, 0);
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Loaded);
    assert!(cell.lifecycle_hooks_declared);
    assert!(!cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);

    let topology = core.topology_bytes();
    assert!(!topology_has_relation(
        &topology,
        ElmMgrRelationKind::Dependency,
        cell_id,
        ELM_MGR_ID.0,
        "core.event@1",
        "",
    ));
    assert!(!topology_has_relation(
        &topology,
        ElmMgrRelationKind::ExtensionPoint,
        cell_id,
        0,
        "demo.point@1",
        "demo.point",
    ));
    assert!(!topology_has_relation(
        &topology,
        ElmMgrRelationKind::Extension,
        cell_id,
        ELM_MGR_ID.0,
        "mgr.menu.item@1",
        "menu.item",
    ));

    let providers = core.provider_ports_bytes();
    assert!(
        !providers
            .windows("eki.provider@1".len())
            .any(|window| window == b"eki.provider@1")
    );

    let detach = core.detach_cell(ElmId(cell_id));
    assert_eq!(detach.status, ELM_MGR_STATUS_OK);
    let providers = core.provider_ports_bytes();
    assert!(
        !providers
            .windows("eki.provider@1".len())
            .any(|window| window == b"eki.provider@1")
    );
}

#[ktest]
fn elm_mgr_activates_eki_declarative_topology_with_lifecycle_executor() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("eki-topology-active-cell", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Dependencies,
            eki_dependency_block("elm-mgr", "core.event@1"),
        ),
        (
            ElmEkiBlockKind::ExtensionPoints,
            eki_extension_point_block("demo.point", "demo.point@1"),
        ),
        (
            ElmEkiBlockKind::Extensions,
            eki_extension_block("elm-mgr", "menu.item", "mgr.menu.item@1"),
        ),
        (
            ElmEkiBlockKind::ProviderPorts,
            eki_provider_port_block(
                "eki.active.provider@1",
                ElmPortAccessPolicy::Public,
                FlowDirection::Control,
                FlowMode::Shared,
            ),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let unit = elm_model::parse_eki_ebi_unit(&image).unwrap();
    let mut executor = RecordingLifecycleExecutor::default();

    let response =
        core.load_ebi_unit_with_lifecycle_executor(unit, ElmEbiArch::Riscv64, &mut executor);
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(response.final_state, state_code(ElmState::Active));
    assert_eq!(executor.initialize_calls, 1);
    let cell_id = response.cell_id;

    let topology = core.topology_bytes();
    assert!(topology_has_relation(
        &topology,
        ElmMgrRelationKind::Dependency,
        cell_id,
        ELM_MGR_ID.0,
        "core.event@1",
        "",
    ));
    assert!(topology_has_relation(
        &topology,
        ElmMgrRelationKind::ExtensionPoint,
        cell_id,
        0,
        "demo.point@1",
        "demo.point",
    ));
    assert!(topology_has_relation(
        &topology,
        ElmMgrRelationKind::Extension,
        cell_id,
        ELM_MGR_ID.0,
        "mgr.menu.item@1",
        "menu.item",
    ));

    let providers = core.provider_ports_bytes();
    assert!(
        providers
            .windows("eki.active.provider@1".len())
            .any(|window| window == b"eki.active.provider@1")
    );

    let detach = core.detach_cell_with_lifecycle_executor(ElmId(cell_id), &mut executor);
    assert_eq!(detach.status, ELM_MGR_STATUS_OK);
    assert_eq!(executor.finalize_calls, 1);
    let providers = core.provider_ports_bytes();
    assert!(
        !providers
            .windows("eki.active.provider@1".len())
            .any(|window| window == b"eki.active.provider@1")
    );
}

#[ktest]
fn elm_mgr_rejects_eki_extension_with_missing_target_without_half_cell() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("bad-topology-cell", "0.1.0", ElmKind::Service),
        ),
        (
            ElmEkiBlockKind::Extensions,
            eki_extension_block("missing-cell", "menu.item", "mgr.menu.item@1"),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let payload = ebi_source_payload(ElmEbiSourceKind::Eki, &image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_u64(load, 0), 0);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::RuntimeRejected as i32);
    assert_eq!(core.cells().len(), 1);
}

#[ktest]
fn elm_mgr_action_provider_invokes_builtin_health_action() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let action = core
        .menu_items()
        .iter()
        .find(|item| item.route == "elm/mgr/health")
        .unwrap()
        .action;
    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, 4, "mgr.action.invoke@1");
    let plan = core.preflight_bind(bind);
    assert_eq!(plan.allowed, 1);
    assert_eq!(plan.status, ELM_MGR_STATUS_OK);

    let bind_response = core.commit_bind(bind);
    assert_eq!(bind_response.allowed, 1);
    assert_eq!(bind_response.status, ELM_MGR_STATUS_OK);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action.0));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        1,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_OK);
    assert_eq!(
        response.reply.payload_len as usize,
        core::mem::size_of::<elm_model::ElmActionInvokeReply>()
    );
    assert_eq!(read_u64(&response.reply.payload, 0), action.0);
    assert_eq!(read_u64(&response.reply.payload, 16), ELM_MGR_ID.0);
    assert_eq!(
        read_u32(&response.reply.payload, 24),
        ELM_ACTION_RESULT_HEALTH
    );
    assert_eq!(read_i32(&response.reply.payload, 28), ELM_MGR_STATUS_OK);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(999_999));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        2,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_NOT_FOUND);

    let frame = ElmCallFrame::new(bind_response.binding_id, 3, 0xffff, &[]);
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_UNSUPPORTED);

    let frame = ElmCallFrame::new(bind_response.binding_id, 4, ELM_ACTION_OPCODE_INVOKE, b"x");
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_INVALID);

    let stats = core.provider_stats_bytes();
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(&stats, 2) as usize;
    let record_count = read_u32(&stats, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(&stats, offset) != 4 {
            continue;
        }
        found = true;
        assert_eq!(read_u64(&stats, offset + 8), ELM_MGR_ID.0);
        assert_eq!(
            read_u32(&stats, offset + 20),
            u32::from(ELM_PROVIDER_FLAG_KERNEL_BACKEND)
        );
        assert_eq!(read_u64(&stats, offset + 24), 1);
        assert_eq!(read_u64(&stats, offset + 32), 3);
    }
    assert!(found);
}

#[ktest]
fn elm_provider_async_core_completes_and_releases_lease_on_poll() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        100,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit =
        core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 1_000, 1_000), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    assert_eq!(submit.state, ElmProviderAsyncState::Queued as u32);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(unbind.blockers & ELM_POLICY_BLOCK_LEASE_BUSY, 0);

    assert!(core.run_one_async_provider_job_at(now.saturating_add(100_000)));
    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(200_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Completed as u32);
    assert_eq!(poll.status, ELM_MGR_STATUS_OK);
    assert_eq!(poll.reply.status, ELM_CALL_STATUS_OK);
    assert_eq!(
        poll.reply.payload_len as usize,
        core::mem::size_of::<elm_model::ElmActionInvokeReply>()
    );

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(unbind.allowed, 1);
}

#[ktest]
fn elm_provider_async_cancel_queued_job_releases_lease() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        101,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit = core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 0, 0), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);

    let cancel = core.cancel_provider_call(
        ElmProviderAsyncCancelRequest::new(submit.ticket_id),
        now.saturating_add(1),
    );
    assert_eq!(cancel.state, ElmProviderAsyncState::Canceled as u32);
    assert_eq!(cancel.status, ELM_MGR_STATUS_OK);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(unbind.allowed, 1);
}

#[ktest]
fn elm_provider_async_running_job_is_observable_and_cancelable() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        103,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit =
        core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 1_000, 1_000), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    assert!(
        core.move_provider_ticket_to_running_for_test(submit.ticket_id, now.saturating_add(1_000))
    );

    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(2_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Running as u32);
    assert_eq!(poll.status, ELM_MGR_STATUS_BUSY);
    assert_eq!(poll.reply.status, ELM_CALL_STATUS_BUSY);
    assert_eq!(poll.blockers, 0);

    let stats = provider_queue_stats_by_port(&mut core, 4, now.saturating_add(3_000)).unwrap();
    assert_eq!(stats.0, 0);
    assert_eq!(stats.1, 1);
    assert_eq!(stats.2, 0);
    assert_eq!(stats.5, 0);

    let cancel = core.cancel_provider_call(
        ElmProviderAsyncCancelRequest::new(submit.ticket_id),
        now.saturating_add(4_000),
    );
    assert_eq!(cancel.state, ElmProviderAsyncState::Running as u32);
    assert_eq!(cancel.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(cancel.blockers & ELM_POLICY_BLOCK_PROVIDER_BUSY, 0);

    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(5_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Running as u32);
    assert_ne!(poll.blockers & ELM_POLICY_BLOCK_PROVIDER_BUSY, 0);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(unbind.blockers & ELM_POLICY_BLOCK_LEASE_BUSY, 0);

    assert!(core.finish_provider_ticket_for_test(submit.ticket_id, now.saturating_add(6_000)));
    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(7_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Canceled as u32);
    assert_eq!(poll.status, ELM_MGR_STATUS_OK);
    assert_eq!(poll.reply.status, ELM_CALL_STATUS_BUSY);

    let stats = provider_queue_stats_by_port(&mut core, 4, now.saturating_add(8_000)).unwrap();
    assert_eq!(stats.1, 0);
    assert_eq!(stats.2, 0);
    assert_eq!(stats.5, 1);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(unbind.allowed, 1);
}

#[ktest]
fn elm_provider_async_running_timeout_is_retained_until_poll() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        104,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit =
        core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 1, 1_000), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    assert!(
        core.move_provider_ticket_to_running_for_test(
            submit.ticket_id,
            now.saturating_add(100_000)
        )
    );
    assert!(core.finish_provider_ticket_for_test(submit.ticket_id, now.saturating_add(2_000_000)));

    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(2_100_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Expired as u32);
    assert_eq!(poll.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(poll.blockers & ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, 0);

    let stats = provider_queue_stats_by_port(&mut core, 4, now.saturating_add(2_200_000)).unwrap();
    assert_eq!(stats.1, 0);
    assert_eq!(stats.2, 0);
    assert_eq!(stats.6, 1);
}

#[ktest]
fn elm_provider_async_queued_timeout_is_retained_until_poll() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        102,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit =
        core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 1, 1_000), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    assert_eq!(
        core.expire_provider_jobs_at(now.saturating_add(2_000_000)),
        1
    );

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(unbind.blockers & ELM_POLICY_BLOCK_LEASE_BUSY, 0);

    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(2_100_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Expired as u32);
    assert_eq!(poll.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(poll.blockers & ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, 0);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(unbind.allowed, 1);
}

#[ktest]
fn elm_provider_async_queue_full_rejects_without_holding_lease() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);
    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let now = sched::now_ns_public();

    for call_id in 0..ELM_PROVIDER_ASYNC_QUEUE_LIMIT {
        let frame = ElmCallFrame::new(
            bind_response.binding_id,
            200 + u64::from(call_id),
            ELM_ACTION_OPCODE_INVOKE,
            &payload,
        );
        let submit =
            core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 0, 0), now);
        assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    }

    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        999,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let submit = core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 0, 0), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(submit.blockers & ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, 0);
    assert_eq!(submit.queue_depth, ELM_PROVIDER_ASYNC_QUEUE_LIMIT);
}

#[ktest]
fn elm_native_ebi_unit_stays_loaded_until_native_loader_exists() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let response = core.load_ebi_unit(native_unit("elm-native-todo"), ElmEbiArch::Riscv64);
    assert_eq!(response.status, ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(response.final_state, state_code(ElmState::Loaded));

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Loaded);
    assert_eq!(cell.ebi_status, ElmEbiLoadStatus::NativeCodeTodo);
    assert!(cell.has_native_code);
    assert_eq!(cell.native_segment_count, 1);
    assert_eq!(cell.native_import_count, 0);
    assert_eq!(cell.native_export_count, 0);
}

#[ktest]
fn elm_native_ebi_unit_ignores_test_lifecycle_executor_until_loader_exists() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::default();

    let response = core.load_ebi_unit_with_lifecycle_executor(
        native_unit("elm-native-still-todo"),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(response.status, ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(response.final_state, state_code(ElmState::Loaded));
    assert_eq!(executor.initialize_calls, 0);

    let detach = core.detach_cell(ElmId(response.cell_id));
    assert_eq!(detach.status, ELM_MGR_STATUS_OK);
    assert_eq!(executor.finalize_calls, 0);
}

#[ktest]
fn elm_dynamic_provider_is_queryable_and_bind_preflight_is_todo() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let register = ElmProviderPortRegisterRequest::new(
        ELM_MGR_ID.0,
        "test.dynamic.provider@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let response = core.register_provider_port(register);
    assert_eq!(response.status, ELM_MGR_STATUS_OK);

    let provider_bytes = core.provider_ports_bytes();
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(&provider_bytes, 2) as usize;
    let record_count = read_u32(&provider_bytes, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(&provider_bytes, offset) != response.port_id {
            continue;
        }
        found = true;
        assert_eq!(read_u64(&provider_bytes, offset + 8), ELM_MGR_ID.0);
        assert_eq!(read_u32(&provider_bytes, offset + 28), 0);
        assert_eq!(read_u32(&provider_bytes, offset + 32), 0);
        assert_eq!(
            read_u16(&provider_bytes, offset + 42),
            ELM_PROVIDER_FLAG_DYNAMIC | ELM_PROVIDER_FLAG_TODO_BACKEND
        );
        assert_eq!(
            read_u16(&provider_bytes, offset + 42) & ELM_PROVIDER_FLAG_NATIVE_BACKEND,
            0
        );
    }
    assert!(found);

    let mut bind =
        ElmNexusBindRequest::new(ELM_MGR_ID.0, response.port_id, "test.dynamic.provider@1");
    assert_eq!(bind.contract.len(), ELM_NEXUS_CONTRACT_LEN);
    bind.flags = 0;
    let plan = core.preflight_bind(bind);
    assert_eq!(plan.allowed, 0);
    assert_eq!(plan.status, ELM_MGR_STATUS_TODO);
    assert_ne!(plan.blockers & ELM_POLICY_BLOCK_PORT_TODO, 0);
}

#[ktest]
fn elm_dynamic_provider_registration_respects_cell_resource_budget() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::default();
    let response = core.load_ebi_unit_with_lifecycle_executor(
        menu_unit("elm-quota-provider"),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    let owner = ElmId(response.cell_id);
    let limit = core
        .cells()
        .iter()
        .find(|cell| cell.id == owner)
        .unwrap()
        .resource_budget
        .max_provider_ports;

    for index in 0..limit {
        let contract = format!("quota.provider.{}@1", index);
        let register = ElmProviderPortRegisterRequest::new(
            owner.0,
            &contract,
            ElmPortAccessPolicy::Internal,
            FlowDirection::Control,
            FlowMode::Shared,
            0,
        );
        let response = core.register_provider_port(register);
        assert_eq!(response.status, ELM_MGR_STATUS_OK);
    }

    let register = ElmProviderPortRegisterRequest::new(
        owner.0,
        "quota.provider.overflow@1",
        ElmPortAccessPolicy::Internal,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let response = core.register_provider_port(register);
    assert_eq!(response.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(response.blockers & ELM_POLICY_BLOCK_RESOURCE_QUOTA, 0);
}

#[ktest]
fn elm_subsystem_provider_specs_are_registered_and_invokable() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    assert_eq!(
        super::subsystems::register_builtin_provider_specs(&mut core).unwrap(),
        11
    );
    assert_eq!(
        super::subsystems::register_builtin_provider_specs(&mut core).unwrap(),
        0
    );

    let port_id = provider_port_id_by_contract(&mut core, "vfs.lookup@1").unwrap();
    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_ne!(stats.0 & u32::from(ELM_PROVIDER_FLAG_KERNEL_BACKEND), 0);

    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, port_id, "vfs.lookup@1");
    let plan = core.preflight_bind(bind);
    assert_eq!(plan.allowed, 1);
    assert_eq!(plan.status, ELM_MGR_STATUS_OK);

    let bind_response = core.commit_bind(bind);
    assert_eq!(bind_response.allowed, 1);
    assert_eq!(bind_response.status, ELM_MGR_STATUS_OK);

    let payload = vfs_lookup_payload("/");
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        77,
        vfs::elm::ELM_VFS_LOOKUP_OPCODE_QUERY,
        &payload,
    );
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_NOT_FOUND);
    assert_eq!(
        response.reply.payload_len as usize,
        vfs::elm::ELM_VFS_LOOKUP_REPLY_FIXED_LEN
    );
    assert_eq!(
        read_i32(&response.reply.payload, 4),
        errno::Errno::EBADF.as_i32()
    );

    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_eq!(stats.1, 0);
    assert_eq!(stats.2, 1);
    assert_eq!(stats.3, 0);

    let malformed = core
        .invoke_provider(ElmProviderInvokeRequest::new(ElmCallFrame::empty(
            bind_response.binding_id,
            78,
            vfs::elm::ELM_VFS_LOOKUP_OPCODE_QUERY,
        )))
        .unwrap();
    assert_eq!(malformed.reply.status, ELM_CALL_STATUS_INVALID);
    assert_eq!(
        malformed.reply.payload_len as usize,
        vfs::elm::ELM_VFS_LOOKUP_REPLY_FIXED_LEN
    );
    assert_eq!(
        read_i32(&malformed.reply.payload, 4),
        errno::Errno::EINVAL.as_i32()
    );

    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_eq!(stats.1, 0);
    assert_eq!(stats.2, 2);
    assert_eq!(stats.3, 0);

    let unbind = core.commit_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_eq!(stats.3, 1);
}

#[ktest]
fn elm_device_discovery_provider_returns_real_snapshot_and_query_reply() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    super::subsystems::register_builtin_provider_specs(&mut core).unwrap();

    let port_id = provider_port_id_by_contract(&mut core, "device.discovered@1").unwrap();
    let bytes = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_port(port_id))
        .unwrap();
    assert_eq!(read_i32(&bytes, 4), ELM_MGR_STATUS_OK);
    assert_eq!(read_u64(&bytes, 8), port_id);

    let outer_header = core::mem::size_of::<ElmProviderSnapshotHeader>();
    let payload_len = read_u32(&bytes, 24) as usize;
    assert_eq!(bytes.len(), outer_header + payload_len);
    assert!(payload_len >= core::mem::size_of::<general::dev::elm::ElmDeviceDiscoveryHeader>());

    let payload = &bytes[outer_header..];
    let record_size = read_u16(payload, 2) as usize;
    let record_count = read_u32(payload, 4) as usize;
    let total_count = read_u32(payload, 8) as usize;
    assert_eq!(
        record_size,
        core::mem::size_of::<general::dev::elm::ElmDeviceDiscoveryRecord>()
    );
    assert!(record_count <= total_count);
    assert_eq!(
        payload_len,
        core::mem::size_of::<general::dev::elm::ElmDeviceDiscoveryHeader>()
            + record_count * record_size
    );
    for index in 0..record_count {
        let offset = core::mem::size_of::<general::dev::elm::ElmDeviceDiscoveryHeader>()
            + index * record_size;
        assert_eq!(read_u64(payload, offset), index as u64 + 1);
        assert!(
            read_u16(payload, offset + 8) as usize
                <= general::dev::elm::ELM_DEV_DISCOVERY_CLASS_LEN
        );
        assert!(
            read_u16(payload, offset + 10) as usize
                <= general::dev::elm::ELM_DEV_DISCOVERY_NAME_LEN
        );
    }

    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, port_id, "device.discovered@1");
    let bind_response = core.commit_bind(bind);
    assert_eq!(bind_response.status, ELM_MGR_STATUS_OK);
    assert_eq!(bind_response.allowed, 1);

    let frame = ElmCallFrame::empty(
        bind_response.binding_id,
        88,
        general::dev::elm::ELM_DEV_DISCOVERY_OPCODE_QUERY,
    );
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_OK);
    assert!(
        response.reply.payload_len as usize
            >= core::mem::size_of::<general::dev::elm::ElmDeviceDiscoveryHeader>()
    );
    assert_eq!(
        read_u16(&response.reply.payload, 2) as usize,
        core::mem::size_of::<general::dev::elm::ElmDeviceDiscoveryRecord>()
    );

    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_eq!(stats.1, 2);
    assert_eq!(stats.2, 0);
}

#[ktest]
fn elm_device_claim_provider_acquires_releases_and_revokes_claims() {
    let function: Arc<dyn general::dev::function::DeviceFunction> = Arc::new(TestElmDeviceFunction);
    general::dev::enumerate::DEVICES.unregister_function(&function);
    general::dev::enumerate::DEVICES
        .register_function(Arc::clone(&function))
        .unwrap();

    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    super::subsystems::register_builtin_provider_specs(&mut core).unwrap();

    let port_id = provider_port_id_by_contract(&mut core, "device.claim@1").unwrap();
    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, port_id, "device.claim@1");
    let bind_response = core.commit_bind(bind);
    assert_eq!(bind_response.status, ELM_MGR_STATUS_OK);
    assert_eq!(bind_response.allowed, 1);

    let payload = device_claim_payload("elmtest", "elm-claim-test");
    let acquire = core
        .invoke_provider(ElmProviderInvokeRequest::new(ElmCallFrame::new(
            bind_response.binding_id,
            1,
            general::dev::elm::ELM_DEV_CLAIM_OPCODE_ACQUIRE,
            &payload,
        )))
        .unwrap();
    assert_eq!(acquire.reply.status, ELM_CALL_STATUS_OK);
    assert_eq!(
        acquire.reply.payload_len as usize,
        core::mem::size_of::<general::dev::elm::ElmDeviceClaimReply>()
    );
    assert_eq!(
        read_u64(&acquire.reply.payload, 8),
        bind_response.binding_id
    );
    assert_eq!(
        read_u16(&acquire.reply.payload, 24) as usize,
        "elmtest".len()
    );
    assert_eq!(
        read_u16(&acquire.reply.payload, 26) as usize,
        "elm-claim-test".len()
    );

    let duplicate = core
        .invoke_provider(ElmProviderInvokeRequest::new(ElmCallFrame::new(
            bind_response.binding_id,
            2,
            general::dev::elm::ELM_DEV_CLAIM_OPCODE_ACQUIRE,
            &payload,
        )))
        .unwrap();
    assert_eq!(duplicate.reply.status, ELM_CALL_STATUS_OK);

    let query = core
        .invoke_provider(ElmProviderInvokeRequest::new(ElmCallFrame::new(
            bind_response.binding_id,
            3,
            general::dev::elm::ELM_DEV_CLAIM_OPCODE_QUERY,
            &payload,
        )))
        .unwrap();
    assert_eq!(query.reply.status, ELM_CALL_STATUS_OK);

    let bytes = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_port(port_id))
        .unwrap();
    assert_eq!(read_i32(&bytes, 4), ELM_MGR_STATUS_OK);
    let outer_header = core::mem::size_of::<ElmProviderSnapshotHeader>();
    let snapshot_payload = &bytes[outer_header..];
    assert_eq!(
        read_u16(snapshot_payload, 2) as usize,
        core::mem::size_of::<general::dev::elm::ElmDeviceClaimRecord>()
    );
    assert!(read_u32(snapshot_payload, 4) >= 1);
    assert!(device_claim_snapshot_has_binding(
        snapshot_payload,
        bind_response.binding_id
    ));

    let release = core
        .invoke_provider(ElmProviderInvokeRequest::new(ElmCallFrame::new(
            bind_response.binding_id,
            4,
            general::dev::elm::ELM_DEV_CLAIM_OPCODE_RELEASE,
            &payload,
        )))
        .unwrap();
    assert_eq!(release.reply.status, ELM_CALL_STATUS_OK);

    let query_after_release = core
        .invoke_provider(ElmProviderInvokeRequest::new(ElmCallFrame::new(
            bind_response.binding_id,
            5,
            general::dev::elm::ELM_DEV_CLAIM_OPCODE_QUERY,
            &payload,
        )))
        .unwrap();
    assert_eq!(query_after_release.reply.status, ELM_CALL_STATUS_NOT_FOUND);

    let reacquire = core
        .invoke_provider(ElmProviderInvokeRequest::new(ElmCallFrame::new(
            bind_response.binding_id,
            6,
            general::dev::elm::ELM_DEV_CLAIM_OPCODE_ACQUIRE,
            &payload,
        )))
        .unwrap();
    assert_eq!(reacquire.reply.status, ELM_CALL_STATUS_OK);

    let unbind = core.commit_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    let bytes = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_port(port_id))
        .unwrap();
    let snapshot_payload = &bytes[core::mem::size_of::<ElmProviderSnapshotHeader>()..];
    assert!(!device_claim_snapshot_has_binding(
        snapshot_payload,
        bind_response.binding_id
    ));

    general::dev::enumerate::DEVICES.unregister_function(&function);
}

#[ktest]
fn elm_kernel_provider_spec_revoke_callback_runs_on_unbind() {
    TEST_PROVIDER_REVOKES.store(0, Ordering::Relaxed);
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    assert_eq!(
        core.register_kernel_provider_specs(&TEST_REVOKE_PROVIDERS)
            .unwrap(),
        1
    );

    let port_id = provider_port_id_by_contract(&mut core, "test.revoke@1").unwrap();
    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, port_id, "test.revoke@1");
    let bind_response = core.commit_bind(bind);
    assert_eq!(bind_response.status, ELM_MGR_STATUS_OK);

    let unbind = core.commit_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(TEST_PROVIDER_REVOKES.load(Ordering::Relaxed), 1);
}

#[ktest]
fn elm_kernel_provider_spec_snapshot_callback_is_routed() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    assert_eq!(
        core.register_kernel_provider_specs(&TEST_SNAPSHOT_PROVIDERS)
            .unwrap(),
        1
    );

    let port_id = provider_port_id_by_contract(&mut core, "test.snapshot@1").unwrap();
    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, port_id, "test.snapshot@1");
    let bind_response = core.commit_bind(bind);
    assert_eq!(bind_response.status, ELM_MGR_STATUS_OK);

    let bytes = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_binding(
            bind_response.binding_id,
        ))
        .unwrap();
    assert_eq!(read_i32(&bytes, 4), ELM_MGR_STATUS_OK);
    assert_eq!(read_u64(&bytes, 8), port_id);
    assert_eq!(read_u64(&bytes, 16), bind_response.binding_id);
    assert_eq!(read_u32(&bytes, 24), "snapshot-ok".len() as u32);
    assert_eq!(
        &bytes[core::mem::size_of::<ElmProviderSnapshotHeader>()..],
        b"snapshot-ok"
    );

    let bad = ElmProviderSnapshotRequest {
        port_id: port_id + 1,
        binding_id: bind_response.binding_id,
        flags: 0,
        reserved: 0,
    };
    assert_eq!(
        core.provider_snapshot_bytes(bad).unwrap_err(),
        ELM_MGR_STATUS_INVALID
    );
    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_eq!(stats.1, 1);
    assert_eq!(stats.2, 0);

    let paged = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_binding_paged(
            bind_response.binding_id,
            0,
        ))
        .unwrap();
    assert_eq!(read_i32(&paged, 4), ELM_MGR_STATUS_UNSUPPORTED);
    assert_eq!(read_u64(&paged, 8), port_id);
    assert_eq!(read_u32(&paged, 24), 0);
}

#[ktest]
fn elm_kernel_provider_spec_paged_snapshot_callback_is_routed() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    assert_eq!(
        core.register_kernel_provider_specs(&TEST_PAGED_SNAPSHOT_PROVIDERS)
            .unwrap(),
        1
    );

    let port_id = provider_port_id_by_contract(&mut core, "test.snapshot.paged@1").unwrap();
    let first = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_port_paged(port_id, 0))
        .unwrap();
    assert_eq!(read_i32(&first, 4), ELM_MGR_STATUS_OK);
    assert_eq!(read_u64(&first, 8), port_id);
    assert_eq!(read_u32(&first, 24), "page-a".len() as u32);
    assert_eq!(read_u32(&first, 28), 1);
    assert_eq!(
        read_u32(&first, 32) & ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE,
        ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE
    );
    assert_eq!(read_u32(&first, 36), 1);
    assert_eq!(
        &first[core::mem::size_of::<ElmProviderSnapshotHeader>()..],
        b"page-a"
    );

    let second = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_port_paged(port_id, 1))
        .unwrap();
    assert_eq!(read_i32(&second, 4), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(&second, 24), "page-b".len() as u32);
    assert_eq!(read_u32(&second, 28), 1);
    assert_eq!(read_u32(&second, 32), 0);
    assert_eq!(read_u32(&second, 36), 0);
    assert_eq!(
        &second[core::mem::size_of::<ElmProviderSnapshotHeader>()..],
        b"page-b"
    );

    let bad_cursor = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_port_paged(port_id, 99))
        .unwrap();
    assert_eq!(read_i32(&bad_cursor, 4), ELM_MGR_STATUS_NOT_FOUND);

    let bad = ElmProviderSnapshotRequest {
        port_id,
        binding_id: 0,
        flags: ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED << 1,
        reserved: 0,
    };
    assert_eq!(
        core.provider_snapshot_bytes(bad).unwrap_err(),
        ELM_MGR_STATUS_INVALID
    );

    let request = provider_snapshot_payload(&ElmProviderSnapshotRequest::by_port_paged(port_id, 0));
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryProviderSnapshot, &request),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let payload = response_payload(&response);
    assert_eq!(read_i32(payload, 4), ELM_MGR_STATUS_OK);
    assert_ne!(
        read_u32(payload, 32) & ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE,
        0
    );
}

#[ktest]
fn elm_provider_snapshot_without_callback_returns_provider_status() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    super::subsystems::register_builtin_provider_specs(&mut core).unwrap();

    let port_id = provider_port_id_by_contract(&mut core, "vfs.lookup@1").unwrap();
    let bytes = core
        .provider_snapshot_bytes(ElmProviderSnapshotRequest::by_port(port_id))
        .unwrap();
    assert_eq!(
        bytes.len(),
        core::mem::size_of::<ElmProviderSnapshotHeader>()
    );
    assert_eq!(read_i32(&bytes, 4), ELM_MGR_STATUS_UNSUPPORTED);
    assert_eq!(read_u64(&bytes, 8), port_id);
    assert_eq!(read_u32(&bytes, 24), 0);

    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_eq!(stats.1, 0);
    assert_eq!(stats.2, 1);
}

#[ktest]
fn elm_mgr_channel_dispatches_core_queries_and_provider_flow() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let policy = dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryPolicy, &[]));
    assert_eq!(response_status(&policy), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&policy),
        core::mem::size_of::<ElmMgrPolicyInfo>()
    );

    let health = dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryHealth, &[]));
    assert_eq!(response_status(&health), ELM_MGR_STATUS_OK);
    assert!(response_payload_len(&health) >= core::mem::size_of::<ElmCoreHealthHeader>());
    let health_payload = response_payload(&health);
    assert_eq!(read_i32(health_payload, 8), ELM_MGR_STATUS_OK);

    let action = core
        .menu_items()
        .iter()
        .find(|item| item.route == "elm/mgr/health")
        .unwrap()
        .action;
    let action_bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, 4, "mgr.action.invoke@1");
    let action_bind_payload = nexus_bind_payload(&action_bind);
    let action_bind_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::CommitBind, &action_bind_payload),
    );
    assert_eq!(response_status(&action_bind_response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&action_bind_response),
        core::mem::size_of::<ElmNexusBindPlanResponse>()
    );
    let action_bind_response_payload = response_payload(&action_bind_response);
    assert_eq!(
        read_i32(action_bind_response_payload, 40),
        ELM_MGR_STATUS_OK
    );
    assert_eq!(read_u32(action_bind_response_payload, 44), 1);
    let action_binding = read_u64(action_bind_response_payload, 16);

    let action_payload = action_invoke_payload(&ElmActionInvokeRequest::new(action.0));
    let frame = ElmCallFrame::new(
        action_binding,
        10,
        ELM_ACTION_OPCODE_INVOKE,
        &action_payload,
    );
    let invoke_payload = provider_invoke_payload(&ElmProviderInvokeRequest::new(frame));
    let invoke_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::InvokeProvider, &invoke_payload),
    );
    assert_eq!(response_status(&invoke_response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&invoke_response),
        core::mem::size_of::<ElmProviderInvokeResponse>()
    );
    let invoke_response_payload = response_payload(&invoke_response);
    assert_eq!(read_i32(invoke_response_payload, 16), ELM_CALL_STATUS_OK);
    assert_eq!(
        read_u16(invoke_response_payload, 24) as usize,
        core::mem::size_of::<elm_model::ElmActionInvokeReply>()
    );
    assert_eq!(
        read_u32(invoke_response_payload, 32 + 24),
        ELM_ACTION_RESULT_HEALTH
    );
    assert_eq!(
        read_i32(invoke_response_payload, 32 + 28),
        ELM_MGR_STATUS_OK
    );

    let missing_action_payload = action_invoke_payload(&ElmActionInvokeRequest::new(999_999));
    let frame = ElmCallFrame::new(
        action_binding,
        11,
        ELM_ACTION_OPCODE_INVOKE,
        &missing_action_payload,
    );
    let invoke_payload = provider_invoke_payload(&ElmProviderInvokeRequest::new(frame));
    let invoke_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::InvokeProvider, &invoke_payload),
    );
    assert_eq!(response_status(&invoke_response), ELM_MGR_STATUS_OK);
    let invoke_response_payload = response_payload(&invoke_response);
    assert_eq!(
        read_i32(invoke_response_payload, 16),
        ELM_CALL_STATUS_NOT_FOUND
    );

    let audits = core.audit_bytes();
    let audit_header_size = core::mem::size_of::<ElmMgrAuditHeader>();
    let audit_record_size = read_u16(&audits, 2) as usize;
    let audit_record_count = read_u32(&audits, 4) as usize;
    let last_audit = audit_header_size + (audit_record_count - 1) * audit_record_size;
    assert_eq!(
        read_u32(&audits, last_audit + 8),
        ELM_MGR_ACTION_PROVIDER_INVOKE
    );
    assert_eq!(read_i32(&audits, last_audit + 12), ELM_MGR_STATUS_INVALID);
    assert_ne!(
        read_u64(&audits, last_audit + 24) & ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
        0
    );

    let register = ElmProviderPortRegisterRequest::new(
        ELM_MGR_ID.0,
        "test.channel.provider@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let register_payload = provider_register_payload(&register);
    let register_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&register_response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&register_response),
        core::mem::size_of::<ElmProviderPortRegisterResponse>()
    );
    let register_response_payload = response_payload(&register_response);
    assert_eq!(read_i32(register_response_payload, 16), ELM_MGR_STATUS_OK);
    let port_id = read_u64(register_response_payload, 8);

    let providers = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryProviderPorts, &[]),
    );
    assert_eq!(response_status(&providers), ELM_MGR_STATUS_OK);
    let provider_payload = response_payload(&providers);
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(provider_payload, 2) as usize;
    let record_count = read_u32(provider_payload, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(provider_payload, offset) != port_id {
            continue;
        }
        found = true;
        assert_eq!(read_u64(provider_payload, offset + 8), ELM_MGR_ID.0);
        assert_eq!(
            read_u16(provider_payload, offset + 42),
            ELM_PROVIDER_FLAG_DYNAMIC | ELM_PROVIDER_FLAG_TODO_BACKEND
        );
    }
    assert!(found);

    let snapshot_payload = provider_snapshot_payload(&ElmProviderSnapshotRequest::by_port(port_id));
    let snapshot_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryProviderSnapshot, &snapshot_payload),
    );
    assert_eq!(response_status(&snapshot_response), ELM_MGR_STATUS_OK);
    let snapshot_response_payload = response_payload(&snapshot_response);
    assert_eq!(
        snapshot_response_payload.len(),
        core::mem::size_of::<ElmProviderSnapshotHeader>()
    );
    assert_eq!(read_i32(snapshot_response_payload, 4), ELM_MGR_STATUS_TODO);
    assert_eq!(read_u64(snapshot_response_payload, 8), port_id);
    assert_eq!(read_u32(snapshot_response_payload, 24), 0);

    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, port_id, "test.channel.provider@1");
    let bind_payload = nexus_bind_payload(&bind);
    let plan = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::PreflightBind, &bind_payload),
    );
    assert_eq!(response_status(&plan), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&plan),
        core::mem::size_of::<ElmNexusBindPlanResponse>()
    );
    let plan_payload = response_payload(&plan);
    assert_eq!(read_i32(plan_payload, 40), ELM_MGR_STATUS_TODO);
    assert_eq!(read_u32(plan_payload, 44), 0);
    assert_ne!(read_u64(plan_payload, 48) & ELM_POLICY_BLOCK_PORT_TODO, 0);
}

#[ktest]
fn elm_mgr_channel_dispatches_async_provider_queue_flow() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let action_payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        700,
        ELM_ACTION_OPCODE_INVOKE,
        &action_payload,
    );
    let submit_payload =
        provider_async_submit_payload(&ElmProviderAsyncSubmitRequest::new(frame, 1_000, 1_000));
    let submit_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::SubmitProviderCall, &submit_payload),
    );
    assert_eq!(response_status(&submit_response), ELM_MGR_STATUS_OK);
    let submit_response_payload = response_payload(&submit_response);
    assert_eq!(read_i32(submit_response_payload, 24), ELM_MGR_STATUS_OK);
    assert_eq!(
        read_u32(submit_response_payload, 28),
        ElmProviderAsyncState::Queued as u32
    );
    let ticket_id = read_u64(submit_response_payload, 0);
    assert_ne!(ticket_id, 0);

    let queue_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryProviderQueue, &[]),
    );
    assert_eq!(response_status(&queue_response), ELM_MGR_STATUS_OK);
    let queue_payload = response_payload(&queue_response);
    assert!(queue_payload.len() >= core::mem::size_of::<ElmProviderQueueStatsHeader>());
    let header_size = core::mem::size_of::<ElmProviderQueueStatsHeader>();
    let record_size = read_u16(queue_payload, 2) as usize;
    let record_count = read_u32(queue_payload, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(queue_payload, offset) != 4 {
            continue;
        }
        found = true;
        assert_eq!(read_u32(queue_payload, offset + 8), 1);
        assert_eq!(read_u32(queue_payload, offset + 16), 0);
    }
    assert!(found);

    assert!(core.run_one_async_provider_job_at(sched::now_ns_public().saturating_add(100_000)));
    let poll_payload = provider_async_poll_payload(&ElmProviderAsyncPollRequest::new(ticket_id));
    let poll_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::PollProviderReply, &poll_payload),
    );
    assert_eq!(response_status(&poll_response), ELM_MGR_STATUS_OK);
    let poll_response_payload = response_payload(&poll_response);
    assert_eq!(
        read_u32(poll_response_payload, 8),
        ElmProviderAsyncState::Completed as u32
    );
    assert_eq!(read_i32(poll_response_payload, 12), ELM_MGR_STATUS_OK);
    assert_eq!(read_i32(poll_response_payload, 32), ELM_CALL_STATUS_OK);

    let audits = core.audit_bytes();
    let audit_header_size = core::mem::size_of::<ElmMgrAuditHeader>();
    let audit_record_size = read_u16(&audits, 2) as usize;
    let audit_record_count = read_u32(&audits, 4) as usize;
    let last_audit = audit_header_size + (audit_record_count - 1) * audit_record_size;
    assert_eq!(
        read_u32(&audits, last_audit + 8),
        ELM_MGR_ACTION_PROVIDER_ASYNC
    );
}

#[ktest]
fn elm_mgr_channel_exposes_mgr_runtime_api_and_event_subscriptions() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    super::subsystems::register_builtin_provider_specs(&mut core).unwrap();

    let api =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryApiRegistry, &[]));
    assert_eq!(response_status(&api), ELM_MGR_STATUS_OK);
    let api_payload = response_payload(&api);
    assert!(api_payload.len() >= core::mem::size_of::<ElmMgrApiRegistryHeader>());
    let api_record_size = read_u16(api_payload, 2) as usize;
    let api_record_count = read_u32(api_payload, 4) as usize;
    assert!(api_record_count >= 21);
    assert_eq!(
        api_payload.len(),
        core::mem::size_of::<ElmMgrApiRegistryHeader>() + api_record_count * api_record_size
    );

    let native_caps = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryNativeCapabilities, &[]),
    );
    assert_eq!(response_status(&native_caps), ELM_MGR_STATUS_OK);
    let native_caps_payload = response_payload(&native_caps);
    assert_eq!(
        native_caps_payload.len(),
        core::mem::size_of::<ElmNativeCapabilityHeader>()
    );
    assert_eq!(read_u32(native_caps_payload, 4), 0);
    let native_caps_bad = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryNativeCapabilities, &[1]),
    );
    assert_eq!(response_status(&native_caps_bad), ELM_MGR_STATUS_INVALID);

    let subscribe_payload =
        event_subscribe_payload(&ElmMgrEventSubscribeRequest::new(ELM_MGR_ID.0));
    let subscribe = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::SubscribeEvent, &subscribe_payload),
    );
    assert_eq!(response_status(&subscribe), ELM_MGR_STATUS_OK);
    let subscribe_payload = response_payload(&subscribe);
    let subscription_id = read_u64(subscribe_payload, 0);
    let original_cursor = read_u64(subscribe_payload, 24);
    assert_ne!(subscription_id, 0);
    assert_ne!(read_u64(subscribe_payload, 8), 0);
    assert_eq!(read_u64(subscribe_payload, 16), ELM_MGR_ID.0);
    assert_eq!(read_i32(subscribe_payload, 32), ELM_MGR_STATUS_OK);

    let subscriptions = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryEventSubscriptions, &[]),
    );
    assert_eq!(response_status(&subscriptions), ELM_MGR_STATUS_OK);
    let subscriptions_payload = response_payload(&subscriptions);
    assert_eq!(
        read_u16(subscriptions_payload, 2) as usize,
        core::mem::size_of::<elm_model::ElmMgrEventSubscriptionRecord>()
    );
    assert_eq!(read_u32(subscriptions_payload, 4), 1);

    let action_bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, 4, "mgr.action.invoke@1");
    let action_bind_payload = nexus_bind_payload(&action_bind);
    let action_bind_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::CommitBind, &action_bind_payload),
    );
    assert_eq!(response_status(&action_bind_response), ELM_MGR_STATUS_OK);
    assert_eq!(
        read_i32(response_payload(&action_bind_response), 40),
        ELM_MGR_STATUS_OK
    );

    let mut peek_request = ElmMgrSubscribedEventReadRequest::new(subscription_id, 0, 1);
    peek_request.flags = 0;
    let peek_payload = subscribed_event_read_payload(&peek_request);
    let peek = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::ReadSubscribedEvents, &peek_payload),
    );
    assert_eq!(response_status(&peek), ELM_MGR_STATUS_OK);
    let peek_payload = response_payload(&peek);
    assert_eq!(read_i32(peek_payload, 8), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(peek_payload, 12), 0);
    assert_eq!(read_u32(peek_payload, 4), 1);
    assert!(read_u64(peek_payload, 32) > read_u64(peek_payload, 24));

    let subscriptions = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryEventSubscriptions, &[]),
    );
    let subscriptions_payload = response_payload(&subscriptions);
    let subscription_record_offset =
        core::mem::size_of::<elm_model::ElmMgrEventSubscriptionHeader>();
    assert_eq!(
        read_u64(subscriptions_payload, subscription_record_offset + 24),
        original_cursor
    );

    let read_payload = subscribed_event_read_payload(&ElmMgrSubscribedEventReadRequest::new(
        subscription_id,
        0,
        8,
    ));
    let read = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::ReadSubscribedEvents, &read_payload),
    );
    assert_eq!(response_status(&read), ELM_MGR_STATUS_OK);
    let read_payload = response_payload(&read);
    assert!(read_payload.len() >= core::mem::size_of::<ElmMgrSubscribedEventReadHeader>());
    assert_eq!(read_i32(read_payload, 8), ELM_MGR_STATUS_OK);
    assert!(read_u32(read_payload, 4) >= 2);
    assert!(read_u64(read_payload, 32) > read_u64(read_payload, 24));

    let unsubscribe_payload = event_unsubscribe_payload(&ElmMgrEventUnsubscribeRequest::new(
        subscription_id,
        ELM_MGR_ID.0,
    ));
    let unsubscribe = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::UnsubscribeEvent, &unsubscribe_payload),
    );
    assert_eq!(response_status(&unsubscribe), ELM_MGR_STATUS_OK);
    let unsubscribe_payload = response_payload(&unsubscribe);
    assert_eq!(read_i32(unsubscribe_payload, 24), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(unsubscribe_payload, 28), 1);

    let subscriptions = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryEventSubscriptions, &[]),
    );
    assert_eq!(response_status(&subscriptions), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(response_payload(&subscriptions), 4), 0);
}

#[ktest]
fn elm_mgr_channel_rejects_malformed_byte_requests() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let truncated = [0u8; 3];
    let response = dispatch_mgr_call_on_core(&mut core, &truncated);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let header_flags = raw_mgr_call(ElmMgrCallKind::QueryPolicy as u32, 1, 0, &[]);
    let response = dispatch_mgr_call_on_core(&mut core, &header_flags);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let header_reserved = raw_mgr_call(ElmMgrCallKind::QueryPolicy as u32, 0, 1, &[]);
    let response = dispatch_mgr_call_on_core(&mut core, &header_reserved);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let unsupported = raw_mgr_call(0xffff, 0, 0, &[]);
    let response = dispatch_mgr_call_on_core(&mut core, &unsupported);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_UNSUPPORTED);

    let non_empty_query_payload = [1u8];
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryPolicy, &non_empty_query_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let register = ElmProviderPortRegisterRequest::new(
        ELM_MGR_ID.0,
        "test.invalid.provider@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let mut register_payload = provider_register_payload(&register);
    register_payload[26] = 1;
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let mut register_payload = provider_register_payload(&register);
    register_payload[28] = 1;
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let mut register_payload = provider_register_payload(&register);
    register_payload[24..26].copy_from_slice(&((ELM_NEXUS_CONTRACT_LEN as u16) + 1).to_le_bytes());
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let cancel_payload = provider_async_cancel_payload(&ElmProviderAsyncCancelRequest::new(1));
    let mut malformed_cancel = cancel_payload;
    malformed_cancel[8] = 1;
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::CancelProviderCall, &malformed_cancel),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);
}
