//! Versioned, allocation-free profiling snapshot encoder.

use super::*;

pub const BINARY_SCHEMA_VERSION: u16 = 3;
const HEADER_SIZE: usize = 320;
const DIRECTORY_OFFSET: usize = 64;
const DIRECTORY_ENTRY_SIZE: usize = 24;
const EVENT_MASK_HIGH_OFFSET: usize = 232;
const WORKLOAD_ROOT_OFFSET: usize = 240;
const SAMPLE_HZ_OFFSET: usize = 248;
const DROPPED_SAMPLES_OFFSET: usize = 256;
const DROPPED_TRACE_OFFSET: usize = 264;
const DROPPED_ERRNO_OFFSET: usize = 272;
const DROPPED_TASK_OFFSET: usize = 280;
const EVENT_RECORD_SIZE: usize = 608;
const METRIC_RECORD_SIZE: usize = 544;
const SYSCALL_RECORD_SIZE: usize = 624;
const ERRNO_RECORD_SIZE: usize = 32;
const TASK_RECORD_SIZE: usize = 128;
const SAMPLE_RECORD_SIZE: usize = 40;
const TRACE_RECORD_SIZE: usize = 80;
const MAX_RECORD_SIZE: usize = SYSCALL_RECORD_SIZE;

const EVENT_COUNT_TOTAL: usize = CPU_SLOTS * EVENT_COUNT;
const METRIC_COUNT_TOTAL: usize = CPU_SLOTS * METRIC_COUNT;
const SYSCALL_COUNT_TOTAL: usize = MAX_PHASES * SYSCALL_SLOTS;
const ERRNO_COUNT_TOTAL: usize = ERRNO_SLOTS;
const TASK_COUNT_TOTAL: usize = TASK_SLOTS;
const SAMPLE_COUNT_TOTAL: usize = MAX_CPUS * SAMPLE_SLOTS;
const TRACE_COUNT_TOTAL: usize = MAX_CPUS * TRACE_SLOTS_PER_CPU;

const EVENT_OFFSET: usize = HEADER_SIZE;
const METRIC_OFFSET: usize = EVENT_OFFSET + EVENT_COUNT_TOTAL * EVENT_RECORD_SIZE;
const SYSCALL_OFFSET: usize = METRIC_OFFSET + METRIC_COUNT_TOTAL * METRIC_RECORD_SIZE;
const ERRNO_OFFSET: usize = SYSCALL_OFFSET + SYSCALL_COUNT_TOTAL * SYSCALL_RECORD_SIZE;
const TASK_OFFSET: usize = ERRNO_OFFSET + ERRNO_COUNT_TOTAL * ERRNO_RECORD_SIZE;
const SAMPLE_OFFSET: usize = TASK_OFFSET + TASK_COUNT_TOTAL * TASK_RECORD_SIZE;
const TRACE_OFFSET: usize = SAMPLE_OFFSET + SAMPLE_COUNT_TOTAL * SAMPLE_RECORD_SIZE;
const TOTAL_SIZE: usize = TRACE_OFFSET + TRACE_COUNT_TOTAL * TRACE_RECORD_SIZE;

const SECTIONS: [(u16, usize, usize, usize); 7] = [
    (1, EVENT_OFFSET, EVENT_COUNT_TOTAL, EVENT_RECORD_SIZE),
    (2, METRIC_OFFSET, METRIC_COUNT_TOTAL, METRIC_RECORD_SIZE),
    (3, SYSCALL_OFFSET, SYSCALL_COUNT_TOTAL, SYSCALL_RECORD_SIZE),
    (4, ERRNO_OFFSET, ERRNO_COUNT_TOTAL, ERRNO_RECORD_SIZE),
    (5, TASK_OFFSET, TASK_COUNT_TOTAL, TASK_RECORD_SIZE),
    (6, SAMPLE_OFFSET, SAMPLE_COUNT_TOTAL, SAMPLE_RECORD_SIZE),
    (7, TRACE_OFFSET, TRACE_COUNT_TOTAL, TRACE_RECORD_SIZE),
];

pub const fn binary_snapshot_len() -> usize {
    TOTAL_SIZE
}

pub fn read_binary_snapshot(output: &mut [u8], offset: u64) -> usize {
    if state() != SessionState::Frozen {
        return 0;
    }
    let mut absolute = (offset as usize).min(TOTAL_SIZE);
    let mut written = 0usize;
    while written < output.len() && absolute < TOTAL_SIZE {
        let mut record = [0u8; MAX_RECORD_SIZE];
        let (record_start, record_len) = encode_record_at(absolute, &mut record);
        let within = absolute - record_start;
        let amount = (record_len - within)
            .min(output.len() - written)
            .min(TOTAL_SIZE - absolute);
        output[written..written + amount].copy_from_slice(&record[within..within + amount]);
        written += amount;
        absolute += amount;
    }
    written
}

fn encode_record_at(absolute: usize, record: &mut [u8; MAX_RECORD_SIZE]) -> (usize, usize) {
    if absolute < HEADER_SIZE {
        encode_header(&mut record[..HEADER_SIZE]);
        return (0, HEADER_SIZE);
    }
    for (kind, start, count, size) in SECTIONS {
        let end = start + count * size;
        if absolute < end {
            let index = (absolute - start) / size;
            encode_section(kind, index, &mut record[..size]);
            return (start + index * size, size);
        }
    }
    (TOTAL_SIZE, 0)
}

fn encode_header(out: &mut [u8]) {
    out.fill(0);
    out[..8].copy_from_slice(b"MYGOPRF\0");
    put_u16(out, 8, BINARY_SCHEMA_VERSION);
    put_u16(out, 10, HEADER_SIZE as u16);
    put_u32(out, 12, 0x0102_0304);
    put_u64(out, 16, TOTAL_SIZE as u64);
    put_u64(out, 24, session_id());
    put_u64(out, 32, generation());
    put_u64(out, 40, counter_hz());
    put_u64(out, 48, event_mask());
    put_u32(out, 56, phase() as u32);
    put_u16(out, 60, MAX_CPUS as u16);
    put_u16(out, 62, SECTIONS.len() as u16);
    for (index, (kind, start, count, size)) in SECTIONS.iter().copied().enumerate() {
        let base = DIRECTORY_OFFSET + index * DIRECTORY_ENTRY_SIZE;
        put_u16(out, base, kind);
        put_u16(out, base + 2, size as u16);
        put_u32(out, base + 4, 0);
        put_u64(out, base + 8, start as u64);
        put_u64(out, base + 16, count as u64);
    }
    put_u64(out, EVENT_MASK_HIGH_OFFSET, event_mask_high());
    put_u64(out, WORKLOAD_ROOT_OFFSET, workload_root());
    put_u64(out, SAMPLE_HZ_OFFSET, sample_hz());
    let dropped_samples = (0..MAX_CPUS).map(dropped_samples).sum::<u64>();
    let dropped_trace = (0..MAX_CPUS)
        .map(|cpu| trace_window(cpu).overwritten)
        .sum::<u64>();
    put_u64(out, DROPPED_SAMPLES_OFFSET, dropped_samples);
    put_u64(out, DROPPED_TRACE_OFFSET, dropped_trace);
    put_u64(out, DROPPED_ERRNO_OFFSET, dropped_errno_records());
    put_u64(out, DROPPED_TASK_OFFSET, dropped_task_records());
}

fn encode_section(kind: u16, index: usize, out: &mut [u8]) {
    out.fill(0);
    match kind {
        1 => encode_event(index, out),
        2 => encode_metric(index, out),
        3 => encode_syscall(index, out),
        4 => encode_errno(index, out),
        5 => encode_task(index, out),
        6 => encode_sample(index, out),
        7 => encode_trace(index, out),
        _ => {}
    }
}

fn encode_event(index: usize, out: &mut [u8]) {
    let cpu = index / EVENT_COUNT;
    let event_id = index % EVENT_COUNT;
    let Some(event) = Event::from_id(event_id) else {
        return;
    };
    let value = snapshot(cpu, event);
    put_u16(out, 0, cpu as u16);
    put_u16(out, 2, event_id as u16);
    encode_timing(&value, out, 8);
}

fn encode_metric(index: usize, out: &mut [u8]) {
    let cpu = index / METRIC_COUNT;
    let metric_id = index % METRIC_COUNT;
    let Some(metric) = Metric::from_id(metric_id) else {
        return;
    };
    let value = metric_snapshot(cpu, metric);
    put_u16(out, 0, cpu as u16);
    put_u16(out, 2, metric_id as u16);
    put_u64(out, 8, value.observations);
    put_u64(out, 16, value.sum);
    put_u64(out, 24, value.max);
    encode_histogram(&value.values, out, 32);
}

fn encode_syscall(index: usize, out: &mut [u8]) {
    let phase = index / SYSCALL_SLOTS;
    let nr = index % SYSCALL_SLOTS;
    put_u16(out, 0, phase as u16);
    put_u16(out, 2, nr as u16);
    let Some(value) = syscall_snapshot(phase, nr) else {
        return;
    };
    put_u64(out, 8, value.success);
    put_u64(out, 16, value.errors);
    encode_timing(&value.timing, out, 24);
}

fn encode_errno(index: usize, out: &mut [u8]) {
    let Some(value) = errno_snapshot(index) else {
        return;
    };
    put_u16(out, 0, value.phase as u16);
    put_u16(out, 2, value.nr as u16);
    put_u32(out, 4, value.errno as u32);
    put_u64(out, 8, value.count);
}

fn encode_task(index: usize, out: &mut [u8]) {
    let Some(value) = task_snapshot(index) else {
        return;
    };
    put_u64(out, 0, value.session);
    put_u32(out, 8, value.pid as u32);
    put_u32(out, 12, value.tgid as u32);
    put_u32(out, 16, value.ppid as u32);
    put_u32(out, 20, u32::from(value.exited));
    put_u64(out, 24, value.runtime_ns);
    put_u64(out, 32, value.voluntary_switches);
    put_u64(out, 40, value.involuntary_switches);
    put_u64(out, 48, value.migrations);
    put_u32(out, 56, value.exit_code as u32);
    put_u64(out, 64, value.main_image_id);
    put_u64(out, 72, value.main_image_base);
    put_u64(out, 80, value.main_image_end);
    put_u64(out, 88, value.interpreter_image_id);
    put_u64(out, 96, value.interpreter_image_base);
    put_u64(out, 104, value.interpreter_image_end);
}

fn encode_sample(index: usize, out: &mut [u8]) {
    let cpu = index / SAMPLE_SLOTS;
    let slot = index % SAMPLE_SLOTS;
    put_u16(out, 0, cpu as u16);
    let Some(value) = sample_slot(cpu, slot) else {
        return;
    };
    put_u16(out, 2, u16::from(value.from_user));
    put_u64(out, 8, value.pc as u64);
    put_u64(out, 16, value.image_id);
    put_u64(out, 24, value.load_base as u64);
    put_u64(out, 32, value.samples);
}

fn encode_trace(index: usize, out: &mut [u8]) {
    let cpu = index / TRACE_SLOTS_PER_CPU;
    let sequence = index % TRACE_SLOTS_PER_CPU;
    let Some(value) = trace_record(cpu, sequence as u64) else {
        return;
    };
    put_u64(out, 0, value.sequence);
    put_u64(out, 8, value.timestamp_cycles);
    put_u64(out, 16, value.duration_cycles);
    put_u64(out, 24, value.session_id);
    put_u64(out, 32, value.generation);
    put_u64(out, 40, value.task_id);
    put_u64(out, 48, value.span_id);
    put_u16(out, 56, value.cpu as u16);
    out[58] = value.kind as u8;
    out[59] = value.event as u8;
    put_u64(out, 64, value.arg0);
    put_u64(out, 72, value.arg1);
}

fn encode_timing(value: &Snapshot, out: &mut [u8], base: usize) {
    for (index, value) in [
        value.calls,
        value.cycles,
        value.bytes,
        value.packets,
        value.max_cycles,
        value.wall_ns,
        value.on_cpu_ns,
        value.off_cpu_ns,
        value.max_latency_ns,
        value.migrations,
    ]
    .into_iter()
    .enumerate()
    {
        put_u64(out, base + index * 8, value);
    }
    encode_histogram(&value.latency, out, base + 80);
}

fn encode_histogram(histogram: &[u64; HISTOGRAM_BUCKETS], out: &mut [u8], base: usize) {
    for (index, value) in histogram.iter().copied().enumerate() {
        put_u64(out, base + index * 8, value);
    }
}

fn put_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
