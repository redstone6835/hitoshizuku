//! LoongArch64 的异常处理。
//!
//! LoongArch64 的异常处理主要通过安装异常入口地址到处理器的 CSR_EENTRY 寄存器来实
//! 现。当 CPU 捕获到异常时，会自动跳转到这个入口地址执行异常处理逻辑。我们在这个
//! 入口地址处编写了一个通用的异常处理器，它会根据当前的异常上下文（TrapFrame）和
//! 异常状态寄存器（ESTAT）的值来分发异常到具体的处理器函数进行处理。异常处理器会
//! 保存所有必要的寄存器状态，并将异常信息传递给 Rust 代码进行进一步的处理和决策。

mod interrupt;
pub use interrupt::*;

mod exception;
pub use exception::*;

use core::arch::naked_asm;

use crate::*;

pub const ESTAT_IS_MASK: usize = 0x7fff;

#[inline]
/// 把 DMW1 高半区地址还原为物理地址。
///
/// 早期 `TLBRENTRY` 和 `MERRENTRY` 这类入口在 `DA=1, PG=0` 模式下按物理地址解释，
/// 不能直接写入带 DMW1 前缀的高半区虚拟地址，因此需要显式做一次窗口基址还原。
const fn dmw1_virt_to_phys(vaddr: usize) -> usize {
    vaddr.wrapping_sub(DMW1_CACHED_BASE)
}

/// 安装异常入口地址到处理器。
/// 该函数将 [`__loongarch_exception_entry`]、[`__loongarch_tlb_refill_entry`] 和
/// [`__loongarch_machine_error_entry`] 的地址分别写入 `CSR_EENTRY`（通用异常入口）、
/// `CSR_TLBRENTRY`（TLB 重填快路径）和 `CSR_MERRENTRY`（机器错误入口）。
///
/// # Safety
/// 必须保证：
/// 1. 异常入口地址有效且可执行；
/// 2. 在允许写 CSR 的特权级调用 (内核态)；
/// 3. 只在系统初始化时调用一次或在禁用中断的情况下调用。
///
/// 这里实际建立的是三条不同语义的入口：
///
/// - `EENTRY`：普通异常与中断入口，负责完整保存现场并进入 Rust 分发；
/// - `TLBRENTRY`：TLB refill 快路径，尽量依赖硬件页表遍历快速完成；
/// - `MERRENTRY`：机器错误入口，单独处理独立 CSR 现场。
pub unsafe fn install_exception_entry() {
    let exception_entry = __loongarch_exception_entry as *const () as usize;
    let tlbrentry_phys = dmw1_virt_to_phys(__loongarch_tlb_refill_entry as *const () as usize);
    let merrentry_phys = dmw1_virt_to_phys(__loongarch_machine_error_entry as *const () as usize);

    unsafe {
        core::arch::asm!(
            // 统一配置为 0 间距向量模式（VS=0），确保异常入口基址语义确定。
            "ori $r13, $r0, 0x7",
            "slli.d $r13, $r13, {ecfg_vs_offset}",
            "csrxchg $r0, $r13, {csr_ecfg}",

            // 通用异常入口使用当前虚拟地址（由正常地址翻译路径解析）。
            "csrwr {exception_entry}, {csr_eentry}",
            // TLB 重填与机器错误入口在 DA=1,PG=0 模式下取址，必须写入物理地址。
            "csrwr {tlbrentry_phys}, {csr_tlbrentry}",
            "csrwr {merrentry_phys}, {csr_merrentry}",
            // KS0 预置为当前内核异常栈栈顶，供用户态陷入时切栈使用。
            "or $r12, $r3, $r0",
            "csrwr $r12, {csr_ks0}",
            exception_entry = in(reg) exception_entry,
            tlbrentry_phys = in(reg) tlbrentry_phys,
            merrentry_phys = in(reg) merrentry_phys,
            csr_ecfg = const CSR_ECFG,
            csr_eentry = const CSR_EENTRY,
            csr_tlbrentry = const CSR_TLBRENTRY,
            csr_merrentry = const CSR_MERRENTRY,
            csr_ks0 = const CSR_KS0,
            ecfg_vs_offset = const CSR_ECFG_VS_OFFSET,
            out("$r12") _,
            out("$r13") _,
        );
    }
}

/// 机器错误入口。
///
/// 根据 LoongArch 规范，机器错误异常使用独立入口和独立 CSR 现场。
/// 这里将 `CSR_MERRENTRY` 指向独立处理逻辑，避免误走通用异常入口读取 PRMD/ERA。
///
/// 当前策略是 fail-stop：保存一个最小寄存器现场后直接停机。这样做偏保守，但更符合
/// 早期内核对“机器错误后状态已经不再可信”的判断。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.trap.merr")]
pub unsafe extern "C" fn __loongarch_machine_error_entry() {
    naked_asm!(
        // 保留一个通用寄存器现场，便于后续扩展机器错误报告路径。
        "csrwr $r12, {csr_merrsave}",

        // 当前实现对机器错误采用停机策略。
        ".L_merr_halt:",
        "idle 0",
        "b .L_merr_halt",

        csr_merrsave = const CSR_MERRSAVE,
    )
}
/// TLB 快路径处理器。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.trap.tlbr")]
pub unsafe extern "C" fn __loongarch_tlb_refill_entry() {
    naked_asm!(
        // 保存 $r12 寄存器的值到 CSR_TLBRSAVE，避免原始值在后续过程中被破坏。
        "csrwr $r12, {csr_tlbrsave}",

        // 从 CSR_PGD 中读取页表根目录的物理地址到 $r12，准备进行页表遍历。
        "csrrd $r12, {csr_pgd}",

        // 4 级页表遍历：Dir3 -> Dir2 -> Dir1 -> PTE。
        "lddir $r12, $r12, 3",
        "lddir $r12, $r12, 2",
        "lddir $r12, $r12, 1",

        // 从当前 $r12 指向的 PTE 页的物理地址处加载一对页表项（PTE0 和 PTE1），
        // 并将其填充到 TLB 中，完成 TLB 重填。
        "ldpte $r12, 0",
        "ldpte $r12, 1",
        "tlbfill",

        // 恢复 $r12 寄存器的值并返回。
        "csrrd $r12, {csr_tlbrsave}",
        "ertn",

        csr_pgd = const CSR_PGD,
        csr_tlbrsave = const CSR_TLBRSAVE,
    )
}

/// 通用异常入口处理器。
///
/// 这条入口的职责是先把硬件异常现场规范化成 `TrapFrame`，再把控制权转交给 Rust。
/// 它首先区分“来自用户态还是内核态”，原因在于两者的栈语义不同：用户态陷入必须切换
/// 到内核异常栈，而内核态陷入可以继续沿用当前内核栈。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.trap.eentry")]
pub unsafe extern "C" fn __loongarch_exception_entry() {
    naked_asm!(
        // 保护 $r12 的值。
        "csrwr $r12, {csr_ks1}",

        // 读取 CSR_PRMD 寄存器，检查当前异常是来自用户态还是内核态。
        "csrrd $r12, {csr_prmd}",
        "andi $r12, $r12, {prmd_pplv_mask}",
        "beqz $r12, .L_from_kernel",

        // ========= 用户态异常处理逻辑 ========
        //
        // 从内核预先写入的 KS0 读取当前 CPU 异常栈栈顶。
        "csrrd $r12, {csr_ks0}",
        "addi.d $r12, $r12, -{frame_size}",

        // 用户态异常必须切换到内核栈。将当前的用户栈指针 ($r3) 和线程指针 ($r2)
        // 保存到新的陷阱帧中，为后续的异常处理做好准备。
        "st.d $r3, $r12, {sp_offset}",
        "st.d $r2, $r12, {tp_offset}",

        // 更新 $r3 寄存器为新的内核异常栈指针并切换到公共的异常处理逻辑。
        "or $r3, $r12, $r0",
        "b .L_save_context",

        // ========= 内核态异常处理逻辑 ========
        //
        // 内核态异常不切换栈，直接使用当前栈指针 ($r3) 进行异常处理。
        ".L_from_kernel:",

        "addi.d $r12, $r3, -{frame_size}",
        "st.d $r3, $r12, {sp_offset}",
        "st.d $r2, $r12, {tp_offset}",
        "or $r3, $r12, $r0",

        // =========  公共异常处理逻辑  ========
        ".L_save_context:",

        // 暂时恢复 $r12 寄存器的值，以便后续寄存器状态的保存。
        "csrrd $r12, {csr_ks1}",

        // 保存剩下的通用寄存器 $r1..$r31 到陷阱帧中。
        "st.d $r1, $r3, {ra_offset}",
        "st.d $r4, $r3, {a0_offset}",   "st.d $r5, $r3, {a1_offset}",
        "st.d $r6, $r3, {a2_offset}",   "st.d $r7, $r3, {a3_offset}",
        "st.d $r8, $r3, {a4_offset}",   "st.d $r9, $r3, {a5_offset}",
        "st.d $r10, $r3, {a6_offset}",  "st.d $r11, $r3, {a7_offset}",

        "st.d $r12, $r3, {t0_offset}",  "st.d $r13, $r3, {t1_offset}",
        "st.d $r14, $r3, {t2_offset}",  "st.d $r15, $r3, {t3_offset}",
        "st.d $r16, $r3, {t4_offset}",  "st.d $r17, $r3, {t5_offset}",
        "st.d $r18, $r3, {t6_offset}",  "st.d $r19, $r3, {t7_offset}",
        "st.d $r20, $r3, {t8_offset}",

        "st.d $r21, $r3, {rx_offset}",

        "st.d $r22, $r3, {s0_offset}",  "st.d $r23, $r3, {s1_offset}",
        "st.d $r24, $r3, {s2_offset}",  "st.d $r25, $r3, {s3_offset}",
        "st.d $r26, $r3, {s4_offset}",  "st.d $r27, $r3, {s5_offset}",
        "st.d $r28, $r3, {s6_offset}",  "st.d $r29, $r3, {s7_offset}",
        "st.d $r30, $r3, {s8_offset}",  "st.d $r31, $r3, {s9_offset}",

        // 保存程序计数器和状态寄存器的值到陷阱帧中。
        "csrrd $r12, {csr_era}",
        "st.d $r12, $r3, {pc_offset}",
        "csrrd $r12, {csr_prmd}",
        "st.d $r12, $r3, {status_offset}",
        "csrrd $r12, {csr_euen}",
        "csrrd $r14, {csr_llbctl}",
        "st.d $r14, $r3, {llbctl_offset}",

        // 当前内核不使用 LSX/LASX 寄存器，用户态启用 SXE/ASXE 时 trap 路径只保存
        // EUEN 并在返回时恢复。BTE 仍未接入上下文管理，保留 fail-closed。
        "andi $r13, $r12, {euen_unsupported_context_mask}",
        "bnez $r13, .L_halt",

        // 基于入口 FPE 决定是否保存 FPU，结果以 FPU_SAVED 标志存在 bit 4。
        // 恢复路径据此判断，不受 handler 改写 tf.euen 影响。
        "andi $r13, $r12, {euen_fpe}",
        "beqz $r13, .L_no_fpu_save",

        // 入口 FPE=1: 置标志后存 EUEN。
        "ori $r12, $r12, {fpu_saved}",
        "st.d $r12, $r3, {euen_offset}",

        // 保存 FCSR 寄存器的值到陷阱帧中。FCSR 包含了浮点异常标志和控制位。
        "movfcsr2gr $r12, $fcsr0",
        "st.d $r12, $r3, {fcsr_offset}",

        // 把 FCC0..FCC7 逐位打包到 r13[7:0]，然后保存到陷阱帧中。
        "movcf2gr $r13, $fcc0",
        "andi $r13, $r13, 0x1",
        "movcf2gr $r14, $fcc1",
        "andi $r14, $r14, 0x1",
        "slli.d $r14, $r14, 1",
        "or $r13, $r13, $r14",
        "movcf2gr $r14, $fcc2",
        "andi $r14, $r14, 0x1",
        "slli.d $r14, $r14, 2",
        "or $r13, $r13, $r14",
        "movcf2gr $r14, $fcc3",
        "andi $r14, $r14, 0x1",
        "slli.d $r14, $r14, 3",
        "or $r13, $r13, $r14",
        "movcf2gr $r14, $fcc4",
        "andi $r14, $r14, 0x1",
        "slli.d $r14, $r14, 4",
        "or $r13, $r13, $r14",
        "movcf2gr $r14, $fcc5",
        "andi $r14, $r14, 0x1",
        "slli.d $r14, $r14, 5",
        "or $r13, $r13, $r14",
        "movcf2gr $r14, $fcc6",
        "andi $r14, $r14, 0x1",
        "slli.d $r14, $r14, 6",
        "or $r13, $r13, $r14",
        "movcf2gr $r14, $fcc7",
        "andi $r14, $r14, 0x1",
        "slli.d $r14, $r14, 7",
        "or $r13, $r13, $r14",
        "st.d $r13, $r3, {fcc_offset}",

        // 保存 F0..F31 浮点寄存器的值到陷阱帧中。
        "addi.d $r12, $r3, {f_offset}",
        "fst.d $f0, $r12, 0 * 8",     "fst.d $f1, $r12, 1 * 8",
        "fst.d $f2, $r12, 2 * 8",     "fst.d $f3, $r12, 3 * 8",
        "fst.d $f4, $r12, 4 * 8",     "fst.d $f5, $r12, 5 * 8",
        "fst.d $f6, $r12, 6 * 8",     "fst.d $f7, $r12, 7 * 8",
        "fst.d $f8, $r12, 8 * 8",     "fst.d $f9, $r12, 9 * 8",
        "fst.d $f10, $r12, 10 * 8",   "fst.d $f11, $r12, 11 * 8",
        "fst.d $f12, $r12, 12 * 8",   "fst.d $f13, $r12, 13 * 8",
        "fst.d $f14, $r12, 14 * 8",   "fst.d $f15, $r12, 15 * 8",
        "fst.d $f16, $r12, 16 * 8",   "fst.d $f17, $r12, 17 * 8",
        "fst.d $f18, $r12, 18 * 8",   "fst.d $f19, $r12, 19 * 8",
        "fst.d $f20, $r12, 20 * 8",   "fst.d $f21, $r12, 21 * 8",
        "fst.d $f22, $r12, 22 * 8",   "fst.d $f23, $r12, 23 * 8",
        "fst.d $f24, $r12, 24 * 8",   "fst.d $f25, $r12, 25 * 8",
        "fst.d $f26, $r12, 26 * 8",   "fst.d $f27, $r12, 27 * 8",
        "fst.d $f28, $r12, 28 * 8",   "fst.d $f29, $r12, 29 * 8",
        "fst.d $f30, $r12, 30 * 8",   "fst.d $f31, $r12, 31 * 8",

        "j .L_fpu_save_done",

        ".L_no_fpu_save:",
        "st.d $r12, $r3, {euen_offset}",

        ".L_fpu_save_done:",

        // 将参数按照 ABI 规范准备好后调用异常报告函数，将异常信息传递给 Rust 代
        // 码进行处理。
        "ld.d $r4, $r3, {pc_offset}",
        "csrrd $r5, {csr_estat}",
        "csrrd $r6, {csr_badv}",
        "ld.d $r7, $r3, {sp_offset}",
        "or $r8, $r3, $r0",
        "la.abs $r12, {report}",
        "jirl $r1, $r12, 0",

        // 根据 Rust 代码的返回值决定是继续执行还是宕机。
        "beqz $r4, .L_halt",

        // 开始恢复寄存器状态，准备继续执行用户态程序。
        "or $r31, $r4, $r0",

        // 恢复程序计数器和状态寄存器的值。
        "ld.d $r12, $r31, {status_offset}",
        "csrwr $r12, {csr_prmd}",
        "ld.d $r12, $r31, {pc_offset}",
        "csrwr $r12, {csr_era}",

        // 检查 FPU_SAVED 标志决定是否恢复 FPU。
        // 不能用 FPE 位 − handler 在 FPD 路径把它从 0 改成了 1。
        "ld.d $r12, $r31, {euen_offset}",
        "andi $r13, $r12, {fpu_saved}",
        "beqz $r13, .L_skip_fpu_restore",

        // 在执行任何浮点恢复指令前，临时开启当前 CPU 的 FPE 使能位。
        "csrrd $r14, {csr_euen}",
        "ori $r15, $r14, {euen_fpe}",
        "csrwr $r15, {csr_euen}",

        // 按照之前的打包规范恢复 F0...F31 浮点寄存器的值。
        "addi.d $r12, $r31, {f_offset}",
        "fld.d $f0, $r12, 0 * 8",     "fld.d $f1, $r12, 1 * 8",
        "fld.d $f2, $r12, 2 * 8",     "fld.d $f3, $r12, 3 * 8",
        "fld.d $f4, $r12, 4 * 8",     "fld.d $f5, $r12, 5 * 8",
        "fld.d $f6, $r12, 6 * 8",     "fld.d $f7, $r12, 7 * 8",
        "fld.d $f8, $r12, 8 * 8",     "fld.d $f9, $r12, 9 * 8",
        "fld.d $f10, $r12, 10 * 8",   "fld.d $f11, $r12, 11 * 8",
        "fld.d $f12, $r12, 12 * 8",   "fld.d $f13, $r12, 13 * 8",
        "fld.d $f14, $r12, 14 * 8",   "fld.d $f15, $r12, 15 * 8",
        "fld.d $f16, $r12, 16 * 8",   "fld.d $f17, $r12, 17 * 8",
        "fld.d $f18, $r12, 18 * 8",   "fld.d $f19, $r12, 19 * 8",
        "fld.d $f20, $r12, 20 * 8",   "fld.d $f21, $r12, 21 * 8",
        "fld.d $f22, $r12, 22 * 8",   "fld.d $f23, $r12, 23 * 8",
        "fld.d $f24, $r12, 24 * 8",   "fld.d $f25, $r12, 25 * 8",
        "fld.d $f26, $r12, 26 * 8",   "fld.d $f27, $r12, 27 * 8",
        "fld.d $f28, $r12, 28 * 8",   "fld.d $f29, $r12, 29 * 8",
        "fld.d $f30, $r12, 30 * 8",   "fld.d $f31, $r12, 31 * 8",

        // 恢复 FCSR 寄存器的值。
        "ld.d $r12, $r31, {fcsr_offset}",
        "movgr2fcsr $fcsr0, $r12",

        // 按照之前的打包规范恢复 FCC0..FCC7 寄存器的值。
        "ld.d $r13, $r31, {fcc_offset}",
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

        ".L_skip_fpu_restore:",

        // 恢复 EUEN。先清除内部 FPU_SAVED 标志再写硬件。
        "ld.d $r12, $r31, {euen_offset}",
        "bstrins.d $r12, $r0, 4, 4",
        "csrwr $r12, {csr_euen}",

        // 恢复 LLBit 相关控制位。
        "ld.d $r12, $r31, {llbctl_offset}",
        "csrwr $r12, {csr_llbctl}",

        // 恢复状态寄存器的值到 CSR_PRMD 中。
        "ld.d $r12, $r31, {status_offset}",
        "andi $r12, $r12, {prmd_pplv_mask}",
        "beqz $r12, .L_restore_gprs",

        // 当前实现采用单地址空间模型（返回用户态前无需额外切换页表上下文）。
        // 若未来引入独立用户地址空间，可在此处补充返回用户态前的地址空间切换。

        ".L_restore_gprs:",

        // 恢复通用寄存器 $r1..$r31 的值。
        "ld.d $r1, $r31, {ra_offset}",

        "ld.d $r2, $r31, {tp_offset}",

        "ld.d $r4, $r31, {a0_offset}",   "ld.d $r5, $r31, {a1_offset}",
        "ld.d $r6, $r31, {a2_offset}",   "ld.d $r7, $r31, {a3_offset}",
        "ld.d $r8, $r31, {a4_offset}",   "ld.d $r9, $r31, {a5_offset}",
        "ld.d $r10, $r31, {a6_offset}",  "ld.d $r11, $r31, {a7_offset}",

        "ld.d $r12, $r31, {t0_offset}",  "ld.d $r13, $r31, {t1_offset}",
        "ld.d $r14, $r31, {t2_offset}",  "ld.d $r15, $r31, {t3_offset}",
        "ld.d $r16, $r31, {t4_offset}",  "ld.d $r17, $r31, {t5_offset}",
        "ld.d $r18, $r31, {t6_offset}",  "ld.d $r19, $r31, {t7_offset}",
        "ld.d $r20, $r31, {t8_offset}",

        "ld.d $r21, $r31, {rx_offset}",  "ld.d $r22, $r31, {s0_offset}",
        "ld.d $r23, $r31, {s1_offset}",  "ld.d $r24, $r31, {s2_offset}",
        "ld.d $r25, $r31, {s3_offset}",  "ld.d $r26, $r31, {s4_offset}",
        "ld.d $r27, $r31, {s5_offset}",  "ld.d $r28, $r31, {s6_offset}",
        "ld.d $r29, $r31, {s7_offset}",  "ld.d $r30, $r31, {s8_offset}",

        "ld.d $r3, $r31, {sp_offset}",
        "ld.d $r31, $r31, {s9_offset}",

        // 返回。
        "ertn",

        // 正常情况下不会执行到这里。如果异常报告函数返回了 0，说明需要宕机，
        // 进入下面的死循环。
        ".L_halt:",
        "idle 0",
        "b .L_halt",

        report = sym loongarch64_handle_exception,
        frame_size = const FRAME_SIZE,
        csr_prmd = const CSR_PRMD,
        csr_euen = const CSR_EUEN,
        csr_llbctl = const CSR_LLBCTL,
        csr_estat = const CSR_ESTAT,
        csr_era = const CSR_ERA,
        csr_badv = const CSR_BADV,
        csr_ks0 = const CSR_KS0,
        csr_ks1 = const CSR_KS1,
        prmd_pplv_mask = const CSR_PRMD_PPLV_MASK,
        euen_fpe = const EUEN_FPE,
        fpu_saved = const FPU_SAVED,
        euen_unsupported_context_mask = const EUEN_BTE,

        ra_offset = const RA_OFFSET,
        tp_offset = const TP_OFFSET,
        sp_offset = const SP_OFFSET,
        a0_offset = const A0_OFFSET,
        a1_offset = const A1_OFFSET,
        a2_offset = const A2_OFFSET,
        a3_offset = const A3_OFFSET,
        a4_offset = const A4_OFFSET,
        a5_offset = const A5_OFFSET,
        a6_offset = const A6_OFFSET,
        a7_offset = const A7_OFFSET,
        t0_offset = const T0_OFFSET,
        t1_offset = const T1_OFFSET,
        t2_offset = const T2_OFFSET,
        t3_offset = const T3_OFFSET,
        t4_offset = const T4_OFFSET,
        t5_offset = const T5_OFFSET,
        t6_offset = const T6_OFFSET,
        t7_offset = const T7_OFFSET,
        t8_offset = const T8_OFFSET,
        rx_offset = const RX_OFFSET,
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
        pc_offset = const PC_OFFSET,
        status_offset = const STATUS_OFFSET,
        euen_offset = const EUEN_OFFSET,
        llbctl_offset = const LLBCTL_OFFSET,
        fcsr_offset = const FCSR_OFFSET,
        fcc_offset = const FCC_OFFSET,
        f_offset = const F_OFFSET,
    )
}
