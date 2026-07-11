//! 单元管理器调用外壳。

pub mod api;

use crate::ctl::ELM_CTL_ABI_VERSION;
use crate::event::ElmEventRecord;
use crate::frame::{ElmCallFrame, ElmReplyFrame};
use crate::graph::ElmMixinMode;
use crate::ports::ElmPortAccessPolicy;
use crate::resource::ElmResourceBudget;
use crate::snapshot::state_code;
use crate::state::ElmState;

pub const ELM_MGR_STATUS_OK: i32 = 0;
pub const ELM_MGR_STATUS_PERMISSION: i32 = -1;
pub const ELM_MGR_STATUS_NOT_FOUND: i32 = -2;
pub const ELM_MGR_STATUS_BUSY: i32 = -16;
pub const ELM_MGR_STATUS_INVALID: i32 = -22;
pub const ELM_MGR_STATUS_NO_MEMORY: i32 = -12;
pub const ELM_MGR_STATUS_INTEGRITY: i32 = -74;
pub const ELM_MGR_STATUS_EXPIRED: i32 = -110;
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
pub const ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE: u32 = 11;
pub const ELM_LIFECYCLE_REASON_ABI_FINGERPRINT: u32 = 12;
pub const ELM_LIFECYCLE_REASON_ROLLBACK_REJECTED: u32 = 13;
pub const ELM_LIFECYCLE_REASON_CALLER_NOT_FOUND: u32 = 14;
pub const ELM_LIFECYCLE_REASON_CALLER_STALE: u32 = 15;
pub const ELM_LIFECYCLE_REASON_SCOPE_DENIED: u32 = 16;
pub const ELM_LIFECYCLE_REASON_POLICY_ESCALATION: u32 = 17;

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
pub const ELM_MGR_ACTION_EXTENSION_QUERY: u32 = 1 << 21;
pub const ELM_MGR_ACTION_EXTENSION_ATTACH: u32 = 1 << 22;
pub const ELM_MGR_ACTION_EXTENSION_DETACH: u32 = 1 << 23;
pub const ELM_MGR_ACTION_EXTENSION_DISPATCH: u32 = 1 << 24;
pub const ELM_MGR_ACTION_FAULT_QUERY: u32 = 1 << 25;
pub const ELM_MGR_ACTION_TRACE_QUERY: u32 = 1 << 26;
pub const ELM_MGR_ACTION_POLICY_UPDATE: u32 = 1 << 27;
pub const ELM_MGR_ACTION_RESOURCE_UPDATE: u32 = 1 << 28;
pub const ELM_MGR_ACTION_TRUST_QUERY: u32 = 1 << 29;
pub const ELM_MGR_ACTION_IMAGE_SESSION: u32 = 1 << 30;

pub const ELM_MGR_POLICY_PREFLIGHT: u64 = 1 << 0;
pub const ELM_MGR_POLICY_AUDIT: u64 = 1 << 1;
pub const ELM_MGR_POLICY_LOAD_REQUIRES_EBI_SOURCE: u64 = 1 << 2;
pub const ELM_MGR_POLICY_HOT_REPLACE: u64 = 1 << 3;
pub const ELM_MGR_POLICY_NATIVE_LIFECYCLE: u64 = 1 << 4;
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
pub const ELM_MGR_POLICY_EXTENSION_RUNTIME: u64 = 1 << 15;
pub const ELM_MGR_POLICY_FAULT_OBSERVABILITY: u64 = 1 << 16;
pub const ELM_MGR_POLICY_TRACE_RINGS: u64 = 1 << 17;
pub const ELM_MGR_POLICY_CELL_CAPABILITIES: u64 = 1 << 18;
pub const ELM_MGR_POLICY_RUNTIME_JOURNAL: u64 = 1 << 19;
pub const ELM_MGR_POLICY_TRUST: u64 = 1 << 20;
pub const ELM_MGR_POLICY_IMAGE_SESSIONS: u64 = 1 << 21;

pub const ELM_POLICY_BLOCK_BUILTIN_PROTECTED: u64 = 1 << 0;
pub const ELM_POLICY_BLOCK_CELL_NOT_FOUND: u64 = 1 << 1;
pub const ELM_POLICY_BLOCK_INVALID_STATE: u64 = 1 << 2;
pub const ELM_POLICY_BLOCK_NATIVE_TODO: u64 = 1 << 3;
pub const ELM_POLICY_BLOCK_HAS_CHILDREN: u64 = 1 << 4;
pub const ELM_POLICY_BLOCK_HAS_DEPENDENTS: u64 = 1 << 5;
pub const ELM_POLICY_BLOCK_HAS_EXTENSIONS: u64 = 1 << 6;
pub const ELM_POLICY_BLOCK_LEASE_BUSY: u64 = 1 << 7;
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
pub const ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND: u64 = 1 << 25;
pub const ELM_POLICY_BLOCK_EXTENSION_DUPLICATE: u64 = 1 << 26;
pub const ELM_POLICY_BLOCK_CAPABILITY_DENIED: u64 = 1 << 28;
pub const ELM_POLICY_BLOCK_UNTRUSTED_IMAGE: u64 = 1 << 29;
pub const ELM_POLICY_BLOCK_ABI_FINGERPRINT: u64 = 1 << 30;
pub const ELM_POLICY_BLOCK_ROLLBACK_REJECTED: u64 = 1 << 31;
pub const ELM_POLICY_BLOCK_CALLER_NOT_FOUND: u64 = 1 << 32;
pub const ELM_POLICY_BLOCK_CALLER_STALE: u64 = 1 << 33;
pub const ELM_POLICY_BLOCK_SCOPE_DENIED: u64 = 1 << 34;
pub const ELM_POLICY_BLOCK_POLICY_ESCALATION: u64 = 1 << 35;
pub const ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE: u64 = 1 << 36;

pub const ELM_MGR_RELATION_CONTRACT_LEN: usize = 64;
pub const ELM_MGR_RELATION_POINT_LEN: usize = 32;
pub const ELM_MGR_EXTENSION_POINT_LEN: usize = 32;
pub const ELM_MGR_EXTENSION_CONTRACT_LEN: usize = 64;
pub const ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN: usize = ELM_MGR_EXTENSION_CONTRACT_LEN;
pub const ELM_MGR_EXTENSION_PAYLOAD_LEN: usize = 256;
pub const ELM_NEXUS_CONTRACT_LEN: usize = 64;
pub const ELM_RUNTIME_LOG_MESSAGE_LEN: usize = 256;
pub const ELM_MGR_MAX_PAYLOAD: usize = 256 * 1024;
pub const ELM_MGR_MAX_INPUT: usize = ELM_MGR_MAX_PAYLOAD + core::mem::size_of::<ElmMgrCallHeader>();
pub const ELM_IMAGE_SESSION_ABI_VERSION: u16 = 1;
pub const ELM_IMAGE_SESSION_HASH_SHA256: u16 = 1;
pub const ELM_IMAGE_SESSION_DIGEST_LEN: usize = 32;
pub const ELM_IMAGE_SESSION_MAX_CHUNK: usize = 64 * 1024;
pub const ELM_IMAGE_SESSION_MAX_LENGTH: usize = 256 * 1024 * 1024;
pub const ELM_IMAGE_SESSION_MAX_ACTIVE: usize = 32;
pub const ELM_IMAGE_SESSION_MAX_PER_OWNER: usize = 4;
pub const ELM_IMAGE_SESSION_MAX_RESERVED_BYTES: usize = 512 * 1024 * 1024;
pub const ELM_IMAGE_SESSION_DEFAULT_TTL_MS: u32 = 60_000;
pub const ELM_IMAGE_SESSION_MAX_TTL_MS: u32 = 10 * 60_000;
pub const ELM_IMAGE_SESSION_FLAG_NONE: u32 = 0;
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
pub const ELM_RUNTIME_TRACE_KIND_LIFECYCLE: u32 = 1;
pub const ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL: u32 = 2;
pub const ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH: u32 = 3;
pub const ELM_RUNTIME_TRACE_KIND_REPLACE: u32 = 4;
pub const ELM_RUNTIME_TRACE_KIND_POLICY: u32 = 5;
pub const ELM_RUNTIME_TRACE_KIND_RESOURCE: u32 = 6;
pub const ELM_RUNTIME_TRACE_KIND_JOURNAL: u32 = 7;
pub const ELM_RUNTIME_TRACE_KIND_TRUST: u32 = 8;

pub const ELM_TRUST_FLAG_SEALED: u32 = 1 << 0;
pub const ELM_TRUST_FLAG_ALLOW_UNSIGNED: u32 = 1 << 1;
pub const ELM_TRUST_FLAG_UNSIGNED_ACTIVE: u32 = 1 << 2;

pub const ELM_CELL_POLICY_ALLOW_LIFECYCLE: u32 = 1 << 0;
pub const ELM_CELL_POLICY_ALLOW_BIND: u32 = 1 << 1;
pub const ELM_CELL_POLICY_ALLOW_PROVIDER: u32 = 1 << 2;
pub const ELM_CELL_POLICY_ALLOW_EVENT: u32 = 1 << 3;
pub const ELM_CELL_POLICY_ALLOW_EXTENSION: u32 = 1 << 4;
pub const ELM_CELL_POLICY_ALLOW_NATIVE: u32 = 1 << 5;
pub const ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE: u32 = 1 << 6;
pub const ELM_CELL_POLICY_ALLOW_POLICY_UPDATE: u32 = 1 << 7;
pub const ELM_CELL_POLICY_ALLOW_OBSERVE: u32 = 1 << 8;
pub const ELM_CELL_POLICY_ALLOW_ALL: u32 = ELM_CELL_POLICY_ALLOW_LIFECYCLE
    | ELM_CELL_POLICY_ALLOW_BIND
    | ELM_CELL_POLICY_ALLOW_PROVIDER
    | ELM_CELL_POLICY_ALLOW_EVENT
    | ELM_CELL_POLICY_ALLOW_EXTENSION
    | ELM_CELL_POLICY_ALLOW_NATIVE
    | ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE
    | ELM_CELL_POLICY_ALLOW_POLICY_UPDATE
    | ELM_CELL_POLICY_ALLOW_OBSERVE;

pub const ELM_CELL_POLICY_FLAG_LOCKED: u32 = 1 << 0;
pub const ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION: u32 = 1 << 1;
pub const ELM_CELL_POLICY_FLAG_AUDIT_ALL: u32 = 1 << 2;
pub const ELM_CELL_POLICY_FLAGS_MASK: u32 = ELM_CELL_POLICY_FLAG_LOCKED
    | ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION
    | ELM_CELL_POLICY_FLAG_AUDIT_ALL;

pub const ELM_PROVIDER_POLICY_REGISTER: u32 = 1 << 0;
pub const ELM_PROVIDER_POLICY_UNREGISTER: u32 = 1 << 1;
pub const ELM_PROVIDER_POLICY_INVOKE: u32 = 1 << 2;
pub const ELM_PROVIDER_POLICY_ASYNC: u32 = 1 << 3;
pub const ELM_PROVIDER_POLICY_SNAPSHOT: u32 = 1 << 4;
pub const ELM_PROVIDER_POLICY_ALL: u32 = ELM_PROVIDER_POLICY_REGISTER
    | ELM_PROVIDER_POLICY_UNREGISTER
    | ELM_PROVIDER_POLICY_INVOKE
    | ELM_PROVIDER_POLICY_ASYNC
    | ELM_PROVIDER_POLICY_SNAPSHOT;

pub const ELM_EXTENSION_POLICY_ATTACH: u32 = 1 << 0;
pub const ELM_EXTENSION_POLICY_ACCEPT: u32 = 1 << 1;
pub const ELM_EXTENSION_POLICY_DETACH: u32 = 1 << 2;
pub const ELM_EXTENSION_POLICY_DISPATCH: u32 = 1 << 3;
pub const ELM_EXTENSION_POLICY_MIXIN_PATCH: u32 = 1 << 4;
pub const ELM_EXTENSION_POLICY_ALL: u32 = ELM_EXTENSION_POLICY_ATTACH
    | ELM_EXTENSION_POLICY_ACCEPT
    | ELM_EXTENSION_POLICY_DETACH
    | ELM_EXTENSION_POLICY_DISPATCH
    | ELM_EXTENSION_POLICY_MIXIN_PATCH;

pub const ELM_NATIVE_POLICY_EXECUTE: u32 = 1 << 0;
pub const ELM_NATIVE_POLICY_IMPORT: u32 = 1 << 1;
pub const ELM_NATIVE_POLICY_EXPORT: u32 = 1 << 2;
pub const ELM_NATIVE_POLICY_REPLACE: u32 = 1 << 3;
pub const ELM_NATIVE_POLICY_MIXIN_PATCH: u32 = 1 << 4;
pub const ELM_NATIVE_POLICY_ALL: u32 = ELM_NATIVE_POLICY_EXECUTE
    | ELM_NATIVE_POLICY_IMPORT
    | ELM_NATIVE_POLICY_EXPORT
    | ELM_NATIVE_POLICY_REPLACE
    | ELM_NATIVE_POLICY_MIXIN_PATCH;

pub const ELM_RESOURCE_POLICY_QUERY: u32 = 1 << 0;
pub const ELM_RESOURCE_POLICY_UPDATE: u32 = 1 << 1;
pub const ELM_RESOURCE_POLICY_OWN: u32 = 1 << 2;
pub const ELM_RESOURCE_POLICY_ALL: u32 =
    ELM_RESOURCE_POLICY_QUERY | ELM_RESOURCE_POLICY_UPDATE | ELM_RESOURCE_POLICY_OWN;

pub const ELM_AUDIT_AUTHORITY_KERNEL: u32 = 1;
pub const ELM_AUDIT_AUTHORITY_USER_ADMIN: u32 = 2;
pub const ELM_AUDIT_AUTHORITY_MANAGER: u32 = 3;
pub const ELM_AUDIT_AUTHORITY_ANCESTOR: u32 = 4;
pub const ELM_AUDIT_AUTHORITY_SELF: u32 = 5;
pub const ELM_AUDIT_FLAG_OPERATION: u32 = 1 << 0;
pub const ELM_AUDIT_FLAG_AUTHORIZATION: u32 = 1 << 1;

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
pub const ELM_HEALTH_CHECK_TRUST: u32 = 12;
pub const ELM_HEALTH_CHECK_PROJECTION_SOURCES: u32 = 13;
pub const ELM_HEALTH_CHECK_JOURNAL: u32 = 14;
pub const ELM_HEALTH_CHECK_RESOURCES: u32 = 15;
pub const ELM_HEALTH_CHECK_EXECUTIONS: u32 = 16;
pub const ELM_HEALTH_CHECK_SEQUENCES: u32 = 17;

pub const ELM_HEALTH_DETAIL_NONE: u64 = 0;
pub const ELM_HEALTH_DETAIL_GRAPH_INVALID: u64 = 1;
pub const ELM_HEALTH_DETAIL_MISSING_OBJECT: u64 = 2;
pub const ELM_HEALTH_DETAIL_DUPLICATE_OBJECT: u64 = 3;
pub const ELM_HEALTH_DETAIL_DANGLING_REFERENCE: u64 = 4;
pub const ELM_HEALTH_DETAIL_CONTRACT_INVALID: u64 = 5;
pub const ELM_HEALTH_DETAIL_SEQUENCE_INVALID: u64 = 6;
pub const ELM_HEALTH_DETAIL_KIND_MISMATCH: u64 = 7;
pub const ELM_HEALTH_DETAIL_STATE_INVALID: u64 = 8;
pub const ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED: u64 = 9;
pub const ELM_HEALTH_DETAIL_PERSISTENCE_FAILED: u64 = 10;
pub const ELM_HEALTH_DETAIL_RESOURCE_LEAK: u64 = 11;
pub const ELM_HEALTH_DETAIL_STUCK_REFERENCE: u64 = 12;
pub const ELM_HEALTH_DETAIL_DROPPED_RECORDS: u64 = 13;

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
    QueryExtensions = 38,
    PreflightExtensionAttach = 39,
    CommitExtensionAttach = 40,
    CommitExtensionDetach = 41,
    DispatchExtension = 42,
    QueryFaultDump = 43,
    QueryLifecycleTrace = 44,
    QueryProviderCallTrace = 45,
    QueryMixinTrace = 46,
    QueryReplaceTrace = 47,
    QueryPolicyTrace = 48,
    QueryResourceDiagnostics = 49,
    QueryRuntimeJournal = 50,
    QueryCellPolicy = 51,
    UpdateCellPolicy = 52,
    QueryResourceBudget = 53,
    UpdateResourceBudget = 54,
    QueryTrustState = 55,
    BeginImageSession = 56,
    WriteImageSession = 57,
    SealImageSession = 58,
    AbortImageSession = 59,
    QueryImageSession = 60,
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
            38 => Some(Self::QueryExtensions),
            39 => Some(Self::PreflightExtensionAttach),
            40 => Some(Self::CommitExtensionAttach),
            41 => Some(Self::CommitExtensionDetach),
            42 => Some(Self::DispatchExtension),
            43 => Some(Self::QueryFaultDump),
            44 => Some(Self::QueryLifecycleTrace),
            45 => Some(Self::QueryProviderCallTrace),
            46 => Some(Self::QueryMixinTrace),
            47 => Some(Self::QueryReplaceTrace),
            48 => Some(Self::QueryPolicyTrace),
            49 => Some(Self::QueryResourceDiagnostics),
            50 => Some(Self::QueryRuntimeJournal),
            51 => Some(Self::QueryCellPolicy),
            52 => Some(Self::UpdateCellPolicy),
            53 => Some(Self::QueryResourceBudget),
            54 => Some(Self::UpdateResourceBudget),
            55 => Some(Self::QueryTrustState),
            56 => Some(Self::BeginImageSession),
            57 => Some(Self::WriteImageSession),
            58 => Some(Self::SealImageSession),
            59 => Some(Self::AbortImageSession),
            60 => Some(Self::QueryImageSession),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmImageSessionState {
    Uploading = 1,
    Sealed = 2,
}

impl ElmImageSessionState {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Uploading),
            2 => Some(Self::Sealed),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmImageSessionBeginRequestV1 {
    pub abi_version: u16,
    pub hash_alg: u16,
    pub flags: u32,
    pub total_len: u64,
    pub ttl_ms: u32,
    pub digest_len: u16,
    pub reserved0: u16,
    pub expected_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
    pub reserved1: u64,
}

impl ElmImageSessionBeginRequestV1 {
    pub const fn new(
        total_len: u64,
        ttl_ms: u32,
        expected_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
    ) -> Self {
        Self {
            abi_version: ELM_IMAGE_SESSION_ABI_VERSION,
            hash_alg: ELM_IMAGE_SESSION_HASH_SHA256,
            flags: ELM_IMAGE_SESSION_FLAG_NONE,
            total_len,
            ttl_ms,
            digest_len: ELM_IMAGE_SESSION_DIGEST_LEN as u16,
            reserved0: 0,
            expected_digest,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmImageSessionWriteRequestV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub session_id: u64,
    pub offset: u64,
    pub chunk_len: u32,
    pub reserved1: u32,
}

impl ElmImageSessionWriteRequestV1 {
    pub const fn new(session_id: u64, offset: u64, chunk_len: u32) -> Self {
        Self {
            abi_version: ELM_IMAGE_SESSION_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            session_id,
            offset,
            chunk_len,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmImageSessionRequestV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved: u32,
    pub session_id: u64,
}

impl ElmImageSessionRequestV1 {
    pub const fn new(session_id: u64) -> Self {
        Self {
            abi_version: ELM_IMAGE_SESSION_ABI_VERSION,
            flags: 0,
            reserved: 0,
            session_id,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmImageSessionInfoV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub state: u32,
    pub session_id: u64,
    pub total_len: u64,
    pub written_len: u64,
    pub created_at_ns: u64,
    pub expires_at_ns: u64,
    pub hash_alg: u16,
    pub digest_len: u16,
    pub flags: u32,
    pub expected_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
    pub actual_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmTrustRuntimeInfoV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub flags: u32,
    pub anchor_count: u32,
    pub revoked_count: u32,
    pub accepted_epoch_count: u32,
    pub reserved: u32,
}

impl ElmTrustRuntimeInfoV1 {
    pub const fn new(
        flags: u32,
        anchor_count: u32,
        revoked_count: u32,
        accepted_epoch_count: u32,
    ) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            flags,
            anchor_count,
            revoked_count,
            accepted_epoch_count,
            reserved: 0,
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
                | ELM_MGR_ACTION_EXTENSION_QUERY
                | ELM_MGR_ACTION_EXTENSION_ATTACH
                | ELM_MGR_ACTION_EXTENSION_DETACH
                | ELM_MGR_ACTION_EXTENSION_DISPATCH
                | ELM_MGR_ACTION_FAULT_QUERY
                | ELM_MGR_ACTION_TRACE_QUERY
                | ELM_MGR_ACTION_POLICY_UPDATE
                | ELM_MGR_ACTION_RESOURCE_UPDATE
                | ELM_MGR_ACTION_TRUST_QUERY
                | ELM_MGR_ACTION_IMAGE_SESSION
                | ELM_MGR_ACTION_REPLACE,
            policy_flags: ELM_MGR_POLICY_PREFLIGHT
                | ELM_MGR_POLICY_AUDIT
                | ELM_MGR_POLICY_LOAD_REQUIRES_EBI_SOURCE
                | ELM_MGR_POLICY_HOT_REPLACE
                | ELM_MGR_POLICY_NATIVE_LIFECYCLE
                | ELM_MGR_POLICY_NEXUS_BINDING
                | ELM_MGR_POLICY_MENU_BINDING
                | ELM_MGR_POLICY_PROVIDER_PORTS
                | ELM_MGR_POLICY_HEALTH
                | ELM_MGR_POLICY_PROVIDER_ASYNC
                | ELM_MGR_POLICY_API_REGISTRY
                | ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS
                | ELM_MGR_POLICY_NATIVE_CAPABILITIES
                | ELM_MGR_POLICY_TODO_REGISTRY
                | ELM_MGR_POLICY_EXTENSION_RUNTIME
                | ELM_MGR_POLICY_FAULT_OBSERVABILITY
                | ELM_MGR_POLICY_TRACE_RINGS
                | ELM_MGR_POLICY_CELL_CAPABILITIES
                | ELM_MGR_POLICY_RUNTIME_JOURNAL
                | ELM_MGR_POLICY_RESOURCE_BUDGET
                | ELM_MGR_POLICY_TRUST
                | ELM_MGR_POLICY_IMAGE_SESSIONS,
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
                | ELM_POLICY_BLOCK_RESOURCE_QUOTA
                | ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND
                | ELM_POLICY_BLOCK_EXTENSION_DUPLICATE
                | ELM_POLICY_BLOCK_CAPABILITY_DENIED
                | ELM_POLICY_BLOCK_UNTRUSTED_IMAGE
                | ELM_POLICY_BLOCK_ABI_FINGERPRINT
                | ELM_POLICY_BLOCK_ROLLBACK_REJECTED
                | ELM_POLICY_BLOCK_CALLER_NOT_FOUND
                | ELM_POLICY_BLOCK_CALLER_STALE
                | ELM_POLICY_BLOCK_SCOPE_DENIED
                | ELM_POLICY_BLOCK_POLICY_ESCALATION
                | ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE,
            audit_capacity,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimeTraceHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub dropped_count: u32,
    pub trace_kind: u32,
    pub last_sequence: u64,
}

impl ElmRuntimeTraceHeader {
    pub const fn new(
        record_count: u32,
        dropped_count: u32,
        trace_kind: u32,
        last_sequence: u64,
    ) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmRuntimeTraceRecord>() as u16,
            record_count,
            dropped_count,
            trace_kind,
            last_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRuntimeTraceRecord {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub trace_kind: u32,
    pub action: u32,
    pub status: i32,
    pub reserved0: u32,
    pub cell_id: u64,
    pub subject_id: u64,
    pub aux_id: u64,
    pub value: u64,
    pub blockers: u64,
}

impl ElmRuntimeTraceRecord {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        sequence: u64,
        timestamp_ns: u64,
        trace_kind: u32,
        action: u32,
        status: i32,
        cell_id: u64,
        subject_id: u64,
        aux_id: u64,
        value: u64,
        blockers: u64,
    ) -> Self {
        Self {
            sequence,
            timestamp_ns,
            trace_kind,
            action,
            status,
            reserved0: 0,
            cell_id,
            subject_id,
            aux_id,
            value,
            blockers,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCellPolicyRequest {
    pub cell_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmCellPolicyRequest {
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
pub struct ElmCellPolicyV1 {
    pub cell_id: u64,
    pub generation: u64,
    pub policy_epoch: u64,
    pub flags: u32,
    pub allowed_actions: u32,
    pub provider_flags: u32,
    pub extension_flags: u32,
    pub native_flags: u32,
    pub resource_flags: u32,
    pub status: i32,
    pub reserved: u32,
    pub blockers: u64,
}

impl ElmCellPolicyV1 {
    pub const fn new(
        cell_id: u64,
        generation: u64,
        allowed_actions: u32,
        status: i32,
        blockers: u64,
    ) -> Self {
        Self {
            cell_id,
            generation,
            policy_epoch: 1,
            flags: 0,
            allowed_actions,
            provider_flags: ELM_PROVIDER_POLICY_ALL,
            extension_flags: ELM_EXTENSION_POLICY_ALL,
            native_flags: ELM_NATIVE_POLICY_ALL,
            resource_flags: ELM_RESOURCE_POLICY_ALL,
            status,
            reserved: 0,
            blockers,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmResourceBudgetRequest {
    pub cell_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmResourceBudgetRequest {
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
pub struct ElmResourceBudgetResponse {
    pub cell_id: u64,
    pub status: i32,
    pub reserved: u32,
    pub blockers: u64,
    pub budget: ElmResourceBudget,
    pub usage: crate::resource::ElmResourceUsage,
}

impl ElmResourceBudgetResponse {
    pub const fn new(
        cell_id: u64,
        status: i32,
        blockers: u64,
        budget: ElmResourceBudget,
        usage: crate::resource::ElmResourceUsage,
    ) -> Self {
        Self {
            cell_id,
            status,
            reserved: 0,
            blockers,
            budget,
            usage,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmResourceBudgetUpdateRequest {
    pub cell_id: u64,
    pub flags: u32,
    pub reserved: u32,
    pub budget: ElmResourceBudget,
}

impl ElmResourceBudgetUpdateRequest {
    pub const fn new(cell_id: u64, budget: ElmResourceBudget) -> Self {
        Self {
            cell_id,
            flags: 0,
            reserved: 0,
            budget,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmFaultDumpHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub flags: u32,
    pub dropped_count: u32,
    pub last_sequence: u64,
}

impl ElmFaultDumpHeader {
    pub const fn new(record_count: u32, dropped_count: u32, last_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmFaultDumpRecord>() as u16,
            record_count,
            flags: if dropped_count == 0 { 0 } else { 1 },
            dropped_count,
            last_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmFaultDumpRecord {
    pub sequence: u64,
    pub cell_id: u64,
    pub pc: u64,
    pub addr: u64,
    pub return_pc: u64,
    pub phase: u32,
    pub code: u32,
    pub return_sp: u64,
    pub cpu_id: u32,
    pub depth: u32,
    pub reason: u32,
    pub reserved: u32,
}

impl ElmFaultDumpRecord {
    pub const fn new(
        sequence: u64,
        cell_id: u64,
        phase: u32,
        pc: u64,
        addr: u64,
        code: u32,
        return_pc: u64,
        return_sp: u64,
        cpu_id: u32,
        depth: u32,
        reason: u32,
    ) -> Self {
        Self {
            sequence,
            cell_id,
            pc,
            addr,
            return_pc,
            phase,
            code,
            return_sp,
            cpu_id,
            depth,
            reason,
            reserved: 0,
        }
    }
}

pub const ELM_EXTENSION_RECORD_KIND_POINT: u32 = 1;
pub const ELM_EXTENSION_RECORD_KIND_EDGE: u32 = 2;
pub const ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION: u32 = 1 << 0;
pub const ELM_EXTENSION_DISPATCH_FLAGS_MASK: u32 =
    ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION;
pub const ELM_MIXIN_REPLY_CONTINUE: u32 = 0;
pub const ELM_MIXIN_REPLY_STOP: u32 = 1 << 0;
pub const ELM_MIXIN_REPLY_REPLACE: u32 = 1 << 1;
pub const ELM_MIXIN_REPLY_DENY: u32 = 1 << 2;
pub const ELM_MIXIN_REPLY_FLAGS_MASK: u32 =
    ELM_MIXIN_REPLY_STOP | ELM_MIXIN_REPLY_REPLACE | ELM_MIXIN_REPLY_DENY;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmExtensionSnapshotHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub point_count: u32,
    pub edge_count: u32,
    pub reserved: u32,
    pub event_sequence: u64,
}

impl ElmExtensionSnapshotHeader {
    pub const fn new(point_count: u32, edge_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmExtensionSnapshotRecord>() as u16,
            point_count,
            edge_count,
            reserved: 0,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmExtensionSnapshotRecord {
    pub kind: u32,
    pub flags: u32,
    pub owner_cell_id: u64,
    pub target_cell_id: u64,
    pub extension_cell_id: u64,
    pub mode: u32,
    pub priority: i32,
    pub point_len: u16,
    pub contract_len: u16,
    pub handler_contract_len: u16,
    pub reserved: u16,
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
    pub contract: [u8; ELM_MGR_EXTENSION_CONTRACT_LEN],
    pub handler_contract: [u8; ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
}

impl ElmExtensionSnapshotRecord {
    pub fn point(owner: u64, point: &str, contract: &str) -> Self {
        Self::point_with_mode(owner, point, contract, ElmMixinMode::Chain)
    }

    pub fn point_with_mode(owner: u64, point: &str, contract: &str, mode: ElmMixinMode) -> Self {
        let mut out = Self {
            kind: ELM_EXTENSION_RECORD_KIND_POINT,
            flags: 0,
            owner_cell_id: owner,
            target_cell_id: 0,
            extension_cell_id: 0,
            mode: mode as u32,
            priority: 0,
            point_len: 0,
            contract_len: 0,
            handler_contract_len: 0,
            reserved: 0,
            point: [0; ELM_MGR_EXTENSION_POINT_LEN],
            contract: [0; ELM_MGR_EXTENSION_CONTRACT_LEN],
            handler_contract: [0; ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
        };
        out.point_len = copy_str(point, &mut out.point) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }

    pub fn edge(extension: u64, target: u64, point: &str, contract: &str) -> Self {
        Self::edge_with_dispatch(
            extension,
            target,
            point,
            contract,
            contract,
            0,
            ElmMixinMode::Chain,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn edge_with_dispatch(
        extension: u64,
        target: u64,
        point: &str,
        contract: &str,
        handler_contract: &str,
        priority: i32,
        mode: ElmMixinMode,
    ) -> Self {
        let mut out = Self {
            kind: ELM_EXTENSION_RECORD_KIND_EDGE,
            flags: 0,
            owner_cell_id: target,
            target_cell_id: target,
            extension_cell_id: extension,
            mode: mode as u32,
            priority,
            point_len: 0,
            contract_len: 0,
            handler_contract_len: 0,
            reserved: 0,
            point: [0; ELM_MGR_EXTENSION_POINT_LEN],
            contract: [0; ELM_MGR_EXTENSION_CONTRACT_LEN],
            handler_contract: [0; ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
        };
        out.point_len = copy_str(point, &mut out.point) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out.handler_contract_len = copy_str(handler_contract, &mut out.handler_contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmExtensionAttachRequest {
    pub extension_cell_id: u64,
    pub target_cell_id: u64,
    pub flags: u32,
    pub priority: i32,
    pub point_len: u16,
    pub contract_len: u16,
    pub handler_contract_len: u16,
    pub reserved: u16,
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
    pub contract: [u8; ELM_MGR_EXTENSION_CONTRACT_LEN],
    pub handler_contract: [u8; ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
}

impl ElmExtensionAttachRequest {
    pub fn new(extension_cell_id: u64, target_cell_id: u64, point: &str, contract: &str) -> Self {
        let mut out = Self {
            extension_cell_id,
            target_cell_id,
            flags: 0,
            priority: 0,
            point_len: 0,
            contract_len: 0,
            handler_contract_len: 0,
            reserved: 0,
            point: [0; ELM_MGR_EXTENSION_POINT_LEN],
            contract: [0; ELM_MGR_EXTENSION_CONTRACT_LEN],
            handler_contract: [0; ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
        };
        out.point_len = copy_str(point, &mut out.point) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out.handler_contract_len = copy_str(contract, &mut out.handler_contract) as u16;
        out
    }

    pub fn with_dispatch(mut self, handler_contract: &str, priority: i32) -> Self {
        self.priority = priority;
        self.handler_contract.fill(0);
        self.handler_contract_len = copy_str(handler_contract, &mut self.handler_contract) as u16;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmExtensionDetachRequest {
    pub extension_cell_id: u64,
    pub target_cell_id: u64,
    pub flags: u32,
    pub point_len: u16,
    pub reserved: u16,
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
}

impl ElmExtensionDetachRequest {
    pub fn new(extension_cell_id: u64, target_cell_id: u64, point: &str) -> Self {
        let mut out = Self {
            extension_cell_id,
            target_cell_id,
            flags: 0,
            point_len: 0,
            reserved: 0,
            point: [0; ELM_MGR_EXTENSION_POINT_LEN],
        };
        out.point_len = copy_str(point, &mut out.point) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmExtensionAttachResponse {
    pub extension_cell_id: u64,
    pub target_cell_id: u64,
    pub generation: u64,
    pub status: i32,
    pub allowed: u32,
    pub blockers: u64,
}

impl ElmExtensionAttachResponse {
    pub const fn new(
        extension_cell_id: u64,
        target_cell_id: u64,
        generation: u64,
        allowed: bool,
        status: i32,
        blockers: u64,
    ) -> Self {
        Self {
            extension_cell_id,
            target_cell_id,
            generation,
            status,
            allowed: if allowed { 1 } else { 0 },
            blockers,
        }
    }
}

pub type ElmExtensionDetachResponse = ElmExtensionAttachResponse;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmExtensionDispatchRequest {
    pub target_cell_id: u64,
    pub extension_cell_id: u64,
    pub opcode: u32,
    pub flags: u32,
    pub point_len: u16,
    pub contract_len: u16,
    pub payload_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
    pub contract: [u8; ELM_MGR_EXTENSION_CONTRACT_LEN],
    pub payload: [u8; ELM_MGR_EXTENSION_PAYLOAD_LEN],
}

impl ElmExtensionDispatchRequest {
    pub fn new(
        target_cell_id: u64,
        extension_cell_id: u64,
        opcode: u32,
        point: &str,
        contract: &str,
    ) -> Self {
        let mut out = Self {
            target_cell_id,
            extension_cell_id,
            opcode,
            flags: 0,
            point_len: 0,
            contract_len: 0,
            payload_len: 0,
            reserved0: 0,
            reserved1: 0,
            point: [0; ELM_MGR_EXTENSION_POINT_LEN],
            contract: [0; ELM_MGR_EXTENSION_CONTRACT_LEN],
            payload: [0; ELM_MGR_EXTENSION_PAYLOAD_LEN],
        };
        out.point_len = copy_str(point, &mut out.point) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmExtensionDispatchResponse {
    pub status: i32,
    pub matched_extensions: u32,
    pub called_extensions: u32,
    pub mode: u32,
    pub blockers: u64,
    pub reply: ElmReplyFrame,
}

impl ElmExtensionDispatchResponse {
    pub const fn new(
        status: i32,
        matched_extensions: u32,
        called_extensions: u32,
        blockers: u64,
        reply: ElmReplyFrame,
    ) -> Self {
        Self {
            status,
            matched_extensions,
            called_extensions,
            mode: ElmMixinMode::Chain as u32,
            blockers,
            reply,
        }
    }

    pub const fn with_mode(mut self, mode: ElmMixinMode) -> Self {
        self.mode = mode as u32;
        self
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
    pub flags: u32,
    pub actor_kind: u32,
    pub authority: u32,
    pub actor_id: u64,
    pub authority_id: u64,
    pub actor_generation: u64,
    pub policy_epoch: u64,
    pub credential_id: u64,
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
            flags: ELM_AUDIT_FLAG_OPERATION,
            actor_kind: crate::policy::ElmPrincipalKind::Kernel as u32,
            authority: ELM_AUDIT_AUTHORITY_KERNEL,
            actor_id: 0,
            authority_id: 0,
            actor_generation: 0,
            policy_epoch: 0,
            credential_id: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub const fn with_authority(
        mut self,
        flags: u32,
        actor_kind: u32,
        authority: u32,
        actor_id: u64,
        authority_id: u64,
        actor_generation: u64,
        policy_epoch: u64,
        credential_id: u64,
    ) -> Self {
        self.flags = flags;
        self.actor_kind = actor_kind;
        self.authority = authority;
        self.actor_id = actor_id;
        self.authority_id = authority_id;
        self.actor_generation = actor_generation;
        self.policy_epoch = policy_epoch;
        self.credential_id = credential_id;
        self
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
    pub const fn error(status: i32) -> Self {
        Self {
            status,
            payload_len: 0,
            reserved: 0,
        }
    }

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

    pub const fn permission() -> Self {
        Self {
            status: ELM_MGR_STATUS_PERMISSION,
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
            | ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND
            | ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND)
        != 0
    {
        ELM_MGR_STATUS_NOT_FOUND
    } else if blockers
        & (ELM_POLICY_BLOCK_CAPABILITY_DENIED
            | ELM_POLICY_BLOCK_UNTRUSTED_IMAGE
            | ELM_POLICY_BLOCK_ABI_FINGERPRINT
            | ELM_POLICY_BLOCK_ROLLBACK_REJECTED
            | ELM_POLICY_BLOCK_CALLER_NOT_FOUND
            | ELM_POLICY_BLOCK_CALLER_STALE
            | ELM_POLICY_BLOCK_SCOPE_DENIED
            | ELM_POLICY_BLOCK_POLICY_ESCALATION)
        != 0
    {
        ELM_MGR_STATUS_PERMISSION
    } else if blockers & ELM_POLICY_BLOCK_BUILTIN_PROTECTED != 0 {
        ELM_MGR_STATUS_BUSY
    } else if blockers & ELM_POLICY_BLOCK_BINDING_PROTECTED != 0 {
        ELM_MGR_STATUS_PERMISSION
    } else if blockers
        & (ELM_POLICY_BLOCK_HAS_CHILDREN
            | ELM_POLICY_BLOCK_HAS_DEPENDENTS
            | ELM_POLICY_BLOCK_HAS_EXTENSIONS
            | ELM_POLICY_BLOCK_LEASE_BUSY
            | ELM_POLICY_BLOCK_DUPLICATE_BINDING
            | ELM_POLICY_BLOCK_EXTENSION_DUPLICATE
            | ELM_POLICY_BLOCK_PROVIDER_BUSY
            | ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL
            | ELM_POLICY_BLOCK_RESOURCE_QUOTA
            | ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE)
        != 0
    {
        ELM_MGR_STATUS_BUSY
    } else if blockers
        & (ELM_POLICY_BLOCK_NATIVE_TODO
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
    } else if blockers & ELM_POLICY_BLOCK_UNTRUSTED_IMAGE != 0 {
        ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE
    } else if blockers & ELM_POLICY_BLOCK_ABI_FINGERPRINT != 0 {
        ELM_LIFECYCLE_REASON_ABI_FINGERPRINT
    } else if blockers & ELM_POLICY_BLOCK_ROLLBACK_REJECTED != 0 {
        ELM_LIFECYCLE_REASON_ROLLBACK_REJECTED
    } else if blockers & ELM_POLICY_BLOCK_CALLER_NOT_FOUND != 0 {
        ELM_LIFECYCLE_REASON_CALLER_NOT_FOUND
    } else if blockers & ELM_POLICY_BLOCK_CALLER_STALE != 0 {
        ELM_LIFECYCLE_REASON_CALLER_STALE
    } else if blockers & ELM_POLICY_BLOCK_SCOPE_DENIED != 0 {
        ELM_LIFECYCLE_REASON_SCOPE_DENIED
    } else if blockers & ELM_POLICY_BLOCK_POLICY_ESCALATION != 0 {
        ELM_LIFECYCLE_REASON_POLICY_ESCALATION
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
