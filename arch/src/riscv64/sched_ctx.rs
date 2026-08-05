//! RISC-V64 内核上下文切换。
//!
//! 保存/恢复 callee-saved 寄存器（ra, sp, s0-s11），实现内核线程之间的调度切换。
//! tp 指向 per-hart HartLocal，跨切换不变，不保存。
//!
//! 新线程首次被切入时 `ret` 到 `__kthread_trampoline`，trampoline 把
//! s0（entry）和 s1（arg）分别送入 t0/a0 后 `jr t0`。entry 签名为
//! `unsafe extern "C" fn(usize) -> !`，永不返回。

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use general::TaskOps;
use sched::arch_hooks::{
    ArchContextOps, ArchDeadlineTimerOps, ArchIdleOps, ArchTimeOps, ArchTrapOps, KernelEntry,
};

use crate::riscv64::specific::{
    CONTEXT_SWITCH_TOKEN_STRIDE, HART_LOCAL_CONTEXT_SWITCH_SEQ_OFF, current_cpu_id,
    kernel_timestamp_ns,
};
use crate::riscv64::task::Riscv64TaskOps;
use crate::riscv64::trap::Riscv64InterruptOps;

pub(crate) const KCTX_SIZE: usize = 112; // 14 × u64
pub(crate) const KCTX_ALIGN: usize = 16;

// 内核上下文帧布局（14 × 8 = 112 字节）：
//   +0x00 ra    +0x08 sp    +0x10 s0    +0x18 s1
//   +0x20 s2    +0x28 s3    +0x30 s4    +0x38 s5
//   +0x40 s6    +0x48 s7    +0x50 s8    +0x58 s9
//   +0x60 s10   +0x68 s11
const RA_OFFSET: usize = 0x00;
const SP_OFFSET: usize = 0x08;
const S0_OFFSET: usize = 0x10;
const S1_OFFSET: usize = 0x18;
const S2_OFFSET: usize = 0x20;
const S3_OFFSET: usize = 0x28;
const S4_OFFSET: usize = 0x30;
const S5_OFFSET: usize = 0x38;
const S6_OFFSET: usize = 0x40;
const S7_OFFSET: usize = 0x48;
const S8_OFFSET: usize = 0x50;
const S9_OFFSET: usize = 0x58;
const S10_OFFSET: usize = 0x60;
const S11_OFFSET: usize = 0x68;

/// # Safety
///
/// - `ctx` 必须指向至少 `KCTX_SIZE` 字节的可写、`KCTX_ALIGN` 对齐内存
/// - `stack_top` 必须是有效内核栈顶（16 字节对齐，且栈空间已分配）
/// - `entry` 签名为 `fn(usize) -> !`，永不返回
unsafe fn init_kernel_context(ctx: NonNull<u8>, stack_top: usize, entry: KernelEntry, arg: usize) {
    let base = ctx.as_ptr();
    unsafe {
        core::ptr::write_bytes(base, 0, KCTX_SIZE);
        let w = |off: usize, v: usize| (base.add(off) as *mut usize).write(v);
        w(RA_OFFSET, __kthread_trampoline as usize);
        w(SP_OFFSET, stack_top & !0xF); // 保证 16 字节对齐
        w(S0_OFFSET, entry as usize);
        w(S1_OFFSET, arg);
    }
}

#[unsafe(naked)]
unsafe extern "C" fn switch_context(
    _prev: NonNull<u8>,
    _next: NonNull<u8>,
    _prev_on_cpu: NonNull<core::sync::atomic::AtomicUsize>,
) {
    core::arch::naked_asm!(
        // fast syscall 可在内部阻塞并迁移，随后从同一内核调用点继续。token 低位
        // 编码 hart id，按固定步长递增，因此跨 hart 或本 hart 切换都必然变化。
        "ld t0, {switch_seq}(tp)",
        "addi t0, t0, {switch_stride}",
        "sd t0, {switch_seq}(tp)",

        "sd ra,  {ra}(a0)",
        "sd sp,  {sp}(a0)",
        "sd s0,  {s0}(a0)",
        "sd s1,  {s1}(a0)",
        "sd s2,  {s2}(a0)",
        "sd s3,  {s3}(a0)",
        "sd s4,  {s4}(a0)",
        "sd s5,  {s5}(a0)",
        "sd s6,  {s6}(a0)",
        "sd s7,  {s7}(a0)",
        "sd s8,  {s8}(a0)",
        "sd s9,  {s9}(a0)",
        "sd s10, {s10}(a0)",
        "sd s11, {s11}(a0)",

        // 只有保存完整上下文后，远端 CPU 才能认领并恢复 prev。
        "fence rw, w",
        "sd zero, 0(a2)",

        "ld ra,  {ra}(a1)",
        "ld sp,  {sp}(a1)",
        "ld s0,  {s0}(a1)",
        "ld s1,  {s1}(a1)",
        "ld s2,  {s2}(a1)",
        "ld s3,  {s3}(a1)",
        "ld s4,  {s4}(a1)",
        "ld s5,  {s5}(a1)",
        "ld s6,  {s6}(a1)",
        "ld s7,  {s7}(a1)",
        "ld s8,  {s8}(a1)",
        "ld s9,  {s9}(a1)",
        "ld s10, {s10}(a1)",
        "ld s11, {s11}(a1)",

        "ret",
        ra = const RA_OFFSET, sp = const SP_OFFSET,
        s0 = const S0_OFFSET, s1 = const S1_OFFSET,
        s2 = const S2_OFFSET, s3 = const S3_OFFSET,
        s4 = const S4_OFFSET, s5 = const S5_OFFSET,
        s6 = const S6_OFFSET, s7 = const S7_OFFSET,
        s8 = const S8_OFFSET, s9 = const S9_OFFSET,
        s10 = const S10_OFFSET, s11 = const S11_OFFSET,
        switch_seq = const HART_LOCAL_CONTEXT_SWITCH_SEQ_OFF,
        switch_stride = const CONTEXT_SWITCH_TOKEN_STRIDE,
    );
}

/// entry 签名为 `fn(usize) -> !`，永不返回，故 unimp 仅作 trap 安全网。
#[unsafe(naked)]
unsafe extern "C" fn __kthread_trampoline() {
    core::arch::naked_asm!("mv a0, s1", "jr s0", "unimp",);
}

static ARCH_CONTEXT_OPS: ArchContextOps = ArchContextOps {
    context_size: KCTX_SIZE,
    context_align: KCTX_ALIGN,
    init_kernel_context,
    switch_context,
};

static ARCH_TIME_OPS: ArchTimeOps = ArchTimeOps {
    now_ns: kernel_timestamp_ns,
    current_cpu_id,
};

static ARCH_DEADLINE_TIMER_OPS: ArchDeadlineTimerOps = ArchDeadlineTimerOps {
    reprogram: super::time::rearm_local_timer,
};

/// # Safety
///
/// `stack_top` 必须是当前任务的有效内核栈顶。
unsafe fn set_kernel_trap_stack_raw(stack_top: usize) {
    <Riscv64TaskOps as TaskOps>::set_kernel_trap_stack(stack_top);
}

/// # Safety
/// `task_ptr` 必须由调度器已发布且仍持有强引用的 current task 提供；
/// `cpu_work_ptr` 必须指向稳定的 CpuSchedState 返回工作 hint。
unsafe fn set_current_task_raw(task_ptr: usize, cpu_work_ptr: usize) {
    unsafe { crate::riscv64::specific::set_current_task_ptr(task_ptr, cpu_work_ptr) };
}

static ARCH_TRAP_OPS: ArchTrapOps = ArchTrapOps {
    set_kernel_trap_stack: set_kernel_trap_stack_raw,
    set_current_task: set_current_task_raw,
};

static ARCH_IDLE_OPS: ArchIdleOps = ArchIdleOps {
    idle_relax: riscv64_idle_relax,
};

fn riscv64_idle_relax() {
    unsafe {
        // idle 线程进入等待窗口前必须临时开本地中断，否则 timer/event 只能
        // 置 pending，无法真正打断 idle 并唤醒睡眠任务。
        Riscv64InterruptOps::enable_interrupts();
        if sched::needs_resched(current_cpu_id()) {
            Riscv64InterruptOps::disable_interrupts();
            return;
        }
        core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        Riscv64InterruptOps::disable_interrupts();
    }
}

static REGISTERED: AtomicBool = AtomicBool::new(false);

pub fn register() {
    if REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        sched::arch_hooks::register(&ARCH_CONTEXT_OPS);
        sched::arch_hooks::register_time(&ARCH_TIME_OPS);
        sched::arch_hooks::register_deadline_timer(&ARCH_DEADLINE_TIMER_OPS);
        sched::arch_hooks::register_trap(&ARCH_TRAP_OPS);
        sched::arch_hooks::register_idle(&ARCH_IDLE_OPS);
        sched::arch_hooks::register_cpu_control(&super::smp::CPU_CONTROL_OPS);
        crate::riscv64::mm::register();
        crate::riscv64::syscall::register();
    }
}
