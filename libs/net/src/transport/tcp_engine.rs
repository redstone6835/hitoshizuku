//! TCP 单分片流表、重传控制和报文构造。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;

#[cfg(test)]
use core::sync::atomic::{AtomicU64, Ordering};

use crate::buf::{PacketChain, PacketMetadata};
use crate::control::RouteDecision;
use crate::flow::{FlowKey, FlowTable, flow_hash64, rss_hash};
use crate::pipeline::{EthernetHeader, FrontendPacket, IpPacket, transport_checksum};
use crate::transport::{
    TCP_PROTOCOL_NUMBER, TcpFlags, TcpPacket, TcpSackBlock, TcpSequence, TcpState, TcpStateMachine,
    TcpTransmit, TransportControlError,
};
use crate::{
    AddressFamily, Endpoint, FlowId, InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr, OwnerRef,
    SocketError, SocketFacade, TcpTxLease, TransportProtocol,
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
}

pub struct PreparedTcpTx {
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
    pub completion: u64,
    pub low_latency: bool,
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
}

struct Listener {
    facade: Arc<SocketFacade>,
    syn_count: usize,
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

#[derive(Clone, Copy)]
struct TcpDeadlines {
    retransmit: Option<u64>,
    delayed_ack: Option<u64>,
    persist: Option<u64>,
    time_wait: Option<u64>,
    cork: Option<u64>,
    keepalive: Option<u64>,
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
    accept_parent: Option<Arc<SocketFacade>>,
    retransmit: VecDeque<SentSegment>,
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
}

impl TcpFlow {
    fn flight_size(&self) -> u32 {
        self.retransmit
            .iter()
            .filter(|segment| !segment.sacked)
            .map(|segment| segment.end.distance_from(segment.sequence))
            .sum()
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
    rss_key: [u8; 40],
    isn_key: [u8; 16],
    flows: FlowTable<TcpFlow>,
    listeners: BTreeMap<(IpAddr, u16, Option<InterfaceId>), Listener>,
    output: VecDeque<PreparedTcpTx>,
    next_completion: u64,
    stats: TcpEngineStats,
}

impl TcpEndpointTable {
    pub fn new(rss_key: [u8; 40], isn_key: [u8; 16]) -> Self {
        Self {
            rss_key,
            isn_key,
            flows: FlowTable::new(),
            listeners: BTreeMap::new(),
            output: VecDeque::with_capacity(MAX_PENDING_OUTPUT),
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
        facade: Arc<SocketFacade>,
    ) -> Result<(), TcpBindError> {
        if local.port == 0 {
            return Err(TcpBindError::InvalidEndpoint);
        }
        let key = (local.addr, local.port, interface);
        if self.listeners.contains_key(&key) {
            return Err(TcpBindError::Duplicate);
        }
        self.listeners.insert(
            key,
            Listener {
                facade,
                syn_count: 0,
            },
        );
        Ok(())
    }

    pub fn close_listener(&mut self, facade: &Arc<SocketFacade>) -> bool {
        let key = self
            .listeners
            .iter()
            .find(|(_, listener)| Arc::ptr_eq(&listener.facade, facade))
            .map(|(key, _)| *key);
        let Some(key) = key else {
            return false;
        };
        if self.listeners.remove(&key).is_none() {
            return false;
        }
        let pending: Vec<_> = (1..=4096)
            .map(FlowId)
            .filter(|id| {
                self.flows
                    .get(*id)
                    .is_some_and(|flow| flow.listener_key == Some(key))
            })
            .collect();
        for id in pending {
            self.reap(id, Some(SocketError::ConnectionReset));
        }
        true
    }

    pub fn connect(
        &mut self,
        local: Endpoint,
        remote: Endpoint,
        path: TcpPath,
        facade: Arc<SocketFacade>,
        control_sequence: u64,
        now_ns: u64,
    ) -> Result<FlowId, TcpBindError> {
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp)
            .ok_or(TcpBindError::InvalidEndpoint)?;
        let mss = apply_user_mss(path_mss(path.route.mtu, local.addr), facade.tcp_maxseg());
        let iss = self.initial_sequence(key, now_ns);
        let mut machine = TcpStateMachine::new(iss, advertised_window(&facade, 0));
        let transmit = machine.active_open().unwrap();
        let local_window_scale = choose_window_scale(facade.buffer_limits().1);
        let initial_window = advertised_window(&facade, local_window_scale);
        let flow = TcpFlow {
            facade: Arc::clone(&facade),
            machine,
            path,
            remote,
            local,
            pending_connect: Some(control_sequence),
            accept_parent: None,
            retransmit: VecDeque::new(),
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
        let chain = packet.chain;
        let Some(key) = packet.parsed.flow else {
            return Err((TcpIngressError::Malformed, chain, metadata));
        };
        let Some(tcp) = packet.parsed.tcp else {
            return Err((TcpIngressError::Malformed, chain, metadata));
        };
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let id = match self.flows.find(&key, hash) {
            Some(id) => id,
            None if tcp.flags.contains(TcpFlags::SYN) && !tcp.flags.contains(TcpFlags::ACK) => {
                match self.accept_syn(interface, path, key, tcp, now_ns) {
                    Ok(id) => id,
                    Err(error) => return Err((error, chain, metadata)),
                }
            }
            None => return Err((TcpIngressError::NoEndpoint, chain, metadata)),
        };

        let mut payload = Vec::new();
        if tcp.payload_len != 0 {
            payload.resize(tcp.payload_len as usize, 0);
            if chain
                .copy_out(usize::from(tcp.payload_offset), &mut payload)
                .is_err()
            {
                return Err((TcpIngressError::Malformed, chain, metadata));
            }
        }
        let result = self.process_segment(id, tcp, payload, now_ns);
        match result {
            Ok(()) => {
                self.stats.delivered = self.stats.delivered.saturating_add(1);
                Ok((id, chain, metadata))
            }
            Err(error) => Err((error, chain, metadata)),
        }
    }

    pub fn drain_send(&mut self, id: FlowId, now_ns: u64) -> bool {
        let mut queued = false;
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
        loop {
            let Some(flow) = self.flows.get_mut(id) else {
                break;
            };
            if !matches!(
                flow.machine.state(),
                TcpState::Established | TcpState::CloseWait
            ) {
                break;
            }
            if flow.peer_window == 0 {
                flow.deadlines
                    .persist
                    .get_or_insert(now_ns.saturating_add(flow.persist_ns));
                break;
            }
            let unsent = flow.facade.stream_unsent_len();
            if (flow.facade.tcp_cork() || flow.facade.tcp_more())
                && !flow.cork_force
                && unsent < usize::from(flow.peer_mss)
            {
                flow.deadlines
                    .cork
                    .get_or_insert(now_ns.saturating_add(CORK_TIMEOUT_NS));
                break;
            }
            if !flow.facade.tcp_nodelay()
                && flow.flight_size() != 0
                && unsent < usize::from(flow.peer_mss)
            {
                break;
            }
            let allowance = flow.send_allowance().min(u32::from(flow.peer_mss));
            if allowance == 0 {
                break;
            }
            let Some(payload) = flow.facade.take_stream_tx(allowance as usize) else {
                break;
            };
            let Some(sequence) = flow.machine.reserve_send(u32::from(payload.len)) else {
                break;
            };
            let transmit = TcpTransmit {
                sequence,
                acknowledgement: flow.machine.receive_next(),
                flags: TcpFlags::ACK | TcpFlags::PSH,
                window: advertised_window(&flow.facade, flow.local_window_scale),
            };
            self.queue_transmit(id, transmit, Some(payload), now_ns, false, true);
            if let Some(flow) = self.flows.get_mut(id) {
                flow.cork_force = false;
                flow.deadlines.cork = None;
            }
            queued = true;
        }
        queued
    }

    pub fn close_flow(&mut self, id: FlowId, now_ns: u64) -> bool {
        let Some(flow) = self.flows.get_mut(id) else {
            return false;
        };
        flow.close_requested = true;
        self.drain_send(id, now_ns);
        self.maybe_send_fin(id, now_ns);
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
            TransportControlError::PortUnreachable
                if self.flows.get(id).unwrap().machine.state() == TcpState::SynSent =>
            {
                self.reap(id, Some(SocketError::ConnectionRefused));
            }
            TransportControlError::PacketTooBig { mtu } => {
                let flow = self.flows.get_mut(id).unwrap();
                flow.peer_mss = flow.peer_mss.min(path_mss(mtu, flow.local.addr));
            }
            TransportControlError::TimeExceeded | TransportControlError::ParameterProblem => {
                self.flows
                    .get(id)
                    .unwrap()
                    .facade
                    .set_pending_error(SocketError::HostUnreachable);
            }
            TransportControlError::PortUnreachable => {
                self.flows
                    .get(id)
                    .unwrap()
                    .facade
                    .set_pending_error(SocketError::ConnectionReset);
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
    ) -> Result<FlowId, TcpIngressError> {
        let listener_key = self
            .find_listener_key(key.local, interface)
            .ok_or(TcpIngressError::NoEndpoint)?;
        let listener = self.listeners.get_mut(&listener_key).unwrap();
        if listener.syn_count >= listener.facade.listener_backlog() {
            return Err(TcpIngressError::FlowTableFull);
        }
        listener.syn_count += 1;
        let parent = Arc::clone(&listener.facade);
        let child = create_child_facade(address_family(key.local.addr)).map_err(|_| {
            self.listeners.get_mut(&listener_key).unwrap().syn_count -= 1;
            TcpIngressError::FlowTableFull
        })?;
        child.set_tcp_maxseg(parent.tcp_maxseg());
        let mss = apply_user_mss(path_mss(path.route.mtu, key.local.addr), child.tcp_maxseg());
        let iss = self.initial_sequence(key, now_ns);
        let mut machine = TcpStateMachine::new(iss, advertised_window(&child, 0));
        machine.listen();
        let output = machine.on_segment(tcp.segment());
        let local_window_scale = choose_window_scale(child.buffer_limits().1);
        let initial_window = advertised_window(&child, local_window_scale);
        let flow = TcpFlow {
            facade: Arc::clone(&child),
            machine,
            path,
            remote: key.remote,
            local: key.local,
            pending_connect: None,
            accept_parent: Some(parent),
            retransmit: VecDeque::new(),
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
        };
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        let id = self.flows.insert_prehashed(key, hash, flow).map_err(|_| {
            self.listeners.get_mut(&listener_key).unwrap().syn_count -= 1;
            TcpIngressError::FlowTableFull
        })?;
        publish_tcp_info(self.flows.get(id).unwrap());
        let generation = self.flows.generation(id).unwrap();
        child.publish_binding(
            OwnerRef::Flow {
                shard: crate::ShardId(0),
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

    fn process_segment(
        &mut self,
        id: FlowId,
        tcp: TcpPacket,
        payload: Vec<u8>,
        now_ns: u64,
    ) -> Result<(), TcpIngressError> {
        let state_before = self
            .flows
            .get(id)
            .map(|flow| flow.machine.state())
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

        let previous_una = self.flows.get(id).unwrap().machine.send_unacknowledged();
        let receive_before = self.flows.get(id).unwrap().machine.receive_next();
        let output = self
            .flows
            .get_mut(id)
            .unwrap()
            .machine
            .on_segment(tcp.segment());
        let current_una = self.flows.get(id).unwrap().machine.send_unacknowledged();
        if current_una.after(previous_una) {
            self.acknowledge(id, previous_una, current_una, now_ns);
            self.drain_send(id, now_ns);
            self.maybe_send_fin(id, now_ns);
        } else if tcp.flags.contains(TcpFlags::ACK)
            && payload.is_empty()
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
        } else if let Some(flow) = self.flows.get(id) {
            publish_tcp_info(flow);
        }
        Ok(())
    }

    fn receive_payload(
        &mut self,
        id: FlowId,
        expected: TcpSequence,
        sequence: TcpSequence,
        payload: Vec<u8>,
        now_ns: u64,
    ) -> Result<(), TcpIngressError> {
        let payload_start = sequence
            .before(expected)
            .then(|| expected.distance_from(sequence) as usize)
            .unwrap_or(0);
        if sequence.before_or_equal(expected) && payload_start < payload.len() {
            let accepted = &payload[payload_start..];
            let flow = self.flows.get_mut(id).unwrap();
            flow.facade
                .push_stream_rx(accepted)
                .map_err(|_| TcpIngressError::ReceiveBufferFull)?;
            if sequence.before(expected) {
                flow.machine.advance_receive(accepted.len() as u32);
            }
            self.drain_reassembly(id)?;
            let flow = self.flows.get_mut(id).unwrap();
            flow_ack_policy(flow, now_ns);
            return Ok(());
        }
        if sequence.after(expected) {
            let flow = self.flows.get_mut(id).unwrap();
            if flow.reassembly.len() >= MAX_REASSEMBLY_FRAGMENTS
                || flow.reassembly_bytes.saturating_add(payload.len()) > MAX_REASSEMBLY_BYTES
                || payload.len() > flow.facade.stream_receive_window()
            {
                self.queue_ack(id, now_ns);
                return Ok(());
            }
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
            flow.machine.advance_receive(accepted.len() as u32);
        }
        Ok(())
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
        publish_tcp_info(flow);
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

    fn on_established(&mut self, id: FlowId, now_ns: u64) -> bool {
        let listener_key = self.flows.get_mut(id).unwrap().listener_key.take();
        if let Some(key) = listener_key
            && let Some(listener) = self.listeners.get_mut(&key)
        {
            listener.syn_count = listener.syn_count.saturating_sub(1);
        }
        let flow = self.flows.get_mut(id).unwrap();
        flow.facade.publish_connected();
        flow.deadlines.keepalive = flow
            .facade
            .tcp_keepalive_enabled()
            .then(|| now_ns.saturating_add(flow.facade.tcp_keepidle_ns()));
        if let Some(sequence) = flow.pending_connect.take() {
            flow.facade.complete_control(sequence, Ok(()));
        }
        let rejected = flow
            .accept_parent
            .take()
            .is_some_and(|parent| parent.push_accepted(Arc::clone(&flow.facade)).is_err());
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
        true
    }

    fn maybe_send_fin(&mut self, id: FlowId, now_ns: u64) {
        let ready = self.flows.get(id).is_some_and(|flow| {
            flow.close_requested
                && flow.facade.stream_unsent_len() == 0
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
        if self.output.len() >= MAX_PENDING_OUTPUT {
            if let Some(flow) = self.flows.get(id) {
                flow.facade.set_pending_error(SocketError::WouldBlock);
            }
            return;
        }
        let flow = self.flows.get_mut(id).unwrap();
        let payload_len = payload.as_ref().map_or(0, |payload| payload.len);
        if track && flow.retransmit.len() < MAX_RETRANSMIT_SEGMENTS {
            let sequence_len = u32::from(payload_len)
                + transmit.flags.contains(TcpFlags::SYN) as u32
                + transmit.flags.contains(TcpFlags::FIN) as u32;
            if sequence_len != 0 {
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
                flow.deadlines
                    .retransmit
                    .get_or_insert(now_ns.saturating_add(flow.rtt.rto_ns));
            }
        }
        let (options, options_len) = wire_options(flow, transmit.flags, now_ns);
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
            completion,
            low_latency,
        });
        publish_tcp_info(flow);
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
        let Some(key) = self.flows.key(id) else {
            return;
        };
        let hash = flow_hash64(rss_hash(&self.rss_key, &key));
        if let Some(flow) = self.flows.remove(&key, hash) {
            if let Some(listener_key) = flow.listener_key
                && let Some(listener) = self.listeners.get_mut(&listener_key)
            {
                listener.syn_count = listener.syn_count.saturating_sub(1);
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
    for block in blocks.iter().flatten() {
        for segment in &mut flow.retransmit {
            if segment.sequence.after_or_equal(block.left)
                && segment.end.before_or_equal(block.right)
            {
                segment.sacked = true;
            }
        }
    }
}

fn wire_options(flow: &TcpFlow, flags: TcpFlags, now_ns: u64) -> ([u8; 40], u8) {
    let mut options = [0u8; 40];
    let mut len = 0usize;
    if flags.contains(TcpFlags::SYN) {
        options[len..len + 4].copy_from_slice(&[2, 4, 0, 0]);
        options[len + 2..len + 4].copy_from_slice(&flow.mss.to_be_bytes());
        len += 4;
        options[len..len + 4].copy_from_slice(&[1, 3, 3, flow.local_window_scale]);
        len += 4;
        options[len..len + 4].copy_from_slice(&[1, 1, 4, 2]);
        len += 4;
    }
    if flow.timestamp_enabled || flags.contains(TcpFlags::SYN) {
        options[len..len + 2].copy_from_slice(&[1, 1]);
        len += 2;
        options[len..len + 2].copy_from_slice(&[8, 10]);
        options[len + 2..len + 6].copy_from_slice(&((now_ns / 1_000_000) as u32).to_be_bytes());
        options[len + 6..len + 10]
            .copy_from_slice(&flow.timestamp_recent.unwrap_or(0).to_be_bytes());
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
            for fragment in flow.reassembly.iter().take(count) {
                options[len..len + 4].copy_from_slice(&fragment.sequence.0.to_be_bytes());
                options[len + 4..len + 8].copy_from_slice(
                    &(fragment.sequence + fragment.bytes.len() as u32)
                        .0
                        .to_be_bytes(),
                );
                len += 8;
            }
        }
    }
    while len % 4 != 0 {
        options[len] = 1;
        len += 1;
    }
    (options, len as u8)
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
    let unacknowledged = flow
        .retransmit
        .iter()
        .filter(|segment| !segment.sacked)
        .count()
        .min(u32::MAX as usize) as u32;
    let retransmitted = flow
        .retransmit
        .iter()
        .filter(|segment| segment.transmissions > 1 && !segment.sacked)
        .count()
        .min(u32::MAX as usize) as u32;
    flow.facade.update_tcp_info(
        linux_tcp_state(flow.machine.state()),
        to_micros(flow.rtt.rto_ns),
        flow.rtt.smoothed_ns.map_or(0, to_micros),
        to_micros(flow.rtt.variance_ns),
        u32::from(flow.peer_mss),
        flow.congestion.cwnd,
        flow.congestion.ssthresh,
        unacknowledged,
        retransmitted,
    );
}

fn path_mss(mtu: u32, address: IpAddr) -> u16 {
    let header = match address {
        IpAddr::V4(_) => 40,
        IpAddr::V6(_) => 60,
    };
    mtu.saturating_sub(header).min(u32::from(u16::MAX)).max(536) as u16
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
            let checksum = match transport_checksum(
                &payload,
                34,
                tcp_header_len + payload_len,
                work.path.route.source,
                work.remote.addr,
                TCP_PROTOCOL_NUMBER,
            ) {
                Ok(checksum) => checksum,
                Err(_) => return Err(payload),
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
            let checksum = match transport_checksum(
                &payload,
                54,
                transport_len,
                work.path.route.source,
                work.remote.addr,
                TCP_PROTOCOL_NUMBER,
            ) {
                Ok(checksum) => checksum,
                Err(_) => return Err(payload),
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
                .recv_stream(&mut received, false, false, true, None)
                .unwrap(),
            5
        );
        assert_eq!(&received[..5], b"reply");
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
        listener.configure_listener(4);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        table
            .listen(local, Some(InterfaceId(1)), Arc::clone(&listener))
            .unwrap();
        let key = FlowKey::new(remote, local, TransportProtocol::Tcp).unwrap();
        let syn = packet(remote.port, local.port, 700, 0, TcpFlags::SYN, 0);
        let flow = table
            .accept_syn(
                InterfaceId(1),
                path(local.addr, remote.addr),
                key,
                syn,
                1_000,
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
            .recv_stream(&mut bytes, false, false, true, None)
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
    fn full_accept_queue_resets_new_child_and_releases_flow() {
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9005,
        };
        let listener = facade(7);
        listener.configure_listener(1);
        let mut table = TcpEndpointTable::new([1; 40], [2; 16]);
        table
            .listen(local, Some(InterfaceId(1)), Arc::clone(&listener))
            .unwrap();
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
