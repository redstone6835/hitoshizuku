use crate::buf::{CompletionToken, DropReason, PacketBatch, PacketChain, TxBatch, TxPacket};
use crate::control::{ConfigError, ConfigSnapshot, NeighborKey, NeighborTable};
use crate::pipeline::{FrontendBatch, FrontendDisposition, VectorFrontend};
use crate::transport::{
    ControlPacketResult, UdpBindError, UdpDatagram, UdpEndpointTable, UdpTxError, build_udp_packet,
    handle_control_packet,
};
use crate::{Endpoint, FlowId, InterfaceId, ShardId};

use super::TimerWheel;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowShardStats {
    pub udp_delivered: u64,
    pub control_packets: u64,
    pub tx_formed: u64,
    pub dirty_runs: u64,
    pub timer_expired: u64,
}

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
    neighbors: NeighborTable,
    timers: TimerWheel,
    next_completion: u64,
    stats: FlowShardStats,
}

impl FlowShard {
    pub fn new(
        id: ShardId,
        rss_key: [u8; 40],
        rss_generation: u32,
        hash_seed: [u8; 16],
        now_ns: u64,
    ) -> Self {
        Self {
            id,
            frontend: VectorFrontend::new(rss_key, rss_generation),
            frontend_batch: FrontendBatch::new(),
            udp: UdpEndpointTable::new(rss_key),
            neighbors: NeighborTable::new(hash_seed),
            timers: TimerWheel::new(4096, now_ns / 1_000_000),
            next_completion: 1,
            stats: FlowShardStats::default(),
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

    pub fn recv_udp(&mut self, flow: FlowId) -> Option<UdpDatagram> {
        self.udp.recv(flow)
    }

    pub fn take_udp_error(&mut self, flow: FlowId) -> Option<crate::transport::UdpControlError> {
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
        let route = match config.route(destination.addr, mark, bound_source, endpoint.interface) {
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
        let destination_mac = if interface.loopback {
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
        })
    }

    pub fn process_rx(
        &mut self,
        context: FlowTurnContext<'_>,
        input: &mut PacketBatch,
        tx: &mut TxBatch,
        recycle: &mut PacketBatch,
        mut record_drop: impl FnMut(DropReason),
    ) {
        self.frontend.process(
            context.interface,
            context.config,
            input,
            &mut self.frontend_batch,
        );
        let len = self.frontend_batch.len();
        for index in 0..len {
            let Some(packet) = self.frontend_batch.take(index) else {
                continue;
            };
            match packet.parsed.disposition {
                FrontendDisposition::Udp => match self.udp.ingest(context.interface, packet) {
                    Ok(_) => self.stats.udp_delivered = self.stats.udp_delivered.saturating_add(1),
                    Err(mut error) => {
                        error.packet.metadata.drop_reason = error.reason;
                        record_drop(error.reason);
                        recycle_packet(recycle, error.packet.chain, error.packet.metadata);
                    }
                },
                FrontendDisposition::Control(_) => {
                    self.stats.control_packets = self.stats.control_packets.saturating_add(1);
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
                        ControlPacketResult::UdpError {
                            flow,
                            error,
                            packet: chain,
                        } => {
                            let _ = self
                                .udp
                                .record_control_error(context.interface, flow, error);
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

    fn run_timers(&mut self, now_ns: u64) {
        let udp = &mut self.udp;
        let stats = &mut self.stats;
        let _behind = self.timers.advance(now_ns, 256, |expiry| {
            let id = FlowId(u32::from(expiry.owner) + 1);
            if udp.mark_timer(id, expiry.generation) {
                stats.timer_expired = stats.timer_expired.saturating_add(1);
            }
        });
    }

    fn run_dirty(&mut self, budget: usize) {
        for _ in 0..budget {
            if self.udp.pop_dirty().is_none() {
                break;
            }
            self.stats.dirty_runs = self.stats.dirty_runs.saturating_add(1);
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
