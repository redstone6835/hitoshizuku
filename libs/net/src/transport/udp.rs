use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use crate::buf::{CompletionToken, DropReason, PacketChain};
use crate::control::RouteDecision;
use crate::flow::{DIRTY_INGRESS, FlowKey, FlowTable, flow_hash64, rss_hash};
use crate::pipeline::{FrontendPacket, transport_checksum};
use crate::transport::UdpControlError;
use crate::{AddressFamily, Endpoint, FlowId, InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::{SocketFacade, UdpTxLease};

const UDP_RX_DATAGRAMS: usize = 256;
const IP_PROTOCOL_UDP: u8 = 17;

pub struct UdpDatagram {
    pub packet: PacketChain,
    pub source: Endpoint,
    pub destination: Endpoint,
    pub payload_offset: u16,
    pub payload_len: u16,
    pub hop_limit: u8,
    pub ingress_interface: InterfaceId,
    pub rx_timestamp_ns: u64,
}

struct DatagramRing {
    entries: Box<[Option<UdpDatagram>]>,
    head: u16,
    tail: u16,
    len: u16,
}

impl DatagramRing {
    fn new() -> Self {
        Self {
            entries: core::iter::repeat_with(|| None)
                .take(UDP_RX_DATAGRAMS)
                .collect::<alloc::vec::Vec<_>>()
                .into_boxed_slice(),
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn is_full(&self) -> bool {
        usize::from(self.len) == self.entries.len()
    }

    fn push(&mut self, datagram: UdpDatagram) {
        assert!(!self.is_full());
        self.entries[usize::from(self.tail)] = Some(datagram);
        self.tail = (self.tail + 1) % self.entries.len() as u16;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<UdpDatagram> {
        if self.len == 0 {
            return None;
        }
        let datagram = self.entries[usize::from(self.head)].take();
        self.head = (self.head + 1) % self.entries.len() as u16;
        self.len -= 1;
        datagram
    }
}

struct UdpEndpoint {
    local: Endpoint,
    peer: Option<Endpoint>,
    interface: Option<InterfaceId>,
    rx: DatagramRing,
    pending_error: Option<UdpControlError>,
    facade: Option<Arc<SocketFacade>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpEndpointInfo {
    pub local: Endpoint,
    pub peer: Option<Endpoint>,
    pub interface: Option<InterfaceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct UdpBindKey {
    family: AddressFamily,
    address: Option<IpAddr>,
    port: u16,
    interface: Option<InterfaceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpBindError {
    InvalidEndpoint,
    AddressInUse,
    FlowTableFull,
}

pub struct UdpIngressError {
    pub reason: DropReason,
    pub chain: PacketChain,
    pub metadata: crate::buf::PacketMetadata,
}

pub struct PreparedUdpTx {
    pub payload: UdpTxLease,
    pub route: RouteDecision,
    pub destination: Endpoint,
    pub source_port: u16,
    pub source_mac: [u8; 6],
    pub destination_mac: [u8; 6],
    pub completion: CompletionToken,
}

pub struct UdpEndpointTable {
    rss_key: [u8; 40],
    flows: FlowTable<UdpEndpoint>,
    binds: BTreeMap<UdpBindKey, FlowId>,
}

impl UdpEndpointTable {
    pub fn new(rss_key: [u8; 40]) -> Self {
        Self {
            rss_key,
            flows: FlowTable::new(),
            binds: BTreeMap::new(),
        }
    }

    pub fn bind(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
    ) -> Result<FlowId, UdpBindError> {
        self.bind_inner(local, peer, interface, None)
    }

    pub fn bind_facade(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
    ) -> Result<FlowId, UdpBindError> {
        self.bind_inner(local, peer, interface, Some(facade))
    }

    fn bind_inner(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        facade: Option<Arc<SocketFacade>>,
    ) -> Result<FlowId, UdpBindError> {
        let address_family = family(local.addr);
        if local.port == 0
            || peer.is_some_and(|peer| family(peer.addr) != address_family || peer.port == 0)
        {
            return Err(UdpBindError::InvalidEndpoint);
        }
        let bind_key = UdpBindKey {
            family: address_family,
            address: (!local.addr.is_unspecified()).then_some(local.addr),
            port: local.port,
            interface,
        };
        if self.binds.contains_key(&bind_key) {
            return Err(UdpBindError::AddressInUse);
        }
        let remote = peer.unwrap_or(Endpoint {
            addr: unspecified(address_family),
            port: 0,
        });
        let key = FlowKey::new(remote, local, crate::TransportProtocol::Udp)
            .ok_or(UdpBindError::InvalidEndpoint)?;
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let id = self
            .flows
            .insert_prehashed(
                key,
                hash,
                UdpEndpoint {
                    local,
                    peer,
                    interface,
                    rx: DatagramRing::new(),
                    pending_error: None,
                    facade,
                },
            )
            .map_err(|_| UdpBindError::FlowTableFull)?;
        self.binds.insert(bind_key, id);
        Ok(id)
    }

    pub fn ingest(
        &mut self,
        interface: InterfaceId,
        packet: FrontendPacket,
    ) -> Result<FlowId, UdpIngressError> {
        let flow = packet.parsed.flow.expect("UDP packet 必须携带 FlowKey");
        let udp = packet.parsed.udp.expect("UDP packet 必须携带解析结果");
        let hash = flow_hash64(
            packet
                .parsed
                .rss_hash
                .unwrap_or_else(|| rss_hash(&self.rss_key, &flow)),
        );
        let connected = self.flows.find(&flow, hash);
        let id = connected.or_else(|| self.lookup_bound(interface, flow.local));
        let Some(id) = id else {
            return Err(UdpIngressError {
                reason: DropReason::UdpNoEndpoint,
                chain: packet.chain,
                metadata: packet.metadata,
            });
        };
        let endpoint = self
            .flows
            .get_mut(id)
            .expect("bind 表指向有效 UDP endpoint");
        if endpoint.interface.is_some_and(|scope| scope != interface)
            || endpoint.peer.is_some_and(|peer| peer != flow.remote)
        {
            return Err(UdpIngressError {
                reason: DropReason::UdpNoEndpoint,
                chain: packet.chain,
                metadata: packet.metadata,
            });
        }
        if endpoint.facade.is_none() && endpoint.rx.is_full() {
            return Err(UdpIngressError {
                reason: DropReason::UdpRingFull,
                chain: packet.chain,
                metadata: packet.metadata,
            });
        }
        let destination = if endpoint.local.addr.is_unspecified() {
            flow.local
        } else {
            endpoint.local
        };
        let datagram = UdpDatagram {
            packet: packet.chain,
            source: flow.remote,
            destination,
            payload_offset: udp.payload_offset,
            payload_len: udp.payload_len,
            hop_limit: packet.parsed.ip.unwrap().hop_limit,
            ingress_interface: interface,
            rx_timestamp_ns: packet.metadata.rx_timestamp_ns,
        };
        if let Some(facade) = endpoint.facade.as_ref() {
            if let Err(datagram) = facade.push_rx(datagram) {
                return Err(UdpIngressError {
                    reason: DropReason::UdpRingFull,
                    chain: datagram.packet,
                    metadata: packet.metadata,
                });
            }
        } else {
            endpoint.rx.push(datagram);
        }
        self.flows.mark_dirty(id, DIRTY_INGRESS);
        Ok(id)
    }

    pub fn recv(&mut self, id: FlowId) -> Option<UdpDatagram> {
        self.flows.get_mut(id)?.rx.pop()
    }

    pub fn endpoint_info(&self, id: FlowId) -> Option<UdpEndpointInfo> {
        let endpoint = self.flows.get(id)?;
        Some(UdpEndpointInfo {
            local: endpoint.local,
            peer: endpoint.peer,
            interface: endpoint.interface,
        })
    }

    pub fn pop_dirty(&mut self) -> Option<(FlowId, u32)> {
        self.flows.pop_dirty()
    }

    pub fn record_control_error(
        &mut self,
        interface: InterfaceId,
        flow: FlowKey,
        error: UdpControlError,
    ) -> Option<FlowId> {
        let hash = flow_hash64(rss_hash(&self.rss_key, &flow));
        let id = self
            .flows
            .find(&flow, hash)
            .or_else(|| self.lookup_bound(interface, flow.local))?;
        let endpoint = self.flows.get_mut(id)?;
        if endpoint.peer.is_some_and(|peer| peer != flow.remote) {
            return None;
        }
        endpoint.pending_error = Some(error);
        self.flows.mark_dirty(id, crate::flow::DIRTY_CONTROL);
        Some(id)
    }

    pub fn take_control_error(&mut self, id: FlowId) -> Option<UdpControlError> {
        self.flows.get_mut(id)?.pending_error.take()
    }

    pub fn mark_timer(&mut self, id: FlowId, generation: u32) -> bool {
        if self.flows.generation(id) != Some(generation) {
            return false;
        }
        self.flows.mark_dirty(id, crate::flow::DIRTY_TIMER)
    }

    pub fn unbind(&mut self, id: FlowId) -> Option<Arc<SocketFacade>> {
        let key = self.flows.key(id)?;
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let endpoint = self.flows.remove(&key, hash)?;
        self.binds.retain(|_, bound| *bound != id);
        endpoint.facade
    }

    pub fn facade(&self, id: FlowId) -> Option<Arc<SocketFacade>> {
        self.flows.get(id)?.facade.as_ref().map(Arc::clone)
    }

    fn lookup_bound(&self, interface: InterfaceId, local: Endpoint) -> Option<FlowId> {
        let family = family(local.addr);
        [
            UdpBindKey {
                family,
                address: Some(local.addr),
                port: local.port,
                interface: Some(interface),
            },
            UdpBindKey {
                family,
                address: Some(local.addr),
                port: local.port,
                interface: None,
            },
            UdpBindKey {
                family,
                address: None,
                port: local.port,
                interface: Some(interface),
            },
            UdpBindKey {
                family,
                address: None,
                port: local.port,
                interface: None,
            },
        ]
        .into_iter()
        .find_map(|key| self.binds.get(&key).copied())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpTxError {
    AddressFamily,
    DatagramTooLarge,
    MtuExceeded,
    Buffer,
}

/// 在 payload 前直接形成 Ethernet/IP/UDP header，不复制 payload。
pub fn build_udp_packet(
    mut payload: PacketChain,
    route: RouteDecision,
    destination: Endpoint,
    source_port: u16,
    source_mac: [u8; 6],
    destination_mac: [u8; 6],
) -> Result<PacketChain, (UdpTxError, PacketChain)> {
    let payload_len = payload.total_len();
    let (header_len, protocol_header_len) = match (route.source, destination.addr) {
        (IpAddr::V4(_), IpAddr::V4(_)) => (42usize, 28usize),
        (IpAddr::V6(_), IpAddr::V6(_)) => (62usize, 48usize),
        _ => return Err((UdpTxError::AddressFamily, payload)),
    };
    if payload_len > u16::MAX as usize - 8 {
        return Err((UdpTxError::DatagramTooLarge, payload));
    }
    if header_len + payload_len > route.mtu as usize + 14 {
        return Err((UdpTxError::MtuExceeded, payload));
    }
    if payload.prepend_first_zeroed(header_len as u16).is_err() {
        return Err((UdpTxError::Buffer, payload));
    }
    let mut ethernet = [0u8; 14];
    ethernet[0..6].copy_from_slice(&destination_mac);
    ethernet[6..12].copy_from_slice(&source_mac);
    let udp_len = (payload_len + 8) as u16;
    match (route.source, destination.addr) {
        (IpAddr::V4(source), IpAddr::V4(destination_address)) => {
            ethernet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let mut header = [0u8; 28];
            header[0] = 0x45;
            header[2..4].copy_from_slice(&((payload_len + 28) as u16).to_be_bytes());
            header[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            header[8] = 64;
            header[9] = IP_PROTOCOL_UDP;
            header[12..16].copy_from_slice(&source.0);
            header[16..20].copy_from_slice(&destination_address.0);
            let checksum = crate::pipeline::checksum_bytes(&header[..20]);
            header[10..12].copy_from_slice(&checksum.to_be_bytes());
            write_udp_header(&mut header[20..28], source_port, destination.port, udp_len);
            if payload.copy_in(0, &ethernet).is_err() || payload.copy_in(14, &header).is_err() {
                return Err((UdpTxError::Buffer, payload));
            }
            let Ok(checksum) = transport_checksum(
                &payload,
                34,
                usize::from(udp_len),
                route.source,
                destination.addr,
                IP_PROTOCOL_UDP,
            ) else {
                return Err((UdpTxError::Buffer, payload));
            };
            let checksum = if checksum == 0 { 0xffff } else { checksum };
            if payload.copy_in(40, &checksum.to_be_bytes()).is_err() {
                return Err((UdpTxError::Buffer, payload));
            }
        }
        (IpAddr::V6(source), IpAddr::V6(destination_address)) => {
            ethernet[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let mut header = [0u8; 48];
            header[0] = 0x60;
            header[4..6].copy_from_slice(&udp_len.to_be_bytes());
            header[6] = IP_PROTOCOL_UDP;
            header[7] = 64;
            header[8..24].copy_from_slice(&source.0);
            header[24..40].copy_from_slice(&destination_address.0);
            write_udp_header(&mut header[40..48], source_port, destination.port, udp_len);
            if payload.copy_in(0, &ethernet).is_err() || payload.copy_in(14, &header).is_err() {
                return Err((UdpTxError::Buffer, payload));
            }
            let Ok(checksum) = transport_checksum(
                &payload,
                54,
                usize::from(udp_len),
                route.source,
                destination.addr,
                IP_PROTOCOL_UDP,
            ) else {
                return Err((UdpTxError::Buffer, payload));
            };
            let checksum = if checksum == 0 { 0xffff } else { checksum };
            if payload.copy_in(60, &checksum.to_be_bytes()).is_err() {
                return Err((UdpTxError::Buffer, payload));
            }
        }
        _ => unreachable!(),
    }
    debug_assert_eq!(protocol_header_len + 14, header_len);
    Ok(payload)
}

fn write_udp_header(header: &mut [u8], source: u16, destination: u16, len: u16) {
    header[0..2].copy_from_slice(&source.to_be_bytes());
    header[2..4].copy_from_slice(&destination.to_be_bytes());
    header[4..6].copy_from_slice(&len.to_be_bytes());
    header[6..8].fill(0);
}

fn family(address: IpAddr) -> AddressFamily {
    match address {
        IpAddr::V4(_) => AddressFamily::Ipv4,
        IpAddr::V6(_) => AddressFamily::Ipv6,
    }
}

fn unspecified(family: AddressFamily) -> IpAddr {
    match family {
        AddressFamily::Ipv4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        AddressFamily::Ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{EthernetHeader, FrontendDisposition, IpPacket, ParsedPacket, UdpPacket};
    use crate::{Ipv4Addr, TransportProtocol};

    fn parsed_packet(remote_port: u16, local_port: u16) -> FrontendPacket {
        let flow = FlowKey::new(
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
                port: remote_port,
            },
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
                port: local_port,
            },
            TransportProtocol::Udp,
        )
        .unwrap();
        FrontendPacket {
            chain: PacketChain::new(),
            metadata: crate::buf::PacketMetadata::default(),
            parsed: ParsedPacket {
                ethernet: EthernetHeader {
                    destination: [0; 6],
                    source: [0; 6],
                    ethertype: 0x0800,
                },
                ip: Some(IpPacket {
                    source: flow.remote.addr,
                    destination: flow.local.addr,
                    next_header: 17,
                    header_len: 20,
                    payload_offset: 42,
                    payload_len: 8,
                    hop_limit: 64,
                    fragment: None,
                }),
                udp: Some(UdpPacket {
                    source_port: remote_port,
                    destination_port: local_port,
                    payload_offset: 42,
                    payload_len: 0,
                }),
                flow: Some(flow),
                rss_hash: Some(3),
                disposition: FrontendDisposition::Udp,
            },
        }
    }

    #[test]
    fn connected_endpoint_filters_peer_without_scanning_binds() {
        let mut table = UdpEndpointTable::new([3; 40]);
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
            port: 9000,
        };
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
            port: 1000,
        };
        let id = table.bind(local, Some(peer), None).unwrap();
        assert!(
            table
                .ingest(InterfaceId(1), parsed_packet(1000, 9000))
                .is_ok()
        );
        assert_eq!(table.recv(id).unwrap().source, peer);
        assert!(
            table
                .ingest(InterfaceId(1), parsed_packet(1001, 9000))
                .is_err()
        );
    }

    #[test]
    fn wildcard_endpoint_receives_complete_datagram_atomically() {
        let mut table = UdpEndpointTable::new([4; 40]);
        let id = table
            .bind(
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 9000,
                },
                None,
                None,
            )
            .unwrap();
        assert!(
            table
                .ingest(InterfaceId(1), parsed_packet(1000, 9000))
                .is_ok()
        );
        let datagram = table.recv(id).unwrap();
        assert_eq!(datagram.payload_len, 0);
        assert!(table.recv(id).is_none());
    }

    #[test]
    fn icmp_error_is_associated_with_connected_endpoint() {
        let mut table = UdpEndpointTable::new([5; 40]);
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
            port: 9000,
        };
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
            port: 1000,
        };
        let id = table.bind(local, Some(peer), None).unwrap();
        let flow = FlowKey::new(peer, local, TransportProtocol::Udp).unwrap();
        assert_eq!(
            table.record_control_error(InterfaceId(1), flow, UdpControlError::PortUnreachable,),
            Some(id)
        );
        assert_eq!(
            table.take_control_error(id),
            Some(UdpControlError::PortUnreachable)
        );
        assert!(table.take_control_error(id).is_none());
    }
}
