//! 当前协议引擎的私有适配类型。
//!
//! `libs/net` 的公共 API 不直接暴露 smoltcp 类型。需要暂时穿过 stack 和
//! interface 的协议引擎句柄，都先封装在本模块，后续替换协议栈时只需保留
//! 同等语义的轻量标识符。

use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address};

use crate::config::{Endpoint, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::socket::SocketState;

/// 协议引擎 socket 表中的槽位句柄。
///
/// 这是 crate 内部的 opaque handle。生命周期校验仍由
/// [`crate::socket::NetSocketHandle`] 上的 generation/type 字段完成。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProtocolSocketHandle {
    inner: smoltcp::iface::SocketHandle,
}

impl core::fmt::Debug for ProtocolSocketHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ProtocolSocketHandle(..)")
    }
}

impl ProtocolSocketHandle {
    pub(crate) fn from_smoltcp(inner: smoltcp::iface::SocketHandle) -> Self {
        Self { inner }
    }

    pub(crate) fn into_smoltcp(self) -> smoltcp::iface::SocketHandle {
        self.inner
    }
}

/// 将网络层端点转换为当前协议引擎的远端端点。
pub(crate) fn endpoint_to_smoltcp(ep: &Endpoint) -> IpEndpoint {
    let addr = match ep.addr {
        IpAddr::V4(v4) => IpAddress::Ipv4(Ipv4Address::new(v4.0[0], v4.0[1], v4.0[2], v4.0[3])),
        IpAddr::V6(v6) => {
            let o = &v6.0;
            IpAddress::Ipv6(Ipv6Address::new(
                u16::from_be_bytes([o[0], o[1]]),
                u16::from_be_bytes([o[2], o[3]]),
                u16::from_be_bytes([o[4], o[5]]),
                u16::from_be_bytes([o[6], o[7]]),
                u16::from_be_bytes([o[8], o[9]]),
                u16::from_be_bytes([o[10], o[11]]),
                u16::from_be_bytes([o[12], o[13]]),
                u16::from_be_bytes([o[14], o[15]]),
            ))
        }
    };
    IpEndpoint::new(addr, ep.port)
}

/// 将网络层监听端点转换为当前协议引擎的监听端点。
pub(crate) fn endpoint_to_smoltcp_listen(ep: &Endpoint) -> IpListenEndpoint {
    if is_unspecified_ip(&ep.addr) {
        return IpListenEndpoint {
            addr: None,
            port: ep.port,
        };
    }
    IpListenEndpoint {
        addr: Some(endpoint_to_smoltcp(ep).addr),
        port: ep.port,
    }
}

/// 将当前协议引擎端点转换为网络层端点。
pub(crate) fn endpoint_from_smoltcp(ep: IpEndpoint) -> Endpoint {
    let addr = match ep.addr {
        IpAddress::Ipv4(v4) => {
            let o = v4.octets();
            IpAddr::V4(Ipv4Addr(o))
        }
        IpAddress::Ipv6(v6) => IpAddr::V6(Ipv6Addr(v6.octets())),
    };
    Endpoint {
        addr,
        port: ep.port,
    }
}

/// 将当前协议引擎 TCP 状态映射为网络层可观测状态。
pub(crate) fn tcp_state_to_socket_state(state: smoltcp::socket::tcp::State) -> SocketState {
    use smoltcp::socket::tcp::State as S;
    match state {
        S::Closed | S::TimeWait => SocketState::Closed,
        S::Listen => SocketState::Listen,
        S::SynSent | S::SynReceived => SocketState::Connecting,
        S::Established => SocketState::Established,
        S::FinWait1 | S::FinWait2 | S::Closing | S::CloseWait | S::LastAck => SocketState::Closing,
    }
}

/// 对端 FIN 后，即使接收缓冲为空，poll/read 也必须能观察到 EOF。
pub(crate) fn tcp_state_is_read_eof(state: smoltcp::socket::tcp::State) -> bool {
    use smoltcp::socket::tcp::State as S;
    matches!(state, S::CloseWait | S::Closing | S::LastAck | S::TimeWait)
}

fn is_unspecified_ip(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => *v4 == Ipv4Addr::UNSPECIFIED,
        IpAddr::V6(v6) => *v6 == Ipv6Addr::UNSPECIFIED,
    }
}
