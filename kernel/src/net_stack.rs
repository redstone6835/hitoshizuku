//! 常驻网络协议栈 broker。
//!
//! 常驻 host 负责 `net.stack` generation 生命周期和 worker-turn pinned batch 调用；
//! packet ownership 只在完整 sidecar 通过校验后由调用方移动。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use net::stack::{
    NET_STACK_CALL_RUST_ABI, NET_STACK_CALL_STATUS_OK, NET_STACK_OP_FLOW_CALL, NET_STACK_OP_PROBE,
    NET_STACK_OP_TX_FRAGMENT_HEADER, NET_STACK_OP_TX_HEADER, NET_STACK_OP_WORKER_TURN,
    NET_STACK_SOCKET_CALL_RUST_ABI, NET_STACK_SOCKET_OP_PROBE, NetStackCallV1,
    NetStackControlCommand, NetStackEndpoint, NetStackFlowCallV1, NetStackFlowCommand,
    NetStackHandle, NetStackLifecycle, NetStackRegisterError, NetStackRegisterErrorKind,
    NetStackRegistrar, NetStackRegistration, NetStackRemoveError, NetStackSnapshot,
    NetStackSocketCallV1, NetStackSocketEndpoint, NetStackState, NetStackTxError,
    NetStackTxFragmentHeaderV1, NetStackTxFragmentInputV1, NetStackTxHeaderV1, NetStackTxInputV1,
    NetStackWorkerTurnV1,
};
use sched::sync::Spinlock;

static HOST_STARTED: AtomicBool = AtomicBool::new(false);
static BROKER: Spinlock<KernelNetStackBroker> = Spinlock::new(KernelNetStackBroker::new());

struct KernelNetStackRegistrar;

enum StackCall {
    Integrated(net::stack::IntegratedNetStackCall),
    Pinned(Arc<PinnedStackCall>),
}

enum SocketCall {
    Integrated(net::stack::IntegratedNetStackSocketCall),
    Pinned(Arc<PinnedStackCall>),
}

struct PinnedCallPair {
    primary: Spinlock<crate::elm::PinnedNativeCall>,
    nested: Spinlock<crate::elm::PinnedNativeCall>,
}

struct PinnedStackCall {
    owner: elm_model::ElmId,
    generation: elm_model::Generation,
    name: Box<str>,
    contract: Box<str>,
    version: u32,
    rust_abi: &'static str,
    per_cpu: Spinlock<Vec<Option<Arc<PinnedCallPair>>>>,
}

impl Clone for StackCall {
    fn clone(&self) -> Self {
        match self {
            Self::Integrated(call) => Self::Integrated(*call),
            Self::Pinned(call) => Self::Pinned(Arc::clone(call)),
        }
    }
}

impl Clone for SocketCall {
    fn clone(&self) -> Self {
        match self {
            Self::Integrated(call) => Self::Integrated(*call),
            Self::Pinned(call) => Self::Pinned(Arc::clone(call)),
        }
    }
}

impl StackCall {
    fn invoke(
        &self,
        frame: &mut NetStackCallV1,
        host_ranges: &[(usize, usize)],
    ) -> Result<i32, i32> {
        match self {
            Self::Integrated(call) => Ok(call(frame)),
            Self::Pinned(call) => call.invoke(frame, host_ranges),
        }
    }
}

impl SocketCall {
    fn invoke(&self, frame: &mut NetStackSocketCallV1) -> Result<i32, i32> {
        match self {
            Self::Integrated(call) => Ok(call(frame)),
            Self::Pinned(call) => call.invoke(frame, &[]),
        }
    }
}

impl PinnedStackCall {
    fn new(
        endpoint: &net::stack::PinnedNetStackEndpoint,
        rust_abi: &'static str,
    ) -> Result<Self, ()> {
        let owner = elm_model::ElmId(endpoint.owner_cell());
        let generation = elm_model::Generation(endpoint.owner_generation());
        let name: Box<str> = endpoint.export_name().into();
        let contract: Box<str> = endpoint.export_contract().into();
        let version = endpoint.export_version();
        let mut per_cpu = Vec::new();
        per_cpu.try_reserve_exact(sched::NR_CPUS).map_err(|_| ())?;
        per_cpu.resize_with(sched::NR_CPUS, || None);
        let current_cpu = sched::current_cpu_id();
        if current_cpu >= sched::NR_CPUS {
            return Err(());
        }
        per_cpu[current_cpu] = Some(Arc::new(Self::new_pair(
            owner, generation, &name, &contract, version, rust_abi,
        )?));
        Ok(Self {
            owner,
            generation,
            name,
            contract,
            version,
            rust_abi,
            per_cpu: Spinlock::new(per_cpu),
        })
    }

    fn new_pair(
        owner: elm_model::ElmId,
        generation: elm_model::Generation,
        name: &str,
        contract: &str,
        version: u32,
        rust_abi: &str,
    ) -> Result<PinnedCallPair, ()> {
        let primary =
            crate::elm::PinnedNativeCall::new(owner, generation, name, contract, version, rust_abi)
                .map_err(|_| ())?;
        let nested =
            crate::elm::PinnedNativeCall::new(owner, generation, name, contract, version, rust_abi)
                .map_err(|_| ())?;
        Ok(PinnedCallPair {
            primary: Spinlock::new(primary),
            nested: Spinlock::new(nested),
        })
    }

    fn current_pair(&self) -> Result<Arc<PinnedCallPair>, i32> {
        let cpu = sched::current_cpu_id();
        if cpu >= sched::NR_CPUS {
            return Err(elm_model::ELM_MGR_STATUS_BUSY);
        }
        if let Some(pair) = self.per_cpu.lock()[cpu].as_ref().map(Arc::clone) {
            return Ok(pair);
        }
        let pair = Arc::new(
            Self::new_pair(
                self.owner,
                self.generation,
                &self.name,
                &self.contract,
                self.version,
                self.rust_abi,
            )
            .map_err(|_| elm_model::ELM_MGR_STATUS_BUSY)?,
        );
        let mut per_cpu = self.per_cpu.lock();
        if let Some(existing) = per_cpu[cpu].as_ref() {
            return Ok(Arc::clone(existing));
        }
        per_cpu[cpu] = Some(Arc::clone(&pair));
        Ok(pair)
    }

    fn invoke<T>(&self, frame: &mut T, host_ranges: &[(usize, usize)]) -> Result<i32, i32> {
        let pair = self.current_pair()?;
        let deadline = sched::now_ns_public().saturating_add(2_000_000);
        if let Some(call) = pair.primary.try_lock() {
            return crate::elm::invoke_pinned_native(&call, frame, host_ranges, deadline);
        }
        if let Some(call) = pair.nested.try_lock() {
            return crate::elm::invoke_pinned_native(&call, frame, host_ranges, deadline);
        }
        Err(elm_model::ELM_MGR_STATUS_BUSY)
    }
}

impl ElmFlowShard {
    pub(crate) const fn new(id: net::ShardId) -> Self {
        Self { id }
    }

    fn invoke(
        &self,
        command: NetStackFlowCommand,
        extra_ranges: &[(usize, usize)],
    ) -> Result<NetStackFlowCommand, (FlowCallError, NetStackFlowCommand)> {
        let (generation, call) = {
            let broker = BROKER.lock();
            let snapshot = broker.lifecycle.snapshot();
            if snapshot.state != NetStackState::Active || !snapshot.probed {
                return Err((FlowCallError::StackUnavailable, command));
            }
            let Some(record) = broker.record.as_ref() else {
                return Err((FlowCallError::StackUnavailable, command));
            };
            (record.generation, record.call.clone())
        };
        let mut flow = NetStackFlowCallV1::new(generation, self.id, command);
        let pointer = &mut flow as *mut NetStackFlowCallV1;
        let Some(flow_range) = host_range(pointer) else {
            return Err((FlowCallError::CallFailed, flow.command));
        };
        if extra_ranges.len() > 2 {
            return Err((FlowCallError::CallFailed, flow.command));
        }
        let mut ranges = [(0usize, 0usize); 3];
        ranges[0] = flow_range;
        ranges[1..extra_ranges.len() + 1].copy_from_slice(extra_ranges);
        let mut frame = NetStackCallV1::new(NET_STACK_OP_FLOW_CALL, generation);
        frame.reserved1[0] = pointer as usize as u64;
        let result = call.invoke(&mut frame, &ranges[..extra_ranges.len() + 1]);
        let valid = matches!(result, Ok(NET_STACK_CALL_STATUS_OK))
            && frame.valid(NET_STACK_OP_FLOW_CALL, generation)
            && frame.reserved1[0] == pointer as usize as u64
            && frame.ready == 0
            && frame.quiesced == 0
            && flow.committed == 1;
        if valid {
            return Ok(flow.command);
        }
        Err((FlowCallError::CallFailed, flow.command))
    }

    fn invoke_no_ranges(
        &self,
        command: NetStackFlowCommand,
    ) -> Result<NetStackFlowCommand, (FlowCallError, NetStackFlowCommand)> {
        self.invoke(command, &[])
    }

    pub(crate) fn stats(&self) -> net::flow::FlowShardStats {
        let command = NetStackFlowCommand::Stats { output: None };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::Stats {
                output: Some(stats),
            }) => stats,
            _ => net::flow::FlowShardStats::default(),
        }
    }

    pub(crate) fn run_due_timers(&self, now_ns: u64) {
        let _ = self.invoke_no_ranges(NetStackFlowCommand::RunDueTimers { now_ns });
    }

    pub(crate) fn next_timer_deadline_ns(&self) -> Option<u64> {
        match self.invoke_no_ranges(NetStackFlowCommand::NextTimerDeadline { output: None }) {
            Ok(NetStackFlowCommand::NextTimerDeadline {
                output: Some(deadline),
            }) => deadline,
            _ => None,
        }
    }

    pub(crate) fn has_blocked_tcp_output(&self) -> bool {
        match self.invoke_no_ranges(NetStackFlowCommand::HasBlockedTcpOutput { output: None }) {
            Ok(NetStackFlowCommand::HasBlockedTcpOutput {
                output: Some(blocked),
            }) => blocked,
            _ => false,
        }
    }

    pub(crate) fn bind_udp(
        &self,
        local: net::Endpoint,
        peer: Option<net::Endpoint>,
        interface: Option<net::InterfaceId>,
    ) -> Result<net::FlowId, net::transport::UdpBindError> {
        let command = NetStackFlowCommand::BindUdp {
            local,
            peer,
            interface,
            output: None,
        };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::BindUdp {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::UdpBindError::FlowTableFull),
        }
    }

    pub(crate) fn bind_udp_facade(
        &self,
        local: net::Endpoint,
        peer: Option<net::Endpoint>,
        interface: Option<net::InterfaceId>,
        facade: Arc<net::SocketFacade>,
        free_bind: bool,
        accepts_ipv4: bool,
    ) -> Result<net::FlowId, net::transport::UdpBindError> {
        let command = NetStackFlowCommand::BindUdpFacade {
            local,
            peer,
            interface,
            facade,
            free_bind,
            accepts_ipv4,
            output: None,
        };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::BindUdpFacade {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::UdpBindError::FlowTableFull),
        }
    }

    pub(crate) fn reconnect_udp_facade(
        &self,
        flow: net::FlowId,
        local: net::Endpoint,
        peer: net::Endpoint,
        facade: Arc<net::SocketFacade>,
    ) -> Result<net::FlowId, net::transport::UdpBindError> {
        let command = NetStackFlowCommand::ReconnectUdpFacade {
            flow,
            local,
            peer,
            facade,
            output: None,
        };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::ReconnectUdpFacade {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::UdpBindError::FlowTableFull),
        }
    }

    pub(crate) fn close_udp(&self, flow: net::FlowId) {
        let _ = self.invoke_no_ranges(NetStackFlowCommand::CloseUdp { flow });
    }

    pub(crate) fn bind_raw_facade(
        &self,
        local: net::IpAddr,
        interface: Option<net::InterfaceId>,
        facade: Arc<net::SocketFacade>,
        free_bind: bool,
    ) -> Result<net::FlowId, net::transport::RawBindError> {
        let command = NetStackFlowCommand::BindRawFacade {
            local,
            interface,
            facade,
            free_bind,
            output: None,
        };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::BindRawFacade {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::RawBindError::TableFull),
        }
    }

    pub(crate) fn close_raw(&self, flow: net::FlowId) {
        let _ = self.invoke_no_ranges(NetStackFlowCommand::CloseRaw { flow });
    }

    pub(crate) fn listen_tcp(
        &self,
        local: net::Endpoint,
        interface: Option<net::InterfaceId>,
        group: Arc<net::ListenGroup>,
    ) -> Result<(), net::transport::TcpBindError> {
        let command = NetStackFlowCommand::ListenTcp {
            local,
            interface,
            group,
            output: None,
        };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::ListenTcp {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::TcpBindError::Full),
        }
    }

    pub(crate) fn connect_tcp(
        &self,
        local: net::Endpoint,
        remote: net::Endpoint,
        path: net::transport::TcpPath,
        facade: Arc<net::SocketFacade>,
        control_sequence: u64,
        now_ns: u64,
    ) -> Result<net::FlowId, net::transport::TcpBindError> {
        let command = NetStackFlowCommand::ConnectTcp {
            local,
            remote,
            path,
            facade,
            control_sequence,
            now_ns,
            output: None,
        };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::ConnectTcp {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::TcpBindError::Full),
        }
    }

    pub(crate) fn close_tcp(&self, flow: net::FlowId, now_ns: u64) {
        let _ = self.invoke_no_ranges(NetStackFlowCommand::CloseTcp { flow, now_ns });
    }

    pub(crate) fn abort_tcp(&self, flow: net::FlowId, now_ns: u64) {
        let _ = self.invoke_no_ranges(NetStackFlowCommand::AbortTcp { flow, now_ns });
    }

    pub(crate) fn shutdown_tcp_write(&self, flow: net::FlowId, now_ns: u64) {
        let _ = self.invoke_no_ranges(NetStackFlowCommand::ShutdownTcpWrite { flow, now_ns });
    }

    pub(crate) fn close_tcp_listener(&self, group: net::ListenGroupId) -> bool {
        match self.invoke_no_ranges(NetStackFlowCommand::CloseTcpListener {
            group,
            output: None,
        }) {
            Ok(NetStackFlowCommand::CloseTcpListener {
                output: Some(result),
                ..
            }) => result,
            _ => false,
        }
    }

    pub(crate) fn drain_tcp_send(&self, flow: net::FlowId, now_ns: u64) {
        let _ = self.invoke_no_ranges(NetStackFlowCommand::DrainTcpSend { flow, now_ns });
    }

    pub(crate) fn take_tcp_output(&self) -> Option<net::transport::PreparedTcpTx> {
        match self.invoke_no_ranges(NetStackFlowCommand::TakeTcpOutput { output: None }) {
            Ok(NetStackFlowCommand::TakeTcpOutput {
                output: Some(result),
            }) => result,
            _ => None,
        }
    }

    pub(crate) fn resume_tcp_output(&self, now_ns: u64, budget: usize) -> usize {
        match self.invoke_no_ranges(NetStackFlowCommand::ResumeTcpOutput {
            now_ns,
            budget,
            output: None,
        }) {
            Ok(NetStackFlowCommand::ResumeTcpOutput {
                output: Some(result),
                ..
            }) => result,
            _ => 0,
        }
    }

    pub(crate) fn resolve_tcp_path(
        &self,
        destination: net::IpAddr,
        bound_source: Option<net::IpAddr>,
        interface: Option<net::InterfaceId>,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
        free_bind: bool,
    ) -> Result<net::transport::TcpPath, net::SocketError> {
        let Some(config_range) = host_range(config) else {
            return Err(net::SocketError::NetworkUnreachable);
        };
        let command = NetStackFlowCommand::ResolveTcpPath {
            destination,
            bound_source,
            interface,
            config: config as *const _,
            now_ns,
            free_bind,
            output: None,
        };
        match self.invoke(command, &[config_range]) {
            Ok(NetStackFlowCommand::ResolveTcpPath {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::SocketError::NetworkUnreachable),
        }
    }

    pub(crate) fn refresh_tcp_tx_path(
        &self,
        work: &mut net::transport::PreparedTcpTx,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
    ) -> Result<(), net::SocketError> {
        let pointer = work as *mut net::transport::PreparedTcpTx;
        let Some(range) = host_range(pointer) else {
            return Err(net::SocketError::Buffer);
        };
        let Some(config_range) = host_range(config) else {
            return Err(net::SocketError::NetworkUnreachable);
        };
        let command = NetStackFlowCommand::RefreshTcpTxPath {
            work: pointer,
            config: config as *const _,
            now_ns,
            output: None,
        };
        match self.invoke(command, &[range, config_range]) {
            Ok(NetStackFlowCommand::RefreshTcpTxPath {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::SocketError::NetworkUnreachable),
        }
    }

    pub(crate) fn process_local_tcp(
        &self,
        interface: net::InterfaceId,
        path: net::transport::TcpPath,
        key: net::flow::FlowKey,
        packet: net::transport::TcpPacket,
        payload: Option<&net::TcpTxLease>,
        now_ns: u64,
    ) -> Result<net::FlowId, net::transport::TcpIngressError> {
        let pointer = payload.map_or(core::ptr::null(), |payload| payload as *const _);
        let mut ranges = [(0usize, 0usize); 1];
        let range_count = if let Some(payload) = payload {
            let Some(range) = host_range(payload) else {
                return Err(net::transport::TcpIngressError::Malformed);
            };
            ranges[0] = range;
            1
        } else {
            0
        };
        let command = NetStackFlowCommand::ProcessLocalTcp {
            interface,
            path,
            key,
            packet,
            payload: pointer,
            now_ns,
            output: None,
        };
        match self.invoke(command, &ranges[..range_count]) {
            Ok(NetStackFlowCommand::ProcessLocalTcp {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::TcpIngressError::NoEndpoint),
        }
    }

    pub(crate) fn process_local_udp(
        &self,
        interface: net::InterfaceId,
        source: net::Endpoint,
        destination: net::Endpoint,
        payload: &net::UdpTxLease,
        hop_limit: u8,
        traffic_class: u8,
        now_ns: u64,
    ) -> Result<net::FlowId, net::transport::LocalUdpIngressError> {
        let Some(range) = host_range(payload) else {
            return Err(net::transport::LocalUdpIngressError::Unsupported);
        };
        let command = NetStackFlowCommand::ProcessLocalUdp {
            interface,
            source,
            destination,
            payload: payload as *const _,
            hop_limit,
            traffic_class,
            now_ns,
            output: None,
        };
        match self.invoke(command, &[range]) {
            Ok(NetStackFlowCommand::ProcessLocalUdp {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::transport::LocalUdpIngressError::Unsupported),
        }
    }

    pub(crate) fn invalidate_interface(&self, interface: net::InterfaceId) -> usize {
        match self.invoke_no_ranges(NetStackFlowCommand::InvalidateInterface {
            interface,
            output: None,
        }) {
            Ok(NetStackFlowCommand::InvalidateInterface {
                output: Some(result),
                ..
            }) => result,
            _ => 0,
        }
    }

    pub(crate) fn observe_neighbor(
        &self,
        key: net::control::NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
    ) -> bool {
        match self.invoke_no_ranges(NetStackFlowCommand::ObserveNeighbor {
            key,
            mac_address,
            now_ns,
            output: None,
        }) {
            Ok(NetStackFlowCommand::ObserveNeighbor {
                output: Some(result),
                ..
            }) => result,
            _ => false,
        }
    }

    pub(crate) fn lookup_neighbor(
        &self,
        key: net::control::NeighborKey,
        now_ns: u64,
    ) -> Option<[u8; 6]> {
        match self.invoke_no_ranges(NetStackFlowCommand::LookupNeighbor {
            key,
            now_ns,
            output: None,
        }) {
            Ok(NetStackFlowCommand::LookupNeighbor {
                output: Some(result),
                ..
            }) => result,
            _ => None,
        }
    }

    pub(crate) fn prepare_udp_tx(
        &self,
        flow: net::FlowId,
        payload: net::UdpTxLease,
        mark: u32,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
    ) -> Result<net::transport::PreparedUdpTx, (net::SocketError, net::UdpTxLease)> {
        let Some(config_range) = host_range(config) else {
            return Err((net::SocketError::NetworkUnreachable, payload));
        };
        let command = NetStackFlowCommand::PrepareUdpTx {
            flow,
            payload: Some(payload),
            mark,
            config: config as *const _,
            now_ns,
            output: None,
        };
        match self.invoke(command, &[config_range]) {
            Ok(NetStackFlowCommand::PrepareUdpTx {
                output: Some(result),
                ..
            }) => result,
            Ok(NetStackFlowCommand::PrepareUdpTx {
                payload: Some(payload),
                ..
            }) => Err((net::SocketError::RuntimeBusy, payload)),
            Err((
                _,
                NetStackFlowCommand::PrepareUdpTx {
                    payload: Some(payload),
                    ..
                },
            )) => Err((net::SocketError::RuntimeBusy, payload)),
            _ => panic!("net.stack flow call 丢失 UDP payload"),
        }
    }

    pub(crate) fn prepare_raw_tx(
        &self,
        flow: net::FlowId,
        payload: net::UdpTxLease,
        mark: u32,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
    ) -> Result<net::transport::PreparedRawTx, (net::SocketError, net::UdpTxLease)> {
        let Some(config_range) = host_range(config) else {
            return Err((net::SocketError::NetworkUnreachable, payload));
        };
        let command = NetStackFlowCommand::PrepareRawTx {
            flow,
            payload: Some(payload),
            mark,
            config: config as *const _,
            now_ns,
            output: None,
        };
        match self.invoke(command, &[config_range]) {
            Ok(NetStackFlowCommand::PrepareRawTx {
                output: Some(result),
                ..
            }) => result,
            Ok(NetStackFlowCommand::PrepareRawTx {
                payload: Some(payload),
                ..
            }) => Err((net::SocketError::RuntimeBusy, payload)),
            Err((
                _,
                NetStackFlowCommand::PrepareRawTx {
                    payload: Some(payload),
                    ..
                },
            )) => Err((net::SocketError::RuntimeBusy, payload)),
            _ => panic!("net.stack flow call 丢失 raw payload"),
        }
    }

    pub(crate) fn form_udp_packet(
        &self,
        flow: net::FlowId,
        destination: Option<net::Endpoint>,
        payload: net::buf::PacketChain,
        mark: u32,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
    ) -> Result<net::buf::TxPacket, net::flow::UdpSendFailure> {
        let Some(config_range) = host_range(config) else {
            return Err(net::flow::UdpSendFailure {
                error: net::flow::UdpSendError::Route(net::control::ConfigError::NoRoute),
                payload,
            });
        };
        let command = NetStackFlowCommand::FormUdpPacket {
            flow,
            destination,
            payload: Some(payload),
            mark,
            config: config as *const _,
            now_ns,
            output: None,
        };
        match self.invoke(command, &[config_range]) {
            Ok(NetStackFlowCommand::FormUdpPacket {
                output: Some(result),
                ..
            }) => result,
            Ok(NetStackFlowCommand::FormUdpPacket {
                payload: Some(payload),
                ..
            }) => Err(net::flow::UdpSendFailure {
                error: net::flow::UdpSendError::Route(net::control::ConfigError::NoRoute),
                payload,
            }),
            Err((
                _,
                NetStackFlowCommand::FormUdpPacket {
                    payload: Some(payload),
                    ..
                },
            )) => Err(net::flow::UdpSendFailure {
                error: net::flow::UdpSendError::Route(net::control::ConfigError::NoRoute),
                payload,
            }),
            _ => panic!("net.stack flow call 丢失 UDP payload"),
        }
    }

    pub(crate) fn recv_udp(&self, flow: net::FlowId) -> Option<net::transport::UdpDatagram> {
        match self.invoke_no_ranges(NetStackFlowCommand::RecvUdp { flow, output: None }) {
            Ok(NetStackFlowCommand::RecvUdp {
                output: Some(result),
                ..
            }) => result,
            _ => None,
        }
    }

    pub(crate) fn push_frontend_batch(&self, packets: Vec<net::pipeline::FrontendPacket>) {
        let command = NetStackFlowCommand::PushFrontendBatch {
            packets: Some(packets),
        };
        let _ = self.invoke_no_ranges(command);
    }

    pub(crate) fn process_frontend_batch(
        &self,
        interface: net::InterfaceId,
        local_mac: [u8; 6],
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
        tx: &mut net::buf::TxBatch,
        recycle: &mut net::buf::PacketBatch,
    ) -> [u32; net::buf::DropReason::COUNT] {
        let Some(config_range) = host_range(config) else {
            return [0; net::buf::DropReason::COUNT];
        };
        let command = NetStackFlowCommand::ProcessFrontendBatch {
            interface,
            local_mac,
            config: config as *const _,
            now_ns,
            output: None,
            drop_counts: [0; net::buf::DropReason::COUNT],
        };
        let command = match self.invoke(command, &[config_range]) {
            Ok(command) => command,
            Err((_, command)) => command,
        };
        let NetStackFlowCommand::ProcessFrontendBatch {
            output: Some((mut formed, mut recycled)),
            drop_counts,
            ..
        } = command
        else {
            return [0; net::buf::DropReason::COUNT];
        };
        for index in 0..formed.len() {
            let Some(packet) = formed.take(index) else {
                continue;
            };
            if let Err(packet) = tx.push(packet) {
                let _ = recycle.push(packet.chain, net::buf::PacketMetadata::default());
            }
        }
        for index in 0..recycled.len() {
            let Some((packet, metadata)) = recycled.take(index) else {
                continue;
            };
            let _ = recycle.push(packet, metadata);
        }
        drop_counts
    }

    pub(crate) fn take_reassembled_input(&self) -> Option<net::buf::PacketBatch> {
        match self.invoke_no_ranges(NetStackFlowCommand::TakeReassembledInput { output: None }) {
            Ok(NetStackFlowCommand::TakeReassembledInput {
                output: Some(result),
            }) => result,
            _ => None,
        }
    }

    pub(crate) fn parse_reassembled(
        &self,
        input: net::buf::PacketBatch,
        turn: &NetStackWorkerTurnV1,
    ) -> Result<(), net::buf::PacketBatch> {
        let ethernet = turn.ethernet().to_vec();
        let network = turn.network().to_vec();
        let transport = turn.transport().to_vec();
        let command = NetStackFlowCommand::ParseReassembled {
            input: Some(input),
            ethernet,
            network,
            transport,
            output: None,
        };
        match self.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::ParseReassembled {
                output: Some(result),
                ..
            }) => result,
            Ok(NetStackFlowCommand::ParseReassembled {
                input: Some(input), ..
            }) => Err(input),
            Err((
                _,
                NetStackFlowCommand::ParseReassembled {
                    input: Some(input), ..
                },
            )) => Err(input),
            _ => panic!("net.stack flow call 丢失 reassembly batch"),
        }
    }

    pub(crate) fn take_reassembled(&self) -> Option<net::pipeline::FrontendPacket> {
        match self.invoke_no_ranges(NetStackFlowCommand::TakeReassembled { output: None }) {
            Ok(NetStackFlowCommand::TakeReassembled {
                output: Some(result),
            }) => result,
            _ => None,
        }
    }

    pub(crate) fn take_forwarded_error(
        &self,
    ) -> Option<(
        net::InterfaceId,
        net::transport::ControlErrorTarget,
        net::transport::TransportControlError,
        u64,
    )> {
        match self.invoke_no_ranges(NetStackFlowCommand::TakeForwardedError { output: None }) {
            Ok(NetStackFlowCommand::TakeForwardedError {
                output: Some(result),
            }) => result,
            _ => None,
        }
    }

    pub(crate) fn apply_transport_error(
        &self,
        interface: net::InterfaceId,
        target: net::transport::ControlErrorTarget,
        error: net::transport::TransportControlError,
        now_ns: u64,
    ) -> bool {
        match self.invoke_no_ranges(NetStackFlowCommand::ApplyTransportError {
            interface,
            target,
            error,
            now_ns,
            output: None,
        }) {
            Ok(NetStackFlowCommand::ApplyTransportError {
                output: Some(result),
                ..
            }) => result,
            _ => false,
        }
    }
}

impl ElmControlPlane {
    pub(crate) const fn new() -> Self {
        Self {
            call: ElmFlowShard::new(net::ShardId(0)),
        }
    }

    fn invoke_with_ranges(
        &self,
        command: NetStackControlCommand,
        ranges: &[(usize, usize)],
    ) -> Option<NetStackControlCommand> {
        match self
            .call
            .invoke(NetStackFlowCommand::Control { command }, ranges)
        {
            Ok(NetStackFlowCommand::Control { command }) => Some(command),
            _ => None,
        }
    }

    fn invoke(&self, command: NetStackControlCommand) -> Option<NetStackControlCommand> {
        self.invoke_with_ranges(command, &[])
    }

    pub(crate) fn initialize_autoconfig(
        &self,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
    ) -> bool {
        let pointer = config as *const net::control::ConfigSnapshot;
        let Some(range) = host_range(pointer) else {
            return false;
        };
        matches!(
            self.invoke_with_ranges(
                NetStackControlCommand::InitializeAutoconfig {
                    config: pointer,
                    now_ns,
                    output: None,
                },
                &[range],
            ),
            Some(NetStackControlCommand::InitializeAutoconfig {
                output: Some(true),
                ..
            })
        )
    }

    pub(crate) fn run_dad(&self, now_ns: u64) -> Option<net::stack::DadRunOutput> {
        match self.invoke(NetStackControlCommand::RunDad {
            now_ns,
            output: None,
        }) {
            Some(NetStackControlCommand::RunDad { output, .. }) => output,
            _ => None,
        }
    }

    pub(crate) fn observe_dad_conflict(&self, interface: net::InterfaceId, address: net::Ipv6Addr) {
        let _ = self.invoke(NetStackControlCommand::ObserveDadConflict { interface, address });
    }

    pub(crate) fn run_dhcp(
        &self,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
    ) -> Option<net::stack::DhcpRunOutput> {
        let pointer = config as *const net::control::ConfigSnapshot;
        let range = host_range(pointer)?;
        match self.invoke_with_ranges(
            NetStackControlCommand::RunDhcp {
                config: pointer,
                now_ns,
                output: None,
            },
            &[range],
        ) {
            Some(NetStackControlCommand::RunDhcp { output, .. }) => output,
            _ => None,
        }
    }

    pub(crate) fn handle_dhcp_packet(
        &self,
        interface: net::InterfaceId,
        packet: &net::pipeline::FrontendPacket,
        now_ns: u64,
    ) -> Option<net::stack::DhcpPacketOutput> {
        let pointer = packet as *const net::pipeline::FrontendPacket;
        let range = host_range(pointer)?;
        match self.invoke_with_ranges(
            NetStackControlCommand::HandleDhcpPacket {
                interface,
                packet: pointer,
                now_ns,
                output: None,
            },
            &[range],
        ) {
            Some(NetStackControlCommand::HandleDhcpPacket { output, .. }) => output,
            _ => None,
        }
    }

    pub(crate) fn remove_autoconfig_interface(
        &self,
        interface: net::InterfaceId,
    ) -> Option<net::stack::DhcpLeaseChange> {
        match self.invoke(NetStackControlCommand::RemoveAutoconfigInterface {
            interface,
            output: None,
        }) {
            Some(NetStackControlCommand::RemoveAutoconfigInterface { output, .. }) => {
                output.flatten()
            }
            _ => None,
        }
    }

    pub(crate) fn reserve_binding(
        &self,
        socket: net::SocketId,
        request: net::control::BindRequest,
        shard: net::ShardId,
    ) -> Result<net::control::BindToken, net::control::BindError> {
        match self.invoke(NetStackControlCommand::ReserveBinding {
            socket,
            request,
            shard,
            output: None,
        }) {
            Some(NetStackControlCommand::ReserveBinding {
                output: Some(result),
                ..
            }) => result,
            _ => Err(net::control::BindError::NoPorts),
        }
    }

    pub(crate) fn release_binding(&self, socket: net::SocketId) -> bool {
        matches!(
            self.invoke(NetStackControlCommand::ReleaseBinding {
                socket,
                output: None,
            }),
            Some(NetStackControlCommand::ReleaseBinding {
                output: Some(true),
                ..
            })
        )
    }

    pub(crate) fn allocate_listener_id(&self) -> Option<net::ListenGroupId> {
        match self.invoke(NetStackControlCommand::AllocateListener { output: None }) {
            Some(NetStackControlCommand::AllocateListener { output }) => output,
            _ => None,
        }
    }

    pub(crate) fn install_listener(&self, group: net::ListenGroupId) -> bool {
        matches!(
            self.invoke(NetStackControlCommand::InstallListener {
                group,
                output: None,
            }),
            Some(NetStackControlCommand::InstallListener {
                output: Some(true),
                ..
            })
        )
    }

    pub(crate) fn remove_listener(&self, group: net::ListenGroupId) -> bool {
        matches!(
            self.invoke(NetStackControlCommand::RemoveListener {
                group,
                output: None,
            }),
            Some(NetStackControlCommand::RemoveListener {
                output: Some(true),
                ..
            })
        )
    }

    pub(crate) fn has_listener(&self, group: net::ListenGroupId) -> bool {
        matches!(
            self.invoke(NetStackControlCommand::HasListener {
                group,
                output: None,
            }),
            Some(NetStackControlCommand::HasListener {
                output: Some(true),
                ..
            })
        )
    }

    pub(crate) fn flow_shard(
        &self,
        remote: net::Endpoint,
        local: net::Endpoint,
        protocol: net::TransportProtocol,
    ) -> Option<net::ShardId> {
        match self.invoke(NetStackControlCommand::FlowShard {
            remote,
            local,
            protocol,
            output: None,
        }) {
            Some(NetStackControlCommand::FlowShard { output, .. }) => output,
            _ => None,
        }
    }

    pub(crate) fn neighbor_owner(&self, key: net::control::NeighborKey) -> Option<net::ShardId> {
        match self.invoke(NetStackControlCommand::NeighborOwner { key, output: None }) {
            Some(NetStackControlCommand::NeighborOwner { output, .. }) => output,
            _ => None,
        }
    }

    pub(crate) fn enqueue_neighbor(
        &self,
        work: net::stack::PendingNeighborTx,
        now_ns: u64,
    ) -> Result<(), net::stack::PendingNeighborTx> {
        let command = NetStackFlowCommand::Control {
            command: NetStackControlCommand::EnqueueNeighbor {
                work: Some(work),
                now_ns,
                output: None,
            },
        };
        match self.call.invoke_no_ranges(command) {
            Ok(NetStackFlowCommand::Control {
                command:
                    NetStackControlCommand::EnqueueNeighbor {
                        output: Some(result),
                        ..
                    },
            }) => result,
            Err((
                _,
                NetStackFlowCommand::Control {
                    command:
                        NetStackControlCommand::EnqueueNeighbor {
                            work: Some(work), ..
                        },
                },
            )) => Err(work),
            _ => unreachable!("控制面调用必须归还邻居报文所有权"),
        }
    }

    pub(crate) fn resolve_pending_neighbor(
        &self,
        key: net::control::NeighborKey,
        mac_address: [u8; 6],
    ) -> Vec<net::stack::PendingNeighborTx> {
        match self.invoke(NetStackControlCommand::ResolvePendingNeighbor {
            key,
            mac_address,
            output: None,
        }) {
            Some(NetStackControlCommand::ResolvePendingNeighbor {
                output: Some(work), ..
            }) => work,
            _ => Vec::new(),
        }
    }

    pub(crate) fn fail_interface_neighbors(
        &self,
        interface: net::InterfaceId,
    ) -> Vec<net::stack::PendingNeighborTx> {
        match self.invoke(NetStackControlCommand::FailInterfaceNeighbors {
            interface,
            output: None,
        }) {
            Some(NetStackControlCommand::FailInterfaceNeighbors {
                output: Some(work), ..
            }) => work,
            _ => Vec::new(),
        }
    }

    pub(crate) fn run_neighbor_timers(
        &self,
        now_ns: u64,
    ) -> Option<net::stack::NeighborTimerOutput> {
        match self.invoke(NetStackControlCommand::RunNeighborTimers {
            now_ns,
            output: None,
        }) {
            Some(NetStackControlCommand::RunNeighborTimers { output, .. }) => output,
            _ => None,
        }
    }

    pub(crate) fn join_multicast(
        &self,
        socket: net::SocketId,
        membership: net::MulticastMembership,
        interface: net::InterfaceId,
    ) -> Option<bool> {
        match self.invoke(NetStackControlCommand::JoinMulticast {
            socket,
            membership,
            interface,
            output: None,
        }) {
            Some(NetStackControlCommand::JoinMulticast { output, .. }) => output.flatten(),
            _ => None,
        }
    }

    pub(crate) fn leave_multicast(
        &self,
        socket: net::SocketId,
        membership: net::MulticastMembership,
    ) -> Option<(net::InterfaceId, bool)> {
        match self.invoke(NetStackControlCommand::LeaveMulticast {
            socket,
            membership,
            output: None,
        }) {
            Some(NetStackControlCommand::LeaveMulticast { output, .. }) => output.flatten(),
            _ => None,
        }
    }

    pub(crate) fn multicast_groups(&self, interface: net::InterfaceId) -> Vec<net::IpAddr> {
        match self.invoke(NetStackControlCommand::MulticastGroups {
            interface,
            output: None,
        }) {
            Some(NetStackControlCommand::MulticastGroups {
                output: Some(groups),
                ..
            }) => groups,
            _ => Vec::new(),
        }
    }

    pub(crate) fn remove_interface_multicast(&self, interface: net::InterfaceId) {
        let _ = self.invoke(NetStackControlCommand::RemoveInterfaceMulticast { interface });
    }

    pub(crate) fn remove_socket_multicast(
        &self,
        socket: net::SocketId,
    ) -> Vec<(net::InterfaceId, net::IpAddr)> {
        match self.invoke(NetStackControlCommand::RemoveSocketMulticast {
            socket,
            output: None,
        }) {
            Some(NetStackControlCommand::RemoveSocketMulticast {
                output: Some(groups),
                ..
            }) => groups,
            _ => Vec::new(),
        }
    }
}

struct StackRecord {
    handle: NetStackHandle,
    generation: u64,
    call: StackCall,
    socket_call: SocketCall,
}

struct KernelNetStackBroker {
    lifecycle: NetStackLifecycle,
    record: Option<StackRecord>,
}

#[derive(Clone, Copy)]
pub(crate) struct ElmFlowShard {
    id: net::ShardId,
}

#[derive(Clone, Copy)]
pub(crate) struct ElmControlPlane {
    call: ElmFlowShard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlowCallError {
    StackUnavailable,
    CallFailed,
}

impl KernelNetStackBroker {
    const fn new() -> Self {
        Self {
            lifecycle: NetStackLifecycle::new(),
            record: None,
        }
    }

    fn build_call(endpoint: &NetStackEndpoint) -> Result<StackCall, ()> {
        match endpoint {
            NetStackEndpoint::Integrated(call) if *call as usize != 0 => {
                Ok(StackCall::Integrated(*call))
            }
            NetStackEndpoint::Integrated(_) => Err(()),
            NetStackEndpoint::Pinned(endpoint) => {
                let call = PinnedStackCall::new(endpoint, NET_STACK_CALL_RUST_ABI)?;
                Ok(StackCall::Pinned(Arc::new(call)))
            }
        }
    }

    fn build_socket_call(endpoint: &NetStackSocketEndpoint) -> Result<SocketCall, ()> {
        match endpoint {
            NetStackSocketEndpoint::Integrated(call) if *call as usize != 0 => {
                Ok(SocketCall::Integrated(*call))
            }
            NetStackSocketEndpoint::Integrated(_) => Err(()),
            NetStackSocketEndpoint::Pinned(endpoint) => {
                let call = PinnedStackCall::new(endpoint, NET_STACK_SOCKET_CALL_RUST_ABI)?;
                Ok(SocketCall::Pinned(Arc::new(call)))
            }
        }
    }
}

impl NetStackRegistrar for KernelNetStackRegistrar {
    fn register_stack(
        &self,
        registration: NetStackRegistration,
    ) -> Result<NetStackHandle, NetStackRegisterError> {
        let handle = registration.handle();
        let owner_cell = registration.owner_cell();
        let generation = registration.generation();
        let mut broker = BROKER.lock();
        if broker.lifecycle.snapshot().state != net::stack::NetStackState::Absent {
            return Err(NetStackRegisterError {
                kind: NetStackRegisterErrorKind::AlreadyActive,
                registration,
            });
        }
        let call = match KernelNetStackBroker::build_call(registration.endpoint()) {
            Ok(call) => call,
            Err(()) => {
                return Err(NetStackRegisterError {
                    kind: NetStackRegisterErrorKind::ResourceExhausted,
                    registration,
                });
            }
        };
        let socket_call =
            match KernelNetStackBroker::build_socket_call(registration.socket_endpoint()) {
                Ok(call) => call,
                Err(()) => {
                    return Err(NetStackRegisterError {
                        kind: NetStackRegisterErrorKind::ResourceExhausted,
                        registration,
                    });
                }
            };
        if let Err(kind) = broker.lifecycle.activate(handle, owner_cell, generation) {
            return Err(NetStackRegisterError { kind, registration });
        }
        broker.record = Some(StackRecord {
            handle,
            generation,
            call,
            socket_call,
        });
        log::info!(
            "[net-stack] registered generation: cell={} generation={} handle={}",
            owner_cell,
            generation,
            handle.0
        );
        Ok(handle)
    }

    fn begin_remove(
        &self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRemoveError> {
        {
            let mut broker = BROKER.lock();
            broker
                .lifecycle
                .begin_remove(handle, owner_cell, generation)?;
            if !broker.lifecycle.begin_drain(handle) {
                return Err(NetStackRemoveError::Busy);
            }
        }
        let detached = net::detach_socket_generation(generation);
        let mut broker = BROKER.lock();
        broker.record = None;
        if !broker.lifecycle.finish_remove(handle) {
            return Err(NetStackRemoveError::Busy);
        }
        log::info!(
            "[net-stack] removed generation: cell={} generation={} handle={}",
            owner_cell,
            generation,
            handle.0
        );
        if detached != 0 {
            log::info!(
                "[net-stack] detached sockets: generation={} count={}",
                generation,
                detached
            );
        }
        Ok(())
    }

    fn build_tx_header(
        &self,
        payload: &net::buf::PacketChain,
        input: NetStackTxInputV1,
    ) -> Result<NetStackTxHeaderV1, NetStackTxError> {
        tx_header(payload, input)
    }

    fn build_tx_fragment_header(
        &self,
        payload: &net::buf::PacketChain,
        input: NetStackTxFragmentInputV1,
    ) -> Result<NetStackTxFragmentHeaderV1, NetStackTxError> {
        tx_fragment_header(payload, input)
    }

    fn snapshot(&self) -> NetStackSnapshot {
        BROKER.lock().lifecycle.snapshot()
    }
}

pub(crate) fn registrar() -> &'static dyn NetStackRegistrar {
    &KernelNetStackRegistrar
}

fn on_elm_lifecycle_event(event: crate::elm::ElmLifecycleEvent) {
    match event {
        crate::elm::ElmLifecycleEvent::CellLoaded { .. } => probe_active(),
    }
}

/// 启动允许 stack 缺席的常驻 host，并探测已经由 BuildBound 激活的 generation。
pub(crate) fn start_host() {
    if HOST_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    assert!(
        crate::elm::register_lifecycle_observer("net-stack", on_elm_lifecycle_event),
        "无法注册 net.stack 生命周期观察者"
    );
    if net::stack::stack_snapshot().state == net::stack::NetStackState::Absent {
        log::info!("[net-stack] host started without stack generation");
        return;
    }
    probe_active();
}

fn probe_active() {
    if !HOST_STARTED.load(Ordering::Acquire) {
        return;
    }
    let (handle, generation, call, socket_call) = {
        let broker = BROKER.lock();
        let Some(record) = broker.record.as_ref() else {
            return;
        };
        (
            record.handle,
            record.generation,
            record.call.clone(),
            record.socket_call.clone(),
        )
    };
    let mut frame = NetStackCallV1::new(NET_STACK_OP_PROBE, generation);
    let result = call.invoke(&mut frame, &[]);
    let stack_ready = matches!(result, Ok(NET_STACK_CALL_STATUS_OK))
        && frame.valid(NET_STACK_OP_PROBE, generation)
        && frame.ready == 1
        && frame.quiesced == 0;
    let mut socket_frame = NetStackSocketCallV1::new(NET_STACK_SOCKET_OP_PROBE, generation);
    let socket_result = socket_call.invoke(&mut socket_frame);
    let socket_ready = matches!(socket_result, Ok(NET_STACK_CALL_STATUS_OK))
        && socket_frame.valid(NET_STACK_SOCKET_OP_PROBE, generation)
        && socket_frame.ready == 1
        && socket_frame.quiesced == 0
        && socket_frame.committed == 1;
    let success = stack_ready && socket_ready;
    let mut broker = BROKER.lock();
    if success {
        if broker.lifecycle.mark_probed(handle) {
            log::info!(
                "[net-stack] generation probe succeeded: generation={} handle={}",
                generation,
                handle.0
            );
        }
    } else if broker.lifecycle.mark_faulted(handle) {
        log::error!(
            "[net-stack] generation probe failed: generation={} handle={} stack={:?} socket={:?}",
            generation,
            handle.0,
            result,
            socket_result
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerTurnError {
    StackUnavailable,
    CallFailed,
}

/// 在当前 active generation 中执行一次只读 RX batch turn。
pub(crate) fn worker_turn(
    input: &net::buf::PacketBatch,
    interface: net::InterfaceId,
    config: &net::control::ConfigSnapshot,
) -> Result<NetStackWorkerTurnV1, WorkerTurnError> {
    let (handle, generation, call) = {
        let broker = BROKER.lock();
        let snapshot = broker.lifecycle.snapshot();
        if snapshot.state != NetStackState::Active || !snapshot.probed {
            return Err(WorkerTurnError::StackUnavailable);
        }
        let Some(record) = broker.record.as_ref() else {
            return Err(WorkerTurnError::StackUnavailable);
        };
        (record.handle, record.generation, record.call.clone())
    };

    let input_pointer = input as *const net::buf::PacketBatch;
    let input_count = input.len() as u8;
    let local_addresses = config.stack_local_addresses();
    let Ok(local_address_count) = u32::try_from(local_addresses.len()) else {
        return Err(WorkerTurnError::StackUnavailable);
    };
    if interface.0 == 0
        || !local_addresses
            .iter()
            .all(net::stack::NetStackLocalAddressV1::valid)
    {
        return Err(WorkerTurnError::StackUnavailable);
    }
    let local_address_pointer = local_addresses.as_ptr();
    let Some(boot) = net::stack::boot_config() else {
        return Err(WorkerTurnError::StackUnavailable);
    };
    let mut rss_generation_bytes = [0; 4];
    rss_generation_bytes.copy_from_slice(&boot.generation_nonce()[..4]);
    let rss_generation = u32::from_le_bytes(rss_generation_bytes).max(1);
    let rss_key = *boot.rss_key();
    let mut turn = NetStackWorkerTurnV1::new(
        generation,
        config.generation,
        interface.0,
        local_addresses,
        rss_key,
        rss_generation,
        input,
    );
    let turn_pointer = &mut turn as *mut NetStackWorkerTurnV1;
    let mut frame = NetStackCallV1::new(NET_STACK_OP_WORKER_TURN, generation);
    frame.worker_turn = turn_pointer;
    let Some(turn_range) = host_range(turn_pointer) else {
        return Err(WorkerTurnError::CallFailed);
    };
    let Some(input_range) = host_range(input_pointer) else {
        return Err(WorkerTurnError::CallFailed);
    };
    let mut host_ranges = [(0usize, 0usize); 3];
    host_ranges[0] = turn_range;
    host_ranges[1] = input_range;
    let range_count = if local_addresses.is_empty() {
        2
    } else {
        let Some(address_range) = host_slice_range(local_addresses) else {
            return Err(WorkerTurnError::CallFailed);
        };
        host_ranges[2] = address_range;
        3
    };
    let result = call.invoke(&mut frame, &host_ranges[..range_count]);
    let valid = matches!(result, Ok(NET_STACK_CALL_STATUS_OK))
        && frame.valid(NET_STACK_OP_WORKER_TURN, generation)
        && frame.worker_turn == turn_pointer
        && frame.ready == 0
        && frame.quiesced == 0
        && turn.valid_header(
            generation,
            config.generation,
            interface.0,
            input_pointer,
            local_address_pointer,
            local_address_count,
            &rss_key,
            rss_generation,
        )
        && turn.input_count == input_count
        && input.len() == usize::from(input_count)
        && local_addresses
            .iter()
            .all(net::stack::NetStackLocalAddressV1::valid)
        && turn.fully_committed(input);
    if valid {
        return Ok(turn);
    }

    let mut broker = BROKER.lock();
    let current = broker
        .record
        .as_ref()
        .is_some_and(|record| record.handle == handle && record.generation == generation);
    if current && broker.lifecycle.mark_faulted(handle) {
        log::error!(
            "[net-stack] worker turn failed: generation={} handle={} result={:?}",
            generation,
            handle.0,
            result
        );
    }
    Err(WorkerTurnError::CallFailed)
}

fn tx_header(
    payload: &net::buf::PacketChain,
    input: NetStackTxInputV1,
) -> Result<NetStackTxHeaderV1, NetStackTxError> {
    if !input.valid()
        || input
            .payload_offset
            .checked_add(input.payload_len)
            .is_none_or(|end| end > payload.total_len() as u32)
    {
        return Err(NetStackTxError::InvalidInput);
    }
    let (handle, generation, call) = {
        let broker = BROKER.lock();
        let snapshot = broker.lifecycle.snapshot();
        if snapshot.state != NetStackState::Active || !snapshot.probed {
            return Err(NetStackTxError::StackUnavailable);
        }
        let Some(record) = broker.record.as_ref() else {
            return Err(NetStackTxError::StackUnavailable);
        };
        (record.handle, record.generation, record.call.clone())
    };

    let payload_pointer = payload as *const net::buf::PacketChain;
    let mut output = NetStackTxHeaderV1::new(generation, payload, input);
    let output_pointer = &mut output as *mut NetStackTxHeaderV1;
    let mut frame = NetStackCallV1::new(NET_STACK_OP_TX_HEADER, generation);
    frame.tx_header = output_pointer;
    let Some(output_range) = host_range(output_pointer) else {
        return Err(NetStackTxError::CallFailed);
    };
    let Some(payload_range) = host_range(payload_pointer) else {
        return Err(NetStackTxError::CallFailed);
    };
    let result = call.invoke(&mut frame, &[output_range, payload_range]);
    let valid = matches!(result, Ok(NET_STACK_CALL_STATUS_OK))
        && frame.valid(NET_STACK_OP_TX_HEADER, generation)
        && frame.tx_header == output_pointer
        && frame.ready == 0
        && frame.quiesced == 0
        && output.valid_header(generation, payload_pointer, &input)
        && output.fully_committed(payload);
    if valid {
        return Ok(output);
    }

    let mut broker = BROKER.lock();
    let current = broker
        .record
        .as_ref()
        .is_some_and(|record| record.handle == handle && record.generation == generation);
    if current && broker.lifecycle.mark_faulted(handle) {
        log::error!(
            "[net-stack] TX header call failed: generation={} handle={} result={:?}",
            generation,
            handle.0,
            result
        );
    }
    Err(NetStackTxError::CallFailed)
}

fn tx_fragment_header(
    payload: &net::buf::PacketChain,
    input: NetStackTxFragmentInputV1,
) -> Result<NetStackTxFragmentHeaderV1, NetStackTxError> {
    if !input.valid() || input.payload_len as usize > payload.total_len() {
        return Err(NetStackTxError::InvalidInput);
    }
    let (handle, generation, call) = {
        let broker = BROKER.lock();
        let snapshot = broker.lifecycle.snapshot();
        if snapshot.state != NetStackState::Active || !snapshot.probed {
            return Err(NetStackTxError::StackUnavailable);
        }
        let Some(record) = broker.record.as_ref() else {
            return Err(NetStackTxError::StackUnavailable);
        };
        (record.handle, record.generation, record.call.clone())
    };
    let payload_pointer = payload as *const net::buf::PacketChain;
    let mut output = NetStackTxFragmentHeaderV1::new(generation, payload, input);
    let output_pointer = &mut output as *mut NetStackTxFragmentHeaderV1;
    let mut frame = NetStackCallV1::new(NET_STACK_OP_TX_FRAGMENT_HEADER, generation);
    frame.reserved1[0] = output_pointer as usize as u64;
    let Some(output_range) = host_range(output_pointer) else {
        return Err(NetStackTxError::CallFailed);
    };
    let Some(payload_range) = host_range(payload_pointer) else {
        return Err(NetStackTxError::CallFailed);
    };
    let result = call.invoke(&mut frame, &[output_range, payload_range]);
    let valid = matches!(result, Ok(NET_STACK_CALL_STATUS_OK))
        && frame.valid(NET_STACK_OP_TX_FRAGMENT_HEADER, generation)
        && frame.reserved1[0] == output_pointer as usize as u64
        && frame.ready == 0
        && frame.quiesced == 0
        && output.valid_header(generation, payload_pointer, &input)
        && output.fully_committed(payload);
    if valid {
        return Ok(output);
    }
    let mut broker = BROKER.lock();
    let current = broker
        .record
        .as_ref()
        .is_some_and(|record| record.handle == handle && record.generation == generation);
    if current && broker.lifecycle.mark_faulted(handle) {
        log::error!(
            "[net-stack] TX fragment header call failed: generation={} handle={} result={:?}",
            generation,
            handle.0,
            result
        );
    }
    Err(NetStackTxError::CallFailed)
}

fn host_range<T>(pointer: *const T) -> Option<(usize, usize)> {
    let start = pointer as usize;
    let end = start.checked_add(core::mem::size_of::<T>())?;
    (start != 0 && start < end).then_some((start, end))
}

fn host_slice_range<T>(slice: &[T]) -> Option<(usize, usize)> {
    let start = slice.as_ptr() as usize;
    let bytes = core::mem::size_of::<T>().checked_mul(slice.len())?;
    let end = start.checked_add(bytes)?;
    (start != 0 && start < end).then_some((start, end))
}
