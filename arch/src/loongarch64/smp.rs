//! LoongArch 多核启动与核间中断。

use alloc::sync::Arc;
use core::arch::naked_asm;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use general::TaskOps;
use sched::arch_hooks::CpuControlOps;

use super::asid_tracker::CurrentAsidTracker;
use super::heap_vm::activate_kernel_page_table_for_secondary;
use super::loader::{configure_local_timer, timer_hz};
use super::specific::*;
use super::task::LoongArch64TaskOps;
use super::trap::{
    LoongArch64InterruptOps, LoongArch64MessageInterruptOps, install_exception_entry,
};

pub const MAX_CPUS: usize = sched::NR_CPUS;
const UNKNOWN_CPU_ID: usize = usize::MAX;
const AP_STACK_SIZE: usize = 16 * 1024;

const IOCSR_IPI_STATUS: usize = 0x1000;
const IOCSR_IPI_ENABLE: usize = 0x1004;
const IOCSR_IPI_CLEAR: usize = 0x100c;
const IOCSR_MAILBOX1: usize = 0x1028;
const IOCSR_IPI_SEND: usize = 0x1040;
const IOCSR_MAILBOX_SEND: usize = 0x1048;

const IPI_SEND_BLOCKING: u32 = 1 << 31;
const MAILBOX_SEND_BLOCKING: u64 = 1 << 31;
const MAILBOX_SEND_BUFFER_SHIFT: usize = 32;
const MAILBOX_SEND_CPU_SHIFT: usize = 16;
const MAILBOX_SEND_BOX_SHIFT: usize = 2;

const ACTION_BOOT: u32 = 0;
const ACTION_RESCHEDULE: u32 = 1;
const ACTION_TLB_SHOOTDOWN: u32 = 2;
const ACTION_ICACHE_SYNC: u32 = 3;
const ACTION_MEMBARRIER: u32 = 4;

const IPI_RESCHEDULE: u32 = 1 << ACTION_RESCHEDULE;
const IPI_TLB_SHOOTDOWN: u32 = 1 << ACTION_TLB_SHOOTDOWN;
const IPI_ICACHE_SYNC: u32 = 1 << ACTION_ICACHE_SYNC;
const IPI_MEMBARRIER: u32 = 1 << ACTION_MEMBARRIER;

const SHOOTDOWN_TLB: usize = 1;
const SHOOTDOWN_ICACHE: usize = 2;
const ALL_ADDRESSES: usize = usize::MAX;
const SHOOTDOWN_RETRY_DIVISOR: u64 = 100;
const SHOOTDOWN_WARNING_SECONDS: u64 = 5;

static PHYSICAL_CPU_IDS: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(UNKNOWN_CPU_ID) }; MAX_CPUS];
static ONLINE_CPUS: AtomicUsize = AtomicUsize::new(0);
static STARTED_CPUS: AtomicUsize = AtomicUsize::new(0);
static CURRENT_LOGICAL_ASIDS: CurrentAsidTracker<MAX_CPUS> = CurrentAsidTracker::new();
static AP_IDLE_TASKS: [AtomicPtr<sched::Task>; MAX_CPUS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
static TLB_REQUESTED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static TLB_COMPLETED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static ICACHE_REQUESTED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static ICACHE_COMPLETED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static TLB_SHOOTDOWN_ASID: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static TLB_SHOOTDOWN_ADDR: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];

#[repr(C, align(16))]
struct SecondaryStack([u8; AP_STACK_SIZE]);

#[unsafe(link_section = ".bss.stack")]
static mut SECONDARY_STACKS: [SecondaryStack; MAX_CPUS] =
    [const { SecondaryStack([0; AP_STACK_SIZE]) }; MAX_CPUS];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SecondaryCpuReport {
    pub detected: usize,
    pub started: usize,
    pub failed: usize,
}

pub(crate) fn init_boot_cpu_mapping() {
    let physical_id = LoongArch64MessageInterruptOps::current_hardware_cpu_id();
    PHYSICAL_CPU_IDS[0].store(physical_id, Ordering::Release);
    ONLINE_CPUS.store(1, Ordering::Release);
    STARTED_CPUS.store(1, Ordering::Release);
}

pub(crate) fn logical_cpu_id(physical_id: usize) -> usize {
    for (logical_id, mapped_id) in PHYSICAL_CPU_IDS.iter().enumerate() {
        if mapped_id.load(Ordering::Acquire) == physical_id {
            return logical_id;
        }
    }
    0
}

fn physical_cpu_id(logical_id: usize) -> Option<usize> {
    let physical_id = PHYSICAL_CPU_IDS.get(logical_id)?.load(Ordering::Acquire);
    (physical_id != UNKNOWN_CPU_ID).then_some(physical_id)
}

#[inline]
fn iocsr_read32(offset: usize) -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!(
            "iocsrrd.w {value}, {offset}",
            value = out(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
    value
}

#[inline]
fn iocsr_read64(offset: usize) -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!(
            "iocsrrd.d {value}, {offset}",
            value = out(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
    value
}

#[inline]
fn iocsr_write32(offset: usize, value: u32) {
    unsafe {
        core::arch::asm!(
            "iocsrwr.w {value}, {offset}",
            value = in(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
}

#[inline]
fn iocsr_write64(offset: usize, value: u64) {
    unsafe {
        core::arch::asm!(
            "iocsrwr.d {value}, {offset}",
            value = in(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
}

fn send_mailbox(physical_id: usize, mailbox: usize, value: u64) {
    let send_half = |half: usize, data: u64| {
        let command = MAILBOX_SEND_BLOCKING
            | ((half as u64) << MAILBOX_SEND_BOX_SHIFT)
            | ((physical_id as u64) << MAILBOX_SEND_CPU_SHIFT)
            | (data << MAILBOX_SEND_BUFFER_SHIFT);
        iocsr_write64(IOCSR_MAILBOX_SEND, command);
    };
    send_half(mailbox << 1 | 1, value >> 32);
    send_half(mailbox << 1, value as u32 as u64);
}

fn send_ipi_to_physical(physical_id: usize, action: u32) {
    let command = IPI_SEND_BLOCKING | ((physical_id as u32) << 16) | action;
    unsafe {
        core::arch::asm!("dbar 0", options(nostack, preserves_flags));
    }
    iocsr_write32(IOCSR_IPI_SEND, command);
}

fn send_reschedule(logical_id: usize) {
    if let Some(physical_id) = physical_cpu_id(logical_id) {
        send_ipi_to_physical(physical_id, ACTION_RESCHEDULE);
    }
}

fn send_membarrier(logical_id: usize) -> bool {
    if !cpu_is_online(logical_id) {
        return false;
    }
    let Some(physical_id) = physical_cpu_id(logical_id) else {
        return false;
    };
    send_ipi_to_physical(physical_id, ACTION_MEMBARRIER);
    true
}

fn cpu_is_online(logical_id: usize) -> bool {
    logical_id < MAX_CPUS && ONLINE_CPUS.load(Ordering::Acquire) & (1 << logical_id) != 0
}

/// 发布当前 CPU 即将激活的逻辑 ASID；调用方随后必须完成全量本地 TLB 失效。
pub(crate) fn publish_current_logical_asid(asid: usize) {
    let cpu = LoongArch64MessageInterruptOps::current_cpu_id();
    CURRENT_LOGICAL_ASIDS.publish_before_activation(cpu, asid);
}

/// 在 PTE 更新后，仅保留当前仍运行目标逻辑 ASID 的历史 CPU。
pub(crate) fn shootdown_targets_after_pte_update(
    historically_active: &AtomicUsize,
    target_asid: usize,
) -> usize {
    CURRENT_LOGICAL_ASIDS.target_mask_after_pte_update(historically_active, target_asid)
}

fn poll_urgent() {
    handle_shootdown_requests();
    sched::handle_membarrier_ipi();
}

pub(crate) static CPU_CONTROL_OPS: CpuControlOps = CpuControlOps {
    send_resched: send_reschedule,
    send_membarrier,
    poll_urgent,
    is_online: cpu_is_online,
};

pub(crate) fn init_local_ipi() {
    iocsr_write32(IOCSR_IPI_CLEAR, u32::MAX);
    iocsr_write32(IOCSR_IPI_ENABLE, u32::MAX);
}

pub(crate) fn handle_ipi() {
    let action = iocsr_read32(IOCSR_IPI_STATUS);
    if action != 0 {
        iocsr_write32(IOCSR_IPI_CLEAR, action);
        unsafe {
            // 发送端先发布 shootdown 序号，再经 dbar 写 IOCSR。接收端
            // 在观察并清除 IPI 后必须建立对称顺序，否则可能读到旧序号，
            // 同时又把已合并的同类 IPI 清掉，导致远端永久少一次确认。
            core::arch::asm!("dbar 0", options(nostack, preserves_flags));
        }
    }
    if action & (IPI_TLB_SHOOTDOWN | IPI_ICACHE_SYNC) != 0 {
        handle_shootdown_requests();
    }
    if action & IPI_MEMBARRIER != 0 {
        sched::handle_membarrier_ipi();
    }
    // request_resched() 在发送 IPI 前已经发布目标 CPU 的 need_resched。
    let _ = action & IPI_RESCHEDULE;
    sched::acknowledge_resched_notification();
}

fn local_tlb_flush(asid: usize, address: usize) {
    unsafe {
        core::arch::asm!("dbar 0", options(nostack, preserves_flags));
        if address == ALL_ADDRESSES {
            if asid == 0 {
                // No ASID hint: flush everything
                core::arch::asm!("invtlb 0x0, $zero, $zero", options(nostack));
            } else {
                // ASID-specific full flush: invtlb 0x3 flushes all entries for asid
                core::arch::asm!(
                    "invtlb 0x3, {asid}, $zero",
                    asid = in(reg) asid_bits(asid),
                    options(nostack)
                );
            }
        } else {
            core::arch::asm!(
                "invtlb 0x5, {asid}, {address}",
                asid = in(reg) asid_bits(asid),
                address = in(reg) address,
                options(nostack)
            );
        }
    }
}

fn local_icache_sync() {
    unsafe {
        core::arch::asm!("dbar 0", "ibar 0", options(nostack, preserves_flags));
    }
}

pub(crate) fn handle_shootdown_requests() {
    let logical_id = LoongArch64MessageInterruptOps::current_cpu_id();
    loop {
        let requested = TLB_REQUESTED[logical_id].load(Ordering::Acquire);
        let completed = TLB_COMPLETED[logical_id].load(Ordering::Relaxed);
        if shootdown_sequence_reached(completed, requested) {
            break;
        }
        // Read asid/addr hint with Relaxed: worst case we use stale values
        // which just means we over-flush, never under-flush.
        let hint_asid = TLB_SHOOTDOWN_ASID[logical_id].load(Ordering::Relaxed);
        let hint_addr = TLB_SHOOTDOWN_ADDR[logical_id].load(Ordering::Relaxed);
        local_tlb_flush(hint_asid, hint_addr);
        TLB_COMPLETED[logical_id].store(requested, Ordering::Release);
    }
    loop {
        let requested = ICACHE_REQUESTED[logical_id].load(Ordering::Acquire);
        let completed = ICACHE_COMPLETED[logical_id].load(Ordering::Relaxed);
        if shootdown_sequence_reached(completed, requested) {
            break;
        }
        local_icache_sync();
        ICACHE_COMPLETED[logical_id].store(requested, Ordering::Release);
        // flush 期间可能又有新 generation 到达，而 IOCSR 会把同类
        // IPI 合并成一个位。必须重读 requested 直到稳定，不能假设
        // 后续请求一定还会带来新的中断边沿。
    }
}

fn publish_shootdown(
    kind: usize,
    asid: usize,
    address: usize,
    action: u32,
    requested_targets: usize,
) {
    let source = LoongArch64MessageInterruptOps::current_cpu_id();
    let source_bit = 1usize << source;
    let targets = ONLINE_CPUS.load(Ordering::Acquire) & requested_targets & !source_bit;

    match kind {
        SHOOTDOWN_TLB => local_tlb_flush(asid, address),
        SHOOTDOWN_ICACHE => local_icache_sync(),
        _ => return,
    }
    if targets == 0 {
        return;
    }

    let mut expected = [0usize; MAX_CPUS];
    for logical_id in 0..MAX_CPUS {
        if targets & (1 << logical_id) == 0 {
            continue;
        }
        expected[logical_id] = match kind {
            SHOOTDOWN_TLB => {
                // Store shootdown parameters before bumping sequence.
                // Targets use these for precise invtlb when possible.
                // Under concurrent shootdowns the hint may be stale; targets
                // fall back to invtlb 0x0 when asid==0 or addr==ALL_ADDRESSES.
                TLB_SHOOTDOWN_ASID[logical_id].store(asid, Ordering::Relaxed);
                TLB_SHOOTDOWN_ADDR[logical_id].store(address, Ordering::Relaxed);
                TLB_REQUESTED[logical_id]
                    .fetch_add(1, Ordering::AcqRel)
                    .wrapping_add(1)
            }
            SHOOTDOWN_ICACHE => ICACHE_REQUESTED[logical_id]
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1),
            _ => 0,
        };
    }
    send_action_to_mask(targets, action);
    wait_for_shootdown(kind, action, asid, address, targets, &expected);
}

fn wait_for_shootdown(
    kind: usize,
    action: u32,
    asid: usize,
    address: usize,
    targets: usize,
    expected: &[usize; MAX_CPUS],
) {
    let counter_hz = stable_counter_hz().max(1);
    let retry_ticks = (counter_hz / SHOOTDOWN_RETRY_DIVISOR).max(1);
    let warning_ticks = counter_hz.saturating_mul(SHOOTDOWN_WARNING_SECONDS);
    for logical_id in 0..MAX_CPUS {
        if targets & (1 << logical_id) == 0 {
            continue;
        }
        let completed = match kind {
            SHOOTDOWN_TLB => &TLB_COMPLETED[logical_id],
            SHOOTDOWN_ICACHE => &ICACHE_COMPLETED[logical_id],
            _ => return,
        };
        let mut last_kick = stable_counter_raw();
        let mut last_warning = last_kick;
        loop {
            let observed = completed.load(Ordering::Acquire);
            if shootdown_sequence_reached(observed, expected[logical_id]) {
                break;
            }
            // 两个 CPU 可能同时发起 shootdown。在等待对端时主动消费本核请求，
            // 避免双方都处于关中断临界区时相互等待。
            handle_shootdown_requests();
            let now = stable_counter_raw();
            if now.wrapping_sub(last_kick) >= retry_ticks {
                if let Some(physical_id) = physical_cpu_id(logical_id) {
                    // IOCSR IPI 是同位合并通知。目标核长时间关中断或恰好清除同位
                    // pending 时，序号仍是最终真值；重复敲门直到目标核发布确认。
                    send_ipi_to_physical(physical_id, action);
                }
                last_kick = now;
            }
            if now.wrapping_sub(last_warning) >= warning_ticks {
                let target_logical_asid = CURRENT_LOGICAL_ASIDS.current(logical_id);
                log::warning!(
                    "[smp] shootdown 确认等待过长 kind={} asid={} address={:#x} source={} target={} targets={:#x} expected={} completed={} requested={} online={:#x} target_logical_asid={:?} target_asid_matches={}",
                    kind,
                    asid,
                    address,
                    LoongArch64MessageInterruptOps::current_cpu_id(),
                    logical_id,
                    targets,
                    expected[logical_id],
                    completed.load(Ordering::Acquire),
                    match kind {
                        SHOOTDOWN_TLB => TLB_REQUESTED[logical_id].load(Ordering::Acquire),
                        SHOOTDOWN_ICACHE => ICACHE_REQUESTED[logical_id].load(Ordering::Acquire),
                        _ => 0,
                    },
                    ONLINE_CPUS.load(Ordering::Acquire),
                    target_logical_asid,
                    target_logical_asid == Some(asid),
                );
                last_warning = now;
            }
            core::hint::spin_loop();
        }
    }
}

const fn shootdown_sequence_reached(completed: usize, expected: usize) -> bool {
    completed.wrapping_sub(expected) <= usize::MAX / 2
}

fn send_action_to_mask(mask: usize, action: u32) {
    for logical_id in 0..MAX_CPUS {
        if mask & (1 << logical_id) != 0
            && let Some(physical_id) = physical_cpu_id(logical_id)
        {
            send_ipi_to_physical(physical_id, action);
        }
    }
}

pub(crate) fn flush_tlb_all_cpus(asid: usize, address: Option<usize>) {
    publish_shootdown(
        SHOOTDOWN_TLB,
        asid,
        address.unwrap_or(ALL_ADDRESSES),
        ACTION_TLB_SHOOTDOWN,
        usize::MAX,
    );
}

pub(crate) fn flush_tlb_on_cpus(asid: usize, address: Option<usize>, targets: usize) {
    publish_shootdown(
        SHOOTDOWN_TLB,
        asid,
        address.unwrap_or(ALL_ADDRESSES),
        ACTION_TLB_SHOOTDOWN,
        targets,
    );
}

pub(crate) fn sync_icache_all_cpus() {
    publish_shootdown(
        SHOOTDOWN_ICACHE,
        0,
        ALL_ADDRESSES,
        ACTION_ICACHE_SYNC,
        usize::MAX,
    );
}

pub fn start_secondary_cpus() -> SecondaryCpuReport {
    init_local_ipi();
    let boot_physical_id = LoongArch64MessageInterruptOps::current_hardware_cpu_id();
    let mut topology = general::dev::cpu::snapshot_topology();
    topology.sort_by_key(|cpu| (cpu.reg != boot_physical_id as u64, cpu.reg));
    for (logical_id, cpu) in topology.iter_mut().enumerate() {
        cpu.logical_id = logical_id as u32;
    }
    general::dev::cpu::install_topology(topology.clone());

    let detected = topology.len().min(MAX_CPUS);
    for (logical_id, cpu) in topology.iter().take(detected).enumerate() {
        PHYSICAL_CPU_IDS[logical_id].store(cpu.reg as usize, Ordering::Release);
    }

    let mut report = SecondaryCpuReport {
        detected,
        started: 1,
        failed: 0,
    };
    let entry = virt_to_phys(secondary_entry as *const () as usize) as u64;
    for logical_id in 1..detected {
        let Some(physical_id) = physical_cpu_id(logical_id) else {
            report.failed += 1;
            continue;
        };
        let idle = sched::spawn_idle_for_cpu(logical_id);
        AP_IDLE_TASKS[logical_id].store(Arc::into_raw(idle).cast_mut(), Ordering::Release);
        send_mailbox(physical_id, 1, logical_id as u64);
        send_mailbox(physical_id, 0, entry);
        core::sync::atomic::fence(Ordering::SeqCst);
        send_ipi_to_physical(physical_id, ACTION_BOOT);

        let timeout = stable_counter_raw().saturating_add(stable_counter_hz().max(1) / 2);
        while STARTED_CPUS.load(Ordering::Acquire) & (1 << logical_id) == 0
            && stable_counter_raw() < timeout
        {
            core::hint::spin_loop();
        }
        if STARTED_CPUS.load(Ordering::Acquire) & (1 << logical_id) != 0 {
            while !sched::is_cpu_active(logical_id) && stable_counter_raw() < timeout {
                core::hint::spin_loop();
            }
            report.started += 1;
        } else {
            let idle_ptr = AP_IDLE_TASKS[logical_id].swap(core::ptr::null_mut(), Ordering::AcqRel);
            if !idle_ptr.is_null() {
                unsafe { drop(Arc::from_raw(idle_ptr)) };
            }
            let _ = sched::offline_cpu(logical_id);
            report.failed += 1;
        }
    }
    report
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn secondary_entry() -> ! {
    naked_asm!(
        "ori $r12, $r0, 0x4",
        "csrxchg $r0, $r12, {csr_crmd}",
        "ori $r12, $r0, 0x1",
        "lu52i.d $r12, $r12, -2048",
        "csrwr $r12, {csr_dmw0}",
        "ori $r12, $r0, 0x11",
        "lu52i.d $r12, $r12, -1792",
        "csrwr $r12, {csr_dmw1}",
        "csrwr $r0, {csr_dmw2}",
        "csrwr $r0, {csr_dmw3}",
        "ori $r12, $r0, 0x3",
        "csrwr $r12, {csr_euen}",
        "lu12i.w $r12, 1",
        "ori $r12, $r12, 0x28",
        "iocsrrd.d $r4, $r12",
        "addi.d $r4, $r4, 1",
        "slli.d $r4, $r4, 14",
        "la.abs $r3, {stacks}",
        "add.d $r3, $r3, $r4",
        "move $r1, $r0",
        "move $r22, $r0",
        "la.abs $r12, {main}",
        "jirl $r0, $r12, 0",
        csr_crmd = const CSR_CRMD,
        csr_euen = const CSR_EUEN,
        csr_dmw0 = const CSR_DMW0,
        csr_dmw1 = const CSR_DMW1,
        csr_dmw2 = const CSR_DMW2,
        csr_dmw3 = const CSR_DMW3,
        stacks = sym SECONDARY_STACKS,
        main = sym secondary_main,
    )
}

unsafe extern "C" fn secondary_main() -> ! {
    let physical_id = LoongArch64MessageInterruptOps::current_hardware_cpu_id();
    let logical_id = iocsr_read64(IOCSR_MAILBOX1) as usize;
    if logical_id >= MAX_CPUS || physical_cpu_id(logical_id) != Some(physical_id) {
        loop {
            unsafe { core::arch::asm!("idle 0", options(nomem, nostack, preserves_flags)) };
        }
    }

    unsafe { install_exception_entry() };
    unsafe { activate_kernel_page_table_for_secondary() };
    configure_local_timer(timer_hz());
    init_local_ipi();

    let idle_ptr = AP_IDLE_TASKS[logical_id].swap(core::ptr::null_mut(), Ordering::AcqRel);
    if idle_ptr.is_null() {
        panic!("[smp] AP idle task was not prepared");
    }
    let idle = unsafe { Arc::from_raw(idle_ptr) };
    sched::adopt_cpu_current(logical_id, idle.clone())
        .expect("[smp] failed to install AP current task");
    <LoongArch64TaskOps as TaskOps>::set_kernel_trap_stack(idle.ensure_kernel_stack());

    ONLINE_CPUS.fetch_or(1 << logical_id, Ordering::Release);
    STARTED_CPUS.fetch_or(1 << logical_id, Ordering::Release);
    log::info!(
        "[smp] CPU online: logical={} physical={}",
        logical_id,
        physical_id
    );
    unsafe { LoongArch64InterruptOps::enable_interrupts() };
    sched::cpu_start_scheduling(logical_id)
}
