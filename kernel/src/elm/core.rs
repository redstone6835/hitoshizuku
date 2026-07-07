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
    ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_MENU, ELM_HEALTH_CHECK_PORTS,
    ELM_HEALTH_CHECK_PROVIDERS, ELM_HEALTH_CHECK_RUNTIME_PORTS, ELM_HEALTH_DETAIL_CONTRACT_INVALID,
    ELM_HEALTH_DETAIL_DANGLING_REFERENCE, ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
    ELM_HEALTH_DETAIL_GRAPH_INVALID, ELM_HEALTH_DETAIL_KIND_MISMATCH,
    ELM_HEALTH_DETAIL_MISSING_OBJECT, ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
    ELM_HEALTH_DETAIL_STATE_INVALID, ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
    ELM_LIFECYCLE_REASON_LEASE_BUSY, ELM_LIFECYCLE_REASON_NONE, ELM_MENU_FLAG_REQUIRES_SYS_ADMIN,
    ELM_MENU_FLAG_TODO, ELM_MGR_ACTION_BIND, ELM_MGR_ACTION_PROVIDER_ASYNC,
    ELM_MGR_ACTION_PROVIDER_INVOKE, ELM_MGR_ACTION_PROVIDER_REGISTER,
    ELM_MGR_ACTION_PROVIDER_UNREGISTER, ELM_MGR_ACTION_RUNTIME_EVENT_ACK,
    ELM_MGR_ACTION_RUNTIME_EVENT_READ, ELM_MGR_ACTION_RUNTIME_LOG, ELM_MGR_ACTION_UNBIND,
    ELM_MGR_BUILTIN_ID, ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_NOT_FOUND,
    ELM_MGR_STATUS_OK, ELM_MGR_STATUS_TODO, ELM_MGR_STATUS_UNSUPPORTED,
    ELM_POLICY_BLOCK_BINDING_NOT_FOUND, ELM_POLICY_BLOCK_BINDING_PROTECTED,
    ELM_POLICY_BLOCK_BUILTIN_PROTECTED, ELM_POLICY_BLOCK_CELL_NOT_FOUND,
    ELM_POLICY_BLOCK_CONTRACT_MISMATCH, ELM_POLICY_BLOCK_DUPLICATE_BINDING,
    ELM_POLICY_BLOCK_GRAPH_INCONSISTENT, ELM_POLICY_BLOCK_HAS_CHILDREN,
    ELM_POLICY_BLOCK_HAS_DEPENDENTS, ELM_POLICY_BLOCK_HAS_EXTENSIONS,
    ELM_POLICY_BLOCK_INVALID_STATE, ELM_POLICY_BLOCK_LEASE_BUSY, ELM_POLICY_BLOCK_NATIVE_TODO,
    ELM_POLICY_BLOCK_PORT_NOT_FOUND, ELM_POLICY_BLOCK_PORT_TODO, ELM_POLICY_BLOCK_PROVIDER_BUSY,
    ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND, ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL,
    ELM_POLICY_BLOCK_REPLACE_TODO, ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS,
    ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS, ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS,
    ELM_PROVIDER_ASYNC_QUEUE_LIMIT, ELM_PROVIDER_FLAG_DYNAMIC, ELM_PROVIDER_FLAG_KERNEL_BACKEND,
    ELM_PROVIDER_FLAG_TODO_BACKEND, ELM_RUNTIME_LOG_MESSAGE_LEN, ElmActionInvokeReply,
    ElmActionInvokeRequest, ElmCallFrame, ElmCoreHealthHeader, ElmCoreHealthRecord, ElmCoreInfo,
    ElmEbiArch, ElmEbiLoadStatus, ElmEbiUnit, ElmError, ElmEventRecord, ElmEventSequence, ElmId,
    ElmKind, ElmLifecycleAction, ElmLifecyclePlanRequest, ElmLifecyclePlanResponse,
    ElmLifecycleResponse, ElmLoadCellResponse, ElmManifest, ElmMenuItemKind, ElmMgrAuditHeader,
    ElmMgrAuditRecord, ElmMgrPolicyInfo, ElmMgrRelationKind, ElmMgrRelationRecord,
    ElmMgrTopologyHeader, ElmName, ElmNexusBindPlanResponse, ElmNexusBindRequest,
    ElmNexusBindingRecord, ElmNexusBindingSnapshotHeader, ElmNexusUnbindRequest,
    ElmPortAccessPolicy, ElmProviderAsyncCancelRequest, ElmProviderAsyncCancelResponse,
    ElmProviderAsyncPollRequest, ElmProviderAsyncPollResponse, ElmProviderAsyncState,
    ElmProviderAsyncSubmitRequest, ElmProviderAsyncSubmitResponse, ElmProviderInvokeRequest,
    ElmProviderInvokeResponse, ElmProviderPortRecord, ElmProviderPortRegisterRequest,
    ElmProviderPortRegisterResponse, ElmProviderPortStatsHeader, ElmProviderPortStatsRecord,
    ElmProviderPortUnregisterRequest, ElmProviderQueueStatsHeader, ElmProviderQueueStatsRecord,
    ElmReplyFrame, ElmRuntimeEventRequest, ElmRuntimeEventResponse, ElmRuntimeLogRequest,
    ElmRuntimeLogResponse, ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord, ElmState,
    ElmVersion, FlowContract, FlowDirection, FlowMode, Generation, LeaseId, LeaseKind,
    LeaseRegistry, LeaseRights, NexusOffer, PortId, ResourceLease, TopologyEventKind,
    builtin_port_descriptors, first_lifecycle_reason, planned_final_state, state_code,
    status_from_blockers,
};
use sched::sync::Spinlock;

use super::menu::MenuItemRuntime;
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
const EVENT_RING_LIMIT: usize = 128;
const AUDIT_RING_LIMIT: usize = 128;
const FIRST_PROVIDER_TICKET_ID: u64 = 1;
const PROVIDER_RESULT_RING_LIMIT: usize = ELM_PROVIDER_ASYNC_QUEUE_LIMIT as usize;

static CORE: Spinlock<ElmCore> = Spinlock::new(ElmCore::new());

#[derive(Debug, Clone)]
pub(crate) struct CellRuntime {
    pub id: ElmId,
    pub parent: Option<ElmId>,
    pub state: ElmState,
    pub kind: ElmKind,
    pub generation: Generation,
    pub name: String,
    pub ebi_arch: ElmEbiArch,
    pub ebi_status: ElmEbiLoadStatus,
    pub has_native_code: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderBackend {
    Kernel(KernelProviderKind),
    ElmNativeTodo,
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
            ProviderBackend::Kernel(_) => flags |= ELM_PROVIDER_FLAG_KERNEL_BACKEND,
            ProviderBackend::ElmNativeTodo => flags |= ELM_PROVIDER_FLAG_TODO_BACKEND,
        }
        flags
    }
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
struct ProviderAsyncResult {
    ticket: u64,
    port: PortId,
    lease: LeaseId,
    state: ElmProviderAsyncState,
    status: i32,
    reply: ElmReplyFrame,
    blockers: u64,
    expires_at_ns: u64,
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

pub(crate) struct ElmCore {
    initialized: bool,
    graph: BindingGraph,
    cells: Vec<CellRuntime>,
    ports: Vec<PortRuntime>,
    providers: Vec<ProviderRuntime>,
    provider_jobs: VecDeque<ProviderAsyncJob>,
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
            graph: BindingGraph::new(),
            cells: Vec::new(),
            ports: Vec::new(),
            providers: Vec::new(),
            provider_jobs: VecDeque::new(),
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
        self.graph.insert_cell(ELM_MGR_ID, manifest)?;
        self.graph.add_extension_point(
            ELM_MGR_ID,
            "menu.item",
            FlowContract::new("mgr.menu.item@1")?,
        )?;
        self.cells.push(CellRuntime {
            id: ELM_MGR_ID,
            parent: None,
            state: ElmState::Active,
            kind: ElmKind::Manager,
            generation: Generation::FIRST,
            name: "elm-mgr".to_string(),
            ebi_arch: ElmEbiArch::Any,
            ebi_status: ElmEbiLoadStatus::Ok,
            has_native_code: false,
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        self.emit(TopologyEventKind::CellAdded, Some(ELM_MGR_ID));
        self.emit(TopologyEventKind::CellStateChanged, Some(ELM_MGR_ID));
        self.register_builtin_ports();
        self.register_builtin_mgr_actions()?;
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
                provider.in_flight,
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

        let provider = &self.providers[provider_index];
        let pending = self
            .provider_queued_count(provider.port)
            .saturating_add(provider.in_flight as usize);
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
        self.providers[provider_index].in_flight =
            self.providers[provider_index].in_flight.saturating_add(1);

        let (state, status, reply, blockers) = self.execute_provider_async_job(&job);
        let finish_ns = sched::now_ns_public();
        let (state, status, reply, blockers) = if finish_ns >= job.deadline_ns {
            (
                ElmProviderAsyncState::Expired,
                ELM_MGR_STATUS_BUSY,
                ElmReplyFrame::empty(
                    job.frame.binding_id,
                    job.frame.call_id,
                    ELM_CALL_STATUS_BUSY,
                ),
                ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED,
            )
        } else {
            (state, status, reply, blockers)
        };

        if let Some(provider_index) = self.provider_index(job.port) {
            self.providers[provider_index].in_flight =
                self.providers[provider_index].in_flight.saturating_sub(1);
        }
        self.finish_provider_async_job(job, state, status, reply, blockers, finish_ns);
        true
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
        } else if self.provider_binding_count(port) != 0 {
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
        let backend = self.providers[provider_index].backend;
        let reply = match backend {
            ProviderBackend::Kernel(kind) => self.invoke_kernel_provider(kind, &edge, frame),
            ProviderBackend::ElmNativeTodo => {
                self.providers[provider_index].failed_calls = self.providers[provider_index]
                    .failed_calls
                    .saturating_add(1);
                Err(ELM_MGR_STATUS_TODO)
            }
        }?;
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
        self.note_provider_revoke(edge.port);
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
                if self.cell_has_native_code(id) {
                    blockers |= ELM_POLICY_BLOCK_NATIVE_TODO;
                }
                if !matches!(current, ElmState::Active | ElmState::Paused) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
            }
            ElmLifecycleAction::Resume => {
                if self.cell_has_native_code(id) {
                    blockers |= ELM_POLICY_BLOCK_NATIVE_TODO;
                }
                if !matches!(current, ElmState::Paused | ElmState::Active) {
                    blockers |= ELM_POLICY_BLOCK_INVALID_STATE;
                }
            }
            ElmLifecycleAction::Detach => {
                // TODO(elm): 原生代码单元需要卸载执行器；当前只允许未激活的原生元数据直接摘除。
                if self.cell_has_native_code(id) && current != ElmState::Loaded {
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
            }
            ElmLifecycleAction::Replace => {
                // TODO(elm): 热替换需要影子绑定、状态迁移和切换代回滚协议。
                blockers |= ELM_POLICY_BLOCK_REPLACE_TODO;
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

    // TODO(elm): soyo 解析器接入后由容器转换层调用该协议装载入口。
    #[allow(dead_code)]
    pub fn load_ebi_unit(&mut self, unit: ElmEbiUnit, arch: ElmEbiArch) -> ElmLoadCellResponse {
        if let Err(status) = unit.validate(arch) {
            return ElmLoadCellResponse::failed(status);
        }
        let manifest = unit.manifest.clone();
        let name = manifest.name.as_str().to_string();
        let image_arch = unit.target.arch;
        let id = self.alloc_cell_id();

        if let Err(err) = self.insert_loaded_cell(id, manifest, name, image_arch, &unit) {
            log::error!("[elm] EBI cell rejected by runtime: {:?}", err);
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        }

        if unit.has_native_code() {
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::NativeCodeTodo,
                id.0,
                state_code(ElmState::Loaded),
                0,
            );
        }

        if let Err(err) = self.activate_loaded_cell(id, &unit) {
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

    pub fn pause_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        let action = ElmLifecycleAction::Pause;
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(id.0, action));
        if plan.allowed == 0 {
            return self.lifecycle_response_from_plan(action, plan, 0, 0);
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Active => {
                if self.transition_cell_state(id, ElmState::Quiescing).is_err()
                    || self.transition_cell_state(id, ElmState::Paused).is_err()
                {
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
        let action = ElmLifecycleAction::Detach;
        let plan = self.preflight_lifecycle(ElmLifecyclePlanRequest::new(id.0, action));
        if plan.allowed == 0 {
            return self.lifecycle_response_from_plan(action, plan, 0, 0);
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
                self.note_provider_revoke(edge.port);
            }
            self.remove_runtime_binding(binding);
            self.emit_binding(TopologyEventKind::BindingRemoved, binding);
        }

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
            let backend_flags =
                flags & (ELM_PROVIDER_FLAG_KERNEL_BACKEND | ELM_PROVIDER_FLAG_TODO_BACKEND);
            if backend_flags == 0
                || backend_flags
                    == (ELM_PROVIDER_FLAG_KERNEL_BACKEND | ELM_PROVIDER_FLAG_TODO_BACKEND)
                || ((flags & ELM_PROVIDER_FLAG_DYNAMIC) != 0) != provider.dynamic
            {
                records.push(ElmCoreHealthRecord::invalid(
                    ELM_HEALTH_CHECK_PROVIDERS,
                    provider.port.0,
                    ELM_HEALTH_DETAIL_STATE_INVALID,
                ));
            }
            match provider.backend {
                ProviderBackend::Kernel(_) if !port.implemented => {
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

    pub fn debug_dump_bytes(&self) -> Vec<u8> {
        let (health_status, health_records) = self.health_records();
        let health_failures = health_records
            .iter()
            .filter(|record| record.status != ELM_MGR_STATUS_OK)
            .count();
        let mut out = format!(
            "ELM Core 诊断\ncells={}\nports={}\nproviders={}\nbindings={}\nleases={}\nruntime_ports={}\nmenu_items={}\nlast_event_sequence={}\nhealth_status={}\nhealth_records={}\nhealth_failures={}\n",
            self.cells.len(),
            self.ports.len(),
            self.providers.len(),
            self.graph.capability_bindings().len(),
            self.lease_count(),
            self.runtime_ports.len(),
            self.menu_items.len(),
            self.last_event_sequence(),
            health_status,
            health_records.len(),
            health_failures,
        );
        out.push_str("[cells]\n");
        for cell in &self.cells {
            out.push_str(
                format!(
                    "cell id={} parent={} name={} state={:?} kind={:?} generation={} ebi_arch={:?} ebi_status={:?} native_code={} owned_bindings={} owned_menu_items={}\n",
                    cell.id.0,
                    cell.parent.map(|id| id.0).unwrap_or(0),
                    cell.name,
                    cell.state,
                    cell.kind,
                    cell.generation.0,
                    cell.ebi_arch,
                    cell.ebi_status,
                    cell.has_native_code,
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
        out.push_str("TODO(elm): soyo、原生代码执行、热替换和设备类端口仍未接入。\n");
        out.into_bytes()
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
            ebi_arch,
            ebi_status: if unit.has_native_code() {
                ElmEbiLoadStatus::NativeCodeTodo
            } else {
                ElmEbiLoadStatus::Ok
            },
            has_native_code: unit.has_native_code(),
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        self.emit(TopologyEventKind::CellAdded, Some(id));
        self.transition_cell_state(id, ElmState::Verified)?;
        self.transition_cell_state(id, ElmState::Loaded)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn activate_loaded_cell(&mut self, id: ElmId, unit: &ElmEbiUnit) -> Result<(), ElmError> {
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
            .map(|provider| self.provider_binding_count(provider.port))
            .sum()
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

    fn note_provider_revoke(&mut self, port: PortId) {
        if let Some(index) = self.provider_index(port) {
            self.providers[index].revokes = self.providers[index].revokes.saturating_add(1);
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
        let description = "通过能力织网绑定生成的菜单项".to_string();
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

fn provider_call_blockers(status: i32) -> u64 {
    if status == ELM_CALL_STATUS_OK {
        0
    } else {
        ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED
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
