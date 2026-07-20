//! RISC-V64 多 hart 启动、调度 IPI 与远端 fence。

use alloc::sync::Arc;
use core::arch::naked_asm;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use general::TaskOps;
use sched::arch_hooks::CpuControlOps;

use super::addr::{KERNEL_VA_OFFSET, virt_to_phys};
use super::boot::BOOT_HART_ID;
use super::heap_vm;
use super::sbi;
use super::specific::{MAX_HARTS, SATP_MODE_SV48, init_secondary_hart_local};
use super::task::Riscv64TaskOps;
use super::time;
use super::trap::{Riscv64InterruptOps, Riscv64MessageInterruptOps};

pub const MAX_CPUS: usize = sched::NR_CPUS;
const UNKNOWN_HART_ID: usize = usize::MAX;
const AP_STACK_SIZE: usize = 32 * 1024;
const AP_STACK_SHIFT: usize = AP_STACK_SIZE.trailing_zeros() as usize;
const STARTUP_TIMEOUT_SECONDS: u64 = 1;

const _: () = assert!(MAX_CPUS == MAX_HARTS);
const _: () = assert!(AP_STACK_SIZE.is_power_of_two());

static PHYSICAL_HART_IDS: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(UNKNOWN_HART_ID) }; MAX_CPUS];
static ONLINE_HARTS: AtomicUsize = AtomicUsize::new(0);
static STARTED_HARTS: AtomicUsize = AtomicUsize::new(0);
static AP_IDLE_TASKS: [AtomicPtr<sched::Task>; MAX_CPUS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
static REMOTE_FENCE_LOCK: spin::Mutex<()> = spin::Mutex::new(());

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

fn physical_hart_id(logical_id: usize) -> Option<usize> {
    let hart_id = PHYSICAL_HART_IDS.get(logical_id)?.load(Ordering::Acquire);
    (hart_id != UNKNOWN_HART_ID).then_some(hart_id)
}

fn cpu_is_online(logical_id: usize) -> bool {
    logical_id < MAX_CPUS && ONLINE_HARTS.load(Ordering::Acquire) & (1 << logical_id) != 0
}

fn send_reschedule(logical_id: usize) {
    let Some(hart_id) = physical_hart_id(logical_id) else {
        return;
    };
    let ret = sbi::send_ipi(1, hart_id);
    if !ret.is_ok() {
        log::warning!(
            "[smp] reschedule IPI failed: logical={} hart={} error={}",
            logical_id,
            hart_id,
            ret.error
        );
    }
}

pub(crate) static CPU_CONTROL_OPS: CpuControlOps = CpuControlOps {
    send_resched: send_reschedule,
    is_online: cpu_is_online,
};

pub(crate) fn handle_ipi() {
    // request_resched() 在发送 IPI 前已发布目标 CPU 的 need_resched；trap 返回路径
    // 会在安全边界消费该标志。RFENCE 由 OpenSBI 同步执行，不进入 S-mode handler。
}

fn for_each_remote_hart(mut action: impl FnMut(usize) -> sbi::SbiRet) {
    // SBI RFENCE 是同步操作。若多个 hart 同时互相发起 fence，固件可能让每个
    // 调用都等待另一个仍停留在 RFENCE ecall 中的 hart。统一串行化所有远端
    // TLB/I-cache fence，避免形成跨 hart 等待环。
    let _guard = REMOTE_FENCE_LOCK.lock();
    let source = crate::riscv64::specific::current_cpu_id();
    let targets = ONLINE_HARTS.load(Ordering::Acquire) & !(1 << source);
    for logical_id in 0..MAX_CPUS {
        if targets & (1 << logical_id) == 0 {
            continue;
        }
        let hart_id = physical_hart_id(logical_id).expect("[smp] online hart has no mapping");
        let ret = action(hart_id);
        assert!(
            ret.is_ok(),
            "[smp] remote fence failed: logical={} hart={} error={}",
            logical_id,
            hart_id,
            ret.error
        );
    }
}

pub(crate) fn remote_sfence_vma(asid: Option<usize>, address: Option<usize>) {
    let start = address.unwrap_or(0);
    let size = address.map_or(0, |_| allocator::PAGE_SIZE);
    for_each_remote_hart(|hart_id| match asid {
        Some(asid) => sbi::remote_sfence_vma_asid(1, hart_id, start, size, asid),
        None => sbi::remote_sfence_vma(1, hart_id, start, size),
    });
}

pub(crate) fn sync_icache_remote() {
    for_each_remote_hart(|hart_id| sbi::remote_fence_i(1, hart_id));
}

pub fn start_secondary_cpus() -> SecondaryCpuReport {
    let boot_hart_id = BOOT_HART_ID.load(Ordering::Acquire);
    let mut topology = general::dev::cpu::snapshot_topology();
    topology.sort_by_key(|cpu| (cpu.reg != boot_hart_id as u64, cpu.reg));
    for (logical_id, cpu) in topology.iter_mut().enumerate() {
        cpu.logical_id = logical_id as u32;
    }
    general::dev::cpu::install_topology(topology.clone());

    let detected = topology.len().min(MAX_CPUS);
    for (logical_id, cpu) in topology.iter().take(detected).enumerate() {
        PHYSICAL_HART_IDS[logical_id].store(cpu.reg as usize, Ordering::Release);
    }
    ONLINE_HARTS.store(1, Ordering::Release);
    STARTED_HARTS.store(1, Ordering::Release);

    let mut report = SecondaryCpuReport {
        detected,
        started: 1,
        failed: 0,
    };
    if detected <= 1 {
        return report;
    }
    if !(sbi::hsm_available() && sbi::ipi_available() && sbi::rfence_available()) {
        log::warning!(
            "[smp] secondary hart startup disabled: hsm={} ipi={} rfence={}",
            sbi::hsm_available(),
            sbi::ipi_available(),
            sbi::rfence_available()
        );
        report.failed = detected - 1;
        return report;
    }

    heap_vm::install_secondary_identity_mapping();
    let entry = virt_to_phys(secondary_entry as *const () as usize);
    for logical_id in 1..detected {
        let hart_id = physical_hart_id(logical_id).expect("[smp] missing hart mapping");
        let idle = sched::spawn_idle_for_cpu(logical_id);
        AP_IDLE_TASKS[logical_id].store(Arc::into_raw(idle).cast_mut(), Ordering::Release);
        core::sync::atomic::fence(Ordering::SeqCst);

        let ret = sbi::hart_start(hart_id, entry, logical_id);
        if !ret.is_ok() {
            let idle_ptr = AP_IDLE_TASKS[logical_id].swap(core::ptr::null_mut(), Ordering::AcqRel);
            if !idle_ptr.is_null() {
                unsafe { drop(Arc::from_raw(idle_ptr)) };
            }
            report.failed += 1;
            log::warning!(
                "[smp] hart start failed: logical={} hart={} error={} status={:?}",
                logical_id,
                hart_id,
                ret.error,
                sbi::hart_get_status(hart_id)
            );
            continue;
        }

        let timeout = time::stable_counter_raw()
            .saturating_add(time::stable_counter_hz().saturating_mul(STARTUP_TIMEOUT_SECONDS));
        while STARTED_HARTS.load(Ordering::Acquire) & (1 << logical_id) == 0
            && time::stable_counter_raw() < timeout
        {
            core::hint::spin_loop();
        }
        if STARTED_HARTS.load(Ordering::Acquire) & (1 << logical_id) == 0 {
            panic!(
                "[smp] hart startup timed out: logical={} hart={} status={:?}",
                logical_id,
                hart_id,
                sbi::hart_get_status(hart_id)
            );
        }
        while !sched::is_cpu_active(logical_id) && time::stable_counter_raw() < timeout {
            core::hint::spin_loop();
        }
        if sched::is_cpu_active(logical_id) {
            report.started += 1;
        } else {
            panic!(
                "[smp] hart did not enter scheduler: logical={} hart={}",
                logical_id, hart_id
            );
        }
    }
    heap_vm::remove_secondary_identity_mapping();
    report
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn secondary_entry() -> ! {
    naked_asm!(
        "csrci sstatus, 2",
        "csrw sscratch, zero",
        "li t0, {max_cpus}",
        "bgeu a1, t0, 9f",

        // HSM 以 satp=0 进入。临时 identity mapping 让写 satp 后仍能执行到高半区跳转。
        "la t0, {root}",
        "ld t2, 0(t0)",
        "beqz t2, 9f",
        "srli t2, t2, 12",
        "li t3, {satp_mode}",
        "slli t3, t3, 60",
        "or t2, t2, t3",

        // 为每个逻辑 CPU 使用独立的启动栈；切换页表后同时投影到高半区。
        "addi t0, a1, 1",
        "slli t0, t0, {stack_shift}",
        "la sp, {stacks}",
        "add sp, sp, t0",
        "la t0, {main}",
        "li t1, {va_hi32}",
        "slli t1, t1, 32",
        "add t0, t0, t1",
        "add sp, sp, t1",
        "li t4, 3 << 13",
        "csrs sstatus, t4",

        "fence w, w",
        "csrw satp, t2",
        "sfence.vma zero, zero",
        "fence.i",
        "jr t0",

        "9: wfi",
        "j 9b",
        root = sym super::heap_vm::KERNEL_PAGE_TABLE_ROOT,
        stacks = sym SECONDARY_STACKS,
        main = sym secondary_main,
        max_cpus = const MAX_CPUS,
        stack_shift = const AP_STACK_SHIFT,
        satp_mode = const (SATP_MODE_SV48 >> 60),
        va_hi32 = const (KERNEL_VA_OFFSET >> 32),
    )
}

unsafe extern "C" fn secondary_main(hart_id: usize, logical_id: usize) -> ! {
    let expected_hart = physical_hart_id(logical_id);
    if expected_hart != Some(hart_id) {
        park_secondary_hart();
    }

    let kernel_gp: usize;
    unsafe { core::arch::asm!("mv {}, gp", out(reg) kernel_gp, options(nomem, nostack)) };
    let local = unsafe { init_secondary_hart_local(logical_id, hart_id, kernel_gp) };
    unsafe {
        core::arch::asm!("mv tp, {}", in(reg) local, options(nomem, nostack));
        super::trap::install_exception_entry();
    }
    time::init_periodic_timer(time::timer_hz());
    unsafe {
        Riscv64MessageInterruptOps::set_message_interrupt_enable_bits(super::specific::SIE_SSIE)
    };

    let idle_ptr = AP_IDLE_TASKS[logical_id].swap(core::ptr::null_mut(), Ordering::AcqRel);
    if idle_ptr.is_null() {
        park_secondary_hart();
    }
    let idle = unsafe { Arc::from_raw(idle_ptr) };
    sched::adopt_cpu_current(logical_id, idle.clone())
        .expect("[smp] failed to install AP current task");
    <Riscv64TaskOps as TaskOps>::set_kernel_trap_stack(idle.ensure_kernel_stack());

    ONLINE_HARTS.fetch_or(1 << logical_id, Ordering::Release);
    STARTED_HARTS.fetch_or(1 << logical_id, Ordering::Release);
    log::info!("[smp] CPU online: logical={} hart={}", logical_id, hart_id);
    unsafe { Riscv64InterruptOps::enable_interrupts() };
    sched::cpu_start_scheduling(logical_id)
}

fn park_secondary_hart() -> ! {
    let _ = sbi::hart_stop();
    unsafe { Riscv64InterruptOps::disable_interrupts() };
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
