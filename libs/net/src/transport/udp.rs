use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::buf::{CompletionToken, DropReason, PacketChain, PacketFragment, PacketLayout};
use crate::control::RouteDecision;
use crate::flow::{DIRTY_INGRESS, FlowKey, FlowTable, flow_hash64, rss_hash};
use crate::pipeline::FrontendPacket;
use crate::pipeline::{partial_transport_checksum, transport_checksum};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalUdpIngressError {
    NoEndpoint,
    RingFull,
    Suppressed,
    Unsupported,
}

pub(crate) fn local_udp_payload_fits_route(
    destination: IpAddr,
    payload_len: usize,
    route_mtu: u32,
) -> bool {
    let ip_header_len = match destination {
        IpAddr::V4(_) => 20usize,
        IpAddr::V6(_) => 40usize,
    };
    ip_header_len
        .checked_add(8)
        .and_then(|len| len.checked_add(payload_len))
        .is_some_and(|len| len <= route_mtu as usize)
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
    /// IP_OPTIONS：随 IPv4 头携带的选项。
    pub ip_options: crate::ip_options::IpOptions,
    pub mark: u32,
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
        mut packet: FrontendPacket,
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
        if let PacketLayout::UdpSegments(layout) = packet.metadata.layout {
            if recipients.len() != 1
                || !layout.validate(packet.chain.fragment_count(), packet.chain.total_len())
                || layout.header_len != udp.payload_offset
                || layout.payload_len != udp.payload_len
            {
                return Err(UdpIngressError {
                    reason: DropReason::MalformedUdp,
                    chain: packet.chain,
                    metadata: packet.metadata,
                    parsed: packet.parsed,
                });
            }
            for index in 0..packet.chain.fragment_count() {
                let expected = if index == 0 {
                    layout.logical_frame_len()
                } else {
                    usize::from(layout.payload_len)
                };
                if packet.chain.fragment(index).map(PacketFragment::len) != Some(expected) {
                    return Err(UdpIngressError {
                        reason: DropReason::MalformedUdp,
                        chain: packet.chain,
                        metadata: packet.metadata,
                        parsed: packet.parsed,
                    });
                }
            }
            let id = recipients[0];
            let destination = self
                .flows
                .get(id)
                .map(|endpoint| {
                    if endpoint.local.addr.is_unspecified() {
                        flow.local
                    } else {
                        endpoint.local
                    }
                })
                .expect("bind 表指向有效 UDP endpoint");
            let ip = packet.parsed.ip.expect("UDP packet 必须携带 IP sidecar");
            let segment_count = usize::from(layout.segment_count);
            let mut delivered = false;
            for index in 0..segment_count {
                let fragment = packet
                    .chain
                    .take_fragment(index)
                    .expect("已校验的 UDP segment 必须存在");
                let mut chain = PacketChain::new();
                chain.push(fragment).unwrap_or_else(|_| unreachable!());
                let datagram = UdpDatagram {
                    packet: chain,
                    source: flow.remote,
                    destination,
                    payload_offset: if index == 0 { udp.payload_offset } else { 0 },
                    payload_len: layout.payload_len,
                    hop_limit: ip.hop_limit,
                    traffic_class: ip.traffic_class,
                    ingress_interface: interface,
                    rx_timestamp_ns: packet.metadata.rx_timestamp_ns,
                };
                let endpoint = self
                    .flows
                    .get_mut(id)
                    .expect("bind 表指向有效 UDP endpoint");
                let accepted = if let Some(facade) = endpoint.facade.as_ref() {
                    facade.push_rx(datagram).is_ok()
                } else if endpoint.rx.is_full() {
                    false
                } else {
                    endpoint.rx.push(datagram);
                    true
                };
                delivered |= accepted;
            }
            if delivered {
                self.flows.mark_dirty(id, DIRTY_INGRESS);
                return Ok(id);
            }
            return Err(UdpIngressError {
                reason: DropReason::UdpRingFull,
                chain: packet.chain,
                metadata: packet.metadata,
                parsed: packet.parsed,
            });
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

    pub fn ingest_local(
        &mut self,
        interface: InterfaceId,
        source: Endpoint,
        destination: Endpoint,
        payload: &UdpTxLease,
        hop_limit: u8,
        traffic_class: u8,
        mark: u32,
        route_mtu: u32,
        now_ns: u64,
    ) -> Result<FlowId, LocalUdpIngressError> {
        if !crate::socket::local_transport_fast_path_eligible() {
            return Err(LocalUdpIngressError::Unsupported);
        }
        let fanout = destination.addr.is_multicast() || is_broadcast(destination.addr);
        if destination.addr.is_multicast() && !payload.facade().multicast_loop() {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::UdpLocalSuppressedDatagrams, 1);
            return Err(LocalUdpIngressError::Suppressed);
        }
        if !local_udp_payload_fits_route(destination.addr, usize::from(payload.len), route_mtu) {
            return Err(LocalUdpIngressError::Unsupported);
        }
        let flow = FlowKey::new(source, destination, crate::TransportProtocol::Udp)
            .ok_or(LocalUdpIngressError::Unsupported)?;
        let hash = flow_hash64(rss_hash(&self.rss_key, &flow));
        let connected = (!fanout)
            .then(|| self.flows.find(&flow, hash))
            .flatten()
            .filter(|id| {
                self.flows.get(*id).is_some_and(|endpoint| {
                    endpoint.interface.is_none_or(|scope| scope == interface)
                        && endpoint.peer.is_none_or(|peer| peer == source)
                })
            });
        let mut recipients = if let Some(id) = connected {
            alloc::vec![id]
        } else {
            self.lookup_bound(interface, destination)
        };
        recipients.retain(|id| {
            self.flows.get(*id).is_some_and(|endpoint| {
                endpoint.interface.is_none_or(|scope| scope == interface)
                    && endpoint.peer.is_none_or(|peer| peer == source)
                    && (!destination.addr.is_multicast()
                        || endpoint.facade.as_ref().is_some_and(|facade| {
                            facade.accepts_multicast(destination.addr, interface)
                        }))
            })
        });
        if !fanout && connected.is_none() && !recipients.is_empty() {
            let selected = usize::try_from(hash).unwrap_or(0) % recipients.len();
            recipients = alloc::vec![recipients[selected]];
        } else {
            recipients.truncate(16);
        }
        if recipients.is_empty() {
            return Err(LocalUdpIngressError::NoEndpoint);
        }
        if recipients.iter().any(|id| {
            self.flows
                .get(*id)
                .is_some_and(|endpoint| endpoint.facade.is_none())
        }) {
            return Err(LocalUdpIngressError::Unsupported);
        }

        let mut first_delivered = None;
        let mut ring_full = false;
        let mut unsupported = false;
        #[cfg(feature = "performance-profile")]
        let mut fanout_receivers = 0u64;
        #[cfg(feature = "performance-profile")]
        let mut fanout_drops = 0u64;
        let mut route = None;
        for id in recipients {
            let Some(endpoint) = self.flows.get(id) else {
                continue;
            };
            let facade = endpoint
                .facade
                .as_ref()
                .cloned()
                .expect("本地 fanout 已预检 facade");
            let delivered_to = if endpoint.local.addr.is_unspecified() {
                destination
            } else {
                endpoint.local
            };
            match facade.push_local_udp(
                payload,
                source,
                delivered_to,
                hop_limit,
                traffic_class,
                interface,
                now_ns,
            ) {
                Ok(()) => {
                    first_delivered.get_or_insert(id);
                    #[cfg(feature = "performance-profile")]
                    {
                        fanout_receivers += u64::from(fanout);
                    }
                    if !fanout {
                        route = Some((facade, delivered_to));
                    }
                    self.flows.mark_dirty(id, DIRTY_INGRESS);
                }
                Err(crate::SocketError::WouldBlock) => {
                    ring_full = true;
                    #[cfg(feature = "performance-profile")]
                    {
                        fanout_drops += u64::from(fanout);
                    }
                }
                Err(_) => {
                    unsupported = true;
                    #[cfg(feature = "performance-profile")]
                    {
                        fanout_drops += u64::from(fanout);
                    }
                }
            }
        }
        #[cfg(feature = "performance-profile")]
        if fanout {
            if fanout_receivers != 0 {
                profiling::observe(profiling::Metric::UdpLocalFanoutReceivers, fanout_receivers);
            }
            if fanout_drops != 0 {
                profiling::observe(profiling::Metric::UdpLocalFanoutDrops, fanout_drops);
            }
        }
        let Some(delivered) = first_delivered else {
            return Err(if ring_full {
                LocalUdpIngressError::RingFull
            } else if unsupported {
                LocalUdpIngressError::Unsupported
            } else {
                LocalUdpIngressError::NoEndpoint
            });
        };
        if let Some((facade, delivered_to)) = route {
            payload.facade().remember_local_datagram_route(
                facade,
                destination,
                source,
                delivered_to,
                interface,
                payload.dont_route,
                payload.confirm,
                mark,
                hop_limit,
                traffic_class,
                route_mtu,
            );
        }
        Ok(delivered)
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
        payload: &UdpTxLease,
        hop_limit: u8,
        traffic_class: u8,
        now_ns: u64,
    ) -> usize {
        if !destination.addr.is_multicast() {
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
            if endpoint.facade.as_ref().is_some_and(|facade| {
                facade
                    .push_local_udp(
                        payload,
                        source,
                        destination,
                        hop_limit,
                        traffic_class,
                        interface,
                        now_ns,
                    )
                    .is_ok()
            }) {
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
        false,
        crate::ip_options::IpOptions::empty(),
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
    checksum_offload: bool,
    ip_options: crate::ip_options::IpOptions,
) -> Result<PacketChain, (UdpTxError, PacketChain)> {
    let payload_len = payload.total_len();
    let ip_opt_len = ip_options.wire_len();
    let (header_len, protocol_header_len) = match (route.source, destination.addr) {
        (IpAddr::V4(_), IpAddr::V4(_)) => (42usize + ip_opt_len, 28usize + ip_opt_len),
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
            let mut header = alloc::vec![0u8; 28 + ip_opt_len];
            header[0] = 0x45 | ((ip_opt_len / 4) as u8) << 4;
            header[1] = traffic_class;
            header[2..4].copy_from_slice(&((payload_len + 28 + ip_opt_len) as u16).to_be_bytes());
            header[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            header[8] = hop_limit;
            header[9] = IP_PROTOCOL_UDP;
            header[12..16].copy_from_slice(&source.0);
            header[16..20].copy_from_slice(&destination_address.0);
            header[20..20 + ip_opt_len].copy_from_slice(ip_options.wire_slice());
            let checksum = crate::pipeline::checksum_bytes(&header[..20 + ip_opt_len]);
            header[10..12].copy_from_slice(&checksum.to_be_bytes());
            write_udp_header(
                &mut header[20 + ip_opt_len..28 + ip_opt_len],
                source_port,
                destination.port,
                udp_len,
            );
            if payload.copy_in(0, &ethernet).is_err() || payload.copy_in(14, &header).is_err() {
                return Err((UdpTxError::Buffer, payload));
            }
            let checksum = if checksum_offload {
                let Ok(checksum) = partial_transport_checksum(
                    route.source,
                    destination.addr,
                    usize::from(udp_len),
                    IP_PROTOCOL_UDP,
                ) else {
                    return Err((UdpTxError::Buffer, payload));
                };
                checksum
            } else {
                let Ok(checksum) = transport_checksum(
                    &payload,
                    34 + ip_opt_len,
                    usize::from(udp_len),
                    route.source,
                    destination.addr,
                    IP_PROTOCOL_UDP,
                ) else {
                    return Err((UdpTxError::Buffer, payload));
                };
                checksum
            };
            let checksum = if checksum == 0 { 0xffff } else { checksum };
            if payload
                .copy_in(40 + ip_opt_len, &checksum.to_be_bytes())
                .is_err()
            {
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
            let checksum = if checksum_offload {
                let Ok(checksum) = partial_transport_checksum(
                    route.source,
                    destination.addr,
                    usize::from(udp_len),
                    IP_PROTOCOL_UDP,
                ) else {
                    return Err((UdpTxError::Buffer, payload));
                };
                checksum
            } else {
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
                checksum
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
#[cfg(test)]
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
    use crate::buf::NetBufPool;
    use crate::pipeline::{EthernetHeader, FrontendDisposition, IpPacket, ParsedPacket, UdpPacket};
    use crate::{
        AddressFamily, Ipv4Addr, MulticastMembership, SocketFacade, SocketId, SocketKind,
        TransportProtocol,
    };
    use spin::Mutex;

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

    fn segmented_packet(remote_port: u16, local_port: u16) -> FrontendPacket {
        let mut packet = parsed_packet(remote_port, local_port);
        let payloads = [[1u8, 2, 3, 4], [5, 6, 7, 8], [9, 10, 11, 12]];
        let mut first = alloc::vec![0; 42 + payloads[0].len()];
        first[42..].copy_from_slice(&payloads[0]);
        packet
            .chain
            .push(PacketFragment::Owned(first.into_boxed_slice()))
            .unwrap_or_else(|_| unreachable!());
        for payload in payloads.iter().skip(1) {
            packet
                .chain
                .push(PacketFragment::Owned(payload.to_vec().into_boxed_slice()))
                .unwrap_or_else(|_| unreachable!());
        }
        packet.parsed.ip.as_mut().unwrap().payload_len = 12;
        packet.parsed.udp.as_mut().unwrap().payload_len = 4;
        packet.metadata.layout = PacketLayout::UdpSegments(crate::buf::UdpSegmentation {
            segment_count: 3,
            header_len: 42,
            payload_len: 4,
        });
        packet
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
    fn local_udp_delivery_copies_payload_and_reclaims_sender_slot() {
        let mut table = UdpEndpointTable::new([11; 40]);
        let receiver = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 2,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let source = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40000,
        };
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let flow = table
            .bind_facade(
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: destination.port,
                },
                None,
                Some(InterfaceId(1)),
                Arc::clone(&receiver),
            )
            .unwrap();
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 2,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let payload = b"local datagram";
        let lease = sender.test_udp_tx_lease(payload, destination);
        assert_eq!(
            table.ingest_local(
                InterfaceId(1),
                source,
                destination,
                &lease,
                64,
                0,
                0,
                65_535,
                123,
            ),
            Ok(flow)
        );
        lease.complete();

        let mut output = [0u8; 32];
        let received = receiver
            .recv(&mut output, false, false, true, None)
            .unwrap();
        assert_eq!(received.len, payload.len());
        assert_eq!(&output[..received.len], payload);
        assert_eq!(received.source, source);
        assert_eq!(received.destination, destination);
        assert!(sender.readiness().0.contains(crate::Readiness::WRITABLE));
        assert_eq!(sender.test_udp_tx_used_bytes(), 0);
    }

    #[test]
    fn route_mtu_rejects_local_delivery_before_receiver_publication() {
        let mut table = UdpEndpointTable::new([31; 40]);
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9007,
        };
        let receiver = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 8,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        table
            .bind_facade(
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: destination.port,
                },
                None,
                None,
                Arc::clone(&receiver),
            )
            .unwrap();
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 8,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let payload = sender.test_udp_tx_lease(&[0x41; 64], destination);

        assert_eq!(
            table.ingest_local(
                InterfaceId(1),
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 40005,
                },
                destination,
                &payload,
                64,
                0,
                0,
                64,
                124,
            ),
            Err(LocalUdpIngressError::Unsupported)
        );
        assert!(!receiver.readiness().0.contains(crate::Readiness::READABLE));
        let mut retained = [0; 64];
        payload.copy_range(0, &mut retained).unwrap();
        assert_eq!(retained, [0x41; 64]);
        payload.complete();
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
    fn segmented_udp_restores_ordered_datagram_boundaries() {
        let mut table = UdpEndpointTable::new([9; 40]);
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
                .ingest(InterfaceId(1), segmented_packet(1000, 9000))
                .is_ok()
        );
        for expected in [[1u8, 2, 3, 4], [5, 6, 7, 8], [9, 10, 11, 12]] {
            let datagram = table.recv(id).expect("每个 segment 恢复一个 datagram");
            let mut payload = [0u8; 4];
            datagram
                .packet
                .copy_out(usize::from(datagram.payload_offset), &mut payload)
                .unwrap();
            assert_eq!(payload, expected);
            assert_eq!(datagram.payload_len, 4);
        }
        assert!(table.recv(id).is_none());
    }

    #[test]
    fn segmented_udp_rejects_mismatched_fragment_lengths() {
        let mut table = UdpEndpointTable::new([10; 40]);
        table
            .bind(
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 9000,
                },
                None,
                None,
            )
            .unwrap();
        let mut packet = segmented_packet(1000, 9000);
        packet.metadata.layout = PacketLayout::UdpSegments(crate::buf::UdpSegmentation {
            segment_count: 3,
            header_len: 42,
            payload_len: 5,
        });
        assert_eq!(
            table.ingest(InterfaceId(1), packet).unwrap_err().reason,
            DropReason::MalformedUdp
        );
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
    fn local_multicast_fanout_pins_one_reference_for_each_receiver() {
        let mut table = UdpEndpointTable::new([13; 40]);
        let group = IpAddr::V4(Ipv4Addr::new(239, 9, 8, 7));
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 9010,
        };
        let first = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 3,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let second = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 3,
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

        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(1, crate::tuning::RX_PAGE_SIZE).unwrap(),
        ));
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 3,
                counter: 3,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        sender.install_datagram_tx_pool(Arc::clone(&pool));
        let source = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40000,
        };
        let destination = Endpoint {
            addr: group,
            port: local.port,
        };
        let bytes = alloc::vec![0x45; 1024];
        let payload = sender.test_udp_tx_lease(&bytes, destination);
        assert!(
            table
                .ingest_local(
                    InterfaceId(1),
                    source,
                    destination,
                    &payload,
                    1,
                    0,
                    0,
                    65_535,
                    10,
                )
                .is_ok()
        );
        payload.complete();
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 0);

        for receiver in [&first, &second] {
            let mut output = alloc::vec![0; bytes.len()];
            let received = receiver
                .recv_local_datagram_from(output.len(), output.len(), false, |offset, input| {
                    output[offset..offset + input.len()].copy_from_slice(input);
                    Ok::<(), u8>(())
                })
                .ok()
                .flatten()
                .expect("每个成员都必须收到一份完整共享数据报");
            assert_eq!(received.len, bytes.len());
            assert_eq!(output, bytes);
        }
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 1);
    }

    #[test]
    fn local_multicast_loop_disable_suppresses_local_delivery() {
        let mut table = UdpEndpointTable::new([17; 40]);
        let group = IpAddr::V4(Ipv4Addr::new(239, 7, 6, 5));
        let receiver = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 4,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        receiver
            .add_multicast_membership(MulticastMembership {
                group,
                interface: Some(InterfaceId(1)),
            })
            .unwrap();
        table
            .bind_facade(
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 9011,
                },
                None,
                None,
                Arc::clone(&receiver),
            )
            .unwrap();
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 4,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        sender.set_multicast_loop(false);
        let destination = Endpoint {
            addr: group,
            port: 9011,
        };
        let payload = sender.test_udp_tx_lease(&[1, 2, 3], destination);
        assert_eq!(
            table.ingest_local(
                InterfaceId(1),
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 40001,
                },
                destination,
                &payload,
                1,
                0,
                0,
                65_535,
                10,
            ),
            Err(LocalUdpIngressError::Suppressed)
        );
        payload.complete();
        assert!(!receiver.readiness().0.contains(crate::Readiness::READABLE));
    }

    #[test]
    fn local_broadcast_fans_out_but_reuse_unicast_selects_one_receiver() {
        let mut table = UdpEndpointTable::new([19; 40]);
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 9012,
        };
        let first = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 5,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let second = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 5,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        table
            .bind_facade(local, None, None, Arc::clone(&first))
            .unwrap();
        table
            .bind_facade(local, None, None, Arc::clone(&second))
            .unwrap();
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 5,
                counter: 3,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let source = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40002,
        };

        let broadcast = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::BROADCAST),
            port: local.port,
        };
        let payload = sender.test_udp_tx_lease(&[0x31], broadcast);
        assert!(
            table
                .ingest_local(
                    InterfaceId(1),
                    source,
                    broadcast,
                    &payload,
                    1,
                    2,
                    0,
                    65_535,
                    20,
                )
                .is_ok()
        );
        payload.complete();
        assert!(first.readiness().0.contains(crate::Readiness::READABLE));
        assert!(second.readiness().0.contains(crate::Readiness::READABLE));
        let mut byte = [0];
        first.recv(&mut byte, false, false, true, None).unwrap();
        second.recv(&mut byte, false, false, true, None).unwrap();

        let unicast = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: local.port,
        };
        let payload = sender.test_udp_tx_lease(&[0x52], unicast);
        assert!(
            table
                .ingest_local(
                    InterfaceId(1),
                    source,
                    unicast,
                    &payload,
                    64,
                    3,
                    0,
                    65_535,
                    21,
                )
                .is_ok()
        );
        payload.complete();
        let readable = usize::from(first.readiness().0.contains(crate::Readiness::READABLE))
            + usize::from(second.readiness().0.contains(crate::Readiness::READABLE));
        assert_eq!(readable, 1);
    }

    #[test]
    fn unsupported_fanout_receiver_prevents_partial_fast_path_delivery() {
        let mut table = UdpEndpointTable::new([23; 40]);
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 9013,
        };
        let receiver = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 6,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        table
            .bind_facade(local, None, None, Arc::clone(&receiver))
            .unwrap();
        table.bind(local, None, None).unwrap();
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 6,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::BROADCAST),
            port: local.port,
        };
        let payload = sender.test_udp_tx_lease(&[0x61, 0x62], destination);

        assert_eq!(
            table.ingest_local(
                InterfaceId(1),
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 40003,
                },
                destination,
                &payload,
                64,
                0,
                0,
                65_535,
                22,
            ),
            Err(LocalUdpIngressError::Unsupported)
        );
        assert!(!receiver.readiness().0.contains(crate::Readiness::READABLE));
        payload.complete();
    }

    #[test]
    fn full_local_receiver_records_drop_without_consuming_sender_ownership() {
        let mut table = UdpEndpointTable::new([29; 40]);
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9014,
        };
        let receiver = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        receiver.set_buffer_limits(None, Some(16 * 1024));
        table
            .bind_facade(
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: destination.port,
                },
                None,
                None,
                Arc::clone(&receiver),
            )
            .unwrap();
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let bytes = [0x71; 4096];
        for timestamp in 19..23 {
            let payload = sender.test_udp_tx_lease(&bytes, destination);
            assert!(
                table
                    .ingest_local(
                        InterfaceId(1),
                        Endpoint {
                            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                            port: 40004,
                        },
                        destination,
                        &payload,
                        64,
                        0,
                        0,
                        65_535,
                        timestamp,
                    )
                    .is_ok()
            );
            payload.complete();
        }
        let payload = sender.test_udp_tx_lease(&bytes, destination);

        assert_eq!(
            table.ingest_local(
                InterfaceId(1),
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 40004,
                },
                destination,
                &payload,
                64,
                0,
                0,
                65_535,
                23,
            ),
            Err(LocalUdpIngressError::RingFull)
        );
        assert_eq!(receiver.take_rx_overflow(), 1);
        let mut retained = [0; 32];
        payload.copy_range(0, &mut retained).unwrap();
        assert_eq!(retained, bytes[..retained.len()]);
        payload.complete();
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
    #[test]
    fn reuse_port_group_distributes_unicast_by_hash() {
        let mut table = UdpEndpointTable::new([23; 40]);
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 9090,
        };
        let first = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let second = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        let first_id = table
            .bind_facade(local, None, None, Arc::clone(&first))
            .unwrap();
        let second_id = table
            .bind_facade(local, None, None, Arc::clone(&second))
            .unwrap();

        // 多个不同源端点的单播包：hash 分发应把包分散到两个接收端，
        // 且每个包只投递到一个接收端（Linux reuse_port 语义）。
        let mut first_count = 0usize;
        let mut second_count = 0usize;
        for source_port in 41000..41032u16 {
            let sender = Arc::new(SocketFacade::new(
                SocketId {
                    boot_nonce: 7,
                    counter: u64::from(source_port),
                },
                AddressFamily::Ipv4,
                SocketKind::Datagram,
            ));
            let source = Endpoint {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: source_port,
            };
            let payload = sender.test_udp_tx_lease(&[0x55], local);
            let flow = table
                .ingest_local(InterfaceId(1), source, local, &payload, 1, 2, 0, 65_535, 20)
                .expect("单播包应被分发");
            payload.complete();
            if flow == first_id {
                first_count += 1;
            } else if flow == second_id {
                second_count += 1;
            }
        }
        // 32 个不同四元组应分布到两个 socket（各自至少 1 个，总和 32）。
        assert!(first_count >= 1, "第一个 reuse_port socket 未收到任何包");
        assert!(second_count >= 1, "第二个 reuse_port socket 未收到任何包");
        assert_eq!(first_count + second_count, 32);
    }
}
