//! x86_64 对称多处理器启动与 per-CPU 状态。
//!
//! AP 的启动遵循 Intel MP protocol 的 INIT -> SIPI -> SIPI 顺序。低端
//! trampoline 是独立的可加载段，启动参数位于同一 4 KiB 页内；BSP 只在
//! 按 CPU 串行发布参数后发送 SIPI，避免 AP 读取下一颗 CPU 的参数。

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering, fence};

#[cfg(test)]
use core::mem::offset_of;

use super::apic;
use sched::arch_hooks::CpuControlOps;

pub const MAX_CPUS: usize = sched::NR_CPUS;
const UNKNOWN_APIC_ID: u32 = u32::MAX;
const AP_START_TIMEOUT_NS: u64 = 500_000_000;
const AP_TRAMPOLINE_VECTOR: u8 = 0x08;
const MSR_GS_BASE: u32 = 0xc000_0101;
const TLB_SHOOTDOWN_RETRY_NS: u64 = 1_000_000;
const TLB_SHOOTDOWN_TIMEOUT_NS: u64 = 5_000_000_000;

/// The first two fields intentionally match trap::EntryState.  The syscall
/// entry reads these fields through `%gs`, while the trailing id identifies
/// this CPU without touching a global mutable selector.
#[repr(C, align(64))]
pub(crate) struct CpuLocal {
    pub(crate) kernel_stack_top: AtomicUsize,
    pub(crate) user_rsp: AtomicUsize,
    pub(crate) cpu_id: AtomicUsize,
    current_task: AtomicUsize,
    user_return_work: AtomicUsize,
}

impl CpuLocal {
    const fn new(cpu_id: usize) -> Self {
        Self {
            kernel_stack_top: AtomicUsize::new(0),
            user_rsp: AtomicUsize::new(0),
            cpu_id: AtomicUsize::new(cpu_id),
            current_task: AtomicUsize::new(0),
            user_return_work: AtomicUsize::new(0),
        }
    }
}

pub(crate) static CPU_LOCALS: [CpuLocal; MAX_CPUS] = [const { CpuLocal::new(0) }; MAX_CPUS];
pub(crate) const CPU_LOCAL_STRIDE: usize = core::mem::size_of::<CpuLocal>();
pub(crate) const CPU_LOCALS_SIZE: usize = CPU_LOCAL_STRIDE * MAX_CPUS;

const _: () = {
    assert!(CPU_LOCAL_STRIDE.is_power_of_two());
    assert!(CPU_LOCAL_STRIDE >= core::mem::size_of::<CpuLocal>());
};
static HOSTED_CPU_ID: AtomicUsize = AtomicUsize::new(0);
static BOOT_INITIALIZED: AtomicBool = AtomicBool::new(false);
static APIC_IDS: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(UNKNOWN_APIC_ID) }; MAX_CPUS];
static AP_STARTED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static AP_IDLE_TASKS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static AP_STACK_TOPS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static ONLINE_CPUS: AtomicUsize = AtomicUsize::new(0);
static TLB_REQUESTED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static TLB_COMPLETED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

#[repr(C, align(8))]
struct ApTrampolineParams {
    cr3: u64,
    entry: u64,
    stack_top: u64,
    logical_id: u32,
    apic_id: u32,
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data.ap_trampoline")]
static mut __x86_ap_trampoline_params: ApTrampolineParams = ApTrampolineParams {
    cr3: 0,
    entry: 0,
    stack_top: 0,
    logical_id: 0,
    apic_id: 0,
};

// This code starts in real mode at physical 0x8000.  It uses its private
// low-memory GDT until CR3 has been loaded, then transfers to the high-half
// Rust entry using the stack and entry supplied by AP_TRAMPOLINE_PARAMS.
#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .section .text.ap_trampoline,"ax",@progbits
    .balign 16
    .globl __x86_ap_trampoline_start
    .type __x86_ap_trampoline_start,@object
__x86_ap_trampoline_start:
    .code16
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    lgdt __x86_ap_gdt_ptr_phys
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmp $0x08, $__x86_ap_protected_phys

    .code32
.globl __x86_ap_protected
__x86_ap_protected:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %cr4, %eax
    orl $0x20, %eax
    movl %eax, %cr4
    movl $__x86_ap_params_phys, %esi
    movl 0(%esi), %eax
    movl %eax, %cr3
    movl $0xc0000080, %ecx
    rdmsr
    /* The BSP admits APs only after validating NX support.  Mirror the BSP's
       EFER and write-protect policy before entering the shared page tables. */
    orl $0x900, %eax
    wrmsr
    movl %cr0, %eax
    orl $0x80010000, %eax
    movl %eax, %cr0
    /* GDT slot 3 is the long-mode code descriptor (L=1, D=0). */
    ljmp $0x18, $__x86_ap_long_mode_phys

    .code64
.globl __x86_ap_long_mode
__x86_ap_long_mode:
    movabs $__x86_ap_params_phys, %rsi
    movq 16(%rsi), %rsp
    andq $-16, %rsp
    subq $8, %rsp
    movl 24(%rsi), %edi
    /* The BSP publishes a validated high-half entry per AP. */
    movq 8(%rsi), %rax
    jmp *%rax

    .balign 8
.globl __x86_ap_gdt
__x86_ap_gdt:
    .quad 0x0000000000000000
    .quad 0x00cf9a000000ffff
    .quad 0x00cf92000000ffff
    .quad 0x00af9a000000ffff
.globl __x86_ap_gdt_end
__x86_ap_gdt_end:
.globl __x86_ap_gdt_ptr
__x86_ap_gdt_ptr:
    .word __x86_ap_gdt_end - __x86_ap_gdt - 1
    .long __x86_ap_gdt_phys
    .long 0
    .size __x86_ap_trampoline_start, .-__x86_ap_trampoline_start

    .section .data.ap_trampoline,"aw",@progbits
    .globl __x86_ap_trampoline_end
__x86_ap_trampoline_end:
    "#,
    options(att_syntax)
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SecondaryCpuReport {
    pub detected: usize,
    pub started: usize,
    pub failed: usize,
}

/// Install the BSP's GS base before descriptor and syscall setup.
pub(crate) fn initialize_boot_cpu() {
    if BOOT_INITIALIZED.swap(true, Ordering::AcqRel) {
        return;
    }
    CPU_LOCALS[0].cpu_id.store(0, Ordering::Relaxed);
    install_current_cpu(0);
    APIC_IDS[0].store(apic::local_apic_id().unwrap_or(0), Ordering::Release);
    AP_STARTED[0].store(true, Ordering::Release);
}

/// Read the current logical CPU through the GS per-CPU anchor.
pub fn current_cpu_id() -> usize {
    #[cfg(target_os = "none")]
    {
        let gs = unsafe { read_gs_base() };
        if gs != 0 {
            for cpu in 0..MAX_CPUS {
                let local = core::ptr::addr_of!(CPU_LOCALS[cpu]) as usize;
                if local == gs {
                    return cpu;
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "none"))]
    {
        HOSTED_CPU_ID.load(Ordering::Acquire).min(MAX_CPUS - 1)
    }
}

pub(crate) fn current_local_address() -> usize {
    let cpu = current_cpu_id();
    core::ptr::addr_of!(CPU_LOCALS[cpu]) as usize
}

pub(crate) fn set_kernel_stack_top(stack_top: usize) {
    let cpu = current_cpu_id();
    CPU_LOCALS[cpu]
        .kernel_stack_top
        .store(stack_top, Ordering::Release);
}

#[inline]
pub(crate) fn set_current_task(task: usize) {
    let cpu = current_cpu_id();
    CPU_LOCALS[cpu].current_task.store(task, Ordering::Release);
}

#[inline]
pub(crate) fn set_current_task_with_work(task: usize, work: usize) {
    let cpu = current_cpu_id();
    let local = &CPU_LOCALS[cpu];
    local.user_return_work.store(work, Ordering::Release);
    local.current_task.store(task, Ordering::Release);
}

#[inline]
pub(crate) fn current_task() -> usize {
    let cpu = current_cpu_id();
    CPU_LOCALS[cpu].current_task.load(Ordering::Acquire)
}

#[inline]
pub(crate) fn current_user_return_work() -> usize {
    let cpu = current_cpu_id();
    CPU_LOCALS[cpu].user_return_work.load(Ordering::Acquire)
}

pub(crate) fn install_current_cpu(cpu: usize) {
    if cpu >= MAX_CPUS {
        return;
    }
    CPU_LOCALS[cpu].cpu_id.store(cpu, Ordering::Relaxed);
    #[cfg(target_os = "none")]
    unsafe {
        super::write_msr(MSR_GS_BASE, core::ptr::addr_of!(CPU_LOCALS[cpu]) as usize);
    }
    #[cfg(not(target_os = "none"))]
    HOSTED_CPU_ID.store(cpu, Ordering::Release);
}

#[cfg(target_os = "none")]
unsafe fn read_gs_base() -> usize {
    unsafe { super::read_msr(MSR_GS_BASE) }
}

fn apic_id_for(logical_id: usize) -> Option<u32> {
    let id = APIC_IDS.get(logical_id)?.load(Ordering::Acquire);
    (id != UNKNOWN_APIC_ID).then_some(id)
}

fn topology_ids() -> alloc::vec::Vec<u32> {
    let mut topology = general::dev::cpu::snapshot_topology();
    let boot_id = apic::local_apic_id().unwrap_or(0);
    topology.sort_by_key(|cpu| (cpu.reg != u64::from(boot_id), cpu.reg));
    topology
        .into_iter()
        .filter_map(|cpu| u32::try_from(cpu.reg).ok())
        .take(MAX_CPUS)
        .collect()
}

/// Start all MADT CPUs that fit the static scheduler capacity.
pub fn start_secondary_cpus() -> SecondaryCpuReport {
    initialize_boot_cpu();
    let ids = topology_ids();
    let detected = ids.len().max(1);
    for (logical, apic_id) in ids.iter().copied().enumerate() {
        APIC_IDS[logical].store(apic_id, Ordering::Release);
    }
    APIC_IDS[0].store(ids.first().copied().unwrap_or(0), Ordering::Release);
    AP_STARTED[0].store(true, Ordering::Release);
    ONLINE_CPUS.fetch_or(1, Ordering::Release);

    let mut report = SecondaryCpuReport {
        detected,
        started: 1,
        failed: 0,
    };
    if detected <= 1 {
        return report;
    }

    #[cfg(not(target_os = "none"))]
    {
        report.failed = detected - 1;
        return report;
    }

    #[cfg(target_os = "none")]
    {
        let Some(local_apic) = apic::local_apic_base() else {
            report.failed = detected - 1;
            return report;
        };
        let _ = local_apic;
        let cr3 = super::paging::read_cr3() & !(0xfff | (1usize << 63));
        // The trampoline executes MOV CR3 in 32-bit protected mode and can
        // therefore address only a page-table root below 4 GiB.  Refuse AP
        // startup rather than silently truncating a high physical root.
        if !ap_trampoline_cr3_supported(cr3) {
            report.failed = detected - 1;
            return report;
        }
        let entry = secondary_main as *const () as usize;
        for logical in 1..detected {
            let Some(apic_id) = apic_id_for(logical) else {
                report.failed += 1;
                continue;
            };
            if apic_id > 0xff {
                report.failed += 1;
                continue;
            }
            let idle = sched::spawn_idle_for_cpu(logical);
            let stack_top = idle.ensure_kernel_stack();
            AP_IDLE_TASKS[logical]
                .store(alloc::sync::Arc::into_raw(idle) as usize, Ordering::Release);
            AP_STACK_TOPS[logical].store(stack_top, Ordering::Release);
            unsafe {
                __x86_ap_trampoline_params = ApTrampolineParams {
                    cr3: cr3 as u64,
                    entry: entry as u64,
                    stack_top: stack_top as u64,
                    logical_id: logical as u32,
                    apic_id,
                };
            }
            AP_STARTED[logical].store(false, Ordering::Release);
            core::sync::atomic::fence(Ordering::SeqCst);
            if !apic::send_init_sipi(apic_id, AP_TRAMPOLINE_VECTOR) {
                drop_idle(logical);
                report.failed += 1;
                continue;
            }
            let deadline = super::time::stable_counter_raw()
                .saturating_add(counter_ticks_for_ns(AP_START_TIMEOUT_NS));
            while !AP_STARTED[logical].load(Ordering::Acquire)
                && super::time::stable_counter_raw() < deadline
            {
                core::hint::spin_loop();
            }
            if AP_STARTED[logical].load(Ordering::Acquire) {
                report.started += 1;
            } else {
                drop_idle(logical);
                let _ = sched::offline_cpu(logical);
                report.failed += 1;
            }
        }
    }
    report
}

#[cfg(target_os = "none")]
fn drop_idle(logical: usize) {
    let ptr = AP_IDLE_TASKS[logical].swap(0, Ordering::AcqRel);
    if ptr != 0 {
        unsafe { drop(alloc::sync::Arc::from_raw(ptr as *const sched::Task)) };
    }
}

#[cfg(target_os = "none")]
#[inline(never)]
pub(crate) extern "C" fn secondary_main(logical_id: usize) -> ! {
    if logical_id == 0 || logical_id >= MAX_CPUS {
        super::interrupt::disable();
        super::interrupt::halt();
    }
    install_current_cpu(logical_id);
    let stack_top = AP_STACK_TOPS[logical_id].load(Ordering::Acquire);
    if stack_top == 0 {
        super::interrupt::disable();
        super::interrupt::halt();
    }
    set_kernel_stack_top(stack_top);
    unsafe { super::descriptor::initialize_current_cpu(stack_top) };
    if super::fpu::init_secondary_cpu().is_err() {
        // XCR0 is architectural state local to each CPU.  Continuing with an
        // AP that cannot represent the BSP's global policy would let XSAVE
        // leak or truncate user vector registers, so it must never go online.
        super::interrupt::disable();
        super::interrupt::halt();
    }
    let _ = unsafe { super::trap::install_exception_entry() };
    super::activate_kernel_page_table();
    if !super::apic::initialize_current_local_apic() {
        super::interrupt::disable();
        super::interrupt::halt();
    }
    super::time::initialize_local_timer();

    let ptr = AP_IDLE_TASKS[logical_id].swap(0, Ordering::AcqRel);
    if ptr == 0 {
        super::interrupt::disable();
        super::interrupt::halt();
    }
    let idle = unsafe { alloc::sync::Arc::from_raw(ptr as *const sched::Task) };
    if sched::adopt_cpu_current(logical_id, idle.clone()).is_err() {
        super::interrupt::disable();
        super::interrupt::halt();
    }
    <super::task::X86_64TaskOps as general::TaskOps>::set_kernel_trap_stack(
        idle.ensure_kernel_stack(),
    );
    AP_STARTED[logical_id].store(true, Ordering::Release);
    ONLINE_CPUS.fetch_or(1usize << logical_id, Ordering::Release);
    super::interrupt::enable();
    sched::cpu_start_scheduling(logical_id)
}

fn send_reschedule(cpu_id: usize) {
    if !cpu_is_online(cpu_id) || cpu_id == current_cpu_id() {
        return;
    }
    if let Some(apic_id) = apic_id_for(cpu_id) {
        let _ = apic::send_ipi(apic_id, super::apic::RESCHEDULE_VECTOR);
    }
}

fn send_membarrier(cpu_id: usize) -> bool {
    if !cpu_is_online(cpu_id) || cpu_id == current_cpu_id() {
        return false;
    }
    apic_id_for(cpu_id).is_some_and(|apic_id| apic::send_ipi(apic_id, super::apic::IPI_VECTOR))
}

#[inline]
const fn cpu_bit(cpu_id: usize) -> usize {
    if cpu_id < usize::BITS as usize {
        1usize << cpu_id
    } else {
        0
    }
}

#[inline]
const fn shootdown_sequence_reached(completed: usize, expected: usize) -> bool {
    completed.wrapping_sub(expected) <= usize::MAX / 2
}

#[inline]
fn tlb_shootdown_pending(cpu_id: usize) -> bool {
    cpu_id < MAX_CPUS
        && !shootdown_sequence_reached(
            TLB_COMPLETED[cpu_id].load(Ordering::Relaxed),
            TLB_REQUESTED[cpu_id].load(Ordering::Acquire),
        )
}

/// Service one coalesced TLB request on the current CPU.
///
/// A full CR3 reload is deliberately conservative: x86 currently does not
/// allocate PCIDs, and one reload covers every user mapping update published
/// before the observed request generation.  Processing a single generation
/// keeps this urgent-work hook bounded; a racing producer leaves the sequence
/// pending and either its IPI or the urgent poll path invokes us again.
pub(crate) fn handle_tlb_shootdown() {
    let cpu = current_cpu_id();
    if cpu >= MAX_CPUS {
        return;
    }
    let requested = TLB_REQUESTED[cpu].load(Ordering::Acquire);
    let completed = TLB_COMPLETED[cpu].load(Ordering::Relaxed);
    if shootdown_sequence_reached(completed, requested) {
        return;
    }
    unsafe { super::paging::flush_tlb() };
    TLB_COMPLETED[cpu].store(requested, Ordering::Release);
}

#[inline]
fn remote_tlb_targets(active_cpus: usize, online_cpus: usize, source: usize) -> usize {
    active_cpus & online_cpus & !cpu_bit(source)
}

#[inline]
const fn ap_trampoline_cr3_supported(cr3: usize) -> bool {
    cr3 != 0 && cr3 & 0xfff == 0 && cr3 <= u32::MAX as usize
}

#[inline]
fn counter_ticks_for_ns(duration_ns: u64) -> u64 {
    let product =
        u128::from(super::time::stable_counter_hz().max(1)).saturating_mul(u128::from(duration_ns));
    u64::try_from(product.saturating_add(999_999_999) / 1_000_000_000)
        .unwrap_or(u64::MAX)
        .max(1)
}

fn send_tlb_ipi(cpu_id: usize) -> bool {
    cpu_is_online(cpu_id)
        && apic_id_for(cpu_id)
            .is_some_and(|apic_id| apic::send_ipi(apic_id, super::apic::IPI_VECTOR))
}

fn wait_for_tlb_ack(cpu_id: usize, expected: usize) {
    let start = super::time::stable_counter_raw_ordered();
    let mut last_kick = start;
    let retry_ticks = counter_ticks_for_ns(TLB_SHOOTDOWN_RETRY_NS);
    let timeout_ticks = counter_ticks_for_ns(TLB_SHOOTDOWN_TIMEOUT_NS);
    loop {
        if shootdown_sequence_reached(TLB_COMPLETED[cpu_id].load(Ordering::Acquire), expected) {
            return;
        }

        // Crossed shootdowns may reach this CPU while local interrupts are
        // disabled by the caller.  Cooperatively service them so two senders
        // cannot wait on each other indefinitely.
        poll_urgent();
        let now = super::time::stable_counter_raw_ordered();
        if now.wrapping_sub(start) >= timeout_ticks {
            panic!("x86 TLB shootdown timed out for CPU {cpu_id}");
        }
        if now.wrapping_sub(last_kick) >= retry_ticks {
            assert!(
                send_tlb_ipi(cpu_id),
                "x86 TLB shootdown IPI delivery failed"
            );
            last_kick = now;
        }
        core::hint::spin_loop();
    }
}

/// Synchronize stale translations on every CPU currently resident in a user
/// address space.  The caller invokes this only after publishing an update to
/// an existing PTE and after releasing the page-table update lock.
pub(crate) fn shootdown_user_tlb(active_cpus: usize) {
    let source = current_cpu_id();
    let source_bit = cpu_bit(source);
    if active_cpus & source_bit != 0 {
        unsafe { super::paging::flush_tlb() };
    }
    let targets = remote_tlb_targets(active_cpus, ONLINE_CPUS.load(Ordering::Acquire), source);
    if targets == 0 {
        return;
    }

    // Pair PTE publication with the target's Acquire request load.  Per-target
    // monotonic generations allow several concurrent updates to be covered by
    // one conservative full flush without losing an acknowledgement.
    fence(Ordering::SeqCst);
    let mut expected = [0usize; MAX_CPUS];
    for cpu in 0..MAX_CPUS {
        if targets & cpu_bit(cpu) == 0 {
            continue;
        }
        expected[cpu] = TLB_REQUESTED[cpu]
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        sched::mark_urgent_work(cpu);
    }
    for cpu in 0..MAX_CPUS {
        if targets & cpu_bit(cpu) != 0 {
            assert!(send_tlb_ipi(cpu), "x86 TLB shootdown IPI delivery failed");
        }
    }
    for cpu in 0..MAX_CPUS {
        if targets & cpu_bit(cpu) != 0 {
            wait_for_tlb_ack(cpu, expected[cpu]);
        }
    }
    handle_tlb_shootdown();
    fence(Ordering::SeqCst);
}

fn cpu_is_online(cpu_id: usize) -> bool {
    cpu_id < MAX_CPUS && ONLINE_CPUS.load(Ordering::Acquire) & (1usize << cpu_id) != 0
}

fn has_urgent_work() -> bool {
    let cpu = current_cpu_id();
    tlb_shootdown_pending(cpu) || sched::membarrier_pending_on(cpu)
}

fn poll_urgent() {
    handle_tlb_shootdown();
    sched::handle_membarrier_ipi_on(current_cpu_id());
    sched::acknowledge_resched_notification();
}

/// x86 reschedule/membarrier IPI hooks consumed by the generic scheduler.
pub(crate) static CPU_CONTROL_OPS: CpuControlOps = CpuControlOps {
    send_resched: send_reschedule,
    send_membarrier,
    has_urgent_work,
    poll_urgent,
    is_online: cpu_is_online,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trampoline_vector_is_representable_by_sipi() {
        assert!(u32::from(AP_TRAMPOLINE_VECTOR) <= 0xff);
        assert_eq!(offset_of!(ApTrampolineParams, stack_top), 16);
        assert_eq!(offset_of!(CpuLocal, cpu_id), 16);
        assert!(ap_trampoline_cr3_supported(0x1000));
        assert!(ap_trampoline_cr3_supported(0xffff_f000));
        assert!(!ap_trampoline_cr3_supported(0));
        assert!(!ap_trampoline_cr3_supported(0x1001));
        #[cfg(target_pointer_width = "64")]
        assert!(!ap_trampoline_cr3_supported(0x1_0000_0000));
    }

    #[test]
    fn cpu_local_scheduler_state_is_not_shared() {
        let first = CpuLocal::new(0);
        let second = CpuLocal::new(1);
        first.current_task.store(0x1000, Ordering::Release);
        first.user_return_work.store(0x2000, Ordering::Release);
        assert_eq!(first.current_task.load(Ordering::Acquire), 0x1000);
        assert_eq!(first.user_return_work.load(Ordering::Acquire), 0x2000);
        assert_eq!(second.current_task.load(Ordering::Acquire), 0);
        assert_eq!(second.user_return_work.load(Ordering::Acquire), 0);
    }

    #[test]
    fn tlb_target_selection_excludes_source_and_offline_cpus() {
        assert_eq!(remote_tlb_targets(0b1111, 0b1101, 0), 0b1100);
        assert_eq!(remote_tlb_targets(0b1111, 0b1101, 2), 0b1001);
        assert_eq!(remote_tlb_targets(0, usize::MAX, 0), 0);
    }

    #[test]
    fn shootdown_generation_comparison_handles_wraparound() {
        assert!(shootdown_sequence_reached(7, 7));
        assert!(shootdown_sequence_reached(8, 7));
        assert!(!shootdown_sequence_reached(6, 7));
        assert!(shootdown_sequence_reached(0, usize::MAX));
    }

    #[test]
    fn hosted_cpu_identity_is_bounded_and_switchable() {
        install_current_cpu(0);
        assert_eq!(current_cpu_id(), 0);
        install_current_cpu((MAX_CPUS - 1).min(2));
        assert_eq!(current_cpu_id(), (MAX_CPUS - 1).min(2));
        install_current_cpu(0);
    }

    #[test]
    fn topology_report_never_exceeds_scheduler_capacity() {
        let report = start_secondary_cpus();
        assert!(report.detected >= report.started);
        assert!(report.detected <= MAX_CPUS || report.failed != 0);
    }
}
