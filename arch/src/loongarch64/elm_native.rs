//! LoongArch64 ELM 原生调用门。
//!
//! 调用门保存内核 ABI 边界并登记固定恢复 PC/SP。原生 ELM 在任意嵌套深度触发同步
//! 异常后，trap frame 会回到本文件的统一退出路径，再从边界帧恢复内核寄存器。

use core::arch::naked_asm;

use crate::loongarch64::trap::LoongArch64InterruptOps;

const RA_OFFSET: usize = 0;
const TP_OFFSET: usize = 8;
const RX_OFFSET: usize = 16;
const R22_OFFSET: usize = 24;
const R23_OFFSET: usize = 32;
const R24_OFFSET: usize = 40;
const R25_OFFSET: usize = 48;
const R26_OFFSET: usize = 56;
const R27_OFFSET: usize = 64;
const R28_OFFSET: usize = 72;
const R29_OFFSET: usize = 80;
const R30_OFFSET: usize = 88;
const R31_OFFSET: usize = 96;
const ENTRY_OFFSET: usize = 104;
const CONTEXT_OFFSET: usize = 112;
const RESULT_OFFSET: usize = 120;
const STACK_TOP_OFFSET: usize = 128;
const FRAME_SIZE: usize = 144;
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
    // 原生 ELM 执行期间开放本地中断，使 timer trap 能落实超时和取消意图；退出时只
    // 恢复调用者原来的 IE 位，不覆盖 PLV、DA、PG 等其它 CRMD 状态。
    let interrupt_state = unsafe { LoongArch64InterruptOps::save_interrupt_state() };
    unsafe { LoongArch64InterruptOps::enable_interrupts() };
    // 安全性：调用约束由本函数的调用方保证，汇编门负责保存和恢复内核 ABI 边界。
    let result = unsafe { __loongarch64_elm_native_call(entry, context, stack_top) };
    unsafe { LoongArch64InterruptOps::restore_interrupt_state(interrupt_state) };
    result
}

/// 返回 trap frame 必须重定向到的固定恢复出口。
pub fn elm_native_recovery_address() -> usize {
    __loongarch64_elm_native_return as *const () as usize
}

/// 从 panic handler 丢弃原生 ELM 栈并跳回已登记的内核边界。
///
/// # Safety
///
/// 三个参数必须来自当前 ELM guard 消费出的恢复记录。
pub unsafe fn resume_elm_panic(return_pc: usize, return_sp: usize, return_value: usize) -> ! {
    // 安全性：恢复记录只由架构调用门登记，并在消费时进行对齐和非零校验。
    unsafe { __loongarch64_resume_elm_panic(return_pc, return_sp, return_value) }
}

#[unsafe(naked)]
unsafe extern "C" fn __loongarch64_resume_elm_panic(
    _return_pc: usize,
    _return_sp: usize,
    _return_value: usize,
) -> ! {
    naked_asm!(
        "or $t0, $a0, $zero",
        "or $sp, $a1, $zero",
        "or $a0, $a2, $zero",
        "jirl $zero, $t0, 0",
    )
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
unsafe extern "C" fn __loongarch64_elm_native_call(
    _entry: usize,
    _context: *mut u8,
    _stack_top: usize,
) -> i32 {
    naked_asm!(
        "addi.d $sp, $sp, -{frame_size}",
        "st.d $r1, $sp, {ra_offset}",
        "st.d $r2, $sp, {tp_offset}",
        "st.d $r21, $sp, {rx_offset}",
        "st.d $r22, $sp, {r22_offset}",
        "st.d $r23, $sp, {r23_offset}",
        "st.d $r24, $sp, {r24_offset}",
        "st.d $r25, $sp, {r25_offset}",
        "st.d $r26, $sp, {r26_offset}",
        "st.d $r27, $sp, {r27_offset}",
        "st.d $r28, $sp, {r28_offset}",
        "st.d $r29, $sp, {r29_offset}",
        "st.d $r30, $sp, {r30_offset}",
        "st.d $r31, $sp, {r31_offset}",
        "st.d $a0, $sp, {entry_offset}",
        "st.d $a1, $sp, {context_offset}",
        "st.d $a2, $sp, {stack_top_offset}",

        "la.abs $a0, {return_entry}",
        "or $a1, $sp, $zero",
        "la.abs $t0, {arm_recovery}",
        "jirl $ra, $t0, 0",
        "beqz $a0, 2f",

        "ld.d $t0, $sp, {entry_offset}",
        "ld.d $a0, $sp, {context_offset}",
        "ld.d $t1, $sp, {stack_top_offset}",
        "addi.d $t1, $t1, -{native_return_frame_size}",
        "st.d $sp, $t1, {native_kernel_sp_offset}",
        "or $sp, $t1, $zero",
        "jirl $ra, $t0, 0",
        "ld.d $t1, $sp, {native_kernel_sp_offset}",
        "or $sp, $t1, $zero",
        "la.abs $t0, {return_entry}",
        "jirl $zero, $t0, 0",

        "2:",
        "li.d $a0, {fault_status}",
        "la.abs $t0, {return_entry}",
        "jirl $zero, $t0, 0",

        frame_size = const FRAME_SIZE,
        ra_offset = const RA_OFFSET,
        tp_offset = const TP_OFFSET,
        rx_offset = const RX_OFFSET,
        r22_offset = const R22_OFFSET,
        r23_offset = const R23_OFFSET,
        r24_offset = const R24_OFFSET,
        r25_offset = const R25_OFFSET,
        r26_offset = const R26_OFFSET,
        r27_offset = const R27_OFFSET,
        r28_offset = const R28_OFFSET,
        r29_offset = const R29_OFFSET,
        r30_offset = const R30_OFFSET,
        r31_offset = const R31_OFFSET,
        entry_offset = const ENTRY_OFFSET,
        context_offset = const CONTEXT_OFFSET,
        stack_top_offset = const STACK_TOP_OFFSET,
        native_return_frame_size = const NATIVE_RETURN_FRAME_SIZE,
        native_kernel_sp_offset = const NATIVE_KERNEL_SP_OFFSET,
        fault_status = const NATIVE_FAULT_STATUS,
        arm_recovery = sym arm_elm_native_recovery,
        return_entry = sym __loongarch64_elm_native_return,
    );
}

/// 正常返回和 trap 恢复共用的唯一退出路径。
#[unsafe(naked)]
unsafe extern "C" fn __loongarch64_elm_native_return() -> i32 {
    naked_asm!(
        "st.d $a0, $sp, {result_offset}",

        // 原生代码可能破坏固定寄存器，调用 Rust 清理函数前必须先恢复它们。
        "ld.d $r2, $sp, {tp_offset}",
        "ld.d $r21, $sp, {rx_offset}",
        "la.abs $t0, {disarm_recovery}",
        "jirl $ra, $t0, 0",

        "ld.d $a0, $sp, {result_offset}",
        "ld.d $r1, $sp, {ra_offset}",
        "ld.d $r2, $sp, {tp_offset}",
        "ld.d $r21, $sp, {rx_offset}",
        "ld.d $r22, $sp, {r22_offset}",
        "ld.d $r23, $sp, {r23_offset}",
        "ld.d $r24, $sp, {r24_offset}",
        "ld.d $r25, $sp, {r25_offset}",
        "ld.d $r26, $sp, {r26_offset}",
        "ld.d $r27, $sp, {r27_offset}",
        "ld.d $r28, $sp, {r28_offset}",
        "ld.d $r29, $sp, {r29_offset}",
        "ld.d $r30, $sp, {r30_offset}",
        "ld.d $r31, $sp, {r31_offset}",
        "addi.d $sp, $sp, {frame_size}",
        "jirl $zero, $r1, 0",

        frame_size = const FRAME_SIZE,
        ra_offset = const RA_OFFSET,
        tp_offset = const TP_OFFSET,
        rx_offset = const RX_OFFSET,
        r22_offset = const R22_OFFSET,
        r23_offset = const R23_OFFSET,
        r24_offset = const R24_OFFSET,
        r25_offset = const R25_OFFSET,
        r26_offset = const R26_OFFSET,
        r27_offset = const R27_OFFSET,
        r28_offset = const R28_OFFSET,
        r29_offset = const R29_OFFSET,
        r30_offset = const R30_OFFSET,
        r31_offset = const R31_OFFSET,
        result_offset = const RESULT_OFFSET,
        disarm_recovery = sym disarm_elm_native_recovery,
    );
}
