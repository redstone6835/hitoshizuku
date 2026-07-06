#![no_std]

//! ELM（可拓展内核单元）纯模型层。
//!
//! 本库只描述架构无关、内核无关的模型：单元清单、状态机、能力织网、
//! 绑定图、拓展点和资源租约。它不能依赖 `kernel`、`general` 或 `arch`。

extern crate alloc;

pub mod ctl;
pub mod ebi;
pub mod error;
pub mod event;
pub mod graph;
pub mod ids;
pub mod lease;
pub mod manifest;
pub mod menu;
pub mod mgr;
pub mod nexus;
pub mod ports;
pub mod snapshot;
pub mod state;
pub mod topology;

pub use ctl::{
    ELM_CORE_CAP_EVENTS, ELM_CORE_CAP_MGR_CHANNEL, ELM_CORE_CAP_SNAPSHOT, ELM_CTL_ABI_VERSION,
    ELM_CTL_MAGIC, ElmCoreInfo, ElmCtlCommand, ElmCtlHeader, ElmCtlStatus,
};
pub use ebi::{
    ELM_EBI_ABI_VERSION, ELM_EBI_MAX_SEGMENTS, ElmEbiArch, ElmEbiEntry, ElmEbiLoadStatus,
    ElmEbiMenuDecl, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiTarget, ElmEbiUnit,
    ElmLoadCellResponse,
};
pub use error::{ElmError, ElmResult};
pub use event::{ElmEventRecord, ElmEventSequence};
pub use graph::{
    BindingGraph, DependencyEdge, ExtensionEdge, ExtensionPoint, GraphRemovalReport,
    GraphValidationReport, ParentEdge,
};
pub use ids::{ActionId, BindingId, ElmId, Generation, LeaseId, PortId};
pub use lease::{LeaseKind, LeaseRegistry, LeaseRights, LeaseState, ResourceLease};
pub use manifest::{ElmKind, ElmManifest, ElmName, ElmVersion};
pub use menu::{
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_FLAG_DISABLED, ELM_MENU_FLAG_REQUIRES_SYS_ADMIN,
    ELM_MENU_FLAG_TODO, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ElmMenuItemKind,
    ElmMenuItemSnapshot, ElmMenuSnapshotHeader,
};
pub use mgr::{
    ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED, ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT, ELM_LIFECYCLE_REASON_HAS_CHILDREN,
    ELM_LIFECYCLE_REASON_HAS_DEPENDENTS, ELM_LIFECYCLE_REASON_HAS_EXTENSIONS,
    ELM_LIFECYCLE_REASON_INVALID_STATE, ELM_LIFECYCLE_REASON_LEASE_BUSY,
    ELM_LIFECYCLE_REASON_NATIVE_TODO, ELM_LIFECYCLE_REASON_NONE, ELM_MGR_ACTION_DETACH,
    ELM_MGR_ACTION_PAUSE, ELM_MGR_ACTION_REPLACE, ELM_MGR_ACTION_RESUME, ELM_MGR_POLICY_AUDIT,
    ELM_MGR_POLICY_LOAD_REQUIRES_SOYO, ELM_MGR_POLICY_NATIVE_LIFECYCLE_TODO,
    ELM_MGR_POLICY_PREFLIGHT, ELM_MGR_POLICY_REPLACE_TODO, ELM_MGR_RELATION_CONTRACT_LEN,
    ELM_MGR_RELATION_POINT_LEN, ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID,
    ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK, ELM_MGR_STATUS_PERMISSION, ELM_MGR_STATUS_TODO,
    ELM_MGR_STATUS_UNSUPPORTED, ELM_POLICY_BLOCK_BUILTIN_PROTECTED,
    ELM_POLICY_BLOCK_CELL_NOT_FOUND, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
    ELM_POLICY_BLOCK_HAS_CHILDREN, ELM_POLICY_BLOCK_HAS_DEPENDENTS,
    ELM_POLICY_BLOCK_HAS_EXTENSIONS, ELM_POLICY_BLOCK_INVALID_STATE, ELM_POLICY_BLOCK_LEASE_BUSY,
    ELM_POLICY_BLOCK_LOAD_REQUIRES_SOYO, ELM_POLICY_BLOCK_NATIVE_TODO,
    ELM_POLICY_BLOCK_REPLACE_TODO, ElmLifecycleAction, ElmLifecyclePlanRequest,
    ElmLifecyclePlanResponse, ElmLifecycleRequest, ElmLifecycleResponse, ElmMgrAuditHeader,
    ElmMgrAuditRecord, ElmMgrCallHeader, ElmMgrCallKind, ElmMgrPolicyInfo, ElmMgrRelationKind,
    ElmMgrRelationRecord, ElmMgrResponseHeader, ElmMgrTopologyHeader, first_lifecycle_reason,
    planned_final_state, status_from_blockers,
};
pub use nexus::{
    FlowBackpressure, FlowConcurrency, FlowContract, FlowDirection, FlowMode, IntentKind,
    NexusIntent, NexusOffer,
};
pub use ports::{BuiltinPort, PortDescriptor, builtin_port_descriptors};
pub use snapshot::{
    ELM_CELL_NAME_LEN, ELM_CONTRACT_NAME_LEN, ElmCellSnapshot, ElmPortSnapshot, ElmSnapshotHeader,
    state_code,
};
pub use state::{ElmState, ElmTransition};
pub use topology::{TopologyEvent, TopologyEventKind, TopologySnapshot};

#[cfg(test)]
mod tests;
