//! 固定布局的运行拓扑快照。

use crate::ebi::{ElmEbiArch, ElmEbiLoadStatus};
use crate::ids::{ElmId, Generation, PortId};
use crate::manifest::ElmKind;
use crate::nexus::{FlowDirection, FlowMode};
use crate::state::ElmState;

pub const ELM_CELL_NAME_LEN: usize = 64;
pub const ELM_CONTRACT_NAME_LEN: usize = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmSnapshotHeader {
    pub abi_version: u16,
    pub cell_entry_size: u16,
    pub port_entry_size: u16,
    pub reserved: u16,
    pub cell_count: u32,
    pub port_count: u32,
    pub lease_count: u32,
    pub event_sequence: u64,
}

impl ElmSnapshotHeader {
    pub const fn new(
        cell_count: u32,
        port_count: u32,
        lease_count: u32,
        event_sequence: u64,
    ) -> Self {
        Self {
            abi_version: crate::ctl::ELM_CTL_ABI_VERSION,
            cell_entry_size: core::mem::size_of::<ElmCellSnapshot>() as u16,
            port_entry_size: core::mem::size_of::<ElmPortSnapshot>() as u16,
            reserved: 0,
            cell_count,
            port_count,
            lease_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCellSnapshot {
    pub id: u64,
    pub parent: u64,
    pub state: u32,
    pub kind: u32,
    pub ebi_arch: u32,
    pub ebi_status: i32,
    pub native_code: u32,
    pub reserved0: u32,
    pub generation: u64,
    pub name_len: u16,
    pub reserved: u16,
    pub name: [u8; ELM_CELL_NAME_LEN],
}

impl ElmCellSnapshot {
    pub fn new(
        id: ElmId,
        parent: Option<ElmId>,
        state: ElmState,
        kind: ElmKind,
        generation: Generation,
        name: &str,
        ebi_arch: ElmEbiArch,
        ebi_status: ElmEbiLoadStatus,
        native_code: bool,
    ) -> Self {
        let mut out = Self {
            id: id.0,
            parent: parent.map(|id| id.0).unwrap_or(0),
            state: state_code(state),
            kind: kind_code(kind),
            ebi_arch: ebi_arch as u32,
            ebi_status: ebi_status as i32,
            native_code: u32::from(native_code),
            reserved0: 0,
            generation: generation.0,
            name_len: 0,
            reserved: 0,
            name: [0; ELM_CELL_NAME_LEN],
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(ELM_CELL_NAME_LEN);
        out.name[..n].copy_from_slice(&bytes[..n]);
        out.name_len = n as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmPortSnapshot {
    pub id: u64,
    pub owner: u64,
    pub direction: u32,
    pub mode: u32,
    pub implemented: u32,
    pub contract_len: u16,
    pub reserved: u16,
    pub contract: [u8; ELM_CONTRACT_NAME_LEN],
}

impl ElmPortSnapshot {
    pub fn new(
        id: PortId,
        owner: Option<ElmId>,
        contract: &str,
        direction: FlowDirection,
        mode: FlowMode,
        implemented: bool,
    ) -> Self {
        let mut out = Self {
            id: id.0,
            owner: owner.map(|id| id.0).unwrap_or(0),
            direction: direction_code(direction),
            mode: mode_code(mode),
            implemented: u32::from(implemented),
            contract_len: 0,
            reserved: 0,
            contract: [0; ELM_CONTRACT_NAME_LEN],
        };
        let bytes = contract.as_bytes();
        let n = bytes.len().min(ELM_CONTRACT_NAME_LEN);
        out.contract[..n].copy_from_slice(&bytes[..n]);
        out.contract_len = n as u16;
        out
    }
}

pub const fn state_code(state: ElmState) -> u32 {
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

pub const fn kind_code(kind: ElmKind) -> u32 {
    match kind {
        ElmKind::Manager => 1,
        ElmKind::Service => 2,
        ElmKind::Driver => 3,
        ElmKind::Extension => 4,
        ElmKind::Filesystem => 5,
        ElmKind::Network => 6,
        ElmKind::Debug => 7,
        ElmKind::Other => 255,
    }
}

pub const fn direction_code(direction: FlowDirection) -> u32 {
    match direction {
        FlowDirection::Source => 1,
        FlowDirection::Sink => 2,
        FlowDirection::Duplex => 3,
        FlowDirection::Control => 4,
    }
}

pub const fn mode_code(mode: FlowMode) -> u32 {
    match mode {
        FlowMode::Exclusive => 1,
        FlowMode::Shared => 2,
        FlowMode::Ordered => 3,
        FlowMode::Pipeline => 4,
        FlowMode::Broadcast => 5,
    }
}
