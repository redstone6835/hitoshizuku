use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::SocketFacade;
use crate::buf::{CompletionToken, PacketChain};
use crate::control::RouteDecision;
use crate::pipeline::FrontendPacket;
use crate::transport::UdpDatagram;
use crate::{AddressFamily, Endpoint, FlowId, InterfaceId, IpAddr, Ipv4Addr};

const RAW_FANOUT_LIMIT: usize = 16;
const RAW_ENDPOINT_LIMIT: usize = 4096;

struct RawEndpoint {
    facade: Arc<SocketFacade>,
    family: AddressFamily,
    protocol: u8,
    local: IpAddr,
    interface: Option<InterfaceId>,
    free_bind: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEndpointInfo {
    pub local: IpAddr,
    pub interface: Option<InterfaceId>,
    pub protocol: u8,
    pub free_bind: bool,
}

pub struct PreparedRawTx {
    pub payload: crate::UdpTxLease,
    pub route: RouteDecision,
    pub destination: IpAddr,
    pub source_mac: [u8; 6],
    pub destination_mac: [u8; 6],
    pub unresolved_neighbor: Option<crate::control::NeighborKey>,
    pub protocol: u8,
    pub header_included: bool,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub completion: CompletionToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawTxError {
    AddressFamily,
    PacketTooLarge,
    MtuExceeded,
    InvalidHeader,
    Buffer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawBindError {
    InvalidEndpoint,
    TableFull,
}

pub struct RawIngressResult {
    pub delivered: usize,
    pub copied_packets: usize,
    pub copied_bytes: usize,
    pub undelivered: Option<FrontendPacket>,
}

pub struct RawEndpointTable {
    endpoints: BTreeMap<FlowId, RawEndpoint>,
    next_id: u32,
}

impl RawEndpointTable {
    pub const fn new() -> Self {
        Self {
            endpoints: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub fn bind_facade(
        &mut self,
        local: IpAddr,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
    ) -> Result<FlowId, RawBindError> {
        self.bind_facade_with_options(local, interface, facade, false)
    }

    pub fn bind_facade_with_options(
        &mut self,
        local: IpAddr,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
        free_bind: bool,
    ) -> Result<FlowId, RawBindError> {
        if family(local) != facade.family() || facade.protocol() == 0 {
            return Err(RawBindError::InvalidEndpoint);
        }
        if self.endpoints.len() >= RAW_ENDPOINT_LIMIT {
            return Err(RawBindError::TableFull);
        }
        let id = FlowId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.endpoints.insert(
            id,
            RawEndpoint {
                family: facade.family(),
                protocol: facade.protocol(),
                local,
                interface,
                facade,
                free_bind,
            },
        );
        Ok(id)
    }

    pub fn close(&mut self, id: FlowId) -> Option<Arc<SocketFacade>> {
        self.endpoints.remove(&id).map(|entry| entry.facade)
    }

    pub fn endpoint_info(&self, id: FlowId) -> Option<RawEndpointInfo> {
        let endpoint = self.endpoints.get(&id)?;
        Some(RawEndpointInfo {
            local: endpoint.local,
            interface: endpoint.interface,
            protocol: endpoint.protocol,
            free_bind: endpoint.free_bind,
        })
    }

    pub fn facade(&self, id: FlowId) -> Option<Arc<SocketFacade>> {
        self.endpoints
            .get(&id)
            .map(|endpoint| Arc::clone(&endpoint.facade))
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        let affected = self
            .endpoints
            .values()
            .filter(|endpoint| endpoint.interface == Some(interface))
            .collect::<Vec<_>>();
        for endpoint in &affected {
            endpoint
                .facade
                .set_pending_error(crate::SocketError::NetworkUnreachable);
        }
        affected.len()
    }

    pub fn record_control_error(
        &self,
        interface: InterfaceId,
        local: IpAddr,
        remote: IpAddr,
        protocol: u8,
        error: crate::transport::TransportControlError,
    ) -> usize {
        let mut matches = self
            .endpoints
            .values()
            .filter(|entry| {
                entry.protocol == protocol
                    && entry.family == family(local)
                    && entry.interface.is_none_or(|scope| scope == interface)
                    && (entry.local.is_unspecified() || entry.local == local)
            })
            .collect::<Vec<_>>();
        matches.sort_unstable_by_key(|entry| entry.facade.id());
        let mut delivered = 0;
        for endpoint in matches.into_iter().take(RAW_FANOUT_LIMIT) {
            endpoint.facade.set_transport_error(
                error,
                Some(Endpoint {
                    addr: remote,
                    port: 0,
                }),
            );
            delivered += 1;
        }
        delivered
    }

    /// 未知传输协议路径把原包交给第一个接收者，其余接收者得到有界复制。
    pub fn ingest(&mut self, interface: InterfaceId, packet: FrontendPacket) -> RawIngressResult {
        self.fanout(interface, packet)
    }

    /// ICMP 等内核控制协议保留原包，raw socket 全部接收有界复制。
    pub fn copy_fanout(
        &mut self,
        interface: InterfaceId,
        packet: &FrontendPacket,
    ) -> RawIngressResult {
        let copy = copy_packet(packet);
        let Some((bytes, source, destination, hop_limit, traffic_class)) = copy else {
            return RawIngressResult {
                delivered: 0,
                copied_packets: 0,
                copied_bytes: 0,
                undelivered: None,
            };
        };
        let mut delivered = 0;
        let mut copied_bytes = 0;
        for endpoint in self
            .matching(interface, packet)
            .into_iter()
            .take(RAW_FANOUT_LIMIT)
        {
            let chain = PacketChain::from_owned(bytes.clone());
            let datagram = raw_datagram(
                chain,
                0,
                source,
                destination,
                hop_limit,
                traffic_class,
                interface,
                packet.metadata.rx_timestamp_ns,
            );
            if endpoint.facade.push_rx(datagram).is_ok() {
                delivered += 1;
                copied_bytes += bytes.len();
            }
        }
        RawIngressResult {
            delivered,
            copied_packets: delivered,
            copied_bytes,
            undelivered: None,
        }
    }

    fn fanout(&mut self, interface: InterfaceId, packet: FrontendPacket) -> RawIngressResult {
        let matches = self.matching(interface, &packet);
        if matches.is_empty() {
            return RawIngressResult {
                delivered: 0,
                copied_packets: 0,
                copied_bytes: 0,
                undelivered: Some(packet),
            };
        }
        let copied = copy_packet(&packet);
        let ip = packet.parsed.ip.expect("raw packet 必须携带 IP sidecar");
        let source = Endpoint {
            addr: ip.source,
            port: 0,
        };
        let destination = Endpoint {
            addr: ip.destination,
            port: 0,
        };
        let timestamp = packet.metadata.rx_timestamp_ns;
        let metadata = packet.metadata;
        let parsed = packet.parsed;
        let mut original = Some(packet);
        let mut delivered = 0;
        let mut copied_packets = 0;
        let mut copied_bytes = 0;
        for (index, endpoint) in matches.into_iter().take(RAW_FANOUT_LIMIT).enumerate() {
            let datagram = if index == 0 {
                let packet = original.take().expect("首个 raw receiver 取得原包");
                raw_datagram(
                    packet.chain,
                    14,
                    source,
                    destination,
                    ip.hop_limit,
                    ip.traffic_class,
                    interface,
                    timestamp,
                )
            } else {
                let Some((bytes, _, _, _, _)) = copied.as_ref() else {
                    break;
                };
                copied_packets += 1;
                copied_bytes += bytes.len();
                raw_datagram(
                    PacketChain::from_owned(bytes.clone()),
                    0,
                    source,
                    destination,
                    ip.hop_limit,
                    ip.traffic_class,
                    interface,
                    timestamp,
                )
            };
            match endpoint.facade.push_rx(datagram) {
                Ok(()) => delivered += 1,
                Err(datagram) if index == 0 => {
                    original = Some(FrontendPacket {
                        chain: datagram.packet,
                        metadata,
                        parsed,
                    });
                }
                Err(_) => {}
            }
        }
        RawIngressResult {
            delivered,
            copied_packets,
            copied_bytes,
            undelivered: original,
        }
    }

    fn matching<'a>(
        &'a self,
        interface: InterfaceId,
        packet: &FrontendPacket,
    ) -> Vec<&'a RawEndpoint> {
        let Some(ip) = packet.parsed.ip else {
            return Vec::new();
        };
        let family = family(ip.destination);
        let mut entries = self
            .endpoints
            .values()
            .filter(|entry| {
                entry.family == family
                    && entry.protocol == ip.next_header
                    && entry.interface.is_none_or(|scope| scope == interface)
                    && (entry.local.is_unspecified() || entry.local == ip.destination)
                    && (!ip.destination.is_multicast()
                        || entry.facade.accepts_multicast(ip.destination, interface))
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| entry.facade.id());
        entries
    }
}

impl Default for RawEndpointTable {
    fn default() -> Self {
        Self::new()
    }
}

fn copy_packet(packet: &FrontendPacket) -> Option<(Vec<u8>, Endpoint, Endpoint, u8, u8)> {
    let ip = packet.parsed.ip?;
    let len = packet.chain.total_len().checked_sub(14)?;
    let mut bytes = alloc::vec![0; len];
    packet.chain.copy_out(14, &mut bytes).ok()?;
    Some((
        bytes,
        Endpoint {
            addr: ip.source,
            port: 0,
        },
        Endpoint {
            addr: ip.destination,
            port: 0,
        },
        ip.hop_limit,
        ip.traffic_class,
    ))
}

fn raw_datagram(
    packet: PacketChain,
    payload_offset: u16,
    source: Endpoint,
    destination: Endpoint,
    hop_limit: u8,
    traffic_class: u8,
    interface: InterfaceId,
    rx_timestamp_ns: u64,
) -> UdpDatagram {
    let payload_len = packet
        .total_len()
        .saturating_sub(usize::from(payload_offset))
        .min(u16::MAX as usize) as u16;
    UdpDatagram {
        packet,
        source,
        destination,
        payload_offset,
        payload_len,
        hop_limit,
        traffic_class,
        ingress_interface: interface,
        rx_timestamp_ns,
    }
}

fn family(address: IpAddr) -> AddressFamily {
    match address {
        IpAddr::V4(_) => AddressFamily::Ipv4,
        IpAddr::V6(_) => AddressFamily::Ipv6,
    }
}

pub fn build_raw_packet(
    mut payload: PacketChain,
    work: &PreparedRawTx,
) -> Result<PacketChain, (RawTxError, PacketChain)> {
    if work.header_included {
        return build_header_included_ipv4(payload, work);
    }
    let payload_len = payload.total_len();
    let header_len = match (work.route.source, work.destination) {
        (IpAddr::V4(_), IpAddr::V4(_)) => 34usize,
        (IpAddr::V6(_), IpAddr::V6(_)) => 54usize,
        _ => return Err((RawTxError::AddressFamily, payload)),
    };
    if payload_len > u16::MAX as usize || header_len + payload_len > work.route.mtu as usize + 14 {
        return Err((RawTxError::MtuExceeded, payload));
    }
    if payload.prepend_first_zeroed(header_len as u16).is_err() {
        return Err((RawTxError::Buffer, payload));
    }
    let mut ethernet = [0u8; 14];
    ethernet[0..6].copy_from_slice(&work.destination_mac);
    ethernet[6..12].copy_from_slice(&work.source_mac);
    match (work.route.source, work.destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            if payload_len > u16::MAX as usize - 20 {
                return Err((RawTxError::PacketTooLarge, payload));
            }
            ethernet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let mut header = [0u8; 20];
            header[0] = 0x45;
            header[1] = work.traffic_class;
            header[2..4].copy_from_slice(&((payload_len + 20) as u16).to_be_bytes());
            header[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            header[8] = work.hop_limit;
            header[9] = work.protocol;
            header[12..16].copy_from_slice(&source.0);
            header[16..20].copy_from_slice(&destination.0);
            let checksum = crate::pipeline::checksum_bytes(&header);
            header[10..12].copy_from_slice(&checksum.to_be_bytes());
            if payload.copy_in(0, &ethernet).is_err() || payload.copy_in(14, &header).is_err() {
                return Err((RawTxError::Buffer, payload));
            }
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            ethernet[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let mut header = [0u8; 40];
            let version_class = 0x6000_0000u32 | (u32::from(work.traffic_class) << 20);
            header[..4].copy_from_slice(&version_class.to_be_bytes());
            header[4..6].copy_from_slice(&(payload_len as u16).to_be_bytes());
            header[6] = work.protocol;
            header[7] = work.hop_limit;
            header[8..24].copy_from_slice(&source.0);
            header[24..40].copy_from_slice(&destination.0);
            if payload.copy_in(0, &ethernet).is_err() || payload.copy_in(14, &header).is_err() {
                return Err((RawTxError::Buffer, payload));
            }
        }
        _ => unreachable!(),
    }
    Ok(payload)
}

fn build_header_included_ipv4(
    mut packet: PacketChain,
    work: &PreparedRawTx,
) -> Result<PacketChain, (RawTxError, PacketChain)> {
    let (IpAddr::V4(route_source), IpAddr::V4(route_destination)) =
        (work.route.source, work.destination)
    else {
        return Err((RawTxError::AddressFamily, packet));
    };
    let mut header = [0u8; 60];
    if packet.total_len() < 20 || packet.copy_out(0, &mut header[..20]).is_err() {
        return Err((RawTxError::InvalidHeader, packet));
    }
    let header_len = usize::from(header[0] & 0x0f) * 4;
    if header[0] >> 4 != 4
        || !(20..=60).contains(&header_len)
        || packet.total_len() < header_len
        || packet.copy_out(0, &mut header[..header_len]).is_err()
    {
        return Err((RawTxError::InvalidHeader, packet));
    }
    if packet.total_len() > u16::MAX as usize || packet.total_len() > work.route.mtu as usize {
        return Err((RawTxError::MtuExceeded, packet));
    }
    let destination = IpAddr::V4(Ipv4Addr(header[16..20].try_into().unwrap()));
    if !destination.is_unspecified() && destination != IpAddr::V4(route_destination) {
        return Err((RawTxError::InvalidHeader, packet));
    }
    if header[12..16] == [0; 4] {
        header[12..16].copy_from_slice(&route_source.0);
    }
    header[16..20].copy_from_slice(&route_destination.0);
    header[2..4].copy_from_slice(&(packet.total_len() as u16).to_be_bytes());
    header[10..12].fill(0);
    let checksum = crate::pipeline::checksum_bytes(&header[..header_len]);
    header[10..12].copy_from_slice(&checksum.to_be_bytes());
    if packet.copy_in(0, &header[..header_len]).is_err() || packet.prepend_first_zeroed(14).is_err()
    {
        return Err((RawTxError::Buffer, packet));
    }
    let mut ethernet = [0u8; 14];
    ethernet[0..6].copy_from_slice(&work.destination_mac);
    ethernet[6..12].copy_from_slice(&work.source_mac);
    ethernet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    if packet.copy_in(0, &ethernet).is_err() {
        return Err((RawTxError::Buffer, packet));
    }
    Ok(packet)
}

#[cfg(test)]
pub fn build_header_included_ipv4_fragments(
    packet: &[u8],
    work: &PreparedRawTx,
) -> Result<Vec<Vec<u8>>, RawTxError> {
    let (IpAddr::V4(route_source), IpAddr::V4(route_destination)) =
        (work.route.source, work.destination)
    else {
        return Err(RawTxError::AddressFamily);
    };
    if packet.len() < 20 || packet.len() > u16::MAX as usize {
        return Err(RawTxError::InvalidHeader);
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if packet[0] >> 4 != 4 || !(20..=60).contains(&header_len) || packet.len() < header_len {
        return Err(RawTxError::InvalidHeader);
    }
    let flags = u16::from_be_bytes([packet[6], packet[7]]);
    if flags & 0x4000 != 0 {
        return Err(RawTxError::MtuExceeded);
    }
    let destination = Ipv4Addr(packet[16..20].try_into().unwrap());
    if destination != Ipv4Addr::UNSPECIFIED && destination != route_destination {
        return Err(RawTxError::InvalidHeader);
    }
    let fragment_payload = (work.route.mtu.saturating_sub(header_len as u32) as usize) & !7;
    if fragment_payload < 8 {
        return Err(RawTxError::MtuExceeded);
    }
    let body = &packet[header_len..];
    let mut ethernet = [0u8; 14];
    ethernet[0..6].copy_from_slice(&work.destination_mac);
    ethernet[6..12].copy_from_slice(&work.source_mac);
    ethernet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    let mut frames = Vec::new();
    for (index, chunk) in body.chunks(fragment_payload).enumerate() {
        let offset = index * fragment_payload;
        let more = offset + chunk.len() < body.len();
        let mut frame = alloc::vec![0; 14 + header_len + chunk.len()];
        frame[..14].copy_from_slice(&ethernet);
        frame[14..14 + header_len].copy_from_slice(&packet[..header_len]);
        let ip = &mut frame[14..14 + header_len];
        ip[2..4].copy_from_slice(&((header_len + chunk.len()) as u16).to_be_bytes());
        let fragment = (flags & 0x8000) | ((offset / 8) as u16) | if more { 0x2000 } else { 0 };
        ip[6..8].copy_from_slice(&fragment.to_be_bytes());
        if ip[12..16] == [0; 4] {
            ip[12..16].copy_from_slice(&route_source.0);
        }
        ip[16..20].copy_from_slice(&route_destination.0);
        ip[10..12].fill(0);
        let checksum = crate::pipeline::checksum_bytes(ip);
        ip[10..12].copy_from_slice(&checksum.to_be_bytes());
        frame[14 + header_len..].copy_from_slice(chunk);
        frames.push(frame);
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::PacketMetadata;
    use crate::pipeline::{EthernetHeader, FrontendDisposition, IpPacket, ParsedPacket};
    use crate::{Ipv4Addr, SocketId, SocketKind};

    fn facade(counter: u64, protocol: u8) -> Arc<SocketFacade> {
        Arc::new(SocketFacade::new_with_protocol(
            SocketId {
                boot_nonce: 1,
                counter,
            },
            AddressFamily::Ipv4,
            SocketKind::Raw,
            protocol,
        ))
    }

    fn packet(protocol: u8) -> FrontendPacket {
        let ip = IpPacket {
            source: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            destination: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            next_header: protocol,
            header_len: 20,
            payload_offset: 34,
            payload_len: 4,
            hop_limit: 63,
            traffic_class: 0,
            fragment: None,
        };
        let mut bytes = alloc::vec![0; 38];
        bytes[14] = 0x45;
        bytes[34..].copy_from_slice(b"raw!");
        FrontendPacket {
            chain: PacketChain::from_owned(bytes),
            metadata: PacketMetadata {
                rx_timestamp_ns: 77,
                ..PacketMetadata::default()
            },
            parsed: ParsedPacket {
                ethernet: EthernetHeader {
                    destination: [2; 6],
                    source: [1; 6],
                    ethertype: 0x0800,
                },
                ip: Some(ip),
                tcp: None,
                udp: None,
                flow: None,
                rss_hash: None,
                disposition: FrontendDisposition::Raw,
            },
        }
    }

    #[test]
    fn first_raw_receiver_gets_original_and_later_receiver_gets_copy() {
        let first = facade(1, 99);
        let second = facade(2, 99);
        let mut table = RawEndpointTable::new();
        table
            .bind_facade(IpAddr::V4(Ipv4Addr::UNSPECIFIED), None, Arc::clone(&second))
            .unwrap();
        table
            .bind_facade(IpAddr::V4(Ipv4Addr::UNSPECIFIED), None, Arc::clone(&first))
            .unwrap();
        let result = table.ingest(InterfaceId(1), packet(99));
        assert_eq!(result.delivered, 2);
        assert_eq!(result.copied_packets, 1);
        assert!(result.undelivered.is_none());
        let mut first_bytes = [0u8; 24];
        let first_rx = first
            .recv(&mut first_bytes, false, false, true, None)
            .unwrap();
        let mut second_bytes = [0u8; 24];
        let second_rx = second
            .recv(&mut second_bytes, false, false, true, None)
            .unwrap();
        assert_eq!(first_rx.len, 24);
        assert_eq!(second_rx.len, 24);
        assert_eq!(first_bytes, second_bytes);
        assert_eq!(&first_bytes[20..], b"raw!");
    }

    #[test]
    fn protocol_and_interface_scope_filter_raw_fanout() {
        let receiver = facade(1, 58);
        let mut table = RawEndpointTable::new();
        table
            .bind_facade(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                Some(InterfaceId(2)),
                receiver,
            )
            .unwrap();
        let result = table.ingest(InterfaceId(1), packet(58));
        assert_eq!(result.delivered, 0);
        assert!(result.undelivered.is_some());
    }

    #[test]
    fn header_included_df_rejects_fragmentation() {
        let mut bytes = alloc::vec![0; 1400];
        let packet_len = bytes.len() as u16;
        bytes[0] = 0x45;
        bytes[2..4].copy_from_slice(&packet_len.to_be_bytes());
        bytes[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        bytes[12..16].copy_from_slice(&[10, 0, 0, 1]);
        bytes[16..20].copy_from_slice(&[10, 0, 0, 2]);
        let facade = facade(9, 99);
        let work = PreparedRawTx {
            payload: facade.test_udp_tx_lease(
                b"x",
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
                    port: 0,
                },
            ),
            route: RouteDecision {
                interface: InterfaceId(1),
                source: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                next_hop: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
                mtu: 576,
                table: 0,
            },
            destination: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            source_mac: [2; 6],
            destination_mac: [1; 6],
            unresolved_neighbor: None,
            protocol: 99,
            header_included: true,
            hop_limit: 64,
            traffic_class: 0,
            completion: CompletionToken(1),
        };
        assert_eq!(
            build_header_included_ipv4_fragments(&bytes, &work),
            Err(RawTxError::MtuExceeded)
        );
    }
}
