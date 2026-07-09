//! ELM 核心全局状态。

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use elm_model::{
    ActionId, BindingGraph, BindingId, ELM_ACTION_OPCODE_INVOKE, ELM_CALL_STATUS_BUSY,
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_NOT_FOUND, ELM_CALL_STATUS_OK,
    ELM_CALL_STATUS_PROVIDER_FAULT, ELM_CALL_STATUS_UNSUPPORTED, ELM_HEALTH_CHECK_AUDITS,
    ELM_HEALTH_CHECK_BINDINGS, ELM_HEALTH_CHECK_CELLS, ELM_HEALTH_CHECK_EVENTS,
    ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_MENU, ELM_HEALTH_CHECK_NATIVE_CAPABILITIES,
    ELM_HEALTH_CHECK_PORTS, ELM_HEALTH_CHECK_PROVIDERS, ELM_HEALTH_CHECK_RUNTIME_PORTS,
    ELM_HEALTH_CHECK_TODO_REGISTRY, ELM_HEALTH_DETAIL_CONTRACT_INVALID,
    ELM_HEALTH_DETAIL_DANGLING_REFERENCE, ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
    ELM_HEALTH_DETAIL_GRAPH_INVALID, ELM_HEALTH_DETAIL_KIND_MISMATCH,
    ELM_HEALTH_DETAIL_MISSING_OBJECT, ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
    ELM_HEALTH_DETAIL_STATE_INVALID, ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT, ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
    ELM_LIFECYCLE_REASON_HOOK_FAILED, ELM_LIFECYCLE_REASON_INVALID_STATE,
    ELM_LIFECYCLE_REASON_LEASE_BUSY, ELM_LIFECYCLE_REASON_NATIVE_TODO, ELM_LIFECYCLE_REASON_NONE,
    ELM_MENU_FLAG_REQUIRES_SYS_ADMIN, ELM_MENU_FLAG_TODO, ELM_MGR_ACTION_BIND,
    ELM_MGR_ACTION_EVENT_READ, ELM_MGR_ACTION_EVENT_SUBSCRIBE, ELM_MGR_ACTION_EVENT_UNSUBSCRIBE,
    ELM_MGR_ACTION_PROVIDER_ASYNC, ELM_MGR_ACTION_PROVIDER_INVOKE, ELM_MGR_ACTION_PROVIDER_QUERY,
    ELM_MGR_ACTION_PROVIDER_REGISTER, ELM_MGR_ACTION_PROVIDER_UNREGISTER,
    ELM_MGR_ACTION_RUNTIME_EVENT_ACK, ELM_MGR_ACTION_RUNTIME_EVENT_READ,
    ELM_MGR_ACTION_RUNTIME_LOG, ELM_MGR_ACTION_UNBIND, ELM_MGR_BUILTIN_ID, ELM_MGR_MAX_PAYLOAD,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_TODO, ELM_MGR_STATUS_UNSUPPORTED, ELM_POLICY_BLOCK_BINDING_NOT_FOUND,
    ELM_POLICY_BLOCK_BINDING_PROTECTED, ELM_POLICY_BLOCK_BUILTIN_PROTECTED,
    ELM_POLICY_BLOCK_CELL_NOT_FOUND, ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
    ELM_POLICY_BLOCK_DUPLICATE_BINDING, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
    ELM_POLICY_BLOCK_HAS_CHILDREN, ELM_POLICY_BLOCK_HAS_DEPENDENTS,
    ELM_POLICY_BLOCK_HAS_EXTENSIONS, ELM_POLICY_BLOCK_INVALID_STATE, ELM_POLICY_BLOCK_LEASE_BUSY,
    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED, ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
    ELM_POLICY_BLOCK_NATIVE_TODO, ELM_POLICY_BLOCK_PORT_NOT_FOUND, ELM_POLICY_BLOCK_PORT_TODO,
    ELM_POLICY_BLOCK_PROVIDER_BUSY, ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED,
    ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED, ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
    ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, ELM_POLICY_BLOCK_RESOURCE_QUOTA,
    ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS, ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS,
    ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS, ELM_PROVIDER_ASYNC_QUEUE_LIMIT, ELM_PROVIDER_FLAG_DYNAMIC,
    ELM_PROVIDER_FLAG_KERNEL_BACKEND, ELM_PROVIDER_FLAG_NATIVE_BACKEND,
    ELM_PROVIDER_FLAG_TODO_BACKEND, ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK,
    ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE, ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK,
    ELM_REPLACE_MIGRATION_STATE_MAX, ELM_RUNTIME_LOG_MESSAGE_LEN, ELM_TODO_FLAG_ACTIVE,
    ELM_TODO_FLAG_STATIC, ELM_TODO_KIND_FRAMEWORK, ELM_TODO_KIND_NATIVE, ELM_TODO_KIND_RUNTIME,
    ELM_TODO_KIND_SOURCE, ElmActionInvokeReply, ElmActionInvokeRequest, ElmCallFrame, ElmContext,
    ElmCoreHealthHeader, ElmCoreHealthRecord, ElmCoreInfo, ElmEbiArch, ElmEbiExtensionPointDecl,
    ElmEbiImage, ElmEbiLifecycleHooks, ElmEbiLoadStatus, ElmEbiProviderPortDecl, ElmEbiSourceKind,
    ElmEbiTarget, ElmEbiUnit, ElmError, ElmEventRecord, ElmEventSequence, ElmId, ElmKind,
    ElmLifecycleAction, ElmLifecyclePhase, ElmLifecyclePlanRequest, ElmLifecyclePlanResponse,
    ElmLifecycleResponse, ElmLoadCellResponse, ElmManifest, ElmMenuItemKind, ElmMgrApiDescriptor,
    ElmMgrApiRegistryHeader, ElmMgrAuditHeader, ElmMgrAuditRecord, ElmMgrCallKind,
    ElmMgrEventSubscribeRequest, ElmMgrEventSubscribeResponse, ElmMgrEventSubscriptionHeader,
    ElmMgrEventSubscriptionRecord, ElmMgrEventUnsubscribeRequest, ElmMgrEventUnsubscribeResponse,
    ElmMgrPolicyInfo, ElmMgrRelationKind, ElmMgrRelationRecord, ElmMgrSubscribedEventReadHeader,
    ElmMgrSubscribedEventReadRequest, ElmMgrTopologyHeader, ElmName, ElmNativeCapabilityHeader,
    ElmNativeCapabilityRecord, ElmNexusBindPlanResponse, ElmNexusBindRequest,
    ElmNexusBindingRecord, ElmNexusBindingSnapshotHeader, ElmNexusUnbindRequest,
    ElmPortAccessPolicy, ElmProviderAsyncCancelRequest, ElmProviderAsyncCancelResponse,
    ElmProviderAsyncPollRequest, ElmProviderAsyncPollResponse, ElmProviderAsyncState,
    ElmProviderAsyncSubmitRequest, ElmProviderAsyncSubmitResponse, ElmProviderInvokeRequest,
    ElmProviderInvokeResponse, ElmProviderPortRecord, ElmProviderPortRegisterRequest,
    ElmProviderPortRegisterResponse, ElmProviderPortStatsHeader, ElmProviderPortStatsRecord,
    ElmProviderPortUnregisterRequest, ElmProviderQueueStatsHeader, ElmProviderQueueStatsRecord,
    ElmProviderSnapshotHeader, ElmProviderSnapshotRequest, ElmReplaceCellResponseV1, ElmReplyFrame,
    ElmResourceBudget, ElmResourceKind, ElmResourceUsage, ElmResult, ElmRuntimeEventRequest,
    ElmRuntimeEventResponse, ElmRuntimeLogRequest, ElmRuntimeLogResponse,
    ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord, ElmState, ElmTodoRegistryHeader,
    ElmTodoRegistryRecord, ElmVersion, FlowContract, FlowDirection, FlowMode, Generation, LeaseId,
    LeaseKind, LeaseRegistry, LeaseRights, NexusOffer, PortId, ResourceLease, TopologyEventKind,
    builtin_port_descriptors, first_lifecycle_reason, planned_final_state, state_code,
    status_from_blockers,
};
use elm_model::{
    ELM_MGR_API_FLAG_STABLE, ELM_MGR_API_FLAG_SYSCALL, ELM_MGR_API_FLAG_SYSFS,
    ELM_MGR_API_KIND_EVENT, ELM_MGR_API_KIND_PROVIDER, ELM_MGR_API_KIND_SNAPSHOT,
    ELM_MGR_EVENT_READ_ABSOLUTE_MAX_RECORDS, ELM_MGR_EVENT_READ_DEFAULT_MAX_RECORDS,
    ELM_MGR_EVENT_READ_FLAG_ADVANCE, ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED,
    ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD, ELM_NATIVE_CAPABILITY_KIND_EXPORT,
    ELM_NATIVE_CAPABILITY_KIND_IMPORT, ELM_TODO_REGISTRY_FLAG_TRUNCATED, ElmKernelProviderSpec,
};
use sched::sync::Spinlock;

use super::menu::MenuItemRuntime;
use super::native::LoadedElmImage;
use super::ports::PortRuntime;

pub(crate) const ELM_MGR_ID: ElmId = ELM_MGR_BUILTIN_ID;
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
const FIRST_PROVIDER_TICKET_ID: u64 = 1;
const PROVIDER_RESULT_RING_LIMIT: usize = ELM_PROVIDER_ASYNC_QUEUE_LIMIT as usize;
const FIRST_EVENT_SUBSCRIPTION_ID: u64 = 1;
const EVENT_SUBSCRIPTION_LIMIT: usize = 64;

static CORE: Spinlock<ElmCore> = Spinlock::new(ElmCore::new());

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
    pub lifecycle_hooks_declared: bool,
    pub lifecycle_executor_ready: bool,
    pub lifecycle_initialized: bool,
    pub lifecycle_finalized: bool,
    pub resource_budget: ElmResourceBudget,
    pub native_faults: u16,
    pub isolated: bool,
    pub isolation_blocker: u64,
    pub owned_bindings: Vec<BindingId>,
    pub owned_menu_items: Vec<u64>,
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
    handler: usize,
    snapshot: Option<usize>,
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
    extensions: Vec<(ElmId, String, FlowContract)>,
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

#[derive(Debug, Clone)]
struct PendingEbiLoad {
    cell: ElmId,
    unit: ElmEbiUnit,
    topology: ResolvedEbiTopology,
}

#[derive(Debug, Clone)]
struct NativeExportRuntime {
    owner: ElmId,
    name: String,
    contract: FlowContract,
    version: u32,
    address: usize,
}

#[derive(Debug, Clone)]
struct NativeImportRuntime {
    owner: ElmId,
    provider: ElmId,
    import_index: u32,
    name: String,
    contract: FlowContract,
    requested_version: u32,
    selected_version: u32,
    address: usize,
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
struct ProviderSnapshotPageResult {
    status: i32,
    payload_len: usize,
    record_count: u32,
    flags: u32,
    next_cursor: u32,
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

    fn register_api(&mut self, descriptor: ElmMgrApiDescriptor) {
        if self
            .api_registry
            .iter()
            .any(|existing| existing.id == descriptor.id)
        {
            return;
        }
        self.api_registry.push(descriptor);
        self.api_generation = self.api_generation.next();
    }

    fn alloc_event_subscription_id(&mut self) -> u64 {
        let id = self.next_event_subscription_id;
        self.next_event_subscription_id = self.next_event_subscription_id.saturating_add(1);
        if self.next_event_subscription_id == 0 {
            self.next_event_subscription_id = FIRST_EVENT_SUBSCRIPTION_ID;
        }
        id
    }

    fn alloc_kernel_provider_api_id(&mut self) -> u64 {
        let id = self.next_kernel_provider_api_id;
        self.next_kernel_provider_api_id = self.next_kernel_provider_api_id.saturating_add(1);
        if self.next_kernel_provider_api_id == 0 {
            self.next_kernel_provider_api_id = FIRST_KERNEL_PROVIDER_API_ID;
        }
        id
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
    mgr_runtime: ElmMgrRuntime,
    graph: BindingGraph,
    cells: Vec<CellRuntime>,
    pending_ebi_loads: Vec<PendingEbiLoad>,
    native_images: Vec<LoadedElmImage>,
    native_exports: Vec<NativeExportRuntime>,
    native_imports: Vec<NativeImportRuntime>,
    ports: Vec<PortRuntime>,
    providers: Vec<ProviderRuntime>,
    provider_jobs: VecDeque<ProviderAsyncJob>,
    provider_running: Vec<ProviderRunningCall>,
    provider_results: VecDeque<ProviderAsyncResult>,
    runtime_ports: Vec<RuntimePortBinding>,
    menu_items: Vec<MenuItemRuntime>,
    mgr_actions: Vec<MgrActionRuntime>,
    menu_generation: Generation,
    leases: LeaseRegistry,
    events: Vec<ElmEventRecord>,
    next_event_sequence: ElmEventSequence,
    acknowledged_event_sequence: u64,
    audits: Vec<ElmMgrAuditRecord>,
    next_audit_sequence: u64,
    dropped_audit_count: u32,
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
}

impl ElmCore {
    pub const fn new() -> Self {
        Self {
            initialized: false,
            mgr_runtime: ElmMgrRuntime::new(),
            graph: BindingGraph::new(),
            cells: Vec::new(),
            pending_ebi_loads: Vec::new(),
            native_images: Vec::new(),
            native_exports: Vec::new(),
            native_imports: Vec::new(),
            ports: Vec::new(),
            providers: Vec::new(),
            provider_jobs: VecDeque::new(),
            provider_running: Vec::new(),
            provider_results: VecDeque::new(),
            runtime_ports: Vec::new(),
            menu_items: Vec::new(),
            mgr_actions: Vec::new(),
            menu_generation: Generation::FIRST,
            leases: LeaseRegistry::new(),
            events: Vec::new(),
            next_event_sequence: ElmEventSequence::FIRST,
            acknowledged_event_sequence: 0,
            audits: Vec::new(),
            next_audit_sequence: 1,
            dropped_audit_count: 0,
            next_cell_id: FIRST_DYNAMIC_CELL_ID,
            next_port_id: FIRST_DYNAMIC_PORT_ID,
            next_binding_id: 100,
            next_lease_id: 100,
            next_action_id: 100,
            next_menu_item_id: 100,
            next_provider_ticket_id: FIRST_PROVIDER_TICKET_ID,
        }
    }

    pub fn init_builtin_mgr(&mut self) -> Result<(), ElmError> {
        if self.initialized {
            return Ok(());
        }

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
            self.graph.add_extension_point(
                ELM_MGR_ID,
                point.point.clone(),
                point.contract.clone(),
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
            lifecycle_hooks_declared: unit.lifecycle_hooks.is_some(),
            lifecycle_executor_ready: true,
            lifecycle_initialized: true,
            lifecycle_finalized: false,
            resource_budget: ElmResourceBudget::ROOT,
            native_faults: 0,
            isolated: false,
            isolation_blocker: 0,
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        self.emit(TopologyEventKind::CellAdded, Some(ELM_MGR_ID));
        self.emit(TopologyEventKind::CellStateChanged, Some(ELM_MGR_ID));
        self.register_builtin_ports();
        self.register_builtin_mgr_actions()?;
        self.register_builtin_mgr_api();
        self.initialized = true;
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
        self.next_event_sequence.0.saturating_sub(1)
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
            let flags = if import.requested_version == 0 {
                ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD
            } else {
                0
            };
            let record = ElmNativeCapabilityRecord::new(
                ELM_NATIVE_CAPABILITY_KIND_IMPORT,
                ELM_MGR_STATUS_OK,
                import.owner.0,
                import.provider.0,
                import.requested_version,
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

        let subscription = self.mgr_runtime.alloc_event_subscription_id();
        let lease = self.alloc_lease_id();
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

        let paged = request.is_paged();
        let cursor = request.cursor();
        let audit_cell = binding
            .as_ref()
            .map(|edge| edge.consumer)
            .or(self.providers[provider_index].owner)
            .unwrap_or(ElmId(0));
        let backend = self.providers[provider_index].backend;
        let capacity =
            ELM_MGR_MAX_PAYLOAD.saturating_sub(core::mem::size_of::<ElmProviderSnapshotHeader>());
        let mut payload = Vec::new();
        payload.resize(capacity, 0);

        let page = match backend {
            ProviderBackend::KernelOps(spec) => {
                if paged {
                    match spec.snapshot_paged {
                        Some(snapshot) => match snapshot(cursor, &mut payload) {
                            Ok(page)
                                if provider_snapshot_page_is_valid(
                                    page.payload_len,
                                    capacity,
                                    page.flags,
                                    page.next_cursor,
                                    paged,
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
                            Ok(_) => {
                                ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_INVALID)
                            }
                            Err(status) => ProviderSnapshotPageResult::status_only(status),
                        },
                        None => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED),
                    }
                } else {
                    match spec.snapshot {
                        Some(snapshot) => match snapshot(&mut payload) {
                            Ok(len) if len <= capacity => {
                                ProviderSnapshotPageResult::new(ELM_MGR_STATUS_OK, len, 0, 0, 0)
                            }
                            Ok(_) => {
                                ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_INVALID)
                            }
                            Err(status) => ProviderSnapshotPageResult::status_only(status),
                        },
                        None => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED),
                    }
                }
            }
            ProviderBackend::ElmNativeTodo => {
                ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_TODO)
            }
            ProviderBackend::ElmNative(native) => match native.snapshot {
                Some(snapshot) => {
                    let binding_id = binding.as_ref().map(|edge| edge.id.0).unwrap_or(0);
                    let lease = binding
                        .as_ref()
                        .and_then(|edge| edge.lease)
                        .unwrap_or(LeaseId(0));
                    let (status, len, record_count, flags, next_cursor) =
                        super::native::invoke_provider_snapshot(
                            snapshot,
                            native.owner,
                            port,
                            binding_id,
                            lease,
                            request.flags,
                            cursor,
                            &mut payload,
                        );
                    if status == ELM_MGR_STATUS_OK
                        && provider_snapshot_page_is_valid(
                            len,
                            capacity,
                            flags,
                            next_cursor,
                            paged,
                            cursor,
                        )
                    {
                        ProviderSnapshotPageResult::new(
                            status,
                            len,
                            record_count,
                            flags,
                            next_cursor,
                        )
                    } else if status == ELM_MGR_STATUS_OK {
                        self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED);
                        ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_INVALID)
                    } else if matches!(status, ELM_MGR_STATUS_INVALID) {
                        self.mark_native_fault(native.owner, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED);
                        ProviderSnapshotPageResult::status_only(status)
                    } else {
                        ProviderSnapshotPageResult::status_only(status)
                    }
                }
                None => ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED),
            },
            ProviderBackend::Kernel(_) => {
                ProviderSnapshotPageResult::status_only(ELM_MGR_STATUS_UNSUPPORTED)
            }
        };
        payload.truncate(page.payload_len);

        if page.status == ELM_MGR_STATUS_OK {
            self.providers[provider_index].calls =
                self.providers[provider_index].calls.saturating_add(1);
        } else {
            self.providers[provider_index].failed_calls = self.providers[provider_index]
                .failed_calls
                .saturating_add(1);
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
        Ok(out)
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

        let ticket = self.alloc_provider_ticket_id();
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
            let result = self
                .provider_results
                .remove(index)
                .expect("result index valid");
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
            let job = self.provider_jobs.remove(index).expect("job index valid");
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
        !self.provider_jobs.is_empty()
    }

    pub(crate) fn expire_provider_jobs_at(&mut self, now_ns: u64) -> usize {
        let mut expired = 0usize;
        let mut index = 0usize;
        while index < self.provider_jobs.len() {
            if self.provider_jobs[index].deadline_ns > now_ns {
                index += 1;
                continue;
            }
            let job = self.provider_jobs.remove(index).expect("job index valid");
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
        self.cleanup_provider_results_at(now_ns);
        if self.expire_provider_jobs_at(now_ns) != 0 {
            return true;
        }

        let Some(job_index) = self.next_runnable_provider_job_index() else {
            return false;
        };
        let job = self
            .provider_jobs
            .remove(job_index)
            .expect("job index valid");
        let Some(provider_index) = self.provider_index(job.port) else {
            self.finish_provider_async_job(
                job,
                ElmProviderAsyncState::Failed,
                ELM_MGR_STATUS_NOT_FOUND,
                ElmReplyFrame::empty(0, 0, ELM_CALL_STATUS_NOT_FOUND),
                ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
                now_ns,
            );
            return true;
        };
        self.begin_provider_running_call(job.clone(), provider_index, now_ns);

        let (state, status, reply, blockers) = self.execute_provider_async_job(&job);
        let finish_ns = sched::now_ns_public();
        self.finish_provider_running_call(job.ticket, state, status, reply, blockers, finish_ns);
        true
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
        let job = self
            .provider_jobs
            .remove(job_index)
            .expect("job index valid");
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
        self.begin_provider_running_call(job, provider_index, now_ns);
        true
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
        } else if !matches!(
            owner_state.unwrap(),
            ElmState::Loaded | ElmState::Linked | ElmState::Ready | ElmState::Active
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

        let port = self.alloc_port_id();
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
        let reply = match backend {
            ProviderBackend::Kernel(kind) => self.invoke_kernel_provider(kind, &edge, frame),
            ProviderBackend::KernelOps(spec) => Ok((spec.invoke)(frame)),
            ProviderBackend::ElmNative(native) => {
                Ok(self.invoke_native_provider(native, &edge, lease, frame, 0))
            }
            ProviderBackend::ElmNativeTodo => {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
                Err(ELM_MGR_STATUS_TODO)
            }
        }?;
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
        Ok(ElmProviderInvokeResponse::new(reply))
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

    fn invoke_native_provider(
        &self,
        native: NativeProviderBackend,
        edge: &elm_model::CapabilityBindingEdge,
        lease: LeaseId,
        frame: ElmCallFrame,
        deadline_ns: u64,
    ) -> ElmReplyFrame {
        super::native::invoke_provider_handler(
            native.handler,
            native.owner,
            edge.port,
            lease,
            frame,
            deadline_ns,
        )
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

        let binding = self.alloc_binding_id();
        let lease = self.alloc_lease_id();
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
        let dependents = self.graph.dependents_of(id).len();
        let extensions = self.graph.extensions_targeting(id).len();
        let mut blockers = 0;

        if self.is_builtin_cell(id) {
            blockers |= ELM_POLICY_BLOCK_BUILTIN_PROTECTED;
        }
        if self.graph.validate().is_err() {
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
            }
            ElmLifecycleAction::Replace => {
                if !matches!(current, ElmState::Active | ElmState::Paused) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
                if children != 0 {
                    blockers |= ELM_POLICY_BLOCK_HAS_CHILDREN;
                }
                if self.provider_runtime_busy_owned_by(id) != 0
                    || self.provider_busy_owned_by(id) != 0
                {
                    blockers |= ELM_POLICY_BLOCK_PROVIDER_BUSY;
                }
                if self.leases.busy_owned_by(id) != 0 {
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

    // 内核内部测试和内建路径可直接提交 EBI unit；外部通道必须先经过 Source 分发。
    #[allow(dead_code)]
    pub fn load_ebi_unit(&mut self, unit: ElmEbiUnit, arch: ElmEbiArch) -> ElmLoadCellResponse {
        self.load_ebi_unit_inner(unit, arch, None)
    }

    pub fn load_ebi_image(&mut self, image: ElmEbiImage, arch: ElmEbiArch) -> ElmLoadCellResponse {
        if let Err(status) = image.validate(arch) {
            return ElmLoadCellResponse::failed(status);
        }
        if !image.has_code_segment() {
            return self.load_ebi_unit_inner(image.unit, arch, None);
        }
        let mut topology = match self.preflight_ebi_topology(&image.unit) {
            Ok(topology) => topology,
            Err(err) => {
                log::error!("[elm] EBI image topology rejected by runtime: {:?}", err);
                return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
            }
        };
        let manifest = image.unit.manifest.clone();
        let name = manifest.name.as_str().to_string();
        let image_arch = image.unit.target.arch;
        let id = self.alloc_cell_id();
        let resolved_imports = match self.resolve_native_imports(id, &image.unit) {
            Ok((imports, dependencies, resolved_imports)) => {
                topology.dependencies.extend(dependencies);
                (imports, resolved_imports)
            }
            Err(status) => return ElmLoadCellResponse::failed(status),
        };
        let (imports, resolved_native_imports) = resolved_imports;
        if !self.native_exports_available(&image.unit) {
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        }

        if let Err(err) = self.insert_loaded_cell(
            id,
            manifest,
            name,
            image_arch,
            &image.unit,
            ElmEbiSourceKind::Eki,
        ) {
            log::error!("[elm] EBI image cell rejected by runtime: {:?}", err);
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        }

        let loaded = match LoadedElmImage::load(id, &image, &imports) {
            Ok(loaded) => loaded,
            Err(ElmEbiLoadStatus::NativeCodeTodo) => {
                if self.cell_resource_over_quota(id, ElmResourceKind::PendingLoad) {
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
                    unit: image.unit.clone(),
                    topology,
                });
                return ElmLoadCellResponse::new(
                    ElmEbiLoadStatus::NativeCodeTodo,
                    id.0,
                    state_code(ElmState::Loaded),
                    0,
                );
            }
            Err(status) => {
                self.quarantine_cell_after_hook_failure(id);
                return ElmLoadCellResponse::new(
                    status,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                );
            }
        };
        if !self.native_provider_handlers_available(&image, &loaded) {
            self.quarantine_cell_after_hook_failure(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        }
        let exports = match self.collect_native_exports(id, &image, &loaded) {
            Ok(exports) => exports,
            Err(status) => {
                self.quarantine_cell_after_hook_failure(id);
                return ElmLoadCellResponse::new(
                    status,
                    id.0,
                    state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                );
            }
        };

        let Ok(context) = self.lifecycle_context(id, ElmLifecyclePhase::Initialize) else {
            self.quarantine_cell_after_hook_failure(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        };
        if loaded.on_initialize(&context).is_err() {
            self.quarantine_cell_after_hook_failure(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            );
        }
        if let Err(err) = self.activate_loaded_cell(id, &image.unit, &topology, Some(&loaded)) {
            log::error!("[elm] EBI image activation rejected by runtime: {:?}", err);
            if let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Finalize) {
                let mut executor = loaded.lifecycle_executor();
                let _ = executor.on_finalize(&mut context);
            }
            self.rollback_activated_cell_to_quarantine(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        }
        if loaded
            .on_entry(
                self.cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .and_then(|cell| cell.parent),
                self.cells
                    .iter()
                    .find(|cell| cell.id == id)
                    .map(|cell| cell.generation)
                    .unwrap_or(Generation::FIRST),
                self.cell_state(id).unwrap_or(ElmState::Active),
            )
            .is_err()
        {
            if let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Finalize) {
                let mut executor = loaded.lifecycle_executor();
                let _ = executor.on_finalize(&mut context);
            }
            self.rollback_activated_cell_to_quarantine(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            );
        }
        if self.cell_resource_over_quota(id, ElmResourceKind::NativeImage) {
            if let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Finalize) {
                let mut executor = loaded.lifecycle_executor();
                let _ = executor.on_finalize(&mut context);
            }
            self.rollback_activated_cell_to_quarantine(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_LEASE_BUSY,
            );
        }
        self.native_exports.extend(exports);
        self.native_imports.extend(resolved_native_imports);
        if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) {
            cell.ebi_status = ElmEbiLoadStatus::Ok;
            cell.lifecycle_executor_ready = true;
            cell.lifecycle_initialized = true;
            cell.lifecycle_finalized = false;
        }
        self.native_images.push(loaded);
        ElmLoadCellResponse::new(ElmEbiLoadStatus::Ok, id.0, state_code(ElmState::Active), 0)
    }

    pub fn replace_cell_from_ebi_image(
        &mut self,
        id: ElmId,
        image: ElmEbiImage,
        arch: ElmEbiArch,
        migration_limit: u32,
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
        if !image.has_code_segment() {
            return fail(
                self,
                ELM_MGR_STATUS_TODO,
                ELM_POLICY_BLOCK_NATIVE_TODO,
                ELM_LIFECYCLE_REASON_NATIVE_TODO,
            );
        }
        let Some(old_cell_index) = self.cell_index(id) else {
            return fail(
                self,
                ELM_MGR_STATUS_NOT_FOUND,
                ELM_POLICY_BLOCK_CELL_NOT_FOUND,
                ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
            );
        };
        let old_cell = self.cells[old_cell_index].clone();
        if image.unit.manifest.name.as_str() != old_cell.name
            || image.unit.manifest.kind != old_cell.kind
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

        let new_generation = old_generation.next();
        let resolved_imports = match self.resolve_native_imports(id, &image.unit) {
            Ok((imports, dependencies, resolved_imports)) => {
                if dependencies.iter().any(|(provider, _)| *provider == id)
                    || resolved_imports.iter().any(|import| import.provider == id)
                {
                    return fail(
                        self,
                        ELM_MGR_STATUS_INVALID,
                        ELM_POLICY_BLOCK_HAS_DEPENDENTS,
                        ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
                    );
                }
                (imports, resolved_imports)
            }
            Err(_) => {
                return fail(
                    self,
                    ELM_MGR_STATUS_INVALID,
                    ELM_POLICY_BLOCK_HAS_DEPENDENTS,
                    ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
                );
            }
        };
        let (imports, resolved_native_imports) = resolved_imports;
        if !self.native_exports_available_for_replace(id, &image.unit) {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
            );
        }

        let loaded = match LoadedElmImage::load(id, &image, &imports) {
            Ok(loaded) => loaded,
            Err(ElmEbiLoadStatus::NativeCodeTodo) => {
                return fail(
                    self,
                    ELM_MGR_STATUS_TODO,
                    ELM_POLICY_BLOCK_NATIVE_TODO,
                    ELM_LIFECYCLE_REASON_NATIVE_TODO,
                );
            }
            Err(_) => {
                return fail(
                    self,
                    ELM_MGR_STATUS_INVALID,
                    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                    ELM_LIFECYCLE_REASON_HOOK_FAILED,
                );
            }
        };
        if !self.native_provider_handlers_available(&image, &loaded) {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
            );
        }
        let exports = match self.collect_native_exports(id, &image, &loaded) {
            Ok(exports) => exports,
            Err(_) => {
                return fail(
                    self,
                    ELM_MGR_STATUS_INVALID,
                    ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
                );
            }
        };
        if !self.can_rebind_native_importers_for_replace(id, &exports) {
            return fail(
                self,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_HAS_DEPENDENTS,
                ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
            );
        }

        let Some(old_image_index) = self.native_image_index(id) else {
            return fail(
                self,
                ELM_MGR_STATUS_TODO,
                ELM_POLICY_BLOCK_NATIVE_TODO,
                ELM_LIFECYCLE_REASON_NATIVE_TODO,
            );
        };
        let mut old_executor = self.native_images[old_image_index].lifecycle_executor();
        let mut new_executor = loaded.lifecycle_executor();
        let Ok(new_init_context) = self.lifecycle_context_for_generation(
            id,
            new_generation,
            ElmLifecyclePhase::Initialize,
        ) else {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
            );
        };
        if loaded.on_initialize(&new_init_context).is_err() {
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            );
        }

        let mut migration = Vec::new();
        if migration.try_reserve_exact(migration_capacity).is_err() {
            let _ = new_executor.on_finalize(&mut self.lifecycle_context_for_generation_lossy(
                id,
                new_generation,
                ElmLifecyclePhase::Finalize,
            ));
            return fail(
                self,
                ELM_MGR_STATUS_BUSY,
                ELM_POLICY_BLOCK_NATIVE_TODO,
                ELM_LIFECYCLE_REASON_NATIVE_TODO,
            );
        }
        migration.resize(migration_capacity, 0);

        if old_state == ElmState::Active {
            let mut quiesce_context = self.lifecycle_context_for_generation_lossy(
                id,
                old_generation,
                ElmLifecyclePhase::Quiesce,
            );
            if old_executor.on_quiesce(&mut quiesce_context).is_err() {
                let _ = new_executor.on_migrate_abort(
                    id,
                    old_generation,
                    new_generation,
                    &mut migration,
                    0,
                );
                let _ = new_executor.on_finalize(&mut self.lifecycle_context_for_generation_lossy(
                    id,
                    new_generation,
                    ElmLifecyclePhase::Finalize,
                ));
                self.quarantine_cell_after_hook_failure(id);
                return self.replace_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    self.cell_state(id).unwrap_or(ElmState::Quarantined),
                    old_generation,
                    0,
                    ELM_LIFECYCLE_REASON_HOOK_FAILED,
                    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                );
            }
        }
        let migrated_len = match old_executor.on_migrate_export(
            id,
            old_generation,
            new_generation,
            &mut migration,
        ) {
            Ok(len) if len <= migration_capacity => len,
            _ => {
                let _ = new_executor.on_migrate_abort(
                    id,
                    old_generation,
                    new_generation,
                    &mut migration,
                    0,
                );
                let _ = new_executor.on_finalize(&mut self.lifecycle_context_for_generation_lossy(
                    id,
                    new_generation,
                    ElmLifecyclePhase::Finalize,
                ));
                return fail(
                    self,
                    ELM_MGR_STATUS_INVALID,
                    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                    ELM_LIFECYCLE_REASON_HOOK_FAILED,
                );
            }
        };
        if new_executor
            .on_migrate_import(
                id,
                old_generation,
                new_generation,
                &mut migration,
                migrated_len,
            )
            .is_err()
        {
            let _ = new_executor.on_migrate_abort(
                id,
                old_generation,
                new_generation,
                &mut migration,
                migrated_len,
            );
            let _ = new_executor.on_finalize(&mut self.lifecycle_context_for_generation_lossy(
                id,
                new_generation,
                ElmLifecyclePhase::Finalize,
            ));
            return fail(
                self,
                ELM_MGR_STATUS_INVALID,
                ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            );
        }

        let mut old_finalize_context = self.lifecycle_context_for_generation_lossy(
            id,
            old_generation,
            ElmLifecyclePhase::Finalize,
        );
        if old_executor.on_finalize(&mut old_finalize_context).is_err() {
            let _ = new_executor.on_migrate_abort(
                id,
                old_generation,
                new_generation,
                &mut migration,
                migrated_len,
            );
            let _ = new_executor.on_finalize(&mut self.lifecycle_context_for_generation_lossy(
                id,
                new_generation,
                ElmLifecyclePhase::Finalize,
            ));
            self.quarantine_cell_after_hook_failure(id);
            return self.replace_response(
                id,
                ELM_MGR_STATUS_INVALID,
                self.cell_state(id).unwrap_or(ElmState::Quarantined),
                old_generation,
                migrated_len as u32,
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
                ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED,
            );
        }

        self.commit_replaced_cell(
            id,
            old_state,
            new_generation,
            &image.unit,
            &loaded,
            exports,
            resolved_native_imports,
        );
        self.remove_native_image(id);
        self.native_images.push(loaded);
        self.replace_response(
            id,
            ELM_MGR_STATUS_OK,
            old_state,
            new_generation,
            migrated_len as u32,
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
        self.load_ebi_unit_inner(unit, arch, Some(executor))
    }

    fn load_ebi_unit_inner(
        &mut self,
        unit: ElmEbiUnit,
        arch: ElmEbiArch,
        mut executor: Option<&mut dyn ElmLifecycleExecutor>,
    ) -> ElmLoadCellResponse {
        if let Err(status) = unit.validate(arch) {
            return ElmLoadCellResponse::failed(status);
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
        let id = self.alloc_cell_id();

        if let Err(err) = self.insert_loaded_cell(
            id,
            manifest,
            name,
            image_arch,
            &unit,
            ElmEbiSourceKind::Memory,
        ) {
            log::error!("[elm] EBI cell rejected by runtime: {:?}", err);
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
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
            self.pending_ebi_loads.push(PendingEbiLoad {
                cell: id,
                unit: unit.clone(),
                topology: topology.clone(),
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

        if let Err(err) = self.activate_loaded_cell(id, &unit, &topology, None) {
            log::error!("[elm] EBI cell activation rejected by runtime: {:?}", err);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Loaded)),
                0,
            );
        }

        ElmLoadCellResponse::new(ElmEbiLoadStatus::Ok, id.0, state_code(ElmState::Active), 0)
    }

    fn initialize_pending_ebi_load(
        &mut self,
        id: ElmId,
        executor: &mut dyn ElmLifecycleExecutor,
    ) -> ElmLoadCellResponse {
        let Some(index) = self.pending_ebi_load_index(id) else {
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        };
        let pending = self.pending_ebi_loads[index].clone();
        let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Initialize) else {
            self.remove_pending_ebi_load(id);
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        };

        if executor.on_initialize(&mut context).is_err() {
            self.quarantine_cell_after_hook_failure(id);
            self.remove_pending_ebi_load(id);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Quarantined)),
                ELM_LIFECYCLE_REASON_HOOK_FAILED,
            );
        }

        if let Err(err) = self.activate_loaded_cell(id, &pending.unit, &pending.topology, None) {
            log::error!("[elm] EBI cell activation rejected by runtime: {:?}", err);
            self.quarantine_cell_after_hook_failure(id);
            self.remove_pending_ebi_load(id);
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
        self.remove_pending_ebi_load(id);
        ElmLoadCellResponse::new(ElmEbiLoadStatus::Ok, id.0, state_code(ElmState::Active), 0)
    }

    pub fn pause_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        let action = ElmLifecycleAction::Pause;
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(id.0, action));
        if plan.allowed == 0 {
            return self.lifecycle_response_from_plan(action, plan, 0, 0);
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Active => {
                if let Some(index) = self.native_image_index(id) {
                    let mut executor = self.native_images[index].lifecycle_executor();
                    let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Quiesce)
                    else {
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
                    if executor.on_quiesce(&mut context).is_err() {
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
                }
                if self.transition_cell_state(id, ElmState::Quiescing).is_err() {
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
                if let Some(index) = self.native_image_index(id) {
                    let mut executor = self.native_images[index].lifecycle_executor();
                    let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Pause)
                    else {
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
                    if executor.on_pause(&mut context).is_err() {
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
                }
                if self.transition_cell_state(id, ElmState::Paused).is_err() {
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
                let response =
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0);
                self.finish_lifecycle(action, response, 0)
            }
            ElmState::Paused => {
                let response =
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0);
                self.finish_lifecycle(action, response, 0)
            }
            _ => {
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_INVALID_STATE),
                    0,
                    0,
                );
                self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_INVALID_STATE)
            }
        }
    }

    pub fn resume_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        let action = ElmLifecycleAction::Resume;
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(id.0, action));
        if plan.allowed == 0 {
            return self.lifecycle_response_from_plan(action, plan, 0, 0);
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Paused => {
                if let Some(index) = self.native_image_index(id) {
                    let mut executor = self.native_images[index].lifecycle_executor();
                    let Ok(mut context) = self.lifecycle_context(id, ElmLifecyclePhase::Resume)
                    else {
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
                    if executor.on_resume(&mut context).is_err() {
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
                }
                if self.transition_cell_state(id, ElmState::Active).is_err() {
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
                let response =
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0);
                self.finish_lifecycle(action, response, 0)
            }
            ElmState::Active => {
                let response =
                    self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0);
                self.finish_lifecycle(action, response, 0)
            }
            _ => {
                let response = self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    first_lifecycle_reason(ELM_POLICY_BLOCK_INVALID_STATE),
                    0,
                    0,
                );
                self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_INVALID_STATE)
            }
        }
    }

    pub fn detach_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        if let Some(index) = self.native_image_index(id) {
            let mut executor = self.native_images[index].lifecycle_executor();
            let response = self.detach_cell_inner(id, Some(&mut executor));
            if response.status == ELM_MGR_STATUS_OK {
                self.remove_native_image(id);
            }
            response
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

        if self.cell_needs_finalize(id) {
            let Some(executor) = executor.as_deref_mut() else {
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

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Active => {
                if self.transition_cell_state(id, ElmState::Quiescing).is_err() {
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
        self.remove_pending_ebi_load(id);

        if self.graph.remove_cell(id).is_err() {
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
            let response = self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                revoked_leases.len() as u32,
                removed_menu_items as u32,
            );
            return self.finish_lifecycle(action, response, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT);
        }
        self.remove_cell_runtime(id);
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
            "source.remote_soyo",
            "Remote Source 和 soyo EBI 产出层属于后续文件分发主线",
        ));
        records.push(todo_record(
            ELM_TODO_KIND_NATIVE,
            static_flags,
            ELM_POLICY_BLOCK_NATIVE_TODO,
            0,
            "native.trap_trampoline",
            "ELM protected-call guard 已接入，trap frame 重定向 trampoline 仍需架构层完成",
        ));
        records.push(todo_record(
            ELM_TODO_KIND_FRAMEWORK,
            static_flags,
            ELM_POLICY_BLOCK_NATIVE_TODO,
            0,
            "framework.rust_elm",
            "外部 Rust ELM 开发框架和构建链路仍未纳入内核仓库实现",
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
            if provider.in_flight as usize != self.provider_running_count(provider.port)
                || provider.in_flight > provider.max_in_flight
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
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
            if !self.cell_exists(export.owner) {
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
            if !self.cell_exists(import.owner) || !self.cell_exists(import.provider) {
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

    pub fn debug_dump_bytes(&self) -> Vec<u8> {
        let (health_status, health_records) = self.health_records();
        let health_failures = health_records
            .iter()
            .filter(|record| record.status != ELM_MGR_STATUS_OK)
            .count();
        let mut out = format!(
            "ELM Core 诊断\ncells={}\nports={}\nproviders={}\nbindings={}\nleases={}\nruntime_ports={}\nmenu_items={}\nnative_exports={}\nnative_imports={}\nlast_event_sequence={}\nhealth_status={}\nhealth_records={}\nhealth_failures={}\n",
            self.cells.len(),
            self.ports.len(),
            self.providers.len(),
            self.graph.capability_bindings().len(),
            self.lease_count(),
            self.runtime_ports.len(),
            self.menu_items.len(),
            self.native_exports.len(),
            self.native_imports.len(),
            self.last_event_sequence(),
            health_status,
            health_records.len(),
            health_failures,
        );
        out.push_str("[cells]\n");
        for cell in &self.cells {
            out.push_str(
                format!(
                    "cell id={} parent={} name={} state={:?} kind={:?} generation={} source={:?} ebi_arch={:?} ebi_status={:?} native_code={} native_segments={} native_imports={} native_exports={} lifecycle_hooks={} lifecycle_executor_ready={} lifecycle_initialized={} lifecycle_finalized={} isolated={} native_faults={} isolation_blocker=0x{:x} pending_loads={} owned_bindings={} owned_menu_items={}\n",
                    cell.id.0,
                    cell.parent.map(|id| id.0).unwrap_or(0),
                    cell.name,
                    cell.state,
                    cell.kind,
                    cell.generation.0,
                    cell.ebi_source,
                    cell.ebi_arch,
                    cell.ebi_status,
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
                    "native_import owner={} provider={} name={} contract={} requested_version={} selected_version={} address=0x{:x}\n",
                    import.owner.0,
                    import.provider.0,
                    import.name,
                    import.contract.as_str(),
                    import.requested_version,
                    import.selected_version,
                    import.address,
                )
                .as_str(),
            );
        }
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
        out.push_str("TODO(elm): Remote/soyo 分发、trap trampoline、子系统 provider 和 Rust 开发框架仍是后续主线。\n");
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
                    "import owner={} provider={} name={} contract={} requested_version={} selected_version={} address=0x{:x}\n",
                    import.owner.0,
                    import.provider.0,
                    import.name,
                    import.contract.as_str(),
                    import.requested_version,
                    import.selected_version,
                    import.address,
                )
                .as_str(),
            );
        }
        out
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
        let action = self.alloc_action_id();
        let menu_item = self.alloc_menu_item_id();
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
        self.menu_generation = self.menu_generation.next();
        self.emit(TopologyEventKind::MenuItemAdded, Some(ELM_MGR_ID));
        Ok(())
    }

    fn register_builtin_mgr_api(&mut self) {
        let stable_syscall = ELM_MGR_API_FLAG_STABLE | ELM_MGR_API_FLAG_SYSCALL;
        let stable_both = stable_syscall | ELM_MGR_API_FLAG_SYSFS;
        for descriptor in [
            mgr_api(
                1,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryPolicy,
                "policy",
                "elm.mgr.policy@1",
            ),
            mgr_api(
                2,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryHealth,
                "health",
                "elm.mgr.health@1",
            ),
            mgr_api(
                3,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryMenu,
                "menu",
                "elm.mgr.menu@1",
            ),
            mgr_api(
                4,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryTopology,
                "topology",
                "elm.mgr.topology@1",
            ),
            mgr_api(
                5,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryNexusBindings,
                "bindings",
                "elm.mgr.bindings@1",
            ),
            mgr_api(
                6,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryAudit,
                "audit",
                "elm.mgr.audit@1",
            ),
            mgr_api(
                7,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryRuntimePorts,
                "runtime.ports",
                "elm.mgr.runtime.ports@1",
            ),
            mgr_api(
                8,
                ELM_MGR_API_KIND_PROVIDER,
                stable_both,
                ElmMgrCallKind::QueryProviderPorts,
                "providers",
                "elm.mgr.providers@1",
            ),
            mgr_api(
                9,
                ELM_MGR_API_KIND_PROVIDER,
                stable_syscall,
                ElmMgrCallKind::QueryProviderStats,
                "provider.stats",
                "elm.mgr.provider.stats@1",
            ),
            mgr_api(
                10,
                ELM_MGR_API_KIND_PROVIDER,
                stable_both,
                ElmMgrCallKind::QueryProviderQueue,
                "provider.queue",
                "elm.mgr.provider.queue@1",
            ),
            mgr_api(
                11,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryApiRegistry,
                "api.registry",
                "elm.mgr.api.registry@1",
            ),
            mgr_api(
                12,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                ElmMgrCallKind::SubscribeEvent,
                "event.subscribe",
                "elm.mgr.event.subscribe@1",
            ),
            mgr_api(
                13,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                ElmMgrCallKind::UnsubscribeEvent,
                "event.unsubscribe",
                "elm.mgr.event.unsubscribe@1",
            ),
            mgr_api(
                14,
                ELM_MGR_API_KIND_EVENT,
                stable_both,
                ElmMgrCallKind::QueryEventSubscriptions,
                "event.subscriptions",
                "elm.mgr.event.subscriptions@1",
            ),
            mgr_api(
                15,
                ELM_MGR_API_KIND_EVENT,
                stable_syscall,
                ElmMgrCallKind::ReadSubscribedEvents,
                "event.read",
                "elm.mgr.event.read@1",
            ),
            mgr_api(
                16,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryNativeCapabilities,
                "native.capabilities",
                "elm.mgr.native.capabilities@1",
            ),
            mgr_api(
                17,
                ELM_MGR_API_KIND_SNAPSHOT,
                stable_both,
                ElmMgrCallKind::QueryTodoRegistry,
                "todo.registry",
                "elm.mgr.todo.registry@1",
            ),
        ] {
            self.mgr_runtime.register_api(descriptor);
        }
    }

    pub(crate) fn register_kernel_provider_specs(
        &mut self,
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

            let port = self.alloc_port_id();
            let desc = spec.port_descriptor(port, ELM_MGR_ID);
            self.register_port(PortRuntime::from_descriptor(desc));
            self.providers.push(ProviderRuntime {
                port,
                owner: Some(ELM_MGR_ID),
                access: spec.access,
                backend: ProviderBackend::KernelOps(spec),
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
            let api_id = self.mgr_runtime.alloc_kernel_provider_api_id();
            self.mgr_runtime
                .register_api(spec.api_descriptor(api_id, ELM_MGR_ID));
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
        manifest: ElmManifest,
        name: String,
        ebi_arch: ElmEbiArch,
        unit: &ElmEbiUnit,
        source: ElmEbiSourceKind,
    ) -> Result<(), ElmError> {
        let kind = manifest.kind;
        self.graph.insert_cell(id, manifest)?;
        self.graph.set_parent(id, ELM_MGR_ID)?;
        self.cells.push(CellRuntime {
            id,
            parent: Some(ELM_MGR_ID),
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
            lifecycle_hooks_declared: unit.lifecycle_hooks.is_some(),
            lifecycle_executor_ready: false,
            lifecycle_initialized: false,
            lifecycle_finalized: false,
            resource_budget: ElmResourceBudget::DEFAULT,
            native_faults: 0,
            isolated: false,
            isolation_blocker: 0,
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        self.emit(TopologyEventKind::CellAdded, Some(id));
        self.transition_cell_state(id, ElmState::Verified)?;
        self.transition_cell_state(id, ElmState::Loaded)?;
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
        for point in &unit.extension_points {
            self.graph
                .add_extension_point(id, point.point.clone(), point.contract.clone())?;
        }
        for (provider, contract) in &topology.dependencies {
            self.graph.add_dependency(id, *provider, contract.clone())?;
        }
        for (target, point, contract) in &topology.extensions {
            self.graph
                .add_extension(id, *target, point.clone(), contract.clone())?;
        }
        for provider in &unit.provider_ports {
            self.register_ebi_provider_port(id, provider, native_image)?;
        }
        if let Some(menu) = &unit.menu {
            let menu_contract = FlowContract::new("mgr.menu.item@1")?;
            self.graph
                .add_extension(id, ELM_MGR_ID, "menu.item", menu_contract.clone())?;
            let binding = self.alloc_binding_id();
            let lease = self.alloc_lease_id();
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
            for point in self.graph.extension_points() {
                if point.owner == target && point.name == extension.point {
                    point_exists = true;
                    if point.contract == extension.contract {
                        contract_matches = true;
                    }
                }
            }
            if !point_exists {
                return Err(ElmError::ExtensionPointNotFound);
            }
            if !contract_matches {
                return Err(ElmError::ContractMismatch);
            }
            topology
                .extensions
                .push((target, extension.point.clone(), extension.contract.clone()));
        }

        let mut provider_contracts = Vec::new();
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
        for dependency in &unit.dependencies {
            let provider = self.resolve_unique_cell_name(&dependency.provider_name)?;
            topology
                .dependencies
                .push((provider, dependency.contract.clone()));
        }
        for extension in &unit.extensions {
            let target = self.resolve_unique_cell_name(&extension.target_name)?;
            topology
                .extensions
                .push((target, extension.point.clone(), extension.contract.clone()));
        }
        Ok(topology)
    }

    fn replace_surface_compatible(
        &self,
        id: ElmId,
        unit: &ElmEbiUnit,
        topology: &ResolvedEbiTopology,
    ) -> bool {
        let current_dependencies: Vec<_> = self
            .graph
            .dependencies()
            .iter()
            .filter(|edge| edge.consumer == id)
            .map(|edge| (edge.provider, edge.contract.clone()))
            .collect();
        if !same_dependency_set(&current_dependencies, &topology.dependencies) {
            return false;
        }

        let current_extensions: Vec<_> = self
            .graph
            .extensions()
            .iter()
            .filter(|edge| edge.extension == id)
            .map(|edge| (edge.target, edge.point.clone(), edge.contract.clone()))
            .collect();
        if !same_extension_set(&current_extensions, &topology.extensions) {
            return false;
        }

        let current_points: Vec<_> = self
            .graph
            .extension_points()
            .into_iter()
            .filter(|point| point.owner == id)
            .map(|point| (point.name, point.contract))
            .collect();
        let requested_points: Vec<_> = unit
            .extension_points
            .iter()
            .map(|point| (point.point.clone(), point.contract.clone()))
            .collect();
        if !same_extension_point_set(&current_points, &requested_points) {
            return false;
        }

        if unit.menu.is_some() != self.menu_items.iter().any(|item| item.owner == id) {
            return false;
        }

        let current_ports: Vec<_> = self
            .providers
            .iter()
            .filter_map(|provider| {
                if provider.dynamic && provider.owner == Some(id) {
                    self.port_desc(provider.port)
                        .map(|port| (port.contract, port.access, port.direction, port.mode))
                } else {
                    None
                }
            })
            .collect();
        let requested_ports: Vec<_> = unit
            .provider_ports
            .iter()
            .map(|port| {
                (
                    port.contract.as_str().to_string(),
                    port.access,
                    port.direction,
                    port.mode,
                )
            })
            .collect();
        if !same_provider_port_set(&current_ports, &requested_ports) {
            return false;
        }

        let current_exports: Vec<_> = self
            .native_exports
            .iter()
            .filter(|export| export.owner == id)
            .map(|export| (export.name.clone(), export.contract.clone(), export.version))
            .collect();
        let requested_exports: Vec<_> = unit
            .exports
            .iter()
            .map(|export| (export.name.clone(), export.contract.clone(), export.version))
            .collect();
        same_native_export_set(&current_exports, &requested_exports)
    }

    fn native_exports_available_for_replace(&self, owner: ElmId, unit: &ElmEbiUnit) -> bool {
        let mut seen: Vec<(&str, &FlowContract, u32)> = Vec::new();
        for export in &unit.exports {
            let key = (export.name.as_str(), &export.contract, export.version);
            if seen.iter().any(|(name, contract, version)| {
                *name == key.0 && *contract == key.1 && *version == key.2
            }) || self.native_exports.iter().any(|existing| {
                existing.owner != owner
                    && existing.name == export.name
                    && existing.contract == export.contract
                    && existing.version == export.version
            }) {
                return false;
            }
            seen.push(key);
        }
        true
    }

    fn resolve_native_imports(
        &self,
        owner: ElmId,
        unit: &ElmEbiUnit,
    ) -> Result<
        (
            Vec<usize>,
            Vec<(ElmId, FlowContract)>,
            Vec<NativeImportRuntime>,
        ),
        ElmEbiLoadStatus,
    > {
        let mut values = Vec::new();
        let mut dependencies = Vec::new();
        let mut imports = Vec::new();
        for (import_index, import) in unit.imports.iter().enumerate() {
            let candidates: Vec<_> = self
                .native_exports
                .iter()
                .filter(|export| export.name == import.name && export.contract == import.contract)
                .collect();
            let export = if import.version == 0 {
                let Some(max_version) = candidates.iter().map(|export| export.version).max() else {
                    return Err(ElmEbiLoadStatus::RuntimeRejected);
                };
                let matches: Vec<_> = candidates
                    .into_iter()
                    .filter(|export| export.version == max_version)
                    .collect();
                if matches.len() != 1 {
                    return Err(ElmEbiLoadStatus::RuntimeRejected);
                }
                matches[0]
            } else {
                let matches: Vec<_> = candidates
                    .into_iter()
                    .filter(|export| export.version == import.version)
                    .collect();
                if matches.len() != 1 {
                    return Err(ElmEbiLoadStatus::RuntimeRejected);
                }
                matches[0]
            };
            if !self.cell_exists(export.owner) {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            if self.cell_is_isolated(export.owner) {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
            values.push(export.address);
            if !dependencies
                .iter()
                .any(|(owner, contract)| *owner == export.owner && *contract == import.contract)
            {
                dependencies.push((export.owner, import.contract.clone()));
            }
            imports.push(NativeImportRuntime {
                owner,
                provider: export.owner,
                import_index: import_index as u32,
                name: import.name.clone(),
                contract: import.contract.clone(),
                requested_version: import.version,
                selected_version: export.version,
                address: export.address,
            });
        }
        Ok((values, dependencies, imports))
    }

    fn native_exports_available(&self, unit: &ElmEbiUnit) -> bool {
        let mut seen: Vec<(&str, &FlowContract, u32)> = Vec::new();
        for export in &unit.exports {
            let key = (export.name.as_str(), &export.contract, export.version);
            if seen.iter().any(|(name, contract, version)| {
                *name == key.0 && *contract == key.1 && *version == key.2
            }) || self.native_exports.iter().any(|existing| {
                existing.name == export.name
                    && existing.contract == export.contract
                    && existing.version == export.version
            }) {
                return false;
            }
            seen.push(key);
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
        image: &ElmEbiImage,
        loaded: &LoadedElmImage,
    ) -> Result<Vec<NativeExportRuntime>, ElmEbiLoadStatus> {
        let mut exports = Vec::new();
        for export in &image.unit.exports {
            exports.push(NativeExportRuntime {
                owner,
                name: export.name.clone(),
                contract: export.contract.clone(),
                version: export.version,
                address: loaded.export_address(&export.name)?,
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

        let port = self.alloc_port_id();
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
                    handler,
                    snapshot,
                }),
                None => ProviderBackend::ElmNativeTodo,
            },
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
    ) {
        self.providers[provider_index].in_flight =
            self.providers[provider_index].in_flight.saturating_add(1);
        self.provider_running.push(ProviderRunningCall {
            job,
            started_at_ns,
            cancel_requested: false,
        });
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
            ProviderBackend::KernelOps(spec) => Ok((spec.invoke)(job.frame)),
            ProviderBackend::ElmNative(native) => Ok(self.invoke_native_provider(
                native,
                &edge,
                job.lease,
                job.frame,
                job.deadline_ns,
            )),
            ProviderBackend::ElmNativeTodo => Err(ELM_MGR_STATUS_TODO),
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
        if let Some(provider_index) = self.provider_index(running.job.port) {
            self.providers[provider_index].in_flight =
                self.providers[provider_index].in_flight.saturating_sub(1);
        }

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
        true
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
            let result = self
                .provider_results
                .remove(index)
                .expect("result index valid");
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
        None
    }

    fn remove_dynamic_providers_owned_by(&mut self, owner: ElmId) -> usize {
        let ports: Vec<_> = self
            .providers
            .iter()
            .filter_map(|provider| {
                if provider.dynamic
                    && provider.owner == Some(owner)
                    && self.provider_binding_count(provider.port) == 0
                    && self.provider_queued_count(provider.port) == 0
                    && self.provider_retained_result_count(provider.port) == 0
                    && self.provider_in_flight_count(provider) == 0
                {
                    Some(provider.port)
                } else {
                    None
                }
            })
            .collect();
        for port in &ports {
            if let Some(index) = self.provider_index(*port) {
                self.providers.remove(index);
            }
            self.ports.retain(|runtime| runtime.id != *port);
            self.record_mgr_audit(
                ELM_MGR_ACTION_PROVIDER_UNREGISTER,
                owner,
                0,
                self.cell_state(owner).map(state_code).unwrap_or(0),
            );
        }
        ports.len()
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
        let mut plans = Vec::new();
        for (import_runtime_index, import) in self.native_imports.iter().enumerate() {
            if import.provider != owner || import.owner == owner {
                continue;
            }
            let Some(export) = new_exports.iter().find(|export| {
                export.owner == owner
                    && export.name == import.name
                    && export.contract == import.contract
                    && export.version == import.selected_version
            }) else {
                return false;
            };
            let Some(image_index) = self.native_image_index(import.owner) else {
                return false;
            };
            if !self.native_images[image_index]
                .can_rebind_import_to(import.import_index, export.address)
            {
                return false;
            }
            plans.push((
                import_runtime_index,
                image_index,
                import.import_index,
                export.address,
            ));
        }

        for (_, image_index, import_index, address) in &plans {
            if self.native_images[*image_index]
                .rebind_import(*import_index, *address)
                .is_err()
            {
                return false;
            }
        }
        for (import_runtime_index, _, _, address) in plans {
            if let Some(import) = self.native_imports.get_mut(import_runtime_index) {
                import.address = address;
            }
        }
        true
    }

    fn can_rebind_native_importers_for_replace(
        &self,
        owner: ElmId,
        new_exports: &[NativeExportRuntime],
    ) -> bool {
        for import in self.native_imports.iter() {
            if import.provider != owner || import.owner == owner {
                continue;
            }
            let Some(export) = new_exports.iter().find(|export| {
                export.owner == owner
                    && export.name == import.name
                    && export.contract == import.contract
                    && export.version == import.selected_version
            }) else {
                return false;
            };
            let Some(image_index) = self.native_image_index(import.owner) else {
                return false;
            };
            if !self.native_images[image_index]
                .can_rebind_import_to(import.import_index, export.address)
            {
                return false;
            }
        }
        true
    }

    fn rollback_activated_cell_to_quarantine(&mut self, id: ElmId) {
        let _ = self.leases.revoke_and_remove_owned_by(id);
        let removed_bindings = self.take_owned_bindings(id);
        for binding in removed_bindings {
            if let Some(edge) = self.graph.capability_binding(binding).cloned() {
                self.note_provider_revoke(&edge);
            }
            self.remove_runtime_binding(binding);
            let _ = self.graph.remove_capability_binding(binding);
            self.emit_binding(TopologyEventKind::BindingRemoved, binding);
        }
        let _ = self.remove_menu_items_owned_by(id);
        let _ = self.remove_dynamic_providers_owned_by(id);
        let _ = self.mgr_runtime.remove_event_subscriptions_owned_by(id);
        let _ = self.remove_native_exports_owned_by(id);
        let _ = self.remove_native_imports_owned_by(id);
        let _ = self.remove_pending_ebi_load(id);
        let _ = self.graph.remove_cell_relations(id);
        self.quarantine_cell_after_hook_failure(id);
    }

    fn commit_replaced_cell(
        &mut self,
        id: ElmId,
        final_state: ElmState,
        generation: Generation,
        unit: &ElmEbiUnit,
        loaded: &LoadedElmImage,
        exports: Vec<NativeExportRuntime>,
        imports: Vec<NativeImportRuntime>,
    ) {
        let _ = self.rebind_native_importers_for_replace(id, &exports);
        self.remove_native_exports_owned_by(id);
        self.remove_native_imports_owned_by(id);
        self.native_exports.extend(exports);
        self.native_imports.extend(imports);
        self.replace_dynamic_provider_backends(id, unit, loaded);
        self.replace_menu_metadata(id, unit);
        self.rewrite_owned_generation(id, generation);
        if let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) {
            cell.state = final_state;
            cell.generation = generation;
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
        }
        self.emit(TopologyEventKind::CellStateChanged, Some(id));
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
        unit: &ElmEbiUnit,
        loaded: &LoadedElmImage,
    ) {
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
                        handler,
                        snapshot,
                    }),
                    None => ProviderBackend::ElmNativeTodo,
                };
            }
        }
    }

    fn replace_menu_metadata(&mut self, owner: ElmId, unit: &ElmEbiUnit) {
        let Some(menu) = &unit.menu else {
            return;
        };
        if let Some(item) = self.menu_items.iter_mut().find(|item| item.owner == owner) {
            item.kind = menu.kind;
            item.flags = menu.flags | ELM_MENU_FLAG_TODO;
            item.label = menu.label.clone();
            item.description = menu.description.clone();
            item.route = menu.route.clone();
            self.menu_generation = self.menu_generation.next();
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
                on_revoke(Some(edge.id), edge.lease);
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

        let action = self.alloc_action_id();
        let menu_id = self.alloc_menu_item_id();
        self.menu_items.push(MenuItemRuntime::new(
            menu_id,
            id,
            action,
            kind,
            flags | ELM_MENU_FLAG_TODO,
            label,
            description,
            route,
        ));
        let cell = &mut self.cells[cell_index];
        cell.owned_bindings.push(binding);
        cell.owned_menu_items.push(menu_id);
        self.menu_generation = self.menu_generation.next();
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

    pub(crate) fn cell_resource_usage(&self, id: ElmId) -> ElmResourceUsage {
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
            .count() as u16;
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

        ElmResourceUsage {
            provider_ports,
            provider_queue,
            event_subscriptions,
            pending_loads,
            native_images,
            native_faults,
            audit_records,
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
        }
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
        Ok(ElmContext::new(
            cell.id,
            cell.parent,
            cell.generation,
            cell.state,
            phase,
            0,
        ))
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
        Ok(ElmContext::new(
            cell.id,
            cell.parent,
            generation,
            cell.state,
            phase,
            0,
        ))
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

    fn remove_pending_ebi_load(&mut self, id: ElmId) -> Option<PendingEbiLoad> {
        self.pending_ebi_load_index(id)
            .map(|index| self.pending_ebi_loads.remove(index))
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

    fn quarantine_cell_after_hook_failure(&mut self, id: ElmId) {
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
        let record = ElmMgrAuditRecord::new(
            self.next_audit_sequence,
            action,
            status,
            cell_id.0,
            blockers,
            final_state,
        );
        self.next_audit_sequence = self.next_audit_sequence.saturating_add(1);
        if self.audits.len() >= AUDIT_RING_LIMIT {
            self.audits.remove(0);
            self.dropped_audit_count = self.dropped_audit_count.saturating_add(1);
        }
        self.audits.push(record);
    }

    fn remove_menu_items_owned_by(&mut self, id: ElmId) -> usize {
        let Some(index) = self.cell_index(id) else {
            return 0;
        };
        let owned = core::mem::take(&mut self.cells[index].owned_menu_items);
        if owned.is_empty() {
            return 0;
        }

        let before = self.menu_items.len();
        self.menu_items
            .retain(|item| !owned.iter().any(|owned_id| *owned_id == item.id));
        let removed = before - self.menu_items.len();
        if removed != 0 {
            self.menu_generation = self.menu_generation.next();
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

    fn remove_cell_runtime(&mut self, id: ElmId) {
        self.cells.retain(|cell| cell.id != id);
    }

    #[allow(dead_code)]
    fn alloc_cell_id(&mut self) -> ElmId {
        let id = ElmId(self.next_cell_id);
        self.next_cell_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_port_id(&mut self) -> PortId {
        let id = PortId(self.next_port_id);
        self.next_port_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_binding_id(&mut self) -> BindingId {
        let id = BindingId(self.next_binding_id);
        self.next_binding_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_lease_id(&mut self) -> LeaseId {
        let id = LeaseId(self.next_lease_id);
        self.next_lease_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_action_id(&mut self) -> ActionId {
        let id = ActionId(self.next_action_id);
        self.next_action_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_menu_item_id(&mut self) -> u64 {
        let id = self.next_menu_item_id;
        self.next_menu_item_id += 1;
        id
    }

    fn alloc_provider_ticket_id(&mut self) -> u64 {
        let id = self.next_provider_ticket_id;
        self.next_provider_ticket_id = self.next_provider_ticket_id.saturating_add(1);
        if self.next_provider_ticket_id == 0 {
            self.next_provider_ticket_id = FIRST_PROVIDER_TICKET_ID;
        }
        id
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
        self.next_event_sequence = self.next_event_sequence.next();
        if self.events.len() >= EVENT_RING_LIMIT {
            self.events.remove(0);
        }
        self.events.push(record);
    }
}

pub(crate) fn with_core<R>(f: impl FnOnce(&mut ElmCore) -> R) -> R {
    let mut core = CORE.lock();
    f(&mut core)
}

fn push_health_ok_if_clean(records: &mut Vec<ElmCoreHealthRecord>, start: usize, check_kind: u32) {
    if records.len() == start {
        records.push(ElmCoreHealthRecord::ok(check_kind));
    }
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

fn same_dependency_set(left: &[(ElmId, FlowContract)], right: &[(ElmId, FlowContract)]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|item| right.iter().any(|other| other == item))
}

fn same_extension_set(
    left: &[(ElmId, String, FlowContract)],
    right: &[(ElmId, String, FlowContract)],
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|item| right.iter().any(|other| other == item))
}

fn same_extension_point_set(
    left: &[(String, FlowContract)],
    right: &[(String, FlowContract)],
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|item| right.iter().any(|other| other == item))
}

fn same_provider_port_set(
    left: &[(String, ElmPortAccessPolicy, FlowDirection, FlowMode)],
    right: &[(String, ElmPortAccessPolicy, FlowDirection, FlowMode)],
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|item| right.iter().any(|other| other == item))
}

fn same_native_export_set(
    left: &[(String, FlowContract, u32)],
    right: &[(String, FlowContract, u32)],
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|item| right.iter().any(|other| other == item))
}

fn provider_call_blockers(status: i32) -> u64 {
    if status == ELM_CALL_STATUS_OK {
        0
    } else {
        ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED
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
