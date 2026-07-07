//! 内建枢纽端口描述。

use crate::ids::{ELM_MGR_BUILTIN_ID, ElmId, PortId};
use crate::nexus::{FlowDirection, FlowMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmPortAccessPolicy {
    Internal = 1,
    Public = 2,
    ExtensionOnly = 3,
}

impl ElmPortAccessPolicy {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Internal),
            2 => Some(Self::Public),
            3 => Some(Self::ExtensionOnly),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinPort {
    CoreLog,
    CoreEvent,
    MgrMenuItem,
    MgrActionInvoke,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortDescriptor {
    pub id: PortId,
    pub owner: Option<ElmId>,
    pub contract: &'static str,
    pub direction: FlowDirection,
    pub mode: FlowMode,
    pub access: ElmPortAccessPolicy,
    pub invokable: bool,
    pub implemented: bool,
}

pub const fn builtin_port_descriptors() -> [PortDescriptor; 4] {
    [
        desc(
            1,
            "core.log@1",
            FlowDirection::Sink,
            FlowMode::Shared,
            ElmPortAccessPolicy::Public,
            false,
            true,
        ),
        desc(
            2,
            "core.event@1",
            FlowDirection::Source,
            FlowMode::Broadcast,
            ElmPortAccessPolicy::Public,
            false,
            true,
        ),
        desc(
            3,
            "mgr.menu.item@1",
            FlowDirection::Sink,
            FlowMode::Ordered,
            ElmPortAccessPolicy::Public,
            false,
            true,
        ),
        desc_owned(
            4,
            ELM_MGR_BUILTIN_ID,
            "mgr.action.invoke@1",
            FlowDirection::Control,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            true,
        ),
    ]
}

const fn desc(
    id: u64,
    contract: &'static str,
    direction: FlowDirection,
    mode: FlowMode,
    access: ElmPortAccessPolicy,
    invokable: bool,
    implemented: bool,
) -> PortDescriptor {
    PortDescriptor {
        id: PortId(id),
        owner: None,
        contract,
        direction,
        mode,
        access,
        invokable,
        implemented,
    }
}

const fn desc_owned(
    id: u64,
    owner: ElmId,
    contract: &'static str,
    direction: FlowDirection,
    mode: FlowMode,
    access: ElmPortAccessPolicy,
    invokable: bool,
    implemented: bool,
) -> PortDescriptor {
    PortDescriptor {
        id: PortId(id),
        owner: Some(owner),
        contract,
        direction,
        mode,
        access,
        invokable,
        implemented,
    }
}
