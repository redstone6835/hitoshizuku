//! elm-mgr 管理调用的固定布局协议与策略常量。
//!
//! 本模块是 crate 私有实现模块，但其中稳定类型在 crate 根重导出，供内核、管理型 ELM 和
//! 用户态工具共享布局。它覆盖 lifecycle、预检、Nexus binding、provider 同步/异步调用、
//! event subscription、extension/mixin、per-cell policy、resource budget、trust、image session、
//! health、fault dump、trace ring 和 runtime journal。
//!
//! 所有修改型操作遵循“请求 -> 无副作用预检 -> 提交 -> 审计/trace”模型。blocker 位用于解释
//! 为什么不能提交，status 用于传输最终结果，reason/authority 记录授权链。调用方不能只看
//! status 忽略 blocker，也不能把预检成功当作永久承诺；提交阶段必须重新检查 generation、
//! policy、lease 和运行状态。
//!
//! 固定字符串缓冲区以零结尾，所有 flags 都有显式 mask，保留字段必须为零。记录头给出 ABI
//! 版本、record size 和 count，解析器必须使用 checked arithmetic 验证完整缓冲区。

pub mod api;

use crate::ctl::ELM_CTL_ABI_VERSION;
use crate::event::ElmEventRecord;
use crate::frame::{ElmCallFrame, ElmReplyFrame};
use crate::resource::ElmResourceBudget;
use crate::state::ElmState;
use crate::wire::{ElmMixinMode, ElmPortAccessPolicy, ElmPrincipalKind};

/// `ELM_MGR_STATUS_OK` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_OK: i32 = 0;
/// `ELM_MGR_STATUS_PERMISSION` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_PERMISSION: i32 = -1;
/// `ELM_MGR_STATUS_NOT_FOUND` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_NOT_FOUND: i32 = -2;
/// `ELM_MGR_STATUS_BUSY` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_BUSY: i32 = -16;
/// `ELM_MGR_STATUS_INVALID` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_INVALID: i32 = -22;
/// `ELM_MGR_STATUS_NO_MEMORY` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_NO_MEMORY: i32 = -12;
/// `ELM_MGR_STATUS_INTEGRITY` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_INTEGRITY: i32 = -74;
/// `ELM_MGR_STATUS_EXPIRED` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_EXPIRED: i32 = -110;
/// `ELM_MGR_STATUS_TODO` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_TODO: i32 = -4096;
/// `ELM_MGR_STATUS_UNSUPPORTED` 状态码，用于在线格式和调用边界上传递该结果。
pub const ELM_MGR_STATUS_UNSUPPORTED: i32 = -95;

/// `ELM_LIFECYCLE_REASON_NONE` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_NONE: u32 = 0;
/// `ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED: u32 = 1;
/// `ELM_LIFECYCLE_REASON_NATIVE_TODO` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_NATIVE_TODO: u32 = 2;
/// `ELM_LIFECYCLE_REASON_INVALID_STATE` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_INVALID_STATE: u32 = 3;
/// `ELM_LIFECYCLE_REASON_LEASE_BUSY` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_LEASE_BUSY: u32 = 4;
/// `ELM_LIFECYCLE_REASON_CELL_NOT_FOUND` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_CELL_NOT_FOUND: u32 = 5;
/// `ELM_LIFECYCLE_REASON_HAS_CHILDREN` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_HAS_CHILDREN: u32 = 6;
/// `ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT: u32 = 7;
/// `ELM_LIFECYCLE_REASON_HAS_DEPENDENTS` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_HAS_DEPENDENTS: u32 = 8;
/// `ELM_LIFECYCLE_REASON_HAS_EXTENSIONS` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_HAS_EXTENSIONS: u32 = 9;
/// `ELM_LIFECYCLE_REASON_HOOK_FAILED` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_HOOK_FAILED: u32 = 10;
/// `ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE: u32 = 11;
/// `ELM_LIFECYCLE_REASON_ABI_FINGERPRINT` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_ABI_FINGERPRINT: u32 = 12;
/// `ELM_LIFECYCLE_REASON_ROLLBACK_REJECTED` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_ROLLBACK_REJECTED: u32 = 13;
/// `ELM_LIFECYCLE_REASON_CALLER_NOT_FOUND` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_CALLER_NOT_FOUND: u32 = 14;
/// `ELM_LIFECYCLE_REASON_CALLER_STALE` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_CALLER_STALE: u32 = 15;
/// `ELM_LIFECYCLE_REASON_SCOPE_DENIED` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_SCOPE_DENIED: u32 = 16;
/// `ELM_LIFECYCLE_REASON_POLICY_ESCALATION` 原因码，用于审计、诊断或中止路径中精确标识该原因。
pub const ELM_LIFECYCLE_REASON_POLICY_ESCALATION: u32 = 17;

/// `ELM_MGR_ACTION_PAUSE` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_PAUSE: u32 = 1 << 0;
/// `ELM_MGR_ACTION_RESUME` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_RESUME: u32 = 1 << 1;
/// `ELM_MGR_ACTION_DETACH` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_DETACH: u32 = 1 << 2;
/// `ELM_MGR_ACTION_REPLACE` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_REPLACE: u32 = 1 << 3;
/// `ELM_MGR_ACTION_BIND` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_BIND: u32 = 1 << 4;
/// `ELM_MGR_ACTION_UNBIND` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_UNBIND: u32 = 1 << 5;
/// `ELM_MGR_ACTION_RUNTIME_LOG` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_RUNTIME_LOG: u32 = 1 << 6;
/// `ELM_MGR_ACTION_RUNTIME_EVENT_READ` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_RUNTIME_EVENT_READ: u32 = 1 << 7;
/// `ELM_MGR_ACTION_RUNTIME_EVENT_ACK` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_RUNTIME_EVENT_ACK: u32 = 1 << 8;
/// `ELM_MGR_ACTION_PROVIDER_REGISTER` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_PROVIDER_REGISTER: u32 = 1 << 9;
/// `ELM_MGR_ACTION_PROVIDER_UNREGISTER` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_PROVIDER_UNREGISTER: u32 = 1 << 10;
/// `ELM_MGR_ACTION_PROVIDER_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_PROVIDER_QUERY: u32 = 1 << 11;
/// `ELM_MGR_ACTION_PROVIDER_INVOKE` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_PROVIDER_INVOKE: u32 = 1 << 12;
/// `ELM_MGR_ACTION_HEALTH_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_HEALTH_QUERY: u32 = 1 << 13;
/// `ELM_MGR_ACTION_PROVIDER_ASYNC` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_PROVIDER_ASYNC: u32 = 1 << 14;
/// `ELM_MGR_ACTION_API_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_API_QUERY: u32 = 1 << 15;
/// `ELM_MGR_ACTION_EVENT_SUBSCRIBE` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_EVENT_SUBSCRIBE: u32 = 1 << 16;
/// `ELM_MGR_ACTION_EVENT_UNSUBSCRIBE` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_EVENT_UNSUBSCRIBE: u32 = 1 << 17;
/// `ELM_MGR_ACTION_EVENT_READ` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_EVENT_READ: u32 = 1 << 18;
/// `ELM_MGR_ACTION_NATIVE_CAPABILITY_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_NATIVE_CAPABILITY_QUERY: u32 = 1 << 19;
/// `ELM_MGR_ACTION_TODO_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_TODO_QUERY: u32 = 1 << 20;
/// `ELM_MGR_ACTION_EXTENSION_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_EXTENSION_QUERY: u32 = 1 << 21;
/// `ELM_MGR_ACTION_EXTENSION_ATTACH` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_EXTENSION_ATTACH: u32 = 1 << 22;
/// `ELM_MGR_ACTION_EXTENSION_DETACH` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_EXTENSION_DETACH: u32 = 1 << 23;
/// `ELM_MGR_ACTION_EXTENSION_DISPATCH` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_EXTENSION_DISPATCH: u32 = 1 << 24;
/// `ELM_MGR_ACTION_FAULT_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_FAULT_QUERY: u32 = 1 << 25;
/// `ELM_MGR_ACTION_TRACE_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_TRACE_QUERY: u32 = 1 << 26;
/// `ELM_MGR_ACTION_POLICY_UPDATE` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_POLICY_UPDATE: u32 = 1 << 27;
/// `ELM_MGR_ACTION_RESOURCE_UPDATE` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_RESOURCE_UPDATE: u32 = 1 << 28;
/// `ELM_MGR_ACTION_TRUST_QUERY` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_TRUST_QUERY: u32 = 1 << 29;
/// `ELM_MGR_ACTION_IMAGE_SESSION` 操作编号；调用方通过该稳定数值选择对应的运行时动作。
pub const ELM_MGR_ACTION_IMAGE_SESSION: u32 = 1 << 30;

/// `ELM_MGR_POLICY_PREFLIGHT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_PREFLIGHT: u64 = 1 << 0;
/// `ELM_MGR_POLICY_AUDIT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_AUDIT: u64 = 1 << 1;
/// `ELM_MGR_POLICY_LOAD_REQUIRES_EBI_SOURCE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_LOAD_REQUIRES_EBI_SOURCE: u64 = 1 << 2;
/// `ELM_MGR_POLICY_HOT_REPLACE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_HOT_REPLACE: u64 = 1 << 3;
/// `ELM_MGR_POLICY_NATIVE_LIFECYCLE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_NATIVE_LIFECYCLE: u64 = 1 << 4;
/// `ELM_MGR_POLICY_NEXUS_BINDING` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_NEXUS_BINDING: u64 = 1 << 5;
/// `ELM_MGR_POLICY_MENU_BINDING` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_MENU_BINDING: u64 = 1 << 6;
/// `ELM_MGR_POLICY_PROVIDER_PORTS` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_PROVIDER_PORTS: u64 = 1 << 7;
/// `ELM_MGR_POLICY_HEALTH` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_HEALTH: u64 = 1 << 8;
/// `ELM_MGR_POLICY_PROVIDER_ASYNC` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_PROVIDER_ASYNC: u64 = 1 << 9;
/// `ELM_MGR_POLICY_API_REGISTRY` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_API_REGISTRY: u64 = 1 << 10;
/// `ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS: u64 = 1 << 11;
/// `ELM_MGR_POLICY_NATIVE_CAPABILITIES` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_NATIVE_CAPABILITIES: u64 = 1 << 12;
/// `ELM_MGR_POLICY_TODO_REGISTRY` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_TODO_REGISTRY: u64 = 1 << 13;
/// `ELM_MGR_POLICY_RESOURCE_BUDGET` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_RESOURCE_BUDGET: u64 = 1 << 14;
/// `ELM_MGR_POLICY_EXTENSION_RUNTIME` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_EXTENSION_RUNTIME: u64 = 1 << 15;
/// `ELM_MGR_POLICY_FAULT_OBSERVABILITY` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_FAULT_OBSERVABILITY: u64 = 1 << 16;
/// `ELM_MGR_POLICY_TRACE_RINGS` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_TRACE_RINGS: u64 = 1 << 17;
/// `ELM_MGR_POLICY_CELL_CAPABILITIES` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_CELL_CAPABILITIES: u64 = 1 << 18;
/// `ELM_MGR_POLICY_RUNTIME_JOURNAL` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_RUNTIME_JOURNAL: u64 = 1 << 19;
/// `ELM_MGR_POLICY_TRUST` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_TRUST: u64 = 1 << 20;
/// `ELM_MGR_POLICY_IMAGE_SESSIONS` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_MGR_POLICY_IMAGE_SESSIONS: u64 = 1 << 21;

/// `ELM_POLICY_BLOCK_BUILTIN_PROTECTED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_BUILTIN_PROTECTED: u64 = 1 << 0;
/// `ELM_POLICY_BLOCK_CELL_NOT_FOUND` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_CELL_NOT_FOUND: u64 = 1 << 1;
/// `ELM_POLICY_BLOCK_INVALID_STATE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_INVALID_STATE: u64 = 1 << 2;
/// `ELM_POLICY_BLOCK_NATIVE_TODO` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_NATIVE_TODO: u64 = 1 << 3;
/// `ELM_POLICY_BLOCK_HAS_CHILDREN` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_HAS_CHILDREN: u64 = 1 << 4;
/// `ELM_POLICY_BLOCK_HAS_DEPENDENTS` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_HAS_DEPENDENTS: u64 = 1 << 5;
/// `ELM_POLICY_BLOCK_HAS_EXTENSIONS` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_HAS_EXTENSIONS: u64 = 1 << 6;
/// `ELM_POLICY_BLOCK_LEASE_BUSY` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_LEASE_BUSY: u64 = 1 << 7;
/// `ELM_POLICY_BLOCK_GRAPH_INCONSISTENT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_GRAPH_INCONSISTENT: u64 = 1 << 9;
/// `ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE: u64 = 1 << 10;
/// `ELM_POLICY_BLOCK_PORT_NOT_FOUND` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PORT_NOT_FOUND: u64 = 1 << 11;
/// `ELM_POLICY_BLOCK_CONTRACT_MISMATCH` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_CONTRACT_MISMATCH: u64 = 1 << 12;
/// `ELM_POLICY_BLOCK_DUPLICATE_BINDING` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_DUPLICATE_BINDING: u64 = 1 << 13;
/// `ELM_POLICY_BLOCK_PORT_TODO` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PORT_TODO: u64 = 1 << 14;
/// `ELM_POLICY_BLOCK_BINDING_NOT_FOUND` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_BINDING_NOT_FOUND: u64 = 1 << 15;
/// `ELM_POLICY_BLOCK_BINDING_PROTECTED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_BINDING_PROTECTED: u64 = 1 << 16;
/// `ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND: u64 = 1 << 17;
/// `ELM_POLICY_BLOCK_PROVIDER_BUSY` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PROVIDER_BUSY: u64 = 1 << 18;
/// `ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED: u64 = 1 << 19;
/// `ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL: u64 = 1 << 20;
/// `ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED: u64 = 1 << 21;
/// `ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED: u64 = 1 << 22;
/// `ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED: u64 = 1 << 23;
/// `ELM_POLICY_BLOCK_RESOURCE_QUOTA` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_RESOURCE_QUOTA: u64 = 1 << 24;
/// `ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND: u64 = 1 << 25;
/// `ELM_POLICY_BLOCK_EXTENSION_DUPLICATE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_EXTENSION_DUPLICATE: u64 = 1 << 26;
/// `ELM_POLICY_BLOCK_CAPABILITY_DENIED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_CAPABILITY_DENIED: u64 = 1 << 28;
/// `ELM_POLICY_BLOCK_UNTRUSTED_IMAGE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_UNTRUSTED_IMAGE: u64 = 1 << 29;
/// `ELM_POLICY_BLOCK_ABI_FINGERPRINT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_ABI_FINGERPRINT: u64 = 1 << 30;
/// `ELM_POLICY_BLOCK_ROLLBACK_REJECTED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_ROLLBACK_REJECTED: u64 = 1 << 31;
/// `ELM_POLICY_BLOCK_CALLER_NOT_FOUND` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_CALLER_NOT_FOUND: u64 = 1 << 32;
/// `ELM_POLICY_BLOCK_CALLER_STALE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_CALLER_STALE: u64 = 1 << 33;
/// `ELM_POLICY_BLOCK_SCOPE_DENIED` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_SCOPE_DENIED: u64 = 1 << 34;
/// `ELM_POLICY_BLOCK_POLICY_ESCALATION` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_POLICY_ESCALATION: u64 = 1 << 35;
/// `ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE: u64 = 1 << 36;
/// `ELM_POLICY_BLOCK_NATIVE_CALL_FAILED` 策略能力位；数据面 pinned native call 失败。
#[allow(dead_code)]
pub const ELM_POLICY_BLOCK_NATIVE_CALL_FAILED: u64 = 1 << 37;

/// `ELM_MGR_RELATION_CONTRACT_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_RELATION_CONTRACT_LEN: usize = 64;
/// `ELM_MGR_RELATION_POINT_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_RELATION_POINT_LEN: usize = 32;
/// `ELM_MGR_EXTENSION_POINT_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_EXTENSION_POINT_LEN: usize = 32;
/// `ELM_MGR_EXTENSION_CONTRACT_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_EXTENSION_CONTRACT_LEN: usize = 64;
/// `ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN: usize = ELM_MGR_EXTENSION_CONTRACT_LEN;
/// `ELM_MGR_EXTENSION_PAYLOAD_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_EXTENSION_PAYLOAD_LEN: usize = 256;
/// `ELM_NEXUS_CONTRACT_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_NEXUS_CONTRACT_LEN: usize = 64;
/// `ELM_RUNTIME_LOG_MESSAGE_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_RUNTIME_LOG_MESSAGE_LEN: usize = 256;
/// `ELM_MGR_MAX_PAYLOAD` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_MGR_MAX_PAYLOAD: usize = 256 * 1024;
/// `ELM_MGR_MAX_INPUT` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_MGR_MAX_INPUT: usize = ELM_MGR_MAX_PAYLOAD + core::mem::size_of::<ElmMgrCallHeader>();
/// `ELM_IMAGE_SESSION_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_IMAGE_SESSION_ABI_VERSION: u16 = 1;
/// image session 封口和查询使用 SHA-256 内容摘要的算法编号。
pub const ELM_IMAGE_SESSION_HASH_SHA256: u16 = 1;
/// `ELM_IMAGE_SESSION_DIGEST_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_IMAGE_SESSION_DIGEST_LEN: usize = 32;
/// `ELM_IMAGE_SESSION_MAX_CHUNK` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_IMAGE_SESSION_MAX_CHUNK: usize = 64 * 1024;
/// `ELM_IMAGE_SESSION_MAX_LENGTH` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_IMAGE_SESSION_MAX_LENGTH: usize = 256 * 1024 * 1024;
/// `ELM_IMAGE_SESSION_MAX_ACTIVE` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_IMAGE_SESSION_MAX_ACTIVE: usize = 32;
/// `ELM_IMAGE_SESSION_MAX_PER_OWNER` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_IMAGE_SESSION_MAX_PER_OWNER: usize = 4;
/// `ELM_IMAGE_SESSION_MAX_RESERVED_BYTES` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_IMAGE_SESSION_MAX_RESERVED_BYTES: usize = 512 * 1024 * 1024;
/// `ELM_IMAGE_SESSION_DEFAULT_TTL_MS` 是调用方未指定时使用的默认结果存活时间，单位为毫秒。
pub const ELM_IMAGE_SESSION_DEFAULT_TTL_MS: u32 = 60_000;
/// `ELM_IMAGE_SESSION_MAX_TTL_MS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_IMAGE_SESSION_MAX_TTL_MS: u32 = 10 * 60_000;
/// `ELM_IMAGE_SESSION_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_IMAGE_SESSION_FLAG_NONE: u32 = 0;
/// `ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED: u32 = 1 << 0;
/// `ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK: u32 = ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED;
/// `ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE: u32 = 1 << 0;
/// `ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK: u32 = ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE;
/// `ELM_PROVIDER_PORT_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_PROVIDER_PORT_FLAG_NONE: u32 = 0;
/// `ELM_PROVIDER_FLAG_DYNAMIC` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_PROVIDER_FLAG_DYNAMIC: u16 = 1 << 0;
/// `ELM_PROVIDER_FLAG_KERNEL_BACKEND` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_PROVIDER_FLAG_KERNEL_BACKEND: u16 = 1 << 1;
/// `ELM_PROVIDER_FLAG_TODO_BACKEND` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_PROVIDER_FLAG_TODO_BACKEND: u16 = 1 << 2;
/// `ELM_PROVIDER_FLAG_NATIVE_BACKEND` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_PROVIDER_FLAG_NATIVE_BACKEND: u16 = 1 << 3;
/// `ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS` 是异步 provider 调用未指定时使用的默认超时，单位为毫秒。
pub const ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS: u32 = 5_000;
/// `ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS` 是当前 ELM 协议固定的数值；生产者与消费者必须按所属类型说明解释。
pub const ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS: u32 = 30_000;
/// `ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS: u32 = 60_000;
/// `ELM_PROVIDER_ASYNC_QUEUE_LIMIT` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_PROVIDER_ASYNC_QUEUE_LIMIT: u32 = 64;
/// `ELM_NATIVE_CAPABILITY_KIND_EXPORT` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_NATIVE_CAPABILITY_KIND_EXPORT: u32 = 1;
/// `ELM_NATIVE_CAPABILITY_KIND_IMPORT` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_NATIVE_CAPABILITY_KIND_IMPORT: u32 = 2;
/// `ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED: u32 = 1 << 0;
/// `ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD: u32 = 1 << 1;
/// 该能力记录描述由内核直接符号目录解析的 Rust 地址，不属于其他 ELM 的 export。
pub const ELM_NATIVE_CAPABILITY_FLAG_KERNEL_SYMBOL: u32 = 1 << 2;
/// `ELM_NATIVE_CAPABILITY_NAME_LEN` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_NATIVE_CAPABILITY_NAME_LEN: usize = 128;
/// `ELM_REPLACE_CELL_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_REPLACE_CELL_ABI_VERSION: u16 = 1;
/// 热替换请求显式允许可信新镜像绑定高权限内核符号。
pub const ELM_REPLACE_CELL_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS: u16 = 1 << 0;
/// v1 热替换请求支持的全部标志位。
pub const ELM_REPLACE_CELL_FLAGS_MASK: u16 = ELM_REPLACE_CELL_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS;
/// `ELM_REPLACE_MIGRATION_STATE_MAX` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_REPLACE_MIGRATION_STATE_MAX: usize = 64 * 1024;
/// `ELM_TODO_KIND_RUNTIME` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_TODO_KIND_RUNTIME: u32 = 1;
/// `ELM_TODO_KIND_PROVIDER` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_TODO_KIND_PROVIDER: u32 = 2;
/// `ELM_TODO_KIND_SOURCE` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_TODO_KIND_SOURCE: u32 = 3;
/// `ELM_TODO_KIND_NATIVE` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_TODO_KIND_NATIVE: u32 = 4;
/// `ELM_TODO_KIND_FRAMEWORK` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_TODO_KIND_FRAMEWORK: u32 = 5;
/// `ELM_TODO_REGISTRY_FLAG_TRUNCATED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_TODO_REGISTRY_FLAG_TRUNCATED: u32 = 1 << 0;
/// `ELM_TODO_FLAG_STATIC` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_TODO_FLAG_STATIC: u32 = 1 << 0;
/// `ELM_TODO_FLAG_ACTIVE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_TODO_FLAG_ACTIVE: u32 = 1 << 1;
/// `ELM_TODO_NAME_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_TODO_NAME_LEN: usize = 64;
/// `ELM_TODO_DETAIL_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_TODO_DETAIL_LEN: usize = 128;
/// `ELM_RUNTIME_TRACE_KIND_LIFECYCLE` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_LIFECYCLE: u32 = 1;
/// `ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL: u32 = 2;
/// `ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH: u32 = 3;
/// `ELM_RUNTIME_TRACE_KIND_REPLACE` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_REPLACE: u32 = 4;
/// `ELM_RUNTIME_TRACE_KIND_POLICY` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_POLICY: u32 = 5;
/// `ELM_RUNTIME_TRACE_KIND_RESOURCE` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_RESOURCE: u32 = 6;
/// `ELM_RUNTIME_TRACE_KIND_JOURNAL` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_JOURNAL: u32 = 7;
/// `ELM_RUNTIME_TRACE_KIND_TRUST` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_RUNTIME_TRACE_KIND_TRUST: u32 = 8;

/// `ELM_TRUST_FLAG_SEALED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_TRUST_FLAG_SEALED: u32 = 1 << 0;
/// `ELM_TRUST_FLAG_ALLOW_UNSIGNED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_TRUST_FLAG_ALLOW_UNSIGNED: u32 = 1 << 1;
/// `ELM_TRUST_FLAG_UNSIGNED_ACTIVE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_TRUST_FLAG_UNSIGNED_ACTIVE: u32 = 1 << 2;

/// `ELM_CELL_POLICY_ALLOW_LIFECYCLE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_LIFECYCLE: u32 = 1 << 0;
/// `ELM_CELL_POLICY_ALLOW_BIND` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_BIND: u32 = 1 << 1;
/// `ELM_CELL_POLICY_ALLOW_PROVIDER` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_PROVIDER: u32 = 1 << 2;
/// `ELM_CELL_POLICY_ALLOW_EVENT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_EVENT: u32 = 1 << 3;
/// `ELM_CELL_POLICY_ALLOW_EXTENSION` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_EXTENSION: u32 = 1 << 4;
/// `ELM_CELL_POLICY_ALLOW_NATIVE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_NATIVE: u32 = 1 << 5;
/// `ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE: u32 = 1 << 6;
/// `ELM_CELL_POLICY_ALLOW_POLICY_UPDATE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_POLICY_UPDATE: u32 = 1 << 7;
/// `ELM_CELL_POLICY_ALLOW_OBSERVE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_OBSERVE: u32 = 1 << 8;
/// `ELM_CELL_POLICY_ALLOW_MANAGEMENT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_MANAGEMENT: u32 = 1 << 9;
/// `ELM_CELL_POLICY_ALLOW_ALL` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_CELL_POLICY_ALLOW_ALL: u32 = ELM_CELL_POLICY_ALLOW_LIFECYCLE
    | ELM_CELL_POLICY_ALLOW_BIND
    | ELM_CELL_POLICY_ALLOW_PROVIDER
    | ELM_CELL_POLICY_ALLOW_EVENT
    | ELM_CELL_POLICY_ALLOW_EXTENSION
    | ELM_CELL_POLICY_ALLOW_NATIVE
    | ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE
    | ELM_CELL_POLICY_ALLOW_POLICY_UPDATE
    | ELM_CELL_POLICY_ALLOW_OBSERVE;
/// `ELM_CELL_POLICY_ALLOWED_ACTIONS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_CELL_POLICY_ALLOWED_ACTIONS_MASK: u32 =
    ELM_CELL_POLICY_ALLOW_ALL | ELM_CELL_POLICY_ALLOW_MANAGEMENT;

/// `ELM_CELL_POLICY_FLAG_LOCKED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_CELL_POLICY_FLAG_LOCKED: u32 = 1 << 0;
/// `ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION: u32 = 1 << 1;
/// `ELM_CELL_POLICY_FLAG_AUDIT_ALL` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_CELL_POLICY_FLAG_AUDIT_ALL: u32 = 1 << 2;
/// `ELM_CELL_POLICY_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_CELL_POLICY_FLAGS_MASK: u32 = ELM_CELL_POLICY_FLAG_LOCKED
    | ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION
    | ELM_CELL_POLICY_FLAG_AUDIT_ALL;

/// `ELM_PROVIDER_POLICY_REGISTER` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_PROVIDER_POLICY_REGISTER: u32 = 1 << 0;
/// `ELM_PROVIDER_POLICY_UNREGISTER` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_PROVIDER_POLICY_UNREGISTER: u32 = 1 << 1;
/// `ELM_PROVIDER_POLICY_INVOKE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_PROVIDER_POLICY_INVOKE: u32 = 1 << 2;
/// `ELM_PROVIDER_POLICY_ASYNC` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_PROVIDER_POLICY_ASYNC: u32 = 1 << 3;
/// `ELM_PROVIDER_POLICY_SNAPSHOT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_PROVIDER_POLICY_SNAPSHOT: u32 = 1 << 4;
/// `ELM_PROVIDER_POLICY_ALL` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_PROVIDER_POLICY_ALL: u32 = ELM_PROVIDER_POLICY_REGISTER
    | ELM_PROVIDER_POLICY_UNREGISTER
    | ELM_PROVIDER_POLICY_INVOKE
    | ELM_PROVIDER_POLICY_ASYNC
    | ELM_PROVIDER_POLICY_SNAPSHOT;

/// `ELM_EXTENSION_POLICY_ATTACH` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_EXTENSION_POLICY_ATTACH: u32 = 1 << 0;
/// `ELM_EXTENSION_POLICY_ACCEPT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_EXTENSION_POLICY_ACCEPT: u32 = 1 << 1;
/// `ELM_EXTENSION_POLICY_DETACH` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_EXTENSION_POLICY_DETACH: u32 = 1 << 2;
/// `ELM_EXTENSION_POLICY_DISPATCH` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_EXTENSION_POLICY_DISPATCH: u32 = 1 << 3;
/// `ELM_EXTENSION_POLICY_MIXIN_PATCH` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_EXTENSION_POLICY_MIXIN_PATCH: u32 = 1 << 4;
/// `ELM_EXTENSION_POLICY_ALL` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_EXTENSION_POLICY_ALL: u32 = ELM_EXTENSION_POLICY_ATTACH
    | ELM_EXTENSION_POLICY_ACCEPT
    | ELM_EXTENSION_POLICY_DETACH
    | ELM_EXTENSION_POLICY_DISPATCH
    | ELM_EXTENSION_POLICY_MIXIN_PATCH;

/// `ELM_NATIVE_POLICY_EXECUTE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_NATIVE_POLICY_EXECUTE: u32 = 1 << 0;
/// `ELM_NATIVE_POLICY_IMPORT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_NATIVE_POLICY_IMPORT: u32 = 1 << 1;
/// `ELM_NATIVE_POLICY_EXPORT` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_NATIVE_POLICY_EXPORT: u32 = 1 << 2;
/// `ELM_NATIVE_POLICY_REPLACE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_NATIVE_POLICY_REPLACE: u32 = 1 << 3;
/// `ELM_NATIVE_POLICY_MIXIN_PATCH` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_NATIVE_POLICY_MIXIN_PATCH: u32 = 1 << 4;
/// `ELM_NATIVE_POLICY_ALL` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_NATIVE_POLICY_ALL: u32 = ELM_NATIVE_POLICY_EXECUTE
    | ELM_NATIVE_POLICY_IMPORT
    | ELM_NATIVE_POLICY_EXPORT
    | ELM_NATIVE_POLICY_REPLACE
    | ELM_NATIVE_POLICY_MIXIN_PATCH;

/// `ELM_RESOURCE_POLICY_QUERY` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_RESOURCE_POLICY_QUERY: u32 = 1 << 0;
/// `ELM_RESOURCE_POLICY_UPDATE` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_RESOURCE_POLICY_UPDATE: u32 = 1 << 1;
/// `ELM_RESOURCE_POLICY_OWN` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_RESOURCE_POLICY_OWN: u32 = 1 << 2;
/// `ELM_RESOURCE_POLICY_ALL` 策略能力位；策略求值器以此决定是否允许对应操作。
pub const ELM_RESOURCE_POLICY_ALL: u32 =
    ELM_RESOURCE_POLICY_QUERY | ELM_RESOURCE_POLICY_UPDATE | ELM_RESOURCE_POLICY_OWN;

/// 审计记录中表示授权由 `kernel` 主体链提供的稳定编码。
pub const ELM_AUDIT_AUTHORITY_KERNEL: u32 = 1;
/// 审计记录中表示授权由 `user_admin` 主体链提供的稳定编码。
pub const ELM_AUDIT_AUTHORITY_USER_ADMIN: u32 = 2;
/// 审计记录中表示授权由 `manager` 主体链提供的稳定编码。
pub const ELM_AUDIT_AUTHORITY_MANAGER: u32 = 3;
/// 审计记录中表示授权由 `ancestor` 主体链提供的稳定编码。
pub const ELM_AUDIT_AUTHORITY_ANCESTOR: u32 = 4;
/// 审计记录中表示授权由 `self` 主体链提供的稳定编码。
pub const ELM_AUDIT_AUTHORITY_SELF: u32 = 5;
/// 审计记录中表示授权由 `delegated_manager` 主体链提供的稳定编码。
pub const ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER: u32 = 6;
/// `ELM_AUDIT_FLAG_OPERATION` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_AUDIT_FLAG_OPERATION: u32 = 1 << 0;
/// `ELM_AUDIT_FLAG_AUTHORIZATION` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_AUDIT_FLAG_AUTHORIZATION: u32 = 1 << 1;

/// `ELM_HEALTH_FLAG_HAS_FAILURES` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_HEALTH_FLAG_HAS_FAILURES: u32 = 1 << 0;

/// core health 快照中选择 `graph` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_GRAPH: u32 = 1;
/// core health 快照中选择 `cells` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_CELLS: u32 = 2;
/// core health 快照中选择 `ports` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_PORTS: u32 = 3;
/// core health 快照中选择 `providers` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_PROVIDERS: u32 = 4;
/// core health 快照中选择 `bindings` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_BINDINGS: u32 = 5;
/// core health 快照中选择 `runtime_ports` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_RUNTIME_PORTS: u32 = 6;
/// core health 快照中选择 `menu` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_MENU: u32 = 7;
/// core health 快照中选择 `events` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_EVENTS: u32 = 8;
/// core health 快照中选择 `audits` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_AUDITS: u32 = 9;
/// core health 快照中选择 `native_capabilities` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_NATIVE_CAPABILITIES: u32 = 10;
/// core health 快照中选择 `todo_registry` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_TODO_REGISTRY: u32 = 11;
/// core health 快照中选择 `trust` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_TRUST: u32 = 12;
/// core health 快照中选择 `projection_sources` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_PROJECTION_SOURCES: u32 = 13;
/// core health 快照中选择 `journal` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_JOURNAL: u32 = 14;
/// core health 快照中选择 `resources` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_RESOURCES: u32 = 15;
/// core health 快照中选择 `executions` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_EXECUTIONS: u32 = 16;
/// core health 快照中选择 `sequences` 不变量检查器的编号。
pub const ELM_HEALTH_CHECK_SEQUENCES: u32 = 17;

/// health 记录中表示 `none` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_NONE: u64 = 0;
/// health 记录中表示 `graph_invalid` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_GRAPH_INVALID: u64 = 1;
/// health 记录中表示 `missing_object` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_MISSING_OBJECT: u64 = 2;
/// health 记录中表示 `duplicate_object` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_DUPLICATE_OBJECT: u64 = 3;
/// health 记录中表示 `dangling_reference` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_DANGLING_REFERENCE: u64 = 4;
/// `ELM_HEALTH_DETAIL_CONTRACT_INVALID` 的规范 identifier 或契约名称；比较时使用完整字节串而不是截断哈希。
pub const ELM_HEALTH_DETAIL_CONTRACT_INVALID: u64 = 5;
/// health 记录中表示 `sequence_invalid` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_SEQUENCE_INVALID: u64 = 6;
/// `ELM_HEALTH_DETAIL_KIND_MISMATCH` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_HEALTH_DETAIL_KIND_MISMATCH: u64 = 7;
/// health 记录中表示 `state_invalid` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_STATE_INVALID: u64 = 8;
/// health 记录中表示 `counter_exhausted` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED: u64 = 9;
/// health 记录中表示 `persistence_failed` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_PERSISTENCE_FAILED: u64 = 10;
/// health 记录中表示 `resource_leak` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_RESOURCE_LEAK: u64 = 11;
/// health 记录中表示 `stuck_reference` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_STUCK_REFERENCE: u64 = 12;
/// health 记录中表示 `dropped_records` 诊断原因的细节码。
pub const ELM_HEALTH_DETAIL_DROPPED_RECORDS: u64 = 13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmMgrCallKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmMgrCallKind {
    /// 选择 `QueryMenu` 管理调用，用于查询 `Menu` 对应的运行时对象或快照。
    QueryMenu = 1,
    /// 选择 `LoadCell` 管理调用，用于装载 `Cell` 对应的运行时对象或快照。
    LoadCell = 2,
    /// 选择 `DetachCell` 管理调用，用于卸载 `Cell` 对应的运行时对象或快照。
    DetachCell = 3,
    /// 选择 `PauseCell` 管理调用，用于暂停 `Cell` 对应的运行时对象或快照。
    PauseCell = 4,
    /// 选择 `ResumeCell` 管理调用，用于恢复 `Cell` 对应的运行时对象或快照。
    ResumeCell = 5,
    /// 选择 `ReplaceCell` 管理调用，用于热替换 `Cell` 对应的运行时对象或快照。
    ReplaceCell = 6,
    /// 选择 `QueryTopology` 管理调用，用于查询 `Topology` 对应的运行时对象或快照。
    QueryTopology = 7,
    /// 选择 `QueryPolicy` 管理调用，用于查询 `Policy` 对应的运行时对象或快照。
    QueryPolicy = 8,
    /// 选择 `PreflightLifecycle` 管理调用，用于预检 `Lifecycle` 对应的运行时对象或快照。
    PreflightLifecycle = 9,
    /// 选择 `QueryAudit` 管理调用，用于查询 `Audit` 对应的运行时对象或快照。
    QueryAudit = 10,
    /// 选择 `QueryNexusBindings` 管理调用，用于查询 `NexusBindings` 对应的运行时对象或快照。
    QueryNexusBindings = 11,
    /// 选择 `PreflightBind` 管理调用，用于预检 `Bind` 对应的运行时对象或快照。
    PreflightBind = 12,
    /// 选择 `CommitBind` 管理调用，用于提交 `Bind` 对应的运行时对象或快照。
    CommitBind = 13,
    /// 选择 `PreflightUnbind` 管理调用，用于预检 `Unbind` 对应的运行时对象或快照。
    PreflightUnbind = 14,
    /// 选择 `CommitUnbind` 管理调用，用于提交 `Unbind` 对应的运行时对象或快照。
    CommitUnbind = 15,
    /// 选择 `SubmitRuntimeLog` 管理调用，用于提交 `RuntimeLog` 对应的运行时对象或快照。
    SubmitRuntimeLog = 16,
    /// 选择 `ReadRuntimeEvent` 管理调用，用于读取 `RuntimeEvent` 对应的运行时对象或快照。
    ReadRuntimeEvent = 17,
    /// 选择 `AckRuntimeEvent` 管理调用，用于确认 `RuntimeEvent` 对应的运行时对象或快照。
    AckRuntimeEvent = 18,
    /// 选择 `QueryRuntimePorts` 管理调用，用于查询 `RuntimePorts` 对应的运行时对象或快照。
    QueryRuntimePorts = 19,
    /// 选择 `RegisterProviderPort` 管理调用，用于注册 `ProviderPort` 对应的运行时对象或快照。
    RegisterProviderPort = 20,
    /// 选择 `UnregisterProviderPort` 管理调用，用于注销 `ProviderPort` 对应的运行时对象或快照。
    UnregisterProviderPort = 21,
    /// 选择 `QueryProviderPorts` 管理调用，用于查询 `ProviderPorts` 对应的运行时对象或快照。
    QueryProviderPorts = 22,
    /// 选择 `InvokeProvider` 管理调用，用于调用 `Provider` 对应的运行时对象或快照。
    InvokeProvider = 23,
    /// 选择 `QueryProviderStats` 管理调用，用于查询 `ProviderStats` 对应的运行时对象或快照。
    QueryProviderStats = 24,
    /// 选择 `QueryHealth` 管理调用，用于查询 `Health` 对应的运行时对象或快照。
    QueryHealth = 25,
    /// 选择 `SubmitProviderCall` 管理调用，用于提交 `ProviderCall` 对应的运行时对象或快照。
    SubmitProviderCall = 26,
    /// 选择 `PollProviderReply` 管理调用，用于轮询 `ProviderReply` 对应的运行时对象或快照。
    PollProviderReply = 27,
    /// 选择 `CancelProviderCall` 管理调用，用于取消 `ProviderCall` 对应的运行时对象或快照。
    CancelProviderCall = 28,
    /// 选择 `QueryProviderQueue` 管理调用，用于查询 `ProviderQueue` 对应的运行时对象或快照。
    QueryProviderQueue = 29,
    /// 选择 `QueryApiRegistry` 管理调用，用于查询 `ApiRegistry` 对应的运行时对象或快照。
    QueryApiRegistry = 30,
    /// 选择 `SubscribeEvent` 管理调用，用于订阅 `Event` 对应的运行时对象或快照。
    SubscribeEvent = 31,
    /// 选择 `UnsubscribeEvent` 管理调用，用于取消订阅 `Event` 对应的运行时对象或快照。
    UnsubscribeEvent = 32,
    /// 选择 `QueryEventSubscriptions` 管理调用，用于查询 `EventSubscriptions` 对应的运行时对象或快照。
    QueryEventSubscriptions = 33,
    /// 选择 `ReadSubscribedEvents` 管理调用，用于读取 `SubscribedEvents` 对应的运行时对象或快照。
    ReadSubscribedEvents = 34,
    /// 选择 `QueryProviderSnapshot` 管理调用，用于查询 `ProviderSnapshot` 对应的运行时对象或快照。
    QueryProviderSnapshot = 35,
    /// 选择 `QueryNativeCapabilities` 管理调用，用于查询 `NativeCapabilities` 对应的运行时对象或快照。
    QueryNativeCapabilities = 36,
    /// 选择 `QueryTodoRegistry` 管理调用，用于查询 `TodoRegistry` 对应的运行时对象或快照。
    QueryTodoRegistry = 37,
    /// 选择 `QueryExtensions` 管理调用，用于查询 `Extensions` 对应的运行时对象或快照。
    QueryExtensions = 38,
    /// 选择 `PreflightExtensionAttach` 管理调用，用于预检 `ExtensionAttach` 对应的运行时对象或快照。
    PreflightExtensionAttach = 39,
    /// 选择 `CommitExtensionAttach` 管理调用，用于提交 `ExtensionAttach` 对应的运行时对象或快照。
    CommitExtensionAttach = 40,
    /// 选择 `CommitExtensionDetach` 管理调用，用于提交 `ExtensionDetach` 对应的运行时对象或快照。
    CommitExtensionDetach = 41,
    /// 选择 `DispatchExtension` 管理调用，用于分发 `Extension` 对应的运行时对象或快照。
    DispatchExtension = 42,
    /// 选择 `QueryFaultDump` 管理调用，用于查询 `FaultDump` 对应的运行时对象或快照。
    QueryFaultDump = 43,
    /// 选择 `QueryLifecycleTrace` 管理调用，用于查询 `LifecycleTrace` 对应的运行时对象或快照。
    QueryLifecycleTrace = 44,
    /// 选择 `QueryProviderCallTrace` 管理调用，用于查询 `ProviderCallTrace` 对应的运行时对象或快照。
    QueryProviderCallTrace = 45,
    /// 选择 `QueryMixinTrace` 管理调用，用于查询 `MixinTrace` 对应的运行时对象或快照。
    QueryMixinTrace = 46,
    /// 选择 `QueryReplaceTrace` 管理调用，用于查询 `ReplaceTrace` 对应的运行时对象或快照。
    QueryReplaceTrace = 47,
    /// 选择 `QueryPolicyTrace` 管理调用，用于查询 `PolicyTrace` 对应的运行时对象或快照。
    QueryPolicyTrace = 48,
    /// 选择 `QueryResourceDiagnostics` 管理调用，用于查询 `ResourceDiagnostics` 对应的运行时对象或快照。
    QueryResourceDiagnostics = 49,
    /// 选择 `QueryRuntimeJournal` 管理调用，用于查询 `RuntimeJournal` 对应的运行时对象或快照。
    QueryRuntimeJournal = 50,
    /// 选择 `QueryCellPolicy` 管理调用，用于查询 `CellPolicy` 对应的运行时对象或快照。
    QueryCellPolicy = 51,
    /// 选择 `UpdateCellPolicy` 管理调用，用于更新 `CellPolicy` 对应的运行时对象或快照。
    UpdateCellPolicy = 52,
    /// 选择 `QueryResourceBudget` 管理调用，用于查询 `ResourceBudget` 对应的运行时对象或快照。
    QueryResourceBudget = 53,
    /// 选择 `UpdateResourceBudget` 管理调用，用于更新 `ResourceBudget` 对应的运行时对象或快照。
    UpdateResourceBudget = 54,
    /// 选择 `QueryTrustState` 管理调用，用于查询 `TrustState` 对应的运行时对象或快照。
    QueryTrustState = 55,
    /// 选择 `BeginImageSession` 管理调用，用于开始 `ImageSession` 对应的运行时对象或快照。
    BeginImageSession = 56,
    /// 选择 `WriteImageSession` 管理调用，用于写入 `ImageSession` 对应的运行时对象或快照。
    WriteImageSession = 57,
    /// 选择 `SealImageSession` 管理调用，用于封口 `ImageSession` 对应的运行时对象或快照。
    SealImageSession = 58,
    /// 选择 `AbortImageSession` 管理调用，用于中止 `ImageSession` 对应的运行时对象或快照。
    AbortImageSession = 59,
    /// 选择 `QueryImageSession` 管理调用，用于查询 `ImageSession` 对应的运行时对象或快照。
    QueryImageSession = 60,
}

impl ElmMgrCallKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
/// `ElmImageSessionState` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmImageSessionState {
    /// `Uploading` 表示 `ElmImageSessionState` 的生命周期状态：`uploading`。
    Uploading = 1,
    /// `Sealed` 表示 `ElmImageSessionState` 的生命周期状态：`sealed`。
    Sealed = 2,
}

impl ElmImageSessionState {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
/// `ElmImageSessionBeginRequestV1` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmImageSessionBeginRequestV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `hash_alg` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub hash_alg: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `total_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub total_len: u64,
    /// 对象或结果的存活时间，单位为毫秒。
    pub ttl_ms: u32,
    /// `digest_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub digest_len: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// `expected_digest` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub expected_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u64,
}

impl ElmImageSessionBeginRequestV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmImageSessionWriteRequestV1` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmImageSessionWriteRequestV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// `session_id` 所指对象的稳定运行时标识符。
    pub session_id: u64,
    /// `offset` 是相对于所属块、段或文件起点的字节偏移；与长度相加前必须检查溢出。
    pub offset: u64,
    /// `chunk_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub chunk_len: u32,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
}

impl ElmImageSessionWriteRequestV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmImageSessionRequestV1` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmImageSessionRequestV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// `session_id` 所指对象的稳定运行时标识符。
    pub session_id: u64,
}

impl ElmImageSessionRequestV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// image session 的 owner、状态、长度、摘要、TTL 和资源占用快照。
pub struct ElmImageSessionInfoV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u16,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// `session_id` 所指对象的稳定运行时标识符。
    pub session_id: u64,
    /// `total_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub total_len: u64,
    /// `written_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub written_len: u64,
    /// `created_at_ns` 使用纳秒单位；具体时钟域由所属记录定义。
    pub created_at_ns: u64,
    /// `expires_at_ns` 使用纳秒单位；具体时钟域由所属记录定义。
    pub expires_at_ns: u64,
    /// `hash_alg` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub hash_alg: u16,
    /// `digest_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub digest_len: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `expected_digest` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub expected_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
    /// `actual_digest` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub actual_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// trust store 当前 anchor、撤销、epoch reservation、持久化和策略状态摘要。
pub struct ElmTrustRuntimeInfoV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `anchor_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub anchor_count: u32,
    /// `revoked_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub revoked_count: u32,
    /// `accepted_epoch_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub accepted_epoch_count: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmTrustRuntimeInfoV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderAsyncState` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmProviderAsyncState {
    /// `Queued` 表示 `ElmProviderAsyncState` 的生命周期状态：`queued`。
    Queued = 1,
    /// `Running` 表示 `ElmProviderAsyncState` 的生命周期状态：`running`。
    Running = 2,
    /// `Completed` 表示 `ElmProviderAsyncState` 的生命周期状态：`completed`。
    Completed = 3,
    /// `Failed` 表示 `ElmProviderAsyncState` 的生命周期状态：`failed`。
    Failed = 4,
    /// `Canceled` 表示 `ElmProviderAsyncState` 的生命周期状态：`canceled`。
    Canceled = 5,
    /// `Expired` 表示 `ElmProviderAsyncState` 的生命周期状态：`expired`。
    Expired = 6,
}

impl ElmProviderAsyncState {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
/// `ElmLifecycleAction` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmLifecycleAction {
    /// `Pause` 表示 `ElmLifecycleAction` 的生命周期动作：`pause`。
    Pause = 1,
    /// `Resume` 表示 `ElmLifecycleAction` 的生命周期动作：`resume`。
    Resume = 2,
    /// `Detach` 表示 `ElmLifecycleAction` 的生命周期动作：`detach`。
    Detach = 3,
    /// `Replace` 表示 `ElmLifecycleAction` 的生命周期动作：`replace`。
    Replace = 4,
}

impl ElmLifecycleAction {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Pause),
            2 => Some(Self::Resume),
            3 => Some(Self::Detach),
            4 => Some(Self::Replace),
            _ => None,
        }
    }

    /// 执行 `bit` 定义的模型或协议操作；返回值反映校验后的结果。
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
/// `ElmMgrRelationKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmMgrRelationKind {
    /// `Parent` 表示 `ElmMgrRelationKind` 的对象类别：`parent`。
    Parent = 1,
    /// `Dependency` 表示 `ElmMgrRelationKind` 的对象类别：`dependency`。
    Dependency = 2,
    /// `Extension` 表示 `ElmMgrRelationKind` 的对象类别：`extension`。
    Extension = 3,
    /// `ExtensionPoint` 表示 `ElmMgrRelationKind` 的对象类别：`extension point`。
    ExtensionPoint = 4,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrCallHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMgrCallHeader {
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmMgrCallHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(kind: ElmMgrCallKind, payload_len: u32) -> Self {
        Self {
            kind: kind as u32,
            flags: 0,
            payload_len,
            reserved: 0,
        }
    }

    /// 构造不携带有效载荷的空值，供调用方继续填写必要字段。
    pub const fn empty(kind: ElmMgrCallKind) -> Self {
        Self::new(kind, 0)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmLifecycleRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmLifecycleRequest {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmLifecycleRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmLifecycleResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmLifecycleResponse {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 操作成功或失败收口后预期/实际到达的 cell 状态。
    pub final_state: u32,
    /// `revoked_leases` 保存所属对象声明或快照中的有序记录集合。
    pub revoked_leases: u32,
    /// `removed_menu_items` 保存所属对象声明或快照中的有序记录集合。
    pub removed_menu_items: u32,
    /// `reason` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub reason: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmLifecycleResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmReplaceCellRequestV1` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmReplaceCellRequestV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// `source_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub source_kind: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// `target_cell_id` 所指对象的稳定运行时标识符。
    pub target_cell_id: u64,
    /// `migration_limit` 是对应缓冲区、队列或记录集合的最大容量。
    pub migration_limit: u32,
    /// `source_payload_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub source_payload_len: u32,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u64,
}

impl ElmReplaceCellRequestV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 显式请求允许可信的新 generation 绑定高权限内核符号。
    pub const fn with_privileged_symbol_authorization(mut self) -> Self {
        self.flags |= ELM_REPLACE_CELL_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS;
        self
    }

    /// 返回该替换事务是否请求高权限内核符号授权。
    pub const fn authorizes_privileged_symbols(self) -> bool {
        self.flags & ELM_REPLACE_CELL_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS != 0
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmReplaceCellResponseV1` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmReplaceCellResponseV1 {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 操作成功或失败收口后预期/实际到达的 cell 状态。
    pub final_state: u32,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// `migrated_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub migrated_len: u32,
    /// `reason` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub reason: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
}

impl ElmReplaceCellResponseV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmLifecyclePlanRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmLifecyclePlanRequest {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 请求执行或审计记录中的动作编号。
    pub action: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
}

impl ElmLifecyclePlanRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmLifecyclePlanResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmLifecyclePlanResponse {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 请求执行或审计记录中的动作编号。
    pub action: u32,
    /// `allowed` 表示该条件在当前快照或计划中是否成立。
    pub allowed: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 操作成功或失败收口后预期/实际到达的 cell 状态。
    pub final_state: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
    /// `affected_children` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub affected_children: u32,
    /// `affected_dependents` 保存所属对象声明或快照中的有序记录集合。
    pub affected_dependents: u32,
    /// `affected_extensions` 保存所属对象声明或快照中的有序记录集合。
    pub affected_extensions: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmLifecyclePlanResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `affected` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_affected(mut self, children: u32, dependents: u32, extensions: u32) -> Self {
        self.affected_children = children;
        self.affected_dependents = dependents;
        self.affected_extensions = extensions;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrPolicyInfo` 表示 ELM 的授权和约束策略；更新必须经过管理权限、代际与审计检查。
pub struct ElmMgrPolicyInfo {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// `supported_actions` 保存所属对象声明或快照中的有序记录集合。
    pub supported_actions: u32,
    /// `policy_flags` 标志位集合；必须拒绝相应有效掩码之外的未知位。
    pub policy_flags: u64,
    /// 阻止当前操作提交的全部原因位集合。
    pub blocker_mask: u64,
    /// `audit_capacity` 是对应缓冲区、队列或记录集合的最大容量。
    pub audit_capacity: u32,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
}

impl ElmMgrPolicyInfo {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmRuntimeTraceHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmRuntimeTraceHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// `dropped_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub dropped_count: u32,
    /// `trace_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub trace_kind: u32,
    /// `last_sequence` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub last_sequence: u64,
}

impl ElmRuntimeTraceHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmRuntimeTraceRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmRuntimeTraceRecord {
    /// 单调递增的序列号，用于排序、游标推进和丢失检测。
    pub sequence: u64,
    /// 以纳秒表示的时间戳；时钟域由所属记录定义。
    pub timestamp_ns: u64,
    /// `trace_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub trace_kind: u32,
    /// 请求执行或审计记录中的动作编号。
    pub action: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// `subject_id` 所指对象的稳定运行时标识符。
    pub subject_id: u64,
    /// `aux_id` 所指对象的稳定运行时标识符。
    pub aux_id: u64,
    /// `value` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub value: u64,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
}

impl ElmRuntimeTraceRecord {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmCellPolicyRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmCellPolicyRequest {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmCellPolicyRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmCellPolicyV1` 表示 ELM 的授权和约束策略；更新必须经过管理权限、代际与审计检查。
pub struct ElmCellPolicyV1 {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// `policy_epoch` 是单调发布或策略纪元，用于拒绝回滚和陈旧更新。
    pub policy_epoch: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 当前上下文允许执行的管理动作位集合。
    pub allowed_actions: u32,
    /// `provider_flags` 标志位集合；必须拒绝相应有效掩码之外的未知位。
    pub provider_flags: u32,
    /// `extension_flags` 标志位集合；必须拒绝相应有效掩码之外的未知位。
    pub extension_flags: u32,
    /// `native_flags` 标志位集合；必须拒绝相应有效掩码之外的未知位。
    pub native_flags: u32,
    /// `resource_flags` 标志位集合；必须拒绝相应有效掩码之外的未知位。
    pub resource_flags: u32,
    /// 该单元获准直接绑定的内核符号能力组；调用期间不再重复查询该字段。
    pub kernel_symbol_capabilities: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
}

impl ElmCellPolicyV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
            kernel_symbol_capabilities: kernel_symbols::capability::SAFE_DEFAULT,
            status,
            reserved: 0,
            blockers,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmResourceBudgetRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmResourceBudgetRequest {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmResourceBudgetRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmResourceBudgetResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmResourceBudgetResponse {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
    /// 该 cell 当前生效的资源预算。
    pub budget: ElmResourceBudget,
    /// 该 cell 当前已核算的资源用量快照。
    pub usage: crate::resource::ElmResourceUsage,
}

impl ElmResourceBudgetResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmResourceBudgetUpdateRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmResourceBudgetUpdateRequest {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 该 cell 当前生效的资源预算。
    pub budget: ElmResourceBudget,
}

impl ElmResourceBudgetUpdateRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmFaultDumpHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmFaultDumpHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `dropped_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub dropped_count: u32,
    /// `last_sequence` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub last_sequence: u64,
}

impl ElmFaultDumpHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmFaultDumpRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmFaultDumpRecord {
    /// 单调递增的序列号，用于排序、游标推进和丢失检测。
    pub sequence: u64,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 故障或追踪事件发生时的程序计数器。
    pub pc: u64,
    /// 故障访问地址或经验证的目标地址。
    pub addr: u64,
    /// 原生故障恢复时记录的返回程序计数器。
    pub return_pc: u64,
    /// 当前生命周期或迁移阶段编码。
    pub phase: u32,
    /// `code` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub code: u32,
    /// 原生故障恢复时记录的返回栈指针。
    pub return_sp: u64,
    /// `cpu_id` 所指对象的稳定运行时标识符。
    pub cpu_id: u32,
    /// `depth` 是当前层级或队列深度；消费者必须结合对应上限判断资源压力。
    pub depth: u32,
    /// `reason` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub reason: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmFaultDumpRecord {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

/// `ELM_EXTENSION_RECORD_KIND_POINT` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_EXTENSION_RECORD_KIND_POINT: u32 = 1;
/// `ELM_EXTENSION_RECORD_KIND_EDGE` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_EXTENSION_RECORD_KIND_EDGE: u32 = 2;
/// `ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION: u32 = 1 << 0;
/// `ELM_EXTENSION_DISPATCH_FLAG_ALLOW_EMPTY` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EXTENSION_DISPATCH_FLAG_ALLOW_EMPTY: u32 = 1 << 1;
/// `ELM_EXTENSION_DISPATCH_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_EXTENSION_DISPATCH_FLAGS_MASK: u32 =
    ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION | ELM_EXTENSION_DISPATCH_FLAG_ALLOW_EMPTY;
/// mixin handler 回复中请求 `continue` 控制行为的标志位。
pub const ELM_MIXIN_REPLY_CONTINUE: u32 = 0;
/// mixin handler 回复中请求 `stop` 控制行为的标志位。
pub const ELM_MIXIN_REPLY_STOP: u32 = 1 << 0;
/// mixin handler 回复中请求 `replace` 控制行为的标志位。
pub const ELM_MIXIN_REPLY_REPLACE: u32 = 1 << 1;
/// mixin handler 回复中请求 `deny` 控制行为的标志位。
pub const ELM_MIXIN_REPLY_DENY: u32 = 1 << 2;
/// `ELM_MIXIN_REPLY_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_MIXIN_REPLY_FLAGS_MASK: u32 =
    ELM_MIXIN_REPLY_STOP | ELM_MIXIN_REPLY_REPLACE | ELM_MIXIN_REPLY_DENY;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmExtensionSnapshotHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmExtensionSnapshotHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// `point_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub point_count: u32,
    /// `edge_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub edge_count: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmExtensionSnapshotHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmExtensionSnapshotRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmExtensionSnapshotRecord {
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// `target_cell_id` 所指对象的稳定运行时标识符。
    pub target_cell_id: u64,
    /// `extension_cell_id` 所指对象的稳定运行时标识符。
    pub extension_cell_id: u64,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: u32,
    /// 同一扩展点中的调度优先级；排序规则由扩展运行时定义。
    pub priority: i32,
    /// `point_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub point_len: u16,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// `handler_contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub handler_contract_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_MGR_EXTENSION_CONTRACT_LEN],
    /// mixin/provider 处理器自身的调用契约。
    pub handler_contract: [u8; ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
}

impl ElmExtensionSnapshotRecord {
    /// 执行 `point` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn point(owner: u64, point: &str, contract: &str) -> Self {
        Self::point_with_mode(owner, point, contract, ElmMixinMode::Chain)
    }

    /// 执行 `point_with_mode` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `edge` 定义的模型或协议操作；返回值反映校验后的结果。
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
    /// 执行 `edge_with_dispatch` 定义的模型或协议操作；返回值反映校验后的结果。
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
/// `ElmExtensionAttachRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmExtensionAttachRequest {
    /// `extension_cell_id` 所指对象的稳定运行时标识符。
    pub extension_cell_id: u64,
    /// `target_cell_id` 所指对象的稳定运行时标识符。
    pub target_cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 同一扩展点中的调度优先级；排序规则由扩展运行时定义。
    pub priority: i32,
    /// `point_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub point_len: u16,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// `handler_contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub handler_contract_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_MGR_EXTENSION_CONTRACT_LEN],
    /// mixin/provider 处理器自身的调用契约。
    pub handler_contract: [u8; ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
}

impl ElmExtensionAttachRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `dispatch` 并返回更新后的值，便于构建器式初始化。
    pub fn with_dispatch(mut self, handler_contract: &str, priority: i32) -> Self {
        self.priority = priority;
        self.handler_contract.fill(0);
        self.handler_contract_len = copy_str(handler_contract, &mut self.handler_contract) as u16;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmExtensionDetachRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmExtensionDetachRequest {
    /// `extension_cell_id` 所指对象的稳定运行时标识符。
    pub extension_cell_id: u64,
    /// `target_cell_id` 所指对象的稳定运行时标识符。
    pub target_cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `point_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub point_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
}

impl ElmExtensionDetachRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmExtensionAttachResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmExtensionAttachResponse {
    /// `extension_cell_id` 所指对象的稳定运行时标识符。
    pub extension_cell_id: u64,
    /// `target_cell_id` 所指对象的稳定运行时标识符。
    pub target_cell_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `allowed` 表示该条件在当前快照或计划中是否成立。
    pub allowed: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
}

impl ElmExtensionAttachResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

/// `ElmExtensionDetachResponse` 为该调用路径使用的规范类型别名，统一公开签名并避免重复表达底层布局。
pub type ElmExtensionDetachResponse = ElmExtensionAttachResponse;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmExtensionDispatchRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmExtensionDispatchRequest {
    /// `target_cell_id` 所指对象的稳定运行时标识符。
    pub target_cell_id: u64,
    /// `extension_cell_id` 所指对象的稳定运行时标识符。
    pub extension_cell_id: u64,
    /// 契约内部的操作编号。
    pub opcode: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `point_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub point_len: u16,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: [u8; ELM_MGR_EXTENSION_POINT_LEN],
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_MGR_EXTENSION_CONTRACT_LEN],
    /// 固定容量的线格式载荷缓冲区；仅前 `payload_len` 字节有效。
    pub payload: [u8; ELM_MGR_EXTENSION_PAYLOAD_LEN],
    /// 保留字段；生产者必须写零，消费者在当前 ABI 必须验证为零。
    pub reserved2: u32,
}

impl ElmExtensionDispatchRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
            reserved2: 0,
        };
        out.point_len = copy_str(point, &mut out.point) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmExtensionDispatchResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmExtensionDispatchResponse {
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `matched_extensions` 保存所属对象声明或快照中的有序记录集合。
    pub matched_extensions: u32,
    /// `called_extensions` 保存所属对象声明或快照中的有序记录集合。
    pub called_extensions: u32,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
    /// 本次调用或 extension 分发产生的固定回复。
    pub reply: ElmReplyFrame,
}

impl ElmExtensionDispatchResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `mode` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_mode(mut self, mode: ElmMixinMode) -> Self {
        self.mode = mode as u32;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmNativeCapabilityHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmNativeCapabilityHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmNativeCapabilityHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmNativeCapabilityRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmNativeCapabilityRecord {
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// `peer_cell_id` 所指对象的稳定运行时标识符。
    pub peer_cell_id: u64,
    /// `requested_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub requested_version: u32,
    /// `selected_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub selected_version: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `name_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub name_len: u16,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: [u8; ELM_NATIVE_CAPABILITY_NAME_LEN],
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmNativeCapabilityRecord {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmTodoRegistryHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmTodoRegistryHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// `active_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub active_count: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmTodoRegistryHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(record_count: u32, active_count: u32, event_sequence: u64) -> Self {
        Self::new_with_flags(record_count, active_count, 0, event_sequence)
    }

    /// 执行 `new_with_flags` 定义的模型或协议操作；返回值反映校验后的结果。
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
/// `ElmTodoRegistryRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmTodoRegistryRecord {
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 阻止当前操作提交的首要原因码。
    pub blocker: u64,
    /// `subject_id` 所指对象的稳定运行时标识符。
    pub subject_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `name_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub name_len: u16,
    /// `detail_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub detail_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: [u8; ELM_TODO_NAME_LEN],
    /// 供诊断使用的细化原因码。
    pub detail: [u8; ELM_TODO_DETAIL_LEN],
}

impl ElmTodoRegistryRecord {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmMgrTopologyHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMgrTopologyHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `relation_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub relation_entry_size: u16,
    /// `relation_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub relation_count: u32,
    /// `cell_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub cell_count: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmMgrTopologyHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmMgrRelationRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmMgrRelationRecord {
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 关系、投影或调用的来源对象。
    pub source: u64,
    /// 关系、重定位或调用的目标对象。
    pub target: u64,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// `point_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub point_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_MGR_RELATION_CONTRACT_LEN],
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: [u8; ELM_MGR_RELATION_POINT_LEN],
}

impl ElmMgrRelationRecord {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmMgrAuditHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMgrAuditHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// `dropped_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub dropped_count: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// `last_sequence` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub last_sequence: u64,
}

impl ElmMgrAuditHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmMgrAuditRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmMgrAuditRecord {
    /// 单调递增的序列号，用于排序、游标推进和丢失检测。
    pub sequence: u64,
    /// 请求执行或审计记录中的动作编号。
    pub action: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
    /// 操作成功或失败收口后预期/实际到达的 cell 状态。
    pub final_state: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `actor_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub actor_kind: u32,
    /// 批准该动作的授权主体类别。
    pub authority: u32,
    /// `actor_id` 所指对象的稳定运行时标识符。
    pub actor_id: u64,
    /// `authority_id` 所指对象的稳定运行时标识符。
    pub authority_id: u64,
    /// `actor_generation` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub actor_generation: u64,
    /// `policy_epoch` 是单调发布或策略纪元，用于拒绝回滚和陈旧更新。
    pub policy_epoch: u64,
    /// `credential_id` 所指对象的稳定运行时标识符。
    pub credential_id: u64,
}

impl ElmMgrAuditRecord {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
            actor_kind: ElmPrincipalKind::Kernel as u32,
            authority: ELM_AUDIT_AUTHORITY_KERNEL,
            actor_id: 0,
            authority_id: 0,
            actor_generation: 0,
            policy_epoch: 0,
            credential_id: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// 设置 `authority` 并返回更新后的值，便于构建器式初始化。
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
/// `ElmNexusBindRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmNexusBindRequest {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmNexusBindRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmNexusBindPlanResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmNexusBindPlanResponse {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `allowed` 表示该条件在当前快照或计划中是否成立。
    pub allowed: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u64,
}

impl ElmNexusBindPlanResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmNexusUnbindRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmNexusUnbindRequest {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmNexusUnbindRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmNexusBindingSnapshotHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmNexusBindingSnapshotHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `binding_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub binding_entry_size: u16,
    /// `binding_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub binding_count: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmNexusBindingSnapshotHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmNexusBindingRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmNexusBindingRecord {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// `active` 表示该条件在当前快照或计划中是否成立。
    pub active: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmNexusBindingRecord {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmRuntimeLogRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmRuntimeLogRequest {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// `level` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub level: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `message_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub message_len: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
    /// 结构化日志或诊断记录携带的消息字节。
    pub message: [u8; ELM_RUNTIME_LOG_MESSAGE_LEN],
}

impl ElmRuntimeLogRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmRuntimeLogResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmRuntimeLogResponse {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// `accepted_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub accepted_len: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `submitted_logs` 保存所属对象声明或快照中的有序记录集合。
    pub submitted_logs: u64,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u64,
}

impl ElmRuntimeLogResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmRuntimeEventRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmRuntimeEventRequest {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 分页或事件读取游标；其语义由对应请求类型定义。
    pub cursor: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmRuntimeEventRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmRuntimeEventResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmRuntimeEventResponse {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 分页或事件读取游标；其语义由对应请求类型定义。
    pub cursor: u64,
    /// 下一页或下一批记录的游标。
    pub next_cursor: u64,
    /// `dropped_events` 保存所属对象声明或快照中的有序记录集合。
    pub dropped_events: u64,
    /// `has_event` 表示该条件在当前快照或计划中是否成立。
    pub has_event: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `event` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub event: ElmEventRecord,
}

impl ElmRuntimeEventResponse {
    /// 构造不携带有效载荷的空值，供调用方继续填写必要字段。
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

    /// 设置 `event` 并返回更新后的值，便于构建器式初始化。
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
/// `ElmRuntimePortStatsHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmRuntimePortStatsHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmRuntimePortStatsHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmRuntimePortStatsRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmRuntimePortStatsRecord {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 分页或事件读取游标；其语义由对应请求类型定义。
    pub cursor: u64,
    /// `submitted_logs` 保存所属对象声明或快照中的有序记录集合。
    pub submitted_logs: u64,
    /// `delivered_events` 保存所属对象声明或快照中的有序记录集合。
    pub delivered_events: u64,
    /// `dropped_events` 保存所属对象声明或快照中的有序记录集合。
    pub dropped_events: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmRuntimePortStatsRecord {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderPortRegisterRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProviderPortRegisterRequest {
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `access_policy` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub access_policy: u32,
    /// 端口的数据流方向编码。
    pub direction: u32,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: u32,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmProviderPortRegisterRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderPortRegisterResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmProviderPortRegisterResponse {
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `access_policy` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub access_policy: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u64,
}

impl ElmProviderPortRegisterResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderPortUnregisterRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProviderPortUnregisterRequest {
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmProviderPortUnregisterRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderInvokeRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProviderInvokeRequest {
    /// 本次调用、补缀或故障记录关联的固定调用帧。
    pub frame: ElmCallFrame,
}

impl ElmProviderInvokeRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(frame: ElmCallFrame) -> Self {
        Self { frame }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmProviderInvokeResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmProviderInvokeResponse {
    /// 本次调用或 extension 分发产生的固定回复。
    pub reply: ElmReplyFrame,
}

impl ElmProviderInvokeResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(reply: ElmReplyFrame) -> Self {
        Self { reply }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmProviderSnapshotRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProviderSnapshotRequest {
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmProviderSnapshotRequest {
    /// 执行 `by_port` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn by_port(port_id: u64) -> Self {
        Self {
            port_id,
            binding_id: 0,
            flags: 0,
            reserved: 0,
        }
    }

    /// 执行 `by_port_paged` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn by_port_paged(port_id: u64, cursor: u32) -> Self {
        Self {
            port_id,
            binding_id: 0,
            flags: ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED,
            reserved: cursor,
        }
    }

    /// 执行 `by_binding` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn by_binding(binding_id: u64) -> Self {
        Self {
            port_id: 0,
            binding_id,
            flags: 0,
            reserved: 0,
        }
    }

    /// 执行 `by_binding_paged` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn by_binding_paged(binding_id: u64, cursor: u32) -> Self {
        Self {
            port_id: 0,
            binding_id,
            flags: ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED,
            reserved: cursor,
        }
    }

    /// 执行 `is_paged` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn is_paged(&self) -> bool {
        self.flags & ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED != 0
    }

    /// 执行 `cursor` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn cursor(&self) -> u32 {
        self.reserved
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmProviderSnapshotHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmProviderSnapshotHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 头部占用的字节数；载荷或记录区从该偏移之后开始。
    pub header_size: u16,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u32,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmProviderSnapshotHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `page` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_page(mut self, flags: u32, next_cursor: u32) -> Self {
        self.flags = flags;
        self.reserved = next_cursor;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmProviderAsyncSubmitRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProviderAsyncSubmitRequest {
    /// 本次调用、补缀或故障记录关联的固定调用帧。
    pub frame: ElmCallFrame,
    /// 该操作允许等待的最长时间，单位为毫秒。
    pub timeout_ms: u32,
    /// `result_ttl_ms` 使用毫秒单位，并受运行时规定的最大值限制。
    pub result_ttl_ms: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmProviderAsyncSubmitRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderAsyncSubmitResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmProviderAsyncSubmitResponse {
    /// `ticket_id` 所指对象的稳定运行时标识符。
    pub ticket_id: u64,
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 一次调用的关联标识符，回复必须原样返回。
    pub call_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// `queue_depth` 是当前层级或队列深度；消费者必须结合对应上限判断资源压力。
    pub queue_depth: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
}

impl ElmProviderAsyncSubmitResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderAsyncPollRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProviderAsyncPollRequest {
    /// `ticket_id` 所指对象的稳定运行时标识符。
    pub ticket_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmProviderAsyncPollRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderAsyncPollResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmProviderAsyncPollResponse {
    /// `ticket_id` 所指对象的稳定运行时标识符。
    pub ticket_id: u64,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 本次调用或 extension 分发产生的固定回复。
    pub reply: ElmReplyFrame,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
    /// `expires_at_ns` 使用纳秒单位；具体时钟域由所属记录定义。
    pub expires_at_ns: u64,
}

impl ElmProviderAsyncPollResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderAsyncCancelRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProviderAsyncCancelRequest {
    /// `ticket_id` 所指对象的稳定运行时标识符。
    pub ticket_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmProviderAsyncCancelRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderAsyncCancelResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmProviderAsyncCancelResponse {
    /// `ticket_id` 所指对象的稳定运行时标识符。
    pub ticket_id: u64,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
}

impl ElmProviderAsyncCancelResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderPortStatsHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmProviderPortStatsHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmProviderPortStatsHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(record_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmProviderPortRecord>() as u16,
            record_count,
            event_sequence,
        }
    }

    /// 执行 `new_stats` 定义的模型或协议操作；返回值反映校验后的结果。
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
/// `ElmProviderPortRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmProviderPortRecord {
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// `access_policy` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub access_policy: u32,
    /// 端口的数据流方向编码。
    pub direction: u32,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: u32,
    /// `implemented` 表示该条件在当前快照或计划中是否成立。
    pub implemented: u32,
    /// `invokable` 表示该条件在当前快照或计划中是否成立。
    pub invokable: u32,
    /// `binding_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub binding_count: u32,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// `calls` 是对应对象、调用或引用的数量。
    pub calls: u64,
    /// `failed_calls` 保存所属对象声明或快照中的有序记录集合。
    pub failed_calls: u64,
    /// `revokes` 保存所属对象声明或快照中的有序记录集合。
    pub revokes: u64,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_NEXUS_CONTRACT_LEN],
}

impl ElmProviderPortRecord {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderPortStatsRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmProviderPortStatsRecord {
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// `binding_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub binding_count: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `calls` 是对应对象、调用或引用的数量。
    pub calls: u64,
    /// `failed_calls` 保存所属对象声明或快照中的有序记录集合。
    pub failed_calls: u64,
    /// `revokes` 保存所属对象声明或快照中的有序记录集合。
    pub revokes: u64,
}

impl ElmProviderPortStatsRecord {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderQueueStatsHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmProviderQueueStatsHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmProviderQueueStatsHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmProviderQueueStatsRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmProviderQueueStatsRecord {
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// `queued` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub queued: u32,
    /// `running` 表示该条件在当前快照或计划中是否成立。
    pub running: u32,
    /// `retained` 表示该条件在当前快照或计划中是否成立。
    pub retained: u32,
    /// `queue_limit` 是对应缓冲区、队列或记录集合的最大容量。
    pub queue_limit: u32,
    /// `max_in_flight` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_in_flight: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// `submitted` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub submitted: u64,
    /// `completed` 表示该条件在当前快照或计划中是否成立。
    pub completed: u64,
    /// `canceled` 表示该条件在当前快照或计划中是否成立。
    pub canceled: u64,
    /// `expired` 表示该条件在当前快照或计划中是否成立。
    pub expired: u64,
    /// `rejected` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub rejected: u64,
}

impl ElmProviderQueueStatsRecord {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmCoreHealthHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmCoreHealthHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmCoreHealthHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmCoreHealthRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmCoreHealthRecord {
    /// `check_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub check_kind: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `subject_id` 所指对象的稳定运行时标识符。
    pub subject_id: u64,
    /// 供诊断使用的细化原因码。
    pub detail: u64,
}

impl ElmCoreHealthRecord {
    /// 构造状态为成功且不携带载荷的回复。
    pub const fn ok(check_kind: u32) -> Self {
        Self {
            check_kind,
            status: ELM_MGR_STATUS_OK,
            subject_id: 0,
            detail: ELM_HEALTH_DETAIL_NONE,
        }
    }

    /// 执行 `invalid` 定义的模型或协议操作；返回值反映校验后的结果。
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
/// `ElmMgrResponseHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMgrResponseHeader {
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u64,
}

impl ElmMgrResponseHeader {
    /// 构造不携带载荷的失败回复。
    pub const fn error(status: i32) -> Self {
        Self {
            status,
            payload_len: 0,
            reserved: 0,
        }
    }

    /// 构造状态为成功且不携带载荷的回复。
    pub const fn ok(payload_len: u32) -> Self {
        Self {
            status: ELM_MGR_STATUS_OK,
            payload_len,
            reserved: 0,
        }
    }

    /// 执行 `invalid` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn invalid() -> Self {
        Self {
            status: ELM_MGR_STATUS_INVALID,
            payload_len: 0,
            reserved: 0,
        }
    }

    /// 执行 `not_found` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn not_found() -> Self {
        Self {
            status: ELM_MGR_STATUS_NOT_FOUND,
            payload_len: 0,
            reserved: 0,
        }
    }

    /// 执行 `busy` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn busy() -> Self {
        Self {
            status: ELM_MGR_STATUS_BUSY,
            payload_len: 0,
            reserved: 0,
        }
    }

    /// 执行 `permission` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn permission() -> Self {
        Self {
            status: ELM_MGR_STATUS_PERMISSION,
            payload_len: 0,
            reserved: 0,
        }
    }

    /// 执行 `todo` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn todo() -> Self {
        Self {
            status: ELM_MGR_STATUS_TODO,
            payload_len: 0,
            reserved: 0,
        }
    }

    /// 执行 `unsupported` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn unsupported() -> Self {
        Self {
            status: ELM_MGR_STATUS_UNSUPPORTED,
            payload_len: 0,
            reserved: 0,
        }
    }
}

/// 执行 `status_from_blockers` 定义的模型或协议操作；返回值反映校验后的结果。
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

/// 执行 `first_lifecycle_reason` 定义的模型或协议操作；返回值反映校验后的结果。
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

/// 执行 `planned_final_state` 定义的模型或协议操作；返回值反映校验后的结果。
pub const fn planned_final_state(action: ElmLifecycleAction, current: ElmState) -> u32 {
    match action {
        ElmLifecycleAction::Pause => state_code(ElmState::Paused),
        ElmLifecycleAction::Resume => state_code(ElmState::Active),
        ElmLifecycleAction::Detach => state_code(ElmState::Retired),
        ElmLifecycleAction::Replace => state_code(current),
    }
}

const fn state_code(state: ElmState) -> u32 {
    match state {
        ElmState::Discovered => 1,
        ElmState::Verified => 2,
        ElmState::Loaded => 3,
        ElmState::Linked => 4,
        ElmState::Ready => 5,
        ElmState::Active => 6,
        ElmState::Quiescing => 7,
        ElmState::Paused => 8,
        ElmState::Detached => 9,
        ElmState::Retired => 10,
        ElmState::Faulted => 11,
        ElmState::Quarantined => 12,
    }
}

fn copy_str(src: &str, dst: &mut [u8]) -> usize {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    n
}
