//! 单元管理器调用外壳。

use crate::ctl::ELM_CTL_ABI_VERSION;
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

pub const ELM_MGR_ACTION_PAUSE: u32 = 1 << 0;
pub const ELM_MGR_ACTION_RESUME: u32 = 1 << 1;
pub const ELM_MGR_ACTION_DETACH: u32 = 1 << 2;
pub const ELM_MGR_ACTION_REPLACE: u32 = 1 << 3;
pub const ELM_MGR_ACTION_BIND: u32 = 1 << 4;
pub const ELM_MGR_ACTION_UNBIND: u32 = 1 << 5;

pub const ELM_MGR_POLICY_PREFLIGHT: u64 = 1 << 0;
pub const ELM_MGR_POLICY_AUDIT: u64 = 1 << 1;
pub const ELM_MGR_POLICY_LOAD_REQUIRES_SOYO: u64 = 1 << 2;
pub const ELM_MGR_POLICY_REPLACE_TODO: u64 = 1 << 3;
pub const ELM_MGR_POLICY_NATIVE_LIFECYCLE_TODO: u64 = 1 << 4;
pub const ELM_MGR_POLICY_NEXUS_BINDING: u64 = 1 << 5;
pub const ELM_MGR_POLICY_MENU_BINDING: u64 = 1 << 6;

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
pub const ELM_POLICY_BLOCK_LOAD_REQUIRES_SOYO: u64 = 1 << 10;
pub const ELM_POLICY_BLOCK_PORT_NOT_FOUND: u64 = 1 << 11;
pub const ELM_POLICY_BLOCK_CONTRACT_MISMATCH: u64 = 1 << 12;
pub const ELM_POLICY_BLOCK_DUPLICATE_BINDING: u64 = 1 << 13;
pub const ELM_POLICY_BLOCK_PORT_TODO: u64 = 1 << 14;
pub const ELM_POLICY_BLOCK_BINDING_NOT_FOUND: u64 = 1 << 15;
pub const ELM_POLICY_BLOCK_BINDING_PROTECTED: u64 = 1 << 16;

pub const ELM_MGR_RELATION_CONTRACT_LEN: usize = 64;
pub const ELM_MGR_RELATION_POINT_LEN: usize = 32;
pub const ELM_NEXUS_CONTRACT_LEN: usize = 64;

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
                | ELM_MGR_ACTION_UNBIND,
            policy_flags: ELM_MGR_POLICY_PREFLIGHT
                | ELM_MGR_POLICY_AUDIT
                | ELM_MGR_POLICY_LOAD_REQUIRES_SOYO
                | ELM_MGR_POLICY_REPLACE_TODO
                | ELM_MGR_POLICY_NATIVE_LIFECYCLE_TODO
                | ELM_MGR_POLICY_NEXUS_BINDING
                | ELM_MGR_POLICY_MENU_BINDING,
            blocker_mask: ELM_POLICY_BLOCK_BUILTIN_PROTECTED
                | ELM_POLICY_BLOCK_CELL_NOT_FOUND
                | ELM_POLICY_BLOCK_INVALID_STATE
                | ELM_POLICY_BLOCK_NATIVE_TODO
                | ELM_POLICY_BLOCK_HAS_CHILDREN
                | ELM_POLICY_BLOCK_HAS_DEPENDENTS
                | ELM_POLICY_BLOCK_HAS_EXTENSIONS
                | ELM_POLICY_BLOCK_LEASE_BUSY
                | ELM_POLICY_BLOCK_REPLACE_TODO
                | ELM_POLICY_BLOCK_GRAPH_INCONSISTENT
                | ELM_POLICY_BLOCK_LOAD_REQUIRES_SOYO
                | ELM_POLICY_BLOCK_PORT_NOT_FOUND
                | ELM_POLICY_BLOCK_CONTRACT_MISMATCH
                | ELM_POLICY_BLOCK_DUPLICATE_BINDING
                | ELM_POLICY_BLOCK_PORT_TODO
                | ELM_POLICY_BLOCK_BINDING_NOT_FOUND
                | ELM_POLICY_BLOCK_BINDING_PROTECTED,
            audit_capacity,
            reserved1: 0,
        }
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
    } else if blockers & (ELM_POLICY_BLOCK_PORT_NOT_FOUND | ELM_POLICY_BLOCK_BINDING_NOT_FOUND) != 0
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
            | ELM_POLICY_BLOCK_DUPLICATE_BINDING)
        != 0
    {
        ELM_MGR_STATUS_BUSY
    } else if blockers
        & (ELM_POLICY_BLOCK_NATIVE_TODO
            | ELM_POLICY_BLOCK_REPLACE_TODO
            | ELM_POLICY_BLOCK_LOAD_REQUIRES_SOYO
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
