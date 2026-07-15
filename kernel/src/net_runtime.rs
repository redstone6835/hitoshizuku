//! 网络设备接管与 NetWorker 运行时。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

use net::buf::{
    CompletionBatch, DropReason, NetBufPoolOwner, PacketBatch, PacketFragment, PacketMetadata,
    RxRefillBatch, TxBatch,
};
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
use net::buf::{CompletionToken, PacketChain, TxPacket};
use net::device::{
    NetDeviceHandle, NetDeviceRegisterError, NetDeviceRegisterErrorKind, NetDeviceRegistrar,
    NetDeviceRegistration, NetDeviceRemoveError, NetDeviceSnapshot, NetDeviceStats,
    NetDeviceTeardown, NetQueueRegistration, NetStat, QueueIrqControl, QueueWakeHandle,
};
use net::queue::{NetQueuePair, RxBudget};
use sched::sync::Spinlock;

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static DEVICES: Spinlock<Vec<DeviceRecord>> = Spinlock::new(Vec::new());
static WORKER_STARTS: Spinlock<Vec<Option<Box<WorkerContext>>>> = Spinlock::new(Vec::new());
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
static WORKER_TASKS: Spinlock<Vec<Arc<sched::Task>>> = Spinlock::new(Vec::new());
static REGISTRAR: KernelNetRegistrar = KernelNetRegistrar;
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
    rss_generation: u32,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    arp_probe_enabled: bool,
    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    arp_probe_done: bool,
    control: Arc<WorkerControl>,
    rx_no_consumer: u64,
    stats: Arc<QueueRuntimeStats>,
}

/// 调度器初始化完成后，为启动期接管的每个 queue 创建固定 affinity worker。
pub fn start_workers() {
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
    for device in devices.iter_mut().filter(|device| !device.started) {
        let Some(queues) = device.queues.take() else {
            continue;
        };
        for (queue_index, registration) in queues.into_vec().into_iter().enumerate() {
            device.control.worker_count.fetch_add(1, Ordering::Relaxed);
            let cpu = active_cpus[registration.id.0 as usize % active_cpus.len()];
            let irq = Arc::clone(&registration.irq);
            let context = WorkerContext {
                queue: Some(registration.queue),
                rx_pool: Some(registration.rx_pool),
                tx_header_pool: Some(registration.tx_header_pool),
                tx_payload_pool: Some(registration.tx_payload_pool),
                irq,
                rx_batch: PacketBatch::new(),
                refill_batch: RxRefillBatch::new(),
                completion_batch: CompletionBatch::new(),
                tx_batch: TxBatch::new(),
                ingress_device: device.snapshot.id,
                rss_generation,
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                arp_probe_enabled: device.snapshot.name.as_ref() != "lo",
                #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
                arp_probe_done: false,
                control: Arc::clone(&device.control),
                rx_no_consumer: 0,
                stats: Arc::clone(&device.queue_stats[queue_index]),
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
            task.set_cpu_affinity(1u64 << cpu);
            device.control.tasks.lock().push(Arc::clone(&task));
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            WORKER_TASKS.lock().push(Arc::clone(&task));
            WORKER_STARTS.lock()[slot]
                .as_ref()
                .expect("NetWorker 启动上下文丢失")
                .irq
                .set_waker(Arc::new(TaskWake {
                    task: Arc::clone(&task),
                }))
                .unwrap_or_else(|error| panic!("NetWorker waker 安装失败: {:?}", error));
            sched::activate_task(&task)
                .unwrap_or_else(|error| panic!("NetWorker 启动失败: {:?}", error));
        }
        device.started = true;
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
            self.submit_tx();
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
                self.consume_unhandled_rx();
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
            let pool_stats = self.rx_pool.as_ref().unwrap().pool().stats();
            self.stats
                .pool_local_recycle
                .store(pool_stats.local_recycle, Ordering::Relaxed);
            self.stats
                .pool_remote_recycle
                .store(pool_stats.remote_recycle, Ordering::Relaxed);
            self.refill_rx();

            if self.queue.as_mut().unwrap().has_pending_work() {
                let _ = sched::operation::sched_yield();
                continue;
            }
            self.sleep_until_queue_event();
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

    fn consume_unhandled_rx(&mut self) {
        let len = self.rx_batch.len();
        for index in 0..len {
            let Some((mut packet, mut metadata)) = self.rx_batch.take(index) else {
                continue;
            };
            #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
            self.observe_arp_reply(&packet);
            metadata.drop_reason = DropReason::NoConsumer;
            self.rx_no_consumer = self.rx_no_consumer.saturating_add(1);
            self.stats.rx_no_consumer.fetch_add(1, Ordering::Relaxed);
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
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
        frame[38..42].copy_from_slice(&[10, 0, 2, 2]);
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
        #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
        if result.packets != 0 {
            ARP_PROBE_SENT.store(true, Ordering::Release);
        }
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
                self.consume_unhandled_rx();
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
            || frame[28..32] != [10, 0, 2, 2]
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
        if self.queue.as_mut().unwrap().has_pending_work() {
            let _ = self.irq.ack_and_mask();
            let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running);
            return;
        }
        drop(task);
        sched::schedule_once(sched::now_ns_public());
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
