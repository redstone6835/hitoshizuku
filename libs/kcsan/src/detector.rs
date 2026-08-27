use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, compiler_fence};

use crate::{Access, AccessKind, Config, RuntimeHooks, Stats};

const MAX_CPU_SLOTS: usize = 512;
#[cfg(any(test, target_arch = "loongarch64", target_arch = "riscv64"))]
const MIXED_CPU_SLOT: usize = MAX_CPU_SLOTS - 1;
const WATCHPOINT_SLOTS: usize = 256;
const WATCHPOINT_PROBES: usize = 2;
const WATCH_GRANULE_SHIFT: usize = 4;
const MAX_HOOK_ACCESS_SIZE: usize = 16;
const DEDUP_SLOTS: usize = 64;
const KERNEL_ADDRESS_MIN: usize = 1usize << (usize::BITS - 1);

const SLOT_STATE_BITS: usize = 2;
const SLOT_STATE_MASK: usize = (1 << SLOT_STATE_BITS) - 1;
const SLOT_GENERATION_STEP: usize = 1 << SLOT_STATE_BITS;
const SLOT_EMPTY: usize = 0;
const SLOT_PREPARING: usize = 1;
const SLOT_ACTIVE: usize = 2;
const SLOT_CLAIMED: usize = 3;

const INSTALL_EMPTY: usize = 0;
const INSTALLING: usize = 1;
const INSTALL_READY: usize = 2;

const RANDOM_STEP: u64 = 0x9e37_79b9_7f4a_7c15;

struct HooksCell(UnsafeCell<RuntimeHooks>);

// Safety: install() 只允许成功一次；INSTALL_STATE 的 Release/Acquire 在回调可读前发布
// 唯一写入，之后所有 CPU 只读该单元。
unsafe impl Sync for HooksCell {}

const fn zero_u64() -> u64 {
    0
}

static HOOKS: HooksCell = HooksCell(UnsafeCell::new(RuntimeHooks {
    current_task: zero_u64,
    timestamp: zero_u64,
}));
static INSTALL_STATE: AtomicUsize = AtomicUsize::new(INSTALL_EMPTY);
static ENABLED: AtomicBool = AtomicBool::new(false);
static SAMPLE_INTERVAL: AtomicU32 = AtomicU32::new(4_096);
static DELAY_ITERATIONS: AtomicU32 = AtomicU32::new(4_096);
static REPORT_REPEATED: AtomicBool = AtomicBool::new(false);

static SAMPLES: AtomicU64 = AtomicU64::new(0);
static WATCHPOINT_MISSES: AtomicU64 = AtomicU64::new(0);
static CONFLICTS: AtomicU64 = AtomicU64::new(0);
static REPORTS: AtomicU64 = AtomicU64::new(0);
static DROPPED_REPORTS: AtomicU64 = AtomicU64::new(0);
static DUPLICATE_REPORTS: AtomicU64 = AtomicU64::new(0);
static ACTIVE_WATCHPOINTS: AtomicUsize = AtomicUsize::new(0);

#[repr(C, align(64))]
struct CpuState {
    disable_depth: AtomicU32,
    next_sample: AtomicU32,
    random: AtomicU64,
}

impl CpuState {
    const fn new() -> Self {
        Self {
            disable_depth: AtomicU32::new(0),
            next_sample: AtomicU32::new(0),
            random: AtomicU64::new(0),
        }
    }
}

static CPU_STATES: [CpuState; MAX_CPU_SLOTS] = [const { CpuState::new() }; MAX_CPU_SLOTS];

#[repr(C, align(64))]
struct WatchSlot {
    state: AtomicUsize,
    first_address: AtomicUsize,
    first_meta: AtomicU64,
    first_task: AtomicU64,
    first_pc: AtomicUsize,
    first_timestamp: AtomicU64,
}

impl WatchSlot {
    const fn new() -> Self {
        Self {
            state: AtomicUsize::new(SLOT_EMPTY),
            first_address: AtomicUsize::new(0),
            first_meta: AtomicU64::new(0),
            first_task: AtomicU64::new(0),
            first_pc: AtomicUsize::new(0),
            first_timestamp: AtomicU64::new(0),
        }
    }

    fn write_first(&self, access: Access) {
        self.first_address.store(access.address, Ordering::Relaxed);
        self.first_meta
            .store(encode_meta(access), Ordering::Relaxed);
        self.first_task.store(access.task, Ordering::Relaxed);
        self.first_pc.store(access.pc, Ordering::Relaxed);
        self.first_timestamp
            .store(access.timestamp, Ordering::Relaxed);
    }

    fn read_first(&self) -> Access {
        decode_access(
            self.first_address.load(Ordering::Relaxed),
            self.first_meta.load(Ordering::Relaxed),
            self.first_task.load(Ordering::Relaxed),
            self.first_pc.load(Ordering::Relaxed),
            self.first_timestamp.load(Ordering::Relaxed),
        )
    }
}

static WATCHPOINTS: [WatchSlot; WATCHPOINT_SLOTS] = [const { WatchSlot::new() }; WATCHPOINT_SLOTS];
static DEDUP: [AtomicU64; DEDUP_SLOTS] = [const { AtomicU64::new(0) }; DEDUP_SLOTS];

/// 当前 CPU 上的检测抑制 guard。
///
/// guard 只抑制创建它的 CPU，不得持有它跨越可能调度或迁移的调用。
pub struct DisableGuard {
    cpu: usize,
    active: bool,
}

impl Drop for DisableGuard {
    fn drop(&mut self) {
        if self.active {
            CPU_STATES[self.cpu]
                .disable_depth
                .fetch_sub(1, Ordering::Release);
        }
    }
}

struct InternalDisable {
    cpu: usize,
}

impl InternalDisable {
    fn new(cpu: usize) -> Self {
        CPU_STATES[cpu]
            .disable_depth
            .fetch_add(1, Ordering::Acquire);
        Self { cpu }
    }
}

impl Drop for InternalDisable {
    fn drop(&mut self) {
        CPU_STATES[self.cpu]
            .disable_depth
            .fetch_sub(1, Ordering::Release);
    }
}

/// 安装平台回调并启用检测器。重复安装返回 false。
pub fn install(hooks: RuntimeHooks, config: Config) -> bool {
    if INSTALL_STATE
        .compare_exchange(
            INSTALL_EMPTY,
            INSTALLING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return false;
    }
    configure(config);
    // Safety: INSTALLING 只由当前 boot CPU 持有；INSTALL_READY 前没有读者。
    unsafe { HOOKS.0.get().write(hooks) };
    for (cpu, state) in CPU_STATES.iter().enumerate() {
        state
            .next_sample
            .store(randomized_interval(state, cpu), Ordering::Relaxed);
    }
    INSTALL_STATE.store(INSTALL_READY, Ordering::Release);
    ENABLED.store(true, Ordering::Release);
    true
}

/// 更新采样参数；可以在检测器运行期间调用。
pub fn configure(config: Config) {
    SAMPLE_INTERVAL.store(config.sample_interval.max(1), Ordering::Release);
    DELAY_ITERATIONS.store(config.delay_iterations.max(1), Ordering::Release);
    REPORT_REPEATED.store(config.report_repeated, Ordering::Release);
}

/// 启用或暂停检测。尚未 install 时启用请求会被忽略。
pub fn set_enabled(enabled: bool) {
    ENABLED.store(
        enabled && INSTALL_STATE.load(Ordering::Acquire) == INSTALL_READY,
        Ordering::Release,
    );
}

/// 返回检测器当前是否启用。
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// 暂时抑制当前 CPU 的访问 hook。
pub fn disable() -> DisableGuard {
    let active = INSTALL_STATE.load(Ordering::Acquire) == INSTALL_READY;
    let cpu = if active { current_cpu_slot() } else { 0 };
    if active {
        CPU_STATES[cpu]
            .disable_depth
            .fetch_add(1, Ordering::Acquire);
    }
    DisableGuard { cpu, active }
}

/// 强制当前 CPU 的下一次合格访问建立 watchpoint。
pub fn force_sample() {
    if INSTALL_STATE.load(Ordering::Acquire) == INSTALL_READY {
        CPU_STATES[current_cpu_slot()]
            .next_sample
            .store(1, Ordering::Release);
    }
}

pub(crate) fn stats() -> Stats {
    Stats {
        samples: SAMPLES.load(Ordering::Relaxed),
        watchpoint_misses: WATCHPOINT_MISSES.load(Ordering::Relaxed),
        conflicts: CONFLICTS.load(Ordering::Relaxed),
        reports: REPORTS.load(Ordering::Relaxed),
        dropped_reports: DROPPED_REPORTS.load(Ordering::Relaxed),
        duplicate_reports: DUPLICATE_REPORTS.load(Ordering::Relaxed),
        active_watchpoints: ACTIVE_WATCHPOINTS.load(Ordering::Acquire),
    }
}

pub(crate) fn check_access(address: usize, size: usize, kind: AccessKind, pc: usize) {
    if !ENABLED.load(Ordering::Relaxed)
        || size == 0
        || address < KERNEL_ADDRESS_MIN
        || kind.is_volatile()
    {
        return;
    }
    let cpu = current_cpu_slot();
    if CPU_STATES[cpu].disable_depth.load(Ordering::Relaxed) != 0 {
        return;
    }
    let mut offset = 0usize;
    while offset < size {
        let Some(chunk_address) = address.checked_add(offset) else {
            break;
        };
        let chunk = (size - offset).min(MAX_HOOK_ACCESS_SIZE);
        check_one(cpu, chunk_address, chunk, kind, pc);
        offset += chunk;
    }
}

fn check_one(cpu: usize, address: usize, size: usize, kind: AccessKind, pc: usize) {
    if claim_conflict(cpu, address, size, kind, pc) {
        return;
    }
    if !should_sample(cpu) {
        return;
    }
    let access = capture_access(cpu, address, size, kind, pc);
    let key = watch_key(address);
    for probe in 0..WATCHPOINT_PROBES {
        let slot = &WATCHPOINTS[slot_index(key, probe)];
        let empty = slot.state.load(Ordering::Acquire);
        if slot_status(empty) != SLOT_EMPTY {
            continue;
        }
        let generation = next_generation(empty);
        let preparing = generation | SLOT_PREPARING;
        if slot
            .state
            .compare_exchange(empty, preparing, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        slot.write_first(access);
        SAMPLES.fetch_add(1, Ordering::Relaxed);
        ACTIVE_WATCHPOINTS.fetch_add(1, Ordering::AcqRel);
        let active = generation | SLOT_ACTIVE;
        slot.state.store(active, Ordering::Release);
        delay_watchpoint();
        finish_watchpoint(slot, active);
        return;
    }
    WATCHPOINT_MISSES.fetch_add(1, Ordering::Relaxed);
}

fn claim_conflict(cpu: usize, address: usize, size: usize, kind: AccessKind, pc: usize) -> bool {
    let key = watch_key(address);
    let keys = [key.saturating_sub(1), key, key.saturating_add(1)];
    for candidate in keys {
        for probe in 0..WATCHPOINT_PROBES {
            let slot = &WATCHPOINTS[slot_index(candidate, probe)];
            let active = slot.state.load(Ordering::Acquire);
            if slot_status(active) != SLOT_ACTIVE {
                continue;
            }
            let first = slot.read_first();
            if !ranges_overlap(first.address, first.size, address, size)
                || !accesses_conflict(first.kind, kind)
            {
                continue;
            }
            if slot
                .state
                .compare_exchange(
                    active,
                    slot_generation(active) | SLOT_CLAIMED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            let second = capture_access(cpu, address, size, kind, pc);
            CONFLICTS.fetch_add(1, Ordering::Relaxed);
            if should_report(first, second) {
                if crate::report::publish(first, second).is_some() {
                    REPORTS.fetch_add(1, Ordering::Relaxed);
                } else {
                    DROPPED_REPORTS.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                DUPLICATE_REPORTS.fetch_add(1, Ordering::Relaxed);
            }
            slot.state
                .store(slot_generation(active) | SLOT_EMPTY, Ordering::Release);
            ACTIVE_WATCHPOINTS.fetch_sub(1, Ordering::AcqRel);
            return true;
        }
    }
    false
}

fn finish_watchpoint(slot: &WatchSlot, active: usize) {
    if slot
        .state
        .compare_exchange(
            active,
            slot_generation(active) | SLOT_EMPTY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        ACTIVE_WATCHPOINTS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn should_sample(cpu: usize) -> bool {
    let state = &CPU_STATES[cpu];
    let old = state.next_sample.fetch_sub(1, Ordering::Relaxed);
    if old > 1 {
        return false;
    }
    state
        .next_sample
        .store(randomized_interval(state, cpu), Ordering::Relaxed);
    old == 1
}

fn randomized_interval(state: &CpuState, cpu: usize) -> u32 {
    let base = SAMPLE_INTERVAL.load(Ordering::Relaxed).max(1);
    if base == 1 {
        return 1;
    }
    let value = state
        .random
        .fetch_add(RANDOM_STEP ^ cpu as u64, Ordering::Relaxed)
        .wrapping_add(RANDOM_STEP);
    let mixed = mix64(value ^ ((cpu as u64) << 32));
    (base / 2).saturating_add((mixed as u32) % base).max(1)
}

fn delay_watchpoint() {
    let iterations = DELAY_ITERATIONS.load(Ordering::Relaxed);
    for _ in 0..iterations {
        compiler_fence(Ordering::SeqCst);
        spin_loop();
    }
}

fn capture_access(cpu: usize, address: usize, size: usize, kind: AccessKind, pc: usize) -> Access {
    let _disabled = InternalDisable::new(cpu);
    let hooks = runtime_hooks();
    Access {
        address,
        size,
        kind,
        cpu,
        task: (hooks.current_task)(),
        pc,
        timestamp: (hooks.timestamp)(),
    }
}

fn runtime_hooks() -> &'static RuntimeHooks {
    debug_assert_eq!(INSTALL_STATE.load(Ordering::Acquire), INSTALL_READY);
    // Safety: install() 在 INSTALL_READY 的 Release store 前完成唯一写入，此后不再修改。
    unsafe { &*HOOKS.0.get() }
}

fn should_report(first: Access, second: Access) -> bool {
    if REPORT_REPEATED.load(Ordering::Relaxed) {
        return true;
    }
    let mut hash = mix64((first.address >> WATCH_GRANULE_SHIFT) as u64);
    let (low_pc, high_pc) = if first.pc <= second.pc {
        (first.pc, second.pc)
    } else {
        (second.pc, first.pc)
    };
    hash ^= mix64(low_pc as u64);
    hash ^= mix64((high_pc as u64).rotate_left(17));
    hash ^= u64::from(first.kind as u8) << 8;
    hash ^= u64::from(second.kind as u8) << 16;
    hash |= 1;
    let slot = &DEDUP[hash as usize & (DEDUP_SLOTS - 1)];
    slot.swap(hash, Ordering::AcqRel) != hash
}

fn accesses_conflict(first: AccessKind, second: AccessKind) -> bool {
    if first.is_volatile() || second.is_volatile() {
        return false;
    }
    if first.is_atomic() && second.is_atomic() {
        return false;
    }
    first.is_write() || second.is_write()
}

fn ranges_overlap(first: usize, first_size: usize, second: usize, second_size: usize) -> bool {
    let first_end = first.saturating_add(first_size);
    let second_end = second.saturating_add(second_size);
    first < second_end && second < first_end
}

fn watch_key(address: usize) -> usize {
    address >> WATCH_GRANULE_SHIFT
}

fn slot_status(state: usize) -> usize {
    state & SLOT_STATE_MASK
}

fn slot_generation(state: usize) -> usize {
    state & !SLOT_STATE_MASK
}

fn next_generation(state: usize) -> usize {
    slot_generation(state).wrapping_add(SLOT_GENERATION_STEP) & !SLOT_STATE_MASK
}

fn slot_index(key: usize, probe: usize) -> usize {
    let hash = mix64((key as u64).wrapping_add((probe as u64).wrapping_mul(RANDOM_STEP)));
    hash as usize & (WATCHPOINT_SLOTS - 1)
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn encode_meta(access: Access) -> u64 {
    (access.size.min(u16::MAX as usize) as u64)
        | (u64::from(access.kind as u8) << 16)
        | ((access.cpu.min(u16::MAX as usize) as u64) << 32)
}

fn decode_access(address: usize, meta: u64, task: u64, pc: usize, timestamp: u64) -> Access {
    Access {
        address,
        size: (meta & 0xffff) as usize,
        kind: AccessKind::from_raw(((meta >> 16) & 0xff) as u8),
        cpu: ((meta >> 32) & 0xffff) as usize,
        task,
        pc,
        timestamp,
    }
}

pub(crate) fn watchpoint_active_for(address: usize) -> bool {
    WATCHPOINTS.iter().any(|slot| {
        let before = slot.state.load(Ordering::Acquire);
        matches!(slot_status(before), SLOT_ACTIVE | SLOT_CLAIMED) && {
            let access = slot.read_first();
            slot.state.load(Ordering::Acquire) == before
                && ranges_overlap(access.address, access.size, address, 1)
        }
    })
}

#[cfg(target_arch = "loongarch64")]
fn current_cpu_slot() -> usize {
    let cpuid: usize;
    // Safety: CSR 0x20 是只读 CPUID，读取不修改处理器状态。
    unsafe {
        core::arch::asm!(
            "csrrd {cpuid}, 0x20",
            cpuid = out(reg) cpuid,
            options(nomem, nostack, preserves_flags),
        );
    }
    (cpuid & 0x1ff).min(MIXED_CPU_SLOT)
}

#[cfg(target_arch = "riscv64")]
fn current_cpu_slot() -> usize {
    let tp: usize;
    // Safety: 只读取当前 hart 的线程指针寄存器。
    unsafe {
        core::arch::asm!("mv {tp}, tp", tp = out(reg) tp, options(nomem, nostack));
    }
    if tp == 0 {
        return MIXED_CPU_SLOT;
    }
    // HartLocal.logical_id 是第二个 usize 字段；AP/boot hart 在进入 Rust 调试路径前
    // 已经初始化 tp。runtime crate 不经过自动插桩，读取不会递归进入 hook。
    let logical_id = unsafe { (tp as *const usize).add(1).read() };
    logical_id.min(MIXED_CPU_SLOT)
}

#[cfg(all(test, not(any(target_arch = "loongarch64", target_arch = "riscv64"))))]
std::thread_local! {
    static TEST_CPU: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(all(test, not(any(target_arch = "loongarch64", target_arch = "riscv64"))))]
fn current_cpu_slot() -> usize {
    TEST_CPU.with(|cpu| cpu.get().min(MIXED_CPU_SLOT))
}

#[cfg(all(
    not(test),
    not(any(target_arch = "loongarch64", target_arch = "riscv64"))
))]
fn current_cpu_slot() -> usize {
    0
}

#[cfg(test)]
fn set_test_cpu(cpu: usize) {
    TEST_CPU.with(|current| current.set(cpu));
}

#[cfg(test)]
fn reset_for_test() {
    ENABLED.store(false, Ordering::SeqCst);
    for slot in &WATCHPOINTS {
        slot.state.store(SLOT_EMPTY, Ordering::SeqCst);
    }
    for state in &CPU_STATES {
        state.disable_depth.store(0, Ordering::SeqCst);
        state.next_sample.store(0, Ordering::SeqCst);
        state.random.store(0, Ordering::SeqCst);
    }
    for slot in &DEDUP {
        slot.store(0, Ordering::SeqCst);
    }
    SAMPLES.store(0, Ordering::SeqCst);
    WATCHPOINT_MISSES.store(0, Ordering::SeqCst);
    CONFLICTS.store(0, Ordering::SeqCst);
    REPORTS.store(0, Ordering::SeqCst);
    DROPPED_REPORTS.store(0, Ordering::SeqCst);
    DUPLICATE_REPORTS.store(0, Ordering::SeqCst);
    ACTIVE_WATCHPOINTS.store(0, Ordering::SeqCst);
    crate::report::reset_for_test();
    INSTALL_STATE.store(INSTALL_EMPTY, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use super::*;

    static START: AtomicBool = AtomicBool::new(false);

    fn install_test_runtime(kind: AccessKind) -> Option<crate::Report> {
        reset_for_test();
        assert!(install(
            RuntimeHooks {
                current_task: || 7,
                timestamp: || 11,
            },
            Config {
                sample_interval: u32::MAX,
                delay_iterations: 2_000_000,
                report_repeated: true,
            },
        ));
        let address = KERNEL_ADDRESS_MIN + 0x12_34f;
        START.store(false, Ordering::Release);
        let owner = thread::spawn(move || {
            set_test_cpu(1);
            force_sample();
            START.store(true, Ordering::Release);
            check_access(address, 8, AccessKind::Write, 0x1000);
        });
        while !START.load(Ordering::Acquire) || !watchpoint_active_for(address) {
            thread::yield_now();
        }
        set_test_cpu(2);
        check_access(address + 4, 4, kind, 0x2000);
        owner.join().unwrap();
        let window = crate::report::report_window();
        assert_eq!(stats().active_watchpoints, 0);
        (window.first_sequence < window.next_sequence)
            .then(|| crate::report::report(window.first_sequence))
            .flatten()
    }

    #[test]
    fn overlapping_write_and_read_are_reported() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let report = install_test_runtime(AccessKind::Read).expect("必须产生冲突报告");
        assert_eq!(report.first.kind, AccessKind::Write);
        assert_eq!(report.second.kind, AccessKind::Read);
        assert_eq!((report.first.cpu, report.second.cpu), (1, 2));
        assert!(ranges_overlap(
            report.first.address,
            report.first.size,
            report.second.address,
            report.second.size,
        ));
    }

    #[test]
    fn overlapping_reads_do_not_report() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_for_test();
        assert!(install(
            RuntimeHooks {
                current_task: || 1,
                timestamp: || 2,
            },
            Config {
                sample_interval: u32::MAX,
                delay_iterations: 1_000_000,
                report_repeated: true,
            },
        ));
        let address = KERNEL_ADDRESS_MIN + 0x20_000;
        let owner = thread::spawn(move || {
            set_test_cpu(3);
            force_sample();
            check_access(address, 8, AccessKind::Read, 0x3000);
        });
        while !watchpoint_active_for(address) {
            thread::yield_now();
        }
        set_test_cpu(4);
        check_access(address, 8, AccessKind::Read, 0x4000);
        owner.join().unwrap();
        assert_eq!(crate::report::report_window().next_sequence, 1);
    }

    #[test]
    fn atomic_pair_and_volatile_accesses_are_ignored() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(!accesses_conflict(
            AccessKind::AtomicWrite,
            AccessKind::AtomicRead
        ));
        assert!(!accesses_conflict(
            AccessKind::VolatileWrite,
            AccessKind::Read
        ));
        assert!(accesses_conflict(AccessKind::AtomicWrite, AccessKind::Read));
    }

    #[test]
    fn overlap_lookup_covers_adjacent_watch_granules() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let boundary = KERNEL_ADDRESS_MIN + 0x40_000;
        assert!(ranges_overlap(boundary + 15, 16, boundary + 30, 1));
        assert!(!ranges_overlap(boundary, 8, boundary + 8, 8));
    }

    #[test]
    fn generation_token_rejects_stale_claim_after_slot_reuse() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_for_test();
        let slot = &WATCHPOINTS[0];
        let first_generation = next_generation(SLOT_EMPTY);
        let stale_active = first_generation | SLOT_ACTIVE;
        slot.state.store(stale_active, Ordering::Release);

        slot.state
            .store(first_generation | SLOT_EMPTY, Ordering::Release);
        let second_generation = next_generation(first_generation | SLOT_EMPTY);
        let current_active = second_generation | SLOT_ACTIVE;
        slot.state.store(current_active, Ordering::Release);

        assert!(
            slot.state
                .compare_exchange(
                    stale_active,
                    first_generation | SLOT_CLAIMED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        );
        assert_eq!(slot.state.load(Ordering::Acquire), current_active);
    }

    #[test]
    fn sampled_owner_never_waits_for_claimant() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_for_test();
        let slot = &WATCHPOINTS[0];
        let generation = next_generation(SLOT_EMPTY);
        let active = generation | SLOT_ACTIVE;
        ACTIVE_WATCHPOINTS.store(1, Ordering::Release);
        slot.state
            .store(generation | SLOT_CLAIMED, Ordering::Release);

        finish_watchpoint(slot, active);

        assert_eq!(
            slot.state.load(Ordering::Acquire),
            generation | SLOT_CLAIMED
        );
        assert_eq!(ACTIVE_WATCHPOINTS.load(Ordering::Acquire), 1);
        slot.state.store(generation | SLOT_EMPTY, Ordering::Release);
        ACTIVE_WATCHPOINTS.store(0, Ordering::Release);
    }
}
