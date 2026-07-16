//! 网络栈的架构无关核心。
//!
//! 提供 buffer 所有权、批量 queue、协议数据面和设备注册边界。

#![no_std]

extern crate alloc;

pub mod address;
pub mod buf;
pub mod control;
pub mod device;
pub mod flow;
pub mod id;
pub mod pipeline;
pub mod queue;
pub mod ring;
pub mod socket;
pub mod transport;
pub mod tuning;

pub use address::{AddressFamily, Endpoint, IpAddr, Ipv4Addr, Ipv6Addr, TransportProtocol};
pub use flow::{FlowShard, FlowTurnContext, UdpSendError, UdpSendFailure};
pub use id::{FlowId, InterfaceId, NetDeviceId, QueuePairId, ShardId, SocketId};
pub use socket::{
    InstallSocketRuntimeError, OwnerRef, Readiness, ReadinessObserver, SocketCommand, SocketError,
    SocketFacade, SocketKind, SocketRuntime, TcpInfoSnapshot, TcpTxLease, UdpReceive, UdpTxLease,
    install_socket_runtime, new_socket_facade, new_tcp_socket_facade,
};
