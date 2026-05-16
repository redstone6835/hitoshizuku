//! LoongArch64 缺页 / 用户访问 fault 解码 → 注入 [`general::mm::FaultDecodeOps`]。
//!
//! 本文件唯一对外符号是 `static FAULT_DECODE_OPS`。general 的 `dispatch_page_fault`
//! 通过它读出"缺页类型 / 触发地址 / 来源权级 / 是否命中 __ex_table"。
//!
//! ## 当前实现
//!
//! - `fault_kind`：按 ESTAT.ECODE 分类（PIL/PIS/PIF/PME/PNR/PNX）。**目前从
//!   CSR 直接读 ESTAT**——TrapFrame 里没缓存它。改从 trap frame 读需要修改
//!   asm 入口；现阶段 CSR 直读足够（handler 还没 enable 中断）。
//! - `fault_addr`：读 CSR_BADV。
//! - `fault_from_user`：读 TrapFrame.status 的 PPLV 字段。
//! - `try_fixup_kernel_access`：在线性 `__ex_table` 中查 fault PC，命中则把
//!   TrapFrame.pc 改写到 fixup PC。

use core::arch::asm;

use general::TrapFramePtr;
use general::mm::{FaultDecodeOps, FaultKind};

use crate::loongarch64::specific::{
    CSR_BADV, CSR_ESTAT, CSR_PRMD_PPLV_MASK, ECODE_PIF, ECODE_PIL, ECODE_PIS, ECODE_PME, ECODE_PNR,
    ECODE_PNX, TrapFrame,
};

const ESTAT_ECODE_SHIFT: usize = 16;
const ESTAT_ECODE_MASK: usize = 0x3f;

/// # Safety
/// 调用方保证 `tf` 在当前 trap 上下文仍然合法。
unsafe fn trap_frame<'a>(tf: TrapFramePtr) -> &'a TrapFrame {
    // Safety: 调用方保证 tf 来自 arch 入口写入的 TrapFrame 指针，生命周期
    //         在 trap 返回前一直有效。
    unsafe { &*(tf.as_usize() as *const TrapFrame) }
}

/// # Safety
/// 同 [`trap_frame`]，但调用方还必须保证本次 trap 处理独占修改该 frame。
unsafe fn trap_frame_mut<'a>(tf: TrapFramePtr) -> &'a mut TrapFrame {
    unsafe { &mut *(tf.as_usize() as *mut TrapFrame) }
}

#[repr(C)]
struct ExceptionTableEntry {
    fault: usize,
    fixup: usize,
}

unsafe extern "C" {
    static __ex_table_start: u8;
    static __ex_table_end: u8;
}

fn read_estat() -> usize {
    let estat: usize;
    // Safety: 读 CSR 不访问内存，preserves_flags。
    unsafe {
        asm!(
            "csrrd {v}, {csr}",
            v = out(reg) estat,
            csr = const CSR_ESTAT,
            options(nostack, preserves_flags)
        );
    }
    estat
}

fn read_badv() -> usize {
    let badv: usize;
    // Safety: 同 read_estat。
    unsafe {
        asm!(
            "csrrd {v}, {csr}",
            v = out(reg) badv,
            csr = const CSR_BADV,
            options(nostack, preserves_flags)
        );
    }
    badv
}

fn fault_kind(_tf: TrapFramePtr) -> FaultKind {
    let estat = read_estat();
    let ecode = (estat >> ESTAT_ECODE_SHIFT) & ESTAT_ECODE_MASK;
    match ecode {
        ECODE_PIL => FaultKind::Load,
        ECODE_PIS => FaultKind::Store,
        ECODE_PIF => FaultKind::Exec,
        ECODE_PME => FaultKind::PermWrite,
        ECODE_PNR => FaultKind::PermRead,
        ECODE_PNX => FaultKind::PermExec,
        // 非缺页族 → 归 Load，由 VmSpace::handle_fault 据地址走 Segv 分支。
        _ => FaultKind::Load,
    }
}

fn fault_addr(_tf: TrapFramePtr) -> usize {
    read_badv()
}

fn fault_from_user(tf: TrapFramePtr) -> bool {
    // Safety: 调用方约束。
    let frame = unsafe { trap_frame(tf) };
    (frame.status & CSR_PRMD_PPLV_MASK) != 0
}

fn try_fixup_kernel_access(tf: TrapFramePtr) -> bool {
    let frame = unsafe { trap_frame(tf) };
    let pc = frame.pc;
    let start = unsafe { &__ex_table_start as *const u8 as usize };
    let end = unsafe { &__ex_table_end as *const u8 as usize };
    let entry_size = core::mem::size_of::<ExceptionTableEntry>();
    if end < start || (end - start) % entry_size != 0 {
        return false;
    }
    let mut cur = start as *const ExceptionTableEntry;
    let last = end as *const ExceptionTableEntry;
    while cur < last {
        let entry = unsafe { core::ptr::read_unaligned(cur) };
        if entry.fault == pc {
            unsafe { trap_frame_mut(tf).pc = entry.fixup };
            return true;
        }
        cur = unsafe { cur.add(1) };
    }
    false
}

pub(super) static FAULT_DECODE_OPS: FaultDecodeOps = FaultDecodeOps {
    fault_kind,
    fault_addr,
    fault_from_user,
    try_fixup_kernel_access,
};
