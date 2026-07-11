//! RISC-V64 ELM 原生调用门。
//!
//! 调用门在进入原生 ELM 前保存内核 ABI 边界，并把固定恢复出口与边界栈指针登记到
//! ELM guard。同步异常只能返回该出口，再由出口恢复内核寄存器，不能依赖故障现场
//! 的 `ra` 或 ELM 内部栈深度。

use core::arch::naked_asm;

use crate::riscv64::trap::Riscv64InterruptOps;

const RA_OFFSET: usize = 0;
const GP_OFFSET: usize = 8;
const TP_OFFSET: usize = 16;
const S0_OFFSET: usize = 24;
const S1_OFFSET: usize = 32;
const S2_OFFSET: usize = 40;
const S3_OFFSET: usize = 48;
const S4_OFFSET: usize = 56;
const S5_OFFSET: usize = 64;
const S6_OFFSET: usize = 72;
const S7_OFFSET: usize = 80;
const S8_OFFSET: usize = 88;
const S9_OFFSET: usize = 96;
const S10_OFFSET: usize = 104;
const S11_OFFSET: usize = 112;
const ENTRY_OFFSET: usize = 120;
const CONTEXT_OFFSET: usize = 128;
const RESULT_OFFSET: usize = 136;
const STACK_TOP_OFFSET: usize = 144;
const FRAME_SIZE: usize = 160;
const NATIVE_RETURN_FRAME_SIZE: usize = 32;
const NATIVE_KERNEL_SP_OFFSET: usize = 24;
const NATIVE_FAULT_STATUS: i32 = -4098;

/// 通过受保护调用门执行一个“单指针参数、返回 i32”的原生 ELM 入口。
///
/// # Safety
///
/// `entry` 必须指向符合 ELM v1 原生调用约定的可执行代码，`context` 必须在整个
/// 调用期间保持有效。调用方必须已经进入一个 ELM guard。
pub unsafe fn call_elm_native(entry: usize, context: *mut u8, stack_top: usize) -> i32 {
    // 原生 ELM 可能从关闭中断的 syscall/trap 上下文进入。调用期间必须开放定时器，
    // 否则超时和跨 CPU 取消无法把失控代码重定向到恢复出口。
    let interrupt_state = unsafe { Riscv64InterruptOps::save_interrupt_state() };
    unsafe { Riscv64InterruptOps::enable_interrupts() };
    // 安全性：调用约束由本函数的调用方保证，汇编门负责保存和恢复内核 ABI 边界。
    let result = unsafe { __riscv64_elm_native_call(entry, context, stack_top) };
    unsafe { Riscv64InterruptOps::restore_interrupt_state(interrupt_state) };
    result
}

/// 返回 trap frame 必须重定向到的固定恢复出口。
pub fn elm_native_recovery_address() -> usize {
    __riscv64_elm_native_return as *const () as usize
}

/// 从 panic handler 丢弃原生 ELM 栈并跳回已登记的内核边界。
///
/// # Safety
///
/// 三个参数必须来自当前 ELM guard 消费出的恢复记录。
pub unsafe fn resume_elm_panic(return_pc: usize, return_sp: usize, return_value: usize) -> ! {
    // 安全性：恢复记录只由架构调用门登记，并在消费时进行对齐和非零校验。
    unsafe { __riscv64_resume_elm_panic(return_pc, return_sp, return_value) }
}

#[unsafe(naked)]
unsafe extern "C" fn __riscv64_resume_elm_panic(
    _return_pc: usize,
    _return_sp: usize,
    _return_value: usize,
) -> ! {
    naked_asm!("mv t0, a0", "mv sp, a1", "mv a0, a2", "jr t0",)
}

extern "C" fn arm_elm_native_recovery(return_pc: usize, return_sp: usize) -> usize {
    usize::from(general::elm_guard::arm_current_recovery(
        return_pc, return_sp,
    ))
}

extern "C" fn disarm_elm_native_recovery() {
    let _ = general::elm_guard::disarm_current_recovery();
}

#[unsafe(naked)]
unsafe extern "C" fn __riscv64_elm_native_call(
    _entry: usize,
    _context: *mut u8,
    _stack_top: usize,
) -> i32 {
    naked_asm!(
        "addi sp, sp, -{frame_size}",
        "sd ra, {ra_offset}(sp)",
        "sd gp, {gp_offset}(sp)",
        "sd tp, {tp_offset}(sp)",
        "sd s0, {s0_offset}(sp)",
        "sd s1, {s1_offset}(sp)",
        "sd s2, {s2_offset}(sp)",
        "sd s3, {s3_offset}(sp)",
        "sd s4, {s4_offset}(sp)",
        "sd s5, {s5_offset}(sp)",
        "sd s6, {s6_offset}(sp)",
        "sd s7, {s7_offset}(sp)",
        "sd s8, {s8_offset}(sp)",
        "sd s9, {s9_offset}(sp)",
        "sd s10, {s10_offset}(sp)",
        "sd s11, {s11_offset}(sp)",
        "sd a0, {entry_offset}(sp)",
        "sd a1, {context_offset}(sp)",
        "sd a2, {stack_top_offset}(sp)",

        "la a0, {return_entry}",
        "mv a1, sp",
        "call {arm_recovery}",
        "beqz a0, 2f",

        "ld t0, {entry_offset}(sp)",
        "ld a0, {context_offset}(sp)",
        "ld t1, {stack_top_offset}(sp)",
        "addi t1, t1, -{native_return_frame_size}",
        "sd sp, {native_kernel_sp_offset}(t1)",
        "mv sp, t1",
        "jalr ra, t0, 0",
        "ld t1, {native_kernel_sp_offset}(sp)",
        "mv sp, t1",
        "tail {return_entry}",

        "2:",
        "li a0, {fault_status}",
        "tail {return_entry}",

        frame_size = const FRAME_SIZE,
        ra_offset = const RA_OFFSET,
        gp_offset = const GP_OFFSET,
        tp_offset = const TP_OFFSET,
        s0_offset = const S0_OFFSET,
        s1_offset = const S1_OFFSET,
        s2_offset = const S2_OFFSET,
        s3_offset = const S3_OFFSET,
        s4_offset = const S4_OFFSET,
        s5_offset = const S5_OFFSET,
        s6_offset = const S6_OFFSET,
        s7_offset = const S7_OFFSET,
        s8_offset = const S8_OFFSET,
        s9_offset = const S9_OFFSET,
        s10_offset = const S10_OFFSET,
        s11_offset = const S11_OFFSET,
        entry_offset = const ENTRY_OFFSET,
        context_offset = const CONTEXT_OFFSET,
        stack_top_offset = const STACK_TOP_OFFSET,
        native_return_frame_size = const NATIVE_RETURN_FRAME_SIZE,
        native_kernel_sp_offset = const NATIVE_KERNEL_SP_OFFSET,
        fault_status = const NATIVE_FAULT_STATUS,
        arm_recovery = sym arm_elm_native_recovery,
        return_entry = sym __riscv64_elm_native_return,
    );
}

/// 正常返回和 trap 恢复共用的唯一退出路径。
#[unsafe(naked)]
unsafe extern "C" fn __riscv64_elm_native_return() -> i32 {
    naked_asm!(
        "sd a0, {result_offset}(sp)",

        // 原生代码可能破坏固定寄存器，调用 Rust 清理函数前必须先恢复它们。
        "ld gp, {gp_offset}(sp)",
        "ld tp, {tp_offset}(sp)",
        "call {disarm_recovery}",

        "ld a0, {result_offset}(sp)",
        "ld ra, {ra_offset}(sp)",
        "ld gp, {gp_offset}(sp)",
        "ld tp, {tp_offset}(sp)",
        "ld s0, {s0_offset}(sp)",
        "ld s1, {s1_offset}(sp)",
        "ld s2, {s2_offset}(sp)",
        "ld s3, {s3_offset}(sp)",
        "ld s4, {s4_offset}(sp)",
        "ld s5, {s5_offset}(sp)",
        "ld s6, {s6_offset}(sp)",
        "ld s7, {s7_offset}(sp)",
        "ld s8, {s8_offset}(sp)",
        "ld s9, {s9_offset}(sp)",
        "ld s10, {s10_offset}(sp)",
        "ld s11, {s11_offset}(sp)",
        "addi sp, sp, {frame_size}",
        "ret",

        frame_size = const FRAME_SIZE,
        ra_offset = const RA_OFFSET,
        gp_offset = const GP_OFFSET,
        tp_offset = const TP_OFFSET,
        s0_offset = const S0_OFFSET,
        s1_offset = const S1_OFFSET,
        s2_offset = const S2_OFFSET,
        s3_offset = const S3_OFFSET,
        s4_offset = const S4_OFFSET,
        s5_offset = const S5_OFFSET,
        s6_offset = const S6_OFFSET,
        s7_offset = const S7_OFFSET,
        s8_offset = const S8_OFFSET,
        s9_offset = const S9_OFFSET,
        s10_offset = const S10_OFFSET,
        s11_offset = const S11_OFFSET,
        result_offset = const RESULT_OFFSET,
        disarm_recovery = sym disarm_elm_native_recovery,
    );
}
