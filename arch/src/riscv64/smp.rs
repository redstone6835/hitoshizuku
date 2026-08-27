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

fn send_software_ipi(logical_id: usize) -> bool {
    let Some(hart_id) = physical_hart_id(logical_id) else {
        return false;
    };
    let ret = sbi::send_ipi(1, hart_id);
    ret.is_ok()
}

/// Requests every online remote hart to synchronize its external IRQ state.
pub(super) fn request_external_irq_sync() {
    let current = crate::riscv64::specific::current_cpu_id();
    let online = ONLINE_HARTS.load(Ordering::Acquire);
    for logical_id in 0..MAX_CPUS {
        if logical_id == current || online & (1 << logical_id) == 0 {
            continue;
        }
        if !send_software_ipi(logical_id) {
            let hart_id = physical_hart_id(logical_id).unwrap_or(UNKNOWN_HART_ID);
            log::warning!(
                "[smp] external IRQ sync IPI failed: logical={} hart={}",
                logical_id,
                hart_id,
            );
        }
    }
}

fn send_reschedule(logical_id: usize) {
    if !send_software_ipi(logical_id) {
        let hart_id = physical_hart_id(logical_id).unwrap_or(UNKNOWN_HART_ID);
        log::warning!(
            "[smp] reschedule IPI failed: logical={} hart={}",
            logical_id,
            hart_id,
        );
    }
}

fn send_membarrier(logical_id: usize) -> bool {
    let sent = send_software_ipi(logical_id);
    if !sent {
        let hart_id = physical_hart_id(logical_id).unwrap_or(UNKNOWN_HART_ID);
        log::warning!(
            "[smp] membarrier IPI failed: logical={} hart={}",
            logical_id,
            hart_id,
        );
    }
    sent
}

fn poll_urgent() {
    sched::handle_membarrier_ipi_on(crate::riscv64::specific::current_cpu_id());
}

fn has_urgent_work() -> bool {
    sched::membarrier_pending_on(crate::riscv64::specific::current_cpu_id())
}

pub(crate) static CPU_CONTROL_OPS: CpuControlOps = CpuControlOps {
    send_resched: send_reschedule,
    send_membarrier,
    has_urgent_work,
    poll_urgent,
    is_online: cpu_is_online,
};

pub(crate) fn handle_ipi() {
    super::trap::sync_external_irq_current_cpu();
    sched::poll_urgent_work();
    // request_resched() 在发送 IPI 前已发布目标 CPU 的 need_resched；trap 返回路径
    // 会在安全边界消费该标志。RFENCE 由 OpenSBI 同步执行，不进入 S-mode handler。
    sched::acknowledge_resched_notification();
}

fn for_each_remote_hart_mask(
    logical_targets: usize,
    mut action: impl FnMut(usize, usize) -> sbi::SbiRet,
) {
    if logical_targets == 0 {
        return;
    }
    let online = ONLINE_HARTS.load(Ordering::Acquire);
    if online == 0 {
        return;
    }
    let source = crate::riscv64::specific::current_cpu_id();
    let targets = logical_targets & online & !(1 << source);
    if targets == 0 {
        return;
    }
    // SBI 的 hart mask 允许一次 ecall 通知一组 hart。旧实现按逻辑 CPU
    // 逐个调用 RFENCE；在 QEMU/OpenSBI 上这会把一次页表更新放大成最多
    // MAX_CPUS-1 次 M-mode 往返。物理 hart 编号不保证连续，因此先按
    // `hart_mask_base` 可表示的窗口分组；通常 QEMU 的 hart 编号落在同一
    // 个 usize 窗口内，只产生一次 ecall。
    let mut hart_ids = [0usize; MAX_CPUS];
    let mut count = 0usize;
    for logical_id in 0..MAX_CPUS {
        if targets & (1 << logical_id) == 0 {
            continue;
        }
        hart_ids[count] = physical_hart_id(logical_id).expect("[smp] online hart has no mapping");
        count += 1;
    }

    let mask_bits = usize::BITS as usize;
    let min_hart = hart_ids[..count]
        .iter()
        .copied()
        .min()
        .expect("[smp] non-empty remote hart set");
    let max_hart = hart_ids[..count]
        .iter()
        .copied()
        .max()
        .expect("[smp] non-empty remote hart set");
    if max_hart - min_hart < mask_bits {
        let mut mask = 0usize;
        for &hart_id in &hart_ids[..count] {
            mask |= 1usize << (hart_id - min_hart);
        }
        let ret = action(mask, min_hart);
        assert!(
            ret.is_ok(),
            "[smp] remote fence failed: mask={:#x} base={} error={}",
            mask,
            min_hart,
            ret.error
        );
        return;
    }

    // 物理 hart 跨越一个 SBI mask 窗口时才需要排序并分组；该路径只出现在
    // 非连续或编号稀疏的硬件拓扑上。
    hart_ids[..count].sort_unstable();
    let mut first = 0usize;
    while first < count {
        let base = hart_ids[first];
        let mut mask = 0usize;
        let mut next = first;
        while next < count {
            let offset = hart_ids[next].wrapping_sub(base);
            if offset >= mask_bits {
                break;
            }
            mask |= 1usize << offset;
            next += 1;
        }
        let ret = action(mask, base);
        assert!(
            ret.is_ok(),
            "[smp] remote fence failed: mask={:#x} base={} error={}",
            mask,
            base,
            ret.error
        );
        first = next;
    }
}

pub(crate) fn remote_sfence_vma(asid: Option<usize>, address: Option<usize>) {
    remote_sfence_vma_on(usize::MAX, asid, address);
}

/// 只在指定逻辑 CPU 集合上执行远端 TLB 失效。
///
/// `logical_targets` 来自用户地址空间记录的活跃 CPU 位图；函数仍会与在线集合
/// 求交并排除当前 CPU，因此离线 CPU 和本地 hart 不会进入 SBI hart mask。
pub(crate) fn remote_sfence_vma_on(
    logical_targets: usize,
    asid: Option<usize>,
    address: Option<usize>,
) {
    let start = address.unwrap_or(0);
    let size = address.map_or(0, |_| allocator::PAGE_SIZE);
    remote_sfence_vma_range_on(logical_targets, asid, start, size);
}

/// 使用一次 SBI RFENCE 失效指定逻辑 CPU 上的连续虚拟地址范围。
///
/// 本地 hart 不包含在目标集合中，调用方需要先完成对应的本地 `sfence.vma`。
/// `size == 0` 保留 SBI 的“全部地址”语义；非零范围必须按基本页对齐。
pub(crate) fn remote_sfence_vma_range_on(
    logical_targets: usize,
    asid: Option<usize>,
    start: usize,
    size: usize,
) {
    debug_assert!(size == 0 || start.is_multiple_of(allocator::PAGE_SIZE));
    debug_assert!(size == 0 || size.is_multiple_of(allocator::PAGE_SIZE));
    debug_assert!(size == 0 || start.checked_add(size).is_some());
    for_each_remote_hart_mask(logical_targets, |hart_mask, hart_mask_base| match asid {
        Some(asid) => sbi::remote_sfence_vma_asid(hart_mask, hart_mask_base, start, size, asid),
        None => sbi::remote_sfence_vma(hart_mask, hart_mask_base, start, size),
    });
}

pub(crate) fn sync_icache_remote() {
    for_each_remote_hart_mask(usize::MAX, sbi::remote_fence_i);
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
