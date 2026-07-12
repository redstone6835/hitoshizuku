//! ELM 核心全局状态。

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use elm_model::{
    ActionId, BindingGraph, BindingId, ELM_ACTION_OPCODE_INVOKE, ELM_API_CURRENT_VERSION,
    ELM_API_FEATURES_V1, ELM_API_MANAGEMENT_IDENTIFIER, ELM_API_NAMESPACE_FLAG_MANAGEMENT,
    ELM_API_NAMESPACE_FLAG_PUBLIC, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
    ELM_API_ROOT_MAGIC, ELM_API_RUNTIME_IDENTIFIER, ELM_API_STATUS_BUFFER_TOO_SMALL,
    ELM_API_STATUS_INVALID, ELM_API_STATUS_NOT_FOUND, ELM_API_STATUS_OK, ELM_API_STATUS_PERMISSION,
    ELM_API_STATUS_UNSUPPORTED, ELM_CALL_STATUS_BUSY, ELM_CALL_STATUS_INVALID,
    ELM_CALL_STATUS_NOT_FOUND, ELM_CALL_STATUS_OK, ELM_CALL_STATUS_PROVIDER_FAULT,
    ELM_CALL_STATUS_UNSUPPORTED, ELM_EBI_EXPORT_FLAG_MANAGED, ELM_EBI_IMPORT_FLAG_MANAGED,
    ELM_EKI_BUILTIN_ID, ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION,
    ELM_EXTENSION_DISPATCH_FLAGS_MASK, ELM_HEALTH_CHECK_AUDITS, ELM_HEALTH_CHECK_BINDINGS,
    ELM_HEALTH_CHECK_CELLS, ELM_HEALTH_CHECK_EVENTS, ELM_HEALTH_CHECK_EXECUTIONS,
    ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_JOURNAL, ELM_HEALTH_CHECK_MENU,
    ELM_HEALTH_CHECK_NATIVE_CAPABILITIES, ELM_HEALTH_CHECK_PORTS,
    ELM_HEALTH_CHECK_PROJECTION_SOURCES, ELM_HEALTH_CHECK_PROVIDERS, ELM_HEALTH_CHECK_RESOURCES,
    ELM_HEALTH_CHECK_RUNTIME_PORTS, ELM_HEALTH_CHECK_SEQUENCES, ELM_HEALTH_CHECK_TODO_REGISTRY,
    ELM_HEALTH_CHECK_TRUST, ELM_HEALTH_DETAIL_CONTRACT_INVALID,
    ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED, ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT, ELM_HEALTH_DETAIL_GRAPH_INVALID,
    ELM_HEALTH_DETAIL_KIND_MISMATCH, ELM_HEALTH_DETAIL_MISSING_OBJECT,
    ELM_HEALTH_DETAIL_PERSISTENCE_FAILED, ELM_HEALTH_DETAIL_RESOURCE_LEAK,
    ELM_HEALTH_DETAIL_SEQUENCE_INVALID, ELM_HEALTH_DETAIL_STATE_INVALID,
    ELM_HEALTH_DETAIL_STUCK_REFERENCE, ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT, ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
    ELM_LIFECYCLE_REASON_HOOK_FAILED, ELM_LIFECYCLE_REASON_INVALID_STATE,
    ELM_LIFECYCLE_REASON_LEASE_BUSY, ELM_LIFECYCLE_REASON_NATIVE_TODO, ELM_LIFECYCLE_REASON_NONE,
    ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE, ELM_MENU_FLAG_REQUIRES_SYS_ADMIN, ELM_MENU_FLAG_TODO,
    ELM_MGR_ACTION_BIND, ELM_MGR_ACTION_EVENT_READ, ELM_MGR_ACTION_EVENT_SUBSCRIBE,
    ELM_MGR_ACTION_EVENT_UNSUBSCRIBE, ELM_MGR_ACTION_EXTENSION_ATTACH,
    ELM_MGR_ACTION_EXTENSION_DETACH, ELM_MGR_ACTION_EXTENSION_DISPATCH,
    ELM_MGR_ACTION_POLICY_UPDATE, ELM_MGR_ACTION_PROVIDER_ASYNC, ELM_MGR_ACTION_PROVIDER_INVOKE,
    ELM_MGR_ACTION_PROVIDER_QUERY, ELM_MGR_ACTION_PROVIDER_REGISTER,
    ELM_MGR_ACTION_PROVIDER_UNREGISTER, ELM_MGR_ACTION_RESOURCE_UPDATE,
    ELM_MGR_ACTION_RUNTIME_EVENT_ACK, ELM_MGR_ACTION_RUNTIME_EVENT_READ,
    ELM_MGR_ACTION_RUNTIME_LOG, ELM_MGR_ACTION_UNBIND, ELM_MGR_BUILTIN_ID, ELM_MGR_MAX_PAYLOAD,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_PERMISSION, ELM_MGR_STATUS_TODO, ELM_MGR_STATUS_UNSUPPORTED,
    ELM_MIXIN_REPLY_CONTINUE, ELM_MIXIN_REPLY_DENY, ELM_MIXIN_REPLY_FLAGS_MASK,
    ELM_MIXIN_REPLY_REPLACE, ELM_MIXIN_REPLY_STOP, ELM_POLICY_BLOCK_ABI_FINGERPRINT,
    ELM_POLICY_BLOCK_BINDING_NOT_FOUND, ELM_POLICY_BLOCK_BINDING_PROTECTED,
    ELM_POLICY_BLOCK_BUILTIN_PROTECTED, ELM_POLICY_BLOCK_CALLER_NOT_FOUND,
    ELM_POLICY_BLOCK_CALLER_STALE, ELM_POLICY_BLOCK_CELL_NOT_FOUND,
    ELM_POLICY_BLOCK_CONTRACT_MISMATCH, ELM_POLICY_BLOCK_DUPLICATE_BINDING,
    ELM_POLICY_BLOCK_EXTENSION_DUPLICATE, ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND,
    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT, ELM_POLICY_BLOCK_HAS_CHILDREN,
    ELM_POLICY_BLOCK_HAS_DEPENDENTS, ELM_POLICY_BLOCK_HAS_EXTENSIONS,
    ELM_POLICY_BLOCK_INVALID_STATE, ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE,
    ELM_POLICY_BLOCK_LEASE_BUSY, ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
    ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE, ELM_POLICY_BLOCK_NATIVE_TODO,
    ELM_POLICY_BLOCK_POLICY_ESCALATION, ELM_POLICY_BLOCK_PORT_NOT_FOUND,
    ELM_POLICY_BLOCK_PORT_TODO, ELM_POLICY_BLOCK_PROVIDER_BUSY,
    ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND, ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL,
    ELM_POLICY_BLOCK_RESOURCE_QUOTA, ELM_POLICY_BLOCK_ROLLBACK_REJECTED,
    ELM_POLICY_BLOCK_SCOPE_DENIED, ELM_POLICY_BLOCK_UNTRUSTED_IMAGE,
    ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS, ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS,
    ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS, ELM_PROVIDER_ASYNC_QUEUE_LIMIT, ELM_PROVIDER_FLAG_DYNAMIC,
    ELM_PROVIDER_FLAG_KERNEL_BACKEND, ELM_PROVIDER_FLAG_NATIVE_BACKEND,
    ELM_PROVIDER_FLAG_TODO_BACKEND, ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK,
    ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE, ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK,
    ELM_REPLACE_MIGRATION_STATE_MAX, ELM_RUNTIME_LOG_MESSAGE_LEN, ELM_TODO_FLAG_ACTIVE,
    ELM_TODO_FLAG_STATIC, ELM_TODO_KIND_FRAMEWORK, ELM_TODO_KIND_NATIVE, ELM_TODO_KIND_PROVIDER,
    ELM_TODO_KIND_RUNTIME, ELM_TODO_KIND_SOURCE, ElmActionInvokeReply, ElmActionInvokeRequest,
    ElmApiContextV1, ElmApiNamespaceDescriptorV1, ElmApiNamespaceV1, ElmApiRootV1, ElmCallFrame,
    ElmContext, ElmCoreHealthHeader, ElmCoreHealthRecord, ElmCoreInfo, ElmCurrentContext,
    ElmEbiArch, ElmEbiExtensionPointDecl, ElmEbiImage, ElmEbiLifecycleHooks, ElmEbiLoadStatus,
    ElmEbiProviderPortDecl, ElmEbiSourceKind, ElmEbiTarget, ElmEbiUnit, ElmError, ElmEventRecord,
    ElmEventSequence, ElmExtensionAttachRequest, ElmExtensionAttachResponse,
    ElmExtensionDetachRequest, ElmExtensionDetachResponse, ElmExtensionDispatchRequest,
    ElmExtensionDispatchResponse, ElmExtensionSnapshotHeader, ElmExtensionSnapshotRecord,
    ElmFaultDumpHeader, ElmFaultDumpRecord, ElmId, ElmKind, ElmLifecycleAction, ElmLifecyclePhase,
    ElmLifecyclePlanRequest, ElmLifecyclePlanResponse, ElmLifecycleResponse, ElmLoadCellResponse,
    ElmManagementApiV1, ElmManifest, ElmMenuItemKind, ElmMgrApiDescriptor, ElmMgrApiRegistryHeader,
    ElmMgrAuditHeader, ElmMgrAuditRecord, ElmMgrCallHeader, ElmMgrCallKind,
    ElmMgrEventSubscribeRequest, ElmMgrEventSubscribeResponse, ElmMgrEventSubscriptionHeader,
    ElmMgrEventSubscriptionRecord, ElmMgrEventUnsubscribeRequest, ElmMgrEventUnsubscribeResponse,
    ElmMgrPolicyInfo, ElmMgrRelationKind, ElmMgrRelationRecord, ElmMgrSubscribedEventReadHeader,
    ElmMgrSubscribedEventReadRequest, ElmMgrTopologyHeader, ElmMixinMode, ElmName,
    ElmNativeCapabilityHeader, ElmNativeCapabilityRecord, ElmNexusBindPlanResponse,
    ElmNexusBindRequest, ElmNexusBindingRecord, ElmNexusBindingSnapshotHeader,
    ElmNexusUnbindRequest, ElmPortAccessPolicy, ElmPrincipal, ElmPrincipalKind,
    ElmProviderAsyncCancelRequest, ElmProviderAsyncCancelResponse, ElmProviderAsyncPollRequest,
    ElmProviderAsyncPollResponse, ElmProviderAsyncState, ElmProviderAsyncSubmitRequest,
    ElmProviderAsyncSubmitResponse, ElmProviderInvokeRequest, ElmProviderInvokeResponse,
    ElmProviderPortRecord, ElmProviderPortRegisterRequest, ElmProviderPortRegisterResponse,
    ElmProviderPortStatsHeader, ElmProviderPortStatsRecord, ElmProviderPortUnregisterRequest,
    ElmProviderQueueStatsHeader, ElmProviderQueueStatsRecord, ElmProviderSnapshotHeader,
    ElmProviderSnapshotRequest, ElmReplaceCellResponseV1, ElmReplyFrame, ElmResourceBudget,
    ElmResourceBudgetRequest, ElmResourceBudgetResponse, ElmResourceBudgetUpdateRequest,
    ElmResourceKind, ElmResourceUsage, ElmResult, ElmRuntimeApiV1, ElmRuntimeEventRequest,
    ElmRuntimeEventResponse, ElmRuntimeLogRequest, ElmRuntimeLogResponse,
    ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord, ElmRuntimeTraceHeader,
    ElmRuntimeTraceRecord, ElmState, ElmTodoRegistryHeader, ElmTodoRegistryRecord,
    ElmTrustAcceptance, ElmTrustAnchor, ElmTrustError, ElmTrustRuntimeInfoV1, ElmTrustStore,
    ElmVersion, ExtensionEdge, FlowContract, FlowDirection, FlowMode, Generation, LeaseId,
    LeaseKind, LeaseRegistry, LeaseRights, NexusOffer, PortId, ResourceLease, TopologyEventKind,
    builtin_port_descriptors, first_lifecycle_reason, kernel_api_manifest_v1, planned_final_state,
    state_code, status_from_blockers,
};
use elm_model::{
    ELM_AUDIT_AUTHORITY_ANCESTOR, ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER,
    ELM_AUDIT_AUTHORITY_KERNEL, ELM_AUDIT_AUTHORITY_MANAGER, ELM_AUDIT_AUTHORITY_SELF,
    ELM_AUDIT_AUTHORITY_USER_ADMIN, ELM_AUDIT_FLAG_AUTHORIZATION, ELM_AUDIT_FLAG_OPERATION,
    ELM_CELL_POLICY_ALLOW_ALL, ELM_CELL_POLICY_ALLOW_BIND, ELM_CELL_POLICY_ALLOW_EVENT,
    ELM_CELL_POLICY_ALLOW_EXTENSION, ELM_CELL_POLICY_ALLOW_LIFECYCLE,
    ELM_CELL_POLICY_ALLOW_MANAGEMENT, ELM_CELL_POLICY_ALLOW_NATIVE, ELM_CELL_POLICY_ALLOW_OBSERVE,
    ELM_CELL_POLICY_ALLOW_POLICY_UPDATE, ELM_CELL_POLICY_ALLOW_PROVIDER,
    ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE, ELM_CELL_POLICY_ALLOWED_ACTIONS_MASK,
    ELM_CELL_POLICY_FLAG_AUDIT_ALL, ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION,
    ELM_CELL_POLICY_FLAG_LOCKED, ELM_CELL_POLICY_FLAGS_MASK, ELM_EXTENSION_POLICY_ACCEPT,
    ELM_EXTENSION_POLICY_ALL, ELM_EXTENSION_POLICY_ATTACH, ELM_EXTENSION_POLICY_DETACH,
    ELM_EXTENSION_POLICY_DISPATCH, ELM_EXTENSION_POLICY_MIXIN_PATCH, ELM_MGR_API_FLAG_STABLE,
    ELM_MGR_API_FLAG_SYSCALL, ELM_MGR_API_FLAG_SYSFS, ELM_MGR_API_KIND_CONTROL,
    ELM_MGR_API_KIND_EVENT, ELM_MGR_API_KIND_PROVIDER, ELM_MGR_API_KIND_SNAPSHOT,
    ELM_MGR_EVENT_READ_ABSOLUTE_MAX_RECORDS, ELM_MGR_EVENT_READ_DEFAULT_MAX_RECORDS,
    ELM_MGR_EVENT_READ_FLAG_ADVANCE, ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED,
    ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD, ELM_NATIVE_CAPABILITY_KIND_EXPORT,
    ELM_NATIVE_CAPABILITY_KIND_IMPORT, ELM_NATIVE_POLICY_ALL, ELM_NATIVE_POLICY_EXECUTE,
    ELM_NATIVE_POLICY_EXPORT, ELM_NATIVE_POLICY_IMPORT, ELM_NATIVE_POLICY_MIXIN_PATCH,
    ELM_NATIVE_POLICY_REPLACE, ELM_POLICY_BLOCK_CAPABILITY_DENIED, ELM_PROVIDER_POLICY_ALL,
    ELM_PROVIDER_POLICY_ASYNC, ELM_PROVIDER_POLICY_INVOKE, ELM_PROVIDER_POLICY_REGISTER,
    ELM_PROVIDER_POLICY_SNAPSHOT, ELM_PROVIDER_POLICY_UNREGISTER, ELM_RESOURCE_POLICY_ALL,
    ELM_RESOURCE_POLICY_OWN, ELM_RESOURCE_POLICY_QUERY, ELM_RESOURCE_POLICY_UPDATE,
    ELM_RUNTIME_LOG_EXPORT_CONTRACT, ELM_RUNTIME_LOG_EXPORT_NAME, ELM_RUNTIME_LOG_EXPORT_VERSION,
    ELM_RUNTIME_TRACE_KIND_JOURNAL, ELM_RUNTIME_TRACE_KIND_LIFECYCLE,
    ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH, ELM_RUNTIME_TRACE_KIND_POLICY,
    ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL, ELM_RUNTIME_TRACE_KIND_REPLACE,
    ELM_RUNTIME_TRACE_KIND_RESOURCE, ELM_RUST_ABI_TARGET_FEATURE_FLOAT,
    ELM_RUST_ABI_TARGET_FEATURE_SIMD, ELM_RUST_ABI_TARGET_FEATURE_VECTOR,
    ELM_TODO_REGISTRY_FLAG_TRUNCATED, ELM_TRUST_FLAG_ALLOW_UNSIGNED, ELM_TRUST_FLAG_SEALED,
    ELM_TRUST_FLAG_UNSIGNED_ACTIVE, ElmCellPolicyRequest, ElmCellPolicyV1, ElmKernelProviderRevoke,
    ElmKernelProviderSpec, ElmPanicStrategy, current_cell, current_context, sha256,
};
use sched::sync::Spinlock;

use super::menu::MenuItemRuntime;
use super::native::{LoadedElmImage, NativeExecutionBounds, NativeHookExecutor};
use super::ports::PortRuntime;

pub(crate) const ELM_MGR_ID: ElmId = ELM_MGR_BUILTIN_ID;
pub(crate) const ELM_EKI_ID: ElmId = ELM_EKI_BUILTIN_ID;
const ELM_CORE_LOG_PORT_ID: PortId = PortId(1);
const ELM_CORE_EVENT_PORT_ID: PortId = PortId(2);
const ELM_MGR_MENU_PORT_ID: PortId = PortId(3);
const ELM_MGR_ACTION_PORT_ID: PortId = PortId(4);
const ELM_CORE_LOG_CONTRACT: &str = "core.log@1";
const ELM_CORE_EVENT_CONTRACT: &str = "core.event@1";
const ELM_MGR_MENU_CONTRACT: &str = "mgr.menu.item@1";
const ELM_MGR_ACTION_CONTRACT: &str = "mgr.action.invoke@1";
const FIRST_DYNAMIC_CELL_ID: u64 = 100;
const FIRST_DYNAMIC_PORT_ID: u64 = 100;
const FIRST_KERNEL_PROVIDER_API_ID: u64 = 100;
const EVENT_RING_LIMIT: usize = 128;
const AUDIT_RING_LIMIT: usize = 128;
const TRACE_RING_LIMIT: usize = 128;
const FIRST_PROVIDER_TICKET_ID: u64 = 1;
const FIRST_MANAGED_IMPORT_HANDLE: u64 = 1;
const PROVIDER_RESULT_RING_LIMIT: usize = ELM_PROVIDER_ASYNC_QUEUE_LIMIT as usize;
const FIRST_EVENT_SUBSCRIPTION_ID: u64 = 1;
const EVENT_SUBSCRIPTION_LIMIT: usize = 64;

static CORE: Spinlock<ElmCore> = Spinlock::new(ElmCore::new());

static ELM_RUNTIME_API_V1: ElmRuntimeApiV1 = ElmRuntimeApiV1 {
    struct_size: core::mem::size_of::<ElmRuntimeApiV1>() as u32,
    abi_version: ELM_API_CURRENT_VERSION,
    reserved0: 0,
    features: ELM_API_FEATURES_V1,
    dispatch_mixin: elm_api_dispatch_mixin_v1,
    current_context: elm_api_current_context_v1,
    log: elm_runtime_log_v1,
    abort_current: elm_api_abort_current_v1,
    invoke_managed: elm_api_invoke_managed_v1,
};

static ELM_MANAGEMENT_API_V1: ElmManagementApiV1 = ElmManagementApiV1 {
    struct_size: core::mem::size_of::<ElmManagementApiV1>() as u32,
    abi_version: ELM_API_CURRENT_VERSION,
    reserved0: 0,
    dispatch: elm_api_management_dispatch_v1,
};

static ELM_API_ROOT_V1: ElmApiRootV1 = ElmApiRootV1 {
    magic: ELM_API_ROOT_MAGIC,
    struct_size: core::mem::size_of::<ElmApiRootV1>() as u32,
    abi_version: ELM_API_CURRENT_VERSION,
    selected_version: ELM_API_CURRENT_VERSION,
    features: ELM_API_FEATURES_V1,
    runtime_table: &ELM_RUNTIME_API_V1,
    runtime_table_size: core::mem::size_of::<ElmRuntimeApiV1>() as u32,
    reserved0: 0,
    query_namespace: elm_api_query_namespace_v1,
};

static ELM_RUNTIME_NAMESPACE_V1: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
    ELM_API_RUNTIME_IDENTIFIER,
    ELM_API_CURRENT_VERSION,
    ELM_API_NAMESPACE_FLAG_PUBLIC,
    ELM_API_FEATURES_V1,
    &ELM_RUNTIME_API_V1,
    [0; 32],
);

static ELM_MANAGEMENT_NAMESPACE_V1: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
    ELM_API_MANAGEMENT_IDENTIFIER,
    ELM_API_CURRENT_VERSION,
    ELM_API_NAMESPACE_FLAG_MANAGEMENT,
    ELM_CELL_POLICY_ALLOW_MANAGEMENT as u64,
    &ELM_MANAGEMENT_API_V1,
    [0; 32],
);

pub(crate) trait ElmLifecycleExecutor {
    fn on_initialize(&mut self, context: &mut ElmContext) -> ElmResult<()>;
    fn on_finalize(&mut self, context: &mut ElmContext) -> ElmResult<()>;
    fn on_quiesce(&mut self, _context: &mut ElmContext) -> ElmResult<()> {
        Ok(())
    }
    fn on_pause(&mut self, _context: &mut ElmContext) -> ElmResult<()> {
        Ok(())
    }
    fn on_resume(&mut self, _context: &mut ElmContext) -> ElmResult<()> {
        Ok(())
    }
    fn on_migrate_export(
        &mut self,
        _cell: ElmId,
        _old_generation: Generation,
        _new_generation: Generation,
        _buffer: &mut [u8],
    ) -> ElmResult<usize> {
        Err(ElmError::InvalidTransition)
    }
    fn on_migrate_import(
        &mut self,
        _cell: ElmId,
        _old_generation: Generation,
        _new_generation: Generation,
        _buffer: &mut [u8],
        _len: usize,
    ) -> ElmResult<()> {
        Err(ElmError::InvalidTransition)
    }
    fn on_migrate_abort(
        &mut self,
        _cell: ElmId,
        _old_generation: Generation,
        _new_generation: Generation,
        _buffer: &mut [u8],
        _len: usize,
    ) -> ElmResult<()> {
        Ok(())
    }
}

struct DeclarativeLifecycleExecutor;

impl ElmLifecycleExecutor for DeclarativeLifecycleExecutor {
    fn on_initialize(&mut self, _context: &mut ElmContext) -> ElmResult<()> {
        Ok(())
    }

    fn on_finalize(&mut self, _context: &mut ElmContext) -> ElmResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CellRuntime {
    pub id: ElmId,
    pub parent: Option<ElmId>,
    pub state: ElmState,
    pub kind: ElmKind,
    pub generation: Generation,
    pub name: String,
    pub ebi_source: ElmEbiSourceKind,
    pub ebi_arch: ElmEbiArch,
    pub ebi_status: ElmEbiLoadStatus,
    pub has_native_code: bool,
    pub native_segment_count: u16,
    pub native_import_count: u16,
    pub native_export_count: u16,
    pub elmapi_version: u16,
    pub lifecycle_hooks_declared: bool,
    pub lifecycle_executor_ready: bool,
    pub lifecycle_initialized: bool,
    pub lifecycle_finalized: bool,
    pub resource_budget: ElmResourceBudget,
    pub cell_policy: ElmCellPolicyV1,
    pub policy_epoch: u64,
    pub active_executions: u32,
    pub exclusive_execution: bool,
    pub native_faults: u16,
    pub isolated: bool,
    pub isolation_blocker: u64,
    pub trust_unsigned: bool,
    pub signer_key_id: [u8; 32],
    pub release_epoch: u64,
    pub owned_bindings: Vec<BindingId>,
    pub owned_menu_items: Vec<u64>,
}

#[derive(Debug, Clone)]
struct PreparedImageTrust {
    acceptance: Option<ElmTrustAcceptance>,
    acceptance_reserved: bool,
    unsigned: bool,
    signer_key_id: [u8; 32],
    release_epoch: u64,
}

impl PreparedImageTrust {
    const fn internal() -> Self {
        Self {
            acceptance: None,
            acceptance_reserved: false,
            unsigned: false,
            signer_key_id: [0; 32],
            release_epoch: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct KernelApiGrantRequest {
    requested: bool,
    authority: u32,
    authority_id: u64,
}

impl KernelApiGrantRequest {
    pub(crate) const fn none() -> Self {
        Self {
            requested: false,
            authority: 0,
            authority_id: 0,
        }
    }

    pub(crate) const fn from_authorization(
        requested: bool,
        authorization: ElmMgrAuthorization,
    ) -> Self {
        Self {
            requested: requested && authorization.allowed(),
            authority: authorization.authority,
            authority_id: authorization.authority_id,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ElmMgrAccessTarget {
    Global,
    Load(ElmId, ElmResourceBudget),
    Cell(ElmId),
    Cells(ElmId, ElmId),
    Port(PortId),
    Binding(BindingId),
    Subscription(u64),
    ProviderTicket(u64),
    PolicyUpdate(ElmCellPolicyV1),
    ResourceUpdate(ElmResourceBudgetUpdateRequest),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ElmMgrAuthorization {
    pub principal: ElmPrincipal,
    pub authority: u32,
    pub authority_id: u64,
    pub actor_generation: u64,
    pub policy_epoch: u64,
    pub subject_id: u64,
    pub blockers: u64,
}

impl ElmMgrAuthorization {
    pub const fn allowed(&self) -> bool {
        self.blockers == 0
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ElmMgrAuthorizationExecution {
    actor: Option<CellExecutionToken>,
    subject: Option<CellExecutionToken>,
}

#[derive(Debug, Clone)]
struct RuntimePortBinding {
    binding: BindingId,
    cell: ElmId,
    port: PortId,
    lease: LeaseId,
    cursor: u64,
    submitted_logs: u64,
    delivered_events: u64,
    dropped_events: u64,
}

#[derive(Debug, Clone, Copy)]
enum ProviderBackend {
    Kernel(KernelProviderKind),
    KernelOps(&'static ElmKernelProviderSpec),
    ElmNative(NativeProviderBackend),
    ElmNativeTodo,
}

#[derive(Debug, Clone, Copy)]
struct NativeProviderBackend {
    owner: ElmId,
    generation: Generation,
    handler: usize,
    snapshot: Option<usize>,
    bounds: NativeExecutionBounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KernelProviderKind {
    StaticPort,
    MgrActionInvoke,
}

#[derive(Debug, Clone)]
struct ProviderRuntime {
    port: PortId,
    owner: Option<ElmId>,
    access: ElmPortAccessPolicy,
    backend: ProviderBackend,
    backend_epoch: u64,
    dynamic: bool,
    queue_limit: u32,
    max_in_flight: u32,
    in_flight: u32,
    calls: u64,
    failed_calls: u64,
    revokes: u64,
    async_submitted: u64,
    async_completed: u64,
    async_canceled: u64,
    async_expired: u64,
    async_rejected: u64,
}

impl ProviderRuntime {
    fn record_flags(&self) -> u16 {
        let mut flags = 0;
        if self.dynamic {
            flags |= ELM_PROVIDER_FLAG_DYNAMIC;
        }
        match self.backend {
            ProviderBackend::Kernel(_) | ProviderBackend::KernelOps(_) => {
                flags |= ELM_PROVIDER_FLAG_KERNEL_BACKEND
            }
            ProviderBackend::ElmNative(_) => flags |= ELM_PROVIDER_FLAG_NATIVE_BACKEND,
            ProviderBackend::ElmNativeTodo => flags |= ELM_PROVIDER_FLAG_TODO_BACKEND,
        }
        flags
    }
}

#[derive(Debug, Clone)]
struct ResolvedEbiTopology {
    dependencies: Vec<(ElmId, FlowContract)>,
    extensions: Vec<ResolvedEbiExtension>,
}

#[derive(Debug, Clone)]
struct ResolvedEbiExtension {
    target: ElmId,
    point: String,
    contract: FlowContract,
    handler_contract: FlowContract,
    priority: i32,
}

impl ResolvedEbiTopology {
    fn empty() -> Self {
        Self {
            dependencies: Vec::new(),
            extensions: Vec::new(),
        }
    }
}

fn unit_requires_native_image_loader(unit: &ElmEbiUnit) -> bool {
    unit.entry.is_some()
        || unit
            .segments
            .iter()
            .any(|segment| segment.requires_native_loader())
}

#[derive(Debug)]
struct PendingEbiLoad {
    cell: ElmId,
    unit: ElmEbiUnit,
    topology: ResolvedEbiTopology,
    trust: PreparedImageTrust,
}

#[derive(Debug, Clone)]
struct NativeExportRuntime {
    owner: ElmId,
    generation: Generation,
    name: String,
    contract: FlowContract,
    version: u32,
    flags: u32,
    address: usize,
    bounds: Option<NativeExecutionBounds>,
}

#[derive(Debug, Clone)]
struct NativeImportRuntime {
    handle: u64,
    owner: ElmId,
    owner_generation: Generation,
    provider: ElmId,
    provider_generation: Generation,
    name: String,
    contract: FlowContract,
    min_version: u32,
    max_version: u32,
    selected_version: u32,
    flags: u32,
    address: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeImportStageKey {
    owner: ElmId,
    owner_generation: Generation,
}

#[derive(Debug)]
struct StagedNativeImports {
    key: NativeImportStageKey,
    execution: CellExecutionToken,
    imports: Vec<NativeImportRuntime>,
}

#[derive(Debug, Clone, Copy)]
struct NativeImportRebindPlan {
    runtime_index: usize,
    old_version: u32,
    new_version: u32,
    old_address: usize,
    new_address: usize,
    old_provider_generation: Generation,
    new_provider_generation: Generation,
}

struct RetiredNativeImage {
    owner: ElmId,
    generation: Generation,
    image: LoadedElmImage,
}

#[derive(Debug, Clone)]
struct ProviderAsyncJob {
    ticket: u64,
    frame: ElmCallFrame,
    consumer: ElmId,
    port: PortId,
    lease: LeaseId,
    deadline_ns: u64,
    result_ttl_ns: u64,
}

#[derive(Debug, Clone)]
struct ProviderRunningCall {
    job: ProviderAsyncJob,
    started_at_ns: u64,
    cancel_requested: bool,
}

#[derive(Debug, Clone)]
struct ProviderAsyncResult {
    ticket: u64,
    consumer: ElmId,
    port: PortId,
    lease: LeaseId,
    state: ElmProviderAsyncState,
    status: i32,
    reply: ElmReplyFrame,
    blockers: u64,
    expires_at_ns: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProviderRevokeNotification {
    callback: ElmKernelProviderRevoke,
    binding: Option<BindingId>,
    lease: Option<LeaseId>,
}

#[derive(Debug, Clone, Copy)]
struct ProviderSnapshotPageResult {
    status: i32,
    payload_len: usize,
    record_count: u32,
    flags: u32,
    next_cursor: u32,
}

#[derive(Debug, Clone, Copy)]
struct CellExecutionToken {
    cell: ElmId,
    generation: Generation,
    policy_epoch: u64,
    allowed_actions: u32,
    exclusive: bool,
}

#[derive(Debug, Clone)]
struct ProviderExecutionReservation {
    diagnostic_id: u64,
    port: PortId,
    provider_epoch: u64,
    binding: Option<BindingId>,
    validate_binding: bool,
    lease: Option<LeaseId>,
    release_lease_ref: bool,
    cells: Vec<CellExecutionToken>,
}

#[derive(Debug, Clone, Copy)]
struct ProviderExecutionDiagnostic {
    id: u64,
    port: PortId,
    binding: Option<BindingId>,
    lease: Option<LeaseId>,
    provider_epoch: u64,
    started_at_ns: u64,
    deadline_ns: u64,
}

#[derive(Debug, Clone)]
struct ProviderCallExecutionPlan {
    reservation: ProviderExecutionReservation,
    backend: ProviderBackend,
    edge: elm_model::CapabilityBindingEdge,
    frame: ElmCallFrame,
    deadline_ns: u64,
    reply_flags_mask: u32,
}

#[derive(Debug, Clone)]
struct ProviderSnapshotExecutionPlan {
    reservation: ProviderExecutionReservation,
    backend: ProviderBackend,
    request: ElmProviderSnapshotRequest,
    binding_id: u64,
    lease: LeaseId,
    audit_cell: ElmId,
    allowed_actions: u32,
    capacity: usize,
}

#[derive(Debug, Clone, Copy)]
enum ManagedCallerReservation {
    Active(CellExecutionToken),
    Staged {
        stage: NativeImportStageKey,
        execution: CellExecutionToken,
    },
}

impl ManagedCallerReservation {
    const fn cell(self) -> ElmId {
        match self {
            Self::Active(token) => token.cell,
            Self::Staged { stage, .. } => stage.owner,
        }
    }

    const fn generation(self) -> Generation {
        match self {
            Self::Active(token) => token.generation,
            Self::Staged { stage, .. } => stage.owner_generation,
        }
    }
}

struct ManagedCallExecutionPlan {
    caller: ManagedCallerReservation,
    callee: CellExecutionToken,
    import_handle: u64,
    address: usize,
    bounds: NativeExecutionBounds,
    frame: ElmCallFrame,
}

enum PreparedProviderCall {
    Immediate(Result<ElmProviderInvokeResponse, i32>),
    External(ProviderCallExecutionPlan),
}

enum PreparedProviderSnapshot {
    Immediate(Result<Vec<u8>, i32>),
    External(ProviderSnapshotExecutionPlan),
}

struct AsyncProviderExecutionPlan {
    ticket: u64,
    call: ProviderCallExecutionPlan,
}

enum PreparedAsyncProviderWork {
    None,
    Handled,
    External(AsyncProviderExecutionPlan),
}

struct ExtensionDispatchExecutionPlan {
    target: CellExecutionToken,
    requested_extension: Option<ElmId>,
    matched_edges: Vec<ExtensionEdge>,
    mode: ElmMixinMode,
    opcode: u32,
    payload: Vec<u8>,
}

struct MixinDispatchState {
    mode: ElmMixinMode,
    original_payload: Vec<u8>,
    current_payload: Vec<u8>,
    called: u32,
    blockers: u64,
    last_reply: ElmReplyFrame,
    halted: bool,
    has_failure_reply: bool,
}

impl MixinDispatchState {
    fn new(mode: ElmMixinMode, opcode: u32, payload: Vec<u8>) -> Self {
        Self {
            mode,
            original_payload: payload.clone(),
            current_payload: payload,
            called: 0,
            blockers: 0,
            last_reply: ElmReplyFrame::empty(0, u64::from(opcode), ELM_CALL_STATUS_OK),
            halted: false,
            has_failure_reply: false,
        }
    }

    fn payload(&self) -> &[u8] {
        match self.mode {
            ElmMixinMode::Observer => &self.original_payload,
            ElmMixinMode::Chain | ElmMixinMode::Exclusive => &self.current_payload,
        }
    }

    fn note_invocation(&mut self) -> bool {
        let Some(called) = self.called.checked_add(1) else {
            self.blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
            self.halted = true;
            return false;
        };
        self.called = called;
        true
    }

    fn record_execution_error(&mut self, status: i32) {
        self.blockers |= extension_dispatch_blocker(status);
        if self.mode != ElmMixinMode::Observer {
            self.halted = true;
        }
    }

    fn record_reply(&mut self, reply: ElmReplyFrame) {
        let reply_len = usize::from(reply.payload_len);
        if reply.status != ELM_CALL_STATUS_OK {
            self.blockers |= provider_call_blockers(reply.status);
            self.record_failure_reply(reply);
            if self.mode != ElmMixinMode::Observer {
                self.halted = true;
            }
            return;
        }
        if reply_len > reply.payload.len() {
            self.blockers |= ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED;
            self.record_failure_reply(ElmReplyFrame::empty(
                reply.binding_id,
                reply.call_id,
                ELM_CALL_STATUS_INVALID,
            ));
            if self.mode != ElmMixinMode::Observer {
                self.halted = true;
            }
            return;
        }

        if self.mode == ElmMixinMode::Observer {
            if reply.flags != ELM_MIXIN_REPLY_CONTINUE {
                self.blockers |= ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED;
                self.record_failure_reply(ElmReplyFrame::empty(
                    reply.binding_id,
                    reply.call_id,
                    ELM_CALL_STATUS_INVALID,
                ));
            } else if !self.has_failure_reply {
                self.last_reply = reply;
            }
            return;
        }

        if reply.flags & !ELM_MIXIN_REPLY_FLAGS_MASK != 0
            || reply.flags & ELM_MIXIN_REPLY_DENY != 0 && reply.flags != ELM_MIXIN_REPLY_DENY
        {
            self.blockers |= ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED;
            self.record_failure_reply(ElmReplyFrame::empty(
                reply.binding_id,
                reply.call_id,
                ELM_CALL_STATUS_INVALID,
            ));
            self.halted = true;
            return;
        }
        if reply.flags & ELM_MIXIN_REPLY_DENY != 0 {
            self.blockers |= ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED;
            self.record_failure_reply(reply);
            self.halted = true;
            return;
        }
        if reply.flags & ELM_MIXIN_REPLY_REPLACE != 0 {
            self.current_payload.clear();
            self.current_payload
                .extend_from_slice(&reply.payload[..reply_len]);
        }
        self.last_reply = reply;
        if reply.flags & ELM_MIXIN_REPLY_STOP != 0 {
            self.halted = true;
        }
    }

    fn record_failure_reply(&mut self, reply: ElmReplyFrame) {
        if !self.has_failure_reply {
            self.last_reply = reply;
            self.has_failure_reply = true;
        }
    }
}

struct MixinProviderExecutionPlan {
    call: ProviderCallExecutionPlan,
    ephemeral_lease: LeaseId,
}

enum PreparedExtensionDispatch {
    Immediate(Result<ElmExtensionDispatchResponse, i32>),
    External(ExtensionDispatchExecutionPlan),
}

enum NativeLifecycleWork {
    Pause {
        quiesce: ElmContext,
        pause: ElmContext,
    },
    Resume {
        resume: ElmContext,
    },
    Detach {
        quiesce: Option<ElmContext>,
        finalize: Option<ElmContext>,
        owner: ElmId,
        generation: Generation,
    },
}

struct NativeLifecycleExecutionPlan {
    token: CellExecutionToken,
    action: ElmLifecycleAction,
    executor: Option<NativeHookExecutor>,
    work: NativeLifecycleWork,
    source_suspension: Option<super::source::ProjectionSourceSuspension>,
}

struct NativeLifecycleExecutionOutcome {
    result: ElmResult<()>,
    blockers: u64,
    reason: u32,
    drained_resources: u32,
}

enum PreparedNativeLifecycle {
    Immediate(ElmLifecycleResponse),
    External(NativeLifecycleExecutionPlan),
}

struct NativeLoadExecutionPlan {
    token: CellExecutionToken,
    id: ElmId,
    parent: Option<ElmId>,
    unit: ElmEbiUnit,
    topology: ResolvedEbiTopology,
    loaded: LoadedElmImage,
    exports: Vec<NativeExportRuntime>,
    import_stage: NativeImportStageKey,
    initialize: ElmContext,
    trust: PreparedImageTrust,
}

struct NativeLoadFailurePlan {
    id: ElmId,
    token: CellExecutionToken,
    import_stage: NativeImportStageKey,
    loaded: LoadedElmImage,
    finalize: ElmContext,
    response: ElmLoadCellResponse,
}

enum PreparedNativeLoad {
    Immediate(ElmLoadCellResponse),
    Initialize(NativeLoadExecutionPlan),
}

enum NativeLoadCommit {
    Complete(ElmLoadCellResponse),
    Entry(NativeLoadExecutionPlan),
    Finalize(NativeLoadFailurePlan),
}

struct NativeReplaceExecutionPlan {
    token: CellExecutionToken,
    id: ElmId,
    old_state: ElmState,
    old_generation: Generation,
    new_generation: Generation,
    suspended_projection_sources: usize,
    unit: ElmEbiUnit,
    loaded: LoadedElmImage,
    exports: Vec<NativeExportRuntime>,
    import_stage: NativeImportStageKey,
    old_executor: NativeHookExecutor,
    new_executor: NativeHookExecutor,
    new_initialize: ElmContext,
    new_finalize: ElmContext,
    old_quiesce: Option<ElmContext>,
    old_finalize: ElmContext,
    old_resume: Option<ElmContext>,
    migration: Vec<u8>,
    trust: PreparedImageTrust,
}

struct NativeReplaceExecutionOutcome {
    commit: bool,
    old_execution: OldGenerationExecutionState,
    status: i32,
    blockers: u64,
    reason: u32,
    migrated_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OldGenerationExecutionState {
    Untouched,
    Quiesced,
    Resumed,
    Compromised,
}

impl OldGenerationExecutionState {
    const fn recovered(self) -> bool {
        matches!(self, Self::Untouched | Self::Resumed)
    }
}

#[cfg(feature = "kernel-tests")]
#[derive(Default)]
struct ReplaceRecoveryTestExecutor {
    resume_calls: u32,
    fail_resume: bool,
}

#[cfg(feature = "kernel-tests")]
impl ElmLifecycleExecutor for ReplaceRecoveryTestExecutor {
    fn on_initialize(&mut self, _context: &mut ElmContext) -> ElmResult<()> {
        Ok(())
    }

    fn on_finalize(&mut self, _context: &mut ElmContext) -> ElmResult<()> {
        Ok(())
    }

    fn on_resume(&mut self, context: &mut ElmContext) -> ElmResult<()> {
        if context.phase() != ElmLifecyclePhase::Resume {
            return Err(ElmError::InvalidTransition);
        }
        self.resume_calls += 1;
        if self.fail_resume {
            Err(ElmError::InvalidTransition)
        } else {
            Ok(())
        }
    }
}

enum PreparedNativeReplace {
    Immediate(ElmReplaceCellResponseV1),
    Execute(NativeReplaceExecutionPlan),
}

impl ProviderSnapshotPageResult {
    const fn new(
        status: i32,
        payload_len: usize,
        record_count: u32,
        flags: u32,
        next_cursor: u32,
    ) -> Self {
        Self {
            status,
            payload_len,
            record_count,
            flags,
            next_cursor,
        }
    }

    const fn status_only(status: i32) -> Self {
        Self::new(status, 0, 0, 0, 0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MgrActionKind {
    Health,
}

#[derive(Debug, Clone)]
struct MgrActionRuntime {
    action: ActionId,
    menu_item: u64,
    owner: ElmId,
    kind: MgrActionKind,
}

#[allow(dead_code)]
pub(crate) trait ElmMgrProviderOps {
    fn descriptor(&self) -> ElmMgrApiDescriptor;
    fn invoke(&self, frame: ElmCallFrame) -> ElmReplyFrame;
    fn poll_ready(&self) -> bool {
        true
    }
    fn snapshot(&self, out: &mut Vec<u8>) -> Result<(), i32>;
    fn on_revoke(&self, binding: Option<BindingId>, lease: Option<LeaseId>);
}

#[derive(Debug, Clone)]
struct EventSubscriptionRuntime {
    subscription: u64,
    owner: ElmId,
    lease: LeaseId,
    cursor: u64,
    kind_filter: u32,
    cell_filter: u64,
    port_filter: u64,
    binding_filter: u64,
    lease_filter: u64,
    delivered_events: u64,
    dropped_events: u64,
}

impl EventSubscriptionRuntime {
    fn record(&self) -> ElmMgrEventSubscriptionRecord {
        ElmMgrEventSubscriptionRecord::new(
            self.subscription,
            self.owner.0,
            self.lease.0,
            self.cursor,
            self.kind_filter,
            true,
            self.cell_filter,
            self.port_filter,
            self.binding_filter,
            self.lease_filter,
            self.delivered_events,
            self.dropped_events,
        )
    }

    fn matches(&self, event: &ElmEventRecord) -> bool {
        (self.kind_filter == 0 || self.kind_filter == event.kind)
            && (self.cell_filter == 0 || self.cell_filter == event.cell)
            && (self.port_filter == 0 || self.port_filter == event.port)
            && (self.binding_filter == 0 || self.binding_filter == event.binding)
            && (self.lease_filter == 0 || self.lease_filter == event.lease)
    }
}

#[derive(Debug)]
struct ElmMgrRuntime {
    api_registry: Vec<ElmMgrApiDescriptor>,
    api_generation: Generation,
    event_subscriptions: Vec<EventSubscriptionRuntime>,
    next_event_subscription_id: u64,
    next_kernel_provider_api_id: u64,
}

impl ElmMgrRuntime {
    const fn new() -> Self {
        Self {
            api_registry: Vec::new(),
            api_generation: Generation::FIRST,
            event_subscriptions: Vec::new(),
            next_event_subscription_id: FIRST_EVENT_SUBSCRIPTION_ID,
            next_kernel_provider_api_id: FIRST_KERNEL_PROVIDER_API_ID,
        }
    }

    fn register_api(&mut self, descriptor: ElmMgrApiDescriptor) -> bool {
        if self
            .api_registry
            .iter()
            .any(|existing| existing.id == descriptor.id)
        {
            return true;
        }
        let Some(next_generation) = self.api_generation.checked_next() else {
            return false;
        };
        if self.api_registry.try_reserve(1).is_err() {
            return false;
        }
        self.api_registry.push(descriptor);
        self.api_generation = next_generation;
        true
    }

    fn alloc_event_subscription_id(&mut self) -> Option<u64> {
        take_monotonic_id(&mut self.next_event_subscription_id)
    }

    fn alloc_kernel_provider_api_id(&mut self) -> Option<u64> {
        take_monotonic_id(&mut self.next_kernel_provider_api_id)
    }

    fn event_subscription_index(&self, subscription: u64) -> Option<usize> {
        self.event_subscriptions
            .iter()
            .position(|entry| entry.subscription == subscription)
    }

    fn remove_event_subscriptions_owned_by(&mut self, owner: ElmId) -> usize {
        let before = self.event_subscriptions.len();
        self.event_subscriptions
            .retain(|entry| entry.owner != owner);
        before - self.event_subscriptions.len()
    }
}

pub(crate) struct ElmCore {
    initialized: bool,
    trust_store: ElmTrustStore,
    allow_unsigned_external: bool,
    mgr_runtime: ElmMgrRuntime,
    graph: BindingGraph,
    cells: Vec<CellRuntime>,
    pending_ebi_loads: Vec<PendingEbiLoad>,
    native_images: Vec<LoadedElmImage>,
    retired_native_images: Vec<RetiredNativeImage>,
    native_exports: Vec<NativeExportRuntime>,
    native_imports: Vec<NativeImportRuntime>,
    staged_native_imports: Vec<StagedNativeImports>,
    ports: Vec<PortRuntime>,
    providers: Vec<ProviderRuntime>,
    provider_jobs: VecDeque<ProviderAsyncJob>,
    provider_running: Vec<ProviderRunningCall>,
    provider_results: VecDeque<ProviderAsyncResult>,
    provider_revoke_notifications: VecDeque<ProviderRevokeNotification>,
    active_provider_executions: Vec<ProviderExecutionDiagnostic>,
    runtime_ports: Vec<RuntimePortBinding>,
    menu_items: Vec<MenuItemRuntime>,
    mgr_actions: Vec<MgrActionRuntime>,
    menu_generation: Generation,
    leases: LeaseRegistry,
    events: Vec<ElmEventRecord>,
    next_event_sequence: ElmEventSequence,
    dropped_event_count: u64,
    acknowledged_event_sequence: u64,
    audits: Vec<ElmMgrAuditRecord>,
    next_audit_sequence: u64,
    dropped_audit_count: u32,
    lifecycle_traces: Vec<ElmRuntimeTraceRecord>,
    provider_call_traces: Vec<ElmRuntimeTraceRecord>,
    mixin_traces: Vec<ElmRuntimeTraceRecord>,
    replace_traces: Vec<ElmRuntimeTraceRecord>,
    policy_traces: Vec<ElmRuntimeTraceRecord>,
    resource_traces: Vec<ElmRuntimeTraceRecord>,
    runtime_journal: Vec<ElmRuntimeTraceRecord>,
    next_trace_sequence: u64,
    dropped_lifecycle_trace_count: u32,
    dropped_provider_call_trace_count: u32,
    dropped_mixin_trace_count: u32,
    dropped_replace_trace_count: u32,
    dropped_policy_trace_count: u32,
    dropped_resource_trace_count: u32,
    dropped_runtime_journal_count: u32,
    #[allow(dead_code)]
    next_cell_id: u64,
    #[allow(dead_code)]
    next_port_id: u64,
    #[allow(dead_code)]
    next_binding_id: u64,
    #[allow(dead_code)]
    next_lease_id: u64,
    #[allow(dead_code)]
    next_action_id: u64,
    #[allow(dead_code)]
    next_menu_item_id: u64,
    next_provider_ticket_id: u64,
    next_provider_execution_id: u64,
    next_managed_import_handle: u64,
}

impl ElmCore {
    pub const fn new() -> Self {
        Self {
            initialized: false,
            trust_store: ElmTrustStore::new(),
            allow_unsigned_external: cfg!(feature = "kernel-tests"),
            mgr_runtime: ElmMgrRuntime::new(),
            graph: BindingGraph::new(),
            cells: Vec::new(),
            pending_ebi_loads: Vec::new(),
            native_images: Vec::new(),
            retired_native_images: Vec::new(),
            native_exports: Vec::new(),
            native_imports: Vec::new(),
            staged_native_imports: Vec::new(),
            ports: Vec::new(),
            providers: Vec::new(),
            provider_jobs: VecDeque::new(),
            provider_running: Vec::new(),
            provider_results: VecDeque::new(),
            provider_revoke_notifications: VecDeque::new(),
            active_provider_executions: Vec::new(),
            runtime_ports: Vec::new(),
            menu_items: Vec::new(),
            mgr_actions: Vec::new(),
            menu_generation: Generation::FIRST,
            leases: LeaseRegistry::new(),
            events: Vec::new(),
            next_event_sequence: ElmEventSequence::FIRST,
            dropped_event_count: 0,
            acknowledged_event_sequence: 0,
            audits: Vec::new(),
            next_audit_sequence: 1,
            dropped_audit_count: 0,
            lifecycle_traces: Vec::new(),
            provider_call_traces: Vec::new(),
            mixin_traces: Vec::new(),
            replace_traces: Vec::new(),
            policy_traces: Vec::new(),
            resource_traces: Vec::new(),
            runtime_journal: Vec::new(),
            next_trace_sequence: 1,
            dropped_lifecycle_trace_count: 0,
            dropped_provider_call_trace_count: 0,
            dropped_mixin_trace_count: 0,
            dropped_replace_trace_count: 0,
            dropped_policy_trace_count: 0,
            dropped_resource_trace_count: 0,
            dropped_runtime_journal_count: 0,
            next_cell_id: FIRST_DYNAMIC_CELL_ID,
            next_port_id: FIRST_DYNAMIC_PORT_ID,
            next_binding_id: 100,
            next_lease_id: 100,
            next_action_id: 100,
            next_menu_item_id: 100,
            next_provider_ticket_id: FIRST_PROVIDER_TICKET_ID,
            next_provider_execution_id: 1,
            next_managed_import_handle: FIRST_MANAGED_IMPORT_HANDLE,
        }
    }

    pub fn register_trust_anchor(&mut self, anchor: ElmTrustAnchor) -> Result<(), ElmTrustError> {
        if self.initialized {
            return Err(ElmTrustError::Sealed);
        }
        self.trust_store.register_anchor(anchor)
    }

    pub(crate) const fn initialized(&self) -> bool {
        self.initialized
    }

    pub fn set_allow_unsigned_external(&mut self, allow: bool) -> Result<(), ElmTrustError> {
        if self.initialized || self.trust_store.sealed() {
            return Err(ElmTrustError::Sealed);
        }
        self.allow_unsigned_external = allow;
        Ok(())
    }

    pub fn trust_runtime_info(&self) -> ElmTrustRuntimeInfoV1 {
        let mut flags = 0;
        if self.trust_store.sealed() {
            flags |= ELM_TRUST_FLAG_SEALED;
        }
        if self.allow_unsigned_external {
            flags |= ELM_TRUST_FLAG_ALLOW_UNSIGNED;
        }
        if self.cells.iter().any(|cell| {
            cell.trust_unsigned && !matches!(cell.state, ElmState::Detached | ElmState::Retired)
        }) {
            flags |= ELM_TRUST_FLAG_UNSIGNED_ACTIVE;
        }
        ElmTrustRuntimeInfoV1::new(
            flags,
            self.trust_store.anchors().len() as u32,
            self.trust_store.revoked().len() as u32,
            self.trust_store.accepted_epochs().len() as u32,
        )
    }

    pub(crate) fn authorize_mgr_call(
        &self,
        principal: ElmPrincipal,
        kind: ElmMgrCallKind,
        target: ElmMgrAccessTarget,
    ) -> ElmMgrAuthorization {
        let mut authorization = match principal.kind {
            ElmPrincipalKind::Kernel => ElmMgrAuthorization {
                principal,
                authority: ELM_AUDIT_AUTHORITY_KERNEL,
                authority_id: 0,
                actor_generation: 0,
                policy_epoch: 0,
                subject_id: self
                    .access_target_subject(target)
                    .map(|id| id.0)
                    .unwrap_or(0),
                blockers: 0,
            },
            ElmPrincipalKind::UserAdmin => ElmMgrAuthorization {
                principal,
                authority: ELM_AUDIT_AUTHORITY_USER_ADMIN,
                authority_id: principal.credential_id,
                actor_generation: 0,
                policy_epoch: 0,
                subject_id: self
                    .access_target_subject(target)
                    .map(|id| id.0)
                    .unwrap_or(0),
                blockers: 0,
            },
            ElmPrincipalKind::ElmCell => self.authorize_elm_cell_call(principal, kind, target),
        };
        if mgr_call_is_mutating(kind) && !super::journal::mutation_allowed() {
            authorization.blockers |= ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE;
        }
        authorization
    }

    fn revalidate_mgr_authorization(
        &self,
        previous: ElmMgrAuthorization,
        kind: ElmMgrCallKind,
        target: ElmMgrAccessTarget,
    ) -> ElmMgrAuthorization {
        let mut current = self.authorize_mgr_call(previous.principal, kind, target);
        if previous.principal.kind == ElmPrincipalKind::ElmCell
            && current.allowed()
            && (current.actor_generation != previous.actor_generation
                || current.policy_epoch != previous.policy_epoch
                || current.authority != previous.authority
                || current.authority_id != previous.authority_id
                || current.subject_id != previous.subject_id)
        {
            current.blockers = ELM_POLICY_BLOCK_CALLER_STALE;
        }
        current
    }

    pub(crate) fn reserve_mgr_authorization_execution(
        &mut self,
        authorization: ElmMgrAuthorization,
        kind: ElmMgrCallKind,
    ) -> Result<ElmMgrAuthorizationExecution, i32> {
        let mut subject = None;
        if kind == ElmMgrCallKind::LoadCell && authorization.subject_id != ELM_MGR_ID.0 {
            subject = Some(self.reserve_cell_execution(ElmId(authorization.subject_id))?);
        }
        let actor = if authorization.principal.kind == ElmPrincipalKind::ElmCell {
            let actor = ElmId(authorization.principal.actor_id);
            if actor != ELM_MGR_ID
                && subject.is_none_or(|token| token.cell != actor)
                && (actor.0 != authorization.subject_id || kind == ElmMgrCallKind::LoadCell)
            {
                match self.reserve_cell_execution(actor) {
                    Ok(token) => Some(token),
                    Err(status) => {
                        if let Some(token) = subject {
                            self.release_cell_execution(token);
                        }
                        return Err(status);
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        Ok(ElmMgrAuthorizationExecution { actor, subject })
    }

    fn mgr_authorization_execution_is_current(
        &self,
        execution: ElmMgrAuthorizationExecution,
    ) -> bool {
        execution
            .actor
            .is_none_or(|token| self.cell_execution_is_current(token))
            && execution
                .subject
                .is_none_or(|token| self.cell_execution_is_current(token))
    }

    pub(crate) fn release_mgr_authorization_execution(
        &mut self,
        execution: ElmMgrAuthorizationExecution,
    ) {
        if let Some(token) = execution.actor {
            self.release_cell_execution(token);
        }
        if let Some(token) = execution.subject {
            self.release_cell_execution(token);
        }
    }

    fn authorize_elm_cell_call(
        &self,
        principal: ElmPrincipal,
        kind: ElmMgrCallKind,
        target: ElmMgrAccessTarget,
    ) -> ElmMgrAuthorization {
        let actor_id = ElmId(principal.actor_id);
        let Some(actor) = self.cells.iter().find(|cell| cell.id == actor_id) else {
            return ElmMgrAuthorization {
                principal,
                authority: 0,
                authority_id: 0,
                actor_generation: principal.generation.0,
                policy_epoch: 0,
                subject_id: self
                    .access_target_subject(target)
                    .map(|id| id.0)
                    .unwrap_or(0),
                blockers: ELM_POLICY_BLOCK_CALLER_NOT_FOUND,
            };
        };
        let mut authorization = ElmMgrAuthorization {
            principal,
            authority: if actor_id == ELM_MGR_ID {
                ELM_AUDIT_AUTHORITY_MANAGER
            } else if self.cell_has_global_management_scope(actor_id) {
                ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER
            } else {
                ELM_AUDIT_AUTHORITY_SELF
            },
            authority_id: actor_id.0,
            actor_generation: actor.generation.0,
            policy_epoch: actor.policy_epoch,
            subject_id: self
                .access_target_subject(target)
                .map(|id| id.0)
                .unwrap_or(0),
            blockers: 0,
        };
        if principal.generation != actor.generation
            || matches!(
                actor.state,
                ElmState::Detached | ElmState::Retired | ElmState::Faulted | ElmState::Quarantined
            )
        {
            authorization.blockers = ELM_POLICY_BLOCK_CALLER_STALE;
            return authorization;
        }
        if actor_id != ELM_MGR_ID {
            let required = mgr_call_required_action(kind);
            if required != 0 && actor.cell_policy.allowed_actions & required == 0 {
                authorization.blockers |= ELM_POLICY_BLOCK_CAPABILITY_DENIED;
            }
            authorization.blockers |= detailed_policy_blockers(actor.cell_policy, kind);
        }
        if authorization.blockers != 0 {
            return authorization;
        }

        match target {
            ElmMgrAccessTarget::Global => {
                if !self.cell_has_global_management_scope(actor_id)
                    && mgr_call_is_manager_only_query(kind)
                {
                    authorization.blockers = ELM_POLICY_BLOCK_SCOPE_DENIED;
                }
            }
            ElmMgrAccessTarget::Load(parent, budget) => {
                self.apply_cell_scope(actor_id, parent, &mut authorization);
                if authorization.blockers == 0 {
                    self.authorize_child_load(parent, budget, &mut authorization);
                }
            }
            ElmMgrAccessTarget::Cell(target_id) => {
                self.apply_cell_scope(actor_id, target_id, &mut authorization);
            }
            ElmMgrAccessTarget::Cells(first, second) => {
                self.apply_pair_scope(actor_id, kind, first, second, &mut authorization);
            }
            ElmMgrAccessTarget::Port(port_id) => {
                let owner = self
                    .ports
                    .iter()
                    .find(|port| port.id == port_id)
                    .and_then(|port| port.owner);
                match owner {
                    Some(owner) => self.apply_cell_scope(actor_id, owner, &mut authorization),
                    None if !self.cell_has_global_management_scope(actor_id) => {
                        authorization.blockers = ELM_POLICY_BLOCK_SCOPE_DENIED;
                    }
                    None => {}
                }
            }
            ElmMgrAccessTarget::Binding(binding_id) => {
                let owner = self
                    .graph
                    .capability_bindings()
                    .iter()
                    .find(|edge| edge.id == binding_id)
                    .map(|edge| edge.consumer);
                match owner {
                    Some(owner) => self.apply_cell_scope(actor_id, owner, &mut authorization),
                    None => authorization.blockers = ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
                }
            }
            ElmMgrAccessTarget::Subscription(subscription_id) => {
                let owner = self
                    .mgr_runtime
                    .event_subscriptions
                    .iter()
                    .find(|entry| entry.subscription == subscription_id)
                    .map(|entry| entry.owner);
                match owner {
                    Some(owner) => self.apply_cell_scope(actor_id, owner, &mut authorization),
                    None => authorization.blockers = ELM_POLICY_BLOCK_CELL_NOT_FOUND,
                }
            }
            ElmMgrAccessTarget::ProviderTicket(ticket) => {
                let owner = self.provider_ticket_owner(ticket);
                match owner {
                    Some(owner) => self.apply_cell_scope(actor_id, owner, &mut authorization),
                    None => authorization.blockers = ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
                }
            }
            ElmMgrAccessTarget::PolicyUpdate(policy) => {
                let target_id = ElmId(policy.cell_id);
                self.apply_cell_scope(actor_id, target_id, &mut authorization);
                if authorization.blockers == 0 {
                    self.authorize_policy_update(actor, target_id, policy, &mut authorization);
                }
            }
            ElmMgrAccessTarget::ResourceUpdate(request) => {
                let target_id = ElmId(request.cell_id);
                self.apply_cell_scope(actor_id, target_id, &mut authorization);
                if authorization.blockers == 0 {
                    self.authorize_resource_update(actor, target_id, request, &mut authorization);
                }
            }
        }
        authorization
    }

    fn access_target_subject(&self, target: ElmMgrAccessTarget) -> Option<ElmId> {
        match target {
            ElmMgrAccessTarget::Global => None,
            ElmMgrAccessTarget::Load(parent, _) => Some(parent),
            ElmMgrAccessTarget::Cell(id) => Some(id),
            ElmMgrAccessTarget::Cells(first, _) => Some(first),
            ElmMgrAccessTarget::Port(port_id) => self
                .ports
                .iter()
                .find(|port| port.id == port_id)
                .and_then(|port| port.owner),
            ElmMgrAccessTarget::Binding(binding_id) => self
                .graph
                .capability_bindings()
                .iter()
                .find(|edge| edge.id == binding_id)
                .map(|edge| edge.consumer),
            ElmMgrAccessTarget::Subscription(subscription_id) => self
                .mgr_runtime
                .event_subscriptions
                .iter()
                .find(|entry| entry.subscription == subscription_id)
                .map(|entry| entry.owner),
            ElmMgrAccessTarget::ProviderTicket(ticket) => self.provider_ticket_owner(ticket),
            ElmMgrAccessTarget::PolicyUpdate(policy) => Some(ElmId(policy.cell_id)),
            ElmMgrAccessTarget::ResourceUpdate(request) => Some(ElmId(request.cell_id)),
        }
    }

    fn apply_cell_scope(
        &self,
        actor: ElmId,
        target: ElmId,
        authorization: &mut ElmMgrAuthorization,
    ) {
        authorization.subject_id = target.0;
        if self.cell_index(target).is_none() {
            authorization.blockers = ELM_POLICY_BLOCK_CELL_NOT_FOUND;
        } else if self.cell_has_global_management_scope(actor) {
            authorization.authority = if actor == ELM_MGR_ID {
                ELM_AUDIT_AUTHORITY_MANAGER
            } else {
                ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER
            };
        } else if actor == target {
            authorization.authority = ELM_AUDIT_AUTHORITY_SELF;
        } else if self.cell_is_descendant_of(target, actor) {
            authorization.authority = ELM_AUDIT_AUTHORITY_ANCESTOR;
        } else {
            authorization.blockers = ELM_POLICY_BLOCK_SCOPE_DENIED;
        }
    }

    fn apply_pair_scope(
        &self,
        actor: ElmId,
        kind: ElmMgrCallKind,
        first: ElmId,
        second: ElmId,
        authorization: &mut ElmMgrAuthorization,
    ) {
        match kind {
            ElmMgrCallKind::PreflightExtensionAttach | ElmMgrCallKind::CommitExtensionAttach => {
                self.apply_cell_scope(actor, first, authorization);
                if authorization.blockers != 0 {
                    return;
                }
                let Some(target) = self.cells.iter().find(|cell| cell.id == second) else {
                    authorization.blockers = ELM_POLICY_BLOCK_CELL_NOT_FOUND;
                    return;
                };
                if !self.cell_has_global_management_scope(actor)
                    && target.cell_policy.extension_flags & ELM_EXTENSION_POLICY_ACCEPT == 0
                {
                    authorization.blockers = ELM_POLICY_BLOCK_CAPABILITY_DENIED;
                }
            }
            ElmMgrCallKind::CommitExtensionDetach => {
                let first_in_scope = self.cell_has_global_management_scope(actor)
                    || actor == first
                    || self.cell_is_descendant_of(first, actor);
                let second_in_scope = self.cell_has_global_management_scope(actor)
                    || actor == second
                    || self.cell_is_descendant_of(second, actor);
                if !first_in_scope && !second_in_scope {
                    authorization.blockers = ELM_POLICY_BLOCK_SCOPE_DENIED;
                } else {
                    authorization.subject_id = first.0;
                    authorization.authority = if self.cell_has_global_management_scope(actor) {
                        if actor == ELM_MGR_ID {
                            ELM_AUDIT_AUTHORITY_MANAGER
                        } else {
                            ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER
                        }
                    } else if actor == first || actor == second {
                        ELM_AUDIT_AUTHORITY_SELF
                    } else {
                        ELM_AUDIT_AUTHORITY_ANCESTOR
                    };
                }
            }
            ElmMgrCallKind::DispatchExtension => {
                self.apply_cell_scope(actor, first, authorization);
            }
            _ => self.apply_cell_scope(actor, first, authorization),
        }
    }

    fn authorize_policy_update(
        &self,
        actor: &CellRuntime,
        target_id: ElmId,
        policy: ElmCellPolicyV1,
        authorization: &mut ElmMgrAuthorization,
    ) {
        let Some(target) = self.cells.iter().find(|cell| cell.id == target_id) else {
            authorization.blockers = ELM_POLICY_BLOCK_CELL_NOT_FOUND;
            return;
        };
        if target.cell_policy.flags & ELM_CELL_POLICY_FLAG_LOCKED != 0 {
            authorization.blockers = ELM_POLICY_BLOCK_CAPABILITY_DENIED;
            return;
        }
        if policy.generation != target.generation.0 || policy.policy_epoch != target.policy_epoch {
            authorization.blockers = ELM_POLICY_BLOCK_CALLER_STALE;
            return;
        }
        let ceiling = if actor.id == target_id {
            target.cell_policy
        } else {
            actor.cell_policy
        };
        if policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT
            != target.cell_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT
            || !policy_capabilities_subset(policy, ceiling)
            || (actor.id == target_id && target.cell_policy.flags & !policy.flags != 0)
            || !self.policy_update_respects_hierarchy(target_id, policy)
        {
            authorization.blockers = ELM_POLICY_BLOCK_POLICY_ESCALATION;
            return;
        }
        if (policy.native_flags & ELM_NATIVE_POLICY_EXECUTE == 0
            && self.native_image_index(target_id).is_some())
            || (policy.native_flags & ELM_NATIVE_POLICY_IMPORT == 0
                && self
                    .native_imports
                    .iter()
                    .any(|import| import.owner == target_id))
            || (policy.native_flags & ELM_NATIVE_POLICY_EXPORT == 0
                && self
                    .native_exports
                    .iter()
                    .any(|export| export.owner == target_id))
        {
            authorization.blockers = ELM_POLICY_BLOCK_PROVIDER_BUSY;
        }
    }

    fn cell_has_global_management_scope(&self, id: ElmId) -> bool {
        id == ELM_MGR_ID
            || self.cells.iter().any(|cell| {
                cell.id == id
                    && cell.kind == ElmKind::Manager
                    && cell.cell_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT != 0
                    && !matches!(
                        cell.state,
                        ElmState::Detached
                            | ElmState::Retired
                            | ElmState::Faulted
                            | ElmState::Quarantined
                    )
            })
    }

    fn cell_requires_signed_management(&self, id: ElmId) -> bool {
        self.cells.iter().any(|cell| {
            cell.id == id
                && cell.kind == ElmKind::Manager
                && cell.cell_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT != 0
        })
    }

    fn policy_update_respects_hierarchy(
        &self,
        target_id: ElmId,
        candidate: ElmCellPolicyV1,
    ) -> bool {
        let Some(target) = self.cells.iter().find(|cell| cell.id == target_id) else {
            return false;
        };
        if let Some(parent_id) = target.parent {
            let Some(parent) = self.cells.iter().find(|cell| cell.id == parent_id) else {
                return false;
            };
            if !policy_is_delegable_from(candidate, parent.cell_policy) {
                return false;
            }
            if parent.cell_policy.flags & ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION != 0
                && !policy_capabilities_subset(candidate, target.cell_policy)
            {
                return false;
            }
        }
        self.cells
            .iter()
            .filter(|cell| {
                cell.parent == Some(target_id)
                    && !matches!(cell.state, ElmState::Detached | ElmState::Retired)
            })
            .all(|child| policy_is_delegable_from(child.cell_policy, candidate))
    }

    fn authorize_resource_update(
        &self,
        actor: &CellRuntime,
        target_id: ElmId,
        request: ElmResourceBudgetUpdateRequest,
        authorization: &mut ElmMgrAuthorization,
    ) {
        let Some(target) = self.cells.iter().find(|cell| cell.id == target_id) else {
            authorization.blockers = ELM_POLICY_BLOCK_CELL_NOT_FOUND;
            return;
        };
        if actor.id == target_id {
            if !budget_is_subset(request.budget, target.resource_budget) {
                authorization.blockers = ELM_POLICY_BLOCK_POLICY_ESCALATION;
                return;
            }
        }
        if !self.cell_budget_covers_usage_and_children(target_id, request.budget)
            || target.parent.is_some_and(|parent| {
                !self.child_budget_allocation_fits(parent, target_id, request.budget)
            })
            || (actor.id != target_id && !budget_is_subset(request.budget, actor.resource_budget))
        {
            authorization.blockers = ELM_POLICY_BLOCK_RESOURCE_QUOTA;
        }
    }

    fn authorize_child_load(
        &self,
        parent_id: ElmId,
        budget: ElmResourceBudget,
        authorization: &mut ElmMgrAuthorization,
    ) {
        let Some(parent) = self.cells.iter().find(|cell| cell.id == parent_id) else {
            authorization.blockers = ELM_POLICY_BLOCK_CELL_NOT_FOUND;
            return;
        };
        if parent.state != ElmState::Active || parent.isolated {
            authorization.blockers = ELM_POLICY_BLOCK_INVALID_STATE;
            return;
        }
        if !budget_is_subset(budget, parent.resource_budget) {
            authorization.blockers = ELM_POLICY_BLOCK_RESOURCE_QUOTA;
        }
    }

    fn cell_is_descendant_of(&self, candidate: ElmId, ancestor: ElmId) -> bool {
        let mut current = Some(candidate);
        let mut remaining = self.cells.len().saturating_add(1);
        while let Some(id) = current {
            if id == ancestor {
                return candidate != ancestor;
            }
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
            current = self
                .cells
                .iter()
                .find(|cell| cell.id == id)
                .and_then(|cell| cell.parent);
        }
        false
    }

    fn provider_ticket_owner(&self, ticket: u64) -> Option<ElmId> {
        self.provider_jobs
            .iter()
            .find(|job| job.ticket == ticket)
            .map(|job| job.consumer)
            .or_else(|| {
                self.provider_running
                    .iter()
                    .find(|running| running.job.ticket == ticket)
                    .map(|running| running.job.consumer)
            })
            .or_else(|| {
                self.provider_results
                    .iter()
                    .find(|result| result.ticket == ticket)
                    .map(|result| result.consumer)
            })
    }

    fn child_budget_allocation_fits(
        &self,
        parent: ElmId,
        replacing: ElmId,
        replacement: ElmResourceBudget,
    ) -> bool {
        let Some(parent_budget) = self
            .cells
            .iter()
            .find(|cell| cell.id == parent)
            .map(|cell| cell.resource_budget)
        else {
            return false;
        };
        let mut total = ResourceBudgetAccumulator::default();
        let parent_usage = self.cell_resource_usage(parent);
        total.add_usage(parent_usage, parent_budget.cpu_period_ns);
        total.add(replacement);
        for child in self.cells.iter().filter(|cell| {
            cell.parent == Some(parent)
                && cell.ebi_source != ElmEbiSourceKind::Builtin
                && !matches!(cell.state, ElmState::Detached | ElmState::Retired)
                && cell.id != replacing
        }) {
            total.add(child.resource_budget);
        }
        total.fits(parent_budget)
    }

    fn cell_budget_covers_usage_and_children(&self, id: ElmId, budget: ElmResourceBudget) -> bool {
        let mut total = ResourceBudgetAccumulator::default();
        total.add_usage(self.cell_resource_usage(id), budget.cpu_period_ns);
        for child in self.cells.iter().filter(|cell| {
            cell.parent == Some(id)
                && cell.ebi_source != ElmEbiSourceKind::Builtin
                && !matches!(cell.state, ElmState::Detached | ElmState::Retired)
        }) {
            total.add(child.resource_budget);
        }
        total.fits(budget)
    }

    fn prepare_image_trust(
        &mut self,
        image: &ElmEbiImage,
        source: ElmEbiSourceKind,
    ) -> Result<PreparedImageTrust, ElmEbiLoadStatus> {
        if matches!(source, ElmEbiSourceKind::Builtin | ElmEbiSourceKind::Memory) {
            return Ok(PreparedImageTrust::internal());
        }

        let fingerprint = image
            .abi_fingerprint
            .as_ref()
            .ok_or(ElmEbiLoadStatus::AbiFingerprintRejected)?;
        if fingerprint.elmapi_version != ELM_API_CURRENT_VERSION
            || fingerprint.panic_strategy != ElmPanicStrategy::AbortThroughRuntime
            || fingerprint.code_model != 1
            || fingerprint.target_features
                & (ELM_RUST_ABI_TARGET_FEATURE_FLOAT
                    | ELM_RUST_ABI_TARGET_FEATURE_VECTOR
                    | ELM_RUST_ABI_TARGET_FEATURE_SIMD)
                != 0
            || fingerprint.target_features
                & !(ELM_RUST_ABI_TARGET_FEATURE_FLOAT
                    | ELM_RUST_ABI_TARGET_FEATURE_VECTOR
                    | ELM_RUST_ABI_TARGET_FEATURE_SIMD)
                != 0
            || fingerprint.rustc_commit_hash != sha256(env!("ELM_RUSTC_VERSION").as_bytes())
            || fingerprint.target_spec_hash != expected_target_spec_hash(image.unit.target.arch)
            || fingerprint.kernel_api_hash
                != sha256(kernel_api_manifest_v1(image.unit.target.arch as u32).as_bytes())
        {
            return Err(ElmEbiLoadStatus::AbiFingerprintRejected);
        }

        let Some(proof) = image.proof.as_ref() else {
            if self.allow_unsigned_external {
                return Ok(PreparedImageTrust {
                    acceptance: None,
                    acceptance_reserved: false,
                    unsigned: true,
                    signer_key_id: [0; 32],
                    release_epoch: 0,
                });
            }
            return Err(ElmEbiLoadStatus::UntrustedImage);
        };
        let acceptance =
            self.trust_store
                .verify(image, proof, fingerprint)
                .map_err(|err| match err {
                    ElmTrustError::Rollback => ElmEbiLoadStatus::RollbackRejected,
                    _ => ElmEbiLoadStatus::UntrustedImage,
                })?;
        let acceptance_reserved = self
            .trust_store
            .reserve_acceptance(&acceptance)
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        Ok(PreparedImageTrust {
            signer_key_id: acceptance.signer_key_id(),
            release_epoch: acceptance.release_epoch(),
            acceptance: Some(acceptance),
            acceptance_reserved,
            unsigned: false,
        })
    }

    fn kernel_api_grant_approval(
        source: ElmEbiSourceKind,
        request: KernelApiGrantRequest,
        trust: &PreparedImageTrust,
    ) -> Option<super::api_registry::ApiGrantApproval> {
        if let Some(approval) = super::api_registry::ApiGrantApproval::internal(source) {
            return Some(approval);
        }
        if !request.requested || trust.unsigned || trust.acceptance.is_none() {
            return None;
        }
        super::api_registry::ApiGrantApproval::signed_projection(
            request.authority,
            request.authority_id,
            trust.signer_key_id,
        )
    }

    fn commit_image_trust(
        &mut self,
        cell: ElmId,
        trust: &PreparedImageTrust,
    ) -> Result<(), ElmTrustError> {
        self.commit_image_trust_acceptance(trust)?;
        self.apply_image_trust_metadata(cell, trust);
        Ok(())
    }

    fn commit_image_trust_acceptance(
        &mut self,
        trust: &PreparedImageTrust,
    ) -> Result<(), ElmTrustError> {
        if let Some(acceptance) = trust.acceptance.clone() {
            super::journal::append_trust_acceptance(
                acceptance.rollback_authority_id(),
                acceptance.module_digest(),
                acceptance.signer_key_id(),
                acceptance.release_epoch(),
            )
            .map_err(|_| ElmTrustError::Persistence)?;
            self.trust_store
                .accept_reserved(acceptance, trust.acceptance_reserved)?;
        }
        Ok(())
    }

    fn restore_persisted_trust_epochs(&mut self) -> Result<(), ElmTrustError> {
        let epochs = super::journal::take_replayed_trust_epochs();
        let mut restored = self.trust_store.clone();
        let result = (|| {
            for epoch in &epochs {
                if !restored
                    .anchors()
                    .iter()
                    .any(|anchor| anchor.rollback_authority_id() == epoch.rollback_authority_id)
                {
                    return Err(ElmTrustError::InvalidAnchor);
                }
                restored.try_accept(ElmTrustAcceptance::from_persisted(
                    epoch.signer_key_id,
                    epoch.rollback_authority_id,
                    epoch.module_digest,
                    epoch.release_epoch,
                )?)?;
            }
            Ok(())
        })();
        if let Err(err) = result {
            if let Err(journal_err) = super::journal::restore_replayed_trust_epochs(epochs) {
                log::error!("[elm] 无法归还未恢复的信任回放记录: {:?}", journal_err);
            }
            return Err(err);
        }
        self.trust_store = restored;
        Ok(())
    }

    fn apply_image_trust_metadata(&mut self, cell: ElmId, trust: &PreparedImageTrust) {
        if let Some(runtime) = self.cells.iter_mut().find(|runtime| runtime.id == cell) {
            runtime.trust_unsigned = trust.unsigned;
            runtime.signer_key_id = trust.signer_key_id;
            runtime.release_epoch = trust.release_epoch;
        }
    }

    fn abort_image_trust(&mut self, trust: &PreparedImageTrust) {
        if let Err(err) = self
            .trust_store
            .cancel_acceptance_reservation(trust.acceptance_reserved)
        {
            log::error!(
                "[elm] trust acceptance reservation cleanup failed: {:?}",
                err
            );
        }
    }

    fn grant_management_to_loaded_cell(&mut self, id: ElmId, trust: &PreparedImageTrust) -> bool {
        if trust.unsigned || trust.acceptance.is_none() {
            return false;
        }
        let Some(index) = self.cell_index(id) else {
            return false;
        };
        let Some(parent) = self.cells[index].parent else {
            return false;
        };
        let parent_allows_management = self.cells.iter().any(|cell| {
            cell.id == parent
                && cell.cell_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT != 0
        });
        if self.cells[index].kind != ElmKind::Manager || !parent_allows_management {
            return false;
        }
        self.cells[index].cell_policy.allowed_actions |= ELM_CELL_POLICY_ALLOW_MANAGEMENT;
        true
    }

    pub fn init_builtin_mgr(&mut self) -> Result<(), ElmError> {
        if self.initialized {
            return Ok(());
        }
        super::register_kernel_api_namespace(&ELM_RUNTIME_NAMESPACE_V1)
            .map_err(|_| ElmError::InvalidTransition)?;
        super::register_kernel_api_namespace(&ELM_MANAGEMENT_NAMESPACE_V1)
            .map_err(|_| ElmError::InvalidTransition)?;
        if !super::resource_accounting::init()
            || !super::resource_accounting::register_cell(ELM_MGR_ID, ElmResourceBudget::ROOT)
        {
            return Err(ElmError::LeaseBusy);
        }
        if !super::owned_resource::register_owner(ELM_MGR_ID, Generation::FIRST) {
            let _ = super::resource_accounting::retire_cell(ELM_MGR_ID);
            return Err(ElmError::LeaseBusy);
        }
        if !super::resource_accounting::register_cell(ELM_EKI_ID, ElmResourceBudget::DEFAULT) {
            let _ = super::owned_resource::retire_owner(ELM_MGR_ID, Generation::FIRST);
            let _ = super::resource_accounting::retire_cell(ELM_MGR_ID);
            return Err(ElmError::LeaseBusy);
        }
        if !super::owned_resource::register_owner(ELM_EKI_ID, Generation::FIRST) {
            let _ = super::resource_accounting::retire_cell(ELM_EKI_ID);
            let _ = super::owned_resource::retire_owner(ELM_MGR_ID, Generation::FIRST);
            let _ = super::resource_accounting::retire_cell(ELM_MGR_ID);
            return Err(ElmError::LeaseBusy);
        }

        self.restore_persisted_trust_epochs()
            .map_err(|_| ElmError::InvalidTransition)?;
        self.trust_store.seal();

        let manifest = ElmManifest::new(
            ElmName::new("elm-mgr")?,
            ElmVersion::new("0.1.0")?,
            ElmKind::Manager,
        )
        .with_offer(NexusOffer::new(
            FlowContract::new("mgr.menu.item@1")?,
            FlowMode::Ordered,
        ));
        let unit = ElmEbiUnit::new(manifest, ElmEbiTarget::new(ElmEbiArch::Any))
            .with_extension_point(
                ElmEbiExtensionPointDecl::new("menu.item", "mgr.menu.item@1")
                    .map_err(|_| ElmError::InvalidTransition)?,
            )
            .with_lifecycle_hooks(ElmEbiLifecycleHooks::rust_context_result_v1());
        unit.validate(ElmEbiArch::Any)
            .map_err(|_| ElmError::InvalidTransition)?;
        self.graph.insert_cell(ELM_MGR_ID, unit.manifest.clone())?;
        for point in &unit.extension_points {
            self.graph.add_extension_point_with_mode(
                ELM_MGR_ID,
                point.point.clone(),
                point.contract.clone(),
                point.mode,
            )?;
        }
        self.cells.push(CellRuntime {
            id: ELM_MGR_ID,
            parent: None,
            state: ElmState::Active,
            kind: unit.manifest.kind,
            generation: Generation::FIRST,
            name: unit.manifest.name.as_str().to_string(),
            ebi_source: ElmEbiSourceKind::Builtin,
            ebi_arch: unit.target.arch,
            ebi_status: ElmEbiLoadStatus::Ok,
            has_native_code: false,
            native_segment_count: unit.segments.len() as u16,
            native_import_count: unit.imports.len() as u16,
            native_export_count: unit.exports.len() as u16,
            elmapi_version: ELM_API_CURRENT_VERSION,
            lifecycle_hooks_declared: unit.lifecycle_hooks.is_some(),
            lifecycle_executor_ready: true,
            lifecycle_initialized: true,
            lifecycle_finalized: false,
            resource_budget: ElmResourceBudget::ROOT,
            cell_policy: ElmCellPolicyV1::new(
                ELM_MGR_ID.0,
                Generation::FIRST.0,
                ELM_CELL_POLICY_ALLOW_ALL | ELM_CELL_POLICY_ALLOW_MANAGEMENT,
                ELM_MGR_STATUS_OK,
                0,
            ),
            policy_epoch: 1,
            active_executions: 0,
            exclusive_execution: false,
            native_faults: 0,
            isolated: false,
            isolation_blocker: 0,
            trust_unsigned: false,
            signer_key_id: [0; 32],
            release_epoch: 0,
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        let eki_manifest = ElmManifest::new(
            ElmName::new("eki")?,
            ElmVersion::new("0.1.0")?,
            ElmKind::Service,
        );
        let eki_unit = ElmEbiUnit::new(eki_manifest, ElmEbiTarget::new(ElmEbiArch::Any))
            .with_lifecycle_hooks(ElmEbiLifecycleHooks::rust_context_result_v1());
        eki_unit
            .validate(ElmEbiArch::Any)
            .map_err(|_| ElmError::InvalidTransition)?;
        self.graph
            .insert_cell(ELM_EKI_ID, eki_unit.manifest.clone())?;
        self.graph.set_parent(ELM_EKI_ID, ELM_MGR_ID)?;
        self.cells.push(CellRuntime {
            id: ELM_EKI_ID,
            parent: Some(ELM_MGR_ID),
            state: ElmState::Active,
            kind: eki_unit.manifest.kind,
            generation: Generation::FIRST,
            name: eki_unit.manifest.name.as_str().to_string(),
            ebi_source: ElmEbiSourceKind::Builtin,
            ebi_arch: eki_unit.target.arch,
            ebi_status: ElmEbiLoadStatus::Ok,
            has_native_code: false,
            native_segment_count: eki_unit.segments.len() as u16,
            native_import_count: eki_unit.imports.len() as u16,
            native_export_count: eki_unit.exports.len() as u16,
            elmapi_version: 0,
            lifecycle_hooks_declared: eki_unit.lifecycle_hooks.is_some(),
            lifecycle_executor_ready: true,
            lifecycle_initialized: true,
            lifecycle_finalized: false,
            resource_budget: ElmResourceBudget::DEFAULT,
            cell_policy: ElmCellPolicyV1::new(
                ELM_EKI_ID.0,
                Generation::FIRST.0,
                ELM_CELL_POLICY_ALLOW_ALL,
                ELM_MGR_STATUS_OK,
                0,
            ),
            policy_epoch: 1,
            active_executions: 0,
            exclusive_execution: false,
            native_faults: 0,
            isolated: false,
            isolation_blocker: 0,
            trust_unsigned: false,
            signer_key_id: [0; 32],
            release_epoch: 0,
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        super::source::register_builtin_eki_projection_source()
            .map_err(|_| ElmError::InvalidTransition)?;
        self.emit(TopologyEventKind::CellAdded, Some(ELM_MGR_ID));
        self.emit(TopologyEventKind::CellStateChanged, Some(ELM_MGR_ID));
        self.emit(TopologyEventKind::CellAdded, Some(ELM_EKI_ID));
        self.emit(TopologyEventKind::CellStateChanged, Some(ELM_EKI_ID));
        self.register_builtin_ports();
        self.register_builtin_mgr_actions()?;
        self.register_builtin_mgr_api()?;
        self.register_builtin_native_exports()?;
        self.initialized = true;
        self.push_journal_trace(
            ELM_MGR_ID,
            ELM_MGR_ACTION_PROVIDER_QUERY,
            ELM_MGR_STATUS_OK,
            0,
        );
        if let Err(err) = super::journal::append(
            ELM_MGR_ACTION_PROVIDER_QUERY,
            ELM_MGR_STATUS_OK,
            ELM_MGR_ID.0,
            ELM_MGR_ID.0,
            ELM_EKI_ID.0,
            self.cells.len() as u64,
            0,
            0,
        ) {
            log::error!("[elm] 启动日志持久化失败: {:?}", err);
        }
        log::info!("[elm] Core initialized with builtin elm-mgr");
        Ok(())
    }

    pub fn core_info(&self) -> ElmCoreInfo {
        ElmCoreInfo::new(
            self.cells.len() as u32,
            self.ports.len() as u32,
            self.lease_count() as u32,
            self.last_event_sequence(),
        )
    }

    pub fn cells(&self) -> &[CellRuntime] {
        &self.cells
    }

    pub fn ports(&self) -> &[PortRuntime] {
        &self.ports
    }

    pub fn menu_items(&self) -> &[MenuItemRuntime] {
        &self.menu_items
    }

    pub fn menu_generation(&self) -> Generation {
        self.menu_generation
    }

    pub fn lease_count(&self) -> usize {
        self.leases.len()
    }

    pub fn last_event_sequence(&self) -> u64 {
        self.events.last().map(|event| event.sequence).unwrap_or(0)
    }

    pub fn read_next_event(&self) -> Option<ElmEventRecord> {
        self.events
            .iter()
            .find(|event| event.sequence > self.acknowledged_event_sequence)
            .copied()
    }

    pub fn ack_event(&mut self, sequence: u64) {
        self.acknowledged_event_sequence = self.acknowledged_event_sequence.max(sequence);
    }

    pub fn policy_info(&self) -> ElmMgrPolicyInfo {
        ElmMgrPolicyInfo::new(AUDIT_RING_LIMIT as u32)
    }

    pub fn fault_dump_bytes(&self) -> Vec<u8> {
        let available = general::elm_guard::fault_snapshot_count();
        let mut snapshots = Vec::new();
        let mut allocation_dropped = 0u32;
        if snapshots.try_reserve_exact(available).is_ok() {
            general::elm_guard::visit_fault_snapshots(|snapshot| snapshots.push(snapshot));
            snapshots.sort_unstable_by_key(|snapshot| snapshot.sequence);
        } else {
            allocation_dropped = available.min(u32::MAX as usize) as u32;
        }
        let header_size = core::mem::size_of::<ElmFaultDumpHeader>();
        let record_size = core::mem::size_of::<ElmFaultDumpRecord>();
        let max_records = ELM_MGR_MAX_PAYLOAD.saturating_sub(header_size) / record_size;
        let skipped = snapshots.len().saturating_sub(max_records);
        let emitted = &snapshots[skipped..];
        let dropped = general::elm_guard::dropped_fault_snapshot_count()
            .saturating_add(skipped as u64)
            .saturating_add(u64::from(allocation_dropped))
            .min(u64::from(u32::MAX)) as u32;
        let header = ElmFaultDumpHeader::new(
            emitted.len() as u32,
            dropped,
            snapshots.last().map(|fault| fault.sequence).unwrap_or(0),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for fault in emitted {
            let record = ElmFaultDumpRecord::new(
                fault.sequence,
                fault.cell,
                fault.phase,
                fault.pc as u64,
                fault.addr as u64,
                fault.code as u32,
                fault.return_pc as u64,
                fault.return_sp as u64,
                fault.cpu_id,
                fault.depth,
                fault.reason as u32,
            );
            push_plain(&mut out, &record);
        }
        out
    }

    pub fn lifecycle_trace_bytes(&self) -> Vec<u8> {
        trace_bytes(
            &self.lifecycle_traces,
            self.dropped_lifecycle_trace_count,
            ELM_RUNTIME_TRACE_KIND_LIFECYCLE,
        )
    }

    pub fn provider_call_trace_bytes(&self) -> Vec<u8> {
        trace_bytes(
            &self.provider_call_traces,
            self.dropped_provider_call_trace_count,
            ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL,
        )
    }

    pub fn mixin_trace_bytes(&self) -> Vec<u8> {
        trace_bytes(
            &self.mixin_traces,
            self.dropped_mixin_trace_count,
            ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH,
        )
    }

    pub fn replace_trace_bytes(&self) -> Vec<u8> {
        trace_bytes(
            &self.replace_traces,
            self.dropped_replace_trace_count,
            ELM_RUNTIME_TRACE_KIND_REPLACE,
        )
    }

    pub fn policy_trace_bytes(&self) -> Vec<u8> {
        trace_bytes(
            &self.policy_traces,
            self.dropped_policy_trace_count,
            ELM_RUNTIME_TRACE_KIND_POLICY,
        )
    }

    pub fn resource_diagnostics_bytes(&self) -> Vec<u8> {
        trace_bytes(
            &self.resource_traces,
            self.dropped_resource_trace_count,
            ELM_RUNTIME_TRACE_KIND_RESOURCE,
        )
    }

    pub fn runtime_journal_bytes(&self) -> Vec<u8> {
        let journal_info = super::journal::runtime_info();
        let records: Vec<_> = super::journal::records()
            .into_iter()
            .map(|record| {
                ElmRuntimeTraceRecord::new(
                    record.sequence,
                    record.timestamp_ns,
                    ELM_RUNTIME_TRACE_KIND_JOURNAL,
                    record.action,
                    record.status,
                    record.cell,
                    record.subject,
                    record.aux,
                    record.value,
                    record.blockers,
                )
            })
            .collect();
        trace_bytes(
            &records,
            journal_info.dropped_records,
            ELM_RUNTIME_TRACE_KIND_JOURNAL,
        )
    }

    pub fn query_cell_policy(&self, request: ElmCellPolicyRequest) -> ElmCellPolicyV1 {
        if request.flags != 0 || request.reserved != 0 || request.cell_id == 0 {
            return ElmCellPolicyV1::new(
                request.cell_id,
                0,
                0,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }
        self.cells
            .iter()
            .find(|cell| cell.id.0 == request.cell_id)
            .map(|cell| cell.cell_policy)
            .unwrap_or_else(|| {
                ElmCellPolicyV1::new(
                    request.cell_id,
                    0,
                    0,
                    ELM_MGR_STATUS_NOT_FOUND,
                    ELM_POLICY_BLOCK_CELL_NOT_FOUND,
                )
            })
    }

    pub fn update_cell_policy(&mut self, mut policy: ElmCellPolicyV1) -> ElmCellPolicyV1 {
        if policy.cell_id == 0
            || policy.reserved != 0
            || policy.status != ELM_MGR_STATUS_OK
            || policy.blockers != 0
            || policy.flags & !ELM_CELL_POLICY_FLAGS_MASK != 0
            || policy.allowed_actions & !ELM_CELL_POLICY_ALLOWED_ACTIONS_MASK != 0
            || policy.provider_flags & !ELM_PROVIDER_POLICY_ALL != 0
            || policy.extension_flags & !ELM_EXTENSION_POLICY_ALL != 0
            || policy.native_flags & !ELM_NATIVE_POLICY_ALL != 0
            || policy.resource_flags & !ELM_RESOURCE_POLICY_ALL != 0
        {
            policy.status = ELM_MGR_STATUS_INVALID;
            policy.blockers = ELM_POLICY_BLOCK_INVALID_STATE;
            return policy;
        }
        let Some(index) = self
            .cells
            .iter()
            .position(|cell| cell.id.0 == policy.cell_id)
        else {
            policy.status = ELM_MGR_STATUS_NOT_FOUND;
            policy.blockers = ELM_POLICY_BLOCK_CELL_NOT_FOUND;
            return policy;
        };
        if self.is_builtin_cell(ElmId(policy.cell_id)) {
            policy.status = ELM_MGR_STATUS_PERMISSION;
            policy.blockers = ELM_POLICY_BLOCK_BUILTIN_PROTECTED;
            self.push_policy_trace(policy.cell_id, 0, policy.status, policy.blockers);
            return policy;
        }
        if self.cells[index].cell_policy.flags & ELM_CELL_POLICY_FLAG_LOCKED != 0 {
            policy.status = ELM_MGR_STATUS_PERMISSION;
            policy.blockers = ELM_POLICY_BLOCK_CAPABILITY_DENIED;
            self.push_policy_trace(policy.cell_id, 0, policy.status, policy.blockers);
            return policy;
        }
        if policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT
            != self.cells[index].cell_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT
        {
            policy.status = ELM_MGR_STATUS_PERMISSION;
            policy.blockers = ELM_POLICY_BLOCK_POLICY_ESCALATION;
            self.push_policy_trace(policy.cell_id, 0, policy.status, policy.blockers);
            return policy;
        }
        if policy.generation != self.cells[index].generation.0
            || policy.policy_epoch != self.cells[index].policy_epoch
        {
            policy.status = ELM_MGR_STATUS_PERMISSION;
            policy.blockers = ELM_POLICY_BLOCK_CALLER_STALE;
            self.push_policy_trace(policy.cell_id, 0, policy.status, policy.blockers);
            return policy;
        }
        if self.cells[index].active_executions != 0 {
            policy.status = ELM_MGR_STATUS_BUSY;
            policy.blockers = ELM_POLICY_BLOCK_PROVIDER_BUSY;
            self.push_policy_trace(policy.cell_id, 0, policy.status, policy.blockers);
            return policy;
        }
        if !self.policy_update_respects_hierarchy(ElmId(policy.cell_id), policy) {
            policy.status = ELM_MGR_STATUS_PERMISSION;
            policy.blockers = ELM_POLICY_BLOCK_POLICY_ESCALATION;
            self.push_policy_trace(policy.cell_id, 0, policy.status, policy.blockers);
            return policy;
        }
        let Some(next_policy_epoch) = self.cells[index].policy_epoch.checked_add(1) else {
            policy.status = ELM_MGR_STATUS_BUSY;
            policy.blockers = ELM_POLICY_BLOCK_RESOURCE_QUOTA;
            self.push_policy_trace(policy.cell_id, 0, policy.status, policy.blockers);
            return policy;
        };
        policy.generation = self.cells[index].generation.0;
        policy.policy_epoch = next_policy_epoch;
        policy.status = ELM_MGR_STATUS_OK;
        policy.blockers = 0;
        self.cells[index].cell_policy = policy;
        self.cells[index].policy_epoch = policy.policy_epoch;
        self.push_policy_trace(
            policy.cell_id,
            policy.allowed_actions as u64,
            policy.status,
            0,
        );
        policy
    }

    pub fn query_resource_budget(
        &self,
        request: ElmResourceBudgetRequest,
    ) -> ElmResourceBudgetResponse {
        if request.flags != 0 || request.reserved != 0 || request.cell_id == 0 {
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
                ElmResourceBudget::DEFAULT,
                ElmResourceUsage::default(),
            );
        }
        let id = ElmId(request.cell_id);
        if self.cell_state(id).is_none() {
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_CELL_NOT_FOUND,
                ElmResourceBudget::DEFAULT,
                ElmResourceUsage::default(),
            );
        }
        ElmResourceBudgetResponse::new(
            request.cell_id,
            ELM_MGR_STATUS_OK,
            0,
            self.cell_resource_budget(id),
            self.cell_resource_usage(id),
        )
    }

    pub fn update_resource_budget(
        &mut self,
        request: ElmResourceBudgetUpdateRequest,
    ) -> ElmResourceBudgetResponse {
        if request.flags != 0
            || request.reserved != 0
            || request.cell_id == 0
            || !super::resource_accounting::budget_is_valid(request.budget)
        {
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
                request.budget,
                ElmResourceUsage::default(),
            );
        }
        let id = ElmId(request.cell_id);
        let Some(index) = self.cell_index(id) else {
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_CELL_NOT_FOUND,
                request.budget,
                ElmResourceUsage::default(),
            );
        };
        if self.is_builtin_cell(id) {
            let usage = self.cell_resource_usage(id);
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_PERMISSION,
                ELM_POLICY_BLOCK_BUILTIN_PROTECTED,
                self.cells[index].resource_budget,
                usage,
            );
        }
        if self.cells[index].active_executions != 0 {
            let usage = self.cell_resource_usage(id);
            self.push_resource_trace(id.0, 0, ELM_MGR_STATUS_BUSY, ELM_POLICY_BLOCK_PROVIDER_BUSY);
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_PROVIDER_BUSY,
                self.cells[index].resource_budget,
                usage,
            );
        }
        let usage = self.cell_resource_usage(id);
        let parent_allocation_fits = self.cells[index]
            .parent
            .is_none_or(|parent| self.child_budget_allocation_fits(parent, id, request.budget));
        if !self.cell_budget_covers_usage_and_children(id, request.budget)
            || !parent_allocation_fits
        {
            self.push_resource_trace(
                id.0,
                0,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                self.cells[index].resource_budget,
                usage,
            );
        }
        if !super::resource_accounting::update_budget(id, request.budget) {
            self.push_resource_trace(
                id.0,
                0,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
            return ElmResourceBudgetResponse::new(
                request.cell_id,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                self.cells[index].resource_budget,
                usage,
            );
        }
        self.cells[index].resource_budget = request.budget;
        self.push_resource_trace(id.0, 0, ELM_MGR_STATUS_OK, 0);
        ElmResourceBudgetResponse::new(request.cell_id, ELM_MGR_STATUS_OK, 0, request.budget, usage)
    }

    pub fn native_capabilities_bytes(&self) -> Vec<u8> {
        let total_records = self
            .native_exports
            .len()
            .saturating_add(self.native_imports.len());
        let header_size = core::mem::size_of::<ElmNativeCapabilityHeader>();
        let record_size = core::mem::size_of::<ElmNativeCapabilityRecord>();
        let max_records = ELM_MGR_MAX_PAYLOAD
            .saturating_sub(header_size)
            .checked_div(record_size)
            .unwrap_or(0);
        let emitted_records = total_records.min(max_records);
        let flags = if emitted_records < total_records {
            ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED
        } else {
            0
        };
        let header = ElmNativeCapabilityHeader::new(
            emitted_records as u32,
            flags,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        let mut emitted = 0usize;
        for export in &self.native_exports {
            if emitted >= emitted_records {
                break;
            }
            let record = ElmNativeCapabilityRecord::new(
                ELM_NATIVE_CAPABILITY_KIND_EXPORT,
                ELM_MGR_STATUS_OK,
                export.owner.0,
                0,
                export.version,
                export.version,
                0,
                &export.name,
                export.contract.as_str(),
            );
            push_plain(&mut out, &record);
            emitted += 1;
        }
        for import in &self.native_imports {
            if emitted >= emitted_records {
                break;
            }
            let flags = if import.min_version != import.max_version {
                ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD
            } else {
                0
            };
            let record = ElmNativeCapabilityRecord::new(
                ELM_NATIVE_CAPABILITY_KIND_IMPORT,
                ELM_MGR_STATUS_OK,
                import.owner.0,
                import.provider.0,
                import.min_version,
                import.selected_version,
                flags,
                &import.name,
                import.contract.as_str(),
            );
            push_plain(&mut out, &record);
            emitted += 1;
        }
        out
    }

    pub fn todo_registry_bytes(&self) -> Vec<u8> {
        let records = self.todo_registry_records();
        let header_size = core::mem::size_of::<ElmTodoRegistryHeader>();
        let record_size = core::mem::size_of::<ElmTodoRegistryRecord>();
        let max_records = ELM_MGR_MAX_PAYLOAD
            .saturating_sub(header_size)
            .checked_div(record_size)
            .unwrap_or(0);
        let emitted_records = records.len().min(max_records);
        let active_count = records
            .iter()
            .take(emitted_records)
            .filter(|record| record.flags & ELM_TODO_FLAG_ACTIVE != 0)
            .count() as u32;
        let flags = if emitted_records < records.len() {
            ELM_TODO_REGISTRY_FLAG_TRUNCATED
        } else {
            0
        };
        let header = ElmTodoRegistryHeader::new_with_flags(
            emitted_records as u32,
            active_count,
            flags,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for record in records.iter().take(emitted_records) {
            push_plain(&mut out, record);
        }
        out
    }

    pub fn api_registry_bytes(&self) -> Vec<u8> {
        let header = ElmMgrApiRegistryHeader::new(
            self.mgr_runtime.api_registry.len() as u32,
            self.mgr_runtime.api_generation.0,
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for descriptor in &self.mgr_runtime.api_registry {
            push_plain(&mut out, descriptor);
        }
        out
    }

    pub fn subscribe_event(
        &mut self,
        request: ElmMgrEventSubscribeRequest,
    ) -> ElmMgrEventSubscribeResponse {
        let owner = ElmId(request.owner_cell_id);
        if request.flags != 0 {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                self.last_event_sequence(),
                ELM_MGR_STATUS_INVALID,
                0,
            );
        }
        let Some(cell) = self.cells.iter().find(|cell| cell.id == owner) else {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                self.last_event_sequence(),
                ELM_MGR_STATUS_NOT_FOUND,
                0,
            );
        };
        let owner_generation = cell.generation;
        let owner_state = cell.state;
        if !self.cell_policy_allows(owner, ELM_CELL_POLICY_ALLOW_EVENT) {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                self.last_event_sequence(),
                ELM_MGR_STATUS_PERMISSION,
                ELM_POLICY_BLOCK_CAPABILITY_DENIED,
            );
        }
        if self.cell_resource_over_quota(owner, ElmResourceKind::EventSubscription) {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                self.last_event_sequence(),
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
        }
        if self.mgr_runtime.event_subscriptions.len() >= EVENT_SUBSCRIPTION_LIMIT {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                self.last_event_sequence(),
                ELM_MGR_STATUS_BUSY,
                0,
            );
        }

        if self.mgr_runtime.event_subscriptions.try_reserve(1).is_err() {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                self.last_event_sequence(),
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
        }
        let (Some(subscription), Some(lease)) = (
            self.mgr_runtime.alloc_event_subscription_id(),
            self.alloc_lease_id(),
        ) else {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                self.last_event_sequence(),
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
        };
        let cursor = self.last_event_sequence();
        if self
            .leases
            .insert(ResourceLease::new(
                lease,
                owner,
                LeaseKind::EventSubscription,
                LeaseRights::READ,
                owner_generation,
            ))
            .is_err()
        {
            return ElmMgrEventSubscribeResponse::new(
                0,
                0,
                request.owner_cell_id,
                cursor,
                ELM_MGR_STATUS_INVALID,
                0,
            );
        }
        self.mgr_runtime
            .event_subscriptions
            .push(EventSubscriptionRuntime {
                subscription,
                owner,
                lease,
                cursor,
                kind_filter: request.kind_filter,
                cell_filter: request.cell_filter,
                port_filter: request.port_filter,
                binding_filter: request.binding_filter,
                lease_filter: request.lease_filter,
                delivered_events: 0,
                dropped_events: 0,
            });
        self.emit_lease(TopologyEventKind::LeaseAdded, lease);
        self.record_mgr_audit(
            ELM_MGR_ACTION_EVENT_SUBSCRIBE,
            owner,
            0,
            state_code(owner_state),
        );
        ElmMgrEventSubscribeResponse::new(
            subscription,
            lease.0,
            owner.0,
            cursor,
            ELM_MGR_STATUS_OK,
            0,
        )
    }

    pub fn unsubscribe_event(
        &mut self,
        request: ElmMgrEventUnsubscribeRequest,
    ) -> ElmMgrEventUnsubscribeResponse {
        if request.flags != 0 || request.reserved != 0 || request.subscription_id == 0 {
            return ElmMgrEventUnsubscribeResponse::new(
                request.subscription_id,
                0,
                request.owner_cell_id,
                ELM_MGR_STATUS_INVALID,
                false,
                0,
                0,
            );
        }
        let Some(index) = self
            .mgr_runtime
            .event_subscription_index(request.subscription_id)
        else {
            return ElmMgrEventUnsubscribeResponse::new(
                request.subscription_id,
                0,
                request.owner_cell_id,
                ELM_MGR_STATUS_NOT_FOUND,
                false,
                0,
                0,
            );
        };
        let subscription = self.mgr_runtime.event_subscriptions[index].clone();
        if request.owner_cell_id != 0 && request.owner_cell_id != subscription.owner.0 {
            return ElmMgrEventUnsubscribeResponse::new(
                request.subscription_id,
                subscription.lease.0,
                subscription.owner.0,
                ELM_MGR_STATUS_INVALID,
                false,
                subscription.delivered_events,
                subscription.dropped_events,
            );
        }
        self.mgr_runtime.event_subscriptions.remove(index);
        let revoked = self.leases.revoke_and_remove(subscription.lease).is_ok();
        if revoked {
            self.emit_lease(TopologyEventKind::LeaseRevoked, subscription.lease);
        }
        self.record_mgr_audit(
            ELM_MGR_ACTION_EVENT_UNSUBSCRIBE,
            subscription.owner,
            0,
            self.cell_state(subscription.owner)
                .map(state_code)
                .unwrap_or(0),
        );
        ElmMgrEventUnsubscribeResponse::new(
            subscription.subscription,
            subscription.lease.0,
            subscription.owner.0,
            ELM_MGR_STATUS_OK,
            revoked,
            subscription.delivered_events,
            subscription.dropped_events,
        )
    }

    pub fn event_subscriptions_bytes(&self) -> Vec<u8> {
        let header = ElmMgrEventSubscriptionHeader::new(
            self.mgr_runtime.event_subscriptions.len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for subscription in &self.mgr_runtime.event_subscriptions {
            push_plain(&mut out, &subscription.record());
        }
        out
    }

    pub fn read_subscribed_events(
        &mut self,
        request: ElmMgrSubscribedEventReadRequest,
    ) -> Result<Vec<u8>, i32> {
        if request.subscription_id == 0 || request.flags & !ELM_MGR_EVENT_READ_FLAG_ADVANCE != 0 {
            return Err(ELM_MGR_STATUS_INVALID);
        }
        let Some(index) = self
            .mgr_runtime
            .event_subscription_index(request.subscription_id)
        else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        let max_records = normalize_event_read_limit(request.max_records);
        let mut cursor = if request.cursor == 0 {
            self.mgr_runtime.event_subscriptions[index].cursor
        } else {
            request.cursor
        };
        let mut dropped = 0;
        recover_stale_subscription_cursor(&self.events, &mut cursor, &mut dropped);
        let subscription = self.mgr_runtime.event_subscriptions[index].clone();
        let mut records = Vec::new();
        let mut next_cursor = cursor;
        for event in self.events.iter().filter(|event| event.sequence > cursor) {
            next_cursor = event.sequence;
            if !subscription.matches(event) {
                continue;
            }
            records.push(*event);
            if records.len() >= max_records {
                break;
            }
        }
        let record_count = records.len() as u32;
        let advance = request.flags & ELM_MGR_EVENT_READ_FLAG_ADVANCE != 0;
        let (owner, dropped_events) = {
            let subscription = &mut self.mgr_runtime.event_subscriptions[index];
            if advance {
                subscription.cursor = next_cursor;
            }
            subscription.delivered_events = subscription
                .delivered_events
                .saturating_add(record_count as u64);
            subscription.dropped_events = subscription.dropped_events.saturating_add(dropped);
            (subscription.owner, subscription.dropped_events)
        };

        let header = ElmMgrSubscribedEventReadHeader::new(
            record_count,
            ELM_MGR_STATUS_OK,
            request.flags & ELM_MGR_EVENT_READ_FLAG_ADVANCE,
            request.subscription_id,
            cursor,
            next_cursor,
            dropped_events,
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for event in &records {
            push_plain(&mut out, event);
        }
        self.record_mgr_audit(
            ELM_MGR_ACTION_EVENT_READ,
            owner,
            0,
            self.cell_state(owner).map(state_code).unwrap_or(0),
        );
        Ok(out)
    }

    pub fn topology_bytes(&self) -> Vec<u8> {
        let mut records = Vec::new();
        for edge in self.graph.parent_edges() {
            records.push(ElmMgrRelationRecord::new(
                ElmMgrRelationKind::Parent,
                edge.child.0,
                edge.parent.0,
                "",
                "",
            ));
        }
        for edge in self.graph.dependencies() {
            records.push(ElmMgrRelationRecord::new(
                ElmMgrRelationKind::Dependency,
                edge.consumer.0,
                edge.provider.0,
                edge.contract.as_str(),
                "",
            ));
        }
        for point in self.graph.extension_points() {
            records.push(ElmMgrRelationRecord::new(
                ElmMgrRelationKind::ExtensionPoint,
                point.owner.0,
                0,
                point.contract.as_str(),
                &point.name,
            ));
        }
        for edge in self.graph.extensions() {
            records.push(ElmMgrRelationRecord::new(
                ElmMgrRelationKind::Extension,
                edge.extension.0,
                edge.target.0,
                edge.contract.as_str(),
                &edge.point,
            ));
        }

        let header = ElmMgrTopologyHeader::new(
            records.len() as u32,
            self.cells.len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for record in records {
            push_plain(&mut out, &record);
        }
        out
    }

    pub fn extensions_bytes(&self) -> Vec<u8> {
        let points = self.graph.extension_points();
        let edges = self.graph.extensions();
        let header = ElmExtensionSnapshotHeader::new(
            points.len() as u32,
            edges.len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for point in points {
            push_plain(
                &mut out,
                &ElmExtensionSnapshotRecord::point_with_mode(
                    point.owner.0,
                    &point.name,
                    point.contract.as_str(),
                    point.mode,
                ),
            );
        }
        for edge in edges {
            let mode = self
                .graph
                .extension_point(edge.target, &edge.point)
                .map(|point| point.mode)
                .unwrap_or(ElmMixinMode::Chain);
            push_plain(
                &mut out,
                &ElmExtensionSnapshotRecord::edge_with_dispatch(
                    edge.extension.0,
                    edge.target.0,
                    &edge.point,
                    edge.contract.as_str(),
                    edge.handler_contract.as_str(),
                    edge.priority,
                    mode,
                ),
            );
        }
        out
    }

    pub fn preflight_extension_attach(
        &self,
        request: ElmExtensionAttachRequest,
    ) -> ElmExtensionAttachResponse {
        let (status, blockers, generation) = self.extension_attach_plan(request);
        ElmExtensionAttachResponse::new(
            request.extension_cell_id,
            request.target_cell_id,
            generation,
            blockers == 0,
            status,
            blockers,
        )
    }

    pub fn commit_extension_attach(
        &mut self,
        request: ElmExtensionAttachRequest,
    ) -> ElmExtensionAttachResponse {
        let (status, blockers, generation) = self.extension_attach_plan(request);
        if blockers != 0 {
            self.record_mgr_audit(
                ELM_MGR_ACTION_EXTENSION_ATTACH,
                ElmId(request.extension_cell_id),
                blockers,
                self.cell_state(ElmId(request.extension_cell_id))
                    .map(state_code)
                    .unwrap_or(0),
            );
            return ElmExtensionAttachResponse::new(
                request.extension_cell_id,
                request.target_cell_id,
                generation,
                false,
                status,
                blockers,
            );
        }
        let (Some(point), Some(contract), Some(handler_contract)) = (
            extension_request_point(&request),
            extension_request_contract(&request),
            extension_request_handler_contract(&request),
        ) else {
            return ElmExtensionAttachResponse::new(
                request.extension_cell_id,
                request.target_cell_id,
                generation,
                false,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            );
        };
        let Ok(contract) = FlowContract::new(contract) else {
            return ElmExtensionAttachResponse::new(
                request.extension_cell_id,
                request.target_cell_id,
                generation,
                false,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            );
        };
        let Ok(handler_contract) = FlowContract::new(handler_contract) else {
            return ElmExtensionAttachResponse::new(
                request.extension_cell_id,
                request.target_cell_id,
                generation,
                false,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            );
        };
        match self.graph.add_extension_with_dispatch(
            ElmId(request.extension_cell_id),
            ElmId(request.target_cell_id),
            point.to_string(),
            contract,
            handler_contract,
            request.priority,
        ) {
            Ok(()) => {
                self.emit(
                    TopologyEventKind::CellStateChanged,
                    Some(ElmId(request.extension_cell_id)),
                );
                self.record_mgr_audit(
                    ELM_MGR_ACTION_EXTENSION_ATTACH,
                    ElmId(request.extension_cell_id),
                    0,
                    self.cell_state(ElmId(request.extension_cell_id))
                        .map(state_code)
                        .unwrap_or(0),
                );
                ElmExtensionAttachResponse::new(
                    request.extension_cell_id,
                    request.target_cell_id,
                    generation,
                    true,
                    ELM_MGR_STATUS_OK,
                    0,
                )
            }
            Err(_) => {
                let blockers = ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
                self.record_mgr_audit(
                    ELM_MGR_ACTION_EXTENSION_ATTACH,
                    ElmId(request.extension_cell_id),
                    blockers,
                    self.cell_state(ElmId(request.extension_cell_id))
                        .map(state_code)
                        .unwrap_or(0),
                );
                ElmExtensionAttachResponse::new(
                    request.extension_cell_id,
                    request.target_cell_id,
                    generation,
                    false,
                    status_from_blockers(blockers),
                    blockers,
                )
            }
        }
    }

    pub fn commit_extension_detach(
        &mut self,
        request: ElmExtensionDetachRequest,
    ) -> ElmExtensionDetachResponse {
        let extension = ElmId(request.extension_cell_id);
        let target = ElmId(request.target_cell_id);
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == extension)
            .map(|cell| cell.generation.0)
            .unwrap_or(0);
        let mut blockers = 0;
        let point = extension_detach_request_point(&request);
        if request.flags != 0 || point.is_none() {
            blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
        }
        if self.cell_state(extension).is_none() || self.cell_state(target).is_none() {
            blockers |= ELM_POLICY_BLOCK_CELL_NOT_FOUND;
        }
        if blockers == 0
            && self
                .graph
                .remove_extension(extension, target, point.unwrap_or(""))
                .is_none()
        {
            blockers |= ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND;
        }
        self.record_mgr_audit(
            ELM_MGR_ACTION_EXTENSION_DETACH,
            extension,
            blockers,
            self.cell_state(extension).map(state_code).unwrap_or(0),
        );
        if blockers == 0 {
            self.emit(TopologyEventKind::CellStateChanged, Some(extension));
        }
        ElmExtensionDetachResponse::new(
            request.extension_cell_id,
            request.target_cell_id,
            generation,
            blockers == 0,
            status_from_blockers(blockers),
            blockers,
        )
    }

    pub(crate) fn dispatch_extension_on_local_core(
        &mut self,
        request: ElmExtensionDispatchRequest,
    ) -> Result<ElmExtensionDispatchResponse, i32> {
        // 该入口只用于未持有全局 Core 自旋锁的局部测试实例，并复用正式事务语义。
        let plan = match self.prepare_extension_dispatch_execution(request) {
            PreparedExtensionDispatch::Immediate(result) => return result,
            PreparedExtensionDispatch::External(plan) => plan,
        };
        let mut state = MixinDispatchState::new(plan.mode, plan.opcode, plan.payload.clone());
        for edge in plan.matched_edges.clone() {
            let mixin =
                match self.prepare_mixin_provider_execution(&edge, plan.opcode, state.payload()) {
                    Ok(mixin) => mixin,
                    Err(status) => {
                        state.record_execution_error(status);
                        if state.halted {
                            break;
                        }
                        continue;
                    }
                };
            let result = execute_provider_call_plan(&mixin.call);
            if !state.note_invocation() {
                let _ = self.complete_mixin_provider_execution(mixin, result);
                break;
            }
            let reply = match self.complete_mixin_provider_execution(mixin, result) {
                Ok(reply) => reply,
                Err(status) => {
                    state.record_execution_error(status);
                    if state.halted {
                        break;
                    }
                    continue;
                }
            };
            state.record_reply(reply);
            if state.halted {
                break;
            }
        }
        Ok(self.complete_extension_dispatch_execution(
            plan,
            state.called,
            state.blockers,
            state.last_reply,
        ))
    }

    fn mixin_provider_index(&self, extension: ElmId, contract: &str) -> Option<usize> {
        if let Some(index) = self
            .providers
            .iter()
            .enumerate()
            .find(|(_, provider)| {
                provider.owner == Some(extension)
                    && self
                        .port_desc(provider.port)
                        .is_some_and(|port| port.contract() == contract && port.invokable)
            })
            .map(|(index, _)| index)
        {
            return Some(index);
        }

        // 精确契约无法命中时，仅允许唯一可调用端口作为无歧义回退入口。
        let mut candidates = self.providers.iter().enumerate().filter(|(_, provider)| {
            provider.owner == Some(extension)
                && self
                    .port_desc(provider.port)
                    .is_some_and(|port| port.invokable)
        });
        let (index, _) = candidates.next()?;
        candidates.next().is_none().then_some(index)
    }
    fn prepare_extension_dispatch_execution(
        &mut self,
        request: ElmExtensionDispatchRequest,
    ) -> PreparedExtensionDispatch {
        if request.flags & !ELM_EXTENSION_DISPATCH_FLAGS_MASK != 0
            || request.reserved0 != 0
            || request.reserved1 != 0
            || usize::from(request.payload_len) > request.payload.len()
        {
            return PreparedExtensionDispatch::Immediate(Err(ELM_MGR_STATUS_INVALID));
        }
        let Some(point) = extension_dispatch_request_point(&request) else {
            return PreparedExtensionDispatch::Immediate(Err(ELM_MGR_STATUS_INVALID));
        };
        let Some(contract) = extension_dispatch_request_contract(&request) else {
            return PreparedExtensionDispatch::Immediate(Err(ELM_MGR_STATUS_INVALID));
        };
        let target = ElmId(request.target_cell_id);
        if self.cell_state(target).is_none() {
            return PreparedExtensionDispatch::Immediate(Err(ELM_MGR_STATUS_NOT_FOUND));
        }
        let extension_point = self
            .graph
            .extension_point(target, point)
            .map(|extension_point| {
                (
                    extension_point.mode,
                    extension_point.contract.as_str() == contract,
                )
            });
        let mode = match extension_point {
            Some((mode, true)) => mode,
            Some((mode, false)) => {
                let blockers = ELM_POLICY_BLOCK_CONTRACT_MISMATCH;
                self.record_mgr_audit(
                    ELM_MGR_ACTION_EXTENSION_DISPATCH,
                    target,
                    blockers,
                    self.cell_state(target).map(state_code).unwrap_or(0),
                );
                self.push_mixin_trace(
                    target,
                    ElmId(request.extension_cell_id),
                    status_from_blockers(blockers),
                    0,
                    blockers,
                );
                return PreparedExtensionDispatch::Immediate(Ok(
                    ElmExtensionDispatchResponse::new(
                        status_from_blockers(blockers),
                        0,
                        0,
                        blockers,
                        ElmReplyFrame::empty(0, u64::from(request.opcode), ELM_CALL_STATUS_INVALID),
                    )
                    .with_mode(mode),
                ));
            }
            None => {
                let blockers = ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND;
                self.record_mgr_audit(
                    ELM_MGR_ACTION_EXTENSION_DISPATCH,
                    target,
                    blockers,
                    self.cell_state(target).map(state_code).unwrap_or(0),
                );
                self.push_mixin_trace(
                    target,
                    ElmId(request.extension_cell_id),
                    status_from_blockers(blockers),
                    0,
                    blockers,
                );
                return PreparedExtensionDispatch::Immediate(Ok(
                    ElmExtensionDispatchResponse::new(
                        status_from_blockers(blockers),
                        0,
                        0,
                        blockers,
                        ElmReplyFrame::empty(
                            0,
                            u64::from(request.opcode),
                            ELM_CALL_STATUS_NOT_FOUND,
                        ),
                    ),
                ));
            }
        };
        if !self.cell_policy_allows(target, ELM_CELL_POLICY_ALLOW_EXTENSION) {
            let blockers = ELM_POLICY_BLOCK_CAPABILITY_DENIED;
            self.record_mgr_audit(
                ELM_MGR_ACTION_EXTENSION_DISPATCH,
                target,
                blockers,
                self.cell_state(target).map(state_code).unwrap_or(0),
            );
            self.push_mixin_trace(
                target,
                ElmId(request.extension_cell_id),
                ELM_MGR_STATUS_PERMISSION,
                0,
                blockers,
            );
            return PreparedExtensionDispatch::Immediate(Ok(ElmExtensionDispatchResponse::new(
                ELM_MGR_STATUS_PERMISSION,
                0,
                0,
                blockers,
                ElmReplyFrame::empty(0, u64::from(request.opcode), ELM_CALL_STATUS_INVALID),
            )
            .with_mode(mode)));
        }
        let requested_extension = if request.extension_cell_id == 0 {
            None
        } else {
            Some(ElmId(request.extension_cell_id))
        };
        if request.flags & ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION != 0
            && requested_extension.is_none()
        {
            return PreparedExtensionDispatch::Immediate(Err(ELM_MGR_STATUS_INVALID));
        }
        let mut matched_edges: Vec<_> = self
            .graph
            .extensions()
            .iter()
            .filter(|edge| {
                edge.target == target
                    && edge.point == point
                    && edge.contract.as_str() == contract
                    && requested_extension.is_none_or(|extension| edge.extension == extension)
            })
            .cloned()
            .collect();
        matched_edges.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.extension.0.cmp(&right.extension.0))
        });
        if matched_edges.is_empty() {
            if request.flags & elm_model::ELM_EXTENSION_DISPATCH_FLAG_ALLOW_EMPTY != 0 {
                self.record_mgr_audit(
                    ELM_MGR_ACTION_EXTENSION_DISPATCH,
                    target,
                    0,
                    self.cell_state(target).map(state_code).unwrap_or(0),
                );
                self.push_mixin_trace(
                    target,
                    requested_extension.unwrap_or(ElmId(0)),
                    ELM_MGR_STATUS_OK,
                    0,
                    0,
                );
                return PreparedExtensionDispatch::Immediate(Ok(
                    ElmExtensionDispatchResponse::new(
                        ELM_MGR_STATUS_OK,
                        0,
                        0,
                        0,
                        ElmReplyFrame::empty(0, u64::from(request.opcode), ELM_CALL_STATUS_OK),
                    )
                    .with_mode(mode),
                ));
            }
            let blockers = ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND;
            self.record_mgr_audit(
                ELM_MGR_ACTION_EXTENSION_DISPATCH,
                target,
                blockers,
                self.cell_state(target).map(state_code).unwrap_or(0),
            );
            self.push_mixin_trace(
                target,
                requested_extension.unwrap_or(ElmId(0)),
                status_from_blockers(blockers),
                0,
                blockers,
            );
            return PreparedExtensionDispatch::Immediate(Ok(ElmExtensionDispatchResponse::new(
                status_from_blockers(blockers),
                0,
                0,
                blockers,
                ElmReplyFrame::empty(0, u64::from(request.opcode), ELM_CALL_STATUS_NOT_FOUND),
            )
            .with_mode(mode)));
        }
        let target = match self.reserve_cell_execution(target) {
            Ok(token) => token,
            Err(status) => {
                return PreparedExtensionDispatch::Immediate(Err(status));
            }
        };
        PreparedExtensionDispatch::External(ExtensionDispatchExecutionPlan {
            target,
            requested_extension,
            matched_edges,
            mode,
            opcode: request.opcode,
            payload: request.payload[..usize::from(request.payload_len)].to_vec(),
        })
    }

    fn prepare_mixin_provider_execution(
        &mut self,
        extension_edge: &ExtensionEdge,
        opcode: u32,
        payload: &[u8],
    ) -> Result<MixinProviderExecutionPlan, i32> {
        if !self.graph.extensions().contains(extension_edge) {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        }
        if !self.cell_policy_allows(extension_edge.extension, ELM_CELL_POLICY_ALLOW_EXTENSION) {
            return Err(ELM_MGR_STATUS_PERMISSION);
        }
        let Some(provider_index) = self.mixin_provider_index(
            extension_edge.extension,
            extension_edge.handler_contract.as_str(),
        ) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        let provider = self.providers[provider_index].clone();
        let Some(port) = self.port_desc(provider.port) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if !port.invokable {
            return Err(ELM_MGR_STATUS_UNSUPPORTED);
        }
        if matches!(
            provider.backend,
            ProviderBackend::Kernel(_) | ProviderBackend::ElmNativeTodo
        ) {
            return Err(ELM_MGR_STATUS_UNSUPPORTED);
        }
        let contract = FlowContract::new(port.contract()).map_err(|_| ELM_MGR_STATUS_INVALID)?;
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == extension_edge.extension)
            .map(|cell| cell.generation)
            .ok_or(ELM_MGR_STATUS_NOT_FOUND)?;
        let (Some(binding), Some(lease)) = (self.alloc_binding_id(), self.alloc_lease_id()) else {
            return Err(ELM_MGR_STATUS_BUSY);
        };
        let edge = elm_model::CapabilityBindingEdge {
            id: binding,
            consumer: extension_edge.extension,
            port: provider.port,
            contract,
            generation,
            lease: Some(lease),
            active: true,
        };
        if let Some(blockers) = self.provider_call_blocker(&edge, &provider) {
            return Err(status_from_blockers(blockers));
        }
        self.leases
            .insert(
                ResourceLease::new(
                    lease,
                    extension_edge.extension,
                    LeaseKind::Provider,
                    LeaseRights::CONTROL,
                    generation,
                )
                .with_binding(binding),
            )
            .map_err(|_| ELM_MGR_STATUS_INVALID)?;
        let reservation = match self.reserve_provider_execution(
            provider_index,
            Some(&edge),
            Some(lease),
            true,
            false,
            0,
        ) {
            Ok(reservation) => reservation,
            Err(status) => {
                let _ = self.leases.revoke_and_remove(lease);
                return Err(status);
            }
        };
        let frame = ElmCallFrame::new(binding.0, self.last_event_sequence(), opcode, payload);
        Ok(MixinProviderExecutionPlan {
            call: ProviderCallExecutionPlan {
                reservation,
                backend: provider.backend,
                edge,
                frame,
                deadline_ns: 0,
                reply_flags_mask: ELM_MIXIN_REPLY_FLAGS_MASK,
            },
            ephemeral_lease: lease,
        })
    }

    fn complete_mixin_provider_execution(
        &mut self,
        plan: MixinProviderExecutionPlan,
        result: Result<ElmReplyFrame, i32>,
    ) -> Result<ElmReplyFrame, i32> {
        let port = plan.call.edge.port;
        let extension = plan.call.edge.consumer;
        let backend = plan.call.backend;
        let may_patch = self
            .cells
            .iter()
            .find(|cell| cell.id == extension)
            .is_some_and(|cell| {
                cell.cell_policy.extension_flags & ELM_EXTENSION_POLICY_MIXIN_PATCH != 0
                    && cell.cell_policy.native_flags & ELM_NATIVE_POLICY_MIXIN_PATCH != 0
            });
        let current = self.finish_provider_execution(plan.call.reservation, true);
        if let Err(err) = self.leases.revoke_and_remove(plan.ephemeral_lease) {
            log::warning!(
                "[elm] mixin temporary lease cleanup failed lease={} err={:?}",
                plan.ephemeral_lease.0,
                err
            );
        }
        let Some(provider_index) = self.provider_index(port) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if !current {
            self.providers[provider_index].failed_calls = self.providers[provider_index]
                .failed_calls
                .saturating_add(1);
            return Err(ELM_MGR_STATUS_BUSY);
        }
        match result {
            Ok(reply) => {
                if let ProviderBackend::ElmNative(native) = backend
                    && reply.status == ELM_CALL_STATUS_PROVIDER_FAULT
                {
                    self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED);
                }
                if reply.status == ELM_CALL_STATUS_OK
                    && reply.flags & ELM_MIXIN_REPLY_REPLACE != 0
                    && !may_patch
                {
                    self.providers[provider_index].failed_calls = self.providers[provider_index]
                        .failed_calls
                        .saturating_add(1);
                    return Err(ELM_MGR_STATUS_PERMISSION);
                }
                if reply.status == ELM_CALL_STATUS_OK {
                    self.providers[provider_index].calls =
                        self.providers[provider_index].calls.saturating_add(1);
                } else {
                    self.providers[provider_index].failed_calls = self.providers[provider_index]
                        .failed_calls
                        .saturating_add(1);
                }
                Ok(reply)
            }
            Err(status) => {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
                Err(status)
            }
        }
    }

    fn complete_extension_dispatch_execution(
        &mut self,
        plan: ExtensionDispatchExecutionPlan,
        called: u32,
        mut blockers: u64,
        last_reply: ElmReplyFrame,
    ) -> ElmExtensionDispatchResponse {
        let target = plan.target.cell;
        let matched = plan.matched_edges.len() as u32;
        let graph_current = plan
            .matched_edges
            .iter()
            .all(|edge| self.graph.extensions().contains(edge));
        if !self.cell_execution_is_current(plan.target) || !graph_current {
            blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
        }
        self.release_cell_execution(plan.target);
        if called == 0 && blockers == ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND {
            blockers = ELM_POLICY_BLOCK_PORT_TODO;
        }
        let status = if blockers == ELM_POLICY_BLOCK_PORT_TODO {
            ELM_MGR_STATUS_UNSUPPORTED
        } else {
            status_from_blockers(blockers)
        };
        self.record_mgr_audit(
            ELM_MGR_ACTION_EXTENSION_DISPATCH,
            target,
            blockers,
            self.cell_state(target).map(state_code).unwrap_or(0),
        );
        self.push_mixin_trace(
            target,
            plan.requested_extension.unwrap_or(ElmId(0)),
            status,
            called,
            blockers,
        );
        if blockers == 0 {
            self.emit(TopologyEventKind::CellStateChanged, Some(target));
        }
        ElmExtensionDispatchResponse::new(status, matched, called, blockers, last_reply)
            .with_mode(plan.mode)
    }

    fn extension_attach_plan(&self, request: ElmExtensionAttachRequest) -> (i32, u64, u64) {
        let extension = ElmId(request.extension_cell_id);
        let target = ElmId(request.target_cell_id);
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == extension)
            .map(|cell| cell.generation.0)
            .unwrap_or(0);
        let mut blockers = 0;
        let point = extension_request_point(&request);
        let contract_text = extension_request_contract(&request);
        let handler_contract_text = extension_request_handler_contract(&request);
        if request.flags != 0
            || request.reserved != 0
            || point.is_none()
            || contract_text.is_none()
            || handler_contract_text.is_none()
        {
            blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
        }
        if self.cell_state(extension).is_none() || self.cell_state(target).is_none() {
            blockers |= ELM_POLICY_BLOCK_CELL_NOT_FOUND;
        } else if !self.cell_policy_allows(extension, ELM_CELL_POLICY_ALLOW_EXTENSION) {
            blockers |= ELM_POLICY_BLOCK_CAPABILITY_DENIED;
        }
        if self.graph.validate().is_err() {
            blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
        }
        if blockers == 0 {
            let contract = match FlowContract::new(contract_text.unwrap_or("")) {
                Ok(contract) => contract,
                Err(_) => {
                    return (
                        status_from_blockers(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                        ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                        generation,
                    );
                }
            };
            let handler_contract = match FlowContract::new(handler_contract_text.unwrap_or("")) {
                Ok(contract) => contract,
                Err(_) => {
                    return (
                        status_from_blockers(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                        ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                        generation,
                    );
                }
            };
            if self
                .graph
                .extension_exists(extension, target, point.unwrap_or(""), &contract)
            {
                blockers |= ELM_POLICY_BLOCK_EXTENSION_DUPLICATE;
            } else {
                let mut graph = self.graph.clone();
                match graph.add_extension_with_dispatch(
                    extension,
                    target,
                    point.unwrap_or("").to_string(),
                    contract,
                    handler_contract,
                    request.priority,
                ) {
                    Ok(()) => {}
                    Err(ElmError::ExtensionPointNotFound) => {
                        blockers |= ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND;
                    }
                    Err(ElmError::ContractMismatch) => {
                        blockers |= ELM_POLICY_BLOCK_CONTRACT_MISMATCH;
                    }
                    Err(ElmError::ExtensionCycle) => {
                        blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
                    }
                    Err(ElmError::DuplicateBinding) => {
                        blockers |= ELM_POLICY_BLOCK_EXTENSION_DUPLICATE;
                    }
                    Err(_) => {
                        blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
                    }
                }
            }
        }
        (status_from_blockers(blockers), blockers, generation)
    }

    pub fn audit_bytes(&self) -> Vec<u8> {
        let header = ElmMgrAuditHeader::new(
            self.audits.len() as u32,
            self.dropped_audit_count,
            self.next_audit_sequence.saturating_sub(1),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for audit in &self.audits {
            push_plain(&mut out, audit);
        }
        out
    }

    pub fn nexus_bindings_bytes(&self) -> Vec<u8> {
        let header = ElmNexusBindingSnapshotHeader::new(
            self.graph.capability_bindings().len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for edge in self.graph.capability_bindings() {
            let record = ElmNexusBindingRecord::new(
                edge.id.0,
                edge.consumer.0,
                edge.port.0,
                edge.lease.map(|lease| lease.0).unwrap_or(0),
                edge.generation.0,
                edge.active,
                edge.contract.as_str(),
            );
            push_plain(&mut out, &record);
        }
        out
    }

    pub fn runtime_ports_bytes(&self) -> Vec<u8> {
        let header = ElmRuntimePortStatsHeader::new(
            self.runtime_ports.len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for runtime in &self.runtime_ports {
            let record = ElmRuntimePortStatsRecord::new(
                runtime.binding.0,
                runtime.cell.0,
                runtime.port.0,
                runtime.lease.0,
                runtime.cursor,
                runtime.submitted_logs,
                runtime.delivered_events,
                runtime.dropped_events,
            );
            push_plain(&mut out, &record);
        }
        out
    }

    pub fn provider_ports_bytes(&self) -> Vec<u8> {
        let header = ElmProviderPortStatsHeader::new(
            self.providers.len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for provider in &self.providers {
            let Some(port) = self.port_desc(provider.port) else {
                continue;
            };
            let record = ElmProviderPortRecord::new(
                provider.port.0,
                provider.owner.map(|owner| owner.0).unwrap_or(0),
                provider.access as u32,
                port.direction as u32,
                port.mode as u32,
                port.implemented,
                port.invokable,
                self.provider_binding_count(provider.port) as u32,
                provider.record_flags(),
                provider.calls,
                provider.failed_calls,
                provider.revokes,
                port.contract(),
            );
            push_plain(&mut out, &record);
        }
        out
    }

    pub fn provider_stats_bytes(&self) -> Vec<u8> {
        let header = ElmProviderPortStatsHeader::new_stats(
            self.providers.len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for provider in &self.providers {
            let record = ElmProviderPortStatsRecord::new(
                provider.port.0,
                provider.owner.map(|owner| owner.0).unwrap_or(0),
                self.provider_binding_count(provider.port) as u32,
                u32::from(provider.record_flags()),
                provider.calls,
                provider.failed_calls,
                provider.revokes,
            );
            push_plain(&mut out, &record);
        }
        out
    }

    pub fn provider_snapshot_bytes(
        &mut self,
        request: ElmProviderSnapshotRequest,
    ) -> Result<Vec<u8>, i32> {
        // 该入口只用于未持有全局 Core 自旋锁的局部测试实例，并复用正式事务语义。
        match self.prepare_provider_snapshot_execution(request)? {
            PreparedProviderSnapshot::Immediate(result) => result,
            PreparedProviderSnapshot::External(plan) => {
                let (page, payload) = execute_provider_snapshot_plan(&plan);
                self.complete_provider_snapshot_execution(plan, page, payload)
            }
        }
    }
    fn prepare_provider_snapshot_execution(
        &mut self,
        request: ElmProviderSnapshotRequest,
    ) -> Result<PreparedProviderSnapshot, i32> {
        if request.flags & !ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK != 0
            || (!request.is_paged() && request.reserved != 0)
            || (request.port_id == 0 && request.binding_id == 0)
        {
            return Err(ELM_MGR_STATUS_INVALID);
        }

        let binding = if request.binding_id == 0 {
            None
        } else {
            let edge = self
                .graph
                .capability_binding(BindingId(request.binding_id))
                .ok_or(ELM_MGR_STATUS_NOT_FOUND)?;
            if !edge.active {
                return Err(ELM_MGR_STATUS_INVALID);
            }
            Some(edge.clone())
        };
        let port = if request.port_id == 0 {
            binding
                .as_ref()
                .map(|edge| edge.port)
                .ok_or(ELM_MGR_STATUS_INVALID)?
        } else {
            let port = PortId(request.port_id);
            if binding.as_ref().is_some_and(|edge| edge.port != port) {
                return Err(ELM_MGR_STATUS_INVALID);
            }
            port
        };
        let Some(provider_index) = self.provider_index(port) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        let audit_cell = binding
            .as_ref()
            .map(|edge| edge.consumer)
            .or(self.providers[provider_index].owner)
            .unwrap_or(ElmId(0));
        let capacity =
            ELM_MGR_MAX_PAYLOAD.saturating_sub(core::mem::size_of::<ElmProviderSnapshotHeader>());
        let backend = self.providers[provider_index].backend;
        let executable = match backend {
            ProviderBackend::KernelOps(spec) => {
                if request.is_paged() {
                    spec.snapshot_paged.is_some()
                } else {
                    spec.snapshot.is_some()
                }
            }
            ProviderBackend::ElmNative(native) => native.snapshot.is_some(),
            ProviderBackend::Kernel(_) | ProviderBackend::ElmNativeTodo => false,
        };
        if !executable {
            let status = match backend {
                ProviderBackend::ElmNativeTodo => ELM_MGR_STATUS_TODO,
                _ => ELM_MGR_STATUS_UNSUPPORTED,
            };
            let result = self.finish_provider_snapshot_result(
                request,
                port,
                audit_cell,
                ProviderSnapshotPageResult::status_only(status),
                Vec::new(),
            );
            return Ok(PreparedProviderSnapshot::Immediate(Ok(result)));
        }

        let lease = binding.as_ref().and_then(|edge| edge.lease);
        if binding.is_some() && lease.is_none() {
            return Err(ELM_MGR_STATUS_INVALID);
        }
        let reservation = self.reserve_provider_execution(
            provider_index,
            binding.as_ref(),
            lease,
            lease.is_some(),
            true,
            0,
        )?;
        let owner = self.providers[provider_index].owner.unwrap_or(ElmId(0));
        let allowed_actions = reservation
            .cells
            .iter()
            .find(|token| token.cell == owner)
            .map(|token| token.allowed_actions)
            .unwrap_or(0);
        Ok(PreparedProviderSnapshot::External(
            ProviderSnapshotExecutionPlan {
                reservation,
                backend,
                request,
                binding_id: binding.as_ref().map(|edge| edge.id.0).unwrap_or(0),
                lease: lease.unwrap_or(LeaseId(0)),
                audit_cell,
                allowed_actions,
                capacity,
            },
        ))
    }

    fn complete_provider_snapshot_execution(
        &mut self,
        plan: ProviderSnapshotExecutionPlan,
        page: ProviderSnapshotPageResult,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, i32> {
        let port = plan.reservation.port;
        let current = self.finish_provider_execution(plan.reservation, true);
        let (page, payload) = if current {
            (page, payload)
        } else {
            (
                ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_BUSY),
                Vec::new(),
            )
        };
        if let ProviderBackend::ElmNative(native) = plan.backend
            && matches!(page.status, ELM_MGR_STATUS_INVALID)
        {
            self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED);
        }
        Ok(
            self.finish_provider_snapshot_result(
                plan.request,
                port,
                plan.audit_cell,
                page,
                payload,
            ),
        )
    }

    fn finish_provider_snapshot_result(
        &mut self,
        request: ElmProviderSnapshotRequest,
        port: PortId,
        audit_cell: ElmId,
        page: ProviderSnapshotPageResult,
        mut payload: Vec<u8>,
    ) -> Vec<u8> {
        payload.truncate(page.payload_len.min(payload.len()));
        if let Some(provider_index) = self.provider_index(port) {
            if page.status == ELM_MGR_STATUS_OK {
                self.providers[provider_index].calls =
                    self.providers[provider_index].calls.saturating_add(1);
            } else {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
            }
        }
        self.record_audit(
            ELM_MGR_ACTION_PROVIDER_QUERY,
            audit_cell,
            page.status,
            provider_snapshot_blockers(page.status),
            self.cell_state(audit_cell).map(state_code).unwrap_or(0),
        );
        let header = ElmProviderSnapshotHeader::new(
            page.status,
            port.0,
            request.binding_id,
            payload.len() as u32,
            page.record_count,
        )
        .with_page(page.flags, page.next_cursor);
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        out.extend_from_slice(&payload);
        out
    }

    pub fn provider_queue_bytes(&mut self, now_ns: u64) -> Vec<u8> {
        self.cleanup_provider_results_at(now_ns);
        self.expire_provider_jobs_at(now_ns);

        let header = ElmProviderQueueStatsHeader::new(
            self.providers.len() as u32,
            self.last_event_sequence(),
        );
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for provider in &self.providers {
            let record = ElmProviderQueueStatsRecord::new(
                provider.port.0,
                self.provider_queued_count(provider.port) as u32,
                self.provider_running_count(provider.port) as u32,
                self.provider_retained_result_count(provider.port) as u32,
                provider.queue_limit,
                provider.max_in_flight,
                provider.async_submitted,
                provider.async_completed,
                provider.async_canceled,
                provider.async_expired,
                provider.async_rejected,
            );
            push_plain(&mut out, &record);
        }
        out
    }

    pub fn submit_provider_call(
        &mut self,
        request: ElmProviderAsyncSubmitRequest,
        now_ns: u64,
    ) -> ElmProviderAsyncSubmitResponse {
        self.cleanup_provider_results_at(now_ns);
        self.expire_provider_jobs_at(now_ns);

        let frame = request.frame;
        if request.flags != 0
            || request.reserved != 0
            || usize::from(frame.payload_len) > frame.payload.len()
        {
            return ElmProviderAsyncSubmitResponse::new(
                0,
                frame.binding_id,
                frame.call_id,
                ELM_MGR_STATUS_INVALID,
                ElmProviderAsyncState::Failed,
                0,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }

        let submit = self.prepare_provider_async_submit(frame);
        let (edge, provider_index, lease) = match submit {
            Ok(prepared) => prepared,
            Err((status, blockers, port)) => {
                self.record_provider_async_rejection(port);
                self.record_provider_async_audit(frame.binding_id, status, blockers);
                return ElmProviderAsyncSubmitResponse::new(
                    0,
                    frame.binding_id,
                    frame.call_id,
                    status,
                    ElmProviderAsyncState::Failed,
                    port.map(|port| self.provider_queued_count(port) as u32)
                        .unwrap_or(0),
                    blockers,
                );
            }
        };

        let provider_port = self.providers[provider_index].port;
        if !self.cell_policy_allows(edge.consumer, ELM_CELL_POLICY_ALLOW_PROVIDER) {
            let blockers = ELM_POLICY_BLOCK_CAPABILITY_DENIED;
            self.providers[provider_index].async_rejected = self.providers[provider_index]
                .async_rejected
                .saturating_add(1);
            self.record_provider_async_audit(frame.binding_id, ELM_MGR_STATUS_PERMISSION, blockers);
            return ElmProviderAsyncSubmitResponse::new(
                0,
                frame.binding_id,
                frame.call_id,
                ELM_MGR_STATUS_PERMISSION,
                ElmProviderAsyncState::Failed,
                self.provider_queued_count(provider_port) as u32,
                blockers,
            );
        }
        if self.cell_resource_over_quota(edge.consumer, ElmResourceKind::ProviderQueue) {
            let blockers = ELM_POLICY_BLOCK_RESOURCE_QUOTA;
            self.providers[provider_index].async_rejected = self.providers[provider_index]
                .async_rejected
                .saturating_add(1);
            self.record_provider_async_audit(frame.binding_id, ELM_MGR_STATUS_BUSY, blockers);
            return ElmProviderAsyncSubmitResponse::new(
                0,
                frame.binding_id,
                frame.call_id,
                ELM_MGR_STATUS_BUSY,
                ElmProviderAsyncState::Failed,
                self.provider_queued_count(provider_port) as u32,
                blockers,
            );
        }
        let provider = &self.providers[provider_index];
        let pending = self
            .provider_queued_count(provider.port)
            .saturating_add(self.provider_in_flight_count(provider));
        if pending >= provider.queue_limit as usize {
            let blockers = ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL;
            self.providers[provider_index].async_rejected = self.providers[provider_index]
                .async_rejected
                .saturating_add(1);
            self.record_provider_async_audit(frame.binding_id, ELM_MGR_STATUS_BUSY, blockers);
            return ElmProviderAsyncSubmitResponse::new(
                0,
                frame.binding_id,
                frame.call_id,
                ELM_MGR_STATUS_BUSY,
                ElmProviderAsyncState::Failed,
                pending as u32,
                blockers,
            );
        }

        if self.leases.add_active_ref(lease).is_err() {
            let blockers = ELM_POLICY_BLOCK_LEASE_BUSY;
            self.providers[provider_index].async_rejected = self.providers[provider_index]
                .async_rejected
                .saturating_add(1);
            self.record_provider_async_audit(frame.binding_id, ELM_MGR_STATUS_BUSY, blockers);
            return ElmProviderAsyncSubmitResponse::new(
                0,
                frame.binding_id,
                frame.call_id,
                ELM_MGR_STATUS_BUSY,
                ElmProviderAsyncState::Failed,
                pending as u32,
                blockers,
            );
        }

        let Some(ticket) = self.alloc_provider_ticket_id() else {
            self.release_provider_result_lease(lease);
            let blockers = ELM_POLICY_BLOCK_RESOURCE_QUOTA;
            self.providers[provider_index].async_rejected = self.providers[provider_index]
                .async_rejected
                .saturating_add(1);
            self.record_provider_async_audit(frame.binding_id, ELM_MGR_STATUS_BUSY, blockers);
            return ElmProviderAsyncSubmitResponse::new(
                0,
                frame.binding_id,
                frame.call_id,
                ELM_MGR_STATUS_BUSY,
                ElmProviderAsyncState::Failed,
                pending as u32,
                blockers,
            );
        };
        let timeout_ns = provider_async_timeout_ns(request.timeout_ms);
        let result_ttl_ns = provider_async_result_ttl_ns(request.result_ttl_ms);
        self.provider_jobs.push_back(ProviderAsyncJob {
            ticket,
            frame,
            consumer: edge.consumer,
            port: edge.port,
            lease,
            deadline_ns: now_ns.saturating_add(timeout_ns),
            result_ttl_ns,
        });
        self.providers[provider_index].async_submitted = self.providers[provider_index]
            .async_submitted
            .saturating_add(1);
        let queue_depth = self.provider_queued_count(edge.port) as u32;
        self.record_provider_async_audit(frame.binding_id, ELM_MGR_STATUS_OK, 0);

        ElmProviderAsyncSubmitResponse::new(
            ticket,
            frame.binding_id,
            frame.call_id,
            ELM_MGR_STATUS_OK,
            ElmProviderAsyncState::Queued,
            queue_depth,
            0,
        )
    }

    pub fn poll_provider_reply(
        &mut self,
        request: ElmProviderAsyncPollRequest,
        now_ns: u64,
    ) -> ElmProviderAsyncPollResponse {
        self.cleanup_provider_results_at(now_ns);
        self.expire_provider_jobs_at(now_ns);

        if request.flags != 0 || request.reserved != 0 || request.ticket_id == 0 {
            return provider_async_poll_failure(
                request.ticket_id,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }

        if let Some(job) = self
            .provider_jobs
            .iter()
            .find(|job| job.ticket == request.ticket_id)
        {
            return ElmProviderAsyncPollResponse::new(
                job.ticket,
                ElmProviderAsyncState::Queued,
                ELM_MGR_STATUS_BUSY,
                ElmReplyFrame::empty(
                    job.frame.binding_id,
                    job.frame.call_id,
                    ELM_CALL_STATUS_BUSY,
                ),
                0,
                job.deadline_ns,
            );
        }

        if let Some(running) = self
            .provider_running
            .iter()
            .find(|running| running.job.ticket == request.ticket_id)
        {
            let blockers = if running.cancel_requested {
                ELM_POLICY_BLOCK_PROVIDER_BUSY
            } else {
                0
            };
            return ElmProviderAsyncPollResponse::new(
                running.job.ticket,
                ElmProviderAsyncState::Running,
                ELM_MGR_STATUS_BUSY,
                ElmReplyFrame::empty(
                    running.job.frame.binding_id,
                    running.job.frame.call_id,
                    ELM_CALL_STATUS_BUSY,
                ),
                blockers,
                running.job.deadline_ns,
            );
        }

        if let Some(index) = self
            .provider_results
            .iter()
            .position(|result| result.ticket == request.ticket_id)
        {
            let Some(result) = self.provider_results.remove(index) else {
                return provider_async_poll_failure(
                    request.ticket_id,
                    ELM_MGR_STATUS_NOT_FOUND,
                    ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
                );
            };
            self.release_provider_result_lease(result.lease);
            return ElmProviderAsyncPollResponse::new(
                result.ticket,
                result.state,
                result.status,
                result.reply,
                result.blockers,
                result.expires_at_ns,
            );
        }

        provider_async_poll_failure(
            request.ticket_id,
            ELM_MGR_STATUS_NOT_FOUND,
            ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
        )
    }

    pub fn cancel_provider_call(
        &mut self,
        request: ElmProviderAsyncCancelRequest,
        now_ns: u64,
    ) -> ElmProviderAsyncCancelResponse {
        self.cleanup_provider_results_at(now_ns);
        self.expire_provider_jobs_at(now_ns);

        if request.flags != 0 || request.reserved != 0 || request.ticket_id == 0 {
            return ElmProviderAsyncCancelResponse::new(
                request.ticket_id,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }

        if let Some(index) = self
            .provider_jobs
            .iter()
            .position(|job| job.ticket == request.ticket_id)
        {
            let Some(job) = self.provider_jobs.remove(index) else {
                return ElmProviderAsyncCancelResponse::new(
                    request.ticket_id,
                    ElmProviderAsyncState::Failed,
                    ELM_MGR_STATUS_NOT_FOUND,
                    ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
                );
            };
            if let Some(provider_index) = self.provider_index(job.port) {
                self.providers[provider_index].async_canceled = self.providers[provider_index]
                    .async_canceled
                    .saturating_add(1);
            }
            self.release_provider_result_lease(job.lease);
            self.record_provider_async_audit(job.frame.binding_id, ELM_MGR_STATUS_OK, 0);
            return ElmProviderAsyncCancelResponse::new(
                job.ticket,
                ElmProviderAsyncState::Canceled,
                ELM_MGR_STATUS_OK,
                0,
            );
        }

        if let Some(index) = self
            .provider_running
            .iter()
            .position(|running| running.job.ticket == request.ticket_id)
        {
            if !self.provider_running[index].cancel_requested {
                let binding_id = self.provider_running[index].job.frame.binding_id;
                self.provider_running[index].cancel_requested = true;
                self.record_provider_async_audit(
                    binding_id,
                    ELM_MGR_STATUS_BUSY,
                    ELM_POLICY_BLOCK_PROVIDER_BUSY,
                );
            }
            return ElmProviderAsyncCancelResponse::new(
                request.ticket_id,
                ElmProviderAsyncState::Running,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_PROVIDER_BUSY,
            );
        }

        if let Some(result) = self
            .provider_results
            .iter()
            .find(|result| result.ticket == request.ticket_id)
        {
            return ElmProviderAsyncCancelResponse::new(
                result.ticket,
                result.state,
                result.status,
                result.blockers,
            );
        }

        ElmProviderAsyncCancelResponse::new(
            request.ticket_id,
            ElmProviderAsyncState::Failed,
            ELM_MGR_STATUS_NOT_FOUND,
            ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
        )
    }

    pub(crate) fn has_provider_async_work(&self) -> bool {
        !self.provider_jobs.is_empty() || !self.provider_revoke_notifications.is_empty()
    }

    fn take_provider_revoke_notification(&mut self) -> Option<ProviderRevokeNotification> {
        self.provider_revoke_notifications.pop_front()
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn drain_provider_revoke_notifications_for_test(&mut self) -> usize {
        let mut drained = 0;
        while let Some(notification) = self.take_provider_revoke_notification() {
            (notification.callback)(notification.binding, notification.lease);
            drained += 1;
        }
        drained
    }

    pub(crate) fn expire_provider_jobs_at(&mut self, now_ns: u64) -> usize {
        let mut expired = 0usize;
        let mut index = 0usize;
        while index < self.provider_jobs.len() {
            if self.provider_jobs[index].deadline_ns > now_ns {
                index += 1;
                continue;
            }
            let Some(job) = self.provider_jobs.remove(index) else {
                break;
            };
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Expired,
                ELM_MGR_STATUS_BUSY,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_BUSY),
                ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED,
                now_ns,
            );
            expired += 1;
        }
        expired
    }

    pub(crate) fn run_one_async_provider_job_at(&mut self, now_ns: u64) -> bool {
        // 局部测试入口复用正式 prepare/execute/complete 链路，不维护第二套外部执行语义。
        match self.prepare_one_async_provider_execution(now_ns) {
            PreparedAsyncProviderWork::None => false,
            PreparedAsyncProviderWork::Handled => true,
            PreparedAsyncProviderWork::External(plan) => {
                let result = execute_provider_call_plan(&plan.call);
                self.complete_async_provider_execution(plan, result, sched::now_ns_public())
            }
        }
    }
    fn prepare_one_async_provider_execution(&mut self, now_ns: u64) -> PreparedAsyncProviderWork {
        self.cleanup_provider_results_at(now_ns);
        if self.expire_provider_jobs_at(now_ns) != 0 {
            return PreparedAsyncProviderWork::Handled;
        }

        let Some(job_index) = self.next_runnable_provider_job_index() else {
            return PreparedAsyncProviderWork::None;
        };
        let Some(job) = self.provider_jobs.remove(job_index) else {
            return PreparedAsyncProviderWork::Handled;
        };
        let Some(provider_index) = self.provider_index(job.port) else {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_NOT_FOUND,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_NOT_FOUND),
                ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
                now_ns,
            );
            return PreparedAsyncProviderWork::Handled;
        };
        let Some(edge) = self
            .graph
            .capability_binding(BindingId(job.frame.binding_id))
            .cloned()
        else {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_NOT_FOUND,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_NOT_FOUND),
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
                now_ns,
            );
            return PreparedAsyncProviderWork::Handled;
        };
        if !edge.active
            || edge.port != job.port
            || edge.consumer != job.consumer
            || edge.lease != Some(job.lease)
        {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_INVALID,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_INVALID),
                ELM_POLICY_BLOCK_INVALID_STATE,
                now_ns,
            );
            return PreparedAsyncProviderWork::Handled;
        }
        if !self.cell_policy_allows(edge.consumer, ELM_CELL_POLICY_ALLOW_PROVIDER) {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_PERMISSION,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_INVALID),
                ELM_POLICY_BLOCK_CAPABILITY_DENIED,
                now_ns,
            );
            return PreparedAsyncProviderWork::Handled;
        }
        if let Some(blockers) = self.provider_call_blocker(&edge, &self.providers[provider_index]) {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                status_from_blockers(blockers),
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_BUSY),
                blockers,
                now_ns,
            );
            return PreparedAsyncProviderWork::Handled;
        }

        let backend = self.providers[provider_index].backend;
        if matches!(
            backend,
            ProviderBackend::Kernel(_) | ProviderBackend::ElmNativeTodo
        ) {
            if let Err(job) = self.begin_provider_running_call(job.clone(), provider_index, now_ns)
            {
                self.finish_provider_async_job(
                    job,
                    ElmProviderAsyncState::Failed,
                    ELM_MGR_STATUS_BUSY,
                    ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_BUSY),
                    ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                    now_ns,
                );
                return PreparedAsyncProviderWork::Handled;
            }
            let (state, status, reply, blockers) = self.execute_provider_async_job(&job);
            self.finish_provider_running_call(
                job.ticket,
                state,
                status,
                reply,
                blockers,
                sched::now_ns_public(),
            );
            return PreparedAsyncProviderWork::Handled;
        }

        if self.provider_running.try_reserve(1).is_err() {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_BUSY,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_BUSY),
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                now_ns,
            );
            return PreparedAsyncProviderWork::Handled;
        }
        let reservation = match self.reserve_provider_execution(
            provider_index,
            Some(&edge),
            Some(job.lease),
            false,
            true,
            job.deadline_ns,
        ) {
            Ok(reservation) => reservation,
            Err(status) => {
                self.finish_provider_async_job(
                    job,
                    ElmProviderAsyncState::Failed,
                    status,
                    ElmReplyFrame::empty(0, 0, provider_async_call_status_from_mgr(status)),
                    provider_async_blocker_from_mgr_status(status),
                    now_ns,
                );
                return PreparedAsyncProviderWork::Handled;
            }
        };
        self.provider_running.push(ProviderRunningCall {
            job: job.clone(),
            started_at_ns: now_ns,
            cancel_requested: false,
        });
        PreparedAsyncProviderWork::External(AsyncProviderExecutionPlan {
            ticket: job.ticket,
            call: ProviderCallExecutionPlan {
                reservation,
                backend,
                edge,
                frame: job.frame,
                deadline_ns: job.deadline_ns,
                reply_flags_mask: 0,
            },
        })
    }

    fn complete_async_provider_execution(
        &mut self,
        plan: AsyncProviderExecutionPlan,
        result: Result<ElmReplyFrame, i32>,
        finish_ns: u64,
    ) -> bool {
        let current = self.finish_provider_execution(plan.call.reservation, false);
        let (state, status, reply, blockers) = if !current {
            (
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_BUSY,
                ElmReplyFrame::empty(
                    plan.call.frame.binding_id,
                    plan.call.frame.call_id,
                    ELM_CALL_STATUS_BUSY,
                ),
                ELM_POLICY_BLOCK_PROVIDER_BUSY,
            )
        } else {
            match result {
                Ok(reply) if reply.status == ELM_CALL_STATUS_OK => {
                    if let Some(provider_index) = self.provider_index(plan.call.edge.port) {
                        self.providers[provider_index].calls =
                            self.providers[provider_index].calls.saturating_add(1);
                    }
                    (
                        ElmProviderAsyncState::Completed,
                        ELM_MGR_STATUS_OK,
                        reply,
                        0,
                    )
                }
                Ok(reply) => {
                    if let ProviderBackend::ElmNative(native) = plan.call.backend
                        && reply.status == ELM_CALL_STATUS_PROVIDER_FAULT
                    {
                        self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED);
                    }
                    if let Some(provider_index) = self.provider_index(plan.call.edge.port) {
                        self.providers[provider_index].failed_calls = self.providers
                            [provider_index]
                            .failed_calls
                            .saturating_add(1);
                    }
                    (
                        ElmProviderAsyncState::Failed,
                        ELM_MGR_STATUS_INVALID,
                        reply,
                        provider_call_blockers(reply.status),
                    )
                }
                Err(status) => {
                    if let Some(provider_index) = self.provider_index(plan.call.edge.port) {
                        self.providers[provider_index].failed_calls = self.providers
                            [provider_index]
                            .failed_calls
                            .saturating_add(1);
                    }
                    (
                        ElmProviderAsyncState::Failed,
                        status,
                        ElmReplyFrame::empty(
                            plan.call.frame.binding_id,
                            plan.call.frame.call_id,
                            provider_async_call_status_from_mgr(status),
                        ),
                        provider_async_blocker_from_mgr_status(status),
                    )
                }
            }
        };
        self.finish_provider_running_call(plan.ticket, state, status, reply, blockers, finish_ns)
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn move_provider_ticket_to_running_for_test(
        &mut self,
        ticket: u64,
        now_ns: u64,
    ) -> bool {
        let Some(job_index) = self
            .provider_jobs
            .iter()
            .position(|job| job.ticket == ticket)
        else {
            return false;
        };
        let Some(job) = self.provider_jobs.remove(job_index) else {
            return false;
        };
        let Some(provider_index) = self.provider_index(job.port) else {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_NOT_FOUND,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_NOT_FOUND),
                ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
                now_ns,
            );
            return false;
        };
        self.begin_provider_running_call(job, provider_index, now_ns)
            .is_ok()
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn finish_provider_ticket_for_test(&mut self, ticket: u64, finish_ns: u64) -> bool {
        let Some(running) = self
            .provider_running
            .iter()
            .find(|running| running.job.ticket == ticket)
        else {
            return false;
        };
        let reply = ElmReplyFrame::empty(
            running.job.frame.binding_id,
            running.job.frame.call_id,
            ELM_CALL_STATUS_OK,
        );
        self.finish_provider_running_call(
            ticket,
            ElmProviderAsyncState::Completed,
            ELM_MGR_STATUS_OK,
            reply,
            0,
            finish_ns,
        )
    }

    pub fn health_bytes(&self) -> Vec<u8> {
        let (status, records) = self.health_records();
        let header =
            ElmCoreHealthHeader::new(records.len() as u32, status, self.last_event_sequence());
        let mut out = Vec::new();
        push_plain(&mut out, &header);
        for record in &records {
            push_plain(&mut out, record);
        }
        out
    }

    pub fn register_provider_port(
        &mut self,
        request: ElmProviderPortRegisterRequest,
    ) -> ElmProviderPortRegisterResponse {
        let owner = ElmId(request.owner_cell_id);
        let mut blockers = 0;
        let access = match ElmPortAccessPolicy::from_raw(request.access_policy) {
            Some(access) => access,
            None => {
                blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                ElmPortAccessPolicy::Internal
            }
        };
        let direction = match FlowDirection::from_raw(request.direction) {
            Some(direction) => direction,
            None => {
                blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                FlowDirection::Control
            }
        };
        let mode = match FlowMode::from_raw(request.mode) {
            Some(mode) => mode,
            None => {
                blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                FlowMode::Shared
            }
        };
        let Some(contract) = provider_request_contract(&request) else {
            blockers |= ELM_POLICY_BLOCK_CONTRACT_MISMATCH;
            return self.provider_register_response(owner, PortId(0), access, blockers);
        };
        if FlowContract::new(contract).is_err() {
            blockers |= ELM_POLICY_BLOCK_CONTRACT_MISMATCH;
        }
        let owner_state = self.cell_state(owner);
        if owner_state.is_none() {
            blockers |= ELM_POLICY_BLOCK_CELL_NOT_FOUND;
        } else if !self.cell_policy_allows(owner, ELM_CELL_POLICY_ALLOW_PROVIDER) {
            blockers |= ELM_POLICY_BLOCK_CAPABILITY_DENIED;
        } else if !matches!(
            owner_state,
            Some(ElmState::Loaded | ElmState::Linked | ElmState::Ready | ElmState::Active)
        ) {
            blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
        } else if self.cell_is_isolated(owner) {
            blockers |= ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED;
        } else if self.cell_resource_over_quota(owner, ElmResourceKind::ProviderPort) {
            blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
        }
        if request.flags != 0 {
            blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
        }
        if self.ports.iter().any(|port| port.contract() == contract) {
            blockers |= ELM_POLICY_BLOCK_DUPLICATE_BINDING;
        }
        if blockers != 0 {
            return self.provider_register_response(owner, PortId(0), access, blockers);
        }

        let Some(port) = self.alloc_port_id() else {
            return self.provider_register_response(
                owner,
                PortId(0),
                access,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
        };
        let runtime = PortRuntime::new(
            port,
            Some(owner),
            contract,
            direction,
            mode,
            access,
            false,
            false,
        );
        self.register_port(runtime);
        self.providers.push(ProviderRuntime {
            port,
            owner: Some(owner),
            access,
            backend: ProviderBackend::ElmNativeTodo,
            backend_epoch: 1,
            dynamic: true,
            queue_limit: provider_queue_limit_for_mode(mode),
            max_in_flight: provider_max_in_flight_for_mode(mode),
            in_flight: 0,
            calls: 0,
            failed_calls: 0,
            revokes: 0,
            async_submitted: 0,
            async_completed: 0,
            async_canceled: 0,
            async_expired: 0,
            async_rejected: 0,
        });
        self.record_mgr_audit(
            ELM_MGR_ACTION_PROVIDER_REGISTER,
            owner,
            0,
            state_code(self.cell_state(owner).unwrap_or(ElmState::Active)),
        );
        ElmProviderPortRegisterResponse::new(owner.0, port.0, ELM_MGR_STATUS_OK, access as u32, 0)
    }

    pub fn unregister_provider_port(
        &mut self,
        request: ElmProviderPortUnregisterRequest,
    ) -> ElmProviderPortRegisterResponse {
        let port = PortId(request.port_id);
        let Some(index) = self.provider_index(port) else {
            return ElmProviderPortRegisterResponse::new(
                0,
                request.port_id,
                ELM_MGR_STATUS_NOT_FOUND,
                0,
                ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
            );
        };
        let provider = self.providers[index].clone();
        let owner = provider.owner.unwrap_or(ElmId(0));
        let blockers = if !provider.dynamic {
            ELM_POLICY_BLOCK_BUILTIN_PROTECTED
        } else if self.provider_binding_count(port) != 0
            || self.provider_queued_count(port) != 0
            || self.provider_retained_result_count(port) != 0
            || self.provider_in_flight_count(&provider) != 0
        {
            ELM_POLICY_BLOCK_PROVIDER_BUSY
        } else {
            0
        };
        if blockers != 0 {
            self.record_mgr_audit(
                ELM_MGR_ACTION_PROVIDER_UNREGISTER,
                owner,
                blockers,
                self.cell_state(owner).map(state_code).unwrap_or(0),
            );
            return ElmProviderPortRegisterResponse::new(
                owner.0,
                port.0,
                status_from_blockers(blockers),
                provider.access as u32,
                blockers,
            );
        }
        self.providers.remove(index);
        self.ports.retain(|runtime| runtime.id != port);
        self.record_mgr_audit(
            ELM_MGR_ACTION_PROVIDER_UNREGISTER,
            owner,
            0,
            self.cell_state(owner).map(state_code).unwrap_or(0),
        );
        ElmProviderPortRegisterResponse::new(
            owner.0,
            port.0,
            ELM_MGR_STATUS_OK,
            provider.access as u32,
            0,
        )
    }

    pub fn invoke_provider(
        &mut self,
        request: ElmProviderInvokeRequest,
    ) -> Result<ElmProviderInvokeResponse, i32> {
        // 该入口只用于未持有全局 Core 自旋锁的局部测试实例，并复用正式事务语义。
        match self.prepare_provider_call_execution(request)? {
            PreparedProviderCall::Immediate(result) => result,
            PreparedProviderCall::External(plan) => {
                let result = execute_provider_call_plan(&plan);
                self.complete_provider_call_execution(plan, result)
            }
        }
    }
    fn invoke_kernel_provider(
        &self,
        kind: KernelProviderKind,
        edge: &elm_model::CapabilityBindingEdge,
        frame: ElmCallFrame,
    ) -> Result<ElmReplyFrame, i32> {
        match kind {
            KernelProviderKind::MgrActionInvoke => self.invoke_mgr_action_provider(edge, frame),
            KernelProviderKind::StaticPort => Ok(ElmReplyFrame::empty(
                frame.binding_id,
                frame.call_id,
                ELM_CALL_STATUS_UNSUPPORTED,
            )),
        }
    }

    fn invoke_mgr_action_provider(
        &self,
        _edge: &elm_model::CapabilityBindingEdge,
        frame: ElmCallFrame,
    ) -> Result<ElmReplyFrame, i32> {
        if frame.opcode != ELM_ACTION_OPCODE_INVOKE {
            return Ok(ElmReplyFrame::empty(
                frame.binding_id,
                frame.call_id,
                ELM_CALL_STATUS_UNSUPPORTED,
            ));
        }
        let Some(request) = read_action_invoke_request(&frame) else {
            return Ok(ElmReplyFrame::empty(
                frame.binding_id,
                frame.call_id,
                ELM_CALL_STATUS_INVALID,
            ));
        };
        let Some(action) = self
            .mgr_actions
            .iter()
            .find(|action| action.action.0 == request.action_id)
        else {
            return Ok(ElmReplyFrame::empty(
                frame.binding_id,
                frame.call_id,
                ELM_CALL_STATUS_NOT_FOUND,
            ));
        };
        match action.kind {
            MgrActionKind::Health => {
                let (health_status, _) = self.health_records();
                let reply = ElmActionInvokeReply::health(
                    action.action.0,
                    action.menu_item,
                    action.owner.0,
                    health_status,
                    self.last_event_sequence(),
                );
                Ok(ElmReplyFrame::new(
                    frame.binding_id,
                    frame.call_id,
                    ELM_CALL_STATUS_OK,
                    plain_bytes(&reply),
                ))
            }
        }
    }

    pub fn submit_runtime_log(
        &mut self,
        request: ElmRuntimeLogRequest,
    ) -> Result<ElmRuntimeLogResponse, i32> {
        let binding = BindingId(request.binding_id);
        let Some(index) = self.runtime_port_index(binding) else {
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_LOG,
                None,
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
            );
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if let Err(status) = self.validate_runtime_port(index, ELM_CORE_LOG_PORT_ID) {
            let blockers = runtime_status_blocker(status);
            self.record_runtime_audit(ELM_MGR_ACTION_RUNTIME_LOG, Some(index), status, blockers);
            return Err(status);
        }
        let message_len = usize::from(request.message_len);
        if message_len > ELM_RUNTIME_LOG_MESSAGE_LEN {
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_LOG,
                Some(index),
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
            return Err(ELM_MGR_STATUS_INVALID);
        }
        let Ok(message) = core::str::from_utf8(&request.message[..message_len]) else {
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_LOG,
                Some(index),
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
            return Err(ELM_MGR_STATUS_INVALID);
        };
        let Some(level) = runtime_log_level(request.level) else {
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_LOG,
                Some(index),
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
            return Err(ELM_MGR_STATUS_INVALID);
        };

        let cell = self.runtime_ports[index].cell;
        let line = format!(
            "[elm-runtime][cell={} binding={}] {}",
            cell.0, binding.0, message
        );
        log::logger_entry(level, log::get_timestamp_ns(), &line);
        self.runtime_ports[index].submitted_logs =
            self.runtime_ports[index].submitted_logs.saturating_add(1);
        let response = ElmRuntimeLogResponse::new(
            binding.0,
            message_len as u32,
            ELM_MGR_STATUS_OK,
            self.runtime_ports[index].submitted_logs,
        );
        self.record_runtime_audit(
            ELM_MGR_ACTION_RUNTIME_LOG,
            Some(index),
            ELM_MGR_STATUS_OK,
            0,
        );
        Ok(response)
    }

    pub fn read_runtime_event(
        &mut self,
        request: ElmRuntimeEventRequest,
    ) -> Result<ElmRuntimeEventResponse, i32> {
        let binding = BindingId(request.binding_id);
        let Some(index) = self.runtime_port_index(binding) else {
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_EVENT_READ,
                None,
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
            );
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if let Err(status) = self.validate_runtime_port(index, ELM_CORE_EVENT_PORT_ID) {
            let blockers = runtime_status_blocker(status);
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_EVENT_READ,
                Some(index),
                status,
                blockers,
            );
            return Err(status);
        }

        let mut cursor = if request.cursor == 0 {
            self.runtime_ports[index].cursor
        } else {
            request.cursor
        };
        self.recover_stale_runtime_cursor(index, &mut cursor, request.cursor == 0);
        let event = self
            .events
            .iter()
            .find(|event| event.sequence > cursor)
            .copied();
        let dropped_events = self.runtime_ports[index].dropped_events;
        let response = match event {
            Some(event) => {
                self.runtime_ports[index].delivered_events =
                    self.runtime_ports[index].delivered_events.saturating_add(1);
                ElmRuntimeEventResponse::with_event(
                    binding.0,
                    cursor,
                    event,
                    dropped_events,
                    ELM_MGR_STATUS_OK,
                )
            }
            None => {
                ElmRuntimeEventResponse::empty(binding.0, cursor, dropped_events, ELM_MGR_STATUS_OK)
            }
        };
        self.record_runtime_audit(
            ELM_MGR_ACTION_RUNTIME_EVENT_READ,
            Some(index),
            ELM_MGR_STATUS_OK,
            0,
        );
        Ok(response)
    }

    pub fn ack_runtime_event(
        &mut self,
        request: ElmRuntimeEventRequest,
    ) -> Result<ElmRuntimeEventResponse, i32> {
        let binding = BindingId(request.binding_id);
        let Some(index) = self.runtime_port_index(binding) else {
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_EVENT_ACK,
                None,
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
            );
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if let Err(status) = self.validate_runtime_port(index, ELM_CORE_EVENT_PORT_ID) {
            let blockers = runtime_status_blocker(status);
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_EVENT_ACK,
                Some(index),
                status,
                blockers,
            );
            return Err(status);
        }

        let current = self.runtime_ports[index].cursor;
        if request.cursor < current || request.cursor > self.last_event_sequence() {
            self.record_runtime_audit(
                ELM_MGR_ACTION_RUNTIME_EVENT_ACK,
                Some(index),
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
            return Err(ELM_MGR_STATUS_INVALID);
        }
        self.runtime_ports[index].cursor = request.cursor;
        let dropped_events = self.runtime_ports[index].dropped_events;
        let response = ElmRuntimeEventResponse::empty(
            binding.0,
            request.cursor,
            dropped_events,
            ELM_MGR_STATUS_OK,
        );
        self.record_runtime_audit(
            ELM_MGR_ACTION_RUNTIME_EVENT_ACK,
            Some(index),
            ELM_MGR_STATUS_OK,
            0,
        );
        Ok(response)
    }

    pub fn preflight_bind(&self, request: ElmNexusBindRequest) -> ElmNexusBindPlanResponse {
        let id = ElmId(request.cell_id);
        let port = PortId(request.port_id);
        let mut blockers = 0;

        let cell_generation = match self.cells.iter().find(|cell| cell.id == id) {
            Some(cell) => {
                if !self.cell_policy_allows(id, ELM_CELL_POLICY_ALLOW_BIND) {
                    blockers |= ELM_POLICY_BLOCK_CAPABILITY_DENIED;
                }
                if !matches!(
                    cell.state,
                    ElmState::Loaded | ElmState::Linked | ElmState::Ready | ElmState::Active
                ) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
                if cell.isolated {
                    blockers |= ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED;
                }
                cell.generation
            }
            None => {
                blockers |= ELM_POLICY_BLOCK_CELL_NOT_FOUND;
                Generation::FIRST
            }
        };

        let request_contract = request_contract(&request);
        let port_desc = match self.port_desc(port) {
            Some(desc) => {
                if request_contract != Some(desc.contract()) {
                    blockers |= ELM_POLICY_BLOCK_CONTRACT_MISMATCH;
                }
                if !self.provider_access_allowed(id, &desc) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
                // 已实现端口才允许进入真实绑定提交路径；其它端口只暴露描述和预检。
                if !self.is_bind_supported_port(&desc) {
                    blockers |= ELM_POLICY_BLOCK_PORT_TODO;
                }
                Some(desc)
            }
            None => {
                blockers |= ELM_POLICY_BLOCK_PORT_NOT_FOUND;
                None
            }
        };

        if let (Some(contract), Some(_)) = (request_contract, port_desc) {
            if let Ok(contract) = FlowContract::new(contract) {
                if self
                    .graph
                    .capability_binding_for(id, port, &contract)
                    .is_some()
                {
                    blockers |= ELM_POLICY_BLOCK_DUPLICATE_BINDING;
                }
            } else {
                blockers |= ELM_POLICY_BLOCK_CONTRACT_MISMATCH;
            }
        }

        if self.next_binding_id == 0 || self.next_lease_id == 0 {
            blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
        }
        if port == ELM_MGR_MENU_PORT_ID
            && (self.menu_generation.checked_next().is_none()
                || self.next_action_id == 0
                || self.next_menu_item_id == 0)
        {
            blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
        }

        if self.graph.validate().is_err() {
            blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
        }

        ElmNexusBindPlanResponse::new(
            request.cell_id,
            request.port_id,
            0,
            0,
            cell_generation.0,
            blockers == 0,
            status_from_blockers(blockers),
            blockers,
        )
    }

    pub fn commit_bind(&mut self, request: ElmNexusBindRequest) -> ElmNexusBindPlanResponse {
        let plan = self.preflight_bind(request);
        if plan.allowed == 0 {
            self.record_mgr_audit(
                ELM_MGR_ACTION_BIND,
                ElmId(request.cell_id),
                plan.blockers,
                self.cell_state(ElmId(request.cell_id))
                    .map(state_code)
                    .unwrap_or(0),
            );
            return plan;
        }

        let id = ElmId(request.cell_id);
        let port = PortId(request.port_id);
        let Some(contract) = request_contract(&request) else {
            return self.bind_failure_response(request, ELM_POLICY_BLOCK_CONTRACT_MISMATCH);
        };
        let Ok(contract) = FlowContract::new(contract) else {
            return self.bind_failure_response(request, ELM_POLICY_BLOCK_CONTRACT_MISMATCH);
        };

        let (Some(binding), Some(lease)) = (self.alloc_binding_id(), self.alloc_lease_id()) else {
            return self.bind_failure_response(request, ELM_POLICY_BLOCK_RESOURCE_QUOTA);
        };
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .unwrap_or(Generation::FIRST);

        let attach_result = match self.port_desc(port) {
            Some(desc) if desc.id == ELM_MGR_MENU_PORT_ID => {
                self.attach_menu_binding(id, port, contract, binding, lease)
            }
            Some(desc)
                if desc.id == ELM_CORE_LOG_PORT_ID && desc.contract() == ELM_CORE_LOG_CONTRACT =>
            {
                self.attach_runtime_port_binding(
                    id,
                    port,
                    contract,
                    binding,
                    lease,
                    LeaseKind::RuntimePort,
                    LeaseRights::WRITE,
                )
            }
            Some(desc)
                if desc.id == ELM_CORE_EVENT_PORT_ID
                    && desc.contract() == ELM_CORE_EVENT_CONTRACT =>
            {
                self.attach_runtime_port_binding(
                    id,
                    port,
                    contract,
                    binding,
                    lease,
                    LeaseKind::RuntimePort,
                    LeaseRights::READ,
                )
            }
            Some(desc) if self.provider_index(desc.id).is_some() => {
                self.attach_provider_binding(id, port, contract, binding, lease)
            }
            _ => Err(ElmError::PortNotFound),
        };

        if let Err(err) = attach_result {
            log::error!("[elm] commit Nexus binding failed: {:?}", err);
            return self.bind_failure_response(request, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
        }

        if self.activate_bound_cell(id).is_err() {
            return self.bind_failure_response(request, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
        }

        let response = ElmNexusBindPlanResponse::new(
            request.cell_id,
            request.port_id,
            binding.0,
            lease.0,
            generation.0,
            true,
            ELM_MGR_STATUS_OK,
            0,
        );
        self.record_mgr_audit(
            ELM_MGR_ACTION_BIND,
            id,
            0,
            self.cell_state(id).map(state_code).unwrap_or(0),
        );
        response
    }

    pub fn preflight_unbind(&self, request: ElmNexusUnbindRequest) -> ElmNexusBindPlanResponse {
        let binding = BindingId(request.binding_id);
        let Some(edge) = self.graph.capability_binding(binding) else {
            return ElmNexusBindPlanResponse::new(
                0,
                0,
                request.binding_id,
                0,
                0,
                false,
                status_from_blockers(ELM_POLICY_BLOCK_BINDING_NOT_FOUND),
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
            );
        };

        let mut blockers = 0;
        if edge.id.0 < FIRST_DYNAMIC_CELL_ID {
            blockers |= ELM_POLICY_BLOCK_BINDING_PROTECTED;
        }
        if let Some(lease) = edge.lease {
            match self.leases.get(lease) {
                Some(lease) if lease.active_refs != 0 => blockers |= ELM_POLICY_BLOCK_LEASE_BUSY,
                Some(_) => {}
                None => blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
            }
        }
        if edge.port == ELM_MGR_MENU_PORT_ID && self.menu_generation.checked_next().is_none() {
            blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
        }
        if self.graph.validate().is_err() {
            blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
        }

        ElmNexusBindPlanResponse::new(
            edge.consumer.0,
            edge.port.0,
            edge.id.0,
            edge.lease.map(|lease| lease.0).unwrap_or(0),
            edge.generation.0,
            blockers == 0,
            status_from_blockers(blockers),
            blockers,
        )
    }

    pub fn commit_unbind(&mut self, request: ElmNexusUnbindRequest) -> ElmNexusBindPlanResponse {
        let plan = self.preflight_unbind(request);
        if plan.allowed == 0 {
            self.record_mgr_audit(
                ELM_MGR_ACTION_UNBIND,
                ElmId(plan.cell_id),
                plan.blockers,
                self.cell_state(ElmId(plan.cell_id))
                    .map(state_code)
                    .unwrap_or(0),
            );
            return plan;
        }

        let binding = BindingId(request.binding_id);
        let Some(edge) = self.graph.capability_binding(binding).cloned() else {
            return ElmNexusBindPlanResponse::new(
                0,
                0,
                request.binding_id,
                0,
                0,
                false,
                status_from_blockers(ELM_POLICY_BLOCK_BINDING_NOT_FOUND),
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
            );
        };
        if let Some(lease) = edge.lease {
            match self.leases.get(lease) {
                Some(resource) if resource.active_refs != 0 => {
                    return ElmNexusBindPlanResponse::new(
                        edge.consumer.0,
                        edge.port.0,
                        edge.id.0,
                        lease.0,
                        edge.generation.0,
                        false,
                        status_from_blockers(ELM_POLICY_BLOCK_LEASE_BUSY),
                        ELM_POLICY_BLOCK_LEASE_BUSY,
                    );
                }
                Some(_) => {}
                None => {
                    return ElmNexusBindPlanResponse::new(
                        edge.consumer.0,
                        edge.port.0,
                        edge.id.0,
                        lease.0,
                        edge.generation.0,
                        false,
                        status_from_blockers(ELM_POLICY_BLOCK_GRAPH_INCONSISTENT),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
            }
        }

        let Ok(edge) = self.graph.remove_capability_binding(binding) else {
            return ElmNexusBindPlanResponse::new(
                0,
                0,
                request.binding_id,
                0,
                0,
                false,
                status_from_blockers(ELM_POLICY_BLOCK_BINDING_NOT_FOUND),
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
            );
        };

        if let Some(lease) = edge.lease {
            match self.leases.revoke_and_remove(lease) {
                Ok(lease) => self.emit_lease(TopologyEventKind::LeaseRevoked, lease),
                Err(ElmError::LeaseBusy) => {
                    return ElmNexusBindPlanResponse::new(
                        edge.consumer.0,
                        edge.port.0,
                        edge.id.0,
                        lease.0,
                        edge.generation.0,
                        false,
                        status_from_blockers(ELM_POLICY_BLOCK_LEASE_BUSY),
                        ELM_POLICY_BLOCK_LEASE_BUSY,
                    );
                }
                Err(_) => {
                    return ElmNexusBindPlanResponse::new(
                        edge.consumer.0,
                        edge.port.0,
                        edge.id.0,
                        lease.0,
                        edge.generation.0,
                        false,
                        status_from_blockers(ELM_POLICY_BLOCK_GRAPH_INCONSISTENT),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
            }
        }

        self.remove_owned_binding(edge.consumer, edge.id);
        self.remove_runtime_binding(edge.id);
        self.note_provider_revoke(&edge);
        let removed_menu_items = if edge.port == ELM_MGR_MENU_PORT_ID {
            self.remove_menu_items_owned_by(edge.consumer)
        } else {
            0
        };
        if removed_menu_items != 0 {
            log::debug!(
                "[elm] removed {} menu item(s) for Nexus binding {}",
                removed_menu_items,
                edge.id.0
            );
        }
        self.emit_binding(TopologyEventKind::BindingRemoved, edge.id);
        self.record_mgr_audit(
            ELM_MGR_ACTION_UNBIND,
            edge.consumer,
            0,
            self.cell_state(edge.consumer).map(state_code).unwrap_or(0),
        );

        ElmNexusBindPlanResponse::new(
            edge.consumer.0,
            edge.port.0,
            edge.id.0,
            edge.lease.map(|lease| lease.0).unwrap_or(0),
            edge.generation.0,
            true,
            ELM_MGR_STATUS_OK,
            0,
        )
    }

    pub fn preflight_lifecycle(
        &self,
        request: ElmLifecyclePlanRequest,
    ) -> ElmLifecyclePlanResponse {
        self.preflight_lifecycle_inner(request, false)
    }

    fn preflight_lifecycle_inner(
        &self,
        request: ElmLifecyclePlanRequest,
        lifecycle_executor_available: bool,
    ) -> ElmLifecyclePlanResponse {
        let Some(action) = ElmLifecycleAction::from_raw(request.action) else {
            return ElmLifecyclePlanResponse {
                cell_id: request.cell_id,
                action: request.action,
                allowed: 0,
                status: ELM_MGR_STATUS_INVALID,
                final_state: self
                    .cell_state(ElmId(request.cell_id))
                    .map(state_code)
                    .unwrap_or(0),
                blockers: ELM_POLICY_BLOCK_INVALID_STATE,
                affected_children: 0,
                affected_dependents: 0,
                affected_extensions: 0,
                reserved: 0,
            };
        };

        let id = ElmId(request.cell_id);
        let Some(current) = self.cell_state(id) else {
            return ElmLifecyclePlanResponse::new(
                request.cell_id,
                action,
                false,
                status_from_blockers(ELM_POLICY_BLOCK_CELL_NOT_FOUND),
                0,
                ELM_POLICY_BLOCK_CELL_NOT_FOUND,
            );
        };

        let children = self.live_child_count(id);
        let dependents = self.graph.dependent_count(id);
        let extensions = self.graph.extension_target_count(id);
        let mut blockers = 0;

        if !self.cell_policy_allows(id, ELM_CELL_POLICY_ALLOW_LIFECYCLE) {
            blockers |= ELM_POLICY_BLOCK_CAPABILITY_DENIED;
        }
        if self.is_builtin_cell(id) {
            blockers |= ELM_POLICY_BLOCK_BUILTIN_PROTECTED;
        }
        if self.graph.validate().is_err() {
            blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
        }
        if self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .is_some_and(|cell| cell.active_executions != 0)
        {
            blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
        }
        if super::owned_resource::owner_snapshot(id).is_none_or(|owner| {
            self.cells
                .iter()
                .find(|cell| cell.id == id)
                .is_none_or(|cell| owner.generation != cell.generation)
        }) {
            blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
        }

        match action {
            ElmLifecycleAction::Pause => {
                if !matches!(current, ElmState::Active | ElmState::Paused) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
                if self.provider_runtime_busy_owned_by(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
                }
                if self.leases.busy_owned_by(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_LEASE_BUSY;
                }
                if self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .is_some_and(|cell| {
                        super::owned_resource::count_owned_by(id, cell.generation) != 0
                    })
                {
                    // v1 资源操作表只定义不可逆退役阶段，不能伪造可恢复的暂停语义。
                    blockers |= ELM_POLICY_BLOCK_LEASE_BUSY;
                }
                if self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .is_some_and(|cell| super::source::owner_generation_busy(id, cell.generation))
                {
                    blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
                }
            }
            ElmLifecycleAction::Resume => {
                if !matches!(current, ElmState::Paused | ElmState::Active) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
            }
            ElmLifecycleAction::Detach => {
                // 已激活的原生单元必须能拿到生命周期执行器，确保卸载前执行 finalize。
                if self.cell_has_native_code(id)
                    && current != ElmState::Loaded
                    && !lifecycle_executor_available
                {
                    blockers |= ELM_POLICY_BLOCK_NATIVE_TODO;
                }
                if !matches!(
                    current,
                    ElmState::Active
                        | ElmState::Quiescing
                        | ElmState::Paused
                        | ElmState::Loaded
                        | ElmState::Quarantined
                        | ElmState::Detached
                ) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
                if children != 0 {
                    blockers |= ELM_POLICY_BLOCK_HAS_CHILDREN;
                }
                if dependents != 0 {
                    blockers |= ELM_POLICY_BLOCK_HAS_DEPENDENTS;
                }
                if extensions != 0 {
                    blockers |= ELM_POLICY_BLOCK_HAS_EXTENSIONS;
                }
                if self.leases.busy_owned_by(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_LEASE_BUSY;
                }
                if self.provider_busy_owned_by(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
                }
                if self.native_export_importer_count(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_HAS_DEPENDENTS;
                }
                if self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .is_some_and(|cell| {
                        !cell.owned_menu_items.is_empty()
                            && self.menu_generation.checked_next().is_none()
                    })
                {
                    blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
                }
                if self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .is_some_and(|cell| super::source::owner_generation_busy(id, cell.generation))
                {
                    blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
                }
            }
            ElmLifecycleAction::Replace => {
                if !matches!(current, ElmState::Active | ElmState::Paused) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
                if super::resource_accounting::has_live_allocations(id) {
                    // 普通动态分配当前按 cell 计量，不能在代际切换时证明其中不存在旧镜像
                    // 的 Rust 引用。要求替换前清空堆状态，避免新 generation 继承裸指针。
                    blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
                }
                if children != 0 {
                    blockers |= ELM_POLICY_BLOCK_HAS_CHILDREN;
                }
                // 已建立的 binding 在切换时原地保留；只有真实运行中工作阻断替换。
                if self.provider_runtime_busy_owned_by(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
                }
                if self.native_direct_pinned_importer_count(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_HAS_DEPENDENTS;
                }
                if self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .is_some_and(|cell| super::source::owner_generation_busy(id, cell.generation))
                {
                    blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
                }
                if self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .is_some_and(|cell| {
                        cell.policy_epoch == u64::MAX
                            || (!cell.owned_menu_items.is_empty()
                                && self.menu_generation.checked_next().is_none())
                    })
                    || self.providers.iter().any(|provider| {
                        provider.owner == Some(id) && provider.backend_epoch == u64::MAX
                    })
                {
                    blockers |= ELM_POLICY_BLOCK_RESOURCE_QUOTA;
                }
                if self.leases.busy_owned_by(id) != 0 {
                    blockers |= ELM_POLICY_BLOCK_LEASE_BUSY;
                }
                if self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .is_some_and(|cell| {
                        super::owned_resource::count_owned_by(id, cell.generation) != 0
                    })
                {
                    blockers |= ELM_POLICY_BLOCK_LEASE_BUSY;
                }
            }
        }

        let allowed = blockers == 0;
        let final_state = if allowed {
            planned_final_state(action, current)
        } else {
            state_code(current)
        };
        ElmLifecyclePlanResponse::new(
            request.cell_id,
            action,
            allowed,
            status_from_blockers(blockers),
            final_state,
            blockers,
        )
        .with_affected(children as u32, dependents as u32, extensions as u32)
    }

    pub fn record_mgr_audit(
        &mut self,
        action: u32,
        cell_id: ElmId,
        blockers: u64,
        final_state: u32,
    ) -> i32 {
        let status = status_from_blockers(blockers);
        self.record_audit(action, cell_id, status, blockers, final_state);
        status
    }

    pub(crate) fn record_mgr_authorization(
        &mut self,
        kind: ElmMgrCallKind,
        authorization: ElmMgrAuthorization,
        status: i32,
    ) {
        let subject = ElmId(authorization.subject_id);
        let final_state = self.cell_state(subject).map(state_code).unwrap_or(0);
        if let Some(sequence) = self.alloc_audit_sequence() {
            let record = ElmMgrAuditRecord::new(
                sequence,
                kind as u32,
                status,
                authorization.subject_id,
                authorization.blockers,
                final_state,
            )
            .with_authority(
                ELM_AUDIT_FLAG_OPERATION | ELM_AUDIT_FLAG_AUTHORIZATION,
                authorization.principal.kind as u32,
                authorization.authority,
                authorization.principal.actor_id,
                authorization.authority_id,
                authorization.actor_generation,
                authorization.policy_epoch,
                authorization.principal.credential_id,
            );
            self.push_audit_record(record);
        }
        if let Err(err) = super::journal::append(
            kind as u32,
            status,
            authorization.principal.actor_id,
            authorization.subject_id,
            authorization.authority_id,
            authorization.policy_epoch,
            authorization.blockers,
            super::journal::ELM_JOURNAL_FLAG_AUTHORIZATION,
        ) {
            log::error!("[elm] 授权日志持久化失败: {:?}", err);
        }
        self.push_policy_trace(
            authorization.subject_id,
            kind as u64,
            status,
            authorization.blockers,
        );
    }

    // 内核内部测试和内建路径可直接提交 EBI unit；外部通道必须先经过 Source 分发。
    #[allow(dead_code)]
    pub fn load_ebi_unit(&mut self, unit: ElmEbiUnit, arch: ElmEbiArch) -> ElmLoadCellResponse {
        self.load_ebi_unit_under_parent(unit, arch, ELM_MGR_ID, ElmResourceBudget::DEFAULT)
    }

    pub fn load_ebi_image(&mut self, image: ElmEbiImage, arch: ElmEbiArch) -> ElmLoadCellResponse {
        self.load_declarative_ebi_image_from_source_under_parent(
            image,
            arch,
            ElmEbiSourceKind::Projection,
            ELM_MGR_ID,
            ElmResourceBudget::DEFAULT,
            false,
        )
    }

    pub(crate) fn load_ebi_unit_under_parent(
        &mut self,
        unit: ElmEbiUnit,
        arch: ElmEbiArch,
        parent: ElmId,
        budget: ElmResourceBudget,
    ) -> ElmLoadCellResponse {
        self.load_ebi_unit_inner(
            unit,
            arch,
            ElmEbiSourceKind::Memory,
            parent,
            budget,
            false,
            super::api_registry::ApiGrantApproval::internal(ElmEbiSourceKind::Memory),
            None,
        )
    }

    pub(crate) fn load_declarative_ebi_image_from_source_under_parent(
        &mut self,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        source: ElmEbiSourceKind,
        parent: ElmId,
        budget: ElmResourceBudget,
        grant_management: bool,
    ) -> ElmLoadCellResponse {
        self.load_declarative_ebi_image_from_source_under_parent_with_kernel_api_grant(
            image,
            arch,
            source,
            parent,
            budget,
            grant_management,
            KernelApiGrantRequest::none(),
        )
    }

    pub(crate) fn load_declarative_ebi_image_from_source_under_parent_with_kernel_api_grant(
        &mut self,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        source: ElmEbiSourceKind,
        parent: ElmId,
        budget: ElmResourceBudget,
        grant_management: bool,
        kernel_api_grant: KernelApiGrantRequest,
    ) -> ElmLoadCellResponse {
        if let Err(status) = image.validate(arch) {
            return ElmLoadCellResponse::failed(status);
        }
        // 局部 Core 入口不执行原生代码，原生镜像必须由正式脱锁装载事务处理。
        if image.has_code_segment() {
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        }
        if !self.native_unit_allowed_by_policy(parent, &image.unit) {
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        }
        let trust = match self.prepare_image_trust(&image, source) {
            Ok(trust) => trust,
            Err(status) => return ElmLoadCellResponse::failed(status),
        };
        if grant_management && (image.unit.manifest.kind != ElmKind::Manager || trust.unsigned) {
            self.abort_image_trust(&trust);
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::UntrustedImage);
        }
        let kernel_api_approval = Self::kernel_api_grant_approval(source, kernel_api_grant, &trust);
        let response = self.load_ebi_unit_inner(
            image.unit,
            arch,
            source,
            parent,
            budget,
            grant_management,
            kernel_api_approval,
            None,
        );
        if response.status != ElmEbiLoadStatus::Ok as i32 {
            self.abort_image_trust(&trust);
            return response;
        }
        let id = ElmId(response.cell_id);
        if let Err(err) = self.commit_image_trust(id, &trust) {
            log::error!(
                "[elm] trust acceptance commit failed cell={}: {:?}",
                id.0,
                err
            );
            self.rollback_activated_cell_to_quarantine(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            );
        }
        response
    }

    pub(crate) fn load_ebi_image_in_detached_core(
        &mut self,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        source: ElmEbiSourceKind,
        parent: ElmId,
        budget: ElmResourceBudget,
        grant_management: bool,
        kernel_api_grant: KernelApiGrantRequest,
    ) -> ElmLoadCellResponse {
        let plan = match self.prepare_native_load_execution(
            image,
            arch,
            source,
            parent,
            budget,
            grant_management,
            kernel_api_grant,
        ) {
            PreparedNativeLoad::Immediate(response) => return response,
            PreparedNativeLoad::Initialize(plan) => plan,
        };

        let initialize_result = plan.loaded.on_initialize(&plan.initialize);
        match self.commit_native_load_initialize(plan, initialize_result) {
            NativeLoadCommit::Complete(response) => response,
            NativeLoadCommit::Finalize(failure) => {
                self.finish_native_load_failure_in_detached_core(failure)
            }
            NativeLoadCommit::Entry(plan) => {
                let entry_result =
                    plan.loaded
                        .on_entry(plan.parent, plan.token.generation, ElmState::Active);
                match self.commit_native_load_entry(plan, entry_result) {
                    NativeLoadCommit::Complete(response) => response,
                    NativeLoadCommit::Finalize(failure) => {
                        self.finish_native_load_failure_in_detached_core(failure)
                    }
                    NativeLoadCommit::Entry(_) => {
                        ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected)
                    }
                }
            }
        }
    }

    fn finish_native_load_failure_in_detached_core(
        &mut self,
        mut failure: NativeLoadFailurePlan,
    ) -> ElmLoadCellResponse {
        let mut executor = failure.loaded.lifecycle_executor();
        let result = executor.on_finalize(&mut failure.finalize);
        self.complete_native_load_failure(failure.id, failure.token, failure.import_stage, result);
        failure.response
    }

    fn prepare_native_load_execution(
        &mut self,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        source: ElmEbiSourceKind,
        parent: ElmId,
        budget: ElmResourceBudget,
        grant_management: bool,
        kernel_api_grant: KernelApiGrantRequest,
    ) -> PreparedNativeLoad {
        if let Err(status) = image.validate(arch) {
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(status));
        }
        if !self.native_unit_allowed_by_policy(parent, &image.unit) {
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(
                ElmEbiLoadStatus::RuntimeRejected,
            ));
        }
        if grant_management && image.unit.manifest.kind != ElmKind::Manager {
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(
                ElmEbiLoadStatus::RuntimeRejected,
            ));
        }
        if !image.has_code_segment() {
            return PreparedNativeLoad::Immediate(
                self.load_declarative_ebi_image_from_source_under_parent_with_kernel_api_grant(
                    image,
                    arch,
                    source,
                    parent,
                    budget,
                    grant_management,
                    kernel_api_grant,
                ),
            );
        }
        let mut topology = match self.preflight_ebi_topology(&image.unit) {
            Ok(topology) => topology,
            Err(err) => {
                log::error!("[elm] EBI image topology rejected by runtime: {:?}", err);
                return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(
                    ElmEbiLoadStatus::RuntimeRejected,
                ));
            }
        };
        let manifest = image.unit.manifest.clone();
        let name = manifest.name.as_str().to_string();
        let image_arch = image.unit.target.arch;
        let Some(id) = self.alloc_cell_id() else {
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(
                ElmEbiLoadStatus::RuntimeRejected,
            ));
        };
        let (imports, resolved_native_imports) =
            match self.resolve_native_imports(id, parent, Generation::FIRST, &image.unit) {
                Ok((imports, dependencies, resolved_imports)) => {
                    if topology
                        .dependencies
                        .try_reserve(dependencies.len())
                        .is_err()
                    {
                        return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(
                            ElmEbiLoadStatus::RuntimeRejected,
                        ));
                    }
                    for dependency in dependencies {
                        if !topology.dependencies.iter().any(|existing| {
                            existing.0 == dependency.0 && existing.1 == dependency.1
                        }) {
                            topology.dependencies.push(dependency);
                        }
                    }
                    (imports, resolved_imports)
                }
                Err(status) => {
                    return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(status));
                }
            };
        if !self.native_exports_available(&image.unit) {
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(
                ElmEbiLoadStatus::RuntimeRejected,
            ));
        }
        if let Err(err) = self.insert_loaded_cell(
            id,
            parent,
            budget,
            manifest,
            name.clone(),
            image_arch,
            &image.unit,
            source,
            false,
        ) {
            log::error!("[elm] EBI image cell rejected by runtime: {:?}", err);
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::failed(
                ElmEbiLoadStatus::RuntimeRejected,
            ));
        }
        let loaded = match LoadedElmImage::load(id, &image, &imports) {
            Ok(loaded) => loaded,
            Err(ElmEbiLoadStatus::NativeCodeTodo) => {
                if self.cell_resource_over_quota(id, ElmResourceKind::PendingLoad) {
                    self.quarantine_cell_after_hook_failure(id);
                    return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                        ElmEbiLoadStatus::RuntimeRejected,
                        id.0,
                        state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                        ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    ));
                }
                if self.pending_ebi_loads.try_reserve(1).is_err() {
                    self.quarantine_cell_after_hook_failure(id);
                    return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                        ElmEbiLoadStatus::RuntimeRejected,
                        id.0,
                        state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                        ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    ));
                }
                let trust = match self.prepare_image_trust(&image, source) {
                    Ok(trust) => trust,
                    Err(status) => {
                        self.quarantine_cell_after_hook_failure(id);
                        return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                            status,
                            id.0,
                            state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                            ELM_LIFECYCLE_REASON_LEASE_BUSY,
                        ));
                    }
                };
                if grant_management && !self.grant_management_to_loaded_cell(id, &trust) {
                    self.abort_image_trust(&trust);
                    self.quarantine_cell_after_hook_failure(id);
                    return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                        ElmEbiLoadStatus::UntrustedImage,
                        id.0,
                        state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                        ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE,
                    ));
                }
                let approval = Self::kernel_api_grant_approval(source, kernel_api_grant, &trust);
                if let Err(err) = super::api_registry::grant_requirements(
                    id,
                    Generation::FIRST,
                    &image.unit.kernel_api_requirements,
                    approval,
                ) {
                    log::error!(
                        "[elm] 待处理镜像 Kernel API 依赖拒绝 cell={} name={}: {:?}",
                        id.0,
                        name,
                        err
                    );
                    self.abort_image_trust(&trust);
                    self.quarantine_cell_after_hook_failure(id);
                    return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                        ElmEbiLoadStatus::UntrustedImage,
                        id.0,
                        state_code(ElmState::Quarantined),
                        ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE,
                    ));
                }
                self.pending_ebi_loads.push(PendingEbiLoad {
                    cell: id,
                    unit: image.unit.clone(),
                    topology,
                    trust,
                });
                return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::NativeCodeTodo,
                    id.0,
                    state_code(ElmState::Loaded),
                    0,
                ));
            }
            Err(status) => {
                log::error!(
                    "[elm] 原生镜像装载器拒绝 cell={} name={} status={:?}",
                    id.0,
                    name,
                    status
                );
                self.quarantine_cell_after_hook_failure(id);
                return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                    status,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                ));
            }
        };
        if !self.native_image_reservation_fits(id, loaded.size() as u64) {
            self.quarantine_cell_after_hook_failure(id);
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            ));
        }
        if !self.native_provider_handlers_available(&image, &loaded) {
            log::error!(
                "[elm] 原生 provider 符号校验失败 cell={} name={}",
                id.0,
                name
            );
            self.quarantine_cell_after_hook_failure(id);
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            ));
        }
        let exports = match self.collect_native_exports(id, Generation::FIRST, &image, &loaded) {
            Ok(exports) => exports,
            Err(status) => {
                log::error!(
                    "[elm] 原生 export 收集失败 cell={} name={} status={:?}",
                    id.0,
                    name,
                    status
                );
                self.quarantine_cell_after_hook_failure(id);
                return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                    status,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                ));
            }
        };
        if self.native_exports.try_reserve(exports.len()).is_err()
            || self.native_images.try_reserve(1).is_err()
        {
            self.quarantine_cell_after_hook_failure(id);
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            ));
        }
        let token = match self.reserve_cell_execution_exclusive(id) {
            Ok(token) => token,
            Err(_) => {
                self.quarantine_cell_after_hook_failure(id);
                return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::RuntimeRejected,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                ));
            }
        };
        let trust = match self.prepare_image_trust(&image, source) {
            Ok(trust) => trust,
            Err(status) => {
                self.release_cell_execution(token);
                self.quarantine_cell_after_hook_failure(id);
                return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                    status,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                ));
            }
        };
        if grant_management && !self.grant_management_to_loaded_cell(id, &trust) {
            self.abort_image_trust(&trust);
            self.release_cell_execution(token);
            self.quarantine_cell_after_hook_failure(id);
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                ElmEbiLoadStatus::UntrustedImage,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE,
            ));
        }
        let approval = Self::kernel_api_grant_approval(source, kernel_api_grant, &trust);
        if let Err(err) = super::api_registry::grant_requirements(
            id,
            Generation::FIRST,
            &image.unit.kernel_api_requirements,
            approval,
        ) {
            log::error!(
                "[elm] 原生镜像 Kernel API 依赖拒绝 cell={} name={}: {:?}",
                id.0,
                name,
                err
            );
            self.abort_image_trust(&trust);
            self.release_cell_execution(token);
            self.quarantine_cell_after_hook_failure(id);
            return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                ElmEbiLoadStatus::UntrustedImage,
                id.0,
                state_code(ElmState::Quarantined),
                ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE,
            ));
        }
        let initialize = match self.lifecycle_context(id, ElmLifecyclePhase::Initialize) {
            Ok(context) => context,
            Err(_) => {
                self.abort_image_trust(&trust);
                self.release_cell_execution(token);
                self.quarantine_cell_after_hook_failure(id);
                return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::RuntimeRejected,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                ));
            }
        };
        let import_stage =
            match self.stage_native_imports(token, Generation::FIRST, resolved_native_imports) {
                Ok(stage) => stage,
                Err(()) => {
                    self.abort_image_trust(&trust);
                    self.release_cell_execution(token);
                    self.quarantine_cell_after_hook_failure(id);
                    return PreparedNativeLoad::Immediate(ElmLoadCellResponse::new(
                        ElmEbiLoadStatus::RuntimeRejected,
                        id.0,
                        state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                        ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    ));
                }
            };
        PreparedNativeLoad::Initialize(NativeLoadExecutionPlan {
            token,
            id,
            parent: self
                .cells
                .iter()
                .find(|cell| cell.id == id)
                .and_then(|cell| cell.parent),
            unit: image.unit,
            topology,
            loaded,
            exports,
            import_stage,
            initialize,
            trust,
        })
    }

    fn commit_native_load_initialize(
        &mut self,
        plan: NativeLoadExecutionPlan,
        result: ElmResult<()>,
    ) -> NativeLoadCommit {
        if result.is_err() {
            self.abort_image_trust(&plan.trust);
            self.discard_native_import_stage(plan.import_stage);
            self.release_cell_execution(plan.token);
            self.quarantine_cell_after_hook_failure(plan.id);
            return NativeLoadCommit::Complete(ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                plan.id.0,
                state_code(self.cell_state(plan.id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            ));
        }
        if !self.cell_execution_is_current(plan.token)
            || !self.native_import_stage_is_current(plan.import_stage)
        {
            return NativeLoadCommit::Finalize(self.abort_native_load_after_initialize(
                plan,
                ElmEbiLoadStatus::RuntimeRejected,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            ));
        }
        if let Err(err) =
            self.activate_loaded_cell(plan.id, &plan.unit, &plan.topology, Some(&plan.loaded))
        {
            log::error!("[elm] EBI image activation rejected by runtime: {:?}", err);
            return NativeLoadCommit::Finalize(self.abort_native_load_after_initialize(
                plan,
                ElmEbiLoadStatus::RuntimeRejected,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            ));
        }
        NativeLoadCommit::Entry(plan)
    }

    fn commit_native_load_entry(
        &mut self,
        plan: NativeLoadExecutionPlan,
        result: ElmResult<()>,
    ) -> NativeLoadCommit {
        if result.is_err()
            || !self.cell_execution_is_current(plan.token)
            || !self.native_import_stage_is_current(plan.import_stage)
        {
            return NativeLoadCommit::Finalize(self.abort_native_load_after_initialize(
                plan,
                ElmEbiLoadStatus::RuntimeRejected,
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            ));
        }
        if self.cell_resource_over_quota(plan.id, ElmResourceKind::NativeImage) {
            return NativeLoadCommit::Finalize(self.abort_native_load_after_initialize(
                plan,
                ElmEbiLoadStatus::RuntimeRejected,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            ));
        }
        if !self.reserve_native_import_stage_promotion(plan.import_stage) {
            return NativeLoadCommit::Finalize(self.abort_native_load_after_initialize(
                plan,
                ElmEbiLoadStatus::RuntimeRejected,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            ));
        }
        if let Err(err) = self.commit_image_trust(plan.id, &plan.trust) {
            log::error!(
                "[elm] trust acceptance commit failed cell={}: {:?}",
                plan.id.0,
                err
            );
            return NativeLoadCommit::Finalize(self.abort_native_load_after_initialize(
                plan,
                ElmEbiLoadStatus::RuntimeRejected,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            ));
        }
        if !self.promote_native_import_stage(plan.import_stage) {
            log::error!(
                "[elm] 原生 import 暂存事务在提交时丢失 cell={} generation={}",
                plan.id.0,
                plan.token.generation.0
            );
            self.rollback_activated_cell_to_quarantine(plan.id);
            self.release_cell_execution(plan.token);
            return NativeLoadCommit::Complete(ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                plan.id.0,
                state_code(self.cell_state(plan.id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            ));
        }
        self.native_exports.extend(plan.exports);
        if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == plan.id) {
            cell.ebi_status = ElmEbiLoadStatus::Ok;
            cell.lifecycle_executor_ready = true;
            cell.lifecycle_initialized = true;
            cell.lifecycle_finalized = false;
        }
        self.native_images.push(plan.loaded);
        self.release_cell_execution(plan.token);
        NativeLoadCommit::Complete(ElmLoadCellResponse::new(
            ElmEbiLoadStatus::Ok,
            plan.id.0,
            state_code(ElmState::Active),
            0,
        ))
    }

    fn abort_native_load_after_initialize(
        &mut self,
        plan: NativeLoadExecutionPlan,
        status: ElmEbiLoadStatus,
        reason: u32,
    ) -> NativeLoadFailurePlan {
        self.abort_image_trust(&plan.trust);
        let finalize = self.lifecycle_context_for_generation_lossy(
            plan.id,
            plan.token.generation,
            ElmLifecyclePhase::Finalize,
        );
        self.rollback_activated_cell_to_quarantine(plan.id);
        NativeLoadFailurePlan {
            id: plan.id,
            token: plan.token,
            import_stage: plan.import_stage,
            loaded: plan.loaded,
            finalize,
            response: ElmLoadCellResponse::new(
                status,
                plan.id.0,
                state_code(self.cell_state(plan.id).unwrap_or(ElmState::Quarantined)),
                reason,
            ),
        }
    }

    fn complete_native_load_failure(
        &mut self,
        id: ElmId,
        token: CellExecutionToken,
        import_stage: NativeImportStageKey,
        result: ElmResult<()>,
    ) {
        self.discard_native_import_stage(import_stage);
        self.release_cell_execution(token);
        if result.is_err() {
            self.mark_native_fault(id, ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED);
        }
    }

    fn prepare_native_replace_execution(
        &mut self,
        id: ElmId,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        migration_limit: u32,
        source: ElmEbiSourceKind,
        kernel_api_grant: KernelApiGrantRequest,
    ) -> PreparedNativeReplace {
        let old_state = self.cell_state(id).unwrap_or(ElmState::Retired);
        let old_generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .unwrap_or(Generation(0));
        if !image.has_code_segment() {
            return PreparedNativeReplace::Immediate(
                self.replace_declarative_cell_from_ebi_image_with_source_and_kernel_api_grant(
                    id,
                    image,
                    arch,
                    migration_limit,
                    source,
                    kernel_api_grant,
                ),
            );
        }
        if !self.native_unit_allowed_by_policy(id, &image.unit) {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_PERMISSION,
                old_state,
                old_generation,
                0,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CAPABILITY_DENIED),
                ELM_POLICY_BLOCK_CAPABILITY_DENIED,
            ));
        }
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(
            id.0,
            ElmLifecycleAction::Replace,
        ));
        if plan.allowed == 0 {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                plan.status,
                old_state,
                old_generation,
                0,
                first_lifecycle_reason(plan.blockers),
                plan.blockers,
            ));
        }
        if image.validate(arch).is_err() {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_INVALID,
                old_state,
                old_generation,
                0,
                first_lifecycle_reason(ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE),
                ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
            ));
        }
        let Some(old_cell_index) = self.cell_index(id) else {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_NOT_FOUND,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
                ELM_POLICY_BLOCK_CELL_NOT_FOUND,
            ));
        };
        let old_cell = self.cells[old_cell_index].clone();
        if image.unit.manifest.name.as_str() != old_cell.name
            || image.unit.manifest.kind != old_cell.kind
        {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_INVALID,
                old_state,
                old_generation,
                0,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            ));
        }
        let migration_capacity = if migration_limit == 0 {
            ELM_REPLACE_MIGRATION_STATE_MAX
        } else {
            migration_limit as usize
        };
        if migration_capacity > ELM_REPLACE_MIGRATION_STATE_MAX {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_INVALID,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
                ELM_POLICY_BLOCK_INVALID_STATE,
            ));
        }
        let mut topology = match self.preflight_ebi_topology_for_replace(id, &image.unit) {
            Ok(topology) => topology,
            Err(_) => {
                return PreparedNativeReplace::Immediate(self.replace_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    old_state,
                    old_generation,
                    0,
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                ));
            }
        };
        if !self.prepare_replace_commit_capacity(id, &image.unit) {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_BUSY,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            ));
        }
        let Some(new_generation) = old_generation.checked_next() else {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_BUSY,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            ));
        };
        let replace_parent = old_cell.parent.unwrap_or(ELM_MGR_ID);
        let (imports, resolved_native_imports) =
            match self.resolve_native_imports(id, replace_parent, new_generation, &image.unit) {
                Ok((imports, dependencies, resolved_imports)) => {
                    if dependencies.iter().any(|(provider, _)| *provider == id)
                        || resolved_imports.iter().any(|import| import.provider == id)
                    {
                        return PreparedNativeReplace::Immediate(self.replace_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            old_state,
                            old_generation,
                            0,
                            ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
                            ELM_POLICY_BLOCK_HAS_DEPENDENTS,
                        ));
                    }
                    if topology
                        .dependencies
                        .try_reserve(dependencies.len())
                        .is_err()
                    {
                        return PreparedNativeReplace::Immediate(self.replace_response(
                            id,
                            ELM_MGR_STATUS_BUSY,
                            old_state,
                            old_generation,
                            0,
                            ELM_LIFECYCLE_REASON_LEASE_BUSY,
                            ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                        ));
                    }
                    for dependency in dependencies {
                        if !topology.dependencies.iter().any(|existing| {
                            existing.0 == dependency.0 && existing.1 == dependency.1
                        }) {
                            topology.dependencies.push(dependency);
                        }
                    }
                    (imports, resolved_imports)
                }
                Err(_) => {
                    return PreparedNativeReplace::Immediate(self.replace_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        old_state,
                        old_generation,
                        0,
                        ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
                        ELM_POLICY_BLOCK_HAS_DEPENDENTS,
                    ));
                }
            };
        if !self.replace_surface_compatible(id, &image.unit, &topology) {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_INVALID,
                old_state,
                old_generation,
                0,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            ));
        }
        if !self.native_exports_available_for_replace(id, &image.unit) {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_INVALID,
                old_state,
                old_generation,
                0,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            ));
        }
        let loaded = match LoadedElmImage::load(id, &image, &imports) {
            Ok(loaded) => loaded,
            Err(ElmEbiLoadStatus::NativeCodeTodo) => {
                return PreparedNativeReplace::Immediate(self.replace_response(
                    id,
                    ELM_MGR_STATUS_TODO,
                    old_state,
                    old_generation,
                    0,
                    ELM_LIFECYCLE_REASON_NATIVE_TODO,
                    ELM_POLICY_BLOCK_NATIVE_TODO,
                ));
            }
            Err(_) => {
                return PreparedNativeReplace::Immediate(self.replace_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    old_state,
                    old_generation,
                    0,
                    ELM_LIFECYCLE_REASON_HOOK_FAILED,
                    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                ));
            }
        };
        if !self.native_image_reservation_fits(id, loaded.size() as u64) {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_BUSY,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            ));
        }
        if !self.native_provider_handlers_available(&image, &loaded) {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_INVALID,
                old_state,
                old_generation,
                0,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            ));
        }
        let exports = match self.collect_native_exports(id, new_generation, &image, &loaded) {
            Ok(exports) => exports,
            Err(_) => {
                return PreparedNativeReplace::Immediate(self.replace_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    old_state,
                    old_generation,
                    0,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                    ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                ));
            }
        };
        if !self.can_rebind_native_importers_for_replace(id, &exports) {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_BUSY,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
                ELM_POLICY_BLOCK_HAS_DEPENDENTS,
            ));
        }
        if self.native_exports.try_reserve(exports.len()).is_err()
            || self.retired_native_images.try_reserve(1).is_err()
        {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_BUSY,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            ));
        }
        let Some(old_image_index) = self.native_image_index(id) else {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_TODO,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_NATIVE_TODO,
                ELM_POLICY_BLOCK_NATIVE_TODO,
            ));
        };
        let new_initialize = match self.lifecycle_context_for_generation(
            id,
            new_generation,
            ElmLifecyclePhase::Initialize,
        ) {
            Ok(context) => context,
            Err(_) => {
                return PreparedNativeReplace::Immediate(self.replace_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    old_state,
                    old_generation,
                    0,
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                ));
            }
        };
        let old_quiesce = if old_state == ElmState::Active {
            Some(self.lifecycle_context_for_generation_lossy(
                id,
                old_generation,
                ElmLifecyclePhase::Quiesce,
            ))
        } else {
            None
        };
        let old_finalize = self.lifecycle_context_for_generation_lossy(
            id,
            old_generation,
            ElmLifecyclePhase::Finalize,
        );
        let old_resume = if old_state == ElmState::Active {
            Some(self.lifecycle_context_for_generation_lossy(
                id,
                old_generation,
                ElmLifecyclePhase::Resume,
            ))
        } else {
            None
        };
        let new_finalize = self.lifecycle_context_for_generation_lossy(
            id,
            new_generation,
            ElmLifecyclePhase::Finalize,
        );
        let mut migration = Vec::new();
        if migration.try_reserve_exact(migration_capacity).is_err() {
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                ELM_MGR_STATUS_BUSY,
                old_state,
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_NATIVE_TODO,
                ELM_POLICY_BLOCK_NATIVE_TODO,
            ));
        }
        migration.resize(migration_capacity, 0);
        let token = match self.reserve_cell_execution_exclusive(id) {
            Ok(token) => token,
            Err(status) => {
                return PreparedNativeReplace::Immediate(self.replace_response(
                    id,
                    status,
                    old_state,
                    old_generation,
                    0,
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    ELM_POLICY_BLOCK_PROVIDER_BUSY,
                ));
            }
        };
        let suspended_projection_sources =
            match super::source::suspend_projection_sources(id, old_generation) {
                Ok(count) => count,
                Err(_) => {
                    self.release_cell_execution(token);
                    return PreparedNativeReplace::Immediate(self.replace_response(
                        id,
                        ELM_MGR_STATUS_BUSY,
                        old_state,
                        old_generation,
                        0,
                        ELM_LIFECYCLE_REASON_LEASE_BUSY,
                        ELM_POLICY_BLOCK_PROVIDER_BUSY,
                    ));
                }
            };
        let trust = match self.prepare_image_trust(&image, source) {
            Ok(trust) => trust,
            Err(status) => {
                let sources_restored = self.resume_projection_sources_for_cell(id, old_generation);
                self.release_cell_execution(token);
                let mut blockers = trust_blocker(status);
                if !sources_restored {
                    blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
                    self.quarantine_cell_after_hook_failure(id);
                }
                return PreparedNativeReplace::Immediate(self.replace_response(
                    id,
                    status_from_blockers(blockers),
                    self.cell_state(id).unwrap_or(old_state),
                    old_generation,
                    0,
                    first_lifecycle_reason(blockers),
                    blockers,
                ));
            }
        };
        if self.cell_requires_signed_management(id)
            && (trust.unsigned || trust.acceptance.is_none())
        {
            self.abort_image_trust(&trust);
            let sources_restored = self.resume_projection_sources_for_cell(id, old_generation);
            self.release_cell_execution(token);
            let mut blockers = ELM_POLICY_BLOCK_UNTRUSTED_IMAGE;
            if !sources_restored {
                blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
                self.quarantine_cell_after_hook_failure(id);
            }
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                status_from_blockers(blockers),
                self.cell_state(id).unwrap_or(old_state),
                old_generation,
                0,
                ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE,
                blockers,
            ));
        }
        let import_stage =
            match self.stage_native_imports(token, new_generation, resolved_native_imports) {
                Ok(stage) => stage,
                Err(()) => {
                    self.abort_image_trust(&trust);
                    let sources_restored =
                        self.resume_projection_sources_for_cell(id, old_generation);
                    self.release_cell_execution(token);
                    let mut blockers = ELM_POLICY_BLOCK_RESOURCE_QUOTA;
                    if !sources_restored {
                        blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
                        self.quarantine_cell_after_hook_failure(id);
                    }
                    return PreparedNativeReplace::Immediate(self.replace_response(
                        id,
                        status_from_blockers(blockers),
                        self.cell_state(id).unwrap_or(old_state),
                        old_generation,
                        0,
                        first_lifecycle_reason(blockers),
                        blockers,
                    ));
                }
            };
        if let Err(err) = super::api_registry::grant_requirements(
            id,
            new_generation,
            &image.unit.kernel_api_requirements,
            Self::kernel_api_grant_approval(source, kernel_api_grant, &trust),
        ) {
            log::error!(
                "[elm] 替换镜像 Kernel API 依赖拒绝 cell={} generation={}: {:?}",
                id.0,
                new_generation.0,
                err
            );
            self.discard_native_import_stage(import_stage);
            self.abort_image_trust(&trust);
            let sources_restored = self.resume_projection_sources_for_cell(id, old_generation);
            self.release_cell_execution(token);
            let mut blockers = ELM_POLICY_BLOCK_CAPABILITY_DENIED;
            if !sources_restored {
                blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
                self.quarantine_cell_after_hook_failure(id);
            }
            return PreparedNativeReplace::Immediate(self.replace_response(
                id,
                status_from_blockers(blockers),
                self.cell_state(id).unwrap_or(old_state),
                old_generation,
                0,
                first_lifecycle_reason(blockers),
                blockers,
            ));
        }
        let new_executor = loaded.lifecycle_executor();
        PreparedNativeReplace::Execute(NativeReplaceExecutionPlan {
            token,
            id,
            old_state,
            old_generation,
            new_generation,
            suspended_projection_sources,
            unit: image.unit,
            loaded,
            exports,
            import_stage,
            old_executor: self.native_images[old_image_index].lifecycle_executor(),
            new_executor,
            new_initialize,
            new_finalize,
            old_quiesce,
            old_finalize,
            old_resume,
            migration,
            trust,
        })
    }

    fn complete_native_replace_execution(
        &mut self,
        mut plan: NativeReplaceExecutionPlan,
        outcome: NativeReplaceExecutionOutcome,
    ) -> ElmReplaceCellResponseV1 {
        let current = self.cell_execution_is_current(plan.token)
            && self.native_import_stage_is_current(plan.import_stage);
        if outcome.commit && current {
            if !self.reserve_native_import_stage_promotion(plan.import_stage) {
                let old_recovered = recover_old_replace_generation(
                    &mut plan.old_executor,
                    plan.old_resume.as_mut(),
                    outcome.old_execution,
                );
                let sources_restored = self.rollback_projection_source_replace(
                    plan.id,
                    plan.old_generation,
                    plan.new_generation,
                );
                self.abort_image_trust(&plan.trust);
                self.discard_native_import_stage(plan.import_stage);
                super::api_registry::remove_generation(plan.id, plan.new_generation);
                self.release_cell_execution(plan.token);
                if !old_recovered || !sources_restored {
                    self.quarantine_cell_after_hook_failure(plan.id);
                }
                return self.replace_response(
                    plan.id,
                    ELM_MGR_STATUS_BUSY,
                    self.cell_state(plan.id).unwrap_or(plan.old_state),
                    plan.old_generation,
                    outcome.migrated_len as u32,
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                );
            }
            if let Err(err) = self.commit_image_trust_acceptance(&plan.trust) {
                log::error!(
                    "[elm] 替换提交前无法固化镜像信任 cell={}: {:?}",
                    plan.id.0,
                    err
                );
                let old_recovered = recover_old_replace_generation(
                    &mut plan.old_executor,
                    plan.old_resume.as_mut(),
                    outcome.old_execution,
                );
                let sources_restored = self.rollback_projection_source_replace(
                    plan.id,
                    plan.old_generation,
                    plan.new_generation,
                );
                self.discard_native_import_stage(plan.import_stage);
                super::api_registry::remove_generation(plan.id, plan.new_generation);
                self.release_cell_execution(plan.token);
                if !old_recovered || !sources_restored {
                    self.quarantine_cell_after_hook_failure(plan.id);
                }
                return self.replace_response(
                    plan.id,
                    ELM_MGR_STATUS_INVALID,
                    self.cell_state(plan.id).unwrap_or(plan.old_state),
                    plan.old_generation,
                    outcome.migrated_len as u32,
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                );
            }
            let committed = super::source::commit_projection_source_generation(
                plan.id,
                plan.old_generation,
                plan.new_generation,
                || {
                    self.commit_replaced_cell(
                        plan.id,
                        plan.old_state,
                        plan.new_generation,
                        &plan.unit,
                        &plan.loaded,
                        plan.exports,
                        plan.import_stage,
                    )
                },
            )
            .is_ok_and(|(_, committed)| committed);
            if !committed {
                let sources_restored = self.rollback_projection_source_replace(
                    plan.id,
                    plan.old_generation,
                    plan.new_generation,
                );
                let old_recovered = recover_old_replace_generation(
                    &mut plan.old_executor,
                    plan.old_resume.as_mut(),
                    outcome.old_execution,
                );
                if !sources_restored || !old_recovered {
                    self.quarantine_cell_after_hook_failure(plan.id);
                }
                self.discard_native_import_stage(plan.import_stage);
                super::api_registry::remove_generation(plan.id, plan.new_generation);
                self.release_cell_execution(plan.token);
                return self.replace_response(
                    plan.id,
                    ELM_MGR_STATUS_INVALID,
                    self.cell_state(plan.id).unwrap_or(plan.old_state),
                    plan.old_generation,
                    outcome.migrated_len as u32,
                    ELM_LIFECYCLE_REASON_HOOK_FAILED,
                    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                );
            }
            self.apply_image_trust_metadata(plan.id, &plan.trust);
            if plan
                .old_executor
                .on_finalize(&mut plan.old_finalize)
                .is_err()
            {
                log::error!(
                    "[elm] 旧代际已切换但 finalize 失败 cell={} generation={}",
                    plan.id.0,
                    plan.old_generation.0
                );
            }
            self.retire_replaced_native_image(plan.id, plan.old_generation);
            super::api_registry::remove_generation(plan.id, plan.old_generation);
            self.native_images.push(plan.loaded);
            self.release_cell_execution(plan.token);
            return self.replace_response(
                plan.id,
                ELM_MGR_STATUS_OK,
                plan.old_state,
                plan.new_generation,
                outcome.migrated_len as u32,
                ELM_LIFECYCLE_REASON_NONE,
                0,
            );
        }

        let old_recovered = recover_old_replace_generation(
            &mut plan.old_executor,
            plan.old_resume.as_mut(),
            outcome.old_execution,
        );
        let sources_restored = self.rollback_projection_source_replace(
            plan.id,
            plan.old_generation,
            plan.new_generation,
        );
        self.abort_image_trust(&plan.trust);
        self.discard_native_import_stage(plan.import_stage);
        super::api_registry::remove_generation(plan.id, plan.new_generation);
        if !old_recovered || !sources_restored {
            self.quarantine_cell_after_hook_failure(plan.id);
        }
        self.release_cell_execution(plan.token);
        let mut blockers = if outcome.commit && !current {
            ELM_POLICY_BLOCK_PROVIDER_BUSY | ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED
        } else {
            outcome.blockers
        };
        if !sources_restored {
            blockers |= ELM_POLICY_BLOCK_GRAPH_INCONSISTENT;
        }
        let status = if outcome.commit && !current {
            ELM_MGR_STATUS_BUSY
        } else {
            outcome.status
        };
        let reason = if outcome.commit && !current {
            ELM_LIFECYCLE_REASON_HOOK_FAILED
        } else {
            outcome.reason
        };
        self.replace_response(
            plan.id,
            status,
            self.cell_state(plan.id).unwrap_or(plan.old_state),
            plan.old_generation,
            outcome.migrated_len as u32,
            reason,
            blockers,
        )
    }

    pub(crate) fn replace_declarative_cell_from_ebi_image_with_source(
        &mut self,
        id: ElmId,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        migration_limit: u32,
        source: ElmEbiSourceKind,
    ) -> ElmReplaceCellResponseV1 {
        self.replace_declarative_cell_from_ebi_image_with_source_and_kernel_api_grant(
            id,
            image,
            arch,
            migration_limit,
            source,
            KernelApiGrantRequest::none(),
        )
    }

    pub(crate) fn replace_declarative_cell_from_ebi_image_with_source_and_kernel_api_grant(
        &mut self,
        id: ElmId,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        migration_limit: u32,
        source: ElmEbiSourceKind,
        kernel_api_grant: KernelApiGrantRequest,
    ) -> ElmReplaceCellResponseV1 {
        let old_state = self.cell_state(id).unwrap_or(ElmState::Retired);
        let old_generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .unwrap_or(Generation(0));
        let fail = |this: &mut Self, status, blockers, reason| {
            this.replace_response(id, status, old_state, old_generation, 0, reason, blockers)
        };

        // 局部 Core 入口只允许纯声明式替换，原生生命周期必须由正式脱锁事务执行。
        if image.has_code_segment() {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
            );
        }
        if !self.native_unit_allowed_by_policy(id, &image.unit) {
            return fail(
                self,
                ELM_MGR_STATUS_PERMISSION,
                ELM_POLICY_BLOCK_CAPABILITY_DENIED,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CAPABILITY_DENIED),
            );
        }
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(
            id.0,
            ElmLifecycleAction::Replace,
        ));
        if plan.allowed == 0 {
            return fail(
                self,
                plan.status,
                plan.blockers,
                first_lifecycle_reason(plan.blockers),
            );
        }
        if image.validate(arch).is_err() {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
                first_lifecycle_reason(ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE),
            );
        }
        let Some(old_cell) = self.cells.iter().find(|cell| cell.id == id) else {
            return fail(
                self,
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_CELL_NOT_FOUND,
                ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
            );
        };
        if image.unit.manifest.name.as_str() != old_cell.name
            || image.unit.manifest.kind != old_cell.kind
            || !image.unit.imports.is_empty()
            || !image.unit.exports.is_empty()
        {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
            );
        }
        let migration_capacity = if migration_limit == 0 {
            ELM_REPLACE_MIGRATION_STATE_MAX
        } else {
            migration_limit as usize
        };
        if migration_capacity > ELM_REPLACE_MIGRATION_STATE_MAX {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
            );
        }
        let topology = match self.preflight_ebi_topology_for_replace(id, &image.unit) {
            Ok(topology) => topology,
            Err(_) => {
                return fail(
                    self,
                    ELM_MGR_STATUS_INVALID,
                    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                );
            }
        };
        if !self.replace_surface_compatible(id, &image.unit, &topology) {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
            );
        }
        if !self.prepare_replace_commit_capacity(id, &image.unit) {
            return fail(
                self,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
            );
        }
        let Some(new_generation) = old_generation.checked_next() else {
            return fail(
                self,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
            );
        };
        let trust = match self.prepare_image_trust(&image, source) {
            Ok(trust) => trust,
            Err(status) => {
                let blockers = trust_blocker(status);
                return fail(
                    self,
                    status_from_blockers(blockers),
                    blockers,
                    first_lifecycle_reason(blockers),
                );
            }
        };
        if self.cell_requires_signed_management(id)
            && (trust.unsigned || trust.acceptance.is_none())
        {
            self.abort_image_trust(&trust);
            return fail(
                self,
                ELM_MGR_STATUS_PERMISSION,
                ELM_POLICY_BLOCK_UNTRUSTED_IMAGE,
                ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE,
            );
        }
        if let Err(err) = super::api_registry::grant_requirements(
            id,
            new_generation,
            &image.unit.kernel_api_requirements,
            Self::kernel_api_grant_approval(source, kernel_api_grant, &trust),
        ) {
            log::error!(
                "[elm] 声明式替换 Kernel API 依赖拒绝 cell={} generation={}: {:?}",
                id.0,
                new_generation.0,
                err
            );
            self.abort_image_trust(&trust);
            return fail(
                self,
                ELM_MGR_STATUS_PERMISSION,
                ELM_POLICY_BLOCK_CAPABILITY_DENIED,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CAPABILITY_DENIED),
            );
        }
        if !self.commit_replaced_declarative_cell(id, old_state, new_generation, &image.unit) {
            super::api_registry::remove_generation(id, new_generation);
            self.abort_image_trust(&trust);
            return fail(
                self,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
            );
        }
        if let Err(err) = self.commit_image_trust(id, &trust) {
            log::error!(
                "[elm] trust acceptance commit failed cell={}: {:?}",
                id.0,
                err
            );
            self.quarantine_cell_after_hook_failure(id);
            return self.replace_response(
                id,
                ELM_MGR_STATUS_BUSY,
                self.cell_state(id).unwrap_or(ElmState::Quarantined),
                new_generation,
                0,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
        }
        super::api_registry::remove_generation(id, old_generation);
        self.replace_response(
            id,
            ELM_MGR_STATUS_OK,
            old_state,
            new_generation,
            0,
            ELM_LIFECYCLE_REASON_NONE,
            0,
        )
    }
    #[allow(dead_code)]
    pub(crate) fn load_ebi_unit_with_lifecycle_executor(
        &mut self,
        unit: ElmEbiUnit,
        arch: ElmEbiArch,
        executor: &mut dyn ElmLifecycleExecutor,
    ) -> ElmLoadCellResponse {
        self.load_ebi_unit_inner(
            unit,
            arch,
            ElmEbiSourceKind::Memory,
            ELM_MGR_ID,
            ElmResourceBudget::DEFAULT,
            false,
            super::api_registry::ApiGrantApproval::internal(ElmEbiSourceKind::Memory),
            Some(executor),
        )
    }

    fn load_ebi_unit_inner(
        &mut self,
        unit: ElmEbiUnit,
        arch: ElmEbiArch,
        source: ElmEbiSourceKind,
        parent: ElmId,
        budget: ElmResourceBudget,
        grant_management: bool,
        kernel_api_approval: Option<super::api_registry::ApiGrantApproval>,
        mut executor: Option<&mut dyn ElmLifecycleExecutor>,
    ) -> ElmLoadCellResponse {
        if let Err(status) = unit.validate(arch) {
            return ElmLoadCellResponse::failed(status);
        }
        if let Err(status) = self.select_elmapi_version(&unit) {
            return ElmLoadCellResponse::failed(status);
        }
        if unit.api_compatibility.is_some() && !unit.has_native_code() {
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::InvalidTarget);
        }
        let topology = match self.preflight_ebi_topology(&unit) {
            Ok(topology) => topology,
            Err(err) => {
                log::error!("[elm] EBI topology rejected by runtime: {:?}", err);
                return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
            }
        };
        let manifest = unit.manifest.clone();
        let name = manifest.name.as_str().to_string();
        let image_arch = unit.target.arch;
        let Some(id) = self.alloc_cell_id() else {
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        };

        if let Err(err) = self.insert_loaded_cell(
            id,
            parent,
            budget,
            manifest,
            name,
            image_arch,
            &unit,
            source,
            grant_management,
        ) {
            log::error!("[elm] EBI cell rejected by runtime: {:?}", err);
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        }
        if let Err(err) = super::api_registry::grant_requirements(
            id,
            Generation::FIRST,
            &unit.kernel_api_requirements,
            kernel_api_approval,
        ) {
            log::error!("[elm] Kernel API 依赖拒绝 cell={}: {:?}", id.0, err);
            self.quarantine_cell_after_hook_failure(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(ElmState::Quarantined),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        }

        if unit.has_native_code() {
            let requires_native_image_loader = unit_requires_native_image_loader(&unit);
            if self.cell_resource_over_quota(id, ElmResourceKind::PendingLoad) {
                self.quarantine_cell_after_hook_failure(id);
                return ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::RuntimeRejected,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                );
            }
            if self.pending_ebi_loads.try_reserve(1).is_err() {
                self.quarantine_cell_after_hook_failure(id);
                return ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::RuntimeRejected,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                );
            }
            self.pending_ebi_loads.push(PendingEbiLoad {
                cell: id,
                unit: unit.clone(),
                topology: topology.clone(),
                trust: PreparedImageTrust::internal(),
            });
            if let Some(executor) = executor.as_deref_mut()
                && !requires_native_image_loader
            {
                return self.initialize_pending_ebi_load(id, executor);
            }
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::NativeCodeTodo,
                id.0,
                state_code(ElmState::Loaded),
                0,
            );
        }

        if let Some(executor) = executor.as_deref_mut() {
            let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Initialize) else {
                self.quarantine_cell_after_hook_failure(id);
                return ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::RuntimeRejected,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                );
            };
            if executor.on_initialize(&mut context).is_err() {
                self.quarantine_cell_after_hook_failure(id);
                return ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::RuntimeRejected,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_HOOK_FAILED,
                );
            }
        }

        if let Err(err) = self.activate_loaded_cell(id, &unit, &topology, None) {
            log::error!("[elm] EBI cell activation rejected by runtime: {:?}", err);
            self.rollback_activated_cell_to_quarantine(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        }
        if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) {
            cell.ebi_status = ElmEbiLoadStatus::Ok;
            cell.lifecycle_executor_ready = true;
            cell.lifecycle_initialized = true;
            cell.lifecycle_finalized = false;
        }

        ElmLoadCellResponse::new(ElmEbiLoadStatus::Ok, id.0, state_code(ElmState::Active), 0)
    }

    fn initialize_pending_ebi_load(
        &mut self,
        id: ElmId,
        executor: &mut dyn ElmLifecycleExecutor,
    ) -> ElmLoadCellResponse {
        let Some(pending) = self.take_pending_ebi_load(id) else {
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        };
        let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Initialize) else {
            self.abort_image_trust(&pending.trust);
            self.rollback_activated_cell_to_quarantine(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        };

        if executor.on_initialize(&mut context).is_err() {
            self.quarantine_cell_after_hook_failure(id);
            self.abort_image_trust(&pending.trust);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            );
        }

        if let Err(err) = self.activate_loaded_cell(id, &pending.unit, &pending.topology, None) {
            log::error!("[elm] EBI cell activation rejected by runtime: {:?}", err);
            self.rollback_activated_cell_to_quarantine(id);
            self.abort_image_trust(&pending.trust);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        }

        if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) {
            cell.ebi_status = ElmEbiLoadStatus::Ok;
            cell.lifecycle_executor_ready = true;
            cell.lifecycle_initialized = true;
            cell.lifecycle_finalized = false;
        }
        if let Err(err) = self.commit_image_trust(id, &pending.trust) {
            log::error!(
                "[elm] trust acceptance commit failed cell={}: {:?}",
                id.0,
                err
            );
            self.rollback_activated_cell_to_quarantine(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            );
        }
        ElmLoadCellResponse::new(ElmEbiLoadStatus::Ok, id.0, state_code(ElmState::Active), 0)
    }

    fn prepare_native_lifecycle_execution(
        &mut self,
        id: ElmId,
        action: ElmLifecycleAction,
    ) -> PreparedNativeLifecycle {
        let native_image_index = self.native_image_index(id);
        let executor_available = native_image_index.is_some();
        let plan = self.preflight_lifecycle_inner(
            ElmLifecyclePlanRequest::new(id.0, action),
            executor_available,
        );
        if plan.allowed == 0 {
            return PreparedNativeLifecycle::Immediate(
                self.lifecycle_response_from_plan(action, plan, 0, 0),
            );
        }

        let state = self.cell_state(id).unwrap_or(ElmState::Retired);
        let needs_external_execution = match action {
            ElmLifecycleAction::Pause => executor_available && state == ElmState::Active,
            ElmLifecycleAction::Resume => executor_available && state == ElmState::Paused,
            // 声明式单元也必须在 Core 锁外排空其拥有的子系统资源。
            ElmLifecycleAction::Detach => true,
            ElmLifecycleAction::Replace => false,
        };
        if !needs_external_execution {
            let response = match action {
                ElmLifecycleAction::Pause => self.pause_cell(id),
                ElmLifecycleAction::Resume => self.resume_cell(id),
                ElmLifecycleAction::Detach => self.detach_cell(id),
                ElmLifecycleAction::Replace => self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_INVALID_STATE,
                    0,
                    0,
                ),
            };
            return PreparedNativeLifecycle::Immediate(response);
        }

        let executor =
            native_image_index.map(|index| self.native_images[index].lifecycle_executor());
        let work = match action {
            ElmLifecycleAction::Pause => {
                let quiesce = match self.lifecycle_context(id, ElmLifecyclePhase::Quiesce) {
                    Ok(context) => context,
                    Err(_) => {
                        return PreparedNativeLifecycle::Immediate(self.finish_lifecycle(
                            action,
                            self.lifecycle_response(
                                id,
                                ELM_MGR_STATUS_INVALID,
                                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                                0,
                                0,
                            ),
                            ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                        ));
                    }
                };
                let mut pause = match self.lifecycle_context(id, ElmLifecyclePhase::Pause) {
                    Ok(context) => context,
                    Err(_) => {
                        return PreparedNativeLifecycle::Immediate(self.finish_lifecycle(
                            action,
                            self.lifecycle_response(
                                id,
                                ELM_MGR_STATUS_INVALID,
                                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                                0,
                                0,
                            ),
                            ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                        ));
                    }
                };
                pause.set_state(ElmState::Quiescing);
                NativeLifecycleWork::Pause { quiesce, pause }
            }
            ElmLifecycleAction::Resume => {
                let resume = match self.lifecycle_context(id, ElmLifecyclePhase::Resume) {
                    Ok(context) => context,
                    Err(_) => {
                        return PreparedNativeLifecycle::Immediate(self.finish_lifecycle(
                            action,
                            self.lifecycle_response(
                                id,
                                ELM_MGR_STATUS_INVALID,
                                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                                0,
                                0,
                            ),
                            ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                        ));
                    }
                };
                NativeLifecycleWork::Resume { resume }
            }
            ElmLifecycleAction::Detach => {
                let generation = self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .map(|cell| cell.generation)
                    .unwrap_or(Generation(0));
                let quiesce = if executor_available && state == ElmState::Active {
                    match self.lifecycle_context(id, ElmLifecyclePhase::Quiesce) {
                        Ok(context) => Some(context),
                        Err(_) => {
                            return PreparedNativeLifecycle::Immediate(self.finish_lifecycle(
                                action,
                                self.lifecycle_response(
                                    id,
                                    ELM_MGR_STATUS_INVALID,
                                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                                    0,
                                    0,
                                ),
                                ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                            ));
                        }
                    }
                } else {
                    None
                };
                let finalize = if executor_available && self.cell_needs_finalize(id) {
                    match self.lifecycle_context(id, ElmLifecyclePhase::Finalize) {
                        Ok(context) => Some(context),
                        Err(_) => {
                            return PreparedNativeLifecycle::Immediate(self.finish_lifecycle(
                                action,
                                self.lifecycle_response(
                                    id,
                                    ELM_MGR_STATUS_INVALID,
                                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                                    0,
                                    0,
                                ),
                                ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                            ));
                        }
                    }
                } else {
                    None
                };
                NativeLifecycleWork::Detach {
                    quiesce,
                    finalize,
                    owner: id,
                    generation,
                }
            }
            ElmLifecycleAction::Replace => {
                return PreparedNativeLifecycle::Immediate(self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_INVALID_STATE,
                    0,
                    0,
                ));
            }
        };
        let token = match self.reserve_cell_execution_exclusive(id) {
            Ok(token) => token,
            Err(status) => {
                let blockers = ELM_POLICY_BLOCK_PROVIDER_BUSY;
                return PreparedNativeLifecycle::Immediate(self.finish_lifecycle(
                    action,
                    self.lifecycle_response(id, status, first_lifecycle_reason(blockers), 0, 0),
                    blockers,
                ));
            }
        };
        let source_suspension = if matches!(
            action,
            ElmLifecycleAction::Pause | ElmLifecycleAction::Detach
        ) {
            match super::source::suspend_projection_sources_guard(id, token.generation) {
                Ok(suspension) => Some(suspension),
                Err(_) => {
                    self.release_cell_execution(token);
                    let blockers = ELM_POLICY_BLOCK_PROVIDER_BUSY;
                    return PreparedNativeLifecycle::Immediate(self.finish_lifecycle(
                        action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_BUSY,
                            ELM_LIFECYCLE_REASON_LEASE_BUSY,
                            0,
                            0,
                        ),
                        blockers,
                    ));
                }
            }
        } else {
            None
        };
        PreparedNativeLifecycle::External(NativeLifecycleExecutionPlan {
            token,
            action,
            executor,
            work,
            source_suspension,
        })
    }

    fn complete_native_lifecycle_execution(
        &mut self,
        mut plan: NativeLifecycleExecutionPlan,
        outcome: NativeLifecycleExecutionOutcome,
    ) -> ElmLifecycleResponse {
        let id = plan.token.cell;
        let current = self.cell_execution_is_current(plan.token);
        self.release_cell_execution(plan.token);
        if !current {
            if let Some(suspension) = plan.source_suspension.take() {
                let _ = suspension.keep_suspended();
            }
            self.quarantine_cell_after_hook_failure(id);
            let blockers = ELM_POLICY_BLOCK_PROVIDER_BUSY;
            return self.finish_lifecycle(
                plan.action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_BUSY,
                    first_lifecycle_reason(blockers),
                    0,
                    0,
                ),
                blockers,
            );
        }
        if outcome.result.is_err() {
            if let Some(suspension) = plan.source_suspension.take() {
                let _ = suspension.keep_suspended();
            }
            if outcome.drained_resources != 0 || outcome.blockers & ELM_POLICY_BLOCK_LEASE_BUSY != 0
            {
                self.push_resource_trace(
                    id.0,
                    u64::from(outcome.drained_resources),
                    status_from_blockers(outcome.blockers),
                    outcome.blockers,
                );
            }
            self.quarantine_cell_after_hook_failure(id);
            return self.finish_lifecycle(
                plan.action,
                self.lifecycle_response(
                    id,
                    status_from_blockers(outcome.blockers),
                    outcome.reason,
                    0,
                    0,
                ),
                outcome.blockers,
            );
        }

        match plan.action {
            ElmLifecycleAction::Pause => {
                if self.transition_cell_state(id, ElmState::Quiescing).is_err()
                    || self.transition_cell_state(id, ElmState::Paused).is_err()
                {
                    if self.transition_cell_state(id, ElmState::Active).is_err() {
                        if let Some(suspension) = plan.source_suspension.take() {
                            let _ = suspension.keep_suspended();
                        }
                        self.quarantine_cell_after_hook_failure(id);
                    }
                    return self.finish_lifecycle(
                        plan.action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                            0,
                            0,
                        ),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
                if let Some(suspension) = plan.source_suspension.take() {
                    let _ = suspension.keep_suspended();
                }
                self.finish_lifecycle(
                    plan.action,
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0),
                    0,
                )
            }
            ElmLifecycleAction::Resume => {
                if !self.resume_projection_sources_for_cell(id, plan.token.generation) {
                    self.quarantine_cell_after_hook_failure(id);
                    return self.finish_lifecycle(
                        plan.action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                            0,
                            0,
                        ),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
                if self.transition_cell_state(id, ElmState::Active).is_err() {
                    let _ = super::source::suspend_projection_sources(id, plan.token.generation);
                    self.quarantine_cell_after_hook_failure(id);
                    return self.finish_lifecycle(
                        plan.action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                            0,
                            0,
                        ),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
                self.finish_lifecycle(
                    plan.action,
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0),
                    0,
                )
            }
            ElmLifecycleAction::Detach => {
                if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) {
                    cell.lifecycle_finalized = true;
                }
                let Some(source_suspension) = plan.source_suspension.take() else {
                    self.quarantine_cell_after_hook_failure(id);
                    return self.finish_lifecycle(
                        plan.action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                            0,
                            0,
                        ),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                };
                let response =
                    self.commit_detached_cell(id, plan.token.generation, source_suspension);
                if response.status == ELM_MGR_STATUS_OK {
                    self.remove_native_image(id);
                }
                response
            }
            ElmLifecycleAction::Replace => self.finish_lifecycle(
                plan.action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_INVALID_STATE,
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_INVALID_STATE,
            ),
        }
    }

    pub fn pause_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        let action = ElmLifecycleAction::Pause;
        // 局部 Core 入口不得执行原生钩子；原生单元由正式脱锁生命周期事务处理。
        if self.native_image_index(id).is_some() {
            return self.finish_lifecycle(
                action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_INVALID_STATE,
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(id.0, action));
        if plan.allowed == 0 {
            return self.lifecycle_response_from_plan(action, plan, 0, 0);
        }

        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .unwrap_or(Generation(0));
        let source_suspension =
            match super::source::suspend_projection_sources_guard(id, generation) {
                Ok(suspension) => suspension,
                Err(_) => {
                    let response = self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_BUSY,
                        ELM_LIFECYCLE_REASON_LEASE_BUSY,
                        0,
                        0,
                    );
                    return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_PROVIDER_BUSY);
                }
            };

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Active => {
                if self.transition_cell_state(id, ElmState::Quiescing).is_err()
                    || self.transition_cell_state(id, ElmState::Paused).is_err()
                {
                    if self.transition_cell_state(id, ElmState::Active).is_err() {
                        let _ = source_suspension.keep_suspended();
                        self.quarantine_cell_after_hook_failure(id);
                    }
                    let response = self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                        0,
                        0,
                    );
                    return self.finish_lifecycle(
                        action,
                        response,
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
                let _ = source_suspension.keep_suspended();
                self.finish_lifecycle(
                    action,
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0),
                    0,
                )
            }
            ElmState::Paused => {
                let _ = source_suspension.keep_suspended();
                self.finish_lifecycle(
                    action,
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0),
                    0,
                )
            }
            _ => self.finish_lifecycle(
                action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_INVALID_STATE),
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_INVALID_STATE,
            ),
        }
    }
    pub fn resume_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        let action = ElmLifecycleAction::Resume;
        // 局部 Core 入口不得执行原生钩子；原生单元由正式脱锁生命周期事务处理。
        if self.native_image_index(id).is_some() {
            return self.finish_lifecycle(
                action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_INVALID_STATE,
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(id.0, action));
        if plan.allowed == 0 {
            return self.lifecycle_response_from_plan(action, plan, 0, 0);
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Paused => {
                let generation = self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .map(|cell| cell.generation)
                    .unwrap_or(Generation(0));
                if !self.resume_projection_sources_for_cell(id, generation) {
                    self.quarantine_cell_after_hook_failure(id);
                    return self.finish_lifecycle(
                        action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                            0,
                            0,
                        ),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
                if self.transition_cell_state(id, ElmState::Active).is_err() {
                    let _ = super::source::suspend_projection_sources(id, generation);
                    self.quarantine_cell_after_hook_failure(id);
                    return self.finish_lifecycle(
                        action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                            0,
                            0,
                        ),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
                self.finish_lifecycle(
                    action,
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0),
                    0,
                )
            }
            ElmState::Active => {
                let generation = self
                    .cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .map(|cell| cell.generation)
                    .unwrap_or(Generation(0));
                if !self.resume_projection_sources_for_cell(id, generation) {
                    self.quarantine_cell_after_hook_failure(id);
                    return self.finish_lifecycle(
                        action,
                        self.lifecycle_response(
                            id,
                            ELM_MGR_STATUS_INVALID,
                            ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                            0,
                            0,
                        ),
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
                self.finish_lifecycle(
                    action,
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0),
                    0,
                )
            }
            _ => self.finish_lifecycle(
                action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_INVALID_STATE),
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_INVALID_STATE,
            ),
        }
    }
    pub fn detach_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        // 局部 Core 入口不得执行原生钩子；原生单元由正式脱锁生命周期事务处理。
        if self.native_image_index(id).is_some() {
            return self.finish_lifecycle(
                ElmLifecycleAction::Detach,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_INVALID_STATE,
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }
        if !self.cell_has_native_code(id) {
            let mut executor = DeclarativeLifecycleExecutor;
            self.detach_cell_inner(id, Some(&mut executor))
        } else {
            self.detach_cell_inner(id, None)
        }
    }

    #[allow(dead_code)]
    pub(crate) fn detach_cell_with_lifecycle_executor(
        &mut self,
        id: ElmId,
        executor: &mut dyn ElmLifecycleExecutor,
    ) -> ElmLifecycleResponse {
        self.detach_cell_inner(id, Some(executor))
    }

    fn detach_cell_inner(
        &mut self,
        id: ElmId,
        mut executor: Option<&mut dyn ElmLifecycleExecutor>,
    ) -> ElmLifecycleResponse {
        let action = ElmLifecycleAction::Detach;
        let plan = self.preflight_lifecycle_inner(
            ElmLifecyclePlanRequest::new(id.0, action),
            executor.is_some(),
        );
        if plan.allowed == 0 {
            return self.lifecycle_response_from_plan(action, plan, 0, 0);
        }

        let source_generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .unwrap_or(Generation(0));
        let source_suspension =
            match super::source::suspend_projection_sources_guard(id, source_generation) {
                Ok(suspension) => suspension,
                Err(_) => {
                    let response = self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_BUSY,
                        ELM_LIFECYCLE_REASON_LEASE_BUSY,
                        0,
                        0,
                    );
                    return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_PROVIDER_BUSY);
                }
            };

        if self.cell_state(id) == Some(ElmState::Active) {
            let Some(executor) = executor.as_deref_mut() else {
                let _ = source_suspension.keep_suspended();
                let _ = super::owned_resource::stop_accepting(id, source_generation);
                return self.finish_lifecycle(
                    action,
                    self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_TODO,
                        ELM_LIFECYCLE_REASON_NATIVE_TODO,
                        0,
                        0,
                    ),
                    ELM_POLICY_BLOCK_NATIVE_TODO,
                );
            };
            let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Quiesce) else {
                let _ = source_suspension.keep_suspended();
                let _ = super::owned_resource::stop_accepting(id, source_generation);
                return self.finish_lifecycle(
                    action,
                    self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                        0,
                        0,
                    ),
                    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                );
            };
            if executor.on_quiesce(&mut context).is_err() {
                let _ = source_suspension.keep_suspended();
                let _ = super::owned_resource::stop_accepting(id, source_generation);
                self.quarantine_cell_after_hook_failure(id);
                return self.finish_lifecycle(
                    action,
                    self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        ELM_LIFECYCLE_REASON_HOOK_FAILED,
                        0,
                        0,
                    ),
                    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                );
            }
        }

        let owned_before = super::owned_resource::count_owned_by(id, source_generation);
        if super::owned_resource::drain_owner(id, source_generation).is_err() {
            let owned_after = super::owned_resource::count_owned_by(id, source_generation);
            let drained = owned_before.saturating_sub(owned_after);
            let _ = source_suspension.keep_suspended();
            self.quarantine_cell_after_hook_failure(id);
            self.push_resource_trace(
                id.0,
                drained.min(u64::MAX as usize) as u64,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_LEASE_BUSY | ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
            );
            return self.finish_lifecycle(
                action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_BUSY,
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_LEASE_BUSY | ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
            );
        }

        if self.cell_needs_finalize(id) {
            let Some(executor) = executor.as_deref_mut() else {
                let _ = source_suspension.keep_suspended();
                self.quarantine_cell_after_hook_failure(id);
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_TODO,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_NATIVE_TODO),
                    0,
                    0,
                );
                return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_NATIVE_TODO);
            };
            let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Finalize) else {
                let _ = source_suspension.keep_suspended();
                self.quarantine_cell_after_hook_failure(id);
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                    0,
                    0,
                );
                return self.finish_lifecycle(
                    action,
                    response,
                    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                );
            };
            if executor.on_finalize(&mut context).is_err() {
                let _ = source_suspension.keep_suspended();
                self.quarantine_cell_after_hook_failure(id);
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_HOOK_FAILED,
                    0,
                    0,
                );
                return self.finish_lifecycle(
                    action,
                    response,
                    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                );
            }
            if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) {
                cell.lifecycle_finalized = true;
            }
        }

        self.commit_detached_cell(id, source_generation, source_suspension)
    }

    fn commit_detached_cell(
        &mut self,
        id: ElmId,
        generation: Generation,
        source_suspension: super::source::ProjectionSourceSuspension,
    ) -> ElmLifecycleResponse {
        let action = ElmLifecycleAction::Detach;
        if self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .is_none_or(|cell| cell.generation != generation)
        {
            let _ = source_suspension.keep_suspended();
            self.quarantine_cell_after_hook_failure(id);
            return self.finish_lifecycle(
                action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
            );
        }
        if super::owned_resource::count_owned_by(id, generation) != 0 {
            let _ = source_suspension.keep_suspended();
            self.quarantine_cell_after_resource_failure(id);
            return self.finish_lifecycle(
                action,
                self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_BUSY,
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    0,
                    0,
                ),
                ELM_POLICY_BLOCK_LEASE_BUSY,
            );
        }
        if super::resource_accounting::has_live_allocations(id) {
            let _ = source_suspension.keep_suspended();
            self.quarantine_cell_after_resource_failure(id);
            self.push_resource_trace(
                id.0,
                0,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            );
            let response = self.lifecycle_response(
                id,
                ELM_MGR_STATUS_BUSY,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
                0,
                0,
            );
            return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_RESOURCE_QUOTA);
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Active => {
                if self.transition_cell_state(id, ElmState::Quiescing).is_err() {
                    let _ = source_suspension.keep_suspended();
                    self.quarantine_cell_after_hook_failure(id);
                    let response = self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                        0,
                        0,
                    );
                    return self.finish_lifecycle(
                        action,
                        response,
                        ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                    );
                }
            }
            ElmState::Quiescing
            | ElmState::Paused
            | ElmState::Loaded
            | ElmState::Quarantined
            | ElmState::Detached => {}
            _ => {
                let _ = source_suspension.keep_suspended();
                self.quarantine_cell_after_hook_failure(id);
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_INVALID_STATE),
                    0,
                    0,
                );
                return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_INVALID_STATE);
            }
        }

        let revoked_leases = match self.leases.revoke_and_remove_owned_by(id) {
            Ok(ids) => ids,
            Err(ElmError::LeaseBusy) => {
                let _ = source_suspension.keep_suspended();
                self.quarantine_cell_after_hook_failure(id);
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_BUSY,
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    0,
                    0,
                );
                return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_LEASE_BUSY);
            }
            Err(_) => {
                let _ = source_suspension.keep_suspended();
                self.quarantine_cell_after_hook_failure(id);
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                    0,
                    0,
                );
                return self.finish_lifecycle(
                    action,
                    response,
                    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                );
            }
        };
        for lease in &revoked_leases {
            self.emit_lease(TopologyEventKind::LeaseRevoked, *lease);
        }

        let removed_menu_items = self.remove_menu_items_owned_by(id);
        let removed_bindings = self.take_owned_bindings(id);
        for binding in removed_bindings {
            if let Some(edge) = self.graph.capability_binding(binding).cloned() {
                self.note_provider_revoke(&edge);
            }
            self.remove_runtime_binding(binding);
            self.emit_binding(TopologyEventKind::BindingRemoved, binding);
        }
        let _removed_provider_ports = self.remove_dynamic_providers_owned_by(id);
        let _removed_event_subscriptions = self.mgr_runtime.remove_event_subscriptions_owned_by(id);
        self.remove_native_exports_owned_by(id);
        self.remove_native_imports_owned_by(id);
        self.discard_pending_ebi_load(id);

        if self.graph.remove_cell(id).is_err() {
            let _ = source_suspension.keep_suspended();
            self.quarantine_cell_after_hook_failure(id);
            let response = self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                revoked_leases.len() as u32,
                removed_menu_items as u32,
            );
            return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
        }

        let detach_result = match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Loaded => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Paused => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Quiescing => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Quarantined => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Detached => Ok(()),
            _ => Err(ElmError::InvalidTransition),
        };
        if detach_result.is_err() || self.transition_cell_state(id, ElmState::Retired).is_err() {
            let _ = source_suspension.keep_suspended();
            self.quarantine_cell_after_hook_failure(id);
            let response = self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                revoked_leases.len() as u32,
                removed_menu_items as u32,
            );
            return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
        }
        if source_suspension.retire().is_err() {
            self.quarantine_cell_after_hook_failure(id);
            let response = self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                revoked_leases.len() as u32,
                removed_menu_items as u32,
            );
            return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
        }
        if !self.remove_cell_runtime(id) {
            let response = self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
                revoked_leases.len() as u32,
                removed_menu_items as u32,
            );
            return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_RESOURCE_QUOTA);
        }
        self.emit(TopologyEventKind::CellRemoved, Some(id));

        let response = ElmLifecycleResponse::new(
            id.0,
            ELM_MGR_STATUS_OK,
            state_code(ElmState::Retired),
            revoked_leases.len() as u32,
            removed_menu_items as u32,
            ELM_LIFECYCLE_REASON_NONE,
        );
        self.finish_lifecycle(action, response, 0)
    }

    fn todo_registry_records(&self) -> Vec<ElmTodoRegistryRecord> {
        let mut records = Vec::new();
        let static_flags = ELM_TODO_FLAG_STATIC | ELM_TODO_FLAG_ACTIVE;
        records.push(todo_record(
            ELM_TODO_KIND_SOURCE,
            static_flags,
            ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
            0,
            "projection.soyo_profile",
            "soyo 只能通过 Projection provider 产出 EBI，不能成为 ELM Core 内建来源",
        ));
        records.push(todo_record(
            ELM_TODO_KIND_FRAMEWORK,
            static_flags,
            ELM_POLICY_BLOCK_NATIVE_TODO,
            0,
            "framework.rust_elm_distribution",
            "外部 Rust ELM 的调试符号归档、依赖锁定和发布仓库流程仍未完成",
        ));
        for pending in &self.pending_ebi_loads {
            records.push(todo_record(
                ELM_TODO_KIND_NATIVE,
                ELM_TODO_FLAG_ACTIVE,
                ELM_POLICY_BLOCK_NATIVE_TODO,
                pending.cell.0,
                "native.pending_loader",
                pending.unit.manifest.name.as_str(),
            ));
        }
        for cell in &self.cells {
            if cell.ebi_status == ElmEbiLoadStatus::NativeCodeTodo
                && !self
                    .pending_ebi_loads
                    .iter()
                    .any(|pending| pending.cell == cell.id)
            {
                records.push(todo_record(
                    ELM_TODO_KIND_NATIVE,
                    ELM_TODO_FLAG_ACTIVE,
                    ELM_POLICY_BLOCK_NATIVE_TODO,
                    cell.id.0,
                    "native.code_todo",
                    &cell.name,
                ));
            }
        }
        for provider in &self.providers {
            if matches!(provider.backend, ProviderBackend::ElmNativeTodo) {
                records.push(todo_record(
                    ELM_TODO_KIND_PROVIDER,
                    ELM_TODO_FLAG_ACTIVE,
                    ELM_POLICY_BLOCK_PORT_TODO,
                    provider.port.0,
                    "provider.native_backend",
                    "动态 ELM provider 已注册声明，但尚未绑定可执行的原生处理函数",
                ));
            }
            if self.provider_in_flight_count(provider) != 0 {
                records.push(todo_record(
                    ELM_TODO_KIND_RUNTIME,
                    ELM_TODO_FLAG_ACTIVE,
                    ELM_POLICY_BLOCK_PROVIDER_BUSY,
                    provider.port.0,
                    "runtime.provider_in_flight",
                    "该 provider 当前存在运行中调用；已接入取消和超时意图保护域",
                ));
            }
        }
        records
    }

    fn health_records(&self) -> (i32, Vec<ElmCoreHealthRecord>) {
        let mut records = Vec::new();
        self.check_health_graph(&mut records);
        self.check_health_cells(&mut records);
        self.check_health_ports(&mut records);
        self.check_health_providers(&mut records);
        self.check_health_bindings(&mut records);
        self.check_health_runtime_ports(&mut records);
        self.check_health_menu(&mut records);
        self.check_health_events(&mut records);
        self.check_health_audits(&mut records);
        self.check_health_native_capabilities(&mut records);
        self.check_health_todo_registry(&mut records);
        self.check_health_trust(&mut records);
        self.check_health_projection_sources(&mut records);
        self.check_health_journal(&mut records);
        self.check_health_resources(&mut records);
        self.check_health_executions(&mut records);
        self.check_health_sequences(&mut records);

        let status = if records
            .iter()
            .any(|record| record.status != ELM_MGR_STATUS_OK)
        {
            ELM_MGR_STATUS_INVALID
        } else {
            ELM_MGR_STATUS_OK
        };
        (status, records)
    }

    fn check_health_graph(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        match self.graph.validate() {
            Ok(report) => {
                if report.cells != self.cells.len() {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_GRAPH,
                        0,
                        ELM_HEALTH_DETAIL_MISSING_OBJECT,
                    ));
                }
            }
            Err(_) => records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_GRAPH,
                0,
                ELM_HEALTH_DETAIL_GRAPH_INVALID,
            )),
        }
        if self.initialized && self.graph.cell(ELM_MGR_ID).is_none() {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_GRAPH,
                ELM_MGR_ID.0,
                ELM_HEALTH_DETAIL_MISSING_OBJECT,
            ));
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_GRAPH);
    }

    fn check_health_cells(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for (index, cell) in self.cells.iter().enumerate() {
            if self.cells[..index].iter().any(|prev| prev.id == cell.id) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_CELLS,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
            if self.graph.cell(cell.id).is_none() {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_CELLS,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
            }
            if self.graph.parent(cell.id) != cell.parent {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_CELLS,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
            if cell.state == ElmState::Retired {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_CELLS,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            for binding in &cell.owned_bindings {
                match self.graph.capability_binding(*binding) {
                    Some(edge) if edge.consumer == cell.id => {}
                    _ => records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_CELLS,
                        binding.0,
                        ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                    )),
                }
            }
            for menu_id in &cell.owned_menu_items {
                if !self
                    .menu_items
                    .iter()
                    .any(|item| item.id == *menu_id && item.owner == cell.id)
                {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_CELLS,
                        *menu_id,
                        ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                    ));
                }
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_CELLS);
    }

    fn check_health_ports(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for (index, port) in self.ports.iter().enumerate() {
            if self.ports[..index].iter().any(|prev| prev.id == port.id) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PORTS,
                    port.id.0,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
            if self.ports[..index]
                .iter()
                .any(|prev| prev.contract() == port.contract())
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PORTS,
                    port.id.0,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
            if FlowContract::new(port.contract()).is_err() {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PORTS,
                    port.id.0,
                    ELM_HEALTH_DETAIL_CONTRACT_INVALID,
                ));
            }
            if let Some(owner) = port.owner {
                if !self.cell_exists(owner) {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_PORTS,
                        port.id.0,
                        ELM_HEALTH_DETAIL_MISSING_OBJECT,
                    ));
                }
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_PORTS);
    }

    fn check_health_providers(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for (index, provider) in self.providers.iter().enumerate() {
            if self.providers[..index]
                .iter()
                .any(|prev| prev.port == provider.port)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }

            let Some(port) = self.ports.iter().find(|port| port.id == provider.port) else {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
                continue;
            };
            if provider.owner != port.owner || provider.access != port.access {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_KIND_MISMATCH,
                ));
            }
            if let Some(owner) = provider.owner {
                if !self.cell_exists(owner) {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_PROVIDERS,
                        provider.port.0,
                        ELM_HEALTH_DETAIL_MISSING_OBJECT,
                    ));
                }
            }
            let flags = provider.record_flags();
            let backend_flags = flags
                & (ELM_PROVIDER_FLAG_KERNEL_BACKEND
                    | ELM_PROVIDER_FLAG_NATIVE_BACKEND
                    | ELM_PROVIDER_FLAG_TODO_BACKEND);
            if backend_flags.count_ones() != 1
                || ((flags & ELM_PROVIDER_FLAG_DYNAMIC) != 0) != provider.dynamic
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            match provider.backend {
                ProviderBackend::Kernel(_) | ProviderBackend::KernelOps(_) if !port.implemented => {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_PROVIDERS,
                        provider.port.0,
                        ELM_HEALTH_DETAIL_STATE_INVALID,
                    ));
                }
                ProviderBackend::ElmNative(_) if !port.implemented || !port.invokable => {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_PROVIDERS,
                        provider.port.0,
                        ELM_HEALTH_DETAIL_STATE_INVALID,
                    ));
                }
                ProviderBackend::ElmNativeTodo if port.implemented || port.invokable => {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_PROVIDERS,
                        provider.port.0,
                        ELM_HEALTH_DETAIL_STATE_INVALID,
                    ));
                }
                _ => {}
            }
            if provider.dynamic && provider.port.0 < FIRST_DYNAMIC_PORT_ID {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            if provider.in_flight as usize != self.provider_active_execution_count(provider.port)
                || provider.in_flight > provider.max_in_flight
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            if [
                provider.calls,
                provider.failed_calls,
                provider.revokes,
                provider.async_submitted,
                provider.async_completed,
                provider.async_canceled,
                provider.async_expired,
                provider.async_rejected,
            ]
            .contains(&u64::MAX)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_PROVIDERS);
    }

    fn check_health_bindings(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for edge in self.graph.capability_bindings() {
            if !self.cell_exists(edge.consumer) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_BINDINGS,
                    edge.id.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
            }
            match self.port_desc(edge.port) {
                Some(port) => {
                    if port.contract() != edge.contract.as_str() {
                        records.push(ElmCoreHealthRecord::invalid(
                            ELM_HEALTH_CHECK_BINDINGS,
                            edge.id.0,
                            ELM_HEALTH_DETAIL_CONTRACT_INVALID,
                        ));
                    }
                }
                None => records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_BINDINGS,
                    edge.id.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                )),
            }
            if FlowContract::new(edge.contract.as_str()).is_err() {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_BINDINGS,
                    edge.id.0,
                    ELM_HEALTH_DETAIL_CONTRACT_INVALID,
                ));
            }
            let Some(lease) = edge.lease else {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_BINDINGS,
                    edge.id.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
                continue;
            };
            match self.leases.get(lease) {
                Some(resource) => {
                    if resource.owner != edge.consumer
                        || resource.binding != Some(edge.id)
                        || resource.generation != edge.generation
                    {
                        records.push(ElmCoreHealthRecord::invalid(
                            ELM_HEALTH_CHECK_BINDINGS,
                            edge.id.0,
                            ELM_HEALTH_DETAIL_KIND_MISMATCH,
                        ));
                    }
                    if let Some(expected_kind) = self.expected_lease_kind_for_port(edge.port) {
                        if resource.kind != expected_kind {
                            records.push(ElmCoreHealthRecord::invalid(
                                ELM_HEALTH_CHECK_BINDINGS,
                                edge.id.0,
                                ELM_HEALTH_DETAIL_KIND_MISMATCH,
                            ));
                        }
                    }
                }
                None => records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_BINDINGS,
                    edge.id.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                )),
            }
            if !self.cell_owns_binding(edge.consumer, edge.id) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_BINDINGS,
                    edge.id.0,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_BINDINGS);
    }

    fn check_health_runtime_ports(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for (index, runtime) in self.runtime_ports.iter().enumerate() {
            if self.runtime_ports[..index]
                .iter()
                .any(|prev| prev.binding == runtime.binding)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RUNTIME_PORTS,
                    runtime.binding.0,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
            if self.validate_runtime_port(index, runtime.port).is_err() {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RUNTIME_PORTS,
                    runtime.binding.0,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
            if !matches!(runtime.port, ELM_CORE_LOG_PORT_ID | ELM_CORE_EVENT_PORT_ID) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RUNTIME_PORTS,
                    runtime.binding.0,
                    ELM_HEALTH_DETAIL_KIND_MISMATCH,
                ));
            }
            match self.leases.get(runtime.lease) {
                Some(lease)
                    if lease.kind == LeaseKind::RuntimePort
                        && lease.owner == runtime.cell
                        && lease.binding == Some(runtime.binding) => {}
                Some(_) => records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RUNTIME_PORTS,
                    runtime.binding.0,
                    ELM_HEALTH_DETAIL_KIND_MISMATCH,
                )),
                None => records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RUNTIME_PORTS,
                    runtime.binding.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                )),
            }
            if runtime.cursor > self.last_event_sequence() {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RUNTIME_PORTS,
                    runtime.binding.0,
                    ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_RUNTIME_PORTS);
    }

    fn check_health_menu(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for (index, item) in self.menu_items.iter().enumerate() {
            if self.menu_items[..index]
                .iter()
                .any(|prev| prev.id == item.id)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_MENU,
                    item.id,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
            if !self.cell_exists(item.owner) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_MENU,
                    item.id,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
            }
            if !self.cell_owns_menu_item(item.owner, item.id) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_MENU,
                    item.id,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_MENU);
    }

    fn check_health_events(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        let mut previous = 0;
        for event in &self.events {
            if event.sequence <= previous {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EVENTS,
                    event.sequence,
                    ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
                ));
            }
            previous = event.sequence;
        }
        let newest = self.events.last().map(|event| event.sequence).unwrap_or(0);
        if newest != self.last_event_sequence() {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_EVENTS,
                newest,
                ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
            ));
        }
        if self.acknowledged_event_sequence > self.last_event_sequence() {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_EVENTS,
                self.acknowledged_event_sequence,
                ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
            ));
        }
        for subscription in &self.mgr_runtime.event_subscriptions {
            if !self.cell_exists(subscription.owner) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EVENTS,
                    subscription.subscription,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
            match self.leases.get(subscription.lease) {
                Some(lease)
                    if lease.owner == subscription.owner
                        && lease.kind == LeaseKind::EventSubscription => {}
                Some(_) => records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EVENTS,
                    subscription.subscription,
                    ELM_HEALTH_DETAIL_KIND_MISMATCH,
                )),
                None => records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EVENTS,
                    subscription.subscription,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                )),
            }
            if subscription.cursor > self.last_event_sequence() {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EVENTS,
                    subscription.subscription,
                    ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_EVENTS);
    }

    fn check_health_audits(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        let mut previous = 0;
        for audit in &self.audits {
            if audit.sequence <= previous || audit.sequence >= self.next_audit_sequence {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_AUDITS,
                    audit.sequence,
                    ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
                ));
            }
            previous = audit.sequence;
        }
        if self.next_audit_sequence == 0 {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_AUDITS,
                0,
                ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
            ));
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_AUDITS);
    }

    fn check_health_native_capabilities(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for (index, export) in self.native_exports.iter().enumerate() {
            if self
                .cells
                .iter()
                .find(|cell| cell.id == export.owner)
                .is_none_or(|cell| cell.generation != export.generation)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_NATIVE_CAPABILITIES,
                    export.owner.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
            }
            if self.native_exports[..index].iter().any(|previous| {
                previous.name == export.name
                    && previous.contract == export.contract
                    && previous.version == export.version
            }) {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_NATIVE_CAPABILITIES,
                    export.owner.0,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
        }
        for import in &self.native_imports {
            let owner_current = self
                .cells
                .iter()
                .find(|cell| cell.id == import.owner)
                .is_some_and(|cell| cell.generation == import.owner_generation);
            let provider_current = self
                .cells
                .iter()
                .find(|cell| cell.id == import.provider)
                .is_some_and(|cell| cell.generation == import.provider_generation);
            if !owner_current || !provider_current {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_NATIVE_CAPABILITIES,
                    import.owner.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
            }
            let resolved = self.native_exports.iter().any(|export| {
                export.owner == import.provider
                    && export.name == import.name
                    && export.contract == import.contract
                    && export.version == import.selected_version
                    && export.address == import.address
                    && export.generation == import.provider_generation
            });
            if !resolved {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_NATIVE_CAPABILITIES,
                    import.owner.0,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_NATIVE_CAPABILITIES);
    }

    fn check_health_todo_registry(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        let header_size = core::mem::size_of::<ElmTodoRegistryHeader>();
        let record_size = core::mem::size_of::<ElmTodoRegistryRecord>();
        if record_size == 0 || header_size >= ELM_MGR_MAX_PAYLOAD {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_TODO_REGISTRY,
                0,
                ELM_HEALTH_DETAIL_STATE_INVALID,
            ));
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_TODO_REGISTRY);
    }

    fn check_health_trust(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        if self.initialized && !self.trust_store.sealed() {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_TRUST,
                0,
                ELM_HEALTH_DETAIL_STATE_INVALID,
            ));
        }
        for acceptance in self.trust_store.accepted_epochs() {
            if acceptance.release_epoch() == 0
                || !self.trust_store.anchors().iter().any(|anchor| {
                    anchor.rollback_authority_id() == acceptance.rollback_authority_id()
                })
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_TRUST,
                    0,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
        }
        for cell in &self.cells {
            if matches!(
                cell.ebi_source,
                ElmEbiSourceKind::Builtin | ElmEbiSourceKind::Memory
            ) {
                if cell.trust_unsigned || cell.signer_key_id != [0; 32] || cell.release_epoch != 0 {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_TRUST,
                        cell.id.0,
                        ELM_HEALTH_DETAIL_KIND_MISMATCH,
                    ));
                }
                continue;
            }
            if self
                .pending_ebi_loads
                .iter()
                .any(|pending| pending.cell == cell.id)
            {
                continue;
            }
            if cell.trust_unsigned {
                if !self.allow_unsigned_external
                    || cell.signer_key_id != [0; 32]
                    || cell.release_epoch != 0
                {
                    records.push(ElmCoreHealthRecord::invalid(
                        ELM_HEALTH_CHECK_TRUST,
                        cell.id.0,
                        ELM_HEALTH_DETAIL_STATE_INVALID,
                    ));
                }
                continue;
            }
            let module_digest = sha256(cell.name.as_bytes());
            let rollback_authority_id = self
                .trust_store
                .anchors()
                .iter()
                .find(|anchor| anchor.key_id == cell.signer_key_id)
                .map(ElmTrustAnchor::rollback_authority_id);
            let accepted = rollback_authority_id.is_some_and(|rollback_authority_id| {
                self.trust_store.accepted_epochs().iter().any(|acceptance| {
                    acceptance.rollback_authority_id() == rollback_authority_id
                        && acceptance.module_digest() == module_digest
                        && acceptance.release_epoch() >= cell.release_epoch
                })
            });
            if cell.signer_key_id == [0; 32]
                || cell.release_epoch == 0
                || rollback_authority_id.is_none()
                || !accepted
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_TRUST,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_TRUST);
    }

    fn check_health_projection_sources(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        let snapshots = match super::source::projection_source_snapshots() {
            Ok(snapshots) => snapshots,
            Err(_) => {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROJECTION_SOURCES,
                    0,
                    ELM_HEALTH_DETAIL_RESOURCE_LEAK,
                ));
                push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_PROJECTION_SOURCES);
                return;
            }
        };
        for (index, source) in snapshots.iter().enumerate() {
            if source.id == 0 || source.owner.0 == 0 || source.generation.0 == 0 {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROJECTION_SOURCES,
                    source.id,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            if source.active && (source.suspended || source.retiring)
                || source.suspended && source.retiring
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROJECTION_SOURCES,
                    source.id,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            let Some(cell) = self.cells.iter().find(|cell| cell.id == source.owner) else {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROJECTION_SOURCES,
                    source.id,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
                continue;
            };
            if source.active && source.generation != cell.generation {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROJECTION_SOURCES,
                    source.id,
                    ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
                ));
            }
            if source.active
                && snapshots[..index]
                    .iter()
                    .any(|previous| previous.id == source.id && previous.active)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROJECTION_SOURCES,
                    source.id,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
        }
        if self.initialized
            && !snapshots.iter().any(|source| {
                source.id == elm_model::ELM_EKI_PROJECTION_SOURCE_ID
                    && source.owner == ELM_EKI_ID
                    && source.generation == Generation::FIRST
                    && source.active
            })
        {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_PROJECTION_SOURCES,
                elm_model::ELM_EKI_PROJECTION_SOURCE_ID,
                ELM_HEALTH_DETAIL_MISSING_OBJECT,
            ));
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_PROJECTION_SOURCES);
    }

    fn check_health_journal(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        let info = super::journal::runtime_info();
        if !info.initialized {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_JOURNAL,
                0,
                ELM_HEALTH_DETAIL_STATE_INVALID,
            ));
        }
        if info.failed {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_JOURNAL,
                info.last_sequence,
                if info.sequence_exhausted {
                    ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED
                } else {
                    ELM_HEALTH_DETAIL_PERSISTENCE_FAILED
                },
            ));
        }
        if info.configured && !info.persistent && !info.failed
            || info.backend_bytes_used % super::journal::ELM_JOURNAL_RECORD_SIZE as u64 != 0
        {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_JOURNAL,
                info.backend_bytes_used,
                ELM_HEALTH_DETAIL_STATE_INVALID,
            ));
        }
        let journal_records = super::journal::records();
        let mut previous: Option<super::journal::JournalRecord> = None;
        for record in &journal_records {
            if let Some(previous_record) = previous
                && (record.sequence != previous_record.sequence.saturating_add(1)
                    || record.previous_hash != previous_record.record_hash)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_JOURNAL,
                    record.sequence,
                    ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
                ));
            }
            previous = Some(*record);
        }
        if journal_records
            .last()
            .is_some_and(|record| record.sequence != info.last_sequence)
            || journal_records.is_empty() && info.last_sequence != 0
        {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_JOURNAL,
                info.last_sequence,
                ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
            ));
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_JOURNAL);
    }

    fn check_health_resources(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        let now_ns = sched::now_ns_public();
        for cell in &self.cells {
            if !super::resource_accounting::registered(cell.id)
                || super::resource_accounting::registered_budget(cell.id)
                    != Some(cell.resource_budget)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RESOURCES,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
                continue;
            }
            let Some(owner) = super::owned_resource::owner_snapshot(cell.id) else {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RESOURCES,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_MISSING_OBJECT,
                ));
                continue;
            };
            if owner.owner != cell.id
                || owner.generation != cell.generation
                || (!owner.accepting
                    && matches!(
                        cell.state,
                        ElmState::Active | ElmState::Loaded | ElmState::Paused
                    ))
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RESOURCES,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            let accounting = super::resource_accounting::snapshot(cell.id, now_ns);
            if accounting.accounting_errors != 0 {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RESOURCES,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_RESOURCE_LEAK,
                ));
            }
            if accounting.dynamic_alloc_bytes > cell.resource_budget.max_dynamic_alloc_bytes
                || accounting.native_stack_bytes > cell.resource_budget.max_native_stack_bytes
                || accounting.cpu_time_ns_period > cell.resource_budget.cpu_budget_ns_per_period
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_RESOURCES,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
        }
        if let Some(orphan) = super::resource_accounting::first_orphaned_cell(|id| {
            self.cells.iter().any(|cell| cell.id == id)
        }) {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_RESOURCES,
                orphan.0,
                ELM_HEALTH_DETAIL_RESOURCE_LEAK,
            ));
        }
        if let Some((owner, _)) = super::owned_resource::first_orphaned_owner(|id, generation| {
            self.cells
                .iter()
                .any(|cell| cell.id == id && cell.generation == generation)
        }) {
            records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_RESOURCES,
                owner.0,
                ELM_HEALTH_DETAIL_RESOURCE_LEAK,
            ));
        }
        match super::owned_resource::snapshots() {
            Ok(snapshots) => {
                for snapshot in snapshots {
                    if snapshot.state == elm_model::ElmOwnedResourceState::Failed as u32 {
                        records.push(ElmCoreHealthRecord::invalid(
                            ELM_HEALTH_CHECK_RESOURCES,
                            snapshot.resource_id,
                            ELM_HEALTH_DETAIL_RESOURCE_LEAK,
                        ));
                    }
                }
            }
            Err(_) => records.push(ElmCoreHealthRecord::invalid(
                ELM_HEALTH_CHECK_RESOURCES,
                0,
                ELM_HEALTH_DETAIL_STATE_INVALID,
            )),
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_RESOURCES);
    }

    fn check_health_executions(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        let now_ns = sched::now_ns_public();
        for cell in &self.cells {
            if cell.exclusive_execution && cell.active_executions != 1
                || !cell.exclusive_execution && cell.active_executions == u32::MAX
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EXECUTIONS,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_STUCK_REFERENCE,
                ));
            }
        }
        for (index, active) in self.active_provider_executions.iter().enumerate() {
            if self.active_provider_executions[..index]
                .iter()
                .any(|previous| previous.id == active.id)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EXECUTIONS,
                    active.id,
                    ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
                ));
            }
            let provider_valid = self.providers.iter().any(|provider| {
                provider.port == active.port && provider.backend_epoch == active.provider_epoch
            });
            if !provider_valid {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EXECUTIONS,
                    active.id,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
            if active.deadline_ns != 0 && now_ns > active.deadline_ns {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EXECUTIONS,
                    active.id,
                    ELM_HEALTH_DETAIL_STUCK_REFERENCE,
                ));
            }
            if let Some(lease) = active.lease
                && self
                    .leases
                    .get(lease)
                    .is_none_or(|lease| lease.active_refs == 0)
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EXECUTIONS,
                    active.id,
                    ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
                ));
            }
        }
        for lease in self.leases.iter() {
            let expected = self
                .provider_jobs
                .iter()
                .filter(|job| job.lease == lease.id)
                .count()
                + self
                    .provider_results
                    .iter()
                    .filter(|result| result.lease == lease.id)
                    .count()
                + self
                    .active_provider_executions
                    .iter()
                    .filter(|active| active.lease == Some(lease.id))
                    .count();
            if lease.active_refs as usize != expected {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_EXECUTIONS,
                    lease.id.0,
                    ELM_HEALTH_DETAIL_STUCK_REFERENCE,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_EXECUTIONS);
    }

    fn check_health_sequences(&self, records: &mut Vec<ElmCoreHealthRecord>) {
        let start = records.len();
        for (subject, exhausted) in [
            (1, self.next_cell_id == 0),
            (2, self.next_port_id == 0),
            (3, self.next_binding_id == 0),
            (4, self.next_lease_id == 0),
            (5, self.next_action_id == 0),
            (6, self.next_menu_item_id == 0),
            (7, self.next_provider_ticket_id == 0),
            (8, self.next_provider_execution_id == 0),
            (9, self.mgr_runtime.next_event_subscription_id == 0),
            (10, self.mgr_runtime.next_kernel_provider_api_id == 0),
            (11, self.next_event_sequence.0 == 0),
            (12, self.next_audit_sequence == 0),
            (13, self.next_trace_sequence == 0),
            (14, self.menu_generation.checked_next().is_none()),
            (15, self.mgr_runtime.api_generation.checked_next().is_none()),
        ] {
            if exhausted {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_SEQUENCES,
                    subject,
                    ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED,
                ));
            }
        }
        for cell in &self.cells {
            if cell.policy_epoch == u64::MAX || cell.generation.checked_next().is_none() {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_SEQUENCES,
                    cell.id.0,
                    ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED,
                ));
            }
        }
        for provider in &self.providers {
            if provider.backend_epoch == u64::MAX {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_SEQUENCES,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED,
                ));
            }
        }
        push_health_ok_if_clean(records, start, ELM_HEALTH_CHECK_SEQUENCES);
    }

    pub fn debug_dump_bytes(&self) -> Vec<u8> {
        let (health_status, health_records) = self.health_records();
        let projection_sources = super::source::projection_source_snapshots().unwrap_or_default();
        let journal_info = super::journal::runtime_info();
        let health_failures = health_records
            .iter()
            .filter(|record| record.status != ELM_MGR_STATUS_OK)
            .count();
        let mut out = format!(
            "ELM Core 诊断\ncells={}\nports={}\nproviders={}\nbindings={}\nleases={}\nruntime_ports={}\nmenu_items={}\nnative_exports={}\nnative_imports={}\nprojection_sources={}\nactive_provider_executions={}\ntrust_anchors={}\ntrust_revoked={}\ntrust_epochs={}\ntrust_flags=0x{:x}\njournal_configured={}\njournal_persistent={}\njournal_required={}\njournal_failed={}\njournal_last_error={}\njournal_last_sequence={}\njournal_dropped={}\nlast_event_sequence={}\ndropped_events={}\ndropped_audits={}\ndropped_faults={}\nhealth_status={}\nhealth_records={}\nhealth_failures={}\n",
            self.cells.len(),
            self.ports.len(),
            self.providers.len(),
            self.graph.capability_bindings().len(),
            self.lease_count(),
            self.runtime_ports.len(),
            self.menu_items.len(),
            self.native_exports.len(),
            self.native_imports.len(),
            projection_sources.len(),
            self.active_provider_executions.len(),
            self.trust_store.anchors().len(),
            self.trust_store.revoked().len(),
            self.trust_store.accepted_epochs().len(),
            self.trust_runtime_info().flags,
            u32::from(journal_info.configured),
            u32::from(journal_info.persistent),
            u32::from(journal_info.required),
            u32::from(journal_info.failed),
            journal_info.last_error,
            journal_info.last_sequence,
            journal_info.dropped_records,
            self.last_event_sequence(),
            self.dropped_event_count,
            self.dropped_audit_count,
            general::elm_guard::dropped_fault_snapshot_count(),
            health_status,
            health_records.len(),
            health_failures,
        );
        out.push_str("[cells]\n");
        for cell in &self.cells {
            out.push_str(
                format!(
                    "cell id={} parent={} name={} state={:?} kind={:?} generation={} elmapi={} policy_epoch={} active_executions={} exclusive_execution={} source={:?} ebi_arch={:?} ebi_status={:?} trust_unsigned={} release_epoch={} signer_key_id={:02x?} native_code={} native_segments={} native_imports={} native_exports={} lifecycle_hooks={} lifecycle_executor_ready={} lifecycle_initialized={} lifecycle_finalized={} isolated={} native_faults={} isolation_blocker=0x{:x} pending_loads={} owned_bindings={} owned_menu_items={}\n",
                    cell.id.0,
                    cell.parent.map(|id| id.0).unwrap_or(0),
                    cell.name,
                    cell.state,
                    cell.kind,
                    cell.generation.0,
                    cell.elmapi_version,
                    cell.policy_epoch,
                    cell.active_executions,
                    u32::from(cell.exclusive_execution),
                    cell.ebi_source,
                    cell.ebi_arch,
                    cell.ebi_status,
                    u32::from(cell.trust_unsigned),
                    cell.release_epoch,
                    cell.signer_key_id,
                    cell.has_native_code,
                    cell.native_segment_count,
                    cell.native_import_count,
                    cell.native_export_count,
                    cell.lifecycle_hooks_declared,
                    cell.lifecycle_executor_ready,
                    cell.lifecycle_initialized,
                    cell.lifecycle_finalized,
                    u32::from(cell.isolated),
                    cell.native_faults,
                    cell.isolation_blocker,
                    self.pending_ebi_loads.iter().filter(|pending| pending.cell == cell.id).count(),
                    cell.owned_bindings.len(),
                    cell.owned_menu_items.len(),
                )
                .as_str(),
            );
        }
        out.push_str("[ports]\n");
        for port in &self.ports {
            out.push_str(
                format!(
                    "port id={} owner={} contract={} direction={:?} mode={:?} access={:?} invokable={} implemented={}\n",
                    port.id.0,
                    port.owner.map(|owner| owner.0).unwrap_or(0),
                    port.contract(),
                    port.direction,
                    port.mode,
                    port.access,
                    port.invokable,
                    port.implemented,
                )
                .as_str(),
            );
        }
        out.push_str("[providers]\n");
        for provider in &self.providers {
            out.push_str(
                format!(
                    "provider port={} owner={} access={:?} backend={:?} dynamic={} bindings={} calls={} failed_calls={} revokes={}\n",
                    provider.port.0,
                    provider.owner.map(|owner| owner.0).unwrap_or(0),
                    provider.access,
                    provider.backend,
                    provider.dynamic,
                    self.provider_binding_count(provider.port),
                    provider.calls,
                    provider.failed_calls,
                    provider.revokes,
                )
                .as_str(),
            );
        }
        out.push_str("[bindings]\n");
        for edge in self.graph.capability_bindings() {
            out.push_str(
                format!(
                    "binding id={} cell={} port={} lease={} generation={} active={} contract={}\n",
                    edge.id.0,
                    edge.consumer.0,
                    edge.port.0,
                    edge.lease.map(|lease| lease.0).unwrap_or(0),
                    edge.generation.0,
                    edge.active,
                    edge.contract.as_str(),
                )
                .as_str(),
            );
        }
        out.push_str("[native_capabilities]\n");
        for export in &self.native_exports {
            out.push_str(
                format!(
                    "native_export owner={} name={} contract={} version={} address=0x{:x}\n",
                    export.owner.0,
                    export.name,
                    export.contract.as_str(),
                    export.version,
                    export.address,
                )
                .as_str(),
            );
        }
        for import in &self.native_imports {
            out.push_str(
                format!(
                    "native_import owner={} provider={} name={} contract={} version_range={}..={} selected_version={} address=0x{:x}\n",
                    import.owner.0,
                    import.provider.0,
                    import.name,
                    import.contract.as_str(),
                    import.min_version,
                    import.max_version,
                    import.selected_version,
                    import.address,
                )
                .as_str(),
            );
        }
        out.push_str("[projection_sources]\n");
        for source in &projection_sources {
            out.push_str(
                format!(
                    "source id={} owner={} generation={} refs={} active={} suspended={} retiring={}\n",
                    source.id,
                    source.owner.0,
                    source.generation.0,
                    source.active_refs,
                    u32::from(source.active),
                    u32::from(source.suspended),
                    u32::from(source.retiring),
                )
                .as_str(),
            );
        }
        out.push_str("[active_provider_executions]\n");
        for active in &self.active_provider_executions {
            out.push_str(
                format!(
                    "execution id={} port={} binding={} lease={} backend_epoch={} started_at_ns={} deadline_ns={} age_ns={}\n",
                    active.id,
                    active.port.0,
                    active.binding.map(|binding| binding.0).unwrap_or(0),
                    active.lease.map(|lease| lease.0).unwrap_or(0),
                    active.provider_epoch,
                    active.started_at_ns,
                    active.deadline_ns,
                    sched::now_ns_public().saturating_sub(active.started_at_ns),
                )
                .as_str(),
            );
        }
        out.push_str("[faults]\n");
        out.push_str(self.sysfs_faults_text().as_str());
        out.push_str("[leases]\n");
        for lease in self.leases.iter() {
            out.push_str(
                format!(
                    "lease id={} owner={} kind={:?} rights={:?} state={:?} active_refs={} binding={}\n",
                    lease.id.0,
                    lease.owner.0,
                    lease.kind,
                    lease.rights,
                    lease.state,
                    lease.active_refs,
                    lease.binding.map(|binding| binding.0).unwrap_or(0),
                )
                .as_str(),
            );
        }
        out.push_str("[runtime_ports]\n");
        for runtime in &self.runtime_ports {
            out.push_str(
                format!(
                    "runtime binding={} cell={} port={} lease={} cursor={} submitted_logs={} delivered_events={} dropped_events={}\n",
                    runtime.binding.0,
                    runtime.cell.0,
                    runtime.port.0,
                    runtime.lease.0,
                    runtime.cursor,
                    runtime.submitted_logs,
                    runtime.delivered_events,
                    runtime.dropped_events,
                )
                .as_str(),
            );
        }
        out.push_str("[remaining_scope]\n子系统 provider 接入、完整用户态管理工具以及 ELM 调试与发布生态属于后续独立主线。\n");
        out.into_bytes()
    }

    pub fn sysfs_text(&self, name: &str) -> String {
        match name {
            "core" => self.sysfs_core_text(),
            "policy" => self.sysfs_policy_text(),
            "health" => self.sysfs_health_text(),
            "menu" => self.sysfs_menu_text(),
            "topology" => self.sysfs_topology_text(),
            "ports" => self.sysfs_ports_text(),
            "providers" => self.sysfs_providers_text(),
            "bindings" => self.sysfs_bindings_text(),
            "events" => self.sysfs_events_text(),
            "audit" => self.sysfs_audit_text(),
            "api" => self.sysfs_api_text(),
            "faults" => self.sysfs_faults_text(),
            "trust" => self.sysfs_trust_text(),
            "projection-sources" | "sources" => self.sysfs_projection_sources_text(),
            "journal" => self.sysfs_journal_text(),
            "executions" => self.sysfs_executions_text(),
            "owned-resources" => self.sysfs_owned_resources_text(),
            "diagnostics" => String::from_utf8_lossy(&self.debug_dump_bytes()).into_owned(),
            "native-capabilities" | "native" => self.sysfs_native_capabilities_text(),
            "todo" | "todo-registry" => self.sysfs_todo_text(),
            _ => "status=not-found\n".to_string(),
        }
    }

    fn sysfs_core_text(&self) -> String {
        let (health_status, health_records) = self.health_records();
        format!(
            "name=elm-mgr\ninitialized={}\ncells={}\nports={}\nproviders={}\nbindings={}\nleases={}\nruntime_ports={}\nsubscriptions={}\nmenu_items={}\napi_records={}\nnative_exports={}\nnative_imports={}\nlast_event_sequence={}\nhealth_status={}\nhealth_records={}\n",
            u32::from(self.initialized),
            self.cells.len(),
            self.ports.len(),
            self.providers.len(),
            self.graph.capability_bindings().len(),
            self.lease_count(),
            self.runtime_ports.len(),
            self.mgr_runtime.event_subscriptions.len(),
            self.menu_items.len(),
            self.mgr_runtime.api_registry.len(),
            self.native_exports.len(),
            self.native_imports.len(),
            self.last_event_sequence(),
            health_status,
            health_records.len(),
        )
    }

    fn sysfs_policy_text(&self) -> String {
        let policy = self.policy_info();
        format!(
            "abi_version={}\nsupported_actions=0x{:x}\npolicy_flags=0x{:x}\nblocker_mask=0x{:x}\naudit_capacity={}\n",
            policy.abi_version,
            policy.supported_actions,
            policy.policy_flags,
            policy.blocker_mask,
            policy.audit_capacity,
        )
    }

    fn sysfs_trust_text(&self) -> String {
        let info = self.trust_runtime_info();
        let mut out = format!(
            "abi_version={}\nflags=0x{:x}\nanchors={}\nrevoked={}\naccepted_epochs={}\n",
            info.abi_version,
            info.flags,
            info.anchor_count,
            info.revoked_count,
            info.accepted_epoch_count,
        );
        for anchor in self.trust_store.anchors() {
            out.push_str(
                format!(
                    "anchor={} rollback_authority={} key_id={:02x?}\n",
                    anchor.identifier, anchor.rollback_authority_identifier, anchor.key_id,
                )
                .as_str(),
            );
        }
        for acceptance in self.trust_store.accepted_epochs() {
            out.push_str(
                format!(
                    "accepted rollback_authority_id={:02x?} module_digest={:02x?} signer_key_id={:02x?} release_epoch={}\n",
                    acceptance.rollback_authority_id(),
                    acceptance.module_digest(),
                    acceptance.signer_key_id(),
                    acceptance.release_epoch(),
                )
                .as_str(),
            );
        }
        for cell in &self.cells {
            out.push_str(
                format!(
                    "cell={} source={:?} unsigned={} release_epoch={} signer_key_id={:02x?}\n",
                    cell.id.0,
                    cell.ebi_source,
                    u32::from(cell.trust_unsigned),
                    cell.release_epoch,
                    cell.signer_key_id,
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_projection_sources_text(&self) -> String {
        let snapshots = match super::source::projection_source_snapshots() {
            Ok(snapshots) => snapshots,
            Err(err) => return format!("status=unavailable\nerror={:?}\n", err),
        };
        let mut out = format!("status=ok\nsources={}\n", snapshots.len());
        for source in snapshots {
            out.push_str(
                format!(
                    "source={} owner={} generation={} refs={} active={} suspended={} retiring={}\n",
                    source.id,
                    source.owner.0,
                    source.generation.0,
                    source.active_refs,
                    u32::from(source.active),
                    u32::from(source.suspended),
                    u32::from(source.retiring),
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_journal_text(&self) -> String {
        let info = super::journal::runtime_info();
        format!(
            "initialized={}\nconfigured={}\npersistent={}\nrequired={}\nfailed={}\nsequence_exhausted={}\nlast_error={}\nreplayed_records={}\ntrust_epochs={}\nretained_records={}\ndropped_records={}\nlast_sequence={}\nbackend_bytes_used={}\nlast_hash={:02x?}\n",
            u32::from(info.initialized),
            u32::from(info.configured),
            u32::from(info.persistent),
            u32::from(info.required),
            u32::from(info.failed),
            u32::from(info.sequence_exhausted),
            info.last_error,
            info.replayed_records,
            info.trust_epoch_count,
            info.retained_records,
            info.dropped_records,
            info.last_sequence,
            info.backend_bytes_used,
            info.last_hash,
        )
    }

    fn sysfs_executions_text(&self) -> String {
        let now_ns = sched::now_ns_public();
        let mut out = format!(
            "active_provider_executions={}\nqueued_provider_jobs={}\nrunning_provider_jobs={}\nretained_provider_results={}\n",
            self.active_provider_executions.len(),
            self.provider_jobs.len(),
            self.provider_running.len(),
            self.provider_results.len(),
        );
        for active in &self.active_provider_executions {
            out.push_str(
                format!(
                    "execution={} port={} binding={} lease={} backend_epoch={} started_at_ns={} deadline_ns={} age_ns={} expired={}\n",
                    active.id,
                    active.port.0,
                    active.binding.map(|binding| binding.0).unwrap_or(0),
                    active.lease.map(|lease| lease.0).unwrap_or(0),
                    active.provider_epoch,
                    active.started_at_ns,
                    active.deadline_ns,
                    now_ns.saturating_sub(active.started_at_ns),
                    u32::from(active.deadline_ns != 0 && now_ns > active.deadline_ns),
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_owned_resources_text(&self) -> String {
        let snapshots = match super::owned_resource::snapshots() {
            Ok(snapshots) => snapshots,
            Err(err) => return format!("status=unavailable\nerror={:?}\n", err),
        };
        let mut out = format!("status=ok\nresources={}\n", snapshots.len());
        for cell in &self.cells {
            match super::owned_resource::owner_snapshot(cell.id) {
                Some(owner) => out.push_str(
                    format!(
                        "owner={} generation={} accepting={} resources={}\n",
                        owner.owner.0,
                        owner.generation.0,
                        u32::from(owner.accepting),
                        owner.resource_count,
                    )
                    .as_str(),
                ),
                None => out.push_str(
                    format!(
                        "owner={} generation={} status=missing\n",
                        cell.id.0, cell.generation.0,
                    )
                    .as_str(),
                ),
            }
        }
        for resource in snapshots {
            out.push_str(
                format!(
                    "resource={} owner={} generation={} kind={} handle={} state={} last_status={}\n",
                    resource.resource_id,
                    resource.owner_cell_id,
                    resource.owner_generation,
                    resource.kind,
                    resource.handle,
                    resource.state,
                    resource.last_status,
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_health_text(&self) -> String {
        let (status, records) = self.health_records();
        let mut out = format!("status={}\nrecords={}\n", status, records.len());
        for record in records {
            out.push_str(
                format!(
                    "check={} status={} subject={} detail={}\n",
                    record.check_kind, record.status, record.subject_id, record.detail,
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_menu_text(&self) -> String {
        let mut out = format!(
            "generation={}\nitems={}\n",
            self.menu_generation.0,
            self.menu_items.len()
        );
        for item in &self.menu_items {
            out.push_str(
                format!(
                    "item={} owner={} action={} kind={:?} flags=0x{:x} label={} route={}\n",
                    item.id,
                    item.owner.0,
                    item.action.0,
                    item.kind,
                    item.flags,
                    item.label,
                    item.route,
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_topology_text(&self) -> String {
        let mut out = format!(
            "cells={}\nparents={}\ndependencies={}\nextension_points={}\nextensions={}\nevent_sequence={}\n",
            self.cells.len(),
            self.graph.parent_edges().len(),
            self.graph.dependencies().len(),
            self.graph.extension_points().len(),
            self.graph.extensions().len(),
            self.last_event_sequence(),
        );
        for edge in self.graph.parent_edges() {
            out.push_str(
                format!("parent child={} parent={}\n", edge.child.0, edge.parent.0).as_str(),
            );
        }
        for edge in self.graph.dependencies() {
            out.push_str(
                format!(
                    "dependency consumer={} provider={} contract={}\n",
                    edge.consumer.0,
                    edge.provider.0,
                    edge.contract.as_str(),
                )
                .as_str(),
            );
        }
        for point in self.graph.extension_points() {
            out.push_str(
                format!(
                    "extension_point owner={} point={} contract={}\n",
                    point.owner.0,
                    point.name,
                    point.contract.as_str(),
                )
                .as_str(),
            );
        }
        for edge in self.graph.extensions() {
            out.push_str(
                format!(
                    "extension extension={} target={} point={} contract={}\n",
                    edge.extension.0,
                    edge.target.0,
                    edge.point,
                    edge.contract.as_str(),
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_ports_text(&self) -> String {
        let mut out = format!("ports={}\n", self.ports.len());
        for port in &self.ports {
            out.push_str(
                format!(
                    "port={} owner={} contract={} direction={:?} mode={:?} access={:?} implemented={} invokable={}\n",
                    port.id.0,
                    port.owner.map(|owner| owner.0).unwrap_or(0),
                    port.contract(),
                    port.direction,
                    port.mode,
                    port.access,
                    u32::from(port.implemented),
                    u32::from(port.invokable),
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_providers_text(&self) -> String {
        let mut out = format!("providers={}\n", self.providers.len());
        for provider in &self.providers {
            out.push_str(
                format!(
                    "provider_port={} owner={} backend={:?} dynamic={} bindings={} calls={} failed_calls={} revokes={} queued={} running={} retained={} oldest_running_start_ns={}\n",
                    provider.port.0,
                    provider.owner.map(|owner| owner.0).unwrap_or(0),
                    provider.backend,
                    u32::from(provider.dynamic),
                    self.provider_binding_count(provider.port),
                    provider.calls,
                    provider.failed_calls,
                    provider.revokes,
                    self.provider_queued_count(provider.port),
                    self.provider_running_count(provider.port),
                    self.provider_retained_result_count(provider.port),
                    self.provider_oldest_running_start_ns(provider.port),
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_bindings_text(&self) -> String {
        let bindings = self.graph.capability_bindings();
        let mut out = format!("bindings={}\n", bindings.len());
        for edge in bindings {
            out.push_str(
                format!(
                    "binding={} cell={} port={} lease={} active={} generation={} contract={}\n",
                    edge.id.0,
                    edge.consumer.0,
                    edge.port.0,
                    edge.lease.map(|lease| lease.0).unwrap_or(0),
                    u32::from(edge.active),
                    edge.generation.0,
                    edge.contract.as_str(),
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_events_text(&self) -> String {
        let mut out = format!(
            "last_event_sequence={}\nacknowledged_event_sequence={}\nevents={}\nsubscriptions={}\n",
            self.last_event_sequence(),
            self.acknowledged_event_sequence,
            self.events.len(),
            self.mgr_runtime.event_subscriptions.len(),
        );
        for event in &self.events {
            out.push_str(
                format!(
                    "event={} kind={} cell={} port={} binding={} lease={}\n",
                    event.sequence, event.kind, event.cell, event.port, event.binding, event.lease,
                )
                .as_str(),
            );
        }
        for subscription in &self.mgr_runtime.event_subscriptions {
            out.push_str(
                format!(
                    "subscription={} owner={} lease={} cursor={} kind_filter={} cell_filter={} port_filter={} binding_filter={} lease_filter={} delivered={} dropped={}\n",
                    subscription.subscription,
                    subscription.owner.0,
                    subscription.lease.0,
                    subscription.cursor,
                    subscription.kind_filter,
                    subscription.cell_filter,
                    subscription.port_filter,
                    subscription.binding_filter,
                    subscription.lease_filter,
                    subscription.delivered_events,
                    subscription.dropped_events,
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_audit_text(&self) -> String {
        let mut out = format!(
            "records={}\ndropped={}\nlast_sequence={}\n",
            self.audits.len(),
            self.dropped_audit_count,
            self.next_audit_sequence.saturating_sub(1),
        );
        for audit in &self.audits {
            out.push_str(
                format!(
                    "audit={} action={} status={} cell={} blockers=0x{:x} final_state={}\n",
                    audit.sequence,
                    audit.action,
                    audit.status,
                    audit.cell_id,
                    audit.blockers,
                    audit.final_state,
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_api_text(&self) -> String {
        let mut out = format!(
            "generation={}\nrecords={}\n",
            self.mgr_runtime.api_generation.0,
            self.mgr_runtime.api_registry.len(),
        );
        for api in &self.mgr_runtime.api_registry {
            out.push_str(
                format!(
                    "api={} namespace={} name={} contract={} kind={} flags=0x{:x} call_kind={} owner={}\n",
                    api.id,
                    fixed_field(&api.namespace, api.namespace_len),
                    fixed_field(&api.name, api.name_len),
                    fixed_field(&api.contract, api.contract_len),
                    api.kind,
                    api.flags,
                    api.call_kind,
                    api.owner_cell_id,
                )
                .as_str(),
            );
        }
        out.push_str(super::api_registry::diagnostic_text().as_str());
        out
    }

    fn sysfs_native_capabilities_text(&self) -> String {
        let mut out = format!(
            "exports={}\nimports={}\nevent_sequence={}\n",
            self.native_exports.len(),
            self.native_imports.len(),
            self.last_event_sequence(),
        );
        for export in &self.native_exports {
            out.push_str(
                format!(
                    "export owner={} name={} contract={} version={} address=0x{:x}\n",
                    export.owner.0,
                    export.name,
                    export.contract.as_str(),
                    export.version,
                    export.address,
                )
                .as_str(),
            );
        }
        for import in &self.native_imports {
            out.push_str(
                format!(
                    "import owner={} provider={} name={} contract={} version_range={}..={} selected_version={} address=0x{:x}\n",
                    import.owner.0,
                    import.provider.0,
                    import.name,
                    import.contract.as_str(),
                    import.min_version,
                    import.max_version,
                    import.selected_version,
                    import.address,
                )
                .as_str(),
            );
        }
        out
    }

    fn sysfs_faults_text(&self) -> String {
        let Some(fault) = general::elm_guard::last_fault_snapshot() else {
            return "faults=0\n".to_string();
        };
        format!(
            "faults=1\nlast_sequence={}\ncell={}\nphase={}\npc=0x{:x}\naddr=0x{:x}\ncode=0x{:x}\nreturn_pc=0x{:x}\nreturn_sp=0x{:x}\n",
            fault.sequence,
            fault.cell,
            fault.phase,
            fault.pc,
            fault.addr,
            fault.code,
            fault.return_pc,
            fault.return_sp,
        )
    }

    fn sysfs_todo_text(&self) -> String {
        let records = self.todo_registry_records();
        let active = records
            .iter()
            .filter(|record| record.flags & ELM_TODO_FLAG_ACTIVE != 0)
            .count();
        let mut out = format!(
            "records={}\nactive={}\nevent_sequence={}\n",
            records.len(),
            active,
            self.last_event_sequence(),
        );
        for record in &records {
            out.push_str(
                format!(
                    "todo kind={} flags=0x{:x} blocker=0x{:x} subject={} status={} name={} detail={}\n",
                    record.kind,
                    record.flags,
                    record.blocker,
                    record.subject_id,
                    record.status,
                    fixed_field(&record.name, record.name_len),
                    fixed_field(&record.detail, record.detail_len),
                )
                .as_str(),
            );
        }
        out
    }

    fn register_builtin_ports(&mut self) {
        for desc in builtin_port_descriptors() {
            self.register_port(PortRuntime::from_descriptor(desc));
            if desc.implemented {
                self.providers.push(ProviderRuntime {
                    port: desc.id,
                    owner: desc.owner,
                    access: desc.access,
                    backend: ProviderBackend::Kernel(kernel_provider_kind(desc.id)),
                    backend_epoch: 1,
                    dynamic: false,
                    queue_limit: provider_queue_limit_for_mode(desc.mode),
                    max_in_flight: provider_max_in_flight_for_mode(desc.mode),
                    in_flight: 0,
                    calls: 0,
                    failed_calls: 0,
                    revokes: 0,
                    async_submitted: 0,
                    async_completed: 0,
                    async_canceled: 0,
                    async_expired: 0,
                    async_rejected: 0,
                });
            }
        }
        log::info!("[elm] registered {} Nexus ports", self.ports.len());
    }

    fn register_builtin_mgr_actions(&mut self) -> Result<(), ElmError> {
        let cell_index = self.cell_index(ELM_MGR_ID).ok_or(ElmError::CellNotFound)?;
        let next_menu_generation = self
            .menu_generation
            .checked_next()
            .ok_or(ElmError::LeaseBusy)?;
        self.menu_items
            .try_reserve(1)
            .map_err(|_| ElmError::LeaseBusy)?;
        self.mgr_actions
            .try_reserve(1)
            .map_err(|_| ElmError::LeaseBusy)?;
        self.cells[cell_index]
            .owned_menu_items
            .try_reserve(1)
            .map_err(|_| ElmError::LeaseBusy)?;
        let action = self.alloc_action_id().ok_or(ElmError::LeaseBusy)?;
        let menu_item = self.alloc_menu_item_id().ok_or(ElmError::LeaseBusy)?;
        self.menu_items.push(MenuItemRuntime::new(
            menu_item,
            ELM_MGR_ID,
            action,
            ElmMenuItemKind::Action,
            ELM_MENU_FLAG_REQUIRES_SYS_ADMIN,
            "ELM Core 健康检查",
            "调用管理动作 provider 执行 Core 健康检查",
            "elm/mgr/health",
        ));
        self.mgr_actions.push(MgrActionRuntime {
            action,
            menu_item,
            owner: ELM_MGR_ID,
            kind: MgrActionKind::Health,
        });
        self.cells[cell_index].owned_menu_items.push(menu_item);
        self.menu_generation = next_menu_generation;
        self.emit(TopologyEventKind::MenuItemAdded, Some(ELM_MGR_ID));
        Ok(())
    }

    fn register_builtin_mgr_api(&mut self) -> Result<(), ElmError> {
        let stable_syscall = ELM_MGR_API_FLAG_STABLE | ELM_MGR_API_FLAG_SYSCALL;
        let stable_both = stable_syscall | ELM_MGR_API_FLAG_SYSFS;
        macro_rules! api {
            ($call:ident, $kind:expr, $flags:expr, $name:literal, $contract:literal) => {
                mgr_api(
                    ElmMgrCallKind::$call as u64,
                    $kind,
                    $flags,
                    ElmMgrCallKind::$call,
                    $name,
                    $contract,
                )
            };
        }
        let descriptors = [
            api!(
                QueryMenu,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "menu",
                "elm.mgr.menu@1"
            ),
            api!(
                LoadCell,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "cell.load",
                "elm.mgr.cell.load@1"
            ),
            api!(
                DetachCell,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "cell.detach",
                "elm.mgr.cell.detach@1"
            ),
            api!(
                PauseCell,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "cell.pause",
                "elm.mgr.cell.pause@1"
            ),
            api!(
                ResumeCell,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "cell.resume",
                "elm.mgr.cell.resume@1"
            ),
            api!(
                ReplaceCell,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "cell.replace",
                "elm.mgr.cell.replace@1"
            ),
            api!(
                QueryTopology,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "topology",
                "elm.mgr.topology@1"
            ),
            api!(
                QueryPolicy,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "policy",
                "elm.mgr.policy@1"
            ),
            api!(
                PreflightLifecycle,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "lifecycle.preflight",
                "elm.mgr.lifecycle@1"
            ),
            api!(
                QueryAudit,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "audit",
                "elm.mgr.audit@1"
            ),
            api!(
                QueryNexusBindings,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "bindings",
                "elm.mgr.bindings@1"
            ),
            api!(
                PreflightBind,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "binding.preflight",
                "elm.mgr.binding@1"
            ),
            api!(
                CommitBind,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "binding.commit",
                "elm.mgr.binding@1"
            ),
            api!(
                PreflightUnbind,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "unbinding.preflight",
                "elm.mgr.unbinding@1"
            ),
            api!(
                CommitUnbind,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "unbinding.commit",
                "elm.mgr.unbinding@1"
            ),
            api!(
                SubmitRuntimeLog,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                "runtime.log",
                "elm.mgr.runtime.log@1"
            ),
            api!(
                ReadRuntimeEvent,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                "runtime.event.read",
                "elm.mgr.runtime.event@1"
            ),
            api!(
                AckRuntimeEvent,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                "runtime.event.ack",
                "elm.mgr.runtime.event@1"
            ),
            api!(
                QueryRuntimePorts,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "runtime.ports",
                "elm.mgr.runtime.ports@1"
            ),
            api!(
                RegisterProviderPort,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.register",
                "elm.mgr.provider.register@1"
            ),
            api!(
                UnregisterProviderPort,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.unregister",
                "elm.mgr.provider.unregister@1"
            ),
            api!(
                QueryProviderPorts,
                ELM_MGR_API_KIND_PROVIDER,
                stable_both,
                "providers",
                "elm.mgr.providers@1"
            ),
            api!(
                InvokeProvider,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.invoke",
                "elm.mgr.provider.invoke@1"
            ),
            api!(
                QueryProviderStats,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.stats",
                "elm.mgr.provider.stats@1"
            ),
            api!(
                QueryHealth,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "health",
                "elm.mgr.health@1"
            ),
            api!(
                SubmitProviderCall,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.async.submit",
                "elm.mgr.provider.async@1"
            ),
            api!(
                PollProviderReply,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.async.poll",
                "elm.mgr.provider.async@1"
            ),
            api!(
                CancelProviderCall,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.async.cancel",
                "elm.mgr.provider.async@1"
            ),
            api!(
                QueryProviderQueue,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.queue",
                "elm.mgr.provider.queue@1"
            ),
            api!(
                QueryApiRegistry,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "api.registry",
                "elm.mgr.api.registry@1"
            ),
            api!(
                SubscribeEvent,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                "event.subscribe",
                "elm.mgr.event.subscribe@1"
            ),
            api!(
                UnsubscribeEvent,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                "event.unsubscribe",
                "elm.mgr.event.unsubscribe@1"
            ),
            api!(
                QueryEventSubscriptions,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                "event.subscriptions",
                "elm.mgr.event.subscriptions@1"
            ),
            api!(
                ReadSubscribedEvents,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                "event.read",
                "elm.mgr.event.read@1"
            ),
            api!(
                QueryProviderSnapshot,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                "provider.snapshot",
                "elm.mgr.provider.snapshot@1"
            ),
            api!(
                QueryNativeCapabilities,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "native.capabilities",
                "elm.mgr.native.capabilities@1"
            ),
            api!(
                QueryTodoRegistry,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "todo.registry",
                "elm.mgr.todo.registry@1"
            ),
            api!(
                QueryExtensions,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "extensions",
                "elm.mgr.extensions@1"
            ),
            api!(
                PreflightExtensionAttach,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "extension.attach.preflight",
                "elm.mgr.extension.attach@1"
            ),
            api!(
                CommitExtensionAttach,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "extension.attach",
                "elm.mgr.extension.attach@1"
            ),
            api!(
                CommitExtensionDetach,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "extension.detach",
                "elm.mgr.extension.detach@1"
            ),
            api!(
                DispatchExtension,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "extension.dispatch",
                "elm.mgr.extension.dispatch@1"
            ),
            api!(
                QueryFaultDump,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "fault.dump",
                "elm.mgr.fault.dump@1"
            ),
            api!(
                QueryLifecycleTrace,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "trace.lifecycle",
                "elm.mgr.trace.lifecycle@1"
            ),
            api!(
                QueryProviderCallTrace,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "trace.provider",
                "elm.mgr.trace.provider@1"
            ),
            api!(
                QueryMixinTrace,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "trace.mixin",
                "elm.mgr.trace.mixin@1"
            ),
            api!(
                QueryReplaceTrace,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "trace.replace",
                "elm.mgr.trace.replace@1"
            ),
            api!(
                QueryPolicyTrace,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "trace.policy",
                "elm.mgr.trace.policy@1"
            ),
            api!(
                QueryResourceDiagnostics,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "trace.resource",
                "elm.mgr.trace.resource@1"
            ),
            api!(
                QueryRuntimeJournal,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "journal",
                "elm.mgr.journal@1"
            ),
            api!(
                QueryCellPolicy,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "cell.policy",
                "elm.mgr.cell.policy@1"
            ),
            api!(
                UpdateCellPolicy,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "cell.policy.update",
                "elm.mgr.cell.policy@1"
            ),
            api!(
                QueryResourceBudget,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_syscall,
                "resource.budget",
                "elm.mgr.resource.budget@1"
            ),
            api!(
                UpdateResourceBudget,
                ELM_MGR_API_KIND_CONTROL,
                stable_syscall,
                "resource.budget.update",
                "elm.mgr.resource.budget@1"
            ),
            api!(
                QueryTrustState,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                "trust",
                "elm.mgr.trust@1"
            ),
        ];
        self.mgr_runtime
            .api_registry
            .try_reserve(descriptors.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        if self
            .mgr_runtime
            .api_generation
            .0
            .checked_add(descriptors.len() as u64)
            .is_none()
        {
            return Err(ElmError::LeaseBusy);
        }
        for descriptor in descriptors {
            if !self.mgr_runtime.register_api(descriptor) {
                return Err(ElmError::LeaseBusy);
            }
        }
        Ok(())
    }

    fn register_builtin_native_exports(&mut self) -> Result<(), ElmError> {
        self.native_exports.push(NativeExportRuntime {
            owner: ELM_MGR_ID,
            generation: Generation::FIRST,
            name: ELM_API_ROOT_IMPORT_NAME.to_string(),
            contract: FlowContract::new(ELM_API_ROOT_IMPORT_CONTRACT)?,
            version: u32::from(ELM_API_CURRENT_VERSION),
            flags: 0,
            address: &ELM_API_ROOT_V1 as *const ElmApiRootV1 as usize,
            bounds: None,
        });
        self.native_exports.push(NativeExportRuntime {
            owner: ELM_MGR_ID,
            generation: Generation::FIRST,
            name: ELM_RUNTIME_LOG_EXPORT_NAME.to_string(),
            contract: FlowContract::new(ELM_RUNTIME_LOG_EXPORT_CONTRACT)?,
            version: ELM_RUNTIME_LOG_EXPORT_VERSION,
            flags: 0,
            address: elm_runtime_log_v1 as usize,
            bounds: None,
        });
        Ok(())
    }

    pub(crate) fn register_kernel_provider_specs(
        &mut self,
        specs: &'static [ElmKernelProviderSpec],
    ) -> Result<usize, ElmError> {
        self.register_kernel_provider_specs_for_owner(ELM_MGR_ID, specs)
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn register_kernel_provider_specs_for_owner(
        &mut self,
        owner: ElmId,
        specs: &'static [ElmKernelProviderSpec],
    ) -> Result<usize, ElmError> {
        self.register_kernel_provider_specs_for_owner_inner(owner, specs)
    }

    #[cfg(not(feature = "kernel-tests"))]
    fn register_kernel_provider_specs_for_owner(
        &mut self,
        owner: ElmId,
        specs: &'static [ElmKernelProviderSpec],
    ) -> Result<usize, ElmError> {
        self.register_kernel_provider_specs_for_owner_inner(owner, specs)
    }

    fn register_kernel_provider_specs_for_owner_inner(
        &mut self,
        owner: ElmId,
        specs: &'static [ElmKernelProviderSpec],
    ) -> Result<usize, ElmError> {
        let mut registered = 0usize;
        for spec in specs {
            FlowContract::new(spec.port_contract)?;
            FlowContract::new(spec.api_contract)?;
            if self
                .ports
                .iter()
                .any(|port| port.contract() == spec.port_contract)
            {
                continue;
            }

            let port = self.alloc_port_id().ok_or(ElmError::LeaseBusy)?;
            let api_id = self
                .mgr_runtime
                .alloc_kernel_provider_api_id()
                .ok_or(ElmError::LeaseBusy)?;
            let desc = spec.port_descriptor(port, owner);
            self.register_port(PortRuntime::from_descriptor(desc));
            self.providers.push(ProviderRuntime {
                port,
                owner: Some(owner),
                access: spec.access,
                backend: ProviderBackend::KernelOps(spec),
                backend_epoch: 1,
                dynamic: false,
                queue_limit: provider_queue_limit_for_mode(spec.mode),
                max_in_flight: provider_max_in_flight_for_mode(spec.mode),
                in_flight: 0,
                calls: 0,
                failed_calls: 0,
                revokes: 0,
                async_submitted: 0,
                async_completed: 0,
                async_canceled: 0,
                async_expired: 0,
                async_rejected: 0,
            });
            if !self
                .mgr_runtime
                .register_api(spec.api_descriptor(api_id, owner))
            {
                if let Some(index) = self.provider_index(port) {
                    self.providers.remove(index);
                }
                self.ports.retain(|runtime| runtime.id != port);
                return Err(ElmError::LeaseBusy);
            }
            registered = registered.saturating_add(1);
        }
        if registered != 0 {
            log::info!("[elm] registered {} kernel provider spec(s)", registered);
        }
        Ok(registered)
    }

    fn register_port(&mut self, runtime: PortRuntime) {
        let port = runtime.id;
        let contract = runtime.contract().to_string();
        let implemented = runtime.implemented;
        self.ports.push(runtime);
        self.emit_port(TopologyEventKind::PortAdded, port);
        if !implemented {
            log::debug!("[elm] port {} registered as TODO(elm) 提供者", contract);
        }
    }

    #[allow(dead_code)]
    fn insert_loaded_cell(
        &mut self,
        id: ElmId,
        parent: ElmId,
        resource_budget: ElmResourceBudget,
        manifest: ElmManifest,
        name: String,
        ebi_arch: ElmEbiArch,
        unit: &ElmEbiUnit,
        source: ElmEbiSourceKind,
        grant_management: bool,
    ) -> Result<(), ElmError> {
        self.cells.try_reserve(1).map_err(|_| {
            log::error!("[elm] 单元表扩容失败 cell={} parent={}", id.0, parent.0);
            ElmError::LeaseBusy
        })?;
        let parent_policy = {
            let parent_cell = self
                .cells
                .iter()
                .find(|cell| cell.id == parent)
                .ok_or(ElmError::CellNotFound)?;
            if parent_cell.state != ElmState::Active || parent_cell.isolated {
                log::error!(
                    "[elm] 父单元不可接纳子单元 cell={} parent={} state={:?} isolated={}",
                    id.0,
                    parent.0,
                    parent_cell.state,
                    parent_cell.isolated
                );
                return Err(ElmError::InvalidTransition);
            }
            if !budget_is_subset(resource_budget, parent_cell.resource_budget)
                || !self.child_budget_allocation_fits(parent, id, resource_budget)
            {
                log::error!(
                    "[elm] 子单元预算超出父单元配额 cell={} parent={}",
                    id.0,
                    parent.0
                );
                return Err(ElmError::LeaseBusy);
            }
            parent_cell.cell_policy
        };
        let kind = manifest.kind;
        if !super::resource_accounting::register_cell(id, resource_budget) {
            log::error!("[elm] 资源账本拒绝登记 cell={}", id.0);
            return Err(ElmError::LeaseBusy);
        }
        if !super::owned_resource::register_owner(id, Generation::FIRST) {
            log::error!("[elm] 所有权资源表拒绝登记 cell={}", id.0);
            let _ = super::resource_accounting::retire_cell(id);
            return Err(ElmError::LeaseBusy);
        }
        if let Err(err) = self.graph.insert_cell(id, manifest) {
            log::error!("[elm] 绑定图拒绝插入 cell={}: {:?}", id.0, err);
            let _ = super::owned_resource::retire_owner(id, Generation::FIRST);
            let _ = super::resource_accounting::retire_cell(id);
            return Err(err);
        }
        if let Err(err) = self.graph.set_parent(id, parent) {
            log::error!(
                "[elm] 绑定图拒绝父子关系 cell={} parent={}: {:?}",
                id.0,
                parent.0,
                err
            );
            let _ = self.graph.remove_cell(id);
            let _ = super::owned_resource::retire_owner(id, Generation::FIRST);
            let _ = super::resource_accounting::retire_cell(id);
            return Err(err);
        }
        if grant_management
            && (kind != ElmKind::Manager
                || parent_policy.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT == 0)
        {
            let _ = self.graph.remove_cell(id);
            let _ = super::owned_resource::retire_owner(id, Generation::FIRST);
            let _ = super::resource_accounting::retire_cell(id);
            return Err(ElmError::PermissionDenied);
        }
        let mut allowed_actions = parent_policy.allowed_actions & !ELM_CELL_POLICY_ALLOW_MANAGEMENT;
        if grant_management {
            allowed_actions |= ELM_CELL_POLICY_ALLOW_MANAGEMENT;
        }
        let mut cell_policy = ElmCellPolicyV1::new(
            id.0,
            Generation::FIRST.0,
            allowed_actions,
            ELM_MGR_STATUS_OK,
            0,
        );
        cell_policy.flags = parent_policy.flags & !ELM_CELL_POLICY_FLAG_LOCKED;
        cell_policy.provider_flags = parent_policy.provider_flags;
        cell_policy.extension_flags = parent_policy.extension_flags;
        cell_policy.native_flags = parent_policy.native_flags;
        cell_policy.resource_flags = parent_policy.resource_flags;
        self.cells.push(CellRuntime {
            id,
            parent: Some(parent),
            state: ElmState::Discovered,
            kind,
            generation: Generation::FIRST,
            name,
            ebi_source: source,
            ebi_arch,
            ebi_status: if unit.has_native_code() {
                ElmEbiLoadStatus::NativeCodeTodo
            } else {
                ElmEbiLoadStatus::Ok
            },
            has_native_code: unit.has_native_code(),
            native_segment_count: unit.segments.len() as u16,
            native_import_count: unit.imports.len() as u16,
            native_export_count: unit.exports.len() as u16,
            elmapi_version: self.select_elmapi_version(unit).ok().flatten().unwrap_or(0),
            lifecycle_hooks_declared: unit.lifecycle_hooks.is_some(),
            lifecycle_executor_ready: false,
            lifecycle_initialized: false,
            lifecycle_finalized: false,
            resource_budget,
            cell_policy,
            policy_epoch: 1,
            active_executions: 0,
            exclusive_execution: false,
            native_faults: 0,
            isolated: false,
            isolation_blocker: 0,
            trust_unsigned: false,
            signer_key_id: [0; 32],
            release_epoch: 0,
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        self.emit(TopologyEventKind::CellAdded, Some(id));
        if let Err(err) = self
            .transition_cell_state(id, ElmState::Verified)
            .and_then(|_| self.transition_cell_state(id, ElmState::Loaded))
        {
            log::error!("[elm] 单元初始状态迁移失败 cell={}: {:?}", id.0, err);
            self.cells.retain(|cell| cell.id != id);
            let _ = self.graph.remove_cell(id);
            let _ = super::owned_resource::retire_owner(id, Generation::FIRST);
            let _ = super::resource_accounting::retire_cell(id);
            self.emit(TopologyEventKind::CellRemoved, Some(id));
            return Err(err);
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn activate_loaded_cell(
        &mut self,
        id: ElmId,
        unit: &ElmEbiUnit,
        topology: &ResolvedEbiTopology,
        native_image: Option<&LoadedElmImage>,
    ) -> Result<(), ElmError> {
        self.graph.try_reserve_edges(
            topology.dependencies.len(),
            topology
                .extensions
                .len()
                .saturating_add(usize::from(unit.menu.is_some())),
            usize::from(unit.menu.is_some()),
        )?;
        self.ports
            .try_reserve(unit.provider_ports.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        self.providers
            .try_reserve(unit.provider_ports.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        if unit.menu.is_some() {
            self.menu_items
                .try_reserve(1)
                .map_err(|_| ElmError::LeaseBusy)?;
            let cell_index = self.cell_index(id).ok_or(ElmError::CellNotFound)?;
            self.cells[cell_index]
                .owned_bindings
                .try_reserve(1)
                .map_err(|_| ElmError::LeaseBusy)?;
            self.cells[cell_index]
                .owned_menu_items
                .try_reserve(1)
                .map_err(|_| ElmError::LeaseBusy)?;
        }
        for point in &unit.extension_points {
            self.graph.add_extension_point_with_mode(
                id,
                point.point.clone(),
                point.contract.clone(),
                point.mode,
            )?;
        }
        for (provider, contract) in &topology.dependencies {
            self.graph.add_dependency(id, *provider, contract.clone())?;
        }
        for extension in &topology.extensions {
            self.graph.add_extension_with_dispatch(
                id,
                extension.target,
                extension.point.clone(),
                extension.contract.clone(),
                extension.handler_contract.clone(),
                extension.priority,
            )?;
        }
        for provider in &unit.provider_ports {
            self.register_ebi_provider_port(id, provider, native_image)?;
        }
        if let Some(menu) = &unit.menu {
            let menu_contract = FlowContract::new("mgr.menu.item@1")?;
            self.graph
                .add_extension(id, ELM_MGR_ID, "menu.item", menu_contract.clone())?;
            let binding = self.alloc_binding_id().ok_or(ElmError::LeaseBusy)?;
            let lease = self.alloc_lease_id().ok_or(ElmError::LeaseBusy)?;
            self.attach_menu_binding_with_menu(
                id,
                ELM_MGR_MENU_PORT_ID,
                menu_contract,
                binding,
                lease,
                menu.kind,
                menu.flags,
                &menu.label,
                &menu.description,
                &menu.route,
            )?;
        }

        self.transition_cell_state(id, ElmState::Linked)?;
        self.transition_cell_state(id, ElmState::Ready)?;
        self.transition_cell_state(id, ElmState::Active)?;
        Ok(())
    }

    fn preflight_ebi_topology(&self, unit: &ElmEbiUnit) -> Result<ResolvedEbiTopology, ElmError> {
        if self.cell_id_by_name(unit.manifest.name.as_str()).is_some() {
            return Err(ElmError::DuplicateCell);
        }

        let mut topology = ResolvedEbiTopology::empty();
        topology
            .dependencies
            .try_reserve_exact(unit.dependencies.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        topology
            .extensions
            .try_reserve_exact(unit.extensions.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        for dependency in &unit.dependencies {
            let provider = self.resolve_unique_cell_name(&dependency.provider_name)?;
            topology
                .dependencies
                .push((provider, dependency.contract.clone()));
        }

        for (index, point) in unit.extension_points.iter().enumerate() {
            if unit
                .extension_points
                .iter()
                .skip(index + 1)
                .any(|other| other.point == point.point)
            {
                return Err(ElmError::DuplicateExtensionPoint);
            }
        }

        for extension in &unit.extensions {
            let target = self.resolve_unique_cell_name(&extension.target_name)?;
            let mut point_exists = false;
            let mut contract_matches = false;
            let mut point_mode = ElmMixinMode::Chain;
            for point in self.graph.extension_points() {
                if point.owner == target && point.name == extension.point {
                    point_exists = true;
                    if point.contract == extension.contract {
                        contract_matches = true;
                        point_mode = point.mode;
                    }
                }
            }
            if !point_exists {
                return Err(ElmError::ExtensionPointNotFound);
            }
            if !contract_matches {
                return Err(ElmError::ContractMismatch);
            }
            if point_mode == ElmMixinMode::Exclusive
                && (self
                    .graph
                    .extensions()
                    .iter()
                    .any(|edge| edge.target == target && edge.point == extension.point)
                    || topology
                        .extensions
                        .iter()
                        .any(|edge| edge.target == target && edge.point == extension.point))
            {
                return Err(ElmError::DuplicateBinding);
            }
            topology.extensions.push(ResolvedEbiExtension {
                target,
                point: extension.point.clone(),
                contract: extension.contract.clone(),
                handler_contract: extension.handler_contract.clone(),
                priority: extension.priority,
            });
        }

        let mut provider_contracts = Vec::new();
        provider_contracts
            .try_reserve_exact(unit.provider_ports.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        for provider in &unit.provider_ports {
            if provider.flags != 0 {
                return Err(ElmError::InvalidTransition);
            }
            let contract = provider.contract.as_str();
            if self.ports.iter().any(|port| port.contract() == contract)
                || provider_contracts
                    .iter()
                    .any(|seen: &String| seen.as_str() == contract)
            {
                return Err(ElmError::DuplicatePort);
            }
            provider_contracts.push(contract.to_string());
        }

        Ok(topology)
    }

    fn preflight_ebi_topology_for_replace(
        &self,
        target: ElmId,
        unit: &ElmEbiUnit,
    ) -> Result<ResolvedEbiTopology, ElmError> {
        if self.cell_id_by_name(unit.manifest.name.as_str()) != Some(target) {
            return Err(ElmError::DuplicateCell);
        }

        let mut topology = ResolvedEbiTopology::empty();
        topology
            .dependencies
            .try_reserve_exact(unit.dependencies.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        topology
            .extensions
            .try_reserve_exact(unit.extensions.len())
            .map_err(|_| ElmError::LeaseBusy)?;
        for dependency in &unit.dependencies {
            let provider = self.resolve_unique_cell_name(&dependency.provider_name)?;
            topology
                .dependencies
                .push((provider, dependency.contract.clone()));
        }
        for extension in &unit.extensions {
            let target = self.resolve_unique_cell_name(&extension.target_name)?;
            topology.extensions.push(ResolvedEbiExtension {
                target,
                point: extension.point.clone(),
                contract: extension.contract.clone(),
                handler_contract: extension.handler_contract.clone(),
                priority: extension.priority,
            });
        }
        Ok(topology)
    }

    fn replace_surface_compatible(
        &self,
        id: ElmId,
        unit: &ElmEbiUnit,
        topology: &ResolvedEbiTopology,
    ) -> bool {
        let current_dependency_count = self
            .graph
            .dependencies()
            .iter()
            .filter(|edge| edge.consumer == id)
            .count();
        if current_dependency_count != topology.dependencies.len()
            || self
                .graph
                .dependencies()
                .iter()
                .filter(|edge| edge.consumer == id)
                .any(|edge| {
                    !topology.dependencies.iter().any(|(provider, contract)| {
                        edge.provider == *provider && edge.contract == *contract
                    })
                })
        {
            log::error!(
                "[elm] 替换表面不兼容：dependency cell={} runtime={} image={}",
                id.0,
                current_dependency_count,
                topology.dependencies.len()
            );
            return false;
        }

        let current_extension_count = self
            .graph
            .extensions()
            .iter()
            .filter(|edge| edge.extension == id)
            .count();
        if current_extension_count != topology.extensions.len()
            || self
                .graph
                .extensions()
                .iter()
                .filter(|edge| edge.extension == id)
                .any(|edge| {
                    !topology.extensions.iter().any(|requested| {
                        edge.target == requested.target
                            && edge.point == requested.point
                            && edge.contract == requested.contract
                            && edge.handler_contract == requested.handler_contract
                            && edge.priority == requested.priority
                    })
                })
        {
            log::error!(
                "[elm] 替换表面不兼容：extension cell={} runtime={} image={}",
                id.0,
                current_extension_count,
                topology.extensions.len()
            );
            return false;
        }

        let current_point_count = self
            .graph
            .extension_points_iter()
            .filter(|point| point.owner == id)
            .count();
        if current_point_count != unit.extension_points.len()
            || self
                .graph
                .extension_points_iter()
                .filter(|point| point.owner == id)
                .any(|point| {
                    !unit.extension_points.iter().any(|requested| {
                        point.name == requested.point
                            && point.contract == requested.contract
                            && point.mode == requested.mode
                    })
                })
        {
            log::error!(
                "[elm] 替换表面不兼容：extension point cell={} runtime={} image={}",
                id.0,
                current_point_count,
                unit.extension_points.len()
            );
            return false;
        }

        if unit.menu.is_some() != self.menu_items.iter().any(|item| item.owner == id) {
            log::error!("[elm] 替换表面不兼容：menu cell={}", id.0);
            return false;
        }

        let current_port_count = self
            .providers
            .iter()
            .filter(|provider| provider.dynamic && provider.owner == Some(id))
            .count();
        if current_port_count != unit.provider_ports.len()
            || self
                .providers
                .iter()
                .filter(|provider| provider.dynamic && provider.owner == Some(id))
                .any(|provider| {
                    self.port_desc(provider.port).is_none_or(|port| {
                        !unit.provider_ports.iter().any(|requested| {
                            port.contract == requested.contract.as_str()
                                && port.access == requested.access
                                && port.direction == requested.direction
                                && port.mode == requested.mode
                        })
                    })
                })
        {
            log::error!(
                "[elm] 替换表面不兼容：provider port cell={} runtime={} image={}",
                id.0,
                current_port_count,
                unit.provider_ports.len()
            );
            return false;
        }

        let compatible = self.native_export_surface_compatible_for_replace(id, unit);
        if !compatible {
            log::error!("[elm] 替换表面不兼容：native export cell={}", id.0);
        }
        compatible
    }

    fn native_export_surface_compatible_for_replace(
        &self,
        owner: ElmId,
        unit: &ElmEbiUnit,
    ) -> bool {
        // 直接固定导出形成地址稳定承诺，替换镜像必须原样保留其声明。
        if self
            .native_exports
            .iter()
            .filter(|export| export.owner == owner)
            .any(|export| {
                !native_export_is_managed(export.flags)
                    && !unit.exports.iter().any(|requested| {
                        export.name == requested.name
                            && export.contract == requested.contract
                            && export.version == requested.version
                            && export.flags == requested.flags
                    })
            })
        {
            return false;
        }

        // 受管导出允许版本演进，但每个现存 importer 必须能唯一选择最高兼容版本。
        self.native_imports
            .iter()
            .filter(|import| import.provider == owner)
            .all(|import| {
                if !native_import_is_managed(import.flags) {
                    return unit.exports.iter().any(|export| {
                        export.name == import.name
                            && export.contract == import.contract
                            && export.version == import.selected_version
                            && !native_export_is_managed(export.flags)
                    });
                }
                let Some(highest) = unit
                    .exports
                    .iter()
                    .filter(|export| {
                        export.name == import.name
                            && export.contract == import.contract
                            && native_export_is_managed(export.flags)
                            && export.version >= import.min_version
                            && export.version <= import.max_version
                    })
                    .map(|export| export.version)
                    .max()
                else {
                    return false;
                };
                unit.exports
                    .iter()
                    .filter(|export| {
                        export.name == import.name
                            && export.contract == import.contract
                            && native_export_is_managed(export.flags)
                            && export.version == highest
                    })
                    .count()
                    == 1
            })
    }

    fn native_exports_available_for_replace(&self, owner: ElmId, unit: &ElmEbiUnit) -> bool {
        for (index, export) in unit.exports.iter().enumerate() {
            if unit.exports[..index].iter().any(|seen| {
                seen.name == export.name
                    && seen.contract == export.contract
                    && seen.version == export.version
            }) || self.native_exports.iter().any(|existing| {
                existing.owner != owner
                    && existing.name == export.name
                    && existing.contract == export.contract
                    && existing.version == export.version
            }) {
                return false;
            }
        }
        true
    }

    fn resolve_native_imports(
        &mut self,
        owner: ElmId,
        parent: ElmId,
        owner_generation: Generation,
        unit: &ElmEbiUnit,
    ) -> Result<
        (
            Vec<usize>,
            Vec<(ElmId, FlowContract)>,
            Vec<NativeImportRuntime>,
        ),
        ElmEbiLoadStatus,
    > {
        let selected_elmapi = self.select_elmapi_version(unit)?;
        let mut values = Vec::new();
        let mut dependencies = Vec::new();
        let mut imports = Vec::new();
        values
            .try_reserve_exact(unit.imports.len())
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        dependencies
            .try_reserve_exact(unit.imports.len())
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        imports
            .try_reserve_exact(unit.imports.len())
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        for (import_index, import) in unit.imports.iter().enumerate() {
            let elmapi_root = unit
                .api_compatibility
                .as_ref()
                .is_some_and(|compatibility| {
                    compatibility.root_import_index == import_index as u32
                });
            let required_version = if elmapi_root {
                Some(u32::from(
                    selected_elmapi.ok_or(ElmEbiLoadStatus::UnsupportedAbi)?,
                ))
            } else {
                None
            };
            let mut selected_export: Option<&NativeExportRuntime> = None;
            let mut ambiguous = false;
            for export in self.native_exports.iter().filter(|export| {
                export.owner != owner
                    && export.name == import.name
                    && export.contract == import.contract
                    && import.accepts_version(export.version)
                    && required_version.is_none_or(|version| export.version == version)
                    && native_import_is_managed(import.flags)
                        == native_export_is_managed(export.flags)
                    && self.native_export_visible_to_import(parent, unit, import, export)
            }) {
                match selected_export {
                    None => {
                        selected_export = Some(export);
                        ambiguous = false;
                    }
                    Some(current) if export.version > current.version => {
                        selected_export = Some(export);
                        ambiguous = false;
                    }
                    Some(current) if export.version == current.version => ambiguous = true,
                    Some(_) => {}
                }
            }
            let Some(export) = selected_export else {
                if import.is_optional() && !elmapi_root {
                    values.push(0);
                    continue;
                }
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            };
            if ambiguous {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            if !self.cell_exists(export.owner) {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            if self.cell_is_isolated(export.owner) {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            let provider_generation = self
                .cells
                .iter()
                .find(|cell| cell.id == export.owner)
                .map(|cell| cell.generation)
                .ok_or(ElmEbiLoadStatus::RuntimeRejected)?;
            if provider_generation != export.generation {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            let managed = native_import_is_managed(import.flags);
            if managed && export.bounds.is_none() {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            if !managed && export.owner != ELM_MGR_ID {
                // 动态 ELM 之间不得建立裸 Rust 地址依赖；直接固定只保留给内建根表。
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            let handle = if managed {
                take_monotonic_id(&mut self.next_managed_import_handle)
                    .ok_or(ElmEbiLoadStatus::RuntimeRejected)?
            } else {
                0
            };
            values.push(if managed {
                usize::try_from(handle).map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?
            } else {
                export.address
            });
            if !dependencies
                .iter()
                .any(|(owner, contract)| *owner == export.owner && *contract == import.contract)
            {
                dependencies.push((export.owner, import.contract.clone()));
            }
            imports.push(NativeImportRuntime {
                handle,
                owner,
                owner_generation,
                provider: export.owner,
                provider_generation,
                name: import.name.clone(),
                contract: import.contract.clone(),
                min_version: import.min_version,
                max_version: import.max_version,
                selected_version: export.version,
                flags: import.flags,
                address: export.address,
            });
        }
        Ok((values, dependencies, imports))
    }

    fn native_export_visible_to_import(
        &self,
        parent: ElmId,
        unit: &ElmEbiUnit,
        import: &elm_model::ElmEbiImportDecl,
        export: &NativeExportRuntime,
    ) -> bool {
        if export.owner == ELM_MGR_ID && import.allows_builtin() {
            return true;
        }
        let declared_dependency = self
            .cells
            .iter()
            .find(|cell| cell.id == export.owner)
            .is_some_and(|cell| {
                unit.dependencies.iter().any(|dependency| {
                    dependency.provider_name == cell.name && dependency.contract == import.contract
                })
            });
        if declared_dependency
            && (export.flags
                & (elm_model::ELM_EBI_EXPORT_FLAG_PRIVATE
                    | elm_model::ELM_EBI_EXPORT_FLAG_DEPENDENCY
                    | elm_model::ELM_EBI_EXPORT_FLAG_SUBTREE)
                == 0
                || export.flags & elm_model::ELM_EBI_EXPORT_FLAG_DEPENDENCY != 0)
        {
            return true;
        }
        import.allows_ancestor()
            && export.flags & elm_model::ELM_EBI_EXPORT_FLAG_SUBTREE != 0
            && (export.owner == parent || self.cell_is_descendant_of(parent, export.owner))
    }

    fn select_elmapi_version(&self, unit: &ElmEbiUnit) -> Result<Option<u16>, ElmEbiLoadStatus> {
        let Some(compatibility) = &unit.api_compatibility else {
            return Ok(None);
        };
        let selected = compatibility
            .select_highest_common(&[ELM_API_CURRENT_VERSION])
            .ok_or(ElmEbiLoadStatus::UnsupportedAbi)?;
        let features = match selected {
            ELM_API_CURRENT_VERSION => ELM_API_FEATURES_V1,
            _ => return Err(ElmEbiLoadStatus::UnsupportedAbi),
        };
        if compatibility.required_features & !features != 0 {
            return Err(ElmEbiLoadStatus::UnsupportedAbi);
        }
        let root = unit
            .imports
            .get(compatibility.root_import_index as usize)
            .ok_or(ElmEbiLoadStatus::InvalidTarget)?;
        if root.name != ELM_API_ROOT_IMPORT_NAME
            || root.contract.as_str() != ELM_API_ROOT_IMPORT_CONTRACT
        {
            return Err(ElmEbiLoadStatus::InvalidTarget);
        }
        Ok(Some(selected))
    }

    fn native_exports_available(&self, unit: &ElmEbiUnit) -> bool {
        for (index, export) in unit.exports.iter().enumerate() {
            if unit.exports[..index].iter().any(|seen| {
                seen.name == export.name
                    && seen.contract == export.contract
                    && seen.version == export.version
            }) || self.native_exports.iter().any(|existing| {
                existing.name == export.name
                    && existing.contract == export.contract
                    && existing.version == export.version
            }) {
                return false;
            }
        }
        true
    }

    fn native_provider_handlers_available(
        &self,
        image: &ElmEbiImage,
        loaded: &LoadedElmImage,
    ) -> bool {
        if !image.has_code_segment() {
            return true;
        }
        image.unit.provider_ports.iter().all(|provider| {
            provider.handler_symbol.is_some()
                && matches!(loaded.provider_handler_for_decl(provider), Ok(Some(_)))
                && (provider.snapshot_symbol.is_none()
                    || matches!(loaded.provider_snapshot_for_decl(provider), Ok(Some(_))))
        })
    }

    fn collect_native_exports(
        &self,
        owner: ElmId,
        generation: Generation,
        image: &ElmEbiImage,
        loaded: &LoadedElmImage,
    ) -> Result<Vec<NativeExportRuntime>, ElmEbiLoadStatus> {
        let bounds = loaded
            .execution_bounds()
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        let mut exports = Vec::new();
        exports
            .try_reserve_exact(image.unit.exports.len())
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        for export in &image.unit.exports {
            if !native_export_is_managed(export.flags) {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            exports.push(NativeExportRuntime {
                owner,
                generation,
                name: export.name.clone(),
                contract: export.contract.clone(),
                version: export.version,
                flags: export.flags,
                address: loaded.export_address(&export.name)?,
                bounds: Some(bounds),
            });
        }
        Ok(exports)
    }

    fn port_desc(&self, id: PortId) -> Option<PortRuntime> {
        self.ports.iter().find(|port| port.id == id).cloned()
    }

    fn is_bind_supported_port(&self, desc: &PortRuntime) -> bool {
        desc.implemented
            && (matches!(
                (desc.id, desc.contract()),
                (ELM_CORE_LOG_PORT_ID, ELM_CORE_LOG_CONTRACT)
                    | (ELM_CORE_EVENT_PORT_ID, ELM_CORE_EVENT_CONTRACT)
                    | (ELM_MGR_MENU_PORT_ID, ELM_MGR_MENU_CONTRACT)
                    | (ELM_MGR_ACTION_PORT_ID, ELM_MGR_ACTION_CONTRACT)
            ) || self.provider_index(desc.id).is_some())
    }

    fn provider_register_response(
        &mut self,
        owner: ElmId,
        port: PortId,
        access: ElmPortAccessPolicy,
        blockers: u64,
    ) -> ElmProviderPortRegisterResponse {
        self.record_mgr_audit(
            ELM_MGR_ACTION_PROVIDER_REGISTER,
            owner,
            blockers,
            self.cell_state(owner).map(state_code).unwrap_or(0),
        );
        ElmProviderPortRegisterResponse::new(
            owner.0,
            port.0,
            status_from_blockers(blockers),
            access as u32,
            blockers,
        )
    }

    fn register_ebi_provider_port(
        &mut self,
        owner: ElmId,
        decl: &ElmEbiProviderPortDecl,
        native_image: Option<&LoadedElmImage>,
    ) -> Result<(), ElmError> {
        if decl.flags != 0 {
            return Err(ElmError::InvalidTransition);
        }
        if self.cell_is_isolated(owner)
            || self.cell_resource_over_quota(owner, ElmResourceKind::ProviderPort)
        {
            return Err(ElmError::InvalidTransition);
        }
        if self
            .ports
            .iter()
            .any(|port| port.contract() == decl.contract.as_str())
        {
            return Err(ElmError::DuplicatePort);
        }
        self.ports.try_reserve(1).map_err(|_| ElmError::LeaseBusy)?;
        self.providers
            .try_reserve(1)
            .map_err(|_| ElmError::LeaseBusy)?;
        let handler = match native_image {
            Some(image) => image
                .provider_handler_for_decl(decl)
                .map_err(|_| ElmError::InvalidTransition)?,
            None => None,
        };
        let snapshot = match native_image {
            Some(image) => image
                .provider_snapshot_for_decl(decl)
                .map_err(|_| ElmError::InvalidTransition)?,
            None => None,
        };
        let bounds = match native_image {
            Some(image) => Some(image.execution_bounds()?),
            None => None,
        };

        let port = self.alloc_port_id().ok_or(ElmError::LeaseBusy)?;
        let runtime = PortRuntime::new(
            port,
            Some(owner),
            decl.contract.as_str(),
            decl.direction,
            decl.mode,
            decl.access,
            handler.is_some(),
            handler.is_some(),
        );
        self.register_port(runtime);
        self.providers.push(ProviderRuntime {
            port,
            owner: Some(owner),
            access: decl.access,
            backend: match handler {
                Some(handler) => ProviderBackend::ElmNative(NativeProviderBackend {
                    owner,
                    generation: self
                        .cells
                        .iter()
                        .find(|cell| cell.id == owner)
                        .map(|cell| cell.generation)
                        .unwrap_or(Generation::FIRST),
                    handler,
                    snapshot,
                    bounds: bounds.ok_or(ElmError::InvalidTransition)?,
                }),
                None => ProviderBackend::ElmNativeTodo,
            },
            backend_epoch: 1,
            dynamic: true,
            queue_limit: provider_queue_limit_for_mode(decl.mode),
            max_in_flight: provider_max_in_flight_for_mode(decl.mode),
            in_flight: 0,
            calls: 0,
            failed_calls: 0,
            revokes: 0,
            async_submitted: 0,
            async_completed: 0,
            async_canceled: 0,
            async_expired: 0,
            async_rejected: 0,
        });
        self.record_mgr_audit(
            ELM_MGR_ACTION_PROVIDER_REGISTER,
            owner,
            0,
            state_code(self.cell_state(owner).unwrap_or(ElmState::Loaded)),
        );
        Ok(())
    }

    fn provider_index(&self, port: PortId) -> Option<usize> {
        self.providers
            .iter()
            .position(|provider| provider.port == port)
    }

    fn reserve_cell_execution(&mut self, id: ElmId) -> Result<CellExecutionToken, i32> {
        let Some(index) = self.cell_index(id) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if self.cells[index].isolated {
            return Err(ELM_MGR_STATUS_PERMISSION);
        }
        if self.cells[index].exclusive_execution && current_cell() != Some(id) {
            return Err(ELM_MGR_STATUS_BUSY);
        }
        let Some(active_executions) = self.cells[index].active_executions.checked_add(1) else {
            return Err(ELM_MGR_STATUS_BUSY);
        };
        let token = CellExecutionToken {
            cell: id,
            generation: self.cells[index].generation,
            policy_epoch: self.cells[index].policy_epoch,
            allowed_actions: self.cells[index].cell_policy.allowed_actions,
            exclusive: false,
        };
        self.cells[index].active_executions = active_executions;
        Ok(token)
    }

    fn reserve_cell_execution_exclusive(&mut self, id: ElmId) -> Result<CellExecutionToken, i32> {
        let Some(index) = self.cell_index(id) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if self.cells[index].isolated {
            return Err(ELM_MGR_STATUS_PERMISSION);
        }
        if self.cells[index].exclusive_execution || self.cells[index].active_executions != 0 {
            return Err(ELM_MGR_STATUS_BUSY);
        }
        self.cells[index].exclusive_execution = true;
        self.cells[index].active_executions = 1;
        Ok(CellExecutionToken {
            cell: id,
            generation: self.cells[index].generation,
            policy_epoch: self.cells[index].policy_epoch,
            allowed_actions: self.cells[index].cell_policy.allowed_actions,
            exclusive: true,
        })
    }

    fn release_cell_execution(&mut self, token: CellExecutionToken) {
        let Some(index) = self.cell_index(token.cell) else {
            log::warning!(
                "[elm] execution token lost cell={} generation={}",
                token.cell.0,
                token.generation.0
            );
            return;
        };
        if self.cells[index].active_executions == 0 {
            log::warning!(
                "[elm] execution reference underflow cell={} generation={}",
                token.cell.0,
                token.generation.0
            );
            return;
        }
        self.cells[index].active_executions -= 1;
        if token.exclusive {
            self.cells[index].exclusive_execution = false;
        }
    }

    fn cell_execution_is_current(&self, token: CellExecutionToken) -> bool {
        self.cells.iter().any(|cell| {
            cell.id == token.cell
                && cell.generation == token.generation
                && cell.policy_epoch == token.policy_epoch
                && !cell.isolated
                && (!token.exclusive || cell.exclusive_execution)
        })
    }

    fn stage_native_imports(
        &mut self,
        execution: CellExecutionToken,
        owner_generation: Generation,
        imports: Vec<NativeImportRuntime>,
    ) -> Result<NativeImportStageKey, ()> {
        let key = NativeImportStageKey {
            owner: execution.cell,
            owner_generation,
        };
        if !execution.exclusive
            || !self.cell_execution_is_current(execution)
            || self
                .staged_native_imports
                .iter()
                .any(|stage| stage.key == key)
            || imports.iter().any(|import| {
                import.owner != key.owner || import.owner_generation != key.owner_generation
            })
        {
            return Err(());
        }
        for import in imports
            .iter()
            .filter(|import| native_import_is_managed(import.flags))
        {
            if import.handle == 0
                || self
                    .native_imports
                    .iter()
                    .any(|active| active.handle == import.handle)
                || self.staged_native_imports.iter().any(|stage| {
                    stage
                        .imports
                        .iter()
                        .any(|staged| staged.handle == import.handle)
                })
            {
                return Err(());
            }
        }
        self.staged_native_imports.try_reserve(1).map_err(|_| ())?;
        self.staged_native_imports.push(StagedNativeImports {
            key,
            execution,
            imports,
        });
        Ok(key)
    }

    fn native_import_stage_is_current(&self, key: NativeImportStageKey) -> bool {
        self.staged_native_imports
            .iter()
            .find(|stage| stage.key == key)
            .is_some_and(|stage| self.native_import_stage_execution_is_current(stage.execution))
    }

    fn native_import_stage_execution_is_current(&self, execution: CellExecutionToken) -> bool {
        execution.exclusive
            && self.cells.iter().any(|cell| {
                cell.id == execution.cell
                    && cell.generation == execution.generation
                    && cell.policy_epoch == execution.policy_epoch
                    && cell.exclusive_execution
                    && cell.active_executions != 0
            })
    }

    fn reserve_native_import_stage_promotion(&mut self, key: NativeImportStageKey) -> bool {
        let Some(import_count) = self
            .staged_native_imports
            .iter()
            .find(|stage| stage.key == key)
            .map(|stage| stage.imports.len())
        else {
            return false;
        };
        self.native_imports.try_reserve(import_count).is_ok()
    }

    fn promote_native_import_stage(&mut self, key: NativeImportStageKey) -> bool {
        let Some(index) = self
            .staged_native_imports
            .iter()
            .position(|stage| stage.key == key)
        else {
            return false;
        };
        if !self
            .native_import_stage_execution_is_current(self.staged_native_imports[index].execution)
        {
            return false;
        }
        let mut stage = self.staged_native_imports.swap_remove(index);
        self.native_imports.append(&mut stage.imports);
        true
    }

    fn discard_native_import_stage(&mut self, key: NativeImportStageKey) -> usize {
        let Some(index) = self
            .staged_native_imports
            .iter()
            .position(|stage| stage.key == key)
        else {
            return 0;
        };
        self.staged_native_imports.swap_remove(index).imports.len()
    }

    fn staged_native_import(
        &self,
        caller: ElmId,
        caller_generation: Generation,
        import_handle: u64,
        phase: ElmLifecyclePhase,
    ) -> Result<
        (
            NativeImportRuntime,
            NativeImportStageKey,
            CellExecutionToken,
        ),
        i32,
    > {
        if !native_import_stage_phase_allowed(phase) {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        }
        let Some(stage) = self.staged_native_imports.iter().find(|stage| {
            stage
                .imports
                .iter()
                .any(|import| import.handle == import_handle)
        }) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if stage.key.owner != caller
            || stage.key.owner_generation != caller_generation
            || !stage.execution.exclusive
            || !self.native_import_stage_execution_is_current(stage.execution)
        {
            return Err(ELM_MGR_STATUS_PERMISSION);
        }
        let import = stage
            .imports
            .iter()
            .find(|import| import.handle == import_handle)
            .cloned()
            .ok_or(ELM_MGR_STATUS_NOT_FOUND)?;
        Ok((import, stage.key, stage.execution))
    }

    fn managed_caller_is_current(&self, caller: ManagedCallerReservation) -> bool {
        match caller {
            ManagedCallerReservation::Active(token) => self.cell_execution_is_current(token),
            ManagedCallerReservation::Staged { stage, execution } => {
                self.native_import_stage_execution_is_current(execution)
                    && self.staged_native_imports.iter().any(|candidate| {
                        candidate.key == stage && candidate.execution.cell == execution.cell
                    })
            }
        }
    }

    fn release_managed_caller(&mut self, caller: ManagedCallerReservation) {
        if let ManagedCallerReservation::Active(token) = caller {
            self.release_cell_execution(token);
        }
    }

    fn prepare_managed_call(
        &mut self,
        caller: ElmId,
        caller_generation: Generation,
        caller_phase: ElmLifecyclePhase,
        import_handle: u64,
        frame: ElmCallFrame,
    ) -> Result<ManagedCallExecutionPlan, i32> {
        if import_handle == 0
            || frame.flags != 0
            || frame.reserved0 != 0
            || frame.reserved1 != 0
            || usize::from(frame.payload_len) > frame.payload.len()
        {
            return Err(ELM_MGR_STATUS_INVALID);
        }
        let active_import = self
            .native_imports
            .iter()
            .find(|import| import.handle == import_handle)
            .cloned();
        let (import, staged_caller) = match active_import {
            Some(import) => {
                if import.owner != caller
                    || import.owner_generation != caller_generation
                    || !native_import_is_managed(import.flags)
                {
                    return Err(ELM_MGR_STATUS_PERMISSION);
                }
                (import, None)
            }
            None => {
                let (import, stage, execution) = self.staged_native_import(
                    caller,
                    caller_generation,
                    import_handle,
                    caller_phase,
                )?;
                if !native_import_is_managed(import.flags) {
                    return Err(ELM_MGR_STATUS_PERMISSION);
                }
                (import, Some((stage, execution)))
            }
        };
        let export = self
            .native_exports
            .iter()
            .find(|export| {
                export.owner == import.provider
                    && export.generation == import.provider_generation
                    && export.name == import.name
                    && export.contract == import.contract
                    && export.version == import.selected_version
                    && native_export_is_managed(export.flags)
            })
            .cloned()
            .ok_or(ELM_MGR_STATUS_NOT_FOUND)?;
        let bounds = export.bounds.ok_or(ELM_MGR_STATUS_INVALID)?;
        if self.cell_state(import.provider) != Some(ElmState::Active)
            || (staged_caller.is_none() && self.cell_state(caller) != Some(ElmState::Active))
        {
            return Err(ELM_MGR_STATUS_BUSY);
        }
        let caller_reservation = match staged_caller {
            Some((stage, execution)) => ManagedCallerReservation::Staged { stage, execution },
            None => {
                let token = self.reserve_cell_execution(caller)?;
                if token.generation != caller_generation {
                    self.release_cell_execution(token);
                    return Err(ELM_MGR_STATUS_BUSY);
                }
                ManagedCallerReservation::Active(token)
            }
        };
        let callee_token = match self.reserve_cell_execution(import.provider) {
            Ok(token) => token,
            Err(status) => {
                self.release_managed_caller(caller_reservation);
                return Err(status);
            }
        };
        if callee_token.generation != import.provider_generation {
            self.release_cell_execution(callee_token);
            self.release_managed_caller(caller_reservation);
            return Err(ELM_MGR_STATUS_BUSY);
        }
        Ok(ManagedCallExecutionPlan {
            caller: caller_reservation,
            callee: callee_token,
            import_handle,
            address: export.address,
            bounds,
            frame,
        })
    }

    fn complete_managed_call(
        &mut self,
        plan: ManagedCallExecutionPlan,
        reply: ElmReplyFrame,
    ) -> Result<ElmReplyFrame, i32> {
        let current = self.managed_caller_is_current(plan.caller)
            && self.cell_execution_is_current(plan.callee);
        self.release_cell_execution(plan.callee);
        self.release_managed_caller(plan.caller);
        if !current {
            return Err(ELM_MGR_STATUS_BUSY);
        }
        if reply.status == ELM_CALL_STATUS_PROVIDER_FAULT {
            self.mark_native_fault(plan.callee.cell, ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED);
        }
        Ok(reply)
    }

    fn reserve_provider_execution(
        &mut self,
        provider_index: usize,
        binding: Option<&elm_model::CapabilityBindingEdge>,
        lease: Option<LeaseId>,
        acquire_lease_ref: bool,
        validate_binding: bool,
        deadline_ns: u64,
    ) -> Result<ProviderExecutionReservation, i32> {
        let Some(provider) = self.providers.get(provider_index).cloned() else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        let Some(diagnostic_id) = take_monotonic_id(&mut self.next_provider_execution_id) else {
            return Err(ELM_MGR_STATUS_BUSY);
        };
        if self.active_provider_executions.try_reserve(1).is_err() {
            return Err(ELM_MGR_STATUS_BUSY);
        }
        let Some(next_in_flight) = provider.in_flight.checked_add(1) else {
            return Err(ELM_MGR_STATUS_BUSY);
        };
        if next_in_flight > provider.max_in_flight {
            return Err(ELM_MGR_STATUS_BUSY);
        }

        let mut cells = Vec::new();
        if let Some(owner) = provider.owner {
            cells.push(self.reserve_cell_execution(owner)?);
        }
        if let Some(edge) = binding
            && !cells.iter().any(|token| token.cell == edge.consumer)
        {
            match self.reserve_cell_execution(edge.consumer) {
                Ok(token) => cells.push(token),
                Err(status) => {
                    for token in cells.drain(..).rev() {
                        self.release_cell_execution(token);
                    }
                    return Err(status);
                }
            }
        }

        if acquire_lease_ref
            && let Some(lease) = lease
            && self.leases.add_active_ref(lease).is_err()
        {
            for token in cells.drain(..).rev() {
                self.release_cell_execution(token);
            }
            return Err(ELM_MGR_STATUS_BUSY);
        }

        self.providers[provider_index].in_flight = next_in_flight;
        self.active_provider_executions
            .push(ProviderExecutionDiagnostic {
                id: diagnostic_id,
                port: provider.port,
                binding: binding.map(|edge| edge.id),
                lease,
                provider_epoch: provider.backend_epoch,
                started_at_ns: sched::now_ns_public(),
                deadline_ns,
            });
        Ok(ProviderExecutionReservation {
            diagnostic_id,
            port: provider.port,
            provider_epoch: provider.backend_epoch,
            binding: binding.map(|edge| edge.id),
            validate_binding,
            lease,
            release_lease_ref: acquire_lease_ref && lease.is_some(),
            cells,
        })
    }

    fn provider_execution_is_current(&self, reservation: &ProviderExecutionReservation) -> bool {
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.port == reservation.port)
        else {
            return false;
        };
        if provider.backend_epoch != reservation.provider_epoch {
            return false;
        }
        if !self.active_provider_executions.iter().any(|active| {
            active.id == reservation.diagnostic_id
                && active.port == reservation.port
                && active.binding == reservation.binding
                && active.lease == reservation.lease
                && active.provider_epoch == reservation.provider_epoch
        }) {
            return false;
        }
        if let ProviderBackend::ElmNative(native) = provider.backend
            && !reservation
                .cells
                .iter()
                .any(|token| token.cell == native.owner && token.generation == native.generation)
        {
            return false;
        }
        for token in &reservation.cells {
            if !self.cell_execution_is_current(*token) {
                return false;
            }
        }
        if reservation.validate_binding
            && let Some(binding) = reservation.binding
        {
            let Some(edge) = self.graph.capability_binding(binding) else {
                return false;
            };
            if !edge.active
                || edge.port != reservation.port
                || edge.lease != reservation.lease
                || !reservation
                    .cells
                    .iter()
                    .any(|token| token.cell == edge.consumer && token.generation == edge.generation)
            {
                return false;
            }
        }
        if let Some(lease) = reservation.lease {
            let Some(runtime) = self.leases.get(lease) else {
                return false;
            };
            if runtime.binding != reservation.binding || runtime.active_refs == 0 {
                return false;
            }
        }
        true
    }

    fn finish_provider_execution(
        &mut self,
        reservation: ProviderExecutionReservation,
        release_provider_slot: bool,
    ) -> bool {
        let current = self.provider_execution_is_current(&reservation);
        let diagnostic_removed = self
            .active_provider_executions
            .iter()
            .position(|active| active.id == reservation.diagnostic_id)
            .map(|index| {
                self.active_provider_executions.swap_remove(index);
            })
            .is_some();
        if !diagnostic_removed {
            log::error!(
                "[elm] provider execution diagnostic missing id={} port={}",
                reservation.diagnostic_id,
                reservation.port.0
            );
        }
        let provider_slot_released = if release_provider_slot {
            self.provider_index(reservation.port)
                .is_some_and(|provider_index| self.release_provider_slot(provider_index))
        } else {
            true
        };
        if reservation.release_lease_ref
            && let Some(lease) = reservation.lease
            && let Err(err) = self.leases.release_active_ref(lease)
        {
            log::warning!(
                "[elm] provider execution lease release failed lease={} err={:?}",
                lease.0,
                err
            );
        }
        for token in reservation.cells.into_iter().rev() {
            self.release_cell_execution(token);
        }
        current && diagnostic_removed && provider_slot_released
    }

    fn release_provider_slot(&mut self, provider_index: usize) -> bool {
        let Some(provider) = self.providers.get_mut(provider_index) else {
            return false;
        };
        if provider.in_flight == 0 {
            log::error!(
                "[elm] provider in-flight reference underflow port={}",
                provider.port.0
            );
            return false;
        }
        provider.in_flight -= 1;
        true
    }

    fn prepare_provider_call_execution(
        &mut self,
        request: ElmProviderInvokeRequest,
    ) -> Result<PreparedProviderCall, i32> {
        let frame = request.frame;
        if usize::from(frame.payload_len) > frame.payload.len() {
            return Err(ELM_MGR_STATUS_INVALID);
        }
        let binding = BindingId(frame.binding_id);
        let Some(edge) = self.graph.capability_binding(binding).cloned() else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if !edge.active {
            return Err(ELM_MGR_STATUS_INVALID);
        }
        let Some(lease) = edge.lease else {
            return Err(ELM_MGR_STATUS_INVALID);
        };
        if self.leases.get(lease).is_none() {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        }
        if !self.cell_policy_allows(edge.consumer, ELM_CELL_POLICY_ALLOW_PROVIDER) {
            let blockers = ELM_POLICY_BLOCK_CAPABILITY_DENIED;
            self.record_mgr_audit(
                ELM_MGR_ACTION_PROVIDER_INVOKE,
                edge.consumer,
                blockers,
                self.cell_state(edge.consumer).map(state_code).unwrap_or(0),
            );
            self.push_provider_call_trace(
                edge.consumer,
                binding,
                edge.port,
                ELM_MGR_STATUS_PERMISSION,
                blockers,
            );
            return Err(ELM_MGR_STATUS_PERMISSION);
        }
        let Some(provider_index) = self.provider_index(edge.port) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        let Some(port) = self.port_desc(edge.port) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if !port.invokable {
            self.providers[provider_index].failed_calls = self.providers[provider_index]
                .failed_calls
                .saturating_add(1);
            return Err(ELM_MGR_STATUS_UNSUPPORTED);
        }
        if let Some(blockers) = self.provider_call_blocker(&edge, &self.providers[provider_index]) {
            self.providers[provider_index].failed_calls = self.providers[provider_index]
                .failed_calls
                .saturating_add(1);
            self.record_mgr_audit(
                ELM_MGR_ACTION_PROVIDER_INVOKE,
                edge.consumer,
                blockers,
                self.cell_state(edge.consumer).map(state_code).unwrap_or(0),
            );
            return Err(status_from_blockers(blockers));
        }

        let backend = self.providers[provider_index].backend;
        match backend {
            ProviderBackend::Kernel(kind) => {
                let result = self.invoke_kernel_provider(kind, &edge, frame);
                Ok(PreparedProviderCall::Immediate(
                    self.finish_provider_call_result(&edge, backend, result),
                ))
            }
            ProviderBackend::ElmNativeTodo => {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
                Ok(PreparedProviderCall::Immediate(Err(ELM_MGR_STATUS_TODO)))
            }
            ProviderBackend::KernelOps(_) | ProviderBackend::ElmNative(_) => {
                let reservation = match self.reserve_provider_execution(
                    provider_index,
                    Some(&edge),
                    Some(lease),
                    true,
                    true,
                    0,
                ) {
                    Ok(reservation) => reservation,
                    Err(status) => {
                        self.providers[provider_index].failed_calls = self.providers
                            [provider_index]
                            .failed_calls
                            .saturating_add(1);
                        return Err(status);
                    }
                };
                Ok(PreparedProviderCall::External(ProviderCallExecutionPlan {
                    reservation,
                    backend,
                    edge,
                    frame,
                    deadline_ns: 0,
                    reply_flags_mask: 0,
                }))
            }
        }
    }

    fn complete_provider_call_execution(
        &mut self,
        plan: ProviderCallExecutionPlan,
        result: Result<ElmReplyFrame, i32>,
    ) -> Result<ElmProviderInvokeResponse, i32> {
        let current = self.finish_provider_execution(plan.reservation, true);
        if !current {
            let blockers = ELM_POLICY_BLOCK_PROVIDER_BUSY;
            if let Some(provider_index) = self.provider_index(plan.edge.port) {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
            }
            self.record_mgr_audit(
                ELM_MGR_ACTION_PROVIDER_INVOKE,
                plan.edge.consumer,
                blockers,
                self.cell_state(plan.edge.consumer)
                    .map(state_code)
                    .unwrap_or(0),
            );
            self.push_provider_call_trace(
                plan.edge.consumer,
                plan.edge.id,
                plan.edge.port,
                ELM_MGR_STATUS_BUSY,
                blockers,
            );
            return Err(ELM_MGR_STATUS_BUSY);
        }
        self.finish_provider_call_result(&plan.edge, plan.backend, result)
    }

    fn finish_provider_call_result(
        &mut self,
        edge: &elm_model::CapabilityBindingEdge,
        backend: ProviderBackend,
        result: Result<ElmReplyFrame, i32>,
    ) -> Result<ElmProviderInvokeResponse, i32> {
        let Some(provider_index) = self.provider_index(edge.port) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        let reply = match result {
            Ok(reply) => reply,
            Err(status) => {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
                let blockers = provider_async_blocker_from_mgr_status(status);
                self.record_mgr_audit(
                    ELM_MGR_ACTION_PROVIDER_INVOKE,
                    edge.consumer,
                    blockers,
                    self.cell_state(edge.consumer).map(state_code).unwrap_or(0),
                );
                self.push_provider_call_trace(edge.consumer, edge.id, edge.port, status, blockers);
                return Err(status);
            }
        };
        if let ProviderBackend::ElmNative(native) = backend
            && reply.status == ELM_CALL_STATUS_PROVIDER_FAULT
        {
            self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED);
        }
        if reply.status == ELM_CALL_STATUS_OK {
            self.providers[provider_index].calls =
                self.providers[provider_index].calls.saturating_add(1);
        } else {
            self.providers[provider_index].failed_calls = self.providers[provider_index]
                .failed_calls
                .saturating_add(1);
        }
        let audit_blockers = provider_call_blockers(reply.status);
        self.record_mgr_audit(
            ELM_MGR_ACTION_PROVIDER_INVOKE,
            edge.consumer,
            audit_blockers,
            self.cell_state(edge.consumer).map(state_code).unwrap_or(0),
        );
        self.push_provider_call_trace(
            edge.consumer,
            edge.id,
            edge.port,
            reply.status,
            audit_blockers,
        );
        Ok(ElmProviderInvokeResponse::new(reply))
    }

    fn provider_binding_count(&self, port: PortId) -> usize {
        self.graph
            .capability_bindings()
            .iter()
            .filter(|edge| edge.active && edge.port == port)
            .count()
    }

    fn provider_queued_count(&self, port: PortId) -> usize {
        self.provider_jobs
            .iter()
            .filter(|job| job.port == port)
            .count()
    }

    fn provider_running_count(&self, port: PortId) -> usize {
        self.provider_running
            .iter()
            .filter(|running| running.job.port == port)
            .count()
    }

    fn provider_active_execution_count(&self, port: PortId) -> usize {
        self.active_provider_executions
            .iter()
            .filter(|active| active.port == port)
            .count()
    }

    fn provider_oldest_running_start_ns(&self, port: PortId) -> u64 {
        self.provider_running
            .iter()
            .filter(|running| running.job.port == port)
            .map(|running| running.started_at_ns)
            .min()
            .unwrap_or(0)
    }

    fn provider_in_flight_count(&self, provider: &ProviderRuntime) -> usize {
        self.provider_running_count(provider.port)
            .max(provider.in_flight as usize)
    }

    fn provider_retained_result_count(&self, port: PortId) -> usize {
        self.provider_results
            .iter()
            .filter(|result| result.port == port)
            .count()
    }

    fn prepare_provider_async_submit(
        &self,
        frame: ElmCallFrame,
    ) -> Result<(elm_model::CapabilityBindingEdge, usize, LeaseId), (i32, u64, Option<PortId>)>
    {
        let binding = BindingId(frame.binding_id);
        let Some(edge) = self.graph.capability_binding(binding).cloned() else {
            return Err((
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
                None,
            ));
        };
        if !edge.active {
            return Err((
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
                Some(edge.port),
            ));
        }
        let Some(lease) = edge.lease else {
            return Err((
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_INVALID_STATE,
                Some(edge.port),
            ));
        };
        if self.leases.get(lease).is_none() {
            return Err((
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_LEASE_BUSY,
                Some(edge.port),
            ));
        }
        let Some(provider_index) = self.provider_index(edge.port) else {
            return Err((
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
                Some(edge.port),
            ));
        };
        let Some(port) = self.port_desc(edge.port) else {
            return Err((
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_PORT_NOT_FOUND,
                Some(edge.port),
            ));
        };
        if !port.invokable {
            return Err((
                ELM_MGR_STATUS_UNSUPPORTED,
                ELM_POLICY_BLOCK_PORT_TODO,
                Some(edge.port),
            ));
        }
        if let Some(blockers) = self.provider_call_blocker(&edge, &self.providers[provider_index]) {
            return Err((status_from_blockers(blockers), blockers, Some(edge.port)));
        }
        if matches!(
            self.providers[provider_index].backend,
            ProviderBackend::ElmNativeTodo
        ) {
            return Err((
                ELM_MGR_STATUS_TODO,
                ELM_POLICY_BLOCK_NATIVE_TODO,
                Some(edge.port),
            ));
        }
        Ok((edge, provider_index, lease))
    }

    fn record_provider_async_rejection(&mut self, port: Option<PortId>) {
        if let Some(port) = port {
            if let Some(index) = self.provider_index(port) {
                self.providers[index].async_rejected =
                    self.providers[index].async_rejected.saturating_add(1);
            }
        }
    }

    fn next_runnable_provider_job_index(&self) -> Option<usize> {
        self.provider_jobs
            .iter()
            .enumerate()
            .find(|(_, job)| {
                self.provider_index(job.port)
                    .and_then(|index| self.providers.get(index))
                    .is_some_and(|provider| provider.in_flight < provider.max_in_flight)
            })
            .map(|(index, _)| index)
    }

    fn begin_provider_running_call(
        &mut self,
        job: ProviderAsyncJob,
        provider_index: usize,
        started_at_ns: u64,
    ) -> Result<(), ProviderAsyncJob> {
        if self.provider_running.try_reserve(1).is_err() {
            return Err(job);
        }
        let Some(provider) = self.providers.get(provider_index) else {
            return Err(job);
        };
        let Some(next_in_flight) = provider.in_flight.checked_add(1) else {
            return Err(job);
        };
        if next_in_flight > provider.max_in_flight {
            return Err(job);
        }
        self.providers[provider_index].in_flight = next_in_flight;
        self.provider_running.push(ProviderRunningCall {
            job,
            started_at_ns,
            cancel_requested: false,
        });
        Ok(())
    }

    fn execute_provider_async_job(
        &mut self,
        job: &ProviderAsyncJob,
    ) -> (ElmProviderAsyncState, i32, ElmReplyFrame, u64) {
        let Some(edge) = self
            .graph
            .capability_binding(BindingId(job.frame.binding_id))
            .cloned()
        else {
            return (
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_NOT_FOUND,
                ElmReplyFrame::empty(
                    job.frame.binding_id,
                    job.frame.call_id,
                    ELM_CALL_STATUS_NOT_FOUND,
                ),
                ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
            );
        };
        if !edge.active || edge.port != job.port || edge.consumer != job.consumer {
            return (
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_INVALID,
                ElmReplyFrame::empty(
                    job.frame.binding_id,
                    job.frame.call_id,
                    ELM_CALL_STATUS_INVALID,
                ),
                ELM_POLICY_BLOCK_INVALID_STATE,
            );
        }
        let Some(provider_index) = self.provider_index(job.port) else {
            return (
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_NOT_FOUND,
                ElmReplyFrame::empty(
                    job.frame.binding_id,
                    job.frame.call_id,
                    ELM_CALL_STATUS_NOT_FOUND,
                ),
                ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
            );
        };
        let backend = self.providers[provider_index].backend;
        let reply = match backend {
            ProviderBackend::Kernel(kind) => self.invoke_kernel_provider(kind, &edge, job.frame),
            ProviderBackend::ElmNativeTodo => Err(ELM_MGR_STATUS_TODO),
            ProviderBackend::KernelOps(_) | ProviderBackend::ElmNative(_) => {
                Err(ELM_MGR_STATUS_UNSUPPORTED)
            }
        };

        match reply {
            Ok(reply) if reply.status == ELM_CALL_STATUS_OK => {
                self.providers[provider_index].calls =
                    self.providers[provider_index].calls.saturating_add(1);
                (
                    ElmProviderAsyncState::Completed,
                    ELM_MGR_STATUS_OK,
                    reply,
                    0,
                )
            }
            Ok(reply) => {
                if let ProviderBackend::ElmNative(native) = backend
                    && reply.status == ELM_CALL_STATUS_PROVIDER_FAULT
                {
                    self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED);
                }
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
                (
                    ElmProviderAsyncState::Failed,
                    ELM_MGR_STATUS_INVALID,
                    reply,
                    provider_call_blockers(reply.status),
                )
            }
            Err(status) => {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
                let call_status = provider_async_call_status_from_mgr(status);
                (
                    ElmProviderAsyncState::Failed,
                    status,
                    ElmReplyFrame::empty(job.frame.binding_id, job.frame.call_id, call_status),
                    provider_async_blocker_from_mgr_status(status),
                )
            }
        }
    }

    fn finish_provider_running_call(
        &mut self,
        ticket: u64,
        state: ElmProviderAsyncState,
        status: i32,
        reply: ElmReplyFrame,
        blockers: u64,
        finish_ns: u64,
    ) -> bool {
        let Some(index) = self
            .provider_running
            .iter()
            .position(|running| running.job.ticket == ticket)
        else {
            return false;
        };
        let running = self.provider_running.remove(index);
        let slot_released = self
            .provider_index(running.job.port)
            .is_some_and(|provider_index| self.release_provider_slot(provider_index));

        let (state, status, reply, blockers) = if running.cancel_requested {
            (
                ElmProviderAsyncState::Canceled,
                ELM_MGR_STATUS_OK,
                ElmReplyFrame::empty(
                    running.job.frame.binding_id,
                    running.job.frame.call_id,
                    ELM_CALL_STATUS_BUSY,
                ),
                0,
            )
        } else if finish_ns >= running.job.deadline_ns {
            (
                ElmProviderAsyncState::Expired,
                ELM_MGR_STATUS_BUSY,
                ElmReplyFrame::empty(
                    running.job.frame.binding_id,
                    running.job.frame.call_id,
                    ELM_CALL_STATUS_BUSY,
                ),
                ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED,
            )
        } else {
            (state, status, reply, blockers)
        };

        if matches!(state, ElmProviderAsyncState::Expired)
            && let Some(provider_index) = self.provider_index(running.job.port)
            && let ProviderBackend::ElmNative(native) = self.providers[provider_index].backend
        {
            self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED);
        }
        self.finish_provider_async_job(running.job, state, status, reply, blockers, finish_ns);
        slot_released
    }

    fn finish_provider_async_job(
        &mut self,
        job: ProviderAsyncJob,
        state: ElmProviderAsyncState,
        status: i32,
        reply: ElmReplyFrame,
        blockers: u64,
        now_ns: u64,
    ) {
        if let Some(provider_index) = self.provider_index(job.port) {
            match state {
                ElmProviderAsyncState::Completed => {
                    self.providers[provider_index].async_completed = self.providers[provider_index]
                        .async_completed
                        .saturating_add(1);
                }
                ElmProviderAsyncState::Failed => {
                    self.providers[provider_index].async_completed = self.providers[provider_index]
                        .async_completed
                        .saturating_add(1);
                }
                ElmProviderAsyncState::Expired => {
                    self.providers[provider_index].async_expired = self.providers[provider_index]
                        .async_expired
                        .saturating_add(1);
                }
                ElmProviderAsyncState::Canceled => {
                    self.providers[provider_index].async_canceled = self.providers[provider_index]
                        .async_canceled
                        .saturating_add(1);
                }
                ElmProviderAsyncState::Queued | ElmProviderAsyncState::Running => {}
            }
        }
        let reply = if reply.binding_id == 0 && reply.call_id == 0 {
            ElmReplyFrame::empty(job.frame.binding_id, job.frame.call_id, reply.status)
        } else {
            reply
        };
        self.push_provider_result(ProviderAsyncResult {
            ticket: job.ticket,
            consumer: job.consumer,
            port: job.port,
            lease: job.lease,
            state,
            status,
            reply,
            blockers,
            expires_at_ns: now_ns.saturating_add(job.result_ttl_ns),
        });
        self.record_provider_async_audit(job.frame.binding_id, status, blockers);
    }

    fn push_provider_result(&mut self, result: ProviderAsyncResult) {
        while self.provider_results.len() >= PROVIDER_RESULT_RING_LIMIT {
            let Some(evicted) = self.provider_results.pop_front() else {
                break;
            };
            self.release_provider_result_lease(evicted.lease);
        }
        self.provider_results.push_back(result);
    }

    fn cleanup_provider_results_at(&mut self, now_ns: u64) -> usize {
        let mut removed = 0usize;
        let mut index = 0usize;
        while index < self.provider_results.len() {
            if self.provider_results[index].expires_at_ns > now_ns {
                index += 1;
                continue;
            }
            let Some(result) = self.provider_results.remove(index) else {
                break;
            };
            self.release_provider_result_lease(result.lease);
            removed += 1;
        }
        removed
    }

    fn release_provider_result_lease(&mut self, lease: LeaseId) {
        if let Err(err) = self.leases.release_active_ref(lease) {
            log::warning!(
                "[elm] provider async lease release failed lease={} err={:?}",
                lease.0,
                err
            );
        }
    }

    fn record_provider_async_audit(&mut self, binding_id: u64, status: i32, blockers: u64) {
        let cell = self
            .graph
            .capability_binding(BindingId(binding_id))
            .map(|edge| edge.consumer)
            .unwrap_or(ElmId(0));
        self.record_audit(
            ELM_MGR_ACTION_PROVIDER_ASYNC,
            cell,
            status,
            blockers,
            self.cell_state(cell).map(state_code).unwrap_or(0),
        );
    }

    fn provider_busy_owned_by(&self, owner: ElmId) -> usize {
        self.providers
            .iter()
            .filter(|provider| provider.owner == Some(owner))
            .map(|provider| {
                self.provider_binding_count(provider.port)
                    .saturating_add(self.provider_queued_count(provider.port))
                    .saturating_add(self.provider_retained_result_count(provider.port))
                    .saturating_add(self.provider_in_flight_count(provider))
            })
            .sum()
    }

    fn provider_runtime_busy_owned_by(&self, owner: ElmId) -> usize {
        self.providers
            .iter()
            .filter(|provider| provider.owner == Some(owner))
            .map(|provider| {
                self.provider_queued_count(provider.port)
                    .saturating_add(self.provider_retained_result_count(provider.port))
                    .saturating_add(self.provider_in_flight_count(provider))
            })
            .sum()
    }

    fn native_export_importer_count(&self, owner: ElmId) -> usize {
        self.native_imports
            .iter()
            .filter(|import| import.provider == owner && import.owner != owner)
            .count()
    }

    fn native_direct_pinned_importer_count(&self, owner: ElmId) -> usize {
        self.native_imports
            .iter()
            .filter(|import| {
                import.provider == owner
                    && import.owner != owner
                    && !native_import_is_managed(import.flags)
            })
            .count()
    }

    fn provider_call_blocker(
        &self,
        edge: &elm_model::CapabilityBindingEdge,
        provider: &ProviderRuntime,
    ) -> Option<u64> {
        if !matches!(self.cell_state(edge.consumer), Some(ElmState::Active)) {
            return Some(ELM_POLICY_BLOCK_INVALID_STATE);
        }
        if self.cell_is_isolated(edge.consumer) {
            return Some(ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED);
        }
        if let Some(owner) = provider.owner
            && !matches!(self.cell_state(owner), Some(ElmState::Active))
        {
            return Some(ELM_POLICY_BLOCK_PROVIDER_BUSY);
        }
        if let Some(owner) = provider.owner
            && self.cell_is_isolated(owner)
        {
            return Some(ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED);
        }
        if let ProviderBackend::ElmNative(native) = provider.backend
            && self
                .cells
                .iter()
                .find(|cell| cell.id == native.owner)
                .is_none_or(|cell| cell.generation != native.generation)
        {
            return Some(ELM_POLICY_BLOCK_PROVIDER_BUSY);
        }
        if let Some(owner) = provider.owner
            && self
                .cells
                .iter()
                .find(|cell| cell.id == owner)
                .is_some_and(|cell| cell.exclusive_execution)
            && current_cell() != Some(owner)
        {
            return Some(ELM_POLICY_BLOCK_PROVIDER_BUSY);
        }
        None
    }

    fn remove_dynamic_providers_owned_by(&mut self, owner: ElmId) -> usize {
        let mut removed = 0usize;
        let mut index = 0usize;
        while index < self.providers.len() {
            let port = self.providers[index].port;
            let removable = self.providers[index].dynamic
                && self.providers[index].owner == Some(owner)
                && self.provider_binding_count(port) == 0
                && self.provider_queued_count(port) == 0
                && self.provider_retained_result_count(port) == 0
                && self.provider_in_flight_count(&self.providers[index]) == 0;
            if !removable {
                index += 1;
                continue;
            }
            self.providers.remove(index);
            self.ports.retain(|runtime| runtime.id != port);
            self.record_mgr_audit(
                ELM_MGR_ACTION_PROVIDER_UNREGISTER,
                owner,
                0,
                self.cell_state(owner).map(state_code).unwrap_or(0),
            );
            removed += 1;
        }
        removed
    }

    fn remove_native_exports_owned_by(&mut self, owner: ElmId) -> usize {
        let before = self.native_exports.len();
        self.native_exports.retain(|export| export.owner != owner);
        before.saturating_sub(self.native_exports.len())
    }

    fn remove_native_imports_owned_by(&mut self, owner: ElmId) -> usize {
        let before = self.native_imports.len();
        self.native_imports.retain(|import| import.owner != owner);
        before.saturating_sub(self.native_imports.len())
    }

    fn rebind_native_importers_for_replace(
        &mut self,
        owner: ElmId,
        new_exports: &[NativeExportRuntime],
    ) -> bool {
        let Some(plans) = self.plan_native_import_rebinds(owner, new_exports) else {
            return false;
        };
        for plan in plans {
            if let Some(import) = self.native_imports.get_mut(plan.runtime_index) {
                debug_assert_eq!(import.address, plan.old_address);
                debug_assert_eq!(import.selected_version, plan.old_version);
                debug_assert_eq!(import.provider_generation, plan.old_provider_generation);
                import.address = plan.new_address;
                import.selected_version = plan.new_version;
                import.provider_generation = plan.new_provider_generation;
            }
        }
        true
    }

    fn can_rebind_native_importers_for_replace(
        &self,
        owner: ElmId,
        new_exports: &[NativeExportRuntime],
    ) -> bool {
        self.plan_native_import_rebinds(owner, new_exports)
            .is_some()
    }

    fn plan_native_import_rebinds(
        &self,
        owner: ElmId,
        new_exports: &[NativeExportRuntime],
    ) -> Option<Vec<NativeImportRebindPlan>> {
        let expected = self
            .native_imports
            .iter()
            .filter(|import| import.provider == owner && import.owner != owner)
            .count();
        let mut plans = Vec::new();
        plans.try_reserve_exact(expected).ok()?;
        for (runtime_index, import) in self.native_imports.iter().enumerate() {
            if import.provider != owner || import.owner == owner {
                continue;
            }
            if !native_import_is_managed(import.flags) {
                return None;
            }
            let export = select_managed_export_for_import(owner, import, new_exports)?;
            if export.bounds.is_none() {
                return None;
            }
            plans.push(NativeImportRebindPlan {
                runtime_index,
                old_version: import.selected_version,
                new_version: export.version,
                old_address: import.address,
                new_address: export.address,
                old_provider_generation: import.provider_generation,
                new_provider_generation: export.generation,
            });
        }
        Some(plans)
    }

    fn rollback_activated_cell_to_quarantine(&mut self, id: ElmId) {
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .unwrap_or(Generation(0));
        if self.leases.busy_owned_by(id) != 0
            || self.provider_busy_owned_by(id) != 0
            || super::source::owner_generation_busy(id, generation)
        {
            log::error!(
                "[elm] 激活回滚被运行中资源阻断 cell={} generation={}",
                id.0,
                generation.0
            );
            self.quarantine_cell_after_hook_failure(id);
            return;
        }

        let mut clean = true;
        if let Err(err) = self.leases.revoke_and_remove_owned_by(id) {
            log::error!("[elm] 激活回滚无法撤销租约 cell={}: {:?}", id.0, err);
            clean = false;
        }
        let removed_bindings = self.take_owned_bindings(id);
        for binding in removed_bindings {
            if let Some(edge) = self.graph.capability_binding(binding).cloned() {
                self.note_provider_revoke(&edge);
            }
            self.remove_runtime_binding(binding);
            if let Err(err) = self.graph.remove_capability_binding(binding) {
                log::error!(
                    "[elm] 激活回滚无法删除绑定 cell={} binding={}: {:?}",
                    id.0,
                    binding.0,
                    err
                );
                clean = false;
            }
            self.emit_binding(TopologyEventKind::BindingRemoved, binding);
        }
        let expected_menu_items = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.owned_menu_items.len())
            .unwrap_or(0);
        if self.remove_menu_items_owned_by(id) != expected_menu_items {
            log::error!("[elm] 激活回滚未能删除全部菜单项 cell={}", id.0);
            clean = false;
        }
        let expected_providers = self
            .providers
            .iter()
            .filter(|provider| provider.dynamic && provider.owner == Some(id))
            .count();
        if self.remove_dynamic_providers_owned_by(id) != expected_providers {
            log::error!("[elm] 激活回滚未能删除全部 provider cell={}", id.0);
            clean = false;
        }
        self.mgr_runtime.remove_event_subscriptions_owned_by(id);
        self.remove_native_exports_owned_by(id);
        self.remove_native_imports_owned_by(id);
        self.discard_pending_ebi_load(id);
        if let Err(err) = self.graph.remove_cell_relations(id) {
            log::error!("[elm] 激活回滚无法删除拓扑关系 cell={}: {:?}", id.0, err);
            clean = false;
        }
        if generation.0 != 0
            && let Err(err) = super::source::retire_projection_sources_owned_by(id, generation)
        {
            log::error!(
                "[elm] 激活回滚无法退役 Projection Source cell={} generation={}: {:?}",
                id.0,
                generation.0,
                err
            );
            clean = false;
        }
        if !clean {
            self.mark_native_fault(id, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
        }
        self.quarantine_cell_after_hook_failure(id);
    }

    fn resume_projection_sources_for_cell(&mut self, id: ElmId, generation: Generation) -> bool {
        match super::source::resume_projection_sources(id, generation) {
            Ok(_) => true,
            Err(err) => {
                log::error!(
                    "[elm] Projection Source 恢复失败 cell={} generation={}: {:?}",
                    id.0,
                    generation.0,
                    err
                );
                self.mark_native_fault(id, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
                false
            }
        }
    }

    fn rollback_projection_source_replace(
        &mut self,
        id: ElmId,
        old_generation: Generation,
        new_generation: Generation,
    ) -> bool {
        let mut clean = true;
        if let Err(err) = super::source::retire_projection_sources_owned_by(id, new_generation) {
            log::error!(
                "[elm] 替换回滚无法退役新 Projection Source cell={} generation={}: {:?}",
                id.0,
                new_generation.0,
                err
            );
            clean = false;
        }
        if !self.resume_projection_sources_for_cell(id, old_generation) {
            clean = false;
        }
        clean
    }

    fn commit_replaced_cell(
        &mut self,
        id: ElmId,
        final_state: ElmState,
        generation: Generation,
        unit: &ElmEbiUnit,
        loaded: &LoadedElmImage,
        exports: Vec<NativeExportRuntime>,
        import_stage: NativeImportStageKey,
    ) -> bool {
        if !self.replace_commit_capacity_available(id, unit) {
            return false;
        }
        let Some(cell_index) = self.cell_index(id) else {
            return false;
        };
        let old_generation = self.cells[cell_index].generation;
        let elmapi_version = self.select_elmapi_version(unit).ok().flatten().unwrap_or(0);
        if !super::owned_resource::replace_owner_generation(id, old_generation, generation) {
            return false;
        }
        if !self.rebind_native_importers_for_replace(id, &exports) {
            let _ = super::owned_resource::replace_owner_generation(id, generation, old_generation);
            return false;
        }
        self.remove_native_exports_owned_by(id);
        self.remove_native_imports_owned_by(id);
        self.native_exports.extend(exports);
        if !self.promote_native_import_stage(import_stage) {
            log::error!(
                "[elm] 替换提交时原生 import 暂存事务丢失 cell={} generation={}",
                id.0,
                generation.0
            );
            let _ = super::owned_resource::replace_owner_generation(id, generation, old_generation);
            return false;
        }
        self.replace_dynamic_provider_backends(id, generation, unit, loaded);
        self.replace_menu_metadata(id, unit);
        self.rewrite_owned_generation(id, generation);
        if let Some(cell) = self.cells.get_mut(cell_index) {
            cell.state = final_state;
            cell.generation = generation;
            cell.cell_policy.generation = generation.0;
            cell.policy_epoch += 1;
            cell.cell_policy.policy_epoch = cell.policy_epoch;
            cell.elmapi_version = elmapi_version;
            cell.ebi_arch = unit.target.arch;
            cell.ebi_status = ElmEbiLoadStatus::Ok;
            cell.has_native_code = unit.has_native_code();
            cell.native_segment_count = unit.segments.len() as u16;
            cell.native_import_count = unit.imports.len() as u16;
            cell.native_export_count = unit.exports.len() as u16;
            cell.lifecycle_hooks_declared = unit.lifecycle_hooks.is_some();
            cell.lifecycle_executor_ready = true;
            cell.lifecycle_initialized = true;
            cell.lifecycle_finalized = false;
        } else {
            let _ = super::owned_resource::replace_owner_generation(id, generation, old_generation);
            return false;
        }
        self.emit(TopologyEventKind::CellStateChanged, Some(id));
        true
    }

    fn commit_replaced_declarative_cell(
        &mut self,
        id: ElmId,
        final_state: ElmState,
        generation: Generation,
        unit: &ElmEbiUnit,
    ) -> bool {
        if !self.replace_commit_capacity_available(id, unit) {
            return false;
        }
        let Some(cell_index) = self.cell_index(id) else {
            return false;
        };
        let old_generation = self.cells[cell_index].generation;
        if !super::owned_resource::replace_owner_generation(id, old_generation, generation) {
            return false;
        }
        let elmapi_version = self.select_elmapi_version(unit).ok().flatten().unwrap_or(0);
        self.replace_declarative_provider_metadata(id, unit);
        self.replace_menu_metadata(id, unit);
        self.rewrite_owned_generation(id, generation);
        if let Some(cell) = self.cells.get_mut(cell_index) {
            cell.state = final_state;
            cell.generation = generation;
            cell.cell_policy.generation = generation.0;
            cell.policy_epoch += 1;
            cell.cell_policy.policy_epoch = cell.policy_epoch;
            cell.elmapi_version = elmapi_version;
            cell.ebi_arch = unit.target.arch;
            cell.ebi_status = ElmEbiLoadStatus::Ok;
            cell.has_native_code = false;
            cell.native_segment_count = unit.segments.len() as u16;
            cell.native_import_count = 0;
            cell.native_export_count = 0;
            cell.lifecycle_hooks_declared = unit.lifecycle_hooks.is_some();
            cell.lifecycle_executor_ready = true;
            cell.lifecycle_initialized = true;
            cell.lifecycle_finalized = false;
        } else {
            let _ = super::owned_resource::replace_owner_generation(id, generation, old_generation);
            return false;
        }
        self.emit(TopologyEventKind::CellStateChanged, Some(id));
        true
    }

    fn replace_commit_capacity_available(&self, id: ElmId, unit: &ElmEbiUnit) -> bool {
        let Some(cell) = self.cells.iter().find(|cell| cell.id == id) else {
            return false;
        };
        if cell.policy_epoch.checked_add(1).is_none() {
            return false;
        }
        if unit.menu.is_some()
            && (!self.menu_items.iter().any(|item| item.owner == id)
                || self.menu_generation.checked_next().is_none())
        {
            return false;
        }
        self.providers
            .iter()
            .filter(|provider| provider.owner == Some(id))
            .all(|provider| provider.backend_epoch.checked_add(1).is_some())
    }

    fn prepare_replace_commit_capacity(&mut self, id: ElmId, unit: &ElmEbiUnit) -> bool {
        if !self.replace_commit_capacity_available(id, unit) {
            return false;
        }
        let Some(menu) = &unit.menu else {
            return true;
        };
        let Some(item) = self.menu_items.iter_mut().find(|item| item.owner == id) else {
            return false;
        };
        item.label
            .try_reserve(menu.label.len().saturating_sub(item.label.len()))
            .is_ok()
            && item
                .description
                .try_reserve(
                    menu.description
                        .len()
                        .saturating_sub(item.description.len()),
                )
                .is_ok()
            && item
                .route
                .try_reserve(menu.route.len().saturating_sub(item.route.len()))
                .is_ok()
    }

    fn replace_response(
        &mut self,
        id: ElmId,
        status: i32,
        final_state: ElmState,
        generation: Generation,
        migrated_len: u32,
        reason: u32,
        blockers: u64,
    ) -> ElmReplaceCellResponseV1 {
        self.record_audit(
            ElmLifecycleAction::Replace as u32,
            id,
            status,
            blockers,
            state_code(final_state),
        );
        self.push_replace_trace(id, generation, status, migrated_len, blockers);
        ElmReplaceCellResponseV1::new(
            id.0,
            status,
            state_code(final_state),
            generation.0,
            migrated_len,
            reason,
            blockers,
        )
    }

    fn replace_dynamic_provider_backends(
        &mut self,
        owner: ElmId,
        generation: Generation,
        unit: &ElmEbiUnit,
        loaded: &LoadedElmImage,
    ) {
        let Ok(bounds) = loaded.execution_bounds() else {
            log::error!("[elm] 替换后的原生镜像缺少有效执行边界 cell={}", owner.0);
            return;
        };
        for decl in &unit.provider_ports {
            let Some(port_id) = self
                .ports
                .iter()
                .find(|port| port.owner == Some(owner) && port.contract() == decl.contract.as_str())
                .map(|port| port.id)
            else {
                continue;
            };
            let handler = loaded.provider_handler_for_decl(decl).ok().flatten();
            let snapshot = loaded.provider_snapshot_for_decl(decl).ok().flatten();
            if let Some(port) = self.ports.iter_mut().find(|port| port.id == port_id) {
                port.access = decl.access;
                port.direction = decl.direction;
                port.mode = decl.mode;
                port.invokable = handler.is_some();
                port.implemented = handler.is_some();
            }
            if let Some(provider) = self
                .providers
                .iter_mut()
                .find(|provider| provider.port == port_id)
            {
                provider.access = decl.access;
                provider.queue_limit = provider_queue_limit_for_mode(decl.mode);
                provider.max_in_flight = provider_max_in_flight_for_mode(decl.mode);
                provider.backend = match handler {
                    Some(handler) => ProviderBackend::ElmNative(NativeProviderBackend {
                        owner,
                        generation,
                        handler,
                        snapshot,
                        bounds,
                    }),
                    None => ProviderBackend::ElmNativeTodo,
                };
                provider.backend_epoch += 1;
            }
        }
    }

    fn replace_declarative_provider_metadata(&mut self, owner: ElmId, unit: &ElmEbiUnit) {
        for decl in &unit.provider_ports {
            let Some(port_id) = self
                .ports
                .iter()
                .find(|port| port.owner == Some(owner) && port.contract() == decl.contract.as_str())
                .map(|port| port.id)
            else {
                continue;
            };
            if let Some(port) = self.ports.iter_mut().find(|port| port.id == port_id) {
                port.access = decl.access;
                port.direction = decl.direction;
                port.mode = decl.mode;
                port.invokable = false;
                port.implemented = false;
            }
            if let Some(provider) = self
                .providers
                .iter_mut()
                .find(|provider| provider.port == port_id)
            {
                provider.access = decl.access;
                provider.queue_limit = provider_queue_limit_for_mode(decl.mode);
                provider.max_in_flight = provider_max_in_flight_for_mode(decl.mode);
                provider.backend = ProviderBackend::ElmNativeTodo;
                provider.backend_epoch += 1;
            }
        }
    }

    fn replace_menu_metadata(&mut self, owner: ElmId, unit: &ElmEbiUnit) {
        let Some(menu) = &unit.menu else {
            return;
        };
        let Some(next_menu_generation) = self.menu_generation.checked_next() else {
            log::error!(
                "[elm] replace commit reached exhausted menu generation owner={}",
                owner.0
            );
            return;
        };
        if let Some(item) = self.menu_items.iter_mut().find(|item| item.owner == owner) {
            item.kind = menu.kind;
            item.flags = menu.flags | ELM_MENU_FLAG_TODO;
            item.label.clear();
            item.label.push_str(&menu.label);
            item.description.clear();
            item.description.push_str(&menu.description);
            item.route.clear();
            item.route.push_str(&menu.route);
            self.menu_generation = next_menu_generation;
            self.emit(TopologyEventKind::MenuItemAdded, Some(owner));
        }
    }

    fn rewrite_owned_generation(&mut self, owner: ElmId, generation: Generation) {
        for edge in self.graph.capability_bindings_mut_for_cell(owner) {
            edge.generation = generation;
        }
        let leases = self.leases.iter_mut().filter(|lease| lease.owner == owner);
        for lease in leases {
            lease.generation = generation;
        }
    }

    fn provider_access_allowed(&self, consumer: ElmId, desc: &PortRuntime) -> bool {
        match desc.access {
            ElmPortAccessPolicy::Public => true,
            ElmPortAccessPolicy::Internal => desc.owner == Some(consumer),
            ElmPortAccessPolicy::ExtensionOnly => {
                let Some(owner) = desc.owner else {
                    return false;
                };
                owner == consumer
                    || self
                        .graph
                        .extensions()
                        .iter()
                        .any(|edge| edge.extension == consumer && edge.target == owner)
            }
        }
    }

    fn expected_lease_kind_for_port(&self, port: PortId) -> Option<LeaseKind> {
        match port {
            ELM_MGR_MENU_PORT_ID => Some(LeaseKind::MenuItem),
            ELM_CORE_LOG_PORT_ID | ELM_CORE_EVENT_PORT_ID => Some(LeaseKind::RuntimePort),
            _ if self.provider_index(port).is_some() => Some(LeaseKind::Provider),
            _ => None,
        }
    }

    fn runtime_port_index(&self, binding: BindingId) -> Option<usize> {
        self.runtime_ports
            .iter()
            .position(|runtime| runtime.binding == binding)
    }

    fn validate_runtime_port(&self, index: usize, expected_port: PortId) -> Result<(), i32> {
        let Some(runtime) = self.runtime_ports.get(index) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if runtime.port != expected_port {
            return Err(ELM_MGR_STATUS_INVALID);
        }
        let Some(edge) = self.graph.capability_binding(runtime.binding) else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if !edge.active
            || edge.consumer != runtime.cell
            || edge.port != runtime.port
            || edge.lease != Some(runtime.lease)
        {
            return Err(ELM_MGR_STATUS_INVALID);
        }
        if self.leases.get(runtime.lease).is_none() {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        }
        Ok(())
    }

    fn recover_stale_runtime_cursor(
        &mut self,
        index: usize,
        cursor: &mut u64,
        update_saved_cursor: bool,
    ) -> u64 {
        let Some(first) = self.events.first() else {
            return 0;
        };
        let next_requested = cursor.saturating_add(1);
        if next_requested >= first.sequence {
            return 0;
        }
        let dropped = first.sequence - next_requested;
        *cursor = first.sequence.saturating_sub(1);
        self.runtime_ports[index].dropped_events = self.runtime_ports[index]
            .dropped_events
            .saturating_add(dropped);
        if update_saved_cursor {
            self.runtime_ports[index].cursor = *cursor;
        }
        dropped
    }

    fn record_runtime_audit(
        &mut self,
        action: u32,
        index: Option<usize>,
        status: i32,
        blockers: u64,
    ) {
        let cell = index
            .and_then(|index| self.runtime_ports.get(index))
            .map(|runtime| runtime.cell)
            .unwrap_or(ElmId(0));
        let final_state = self.cell_state(cell).map(state_code).unwrap_or(0);
        self.record_audit(action, cell, status, blockers, final_state);
    }

    fn remove_runtime_binding(&mut self, binding: BindingId) {
        self.runtime_ports
            .retain(|runtime| runtime.binding != binding);
    }

    fn note_provider_revoke(&mut self, edge: &elm_model::CapabilityBindingEdge) {
        if let Some(index) = self.provider_index(edge.port) {
            let on_revoke = match self.providers[index].backend {
                ProviderBackend::KernelOps(spec) => spec.on_revoke,
                _ => None,
            };
            self.providers[index].revokes = self.providers[index].revokes.saturating_add(1);
            if let Some(on_revoke) = on_revoke {
                self.provider_revoke_notifications
                    .push_back(ProviderRevokeNotification {
                        callback: on_revoke,
                        binding: Some(edge.id),
                        lease: edge.lease,
                    });
                super::executor::wake_provider_worker();
            }
        }
    }

    fn bind_failure_response(
        &mut self,
        request: ElmNexusBindRequest,
        blockers: u64,
    ) -> ElmNexusBindPlanResponse {
        let id = ElmId(request.cell_id);
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation.0)
            .unwrap_or(0);
        self.record_mgr_audit(
            ELM_MGR_ACTION_BIND,
            id,
            blockers,
            self.cell_state(id).map(state_code).unwrap_or(0),
        );
        ElmNexusBindPlanResponse::new(
            request.cell_id,
            request.port_id,
            0,
            0,
            generation,
            false,
            status_from_blockers(blockers),
            blockers,
        )
    }

    fn attach_menu_binding(
        &mut self,
        id: ElmId,
        port: PortId,
        contract: FlowContract,
        binding: BindingId,
        lease: LeaseId,
    ) -> Result<(), ElmError> {
        let name = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.name.clone())
            .ok_or(ElmError::CellNotFound)?;
        let label = format!("ELM 单元 {}", name);
        let description = "通过枢纽连接层绑定生成的菜单项".to_string();
        let route = format!("elm/cell/{}/action", id.0);
        self.attach_menu_binding_with_menu(
            id,
            port,
            contract,
            binding,
            lease,
            ElmMenuItemKind::Action,
            ELM_MENU_FLAG_TODO | ELM_MENU_FLAG_REQUIRES_SYS_ADMIN,
            &label,
            &description,
            &route,
        )
    }

    fn attach_menu_binding_with_menu(
        &mut self,
        id: ElmId,
        port: PortId,
        contract: FlowContract,
        binding: BindingId,
        lease: LeaseId,
        kind: ElmMenuItemKind,
        flags: u32,
        label: &str,
        description: &str,
        route: &str,
    ) -> Result<(), ElmError> {
        let cell_index = self.cell_index(id).ok_or(ElmError::CellNotFound)?;
        let generation = self.cells[cell_index].generation;
        let next_menu_generation = self
            .menu_generation
            .checked_next()
            .ok_or(ElmError::LeaseBusy)?;
        self.menu_items
            .try_reserve(1)
            .map_err(|_| ElmError::LeaseBusy)?;
        self.cells[cell_index]
            .owned_bindings
            .try_reserve(1)
            .map_err(|_| ElmError::LeaseBusy)?;
        self.cells[cell_index]
            .owned_menu_items
            .try_reserve(1)
            .map_err(|_| ElmError::LeaseBusy)?;
        let action = self.alloc_action_id().ok_or(ElmError::LeaseBusy)?;
        let menu_id = self.alloc_menu_item_id().ok_or(ElmError::LeaseBusy)?;
        let menu_item = MenuItemRuntime::new(
            menu_id,
            id,
            action,
            kind,
            flags | ELM_MENU_FLAG_TODO,
            label,
            description,
            route,
        );
        self.graph
            .add_capability_binding(binding, id, port, contract, generation, Some(lease))?;
        if let Err(err) = self.leases.insert(
            ResourceLease::new(
                lease,
                id,
                LeaseKind::MenuItem,
                LeaseRights::CONTROL,
                generation,
            )
            .with_binding(binding),
        ) {
            let _ = self.graph.remove_capability_binding(binding);
            return Err(err);
        }
        self.emit_binding(TopologyEventKind::BindingAdded, binding);
        self.emit_lease(TopologyEventKind::LeaseAdded, lease);

        self.menu_items.push(menu_item);
        let cell = &mut self.cells[cell_index];
        cell.owned_bindings.push(binding);
        cell.owned_menu_items.push(menu_id);
        self.menu_generation = next_menu_generation;
        self.emit(TopologyEventKind::MenuItemAdded, Some(id));
        Ok(())
    }

    fn attach_runtime_port_binding(
        &mut self,
        id: ElmId,
        port: PortId,
        contract: FlowContract,
        binding: BindingId,
        lease: LeaseId,
        lease_kind: LeaseKind,
        rights: LeaseRights,
    ) -> Result<(), ElmError> {
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .ok_or(ElmError::CellNotFound)?;
        self.graph
            .add_capability_binding(binding, id, port, contract, generation, Some(lease))?;
        if let Err(err) = self.leases.insert(
            ResourceLease::new(lease, id, lease_kind, rights, generation).with_binding(binding),
        ) {
            let _ = self.graph.remove_capability_binding(binding);
            return Err(err);
        }
        let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) else {
            let _ = self.leases.revoke_and_remove(lease);
            let _ = self.graph.remove_capability_binding(binding);
            return Err(ElmError::CellNotFound);
        };
        cell.owned_bindings.push(binding);
        self.runtime_ports.push(RuntimePortBinding {
            binding,
            cell: id,
            port,
            lease,
            cursor: self.last_event_sequence(),
            submitted_logs: 0,
            delivered_events: 0,
            dropped_events: 0,
        });
        self.emit_binding(TopologyEventKind::BindingAdded, binding);
        self.emit_lease(TopologyEventKind::LeaseAdded, lease);
        Ok(())
    }

    fn attach_provider_binding(
        &mut self,
        id: ElmId,
        port: PortId,
        contract: FlowContract,
        binding: BindingId,
        lease: LeaseId,
    ) -> Result<(), ElmError> {
        let generation = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
            .ok_or(ElmError::CellNotFound)?;
        self.graph
            .add_capability_binding(binding, id, port, contract, generation, Some(lease))?;
        if let Err(err) = self.leases.insert(
            ResourceLease::new(
                lease,
                id,
                LeaseKind::Provider,
                LeaseRights::CONTROL,
                generation,
            )
            .with_binding(binding),
        ) {
            let _ = self.graph.remove_capability_binding(binding);
            return Err(err);
        }
        let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) else {
            let _ = self.leases.revoke_and_remove(lease);
            let _ = self.graph.remove_capability_binding(binding);
            return Err(ElmError::CellNotFound);
        };
        cell.owned_bindings.push(binding);
        self.emit_binding(TopologyEventKind::BindingAdded, binding);
        self.emit_lease(TopologyEventKind::LeaseAdded, lease);
        Ok(())
    }

    fn activate_bound_cell(&mut self, id: ElmId) -> Result<(), ElmError> {
        match self.cell_state(id).ok_or(ElmError::CellNotFound)? {
            ElmState::Loaded => {
                self.transition_cell_state(id, ElmState::Linked)?;
                self.transition_cell_state(id, ElmState::Ready)?;
                self.transition_cell_state(id, ElmState::Active)
            }
            ElmState::Linked => {
                self.transition_cell_state(id, ElmState::Ready)?;
                self.transition_cell_state(id, ElmState::Active)
            }
            ElmState::Ready => self.transition_cell_state(id, ElmState::Active),
            ElmState::Active => Ok(()),
            _ => Err(ElmError::InvalidTransition),
        }
    }

    fn transition_cell_state(&mut self, id: ElmId, to: ElmState) -> Result<(), ElmError> {
        let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) else {
            return Err(ElmError::CellNotFound);
        };
        cell.state.transition_to(to)?;
        cell.state = to;
        self.emit(TopologyEventKind::CellStateChanged, Some(id));
        Ok(())
    }

    fn cell_state(&self, id: ElmId) -> Option<ElmState> {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.state)
    }

    fn cell_exists(&self, id: ElmId) -> bool {
        self.cells.iter().any(|cell| cell.id == id)
    }

    fn cell_owns_binding(&self, id: ElmId, binding: BindingId) -> bool {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.owned_bindings.iter().any(|owned| *owned == binding))
            .unwrap_or(false)
    }

    fn cell_owns_menu_item(&self, id: ElmId, menu_item: u64) -> bool {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| {
                cell.owned_menu_items
                    .iter()
                    .any(|owned| *owned == menu_item)
            })
            .unwrap_or(false)
    }

    fn cell_index(&self, id: ElmId) -> Option<usize> {
        self.cells.iter().position(|cell| cell.id == id)
    }

    fn cell_id_by_name(&self, name: &str) -> Option<ElmId> {
        self.cells
            .iter()
            .find(|cell| cell.name == name)
            .map(|cell| cell.id)
    }

    fn resolve_unique_cell_name(&self, name: &str) -> Result<ElmId, ElmError> {
        let mut found = None;
        for cell in &self.cells {
            if cell.name != name {
                continue;
            }
            if found.is_some() {
                return Err(ElmError::DuplicateCell);
            }
            found = Some(cell.id);
        }
        found.ok_or(ElmError::CellNotFound)
    }

    fn is_builtin_cell(&self, id: ElmId) -> bool {
        id.0 < FIRST_DYNAMIC_CELL_ID
    }

    fn live_child_count(&self, id: ElmId) -> usize {
        self.cells
            .iter()
            .filter(|cell| cell.parent == Some(id) && cell.state != ElmState::Retired)
            .count()
    }

    fn cell_has_native_code(&self, id: ElmId) -> bool {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.has_native_code)
            .unwrap_or(false)
    }

    fn cell_is_isolated(&self, id: ElmId) -> bool {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.isolated)
            .unwrap_or(false)
    }

    fn cell_resource_budget(&self, id: ElmId) -> ElmResourceBudget {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.resource_budget)
            .unwrap_or(ElmResourceBudget::DEFAULT)
    }

    fn cell_policy_allows(&self, id: ElmId, action: u32) -> bool {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.cell_policy.allowed_actions & action != 0)
            .unwrap_or(false)
    }

    pub(crate) fn allows_owned_resource_registration(
        &self,
        id: ElmId,
        generation: Generation,
    ) -> bool {
        self.cells.iter().any(|cell| {
            cell.id == id
                && cell.generation == generation
                && !cell.isolated
                && matches!(
                    cell.state,
                    ElmState::Loaded | ElmState::Active | ElmState::Paused
                )
                && cell.cell_policy.resource_flags & ELM_RESOURCE_POLICY_OWN != 0
        })
    }

    fn native_unit_allowed_by_policy(&self, authority: ElmId, unit: &ElmEbiUnit) -> bool {
        if !unit.has_native_code() && unit.imports.is_empty() && unit.exports.is_empty() {
            return true;
        }
        let Some(policy) = self
            .cells
            .iter()
            .find(|cell| cell.id == authority)
            .map(|cell| cell.cell_policy)
        else {
            return false;
        };
        policy.native_flags & ELM_NATIVE_POLICY_EXECUTE != 0
            && (unit.imports.is_empty() || policy.native_flags & ELM_NATIVE_POLICY_IMPORT != 0)
            && (unit.exports.is_empty() || policy.native_flags & ELM_NATIVE_POLICY_EXPORT != 0)
    }

    pub(crate) fn cell_resource_usage(&self, id: ElmId) -> ElmResourceUsage {
        let accounted = super::resource_accounting::snapshot(id, sched::now_ns_public());
        let provider_ports = self
            .providers
            .iter()
            .filter(|provider| provider.dynamic && provider.owner == Some(id))
            .count() as u16;
        let provider_queue = self
            .provider_jobs
            .iter()
            .filter(|job| job.consumer == id)
            .count()
            .saturating_add(
                self.provider_running
                    .iter()
                    .filter(|running| running.job.consumer == id)
                    .count(),
            )
            .saturating_add(
                self.provider_results
                    .iter()
                    .filter(|result| result.consumer == id)
                    .count(),
            ) as u16;
        let event_subscriptions = self
            .mgr_runtime
            .event_subscriptions
            .iter()
            .filter(|subscription| subscription.owner == id)
            .count() as u16;
        let pending_loads = self
            .pending_ebi_loads
            .iter()
            .filter(|pending| pending.cell == id)
            .count() as u16;
        let native_images = self
            .native_images
            .iter()
            .filter(|image| image.cell() == id)
            .count()
            .saturating_add(
                self.retired_native_images
                    .iter()
                    .filter(|retired| retired.owner == id)
                    .count(),
            ) as u16;
        let native_image_bytes = self
            .native_images
            .iter()
            .filter(|image| image.cell() == id)
            .fold(0u64, |total, image| {
                total.saturating_add(image.size() as u64)
            })
            .saturating_add(
                self.retired_native_images
                    .iter()
                    .filter(|retired| retired.owner == id)
                    .fold(0u64, |total, retired| {
                        total.saturating_add(retired.image.size() as u64)
                    }),
            );
        let native_faults = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.native_faults)
            .unwrap_or(0);
        let audit_records = self
            .audits
            .iter()
            .filter(|audit| audit.cell_id == id.0)
            .count() as u16;

        let core_active_calls = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.active_executions.min(u32::from(u16::MAX)) as u16)
            .unwrap_or(0);
        ElmResourceUsage {
            provider_ports,
            provider_queue,
            event_subscriptions,
            pending_loads,
            native_images,
            native_faults,
            audit_records,
            active_calls: core_active_calls
                .max(accounted.active_native_calls.min(u32::from(u16::MAX)) as u16),
            native_image_bytes,
            native_stack_bytes: accounted.native_stack_bytes,
            dynamic_alloc_bytes: accounted.dynamic_alloc_bytes,
            peak_dynamic_alloc_bytes: accounted.peak_dynamic_alloc_bytes,
            cpu_time_ns_total: accounted.cpu_time_ns_total,
            cpu_time_ns_period: accounted.cpu_time_ns_period,
        }
    }

    fn cell_resource_over_quota(&self, id: ElmId, kind: ElmResourceKind) -> bool {
        let budget = self.cell_resource_budget(id);
        let usage = self.cell_resource_usage(id);
        match kind {
            ElmResourceKind::ProviderPort => usage.provider_ports >= budget.max_provider_ports,
            ElmResourceKind::ProviderQueue => usage.provider_queue >= budget.max_provider_queue,
            ElmResourceKind::EventSubscription => {
                usage.event_subscriptions >= budget.max_event_subscriptions
            }
            ElmResourceKind::PendingLoad => usage.pending_loads >= budget.max_pending_loads,
            ElmResourceKind::NativeImage => usage.native_images >= budget.max_native_images,
            ElmResourceKind::NativeFault => usage.native_faults >= budget.max_native_faults,
            ElmResourceKind::AuditRecord => usage.audit_records >= budget.max_audit_records,
            ElmResourceKind::ConcurrentCall => usage.active_calls >= budget.max_concurrent_calls,
            ElmResourceKind::NativeImageBytes => {
                usage.native_image_bytes >= budget.max_native_image_bytes
            }
            ElmResourceKind::NativeStackBytes => {
                usage.native_stack_bytes >= budget.max_native_stack_bytes
            }
            ElmResourceKind::DynamicAllocBytes => {
                usage.dynamic_alloc_bytes >= budget.max_dynamic_alloc_bytes
            }
            ElmResourceKind::CpuTimePerCall => budget.max_cpu_time_ns_per_call == 0,
            ElmResourceKind::CpuTimePerPeriod => {
                usage.cpu_time_ns_period >= budget.cpu_budget_ns_per_period
            }
        }
    }

    fn native_image_reservation_fits(&self, id: ElmId, image_bytes: u64) -> bool {
        let budget = self.cell_resource_budget(id);
        let usage = self.cell_resource_usage(id);
        usage.native_images < budget.max_native_images
            && usage
                .native_image_bytes
                .checked_add(image_bytes)
                .is_some_and(|bytes| bytes <= budget.max_native_image_bytes)
    }

    fn cell_needs_finalize(&self, id: ElmId) -> bool {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.lifecycle_initialized && !cell.lifecycle_finalized)
            .unwrap_or(false)
    }

    fn lifecycle_context(
        &self,
        id: ElmId,
        phase: ElmLifecyclePhase,
    ) -> Result<ElmContext, ElmError> {
        let Some(cell) = self.cells.iter().find(|cell| cell.id == id) else {
            return Err(ElmError::CellNotFound);
        };
        Ok(
            ElmContext::new(cell.id, cell.parent, cell.generation, cell.state, phase, 0)
                .with_kind(cell.kind)
                .with_allowed_actions(cell.cell_policy.allowed_actions),
        )
    }

    fn lifecycle_context_for_generation(
        &self,
        id: ElmId,
        generation: Generation,
        phase: ElmLifecyclePhase,
    ) -> Result<ElmContext, ElmError> {
        let Some(cell) = self.cells.iter().find(|cell| cell.id == id) else {
            return Err(ElmError::CellNotFound);
        };
        Ok(
            ElmContext::new(cell.id, cell.parent, generation, cell.state, phase, 0)
                .with_kind(cell.kind)
                .with_allowed_actions(cell.cell_policy.allowed_actions),
        )
    }

    fn lifecycle_context_for_generation_lossy(
        &self,
        id: ElmId,
        generation: Generation,
        phase: ElmLifecyclePhase,
    ) -> ElmContext {
        self.lifecycle_context_for_generation(id, generation, phase)
            .unwrap_or_else(|_| {
                ElmContext::new(id, Some(ELM_MGR_ID), generation, ElmState::Loaded, phase, 0)
            })
    }

    fn pending_ebi_load_index(&self, id: ElmId) -> Option<usize> {
        self.pending_ebi_loads
            .iter()
            .position(|pending| pending.cell == id)
    }

    fn take_pending_ebi_load(&mut self, id: ElmId) -> Option<PendingEbiLoad> {
        self.pending_ebi_load_index(id)
            .map(|index| self.pending_ebi_loads.remove(index))
    }

    fn discard_pending_ebi_load(&mut self, id: ElmId) {
        if let Some(pending) = self.take_pending_ebi_load(id) {
            self.abort_image_trust(&pending.trust);
        }
    }

    fn native_image_index(&self, id: ElmId) -> Option<usize> {
        self.native_images
            .iter()
            .position(|image| image.cell() == id)
    }

    fn remove_native_image(&mut self, id: ElmId) -> Option<LoadedElmImage> {
        self.native_image_index(id)
            .map(|index| self.native_images.remove(index))
    }

    fn retire_replaced_native_image(&mut self, id: ElmId, generation: Generation) {
        let Some(image) = self.remove_native_image(id) else {
            return;
        };
        if super::source::owner_generation_busy(id, generation) {
            self.retired_native_images.push(RetiredNativeImage {
                owner: id,
                generation,
                image,
            });
        }
    }

    fn reap_retired_native_images(&mut self) {
        self.retired_native_images.retain(|retired| {
            super::source::owner_generation_busy(retired.owner, retired.generation)
        });
    }

    fn quarantine_cell_after_hook_failure(&mut self, id: ElmId) {
        super::api_registry::remove_cell(id);
        self.mark_native_fault(id, ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED);
        match self.cell_state(id) {
            Some(ElmState::Faulted) => {}
            Some(ElmState::Quarantined) => return,
            Some(_) => {
                if self.transition_cell_state(id, ElmState::Faulted).is_err() {
                    return;
                }
            }
            None => return,
        }
        let _ = self.transition_cell_state(id, ElmState::Quarantined);
        if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) {
            cell.ebi_status = ElmEbiLoadStatus::RuntimeRejected;
        }
    }

    fn quarantine_cell_after_resource_failure(&mut self, id: ElmId) {
        super::api_registry::remove_cell(id);
        self.mark_native_fault(id, ELM_POLICY_BLOCK_RESOURCE_QUOTA);
        match self.cell_state(id) {
            Some(ElmState::Faulted) => {}
            Some(ElmState::Quarantined) => return,
            Some(_) => {
                if self.transition_cell_state(id, ElmState::Faulted).is_err() {
                    return;
                }
            }
            None => return,
        }
        let _ = self.transition_cell_state(id, ElmState::Quarantined);
    }

    fn mark_native_fault(&mut self, id: ElmId, blocker: u64) {
        let over_quota = self.cell_resource_over_quota(id, ElmResourceKind::NativeFault);
        let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) else {
            return;
        };
        cell.native_faults = cell.native_faults.saturating_add(1);
        cell.isolated = true;
        cell.isolation_blocker = if over_quota {
            ELM_POLICY_BLOCK_RESOURCE_QUOTA
        } else {
            blocker
        };
    }

    fn lifecycle_response(
        &self,
        id: ElmId,
        status: i32,
        reason: u32,
        revoked_leases: u32,
        removed_menu_items: u32,
    ) -> ElmLifecycleResponse {
        ElmLifecycleResponse::new(
            id.0,
            status,
            self.cell_state(id).map(state_code).unwrap_or(0),
            revoked_leases,
            removed_menu_items,
            reason,
        )
    }

    fn lifecycle_response_from_plan(
        &mut self,
        action: ElmLifecycleAction,
        plan: ElmLifecyclePlanResponse,
        revoked_leases: u32,
        removed_menu_items: u32,
    ) -> ElmLifecycleResponse {
        let response = ElmLifecycleResponse::new(
            plan.cell_id,
            plan.status,
            plan.final_state,
            revoked_leases,
            removed_menu_items,
            first_lifecycle_reason(plan.blockers),
        );
        self.finish_lifecycle(action, response, plan.blockers)
    }

    fn finish_lifecycle(
        &mut self,
        action: ElmLifecycleAction,
        response: ElmLifecycleResponse,
        blockers: u64,
    ) -> ElmLifecycleResponse {
        self.record_audit(
            action as u32,
            ElmId(response.cell_id),
            response.status,
            blockers,
            response.final_state,
        );
        response
    }

    fn record_audit(
        &mut self,
        action: u32,
        cell_id: ElmId,
        status: i32,
        blockers: u64,
        final_state: u32,
    ) {
        if let Some(sequence) = self.alloc_audit_sequence() {
            let record =
                ElmMgrAuditRecord::new(sequence, action, status, cell_id.0, blockers, final_state);
            self.push_audit_record(record);
        }
        if let Err(err) = super::journal::append(
            action,
            status,
            cell_id.0,
            cell_id.0,
            0,
            u64::from(final_state),
            blockers,
            0,
        ) {
            log::error!("[elm] 运行时日志持久化失败: {:?}", err);
        }
        self.push_journal_trace(cell_id, action, status, blockers);
        if ElmLifecycleAction::from_raw(action).is_some() {
            self.push_lifecycle_trace(cell_id, action, status, blockers);
        }
    }

    fn push_audit_record(&mut self, record: ElmMgrAuditRecord) {
        if self.audits.len() >= AUDIT_RING_LIMIT {
            self.audits.remove(0);
            self.dropped_audit_count = self.dropped_audit_count.saturating_add(1);
        }
        self.audits.push(record);
    }

    fn alloc_audit_sequence(&mut self) -> Option<u64> {
        let sequence = self.next_audit_sequence;
        if sequence == 0 {
            self.dropped_audit_count = self.dropped_audit_count.saturating_add(1);
            return None;
        }
        self.next_audit_sequence = match sequence.checked_add(1) {
            Some(next) => next,
            None => 0,
        };
        Some(sequence)
    }

    fn push_lifecycle_trace(&mut self, cell: ElmId, action: u32, status: i32, blockers: u64) {
        self.push_trace(
            ELM_RUNTIME_TRACE_KIND_LIFECYCLE,
            action,
            status,
            cell.0,
            cell.0,
            0,
            0,
            blockers,
        );
    }

    fn push_provider_call_trace(
        &mut self,
        cell: ElmId,
        binding: BindingId,
        port: PortId,
        status: i32,
        blockers: u64,
    ) {
        self.push_trace(
            ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL,
            ELM_MGR_ACTION_PROVIDER_INVOKE,
            status,
            cell.0,
            binding.0,
            port.0,
            0,
            blockers,
        );
    }

    fn push_mixin_trace(
        &mut self,
        target: ElmId,
        extension: ElmId,
        status: i32,
        called: u32,
        blockers: u64,
    ) {
        self.push_trace(
            ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH,
            ELM_MGR_ACTION_EXTENSION_DISPATCH,
            status,
            target.0,
            extension.0,
            u64::from(called),
            0,
            blockers,
        );
    }

    fn push_replace_trace(
        &mut self,
        cell: ElmId,
        generation: Generation,
        status: i32,
        migrated_len: u32,
        blockers: u64,
    ) {
        self.push_trace(
            ELM_RUNTIME_TRACE_KIND_REPLACE,
            ElmLifecycleAction::Replace as u32,
            status,
            cell.0,
            generation.0,
            u64::from(migrated_len),
            0,
            blockers,
        );
    }

    fn push_policy_trace(&mut self, cell_id: u64, value: u64, status: i32, blockers: u64) {
        self.push_trace(
            ELM_RUNTIME_TRACE_KIND_POLICY,
            ELM_MGR_ACTION_POLICY_UPDATE,
            status,
            cell_id,
            cell_id,
            0,
            value,
            blockers,
        );
    }

    fn push_resource_trace(&mut self, cell_id: u64, value: u64, status: i32, blockers: u64) {
        self.push_trace(
            ELM_RUNTIME_TRACE_KIND_RESOURCE,
            ELM_MGR_ACTION_RESOURCE_UPDATE,
            status,
            cell_id,
            cell_id,
            0,
            value,
            blockers,
        );
    }

    fn push_journal_trace(&mut self, cell: ElmId, action: u32, status: i32, blockers: u64) {
        self.push_trace(
            ELM_RUNTIME_TRACE_KIND_JOURNAL,
            action,
            status,
            cell.0,
            cell.0,
            0,
            0,
            blockers,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push_trace(
        &mut self,
        kind: u32,
        action: u32,
        status: i32,
        cell_id: u64,
        subject_id: u64,
        aux_id: u64,
        value: u64,
        blockers: u64,
    ) {
        let sequence = self.next_trace_sequence;
        if sequence == 0 {
            self.note_dropped_trace(kind);
            return;
        }
        self.next_trace_sequence = match sequence.checked_add(1) {
            Some(next) => next,
            None => 0,
        };
        let record = ElmRuntimeTraceRecord::new(
            sequence,
            sched::now_ns_public(),
            kind,
            action,
            status,
            cell_id,
            subject_id,
            aux_id,
            value,
            blockers,
        );
        match kind {
            ELM_RUNTIME_TRACE_KIND_LIFECYCLE => push_trace_record(
                &mut self.lifecycle_traces,
                &mut self.dropped_lifecycle_trace_count,
                record,
            ),
            ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL => push_trace_record(
                &mut self.provider_call_traces,
                &mut self.dropped_provider_call_trace_count,
                record,
            ),
            ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH => push_trace_record(
                &mut self.mixin_traces,
                &mut self.dropped_mixin_trace_count,
                record,
            ),
            ELM_RUNTIME_TRACE_KIND_REPLACE => push_trace_record(
                &mut self.replace_traces,
                &mut self.dropped_replace_trace_count,
                record,
            ),
            ELM_RUNTIME_TRACE_KIND_POLICY => push_trace_record(
                &mut self.policy_traces,
                &mut self.dropped_policy_trace_count,
                record,
            ),
            ELM_RUNTIME_TRACE_KIND_RESOURCE => push_trace_record(
                &mut self.resource_traces,
                &mut self.dropped_resource_trace_count,
                record,
            ),
            ELM_RUNTIME_TRACE_KIND_JOURNAL => push_trace_record(
                &mut self.runtime_journal,
                &mut self.dropped_runtime_journal_count,
                record,
            ),
            _ => {}
        }
    }

    fn note_dropped_trace(&mut self, kind: u32) {
        let dropped = match kind {
            ELM_RUNTIME_TRACE_KIND_LIFECYCLE => &mut self.dropped_lifecycle_trace_count,
            ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL => &mut self.dropped_provider_call_trace_count,
            ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH => &mut self.dropped_mixin_trace_count,
            ELM_RUNTIME_TRACE_KIND_REPLACE => &mut self.dropped_replace_trace_count,
            ELM_RUNTIME_TRACE_KIND_POLICY => &mut self.dropped_policy_trace_count,
            ELM_RUNTIME_TRACE_KIND_RESOURCE => &mut self.dropped_resource_trace_count,
            ELM_RUNTIME_TRACE_KIND_JOURNAL => &mut self.dropped_runtime_journal_count,
            _ => return,
        };
        *dropped = dropped.saturating_add(1);
    }

    fn remove_menu_items_owned_by(&mut self, id: ElmId) -> usize {
        let Some(index) = self.cell_index(id) else {
            return 0;
        };
        if self.cells[index].owned_menu_items.is_empty() {
            return 0;
        }
        let Some(next_menu_generation) = self.menu_generation.checked_next() else {
            log::error!(
                "[elm] refusing menu removal after generation exhaustion owner={}",
                id.0
            );
            return 0;
        };
        let owned = core::mem::take(&mut self.cells[index].owned_menu_items);

        let before = self.menu_items.len();
        self.menu_items
            .retain(|item| !owned.iter().any(|owned_id| *owned_id == item.id));
        let removed = before - self.menu_items.len();
        if removed != 0 {
            self.menu_generation = next_menu_generation;
            self.emit(TopologyEventKind::MenuItemRemoved, Some(id));
        }
        removed
    }

    fn take_owned_bindings(&mut self, id: ElmId) -> Vec<BindingId> {
        let Some(index) = self.cell_index(id) else {
            return Vec::new();
        };
        core::mem::take(&mut self.cells[index].owned_bindings)
    }

    fn remove_owned_binding(&mut self, id: ElmId, binding: BindingId) {
        let Some(index) = self.cell_index(id) else {
            return;
        };
        self.cells[index]
            .owned_bindings
            .retain(|owned| *owned != binding);
    }

    fn remove_cell_runtime(&mut self, id: ElmId) -> bool {
        let Some(generation) = self
            .cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.generation)
        else {
            return false;
        };
        if !super::resource_accounting::retire_cell(id) {
            log::error!("[elm] 单元退役后资源账本仍非空 cell={}", id.0);
            self.mark_native_fault(id, ELM_POLICY_BLOCK_RESOURCE_QUOTA);
            return false;
        }
        if !super::owned_resource::retire_owner(id, generation) {
            log::error!(
                "[elm] 单元退役后仍持有子系统资源 cell={} generation={}",
                id.0,
                generation.0
            );
            let _ = super::resource_accounting::register_cell(id, self.cell_resource_budget(id));
            self.mark_native_fault(id, ELM_POLICY_BLOCK_LEASE_BUSY);
            return false;
        }
        super::api_registry::remove_cell(id);
        self.cells.retain(|cell| cell.id != id);
        true
    }

    #[allow(dead_code)]
    fn alloc_cell_id(&mut self) -> Option<ElmId> {
        take_monotonic_id(&mut self.next_cell_id).map(ElmId)
    }

    #[allow(dead_code)]
    fn alloc_port_id(&mut self) -> Option<PortId> {
        take_monotonic_id(&mut self.next_port_id).map(PortId)
    }

    #[allow(dead_code)]
    fn alloc_binding_id(&mut self) -> Option<BindingId> {
        take_monotonic_id(&mut self.next_binding_id).map(BindingId)
    }

    #[allow(dead_code)]
    fn alloc_lease_id(&mut self) -> Option<LeaseId> {
        take_monotonic_id(&mut self.next_lease_id).map(LeaseId)
    }

    #[allow(dead_code)]
    fn alloc_action_id(&mut self) -> Option<ActionId> {
        take_monotonic_id(&mut self.next_action_id).map(ActionId)
    }

    #[allow(dead_code)]
    fn alloc_menu_item_id(&mut self) -> Option<u64> {
        take_monotonic_id(&mut self.next_menu_item_id)
    }

    fn alloc_provider_ticket_id(&mut self) -> Option<u64> {
        take_monotonic_id(&mut self.next_provider_ticket_id)
    }

    fn emit(&mut self, kind: TopologyEventKind, cell: Option<ElmId>) {
        let record = ElmEventRecord::new(self.next_event_sequence, kind, cell, None, None, None);
        self.push_event(record);
    }

    fn emit_port(&mut self, kind: TopologyEventKind, port: PortId) {
        let record =
            ElmEventRecord::new(self.next_event_sequence, kind, None, Some(port), None, None);
        self.push_event(record);
    }

    fn emit_binding(&mut self, kind: TopologyEventKind, binding: BindingId) {
        let record = ElmEventRecord::new(
            self.next_event_sequence,
            kind,
            None,
            None,
            Some(binding),
            None,
        );
        self.push_event(record);
    }

    fn emit_lease(&mut self, kind: TopologyEventKind, lease: LeaseId) {
        let record = ElmEventRecord::new(
            self.next_event_sequence,
            kind,
            None,
            None,
            None,
            Some(lease),
        );
        self.push_event(record);
    }

    fn push_event(&mut self, record: ElmEventRecord) {
        if record.sequence == 0 || record.sequence != self.next_event_sequence.0 {
            self.dropped_event_count = self.dropped_event_count.saturating_add(1);
            return;
        }
        self.next_event_sequence = self
            .next_event_sequence
            .checked_next()
            .unwrap_or(ElmEventSequence(0));
        if self.events.len() >= EVENT_RING_LIMIT {
            self.events.remove(0);
            self.dropped_event_count = self.dropped_event_count.saturating_add(1);
        }
        self.events.push(record);
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn test_monotonic_exhaustion() -> bool {
        let mut core = Self::new();
        core.next_cell_id = u64::MAX;
        core.next_port_id = u64::MAX;
        core.next_binding_id = u64::MAX;
        core.next_lease_id = u64::MAX;
        core.next_action_id = u64::MAX;
        core.next_menu_item_id = u64::MAX;
        core.next_provider_ticket_id = u64::MAX;
        core.next_provider_execution_id = u64::MAX;
        core.mgr_runtime.next_event_subscription_id = u64::MAX;
        core.mgr_runtime.next_kernel_provider_api_id = u64::MAX;

        let first_ids_ok = core.alloc_cell_id() == Some(ElmId(u64::MAX))
            && core.alloc_port_id() == Some(PortId(u64::MAX))
            && core.alloc_binding_id() == Some(BindingId(u64::MAX))
            && core.alloc_lease_id() == Some(LeaseId(u64::MAX))
            && core.alloc_action_id() == Some(ActionId(u64::MAX))
            && core.alloc_menu_item_id() == Some(u64::MAX)
            && core.alloc_provider_ticket_id() == Some(u64::MAX)
            && take_monotonic_id(&mut core.next_provider_execution_id) == Some(u64::MAX)
            && core.mgr_runtime.alloc_event_subscription_id() == Some(u64::MAX)
            && core.mgr_runtime.alloc_kernel_provider_api_id() == Some(u64::MAX);
        let exhausted_ids_reject = core.alloc_cell_id().is_none()
            && core.alloc_port_id().is_none()
            && core.alloc_binding_id().is_none()
            && core.alloc_lease_id().is_none()
            && core.alloc_action_id().is_none()
            && core.alloc_menu_item_id().is_none()
            && core.alloc_provider_ticket_id().is_none()
            && take_monotonic_id(&mut core.next_provider_execution_id).is_none()
            && core.mgr_runtime.alloc_event_subscription_id().is_none()
            && core.mgr_runtime.alloc_kernel_provider_api_id().is_none();

        core.next_event_sequence = ElmEventSequence(u64::MAX);
        core.emit(TopologyEventKind::CellAdded, None);
        core.emit(TopologyEventKind::CellAdded, None);
        let event_exhaustion_ok = core.events.len() == 1
            && core.events[0].sequence == u64::MAX
            && core.next_event_sequence.0 == 0
            && core.dropped_event_count == 1;

        core.next_audit_sequence = u64::MAX;
        let audit_exhaustion_ok = core.alloc_audit_sequence() == Some(u64::MAX)
            && core.alloc_audit_sequence().is_none()
            && core.next_audit_sequence == 0
            && core.dropped_audit_count == 1;

        core.next_trace_sequence = u64::MAX;
        core.push_lifecycle_trace(ElmId(1), 1, 0, 0);
        core.push_lifecycle_trace(ElmId(1), 1, 0, 0);
        let trace_exhaustion_ok = core.lifecycle_traces.len() == 1
            && core.lifecycle_traces[0].sequence == u64::MAX
            && core.next_trace_sequence == 0
            && core.dropped_lifecycle_trace_count == 1;

        first_ids_ok
            && exhausted_ids_reject
            && event_exhaustion_ok
            && audit_exhaustion_ok
            && trace_exhaustion_ok
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn test_native_replace_selection_policy() -> bool {
        let provider = ElmId(90);
        let consumer = ElmId(91);
        let contract = match FlowContract::new("test.managed@1") {
            Ok(contract) => contract,
            Err(_) => return false,
        };
        let managed_import = NativeImportRuntime {
            handle: 1,
            owner: consumer,
            owner_generation: Generation::FIRST,
            provider,
            provider_generation: Generation::FIRST,
            name: "managed.symbol".to_string(),
            contract: contract.clone(),
            min_version: 1,
            max_version: 3,
            selected_version: 1,
            flags: ELM_EBI_IMPORT_FLAG_MANAGED,
            address: 0x1000,
        };
        let export = |version, address| NativeExportRuntime {
            owner: provider,
            generation: Generation(2),
            name: "managed.symbol".to_string(),
            contract: contract.clone(),
            version,
            flags: ELM_EBI_EXPORT_FLAG_MANAGED,
            address,
            bounds: None,
        };
        let exports = alloc::vec![export(1, 0x2000), export(3, 0x3000), export(2, 0x2800)];
        let highest = select_managed_export_for_import(provider, &managed_import, &exports)
            .is_some_and(|selected| selected.version == 3 && selected.address == 0x3000);

        let duplicate_highest = alloc::vec![export(3, 0x3000), export(3, 0x3800)];
        let duplicate_rejected =
            select_managed_export_for_import(provider, &managed_import, &duplicate_highest)
                .is_none();

        let mut core = Self::new();
        let mut direct = managed_import;
        direct.flags = 0;
        core.native_imports.push(direct);
        let direct_blocks_replace = core.native_direct_pinned_importer_count(provider) == 1;

        highest && duplicate_rejected && direct_blocks_replace
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn test_native_import_staging_transaction() -> bool {
        let mut core = Self::new();
        if core.init_builtin_mgr().is_err() {
            return false;
        }
        let owner = ELM_EKI_ID;
        let provider = ELM_MGR_ID;
        let contract = match FlowContract::new("test.staged-import@1") {
            Ok(contract) => contract,
            Err(_) => return false,
        };
        core.native_exports.push(NativeExportRuntime {
            owner: provider,
            generation: Generation::FIRST,
            name: "test.staged-call".to_string(),
            contract: contract.clone(),
            version: 1,
            flags: ELM_EBI_EXPORT_FLAG_MANAGED,
            address: 0x1100,
            bounds: Some(NativeExecutionBounds {
                code_start: 0x1000,
                code_end: 0x2000,
                image_start: 0x1000,
                image_end: 0x3000,
            }),
        });
        let import = |handle, generation| NativeImportRuntime {
            handle,
            owner,
            owner_generation: generation,
            provider,
            provider_generation: Generation::FIRST,
            name: "test.staged-call".to_string(),
            contract: contract.clone(),
            min_version: 1,
            max_version: 1,
            selected_version: 1,
            flags: ELM_EBI_IMPORT_FLAG_MANAGED,
            address: 0x1100,
        };
        let frame = ElmCallFrame::empty(0, 1, 0);

        // 首次装载事务只能在受允许的生命周期阶段看见暂存 handle。
        let first_token = match core.reserve_cell_execution_exclusive(owner) {
            Ok(token) => token,
            Err(_) => return false,
        };
        let first_stage = match core.stage_native_imports(
            first_token,
            Generation::FIRST,
            alloc::vec![import(0x100, Generation::FIRST)],
        ) {
            Ok(stage) => stage,
            Err(_) => return false,
        };
        let wrong_phase_rejected = matches!(
            core.prepare_managed_call(
                owner,
                Generation::FIRST,
                ElmLifecyclePhase::Pause,
                0x100,
                frame,
            ),
            Err(ELM_MGR_STATUS_NOT_FOUND)
        );
        let wrong_generation_rejected = matches!(
            core.prepare_managed_call(
                owner,
                Generation(2),
                ElmLifecyclePhase::Initialize,
                0x100,
                frame,
            ),
            Err(ELM_MGR_STATUS_PERMISSION)
        );
        let first_plan = match core.prepare_managed_call(
            owner,
            Generation::FIRST,
            ElmLifecyclePhase::Initialize,
            0x100,
            frame,
        ) {
            Ok(plan) => plan,
            Err(_) => return false,
        };
        let first_visible = first_plan.caller.generation() == Generation::FIRST
            && core
                .complete_managed_call(first_plan, ElmReplyFrame::empty(0, 1, ELM_CALL_STATUS_OK))
                .is_ok();
        let first_discarded = core.discard_native_import_stage(first_stage) == 1
            && core.staged_native_imports.is_empty()
            && !core
                .native_imports
                .iter()
                .any(|candidate| candidate.handle == 0x100);
        core.release_cell_execution(first_token);

        // 替换事务使用新逻辑代际，但仍由旧代际的独占执行令牌保护。
        let replace_token = match core.reserve_cell_execution_exclusive(owner) {
            Ok(token) => token,
            Err(_) => return false,
        };
        let replace_stage = match core.stage_native_imports(
            replace_token,
            Generation(2),
            alloc::vec![import(0x101, Generation(2))],
        ) {
            Ok(stage) => stage,
            Err(_) => return false,
        };
        let replace_plan = match core.prepare_managed_call(
            owner,
            Generation(2),
            ElmLifecyclePhase::MigrateImport,
            0x101,
            frame,
        ) {
            Ok(plan) => plan,
            Err(_) => return false,
        };
        let replace_visible = replace_plan.caller.generation() == Generation(2)
            && core
                .complete_managed_call(replace_plan, ElmReplyFrame::empty(0, 1, ELM_CALL_STATUS_OK))
                .is_ok();
        let replace_discarded = core.discard_native_import_stage(replace_stage) == 1
            && matches!(
                core.prepare_managed_call(
                    owner,
                    Generation(2),
                    ElmLifecyclePhase::Initialize,
                    0x101,
                    frame,
                ),
                Err(ELM_MGR_STATUS_NOT_FOUND)
            );
        core.release_cell_execution(replace_token);

        // 成功提交只移动一次暂存记录，随后按正式 Active import 使用。
        let promote_token = match core.reserve_cell_execution_exclusive(owner) {
            Ok(token) => token,
            Err(_) => return false,
        };
        let promote_stage = match core.stage_native_imports(
            promote_token,
            Generation::FIRST,
            alloc::vec![import(0x102, Generation::FIRST)],
        ) {
            Ok(stage) => stage,
            Err(_) => return false,
        };
        let promoted = core.reserve_native_import_stage_promotion(promote_stage)
            && core.promote_native_import_stage(promote_stage)
            && core.staged_native_imports.is_empty()
            && core
                .native_imports
                .iter()
                .filter(|candidate| candidate.handle == 0x102)
                .count()
                == 1
            && !core.promote_native_import_stage(promote_stage);
        core.release_cell_execution(promote_token);
        let active_plan = match core.prepare_managed_call(
            owner,
            Generation::FIRST,
            ElmLifecyclePhase::Pause,
            0x102,
            frame,
        ) {
            Ok(plan) => plan,
            Err(_) => return false,
        };
        let active_visible = core
            .complete_managed_call(active_plan, ElmReplyFrame::empty(0, 1, ELM_CALL_STATUS_OK))
            .is_ok();

        wrong_phase_rejected
            && wrong_generation_rejected
            && first_visible
            && first_discarded
            && replace_visible
            && replace_discarded
            && promoted
            && active_visible
    }

    #[cfg(feature = "kernel-tests")]
    pub(crate) fn test_native_replace_old_generation_recovery() -> bool {
        let mut context = ElmContext::new(
            ELM_EKI_ID,
            Some(ELM_MGR_ID),
            Generation::FIRST,
            ElmState::Active,
            ElmLifecyclePhase::Resume,
            0,
        );
        let mut executor = ReplaceRecoveryTestExecutor::default();
        let untouched = resume_old_replace_generation(
            &mut executor,
            Some(&mut context),
            OldGenerationExecutionState::Untouched,
        );
        let resumed = resume_old_replace_generation(
            &mut executor,
            Some(&mut context),
            OldGenerationExecutionState::Quiesced,
        );

        let mut failed_executor = ReplaceRecoveryTestExecutor {
            fail_resume: true,
            ..ReplaceRecoveryTestExecutor::default()
        };
        let compromised = resume_old_replace_generation(
            &mut failed_executor,
            Some(&mut context),
            OldGenerationExecutionState::Quiesced,
        );
        let missing_context = resume_old_replace_generation(
            &mut executor,
            None,
            OldGenerationExecutionState::Quiesced,
        );

        untouched == OldGenerationExecutionState::Untouched
            && resumed == OldGenerationExecutionState::Resumed
            && compromised == OldGenerationExecutionState::Compromised
            && missing_context == OldGenerationExecutionState::Compromised
            && executor.resume_calls == 1
            && failed_executor.resume_calls == 1
    }
}

pub(crate) fn with_core<R>(f: impl FnOnce(&mut ElmCore) -> R) -> R {
    let mut core = CORE.lock();
    core.reap_retired_native_images();
    f(&mut core)
}

fn prepare_authorized_unlocked<T>(
    authorization: &mut ElmMgrAuthorization,
    kind: ElmMgrCallKind,
    target: ElmMgrAccessTarget,
    prepare: impl FnOnce(&mut ElmCore) -> T,
) -> Result<(T, ElmMgrAuthorizationExecution), i32> {
    with_core(|core| {
        let current = core.revalidate_mgr_authorization(*authorization, kind, target);
        *authorization = current;
        if !current.allowed() {
            return Err(ELM_MGR_STATUS_PERMISSION);
        }
        let execution = core.reserve_mgr_authorization_execution(current, kind)?;
        Ok((prepare(core), execution))
    })
}

fn authorization_execution_is_current(
    core: &ElmCore,
    authorization: &mut ElmMgrAuthorization,
    kind: ElmMgrCallKind,
    target: ElmMgrAccessTarget,
    execution: ElmMgrAuthorizationExecution,
) -> bool {
    let mut current = core.revalidate_mgr_authorization(*authorization, kind, target);
    if current.allowed() && !core.mgr_authorization_execution_is_current(execution) {
        current.blockers = ELM_POLICY_BLOCK_CALLER_STALE;
    }
    *authorization = current;
    current.allowed()
}

fn release_authorization_execution(execution: ElmMgrAuthorizationExecution) {
    with_core(|core| core.release_mgr_authorization_execution(execution));
}

pub(crate) fn invoke_provider_unlocked(
    request: ElmProviderInvokeRequest,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmProviderInvokeResponse, i32> {
    let target = ElmMgrAccessTarget::Binding(BindingId(request.frame.binding_id));
    let (prepared, authorization_execution) = prepare_authorized_unlocked(
        authorization,
        ElmMgrCallKind::InvokeProvider,
        target,
        |core| core.prepare_provider_call_execution(request),
    )?;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(status) => {
            release_authorization_execution(authorization_execution);
            return Err(status);
        }
    };
    match prepared {
        PreparedProviderCall::Immediate(result) => {
            release_authorization_execution(authorization_execution);
            result
        }
        PreparedProviderCall::External(plan) => {
            let result = execute_provider_call_plan(&plan);
            with_core(|core| {
                let current = authorization_execution_is_current(
                    core,
                    authorization,
                    ElmMgrCallKind::InvokeProvider,
                    target,
                    authorization_execution,
                );
                let response = if current {
                    core.complete_provider_call_execution(plan, result)
                } else {
                    let _ =
                        core.complete_provider_call_execution(plan, Err(ELM_MGR_STATUS_PERMISSION));
                    Err(ELM_MGR_STATUS_PERMISSION)
                };
                core.release_mgr_authorization_execution(authorization_execution);
                response
            })
        }
    }
}

pub(crate) fn provider_snapshot_unlocked(
    request: ElmProviderSnapshotRequest,
    authorization: &mut ElmMgrAuthorization,
) -> Result<Vec<u8>, i32> {
    let target = if request.binding_id != 0 {
        ElmMgrAccessTarget::Binding(BindingId(request.binding_id))
    } else {
        ElmMgrAccessTarget::Port(PortId(request.port_id))
    };
    let (prepared, authorization_execution) = prepare_authorized_unlocked(
        authorization,
        ElmMgrCallKind::QueryProviderSnapshot,
        target,
        |core| core.prepare_provider_snapshot_execution(request),
    )?;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(status) => {
            release_authorization_execution(authorization_execution);
            return Err(status);
        }
    };
    match prepared {
        PreparedProviderSnapshot::Immediate(result) => {
            release_authorization_execution(authorization_execution);
            result
        }
        PreparedProviderSnapshot::External(plan) => {
            let (page, payload) = execute_provider_snapshot_plan(&plan);
            with_core(|core| {
                let current = authorization_execution_is_current(
                    core,
                    authorization,
                    ElmMgrCallKind::QueryProviderSnapshot,
                    target,
                    authorization_execution,
                );
                let response = if current {
                    core.complete_provider_snapshot_execution(plan, page, payload)
                } else {
                    let _ = core.complete_provider_snapshot_execution(
                        plan,
                        ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_PERMISSION),
                        Vec::new(),
                    );
                    Err(ELM_MGR_STATUS_PERMISSION)
                };
                core.release_mgr_authorization_execution(authorization_execution);
                response
            })
        }
    }
}

pub(crate) fn dispatch_extension_unlocked(
    request: ElmExtensionDispatchRequest,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmExtensionDispatchResponse, i32> {
    let target = ElmMgrAccessTarget::Cells(
        ElmId(request.target_cell_id),
        ElmId(request.extension_cell_id),
    );
    let (prepared, authorization_execution) = prepare_authorized_unlocked(
        authorization,
        ElmMgrCallKind::DispatchExtension,
        target,
        |core| core.prepare_extension_dispatch_execution(request),
    )?;
    let plan = match prepared {
        PreparedExtensionDispatch::Immediate(result) => {
            release_authorization_execution(authorization_execution);
            return result;
        }
        PreparedExtensionDispatch::External(plan) => plan,
    };

    let mut state = MixinDispatchState::new(plan.mode, plan.opcode, plan.payload.clone());
    for edge in plan.matched_edges.clone() {
        let prepared = with_core(|core| {
            core.prepare_mixin_provider_execution(&edge, plan.opcode, state.payload())
        });
        let mixin = match prepared {
            Ok(mixin) => mixin,
            Err(status) => {
                state.record_execution_error(status);
                if state.halted {
                    break;
                }
                continue;
            }
        };
        let result = execute_provider_call_plan(&mixin.call);
        if !state.note_invocation() {
            let _ = with_core(|core| core.complete_mixin_provider_execution(mixin, result));
            break;
        }
        let reply = match with_core(|core| core.complete_mixin_provider_execution(mixin, result)) {
            Ok(reply) => reply,
            Err(status) => {
                state.record_execution_error(status);
                if state.halted {
                    break;
                }
                continue;
            }
        };
        state.record_reply(reply);
        if state.halted {
            break;
        }
    }
    with_core(|core| {
        let current = authorization_execution_is_current(
            core,
            authorization,
            ElmMgrCallKind::DispatchExtension,
            target,
            authorization_execution,
        );
        let response = if current {
            Ok(core.complete_extension_dispatch_execution(
                plan,
                state.called,
                state.blockers,
                state.last_reply,
            ))
        } else {
            let _ = core.complete_extension_dispatch_execution(
                plan,
                state.called,
                state.blockers | ELM_POLICY_BLOCK_CAPABILITY_DENIED,
                state.last_reply,
            );
            Err(ELM_MGR_STATUS_PERMISSION)
        };
        core.release_mgr_authorization_execution(authorization_execution);
        response
    })
}

pub(crate) fn pause_cell_unlocked(
    id: ElmId,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmLifecycleResponse, i32> {
    run_native_lifecycle_unlocked(id, ElmLifecycleAction::Pause, authorization)
}

pub(crate) fn resume_cell_unlocked(
    id: ElmId,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmLifecycleResponse, i32> {
    run_native_lifecycle_unlocked(id, ElmLifecycleAction::Resume, authorization)
}

pub(crate) fn detach_cell_unlocked(
    id: ElmId,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmLifecycleResponse, i32> {
    run_native_lifecycle_unlocked(id, ElmLifecycleAction::Detach, authorization)
}

pub(crate) fn load_ebi_image_unlocked(
    image: ElmEbiImage,
    arch: ElmEbiArch,
    source: ElmEbiSourceKind,
    parent: ElmId,
    budget: ElmResourceBudget,
    grant_management: bool,
    grant_kernel_api: bool,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmLoadCellResponse, i32> {
    let target = ElmMgrAccessTarget::Load(parent, budget);
    let kernel_api_grant =
        KernelApiGrantRequest::from_authorization(grant_kernel_api, *authorization);
    let (prepared, authorization_execution) =
        prepare_authorized_unlocked(authorization, ElmMgrCallKind::LoadCell, target, |core| {
            core.prepare_native_load_execution(
                image,
                arch,
                source,
                parent,
                budget,
                grant_management,
                kernel_api_grant,
            )
        })?;
    let plan = match prepared {
        PreparedNativeLoad::Immediate(response) => {
            release_authorization_execution(authorization_execution);
            return Ok(response);
        }
        PreparedNativeLoad::Initialize(plan) => plan,
    };
    let initialize_result = plan.loaded.on_initialize(&plan.initialize);
    let mut authorization_failed = false;
    let initialize_commit = with_core(|core| {
        let current = authorization_execution_is_current(
            core,
            authorization,
            ElmMgrCallKind::LoadCell,
            target,
            authorization_execution,
        );
        let commit = if current {
            core.commit_native_load_initialize(plan, initialize_result)
        } else {
            authorization_failed = true;
            NativeLoadCommit::Finalize(core.abort_native_load_after_initialize(
                plan,
                ElmEbiLoadStatus::RuntimeRejected,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
            ))
        };
        if !matches!(&commit, NativeLoadCommit::Entry(_)) {
            core.release_mgr_authorization_execution(authorization_execution);
        }
        commit
    });
    if authorization_failed {
        if let NativeLoadCommit::Finalize(failure) = initialize_commit {
            let _ = finish_failed_native_load(failure);
        }
        return Err(ELM_MGR_STATUS_PERMISSION);
    }
    match initialize_commit {
        NativeLoadCommit::Complete(response) => Ok(response),
        NativeLoadCommit::Finalize(failure) => Ok(finish_failed_native_load(failure)),
        NativeLoadCommit::Entry(plan) => {
            let entry_result =
                plan.loaded
                    .on_entry(plan.parent, plan.token.generation, ElmState::Active);
            let mut authorization_failed = false;
            let entry_commit = with_core(|core| {
                let current = authorization_execution_is_current(
                    core,
                    authorization,
                    ElmMgrCallKind::LoadCell,
                    target,
                    authorization_execution,
                );
                let commit = if current {
                    core.commit_native_load_entry(plan, entry_result)
                } else {
                    authorization_failed = true;
                    NativeLoadCommit::Finalize(core.abort_native_load_after_initialize(
                        plan,
                        ElmEbiLoadStatus::RuntimeRejected,
                        ELM_LIFECYCLE_REASON_INVALID_STATE,
                    ))
                };
                core.release_mgr_authorization_execution(authorization_execution);
                commit
            });
            if authorization_failed {
                if let NativeLoadCommit::Finalize(failure) = entry_commit {
                    let _ = finish_failed_native_load(failure);
                }
                return Err(ELM_MGR_STATUS_PERMISSION);
            }
            match entry_commit {
                NativeLoadCommit::Complete(response) => Ok(response),
                NativeLoadCommit::Finalize(failure) => Ok(finish_failed_native_load(failure)),
                NativeLoadCommit::Entry(_) => Err(ELM_MGR_STATUS_INVALID),
            }
        }
    }
}

pub(crate) fn replace_cell_unlocked(
    id: ElmId,
    image: ElmEbiImage,
    arch: ElmEbiArch,
    migration_limit: u32,
    source: ElmEbiSourceKind,
    grant_kernel_api: bool,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmReplaceCellResponseV1, i32> {
    let target = ElmMgrAccessTarget::Cell(id);
    let kernel_api_grant =
        KernelApiGrantRequest::from_authorization(grant_kernel_api, *authorization);
    let (prepared, authorization_execution) =
        prepare_authorized_unlocked(authorization, ElmMgrCallKind::ReplaceCell, target, |core| {
            core.prepare_native_replace_execution(
                id,
                image,
                arch,
                migration_limit,
                source,
                kernel_api_grant,
            )
        })?;
    match prepared {
        PreparedNativeReplace::Immediate(response) => {
            release_authorization_execution(authorization_execution);
            Ok(response)
        }
        PreparedNativeReplace::Execute(mut plan) => {
            let mut outcome = execute_native_replace_plan(&mut plan);
            with_core(|core| {
                let current = authorization_execution_is_current(
                    core,
                    authorization,
                    ElmMgrCallKind::ReplaceCell,
                    target,
                    authorization_execution,
                );
                let response = if current {
                    Ok(core.complete_native_replace_execution(plan, outcome))
                } else {
                    outcome.commit = false;
                    outcome.status = ELM_MGR_STATUS_PERMISSION;
                    outcome.blockers = ELM_POLICY_BLOCK_CALLER_STALE;
                    outcome.reason = ELM_LIFECYCLE_REASON_INVALID_STATE;
                    let _ = core.complete_native_replace_execution(plan, outcome);
                    Err(ELM_MGR_STATUS_PERMISSION)
                };
                core.release_mgr_authorization_execution(authorization_execution);
                response
            })
        }
    }
}

fn finish_failed_native_load(mut failure: NativeLoadFailurePlan) -> ElmLoadCellResponse {
    let mut executor = failure.loaded.lifecycle_executor();
    let result = executor.on_finalize(&mut failure.finalize);
    with_core(|core| {
        core.complete_native_load_failure(failure.id, failure.token, failure.import_stage, result)
    });
    failure.response
}

fn run_native_lifecycle_unlocked(
    id: ElmId,
    action: ElmLifecycleAction,
    authorization: &mut ElmMgrAuthorization,
) -> Result<ElmLifecycleResponse, i32> {
    let kind = match action {
        ElmLifecycleAction::Pause => ElmMgrCallKind::PauseCell,
        ElmLifecycleAction::Resume => ElmMgrCallKind::ResumeCell,
        ElmLifecycleAction::Detach => ElmMgrCallKind::DetachCell,
        ElmLifecycleAction::Replace => return Err(ELM_MGR_STATUS_INVALID),
    };
    let target = ElmMgrAccessTarget::Cell(id);
    let (prepared, authorization_execution) =
        prepare_authorized_unlocked(authorization, kind, target, |core| {
            core.prepare_native_lifecycle_execution(id, action)
        })?;
    match prepared {
        PreparedNativeLifecycle::Immediate(response) => {
            release_authorization_execution(authorization_execution);
            Ok(response)
        }
        PreparedNativeLifecycle::External(mut plan) => {
            let outcome = execute_native_lifecycle_plan(&mut plan);
            with_core(|core| {
                let current = authorization_execution_is_current(
                    core,
                    authorization,
                    kind,
                    target,
                    authorization_execution,
                );
                let response = if current {
                    Ok(core.complete_native_lifecycle_execution(plan, outcome))
                } else {
                    if let Some(suspension) = plan.source_suspension.take() {
                        let _ = suspension.keep_suspended();
                    }
                    core.quarantine_cell_after_hook_failure(id);
                    core.release_cell_execution(plan.token);
                    Err(ELM_MGR_STATUS_PERMISSION)
                };
                core.release_mgr_authorization_execution(authorization_execution);
                response
            })
        }
    }
}

pub(crate) fn run_one_async_provider_job_unlocked(now_ns: u64) -> bool {
    if let Some(notification) = with_core(|core| core.take_provider_revoke_notification()) {
        (notification.callback)(notification.binding, notification.lease);
        return true;
    }
    let prepared = with_core(|core| core.prepare_one_async_provider_execution(now_ns));
    match prepared {
        PreparedAsyncProviderWork::None => false,
        PreparedAsyncProviderWork::Handled => true,
        PreparedAsyncProviderWork::External(plan) => {
            let result = execute_provider_call_plan(&plan.call);
            let finish_ns = sched::now_ns_public();
            with_core(|core| core.complete_async_provider_execution(plan, result, finish_ns));
            true
        }
    }
}

fn invoke_managed_unlocked(
    caller: ElmId,
    caller_generation: Generation,
    caller_phase: ElmLifecyclePhase,
    import_handle: u64,
    frame: ElmCallFrame,
) -> Result<ElmReplyFrame, i32> {
    let plan = with_core(|core| {
        core.prepare_managed_call(
            caller,
            caller_generation,
            caller_phase,
            import_handle,
            frame,
        )
    })?;
    let reply = super::native::invoke_managed_export(
        plan.address,
        plan.bounds,
        plan.import_handle,
        plan.caller.cell(),
        plan.caller.generation(),
        plan.callee.cell,
        plan.callee.generation,
        plan.frame,
        plan.callee.allowed_actions,
    );
    with_core(|core| core.complete_managed_call(plan, reply))
}

fn execute_provider_call_plan(plan: &ProviderCallExecutionPlan) -> Result<ElmReplyFrame, i32> {
    match plan.backend {
        ProviderBackend::KernelOps(spec) => Ok((spec.invoke)(plan.frame)),
        ProviderBackend::ElmNative(native) => {
            let allowed_actions = plan
                .reservation
                .cells
                .iter()
                .find(|token| token.cell == native.owner)
                .map(|token| token.allowed_actions)
                .unwrap_or(0);
            Ok(super::native::invoke_provider_handler(
                native.handler,
                native.bounds,
                native.owner,
                native.generation,
                plan.edge.port,
                plan.reservation.lease.unwrap_or_else(|| LeaseId(0)),
                plan.frame,
                plan.deadline_ns,
                allowed_actions,
                plan.reply_flags_mask,
            ))
        }
        ProviderBackend::Kernel(_) | ProviderBackend::ElmNativeTodo => {
            Err(ELM_MGR_STATUS_UNSUPPORTED)
        }
    }
}

fn execute_native_lifecycle_plan(
    plan: &mut NativeLifecycleExecutionPlan,
) -> NativeLifecycleExecutionOutcome {
    let hook_failure = |result| NativeLifecycleExecutionOutcome {
        result,
        blockers: ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
        reason: ELM_LIFECYCLE_REASON_HOOK_FAILED,
        drained_resources: 0,
    };
    match &mut plan.work {
        NativeLifecycleWork::Pause { quiesce, pause } => {
            let Some(executor) = plan.executor.as_mut() else {
                return hook_failure(Err(ElmError::InvalidTransition));
            };
            if let Err(err) = executor.on_quiesce(quiesce) {
                return hook_failure(Err(err));
            }
            match executor.on_pause(pause) {
                Ok(()) => NativeLifecycleExecutionOutcome {
                    result: Ok(()),
                    blockers: 0,
                    reason: ELM_LIFECYCLE_REASON_NONE,
                    drained_resources: 0,
                },
                Err(err) => hook_failure(Err(err)),
            }
        }
        NativeLifecycleWork::Resume { resume } => {
            let Some(executor) = plan.executor.as_mut() else {
                return hook_failure(Err(ElmError::InvalidTransition));
            };
            match executor.on_resume(resume) {
                Ok(()) => NativeLifecycleExecutionOutcome {
                    result: Ok(()),
                    blockers: 0,
                    reason: ELM_LIFECYCLE_REASON_NONE,
                    drained_resources: 0,
                },
                Err(err) => hook_failure(Err(err)),
            }
        }
        NativeLifecycleWork::Detach {
            quiesce,
            finalize,
            owner,
            generation,
        } => {
            if let Some(context) = quiesce {
                let Some(executor) = plan.executor.as_mut() else {
                    let _ = super::owned_resource::stop_accepting(*owner, *generation);
                    return hook_failure(Err(ElmError::InvalidTransition));
                };
                if let Err(err) = executor.on_quiesce(context) {
                    let _ = super::owned_resource::stop_accepting(*owner, *generation);
                    return hook_failure(Err(err));
                }
            }
            let before = super::owned_resource::count_owned_by(*owner, *generation);
            let drained_resources = match super::owned_resource::drain_owner(*owner, *generation) {
                Ok(report) => report.drained.min(u32::MAX as usize) as u32,
                Err(_) => {
                    let remaining = super::owned_resource::count_owned_by(*owner, *generation);
                    return NativeLifecycleExecutionOutcome {
                        result: Err(ElmError::LeaseBusy),
                        blockers: ELM_POLICY_BLOCK_LEASE_BUSY
                            | ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                        reason: ELM_LIFECYCLE_REASON_LEASE_BUSY,
                        drained_resources: before.saturating_sub(remaining).min(u32::MAX as usize)
                            as u32,
                    };
                }
            };
            if let Some(context) = finalize {
                let Some(executor) = plan.executor.as_mut() else {
                    return hook_failure(Err(ElmError::InvalidTransition));
                };
                if let Err(err) = executor.on_finalize(context) {
                    return NativeLifecycleExecutionOutcome {
                        result: Err(err),
                        blockers: ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                        reason: ELM_LIFECYCLE_REASON_HOOK_FAILED,
                        drained_resources,
                    };
                }
            }
            NativeLifecycleExecutionOutcome {
                result: Ok(()),
                blockers: 0,
                reason: ELM_LIFECYCLE_REASON_NONE,
                drained_resources,
            }
        }
    }
}

fn resume_old_replace_generation<E: ElmLifecycleExecutor>(
    executor: &mut E,
    resume: Option<&mut ElmContext>,
    state: OldGenerationExecutionState,
) -> OldGenerationExecutionState {
    if state != OldGenerationExecutionState::Quiesced {
        return state;
    }
    let Some(context) = resume else {
        return OldGenerationExecutionState::Compromised;
    };
    if executor.on_resume(context).is_ok() {
        OldGenerationExecutionState::Resumed
    } else {
        OldGenerationExecutionState::Compromised
    }
}

fn recover_old_replace_generation<E: ElmLifecycleExecutor>(
    executor: &mut E,
    resume: Option<&mut ElmContext>,
    state: OldGenerationExecutionState,
) -> bool {
    resume_old_replace_generation(executor, resume, state).recovered()
}

fn execute_native_replace_plan(
    plan: &mut NativeReplaceExecutionPlan,
) -> NativeReplaceExecutionOutcome {
    if let Err(err) = plan.new_executor.on_initialize(&mut plan.new_initialize) {
        log::error!(
            "[elm] 原生热替换失败 cell={} old_generation={} new_generation={} stage=initialize err={:?}",
            plan.id.0,
            plan.old_generation.0,
            plan.new_generation.0,
            err
        );
        let _ = plan.new_executor.on_finalize(&mut plan.new_finalize);
        return NativeReplaceExecutionOutcome {
            commit: false,
            old_execution: OldGenerationExecutionState::Untouched,
            status: ELM_MGR_STATUS_INVALID,
            blockers: ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
            reason: ELM_LIFECYCLE_REASON_HOOK_FAILED,
            migrated_len: 0,
        };
    }
    if plan.suspended_projection_sources != 0
        && !super::source::projection_source_generation_ready(
            plan.id,
            plan.old_generation,
            plan.new_generation,
        )
    {
        log::error!(
            "[elm] 原生热替换失败 cell={} old_generation={} new_generation={} stage=projection-source-ready suspended={}",
            plan.id.0,
            plan.old_generation.0,
            plan.new_generation.0,
            plan.suspended_projection_sources
        );
        let _ = plan.new_executor.on_finalize(&mut plan.new_finalize);
        return NativeReplaceExecutionOutcome {
            commit: false,
            old_execution: OldGenerationExecutionState::Untouched,
            status: ELM_MGR_STATUS_INVALID,
            blockers: ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
            reason: ELM_LIFECYCLE_REASON_INVALID_STATE,
            migrated_len: 0,
        };
    }

    let mut old_execution = OldGenerationExecutionState::Untouched;
    if let Some(context) = plan.old_quiesce.as_mut() {
        if let Err(err) = plan.old_executor.on_quiesce(context) {
            log::error!(
                "[elm] 原生热替换失败 cell={} old_generation={} new_generation={} stage=quiesce err={:?}",
                plan.id.0,
                plan.old_generation.0,
                plan.new_generation.0,
                err
            );
            let _ = plan.new_executor.on_migrate_abort(
                plan.id,
                plan.old_generation,
                plan.new_generation,
                &mut plan.migration,
                0,
            );
            let _ = plan.new_executor.on_finalize(&mut plan.new_finalize);
            return NativeReplaceExecutionOutcome {
                commit: false,
                old_execution: OldGenerationExecutionState::Compromised,
                status: ELM_MGR_STATUS_INVALID,
                blockers: ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                reason: ELM_LIFECYCLE_REASON_HOOK_FAILED,
                migrated_len: 0,
            };
        }
        old_execution = OldGenerationExecutionState::Quiesced;
    }

    let export_result = plan.old_executor.on_migrate_export(
        plan.id,
        plan.old_generation,
        plan.new_generation,
        &mut plan.migration,
    );
    let migrated_len = match export_result {
        Ok(len) if len <= plan.migration.len() => len,
        result => {
            log::error!(
                "[elm] 原生热替换失败 cell={} old_generation={} new_generation={} stage=migrate-export capacity={} result={:?}",
                plan.id.0,
                plan.old_generation.0,
                plan.new_generation.0,
                plan.migration.len(),
                result
            );
            let _ = plan.new_executor.on_migrate_abort(
                plan.id,
                plan.old_generation,
                plan.new_generation,
                &mut plan.migration,
                0,
            );
            let _ = plan.new_executor.on_finalize(&mut plan.new_finalize);
            old_execution = resume_old_replace_generation(
                &mut plan.old_executor,
                plan.old_resume.as_mut(),
                old_execution,
            );
            if old_execution == OldGenerationExecutionState::Compromised {
                log::error!(
                    "[elm] 原生热替换回滚失败 cell={} old_generation={} new_generation={} stage=resume-after-migrate-export",
                    plan.id.0,
                    plan.old_generation.0,
                    plan.new_generation.0
                );
            }
            return NativeReplaceExecutionOutcome {
                commit: false,
                old_execution,
                status: ELM_MGR_STATUS_INVALID,
                blockers: ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                reason: ELM_LIFECYCLE_REASON_HOOK_FAILED,
                migrated_len: 0,
            };
        }
    };

    if let Err(err) = plan.new_executor.on_migrate_import(
        plan.id,
        plan.old_generation,
        plan.new_generation,
        &mut plan.migration,
        migrated_len,
    ) {
        log::error!(
            "[elm] 原生热替换失败 cell={} old_generation={} new_generation={} stage=migrate-import migrated_len={} err={:?}",
            plan.id.0,
            plan.old_generation.0,
            plan.new_generation.0,
            migrated_len,
            err
        );
        let _ = plan.new_executor.on_migrate_abort(
            plan.id,
            plan.old_generation,
            plan.new_generation,
            &mut plan.migration,
            migrated_len,
        );
        let _ = plan.new_executor.on_finalize(&mut plan.new_finalize);
        old_execution = resume_old_replace_generation(
            &mut plan.old_executor,
            plan.old_resume.as_mut(),
            old_execution,
        );
        if old_execution == OldGenerationExecutionState::Compromised {
            log::error!(
                "[elm] 原生热替换回滚失败 cell={} old_generation={} new_generation={} stage=resume-after-migrate-import",
                plan.id.0,
                plan.old_generation.0,
                plan.new_generation.0
            );
        }
        return NativeReplaceExecutionOutcome {
            commit: false,
            old_execution,
            status: ELM_MGR_STATUS_INVALID,
            blockers: ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
            reason: ELM_LIFECYCLE_REASON_HOOK_FAILED,
            migrated_len,
        };
    }

    NativeReplaceExecutionOutcome {
        commit: true,
        old_execution,
        status: ELM_MGR_STATUS_OK,
        blockers: 0,
        reason: ELM_LIFECYCLE_REASON_NONE,
        migrated_len,
    }
}

fn execute_provider_snapshot_plan(
    plan: &ProviderSnapshotExecutionPlan,
) -> (ProviderSnapshotPageResult, Vec<u8>) {
    let paged = plan.request.is_paged();
    let cursor = plan.request.cursor();
    let mut payload = Vec::new();
    payload.resize(plan.capacity, 0);
    let page = match plan.backend {
        ProviderBackend::KernelOps(spec) => {
            if paged {
                match spec.snapshot_paged {
                    Some(snapshot) => match snapshot(cursor, &mut payload) {
                        Ok(page)
                            if provider_snapshot_page_is_valid(
                                page.payload_len,
                                plan.capacity,
                                page.flags,
                                page.next_cursor,
                                true,
                                cursor,
                            ) =>
                        {
                            ProviderSnapshotPageResult::new(
                                ELM_MGR_STATUS_OK,
                                page.payload_len,
                                page.record_count,
                                page.flags,
                                page.next_cursor,
                            )
                        }
                        Ok(_) => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_INVALID),
                        Err(status) => ProviderSnapshotPageResult::status_only(status),
                    },
                    None => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED),
                }
            } else {
                match spec.snapshot {
                    Some(snapshot) => match snapshot(&mut payload) {
                        Ok(len) if len <= plan.capacity => {
                            ProviderSnapshotPageResult::new(ELM_MGR_STATUS_OK, len, 0, 0, 0)
                        }
                        Ok(_) => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_INVALID),
                        Err(status) => ProviderSnapshotPageResult::status_only(status),
                    },
                    None => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED),
                }
            }
        }
        ProviderBackend::ElmNative(native) => match native.snapshot {
            Some(snapshot) => {
                let (status, len, record_count, flags, next_cursor) =
                    super::native::invoke_provider_snapshot(
                        snapshot,
                        native.bounds,
                        native.owner,
                        native.generation,
                        plan.reservation.port,
                        plan.binding_id,
                        plan.lease,
                        plan.request.flags,
                        cursor,
                        &mut payload,
                        plan.allowed_actions,
                    );
                if status == ELM_MGR_STATUS_OK
                    && provider_snapshot_page_is_valid(
                        len,
                        plan.capacity,
                        flags,
                        next_cursor,
                        paged,
                        cursor,
                    )
                {
                    ProviderSnapshotPageResult::new(status, len, record_count, flags, next_cursor)
                } else if status == ELM_MGR_STATUS_OK {
                    ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_INVALID)
                } else {
                    ProviderSnapshotPageResult::status_only(status)
                }
            }
            None => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED),
        },
        ProviderBackend::Kernel(_) | ProviderBackend::ElmNativeTodo => {
            ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED)
        }
    };
    payload.truncate(page.payload_len.min(payload.len()));
    (page, payload)
}

fn push_health_ok_if_clean(records: &mut Vec<ElmCoreHealthRecord>, start: usize, check_kind: u32) {
    if records.len() == start {
        records.push(ElmCoreHealthRecord::ok(check_kind));
    }
}

fn take_monotonic_id(next: &mut u64) -> Option<u64> {
    let id = *next;
    if id == 0 {
        return None;
    }
    *next = match id.checked_add(1) {
        Some(value) => value,
        None => 0,
    };
    Some(id)
}

fn push_plain<T>(out: &mut Vec<u8>, value: &T) {
    out.extend_from_slice(plain_bytes(value));
}

fn plain_bytes<T>(value: &T) -> &[u8] {
    // 安全性：调用点只传入 ELM 管理通道 `#[repr(C)]` 固定布局结构，
    // 这些结构不包含内核指针，只作为用户态协议字节输出。
    unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    }
}

fn mgr_api(
    id: u64,
    kind: u32,
    flags: u32,
    call: ElmMgrCallKind,
    name: &str,
    contract: &str,
) -> ElmMgrApiDescriptor {
    ElmMgrApiDescriptor::new(
        id,
        ELM_MGR_ID.0,
        kind,
        flags,
        call as u32,
        "elm.mgr",
        name,
        contract,
    )
}

fn todo_record(
    kind: u32,
    flags: u32,
    blocker: u64,
    subject_id: u64,
    name: &str,
    detail: &str,
) -> ElmTodoRegistryRecord {
    ElmTodoRegistryRecord::new(
        kind,
        flags,
        blocker,
        subject_id,
        status_from_blockers(blocker),
        name,
        detail,
    )
}

fn normalize_event_read_limit(max_records: u32) -> usize {
    let requested = if max_records == 0 {
        ELM_MGR_EVENT_READ_DEFAULT_MAX_RECORDS
    } else {
        max_records
    };
    requested.min(ELM_MGR_EVENT_READ_ABSOLUTE_MAX_RECORDS) as usize
}

fn recover_stale_subscription_cursor(
    events: &[ElmEventRecord],
    cursor: &mut u64,
    dropped: &mut u64,
) {
    let Some(first) = events.first() else {
        return;
    };
    let next_requested = cursor.saturating_add(1);
    if next_requested >= first.sequence {
        return;
    }
    *dropped = (*dropped).saturating_add(first.sequence.saturating_sub(next_requested));
    *cursor = first.sequence.saturating_sub(1);
}

fn fixed_field(bytes: &[u8], len: u16) -> &str {
    let len = core::cmp::min(usize::from(len), bytes.len());
    core::str::from_utf8(&bytes[..len]).unwrap_or("<invalid>")
}

fn request_contract(request: &ElmNexusBindRequest) -> Option<&str> {
    let len = usize::from(request.contract_len);
    if len == 0 || len > request.contract.len() {
        return None;
    }
    core::str::from_utf8(&request.contract[..len]).ok()
}

fn provider_request_contract(request: &ElmProviderPortRegisterRequest) -> Option<&str> {
    let len = usize::from(request.contract_len);
    if len == 0 || len > request.contract.len() {
        return None;
    }
    core::str::from_utf8(&request.contract[..len]).ok()
}

fn extension_request_point(request: &ElmExtensionAttachRequest) -> Option<&str> {
    let len = usize::from(request.point_len);
    if len == 0 || len > request.point.len() {
        return None;
    }
    core::str::from_utf8(&request.point[..len]).ok()
}

fn extension_request_contract(request: &ElmExtensionAttachRequest) -> Option<&str> {
    let len = usize::from(request.contract_len);
    if len == 0 || len > request.contract.len() {
        return None;
    }
    core::str::from_utf8(&request.contract[..len]).ok()
}

fn extension_request_handler_contract(request: &ElmExtensionAttachRequest) -> Option<&str> {
    let len = usize::from(request.handler_contract_len);
    if len == 0 || len > request.handler_contract.len() {
        return None;
    }
    core::str::from_utf8(&request.handler_contract[..len]).ok()
}

fn extension_detach_request_point(request: &ElmExtensionDetachRequest) -> Option<&str> {
    let len = usize::from(request.point_len);
    if len == 0 || len > request.point.len() || request.reserved != 0 {
        return None;
    }
    core::str::from_utf8(&request.point[..len]).ok()
}

fn extension_dispatch_request_point(request: &ElmExtensionDispatchRequest) -> Option<&str> {
    let len = usize::from(request.point_len);
    if len == 0 || len > request.point.len() || request.reserved0 != 0 || request.reserved1 != 0 {
        return None;
    }
    core::str::from_utf8(&request.point[..len]).ok()
}

fn extension_dispatch_request_contract(request: &ElmExtensionDispatchRequest) -> Option<&str> {
    let len = usize::from(request.contract_len);
    if len == 0 || len > request.contract.len() {
        return None;
    }
    core::str::from_utf8(&request.contract[..len]).ok()
}

fn read_action_invoke_request(frame: &ElmCallFrame) -> Option<ElmActionInvokeRequest> {
    if usize::from(frame.payload_len) != core::mem::size_of::<ElmActionInvokeRequest>() {
        return None;
    }
    let payload = &frame.payload[..core::mem::size_of::<ElmActionInvokeRequest>()];
    let request = ElmActionInvokeRequest {
        action_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn runtime_status_blocker(status: i32) -> u64 {
    match status {
        ELM_MGR_STATUS_NOT_FOUND => ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
        ELM_MGR_STATUS_INVALID => ELM_POLICY_BLOCK_INVALID_STATE,
        _ => ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
    }
}

const fn native_import_is_managed(flags: u32) -> bool {
    flags & ELM_EBI_IMPORT_FLAG_MANAGED != 0
}

const fn native_export_is_managed(flags: u32) -> bool {
    flags & ELM_EBI_EXPORT_FLAG_MANAGED != 0
}

const fn native_import_stage_phase_allowed(phase: ElmLifecyclePhase) -> bool {
    matches!(
        phase,
        ElmLifecyclePhase::Initialize
            | ElmLifecyclePhase::Finalize
            | ElmLifecyclePhase::MigrateImport
            | ElmLifecyclePhase::MigrateAbort
    )
}

fn select_managed_export_for_import<'a>(
    owner: ElmId,
    import: &NativeImportRuntime,
    exports: &'a [NativeExportRuntime],
) -> Option<&'a NativeExportRuntime> {
    let highest = exports
        .iter()
        .filter(|export| {
            export.owner == owner
                && export.name == import.name
                && export.contract == import.contract
                && native_export_is_managed(export.flags)
                && export.version >= import.min_version
                && export.version <= import.max_version
        })
        .map(|export| export.version)
        .max()?;
    let mut matches = exports.iter().filter(|export| {
        export.owner == owner
            && export.name == import.name
            && export.contract == import.contract
            && native_export_is_managed(export.flags)
            && export.version == highest
    });
    let selected = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(selected)
    }
}

fn provider_call_blockers(status: i32) -> u64 {
    if status == ELM_CALL_STATUS_OK {
        0
    } else {
        ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED
    }
}

fn extension_dispatch_blocker(status: i32) -> u64 {
    match status {
        ELM_MGR_STATUS_NOT_FOUND => ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
        ELM_MGR_STATUS_UNSUPPORTED | ELM_MGR_STATUS_TODO => ELM_POLICY_BLOCK_PORT_TODO,
        ELM_MGR_STATUS_BUSY => ELM_POLICY_BLOCK_PROVIDER_BUSY,
        ELM_MGR_STATUS_PERMISSION => ELM_POLICY_BLOCK_CAPABILITY_DENIED,
        _ => ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    }
}

fn provider_snapshot_blockers(status: i32) -> u64 {
    match status {
        ELM_MGR_STATUS_OK => 0,
        ELM_MGR_STATUS_NOT_FOUND => ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
        ELM_MGR_STATUS_TODO => ELM_POLICY_BLOCK_NATIVE_TODO,
        ELM_MGR_STATUS_UNSUPPORTED => ELM_POLICY_BLOCK_PORT_TODO,
        ELM_MGR_STATUS_BUSY => ELM_POLICY_BLOCK_PROVIDER_BUSY,
        ELM_MGR_STATUS_INVALID => ELM_POLICY_BLOCK_INVALID_STATE,
        _ => ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    }
}

fn provider_snapshot_page_is_valid(
    payload_len: usize,
    capacity: usize,
    flags: u32,
    next_cursor: u32,
    paged: bool,
    cursor: u32,
) -> bool {
    if payload_len > capacity || flags & !ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK != 0 {
        return false;
    }
    let has_more = flags & ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE != 0;
    if has_more {
        paged && next_cursor != 0 && next_cursor != cursor
    } else {
        next_cursor == 0
    }
}

fn provider_async_poll_failure(
    ticket_id: u64,
    status: i32,
    blockers: u64,
) -> ElmProviderAsyncPollResponse {
    ElmProviderAsyncPollResponse::new(
        ticket_id,
        ElmProviderAsyncState::Failed,
        status,
        ElmReplyFrame::empty(0, 0, provider_async_call_status_from_mgr(status)),
        blockers,
        0,
    )
}

fn provider_async_blocker_from_mgr_status(status: i32) -> u64 {
    match status {
        ELM_MGR_STATUS_NOT_FOUND => ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
        ELM_MGR_STATUS_TODO => ELM_POLICY_BLOCK_NATIVE_TODO,
        ELM_MGR_STATUS_UNSUPPORTED => ELM_POLICY_BLOCK_PORT_TODO,
        ELM_MGR_STATUS_BUSY => ELM_POLICY_BLOCK_PROVIDER_BUSY,
        ELM_MGR_STATUS_INVALID => ELM_POLICY_BLOCK_INVALID_STATE,
        _ => ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    }
}

fn provider_async_call_status_from_mgr(status: i32) -> i32 {
    match status {
        ELM_MGR_STATUS_NOT_FOUND => ELM_CALL_STATUS_NOT_FOUND,
        ELM_MGR_STATUS_BUSY => ELM_CALL_STATUS_BUSY,
        ELM_MGR_STATUS_INVALID => ELM_CALL_STATUS_INVALID,
        ELM_MGR_STATUS_UNSUPPORTED => ELM_CALL_STATUS_UNSUPPORTED,
        _ => ELM_CALL_STATUS_PROVIDER_FAULT,
    }
}

fn provider_async_timeout_ns(timeout_ms: u32) -> u64 {
    let timeout_ms = normalize_provider_async_ms(timeout_ms, ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS);
    u64::from(timeout_ms).saturating_mul(1_000_000)
}

fn provider_async_result_ttl_ns(ttl_ms: u32) -> u64 {
    let ttl_ms = normalize_provider_async_ms(ttl_ms, ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS);
    u64::from(ttl_ms).saturating_mul(1_000_000)
}

fn normalize_provider_async_ms(value: u32, default: u32) -> u32 {
    let value = if value == 0 { default } else { value };
    value.min(ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS)
}

fn kernel_provider_kind(port: PortId) -> KernelProviderKind {
    if port == ELM_MGR_ACTION_PORT_ID {
        KernelProviderKind::MgrActionInvoke
    } else {
        KernelProviderKind::StaticPort
    }
}

fn provider_queue_limit_for_mode(mode: FlowMode) -> u32 {
    match mode {
        FlowMode::Exclusive => 1,
        FlowMode::Ordered => 32,
        FlowMode::Shared | FlowMode::Pipeline | FlowMode::Broadcast => {
            ELM_PROVIDER_ASYNC_QUEUE_LIMIT
        }
    }
}

fn provider_max_in_flight_for_mode(mode: FlowMode) -> u32 {
    match mode {
        FlowMode::Exclusive | FlowMode::Ordered => 1,
        FlowMode::Shared | FlowMode::Pipeline | FlowMode::Broadcast => 4,
    }
}

fn push_trace_record(
    ring: &mut Vec<ElmRuntimeTraceRecord>,
    dropped: &mut u32,
    record: ElmRuntimeTraceRecord,
) {
    if ring.len() >= TRACE_RING_LIMIT {
        ring.remove(0);
        *dropped = dropped.saturating_add(1);
    }
    ring.push(record);
}

fn trace_bytes(records: &[ElmRuntimeTraceRecord], dropped: u32, kind: u32) -> Vec<u8> {
    let header_size = core::mem::size_of::<ElmRuntimeTraceHeader>();
    let record_size = core::mem::size_of::<ElmRuntimeTraceRecord>();
    let max_records = ELM_MGR_MAX_PAYLOAD
        .saturating_sub(header_size)
        .checked_div(record_size)
        .unwrap_or(0);
    let emitted_records = records.len().min(max_records);
    let last_sequence = records.last().map(|record| record.sequence).unwrap_or(0);
    let header = ElmRuntimeTraceHeader::new(
        emitted_records as u32,
        dropped.saturating_add((records.len() - emitted_records) as u32),
        kind,
        last_sequence,
    );
    let mut out = Vec::new();
    push_plain(&mut out, &header);
    for record in records.iter().take(emitted_records) {
        push_plain(&mut out, record);
    }
    out
}

const fn mgr_call_required_action(kind: ElmMgrCallKind) -> u32 {
    match kind {
        ElmMgrCallKind::LoadCell
        | ElmMgrCallKind::DetachCell
        | ElmMgrCallKind::PauseCell
        | ElmMgrCallKind::ResumeCell
        | ElmMgrCallKind::ReplaceCell
        | ElmMgrCallKind::PreflightLifecycle => ELM_CELL_POLICY_ALLOW_LIFECYCLE,
        ElmMgrCallKind::PreflightBind
        | ElmMgrCallKind::CommitBind
        | ElmMgrCallKind::PreflightUnbind
        | ElmMgrCallKind::CommitUnbind => ELM_CELL_POLICY_ALLOW_BIND,
        ElmMgrCallKind::SubmitRuntimeLog
        | ElmMgrCallKind::ReadRuntimeEvent
        | ElmMgrCallKind::AckRuntimeEvent
        | ElmMgrCallKind::QueryRuntimePorts
        | ElmMgrCallKind::SubscribeEvent
        | ElmMgrCallKind::UnsubscribeEvent
        | ElmMgrCallKind::QueryEventSubscriptions
        | ElmMgrCallKind::ReadSubscribedEvents => ELM_CELL_POLICY_ALLOW_EVENT,
        ElmMgrCallKind::RegisterProviderPort
        | ElmMgrCallKind::UnregisterProviderPort
        | ElmMgrCallKind::QueryProviderPorts
        | ElmMgrCallKind::InvokeProvider
        | ElmMgrCallKind::QueryProviderStats
        | ElmMgrCallKind::SubmitProviderCall
        | ElmMgrCallKind::PollProviderReply
        | ElmMgrCallKind::CancelProviderCall
        | ElmMgrCallKind::QueryProviderQueue
        | ElmMgrCallKind::QueryProviderSnapshot => ELM_CELL_POLICY_ALLOW_PROVIDER,
        ElmMgrCallKind::QueryExtensions
        | ElmMgrCallKind::PreflightExtensionAttach
        | ElmMgrCallKind::CommitExtensionAttach
        | ElmMgrCallKind::CommitExtensionDetach
        | ElmMgrCallKind::DispatchExtension => ELM_CELL_POLICY_ALLOW_EXTENSION,
        ElmMgrCallKind::QueryNativeCapabilities => ELM_CELL_POLICY_ALLOW_NATIVE,
        ElmMgrCallKind::BeginImageSession
        | ElmMgrCallKind::WriteImageSession
        | ElmMgrCallKind::SealImageSession
        | ElmMgrCallKind::AbortImageSession => ELM_CELL_POLICY_ALLOW_NATIVE,
        ElmMgrCallKind::UpdateCellPolicy => ELM_CELL_POLICY_ALLOW_POLICY_UPDATE,
        ElmMgrCallKind::UpdateResourceBudget => ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE,
        ElmMgrCallKind::QueryMenu
        | ElmMgrCallKind::QueryTopology
        | ElmMgrCallKind::QueryPolicy
        | ElmMgrCallKind::QueryAudit
        | ElmMgrCallKind::QueryHealth
        | ElmMgrCallKind::QueryApiRegistry
        | ElmMgrCallKind::QueryTodoRegistry
        | ElmMgrCallKind::QueryFaultDump
        | ElmMgrCallKind::QueryLifecycleTrace
        | ElmMgrCallKind::QueryProviderCallTrace
        | ElmMgrCallKind::QueryMixinTrace
        | ElmMgrCallKind::QueryReplaceTrace
        | ElmMgrCallKind::QueryPolicyTrace
        | ElmMgrCallKind::QueryResourceDiagnostics
        | ElmMgrCallKind::QueryRuntimeJournal
        | ElmMgrCallKind::QueryCellPolicy
        | ElmMgrCallKind::QueryResourceBudget
        | ElmMgrCallKind::QueryTrustState
        | ElmMgrCallKind::QueryImageSession
        | ElmMgrCallKind::QueryNexusBindings => ELM_CELL_POLICY_ALLOW_OBSERVE,
    }
}

fn detailed_policy_blockers(policy: ElmCellPolicyV1, kind: ElmMgrCallKind) -> u64 {
    let allowed = match kind {
        ElmMgrCallKind::RegisterProviderPort => {
            policy.provider_flags & ELM_PROVIDER_POLICY_REGISTER != 0
        }
        ElmMgrCallKind::UnregisterProviderPort => {
            policy.provider_flags & ELM_PROVIDER_POLICY_UNREGISTER != 0
        }
        ElmMgrCallKind::InvokeProvider => policy.provider_flags & ELM_PROVIDER_POLICY_INVOKE != 0,
        ElmMgrCallKind::SubmitProviderCall
        | ElmMgrCallKind::PollProviderReply
        | ElmMgrCallKind::CancelProviderCall
        | ElmMgrCallKind::QueryProviderQueue => {
            policy.provider_flags & ELM_PROVIDER_POLICY_ASYNC != 0
        }
        ElmMgrCallKind::QueryProviderSnapshot => {
            policy.provider_flags & ELM_PROVIDER_POLICY_SNAPSHOT != 0
        }
        ElmMgrCallKind::PreflightExtensionAttach | ElmMgrCallKind::CommitExtensionAttach => {
            policy.extension_flags & ELM_EXTENSION_POLICY_ATTACH != 0
        }
        ElmMgrCallKind::CommitExtensionDetach => {
            policy.extension_flags & ELM_EXTENSION_POLICY_DETACH != 0
        }
        ElmMgrCallKind::DispatchExtension => {
            policy.extension_flags & ELM_EXTENSION_POLICY_DISPATCH != 0
        }
        ElmMgrCallKind::QueryResourceBudget => {
            policy.resource_flags & ELM_RESOURCE_POLICY_QUERY != 0
        }
        ElmMgrCallKind::UpdateResourceBudget => {
            policy.resource_flags & ELM_RESOURCE_POLICY_UPDATE != 0
        }
        ElmMgrCallKind::LoadCell => policy.native_flags & ELM_NATIVE_POLICY_EXECUTE != 0,
        ElmMgrCallKind::BeginImageSession
        | ElmMgrCallKind::WriteImageSession
        | ElmMgrCallKind::SealImageSession
        | ElmMgrCallKind::AbortImageSession => policy.native_flags & ELM_NATIVE_POLICY_EXECUTE != 0,
        ElmMgrCallKind::ReplaceCell => {
            policy.native_flags & (ELM_NATIVE_POLICY_REPLACE | ELM_NATIVE_POLICY_EXECUTE)
                == (ELM_NATIVE_POLICY_REPLACE | ELM_NATIVE_POLICY_EXECUTE)
        }
        _ => true,
    };
    if allowed {
        0
    } else {
        ELM_POLICY_BLOCK_CAPABILITY_DENIED
    }
}

const fn mgr_call_is_manager_only_query(kind: ElmMgrCallKind) -> bool {
    matches!(
        kind,
        ElmMgrCallKind::QueryAudit
            | ElmMgrCallKind::QueryFaultDump
            | ElmMgrCallKind::QueryLifecycleTrace
            | ElmMgrCallKind::QueryProviderCallTrace
            | ElmMgrCallKind::QueryMixinTrace
            | ElmMgrCallKind::QueryReplaceTrace
            | ElmMgrCallKind::QueryPolicyTrace
            | ElmMgrCallKind::QueryResourceDiagnostics
            | ElmMgrCallKind::QueryRuntimeJournal
            | ElmMgrCallKind::QueryTodoRegistry
    )
}

const fn mgr_call_is_mutating(kind: ElmMgrCallKind) -> bool {
    !matches!(
        kind,
        ElmMgrCallKind::QueryMenu
            | ElmMgrCallKind::QueryTopology
            | ElmMgrCallKind::QueryPolicy
            | ElmMgrCallKind::QueryAudit
            | ElmMgrCallKind::QueryNexusBindings
            | ElmMgrCallKind::QueryRuntimePorts
            | ElmMgrCallKind::PreflightLifecycle
            | ElmMgrCallKind::PreflightBind
            | ElmMgrCallKind::PreflightUnbind
            | ElmMgrCallKind::QueryProviderPorts
            | ElmMgrCallKind::QueryProviderStats
            | ElmMgrCallKind::QueryHealth
            | ElmMgrCallKind::QueryApiRegistry
            | ElmMgrCallKind::QueryEventSubscriptions
            | ElmMgrCallKind::QueryTodoRegistry
            | ElmMgrCallKind::QueryNativeCapabilities
            | ElmMgrCallKind::QueryExtensions
            | ElmMgrCallKind::PreflightExtensionAttach
            | ElmMgrCallKind::QueryFaultDump
            | ElmMgrCallKind::QueryLifecycleTrace
            | ElmMgrCallKind::QueryProviderCallTrace
            | ElmMgrCallKind::QueryMixinTrace
            | ElmMgrCallKind::QueryReplaceTrace
            | ElmMgrCallKind::QueryPolicyTrace
            | ElmMgrCallKind::QueryResourceDiagnostics
            | ElmMgrCallKind::QueryRuntimeJournal
            | ElmMgrCallKind::QueryCellPolicy
            | ElmMgrCallKind::QueryResourceBudget
            | ElmMgrCallKind::QueryTrustState
            | ElmMgrCallKind::BeginImageSession
            | ElmMgrCallKind::WriteImageSession
            | ElmMgrCallKind::SealImageSession
            | ElmMgrCallKind::AbortImageSession
            | ElmMgrCallKind::QueryImageSession
    )
}

fn policy_capabilities_subset(candidate: ElmCellPolicyV1, ceiling: ElmCellPolicyV1) -> bool {
    candidate.allowed_actions & !ceiling.allowed_actions == 0
        && candidate.provider_flags & !ceiling.provider_flags == 0
        && candidate.extension_flags & !ceiling.extension_flags == 0
        && candidate.native_flags & !ceiling.native_flags == 0
        && candidate.resource_flags & !ceiling.resource_flags == 0
}

fn policy_is_delegable_from(candidate: ElmCellPolicyV1, parent: ElmCellPolicyV1) -> bool {
    const INHERITED_FLAGS: u32 =
        ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION | ELM_CELL_POLICY_FLAG_AUDIT_ALL;
    policy_capabilities_subset(candidate, parent)
        && (candidate.flags & (parent.flags & INHERITED_FLAGS)) == (parent.flags & INHERITED_FLAGS)
}

const fn budget_is_subset(candidate: ElmResourceBudget, ceiling: ElmResourceBudget) -> bool {
    super::resource_accounting::budget_is_valid(candidate)
        && super::resource_accounting::budget_is_valid(ceiling)
        && candidate.max_provider_ports <= ceiling.max_provider_ports
        && candidate.max_provider_queue <= ceiling.max_provider_queue
        && candidate.max_event_subscriptions <= ceiling.max_event_subscriptions
        && candidate.max_pending_loads <= ceiling.max_pending_loads
        && candidate.max_native_images <= ceiling.max_native_images
        && candidate.max_native_faults <= ceiling.max_native_faults
        && candidate.max_audit_records <= ceiling.max_audit_records
        && candidate.max_concurrent_calls <= ceiling.max_concurrent_calls
        && candidate.max_native_image_bytes <= ceiling.max_native_image_bytes
        && candidate.max_native_stack_bytes <= ceiling.max_native_stack_bytes
        && candidate.max_dynamic_alloc_bytes <= ceiling.max_dynamic_alloc_bytes
        && candidate.max_cpu_time_ns_per_call <= ceiling.max_cpu_time_ns_per_call
        && cpu_rate_is_subset(candidate, ceiling)
}

const fn cpu_rate_is_subset(candidate: ElmResourceBudget, ceiling: ElmResourceBudget) -> bool {
    if candidate.cpu_budget_ns_per_period == 0 {
        return true;
    }
    if ceiling.cpu_budget_ns_per_period == 0
        || candidate.cpu_period_ns == 0
        || ceiling.cpu_period_ns == 0
    {
        return false;
    }
    (candidate.cpu_budget_ns_per_period as u128) * (ceiling.cpu_period_ns as u128)
        <= (ceiling.cpu_budget_ns_per_period as u128) * (candidate.cpu_period_ns as u128)
}

#[derive(Default)]
struct ResourceBudgetAccumulator {
    provider_ports: u64,
    provider_queue: u64,
    event_subscriptions: u64,
    pending_loads: u64,
    native_images: u64,
    native_faults: u64,
    audit_records: u64,
    concurrent_calls: u64,
    native_image_bytes: u64,
    native_stack_bytes: u64,
    dynamic_alloc_bytes: u64,
    cpu_time_ns_per_call: u64,
    cpu_rate_ppb: u128,
}

impl ResourceBudgetAccumulator {
    fn add(&mut self, budget: ElmResourceBudget) {
        self.provider_ports = self
            .provider_ports
            .saturating_add(u64::from(budget.max_provider_ports));
        self.provider_queue = self
            .provider_queue
            .saturating_add(u64::from(budget.max_provider_queue));
        self.event_subscriptions = self
            .event_subscriptions
            .saturating_add(u64::from(budget.max_event_subscriptions));
        self.pending_loads = self
            .pending_loads
            .saturating_add(u64::from(budget.max_pending_loads));
        self.native_images = self
            .native_images
            .saturating_add(u64::from(budget.max_native_images));
        self.native_faults = self
            .native_faults
            .saturating_add(u64::from(budget.max_native_faults));
        self.audit_records = self
            .audit_records
            .saturating_add(u64::from(budget.max_audit_records));
        self.concurrent_calls = self
            .concurrent_calls
            .saturating_add(u64::from(budget.max_concurrent_calls));
        self.native_image_bytes = self
            .native_image_bytes
            .saturating_add(budget.max_native_image_bytes);
        self.native_stack_bytes = self
            .native_stack_bytes
            .saturating_add(budget.max_native_stack_bytes);
        self.dynamic_alloc_bytes = self
            .dynamic_alloc_bytes
            .saturating_add(budget.max_dynamic_alloc_bytes);
        self.cpu_time_ns_per_call = self
            .cpu_time_ns_per_call
            .max(budget.max_cpu_time_ns_per_call);
        self.cpu_rate_ppb = self.cpu_rate_ppb.saturating_add(cpu_rate_ppb(
            budget.cpu_budget_ns_per_period,
            budget.cpu_period_ns,
        ));
    }

    fn add_usage(&mut self, usage: ElmResourceUsage, cpu_period_ns: u64) {
        self.provider_ports = self
            .provider_ports
            .saturating_add(u64::from(usage.provider_ports));
        self.provider_queue = self
            .provider_queue
            .saturating_add(u64::from(usage.provider_queue));
        self.event_subscriptions = self
            .event_subscriptions
            .saturating_add(u64::from(usage.event_subscriptions));
        self.pending_loads = self
            .pending_loads
            .saturating_add(u64::from(usage.pending_loads));
        self.native_images = self
            .native_images
            .saturating_add(u64::from(usage.native_images));
        self.native_faults = self
            .native_faults
            .saturating_add(u64::from(usage.native_faults));
        self.audit_records = self
            .audit_records
            .saturating_add(u64::from(usage.audit_records));
        self.concurrent_calls = self
            .concurrent_calls
            .saturating_add(u64::from(usage.active_calls));
        self.native_image_bytes = self
            .native_image_bytes
            .saturating_add(usage.native_image_bytes);
        self.native_stack_bytes = self
            .native_stack_bytes
            .saturating_add(usage.native_stack_bytes);
        self.dynamic_alloc_bytes = self
            .dynamic_alloc_bytes
            .saturating_add(usage.dynamic_alloc_bytes);
        self.cpu_rate_ppb = self
            .cpu_rate_ppb
            .saturating_add(cpu_rate_ppb(usage.cpu_time_ns_period, cpu_period_ns));
    }

    fn fits(&self, budget: ElmResourceBudget) -> bool {
        self.provider_ports <= u64::from(budget.max_provider_ports)
            && self.provider_queue <= u64::from(budget.max_provider_queue)
            && self.event_subscriptions <= u64::from(budget.max_event_subscriptions)
            && self.pending_loads <= u64::from(budget.max_pending_loads)
            && self.native_images <= u64::from(budget.max_native_images)
            && self.native_faults <= u64::from(budget.max_native_faults)
            && self.audit_records <= u64::from(budget.max_audit_records)
            && self.concurrent_calls <= u64::from(budget.max_concurrent_calls)
            && self.native_image_bytes <= budget.max_native_image_bytes
            && self.native_stack_bytes <= budget.max_native_stack_bytes
            && self.dynamic_alloc_bytes <= budget.max_dynamic_alloc_bytes
            && self.cpu_time_ns_per_call <= budget.max_cpu_time_ns_per_call
            && self.cpu_rate_ppb
                <= cpu_rate_ppb(budget.cpu_budget_ns_per_period, budget.cpu_period_ns)
    }
}

const fn cpu_rate_ppb(budget_ns: u64, period_ns: u64) -> u128 {
    if budget_ns == 0 {
        return 0;
    }
    if period_ns == 0 {
        return u128::MAX;
    }
    let scaled = (budget_ns as u128).saturating_mul(1_000_000_000);
    scaled.saturating_add(period_ns as u128 - 1) / period_ns as u128
}

fn runtime_log_level(level: u32) -> Option<log::LogLevel> {
    match level {
        0 => Some(log::LogLevel::Emergency),
        1 => Some(log::LogLevel::Alert),
        2 => Some(log::LogLevel::Critical),
        3 => Some(log::LogLevel::Error),
        4 => Some(log::LogLevel::Warning),
        5 => Some(log::LogLevel::Notice),
        6 => Some(log::LogLevel::Info),
        7 => Some(log::LogLevel::Debug),
        _ => None,
    }
}

fn expected_target_spec_hash(arch: ElmEbiArch) -> [u8; 32] {
    let identifier: &[u8] = match arch {
        ElmEbiArch::Any => b"any",
        ElmEbiArch::Riscv64 => b"riscv64gc-unknown-none-elf",
        ElmEbiArch::LoongArch64 => b"loongarch64-unknown-none",
    };
    sha256(identifier)
}

const fn trust_blocker(status: ElmEbiLoadStatus) -> u64 {
    match status {
        ElmEbiLoadStatus::AbiFingerprintRejected => ELM_POLICY_BLOCK_ABI_FINGERPRINT,
        ElmEbiLoadStatus::RollbackRejected => ELM_POLICY_BLOCK_ROLLBACK_REJECTED,
        _ => ELM_POLICY_BLOCK_UNTRUSTED_IMAGE,
    }
}

extern "C" fn elm_api_dispatch_mixin_v1(
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    elm_api_dispatch_command_v1(
        false,
        ElmMgrCallKind::DispatchExtension as u32,
        input,
        input_len,
        output,
        output_capacity,
        output_len,
    )
}

extern "C" fn elm_api_management_dispatch_v1(
    kind: u32,
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    elm_api_dispatch_command_v1(
        true,
        kind,
        input,
        input_len,
        output,
        output_capacity,
        output_len,
    )
}

fn elm_api_dispatch_command_v1(
    require_management: bool,
    kind: u32,
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    let Some(_domain) = general::elm_guard::enter_current_domain(
        general::elm_guard::ElmExecutionDomain::KernelCall,
    ) else {
        return ELM_API_STATUS_PERMISSION;
    };
    let Some(context) = current_context() else {
        return ELM_API_STATUS_PERMISSION;
    };
    if require_management && !management_namespace_allowed(context) {
        return ELM_API_STATUS_PERMISSION;
    }
    if output_len.is_null()
        || (input.is_null() && input_len != 0)
        || (output.is_null() && output_capacity != 0)
        || input_len > ELM_MGR_MAX_PAYLOAD
        || !general::elm_guard::validate_current_memory_range(
            output_len as usize,
            core::mem::size_of::<usize>(),
            true,
        )
        || !general::elm_guard::validate_current_memory_range(input as usize, input_len, false)
        || !general::elm_guard::validate_current_memory_range(
            output as usize,
            output_capacity,
            true,
        )
    {
        return ELM_API_STATUS_INVALID;
    }
    let Ok(payload_len) = u32::try_from(input_len) else {
        return ELM_API_STATUS_INVALID;
    };
    let mut call = Vec::new();
    push_plain(
        &mut call,
        &ElmMgrCallHeader {
            kind,
            flags: 0,
            payload_len,
            reserved: 0,
        },
    );
    if input_len != 0 {
        // 安全性：原生 ELM 属于受信内核代码；guard 负责把意外地址故障转为 ELM fault。
        let input = unsafe { core::slice::from_raw_parts(input, input_len) };
        call.extend_from_slice(input);
    }
    let response = super::mgr_channel::dispatch_mgr_call_as(
        ElmPrincipal::elm_cell(context.cell_id, context.generation),
        &call,
    );
    // 安全性：前面已验证输出长度指针非空。
    unsafe { output_len.write(response.len()) };
    if response.len() > output_capacity {
        return ELM_API_STATUS_BUFFER_TOO_SMALL;
    }
    if !response.is_empty() {
        // 安全性：调用者声明了足够的输出容量，源缓冲在本函数返回前保持有效。
        unsafe { core::ptr::copy_nonoverlapping(response.as_ptr(), output, response.len()) };
    }
    response
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(i32::from_le_bytes)
        .unwrap_or(ELM_API_STATUS_INVALID)
}

extern "C" fn elm_api_current_context_v1(output: *mut ElmApiContextV1) -> i32 {
    let Some(_domain) = general::elm_guard::enter_current_domain(
        general::elm_guard::ElmExecutionDomain::KernelCall,
    ) else {
        return ELM_API_STATUS_PERMISSION;
    };
    if output.is_null()
        || !general::elm_guard::validate_current_memory_range(
            output as usize,
            core::mem::size_of::<ElmApiContextV1>(),
            true,
        )
    {
        return ELM_API_STATUS_INVALID;
    }
    let Some(context) = current_context() else {
        return ELM_API_STATUS_PERMISSION;
    };
    let value = ElmApiContextV1 {
        struct_size: core::mem::size_of::<ElmApiContextV1>() as u32,
        flags: context.flags,
        cell_id: context.cell_id.0,
        parent_id: context.parent_id.map(|id| id.0).unwrap_or(0),
        generation: context.generation.0,
        state: ElmApiContextV1::state_code(context.state),
        phase: ElmApiContextV1::phase_code(context.phase),
        kind: context.kind.as_raw(),
        allowed_actions: context.allowed_actions,
        reserved: 0,
    };
    // 安全性：调用方提供固定布局输出槽，原生 guard 处理意外地址故障。
    unsafe { output.write(value) };
    ELM_API_STATUS_OK
}

extern "C" fn elm_api_query_namespace_v1(
    identifier: *const u8,
    identifier_len: usize,
    compatible_versions: *const u16,
    compatible_version_count: usize,
    output: *mut ElmApiNamespaceV1,
) -> i32 {
    let Some(_domain) = general::elm_guard::enter_current_domain(
        general::elm_guard::ElmExecutionDomain::KernelCall,
    ) else {
        return ELM_API_STATUS_PERMISSION;
    };
    if identifier.is_null()
        || identifier_len == 0
        || identifier_len > elm_model::ELM_KERNEL_API_IDENTIFIER_MAX_LEN
        || compatible_versions.is_null()
        || compatible_version_count == 0
        || compatible_version_count > elm_model::ELM_API_MAX_COMPATIBLE_VERSIONS
        || output.is_null()
        || !general::elm_guard::validate_current_memory_range(
            identifier as usize,
            identifier_len,
            false,
        )
        || !general::elm_guard::validate_current_memory_range(
            compatible_versions as usize,
            compatible_version_count.saturating_mul(core::mem::size_of::<u16>()),
            false,
        )
        || !general::elm_guard::validate_current_memory_range(
            output as usize,
            core::mem::size_of::<ElmApiNamespaceV1>(),
            true,
        )
    {
        return ELM_API_STATUS_INVALID;
    }
    // 安全性：原生 ELM 属于受信内核代码，长度均已施加硬上限。
    let identifier = unsafe { core::slice::from_raw_parts(identifier, identifier_len) };
    let versions =
        unsafe { core::slice::from_raw_parts(compatible_versions, compatible_version_count) };
    let Some(context) = current_context() else {
        return ELM_API_STATUS_PERMISSION;
    };
    let namespace = match super::api_registry::query(
        context.cell_id,
        context.generation,
        identifier,
        versions,
        management_namespace_allowed(context),
    ) {
        Ok(namespace) => namespace,
        Err(super::api_registry::ApiRegistryError::NamespaceUnavailable) => {
            return ELM_API_STATUS_NOT_FOUND;
        }
        Err(super::api_registry::ApiRegistryError::VersionUnsupported) => {
            return ELM_API_STATUS_UNSUPPORTED;
        }
        Err(super::api_registry::ApiRegistryError::CapabilityDenied) => {
            return ELM_API_STATUS_PERMISSION;
        }
        Err(_) => return ELM_API_STATUS_INVALID,
    };
    // 安全性：调用方提供固定布局输出槽，原生 guard 处理意外地址故障。
    unsafe { output.write(namespace) };
    ELM_API_STATUS_OK
}

pub(crate) fn management_namespace_allowed(context: ElmCurrentContext) -> bool {
    context.kind == ElmKind::Manager
        && context.generation.0 != 0
        && context.allowed_actions & ELM_CELL_POLICY_ALLOW_MANAGEMENT != 0
        && !matches!(
            context.state,
            ElmState::Detached | ElmState::Retired | ElmState::Faulted | ElmState::Quarantined
        )
}

extern "C" fn elm_runtime_log_v1(level: u32, message_ptr: *const u8, message_len: usize) -> i32 {
    let Some(_domain) = general::elm_guard::enter_current_domain(
        general::elm_guard::ElmExecutionDomain::KernelCall,
    ) else {
        return ELM_API_STATUS_PERMISSION;
    };
    if message_len > ELM_RUNTIME_LOG_MESSAGE_LEN
        || (message_ptr.is_null() && message_len != 0)
        || !general::elm_guard::validate_current_memory_range(
            message_ptr as usize,
            message_len,
            false,
        )
    {
        return ELM_MGR_STATUS_INVALID;
    }
    let Some(level) = runtime_log_level(level) else {
        return ELM_MGR_STATUS_INVALID;
    };
    let message = if message_len == 0 {
        ""
    } else {
        // 安全性：调用方是已装载的原生 ELM。地址有效性由原生执行保护层兜底；
        // 这里仍限制长度并要求 UTF-8，避免日志路径解析任意非文本数据。
        let bytes = unsafe { core::slice::from_raw_parts(message_ptr, message_len) };
        match core::str::from_utf8(bytes) {
            Ok(message) => message,
            Err(_) => return ELM_MGR_STATUS_INVALID,
        }
    };
    let cell = current_cell().map(|id| id.0).unwrap_or(0);
    let line = format!("[elm-runtime][cell={} native] {}", cell, message);
    log::logger_entry(level, log::get_timestamp_ns(), &line);
    ELM_MGR_STATUS_OK
}

extern "C" fn elm_api_invoke_managed_v1(
    import_handle: u64,
    request: *const ElmCallFrame,
    reply: *mut ElmReplyFrame,
) -> i32 {
    let Some(_domain) = general::elm_guard::enter_current_domain(
        general::elm_guard::ElmExecutionDomain::KernelCall,
    ) else {
        return ELM_API_STATUS_PERMISSION;
    };
    let Some(context) = current_context() else {
        return ELM_API_STATUS_PERMISSION;
    };
    if request.is_null()
        || reply.is_null()
        || !general::elm_guard::validate_current_memory_range(
            request as usize,
            core::mem::size_of::<ElmCallFrame>(),
            false,
        )
        || !general::elm_guard::validate_current_memory_range(
            reply as usize,
            core::mem::size_of::<ElmReplyFrame>(),
            true,
        )
    {
        return ELM_API_STATUS_INVALID;
    }
    // 安全性：范围已经由任务级 ELM 执行边界验证，随后立即复制到内核栈。
    let frame = unsafe { request.read() };
    match invoke_managed_unlocked(
        context.cell_id,
        context.generation,
        context.phase,
        import_handle,
        frame,
    ) {
        Ok(response) => {
            // 安全性：输出槽已经完成可写范围验证。
            unsafe { reply.write(response) };
            ELM_API_STATUS_OK
        }
        Err(status) => status,
    }
}

pub(crate) extern "C" fn elm_api_abort_current_v1(reason: u32) -> ! {
    let reason = match reason {
        elm_model::ELM_API_ABORT_REASON_CANCEL => general::elm_guard::ELM_GUARD_ABORT_CANCEL,
        elm_model::ELM_API_ABORT_REASON_TIMEOUT => general::elm_guard::ELM_GUARD_ABORT_TIMEOUT,
        elm_model::ELM_API_ABORT_REASON_PANIC => general::elm_guard::ELM_GUARD_ABORT_PANIC,
        _ => general::elm_guard::ELM_GUARD_ABORT_PANIC,
    };
    if let Some(recovery) = general::elm_guard::try_recover_explicit_abort(reason) {
        // 安全性：恢复地址和栈只可能来自当前任务调用门发布的边界帧。
        unsafe {
            arch::resume_elm_panic(
                recovery.return_pc,
                recovery.return_sp,
                recovery.return_value,
            )
        }
    }
    panic!("elmapi::abort_current 在 ELM 原生执行域之外被调用")
}
