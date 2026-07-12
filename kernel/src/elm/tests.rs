use ktest::ktest;

use core::any::Any;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ed25519_dalek::{Signer, SigningKey};

use elm_model::{
    ELM_ACTION_OPCODE_INVOKE, ELM_ACTION_RESULT_HEALTH, ELM_API_CURRENT_VERSION,
    ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER, ELM_AUDIT_AUTHORITY_MANAGER,
    ELM_AUDIT_FLAG_AUTHORIZATION, ELM_AUDIT_FLAG_OPERATION, ELM_CALL_STATUS_BUSY,
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_NOT_FOUND, ELM_CALL_STATUS_OK,
    ELM_CALL_STATUS_UNSUPPORTED, ELM_CELL_POLICY_ALLOW_MANAGEMENT, ELM_CELL_POLICY_FLAG_LOCKED,
    ELM_CTL_ABI_VERSION, ELM_EBI_HOOK_ON_FINALIZE, ELM_EBI_HOOK_ON_INITIALIZE, ELM_EBI_NAME_LEN,
    ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT, ELM_EBI_SYMBOL_NAME_LEN, ELM_EKI_BLOCK_DESC_SIZE,
    ELM_EKI_FORMAT_VERSION, ELM_EKI_HEADER_SIZE, ELM_EKI_MAGIC, ELM_EKI_MANIFEST_NAME_LEN,
    ELM_EKI_MANIFEST_VERSION_LEN, ELM_EKI_PROVIDER_PORT_RECORD_SIZE,
    ELM_EKI_SYMBOL_LOCATION_RECORD_SIZE, ELM_EXTENSION_POLICY_MIXIN_PATCH, ELM_HEALTH_CHECK_AUDITS,
    ELM_HEALTH_CHECK_BINDINGS, ELM_HEALTH_CHECK_CELLS, ELM_HEALTH_CHECK_EVENTS,
    ELM_HEALTH_CHECK_EXECUTIONS, ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_JOURNAL,
    ELM_HEALTH_CHECK_MENU, ELM_HEALTH_CHECK_NATIVE_CAPABILITIES, ELM_HEALTH_CHECK_PORTS,
    ELM_HEALTH_CHECK_PROJECTION_SOURCES, ELM_HEALTH_CHECK_PROVIDERS, ELM_HEALTH_CHECK_RESOURCES,
    ELM_HEALTH_CHECK_RUNTIME_PORTS, ELM_HEALTH_CHECK_SEQUENCES, ELM_HEALTH_CHECK_TODO_REGISTRY,
    ELM_HEALTH_CHECK_TRUST, ELM_KERNEL_PROVIDER_FLAG_NONE, ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED,
    ELM_LIFECYCLE_REASON_HOOK_FAILED, ELM_MENU_FLAG_TODO, ELM_MGR_ACTION_PROVIDER_ASYNC,
    ELM_MGR_ACTION_PROVIDER_INVOKE, ELM_MGR_API_KIND_SUBSYSTEM, ELM_MGR_RELATION_POINT_LEN,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_PERMISSION, ELM_MGR_STATUS_TODO, ELM_MGR_STATUS_UNSUPPORTED,
    ELM_MIXIN_REPLY_REPLACE, ELM_MIXIN_REPLY_STOP, ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED,
    ELM_NATIVE_POLICY_MIXIN_PATCH, ELM_NEXUS_CONTRACT_LEN, ELM_POLICY_BLOCK_BUILTIN_PROTECTED,
    ELM_POLICY_BLOCK_CALLER_STALE, ELM_POLICY_BLOCK_CAPABILITY_DENIED,
    ELM_POLICY_BLOCK_EXTENSION_DUPLICATE, ELM_POLICY_BLOCK_LEASE_BUSY,
    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED, ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
    ELM_POLICY_BLOCK_POLICY_ESCALATION, ELM_POLICY_BLOCK_PORT_TODO, ELM_POLICY_BLOCK_PROVIDER_BUSY,
    ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, ELM_POLICY_BLOCK_RESOURCE_QUOTA,
    ELM_POLICY_BLOCK_SCOPE_DENIED, ELM_PROVIDER_ASYNC_QUEUE_LIMIT, ELM_PROVIDER_FLAG_DYNAMIC,
    ELM_PROVIDER_FLAG_KERNEL_BACKEND, ELM_PROVIDER_FLAG_NATIVE_BACKEND,
    ELM_PROVIDER_FLAG_TODO_BACKEND, ELM_PROVIDER_POLICY_ALL,
    ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED, ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE,
    ELM_TRUST_FLAG_ALLOW_UNSIGNED, ELM_TRUST_FLAG_SEALED, ELM_TRUST_FLAG_UNSIGNED_ACTIVE,
    ElmActionInvokeRequest, ElmCallFrame, ElmCellPolicyRequest, ElmCellPolicyV1, ElmContext,
    ElmCoreHealthHeader, ElmCoreHealthRecord, ElmEbiArch, ElmEbiExtensionPointDecl, ElmEbiImage,
    ElmEbiKernelApiRequirement, ElmEbiLifecycleHookKind, ElmEbiLifecycleHooks, ElmEbiLoadStatus,
    ElmEbiMenuDecl, ElmEbiProofV1, ElmEbiProviderPortDecl, ElmEbiRustHookSignature, ElmEbiSegment,
    ElmEbiSegmentKind, ElmEbiSourceKind, ElmEbiSourceRequest, ElmEbiTarget, ElmEbiUnit,
    ElmEkiBlockKind, ElmError, ElmExtensionAttachRequest, ElmExtensionDispatchRequest, ElmId,
    ElmKernelProviderSnapshotPage, ElmKernelProviderSpec, ElmKind, ElmLifecycleAction,
    ElmLifecyclePhase, ElmLifecyclePlanRequest, ElmManifest, ElmMenuItemKind,
    ElmMgrApiRegistryHeader, ElmMgrAuditHeader, ElmMgrCallHeader, ElmMgrCallKind,
    ElmMgrEventSubscribeRequest, ElmMgrEventUnsubscribeRequest, ElmMgrPolicyInfo,
    ElmMgrRelationKind, ElmMgrResponseHeader, ElmMgrSubscribedEventReadHeader,
    ElmMgrSubscribedEventReadRequest, ElmMixinMode, ElmName, ElmNativeCapabilityHeader,
    ElmNativeEntryFrameV1, ElmNexusBindPlanResponse, ElmNexusBindRequest, ElmNexusUnbindRequest,
    ElmOwnedResourceKind, ElmOwnedResourceOpsV1, ElmPanicStrategy, ElmPortAccessPolicy,
    ElmPrincipal, ElmPrincipalKind, ElmProjectionSourceRequest, ElmProviderAsyncCancelRequest,
    ElmProviderAsyncPollRequest, ElmProviderAsyncState, ElmProviderAsyncSubmitRequest,
    ElmProviderInvokeRequest, ElmProviderInvokeResponse, ElmProviderPortRegisterRequest,
    ElmProviderPortRegisterResponse, ElmProviderPortStatsHeader, ElmProviderQueueStatsHeader,
    ElmProviderSnapshotHeader, ElmProviderSnapshotRequest, ElmReplaceCellRequestV1, ElmReplyFrame,
    ElmResourceBudget, ElmResourceBudgetUpdateRequest, ElmResult, ElmRustAbiFingerprintV1,
    ElmState, ElmTodoRegistryHeader, ElmTodoRegistryRecord, ElmTrustAnchor, ElmTrustRuntimeInfoV1,
    ElmVersion, FlowDirection, FlowMode, Generation, canonical_ebi_digest, sha256, state_code,
};

use super::core::{
    ELM_EKI_ID, ELM_MGR_ID, ElmCore, ElmLifecycleExecutor, ElmMgrAccessTarget,
    KernelApiGrantRequest, management_namespace_allowed,
};
use super::mgr_channel::{dispatch_mgr_call_on_core, dispatch_mgr_call_on_core_as};

static OWNED_RESOURCE_TRACE: AtomicU64 = AtomicU64::new(0);

#[ktest]
fn elm_kernel_api_registry_grants_declared_namespace() {
    assert!(super::api_registry::test_requirement_roundtrip());
}

#[ktest]
fn elm_kernel_memory_allocator_enforces_exact_owner() {
    let before = super::resource_accounting::snapshot(ELM_MGR_ID, sched::now_ns_public());
    let request = allocator::MemoryRequest::new(allocator::MemoryDomain::Kernel, 64, 16)
        .with_zeroing(allocator::Zeroing::Zeroed);
    let record = allocator::KERNEL_ALLOCATOR
        .allocate_owned(ELM_MGR_ID.0, request)
        .expect("ELM owner 分配应成功");
    assert_eq!(record.accounting_owner(), ELM_MGR_ID.0);
    // Safety: 测试持有该分配的独占所有权，且逻辑长度由 allocator 记录确认。
    let bytes = unsafe { core::slice::from_raw_parts(record.ptr as *const u8, record.size) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    assert_eq!(
        allocator::KERNEL_ALLOCATOR.query_owned_allocation(ELM_EKI_ID.0, record.ptr),
        Err(allocator::OwnedAllocationError::PermissionDenied)
    );
    assert_eq!(
        allocator::KERNEL_ALLOCATOR.deallocate_owned(ELM_EKI_ID.0, record.ptr),
        Err(allocator::OwnedAllocationError::PermissionDenied)
    );
    assert_eq!(
        allocator::KERNEL_ALLOCATOR.reallocate_owned_excluding_range(
            ELM_MGR_ID.0,
            record.ptr,
            allocator::MemoryRequest::new(allocator::MemoryDomain::Kernel, 160, 32),
            record.ptr,
            core::mem::size_of::<kernel_api::memory::KernelMemoryAllocationV1>(),
        ),
        Err(allocator::OwnedAllocationError::AliasedRange)
    );
    let grown = allocator::KERNEL_ALLOCATOR
        .reallocate_owned(
            ELM_MGR_ID.0,
            record.ptr,
            allocator::MemoryRequest::new(allocator::MemoryDomain::Kernel, 160, 32),
        )
        .expect("同一 owner 扩容应成功");
    assert_eq!(grown.accounting_owner(), ELM_MGR_ID.0);
    assert_eq!(grown.size, 160);
    allocator::KERNEL_ALLOCATOR
        .deallocate_owned(ELM_MGR_ID.0, grown.ptr)
        .expect("同一 owner 释放应成功");
    let after = super::resource_accounting::snapshot(ELM_MGR_ID, sched::now_ns_public());
    assert_eq!(after.dynamic_alloc_bytes, before.dynamic_alloc_bytes);
    assert!(after.peak_dynamic_alloc_bytes >= before.peak_dynamic_alloc_bytes);
}

#[ktest]
fn elm_kernel_memory_function_table_executes_with_live_grant() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let requirement = ElmEbiKernelApiRequirement::new(
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER,
        kernel_api::memory::KERNEL_MEMORY_API_VERSION,
        kernel_api::memory::KERNEL_MEMORY_CAPABILITIES,
        kernel_api::memory::KERNEL_MEMORY_LAYOUT_HASH_V1,
    )
    .unwrap();
    let unit = ElmEbiUnit::new(
        manifest("kernel-memory-client", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(requirement);
    let loaded = core.load_ebi_unit(unit, ElmEbiArch::Any);
    assert_eq!(loaded.status, ElmEbiLoadStatus::Ok as i32);
    let id = ElmId(loaded.cell_id);
    let namespace = super::api_registry::query(
        id,
        Generation::FIRST,
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
        &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
        false,
    )
    .expect("完整内存能力 requirement 应取得函数表");
    // Safety: ApiGrantTokenV1 是已由布局测试固定为两个连续 u64 的 repr(C) 值；测试只用
    // 注册表刚返回的 grant 和 generation 构造与该 namespace 配套的调用令牌。
    let token = unsafe {
        core::mem::transmute::<[u64; 2], kernel_api::ApiGrantTokenV1>([
            namespace.grant_id,
            namespace.generation,
        ])
    };
    // Safety: namespace 注册表只保存经过静态尺寸、对齐和表头校验的常驻函数表地址。
    let table =
        unsafe { &*(namespace.table_address as *const kernel_api::memory::KernelMemoryApiV1) };

    let context = ElmContext::new(
        id,
        Some(ELM_MGR_ID),
        Generation::FIRST,
        ElmState::Active,
        ElmLifecyclePhase::Initialize,
        0,
    )
    .with_kind(ElmKind::Service);
    let _context = elm_model::enter_current_context(&context).expect("应建立测试 ELM 上下文");
    let native_guard =
        general::elm_guard::ElmGuard::enter(id.0, general::elm_guard::ELM_GUARD_PHASE_HOOK, 0)
            .expect("应建立测试原生边界");
    let stack_anchor = 0u64;
    let stack_middle = &stack_anchor as *const u64 as usize;
    assert!(native_guard.configure_native_bounds(
        0x10_0000,
        0x10_1000,
        0x10_0000,
        0x10_2000,
        stack_middle.saturating_sub(64 * 1024),
        stack_middle.saturating_add(64 * 1024),
        &[],
    ));

    let allocation = table
        .allocate_memory(
            token,
            kernel_api::memory::KernelMemoryRequestV1::new(64, 16).zeroed(),
        )
        .expect("带有效 grant 的分配应成功");
    assert_eq!(allocation.size, 64);
    // Safety: 返回记录证明该地址是当前 cell 持有的 64 字节活跃分配。
    let bytes = unsafe { core::slice::from_raw_parts(allocation.address as *const u8, 64) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    assert_eq!(
        table
            .query_memory(token, allocation.address)
            .expect("查询自有分配应成功")
            .address,
        allocation.address
    );
    assert_eq!(
        table
            .memory_stats(token)
            .expect("读取当前 cell 内存账本应成功")
            .current_bytes,
        64
    );
    // Safety: 测试没有保留指向旧对象的活跃引用，扩容后只使用返回的新地址。
    let grown = unsafe {
        table.reallocate_memory(
            token,
            allocation.address,
            kernel_api::memory::KernelMemoryRequestV1::new(160, 32),
        )
    }
    .expect("带有效 grant 的扩容应成功");
    assert_eq!(grown.size, 160);
    assert_eq!(
        table
            .memory_stats(token)
            .expect("扩容后账本应可读取")
            .current_bytes,
        160
    );
    // Safety: 测试不再持有 grown 指向对象的任何引用。
    unsafe { table.deallocate_memory(token, grown.address) }.expect("释放自有分配应成功");
    assert_eq!(
        table
            .memory_stats(token)
            .expect("释放后账本应可读取")
            .current_bytes,
        0
    );
    assert_eq!(super::api_registry::remove_cell(id), 1);
    assert_eq!(
        table.memory_stats(token),
        Err(kernel_api::memory::KERNEL_MEMORY_STATUS_PERMISSION)
    );
}

fn push_owned_resource_trace(stage: u64, handle: u64) -> Result<(), i32> {
    OWNED_RESOURCE_TRACE
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_mul(100)
                .and_then(|value| value.checked_add(stage * 10 + handle))
        })
        .map(|_| ())
        .map_err(|_| -1)
}

fn test_resource_quiesce(_: ElmId, _: Generation, handle: u64) -> Result<(), i32> {
    push_owned_resource_trace(1, handle)
}

fn test_resource_cancel(_: ElmId, _: Generation, handle: u64) -> Result<(), i32> {
    push_owned_resource_trace(2, handle)
}

fn test_resource_drain(_: ElmId, _: Generation, handle: u64) -> Result<(), i32> {
    push_owned_resource_trace(3, handle)
}

fn test_resource_release(_: ElmId, _: Generation, handle: u64) -> Result<(), i32> {
    push_owned_resource_trace(4, handle)
}

static TEST_OWNED_RESOURCE_OPS: ElmOwnedResourceOpsV1 = ElmOwnedResourceOpsV1::new(
    test_resource_quiesce,
    test_resource_cancel,
    test_resource_drain,
    test_resource_release,
);

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

fn delegated_budget(provider_ports: u16) -> ElmResourceBudget {
    ElmResourceBudget {
        max_provider_ports: provider_ports,
        max_provider_queue: 2,
        max_event_subscriptions: 2,
        max_pending_loads: 1,
        max_native_images: 1,
        max_native_faults: 1,
        max_audit_records: 16,
        ..ElmResourceBudget::DEFAULT
    }
}

fn parent_budget(provider_ports: u16) -> ElmResourceBudget {
    ElmResourceBudget {
        max_provider_ports: provider_ports,
        max_provider_queue: 8,
        max_event_subscriptions: 8,
        max_pending_loads: 4,
        max_native_images: 4,
        max_native_faults: 4,
        max_audit_records: 64,
        ..ElmResourceBudget::ROOT
    }
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

unsafe extern "C" fn test_native_entry_requests_abort(frame: *mut ElmNativeEntryFrameV1) -> i32 {
    if frame.is_null() {
        return -1;
    }
    // 安全性：调用方按照 ELM native entry v1 约定传入可读帧指针。
    let cell = unsafe { (*frame).cell_id };
    let _ = general::elm_guard::request_panic_recovery(cell);
    0
}

#[inline(never)]
unsafe extern "C" fn test_native_entry_panics(_frame: *mut ElmNativeEntryFrameV1) -> i32 {
    super::core::elm_api_abort_current_v1(elm_model::ELM_API_ABORT_REASON_PANIC)
}

#[inline(never)]
unsafe extern "C" fn test_native_entry_spins(_frame: *mut ElmNativeEntryFrameV1) -> i32 {
    loop {
        core::hint::spin_loop();
    }
}

#[inline(never)]
unsafe fn test_native_nested_fault_leaf() -> i32 {
    let fault_address = 0x4000_0000_0000usize as *const u64;
    // 安全性：该地址刻意选择为双架构均未映射的内核地址，用于验证 trap 恢复出口。
    unsafe { core::ptr::read_volatile(fault_address) as i32 }
}

#[inline(never)]
unsafe fn test_native_nested_fault_middle() -> i32 {
    // 安全性：测试必须让 fault 发生在调用门之下至少两层，证明现场 ra 不参与恢复。
    unsafe { test_native_nested_fault_leaf() }
}

#[inline(never)]
unsafe extern "C" fn test_native_entry_nested_fault(frame: *mut ElmNativeEntryFrameV1) -> i32 {
    if frame.is_null() {
        return -1;
    }
    // 安全性：调用方已经通过原生调用门建立受控恢复边界。
    unsafe { test_native_nested_fault_middle() }
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

static TEST_MIXIN_CALLS: AtomicUsize = AtomicUsize::new(0);

fn test_mixin_provider_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    TEST_MIXIN_CALLS.fetch_add(1, Ordering::Relaxed);
    let mut reply = ElmReplyFrame::new(
        frame.binding_id,
        frame.call_id,
        ELM_CALL_STATUS_OK,
        b"patched",
    );
    reply.flags = ELM_MIXIN_REPLY_STOP;
    reply
}

static TEST_MIXIN_PROVIDERS: [ElmKernelProviderSpec; 1] = [ElmKernelProviderSpec::new(
    "elm.test",
    "mixin",
    "elm.test.mixin@1",
    ELM_MGR_API_KIND_SUBSYSTEM,
    0,
    0,
    "test.mixin.handler@1",
    FlowDirection::Control,
    FlowMode::Shared,
    ElmPortAccessPolicy::Internal,
    true,
    ELM_KERNEL_PROVIDER_FLAG_NONE,
    test_mixin_provider_invoke,
    None,
    None,
)];

fn test_mixin_replace_provider_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    let mut reply = ElmReplyFrame::new(
        frame.binding_id,
        frame.call_id,
        ELM_CALL_STATUS_OK,
        b"replacement",
    );
    reply.flags = ELM_MIXIN_REPLY_REPLACE | ELM_MIXIN_REPLY_STOP;
    reply
}

static TEST_MIXIN_REPLACE_PROVIDERS: [ElmKernelProviderSpec; 1] = [ElmKernelProviderSpec::new(
    "elm.test",
    "mixin-replace",
    "elm.test.mixin.replace@1",
    ELM_MGR_API_KIND_SUBSYSTEM,
    0,
    0,
    "test.mixin.replace.handler@1",
    FlowDirection::Control,
    FlowMode::Shared,
    ElmPortAccessPolicy::Internal,
    true,
    ELM_KERNEL_PROVIDER_FLAG_NONE,
    test_mixin_replace_provider_invoke,
    None,
    None,
)];

static TEST_MIXIN_CHAIN_TRACE: AtomicU64 = AtomicU64::new(0);
static TEST_MIXIN_OBSERVER_TRACE: AtomicU64 = AtomicU64::new(0);
static TEST_MIXIN_DECOY_CALLS: AtomicUsize = AtomicUsize::new(0);

fn push_mixin_trace(trace: &AtomicU64, digit: u64) {
    trace
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_mul(10)?.checked_add(digit)
        })
        .unwrap();
}

fn mixin_payload_matches(frame: &ElmCallFrame, expected: &[u8]) -> bool {
    usize::from(frame.payload_len) == expected.len()
        && &frame.payload[..usize::from(frame.payload_len)] == expected
}

fn test_mixin_chain_decoy_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    TEST_MIXIN_DECOY_CALLS.fetch_add(1, Ordering::Relaxed);
    ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_INVALID)
}

fn test_mixin_chain_high_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    push_mixin_trace(
        &TEST_MIXIN_CHAIN_TRACE,
        if mixin_payload_matches(&frame, b"initial") {
            1
        } else {
            9
        },
    );
    let mut reply = ElmReplyFrame::new(
        frame.binding_id,
        frame.call_id,
        ELM_CALL_STATUS_OK,
        b"after-high",
    );
    reply.flags = ELM_MIXIN_REPLY_REPLACE;
    reply
}

fn test_mixin_chain_low_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    push_mixin_trace(
        &TEST_MIXIN_CHAIN_TRACE,
        if mixin_payload_matches(&frame, b"after-high") {
            2
        } else {
            8
        },
    );
    let mut reply = ElmReplyFrame::new(
        frame.binding_id,
        frame.call_id,
        ELM_CALL_STATUS_OK,
        b"chain-complete",
    );
    reply.flags = ELM_MIXIN_REPLY_STOP;
    reply
}

static TEST_MIXIN_CHAIN_HIGH_PROVIDERS: [ElmKernelProviderSpec; 2] = [
    ElmKernelProviderSpec::new(
        "elm.test",
        "mixin-chain-decoy",
        "elm.test.mixin.chain.decoy@1",
        ELM_MGR_API_KIND_SUBSYSTEM,
        0,
        0,
        "test.mixin.chain.decoy@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
        ELM_KERNEL_PROVIDER_FLAG_NONE,
        test_mixin_chain_decoy_invoke,
        None,
        None,
    ),
    ElmKernelProviderSpec::new(
        "elm.test",
        "mixin-chain-high",
        "elm.test.mixin.chain.high@1",
        ELM_MGR_API_KIND_SUBSYSTEM,
        0,
        0,
        "test.mixin.chain.high.handler@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
        ELM_KERNEL_PROVIDER_FLAG_NONE,
        test_mixin_chain_high_invoke,
        None,
        None,
    ),
];

static TEST_MIXIN_CHAIN_LOW_PROVIDERS: [ElmKernelProviderSpec; 1] = [ElmKernelProviderSpec::new(
    "elm.test",
    "mixin-chain-low",
    "elm.test.mixin.chain.low@1",
    ELM_MGR_API_KIND_SUBSYSTEM,
    0,
    0,
    "test.mixin.chain.low.handler@1",
    FlowDirection::Control,
    FlowMode::Shared,
    ElmPortAccessPolicy::Internal,
    true,
    ELM_KERNEL_PROVIDER_FLAG_NONE,
    test_mixin_chain_low_invoke,
    None,
    None,
)];

fn test_mixin_observer_control_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    push_mixin_trace(
        &TEST_MIXIN_OBSERVER_TRACE,
        if mixin_payload_matches(&frame, b"observed") {
            1
        } else {
            9
        },
    );
    let mut reply = ElmReplyFrame::new(
        frame.binding_id,
        frame.call_id,
        ELM_CALL_STATUS_OK,
        b"must-not-propagate",
    );
    reply.flags = ELM_MIXIN_REPLY_REPLACE | ELM_MIXIN_REPLY_STOP;
    reply
}

fn test_mixin_observer_passive_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    push_mixin_trace(
        &TEST_MIXIN_OBSERVER_TRACE,
        if mixin_payload_matches(&frame, b"observed") {
            2
        } else {
            8
        },
    );
    ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_OK)
}

static TEST_MIXIN_OBSERVER_CONTROL_PROVIDERS: [ElmKernelProviderSpec; 1] =
    [ElmKernelProviderSpec::new(
        "elm.test",
        "mixin-observer-control",
        "elm.test.mixin.observer.control@1",
        ELM_MGR_API_KIND_SUBSYSTEM,
        0,
        0,
        "test.mixin.observer.control.handler@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
        ELM_KERNEL_PROVIDER_FLAG_NONE,
        test_mixin_observer_control_invoke,
        None,
        None,
    )];

static TEST_MIXIN_OBSERVER_PASSIVE_PROVIDERS: [ElmKernelProviderSpec; 1] =
    [ElmKernelProviderSpec::new(
        "elm.test",
        "mixin-observer-passive",
        "elm.test.mixin.observer.passive@1",
        ELM_MGR_API_KIND_SUBSYSTEM,
        0,
        0,
        "test.mixin.observer.passive.handler@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
        ELM_KERNEL_PROVIDER_FLAG_NONE,
        test_mixin_observer_passive_invoke,
        None,
        None,
    )];

fn test_snapshot_provider_snapshot(out: &mut [u8]) -> Result<usize, i32> {
    let payload = b"snapshot-ok";
    out[..payload.len()].copy_from_slice(payload);
    Ok(payload.len())
}

fn test_projection_source_provider(
    reader: &dyn elm_model::ElmImageReader,
    _arch: ElmEbiArch,
) -> Result<elm_model::ElmEbiImage, ElmEbiLoadStatus> {
    let payload = reader.read_all(elm_model::ELM_IMAGE_SESSION_MAX_LENGTH)?;
    elm_model::parse_eki_image(&payload).map_err(|_| ElmEbiLoadStatus::InvalidUnit)
}

const BUSY_PROJECTION_SOURCE_ID: u64 = 0x5453_5450_524f_4a33;
const BUSY_PROJECTION_SOURCE_OWNER: ElmId = ElmId(0x5453_5402);
const BUSY_PROJECTION_SOURCE_GENERATION: Generation = Generation(9);

fn test_busy_projection_source_provider(
    _reader: &dyn elm_model::ElmImageReader,
    _arch: ElmEbiArch,
) -> Result<elm_model::ElmEbiImage, ElmEbiLoadStatus> {
    assert_eq!(
        super::source::retire_projection_sources_owned_by(
            BUSY_PROJECTION_SOURCE_OWNER,
            BUSY_PROJECTION_SOURCE_GENERATION,
        ),
        Err(super::source::ProjectionSourceRegistryError::Busy)
    );
    Err(ElmEbiLoadStatus::RuntimeRejected)
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
    let mut out = vec![0; record + 16 + ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN];
    write_u32(&mut out, 0, 1);
    write_u16(&mut out, record, point.len() as u16);
    write_u16(&mut out, record + 2, contract.len() as u16);
    write_u32(&mut out, record + 4, ElmMixinMode::Chain as u32);
    fixed_copy(&mut out, record + 16, ELM_MGR_RELATION_POINT_LEN, point);
    fixed_copy(
        &mut out,
        record + 16 + ELM_MGR_RELATION_POINT_LEN,
        ELM_NEXUS_CONTRACT_LEN,
        contract,
    );
    out
}

fn eki_extension_block(target_name: &str, point: &str, contract: &str) -> Vec<u8> {
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
    write_u16(&mut out, record + 6, contract.len() as u16);
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
    let mut out = vec![0; record + ELM_EKI_PROVIDER_PORT_RECORD_SIZE];
    write_u32(&mut out, 0, 1);
    write_u32(&mut out, record, access as u32);
    write_u32(&mut out, record + 4, direction as u32);
    write_u32(&mut out, record + 8, mode as u32);
    write_u16(&mut out, record + 16, contract.len() as u16);
    fixed_copy(&mut out, record + 24, ELM_NEXUS_CONTRACT_LEN, contract);
    out
}

fn kernel_abi_fingerprint(arch: ElmEbiArch) -> ElmRustAbiFingerprintV1 {
    let target = match arch {
        ElmEbiArch::Any => b"any".as_slice(),
        ElmEbiArch::Riscv64 => b"riscv64gc-unknown-none-elf".as_slice(),
        ElmEbiArch::LoongArch64 => b"loongarch64-unknown-none".as_slice(),
    };
    ElmRustAbiFingerprintV1::new(
        sha256(env!("ELM_RUSTC_VERSION").as_bytes()),
        sha256(target),
        sha256(elm_model::kernel_api_manifest_v1(arch as u32).as_bytes()),
        ELM_API_CURRENT_VERSION,
        ElmPanicStrategy::AbortThroughRuntime,
        1,
        0,
    )
}

fn signed_metadata_image(name: &str, release_epoch: u64, signing: &SigningKey) -> ElmEbiImage {
    signed_metadata_image_with_kind(name, ElmKind::Service, release_epoch, signing)
}

fn signed_metadata_image_with_kind(
    name: &str,
    kind: ElmKind,
    release_epoch: u64,
    signing: &SigningKey,
) -> ElmEbiImage {
    let unit = ElmEbiUnit::new(manifest(name, kind), ElmEbiTarget::new(ElmEbiArch::Any))
        .with_lifecycle_hooks(lifecycle_hooks());
    signed_unit_image(unit, release_epoch, signing)
}

fn signed_unit_image(unit: ElmEbiUnit, release_epoch: u64, signing: &SigningKey) -> ElmEbiImage {
    let fingerprint = kernel_abi_fingerprint(ElmEbiArch::Any);
    let image = ElmEbiImage::new(unit).with_abi_fingerprint(fingerprint.clone());
    let public_key = signing.verifying_key().to_bytes();
    let mut proof = ElmEbiProofV1 {
        source_identifier: "kernel-test".into(),
        source_digest: sha256(b"kernel-test-source"),
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
    image.with_proof(proof)
}

fn eki_abi_fingerprint_block(arch: ElmEbiArch) -> Vec<u8> {
    let fingerprint = kernel_abi_fingerprint(arch);
    let mut out = vec![0; elm_model::ELM_EKI_ABI_FINGERPRINT_BLOCK_SIZE];
    write_u16(&mut out, 0, elm_model::ELM_RUST_ABI_FINGERPRINT_VERSION);
    write_u16(&mut out, 2, fingerprint.elmapi_version);
    out[4] = fingerprint.panic_strategy as u8;
    out[5] = fingerprint.code_model;
    write_u64(&mut out, 8, fingerprint.target_features);
    write_u32(&mut out, 16, fingerprint.flags);
    out[24..56].copy_from_slice(&fingerprint.rustc_commit_hash);
    out[56..88].copy_from_slice(&fingerprint.target_spec_hash);
    out[88..120].copy_from_slice(&fingerprint.kernel_api_hash);
    out
}

fn eki_image(blocks: &[(ElmEkiBlockKind, Vec<u8>)]) -> Vec<u8> {
    let mut all_blocks = blocks.to_vec();
    if !all_blocks
        .iter()
        .any(|(kind, _)| *kind == ElmEkiBlockKind::AbiFingerprint)
    {
        all_blocks.push((
            ElmEkiBlockKind::AbiFingerprint,
            eki_abi_fingerprint_block(ElmEbiArch::Any),
        ));
    }
    let block_count = all_blocks.len();
    let mut out = vec![0; ELM_EKI_HEADER_SIZE + block_count * ELM_EKI_BLOCK_DESC_SIZE];
    let mut payload_offset = out.len();
    for (index, (kind, payload)) in all_blocks.iter().enumerate() {
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
    ebi_source_payload_under_parent(kind, payload, ELM_MGR_ID, ElmResourceBudget::DEFAULT)
}

fn eki_source_payload(payload: &[u8]) -> Vec<u8> {
    eki_source_payload_under_parent(payload, ELM_MGR_ID, ElmResourceBudget::DEFAULT)
}

fn eki_source_payload_under_parent(
    payload: &[u8],
    parent: ElmId,
    budget: ElmResourceBudget,
) -> Vec<u8> {
    let projection = projection_source_payload(elm_model::ELM_EKI_PROJECTION_SOURCE_ID, payload);
    ebi_source_payload_under_parent(ElmEbiSourceKind::Projection, &projection, parent, budget)
}

fn ebi_source_payload_under_parent(
    kind: ElmEbiSourceKind,
    payload: &[u8],
    parent: ElmId,
    budget: ElmResourceBudget,
) -> Vec<u8> {
    let request = ElmEbiSourceRequest::new_under_parent(kind, parent, budget, payload.len() as u32);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.source_kind);
    push_u32(&mut out, request.flags);
    push_u64(&mut out, request.parent_cell_id);
    push_resource_budget(&mut out, request.budget);
    push_u16(&mut out, request.reserved0);
    push_u16(&mut out, request.reserved1);
    push_u32(&mut out, request.payload_len);
    push_u32(&mut out, request.reserved2);
    push_u32(&mut out, request.reserved3);
    out.extend_from_slice(payload);
    out
}

fn raw_ebi_source_payload(source_kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_u16(&mut out, elm_model::ELM_EBI_SOURCE_ABI_VERSION);
    push_u16(&mut out, source_kind);
    push_u32(&mut out, 0);
    push_u64(&mut out, ELM_MGR_ID.0);
    push_resource_budget(&mut out, ElmResourceBudget::DEFAULT);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u32(&mut out, payload.len() as u32);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    out.extend_from_slice(payload);
    out
}

fn push_resource_budget(out: &mut Vec<u8>, budget: ElmResourceBudget) {
    push_u16(out, budget.max_provider_ports);
    push_u16(out, budget.max_provider_queue);
    push_u16(out, budget.max_event_subscriptions);
    push_u16(out, budget.max_pending_loads);
    push_u16(out, budget.max_native_images);
    push_u16(out, budget.max_native_faults);
    push_u16(out, budget.max_audit_records);
    push_u16(out, budget.max_concurrent_calls);
    push_u64(out, budget.max_native_image_bytes);
    push_u64(out, budget.max_native_stack_bytes);
    push_u64(out, budget.max_dynamic_alloc_bytes);
    push_u64(out, budget.max_cpu_time_ns_per_call);
    push_u64(out, budget.cpu_budget_ns_per_period);
    push_u64(out, budget.cpu_period_ns);
}

fn projection_source_payload(provider_id: u64, payload: &[u8]) -> Vec<u8> {
    let request = ElmProjectionSourceRequest::new(provider_id, payload.len() as u32);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.flags);
    push_u32(&mut out, request.reserved0);
    push_u64(&mut out, request.provider_id);
    push_u32(&mut out, request.payload_len);
    push_u32(&mut out, request.reserved1);
    out.extend_from_slice(payload);
    out
}

fn projection_session_payload(provider_id: u64, session_id: u64) -> Vec<u8> {
    let request = ElmProjectionSourceRequest::from_image_session(provider_id);
    let reference = elm_model::ElmImageSessionReferenceV1::new(session_id);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.flags);
    push_u32(&mut out, request.reserved0);
    push_u64(&mut out, request.provider_id);
    push_u32(&mut out, request.payload_len);
    push_u32(&mut out, request.reserved1);
    push_u16(&mut out, reference.abi_version);
    push_u16(&mut out, reference.flags);
    push_u32(&mut out, reference.reserved);
    push_u64(&mut out, reference.session_id);
    out
}

fn image_session_begin_payload(image: &[u8], ttl_ms: u32) -> Vec<u8> {
    let request =
        elm_model::ElmImageSessionBeginRequestV1::new(image.len() as u64, ttl_ms, sha256(image));
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.hash_alg);
    push_u32(&mut out, request.flags);
    push_u64(&mut out, request.total_len);
    push_u32(&mut out, request.ttl_ms);
    push_u16(&mut out, request.digest_len);
    push_u16(&mut out, request.reserved0);
    out.extend_from_slice(&request.expected_digest);
    push_u64(&mut out, request.reserved1);
    out
}

fn image_session_write_payload(session_id: u64, offset: u64, chunk: &[u8]) -> Vec<u8> {
    let request =
        elm_model::ElmImageSessionWriteRequestV1::new(session_id, offset, chunk.len() as u32);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.flags);
    push_u32(&mut out, request.reserved0);
    push_u64(&mut out, request.session_id);
    push_u64(&mut out, request.offset);
    push_u32(&mut out, request.chunk_len);
    push_u32(&mut out, request.reserved1);
    out.extend_from_slice(chunk);
    out
}

fn image_session_request_payload(session_id: u64) -> Vec<u8> {
    let request = elm_model::ElmImageSessionRequestV1::new(session_id);
    let mut out = Vec::new();
    push_u16(&mut out, request.abi_version);
    push_u16(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    push_u64(&mut out, request.session_id);
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

fn cell_policy_payload(policy: &ElmCellPolicyV1) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, policy.cell_id);
    push_u64(&mut out, policy.generation);
    push_u64(&mut out, policy.policy_epoch);
    push_u32(&mut out, policy.flags);
    push_u32(&mut out, policy.allowed_actions);
    push_u32(&mut out, policy.provider_flags);
    push_u32(&mut out, policy.extension_flags);
    push_u32(&mut out, policy.native_flags);
    push_u32(&mut out, policy.resource_flags);
    push_u32(&mut out, policy.status as u32);
    push_u32(&mut out, policy.reserved);
    push_u64(&mut out, policy.blockers);
    out
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

fn register_device_provider_specs(core: &mut ElmCore) -> usize {
    core.register_kernel_provider_specs(general::dev::elm::providers())
        .unwrap()
}

fn register_vfs_provider_specs(core: &mut ElmCore) -> usize {
    core.register_kernel_provider_specs(vfs::elm::providers())
        .unwrap()
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

fn eki_replace_payload(target: ElmId, image: &[u8], migration_limit: u32) -> Vec<u8> {
    let projection = projection_source_payload(elm_model::ELM_EKI_PROJECTION_SOURCE_ID, image);
    let mut request = ElmReplaceCellRequestV1::new(
        target.0,
        ElmEbiSourceKind::Projection as u16,
        projection.len() as u32,
    );
    request.migration_limit = migration_limit;
    replace_cell_payload(&request, &projection)
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

    assert_eq!(core.cells().len(), 2);
    let mgr = core
        .cells()
        .iter()
        .find(|cell| cell.id == ELM_MGR_ID)
        .unwrap();
    assert_eq!(mgr.parent, None);
    assert_eq!(mgr.state, ElmState::Active);
    assert_eq!(mgr.kind, ElmKind::Manager);
    assert_eq!(mgr.ebi_source, ElmEbiSourceKind::Builtin);
    assert_eq!(mgr.resource_budget.max_provider_ports, 256);
    assert!(!mgr.isolated);
    let eki = core
        .cells()
        .iter()
        .find(|cell| cell.id == ELM_EKI_ID)
        .unwrap();
    assert_eq!(eki.parent, Some(ELM_MGR_ID));
    assert_eq!(eki.state, ElmState::Active);
    assert_eq!(eki.kind, ElmKind::Service);
    assert_eq!(eki.ebi_source, ElmEbiSourceKind::Builtin);
    assert_eq!(eki.ebi_status, ElmEbiLoadStatus::Ok);
    assert!(eki.lifecycle_executor_ready);
    assert!(eki.lifecycle_initialized);
    assert!(!eki.has_native_code);
    assert_eq!(core.menu_items().len(), 1);
    assert_eq!(core.menu_items()[0].owner, ELM_MGR_ID);
    assert_eq!(core.menu_items()[0].route, "elm/mgr/health");
    assert_eq!(core.menu_items()[0].flags & ELM_MENU_FLAG_TODO, 0);
    assert!(provider_port_id_by_contract(&mut core, "device.discovered@1").is_none());
    assert!(provider_port_id_by_contract(&mut core, "vfs.lookup@1").is_none());
    assert!(provider_port_id_by_contract(&mut core, "io.packet.rx@1").is_none());

    let health = core.health_bytes();
    assert_eq!(read_i32(&health, 8), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(&health, 12), 0);
    assert_eq!(read_u32(&health, 4), 17);

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
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_TRUST), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_PROJECTION_SOURCES), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_JOURNAL), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_RESOURCES), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_EXECUTIONS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_SEQUENCES), 0);
}

#[ktest]
fn elm_owned_resources_block_replace_and_drain_before_detach() {
    OWNED_RESOURCE_TRACE.store(0, Ordering::Release);
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let loaded = core.load_ebi_unit(menu_unit("owned-resource-cell"), ElmEbiArch::Any);
    assert_eq!(loaded.status, ElmEbiLoadStatus::Ok as i32);
    let cell = ElmId(loaded.cell_id);

    let first = super::owned_resource::register(
        cell,
        Generation::FIRST,
        ElmOwnedResourceKind::Timer,
        1,
        TEST_OWNED_RESOURCE_OPS,
    )
    .unwrap();
    let second = super::owned_resource::register(
        cell,
        Generation::FIRST,
        ElmOwnedResourceKind::WorkItem,
        2,
        TEST_OWNED_RESOURCE_OPS,
    )
    .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        super::owned_resource::owner_snapshot(cell)
            .unwrap()
            .resource_count,
        2
    );

    let replace = core.preflight_lifecycle(ElmLifecyclePlanRequest::new(
        cell.0,
        ElmLifecycleAction::Replace,
    ));
    assert_eq!(replace.allowed, 0);
    assert_ne!(replace.blockers & ELM_POLICY_BLOCK_LEASE_BUSY, 0);
    let pause = core.preflight_lifecycle(ElmLifecyclePlanRequest::new(
        cell.0,
        ElmLifecycleAction::Pause,
    ));
    assert_eq!(pause.allowed, 0);
    assert_ne!(pause.blockers & ELM_POLICY_BLOCK_LEASE_BUSY, 0);

    let detached = core.detach_cell(cell);
    assert_eq!(detached.status, ELM_MGR_STATUS_OK);
    assert_eq!(detached.final_state, state_code(ElmState::Retired));
    assert_eq!(
        OWNED_RESOURCE_TRACE.load(Ordering::Acquire),
        1_211_222_132_314_241
    );
    assert!(super::owned_resource::owner_snapshot(cell).is_none());
    assert_eq!(
        super::owned_resource::count_owned_by(cell, Generation::FIRST),
        0
    );
    assert!(core.cells().iter().all(|runtime| runtime.id != cell));
}

#[ktest]
fn elm_dynamic_allocations_block_generation_replace_until_released() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let loaded = core.load_ebi_unit(menu_unit("heap-owner-cell"), ElmEbiArch::Any);
    assert_eq!(loaded.status, ElmEbiLoadStatus::Ok as i32);
    let cell = ElmId(loaded.cell_id);

    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_owned(
            cell.0,
            allocator::MemoryRequest::new(allocator::MemoryDomain::Kernel, 96, 16),
        )
        .expect("ELM 动态分配应成功");
    let blocked = core.preflight_lifecycle(ElmLifecyclePlanRequest::new(
        cell.0,
        ElmLifecycleAction::Replace,
    ));
    assert_eq!(blocked.allowed, 0);
    assert_ne!(blocked.blockers & ELM_POLICY_BLOCK_RESOURCE_QUOTA, 0);

    allocator::KERNEL_ALLOCATOR
        .deallocate_owned(cell.0, allocation.ptr)
        .expect("释放 ELM 动态分配应成功");
    let allowed = core.preflight_lifecycle(ElmLifecyclePlanRequest::new(
        cell.0,
        ElmLifecycleAction::Replace,
    ));
    assert_eq!(allowed.allowed, 1);
    assert_eq!(allowed.blockers & ELM_POLICY_BLOCK_RESOURCE_QUOTA, 0);

    let detached = core.detach_cell(cell);
    assert_eq!(detached.status, ELM_MGR_STATUS_OK);
}

#[ktest]
fn elm_mgr_reports_sealed_unsigned_test_policy() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryTrustState, &[]));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&response),
        core::mem::size_of::<ElmTrustRuntimeInfoV1>()
    );
    let payload = response_payload(&response);
    let flags = read_u32(payload, 4);
    assert_ne!(flags & ELM_TRUST_FLAG_SEALED, 0);
    assert_ne!(flags & ELM_TRUST_FLAG_ALLOW_UNSIGNED, 0);
    assert_eq!(flags & ELM_TRUST_FLAG_UNSIGNED_ACTIVE, 0);
    assert_eq!(read_u32(payload, 8), 0);
    assert_eq!(read_u32(payload, 12), 0);
    assert_eq!(read_u32(payload, 16), 0);
}

#[ktest]
fn elm_trust_signed_load_commits_epoch_and_rejects_rollback() {
    let signing = SigningKey::from_bytes(&[17; 32]);
    let image = signed_metadata_image("signed-core-cell", 7, &signing);
    let public_key = signing.verifying_key().to_bytes();
    let mut core = ElmCore::new();
    core.set_allow_unsigned_external(false).unwrap();
    core.register_trust_anchor(ElmTrustAnchor::new("kernel-test-root", public_key).unwrap())
        .unwrap();
    core.init_builtin_mgr().unwrap();

    let response = core.load_ebi_image(image, ElmEbiArch::Any);
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(response.final_state, state_code(ElmState::Active));
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert!(!cell.trust_unsigned);
    assert_eq!(cell.signer_key_id, sha256(&public_key));
    assert_eq!(cell.release_epoch, 7);
    let trust = core.trust_runtime_info();
    assert_eq!(trust.anchor_count, 1);
    assert_eq!(trust.accepted_epoch_count, 1);
    assert_eq!(trust.flags & ELM_TRUST_FLAG_UNSIGNED_ACTIVE, 0);

    assert_eq!(
        core.detach_cell(ElmId(response.cell_id)).status,
        ELM_MGR_STATUS_OK
    );
    let rollback = core.load_ebi_image(
        signed_metadata_image("signed-core-cell", 6, &signing),
        ElmEbiArch::Any,
    );
    assert_eq!(rollback.status, ElmEbiLoadStatus::RollbackRejected as i32);
    assert_eq!(core.trust_runtime_info().accepted_epoch_count, 1);
}

#[ktest]
fn elm_trust_rejects_abi_mismatch_before_unsigned_fallback() {
    let mut core = ElmCore::new();
    core.set_allow_unsigned_external(true).unwrap();
    core.init_builtin_mgr().unwrap();
    let unit = ElmEbiUnit::new(
        manifest("abi-mismatch-cell", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks());
    let mut fingerprint = kernel_abi_fingerprint(ElmEbiArch::Any);
    fingerprint.kernel_api_hash[0] ^= 0xff;
    let image = ElmEbiImage::new(unit).with_abi_fingerprint(fingerprint);

    let response = core.load_ebi_image(image, ElmEbiArch::Any);
    assert_eq!(
        response.status,
        ElmEbiLoadStatus::AbiFingerprintRejected as i32
    );
    assert_eq!(core.cells().len(), 2);
    assert_eq!(core.trust_runtime_info().accepted_epoch_count, 0);
}

#[ktest]
fn elm_trust_does_not_ignore_invalid_proof_when_unsigned_is_allowed() {
    let signing = SigningKey::from_bytes(&[23; 32]);
    let public_key = signing.verifying_key().to_bytes();
    let mut image = signed_metadata_image("invalid-proof-cell", 1, &signing);
    image.proof.as_mut().unwrap().signature[0] ^= 0x80;
    let mut core = ElmCore::new();
    core.set_allow_unsigned_external(true).unwrap();
    core.register_trust_anchor(ElmTrustAnchor::new("invalid-proof-root", public_key).unwrap())
        .unwrap();
    core.init_builtin_mgr().unwrap();

    let response = core.load_ebi_image(image, ElmEbiArch::Any);
    assert_eq!(response.status, ElmEbiLoadStatus::UntrustedImage as i32);
    assert_eq!(core.cells().len(), 2);
    assert_eq!(core.trust_runtime_info().accepted_epoch_count, 0);
}

#[ktest]
fn elm_trust_unsigned_active_clears_after_last_unsigned_cell_detaches() {
    let mut core = ElmCore::new();
    core.set_allow_unsigned_external(true).unwrap();
    core.init_builtin_mgr().unwrap();
    let unit = ElmEbiUnit::new(
        manifest("unsigned-cell", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks());
    let image =
        ElmEbiImage::new(unit).with_abi_fingerprint(kernel_abi_fingerprint(ElmEbiArch::Any));

    let response = core.load_ebi_image(image, ElmEbiArch::Any);
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    assert_ne!(
        core.trust_runtime_info().flags & ELM_TRUST_FLAG_UNSIGNED_ACTIVE,
        0
    );
    assert_eq!(
        core.detach_cell(ElmId(response.cell_id)).status,
        ELM_MGR_STATUS_OK
    );
    assert_eq!(
        core.trust_runtime_info().flags & ELM_TRUST_FLAG_UNSIGNED_ACTIVE,
        0
    );
}

#[ktest]
fn elm_mgr_rejects_stale_and_out_of_scope_cell_principals() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let first = core.load_ebi_unit(menu_unit("auth-first"), ElmEbiArch::Any);
    let second = core.load_ebi_unit(menu_unit("auth-second"), ElmEbiArch::Any);
    assert_eq!(first.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(second.status, ElmEbiLoadStatus::Ok as i32);
    let first_id = ElmId(first.cell_id);
    let generation = core
        .cells()
        .iter()
        .find(|cell| cell.id == first_id)
        .unwrap()
        .generation;

    let query = mgr_call(ElmMgrCallKind::QueryMenu, &[]);
    let response = dispatch_mgr_call_on_core_as(
        &mut core,
        ElmPrincipal::elm_cell(first_id, generation),
        &query,
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);

    let stale = dispatch_mgr_call_on_core_as(
        &mut core,
        ElmPrincipal::elm_cell(first_id, Generation(generation.0 + 1)),
        &query,
    );
    assert_eq!(response_status(&stale), ELM_MGR_STATUS_PERMISSION);
    let audits = core.audit_bytes();
    let record_size = read_u16(&audits, 2) as usize;
    let count = read_u32(&audits, 4) as usize;
    let last = core::mem::size_of::<ElmMgrAuditHeader>() + (count - 1) * record_size;
    assert_ne!(
        read_u64(&audits, last + 24) & ELM_POLICY_BLOCK_CALLER_STALE,
        0
    );
    assert_eq!(
        read_u32(&audits, last + 40),
        ElmPrincipalKind::ElmCell as u32
    );
    assert_eq!(read_u64(&audits, last + 48), first_id.0);

    let pause_other =
        lifecycle_request_payload(&elm_model::ElmLifecycleRequest::new(second.cell_id));
    let denied = dispatch_mgr_call_on_core_as(
        &mut core,
        ElmPrincipal::elm_cell(first_id, generation),
        &mgr_call(ElmMgrCallKind::PauseCell, &pause_other),
    );
    assert_eq!(response_status(&denied), ELM_MGR_STATUS_PERMISSION);
    let audits = core.audit_bytes();
    let count = read_u32(&audits, 4) as usize;
    let last = core::mem::size_of::<ElmMgrAuditHeader>() + (count - 1) * record_size;
    assert_ne!(
        read_u64(&audits, last + 24) & ELM_POLICY_BLOCK_SCOPE_DENIED,
        0
    );
}

#[ktest]
fn elm_mgr_cell_policy_update_cannot_self_escalate() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let load = core.load_ebi_unit(menu_unit("auth-policy"), ElmEbiArch::Any);
    assert_eq!(load.status, ElmEbiLoadStatus::Ok as i32);
    let id = ElmId(load.cell_id);
    let mut restricted = core.query_cell_policy(ElmCellPolicyRequest::new(id.0));
    restricted.provider_flags = 0;
    let restricted = core.update_cell_policy(restricted);
    assert_eq!(restricted.status, ELM_MGR_STATUS_OK);

    let mut escalation = restricted;
    escalation.provider_flags = ELM_PROVIDER_POLICY_ALL;
    let response = dispatch_mgr_call_on_core_as(
        &mut core,
        ElmPrincipal::elm_cell(id, Generation(restricted.generation)),
        &mgr_call(
            ElmMgrCallKind::UpdateCellPolicy,
            &cell_policy_payload(&escalation),
        ),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_PERMISSION);
    let audits = core.audit_bytes();
    let record_size = read_u16(&audits, 2) as usize;
    let count = read_u32(&audits, 4) as usize;
    let last = core::mem::size_of::<ElmMgrAuditHeader>() + (count - 1) * record_size;
    assert_ne!(
        read_u64(&audits, last + 24) & ELM_POLICY_BLOCK_POLICY_ESCALATION,
        0
    );
}

#[ktest]
fn elm_mgr_manager_and_user_admin_principals_have_global_authority() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mgr_generation = core
        .cells()
        .iter()
        .find(|cell| cell.id == ELM_MGR_ID)
        .unwrap()
        .generation;
    let query = mgr_call(ElmMgrCallKind::QueryAudit, &[]);

    let manager = dispatch_mgr_call_on_core_as(
        &mut core,
        ElmPrincipal::elm_cell(ELM_MGR_ID, mgr_generation),
        &query,
    );
    assert_eq!(response_status(&manager), ELM_MGR_STATUS_OK);

    let admin = dispatch_mgr_call_on_core_as(&mut core, ElmPrincipal::user_admin(42, 0), &query);
    assert_eq!(response_status(&admin), ELM_MGR_STATUS_OK);
    let audits = core.audit_bytes();
    let record_size = read_u16(&audits, 2) as usize;
    let count = read_u32(&audits, 4) as usize;
    let last = core::mem::size_of::<ElmMgrAuditHeader>() + (count - 1) * record_size;
    assert_eq!(
        read_u32(&audits, last + 36),
        ELM_AUDIT_FLAG_OPERATION | ELM_AUDIT_FLAG_AUTHORIZATION
    );
    assert_eq!(
        read_u32(&audits, last + 40),
        ElmPrincipalKind::UserAdmin as u32
    );
    assert_eq!(read_u64(&audits, last + 48), 42);
}

#[ktest]
fn elm_delegated_manager_requires_signed_explicit_grant_and_has_global_scope() {
    let signing = SigningKey::from_bytes(&[41; 32]);
    let public_key = signing.verifying_key().to_bytes();
    let mut core = ElmCore::new();
    core.set_allow_unsigned_external(false).unwrap();
    core.register_trust_anchor(ElmTrustAnchor::new("delegated-manager-root", public_key).unwrap())
        .unwrap();
    core.init_builtin_mgr().unwrap();

    let loaded = core.load_declarative_ebi_image_from_source_under_parent(
        signed_metadata_image_with_kind("delegated-manager", ElmKind::Manager, 1, &signing),
        ElmEbiArch::Any,
        ElmEbiSourceKind::Projection,
        ELM_MGR_ID,
        ElmResourceBudget::DEFAULT,
        true,
    );
    assert_eq!(loaded.status, ElmEbiLoadStatus::Ok as i32);
    let manager_id = ElmId(loaded.cell_id);
    let manager = core
        .cells()
        .iter()
        .find(|cell| cell.id == manager_id)
        .unwrap();
    assert_eq!(manager.kind, ElmKind::Manager);
    assert_ne!(
        manager.cell_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT,
        0
    );
    let manager_generation = manager.generation;

    let target = core.load_ebi_unit(menu_unit("delegated-manager-target"), ElmEbiArch::Any);
    assert_eq!(target.status, ElmEbiLoadStatus::Ok as i32);
    let authorization = core.authorize_mgr_call(
        ElmPrincipal::elm_cell(manager_id, manager_generation),
        ElmMgrCallKind::PauseCell,
        ElmMgrAccessTarget::Cell(ElmId(target.cell_id)),
    );
    assert!(authorization.allowed());
    assert_eq!(
        authorization.authority,
        ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER
    );

    let child = core.load_ebi_unit_under_parent(
        menu_unit("delegated-manager-child"),
        ElmEbiArch::Any,
        manager_id,
        delegated_budget(1),
    );
    assert_eq!(child.status, ElmEbiLoadStatus::Ok as i32);
    let child_policy = core.query_cell_policy(ElmCellPolicyRequest::new(child.cell_id));
    assert_eq!(
        child_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT,
        0
    );

    let mut manager_policy = core.query_cell_policy(ElmCellPolicyRequest::new(manager_id.0));
    manager_policy.allowed_actions &= !ELM_CELL_POLICY_ALLOW_MANAGEMENT;
    let denied = core.update_cell_policy(manager_policy);
    assert_eq!(denied.status, ELM_MGR_STATUS_PERMISSION);
    assert_eq!(denied.blockers, ELM_POLICY_BLOCK_POLICY_ESCALATION);
}

#[ktest]
fn elm_kernel_api_requires_signed_explicit_external_approval() {
    let signing = SigningKey::from_bytes(&[43; 32]);
    let public_key = signing.verifying_key().to_bytes();
    let mut core = ElmCore::new();
    core.set_allow_unsigned_external(false).unwrap();
    core.register_trust_anchor(ElmTrustAnchor::new("kernel-api-root", public_key).unwrap())
        .unwrap();
    core.init_builtin_mgr().unwrap();

    let requirement = ElmEbiKernelApiRequirement::new(
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER,
        kernel_api::memory::KERNEL_MEMORY_API_VERSION,
        kernel_api::memory::KERNEL_MEMORY_CAP_ALLOCATE
            | kernel_api::memory::KERNEL_MEMORY_CAP_QUERY,
        kernel_api::memory::KERNEL_MEMORY_LAYOUT_HASH_V1,
    )
    .unwrap();
    let denied_unit = ElmEbiUnit::new(
        manifest("kernel-api-no-approval", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(requirement.clone());
    let denied = core.load_declarative_ebi_image_from_source_under_parent(
        signed_unit_image(denied_unit, 1, &signing),
        ElmEbiArch::Any,
        ElmEbiSourceKind::Projection,
        ELM_MGR_ID,
        ElmResourceBudget::DEFAULT,
        false,
    );
    assert_eq!(denied.status, ElmEbiLoadStatus::RuntimeRejected as i32);
    assert_eq!(
        super::api_registry::query(
            ElmId(denied.cell_id),
            Generation::FIRST,
            kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
            &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
            false,
        ),
        Err(super::api_registry::ApiRegistryError::CapabilityDenied)
    );

    let approved_unit = ElmEbiUnit::new(
        manifest("kernel-api-approved", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(requirement);
    let principal = ElmPrincipal::user_admin(77, 43);
    let authorization = core.authorize_mgr_call(
        principal,
        ElmMgrCallKind::LoadCell,
        ElmMgrAccessTarget::Load(ELM_MGR_ID, ElmResourceBudget::DEFAULT),
    );
    assert!(authorization.allowed());
    let approved = core.load_declarative_ebi_image_from_source_under_parent_with_kernel_api_grant(
        signed_unit_image(approved_unit, 1, &signing),
        ElmEbiArch::Any,
        ElmEbiSourceKind::Projection,
        ELM_MGR_ID,
        ElmResourceBudget::DEFAULT,
        false,
        KernelApiGrantRequest::from_authorization(true, authorization),
    );
    assert_eq!(approved.status, ElmEbiLoadStatus::Ok as i32);
    let approved_id = ElmId(approved.cell_id);
    let namespace = super::api_registry::query(
        approved_id,
        Generation::FIRST,
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
        &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
        false,
    )
    .expect("签名且显式批准的 requirement 应取得函数表");
    assert_ne!(namespace.grant_id, 0);
    assert_eq!(
        namespace.capabilities,
        kernel_api::memory::KERNEL_MEMORY_CAP_ALLOCATE
            | kernel_api::memory::KERNEL_MEMORY_CAP_QUERY
    );
    assert_eq!(super::api_registry::remove_cell(approved_id), 1);

    let manager_requirement = ElmEbiKernelApiRequirement::new(
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER,
        kernel_api::memory::KERNEL_MEMORY_API_VERSION,
        kernel_api::memory::KERNEL_MEMORY_CAP_STATS,
        kernel_api::memory::KERNEL_MEMORY_LAYOUT_HASH_V1,
    )
    .unwrap();
    let manager_unit = ElmEbiUnit::new(
        manifest("kernel-api-manager-approved", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(manager_requirement);
    let manager_authorization = core.authorize_mgr_call(
        ElmPrincipal::elm_cell(ELM_MGR_ID, Generation::FIRST),
        ElmMgrCallKind::LoadCell,
        ElmMgrAccessTarget::Load(ELM_MGR_ID, ElmResourceBudget::DEFAULT),
    );
    assert!(manager_authorization.allowed());
    assert_eq!(manager_authorization.authority, ELM_AUDIT_AUTHORITY_MANAGER);
    let manager_approved = core
        .load_declarative_ebi_image_from_source_under_parent_with_kernel_api_grant(
            signed_unit_image(manager_unit, 1, &signing),
            ElmEbiArch::Any,
            ElmEbiSourceKind::Projection,
            ELM_MGR_ID,
            ElmResourceBudget::DEFAULT,
            false,
            KernelApiGrantRequest::from_authorization(true, manager_authorization),
        );
    assert_eq!(manager_approved.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(
        super::api_registry::remove_cell(ElmId(manager_approved.cell_id)),
        1
    );
}

#[ktest]
fn elm_kernel_api_internal_memory_source_is_automatically_approved() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let requirement = ElmEbiKernelApiRequirement::new(
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER,
        kernel_api::memory::KERNEL_MEMORY_API_VERSION,
        kernel_api::memory::KERNEL_MEMORY_CAP_STATS,
        kernel_api::memory::KERNEL_MEMORY_LAYOUT_HASH_V1,
    )
    .unwrap();
    let unit = ElmEbiUnit::new(
        manifest("kernel-api-internal", ElmKind::Other),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(requirement);
    let loaded = core.load_ebi_unit(unit, ElmEbiArch::Any);
    assert_eq!(loaded.status, ElmEbiLoadStatus::Ok as i32);
    let id = ElmId(loaded.cell_id);
    let namespace = super::api_registry::query(
        id,
        Generation::FIRST,
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
        &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
        false,
    )
    .expect("内核 Memory 来源应自动批准 requirements");
    assert_eq!(
        namespace.capabilities,
        kernel_api::memory::KERNEL_MEMORY_CAP_STATS
    );
    assert_eq!(super::api_registry::remove_cell(id), 1);
}

#[ktest]
fn elm_kernel_api_replace_switches_generation_transactionally() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let initial_requirement = ElmEbiKernelApiRequirement::new(
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER,
        kernel_api::memory::KERNEL_MEMORY_API_VERSION,
        kernel_api::memory::KERNEL_MEMORY_CAP_QUERY,
        kernel_api::memory::KERNEL_MEMORY_LAYOUT_HASH_V1,
    )
    .unwrap();
    let initial_unit = ElmEbiUnit::new(
        manifest("kernel-api-replace", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(initial_requirement);
    let loaded = core.load_ebi_unit(initial_unit, ElmEbiArch::Any);
    assert_eq!(loaded.status, ElmEbiLoadStatus::Ok as i32);
    let id = ElmId(loaded.cell_id);
    assert!(
        super::api_registry::query(
            id,
            Generation::FIRST,
            kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
            &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
            false,
        )
        .is_ok_and(|namespace| {
            namespace.capabilities == kernel_api::memory::KERNEL_MEMORY_CAP_QUERY
        })
    );

    let next_requirement = ElmEbiKernelApiRequirement::new(
        kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER,
        kernel_api::memory::KERNEL_MEMORY_API_VERSION,
        kernel_api::memory::KERNEL_MEMORY_CAP_STATS,
        kernel_api::memory::KERNEL_MEMORY_LAYOUT_HASH_V1,
    )
    .unwrap();
    let missing_requirement =
        ElmEbiKernelApiRequirement::new("kernel.missing", 1, 1, [0x91; 32]).unwrap();
    let rejected_unit = ElmEbiUnit::new(
        manifest("kernel-api-replace", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(next_requirement.clone())
    .with_kernel_api_requirement(missing_requirement);
    let rejected = core.replace_declarative_cell_from_ebi_image_with_source(
        id,
        ElmEbiImage::new(rejected_unit),
        ElmEbiArch::Any,
        0,
        ElmEbiSourceKind::Memory,
    );
    assert_eq!(rejected.status, ELM_MGR_STATUS_PERMISSION);
    assert_eq!(rejected.generation, Generation::FIRST.0);
    assert!(
        super::api_registry::query(
            id,
            Generation::FIRST,
            kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
            &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
            false,
        )
        .is_ok_and(|namespace| {
            namespace.capabilities == kernel_api::memory::KERNEL_MEMORY_CAP_QUERY
        })
    );
    assert_eq!(
        super::api_registry::query(
            id,
            Generation(2),
            kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
            &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
            false,
        ),
        Err(super::api_registry::ApiRegistryError::CapabilityDenied)
    );

    let replacement_unit = ElmEbiUnit::new(
        manifest("kernel-api-replace", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_kernel_api_requirement(next_requirement);
    let replaced = core.replace_declarative_cell_from_ebi_image_with_source(
        id,
        ElmEbiImage::new(replacement_unit),
        ElmEbiArch::Any,
        0,
        ElmEbiSourceKind::Memory,
    );
    assert_eq!(replaced.status, ELM_MGR_STATUS_OK);
    assert_eq!(replaced.generation, 2);
    assert_eq!(
        super::api_registry::query(
            id,
            Generation::FIRST,
            kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
            &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
            false,
        ),
        Err(super::api_registry::ApiRegistryError::CapabilityDenied)
    );
    assert!(
        super::api_registry::query(
            id,
            Generation(2),
            kernel_api::memory::KERNEL_MEMORY_API_IDENTIFIER.as_bytes(),
            &[kernel_api::memory::KERNEL_MEMORY_API_VERSION],
            false,
        )
        .is_ok_and(|namespace| {
            namespace.capabilities == kernel_api::memory::KERNEL_MEMORY_CAP_STATS
        })
    );
    assert_eq!(super::api_registry::remove_cell(id), 1);
}

#[ktest]
fn elm_management_grant_rejects_wrong_kind_unsigned_image_and_self_grant() {
    let signing = SigningKey::from_bytes(&[42; 32]);
    let public_key = signing.verifying_key().to_bytes();
    let mut core = ElmCore::new();
    core.set_allow_unsigned_external(false).unwrap();
    core.register_trust_anchor(ElmTrustAnchor::new("management-kind-root", public_key).unwrap())
        .unwrap();
    core.init_builtin_mgr().unwrap();

    let wrong_kind = core.load_declarative_ebi_image_from_source_under_parent(
        signed_metadata_image_with_kind("not-a-manager", ElmKind::Service, 1, &signing),
        ElmEbiArch::Any,
        ElmEbiSourceKind::Projection,
        ELM_MGR_ID,
        ElmResourceBudget::DEFAULT,
        true,
    );
    assert_eq!(wrong_kind.status, ElmEbiLoadStatus::UntrustedImage as i32);

    let service = core.load_ebi_unit(menu_unit("self-grant-service"), ElmEbiArch::Any);
    let mut policy = core.query_cell_policy(ElmCellPolicyRequest::new(service.cell_id));
    policy.allowed_actions |= ELM_CELL_POLICY_ALLOW_MANAGEMENT;
    let denied = core.update_cell_policy(policy);
    assert_eq!(denied.status, ELM_MGR_STATUS_PERMISSION);
    assert_eq!(denied.blockers, ELM_POLICY_BLOCK_POLICY_ESCALATION);

    let mut unsigned_core = ElmCore::new();
    unsigned_core.set_allow_unsigned_external(true).unwrap();
    unsigned_core.init_builtin_mgr().unwrap();
    let unsigned_unit = ElmEbiUnit::new(
        manifest("unsigned-manager", ElmKind::Manager),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks());
    let unsigned_image = ElmEbiImage::new(unsigned_unit)
        .with_abi_fingerprint(kernel_abi_fingerprint(ElmEbiArch::Any));
    let unsigned = unsigned_core.load_declarative_ebi_image_from_source_under_parent(
        unsigned_image,
        ElmEbiArch::Any,
        ElmEbiSourceKind::Projection,
        ELM_MGR_ID,
        ElmResourceBudget::DEFAULT,
        true,
    );
    assert_eq!(unsigned.status, ElmEbiLoadStatus::UntrustedImage as i32);
}

#[ktest]
fn elm_management_namespace_requires_manager_kind_state_generation_and_capability() {
    let base = ElmContext::new(
        ElmId(100),
        Some(ELM_MGR_ID),
        Generation::FIRST,
        ElmState::Loaded,
        ElmLifecyclePhase::Initialize,
        0,
    );
    let granted = base
        .with_kind(ElmKind::Manager)
        .with_allowed_actions(ELM_CELL_POLICY_ALLOW_MANAGEMENT);
    let guard = elm_model::enter_current_context(&granted).unwrap();
    assert!(management_namespace_allowed(
        elm_model::current_context().unwrap()
    ));
    drop(guard);

    let service = base
        .with_kind(ElmKind::Service)
        .with_allowed_actions(ELM_CELL_POLICY_ALLOW_MANAGEMENT);
    let guard = elm_model::enter_current_context(&service).unwrap();
    assert!(!management_namespace_allowed(
        elm_model::current_context().unwrap()
    ));
    drop(guard);

    let ungranted = base.with_kind(ElmKind::Manager).with_allowed_actions(0);
    let guard = elm_model::enter_current_context(&ungranted).unwrap();
    assert!(!management_namespace_allowed(
        elm_model::current_context().unwrap()
    ));
    drop(guard);

    let faulted = ElmContext::new(
        ElmId(100),
        Some(ELM_MGR_ID),
        Generation::FIRST,
        ElmState::Faulted,
        ElmLifecyclePhase::Finalize,
        0,
    )
    .with_kind(ElmKind::Manager)
    .with_allowed_actions(ELM_CELL_POLICY_ALLOW_MANAGEMENT);
    let guard = elm_model::enter_current_context(&faulted).unwrap();
    assert!(!management_namespace_allowed(
        elm_model::current_context().unwrap()
    ));
    drop(guard);
}

#[ktest]
fn elm_non_builtin_cell_cannot_request_management_grant() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let actor = core.load_ebi_unit(menu_unit("grant-request-actor"), ElmEbiArch::Any);
    let actor_id = ElmId(actor.cell_id);
    let generation = core
        .cells()
        .iter()
        .find(|cell| cell.id == actor_id)
        .unwrap()
        .generation;
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("grant-request-manager", "0.1.0", ElmKind::Manager),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let mut payload = eki_source_payload(&image);
    write_u32(&mut payload, 4, ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT);
    let response = dispatch_mgr_call_on_core_as(
        &mut core,
        ElmPrincipal::elm_cell(actor_id, generation),
        &mgr_call(ElmMgrCallKind::LoadCell, &payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_PERMISSION);
    assert_eq!(core.cells().len(), 3);
}

#[ktest]
fn elm_mgr_load_request_attaches_child_to_explicit_parent() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let parent = core.load_ebi_unit_under_parent(
        menu_unit("load-parent"),
        ElmEbiArch::Any,
        ELM_MGR_ID,
        parent_budget(10),
    );
    assert_eq!(parent.status, ElmEbiLoadStatus::Ok as i32);
    let parent_id = ElmId(parent.cell_id);
    let parent_generation = core
        .cells()
        .iter()
        .find(|cell| cell.id == parent_id)
        .unwrap()
        .generation;
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("load-child", "0.1.0", ElmKind::Extension),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let child_budget = delegated_budget(3);
    let payload = eki_source_payload_under_parent(&image, parent_id, child_budget);
    let response = dispatch_mgr_call_on_core_as(
        &mut core,
        ElmPrincipal::elm_cell(parent_id, parent_generation),
        &mgr_call(ElmMgrCallKind::LoadCell, &payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::Ok as i32);
    let child_id = ElmId(read_u64(load, 0));
    let child = core
        .cells()
        .iter()
        .find(|cell| cell.id == child_id)
        .unwrap();
    assert_eq!(child.parent, Some(parent_id));
    assert_eq!(child.resource_budget, child_budget);
    let parent_policy = core.query_cell_policy(ElmCellPolicyRequest::new(parent_id.0));
    assert_eq!(
        child.cell_policy.allowed_actions,
        parent_policy.allowed_actions
    );
    assert_eq!(
        child.cell_policy.provider_flags,
        parent_policy.provider_flags
    );

    let mut reduced_parent = parent_policy;
    reduced_parent.provider_flags = 0;
    let rejected = core.update_cell_policy(reduced_parent);
    assert_eq!(rejected.status, ELM_MGR_STATUS_PERMISSION);
    assert_eq!(rejected.blockers, ELM_POLICY_BLOCK_POLICY_ESCALATION);

    let mut reduced_child = core.query_cell_policy(ElmCellPolicyRequest::new(child_id.0));
    reduced_child.provider_flags = 0;
    assert_eq!(
        core.update_cell_policy(reduced_child).status,
        ELM_MGR_STATUS_OK
    );
    assert_eq!(
        core.update_cell_policy(reduced_parent).status,
        ELM_MGR_STATUS_OK
    );
}

#[ktest]
fn elm_mgr_ancestor_execution_token_blocks_policy_update() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let parent = core.load_ebi_unit_under_parent(
        menu_unit("token-parent"),
        ElmEbiArch::Any,
        ELM_MGR_ID,
        parent_budget(10),
    );
    let parent_id = ElmId(parent.cell_id);
    let child = core.load_ebi_unit_under_parent(
        menu_unit("token-child"),
        ElmEbiArch::Any,
        parent_id,
        delegated_budget(3),
    );
    assert_eq!(child.status, ElmEbiLoadStatus::Ok as i32);
    let generation = core
        .cells()
        .iter()
        .find(|cell| cell.id == parent_id)
        .unwrap()
        .generation;
    let authorization = core.authorize_mgr_call(
        ElmPrincipal::elm_cell(parent_id, generation),
        ElmMgrCallKind::PauseCell,
        ElmMgrAccessTarget::Cell(ElmId(child.cell_id)),
    );
    assert!(authorization.allowed());
    let execution = core
        .reserve_mgr_authorization_execution(authorization, ElmMgrCallKind::PauseCell)
        .unwrap();
    let policy = core.query_cell_policy(ElmCellPolicyRequest::new(parent_id.0));
    let blocked = core.update_cell_policy(policy);
    assert_eq!(blocked.status, ELM_MGR_STATUS_BUSY);
    assert_eq!(blocked.blockers, ELM_POLICY_BLOCK_PROVIDER_BUSY);
    core.release_mgr_authorization_execution(execution);
    assert_eq!(core.update_cell_policy(policy).status, ELM_MGR_STATUS_OK);
}

#[ktest]
fn elm_mgr_policy_epoch_and_lock_are_enforced_by_core() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let load = core.load_ebi_unit(menu_unit("policy-lock"), ElmEbiArch::Any);
    let id = ElmId(load.cell_id);
    let original = core.query_cell_policy(ElmCellPolicyRequest::new(id.0));
    let current = core.update_cell_policy(original);
    assert_eq!(current.status, ELM_MGR_STATUS_OK);

    let stale = core.update_cell_policy(original);
    assert_eq!(stale.status, ELM_MGR_STATUS_PERMISSION);
    assert_eq!(stale.blockers, ELM_POLICY_BLOCK_CALLER_STALE);

    let mut locked = current;
    locked.flags |= ELM_CELL_POLICY_FLAG_LOCKED;
    let locked = core.update_cell_policy(locked);
    assert_eq!(locked.status, ELM_MGR_STATUS_OK);
    let denied = core.update_cell_policy(locked);
    assert_eq!(denied.status, ELM_MGR_STATUS_PERMISSION);
    assert_eq!(denied.blockers, ELM_POLICY_BLOCK_CAPABILITY_DENIED);
}

#[ktest]
fn elm_mgr_resource_budget_is_hierarchically_delegated() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let parent = core.load_ebi_unit_under_parent(
        menu_unit("budget-parent"),
        ElmEbiArch::Any,
        ELM_MGR_ID,
        parent_budget(10),
    );
    assert_eq!(parent.status, ElmEbiLoadStatus::Ok as i32);
    let parent_id = ElmId(parent.cell_id);
    let first = core.load_ebi_unit_under_parent(
        menu_unit("budget-first"),
        ElmEbiArch::Any,
        parent_id,
        delegated_budget(6),
    );
    assert_eq!(first.status, ElmEbiLoadStatus::Ok as i32);
    let rejected = core.load_ebi_unit_under_parent(
        menu_unit("budget-overflow"),
        ElmEbiArch::Any,
        parent_id,
        delegated_budget(5),
    );
    assert_eq!(rejected.status, ElmEbiLoadStatus::RuntimeRejected as i32);
    let second = core.load_ebi_unit_under_parent(
        menu_unit("budget-second"),
        ElmEbiArch::Any,
        parent_id,
        delegated_budget(4),
    );
    assert_eq!(second.status, ElmEbiLoadStatus::Ok as i32);

    let shrink = core.update_resource_budget(ElmResourceBudgetUpdateRequest::new(
        parent_id.0,
        parent_budget(9),
    ));
    assert_eq!(shrink.status, ELM_MGR_STATUS_BUSY);
    assert_eq!(shrink.blockers, ELM_POLICY_BLOCK_RESOURCE_QUOTA);

    assert_eq!(
        core.detach_cell(ElmId(second.cell_id)).status,
        ELM_MGR_STATUS_OK
    );
    let shrink = core.update_resource_budget(ElmResourceBudgetUpdateRequest::new(
        parent_id.0,
        parent_budget(6),
    ));
    assert_eq!(shrink.status, ELM_MGR_STATUS_OK);

    let first_id = ElmId(first.cell_id);
    let first_generation = core
        .cells()
        .iter()
        .find(|cell| cell.id == first_id)
        .unwrap()
        .generation;
    let escalation = core.authorize_mgr_call(
        ElmPrincipal::elm_cell(first_id, first_generation),
        ElmMgrCallKind::UpdateResourceBudget,
        ElmMgrAccessTarget::ResourceUpdate(ElmResourceBudgetUpdateRequest::new(
            first_id.0,
            delegated_budget(7),
        )),
    );
    assert!(!escalation.allowed());
    assert_eq!(escalation.blockers, ELM_POLICY_BLOCK_POLICY_ESCALATION);
}

#[ktest]
fn elm_builtin_eki_cell_is_protected_like_mgr() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let pause = core.pause_cell(ELM_EKI_ID);
    assert_eq!(pause.status, ELM_MGR_STATUS_BUSY);
    assert_eq!(pause.reason, ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED);

    let detach = core.detach_cell(ELM_EKI_ID);
    assert_eq!(detach.status, ELM_MGR_STATUS_BUSY);
    assert_eq!(detach.reason, ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED);

    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("replace-builtin-eki", "0.1.0", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let payload = eki_replace_payload(ELM_EKI_ID, &image, 0);
    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::ReplaceCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let replace = response_payload(&response);
    assert_eq!(read_i32(replace, 8), ELM_MGR_STATUS_BUSY);
    assert_ne!(
        read_u64(replace, 32) & ELM_POLICY_BLOCK_BUILTIN_PROTECTED,
        0
    );
}

#[ktest]
fn elm_native_capability_snapshot_contains_builtin_runtime_exports() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let bytes = core.native_capabilities_bytes();
    assert_eq!(
        read_u16(&bytes, 2) as usize,
        core::mem::size_of::<elm_model::ElmNativeCapabilityRecord>()
    );
    assert_eq!(read_u32(&bytes, 4), 2);
    assert_eq!(
        read_u32(&bytes, 8) & ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED,
        0
    );
    assert_eq!(read_u64(&bytes, 16), core.last_event_sequence());
    assert_eq!(
        bytes.len(),
        core::mem::size_of::<ElmNativeCapabilityHeader>()
            + 2 * core::mem::size_of::<elm_model::ElmNativeCapabilityRecord>()
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
    assert!(read_u32(&bytes, 4) >= 2);
    assert!(read_u32(&bytes, 8) >= 2);
    for removed in [
        "runtime.elm_mgr_eki_boot",
        "runtime.resource_quota",
        "source.non_eki",
        "source.projection_remote",
        "native.fault_isolation",
        "native.trap_recovery",
        "runtime.hot_replace_rebind",
        "provider.snapshot_streaming",
    ] {
        assert!(
            !bytes
                .windows(removed.len())
                .any(|window| window == removed.as_bytes())
        );
    }
    assert!(
        bytes
            .windows("projection.soyo_profile".len())
            .any(|window| window == b"projection.soyo_profile")
    );
    assert!(
        !bytes
            .windows("provenance.external_resolver".len())
            .any(|window| window == b"provenance.external_resolver")
    );
    assert!(
        !bytes
            .windows("native.trap_trampoline".len())
            .any(|window| window == b"native.trap_trampoline")
    );
    assert!(
        !bytes
            .windows("native.panic_boundary".len())
            .any(|window| window == b"native.panic_boundary")
    );
    assert!(
        bytes
            .windows("framework.rust_elm_distribution".len())
            .any(|window| window == b"framework.rust_elm_distribution")
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
fn elm_menu_ebi_unit_activates_with_default_lifecycle() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let response = core.load_ebi_unit(menu_unit("elm-test-menu"), ElmEbiArch::Riscv64);
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(response.final_state, state_code(ElmState::Active));
    assert_eq!(core.menu_items().len(), 2);

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.parent, Some(ELM_MGR_ID));
    assert_eq!(cell.state, ElmState::Active);
    assert!(cell.lifecycle_hooks_declared);
    assert!(cell.lifecycle_initialized);
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
fn elm_mixin_dispatch_invokes_extension_provider() {
    TEST_MIXIN_CALLS.store(0, Ordering::Relaxed);
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::default();

    let unit = ElmEbiUnit::new(
        manifest("elm-test-mixin", ElmKind::Extension),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks());
    let response =
        core.load_ebi_unit_with_lifecycle_executor(unit, ElmEbiArch::Riscv64, &mut executor);
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    let extension = ElmId(response.cell_id);
    assert_eq!(
        core.register_kernel_provider_specs_for_owner(extension, &TEST_MIXIN_PROVIDERS)
            .unwrap(),
        1
    );

    let attach =
        ElmExtensionAttachRequest::new(extension.0, ELM_MGR_ID.0, "menu.item", "mgr.menu.item@1");
    let attach_response = core.commit_extension_attach(attach);
    assert_eq!(attach_response.status, ELM_MGR_STATUS_OK);
    assert_eq!(attach_response.allowed, 1);

    let dispatch = ElmExtensionDispatchRequest::new(
        ELM_MGR_ID.0,
        extension.0,
        7,
        "menu.item",
        "mgr.menu.item@1",
    );
    let dispatch_response = core.dispatch_extension_on_local_core(dispatch).unwrap();
    assert_eq!(dispatch_response.status, ELM_MGR_STATUS_OK);
    assert_eq!(dispatch_response.matched_extensions, 1);
    assert_eq!(dispatch_response.called_extensions, 1);
    assert_eq!(dispatch_response.reply.status, ELM_CALL_STATUS_OK);
    assert_eq!(dispatch_response.reply.flags, ELM_MIXIN_REPLY_STOP);
    assert_eq!(
        &dispatch_response.reply.payload[..usize::from(dispatch_response.reply.payload_len)],
        b"patched"
    );
    assert_eq!(TEST_MIXIN_CALLS.load(Ordering::Relaxed), 1);
}

#[ktest]
fn elm_mixin_chain_uses_priority_payload_and_handler_contract() {
    TEST_MIXIN_CHAIN_TRACE.store(0, Ordering::Relaxed);
    TEST_MIXIN_DECOY_CALLS.store(0, Ordering::Relaxed);
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::default();

    let high = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-mixin-chain-high", ElmKind::Extension),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks()),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    let low = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-mixin-chain-low", ElmKind::Extension),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks()),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(high.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(low.status, ElmEbiLoadStatus::Ok as i32);
    let high = ElmId(high.cell_id);
    let low = ElmId(low.cell_id);
    assert_eq!(
        core.register_kernel_provider_specs_for_owner(high, &TEST_MIXIN_CHAIN_HIGH_PROVIDERS)
            .unwrap(),
        2
    );
    assert_eq!(
        core.register_kernel_provider_specs_for_owner(low, &TEST_MIXIN_CHAIN_LOW_PROVIDERS)
            .unwrap(),
        1
    );

    let high_attach =
        ElmExtensionAttachRequest::new(high.0, ELM_MGR_ID.0, "menu.item", "mgr.menu.item@1")
            .with_dispatch("test.mixin.chain.high.handler@1", 50);
    let low_attach =
        ElmExtensionAttachRequest::new(low.0, ELM_MGR_ID.0, "menu.item", "mgr.menu.item@1")
            .with_dispatch("test.mixin.chain.low.handler@1", -10);
    assert_eq!(
        core.commit_extension_attach(low_attach).status,
        ELM_MGR_STATUS_OK
    );
    assert_eq!(
        core.commit_extension_attach(high_attach).status,
        ELM_MGR_STATUS_OK
    );

    let mut dispatch =
        ElmExtensionDispatchRequest::new(ELM_MGR_ID.0, 0, 11, "menu.item", "mgr.menu.item@1");
    dispatch.payload_len = b"initial".len() as u16;
    dispatch.payload[..b"initial".len()].copy_from_slice(b"initial");
    let response = core.dispatch_extension_on_local_core(dispatch).unwrap();
    assert_eq!(response.status, ELM_MGR_STATUS_OK);
    assert_eq!(response.mode, ElmMixinMode::Chain as u32);
    assert_eq!(response.matched_extensions, 2);
    assert_eq!(response.called_extensions, 2);
    assert_eq!(TEST_MIXIN_CHAIN_TRACE.load(Ordering::Relaxed), 12);
    assert_eq!(TEST_MIXIN_DECOY_CALLS.load(Ordering::Relaxed), 0);
    assert_eq!(response.reply.flags, ELM_MIXIN_REPLY_STOP);
    assert_eq!(
        &response.reply.payload[..usize::from(response.reply.payload_len)],
        b"chain-complete"
    );
}

#[ktest]
fn elm_mixin_observer_broadcasts_original_payload_without_control() {
    TEST_MIXIN_OBSERVER_TRACE.store(0, Ordering::Relaxed);
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::default();

    let target = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-observer-target", ElmKind::Service),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks())
        .with_extension_point(
            ElmEbiExtensionPointDecl::new("test.observe", "test.observe@1")
                .unwrap()
                .with_mode(ElmMixinMode::Observer),
        ),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    let control = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-observer-control", ElmKind::Extension),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks()),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    let passive = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-observer-passive", ElmKind::Extension),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks()),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(target.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(control.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(passive.status, ElmEbiLoadStatus::Ok as i32);
    let target = ElmId(target.cell_id);
    let control = ElmId(control.cell_id);
    let passive = ElmId(passive.cell_id);
    assert_eq!(
        core.register_kernel_provider_specs_for_owner(
            control,
            &TEST_MIXIN_OBSERVER_CONTROL_PROVIDERS,
        )
        .unwrap(),
        1
    );
    assert_eq!(
        core.register_kernel_provider_specs_for_owner(
            passive,
            &TEST_MIXIN_OBSERVER_PASSIVE_PROVIDERS,
        )
        .unwrap(),
        1
    );
    assert_eq!(
        core.commit_extension_attach(
            ElmExtensionAttachRequest::new(passive.0, target.0, "test.observe", "test.observe@1",)
                .with_dispatch("test.mixin.observer.passive.handler@1", -10),
        )
        .status,
        ELM_MGR_STATUS_OK
    );
    assert_eq!(
        core.commit_extension_attach(
            ElmExtensionAttachRequest::new(control.0, target.0, "test.observe", "test.observe@1",)
                .with_dispatch("test.mixin.observer.control.handler@1", 10),
        )
        .status,
        ELM_MGR_STATUS_OK
    );

    let mut dispatch =
        ElmExtensionDispatchRequest::new(target.0, 0, 12, "test.observe", "test.observe@1");
    dispatch.payload_len = b"observed".len() as u16;
    dispatch.payload[..b"observed".len()].copy_from_slice(b"observed");
    let response = core.dispatch_extension_on_local_core(dispatch).unwrap();
    assert_ne!(response.status, ELM_MGR_STATUS_OK);
    assert_eq!(response.mode, ElmMixinMode::Observer as u32);
    assert_eq!(response.matched_extensions, 2);
    assert_eq!(response.called_extensions, 2);
    assert_ne!(response.blockers & ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED, 0);
    assert_eq!(TEST_MIXIN_OBSERVER_TRACE.load(Ordering::Relaxed), 12);
}

#[ktest]
fn elm_mixin_exclusive_rejects_second_attachment() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let mut executor = RecordingLifecycleExecutor::default();
    let target = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-exclusive-target", ElmKind::Service),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks())
        .with_extension_point(
            ElmEbiExtensionPointDecl::new("test.exclusive", "test.exclusive@1")
                .unwrap()
                .with_mode(ElmMixinMode::Exclusive),
        ),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    let first = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-exclusive-first", ElmKind::Extension),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks()),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    let second = core.load_ebi_unit_with_lifecycle_executor(
        ElmEbiUnit::new(
            manifest("elm-test-exclusive-second", ElmKind::Extension),
            ElmEbiTarget::new(ElmEbiArch::Any),
        )
        .with_lifecycle_hooks(lifecycle_hooks()),
        ElmEbiArch::Riscv64,
        &mut executor,
    );
    assert_eq!(target.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(first.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(second.status, ElmEbiLoadStatus::Ok as i32);
    let target = ElmId(target.cell_id);
    let first = ElmId(first.cell_id);
    let second = ElmId(second.cell_id);
    assert_eq!(
        core.commit_extension_attach(ElmExtensionAttachRequest::new(
            first.0,
            target.0,
            "test.exclusive",
            "test.exclusive@1",
        ))
        .status,
        ELM_MGR_STATUS_OK
    );
    let rejected = core.preflight_extension_attach(ElmExtensionAttachRequest::new(
        second.0,
        target.0,
        "test.exclusive",
        "test.exclusive@1",
    ));
    assert_eq!(rejected.allowed, 0);
    assert_ne!(rejected.blockers & ELM_POLICY_BLOCK_EXTENSION_DUPLICATE, 0);
}

#[ktest]
fn elm_mixin_replace_requires_explicit_patch_policy() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let unit = ElmEbiUnit::new(
        manifest("elm-test-mixin-policy", ElmKind::Extension),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks());
    let response = core.load_ebi_unit(unit, ElmEbiArch::Riscv64);
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    let extension = ElmId(response.cell_id);
    assert_eq!(
        core.register_kernel_provider_specs_for_owner(extension, &TEST_MIXIN_REPLACE_PROVIDERS)
            .unwrap(),
        1
    );
    let attach =
        ElmExtensionAttachRequest::new(extension.0, ELM_MGR_ID.0, "menu.item", "mgr.menu.item@1");
    assert_eq!(
        core.commit_extension_attach(attach).status,
        ELM_MGR_STATUS_OK
    );

    let mut policy = core.query_cell_policy(ElmCellPolicyRequest::new(extension.0));
    policy.extension_flags &= !ELM_EXTENSION_POLICY_MIXIN_PATCH;
    policy.native_flags &= !ELM_NATIVE_POLICY_MIXIN_PATCH;
    assert_eq!(core.update_cell_policy(policy).status, ELM_MGR_STATUS_OK);

    let dispatch = ElmExtensionDispatchRequest::new(
        ELM_MGR_ID.0,
        extension.0,
        9,
        "menu.item",
        "mgr.menu.item@1",
    );
    let result = core.dispatch_extension_on_local_core(dispatch).unwrap();
    assert_eq!(result.status, ELM_MGR_STATUS_PERMISSION);
    assert_eq!(result.called_extensions, 1);
    assert_ne!(result.blockers & ELM_POLICY_BLOCK_CAPABILITY_DENIED, 0);
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
    assert_eq!(core.cells().len(), 2);
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
fn elm_mgr_rejects_external_distribution_source_kind() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let payload = raw_ebi_source_payload(5, &[]);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);
}

#[ktest]
fn elm_mgr_rejects_unknown_source_flags_and_cross_protocol_session_flags() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let mut source = raw_ebi_source_payload(ElmEbiSourceKind::Projection as u16, &[]);
    write_u32(&mut source, 4, 1 << 31);
    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &source));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let mut session = image_session_begin_payload(b"image", 1_000);
    write_u32(&mut session, 4, ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT);
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::BeginImageSession, &session),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);
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
    let payload = eki_source_payload(&image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&response),
        core::mem::size_of::<elm_model::ElmLoadCellResponse>()
    );
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::Ok as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Active));
    let cell_id = read_u64(load, 0);
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(cell_id))
        .unwrap();
    assert_eq!(cell.parent, Some(ELM_MGR_ID));
    assert_eq!(cell.state, ElmState::Active);
    assert_eq!(cell.ebi_source, ElmEbiSourceKind::Projection);
    assert_eq!(core.menu_items().len(), 2);
}

#[ktest]
fn elm_mgr_image_session_is_ordered_verified_and_consumed_once() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("session-menu-cell", "0.1.0", ElmKind::Extension),
        ),
        (
            ElmEkiBlockKind::Menu,
            eki_menu_block("会话菜单", "来自分段镜像会话", "session/menu"),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);

    let begin = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(
            ElmMgrCallKind::BeginImageSession,
            &image_session_begin_payload(&image, 5_000),
        ),
    );
    assert_eq!(response_status(&begin), ELM_MGR_STATUS_OK);
    let begin_info = response_payload(&begin);
    assert_eq!(
        read_u32(begin_info, 4),
        elm_model::ElmImageSessionState::Uploading as u32
    );
    let session_id = read_u64(begin_info, 8);
    assert_ne!(session_id, 0);

    let invalid_write = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(
            ElmMgrCallKind::WriteImageSession,
            &image_session_write_payload(session_id, 1, &image[..1]),
        ),
    );
    assert_eq!(response_status(&invalid_write), ELM_MGR_STATUS_INVALID);

    let split = image.len() / 2;
    let first = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(
            ElmMgrCallKind::WriteImageSession,
            &image_session_write_payload(session_id, 0, &image[..split]),
        ),
    );
    assert_eq!(response_status(&first), ELM_MGR_STATUS_OK);
    assert_eq!(read_u64(response_payload(&first), 24), split as u64);
    let second = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(
            ElmMgrCallKind::WriteImageSession,
            &image_session_write_payload(session_id, split as u64, &image[split..]),
        ),
    );
    assert_eq!(response_status(&second), ELM_MGR_STATUS_OK);
    assert_eq!(read_u64(response_payload(&second), 24), image.len() as u64);

    let seal = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(
            ElmMgrCallKind::SealImageSession,
            &image_session_request_payload(session_id),
        ),
    );
    assert_eq!(response_status(&seal), ELM_MGR_STATUS_OK);
    assert_eq!(
        read_u32(response_payload(&seal), 4),
        elm_model::ElmImageSessionState::Sealed as u32
    );

    let projection =
        projection_session_payload(elm_model::ELM_EKI_PROJECTION_SOURCE_ID, session_id);
    let payload = ebi_source_payload(ElmEbiSourceKind::Projection, &projection);
    let load = dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&load), ELM_MGR_STATUS_OK);
    assert_eq!(
        read_i32(response_payload(&load), 8),
        ElmEbiLoadStatus::Ok as i32
    );

    let consumed = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(
            ElmMgrCallKind::QueryImageSession,
            &image_session_request_payload(session_id),
        ),
    );
    assert_eq!(response_status(&consumed), ELM_MGR_STATUS_NOT_FOUND);
}

#[ktest]
fn elm_mgr_loads_projection_source_from_registered_provider() {
    const TEST_PROJECTION_PROVIDER: u64 = 0x454c_4d50_524f_4a31;

    let _ = super::source::register_projection_source(
        TEST_PROJECTION_PROVIDER,
        test_projection_source_provider,
    );
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("projection-menu-cell", "0.1.0", ElmKind::Extension),
        ),
        (
            ElmEkiBlockKind::Menu,
            eki_menu_block("投影菜单", "来自 Projection Source", "projection/menu"),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let projection = projection_source_payload(TEST_PROJECTION_PROVIDER, &image);
    let payload = ebi_source_payload(ElmEbiSourceKind::Projection, &projection);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::Ok as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Active));
    assert_eq!(core.menu_items().len(), 2);
}

#[ktest]
fn elm_mgr_quarantines_faulting_native_eki_hooks() {
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
    let payload = eki_source_payload(&image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::RuntimeRejected as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Quarantined));
    assert_eq!(read_u32(load, 16), ELM_LIFECYCLE_REASON_HOOK_FAILED);
    let cell_id = read_u64(load, 0);
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(cell_id))
        .unwrap();
    assert_eq!(cell.native_segment_count, 1);
    assert_eq!(cell.native_import_count, 0);
    assert_eq!(cell.native_export_count, 0);
    assert_eq!(cell.parent, Some(ELM_MGR_ID));
    assert_eq!(cell.ebi_source, ElmEbiSourceKind::Projection);
    assert!(cell.lifecycle_hooks_declared);
    assert!(!cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);
    assert!(cell.isolated);
    assert_eq!(cell.native_faults, 1);
}

#[ktest]
fn elm_mgr_quarantines_entry_image_with_faulting_initialize_hook() {
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
    let payload = eki_source_payload(&image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::RuntimeRejected as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Quarantined));
    assert_eq!(read_u32(load, 16), ELM_LIFECYCLE_REASON_HOOK_FAILED);
    let cell_id = read_u64(load, 0);
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Quarantined);
    assert!(!cell.lifecycle_initialized);
    assert!(cell.isolated);
    assert_eq!(cell.native_faults, 1);
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
fn elm_native_entry_rejects_guard_abort() {
    let result = super::native::test_call_native_entry(
        test_native_entry_requests_abort as usize,
        ElmId(7),
        Some(ELM_MGR_ID),
        Generation(3),
        ElmState::Active,
    );

    assert!(result.is_err());
}

#[ktest]
fn elm_native_panic_uses_controlled_recovery_exit() {
    let cell = ElmId(0x7001);
    let result = super::native::test_call_native_entry(
        test_native_entry_panics as usize,
        cell,
        Some(ELM_MGR_ID),
        Generation(3),
        ElmState::Active,
    );

    assert!(result.is_err());
    let snapshot = general::elm_guard::last_fault_snapshot().expect("native panic snapshot");
    assert_eq!(snapshot.cell, cell.0);
    assert_eq!(snapshot.reason, general::elm_guard::ELM_GUARD_ABORT_PANIC);
    assert_eq!(snapshot.return_pc, arch::elm_native_recovery_address());
    assert_ne!(snapshot.return_sp, 0);
}

#[ktest]
fn elm_native_timeout_forces_controlled_exit() {
    let cell = ElmId(0x7002);
    let result = super::native::test_call_native_entry(
        test_native_entry_spins as usize,
        cell,
        Some(ELM_MGR_ID),
        Generation(3),
        ElmState::Active,
    );

    assert!(result.is_err());
    let snapshot = general::elm_guard::last_fault_snapshot().expect("native timeout snapshot");
    assert_eq!(snapshot.cell, cell.0);
    assert_eq!(snapshot.reason, general::elm_guard::ELM_GUARD_ABORT_TIMEOUT);
    assert_eq!(snapshot.return_pc, arch::elm_native_recovery_address());
    assert_ne!(snapshot.return_sp, 0);
}

#[ktest]
fn elm_native_nested_fault_uses_controlled_recovery_exit() {
    let result = super::native::test_call_native_entry(
        test_native_entry_nested_fault as usize,
        ElmId(7),
        Some(ELM_MGR_ID),
        Generation(3),
        ElmState::Active,
    );

    assert!(result.is_err());
    let snapshot = general::elm_guard::last_fault_snapshot().expect("nested native fault");
    assert_eq!(snapshot.cell, 7);
    assert_eq!(snapshot.phase, general::elm_guard::ELM_GUARD_PHASE_ENTRY);
    assert_eq!(snapshot.return_pc, arch::elm_native_recovery_address());
    assert_ne!(snapshot.pc, snapshot.return_pc);
    assert_ne!(snapshot.return_sp, 0);
    assert_eq!(snapshot.return_sp & 0xf, 0);
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
    let payload = eki_source_payload(&image);

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
fn elm_mgr_replace_cell_rejects_malformed_projection_source() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let request =
        ElmReplaceCellRequestV1::new(ELM_MGR_ID.0, ElmEbiSourceKind::Projection as u16, 0);
    let payload = replace_cell_payload(&request, &[]);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::ReplaceCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);
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
    let original_payload = eki_source_payload(&original_image);
    let load = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::LoadCell, &original_payload),
    );
    assert_eq!(response_status(&load), ELM_MGR_STATUS_OK);
    let load_payload = response_payload(&load);
    assert_eq!(read_i32(load_payload, 8), ElmEbiLoadStatus::Ok as i32);
    let cell_id = read_u64(load_payload, 0);

    let replacement_image = eki_image(&[
        (
            ElmEkiBlockKind::Manifest,
            eki_manifest_block("replace-target", "0.1.1", ElmKind::Service),
        ),
        (ElmEkiBlockKind::LifecycleHooks, eki_lifecycle_hooks_block()),
    ]);
    let payload = eki_replace_payload(ElmId(cell_id), &replacement_image, 0);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::ReplaceCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let replace = response_payload(&response);
    assert_eq!(read_i32(replace, 8), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(replace, 12), state_code(ElmState::Active));
    assert_eq!(read_u64(replace, 16), 2);
    assert_eq!(read_u64(replace, 32), 0);
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
    let payload = eki_source_payload(&image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::Ok as i32);
    assert_eq!(read_u32(load, 12), state_code(ElmState::Active));
    let cell_id = read_u64(load, 0);
    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Active);
    assert!(cell.lifecycle_hooks_declared);
    assert!(cell.lifecycle_initialized);
    assert!(!cell.lifecycle_finalized);

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
            .windows("eki.provider@1".len())
            .any(|window| window == b"eki.provider@1")
    );

    let detach_plan = core.preflight_lifecycle(elm_model::ElmLifecyclePlanRequest::new(
        cell_id,
        elm_model::ElmLifecycleAction::Detach,
    ));
    assert_eq!(
        detach_plan.blockers,
        0,
        "detach preflight blocked: status={} children={} dependents={} extensions={}",
        detach_plan.status,
        detach_plan.affected_children,
        detach_plan.affected_dependents,
        detach_plan.affected_extensions,
    );
    let resources = super::resource_accounting::snapshot(ElmId(cell_id), sched::now_ns_public());
    assert_eq!(
        (
            resources.dynamic_alloc_bytes,
            resources.native_stack_bytes,
            resources.active_native_calls
        ),
        (0, 0, 0),
        "detach resource accounting is not idle",
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
    let payload = eki_source_payload(&image);

    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::LoadCell, &payload));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let load = response_payload(&response);
    assert_eq!(read_u64(load, 0), 0);
    assert_eq!(read_i32(load, 8), ElmEbiLoadStatus::RuntimeRejected as i32);
    assert_eq!(core.cells().len(), 2);
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
fn elm_declarative_activation_failure_rolls_back_partial_providers() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let unit = ElmEbiUnit::new(
        manifest("elm-provider-rollback", ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_lifecycle_hooks(lifecycle_hooks())
    .with_provider_port(
        ElmEbiProviderPortDecl::new(
            "rollback.provider.first@1",
            ElmPortAccessPolicy::Public,
            FlowDirection::Control,
            FlowMode::Shared,
            0,
        )
        .unwrap(),
    )
    .with_provider_port(
        ElmEbiProviderPortDecl::new(
            "rollback.provider.second@1",
            ElmPortAccessPolicy::Public,
            FlowDirection::Control,
            FlowMode::Shared,
            0,
        )
        .unwrap(),
    );

    let response =
        core.load_ebi_unit_under_parent(unit, ElmEbiArch::Any, ELM_MGR_ID, delegated_budget(1));
    assert_eq!(response.status, ElmEbiLoadStatus::RuntimeRejected as i32);
    let id = ElmId(response.cell_id);
    assert_eq!(response.final_state, state_code(ElmState::Quarantined));
    assert_eq!(core.cell_resource_usage(id).provider_ports, 0);
    let providers = core.provider_ports_bytes();
    assert!(
        !providers
            .windows("rollback.provider.first@1".len())
            .any(|window| window == b"rollback.provider.first@1")
    );
    assert!(
        !providers
            .windows("rollback.provider.second@1".len())
            .any(|window| window == b"rollback.provider.second@1")
    );
}

#[ktest]
fn elm_subsystem_provider_specs_are_registered_and_invokable() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    assert_eq!(register_vfs_provider_specs(&mut core), 3);
    assert_eq!(register_vfs_provider_specs(&mut core), 0);

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
    assert_eq!(response.reply.status, ELM_CALL_STATUS_OK);
    assert_eq!(
        response.reply.payload_len as usize,
        vfs::elm::ELM_VFS_LOOKUP_REPLY_FIXED_LEN
    );
    assert_eq!(
        read_i32(&response.reply.payload, 4),
        errno::Errno::ESUCCESS.as_i32()
    );

    let stats = provider_stats_by_port(&mut core, port_id).unwrap();
    assert_eq!(stats.1, 1);
    assert_eq!(stats.2, 0);
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
    assert_eq!(stats.1, 1);
    assert_eq!(stats.2, 1);
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
    register_device_provider_specs(&mut core);

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
    register_device_provider_specs(&mut core);

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
    assert_eq!(core.drain_provider_revoke_notifications_for_test(), 1);
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
    assert_eq!(core.drain_provider_revoke_notifications_for_test(), 1);
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
    register_vfs_provider_specs(&mut core);

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

    let api =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryApiRegistry, &[]));
    assert_eq!(response_status(&api), ELM_MGR_STATUS_OK);
    let api_payload = response_payload(&api);
    assert!(api_payload.len() >= core::mem::size_of::<ElmMgrApiRegistryHeader>());
    let api_record_size = read_u16(api_payload, 2) as usize;
    let api_record_count = read_u32(api_payload, 4) as usize;
    assert_eq!(
        api_record_size,
        core::mem::size_of::<elm_model::ElmMgrApiDescriptor>()
    );
    assert_eq!(api_record_count, ElmMgrCallKind::QueryTrustState as usize);
    assert_eq!(
        api_payload.len(),
        core::mem::size_of::<ElmMgrApiRegistryHeader>() + api_record_count * api_record_size
    );
    for index in 0..api_record_count {
        let offset = core::mem::size_of::<ElmMgrApiRegistryHeader>() + index * api_record_size;
        let expected = (index + 1) as u64;
        assert_eq!(read_u64(api_payload, offset), expected);
        assert_eq!(u64::from(read_u32(api_payload, offset + 24)), expected);
    }

    let native_caps = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryNativeCapabilities, &[]),
    );
    assert_eq!(response_status(&native_caps), ELM_MGR_STATUS_OK);
    let native_caps_payload = response_payload(&native_caps);
    let native_cap_record_size = read_u16(native_caps_payload, 2) as usize;
    let native_cap_record_count = read_u32(native_caps_payload, 4) as usize;
    assert_eq!(
        native_cap_record_size,
        core::mem::size_of::<elm_model::ElmNativeCapabilityRecord>()
    );
    assert_eq!(native_cap_record_count, 2);
    assert_eq!(
        native_caps_payload.len(),
        core::mem::size_of::<ElmNativeCapabilityHeader>()
            + native_cap_record_count * native_cap_record_size
    );
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

#[ktest]
fn elm_guard_does_not_recover_without_controlled_exit() {
    let guard =
        general::elm_guard::ElmGuard::enter(0x1111, general::elm_guard::ELM_GUARD_PHASE_HOOK, 0)
            .expect("guard enter");

    assert!(general::elm_guard::try_recover_kernel_fault(0x10, 0x20, 0x30).is_none());
    assert!(!guard.aborted());
}

#[ktest]
fn elm_guard_nested_recovery_consumes_only_top_frame() {
    let outer =
        general::elm_guard::ElmGuard::enter(0x1111, general::elm_guard::ELM_GUARD_PHASE_HOOK, 0)
            .expect("outer guard");
    assert!(outer.configure_native_bounds(0x40, 0x100, 0x40, 0x200, 0x1000, 0x5000, &[]));
    let _outer_domain = outer
        .enter_domain(general::elm_guard::ElmExecutionDomain::ElmCode)
        .expect("outer domain");
    assert!(general::elm_guard::arm_current_recovery(0x1000, 0x2000));
    assert!(!general::elm_guard::arm_current_recovery(0x3000, 0x4000));

    {
        let inner = general::elm_guard::ElmGuard::enter(
            0x2222,
            general::elm_guard::ELM_GUARD_PHASE_PROVIDER_CALL,
            0,
        )
        .expect("inner guard");
        assert!(inner.configure_native_bounds(0x40, 0x100, 0x40, 0x200, 0x1000, 0x5000, &[]));
        let _inner_domain = inner
            .enter_domain(general::elm_guard::ElmExecutionDomain::ElmCode)
            .expect("inner domain");
        assert!(general::elm_guard::arm_current_recovery(0x3000, 0x4000));
        let recovery =
            general::elm_guard::try_recover_kernel_fault(0x50, 0x60, 0x70).expect("inner recovery");
        assert_eq!(recovery.cell, 0x2222);
        assert_eq!(recovery.return_pc, 0x3000);
        assert_eq!(recovery.return_sp, 0x4000);
        assert!(inner.aborted());
    }

    let recovery =
        general::elm_guard::try_recover_kernel_fault(0x80, 0x90, 0xa0).expect("outer recovery");
    assert_eq!(recovery.cell, 0x1111);
    assert_eq!(recovery.return_pc, 0x1000);
    assert_eq!(recovery.return_sp, 0x2000);
    assert!(outer.aborted());
}

#[ktest]
fn elm_guard_fault_recovery_is_observable() {
    let guard = general::elm_guard::ElmGuard::enter(
        0x1234,
        general::elm_guard::ELM_GUARD_PHASE_PROVIDER_CALL,
        0,
    )
    .expect("guard enter");
    assert!(guard.configure_native_bounds(0x1000, 0x1800, 0x1000, 0x2000, 0x3000, 0x5000, &[],));
    let domain = guard
        .enter_domain(general::elm_guard::ElmExecutionDomain::ElmCode)
        .expect("elm domain");
    assert!(general::elm_guard::arm_current_recovery(0x3000, 0x4000));
    let recovery =
        general::elm_guard::try_recover_kernel_fault(0x1000, 0x2000, 0xf).expect("recovery");
    assert_eq!(recovery.cell, 0x1234);
    assert_eq!(
        recovery.phase,
        general::elm_guard::ELM_GUARD_PHASE_PROVIDER_CALL
    );
    assert_eq!(recovery.return_pc, 0x3000);
    assert_eq!(recovery.return_sp, 0x4000);
    assert!(guard.aborted());

    let snapshot = general::elm_guard::last_fault_snapshot().expect("fault snapshot");
    assert_eq!(snapshot.cell, 0x1234);
    assert_eq!(
        snapshot.phase,
        general::elm_guard::ELM_GUARD_PHASE_PROVIDER_CALL
    );
    assert_eq!(snapshot.pc, 0x1000);
    assert_eq!(snapshot.addr, 0x2000);
    assert_eq!(snapshot.code, 0xf);
    assert_eq!(snapshot.return_pc, 0x3000);
    assert_eq!(snapshot.return_sp, 0x4000);
    drop(domain);
    drop(guard);

    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let response =
        dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryFaultDump, &[]));
    assert_eq!(response_status(&response), ELM_MGR_STATUS_OK);
    let payload = response_payload(&response);
    assert_eq!(read_u16(payload, 0), ELM_CTL_ABI_VERSION);
    let record_size = usize::from(read_u16(payload, 2));
    let record_count = read_u32(payload, 4) as usize;
    assert!(record_count >= 1);
    assert_eq!(read_u64(payload, 16), snapshot.sequence);
    let latest = 24 + (record_count - 1) * record_size;
    assert_eq!(read_u64(payload, latest), snapshot.sequence);
    assert_eq!(read_u64(payload, latest + 8), 0x1234);
    assert_eq!(read_u64(payload, latest + 48), 0x4000);
}

#[ktest]
fn elm_projection_source_shadow_switch_is_atomic() {
    const SOURCE_ID: u64 = 0x5453_5450_524f_4a32;
    let owner = ElmId(0x5453_5401);
    let old_generation = Generation(7);
    let new_generation = Generation(8);

    assert!(
        super::source::register_projection_source_owned(
            SOURCE_ID,
            owner,
            old_generation,
            test_projection_source_provider,
        )
        .is_ok()
    );
    assert!(
        super::source::register_projection_source_owned(
            SOURCE_ID,
            owner,
            new_generation,
            test_projection_source_provider,
        )
        .is_ok()
    );
    let before = super::source::projection_source_snapshots().unwrap();
    assert!(before.iter().any(|source| {
        source.id == SOURCE_ID && source.generation == old_generation && source.active
    }));
    assert!(before.iter().any(|source| {
        source.id == SOURCE_ID && source.generation == new_generation && !source.active
    }));

    assert_eq!(
        super::source::suspend_projection_sources(owner, old_generation),
        Ok(1)
    );
    assert!(super::source::projection_source_generation_ready(
        owner,
        old_generation,
        new_generation,
    ));
    assert_eq!(
        super::source::commit_projection_source_generation(
            owner,
            old_generation,
            new_generation,
            || true,
        ),
        Ok((1, true))
    );
    let after = super::source::projection_source_snapshots().unwrap();
    assert!(
        !after
            .iter()
            .any(|source| { source.id == SOURCE_ID && source.generation == old_generation })
    );
    assert!(after.iter().any(|source| {
        source.id == SOURCE_ID && source.generation == new_generation && source.active
    }));
    assert_eq!(
        super::source::unregister_projection_source(SOURCE_ID, owner, old_generation),
        Err(super::source::ProjectionSourceRegistryError::StaleGeneration)
    );
    assert!(super::source::unregister_projection_source(SOURCE_ID, owner, new_generation).is_ok());
}

#[ktest]
fn elm_projection_source_busy_retire_preserves_active_state() {
    assert!(
        super::source::register_projection_source_owned(
            BUSY_PROJECTION_SOURCE_ID,
            BUSY_PROJECTION_SOURCE_OWNER,
            BUSY_PROJECTION_SOURCE_GENERATION,
            test_busy_projection_source_provider,
        )
        .is_ok()
    );
    assert_eq!(
        super::source::project_ebi_image(
            BUSY_PROJECTION_SOURCE_ID,
            &elm_model::ElmSliceImageReader::new(&[]),
            ElmEbiArch::Any,
        ),
        Err(ElmEbiLoadStatus::RuntimeRejected)
    );
    let snapshots = super::source::projection_source_snapshots().unwrap();
    let source = snapshots
        .iter()
        .find(|source| source.id == BUSY_PROJECTION_SOURCE_ID)
        .expect("测试 Projection Source 必须仍然存在");
    assert!(source.active);
    assert!(!source.suspended);
    assert!(!source.retiring);
    assert_eq!(source.active_refs, 0);
    assert!(
        super::source::unregister_projection_source(
            BUSY_PROJECTION_SOURCE_ID,
            BUSY_PROJECTION_SOURCE_OWNER,
            BUSY_PROJECTION_SOURCE_GENERATION,
        )
        .is_ok()
    );
}

#[ktest]
fn elm_journal_codec_rejects_tampering() {
    assert!(super::journal::test_codec_and_hash_chain());
    assert!(super::journal::test_tamper_is_rejected());
}

#[ktest]
fn elm_journal_backend_failure_semantics_are_stable() {
    assert!(super::journal::test_optional_and_required_backend_failures());
    assert!(super::journal::test_backend_capacity_and_replay_failures());
    assert!(super::journal::test_backend_read_and_sequence_exhaustion());
    assert!(super::journal::test_trust_epoch_replay_and_rollback_rejection());
}

#[ktest]
fn elm_resource_health_ignores_idle_foreign_core_history() {
    let cell = ElmId(0xffff_ffff_ffff_ff01);
    assert!(super::resource_accounting::register_cell(
        cell,
        ElmResourceBudget::DEFAULT,
    ));
    assert_eq!(
        super::resource_accounting::first_orphaned_cell(|candidate| candidate != cell),
        None
    );

    let permit = super::resource_accounting::begin_native_call(cell, 4096, 0, 1).unwrap();
    assert!(!super::resource_accounting::register_cell(
        cell,
        ElmResourceBudget::DEFAULT,
    ));
    assert_eq!(
        super::resource_accounting::first_orphaned_cell(|candidate| candidate != cell),
        Some(cell)
    );
    let _ = permit.finish(2);
    assert!(super::resource_accounting::retire_cell(cell));
}

#[ktest]
fn elm_monotonic_identifiers_never_wrap_or_reuse() {
    assert!(ElmCore::test_monotonic_exhaustion());
}

#[ktest]
fn elm_native_replace_selects_highest_unique_managed_export() {
    assert!(ElmCore::test_native_replace_selection_policy());
}

#[ktest]
fn elm_native_import_staging_is_transactional() {
    assert!(ElmCore::test_native_import_staging_transaction());
}

#[ktest]
fn elm_native_replace_recovers_only_a_healthy_old_generation() {
    assert!(ElmCore::test_native_replace_old_generation_recovery());
}
