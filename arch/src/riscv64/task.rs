//! RISC-V64 任务（trap frame）操作与上下文恢复。
//!
//! 本模块实现 [`general::TaskOps`] trait，提供：
//!
//! - trap frame 字段的读写（pc / sp / status / args）；
//! - `sscratch` 管理（用户态 trap 时恢复内核栈）；
//! - [`__riscv64_resume_to_trap_frame`]：设 sepc/sstatus/satp，恢复通用寄存器后 sret；
//! - 内核任务 / 用户任务 / idle 任务入口桩。

use general::{TaskOps, TrapFramePtr};

use crate::riscv64::specific::{
    CSR_FCSR, CSR_SEPC, CSR_SSCRATCH, CSR_SSTATUS, EXC_ECALL_S, EXC_ECALL_U, FRAME_SIZE,
    SSTATUS_FS_DIRTY, SSTATUS_FS_MASK, SSTATUS_SIE, SSTATUS_SPIE, SSTATUS_SPP, TrapFrame,
};
use core::arch::naked_asm;

// ── TaskOps impl ────────────────────────────────────────────────────────────────

pub struct Riscv64TaskOps;

impl TaskOps for Riscv64TaskOps {
    fn trap_frame_pc(trap_frame_ptr: TrapFramePtr) -> usize {
        let tf = unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) };
        tf.sepc
    }

    fn trap_frame_sp(trap_frame_ptr: TrapFramePtr) -> usize {
        let tf = unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) };
        tf.sp
    }

    fn trap_frame_status(trap_frame_ptr: TrapFramePtr) -> usize {
        let tf = unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) };
        tf.status
    }

    fn set_trap_frame_sp(trap_frame_ptr: TrapFramePtr, sp: usize) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        tf.sp = sp;
    }

    fn set_trap_frame_gp(trap_frame_ptr: TrapFramePtr, gp: usize) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        tf.gp = gp;
    }

    fn set_trap_frame_tp(trap_frame_ptr: TrapFramePtr, tp: usize) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        tf.tp = tp;
    }

    fn trap_frame_size() -> usize {
        FRAME_SIZE
    }

    fn trap_frame_align() -> usize {
        core::mem::align_of::<TrapFrame>()
    }

    fn set_kernel_trap_stack(_stack_top: usize) {
        // RISC-V trap entry 依赖 sscratch=0 表示当前在 S-mode；
        // 返回 U-mode 时 resume_to_trap_frame 会写入用户 trap 栈顶。
        write_csr!(sscratch, 0);
    }

    unsafe fn resume_to_trap_frame(trap_frame_ptr: TrapFramePtr) -> ! {
        unsafe { __riscv64_resume_to_trap_frame(trap_frame_ptr.as_usize()) };
        unsafe { core::hint::unreachable_unchecked() }
    }

    fn init_kernel_trap_frame(trap_frame_ptr: TrapFramePtr, entry_pc: usize, kernel_sp: usize) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        *tf = TrapFrame::default();
        tf.sepc = entry_pc;
        tf.sp = kernel_sp;
        // SPP=1（sret 回到 S-mode）+ SPIE=1（sret 后开中断）
        tf.status = SSTATUS_SPP | SSTATUS_SPIE;
        tf.satp = 0;
    }

    fn init_user_trap_frame(
        trap_frame_ptr: TrapFramePtr,
        entry_pc: usize,
        user_sp: usize,
        arg0: usize,
    ) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        *tf = TrapFrame::default();
        tf.sepc = entry_pc;
        tf.sp = user_sp;
        tf.a0 = arg0;
        // HACK: OpenSBI/QEMU 当前没有把 illegal instruction 委托给 S-mode；
        // 用户态浮点指令不能依赖 illegal trap 做 lazy enable。
        tf.status = SSTATUS_SPIE | SSTATUS_SIE | SSTATUS_FS_DIRTY;
        // 初始化时使用当前内核页表。exec 系统调用时会被替换为目标进程的用户页表。
        let satp_val: usize = read_csr!(satp);
        tf.satp = satp_val;
    }

    fn set_user_trap_frame_args(
        trap_frame_ptr: TrapFramePtr,
        arg0: usize,
        arg1: usize,
        arg2: usize,
    ) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        tf.a0 = arg0;
        tf.a1 = arg1;
        tf.a2 = arg2;
    }

    fn signal_interrupted_syscall_pc(trap_frame_ptr: TrapFramePtr) -> Option<usize> {
        let tf = unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) };
        if tf.cause != EXC_ECALL_U && tf.cause != EXC_ECALL_S {
            return None;
        }
        let pc = tf.sepc.checked_sub(4)?;

        // RISC-V C 扩展允许 32 位指令只按 2 字节对齐；确认回退位置确实是 ecall，
        // 避免把 ucontext PC 暴露到上一条 32 位指令的后半。
        let mut insn = [0u8; 4];
        general::mm::copy_from_user(pc, &mut insn).ok()?;
        (u32::from_le_bytes(insn) == 0x0000_0073).then_some(pc)
    }

    fn init_user_entry() -> unsafe extern "C" fn() -> ! {
        __riscv64_user_entry
    }

    fn demo_user_entry() -> unsafe extern "C" fn() -> ! {
        __riscv64_demo_user_entry
    }

    fn idle_task_entry() -> unsafe extern "C" fn() -> ! {
        __riscv64_idle_task
    }

    fn sync_icache() {
        unsafe {
            core::arch::asm!("fence.i", options(nostack, preserves_flags));
        }
    }
}

impl Default for TrapFrame {
    fn default() -> Self {
        Self {
            ra: 0,
            tp: 0,
            sp: 0,
            gp: 0,
            t0: 0,
            t1: 0,
            t2: 0,
            s0: 0,
            s1: 0,
            a0: 0,
            a1: 0,
            a2: 0,
            a3: 0,
            a4: 0,
            a5: 0,
            a6: 0,
            a7: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            s5: 0,
            s6: 0,
            s7: 0,
            s8: 0,
            s9: 0,
            s10: 0,
            s11: 0,
            t3: 0,
            t4: 0,
            t5: 0,
            t6: 0,
            sepc: 0,
            status: 0,
            cause: 0,
            tval: 0,
            satp: 0,
            kstack_top: 0,
            f: [0; 32],
            fcsr: 0,
            _pad: 0,
        }
    }
}

/// 统一的 trap frame 恢复入口。
///
/// 所有"恢复到 trap frame 描述的上下文"的路径（异常返回、fork return、exec、
/// 上下文切换恢复）均跳到此处。
///
/// # 约定
///
/// - `a0` = 指向有效 `TrapFrame` 的指针
/// - 函数不会返回（以 sret 结束）
///
/// # 流程
///
/// 1. 写 sepc / sstatus（清 SIE 防恢复期间中断）
/// 2. 判断 SPP：若返回用户态，切换 satp 并设置 sscratch
/// 3. 条件恢复 FPU（检查 FS 字段）
/// 4. 恢复通用寄存器
/// 5. sret
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __riscv64_resume_to_trap_frame(_tf_ptr: usize) {
    naked_asm!(
        "mv s11, a0",

        // 写 sepc
        "ld t0, {sepc_off}(s11)",
        "csrw {sepc}, t0",

        // 写 sstatus（清 SIE bit 防止恢复期间被中断）
        "ld t0, {status_off}(s11)",
        "andi t0, t0, -3",
        "csrw {sstatus}, t0",

        // 判断 SPP：返回 user 还是 kernel
        "andi t1, t0, {spp}",
        "bnez t1, 1f",

        // return-to-user：目标 satp 已经生效时跳过写 CSR 和全局 sfence.vma。
        // 同一进程的 pthread/futex 热路径会频繁在相同地址空间内往返，重复刷新
        // TLB 会把短线程 benchmark 放大成页表切换开销。
        "ld t2, {satp_off}(s11)",
        "csrr t3, {satp}",
        "beq t2, t3, 4f",
        "csrw {satp}, t2",
        "sfence.vma",
        "4:",
        "ld t0, {kstack_top_off}(s11)",
        "beqz t0, 2f",
        "csrw {sscratch}, t0",
        "j 2f",

        // return-to-kernel：确保 sscratch=0（from_kernel 路径依赖此约定）
        "1:",
        "csrw {sscratch}, x0",

        // Vector 恢复（只在用户态 V active 时真正执行）
        "2:",
        "mv a0, s11",
        "call {restore_vector}",

        // FPU 恢复（检查 sstatus.FS != Off）
        "ld t0, {status_off}(s11)",
        "li t1, {fs_mask}",
        "and t1, t0, t1",
        "beqz t1, 3f",

        // FPU 寄存器恢复
        ".option arch, +d",
        "ld t0, {fcsr_off}(s11)",
        "csrw {fcsr}, t0",
        "fld f0, {f_off}(s11)",     "fld f1, {f_off}+8(s11)",
        "fld f2, {f_off}+16(s11)",   "fld f3, {f_off}+24(s11)",
        "fld f4, {f_off}+32(s11)",   "fld f5, {f_off}+40(s11)",
        "fld f6, {f_off}+48(s11)",   "fld f7, {f_off}+56(s11)",
        "fld f8, {f_off}+64(s11)",   "fld f9, {f_off}+72(s11)",
        "fld f10, {f_off}+80(s11)",  "fld f11, {f_off}+88(s11)",
        "fld f12, {f_off}+96(s11)",  "fld f13, {f_off}+104(s11)",
        "fld f14, {f_off}+112(s11)", "fld f15, {f_off}+120(s11)",
        "fld f16, {f_off}+128(s11)", "fld f17, {f_off}+136(s11)",
        "fld f18, {f_off}+144(s11)", "fld f19, {f_off}+152(s11)",
        "fld f20, {f_off}+160(s11)", "fld f21, {f_off}+168(s11)",
        "fld f22, {f_off}+176(s11)", "fld f23, {f_off}+184(s11)",
        "fld f24, {f_off}+192(s11)", "fld f25, {f_off}+200(s11)",
        "fld f26, {f_off}+208(s11)", "fld f27, {f_off}+216(s11)",
        "fld f28, {f_off}+224(s11)", "fld f29, {f_off}+232(s11)",
        "fld f30, {f_off}+240(s11)", "fld f31, {f_off}+248(s11)",

        // 通用寄存器恢复
        "3:",
        "ld ra, {ra_off}(s11)",
        "ld tp, {tp_off}(s11)",
        "ld gp, {gp_off}(s11)",
        "ld t0, {t0_off}(s11)",
        "ld t1, {t1_off}(s11)",
        "ld t2, {t2_off}(s11)",
        "ld s0, {s0_off}(s11)",
        "ld s1, {s1_off}(s11)",
        "ld a0, {a0_off}(s11)",
        "ld a1, {a1_off}(s11)",
        "ld a2, {a2_off}(s11)",
        "ld a3, {a3_off}(s11)",
        "ld a4, {a4_off}(s11)",
        "ld a5, {a5_off}(s11)",
        "ld a6, {a6_off}(s11)",
        "ld a7, {a7_off}(s11)",
        "ld s2, {s2_off}(s11)",
        "ld s3, {s3_off}(s11)",
        "ld s4, {s4_off}(s11)",
        "ld s5, {s5_off}(s11)",
        "ld s6, {s6_off}(s11)",
        "ld s7, {s7_off}(s11)",
        "ld s8, {s8_off}(s11)",
        "ld s9, {s9_off}(s11)",
        "ld s10, {s10_off}(s11)",
        "ld t3, {t3_off}(s11)",
        "ld t4, {t4_off}(s11)",
        "ld t5, {t5_off}(s11)",
        "ld t6, {t6_off}(s11)",
        "ld sp, {sp_off}(s11)",
        "ld s11, {s11_off}(s11)",

        "sret",

        ra_off = const crate::riscv64::specific::RA_OFFSET,
        tp_off = const crate::riscv64::specific::TP_OFFSET,
        sp_off = const crate::riscv64::specific::SP_OFFSET,
        gp_off = const crate::riscv64::specific::GP_OFFSET,
        t0_off = const crate::riscv64::specific::T0_OFFSET,
        t1_off = const crate::riscv64::specific::T1_OFFSET,
        t2_off = const crate::riscv64::specific::T2_OFFSET,
        s0_off = const crate::riscv64::specific::S0_OFFSET,
        s1_off = const crate::riscv64::specific::S1_OFFSET,
        a0_off = const crate::riscv64::specific::A0_OFFSET,
        a1_off = const crate::riscv64::specific::A1_OFFSET,
        a2_off = const crate::riscv64::specific::A2_OFFSET,
        a3_off = const crate::riscv64::specific::A3_OFFSET,
        a4_off = const crate::riscv64::specific::A4_OFFSET,
        a5_off = const crate::riscv64::specific::A5_OFFSET,
        a6_off = const crate::riscv64::specific::A6_OFFSET,
        a7_off = const crate::riscv64::specific::A7_OFFSET,
        s2_off = const crate::riscv64::specific::S2_OFFSET,
        s3_off = const crate::riscv64::specific::S3_OFFSET,
        s4_off = const crate::riscv64::specific::S4_OFFSET,
        s5_off = const crate::riscv64::specific::S5_OFFSET,
        s6_off = const crate::riscv64::specific::S6_OFFSET,
        s7_off = const crate::riscv64::specific::S7_OFFSET,
        s8_off = const crate::riscv64::specific::S8_OFFSET,
        s9_off = const crate::riscv64::specific::S9_OFFSET,
        s10_off = const crate::riscv64::specific::S10_OFFSET,
        s11_off = const crate::riscv64::specific::S11_OFFSET,
        t3_off = const crate::riscv64::specific::T3_OFFSET,
        t4_off = const crate::riscv64::specific::T4_OFFSET,
        t5_off = const crate::riscv64::specific::T5_OFFSET,
        t6_off = const crate::riscv64::specific::T6_OFFSET,
        sepc_off = const crate::riscv64::specific::SEPC_OFFSET,
        status_off = const crate::riscv64::specific::STATUS_OFFSET,
        kstack_top_off = const crate::riscv64::specific::KSTACK_TOP_OFFSET,
        fcsr_off = const crate::riscv64::specific::FCSR_OFFSET,
        f_off = const crate::riscv64::specific::F_OFFSET,
        sepc = const CSR_SEPC,
        sstatus = const CSR_SSTATUS,
        sscratch = const CSR_SSCRATCH,
        satp = const crate::riscv64::specific::CSR_SATP,
        fcsr = const CSR_FCSR,
        restore_vector = sym crate::riscv64::vector::restore_vector_from_resume,
        spp = const SSTATUS_SPP,
        fs_mask = const SSTATUS_FS_MASK,
        satp_off = const crate::riscv64::specific::SATP_OFFSET,
    );
}

// ── 占位入口桩 ────────────────────────────────────────────────────────────────
//
// 以下三个函数是占位实现（均为 wfi 死循环）。真正的入口由调度器通过
// 陷阱帧的 sepc 指定，这里仅满足 trait 要求的函数指针返回。

unsafe extern "C" fn __riscv64_user_entry() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) }
    }
}

unsafe extern "C" fn __riscv64_demo_user_entry() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) }
    }
}

unsafe extern "C" fn __riscv64_idle_task() -> ! {
    // B9: 低功耗 idle 循环。
    // SIE=0 时 wfi 仍能被中断唤醒（只是不 trap），唤醒后开 SIE 让 trap 立即发生。
    // 避免在 idle 路径上产生多余的 trap → 中断返回 → 再 wfi 的开销。
    loop {
        unsafe {
            core::arch::asm!(
                "csrc sstatus, {sie_mask}",  // 关中断
                "wfi",                        // 休眠等待中断唤醒
                "csrs sstatus, {sie_mask}",  // 开中断，pending 中断立即 trap
                sie_mask = const SSTATUS_SIE,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
}
