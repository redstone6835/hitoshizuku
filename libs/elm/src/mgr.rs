//! 单元管理器调用外壳。

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmMgrCallKind {
    QueryMenu = 1,
    LoadCell = 2,
    DetachCell = 3,
    PauseCell = 4,
    ResumeCell = 5,
    ReplaceCell = 6,
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
            _ => None,
        }
    }
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
