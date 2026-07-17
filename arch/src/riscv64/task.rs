//! RISC-V64 任务（trap frame）操作与上下文恢复。
//!
//! 本模块实现 [`general::TaskOps`] trait，提供：
//!
//! - trap frame 字段的读写（pc / sp / status / args）；
//! - `sscratch`/per-hart TrapAnchor 管理（用户态 trap 时恢复内核栈）；
//! - [`__riscv64_resume_to_trap_frame`]：设 sepc/sstatus/satp，恢复通用寄存器后 sret；
//! - 内核任务 / 用户任务 / idle 任务入口桩。

use general::{TaskOps, TrapFramePtr};

use crate::riscv64::specific::{
    CSR_FCSR, CSR_SEPC, CSR_SSCRATCH, CSR_SSTATUS, EXC_ECALL_S, EXC_ECALL_U, FRAME_SIZE,
    SSTATUS_FS_CLEAN, SSTATUS_FS_INITIAL, SSTATUS_FS_MASK, SSTATUS_SIE, SSTATUS_SPIE, SSTATUS_SPP,
    SSTATUS_USER_RESTORE_MASK, SSTATUS_USER_RETURN_BASE, SSTATUS_UXL_64, SSTATUS_VS_MASK,
    TrapFrame,
};
use core::arch::naked_asm;

/// satp 中除 ASID[59:44] 外用于标识地址空间根的 MODE + PPN 位。
const SATP_ADDRESS_SPACE_MASK: usize = !(0xffffusize << 44);

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

    fn set_kernel_trap_stack(stack_top: usize) {
        unsafe { crate::riscv64::specific::set_current_kernel_stack_top(stack_top) };
        // RISC-V trap entry 依赖 sscratch=0 表示普通 S-mode；
        // 返回 U-mode 时 resume_to_trap_frame 会发布当前 HartLocal TrapAnchor。
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
        // 使用 Initial 而不是 Off：不依赖 OpenSBI 是否直接委托 illegal instruction。
        // 首次 resume 的 fld 会把硬件 FS 标成 Dirty，第一次 trap 因而会把零初始化
        // 状态落到任务自己的固定 TrapFrame；之后按 Dirty/Clean 状态机增量保存。
        tf.status = SSTATUS_SPIE | SSTATUS_UXL_64 | SSTATUS_FS_INITIAL;
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
        crate::riscv64::mm::user_copy::copy_instruction_from_user(pc, &mut insn).ok()?;
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
/// 2. 根据可信 `kstack_top` 区分 U/S 返回；U-mode 返回时切换 satp
/// 3. 条件恢复 FPU（检查 FS 字段）
/// 4. 恢复通用寄存器
/// 5. 在最终窗口发布 sscratch 并 sret
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __riscv64_resume_to_trap_frame(_tf_ptr: usize) {
    naked_asm!(
        "mv s11, a0",

        // 写 sepc
        "ld t0, {sepc_off}(s11)",
        "csrw {sepc}, t0",

        // kstack_top 是由 arch trap 入口或内核上下文构造代码写入的可信返回类型标记：
        // 非零表示 U-mode，零表示 S-mode。不能再用用户可修改的 status.SPP 判定。
        "ld t2, {kstack_top_off}(s11)",
        "beqz t2, 1f",
        // 发布到当前 per-hart anchor；覆盖初次 enter_user_mode 或任务切换中尚未
        // 调用 set_kernel_trap_stack 的情况。
        "sd t2, {hart_kstack_off}(tp)",

        // return-to-user：只恢复用户拥有的 FS/VS，强制 SPP/SIE/SUM/MXR=0、
        // SPIE=1、UXL=64。
        "ld t0, {status_off}(s11)",
        "li t1, {user_status_keep}",
        "and t0, t0, t1",
        "li t1, {user_status_base}",
        "or t0, t0, t1",
        "csrw {sstatus}, t0",

        // TrapFrame 可能跨 ASID generation 休眠。只比较 MODE+PPN：地址空间根
        // 相同时保留 activate() 已安装的新 ASID，并回写 frame，不能让旧 frame
        // 把回卷前的 ASID 写回硬件。
        "ld t2, {satp_off}(s11)",
        "csrr t3, {satp}",
        "xor t4, t2, t3",
        "li t5, {satp_address_space_mask}",
        "and t4, t4, t5",
        "beqz t4, 8f",
        "csrw {satp}, t2",
        "sfence.vma",
        "j 9f",
        "8:",
        "sd t3, {satp_off}(s11)",
        "9:",

        // Vector helper 只在用户 VS active 时调用，避免普通 syscall 的函数调用开销。
        "ld t0, {status_off}(s11)",
        "li t1, {vs_mask}",
        "and t1, t0, t1",
        "beqz t1, 2f",
        "mv a0, s11",
        "call {restore_vector}",
        "j 2f",

        // return-to-kernel：强制 SPP=1，并清 SIE 防止恢复期间被中断。
        "1:",
        "ld t0, {status_off}(s11)",
        "ori t0, t0, {spp}",
        "andi t0, t0, -3",
        "csrw {sstatus}, t0",

        "2:",
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

        // 恢复指令会把硬件 FS 标成 Dirty。Initial 用于首次从临时 frame 进入用户态，
        // 保持 Dirty 以确保第一次 trap 把状态写入任务固定 frame；其余已落盘状态
        // 改回 Clean，使用户未执行浮点指令时下一次 trap 可以跳过 32 次 fsd。
        "ld t0, {status_off}(s11)",
        "li t1, {fs_mask}",
        "and t0, t0, t1",
        "li t1, {fs_initial}",
        "beq t0, t1, 3f",
        "li t0, {fs_mask}",
        "csrc {sstatus}, t0",
        "li t0, {fs_clean}",
        "csrs {sstatus}, t0",

        // 通用寄存器恢复
        "3:",
        "ld ra, {ra_off}(s11)",
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

        // 到最终不可调用窗口才发布 sscratch。U-mode frame 以非零 kstack_top
        // 作为类型标记，但实际发布当前内核 tp（per-hart TrapAnchor）；被打断的
        // return-to-user kernel frame 则从 satp 字段恢复原 anchor 指针。
        "ld sp, {kstack_top_off}(s11)",
        "beqz sp, 7f",
        "csrw {sscratch}, tp",
        "j 6f",
        "7:",
        "ld sp, {satp_off}(s11)",
        "beqz sp, 5f",
        "csrw {sscratch}, sp",
        "j 6f",
        "5:",
        "csrw {sscratch}, x0",
        "6:",
        "ld tp, {tp_off}(s11)",
        "ld gp, {gp_off}(s11)",
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
        fs_initial = const SSTATUS_FS_INITIAL,
        fs_clean = const SSTATUS_FS_CLEAN,
        vs_mask = const SSTATUS_VS_MASK,
        user_status_keep = const SSTATUS_USER_RESTORE_MASK,
        user_status_base = const SSTATUS_USER_RETURN_BASE,
        hart_kstack_off = const crate::riscv64::specific::HART_LOCAL_KERNEL_STACK_TOP_OFF,
        satp_off = const crate::riscv64::specific::SATP_OFFSET,
        satp_address_space_mask = const SATP_ADDRESS_SPACE_MASK,
    );
}

/// syscall 最小返回路径使用的 FPU 恢复桩。
///
/// 入口 a0 指向 FS=Clean 的用户 TrapFrame。内核 Rust 调用期间可能使用过浮点
/// caller/callee-saved 寄存器，因此返回 U-mode 前完整恢复并把硬件 FS 改回 Clean。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __riscv64_restore_clean_fpu_for_fast_return(_tf_ptr: usize) {
    naked_asm!(
        ".option arch, +d",
        "ld t0, {fcsr_off}(a0)",
        "csrw {fcsr}, t0",
        "fld f0, {f_off}(a0)",     "fld f1, {f_off}+8(a0)",
        "fld f2, {f_off}+16(a0)",   "fld f3, {f_off}+24(a0)",
        "fld f4, {f_off}+32(a0)",   "fld f5, {f_off}+40(a0)",
        "fld f6, {f_off}+48(a0)",   "fld f7, {f_off}+56(a0)",
        "fld f8, {f_off}+64(a0)",   "fld f9, {f_off}+72(a0)",
        "fld f10, {f_off}+80(a0)",  "fld f11, {f_off}+88(a0)",
        "fld f12, {f_off}+96(a0)",  "fld f13, {f_off}+104(a0)",
        "fld f14, {f_off}+112(a0)", "fld f15, {f_off}+120(a0)",
        "fld f16, {f_off}+128(a0)", "fld f17, {f_off}+136(a0)",
        "fld f18, {f_off}+144(a0)", "fld f19, {f_off}+152(a0)",
        "fld f20, {f_off}+160(a0)", "fld f21, {f_off}+168(a0)",
        "fld f22, {f_off}+176(a0)", "fld f23, {f_off}+184(a0)",
        "fld f24, {f_off}+192(a0)", "fld f25, {f_off}+200(a0)",
        "fld f26, {f_off}+208(a0)", "fld f27, {f_off}+216(a0)",
        "fld f28, {f_off}+224(a0)", "fld f29, {f_off}+232(a0)",
        "fld f30, {f_off}+240(a0)", "fld f31, {f_off}+248(a0)",
        "li t0, {fs_mask}",
        "csrc {sstatus}, t0",
        "li t0, {fs_clean}",
        "csrs {sstatus}, t0",
        "ret",
        fcsr = const CSR_FCSR,
        fcsr_off = const crate::riscv64::specific::FCSR_OFFSET,
        f_off = const crate::riscv64::specific::F_OFFSET,
        fs_mask = const SSTATUS_FS_MASK,
        fs_clean = const SSTATUS_FS_CLEAN,
        sstatus = const CSR_SSTATUS,
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
