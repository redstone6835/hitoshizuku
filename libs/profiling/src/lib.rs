#![no_std]
//! 固定内存、低侵入的内核性能剖析原语。

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub const MAX_CPUS: usize = 8;
pub const HISTOGRAM_BUCKETS: usize = 32;
pub const SAMPLE_SLOTS: usize = 4096;
const SAMPLE_PROBES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Event {
    SysSendCopy = 0,
    SysSendSocket,
    SysRecvSocket,
    SysRecvCopy,
    NetProtocolTurn,
    NetProtocolIngress,
    NetTcpOutput,
    NetEgressBackpressure,
    NetWorkerTurn,
    NetTxMaterialize,
    NetChecksum,
    NetVirtioSubmit,
    NetVirtioReclaim,
    SchedYield,
    SchedSwitch,
    WaitSocketRead,
    WaitSocketWrite,
    WaitPoll,
    WaitMutex,
    WaitFutex,
    WaitTimer,
    WaitYield,
    WaitOther,
    WakeupLatency,
}

impl Event {
    pub const ALL: [Self; 24] = [
        Self::SysSendCopy,
        Self::SysSendSocket,
        Self::SysRecvSocket,
        Self::SysRecvCopy,
        Self::NetProtocolTurn,
        Self::NetProtocolIngress,
        Self::NetTcpOutput,
        Self::NetEgressBackpressure,
        Self::NetWorkerTurn,
        Self::NetTxMaterialize,
        Self::NetChecksum,
        Self::NetVirtioSubmit,
        Self::NetVirtioReclaim,
        Self::SchedYield,
        Self::SchedSwitch,
        Self::WaitSocketRead,
        Self::WaitSocketWrite,
        Self::WaitPoll,
        Self::WaitMutex,
        Self::WaitFutex,
        Self::WaitTimer,
        Self::WaitYield,
        Self::WaitOther,
        Self::WakeupLatency,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::SysSendCopy => "sys_send_copy",
            Self::SysSendSocket => "sys_send_socket",
            Self::SysRecvSocket => "sys_recv_socket",
            Self::SysRecvCopy => "sys_recv_copy",
            Self::NetProtocolTurn => "net_protocol_turn",
            Self::NetProtocolIngress => "net_protocol_ingress",
            Self::NetTcpOutput => "net_tcp_output",
            Self::NetEgressBackpressure => "net_egress_backpressure",
            Self::NetWorkerTurn => "net_worker_turn",
            Self::NetTxMaterialize => "net_tx_materialize",
            Self::NetChecksum => "net_checksum",
            Self::NetVirtioSubmit => "net_virtio_submit",
            Self::NetVirtioReclaim => "net_virtio_reclaim",
            Self::SchedYield => "sched_yield_delay",
            Self::SchedSwitch => "sched_switch",
            Self::WaitSocketRead => "wait_socket_read",
            Self::WaitSocketWrite => "wait_socket_write",
            Self::WaitPoll => "wait_poll",
            Self::WaitMutex => "wait_mutex",
            Self::WaitFutex => "wait_futex",
            Self::WaitTimer => "wait_timer",
            Self::WaitYield => "wait_yield",
            Self::WaitOther => "wait_other",
            Self::WakeupLatency => "wakeup_latency",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Metric {
    UdpTxQueueDepth = 0,
    IngressRingDepth,
    DirtyDrainSockets,
    SocketDrainDatagrams,
    LocalWorkBatchSize,
    RxRingDepth,
    SocketWakeup,
    SocketEmptyWakeup,
    PayloadCopyBytes,
    PayloadCopyCycles,
}

impl Metric {
    pub const ALL: [Self; 10] = [
        Self::UdpTxQueueDepth,
        Self::IngressRingDepth,
        Self::DirtyDrainSockets,
        Self::SocketDrainDatagrams,
        Self::LocalWorkBatchSize,
        Self::RxRingDepth,
        Self::SocketWakeup,
        Self::SocketEmptyWakeup,
        Self::PayloadCopyBytes,
        Self::PayloadCopyCycles,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::UdpTxQueueDepth => "udp_tx_queue_depth",
            Self::IngressRingDepth => "ingress_ring_depth",
            Self::DirtyDrainSockets => "dirty_drain_sockets",
            Self::SocketDrainDatagrams => "socket_drain_datagrams",
            Self::LocalWorkBatchSize => "local_work_batch_size",
            Self::RxRingDepth => "rx_ring_depth",
            Self::SocketWakeup => "socket_wakeup",
            Self::SocketEmptyWakeup => "socket_empty_wakeup",
            Self::PayloadCopyBytes => "payload_copy_bytes",
            Self::PayloadCopyCycles => "payload_copy_cycles",
        }
    }
}

const EVENT_COUNT: usize = Event::ALL.len();
const METRIC_COUNT: usize = Metric::ALL.len();

struct Histogram {
    buckets: [AtomicU64; HISTOGRAM_BUCKETS],
}

impl Histogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; HISTOGRAM_BUCKETS],
        }
    }

    fn observe(&self, value: u64) {
        self.buckets[histogram_bucket(value)].fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        for bucket in &self.buckets {
            bucket.store(0, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> [u64; HISTOGRAM_BUCKETS] {
        core::array::from_fn(|index| self.buckets[index].load(Ordering::Relaxed))
    }
}

struct Counter {
    calls: AtomicU64,
    cycles: AtomicU64,
    bytes: AtomicU64,
    packets: AtomicU64,
    max_cycles: AtomicU64,
    wall_ns: AtomicU64,
    on_cpu_ns: AtomicU64,
    off_cpu_ns: AtomicU64,
    max_latency_ns: AtomicU64,
    latency: Histogram,
}

impl Counter {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            max_cycles: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
            on_cpu_ns: AtomicU64::new(0),
            off_cpu_ns: AtomicU64::new(0),
            max_latency_ns: AtomicU64::new(0),
            latency: Histogram::new(),
        }
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.cycles.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.packets.store(0, Ordering::Relaxed);
        self.max_cycles.store(0, Ordering::Relaxed);
        self.wall_ns.store(0, Ordering::Relaxed);
        self.on_cpu_ns.store(0, Ordering::Relaxed);
        self.off_cpu_ns.store(0, Ordering::Relaxed);
        self.max_latency_ns.store(0, Ordering::Relaxed);
        self.latency.reset();
    }
}

struct MetricCounter {
    observations: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
    values: Histogram,
}

impl MetricCounter {
    const fn new() -> Self {
        Self {
            observations: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
            values: Histogram::new(),
        }
    }

    fn reset(&self) {
        self.observations.store(0, Ordering::Relaxed);
        self.sum.store(0, Ordering::Relaxed);
        self.max.store(0, Ordering::Relaxed);
        self.values.reset();
    }
}

struct SampleSlot {
    /// PC 的最低位保存用户态标志；指令地址至少 2 字节对齐。
    key: AtomicUsize,
    samples: AtomicU64,
}

impl SampleSlot {
    const fn new() -> Self {
        Self {
            key: AtomicUsize::new(0),
            samples: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.samples.store(0, Ordering::Relaxed);
        self.key.store(0, Ordering::Relaxed);
    }
}

static COUNTERS: [[Counter; EVENT_COUNT]; MAX_CPUS] =
    [const { [const { Counter::new() }; EVENT_COUNT] }; MAX_CPUS];
static METRICS: [[MetricCounter; METRIC_COUNT]; MAX_CPUS] =
    [const { [const { MetricCounter::new() }; METRIC_COUNT] }; MAX_CPUS];
static SAMPLES: [[SampleSlot; SAMPLE_SLOTS]; MAX_CPUS] =
    [const { [const { SampleSlot::new() }; SAMPLE_SLOTS] }; MAX_CPUS];
static DROPPED_SAMPLES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

static ENABLED: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(1);
static COUNTER_HZ: AtomicU64 = AtomicU64::new(0);
static READ_COUNTER: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TASK_CPU_NS: AtomicUsize = AtomicUsize::new(0);

pub fn install(
    read_counter: fn() -> u64,
    current_cpu: fn() -> usize,
    current_task_cpu_ns: fn() -> u64,
    counter_hz: u64,
) {
    READ_COUNTER.store(read_counter as usize, Ordering::Release);
    CURRENT_CPU.store(current_cpu as usize, Ordering::Release);
    CURRENT_TASK_CPU_NS.store(current_task_cpu_ns as usize, Ordering::Release);
    COUNTER_HZ.store(counter_hz, Ordering::Release);
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub fn counter_hz() -> u64 {
    COUNTER_HZ.load(Ordering::Acquire)
}

pub fn reset() {
    let was_enabled = ENABLED.swap(false, Ordering::AcqRel);
    for cpu in 0..MAX_CPUS {
        for counter in &COUNTERS[cpu] {
            counter.reset();
        }
        for metric in &METRICS[cpu] {
            metric.reset();
        }
        for slot in &SAMPLES[cpu] {
            slot.reset();
        }
        DROPPED_SAMPLES[cpu].store(0, Ordering::Relaxed);
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
    ENABLED.store(was_enabled, Ordering::Release);
}

fn installed_fn(raw: &AtomicUsize) -> usize {
    raw.load(Ordering::Acquire)
}

pub fn read_counter() -> u64 {
    let raw = installed_fn(&READ_COUNTER);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let read: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    read()
}

fn current_cpu() -> usize {
    let raw = installed_fn(&CURRENT_CPU);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let current: fn() -> usize = unsafe { core::mem::transmute(raw) };
    current().min(MAX_CPUS - 1)
}

fn current_task_cpu_ns() -> u64 {
    let raw = installed_fn(&CURRENT_TASK_CPU_NS);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let current: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    current()
}

pub struct Scope {
    event: Event,
    start_cycles: u64,
    start_on_cpu_ns: u64,
    bytes: u64,
    packets: u64,
    active: bool,
    generation: u64,
}

impl Scope {
    pub fn bytes(mut self, bytes: usize) -> Self {
        self.bytes = bytes as u64;
        self
    }

    pub fn packets(mut self, packets: usize) -> Self {
        self.packets = packets as u64;
        self
    }

    pub fn set_bytes(&mut self, bytes: usize) {
        self.bytes = bytes as u64;
    }

    pub fn set_packets(&mut self, packets: usize) {
        self.packets = packets as u64;
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if !self.active || self.generation != generation() {
            return;
        }
        let on_cpu_ns = current_task_cpu_ns().saturating_sub(self.start_on_cpu_ns);
        let cycles = read_counter().wrapping_sub(self.start_cycles);
        record_scope(self.event, cycles, on_cpu_ns, self.bytes, self.packets);
    }
}

pub fn scope(event: Event) -> Scope {
    let generation = generation();
    let active = enabled() && installed_fn(&READ_COUNTER) != 0;
    Scope {
        event,
        start_cycles: if active { read_counter() } else { 0 },
        start_on_cpu_ns: if active { current_task_cpu_ns() } else { 0 },
        bytes: 0,
        packets: 0,
        active,
        generation,
    }
}

fn cycles_to_ns(cycles: u64) -> u64 {
    let hz = counter_hz();
    if hz == 0 {
        return 0;
    }
    let seconds = cycles / hz;
    let remainder = cycles % hz;
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(remainder.saturating_mul(1_000_000_000) / hz)
}

fn record_scope(event: Event, cycles: u64, on_cpu_ns: u64, bytes: u64, packets: u64) {
    if !enabled() {
        return;
    }
    let wall_ns = cycles_to_ns(cycles);
    let on_cpu_ns = on_cpu_ns.min(wall_ns);
    let counter = &COUNTERS[current_cpu()][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.cycles.fetch_add(cycles, Ordering::Relaxed);
    counter.bytes.fetch_add(bytes, Ordering::Relaxed);
    counter.packets.fetch_add(packets, Ordering::Relaxed);
    counter.max_cycles.fetch_max(cycles, Ordering::Relaxed);
    counter.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
    counter.on_cpu_ns.fetch_add(on_cpu_ns, Ordering::Relaxed);
    counter
        .off_cpu_ns
        .fetch_add(wall_ns.saturating_sub(on_cpu_ns), Ordering::Relaxed);
    counter.max_latency_ns.fetch_max(wall_ns, Ordering::Relaxed);
    counter.latency.observe(wall_ns);
}

/// 记录已有调用点的 cycle 统计。该接口把时间视为纯 on-CPU。
pub fn record(event: Event, cycles: u64, bytes: u64, packets: u64) {
    if !enabled() {
        return;
    }
    let ns = cycles_to_ns(cycles);
    let counter = &COUNTERS[current_cpu()][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.cycles.fetch_add(cycles, Ordering::Relaxed);
    counter.bytes.fetch_add(bytes, Ordering::Relaxed);
    counter.packets.fetch_add(packets, Ordering::Relaxed);
    counter.max_cycles.fetch_max(cycles, Ordering::Relaxed);
    counter.wall_ns.fetch_add(ns, Ordering::Relaxed);
    counter.on_cpu_ns.fetch_add(ns, Ordering::Relaxed);
    if ns != 0 {
        counter.max_latency_ns.fetch_max(ns, Ordering::Relaxed);
        counter.latency.observe(ns);
    }
}

/// 记录阻塞或唤醒延迟，单位为纳秒。
pub fn record_duration(event: Event, duration_ns: u64) {
    if !enabled() {
        return;
    }
    let counter = &COUNTERS[current_cpu()][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.wall_ns.fetch_add(duration_ns, Ordering::Relaxed);
    counter.off_cpu_ns.fetch_add(duration_ns, Ordering::Relaxed);
    counter
        .max_latency_ns
        .fetch_max(duration_ns, Ordering::Relaxed);
    counter.latency.observe(duration_ns);
}

pub fn observe(metric: Metric, value: u64) {
    if !enabled() {
        return;
    }
    let counter = &METRICS[current_cpu()][metric as usize];
    counter.observations.fetch_add(1, Ordering::Relaxed);
    counter.sum.fetch_add(value, Ordering::Relaxed);
    counter.max.fetch_max(value, Ordering::Relaxed);
    counter.values.observe(value);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub calls: u64,
    pub cycles: u64,
    pub bytes: u64,
    pub packets: u64,
    pub max_cycles: u64,
    pub wall_ns: u64,
    pub on_cpu_ns: u64,
    pub off_cpu_ns: u64,
    pub max_latency_ns: u64,
    pub latency: [u64; HISTOGRAM_BUCKETS],
}

pub fn snapshot(cpu: usize, event: Event) -> Snapshot {
    if cpu >= MAX_CPUS {
        return Snapshot::default();
    }
    let counter = &COUNTERS[cpu][event as usize];
    Snapshot {
        calls: counter.calls.load(Ordering::Relaxed),
        cycles: counter.cycles.load(Ordering::Relaxed),
        bytes: counter.bytes.load(Ordering::Relaxed),
        packets: counter.packets.load(Ordering::Relaxed),
        max_cycles: counter.max_cycles.load(Ordering::Relaxed),
        wall_ns: counter.wall_ns.load(Ordering::Relaxed),
        on_cpu_ns: counter.on_cpu_ns.load(Ordering::Relaxed),
        off_cpu_ns: counter.off_cpu_ns.load(Ordering::Relaxed),
        max_latency_ns: counter.max_latency_ns.load(Ordering::Relaxed),
        latency: counter.latency.snapshot(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetricSnapshot {
    pub observations: u64,
    pub sum: u64,
    pub max: u64,
    pub values: [u64; HISTOGRAM_BUCKETS],
}

pub fn metric_snapshot(cpu: usize, metric: Metric) -> MetricSnapshot {
    if cpu >= MAX_CPUS {
        return MetricSnapshot::default();
    }
    let counter = &METRICS[cpu][metric as usize];
    MetricSnapshot {
        observations: counter.observations.load(Ordering::Relaxed),
        sum: counter.sum.load(Ordering::Relaxed),
        max: counter.max.load(Ordering::Relaxed),
        values: counter.values.snapshot(),
    }
}

pub const fn histogram_bucket(value: u64) -> usize {
    if value == 0 {
        return 0;
    }
    let bucket = (u64::BITS - value.leading_zeros()) as usize;
    if bucket >= HISTOGRAM_BUCKETS {
        HISTOGRAM_BUCKETS - 1
    } else {
        bucket
    }
}

pub fn histogram_percentile(histogram: &[u64; HISTOGRAM_BUCKETS], percentile: u64) -> u64 {
    let total = histogram.iter().copied().sum::<u64>();
    if total == 0 {
        return 0;
    }
    let target = total.saturating_mul(percentile.clamp(1, 100)).div_ceil(100);
    let mut seen = 0u64;
    for (bucket, count) in histogram.iter().copied().enumerate() {
        seen = seen.saturating_add(count);
        if seen >= target {
            return if bucket == 0 { 0 } else { 1u64 << (bucket - 1) };
        }
    }
    1u64 << (HISTOGRAM_BUCKETS - 2)
}

/// 在 timer IRQ 中记录被打断的 PC。函数只执行有界原子探测。
pub fn sample_pc(pc: usize, from_user: bool) {
    if !enabled() || pc == 0 {
        return;
    }
    let cpu = current_cpu();
    let key = (pc & !1usize) | usize::from(from_user);
    let mut slot_index = sample_hash(key) & (SAMPLE_SLOTS - 1);
    for _ in 0..SAMPLE_PROBES {
        let slot = &SAMPLES[cpu][slot_index];
        let observed = slot.key.load(Ordering::Relaxed);
        if observed == key {
            slot.samples.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if observed == 0
            && slot
                .key
                .compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            slot.samples.fetch_add(1, Ordering::Relaxed);
            return;
        }
        slot_index = (slot_index + 1) & (SAMPLE_SLOTS - 1);
    }
    DROPPED_SAMPLES[cpu].fetch_add(1, Ordering::Relaxed);
}

fn sample_hash(key: usize) -> usize {
    let mut value = key as u64;
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    (value ^ (value >> 33)) as usize
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcSample {
    pub pc: usize,
    pub from_user: bool,
    pub samples: u64,
}

pub fn sample_slot(cpu: usize, slot: usize) -> Option<PcSample> {
    if cpu >= MAX_CPUS || slot >= SAMPLE_SLOTS {
        return None;
    }
    let entry = &SAMPLES[cpu][slot];
    let key = entry.key.load(Ordering::Relaxed);
    let samples = entry.samples.load(Ordering::Relaxed);
    if key == 0 || samples == 0 {
        return None;
    }
    Some(PcSample {
        pc: key & !1usize,
        from_user: key & 1 != 0,
        samples,
    })
}

pub fn dropped_samples(cpu: usize) -> u64 {
    DROPPED_SAMPLES
        .get(cpu)
        .map_or(0, |value| value.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    static CLOCK: AtomicU64 = AtomicU64::new(10);
    static TASK_CPU_NS: AtomicU64 = AtomicU64::new(100);

    fn clock() -> u64 {
        CLOCK.fetch_add(7, Ordering::Relaxed)
    }

    fn cpu() -> usize {
        1
    }

    fn task_cpu_ns() -> u64 {
        TASK_CPU_NS.fetch_add(3, Ordering::Relaxed)
    }

    #[test]
    fn scope_records_wall_and_cpu_time() {
        install(clock, cpu, task_cpu_ns, 1_000_000_000);
        reset();
        set_enabled(true);
        drop(scope(Event::NetProtocolTurn).bytes(64).packets(2));
        let value = snapshot(1, Event::NetProtocolTurn);
        assert_eq!(value.calls, 1);
        assert_eq!(value.cycles, 7);
        assert_eq!(value.wall_ns, 7);
        assert_eq!(value.on_cpu_ns, 3);
        assert_eq!(value.off_cpu_ns, 4);
        assert_eq!(value.bytes, 64);
        assert_eq!(value.packets, 2);

        let stale = scope(Event::NetWorkerTurn);
        reset();
        drop(stale);
        assert_eq!(snapshot(1, Event::NetWorkerTurn).calls, 0);
    }

    #[test]
    fn histogram_and_sampler_are_bounded() {
        install(clock, cpu, task_cpu_ns, 1_000_000_000);
        reset();
        set_enabled(true);
        observe(Metric::IngressRingDepth, 17);
        let metric = metric_snapshot(1, Metric::IngressRingDepth);
        assert_eq!(metric.observations, 1);
        assert_eq!(histogram_percentile(&metric.values, 50), 16);

        sample_pc(0x8020_1234, false);
        sample_pc(0x8020_1234, false);
        let found = (0..SAMPLE_SLOTS)
            .filter_map(|slot| sample_slot(1, slot))
            .find(|sample| sample.pc == 0x8020_1234)
            .unwrap();
        assert_eq!(found.samples, 2);
        assert!(!found.from_user);
    }
}
