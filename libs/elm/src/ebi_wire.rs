//! EBI 来源管理所需的固定布局线类型。

use crate::ids::{ELM_MGR_BUILTIN_ID, ElmId};
use crate::resource::ElmResourceBudget;

pub const ELM_EBI_SOURCE_ABI_VERSION: u16 = 1;
pub const ELM_EBI_SOURCE_REQUEST_SIZE: usize = core::mem::size_of::<ElmEbiSourceRequest>();
pub const ELM_EBI_SOURCE_FLAG_NONE: u32 = 0;
pub const ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT: u32 = 1 << 0;
pub const ELM_EBI_SOURCE_FLAGS_MASK: u32 = ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT;
pub const ELM_EBI_PROJECTION_SOURCE_ABI_VERSION: u16 = 1;
pub const ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE: usize =
    core::mem::size_of::<ElmProjectionSourceRequest>();
pub const ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION: u16 = 1 << 0;
pub const ELM_EBI_PROJECTION_SOURCE_FLAGS_MASK: u16 = ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION;
pub const ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ElmEbiSourceKind {
    Projection = 2,
    Builtin = 3,
    Memory = 4,
}

impl ElmEbiSourceKind {
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            2 => Some(Self::Projection),
            3 => Some(Self::Builtin),
            4 => Some(Self::Memory),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEbiSourceRequest {
    pub abi_version: u16,
    pub source_kind: u16,
    pub flags: u32,
    pub parent_cell_id: u64,
    pub budget: ElmResourceBudget,
    pub reserved0: u16,
    pub reserved1: u16,
    pub payload_len: u32,
    pub reserved2: u32,
    pub reserved3: u32,
}

impl ElmEbiSourceRequest {
    pub const fn new(kind: ElmEbiSourceKind, payload_len: u32) -> Self {
        Self::new_under_parent(
            kind,
            ELM_MGR_BUILTIN_ID,
            ElmResourceBudget::DEFAULT,
            payload_len,
        )
    }

    pub const fn new_under_parent(
        kind: ElmEbiSourceKind,
        parent: ElmId,
        budget: ElmResourceBudget,
        payload_len: u32,
    ) -> Self {
        Self {
            abi_version: ELM_EBI_SOURCE_ABI_VERSION,
            source_kind: kind as u16,
            flags: ELM_EBI_SOURCE_FLAG_NONE,
            parent_cell_id: parent.0,
            budget,
            reserved0: 0,
            reserved1: 0,
            payload_len,
            reserved2: 0,
            reserved3: 0,
        }
    }

    pub const fn with_management_grant(mut self) -> Self {
        self.flags |= ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT;
        self
    }

    pub const fn grants_management(self) -> bool {
        self.flags & ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT != 0
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmProjectionSourceRequest {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub provider_id: u64,
    pub payload_len: u32,
    pub reserved1: u32,
}

impl ElmProjectionSourceRequest {
    pub const fn new(provider_id: u64, payload_len: u32) -> Self {
        Self {
            abi_version: ELM_EBI_PROJECTION_SOURCE_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            provider_id,
            payload_len,
            reserved1: 0,
        }
    }

    pub const fn from_image_session(provider_id: u64) -> Self {
        Self {
            abi_version: ELM_EBI_PROJECTION_SOURCE_ABI_VERSION,
            flags: ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION,
            reserved0: 0,
            provider_id,
            payload_len: core::mem::size_of::<ElmImageSessionReferenceV1>() as u32,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmImageSessionReferenceV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved: u32,
    pub session_id: u64,
}

impl ElmImageSessionReferenceV1 {
    pub const fn new(session_id: u64) -> Self {
        Self {
            abi_version: ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION,
            flags: 0,
            reserved: 0,
            session_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ElmEbiLoadStatus {
    Ok = 0,
    InvalidUnit = -1,
    UnsupportedAbi = -2,
    InvalidTarget = -3,
    InvalidSegment = -4,
    ArchMismatch = -5,
    InvalidManifest = -6,
    InvalidMenu = -7,
    NativeCodeTodo = -4096,
    RuntimeRejected = -4097,
    UntrustedImage = -4098,
    AbiFingerprintRejected = -4099,
    RollbackRejected = -4100,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmLoadCellResponse {
    pub cell_id: u64,
    pub status: i32,
    pub final_state: u32,
    pub reason: u32,
    pub reserved: u32,
}

impl ElmLoadCellResponse {
    pub const fn new(
        status: ElmEbiLoadStatus,
        cell_id: u64,
        final_state: u32,
        reason: u32,
    ) -> Self {
        Self {
            cell_id,
            status: status as i32,
            final_state,
            reason,
            reserved: 0,
        }
    }

    pub const fn failed(status: ElmEbiLoadStatus) -> Self {
        Self::new(status, 0, 0, 0)
    }
}
