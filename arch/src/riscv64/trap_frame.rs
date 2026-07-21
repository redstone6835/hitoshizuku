//! TrapFrame 结构体与偏移常量。
//!
//! 字段按 trap entry 汇编的 sd 顺序排列，不可重排（实测重排导致 60% 性能退化，
//! 原因是 CPU store buffer 对顺序写入有优化）。
//
// Vector 状态由 `vector.rs` 中的任务扩展对象独立维护。TrapFrame 只保留 VS
// 状态位，不能在这里重复嵌入可变大小的 v0-v31 保存区。

use core::mem::offset_of;

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapFrame {
    pub ra: usize,  // x1
    pub tp: usize,  // x4  (trap entry 先存 tp，再存 sp)
    pub sp: usize,  // x2
    pub gp: usize,  // x3
    pub t0: usize,  // x5
    pub t1: usize,  // x6
    pub t2: usize,  // x7
    pub s0: usize,  // x8 (fp)
    pub s1: usize,  // x9
    pub a0: usize,  // x10
    pub a1: usize,  // x11
    pub a2: usize,  // x12
    pub a3: usize,  // x13
    pub a4: usize,  // x14
    pub a5: usize,  // x15
    pub a6: usize,  // x16
    pub a7: usize,  // x17 (syscall nr)
    pub s2: usize,  // x18
    pub s3: usize,  // x19
    pub s4: usize,  // x20
    pub s5: usize,  // x21
    pub s6: usize,  // x22
    pub s7: usize,  // x23
    pub s8: usize,  // x24
    pub s9: usize,  // x25
    pub s10: usize, // x26
    pub s11: usize, // x27
    pub t3: usize,  // x28
    pub t4: usize,  // x29
    pub t5: usize,  // x30
    pub t6: usize,  // x31
    pub sepc: usize,
    pub status: usize,
    pub cause: usize,
    pub tval: usize,
    pub satp: usize,
    /// 可信内核栈顶；非零同时作为 resume 返回 U-mode 的类型标记。
    pub kstack_top: usize,
    pub f: [u64; 32],
    pub fcsr: u32,
    pub _pad: u32,
}

impl TrapFrame {
    #[inline]
    pub fn syscall_id(&self) -> usize {
        self.a7
    }
    #[inline]
    pub fn syscall_args(&self) -> [usize; 6] {
        [self.a0, self.a1, self.a2, self.a3, self.a4, self.a5]
    }
    #[inline]
    pub fn set_syscall_return(&mut self, v: usize) {
        self.a0 = v;
    }
    #[inline]
    pub fn skip_syscall_insn(&mut self) {
        self.sepc = self.sepc.wrapping_add(4);
    }
}

// ── 偏移常量 ──────────────────────────────────────────────────────────────────

pub const FRAME_SIZE: usize = (core::mem::size_of::<TrapFrame>() + 15) & !15;

pub const RA_OFFSET: usize = offset_of!(TrapFrame, ra);
pub const TP_OFFSET: usize = offset_of!(TrapFrame, tp);
pub const SP_OFFSET: usize = offset_of!(TrapFrame, sp);
pub const GP_OFFSET: usize = offset_of!(TrapFrame, gp);
pub const T0_OFFSET: usize = offset_of!(TrapFrame, t0);
pub const T1_OFFSET: usize = offset_of!(TrapFrame, t1);
pub const T2_OFFSET: usize = offset_of!(TrapFrame, t2);
pub const S0_OFFSET: usize = offset_of!(TrapFrame, s0);
pub const S1_OFFSET: usize = offset_of!(TrapFrame, s1);
pub const A0_OFFSET: usize = offset_of!(TrapFrame, a0);
pub const A1_OFFSET: usize = offset_of!(TrapFrame, a1);
pub const A2_OFFSET: usize = offset_of!(TrapFrame, a2);
pub const A3_OFFSET: usize = offset_of!(TrapFrame, a3);
pub const A4_OFFSET: usize = offset_of!(TrapFrame, a4);
pub const A5_OFFSET: usize = offset_of!(TrapFrame, a5);
pub const A6_OFFSET: usize = offset_of!(TrapFrame, a6);
pub const A7_OFFSET: usize = offset_of!(TrapFrame, a7);
pub const S2_OFFSET: usize = offset_of!(TrapFrame, s2);
pub const S3_OFFSET: usize = offset_of!(TrapFrame, s3);
pub const S4_OFFSET: usize = offset_of!(TrapFrame, s4);
pub const S5_OFFSET: usize = offset_of!(TrapFrame, s5);
pub const S6_OFFSET: usize = offset_of!(TrapFrame, s6);
pub const S7_OFFSET: usize = offset_of!(TrapFrame, s7);
pub const S8_OFFSET: usize = offset_of!(TrapFrame, s8);
pub const S9_OFFSET: usize = offset_of!(TrapFrame, s9);
pub const S10_OFFSET: usize = offset_of!(TrapFrame, s10);
pub const S11_OFFSET: usize = offset_of!(TrapFrame, s11);
pub const T3_OFFSET: usize = offset_of!(TrapFrame, t3);
pub const T4_OFFSET: usize = offset_of!(TrapFrame, t4);
pub const T5_OFFSET: usize = offset_of!(TrapFrame, t5);
pub const T6_OFFSET: usize = offset_of!(TrapFrame, t6);
pub const SEPC_OFFSET: usize = offset_of!(TrapFrame, sepc);
pub const STATUS_OFFSET: usize = offset_of!(TrapFrame, status);
pub const CAUSE_OFFSET: usize = offset_of!(TrapFrame, cause);
pub const TVAL_OFFSET: usize = offset_of!(TrapFrame, tval);
pub const SATP_OFFSET: usize = offset_of!(TrapFrame, satp);
pub const KSTACK_TOP_OFFSET: usize = offset_of!(TrapFrame, kstack_top);
pub const F_OFFSET: usize = offset_of!(TrapFrame, f);
pub const FCSR_OFFSET: usize = offset_of!(TrapFrame, fcsr);

const _: () = {
    let raw_size = core::mem::size_of::<TrapFrame>();
    let word = core::mem::size_of::<usize>();
    let word_offsets = [
        RA_OFFSET,
        TP_OFFSET,
        SP_OFFSET,
        GP_OFFSET,
        T0_OFFSET,
        T1_OFFSET,
        T2_OFFSET,
        S0_OFFSET,
        S1_OFFSET,
        A0_OFFSET,
        A1_OFFSET,
        A2_OFFSET,
        A3_OFFSET,
        A4_OFFSET,
        A5_OFFSET,
        A6_OFFSET,
        A7_OFFSET,
        S2_OFFSET,
        S3_OFFSET,
        S4_OFFSET,
        S5_OFFSET,
        S6_OFFSET,
        S7_OFFSET,
        S8_OFFSET,
        S9_OFFSET,
        S10_OFFSET,
        S11_OFFSET,
        T3_OFFSET,
        T4_OFFSET,
        T5_OFFSET,
        T6_OFFSET,
        SEPC_OFFSET,
        STATUS_OFFSET,
        CAUSE_OFFSET,
        TVAL_OFFSET,
        SATP_OFFSET,
        KSTACK_TOP_OFFSET,
    ];
    assert!(FRAME_SIZE % 16 == 0);
    assert!(core::mem::align_of::<TrapFrame>() == 16);
    assert!(raw_size <= FRAME_SIZE);
    assert!(FRAME_SIZE - raw_size < 16);
    let mut index = 0usize;
    while index < word_offsets.len() {
        assert!(word_offsets[index] == index * word);
        index += 1;
    }
    assert!(F_OFFSET == word_offsets.len() * word);
    assert!(F_OFFSET % core::mem::align_of::<u64>() == 0);
    assert!(FCSR_OFFSET == F_OFFSET + 32 * core::mem::size_of::<u64>());
    assert!(raw_size == FCSR_OFFSET + 2 * core::mem::size_of::<u32>());
};
