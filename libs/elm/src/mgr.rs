//! 单元管理器调用外壳。

pub mod api;

use crate::ctl::ELM_CTL_ABI_VERSION;
use crate::event::ElmEventRecord;
use crate::frame::{ElmCallFrame, ElmReplyFrame};
use crate::ports::ElmPortAccessPolicy;
use crate::snapshot::state_code;
use crate::state::ElmState;

pub const ELM_MGR_STATUS_OK: i32 = 0;
pub const ELM_MGR_STATUS_PERMISSION: i32 = -1;
pub const ELM_MGR_STATUS_NOT_FOUND: i32 = -2;
pub const ELM_MGR_STATUS_BUSY: i32 = -16;
pub const ELM_MGR_STATUS_INVALID: i32 = -22;
pub const ELM_MGR_STATUS_TODO: i32 = -4096;
pub const ELM_MGR_STATUS_UNSUPPORTED: i32 = -95;

pub const ELM_LIFECYCLE_REASON_NONE: u32 = 0;
pub const ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED: u32 = 1;
pub const ELM_LIFECYCLE_REASON_NATIVE_TODO: u32 = 2;
pub const ELM_LIFECYCLE_REASON_INVALID_STATE: u32 = 3;
pub const ELM_LIFECYCLE_REASON_LEASE_BUSY: u32 = 4;
pub const ELM_LIFECYCLE_REASON_CELL_NOT_FOUND: u32 = 5;
pub const ELM_LIFECYCLE_REASON_HAS_CHILDREN: u32 = 6;
pub const ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT: u32 = 7;
pub const ELM_LIFECYCLE_REASON_HAS_DEPENDENTS: u32 = 8;
pub const ELM_LIFECYCLE_REASON_HAS_EXTENSIONS: u32 = 9;
pub const ELM_LIFECYCLE_REASON_HOOK_FAILED: u32 = 10;

pub const ELM_MGR_ACTION_PAUSE: u32 = 1 << 0;
pub const ELM_MGR_ACTION_RESUME: u32 = 1 << 1;
pub const ELM_MGR_ACTION_DETACH: u32 = 1 << 2;
pub const ELM_MGR_ACTION_REPLACE: u32 = 1 << 3;
pub const ELM_MGR_ACTION_BIND: u32 = 1 << 4;
pub const ELM_MGR_ACTION_UNBIND: u32 = 1 << 5;
pub const ELM_MGR_ACTION_RUNTIME_LOG: u32 = 1 << 6;
pub const ELM_MGR_ACTION_RUNTIME_EVENT_READ: u32 = 1 << 7;
pub const ELM_MGR_ACTION_RUNTIME_EVENT_ACK: u32 = 1 << 8;
pub const ELM_MGR_ACTION_PROVIDER_REGISTER: u32 = 1 << 9;
pub const ELM_MGR_ACTION_PROVIDER_UNREGISTER: u32 = 1 << 10;
pub const ELM_MGR_ACTION_PROVIDER_QUERY: u32 = 1 << 11;
pub const ELM_MGR_ACTION_PROVIDER_INVOKE: u32 = 1 << 12;
pub const ELM_MGR_ACTION_HEALTH_QUERY: u32 = 1 << 13;
pub const ELM_MGR_ACTION_PROVIDER_ASYNC: u32 = 1 << 14;
pub const ELM_MGR_ACTION_API_QUERY: u32 = 1 << 15;
pub const ELM_MGR_ACTION_EVENT_SUBSCRIBE: u32 = 1 << 16;
pub const ELM_MGR_ACTION_EVENT_UNSUBSCRIBE: u32 = 1 << 17;
pub const ELM_MGR_ACTION_EVENT_READ: u32 = 1 << 18;
pub const ELM_MGR_ACTION_NATIVE_CAPABILITY_QUERY: u32 = 1 << 19;
pub const ELM_MGR_ACTION_TODO_QUERY: u32 = 1 << 20;

pub const ELM_MGR_POLICY_PREFLIGHT: u64 = 1 << 0;
pub const ELM_MGR_POLICY_AUDIT: u64 = 1 << 1;
pub const ELM_MGR_POLICY_LOAD_REQUIRES_EBI_SOURCE: u64 = 1 << 2;
pub const ELM_MGR_POLICY_REPLACE_TODO: u64 = 1 << 3;
pub const ELM_MGR_POLICY_NATIVE_LIFECYCLE_TODO: u64 = 1 << 4;
pub const ELM_MGR_POLICY_NEXUS_BINDING: u64 = 1 << 5;
pub const ELM_MGR_POLICY_MENU_BINDING: u64 = 1 << 6;
pub const ELM_MGR_POLICY_PROVIDER_PORTS: u64 = 1 << 7;
pub const ELM_MGR_POLICY_HEALTH: u64 = 1 << 8;
pub const ELM_MGR_POLICY_PROVIDER_ASYNC: u64 = 1 << 9;
pub const ELM_MGR_POLICY_API_REGISTRY: u64 = 1 << 10;
pub const ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS: u64 = 1 << 11;
pub const ELM_MGR_POLICY_NATIVE_CAPABILITIES: u64 = 1 << 12;
pub const ELM_MGR_POLICY_TODO_REGISTRY: u64 = 1 << 13;
pub const ELM_MGR_POLICY_RESOURCE_BUDGET: u64 = 1 << 14;

pub const ELM_POLICY_BLOCK_BUILTIN_PROTECTED: u64 = 1 << 0;
pub const ELM_POLICY_BLOCK_CELL_NOT_FOUND: u64 = 1 << 1;
pub const ELM_POLICY_BLOCK_INVALID_STATE: u64 = 1 << 2;
pub const ELM_POLICY_BLOCK_NATIVE_TODO: u64 = 1 << 3;
pub const ELM_POLICY_BLOCK_HAS_CHILDREN: u64 = 1 << 4;
pub const ELM_POLICY_BLOCK_HAS_DEPENDENTS: u64 = 1 << 5;
pub const ELM_POLICY_BLOCK_HAS_EXTENSIONS: u64 = 1 << 6;
pub const ELM_POLICY_BLOCK_LEASE_BUSY: u64 = 1 << 7;
pub const ELM_POLICY_BLOCK_REPLACE_TODO: u64 = 1 << 8;
pub const ELM_POLICY_BLOCK_GRAPH_INCONSISTENT: u64 = 1 << 9;
pub const ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE: u64 = 1 << 10;
pub const ELM_POLICY_BLOCK_PORT_NOT_FOUND: u64 = 1 << 11;
pub const ELM_POLICY_BLOCK_CONTRACT_MISMATCH: u64 = 1 << 12;
pub const ELM_POLICY_BLOCK_DUPLICATE_BINDING: u64 = 1 << 13;
pub const ELM_POLICY_BLOCK_PORT_TODO: u64 = 1 << 14;
pub const ELM_POLICY_BLOCK_BINDING_NOT_FOUND: u64 = 1 << 15;
pub const ELM_POLICY_BLOCK_BINDING_PROTECTED: u64 = 1 << 16;
pub const ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND: u64 = 1 << 17;
pub const ELM_POLICY_BLOCK_PROVIDER_BUSY: u64 = 1 << 18;
pub const ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED: u64 = 1 << 19;
pub const ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL: u64 = 1 << 20;
pub const ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED: u64 = 1 << 21;
pub const ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED: u64 = 1 << 22;
pub const ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED: u64 = 1 << 23;
pub const ELM_POLICY_BLOCK_RESOURCE_QUOTA: u64 = 1 << 24;

pub const ELM_MGR_RELATION_CONTRACT_LEN: usize = 64;
pub const ELM_MGR_RELATION_POINT_LEN: usize = 32;
pub const ELM_NEXUS_CONTRACT_LEN: usize = 64;
pub const ELM_RUNTIME_LOG_MESSAGE_LEN: usize = 256;
pub const ELM_MGR_MAX_PAYLOAD: usize = 4096;
pub const ELM_MGR_MAX_INPUT: usize = ELM_MGR_MAX_PAYLOAD + core::mem::size_of::<ElmMgrCallHeader>();
pub const ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED: u32 = 1 << 0;
pub const ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK: u32 = ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED;
pub const ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE: u32 = 1 << 0;
pub const ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK: u32 = ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE;
pub const ELM_PROVIDER_PORT_FLAG_NONE: u32 = 0;
pub const ELM_PROVIDER_FLAG_DYNAMIC: u16 = 1 << 0;
pub const ELM_PROVIDER_FLAG_KERNEL_BACKEND: u16 = 1 << 1;
pub const ELM_PROVIDER_FLAG_TODO_BACKEND: u16 = 1 << 2;
pub const ELM_PROVIDER_FLAG_NATIVE_BACKEND: u16 = 1 << 3;
pub const ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS: u32 = 5_000;
pub const ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS: u32 = 30_000;
pub const ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS: u32 = 60_000;
pub const ELM_PROVIDER_ASYNC_QUEUE_LIMIT: u32 = 64;
pub const ELM_NATIVE_CAPABILITY_KIND_EXPORT: u32 = 1;
pub const ELM_NATIVE_CAPABILITY_KIND_IMPORT: u32 = 2;
pub const ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED: u32 = 1 << 0;
pub const ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD: u32 = 1 << 1;
pub const ELM_NATIVE_CAPABILITY_NAME_LEN: usize = 128;
pub const ELM_REPLACE_CELL_ABI_VERSION: u16 = 1;
pub const ELM_REPLACE_MIGRATION_STATE_MAX: usize = 64 * 1024;
pub const ELM_TODO_KIND_RUNTIME: u32 = 1;
pub const ELM_TODO_KIND_PROVIDER: u32 = 2;
pub const ELM_TODO_KIND_SOURCE: u32 = 3;
pub const ELM_TODO_KIND_NATIVE: u32 = 4;
pub const ELM_TODO_KIND_FRAMEWORK: u32 = 5;
pub const ELM_TODO_REGISTRY_FLAG_TRUNCATED: u32 = 1 << 0;
pub const ELM_TODO_FLAG_STATIC: u32 = 1 << 0;
pub const ELM_TODO_FLAG_ACTIVE: u32 = 1 << 1;
pub const ELM_TODO_NAME_LEN: usize = 64;
pub const ELM_TODO_DETAIL_LEN: usize = 128;

pub const ELM_HEALTH_FLAG_HAS_FAILURES: u32 = 1 << 0;

pub const ELM_HEALTH_CHECK_GRAPH: u32 = 1;
pub const ELM_HEALTH_CHECK_CELLS: u32 = 2;
pub const ELM_HEALTH_CHECK_PORTS: u32 = 3;
pub const ELM_HEALTH_CHECK_PROVIDERS: u32 = 4;
pub const ELM_HEALTH_CHECK_BINDINGS: u32 = 5;
pub const ELM_HEALTH_CHECK_RUNTIME_PORTS: u32 = 6;
pub const ELM_HEALTH_CHECK_MENU: u32 = 7;
pub const ELM_HEALTH_CHECK_EVENTS: u32 = 8;
pub const ELM_HEALTH_CHECK_AUDITS: u32 = 9;
pub const ELM_HEALTH_CHECK_NATIVE_CAPABILITIES: u32 = 10;
pub const ELM_HEALTH_CHECK_TODO_REGISTRY: u32 = 11;

pub const ELM_HEALTH_DETAIL_NONE: u64 = 0;
pub const ELM_HEALTH_DETAIL_GRAPH_INVALID: u64 = 1;
pub const ELM_HEALTH_DETAIL_MISSING_OBJECT: u64 = 2;
pub const ELM_HEALTH_DETAIL_DUPLICATE_OBJECT: u64 = 3;
pub const ELM_HEALTH_DETAIL_DANGLING_REFERENCE: u64 = 4;
pub const ELM_HEALTH_DETAIL_CONTRACT_INVALID: u64 = 5;
pub const ELM_HEALTH_DETAIL_SEQUENCE_INVALID: u64 = 6;
pub const ELM_HEALTH_DETAIL_KIND_MISMATCH: u64 = 7;
pub const ELM_HEALTH_DETAIL_STATE_INVALID: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmMgrCallKind {
    QueryMenu = 1,
    LoadCell = 2,
    DetachCell = 3,
    PauseCell = 4,
    ResumeCell = 5,
    ReplaceCell = 6,
    QueryTopology = 7,
    QueryPolicy = 8,
    PreflightLifecycle = 9,
    QueryAudit = 10,
    QueryNexusBindings = 11,
    PreflightBind = 12,
    CommitBind = 13,
    PreflightUnbind = 14,
    CommitUnbind = 15,
    SubmitRuntimeLog = 16,
    ReadRuntimeEvent = 17,
    AckRuntimeEvent = 18,
    QueryRuntimePorts = 19,
    RegisterProviderPort = 20,
    UnregisterProviderPort = 21,
    QueryProviderPorts = 22,
    InvokeProvider = 23,
    QueryProviderStats = 24,
    QueryHealth = 25,
    SubmitProviderCall = 26,
    PollProviderReply = 27,
    CancelProviderCall = 28,
    QueryProviderQueue = 29,
    QueryApiRegistry = 30,
    SubscribeEvent = 31,
    UnsubscribeEvent = 32,
    QueryEventSubscriptions = 33,
    ReadSubscribedEvents = 34,
    QueryProviderSnapshot = 35,
    QueryNativeCapabilities = 36,
    QueryTodoRegistry = 37,
}

impl ElmMgrCallKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::QueryMenu),
            2 => Some(Self::LoadCell),
            3 => Some(Self::DetachCell),
            4 => Some(Self::PauseCell),
            5 => Some(Self::ResumeCell),
            6 => Some(Self::ReplaceCell),
            7 => Some(Self::QueryTopology),
            8 => Some(Self::QueryPolicy),
            9 => Some(Self::PreflightLifecycle),
            10 => Some(Self::QueryAudit),
            11 => Some(Self::QueryNexusBindings),
            12 => Some(Self::PreflightBind),
            13 => Some(Self::CommitBind),
            14 => Some(Self::PreflightUnbind),
            15 => Some(Self::CommitUnbind),
            16 => Some(Self::SubmitRuntimeLog),
            17 => Some(Self::ReadRuntimeEvent),
            18 => Some(Self::AckRuntimeEvent),
            19 => Some(Self::QueryRuntimePorts),
            20 => Some(Self::RegisterProviderPort),
            21 => Some(Self::UnregisterProviderPort),
            22 => Some(Self::QueryProviderPorts),
            23 => Some(Self::InvokeProvider),
            24 => Some(Self::QueryProviderStats),
            25 => Some(Self::QueryHealth),
            26 => Some(Self::SubmitProviderCall),
            27 => Some(Self::PollProviderReply),
            28 => Some(Self::CancelProviderCall),
            29 => Some(Self::QueryProviderQueue),
            30 => Some(Self::QueryApiRegistry),
            31 => Some(Self::SubscribeEvent),
            32 => Some(Self::UnsubscribeEvent),
            33 => Some(Self::QueryEventSubscriptions),
            34 => Some(Self::ReadSubscribedEvents),
            35 => Some(Self::QueryProviderSnapshot),
            36 => Some(Self::QueryNativeCapabilities),
            37 => Some(Self::QueryTodoRegistry),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmProviderAsyncState {
    Queued = 1,
    Running = 2,
    Completed = 3,
    Failed = 4,
    Canceled = 5,
    Expired = 6,
}

impl ElmProviderAsyncState {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Queued),
            2 => Some(Self::Running),
            3 => Some(Self::Completed),
            4 => Some(Self::Failed),
            5 => Some(Self::Canceled),
            6 => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmLifecycleAction {
    Pause = 1,
    Resume = 2,
    Detach = 3,
    Replace = 4,
}

impl ElmLifecycleAction {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Pause),
            2 => Some(Self::Resume),
            3 => Some(Self::Detach),
            4 => Some(Self::Replace),
            _ => None,
        }
    }

    pub const fn bit(self) -> u32 {
        match self {
            Self::Pause => ELM_MGR_ACTION_PAUSE,
            Self::Resume => ELM_MGR_ACTION_RESUME,
            Self::Detach => ELM_MGR_ACTION_DETACH,
            Self::Replace => ELM_MGR_ACTION_REPLACE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmMgrRelationKind {
    Parent = 1,
    Dependency = 2,
    Extension = 3,
    ExtensionPoint = 4,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrCallHeader {
    pub kind: u32,
    pub flags: u32,
    pub payload_len: u32,
    pub reserved: u32,
}

impl ElmMgrCallHeader {
    pub const fn new(kind: ElmMgrCallKind, payload_len: u32) -> Self {
        Self {
            kind: kind as u32,
            flags: 0,
            payload_len,
            reserved: 0,
        }
    }

    pub const fn empty(kind: ElmMgrCallKind) -> Self {
        Self::new(kind, 0)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmLifecycleRequest {
    pub cell_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmLifecycleRequest {
    pub const fn new(cell_id: u64) -> Self {
        Self {
            cell_id,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmLifecycleResponse {
    pub cell_id: u64,
    pub status: i32,
    pub final_state: u32,
    pub revoked_leases: u32,
    pub removed_menu_items: u32,
    pub reason: u32,
    pub reserved: u32,
}

impl ElmLifecycleResponse {
    pub const fn new(
        cell_id: u64,
        status: i32,
        final_state: u32,
        revoked_leases: u32,
        removed_menu_items: u32,
        reason: u32,
    ) -> Self {
        Self {
            cell_id,
            status,
            final_state,
            revoked_leases,
            removed_menu_items,
            reason,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmReplaceCellRequestV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub source_kind: u16,
    pub reserved0: u16,
    pub target_cell_id: u64,
    pub migration_limit: u32,
    pub source_payload_len: u32,
    pub reserved1: u64,
}

impl ElmReplaceCellRequestV1 {
    pub const fn new(target_cell_id: u64, source_kind: u16, source_payload_len: u32) -> Self {
        Self {
            abi_version: ELM_REPLACE_CELL_ABI_VERSION,
            flags: 0,
            source_kind,
            reserved0: 0,
            target_cell_id,
            migration_limit: 0,
            source_payload_len,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmReplaceCellResponseV1 {
    pub cell_id: u64,
    pub status: i32,
    pub final_state: u32,
    pub generation: u64,
    pub migrated_len: u32,
    pub reason: u32,
    pub blockers: u64,
}

impl ElmReplaceCellResponseV1 {
    pub const fn new(
        cell_id: u64,
        status: i32,
        final_state: u32,
        generation: u64,
        migrated_len: u32,
        reason: u32,
        blockers: u64,
    ) -> Self {
        Self {
            cell_id,
            status,
            final_state,
            generation,
            migrated_len,
            reason,
            blockers,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmLifecyclePlanRequest {
    pub cell_id: u64,
    pub action: u32,
    pub flags: u32,
}

impl ElmLifecyclePlanRequest {
    pub const fn new(cell_id: u64, action: ElmLifecycleAction) -> Self {
        Self {
            cell_id,
            action: action as u32,
            flags: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmLifecyclePlanResponse {
    pub cell_id: u64,
    pub action: u32,
    pub allowed: u32,
    pub status: i32,
    pub final_state: u32,
    pub blockers: u64,
    pub affected_children: u32,
    pub affected_dependents: u32,
    pub affected_extensions: u32,
    pub reserved: u32,
}

impl ElmLifecyclePlanResponse {
    pub const fn new(
        cell_id: u64,
        action: ElmLifecycleAction,
        allowed: bool,
        status: i32,
        final_state: u32,
        blockers: u64,
    ) -> Self {
        Self {
            cell_id,
            action: action as u32,
            allowed: if allowed { 1 } else { 0 },
            status,
            final_state,
            blockers,
            affected_children: 0,
            affected_dependents: 0,
            affected_extensions: 0,
            reserved: 0,
        }
    }

    pub const fn with_affected(mut self, children: u32, dependents: u32, extensions: u32) -> Self {
        self.affected_children = children;
        self.affected_dependents = dependents;
        self.affected_extensions = extensions;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrPolicyInfo {
    pub abi_version: u16,
    pub reserved0: u16,
    pub supported_actions: u32,
    pub policy_flags: u64,
    pub blocker_mask: u64,
    pub audit_capacity: u32,
    pub reserved1: u32,
}

impl ElmMgrPolicyInfo {
    pub const fn new(audit_capacity: u32) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            reserved0: 0,
            supported_actions: ELM_MGR_ACTION_PAUSE
                | ELM_MGR_ACTION_RESUME
                | ELM_MGR_ACTION_DETACH
                | ELM_MGR_ACTION_BIND
                | ELM_MGR_ACTION_UNBIND
                | ELM_MGR_ACTION_RUNTIME_LOG
                | ELM_MGR_ACTION_RUNTIME_EVENT_READ
                | ELM_MGR_ACTION_RUNTIME_EVENT_ACK
                | ELM_MGR_ACTION_PROVIDER_REGISTER
                | ELM_MGR_ACTION_PROVIDER_UNREGISTER
                | ELM_MGR_ACTION_PROVIDER_QUERY
                | ELM_MGR_ACTION_PROVIDER_INVOKE
                | ELM_MGR_ACTION_HEALTH_QUERY
                | ELM_MGR_ACTION_PROVIDER_ASYNC
                | ELM_MGR_ACTION_API_QUERY
                | ELM_MGR_ACTION_EVENT_SUBSCRIBE
                | ELM_MGR_ACTION_EVENT_UNSUBSCRIBE
                | ELM_MGR_ACTION_EVENT_READ
                | ELM_MGR_ACTION_NATIVE_CAPABILITY_QUERY
                | ELM_MGR_ACTION_TODO_QUERY
                | ELM_MGR_ACTION_REPLACE,
            policy_flags: ELM_MGR_POLICY_PREFLIGHT
                | ELM_MGR_POLICY_AUDIT
                | ELM_MGR_POLICY_LOAD_REQUIRES_EBI_SOURCE
                | ELM_MGR_POLICY_NATIVE_LIFECYCLE_TODO
                | ELM_MGR_POLICY_NEXUS_BINDING
                | ELM_MGR_POLICY_MENU_BINDING
                | ELM_MGR_POLICY_PROVIDER_PORTS
                | ELM_MGR_POLICY_HEALTH
                | ELM_MGR_POLICY_PROVIDER_ASYNC
                | ELM_MGR_POLICY_API_REGISTRY
                | ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS
                | ELM_MGR_POLICY_NATIVE_CAPABILITIES
                | ELM_MGR_POLICY_TODO_REGISTRY
                | ELM_MGR_POLICY_RESOURCE_BUDGET,
            blocker_mask: ELM_POLICY_BLOCK_BUILTIN_PROTECTED
                | ELM_POLICY_BLOCK_CELL_NOT_FOUND
                | ELM_POLICY_BLOCK_INVALID_STATE
                | ELM_POLICY_BLOCK_NATIVE_TODO
                | ELM_POLICY_BLOCK_HAS_CHILDREN
                | ELM_POLICY_BLOCK_HAS_DEPENDENTS
                | ELM_POLICY_BLOCK_HAS_EXTENSIONS
                | ELM_POLICY_BLOCK_LEASE_BUSY
                | ELM_POLICY_BLOCK_GRAPH_INCONSISTENT
                | ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE
                | ELM_POLICY_BLOCK_PORT_NOT_FOUND
                | ELM_POLICY_BLOCK_CONTRACT_MISMATCH
                | ELM_POLICY_BLOCK_DUPLICATE_BINDING
                | ELM_POLICY_BLOCK_PORT_TODO
                | ELM_POLICY_BLOCK_BINDING_NOT_FOUND
                | ELM_POLICY_BLOCK_BINDING_PROTECTED
                | ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND
                | ELM_POLICY_BLOCK_PROVIDER_BUSY
                | ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED
                | ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL
                | ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED
                | ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED
                | ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED
                | ELM_POLICY_BLOCK_RESOURCE_QUOTA,
            audit_capacity,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeCapabilityHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub flags: u32,
    pub reserved: u32,
    pub event_sequence: u64,
}

impl ElmNativeCapabilityHeader {
    pub const fn new(record_count: u32, flags: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmNativeCapabilityRecord>() as u16,
            record_count,
            flags,
            reserved: 0,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeCapabilityRecord {
    pub kind: u32,
    pub status: i32,
    pub owner_cell_id: u64,
    pub peer_cell_id: u64,
    pub requested_version: u32,
    pub selected_version: u32,
    pub flags: u32,
    pub name_len: u16,
    pub contract_len: u16,
    pub reserved: u32,
    pub name: [u8; ELM_NATIVE_CAPABILITY_NAME_LEN],
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmNativeCapabilityRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: u32,
        status: i32,
        owner_cell_id: u64,
        peer_cell_id: u64,
        requested_version: u32,
        selected_version: u32,
        flags: u32,
        name: &str,
        contract: &str,
    ) -> Self {
        let mut out = Self {
            kind,
            status,
            owner_cell_id,
            peer_cell_id,
            requested_version,
            selected_version,
            flags,
            name_len: 0,
            contract_len: 0,
            reserved: 0,
            name: [0; ELM_NATIVE_CAPABILITY_NAME_LEN],
            contract: [0; ELM_NEXUS_CONTRACT_LEN],
        };
        out.name_len = copy_str(name, &mut out.name) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmTodoRegistryHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub active_count: u32,
    pub flags: u32,
    pub event_sequence: u64,
}

impl ElmTodoRegistryHeader {
    pub const fn new(record_count: u32, active_count: u32, event_sequence: u64) -> Self {
        Self::new_with_flags(record_count, active_count, 0, event_sequence)
    }

    pub const fn new_with_flags(
        record_count: u32,
        active_count: u32,
        flags: u32,
        event_sequence: u64,
    ) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmTodoRegistryRecord>() as u16,
            record_count,
            active_count,
            flags,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmTodoRegistryRecord {
    pub kind: u32,
    pub flags: u32,
    pub blocker: u64,
    pub subject_id: u64,
    pub status: i32,
    pub name_len: u16,
    pub detail_len: u16,
    pub reserved: u32,
    pub name: [u8; ELM_TODO_NAME_LEN],
    pub detail: [u8; ELM_TODO_DETAIL_LEN],
}

impl ElmTodoRegistryRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: u32,
        flags: u32,
        blocker: u64,
        subject_id: u64,
        status: i32,
        name: &str,
        detail: &str,
    ) -> Self {
        let mut out = Self {
            kind,
            flags,
            blocker,
            subject_id,
            status,
            name_len: 0,
            detail_len: 0,
            reserved: 0,
            name: [0; ELM_TODO_NAME_LEN],
            detail: [0; ELM_TODO_DETAIL_LEN],
        };
        out.name_len = copy_str(name, &mut out.name) as u16;
        out.detail_len = copy_str(detail, &mut out.detail) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrTopologyHeader {
    pub abi_version: u16,
    pub relation_entry_size: u16,
    pub relation_count: u32,
    pub cell_count: u32,
    pub reserved: u32,
    pub event_sequence: u64,
}

impl ElmMgrTopologyHeader {
    pub const fn new(relation_count: u32, cell_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            relation_entry_size: core::mem::size_of::<ElmMgrRelationRecord>() as u16,
            relation_count,
            cell_count,
            reserved: 0,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrRelationRecord {
    pub kind: u32,
    pub flags: u32,
    pub source: u64,
    pub target: u64,
    pub contract_len: u16,
    pub point_len: u16,
    pub reserved: u32,
    pub contract: [u8; ELM_MGR_RELATION_CONTRACT_LEN],
    pub point: [u8; ELM_MGR_RELATION_POINT_LEN],
}

impl ElmMgrRelationRecord {
    pub fn new(
        kind: ElmMgrRelationKind,
        source: u64,
        target: u64,
        contract: &str,
        point: &str,
    ) -> Self {
        let mut out = Self {
            kind: kind as u32,
            flags: 0,
            source,
            target,
            contract_len: 0,
            point_len: 0,
            reserved: 0,
            contract: [0; ELM_MGR_RELATION_CONTRACT_LEN],
            point: [0; ELM_MGR_RELATION_POINT_LEN],
        };
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out.point_len = copy_str(point, &mut out.point) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrAuditHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub dropped_count: u32,
    pub reserved: u32,
    pub last_sequence: u64,
}

impl ElmMgrAuditHeader {
    pub const fn new(record_count: u32, dropped_count: u32, last_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmMgrAuditRecord>() as u16,
            record_count,
            dropped_count,
            reserved: 0,
            last_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrAuditRecord {
    pub sequence: u64,
    pub action: u32,
    pub status: i32,
    pub cell_id: u64,
    pub blockers: u64,
    pub final_state: u32,
    pub reserved: u32,
}

impl ElmMgrAuditRecord {
    pub const fn new(
        sequence: u64,
        action: u32,
        status: i32,
        cell_id: u64,
        blockers: u64,
        final_state: u32,
    ) -> Self {
        Self {
            sequence,
            action,
            status,
            cell_id,
            blockers,
            final_state,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNexusBindRequest {
    pub cell_id: u64,
    pub port_id: u64,
    pub flags: u32,
    pub contract_len: u16,
    pub reserved: u16,
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmNexusBindRequest {
    pub fn new(cell_id: u64, port_id: u64, contract: &str) -> Self {
        let mut out = Self {
            cell_id,
            port_id,
            flags: 0,
            contract_len: 0,
            reserved: 0,
            contract: [0; ELM_NEXUS_CONTRACT_LEN],
        };
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNexusBindPlanResponse {
    pub cell_id: u64,
    pub port_id: u64,
    pub binding_id: u64,
    pub lease_id: u64,
    pub generation: u64,
    pub status: i32,
    pub allowed: u32,
    pub blockers: u64,
    pub reserved: u64,
}

impl ElmNexusBindPlanResponse {
    pub const fn new(
        cell_id: u64,
        port_id: u64,
        binding_id: u64,
        lease_id: u64,
        generation: u64,
        allowed: bool,
        status: i32,
        blockers: u64,
    ) -> Self {
        Self {
            cell_id,
            port_id,
            binding_id,
            lease_id,
            generation,
            status,
            allowed: if allowed { 1 } else { 0 },
            blockers,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNexusUnbindRequest {
    pub binding_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmNexusUnbindRequest {
    pub const fn new(binding_id: u64) -> Self {
        Self {
            binding_id,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNexusBindingSnapshotHeader {
    pub abi_version: u16,
    pub binding_entry_size: u16,
    pub binding_count: u32,
    pub event_sequence: u64,
}

impl ElmNexusBindingSnapshotHeader {
    pub const fn new(binding_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            binding_entry_size: core::mem::size_of::<ElmNexusBindingRecord>() as u16,
            binding_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNexusBindingRecord {
    pub binding_id: u64,
    pub cell_id: u64,
    pub port_id: u64,
    pub lease_id: u64,
    pub generation: u64,
    pub active: u32,
    pub flags: u32,
    pub contract_len: u16,
    pub reserved: u16,
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmNexusBindingRecord {
    pub fn new(
        binding_id: u64,
        cell_id: u64,
        port_id: u64,
        lease_id: u64,
        generation: u64,
        active: bool,
        contract: &str,
    ) -> Self {
        let mut out = Self {
            binding_id,
            cell_id,
            port_id,
            lease_id,
            generation,
            active: if active { 1 } else { 0 },
            flags: 0,
            contract_len: 0,
            reserved: 0,
            contract: [0; ELM_NEXUS_CONTRACT_LEN],
        };
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimeLogRequest {
    pub binding_id: u64,
    pub level: u32,
    pub flags: u32,
    pub message_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub message: [u8; ELM_RUNTIME_LOG_MESSAGE_LEN],
}

impl ElmRuntimeLogRequest {
    pub fn new(binding_id: u64, level: u32, message: &str) -> Self {
        let mut out = Self {
            binding_id,
            level,
            flags: 0,
            message_len: 0,
            reserved0: 0,
            reserved1: 0,
            message: [0; ELM_RUNTIME_LOG_MESSAGE_LEN],
        };
        out.message_len = copy_str(message, &mut out.message) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimeLogResponse {
    pub binding_id: u64,
    pub accepted_len: u32,
    pub status: i32,
    pub submitted_logs: u64,
    pub reserved: u64,
}

impl ElmRuntimeLogResponse {
    pub const fn new(binding_id: u64, accepted_len: u32, status: i32, submitted_logs: u64) -> Self {
        Self {
            binding_id,
            accepted_len,
            status,
            submitted_logs,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimeEventRequest {
    pub binding_id: u64,
    pub cursor: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmRuntimeEventRequest {
    pub const fn new(binding_id: u64, cursor: u64) -> Self {
        Self {
            binding_id,
            cursor,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimeEventResponse {
    pub binding_id: u64,
    pub cursor: u64,
    pub next_cursor: u64,
    pub dropped_events: u64,
    pub has_event: u32,
    pub status: i32,
    pub event: ElmEventRecord,
}

impl ElmRuntimeEventResponse {
    pub const fn empty(binding_id: u64, cursor: u64, dropped_events: u64, status: i32) -> Self {
        Self {
            binding_id,
            cursor,
            next_cursor: cursor,
            dropped_events,
            has_event: 0,
            status,
            event: ElmEventRecord::zero(),
        }
    }

    pub const fn with_event(
        binding_id: u64,
        cursor: u64,
        event: ElmEventRecord,
        dropped_events: u64,
        status: i32,
    ) -> Self {
        Self {
            binding_id,
            cursor,
            next_cursor: event.sequence,
            dropped_events,
            has_event: 1,
            status,
            event,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimePortStatsHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub event_sequence: u64,
}

impl ElmRuntimePortStatsHeader {
    pub const fn new(record_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmRuntimePortStatsRecord>() as u16,
            record_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimePortStatsRecord {
    pub binding_id: u64,
    pub cell_id: u64,
    pub port_id: u64,
    pub lease_id: u64,
    pub cursor: u64,
    pub submitted_logs: u64,
    pub delivered_events: u64,
    pub dropped_events: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmRuntimePortStatsRecord {
    pub const fn new(
        binding_id: u64,
        cell_id: u64,
        port_id: u64,
        lease_id: u64,
        cursor: u64,
        submitted_logs: u64,
        delivered_events: u64,
        dropped_events: u64,
    ) -> Self {
        Self {
            binding_id,
            cell_id,
            port_id,
            lease_id,
            cursor,
            submitted_logs,
            delivered_events,
            dropped_events,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderPortRegisterRequest {
    pub owner_cell_id: u64,
    pub flags: u32,
    pub access_policy: u32,
    pub direction: u32,
    pub mode: u32,
    pub contract_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmProviderPortRegisterRequest {
    pub fn new(
        owner_cell_id: u64,
        contract: &str,
        access_policy: ElmPortAccessPolicy,
        direction: crate::FlowDirection,
        mode: crate::FlowMode,
        flags: u32,
    ) -> Self {
        let mut out = Self {
            owner_cell_id,
            flags,
            access_policy: access_policy as u32,
            direction: direction as u32,
            mode: mode as u32,
            contract_len: 0,
            reserved0: 0,
            reserved1: 0,
            contract: [0; ELM_NEXUS_CONTRACT_LEN],
        };
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderPortRegisterResponse {
    pub owner_cell_id: u64,
    pub port_id: u64,
    pub status: i32,
    pub access_policy: u32,
    pub blockers: u64,
    pub reserved: u64,
}

impl ElmProviderPortRegisterResponse {
    pub const fn new(
        owner_cell_id: u64,
        port_id: u64,
        status: i32,
        access_policy: u32,
        blockers: u64,
    ) -> Self {
        Self {
            owner_cell_id,
            port_id,
            status,
            access_policy,
            blockers,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderPortUnregisterRequest {
    pub port_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmProviderPortUnregisterRequest {
    pub const fn new(port_id: u64) -> Self {
        Self {
            port_id,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderInvokeRequest {
    pub frame: ElmCallFrame,
}

impl ElmProviderInvokeRequest {
    pub const fn new(frame: ElmCallFrame) -> Self {
        Self { frame }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderInvokeResponse {
    pub reply: ElmReplyFrame,
}

impl ElmProviderInvokeResponse {
    pub const fn new(reply: ElmReplyFrame) -> Self {
        Self { reply }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderSnapshotRequest {
    pub port_id: u64,
    pub binding_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmProviderSnapshotRequest {
    pub const fn by_port(port_id: u64) -> Self {
        Self {
            port_id,
            binding_id: 0,
            flags: 0,
            reserved: 0,
        }
    }

    pub const fn by_port_paged(port_id: u64, cursor: u32) -> Self {
        Self {
            port_id,
            binding_id: 0,
            flags: ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED,
            reserved: cursor,
        }
    }

    pub const fn by_binding(binding_id: u64) -> Self {
        Self {
            port_id: 0,
            binding_id,
            flags: 0,
            reserved: 0,
        }
    }

    pub const fn by_binding_paged(binding_id: u64, cursor: u32) -> Self {
        Self {
            port_id: 0,
            binding_id,
            flags: ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED,
            reserved: cursor,
        }
    }

    pub const fn is_paged(&self) -> bool {
        self.flags & ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED != 0
    }

    pub const fn cursor(&self) -> u32 {
        self.reserved
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderSnapshotHeader {
    pub abi_version: u16,
    pub header_size: u16,
    pub status: i32,
    pub port_id: u64,
    pub binding_id: u64,
    pub payload_len: u32,
    pub record_count: u32,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmProviderSnapshotHeader {
    pub const fn new(
        status: i32,
        port_id: u64,
        binding_id: u64,
        payload_len: u32,
        record_count: u32,
    ) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            header_size: core::mem::size_of::<Self>() as u16,
            status,
            port_id,
            binding_id,
            payload_len,
            record_count,
            flags: 0,
            reserved: 0,
        }
    }

    pub const fn with_page(mut self, flags: u32, next_cursor: u32) -> Self {
        self.flags = flags;
        self.reserved = next_cursor;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderAsyncSubmitRequest {
    pub frame: ElmCallFrame,
    pub timeout_ms: u32,
    pub result_ttl_ms: u32,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmProviderAsyncSubmitRequest {
    pub const fn new(frame: ElmCallFrame, timeout_ms: u32, result_ttl_ms: u32) -> Self {
        Self {
            frame,
            timeout_ms,
            result_ttl_ms,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderAsyncSubmitResponse {
    pub ticket_id: u64,
    pub binding_id: u64,
    pub call_id: u64,
    pub status: i32,
    pub state: u32,
    pub queue_depth: u32,
    pub reserved: u32,
    pub blockers: u64,
}

impl ElmProviderAsyncSubmitResponse {
    pub const fn new(
        ticket_id: u64,
        binding_id: u64,
        call_id: u64,
        status: i32,
        state: ElmProviderAsyncState,
        queue_depth: u32,
        blockers: u64,
    ) -> Self {
        Self {
            ticket_id,
            binding_id,
            call_id,
            status,
            state: state as u32,
            queue_depth,
            reserved: 0,
            blockers,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderAsyncPollRequest {
    pub ticket_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmProviderAsyncPollRequest {
    pub const fn new(ticket_id: u64) -> Self {
        Self {
            ticket_id,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderAsyncPollResponse {
    pub ticket_id: u64,
    pub state: u32,
    pub status: i32,
    pub reply: ElmReplyFrame,
    pub blockers: u64,
    pub expires_at_ns: u64,
}

impl ElmProviderAsyncPollResponse {
    pub const fn new(
        ticket_id: u64,
        state: ElmProviderAsyncState,
        status: i32,
        reply: ElmReplyFrame,
        blockers: u64,
        expires_at_ns: u64,
    ) -> Self {
        Self {
            ticket_id,
            state: state as u32,
            status,
            reply,
            blockers,
            expires_at_ns,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderAsyncCancelRequest {
    pub ticket_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmProviderAsyncCancelRequest {
    pub const fn new(ticket_id: u64) -> Self {
        Self {
            ticket_id,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderAsyncCancelResponse {
    pub ticket_id: u64,
    pub state: u32,
    pub status: i32,
    pub blockers: u64,
}

impl ElmProviderAsyncCancelResponse {
    pub const fn new(
        ticket_id: u64,
        state: ElmProviderAsyncState,
        status: i32,
        blockers: u64,
    ) -> Self {
        Self {
            ticket_id,
            state: state as u32,
            status,
            blockers,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderPortStatsHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub event_sequence: u64,
}

impl ElmProviderPortStatsHeader {
    pub const fn new(record_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmProviderPortRecord>() as u16,
            record_count,
            event_sequence,
        }
    }

    pub const fn new_stats(record_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmProviderPortStatsRecord>() as u16,
            record_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderPortRecord {
    pub port_id: u64,
    pub owner_cell_id: u64,
    pub access_policy: u32,
    pub direction: u32,
    pub mode: u32,
    pub implemented: u32,
    pub invokable: u32,
    pub binding_count: u32,
    pub contract_len: u16,
    pub flags: u16,
    pub calls: u64,
    pub failed_calls: u64,
    pub revokes: u64,
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmProviderPortRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        port_id: u64,
        owner_cell_id: u64,
        access_policy: u32,
        direction: u32,
        mode: u32,
        implemented: bool,
        invokable: bool,
        binding_count: u32,
        flags: u16,
        calls: u64,
        failed_calls: u64,
        revokes: u64,
        contract: &str,
    ) -> Self {
        let mut out = Self {
            port_id,
            owner_cell_id,
            access_policy,
            direction,
            mode,
            implemented: u32::from(implemented),
            invokable: u32::from(invokable),
            binding_count,
            contract_len: 0,
            flags,
            calls,
            failed_calls,
            revokes,
            contract: [0; ELM_NEXUS_CONTRACT_LEN],
        };
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderPortStatsRecord {
    pub port_id: u64,
    pub owner_cell_id: u64,
    pub binding_count: u32,
    pub flags: u32,
    pub calls: u64,
    pub failed_calls: u64,
    pub revokes: u64,
}

impl ElmProviderPortStatsRecord {
    pub const fn new(
        port_id: u64,
        owner_cell_id: u64,
        binding_count: u32,
        flags: u32,
        calls: u64,
        failed_calls: u64,
        revokes: u64,
    ) -> Self {
        Self {
            port_id,
            owner_cell_id,
            binding_count,
            flags,
            calls,
            failed_calls,
            revokes,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderQueueStatsHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub event_sequence: u64,
}

impl ElmProviderQueueStatsHeader {
    pub const fn new(record_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmProviderQueueStatsRecord>() as u16,
            record_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProviderQueueStatsRecord {
    pub port_id: u64,
    pub queued: u32,
    pub running: u32,
    pub retained: u32,
    pub queue_limit: u32,
    pub max_in_flight: u32,
    pub reserved: u32,
    pub submitted: u64,
    pub completed: u64,
    pub canceled: u64,
    pub expired: u64,
    pub rejected: u64,
}

impl ElmProviderQueueStatsRecord {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        port_id: u64,
        queued: u32,
        running: u32,
        retained: u32,
        queue_limit: u32,
        max_in_flight: u32,
        submitted: u64,
        completed: u64,
        canceled: u64,
        expired: u64,
        rejected: u64,
    ) -> Self {
        Self {
            port_id,
            queued,
            running,
            retained,
            queue_limit,
            max_in_flight,
            reserved: 0,
            submitted,
            completed,
            canceled,
            expired,
            rejected,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCoreHealthHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub status: i32,
    pub flags: u32,
    pub event_sequence: u64,
}

impl ElmCoreHealthHeader {
    pub const fn new(record_count: u32, status: i32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmCoreHealthRecord>() as u16,
            record_count,
            status,
            flags: if status == ELM_MGR_STATUS_OK {
                0
            } else {
                ELM_HEALTH_FLAG_HAS_FAILURES
            },
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCoreHealthRecord {
    pub check_kind: u32,
    pub status: i32,
    pub subject_id: u64,
    pub detail: u64,
}

impl ElmCoreHealthRecord {
    pub const fn ok(check_kind: u32) -> Self {
        Self {
            check_kind,
            status: ELM_MGR_STATUS_OK,
            subject_id: 0,
            detail: ELM_HEALTH_DETAIL_NONE,
        }
    }

    pub const fn invalid(check_kind: u32, subject_id: u64, detail: u64) -> Self {
        Self {
            check_kind,
            status: ELM_MGR_STATUS_INVALID,
            subject_id,
            detail,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrResponseHeader {
    pub status: i32,
    pub payload_len: u32,
    pub reserved: u64,
}

impl ElmMgrResponseHeader {
    pub const fn ok(payload_len: u32) -> Self {
        Self {
            status: ELM_MGR_STATUS_OK,
            payload_len,
            reserved: 0,
        }
    }

    pub const fn invalid() -> Self {
        Self {
            status: ELM_MGR_STATUS_INVALID,
            payload_len: 0,
            reserved: 0,
        }
    }

    pub const fn not_found() -> Self {
        Self {
            status: ELM_MGR_STATUS_NOT_FOUND,
            payload_len: 0,
            reserved: 0,
        }
    }

    pub const fn busy() -> Self {
        Self {
            status: ELM_MGR_STATUS_BUSY,
            payload_len: 0,
            reserved: 0,
        }
    }

    pub const fn todo() -> Self {
        Self {
            status: ELM_MGR_STATUS_TODO,
            payload_len: 0,
            reserved: 0,
        }
    }

    pub const fn unsupported() -> Self {
        Self {
            status: ELM_MGR_STATUS_UNSUPPORTED,
            payload_len: 0,
            reserved: 0,
        }
    }
}

pub const fn status_from_blockers(blockers: u64) -> i32 {
    if blockers == 0 {
        ELM_MGR_STATUS_OK
    } else if blockers & ELM_POLICY_BLOCK_CELL_NOT_FOUND != 0 {
        ELM_MGR_STATUS_NOT_FOUND
    } else if blockers
        & (ELM_POLICY_BLOCK_PORT_NOT_FOUND
            | ELM_POLICY_BLOCK_BINDING_NOT_FOUND
            | ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND)
        != 0
    {
        ELM_MGR_STATUS_NOT_FOUND
    } else if blockers & ELM_POLICY_BLOCK_BUILTIN_PROTECTED != 0 {
        ELM_MGR_STATUS_PERMISSION
    } else if blockers & ELM_POLICY_BLOCK_BINDING_PROTECTED != 0 {
        ELM_MGR_STATUS_PERMISSION
    } else if blockers
        & (ELM_POLICY_BLOCK_HAS_CHILDREN
            | ELM_POLICY_BLOCK_HAS_DEPENDENTS
            | ELM_POLICY_BLOCK_HAS_EXTENSIONS
            | ELM_POLICY_BLOCK_LEASE_BUSY
            | ELM_POLICY_BLOCK_DUPLICATE_BINDING
            | ELM_POLICY_BLOCK_PROVIDER_BUSY
            | ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL
            | ELM_POLICY_BLOCK_RESOURCE_QUOTA)
        != 0
    {
        ELM_MGR_STATUS_BUSY
    } else if blockers
        & (ELM_POLICY_BLOCK_NATIVE_TODO
            | ELM_POLICY_BLOCK_REPLACE_TODO
            | ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE
            | ELM_POLICY_BLOCK_PORT_TODO)
        != 0
    {
        ELM_MGR_STATUS_TODO
    } else {
        ELM_MGR_STATUS_INVALID
    }
}

pub const fn first_lifecycle_reason(blockers: u64) -> u32 {
    if blockers & ELM_POLICY_BLOCK_BUILTIN_PROTECTED != 0 {
        ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED
    } else if blockers & ELM_POLICY_BLOCK_NATIVE_TODO != 0 {
        ELM_LIFECYCLE_REASON_NATIVE_TODO
    } else if blockers & ELM_POLICY_BLOCK_CELL_NOT_FOUND != 0 {
        ELM_LIFECYCLE_REASON_CELL_NOT_FOUND
    } else if blockers & ELM_POLICY_BLOCK_HAS_CHILDREN != 0 {
        ELM_LIFECYCLE_REASON_HAS_CHILDREN
    } else if blockers & ELM_POLICY_BLOCK_HAS_DEPENDENTS != 0 {
        ELM_LIFECYCLE_REASON_HAS_DEPENDENTS
    } else if blockers & ELM_POLICY_BLOCK_HAS_EXTENSIONS != 0 {
        ELM_LIFECYCLE_REASON_HAS_EXTENSIONS
    } else if blockers & ELM_POLICY_BLOCK_LEASE_BUSY != 0 {
        ELM_LIFECYCLE_REASON_LEASE_BUSY
    } else if blockers & ELM_POLICY_BLOCK_GRAPH_INCONSISTENT != 0 {
        ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT
    } else if blockers & ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED != 0 {
        ELM_LIFECYCLE_REASON_HOOK_FAILED
    } else if blockers != 0 {
        ELM_LIFECYCLE_REASON_INVALID_STATE
    } else {
        ELM_LIFECYCLE_REASON_NONE
    }
}

pub const fn planned_final_state(action: ElmLifecycleAction, current: ElmState) -> u32 {
    match action {
        ElmLifecycleAction::Pause => state_code(ElmState::Paused),
        ElmLifecycleAction::Resume => state_code(ElmState::Active),
        ElmLifecycleAction::Detach => state_code(ElmState::Retired),
        ElmLifecycleAction::Replace => state_code(current),
    }
}

fn copy_str(src: &str, dst: &mut [u8]) -> usize {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    n
}
