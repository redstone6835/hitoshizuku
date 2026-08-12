//! 传输协议状态机。

mod icmp;
mod raw;
mod tcp;
mod tcp_engine;
mod udp;

pub use icmp::{
    ControlErrorTarget, ControlPacketResult, TransportControlError, build_port_unreachable,
    handle_control_packet,
};
#[cfg(test)]
pub use raw::build_header_included_ipv4_fragments;
pub use raw::{
    PreparedRawTx, RawBindError, RawEndpointInfo, RawEndpointTable, RawIngressResult, RawTxError,
    build_raw_packet,
};
pub use tcp::{
    TCP_MAX_HEADER_LEN, TCP_MIN_HEADER_LEN, TCP_PROTOCOL_NUMBER, TcpFlags, TcpMachineOutput,
    TcpOptions, TcpPacket, TcpSackBlock, TcpSegment, TcpSequence, TcpState, TcpStateMachine,
    TcpTimestamp, TcpTransmit,
};
#[cfg(test)]
pub(crate) use tcp::{parse_tcp_packet, parse_tcp_packet_trusted};
pub use tcp_engine::{
    PreparedTcpTx, TcpBindError, TcpEndpointTable, TcpEngineStats, TcpIngressError, TcpPath,
    build_tcp_packet, build_tcp_reset,
};
#[cfg(test)]
pub use udp::build_udp_fragments;
pub(crate) use udp::local_udp_payload_fits_route;
pub use udp::{
    LocalUdpIngressError, PreparedUdpTx, UdpBindError, UdpDatagram, UdpEndpointInfo,
    UdpEndpointTable, UdpIngressError, UdpTxError, build_udp_packet, build_udp_packet_with_options,
};
