//! 运行拓扑快照模型。

use alloc::vec::Vec;

use crate::ids::{BindingId, ElmId, LeaseId};
use crate::state::ElmState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyEventKind {
    CellAdded,
    CellStateChanged,
    BindingAdded,
    BindingRemoved,
    LeaseAdded,
    LeaseRevoked,
    PortAdded,
    MenuItemAdded,
    MenuItemRemoved,
    CellRemoved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopologyEvent {
    pub kind: TopologyEventKind,
    pub cell: Option<ElmId>,
    pub port: Option<crate::PortId>,
    pub binding: Option<BindingId>,
    pub lease: Option<LeaseId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TopologySnapshot {
    pub cells: Vec<(ElmId, ElmState)>,
    pub bindings: Vec<BindingId>,
    pub leases: Vec<LeaseId>,
}
