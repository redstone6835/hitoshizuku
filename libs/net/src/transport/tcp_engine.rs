//! TCP 单分片流表、重传控制和报文构造。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;

#[cfg(test)]
use core::sync::atomic::{AtomicU64, Ordering};

use crate::buf::{PacketChain, PacketMetadata, RxPoolPressure};
use crate::control::RouteDecision;
use crate::flow::{FlowKey, FlowTable, flow_hash64, rss_hash};
use crate::pipeline::{EthernetHeader, FrontendPacket, IpPacket};
use crate::pipeline::{partial_transport_checksum, transport_checksum};
use crate::socket::{StreamRxCommit, StreamRxStorageKind};
use crate::transport::TCP_PROTOCOL_NUMBER;
use crate::transport::{
    TcpFlags, TcpPacket, TcpSackBlock, TcpSequence, TcpState, TcpStateMachine, TcpTransmit,
    TransportControlError,
};
use crate::{
    AddressFamily, Endpoint, FlowId, InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr, ListenGroup,
    ListenGroupId, OwnerRef, ShardId, SocketError, SocketFacade, TcpTxLease, TransportProtocol,
};

#[cfg(not(test))]
use crate::new_tcp_socket_facade;

const MAX_PENDING_OUTPUT: usize = 512;
const MAX_RETRANSMIT_SEGMENTS: usize = 256;
const MAX_REASSEMBLY_FRAGMENTS: usize = 128;
const MAX_REASSEMBLY_BYTES: usize = 256 * 1024;
const INITIAL_RTO_NS: u64 = 1_000_000_000;
const MIN_RTO_NS: u64 = 200_000_000;
const MAX_RTO_NS: u64 = 60_000_000_000;
const DELAYED_ACK_NS: u64 = 40_000_000;
const CORK_TIMEOUT_NS: u64 = 200_000_000;
const TIME_WAIT_NS: u64 = 60_000_000_000;
const PERSIST_INITIAL_NS: u64 = 1_000_000_000;
const ACTIVE_SYN_RETRIES: u8 = 6;
const PASSIVE_SYN_ACK_RETRIES: u8 = 5;
#[cfg(test)]
const DEFAULT_IPV4_MSS: u16 = 1460;

#[cfg(test)]
static NEXT_TEST_SOCKET: AtomicU64 = AtomicU64::new(10_000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpPath {
    pub route: RouteDecision,
    pub source_mac: [u8; 6],
    pub destination_mac: [u8; 6],
    pub unresolved_neighbor: Option<crate::control::NeighborKey>,
    pub config_generation: u64,
}

pub struct PreparedTcpTx {
    pub flow: FlowId,
    pub flow_generation: u32,
    pub facade_generation: u32,
    pub facade: Arc<SocketFacade>,
    pub payload: Option<TcpTxLease>,
    pub path: TcpPath,
    pub remote: Endpoint,
    pub local_port: u16,
    pub sequence: TcpSequence,
    pub acknowledgement: TcpSequence,
    pub flags: TcpFlags,
    pub window: u16,
    pub options: [u8; 40],
    pub options_len: u8,
    pub parsed_options: crate::transport::TcpOptions,
    pub completion: u64,
    pub low_latency: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalTcpPeerHint {
    flow: FlowId,
    flow_generation: u32,
    facade_generation: u32,
    stack_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpBindError {
    Duplicate,
    Full,
    InvalidEndpoint,
    NotListener,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpIngressError {
    NoEndpoint,
    Malformed,
    ReceiveBufferFull,
    FlowTableFull,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpEngineStats {
    pub delivered: u64,
    pub established: u64,
    pub reset: u64,
    pub retransmitted: u64,
    pub fast_retransmit: u64,
    pub out_of_order: u64,
    pub paws_drop: u64,
    pub rx_pinned_bytes: u64,
    pub rx_compact_copy_bytes: u64,
    pub loopback_shared_bytes: u64,
    pub rx_pool_low_water_fallbacks: u64,
}

struct Listener {
    group: Arc<ListenGroup>,
}

struct SentSegment {
    sequence: TcpSequence,
    end: TcpSequence,
    stream_start: Option<u64>,
    payload_len: u16,
    flags: TcpFlags,
    sent_ns: u64,
    first_sent_ns: u64,
    transmissions: u8,
    sacked: bool,
}

struct ReassemblyFragment {
    sequence: TcpSequence,
    bytes: Vec<u8>,
}

enum IngressPayload<'a> {
    Empty,
    #[cfg(test)]
    Owned(Vec<u8>),
    Packet {
        chain: &'a mut PacketChain,
        offset: usize,
        len: usize,
        pressure: RxPoolPressure,
    },
    Lease {
        lease: &'a TcpTxLease,
        offset: usize,
        len: usize,
    },
}

impl IngressPayload<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            #[cfg(test)]
            Self::Owned(bytes) => bytes.len(),
            Self::Packet { len, .. } => *len,
            Self::Lease { len, .. } => *len,
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn copy_to_socket(
        &mut self,
        facade: &SocketFacade,
        payload_offset: usize,
    ) -> Result<StreamRxCommit, SocketError> {
        let len = self.len().saturating_sub(payload_offset);
        match self {
            Self::Empty => Ok(StreamRxCommit {
                len: 0,
                storage: StreamRxStorageKind::Discarded,
                low_water_fallback: false,
            }),
            #[cfg(test)]
            Self::Owned(bytes) => facade.push_stream_rx_compact(&bytes[payload_offset..]),
            Self::Packet {
                chain,
                offset,
                pressure,
                ..
            } => facade.push_stream_rx_packet(chain, *offset + payload_offset, len, *pressure),
            Self::Lease { lease, offset, .. } => {
                facade.push_stream_rx_lease(lease, *offset + payload_offset, len, true)
            }
        }
    }

    fn into_vec(self) -> Result<Vec<u8>, TcpIngressError> {
        match self {
            Self::Empty => Ok(Vec::new()),
            #[cfg(test)]
            Self::Owned(bytes) => Ok(bytes),
            Self::Packet {
                chain, offset, len, ..
            } => {
                let mut bytes = Vec::new();
                bytes.resize(len, 0);
                chain
                    .copy_out(offset, &mut bytes)
                    .map_err(|_| TcpIngressError::Malformed)?;
                Ok(bytes)
            }
            Self::Lease { lease, offset, len } => {
                let mut bytes = Vec::new();
                bytes.resize(len, 0);
                lease
                    .copy_range(offset, &mut bytes)
                    .map_err(|_| TcpIngressError::Malformed)?;
                Ok(bytes)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TcpDeadlines {
    retransmit: Option<u64>,
    delayed_ack: Option<u64>,
    persist: Option<u64>,
    time_wait: Option<u64>,
    cork: Option<u64>,
    keepalive: Option<u64>,
    defer_accept: Option<u64>,
}

impl TcpDeadlines {
    const fn new() -> Self {
        Self {
            retransmit: None,
            delayed_ack: None,
            persist: None,
            time_wait: None,
            cork: None,
            keepalive: None,
            defer_accept: None,
        }
    }

    fn earliest(self) -> Option<u64> {
        [
            self.retransmit,
            self.delayed_ack,
            self.persist,
            self.time_wait,
            self.cork,
            self.keepalive,
            self.defer_accept,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

struct RttEstimator {
    smoothed_ns: Option<u64>,
    variance_ns: u64,
    rto_ns: u64,
}

impl RttEstimator {
    const fn new() -> Self {
        Self {
            smoothed_ns: None,
            variance_ns: INITIAL_RTO_NS / 2,
            rto_ns: INITIAL_RTO_NS,
        }
    }

    fn sample(&mut self, sample_ns: u64) {
        if let Some(smoothed) = self.smoothed_ns {
            let error = smoothed.abs_diff(sample_ns);
            self.variance_ns = (self.variance_ns.saturating_mul(3) + error) / 4;
            self.smoothed_ns = Some((smoothed.saturating_mul(7) + sample_ns) / 8);
        } else {
            self.smoothed_ns = Some(sample_ns);
            self.variance_ns = sample_ns / 2;
        }
        self.rto_ns = self
            .smoothed_ns
            .unwrap_or(sample_ns)
            .saturating_add(self.variance_ns.saturating_mul(4))
            .clamp(MIN_RTO_NS, MAX_RTO_NS);
    }

    fn backoff(&mut self) {
        self.rto_ns = self.rto_ns.saturating_mul(2).min(MAX_RTO_NS);
    }
}

struct CongestionControl {
    cwnd: u32,
    ssthresh: u32,
    duplicate_acks: u8,
    recover: TcpSequence,
    fast_recovery: bool,
}

impl CongestionControl {
    fn new(mss: u16) -> Self {
        let mss = u32::from(mss);
        Self {
            cwnd: (mss.saturating_mul(10))
                .min(14_600)
                .max(mss.saturating_mul(2)),
            ssthresh: u32::MAX,
            duplicate_acks: 0,
            recover: TcpSequence(0),
            fast_recovery: false,
        }
    }

    fn new_ack(&mut self, acknowledged: u32, mss: u16, send_next: TcpSequence) {
        self.duplicate_acks = 0;
        if self.fast_recovery && self.recover.before_or_equal(send_next) {
            self.fast_recovery = false;
            self.cwnd = self.ssthresh;
            return;
        }
        let mss = u32::from(mss);
        if self.cwnd < self.ssthresh {
            self.cwnd = self.cwnd.saturating_add(acknowledged.min(mss));
        } else {
            self.cwnd = self
                .cwnd
                .saturating_add(mss.saturating_mul(mss) / self.cwnd.max(1));
        }
    }

    fn duplicate_ack(&mut self, flight: u32, mss: u16, send_next: TcpSequence) -> bool {
        self.duplicate_acks = self.duplicate_acks.saturating_add(1);
        let mss = u32::from(mss);
        if self.duplicate_acks == 3 {
            self.ssthresh = (flight / 2).max(mss.saturating_mul(2));
            self.cwnd = self.ssthresh.saturating_add(mss.saturating_mul(3));
            self.recover = send_next;
            self.fast_recovery = true;
            true
        } else {
            if self.fast_recovery {
                self.cwnd = self.cwnd.saturating_add(mss);
            }
            false
        }
    }
}

struct TcpFlow {
    facade: Arc<SocketFacade>,
    machine: TcpStateMachine,
    path: TcpPath,
    remote: Endpoint,
    local: Endpoint,
    pending_connect: Option<u64>,
    accept_group: Option<Arc<ListenGroup>>,
    accept_reserved: bool,
    retransmit: VecDeque<SentSegment>,
    unacknowledged_segments: u32,
    retransmitted_segments: u32,
    flight_bytes: u32,
    reassembly: Vec<ReassemblyFragment>,
    reassembly_bytes: usize,
    deadlines: TcpDeadlines,
    rtt: RttEstimator,
    congestion: CongestionControl,
    peer_window: u32,
    peer_window_scale: u8,
    local_window_scale: u8,
    mss: u16,
    peer_mss: u16,
    ack_pending: u8,
    persist_ns: u64,
    timestamp_enabled: bool,
    timestamp_recent: Option<u32>,
    sack_permitted: bool,
    cork_force: bool,
    close_requested: bool,
    listener_key: Option<(IpAddr, u16, Option<InterfaceId>)>,
    last_activity_ns: u64,
    keepalive_probes: u8,
    last_advertised_window: u16,
    output_blocked: bool,
    local_transport: bool,
    local_peer_hint: Option<LocalTcpPeerHint>,
}

impl TcpFlow {
    fn flight_size(&self) -> u32 {
        self.flight_bytes
    }

    fn send_allowance(&self) -> u32 {
        self.peer_window
            .min(self.congestion.cwnd)
            .saturating_sub(self.flight_size())
    }

    fn earliest_deadline(&self) -> Option<u64> {
        self.deadlines.earliest()
    }
}

pub struct TcpEndpointTable {
    shard: ShardId,
    rss_key: [u8; 40],
    isn_key: [u8; 16],
    flows: FlowTable<TcpFlow>,
    listeners: BTreeMap<(IpAddr, u16, Option<InterfaceId>), Listener>,
    output: VecDeque<PreparedTcpTx>,
    output_blocked: VecDeque<FlowId>,
    next_completion: u64,
    stats: TcpEngineStats,
}

impl TcpEndpointTable {
    #[cfg(test)]
    pub fn new(rss_key: [u8; 40], isn_key: [u8; 16]) -> Self {
        Self::new_on_shard(ShardId(0), rss_key, isn_key)
    }

    pub fn new_on_shard(shard: ShardId, rss_key: [u8; 40], isn_key: [u8; 16]) -> Self {
        Self {
            shard,
            rss_key,
            isn_key,
            flows: FlowTable::new(),
            listeners: BTreeMap::new(),
            output: VecDeque::with_capacity(MAX_PENDING_OUTPUT),
            output_blocked: VecDeque::new(),
            next_completion: 1,
            stats: TcpEngineStats::default(),
        }
    }

    pub const fn stats(&self) -> TcpEngineStats {
        self.stats
    }

    pub fn listen(
        &mut self,
        local: Endpoint,
        interface: Option<InterfaceId>,
        group: Arc<ListenGroup>,
    ) -> Result<(), TcpBindError> {
        if local.port == 0 {
            return Err(TcpBindError::InvalidEndpoint);
        }
        let key = (local.addr, local.port, interface);
        // close() 先标记 ListenGroup，再把跨 shard 的清理工作排队。
        // 允许新 listener 在这段过渡期替换旧条目，避免连续 close/bind/listen
        // 时因为某个 shard 仍未消费 RemoveListener 而让整个安装事务失败。
        if self
            .listeners
            .get(&key)
            .is_some_and(|listener| !listener.group.is_closing())
        {
            return Err(TcpBindError::Duplicate);
        }
        self.listeners.insert(key, Listener { group });
        Ok(())
    }

    pub fn close_listener(&mut self, group: ListenGroupId) -> bool {
        let keys = self
            .listeners
            .iter()
            .filter(|(_, listener)| listener.group.id() == group)
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return false;
        }
        for key in &keys {
            self.listeners.remove(key);
        }
        let pending: Vec<_> = (1..=4096)
            .map(FlowId)
            .filter(|id| {
                self.flows.get(*id).is_some_and(|flow| {
                    flow.listener_key.is_some_and(|key| keys.contains(&key))
                        || flow
                            .accept_group
                            .as_ref()
                            .is_some_and(|listener| listener.id() == group)
                })
            })
            .collect();
        for id in pending {
            self.reap(id, Some(SocketError::ConnectionReset));
        }
        true
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        let affected = (1..=4096)
            .map(FlowId)
            .filter(|id| {
                self.flows
                    .get(*id)
                    .is_some_and(|flow| flow.path.route.interface == interface)
            })
            .collect::<Vec<_>>();
        for id in &affected {
            self.reap(*id, Some(SocketError::NetworkUnreachable));
        }
        affected.len()
    }

    pub fn connect(
        &mut self,
        local: Endpoint,
        remote: Endpoint,
        path: TcpPath,
        facade: Arc<SocketFacade>,
        control_sequence: u64,
        local_transport: bool,
        now_ns: u64,
    ) -> Result<FlowId, TcpBindError> {
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp)
            .ok_or(TcpBindError::InvalidEndpoint)?;
        let mss = apply_user_mss(path_mss(path.route.mtu, local.addr), facade.tcp_maxseg());
        let iss = self.initial_sequence(key, now_ns);
        let mut machine = TcpStateMachine::new(iss, advertised_window(&facade, 0));
        let transmit = machine.active_open().unwrap();
        let local_window_scale = choose_window_scale(facade.receive_window_scale_limit());
        let initial_window = advertised_window(&facade, local_window_scale);
        let flow = TcpFlow {
            facade: Arc::clone(&facade),
            machine,
            path,
            remote,
            local,
            pending_connect: Some(control_sequence),
            accept_group: None,
            accept_reserved: false,
            retransmit: VecDeque::new(),
            unacknowledged_segments: 0,
            retransmitted_segments: 0,
            flight_bytes: 0,
            reassembly: Vec::new(),
            reassembly_bytes: 0,
            deadlines: TcpDeadlines::new(),
            rtt: RttEstimator::new(),
            congestion: CongestionControl::new(mss),
            peer_window: u32::from(u16::MAX),
            peer_window_scale: 0,
            local_window_scale,
            mss,
            peer_mss: mss,
            ack_pending: 0,
            persist_ns: PERSIST_INITIAL_NS,
            timestamp_enabled: false,
            timestamp_recent: None,
            sack_permitted: false,
            cork_force: false,
            close_requested: false,
            listener_key: None,
            last_activity_ns: now_ns,
            keepalive_probes: 0,
            last_advertised_window: initial_window,
            output_blocked: false,
            local_transport,
            local_peer_hint: None,
        };
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let id = self
            .flows
            .insert_prehashed(key, hash, flow)
            .map_err(|error| match error {
                crate::flow::FlowInsertError::Duplicate => TcpBindError::Duplicate,
                _ => TcpBindError::Full,
            })?;
        publish_tcp_info(self.flows.get(id).unwrap());
        facade.publish_connecting();
        self.queue_control(id, transmit, now_ns, true);
        Ok(id)
    }

    pub fn ingest(
        &mut self,
        interface: InterfaceId,
        path: TcpPath,
        packet: FrontendPacket,
        now_ns: u64,
    ) -> Result<(FlowId, PacketChain, PacketMetadata), (TcpIngressError, PacketChain, PacketMetadata)>
    {
        let metadata = packet.metadata;
        let mut chain = packet.chain;
        let Some(key) = packet.parsed.flow else {
            return Err((TcpIngressError::Malformed, chain, metadata));
        };
        let Some(tcp) = packet.parsed.tcp else {
            return Err((TcpIngressError::Malformed, chain, metadata));
        };
        let id = match self.ingress_flow(interface, path, key, tcp, now_ns, false) {
            Ok(id) => id,
            Err(error) => return Err((error, chain, metadata)),
        };

        let payload_offset = usize::from(tcp.payload_offset);
        let payload_len = tcp.payload_len as usize;
        if payload_offset
            .checked_add(payload_len)
            .is_none_or(|end| end > chain.total_len())
        {
            return Err((TcpIngressError::Malformed, chain, metadata));
        }
        let result = self.process_segment_inner(
            id,
            tcp,
            IngressPayload::Packet {
                chain: &mut chain,
                offset: payload_offset,
                len: payload_len,
                pressure: metadata.rx_pool_pressure,
            },
            now_ns,
            true,
        );
        match result {
            Ok(()) => {
                self.stats.delivered = self.stats.delivered.saturating_add(1);
                Ok((id, chain, metadata))
            }
            Err(error) => Err((error, chain, metadata)),
        }
    }

    pub fn ingest_local(
        &mut self,
        interface: InterfaceId,
        path: TcpPath,
        key: FlowKey,
        tcp: TcpPacket,
        payload: Option<&TcpTxLease>,
        now_ns: u64,
    ) -> Result<FlowId, TcpIngressError> {
        self.ingest_local_with_info(interface, path, key, tcp, payload, now_ns, true)
    }

    pub fn ingest_local_deferred_info(
        &mut self,
        interface: InterfaceId,
        path: TcpPath,
        key: FlowKey,
        tcp: TcpPacket,
        payload: Option<&TcpTxLease>,
        now_ns: u64,
    ) -> Result<FlowId, TcpIngressError> {
        self.ingest_local_with_info(interface, path, key, tcp, payload, now_ns, false)
    }

    pub fn try_local_data_effect(
        &mut self,
        interface: InterfaceId,
        work: &PreparedTcpTx,
        now_ns: u64,
    ) -> Result<Option<(FlowId, FlowId)>, TcpIngressError> {
        #[cfg(feature = "performance-profile")]
        let effect_start = profiling::read_counter();
        if !crate::socket::local_transport_fast_path_eligible() {
            return Ok(None);
        }
        let Some(payload) = work.payload.as_ref() else {
            return Ok(None);
        };
        let payload_len = usize::from(payload.len);
        if payload_len == 0
            || work.flags.bits() & !(TcpFlags::ACK | TcpFlags::PSH).bits() != 0
            || !work.flags.contains(TcpFlags::ACK)
            || work.parsed_options.maximum_segment_size.is_some()
            || work.parsed_options.window_scale.is_some()
            || work.parsed_options.sack_permitted
            || work.parsed_options.sack_blocks.iter().any(Option::is_some)
        {
            return Ok(None);
        }

        let source = Endpoint {
            addr: work.path.route.source,
            port: work.local_port,
        };
        if self.flows.generation(work.flow) != Some(work.flow_generation)
            || work.facade.generation() != work.facade_generation
            || work.facade.is_closing()
        {
            return Ok(None);
        }
        let sender_id = work.flow;
        let Some(peer_key) = FlowKey::new(source, work.remote, TransportProtocol::Tcp) else {
            return Ok(None);
        };
        let cached_peer = self
            .flows
            .get(sender_id)
            .and_then(|sender| sender.local_peer_hint);
        let peer_id = if let Some(hint) = cached_peer {
            let valid = self.flows.generation(hint.flow) == Some(hint.flow_generation)
                && self.flows.key(hint.flow) == Some(peer_key)
                && self.flows.get(hint.flow).is_some_and(|peer| {
                    peer.facade.generation() == hint.facade_generation
                        && peer.facade.stack_generation() == hint.stack_generation
                        && !peer.facade.is_closing()
                });
            if valid {
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::TcpLocalPeerHintHits, 1);
                hint.flow
            } else {
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::TcpLocalPeerHintInvalid, 1);
                self.flows.get_mut(sender_id).unwrap().local_peer_hint = None;
                let peer_hash = flow_hash64(rss_hash(&self.rss_key, &peer_key));
                let Some(peer_id) = self.flows.find(&peer_key, peer_hash) else {
                    return Ok(None);
                };
                peer_id
            }
        } else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpLocalPeerHintMisses, 1);
            let peer_hash = flow_hash64(rss_hash(&self.rss_key, &peer_key));
            let Some(peer_id) = self.flows.find(&peer_key, peer_hash) else {
                return Ok(None);
            };
            peer_id
        };
        if sender_id == peer_id {
            return Ok(None);
        }

        let segment = crate::transport::TcpSegment {
            sequence: work.sequence,
            acknowledgement: work.acknowledgement,
            flags: work.flags,
            window: work.window,
            payload_len: payload.len.into(),
        };
        let payload_end = work.sequence + u32::from(payload.len);
        let Some((peer_facade, peer_window, peer_timestamp_enabled, peer_hint)) =
            self.flows.get(peer_id).and_then(|peer| {
                let sender = self.flows.get(sender_id)?;
                let peer_generation = self.flows.generation(peer_id)?;
                (sender.local == source
                    && sender.remote == work.remote
                    && peer.local == work.remote
                    && peer.remote == source
                    && !sender.accept_reserved
                    && !peer.accept_reserved
                    && peer.path.route.interface == interface
                    && sender.path.route.interface == interface
                    && Arc::ptr_eq(&sender.facade, &work.facade)
                    && sender.facade.stack_generation() == peer.facade.stack_generation()
                    && !peer.facade.is_closing()
                    && sender.machine.state() == TcpState::Established
                    && sender
                        .machine
                        .send_unacknowledged()
                        .before_or_equal(work.sequence)
                    && payload_end.before_or_equal(sender.machine.send_next())
                    && peer.machine.accepts_local_data(segment)
                    && peer.timestamp_enabled == work.parsed_options.timestamp.is_some())
                .then(|| {
                    (
                        Arc::clone(&peer.facade),
                        u32::from(work.window) << peer.peer_window_scale,
                        peer.timestamp_enabled,
                        LocalTcpPeerHint {
                            flow: peer_id,
                            flow_generation: peer_generation,
                            facade_generation: peer.facade.generation(),
                            stack_generation: peer.facade.stack_generation(),
                        },
                    )
                })
            })
        else {
            return Ok(None);
        };
        self.flows.get_mut(sender_id).unwrap().local_peer_hint = Some(peer_hint);
        let receive_window = peer_facade.stream_receive_window();
        #[cfg(feature = "performance-profile")]
        profiling::observe(
            profiling::Metric::TcpLocalEffectReceiveWindow,
            receive_window as u64,
        );
        if payload_len > receive_window {
            peer_facade.mark_local_stream_window_blocked();
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpLocalEffectWindowRejects, 1);
            return Ok(None);
        }
        if peer_timestamp_enabled
            && work.parsed_options.timestamp.is_some_and(|timestamp| {
                self.flows.get(peer_id).is_some_and(|peer| {
                    peer.timestamp_recent
                        .is_some_and(|recent| (timestamp.value.wrapping_sub(recent) as i32) < 0)
                })
            })
        {
            return Ok(None);
        }

        #[cfg(feature = "performance-profile")]
        let lookup_done = profiling::read_counter();
        let commit = peer_facade
            .push_stream_rx_lease(payload, 0, payload_len, work.flags.contains(TcpFlags::PSH))
            .map_err(|_| {
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::TcpLocalEffectRingRejects, 1);
                TcpIngressError::ReceiveBufferFull
            })?;
        self.record_rx_commit(commit);
        #[cfg(feature = "performance-profile")]
        let commit_done = profiling::read_counter();

        let (peer_previous_ack, peer_current_ack, peer_wire_window) = {
            let peer = self.flows.get_mut(peer_id).unwrap();
            let (previous, current) = peer
                .machine
                .accept_local_data(segment)
                .expect("本地 effect 在提交 payload 前已完整校验");
            peer.peer_window = peer_window;
            peer.timestamp_recent = work
                .parsed_options
                .timestamp
                .map(|timestamp| timestamp.value)
                .or(peer.timestamp_recent);
            peer.ack_pending = 0;
            peer.deadlines.delayed_ack = None;
            peer.last_activity_ns = now_ns;
            let wire_window = advertised_window(&peer.facade, peer.local_window_scale);
            peer.last_advertised_window = wire_window;
            (previous, current, wire_window)
        };
        let (sender_previous_ack, sender_current_ack) = {
            let sender = self.flows.get_mut(sender_id).unwrap();
            let acknowledged = sender
                .machine
                .accept_local_ack(payload_end)
                .expect("本地 effect 的发送序列在提交 payload 前已完整校验");
            sender.peer_window = u32::from(peer_wire_window) << sender.peer_window_scale;
            if sender.timestamp_enabled {
                sender.timestamp_recent = Some((now_ns / 1_000_000) as u32);
            }
            sender.last_activity_ns = now_ns;
            acknowledged
        };

        if peer_current_ack.after(peer_previous_ack) {
            self.acknowledge(peer_id, peer_previous_ack, peer_current_ack, now_ns);
            self.drain_send_with_info(peer_id, now_ns, false);
        }
        self.acknowledge(sender_id, sender_previous_ack, sender_current_ack, now_ns);
        self.drain_send_with_info(sender_id, now_ns, false);
        if work.facade.stream_unsent_len() == 0
            && !self
                .output
                .iter()
                .any(|pending| pending.flow == sender_id && pending.payload.is_some())
        {
            work.facade.install_local_tcp_direct_peer(&peer_facade);
        }
        if peer_facade.stream_unsent_len() == 0
            && !self
                .output
                .iter()
                .any(|pending| pending.flow == peer_id && pending.payload.is_some())
        {
            peer_facade.install_local_tcp_direct_peer(&work.facade);
        }
        self.stats.delivered = self.stats.delivered.saturating_add(1);
        #[cfg(feature = "performance-profile")]
        {
            let effect_done = profiling::read_counter();
            profiling::observe(
                profiling::Metric::TcpLocalEffectLookupCycles,
                lookup_done.wrapping_sub(effect_start),
            );
            profiling::observe(
                profiling::Metric::TcpLocalEffectCommitCycles,
                commit_done.wrapping_sub(lookup_done),
            );
            profiling::observe(
                profiling::Metric::TcpLocalEffectAckCycles,
                effect_done.wrapping_sub(commit_done),
            );
            profiling::observe(
                profiling::Metric::TcpLocalEffectCycles,
                effect_done.wrapping_sub(effect_start),
            );
        }
        Ok(Some((sender_id, peer_id)))
    }

    /// 查找仍由流表持有的本地 TCP 对端。
    ///
    /// `SocketFacade::close()` 会在 ELM 处理关闭命令前递增 facade 代际，
    /// 但已经交付到接收环形缓冲区的数据仍必须结算。这里以流代际、反向四元组
    /// 和协议栈代际确认身份，不使用只适合发送快速路径的 facade 状态。
    fn local_peer_flow_id(&self, sender_id: FlowId) -> Option<FlowId> {
        let sender = self.flows.get(sender_id)?;
        let hint = sender.local_peer_hint?;
        let peer_id = hint.flow;
        if sender_id == peer_id || self.flows.generation(peer_id) != Some(hint.flow_generation) {
            return None;
        }
        let peer = self.flows.get(peer_id)?;
        (sender.local_transport
            && peer.local_transport
            && sender.local == peer.remote
            && sender.remote == peer.local
            && sender.path.route.interface == peer.path.route.interface
            && sender.facade.stack_generation() == hint.stack_generation
            && peer.facade.stack_generation() == hint.stack_generation)
            .then_some(peer_id)
    }

    /// 对 socket 直达 lane 已经交付的字节统一推进 TCP 序列空间。
    ///
    /// 数据在进入这里前已经由代际校验的 peer route 原子地提交到接收 ring；本函数
    /// 只在 ELM owner turn 内更新双方状态机，不再构造报文或重复持有 payload。
    pub fn reconcile_local_direct(
        &mut self,
        sender_id: FlowId,
        mut bytes: u32,
        now_ns: u64,
    ) -> Option<(FlowId, FlowId)> {
        if bytes == 0 {
            return None;
        }
        let peer_id = self.local_peer_flow_id(sender_id)?;

        while bytes != 0 {
            let chunk = bytes;
            let (sequence, acknowledgement) = {
                let sender = self.flows.get(sender_id)?;
                let peer = self.flows.get(peer_id)?;
                if sender.machine.state() != TcpState::Established
                    || peer.machine.state() != TcpState::Established
                    || sender.machine.send_next() != peer.machine.receive_next()
                {
                    return None;
                }
                (
                    sender.machine.send_next(),
                    peer.machine.send_unacknowledged(),
                )
            };
            let segment = crate::transport::TcpSegment {
                sequence,
                acknowledgement,
                flags: TcpFlags::ACK | TcpFlags::PSH,
                window: 0,
                payload_len: chunk,
            };
            let reserved = self.flows.get_mut(sender_id)?.machine.reserve_send(chunk)?;
            debug_assert_eq!(reserved, sequence);
            let peer_wire_window = {
                let peer = self.flows.get_mut(peer_id)?;
                peer.machine.accept_local_data(segment)?;
                peer.last_activity_ns = now_ns;
                let window = advertised_window(&peer.facade, peer.local_window_scale);
                peer.last_advertised_window = window;
                window
            };
            let end = sequence + chunk;
            let sender = self.flows.get_mut(sender_id)?;
            let (previous, current) = sender.machine.accept_local_ack(end)?;
            sender.peer_window = u32::from(peer_wire_window) << sender.peer_window_scale;
            sender.last_activity_ns = now_ns;
            sender.congestion.new_ack(
                current.distance_from(previous),
                sender.mss,
                sender.machine.send_next(),
            );
            sender.deadlines.retransmit = None;
            sender.deadlines.persist = None;
            bytes -= chunk;
        }
        self.stats.delivered = self.stats.delivered.saturating_add(1);
        Some((sender_id, peer_id))
    }

    pub fn local_peer_facade(&self, sender_id: FlowId) -> Option<(FlowId, Arc<SocketFacade>)> {
        let peer_id = self.local_peer_flow_id(sender_id)?;
        let peer = self.flows.get(peer_id)?;
        Some((peer_id, Arc::clone(&peer.facade)))
    }

    fn ingest_local_with_info(
        &mut self,
        interface: InterfaceId,
        path: TcpPath,
        key: FlowKey,
        tcp: TcpPacket,
        payload: Option<&TcpTxLease>,
        now_ns: u64,
        publish_info: bool,
    ) -> Result<FlowId, TcpIngressError> {
        let payload_len = tcp.payload_len as usize;
        if payload
            .as_ref()
            .map_or(0, |payload| usize::from(payload.len))
            != payload_len
        {
            return Err(TcpIngressError::Malformed);
        }
        let id = self.ingress_flow(interface, path, key, tcp, now_ns, true)?;
        let ingress = payload.map_or(IngressPayload::Empty, |payload| IngressPayload::Lease {
            lease: payload,
            offset: 0,
            len: payload_len,
        });
        self.process_segment_inner(id, tcp, ingress, now_ns, publish_info)?;
        self.stats.delivered = self.stats.delivered.saturating_add(1);
        Ok(id)
    }

    fn ingress_flow(
        &mut self,
        interface: InterfaceId,
        path: TcpPath,
        key: FlowKey,
        tcp: TcpPacket,
        now_ns: u64,
        local_transport: bool,
    ) -> Result<FlowId, TcpIngressError> {
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        match self.flows.find(&key, hash) {
            Some(id) => Ok(id),
            None if tcp.flags.contains(TcpFlags::SYN) && !tcp.flags.contains(TcpFlags::ACK) => {
                self.accept_syn(interface, path, key, tcp, now_ns, local_transport)
            }
            None => Err(TcpIngressError::NoEndpoint),
        }
    }

    pub fn drain_send(&mut self, id: FlowId, now_ns: u64) -> bool {
        self.drain_send_with_info(id, now_ns, true)
    }

    pub fn drain_send_deferred_info(&mut self, id: FlowId, now_ns: u64) -> bool {
        self.drain_send_with_info(id, now_ns, false)
    }

    fn drain_send_with_info(&mut self, id: FlowId, now_ns: u64, publish_info: bool) -> bool {
        let mut queued = false;
        let mut queued_bytes = 0usize;
        let window_update = self.flows.get_mut(id).is_some_and(|flow| {
            if flow.facade.tcp_keepalive_enabled()
                && matches!(
                    flow.machine.state(),
                    TcpState::Established | TcpState::CloseWait
                )
            {
                flow.deadlines.keepalive.get_or_insert(
                    flow.last_activity_ns
                        .saturating_add(flow.facade.tcp_keepidle_ns()),
                );
            } else {
                flow.deadlines.keepalive = None;
                flow.keepalive_probes = 0;
            }
            if !flow.facade.take_receive_window_update() {
                return false;
            }
            let current = advertised_window(&flow.facade, flow.local_window_scale);
            current > flow.last_advertised_window
                && (flow.last_advertised_window == 0
                    || u32::from(current - flow.last_advertised_window) << flow.local_window_scale
                        >= u32::from(flow.mss))
        });
        if window_update {
            self.queue_ack(id, now_ns);
            queued = true;
        }
        let mut unsent_hint = None;
        loop {
            if self.output.len() >= MAX_PENDING_OUTPUT {
                self.mark_output_blocked(id);
                break;
            }
            let Some(flow) = self.flows.get_mut(id) else {
                break;
            };
            if !matches!(
                flow.machine.state(),
                TcpState::Established | TcpState::CloseWait
            ) {
                break;
            }
            if flow.retransmit.len() >= MAX_RETRANSMIT_SEGMENTS {
                break;
            }
            if flow.peer_window == 0 {
                flow.deadlines
                    .persist
                    .get_or_insert(now_ns.saturating_add(flow.persist_ns));
                break;
            }
            let (_, options_len, _) = wire_options(flow, TcpFlags::ACK | TcpFlags::PSH, now_ns);
            let segment_limit = effective_payload_mss(
                flow.path.route.mtu,
                flow.local.addr,
                flow.peer_mss,
                options_len,
            );
            let unsent = unsent_hint.unwrap_or_else(|| flow.facade.stream_unsent_len());
            if (flow.facade.tcp_cork() || flow.facade.tcp_more())
                && !flow.cork_force
                && unsent < usize::from(segment_limit)
            {
                flow.deadlines
                    .cork
                    .get_or_insert(now_ns.saturating_add(CORK_TIMEOUT_NS));
                break;
            }
            if !flow.facade.tcp_nodelay()
                && flow.flight_size() != 0
                && unsent < usize::from(segment_limit)
            {
                break;
            }
            let allowance = flow.send_allowance().min(u32::from(segment_limit));
            if allowance == 0 {
                break;
            }
            let Some(payload) = flow.facade.take_stream_tx_deferred(allowance as usize) else {
                break;
            };
            queued_bytes = queued_bytes.saturating_add(usize::from(payload.len));
            let remaining_unsent = unsent.saturating_sub(usize::from(payload.len));
            unsent_hint = Some(remaining_unsent);
            let Some(sequence) = flow.machine.reserve_send(u32::from(payload.len)) else {
                break;
            };
            let flags = if remaining_unsent == 0 {
                TcpFlags::ACK | TcpFlags::PSH
            } else {
                TcpFlags::ACK
            };
            let transmit = TcpTransmit {
                sequence,
                acknowledgement: flow.machine.receive_next(),
                flags,
                window: advertised_window(&flow.facade, flow.local_window_scale),
            };
            self.queue_transmit(id, transmit, Some(payload), now_ns, false, true);
            if let Some(flow) = self.flows.get_mut(id) {
                flow.cork_force = false;
                flow.deadlines.cork = None;
            }
            queued = true;
        }
        if queued_bytes != 0
            && let Some(flow) = self.flows.get(id)
        {
            flow.facade.finish_stream_tx_batch(queued_bytes);
            if publish_info {
                publish_tcp_info(flow);
            }
        }
        queued
    }

    pub fn publish_tcp_info(&self, id: FlowId) {
        if let Some(flow) = self.flows.get(id) {
            publish_tcp_info(flow);
        }
    }

    pub fn resume_output_blocked(&mut self, now_ns: u64, budget: usize) -> usize {
        let mut resumed = 0;
        while resumed < budget && self.output.len() < MAX_PENDING_OUTPUT {
            let Some(id) = self.output_blocked.pop_front() else {
                break;
            };
            let Some(flow) = self.flows.get_mut(id) else {
                continue;
            };
            flow.output_blocked = false;
            self.drain_send(id, now_ns);
            resumed += 1;
        }
        resumed
    }

    pub fn has_output_blocked(&self) -> bool {
        !self.output_blocked.is_empty()
    }

    pub fn has_pending_output(&self) -> bool {
        !self.output.is_empty()
    }

    fn mark_output_blocked(&mut self, id: FlowId) {
        let Some(flow) = self.flows.get_mut(id) else {
            return;
        };
        if !flow.output_blocked {
            flow.output_blocked = true;
            self.output_blocked.push_back(id);
        }
    }

    pub fn close_flow(&mut self, id: FlowId, now_ns: u64) -> bool {
        self.quiesce_local_pair(id, now_ns);
        let Some(flow) = self.flows.get_mut(id) else {
            return false;
        };
        flow.close_requested = true;
        self.drain_send(id, now_ns);
        self.maybe_send_fin(id, now_ns);
        true
    }

    pub fn abort_flow(&mut self, id: FlowId, now_ns: u64) -> bool {
        self.quiesce_local_pair(id, now_ns);
        let Some(flow) = self.flows.get(id) else {
            return false;
        };
        let transmit = TcpTransmit {
            sequence: flow.machine.send_next(),
            acknowledgement: flow.machine.receive_next(),
            flags: TcpFlags::RST | TcpFlags::ACK,
            window: 0,
        };
        self.queue_transmit(id, transmit, None, now_ns, true, false);
        self.reap(id, None);
        true
    }

    pub fn shutdown_write(&mut self, id: FlowId, now_ns: u64) -> bool {
        self.close_flow(id, now_ns)
    }

    pub fn handle_timer(&mut self, id: FlowId, generation: u32, now_ns: u64) -> bool {
        if self.flows.generation(id) != Some(generation) {
            return false;
        }
        let Some(flow) = self.flows.get(id) else {
            return false;
        };
        let deadlines = flow.deadlines;
        if deadlines
            .time_wait
            .is_some_and(|deadline| deadline <= now_ns)
        {
            self.flows.get_mut(id).unwrap().machine.expire_time_wait();
            self.reap(id, None);
            return true;
        }
        let mut delayed = None;
        if deadlines
            .delayed_ack
            .is_some_and(|deadline| deadline <= now_ns)
        {
            let flow = self.flows.get_mut(id).unwrap();
            flow.deadlines.delayed_ack = None;
            flow.ack_pending = 0;
            delayed = Some(ack_for(flow));
        }
        if let Some(transmit) = delayed {
            self.queue_transmit(id, transmit, None, now_ns, true, false);
        }
        let mut persist = None;
        if deadlines.persist.is_some_and(|deadline| deadline <= now_ns) {
            let flow = self.flows.get_mut(id).unwrap();
            flow.deadlines.persist = None;
            flow.persist_ns = flow.persist_ns.saturating_mul(2).min(MAX_RTO_NS);
            if let Some(segment) = flow.retransmit.front() {
                let sequence = segment.sequence;
                let stream_start = segment.stream_start;
                let payload_len = segment.payload_len;
                let flags = segment.flags;
                let payload = stream_start.and_then(|start| {
                    flow.facade
                        .retransmit_stream(start, usize::from(payload_len.min(1)))
                });
                let transmit = TcpTransmit {
                    sequence,
                    acknowledgement: flow.machine.receive_next(),
                    flags,
                    window: advertised_window(&flow.facade, flow.local_window_scale),
                };
                persist = Some((transmit, payload));
            } else if flow.facade.stream_unsent_len() != 0
                && let Some(payload) = flow.facade.take_stream_tx(1)
                && let Some(sequence) = flow.machine.reserve_send(1)
            {
                let transmit = TcpTransmit {
                    sequence,
                    acknowledgement: flow.machine.receive_next(),
                    flags: TcpFlags::ACK,
                    window: advertised_window(&flow.facade, flow.local_window_scale),
                };
                persist = Some((transmit, Some(payload)));
            }
            if flow.peer_window == 0
                && (flow.facade.stream_unsent_len() != 0 || !flow.retransmit.is_empty())
            {
                flow.deadlines.persist = Some(now_ns.saturating_add(flow.persist_ns));
            }
        }
        if let Some((transmit, payload)) = persist {
            let track = payload.as_ref().is_some_and(|payload| {
                self.flows.get(id).is_some_and(|flow| {
                    !flow
                        .retransmit
                        .iter()
                        .any(|segment| segment.stream_start == Some(payload.start))
                })
            });
            self.queue_transmit(id, transmit, payload, now_ns, true, track);
            if track && let Some(flow) = self.flows.get_mut(id) {
                flow.deadlines.retransmit = None;
                flow.deadlines.persist = Some(now_ns.saturating_add(flow.persist_ns));
            }
        }
        if deadlines
            .retransmit
            .is_some_and(|deadline| deadline <= now_ns)
        {
            self.retransmit_first(id, now_ns, false);
            if self.flows.get(id).is_none() {
                return true;
            }
        }
        if deadlines.cork.is_some_and(|deadline| deadline <= now_ns) {
            if let Some(flow) = self.flows.get_mut(id) {
                flow.deadlines.cork = None;
                flow.cork_force = true;
            }
            self.drain_send(id, now_ns);
        }
        if deadlines
            .keepalive
            .is_some_and(|deadline| deadline <= now_ns)
        {
            let expired = self
                .flows
                .get(id)
                .is_some_and(|flow| flow.keepalive_probes >= flow.facade.tcp_keepcount());
            if expired {
                self.reap(id, Some(SocketError::TimedOut));
                return true;
            }
            let transmit = {
                let flow = self.flows.get_mut(id).unwrap();
                flow.keepalive_probes = flow.keepalive_probes.saturating_add(1);
                flow.deadlines.keepalive =
                    Some(now_ns.saturating_add(flow.facade.tcp_keepintvl_ns()));
                TcpTransmit {
                    sequence: flow.machine.send_next().wrapping_sub(1),
                    acknowledgement: flow.machine.receive_next(),
                    flags: TcpFlags::ACK,
                    window: advertised_window(&flow.facade, flow.local_window_scale),
                }
            };
            self.queue_transmit(id, transmit, None, now_ns, true, false);
        }
        if deadlines
            .defer_accept
            .is_some_and(|deadline| deadline <= now_ns)
            && !self.promote_deferred(id, now_ns)
        {
            return true;
        }
        true
    }

    pub fn generation(&self, id: FlowId) -> Option<u32> {
        self.flows.generation(id)
    }

    pub fn earliest_deadline(&self, id: FlowId) -> Option<u64> {
        self.flows.get(id).and_then(TcpFlow::earliest_deadline)
    }

    pub fn take_output(&mut self) -> Option<PreparedTcpTx> {
        self.output.pop_front()
    }

    pub fn facade(&self, id: FlowId) -> Option<Arc<SocketFacade>> {
        self.flows.get(id).map(|flow| Arc::clone(&flow.facade))
    }

    pub fn record_control_error(
        &mut self,
        key: FlowKey,
        error: TransportControlError,
        _now_ns: u64,
    ) -> bool {
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let Some(id) = self.flows.find(&key, hash) else {
            return false;
        };
        match error {
            TransportControlError::NetworkUnreachable
                if self.flows.get(id).unwrap().machine.state() == TcpState::SynSent =>
            {
                self.reap(id, Some(SocketError::NetworkUnreachable));
            }
            TransportControlError::HostUnreachable
                if self.flows.get(id).unwrap().machine.state() == TcpState::SynSent =>
            {
                self.reap(id, Some(SocketError::HostUnreachable));
            }
            TransportControlError::PortUnreachable
                if self.flows.get(id).unwrap().machine.state() == TcpState::SynSent =>
            {
                self.reap(id, Some(SocketError::ConnectionRefused));
            }
            TransportControlError::PacketTooBig { mtu } => {
                let flow = self.flows.get_mut(id).unwrap();
                flow.peer_mss = flow.peer_mss.min(path_mss(mtu, flow.local.addr));
                flow.facade.set_transport_error(error, Some(flow.remote));
            }
            TransportControlError::NetworkUnreachable
            | TransportControlError::HostUnreachable
            | TransportControlError::TimeExceeded
            | TransportControlError::ParameterProblem => {
                self.flows
                    .get(id)
                    .unwrap()
                    .facade
                    .set_transport_error(error, Some(key.remote));
            }
            TransportControlError::PortUnreachable => {
                self.flows
                    .get(id)
                    .unwrap()
                    .facade
                    .set_transport_error(error, Some(key.remote));
            }
        }
        true
    }

    fn accept_syn(
        &mut self,
        interface: InterfaceId,
        path: TcpPath,
        key: FlowKey,
        tcp: TcpPacket,
        now_ns: u64,
        local_transport: bool,
    ) -> Result<FlowId, TcpIngressError> {
        let listener_key = self
            .find_listener_key(key.local, interface)
            .ok_or(TcpIngressError::NoEndpoint)?;
        let group = Arc::clone(&self.listeners.get(&listener_key).unwrap().group);
        if !group.reserve_syn() {
            return Err(TcpIngressError::FlowTableFull);
        }
        let Some(parent) = group.parent() else {
            group.release_syn();
            return Err(TcpIngressError::NoEndpoint);
        };
        let child = create_child_facade(parent.family()).map_err(|_| {
            group.release_syn();
            TcpIngressError::FlowTableFull
        })?;
        child.inherit_stack_generation(&parent);
        child.set_tcp_maxseg(parent.tcp_maxseg());
        child.set_tcp_defer_accept_ns(parent.tcp_defer_accept_ns());
        let mss = apply_user_mss(path_mss(path.route.mtu, key.local.addr), child.tcp_maxseg());
        let iss = self.initial_sequence(key, now_ns);
        let mut machine = TcpStateMachine::new(iss, advertised_window(&child, 0));
        machine.listen();
        let output = machine.on_segment(tcp.segment());
        let local_window_scale = choose_window_scale(child.receive_window_scale_limit());
        let initial_window = advertised_window(&child, local_window_scale);
        let flow = TcpFlow {
            facade: Arc::clone(&child),
            machine,
            path,
            remote: key.remote,
            local: key.local,
            pending_connect: None,
            accept_group: Some(Arc::clone(&group)),
            accept_reserved: false,
            retransmit: VecDeque::new(),
            unacknowledged_segments: 0,
            retransmitted_segments: 0,
            flight_bytes: 0,
            reassembly: Vec::new(),
            reassembly_bytes: 0,
            deadlines: TcpDeadlines::new(),
            rtt: RttEstimator::new(),
            congestion: CongestionControl::new(mss),
            peer_window: u32::from(tcp.window),
            peer_window_scale: tcp.options.window_scale.unwrap_or(0),
            local_window_scale,
            mss,
            peer_mss: tcp.options.maximum_segment_size.unwrap_or(mss).min(mss),
            ack_pending: 0,
            persist_ns: PERSIST_INITIAL_NS,
            timestamp_enabled: tcp.options.timestamp.is_some(),
            timestamp_recent: tcp.options.timestamp.map(|timestamp| timestamp.value),
            sack_permitted: tcp.options.sack_permitted,
            cork_force: false,
            close_requested: false,
            listener_key: Some(listener_key),
            last_activity_ns: now_ns,
            keepalive_probes: 0,
            last_advertised_window: initial_window,
            output_blocked: false,
            local_transport,
            local_peer_hint: None,
        };
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let id = self.flows.insert_prehashed(key, hash, flow).map_err(|_| {
            group.release_syn();
            TcpIngressError::FlowTableFull
        })?;
        publish_tcp_info(self.flows.get(id).unwrap());
        let generation = self.flows.generation(id).unwrap();
        child.publish_binding(
            OwnerRef::Flow {
                shard: self.shard,
                flow: id,
                generation: child.generation(),
            },
            key.local,
            Some(key.remote),
            Some(interface),
        );
        debug_assert!(generation != 0);
        if let Some(transmit) = output.transmit {
            self.queue_control(id, transmit, now_ns, true);
        }
        Ok(id)
    }

    #[cfg(test)]
    fn process_segment(
        &mut self,
        id: FlowId,
        tcp: TcpPacket,
        payload: Vec<u8>,
        now_ns: u64,
    ) -> Result<(), TcpIngressError> {
        self.process_segment_inner(id, tcp, IngressPayload::Owned(payload), now_ns, true)
    }

    fn process_segment_inner(
        &mut self,
        id: FlowId,
        tcp: TcpPacket,
        payload: IngressPayload<'_>,
        now_ns: u64,
        publish_info: bool,
    ) -> Result<(), TcpIngressError> {
        if tcp.flags.intersects(TcpFlags::FIN | TcpFlags::RST) {
            self.quiesce_local_pair(id, now_ns);
        }
        let (state_before, peer_window_before) = self
            .flows
            .get(id)
            .map(|flow| (flow.machine.state(), flow.peer_window))
            .ok_or(TcpIngressError::NoEndpoint)?;
        {
            let flow = self.flows.get_mut(id).unwrap();
            if state_before == TcpState::SynSent && tcp.flags.contains(TcpFlags::SYN) {
                flow.peer_mss = tcp
                    .options
                    .maximum_segment_size
                    .unwrap_or(flow.mss)
                    .min(flow.mss);
                flow.peer_window_scale = tcp.options.window_scale.unwrap_or(0);
                flow.timestamp_enabled = tcp.options.timestamp.is_some();
                flow.sack_permitted = tcp.options.sack_permitted;
            }
            if flow.timestamp_enabled {
                let Some(timestamp) = tcp.options.timestamp else {
                    return Err(TcpIngressError::Malformed);
                };
                if flow
                    .timestamp_recent
                    .is_some_and(|recent| (timestamp.value.wrapping_sub(recent) as i32) < 0)
                    && !tcp.flags.contains(TcpFlags::RST)
                {
                    self.stats.paws_drop = self.stats.paws_drop.saturating_add(1);
                    return Ok(());
                }
                flow.timestamp_recent = Some(timestamp.value);
            }
            flow.peer_window = if tcp.flags.contains(TcpFlags::SYN) {
                u32::from(tcp.window)
            } else {
                u32::from(tcp.window) << flow.peer_window_scale
            };
            if flow.sack_permitted {
                apply_sack(flow, &tcp.options.sack_blocks);
            }
            flow.last_activity_ns = now_ns;
            flow.keepalive_probes = 0;
            flow.deadlines.keepalive = flow
                .facade
                .tcp_keepalive_enabled()
                .then(|| now_ns.saturating_add(flow.facade.tcp_keepidle_ns()));
        }
        let peer_window_after = self.flows.get(id).unwrap().peer_window;
        let peer_window_changed = peer_window_after != peer_window_before;
        let peer_window_increased = peer_window_after > peer_window_before;

        let previous_una = self.flows.get(id).unwrap().machine.send_unacknowledged();
        let receive_before = self.flows.get(id).unwrap().machine.receive_next();
        if !payload.is_empty()
            && !tcp.flags.contains(TcpFlags::RST)
            && matches!(
                state_before,
                TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
            )
            && tcp.sequence.before_or_equal(receive_before)
        {
            let payload_start = tcp
                .sequence
                .before(receive_before)
                .then(|| receive_before.distance_from(tcp.sequence) as usize)
                .unwrap_or(0);
            let accepted = payload.len().saturating_sub(payload_start);
            if accepted > self.flows.get(id).unwrap().facade.stream_receive_window() {
                self.queue_ack(id, now_ns);
                return Ok(());
            }
        }
        let output = self
            .flows
            .get_mut(id)
            .unwrap()
            .machine
            .on_segment(tcp.segment());
        let current_una = self.flows.get(id).unwrap().machine.send_unacknowledged();
        if current_una.after(previous_una) {
            self.acknowledge(id, previous_una, current_una, now_ns);
            self.drain_send_with_info(id, now_ns, publish_info);
            self.maybe_send_fin(id, now_ns);
        } else if tcp.flags.contains(TcpFlags::ACK)
            && peer_window_increased
            && matches!(
                state_before,
                TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
            )
        {
            self.drain_send_with_info(id, now_ns, publish_info);
        } else if tcp.flags.contains(TcpFlags::ACK)
            && payload.is_empty()
            && !peer_window_changed
            && matches!(
                state_before,
                TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
            )
        {
            let fast = {
                let flow = self.flows.get_mut(id).unwrap();
                flow.congestion.duplicate_ack(
                    flow.flight_size(),
                    flow.mss,
                    flow.machine.send_next(),
                )
            };
            if fast {
                self.stats.fast_retransmit = self.stats.fast_retransmit.saturating_add(1);
                self.retransmit_first(id, now_ns, true);
            }
        }

        if !payload.is_empty() {
            self.receive_payload(id, receive_before, tcp.sequence, payload, now_ns)?;
            if !self.promote_deferred(id, now_ns) {
                return Ok(());
            }
        }
        if tcp.flags.contains(TcpFlags::FIN) {
            self.flows.get(id).unwrap().facade.publish_stream_eof();
        }
        if output.established && !self.on_established(id, now_ns) {
            return Ok(());
        }
        if let Some(transmit) = output.transmit {
            if transmit.flags.contains(TcpFlags::ACK)
                && let Some(flow) = self.flows.get_mut(id)
            {
                flow.ack_pending = 0;
                flow.deadlines.delayed_ack = None;
            }
            self.queue_control(id, transmit, now_ns, true);
        }
        let state = self.flows.get(id).unwrap().machine.state();
        if state == TcpState::TimeWait {
            self.flows.get_mut(id).unwrap().deadlines.time_wait =
                Some(now_ns.saturating_add(TIME_WAIT_NS));
        }
        if output.closed {
            self.stats.reset = self.stats.reset.saturating_add(1);
            let error = if state_before == TcpState::SynSent && tcp.flags.contains(TcpFlags::RST) {
                SocketError::ConnectionRefused
            } else {
                SocketError::ConnectionReset
            };
            self.reap(id, Some(error));
        } else if publish_info && let Some(flow) = self.flows.get(id) {
            publish_tcp_info(flow);
        }
        Ok(())
    }

    fn receive_payload(
        &mut self,
        id: FlowId,
        expected: TcpSequence,
        sequence: TcpSequence,
        mut payload: IngressPayload<'_>,
        now_ns: u64,
    ) -> Result<(), TcpIngressError> {
        let payload_start = sequence
            .before(expected)
            .then(|| expected.distance_from(sequence) as usize)
            .unwrap_or(0);
        if sequence.before_or_equal(expected) && payload_start < payload.len() {
            let accepted = payload.len() - payload_start;
            let commit = {
                let flow = self.flows.get_mut(id).unwrap();
                payload
                    .copy_to_socket(&flow.facade, payload_start)
                    .map_err(|_| TcpIngressError::ReceiveBufferFull)?
            };
            self.record_rx_commit(commit);
            let flow = self.flows.get_mut(id).unwrap();
            if sequence.before(expected) {
                flow.machine.advance_receive(accepted as u32);
            }
            self.drain_reassembly(id)?;
            let flow = self.flows.get_mut(id).unwrap();
            flow_ack_policy(flow, now_ns);
            return Ok(());
        }
        if sequence.after(expected) {
            let payload_len = payload.len();
            let flow = self.flows.get_mut(id).unwrap();
            if flow.reassembly.len() >= MAX_REASSEMBLY_FRAGMENTS
                || flow.reassembly_bytes.saturating_add(payload_len) > MAX_REASSEMBLY_BYTES
                || payload_len > flow.facade.stream_receive_window()
            {
                self.queue_ack(id, now_ns);
                return Ok(());
            }
            let payload = payload.into_vec()?;
            if insert_reassembly(flow, expected, sequence, payload) {
                self.stats.out_of_order = self.stats.out_of_order.saturating_add(1);
            }
            self.queue_ack(id, now_ns);
        }
        Ok(())
    }

    fn drain_reassembly(&mut self, id: FlowId) -> Result<(), TcpIngressError> {
        loop {
            let expected = self.flows.get(id).unwrap().machine.receive_next();
            if let Some(index) =
                self.flows
                    .get(id)
                    .unwrap()
                    .reassembly
                    .iter()
                    .position(|fragment| {
                        fragment.sequence.before(expected)
                            && expected.distance_from(fragment.sequence)
                                >= fragment.bytes.len() as u32
                    })
            {
                let fragment = self.flows.get_mut(id).unwrap().reassembly.remove(index);
                let flow = self.flows.get_mut(id).unwrap();
                flow.reassembly_bytes = flow.reassembly_bytes.saturating_sub(fragment.bytes.len());
                continue;
            }
            let index = self
                .flows
                .get(id)
                .unwrap()
                .reassembly
                .iter()
                .position(|fragment| {
                    fragment.sequence.before_or_equal(expected)
                        && expected.distance_from(fragment.sequence) < fragment.bytes.len() as u32
                });
            let Some(index) = index else {
                break;
            };
            let fragment = self.flows.get_mut(id).unwrap().reassembly.remove(index);
            let flow = self.flows.get_mut(id).unwrap();
            flow.reassembly_bytes = flow.reassembly_bytes.saturating_sub(fragment.bytes.len());
            let offset = expected.distance_from(fragment.sequence) as usize;
            let accepted = &fragment.bytes[offset..];
            flow.facade
                .push_stream_rx(accepted)
                .map_err(|_| TcpIngressError::ReceiveBufferFull)?;
            self.stats.rx_compact_copy_bytes = self
                .stats
                .rx_compact_copy_bytes
                .saturating_add(accepted.len() as u64);
            flow.machine.advance_receive(accepted.len() as u32);
        }
        Ok(())
    }

    fn record_rx_commit(&mut self, commit: StreamRxCommit) {
        let len = commit.len as u64;
        match commit.storage {
            StreamRxStorageKind::Discarded => {}
            StreamRxStorageKind::Compact => {
                self.stats.rx_compact_copy_bytes =
                    self.stats.rx_compact_copy_bytes.saturating_add(len);
            }
            StreamRxStorageKind::PhysicalPinned => {
                self.stats.rx_pinned_bytes = self.stats.rx_pinned_bytes.saturating_add(len);
            }
            StreamRxStorageKind::LoopbackShared => {
                self.stats.loopback_shared_bytes =
                    self.stats.loopback_shared_bytes.saturating_add(len);
            }
        }
        if commit.low_water_fallback {
            self.stats.rx_pool_low_water_fallbacks =
                self.stats.rx_pool_low_water_fallbacks.saturating_add(1);
        }
    }

    fn acknowledge(
        &mut self,
        id: FlowId,
        previous: TcpSequence,
        acknowledgement: TcpSequence,
        now_ns: u64,
    ) {
        let flow = self.flows.get_mut(id).unwrap();
        let mut acknowledged_payload = 0usize;
        let mut rtt_sample = None;
        while let Some(segment) = flow.retransmit.front_mut() {
            if acknowledgement.before_or_equal(segment.sequence) {
                break;
            }
            if acknowledgement.before(segment.end) {
                let acknowledged = acknowledgement.distance_from(segment.sequence) as usize;
                if !segment.sacked {
                    flow.flight_bytes = flow.flight_bytes.saturating_sub(acknowledged as u32);
                }
                if segment.payload_len != 0 {
                    let payload_ack = acknowledged.min(usize::from(segment.payload_len));
                    acknowledged_payload += payload_ack;
                    segment.sequence += payload_ack as u32;
                    segment.payload_len -= payload_ack as u16;
                    if let Some(start) = segment.stream_start.as_mut() {
                        *start = start.saturating_add(payload_ack as u64);
                    }
                }
                break;
            }
            let segment = flow.retransmit.pop_front().unwrap();
            if !segment.sacked {
                flow.unacknowledged_segments = flow.unacknowledged_segments.saturating_sub(1);
                if segment.transmissions > 1 {
                    flow.retransmitted_segments = flow.retransmitted_segments.saturating_sub(1);
                }
                flow.flight_bytes = flow
                    .flight_bytes
                    .saturating_sub(segment.end.distance_from(segment.sequence));
            }
            acknowledged_payload += usize::from(segment.payload_len);
            if segment.transmissions == 1 {
                rtt_sample = Some(now_ns.saturating_sub(segment.sent_ns));
            }
        }
        if acknowledged_payload != 0 {
            flow.facade.acknowledge_stream(acknowledged_payload);
        }
        if let Some(sample) = rtt_sample {
            flow.rtt.sample(sample);
        }
        flow.congestion.new_ack(
            acknowledgement.distance_from(previous),
            flow.mss,
            flow.machine.send_next(),
        );
        flow.deadlines.retransmit = flow
            .retransmit
            .front()
            .map(|segment| segment.sent_ns.saturating_add(flow.rtt.rto_ns));
        flow.deadlines.persist = None;
        flow.persist_ns = PERSIST_INITIAL_NS;
    }

    fn retransmit_first(&mut self, id: FlowId, now_ns: u64, fast: bool) {
        let timed_out = self.flows.get(id).is_some_and(|flow| {
            let timeout = flow.facade.tcp_user_timeout_ns();
            timeout != 0
                && flow
                    .retransmit
                    .iter()
                    .find(|segment| !segment.sacked)
                    .is_some_and(|segment| now_ns.saturating_sub(segment.first_sent_ns) >= timeout)
        });
        if timed_out {
            self.reap(id, Some(SocketError::TimedOut));
            return;
        }
        let syn_retries_exhausted = self.flows.get(id).is_some_and(|flow| {
            let Some(segment) = flow.retransmit.iter().find(|segment| !segment.sacked) else {
                return false;
            };
            if !segment.flags.contains(TcpFlags::SYN) {
                return false;
            }
            let limit = if flow.machine.state() == TcpState::SynSent {
                ACTIVE_SYN_RETRIES
            } else {
                PASSIVE_SYN_ACK_RETRIES
            };
            segment.transmissions > limit
        });
        if syn_retries_exhausted {
            self.reap(id, Some(SocketError::TimedOut));
            return;
        }
        let Some(flow) = self.flows.get_mut(id) else {
            return;
        };
        let Some(segment) = flow.retransmit.iter_mut().find(|segment| !segment.sacked) else {
            flow.deadlines.retransmit = None;
            return;
        };
        let payload = segment.stream_start.and_then(|start| {
            flow.facade
                .retransmit_stream(start, usize::from(segment.payload_len))
        });
        let transmit = TcpTransmit {
            sequence: segment.sequence,
            acknowledgement: flow.machine.receive_next(),
            flags: segment.flags,
            window: advertised_window(&flow.facade, flow.local_window_scale),
        };
        segment.sent_ns = now_ns;
        if segment.transmissions == 1 {
            flow.retransmitted_segments = flow.retransmitted_segments.saturating_add(1);
        }
        segment.transmissions = segment.transmissions.saturating_add(1);
        if !fast {
            flow.rtt.backoff();
            flow.congestion.ssthresh = (flow.flight_size() / 2).max(u32::from(flow.mss) * 2);
            flow.congestion.cwnd = u32::from(flow.mss);
        }
        flow.deadlines.retransmit = Some(now_ns.saturating_add(flow.rtt.rto_ns));
        self.stats.retransmitted = self.stats.retransmitted.saturating_add(1);
        flow.facade.record_tcp_retransmission();
        publish_tcp_info(flow);
        self.queue_transmit(id, transmit, payload, now_ns, true, false);
    }

    fn install_established_local_pair(&mut self, id: FlowId) {
        let Some(flow_generation) = self.flows.generation(id) else {
            return;
        };
        let Some(flow) = self.flows.get(id) else {
            return;
        };
        if !flow.local_transport
            || flow.machine.state() != TcpState::Established
            || flow.accept_reserved
            || flow.facade.is_closing()
        {
            return;
        }
        let Some(peer_key) = FlowKey::new(flow.local, flow.remote, TransportProtocol::Tcp) else {
            return;
        };
        let interface = flow.path.route.interface;
        let facade = Arc::clone(&flow.facade);
        let facade_generation = facade.generation();
        let stack_generation = facade.stack_generation();
        if stack_generation == 0 {
            return;
        }

        let peer_hash = flow_hash64(rss_hash(&self.rss_key, &peer_key));
        let Some(peer_id) = self.flows.find(&peer_key, peer_hash) else {
            return;
        };
        if peer_id == id {
            return;
        }
        let Some(peer_flow_generation) = self.flows.generation(peer_id) else {
            return;
        };
        let Some(peer) = self.flows.get(peer_id) else {
            return;
        };
        if !peer.local_transport
            || peer.machine.state() != TcpState::Established
            || peer.accept_reserved
            || peer.path.route.interface != interface
            || peer.local != peer_key.local
            || peer.remote != peer_key.remote
            || peer.facade.is_closing()
            || peer.facade.stack_generation() != stack_generation
        {
            return;
        }
        let peer_facade = Arc::clone(&peer.facade);
        let peer_facade_generation = peer_facade.generation();
        let facade_ready = facade.stream_unsent_len() == 0
            && !self
                .output
                .iter()
                .any(|pending| pending.flow == id && pending.payload.is_some());
        let peer_facade_ready = peer_facade.stream_unsent_len() == 0
            && !self
                .output
                .iter()
                .any(|pending| pending.flow == peer_id && pending.payload.is_some());

        self.flows.get_mut(id).unwrap().local_peer_hint = Some(LocalTcpPeerHint {
            flow: peer_id,
            flow_generation: peer_flow_generation,
            facade_generation: peer_facade_generation,
            stack_generation,
        });
        self.flows.get_mut(peer_id).unwrap().local_peer_hint = Some(LocalTcpPeerHint {
            flow: id,
            flow_generation,
            facade_generation,
            stack_generation,
        });
        if facade_ready {
            facade.install_local_tcp_direct_peer(&peer_facade);
        }
        if peer_facade_ready {
            peer_facade.install_local_tcp_direct_peer(&facade);
        }
    }

    fn quiesce_local_pair(&mut self, id: FlowId, now_ns: u64) {
        let Some(facade) = self.flows.get(id).map(|flow| Arc::clone(&flow.facade)) else {
            return;
        };
        let Some((peer_id, peer_facade)) = self.local_peer_facade(id) else {
            facade.clear_local_tcp_direct_route();
            return;
        };
        facade.clear_local_tcp_direct_route();
        peer_facade.clear_local_tcp_direct_route();
        let now_ns = now_ns.max(
            self.flows
                .get(id)
                .map_or(0, |flow| flow.last_activity_ns)
                .max(
                    self.flows
                        .get(peer_id)
                        .map_or(0, |flow| flow.last_activity_ns),
                ),
        );
        for (flow_id, flow_facade) in [(id, facade), (peer_id, peer_facade)] {
            let bytes = flow_facade.take_local_tcp_direct_pending();
            if bytes == 0 {
                continue;
            }
            assert!(
                self.reconcile_local_direct(flow_id, bytes, now_ns)
                    .is_some(),
                "本地 TCP route 失效前必须结算已交付字节"
            );
        }
    }

    fn on_established(&mut self, id: FlowId, now_ns: u64) -> bool {
        let flow = self.flows.get_mut(id).unwrap();
        flow.listener_key.take();
        flow.facade.publish_connected();
        flow.deadlines.keepalive = flow
            .facade
            .tcp_keepalive_enabled()
            .then(|| now_ns.saturating_add(flow.facade.tcp_keepidle_ns()));
        if let Some(sequence) = flow.pending_connect.take() {
            flow.facade.complete_control(sequence, Ok(()));
        }
        let defer_accept_ns = flow.facade.tcp_defer_accept_ns();
        let rejected = if defer_accept_ns != 0 {
            flow.accept_group.as_ref().is_some_and(|group| {
                if group.reserve_deferred() {
                    flow.accept_reserved = true;
                    flow.deadlines.defer_accept = Some(now_ns.saturating_add(defer_accept_ns));
                    false
                } else {
                    true
                }
            })
        } else {
            flow.accept_group.take().is_some_and(|group| {
                group
                    .publish_established(self.shard, Arc::clone(&flow.facade))
                    .is_err()
            })
        };
        if rejected {
            let transmit = TcpTransmit {
                sequence: flow.machine.send_next(),
                acknowledgement: flow.machine.receive_next(),
                flags: TcpFlags::RST | TcpFlags::ACK,
                window: 0,
            };
            self.queue_transmit(id, transmit, None, now_ns, true, false);
            self.reap(id, Some(SocketError::ConnectionReset));
            return false;
        }
        self.stats.established = self.stats.established.saturating_add(1);
        publish_tcp_info(self.flows.get(id).unwrap());
        self.install_established_local_pair(id);
        true
    }

    fn promote_deferred(&mut self, id: FlowId, now_ns: u64) -> bool {
        let Some(flow) = self.flows.get_mut(id) else {
            return false;
        };
        if !flow.accept_reserved {
            return true;
        }
        flow.accept_reserved = false;
        flow.deadlines.defer_accept = None;
        let group = flow
            .accept_group
            .take()
            .expect("deferred flow 必须保留 ListenGroup");
        if group
            .publish_deferred(self.shard, Arc::clone(&flow.facade))
            .is_ok()
        {
            return true;
        }
        let transmit = TcpTransmit {
            sequence: flow.machine.send_next(),
            acknowledgement: flow.machine.receive_next(),
            flags: TcpFlags::RST | TcpFlags::ACK,
            window: 0,
        };
        self.queue_transmit(id, transmit, None, now_ns, true, false);
        self.reap(id, Some(SocketError::ConnectionReset));
        false
    }

    fn maybe_send_fin(&mut self, id: FlowId, now_ns: u64) {
        let ready = self.flows.get(id).is_some_and(|flow| {
            flow.close_requested
                && flow.facade.stream_unsent_len() == 0
                && flow.retransmit.len() < MAX_RETRANSMIT_SEGMENTS
                && matches!(
                    flow.machine.state(),
                    TcpState::Established | TcpState::CloseWait
                )
        });
        if !ready {
            return;
        }
        let output = self.flows.get_mut(id).unwrap().machine.close();
        if let Some(transmit) = output.transmit {
            self.queue_transmit(id, transmit, None, now_ns, true, true);
        }
        if output.closed {
            self.reap(id, None);
        }
    }

    fn queue_ack(&mut self, id: FlowId, now_ns: u64) {
        let flow = self.flows.get_mut(id).unwrap();
        flow.ack_pending = 0;
        flow.deadlines.delayed_ack = None;
        let transmit = ack_for(flow);
        self.queue_transmit(id, transmit, None, now_ns, true, false);
    }

    fn queue_control(&mut self, id: FlowId, transmit: TcpTransmit, now_ns: u64, track: bool) {
        self.queue_transmit(id, transmit, None, now_ns, true, track);
    }

    fn queue_transmit(
        &mut self,
        id: FlowId,
        transmit: TcpTransmit,
        payload: Option<TcpTxLease>,
        now_ns: u64,
        low_latency: bool,
        track: bool,
    ) {
        if self.output.len() >= MAX_PENDING_OUTPUT && !low_latency {
            if let Some(flow) = self.flows.get(id) {
                flow.facade.set_pending_error(SocketError::WouldBlock);
            }
            return;
        }
        let flow_generation = self.flows.generation(id).unwrap();
        let flow = self.flows.get_mut(id).unwrap();
        let payload_len = payload.as_ref().map_or(0, |payload| payload.len);
        let sequence_len = u32::from(payload_len)
            + transmit.flags.contains(TcpFlags::SYN) as u32
            + transmit.flags.contains(TcpFlags::FIN) as u32;
        if track && sequence_len != 0 {
            assert!(
                flow.retransmit.len() < MAX_RETRANSMIT_SEGMENTS,
                "可确认 TCP 段必须在推进状态前预留重传账本"
            );
            flow.retransmit.push_back(SentSegment {
                sequence: transmit.sequence,
                end: transmit.sequence + sequence_len,
                stream_start: payload.as_ref().map(|payload| payload.start),
                payload_len,
                flags: transmit.flags,
                sent_ns: now_ns,
                first_sent_ns: now_ns,
                transmissions: 1,
                sacked: false,
            });
            flow.unacknowledged_segments = flow.unacknowledged_segments.saturating_add(1);
            flow.flight_bytes = flow.flight_bytes.saturating_add(sequence_len);
            flow.deadlines
                .retransmit
                .get_or_insert(now_ns.saturating_add(flow.rtt.rto_ns));
        }
        let (options, options_len, parsed_options) = wire_options(flow, transmit.flags, now_ns);
        let advertised = advertised_window(&flow.facade, flow.local_window_scale);
        let wire_window = if transmit.flags.contains(TcpFlags::RST) {
            transmit.window
        } else {
            advertised
        };
        flow.last_advertised_window = wire_window;
        let completion = self.next_completion;
        self.next_completion = self.next_completion.wrapping_add(1).max(1);
        self.output.push_back(PreparedTcpTx {
            flow: id,
            flow_generation,
            facade_generation: flow.facade.generation(),
            facade: Arc::clone(&flow.facade),
            payload,
            path: flow.path,
            remote: flow.remote,
            local_port: flow.local.port,
            sequence: transmit.sequence,
            acknowledgement: transmit.acknowledgement,
            flags: transmit.flags,
            window: wire_window,
            options,
            options_len,
            parsed_options,
            completion,
            low_latency,
        });
    }

    fn find_listener_key(
        &self,
        local: Endpoint,
        interface: InterfaceId,
    ) -> Option<(IpAddr, u16, Option<InterfaceId>)> {
        let unspecified = unspecified_address(address_family(local.addr));
        [
            (local.addr, local.port, Some(interface)),
            (local.addr, local.port, None),
            (unspecified, local.port, Some(interface)),
            (unspecified, local.port, None),
        ]
        .into_iter()
        .find(|key| self.listeners.contains_key(key))
    }

    fn initial_sequence(&self, key: FlowKey, now_ns: u64) -> TcpSequence {
        let mut bytes = [0u8; 36];
        let len = encode_flow_key(key, &mut bytes);
        let hash = siphash24(self.isn_key, &bytes[..len]);
        TcpSequence((hash as u32).wrapping_add((now_ns / 64) as u32))
    }

    fn reap(&mut self, id: FlowId, error: Option<SocketError>) {
        self.quiesce_local_pair(id, 0);
        let Some(key) = self.flows.key(id) else {
            return;
        };
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        if let Some(flow) = self.flows.remove(&key, hash) {
            if let Some(group) = flow.accept_group {
                if flow.accept_reserved {
                    group.release_deferred();
                } else if flow.listener_key.is_some() {
                    group.release_syn();
                }
            }
            if let Some(sequence) = flow.pending_connect {
                let result = Err(error.unwrap_or(SocketError::Closed));
                flow.facade.complete_control(sequence, result);
            }
            if let Some(error) = error {
                flow.facade.publish_connection_error(error);
            }
            flow.facade.update_tcp_info(
                7,
                to_micros(flow.rtt.rto_ns),
                flow.rtt.smoothed_ns.map_or(0, to_micros),
                to_micros(flow.rtt.variance_ns),
                u32::from(flow.peer_mss),
                flow.congestion.cwnd,
                flow.congestion.ssthresh,
                0,
                0,
            );
            flow.facade.abort_stream_tx();
            flow.facade.publish_closed();
        }
    }
}

fn flow_ack_policy(flow: &mut TcpFlow, now_ns: u64) {
    flow.ack_pending = flow.ack_pending.saturating_add(1);
    if flow.ack_pending >= 2 || flow.facade.take_quick_ack() {
        flow.deadlines.delayed_ack = Some(now_ns);
    } else {
        flow.deadlines
            .delayed_ack
            .get_or_insert(now_ns.saturating_add(DELAYED_ACK_NS));
    }
}

fn insert_reassembly(
    flow: &mut TcpFlow,
    expected: TcpSequence,
    sequence: TcpSequence,
    mut bytes: Vec<u8>,
) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let previous_bytes = flow.reassembly_bytes;
    let mut start = sequence.distance_from(expected) as usize;
    let mut merged_sequence = sequence;
    let mut index = 0usize;
    while index < flow.reassembly.len() {
        let fragment = &flow.reassembly[index];
        let fragment_start = fragment.sequence.distance_from(expected) as usize;
        let fragment_end = fragment_start.saturating_add(fragment.bytes.len());
        let end = start.saturating_add(bytes.len());
        if fragment_end < start {
            index += 1;
            continue;
        }
        if end < fragment_start {
            break;
        }

        let union_start = start.min(fragment_start);
        let union_end = end.max(fragment_end);
        let mut union = alloc::vec![0; union_end - union_start];
        let old_offset = fragment_start - union_start;
        union[old_offset..old_offset + fragment.bytes.len()].copy_from_slice(&fragment.bytes);
        let new_offset = start - union_start;
        union[new_offset..new_offset + bytes.len()].copy_from_slice(&bytes);
        let removed = flow.reassembly.remove(index);
        flow.reassembly_bytes = flow.reassembly_bytes.saturating_sub(removed.bytes.len());
        start = union_start;
        merged_sequence = expected + start as u32;
        bytes = union;
    }
    flow.reassembly_bytes = flow.reassembly_bytes.saturating_add(bytes.len());
    flow.reassembly.insert(
        index,
        ReassemblyFragment {
            sequence: merged_sequence,
            bytes,
        },
    );
    flow.reassembly_bytes > previous_bytes
}

fn ack_for(flow: &TcpFlow) -> TcpTransmit {
    TcpTransmit {
        sequence: flow.machine.send_next(),
        acknowledgement: flow.machine.receive_next(),
        flags: TcpFlags::ACK,
        window: advertised_window(&flow.facade, flow.local_window_scale),
    }
}

fn apply_sack(flow: &mut TcpFlow, blocks: &[Option<TcpSackBlock>; 4]) {
    let mut newly_sacked = 0u32;
    let mut newly_sacked_segments = 0u32;
    let mut newly_sacked_retransmitted = 0u32;
    for block in blocks.iter().flatten() {
        for segment in &mut flow.retransmit {
            if segment.sequence.after_or_equal(block.left)
                && segment.end.before_or_equal(block.right)
                && !segment.sacked
            {
                segment.sacked = true;
                newly_sacked_segments = newly_sacked_segments.saturating_add(1);
                if segment.transmissions > 1 {
                    newly_sacked_retransmitted = newly_sacked_retransmitted.saturating_add(1);
                }
                newly_sacked =
                    newly_sacked.saturating_add(segment.end.distance_from(segment.sequence));
            }
        }
    }
    flow.flight_bytes = flow.flight_bytes.saturating_sub(newly_sacked);
    flow.unacknowledged_segments = flow
        .unacknowledged_segments
        .saturating_sub(newly_sacked_segments);
    flow.retransmitted_segments = flow
        .retransmitted_segments
        .saturating_sub(newly_sacked_retransmitted);
}

fn wire_options(
    flow: &TcpFlow,
    flags: TcpFlags,
    now_ns: u64,
) -> ([u8; 40], u8, crate::transport::TcpOptions) {
    let mut options = [0u8; 40];
    let mut parsed = crate::transport::TcpOptions::default();
    let mut len = 0usize;
    if flags.contains(TcpFlags::SYN) {
        options[len..len + 4].copy_from_slice(&[2, 4, 0, 0]);
        options[len + 2..len + 4].copy_from_slice(&flow.mss.to_be_bytes());
        parsed.maximum_segment_size = Some(flow.mss);
        len += 4;
        options[len..len + 4].copy_from_slice(&[1, 3, 3, flow.local_window_scale]);
        parsed.window_scale = Some(flow.local_window_scale);
        len += 4;
        options[len..len + 4].copy_from_slice(&[1, 1, 4, 2]);
        parsed.sack_permitted = true;
        len += 4;
    }
    if flow.timestamp_enabled || flags.contains(TcpFlags::SYN) {
        options[len..len + 2].copy_from_slice(&[1, 1]);
        len += 2;
        options[len..len + 2].copy_from_slice(&[8, 10]);
        let value = (now_ns / 1_000_000) as u32;
        let echo_reply = flow.timestamp_recent.unwrap_or(0);
        options[len + 2..len + 6].copy_from_slice(&value.to_be_bytes());
        options[len + 6..len + 10].copy_from_slice(&echo_reply.to_be_bytes());
        parsed.timestamp = Some(crate::transport::TcpTimestamp { value, echo_reply });
        len += 10;
    }
    if !flags.contains(TcpFlags::SYN) && flow.sack_permitted && !flow.reassembly.is_empty() {
        let count = flow
            .reassembly
            .len()
            .min(3)
            .min((40usize.saturating_sub(len + 2)) / 8);
        if count != 0 {
            options[len] = 5;
            options[len + 1] = (2 + count * 8) as u8;
            len += 2;
            for (index, fragment) in flow.reassembly.iter().take(count).enumerate() {
                options[len..len + 4].copy_from_slice(&fragment.sequence.0.to_be_bytes());
                let right = fragment.sequence + fragment.bytes.len() as u32;
                options[len + 4..len + 8].copy_from_slice(&right.0.to_be_bytes());
                parsed.sack_blocks[index] = Some(crate::transport::TcpSackBlock {
                    left: fragment.sequence,
                    right,
                });
                len += 8;
            }
        }
    }
    while len % 4 != 0 {
        options[len] = 1;
        len += 1;
    }
    (options, len as u8, parsed)
}

fn advertised_window(facade: &SocketFacade, scale: u8) -> u16 {
    (facade.stream_receive_window() >> scale).min(u16::MAX as usize) as u16
}

fn choose_window_scale(limit: usize) -> u8 {
    let mut scale = 0u8;
    while scale < 14 && (u16::MAX as usize) << scale < limit {
        scale += 1;
    }
    scale
}

fn apply_user_mss(path_mss: u16, requested: u16) -> u16 {
    if requested == 0 {
        path_mss
    } else {
        path_mss.min(requested)
    }
}

fn to_micros(value_ns: u64) -> u32 {
    (value_ns / 1_000).min(u64::from(u32::MAX)) as u32
}

fn linux_tcp_state(state: TcpState) -> u8 {
    match state {
        TcpState::Established => 1,
        TcpState::SynSent => 2,
        TcpState::SynReceived => 3,
        TcpState::FinWait1 => 4,
        TcpState::FinWait2 => 5,
        TcpState::TimeWait => 6,
        TcpState::Closed => 7,
        TcpState::CloseWait => 8,
        TcpState::LastAck => 9,
        TcpState::Listen => 10,
        TcpState::Closing => 11,
    }
}

fn publish_tcp_info(flow: &TcpFlow) {
    debug_assert_eq!(
        flow.unacknowledged_segments,
        flow.retransmit
            .iter()
            .filter(|segment| !segment.sacked)
            .count() as u32
    );
    debug_assert_eq!(
        flow.retransmitted_segments,
        flow.retransmit
            .iter()
            .filter(|segment| segment.transmissions > 1 && !segment.sacked)
            .count() as u32
    );
    flow.facade.update_tcp_info(
        linux_tcp_state(flow.machine.state()),
        to_micros(flow.rtt.rto_ns),
        flow.rtt.smoothed_ns.map_or(0, to_micros),
        to_micros(flow.rtt.variance_ns),
        u32::from(flow.peer_mss),
        flow.congestion.cwnd,
        flow.congestion.ssthresh,
        flow.unacknowledged_segments,
        flow.retransmitted_segments,
    );
    #[cfg(feature = "performance-profile")]
    {
        profiling::observe(
            profiling::Metric::TcpSendAllowance,
            u64::from(flow.send_allowance()),
        );
        profiling::observe(
            profiling::Metric::TcpFlightBytes,
            u64::from(flow.flight_bytes),
        );
        profiling::observe(
            profiling::Metric::TcpPeerWindow,
            u64::from(flow.peer_window),
        );
        profiling::observe(
            profiling::Metric::TcpCongestionWindow,
            u64::from(flow.congestion.cwnd),
        );
        profiling::observe(
            profiling::Metric::TcpUnacknowledgedSegments,
            u64::from(flow.unacknowledged_segments),
        );
        profiling::observe(
            profiling::Metric::TcpRetransmittedSegments,
            u64::from(flow.retransmitted_segments),
        );
        profiling::observe(
            profiling::Metric::TcpStreamUnsentBytes,
            flow.facade.stream_unsent_len() as u64,
        );

        if flow.facade.tcp_profile_trace_due() {
            let socket = flow.facade.id().counter;
            profiling::trace_point(
                profiling::Event::NetTcpSequence,
                socket,
                (u64::from(flow.machine.send_unacknowledged().0) << 32)
                    | u64::from(flow.machine.send_next().0),
            );
            profiling::trace_point(
                profiling::Event::NetTcpReceiveSequence,
                socket,
                (u64::from(flow.machine.receive_next().0) << 32)
                    | u64::from(flow.last_advertised_window),
            );
            profiling::trace_point(
                profiling::Event::NetTcpWindow,
                socket,
                (u64::from(flow.peer_window) << 32) | u64::from(flow.congestion.cwnd),
            );
        }
    }
}

fn path_mss(mtu: u32, address: IpAddr) -> u16 {
    let header = match address {
        IpAddr::V4(_) => 40,
        IpAddr::V6(_) => 60,
    };
    mtu.min(u32::from(u16::MAX)).saturating_sub(header).max(536) as u16
}

fn effective_payload_mss(mtu: u32, address: IpAddr, peer_mss: u16, options_len: u8) -> u16 {
    let ip_header = match address {
        IpAddr::V4(_) => 20,
        IpAddr::V6(_) => 40,
    };
    let path_limit = mtu
        .min(u32::from(u16::MAX))
        .saturating_sub(ip_header + 20 + u32::from(options_len));
    peer_mss.min(path_limit.max(1) as u16)
}

fn address_family(address: IpAddr) -> AddressFamily {
    match address {
        IpAddr::V4(_) => AddressFamily::Ipv4,
        IpAddr::V6(_) => AddressFamily::Ipv6,
    }
}

fn unspecified_address(family: AddressFamily) -> IpAddr {
    match family {
        AddressFamily::Ipv4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        AddressFamily::Ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

fn create_child_facade(family: AddressFamily) -> Result<Arc<SocketFacade>, SocketError> {
    #[cfg(not(test))]
    {
        new_tcp_socket_facade(family)
    }
    #[cfg(test)]
    {
        Ok(Arc::new(SocketFacade::new(
            crate::SocketId {
                boot_nonce: 1,
                counter: NEXT_TEST_SOCKET.fetch_add(1, Ordering::Relaxed),
            },
            family,
            crate::SocketKind::Stream,
        )))
    }
}

fn encode_flow_key(key: FlowKey, output: &mut [u8; 36]) -> usize {
    match (key.remote.addr, key.local.addr) {
        (IpAddr::V4(remote), IpAddr::V4(local)) => {
            output[0..4].copy_from_slice(&remote.0);
            output[4..8].copy_from_slice(&local.0);
            output[8..10].copy_from_slice(&key.remote.port.to_be_bytes());
            output[10..12].copy_from_slice(&key.local.port.to_be_bytes());
            12
        }
        (IpAddr::V6(remote), IpAddr::V6(local)) => {
            output[0..16].copy_from_slice(&remote.0);
            output[16..32].copy_from_slice(&local.0);
            output[32..34].copy_from_slice(&key.remote.port.to_be_bytes());
            output[34..36].copy_from_slice(&key.local.port.to_be_bytes());
            36
        }
        _ => 0,
    }
}

fn siphash24(key: [u8; 16], input: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let k1 = u64::from_le_bytes(key[8..16].try_into().unwrap());
    let mut v0 = 0x736f_6d65_7073_6575 ^ k0;
    let mut v1 = 0x646f_7261_6e64_6f6d ^ k1;
    let mut v2 = 0x6c79_6765_6e65_7261 ^ k0;
    let mut v3 = 0x7465_6462_7974_6573 ^ k1;
    let mut chunks = input.chunks_exact(8);
    for chunk in &mut chunks {
        let message = u64::from_le_bytes(chunk.try_into().unwrap());
        v3 ^= message;
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= message;
    }
    let mut tail = (input.len() as u64) << 56;
    for (index, byte) in chunks.remainder().iter().enumerate() {
        tail |= u64::from(*byte) << (index * 8);
    }
    v3 ^= tail;
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= tail;
    v2 ^= 0xff;
    for _ in 0..4 {
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    }
    v0 ^ v1 ^ v2 ^ v3
}

fn sip_round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    *v0 = v0.wrapping_add(*v1);
    *v1 = v1.rotate_left(13) ^ *v0;
    *v0 = v0.rotate_left(32);
    *v2 = v2.wrapping_add(*v3);
    *v3 = v3.rotate_left(16) ^ *v2;
    *v0 = v0.wrapping_add(*v3);
    *v3 = v3.rotate_left(21) ^ *v0;
    *v2 = v2.wrapping_add(*v1);
    *v1 = v1.rotate_left(17) ^ *v2;
    *v2 = v2.rotate_left(32);
}

pub fn build_tcp_packet(
    mut payload: PacketChain,
    work: &PreparedTcpTx,
    checksum_offload: bool,
) -> Result<PacketChain, PacketChain> {
    let options_len = usize::from(work.options_len);
    let tcp_header_len = 20 + options_len;
    let payload_len = payload.total_len();
    let ip_header_len = match work.path.route.source {
        IpAddr::V4(_) => 20,
        IpAddr::V6(_) => 40,
    };
    let header_len = 14 + ip_header_len + tcp_header_len;
    if payload.prepend_first_zeroed(header_len as u16).is_err() {
        return Err(payload);
    }
    let mut ethernet = [0u8; 14];
    ethernet[0..6].copy_from_slice(&work.path.destination_mac);
    ethernet[6..12].copy_from_slice(&work.path.source_mac);
    let mut tcp = [0u8; 60];
    tcp[0..2].copy_from_slice(&work.local_port.to_be_bytes());
    tcp[2..4].copy_from_slice(&work.remote.port.to_be_bytes());
    tcp[4..8].copy_from_slice(&work.sequence.0.to_be_bytes());
    tcp[8..12].copy_from_slice(&work.acknowledgement.0.to_be_bytes());
    tcp[12] = ((tcp_header_len / 4) as u8) << 4 | u8::from(work.flags.contains(TcpFlags::NS));
    tcp[13] = work.flags.bits() as u8;
    tcp[14..16].copy_from_slice(&work.window.to_be_bytes());
    tcp[20..20 + options_len].copy_from_slice(&work.options[..options_len]);

    match (work.path.route.source, work.remote.addr) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            ethernet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let total_len = 20 + tcp_header_len + payload_len;
            if total_len > u16::MAX as usize {
                return Err(payload);
            }
            let mut ip = [0u8; 20];
            ip[0] = 0x45;
            ip[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
            ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            ip[8] = 64;
            ip[9] = TCP_PROTOCOL_NUMBER;
            ip[12..16].copy_from_slice(&source.0);
            ip[16..20].copy_from_slice(&destination.0);
            let checksum = crate::pipeline::checksum_bytes(&ip);
            ip[10..12].copy_from_slice(&checksum.to_be_bytes());
            if payload.copy_in(0, &ethernet).is_err()
                || payload.copy_in(14, &ip).is_err()
                || payload.copy_in(34, &tcp[..tcp_header_len]).is_err()
            {
                return Err(payload);
            }
            let checksum = if checksum_offload {
                let Ok(checksum) = partial_transport_checksum(
                    work.path.route.source,
                    work.remote.addr,
                    tcp_header_len + payload_len,
                    TCP_PROTOCOL_NUMBER,
                ) else {
                    return Err(payload);
                };
                checksum
            } else {
                match transport_checksum(
                    &payload,
                    34,
                    tcp_header_len + payload_len,
                    work.path.route.source,
                    work.remote.addr,
                    TCP_PROTOCOL_NUMBER,
                ) {
                    Ok(checksum) => checksum,
                    Err(_) => return Err(payload),
                }
            };
            if payload.copy_in(50, &checksum.to_be_bytes()).is_err() {
                return Err(payload);
            }
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            ethernet[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let transport_len = tcp_header_len + payload_len;
            if transport_len > u16::MAX as usize {
                return Err(payload);
            }
            let mut ip = [0u8; 40];
            ip[0] = 0x60;
            ip[4..6].copy_from_slice(&(transport_len as u16).to_be_bytes());
            ip[6] = TCP_PROTOCOL_NUMBER;
            ip[7] = 64;
            ip[8..24].copy_from_slice(&source.0);
            ip[24..40].copy_from_slice(&destination.0);
            if payload.copy_in(0, &ethernet).is_err()
                || payload.copy_in(14, &ip).is_err()
                || payload.copy_in(54, &tcp[..tcp_header_len]).is_err()
            {
                return Err(payload);
            }
            let checksum = if checksum_offload {
                let Ok(checksum) = partial_transport_checksum(
                    work.path.route.source,
                    work.remote.addr,
                    transport_len,
                    TCP_PROTOCOL_NUMBER,
                ) else {
                    return Err(payload);
                };
                checksum
            } else {
                match transport_checksum(
                    &payload,
                    54,
                    transport_len,
                    work.path.route.source,
                    work.remote.addr,
                    TCP_PROTOCOL_NUMBER,
                ) {
                    Ok(checksum) => checksum,
                    Err(_) => return Err(payload),
                }
            };
            if payload.copy_in(70, &checksum.to_be_bytes()).is_err() {
                return Err(payload);
            }
        }
        _ => return Err(payload),
    }
    Ok(payload)
}

pub fn build_tcp_reset(
    mut packet: PacketChain,
    ethernet: EthernetHeader,
    ip: IpPacket,
    tcp: TcpPacket,
) -> Result<PacketChain, PacketChain> {
    if tcp.flags.contains(TcpFlags::RST) {
        return Err(packet);
    }
    let mut ethernet_header = [0u8; 14];
    ethernet_header[0..6].copy_from_slice(&ethernet.source);
    ethernet_header[6..12].copy_from_slice(&ethernet.destination);
    let mut tcp_header = [0u8; 20];
    tcp_header[0..2].copy_from_slice(&tcp.destination_port.to_be_bytes());
    tcp_header[2..4].copy_from_slice(&tcp.source_port.to_be_bytes());
    let flags = if tcp.flags.contains(TcpFlags::ACK) {
        tcp_header[4..8].copy_from_slice(&tcp.acknowledgement.0.to_be_bytes());
        TcpFlags::RST
    } else {
        tcp_header[8..12].copy_from_slice(&(tcp.sequence + tcp.sequence_len()).0.to_be_bytes());
        TcpFlags::RST | TcpFlags::ACK
    };
    tcp_header[12] = 5 << 4;
    tcp_header[13] = flags.bits() as u8;

    match (ip.source, ip.destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            ethernet_header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let mut ip_header = [0u8; 20];
            ip_header[0] = 0x45;
            ip_header[2..4].copy_from_slice(&40u16.to_be_bytes());
            ip_header[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            ip_header[8] = 64;
            ip_header[9] = TCP_PROTOCOL_NUMBER;
            ip_header[12..16].copy_from_slice(&destination.0);
            ip_header[16..20].copy_from_slice(&source.0);
            let checksum = crate::pipeline::checksum_bytes(&ip_header);
            ip_header[10..12].copy_from_slice(&checksum.to_be_bytes());
            if packet.copy_in(0, &ethernet_header).is_err()
                || packet.copy_in(14, &ip_header).is_err()
                || packet.copy_in(34, &tcp_header).is_err()
            {
                return Err(packet);
            }
            let checksum = match transport_checksum(
                &packet,
                34,
                20,
                IpAddr::V4(destination),
                IpAddr::V4(source),
                TCP_PROTOCOL_NUMBER,
            ) {
                Ok(checksum) => checksum,
                Err(_) => return Err(packet),
            };
            if packet.copy_in(50, &checksum.to_be_bytes()).is_err() {
                return Err(packet);
            }
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            ethernet_header[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let mut ip_header = [0u8; 40];
            ip_header[0] = 0x60;
            ip_header[4..6].copy_from_slice(&20u16.to_be_bytes());
            ip_header[6] = TCP_PROTOCOL_NUMBER;
            ip_header[7] = 64;
            ip_header[8..24].copy_from_slice(&destination.0);
            ip_header[24..40].copy_from_slice(&source.0);
            if packet.copy_in(0, &ethernet_header).is_err()
                || packet.copy_in(14, &ip_header).is_err()
                || packet.copy_in(54, &tcp_header).is_err()
            {
                return Err(packet);
            }
            let checksum = match transport_checksum(
                &packet,
                54,
                20,
                IpAddr::V6(destination),
                IpAddr::V6(source),
                TCP_PROTOCOL_NUMBER,
            ) {
                Ok(checksum) => checksum,
                Err(_) => return Err(packet),
            };
            if packet.copy_in(70, &checksum.to_be_bytes()).is_err() {
                return Err(packet);
            }
        }
        _ => return Err(packet),
    }
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::ptr::NonNull;
    use std::thread;

    use super::*;
    use crate::buf::{NetBufPool, NetBufStorage};

    struct TestStorage {
        bytes: Box<[u8]>,
    }

    impl NetBufStorage for TestStorage {
        fn capacity(&self) -> usize {
            self.bytes.len()
        }

        fn base_ptr(&self) -> NonNull<u8> {
            NonNull::new(self.bytes.as_ptr() as *mut u8).unwrap()
        }

        fn dma_addr(&self) -> Option<u64> {
            None
        }

        fn sync_for_cpu(&self, _offset: usize, _len: usize) {}
        fn sync_for_device(&self, _offset: usize, _len: usize) {}
    }

    fn empty_packet(len: usize) -> (crate::buf::NetBufPoolOwner, PacketChain) {
        let storage = alloc::vec![Box::new(TestStorage {
            bytes: alloc::vec![0; len].into_boxed_slice(),
        }) as Box<dyn NetBufStorage>]
        .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        let lease = owner
            .lease(0, len as u16, PacketMetadata::default())
            .unwrap();
        (owner, PacketChain::from_lease(lease))
    }

    fn facade(counter: u64) -> Arc<SocketFacade> {
        Arc::new(SocketFacade::new(
            crate::SocketId {
                boot_nonce: 1,
                counter,
            },
            AddressFamily::Ipv4,
            crate::SocketKind::Stream,
        ))
    }

    fn listen_group(
        listener: &Arc<SocketFacade>,
        id: u64,
        shard_count: usize,
        backlog: u32,
    ) -> Arc<ListenGroup> {
        let group = ListenGroup::new(crate::ListenGroupId(id), listener, shard_count, backlog);
        listener.install_listen_group(Arc::clone(&group));
        group
    }

    fn path(local: IpAddr, remote: IpAddr) -> TcpPath {
        TcpPath {
            route: RouteDecision {
                interface: InterfaceId(1),
                source: local,
                next_hop: remote,
                mtu: 1500,
                table: 0,
            },
            source_mac: [2; 6],
            destination_mac: [1; 6],
            unresolved_neighbor: None,
            config_generation: 0,
        }
    }

    fn packet(
        source_port: u16,
        destination_port: u16,
        sequence: u32,
        acknowledgement: u32,
        flags: TcpFlags,
        payload_len: u32,
    ) -> TcpPacket {
        TcpPacket {
            source_port,
            destination_port,
            sequence: TcpSequence(sequence),
            acknowledgement: TcpSequence(acknowledgement),
            flags,
            window: u16::MAX,
            urgent_pointer: 0,
            header_len: 20,
            payload_offset: 0,
            payload_len,
            options: crate::transport::TcpOptions::default(),
        }
    }

    struct LocalPair {
        listener: Arc<SocketFacade>,
        client: Arc<SocketFacade>,
        server: Arc<SocketFacade>,
        client_flow: FlowId,
        server_flow: FlowId,
    }

    fn establish_local_pair(
        table: &mut TcpEndpointTable,
        receive_limit: Option<usize>,
    ) -> LocalPair {
        establish_local_pair_with_defer(table, receive_limit, 0)
    }

    fn establish_local_pair_with_defer(
        table: &mut TcpEndpointTable,
        receive_limit: Option<usize>,
        defer_accept_ns: u64,
    ) -> LocalPair {
        let server_endpoint = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9_100,
        };
        let client_endpoint = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 41_000,
        };
        let listener = facade(90);
        listener.test_set_stack_generation(1);
        listener.set_tcp_defer_accept_ns(defer_accept_ns);
        let group = listen_group(&listener, 90, 1, 4);
        table
            .listen(server_endpoint, Some(InterfaceId(1)), group)
            .unwrap();

        let client = facade(91);
        client.test_set_stack_generation(1);
        let client_flow = table
            .connect(
                client_endpoint,
                server_endpoint,
                path(client_endpoint.addr, server_endpoint.addr),
                Arc::clone(&client),
                1,
                true,
                1_000,
            )
            .unwrap();
        client.publish_binding(
            OwnerRef::Flow {
                shard: ShardId(0),
                flow: client_flow,
                generation: client.generation(),
            },
            client_endpoint,
            Some(server_endpoint),
            Some(InterfaceId(1)),
        );

        let syn = table.take_output().unwrap();
        let server_key =
            FlowKey::new(client_endpoint, server_endpoint, TransportProtocol::Tcp).unwrap();
        let server_flow = table
            .ingest_local(
                InterfaceId(1),
                path(server_endpoint.addr, client_endpoint.addr),
                server_key,
                packet(
                    client_endpoint.port,
                    server_endpoint.port,
                    syn.sequence.0,
                    syn.acknowledgement.0,
                    syn.flags,
                    0,
                ),
                None,
                2_000,
            )
            .unwrap();
        let syn_ack = table.take_output().unwrap();
        let client_key =
            FlowKey::new(server_endpoint, client_endpoint, TransportProtocol::Tcp).unwrap();
        table
            .ingest_local(
                InterfaceId(1),
                path(client_endpoint.addr, server_endpoint.addr),
                client_key,
                packet(
                    server_endpoint.port,
                    client_endpoint.port,
                    syn_ack.sequence.0,
                    syn_ack.acknowledgement.0,
                    syn_ack.flags,
                    0,
                ),
                None,
                3_000,
            )
            .unwrap();
        let ack = table.take_output().unwrap();
        table
            .ingest_local(
                InterfaceId(1),
                path(server_endpoint.addr, client_endpoint.addr),
                server_key,
                packet(
                    client_endpoint.port,
                    server_endpoint.port,
                    ack.sequence.0,
                    ack.acknowledgement.0,
                    ack.flags,
                    0,
                ),
                None,
                4_000,
            )
            .unwrap();
        let server = if defer_accept_ns == 0 {
            listener.accept(true, None).unwrap()
        } else {
            Arc::clone(&table.flows.get(server_flow).unwrap().facade)
        };
        if let Some(limit) = receive_limit {
            server.set_buffer_limits(None, Some(limit));
        }
        assert_eq!(
            server_flow,
            match server.owner() {
                OwnerRef::Flow { flow, .. } => flow,
                _ => panic!("被动连接必须绑定 flow"),
            }
        );
        LocalPair {
            listener,
            client,
            server,
            client_flow,
            server_flow,
        }
    }

    fn ingest_prepared_local_work(
        table: &mut TcpEndpointTable,
        work: &PreparedTcpTx,
        now_ns: u64,
    ) -> Result<FlowId, TcpIngressError> {
        let source = Endpoint {
            addr: work.path.route.source,
            port: work.local_port,
        };
        let key = FlowKey::new(source, work.remote, TransportProtocol::Tcp).unwrap();
        let ingress_path = TcpPath {
            route: RouteDecision {
                interface: work.path.route.interface,
                source: work.remote.addr,
                next_hop: source.addr,
                mtu: work.path.route.mtu,
                table: work.path.route.table,
            },
            source_mac: work.path.destination_mac,
            destination_mac: work.path.source_mac,
            unresolved_neighbor: None,
            config_generation: work.path.config_generation,
        };
        table.ingest_local(
            work.path.route.interface,
            ingress_path,
            key,
            TcpPacket {
                source_port: work.local_port,
                destination_port: work.remote.port,
                sequence: work.sequence,
                acknowledgement: work.acknowledgement,
                flags: work.flags,
                window: work.window,
                urgent_pointer: 0,
                header_len: 20 + u16::from(work.options_len),
                payload_offset: 0,
                payload_len: work
                    .payload
                    .as_ref()
                    .map_or(0, |payload| u32::from(payload.len)),
                options: work.parsed_options,
            },
            work.payload.as_ref(),
            now_ns,
        )
    }

    fn prepare_local_payload(
        table: &mut TcpEndpointTable,
        pair: &LocalPair,
        payload: &[u8],
        now_ns: u64,
    ) -> PreparedTcpTx {
        assert_eq!(pair.client.test_push_stream_tx(payload), payload.len());
        assert!(table.drain_send(pair.client_flow, now_ns));
        loop {
            let work = table.take_output().expect("本地发送必须产生 TCP 输出");
            if work.flow == pair.client_flow && work.payload.is_some() {
                return work;
            }
        }
    }

    fn establish_active(
        table: &mut TcpEndpointTable,
        facade: &Arc<SocketFacade>,
        local: Endpoint,
        remote: Endpoint,
    ) -> FlowId {
        let flow = table
            .connect(
                local,
                remote,
                path(local.addr, remote.addr),
                Arc::clone(facade),
                7,
                false,
                1_000,
            )
            .unwrap();
        facade.publish_binding(
            OwnerRef::Flow {
                shard: crate::ShardId(0),
                flow,
                generation: facade.generation(),
            },
            local,
            Some(remote),
            Some(InterfaceId(1)),
        );
        let syn = table.take_output().unwrap();
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    500,
                    syn.sequence.0.wrapping_add(1),
                    TcpFlags::SYN | TcpFlags::ACK,
                    0,
                ),
                Vec::new(),
                10_000,
            )
            .unwrap();
        assert_eq!(table.take_output().unwrap().flags, TcpFlags::ACK);
        flow
    }

    fn establish_passive(
        table: &mut TcpEndpointTable,
        local: Endpoint,
        remote: Endpoint,
        now_ns: u64,
    ) {
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp).unwrap();
        let flow = table
            .accept_syn(
                InterfaceId(1),
                path(local.addr, remote.addr),
                key,
                packet(remote.port, local.port, 700, 0, TcpFlags::SYN, 0),
                now_ns,
                false,
            )
            .unwrap();
        let syn_ack = table.take_output().unwrap();
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    701,
                    syn_ack.sequence.0.wrapping_add(1),
                    TcpFlags::ACK,
                    0,
                ),
                Vec::new(),
                now_ns + 1_000,
            )
            .unwrap();
    }

    #[test]
    fn rtt_estimator_obeys_rfc6298_bounds() {
        let mut estimator = RttEstimator::new();
        estimator.sample(100_000_000);
        assert_eq!(estimator.rto_ns, 300_000_000);
        estimator.backoff();
        assert_eq!(estimator.rto_ns, 600_000_000);
        for _ in 0..16 {
            estimator.backoff();
        }
        assert_eq!(estimator.rto_ns, MAX_RTO_NS);
    }

    #[test]
    fn stateless_reset_reverses_tuple_and_acknowledges_syn() {
        let (_owner, chain) = empty_packet(64);
        let source = IpAddr::V4(Ipv4Addr([10, 0, 0, 2]));
        let destination = IpAddr::V4(Ipv4Addr([10, 0, 0, 1]));
        let ethernet = EthernetHeader {
            destination: [1; 6],
            source: [2; 6],
            ethertype: 0x0800,
        };
        let ip = IpPacket {
            source,
            destination,
            next_header: TCP_PROTOCOL_NUMBER,
            header_len: 20,
            payload_offset: 34,
            payload_len: 20,
            hop_limit: 64,
            traffic_class: 0,
            fragment: None,
        };
        let incoming = packet(40_000, 9_999, 123, 0, TcpFlags::SYN, 0);
        let reset = match build_tcp_reset(chain, ethernet, ip, incoming) {
            Ok(reset) => reset,
            Err(_) => panic!("构造无状态 TCP RST 失败"),
        };
        let output_ip = IpPacket {
            source: destination,
            destination: source,
            next_header: TCP_PROTOCOL_NUMBER,
            header_len: 20,
            payload_offset: 34,
            payload_len: 20,
            hop_limit: 64,
            traffic_class: 0,
            fragment: None,
        };
        let tcp = crate::transport::parse_tcp_packet(&reset, output_ip).unwrap();
        assert_eq!(tcp.source_port, 9_999);
        assert_eq!(tcp.destination_port, 40_000);
        assert_eq!(tcp.acknowledgement, TcpSequence(124));
        assert_eq!(tcp.flags, TcpFlags::RST | TcpFlags::ACK);
    }

    #[test]
    fn newreno_enters_fast_recovery_on_third_duplicate_ack() {
        let mut congestion = CongestionControl::new(DEFAULT_IPV4_MSS);
        assert!(!congestion.duplicate_ack(14_600, DEFAULT_IPV4_MSS, TcpSequence(10)));
        assert!(!congestion.duplicate_ack(14_600, DEFAULT_IPV4_MSS, TcpSequence(10)));
        assert!(congestion.duplicate_ack(14_600, DEFAULT_IPV4_MSS, TcpSequence(10)));
        assert!(congestion.fast_recovery);
        assert_eq!(congestion.ssthresh, 7300);
    }

    #[test]
    fn keyed_initial_sequence_changes_with_tuple_and_time() {
        let table = TcpEndpointTable::new([1; 40], [2; 16]);
        let first = FlowKey::new(
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 1000,
            },
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 2000,
            },
            TransportProtocol::Tcp,
        )
        .unwrap();
        let second = FlowKey::new(
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 1001,
            },
            first.local,
            TransportProtocol::Tcp,
        )
        .unwrap();
        assert_ne!(
            table.initial_sequence(first, 0),
            table.initial_sequence(second, 0)
        );
        assert_ne!(
            table.initial_sequence(first, 0),
            table.initial_sequence(first, 64)
        );
    }

    #[test]
    fn active_flow_moves_bytes_between_facade_and_wire() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_000,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let facade = facade(1);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = table
            .connect(
                local,
                remote,
                path(local.addr, remote.addr),
                Arc::clone(&facade),
                7,
                false,
                1_000,
            )
            .unwrap();
        facade.publish_binding(
            OwnerRef::Flow {
                shard: crate::ShardId(0),
                flow,
                generation: facade.generation(),
            },
            local,
            Some(remote),
            Some(InterfaceId(1)),
        );
        let syn = table.take_output().unwrap();
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    500,
                    syn.sequence.0.wrapping_add(1),
                    TcpFlags::SYN | TcpFlags::ACK,
                    0,
                ),
                Vec::new(),
                10_000,
            )
            .unwrap();
        assert!(facade.readiness().0.contains(crate::Readiness::WRITABLE));
        assert_eq!(table.take_output().unwrap().flags, TcpFlags::ACK);

        assert_eq!(facade.test_push_stream_tx(b"hello world"), 11);
        assert!(table.drain_send(flow, 20_000));
        let data = table.take_output().unwrap();
        assert_eq!(data.payload.as_ref().unwrap().len, 11);
        assert_eq!(table.flows.get(flow).unwrap().flight_size(), 11);
        assert_eq!(facade.tcp_info().unacknowledged, 1);
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    501,
                    data.sequence.0.wrapping_add(11),
                    TcpFlags::ACK,
                    0,
                ),
                Vec::new(),
                30_000,
            )
            .unwrap();
        assert_eq!(facade.test_stream_tx_len(), 0);
        assert_eq!(table.flows.get(flow).unwrap().flight_size(), 0);
        assert_eq!(facade.tcp_info().unacknowledged, 0);

        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    501,
                    data.sequence.0.wrapping_add(11),
                    TcpFlags::ACK | TcpFlags::PSH,
                    5,
                ),
                b"reply".to_vec(),
                40_000,
            )
            .unwrap();
        let mut received = [0u8; 8];
        assert_eq!(
            facade
                .recv_stream(&mut received, false, false, false, true, None)
                .unwrap(),
            5
        );
        assert_eq!(&received[..5], b"reply");
    }

    #[test]
    fn deferred_tcp_info_keeps_send_accounting_at_the_protocol_boundary() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_001,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9_001,
        };
        let facade = facade(42);
        facade.set_tcp_nodelay(true);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        {
            let state = table.flows.get_mut(flow).unwrap();
            state.peer_window = u32::MAX;
            state.congestion.cwnd = u32::MAX;
        }

        assert_eq!(facade.test_push_stream_tx(b"deferred info"), 13);
        assert!(table.drain_send_deferred_info(flow, 20_000));
        let data = table.take_output().unwrap();
        assert_eq!(facade.tcp_info().bytes_sent, 13);
        assert_eq!(facade.tcp_info().unacknowledged, 0);

        table.publish_tcp_info(flow);
        assert_eq!(facade.tcp_info().unacknowledged, 1);
        table
            .process_segment_inner(
                flow,
                packet(
                    remote.port,
                    local.port,
                    501,
                    data.sequence.0.wrapping_add(13),
                    TcpFlags::ACK,
                    0,
                ),
                IngressPayload::Owned(Vec::new()),
                30_000,
                false,
            )
            .unwrap();
        assert_eq!(facade.tcp_info().unacknowledged, 1);

        table.publish_tcp_info(flow);
        assert_eq!(facade.tcp_info().unacknowledged, 0);
    }

    #[test]
    fn local_tcp_handshake_and_payload_use_transport_ingress() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40000,
        };
        let listener = facade(40);
        let group = listen_group(&listener, 40, 1, 4);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        table.listen(local, Some(InterfaceId(1)), group).unwrap();
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp).unwrap();

        table
            .ingest_local(
                InterfaceId(1),
                path(local.addr, remote.addr),
                key,
                packet(remote.port, local.port, 700, 0, TcpFlags::SYN, 0),
                None,
                1_000,
            )
            .unwrap();
        let syn_ack = table.take_output().unwrap();
        table
            .ingest_local(
                InterfaceId(1),
                path(local.addr, remote.addr),
                key,
                packet(
                    remote.port,
                    local.port,
                    701,
                    syn_ack.sequence.0.wrapping_add(1),
                    TcpFlags::ACK,
                    0,
                ),
                None,
                2_000,
            )
            .unwrap();
        let child = listener.accept(true, None).unwrap();
        let child_flow = match child.owner() {
            OwnerRef::Flow { flow, .. } => flow,
            _ => panic!("accepted child has no flow owner"),
        };

        let sender = facade(41);
        let payload = b"direct tcp payload";
        assert_eq!(sender.test_push_stream_tx(payload), payload.len());
        let lease = sender.take_stream_tx(payload.len()).unwrap();
        let receive_next = table.flows.get(child_flow).unwrap().machine.receive_next();
        let send_next = table.flows.get(child_flow).unwrap().machine.send_next();
        table
            .ingest_local(
                InterfaceId(1),
                path(local.addr, remote.addr),
                key,
                packet(
                    remote.port,
                    local.port,
                    receive_next.0,
                    send_next.0,
                    TcpFlags::ACK | TcpFlags::PSH,
                    payload.len() as u32,
                ),
                Some(&lease),
                3_000,
            )
            .unwrap();
        let mut output = [0u8; 64];
        assert_eq!(
            child.recv_stream(&mut output, false, false, false, true, None),
            Ok(payload.len())
        );
        assert_eq!(&output[..payload.len()], payload);
    }

    #[test]
    fn local_handshake_installs_direct_route_before_first_payload() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let payload = alloc::vec![0x5a; usize::from(DEFAULT_IPV4_MSS) * 2 + 73];

        assert_eq!(
            pair.client.send_stream(&payload, true, None, false),
            Ok(payload.len())
        );
        let mut received = alloc::vec![0; payload.len()];
        assert_eq!(
            pair.server
                .recv_stream(&mut received, true, false, false, true, None),
            Ok(payload.len())
        );
        assert_eq!(received, payload);
    }

    #[test]
    fn local_direct_route_does_not_overtake_dequeued_payload() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let first = prepare_local_payload(&mut table, &pair, b"first buffered payload", 10_000);
        assert!(first.payload.is_some());
        assert_eq!(pair.client.stream_unsent_len(), 0);

        let second = b"second direct payload";
        assert_eq!(
            pair.client.send_stream(second, true, None, false),
            Ok(second.len())
        );
        let mut received = [0u8; 64];
        assert_eq!(
            pair.server
                .recv_stream(&mut received, false, false, false, true, None),
            Err(SocketError::WouldBlock)
        );
        assert_eq!(pair.client.stream_unsent_len(), second.len());
    }

    #[test]
    fn reaped_local_peer_invalidates_direct_route() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        table.reap(pair.server_flow, None);

        let payload = b"payload after peer reap";
        assert_eq!(
            pair.client.send_stream(payload, true, None, false),
            Ok(payload.len())
        );
        assert_eq!(pair.client.stream_unsent_len(), payload.len());
    }

    #[test]
    fn local_fin_disables_direct_route_for_half_closed_reverse_send() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        assert!(table.shutdown_write(pair.client_flow, 10_000));
        let fin = table.take_output().unwrap();
        assert!(fin.flags.contains(TcpFlags::FIN));
        ingest_prepared_local_work(&mut table, &fin, 11_000).unwrap();
        assert_eq!(
            table.flows.get(pair.server_flow).unwrap().machine.state(),
            TcpState::CloseWait
        );

        let payload = b"reverse payload after fin";
        assert_eq!(
            pair.server.send_stream(payload, true, None, false),
            Ok(payload.len())
        );
        assert_eq!(pair.server.stream_unsent_len(), payload.len());
    }

    #[test]
    fn local_shutdown_sequences_fin_after_pending_direct_payload() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let sequence = table
            .flows
            .get(pair.client_flow)
            .unwrap()
            .machine
            .send_next();
        let payload = b"payload before local shutdown";
        assert_eq!(
            pair.client.send_stream(payload, true, None, false),
            Ok(payload.len())
        );

        assert!(table.shutdown_write(pair.client_flow, 10_000));
        let fin = table.take_output().unwrap();
        assert!(fin.flags.contains(TcpFlags::FIN));
        assert_eq!(fin.sequence, sequence + payload.len() as u32);
        assert_eq!(
            table
                .flows
                .get(pair.server_flow)
                .unwrap()
                .machine
                .receive_next(),
            sequence + payload.len() as u32
        );
    }

    #[test]
    fn closing_local_receiver_reconciles_pending_direct_payload() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let sequence = table
            .flows
            .get(pair.client_flow)
            .unwrap()
            .machine
            .send_next();
        let payload = b"payload before local receiver close";
        assert_eq!(
            pair.client.send_stream(payload, true, None, false),
            Ok(payload.len())
        );

        pair.server.close();
        assert!(table.close_flow(pair.server_flow, 10_000));
        assert_eq!(
            table
                .flows
                .get(pair.client_flow)
                .unwrap()
                .machine
                .send_next(),
            sequence + payload.len() as u32
        );
        assert_eq!(
            table
                .flows
                .get(pair.server_flow)
                .unwrap()
                .machine
                .receive_next(),
            sequence + payload.len() as u32
        );
    }

    #[test]
    fn deferred_accept_first_payload_uses_promoting_ingress_path() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair_with_defer(&mut table, None, 1_000_000_000);
        assert!(matches!(
            pair.listener.accept(true, None),
            Err(SocketError::WouldBlock)
        ));
        let first = prepare_local_payload(&mut table, &pair, b"deferred accept payload", 10_000);
        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &first, 11_000),
            Ok(None)
        );
        ingest_prepared_local_work(&mut table, &first, 12_000).unwrap();
        assert!(pair.listener.accept(true, None).is_ok());
    }

    #[test]
    fn local_effect_reuses_generation_checked_peer_hint() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let first = prepare_local_payload(&mut table, &pair, b"first local payload", 10_000);
        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &first, 11_000),
            Ok(Some((pair.client_flow, pair.server_flow)))
        );
        assert!(
            table
                .flows
                .get(pair.client_flow)
                .unwrap()
                .local_peer_hint
                .is_some()
        );

        table.rss_key = [0xa5; 40];
        let second = prepare_local_payload(&mut table, &pair, b"second local payload", 12_000);
        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &second, 13_000),
            Ok(Some((pair.client_flow, pair.server_flow)))
        );
    }

    #[test]
    fn local_direct_reconciliation_advances_both_sequence_spaces_once() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let first = prepare_local_payload(&mut table, &pair, b"route warmup", 10_000);
        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &first, 11_000),
            Ok(Some((pair.client_flow, pair.server_flow)))
        );
        let send_next = table
            .flows
            .get(pair.client_flow)
            .unwrap()
            .machine
            .send_next();
        let receive_next = table
            .flows
            .get(pair.server_flow)
            .unwrap()
            .machine
            .receive_next();
        assert_eq!(send_next, receive_next);

        assert_eq!(
            table.reconcile_local_direct(pair.client_flow, 32 * 1024, 12_000),
            Some((pair.client_flow, pair.server_flow))
        );
        assert_eq!(
            table
                .flows
                .get(pair.client_flow)
                .unwrap()
                .machine
                .send_next(),
            send_next + 32 * 1024
        );
        assert_eq!(
            table
                .flows
                .get(pair.server_flow)
                .unwrap()
                .machine
                .receive_next(),
            receive_next + 32 * 1024
        );
    }

    #[test]
    fn local_effect_rejects_stale_peer_generation() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let first = prepare_local_payload(&mut table, &pair, b"install hint", 10_000);
        assert!(
            table
                .try_local_data_effect(InterfaceId(1), &first, 11_000)
                .unwrap()
                .is_some()
        );
        let pending = prepare_local_payload(&mut table, &pair, b"stale generation", 12_000);
        assert!(table.flows.remove_id(pair.server_flow).is_some());

        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &pending, 13_000),
            Ok(None)
        );
        assert_eq!(
            table.flows.get(pair.client_flow).unwrap().local_peer_hint,
            None
        );
    }

    #[test]
    fn local_effect_rejects_reverse_tuple_mismatch() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let mut work = prepare_local_payload(&mut table, &pair, b"wrong tuple", 10_000);
        work.remote.port = work.remote.port.wrapping_add(1);
        let receive_next = table
            .flows
            .get(pair.server_flow)
            .unwrap()
            .machine
            .receive_next();

        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &work, 11_000),
            Ok(None)
        );
        assert_eq!(
            table
                .flows
                .get(pair.server_flow)
                .unwrap()
                .machine
                .receive_next(),
            receive_next
        );
    }

    #[test]
    fn local_effect_rejects_unsupported_flags() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        let mut work = prepare_local_payload(&mut table, &pair, b"unsupported flags", 10_000);
        work.flags |= TcpFlags::FIN;

        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &work, 11_000),
            Ok(None)
        );
    }

    #[test]
    fn local_effect_full_receiver_does_not_advance_sequence() {
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let pair = establish_local_pair(&mut table, None);
        pair.client.set_tcp_nodelay(true);
        {
            let flow = table.flows.get_mut(pair.client_flow).unwrap();
            flow.peer_window = u32::MAX;
            flow.congestion.cwnd = u32::MAX;
        }
        let payload = alloc::vec![0x51; 32 * 1024 + 140];
        assert_eq!(pair.client.test_push_stream_tx(&payload), payload.len());
        assert!(table.drain_send(pair.client_flow, 10_000));
        let mut output = Vec::new();
        while let Some(work) = table.take_output() {
            output.push(work);
        }
        let second = output.pop().unwrap();
        let fill_len = output
            .iter()
            .map(|work| usize::from(work.payload.as_ref().unwrap().len))
            .sum();
        pair.server.set_buffer_limits(None, Some(fill_len));
        for (index, work) in output.iter().enumerate() {
            assert!(
                table
                    .try_local_data_effect(InterfaceId(1), work, 11_000 + index as u64)
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(pair.server.stream_receive_window(), 0);
        let receive_next = table
            .flows
            .get(pair.server_flow)
            .unwrap()
            .machine
            .receive_next();

        assert_eq!(
            table.try_local_data_effect(InterfaceId(1), &second, 13_000),
            Ok(None)
        );
        assert_eq!(
            table
                .flows
                .get(pair.server_flow)
                .unwrap()
                .machine
                .receive_next(),
            receive_next
        );
    }

    #[test]
    fn output_backpressure_resumes_stream_after_queue_is_drained() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_010,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9_010,
        };
        let facade = facade(10);
        facade.set_tcp_nodelay(true);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        {
            let state = table.flows.get_mut(flow).unwrap();
            state.peer_window = u32::MAX;
            state.congestion.cwnd = u32::MAX;
        }
        for _ in 0..500 {
            table.queue_ack(flow, 20_000);
        }
        let payload = alloc::vec![0x5a; 256 * 1024];
        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());

        assert!(table.drain_send(flow, 30_000));
        assert_eq!(table.output.len(), MAX_PENDING_OUTPUT);
        assert!(table.has_output_blocked());
        assert_ne!(facade.stream_unsent_len(), 0);

        while table.take_output().is_some() {}
        assert_eq!(table.resume_output_blocked(40_000, 1), 1);
        assert_eq!(facade.stream_unsent_len(), 0);
        assert!(!table.has_output_blocked());
    }

    #[test]
    fn stream_send_marks_only_the_batch_tail_with_psh() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_014,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9_014,
        };
        let facade = facade(14);
        facade.set_tcp_maxseg(536);
        facade.set_tcp_nodelay(true);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        {
            let state = table.flows.get_mut(flow).unwrap();
            state.peer_window = u32::MAX;
            state.congestion.cwnd = u32::MAX;
        }
        let payload = alloc::vec![0x5a; 536 * 3 + 17];
        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());

        assert!(table.drain_send(flow, 20_000));
        let flags =
            core::iter::from_fn(|| table.take_output().map(|work| work.flags)).collect::<Vec<_>>();

        assert_eq!(flags.len(), 4);
        assert_eq!(&flags[..3], &[TcpFlags::ACK; 3]);
        assert_eq!(flags[3], TcpFlags::ACK | TcpFlags::PSH);
    }

    #[test]
    fn control_output_is_not_dropped_when_data_queue_is_full() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_012,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9_012,
        };
        let facade = facade(12);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        for _ in 0..MAX_PENDING_OUTPUT {
            table.queue_ack(flow, 20_000);
        }

        table.queue_ack(flow, 30_000);
        assert_eq!(table.output.len(), MAX_PENDING_OUTPUT + 1);
        assert_eq!(facade.take_pending_error(), None);
    }

    #[test]
    fn retransmit_limit_stops_new_data_until_ack_frees_a_slot() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_013,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9_013,
        };
        let facade = facade(13);
        facade.set_tcp_maxseg(536);
        facade.set_tcp_nodelay(true);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        {
            let state = table.flows.get_mut(flow).unwrap();
            state.peer_window = u32::MAX;
            state.peer_window_scale = 4;
            state.congestion.cwnd = u32::MAX;
        }
        let payload = alloc::vec![0x6b; 256 * 1024];
        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());

        assert!(table.drain_send(flow, 20_000));
        assert_eq!(table.flows.get(flow).unwrap().retransmit.len(), 256);
        assert_eq!(table.output.len(), 256);
        let unsent_before = facade.stream_unsent_len();
        assert_ne!(unsent_before, 0);

        let first = table.take_output().unwrap();
        let first_len = u32::from(first.payload.as_ref().unwrap().len);
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    501,
                    (first.sequence + first_len).0,
                    TcpFlags::ACK,
                    0,
                ),
                Vec::new(),
                30_000,
            )
            .unwrap();
        assert_eq!(table.flows.get(flow).unwrap().retransmit.len(), 256);
        assert!(facade.stream_unsent_len() < unsent_before);
    }

    #[test]
    fn full_receive_ring_does_not_advance_sequence() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_010,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9010,
        };
        let facade = facade(30);
        facade.set_buffer_limits(None, Some(16 * 1024));
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        assert_eq!(facade.push_stream_rx(&[0; 16 * 1024]), Ok(16 * 1024));
        let receive_before = table.flows.get(flow).unwrap().machine.receive_next();
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    receive_before.0,
                    table.flows.get(flow).unwrap().machine.send_next().0,
                    TcpFlags::ACK | TcpFlags::PSH,
                    1,
                ),
                alloc::vec![1],
                20_000,
            )
            .unwrap();
        assert_eq!(
            table.flows.get(flow).unwrap().machine.receive_next(),
            receive_before
        );
        assert_eq!(table.take_output().unwrap().flags, TcpFlags::ACK);
    }

    #[test]
    fn passive_handshake_publishes_one_accepted_child() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_000,
        };
        let listener = facade(2);
        let group = listen_group(&listener, 1, 1, 4);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        table.listen(local, Some(InterfaceId(1)), group).unwrap();
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp).unwrap();
        let syn = packet(remote.port, local.port, 700, 0, TcpFlags::SYN, 0);
        let flow = table
            .accept_syn(
                InterfaceId(1),
                path(local.addr, remote.addr),
                key,
                syn,
                1_000,
                false,
            )
            .unwrap();
        let syn_ack = table.take_output().unwrap();
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    701,
                    syn_ack.sequence.0.wrapping_add(1),
                    TcpFlags::ACK,
                    0,
                ),
                Vec::new(),
                2_000,
            )
            .unwrap();
        let child = listener.accept(true, None).unwrap();
        assert_eq!(child.local_endpoint(), Some(local));
        assert_eq!(child.peer_endpoint(), Some(remote));
        assert!(child.readiness().0.contains(crate::Readiness::WRITABLE));
        assert!(matches!(
            listener.accept(true, None),
            Err(SocketError::WouldBlock)
        ));
    }

    #[test]
    fn defer_accept_reserves_backlog_until_timeout() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9002,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_002,
        };
        let listener = facade(22);
        listener.set_tcp_defer_accept_ns(1_000_000_000);
        let group = listen_group(&listener, 22, 1, 4);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        table
            .listen(local, Some(InterfaceId(1)), Arc::clone(&group))
            .unwrap();
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp).unwrap();
        let flow = table
            .accept_syn(
                InterfaceId(1),
                path(local.addr, remote.addr),
                key,
                packet(remote.port, local.port, 900, 0, TcpFlags::SYN, 0),
                1_000,
                false,
            )
            .unwrap();
        let syn_ack = table.take_output().unwrap();
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    901,
                    syn_ack.sequence.0.wrapping_add(1),
                    TcpFlags::ACK,
                    0,
                ),
                Vec::new(),
                2_000,
            )
            .unwrap();
        assert_eq!(group.accept_count(), 1);
        assert!(matches!(
            listener.accept(true, None),
            Err(SocketError::WouldBlock)
        ));
        let generation = table.generation(flow).unwrap();
        assert!(table.handle_timer(flow, generation, 1_000_002_000));
        assert!(listener.accept(true, None).is_ok());
    }

    #[test]
    fn one_listener_accepts_connections_owned_by_different_shards() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9006,
        };
        let listener = facade(20);
        let group = listen_group(&listener, 4, 2, 8);
        let first_group = Arc::clone(&group);
        let second_group = Arc::clone(&group);
        let first = thread::spawn(move || {
            let mut table = TcpEndpointTable::new_on_shard(ShardId(0), [1; 40], [2; 16]);
            table
                .listen(local, Some(InterfaceId(1)), first_group)
                .unwrap();
            establish_passive(
                &mut table,
                local,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 42_000,
                },
                1_000,
            );
        });
        let second = thread::spawn(move || {
            let mut table = TcpEndpointTable::new_on_shard(ShardId(1), [1; 40], [2; 16]);
            table
                .listen(local, Some(InterfaceId(1)), second_group)
                .unwrap();
            establish_passive(
                &mut table,
                local,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 42_001,
                },
                2_000,
            );
        });
        first.join().unwrap();
        second.join().unwrap();

        let owners = [
            listener.accept(true, None).unwrap().owner(),
            listener.accept(true, None).unwrap().owner(),
        ];
        assert!(owners.iter().any(|owner| matches!(
            owner,
            OwnerRef::Flow {
                shard: ShardId(0),
                ..
            }
        )));
        assert!(owners.iter().any(|owner| matches!(
            owner,
            OwnerRef::Flow {
                shard: ShardId(1),
                ..
            }
        )));
    }

    #[test]
    fn closing_listener_can_be_replaced_before_shard_drain() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9007,
        };
        let old_listener = facade(21);
        let old_group = listen_group(&old_listener, 21, 2, 8);
        let mut table = TcpEndpointTable::new_on_shard(ShardId(0), [1; 40], [2; 16]);
        table
            .listen(local, Some(InterfaceId(1)), Arc::clone(&old_group))
            .unwrap();

        old_group.close();
        let new_listener = facade(22);
        let new_group = listen_group(&new_listener, 22, 2, 8);
        assert!(
            table
                .listen(local, Some(InterfaceId(1)), Arc::clone(&new_group))
                .is_ok()
        );

        assert!(!table.close_listener(old_group.id()));
        assert!(table.close_listener(new_group.id()));
    }

    #[test]
    fn overlapping_out_of_order_segments_are_merged_once() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_001,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9001,
        };
        let facade = facade(3);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    504,
                    table.flows.get(flow).unwrap().machine.send_next().0,
                    TcpFlags::ACK,
                    8,
                ),
                b"lo world".to_vec(),
                20_000,
            )
            .unwrap();
        let _ = table.take_output();
        table
            .process_segment(
                flow,
                packet(
                    remote.port,
                    local.port,
                    501,
                    table.flows.get(flow).unwrap().machine.send_next().0,
                    TcpFlags::ACK,
                    5,
                ),
                b"hello".to_vec(),
                30_000,
            )
            .unwrap();
        let mut bytes = [0u8; 16];
        let len = facade
            .recv_stream(&mut bytes, false, false, false, true, None)
            .unwrap();
        assert_eq!(&bytes[..len], b"hello world");
        assert_eq!(table.flows.get(flow).unwrap().reassembly_bytes, 0);
    }

    #[test]
    fn active_syn_retries_stop_after_configured_limit() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_002,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9002,
        };
        let facade = facade(4);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = table
            .connect(
                local,
                remote,
                path(local.addr, remote.addr),
                Arc::clone(&facade),
                9,
                false,
                0,
            )
            .unwrap();
        let _ = table.take_output();
        for retry in 0..ACTIVE_SYN_RETRIES {
            table.retransmit_first(flow, (u64::from(retry) + 1) * MAX_RTO_NS, false);
            assert!(table.take_output().is_some());
        }
        table.retransmit_first(flow, 8 * MAX_RTO_NS, false);
        assert!(table.flows.get(flow).is_none());
        assert_eq!(facade.take_pending_error(), Some(SocketError::TimedOut));
    }

    #[test]
    fn icmp_error_is_delivered_to_matching_tcp_flow() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_005,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9006,
        };
        let facade = facade(8);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = table
            .connect(
                local,
                remote,
                path(local.addr, remote.addr),
                Arc::clone(&facade),
                10,
                false,
                0,
            )
            .unwrap();
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp).unwrap();
        assert!(table.record_control_error(key, TransportControlError::PortUnreachable, 1_000,));
        assert!(table.flows.get(flow).is_none());
        assert_eq!(
            facade.take_pending_error(),
            Some(SocketError::ConnectionRefused)
        );
    }

    #[test]
    fn persist_probe_can_start_from_unsent_data() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_003,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9003,
        };
        let facade = facade(5);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        let mut zero_window = packet(
            remote.port,
            local.port,
            501,
            table.flows.get(flow).unwrap().machine.send_next().0,
            TcpFlags::ACK,
            0,
        );
        zero_window.window = 0;
        table
            .process_segment(flow, zero_window, Vec::new(), 20_000)
            .unwrap();
        assert_eq!(facade.test_push_stream_tx(b"x"), 1);
        assert!(!table.drain_send(flow, 30_000));
        let deadline = table.earliest_deadline(flow).unwrap();
        let generation = table.generation(flow).unwrap();
        assert!(table.handle_timer(flow, generation, deadline));
        let probe = table.take_output().unwrap();
        assert_eq!(probe.payload.as_ref().unwrap().len, 1);
        assert!(table.flows.get(flow).unwrap().deadlines.persist.is_some());
        assert!(
            table
                .flows
                .get(flow)
                .unwrap()
                .deadlines
                .retransmit
                .is_none()
        );
    }

    #[test]
    fn reopening_peer_window_resumes_unsent_stream_without_new_ack() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_011,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9_011,
        };
        let facade = facade(11);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        let acknowledgement = table.flows.get(flow).unwrap().machine.send_next().0;
        let mut zero_window = packet(
            remote.port,
            local.port,
            501,
            acknowledgement,
            TcpFlags::ACK,
            0,
        );
        zero_window.window = 0;
        table
            .process_segment(flow, zero_window, Vec::new(), 20_000)
            .unwrap();
        assert_eq!(facade.test_push_stream_tx(b"window reopened"), 15);
        assert!(!table.drain_send(flow, 30_000));

        let reopened = packet(
            remote.port,
            local.port,
            501,
            acknowledgement,
            TcpFlags::ACK,
            0,
        );
        table
            .process_segment(flow, reopened, Vec::new(), 40_000)
            .unwrap();
        let data = table.take_output().expect("窗口更新后必须恢复发送");
        assert_eq!(data.payload.as_ref().map(|payload| payload.len), Some(15));
    }

    #[test]
    fn keepalive_probe_times_out_idle_flow() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40_004,
        };
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9004,
        };
        let facade = facade(6);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        let flow = establish_active(&mut table, &facade, local, remote);
        facade.set_tcp_keepidle_ns(1_000_000_000);
        facade.set_tcp_keepintvl_ns(1_000_000_000);
        facade.set_tcp_keepcount(1);
        facade.set_tcp_keepalive(true);
        table.drain_send(flow, 20_000);
        let first = table.earliest_deadline(flow).unwrap();
        let generation = table.generation(flow).unwrap();
        table.handle_timer(flow, generation, first);
        let probe = table.take_output().unwrap();
        assert_eq!(probe.payload.as_ref().map(|payload| payload.len), None);
        let second = table.earliest_deadline(flow).unwrap();
        table.handle_timer(flow, generation, second);
        assert!(table.flows.get(flow).is_none());
        assert_eq!(facade.take_pending_error(), Some(SocketError::TimedOut));
    }

    #[test]
    fn jumbo_mtu_mss_respects_ip_length_limit() {
        assert_eq!(path_mss(65_536, IpAddr::V4(Ipv4Addr::LOCALHOST)), 65_495);
        assert_eq!(path_mss(65_536, IpAddr::V6(Ipv6Addr::LOCALHOST)), 65_475);
        assert_eq!(
            effective_payload_mss(65_536, IpAddr::V4(Ipv4Addr::LOCALHOST), 65_495, 12),
            65_483
        );
    }

    #[test]
    fn full_accept_queue_resets_new_child_and_releases_flow() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9005,
        };
        let listener = facade(7);
        let group = listen_group(&listener, 2, 1, 1);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        table.listen(local, Some(InterfaceId(1)), group).unwrap();
        for index in 0..2u16 {
            let remote = Endpoint {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 41_000 + index,
            };
            let key = FlowKey::new(remote, local, TransportProtocol::Tcp).unwrap();
            let flow = table
                .accept_syn(
                    InterfaceId(1),
                    path(local.addr, remote.addr),
                    key,
                    packet(remote.port, local.port, 700, 0, TcpFlags::SYN, 0),
                    1_000 + u64::from(index),
                    false,
                )
                .unwrap();
            let syn_ack = table.take_output().unwrap();
            table
                .process_segment(
                    flow,
                    packet(
                        remote.port,
                        local.port,
                        701,
                        syn_ack.sequence.0.wrapping_add(1),
                        TcpFlags::ACK,
                        0,
                    ),
                    Vec::new(),
                    2_000 + u64::from(index),
                )
                .unwrap();
            if index == 0 {
                assert!(table.flows.get(flow).is_some());
            } else {
                assert!(table.flows.get(flow).is_none());
                assert!(table.take_output().unwrap().flags.contains(TcpFlags::RST));
            }
        }
    }
}
