#![allow(named_asm_labels)]
use general::{TaskOps, TrapFramePtr};

use crate::loongarch64::*;

pub struct LoongArch64TaskOps;

impl TaskOps for LoongArch64TaskOps {
    fn trap_frame_pc(trap_frame_ptr: TrapFramePtr) -> usize {
        let tf = unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) };
        tf.pc
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

    fn set_trap_frame_gp(_trap_frame_ptr: TrapFramePtr, _gp: usize) {
        // LoongArch64 psABI does not expose a dedicated GP register in the
        // saved trap frame. Do not alias this architecture-neutral hook to TP.
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

    fn set_kernel_trap_stack(stack_top: usize) {
        unsafe {
            core::arch::asm!(
                "csrwr {stack}, {csr_ks0}",
                stack = in(reg) stack_top,
                csr_ks0 = const CSR_KS0,
                options(nostack, preserves_flags)
            );
        }
    }

    unsafe fn resume_to_trap_frame(trap_frame_ptr: TrapFramePtr) -> ! {
        unsafe { __loongarch64_resume_to_trap_frame(trap_frame_ptr.as_usize()) };
        unsafe { core::hint::unreachable_unchecked() }
    }

    fn init_kernel_trap_frame(trap_frame_ptr: TrapFramePtr, entry_pc: usize, kernel_sp: usize) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        *tf = TrapFrame::default();
        tf.pc = entry_pc;
        tf.sp = kernel_sp;
        tf.status = PRMD_KERNEL_IE;
        tf.euen = EUEN_FPE | EUEN_SXE;
    }

    fn init_user_trap_frame(
        trap_frame_ptr: TrapFramePtr,
        entry_pc: usize,
        user_sp: usize,
        arg0: usize,
    ) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        *tf = TrapFrame::default();
        tf.pc = entry_pc;
        tf.sp = user_sp;
        tf.a0 = arg0;
        tf.status = PRMD_USER_IE;
        tf.euen = EUEN_FPE | EUEN_SXE;
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
        tf.pc.checked_sub(4)
    }

    fn init_user_entry() -> unsafe extern "C" fn() -> ! {
        __loongarch64_user_entry
    }

    fn demo_user_entry() -> unsafe extern "C" fn() -> ! {
        __loongarch64_demo_user_entry
    }

    fn idle_task_entry() -> unsafe extern "C" fn() -> ! {
        __loongarch64_idle_task
    }

    fn sync_icache() {
        crate::loongarch64::smp::sync_icache_all_cpus();
    }
}

const PRMD_PPLV_USER: usize = CSR_PRMD_PPLV_PLV3;
const PRMD_PIE_ENABLED: usize = 1 << 2;
const PRMD_USER_IE: usize = PRMD_PPLV_USER | PRMD_PIE_ENABLED;
const PRMD_KERNEL_IE: usize = CSR_PRMD_PPLV_PLV0 | PRMD_PIE_ENABLED;

impl Default for TrapFrame {
    fn default() -> Self {
        Self {
            ra: 0,
            tp: 0,
            sp: 0,
            a0: 0,
            a1: 0,
            a2: 0,
            a3: 0,
            a4: 0,
            a5: 0,
            a6: 0,
            a7: 0,
            t0: 0,
            t1: 0,
            t2: 0,
            t3: 0,
            t4: 0,
            t5: 0,
            t6: 0,
            t7: 0,
            t8: 0,
            rx: 0,
            s0: 0,
            s1: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            s5: 0,
            s6: 0,
            s7: 0,
            s8: 0,
            s9: 0,
            pc: 0,
            status: 0,
            euen: 0,
            llbctl: 0,
            _lsx_padding: 0,
            lsx: [[0; 2]; 32],
            f: [0; 32],
            fcsr: 0,
            fcc: 0,
        }
    }
}

#[unsafe(naked)]
unsafe extern "C" fn __loongarch64_resume_to_trap_frame(_tf_ptr: usize) {
    use core::arch::naked_asm;
    naked_asm!(
        "or $r31, $r4, $zero",
        "bl {handle_shootdown}",

        "ld.d $r12, $r31, {status_off}",
        "csrwr $r12, {csr_prmd}",

        "ld.d $r12, $r31, {pc_off}",
        "csrwr $r12, {csr_era}",

        "ld.d $r12, $r31, {euen_off}",
        "andi $r13, $r12, {euen_fpe}",
        "beqz $r13, .Lresume_skip_fpu",

        "csrrd $r14, {csr_euen}",
        "ori $r15, $r14, {euen_fpe}",
        "csrwr $r15, {csr_euen}",

        "addi.d $r12, $r31, {f_off}",
        "fld.d $f0, $r12, 0 * 8",
        "fld.d $f1, $r12, 1 * 8",
        "fld.d $f2, $r12, 2 * 8",
        "fld.d $f3, $r12, 3 * 8",
        "fld.d $f4, $r12, 4 * 8",
        "fld.d $f5, $r12, 5 * 8",
        "fld.d $f6, $r12, 6 * 8",
        "fld.d $f7, $r12, 7 * 8",
        "fld.d $f8, $r12, 8 * 8",
        "fld.d $f9, $r12, 9 * 8",
        "fld.d $f10, $r12, 10 * 8",
        "fld.d $f11, $r12, 11 * 8",
        "fld.d $f12, $r12, 12 * 8",
        "fld.d $f13, $r12, 13 * 8",
        "fld.d $f14, $r12, 14 * 8",
        "fld.d $f15, $r12, 15 * 8",
        "fld.d $f16, $r12, 16 * 8",
        "fld.d $f17, $r12, 17 * 8",
        "fld.d $f18, $r12, 18 * 8",
        "fld.d $f19, $r12, 19 * 8",
        "fld.d $f20, $r12, 20 * 8",
        "fld.d $f21, $r12, 21 * 8",
        "fld.d $f22, $r12, 22 * 8",
        "fld.d $f23, $r12, 23 * 8",
        "fld.d $f24, $r12, 24 * 8",
        "fld.d $f25, $r12, 25 * 8",
        "fld.d $f26, $r12, 26 * 8",
        "fld.d $f27, $r12, 27 * 8",
        "fld.d $f28, $r12, 28 * 8",
        "fld.d $f29, $r12, 29 * 8",
        "fld.d $f30, $r12, 30 * 8",
        "fld.d $f31, $r12, 31 * 8",

        "ld.d $r12, $r31, {fcsr_off}",
        "movgr2fcsr $fcsr0, $r12",

        "ld.d $r13, $r31, {fcc_off}",
        "andi $r14, $r13, 0x1",
        "movgr2cf $fcc0, $r14",
        "srli.d $r14, $r13, 1",
        "andi $r14, $r14, 0x1",
        "movgr2cf $fcc1, $r14",
        "srli.d $r14, $r13, 2",
        "andi $r14, $r14, 0x1",
        "movgr2cf $fcc2, $r14",
        "srli.d $r14, $r13, 3",
        "andi $r14, $r14, 0x1",
        "movgr2cf $fcc3, $r14",
        "srli.d $r14, $r13, 4",
        "andi $r14, $r14, 0x1",
        "movgr2cf $fcc4, $r14",
        "srli.d $r14, $r13, 5",
        "andi $r14, $r14, 0x1",
        "movgr2cf $fcc5, $r14",
        "srli.d $r14, $r13, 6",
        "andi $r14, $r14, 0x1",
        "movgr2cf $fcc6, $r14",
        "srli.d $r14, $r13, 7",
        "andi $r14, $r14, 0x1",
        "movgr2cf $fcc7, $r14",

        ".Lresume_skip_fpu:",

        "ld.d $r12, $r31, {euen_off}",
        "andi $r13, $r12, {euen_sxe}",
        "beqz $r13, .Lresume_skip_lsx",

        "csrrd $r14, {csr_euen}",
        "ori $r15, $r14, {euen_sxe}",
        "csrwr $r15, {csr_euen}",

        "addi.d $r12, $r31, {lsx_off}",
        "vld $vr0, $r12, 0 * 16",     "vld $vr1, $r12, 1 * 16",
        "vld $vr2, $r12, 2 * 16",     "vld $vr3, $r12, 3 * 16",
        "vld $vr4, $r12, 4 * 16",     "vld $vr5, $r12, 5 * 16",
        "vld $vr6, $r12, 6 * 16",     "vld $vr7, $r12, 7 * 16",
        "vld $vr8, $r12, 8 * 16",     "vld $vr9, $r12, 9 * 16",
        "vld $vr10, $r12, 10 * 16",   "vld $vr11, $r12, 11 * 16",
        "vld $vr12, $r12, 12 * 16",   "vld $vr13, $r12, 13 * 16",
        "vld $vr14, $r12, 14 * 16",   "vld $vr15, $r12, 15 * 16",
        "vld $vr16, $r12, 16 * 16",   "vld $vr17, $r12, 17 * 16",
        "vld $vr18, $r12, 18 * 16",   "vld $vr19, $r12, 19 * 16",
        "vld $vr20, $r12, 20 * 16",   "vld $vr21, $r12, 21 * 16",
        "vld $vr22, $r12, 22 * 16",   "vld $vr23, $r12, 23 * 16",
        "vld $vr24, $r12, 24 * 16",   "vld $vr25, $r12, 25 * 16",
        "vld $vr26, $r12, 26 * 16",   "vld $vr27, $r12, 27 * 16",
        "vld $vr28, $r12, 28 * 16",   "vld $vr29, $r12, 29 * 16",
        "vld $vr30, $r12, 30 * 16",   "vld $vr31, $r12, 31 * 16",

        ".Lresume_skip_lsx:",

        "ld.d $r12, $r31, {euen_off}",
        "bstrins.d $r12, $r0, 5, 4",
        "csrwr $r12, {csr_euen}",

        "ld.d $r12, $r31, {llbctl_off}",
        "csrwr $r12, {csr_llbctl}",

        "ld.d $r1, $r31, {ra_off}",

        "ld.d $r2, $r31, {tp_off}",

        "ld.d $r4, $r31, {a0_off}",
        "ld.d $r5, $r31, {a1_off}",
        "ld.d $r6, $r31, {a2_off}",
        "ld.d $r7, $r31, {a3_off}",
        "ld.d $r8, $r31, {a4_off}",
        "ld.d $r9, $r31, {a5_off}",
        "ld.d $r10, $r31, {a6_off}",
        "ld.d $r11, $r31, {a7_off}",

        "ld.d $r12, $r31, {t0_off}",
        "ld.d $r13, $r31, {t1_off}",
        "ld.d $r14, $r31, {t2_off}",
        "ld.d $r15, $r31, {t3_off}",
        "ld.d $r16, $r31, {t4_off}",
        "ld.d $r17, $r31, {t5_off}",
        "ld.d $r18, $r31, {t6_off}",
        "ld.d $r19, $r31, {t7_off}",
        "ld.d $r20, $r31, {t8_off}",

        "ld.d $r21, $r31, {rx_off}",

        "ld.d $r22, $r31, {s0_off}",
        "ld.d $r23, $r31, {s1_off}",
        "ld.d $r24, $r31, {s2_off}",
        "ld.d $r25, $r31, {s3_off}",
        "ld.d $r26, $r31, {s4_off}",
        "ld.d $r27, $r31, {s5_off}",
        "ld.d $r28, $r31, {s6_off}",
        "ld.d $r29, $r31, {s7_off}",
        "ld.d $r30, $r31, {s8_off}",

        "ld.d $r3, $r31, {sp_off}",
        "ld.d $r31, $r31, {s9_off}",

        "ertn",

        status_off = const STATUS_OFFSET,
        pc_off = const PC_OFFSET,
        euen_off = const EUEN_OFFSET,
        llbctl_off = const LLBCTL_OFFSET,
        ra_off = const RA_OFFSET,
        tp_off = const TP_OFFSET,
        sp_off = const SP_OFFSET,
        a0_off = const A0_OFFSET,
        a1_off = const A1_OFFSET,
        a2_off = const A2_OFFSET,
        a3_off = const A3_OFFSET,
        a4_off = const A4_OFFSET,
        a5_off = const A5_OFFSET,
        a6_off = const A6_OFFSET,
        a7_off = const A7_OFFSET,
        t0_off = const T0_OFFSET,
        t1_off = const T1_OFFSET,
        t2_off = const T2_OFFSET,
        t3_off = const T3_OFFSET,
        t4_off = const T4_OFFSET,
        t5_off = const T5_OFFSET,
        t6_off = const T6_OFFSET,
        t7_off = const T7_OFFSET,
        t8_off = const T8_OFFSET,
        rx_off = const RX_OFFSET,
        s0_off = const S0_OFFSET,
        s1_off = const S1_OFFSET,
        s2_off = const S2_OFFSET,
        s3_off = const S3_OFFSET,
        s4_off = const S4_OFFSET,
        s5_off = const S5_OFFSET,
        s6_off = const S6_OFFSET,
        s7_off = const S7_OFFSET,
        s8_off = const S8_OFFSET,
        s9_off = const S9_OFFSET,
        f_off = const F_OFFSET,
        lsx_off = const LSX_OFFSET,
        fcsr_off = const FCSR_OFFSET,
        fcc_off = const FCC_OFFSET,
        csr_prmd = const CSR_PRMD,
        csr_era = const CSR_ERA,
        csr_euen = const CSR_EUEN,
        csr_llbctl = const CSR_LLBCTL,
        euen_fpe = const EUEN_FPE,
        euen_sxe = const EUEN_SXE,
        handle_shootdown = sym crate::loongarch64::smp::handle_shootdown_requests,
    );
}

unsafe extern "C" fn __loongarch64_user_entry() -> ! {
    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack, preserves_flags));
        }
    }
}

unsafe extern "C" fn __loongarch64_demo_user_entry() -> ! {
    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack, preserves_flags));
        }
    }
}

unsafe extern "C" fn __loongarch64_idle_task() -> ! {
    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack, preserves_flags));
        }
    }
}
