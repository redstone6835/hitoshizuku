//! 内建织网端口描述。

use crate::ids::{ElmId, PortId};
use crate::nexus::{FlowDirection, FlowMode};

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
    pub implemented: bool,
}

pub const fn builtin_port_descriptors() -> [PortDescriptor; 15] {
    [
        desc(1, "core.log@1", FlowDirection::Sink, FlowMode::Shared, true),
        desc(
            2,
            "core.event@1",
            FlowDirection::Source,
            FlowMode::Broadcast,
            true,
        ),
        desc(
            3,
            "mgr.menu.item@1",
            FlowDirection::Sink,
            FlowMode::Ordered,
            true,
        ),
        desc(
            4,
            "mgr.action.invoke@1",
            FlowDirection::Control,
            FlowMode::Shared,
            false,
        ),
        desc(
            5,
            "device.discovered@1",
            FlowDirection::Source,
            FlowMode::Broadcast,
            false,
        ),
        desc(
            6,
            "device.claim@1",
            FlowDirection::Control,
            FlowMode::Exclusive,
            false,
        ),
        desc(
            7,
            "irq.event@1",
            FlowDirection::Source,
            FlowMode::Shared,
            false,
        ),
        desc(
            8,
            "dma.buffer@1",
            FlowDirection::Duplex,
            FlowMode::Shared,
            false,
        ),
        desc(
            9,
            "mmio.window@1",
            FlowDirection::Duplex,
            FlowMode::Shared,
            false,
        ),
        desc(
            10,
            "io.block.submit@1",
            FlowDirection::Sink,
            FlowMode::Shared,
            false,
        ),
        desc(
            11,
            "io.packet.rx@1",
            FlowDirection::Source,
            FlowMode::Pipeline,
            false,
        ),
        desc(
            12,
            "io.packet.tx@1",
            FlowDirection::Sink,
            FlowMode::Pipeline,
            false,
        ),
        desc(
            13,
            "vfs.lookup@1",
            FlowDirection::Control,
            FlowMode::Shared,
            false,
        ),
        desc(
            14,
            "vfs.read@1",
            FlowDirection::Control,
            FlowMode::Shared,
            false,
        ),
        desc(
            15,
            "vfs.write@1",
            FlowDirection::Control,
            FlowMode::Shared,
            false,
        ),
    ]
}

const fn desc(
    id: u64,
    contract: &'static str,
    direction: FlowDirection,
    mode: FlowMode,
    implemented: bool,
) -> PortDescriptor {
    PortDescriptor {
        id: PortId(id),
        owner: None,
        contract,
        direction,
        mode,
        implemented,
    }
}
