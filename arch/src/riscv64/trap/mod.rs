//! RISC-V64 的异常处理。
//!
//! RISC-V64 的异常处理主要通过安装异常入口地址到处理器的 CSR_STVEC 寄存器来实
//! 现。当 CPU 捕获到异常时，会自动跳转到这个入口地址执行异常处理逻辑。我们在这个
//! 入口地址处编写了一个通用的异常处理器，它会根据当前的异常上下文（TrapFrame）和
//! 异常状态寄存器（CSR_SCAUSE）的值来分发异常到具体的处理器函数进行处理。异常处理
//! 器会保存所有必要的寄存器状态，并将异常信息传递给 Rust 代码进行进一步的处理和决策。

mod interrupt;
pub use interrupt::*;

pub mod exception;
pub use exception::*;

use core::arch::naked_asm;

use crate::*;

/// 为当前 hart 安装统一 trap 入口、开放 vDSO 所需的用户态 `rdtime`，并清空
/// `sscratch` 的临时锚点。
///
/// 这是 per-hart 初始化：boot hart 和每个 secondary hart 在进入调度器前都必须
/// 调用。把 `scounteren.TIME` 放在这里可避免只初始化 boot hart；任何能够处理
/// trap 并返回 U-mode 的 hart 都会同时具备 vDSO 时间快路径所需的 CSR 权限。
///
/// # Safety
///
/// - 必须在 S-mode 下为当前 hart 调用；
/// - 调用期间不能发生依赖旧 `stvec`/`sscratch` 状态的 trap；
/// - `__riscv_exception_entry` 的映射必须在后续运行期间保持有效；
/// - 返回用户态前，调用方必须按 trap-anchor 约定重新发布当前 hart 的 `sscratch`。
pub unsafe fn install_exception_entry() {
    let entry = __riscv_exception_entry as *const () as usize;
    // vDSO 的 clock_gettime/gettimeofday 会在 U-mode 直接执行 rdtime。
    // scounteren 是 per-hart CSR，必须由每个即将运行用户任务的 hart 设置。
    crate::set_csr!(scounteren, SCOUNTEREN_TIME);
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
        // sscratch 正常为 0（普通 S-mode）或当前 hart 的 TrapAnchor（这里直接使用
        // HartLocal 指针）。先保存会被入口逻辑占用的 t4/t5/t6，并标记脆弱入口窗口；
        // 若窗口内再次 trap，直接切 emergency stack 进入最小 double-fault 路径。
        "csrrw t6, {sscratch}, t6",
        "beqz t6, 20f",
        "sd t4, {entry_t4_off}(t6)",
        "sd t5, {entry_t5_off}(t6)",
        // sscratch 此刻保存原始 t6。先把它落到 HartLocal，再立即重新发布 anchor；
        // 后续任一同步 fault 才能可靠进入下面的嵌套判定，而不会把用户 t6 当指针。
        "csrrw t4, {sscratch}, t6",
        "sd t4, {entry_t6_off}(t6)",
        "ld t5, {entry_state_off}(t6)",
        "bnez t5, 90f",
        "li t5, 1",
        "sd t5, {entry_state_off}(t6)",
        "csrr t5, {sstatus}",
        "andi t5, t5, {spp}",
        "bnez t5, 21f",

        // from_user：真正的 TrapFrame 位于 top-2*FRAME_SIZE，上方完整预留一帧
        // 给最终 return-to-user 窗口可能发生的嵌套 S-mode fault。
        "ld t5, {kernel_stack_top_off}(t6)",
        "addi t5, t5, -{user_frame_span}",
        "sd sp, {sp_off}(t5)",
        "sd tp, {tp_off}(t5)",
        "sd gp, {gp_off}(t5)",
        "ld t4, {entry_t4_off}(t6)",
        "sd t4, {t4_off}(t5)",
        "ld t4, {entry_t5_off}(t6)",
        "sd t4, {t5_off}(t5)",
        "ld t4, {entry_t6_off}(t6)",
        "sd t4, {t6_off}(t5)",
        "ld t4, {kernel_stack_top_off}(t6)",
        "sd t4, {kstack_top_off}(t5)",
        "csrr t4, {satp}",
        "sd t4, {satp_off}(t5)",
        "mv tp, t6",
        "mv t6, t5",
        "mv sp, t6",
        "csrw {sscratch}, x0",
        "ld gp, {kernel_gp_off}(tp)",
        "j 23f",

        // from_kernel 且入口 sscratch 非零：fault 发生在最终用户返回窗口。
        // 使用预留的 top-FRAME_SIZE 槽，不覆盖下方正在恢复的用户 TrapFrame；
        // tp/gp 可能已恢复成用户值，因此同时恢复内核 HartLocal。
        // satp 字段在 kernel frame 中不参与地址空间恢复，借其保存返回锚点。
        "21:",
        "ld t5, {kernel_stack_top_off}(t6)",
        "addi t5, t5, -{frame_size}",
        "sd sp, {sp_off}(t5)",
        "sd tp, {tp_off}(t5)",
        "sd gp, {gp_off}(t5)",
        "ld t4, {entry_t4_off}(t6)",
        "sd t4, {t4_off}(t5)",
        "ld t4, {entry_t5_off}(t6)",
        "sd t4, {t5_off}(t5)",
        "ld t4, {entry_t6_off}(t6)",
        "sd t4, {t6_off}(t5)",
        "sd t6, {satp_off}(t5)",
        "sd zero, {kstack_top_off}(t5)",
        "mv tp, t6",
        "mv t6, t5",
        "ld gp, {kernel_gp_off}(tp)",
        "csrw {sscratch}, x0",
        "mv sp, t6",
        "j 4f",

        // 普通 from_kernel：sscratch=0，直接在被中断内核栈下方保存 frame。
        "20:",
        // 第一次 csrrw 把原始 t6 放进 sscratch；这里原子取回并发布 tp anchor。
        "csrrw t6, {sscratch}, tp",
        "sd t4, {entry_t4_off}(tp)",
        "sd t5, {entry_t5_off}(tp)",
        "sd t6, {entry_t6_off}(tp)",
        "ld t5, {entry_state_off}(tp)",
        "bnez t5, 91f",
        "li t5, 1",
        "sd t5, {entry_state_off}(tp)",
        "addi t6, sp, -{frame_size}",
        "sd sp, {sp_off}(t6)",
        "sd tp, {tp_off}(t6)",
        "sd gp, {gp_off}(t6)",
        "ld t4, {entry_t4_off}(tp)",
        "sd t4, {t4_off}(t6)",
        "ld t5, {entry_t5_off}(tp)",
        "sd t5, {t5_off}(t6)",
        "ld t5, {entry_t6_off}(tp)",
        "sd t5, {t6_off}(t6)",
        "sd zero, {satp_off}(t6)",
        "sd zero, {kstack_top_off}(t6)",
        "csrw {sscratch}, x0",
        "mv sp, t6",
        "j 4f",

        // t6 非零路径的 anchor 在 t6；普通 kernel 路径的 anchor 仍在 tp。
        "90:",
        "mv tp, t6",
        "j 92f",
        "91:",
        "92:",
        "ld t5, {entry_state_off}(tp)",
        "li t4, 2",
        "bgeu t5, t4, 93f",
        "sd t4, {entry_state_off}(tp)",
        "ld sp, {irq_stack_top_off}(tp)",
        "ld gp, {kernel_gp_off}(tp)",
        "csrw {sscratch}, x0",
        // Rust release 构建可能使用 FPU；fatal helper 前防御性打开 FS。
        "li t4, {fs_dirty}",
        "csrs {sstatus}, t4",
        "tail {double_fault}",
        "93:",
        "csrci {sstatus}, 2",
        "94:",
        "wfi",
        "j 94b",

        "23:",

        // 系统调用快速路径只接受来自 U-mode 的 ecall。Vector 必须为 Off；FPU
        // 可以为 Off 或 Clean，后者已有可信内存副本，内核调用后在 sret 前重载即可。
        "csrr t5, {scause}",
        "li t4, 8",
        "bne t5, t4, 4f",
        "csrr t4, {sstatus}",
        "sd t4, {status_off}(t6)",
        "li t5, {vs_mask}",
        "and t5, t4, t5",
        "bnez t5, 4f",
        "li t5, {fs_mask}",
        "and t5, t4, t5",
        "beqz t5, 10f",
        "li t4, {fs_clean}",
        "bne t5, t4, 4f",

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
        "li t0, 8",
        "sd t0, {cause_off}(sp)",
        // syscall 的 stval 无语义，借该字段保存入口时的 context-switch sequence。
        // signal/exec 等完整恢复路径不依赖它；下一次硬件 trap 会重新覆盖。
        "ld t0, {switch_seq_off}(tp)",
        "sd t0, {tval_off}(sp)",

        // 普通内核 syscall 不生成 FPU 指令，保持入口的 FS=Off/Clean 和 live FPR。
        // 若内部发生调度，switch sequence 会变化，返回桩再从 frame 恢复。

        // 调用 syscall 快速路径 handler（不保存/恢复 FPU）
        "sd zero, {entry_state_off}(tp)",
        "mv a0, sp",
        "ld a1, {sp_off}(sp)",
        "call {fast_handler}",

        // 返回值：0=halt, 奇数=完整恢复, 偶数(非零)=快速恢复
        "beqz a0, 6f",
        "andi t0, a0, 1",
        "bnez t0, 11f",

        // 快速 syscall resume：Rust ABI 已保持 s0-s10；frame-rewrite syscall 会被
        // handler 强制送往完整恢复，因此这里无需重复加载未变化的 callee-saved GPR。
        // handler 返回时 SIE 可能已开启；先关中断，再进入脆弱恢复窗口。
        "csrci {sstatus}, 2",
        "mv s11, a0",
        "li t0, 1",
        "sd t0, {entry_state_off}(tp)",
        "ld t2, {switch_seq_off}(tp)",
        "ld t1, {tval_off}(s11)",
        "xor t2, t2, t1",
        // 同时捕获硬件 FS；若未来普通内核代码意外使用 FPU，Dirty 也会强制恢复。
        "csrr t3, {sstatus}",
        "li t1, {fs_mask}",
        "and t3, t3, t1",
        "ld t0, {sepc_off}(s11)",
        // 与 Linux 一致，在返回用户态前用不会改变 sepc 的条件存储清除可能由
        // 用户 LR 指令遗留的 reservation，避免它跨越 syscall/trap 边界存活。
        "addi t1, s11, {sepc_off}",
        ".option push",
        ".option arch, +zalrsc",
        "sc.d zero, t0, (t1)",
        ".option pop",
        "csrw {sepc}, t0",
        "ld t0, {status_off}(s11)",
        "li t1, {user_status_keep}",
        "and t0, t0, t1",
        "li t1, {user_status_base}",
        "or t0, t0, t1",
        "csrw {sstatus}, t0",

        // FS=Off 没有用户状态。FS=Clean 仅在没有 context switch 且硬件仍为
        // Clean 时复用 live FPR；任一条件不满足都从可信 frame 恢复。
        "ld t0, {status_off}(s11)",
        "li t1, {fs_mask}",
        "and t0, t0, t1",
        "beqz t0, 32f",
        "bnez t2, 31f",
        "li t1, {fs_clean}",
        "beq t3, t1, 32f",
        "31:",
        "mv a0, s11",
        "call {restore_fast_fpu}",
        "32:",
        "ld ra, {ra_off}(s11)",
        "ld t0, {t0_off}(s11)",
        "ld t1, {t1_off}(s11)",
        "ld t2, {t2_off}(s11)",
        "ld a0, {a0_off}(s11)",
        "ld a1, {a1_off}(s11)",
        "ld a2, {a2_off}(s11)",
        "ld a3, {a3_off}(s11)",
        "ld a4, {a4_off}(s11)",
        "ld a5, {a5_off}(s11)",
        "ld a6, {a6_off}(s11)",
        "ld a7, {a7_off}(s11)",
        "ld t3, {t3_off}(s11)",
        "ld t4, {t4_off}(s11)",
        "ld t5, {t5_off}(s11)",
        "ld t6, {t6_off}(s11)",

        // 发布当前内核 tp 指向的 per-hart TrapAnchor。tp/gp 延迟到此后恢复，
        // 最终窗口内的 S-mode fault 可直接由 anchor 找回内核栈和 kernel gp。
        "sd zero, {entry_state_off}(tp)",
        "csrw {sscratch}, tp",
        "ld tp, {tp_off}(s11)",
        "ld gp, {gp_off}(s11)",
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

        "ld t0, {status_off}(sp)",
        "li t1, {fs_mask}",
        "and t2, t0, t1",
        "beqz t2, 12f",
        // kernel-origin trap 的 frame 位于任意内核栈位置，不能依赖旧内存副本；
        // 只有固定任务 frame 上的 user FS=Clean 才能安全跳过整组保存。
        "andi t3, t0, {spp}",
        "bnez t3, 18f",
        "li t1, {fs_clean}",
        "beq t2, t1, 13f",

        "18:",

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

        // 保存完成后把可信 frame 状态转成 Clean。硬件 FS 随后保持 Dirty，供内核
        // Rust 代码使用；返回路径重新加载用户状态并把硬件标回 Clean。
        "ld t0, {status_off}(sp)",
        "li t1, {fs_clear_mask}",
        "and t0, t0, t1",
        "li t1, {fs_clean}",
        "or t0, t0, t1",
        "sd t0, {status_off}(sp)",
        "j 13f",

        // FS=Off：先为普通 Rust handler 打开当前 S-mode 的 FS。固定 user frame
        // 在任务创建时已经清零；只有 kernel-origin 的临时栈 frame 仍需初始化，
        // 避免后续内核诊断把旧栈内容误认为扩展状态。
        "12:",
        "li t1, {fs_dirty}",
        "csrs {sstatus}, t1",
        "ld t0, {status_off}(sp)",
        "andi t0, t0, {spp}",
        "beqz t0, 19f",
        "mv a0, sp",
        "call {zero_fpu}",
        "j 19f",
        "13:",
        "li t1, {fs_dirty}",
        "csrs {sstatus}, t1",
        "19:",

        // 保留 frame 指针到 callee-saved s0。普通 kernel-origin trap 继续使用当前
        // 任务的 64 KiB 内核栈；只有最终 return-to-user 窗口生成的 kernel frame
        // 才以非零 satp 字段标记，并切到 per-hart 紧急栈。
        "mv s0, sp",
        "li s1, 0",
        "ld t0, {status_off}(s0)",
        "andi t0, t0, {spp}",
        "beqz t0, 14f",
        "ld t0, {satp_off}(s0)",
        "beqz t0, 14f",
        "ld t1, {irq_stack_top_off}(tp)",
        "beqz t1, 14f",
        "ld t2, {sp_off}(s0)",
        "li t3, {irq_stack_size}",
        "sub t3, t1, t3",
        "bltu t2, t3, 15f",
        "bgeu t2, t1, 15f",
        "j 14f",
        "15:",
        "mv sp, t1",
        "li s1, 1",
        "14:",

        // Vector helper 只在 user-origin 且 VS active 时调用。
        "ld t0, {status_off}(s0)",
        "andi t1, t0, {spp}",
        "bnez t1, 16f",
        "li t1, {vs_mask}",
        "and t1, t0, t1",
        "beqz t1, 16f",
        "mv a0, s0",
        "call {save_vector}",
        "16:",

        // Rust handler 可能调度并切换任务；脆弱入口窗口必须在此之前结束，
        // 否则 per-hart 状态会把其它任务的正常 trap 误判为嵌套 fault。
        "sd zero, {entry_state_off}(tp)",
        "mv a0, s0",
        "ld a1, {sp_off}(s0)",
        "call {handler}",

        // 紧急栈只走极短的最终返回窗口，退出前检查低地址缓冲区。s1/s2 是
        // 汇编调用者自用的 callee-saved 寄存器，真正用户值仍保存在 TrapFrame。
        "mv s2, a0",
        "beqz s1, 17f",
        "call {check_irq_stack_guard}",
        "17:",
        "mv a0, s2",

        // 处理器返回陷阱帧指针（a0），0 表示停机
        "beqz a0, 6f",
        "tail {resume}",

        // fatal handler 返回 0：使用 SRST/无锁 wfi 收敛，不在损坏状态继续运行。
        "6:",
        "tail {fatal_shutdown}",

        handler = sym riscv64_handle_exception,
        fast_handler = sym riscv64_fast_syscall_dispatch,
        zero_fpu = sym riscv64_zero_inactive_fpu_frame,
        restore_fast_fpu = sym crate::riscv64::task::__riscv64_restore_clean_fpu_for_fast_return,
        save_vector = sym crate::riscv64::vector::save_vector_from_trap_entry,
        check_irq_stack_guard = sym crate::riscv64::specific::riscv64_check_irq_stack_guard,
        double_fault = sym crate::riscv64::specific::riscv64_double_fault,
        fatal_shutdown = sym crate::riscv64::specific::riscv64_fatal_trap_shutdown,
        resume = sym crate::riscv64::task::__riscv64_resume_to_trap_frame,
        frame_size = const FRAME_SIZE,
        user_frame_span = const (FRAME_SIZE * 2),
        kernel_stack_top_off = const HART_LOCAL_KERNEL_STACK_TOP_OFF,
        kernel_gp_off = const HART_LOCAL_KERNEL_GP_OFF,
        entry_t4_off = const HART_LOCAL_TRAP_ENTRY_T4_OFF,
        entry_t5_off = const HART_LOCAL_TRAP_ENTRY_T5_OFF,
        entry_t6_off = const HART_LOCAL_TRAP_ENTRY_T6_OFF,
        entry_state_off = const HART_LOCAL_TRAP_ENTRY_STATE_OFF,
        irq_stack_top_off = const HART_LOCAL_IRQ_STACK_TOP_OFF,
        irq_stack_size = const IRQ_STACK_SIZE,
        switch_seq_off = const HART_LOCAL_CONTEXT_SWITCH_SEQ_OFF,

        sscratch = const CSR_SSCRATCH,
        sepc = const CSR_SEPC,
        sstatus = const CSR_SSTATUS,
        scause = const CSR_SCAUSE,
        stval = const CSR_STVAL,
        satp = const CSR_SATP,
        fcsr = const CSR_FCSR,
        fs_mask = const SSTATUS_FS_MASK,
        fs_clear_mask = const (!SSTATUS_FS_MASK),
        fs_clean = const SSTATUS_FS_CLEAN,
        vs_mask = const SSTATUS_VS_MASK,
        fs_dirty = const SSTATUS_FS_DIRTY,
        user_status_keep = const SSTATUS_USER_RESTORE_MASK,
        user_status_base = const SSTATUS_USER_RETURN_BASE,
        spp = const SSTATUS_SPP,

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
