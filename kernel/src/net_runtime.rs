//! 网络设备接管与 NetWorker 运行时。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

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
    NetDeviceHandle, NetDeviceRegisterError, NetDeviceRegisterErrorKind, NetDeviceRegistrar,
    NetDeviceRegistration, NetDeviceRemoveError, NetDeviceSnapshot, NetDeviceStats,
    NetDeviceTeardown, NetQueueRegistration, NetStat, QueueIrqControl, QueueWakeHandle,
};
use net::queue::{NetQueuePair, RxBudget};
use net::ring::BoundedMpsc;
use net::transport::{PreparedTcpTx, PreparedUdpTx, build_tcp_packet, build_udp_packet};
use net::{
    AddressFamily, Endpoint, FlowId, FlowShard, FlowTurnContext, InterfaceId, IpAddr, Ipv4Addr,
    Ipv6Addr, OwnerRef, ShardId, SocketCommand, SocketError, SocketFacade, SocketKind,
    SocketRuntime, TransportProtocol,
};
use sched::sync::Spinlock;

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static DEVICES: Spinlock<Vec<DeviceRecord>> = Spinlock::new(Vec::new());
static CONFIG_STORE: Spinlock<Option<Arc<ConfigStore>>> = Spinlock::new(None);
static WORKER_STARTS: Spinlock<Vec<Option<Box<WorkerContext>>>> = Spinlock::new(Vec::new());
static PROTOCOL_START: Spinlock<Option<Box<ProtocolContext>>> = Spinlock::new(None);
static PROTOCOL_RUNTIME: Spinlock<Option<Arc<ProtocolRuntime>>> = Spinlock::new(None);
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
    if let Some(runtime) = PROTOCOL_RUNTIME.lock().as_ref() {
        runtime.wake_owner();
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
    if let Some(runtime) = PROTOCOL_RUNTIME.lock().as_ref() {
        runtime.wake_owner();
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
pub fn remove_loopback_for_test() -> Result<(), NetDeviceRemoveError> {
    let handle = DEVICES
        .lock()
        .iter()
        .find(|device| device.snapshot.name.as_ref() == "lo")
        .map(|device| device.handle)
        .ok_or(NetDeviceRemoveError::NoDevice)?;
    REGISTRAR.begin_remove(handle).map(|_| ())
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
    done: AtomicBool,
    worker_count: AtomicUsize,
    completed: AtomicUsize,
    tasks: Spinlock<Vec<Arc<sched::Task>>>,
}

impl WorkerControl {
    fn new() -> Self {
        Self {
            remove_requested: AtomicBool::new(false),
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
        let raw_handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        assert!(raw_handle != 0, "NetDeviceHandle 已耗尽");
        let handle = NetDeviceHandle(raw_handle);
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
        DEVICES.lock().push(DeviceRecord {
            handle,
            snapshot,
            queues: Some(registration.queues),
            queue_stats,
            irqs,
            started: false,
            control: Arc::new(WorkerControl::new()),
        });
        publish_device_config();
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
        let mut devices = DEVICES.lock();
        let Some(index) = devices.iter().position(|device| device.handle == handle) else {
            return Err(NetDeviceRemoveError::NoDevice);
        };
        devices.remove(index);
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

struct TaskWake {
    task: Arc<sched::Task>,
}

struct IngressPacket {
    egress: usize,
    interface: InterfaceId,
    local_mac: [u8; 6],
    packet: PacketChain,
    metadata: PacketMetadata,
}

enum IngressWork {
    Packet(IngressPacket),
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
}

struct EgressChannel {
    interface: InterfaceId,
    ring: BoundedMpsc<EgressWork>,
    pending: AtomicBool,
    active: AtomicBool,
    task: Spinlock<Option<Arc<sched::Task>>>,
    stats: Arc<QueueRuntimeStats>,
}

impl EgressChannel {
    fn new(interface: InterfaceId, stats: Arc<QueueRuntimeStats>) -> Self {
        Self {
            interface,
            ring: BoundedMpsc::new(256),
            pending: AtomicBool::new(false),
            active: AtomicBool::new(true),
            task: Spinlock::new(None),
            stats,
        }
    }

    fn set_task(&self, task: Arc<sched::Task>) {
        *self.task.lock() = Some(task);
    }

    fn try_push(&self, work: EgressWork) -> Result<(), EgressWork> {
        if !self.active.load(Ordering::Acquire) {
            return Err(work);
        }
        self.ring.try_push(work)?;
        if !self.pending.swap(true, Ordering::AcqRel) {
            if let Some(task) = self.task.lock().as_ref().cloned() {
                let _ = sched::activate_task(&task);
            }
        }
        Ok(())
    }

    fn finish_drain(&self) -> bool {
        self.pending.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        if !self.ring.is_empty() {
            self.pending.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) || !self.ring.is_empty()
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        while self.ring.try_pop().is_some() {}
        self.pending.store(false, Ordering::Release);
    }
}

struct ProtocolRuntime {
    ingress: BoundedMpsc<IngressWork>,
    control: BoundedMpsc<SocketCommand>,
    dirty: BoundedMpsc<Arc<SocketFacade>>,
    lifecycle: BoundedMpsc<Arc<SocketFacade>>,
    pending: AtomicBool,
    owner_task: Spinlock<Option<Arc<sched::Task>>>,
    deadline_registration: AtomicU64,
    deadline_ns: AtomicU64,
    timer_fired: AtomicBool,
    egress: Box<[Arc<EgressChannel>]>,
}

impl ProtocolRuntime {
    fn new(egress: Vec<Arc<EgressChannel>>) -> Self {
        Self {
            ingress: BoundedMpsc::new(1024),
            control: BoundedMpsc::new(256),
            dirty: BoundedMpsc::new(4096),
            lifecycle: BoundedMpsc::new(4096),
            pending: AtomicBool::new(false),
            owner_task: Spinlock::new(None),
            deadline_registration: AtomicU64::new(0),
            deadline_ns: AtomicU64::new(0),
            timer_fired: AtomicBool::new(false),
            egress: egress.into_boxed_slice(),
        }
    }

    fn set_owner_task(&self, task: Arc<sched::Task>) {
        *self.owner_task.lock() = Some(task);
    }

    fn wake_owner(&self) {
        if let Some(task) = self.owner_task.lock().as_ref().cloned() {
            let _ = sched::activate_task(&task);
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
        self.ingress.try_push(work)?;
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.wake_owner();
        }
        Ok(())
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
    fn runtime(&self) -> Option<Arc<ProtocolRuntime>> {
        PROTOCOL_RUNTIME.lock().as_ref().cloned()
    }

    fn publish_work(&self, runtime: &ProtocolRuntime) {
        if !runtime.pending.swap(true, Ordering::AcqRel) {
            runtime.wake_owner();
        }
    }
}

impl SocketRuntime for KernelSocketRuntime {
    fn submit_control(&self, command: SocketCommand) -> Result<(), SocketCommand> {
        let Some(runtime) = self.runtime() else {
            return Err(command);
        };
        let mut command = command;
        loop {
            match runtime.control.try_push(command) {
                Ok(()) => {
                    self.publish_work(&runtime);
                    return Ok(());
                }
                Err(pending) => {
                    command = pending;
                    self.publish_work(&runtime);
                    let _ = sched::operation::sched_yield();
                }
            }
        }
    }

    fn notify_tx(&self, facade: Arc<SocketFacade>) {
        let runtime = self.runtime().expect("socket runtime 尚未启动");
        runtime
            .dirty
            .try_push(facade)
            .unwrap_or_else(|_| panic!("socket dirty queue 超出流表上限"));
        self.publish_work(&runtime);
    }

    fn notify_lifecycle(&self, facade: Arc<SocketFacade>) {
        let runtime = self.runtime().expect("socket runtime 尚未启动");
        runtime
            .lifecycle
            .try_push(facade)
            .unwrap_or_else(|_| panic!("socket lifecycle queue 超出流表上限"));
        self.publish_work(&runtime);
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

fn publish_device_config() {
    let store = CONFIG_STORE.lock().as_ref().cloned();
    let Some(store) = store else {
        return;
    };
    let generation = store.snapshot().generation.saturating_add(1);
    let devices = DEVICES.lock();
    store
        .publish(build_device_config(&devices, generation))
        .expect("网络设备配置发布失败");
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
    refill_batch: RxRefillBatch,
    completion_batch: CompletionBatch,
    tx_batch: TxBatch,
    ingress_device: net::NetDeviceId,
    interface: InterfaceId,
    local_mac: [u8; 6],
    protocol_runtime: Arc<ProtocolRuntime>,
    egress_index: usize,
    egress: Arc<EgressChannel>,
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

struct ProtocolContext {
    runtime: Arc<ProtocolRuntime>,
    config: Arc<ConfigStore>,
    protocol: FlowShard,
    input: PacketBatch,
    recycle: PacketBatch,
    tx: TxBatch,
    pending: [Option<IngressWork>; 32],
    bind_registry: BindRegistry,
    bindings: BTreeMap<net::SocketId, BindToken>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_flow: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    udp_probe_sender: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_flow: Option<FlowId>,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    physical_udp_probe_sender: Option<FlowId>,
}

/// 调度器初始化完成后，为启动期接管的每个 queue 创建固定 affinity worker。
pub fn start_workers() {
    struct PendingWorker {
        registration: NetQueueRegistration,
        ingress_device: net::NetDeviceId,
        interface: InterfaceId,
        local_mac: [u8; 6],
        cpu: usize,
        egress: Arc<EgressChannel>,
        control: Arc<WorkerControl>,
        stats: Arc<QueueRuntimeStats>,
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        arp_probe_enabled: bool,
    }

    let online = sched::online_cpu_mask();
    let active_cpus = (0..sched::NR_CPUS)
        .filter(|cpu| online & (1u64 << cpu) != 0)
        .collect::<Vec<_>>();
    assert!(!active_cpus.is_empty(), "NetWorker 没有 active CPU");
    let boot = net::device::boot_config().expect("网络启动配置未安装");
    let mut generation_bytes = [0u8; 4];
    generation_bytes.copy_from_slice(&boot.generation_nonce()[..4]);
    let rss_generation = u32::from_le_bytes(generation_bytes).max(1);

    let mut devices = DEVICES.lock();
    let config = Arc::new(ConfigStore::new(build_device_config(&devices, 1)));
    *CONFIG_STORE.lock() = Some(Arc::clone(&config));
    let mut pending_workers = Vec::new();
    for device in devices.iter_mut().filter(|device| !device.started) {
        let Some(queues) = device.queues.take() else {
            continue;
        };
        for (queue_index, registration) in queues.into_vec().into_iter().enumerate() {
            device.control.worker_count.fetch_add(1, Ordering::Relaxed);
            let cpu = active_cpus[registration.id.0 as usize % active_cpus.len()];
            let interface = InterfaceId(device.snapshot.id.raw());
            let stats = Arc::clone(&device.queue_stats[queue_index]);
            let egress = Arc::new(EgressChannel::new(interface, Arc::clone(&stats)));
            pending_workers.push(PendingWorker {
                registration,
                ingress_device: device.snapshot.id,
                interface,
                local_mac: device.snapshot.mac_address,
                cpu,
                egress,
                control: Arc::clone(&device.control),
                stats,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                arp_probe_enabled: device.snapshot.name.as_ref() != "lo",
            });
        }
        device.started = true;
    }
    drop(devices);

    assert!(!pending_workers.is_empty(), "没有可启动的网络 queue");
    let runtime = Arc::new(ProtocolRuntime::new(
        pending_workers
            .iter()
            .map(|worker| Arc::clone(&worker.egress))
            .collect(),
    ));
    let protocol = FlowShard::new(
        ShardId(0),
        *boot.rss_key(),
        rss_generation,
        *boot.hash_seed(),
        *boot.tcp_isn_key(),
        sched::now_ns_public(),
    );
    *PROTOCOL_START.lock() = Some(Box::new(ProtocolContext {
        runtime: Arc::clone(&runtime),
        config: Arc::clone(&config),
        protocol,
        input: PacketBatch::new(),
        recycle: PacketBatch::new(),
        tx: TxBatch::new(),
        pending: core::array::from_fn(|_| None),
        bind_registry: BindRegistry::new(1, boot.hash_seed()),
        bindings: BTreeMap::new(),
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        udp_probe_flow: None,
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        udp_probe_sender: None,
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        physical_udp_probe_flow: None,
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        physical_udp_probe_sender: None,
    }));
    let protocol_task = sched::kthread_create(
        protocol_worker_entry,
        0,
        sched::SchedParams {
            nice: -5,
            slice_ns: 0,
        },
    );
    protocol_task.set_cpu_affinity(1u64 << active_cpus[0]);
    runtime.set_owner_task(Arc::clone(&protocol_task));

    let mut queue_tasks = Vec::new();
    for (egress_index, pending) in pending_workers.into_iter().enumerate() {
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
        let context = WorkerContext {
            queue: Some(queue),
            rx_pool: Some(rx_pool),
            tx_header_pool: Some(tx_header_pool),
            tx_payload_pool: Some(tx_payload_pool),
            irq: Arc::clone(&irq),
            rx_batch: PacketBatch::new(),
            refill_batch: RxRefillBatch::new(),
            completion_batch: CompletionBatch::new(),
            tx_batch: TxBatch::new(),
            ingress_device: pending.ingress_device,
            interface: pending.interface,
            local_mac: pending.local_mac,
            protocol_runtime: Arc::clone(&runtime),
            egress_index,
            egress: Arc::clone(&egress),
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
        };
        let slot = {
            let mut starts = WORKER_STARTS.lock();
            let slot = starts.len();
            starts.push(Some(Box::new(context)));
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
        queue_tasks.push(task);
    }
    *PROTOCOL_RUNTIME.lock() = Some(runtime);
    net::install_socket_runtime(&SOCKET_RUNTIME_ADAPTER)
        .unwrap_or_else(|_| panic!("socket runtime 重复安装"));
    sched::activate_task(&protocol_task)
        .unwrap_or_else(|error| panic!("协议 worker 启动失败: {:?}", error));
    for task in queue_tasks {
        sched::activate_task(&task)
            .unwrap_or_else(|error| panic!("NetWorker 启动失败: {:?}", error));
    }
}

unsafe extern "C" fn net_worker_entry(slot: usize) -> ! {
    let mut context = WORKER_STARTS
        .lock()
        .get_mut(slot)
        .and_then(Option::take)
        .expect("NetWorker 启动上下文不存在");
    context.run()
}

unsafe extern "C" fn protocol_worker_entry(_unused: usize) -> ! {
    let mut context = PROTOCOL_START
        .lock()
        .take()
        .expect("协议 worker 启动上下文不存在");
    context.run()
}

impl ProtocolContext {
    fn run(&mut self) -> ! {
        loop {
            let config = self.config.snapshot();
            let lifecycle = self.drain_lifecycle(256);
            let control = self.drain_control(256, &config);
            let dirty = self.drain_socket_tx(256, &config);
            let mut processed = 0usize;
            while processed < 128 {
                let count = self.drain_ingress();
                if count == 0 {
                    break;
                }
                processed += count;
                self.process_pending(count, &config);
            }
            self.runtime.timer_fired.store(false, Ordering::Release);
            self.protocol.run_due_timers(sched::now_ns_public());
            self.dispatch_tcp_output();
            self.runtime
                .arm_timer(self.protocol.next_timer_deadline_ns());
            if processed == 128
                || lifecycle == 256
                || control == 256
                || dirty == 256
                || self.runtime.finish_drain()
            {
                let _ = sched::operation::sched_yield();
                continue;
            }
            self.sleep_until_ingress();
        }
    }

    fn drain_control(&mut self, budget: usize, config: &ConfigSnapshot) -> usize {
        let mut processed = 0;
        while processed < budget {
            let Some(command) = self.runtime.control.try_pop() else {
                break;
            };
            processed += 1;
            match command {
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
                        Err(SocketError::Closed)
                    } else {
                        self.listen_facade(&facade, backlog, config)
                    };
                    facade.complete_control(sequence, result);
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
        }
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
        if !address_matches_family(family, local.addr)
            || peer.is_some_and(|peer| !address_matches_family(family, peer.addr))
        {
            return Err(SocketError::AddressUnavailable);
        }
        if !local.addr.is_unspecified()
            && !config.addresses.iter().any(|entry| {
                entry.address == local.addr && interface.is_none_or(|id| id == entry.interface)
            })
        {
            return Err(SocketError::AddressUnavailable);
        }
        let request = BindRequest {
            owner: facade.id().counter,
            family,
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
            self.bind_registry
                .reserve_ephemeral(request, ShardId(0))
                .map_err(map_bind_error)?
        } else {
            self.bind_registry
                .reserve(request)
                .map_err(map_bind_error)?
        };
        local.port = token.port;
        let flow = match self
            .protocol
            .bind_udp_facade(local, peer, interface, Arc::clone(facade))
        {
            Ok(flow) => flow,
            Err(error) => {
                let _ = self.bind_registry.release(token);
                return Err(map_udp_bind_error(error));
            }
        };
        self.bindings.insert(facade.id(), token);
        facade.publish_binding(
            OwnerRef::Flow {
                shard: ShardId(0),
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
        if !address_matches_family(family, local.addr) {
            return Err(SocketError::AddressUnavailable);
        }
        if !local.addr.is_unspecified()
            && !config.addresses.iter().any(|entry| {
                entry.address == local.addr && interface.is_none_or(|id| id == entry.interface)
            })
        {
            return Err(SocketError::AddressUnavailable);
        }
        let request = BindRequest {
            owner: facade.id().counter,
            family,
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
            self.bind_registry
                .reserve_ephemeral(request, ShardId(0))
                .map_err(map_bind_error)?
        } else {
            self.bind_registry
                .reserve(request)
                .map_err(map_bind_error)?
        };
        local.port = token.port;
        self.bindings.insert(facade.id(), token);
        facade.publish_binding(
            OwnerRef::Bound {
                shard: ShardId(0),
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
        if !address_matches_family(facade.family(), peer.addr) {
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
                    shard: ShardId(0),
                    flow,
                    generation: facade.generation(),
                },
                local,
                Some(peer),
                Some(path.route.interface),
            );
            self.dispatch_tcp_output();
            return Ok(false);
        }
        match facade.owner() {
            OwnerRef::Unassigned => {
                let local = Endpoint {
                    addr: unspecified_address(facade.family()),
                    port: 0,
                };
                self.bind_udp_facade(facade, local, Some(peer), interface, options, config)?;
                Ok(true)
            }
            OwnerRef::Flow { flow, .. } => {
                let local = facade.local_endpoint().ok_or(SocketError::InvalidState)?;
                let flow = self
                    .protocol
                    .reconnect_udp_facade(flow, peer, Arc::clone(facade))
                    .map_err(map_udp_bind_error)?;
                facade.publish_binding(
                    OwnerRef::Flow {
                        shard: ShardId(0),
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

    fn listen_facade(
        &mut self,
        facade: &Arc<SocketFacade>,
        backlog: u32,
        config: &ConfigSnapshot,
    ) -> Result<(), SocketError> {
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
        if !matches!(
            facade.owner(),
            OwnerRef::Bound { .. } | OwnerRef::Listener { .. }
        ) {
            return Err(SocketError::InvalidState);
        }
        let local = facade.local_endpoint().ok_or(SocketError::InvalidState)?;
        facade.configure_listener(backlog);
        self.protocol
            .listen_tcp(local, facade.interface(), Arc::clone(facade))
            .map_err(map_tcp_bind_error)?;
        facade.publish_binding(
            OwnerRef::Listener {
                shard: ShardId(0),
                generation: facade.generation(),
            },
            local,
            None,
            facade.interface(),
        );
        Ok(())
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
                    shard: ShardId(0),
                    flow,
                    generation,
                } if generation == facade.generation() => flow,
                _ => {
                    facade.set_pending_error(SocketError::Closed);
                    facade.finish_tx_drain();
                    continue;
                }
            };
            match facade.kind() {
                SocketKind::Stream => {
                    self.protocol.drain_tcp_send(flow, sched::now_ns_public());
                    self.dispatch_tcp_output();
                    facade.finish_stream_tx_drain();
                }
                SocketKind::Datagram => {
                    for _ in 0..32 {
                        let Some(payload) = facade.take_tx() else {
                            break;
                        };
                        match self.protocol.prepare_udp_tx(
                            flow,
                            payload,
                            0,
                            config,
                            sched::now_ns_public(),
                        ) {
                            Ok(work) => {
                                let Some(target) = self
                                    .runtime
                                    .egress
                                    .iter()
                                    .find(|target| target.interface == work.route.interface)
                                else {
                                    work.payload
                                        .facade()
                                        .set_pending_error(SocketError::NetworkUnreachable);
                                    continue;
                                };
                                if let Err(EgressWork::Udp(work)) =
                                    target.try_push(EgressWork::Udp(work))
                                {
                                    work.payload
                                        .facade()
                                        .set_pending_error(SocketError::WouldBlock);
                                }
                            }
                            Err((error, payload)) => {
                                payload.facade().set_pending_error(error);
                            }
                        }
                    }
                }
            }
            if facade.kind() == SocketKind::Datagram {
                facade.finish_tx_drain();
            }
        }
        processed
    }

    fn drain_lifecycle(&mut self, budget: usize) -> usize {
        let mut processed = 0;
        while processed < budget {
            let Some(facade) = self.runtime.lifecycle.try_pop() else {
                break;
            };
            processed += 1;
            match facade.owner() {
                OwnerRef::Flow { flow, .. } if facade.kind() == SocketKind::Stream => {
                    if facade.is_closing() {
                        self.protocol.close_tcp(flow, sched::now_ns_public());
                        if let Some(token) = self.bindings.remove(&facade.id()) {
                            let _ = self.bind_registry.release(token);
                        }
                    } else if facade.write_is_shutdown() {
                        self.protocol
                            .shutdown_tcp_write(flow, sched::now_ns_public());
                    }
                    self.dispatch_tcp_output();
                }
                OwnerRef::Flow { flow, .. } if facade.is_closing() => {
                    self.protocol.close_udp(flow);
                    if let Some(token) = self.bindings.remove(&facade.id()) {
                        let _ = self.bind_registry.release(token);
                    }
                    facade.publish_closed();
                }
                OwnerRef::Listener { .. } if facade.is_closing() => {
                    self.protocol.close_tcp_listener(&facade);
                    if let Some(token) = self.bindings.remove(&facade.id()) {
                        let _ = self.bind_registry.release(token);
                    }
                    facade.publish_closed();
                }
                OwnerRef::Bound { .. } | OwnerRef::Unassigned if facade.is_closing() => {
                    if let Some(token) = self.bindings.remove(&facade.id()) {
                        let _ = self.bind_registry.release(token);
                    }
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
        count
    }

    fn process_pending(&mut self, count: usize, config: &ConfigSnapshot) {
        for index in 0..count {
            let Some(work) = self.pending[index].take() else {
                continue;
            };
            match work {
                IngressWork::Packet(packet) => {
                    let egress = packet.egress;
                    let interface = packet.interface;
                    let local_mac = packet.local_mac;
                    self.input
                        .push(packet.packet, packet.metadata)
                        .unwrap_or_else(|_| unreachable!());
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
                        self.input
                            .push(packet.packet, packet.metadata)
                            .unwrap_or_else(|_| unreachable!());
                    }
                    self.process_packet_batch(egress, interface, local_mac, config);
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
    }

    fn process_packet_batch(
        &mut self,
        egress: usize,
        interface: InterfaceId,
        local_mac: [u8; 6],
        config: &ConfigSnapshot,
    ) {
        let target = Arc::clone(&self.runtime.egress[egress]);
        let stats = Arc::clone(&target.stats);
        self.protocol.process_rx(
            FlowTurnContext {
                interface,
                local_mac,
                config,
                now_ns: sched::now_ns_public(),
            },
            &mut self.input,
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
        let target = &self.runtime.egress[egress];
        let len = self.tx.len();
        for index in 0..len {
            let Some(packet) = self.tx.take(index) else {
                continue;
            };
            if target.try_push(EgressWork::Packet(packet)).is_err() {
                target.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
                target.stats.drop_reasons[DropReason::TxQueueFull.index()]
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn dispatch_tcp_output(&mut self) {
        while let Some(work) = self.protocol.take_tcp_output() {
            let Some(target) = self
                .runtime
                .egress
                .iter()
                .find(|target| target.interface == work.path.route.interface)
            else {
                work.facade
                    .set_pending_error(SocketError::NetworkUnreachable);
                continue;
            };
            if let Err(EgressWork::Tcp(work)) = target.try_push(EgressWork::Tcp(work)) {
                work.facade.set_pending_error(SocketError::WouldBlock);
            }
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    fn start_udp_probe(&mut self, egress: usize, payload: PacketChain, config: &ConfigSnapshot) {
        let interface = self.runtime.egress[egress].interface;
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
        if self.runtime.egress[egress]
            .try_push(EgressWork::Packet(packet))
            .is_err()
        {
            self.runtime.egress[egress]
                .stats
                .tx_dropped
                .fetch_add(1, Ordering::Relaxed);
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
        let interface = self.runtime.egress[egress].interface;
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
        if self.runtime.egress[egress]
            .try_push(EgressWork::Packet(packet))
            .is_err()
        {
            self.runtime.egress[egress]
                .stats
                .tx_dropped
                .fetch_add(1, Ordering::Relaxed);
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
            for target in self.runtime.egress.iter() {
                if let Some(task) = target.task.lock().as_ref().cloned() {
                    let _ = sched::activate_task(&task);
                }
            }
        }
    }

    fn sleep_until_ingress(&self) {
        let task = sched::current_task();
        if !task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            return;
        }
        fence(Ordering::SeqCst);
        if !self.runtime.ingress.is_empty()
            || !self.runtime.control.is_empty()
            || !self.runtime.dirty.is_empty()
            || !self.runtime.lifecycle.is_empty()
        {
            self.runtime.pending.store(true, Ordering::Release);
            let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running);
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

fn unspecified_address(family: AddressFamily) -> IpAddr {
    match family {
        AddressFamily::Ipv4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        AddressFamily::Ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
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
    fn run(&mut self) -> ! {
        loop {
            if self.control.remove_requested.load(Ordering::Acquire) {
                self.shutdown();
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
            self.completion_batch.clear();
            let _reclaimed = self
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
            let turn_start = sched::now_ns_public();
            let mut packet_budget = 128u16;
            let mut byte_budget = 256 * 1024u32;
            while packet_budget != 0
                && byte_budget != 0
                && sched::now_ns_public().saturating_sub(turn_start) < 200_000
            {
                self.rx_batch.clear();
                let result = self.queue.as_mut().unwrap().poll_rx_batch(
                    RxBudget {
                        packets: packet_budget.min(32),
                        bytes: byte_budget,
                    },
                    &mut self.rx_batch,
                );
                self.complete_rx_metadata();
                self.record_rx_result(&result);
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                for index in 0..self.rx_batch.len() {
                    if let Some(packet) = self.rx_batch.packet(index) {
                        self.observe_arp_reply(packet);
                    }
                }
                self.enqueue_rx_batch();
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
            self.submit_tx();
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

            if self.queue.as_mut().unwrap().has_pending_work()
                || self.egress.has_pending()
                || !self.tx_batch.is_empty()
                || self.has_test_work()
            {
                let _ = sched::operation::sched_yield();
                continue;
            }
            self.sleep_until_queue_event();
        }
    }

    fn enqueue_rx_batch(&mut self) {
        let len = self.rx_batch.len();
        for index in 0..len {
            let Some((packet, metadata)) = self.rx_batch.take(index) else {
                continue;
            };
            let work = IngressWork::Packet(IngressPacket {
                egress: self.egress_index,
                interface: self.interface,
                local_mac: self.local_mac,
                packet,
                metadata,
            });
            if let Err(IngressWork::Packet(packet)) = self.protocol_runtime.try_push(work) {
                self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                self.stats.drop_reasons[DropReason::IngressRingFull.index()]
                    .fetch_add(1, Ordering::Relaxed);
                self.recycle_chain(packet.packet, packet.metadata, DropReason::IngressRingFull);
            }
        }
    }

    fn drain_egress(&mut self) {
        while self.tx_batch.len() < 32 {
            let Some(work) = self.egress.ring.try_pop() else {
                break;
            };
            match work {
                EgressWork::Packet(packet) => self
                    .tx_batch
                    .push(packet)
                    .unwrap_or_else(|_| unreachable!()),
                EgressWork::Tcp(work) => self.materialize_tcp(work),
                EgressWork::Udp(work) => self.materialize_udp(work),
            }
        }
        let _ = self.egress.finish_drain();
    }

    fn materialize_udp(&mut self, work: PreparedUdpTx) {
        let PreparedUdpTx {
            payload,
            route,
            destination,
            source_port,
            source_mac,
            destination_mac,
            completion,
        } = work;
        let facade = payload.facade();
        let Ok(mut lease) = self.tx_payload_pool.as_mut().unwrap().lease(
            128,
            payload.len,
            PacketMetadata::default(),
        ) else {
            facade.set_pending_error(SocketError::WouldBlock);
            return;
        };
        let Ok(_) = payload.copy_out(
            lease
                .as_mut_slice()
                .expect("socket TX payload lease 范围有效"),
        ) else {
            facade.set_pending_error(SocketError::Buffer);
            return;
        };
        payload.complete();
        let packet = match build_udp_packet(
            PacketChain::from_lease(lease),
            route,
            destination,
            source_port,
            source_mac,
            destination_mac,
        ) {
            Ok(chain) => TxPacket {
                chain,
                completion,
                low_latency: false,
            },
            Err(_) => {
                facade.set_pending_error(SocketError::Buffer);
                return;
            }
        };
        if self.tx_batch.push(packet).is_err() {
            facade.set_pending_error(SocketError::WouldBlock);
        }
    }

    fn materialize_tcp(&mut self, work: PreparedTcpTx) {
        let payload_len = work.payload.as_ref().map_or(0, |payload| payload.len);
        let Ok(mut lease) = self.tx_payload_pool.as_mut().unwrap().lease(
            128,
            payload_len,
            PacketMetadata::default(),
        ) else {
            work.facade.set_pending_error(SocketError::WouldBlock);
            return;
        };
        if let Some(payload) = work.payload.as_ref()
            && payload
                .copy_out(
                    lease
                        .as_mut_slice()
                        .expect("TCP payload lease 范围必须有效"),
                )
                .is_err()
        {
            work.facade.set_pending_error(SocketError::Buffer);
            return;
        }
        let low_latency = work.low_latency;
        let completion = net::buf::CompletionToken(work.completion);
        let chain = match build_tcp_packet(PacketChain::from_lease(lease), &work) {
            Ok(chain) => chain,
            Err(_) => {
                work.facade.set_pending_error(SocketError::Buffer);
                return;
            }
        };
        let packet = TxPacket {
            chain,
            completion,
            low_latency,
        };
        if self.tx_batch.push(packet).is_err() {
            work.facade.set_pending_error(SocketError::WouldBlock);
        }
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

    fn recycle_rx_batch(&mut self, reason: DropReason) {
        let len = self.rx_batch.len();
        for index in 0..len {
            let Some((mut packet, mut metadata)) = self.rx_batch.take(index) else {
                continue;
            };
            metadata.drop_reason = reason;
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            self.stats.drop_reasons[reason.index()].fetch_add(1, Ordering::Relaxed);
            let fragments = packet.fragment_count();
            for fragment_index in 0..fragments {
                match packet.take_fragment(fragment_index) {
                    Some(PacketFragment::Exclusive(mut lease)) => {
                        *lease.metadata_mut() = metadata;
                        let _ = self.rx_pool.as_mut().unwrap().recycle_local_or_defer(lease);
                    }
                    Some(PacketFragment::Shared(chunk)) => drop(chunk),
                    None => {}
                }
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
        match self.protocol_runtime.try_push(work) {
            Ok(()) => self.udp_probe_queued = true,
            Err(IngressWork::UdpProbe { payload, .. }) => {
                self.recycle_chain(
                    payload,
                    PacketMetadata::default(),
                    DropReason::IngressRingFull,
                );
            }
            Err(IngressWork::Packet(_)) => unreachable!(),
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
        match self.protocol_runtime.try_push(work) {
            Ok(()) => self.physical_udp_probe_queued = true,
            Err(IngressWork::PhysicalUdpProbe { payload, .. }) => {
                self.recycle_chain(
                    payload,
                    PacketMetadata::default(),
                    DropReason::IngressRingFull,
                );
            }
            Err(IngressWork::Packet(_) | IngressWork::UdpProbe { .. }) => unreachable!(),
        }
    }

    fn submit_tx(&mut self) {
        if self.tx_batch.is_empty() {
            return;
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
    }

    /// detach 前停止 IRQ、排空可观察 completion，并在释放 queue 前回收所有 batch lease。
    fn shutdown(&mut self) -> ! {
        let _ = self.irq.ack_and_mask();
        self.egress.deactivate();
        if self.queue.is_some() {
            self.queue.as_mut().unwrap().quiesce().ok();
            for _ in 0..64 {
                let (result, pending) = {
                    let queue = self.queue.as_mut().unwrap();
                    self.completion_batch.clear();
                    let _ = queue.reclaim_tx_batch(&mut self.completion_batch);
                    self.rx_batch.clear();
                    let result = queue.poll_rx_batch(
                        RxBudget {
                            packets: 32,
                            bytes: 256 * 1024,
                        },
                        &mut self.rx_batch,
                    );
                    let pending = queue.has_pending_work();
                    (result, pending)
                };
                self.complete_rx_metadata();
                self.recycle_rx_batch(DropReason::DeviceGone);
                if !pending && result.packets == 0 {
                    break;
                }
            }
        }
        for index in 0..self.tx_batch.len() {
            let _ = self.tx_batch.take(index);
        }
        for index in 0..self.refill_batch.len() {
            if let Some(lease) = self.refill_batch.take(index) {
                let _ = self.rx_pool.as_mut().unwrap().recycle_local(lease);
            }
        }
        self.rx_batch.clear();
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
        sched::kthread_finish(sched::ExitCode(0));
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
        if !task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            return;
        }
        self.irq.unmask();
        fence(Ordering::SeqCst);
        if self.queue.as_mut().unwrap().has_pending_work()
            || self.egress.has_pending()
            || !self.tx_batch.is_empty()
            || self.has_test_work()
        {
            let _ = self.irq.ack_and_mask();
            let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running);
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
