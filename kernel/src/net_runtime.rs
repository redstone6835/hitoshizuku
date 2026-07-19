//! 网络设备接管与 NetWorker 运行时。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

use errno::Errno;
use general::mm::{copy_from_user, copy_to_user};
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
use net::buf::CompletionToken;
use net::buf::{
    CompletionBatch, DropReason, NetBufPoolOwner, PacketBatch, PacketChain, PacketFragment,
    PacketMetadata, RxRefillBatch, TxBatch, TxPacket,
};
use net::control::{
    AddressEntry, BindAddress, BindError, BindOptions, BindRegistry, BindRequest, BindToken,
    ConfigSnapshot, ConfigStore, InterfaceSnapshot, RouteEntry,
};
use net::device::{
    NET_QUEUE_CALL_RUST_ABI, NET_QUEUE_CALL_STATUS_OK, NET_QUEUE_OP_HAS_PENDING,
    NET_QUEUE_OP_POLL_RX, NET_QUEUE_OP_QUIESCE, NET_QUEUE_OP_RECLAIM_TX, NET_QUEUE_OP_REFILL_RX,
    NET_QUEUE_OP_SUBMIT_TX, NetDeviceHandle, NetDeviceRegisterError, NetDeviceRegisterErrorKind,
    NetDeviceRegistrar, NetDeviceRegistration, NetDeviceRemoveError, NetDeviceSnapshot,
    NetDeviceStats, NetDeviceTeardown, NetQueueCallV1, NetQueueEndpoint, NetQueueRegistration,
    NetStat, QueueIrqControl, QueueWakeHandle,
};
use net::flow::FlowKey;
use net::pipeline::{FrontendBatch, FrontendDisposition, FrontendPacket, VectorFrontend};
use net::queue::{NetQueuePair, RxBudget};
use net::ring::BoundedMpsc;
use net::transport::{
    LocalUdpIngressError, PreparedRawTx, PreparedTcpTx, PreparedUdpTx, RawTxError, TcpPacket,
    TcpPath, build_header_included_ipv4_fragments, build_raw_packet, build_tcp_packet,
    build_udp_fragments, build_udp_packet_with_options,
};
use net::{
    AddressFamily, Endpoint, FlowId, FlowShard, FlowTurnContext, InterfaceId, IpAddr, Ipv4Addr,
    Ipv6Addr, ListenGroup, ListenGroupId, OwnerRef, ShardId, SocketCommand, SocketError,
    SocketFacade, SocketKind, SocketRuntime, TransportProtocol,
};
use sched::sync::Spinlock;

static DEVICES: Spinlock<Vec<DeviceRecord>> = Spinlock::new(Vec::new());
static CONFIG_STORE: Spinlock<Option<Arc<ConfigStore>>> = Spinlock::new(None);
static NET_IOCTL_LOCK: Spinlock<()> = Spinlock::new(());
static WORKER_STARTS: Spinlock<Vec<Option<Box<WorkerContext>>>> = Spinlock::new(Vec::new());
static PROTOCOL_STARTS: Spinlock<Vec<Option<Box<ProtocolContext>>>> = Spinlock::new(Vec::new());
static PROTOCOL_CLUSTER: Spinlock<Option<Arc<ProtocolCluster>>> = Spinlock::new(None);
static NET_RUNTIME_STARTED: AtomicBool = AtomicBool::new(false);
static NET_ATTACH_LOCK: Spinlock<()> = Spinlock::new(());
static PINNED_QUEUE_FAILURES: AtomicU64 = AtomicU64::new(0);
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static WORKER_TASKS: Spinlock<Vec<Arc<sched::Task>>> = Spinlock::new(Vec::new());
static REGISTRAR: KernelNetRegistrar = KernelNetRegistrar;
static SOCKET_RUNTIME_ADAPTER: KernelSocketRuntime = KernelSocketRuntime;
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
        cluster.coordinator().wake_owner();
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
        cluster.coordinator().wake_owner();
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

    fn invoke(&self, frame: &mut NetQueueCallV1) -> bool {
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
        let deadline = sched::now_ns_public().saturating_add(2_000_000);
        let result =
            crate::elm::invoke_pinned_native(&self.call, frame, &ranges[..range_count], deadline);
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
        let mut frame = NetQueueCallV1::new(NET_QUEUE_OP_REFILL_RX, self.id);
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
        let mut frame = NetQueueCallV1::new(NET_QUEUE_OP_POLL_RX, self.id);
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
        let mut frame = NetQueueCallV1::new(NET_QUEUE_OP_RECLAIM_TX, self.id);
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
        let mut frame = NetQueueCallV1::new(NET_QUEUE_OP_SUBMIT_TX, self.id);
        frame.tx_batch = batch;
        frame.tx_header_pool = header_pool;
        if !self.invoke(&mut frame) || usize::from(frame.tx_submit_result.packets) > original_len {
            return Self::fatal_tx_submit();
        }
        frame.tx_submit_result
    }

    fn has_pending_work(&mut self) -> bool {
        let mut frame = NetQueueCallV1::new(NET_QUEUE_OP_HAS_PENDING, self.id);
        self.invoke(&mut frame) && frame.pending
    }

    fn quiesce(&mut self) -> Result<(), net::queue::QueueFatalError> {
        let mut frame = NetQueueCallV1::new(NET_QUEUE_OP_QUIESCE, self.id);
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
        let invalidation_deadline = sched::now_ns_public().saturating_add(5_000_000_000);
        while !invalidation.done() && sched::now_ns_public() < invalidation_deadline {
            let _ = sched::operation::sched_yield();
        }
        if !invalidation.done() {
            return Err(NetDeviceRemoveError::Busy);
        }
        control.remove_ready.store(true, Ordering::Release);
        for task in control.tasks.lock().iter() {
            let _ = sched::activate_task(task);
        }
        let deadline = sched::now_ns_public().saturating_add(5_000_000_000);
        while !control.done.load(Ordering::Acquire) && sched::now_ns_public() < deadline {
            let _ = sched::operation::sched_yield();
        }
        if !control.done.load(Ordering::Acquire) {
            return Err(NetDeviceRemoveError::Busy);
        }
        let task_exit_deadline = sched::now_ns_public().saturating_add(5_000_000_000);
        while control.tasks.lock().iter().any(worker_task_still_live)
            && sched::now_ns_public() < task_exit_deadline
        {
            let _ = sched::operation::sched_yield();
        }
        if control.tasks.lock().iter().any(worker_task_still_live) {
            return Err(NetDeviceRemoveError::Busy);
        }
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        {
            let tasks = control.tasks.lock();
            let mut worker_tasks = WORKER_TASKS.lock();
            worker_tasks
                .retain(|candidate| !tasks.iter().any(|owned| Arc::ptr_eq(candidate, owned)));
            worker_tasks.shrink_to_fit();
        }
        // 内核线程仍在自己的内核栈上执行时，不能释放自身的 Task 和内核栈。因此调度器
        // 会在 worker 所在 CPU 上退役最后一个 Task Arc，并在下一个调度边界释放它。
        // 在 ELM 检查该代际的分配账本前，必须显式推动调度越过这个边界。
        let task_reap_deadline = sched::now_ns_public().saturating_add(5_000_000_000);
        loop {
            let tasks = control.tasks.lock();
            if tasks.iter().all(|task| Arc::strong_count(task) == 1) {
                break;
            }
            if sched::now_ns_public() >= task_reap_deadline {
                for task in tasks.iter().filter(|task| Arc::strong_count(task) != 1) {
                    log::warning!(
                        "[net] worker task still retained pid={:?} cpu={} refs={}",
                        task.pid_root(),
                        task.current_cpu(),
                        Arc::strong_count(task),
                    );
                }
                return Err(NetDeviceRemoveError::Busy);
            }
            for task in tasks.iter() {
                sched::request_resched(task.current_cpu());
            }
            drop(tasks);
            let _ = sched::operation::sched_yield();
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

fn worker_task_still_live(task: &Arc<sched::Task>) -> bool {
    task.state() != sched::TaskState::Dead
        || (0..sched::NR_CPUS).any(|cpu| {
            sched::current_task_on(cpu)
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, task))
        })
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
        egress: usize,
        interface: InterfaceId,
        work: PreparedTcpTx,
    },
    LocalUdp {
        egress: usize,
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
    Tcp(PreparedTcpTx),
    Udp(PreparedUdpTx),
    UdpBatch(Vec<PreparedUdpTx>),
    Raw(PreparedRawTx),
    ControlFrame(Vec<u8>),
}

impl EgressWork {
    fn priority_class(&self) -> usize {
        let priority = match self {
            Self::Packet(packet) => return usize::from(packet.low_latency) * 3,
            Self::Tcp(work) if work.low_latency => return 3,
            Self::Tcp(work) => work.facade.socket_priority(),
            Self::Udp(work) => work.payload.facade().socket_priority(),
            Self::UdpBatch(work) => work
                .first()
                .map(|work| work.payload.facade().socket_priority())
                .unwrap_or_default(),
            Self::Raw(work) => work.payload.facade().socket_priority(),
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

fn udp_batch_candidate(work: &PreparedUdpTx) -> bool {
    let ip_header_len = match work.route.source {
        IpAddr::V4(_) => 20usize,
        IpAddr::V6(_) => 40usize,
    };
    !work.destination.addr.is_multicast()
        && !matches!(work.destination.addr, IpAddr::V4(address) if address.is_broadcast())
        && ip_header_len + 8 + usize::from(work.payload.len) <= work.route.mtu as usize
        && work.payload.len != 0
}

fn udp_batch_compatible(first: &PreparedUdpTx, next: &PreparedUdpTx) -> bool {
    udp_batch_candidate(first)
        && udp_batch_candidate(next)
        && first.payload.len == next.payload.len
        && first.route == next.route
        && first.destination == next.destination
        && first.source_port == next.source_port
        && first.source_mac == next.source_mac
        && first.destination_mac == next.destination_mac
        && first.hop_limit == next.hop_limit
        && first.traffic_class == next.traffic_class
}

struct PendingTxFrame {
    bytes: Vec<u8>,
    completion: net::buf::CompletionToken,
    facade: Option<Arc<SocketFacade>>,
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

enum PendingNeighborTx {
    Tcp(PreparedTcpTx),
    Udp(PreparedUdpTx),
    Raw(PreparedRawTx),
}

impl PendingNeighborTx {
    fn key(&self) -> net::control::NeighborKey {
        match self {
            Self::Tcp(work) => work.path.unresolved_neighbor,
            Self::Udp(work) => work.unresolved_neighbor,
            Self::Raw(work) => work.unresolved_neighbor,
        }
        .expect("待解析发送必须携带 neighbor key")
    }

    fn facade(&self) -> Arc<SocketFacade> {
        match self {
            Self::Tcp(work) => Arc::clone(&work.facade),
            Self::Udp(work) => work.payload.facade(),
            Self::Raw(work) => work.payload.facade(),
        }
    }

    fn resolve(&mut self, mac_address: [u8; 6]) {
        match self {
            Self::Tcp(work) => {
                work.path.destination_mac = mac_address;
                work.path.unresolved_neighbor = None;
            }
            Self::Udp(work) => {
                work.destination_mac = mac_address;
                work.unresolved_neighbor = None;
            }
            Self::Raw(work) => {
                work.destination_mac = mac_address;
                work.unresolved_neighbor = None;
            }
        }
    }
}

struct PendingNeighbor {
    packets: VecDeque<PendingNeighborTx>,
    probes: u8,
    next_probe_ns: u64,
    expires_ns: u64,
}

struct DadState {
    interface: InterfaceId,
    address: Ipv6Addr,
    probe_sent: bool,
    conflict: bool,
    deadline_ns: u64,
}

#[derive(Clone)]
struct DhcpLease {
    address: Ipv4Addr,
    prefix_len: u8,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    lease_seconds: u32,
}

enum DhcpPhase {
    Discovering,
    Requesting {
        lease: DhcpLease,
        server: Ipv4Addr,
    },
    Bound {
        lease: DhcpLease,
        server: Ipv4Addr,
        renew_ns: u64,
        rebind_ns: u64,
        expires_ns: u64,
    },
}

struct DhcpClient {
    interface: InterfaceId,
    mac_address: [u8; 6],
    transaction_id: u32,
    phase: DhcpPhase,
    next_action_ns: u64,
    retry_seconds: u32,
    installed: Option<DhcpLease>,
}

struct DhcpReply {
    message_type: u8,
    transaction_id: u32,
    client_mac: [u8; 6],
    offered: Ipv4Addr,
    server: Option<Ipv4Addr>,
    subnet_mask: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    lease_seconds: Option<u32>,
    renewal_seconds: Option<u32>,
    rebinding_seconds: Option<u32>,
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
    InterfaceGone {
        interface: InterfaceId,
        ack: Arc<InterfaceGoneBarrier>,
    },
    ResolveNeighbor(PendingNeighborTx),
    NeighborObserved {
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

struct ControlPlane {
    bind_registry: BindRegistry,
    bindings: Spinlock<BTreeMap<net::SocketId, BindToken>>,
    listeners: Spinlock<BTreeMap<ListenGroupId, Arc<ListenGroup>>>,
    next_listener: AtomicU64,
    rss_key: [u8; 40],
    shard_count: usize,
    pending_neighbors_per_interface: Spinlock<BTreeMap<InterfaceId, usize>>,
    dad_errors: Spinlock<BTreeMap<InterfaceId, SocketError>>,
    multicast_refs: Spinlock<BTreeMap<(InterfaceId, IpAddr), usize>>,
    multicast_bindings: Spinlock<BTreeMap<(net::SocketId, net::MulticastMembership), InterfaceId>>,
}

impl ControlPlane {
    fn new(shard_count: usize, rss_key: [u8; 40], hash_seed: &[u8; 16]) -> Self {
        Self {
            bind_registry: BindRegistry::new(shard_count, hash_seed),
            bindings: Spinlock::new(BTreeMap::new()),
            listeners: Spinlock::new(BTreeMap::new()),
            next_listener: AtomicU64::new(1),
            rss_key,
            shard_count,
            pending_neighbors_per_interface: Spinlock::new(BTreeMap::new()),
            dad_errors: Spinlock::new(BTreeMap::new()),
            multicast_refs: Spinlock::new(BTreeMap::new()),
            multicast_bindings: Spinlock::new(BTreeMap::new()),
        }
    }

    fn allocate_listener_id(&self) -> ListenGroupId {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        assert!(id != 0, "ListenGroupId 已耗尽");
        ListenGroupId(id)
    }

    fn flow_shard(
        &self,
        remote: Endpoint,
        local: Endpoint,
        protocol: TransportProtocol,
    ) -> ShardId {
        let key = net::flow::FlowKey::new(remote, local, protocol)
            .expect("协议 flow 端点必须属于同一地址族");
        ShardId((net::flow::rss_hash(&self.rss_key, &key) as usize % self.shard_count) as u16)
    }

    fn neighbor_owner(&self, key: net::control::NeighborKey) -> ShardId {
        let mut hash = u64::from(key.interface.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let bytes: &[u8] = match &key.address {
            IpAddr::V4(address) => &address.0,
            IpAddr::V6(address) => &address.0,
        };
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
        }
        ShardId((hash as usize % self.shard_count) as u16)
    }

    fn reserve_neighbor_packet(&self, interface: InterfaceId) -> bool {
        let mut counts = self.pending_neighbors_per_interface.lock();
        let count = counts.entry(interface).or_default();
        if *count >= 256 {
            return false;
        }
        *count += 1;
        true
    }

    fn release_neighbor_packets(&self, interface: InterfaceId, count: usize) {
        let mut counts = self.pending_neighbors_per_interface.lock();
        let Some(current) = counts.get_mut(&interface) else {
            return;
        };
        *current = current.saturating_sub(count);
        if *current == 0 {
            counts.remove(&interface);
        }
    }

    fn remember_binding(&self, socket: net::SocketId, token: BindToken) {
        self.bindings.lock().insert(socket, token);
    }

    fn release_binding(&self, socket: net::SocketId) {
        if let Some(token) = self.bindings.lock().remove(&socket) {
            let _ = self.bind_registry.release(token);
        }
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
    control: Arc<ControlPlane>,
    cluster: Arc<ProtocolCluster>,
}

impl ListenerInstall {
    fn finish(&self, result: Result<(), SocketError>) {
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        if self.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        if self.failed.load(Ordering::Acquire) || self.facade.generation() != self.generation {
            self.group.close();
            for runtime in &self.cluster.shards {
                let mut work = ControlWork::DiscardListener {
                    group: self.group.id(),
                };
                loop {
                    match runtime.control.try_push(work) {
                        Ok(()) => {
                            if !runtime.pending.swap(true, Ordering::AcqRel) {
                                runtime.wake_owner();
                            }
                            break;
                        }
                        Err(pending) => {
                            work = pending;
                            runtime.wake_owner();
                            let _ = sched::operation::sched_yield();
                        }
                    }
                }
            }
            self.facade
                .complete_control(self.sequence, Err(SocketError::InvalidState));
            return;
        }
        self.control
            .listeners
            .lock()
            .insert(self.group.id(), Arc::clone(&self.group));
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
    control: Arc<ControlPlane>,
}

impl ListenerRemove {
    fn finish(&self) {
        if self.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        self.control.listeners.lock().remove(&self.group);
        self.control.release_binding(self.facade.id());
        self.facade.publish_closed();
    }
}

struct EgressChannel {
    interface: InterfaceId,
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
    fn new(interface: InterfaceId, stats: Arc<QueueRuntimeStats>) -> Self {
        Self {
            interface,
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
        EgressWork::Tcp(work) => work.facade.set_pending_error(error),
        EgressWork::Udp(work) => work.payload.facade().set_pending_error(error),
        EgressWork::UdpBatch(work) => {
            for work in work {
                work.payload.facade().set_pending_error(error);
            }
        }
        EgressWork::Raw(work) => work.payload.facade().set_pending_error(error),
    }
}

struct ProtocolRuntime {
    id: ShardId,
    cpu: usize,
    ingress: BoundedMpsc<IngressWork>,
    control: BoundedMpsc<ControlWork>,
    dirty: BoundedMpsc<Arc<SocketFacade>>,
    lifecycle: BoundedMpsc<Arc<SocketFacade>>,
    pending: AtomicBool,
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
            ingress: BoundedMpsc::new(1024),
            control: BoundedMpsc::new(256),
            dirty: BoundedMpsc::new(4096),
            lifecycle: BoundedMpsc::new(4096),
            pending: AtomicBool::new(false),
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
            let _ = sched::activate_task_with_cpu_hint(&task, self.cpu);
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
            if !self.pending.swap(true, Ordering::AcqRel) {
                self.wake_owner();
            }
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
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.wake_owner();
        }
    }

    fn finish_drain(&self) -> bool {
        self.pending.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        if !self.ingress.is_empty()
            || !self.control.is_empty()
            || !self.dirty.is_empty()
            || !self.lifecycle.is_empty()
            || self.timer_fired.load(Ordering::Acquire)
        {
            self.pending.store(true, Ordering::Release);
            true
        } else {
            false
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

    fn owner_target(&self, owner: OwnerRef) -> &Arc<ProtocolRuntime> {
        match owner {
            OwnerRef::Flow { shard, .. } => self.shard(shard).unwrap_or_else(|| self.coordinator()),
            OwnerRef::Unassigned
            | OwnerRef::Bound { .. }
            | OwnerRef::Listener { .. }
            | OwnerRef::Closed { .. } => self.coordinator(),
        }
    }

    fn publish_control(&self, target: ShardId, work: ControlWork) -> Result<(), ControlWork> {
        let Some(runtime) = self.shard(target) else {
            return Err(work);
        };
        runtime.control.try_push(work)?;
        if !runtime.pending.swap(true, Ordering::AcqRel) {
            runtime.wake_owner();
        }
        Ok(())
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
                        if !runtime.pending.swap(true, Ordering::AcqRel) {
                            runtime.wake_owner();
                        }
                        break;
                    }
                    Err(pending) => {
                        work = pending;
                        runtime.wake_owner();
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
            self.pending.store(true, Ordering::Release);
            self.wake_owner();
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
        if !runtime.pending.swap(true, Ordering::AcqRel) {
            runtime.wake_owner();
        }
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

    fn notify_tx(&self, facade: Arc<SocketFacade>) {
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

fn initial_dad_states(config: &ConfigSnapshot, now_ns: u64) -> Vec<DadState> {
    config
        .interfaces
        .iter()
        .filter(|interface| {
            !interface.loopback && interface.running && interface.mac_address != [0; 6]
        })
        .map(|interface| {
            let mac = interface.mac_address;
            let address = Ipv6Addr([
                0xfe,
                0x80,
                0,
                0,
                0,
                0,
                0,
                0,
                mac[0] ^ 0x02,
                mac[1],
                mac[2],
                0xff,
                0xfe,
                mac[3],
                mac[4],
                mac[5],
            ]);
            DadState {
                interface: interface.id,
                address,
                probe_sent: false,
                conflict: false,
                deadline_ns: now_ns.saturating_add(1_000_000_000),
            }
        })
        .collect()
}

fn initial_dhcp_clients(config: &ConfigSnapshot, now_ns: u64) -> Vec<DhcpClient> {
    config
        .interfaces
        .iter()
        .filter(|interface| {
            !interface.loopback
                && interface.running
                && interface.mac_address != [0; 6]
                && !config.addresses.iter().any(|entry| {
                    entry.interface == interface.id && matches!(entry.address, IpAddr::V4(_))
                })
        })
        .map(|interface| {
            let mut transaction_id = interface.id.0.wrapping_mul(0x9e37_79b9);
            for byte in interface.mac_address {
                transaction_id = transaction_id.rotate_left(5) ^ u32::from(byte);
            }
            DhcpClient {
                interface: interface.id,
                mac_address: interface.mac_address,
                transaction_id: transaction_id.max(1),
                phase: DhcpPhase::Discovering,
                next_action_ns: now_ns,
                retry_seconds: 1,
                installed: None,
            }
        })
        .collect()
}

fn build_dhcp_frame(
    client: &DhcpClient,
    message_type: u8,
    requested: Option<Ipv4Addr>,
    server: Option<Ipv4Addr>,
) -> Vec<u8> {
    let mut payload = alloc::vec![0; 300];
    payload[0] = 1;
    payload[1] = 1;
    payload[2] = 6;
    payload[4..8].copy_from_slice(&client.transaction_id.to_be_bytes());
    payload[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
    if matches!(&client.phase, DhcpPhase::Bound { .. }) {
        if let Some(address) = requested {
            payload[12..16].copy_from_slice(&address.0);
        }
    }
    payload[28..34].copy_from_slice(&client.mac_address);
    payload[236..240].copy_from_slice(&[99, 130, 83, 99]);
    let mut offset = 240;
    payload[offset..offset + 3].copy_from_slice(&[53, 1, message_type]);
    offset += 3;
    payload[offset..offset + 9].copy_from_slice(&[
        61,
        7,
        1,
        client.mac_address[0],
        client.mac_address[1],
        client.mac_address[2],
        client.mac_address[3],
        client.mac_address[4],
        client.mac_address[5],
    ]);
    offset += 9;
    if let Some(address) = requested {
        payload[offset..offset + 6].copy_from_slice(&[
            50,
            4,
            address.0[0],
            address.0[1],
            address.0[2],
            address.0[3],
        ]);
        offset += 6;
    }
    if let Some(server) = server {
        payload[offset..offset + 6].copy_from_slice(&[
            54,
            4,
            server.0[0],
            server.0[1],
            server.0[2],
            server.0[3],
        ]);
        offset += 6;
    }
    payload[offset..offset + 8].copy_from_slice(&[55, 6, 1, 3, 6, 51, 58, 59]);
    offset += 8;
    payload[offset] = 255;
    payload.truncate(offset + 1);

    let udp_len = 8 + payload.len();
    let mut frame = alloc::vec![0; 14 + 20 + udp_len];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&client.mac_address);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[14] = 0x45;
    frame[16..18].copy_from_slice(&((20 + udp_len) as u16).to_be_bytes());
    frame[18..20].copy_from_slice(&(client.transaction_id as u16).to_be_bytes());
    frame[20..22].copy_from_slice(&0x4000u16.to_be_bytes());
    frame[22] = 64;
    frame[23] = 17;
    frame[30..34].fill(0xff);
    let checksum = net::pipeline::checksum_bytes(&frame[14..34]);
    frame[24..26].copy_from_slice(&checksum.to_be_bytes());
    frame[34..36].copy_from_slice(&68u16.to_be_bytes());
    frame[36..38].copy_from_slice(&67u16.to_be_bytes());
    frame[38..40].copy_from_slice(&(udp_len as u16).to_be_bytes());
    frame[42..].copy_from_slice(&payload);
    frame
}

fn parse_dhcp_reply(packet: &FrontendPacket) -> Option<DhcpReply> {
    let udp = packet.parsed.udp?;
    if udp.source_port != 67 || udp.destination_port != 68 || udp.payload_len < 240 {
        return None;
    }
    let mut payload = alloc::vec![0; usize::from(udp.payload_len)];
    packet
        .chain
        .copy_out(usize::from(udp.payload_offset), &mut payload)
        .ok()?;
    if payload[0] != 2
        || payload[1] != 1
        || payload[2] != 6
        || payload[236..240] != [99, 130, 83, 99]
    {
        return None;
    }
    let mut reply = DhcpReply {
        message_type: 0,
        transaction_id: u32::from_be_bytes(payload[4..8].try_into().ok()?),
        client_mac: payload[28..34].try_into().ok()?,
        offered: Ipv4Addr(payload[16..20].try_into().ok()?),
        server: None,
        subnet_mask: None,
        router: None,
        dns: Vec::new(),
        lease_seconds: None,
        renewal_seconds: None,
        rebinding_seconds: None,
    };
    let mut offset = 240usize;
    while offset < payload.len() {
        let kind = payload[offset];
        offset += 1;
        if kind == 0 {
            continue;
        }
        if kind == 255 {
            break;
        }
        let len = usize::from(*payload.get(offset)?);
        offset += 1;
        let value = payload.get(offset..offset.checked_add(len)?)?;
        match (kind, len) {
            (53, 1) => reply.message_type = value[0],
            (54, 4) => reply.server = Some(Ipv4Addr(value.try_into().ok()?)),
            (1, 4) => reply.subnet_mask = Some(Ipv4Addr(value.try_into().ok()?)),
            (3, len) if len >= 4 => reply.router = Some(Ipv4Addr(value[..4].try_into().ok()?)),
            (6, len) if len >= 4 => {
                reply.dns.extend(
                    value
                        .chunks_exact(4)
                        .take(4)
                        .map(|entry| Ipv4Addr(entry.try_into().unwrap())),
                );
            }
            (51, 4) => reply.lease_seconds = Some(u32::from_be_bytes(value.try_into().ok()?)),
            (58, 4) => reply.renewal_seconds = Some(u32::from_be_bytes(value.try_into().ok()?)),
            (59, 4) => reply.rebinding_seconds = Some(u32::from_be_bytes(value.try_into().ok()?)),
            _ => {}
        }
        offset += len;
    }
    (reply.message_type != 0).then_some(reply)
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
const AF_INET: u16 = 2;
const ARPHRD_ETHER: u16 = 1;
const IFF_UP: u16 = 0x1;
const IFF_BROADCAST: u16 = 0x2;
const IFF_LOOPBACK: u16 = 0x8;
const IFF_RUNNING: u16 = 0x40;
const IFF_MULTICAST: u16 = 0x1000;
const SIOCGIFNAME: u32 = 0x8910;
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
    let mut ifreq = [0u8; IFREQ_LEN];
    copy_from_user(arg, &mut ifreq).map_err(|error| error.as_errno())?;
    if cmd == SIOCGIFNAME {
        let index = i32::from_ne_bytes(ifreq[16..20].try_into().unwrap());
        let devices = DEVICES.lock();
        let device = devices
            .iter()
            .find(|device| device.snapshot.id.raw() == index as u32)
            .ok_or(Errno::ENODEV)?;
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
        let _ = sched::activate_task(&self.task);
    }
}

struct WorkerContext {
    queue: Option<Box<dyn NetQueuePair>>,
    rx_pool: Option<NetBufPoolOwner>,
    tx_header_pool: Option<NetBufPoolOwner>,
    tx_payload_pool: Option<NetBufPoolOwner>,
    irq: Arc<dyn QueueIrqControl>,
    rx_batch: PacketBatch,
    frontend: VectorFrontend,
    frontend_batch: FrontendBatch,
    refill_batch: RxRefillBatch,
    completion_batch: CompletionBatch,
    tx_batch: TxBatch,
    retry_egress: VecDeque<EgressWork>,
    pending_tx_frames: VecDeque<PendingTxFrame>,
    next_fragment_id: u32,
    ingress_device: net::NetDeviceId,
    interface: InterfaceId,
    local_mac: [u8; 6],
    protocol_cluster: Arc<ProtocolCluster>,
    config: Arc<ConfigStore>,
    egress_index: usize,
    egress: Arc<EgressChannel>,
    rss_generation: u32,
    rss_key: [u8; 40],
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
    inline_protocol: bool,
}

struct PendingWorker {
    registration: NetQueueRegistration,
    ingress_device: net::NetDeviceId,
    interface: InterfaceId,
    local_mac: [u8; 6],
    cpu: usize,
    egress: Arc<EgressChannel>,
    egress_index: usize,
    control: Arc<WorkerControl>,
    stats: Arc<QueueRuntimeStats>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    arp_probe_enabled: bool,
}

struct ProtocolContext {
    runtime: Arc<ProtocolRuntime>,
    cluster: Arc<ProtocolCluster>,
    control_plane: Arc<ControlPlane>,
    config: Arc<ConfigStore>,
    protocol: FlowShard,
    recycle: PacketBatch,
    tx: TxBatch,
    pending: [Option<IngressWork>; 32],
    pending_neighbors: BTreeMap<net::control::NeighborKey, PendingNeighbor>,
    dad: Vec<DadState>,
    dhcp: Vec<DhcpClient>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_flow: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_sender: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_flow: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_sender: Option<FlowId>,
    local_queue: Option<Box<WorkerContext>>,
}

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
    vfs::net_socket::install_net_realtime_clock(crate::vdso::realtime_ns);
    let online = sched::online_cpu_mask();
    let active_cpus = (0..sched::NR_CPUS)
        .filter(|cpu| online & (1u64 << cpu) != 0)
        .collect::<Vec<_>>();
    assert!(!active_cpus.is_empty(), "NetWorker 没有 active CPU");
    let boot = net::stack::boot_config().expect("网络 stack 启动配置未安装");
    let mut generation_bytes = [0u8; 4];
    generation_bytes.copy_from_slice(&boot.generation_nonce()[..4]);
    let rss_generation = u32::from_le_bytes(generation_bytes).max(1);

    let devices = DEVICES.lock();
    let config = Arc::new(ConfigStore::new(build_device_config(&devices, 1)));
    *CONFIG_STORE.lock() = Some(Arc::clone(&config));
    drop(devices);
    let control_plane = Arc::new(ControlPlane::new(
        active_cpus.len(),
        *boot.rss_key(),
        boot.hash_seed(),
    ));
    let runtimes = active_cpus
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
    let mut protocol_tasks = Vec::with_capacity(runtimes.len());
    for runtime in &runtimes {
        let protocol = FlowShard::new(
            runtime.id,
            *boot.rss_key(),
            rss_generation,
            *boot.hash_seed(),
            *boot.tcp_isn_key(),
            sched::now_ns_public(),
        );
        let dad = if runtime.id == ShardId(0) {
            initial_dad_states(&config.snapshot(), sched::now_ns_public())
        } else {
            Vec::new()
        };
        let dhcp = if runtime.id == ShardId(0) {
            initial_dhcp_clients(&config.snapshot(), sched::now_ns_public())
        } else {
            Vec::new()
        };
        let slot = {
            let mut starts = PROTOCOL_STARTS.lock();
            let slot = starts.len();
            starts.push(Some(Box::new(ProtocolContext {
                runtime: Arc::clone(runtime),
                cluster: Arc::clone(&cluster),
                control_plane: Arc::clone(&control_plane),
                config: Arc::clone(&config),
                protocol,
                recycle: PacketBatch::new(),
                tx: TxBatch::new(),
                pending: core::array::from_fn(|_| None),
                pending_neighbors: BTreeMap::new(),
                dad,
                dhcp,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                udp_probe_flow: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                udp_probe_sender: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                physical_udp_probe_flow: None,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                physical_udp_probe_sender: None,
                local_queue: None,
            })));
            slot
        };
        let task = sched::kthread_create(
            protocol_worker_entry,
            slot,
            sched::SchedParams {
                nice: -5,
                slice_ns: 0,
            },
        );
        task.set_cpu_affinity(online);
        runtime.set_owner_task(Arc::clone(&task));
        protocol_tasks.push((task, runtime.cpu, slot));
    }

    *PROTOCOL_CLUSTER.lock() = Some(cluster);
    net::install_socket_runtime(&SOCKET_RUNTIME_ADAPTER)
        .unwrap_or_else(|_| panic!("socket runtime 重复安装"));
    for (task, cpu, _) in protocol_tasks {
        sched::activate_task_with_cpu_hint(&task, cpu)
            .unwrap_or_else(|error| panic!("协议 worker 启动失败: {:?}", error));
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
    let Some(config) = CONFIG_STORE.lock().as_ref().cloned() else {
        return;
    };
    let boot = net::stack::boot_config().expect("网络 stack 启动配置未安装");
    let online = sched::online_cpu_mask();
    let active_cpus = (0..sched::NR_CPUS)
        .filter(|cpu| online & (1u64 << cpu) != 0)
        .collect::<Vec<_>>();
    if active_cpus.is_empty() {
        return;
    }
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
            let interface = InterfaceId(device.snapshot.id.raw());
            let stats = Arc::clone(&device.queue_stats[queue_index]);
            let egress = Arc::new(EgressChannel::new(interface, Arc::clone(&stats)));
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
                cpu: active_cpus[registration.id.0 as usize % active_cpus.len()],
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
            queue: Some(queue),
            rx_pool: Some(rx_pool),
            tx_header_pool: Some(tx_header_pool),
            tx_payload_pool: Some(tx_payload_pool),
            irq: Arc::clone(&irq),
            rx_batch: PacketBatch::new(),
            frontend: VectorFrontend::new(*boot.rss_key(), rss_generation),
            frontend_batch: FrontendBatch::new(),
            refill_batch: RxRefillBatch::new(),
            completion_batch: CompletionBatch::new(),
            tx_batch: TxBatch::new(),
            retry_egress: VecDeque::new(),
            pending_tx_frames: VecDeque::new(),
            next_fragment_id: 1,
            ingress_device: pending.ingress_device,
            interface: pending.interface,
            local_mac: pending.local_mac,
            protocol_cluster: Arc::clone(&cluster),
            config: Arc::clone(&config),
            egress_index: pending.egress_index,
            egress: Arc::clone(&egress),
            rss_generation,
            rss_key: *boot.rss_key(),
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
            inline_protocol: false,
        });
        let slot = {
            let mut starts = WORKER_STARTS.lock();
            let slot = starts.len();
            starts.push(Some(context));
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
        task.set_cpu_affinity(1u64 << pending.cpu);
        control.tasks.lock().push(Arc::clone(&task));
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        WORKER_TASKS.lock().push(Arc::clone(&task));
        egress.set_task(Arc::clone(&task));
        irq.set_waker(Arc::new(TaskWake {
            task: Arc::clone(&task),
        }))
        .unwrap_or_else(|error| panic!("NetWorker waker 安装失败: {:?}", error));
        sched::activate_task(&task)
            .unwrap_or_else(|error| panic!("NetWorker 启动失败: {:?}", error));
    }
}

unsafe extern "C" fn net_worker_entry(slot: usize) -> ! {
    let context = {
        let mut starts = WORKER_STARTS.lock();
        let context = starts
            .get_mut(slot)
            .and_then(Option::take)
            .expect("NetWorker 启动上下文不存在");
        while starts.last().is_some_and(Option::is_none) {
            starts.pop();
        }
        starts.shrink_to_fit();
        context
    };
    context.run()
}

unsafe extern "C" fn protocol_worker_entry(slot: usize) -> ! {
    let mut context = PROTOCOL_STARTS
        .lock()
        .get_mut(slot)
        .and_then(Option::take)
        .expect("协议 worker 启动上下文不存在");
    context.run()
}

impl ProtocolContext {
    fn run(&mut self) -> ! {
        loop {
            #[cfg(feature = "performance-profile")]
            let profile_turn = profiling::scope(profiling::Event::NetProtocolTurn);
            self.pump_local_queue();
            let config = self.config.snapshot();
            let lifecycle = self.drain_lifecycle(256, &config);
            let control = self.drain_control(256, &config);
            let dirty = self.drain_socket_tx(256, &config);
            self.pump_local_queue();
            let mut processed = 0usize;
            while processed < 128 {
                let count = self.drain_ingress();
                if count == 0 {
                    break;
                }
                processed += count;
                self.process_pending(count, &config);
                self.pump_local_queue();
            }
            self.runtime.timer_fired.store(false, Ordering::Release);
            let now_ns = sched::now_ns_public();
            self.protocol.run_due_timers(now_ns);
            let neighbor_deadline = self.run_neighbor_timers(&config, now_ns);
            let dad_deadline = self.run_dad(now_ns);
            let dhcp_deadline = self.run_dhcp(now_ns);
            self.dispatch_tcp_output();
            self.pump_local_queue();
            self.runtime.arm_timer(
                [
                    self.protocol.next_timer_deadline_ns(),
                    neighbor_deadline,
                    dad_deadline,
                    dhcp_deadline,
                ]
                .into_iter()
                .flatten()
                .min(),
            );
            let keep_running = processed == 128
                || lifecycle == 256
                || control == 256
                || dirty == 256
                || self.protocol.has_blocked_tcp_output()
                || self.runtime.finish_drain()
                || self.local_queue_pending();
            #[cfg(feature = "performance-profile")]
            drop(profile_turn);
            if keep_running {
                let _ = sched::operation::sched_yield();
                continue;
            }
            self.sleep_until_ingress();
        }
    }

    fn pump_local_queue(&mut self) {
        let Some(mut queue) = self.local_queue.take() else {
            return;
        };
        let remove_ready = queue.control.remove_requested.load(Ordering::Acquire)
            && queue.control.remove_ready.load(Ordering::Acquire);
        if !remove_ready && !queue.has_pending_work() {
            self.local_queue = Some(queue);
            return;
        }
        if queue.run_turn() == WorkerTurn::Removed {
            queue.finish_removal();
        } else {
            self.local_queue = Some(queue);
        }
    }

    fn local_queue_pending(&mut self) -> bool {
        self.local_queue
            .as_mut()
            .is_some_and(|queue| queue.has_pending_work())
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
                        let result = if facade.generation() != generation {
                            Err(SocketError::Closed)
                        } else {
                            self.bind_facade(&facade, local, None, interface, options, config)
                                .map(|_| ())
                        };
                        facade.complete_control(sequence, result);
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
                        let result = if facade.generation() != generation {
                            Some(Err(SocketError::Closed))
                        } else {
                            match self.connect_facade(
                                &facade,
                                sequence,
                                peer,
                                interface,
                                options,
                                nonblocking,
                                config,
                            ) {
                                Ok(true) => Some(Ok(())),
                                Ok(false) => None,
                                Err(error) => Some(Err(error)),
                            }
                        };
                        if let Some(result) = result {
                            facade.complete_control(sequence, result);
                        }
                    }
                    SocketCommand::Listen {
                        facade,
                        sequence,
                        generation,
                        backlog,
                    } => {
                        let result = if facade.generation() != generation {
                            Some(Err(SocketError::Closed))
                        } else {
                            match self.listen_facade(&facade, sequence, backlog, config) {
                                Ok(true) => Some(Ok(())),
                                Ok(false) => None,
                                Err(error) => Some(Err(error)),
                            }
                        };
                        if let Some(result) = result {
                            facade.complete_control(sequence, result);
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
                } => {
                    if facade.generation() == generation {
                        if let Err(error) =
                            self.install_tcp_flow(&facade, sequence, local, peer, path)
                        {
                            facade.complete_control(sequence, Err(error));
                        }
                    } else {
                        facade.complete_control(sequence, Err(SocketError::Closed));
                    }
                }
                ControlWork::InstallListener { transaction } => {
                    let result = self
                        .protocol
                        .listen_tcp(
                            transaction.local,
                            transaction.interface,
                            Arc::clone(&transaction.group),
                        )
                        .and_then(|_| {
                            if !transaction.dual_stack {
                                return Ok(());
                            }
                            self.protocol.listen_tcp(
                                Endpoint {
                                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                    port: transaction.local.port,
                                },
                                transaction.interface,
                                Arc::clone(&transaction.group),
                            )
                        })
                        .map_err(map_tcp_bind_error);
                    if result.is_err() {
                        self.protocol.close_tcp_listener(transaction.group.id());
                    }
                    transaction.finish(result);
                }
                ControlWork::RemoveListener { transaction } => {
                    self.protocol.close_tcp_listener(transaction.group);
                    self.dispatch_tcp_output();
                    transaction.finish();
                }
                ControlWork::DiscardListener { group } => {
                    self.protocol.close_tcp_listener(group);
                    self.dispatch_tcp_output();
                }
                ControlWork::InterfaceGone { interface, ack } => {
                    self.protocol.invalidate_interface(interface);
                    self.fail_interface_neighbors(interface, SocketError::NetworkUnreachable);
                    self.dad.retain(|state| state.interface != interface);
                    let removed_lease = self
                        .dhcp
                        .iter()
                        .find(|client| client.interface == interface)
                        .and_then(|client| client.installed.clone());
                    if let Some(lease) = removed_lease.as_ref() {
                        self.replace_dhcp_lease(interface, Some(lease), None);
                    }
                    self.dhcp.retain(|client| client.interface != interface);
                    if self.runtime.id == ShardId(0) {
                        self.remove_interface_multicast(interface);
                    }
                    self.dispatch_tcp_output();
                    ack.finish();
                }
                ControlWork::ResolveNeighbor(work) => {
                    self.enqueue_neighbor(work, config, sched::now_ns_public());
                }
                ControlWork::NeighborObserved {
                    key,
                    mac_address,
                    now_ns,
                } => {
                    if self.protocol.observe_neighbor(key, mac_address, now_ns) {
                        self.resolve_neighbor(key, mac_address);
                    }
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
                    let _ = self
                        .protocol
                        .apply_transport_error(interface, target, error, now_ns);
                    self.dispatch_tcp_output();
                }
            }
        }
        processed
    }

    fn bind_facade(
        &mut self,
        facade: &Arc<SocketFacade>,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        options: BindOptions,
        config: &ConfigSnapshot,
    ) -> Result<(), SocketError> {
        facade.set_v6_only(options.v6_only);
        match facade.kind() {
            SocketKind::Datagram => self
                .bind_udp_facade(facade, local, peer, interface, options, config)
                .map(|_| ()),
            SocketKind::Stream => {
                if peer.is_some() {
                    return Err(SocketError::InvalidState);
                }
                self.bind_tcp_facade(facade, local, interface, options, config)
            }
            SocketKind::Raw => {
                if peer.is_some() {
                    return Err(SocketError::InvalidState);
                }
                self.bind_raw_facade(facade, local, interface, options, config)
                    .map(|_| ())
            }
        }
    }

    fn bind_raw_facade(
        &mut self,
        facade: &Arc<SocketFacade>,
        mut local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        config: &ConfigSnapshot,
    ) -> Result<FlowId, SocketError> {
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
        let flow = self
            .protocol
            .bind_raw_facade(local.addr, interface, Arc::clone(facade), options.free_bind)
            .map_err(|error| match error {
                net::transport::RawBindError::InvalidEndpoint => SocketError::AddressUnavailable,
                net::transport::RawBindError::TableFull => SocketError::Buffer,
            })?;
        facade.publish_binding(
            OwnerRef::Flow {
                shard: self.runtime.id,
                flow,
                generation: facade.generation(),
            },
            local,
            None,
            interface,
        );
        Ok(flow)
    }

    fn bind_udp_facade(
        &mut self,
        facade: &Arc<SocketFacade>,
        mut local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        options: BindOptions,
        config: &ConfigSnapshot,
    ) -> Result<FlowId, SocketError> {
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
        let token = if local.port == 0 {
            self.control_plane
                .bind_registry
                .reserve_ephemeral(request, self.runtime.id)
                .map_err(map_bind_error)?
        } else {
            self.control_plane
                .bind_registry
                .reserve(request)
                .map_err(map_bind_error)?
        };
        local.port = token.port;
        facade.set_free_bind(options.free_bind);
        let accepts_ipv4 = family == AddressFamily::Ipv6
            && !options.v6_only
            && matches!(local.addr, IpAddr::V6(address) if address.is_unspecified());
        let flow = match self.protocol.bind_udp_facade(
            local,
            peer,
            interface,
            Arc::clone(facade),
            options.free_bind,
            accepts_ipv4,
        ) {
            Ok(flow) => flow,
            Err(error) => {
                let _ = self.control_plane.bind_registry.release(token);
                return Err(map_udp_bind_error(error));
            }
        };
        self.control_plane.remember_binding(facade.id(), token);
        facade.publish_binding(
            OwnerRef::Flow {
                shard: self.runtime.id,
                flow,
                generation: facade.generation(),
            },
            local,
            peer,
            interface,
        );
        Ok(flow)
    }

    fn bind_tcp_facade(
        &mut self,
        facade: &Arc<SocketFacade>,
        mut local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
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
        let token = if local.port == 0 {
            self.control_plane
                .bind_registry
                .reserve_ephemeral(request, self.runtime.id)
                .map_err(map_bind_error)?
        } else {
            self.control_plane
                .bind_registry
                .reserve(request)
                .map_err(map_bind_error)?
        };
        local.port = token.port;
        facade.set_free_bind(options.free_bind);
        self.control_plane.remember_binding(facade.id(), token);
        facade.publish_binding(
            OwnerRef::Bound {
                generation: facade.generation(),
            },
            local,
            None,
            interface,
        );
        Ok(())
    }

    fn connect_facade(
        &mut self,
        facade: &Arc<SocketFacade>,
        control_sequence: u64,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        _nonblocking: bool,
        config: &ConfigSnapshot,
    ) -> Result<bool, SocketError> {
        if !address_allowed(facade.family(), peer.addr, options.v6_only) {
            return Err(SocketError::AddressUnavailable);
        }
        if facade.kind() == SocketKind::Stream {
            let bound = facade.local_endpoint();
            let bound_source =
                bound.and_then(|local| (!local.addr.is_unspecified()).then_some(local.addr));
            let path = self.protocol.resolve_tcp_path(
                peer.addr,
                bound_source,
                interface.or_else(|| facade.interface()),
                config,
                sched::now_ns_public(),
                options.free_bind,
            )?;
            if matches!(facade.owner(), OwnerRef::Unassigned) {
                self.bind_tcp_facade(
                    facade,
                    Endpoint {
                        addr: path.route.source,
                        port: 0,
                    },
                    Some(path.route.interface),
                    options,
                    config,
                )?;
            }
            let mut local = facade.local_endpoint().ok_or(SocketError::InvalidState)?;
            if local.addr.is_unspecified() {
                local.addr = path.route.source;
            }
            if !matches!(facade.owner(), OwnerRef::Bound { .. }) {
                return Err(SocketError::AlreadyConnected);
            }
            let target = self
                .control_plane
                .flow_shard(peer, local, TransportProtocol::Tcp);
            if target == self.runtime.id {
                self.install_tcp_flow(facade, control_sequence, local, peer, path)?;
            } else {
                self.cluster
                    .publish_control(
                        target,
                        ControlWork::ConnectTcp {
                            facade: Arc::clone(facade),
                            sequence: control_sequence,
                            generation: facade.generation(),
                            local,
                            peer,
                            path,
                        },
                    )
                    .map_err(|_| SocketError::RuntimeBusy)?;
            }
            return Ok(false);
        }
        if facade.kind() == SocketKind::Raw {
            let flow = match facade.owner() {
                OwnerRef::Unassigned => self.bind_raw_facade(
                    facade,
                    Endpoint {
                        addr: unspecified_address(facade.family()),
                        port: 0,
                    },
                    interface,
                    options,
                    config,
                )?,
                OwnerRef::Flow { flow, .. } => flow,
                OwnerRef::Closed { .. } => return Err(SocketError::Closed),
                _ => return Err(SocketError::InvalidState),
            };
            let local = facade.local_endpoint().ok_or(SocketError::InvalidState)?;
            facade.publish_binding(
                OwnerRef::Flow {
                    shard: self.runtime.id,
                    flow,
                    generation: facade.generation(),
                },
                local,
                Some(peer),
                interface.or_else(|| facade.interface()),
            );
            return Ok(true);
        }
        match facade.owner() {
            OwnerRef::Unassigned => {
                let route = config
                    .route_with_source_policy(
                        peer.addr,
                        facade.socket_mark(),
                        None,
                        interface,
                        options.free_bind,
                    )
                    .map_err(|_| SocketError::NetworkUnreachable)?;
                let local = Endpoint {
                    addr: route.source,
                    port: 0,
                };
                self.bind_udp_facade(facade, local, Some(peer), interface, options, config)?;
                Ok(true)
            }
            OwnerRef::Flow { flow, .. } => {
                let mut local = facade.local_endpoint().ok_or(SocketError::InvalidState)?;
                if !address_matches_family(address_family(peer.addr), local.addr) {
                    let route = config
                        .route_with_source_policy(
                            peer.addr,
                            facade.socket_mark(),
                            None,
                            interface.or_else(|| facade.interface()),
                            options.free_bind,
                        )
                        .map_err(|_| SocketError::NetworkUnreachable)?;
                    local.addr = route.source;
                }
                let flow = self
                    .protocol
                    .reconnect_udp_facade(flow, local, peer, Arc::clone(facade))
                    .map_err(map_udp_bind_error)?;
                facade.publish_binding(
                    OwnerRef::Flow {
                        shard: self.runtime.id,
                        flow,
                        generation: facade.generation(),
                    },
                    local,
                    Some(peer),
                    interface.or_else(|| facade.interface()),
                );
                Ok(true)
            }
            OwnerRef::Bound { .. } | OwnerRef::Listener { .. } => Err(SocketError::InvalidState),
            OwnerRef::Closed { .. } => Err(SocketError::Closed),
        }
    }

    fn install_tcp_flow(
        &mut self,
        facade: &Arc<SocketFacade>,
        control_sequence: u64,
        local: Endpoint,
        peer: Endpoint,
        path: TcpPath,
    ) -> Result<(), SocketError> {
        let interface = path.route.interface;
        let flow = self
            .protocol
            .connect_tcp(
                local,
                peer,
                path,
                Arc::clone(facade),
                control_sequence,
                sched::now_ns_public(),
            )
            .map_err(map_tcp_bind_error)?;
        facade.publish_binding(
            OwnerRef::Flow {
                shard: self.runtime.id,
                flow,
                generation: facade.generation(),
            },
            local,
            Some(peer),
            Some(interface),
        );
        self.dispatch_tcp_output();
        Ok(())
    }

    fn listen_facade(
        &mut self,
        facade: &Arc<SocketFacade>,
        control_sequence: u64,
        backlog: u32,
        config: &ConfigSnapshot,
    ) -> Result<bool, SocketError> {
        if facade.kind() != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if matches!(facade.owner(), OwnerRef::Unassigned) {
            self.bind_tcp_facade(
                facade,
                Endpoint {
                    addr: unspecified_address(facade.family()),
                    port: 0,
                },
                None,
                BindOptions::default(),
                config,
            )?;
        }
        if matches!(facade.owner(), OwnerRef::Listener { .. }) {
            let group = facade.listen_group().ok_or(SocketError::InvalidState)?;
            group.update_backlog(backlog);
            return Ok(true);
        }
        if !matches!(facade.owner(), OwnerRef::Bound { .. }) {
            return Err(SocketError::InvalidState);
        }
        let local = facade.local_endpoint().ok_or(SocketError::InvalidState)?;
        let cpu_hints = self
            .cluster
            .shards
            .iter()
            .map(|runtime| runtime.cpu)
            .collect::<Vec<_>>();
        let group = ListenGroup::new_with_cpu_hints(
            self.control_plane.allocate_listener_id(),
            facade,
            &cpu_hints,
            backlog,
        );
        let transaction = Arc::new(ListenerInstall {
            facade: Arc::clone(facade),
            group,
            local,
            interface: facade.interface(),
            dual_stack: facade.family() == AddressFamily::Ipv6
                && !facade.v6_only()
                && matches!(local.addr, IpAddr::V6(address) if address.is_unspecified()),
            sequence: control_sequence,
            generation: facade.generation(),
            remaining: AtomicUsize::new(self.cluster.shards.len()),
            failed: AtomicBool::new(false),
            control: Arc::clone(&self.control_plane),
            cluster: Arc::clone(&self.cluster),
        });
        for runtime in &self.cluster.shards {
            let mut work = ControlWork::InstallListener {
                transaction: Arc::clone(&transaction),
            };
            loop {
                match runtime.control.try_push(work) {
                    Ok(()) => {
                        if !runtime.pending.swap(true, Ordering::AcqRel) {
                            runtime.wake_owner();
                        }
                        break;
                    }
                    Err(pending) => {
                        work = pending;
                        runtime.wake_owner();
                        let _ = sched::operation::sched_yield();
                    }
                }
            }
        }
        Ok(false)
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
                    self.protocol.drain_tcp_send(flow, sched::now_ns_public());
                    self.dispatch_tcp_output();
                    facade.finish_stream_tx_drain(generation);
                }
                SocketKind::Datagram => {
                    self.drain_udp_socket(&facade, flow, config, 32);
                }
                SocketKind::Raw => {
                    for _ in 0..32 {
                        let Some(payload) = facade.take_tx() else {
                            break;
                        };
                        match self.protocol.prepare_raw_tx(
                            flow,
                            payload,
                            facade.socket_mark(),
                            config,
                            sched::now_ns_public(),
                        ) {
                            Ok(work) => {
                                if work.unresolved_neighbor.is_some() {
                                    self.publish_neighbor_work(PendingNeighborTx::Raw(work));
                                    continue;
                                }
                                let Some(target) = self.runtime.egress_index(work.route.interface)
                                else {
                                    work.payload
                                        .facade()
                                        .set_pending_error(SocketError::NetworkUnreachable);
                                    continue;
                                };
                                self.dispatch_egress(target, EgressWork::Raw(work));
                            }
                            Err((error, payload)) => payload.facade().set_pending_error(error),
                        }
                    }
                }
            }
            if facade.kind() != SocketKind::Stream {
                facade.finish_tx_drain();
            }
        }
        #[cfg(feature = "performance-profile")]
        if processed != 0 {
            profiling::observe(profiling::Metric::DirtyDrainSockets, processed as u64);
        }
        processed
    }

    /// close 必须排在已经被 sendto 接受的数据之后。UDP TX 与 lifecycle 使用不同
    /// 的 MPSC 队列，不能依赖两条队列之间的观察顺序，因此在解绑 flow 前直接
    /// 排空该 socket 的 TX ring。
    fn drain_udp_before_close(
        &mut self,
        facade: &Arc<SocketFacade>,
        flow: FlowId,
        config: &ConfigSnapshot,
    ) {
        self.drain_udp_socket(facade, flow, config, usize::MAX);
        facade.finish_tx_drain();
    }

    fn drain_udp_socket(
        &mut self,
        facade: &Arc<SocketFacade>,
        flow: FlowId,
        config: &ConfigSnapshot,
        limit: usize,
    ) {
        let mut first = None;
        let mut batch = Vec::new();
        let mut batch_target = None;
        #[cfg(feature = "performance-profile")]
        let mut drained = 0usize;
        for _ in 0..limit {
            let Some(payload) = facade.take_tx() else {
                break;
            };
            #[cfg(feature = "performance-profile")]
            {
                drained += 1;
            }
            match self.protocol.prepare_udp_tx(
                flow,
                payload,
                facade.socket_mark(),
                config,
                sched::now_ns_public(),
            ) {
                Ok(work) => {
                    if work.unresolved_neighbor.is_some() {
                        self.flush_udp_accumulator(&mut first, &mut batch_target, &mut batch);
                        self.publish_neighbor_work(PendingNeighborTx::Udp(work));
                        continue;
                    }
                    let Some(target) = self.runtime.egress_index(work.route.interface) else {
                        self.flush_udp_accumulator(&mut first, &mut batch_target, &mut batch);
                        work.payload
                            .facade()
                            .set_pending_error(SocketError::NetworkUnreachable);
                        continue;
                    };
                    if config
                        .interfaces
                        .iter()
                        .any(|interface| interface.id == work.route.interface && interface.loopback)
                        && udp_batch_candidate(&work)
                    {
                        self.flush_udp_accumulator(&mut first, &mut batch_target, &mut batch);
                        self.dispatch_local_udp(target, work);
                        continue;
                    }
                    if !udp_batch_candidate(&work) {
                        self.flush_udp_accumulator(&mut first, &mut batch_target, &mut batch);
                        self.dispatch_egress(target, EgressWork::Udp(work));
                        continue;
                    }
                    if let Some(batch_first) = batch.first() {
                        if batch_target == Some(target)
                            && batch.len() < 16
                            && udp_batch_compatible(batch_first, &work)
                        {
                            batch.push(work);
                        } else {
                            self.flush_udp_batch(batch_target.take(), &mut batch);
                            first = Some((target, work));
                        }
                        continue;
                    }
                    if let Some((pending_target, pending)) = first.take() {
                        if pending_target == target && udp_batch_compatible(&pending, &work) {
                            batch.reserve_exact(16);
                            batch_target = Some(target);
                            batch.push(pending);
                            batch.push(work);
                        } else {
                            self.dispatch_egress(pending_target, EgressWork::Udp(pending));
                            first = Some((target, work));
                        }
                    } else {
                        first = Some((target, work));
                    }
                }
                Err((error, payload)) => {
                    self.flush_udp_accumulator(&mut first, &mut batch_target, &mut batch);
                    payload.facade().set_pending_error(error);
                }
            }
        }
        self.flush_udp_accumulator(&mut first, &mut batch_target, &mut batch);
        #[cfg(feature = "performance-profile")]
        if drained != 0 {
            profiling::observe(profiling::Metric::SocketDrainDatagrams, drained as u64);
        }
    }

    fn dispatch_local_udp(&mut self, egress: usize, work: PreparedUdpTx) {
        let source = Endpoint {
            addr: work.route.source,
            port: work.source_port,
        };
        let key = FlowKey::new(source, work.destination, TransportProtocol::Udp)
            .expect("UDP local transport tuple 必须有效");
        let target = self.cluster.local_ingress_target(&key);
        let ingress = IngressWork::LocalUdp {
            egress,
            interface: work.route.interface,
            work,
        };
        if target.try_push(ingress).is_err() {
            if let Some(egress) = self.runtime.egress(egress) {
                egress.stats.drop_reasons[DropReason::IngressRingFull.index()]
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn flush_udp_accumulator(
        &mut self,
        first: &mut Option<(usize, PreparedUdpTx)>,
        batch_target: &mut Option<usize>,
        batch: &mut Vec<PreparedUdpTx>,
    ) {
        if let Some((target, work)) = first.take() {
            self.dispatch_egress(target, EgressWork::Udp(work));
        }
        self.flush_udp_batch(batch_target.take(), batch);
    }

    fn flush_udp_batch(&mut self, target: Option<usize>, batch: &mut Vec<PreparedUdpTx>) {
        if batch.is_empty() {
            return;
        }
        let target = target.expect("非空 UDP 批次必须拥有 egress");
        let mut pending = core::mem::take(batch);
        if pending.len() == 1 {
            self.dispatch_egress(target, EgressWork::Udp(pending.pop().unwrap()));
        } else {
            self.dispatch_egress(target, EgressWork::UdpBatch(pending));
        }
    }

    fn publish_neighbor_work(&self, work: PendingNeighborTx) {
        let target_id = self.control_plane.neighbor_owner(work.key());
        let Some(target) = self.cluster.shard(target_id) else {
            work.facade()
                .set_pending_error(SocketError::HostUnreachable);
            return;
        };
        let mut control = ControlWork::ResolveNeighbor(work);
        loop {
            match target.control.try_push(control) {
                Ok(()) => {
                    if !target.pending.swap(true, Ordering::AcqRel) {
                        target.wake_owner();
                    }
                    return;
                }
                Err(pending) => {
                    control = pending;
                    target.wake_owner();
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn enqueue_neighbor(
        &mut self,
        mut work: PendingNeighborTx,
        config: &ConfigSnapshot,
        now_ns: u64,
    ) {
        let key = work.key();
        if let Some(mac_address) = self.protocol.lookup_neighbor(key, now_ns) {
            work.resolve(mac_address);
            self.dispatch_neighbor_tx(work);
            return;
        }
        if self
            .pending_neighbors
            .get(&key)
            .is_some_and(|pending| pending.packets.len() >= 32)
            || !self.control_plane.reserve_neighbor_packet(key.interface)
        {
            work.facade()
                .set_pending_error(SocketError::HostUnreachable);
            return;
        }
        self.pending_neighbors
            .entry(key)
            .or_insert_with(|| PendingNeighbor {
                packets: VecDeque::new(),
                probes: 0,
                next_probe_ns: now_ns,
                expires_ns: now_ns.saturating_add(3_000_000_000),
            })
            .packets
            .push_back(work);
        self.run_neighbor_timers(config, now_ns);
    }

    fn resolve_neighbor(&mut self, key: net::control::NeighborKey, mac_address: [u8; 6]) {
        let Some(mut pending) = self.pending_neighbors.remove(&key) else {
            return;
        };
        self.control_plane
            .release_neighbor_packets(key.interface, pending.packets.len());
        while let Some(mut work) = pending.packets.pop_front() {
            work.resolve(mac_address);
            self.dispatch_neighbor_tx(work);
        }
    }

    fn dispatch_neighbor_tx(&mut self, work: PendingNeighborTx) {
        let interface = match &work {
            PendingNeighborTx::Tcp(work) => work.path.route.interface,
            PendingNeighborTx::Udp(work) => work.route.interface,
            PendingNeighborTx::Raw(work) => work.route.interface,
        };
        let Some(target) = self.runtime.egress_index(interface) else {
            work.facade()
                .set_pending_error(SocketError::NetworkUnreachable);
            return;
        };
        let work = match work {
            PendingNeighborTx::Tcp(work) => EgressWork::Tcp(work),
            PendingNeighborTx::Udp(work) => EgressWork::Udp(work),
            PendingNeighborTx::Raw(work) => EgressWork::Raw(work),
        };
        self.dispatch_egress(target, work);
    }

    fn fail_interface_neighbors(&mut self, interface: InterfaceId, error: SocketError) {
        let keys = self
            .pending_neighbors
            .keys()
            .filter(|key| key.interface == interface)
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(mut pending) = self.pending_neighbors.remove(&key) {
                self.control_plane
                    .release_neighbor_packets(interface, pending.packets.len());
                for work in pending.packets.drain(..) {
                    work.facade().set_pending_error(error);
                }
            }
        }
    }

    fn run_neighbor_timers(&mut self, config: &ConfigSnapshot, now_ns: u64) -> Option<u64> {
        let keys = self.pending_neighbors.keys().copied().collect::<Vec<_>>();
        for key in keys {
            let expired = self
                .pending_neighbors
                .get(&key)
                .is_some_and(|pending| pending.expires_ns <= now_ns);
            if expired {
                let mut pending = self.pending_neighbors.remove(&key).unwrap();
                self.control_plane
                    .release_neighbor_packets(key.interface, pending.packets.len());
                for work in pending.packets.drain(..) {
                    work.facade()
                        .set_pending_error(SocketError::HostUnreachable);
                }
                continue;
            }
            let probe = self
                .pending_neighbors
                .get(&key)
                .is_some_and(|pending| pending.probes < 3 && pending.next_probe_ns <= now_ns);
            if probe {
                self.emit_neighbor_probe(key, config);
                let pending = self.pending_neighbors.get_mut(&key).unwrap();
                pending.probes += 1;
                pending.next_probe_ns = pending.next_probe_ns.saturating_add(1_000_000_000);
            }
        }
        self.pending_neighbors
            .values()
            .map(|pending| {
                if pending.probes < 3 {
                    pending.next_probe_ns.min(pending.expires_ns)
                } else {
                    pending.expires_ns
                }
            })
            .min()
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

    fn run_dad(&mut self, now_ns: u64) -> Option<u64> {
        let snapshot = self.config.snapshot();
        for index in 0..self.dad.len() {
            if !self.dad[index].probe_sent {
                self.emit_dad_probe(
                    net::control::NeighborKey {
                        interface: self.dad[index].interface,
                        address: IpAddr::V6(self.dad[index].address),
                    },
                    &snapshot,
                );
                self.dad[index].probe_sent = true;
            }
        }
        let mut index = 0;
        while index < self.dad.len() {
            if self.dad[index].deadline_ns > now_ns {
                index += 1;
                continue;
            }
            let state = self.dad.swap_remove(index);
            if state.conflict {
                self.control_plane
                    .dad_errors
                    .lock()
                    .insert(state.interface, SocketError::AddressInUse);
            } else {
                self.publish_dad_address(state.interface, state.address);
            }
        }
        self.dad.iter().map(|state| state.deadline_ns).min()
    }

    fn publish_dad_address(&self, interface: InterfaceId, address: Ipv6Addr) {
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

    fn run_dhcp(&mut self, now_ns: u64) -> Option<u64> {
        let config = self.config.snapshot();
        self.dhcp.retain(|client| {
            let configured = config.addresses.iter().find_map(|entry| {
                (entry.interface == client.interface)
                    .then_some(entry.address)
                    .and_then(|address| match address {
                        IpAddr::V4(address) => Some(address),
                        IpAddr::V6(_) => None,
                    })
            });
            match (&client.installed, configured) {
                (Some(lease), Some(address)) => lease.address == address,
                (None, None) => true,
                _ => false,
            }
        });
        for index in 0..self.dhcp.len() {
            let expired = matches!(
                &self.dhcp[index].phase,
                DhcpPhase::Bound { expires_ns, .. } if *expires_ns <= now_ns
            );
            if expired {
                let interface = self.dhcp[index].interface;
                let old = self.dhcp[index].installed.take();
                self.replace_dhcp_lease(interface, old.as_ref(), None);
                self.dhcp[index].phase = DhcpPhase::Discovering;
                self.dhcp[index].next_action_ns = now_ns;
                self.dhcp[index].retry_seconds = 1;
            }
            if self.dhcp[index].next_action_ns > now_ns {
                continue;
            }
            let client = &self.dhcp[index];
            let frame = match &client.phase {
                DhcpPhase::Discovering => build_dhcp_frame(client, 1, None, None),
                DhcpPhase::Requesting { lease, server } => {
                    build_dhcp_frame(client, 3, Some(lease.address), Some(*server))
                }
                DhcpPhase::Bound {
                    lease,
                    server,
                    renew_ns,
                    rebind_ns,
                    ..
                } if *renew_ns <= now_ns => build_dhcp_frame(
                    client,
                    3,
                    Some(lease.address),
                    (*rebind_ns > now_ns).then_some(*server),
                ),
                DhcpPhase::Bound { renew_ns, .. } => {
                    self.dhcp[index].next_action_ns = *renew_ns;
                    continue;
                }
            };
            self.emit_control_frame(client.interface, frame);
            let retry = self.dhcp[index].retry_seconds.clamp(1, 64);
            self.dhcp[index].next_action_ns =
                now_ns.saturating_add(u64::from(retry).saturating_mul(1_000_000_000));
            self.dhcp[index].retry_seconds = retry.saturating_mul(2).min(64);
        }
        self.dhcp.iter().map(|client| client.next_action_ns).min()
    }

    fn handle_dhcp_packet(&mut self, interface: InterfaceId, packet: &FrontendPacket) -> bool {
        let Some(reply) = parse_dhcp_reply(packet) else {
            return false;
        };
        let Some(index) = self.dhcp.iter().position(|client| {
            client.interface == interface
                && client.transaction_id == reply.transaction_id
                && client.mac_address == reply.client_mac
        }) else {
            return false;
        };
        match reply.message_type {
            2 => {
                let Some(server) = reply.server else {
                    return true;
                };
                let prefix_len = reply.subnet_mask.and_then(ipv4_mask_prefix).unwrap_or(24);
                self.dhcp[index].phase = DhcpPhase::Requesting {
                    lease: DhcpLease {
                        address: reply.offered,
                        prefix_len,
                        router: reply.router,
                        dns: reply.dns,
                        lease_seconds: reply.lease_seconds.unwrap_or(3600).max(60),
                    },
                    server,
                };
                self.dhcp[index].next_action_ns = sched::now_ns_public();
                self.dhcp[index].retry_seconds = 1;
            }
            5 => {
                let now_ns = sched::now_ns_public();
                let (requested, previous_server) = match &self.dhcp[index].phase {
                    DhcpPhase::Requesting { lease, server }
                    | DhcpPhase::Bound { lease, server, .. } => {
                        (Some(lease.clone()), Some(*server))
                    }
                    DhcpPhase::Discovering => (None, None),
                };
                let address = if reply.offered == Ipv4Addr::UNSPECIFIED {
                    requested
                        .as_ref()
                        .map(|lease| lease.address)
                        .unwrap_or(reply.offered)
                } else {
                    reply.offered
                };
                if address == Ipv4Addr::UNSPECIFIED {
                    return true;
                }
                let lease = DhcpLease {
                    address,
                    prefix_len: reply
                        .subnet_mask
                        .and_then(ipv4_mask_prefix)
                        .or_else(|| requested.as_ref().map(|lease| lease.prefix_len))
                        .unwrap_or(24),
                    router: reply
                        .router
                        .or_else(|| requested.as_ref().and_then(|lease| lease.router)),
                    dns: if reply.dns.is_empty() {
                        requested
                            .as_ref()
                            .map(|lease| lease.dns.clone())
                            .unwrap_or_default()
                    } else {
                        reply.dns
                    },
                    lease_seconds: reply
                        .lease_seconds
                        .or_else(|| requested.as_ref().map(|lease| lease.lease_seconds))
                        .unwrap_or(3600)
                        .max(60),
                };
                let server = reply
                    .server
                    .or(previous_server)
                    .unwrap_or(Ipv4Addr::UNSPECIFIED);
                let old = self.dhcp[index].installed.clone();
                self.replace_dhcp_lease(interface, old.as_ref(), Some(&lease));
                let renew_seconds = reply
                    .renewal_seconds
                    .unwrap_or(lease.lease_seconds / 2)
                    .clamp(1, lease.lease_seconds.saturating_sub(1));
                let renew_ns =
                    now_ns.saturating_add(u64::from(renew_seconds).saturating_mul(1_000_000_000));
                let rebind_seconds = dhcp_rebind_seconds(
                    lease.lease_seconds,
                    renew_seconds,
                    reply.rebinding_seconds,
                );
                let rebind_ns =
                    now_ns.saturating_add(u64::from(rebind_seconds).saturating_mul(1_000_000_000));
                let expires_ns = now_ns
                    .saturating_add(u64::from(lease.lease_seconds).saturating_mul(1_000_000_000));
                self.dhcp[index].installed = Some(lease.clone());
                self.dhcp[index].phase = DhcpPhase::Bound {
                    lease,
                    server,
                    renew_ns,
                    rebind_ns,
                    expires_ns,
                };
                self.dhcp[index].next_action_ns = renew_ns;
                self.dhcp[index].retry_seconds = 1;
            }
            6 => {
                let old = self.dhcp[index].installed.take();
                self.replace_dhcp_lease(interface, old.as_ref(), None);
                self.dhcp[index].phase = DhcpPhase::Discovering;
                self.dhcp[index].next_action_ns = sched::now_ns_public();
                self.dhcp[index].retry_seconds = 1;
            }
            _ => {}
        }
        true
    }

    fn replace_dhcp_lease(
        &self,
        interface: InterfaceId,
        old: Option<&DhcpLease>,
        new: Option<&DhcpLease>,
    ) {
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
                let used_elsewhere = self.dhcp.iter().any(|client| {
                    client.interface != interface
                        && client
                            .installed
                            .as_ref()
                            .is_some_and(|lease| lease.dns.contains(server))
                });
                if !used_elsewhere
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
        &self,
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
            if self
                .control_plane
                .multicast_bindings
                .lock()
                .contains_key(&binding_key)
            {
                return;
            }
            let Some(interface) = self.resolve_multicast_interface(facade, membership, config)
            else {
                facade.set_pending_error(SocketError::NetworkUnreachable);
                return;
            };
            self.control_plane
                .multicast_bindings
                .lock()
                .insert(binding_key, interface);
            let first = {
                let mut refs = self.control_plane.multicast_refs.lock();
                let count = refs.entry((interface, membership.group)).or_insert(0);
                *count += 1;
                *count == 1
            };
            if first {
                self.emit_multicast_control(interface, membership.group, true, config);
            }
            return;
        }
        let Some(interface) = self
            .control_plane
            .multicast_bindings
            .lock()
            .remove(&binding_key)
        else {
            return;
        };
        let last = {
            let mut refs = self.control_plane.multicast_refs.lock();
            let key = (interface, membership.group);
            let Some(count) = refs.get_mut(&key) else {
                return;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                refs.remove(&key);
                true
            } else {
                false
            }
        };
        if last {
            self.emit_multicast_control(interface, membership.group, false, config);
        }
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

    fn emit_interface_multicast_reports(&self, interface: InterfaceId) {
        let groups = self
            .control_plane
            .multicast_refs
            .lock()
            .keys()
            .filter_map(|(candidate, group)| (*candidate == interface).then_some(*group))
            .collect::<Vec<_>>();
        let config = self.config.snapshot();
        for group in groups {
            self.emit_multicast_control(interface, group, true, &config);
        }
    }

    fn remove_interface_multicast(&self, interface: InterfaceId) {
        self.control_plane
            .multicast_bindings
            .lock()
            .retain(|_, bound| *bound != interface);
        self.control_plane
            .multicast_refs
            .lock()
            .retain(|(bound, _), _| *bound != interface);
    }

    fn remove_socket_multicast(&self, socket: net::SocketId) {
        let memberships = self
            .control_plane
            .multicast_bindings
            .lock()
            .keys()
            .filter_map(|(candidate, membership)| (*candidate == socket).then_some(*membership))
            .collect::<Vec<_>>();
        let config = self.config.snapshot();
        for membership in memberships {
            if let Some(interface) = self
                .control_plane
                .multicast_bindings
                .lock()
                .remove(&(socket, membership))
            {
                let mut refs = self.control_plane.multicast_refs.lock();
                let key = (interface, membership.group);
                if let Some(count) = refs.get_mut(&key) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        refs.remove(&key);
                        drop(refs);
                        self.emit_multicast_control(interface, membership.group, false, &config);
                    }
                }
            }
        }
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
                            self.protocol.abort_tcp(flow, sched::now_ns_public());
                        } else {
                            self.protocol.close_tcp(flow, sched::now_ns_public());
                        }
                        self.control_plane.release_binding(facade.id());
                    } else if facade.write_is_shutdown() {
                        self.protocol
                            .shutdown_tcp_write(flow, sched::now_ns_public());
                    }
                    self.dispatch_tcp_output();
                }
                OwnerRef::Flow { shard, flow, .. }
                    if shard == self.runtime.id && facade.is_closing() =>
                {
                    match facade.kind() {
                        SocketKind::Datagram => {
                            self.drain_udp_before_close(&facade, flow, config);
                            self.protocol.close_udp(flow);
                            self.control_plane.release_binding(facade.id());
                        }
                        SocketKind::Raw => self.protocol.close_raw(flow),
                        SocketKind::Stream => unreachable!(),
                    }
                    facade.publish_closed();
                }
                OwnerRef::Listener { group, .. } if facade.is_closing() => {
                    if self.control_plane.listeners.lock().contains_key(&group) {
                        let transaction = Arc::new(ListenerRemove {
                            facade: Arc::clone(&facade),
                            group,
                            remaining: AtomicUsize::new(self.cluster.shards.len()),
                            control: Arc::clone(&self.control_plane),
                        });
                        for runtime in &self.cluster.shards {
                            let mut work = ControlWork::RemoveListener {
                                transaction: Arc::clone(&transaction),
                            };
                            loop {
                                match runtime.control.try_push(work) {
                                    Ok(()) => {
                                        if !runtime.pending.swap(true, Ordering::AcqRel) {
                                            runtime.wake_owner();
                                        }
                                        break;
                                    }
                                    Err(pending) => {
                                        work = pending;
                                        runtime.wake_owner();
                                        let _ = sched::operation::sched_yield();
                                    }
                                }
                            }
                        }
                    } else {
                        self.control_plane.release_binding(facade.id());
                        facade.publish_closed();
                    }
                }
                OwnerRef::Bound { .. } | OwnerRef::Unassigned if facade.is_closing() => {
                    self.control_plane.release_binding(facade.id());
                    facade.publish_closed();
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
                IngressWork::Packet(mut packet) => {
                    let egress = packet.egress;
                    let interface = packet.interface;
                    let local_mac = packet.local_mac;
                    self.observe_ingress_neighbor(interface, &packet.packet);
                    if self.handle_dhcp_packet(interface, &packet.packet) {
                        packet.packet.parsed.disposition =
                            FrontendDisposition::Drop(DropReason::NoConsumer);
                    }
                    self.protocol.push_frontend(packet.packet);
                    for candidate in index + 1..count {
                        let same_source = matches!(
                            self.pending[candidate].as_ref(),
                            Some(IngressWork::Packet(packet))
                                if packet.egress == egress && packet.interface == interface
                        );
                        if !same_source {
                            continue;
                        }
                        let Some(IngressWork::Packet(mut packet)) = self.pending[candidate].take()
                        else {
                            unreachable!();
                        };
                        self.observe_ingress_neighbor(interface, &packet.packet);
                        if self.handle_dhcp_packet(interface, &packet.packet) {
                            packet.packet.parsed.disposition =
                                FrontendDisposition::Drop(DropReason::NoConsumer);
                        }
                        self.protocol.push_frontend(packet.packet);
                    }
                    self.process_packet_batch(egress, interface, local_mac, config);
                }
                IngressWork::LocalTcp {
                    egress,
                    interface,
                    work,
                } => {
                    #[cfg(feature = "performance-profile")]
                    {
                        local_count += 1;
                    }
                    self.process_local_tcp(egress, interface, work, config);
                }
                IngressWork::LocalUdp {
                    egress,
                    interface,
                    work,
                } => {
                    #[cfg(feature = "performance-profile")]
                    {
                        local_count += 1;
                    }
                    self.process_local_udp(egress, interface, work);
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

    fn process_local_tcp(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        work: PreparedTcpTx,
        config: &ConfigSnapshot,
    ) {
        let source = Endpoint {
            addr: work.path.route.source,
            port: work.local_port,
        };
        let key = FlowKey::new(source, work.remote, TransportProtocol::Tcp)
            .expect("TCP local transport tuple 必须有效");
        let path = TcpPath {
            route: net::control::RouteDecision {
                interface,
                source: work.remote.addr,
                next_hop: source.addr,
                mtu: work.path.route.mtu,
                table: work.path.route.table,
            },
            source_mac: work.path.destination_mac,
            destination_mac: work.path.source_mac,
            unresolved_neighbor: None,
            config_generation: config.generation,
        };
        let packet = TcpPacket {
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
        };
        let result = self.protocol.process_local_tcp(
            interface,
            path,
            key,
            packet,
            work.payload.as_ref(),
            sched::now_ns_public(),
        );
        if matches!(result, Err(net::transport::TcpIngressError::NoEndpoint)) {
            self.dispatch_egress(egress, EgressWork::Tcp(work));
        }
        self.dispatch_tcp_output();
    }

    fn process_local_udp(&mut self, egress: usize, interface: InterfaceId, work: PreparedUdpTx) {
        let source = Endpoint {
            addr: work.route.source,
            port: work.source_port,
        };
        let result = self.protocol.process_local_udp(
            interface,
            source,
            work.destination,
            &work.payload,
            work.hop_limit,
            work.traffic_class,
            sched::now_ns_public(),
        );
        match result {
            Ok(_) => work.payload.complete(),
            Err(LocalUdpIngressError::NoEndpoint | LocalUdpIngressError::Unsupported) => {
                self.dispatch_egress(egress, EgressWork::Udp(work));
            }
            Err(LocalUdpIngressError::RingFull) => {}
        }
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
                for state in &mut self.dad {
                    if state.interface == interface && state.address == target {
                        state.conflict = true;
                    }
                }
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
        let now_ns = packet.metadata.rx_timestamp_ns.max(sched::now_ns_public());
        for target in &self.cluster.shards {
            let mut work = ControlWork::NeighborObserved {
                key,
                mac_address,
                now_ns,
            };
            loop {
                match target.control.try_push(work) {
                    Ok(()) => {
                        if !target.pending.swap(true, Ordering::AcqRel) {
                            target.wake_owner();
                        }
                        break;
                    }
                    Err(pending) => {
                        work = pending;
                        target.wake_owner();
                        let _ = sched::operation::sched_yield();
                    }
                }
            }
        }
    }

    fn process_packet_batch(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        config: &ConfigSnapshot,
    ) {
        let Some(target) = self.runtime.egress(egress) else {
            return;
        };
        let stats = Arc::clone(&target.stats);
        loop {
            self.protocol.process_frontend_batch(
                FlowTurnContext {
                    interface,
                    local_mac,
                    config,
                    now_ns: sched::now_ns_public(),
                },
                &mut self.tx,
                &mut self.recycle,
                |reason| {
                    stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    stats.drop_reasons[reason.index()].fetch_add(1, Ordering::Relaxed);
                    if reason == DropReason::NoConsumer {
                        stats.rx_no_consumer.fetch_add(1, Ordering::Relaxed);
                    }
                },
            );
            if !self.protocol.reassembled_input().is_empty() {
                match crate::net_stack::worker_turn(
                    self.protocol.reassembled_input(),
                    interface,
                    config,
                ) {
                    Ok(turn) => self
                        .protocol
                        .parse_reassembled(turn.ethernet(), turn.network()),
                    Err(_) => {
                        while let Some((chain, mut metadata)) =
                            self.protocol.take_unparsed_reassembled()
                        {
                            metadata.drop_reason = DropReason::NoConsumer;
                            stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                            stats.drop_reasons[DropReason::NoConsumer.index()]
                                .fetch_add(1, Ordering::Relaxed);
                            stats.rx_no_consumer.fetch_add(1, Ordering::Relaxed);
                            if let Err(chain) = self.recycle.push(chain, metadata) {
                                drop(chain);
                            }
                        }
                    }
                }
            }
            while let Some((error_interface, error_target, error, now_ns)) =
                self.protocol.take_forwarded_error()
            {
                let owner = match error_target {
                    net::transport::ControlErrorTarget::Flow(flow) => self
                        .control_plane
                        .flow_shard(flow.remote, flow.local, flow.protocol),
                    net::transport::ControlErrorTarget::Raw { .. } => ShardId(0),
                };
                if owner == self.runtime.id {
                    let _ = self.protocol.apply_transport_error(
                        error_interface,
                        error_target,
                        error,
                        now_ns,
                    );
                    continue;
                }
                let mut work = ControlWork::TransportError {
                    interface: error_interface,
                    target: error_target,
                    error,
                    now_ns,
                };
                loop {
                    match self.cluster.publish_control(owner, work) {
                        Ok(()) => break,
                        Err(pending) => {
                            work = pending;
                            let _ = sched::operation::sched_yield();
                        }
                    }
                }
            }
            let mut local = false;
            while let Some(packet) = self.protocol.take_reassembled() {
                let destination = match packet.parsed.disposition {
                    FrontendDisposition::Tcp => self.cluster.ingress_target(packet.parsed.rss_hash),
                    _ => self.cluster.coordinator(),
                };
                if destination.id == self.runtime.id {
                    self.protocol.push_frontend(packet);
                    local = true;
                    continue;
                }
                let work = IngressWork::Packet(IngressPacket {
                    egress,
                    interface,
                    local_mac,
                    packet,
                });
                if let Err(IngressWork::Packet(packet)) = destination.try_push(work) {
                    stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    stats.drop_reasons[DropReason::IngressRingFull.index()]
                        .fetch_add(1, Ordering::Relaxed);
                    drop(packet.packet.chain);
                }
            }
            if !local {
                break;
            }
        }
        self.recycle.clear();
        self.dispatch_tcp_output();
        self.dispatch_tx(egress);
        let protocol_stats = self.protocol.stats();
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
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.observe_udp_probe();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.observe_physical_udp_probe();
    }

    fn dispatch_tx(&mut self, egress: usize) {
        let len = self.tx.len();
        for index in 0..len {
            let Some(packet) = self.tx.take(index) else {
                continue;
            };
            self.dispatch_egress(egress, EgressWork::Packet(packet));
        }
    }

    fn dispatch_tcp_output(&mut self) {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::NetTcpOutput);
        let config = self.config.snapshot();
        let now_ns = sched::now_ns_public();
        let mut resume_budget = 256;
        loop {
            while let Some(mut work) = self.protocol.take_tcp_output() {
                if let Err(error) = self
                    .protocol
                    .refresh_tcp_tx_path(&mut work, &config, now_ns)
                {
                    work.facade.set_pending_error(error);
                    continue;
                }
                if work.path.unresolved_neighbor.is_some() {
                    self.publish_neighbor_work(PendingNeighborTx::Tcp(work));
                    continue;
                }
                let Some(target) = self.runtime.egress_index(work.path.route.interface) else {
                    work.facade
                        .set_pending_error(SocketError::NetworkUnreachable);
                    continue;
                };
                if config.interfaces.iter().any(|interface| {
                    interface.id == work.path.route.interface && interface.loopback
                }) {
                    self.dispatch_local_tcp(target, work);
                    continue;
                }
                self.dispatch_egress(target, EgressWork::Tcp(work));
            }
            if resume_budget == 0 {
                return;
            }
            let resumed = self
                .protocol
                .resume_tcp_output(now_ns, resume_budget.min(32));
            if resumed == 0 {
                return;
            }
            resume_budget -= resumed;
        }
    }

    fn dispatch_local_tcp(&mut self, egress: usize, work: PreparedTcpTx) {
        let source = Endpoint {
            addr: work.path.route.source,
            port: work.local_port,
        };
        let key = FlowKey::new(source, work.remote, TransportProtocol::Tcp)
            .expect("TCP local transport tuple 必须有效");
        let target = self.cluster.local_ingress_target(&key);
        let ingress = IngressWork::LocalTcp {
            egress,
            interface: work.path.route.interface,
            work,
        };
        if target.try_push(ingress).is_err() {
            if let Some(egress) = self.runtime.egress(egress) {
                egress.stats.drop_reasons[DropReason::IngressRingFull.index()]
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn dispatch_egress(&mut self, target: usize, mut work: EgressWork) {
        let local = self
            .local_queue
            .as_ref()
            .is_some_and(|queue| queue.egress_index == target);
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
                    self.pump_local_queue();
                    if self.local_queue.is_none() {
                        fail_egress_work(work, SocketError::NetworkUnreachable);
                        return;
                    }
                }
            }
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
        if self.udp_probe_flow.is_none() {
            let Ok(flow) = self
                .protocol
                .bind_udp(receiver, Some(sender), Some(interface))
            else {
                return;
            };
            self.udp_probe_flow = Some(flow);
        }
        if self.udp_probe_sender.is_none() {
            let Ok(flow) = self
                .protocol
                .bind_udp(sender, Some(receiver), Some(interface))
            else {
                return;
            };
            self.udp_probe_sender = Some(flow);
        }
        let Ok(packet) = self.protocol.form_udp_packet(
            self.udp_probe_sender.unwrap(),
            None,
            payload,
            0,
            config,
            sched::now_ns_public(),
        ) else {
            return;
        };
        if egress.try_push(EgressWork::Packet(packet)).is_err() {
            egress.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            PHYSICAL_UDP_TX_SUBMITTED.store(true, Ordering::Release);
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn observe_udp_probe(&mut self) {
        if UDP_PROBE_COMPLETE.load(Ordering::Acquire) {
            return;
        }
        let Some(flow) = self.udp_probe_flow else {
            return;
        };
        let Some(datagram) = self.protocol.recv_udp(flow) else {
            return;
        };
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
        if self.physical_udp_probe_flow.is_none() {
            let Ok(flow) = self.protocol.bind_udp(receiver, Some(dns), Some(interface)) else {
                return;
            };
            self.physical_udp_probe_flow = Some(flow);
            self.physical_udp_probe_sender = Some(flow);
        }
        let Ok(packet) = self.protocol.form_udp_packet(
            self.physical_udp_probe_sender.unwrap(),
            None,
            payload,
            0,
            config,
            sched::now_ns_public(),
        ) else {
            return;
        };
        if egress.try_push(EgressWork::Packet(packet)).is_err() {
            egress.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn observe_physical_udp_probe(&mut self) {
        if PHYSICAL_UDP_REPLY_SEEN.load(Ordering::Acquire) {
            return;
        }
        let Some(flow) = self.physical_udp_probe_flow else {
            return;
        };
        let Some(datagram) = self.protocol.recv_udp(flow) else {
            return;
        };
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
            drop(datagram);
            PHYSICAL_UDP_REPLY_SEEN.store(true, Ordering::Release);
            for target in self.runtime.egress_snapshot() {
                if let Some(task) = target.task.lock().as_ref().cloned() {
                    let _ = sched::activate_task(&task);
                }
            }
        }
    }

    fn sleep_until_ingress(&mut self) {
        let task = sched::current_task();
        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(sched::WaitReason::Other, sched::now_ns_public());
        if !task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            return;
        }
        fence(Ordering::SeqCst);
        if !self.runtime.ingress.is_empty()
            || !self.runtime.control.is_empty()
            || !self.runtime.dirty.is_empty()
            || !self.runtime.lifecycle.is_empty()
            || self.local_queue_pending()
        {
            self.runtime.pending.store(true, Ordering::Release);
            let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running);
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            return;
        }
        drop(task);
        sched::schedule_once(sched::now_ns_public());
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

pub(crate) fn dhcp_rebind_seconds(
    lease_seconds: u32,
    renew_seconds: u32,
    offered: Option<u32>,
) -> u32 {
    let latest = lease_seconds.saturating_sub(1);
    let earliest = renew_seconds.saturating_add(1).min(latest);
    offered
        .unwrap_or(lease_seconds.saturating_mul(7) / 8)
        .clamp(earliest, latest)
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
    fn run(mut self: Box<Self>) -> ! {
        loop {
            match self.run_turn() {
                WorkerTurn::Removed => {
                    self.finish_removal();
                    drop(self);
                    sched::kthread_finish(sched::ExitCode(0));
                }
                WorkerTurn::Pending => {
                    let _ = sched::operation::sched_yield();
                }
                WorkerTurn::Idle => self.sleep_until_queue_event(),
            }
        }
    }

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
        self.tx_payload_pool.as_mut().unwrap().drain_remote();
        self.refill_rx();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.prepare_arp_probe();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.prepare_udp_probe();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        self.prepare_physical_udp_probe();
        self.drain_egress();
        self.reclaim_tx();
        let turn_start = sched::now_ns_public();
        let mut packet_budget = 128u16;
        let mut byte_budget = 256 * 1024u32;
        while packet_budget != 0
            && byte_budget != 0
            && sched::now_ns_public().saturating_sub(turn_start) < 200_000
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
        if sched::now_ns_public().saturating_sub(turn_start) >= 200_000 {
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
        self.tx_payload_pool.as_mut().unwrap().drain_remote();
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        if ARP_TX_COMPLETED.load(Ordering::Acquire)
            && self.tx_payload_pool.as_ref().unwrap().pool().outstanding() == 0
            && self.tx_payload_pool.as_ref().unwrap().available()
                == self.tx_payload_pool.as_ref().unwrap().pool().capacity()
        {
            ARP_POOL_CONSERVED.store(true, Ordering::Release);
        }
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        if self.arp_probe_enabled
            && PHYSICAL_UDP_TX_SUBMITTED.load(Ordering::Acquire)
            && self.tx_payload_pool.as_ref().unwrap().pool().outstanding() == 0
            && self.tx_payload_pool.as_ref().unwrap().available()
                == self.tx_payload_pool.as_ref().unwrap().pool().capacity()
        {
            PHYSICAL_UDP_POOL_CONSERVED.store(true, Ordering::Release);
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
            || self.has_test_work()
    }

    fn enqueue_rx_batch(&mut self) {
        let config = self.config.snapshot();
        let turn = match crate::net_stack::worker_turn(&self.rx_batch, self.interface, &config) {
            Ok(turn) => turn,
            Err(_) => {
                let len = self.rx_batch.len();
                for index in 0..len {
                    let Some((chain, metadata)) = self.rx_batch.take(index) else {
                        continue;
                    };
                    self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    self.stats.drop_reasons[DropReason::NoConsumer.index()]
                        .fetch_add(1, Ordering::Relaxed);
                    self.stats.rx_no_consumer.fetch_add(1, Ordering::Relaxed);
                    self.recycle_chain(chain, metadata, DropReason::NoConsumer);
                }
                return;
            }
        };
        self.frontend.process_with_stack_sidecars(
            &mut self.rx_batch,
            turn.ethernet(),
            turn.network(),
            &mut self.frontend_batch,
        );
        let len = self.frontend_batch.len();
        let mut published = [false; sched::NR_CPUS];
        for index in 0..len {
            let Some(packet) = self.frontend_batch.take(index) else {
                continue;
            };
            let target = match packet.parsed.disposition {
                FrontendDisposition::Tcp => {
                    self.protocol_cluster.ingress_target(packet.parsed.rss_hash)
                }
                FrontendDisposition::Control(net::pipeline::ControlPacket::Fragment(ip)) => {
                    let fragment = ip
                        .fragment
                        .expect("fragment disposition 必须携带分片 sidecar");
                    let hash = net::flow::fragment_rss_hash(
                        &self.rss_key,
                        self.interface,
                        ip.source,
                        ip.destination,
                        ip.next_header,
                        fragment.identification,
                    );
                    self.protocol_cluster.ingress_target(Some(hash))
                }
                FrontendDisposition::Udp
                | FrontendDisposition::Raw
                | FrontendDisposition::Control(_)
                | FrontendDisposition::Drop(_) => self.protocol_cluster.coordinator(),
            };
            let work = IngressWork::Packet(IngressPacket {
                egress: self.egress_index,
                interface: self.interface,
                local_mac: self.local_mac,
                packet,
            });
            if let Err(IngressWork::Packet(packet)) = target.try_push_deferred(work) {
                self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                self.stats.drop_reasons[DropReason::IngressRingFull.index()]
                    .fetch_add(1, Ordering::Relaxed);
                self.recycle_chain(
                    packet.packet.chain,
                    packet.packet.metadata,
                    DropReason::IngressRingFull,
                );
            } else {
                published[usize::from(target.id.0)] = true;
            }
        }
        for (index, published) in published.into_iter().enumerate() {
            if published {
                let target = &self.protocol_cluster.shards[index];
                if self.inline_protocol {
                    target.pending.store(true, Ordering::Release);
                } else {
                    target.publish_ingress();
                }
            }
        }
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
            EgressWork::Tcp(work) => self.materialize_tcp(work).map_err(EgressWork::Tcp),
            EgressWork::Udp(work) => self.materialize_udp(work).map_err(EgressWork::Udp),
            EgressWork::UdpBatch(work) => self
                .materialize_udp_batch(work)
                .map_err(EgressWork::UdpBatch),
            EgressWork::Raw(work) => self.materialize_raw(work).map_err(EgressWork::Raw),
            EgressWork::ControlFrame(bytes) => {
                self.pending_tx_frames.push_back(PendingTxFrame {
                    bytes,
                    completion: net::buf::CompletionToken(0),
                    facade: None,
                });
                Ok(())
            }
        }
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
            let Ok(mut lease) = self.tx_payload_pool.as_mut().unwrap().lease(
                0,
                frame.bytes.len() as u16,
                PacketMetadata::default(),
            ) else {
                self.pending_tx_frames.push_front(frame);
                break;
            };
            lease
                .as_mut_slice()
                .expect("分片 TX lease 范围有效")
                .copy_from_slice(&frame.bytes);
            let packet = TxPacket {
                chain: PacketChain::from_lease(lease),
                completion: frame.completion,
                low_latency: false,
                checksums_validated: false,
                layout: net::buf::PacketLayout::Plain,
            };
            if let Err(packet) = self.tx_batch.push(packet) {
                self.pending_tx_frames.push_front(PendingTxFrame {
                    bytes: frame.bytes,
                    completion: packet.completion,
                    facade: frame.facade,
                });
                break;
            }
        }
    }

    fn materialize_udp(&mut self, work: PreparedUdpTx) -> Result<(), PreparedUdpTx> {
        let ip_header_len = match work.route.source {
            IpAddr::V4(_) => 20usize,
            IpAddr::V6(_) => 40usize,
        };
        let fragmented =
            ip_header_len + 8 + usize::from(work.payload.len) > work.route.mtu as usize;
        let chain = if fragmented {
            None
        } else {
            let max_fragments = self.max_payload_fragments();
            match allocate_payload_chain(
                self.tx_payload_pool.as_mut().unwrap(),
                usize::from(work.payload.len),
                128,
                max_fragments,
                |offset, output| work.payload.copy_range(offset, output),
            ) {
                Ok(chain) => Some(chain),
                Err(PayloadChainError::Retry) => return Err(work),
                Err(PayloadChainError::Socket(error)) => {
                    work.payload.facade().set_pending_error(error);
                    return Ok(());
                }
            }
        };
        let PreparedUdpTx {
            payload,
            route,
            destination,
            source_port,
            source_mac,
            destination_mac,
            hop_limit,
            traffic_class,
            completion,
            unresolved_neighbor: _,
        } = work;
        let facade = payload.facade();
        if fragmented {
            let mut bytes = alloc::vec![0; usize::from(payload.len)];
            if payload.copy_out(&mut bytes).is_err() {
                facade.set_pending_error(SocketError::Buffer);
                return Ok(());
            }
            payload.complete();
            let identification = self.next_fragment_id;
            self.next_fragment_id = self.next_fragment_id.wrapping_add(1).max(1);
            match build_udp_fragments(
                &bytes,
                route,
                destination,
                source_port,
                source_mac,
                destination_mac,
                hop_limit,
                traffic_class,
                identification,
            ) {
                Ok(frames) => {
                    self.pending_tx_frames
                        .extend(frames.into_iter().map(|bytes| PendingTxFrame {
                            bytes,
                            completion,
                            facade: Some(Arc::clone(&facade)),
                        }));
                    self.drain_pending_tx_frames();
                }
                Err(net::transport::UdpTxError::DatagramTooLarge)
                | Err(net::transport::UdpTxError::MtuExceeded) => {
                    facade.set_pending_error(SocketError::MessageTooLarge)
                }
                Err(_) => facade.set_pending_error(SocketError::Buffer),
            }
            return Ok(());
        }
        payload.complete();
        let packet = match build_udp_packet_with_options(
            chain.expect("未分片 UDP 必须拥有 payload chain"),
            route,
            destination,
            source_port,
            source_mac,
            destination_mac,
            hop_limit,
            traffic_class,
        ) {
            Ok(chain) => TxPacket {
                chain,
                completion,
                low_latency: false,
                checksums_validated: true,
                layout: net::buf::PacketLayout::Plain,
            },
            Err(_) => {
                facade.set_pending_error(SocketError::Buffer);
                return Ok(());
            }
        };
        self.tx_batch
            .push(packet)
            .unwrap_or_else(|_| unreachable!());
        Ok(())
    }

    fn materialize_udp_batch(
        &mut self,
        work: Vec<PreparedUdpTx>,
    ) -> Result<(), Vec<PreparedUdpTx>> {
        let caps = self.queue.as_ref().unwrap().caps();
        if !caps.udp_segmentation
            || work.len() > usize::from(caps.max_udp_segments)
            || work.len() < 2
        {
            return self.materialize_udp_batch_plain(work);
        }
        if self.tx_batch.len() >= 32 {
            return Err(work);
        }

        let payload_len = work[0].payload.len;
        let header_len = match work[0].route.source {
            IpAddr::V4(_) => 42u16,
            IpAddr::V6(_) => 62u16,
        };
        let first = &work[0];
        let route = first.route;
        let destination = first.destination;
        let source_port = first.source_port;
        let source_mac = first.source_mac;
        let destination_mac = first.destination_mac;
        let hop_limit = first.hop_limit;
        let traffic_class = first.traffic_class;
        let completion = first.completion;
        let facade = first.payload.facade();
        let first_fragment = match allocate_payload_chain(
            self.tx_payload_pool.as_mut().unwrap(),
            usize::from(payload_len),
            header_len,
            1,
            |offset, output| first.payload.copy_range(offset, output),
        ) {
            Ok(mut payload) => payload
                .take_fragment(0)
                .expect("单段 UDP payload 必须拥有 fragment"),
            Err(PayloadChainError::Retry) => return Err(work),
            Err(PayloadChainError::Socket(error)) => {
                for item in work {
                    item.payload.facade().set_pending_error(error);
                }
                return Ok(());
            }
        };
        let mut first_chain = PacketChain::new();
        first_chain
            .push(first_fragment)
            .unwrap_or_else(|_| unreachable!());
        let mut chain = match build_udp_packet_with_options(
            first_chain,
            route,
            destination,
            source_port,
            source_mac,
            destination_mac,
            hop_limit,
            traffic_class,
        ) {
            Ok(chain) => chain,
            Err(_) => {
                for item in work {
                    item.payload.complete();
                }
                facade.set_pending_error(SocketError::Buffer);
                return Ok(());
            }
        };
        for item in work.iter().skip(1) {
            let fragment = match allocate_payload_chain(
                self.tx_payload_pool.as_mut().unwrap(),
                usize::from(payload_len),
                0,
                1,
                |offset, output| item.payload.copy_range(offset, output),
            ) {
                Ok(mut payload) => payload
                    .take_fragment(0)
                    .expect("单段 UDP payload 必须拥有 fragment"),
                Err(PayloadChainError::Retry) => return Err(work),
                Err(PayloadChainError::Socket(error)) => {
                    for item in work {
                        item.payload.facade().set_pending_error(error);
                    }
                    return Ok(());
                }
            };
            chain.push(fragment).unwrap_or_else(|_| unreachable!());
        }
        for item in work {
            item.payload.complete();
        }
        let layout = net::buf::UdpSegmentation {
            segment_count: chain.fragment_count() as u8,
            header_len,
            payload_len,
        };
        debug_assert!(layout.validate(chain.fragment_count(), chain.total_len()));
        self.tx_batch
            .push(TxPacket {
                chain,
                completion,
                low_latency: false,
                checksums_validated: true,
                layout: net::buf::PacketLayout::UdpSegments(layout),
            })
            .unwrap_or_else(|_| unreachable!());
        Ok(())
    }

    fn materialize_udp_batch_plain(
        &mut self,
        mut work: Vec<PreparedUdpTx>,
    ) -> Result<(), Vec<PreparedUdpTx>> {
        let available = 32usize.saturating_sub(self.tx_batch.len());
        if work.len() > available {
            return Err(work);
        }
        while !work.is_empty() {
            let item = work.remove(0);
            if let Err(item) = self.materialize_udp(item) {
                work.insert(0, item);
                return Err(work);
            }
        }
        Ok(())
    }

    fn materialize_raw(&mut self, work: PreparedRawTx) -> Result<(), PreparedRawTx> {
        let facade = work.payload.facade();
        if work.header_included && usize::from(work.payload.len) > work.route.mtu as usize {
            let mut bytes = alloc::vec![0; usize::from(work.payload.len)];
            if work.payload.copy_out(&mut bytes).is_err() {
                facade.set_pending_error(SocketError::Buffer);
                return Ok(());
            }
            let completion = work.completion;
            let built = build_header_included_ipv4_fragments(&bytes, &work);
            work.payload.complete();
            match built {
                Ok(frames) => {
                    self.pending_tx_frames
                        .extend(frames.into_iter().map(|bytes| PendingTxFrame {
                            bytes,
                            completion,
                            facade: Some(Arc::clone(&facade)),
                        }));
                    self.drain_pending_tx_frames();
                }
                Err(RawTxError::MtuExceeded | RawTxError::PacketTooLarge) => {
                    facade.set_pending_error(SocketError::MessageTooLarge)
                }
                Err(RawTxError::AddressFamily | RawTxError::InvalidHeader) => {
                    facade.set_pending_error(SocketError::InvalidState)
                }
                Err(RawTxError::Buffer) => facade.set_pending_error(SocketError::Buffer),
            }
            return Ok(());
        }
        let Ok(mut lease) = self.tx_payload_pool.as_mut().unwrap().lease(
            128,
            work.payload.len,
            PacketMetadata::default(),
        ) else {
            return Err(work);
        };
        if work
            .payload
            .copy_out(
                lease
                    .as_mut_slice()
                    .expect("raw socket TX payload lease 范围有效"),
            )
            .is_err()
        {
            facade.set_pending_error(SocketError::Buffer);
            return Ok(());
        }
        let completion = work.completion;
        let built = build_raw_packet(PacketChain::from_lease(lease), &work);
        work.payload.complete();
        let packet = match built {
            Ok(chain) => TxPacket {
                chain,
                completion,
                low_latency: false,
                checksums_validated: false,
                layout: net::buf::PacketLayout::Plain,
            },
            Err((error, _)) => {
                facade.set_pending_error(match error {
                    RawTxError::PacketTooLarge | RawTxError::MtuExceeded => {
                        SocketError::MessageTooLarge
                    }
                    RawTxError::AddressFamily | RawTxError::InvalidHeader => {
                        SocketError::InvalidState
                    }
                    RawTxError::Buffer => SocketError::Buffer,
                });
                return Ok(());
            }
        };
        self.tx_batch
            .push(packet)
            .unwrap_or_else(|_| unreachable!());
        Ok(())
    }

    fn materialize_tcp(&mut self, work: PreparedTcpTx) -> Result<(), PreparedTcpTx> {
        let payload_len = work.payload.as_ref().map_or(0, |payload| payload.len);
        let max_fragments = self.max_payload_fragments();
        let chain = match allocate_payload_chain(
            self.tx_payload_pool.as_mut().unwrap(),
            usize::from(payload_len),
            128,
            max_fragments,
            |offset, output| {
                if let Some(payload) = work.payload.as_ref() {
                    payload.copy_range(offset, output)
                } else {
                    Ok(())
                }
            },
        ) {
            Ok(chain) => chain,
            Err(PayloadChainError::Retry) => return Err(work),
            Err(PayloadChainError::Socket(error)) => {
                work.facade.set_pending_error(error);
                return Ok(());
            }
        };
        let low_latency = work.low_latency;
        let completion = net::buf::CompletionToken(work.completion);
        let chain = match build_tcp_packet(chain, &work) {
            Ok(chain) => chain,
            Err(_) => {
                work.facade.set_pending_error(SocketError::Buffer);
                return Ok(());
            }
        };
        let packet = TxPacket {
            chain,
            completion,
            low_latency,
            checksums_validated: true,
            layout: net::buf::PacketLayout::Plain,
        };
        self.tx_batch
            .push(packet)
            .unwrap_or_else(|_| unreachable!());
        Ok(())
    }

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
        let timestamp = sched::now_ns_public();
        for index in 0..self.rx_batch.len() {
            if let Some(metadata) = self.rx_batch.metadata_mut(index) {
                metadata.ingress_device = self.ingress_device;
                metadata.rx_timestamp_ns = timestamp;
                metadata.rss_generation = self.rss_generation;
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
                .as_mut()
                .unwrap()
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
                    checksums_validated: true,
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
                .as_mut()
                .unwrap()
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
        match self.protocol_cluster.coordinator().try_push(work) {
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
        let Ok(mut lease) = self.tx_payload_pool.as_mut().unwrap().lease(
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
        match self.protocol_cluster.coordinator().try_push(work) {
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
            if let Some(facade) = frame.facade {
                facade.set_pending_error(SocketError::NetworkUnreachable);
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
        self.frontend_batch.clear();
        self.completion_batch.clear();
        self.rx_pool.as_mut().unwrap().begin_dying();
        self.tx_payload_pool.as_mut().unwrap().begin_dying();
        self.tx_header_pool.as_mut().unwrap().begin_dying();
        drop(self.queue.take());
        self.rx_pool.as_mut().unwrap().drain_remote();
        self.tx_payload_pool.as_mut().unwrap().drain_remote();
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

    fn sleep_until_queue_event(&mut self) {
        let task = sched::current_task();
        #[cfg(feature = "performance-profile")]
        task.begin_profile_wait(sched::WaitReason::Other, sched::now_ns_public());
        if !task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            return;
        }
        self.irq.unmask();
        fence(Ordering::SeqCst);
        if self.queue.as_mut().unwrap().has_pending_work()
            || self.egress.has_pending()
            || !self.tx_batch.is_empty()
            || !self.retry_egress.is_empty()
            || !self.pending_tx_frames.is_empty()
            || self.has_test_work()
        {
            let _ = self.irq.ack_and_mask();
            let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running);
            #[cfg(feature = "performance-profile")]
            task.cancel_profile_wait();
            return;
        }
        drop(task);
        sched::schedule_once(sched::now_ns_public());
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
