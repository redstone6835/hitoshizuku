use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::buf::{CompletionToken, DropReason, PacketChain};
use crate::control::RouteDecision;
use crate::flow::{DIRTY_INGRESS, FlowKey, FlowTable, flow_hash64, rss_hash};
use crate::pipeline::{FrontendPacket, transport_checksum};
use crate::transport::TransportControlError;
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
    pub traffic_class: u8,
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
    pending_error: Option<TransportControlError>,
    facade: Option<Arc<SocketFacade>>,
    free_bind: bool,
    accepts_ipv4: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpEndpointInfo {
    pub local: Endpoint,
    pub peer: Option<Endpoint>,
    pub interface: Option<InterfaceId>,
    pub free_bind: bool,
    pub accepts_ipv4: bool,
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
    pub parsed: crate::pipeline::ParsedPacket,
}

pub struct PreparedUdpTx {
    pub payload: UdpTxLease,
    pub route: RouteDecision,
    pub destination: Endpoint,
    pub source_port: u16,
    pub source_mac: [u8; 6],
    pub destination_mac: [u8; 6],
    pub unresolved_neighbor: Option<crate::control::NeighborKey>,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub completion: CompletionToken,
}

pub struct UdpEndpointTable {
    rss_key: [u8; 40],
    flows: FlowTable<UdpEndpoint>,
    binds: BTreeMap<UdpBindKey, alloc::vec::Vec<FlowId>>,
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
        self.bind_inner(local, peer, interface, None, false, false)
    }

    pub fn bind_facade(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
    ) -> Result<FlowId, UdpBindError> {
        self.bind_facade_with_options(local, peer, interface, facade, false, false)
    }

    pub fn bind_facade_with_options(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
        free_bind: bool,
        accepts_ipv4: bool,
    ) -> Result<FlowId, UdpBindError> {
        self.bind_inner(
            local,
            peer,
            interface,
            Some(facade),
            free_bind,
            accepts_ipv4,
        )
    }

    fn bind_inner(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        facade: Option<Arc<SocketFacade>>,
        free_bind: bool,
        accepts_ipv4: bool,
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
        let remote = peer.unwrap_or(Endpoint {
            addr: unspecified(address_family),
            port: 0,
        });
        let key = FlowKey::new(remote, local, crate::TransportProtocol::Udp)
            .ok_or(UdpBindError::InvalidEndpoint)?;
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let duplicate_bind = self
            .binds
            .get(&bind_key)
            .is_some_and(|members| !members.is_empty());
        let value = UdpEndpoint {
            local,
            peer,
            interface,
            rx: DatagramRing::new(),
            pending_error: None,
            facade,
            free_bind,
            accepts_ipv4,
        };
        let id = if duplicate_bind {
            self.flows.insert_unindexed(key, hash, value)
        } else {
            self.flows.insert_prehashed(key, hash, value)
        }
        .map_err(|_| UdpBindError::FlowTableFull)?;
        self.binds.entry(bind_key).or_default().push(id);
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
        let mut recipients = connected
            .map(|id| alloc::vec![id])
            .unwrap_or_else(|| self.lookup_bound(interface, flow.local));
        recipients.retain(|id| {
            let Some(endpoint) = self.flows.get(*id) else {
                return false;
            };
            if endpoint.interface.is_some_and(|scope| scope != interface)
                || endpoint.peer.is_some_and(|peer| peer != flow.remote)
            {
                return false;
            }
            if flow.local.addr.is_multicast() {
                return endpoint
                    .facade
                    .as_ref()
                    .is_none_or(|facade| facade.accepts_multicast(flow.local.addr, interface));
            }
            true
        });
        recipients.truncate(16);
        if recipients.is_empty() {
            return Err(UdpIngressError {
                reason: DropReason::UdpNoEndpoint,
                chain: packet.chain,
                metadata: packet.metadata,
                parsed: packet.parsed,
            });
        }
        if !flow.local.addr.is_multicast() && !is_broadcast(flow.local.addr) && connected.is_none()
        {
            let selected = usize::try_from(hash).unwrap_or(0) % recipients.len();
            recipients = alloc::vec![recipients[selected]];
        }
        let packet_bytes = if recipients.len() > 1 {
            let mut bytes = alloc::vec![0; packet.chain.total_len()];
            if packet.chain.copy_out(0, &mut bytes).is_err() {
                return Err(UdpIngressError {
                    reason: DropReason::UdpRingFull,
                    chain: packet.chain,
                    metadata: packet.metadata,
                    parsed: packet.parsed,
                });
            }
            Some(bytes)
        } else {
            None
        };
        let mut original = Some(packet.chain);
        let mut delivered = None;
        for (index, id) in recipients.into_iter().enumerate() {
            let endpoint = self
                .flows
                .get_mut(id)
                .expect("bind 表指向有效 UDP endpoint");
            if endpoint.facade.is_none() && endpoint.rx.is_full() {
                continue;
            }
            let chain = if index == 0 {
                original.take().expect("首个 UDP receiver 取得原包")
            } else {
                PacketChain::from_owned(packet_bytes.as_ref().unwrap().clone())
            };
            let destination = if endpoint.local.addr.is_unspecified() {
                flow.local
            } else {
                endpoint.local
            };
            let datagram = UdpDatagram {
                packet: chain,
                source: flow.remote,
                destination,
                payload_offset: udp.payload_offset,
                payload_len: udp.payload_len,
                hop_limit: packet.parsed.ip.unwrap().hop_limit,
                traffic_class: packet.parsed.ip.unwrap().traffic_class,
                ingress_interface: interface,
                rx_timestamp_ns: packet.metadata.rx_timestamp_ns,
            };
            let accepted = if let Some(facade) = endpoint.facade.as_ref() {
                match facade.push_rx(datagram) {
                    Ok(()) => true,
                    Err(datagram) => {
                        if index == 0 {
                            original = Some(datagram.packet);
                        }
                        false
                    }
                }
            } else {
                endpoint.rx.push(datagram);
                true
            };
            if accepted {
                delivered.get_or_insert(id);
                self.flows.mark_dirty(id, DIRTY_INGRESS);
            }
        }
        delivered.ok_or_else(|| UdpIngressError {
            reason: DropReason::UdpRingFull,
            chain: original
                .unwrap_or_else(|| PacketChain::from_owned(packet_bytes.unwrap_or_default())),
            metadata: packet.metadata,
            parsed: packet.parsed,
        })
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
            free_bind: endpoint.free_bind,
            accepts_ipv4: endpoint.accepts_ipv4,
        })
    }

    pub fn pop_dirty(&mut self) -> Option<(FlowId, u32)> {
        self.flows.pop_dirty()
    }

    pub fn record_control_error(
        &mut self,
        interface: InterfaceId,
        flow: FlowKey,
        error: TransportControlError,
    ) -> Option<FlowId> {
        let hash = flow_hash64(rss_hash(&self.rss_key, &flow));
        let id = self
            .flows
            .find(&flow, hash)
            .or_else(|| self.lookup_bound(interface, flow.local).into_iter().next())?;
        let endpoint = self.flows.get_mut(id)?;
        if endpoint.peer.is_some_and(|peer| peer != flow.remote) {
            return None;
        }
        endpoint.pending_error = Some(error);
        if let Some(facade) = endpoint.facade.as_ref() {
            facade.set_transport_error(error, Some(flow.remote));
        }
        self.flows.mark_dirty(id, crate::flow::DIRTY_CONTROL);
        Some(id)
    }

    pub fn deliver_local_multicast(
        &mut self,
        interface: InterfaceId,
        source: Endpoint,
        destination: Endpoint,
        payload: &[u8],
        hop_limit: u8,
        traffic_class: u8,
        now_ns: u64,
    ) -> usize {
        if !destination.addr.is_multicast() || payload.len() > u16::MAX as usize {
            return 0;
        }
        let mut recipients = self.lookup_bound(interface, destination);
        recipients.retain(|id| {
            self.flows.get(*id).is_some_and(|endpoint| {
                endpoint.interface.is_none_or(|scope| scope == interface)
                    && endpoint.peer.is_none_or(|peer| peer == source)
                    && endpoint
                        .facade
                        .as_ref()
                        .is_some_and(|facade| facade.accepts_multicast(destination.addr, interface))
            })
        });
        recipients.truncate(16);
        let mut delivered = 0;
        for id in recipients {
            let endpoint = self
                .flows
                .get_mut(id)
                .expect("bind 表指向有效 UDP endpoint");
            let datagram = UdpDatagram {
                packet: PacketChain::from_owned(payload.to_vec()),
                source,
                destination,
                payload_offset: 0,
                payload_len: payload.len() as u16,
                hop_limit,
                traffic_class,
                ingress_interface: interface,
                rx_timestamp_ns: now_ns,
            };
            if endpoint
                .facade
                .as_ref()
                .is_some_and(|facade| facade.push_rx(datagram).is_ok())
            {
                delivered += 1;
                self.flows.mark_dirty(id, DIRTY_INGRESS);
            }
        }
        delivered
    }

    pub fn take_control_error(&mut self, id: FlowId) -> Option<TransportControlError> {
        self.flows.get_mut(id)?.pending_error.take()
    }

    pub fn mark_timer(&mut self, id: FlowId, generation: u32) -> bool {
        if self.flows.generation(id) != Some(generation) {
            return false;
        }
        self.flows.mark_dirty(id, crate::flow::DIRTY_TIMER)
    }

    pub fn unbind(&mut self, id: FlowId) -> Option<Arc<SocketFacade>> {
        let endpoint = self.flows.remove_id(id)?;
        self.binds.retain(|_, bound| {
            bound.retain(|candidate| *candidate != id);
            !bound.is_empty()
        });
        endpoint.facade
    }

    pub fn facade(&self, id: FlowId) -> Option<Arc<SocketFacade>> {
        self.flows.get(id)?.facade.as_ref().map(Arc::clone)
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        let affected = (1..=4096)
            .map(FlowId)
            .filter(|id| {
                self.flows
                    .get(*id)
                    .is_some_and(|endpoint| endpoint.interface == Some(interface))
            })
            .collect::<alloc::vec::Vec<_>>();
        for id in &affected {
            if let Some(facade) = self
                .flows
                .get(*id)
                .and_then(|endpoint| endpoint.facade.as_ref())
            {
                facade.set_pending_error(crate::SocketError::NetworkUnreachable);
            }
        }
        affected.len()
    }

    fn lookup_bound(&self, interface: InterfaceId, local: Endpoint) -> alloc::vec::Vec<FlowId> {
        let family = family(local.addr);
        let keys = [
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
        ];
        let fanout = local.addr.is_multicast() || is_broadcast(local.addr);
        let mut result = Vec::new();
        for key in keys {
            let Some(entries) = self.binds.get(&key) else {
                continue;
            };
            result.extend(entries.iter().copied());
            if !fanout {
                break;
            }
        }
        if family == AddressFamily::Ipv4 {
            for key in [
                UdpBindKey {
                    family: AddressFamily::Ipv6,
                    address: None,
                    port: local.port,
                    interface: Some(interface),
                },
                UdpBindKey {
                    family: AddressFamily::Ipv6,
                    address: None,
                    port: local.port,
                    interface: None,
                },
            ] {
                let Some(entries) = self.binds.get(&key) else {
                    continue;
                };
                result.extend(entries.iter().copied().filter(|id| {
                    self.flows
                        .get(*id)
                        .is_some_and(|endpoint| endpoint.accepts_ipv4)
                }));
                if !fanout {
                    break;
                }
            }
        }
        result.sort_unstable();
        result.dedup();
        result
    }
}

fn is_broadcast(address: IpAddr) -> bool {
    matches!(address, IpAddr::V4(address) if address.is_broadcast())
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
    payload: PacketChain,
    route: RouteDecision,
    destination: Endpoint,
    source_port: u16,
    source_mac: [u8; 6],
    destination_mac: [u8; 6],
) -> Result<PacketChain, (UdpTxError, PacketChain)> {
    build_udp_packet_with_options(
        payload,
        route,
        destination,
        source_port,
        source_mac,
        destination_mac,
        64,
        0,
    )
}

pub fn build_udp_packet_with_options(
    mut payload: PacketChain,
    route: RouteDecision,
    destination: Endpoint,
    source_port: u16,
    source_mac: [u8; 6],
    destination_mac: [u8; 6],
    hop_limit: u8,
    traffic_class: u8,
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
            header[1] = traffic_class;
            header[2..4].copy_from_slice(&((payload_len + 28) as u16).to_be_bytes());
            header[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            header[8] = hop_limit;
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
            header[..4].copy_from_slice(
                &(0x6000_0000u32 | (u32::from(traffic_class) << 20)).to_be_bytes(),
            );
            header[4..6].copy_from_slice(&udp_len.to_be_bytes());
            header[6] = IP_PROTOCOL_UDP;
            header[7] = hop_limit;
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

/// UDP 超出路径 MTU 时形成 IPv4 fragment 或 IPv6 Fragment header 报文。
pub fn build_udp_fragments(
    payload: &[u8],
    route: RouteDecision,
    destination: Endpoint,
    source_port: u16,
    source_mac: [u8; 6],
    destination_mac: [u8; 6],
    hop_limit: u8,
    traffic_class: u8,
    identification: u32,
) -> Result<Vec<Vec<u8>>, UdpTxError> {
    if payload.len() > u16::MAX as usize - 8 {
        return Err(UdpTxError::DatagramTooLarge);
    }
    let mut datagram = alloc::vec![0; 8 + payload.len()];
    let datagram_len = datagram.len();
    write_udp_header(
        &mut datagram[..8],
        source_port,
        destination.port,
        datagram_len as u16,
    );
    datagram[8..].copy_from_slice(payload);
    let checksum_chain = PacketChain::from_owned(datagram.clone());
    let checksum = transport_checksum(
        &checksum_chain,
        0,
        datagram.len(),
        route.source,
        destination.addr,
        IP_PROTOCOL_UDP,
    )
    .map_err(|_| UdpTxError::Buffer)?;
    datagram[6..8].copy_from_slice(&(if checksum == 0 { 0xffff } else { checksum }).to_be_bytes());

    let mut ethernet = [0u8; 14];
    ethernet[0..6].copy_from_slice(&destination_mac);
    ethernet[6..12].copy_from_slice(&source_mac);
    let mut frames = Vec::new();
    match (route.source, destination.addr) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            ethernet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let fragment_payload = (route.mtu.saturating_sub(20) as usize) & !7;
            if fragment_payload < 8 {
                return Err(UdpTxError::MtuExceeded);
            }
            for (index, chunk) in datagram.chunks(fragment_payload).enumerate() {
                let offset = index * fragment_payload;
                let more = offset + chunk.len() < datagram.len();
                let mut frame = alloc::vec![0; 14 + 20 + chunk.len()];
                frame[..14].copy_from_slice(&ethernet);
                let ip = &mut frame[14..34];
                ip[0] = 0x45;
                ip[1] = traffic_class;
                ip[2..4].copy_from_slice(&((20 + chunk.len()) as u16).to_be_bytes());
                ip[4..6].copy_from_slice(&(identification as u16).to_be_bytes());
                let fragment = ((offset / 8) as u16) | if more { 0x2000 } else { 0 };
                ip[6..8].copy_from_slice(&fragment.to_be_bytes());
                ip[8] = hop_limit;
                ip[9] = IP_PROTOCOL_UDP;
                ip[12..16].copy_from_slice(&source.0);
                ip[16..20].copy_from_slice(&destination.0);
                let checksum = crate::pipeline::checksum_bytes(ip);
                ip[10..12].copy_from_slice(&checksum.to_be_bytes());
                frame[34..].copy_from_slice(chunk);
                frames.push(frame);
            }
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            ethernet[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let fragment_payload = (route.mtu.saturating_sub(48) as usize) & !7;
            if fragment_payload < 8 {
                return Err(UdpTxError::MtuExceeded);
            }
            for (index, chunk) in datagram.chunks(fragment_payload).enumerate() {
                let offset = index * fragment_payload;
                let more = offset + chunk.len() < datagram.len();
                let mut frame = alloc::vec![0; 14 + 40 + 8 + chunk.len()];
                frame[..14].copy_from_slice(&ethernet);
                let ip = &mut frame[14..54];
                ip[..4].copy_from_slice(
                    &(0x6000_0000u32 | (u32::from(traffic_class) << 20)).to_be_bytes(),
                );
                ip[4..6].copy_from_slice(&((8 + chunk.len()) as u16).to_be_bytes());
                ip[6] = 44;
                ip[7] = hop_limit;
                ip[8..24].copy_from_slice(&source.0);
                ip[24..40].copy_from_slice(&destination.0);
                let fragment = &mut frame[54..62];
                fragment[0] = IP_PROTOCOL_UDP;
                let offset_flags = ((offset / 8) as u16) << 3 | u16::from(more);
                fragment[2..4].copy_from_slice(&offset_flags.to_be_bytes());
                fragment[4..8].copy_from_slice(&identification.to_be_bytes());
                frame[62..].copy_from_slice(chunk);
                frames.push(frame);
            }
        }
        _ => return Err(UdpTxError::AddressFamily),
    }
    Ok(frames)
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
    use crate::{
        AddressFamily, Ipv4Addr, MulticastMembership, SocketFacade, SocketId, SocketKind,
        TransportProtocol,
    };

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
                    traffic_class: 0,
                    fragment: None,
                }),
                tcp: None,
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
    fn multicast_membership_fans_out_to_all_joined_sockets() {
        let mut table = UdpEndpointTable::new([3; 40]);
        let group = IpAddr::V4(Ipv4Addr::new(239, 1, 2, 3));
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 9001,
        };
        let first = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let second = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let membership = MulticastMembership {
            group,
            interface: Some(InterfaceId(1)),
        };
        first.add_multicast_membership(membership).unwrap();
        second.add_multicast_membership(membership).unwrap();
        table
            .bind_facade(local, None, None, Arc::clone(&first))
            .unwrap();
        table
            .bind_facade(local, None, None, Arc::clone(&second))
            .unwrap();
        let mut packet = parsed_packet(1000, 9001);
        let flow = FlowKey::new(
            packet.parsed.flow.unwrap().remote,
            Endpoint {
                addr: group,
                port: 9001,
            },
            TransportProtocol::Udp,
        )
        .unwrap();
        packet.parsed.flow = Some(flow);
        packet.parsed.ip.as_mut().unwrap().destination = group;
        assert!(table.ingest(InterfaceId(1), packet).is_ok());
        assert!(first.readiness().0.contains(crate::Readiness::READABLE));
        assert!(second.readiness().0.contains(crate::Readiness::READABLE));
    }

    #[test]
    fn dual_stack_ipv6_wildcard_receives_ipv4_datagram() {
        let mut table = UdpEndpointTable::new([7; 40]);
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter: 9,
            },
            AddressFamily::Ipv6,
            SocketKind::Datagram,
        ));
        table
            .bind_facade_with_options(
                Endpoint {
                    addr: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    port: 9000,
                },
                None,
                None,
                Arc::clone(&facade),
                false,
                true,
            )
            .unwrap();
        assert!(
            table
                .ingest(InterfaceId(1), parsed_packet(1000, 9000))
                .is_ok()
        );
        assert!(facade.readiness().0.contains(crate::Readiness::READABLE));
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
            table.record_control_error(
                InterfaceId(1),
                flow,
                TransportControlError::PortUnreachable,
            ),
            Some(id)
        );
        assert_eq!(
            table.take_control_error(id),
            Some(TransportControlError::PortUnreachable)
        );
        assert!(table.take_control_error(id).is_none());
    }

    #[test]
    fn oversized_udp_builds_ordered_ipv4_fragments() {
        let payload = alloc::vec![0x5a; 2000];
        let route = RouteDecision {
            interface: InterfaceId(1),
            source: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            next_hop: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            mtu: 600,
            table: 0,
        };
        let frames = build_udp_fragments(
            &payload,
            route,
            Endpoint {
                addr: route.next_hop,
                port: 9000,
            },
            8000,
            [2; 6],
            [1; 6],
            63,
            0x20,
            0x1234,
        )
        .unwrap();
        assert!(frames.len() > 1);
        let mut datagram = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            assert!(frame.len() <= 614);
            assert_eq!(
                u16::from_be_bytes(frame[18..20].try_into().unwrap()),
                0x1234
            );
            let field = u16::from_be_bytes(frame[20..22].try_into().unwrap());
            assert_eq!(usize::from(field & 0x1fff) * 8, datagram.len());
            assert_eq!(field & 0x2000 != 0, index + 1 != frames.len());
            assert_eq!(crate::pipeline::checksum_bytes(&frame[14..34]), 0);
            datagram.extend_from_slice(&frame[34..]);
        }
        assert_eq!(&datagram[8..], payload);
    }

    #[test]
    fn oversized_udp_builds_ipv6_fragment_headers() {
        let payload = alloc::vec![0xa5; 3000];
        let route = RouteDecision {
            interface: InterfaceId(1),
            source: IpAddr::V6(Ipv6Addr([
                0x20, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            ])),
            next_hop: IpAddr::V6(Ipv6Addr([
                0x20, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
            ])),
            mtu: 1280,
            table: 0,
        };
        let frames = build_udp_fragments(
            &payload,
            route,
            Endpoint {
                addr: route.next_hop,
                port: 9000,
            },
            8000,
            [2; 6],
            [1; 6],
            64,
            0,
            0x1234_5678,
        )
        .unwrap();
        assert!(frames.len() > 1);
        let mut offset = 0usize;
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame[20], 44);
            assert_eq!(frame[54], 17);
            assert_eq!(&frame[58..62], &0x1234_5678u32.to_be_bytes());
            let field = u16::from_be_bytes(frame[56..58].try_into().unwrap());
            assert_eq!(usize::from(field >> 3) * 8, offset);
            assert_eq!(field & 1 != 0, index + 1 != frames.len());
            offset += frame.len() - 62;
        }
        assert_eq!(offset, payload.len() + 8);
    }
}
