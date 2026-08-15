//! 网络栈的架构无关核心。
//!
//! 提供 buffer 所有权、批量 queue、协议数据面和设备注册边界。

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod address;
pub mod bpf;
pub mod boot;
pub mod buf;
pub mod control;
pub mod device;
pub mod elm;
pub mod flow;
pub mod id;
pub mod pipeline;
pub mod queue;
pub mod ring;
pub mod runtime;
pub mod socket;
pub mod stack;
pub mod transport;
pub mod tuning;

pub use address::{AddressFamily, Endpoint, IpAddr, Ipv4Addr, Ipv6Addr, TransportProtocol};
pub use flow::{
    FlowExecLease, FlowExecution, FlowExecutionSnapshot, FlowExecutorKind, FlowShard,
    FlowTurnContext, UdpSendError, UdpSendFailure,
};
pub use id::{FlowId, InterfaceId, ListenGroupId, NetDeviceId, QueuePairId, ShardId, SocketId};
pub use socket::{
    DatagramCopyError, InetSocketSnapshot, InstallSocketRuntimeError, ListenGroup,
    MulticastMembership, NetSocketProxy, OwnerRef, Readiness, ReadinessObserver, SocketCommand,
    SocketError, SocketErrorOrigin, SocketErrorRecord, SocketFacade, SocketKind, SocketRuntime,
    SocketTxCause, TcpInfoSnapshot, TcpTxLease, UdpReceive, UdpTxLease, detach_proxy_stack,
    detach_socket_generation, install_socket_runtime, interface_by_name, new_raw_socket_facade,
    new_socket_facade, new_tcp_socket_facade, snapshot_inet_sockets, track_socket_facade,
};

/// 保留网络子系统的 ELM provider 规格代码生成单元。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    elm::providers as usize
}
