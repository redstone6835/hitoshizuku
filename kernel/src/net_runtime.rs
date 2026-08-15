//! 网络设备接管与 NetWorker 运行时。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

use errno::Errno;
use general::mm::{copy_from_user, copy_to_user};
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
use net::buf::CompletionToken;
use net::buf::{
    CompletionBatch, DropReason, NetBufPoolOwner, PacketBatch, PacketChain, PacketFragment,
    PacketMetadata, RxPoolPressure, RxRefillBatch, SharedNetBufPool, TxBatch, TxPacket,
};
use net::control::{
    AddressEntry, BindAddress, BindError, BindOptions, BindRequest, ConfigSnapshot, ConfigStore,
    InterfaceSnapshot, RouteEntry,
};
use net::device::{
    NET_QUEUE_CALL_RUST_ABI, NET_QUEUE_CALL_STATUS_OK, NET_QUEUE_OP_HAS_PENDING,
    NET_QUEUE_OP_POLL_RX, NET_QUEUE_OP_QUIESCE, NET_QUEUE_OP_RECLAIM_TX, NET_QUEUE_OP_REFILL_RX,
    NET_QUEUE_OP_SUBMIT_TX, NetDeviceHandle, NetDeviceRegisterError, NetDeviceRegisterErrorKind,
    NetDeviceRegistrar, NetDeviceRegistration, NetDeviceRemoveError, NetDeviceSnapshot,
    NetDeviceStats, NetDeviceTeardown, NetQueueCall, NetQueueEndpoint, NetQueueRegistration,
    NetStat, QueueIrqControl, QueueWakeHandle,
};
use net::flow::FlowKey;
use net::pipeline::{FrontendBatch, FrontendDisposition, FrontendPacket};
use net::queue::{NetQueuePair, RxBudget};
use net::ring::BoundedMpsc;
use net::runtime::WorkSignal;
use net::stack::{
    NetStackControlCommand, NetStackCooperativeTxResult, NetStackCooperativeUdpTx,
    NetStackFlowCommand, NetStackLocalOutputBatch, PendingNeighborTx, TxPlan,
};
use net::transport::{LocalUdpIngressError, PreparedTcpTx, PreparedUdpTx, TcpPath};
use net::{
    AddressFamily, Endpoint, FlowId, InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr, ListenGroup,
    ListenGroupId, OwnerRef, ShardId, SocketCommand, SocketError, SocketFacade, SocketKind,
    SocketRuntime, SocketTxCause, TransportProtocol,
};
use sched::sync::Spinlock;

static DEVICES: Spinlock<Vec<DeviceRecord>> = Spinlock::new(Vec::new());
static CONFIG_STORE: Spinlock<Option<Arc<ConfigStore>>> = Spinlock::new(None);
static NET_IOCTL_LOCK: Spinlock<()> = Spinlock::new(());
static NET_WORKER_STARTS: Spinlock<Vec<Option<Box<NetWorkerContext>>>> = Spinlock::new(Vec::new());
static PROTOCOL_CLUSTER: Spinlock<Option<Arc<ProtocolCluster>>> = Spinlock::new(None);
static NET_RUNTIME_STARTED: AtomicBool = AtomicBool::new(false);
static NET_ATTACH_LOCK: Spinlock<()> = Spinlock::new(());
static PINNED_QUEUE_FAILURES: AtomicU64 = AtomicU64::new(0);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static WORKER_TASKS: Spinlock<Vec<Arc<sched::Task>>> = Spinlock::new(Vec::new());
static REGISTRAR: KernelNetRegistrar = KernelNetRegistrar;
static SOCKET_RUNTIME_ADAPTER: KernelSocketRuntime = KernelSocketRuntime;
static COOPERATIVE_TX_ACTIVE: [AtomicBool; sched::NR_CPUS] =
    [const { AtomicBool::new(false) }; sched::NR_CPUS];
static COOPERATIVE_TX_SCRATCH: [Spinlock<Option<CooperativeTxScratch>>; sched::NR_CPUS] =
    [const { Spinlock::new(None) }; sched::NR_CPUS];

struct CooperativeTxScratch {
    tcp: NetStackLocalOutputBatch<PreparedTcpTx>,
    udp: NetStackLocalOutputBatch<NetStackCooperativeUdpTx>,
}

impl CooperativeTxScratch {
    fn new() -> Self {
        Self {
            tcp: NetStackLocalOutputBatch::new(),
            udp: NetStackLocalOutputBatch::new(),
        }
    }

    fn clear(&mut self) {
        self.tcp.clear();
        self.udp.clear();
    }
}

struct CooperativeTxGuard {
    cpu: usize,
}

impl CooperativeTxGuard {
    fn try_enter() -> Option<Self> {
        let cpu = sched::current_cpu_id();
        if cpu >= sched::NR_CPUS
            || COOPERATIVE_TX_ACTIVE[cpu]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return None;
        }
        Some(Self { cpu })
    }
}

impl Drop for CooperativeTxGuard {
    fn drop(&mut self) {
        COOPERATIVE_TX_ACTIVE[self.cpu].store(false, Ordering::Release);
    }
}
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static ARP_PROBE_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static ARP_PROBE_SENT: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static ARP_TX_COMPLETED: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static ARP_POOL_CONSERVED: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static ARP_REPLY_SEEN: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static UDP_PROBE_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static UDP_PROBE_COMPLETE: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static PHYSICAL_UDP_PROBE_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static PHYSICAL_UDP_REPLY_SEEN: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static PHYSICAL_UDP_POOL_CONSERVED: AtomicBool = AtomicBool::new(false);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static PHYSICAL_UDP_TX_SUBMITTED: AtomicBool = AtomicBool::new(false);
#[repr(align(64))]
struct CacheLine<T>(T);

/// 请求一个真实设备发送固定 ARP probe。只供内核网络测试使用。
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn request_arp_probe() {
    ARP_PROBE_REQUESTED.store(true, Ordering::Release);
    for task in WORKER_TASKS.lock().iter() {
        let _ = sched::activate_task(task);
    }
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn arp_probe_complete() -> bool {
    ARP_REPLY_SEEN.load(Ordering::Acquire)
        && ARP_TX_COMPLETED.load(Ordering::Acquire)
        && ARP_POOL_CONSERVED.load(Ordering::Acquire)
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn request_udp_loopback_probe() {
    UDP_PROBE_REQUESTED.store(true, Ordering::Release);
    if let Some(cluster) = PROTOCOL_CLUSTER.lock().as_ref() {
        cluster.coordinator().publish_work();
    }
    for task in WORKER_TASKS.lock().iter() {
        let _ = sched::activate_task(task);
    }
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn udp_loopback_probe_complete() -> bool {
    UDP_PROBE_COMPLETE.load(Ordering::Acquire)
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn request_physical_udp_probe() {
    ARP_PROBE_REQUESTED.store(true, Ordering::Release);
    PHYSICAL_UDP_PROBE_REQUESTED.store(true, Ordering::Release);
    if let Some(cluster) = PROTOCOL_CLUSTER.lock().as_ref() {
        cluster.coordinator().publish_work();
    }
    for task in WORKER_TASKS.lock().iter() {
        let _ = sched::activate_task(task);
    }
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn physical_udp_probe_complete() -> bool {
    PHYSICAL_UDP_REPLY_SEEN.load(Ordering::Acquire)
        && PHYSICAL_UDP_POOL_CONSERVED.load(Ordering::Acquire)
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn physical_udp_probe_state() -> (bool, bool) {
    (
        PHYSICAL_UDP_REPLY_SEEN.load(Ordering::Acquire),
        PHYSICAL_UDP_POOL_CONSERVED.load(Ordering::Acquire),
    )
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn physical_network_available() -> bool {
    DEVICES
        .lock()
        .iter()
        .any(|device| device.snapshot.name.as_ref() != "lo" && device.snapshot.running)
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub fn remove_loopback_for_test() -> Result<(), NetDeviceRemoveError> {
    // loopback 归 ELM 所有；通过 registrar 移除会绕过模块生命周期，导致其 queue
    // lease 继续指向已经释放的 pool。
    Err(NetDeviceRemoveError::Busy)
}

pub fn registrar() -> &'static dyn NetDeviceRegistrar {
    &REGISTRAR
}

struct DeviceRecord {
    handle: NetDeviceHandle,
    snapshot: NetDeviceSnapshot,
    queues: Option<Box<[NetQueueRegistration]>>,
    queue_stats: Vec<Arc<QueueRuntimeStats>>,
    irqs: Vec<Arc<dyn QueueIrqControl>>,
    started: bool,
    control: Arc<WorkerControl>,
}

struct WorkerControl {
    remove_requested: AtomicBool,
    remove_ready: AtomicBool,
    done: AtomicBool,
    worker_count: AtomicUsize,
    completed: AtomicUsize,
    tasks: Spinlock<Vec<Arc<sched::Task>>>,
}

impl WorkerControl {
    fn new() -> Self {
        Self {
            remove_requested: AtomicBool::new(false),
            remove_ready: AtomicBool::new(false),
            done: AtomicBool::new(false),
            worker_count: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            tasks: Spinlock::new(Vec::new()),
        }
    }
}

struct QueueRuntimeStats {
    poll_total: AtomicU64,
    budget_packet: AtomicU64,
    budget_byte: AtomicU64,
    budget_time: AtomicU64,
    rx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    rx_errors: AtomicU64,
    rx_dropped: AtomicU64,
    tx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    tx_errors: AtomicU64,
    tx_dropped: AtomicU64,
    rx_batch_1_8: AtomicU64,
    rx_batch_9_16: AtomicU64,
    rx_batch_17_31: AtomicU64,
    rx_batch_32: AtomicU64,
    descriptor_starved: AtomicU64,
    doorbell: AtomicU64,
    pool_local_recycle: AtomicU64,
    pool_remote_recycle: AtomicU64,
    rx_no_consumer: AtomicU64,
    irq_protocol_calls: AtomicU64,
    fatal_device_gone: AtomicU64,
    fatal_device_reset: AtomicU64,
    fatal_dma_fault: AtomicU64,
    fatal_ring_corrupt: AtomicU64,
    drop_reasons: [AtomicU64; DropReason::COUNT],
    protocol_tcp_delivered: AtomicU64,
    protocol_udp_delivered: AtomicU64,
    protocol_control_packets: AtomicU64,
    protocol_tx_formed: AtomicU64,
    protocol_dirty_runs: AtomicU64,
    protocol_timer_expired: AtomicU64,
    tcp_rx_pinned_bytes: AtomicU64,
    tcp_rx_compact_copy_bytes: AtomicU64,
    tcp_loopback_shared_bytes: AtomicU64,
    tcp_rx_pool_low_water_fallbacks: AtomicU64,
}

impl QueueRuntimeStats {
    fn new() -> Self {
        Self {
            poll_total: AtomicU64::new(0),
            budget_packet: AtomicU64::new(0),
            budget_byte: AtomicU64::new(0),
            budget_time: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            rx_errors: AtomicU64::new(0),
            rx_dropped: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            tx_errors: AtomicU64::new(0),
            tx_dropped: AtomicU64::new(0),
            rx_batch_1_8: AtomicU64::new(0),
            rx_batch_9_16: AtomicU64::new(0),
            rx_batch_17_31: AtomicU64::new(0),
            rx_batch_32: AtomicU64::new(0),
            descriptor_starved: AtomicU64::new(0),
            doorbell: AtomicU64::new(0),
            pool_local_recycle: AtomicU64::new(0),
            pool_remote_recycle: AtomicU64::new(0),
            rx_no_consumer: AtomicU64::new(0),
            irq_protocol_calls: AtomicU64::new(0),
            fatal_device_gone: AtomicU64::new(0),
            fatal_device_reset: AtomicU64::new(0),
            fatal_dma_fault: AtomicU64::new(0),
            fatal_ring_corrupt: AtomicU64::new(0),
            drop_reasons: core::array::from_fn(|_| AtomicU64::new(0)),
            protocol_tcp_delivered: AtomicU64::new(0),
            protocol_udp_delivered: AtomicU64::new(0),
            protocol_control_packets: AtomicU64::new(0),
            protocol_tx_formed: AtomicU64::new(0),
            protocol_dirty_runs: AtomicU64::new(0),
            protocol_timer_expired: AtomicU64::new(0),
            tcp_rx_pinned_bytes: AtomicU64::new(0),
            tcp_rx_compact_copy_bytes: AtomicU64::new(0),
            tcp_loopback_shared_bytes: AtomicU64::new(0),
            tcp_rx_pool_low_water_fallbacks: AtomicU64::new(0),
        }
    }
}

struct KernelNetRegistrar;

struct PinnedQueueAdapter {
    id: net::QueuePairId,
    caps: net::queue::NetQueueCaps,
    tx_produces_rx_synchronously: bool,
    call: crate::elm::PinnedNativeCall,
}

impl PinnedQueueAdapter {
    fn new(endpoint: net::device::PinnedNetQueueEndpoint) -> Self {
        let call = crate::elm::PinnedNativeCall::new(
            elm_model::ElmId(endpoint.owner_cell()),
            elm_model::Generation(endpoint.owner_generation()),
            endpoint.export_name(),
            endpoint.export_contract(),
            endpoint.export_version(),
            NET_QUEUE_CALL_RUST_ABI,
        )
        .expect("PinnedNetQueueEndpoint 必须在注册前通过身份校验");
        Self {
            id: endpoint.id(),
            caps: endpoint.caps(),
            tx_produces_rx_synchronously: endpoint.tx_produces_rx_synchronously(),
            call,
        }
    }

    fn invoke(&self, frame: &mut NetQueueCall) -> bool {
        let mut ranges = [(0usize, 0usize); 2];
        let range_count = match frame.opcode {
            NET_QUEUE_OP_REFILL_RX => {
                let Some(range) = host_range(frame.refill_batch) else {
                    return false;
                };
                ranges[0] = range;
                1
            }
            NET_QUEUE_OP_POLL_RX => {
                let Some(range) = host_range(frame.packet_batch) else {
                    return false;
                };
                ranges[0] = range;
                1
            }
            NET_QUEUE_OP_RECLAIM_TX => {
                let Some(range) = host_range(frame.completion_batch) else {
                    return false;
                };
                ranges[0] = range;
                1
            }
            NET_QUEUE_OP_SUBMIT_TX => {
                let (Some(batch), Some(pool)) =
                    (host_range(frame.tx_batch), host_range(frame.tx_header_pool))
                else {
                    return false;
                };
                ranges[0] = batch;
                ranges[1] = pool;
                2
            }
            NET_QUEUE_OP_HAS_PENDING | NET_QUEUE_OP_QUIESCE => 0,
            _ => return false,
        };
        // 队列 ABI 由每轮报文数和字节数边界协作限流，不把墙钟耗时当 CPU 配额。
        let result = crate::elm::invoke_pinned_native(
            &self.call,
            frame,
            &ranges[..range_count],
            crate::elm::NO_WATCHDOG_DEADLINE_NS,
        );
        let valid = frame.valid(frame.opcode, self.id);
        let success = matches!(result, Ok(NET_QUEUE_CALL_STATUS_OK)) && valid;
        if !success {
            let bit = 1u64 << frame.opcode.min(63);
            if PINNED_QUEUE_FAILURES.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                log::error!(
                    "[net] pinned queue call failed: queue={} opcode={} result={:?} frame_valid={}",
                    self.id.0,
                    frame.opcode,
                    result,
                    valid,
                );
            }
        }
        success
    }

    const fn fatal_rx_refill() -> net::queue::RxRefillResult {
        net::queue::RxRefillResult {
            posted: 0,
            descriptor_starved: false,
            fatal: Some(net::queue::QueueFatalError::DeviceGone),
        }
    }

    const fn fatal_rx_poll() -> net::queue::RxPollResult {
        net::queue::RxPollResult {
            packets: 0,
            bytes: 0,
            ring_empty: true,
            descriptor_starved: false,
            fatal: Some(net::queue::QueueFatalError::DeviceGone),
        }
    }

    const fn fatal_tx_reclaim() -> net::queue::TxReclaimResult {
        net::queue::TxReclaimResult {
            completions: 0,
            descriptors: 0,
            ring_empty: true,
            fatal: Some(net::queue::QueueFatalError::DeviceGone),
        }
    }

    const fn fatal_tx_submit() -> net::queue::TxSubmitResult {
        net::queue::TxSubmitResult {
            packets: 0,
            descriptors: 0,
            bytes: 0,
            queue_full: false,
            fatal: Some(net::queue::QueueFatalError::DeviceGone),
        }
    }
}

fn host_range<T>(pointer: *mut T) -> Option<(usize, usize)> {
    let start = pointer as usize;
    let end = start.checked_add(core::mem::size_of::<T>())?;
    (start != 0 && start < end).then_some((start, end))
}

impl NetQueuePair for PinnedQueueAdapter {
    fn id(&self) -> net::QueuePairId {
        self.id
    }

    fn caps(&self) -> net::queue::NetQueueCaps {
        self.caps
    }

    fn tx_produces_rx_synchronously(&self) -> bool {
        self.tx_produces_rx_synchronously
    }

    fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> net::queue::RxRefillResult {
        let original_len = batch.len();
        let mut frame = NetQueueCall::new(NET_QUEUE_OP_REFILL_RX, self.id);
        frame.refill_batch = batch;
        if !self.invoke(&mut frame) || usize::from(frame.rx_refill_result.posted) > original_len {
            return Self::fatal_rx_refill();
        }
        frame.rx_refill_result
    }

    fn poll_rx_batch(
        &mut self,
        budget: RxBudget,
        out: &mut PacketBatch,
    ) -> net::queue::RxPollResult {
        let mut frame = NetQueueCall::new(NET_QUEUE_OP_POLL_RX, self.id);
        frame.budget = budget;
        frame.packet_batch = out;
        if !self.invoke(&mut frame)
            || frame.rx_poll_result.packets > budget.packets
            || frame.rx_poll_result.bytes > budget.bytes
            || out.len() > usize::from(self.caps.max_rx_batch)
        {
            return Self::fatal_rx_poll();
        }
        frame.rx_poll_result
    }

    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> net::queue::TxReclaimResult {
        let mut frame = NetQueueCall::new(NET_QUEUE_OP_RECLAIM_TX, self.id);
        frame.completion_batch = out;
        if !self.invoke(&mut frame)
            || usize::from(frame.tx_reclaim_result.completions) > out.len()
            || out.len() > usize::from(self.caps.max_tx_batch)
        {
            return Self::fatal_tx_reclaim();
        }
        frame.tx_reclaim_result
    }

    fn submit_tx_batch(
        &mut self,
        batch: &mut TxBatch,
        header_pool: &mut NetBufPoolOwner,
    ) -> net::queue::TxSubmitResult {
        let original_len = batch.len();
        let mut frame = NetQueueCall::new(NET_QUEUE_OP_SUBMIT_TX, self.id);
        frame.tx_batch = batch;
        frame.tx_header_pool = header_pool;
        if !self.invoke(&mut frame) || usize::from(frame.tx_submit_result.packets) > original_len {
            return Self::fatal_tx_submit();
        }
        frame.tx_submit_result
    }

    fn has_pending_work(&mut self) -> bool {
        let mut frame = NetQueueCall::new(NET_QUEUE_OP_HAS_PENDING, self.id);
        self.invoke(&mut frame) && frame.pending
    }

    fn quiesce(&mut self) -> Result<(), net::queue::QueueFatalError> {
        let mut frame = NetQueueCall::new(NET_QUEUE_OP_QUIESCE, self.id);
        if !self.invoke(&mut frame) {
            // 生命周期已取得 exclusive token 时新数据面调用会被拒绝；此时资源回调仍可
            // 继续撤销 worker，模块 finalize 随后释放其私有 queue 状态。
            return Ok(());
        }
        frame.quiesce_result.map_or(Ok(()), Err)
    }
}

impl NetDeviceRegistrar for KernelNetRegistrar {
    fn register_device(
        &self,
        registration: NetDeviceRegistration,
    ) -> Result<NetDeviceHandle, NetDeviceRegisterError> {
        if registration.queues.is_empty()
            || registration.mtu == 0
            || registration
                .queues
                .iter()
                .enumerate()
                .any(|(index, queue)| {
                    queue.id.0 as usize != index
                        || queue.queue.id() != queue.id
                        || !queue.queue.caps().validate_data_queue()
                })
        {
            return Err(NetDeviceRegisterError {
                kind: NetDeviceRegisterErrorKind::InvalidRegistration,
                registration,
            });
        }
        let handle = registration.handle();
        let queue_stats = registration
            .queues
            .iter()
            .map(|_| Arc::new(QueueRuntimeStats::new()))
            .collect();
        let irqs = registration
            .queues
            .iter()
            .map(|queue| Arc::clone(&queue.irq))
            .collect();
        let snapshot = NetDeviceSnapshot {
            id: registration.id,
            name: registration.name.clone(),
            mac_address: registration.mac_address,
            mtu: registration.mtu,
            queue_pairs: registration.queues.len() as u16,
            running: registration.running,
            stats: NetDeviceStats::default(),
        };
        let queues = registration
            .queues
            .into_vec()
            .into_iter()
            .map(|registration| {
                let NetQueueRegistration {
                    id,
                    queue,
                    rx_pool,
                    tx_header_pool,
                    tx_payload_pool,
                    socket_tx_pool,
                    irq,
                } = registration;
                let queue: Box<dyn NetQueuePair> = match queue {
                    NetQueueEndpoint::Integrated(queue) => queue,
                    NetQueueEndpoint::Pinned(endpoint) => {
                        Box::new(PinnedQueueAdapter::new(endpoint))
                    }
                };
                NetQueueRegistration {
                    id,
                    queue: NetQueueEndpoint::Integrated(queue),
                    rx_pool,
                    tx_header_pool,
                    tx_payload_pool,
                    socket_tx_pool,
                    irq,
                }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        DEVICES.lock().push(DeviceRecord {
            handle,
            snapshot,
            queues: Some(queues),
            queue_stats,
            irqs,
            started: false,
            control: Arc::new(WorkerControl::new()),
        });
        publish_device_config();
        if NET_RUNTIME_STARTED.load(Ordering::Acquire) && elm_model::current_context().is_none() {
            reconcile_devices();
        }
        Ok(handle)
    }

    fn begin_remove(
        &self,
        handle: NetDeviceHandle,
    ) -> Result<NetDeviceTeardown, NetDeviceRemoveError> {
        let control = {
            let devices = DEVICES.lock();
            let Some(device) = devices.iter().find(|device| device.handle == handle) else {
                return Err(NetDeviceRemoveError::NoDevice);
            };
            if !device.started {
                drop(devices);
                let mut devices = DEVICES.lock();
                let index = devices
                    .iter()
                    .position(|device| device.handle == handle)
                    .unwrap();
                devices.remove(index);
                drop(devices);
                publish_device_config();
                return Ok(NetDeviceTeardown { handle });
            }
            Arc::clone(&device.control)
        };
        if control.remove_requested.swap(true, Ordering::AcqRel) {
            return Err(NetDeviceRemoveError::AlreadyRemoving);
        }
        let interface = {
            let mut devices = DEVICES.lock();
            let device = devices
                .iter_mut()
                .find(|device| device.handle == handle)
                .ok_or(NetDeviceRemoveError::NoDevice)?;
            device.snapshot.running = false;
            InterfaceId(device.snapshot.id.raw())
        };
        // 先从配置快照撤销可用接口和路由，再通知各 shard 失效动态状态。
        publish_device_config();
        let Some(cluster) = PROTOCOL_CLUSTER.lock().as_ref().cloned() else {
            return Err(NetDeviceRemoveError::Busy);
        };
        let invalidation = cluster.invalidate_interface(interface);
        let invalidation_deadline = sched::now_ns_direct().saturating_add(5_000_000_000);
        while !invalidation.done() && sched::now_ns_direct() < invalidation_deadline {
            let _ = sched::operation::sched_yield();
        }
        if !invalidation.done() {
            return Err(NetDeviceRemoveError::Busy);
        }
        control.remove_ready.store(true, Ordering::Release);
        for task in control.tasks.lock().iter() {
            let _ = sched::activate_task(task);
        }
        let deadline = sched::now_ns_direct().saturating_add(5_000_000_000);
        while !control.done.load(Ordering::Acquire) && sched::now_ns_direct() < deadline {
            let _ = sched::operation::sched_yield();
        }
        if !control.done.load(Ordering::Acquire) {
            return Err(NetDeviceRemoveError::Busy);
        }
        {
            let mut tasks = control.tasks.lock();
            tasks.clear();
            tasks.shrink_to_fit();
        }
        let mut devices = DEVICES.lock();
        let Some(index) = devices.iter().position(|device| device.handle == handle) else {
            return Err(NetDeviceRemoveError::NoDevice);
        };
        devices.remove(index);
        devices.shrink_to_fit();
        drop(devices);
        publish_device_config();
        Ok(NetDeviceTeardown { handle })
    }

    fn snapshot_devices(&self) -> Vec<NetDeviceSnapshot> {
        DEVICES
            .lock()
            .iter()
            .map(|device| {
                let mut snapshot = device.snapshot.clone();
                for stats in &device.queue_stats {
                    snapshot.stats.rx_bytes = snapshot
                        .stats
                        .rx_bytes
                        .saturating_add(stats.rx_bytes.load(Ordering::Relaxed));
                    snapshot.stats.rx_packets = snapshot
                        .stats
                        .rx_packets
                        .saturating_add(stats.rx_packets.load(Ordering::Relaxed));
                    snapshot.stats.rx_errors = snapshot
                        .stats
                        .rx_errors
                        .saturating_add(stats.rx_errors.load(Ordering::Relaxed));
                    snapshot.stats.rx_dropped = snapshot
                        .stats
                        .rx_dropped
                        .saturating_add(stats.rx_dropped.load(Ordering::Relaxed));
                    snapshot.stats.tx_bytes = snapshot
                        .stats
                        .tx_bytes
                        .saturating_add(stats.tx_bytes.load(Ordering::Relaxed));
                    snapshot.stats.tx_packets = snapshot
                        .stats
                        .tx_packets
                        .saturating_add(stats.tx_packets.load(Ordering::Relaxed));
                    snapshot.stats.tx_errors = snapshot
                        .stats
                        .tx_errors
                        .saturating_add(stats.tx_errors.load(Ordering::Relaxed));
                    snapshot.stats.tx_dropped = snapshot
                        .stats
                        .tx_dropped
                        .saturating_add(stats.tx_dropped.load(Ordering::Relaxed));
                }
                snapshot
            })
            .collect()
    }

    fn snapshot_stats(&self) -> Vec<NetStat> {
        let devices = DEVICES.lock();
        let mut output = Vec::new();
        for device in devices.iter() {
            for (queue_index, stats) in device.queue_stats.iter().enumerate() {
                let irq = device.irqs[queue_index].stats();
                let queue = net::QueuePairId(queue_index as u16);
                let values = [
                    ("budget_byte", stats.budget_byte.load(Ordering::Relaxed)),
                    ("budget_packet", stats.budget_packet.load(Ordering::Relaxed)),
                    ("budget_time", stats.budget_time.load(Ordering::Relaxed)),
                    (
                        "descriptor_starved",
                        stats.descriptor_starved.load(Ordering::Relaxed),
                    ),
                    ("doorbell", stats.doorbell.load(Ordering::Relaxed)),
                    (
                        "fatal_device_gone",
                        stats.fatal_device_gone.load(Ordering::Relaxed),
                    ),
                    (
                        "fatal_device_reset",
                        stats.fatal_device_reset.load(Ordering::Relaxed),
                    ),
                    (
                        "fatal_dma_fault",
                        stats.fatal_dma_fault.load(Ordering::Relaxed),
                    ),
                    (
                        "fatal_ring_corrupt",
                        stats.fatal_ring_corrupt.load(Ordering::Relaxed),
                    ),
                    ("irq_mask", irq.irq_mask),
                    (
                        "irq_protocol_calls",
                        stats.irq_protocol_calls.load(Ordering::Relaxed),
                    ),
                    ("irq_total", irq.irq_total),
                    ("irq_unmask", irq.irq_unmask),
                    (
                        "rx_no_consumer",
                        stats.rx_no_consumer.load(Ordering::Relaxed),
                    ),
                    ("rx_bytes", stats.rx_bytes.load(Ordering::Relaxed)),
                    ("rx_dropped", stats.rx_dropped.load(Ordering::Relaxed)),
                    ("rx_errors", stats.rx_errors.load(Ordering::Relaxed)),
                    ("poll_total", stats.poll_total.load(Ordering::Relaxed)),
                    (
                        "protocol_control_packets",
                        stats.protocol_control_packets.load(Ordering::Relaxed),
                    ),
                    (
                        "protocol_dirty_runs",
                        stats.protocol_dirty_runs.load(Ordering::Relaxed),
                    ),
                    (
                        "protocol_timer_expired",
                        stats.protocol_timer_expired.load(Ordering::Relaxed),
                    ),
                    (
                        "protocol_tx_formed",
                        stats.protocol_tx_formed.load(Ordering::Relaxed),
                    ),
                    (
                        "protocol_tcp_delivered",
                        stats.protocol_tcp_delivered.load(Ordering::Relaxed),
                    ),
                    (
                        "protocol_udp_delivered",
                        stats.protocol_udp_delivered.load(Ordering::Relaxed),
                    ),
                    (
                        "tcp_loopback_shared_bytes",
                        stats.tcp_loopback_shared_bytes.load(Ordering::Relaxed),
                    ),
                    (
                        "tcp_rx_compact_copy_bytes",
                        stats.tcp_rx_compact_copy_bytes.load(Ordering::Relaxed),
                    ),
                    (
                        "tcp_rx_pinned_bytes",
                        stats.tcp_rx_pinned_bytes.load(Ordering::Relaxed),
                    ),
                    (
                        "tcp_rx_pool_low_water_fallbacks",
                        stats
                            .tcp_rx_pool_low_water_fallbacks
                            .load(Ordering::Relaxed),
                    ),
                    (
                        "pool_local_recycle",
                        stats.pool_local_recycle.load(Ordering::Relaxed),
                    ),
                    (
                        "pool_remote_recycle",
                        stats.pool_remote_recycle.load(Ordering::Relaxed),
                    ),
                    (
                        "rx_batch_17_31",
                        stats.rx_batch_17_31.load(Ordering::Relaxed),
                    ),
                    ("rx_batch_1_8", stats.rx_batch_1_8.load(Ordering::Relaxed)),
                    ("rx_batch_32", stats.rx_batch_32.load(Ordering::Relaxed)),
                    ("rx_batch_9_16", stats.rx_batch_9_16.load(Ordering::Relaxed)),
                    ("rx_packets", stats.rx_packets.load(Ordering::Relaxed)),
                    ("tx_bytes", stats.tx_bytes.load(Ordering::Relaxed)),
                    ("tx_dropped", stats.tx_dropped.load(Ordering::Relaxed)),
                    ("tx_errors", stats.tx_errors.load(Ordering::Relaxed)),
                    ("tx_packets", stats.tx_packets.load(Ordering::Relaxed)),
                ];
                for (key, value) in values {
                    output.push(NetStat {
                        device: device.snapshot.id,
                        queue,
                        key,
                        value,
                    });
                }
                for reason in DropReason::ALL {
                    if reason == DropReason::None {
                        continue;
                    }
                    output.push(NetStat {
                        device: device.snapshot.id,
                        queue,
                        key: reason.stat_key(),
                        value: stats.drop_reasons[reason.index()].load(Ordering::Relaxed),
                    });
                }
            }
        }
        output.sort_by(|left, right| {
            (left.device, left.queue, left.key).cmp(&(right.device, right.queue, right.key))
        });
        output
    }
}

struct TaskWake {
    task: Arc<sched::Task>,
}

struct IngressPacket {
    egress: usize,
    interface: InterfaceId,
    local_mac: [u8; 6],
    packet: FrontendPacket,
}

enum IngressWork {
    Packet(IngressPacket),
    LocalTcp {
        interface: InterfaceId,
        work: PreparedTcpTx,
    },
    LocalUdp {
        interface: InterfaceId,
        work: PreparedUdpTx,
    },
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    UdpProbe {
        egress: usize,
        payload: PacketChain,
    },
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    PhysicalUdpProbe {
        egress: usize,
        payload: PacketChain,
    },
}

enum EgressWork {
    Packet(TxPacket),
    Plan(TxPlan),
    ControlFrame(Vec<u8>),
}

impl EgressWork {
    fn priority_class(&self) -> usize {
        let priority = match self {
            Self::Packet(packet) => return usize::from(packet.low_latency) * 3,
            Self::Plan(plan) if plan.low_latency => return 3,
            Self::Plan(plan) => plan.facade.socket_priority(),
            Self::ControlFrame(_) => return 3,
        };
        tx_priority_class(priority)
    }
}

pub(crate) const fn tx_priority_class(priority: i32) -> usize {
    match priority {
        i32::MIN..=0 => 0,
        1..=2 => 1,
        3..=5 => 2,
        _ => 3,
    }
}

enum PendingTxFrame {
    Bytes {
        bytes: Vec<u8>,
        completion: net::buf::CompletionToken,
        facade: Option<Arc<SocketFacade>>,
    },
}

enum PayloadChainError {
    Retry,
    Socket(SocketError),
}

fn allocate_payload_chain(
    pool: &mut NetBufPoolOwner,
    payload_len: usize,
    headroom: u16,
    max_fragments: usize,
    mut copy: impl FnMut(usize, &mut [u8]) -> Result<(), SocketError>,
) -> Result<PacketChain, PayloadChainError> {
    let capacity = usize::from(pool.buffer_capacity());
    let first_capacity = capacity.saturating_sub(usize::from(headroom));
    if first_capacity == 0 || max_fragments == 0 {
        return Err(PayloadChainError::Socket(SocketError::Buffer));
    }
    let required_fragments = if payload_len <= first_capacity {
        1
    } else {
        1 + (payload_len - first_capacity).div_ceil(capacity)
    };
    if required_fragments > max_fragments {
        return Err(PayloadChainError::Socket(SocketError::MessageTooLarge));
    }

    let mut chain = PacketChain::new();
    let mut copied = 0usize;
    for fragment_index in 0..required_fragments {
        let offset = if fragment_index == 0 { headroom } else { 0 };
        let available = if fragment_index == 0 {
            first_capacity
        } else {
            capacity
        };
        let len = payload_len.saturating_sub(copied).min(available);
        let mut lease = match pool.lease(offset, len as u16, PacketMetadata::default()) {
            Ok(lease) => lease,
            Err(_) => {
                drop(chain);
                pool.drain_remote();
                return Err(PayloadChainError::Retry);
            }
        };
        if len != 0
            && let Err(error) = copy(
                copied,
                lease.as_mut_slice().expect("TX payload lease 范围必须有效"),
            )
        {
            drop(lease);
            drop(chain);
            pool.drain_remote();
            return Err(PayloadChainError::Socket(error));
        }
        copied += len;
        if chain.push(PacketFragment::Exclusive(lease)).is_err() {
            drop(chain);
            pool.drain_remote();
            return Err(PayloadChainError::Socket(SocketError::Buffer));
        }
    }
    Ok(chain)
}

enum ControlWork {
    Socket(SocketCommand),
    ConnectTcp {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Endpoint,
        path: TcpPath,
        local_transport: bool,
    },
    InstallListener {
        transaction: Arc<ListenerInstall>,
    },
    RemoveListener {
        transaction: Arc<ListenerRemove>,
    },
    DiscardListener {
        group: ListenGroupId,
    },
    FinalizeListenerInstall {
        transaction: Arc<ListenerInstall>,
    },
    FinalizeListenerRemove {
        transaction: Arc<ListenerRemove>,
    },
    InterfaceGone {
        interface: InterfaceId,
        ack: Arc<InterfaceGoneBarrier>,
    },
    ResolveNeighbor(PendingNeighborTx),
    ResolveNeighborOwner(PendingNeighborTx),
    NeighborObserved {
        key: net::control::NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
    },
    NeighborObservedOwner {
        key: net::control::NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
    },
    Multicast {
        facade: Arc<SocketFacade>,
        membership: net::MulticastMembership,
        joined: bool,
    },
    TransportError {
        interface: InterfaceId,
        target: net::transport::ControlErrorTarget,
        error: net::transport::TransportControlError,
        now_ns: u64,
    },
    TransportErrorOwner {
        interface: InterfaceId,
        target: net::transport::ControlErrorTarget,
        error: net::transport::TransportControlError,
        now_ns: u64,
    },
    ReleaseBinding {
        facade: Arc<SocketFacade>,
        publish_closed: bool,
    },
    RemoveSocketMulticast {
        socket: net::SocketId,
    },
}

enum TcpReserveNext {
    Bind,
    Connect { peer: Endpoint, path: TcpPath },
    Listen { backlog: u32 },
}

struct InterfaceGoneBarrier {
    remaining: AtomicUsize,
}

impl InterfaceGoneBarrier {
    fn new(shards: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(shards),
        }
    }

    fn finish(&self) {
        self.remaining.fetch_sub(1, Ordering::AcqRel);
    }

    fn done(&self) -> bool {
        self.remaining.load(Ordering::Acquire) == 0
    }
}

struct ListenerInstall {
    facade: Arc<SocketFacade>,
    group: Arc<ListenGroup>,
    local: Endpoint,
    interface: Option<InterfaceId>,
    dual_stack: bool,
    sequence: u64,
    generation: u32,
    remaining: AtomicUsize,
    failed: AtomicBool,
    cluster: Arc<ProtocolCluster>,
}

impl ListenerInstall {
    fn finish(self: &Arc<Self>, result: Result<(), SocketError>) {
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        if self.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let _ = self.cluster.publish_control(
            ShardId(0),
            ControlWork::FinalizeListenerInstall {
                transaction: Arc::clone(self),
            },
        );
    }

    fn fail(&self) {
        self.group.close();
        for runtime in &self.cluster.shards {
            let mut work = ControlWork::DiscardListener {
                group: self.group.id(),
            };
            loop {
                match runtime.control.try_push(work) {
                    Ok(()) => {
                        runtime.publish_work();
                        break;
                    }
                    Err(pending) => {
                        work = pending;
                        runtime.publish_work();
                        let _ = sched::operation::sched_yield();
                    }
                }
            }
        }
        self.facade
            .complete_control(self.sequence, Err(SocketError::InvalidState));
    }

    fn complete(&self) {
        self.facade.install_listen_group(Arc::clone(&self.group));
        self.facade.publish_binding(
            OwnerRef::Listener {
                group: self.group.id(),
                generation: self.facade.generation(),
            },
            self.local,
            None,
            self.interface,
        );
        self.facade.complete_control(self.sequence, Ok(()));
    }
}

struct ListenerRemove {
    facade: Arc<SocketFacade>,
    group: ListenGroupId,
    remaining: AtomicUsize,
    cluster: Arc<ProtocolCluster>,
}

impl ListenerRemove {
    fn finish(self: &Arc<Self>) {
        if self.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let _ = self.cluster.publish_control(
            ShardId(0),
            ControlWork::FinalizeListenerRemove {
                transaction: Arc::clone(self),
            },
        );
    }
}

struct EgressChannel {
    interface: InterfaceId,
    tx_payload_pool: SharedNetBufPool,
    rings: [BoundedMpsc<EgressWork>; 4],
    pending: CacheLine<AtomicBool>,
    lifecycle: CacheLine<EgressLifecycle>,
    task: Spinlock<Option<Arc<sched::Task>>>,
    stats: Arc<QueueRuntimeStats>,
}

struct EgressLifecycle {
    active: AtomicBool,
    pushers: AtomicUsize,
}

impl EgressChannel {
    fn new(
        interface: InterfaceId,
        tx_payload_pool: SharedNetBufPool,
        stats: Arc<QueueRuntimeStats>,
    ) -> Self {
        Self {
            interface,
            tx_payload_pool,
            rings: core::array::from_fn(|_| BoundedMpsc::new(256)),
            pending: CacheLine(AtomicBool::new(false)),
            lifecycle: CacheLine(EgressLifecycle {
                active: AtomicBool::new(true),
                pushers: AtomicUsize::new(0),
            }),
            task: Spinlock::new(None),
            stats,
        }
    }

    fn set_task(&self, task: Arc<sched::Task>) {
        *self.task.lock() = Some(task);
    }

    fn try_push(&self, work: EgressWork) -> Result<(), EgressWork> {
        self.try_push_deferred(work)?;
        self.publish();
        Ok(())
    }

    fn try_push_deferred(&self, work: EgressWork) -> Result<(), EgressWork> {
        if !self.lifecycle.0.active.load(Ordering::Acquire) {
            return Err(work);
        }
        self.lifecycle.0.pushers.fetch_add(1, Ordering::Release);
        if !self.lifecycle.0.active.load(Ordering::Acquire) {
            self.lifecycle.0.pushers.fetch_sub(1, Ordering::Release);
            return Err(work);
        }
        let class = work.priority_class();
        let result = self.rings[class].try_push(work);
        self.lifecycle.0.pushers.fetch_sub(1, Ordering::Release);
        result
    }

    fn publish(&self) {
        if !self.pending.0.swap(true, Ordering::AcqRel) {
            if let Some(task) = self.task.lock().as_ref().cloned() {
                let _ = sched::activate_task(&task);
            }
        }
    }

    fn push_wait(&self, mut work: EgressWork) -> Result<(), EgressWork> {
        #[cfg(feature = "performance-profile")]
        let mut profile = None;
        loop {
            match self.try_push(work) {
                Ok(()) => return Ok(()),
                Err(pending) => {
                    #[cfg(feature = "performance-profile")]
                    if profile.is_none() {
                        profile = Some(profiling::scope(profiling::Event::NetEgressBackpressure));
                    }
                    if !self.lifecycle.0.active.load(Ordering::Acquire) {
                        return Err(pending);
                    }
                    work = pending;
                    if let Some(task) = self.task.lock().as_ref().cloned() {
                        let _ = sched::activate_task(&task);
                    }
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn finish_drain(&self) -> bool {
        self.pending.0.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        if self.rings.iter().any(|ring| !ring.is_empty()) {
            self.pending.0.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    fn has_pending(&self) -> bool {
        self.pending.0.load(Ordering::Acquire) || self.rings.iter().any(|ring| !ring.is_empty())
    }

    fn deactivate(&self) {
        self.lifecycle.0.active.store(false, Ordering::Release);
        while self.lifecycle.0.pushers.load(Ordering::Acquire) != 0 {
            let _ = sched::operation::sched_yield();
        }
        for ring in &self.rings {
            while let Some(work) = ring.try_pop() {
                fail_egress_work(work, SocketError::NetworkUnreachable);
            }
        }
        self.pending.0.store(false, Ordering::Release);
    }

    fn try_pop_class(&self, class: usize) -> Option<EgressWork> {
        self.rings[class].try_pop()
    }
}

fn fail_egress_work(work: EgressWork, error: SocketError) {
    match work {
        EgressWork::Packet(_) | EgressWork::ControlFrame(_) => {}
        EgressWork::Plan(plan) => plan.facade.set_pending_error(error),
    }
}

struct ProtocolRuntime {
    id: ShardId,
    cpu: usize,
    started: AtomicBool,
    ingress: BoundedMpsc<IngressWork>,
    control: BoundedMpsc<ControlWork>,
    dirty: BoundedMpsc<Arc<SocketFacade>>,
    lifecycle: BoundedMpsc<Arc<SocketFacade>>,
    queue_attach: BoundedMpsc<Box<WorkerContext>>,
    work_signal: WorkSignal,
    owner_task: Spinlock<Option<Arc<sched::Task>>>,
    deadline_registration: AtomicU64,
    deadline_ns: AtomicU64,
    timer_fired: AtomicBool,
    egress: Spinlock<Vec<Option<Arc<EgressChannel>>>>,
}

impl ProtocolRuntime {
    fn new(id: ShardId, cpu: usize, egress: Vec<Arc<EgressChannel>>) -> Self {
        Self {
            id,
            cpu,
            started: AtomicBool::new(false),
            ingress: BoundedMpsc::new(1024),
            control: BoundedMpsc::new(256),
            dirty: BoundedMpsc::new(4096),
            lifecycle: BoundedMpsc::new(4096),
            queue_attach: BoundedMpsc::new(64),
            work_signal: WorkSignal::new(),
            owner_task: Spinlock::new(None),
            deadline_registration: AtomicU64::new(0),
            deadline_ns: AtomicU64::new(0),
            timer_fired: AtomicBool::new(false),
            egress: Spinlock::new(egress.into_iter().map(Some).collect()),
        }
    }

    fn set_owner_task(&self, task: Arc<sched::Task>) {
        *self.owner_task.lock() = Some(task);
    }

    fn owner_task(&self) -> Option<Arc<sched::Task>> {
        self.owner_task.lock().as_ref().cloned()
    }

    fn append_egress(&self, egress: Arc<EgressChannel>) -> usize {
        let mut targets = self.egress.lock();
        let index = targets.len();
        targets.push(Some(egress));
        index
    }

    fn egress(&self, index: usize) -> Option<Arc<EgressChannel>> {
        self.egress
            .lock()
            .get(index)
            .and_then(Option::as_ref)
            .cloned()
    }

    fn egress_index(&self, interface: InterfaceId) -> Option<usize> {
        self.egress.lock().iter().position(|target| {
            target
                .as_ref()
                .is_some_and(|target| target.interface == interface)
        })
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn egress_snapshot(&self) -> Vec<Arc<EgressChannel>> {
        self.egress
            .lock()
            .iter()
            .filter_map(|target| target.as_ref().cloned())
            .collect()
    }

    fn remove_egress(&self, index: usize, expected: &Arc<EgressChannel>) -> bool {
        let mut targets = self.egress.lock();
        let Some(slot) = targets.get_mut(index) else {
            return false;
        };
        if !slot
            .as_ref()
            .is_some_and(|target| Arc::ptr_eq(target, expected))
        {
            return false;
        }
        *slot = None;
        while targets.last().is_some_and(Option::is_none) {
            targets.pop();
        }
        targets.shrink_to_fit();
        true
    }

    fn wake_owner(&self) {
        if let Some(task) = self.owner_task.lock().as_ref().cloned() {
            // 通用入口能原地撤销“仍是 current、但已标记 Sleeping”的睡眠准备；
            // 协议任务已有单核亲和性约束，不需要再使用一次性 CPU 提示。
            let _ = sched::activate_task(&task);
        }
    }

    fn publish_work(&self) {
        if self.work_signal.publish_work() {
            self.wake_owner();
        }
    }

    fn arm_timer(self: &Arc<Self>, deadline_ns: Option<u64>) {
        let deadline_ns = deadline_ns.unwrap_or(0);
        let current = self.deadline_registration.load(Ordering::Acquire);
        if current != 0 && self.deadline_ns.load(Ordering::Acquire) == deadline_ns {
            return;
        }
        if current != 0 {
            sched::cancel_deadline_observer(current);
            self.deadline_registration.store(0, Ordering::Release);
        }
        self.deadline_ns.store(deadline_ns, Ordering::Release);
        if deadline_ns == 0 {
            return;
        }
        let registration = sched::reserve_deadline_observer_id();
        self.deadline_registration
            .store(registration, Ordering::Release);
        let observer: Arc<dyn sched::DeadlineObserver> = self.clone();
        if !sched::register_deadline_observer(registration, deadline_ns, Arc::downgrade(&observer))
        {
            self.deadline_registration
                .compare_exchange(registration, 0, Ordering::AcqRel, Ordering::Acquire)
                .ok();
            self.deadline_ns.store(0, Ordering::Release);
            self.timer_fired.store(true, Ordering::Release);
            self.publish_work();
        }
    }

    fn try_push(&self, work: IngressWork) -> Result<(), IngressWork> {
        self.try_push_deferred(work)?;
        self.publish_ingress();
        Ok(())
    }

    fn try_push_deferred(&self, work: IngressWork) -> Result<(), IngressWork> {
        self.ingress.try_push(work)?;
        #[cfg(feature = "performance-profile")]
        profiling::observe(
            profiling::Metric::IngressRingDepth,
            self.ingress.len() as u64,
        );
        Ok(())
    }

    fn publish_ingress(&self) {
        self.publish_work();
    }

    fn publish_protocol_stats(&self, protocol_stats: net::flow::FlowShardStats) {
        for egress in self.egress.lock().iter().filter_map(Option::as_ref) {
            let stats = &egress.stats;
            stats
                .protocol_tcp_delivered
                .store(protocol_stats.tcp_delivered, Ordering::Relaxed);
            stats
                .protocol_udp_delivered
                .store(protocol_stats.udp_delivered, Ordering::Relaxed);
            stats
                .protocol_control_packets
                .store(protocol_stats.control_packets, Ordering::Relaxed);
            stats
                .protocol_tx_formed
                .store(protocol_stats.tx_formed, Ordering::Relaxed);
            stats
                .protocol_dirty_runs
                .store(protocol_stats.dirty_runs, Ordering::Relaxed);
            stats
                .protocol_timer_expired
                .store(protocol_stats.timer_expired, Ordering::Relaxed);
            stats
                .tcp_rx_pinned_bytes
                .store(protocol_stats.tcp_rx_pinned_bytes, Ordering::Relaxed);
            stats
                .tcp_rx_compact_copy_bytes
                .store(protocol_stats.tcp_rx_compact_copy_bytes, Ordering::Relaxed);
            stats
                .tcp_loopback_shared_bytes
                .store(protocol_stats.tcp_loopback_shared_bytes, Ordering::Relaxed);
            stats.tcp_rx_pool_low_water_fallbacks.store(
                protocol_stats.tcp_rx_pool_low_water_fallbacks,
                Ordering::Relaxed,
            );
        }
    }

    fn finish_drain(&self) -> bool {
        self.work_signal.finish_drain(|| {
            !self.ingress.is_empty()
                || !self.control.is_empty()
                || !self.dirty.is_empty()
                || !self.lifecycle.is_empty()
                || !self.queue_attach.is_empty()
                || self.timer_fired.load(Ordering::Acquire)
        })
    }
}

fn push_local_ingress(target: &ProtocolRuntime, mut work: IngressWork) {
    loop {
        match target.try_push(work) {
            Ok(()) => return,
            Err(pending) => {
                work = pending;
                target.publish_work();
                let _ = sched::operation::sched_yield();
            }
        }
    }
}

struct ProtocolCluster {
    shards: Box<[Arc<ProtocolRuntime>]>,
    rss_key: [u8; 40],
}

impl ProtocolCluster {
    fn coordinator(&self) -> &Arc<ProtocolRuntime> {
        &self.shards[0]
    }

    fn shard(&self, id: ShardId) -> Option<&Arc<ProtocolRuntime>> {
        self.shards.get(usize::from(id.0))
    }

    fn ingress_target(&self, hash: Option<u32>) -> &Arc<ProtocolRuntime> {
        let index = hash.map_or(0, |hash| hash as usize % self.shards.len());
        &self.shards[index]
    }

    fn local_ingress_target(&self, key: &net::flow::FlowKey) -> &Arc<ProtocolRuntime> {
        self.ingress_target(Some(net::flow::rss_hash(&self.rss_key, key)))
    }

    fn local_tcp_ingress_target(&self, key: &net::flow::FlowKey) -> &Arc<ProtocolRuntime> {
        self.ingress_target(Some(net::flow::local_transport_hash(&self.rss_key, key)))
    }

    fn owner_target(&self, owner: OwnerRef) -> &Arc<ProtocolRuntime> {
        match owner {
            OwnerRef::Flow { shard, .. } => self.shard(shard).unwrap_or_else(|| self.coordinator()),
            OwnerRef::Unassigned
            | OwnerRef::Bound { .. }
            | OwnerRef::Listener { .. }
            | OwnerRef::Closed { .. } => self.coordinator(),
        }
    }

    fn install_socket_tx_pool(&self, facade: &SocketFacade, interface: InterfaceId) -> bool {
        let runtime = self.owner_target(facade.owner());
        let Some(index) = runtime.egress_index(interface) else {
            return false;
        };
        let Some(egress) = runtime.egress(index) else {
            return false;
        };
        match facade.kind() {
            SocketKind::Stream => {
                facade.install_stream_tx_pool(Arc::clone(&egress.tx_payload_pool))
            }
            SocketKind::Datagram => {
                facade.install_datagram_tx_pool(Arc::clone(&egress.tx_payload_pool));
                true
            }
            SocketKind::Raw => false,
        }
    }

    fn publish_control(&self, target: ShardId, work: ControlWork) -> Result<(), ControlWork> {
        let Some(runtime) = self.shard(target) else {
            return Err(work);
        };
        let mut work = work;
        loop {
            match runtime.control.try_push(work) {
                Ok(()) => {
                    runtime.publish_work();
                    return Ok(());
                }
                Err(pending) => {
                    work = pending;
                    runtime.publish_work();
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn invalidate_interface(&self, interface: InterfaceId) -> Arc<InterfaceGoneBarrier> {
        let barrier = Arc::new(InterfaceGoneBarrier::new(self.shards.len()));
        for runtime in &self.shards {
            let mut work = ControlWork::InterfaceGone {
                interface,
                ack: Arc::clone(&barrier),
            };
            loop {
                match runtime.control.try_push(work) {
                    Ok(()) => {
                        runtime.publish_work();
                        break;
                    }
                    Err(pending) => {
                        work = pending;
                        runtime.publish_work();
                        let _ = sched::operation::sched_yield();
                    }
                }
            }
        }
        barrier
    }

    fn remove_egress(&self, index: usize, expected: &Arc<EgressChannel>) {
        for runtime in &self.shards {
            assert!(
                runtime.remove_egress(index, expected),
                "protocol shard egress 撤销不一致"
            );
        }
    }
}

impl sched::DeadlineObserver for ProtocolRuntime {
    fn deadline_expired(&self, registration: u64, _now_ns: u64) -> Option<u64> {
        if self
            .deadline_registration
            .compare_exchange(registration, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.deadline_ns.store(0, Ordering::Release);
            self.timer_fired.store(true, Ordering::Release);
            self.publish_work();
        }
        None
    }
}

struct KernelSocketRuntime;

impl KernelSocketRuntime {
    fn cluster(&self) -> Option<Arc<ProtocolCluster>> {
        PROTOCOL_CLUSTER.lock().as_ref().cloned()
    }

    fn publish_work(&self, runtime: &ProtocolRuntime) {
        runtime.publish_work();
    }

    fn loopback_config(&self, facade: &SocketFacade) -> Option<(Arc<ConfigSnapshot>, InterfaceId)> {
        let destination = match facade.kind() {
            SocketKind::Stream => facade.peer_endpoint(),
            SocketKind::Datagram => facade.next_datagram_destination(),
            SocketKind::Raw => None,
        }?;
        if destination.addr.is_unspecified() || destination.addr.is_multicast() {
            return None;
        }
        let store = CONFIG_STORE.lock().as_ref().cloned()?;
        let config = store.snapshot();
        let bound_source = facade
            .local_endpoint()
            .and_then(|endpoint| (!endpoint.addr.is_unspecified()).then_some(endpoint.addr));
        let route = config
            .route_with_source_policy(
                destination.addr,
                facade.socket_mark(),
                bound_source,
                facade.interface(),
                facade.free_bind(),
            )
            .ok()?;
        config
            .interfaces
            .iter()
            .any(|interface| interface.id == route.interface && interface.loopback)
            .then_some((config, route.interface))
    }

    fn take_cooperative_scratch(cpu: usize) -> Option<CooperativeTxScratch> {
        COOPERATIVE_TX_SCRATCH.get(cpu)?.try_lock()?.take()
    }

    fn return_cooperative_scratch(cpu: usize, mut scratch: CooperativeTxScratch) {
        scratch.clear();
        let Some(slot) = COOPERATIVE_TX_SCRATCH.get(cpu) else {
            return;
        };
        let mut slot = slot.lock();
        if slot.is_none() {
            *slot = Some(scratch);
        }
    }

    fn reclaim_cooperative_command(cpu: usize, command: NetStackFlowCommand) {
        if let NetStackFlowCommand::CooperativeSocketTx {
            tcp_output,
            udp_output,
            ..
        } = command
        {
            Self::return_cooperative_scratch(
                cpu,
                CooperativeTxScratch {
                    tcp: tcp_output,
                    udp: udp_output,
                },
            );
        }
    }

    fn publish_local_tcp(&self, cluster: &ProtocolCluster, work: PreparedTcpTx) {
        let source = Endpoint {
            addr: work.path.route.source,
            port: work.local_port,
        };
        let Some(key) = FlowKey::new(source, work.remote, TransportProtocol::Tcp) else {
            work.facade.set_pending_error(SocketError::NetworkDown);
            return;
        };
        let target = cluster.local_tcp_ingress_target(&key);
        push_local_ingress(
            &target,
            IngressWork::LocalTcp {
                interface: work.path.route.interface,
                work,
            },
        );
    }

    fn publish_local_udp(&self, cluster: &ProtocolCluster, work: PreparedUdpTx) {
        let source = Endpoint {
            addr: work.route.source,
            port: work.source_port,
        };
        let Some(key) = FlowKey::new(source, work.destination, TransportProtocol::Udp) else {
            work.payload
                .facade()
                .set_pending_error(SocketError::NetworkDown);
            return;
        };
        let target = cluster.local_ingress_target(&key);
        push_local_ingress(
            &target,
            IngressWork::LocalUdp {
                interface: work.route.interface,
                work,
            },
        );
    }

    fn try_cooperative_tx(&self, facade: &Arc<SocketFacade>, cause: SocketTxCause) -> bool {
        if cause == SocketTxCause::StreamLocalDirect {
            return false;
        }
        #[cfg(feature = "performance-profile")]
        let profile_start = profiling::read_counter();
        let Some(guard) = CooperativeTxGuard::try_enter() else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackNested, 1);
            return false;
        };
        // 同一 syscall 或 worker turn 的后续工作直接保留给 owner worker，避免再次完成
        // 路由查询、pool 安装和 ELM frame 准备后才发现本轮调用预算已经耗尽。
        let call_budget_exhausted = crate::net_stack::stack_call_budget_exhausted();
        #[cfg(feature = "performance-profile")]
        profiling::trace_point(
            profiling::Event::NetStackRequest,
            facade.id().counter,
            cause as u64 | (u64::from(call_budget_exhausted) << 8),
        );
        if call_budget_exhausted {
            #[cfg(feature = "performance-profile")]
            {
                profiling::observe(profiling::Metric::NetStackFallbackCallBudget, 1);
                profiling::observe(
                    match cause {
                        SocketTxCause::Datagram => profiling::Metric::NetStackFallbackDatagram,
                        SocketTxCause::StreamPayload => {
                            profiling::Metric::NetStackFallbackTcpPayload
                        }
                        SocketTxCause::StreamState => profiling::Metric::NetStackFallbackTcpState,
                        SocketTxCause::DrainRecheck => {
                            profiling::Metric::NetStackFallbackDrainRecheck
                        }
                        SocketTxCause::StreamLocalDirect => {
                            profiling::Metric::NetStackFallbackTcpPayload
                        }
                    },
                    1,
                );
            }
            return false;
        }
        let OwnerRef::Flow {
            shard,
            flow,
            generation,
        } = facade.owner()
        else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackOwner, 1);
            return false;
        };
        if generation != facade.generation() {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackGeneration, 1);
            return false;
        }
        let Some(cluster) = self.cluster() else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackUnavailable, 1);
            return false;
        };
        let Some((config, interface)) = self.loopback_config(facade) else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackNonLoopback, 1);
            return false;
        };
        facade.prepare_local_stream_send();
        if !cluster.install_socket_tx_pool(facade, interface) {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackTxPool, 1);
            return false;
        }
        if cause == SocketTxCause::StreamPayload && facade.local_stream_prefers_worker_batch() {
            // 大块本地流量由 owner worker 在一次调度轮次内聚合 payload、ACK 和
            // readiness；短消息继续走同步 local turn，避免请求/响应增加一次等待。
            return false;
        }
        let Some(scratch) = Self::take_cooperative_scratch(guard.cpu) else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackScratch, 1);
            return false;
        };
        #[cfg(feature = "performance-profile")]
        let profile_data = match facade.kind() {
            SocketKind::Stream => facade.stream_unsent_len() != 0,
            SocketKind::Datagram => facade.has_pending_datagram_tx(),
            SocketKind::Raw => false,
        };
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(
                if profile_data {
                    profiling::Metric::NetStackCooperativeDataCalls
                } else {
                    profiling::Metric::NetStackCooperativeStateCalls
                },
                1,
            );
        }
        let command = NetStackFlowCommand::CooperativeSocketTx {
            flow,
            facade: Arc::clone(facade),
            mark: facade.socket_mark(),
            config: Arc::as_ptr(&config),
            now_ns: sched::now_ns_direct(),
            limit: net::stack::NET_STACK_LOCAL_TURN_EFFECT_CAPACITY as u16,
            inline_local: true,
            tcp_output: scratch.tcp,
            udp_output: scratch.udp,
            result: None,
        };
        let config_start = Arc::as_ptr(&config) as usize;
        let Some(config_end) = config_start.checked_add(core::mem::size_of::<ConfigSnapshot>())
        else {
            Self::reclaim_cooperative_command(guard.cpu, command);
            return false;
        };
        let client = crate::net_stack::ElmShardTurnClient::new(shard);
        let command = match client.invoke_local_turn(command, &[(config_start, config_end)]) {
            Ok(command) => command,
            Err((_error, command)) => {
                #[cfg(feature = "performance-profile")]
                profiling::observe(
                    match _error {
                        crate::net_stack::ShardTurnError::Busy => {
                            profiling::Metric::NetStackFallbackElmBusy
                        }
                        crate::net_stack::ShardTurnError::StackUnavailable => {
                            profiling::Metric::NetStackFallbackUnavailable
                        }
                        crate::net_stack::ShardTurnError::CallFailed => {
                            profiling::Metric::NetStackFallbackElmFailed
                        }
                    },
                    1,
                );
                Self::reclaim_cooperative_command(guard.cpu, command);
                return false;
            }
        };
        let NetStackFlowCommand::CooperativeSocketTx {
            mut tcp_output,
            mut udp_output,
            result:
                Some(NetStackCooperativeTxResult {
                    more_work, stats, ..
                }),
            ..
        } = command
        else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::NetStackFallbackResult, 1);
            Self::reclaim_cooperative_command(guard.cpu, command);
            return false;
        };
        if let Some(runtime) = cluster.shard(shard) {
            runtime.publish_protocol_stats(stats);
        }
        let tcp_count = tcp_output.len();
        for index in 0..tcp_count {
            let Some(work) = tcp_output.take(index) else {
                continue;
            };
            self.publish_local_tcp(&cluster, work);
        }
        let udp_count = udp_output.len();
        for index in 0..udp_count {
            let Some(outcome) = udp_output.take(index) else {
                continue;
            };
            match outcome {
                NetStackCooperativeUdpTx::Prepared(work) => self.publish_local_udp(&cluster, work),
                NetStackCooperativeUdpTx::Failed(error, payload) => {
                    payload.facade().set_pending_error(error);
                }
            }
        }
        Self::return_cooperative_scratch(
            guard.cpu,
            CooperativeTxScratch {
                tcp: tcp_output,
                udp: udp_output,
            },
        );
        if more_work {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpTxWorkerContinuation, 1);
            let runtime = cluster.owner_target(facade.owner());
            runtime
                .dirty
                .try_push(Arc::clone(facade))
                .unwrap_or_else(|_| panic!("socket dirty queue 超出流表上限"));
            self.publish_work(runtime);
        } else {
            match facade.kind() {
                SocketKind::Stream => {
                    // local turn 内同步产生的 ACK、窗口更新和状态推进已经由本轮处理。
                    // 以返回后的 generation 作为完成水位，clear+fence recheck 仍会捕获
                    // 此后与其它 CPU 并发发布的真实新工作。
                    let satisfied_generation = facade.stream_tx_generation();
                    facade.finish_stream_tx_drain(satisfied_generation);
                }
                SocketKind::Datagram => facade.finish_tx_drain(),
                SocketKind::Raw => unreachable!("raw socket 不进入 cooperative turn"),
            }
        }
        #[cfg(feature = "performance-profile")]
        profiling::observe(
            if profile_data {
                profiling::Metric::NetStackCooperativeDataCycles
            } else {
                profiling::Metric::NetStackCooperativeStateCycles
            },
            profiling::read_counter().wrapping_sub(profile_start),
        );
        true
    }
}

impl SocketRuntime for KernelSocketRuntime {
    fn submit_control(&self, command: SocketCommand) -> Result<(), SocketCommand> {
        let Some(cluster) = self.cluster() else {
            return Err(command);
        };
        let runtime = cluster.coordinator();
        let mut work = ControlWork::Socket(command);
        loop {
            match runtime.control.try_push(work) {
                Ok(()) => {
                    self.publish_work(runtime);
                    return Ok(());
                }
                Err(pending) => {
                    work = pending;
                    self.publish_work(runtime);
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn prepare_stream_tx(&self, facade: &Arc<SocketFacade>) {
        let Some(cluster) = self.cluster() else {
            return;
        };
        let Some((_config, interface)) = self.loopback_config(facade) else {
            return;
        };
        facade.prepare_local_stream_send();
        if cluster.install_socket_tx_pool(facade, interface) {
            facade.mark_local_stream_tx_prepared();
        }
    }

    fn notify_tx(&self, facade: Arc<SocketFacade>, cause: SocketTxCause) {
        if self.try_cooperative_tx(&facade, cause) {
            return;
        }
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::NetStackCooperativeFallbacks, 1);
        let cluster = self.cluster().expect("socket runtime 尚未启动");
        let runtime = cluster.owner_target(facade.owner());
        runtime
            .dirty
            .try_push(facade)
            .unwrap_or_else(|_| panic!("socket dirty queue 超出流表上限"));
        self.publish_work(runtime);
    }

    fn notify_lifecycle(&self, facade: Arc<SocketFacade>) {
        let cluster = self.cluster().expect("socket runtime 尚未启动");
        let runtime = cluster.owner_target(facade.owner());
        let mut facade = facade;
        loop {
            match runtime.lifecycle.try_push(facade) {
                Ok(()) => {
                    self.publish_work(runtime);
                    return;
                }
                Err(pending) => {
                    facade = pending;
                    self.publish_work(runtime);
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn update_multicast(
        &self,
        facade: Arc<SocketFacade>,
        membership: net::MulticastMembership,
        joined: bool,
    ) -> Result<(), SocketError> {
        if joined
            && membership.interface.is_some_and(|interface| {
                CONFIG_STORE.lock().as_ref().is_none_or(|store| {
                    !store.snapshot().interfaces.iter().any(|candidate| {
                        candidate.id == interface && candidate.running && !candidate.loopback
                    })
                })
            })
        {
            return Err(SocketError::AddressUnavailable);
        }
        let cluster = self.cluster().ok_or(SocketError::RuntimeUnavailable)?;
        let runtime = cluster.coordinator();
        let mut work = ControlWork::Multicast {
            facade,
            membership,
            joined,
        };
        loop {
            match runtime.control.try_push(work) {
                Ok(()) => {
                    self.publish_work(runtime);
                    return Ok(());
                }
                Err(pending) => {
                    work = pending;
                    self.publish_work(runtime);
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn interface_by_name(&self, name: &[u8]) -> Option<InterfaceId> {
        DEVICES
            .lock()
            .iter()
            .find(|device| device.snapshot.name.as_bytes() == name)
            .map(|device| InterfaceId(device.snapshot.id.raw()))
    }
}

fn build_device_config(devices: &[DeviceRecord], generation: u64) -> ConfigSnapshot {
    let mut interfaces = Vec::new();
    let mut addresses = Vec::new();
    let mut routes = Vec::new();
    for device in devices {
        let snapshot = &device.snapshot;
        let interface = InterfaceId(snapshot.id.raw());
        interfaces.push(InterfaceSnapshot {
            id: interface,
            device: snapshot.id,
            mac_address: snapshot.mac_address,
            mtu: snapshot.mtu,
            running: snapshot.running,
            loopback: snapshot.name.as_ref() == "lo",
        });
        if snapshot.name.as_ref() == "lo" {
            addresses.push(AddressEntry {
                interface,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                prefix_len: 8,
                primary: true,
            });
            addresses.push(AddressEntry {
                interface,
                address: IpAddr::V6(Ipv6Addr::LOCALHOST),
                prefix_len: 128,
                primary: true,
            });
            routes.push(RouteEntry {
                table: 0,
                network: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)),
                prefix_len: 8,
                gateway: None,
                interface,
                metric: 0,
                mtu: Some(snapshot.mtu),
            });
            routes.push(RouteEntry {
                table: 0,
                network: IpAddr::V6(Ipv6Addr::LOCALHOST),
                prefix_len: 128,
                gateway: None,
                interface,
                metric: 0,
                mtu: Some(snapshot.mtu),
            });
        } else {
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            {
                addresses.push(AddressEntry {
                    interface,
                    address: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
                    prefix_len: 24,
                    primary: true,
                });
                routes.push(RouteEntry {
                    table: 0,
                    network: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 0)),
                    prefix_len: 24,
                    gateway: None,
                    interface,
                    metric: 0,
                    mtu: Some(snapshot.mtu),
                });
            }
        }
    }
    ConfigSnapshot::new(generation, interfaces, addresses, routes, Vec::new())
        .expect("启动网络配置无效")
}

pub(crate) fn autoconfig_egress_ready(
    config: &ConfigSnapshot,
    mut has_egress: impl FnMut(InterfaceId) -> bool,
) -> bool {
    config.interfaces.iter().all(|interface| {
        !interface.running
            || interface.loopback
            || config.addresses.iter().any(|entry| {
                entry.interface == interface.id && matches!(entry.address, IpAddr::V4(_))
            })
            || has_egress(interface.id)
    })
}

fn ipv4_mask_prefix(mask: Ipv4Addr) -> Option<u8> {
    let value = mask.as_u32();
    let prefix = value.leading_ones() as u8;
    let expected = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (value == expected).then_some(prefix)
}

fn ipv4_network(address: Ipv4Addr, prefix_len: u8) -> Ipv4Addr {
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    Ipv4Addr((address.as_u32() & mask).to_be_bytes())
}

const IFREQ_LEN: usize = 40;
const IFNAMSIZ: usize = 16;
const IFCONF_LEN: usize = 16;
const IFCONF_BUF_PTR_OFFSET: usize = 8;
const AF_INET: u16 = 2;
const ARPHRD_ETHER: u16 = 1;
const IFF_UP: u16 = 0x1;
const IFF_BROADCAST: u16 = 0x2;
const IFF_LOOPBACK: u16 = 0x8;
const IFF_RUNNING: u16 = 0x40;
const IFF_MULTICAST: u16 = 0x1000;
const SIOCGIFNAME: u32 = 0x8910;
const SIOCGIFCONF: u32 = 0x8912;
const SIOCGIFFLAGS: u32 = 0x8913;
const SIOCSIFFLAGS: u32 = 0x8914;
const SIOCGIFADDR: u32 = 0x8915;
const SIOCSIFADDR: u32 = 0x8916;
const SIOCGIFBRDADDR: u32 = 0x8919;
const SIOCSIFBRDADDR: u32 = 0x891a;
const SIOCGIFNETMASK: u32 = 0x891b;
const SIOCSIFNETMASK: u32 = 0x891c;
const SIOCGIFMTU: u32 = 0x8921;
const SIOCSIFMTU: u32 = 0x8922;
const SIOCGIFHWADDR: u32 = 0x8927;
const SIOCGIFINDEX: u32 = 0x8933;

#[derive(Clone, Copy)]
enum InterfaceConfigUpdate {
    Address(Ipv4Addr),
    Prefix(u8),
    Running(bool),
    Mtu(u32),
}

fn net_ioctl(cmd: u32, arg: usize) -> Result<usize, Errno> {
    if arg == 0 {
        return Err(Errno::EFAULT);
    }
    if cmd == SIOCGIFCONF {
        return ioctl_get_interface_config(arg);
    }
    let mut ifreq = [0u8; IFREQ_LEN];
    copy_from_user(arg, &mut ifreq).map_err(|error| error.as_errno())?;
    if cmd == SIOCGIFNAME {
        let index = i32::from_ne_bytes(ifreq[16..20].try_into().unwrap());
        let devices = DEVICES.lock();
        let device = devices
            .iter()
            .find(|device| device.snapshot.id.raw() == index as u32)
            .ok_or(Errno::ENXIO)?;
        write_ifreq_name(&mut ifreq, device.snapshot.name.as_bytes());
        copy_to_user(arg, &ifreq).map_err(|error| error.as_errno())?;
        return Ok(0);
    }

    let name = ifreq_name(&ifreq)?;
    let (interface, snapshot) = {
        let devices = DEVICES.lock();
        let device = devices
            .iter()
            .find(|device| device.snapshot.name.as_bytes() == name)
            .ok_or(Errno::ENODEV)?;
        (
            InterfaceId(device.snapshot.id.raw()),
            device.snapshot.clone(),
        )
    };
    match cmd {
        SIOCGIFFLAGS => {
            let config = CONFIG_STORE
                .lock()
                .as_ref()
                .cloned()
                .ok_or(Errno::ENODEV)?
                .snapshot();
            let state = config
                .interfaces
                .iter()
                .find(|candidate| candidate.id == interface)
                .ok_or(Errno::ENODEV)?;
            let mut flags = IFF_MULTICAST;
            if state.loopback {
                flags |= IFF_LOOPBACK;
            } else {
                flags |= IFF_BROADCAST;
            }
            if state.running {
                flags |= IFF_UP | IFF_RUNNING;
            }
            ifreq[16..18].copy_from_slice(&flags.to_ne_bytes());
        }
        SIOCSIFFLAGS => {
            let flags = u16::from_ne_bytes(ifreq[16..18].try_into().unwrap());
            update_interface_config(
                interface,
                InterfaceConfigUpdate::Running(flags & IFF_UP != 0),
            )?;
            return Ok(0);
        }
        SIOCGIFADDR | SIOCGIFNETMASK | SIOCGIFBRDADDR => {
            let config = CONFIG_STORE
                .lock()
                .as_ref()
                .cloned()
                .ok_or(Errno::ENODEV)?
                .snapshot();
            let address = config
                .addresses
                .iter()
                .find_map(|entry| {
                    (entry.interface == interface)
                        .then_some(entry)
                        .and_then(|entry| match entry.address {
                            IpAddr::V4(address) => Some((address, entry.prefix_len)),
                            IpAddr::V6(_) => None,
                        })
                })
                .ok_or(Errno::EADDRNOTAVAIL)?;
            let value = match cmd {
                SIOCGIFADDR => address.0,
                SIOCGIFNETMASK => ipv4_prefix_mask(address.1),
                SIOCGIFBRDADDR => Ipv4Addr(
                    (address.0.as_u32() | !ipv4_prefix_mask(address.1).as_u32()).to_be_bytes(),
                ),
                _ => unreachable!(),
            };
            write_ifreq_ipv4(&mut ifreq, value);
        }
        SIOCSIFADDR => {
            update_interface_config(
                interface,
                InterfaceConfigUpdate::Address(read_ifreq_ipv4(&ifreq)?),
            )?;
            return Ok(0);
        }
        SIOCSIFNETMASK => {
            let mask = read_ifreq_ipv4(&ifreq)?;
            let prefix = ipv4_mask_prefix(mask).ok_or(Errno::EINVAL)?;
            update_interface_config(interface, InterfaceConfigUpdate::Prefix(prefix))?;
            return Ok(0);
        }
        SIOCSIFBRDADDR => {
            let _ = read_ifreq_ipv4(&ifreq)?;
            return Ok(0);
        }
        SIOCGIFMTU => ifreq[16..20].copy_from_slice(&(snapshot.mtu as i32).to_ne_bytes()),
        SIOCSIFMTU => {
            let mtu = i32::from_ne_bytes(ifreq[16..20].try_into().unwrap());
            if mtu <= 0 {
                return Err(Errno::EINVAL);
            }
            update_interface_config(interface, InterfaceConfigUpdate::Mtu(mtu as u32))?;
            return Ok(0);
        }
        SIOCGIFHWADDR => {
            ifreq[16..18].copy_from_slice(&ARPHRD_ETHER.to_ne_bytes());
            ifreq[18..24].copy_from_slice(&snapshot.mac_address);
        }
        SIOCGIFINDEX => {
            ifreq[16..20].copy_from_slice(&(interface.0 as i32).to_ne_bytes());
        }
        _ => return Err(Errno::EOPNOTSUPP),
    }
    copy_to_user(arg, &ifreq).map_err(|error| error.as_errno())?;
    Ok(0)
}

fn netlink_address_snapshot() -> Vec<AddressEntry> {
    CONFIG_STORE
        .lock()
        .as_ref()
        .map(|store| store.snapshot().addresses.clone())
        .unwrap_or_default()
}

fn ioctl_get_interface_config(arg: usize) -> Result<usize, Errno> {
    let mut ifconf = [0u8; IFCONF_LEN];
    copy_from_user(arg, &mut ifconf).map_err(|error| error.as_errno())?;
    let buffer_len = i32::from_ne_bytes(ifconf[..4].try_into().unwrap()).max(0) as usize;
    let buffer = usize::from_ne_bytes(
        ifconf[IFCONF_BUF_PTR_OFFSET..IFCONF_BUF_PTR_OFFSET + core::mem::size_of::<usize>()]
            .try_into()
            .unwrap(),
    );
    let devices: Vec<NetDeviceSnapshot> = DEVICES
        .lock()
        .iter()
        .map(|device| device.snapshot.clone())
        .collect();
    let config = CONFIG_STORE.lock().as_ref().map(|store| store.snapshot());

    let available = if buffer == 0 {
        devices.len()
    } else {
        devices.len().min(buffer_len / IFREQ_LEN)
    };
    if buffer != 0 {
        for (index, device) in devices.iter().take(available).enumerate() {
            let mut ifreq = [0u8; IFREQ_LEN];
            write_ifreq_name(&mut ifreq, device.name.as_bytes());
            ifreq[16..18].copy_from_slice(&AF_INET.to_ne_bytes());
            let interface = InterfaceId(device.id.raw());
            if let Some(address) = config.as_ref().and_then(|snapshot| {
                snapshot.addresses.iter().find_map(|entry| {
                    (entry.interface == interface)
                        .then_some(entry)
                        .and_then(|entry| {
                            if let IpAddr::V4(address) = entry.address {
                                Some(address)
                            } else {
                                None
                            }
                        })
                })
            }) {
                ifreq[20..24].copy_from_slice(&address.0);
            }
            let offset = index
                .checked_mul(IFREQ_LEN)
                .and_then(|offset| buffer.checked_add(offset))
                .ok_or(Errno::EFAULT)?;
            copy_to_user(offset, &ifreq).map_err(|error| error.as_errno())?;
        }
    }

    ifconf[..4].copy_from_slice(&((available * IFREQ_LEN) as i32).to_ne_bytes());
    copy_to_user(arg, &ifconf).map_err(|error| error.as_errno())?;
    Ok(0)
}

fn ifreq_name(ifreq: &[u8; IFREQ_LEN]) -> Result<&[u8], Errno> {
    let end = ifreq[..IFNAMSIZ]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(IFNAMSIZ);
    (end != 0).then_some(&ifreq[..end]).ok_or(Errno::EINVAL)
}

fn write_ifreq_name(ifreq: &mut [u8; IFREQ_LEN], name: &[u8]) {
    ifreq[..IFNAMSIZ].fill(0);
    let len = name.len().min(IFNAMSIZ - 1);
    ifreq[..len].copy_from_slice(&name[..len]);
}

fn read_ifreq_ipv4(ifreq: &[u8; IFREQ_LEN]) -> Result<Ipv4Addr, Errno> {
    let family = u16::from_ne_bytes(ifreq[16..18].try_into().unwrap());
    if family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    Ok(Ipv4Addr(ifreq[20..24].try_into().unwrap()))
}

fn write_ifreq_ipv4(ifreq: &mut [u8; IFREQ_LEN], address: Ipv4Addr) {
    ifreq[16..32].fill(0);
    ifreq[16..18].copy_from_slice(&AF_INET.to_ne_bytes());
    ifreq[20..24].copy_from_slice(&address.0);
}

fn ipv4_prefix_mask(prefix_len: u8) -> Ipv4Addr {
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    Ipv4Addr(mask.to_be_bytes())
}

fn update_interface_config(
    interface: InterfaceId,
    update: InterfaceConfigUpdate,
) -> Result<(), Errno> {
    let _guard = NET_IOCTL_LOCK.lock();
    let store = CONFIG_STORE.lock().as_ref().cloned().ok_or(Errno::ENODEV)?;
    store
        .update(|current| {
            let mut interfaces = current.interfaces.clone();
            let state = interfaces
                .iter_mut()
                .find(|candidate| candidate.id == interface)
                .ok_or(net::control::ConfigError::InvalidInterface)?;
            let mut addresses = current.addresses.clone();
            let mut routes = current.routes.entries().to_vec();
            let old_address = addresses.iter().find_map(|entry| {
                (entry.interface == interface)
                    .then_some(entry)
                    .and_then(|entry| match entry.address {
                        IpAddr::V4(address) => Some((address, entry.prefix_len)),
                        IpAddr::V6(_) => None,
                    })
            });
            if matches!(
                update,
                InterfaceConfigUpdate::Address(_) | InterfaceConfigUpdate::Prefix(_)
            ) && let Some((address, prefix_len)) = old_address
            {
                let network = IpAddr::V4(ipv4_network(address, prefix_len));
                routes.retain(|route| {
                    !(route.interface == interface
                        && route.gateway.is_none()
                        && route.network == network
                        && route.prefix_len == prefix_len)
                });
            }
            match update {
                InterfaceConfigUpdate::Address(address) => {
                    let prefix_len = old_address.map_or(32, |entry| entry.1);
                    addresses.retain(|entry| {
                        entry.interface != interface || !matches!(entry.address, IpAddr::V4(_))
                    });
                    if address != Ipv4Addr::UNSPECIFIED {
                        addresses.push(AddressEntry {
                            interface,
                            address: IpAddr::V4(address),
                            prefix_len,
                            primary: true,
                        });
                    }
                }
                InterfaceConfigUpdate::Prefix(prefix_len) => {
                    let (address, _) =
                        old_address.ok_or(net::control::ConfigError::InvalidAddress)?;
                    if let Some(entry) = addresses.iter_mut().find(|entry| {
                        entry.interface == interface && matches!(entry.address, IpAddr::V4(_))
                    }) {
                        entry.prefix_len = prefix_len;
                    }
                    if address == Ipv4Addr::UNSPECIFIED {
                        return Err(net::control::ConfigError::InvalidAddress);
                    }
                }
                InterfaceConfigUpdate::Running(running) => state.running = running,
                InterfaceConfigUpdate::Mtu(mtu) => {
                    state.mtu = mtu;
                    for route in &mut routes {
                        if route.interface == interface && route.mtu.is_some() {
                            route.mtu = Some(mtu);
                        }
                    }
                }
            }
            let new_address = addresses.iter().find_map(|entry| {
                (entry.interface == interface)
                    .then_some(entry)
                    .and_then(|entry| match entry.address {
                        IpAddr::V4(address) => Some((address, entry.prefix_len)),
                        IpAddr::V6(_) => None,
                    })
            });
            if matches!(
                update,
                InterfaceConfigUpdate::Address(_) | InterfaceConfigUpdate::Prefix(_)
            ) && let Some((address, prefix_len)) = new_address
            {
                routes.push(RouteEntry {
                    table: 0,
                    network: IpAddr::V4(ipv4_network(address, prefix_len)),
                    prefix_len,
                    gateway: None,
                    interface,
                    metric: 0,
                    mtu: Some(state.mtu),
                });
            }
            ConfigSnapshot::new_with_dns(
                current.generation.saturating_add(1),
                interfaces,
                addresses,
                routes,
                current.policy.clone(),
                current.dns_servers.clone(),
            )
        })
        .map_err(|_| Errno::EINVAL)?;
    if let Some(device) = DEVICES
        .lock()
        .iter_mut()
        .find(|device| device.snapshot.id.raw() == interface.0)
    {
        match update {
            InterfaceConfigUpdate::Running(running) => device.snapshot.running = running,
            InterfaceConfigUpdate::Mtu(mtu) => device.snapshot.mtu = mtu,
            InterfaceConfigUpdate::Address(_) | InterfaceConfigUpdate::Prefix(_) => {}
        }
    }
    Ok(())
}

// ── netlink 配置更新（RTM_NEWADDR/NEWROUTE/NEWLINK 写操作）───────────────────

/// 把 ConfigError 映射为负 errno（NLMSG_ERROR 语义）。
fn map_config_error(error: net::control::ConfigError) -> i32 {
    let code = match error {
        net::control::ConfigError::InvalidInterface => Errno::ENODEV,
        net::control::ConfigError::InvalidAddress | net::control::ConfigError::InvalidRoute => {
            Errno::EINVAL
        }
        net::control::ConfigError::GatewayUnreachable => Errno::ENETUNREACH,
        net::control::ConfigError::TooManyRouteTables
        | net::control::ConfigError::TooManyPolicyRules => Errno::ENOBUFS,
        net::control::ConfigError::MissingMainRouteTable => Errno::EINVAL,
        net::control::ConfigError::GenerationNotIncreasing => Errno::EAGAIN,
        net::control::ConfigError::NoRoute | net::control::ConfigError::NoSourceAddress => {
            Errno::ESRCH
        }
    };
    -i32::from(code)
}

/// netlink 配置写请求的真正执行者（由 vfs netlink_socket 经 handler 注入调用）。
fn netlink_config_update(
    request: &vfs::netlink_socket::NetlinkConfigRequest,
) -> Result<(), i32> {
    let _guard = NET_IOCTL_LOCK.lock();
    let store = CONFIG_STORE
        .lock()
        .as_ref()
        .cloned()
        .ok_or(-i32::from(Errno::ENODEV))?;
    let current = store.snapshot();

    // ── 前置校验（自由 errno 检查，闭包内只产生 ConfigError）──────────────
    let interface = match request {
        vfs::netlink_socket::NetlinkConfigRequest::AddAddress { interface, .. }
        | vfs::netlink_socket::NetlinkConfigRequest::DelAddress { interface, .. }
        | vfs::netlink_socket::NetlinkConfigRequest::AddRoute { interface, .. }
        | vfs::netlink_socket::NetlinkConfigRequest::DelRoute { interface, .. }
        | vfs::netlink_socket::NetlinkConfigRequest::SetLinkRunning { interface, .. }
        | vfs::netlink_socket::NetlinkConfigRequest::SetLinkMtu { interface, .. } => *interface,
    };
    if !current
        .interfaces
        .iter()
        .any(|candidate| candidate.id == interface)
    {
        return Err(-i32::from(Errno::ENODEV));
    }
    match request {
        vfs::netlink_socket::NetlinkConfigRequest::AddAddress {
            address,
            prefix_len,
            ..
        } => {
            let max_prefix = match address {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            if *prefix_len > max_prefix {
                return Err(-i32::from(Errno::EINVAL));
            }
        }
        vfs::netlink_socket::NetlinkConfigRequest::DelAddress {
            interface,
            address,
            ..
        } => {
            let present = current
                .addresses
                .iter()
                .any(|entry| entry.interface == *interface && entry.address == *address);
            if !present {
                return Err(-i32::from(Errno::EADDRNOTAVAIL));
            }
        }
        vfs::netlink_socket::NetlinkConfigRequest::AddRoute {
            network,
            gateway,
            ..
        } => {
            if let Some(gateway) = gateway
                && !same_family_public(*network, *gateway)
            {
                return Err(-i32::from(Errno::EINVAL));
            }
        }
        vfs::netlink_socket::NetlinkConfigRequest::DelRoute {
            table,
            network,
            prefix_len,
            gateway,
            interface,
        } => {
            let present = current.routes.entries().iter().any(|route| {
                route.table == *table
                    && route.network == *network
                    && route.prefix_len == *prefix_len
                    && route.gateway == *gateway
                    && route.interface == *interface
            });
            if !present {
                return Err(-i32::from(Errno::ESRCH));
            }
        }
        vfs::netlink_socket::NetlinkConfigRequest::SetLinkRunning { .. }
        | vfs::netlink_socket::NetlinkConfigRequest::SetLinkMtu { .. } => {}
    }
    let interface_mtu = current
        .interfaces
        .iter()
        .find(|candidate| candidate.id == interface)
        .map(|candidate| candidate.mtu)
        .unwrap_or(1500);

    store
        .update(|current| {
            let mut addresses = current.addresses.clone();
            let mut routes = current.routes.entries().to_vec();
            match request {
                vfs::netlink_socket::NetlinkConfigRequest::AddAddress {
                    interface,
                    address,
                    prefix_len,
                } => {
                    addresses.retain(|entry| {
                        !(entry.interface == *interface && entry.address == *address)
                    });
                    addresses.push(AddressEntry {
                        interface: *interface,
                        address: *address,
                        prefix_len: *prefix_len,
                        primary: true,
                    });
                    // Linux 语义：新地址自动补直连路由（仅 IPv4，IPv6 由 DAD/RA 管理）。
                    if let IpAddr::V4(address) = address {
                        routes.push(RouteEntry {
                            table: 0,
                            network: IpAddr::V4(ipv4_network(*address, *prefix_len)),
                            prefix_len: *prefix_len,
                            gateway: None,
                            interface: *interface,
                            metric: 0,
                            mtu: Some(interface_mtu),
                        });
                    }
                }
                vfs::netlink_socket::NetlinkConfigRequest::DelAddress {
                    interface,
                    address,
                    prefix_len,
                } => {
                    addresses.retain(|entry| {
                        !(entry.interface == *interface && entry.address == *address)
                    });
                    // 删除关联的直连路由。
                    if let IpAddr::V4(address) = address {
                        let network = ipv4_network(*address, *prefix_len);
                        routes.retain(|route| {
                            !(route.interface == *interface
                                && route.gateway.is_none()
                                && route.network == IpAddr::V4(network)
                                && route.prefix_len == *prefix_len)
                        });
                    }
                }
                vfs::netlink_socket::NetlinkConfigRequest::AddRoute {
                    table,
                    network,
                    prefix_len,
                    gateway,
                    interface,
                    metric,
                } => {
                    routes.push(RouteEntry {
                        table: *table,
                        network: *network,
                        prefix_len: *prefix_len,
                        gateway: *gateway,
                        interface: *interface,
                        metric: *metric,
                        mtu: Some(interface_mtu),
                    });
                }
                vfs::netlink_socket::NetlinkConfigRequest::DelRoute {
                    table,
                    network,
                    prefix_len,
                    gateway,
                    interface,
                } => {
                    routes.retain(|route| {
                        !(route.table == *table
                            && route.network == *network
                            && route.prefix_len == *prefix_len
                            && route.gateway == *gateway
                            && route.interface == *interface)
                    });
                }
                vfs::netlink_socket::NetlinkConfigRequest::SetLinkRunning { interface, running } => {
                    let mut interfaces = current.interfaces.clone();
                    if let Some(candidate) = interfaces
                        .iter_mut()
                        .find(|candidate| candidate.id == *interface)
                    {
                        candidate.running = *running;
                    }
                    return ConfigSnapshot::new_with_dns(
                        current.generation.saturating_add(1),
                        interfaces,
                        addresses,
                        routes,
                        current.policy.clone(),
                        current.dns_servers.clone(),
                    );
                }
                vfs::netlink_socket::NetlinkConfigRequest::SetLinkMtu { interface, mtu } => {
                    let mut interfaces = current.interfaces.clone();
                    if let Some(candidate) = interfaces
                        .iter_mut()
                        .find(|candidate| candidate.id == *interface)
                    {
                        candidate.mtu = *mtu;
                    }
                    for route in &mut routes {
                        if route.interface == *interface && route.mtu.is_some() {
                            route.mtu = Some(*mtu);
                        }
                    }
                    return ConfigSnapshot::new_with_dns(
                        current.generation.saturating_add(1),
                        interfaces,
                        addresses,
                        routes,
                        current.policy.clone(),
                        current.dns_servers.clone(),
                    );
                }
            }
            ConfigSnapshot::new_with_dns(
                current.generation.saturating_add(1),
                current.interfaces.clone(),
                addresses,
                routes,
                current.policy.clone(),
                current.dns_servers.clone(),
            )
        })
        .map_err(map_config_error)?;
    // 设备快照同步（running/mtu 变更反映到设备层）。
    let link_interface = match request {
        vfs::netlink_socket::NetlinkConfigRequest::SetLinkRunning { interface, .. }
        | vfs::netlink_socket::NetlinkConfigRequest::SetLinkMtu { interface, .. } => {
            Some(*interface)
        }
        _ => None,
    };
    if let Some(interface) = link_interface
        && let Some(device) = DEVICES
            .lock()
            .iter_mut()
            .find(|device| device.snapshot.id.raw() == interface.0)
    {
        match request {
            vfs::netlink_socket::NetlinkConfigRequest::SetLinkRunning { running, .. } => {
                device.snapshot.running = *running;
            }
            vfs::netlink_socket::NetlinkConfigRequest::SetLinkMtu { mtu, .. } => {
                device.snapshot.mtu = *mtu;
            }
            _ => {}
        }
    }
    netlink_broadcast_config_change(request);
    Ok(())
}

fn same_family_public(left: IpAddr, right: IpAddr) -> bool {
    matches!(
        (left, right),
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
    )
}

/// 配置变化后向订阅者广播 RTM_* 事件。
fn netlink_broadcast_config_change(request: &vfs::netlink_socket::NetlinkConfigRequest) {
    use alloc::vec;
    let (msg_type, payload): (u16, Vec<u8>) = match request {
        vfs::netlink_socket::NetlinkConfigRequest::AddAddress { .. } => {
            (vfs::netlink_socket::NETLINK_MSG_RTM_NEWADDR, vec![])
        }
        vfs::netlink_socket::NetlinkConfigRequest::DelAddress { .. } => {
            (vfs::netlink_socket::NETLINK_MSG_RTM_DELADDR, vec![])
        }
        vfs::netlink_socket::NetlinkConfigRequest::AddRoute { .. } => {
            (vfs::netlink_socket::NETLINK_MSG_RTM_NEWROUTE, vec![])
        }
        vfs::netlink_socket::NetlinkConfigRequest::DelRoute { .. } => {
            (vfs::netlink_socket::NETLINK_MSG_RTM_DELROUTE, vec![])
        }
        vfs::netlink_socket::NetlinkConfigRequest::SetLinkRunning { .. }
        | vfs::netlink_socket::NetlinkConfigRequest::SetLinkMtu { .. } => {
            (vfs::netlink_socket::NETLINK_MSG_RTM_NEWLINK, vec![])
        }
    };
    // 最小消息：nlmsghdr(16) + 空载荷。订阅者按类型与 seq=0/pid=0 识别事件。
    let mut message = Vec::with_capacity(16 + payload.len());
    message.extend_from_slice(&((16 + payload.len()) as u32).to_ne_bytes());
    message.extend_from_slice(&msg_type.to_ne_bytes());
    message.extend_from_slice(&0u16.to_ne_bytes()); // flags
    message.extend_from_slice(&0u32.to_ne_bytes()); // seq
    message.extend_from_slice(&0u32.to_ne_bytes()); // pid
    message.extend_from_slice(&payload);
    vfs::netlink_socket::netlink_event_broadcast(msg_type, message);
}

// ── netlink 快照 provider（RTM_GETROUTE / RTM_GETNEIGH / procfs）──────────────

fn netlink_route_snapshot() -> Vec<net::control::RouteEntry> {
    CONFIG_STORE
        .lock()
        .as_ref()
        .map(|store| store.snapshot().routes.entries().to_vec())
        .unwrap_or_default()
}

fn netlink_neighbor_snapshot() -> Vec<vfs::netlink_socket::NeighborSnapshot> {
    net::control::neighbor_snapshot()
        .into_iter()
        .map(|entry| vfs::netlink_socket::NeighborSnapshot {
            interface: entry.interface,
            address: entry.address,
            mac: entry.mac,
            nud_state: entry.nud_state,
        })
        .collect()
}

fn publish_device_config() {
    let store = CONFIG_STORE.lock().as_ref().cloned();
    let Some(store) = store else {
        return;
    };
    // 设备注册和移除可能从 ELM 生命周期钩子调用此函数。发布的快照属于 host 网络栈，
    // 生命周期长于对应的 driver 代际，因此其中的分配不能继承当前 cell 的隐式记账归属。
    let _accounting = allocator::suspend_implicit_allocation_accounting();
    let current = store.snapshot();
    let generation = current.generation.saturating_add(1);
    let devices = DEVICES.lock();
    let mut next = build_device_config(&devices, generation);
    let running = next
        .interfaces
        .iter()
        .filter(|interface| interface.running)
        .map(|interface| interface.id)
        .collect::<Vec<_>>();
    for address in current.addresses.iter().copied() {
        if running.contains(&address.interface) && !next.addresses.contains(&address) {
            next.addresses.push(address);
        }
    }
    let mut routes = next.routes.entries().to_vec();
    for route in current.routes.entries().iter().copied() {
        if running.contains(&route.interface) && !routes.contains(&route) {
            routes.push(route);
        }
    }
    next = ConfigSnapshot::new_with_dns(
        generation,
        next.interfaces,
        next.addresses,
        routes,
        current.policy.clone(),
        current.dns_servers.clone(),
    )
    .expect("网络设备配置重建失败");
    store.publish(next).expect("网络设备配置发布失败");
}

impl QueueWakeHandle for TaskWake {
    fn wake(&self) {
        sched::defer_task_wake(&self.task);
    }
}

struct WorkerContext {
    initialized: bool,
    queue: Option<Box<dyn NetQueuePair>>,
    rx_pool: Option<NetBufPoolOwner>,
    tx_header_pool: Option<NetBufPoolOwner>,
    tx_payload_pool: Option<SharedNetBufPool>,
    irq: Arc<dyn QueueIrqControl>,
    rx_batch: PacketBatch,
    pending_rx_batches: VecDeque<PacketBatch>,
    spare_rx_batches: Vec<PacketBatch>,
    refill_batch: RxRefillBatch,
    completion_batch: CompletionBatch,
    tx_batch: TxBatch,
    retry_egress: VecDeque<EgressWork>,
    pending_tx_frames: VecDeque<PendingTxFrame>,
    ingress_device: net::NetDeviceId,
    interface: InterfaceId,
    local_mac: [u8; 6],
    protocol_cluster: Arc<ProtocolCluster>,
    egress_index: usize,
    egress: Arc<EgressChannel>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    owner_shard: ShardId,
    local_ingress: VecDeque<IngressWork>,
    rss_generation: u32,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    arp_probe_enabled: bool,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    arp_probe_done: bool,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_queued: bool,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_queued: bool,
    control: Arc<WorkerControl>,
    stats: Arc<QueueRuntimeStats>,
}

struct PendingWorker {
    registration: NetQueueRegistration,
    ingress_device: net::NetDeviceId,
    interface: InterfaceId,
    local_mac: [u8; 6],
    runtime: Arc<ProtocolRuntime>,
    egress: Arc<EgressChannel>,
    egress_index: usize,
    control: Arc<WorkerControl>,
    stats: Arc<QueueRuntimeStats>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    arp_probe_enabled: bool,
}

struct NetWorkerContext {
    runtime: Arc<ProtocolRuntime>,
    cluster: Arc<ProtocolCluster>,
    config: Arc<ConfigStore>,
    protocol: crate::net_stack::ElmShardTurnClient,
    frontend_packets: Vec<FrontendPacket>,
    tcp_output: Vec<PreparedTcpTx>,
    cooperative_scratch: Vec<CooperativeTxScratch>,
    inline_stream_pool_installs: Vec<(Arc<SocketFacade>, InterfaceId)>,
    local_ingress: VecDeque<IngressWork>,
    pending: [Option<IngressWork>; 32],
    turn_control_commands: TurnControlCommands,
    turn_control_meta: Vec<TurnControlMeta>,
    turn_commands: TurnCommands,
    turn_meta: Vec<TurnCommandMeta>,
    turn_tx_plans: TurnTxPlans,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_flow: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_sender: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_pending: Option<(usize, PacketChain)>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_polls_remaining: u8,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_flow: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_sender: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_pending: Option<(usize, PacketChain)>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_polls_remaining: u8,
    local_queues: Vec<Box<WorkerContext>>,
}

enum TurnCommandMeta {
    RawRx {
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
    },
    Frontend {
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
    },
    Reassembly {
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
    },
    LocalTcp,
    LocalUdp,
    PlanTx,
    StreamDirty {
        facade: Arc<SocketFacade>,
        generation: u64,
    },
    StreamDirtyLocal {
        facade: Arc<SocketFacade>,
        generation: u64,
    },
    UdpTx {
        facade: Arc<SocketFacade>,
        finish_drain: bool,
    },
    RawTx {
        facade: Arc<SocketFacade>,
        finish_drain: bool,
    },
    NeighborEnqueue,
    NeighborObserved,
    InterfaceInvalidated,
    InterfaceNeighbors {
        ack: Arc<InterfaceGoneBarrier>,
    },
    TransportError,
    TcpLifecycle {
        facade: Arc<SocketFacade>,
        release_binding: bool,
    },
    DatagramClose {
        facade: Arc<SocketFacade>,
    },
    RawClose {
        facade: Arc<SocketFacade>,
    },
    BindRaw {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
    },
    BindUdp {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
    },
    ReconnectUdp {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Endpoint,
        interface: Option<InterfaceId>,
    },
    ResolveTcpPath {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        peer: Endpoint,
        options: BindOptions,
    },
    ConnectTcp {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Endpoint,
        interface: InterfaceId,
    },
    ListenTcp {
        transaction: Arc<ListenerInstall>,
    },
    CloseListener {
        transaction: Option<Arc<ListenerRemove>>,
    },
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    UdpProbeBindReceiver,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    UdpProbeBindSender,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    UdpProbeForm {
        egress: usize,
    },
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    UdpProbeRecv,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    PhysicalUdpProbeBind,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    PhysicalUdpProbeForm {
        egress: usize,
    },
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    PhysicalUdpProbeRecv,
}

enum TurnControlMeta {
    Dad,
    Dhcp,
    DadConflict,
    DhcpPacket {
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
    },
    RemoveAutoconfigInterface,
    ReleaseBinding {
        facade: Option<Arc<SocketFacade>>,
        publish_closed: bool,
    },
    NeighborOwner {
        work: PendingNeighborTx,
    },
    NeighborObservedOwner {
        key: net::control::NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
    },
    TransportErrorOwner {
        interface: InterfaceId,
        target: net::transport::ControlErrorTarget,
        error: net::transport::TransportControlError,
        now_ns: u64,
    },
    JoinMulticast {
        interface: InterfaceId,
        group: IpAddr,
    },
    LeaveMulticast {
        group: IpAddr,
    },
    MulticastGroups {
        interface: InterfaceId,
    },
    RemoveInterfaceMulticast,
    RemoveSocketMulticast,
    ReserveUdp {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        options: BindOptions,
    },
    ReserveTcp {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        next: TcpReserveNext,
    },
    FlowShardTcpConnect {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Endpoint,
        path: TcpPath,
        local_transport: bool,
    },
    AllocateListener {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        backlog: u32,
    },
    InstallListener {
        transaction: Arc<ListenerInstall>,
    },
    RemoveListener {
        transaction: Arc<ListenerRemove>,
    },
}

struct PendingCommandBatch<T> {
    ready: net::stack::NetStackCommandBatch<T>,
    deferred: VecDeque<T>,
}

impl<T> PendingCommandBatch<T> {
    fn new() -> Self {
        Self {
            ready: net::stack::NetStackCommandBatch::new(),
            deferred: VecDeque::new(),
        }
    }

    fn len(&self) -> usize {
        self.ready.len().saturating_add(self.deferred.len())
    }

    fn is_empty(&self) -> bool {
        self.ready.is_empty() && self.deferred.is_empty()
    }

    fn push(&mut self, command: T) {
        if let Err(command) = self.ready.push(command) {
            self.deferred.push_back(command);
        }
    }

    fn move_prefix_into(
        &mut self,
        target: &mut net::stack::NetStackCommandBatch<T>,
        limit: usize,
    ) -> Result<usize, ()> {
        if !target.is_empty() {
            return Err(());
        }
        let count = self
            .len()
            .min(limit)
            .min(net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY);
        let ready_count = self.ready.len().min(count);
        self.ready.move_prefix_into(target, ready_count)?;
        for _ in ready_count..count {
            let command = self
                .deferred
                .pop_front()
                .expect("延后命令数量必须与待执行计数一致");
            target
                .push(command)
                .unwrap_or_else(|_| unreachable!("ELM 命令前缀容量已校验"));
        }
        Ok(count)
    }

    fn prepend_from(
        &mut self,
        prefix: net::stack::NetStackCommandBatch<T>,
    ) -> net::stack::NetStackCommandBatch<T> {
        let mut scratch = core::mem::replace(&mut self.ready, prefix);
        let mut ready_tail = Vec::with_capacity(scratch.len());
        scratch.drain_into_vec(&mut ready_tail);
        let mut deferred = VecDeque::new();
        for command in ready_tail {
            if let Err(command) = self.ready.push(command) {
                deferred.push_back(command);
            }
        }
        deferred.append(&mut self.deferred);
        self.deferred = deferred;
        scratch
    }
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub(crate) fn verify_pending_command_batch_overflow() {
    let capacity = net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY;
    let mut pending = PendingCommandBatch::new();
    for value in 0..capacity + 9 {
        pending.push(value);
    }
    assert_eq!(pending.len(), capacity + 9);

    let first_count = capacity - 3;
    let mut turn = net::stack::NetStackCommandBatch::new();
    assert_eq!(
        pending
            .move_prefix_into(&mut turn, first_count)
            .expect("首次命令前缀必须可移动"),
        first_count
    );
    for index in 0..first_count {
        assert_eq!(turn.take(index), Some(index));
    }
    turn.clear();

    let mut retry = net::stack::NetStackCommandBatch::new();
    retry
        .push(capacity + 100)
        .unwrap_or_else(|_| unreachable!());
    retry
        .push(capacity + 101)
        .unwrap_or_else(|_| unreachable!());
    let scratch = pending.prepend_from(retry);
    assert!(scratch.is_empty());

    let mut observed = Vec::new();
    while !pending.is_empty() {
        let mut turn = net::stack::NetStackCommandBatch::new();
        let count = pending
            .move_prefix_into(&mut turn, capacity)
            .expect("后续命令前缀必须可移动");
        for index in 0..count {
            observed.push(turn.take(index).expect("已移动命令必须保持连续"));
        }
    }
    let mut expected = alloc::vec![capacity + 100, capacity + 101];
    expected.extend(first_count..capacity + 9);
    assert_eq!(observed, expected);
}

struct TurnCommands(
    PendingCommandBatch<NetStackFlowCommand>,
    Option<net::stack::NetStackCommandBatch<NetStackFlowCommand>>,
);

struct TurnControlCommands(
    PendingCommandBatch<NetStackControlCommand>,
    Option<net::stack::NetStackCommandBatch<NetStackControlCommand>>,
);

struct TurnTxPlans(Option<net::stack::TxPlanBatch>);

// 缓冲区只从启动表移动到绑定 CPU 的 worker 一次；其中的调用指针仅由 owner
// worker 在同步调用期间创建和使用。
unsafe impl Send for TurnCommands {}
unsafe impl Send for TurnControlCommands {}
unsafe impl Send for TurnTxPlans {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerTurn {
    Idle,
    Pending,
    Removed,
}

fn on_elm_lifecycle_event(event: crate::elm::ElmLifecycleEvent) {
    match event {
        crate::elm::ElmLifecycleEvent::CellLoaded { .. } => reconcile_devices(),
    }
}

/// 调度器初始化完成后启动允许空设备集合的网络 host 和协议 worker。
pub fn start_workers() {
    assert!(
        crate::elm::register_lifecycle_observer("net-runtime", on_elm_lifecycle_event),
        "无法注册 ELM 生命周期观察者"
    );
    vfs::net_socket::install_net_ioctl_handler(net_ioctl);
    vfs::netlink_socket::install_address_snapshot_provider(netlink_address_snapshot);
    vfs::netlink_socket::install_route_snapshot_provider(netlink_route_snapshot);
    vfs::netlink_socket::install_neighbor_snapshot_provider(netlink_neighbor_snapshot);
    vfs::netlink_socket::install_netlink_config_handler(netlink_config_update);
    vfs::net_socket::install_net_realtime_clock(crate::vdso::realtime_ns);
    let boot = net::stack::boot_config().expect("网络 stack 启动配置未安装");
    let online = sched::online_cpu_mask();
    for cpu in 0..sched::NR_CPUS {
        if online & (1u64 << cpu) != 0 {
            let mut slot = COOPERATIVE_TX_SCRATCH[cpu].lock();
            if slot.is_none() {
                *slot = Some(CooperativeTxScratch::new());
            }
        }
    }
    // ELM 可按启动上限预建状态；host 只激活设备实际提供 queue pair 对应的前缀，
    // 避免单 RX queue 后继续做没有硬件并行收益的软件跨核分发。
    let mut protocol_cpus = (0..sched::NR_CPUS)
        .filter(|cpu| online & (1u64 << cpu) != 0)
        .collect::<Vec<_>>();
    assert!(!protocol_cpus.is_empty(), "NetWorker 没有 active CPU");
    let negotiated_queue_pairs = DEVICES
        .lock()
        .iter()
        .map(|device| usize::from(device.snapshot.queue_pairs))
        .max()
        .unwrap_or(1)
        .max(1);
    let protocol_shards = usize::from(boot.active_cpu_count()).min(negotiated_queue_pairs);
    assert!(
        protocol_shards != 0 && protocol_shards <= protocol_cpus.len(),
        "网络协议 shard 数量与在线 CPU/queue 能力不一致"
    );
    protocol_cpus.truncate(protocol_shards);
    let devices = DEVICES.lock();
    let config = Arc::new(ConfigStore::new(build_device_config(&devices, 1)));
    *CONFIG_STORE.lock() = Some(Arc::clone(&config));
    drop(devices);
    let control_plane = crate::net_stack::ElmControlPlane::new();
    assert!(
        control_plane.configure_active_shards(protocol_shards),
        "net.stack 有效 shard 数配置失败"
    );
    assert!(
        control_plane.initialize_autoconfig(&config.snapshot(), sched::now_ns_direct()),
        "net.stack 自动配置状态初始化失败"
    );
    let runtimes = protocol_cpus
        .iter()
        .enumerate()
        .map(|(index, cpu)| {
            Arc::new(ProtocolRuntime::new(
                ShardId(index as u16),
                *cpu,
                Vec::new(),
            ))
        })
        .collect::<Vec<_>>();
    let cluster = Arc::new(ProtocolCluster {
        shards: runtimes.clone().into_boxed_slice(),
        rss_key: *boot.rss_key(),
    });
    let mut worker_tasks = Vec::with_capacity(runtimes.len());
    for runtime in &runtimes {
        let protocol = crate::net_stack::ElmShardTurnClient::new(runtime.id);
        let slot = {
            let mut starts = NET_WORKER_STARTS.lock();
            let slot = starts.len();
            starts.push(Some(Box::new(NetWorkerContext {
                runtime: Arc::clone(runtime),
                cluster: Arc::clone(&cluster),
                config: Arc::clone(&config),
                protocol,
                frontend_packets: Vec::with_capacity(32),
                tcp_output: Vec::with_capacity(32),
                cooperative_scratch: Vec::with_capacity(8),
                inline_stream_pool_installs: Vec::with_capacity(4),
                local_ingress: VecDeque::with_capacity(128),
                pending: core::array::from_fn(|_| None),
                turn_control_commands: TurnControlCommands(
                    PendingCommandBatch::new(),
                    Some(net::stack::NetStackCommandBatch::new()),
                ),
                turn_control_meta: Vec::with_capacity(
                    net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY,
                ),
                turn_commands: TurnCommands(
                    PendingCommandBatch::new(),
                    Some(net::stack::NetStackCommandBatch::new()),
                ),
                turn_meta: Vec::with_capacity(net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY),
                turn_tx_plans: TurnTxPlans(Some(net::stack::TxPlanBatch::new())),
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                udp_probe_flow: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                udp_probe_sender: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                udp_probe_pending: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                udp_probe_polls_remaining: 0,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                physical_udp_probe_flow: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                physical_udp_probe_sender: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                physical_udp_probe_pending: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                physical_udp_probe_polls_remaining: 0,
                local_queues: Vec::new(),
            })));
            slot
        };
        let task = sched::kthread_create(
            net_worker_entry,
            slot,
            sched::SchedParams {
                nice: -5,
                slice_ns: 0,
            },
        );
        // 每个协议 shard 的队列、timer、waker 与 ELM pinned call 都归属于
        // runtime.cpu；允许迁移会让多个 shard 争用同一 CPU 的调用槽。
        task.set_cpu_affinity(1u64 << runtime.cpu);
        runtime.set_owner_task(Arc::clone(&task));
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        WORKER_TASKS.lock().push(Arc::clone(&task));
        worker_tasks.push((task, runtime.cpu));
    }

    *PROTOCOL_CLUSTER.lock() = Some(cluster);
    net::install_socket_runtime(&SOCKET_RUNTIME_ADAPTER)
        .unwrap_or_else(|_| panic!("socket runtime 重复安装"));
    for (task, cpu) in worker_tasks {
        sched::activate_task_with_cpu_hint(&task, cpu)
            .unwrap_or_else(|error| panic!("NetWorker 启动失败: {:?}", error));
    }
    let startup_deadline = sched::now_ns_direct().saturating_add(1_000_000_000);
    while runtimes
        .iter()
        .any(|runtime| !runtime.started.load(Ordering::Acquire))
    {
        assert!(
            sched::now_ns_direct() < startup_deadline,
            "协议 worker 未在 1 秒内进入主循环"
        );
        // 这里只需把 CPU 交给刚激活的协议 worker。把启动线程送入 timed-sleeper
        // 会额外制造“状态已 Sleeping、deadline 尚未稳定入队”的唤醒窗口。
        let _ = sched::operation::sched_yield();
    }
    NET_RUNTIME_STARTED.store(true, Ordering::Release);
    reconcile_devices();
}

/// 把所有尚未启动的 queue late-attach 到已经运行的 host。
pub(crate) fn reconcile_devices() {
    if !NET_RUNTIME_STARTED.load(Ordering::Acquire) {
        return;
    }
    let _attach = NET_ATTACH_LOCK.lock();
    let Some(cluster) = PROTOCOL_CLUSTER.lock().as_ref().cloned() else {
        return;
    };
    if CONFIG_STORE.lock().is_none() {
        return;
    }
    let boot = net::stack::boot_config().expect("网络 stack 启动配置未安装");
    let mut generation_bytes = [0u8; 4];
    generation_bytes.copy_from_slice(&boot.generation_nonce()[..4]);
    let rss_generation = u32::from_le_bytes(generation_bytes).max(1);

    let mut pending_workers = Vec::new();
    let mut devices = DEVICES.lock();
    for device in devices.iter_mut().filter(|device| !device.started) {
        let Some(queues) = device.queues.take() else {
            continue;
        };
        for (queue_index, registration) in queues.into_vec().into_iter().enumerate() {
            device.control.worker_count.fetch_add(1, Ordering::Relaxed);
            let runtime =
                Arc::clone(&cluster.shards[usize::from(registration.id.0) % cluster.shards.len()]);
            let interface = InterfaceId(device.snapshot.id.raw());
            let stats = Arc::clone(&device.queue_stats[queue_index]);
            let egress = Arc::new(EgressChannel::new(
                interface,
                Arc::clone(&registration.socket_tx_pool),
                Arc::clone(&stats),
            ));
            let mut egress_index = None;
            for runtime in &cluster.shards {
                let index = runtime.append_egress(Arc::clone(&egress));
                assert!(
                    egress_index.is_none_or(|expected| expected == index),
                    "protocol shard egress 索引不一致"
                );
                egress_index = Some(index);
            }
            pending_workers.push(PendingWorker {
                runtime,
                registration,
                ingress_device: device.snapshot.id,
                interface,
                local_mac: device.snapshot.mac_address,
                egress,
                egress_index: egress_index.expect("协议 cluster 必须至少有一个 shard"),
                control: Arc::clone(&device.control),
                stats,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                arp_probe_enabled: device.snapshot.name.as_ref() != "lo",
            });
        }
        device.started = true;
    }
    drop(devices);

    for pending in pending_workers {
        let control = Arc::clone(&pending.control);
        let egress = Arc::clone(&pending.egress);
        let NetQueueRegistration {
            queue,
            rx_pool,
            tx_header_pool,
            tx_payload_pool,
            socket_tx_pool: _,
            irq,
            ..
        } = pending.registration;
        let queue = match queue {
            NetQueueEndpoint::Integrated(queue) => queue,
            NetQueueEndpoint::Pinned(_) => {
                unreachable!("Pinned queue 必须在 registrar 中转换为常驻 adapter")
            }
        };
        let context = Box::new(WorkerContext {
            initialized: false,
            queue: Some(queue),
            rx_pool: Some(rx_pool),
            tx_header_pool: Some(tx_header_pool),
            tx_payload_pool: Some(tx_payload_pool),
            irq: Arc::clone(&irq),
            rx_batch: PacketBatch::new(),
            pending_rx_batches: VecDeque::with_capacity(4),
            spare_rx_batches: Vec::with_capacity(4),
            refill_batch: RxRefillBatch::new(),
            completion_batch: CompletionBatch::new(),
            tx_batch: TxBatch::new(),
            retry_egress: VecDeque::new(),
            pending_tx_frames: VecDeque::with_capacity(net::tuning::PACKET_BATCH_CAPACITY),
            ingress_device: pending.ingress_device,
            interface: pending.interface,
            local_mac: pending.local_mac,
            protocol_cluster: Arc::clone(&cluster),
            egress_index: pending.egress_index,
            egress: Arc::clone(&egress),
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            owner_shard: pending.runtime.id,
            local_ingress: VecDeque::with_capacity(128),
            rss_generation,
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            arp_probe_enabled: pending.arp_probe_enabled,
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            arp_probe_done: false,
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            udp_probe_queued: false,
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            physical_udp_probe_queued: false,
            control: pending.control,
            stats: pending.stats,
        });
        let task = pending
            .runtime
            .owner_task()
            .expect("NetWorker owner task 尚未安装");
        {
            let mut tasks = control.tasks.lock();
            if !tasks.iter().any(|owned| Arc::ptr_eq(owned, &task)) {
                tasks.push(Arc::clone(&task));
            }
        }
        egress.set_task(Arc::clone(&task));
        irq.set_waker(Arc::new(TaskWake {
            task: Arc::clone(&task),
        }))
        .unwrap_or_else(|error| panic!("NetWorker waker 安装失败: {:?}", error));
        let mut context = context;
        loop {
            match pending.runtime.queue_attach.try_push(context) {
                Ok(()) => break,
                Err(returned) => {
                    context = returned;
                    pending.runtime.publish_work();
                    let _ = sched::operation::sched_yield();
                }
            }
        }
        pending.runtime.publish_work();
    }
}

unsafe extern "C" fn net_worker_entry(slot: usize) -> ! {
    let mut context = NET_WORKER_STARTS
        .lock()
        .get_mut(slot)
        .and_then(Option::take)
        .expect("NetWorker 启动上下文不存在");
    context.run()
}

impl NetWorkerContext {
    fn run(&mut self) -> ! {
        self.runtime.started.store(true, Ordering::Release);
        loop {
            let task = sched::current_task_fast();
            assert!(
                task.begin_execution_scope(sched::ExecutionScopeKind::NetworkWorker),
                "NetWorker 不能嵌套进入执行作用域"
            );
            #[cfg(feature = "performance-profile")]
            let profile_turn = profiling::scope(profiling::Event::NetProtocolTurn);
            let config = self.config.snapshot();
            #[cfg(feature = "performance-profile")]
            let stage_start = profiling::read_counter();
            let attached = self.drain_queue_attachments(64);
            let mut queue_turns = self.pump_local_queues(&config);
            #[cfg(feature = "performance-profile")]
            let queue_done = profiling::read_counter();
            let lifecycle = self.drain_lifecycle(256, &config);
            let control = self.drain_control(256, &config);
            let dirty = self.drain_socket_tx(256, &config);
            #[cfg(feature = "performance-profile")]
            let dispatch_done = profiling::read_counter();
            let mut processed = 0usize;
            while processed < 128 {
                let mut count = self.drain_local_ingress(128 - processed);
                if count == 0 {
                    count = self.drain_ingress();
                }
                if count == 0 {
                    break;
                }
                processed += count;
                self.process_pending(count, &config);
            }
            #[cfg(feature = "performance-profile")]
            let ingress_done = profiling::read_counter();
            self.runtime.timer_fired.store(false, Ordering::Release);
            let now_ns = sched::now_ns_direct();
            self.queue_autoconfig_turn(&config, now_ns);
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            self.queue_udp_probe_observers();
            let (protocol_deadline, protocol_blocked) = self.execute_protocol_turn(&config, now_ns);
            queue_turns += self.pump_local_queues(&config);
            self.runtime.arm_timer(protocol_deadline);
            #[cfg(feature = "performance-profile")]
            let protocol_done = profiling::read_counter();
            let keep_running = processed == 128
                || attached == 64
                || lifecycle == 256
                || control == 256
                || dirty == 256
                || queue_turns != 0
                || protocol_blocked
                || self.runtime.finish_drain()
                || !self.local_ingress.is_empty()
                || self.local_queue_pending();
            let claimed_actions =
                task.end_execution_scope(sched::ExecutionScopeKind::NetworkWorker);
            #[cfg(feature = "performance-profile")]
            let finish_done = profiling::read_counter();
            assert!(
                claimed_actions & !crate::net_stack::NET_STACK_EXECUTION_ACTION == 0,
                "NetWorker 单轮认领了未知的有界动作"
            );
            #[cfg(feature = "performance-profile")]
            {
                profiling::observe(
                    profiling::Metric::NetWorkerQueueCycles,
                    queue_done.wrapping_sub(stage_start),
                );
                profiling::observe(
                    profiling::Metric::NetWorkerDispatchCycles,
                    dispatch_done.wrapping_sub(queue_done),
                );
                profiling::observe(
                    profiling::Metric::NetWorkerIngressCycles,
                    ingress_done.wrapping_sub(dispatch_done),
                );
                profiling::observe(
                    profiling::Metric::NetWorkerProtocolCycles,
                    protocol_done.wrapping_sub(ingress_done),
                );
                profiling::observe(
                    profiling::Metric::NetWorkerFinishCycles,
                    finish_done.wrapping_sub(protocol_done),
                );
                drop(profile_turn);
            }
            if keep_running {
                let _ = sched::operation::sched_yield();
                continue;
            }
            self.sleep_until_work();
        }
    }

    fn drain_queue_attachments(&mut self, budget: usize) -> usize {
        let mut attached = 0;
        while attached < budget {
            let Some(queue) = self.runtime.queue_attach.try_pop() else {
                break;
            };
            self.local_queues.push(queue);
            attached += 1;
        }
        attached
    }

    fn pump_local_queues(&mut self, config: &ConfigSnapshot) -> usize {
        let mut index = 0;
        let mut turns = 0;
        while index < self.local_queues.len() {
            let remove_ready = self.local_queues[index]
                .control
                .remove_requested
                .load(Ordering::Acquire)
                && self.local_queues[index]
                    .control
                    .remove_ready
                    .load(Ordering::Acquire);
            if self.local_queues[index].initialized
                && !remove_ready
                && !self.local_queues[index].has_pending_work()
            {
                index += 1;
                continue;
            }
            turns += 1;
            let removed = self.local_queues[index].run_turn() == WorkerTurn::Removed;
            self.local_queues[index].initialized = true;
            self.queue_raw_rx_commands(index, config);
            self.drain_queue_local_ingress(index, config, 128);
            if removed {
                let mut queue = self.local_queues.remove(index);
                queue.finish_removal();
            } else {
                index += 1;
            }
        }
        turns
    }

    fn queue_raw_rx_commands(&mut self, queue_index: usize, config: &ConfigSnapshot) {
        loop {
            let batch = self.local_queues[queue_index]
                .pending_rx_batches
                .pop_front();
            let Some(batch) = batch else {
                break;
            };
            let queue = &self.local_queues[queue_index];
            self.turn_commands
                .0
                .push(NetStackFlowCommand::ParsePacketBatch {
                    input: Some(batch),
                    interface: queue.interface,
                    config: config as *const _,
                    output: None,
                });
            self.turn_meta.push(TurnCommandMeta::RawRx {
                egress: queue.egress_index,
                interface: queue.interface,
                local_mac: queue.local_mac,
            });
        }
    }

    fn drain_queue_local_ingress(
        &mut self,
        queue_index: usize,
        config: &ConfigSnapshot,
        budget: usize,
    ) -> usize {
        let mut processed = 0;
        while processed < budget {
            let count = {
                let queue = &mut self.local_queues[queue_index];
                let mut count = 0;
                while count < self.pending.len() && processed + count < budget {
                    let Some(work) = queue.local_ingress.pop_front() else {
                        break;
                    };
                    self.pending[count] = Some(work);
                    count += 1;
                }
                count
            };
            if count == 0 {
                break;
            }
            self.process_pending(count, config);
            processed += count;
        }
        processed
    }

    fn drain_local_ingress(&mut self, budget: usize) -> usize {
        let mut count = 0;
        while count < self.pending.len() && count < budget {
            let Some(work) = self.local_ingress.pop_front() else {
                break;
            };
            self.pending[count] = Some(work);
            count += 1;
        }
        count
    }

    fn local_queue_pending(&mut self) -> bool {
        self.local_queues.iter_mut().any(|queue| {
            !queue.initialized
                || queue.has_pending_work()
                || !queue.local_ingress.is_empty()
                || (queue.control.remove_requested.load(Ordering::Acquire)
                    && queue.control.remove_ready.load(Ordering::Acquire))
        })
    }

    fn drain_control(&mut self, budget: usize, config: &ConfigSnapshot) -> usize {
        let mut processed = 0;
        while processed < budget {
            let Some(work) = self.runtime.control.try_pop() else {
                break;
            };
            processed += 1;
            match work {
                ControlWork::Socket(command) => match command {
                    SocketCommand::Bind {
                        facade,
                        sequence,
                        generation,
                        local,
                        interface,
                        options,
                    } => {
                        if facade.generation() != generation {
                            facade.complete_control(sequence, Err(SocketError::Closed));
                        } else {
                            self.queue_bind_facade(
                                facade, sequence, generation, local, None, interface, options,
                                config,
                            );
                        }
                    }
                    SocketCommand::Connect {
                        facade,
                        sequence,
                        generation,
                        peer,
                        interface,
                        options,
                        nonblocking,
                    } => {
                        if facade.generation() != generation {
                            facade.complete_control(sequence, Err(SocketError::Closed));
                        } else {
                            self.queue_connect_facade(
                                facade,
                                sequence,
                                generation,
                                peer,
                                interface,
                                options,
                                nonblocking,
                                config,
                            );
                        }
                    }
                    SocketCommand::Listen {
                        facade,
                        sequence,
                        generation,
                        backlog,
                    } => {
                        if facade.generation() != generation {
                            facade.complete_control(sequence, Err(SocketError::Closed));
                        } else {
                            self.queue_listen_facade(facade, sequence, generation, backlog, config);
                        }
                    }
                },
                ControlWork::ConnectTcp {
                    facade,
                    sequence,
                    generation,
                    local,
                    peer,
                    path,
                    local_transport,
                } => {
                    if facade.generation() == generation {
                        let interface = path.route.interface;
                        self.install_stream_tx_pool(&facade, interface);
                        self.turn_commands.0.push(NetStackFlowCommand::ConnectTcp {
                            local,
                            remote: peer,
                            path,
                            facade: Arc::clone(&facade),
                            control_sequence: sequence,
                            local_transport,
                            now_ns: sched::now_ns_direct(),
                            output: None,
                        });
                        self.turn_meta.push(TurnCommandMeta::ConnectTcp {
                            facade,
                            sequence,
                            generation,
                            local,
                            peer,
                            interface,
                        });
                    } else {
                        facade.complete_control(sequence, Err(SocketError::Closed));
                    }
                }
                ControlWork::InstallListener { transaction } => {
                    self.turn_commands.0.push(NetStackFlowCommand::ListenTcp {
                        local: transaction.local,
                        interface: transaction.interface,
                        group: Arc::clone(&transaction.group),
                        output: None,
                    });
                    self.turn_meta.push(TurnCommandMeta::ListenTcp {
                        transaction: Arc::clone(&transaction),
                    });
                    if transaction.dual_stack {
                        self.turn_commands.0.push(NetStackFlowCommand::ListenTcp {
                            local: Endpoint {
                                addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                port: transaction.local.port,
                            },
                            interface: transaction.interface,
                            group: Arc::clone(&transaction.group),
                            output: None,
                        });
                        self.turn_meta
                            .push(TurnCommandMeta::ListenTcp { transaction });
                    }
                }
                ControlWork::RemoveListener { transaction } => {
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::CloseTcpListener {
                            group: transaction.group,
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::CloseListener {
                        transaction: Some(transaction),
                    });
                }
                ControlWork::DiscardListener { group } => {
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::CloseTcpListener {
                            group,
                            output: None,
                        });
                    self.turn_meta
                        .push(TurnCommandMeta::CloseListener { transaction: None });
                }
                ControlWork::FinalizeListenerInstall { transaction } => {
                    if transaction.failed.load(Ordering::Acquire)
                        || transaction.facade.generation() != transaction.generation
                    {
                        transaction.fail();
                    } else {
                        self.turn_control_commands.0.push(
                            NetStackControlCommand::InstallListener {
                                group: transaction.group.id(),
                                output: None,
                            },
                        );
                        self.turn_control_meta
                            .push(TurnControlMeta::InstallListener { transaction });
                    }
                }
                ControlWork::FinalizeListenerRemove { transaction } => {
                    self.turn_control_commands
                        .0
                        .push(NetStackControlCommand::RemoveListener {
                            group: transaction.group,
                            output: None,
                        });
                    self.turn_control_meta
                        .push(TurnControlMeta::RemoveListener { transaction });
                }
                ControlWork::InterfaceGone { interface, ack } => {
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::InvalidateInterface {
                            interface,
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::InterfaceInvalidated);
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::FailInterfaceNeighbors {
                            interface,
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::InterfaceNeighbors {
                        ack: Arc::clone(&ack),
                    });
                    if self.runtime.id == ShardId(0) {
                        self.turn_control_commands.0.push(
                            NetStackControlCommand::RemoveAutoconfigInterface {
                                interface,
                                output: None,
                            },
                        );
                        self.turn_control_meta
                            .push(TurnControlMeta::RemoveAutoconfigInterface);
                        self.turn_control_commands
                            .0
                            .push(NetStackControlCommand::RemoveInterfaceMulticast { interface });
                        self.turn_control_meta
                            .push(TurnControlMeta::RemoveInterfaceMulticast);
                    }
                }
                ControlWork::ResolveNeighbor(work) => {
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::EnqueueNeighbor {
                            work: Some(work),
                            now_ns: sched::now_ns_direct(),
                            interface_limit: self.neighbor_interface_limit(),
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::NeighborEnqueue);
                }
                ControlWork::ResolveNeighborOwner(work) => {
                    self.turn_control_commands
                        .0
                        .push(NetStackControlCommand::NeighborOwner {
                            key: work.key(),
                            output: None,
                        });
                    self.turn_control_meta
                        .push(TurnControlMeta::NeighborOwner { work });
                }
                ControlWork::NeighborObserved {
                    key,
                    mac_address,
                    now_ns,
                } => {
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::ObserveAndResolveNeighbor {
                            key,
                            mac_address,
                            now_ns,
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::NeighborObserved);
                }
                ControlWork::NeighborObservedOwner {
                    key,
                    mac_address,
                    now_ns,
                } => {
                    self.turn_control_commands
                        .0
                        .push(NetStackControlCommand::NeighborOwner { key, output: None });
                    self.turn_control_meta
                        .push(TurnControlMeta::NeighborObservedOwner {
                            key,
                            mac_address,
                            now_ns,
                        });
                }
                ControlWork::Multicast {
                    facade,
                    membership,
                    joined,
                } => self.update_multicast_membership(&facade, membership, joined, config),
                ControlWork::TransportError {
                    interface,
                    target,
                    error,
                    now_ns,
                } => {
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::ApplyTransportError {
                            interface,
                            target,
                            error,
                            now_ns,
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::TransportError);
                }
                ControlWork::TransportErrorOwner {
                    interface,
                    target,
                    error,
                    now_ns,
                } => match target {
                    net::transport::ControlErrorTarget::Flow(flow) => {
                        self.turn_control_commands
                            .0
                            .push(NetStackControlCommand::FlowShard {
                                remote: flow.remote,
                                local: flow.local,
                                protocol: flow.protocol,
                                local_transport: false,
                                output: None,
                            });
                        self.turn_control_meta
                            .push(TurnControlMeta::TransportErrorOwner {
                                interface,
                                target,
                                error,
                                now_ns,
                            });
                    }
                    net::transport::ControlErrorTarget::Raw { .. } => {
                        let _ = self.cluster.publish_control(
                            ShardId(0),
                            ControlWork::TransportError {
                                interface,
                                target,
                                error,
                                now_ns,
                            },
                        );
                    }
                },
                ControlWork::ReleaseBinding {
                    facade,
                    publish_closed,
                } => self.queue_release_binding_local(facade, publish_closed),
                ControlWork::RemoveSocketMulticast { socket } => {
                    self.turn_control_commands.0.push(
                        NetStackControlCommand::RemoveSocketMulticast {
                            socket,
                            output: None,
                        },
                    );
                    self.turn_control_meta
                        .push(TurnControlMeta::RemoveSocketMulticast);
                }
            }
        }
        processed
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_bind_facade(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        options: BindOptions,
        config: &ConfigSnapshot,
    ) {
        facade.set_v6_only(options.v6_only);
        let result = match facade.kind() {
            SocketKind::Datagram => self.queue_udp_binding(
                Arc::clone(&facade),
                sequence,
                generation,
                local,
                peer,
                interface,
                options,
                config,
            ),
            SocketKind::Stream => {
                if peer.is_some() {
                    Err(SocketError::InvalidState)
                } else {
                    self.queue_tcp_binding(
                        Arc::clone(&facade),
                        sequence,
                        generation,
                        local,
                        interface,
                        options,
                        TcpReserveNext::Bind,
                        config,
                    )
                }
            }
            SocketKind::Raw => {
                if peer.is_some() {
                    Err(SocketError::InvalidState)
                } else {
                    self.queue_raw_binding(
                        Arc::clone(&facade),
                        sequence,
                        generation,
                        local,
                        peer,
                        interface,
                        options,
                        config,
                    )
                }
            }
        };
        if let Err(error) = result {
            facade.complete_control(sequence, Err(error));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_raw_binding(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        mut local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        options: BindOptions,
        config: &ConfigSnapshot,
    ) -> Result<(), SocketError> {
        if !address_matches_family(facade.family(), local.addr) || local.port != 0 {
            return Err(SocketError::AddressUnavailable);
        }
        if !options.free_bind
            && !local.addr.is_unspecified()
            && !config.addresses.iter().any(|entry| {
                entry.address == local.addr && interface.is_none_or(|id| id == entry.interface)
            })
        {
            return Err(SocketError::AddressUnavailable);
        }
        local.port = 0;
        facade.set_free_bind(options.free_bind);
        self.turn_commands
            .0
            .push(NetStackFlowCommand::BindRawFacade {
                local: local.addr,
                interface,
                facade: Arc::clone(&facade),
                free_bind: options.free_bind,
                output: None,
            });
        self.turn_meta.push(TurnCommandMeta::BindRaw {
            facade,
            sequence,
            generation,
            local,
            peer,
            interface,
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_udp_binding(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        options: BindOptions,
        config: &ConfigSnapshot,
    ) -> Result<(), SocketError> {
        let family = facade.family();
        if !address_allowed(family, local.addr, options.v6_only)
            || peer.is_some_and(|peer| !address_allowed(family, peer.addr, options.v6_only))
        {
            return Err(SocketError::AddressUnavailable);
        }
        if !options.free_bind
            && !local.addr.is_unspecified()
            && !config.addresses.iter().any(|entry| {
                entry.address == local.addr && interface.is_none_or(|id| id == entry.interface)
            })
        {
            return Err(SocketError::AddressUnavailable);
        }
        let request_family = match local.addr {
            IpAddr::V4(_) => AddressFamily::Ipv4,
            IpAddr::V6(_) => family,
        };
        let request = BindRequest {
            owner: facade.id().counter,
            family: request_family,
            protocol: TransportProtocol::Udp,
            address: if local.addr.is_unspecified() {
                BindAddress::Any
            } else {
                BindAddress::Specified(local.addr)
            },
            port: local.port,
            interface,
            options,
        };
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::ReserveBinding {
                socket: facade.id(),
                request,
                shard: self.runtime.id,
                output: None,
            });
        self.turn_control_meta.push(TurnControlMeta::ReserveUdp {
            facade,
            sequence,
            generation,
            local,
            peer,
            interface,
            options,
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_tcp_binding(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        next: TcpReserveNext,
        config: &ConfigSnapshot,
    ) -> Result<(), SocketError> {
        let family = facade.family();
        if !address_allowed(family, local.addr, options.v6_only) {
            return Err(SocketError::AddressUnavailable);
        }
        if !options.free_bind
            && !local.addr.is_unspecified()
            && !config.addresses.iter().any(|entry| {
                entry.address == local.addr && interface.is_none_or(|id| id == entry.interface)
            })
        {
            return Err(SocketError::AddressUnavailable);
        }
        let request = BindRequest {
            owner: facade.id().counter,
            family: match local.addr {
                IpAddr::V4(_) => AddressFamily::Ipv4,
                IpAddr::V6(_) => family,
            },
            protocol: TransportProtocol::Tcp,
            address: if local.addr.is_unspecified() {
                BindAddress::Any
            } else {
                BindAddress::Specified(local.addr)
            },
            port: local.port,
            interface,
            options,
        };
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::ReserveBinding {
                socket: facade.id(),
                request,
                shard: self.runtime.id,
                output: None,
            });
        self.turn_control_meta.push(TurnControlMeta::ReserveTcp {
            facade,
            sequence,
            generation,
            local,
            interface,
            options,
            next,
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_connect_facade(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        _nonblocking: bool,
        config: &ConfigSnapshot,
    ) {
        if !address_allowed(facade.family(), peer.addr, options.v6_only) {
            facade.complete_control(sequence, Err(SocketError::AddressUnavailable));
            return;
        }
        if facade.kind() == SocketKind::Stream {
            let bound = facade.local_endpoint();
            let bound_source =
                bound.and_then(|local| (!local.addr.is_unspecified()).then_some(local.addr));
            self.turn_commands
                .0
                .push(NetStackFlowCommand::ResolveTcpPath {
                    destination: peer.addr,
                    bound_source,
                    interface: interface.or_else(|| facade.interface()),
                    config: config as *const _,
                    now_ns: sched::now_ns_direct(),
                    free_bind: options.free_bind,
                    output: None,
                });
            self.turn_meta.push(TurnCommandMeta::ResolveTcpPath {
                facade,
                sequence,
                generation,
                peer,
                options,
            });
            return;
        }
        if facade.kind() == SocketKind::Raw {
            match facade.owner() {
                OwnerRef::Unassigned => {
                    let family = facade.family();
                    if let Err(error) = self.queue_raw_binding(
                        Arc::clone(&facade),
                        sequence,
                        generation,
                        Endpoint {
                            addr: unspecified_address(family),
                            port: 0,
                        },
                        Some(peer),
                        interface,
                        options,
                        config,
                    ) {
                        facade.complete_control(sequence, Err(error));
                    }
                }
                OwnerRef::Flow {
                    shard,
                    flow,
                    generation: owner_generation,
                } => {
                    let Some(local) = facade.local_endpoint() else {
                        facade.complete_control(sequence, Err(SocketError::InvalidState));
                        return;
                    };
                    facade.publish_binding(
                        OwnerRef::Flow {
                            shard,
                            flow,
                            generation: owner_generation,
                        },
                        local,
                        Some(peer),
                        interface.or_else(|| facade.interface()),
                    );
                    facade.complete_control(sequence, Ok(()));
                }
                OwnerRef::Closed { .. } => {
                    facade.complete_control(sequence, Err(SocketError::Closed));
                }
                _ => facade.complete_control(sequence, Err(SocketError::InvalidState)),
            }
            return;
        }
        let result = match facade.owner() {
            OwnerRef::Unassigned => {
                match config
                    .route_with_source_policy(
                        peer.addr,
                        facade.socket_mark(),
                        None,
                        interface,
                        options.free_bind,
                    )
                    .map_err(|_| SocketError::NetworkUnreachable)
                {
                    Ok(route) => self.queue_udp_binding(
                        Arc::clone(&facade),
                        sequence,
                        generation,
                        Endpoint {
                            addr: route.source,
                            port: 0,
                        },
                        Some(peer),
                        interface,
                        options,
                        config,
                    ),
                    Err(error) => Err(error),
                }
            }
            OwnerRef::Flow { flow, .. } => {
                let Some(mut local) = facade.local_endpoint() else {
                    facade.complete_control(sequence, Err(SocketError::InvalidState));
                    return;
                };
                if !address_matches_family(address_family(peer.addr), local.addr) {
                    let route = match config
                        .route_with_source_policy(
                            peer.addr,
                            facade.socket_mark(),
                            None,
                            interface.or_else(|| facade.interface()),
                            options.free_bind,
                        )
                        .map_err(|_| SocketError::NetworkUnreachable)
                    {
                        Ok(route) => route,
                        Err(error) => {
                            facade.complete_control(sequence, Err(error));
                            return;
                        }
                    };
                    local.addr = route.source;
                }
                self.turn_commands
                    .0
                    .push(NetStackFlowCommand::ReconnectUdpFacade {
                        flow,
                        local,
                        peer,
                        facade: Arc::clone(&facade),
                        output: None,
                    });
                self.turn_meta.push(TurnCommandMeta::ReconnectUdp {
                    facade: Arc::clone(&facade),
                    sequence,
                    generation,
                    local,
                    peer,
                    interface,
                });
                Ok(())
            }
            OwnerRef::Bound { .. } | OwnerRef::Listener { .. } => Err(SocketError::InvalidState),
            OwnerRef::Closed { .. } => Err(SocketError::Closed),
        };
        if let Err(error) = result {
            facade.complete_control(sequence, Err(error));
        }
    }

    fn queue_tcp_connect_owner(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        peer: Endpoint,
        path: TcpPath,
        local_transport: bool,
    ) {
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::FlowShard {
                remote: peer,
                local,
                protocol: TransportProtocol::Tcp,
                local_transport,
                output: None,
            });
        self.turn_control_meta
            .push(TurnControlMeta::FlowShardTcpConnect {
                facade,
                sequence,
                generation,
                local,
                peer,
                path,
                local_transport,
            });
    }

    fn queue_allocate_listener(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        backlog: u32,
    ) {
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::AllocateListener { output: None });
        self.turn_control_meta
            .push(TurnControlMeta::AllocateListener {
                facade,
                sequence,
                generation,
                backlog,
            });
    }

    fn queue_listen_facade(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        backlog: u32,
        config: &ConfigSnapshot,
    ) {
        if facade.kind() != SocketKind::Stream {
            facade.complete_control(sequence, Err(SocketError::InvalidState));
            return;
        }
        match facade.owner() {
            OwnerRef::Unassigned => {
                if let Err(error) = self.queue_tcp_binding(
                    Arc::clone(&facade),
                    sequence,
                    generation,
                    Endpoint {
                        addr: unspecified_address(facade.family()),
                        port: 0,
                    },
                    None,
                    BindOptions::default(),
                    TcpReserveNext::Listen { backlog },
                    config,
                ) {
                    facade.complete_control(sequence, Err(error));
                }
            }
            OwnerRef::Listener { .. } => {
                let Some(group) = facade.listen_group() else {
                    facade.complete_control(sequence, Err(SocketError::InvalidState));
                    return;
                };
                group.update_backlog(backlog);
                facade.complete_control(sequence, Ok(()));
            }
            OwnerRef::Bound { .. } => {
                self.queue_allocate_listener(facade, sequence, generation, backlog);
            }
            OwnerRef::Flow { .. } => {
                facade.complete_control(sequence, Err(SocketError::InvalidState));
            }
            OwnerRef::Closed { .. } => {
                facade.complete_control(sequence, Err(SocketError::Closed));
            }
        }
    }

    fn publish_listener_install(
        &mut self,
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        backlog: u32,
        group_id: ListenGroupId,
    ) {
        let Some(local) = facade.local_endpoint() else {
            facade.complete_control(sequence, Err(SocketError::InvalidState));
            return;
        };
        let cpu_hints = self
            .cluster
            .shards
            .iter()
            .map(|runtime| runtime.cpu)
            .collect::<Vec<_>>();
        let group = ListenGroup::new_with_cpu_hints(group_id, &facade, &cpu_hints, backlog);
        let dual_stack = facade.family() == AddressFamily::Ipv6
            && !facade.v6_only()
            && matches!(local.addr, IpAddr::V6(address) if address.is_unspecified());
        let interface = facade.interface();
        let commands_per_shard = if dual_stack { 2 } else { 1 };
        let transaction = Arc::new(ListenerInstall {
            facade,
            group,
            local,
            interface,
            dual_stack,
            sequence,
            generation,
            remaining: AtomicUsize::new(self.cluster.shards.len() * commands_per_shard),
            failed: AtomicBool::new(false),
            cluster: Arc::clone(&self.cluster),
        });
        for runtime in &self.cluster.shards {
            let mut work = ControlWork::InstallListener {
                transaction: Arc::clone(&transaction),
            };
            loop {
                match runtime.control.try_push(work) {
                    Ok(()) => {
                        runtime.publish_work();
                        break;
                    }
                    Err(pending) => {
                        work = pending;
                        runtime.publish_work();
                        let _ = sched::operation::sched_yield();
                    }
                }
            }
        }
    }

    fn drain_socket_tx(&mut self, budget: usize, config: &ConfigSnapshot) -> usize {
        let mut processed = 0;
        while processed < budget {
            let Some(facade) = self.runtime.dirty.try_pop() else {
                break;
            };
            processed += 1;
            let flow = match facade.owner() {
                OwnerRef::Flow {
                    shard,
                    flow,
                    generation,
                } if shard == self.runtime.id && generation == facade.generation() => flow,
                _ => {
                    if !facade.is_closing() {
                        facade.set_pending_error(SocketError::Closed);
                    }
                    facade.finish_tx_drain();
                    continue;
                }
            };
            match facade.kind() {
                SocketKind::Stream => {
                    let generation = facade.stream_tx_generation();
                    let local_interface = facade.interface().filter(|interface| {
                        config
                            .interfaces
                            .iter()
                            .any(|candidate| candidate.id == *interface && candidate.loopback)
                    });
                    if local_interface.is_some() {
                        let scratch = self
                            .cooperative_scratch
                            .pop()
                            .unwrap_or_else(CooperativeTxScratch::new);
                        self.turn_commands
                            .0
                            .push(NetStackFlowCommand::CooperativeSocketTx {
                                flow,
                                facade: Arc::clone(&facade),
                                mark: facade.socket_mark(),
                                config: config as *const _,
                                now_ns: sched::now_ns_direct(),
                                limit: net::stack::NET_STACK_LOCAL_TURN_EFFECT_CAPACITY as u16,
                                inline_local: true,
                                tcp_output: scratch.tcp,
                                udp_output: scratch.udp,
                                result: None,
                            });
                        self.turn_meta
                            .push(TurnCommandMeta::StreamDirtyLocal { facade, generation });
                    } else {
                        self.turn_commands
                            .0
                            .push(NetStackFlowCommand::DrainTcpSend {
                                flow,
                                now_ns: sched::now_ns_direct(),
                            });
                        self.turn_meta
                            .push(TurnCommandMeta::StreamDirty { facade, generation });
                    }
                }
                SocketKind::Datagram => {
                    self.queue_udp_socket_tx(&facade, flow, config, 32);
                }
                SocketKind::Raw => {
                    self.queue_raw_socket_tx(&facade, flow, config, 32);
                }
            }
        }
        #[cfg(feature = "performance-profile")]
        if processed != 0 {
            profiling::observe(profiling::Metric::DirtyDrainSockets, processed as u64);
        }
        processed
    }

    fn queue_udp_socket_tx(
        &mut self,
        facade: &Arc<SocketFacade>,
        flow: FlowId,
        config: &ConfigSnapshot,
        limit: usize,
    ) {
        let start = self.turn_meta.len();
        for _ in 0..limit {
            let Some(payload) = facade.take_tx() else {
                break;
            };
            self.turn_commands
                .0
                .push(NetStackFlowCommand::PrepareUdpTx {
                    flow,
                    payload: Some(payload),
                    mark: facade.socket_mark(),
                    config: config as *const _,
                    now_ns: sched::now_ns_direct(),
                    output: None,
                });
            self.turn_meta.push(TurnCommandMeta::UdpTx {
                facade: Arc::clone(facade),
                finish_drain: false,
            });
        }
        if self.turn_meta.len() > start {
            if let Some(TurnCommandMeta::UdpTx { finish_drain, .. }) = self.turn_meta.last_mut() {
                *finish_drain = true;
            }
        } else {
            facade.finish_tx_drain();
        }
    }

    fn queue_raw_socket_tx(
        &mut self,
        facade: &Arc<SocketFacade>,
        flow: FlowId,
        config: &ConfigSnapshot,
        limit: usize,
    ) {
        let start = self.turn_meta.len();
        for _ in 0..limit {
            let Some(payload) = facade.take_tx() else {
                break;
            };
            self.turn_commands
                .0
                .push(NetStackFlowCommand::PrepareRawTx {
                    flow,
                    payload: Some(payload),
                    mark: facade.socket_mark(),
                    config: config as *const _,
                    now_ns: sched::now_ns_direct(),
                    output: None,
                });
            self.turn_meta.push(TurnCommandMeta::RawTx {
                facade: Arc::clone(facade),
                finish_drain: false,
            });
        }
        if self.turn_meta.len() > start {
            if let Some(TurnCommandMeta::RawTx { finish_drain, .. }) = self.turn_meta.last_mut() {
                *finish_drain = true;
            }
        } else {
            facade.finish_tx_drain();
        }
    }

    fn dispatch_local_udp(&mut self, _egress: usize, work: PreparedUdpTx) {
        let source = Endpoint {
            addr: work.route.source,
            port: work.source_port,
        };
        let key = FlowKey::new(source, work.destination, TransportProtocol::Udp)
            .expect("UDP local transport tuple 必须有效");
        let target = self.cluster.local_ingress_target(&key);
        let ingress = IngressWork::LocalUdp {
            interface: work.route.interface,
            work,
        };
        if target.id == self.runtime.id {
            self.local_ingress.push_back(ingress);
            return;
        }
        push_local_ingress(&target, ingress);
    }

    fn publish_neighbor_work(&mut self, work: PendingNeighborTx) {
        let _ = self
            .cluster
            .publish_control(ShardId(0), ControlWork::ResolveNeighborOwner(work));
    }

    fn queue_tx_plan(&mut self, work: PendingNeighborTx) {
        self.turn_commands
            .0
            .push(NetStackFlowCommand::PlanTxWork { work: Some(work) });
        self.turn_meta.push(TurnCommandMeta::PlanTx);
    }

    fn publish_neighbor_to_owner(&self, target_id: ShardId, work: PendingNeighborTx) {
        let Some(target) = self.cluster.shard(target_id) else {
            work.facade()
                .set_pending_error(SocketError::HostUnreachable);
            return;
        };
        let mut control = ControlWork::ResolveNeighbor(work);
        loop {
            match target.control.try_push(control) {
                Ok(()) => {
                    target.publish_work();
                    return;
                }
                Err(pending) => {
                    control = pending;
                    target.publish_work();
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn neighbor_interface_limit(&self) -> u16 {
        let shard_count = self.cluster.shards.len();
        let base = net::flow::MAX_PENDING_NEIGHBOR_PACKETS_PER_INTERFACE / shard_count;
        let remainder = net::flow::MAX_PENDING_NEIGHBOR_PACKETS_PER_INTERFACE % shard_count;
        let limit = base
            + if usize::from(self.runtime.id.0) < remainder {
                1
            } else {
                0
            };
        u16::try_from(limit).expect("neighbor shard 配额必须可表示为 u16")
    }

    fn dispatch_neighbor_tx(&mut self, work: PendingNeighborTx) {
        let interface = match &work {
            PendingNeighborTx::Tcp(work) => work.path.route.interface,
            PendingNeighborTx::Udp(work) => work.route.interface,
            PendingNeighborTx::Raw(work) => work.route.interface,
        };
        if self.runtime.egress_index(interface).is_none() {
            work.facade()
                .set_pending_error(SocketError::NetworkUnreachable);
            return;
        }
        self.queue_tx_plan(work);
    }

    fn finish_neighbor_timers(
        &mut self,
        output: Option<net::stack::NeighborTimerOutput>,
        config: &ConfigSnapshot,
    ) -> Option<u64> {
        let Some(output) = output else {
            return None;
        };
        for key in output.probes {
            self.emit_neighbor_probe(key, config);
        }
        for work in output.expired {
            work.facade()
                .set_pending_error(SocketError::HostUnreachable);
        }
        output.next_deadline_ns
    }

    fn emit_neighbor_probe(&self, key: net::control::NeighborKey, config: &ConfigSnapshot) {
        let Some(frame) = build_neighbor_probe(key, config, false) else {
            return;
        };
        let Some(index) = self.runtime.egress_index(key.interface) else {
            return;
        };
        let Some(target) = self.runtime.egress(index) else {
            return;
        };
        if target.try_push(EgressWork::ControlFrame(frame)).is_err() {
            target.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn emit_dad_probe(&self, key: net::control::NeighborKey, config: &ConfigSnapshot) {
        let Some(frame) = build_neighbor_probe(key, config, true) else {
            return;
        };
        let Some(index) = self.runtime.egress_index(key.interface) else {
            return;
        };
        let Some(target) = self.runtime.egress(index) else {
            return;
        };
        if target.try_push(EgressWork::ControlFrame(frame)).is_err() {
            target.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn queue_autoconfig_turn(&mut self, config: &ConfigSnapshot, now_ns: u64) {
        if self.runtime.id != ShardId(0)
            || !autoconfig_egress_ready(config, |interface| {
                self.runtime.egress_index(interface).is_some()
            })
        {
            return;
        }
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::RunDad {
                now_ns,
                output: None,
            });
        self.turn_control_meta.push(TurnControlMeta::Dad);
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::RunDhcp {
                config: config as *const _,
                now_ns,
                output: None,
            });
        self.turn_control_meta.push(TurnControlMeta::Dhcp);
    }

    fn finish_dad_turn(&mut self, output: net::stack::DadRunOutput) -> Option<u64> {
        let snapshot = self.config.snapshot();
        for key in output.probes {
            self.emit_dad_probe(key, &snapshot);
        }
        for (interface, address) in output.ready {
            self.publish_dad_address(interface, address);
        }
        output.next_deadline_ns
    }

    fn publish_dad_address(&mut self, interface: InterfaceId, address: Ipv6Addr) {
        let current = self.config.snapshot();
        if current
            .addresses
            .iter()
            .any(|entry| entry.interface == interface && entry.address == IpAddr::V6(address))
        {
            return;
        }
        let mut addresses = current.addresses.clone();
        addresses.push(AddressEntry {
            interface,
            address: IpAddr::V6(address),
            prefix_len: 64,
            primary: true,
        });
        let mut routes = current.routes.entries().to_vec();
        routes.push(RouteEntry {
            table: 0,
            network: IpAddr::V6(Ipv6Addr([
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ])),
            prefix_len: 64,
            gateway: None,
            interface,
            metric: 0,
            mtu: current
                .interfaces
                .iter()
                .find(|entry| entry.id == interface)
                .map(|entry| entry.mtu),
        });
        if let Ok(next) = ConfigSnapshot::new_with_dns(
            current.generation.saturating_add(1),
            current.interfaces.clone(),
            addresses,
            routes,
            current.policy.clone(),
            current.dns_servers.clone(),
        ) {
            if self.config.publish(next).is_ok() {
                self.emit_interface_multicast_reports(interface);
            }
        }
    }

    fn finish_dhcp_turn(&mut self, output: net::stack::DhcpRunOutput) -> Option<u64> {
        for change in output.lease_changes {
            self.replace_dhcp_lease(&change);
        }
        for (interface, frame) in output.frames {
            self.emit_control_frame(interface, frame);
        }
        output.next_deadline_ns
    }

    fn replace_dhcp_lease(&mut self, change: &net::stack::DhcpLeaseChange) {
        let interface = change.interface;
        let old = change.old.as_ref();
        let new = change.new.as_ref();
        let current = self.config.snapshot();
        let mut addresses = current.addresses.clone();
        let mut routes = current.routes.entries().to_vec();
        let interface_mtu = current
            .interfaces
            .iter()
            .find(|entry| entry.id == interface)
            .map(|entry| entry.mtu);
        if let Some(old) = old {
            let old_address = AddressEntry {
                interface,
                address: IpAddr::V4(old.address),
                prefix_len: old.prefix_len,
                primary: true,
            };
            if let Some(index) = addresses.iter().rposition(|entry| *entry == old_address) {
                addresses.remove(index);
            }
            let old_network = ipv4_network(old.address, old.prefix_len);
            let connected = RouteEntry {
                table: 0,
                network: IpAddr::V4(old_network),
                prefix_len: old.prefix_len,
                gateway: None,
                interface,
                metric: 0,
                mtu: interface_mtu,
            };
            if let Some(index) = routes.iter().rposition(|route| *route == connected) {
                routes.remove(index);
            }
            if let Some(router) = old.router {
                let default = RouteEntry {
                    table: 0,
                    network: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    prefix_len: 0,
                    gateway: Some(IpAddr::V4(router)),
                    interface,
                    metric: 100,
                    mtu: interface_mtu,
                };
                if let Some(index) = routes.iter().rposition(|route| *route == default) {
                    routes.remove(index);
                }
            }
        }
        let mut dns_servers = current.dns_servers.clone();
        if let Some(old) = old {
            for server in &old.dns {
                let address = IpAddr::V4(*server);
                if !change.retained_dns.contains(server)
                    && let Some(index) = dns_servers.iter().rposition(|entry| *entry == address)
                {
                    dns_servers.remove(index);
                }
            }
        }
        if let Some(new) = new {
            addresses.push(AddressEntry {
                interface,
                address: IpAddr::V4(new.address),
                prefix_len: new.prefix_len,
                primary: true,
            });
            routes.push(RouteEntry {
                table: 0,
                network: IpAddr::V4(ipv4_network(new.address, new.prefix_len)),
                prefix_len: new.prefix_len,
                gateway: None,
                interface,
                metric: 0,
                mtu: interface_mtu,
            });
            if let Some(router) = new.router {
                routes.push(RouteEntry {
                    table: 0,
                    network: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    prefix_len: 0,
                    gateway: Some(IpAddr::V4(router)),
                    interface,
                    metric: 100,
                    mtu: interface_mtu,
                });
            }
            for server in new.dns.iter().copied().map(IpAddr::V4) {
                if !dns_servers.contains(&server) {
                    dns_servers.push(server);
                }
            }
        }
        if let Ok(next) = ConfigSnapshot::new_with_dns(
            current.generation.saturating_add(1),
            current.interfaces.clone(),
            addresses,
            routes,
            current.policy.clone(),
            dns_servers,
        ) {
            if self.config.publish(next).is_ok() {
                self.emit_interface_multicast_reports(interface);
            }
        }
    }

    fn emit_control_frame(&self, interface: InterfaceId, frame: Vec<u8>) {
        let Some(index) = self.runtime.egress_index(interface) else {
            return;
        };
        let Some(target) = self.runtime.egress(index) else {
            return;
        };
        if target.try_push(EgressWork::ControlFrame(frame)).is_err() {
            target.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn update_multicast_membership(
        &mut self,
        facade: &Arc<SocketFacade>,
        membership: net::MulticastMembership,
        joined: bool,
        config: &ConfigSnapshot,
    ) {
        let binding_key = (facade.id(), membership);
        if joined {
            if facade.is_closing() {
                return;
            }
            let Some(interface) = self.resolve_multicast_interface(facade, membership, config)
            else {
                facade.set_pending_error(SocketError::NetworkUnreachable);
                return;
            };
            self.turn_control_commands
                .0
                .push(NetStackControlCommand::JoinMulticast {
                    socket: binding_key.0,
                    membership: binding_key.1,
                    interface,
                    output: None,
                });
            self.turn_control_meta.push(TurnControlMeta::JoinMulticast {
                interface,
                group: membership.group,
            });
            return;
        }
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::LeaveMulticast {
                socket: binding_key.0,
                membership: binding_key.1,
                output: None,
            });
        self.turn_control_meta
            .push(TurnControlMeta::LeaveMulticast {
                group: membership.group,
            });
    }

    fn resolve_multicast_interface(
        &self,
        facade: &SocketFacade,
        membership: net::MulticastMembership,
        config: &ConfigSnapshot,
    ) -> Option<InterfaceId> {
        let requested = membership
            .interface
            .or_else(|| facade.multicast_interface());
        config
            .interfaces
            .iter()
            .filter(|interface| interface.running && !interface.loopback)
            .filter(|interface| requested.is_none_or(|id| id == interface.id))
            .find(|interface| {
                config.addresses.iter().any(|entry| {
                    entry.interface == interface.id
                        && address_family(entry.address) == address_family(membership.group)
                })
            })
            .map(|interface| interface.id)
            .or(requested.filter(|id| {
                config.interfaces.iter().any(|interface| {
                    interface.id == *id && interface.running && !interface.loopback
                })
            }))
    }

    fn emit_multicast_control(
        &self,
        interface: InterfaceId,
        group: IpAddr,
        joined: bool,
        config: &ConfigSnapshot,
    ) {
        if let Some(frame) = build_multicast_control_frame(interface, group, joined, config) {
            self.emit_control_frame(interface, frame);
        }
    }

    fn emit_interface_multicast_reports(&mut self, interface: InterfaceId) {
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::MulticastGroups {
                interface,
                output: None,
            });
        self.turn_control_meta
            .push(TurnControlMeta::MulticastGroups { interface });
    }

    fn remove_socket_multicast(&mut self, socket: net::SocketId) {
        let _ = self
            .cluster
            .publish_control(ShardId(0), ControlWork::RemoveSocketMulticast { socket });
    }

    fn drain_lifecycle(&mut self, budget: usize, config: &ConfigSnapshot) -> usize {
        let mut processed = 0;
        while processed < budget {
            let Some(facade) = self.runtime.lifecycle.try_pop() else {
                break;
            };
            processed += 1;
            facade.begin_lifecycle_drain();
            if facade.is_closing() {
                self.remove_socket_multicast(facade.id());
            }
            match facade.owner() {
                OwnerRef::Flow { shard, flow, .. }
                    if shard == self.runtime.id && facade.kind() == SocketKind::Stream =>
                {
                    if facade.is_closing() {
                        if facade.is_abortive_close() {
                            self.turn_commands.0.push(NetStackFlowCommand::AbortTcp {
                                flow,
                                now_ns: sched::now_ns_direct(),
                            });
                        } else {
                            self.turn_commands.0.push(NetStackFlowCommand::CloseTcp {
                                flow,
                                now_ns: sched::now_ns_direct(),
                            });
                        }
                        self.turn_meta.push(TurnCommandMeta::TcpLifecycle {
                            facade: Arc::clone(&facade),
                            release_binding: true,
                        });
                    } else if facade.write_is_shutdown() {
                        self.turn_commands
                            .0
                            .push(NetStackFlowCommand::ShutdownTcpWrite {
                                flow,
                                now_ns: sched::now_ns_direct(),
                            });
                        self.turn_meta.push(TurnCommandMeta::TcpLifecycle {
                            facade: Arc::clone(&facade),
                            release_binding: false,
                        });
                    }
                }
                OwnerRef::Flow { shard, flow, .. }
                    if shard == self.runtime.id && facade.is_closing() =>
                {
                    match facade.kind() {
                        SocketKind::Datagram => {
                            self.queue_udp_socket_tx(&facade, flow, config, 32);
                            if facade.has_pending_datagram_tx() {
                                facade.retry_lifecycle();
                                continue;
                            }
                            facade.finish_tx_drain();
                            self.turn_commands
                                .0
                                .push(NetStackFlowCommand::CloseUdp { flow });
                            self.turn_meta.push(TurnCommandMeta::DatagramClose {
                                facade: Arc::clone(&facade),
                            });
                        }
                        SocketKind::Raw => {
                            self.turn_commands
                                .0
                                .push(NetStackFlowCommand::CloseRaw { flow });
                            self.turn_meta.push(TurnCommandMeta::RawClose {
                                facade: Arc::clone(&facade),
                            });
                        }
                        SocketKind::Stream => unreachable!(),
                    }
                }
                OwnerRef::Listener { group, .. } if facade.is_closing() => {
                    self.start_listener_remove(facade, group);
                }
                OwnerRef::Bound { .. } | OwnerRef::Unassigned if facade.is_closing() => {
                    self.queue_release_binding(Arc::clone(&facade), true);
                }
                OwnerRef::Closed { .. }
                | OwnerRef::Flow { .. }
                | OwnerRef::Listener { .. }
                | OwnerRef::Bound { .. }
                | OwnerRef::Unassigned => {}
            }
        }
        processed
    }

    fn queue_release_binding(&mut self, facade: Arc<SocketFacade>, publish_closed: bool) {
        if self.runtime.id != ShardId(0) {
            let _ = self.cluster.publish_control(
                ShardId(0),
                ControlWork::ReleaseBinding {
                    facade,
                    publish_closed,
                },
            );
            return;
        }
        self.queue_release_binding_local(facade, publish_closed);
    }

    fn queue_release_binding_local(&mut self, facade: Arc<SocketFacade>, publish_closed: bool) {
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::ReleaseBinding {
                socket: facade.id(),
                output: None,
            });
        self.turn_control_meta
            .push(TurnControlMeta::ReleaseBinding {
                facade: Some(facade),
                publish_closed,
            });
    }

    fn start_listener_remove(&mut self, facade: Arc<SocketFacade>, group: ListenGroupId) {
        self.queue_release_binding_local(Arc::clone(&facade), false);
        let transaction = Arc::new(ListenerRemove {
            facade,
            group,
            remaining: AtomicUsize::new(self.cluster.shards.len()),
            cluster: Arc::clone(&self.cluster),
        });
        for runtime in &self.cluster.shards {
            let mut work = ControlWork::RemoveListener {
                transaction: Arc::clone(&transaction),
            };
            loop {
                match runtime.control.try_push(work) {
                    Ok(()) => {
                        runtime.publish_work();
                        break;
                    }
                    Err(pending) => {
                        work = pending;
                        runtime.publish_work();
                        let _ = sched::operation::sched_yield();
                    }
                }
            }
        }
    }

    fn drain_ingress(&mut self) -> usize {
        let mut count = 0;
        while count < self.pending.len() {
            let Some(work) = self.runtime.ingress.try_pop() else {
                break;
            };
            self.pending[count] = Some(work);
            count += 1;
        }
        #[cfg(feature = "performance-profile")]
        if count != 0 {
            profiling::observe(
                profiling::Metric::IngressRingDepth,
                self.runtime.ingress.len() as u64,
            );
        }
        count
    }

    fn execute_protocol_turn(
        &mut self,
        config: &ConfigSnapshot,
        now_ns: u64,
    ) -> (Option<u64>, bool) {
        let mut control_meta = core::mem::take(&mut self.turn_control_meta);
        let mut meta = core::mem::take(&mut self.turn_meta);
        debug_assert_eq!(self.turn_control_commands.0.len(), control_meta.len());
        debug_assert_eq!(self.turn_commands.0.len(), meta.len());
        let input_capacity = net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY - 5;
        let control_limit = self.turn_control_commands.0.len().min(input_capacity);
        let mut deferred_control_meta = control_meta.split_off(control_limit);
        let flow_limit = self
            .turn_commands
            .0
            .len()
            .min(input_capacity.saturating_sub(control_limit));
        let mut deferred_meta = meta.split_off(flow_limit);
        let deferred = self.turn_control_commands.0.len() > control_limit
            || self.turn_commands.0.len() > flow_limit;
        let mut control_commands = self
            .turn_control_commands
            .1
            .take()
            .expect("NetWorker control batch scratch 必须存在");
        let mut commands = self
            .turn_commands
            .1
            .take()
            .expect("NetWorker command batch scratch 必须存在");
        self.turn_control_commands
            .0
            .move_prefix_into(&mut control_commands, control_limit)
            .unwrap_or_else(|_| unreachable!("control batch scratch 必须为空"));
        self.turn_commands
            .0
            .move_prefix_into(&mut commands, flow_limit)
            .unwrap_or_else(|_| unreachable!("command batch scratch 必须为空"));
        let tx_plans = self
            .turn_tx_plans
            .0
            .take()
            .expect("NetWorker TxPlan scratch 必须存在");
        let output = self.protocol.run_worker_turn(
            control_commands,
            commands,
            tx_plans,
            config,
            now_ns,
            &mut self.tcp_output,
            &mut self.inline_stream_pool_installs,
            true,
        );
        let mut pool_installs = core::mem::take(&mut self.inline_stream_pool_installs);
        for (facade, interface) in pool_installs.drain(..) {
            self.install_stream_tx_pool(&facade, interface);
        }
        self.inline_stream_pool_installs = pool_installs;
        let mut tx_plans = output.tx_plans;
        let tx_plan_count = tx_plans.len();
        for index in 0..tx_plan_count {
            let Some(plan) = tx_plans.take(index) else {
                continue;
            };
            if let Some(target) = self.runtime.egress_index(plan.interface) {
                self.dispatch_egress(target, EgressWork::Plan(plan));
            } else {
                plan.facade
                    .set_pending_error(SocketError::NetworkUnreachable);
            }
        }
        tx_plans.clear();
        self.turn_tx_plans.0 = Some(tx_plans);
        let turn_stats = output.stats;
        let neighbor_timers = output.neighbor_timers;
        let next_timer_deadline = output.next_timer_deadline;
        let blocked = output.blocked;
        if output.retryable {
            let control_scratch = self
                .turn_control_commands
                .0
                .prepend_from(output.control_commands);
            let command_scratch = self.turn_commands.0.prepend_from(output.commands);
            self.turn_control_commands.1 = Some(control_scratch);
            self.turn_commands.1 = Some(command_scratch);
            let mut queued_control_meta = core::mem::take(&mut self.turn_control_meta);
            control_meta.append(&mut deferred_control_meta);
            control_meta.append(&mut queued_control_meta);
            self.turn_control_meta = control_meta;
            let mut queued_meta = core::mem::take(&mut self.turn_meta);
            meta.append(&mut deferred_meta);
            meta.append(&mut queued_meta);
            self.turn_meta = meta;
            return (None, true);
        }
        let committed = output.committed
            && output.control_commands.len() == control_meta.len()
            && output.commands.len() == meta.len();
        let mut control_commands = output.control_commands;
        let mut control_meta = control_meta;
        let mut control_deadline = None;
        if committed {
            for (index, command_meta) in control_meta.drain(..).enumerate() {
                let command = control_commands
                    .take(index)
                    .expect("提交的 control command 必须保持连续");
                match (command, command_meta) {
                    (
                        NetStackControlCommand::RunDad {
                            output: Some(output),
                            ..
                        },
                        TurnControlMeta::Dad,
                    ) => {
                        control_deadline = [control_deadline, self.finish_dad_turn(output)]
                            .into_iter()
                            .flatten()
                            .min();
                    }
                    (
                        NetStackControlCommand::RunDhcp {
                            output: Some(output),
                            ..
                        },
                        TurnControlMeta::Dhcp,
                    ) => {
                        control_deadline = [control_deadline, self.finish_dhcp_turn(output)]
                            .into_iter()
                            .flatten()
                            .min();
                    }
                    (
                        NetStackControlCommand::ObserveDadConflict { .. },
                        TurnControlMeta::DadConflict,
                    ) => {}
                    (
                        NetStackControlCommand::HandleDhcpPacket {
                            packet: Some(packet),
                            output: Some(output),
                            ..
                        },
                        TurnControlMeta::DhcpPacket {
                            egress,
                            interface,
                            local_mac,
                        },
                    ) => {
                        if let Some(change) = output.lease_change {
                            self.replace_dhcp_lease(&change);
                        }
                        if !output.handled {
                            self.queue_frontend_batch(
                                egress,
                                interface,
                                local_mac,
                                config,
                                alloc::vec![packet],
                            );
                        }
                    }
                    (
                        NetStackControlCommand::RemoveAutoconfigInterface {
                            output: Some(change),
                            ..
                        },
                        TurnControlMeta::RemoveAutoconfigInterface,
                    ) => {
                        if let Some(change) = change {
                            self.replace_dhcp_lease(&change);
                        }
                    }
                    (
                        NetStackControlCommand::ReleaseBinding { .. },
                        TurnControlMeta::ReleaseBinding {
                            facade,
                            publish_closed,
                        },
                    ) => {
                        if publish_closed && let Some(facade) = facade {
                            facade.publish_closed();
                        }
                    }
                    (
                        NetStackControlCommand::NeighborOwner {
                            output: Some(target),
                            ..
                        },
                        TurnControlMeta::NeighborOwner { work },
                    ) => self.publish_neighbor_to_owner(target, work),
                    (
                        NetStackControlCommand::NeighborOwner {
                            output: Some(target),
                            ..
                        },
                        TurnControlMeta::NeighborObservedOwner {
                            key,
                            mac_address,
                            now_ns,
                        },
                    ) => {
                        let _ = self.cluster.publish_control(
                            target,
                            ControlWork::NeighborObserved {
                                key,
                                mac_address,
                                now_ns,
                            },
                        );
                    }
                    (
                        NetStackControlCommand::FlowShard {
                            output: Some(owner),
                            ..
                        },
                        TurnControlMeta::TransportErrorOwner {
                            interface,
                            target,
                            error,
                            now_ns,
                        },
                    ) => {
                        let _ = self.cluster.publish_control(
                            owner,
                            ControlWork::TransportError {
                                interface,
                                target,
                                error,
                                now_ns,
                            },
                        );
                    }
                    (
                        NetStackControlCommand::JoinMulticast {
                            output: Some(Some(first)),
                            ..
                        },
                        TurnControlMeta::JoinMulticast { interface, group },
                    ) => {
                        if first {
                            let config = self.config.snapshot();
                            self.emit_multicast_control(interface, group, true, &config);
                        }
                    }
                    (
                        NetStackControlCommand::LeaveMulticast {
                            output: Some(Some((interface, last))),
                            ..
                        },
                        TurnControlMeta::LeaveMulticast { group },
                    ) => {
                        if last {
                            let config = self.config.snapshot();
                            self.emit_multicast_control(interface, group, false, &config);
                        }
                    }
                    (
                        NetStackControlCommand::MulticastGroups {
                            output: Some(groups),
                            ..
                        },
                        TurnControlMeta::MulticastGroups { interface },
                    ) => {
                        let config = self.config.snapshot();
                        for group in groups {
                            self.emit_multicast_control(interface, group, true, &config);
                        }
                    }
                    (
                        NetStackControlCommand::RemoveInterfaceMulticast { .. },
                        TurnControlMeta::RemoveInterfaceMulticast,
                    ) => {}
                    (
                        NetStackControlCommand::RemoveSocketMulticast {
                            output: Some(groups),
                            ..
                        },
                        TurnControlMeta::RemoveSocketMulticast,
                    ) => {
                        let config = self.config.snapshot();
                        for (interface, group) in groups {
                            self.emit_multicast_control(interface, group, false, &config);
                        }
                    }
                    (
                        NetStackControlCommand::ReserveBinding {
                            output: Some(result),
                            ..
                        },
                        TurnControlMeta::ReserveUdp {
                            facade,
                            sequence,
                            generation,
                            mut local,
                            peer,
                            interface,
                            options,
                        },
                    ) => match result {
                        Ok(token) if facade.generation() == generation => {
                            local.port = token.port;
                            facade.set_free_bind(options.free_bind);
                            let accepts_ipv4 = facade.family() == AddressFamily::Ipv6
                                && !options.v6_only
                                && matches!(local.addr, IpAddr::V6(address) if address.is_unspecified());
                            self.turn_commands
                                .0
                                .push(NetStackFlowCommand::BindUdpFacade {
                                    local,
                                    peer,
                                    interface,
                                    facade: Arc::clone(&facade),
                                    free_bind: options.free_bind,
                                    accepts_ipv4,
                                    output: None,
                                });
                            self.turn_meta.push(TurnCommandMeta::BindUdp {
                                facade,
                                sequence,
                                generation,
                                local,
                                peer,
                                interface,
                            });
                        }
                        Ok(_) => {
                            facade.complete_control(sequence, Err(SocketError::Closed));
                            self.queue_release_binding(facade, false);
                        }
                        Err(error) => {
                            facade.complete_control(sequence, Err(map_bind_error(error)));
                        }
                    },
                    (
                        NetStackControlCommand::ReserveBinding {
                            output: Some(result),
                            ..
                        },
                        TurnControlMeta::ReserveTcp {
                            facade,
                            sequence,
                            generation,
                            mut local,
                            interface,
                            options,
                            next,
                        },
                    ) => match result {
                        Ok(token) if facade.generation() == generation => {
                            local.port = token.port;
                            facade.set_free_bind(options.free_bind);
                            facade.publish_binding(
                                OwnerRef::Bound { generation },
                                local,
                                None,
                                interface,
                            );
                            match next {
                                TcpReserveNext::Bind => facade.complete_control(sequence, Ok(())),
                                TcpReserveNext::Connect { peer, path } => {
                                    let local_transport =
                                        config.interfaces.iter().any(|interface| {
                                            interface.id == path.route.interface
                                                && interface.loopback
                                        });
                                    self.queue_tcp_connect_owner(
                                        facade,
                                        sequence,
                                        generation,
                                        local,
                                        peer,
                                        path,
                                        local_transport,
                                    );
                                }
                                TcpReserveNext::Listen { backlog } => {
                                    self.queue_allocate_listener(
                                        facade, sequence, generation, backlog,
                                    );
                                }
                            }
                        }
                        Ok(_) => {
                            facade.complete_control(sequence, Err(SocketError::Closed));
                            self.queue_release_binding(facade, false);
                        }
                        Err(error) => {
                            facade.complete_control(sequence, Err(map_bind_error(error)));
                        }
                    },
                    (
                        NetStackControlCommand::FlowShard {
                            output: Some(target),
                            ..
                        },
                        TurnControlMeta::FlowShardTcpConnect {
                            facade,
                            sequence,
                            generation,
                            local,
                            peer,
                            path,
                            local_transport,
                        },
                    ) => {
                        if self
                            .cluster
                            .publish_control(
                                target,
                                ControlWork::ConnectTcp {
                                    facade: Arc::clone(&facade),
                                    sequence,
                                    generation,
                                    local,
                                    peer,
                                    path,
                                    local_transport,
                                },
                            )
                            .is_err()
                        {
                            facade.complete_control(sequence, Err(SocketError::NetworkDown));
                        }
                    }
                    (
                        NetStackControlCommand::AllocateListener {
                            output: Some(group),
                        },
                        TurnControlMeta::AllocateListener {
                            facade,
                            sequence,
                            generation,
                            backlog,
                        },
                    ) => {
                        self.publish_listener_install(facade, sequence, generation, backlog, group)
                    }
                    (
                        NetStackControlCommand::InstallListener {
                            output: Some(true), ..
                        },
                        TurnControlMeta::InstallListener { transaction },
                    ) => transaction.complete(),
                    (
                        NetStackControlCommand::InstallListener { .. },
                        TurnControlMeta::InstallListener { transaction },
                    ) => transaction.fail(),
                    (
                        NetStackControlCommand::RemoveListener { .. },
                        TurnControlMeta::RemoveListener { transaction },
                    ) => transaction.facade.publish_closed(),
                    (command, command_meta) => {
                        self.fail_turn_control_command(command, command_meta, config);
                    }
                }
            }
        } else {
            for (index, command_meta) in control_meta.drain(..).enumerate() {
                let command = control_commands
                    .take(index)
                    .expect("失败的 control command 必须归还所有权");
                self.fail_turn_control_command(command, command_meta, config);
            }
        }
        control_commands.clear();
        self.turn_control_commands.1 = Some(control_commands);
        let mut queued_control_meta = core::mem::take(&mut self.turn_control_meta);
        self.turn_control_meta = deferred_control_meta;
        self.turn_control_meta.append(&mut queued_control_meta);
        let mut commands = output.commands;
        let mut meta = meta;
        if committed {
            for (index, command_meta) in meta.drain(..).enumerate() {
                let command = commands
                    .take(index)
                    .expect("提交的 flow command 必须保持连续");
                match (command, command_meta) {
                    (
                        NetStackFlowCommand::ParsePacketBatch {
                            input: Some(batch),
                            output: Some(frontend),
                            ..
                        },
                        TurnCommandMeta::RawRx {
                            egress,
                            interface,
                            local_mac,
                        },
                    ) => {
                        self.recycle_rx_batch_container(egress, batch);
                        self.route_frontend_batch(egress, interface, local_mac, frontend);
                    }
                    (
                        NetStackFlowCommand::ProcessFrontendBatch {
                            packets: Some(returned),
                            output: Some((formed, recycled)),
                            drop_counts,
                            stats: Some(protocol_stats),
                            ..
                        },
                        TurnCommandMeta::Frontend {
                            egress,
                            interface,
                            local_mac,
                        },
                    ) => self.finish_frontend_turn(
                        egress,
                        interface,
                        local_mac,
                        returned,
                        formed,
                        recycled,
                        drop_counts,
                        protocol_stats,
                        config,
                    ),
                    (
                        NetStackFlowCommand::DrainReassembly {
                            packets, errors, ..
                        },
                        TurnCommandMeta::Reassembly {
                            egress,
                            interface,
                            local_mac,
                        },
                    ) => {
                        self.route_reassembled_packets(egress, interface, local_mac, packets);
                        self.route_transport_errors(errors);
                    }
                    (
                        NetStackFlowCommand::ProcessLocalTcpWork {
                            work: Some(work),
                            output,
                            ..
                        },
                        TurnCommandMeta::LocalTcp,
                    ) => {
                        if matches!(
                            output,
                            Some(Err(net::transport::TcpIngressError::NoEndpoint))
                        ) {
                            self.queue_tx_plan(PendingNeighborTx::Tcp(work));
                        } else if output.is_none() {
                            work.facade.set_pending_error(SocketError::NetworkDown);
                        }
                    }
                    (
                        NetStackFlowCommand::ProcessLocalUdpWork {
                            work: Some(work),
                            output,
                            ..
                        },
                        TurnCommandMeta::LocalUdp,
                    ) => match output {
                        Some(Ok(_)) => work.payload.complete(),
                        Some(Err(LocalUdpIngressError::Suppressed)) => work.payload.complete(),
                        Some(Err(
                            LocalUdpIngressError::NoEndpoint | LocalUdpIngressError::Unsupported,
                        )) => self.queue_tx_plan(PendingNeighborTx::Udp(work)),
                        Some(Err(LocalUdpIngressError::RingFull)) => work.payload.complete(),
                        None => work
                            .payload
                            .facade()
                            .set_pending_error(SocketError::NetworkDown),
                    },
                    (
                        NetStackFlowCommand::EnqueueNeighbor {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::NeighborEnqueue,
                    ) => match result {
                        net::stack::NeighborEnqueueOutput::Queued => {}
                        net::stack::NeighborEnqueueOutput::Resolved(work) => {
                            self.dispatch_neighbor_tx(work);
                        }
                        net::stack::NeighborEnqueueOutput::Rejected(work) => work
                            .facade()
                            .set_pending_error(SocketError::HostUnreachable),
                    },
                    (
                        NetStackFlowCommand::ObserveAndResolveNeighbor {
                            output: Some(resolved),
                            ..
                        },
                        TurnCommandMeta::NeighborObserved,
                    ) => {
                        for work in resolved {
                            self.dispatch_neighbor_tx(work);
                        }
                    }
                    (
                        NetStackFlowCommand::InvalidateInterface {
                            output: Some(_), ..
                        },
                        TurnCommandMeta::InterfaceInvalidated,
                    ) => {}
                    (
                        NetStackFlowCommand::FailInterfaceNeighbors {
                            output: Some(failed),
                            ..
                        },
                        TurnCommandMeta::InterfaceNeighbors { ack },
                    ) => {
                        for work in failed {
                            work.facade()
                                .set_pending_error(SocketError::NetworkUnreachable);
                        }
                        ack.finish();
                    }
                    (
                        NetStackFlowCommand::DrainTcpSend { .. },
                        TurnCommandMeta::StreamDirty { facade, generation },
                    ) => facade.finish_stream_tx_drain(generation),
                    (
                        NetStackFlowCommand::CooperativeSocketTx {
                            mut tcp_output,
                            mut udp_output,
                            result: Some(result),
                            ..
                        },
                        TurnCommandMeta::StreamDirtyLocal { facade, generation },
                    ) => {
                        let tcp_count = tcp_output.len();
                        for index in 0..tcp_count {
                            if let Some(work) = tcp_output.take(index) {
                                self.tcp_output.push(work);
                            }
                        }
                        debug_assert!(udp_output.is_empty());
                        udp_output.clear();
                        tcp_output.clear();
                        self.cooperative_scratch.push(CooperativeTxScratch {
                            tcp: tcp_output,
                            udp: udp_output,
                        });
                        if result.more_work {
                            self.runtime
                                .dirty
                                .try_push(Arc::clone(&facade))
                                .unwrap_or_else(|_| panic!("socket dirty queue 超出流表上限"));
                            self.runtime.publish_work();
                        } else {
                            facade.finish_stream_tx_drain(generation);
                        }
                    }
                    (
                        NetStackFlowCommand::PrepareUdpTx {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::UdpTx {
                            facade,
                            finish_drain,
                        },
                    ) => {
                        match result {
                            Ok(Some(work)) if work.unresolved_neighbor.is_some() => {
                                self.publish_neighbor_work(PendingNeighborTx::Udp(work));
                            }
                            Ok(Some(work)) => {
                                if let Some(target) =
                                    self.runtime.egress_index(work.route.interface)
                                {
                                    if config.interfaces.iter().any(|interface| {
                                        interface.id == work.route.interface && interface.loopback
                                    }) {
                                        self.dispatch_local_udp(target, work);
                                    } else {
                                        self.queue_tx_plan(PendingNeighborTx::Udp(work));
                                    }
                                } else {
                                    work.payload
                                        .facade()
                                        .set_pending_error(SocketError::NetworkUnreachable);
                                }
                            }
                            Ok(None) => {}
                            Err((error, payload)) => payload.facade().set_pending_error(error),
                        }
                        if finish_drain {
                            facade.finish_tx_drain();
                        }
                    }
                    (
                        NetStackFlowCommand::PrepareRawTx {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::RawTx {
                            facade,
                            finish_drain,
                        },
                    ) => {
                        match result {
                            Ok(Some(work)) if work.unresolved_neighbor.is_some() => {
                                self.publish_neighbor_work(PendingNeighborTx::Raw(work));
                            }
                            Ok(Some(work)) => {
                                if self.runtime.egress_index(work.route.interface).is_some() {
                                    self.queue_tx_plan(PendingNeighborTx::Raw(work));
                                } else {
                                    work.payload
                                        .facade()
                                        .set_pending_error(SocketError::NetworkUnreachable);
                                }
                            }
                            Ok(None) => {}
                            Err((error, payload)) => payload.facade().set_pending_error(error),
                        }
                        if finish_drain {
                            facade.finish_tx_drain();
                        }
                    }
                    (
                        NetStackFlowCommand::ApplyTransportError { .. },
                        TurnCommandMeta::TransportError,
                    ) => {}
                    (NetStackFlowCommand::PlanTxWork { work }, TurnCommandMeta::PlanTx) => {
                        if let Some(work) = work {
                            self.queue_tx_plan(work);
                        }
                    }
                    (
                        NetStackFlowCommand::CloseTcp { .. }
                        | NetStackFlowCommand::AbortTcp { .. }
                        | NetStackFlowCommand::ShutdownTcpWrite { .. },
                        TurnCommandMeta::TcpLifecycle {
                            facade,
                            release_binding,
                        },
                    ) => {
                        if release_binding {
                            self.queue_release_binding(facade, false);
                        }
                    }
                    (
                        NetStackFlowCommand::CloseUdp { .. },
                        TurnCommandMeta::DatagramClose { facade },
                    ) => self.queue_release_binding(facade, true),
                    (
                        NetStackFlowCommand::CloseRaw { .. },
                        TurnCommandMeta::RawClose { facade },
                    ) => facade.publish_closed(),
                    (
                        NetStackFlowCommand::BindRawFacade {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::BindRaw {
                            facade,
                            sequence,
                            generation,
                            local,
                            peer,
                            interface,
                        },
                    ) => match result {
                        Ok(flow) if facade.generation() == generation => {
                            facade.publish_binding(
                                OwnerRef::Flow {
                                    shard: self.runtime.id,
                                    flow,
                                    generation,
                                },
                                local,
                                peer,
                                interface,
                            );
                            facade.complete_control(sequence, Ok(()));
                        }
                        Ok(_) => facade.complete_control(sequence, Err(SocketError::Closed)),
                        Err(error) => facade.complete_control(
                            sequence,
                            Err(match error {
                                net::transport::RawBindError::InvalidEndpoint => {
                                    SocketError::AddressUnavailable
                                }
                                net::transport::RawBindError::TableFull => SocketError::Buffer,
                            }),
                        ),
                    },
                    (
                        NetStackFlowCommand::BindUdpFacade {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::BindUdp {
                            facade,
                            sequence,
                            generation,
                            local,
                            peer,
                            interface,
                        },
                    ) => match result {
                        Ok(flow) if facade.generation() == generation => {
                            facade.publish_binding(
                                OwnerRef::Flow {
                                    shard: self.runtime.id,
                                    flow,
                                    generation,
                                },
                                local,
                                peer,
                                interface,
                            );
                            facade.complete_control(sequence, Ok(()));
                        }
                        Ok(_) => {
                            facade.complete_control(sequence, Err(SocketError::Closed));
                            self.queue_release_binding(facade, false);
                        }
                        Err(error) => {
                            facade.complete_control(sequence, Err(map_udp_bind_error(error)));
                            self.queue_release_binding(facade, false);
                        }
                    },
                    (
                        NetStackFlowCommand::ReconnectUdpFacade {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::ReconnectUdp {
                            facade,
                            sequence,
                            generation,
                            local,
                            peer,
                            interface,
                        },
                    ) => match result {
                        Ok(flow) if facade.generation() == generation => {
                            facade.publish_binding(
                                OwnerRef::Flow {
                                    shard: self.runtime.id,
                                    flow,
                                    generation,
                                },
                                local,
                                Some(peer),
                                interface.or_else(|| facade.interface()),
                            );
                            facade.complete_control(sequence, Ok(()));
                        }
                        Ok(_) => facade.complete_control(sequence, Err(SocketError::Closed)),
                        Err(error) => {
                            facade.complete_control(sequence, Err(map_udp_bind_error(error)))
                        }
                    },
                    (
                        NetStackFlowCommand::ResolveTcpPath {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::ResolveTcpPath {
                            facade,
                            sequence,
                            generation,
                            peer,
                            options,
                        },
                    ) => match result {
                        Ok(path) if facade.generation() == generation => match facade.owner() {
                            OwnerRef::Unassigned => {
                                if let Err(error) = self.queue_tcp_binding(
                                    Arc::clone(&facade),
                                    sequence,
                                    generation,
                                    Endpoint {
                                        addr: path.route.source,
                                        port: 0,
                                    },
                                    Some(path.route.interface),
                                    options,
                                    TcpReserveNext::Connect { peer, path },
                                    config,
                                ) {
                                    facade.complete_control(sequence, Err(error));
                                }
                            }
                            OwnerRef::Bound { .. } => {
                                let Some(mut local) = facade.local_endpoint() else {
                                    facade
                                        .complete_control(sequence, Err(SocketError::InvalidState));
                                    continue;
                                };
                                if local.addr.is_unspecified() {
                                    local.addr = path.route.source;
                                }
                                let local_transport = config.interfaces.iter().any(|interface| {
                                    interface.id == path.route.interface && interface.loopback
                                });
                                self.queue_tcp_connect_owner(
                                    facade,
                                    sequence,
                                    generation,
                                    local,
                                    peer,
                                    path,
                                    local_transport,
                                );
                            }
                            OwnerRef::Closed { .. } => {
                                facade.complete_control(sequence, Err(SocketError::Closed));
                            }
                            _ => facade
                                .complete_control(sequence, Err(SocketError::AlreadyConnected)),
                        },
                        Ok(_) => facade.complete_control(sequence, Err(SocketError::Closed)),
                        Err(error) => facade.complete_control(sequence, Err(error)),
                    },
                    (
                        NetStackFlowCommand::ConnectTcp {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::ConnectTcp {
                            facade,
                            sequence,
                            generation,
                            local,
                            peer,
                            interface,
                        },
                    ) => match result {
                        Ok(flow) if facade.generation() == generation => facade.publish_binding(
                            OwnerRef::Flow {
                                shard: self.runtime.id,
                                flow,
                                generation,
                            },
                            local,
                            Some(peer),
                            Some(interface),
                        ),
                        Ok(_) => facade.complete_control(sequence, Err(SocketError::Closed)),
                        Err(error) => {
                            facade.complete_control(sequence, Err(map_tcp_bind_error(error)))
                        }
                    },
                    (
                        NetStackFlowCommand::ListenTcp {
                            output: Some(result),
                            ..
                        },
                        TurnCommandMeta::ListenTcp { transaction },
                    ) => transaction.finish(result.map_err(map_tcp_bind_error)),
                    (
                        NetStackFlowCommand::CloseTcpListener { .. },
                        TurnCommandMeta::CloseListener { transaction },
                    ) => {
                        if let Some(transaction) = transaction {
                            transaction.finish();
                        }
                    }
                    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                    (
                        NetStackFlowCommand::BindUdp {
                            output: Some(Ok(flow)),
                            ..
                        },
                        TurnCommandMeta::UdpProbeBindReceiver,
                    ) => {
                        self.udp_probe_flow = Some(flow);
                        self.udp_probe_polls_remaining = 8;
                        self.queue_pending_udp_probe(config);
                    }
                    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                    (
                        NetStackFlowCommand::BindUdp {
                            output: Some(Ok(flow)),
                            ..
                        },
                        TurnCommandMeta::UdpProbeBindSender,
                    ) => {
                        self.udp_probe_sender = Some(flow);
                        self.queue_pending_udp_probe(config);
                    }
                    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                    (
                        NetStackFlowCommand::FormUdpPacket {
                            output: Some(Ok(packet)),
                            ..
                        },
                        TurnCommandMeta::UdpProbeForm { egress },
                    ) => {
                        if let Some(target) = self.runtime.egress(egress) {
                            if target.try_push(EgressWork::Packet(packet)).is_err() {
                                target.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                    (
                        NetStackFlowCommand::RecvUdp {
                            output: Some(Some(datagram)),
                            ..
                        },
                        TurnCommandMeta::UdpProbeRecv,
                    ) => {
                        let mut payload = [0u8; 4];
                        if datagram.payload_len == 4
                            && datagram
                                .packet
                                .copy_out(usize::from(datagram.payload_offset), &mut payload)
                                .is_ok()
                            && payload == *b"ping"
                            && datagram.source.port == 1000
                            && datagram.destination.port == 9000
                        {
                            UDP_PROBE_COMPLETE.store(true, Ordering::Release);
                        }
                    }
                    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                    (
                        NetStackFlowCommand::BindUdp {
                            output: Some(Ok(flow)),
                            ..
                        },
                        TurnCommandMeta::PhysicalUdpProbeBind,
                    ) => {
                        self.physical_udp_probe_flow = Some(flow);
                        self.physical_udp_probe_sender = Some(flow);
                        self.physical_udp_probe_polls_remaining = 8;
                        self.queue_pending_physical_udp_probe(config);
                    }
                    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                    (
                        NetStackFlowCommand::FormUdpPacket {
                            output: Some(Ok(packet)),
                            ..
                        },
                        TurnCommandMeta::PhysicalUdpProbeForm { egress },
                    ) => {
                        if let Some(target) = self.runtime.egress(egress) {
                            if target.try_push(EgressWork::Packet(packet)).is_err() {
                                target.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
                            } else {
                                PHYSICAL_UDP_TX_SUBMITTED.store(true, Ordering::Release);
                            }
                        }
                    }
                    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                    (
                        NetStackFlowCommand::RecvUdp {
                            output: Some(Some(datagram)),
                            ..
                        },
                        TurnCommandMeta::PhysicalUdpProbeRecv,
                    ) => {
                        let mut header = [0u8; 4];
                        if datagram.payload_len >= 12
                            && datagram
                                .packet
                                .copy_out(usize::from(datagram.payload_offset), &mut header)
                                .is_ok()
                            && header[0..2] == [0x4d, 0x47]
                            && header[2] & 0x80 != 0
                            && datagram.source.port == 53
                            && datagram.destination.port == 53_000
                        {
                            PHYSICAL_UDP_REPLY_SEEN.store(true, Ordering::Release);
                            for target in self.runtime.egress_snapshot() {
                                if let Some(task) = target.task.lock().as_ref().cloned() {
                                    let _ = sched::activate_task(&task);
                                }
                            }
                        }
                    }
                    (command, command_meta) => self.fail_turn_command(command, Some(command_meta)),
                }
            }
        } else {
            for (index, command_meta) in meta.drain(..).enumerate() {
                let command = commands
                    .take(index)
                    .expect("失败的 flow command 必须归还所有权");
                self.fail_turn_command(command, Some(command_meta));
            }
        }
        commands.clear();
        self.turn_commands.1 = Some(commands);
        let mut queued_meta = core::mem::take(&mut self.turn_meta);
        self.turn_meta = deferred_meta;
        self.turn_meta.append(&mut queued_meta);
        let continuation_pending =
            !self.turn_control_commands.0.is_empty() || !self.turn_commands.0.is_empty();
        self.dispatch_tcp_output_batch(config);
        self.publish_protocol_stats(turn_stats);
        let neighbor_deadline = self.finish_neighbor_timers(neighbor_timers, config);
        (
            [next_timer_deadline, neighbor_deadline, control_deadline]
                .into_iter()
                .flatten()
                .min(),
            blocked || deferred || continuation_pending,
        )
    }

    fn publish_protocol_stats(&self, protocol_stats: net::flow::FlowShardStats) {
        self.runtime.publish_protocol_stats(protocol_stats);
    }

    fn recycle_rx_batch_container(&mut self, egress: usize, batch: PacketBatch) {
        debug_assert!(batch.is_empty());
        if let Some(queue) = self
            .local_queues
            .iter_mut()
            .find(|queue| queue.egress_index == egress)
        {
            queue.spare_rx_batches.push(batch);
        }
    }

    fn route_frontend_batch(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        mut batch: FrontendBatch,
    ) {
        for index in 0..batch.len() {
            let Some(packet) = batch.take(index) else {
                continue;
            };
            self.route_frontend_packet(egress, interface, local_mac, packet);
        }
    }

    fn route_reassembled_packets(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        packets: Vec<FrontendPacket>,
    ) {
        for packet in packets {
            self.route_frontend_packet(egress, interface, local_mac, packet);
        }
    }

    fn route_frontend_packet(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        packet: FrontendPacket,
    ) {
        let destination = match packet.parsed.disposition {
            FrontendDisposition::Tcp => self.cluster.ingress_target(packet.parsed.rss_hash),
            FrontendDisposition::Control(net::pipeline::ControlPacket::Fragment(ip)) => {
                let fragment = ip
                    .fragment
                    .expect("fragment disposition 必须携带分片 sidecar");
                let hash = net::flow::fragment_rss_hash(
                    &self.cluster.rss_key,
                    interface,
                    ip.source,
                    ip.destination,
                    ip.next_header,
                    fragment.identification,
                );
                self.cluster.ingress_target(Some(hash))
            }
            FrontendDisposition::Udp
            | FrontendDisposition::Raw
            | FrontendDisposition::Control(_)
            | FrontendDisposition::Drop(_) => self.cluster.coordinator(),
        };
        let work = IngressWork::Packet(IngressPacket {
            egress,
            interface,
            local_mac,
            packet,
        });
        if destination.id == self.runtime.id {
            self.local_ingress.push_back(work);
            return;
        }
        if let Err(IngressWork::Packet(packet)) = destination.try_push(work) {
            if let Some(target) = self.runtime.egress(egress) {
                target.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                target.stats.drop_reasons[DropReason::IngressRingFull.index()]
                    .fetch_add(1, Ordering::Relaxed);
            }
            drop(packet.packet.chain);
        }
    }

    fn route_transport_errors(
        &mut self,
        errors: Vec<(
            InterfaceId,
            net::transport::ControlErrorTarget,
            net::transport::TransportControlError,
            u64,
        )>,
    ) {
        for (interface, target, error, now_ns) in errors {
            let _ = self.cluster.publish_control(
                ShardId(0),
                ControlWork::TransportErrorOwner {
                    interface,
                    target,
                    error,
                    now_ns,
                },
            );
        }
    }

    fn fail_turn_control_command(
        &mut self,
        command: NetStackControlCommand,
        meta: TurnControlMeta,
        config: &ConfigSnapshot,
    ) {
        match (command, meta) {
            (
                NetStackControlCommand::HandleDhcpPacket {
                    packet: Some(packet),
                    ..
                },
                TurnControlMeta::DhcpPacket {
                    egress,
                    interface,
                    local_mac,
                },
            ) => {
                self.queue_frontend_batch(egress, interface, local_mac, config, alloc::vec![packet])
            }
            (
                _,
                TurnControlMeta::ReserveUdp {
                    facade, sequence, ..
                }
                | TurnControlMeta::ReserveTcp {
                    facade, sequence, ..
                }
                | TurnControlMeta::FlowShardTcpConnect {
                    facade, sequence, ..
                }
                | TurnControlMeta::AllocateListener {
                    facade, sequence, ..
                },
            ) => facade.complete_control(sequence, Err(SocketError::RuntimeBusy)),
            (_, TurnControlMeta::InstallListener { transaction }) => transaction.fail(),
            (_, TurnControlMeta::RemoveListener { transaction }) => {
                transaction.facade.publish_closed();
            }
            (
                _,
                TurnControlMeta::ReleaseBinding {
                    facade: Some(facade),
                    publish_closed: true,
                },
            ) => facade.publish_closed(),
            (_, TurnControlMeta::NeighborOwner { work }) => {
                work.facade().set_pending_error(SocketError::RuntimeBusy);
            }
            _ => {}
        }
    }

    fn fail_turn_command(&mut self, command: NetStackFlowCommand, meta: Option<TurnCommandMeta>) {
        match command {
            NetStackFlowCommand::ParsePacketBatch {
                input: Some(mut batch),
                ..
            } => {
                let egress = match meta.as_ref() {
                    Some(TurnCommandMeta::RawRx { egress, .. }) => Some(*egress),
                    _ => None,
                };
                for index in 0..batch.len() {
                    let Some((chain, _)) = batch.take(index) else {
                        continue;
                    };
                    drop(chain);
                }
                if let Some(egress) = egress {
                    self.recycle_rx_batch_container(egress, batch);
                }
            }
            NetStackFlowCommand::ProcessFrontendBatch {
                packets: Some(packets),
                ..
            } => drop(packets),
            NetStackFlowCommand::ProcessLocalTcpWork {
                work: Some(work), ..
            } => work.facade.set_pending_error(SocketError::NetworkDown),
            NetStackFlowCommand::ProcessLocalUdpWork {
                work: Some(work), ..
            } => work
                .payload
                .facade()
                .set_pending_error(SocketError::NetworkDown),
            NetStackFlowCommand::PlanTxWork { work: Some(work) } => {
                work.facade().set_pending_error(SocketError::RuntimeBusy)
            }
            NetStackFlowCommand::PrepareUdpTx {
                payload: Some(payload),
                ..
            }
            | NetStackFlowCommand::PrepareRawTx {
                payload: Some(payload),
                ..
            } => payload.facade().set_pending_error(SocketError::RuntimeBusy),
            NetStackFlowCommand::EnqueueNeighbor { work, output, .. } => match output {
                Some(net::stack::NeighborEnqueueOutput::Queued) => {}
                Some(net::stack::NeighborEnqueueOutput::Resolved(work)) => {
                    self.dispatch_neighbor_tx(work);
                }
                Some(net::stack::NeighborEnqueueOutput::Rejected(work)) => work
                    .facade()
                    .set_pending_error(SocketError::HostUnreachable),
                None => {
                    if let Some(work) = work {
                        work.facade().set_pending_error(SocketError::RuntimeBusy);
                    }
                }
            },
            NetStackFlowCommand::ObserveAndResolveNeighbor {
                output: Some(resolved),
                ..
            } => {
                for work in resolved {
                    self.dispatch_neighbor_tx(work);
                }
            }
            NetStackFlowCommand::FailInterfaceNeighbors {
                output: Some(failed),
                ..
            } => {
                for work in failed {
                    work.facade()
                        .set_pending_error(SocketError::NetworkUnreachable);
                }
            }
            NetStackFlowCommand::CooperativeSocketTx {
                mut tcp_output,
                mut udp_output,
                ..
            } => {
                let tcp_count = tcp_output.len();
                for index in 0..tcp_count {
                    if let Some(work) = tcp_output.take(index) {
                        work.facade.set_pending_error(SocketError::RuntimeBusy);
                    }
                }
                let udp_count = udp_output.len();
                for index in 0..udp_count {
                    if let Some(outcome) = udp_output.take(index) {
                        match outcome {
                            NetStackCooperativeUdpTx::Prepared(work) => work
                                .payload
                                .facade()
                                .set_pending_error(SocketError::RuntimeBusy),
                            NetStackCooperativeUdpTx::Failed(_, payload) => {
                                payload.facade().set_pending_error(SocketError::RuntimeBusy)
                            }
                        }
                    }
                }
                tcp_output.clear();
                udp_output.clear();
                self.cooperative_scratch.push(CooperativeTxScratch {
                    tcp: tcp_output,
                    udp: udp_output,
                });
            }
            _ => {}
        }
        match meta {
            Some(TurnCommandMeta::StreamDirty { facade, generation }) => {
                facade.finish_stream_tx_drain(generation);
            }
            Some(TurnCommandMeta::StreamDirtyLocal { facade, generation }) => {
                facade.set_pending_error(SocketError::RuntimeBusy);
                facade.finish_stream_tx_drain(generation);
            }
            Some(TurnCommandMeta::PlanTx) => {}
            Some(TurnCommandMeta::UdpTx {
                facade,
                finish_drain: true,
            })
            | Some(TurnCommandMeta::RawTx {
                facade,
                finish_drain: true,
            }) => facade.finish_tx_drain(),
            Some(TurnCommandMeta::InterfaceNeighbors { ack }) => ack.finish(),
            Some(TurnCommandMeta::BindRaw {
                facade, sequence, ..
            })
            | Some(TurnCommandMeta::ReconnectUdp {
                facade, sequence, ..
            })
            | Some(TurnCommandMeta::ResolveTcpPath {
                facade, sequence, ..
            })
            | Some(TurnCommandMeta::ConnectTcp {
                facade, sequence, ..
            }) => facade.complete_control(sequence, Err(SocketError::RuntimeBusy)),
            Some(TurnCommandMeta::BindUdp {
                facade, sequence, ..
            }) => {
                facade.complete_control(sequence, Err(SocketError::RuntimeBusy));
                self.queue_release_binding(facade, false);
            }
            Some(TurnCommandMeta::ListenTcp { transaction }) => {
                transaction.finish(Err(SocketError::RuntimeBusy));
            }
            Some(TurnCommandMeta::CloseListener { transaction }) => {
                if let Some(transaction) = transaction {
                    transaction.finish();
                }
            }
            Some(TurnCommandMeta::TcpLifecycle {
                facade,
                release_binding,
            }) => {
                facade.set_pending_error(SocketError::NetworkDown);
                if release_binding {
                    self.queue_release_binding(facade, false);
                }
            }
            Some(TurnCommandMeta::DatagramClose { facade }) => {
                facade.set_pending_error(SocketError::NetworkDown);
                self.queue_release_binding(facade, true);
            }
            Some(TurnCommandMeta::RawClose { facade }) => facade.publish_closed(),
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_frontend_turn(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        mut returned: Vec<FrontendPacket>,
        mut formed: TxBatch,
        mut recycled: PacketBatch,
        drop_counts: [u32; DropReason::COUNT],
        protocol_stats: net::flow::FlowShardStats,
        config: &ConfigSnapshot,
    ) {
        let Some(target) = self.runtime.egress(egress) else {
            return;
        };
        let stats = Arc::clone(&target.stats);
        for (index, count) in drop_counts.into_iter().enumerate() {
            if count == 0 {
                continue;
            }
            stats
                .rx_dropped
                .fetch_add(u64::from(count), Ordering::Relaxed);
            stats.drop_reasons[index].fetch_add(u64::from(count), Ordering::Relaxed);
            if index == DropReason::NoConsumer.index() {
                stats
                    .rx_no_consumer
                    .fetch_add(u64::from(count), Ordering::Relaxed);
            }
        }
        for index in 0..formed.len() {
            if let Some(packet) = formed.take(index) {
                self.dispatch_egress(egress, EgressWork::Packet(packet));
            }
        }
        for index in 0..recycled.len() {
            let _ = recycled.take(index);
        }
        returned.clear();
        if self.frontend_packets.capacity() < returned.capacity() {
            self.frontend_packets = returned;
        }
        let _ = (interface, local_mac, config);
        stats
            .protocol_tcp_delivered
            .store(protocol_stats.tcp_delivered, Ordering::Relaxed);
        stats
            .protocol_udp_delivered
            .store(protocol_stats.udp_delivered, Ordering::Relaxed);
        stats
            .protocol_control_packets
            .store(protocol_stats.control_packets, Ordering::Relaxed);
        stats
            .protocol_tx_formed
            .store(protocol_stats.tx_formed, Ordering::Relaxed);
        stats
            .protocol_dirty_runs
            .store(protocol_stats.dirty_runs, Ordering::Relaxed);
        stats
            .protocol_timer_expired
            .store(protocol_stats.timer_expired, Ordering::Relaxed);
        stats
            .tcp_rx_pinned_bytes
            .store(protocol_stats.tcp_rx_pinned_bytes, Ordering::Relaxed);
        stats
            .tcp_rx_compact_copy_bytes
            .store(protocol_stats.tcp_rx_compact_copy_bytes, Ordering::Relaxed);
        stats
            .tcp_loopback_shared_bytes
            .store(protocol_stats.tcp_loopback_shared_bytes, Ordering::Relaxed);
        stats.tcp_rx_pool_low_water_fallbacks.store(
            protocol_stats.tcp_rx_pool_low_water_fallbacks,
            Ordering::Relaxed,
        );
    }

    fn process_pending(&mut self, count: usize, config: &ConfigSnapshot) {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::NetProtocolIngress).packets(count);
        #[cfg(feature = "performance-profile")]
        let mut local_count = 0usize;
        for index in 0..count {
            let Some(work) = self.pending[index].take() else {
                continue;
            };
            match work {
                IngressWork::Packet(packet) => {
                    let egress = packet.egress;
                    let interface = packet.interface;
                    let local_mac = packet.local_mac;
                    self.frontend_packets.clear();
                    self.observe_ingress_neighbor(interface, &packet.packet);
                    self.queue_dhcp_or_frontend(egress, interface, local_mac, packet.packet);
                    for candidate in index + 1..count {
                        let same_source = matches!(
                            self.pending[candidate].as_ref(),
                            Some(IngressWork::Packet(packet))
                                if packet.egress == egress && packet.interface == interface
                        );
                        if !same_source {
                            continue;
                        }
                        let Some(IngressWork::Packet(packet)) = self.pending[candidate].take()
                        else {
                            unreachable!();
                        };
                        self.observe_ingress_neighbor(interface, &packet.packet);
                        self.queue_dhcp_or_frontend(egress, interface, local_mac, packet.packet);
                    }
                    if !self.frontend_packets.is_empty() {
                        let packets = core::mem::take(&mut self.frontend_packets);
                        self.queue_frontend_batch(egress, interface, local_mac, config, packets);
                    }
                }
                IngressWork::LocalTcp { interface, work } => {
                    #[cfg(feature = "performance-profile")]
                    {
                        local_count += 1;
                    }
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::ProcessLocalTcpWork {
                            interface,
                            work: Some(work),
                            config: config as *const _,
                            now_ns: sched::now_ns_direct(),
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::LocalTcp);
                }
                IngressWork::LocalUdp { interface, work } => {
                    #[cfg(feature = "performance-profile")]
                    {
                        local_count += 1;
                    }
                    self.turn_commands
                        .0
                        .push(NetStackFlowCommand::ProcessLocalUdpWork {
                            interface,
                            work: Some(work),
                            now_ns: sched::now_ns_direct(),
                            output: None,
                        });
                    self.turn_meta.push(TurnCommandMeta::LocalUdp);
                }
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                IngressWork::UdpProbe { egress, payload } => {
                    self.start_udp_probe(egress, payload, config);
                }
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                IngressWork::PhysicalUdpProbe { egress, payload } => {
                    self.start_physical_udp_probe(egress, payload, config);
                }
            }
        }
        for pending in &mut self.pending[..count] {
            *pending = None;
        }
        #[cfg(feature = "performance-profile")]
        if local_count != 0 {
            profiling::observe(profiling::Metric::LocalWorkBatchSize, local_count as u64);
        }
    }

    fn queue_dhcp_or_frontend(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        packet: FrontendPacket,
    ) {
        let is_dhcp_reply = packet
            .parsed
            .udp
            .is_some_and(|udp| udp.source_port == 67 && udp.destination_port == 68);
        if !is_dhcp_reply {
            self.frontend_packets.push(packet);
            return;
        }
        self.turn_control_commands
            .0
            .push(NetStackControlCommand::HandleDhcpPacket {
                interface,
                packet: Some(packet),
                now_ns: sched::now_ns_direct(),
                output: None,
            });
        self.turn_control_meta.push(TurnControlMeta::DhcpPacket {
            egress,
            interface,
            local_mac,
        });
    }

    fn queue_frontend_batch(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        config: &ConfigSnapshot,
        packets: Vec<FrontendPacket>,
    ) {
        self.turn_commands
            .0
            .push(NetStackFlowCommand::ProcessFrontendBatch {
                packets: Some(packets),
                interface,
                local_mac,
                config: config as *const _,
                now_ns: sched::now_ns_direct(),
                output: None,
                drop_counts: [0; DropReason::COUNT],
                stats: None,
            });
        self.turn_meta.push(TurnCommandMeta::Frontend {
            egress,
            interface,
            local_mac,
        });
        self.turn_commands
            .0
            .push(NetStackFlowCommand::DrainReassembly {
                interface,
                config: config as *const _,
                packets: Vec::new(),
                errors: Vec::new(),
            });
        self.turn_meta.push(TurnCommandMeta::Reassembly {
            egress,
            interface,
            local_mac,
        });
    }

    fn observe_ingress_neighbor(&mut self, interface: InterfaceId, packet: &FrontendPacket) {
        if let FrontendDisposition::Control(net::pipeline::ControlPacket::Icmp {
            ipv6: true,
            packet_offset,
            packet_len,
        }) = packet.parsed.disposition
            && packet_len >= 24
        {
            let mut message = [0u8; 24];
            if packet
                .chain
                .copy_out(usize::from(packet_offset), &mut message)
                .is_ok()
                && matches!(message[0], 135 | 136)
            {
                let target = Ipv6Addr(message[8..24].try_into().unwrap());
                self.turn_control_commands
                    .0
                    .push(NetStackControlCommand::ObserveDadConflict {
                        interface,
                        address: target,
                    });
                self.turn_control_meta.push(TurnControlMeta::DadConflict);
            }
        }
        let observed = match packet.parsed.disposition {
            FrontendDisposition::Control(net::pipeline::ControlPacket::Arp(arp)) => Some((
                net::control::NeighborKey {
                    interface,
                    address: IpAddr::V4(arp.sender_ip),
                },
                arp.sender_mac,
            )),
            FrontendDisposition::Control(net::pipeline::ControlPacket::Icmp {
                ipv6: true,
                packet_offset,
                packet_len,
            }) if packet_len >= 24 => {
                let mut advertisement = [0u8; 24];
                if packet
                    .chain
                    .copy_out(usize::from(packet_offset), &mut advertisement)
                    .is_err()
                    || advertisement[0] != 136
                {
                    None
                } else {
                    Some((
                        net::control::NeighborKey {
                            interface,
                            address: IpAddr::V6(Ipv6Addr(advertisement[8..24].try_into().unwrap())),
                        },
                        packet.parsed.ethernet.source,
                    ))
                }
            }
            _ => None,
        };
        let Some((key, mac_address)) = observed else {
            return;
        };
        let now_ns = packet.metadata.rx_timestamp_ns.max(sched::now_ns_direct());
        let _ = self.cluster.publish_control(
            ShardId(0),
            ControlWork::NeighborObservedOwner {
                key,
                mac_address,
                now_ns,
            },
        );
    }

    fn dispatch_tcp_output_batch(&mut self, config: &ConfigSnapshot) {
        let mut output = core::mem::take(&mut self.tcp_output);
        for work in output.drain(..) {
            self.install_stream_tx_pool(&work.facade, work.path.route.interface);
            if work.path.unresolved_neighbor.is_some() {
                self.publish_neighbor_work(PendingNeighborTx::Tcp(work));
                continue;
            }
            let Some(target) = self.runtime.egress_index(work.path.route.interface) else {
                work.facade
                    .set_pending_error(SocketError::NetworkUnreachable);
                continue;
            };
            if config
                .interfaces
                .iter()
                .any(|interface| interface.id == work.path.route.interface && interface.loopback)
            {
                self.dispatch_local_tcp(target, work);
                continue;
            }
            self.queue_tx_plan(PendingNeighborTx::Tcp(work));
        }
        self.tcp_output = output;
    }

    fn install_stream_tx_pool(&self, facade: &SocketFacade, interface: InterfaceId) {
        let Some(target) = self.runtime.egress_index(interface) else {
            return;
        };
        let Some(egress) = self.runtime.egress(target) else {
            return;
        };
        facade.install_stream_tx_pool(Arc::clone(&egress.tx_payload_pool));
    }

    fn dispatch_local_tcp(&mut self, _egress: usize, work: PreparedTcpTx) {
        work.facade.prepare_local_stream_send();
        let source = Endpoint {
            addr: work.path.route.source,
            port: work.local_port,
        };
        let key = FlowKey::new(source, work.remote, TransportProtocol::Tcp)
            .expect("TCP local transport tuple 必须有效");
        let target = self.cluster.local_tcp_ingress_target(&key);
        let ingress = IngressWork::LocalTcp {
            interface: work.path.route.interface,
            work,
        };
        if target.id == self.runtime.id {
            self.local_ingress.push_back(ingress);
            return;
        }
        push_local_ingress(&target, ingress);
    }

    fn dispatch_egress(&mut self, target: usize, mut work: EgressWork) {
        let local = self
            .local_queues
            .iter()
            .any(|queue| queue.egress_index == target);
        if !local {
            let Some(egress) = self.runtime.egress(target) else {
                fail_egress_work(work, SocketError::NetworkUnreachable);
                return;
            };
            if let Err(work) = egress.push_wait(work) {
                fail_egress_work(work, SocketError::NetworkUnreachable);
            }
            return;
        }

        let Some(egress) = self.runtime.egress(target) else {
            fail_egress_work(work, SocketError::NetworkUnreachable);
            return;
        };
        loop {
            match egress.try_push_deferred(work) {
                Ok(()) => {
                    egress.pending.0.store(true, Ordering::Release);
                    return;
                }
                Err(pending) => {
                    work = pending;
                    if !self.pump_egress_queue(target) {
                        fail_egress_work(work, SocketError::NetworkUnreachable);
                        return;
                    }
                }
            }
        }
    }

    fn pump_egress_queue(&mut self, target: usize) -> bool {
        let Some(index) = self
            .local_queues
            .iter()
            .position(|queue| queue.egress_index == target)
        else {
            return false;
        };
        let removed = self.local_queues[index].run_turn() == WorkerTurn::Removed;
        self.local_queues[index].initialized = true;
        if removed {
            let mut queue = self.local_queues.remove(index);
            queue.finish_removal();
            false
        } else {
            true
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn start_udp_probe(&mut self, egress: usize, payload: PacketChain, config: &ConfigSnapshot) {
        let Some(egress) = self.runtime.egress(egress) else {
            return;
        };
        let interface = egress.interface;
        let receiver = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let sender = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 1000,
        };
        self.udp_probe_pending = Some((self.runtime.egress_index(interface).unwrap(), payload));
        if self.udp_probe_flow.is_none() {
            self.turn_commands.0.push(NetStackFlowCommand::BindUdp {
                local: receiver,
                peer: Some(sender),
                interface: Some(interface),
                output: None,
            });
            self.turn_meta.push(TurnCommandMeta::UdpProbeBindReceiver);
        }
        if self.udp_probe_sender.is_none() {
            self.turn_commands.0.push(NetStackFlowCommand::BindUdp {
                local: sender,
                peer: Some(receiver),
                interface: Some(interface),
                output: None,
            });
            self.turn_meta.push(TurnCommandMeta::UdpProbeBindSender);
        }
        self.queue_pending_udp_probe(config);
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn queue_pending_udp_probe(&mut self, config: &ConfigSnapshot) {
        let Some(flow) = self.udp_probe_sender else {
            return;
        };
        if self.udp_probe_flow.is_none() {
            return;
        }
        let Some((egress, payload)) = self.udp_probe_pending.take() else {
            return;
        };
        self.turn_commands
            .0
            .push(NetStackFlowCommand::FormUdpPacket {
                flow,
                destination: None,
                payload: Some(payload),
                mark: 0,
                config: config as *const _,
                now_ns: sched::now_ns_direct(),
                output: None,
            });
        self.turn_meta
            .push(TurnCommandMeta::UdpProbeForm { egress });
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn start_physical_udp_probe(
        &mut self,
        egress: usize,
        payload: PacketChain,
        config: &ConfigSnapshot,
    ) {
        let Some(egress) = self.runtime.egress(egress) else {
            return;
        };
        let interface = egress.interface;
        let receiver = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
            port: 53_000,
        };
        let dns = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 3)),
            port: 53,
        };
        self.physical_udp_probe_pending =
            Some((self.runtime.egress_index(interface).unwrap(), payload));
        if self.physical_udp_probe_flow.is_none() {
            self.turn_commands.0.push(NetStackFlowCommand::BindUdp {
                local: receiver,
                peer: Some(dns),
                interface: Some(interface),
                output: None,
            });
            self.turn_meta.push(TurnCommandMeta::PhysicalUdpProbeBind);
        }
        self.queue_pending_physical_udp_probe(config);
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn queue_pending_physical_udp_probe(&mut self, config: &ConfigSnapshot) {
        let Some(flow) = self.physical_udp_probe_sender else {
            return;
        };
        let Some((egress, payload)) = self.physical_udp_probe_pending.take() else {
            return;
        };
        self.turn_commands
            .0
            .push(NetStackFlowCommand::FormUdpPacket {
                flow,
                destination: None,
                payload: Some(payload),
                mark: 0,
                config: config as *const _,
                now_ns: sched::now_ns_direct(),
                output: None,
            });
        self.turn_meta
            .push(TurnCommandMeta::PhysicalUdpProbeForm { egress });
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn queue_udp_probe_observers(&mut self) {
        let delivery_pending = self.turn_meta.iter().any(|meta| {
            matches!(
                meta,
                TurnCommandMeta::Frontend { .. } | TurnCommandMeta::LocalUdp
            )
        });
        if delivery_pending {
            if self.udp_probe_flow.is_some() {
                self.udp_probe_polls_remaining = 8;
            }
            if self.physical_udp_probe_flow.is_some() {
                self.physical_udp_probe_polls_remaining = 8;
            }
        }
        if !UDP_PROBE_COMPLETE.load(Ordering::Acquire)
            && let Some(flow) = self.udp_probe_flow
            && self.udp_probe_polls_remaining != 0
        {
            self.udp_probe_polls_remaining -= 1;
            self.turn_commands
                .0
                .push(NetStackFlowCommand::RecvUdp { flow, output: None });
            self.turn_meta.push(TurnCommandMeta::UdpProbeRecv);
        }
        if !PHYSICAL_UDP_REPLY_SEEN.load(Ordering::Acquire)
            && let Some(flow) = self.physical_udp_probe_flow
            && self.physical_udp_probe_polls_remaining != 0
        {
            self.physical_udp_probe_polls_remaining -= 1;
            self.turn_commands
                .0
                .push(NetStackFlowCommand::RecvUdp { flow, output: None });
            self.turn_meta.push(TurnCommandMeta::PhysicalUdpProbeRecv);
        }
    }

    fn sleep_until_work(&mut self) {
        let task = sched::current_task_direct();
        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(sched::WaitReason::Other, sched::now_ns_direct());
        if !self.runtime.work_signal.begin_sleep() {
            self.runtime.work_signal.end_sleep();
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            return;
        }
        if !task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            self.runtime.work_signal.end_sleep();
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            return;
        }
        for queue in &self.local_queues {
            queue.irq.unmask();
        }
        fence(Ordering::SeqCst);
        if self.runtime.work_signal.sleep_invalidated()
            || self.runtime.timer_fired.load(Ordering::Acquire)
            || !self.runtime.ingress.is_empty()
            || !self.runtime.control.is_empty()
            || !self.runtime.dirty.is_empty()
            || !self.runtime.lifecycle.is_empty()
            || !self.runtime.queue_attach.is_empty()
            || !self.local_ingress.is_empty()
            || self.local_queue_pending()
        {
            for queue in &self.local_queues {
                let _ = queue.irq.ack_and_mask();
            }
            let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running);
            self.runtime.work_signal.end_sleep();
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            return;
        }
        drop(task);
        sched::schedule_once(sched::now_ns_direct());
        self.runtime.work_signal.end_sleep();
    }
}

fn address_matches_family(family: AddressFamily, address: IpAddr) -> bool {
    matches!(
        (family, address),
        (AddressFamily::Ipv4, IpAddr::V4(_)) | (AddressFamily::Ipv6, IpAddr::V6(_))
    )
}

fn address_family(address: IpAddr) -> AddressFamily {
    match address {
        IpAddr::V4(_) => AddressFamily::Ipv4,
        IpAddr::V6(_) => AddressFamily::Ipv6,
    }
}

fn address_allowed(family: AddressFamily, address: IpAddr, v6_only: bool) -> bool {
    address_matches_family(family, address)
        || (family == AddressFamily::Ipv6 && !v6_only && matches!(address, IpAddr::V4(_)))
}

fn unspecified_address(family: AddressFamily) -> IpAddr {
    match family {
        AddressFamily::Ipv4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        AddressFamily::Ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

fn build_neighbor_probe(
    key: net::control::NeighborKey,
    config: &ConfigSnapshot,
    dad: bool,
) -> Option<Vec<u8>> {
    let interface = config
        .interfaces
        .iter()
        .find(|interface| interface.id == key.interface)?;
    match key.address {
        IpAddr::V4(target) => {
            let source = config
                .addresses
                .iter()
                .find_map(|entry| {
                    (entry.interface == key.interface && entry.primary)
                        .then_some(entry.address)
                        .and_then(|address| match address {
                            IpAddr::V4(address) => Some(address),
                            IpAddr::V6(_) => None,
                        })
                })
                .unwrap_or(Ipv4Addr::UNSPECIFIED);
            let mut frame = alloc::vec![0; 42];
            frame[..6].fill(0xff);
            frame[6..12].copy_from_slice(&interface.mac_address);
            frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
            frame[14..16].copy_from_slice(&1u16.to_be_bytes());
            frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
            frame[18] = 6;
            frame[19] = 4;
            frame[20..22].copy_from_slice(&1u16.to_be_bytes());
            frame[22..28].copy_from_slice(&interface.mac_address);
            frame[28..32].copy_from_slice(&source.0);
            frame[32..38].fill(0);
            frame[38..42].copy_from_slice(&target.0);
            Some(frame)
        }
        IpAddr::V6(target) => {
            let source = if dad {
                Ipv6Addr::UNSPECIFIED
            } else {
                config
                    .addresses
                    .iter()
                    .find_map(|entry| {
                        (entry.interface == key.interface && entry.primary)
                            .then_some(entry.address)
                            .and_then(|address| match address {
                                IpAddr::V6(address) => Some(address),
                                IpAddr::V4(_) => None,
                            })
                    })
                    .unwrap_or(Ipv6Addr::UNSPECIFIED)
            };
            let mut destination = [0u8; 16];
            destination[0] = 0xff;
            destination[1] = 0x02;
            destination[11] = 0x01;
            destination[12] = 0xff;
            destination[13..16].copy_from_slice(&target.0[13..16]);
            let destination = Ipv6Addr(destination);
            let include_source = !source.is_unspecified();
            let icmp_len = if include_source { 32usize } else { 24usize };
            let mut frame = alloc::vec![0; 14 + 40 + icmp_len];
            frame[0..6].copy_from_slice(&[
                0x33,
                0x33,
                0xff,
                target.0[13],
                target.0[14],
                target.0[15],
            ]);
            frame[6..12].copy_from_slice(&interface.mac_address);
            frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            frame[14..18].copy_from_slice(&0x6000_0000u32.to_be_bytes());
            frame[18..20].copy_from_slice(&(icmp_len as u16).to_be_bytes());
            frame[20] = 58;
            frame[21] = 255;
            frame[22..38].copy_from_slice(&source.0);
            frame[38..54].copy_from_slice(&destination.0);
            frame[54] = 135;
            frame[62..78].copy_from_slice(&target.0);
            if include_source {
                frame[78] = 1;
                frame[79] = 1;
                frame[80..86].copy_from_slice(&interface.mac_address);
            }
            let chain = PacketChain::from_owned(frame.clone());
            let checksum = net::pipeline::transport_checksum(
                &chain,
                54,
                icmp_len,
                IpAddr::V6(source),
                IpAddr::V6(destination),
                58,
            )
            .ok()?;
            frame[56..58].copy_from_slice(&checksum.to_be_bytes());
            Some(frame)
        }
    }
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub(crate) fn dhcp_rebind_seconds(
    lease_seconds: u32,
    renew_seconds: u32,
    offered: Option<u32>,
) -> u32 {
    net::stack::dhcp_rebind_seconds(lease_seconds, renew_seconds, offered)
}

pub(crate) fn build_multicast_control_frame(
    interface_id: InterfaceId,
    group: IpAddr,
    joined: bool,
    config: &ConfigSnapshot,
) -> Option<Vec<u8>> {
    let interface = config
        .interfaces
        .iter()
        .find(|interface| interface.id == interface_id && interface.running)?;
    match group {
        IpAddr::V4(group) => {
            let source = config.addresses.iter().find_map(|entry| {
                (entry.interface == interface_id && entry.primary)
                    .then_some(entry.address)
                    .and_then(|address| match address {
                        IpAddr::V4(address) => Some(address),
                        IpAddr::V6(_) => None,
                    })
            })?;
            let destination = if joined {
                group
            } else {
                Ipv4Addr::new(224, 0, 0, 2)
            };
            let destination_value = destination.as_u32();
            let mut frame = alloc::vec![0; 14 + 24 + 8];
            frame[0..6].copy_from_slice(&[
                0x01,
                0x00,
                0x5e,
                ((destination_value >> 16) as u8) & 0x7f,
                (destination_value >> 8) as u8,
                destination_value as u8,
            ]);
            frame[6..12].copy_from_slice(&interface.mac_address);
            frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let ip = &mut frame[14..38];
            ip[0] = 0x46;
            ip[2..4].copy_from_slice(&32u16.to_be_bytes());
            ip[8] = 1;
            ip[9] = 2;
            ip[12..16].copy_from_slice(&source.0);
            ip[16..20].copy_from_slice(&destination.0);
            ip[20..24].copy_from_slice(&[0x94, 4, 0, 0]);
            let checksum = net::pipeline::checksum_bytes(ip);
            ip[10..12].copy_from_slice(&checksum.to_be_bytes());
            let igmp = &mut frame[38..46];
            igmp[0] = if joined { 0x16 } else { 0x17 };
            igmp[4..8].copy_from_slice(&group.0);
            let checksum = net::pipeline::checksum_bytes(igmp);
            igmp[2..4].copy_from_slice(&checksum.to_be_bytes());
            Some(frame)
        }
        IpAddr::V6(group) => {
            let source = config.addresses.iter().find_map(|entry| {
                (entry.interface == interface_id)
                    .then_some(entry.address)
                    .and_then(|address| match address {
                        IpAddr::V6(address)
                            if address.0[0] == 0xfe && address.0[1] & 0xc0 == 0x80 =>
                        {
                            Some(address)
                        }
                        _ => None,
                    })
            })?;
            let destination = if joined {
                group
            } else {
                Ipv6Addr([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2])
            };
            let mut frame = alloc::vec![0; 14 + 40 + 8 + 24];
            frame[0..6].copy_from_slice(&[
                0x33,
                0x33,
                destination.0[12],
                destination.0[13],
                destination.0[14],
                destination.0[15],
            ]);
            frame[6..12].copy_from_slice(&interface.mac_address);
            frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            frame[14..18].copy_from_slice(&0x6000_0000u32.to_be_bytes());
            frame[18..20].copy_from_slice(&32u16.to_be_bytes());
            frame[20] = 0;
            frame[21] = 1;
            frame[22..38].copy_from_slice(&source.0);
            frame[38..54].copy_from_slice(&destination.0);
            frame[54..62].copy_from_slice(&[58, 0, 5, 2, 0, 0, 1, 0]);
            frame[62] = if joined { 131 } else { 132 };
            frame[70..86].copy_from_slice(&group.0);
            let chain = PacketChain::from_owned(frame.clone());
            let checksum = net::pipeline::transport_checksum(
                &chain,
                62,
                24,
                IpAddr::V6(source),
                IpAddr::V6(destination),
                58,
            )
            .ok()?;
            frame[64..66].copy_from_slice(&checksum.to_be_bytes());
            Some(frame)
        }
    }
}

fn map_bind_error(error: BindError) -> SocketError {
    match error {
        BindError::AddressInUse => SocketError::AddressInUse,
        BindError::NoPorts => SocketError::AddressUnavailable,
        BindError::InvalidAddress | BindError::UnknownReservation => SocketError::InvalidState,
    }
}

fn map_udp_bind_error(error: net::transport::UdpBindError) -> SocketError {
    match error {
        net::transport::UdpBindError::AddressInUse => SocketError::AddressInUse,
        net::transport::UdpBindError::InvalidEndpoint => SocketError::AddressUnavailable,
        net::transport::UdpBindError::FlowTableFull => SocketError::RuntimeBusy,
    }
}

fn map_tcp_bind_error(error: net::transport::TcpBindError) -> SocketError {
    match error {
        net::transport::TcpBindError::Duplicate => SocketError::AddressInUse,
        net::transport::TcpBindError::Full => SocketError::RuntimeBusy,
        net::transport::TcpBindError::InvalidEndpoint => SocketError::AddressUnavailable,
        net::transport::TcpBindError::NotListener => SocketError::InvalidState,
    }
}

impl WorkerContext {
    fn run_turn(&mut self) -> WorkerTurn {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::NetWorkerTurn);
        if self.control.remove_requested.load(Ordering::Acquire)
            && self.control.remove_ready.load(Ordering::Acquire)
        {
            return WorkerTurn::Removed;
        }
        self.rx_pool.as_mut().unwrap().drain_remote();
        self.tx_header_pool.as_mut().unwrap().drain_remote();
        self.tx_payload_pool.as_ref().unwrap().lock().drain_remote();
        self.refill_rx();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.prepare_arp_probe();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.prepare_udp_probe();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.prepare_physical_udp_probe();
        self.drain_egress();
        self.reclaim_tx();
        let turn_start = sched::now_ns_direct();
        let mut packet_budget = 128u16;
        let mut byte_budget = 256 * 1024u32;
        while packet_budget != 0
            && byte_budget != 0
            && sched::now_ns_direct().saturating_sub(turn_start) < 200_000
        {
            let result = self.poll_rx_once(RxBudget {
                packets: packet_budget.min(32),
                bytes: byte_budget,
            });
            packet_budget = packet_budget.saturating_sub(result.packets);
            byte_budget = byte_budget.saturating_sub(result.bytes);
            if result.fatal.is_some() || result.ring_empty || result.packets == 0 {
                break;
            }
        }
        if packet_budget == 0 {
            self.stats.budget_packet.fetch_add(1, Ordering::Relaxed);
        }
        if byte_budget == 0 {
            self.stats.budget_byte.fetch_add(1, Ordering::Relaxed);
        }
        if sched::now_ns_direct().saturating_sub(turn_start) >= 200_000 {
            self.stats.budget_time.fetch_add(1, Ordering::Relaxed);
        }
        self.drain_egress();
        let submitted = self.submit_tx();
        if submitted && self.queue.as_ref().unwrap().tx_produces_rx_synchronously() {
            self.reclaim_tx();
            self.poll_rx_once(RxBudget {
                packets: 32,
                bytes: 256 * 1024,
            });
        }
        self.rx_pool.as_mut().unwrap().drain_remote();
        self.tx_header_pool.as_mut().unwrap().drain_remote();
        self.tx_payload_pool.as_ref().unwrap().lock().drain_remote();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        {
            let pool = self.tx_payload_pool.as_ref().unwrap().lock();
            let conserved =
                pool.pool().outstanding() == 0 && pool.available() == pool.pool().capacity();
            if ARP_TX_COMPLETED.load(Ordering::Acquire) && conserved {
                ARP_POOL_CONSERVED.store(true, Ordering::Release);
            }
            if self.arp_probe_enabled
                && PHYSICAL_UDP_TX_SUBMITTED.load(Ordering::Acquire)
                && conserved
            {
                PHYSICAL_UDP_POOL_CONSERVED.store(true, Ordering::Release);
            }
        }
        let pool_stats = self.rx_pool.as_ref().unwrap().pool().stats();
        self.stats
            .pool_local_recycle
            .store(pool_stats.local_recycle, Ordering::Relaxed);
        self.stats
            .pool_remote_recycle
            .store(pool_stats.remote_recycle, Ordering::Relaxed);
        self.refill_rx();

        if self.has_pending_work() {
            WorkerTurn::Pending
        } else {
            WorkerTurn::Idle
        }
    }

    fn has_pending_work(&mut self) -> bool {
        self.queue.as_mut().unwrap().has_pending_work()
            || self.egress.has_pending()
            || !self.tx_batch.is_empty()
            || !self.retry_egress.is_empty()
            || !self.pending_tx_frames.is_empty()
            || !self.pending_rx_batches.is_empty()
            || self.has_test_work()
    }

    fn enqueue_rx_batch(&mut self) {
        if self.rx_batch.is_empty() {
            return;
        }
        let batch = core::mem::replace(
            &mut self.rx_batch,
            self.spare_rx_batches.pop().unwrap_or_default(),
        );
        self.pending_rx_batches.push_back(batch);
    }

    fn poll_rx_once(&mut self, budget: RxBudget) -> net::queue::RxPollResult {
        self.rx_batch.clear();
        let result = self
            .queue
            .as_mut()
            .unwrap()
            .poll_rx_batch(budget, &mut self.rx_batch);
        self.complete_rx_metadata();
        self.record_rx_result(&result);
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        for index in 0..self.rx_batch.len() {
            if let Some(packet) = self.rx_batch.packet(index) {
                self.observe_arp_reply(packet);
            }
        }
        self.enqueue_rx_batch();
        result
    }

    fn drain_egress(&mut self) {
        self.drain_pending_tx_frames();
        if !self.pending_tx_frames.is_empty() {
            let _ = self.egress.finish_drain();
            return;
        }
        while self.tx_batch.len() < 32 {
            let Some(work) = self.retry_egress.pop_front() else {
                break;
            };
            if let Err(work) = self.materialize_egress(work) {
                self.retry_egress.push_front(work);
                break;
            }
        }
        if !self.retry_egress.is_empty() {
            self.drain_pending_tx_frames();
            let _ = self.egress.finish_drain();
            return;
        }
        'rounds: while self.tx_batch.len() < 32 {
            let mut progressed = false;
            for (class, quantum) in [(3, 8usize), (2, 4), (1, 2), (0, 1)] {
                for _ in 0..quantum {
                    if self.tx_batch.len() >= 32 {
                        break 'rounds;
                    }
                    let Some(work) = self.egress.try_pop_class(class) else {
                        break;
                    };
                    progressed = true;
                    if let Err(work) = self.materialize_egress(work) {
                        self.retry_egress.push_back(work);
                        break 'rounds;
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        self.drain_pending_tx_frames();
        let _ = self.egress.finish_drain();
    }

    fn materialize_egress(&mut self, work: EgressWork) -> Result<(), EgressWork> {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::NetTxMaterialize).packets(1);
        match work {
            EgressWork::Packet(packet) => {
                self.tx_batch
                    .push(packet)
                    .unwrap_or_else(|_| unreachable!());
                Ok(())
            }
            EgressWork::Plan(plan) => self.materialize_tx_plan(plan).map_err(EgressWork::Plan),
            EgressWork::ControlFrame(bytes) => {
                self.pending_tx_frames.push_back(PendingTxFrame::Bytes {
                    bytes,
                    completion: net::buf::CompletionToken(0),
                    facade: None,
                });
                Ok(())
            }
        }
    }

    fn materialize_tx_plan(&mut self, plan: TxPlan) -> Result<(), TxPlan> {
        let payload_offset = plan.payload_offset as usize;
        let payload_len = plan.payload_len as usize;
        let max_payload_fragments = self.max_payload_fragments();
        let mut direct = plan
            .payload
            .packet_chain()
            .ok()
            .flatten()
            .and_then(|chain| chain.pin_shared_range(payload_offset, payload_len).ok());
        if direct.as_ref().is_some_and(|chain| {
            chain.fragment_count() > max_payload_fragments
                || (0..chain.fragment_count()).any(|index| {
                    chain
                        .fragment(index)
                        .and_then(|fragment| fragment.dma_addr().ok().flatten())
                        .is_none()
                })
        }) {
            direct = None;
        }

        if !self.queue.as_ref().unwrap().caps().scatter_gather {
            let frame_len = usize::from(plan.header_len).saturating_add(payload_len);
            let Ok(frame_len) = u16::try_from(frame_len) else {
                plan.facade.set_pending_error(SocketError::MessageTooLarge);
                return Ok(());
            };
            let mut pool = self.tx_payload_pool.as_ref().unwrap().lock();
            if frame_len > pool.buffer_capacity() {
                plan.facade.set_pending_error(SocketError::MessageTooLarge);
                return Ok(());
            }
            let Ok(mut lease) = pool.lease(0, frame_len, PacketMetadata::default()) else {
                return Err(plan);
            };
            let bytes = lease.as_mut_slice().expect("TX plan 连续 lease 范围有效");
            let header_len = usize::from(plan.header_len);
            bytes[..header_len].copy_from_slice(plan.header_bytes());
            if plan
                .payload
                .copy_range(payload_offset, &mut bytes[header_len..])
                .is_err()
            {
                plan.facade.set_pending_error(SocketError::Buffer);
                return Ok(());
            }
            drop(pool);
            self.tx_batch
                .push(TxPacket {
                    chain: PacketChain::from_lease(lease),
                    completion: plan.completion,
                    low_latency: plan.low_latency,
                    checksum: plan.checksum,
                    layout: plan.layout,
                })
                .unwrap_or_else(|_| unreachable!());
            return Ok(());
        }

        let payload = if let Some(payload) = direct {
            payload
        } else {
            match allocate_payload_chain(
                &mut *self.tx_payload_pool.as_ref().unwrap().lock(),
                payload_len,
                0,
                max_payload_fragments,
                |offset, output| plan.payload.copy_range(payload_offset + offset, output),
            ) {
                Ok(payload) => payload,
                Err(PayloadChainError::Retry) => return Err(plan),
                Err(PayloadChainError::Socket(error)) => {
                    plan.facade.set_pending_error(error);
                    return Ok(());
                }
            }
        };
        let Ok(header_len) = u16::try_from(plan.header_bytes().len()) else {
            plan.facade.set_pending_error(SocketError::MessageTooLarge);
            return Ok(());
        };
        let mut pool = self.tx_payload_pool.as_ref().unwrap().lock();
        let Ok(mut header) = pool.lease(0, header_len, PacketMetadata::default()) else {
            return Err(plan);
        };
        header
            .as_mut_slice()
            .expect("TX plan header lease 范围有效")
            .copy_from_slice(plan.header_bytes());
        drop(pool);
        let mut chain = PacketChain::from_lease(header);
        let mut payload = payload;
        let fragments = payload.fragment_count();
        for index in 0..fragments {
            let fragment = payload
                .take_fragment(index)
                .expect("TX plan payload fragment 索引有效");
            chain.push(fragment).unwrap_or_else(|_| unreachable!());
        }
        self.tx_batch
            .push(TxPacket {
                chain,
                completion: plan.completion,
                low_latency: plan.low_latency,
                checksum: plan.checksum,
                layout: plan.layout,
            })
            .unwrap_or_else(|_| unreachable!());
        Ok(())
    }

    fn max_payload_fragments(&self) -> usize {
        let caps = self.queue.as_ref().unwrap().caps();
        if caps.scatter_gather {
            usize::from(caps.max_tx_descriptors)
                .saturating_sub(1)
                .max(1)
        } else {
            1
        }
    }

    fn drain_pending_tx_frames(&mut self) {
        while self.tx_batch.len() < 32 {
            let Some(frame) = self.pending_tx_frames.pop_front() else {
                break;
            };
            let PendingTxFrame::Bytes {
                bytes,
                completion,
                facade,
            } = frame;
            let Ok(len) = u16::try_from(bytes.len()) else {
                if let Some(facade) = facade {
                    facade.set_pending_error(SocketError::MessageTooLarge);
                }
                continue;
            };
            let Ok(mut lease) = self.tx_payload_pool.as_ref().unwrap().lock().lease(
                0,
                len,
                PacketMetadata::default(),
            ) else {
                self.pending_tx_frames.push_front(PendingTxFrame::Bytes {
                    bytes,
                    completion,
                    facade,
                });
                break;
            };
            lease
                .as_mut_slice()
                .expect("待发送 frame lease 范围有效")
                .copy_from_slice(&bytes);
            self.tx_batch
                .push(TxPacket {
                    chain: PacketChain::from_lease(lease),
                    completion,
                    low_latency: false,
                    checksum: net::buf::TxChecksum::Complete,
                    layout: net::buf::PacketLayout::Plain,
                })
                .unwrap_or_else(|_| unreachable!());
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn recycle_chain(
        &mut self,
        mut packet: PacketChain,
        mut metadata: PacketMetadata,
        reason: DropReason,
    ) {
        metadata.drop_reason = reason;
        let fragments = packet.fragment_count();
        for fragment_index in 0..fragments {
            match packet.take_fragment(fragment_index) {
                Some(PacketFragment::Exclusive(mut lease)) => {
                    *lease.metadata_mut() = metadata;
                    let _ = self.rx_pool.as_mut().unwrap().recycle_local_or_defer(lease);
                }
                Some(PacketFragment::Shared(chunk)) => drop(chunk),
                Some(PacketFragment::Owned(bytes)) => drop(bytes),
                None => {}
            }
        }
    }

    fn refill_rx(&mut self) {
        let target = self.queue.as_ref().unwrap().caps().queue_size as usize;
        loop {
            while self.rx_pool.as_ref().unwrap().pool().outstanding() < target
                && self.refill_batch.len() < 32
            {
                match self.rx_pool.as_mut().unwrap().lease(
                    116,
                    (4096 - 116) as u16,
                    PacketMetadata::default(),
                ) {
                    Ok(lease) => self
                        .refill_batch
                        .push(lease)
                        .unwrap_or_else(|_| unreachable!()),
                    Err(_) => break,
                }
            }
            if self.refill_batch.is_empty() {
                break;
            }
            let result = self
                .queue
                .as_mut()
                .unwrap()
                .refill_rx_batch(&mut self.refill_batch);
            let len = self.refill_batch.len();
            for index in 0..len {
                if let Some(lease) = self.refill_batch.take(index) {
                    let _ = self.rx_pool.as_mut().unwrap().recycle_local(lease);
                }
            }
            if result.posted == 0 || result.fatal.is_some() {
                break;
            }
        }
    }

    fn complete_rx_metadata(&mut self) {
        let timestamp = sched::now_ns_direct();
        let pressure = if self.local_mac == [0; 6] {
            RxPoolPressure::Unmanaged
        } else {
            let available = self.rx_pool.as_ref().unwrap().available();
            let queue_size = self.queue.as_ref().unwrap().caps().queue_size as usize;
            if available < net::tuning::RX_POOL_EMERGENCY_RESERVE {
                RxPoolPressure::Emergency
            } else if available < queue_size / 2 {
                RxPoolPressure::Low
            } else {
                RxPoolPressure::Normal
            }
        };
        for index in 0..self.rx_batch.len() {
            if let Some(metadata) = self.rx_batch.metadata_mut(index) {
                metadata.ingress_device = self.ingress_device;
                metadata.rx_timestamp_ns = timestamp;
                metadata.rss_generation = self.rss_generation;
                metadata.rx_pool_pressure = pressure;
            }
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn prepare_arp_probe(&mut self) {
        if !self.arp_probe_enabled
            || self.arp_probe_done
            || !ARP_PROBE_REQUESTED.load(Ordering::Acquire)
            || ARP_PROBE_SENT.load(Ordering::Acquire)
        {
            return;
        }
        let Ok(mut lease) =
            self.tx_payload_pool
                .as_ref()
                .unwrap()
                .lock()
                .lease(64, 42, PacketMetadata::default())
        else {
            return;
        };
        let mac = net::device::snapshot_devices()
            .into_iter()
            .find(|device| device.name.as_ref() != "lo")
            .map(|device| device.mac_address)
            .unwrap_or([0; 6]);
        let frame = lease.as_mut_slice().expect("ARP probe lease 范围有效");
        frame.fill(0);
        frame[0..6].fill(0xff);
        frame[6..12].copy_from_slice(&mac);
        frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
        frame[14..16].copy_from_slice(&1u16.to_be_bytes());
        frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[18] = 6;
        frame[19] = 4;
        frame[20..22].copy_from_slice(&1u16.to_be_bytes());
        frame[22..28].copy_from_slice(&mac);
        frame[28..32].copy_from_slice(&[10, 0, 2, 15]);
        frame[32..38].fill(0);
        frame[38..42].copy_from_slice(&[10, 0, 2, 3]);
        assert!(
            self.tx_batch
                .push(TxPacket {
                    chain: PacketChain::from_lease(lease),
                    completion: CompletionToken(0x4152_5001),
                    low_latency: true,
                    checksum: net::buf::TxChecksum::Complete,
                    layout: net::buf::PacketLayout::Plain,
                })
                .is_ok()
        );
        self.arp_probe_done = true;
        ARP_PROBE_SENT.store(true, Ordering::Release);
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn prepare_udp_probe(&mut self) {
        if self.local_mac != [0; 6]
            || self.udp_probe_queued
            || !UDP_PROBE_REQUESTED.load(Ordering::Acquire)
        {
            return;
        }
        let Ok(mut lease) =
            self.tx_payload_pool
                .as_ref()
                .unwrap()
                .lock()
                .lease(128, 4, PacketMetadata::default())
        else {
            return;
        };
        lease
            .as_mut_slice()
            .expect("UDP probe payload 范围有效")
            .copy_from_slice(b"ping");
        let work = IngressWork::UdpProbe {
            egress: self.egress_index,
            payload: PacketChain::from_lease(lease),
        };
        let coordinator = self.protocol_cluster.coordinator();
        let result = if coordinator.id == self.owner_shard {
            self.local_ingress.push_back(work);
            Ok(())
        } else {
            coordinator.try_push(work)
        };
        match result {
            Ok(()) => self.udp_probe_queued = true,
            Err(IngressWork::UdpProbe { payload, .. }) => {
                self.recycle_chain(
                    payload,
                    PacketMetadata::default(),
                    DropReason::IngressRingFull,
                );
            }
            Err(IngressWork::Packet(_)) => unreachable!(),
            Err(IngressWork::LocalTcp { .. } | IngressWork::LocalUdp { .. }) => unreachable!(),
            Err(IngressWork::PhysicalUdpProbe { .. }) => unreachable!(),
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn prepare_physical_udp_probe(&mut self) {
        if !self.arp_probe_enabled
            || self.physical_udp_probe_queued
            || !PHYSICAL_UDP_PROBE_REQUESTED.load(Ordering::Acquire)
            || !ARP_REPLY_SEEN.load(Ordering::Acquire)
        {
            return;
        }
        const DNS_QUERY: [u8; 29] = [
            0x4d, 0x47, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'e',
            b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00, 0x01, 0x00,
            0x01,
        ];
        let Ok(mut lease) = self.tx_payload_pool.as_ref().unwrap().lock().lease(
            128,
            DNS_QUERY.len() as u16,
            PacketMetadata::default(),
        ) else {
            return;
        };
        lease
            .as_mut_slice()
            .expect("DNS probe payload 范围有效")
            .copy_from_slice(&DNS_QUERY);
        let work = IngressWork::PhysicalUdpProbe {
            egress: self.egress_index,
            payload: PacketChain::from_lease(lease),
        };
        let coordinator = self.protocol_cluster.coordinator();
        let result = if coordinator.id == self.owner_shard {
            self.local_ingress.push_back(work);
            Ok(())
        } else {
            coordinator.try_push(work)
        };
        match result {
            Ok(()) => self.physical_udp_probe_queued = true,
            Err(IngressWork::PhysicalUdpProbe { payload, .. }) => {
                self.recycle_chain(
                    payload,
                    PacketMetadata::default(),
                    DropReason::IngressRingFull,
                );
            }
            Err(IngressWork::Packet(_) | IngressWork::UdpProbe { .. }) => unreachable!(),
            Err(IngressWork::LocalTcp { .. } | IngressWork::LocalUdp { .. }) => unreachable!(),
        }
    }

    fn reclaim_tx(&mut self) {
        self.completion_batch.clear();
        let _ = self
            .queue
            .as_mut()
            .unwrap()
            .reclaim_tx_batch(&mut self.completion_batch);
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        for index in 0..self.completion_batch.len() {
            if self.completion_batch.token(index) == Some(CompletionToken(0x4152_5001)) {
                ARP_TX_COMPLETED.store(true, Ordering::Release);
            }
        }
    }

    fn submit_tx(&mut self) -> bool {
        if self.tx_batch.is_empty() {
            return false;
        }
        let original_len = self.tx_batch.len();
        let result = self
            .queue
            .as_mut()
            .unwrap()
            .submit_tx_batch(&mut self.tx_batch, self.tx_header_pool.as_mut().unwrap());
        self.stats
            .tx_packets
            .fetch_add(u64::from(result.packets), Ordering::Relaxed);
        self.stats
            .tx_bytes
            .fetch_add(u64::from(result.bytes), Ordering::Relaxed);
        self.stats
            .doorbell
            .fetch_add(u64::from(result.packets != 0), Ordering::Relaxed);
        if result.fatal.is_some() {
            self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            self.stats.tx_dropped.fetch_add(
                original_len.saturating_sub(usize::from(result.packets)) as u64,
                Ordering::Relaxed,
            );
            for index in 0..self.tx_batch.len() {
                let _ = self.tx_batch.take(index);
            }
        }
        if result.queue_full && self.tx_batch.len() == original_len {
            let _ = sched::operation::sched_yield();
        }
        result.packets != 0 && result.fatal.is_none()
    }

    /// detach 前停止 IRQ、排空可观察 completion，并在释放 queue 前回收所有 batch lease。
    fn finish_removal(&mut self) {
        let _ = self.irq.ack_and_mask();
        self.irq.clear_waker();
        *self.egress.task.lock() = None;
        self.egress.deactivate();
        self.protocol_cluster
            .remove_egress(self.egress_index, &self.egress);
        while let Some(frame) = self.pending_tx_frames.pop_front() {
            match frame {
                PendingTxFrame::Bytes {
                    facade: Some(facade),
                    ..
                } => {
                    facade.set_pending_error(SocketError::NetworkUnreachable);
                }
                PendingTxFrame::Bytes { facade: None, .. } => {}
            }
        }
        while let Some(work) = self.retry_egress.pop_front() {
            fail_egress_work(work, SocketError::NetworkUnreachable);
        }
        // ELM quiesce 已撤销 queue endpoint。此处调用 pinned export 会报告生命周期拒绝，
        // 并隔离原本可以干净卸载的 cell；新数据面调用被阻断后，释放下方的本地 batch
        // 和 queue 已足以完成清理。
        for index in 0..self.tx_batch.len() {
            let _ = self.tx_batch.take(index);
        }
        for index in 0..self.refill_batch.len() {
            if let Some(lease) = self.refill_batch.take(index) {
                let _ = self.rx_pool.as_mut().unwrap().recycle_local(lease);
            }
        }
        self.rx_batch.clear();
        self.pending_rx_batches.clear();
        self.spare_rx_batches.clear();
        self.completion_batch.clear();
        self.rx_pool.as_mut().unwrap().begin_dying();
        self.tx_payload_pool.as_ref().unwrap().lock().begin_dying();
        self.tx_header_pool.as_mut().unwrap().begin_dying();
        drop(self.queue.take());
        self.rx_pool.as_mut().unwrap().drain_remote();
        self.tx_payload_pool.as_ref().unwrap().lock().drain_remote();
        self.tx_header_pool.as_mut().unwrap().drain_remote();
        drop(self.rx_pool.take());
        drop(self.tx_payload_pool.take());
        drop(self.tx_header_pool.take());
        let completed = self.control.completed.fetch_add(1, Ordering::AcqRel) + 1;
        if completed >= self.control.worker_count.load(Ordering::Acquire) {
            self.control.done.store(true, Ordering::Release);
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn observe_arp_reply(&self, packet: &PacketChain) {
        if !ARP_PROBE_SENT.load(Ordering::Acquire) || ARP_REPLY_SEEN.load(Ordering::Acquire) {
            return;
        }
        let Some(PacketFragment::Exclusive(lease)) = packet.fragment(0) else {
            return;
        };
        let Ok(frame) = lease.as_slice() else {
            return;
        };
        if frame.len() < 42
            || frame[12..14] != 0x0806u16.to_be_bytes()
            || frame[20..22] != 2u16.to_be_bytes()
            || frame[28..32] != [10, 0, 2, 3]
            || frame[38..42] != [10, 0, 2, 15]
        {
            return;
        }
        ARP_REPLY_SEEN.store(true, Ordering::Release);
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn has_test_work(&self) -> bool {
        (self.arp_probe_enabled
            && !self.arp_probe_done
            && ARP_PROBE_REQUESTED.load(Ordering::Acquire)
            && !ARP_PROBE_SENT.load(Ordering::Acquire))
            || (self.local_mac == [0; 6]
                && !self.udp_probe_queued
                && UDP_PROBE_REQUESTED.load(Ordering::Acquire))
            || (self.arp_probe_enabled
                && !self.physical_udp_probe_queued
                && PHYSICAL_UDP_PROBE_REQUESTED.load(Ordering::Acquire)
                && ARP_REPLY_SEEN.load(Ordering::Acquire))
    }

    #[cfg(not(any(feature = "kernel-tests", feature = "network-tests")))]
    const fn has_test_work(&self) -> bool {
        false
    }

    fn record_rx_result(&self, result: &net::queue::RxPollResult) {
        self.stats.poll_total.fetch_add(1, Ordering::Relaxed);
        self.stats
            .rx_packets
            .fetch_add(u64::from(result.packets), Ordering::Relaxed);
        self.stats
            .rx_bytes
            .fetch_add(u64::from(result.bytes), Ordering::Relaxed);
        match result.packets {
            1..=8 => &self.stats.rx_batch_1_8,
            9..=16 => &self.stats.rx_batch_9_16,
            17..=31 => &self.stats.rx_batch_17_31,
            32 => &self.stats.rx_batch_32,
            _ => return self.record_rx_flags(result),
        }
        .fetch_add(1, Ordering::Relaxed);
        self.record_rx_flags(result);
    }

    fn record_rx_flags(&self, result: &net::queue::RxPollResult) {
        if result.descriptor_starved {
            self.stats
                .descriptor_starved
                .fetch_add(1, Ordering::Relaxed);
        }
        match result.fatal {
            Some(net::queue::QueueFatalError::DeviceGone) => &self.stats.fatal_device_gone,
            Some(net::queue::QueueFatalError::DeviceReset) => &self.stats.fatal_device_reset,
            Some(net::queue::QueueFatalError::DmaFault) => &self.stats.fatal_dma_fault,
            Some(net::queue::QueueFatalError::RingCorrupt) => &self.stats.fatal_ring_corrupt,
            None => return,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
    }
}
