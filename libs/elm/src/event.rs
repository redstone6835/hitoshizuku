//! ELM 事件记录模型。

use crate::ids::{BindingId, ElmId, LeaseId, PortId};
use crate::topology::TopologyEventKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ElmEventSequence(pub u64);

impl ElmEventSequence {
    pub const FIRST: Self = Self(1);

    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEventRecord {
    pub sequence: u64,
    pub kind: u32,
    pub cell: u64,
    pub port: u64,
    pub binding: u64,
    pub lease: u64,
}

impl ElmEventRecord {
    pub const fn new(
        sequence: ElmEventSequence,
        kind: TopologyEventKind,
        cell: Option<ElmId>,
        port: Option<PortId>,
        binding: Option<BindingId>,
        lease: Option<LeaseId>,
    ) -> Self {
        Self {
            sequence: sequence.0,
            kind: event_kind_code(kind),
            cell: match cell {
                Some(id) => id.0,
                None => 0,
            },
            port: match port {
                Some(id) => id.0,
                None => 0,
            },
            binding: match binding {
                Some(id) => id.0,
                None => 0,
            },
            lease: match lease {
                Some(id) => id.0,
                None => 0,
            },
        }
    }
}

pub const fn event_kind_code(kind: TopologyEventKind) -> u32 {
    match kind {
        TopologyEventKind::CellAdded => 1,
        TopologyEventKind::CellStateChanged => 2,
        TopologyEventKind::BindingAdded => 3,
        TopologyEventKind::BindingRemoved => 4,
        TopologyEventKind::LeaseAdded => 5,
        TopologyEventKind::LeaseRevoked => 6,
        TopologyEventKind::PortAdded => 7,
        TopologyEventKind::MenuItemAdded => 8,
        TopologyEventKind::MenuItemRemoved => 9,
        TopologyEventKind::CellRemoved => 10,
    }
}
