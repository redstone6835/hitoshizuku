#![no_std]
//! cycle 统计。

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub const MAX_CPUS: usize = 8;

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
}

impl Event {
    pub const ALL: [Self; 15] = [
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
        }
    }
}

const EVENT_COUNT: usize = Event::ALL.len();

struct Counter {
    calls: AtomicU64,
    cycles: AtomicU64,
    bytes: AtomicU64,
    packets: AtomicU64,
    max_cycles: AtomicU64,
}

impl Counter {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            max_cycles: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.cycles.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.packets.store(0, Ordering::Relaxed);
        self.max_cycles.store(0, Ordering::Relaxed);
    }
}

static COUNTERS: [[Counter; EVENT_COUNT]; MAX_CPUS] =
    [const { [const { Counter::new() }; EVENT_COUNT] }; MAX_CPUS];
static ENABLED: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(1);
static COUNTER_HZ: AtomicU64 = AtomicU64::new(0);
static READ_COUNTER: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);

pub fn install(read_counter: fn() -> u64, current_cpu: fn() -> usize, counter_hz: u64) {
    READ_COUNTER.store(read_counter as usize, Ordering::Release);
    CURRENT_CPU.store(current_cpu as usize, Ordering::Release);
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
    for cpu in &COUNTERS {
        for counter in cpu {
            counter.reset();
        }
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
    ENABLED.store(was_enabled, Ordering::Release);
}

fn read_counter() -> u64 {
    let raw = READ_COUNTER.load(Ordering::Acquire);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，写入后不撤销。
    let read: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    read()
}

fn current_cpu() -> usize {
    let raw = CURRENT_CPU.load(Ordering::Acquire);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，写入后不撤销。
    let current: fn() -> usize = unsafe { core::mem::transmute(raw) };
    current().min(MAX_CPUS - 1)
}

pub struct Scope {
    event: Event,
    start: u64,
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
        if self.active && self.generation == generation() {
            record(
                self.event,
                read_counter().wrapping_sub(self.start),
                self.bytes,
                self.packets,
            );
        }
    }
}

pub fn scope(event: Event) -> Scope {
    let generation = generation();
    let active = enabled() && READ_COUNTER.load(Ordering::Acquire) != 0;
    Scope {
        event,
        start: if active { read_counter() } else { 0 },
        bytes: 0,
        packets: 0,
        active,
        generation,
    }
}

pub fn record(event: Event, cycles: u64, bytes: u64, packets: u64) {
    if !enabled() {
        return;
    }
    let counter = &COUNTERS[current_cpu()][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.cycles.fetch_add(cycles, Ordering::Relaxed);
    counter.bytes.fetch_add(bytes, Ordering::Relaxed);
    counter.packets.fetch_add(packets, Ordering::Relaxed);
    counter.max_cycles.fetch_max(cycles, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub calls: u64,
    pub cycles: u64,
    pub bytes: u64,
    pub packets: u64,
    pub max_cycles: u64,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CLOCK: AtomicU64 = AtomicU64::new(10);

    fn clock() -> u64 {
        CLOCK.fetch_add(7, Ordering::Relaxed)
    }

    fn cpu() -> usize {
        1
    }

    #[test]
    fn scope_records_on_selected_cpu() {
        install(clock, cpu, 1_000_000);
        reset();
        set_enabled(true);
        drop(scope(Event::NetProtocolTurn).bytes(64).packets(2));
        let value = snapshot(1, Event::NetProtocolTurn);
        assert_eq!(value.calls, 1);
        assert_eq!(value.cycles, 7);
        assert_eq!(value.bytes, 64);
        assert_eq!(value.packets, 2);
        assert_eq!(snapshot(0, Event::NetProtocolTurn).calls, 0);

        let stale = scope(Event::NetWorkerTurn);
        reset();
        drop(stale);
        assert_eq!(snapshot(1, Event::NetWorkerTurn).calls, 0);
    }
}
