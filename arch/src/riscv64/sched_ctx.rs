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
use sched::arch_hooks::{ArchContextOps, ArchTimeOps, ArchTrapOps, KernelEntry};

use crate::riscv64::specific::{current_cpu_id, kernel_timestamp_ns};
use crate::riscv64::task::Riscv64TaskOps;

pub(crate) const KCTX_SIZE: usize = 112; // 14 × u64
pub(crate) const KCTX_ALIGN: usize = 16;

// 内核上下文帧布局（14 × 8 = 112 字节）：
//   +0x00 ra    +0x08 sp    +0x10 s0    +0x18 s1
//   +0x20 s2    +0x28 s3    +0x30 s4    +0x38 s5
//   +0x40 s6    +0x48 s7    +0x50 s8    +0x58 s9
//   +0x60 s10   +0x68 s11
const RA_OFF: usize = 0x00;
const SP_OFF: usize = 0x08;
const S0_OFF: usize = 0x10;
const S1_OFF: usize = 0x18;
const S2_OFF: usize = 0x20;
const S3_OFF: usize = 0x28;
const S4_OFF: usize = 0x30;
const S5_OFF: usize = 0x38;
const S6_OFF: usize = 0x40;
const S7_OFF: usize = 0x48;
const S8_OFF: usize = 0x50;
const S9_OFF: usize = 0x58;
const S10_OFF: usize = 0x60;
const S11_OFF: usize = 0x68;

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
        w(RA_OFF, __kthread_trampoline as usize);
        w(SP_OFF, stack_top & !0xF); // 保证 16 字节对齐
        w(S0_OFF, entry as usize);
        w(S1_OFF, arg);
    }
}

#[unsafe(naked)]
unsafe extern "C" fn switch_context(_prev: NonNull<u8>, _next: NonNull<u8>) {
    core::arch::naked_asm!(
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
        ra = const RA_OFF, sp = const SP_OFF,
        s0 = const S0_OFF, s1 = const S1_OFF,
        s2 = const S2_OFF, s3 = const S3_OFF,
        s4 = const S4_OFF, s5 = const S5_OFF,
        s6 = const S6_OFF, s7 = const S7_OFF,
        s8 = const S8_OFF, s9 = const S9_OFF,
        s10 = const S10_OFF, s11 = const S11_OFF,
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

/// # Safety
///
/// `stack_top` 必须是当前任务的有效内核栈顶。
unsafe fn set_kernel_trap_stack_raw(stack_top: usize) {
    <Riscv64TaskOps as TaskOps>::set_kernel_trap_stack(stack_top);
}

static ARCH_TRAP_OPS: ArchTrapOps = ArchTrapOps {
    set_kernel_trap_stack: set_kernel_trap_stack_raw,
};

static REGISTERED: AtomicBool = AtomicBool::new(false);

pub fn register() {
    if REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        sched::arch_hooks::register(&ARCH_CONTEXT_OPS);
        sched::arch_hooks::register_time(&ARCH_TIME_OPS);
        sched::arch_hooks::register_trap(&ARCH_TRAP_OPS);
        crate::riscv64::mm::register();
        crate::riscv64::syscall::register();
    }
}
