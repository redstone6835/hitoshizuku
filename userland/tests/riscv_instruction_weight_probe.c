#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#if !defined(__riscv) || __riscv_xlen != 64
#error "riscv_instruction_weight_probe only supports RISC-V64"
#endif

/*
 * 每次函数调用执行 1024 个目标槽。调用循环本身会被 QEMU 插件精确计数，
 * 统计端使用同形态 baseline 和 empty-call 标定链消除这部分公共开销。
 */
#define RV_SLOT_COUNT 1024
#define STRINGIFY_INNER(value) #value
#define STRINGIFY(value) STRINGIFY_INNER(value)

typedef void (*kernel_fn_t)(uintptr_t arg0, uintptr_t arg1);

#define DEFINE_KERNEL(name, option, body)                                      \
    __asm__(                                                                    \
        ".pushsection .text.riscv_weight_kernels,\"ax\",@progbits\n"        \
        ".balign 64\n"                                                        \
        ".globl " #name "\n"                                                 \
        ".type " #name ", @function\n"                                       \
        #name ":\n"                                                           \
        ".option push\n"                                                      \
        ".option norelax\n"                                                   \
        option "\n"                                                           \
        ".rept " STRINGIFY(RV_SLOT_COUNT) "\n"                               \
        body "\n"                                                             \
        ".endr\n"                                                             \
        ".option norvc\n"                                                     \
        "jalr zero, 0(ra)\n"                                                  \
        ".option pop\n"                                                       \
        ".size " #name ", .-" #name "\n"                                    \
        ".popsection\n");                                                     \
    extern void name(uintptr_t, uintptr_t)

#define DEFINE_RV64_KERNEL(name, body) DEFINE_KERNEL(name, ".option norvc", body)
#define DEFINE_RVC_KERNEL(name, body) DEFINE_KERNEL(name, ".option rvc", body)

#define DEFINE_RV64_CONTEXT_KERNEL(name, setup, body)                           \
    __asm__(                                                                    \
        ".pushsection .text.riscv_weight_kernels,\"ax\",@progbits\n"        \
        ".balign 64\n"                                                        \
        ".globl " #name "\n"                                                 \
        ".type " #name ", @function\n"                                       \
        #name ":\n"                                                           \
        ".option push\n"                                                      \
        ".option norelax\n"                                                   \
        ".option norvc\n"                                                     \
        setup "\n"                                                            \
        ".rept " STRINGIFY(RV_SLOT_COUNT) "\n"                               \
        body "\n"                                                             \
        ".endr\n"                                                             \
        "jalr zero, 0(ra)\n"                                                  \
        ".option pop\n"                                                       \
        ".size " #name ", .-" #name "\n"                                    \
        ".popsection\n");                                                     \
    extern void name(uintptr_t, uintptr_t)

#define DEFINE_STACK_RVC_KERNEL(name, body)                                    \
    __asm__(                                                                    \
        ".pushsection .text.riscv_weight_kernels,\"ax\",@progbits\n"        \
        ".balign 64\n"                                                        \
        ".globl " #name "\n"                                                 \
        ".type " #name ", @function\n"                                       \
        #name ":\n"                                                           \
        ".option push\n"                                                      \
        ".option norelax\n"                                                   \
        ".option norvc\n"                                                     \
        "addi t0, sp, 0\n"                                                    \
        "addi sp, a0, 0\n"                                                    \
        ".option rvc\n"                                                       \
        ".rept " STRINGIFY(RV_SLOT_COUNT) "\n"                               \
        body "\n"                                                             \
        ".endr\n"                                                             \
        ".option norvc\n"                                                     \
        "addi sp, t0, 0\n"                                                    \
        "jalr zero, 0(ra)\n"                                                  \
        ".option pop\n"                                                       \
        ".size " #name ", .-" #name "\n"                                    \
        ".popsection\n");                                                     \
    extern void name(uintptr_t, uintptr_t)

#define DEFINE_RVC_LINK_KERNEL(name, body)                                     \
    __asm__(                                                                    \
        ".pushsection .text.riscv_weight_kernels,\"ax\",@progbits\n"        \
        ".balign 64\n"                                                        \
        ".globl " #name "\n"                                                 \
        ".type " #name ", @function\n"                                       \
        #name ":\n"                                                           \
        ".option push\n"                                                      \
        ".option norelax\n"                                                   \
        ".option norvc\n"                                                     \
        "addi t1, ra, 0\n"                                                    \
        ".rept " STRINGIFY(RV_SLOT_COUNT) "\n"                               \
        body "\n"                                                             \
        ".endr\n"                                                             \
        ".option norvc\n"                                                     \
        "addi ra, t1, 0\n"                                                    \
        "jalr zero, 0(ra)\n"                                                  \
        ".option pop\n"                                                       \
        ".size " #name ", .-" #name "\n"                                    \
        ".popsection\n");                                                     \
    extern void name(uintptr_t, uintptr_t)

__asm__(
    ".pushsection .text.riscv_weight_kernels,\"ax\",@progbits\n"
    ".balign 64\n"
    ".globl rv_kernel_empty\n"
    ".type rv_kernel_empty, @function\n"
    "rv_kernel_empty:\n"
    ".option push\n"
    ".option norvc\n"
    "jalr zero, 0(ra)\n"
    ".option pop\n"
    ".size rv_kernel_empty, .-rv_kernel_empty\n"
    ".popsection\n");
extern void rv_kernel_empty(uintptr_t, uintptr_t);

__asm__(
    ".pushsection .text.riscv_weight_kernels,\"ax\",@progbits\n"
    ".balign 64\n"
    ".globl rv_kernel_fp_setup_d\n"
    ".type rv_kernel_fp_setup_d, @function\n"
    "rv_kernel_fp_setup_d:\n"
    ".option push\n"
    ".option norvc\n"
    "csrrw zero, 0x003, zero\n"
    "fld ft0, 0(a0)\n"
    "fld ft1, 8(a0)\n"
    "jalr zero, 0(ra)\n"
    ".option pop\n"
    ".size rv_kernel_fp_setup_d, .-rv_kernel_fp_setup_d\n"
    ".balign 64\n"
    ".globl rv_kernel_fp_setup_s\n"
    ".type rv_kernel_fp_setup_s, @function\n"
    "rv_kernel_fp_setup_s:\n"
    ".option push\n"
    ".option norvc\n"
    "csrrw zero, 0x003, zero\n"
    "flw ft0, 0(a0)\n"
    "flw ft1, 8(a0)\n"
    "jalr zero, 0(ra)\n"
    ".option pop\n"
    ".size rv_kernel_fp_setup_s, .-rv_kernel_fp_setup_s\n"
    ".balign 64\n"
    ".globl rv_kernel_fp_reload_d\n"
    ".type rv_kernel_fp_reload_d, @function\n"
    "rv_kernel_fp_reload_d:\n"
    ".option push\n"
    ".option norvc\n"
    "fld ft0, 0(a0)\n"
    "fld ft1, 8(a0)\n"
    "jalr zero, 0(ra)\n"
    ".option pop\n"
    ".size rv_kernel_fp_reload_d, .-rv_kernel_fp_reload_d\n"
    ".balign 64\n"
    ".globl rv_kernel_fp_reload_s\n"
    ".type rv_kernel_fp_reload_s, @function\n"
    "rv_kernel_fp_reload_s:\n"
    ".option push\n"
    ".option norvc\n"
    "flw ft0, 0(a0)\n"
    "flw ft1, 8(a0)\n"
    "jalr zero, 0(ra)\n"
    ".option pop\n"
    ".size rv_kernel_fp_reload_s, .-rv_kernel_fp_reload_s\n"
    ".popsection\n");
extern void rv_kernel_fp_setup_d(uintptr_t, uintptr_t);
extern void rv_kernel_fp_setup_s(uintptr_t, uintptr_t);
extern void rv_kernel_fp_reload_d(uintptr_t, uintptr_t);
extern void rv_kernel_fp_reload_s(uintptr_t, uintptr_t);

DEFINE_RV64_KERNEL(rv_kernel_nop4, "addi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_addi, "addi a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_mv4, "addi a0, a1, 0");
DEFINE_RV64_KERNEL(rv_kernel_li4, "addi a0, zero, 1");
DEFINE_RV64_KERNEL(rv_kernel_addiw, "addiw a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_sext_w4, "addiw a0, a0, 0");
DEFINE_RV64_KERNEL(rv_kernel_slti, "slti a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_sltiu, "sltiu a0, a0, 2");
DEFINE_RV64_KERNEL(rv_kernel_xori, "xori a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_not4, "xori a0, a0, -1");
DEFINE_RV64_KERNEL(rv_kernel_ori, "ori a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_andi, "andi a0, a0, 255");
DEFINE_RV64_KERNEL(rv_kernel_slli, "slli a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_srli, "srli a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_srai, "srai a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_slliw, "slliw a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_srliw, "srliw a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_sraiw, "sraiw a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_lui, "lui t0, 1");
DEFINE_RV64_KERNEL(rv_kernel_auipc, "auipc t0, 0");

DEFINE_RV64_KERNEL(rv_kernel_add, "add a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_sub, "sub a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_neg4, "sub a0, zero, a1");
DEFINE_RV64_KERNEL(rv_kernel_sll, "sll a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_slt, "slt a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_sgtz4, "slt a0, zero, a1");
DEFINE_RV64_KERNEL(rv_kernel_sltu, "sltu a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_snez4, "sltu a0, zero, a1");
DEFINE_RV64_KERNEL(rv_kernel_seqz4, "sltiu a0, a0, 1");
DEFINE_RV64_KERNEL(rv_kernel_xor, "xor a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_srl, "srl a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_sra, "sra a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_or, "or a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_and, "and a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_addw, "addw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_subw, "subw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_negw4, "subw a0, zero, a1");
DEFINE_RV64_KERNEL(rv_kernel_sllw, "sllw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_srlw, "srlw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_sraw, "sraw a0, a0, a1");

DEFINE_RV64_KERNEL(rv_kernel_mul, "mul a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_mulh, "mulh a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_mulhsu, "mulhsu a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_mulhu, "mulhu a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_div, "div a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_divu, "divu a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_rem, "rem a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_remu, "remu a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_mulw, "mulw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_divw, "divw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_divuw, "divuw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_remw, "remw a0, a0, a1");
DEFINE_RV64_KERNEL(rv_kernel_remuw, "remuw a0, a0, a1");

/*
 * 差分套件在每个目标槽前从 t2 恢复被除数。匹配 baseline 保留恢复指令，
 * 只用 nop 替换目标操作，使精确指令差仍闭合为 RV_SLOT_COUNT。
 */
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_nop4, "addi t2, a0, 0",
                           "addi a0, t2, 0\naddi zero, zero, 0");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_div, "addi t2, a0, 0",
                           "addi a0, t2, 0\ndiv a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_divu, "addi t2, a0, 0",
                           "addi a0, t2, 0\ndivu a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_rem, "addi t2, a0, 0",
                           "addi a0, t2, 0\nrem a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_remu, "addi t2, a0, 0",
                           "addi a0, t2, 0\nremu a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_divw, "addi t2, a0, 0",
                           "addi a0, t2, 0\ndivw a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_divuw, "addi t2, a0, 0",
                           "addi a0, t2, 0\ndivuw a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_remw, "addi t2, a0, 0",
                           "addi a0, t2, 0\nremw a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(rv_kernel_reset_remuw, "addi t2, a0, 0",
                           "addi a0, t2, 0\nremuw a0, a0, a1");

/*
 * 交替上下文为目标和相邻 M 操作分别恢复输入；baseline 仅替换被测操作。
 * 齐次与交替版本都在每个槽恢复目标输入，因此对照集中反映混合 TB 邻域。
 */
DEFINE_RV64_CONTEXT_KERNEL(
    rv_kernel_alternating_rem_div, "addi t2, a0, 0",
    "addi t0, t2, 0\nrem t0, t0, a1\naddi a0, t2, 0\ndiv a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(
    rv_kernel_alternating_rem_div_baseline, "addi t2, a0, 0",
    "addi t0, t2, 0\nrem t0, t0, a1\naddi a0, t2, 0\naddi zero, zero, 0");
DEFINE_RV64_CONTEXT_KERNEL(
    rv_kernel_alternating_div_rem, "addi t2, a0, 0",
    "addi t0, t2, 0\ndiv t0, t0, a1\naddi a0, t2, 0\nrem a0, a0, a1");
DEFINE_RV64_CONTEXT_KERNEL(
    rv_kernel_alternating_div_rem_baseline, "addi t2, a0, 0",
    "addi t0, t2, 0\ndiv t0, t0, a1\naddi a0, t2, 0\naddi zero, zero, 0");

DEFINE_RV64_KERNEL(rv_kernel_lb, "lb t0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_lbu, "lbu t0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_lh, "lh t0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_lhu, "lhu t0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_lw, "lw t0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_lwu, "lwu t0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_ld, "ld t0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_sb, "sb a1, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_sh, "sh a1, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_sw, "sw a1, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_sd, "sd a1, 0(a0)");

/*
 * 分支目标越过一个真实指令槽，因此 taken target 与 fallthrough 不相同。
 * not-taken baseline 保留同一个动态填充槽，只把目标分支替换成 nop。
 */
DEFINE_RV64_KERNEL(rv_kernel_beq, "beq a0, a1, 1f\naddi zero, zero, 0\n1:");
DEFINE_RV64_KERNEL(rv_kernel_bne, "bne a0, a1, 1f\naddi zero, zero, 0\n1:");
DEFINE_RV64_KERNEL(rv_kernel_blt, "blt a0, a1, 1f\naddi zero, zero, 0\n1:");
DEFINE_RV64_KERNEL(rv_kernel_bge, "bge a0, a1, 1f\naddi zero, zero, 0\n1:");
DEFINE_RV64_KERNEL(rv_kernel_bltu, "bltu a0, a1, 1f\naddi zero, zero, 0\n1:");
DEFINE_RV64_KERNEL(rv_kernel_bgeu, "bgeu a0, a1, 1f\naddi zero, zero, 0\n1:");
DEFINE_RV64_KERNEL(rv_kernel_branch_not_taken_baseline,
                   "addi zero, zero, 0\naddi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_j, "jal zero, 1f\naddi zero, zero, 0\n1:");
DEFINE_RVC_LINK_KERNEL(rv_kernel_jal_link,
                       "jal ra, 1f\naddi zero, zero, 0\n1:");
DEFINE_RVC_LINK_KERNEL(rv_kernel_jal_link_baseline,
                       "addi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_jalr,
                   "auipc t0, 0\njalr zero, 12(t0)\naddi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_jalr_baseline,
                   "auipc t0, 0\naddi zero, zero, 0");
DEFINE_RVC_LINK_KERNEL(rv_kernel_jalr_link,
                       "auipc t0, 0\njalr ra, 12(t0)\naddi zero, zero, 0");
DEFINE_RVC_LINK_KERNEL(rv_kernel_jalr_link_baseline,
                       "auipc t0, 0\naddi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_jalr_general_link,
                   "auipc t0, 0\njalr t2, 12(t0)\naddi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_jalr_general_link_baseline,
                   "auipc t0, 0\naddi zero, zero, 0");
DEFINE_RVC_LINK_KERNEL(rv_kernel_ret4,
                       "auipc ra, 0\naddi ra, ra, 16\njalr zero, 0(ra)\naddi zero, zero, 0");
DEFINE_RVC_LINK_KERNEL(rv_kernel_ret4_baseline,
                       "auipc ra, 0\naddi ra, ra, 12\naddi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_fence, "fence rw, rw");
DEFINE_RV64_KERNEL(rv_kernel_fence_11, ".4byte 0x0110000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_14, ".4byte 0x0140000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_22, ".4byte 0x0220000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_23, ".4byte 0x0230000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_31, ".4byte 0x0310000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_55, ".4byte 0x0550000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_82, ".4byte 0x0820000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_aa, ".4byte 0x0aa0000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_f5, ".4byte 0x0f50000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_ff, ".4byte 0x0ff0000f");
DEFINE_RV64_KERNEL(rv_kernel_fence_i, "fence.i");
DEFINE_RV64_KERNEL(rv_kernel_pause, ".4byte 0x0100000f");
DEFINE_RV64_KERNEL(rv_kernel_cbo_zero, ".insn i 0x0f, 2, zero, a0, 4");

DEFINE_RV64_KERNEL(rv_kernel_flw, "flw ft0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_fld, "fld ft0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_fsw, "fsw ft0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_fsd, "fsd ft0, 0(a0)");
DEFINE_RV64_KERNEL(rv_kernel_fadd_d, "fadd.d ft0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fsub_d, "fsub.d ft0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fmul_d, "fmul.d ft0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fdiv_d, "fdiv.d ft0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fdiv_s, "fdiv.s ft0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_feq_d, "feq.d a0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_flt_d, "flt.d a0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fle_d, "fle.d a0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fclass_d, "fclass.d a0, ft0");
DEFINE_RV64_KERNEL(rv_kernel_fsgnj_d, "fsgnj.d ft0, ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fmv_d_x, "fmv.d.x ft0, a0");
DEFINE_RV64_KERNEL(rv_kernel_fmv_w_x, "fmv.w.x ft0, a0");
DEFINE_RV64_KERNEL(rv_kernel_fmv_x_d, "fmv.x.d a0, ft0");
DEFINE_RV64_KERNEL(rv_kernel_fmv_x_w, "fmv.x.w a0, ft0");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_d_l, "fcvt.d.l ft0, a0");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_d_lu, "fcvt.d.lu ft0, a0");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_d_w, ".4byte 0xd2050053");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_l_d, "fcvt.l.d a0, ft0, rtz");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_lu_d, "fcvt.lu.d a0, ft0, rtz");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_s_d, "fcvt.s.d ft0, ft1");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_s_lu, "fcvt.s.lu ft0, a0");
DEFINE_RV64_KERNEL(rv_kernel_fcvt_w_d, "fcvt.w.d a0, ft0, rtz");

DEFINE_RV64_KERNEL(rv_kernel_csrrs_time, "csrrs a0, 0xc01, zero");
DEFINE_RV64_KERNEL(rv_kernel_csrrs_fflags, "csrrs a0, 0x001, zero");
DEFINE_RV64_KERNEL(rv_kernel_csrrs_frm, "csrrs a0, 0x002, zero");
DEFINE_RV64_KERNEL(rv_kernel_csrrs_fcsr, "csrrs a0, 0x003, zero");
DEFINE_RV64_KERNEL(rv_kernel_csrrw_fcsr, "csrrw zero, 0x003, zero");
DEFINE_RV64_KERNEL(rv_kernel_csrrwi_fcsr, "csrrwi zero, 0x003, 0");

DEFINE_RV64_KERNEL(rv_kernel_lr_w, "lr.w t0, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_lr_w_aq, "lr.w.aq t0, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_lr_w_aqrl, "lr.w.aqrl t0, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_lr_d, "lr.d t0, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_lr_d_aq, "lr.d.aq t0, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_lr_d_aqrl, "lr.d.aqrl t0, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_w, "amoadd.w zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_w_aq, "amoadd.w.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_w_rl, "amoadd.w.rl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_w_aqrl, "amoadd.w.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_d, "amoadd.d zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_d_aq, "amoadd.d.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_d_rl, "amoadd.d.rl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoadd_d_aqrl, "amoadd.d.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoswap_w, "amoswap.w zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoswap_w_aq, "amoswap.w.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoswap_w_rl, "amoswap.w.rl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoswap_w_aqrl, "amoswap.w.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoswap_d, "amoswap.d zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoswap_d_aq, "amoswap.d.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoswap_d_aqrl, "amoswap.d.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoand_w, "amoand.w zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoand_w_aqrl, "amoand.w.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoand_d, "amoand.d zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoand_d_rl, "amoand.d.rl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoand_d_aqrl, "amoand.d.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_w, "amoor.w zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_w_aq, "amoor.w.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_w_rl, "amoor.w.rl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_w_aqrl, "amoor.w.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_d, "amoor.d zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_d_aq, "amoor.d.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_d_rl, "amoor.d.rl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoor_d_aqrl, "amoor.d.aqrl zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoxor_w, "amoxor.w zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amoxor_d, "amoxor.d zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amomaxu_w_aq, "amomaxu.w.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amomaxu_d_aq, "amomaxu.d.aq zero, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_amomaxu_d_aqrl, "amomaxu.d.aqrl zero, a1, (a0)");
/* sc 需要有效 reservation；精确 mix 会把配对的 lr 作为 nuisance 保留下来。 */
DEFINE_RV64_KERNEL(rv_kernel_sc_w, "lr.w t0, (a0)\nsc.w t1, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_sc_w_aq, "lr.w t0, (a0)\nsc.w.aq t1, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_sc_w_rl, "lr.w t0, (a0)\nsc.w.rl t1, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_sc_w_aqrl, "lr.w t0, (a0)\nsc.w.aqrl t1, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_sc_d, "lr.d t0, (a0)\nsc.d t1, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_sc_d_aq, "lr.d t0, (a0)\nsc.d.aq t1, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_sc_d_rl, "lr.d t0, (a0)\nsc.d.rl t1, a1, (a0)");
DEFINE_RV64_KERNEL(rv_kernel_sc_baseline_w,
                   "lr.w t0, (a0)\naddi zero, zero, 0");
DEFINE_RV64_KERNEL(rv_kernel_sc_baseline_d,
                   "lr.d t0, (a0)\naddi zero, zero, 0");

DEFINE_RVC_KERNEL(rv_kernel_nop2, "c.nop");
DEFINE_RVC_KERNEL(rv_kernel_c_addi, "c.addi a0, 1");
DEFINE_RVC_KERNEL(rv_kernel_c_addiw, "c.addiw a0, 1");
DEFINE_RVC_KERNEL(rv_kernel_c_sext_w, "c.addiw a0, 0");
DEFINE_RVC_KERNEL(rv_kernel_c_li, "c.li a2, 1");
DEFINE_RVC_KERNEL(rv_kernel_c_lui, "c.lui a2, 1");
DEFINE_RVC_KERNEL(rv_kernel_c_slli, "c.slli a0, 1");
DEFINE_RVC_KERNEL(rv_kernel_c_srli, "c.srli a0, 1");
DEFINE_RVC_KERNEL(rv_kernel_c_srai, "c.srai a0, 1");
DEFINE_RVC_KERNEL(rv_kernel_c_andi, "c.andi a0, 7");
DEFINE_RVC_KERNEL(rv_kernel_c_add, "c.add a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_mv, "c.mv a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_and, "c.and a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_or, "c.or a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_xor, "c.xor a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_sub, "c.sub a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_addw, "c.addw a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_subw, "c.subw a0, a1");
DEFINE_RVC_KERNEL(rv_kernel_c_lw, "c.lw a2, 0(a0)");
DEFINE_RVC_KERNEL(rv_kernel_c_ld, "c.ld a2, 0(a0)");
DEFINE_RVC_KERNEL(rv_kernel_c_sw, "c.sw a1, 0(a0)");
DEFINE_RVC_KERNEL(rv_kernel_c_sd, "c.sd a1, 0(a0)");
DEFINE_RVC_KERNEL(rv_kernel_c_beqz, "c.beqz a0, 1f\nc.nop\n1:");
DEFINE_RVC_KERNEL(rv_kernel_c_bnez, "c.bnez a0, 1f\nc.nop\n1:");
DEFINE_RVC_KERNEL(rv_kernel_c_branch_not_taken_baseline, "c.nop\nc.nop");
DEFINE_RVC_KERNEL(rv_kernel_c_j, "c.j 1f\nc.nop\n1:");
DEFINE_RVC_KERNEL(rv_kernel_c_jr,
                  ".option norvc\nauipc t0, 0\naddi t0, t0, 12\n.option rvc\nc.jr t0\nc.nop");
DEFINE_RVC_KERNEL(rv_kernel_c_jr_baseline,
                  ".option norvc\nauipc t0, 0\naddi t0, t0, 10\n.option rvc\nc.nop");
DEFINE_RVC_LINK_KERNEL(
    rv_kernel_c_jalr,
    "auipc t0, 0\naddi t0, t0, 12\n.option rvc\nc.jalr t0\nc.nop\n.option norvc");
DEFINE_RVC_LINK_KERNEL(
    rv_kernel_c_jalr_baseline,
    "auipc t0, 0\naddi t0, t0, 10\n.option rvc\nc.nop\n.option norvc");
DEFINE_RVC_LINK_KERNEL(
    rv_kernel_c_ret,
    "auipc ra, 0\naddi ra, ra, 12\n.option rvc\nc.jr ra\nc.nop\n.option norvc");
DEFINE_RVC_LINK_KERNEL(
    rv_kernel_c_ret_baseline,
    "auipc ra, 0\naddi ra, ra, 10\n.option rvc\nc.nop\n.option norvc");
DEFINE_RVC_KERNEL(rv_kernel_c_addi4spn, "c.addi4spn a2, sp, 16");
DEFINE_RVC_KERNEL(rv_kernel_c_fld, "c.fld fa0, 0(a0)");
DEFINE_RVC_KERNEL(rv_kernel_c_fsd, "c.fsd fa0, 0(a0)");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_addi16sp, "c.addi16sp sp, 16");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_lwsp, "c.lwsp a2, 0(sp)");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_ldsp, "c.ldsp a2, 0(sp)");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_swsp, "c.swsp a1, 0(sp)");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_sdsp, "c.sdsp a1, 0(sp)");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_fldsp, "c.fldsp fa0, 0(sp)");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_fsdsp, "c.fsdsp fa0, 0(sp)");
DEFINE_STACK_RVC_KERNEL(rv_kernel_c_stack_nop, "c.nop");

/* QEMU 以这两个唯一符号的 PC 为每个动态 segment 开关窗口。 */
__attribute__((noinline, used, externally_visible, aligned(64)))
void riscv_weight_profile_start(void)
{
    __asm__ volatile("" : : : "memory");
}

__attribute__((noinline, used, externally_visible, aligned(64)))
void riscv_weight_profile_stop(void)
{
    __asm__ volatile("" : : : "memory");
}

enum argument_kind {
    ARG_ARITHMETIC,
    ARG_DIV_NONDEGENERATE,
    ARG_MEMORY,
    ARG_EQUAL,
    ARG_NOT_EQUAL,
    ARG_SIGNED_LESS,
    ARG_UNSIGNED_LESS,
    ARG_ZERO,
    ARG_NONZERO,
};

struct instruction_case {
    const char *instruction;
    unsigned int encoding_bytes;
    const char *pattern;
    kernel_fn_t probe;
    kernel_fn_t baseline;
    enum argument_kind argument_kind;
};

#define CASE4(name, pattern, function, arguments)                               \
    {name, 4, pattern, function, rv_kernel_nop4, arguments}
#define CASE2(name, pattern, function, arguments)                               \
    {name, 2, pattern, function, rv_kernel_nop2, arguments}
#define CASE4_WITH_BASE(name, pattern, function, base, arguments)               \
    {name, 4, pattern, function, base, arguments}
#define CASE2_WITH_BASE(name, pattern, function, base, arguments)               \
    {name, 2, pattern, function, base, arguments}

static const struct instruction_case instruction_cases[] = {
    {"nop", 4, "independent", rv_kernel_nop4, rv_kernel_empty, ARG_ARITHMETIC},
    CASE4("addi", "dependency", rv_kernel_addi, ARG_ARITHMETIC),
    CASE4("mv", "register-move", rv_kernel_mv4, ARG_ARITHMETIC),
    CASE4("li", "immediate-load", rv_kernel_li4, ARG_ARITHMETIC),
    CASE4("addiw", "dependency", rv_kernel_addiw, ARG_ARITHMETIC),
    CASE4("sext.w", "dependency", rv_kernel_sext_w4, ARG_ARITHMETIC),
    CASE4("slti", "dependency", rv_kernel_slti, ARG_ARITHMETIC),
    CASE4("sltiu", "dependency", rv_kernel_sltiu, ARG_ARITHMETIC),
    CASE4("xori", "dependency", rv_kernel_xori, ARG_ARITHMETIC),
    CASE4("not", "dependency", rv_kernel_not4, ARG_ARITHMETIC),
    CASE4("ori", "dependency", rv_kernel_ori, ARG_ARITHMETIC),
    CASE4("andi", "dependency", rv_kernel_andi, ARG_ARITHMETIC),
    CASE4("slli", "dependency", rv_kernel_slli, ARG_ARITHMETIC),
    CASE4("srli", "dependency", rv_kernel_srli, ARG_ARITHMETIC),
    CASE4("srai", "dependency", rv_kernel_srai, ARG_ARITHMETIC),
    CASE4("slliw", "dependency", rv_kernel_slliw, ARG_ARITHMETIC),
    CASE4("srliw", "dependency", rv_kernel_srliw, ARG_ARITHMETIC),
    CASE4("sraiw", "dependency", rv_kernel_sraiw, ARG_ARITHMETIC),
    CASE4("lui", "independent", rv_kernel_lui, ARG_ARITHMETIC),
    CASE4("auipc", "independent", rv_kernel_auipc, ARG_ARITHMETIC),
    CASE4("add", "dependency", rv_kernel_add, ARG_ARITHMETIC),
    CASE4("sub", "dependency", rv_kernel_sub, ARG_ARITHMETIC),
    CASE4("neg", "dependency", rv_kernel_neg4, ARG_ARITHMETIC),
    CASE4("sll", "dependency", rv_kernel_sll, ARG_ARITHMETIC),
    CASE4("slt", "dependency", rv_kernel_slt, ARG_ARITHMETIC),
    CASE4("sgtz", "dependency", rv_kernel_sgtz4, ARG_ARITHMETIC),
    CASE4("sltu", "dependency", rv_kernel_sltu, ARG_ARITHMETIC),
    CASE4("snez", "dependency", rv_kernel_snez4, ARG_ARITHMETIC),
    CASE4("seqz", "dependency", rv_kernel_seqz4, ARG_ARITHMETIC),
    CASE4("xor", "dependency", rv_kernel_xor, ARG_ARITHMETIC),
    CASE4("srl", "dependency", rv_kernel_srl, ARG_ARITHMETIC),
    CASE4("sra", "dependency", rv_kernel_sra, ARG_ARITHMETIC),
    CASE4("or", "dependency", rv_kernel_or, ARG_ARITHMETIC),
    CASE4("and", "dependency", rv_kernel_and, ARG_ARITHMETIC),
    CASE4("addw", "dependency", rv_kernel_addw, ARG_ARITHMETIC),
    CASE4("subw", "dependency", rv_kernel_subw, ARG_ARITHMETIC),
    CASE4("negw", "dependency", rv_kernel_negw4, ARG_ARITHMETIC),
    CASE4("sllw", "dependency", rv_kernel_sllw, ARG_ARITHMETIC),
    CASE4("srlw", "dependency", rv_kernel_srlw, ARG_ARITHMETIC),
    CASE4("sraw", "dependency", rv_kernel_sraw, ARG_ARITHMETIC),
    CASE4("mul", "dependency", rv_kernel_mul, ARG_ARITHMETIC),
    CASE4("mulh", "dependency", rv_kernel_mulh, ARG_ARITHMETIC),
    CASE4("mulhsu", "dependency", rv_kernel_mulhsu, ARG_ARITHMETIC),
    CASE4("mulhu", "dependency", rv_kernel_mulhu, ARG_ARITHMETIC),
    CASE4("div", "dependency", rv_kernel_div, ARG_ARITHMETIC),
    CASE4("divu", "dependency", rv_kernel_divu, ARG_ARITHMETIC),
    CASE4("rem", "dependency", rv_kernel_rem, ARG_ARITHMETIC),
    CASE4("remu", "dependency", rv_kernel_remu, ARG_ARITHMETIC),
    CASE4("mulw", "dependency", rv_kernel_mulw, ARG_ARITHMETIC),
    CASE4("divw", "dependency", rv_kernel_divw, ARG_ARITHMETIC),
    CASE4("divuw", "dependency", rv_kernel_divuw, ARG_ARITHMETIC),
    CASE4("remw", "dependency", rv_kernel_remw, ARG_ARITHMETIC),
    CASE4("remuw", "dependency", rv_kernel_remuw, ARG_ARITHMETIC),
    CASE4("lb", "hot-load", rv_kernel_lb, ARG_MEMORY),
    CASE4("lbu", "hot-load", rv_kernel_lbu, ARG_MEMORY),
    CASE4("lh", "hot-load", rv_kernel_lh, ARG_MEMORY),
    CASE4("lhu", "hot-load", rv_kernel_lhu, ARG_MEMORY),
    CASE4("lw", "hot-load", rv_kernel_lw, ARG_MEMORY),
    CASE4("lwu", "hot-load", rv_kernel_lwu, ARG_MEMORY),
    CASE4("ld", "hot-load", rv_kernel_ld, ARG_MEMORY),
    CASE4("sb", "hot-store", rv_kernel_sb, ARG_MEMORY),
    CASE4("sh", "hot-store", rv_kernel_sh, ARG_MEMORY),
    CASE4("sw", "hot-store", rv_kernel_sw, ARG_MEMORY),
    CASE4("sd", "hot-store", rv_kernel_sd, ARG_MEMORY),
    CASE4("beq", "taken-branch", rv_kernel_beq, ARG_EQUAL),
    CASE4_WITH_BASE("beq", "not-taken-branch", rv_kernel_beq,
                    rv_kernel_branch_not_taken_baseline, ARG_NOT_EQUAL),
    CASE4("bne", "taken-branch", rv_kernel_bne, ARG_NOT_EQUAL),
    CASE4_WITH_BASE("bne", "not-taken-branch", rv_kernel_bne,
                    rv_kernel_branch_not_taken_baseline, ARG_EQUAL),
    CASE4("blt", "taken-branch", rv_kernel_blt, ARG_SIGNED_LESS),
    CASE4_WITH_BASE("blt", "not-taken-branch", rv_kernel_blt,
                    rv_kernel_branch_not_taken_baseline, ARG_EQUAL),
    CASE4("bge", "taken-branch", rv_kernel_bge, ARG_EQUAL),
    CASE4_WITH_BASE("bge", "not-taken-branch", rv_kernel_bge,
                    rv_kernel_branch_not_taken_baseline, ARG_SIGNED_LESS),
    CASE4("bltu", "taken-branch", rv_kernel_bltu, ARG_UNSIGNED_LESS),
    CASE4_WITH_BASE("bltu", "not-taken-branch", rv_kernel_bltu,
                    rv_kernel_branch_not_taken_baseline, ARG_EQUAL),
    CASE4("bgeu", "taken-branch", rv_kernel_bgeu, ARG_EQUAL),
    CASE4_WITH_BASE("bgeu", "not-taken-branch", rv_kernel_bgeu,
                    rv_kernel_branch_not_taken_baseline, ARG_UNSIGNED_LESS),
    CASE4("j", "direct-jump", rv_kernel_j, ARG_ARITHMETIC),
    CASE4_WITH_BASE("jal", "direct-link", rv_kernel_jal_link,
                    rv_kernel_jal_link_baseline, ARG_ARITHMETIC),
    CASE4_WITH_BASE("jalr", "indirect-jump", rv_kernel_jalr,
                    rv_kernel_jalr_baseline, ARG_ARITHMETIC),
    CASE4_WITH_BASE("jalr", "indirect-link", rv_kernel_jalr_link,
                    rv_kernel_jalr_link_baseline, ARG_ARITHMETIC),
    CASE4_WITH_BASE("jalr", "indirect-general-link", rv_kernel_jalr_general_link,
                    rv_kernel_jalr_general_link_baseline, ARG_ARITHMETIC),
    CASE4_WITH_BASE("ret", "indirect-return", rv_kernel_ret4,
                    rv_kernel_ret4_baseline, ARG_ARITHMETIC),
    CASE4("fence", "serialization", rv_kernel_fence, ARG_ARITHMETIC),
    CASE4("fence", "serialization-11", rv_kernel_fence_11, ARG_ARITHMETIC),
    CASE4("fence", "serialization-14", rv_kernel_fence_14, ARG_ARITHMETIC),
    CASE4("fence", "serialization-22", rv_kernel_fence_22, ARG_ARITHMETIC),
    CASE4("fence", "serialization-23", rv_kernel_fence_23, ARG_ARITHMETIC),
    CASE4("fence", "serialization-31", rv_kernel_fence_31, ARG_ARITHMETIC),
    CASE4("fence", "serialization-55", rv_kernel_fence_55, ARG_ARITHMETIC),
    CASE4("fence", "serialization-82", rv_kernel_fence_82, ARG_ARITHMETIC),
    CASE4("fence", "serialization-aa", rv_kernel_fence_aa, ARG_ARITHMETIC),
    CASE4("fence", "serialization-f5", rv_kernel_fence_f5, ARG_ARITHMETIC),
    CASE4("fence", "serialization-ff", rv_kernel_fence_ff, ARG_ARITHMETIC),
    CASE4("fence.i", "serialization", rv_kernel_fence_i, ARG_ARITHMETIC),
    CASE4("pause", "hint", rv_kernel_pause, ARG_ARITHMETIC),
    CASE4("flw", "hot-load", rv_kernel_flw, ARG_MEMORY),
    CASE4("fld", "hot-load", rv_kernel_fld, ARG_MEMORY),
    CASE4("fsw", "hot-store", rv_kernel_fsw, ARG_MEMORY),
    CASE4("fsd", "hot-store", rv_kernel_fsd, ARG_MEMORY),
    CASE4("fadd.d", "fp-dependency", rv_kernel_fadd_d, ARG_ARITHMETIC),
    CASE4("fsub.d", "fp-dependency", rv_kernel_fsub_d, ARG_ARITHMETIC),
    CASE4("fmul.d", "fp-dependency", rv_kernel_fmul_d, ARG_ARITHMETIC),
    CASE4("fdiv.d", "fp-dependency", rv_kernel_fdiv_d, ARG_ARITHMETIC),
    CASE4("fdiv.s", "fp-dependency", rv_kernel_fdiv_s, ARG_ARITHMETIC),
    CASE4("feq.d", "fp-compare", rv_kernel_feq_d, ARG_ARITHMETIC),
    CASE4("flt.d", "fp-compare", rv_kernel_flt_d, ARG_ARITHMETIC),
    CASE4("fle.d", "fp-compare", rv_kernel_fle_d, ARG_ARITHMETIC),
    CASE4("fclass.d", "fp-classify", rv_kernel_fclass_d, ARG_ARITHMETIC),
    CASE4("fsgnj.d", "fp-dependency", rv_kernel_fsgnj_d, ARG_ARITHMETIC),
    CASE4("fmv.d.x", "fp-move", rv_kernel_fmv_d_x, ARG_ARITHMETIC),
    CASE4("fmv.w.x", "fp-move", rv_kernel_fmv_w_x, ARG_ARITHMETIC),
    CASE4("fmv.x.d", "fp-move", rv_kernel_fmv_x_d, ARG_ARITHMETIC),
    CASE4("fmv.x.w", "fp-move", rv_kernel_fmv_x_w, ARG_ARITHMETIC),
    CASE4("fcvt.d.l", "fp-convert", rv_kernel_fcvt_d_l, ARG_ARITHMETIC),
    CASE4("fcvt.d.lu", "fp-convert", rv_kernel_fcvt_d_lu, ARG_ARITHMETIC),
    CASE4("fcvt.d.w", "fp-convert", rv_kernel_fcvt_d_w, ARG_ARITHMETIC),
    CASE4("fcvt.l.d", "fp-convert", rv_kernel_fcvt_l_d, ARG_ARITHMETIC),
    CASE4("fcvt.lu.d", "fp-convert", rv_kernel_fcvt_lu_d, ARG_ARITHMETIC),
    CASE4("fcvt.s.d", "fp-convert", rv_kernel_fcvt_s_d, ARG_ARITHMETIC),
    CASE4("fcvt.s.lu", "fp-convert", rv_kernel_fcvt_s_lu, ARG_ARITHMETIC),
    CASE4("fcvt.w.d", "fp-convert", rv_kernel_fcvt_w_d, ARG_ARITHMETIC),
    CASE4("csrrs", "csr-0xc01-read", rv_kernel_csrrs_time, ARG_ARITHMETIC),
    CASE4("csrrs", "csr-0x001-read", rv_kernel_csrrs_fflags, ARG_ARITHMETIC),
    CASE4("csrrs", "csr-0x002-read", rv_kernel_csrrs_frm, ARG_ARITHMETIC),
    CASE4("csrrs", "csr-0x003-read", rv_kernel_csrrs_fcsr, ARG_ARITHMETIC),
    CASE4("csrrw", "csr-0x003-write", rv_kernel_csrrw_fcsr, ARG_ARITHMETIC),
    CASE4("csrrwi", "csr-0x003-write", rv_kernel_csrrwi_fcsr, ARG_ARITHMETIC),
    CASE4("lr.w", "hot-atomic", rv_kernel_lr_w, ARG_MEMORY),
    CASE4("lr.w.aq", "hot-atomic", rv_kernel_lr_w_aq, ARG_MEMORY),
    CASE4("lr.w.aq.rl", "hot-atomic", rv_kernel_lr_w_aqrl, ARG_MEMORY),
    CASE4("lr.d", "hot-atomic", rv_kernel_lr_d, ARG_MEMORY),
    CASE4("lr.d.aq", "hot-atomic", rv_kernel_lr_d_aq, ARG_MEMORY),
    CASE4("lr.d.aq.rl", "hot-atomic", rv_kernel_lr_d_aqrl, ARG_MEMORY),
    CASE4("amoadd.w", "hot-atomic", rv_kernel_amoadd_w, ARG_MEMORY),
    CASE4("amoadd.w.aq", "hot-atomic", rv_kernel_amoadd_w_aq, ARG_MEMORY),
    CASE4("amoadd.w.rl", "hot-atomic", rv_kernel_amoadd_w_rl, ARG_MEMORY),
    CASE4("amoadd.w.aq.rl", "hot-atomic", rv_kernel_amoadd_w_aqrl, ARG_MEMORY),
    CASE4("amoadd.d", "hot-atomic", rv_kernel_amoadd_d, ARG_MEMORY),
    CASE4("amoadd.d.aq", "hot-atomic", rv_kernel_amoadd_d_aq, ARG_MEMORY),
    CASE4("amoadd.d.rl", "hot-atomic", rv_kernel_amoadd_d_rl, ARG_MEMORY),
    CASE4("amoadd.d.aq.rl", "hot-atomic", rv_kernel_amoadd_d_aqrl, ARG_MEMORY),
    CASE4("amoswap.w", "hot-atomic", rv_kernel_amoswap_w, ARG_MEMORY),
    CASE4("amoswap.w.aq", "hot-atomic", rv_kernel_amoswap_w_aq, ARG_MEMORY),
    CASE4("amoswap.w.rl", "hot-atomic", rv_kernel_amoswap_w_rl, ARG_MEMORY),
    CASE4("amoswap.w.aq.rl", "hot-atomic", rv_kernel_amoswap_w_aqrl, ARG_MEMORY),
    CASE4("amoswap.d", "hot-atomic", rv_kernel_amoswap_d, ARG_MEMORY),
    CASE4("amoswap.d.aq", "hot-atomic", rv_kernel_amoswap_d_aq, ARG_MEMORY),
    CASE4("amoswap.d.aq.rl", "hot-atomic", rv_kernel_amoswap_d_aqrl, ARG_MEMORY),
    CASE4("amoand.w", "hot-atomic", rv_kernel_amoand_w, ARG_MEMORY),
    CASE4("amoand.w.aq.rl", "hot-atomic", rv_kernel_amoand_w_aqrl, ARG_MEMORY),
    CASE4("amoand.d", "hot-atomic", rv_kernel_amoand_d, ARG_MEMORY),
    CASE4("amoand.d.rl", "hot-atomic", rv_kernel_amoand_d_rl, ARG_MEMORY),
    CASE4("amoand.d.aq.rl", "hot-atomic", rv_kernel_amoand_d_aqrl, ARG_MEMORY),
    CASE4("amoor.w", "hot-atomic", rv_kernel_amoor_w, ARG_MEMORY),
    CASE4("amoor.w.aq", "hot-atomic", rv_kernel_amoor_w_aq, ARG_MEMORY),
    CASE4("amoor.w.rl", "hot-atomic", rv_kernel_amoor_w_rl, ARG_MEMORY),
    CASE4("amoor.w.aq.rl", "hot-atomic", rv_kernel_amoor_w_aqrl, ARG_MEMORY),
    CASE4("amoor.d", "hot-atomic", rv_kernel_amoor_d, ARG_MEMORY),
    CASE4("amoor.d.aq", "hot-atomic", rv_kernel_amoor_d_aq, ARG_MEMORY),
    CASE4("amoor.d.rl", "hot-atomic", rv_kernel_amoor_d_rl, ARG_MEMORY),
    CASE4("amoor.d.aq.rl", "hot-atomic", rv_kernel_amoor_d_aqrl, ARG_MEMORY),
    CASE4("amoxor.w", "hot-atomic", rv_kernel_amoxor_w, ARG_MEMORY),
    CASE4("amoxor.d", "hot-atomic", rv_kernel_amoxor_d, ARG_MEMORY),
    CASE4("amomaxu.w.aq", "hot-atomic", rv_kernel_amomaxu_w_aq, ARG_MEMORY),
    CASE4("amomaxu.d.aq", "hot-atomic", rv_kernel_amomaxu_d_aq, ARG_MEMORY),
    CASE4("amomaxu.d.aq.rl", "hot-atomic", rv_kernel_amomaxu_d_aqrl, ARG_MEMORY),
    CASE4_WITH_BASE("sc.w", "reservation-pair", rv_kernel_sc_w,
                    rv_kernel_sc_baseline_w, ARG_MEMORY),
    CASE4_WITH_BASE("sc.w.aq", "reservation-pair", rv_kernel_sc_w_aq,
                    rv_kernel_sc_baseline_w, ARG_MEMORY),
    CASE4_WITH_BASE("sc.w.rl", "reservation-pair", rv_kernel_sc_w_rl,
                    rv_kernel_sc_baseline_w, ARG_MEMORY),
    CASE4_WITH_BASE("sc.w.aq.rl", "reservation-pair", rv_kernel_sc_w_aqrl,
                    rv_kernel_sc_baseline_w, ARG_MEMORY),
    CASE4_WITH_BASE("sc.d", "reservation-pair", rv_kernel_sc_d,
                    rv_kernel_sc_baseline_d, ARG_MEMORY),
    CASE4_WITH_BASE("sc.d.aq", "reservation-pair", rv_kernel_sc_d_aq,
                    rv_kernel_sc_baseline_d, ARG_MEMORY),
    CASE4_WITH_BASE("sc.d.rl", "reservation-pair", rv_kernel_sc_d_rl,
                    rv_kernel_sc_baseline_d, ARG_MEMORY),
    {"nop", 2, "independent", rv_kernel_nop2, rv_kernel_empty, ARG_ARITHMETIC},
    CASE2("addi", "dependency", rv_kernel_c_addi, ARG_ARITHMETIC),
    CASE2("addiw", "dependency", rv_kernel_c_addiw, ARG_ARITHMETIC),
    CASE2("sext.w", "dependency", rv_kernel_c_sext_w, ARG_ARITHMETIC),
    CASE2("li", "independent", rv_kernel_c_li, ARG_ARITHMETIC),
    CASE2("lui", "independent", rv_kernel_c_lui, ARG_ARITHMETIC),
    CASE2("slli", "dependency", rv_kernel_c_slli, ARG_ARITHMETIC),
    CASE2("srli", "dependency", rv_kernel_c_srli, ARG_ARITHMETIC),
    CASE2("srai", "dependency", rv_kernel_c_srai, ARG_ARITHMETIC),
    CASE2("andi", "dependency", rv_kernel_c_andi, ARG_ARITHMETIC),
    CASE2("add", "dependency", rv_kernel_c_add, ARG_ARITHMETIC),
    CASE2("mv", "dependency", rv_kernel_c_mv, ARG_ARITHMETIC),
    CASE2("and", "dependency", rv_kernel_c_and, ARG_ARITHMETIC),
    CASE2("or", "dependency", rv_kernel_c_or, ARG_ARITHMETIC),
    CASE2("xor", "dependency", rv_kernel_c_xor, ARG_ARITHMETIC),
    CASE2("sub", "dependency", rv_kernel_c_sub, ARG_ARITHMETIC),
    CASE2("addw", "dependency", rv_kernel_c_addw, ARG_ARITHMETIC),
    CASE2("subw", "dependency", rv_kernel_c_subw, ARG_ARITHMETIC),
    CASE2("lw", "hot-load", rv_kernel_c_lw, ARG_MEMORY),
    CASE2("ld", "hot-load", rv_kernel_c_ld, ARG_MEMORY),
    CASE2("sw", "hot-store", rv_kernel_c_sw, ARG_MEMORY),
    CASE2("sd", "hot-store", rv_kernel_c_sd, ARG_MEMORY),
    CASE2("beqz", "taken-branch", rv_kernel_c_beqz, ARG_ZERO),
    CASE2_WITH_BASE("beqz", "not-taken-branch", rv_kernel_c_beqz,
                    rv_kernel_c_branch_not_taken_baseline, ARG_NONZERO),
    CASE2("bnez", "taken-branch", rv_kernel_c_bnez, ARG_NONZERO),
    CASE2_WITH_BASE("bnez", "not-taken-branch", rv_kernel_c_bnez,
                    rv_kernel_c_branch_not_taken_baseline, ARG_ZERO),
    CASE2("j", "direct-jump", rv_kernel_c_j, ARG_ARITHMETIC),
    CASE2_WITH_BASE("jr", "indirect-jump", rv_kernel_c_jr,
                    rv_kernel_c_jr_baseline, ARG_ARITHMETIC),
    CASE2_WITH_BASE("jalr", "indirect-link", rv_kernel_c_jalr,
                    rv_kernel_c_jalr_baseline, ARG_ARITHMETIC),
    CASE2_WITH_BASE("ret", "indirect-return", rv_kernel_c_ret,
                    rv_kernel_c_ret_baseline, ARG_ARITHMETIC),
    CASE2("addi4spn", "stack-address", rv_kernel_c_addi4spn, ARG_MEMORY),
    CASE2_WITH_BASE("addi16sp", "stack-adjust", rv_kernel_c_addi16sp,
                    rv_kernel_c_stack_nop, ARG_MEMORY),
    CASE2("fld", "hot-load", rv_kernel_c_fld, ARG_MEMORY),
    CASE2("fsd", "hot-store", rv_kernel_c_fsd, ARG_MEMORY),
    CASE2_WITH_BASE("lwsp", "hot-stack-load", rv_kernel_c_lwsp,
                    rv_kernel_c_stack_nop, ARG_MEMORY),
    CASE2_WITH_BASE("ldsp", "hot-stack-load", rv_kernel_c_ldsp,
                    rv_kernel_c_stack_nop, ARG_MEMORY),
    CASE2_WITH_BASE("swsp", "hot-stack-store", rv_kernel_c_swsp,
                    rv_kernel_c_stack_nop, ARG_MEMORY),
    CASE2_WITH_BASE("sdsp", "hot-stack-store", rv_kernel_c_sdsp,
                    rv_kernel_c_stack_nop, ARG_MEMORY),
    CASE2_WITH_BASE("fldsp", "hot-stack-load", rv_kernel_c_fldsp,
                    rv_kernel_c_stack_nop, ARG_MEMORY),
    CASE2_WITH_BASE("fsdsp", "hot-stack-store", rv_kernel_c_fsdsp,
                    rv_kernel_c_stack_nop, ARG_MEMORY),
};

struct differential_case {
    struct instruction_case instruction_case;
    const char *suite;
    const char *contrast;
    const char *context;
};

/* variant 由实际选择的 kernel pattern 决定，避免把 context 文本误当成
 * provenance。合并器会用同一张契约表再次校验这些组合。 */
static const char *differential_variant_for(const struct instruction_case *entry)
{
    if (strcmp(entry->pattern, "dependency-chain") == 0 ||
        strcmp(entry->pattern, "homogeneous-reset") == 0 ||
        strcmp(entry->pattern, "independent") == 0) {
        return "reference";
    }
    if (strcmp(entry->pattern, "independent-reset") == 0) {
        return "independent";
    }
    if (strcmp(entry->pattern, "stability-anchor-positive-div") == 0) {
        return "anchor";
    }
    if (strcmp(entry->pattern, "alternating-rem-div-reset") == 0 ||
        strcmp(entry->pattern, "alternating-div-rem-reset") == 0) {
        return "alternating";
    }
    return "unknown";
}

static int is_stability_anchor(const struct differential_case *entry)
{
    return entry && strcmp(entry->suite, "stability-anchor-v1") == 0;
}

static int is_calibration_case(const struct differential_case *entry)
{
    return strcmp(entry->suite, "differential-calibration-v2") == 0 ||
           strcmp(entry->suite, "stability-anchor-v1") == 0;
}

#define DIFFERENTIAL_CASE(name, pattern, function, baseline, contrast, context) \
    {{name, 4, pattern, function, baseline, ARG_DIV_NONDEGENERATE},               \
     "div-rem-dataflow-v2", contrast, context}
#define INTERACTION_CASE(name, pattern, function, baseline, contrast, context)   \
    {{name, 4, pattern, function, baseline, ARG_DIV_NONDEGENERATE},               \
     "mixed-tb-interaction-v2", contrast, context}
#define CALIBRATION_CASE()                                                        \
    {{"nop", 4, "independent", rv_kernel_nop4, rv_kernel_empty,                \
      ARG_ARITHMETIC},                                                            \
     "differential-calibration-v2", "nop-reference", "independent-nop"}
#define STABILITY_ANCHOR_CASE()                                                   \
    {{"div", 4, "stability-anchor-positive-div", rv_kernel_div,                \
      rv_kernel_nop4, ARG_DIV_NONDEGENERATE},                                    \
     "stability-anchor-v1", "positive-div-anchor", "repeated-positive-anchor"}

/* 该套件只在 filter=differential-v2 时启用，不改变 instruction_cases。 */
static const struct differential_case differential_cases[] = {
    CALIBRATION_CASE(),
    STABILITY_ANCHOR_CASE(),
    DIFFERENTIAL_CASE("div", "dependency-chain", rv_kernel_div, rv_kernel_nop4,
                      "div-dataflow", "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("div", "independent-reset", rv_kernel_reset_div,
                      rv_kernel_reset_nop4, "div-dataflow",
                      "per-slot-reset-nondegenerate"),
    DIFFERENTIAL_CASE("divu", "dependency-chain", rv_kernel_divu, rv_kernel_nop4,
                      "divu-dataflow", "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("divu", "independent-reset", rv_kernel_reset_divu,
                      rv_kernel_reset_nop4, "divu-dataflow",
                      "per-slot-reset-nondegenerate"),
    DIFFERENTIAL_CASE("rem", "dependency-chain", rv_kernel_rem, rv_kernel_nop4,
                      "rem-dataflow", "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("rem", "independent-reset", rv_kernel_reset_rem,
                      rv_kernel_reset_nop4, "rem-dataflow",
                      "per-slot-reset-nondegenerate"),
    DIFFERENTIAL_CASE("remu", "dependency-chain", rv_kernel_remu, rv_kernel_nop4,
                      "remu-dataflow", "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("remu", "independent-reset", rv_kernel_reset_remu,
                      rv_kernel_reset_nop4, "remu-dataflow",
                      "per-slot-reset-nondegenerate"),
    DIFFERENTIAL_CASE("divw", "dependency-chain", rv_kernel_divw, rv_kernel_nop4,
                      "divw-dataflow", "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("divw", "independent-reset", rv_kernel_reset_divw,
                      rv_kernel_reset_nop4, "divw-dataflow",
                      "per-slot-reset-nondegenerate"),
    DIFFERENTIAL_CASE("divuw", "dependency-chain", rv_kernel_divuw,
                      rv_kernel_nop4, "divuw-dataflow",
                      "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("divuw", "independent-reset", rv_kernel_reset_divuw,
                      rv_kernel_reset_nop4, "divuw-dataflow",
                      "per-slot-reset-nondegenerate"),
    DIFFERENTIAL_CASE("remw", "dependency-chain", rv_kernel_remw, rv_kernel_nop4,
                      "remw-dataflow", "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("remw", "independent-reset", rv_kernel_reset_remw,
                      rv_kernel_reset_nop4, "remw-dataflow",
                      "per-slot-reset-nondegenerate"),
    DIFFERENTIAL_CASE("remuw", "dependency-chain", rv_kernel_remuw,
                      rv_kernel_nop4, "remuw-dataflow",
                      "evolving-dependency-chain"),
    DIFFERENTIAL_CASE("remuw", "independent-reset", rv_kernel_reset_remuw,
                      rv_kernel_reset_nop4, "remuw-dataflow",
                      "per-slot-reset-nondegenerate"),
    INTERACTION_CASE("div", "homogeneous-reset", rv_kernel_reset_div,
                     rv_kernel_reset_nop4, "div-rem-alternation",
                     "homogeneous-div-reset"),
    INTERACTION_CASE("div", "alternating-rem-div-reset",
                     rv_kernel_alternating_rem_div,
                     rv_kernel_alternating_rem_div_baseline,
                     "div-rem-alternation", "alternating-with-rem-reset"),
    INTERACTION_CASE("rem", "homogeneous-reset", rv_kernel_reset_rem,
                     rv_kernel_reset_nop4, "rem-div-alternation",
                     "homogeneous-rem-reset"),
    INTERACTION_CASE("rem", "alternating-div-rem-reset",
                     rv_kernel_alternating_div_rem,
                     rv_kernel_alternating_div_rem_baseline,
                     "rem-div-alternation", "alternating-with-div-reset"),
};

struct sample_job {
    size_t case_index;
    unsigned int level;
    size_t order_slot;
};

struct aligned_data {
    _Alignas(64) uint64_t words[4096];
};

enum fp_setup_kind {
    FP_SETUP_NONE,
    FP_SETUP_SINGLE,
    FP_SETUP_DOUBLE,
};

static void reset_probe_data(struct aligned_data *data)
{
    /*
     * 低 32 位是 float 1.0，高 64 位整体也是有限 normal double。
     * 两个源操作数相同，使 mul/div 长依赖链保持在 normal 数值路径。
     */
    data->words[0] = UINT64_C(0x3ff000003f800000);
    data->words[1] = UINT64_C(0x3ff000003f800000);
    data->words[2] = UINT64_C(0x0123456789abcdef);
    data->words[3] = UINT64_C(0xfedcba9876543210);
}

static enum fp_setup_kind fp_setup_for(const struct instruction_case *entry)
{
    const char *instruction = entry->instruction;

    if (strcmp(instruction, "flw") == 0 || strcmp(instruction, "fsw") == 0 ||
        strcmp(instruction, "fdiv.s") == 0 ||
        strcmp(instruction, "fmv.w.x") == 0 ||
        strcmp(instruction, "fmv.x.w") == 0 ||
        strcmp(instruction, "fcvt.s.lu") == 0) {
        return FP_SETUP_SINGLE;
    }
    if ((instruction[0] == 'f' && strncmp(instruction, "fence", 5) != 0) ||
        strncmp(entry->pattern, "csr-0x00", 8) == 0) {
        return FP_SETUP_DOUBLE;
    }
    return FP_SETUP_NONE;
}

static void prepare_window_state(const struct instruction_case *entry,
                                 struct aligned_data *data, int reset_fcsr)
{
    reset_probe_data(data);
    switch (fp_setup_for(entry)) {
    case FP_SETUP_SINGLE:
        (reset_fcsr ? rv_kernel_fp_setup_s : rv_kernel_fp_reload_s)(
            (uintptr_t)data, 0);
        break;
    case FP_SETUP_DOUBLE:
        (reset_fcsr ? rv_kernel_fp_setup_d : rv_kernel_fp_reload_d)(
            (uintptr_t)data, 0);
        break;
    case FP_SETUP_NONE:
        break;
    }
}

static uint64_t timespec_ns(const struct timespec *value)
{
    return (uint64_t)value->tv_sec * UINT64_C(1000000000) +
           (uint64_t)value->tv_nsec;
}

static uint64_t read_time_csr(void)
{
    uint64_t value;
    __asm__ volatile("rdtime %0" : "=r"(value) : : "memory");
    return value;
}

static int parse_u64(const char *text, uint64_t minimum, uint64_t maximum,
                     uint64_t *result)
{
    char *end = NULL;
    errno = 0;
    unsigned long long value = strtoull(text, &end, 10);
    if (errno != 0 || !text[0] || !end || *end || value < minimum ||
        value > maximum) {
        return -1;
    }
    *result = (uint64_t)value;
    return 0;
}

static int token_is_safe(const char *value)
{
    if (!value || !*value) {
        return 0;
    }
    for (const unsigned char *cursor = (const unsigned char *)value; *cursor;
         ++cursor) {
        unsigned char character = *cursor;
        if (!((character >= 'a' && character <= 'z') ||
              (character >= 'A' && character <= 'Z') ||
              (character >= '0' && character <= '9') || character == '.' ||
              character == '_' || character == '-' || character == ':')) {
            return 0;
        }
    }
    return 1;
}

static uint64_t hash_token(const char *value)
{
    uint64_t hash = UINT64_C(1469598103934665603);
    for (const unsigned char *cursor = (const unsigned char *)value; *cursor;
         ++cursor) {
        hash ^= *cursor;
        hash *= UINT64_C(1099511628211);
    }
    return hash;
}

static uint64_t next_random(uint64_t *state)
{
    uint64_t value = *state;
    value ^= value >> 12;
    value ^= value << 25;
    value ^= value >> 27;
    *state = value;
    return value * UINT64_C(2685821657736338717);
}

static int selected(const char *filter, const struct instruction_case *entry)
{
    if (strcmp(filter, "all") == 0) {
        return 1;
    }
    const char *separator = strchr(filter, ':');
    if (!separator) {
        return strcmp(filter, entry->instruction) == 0 ||
               strcmp(entry->instruction, "nop") == 0;
    }
    size_t name_length = (size_t)(separator - filter);
    int target_name = strlen(entry->instruction) == name_length &&
                      memcmp(filter, entry->instruction, name_length) == 0;
    int width_control = strcmp(entry->instruction, "nop") == 0;
    if (!target_name && !width_control) {
        return 0;
    }
    uint64_t width = 0;
    if (parse_u64(separator + 1, 2, 4, &width) != 0 ||
        width != entry->encoding_bytes) {
        return 0;
    }
    return target_name || width_control;
}

static void arguments_for(const struct instruction_case *entry,
                          struct aligned_data *data, uintptr_t *arg0,
                          uintptr_t *arg1)
{
    switch (entry->argument_kind) {
    case ARG_DIV_NONDEGENERATE:
        /* 64/32 位视角下均非零且不整除，供每槽恢复用例稳定复用。 */
        *arg0 = UINT64_C(0x7fedcba987654321);
        *arg1 = UINT64_C(0x000000000001f123);
        break;
    case ARG_MEMORY:
        *arg0 = (uintptr_t)data;
        *arg1 = 0;
        break;
    case ARG_EQUAL:
        *arg0 = 1;
        *arg1 = 1;
        break;
    case ARG_NOT_EQUAL:
        *arg0 = 1;
        *arg1 = 2;
        break;
    case ARG_SIGNED_LESS:
        *arg0 = UINTPTR_MAX;
        *arg1 = 1;
        break;
    case ARG_UNSIGNED_LESS:
        *arg0 = 1;
        *arg1 = 2;
        break;
    case ARG_ZERO:
        *arg0 = 0;
        *arg1 = 0;
        break;
    case ARG_NONZERO:
        *arg0 = 1;
        *arg1 = 0;
        break;
    case ARG_ARITHMETIC:
    default:
        *arg0 = UINT64_C(0x123456789abcdef0);
        *arg1 = 3;
        break;
    }
    if (entry->argument_kind == ARG_ARITHMETIC &&
        fp_setup_for(entry) != FP_SETUP_NONE) {
        /* 让整数到浮点转换保持 exact，避免预热阶段设置异常标志。 */
        *arg0 = 3;
        *arg1 = 2;
    }
}

__attribute__((noinline)) static void run_blocks(kernel_fn_t kernel,
                                                 uintptr_t arg0,
                                                 uintptr_t arg1,
                                                 uint64_t blocks)
{
    for (uint64_t block = 0; block < blocks; ++block) {
        kernel(arg0, arg1);
    }
}

__attribute__((noinline)) static void run_profiled_window(kernel_fn_t kernel,
                                                           uintptr_t arg0,
                                                           uintptr_t arg1,
                                                           uint64_t blocks)
{
    riscv_weight_profile_start();
    run_blocks(kernel, arg0, arg1, blocks);
    riscv_weight_profile_stop();
}

static int measure_window(const char *run_id,
                          const struct instruction_case *entry,
                          const struct differential_case *differential,
                          const char *calibration_profile,
                          const char *anchor_position,
                          const char *role, const char *order,
                          uint64_t block_id, uint64_t pair_id,
                          uint64_t sequence, unsigned int level,
                          uint64_t blocks, uint64_t requested_count,
                          struct aligned_data *data)
{
    struct timespec before;
    struct timespec after;
    uintptr_t arg0;
    uintptr_t arg1;
    kernel_fn_t kernel = strcmp(role, "probe") == 0 ? entry->probe : entry->baseline;
    const char *baseline_instruction = entry->encoding_bytes == 2 ? "nop" : "nop";
    unsigned int baseline_encoding = entry->encoding_bytes;
    const char *executed_instruction =
        strcmp(role, "probe") == 0 ? entry->instruction : baseline_instruction;
    uint64_t target_count = requested_count;

    if (entry->probe == rv_kernel_nop4 || entry->probe == rv_kernel_nop2) {
        baseline_instruction = "empty";
        baseline_encoding = 0;
        if (strcmp(role, "baseline") == 0) {
            executed_instruction = "empty";
            target_count = 0;
        }
    }
    arguments_for(entry, data, &arg0, &arg1);
    /*
     * 每个真实窗口都即时预热相同 kernel/path。这样 fence.i 即使冲掉此前
     * 的 TB，也只影响锚点外的预热；分支 taken/not-taken 的两个目标路径
     * 也分别在相同参数下完成翻译。F/D 与 CSR 状态随后再次复位，确保被测
     * 窗口从确定的 normal operand/fcsr 状态开始。
     */
    prepare_window_state(entry, data, 1);
    kernel(arg0, arg1);
    run_blocks(rv_kernel_empty, (uintptr_t)data, 0, 1);
    /* 预热可能改变 FCSR/fflags；被测窗口必须从同一完整状态重新开始。 */
    prepare_window_state(entry, data, 1);
    if (clock_gettime(CLOCK_MONOTONIC_RAW, &before) != 0) {
        return -1;
    }
    uint64_t time_before = read_time_csr();
    run_profiled_window(kernel, arg0, arg1, blocks);
    uint64_t time_after = read_time_csr();
    if (clock_gettime(CLOCK_MONOTONIC_RAW, &after) != 0) {
        return -1;
    }
    uint64_t elapsed_ns = timespec_ns(&after) - timespec_ns(&before);
    uint64_t checksum = data->words[0] ^ data->words[1] ^ time_after;
    if (differential) {
        const char *differential_variant = differential_variant_for(entry);
        if (strcmp(differential_variant, "unknown") == 0) {
            errno = EINVAL;
            return -1;
        }
        printf("RV_WEIGHT_SAMPLE version=2"
               " probe_contract=mygo.riscv-instruction-weight-differential.v2"
               " operand_set=nondegenerate-7fedcba987654321-by-1f123"
               " calibration_profile=%s"
               " suite=%s contrast=%s"
               " differential_variant=%s context=%s"
               " run_id=%s block_id=%" PRIu64 " segment_id=%" PRIu64
               " pair_id=%" PRIu64 " sequence=%" PRIu64
               " role=%s order=%s round=%" PRIu64
               " count_level=%u instruction=%s encoding_bytes=%u pattern=%s"
               " executed_instruction=%s baseline_instruction=%s"
               " baseline_encoding_bytes=%u control_instruction=empty-call"
               " requested_count=%" PRIu64 " target_count=%" PRIu64
               " blocks=%" PRIu64 " slots_per_block=%u elapsed_ns=%" PRIu64
               " guest_elapsed_ns=%" PRIu64 " rdtime_delta=%" PRIu64
               " anchor_position=%s"
               " timer_reads=4 checksum=%" PRIu64 "\n",
               calibration_profile, differential->suite, differential->contrast,
               differential_variant, differential->context, run_id, block_id,
               pair_id, pair_id, sequence, role, order, block_id, level,
               entry->instruction, entry->encoding_bytes, entry->pattern,
               executed_instruction, baseline_instruction, baseline_encoding,
               requested_count, target_count, blocks, RV_SLOT_COUNT,
               elapsed_ns, elapsed_ns, time_after - time_before,
               anchor_position,
               checksum);
    } else {
        printf("RV_WEIGHT_SAMPLE version=1 run_id=%s block_id=%" PRIu64
               " segment_id=%" PRIu64 " pair_id=%" PRIu64
               " sequence=%" PRIu64 " role=%s order=%s round=%" PRIu64
               " count_level=%u instruction=%s encoding_bytes=%u pattern=%s"
               " executed_instruction=%s baseline_instruction=%s"
               " baseline_encoding_bytes=%u control_instruction=empty-call"
               " requested_count=%" PRIu64 " target_count=%" PRIu64
               " blocks=%" PRIu64 " slots_per_block=%u elapsed_ns=%" PRIu64
               " guest_elapsed_ns=%" PRIu64 " rdtime_delta=%" PRIu64
               " timer_reads=4 checksum=%" PRIu64 "\n",
               run_id, block_id, pair_id, pair_id, sequence, role, order,
               block_id, level, entry->instruction, entry->encoding_bytes,
               entry->pattern, executed_instruction, baseline_instruction,
               baseline_encoding, requested_count, target_count, blocks,
               RV_SLOT_COUNT, elapsed_ns, elapsed_ns, time_after - time_before,
               checksum);
    }
    return 0;
}

int main(int argc, char **argv)
{
    uint64_t base_blocks = 256;
    uint64_t rounds = 9;
    const char *filter = "all";
    const char *run_id = "default";
    static const uint64_t level_multipliers[] = {1, 4, 16};
    static const uint64_t long_calibration_multipliers[] = {16, 64, 256};
    const size_t case_count =
        sizeof(instruction_cases) / sizeof(instruction_cases[0]);
    const size_t differential_case_count =
        sizeof(differential_cases) / sizeof(differential_cases[0]);
    struct aligned_data data = {0};

    if (argc > 1 && parse_u64(argv[1], 1, UINT64_C(1000000), &base_blocks) != 0) {
        goto usage;
    }
    if (argc > 2 && parse_u64(argv[2], 1, 99, &rounds) != 0) {
        goto usage;
    }
    if (argc > 3) {
        filter = argv[3];
    }
    if (argc > 4) {
        run_id = argv[4];
    }
    if (argc > 5 || !token_is_safe(filter) || !token_is_safe(run_id)) {
        goto usage;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);

    int long_calibration_mode =
        strcmp(filter, "differential-v2-long-calibration") == 0 ||
        strcmp(filter, "calibration-v2-long") == 0;
    int calibration_only = strcmp(filter, "calibration-v2-long") == 0;
    int differential_mode = strcmp(filter, "differential-v2") == 0 ||
                            long_calibration_mode;
    const char *calibration_profile =
        long_calibration_mode ? "long-window-v1" : "standard-v2";
    size_t selected_differential_cases =
        calibration_only ? 1 : differential_case_count;
    size_t selected_count = 0;
    if (differential_mode) {
        selected_count = selected_differential_cases;
    } else {
        for (size_t index = 0; index < case_count; ++index) {
            selected_count += selected(filter, &instruction_cases[index]) != 0;
        }
    }
    if (selected_count == 0) {
        fprintf(stderr, "RV_WEIGHT_ERROR unknown_filter=%s\n", filter);
        return 2;
    }
    uint64_t maximum_multiplier = long_calibration_mode
                                      ? long_calibration_multipliers[2]
                                      : level_multipliers[2];
    if (base_blocks > UINT64_MAX / maximum_multiplier ||
        base_blocks * maximum_multiplier > UINT64_MAX / RV_SLOT_COUNT) {
        fprintf(stderr, "RV_WEIGHT_ERROR count_overflow=1\n");
        return 2;
    }

    if (differential_mode) {
        printf("RV_WEIGHT_BENCH version=2 arch=riscv64 isa=rv64imafdc"
               " suite=differential-v2 cases=%zu selected=%zu"
               " base_blocks=%" PRIu64 " levels=3 rounds=%" PRIu64
               " filter=%s run_id=%s slots_per_block=%u"
               " calibration_profile=%s"
               " calibration_level_multipliers=%s"
               " operand_policy=nondegenerate-per-slot-reset"
               " clock=monotonic_raw independent_clock=rdtime\n",
               differential_case_count, selected_count, base_blocks, rounds,
               filter, run_id, RV_SLOT_COUNT, calibration_profile,
               long_calibration_mode ? "16,64,256" : "1,4,16");
        printf("RV_WEIGHT_UNSUPPORTED version=2"
               " classes=privileged,trap,cbo-user-disabled,vector"
               " policy=separate-contextual-probes\n");
    } else {
        printf("RV_WEIGHT_BENCH version=1 arch=riscv64 isa=rv64imafdc"
               " cases=%zu selected=%zu base_blocks=%" PRIu64
               " levels=3 rounds=%" PRIu64 " filter=%s run_id=%s"
               " slots_per_block=%u extensions=zicsr,zifencei,zihintpause"
               " fp_operands=normal-deterministic clock=monotonic_raw"
               " independent_clock=rdtime\n",
               case_count, selected_count, base_blocks, rounds, filter, run_id,
               RV_SLOT_COUNT);
        printf("RV_WEIGHT_UNSUPPORTED version=1"
               " classes=privileged,trap,cbo-user-disabled,vector"
               " policy=separate-contextual-probes\n");
    }

    /* 预翻译所有会进入测量窗口的 kernel，避免首轮 TCG 翻译成本。 */
    reset_probe_data(&data);
    rv_kernel_empty((uintptr_t)&data, 0);
    if (differential_mode) {
        for (size_t index = 0; index < selected_differential_cases; ++index) {
            const struct instruction_case *entry =
                &differential_cases[index].instruction_case;
            uintptr_t arg0;
            uintptr_t arg1;
            arguments_for(entry, &data, &arg0, &arg1);
            prepare_window_state(entry, &data, 1);
            entry->probe(arg0, arg1);
            prepare_window_state(entry, &data, 0);
            entry->baseline(arg0, arg1);
        }
    } else {
        for (size_t index = 0; index < case_count; ++index) {
            if (!selected(filter, &instruction_cases[index])) {
                continue;
            }
            uintptr_t arg0;
            uintptr_t arg1;
            arguments_for(&instruction_cases[index], &data, &arg0, &arg1);
            prepare_window_state(&instruction_cases[index], &data, 1);
            instruction_cases[index].probe(arg0, arg1);
            prepare_window_state(&instruction_cases[index], &data, 0);
            instruction_cases[index].baseline(arg0, arg1);
        }
    }
    /* 首个不记样本的窗口预热 marker、run_blocks 与 stop TB。插件默认丢弃它。 */
    run_profiled_window(rv_kernel_nop4, (uintptr_t)&data, 0, 4);

    size_t jobs_per_round = selected_count * 3;
    struct sample_job *jobs = calloc(jobs_per_round, sizeof(*jobs));
    unsigned char *order_schedule = calloc(
        jobs_per_round * (size_t)rounds, sizeof(*order_schedule));
    if (!jobs || !order_schedule) {
        fprintf(stderr, "RV_WEIGHT_ERROR allocation=jobs\n");
        free(order_schedule);
        free(jobs);
        return 1;
    }
    uint64_t random_state = hash_token(run_id) ^ UINT64_C(0x9e3779b97f4a7c15);
    /*
     * 每个 context/batch 独立生成受约束随机顺序。偶数 rounds 严格 AB/BA
     * 各半；奇数 rounds 最多相差一对。这样保留随机化，同时避免小样本中
     * 独立抛硬币产生的顺序失衡。
     */
    for (size_t slot = 0; slot < jobs_per_round; ++slot) {
        unsigned int offset = (unsigned int)(next_random(&random_state) & 1U);
        unsigned char *schedule = order_schedule + slot * (size_t)rounds;
        for (size_t round = 0; round < (size_t)rounds; ++round) {
            schedule[round] = (unsigned char)((round + offset) & 1U);
        }
        for (size_t index = (size_t)rounds; index > 1; --index) {
            size_t other = (size_t)(next_random(&random_state) % index);
            unsigned char temporary = schedule[index - 1];
            schedule[index - 1] = schedule[other];
            schedule[other] = temporary;
        }
    }
    uint64_t sequence = 0;
    uint64_t pair_id = 0;
    unsigned int failures = 0;

    /*
     * 正成本 anchor 在主随机序列之外于首部先测一次；同一用例仍会在每轮
     * 的随机位置出现，并在末尾再测一次。这样能够分别检查启动、段内和
     * 尾部速度尺度，且 anchor 窗口使用 long profile 提高信噪比。
     */
    if (differential_mode && !calibration_only) {
        const struct differential_case *anchor = &differential_cases[1];
        const uint64_t *anchor_multipliers =
            long_calibration_mode ? long_calibration_multipliers
                                  : level_multipliers;
        uint64_t blocks = base_blocks * anchor_multipliers[1];
        uint64_t requested_count = blocks * RV_SLOT_COUNT;
        ++pair_id;
        for (size_t role_index = 0; role_index < 2; ++role_index) {
            const char *role = role_index == 0 ? "probe" : "baseline";
            ++sequence;
            if (measure_window(run_id, &anchor->instruction_case, anchor,
                               calibration_profile, "head", role, "AB", 0, pair_id,
                               sequence, 1, blocks, requested_count, &data) != 0) {
                ++failures;
            }
        }
    }

    for (uint64_t round = 0; round < rounds; ++round) {
        size_t job_count = 0;
        size_t selected_slot = 0;
        if (differential_mode) {
            for (size_t case_index = 0; case_index < selected_differential_cases;
                 ++case_index) {
                for (unsigned int level = 0; level < 3; ++level) {
                    jobs[job_count++] =
                        (struct sample_job){case_index, level,
                                            selected_slot * 3 + level};
                }
                ++selected_slot;
            }
        } else {
            for (size_t case_index = 0; case_index < case_count; ++case_index) {
                if (!selected(filter, &instruction_cases[case_index])) {
                    continue;
                }
                for (unsigned int level = 0; level < 3; ++level) {
                    jobs[job_count++] =
                        (struct sample_job){case_index, level,
                                            selected_slot * 3 + level};
                }
                ++selected_slot;
            }
        }
        for (size_t index = job_count; index > 1; --index) {
            size_t other = (size_t)(next_random(&random_state) % index);
            struct sample_job temporary = jobs[index - 1];
            jobs[index - 1] = jobs[other];
            jobs[other] = temporary;
        }

        for (size_t job_index = 0; job_index < job_count; ++job_index) {
            const struct sample_job *job = &jobs[job_index];
            const struct differential_case *differential =
                differential_mode ? &differential_cases[job->case_index] : NULL;
            const struct instruction_case *entry =
                differential ? &differential->instruction_case
                             : &instruction_cases[job->case_index];
            const uint64_t *multipliers =
                long_calibration_mode && differential &&
                        is_calibration_case(differential)
                    ? long_calibration_multipliers
                    : level_multipliers;
            uint64_t blocks = base_blocks * multipliers[job->level];
            uint64_t requested_count = blocks * RV_SLOT_COUNT;
            int probe_first =
                order_schedule[job->order_slot * (size_t)rounds + round] == 0;
            const char *order = probe_first ? "AB" : "BA";
            const char *roles[2] = {
                probe_first ? "probe" : "baseline",
                probe_first ? "baseline" : "probe",
            };
            ++pair_id;
            for (size_t role_index = 0; role_index < 2; ++role_index) {
                ++sequence;
                if (measure_window(run_id, entry, differential,
                                   calibration_profile,
                                   is_stability_anchor(differential)
                                       ? "body"
                                       : "not-anchor",
                                   roles[role_index], order,
                                   round + 1, pair_id, sequence, job->level,
                                   blocks, requested_count, &data) != 0) {
                    fprintf(stderr,
                            "RV_WEIGHT_ERROR sequence=%" PRIu64
                            " phase=measure errno=%d\n",
                            sequence, errno);
                    ++failures;
                }
            }
        }
    }
    if (differential_mode && !calibration_only) {
        const struct differential_case *anchor = &differential_cases[1];
        const uint64_t *anchor_multipliers =
            long_calibration_mode ? long_calibration_multipliers
                                  : level_multipliers;
        uint64_t blocks = base_blocks * anchor_multipliers[1];
        uint64_t requested_count = blocks * RV_SLOT_COUNT;
        ++pair_id;
        for (size_t role_index = 0; role_index < 2; ++role_index) {
            const char *role = role_index == 0 ? "baseline" : "probe";
            ++sequence;
            if (measure_window(run_id, &anchor->instruction_case, anchor,
                               calibration_profile, "tail", role, "BA", rounds + 1,
                               pair_id, sequence, 1, blocks, requested_count,
                               &data) != 0) {
                ++failures;
            }
        }
    }
    free(order_schedule);
    free(jobs);
    printf("RV_WEIGHT_BENCH_DONE version=%u status=%u pairs=%" PRIu64
           " windows=%" PRIu64 "\n",
           differential_mode ? 2U : 1U, failures ? 1U : 0U, pair_id,
           sequence);
    return failures ? 1 : 0;

usage:
    fprintf(stderr,
            "usage: %s [base_blocks [rounds [instruction[:2|4]|all|differential-v2|differential-v2-long-calibration|calibration-v2-long [run_id]]]]\n",
            argv[0]);
    return 2;
}
