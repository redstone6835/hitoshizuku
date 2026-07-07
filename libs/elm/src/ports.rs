//! 内建织网端口描述。

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
    DeviceDiscovered,
    DeviceClaim,
    IrqEvent,
    DmaBuffer,
    MmioWindow,
    IoBlockSubmit,
    IoPacketRx,
    IoPacketTx,
    VfsLookup,
    VfsRead,
    VfsWrite,
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

pub const fn builtin_port_descriptors() -> [PortDescriptor; 15] {
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
        desc(
            5,
            "device.discovered@1",
            FlowDirection::Source,
            FlowMode::Broadcast,
            ElmPortAccessPolicy::Internal,
            false,
            false,
        ),
        desc(
            6,
            "device.claim@1",
            FlowDirection::Control,
            FlowMode::Exclusive,
            ElmPortAccessPolicy::Internal,
            true,
            false,
        ),
        desc(
            7,
            "irq.event@1",
            FlowDirection::Source,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            false,
            false,
        ),
        desc(
            8,
            "dma.buffer@1",
            FlowDirection::Duplex,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            false,
        ),
        desc(
            9,
            "mmio.window@1",
            FlowDirection::Duplex,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            false,
        ),
        desc(
            10,
            "io.block.submit@1",
            FlowDirection::Sink,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            false,
        ),
        desc(
            11,
            "io.packet.rx@1",
            FlowDirection::Source,
            FlowMode::Pipeline,
            ElmPortAccessPolicy::Internal,
            false,
            false,
        ),
        desc(
            12,
            "io.packet.tx@1",
            FlowDirection::Sink,
            FlowMode::Pipeline,
            ElmPortAccessPolicy::Internal,
            true,
            false,
        ),
        desc(
            13,
            "vfs.lookup@1",
            FlowDirection::Control,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            false,
        ),
        desc(
            14,
            "vfs.read@1",
            FlowDirection::Control,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            false,
        ),
        desc(
            15,
            "vfs.write@1",
            FlowDirection::Control,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            false,
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
