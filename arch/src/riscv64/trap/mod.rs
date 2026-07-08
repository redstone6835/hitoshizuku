//! RISC-V64 的异常处理。
//!
//! RISC-V64 的异常处理主要通过安装异常入口地址到处理器的 CSR_STVEC 寄存器来实
//! 现。当 CPU 捕获到异常时，会自动跳转到这个入口地址执行异常处理逻辑。我们在这个
//! 入口地址处编写了一个通用的异常处理器，它会根据当前的异常上下文（TrapFrame）和
//! 异常状态寄存器（CSR_SCAUSE）的值来分发异常到具体的处理器函数进行处理。异常处理
//! 器会保存所有必要的寄存器状态，并将异常信息传递给 Rust 代码进行进一步的处理和决策。

mod interrupt;
pub use interrupt::*;

mod exception;
pub use exception::*;

use core::arch::naked_asm;

use crate::*;

pub unsafe fn install_exception_entry() {
    let entry = __riscv_exception_entry as *const () as usize;
    unsafe {
        core::arch::asm!(
            "csrw {stvec}, {entry}",
            stvec = const CSR_STVEC,
            entry = in(reg) entry | STVEC_MODE_DIRECT,
            options(nostack, preserves_flags)
        );
        core::arch::asm!(
            "csrw {sscratch}, x0",
            sscratch = const CSR_SSCRATCH,
            options(nostack, preserves_flags)
        );
    }
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.trap.eentry")]
pub unsafe extern "C" fn __riscv_exception_entry() {
    naked_asm!(
        "csrrw t6, {sscratch}, t6",
        "bnez t6, 2f",

        // from_kernel: sscratch=0 是 S-mode 运行不变量；csrrw 后 sscratch
        // 临时保存原始 t6，t5 尚未被破坏，可以直接写入 TrapFrame。
        "3:",
        "addi t6, sp, -{frame_size}",
        "sd sp, {sp_off}(t6)",
        "sd tp, {tp_off}(t6)",
        "sd gp, {gp_off}(t6)",
        "sd t4, {t4_off}(t6)",
        "sd t5, {t5_off}(t6)",
        "csrr t5, {sscratch}",      // 取回原始 t6
        "sd t5, {t6_off}(t6)",
        "csrw {sscratch}, x0",      // sscratch 归零
        "mv sp, t6",
        "j 4f",

        // from_user: t6=内核栈顶(原sscratch), sscratch=用户原t6
        // 统一页表方案：不切 satp，用户 PGD 已包含完整内核映射
        "2:",
        "addi t6, t6, -{frame_size}",
        "sd sp, {sp_off}(t6)",          // 保存用户 sp
        "sd tp, {tp_off}(t6)",
        "sd gp, {gp_off}(t6)",
        "sd t4, {t4_off}(t6)",
        "sd t5, {t5_off}(t6)",
        "csrr t5, {sscratch}",          // t5 = 用户原始 t6
        "sd t5, {t6_off}(t6)",
        "csrr t4, {satp}",
        "sd t4, {satp_off}(t6)",        // 保存用户 satp（resume 时恢复）
        "addi t4, t6, {frame_size}",    // t4 = 内核栈顶（t6 + FRAME_SIZE）
        "sd t4, {kstack_top_off}(t6)",   // 保存内核栈顶，用于恢复 sscratch
        // 不切换 satp — 用户页表高半区已有内核映射，直接在用户 satp 下执行内核代码
        "csrw {sscratch}, x0",
        "la tp, {hart_locals_sym}",     // 恢复内核 tp → &HART_LOCALS[0]
        "ld gp, {kernel_gp_off}(tp)",    // 用户态 gp 不可用于内核代码

        // 系统调用快速路径判断：来自 U-mode 的 ecall（scause=8）跳过 FPU 保存
        "csrr t5, {scause}",
        "li t4, 8",
        "beq t5, t4, 10f",
        "j 4f",

        // 快速系统调用入口：保存通用寄存器但跳过 FPU
        "10:",
        "mv sp, t6",
        "sd ra, {ra_off}(sp)",
        "sd t0, {t0_off}(sp)",
        "sd t1, {t1_off}(sp)",
        "sd t2, {t2_off}(sp)",
        "sd s0, {s0_off}(sp)",
        "sd s1, {s1_off}(sp)",
        "sd a0, {a0_off}(sp)",
        "sd a1, {a1_off}(sp)",
        "sd a2, {a2_off}(sp)",
        "sd a3, {a3_off}(sp)",
        "sd a4, {a4_off}(sp)",
        "sd a5, {a5_off}(sp)",
        "sd a6, {a6_off}(sp)",
        "sd a7, {a7_off}(sp)",
        "sd s2, {s2_off}(sp)",
        "sd s3, {s3_off}(sp)",
        "sd s4, {s4_off}(sp)",
        "sd s5, {s5_off}(sp)",
        "sd s6, {s6_off}(sp)",
        "sd s7, {s7_off}(sp)",
        "sd s8, {s8_off}(sp)",
        "sd s9, {s9_off}(sp)",
        "sd s10, {s10_off}(sp)",
        "sd s11, {s11_off}(sp)",
        "sd t3, {t3_off}(sp)",

        "csrr t0, {sepc}",
        "sd t0, {sepc_off}(sp)",
        "csrr t0, {sstatus}",
        "sd t0, {status_off}(sp)",
        "sd t5, {cause_off}(sp)",       // t5 还是 scause=8
        // stval 对 syscall 无意义，跳过

        // 调用 syscall 快速路径 handler（不保存/恢复 FPU）
        "mv a0, sp",
        "ld a1, {sp_off}(sp)",
        "call {fast_handler}",

        // 返回值：0=halt, 奇数=完整恢复, 偶数(非零)=快速恢复
        "beqz a0, 6f",
        "andi t0, a0, 1",
        "bnez t0, 11f",

        // 快速 syscall resume：只恢复 ABI 要求的寄存器
        "mv s11, a0",
        "ld t0, {sepc_off}(s11)",
        "csrw {sepc}, t0",
        "ld t0, {status_off}(s11)",
        "andi t0, t0, -3",
        "csrw {sstatus}, t0",
        "ld t0, {kstack_top_off}(s11)",
        "csrw {sscratch}, t0",
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

        // 完整恢复路径（FPU active 或需要调度）
        "11:",
        "andi a0, a0, -2",
        "tail {resume}",

        // 保存上下文：
        "4:",
        "mv sp, t6",

        "sd ra, {ra_off}(sp)",
        "sd t0, {t0_off}(sp)",
        "sd t1, {t1_off}(sp)",
        "sd t2, {t2_off}(sp)",
        "sd s0, {s0_off}(sp)",
        "sd s1, {s1_off}(sp)",
        "sd a0, {a0_off}(sp)",
        "sd a1, {a1_off}(sp)",
        "sd a2, {a2_off}(sp)",
        "sd a3, {a3_off}(sp)",
        "sd a4, {a4_off}(sp)",
        "sd a5, {a5_off}(sp)",
        "sd a6, {a6_off}(sp)",
        "sd a7, {a7_off}(sp)",
        "sd s2, {s2_off}(sp)",
        "sd s3, {s3_off}(sp)",
        "sd s4, {s4_off}(sp)",
        "sd s5, {s5_off}(sp)",
        "sd s6, {s6_off}(sp)",
        "sd s7, {s7_off}(sp)",
        "sd s8, {s8_off}(sp)",
        "sd s9, {s9_off}(sp)",
        "sd s10, {s10_off}(sp)",
        "sd s11, {s11_off}(sp)",
        "sd t3, {t3_off}(sp)",

        "csrr t0, {sepc}",
        "sd t0, {sepc_off}(sp)",
        "csrr t0, {sstatus}",
        "sd t0, {status_off}(sp)",
        "csrr t0, {scause}",
        "sd t0, {cause_off}(sp)",
        "csrr t0, {stval}",
        "sd t0, {tval_off}(sp)",
        // satp 已在 from_user 入口处正确保存（切换内核页表前），此处不再覆盖

        "csrr t0, {sstatus}",
        "li t1, {fs_mask}",
        "and t0, t0, t1",
        "beqz t0, 5f",

        ".option arch, +d",
        "csrr t0, {fcsr}",
        "sd t0, {fcsr_off}(sp)",
        "fsd f0, {f_off}(sp)",     "fsd f1, {f_off}+8(sp)",
        "fsd f2, {f_off}+16(sp)",   "fsd f3, {f_off}+24(sp)",
        "fsd f4, {f_off}+32(sp)",   "fsd f5, {f_off}+40(sp)",
        "fsd f6, {f_off}+48(sp)",   "fsd f7, {f_off}+56(sp)",
        "fsd f8, {f_off}+64(sp)",   "fsd f9, {f_off}+72(sp)",
        "fsd f10, {f_off}+80(sp)",  "fsd f11, {f_off}+88(sp)",
        "fsd f12, {f_off}+96(sp)",  "fsd f13, {f_off}+104(sp)",
        "fsd f14, {f_off}+112(sp)", "fsd f15, {f_off}+120(sp)",
        "fsd f16, {f_off}+128(sp)", "fsd f17, {f_off}+136(sp)",
        "fsd f18, {f_off}+144(sp)", "fsd f19, {f_off}+152(sp)",
        "fsd f20, {f_off}+160(sp)", "fsd f21, {f_off}+168(sp)",
        "fsd f22, {f_off}+176(sp)", "fsd f23, {f_off}+184(sp)",
        "fsd f24, {f_off}+192(sp)", "fsd f25, {f_off}+200(sp)",
        "fsd f26, {f_off}+208(sp)", "fsd f27, {f_off}+216(sp)",
        "fsd f28, {f_off}+224(sp)", "fsd f29, {f_off}+232(sp)",
        "fsd f30, {f_off}+240(sp)", "fsd f31, {f_off}+248(sp)",

        // 跳过 FPU 保存：
        "5:",

        "mv a0, sp",
        "call {save_vector}",

        "mv a0, sp",
        "ld a1, {sp_off}(sp)",
        "call {handler}",

        // 处理器返回陷阱帧指针（a0），0 表示停机
        "beqz a0, 6f",
        "tail {resume}",

        // 停机：
        "6:",
        "wfi",
        "j 6b",

        handler = sym riscv64_handle_exception,
        fast_handler = sym riscv64_fast_syscall_dispatch,
        save_vector = sym crate::riscv64::vector::save_vector_from_trap_entry,
        resume = sym crate::riscv64::task::__riscv64_resume_to_trap_frame,
        frame_size = const FRAME_SIZE,
        kernel_gp_off = const HART_LOCAL_KERNEL_GP_OFF,
        hart_locals_sym = sym crate::riscv64::specific::HART_LOCALS,

        sscratch = const CSR_SSCRATCH,
        sepc = const CSR_SEPC,
        sstatus = const CSR_SSTATUS,
        scause = const CSR_SCAUSE,
        stval = const CSR_STVAL,
        satp = const CSR_SATP,
        fcsr = const CSR_FCSR,
        fs_mask = const SSTATUS_FS_MASK,

        ra_off = const RA_OFFSET,
        tp_off = const TP_OFFSET,
        sp_off = const SP_OFFSET,
        gp_off = const GP_OFFSET,
        t0_off = const T0_OFFSET,
        t1_off = const T1_OFFSET,
        t2_off = const T2_OFFSET,
        s0_off = const S0_OFFSET,
        s1_off = const S1_OFFSET,
        a0_off = const A0_OFFSET,
        a1_off = const A1_OFFSET,
        a2_off = const A2_OFFSET,
        a3_off = const A3_OFFSET,
        a4_off = const A4_OFFSET,
        a5_off = const A5_OFFSET,
        a6_off = const A6_OFFSET,
        a7_off = const A7_OFFSET,
        s2_off = const S2_OFFSET,
        s3_off = const S3_OFFSET,
        s4_off = const S4_OFFSET,
        s5_off = const S5_OFFSET,
        s6_off = const S6_OFFSET,
        s7_off = const S7_OFFSET,
        s8_off = const S8_OFFSET,
        s9_off = const S9_OFFSET,
        s10_off = const S10_OFFSET,
        s11_off = const S11_OFFSET,
        t3_off = const T3_OFFSET,
        t4_off = const T4_OFFSET,
        t5_off = const T5_OFFSET,
        t6_off = const T6_OFFSET,
        sepc_off = const SEPC_OFFSET,
        status_off = const STATUS_OFFSET,
        cause_off = const CAUSE_OFFSET,
        tval_off = const TVAL_OFFSET,
        satp_off = const SATP_OFFSET,
        kstack_top_off = const KSTACK_TOP_OFFSET,
        fcsr_off = const FCSR_OFFSET,
        f_off = const F_OFFSET,
    )
}
