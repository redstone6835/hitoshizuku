use crate::buf::{
    CompletionToken, DropReason, PacketBatch, PacketChain, PacketMetadata, TxBatch, TxPacket,
};
use crate::control::{ConfigError, ConfigSnapshot, NeighborKey, NeighborTable, PmtuCache, PmtuKey};
use crate::flow::FlowKey;
use crate::pipeline::{FrontendBatch, FrontendDisposition, FrontendPacket, VectorFrontend};
use crate::transport::{
    ControlErrorTarget, ControlPacketResult, LocalUdpIngressError, PreparedRawTx, PreparedTcpTx,
    PreparedUdpTx, RawBindError, RawEndpointTable, TcpBindError, TcpEndpointTable, TcpIngressError,
    TcpPacket, TcpPath, UdpBindError, UdpDatagram, UdpEndpointTable, UdpTxError,
    build_port_unreachable, build_tcp_reset, build_udp_packet, handle_control_packet,
};
use crate::{Endpoint, FlowId, InterfaceId, IpAddr, ListenGroup, ListenGroupId, ShardId};
use crate::{OwnerRef, SocketError, SocketFacade, TcpTxLease, UdpTxLease};
use alloc::collections::VecDeque;
use alloc::sync::Arc;

use super::TimerWheel;
use super::reassembly::{ReassemblyResult, ReassemblyTable};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowShardStats {
    pub tcp_delivered: u64,
    pub udp_delivered: u64,
    pub control_packets: u64,
    pub tx_formed: u64,
    pub dirty_runs: u64,
    pub timer_expired: u64,
}

#[derive(Clone, Copy)]
pub struct FlowTurnContext<'a> {
    pub interface: InterfaceId,
    pub local_mac: [u8; 6],
    pub config: &'a ConfigSnapshot,
    pub now_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpSendError {
    NoEndpoint,
    DestinationRequired,
    Route(ConfigError),
    NeighborUnresolved(NeighborKey),
    Packet(UdpTxError),
}

pub struct UdpSendFailure {
    pub error: UdpSendError,
    pub payload: PacketChain,
}

pub struct FlowShard {
    id: ShardId,
    frontend: VectorFrontend,
    frontend_batch: FrontendBatch,
    udp: UdpEndpointTable,
    tcp: TcpEndpointTable,
    raw: RawEndpointTable,
    neighbors: NeighborTable,
    pmtu: PmtuCache,
    reassembly: ReassemblyTable,
    reassembled_input: PacketBatch,
    reassembled: VecDeque<FrontendPacket>,
    forwarded_errors: VecDeque<(
        InterfaceId,
        ControlErrorTarget,
        crate::transport::TransportControlError,
        u64,
    )>,
    timers: TimerWheel,
    next_completion: u64,
    stats: FlowShardStats,
    icmp_error_tokens: u32,
    icmp_error_refill_ns: u64,
}

impl FlowShard {
    pub fn new(
        id: ShardId,
        rss_key: [u8; 40],
        rss_generation: u32,
        hash_seed: [u8; 16],
        tcp_isn_key: [u8; 16],
        now_ns: u64,
    ) -> Self {
        Self {
            id,
            frontend: VectorFrontend::new(rss_key, rss_generation),
            frontend_batch: FrontendBatch::new(),
            udp: UdpEndpointTable::new(rss_key),
            tcp: TcpEndpointTable::new_on_shard(id, rss_key, tcp_isn_key),
            raw: RawEndpointTable::new(),
            neighbors: NeighborTable::new(hash_seed),
            pmtu: PmtuCache::new(),
            reassembly: ReassemblyTable::new(),
            reassembled_input: PacketBatch::new(),
            reassembled: VecDeque::new(),
            forwarded_errors: VecDeque::new(),
            timers: TimerWheel::new(8192, now_ns / 1_000_000),
            next_completion: 1,
            stats: FlowShardStats::default(),
            icmp_error_tokens: 100,
            icmp_error_refill_ns: now_ns,
        }
    }

    pub const fn id(&self) -> ShardId {
        self.id
    }

    pub const fn stats(&self) -> FlowShardStats {
        self.stats
    }

    pub fn bind_udp(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
    ) -> Result<FlowId, UdpBindError> {
        self.udp.bind(local, peer, interface)
    }

    pub fn bind_udp_facade(
        &mut self,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
        free_bind: bool,
        accepts_ipv4: bool,
    ) -> Result<FlowId, UdpBindError> {
        self.udp
            .bind_facade_with_options(local, peer, interface, facade, free_bind, accepts_ipv4)
    }

    pub fn close_udp(&mut self, flow: FlowId) {
        if let Some(facade) = self.udp.unbind(flow) {
            facade.publish_closed();
        }
    }

    pub fn bind_raw_facade(
        &mut self,
        local: IpAddr,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
        free_bind: bool,
    ) -> Result<FlowId, RawBindError> {
        self.raw
            .bind_facade_with_options(local, interface, facade, free_bind)
    }

    pub fn close_raw(&mut self, flow: FlowId) {
        if let Some(facade) = self.raw.close(flow) {
            facade.publish_closed();
        }
    }

    pub fn listen_tcp(
        &mut self,
        local: Endpoint,
        interface: Option<InterfaceId>,
        group: Arc<ListenGroup>,
    ) -> Result<(), TcpBindError> {
        self.tcp.listen(local, interface, group)
    }

    pub fn connect_tcp(
        &mut self,
        local: Endpoint,
        remote: Endpoint,
        path: TcpPath,
        facade: Arc<SocketFacade>,
        control_sequence: u64,
        now_ns: u64,
    ) -> Result<FlowId, TcpBindError> {
        let id = self
            .tcp
            .connect(local, remote, path, facade, control_sequence, now_ns)?;
        self.reschedule_tcp(id);
        Ok(id)
    }

    pub fn close_tcp(&mut self, flow: FlowId, now_ns: u64) {
        if self.tcp.close_flow(flow, now_ns) {
            self.reschedule_tcp(flow);
        }
    }

    pub fn abort_tcp(&mut self, flow: FlowId, now_ns: u64) {
        if self.tcp.abort_flow(flow, now_ns) {
            self.reschedule_tcp(flow);
        }
    }

    pub fn shutdown_tcp_write(&mut self, flow: FlowId, now_ns: u64) {
        if self.tcp.shutdown_write(flow, now_ns) {
            self.reschedule_tcp(flow);
        }
    }

    pub fn close_tcp_listener(&mut self, group: ListenGroupId) -> bool {
        self.tcp.close_listener(group)
    }

    pub fn drain_tcp_send(&mut self, flow: FlowId, now_ns: u64) {
        self.tcp.drain_send(flow, now_ns);
        self.reschedule_tcp(flow);
    }

    pub fn take_tcp_output(&mut self) -> Option<PreparedTcpTx> {
        self.tcp.take_output()
    }

    pub fn process_local_tcp(
        &mut self,
        interface: InterfaceId,
        path: TcpPath,
        key: FlowKey,
        packet: TcpPacket,
        payload: Option<&TcpTxLease>,
        now_ns: u64,
    ) -> Result<FlowId, TcpIngressError> {
        let flow = self
            .tcp
            .ingest_local(interface, path, key, packet, payload, now_ns)?;
        self.stats.tcp_delivered = self.stats.tcp_delivered.saturating_add(1);
        self.reschedule_tcp(flow);
        Ok(flow)
    }

    pub fn process_local_udp(
        &mut self,
        interface: InterfaceId,
        source: Endpoint,
        destination: Endpoint,
        payload: &UdpTxLease,
        hop_limit: u8,
        traffic_class: u8,
        now_ns: u64,
    ) -> Result<FlowId, LocalUdpIngressError> {
        let flow = self.udp.ingest_local(
            interface,
            source,
            destination,
            payload,
            hop_limit,
            traffic_class,
            now_ns,
        )?;
        self.stats.udp_delivered = self.stats.udp_delivered.saturating_add(1);
        Ok(flow)
    }

    pub fn resume_tcp_output(&mut self, now_ns: u64, budget: usize) -> usize {
        self.tcp.resume_output_blocked(now_ns, budget)
    }

    pub fn has_blocked_tcp_output(&self) -> bool {
        self.tcp.has_output_blocked()
    }

    pub fn next_timer_deadline_ns(&self) -> Option<u64> {
        self.timers.next_deadline_ns()
    }

    pub fn resolve_tcp_path(
        &mut self,
        destination: IpAddr,
        bound_source: Option<IpAddr>,
        interface: Option<InterfaceId>,
        config: &ConfigSnapshot,
        now_ns: u64,
        free_bind: bool,
    ) -> Result<TcpPath, SocketError> {
        let route = config
            .route_with_source_policy(destination, 0, bound_source, interface, free_bind)
            .map_err(|_| SocketError::NetworkUnreachable)?;
        let interface = config
            .interfaces
            .iter()
            .find(|candidate| candidate.id == route.interface)
            .ok_or(SocketError::NetworkUnreachable)?;
        let (destination_mac, unresolved_neighbor) = if interface.loopback {
            (interface.mac_address, None)
        } else {
            let key = NeighborKey {
                interface: route.interface,
                address: route.next_hop,
            };
            self.neighbors
                .lookup(key, now_ns)
                .map(|entry| (entry.0, None))
                .unwrap_or(([0; 6], Some(key)))
        };
        Ok(TcpPath {
            route,
            source_mac: interface.mac_address,
            destination_mac,
            unresolved_neighbor,
            config_generation: config.generation,
        })
    }

    pub fn run_due_timers(&mut self, now_ns: u64) {
        self.run_timers(now_ns);
    }

    pub fn refresh_tcp_tx_path(
        &mut self,
        work: &mut PreparedTcpTx,
        config: &ConfigSnapshot,
        now_ns: u64,
    ) -> Result<(), SocketError> {
        if work.path.config_generation == config.generation {
            return Ok(());
        }
        let mut route = config
            .route_with_source_policy(
                work.remote.addr,
                work.facade.socket_mark(),
                Some(work.path.route.source),
                work.facade.interface(),
                work.facade.free_bind(),
            )
            .map_err(|_| SocketError::NetworkUnreachable)?;
        route.mtu = self.pmtu.effective_mtu(
            PmtuKey {
                interface: route.interface,
                destination: work.remote.addr,
            },
            route.mtu,
            now_ns,
        );
        let interface = config
            .interfaces
            .iter()
            .find(|interface| interface.id == route.interface)
            .ok_or(SocketError::NetworkUnreachable)?;
        let (destination_mac, unresolved_neighbor) = if interface.loopback {
            (interface.mac_address, None)
        } else {
            let key = NeighborKey {
                interface: route.interface,
                address: route.next_hop,
            };
            self.neighbors
                .lookup(key, now_ns)
                .map(|entry| (entry.0, None))
                .unwrap_or(([0; 6], Some(key)))
        };
        work.path = TcpPath {
            route,
            source_mac: interface.mac_address,
            destination_mac,
            unresolved_neighbor,
            config_generation: config.generation,
        };
        Ok(())
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        self.neighbors.invalidate_interface(interface)
            + self.pmtu.invalidate_interface(interface)
            + self.reassembly.invalidate_interface(interface)
            + self.udp.invalidate_interface(interface)
            + self.raw.invalidate_interface(interface)
            + self.tcp.invalidate_interface(interface)
    }

    pub fn observe_neighbor(
        &mut self,
        key: NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
    ) -> bool {
        self.neighbors.observe(key, mac_address, now_ns).is_ok()
    }

    pub fn lookup_neighbor(&self, key: NeighborKey, now_ns: u64) -> Option<[u8; 6]> {
        self.neighbors.lookup(key, now_ns).map(|entry| entry.0)
    }

    pub fn reconnect_udp_facade(
        &mut self,
        flow: FlowId,
        local: Endpoint,
        peer: Endpoint,
        facade: Arc<SocketFacade>,
    ) -> Result<FlowId, UdpBindError> {
        let endpoint = self
            .udp
            .endpoint_info(flow)
            .ok_or(UdpBindError::InvalidEndpoint)?;
        let _ = self.udp.unbind(flow);
        match self.udp.bind_facade_with_options(
            local,
            Some(peer),
            endpoint.interface,
            Arc::clone(&facade),
            endpoint.free_bind,
            endpoint.accepts_ipv4,
        ) {
            Ok(flow) => Ok(flow),
            Err(error) => {
                self.udp
                    .bind_facade_with_options(
                        endpoint.local,
                        endpoint.peer,
                        endpoint.interface,
                        facade,
                        endpoint.free_bind,
                        endpoint.accepts_ipv4,
                    )
                    .unwrap_or_else(|_| panic!("UDP connect 回滚失败"));
                Err(error)
            }
        }
    }

    pub fn recv_udp(&mut self, flow: FlowId) -> Option<UdpDatagram> {
        self.udp.recv(flow)
    }

    pub fn take_udp_error(
        &mut self,
        flow: FlowId,
    ) -> Option<crate::transport::TransportControlError> {
        self.udp.take_control_error(flow)
    }

    pub fn form_udp_packet(
        &mut self,
        flow: FlowId,
        destination: Option<Endpoint>,
        payload: PacketChain,
        mark: u32,
        config: &ConfigSnapshot,
        now_ns: u64,
    ) -> Result<TxPacket, UdpSendFailure> {
        let Some(endpoint) = self.udp.endpoint_info(flow) else {
            return Err(UdpSendFailure {
                error: UdpSendError::NoEndpoint,
                payload,
            });
        };
        let Some(destination) = destination.or(endpoint.peer) else {
            return Err(UdpSendFailure {
                error: UdpSendError::DestinationRequired,
                payload,
            });
        };
        if destination.port == 0 {
            return Err(UdpSendFailure {
                error: UdpSendError::DestinationRequired,
                payload,
            });
        }
        let bound_source = (!endpoint.local.addr.is_unspecified()).then_some(endpoint.local.addr);
        let route = match config.route_with_source_policy(
            destination.addr,
            mark,
            bound_source,
            endpoint.interface,
            endpoint.free_bind,
        ) {
            Ok(route) => route,
            Err(error) => {
                return Err(UdpSendFailure {
                    error: UdpSendError::Route(error),
                    payload,
                });
            }
        };
        let interface = config
            .interfaces
            .iter()
            .find(|interface| interface.id == route.interface)
            .expect("route 指向已验证的接口");
        let destination_mac = if destination.addr.is_multicast() {
            multicast_mac(destination.addr)
        } else if interface.loopback {
            interface.mac_address
        } else {
            let key = NeighborKey {
                interface: route.interface,
                address: route.next_hop,
            };
            let Some((mac_address, _, _)) = self.neighbors.lookup(key, now_ns) else {
                return Err(UdpSendFailure {
                    error: UdpSendError::NeighborUnresolved(key),
                    payload,
                });
            };
            mac_address
        };
        let packet = match build_udp_packet(
            payload,
            route,
            destination,
            endpoint.local.port,
            interface.mac_address,
            destination_mac,
        ) {
            Ok(packet) => packet,
            Err((error, payload)) => {
                return Err(UdpSendFailure {
                    error: UdpSendError::Packet(error),
                    payload,
                });
            }
        };
        let completion = CompletionToken(self.next_completion);
        self.next_completion = self.next_completion.wrapping_add(1).max(1);
        Ok(TxPacket {
            chain: packet,
            completion,
            low_latency: false,
            checksums_validated: true,
            layout: crate::buf::PacketLayout::Plain,
        })
    }

    #[cfg(test)]
    pub fn process_rx(
        &mut self,
        context: FlowTurnContext<'_>,
        input: &mut PacketBatch,
        tx: &mut TxBatch,
        recycle: &mut PacketBatch,
        record_drop: impl FnMut(DropReason),
    ) {
        self.frontend.process(
            context.interface,
            context.config,
            input,
            &mut self.frontend_batch,
        );
        self.process_frontend_batch(context, tx, recycle, record_drop);
    }

    pub fn process_frontend_batch(
        &mut self,
        context: FlowTurnContext<'_>,
        tx: &mut TxBatch,
        recycle: &mut PacketBatch,
        mut record_drop: impl FnMut(DropReason),
    ) {
        self.process_frontend_batch_inner(context, tx, recycle, &mut record_drop);
    }

    fn process_frontend_batch_inner(
        &mut self,
        context: FlowTurnContext<'_>,
        tx: &mut TxBatch,
        recycle: &mut PacketBatch,
        record_drop: &mut dyn FnMut(DropReason),
    ) {
        for _ in 0..self.reassembly.expire(context.now_ns) {
            record_drop(DropReason::FragmentTimeout);
        }
        assert!(
            self.reassembled_input.is_empty(),
            "上一次重组 batch 必须先由常驻 worker 完成 ELM 解析或回收"
        );
        let len = self.frontend_batch.len();
        for index in 0..len {
            let Some(packet) = self.frontend_batch.take(index) else {
                continue;
            };
            match packet.parsed.disposition {
                FrontendDisposition::Tcp => {
                    let ip = packet.parsed.ip.expect("TCP packet 必须携带 IP sidecar");
                    let tcp = packet.parsed.tcp.expect("TCP packet 必须携带 TCP sidecar");
                    let ethernet = packet.parsed.ethernet;
                    let interface_snapshot = context
                        .config
                        .interfaces
                        .iter()
                        .find(|candidate| candidate.id == context.interface)
                        .expect("ingress interface 必须存在于配置快照");
                    let path = TcpPath {
                        route: crate::control::RouteDecision {
                            interface: context.interface,
                            source: ip.destination,
                            next_hop: ip.source,
                            mtu: interface_snapshot.mtu,
                            table: 0,
                        },
                        source_mac: packet.parsed.ethernet.destination,
                        destination_mac: packet.parsed.ethernet.source,
                        unresolved_neighbor: None,
                        config_generation: context.config.generation,
                    };
                    match self
                        .tcp
                        .ingest(context.interface, path, packet, context.now_ns)
                    {
                        Ok((flow, chain, metadata)) => {
                            self.stats.tcp_delivered = self.stats.tcp_delivered.saturating_add(1);
                            recycle_packet(recycle, chain, metadata);
                            self.reschedule_tcp(flow);
                        }
                        Err((error, chain, mut metadata)) => {
                            let reason = match error {
                                TcpIngressError::NoEndpoint => DropReason::TcpNoEndpoint,
                                TcpIngressError::ReceiveBufferFull => DropReason::TcpRingFull,
                                TcpIngressError::Malformed => DropReason::MalformedTcp,
                                TcpIngressError::FlowTableFull => DropReason::FlowTableFull,
                            };
                            metadata.drop_reason = reason;
                            record_drop(reason);
                            if error == TcpIngressError::NoEndpoint
                                && !tcp.flags.contains(crate::transport::TcpFlags::RST)
                            {
                                match build_tcp_reset(chain, ethernet, ip, tcp) {
                                    Ok(chain) => {
                                        let packet = TxPacket {
                                            chain,
                                            completion: CompletionToken(self.next_completion),
                                            low_latency: true,
                                            checksums_validated: true,
                                            layout: crate::buf::PacketLayout::Plain,
                                        };
                                        self.next_completion =
                                            self.next_completion.wrapping_add(1).max(1);
                                        if let Err(packet) = tx.push(packet) {
                                            recycle_packet(recycle, packet.chain, metadata);
                                        } else {
                                            self.stats.tx_formed =
                                                self.stats.tx_formed.saturating_add(1);
                                        }
                                    }
                                    Err(chain) => recycle_packet(recycle, chain, metadata),
                                }
                            } else {
                                recycle_packet(recycle, chain, metadata);
                            }
                        }
                    }
                }
                FrontendDisposition::Udp => match self.udp.ingest(context.interface, packet) {
                    Ok(_) => self.stats.udp_delivered = self.stats.udp_delivered.saturating_add(1),
                    Err(error) => {
                        let reply = (error.reason == DropReason::UdpNoEndpoint
                            && self.allow_icmp_error(context.now_ns))
                        .then(|| build_port_unreachable(&error.chain, error.parsed))
                        .flatten();
                        let mut metadata = error.metadata;
                        metadata.drop_reason = error.reason;
                        record_drop(error.reason);
                        recycle_packet(recycle, error.chain, metadata);
                        if let Some(chain) = reply {
                            let packet = TxPacket {
                                chain,
                                completion: CompletionToken(self.next_completion),
                                low_latency: true,
                                checksums_validated: true,
                                layout: crate::buf::PacketLayout::Plain,
                            };
                            self.next_completion = self.next_completion.wrapping_add(1).max(1);
                            if let Err(packet) = tx.push(packet) {
                                recycle_packet(recycle, packet.chain, PacketMetadata::default());
                            } else {
                                self.stats.tx_formed = self.stats.tx_formed.saturating_add(1);
                            }
                        }
                    }
                },
                FrontendDisposition::Raw => {
                    let result = self.raw.ingest(context.interface, packet);
                    if let Some(packet) = result.undelivered {
                        let reason = if result.delivered == 0 {
                            DropReason::RawNoEndpoint
                        } else {
                            DropReason::RawRingFull
                        };
                        let mut metadata = packet.metadata;
                        metadata.drop_reason = reason;
                        record_drop(reason);
                        recycle_packet(recycle, packet.chain, metadata);
                    }
                }
                FrontendDisposition::Control(crate::pipeline::ControlPacket::Fragment(_)) => {
                    self.stats.control_packets = self.stats.control_packets.saturating_add(1);
                    match self
                        .reassembly
                        .ingest(context.interface, context.now_ns, packet)
                    {
                        ReassemblyResult::Pending => {}
                        ReassemblyResult::Complete(chain, metadata) => {
                            if let Err(chain) = self.reassembled_input.push(chain, metadata) {
                                let mut metadata = metadata;
                                metadata.drop_reason = DropReason::FragmentLimit;
                                record_drop(DropReason::FragmentLimit);
                                recycle_packet(recycle, chain, metadata);
                            }
                        }
                        ReassemblyResult::Drop(reason) => record_drop(reason),
                    }
                }
                FrontendDisposition::Control(_) => {
                    self.stats.control_packets = self.stats.control_packets.saturating_add(1);
                    if matches!(
                        packet.parsed.disposition,
                        FrontendDisposition::Control(
                            crate::pipeline::ControlPacket::Ipv6ParameterProblem { .. }
                        )
                    ) && !self.allow_icmp_error(context.now_ns)
                    {
                        recycle_packet(recycle, packet.chain, packet.metadata);
                        continue;
                    }
                    let _ = self.raw.copy_fanout(context.interface, &packet);
                    let packet_metadata = packet.metadata;
                    match handle_control_packet(
                        context.interface,
                        context.local_mac,
                        context.config,
                        &mut self.neighbors,
                        context.now_ns,
                        packet,
                    ) {
                        ControlPacketResult::Reply(chain) => {
                            let completion = CompletionToken(self.next_completion);
                            self.next_completion = self.next_completion.wrapping_add(1).max(1);
                            let packet = TxPacket {
                                chain,
                                completion,
                                low_latency: true,
                                checksums_validated: true,
                                layout: crate::buf::PacketLayout::Plain,
                            };
                            match tx.push(packet) {
                                Ok(()) => {
                                    self.stats.tx_formed = self.stats.tx_formed.saturating_add(1);
                                }
                                Err(packet) => {
                                    let mut metadata = packet_metadata;
                                    metadata.drop_reason = DropReason::TxQueueFull;
                                    record_drop(DropReason::TxQueueFull);
                                    recycle_packet(recycle, packet.chain, metadata);
                                }
                            }
                        }
                        ControlPacketResult::Consumed(chain) => {
                            recycle_packet(recycle, chain, packet_metadata);
                        }
                        ControlPacketResult::Drop(reason, chain) => {
                            let mut metadata = packet_metadata;
                            metadata.drop_reason = reason;
                            record_drop(reason);
                            recycle_packet(recycle, chain, metadata);
                        }
                        ControlPacketResult::TransportError {
                            target,
                            error,
                            packet: chain,
                        } => {
                            let (destination, delivered) = match target {
                                ControlErrorTarget::Flow(flow) => {
                                    let delivered = match flow.protocol {
                                        crate::TransportProtocol::Udp => self
                                            .udp
                                            .record_control_error(context.interface, flow, error)
                                            .is_some(),
                                        crate::TransportProtocol::Tcp => self
                                            .tcp
                                            .record_control_error(flow, error, context.now_ns),
                                    };
                                    (flow.remote.addr, delivered)
                                }
                                ControlErrorTarget::Raw {
                                    local,
                                    remote,
                                    protocol,
                                } => {
                                    let delivered = self.raw.record_control_error(
                                        context.interface,
                                        local,
                                        remote,
                                        protocol,
                                        error,
                                    ) != 0;
                                    (remote, delivered)
                                }
                            };
                            if !delivered {
                                self.forwarded_errors.push_back((
                                    context.interface,
                                    target,
                                    error,
                                    context.now_ns,
                                ));
                            }
                            if let crate::transport::TransportControlError::PacketTooBig { mtu } =
                                error
                            {
                                self.pmtu.observe(
                                    PmtuKey {
                                        interface: context.interface,
                                        destination,
                                    },
                                    mtu,
                                    context.now_ns,
                                );
                            }
                            recycle_packet(recycle, chain, packet_metadata);
                        }
                    }
                }
                FrontendDisposition::Drop(reason) => {
                    record_drop(reason);
                    recycle_packet(recycle, packet.chain, packet.metadata);
                }
            }
        }
        self.run_timers(context.now_ns);
        self.run_dirty(256);
    }

    pub fn reassembled_input(&self) -> &PacketBatch {
        &self.reassembled_input
    }

    pub fn parse_reassembled(
        &mut self,
        context: FlowTurnContext<'_>,
        ethernet: &[crate::stack::NetStackEthernetV1],
    ) {
        self.frontend.process_with_ethernet(
            context.interface,
            context.config,
            &mut self.reassembled_input,
            ethernet,
            &mut self.frontend_batch,
        );
        let count = self.frontend_batch.len();
        for index in 0..count {
            if let Some(packet) = self.frontend_batch.take(index) {
                self.reassembled.push_back(packet);
            }
        }
    }

    pub fn take_unparsed_reassembled(&mut self) -> Option<(PacketChain, PacketMetadata)> {
        let count = self.reassembled_input.len();
        (0..count).find_map(|index| self.reassembled_input.take(index))
    }

    pub fn push_frontend(&mut self, packet: FrontendPacket) {
        self.frontend_batch.push(packet);
    }

    pub fn take_reassembled(&mut self) -> Option<FrontendPacket> {
        self.reassembled.pop_front()
    }

    pub fn take_forwarded_error(
        &mut self,
    ) -> Option<(
        InterfaceId,
        ControlErrorTarget,
        crate::transport::TransportControlError,
        u64,
    )> {
        self.forwarded_errors.pop_front()
    }

    pub fn apply_transport_error(
        &mut self,
        interface: InterfaceId,
        target: ControlErrorTarget,
        error: crate::transport::TransportControlError,
        now_ns: u64,
    ) -> bool {
        let destination = match target {
            ControlErrorTarget::Flow(flow) => {
                let delivered = match flow.protocol {
                    crate::TransportProtocol::Udp => self
                        .udp
                        .record_control_error(interface, flow, error)
                        .is_some(),
                    crate::TransportProtocol::Tcp => {
                        self.tcp.record_control_error(flow, error, now_ns)
                    }
                };
                if !delivered {
                    return false;
                }
                flow.remote.addr
            }
            ControlErrorTarget::Raw {
                local,
                remote,
                protocol,
            } => {
                if self
                    .raw
                    .record_control_error(interface, local, remote, protocol, error)
                    == 0
                {
                    return false;
                }
                remote
            }
        };
        if let crate::transport::TransportControlError::PacketTooBig { mtu } = error {
            self.pmtu.observe(
                PmtuKey {
                    interface,
                    destination,
                },
                mtu,
                now_ns,
            );
        }
        true
    }

    fn run_timers(&mut self, now_ns: u64) {
        let udp = &mut self.udp;
        let tcp = &mut self.tcp;
        let stats = &mut self.stats;
        let mut tcp_expired = alloc::vec::Vec::new();
        let _behind = self.timers.advance(now_ns, 256, |expiry| {
            if expiry.owner < 4096 {
                let id = FlowId(u32::from(expiry.owner) + 1);
                if udp.mark_timer(id, expiry.generation) {
                    stats.timer_expired = stats.timer_expired.saturating_add(1);
                }
            } else {
                let id = FlowId(u32::from(expiry.owner - 4096) + 1);
                if tcp.handle_timer(id, expiry.generation, now_ns) {
                    stats.timer_expired = stats.timer_expired.saturating_add(1);
                    tcp_expired.push(id);
                }
            }
        });
        for flow in tcp_expired {
            self.reschedule_tcp(flow);
        }
    }

    fn run_dirty(&mut self, budget: usize) {
        for _ in 0..budget {
            if self.udp.pop_dirty().is_none() {
                break;
            }
            self.stats.dirty_runs = self.stats.dirty_runs.saturating_add(1);
        }
    }

    fn allow_icmp_error(&mut self, now_ns: u64) -> bool {
        let elapsed = now_ns.saturating_sub(self.icmp_error_refill_ns);
        let refill = elapsed.saturating_mul(100) / 1_000_000_000;
        if refill != 0 {
            self.icmp_error_tokens = self
                .icmp_error_tokens
                .saturating_add(refill.min(u64::from(u32::MAX)) as u32)
                .min(100);
            self.icmp_error_refill_ns = self
                .icmp_error_refill_ns
                .saturating_add(refill.saturating_mul(10_000_000));
        }
        if self.icmp_error_tokens == 0 {
            return false;
        }
        self.icmp_error_tokens -= 1;
        true
    }

    pub fn prepare_udp_tx(
        &mut self,
        flow: FlowId,
        payload: UdpTxLease,
        mark: u32,
        config: &ConfigSnapshot,
        now_ns: u64,
    ) -> Result<PreparedUdpTx, (SocketError, UdpTxLease)> {
        let Some(endpoint) = self.udp.endpoint_info(flow) else {
            return Err((SocketError::Closed, payload));
        };
        let destination = payload.destination;
        let bound_source = (!endpoint.local.addr.is_unspecified()).then_some(endpoint.local.addr);
        let facade = payload.facade();
        let interface_scope = if destination.addr.is_multicast() {
            endpoint.interface.or_else(|| facade.multicast_interface())
        } else {
            endpoint.interface
        };
        let route_result = if destination.addr.is_multicast() {
            config.multicast_route(
                destination.addr,
                bound_source,
                interface_scope,
                endpoint.free_bind,
            )
        } else {
            config.route_with_source_policy(
                destination.addr,
                mark,
                bound_source,
                interface_scope,
                endpoint.free_bind,
            )
        };
        let mut route = match route_result {
            Ok(route) => route,
            Err(_) => return Err((SocketError::NetworkUnreachable, payload)),
        };
        route.mtu = self.pmtu.effective_mtu(
            PmtuKey {
                interface: route.interface,
                destination: destination.addr,
            },
            route.mtu,
            now_ns,
        );
        if payload.dont_route && route.next_hop != destination.addr {
            return Err((SocketError::NetworkUnreachable, payload));
        }
        let interface = config
            .interfaces
            .iter()
            .find(|interface| interface.id == route.interface)
            .expect("route 指向已验证的接口");
        if destination.addr.is_multicast() && facade.multicast_loop() {
            let mut bytes = alloc::vec![0; usize::from(payload.len)];
            if payload.copy_out(&mut bytes).is_ok() {
                let _ = self.udp.deliver_local_multicast(
                    route.interface,
                    Endpoint {
                        addr: route.source,
                        port: endpoint.local.port,
                    },
                    destination,
                    &bytes,
                    facade.multicast_hops(),
                    facade.ip_traffic_class(),
                    now_ns,
                );
            }
        }
        let (destination_mac, unresolved_neighbor) = if destination.addr.is_multicast() {
            (multicast_mac(destination.addr), None)
        } else if interface.loopback {
            (interface.mac_address, None)
        } else {
            let key = NeighborKey {
                interface: route.interface,
                address: route.next_hop,
            };
            match self.neighbors.lookup(key, now_ns) {
                Some((mac_address, _, _)) => {
                    if payload.confirm {
                        let _ = self.neighbors.confirm(key, now_ns);
                    }
                    (mac_address, None)
                }
                None => ([0; 6], Some(key)),
            }
        };
        Ok(PreparedUdpTx {
            payload,
            route,
            destination,
            source_port: endpoint.local.port,
            source_mac: interface.mac_address,
            destination_mac,
            unresolved_neighbor,
            hop_limit: if destination.addr.is_multicast() {
                facade.multicast_hops()
            } else {
                facade.ip_hop_limit()
            },
            traffic_class: facade.ip_traffic_class(),
            completion: {
                let completion = CompletionToken(self.next_completion);
                self.next_completion = self.next_completion.wrapping_add(1).max(1);
                completion
            },
        })
    }

    pub fn prepare_raw_tx(
        &mut self,
        flow: FlowId,
        payload: UdpTxLease,
        mark: u32,
        config: &ConfigSnapshot,
        now_ns: u64,
    ) -> Result<PreparedRawTx, (SocketError, UdpTxLease)> {
        let Some(endpoint) = self.raw.endpoint_info(flow) else {
            return Err((SocketError::Closed, payload));
        };
        let destination = payload.destination.addr;
        let bound_source = (!endpoint.local.is_unspecified()).then_some(endpoint.local);
        let route_result = if destination.is_multicast() {
            config.multicast_route(
                destination,
                bound_source,
                endpoint
                    .interface
                    .or_else(|| payload.facade().multicast_interface()),
                endpoint.free_bind,
            )
        } else {
            config.route_with_source_policy(
                destination,
                mark,
                bound_source,
                endpoint.interface,
                endpoint.free_bind,
            )
        };
        let mut route = match route_result {
            Ok(route) => route,
            Err(_) => return Err((SocketError::NetworkUnreachable, payload)),
        };
        route.mtu = self.pmtu.effective_mtu(
            PmtuKey {
                interface: route.interface,
                destination,
            },
            route.mtu,
            now_ns,
        );
        if payload.dont_route && route.next_hop != destination {
            return Err((SocketError::NetworkUnreachable, payload));
        }
        let interface = config
            .interfaces
            .iter()
            .find(|interface| interface.id == route.interface)
            .expect("route 指向已验证的接口");
        let (destination_mac, unresolved_neighbor) = if destination.is_multicast() {
            (multicast_mac(destination), None)
        } else if interface.loopback {
            (interface.mac_address, None)
        } else {
            let key = NeighborKey {
                interface: route.interface,
                address: route.next_hop,
            };
            match self.neighbors.lookup(key, now_ns) {
                Some((mac_address, _, _)) => {
                    if payload.confirm {
                        let _ = self.neighbors.confirm(key, now_ns);
                    }
                    (mac_address, None)
                }
                None => ([0; 6], Some(key)),
            }
        };
        let facade = payload.facade();
        Ok(PreparedRawTx {
            payload,
            route,
            destination,
            source_mac: interface.mac_address,
            destination_mac,
            unresolved_neighbor,
            protocol: endpoint.protocol,
            header_included: facade.raw_header_included(),
            hop_limit: facade.ip_hop_limit(),
            traffic_class: facade.ip_traffic_class(),
            completion: {
                let completion = CompletionToken(self.next_completion);
                self.next_completion = self.next_completion.wrapping_add(1).max(1);
                completion
            },
        })
    }

    pub fn facade_owner(&self, flow: FlowId) -> Option<OwnerRef> {
        self.udp
            .facade(flow)
            .or_else(|| self.raw.facade(flow))
            .or_else(|| self.tcp.facade(flow))
            .map(|facade| facade.owner())
    }

    fn reschedule_tcp(&mut self, flow: FlowId) {
        let owner = 4096u16.saturating_add(flow.0.saturating_sub(1) as u16);
        let Some(generation) = self.tcp.generation(flow) else {
            self.timers.cancel(owner);
            return;
        };
        if let Some(deadline) = self.tcp.earliest_deadline(flow) {
            self.timers.schedule(owner, generation, deadline);
        } else {
            self.timers.cancel(owner);
        }
    }
}

fn recycle_packet(
    recycle: &mut PacketBatch,
    chain: crate::buf::PacketChain,
    metadata: crate::buf::PacketMetadata,
) {
    recycle
        .push(chain, metadata)
        .unwrap_or_else(|_| panic!("协议回收 batch 超出固定容量"));
}

fn multicast_mac(address: IpAddr) -> [u8; 6] {
    match address {
        IpAddr::V4(address) => {
            let value = address.as_u32() & 0x7f_ffff;
            [
                0x01,
                0x00,
                0x5e,
                ((value >> 16) & 0x7f) as u8,
                (value >> 8) as u8,
                value as u8,
            ]
        }
        IpAddr::V6(address) => [
            0x33,
            0x33,
            address.0[12],
            address.0[13],
            address.0[14],
            address.0[15],
        ],
    }
}
