//! ELM 私有控制面协议。

use crate::error::ElmError;

pub const ELM_CTL_MAGIC: u32 = 0x314d_4c45;
pub const ELM_CTL_ABI_VERSION: u16 = 1;

pub const ELM_CORE_CAP_SNAPSHOT: u64 = 1 << 0;
pub const ELM_CORE_CAP_EVENTS: u64 = 1 << 1;
pub const ELM_CORE_CAP_MGR_CHANNEL: u64 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmCtlCommand {
    CoreQuery = 1,
    MgrCall = 2,
    EventRead = 3,
    EventAck = 4,
    SnapshotRead = 5,
    DebugDump = 6,
}

impl ElmCtlCommand {
    pub const fn from_raw(raw: usize) -> Option<Self> {
        match raw {
            1 => Some(Self::CoreQuery),
            2 => Some(Self::MgrCall),
            3 => Some(Self::EventRead),
            4 => Some(Self::EventAck),
            5 => Some(Self::SnapshotRead),
            6 => Some(Self::DebugDump),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ElmCtlStatus {
    Ok = 0,
    Permission = -1,
    NotFound = -2,
    Invalid = -22,
    Busy = -16,
    NoMemory = -12,
    MessageTooLarge = -90,
    Unsupported = -95,
}

impl ElmCtlStatus {
    pub const fn from_error(error: &ElmError) -> Self {
        match error {
            ElmError::CellNotFound | ElmError::PortNotFound | ElmError::ExtensionPointNotFound => {
                Self::NotFound
            }
            ElmError::LeaseBusy => Self::Busy,
            ElmError::PermissionDenied => Self::Permission,
            _ => Self::Invalid,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCtlHeader {
    pub magic: u32,
    pub abi_version: u16,
    pub command: u16,
    pub flags: u32,
    pub input_len: u32,
    pub output_len: u32,
    pub sequence: u64,
}

impl ElmCtlHeader {
    pub const fn new(command: ElmCtlCommand, input_len: u32, output_len: u32) -> Self {
        Self {
            magic: ELM_CTL_MAGIC,
            abi_version: ELM_CTL_ABI_VERSION,
            command: command as u16,
            flags: 0,
            input_len,
            output_len,
            sequence: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCoreInfo {
    pub magic: u32,
    pub abi_version: u16,
    pub core_version: u16,
    pub capabilities: u64,
    pub cell_count: u32,
    pub port_count: u32,
    pub lease_count: u32,
    pub event_sequence: u64,
}

impl ElmCoreInfo {
    pub const fn new(
        cell_count: u32,
        port_count: u32,
        lease_count: u32,
        event_sequence: u64,
    ) -> Self {
        Self {
            magic: ELM_CTL_MAGIC,
            abi_version: ELM_CTL_ABI_VERSION,
            core_version: 1,
            capabilities: ELM_CORE_CAP_SNAPSHOT | ELM_CORE_CAP_EVENTS | ELM_CORE_CAP_MGR_CHANNEL,
            cell_count,
            port_count,
            lease_count,
            event_sequence,
        }
    }
}
