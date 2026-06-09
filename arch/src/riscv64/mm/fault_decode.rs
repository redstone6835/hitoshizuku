//! RISC-V64 缺页 / 用户访问 fault 解码 → 注入 [`general::mm::FaultDecodeOps`]。
//!
//! 本文件唯一对外符号是 `static FAULT_DECODE_OPS`。general 的 `dispatch_page_fault`
//! 通过它读出"缺页类型 / 触发地址 / 来源权级 / 是否命中 __ex_table"。
//!
//! ## `__ex_table` 格式
//!
//! 每条 16 字节：`(fault_pc: usize, fixup_pc: usize)`。
//! 由 `user_copy.rs` 中的 `.pushsection __ex_table` 写入。
//! 当内核态访问用户地址触发缺页时，查表命中则改写 pc 到 fixup 分支。

use core::ptr::addr_of;

use general::TrapFramePtr;
use general::mm::{FaultDecodeOps, FaultKind};

use crate::riscv64::specific::{
    TrapFrame,
    EXC_INST_PAGE_FAULT,
    EXC_LOAD_PAGE_FAULT,
    EXC_STORE_PAGE_FAULT,
    EXC_LOAD_ACCESS,
    EXC_STORE_ACCESS,
    SCAUSE_INTERRUPT,
};

// ── __ex_table 符号 ──────────────────────────────────────────────────────────

#[repr(C)]
struct ExTableEntry {
    fault_pc: usize,
    fixup_pc: usize,
}

unsafe extern "C" {
    static __ex_table_start: u8;
    static __ex_table_end: u8;
}

// ── TrapFrame 访问 ───────────────────────────────────────────────────────────

/// # Safety
/// 调用方保证 `tf` 指向当前 trap 上下文中合法的 TrapFrame。
unsafe fn as_tf<'a>(tf: TrapFramePtr) -> &'a TrapFrame {
    unsafe { &*(tf.as_usize() as *const TrapFrame) }
}

/// # Safety
/// 同 [`as_tf`]，且调用方保证独占写入权。
unsafe fn as_tf_mut<'a>(tf: TrapFramePtr) -> &'a mut TrapFrame {
    unsafe { &mut *(tf.as_usize() as *mut TrapFrame) }
}

// ── FaultDecodeOps 实现 ──────────────────────────────────────────────────────

fn fault_kind(tf: TrapFramePtr) -> FaultKind {
    let frame = unsafe { as_tf(tf) };
    // 防御性掩除中断位（page fault cause 最高位一定为 0，但避免垃圾值传入）
    let cause = frame.cause & !SCAUSE_INTERRUPT;
    match cause {
        EXC_LOAD_PAGE_FAULT | EXC_LOAD_ACCESS => FaultKind::Load,
        EXC_STORE_PAGE_FAULT | EXC_STORE_ACCESS => FaultKind::Store,
        EXC_INST_PAGE_FAULT => FaultKind::Exec,
        other => panic!("fault_decode: unexpected scause {other:#x}"),
    }
}

fn fault_addr(tf: TrapFramePtr) -> usize {
    unsafe { as_tf(tf) }.tval
}

fn fault_from_user(tf: TrapFramePtr) -> bool {
    let status = unsafe { as_tf(tf) }.status;
    // SPP (bit 8): 0 = 来自 U-mode, 1 = 来自 S-mode
    (status & (1 << 8)) == 0
}

/// 在 `__ex_table` 中线性查找 `fault_pc`。
/// 命中则改写 TrapFrame.pc → fixup_pc，返回 true。
fn try_fixup_kernel_access(tf: TrapFramePtr) -> bool {
    let frame = unsafe { as_tf(tf) };
    let pc = frame.sepc;

    let start = addr_of!(__ex_table_start) as usize;
    let end = addr_of!(__ex_table_end) as usize;
    let entry_size = core::mem::size_of::<ExTableEntry>();

    if end <= start || (end - start) % entry_size != 0 {
        return false;
    }

    let count = (end - start) / entry_size;
    let table = start as *const ExTableEntry;

    for i in 0..count {
        let entry = unsafe { core::ptr::read(table.add(i)) };
        if entry.fault_pc == pc {
            unsafe { as_tf_mut(tf).sepc = entry.fixup_pc };
            return true;
        }
    }
    false
}

pub(super) static FAULT_DECODE_OPS: FaultDecodeOps = FaultDecodeOps {
    fault_kind,
    fault_addr,
    fault_from_user,
    try_fixup_kernel_access,
};