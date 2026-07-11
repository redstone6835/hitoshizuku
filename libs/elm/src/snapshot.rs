//! 固定布局的运行拓扑快照。

use crate::ebi::{ElmEbiArch, ElmEbiLoadStatus, ElmEbiSourceKind};
use crate::ids::{ElmId, Generation, PortId};
use crate::manifest::ElmKind;
use crate::nexus::{FlowDirection, FlowMode};
use crate::resource::{ElmResourceBudget, ElmResourceUsage};
use crate::state::ElmState;

pub const ELM_CELL_NAME_LEN: usize = 64;
pub const ELM_CONTRACT_NAME_LEN: usize = 64;
pub const ELM_CELL_LIFECYCLE_HOOKS_DECLARED: u32 = 1 << 0;
pub const ELM_CELL_LIFECYCLE_EXECUTOR_READY: u32 = 1 << 1;
pub const ELM_CELL_LIFECYCLE_INITIALIZED: u32 = 1 << 2;
pub const ELM_CELL_LIFECYCLE_FINALIZED: u32 = 1 << 3;
pub const ELM_CELL_TRUST_INTERNAL: u32 = 1 << 0;
pub const ELM_CELL_TRUST_SIGNED: u32 = 1 << 1;
pub const ELM_CELL_TRUST_UNSIGNED: u32 = 1 << 2;

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
    pub ebi_source: u32,
    pub lifecycle_flags: u32,
    pub native_segment_count: u16,
    pub native_import_count: u16,
    pub native_export_count: u16,
    pub native_faults: u16,
    pub isolated: u32,
    pub reserved1: u32,
    pub isolation_blocker: u64,
    pub budget_max_provider_ports: u16,
    pub budget_max_provider_queue: u16,
    pub budget_max_event_subscriptions: u16,
    pub budget_max_pending_loads: u16,
    pub budget_max_native_images: u16,
    pub budget_max_native_faults: u16,
    pub budget_max_audit_records: u16,
    pub usage_provider_ports: u16,
    pub usage_provider_queue: u16,
    pub usage_event_subscriptions: u16,
    pub usage_pending_loads: u16,
    pub usage_native_images: u16,
    pub usage_native_faults: u16,
    pub usage_audit_records: u16,
    pub trust_flags: u32,
    pub release_epoch: u64,
    pub signer_key_id: [u8; 32],
}

impl ElmCellSnapshot {
    #[allow(clippy::too_many_arguments)]
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
        ebi_source: ElmEbiSourceKind,
        native_segment_count: u16,
        native_import_count: u16,
        native_export_count: u16,
        lifecycle_hooks_declared: bool,
        lifecycle_executor_ready: bool,
        lifecycle_initialized: bool,
        lifecycle_finalized: bool,
        budget: ElmResourceBudget,
        usage: ElmResourceUsage,
        isolated: bool,
        native_faults: u16,
        isolation_blocker: u64,
        trust_unsigned: bool,
        signer_key_id: [u8; 32],
        release_epoch: u64,
    ) -> Self {
        let trust_flags = if trust_unsigned {
            ELM_CELL_TRUST_UNSIGNED
        } else if signer_key_id != [0; 32] {
            ELM_CELL_TRUST_SIGNED
        } else {
            ELM_CELL_TRUST_INTERNAL
        };
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
            ebi_source: ebi_source as u32,
            lifecycle_flags: lifecycle_flags(
                lifecycle_hooks_declared,
                lifecycle_executor_ready,
                lifecycle_initialized,
                lifecycle_finalized,
            ),
            native_segment_count,
            native_import_count,
            native_export_count,
            native_faults,
            isolated: u32::from(isolated),
            reserved1: 0,
            isolation_blocker,
            budget_max_provider_ports: budget.max_provider_ports,
            budget_max_provider_queue: budget.max_provider_queue,
            budget_max_event_subscriptions: budget.max_event_subscriptions,
            budget_max_pending_loads: budget.max_pending_loads,
            budget_max_native_images: budget.max_native_images,
            budget_max_native_faults: budget.max_native_faults,
            budget_max_audit_records: budget.max_audit_records,
            usage_provider_ports: usage.provider_ports,
            usage_provider_queue: usage.provider_queue,
            usage_event_subscriptions: usage.event_subscriptions,
            usage_pending_loads: usage.pending_loads,
            usage_native_images: usage.native_images,
            usage_native_faults: usage.native_faults,
            usage_audit_records: usage.audit_records,
            trust_flags,
            release_epoch,
            signer_key_id,
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(ELM_CELL_NAME_LEN);
        out.name[..n].copy_from_slice(&bytes[..n]);
        out.name_len = n as u16;
        out
    }
}

const fn lifecycle_flags(
    hooks_declared: bool,
    executor_ready: bool,
    initialized: bool,
    finalized: bool,
) -> u32 {
    (if hooks_declared {
        ELM_CELL_LIFECYCLE_HOOKS_DECLARED
    } else {
        0
    }) | (if executor_ready {
        ELM_CELL_LIFECYCLE_EXECUTOR_READY
    } else {
        0
    }) | (if initialized {
        ELM_CELL_LIFECYCLE_INITIALIZED
    } else {
        0
    }) | (if finalized {
        ELM_CELL_LIFECYCLE_FINALIZED
    } else {
        0
    })
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
