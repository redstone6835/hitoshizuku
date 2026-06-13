//! RISC-V64 内核启动入口。
//!
//! 启动约定（OpenSBI → S-mode payload）：
//!   a0 = hartid, a1 = dtb_paddr, satp = 0, 特权级 = S-mode
//!
//! RISC-V 没有 LoongArch 的 DMW 直映窗口，必须在物理地址空间手动搭建最小页表
//! 后才能开启 MMU。整个启动分三阶段：
//!
//! ```text
//!   _start (PA)             _start_virtualized (VA)         __kernel_arch_loader (VA)
//!       │                          │                                │
//!       ├─ 关中断/清 sscratch       ├─ 设临时栈                      ├─ 正式初始化
//!       ├─ 建 Sv48 最小页表         ├─ 使能 FPU                     └─ 不返回
//!       ├─ csrw satp               ├─ pre_boot_init()
//!       └─ jr VA ─────────────────►└─ jr __kernel_arch_loader ───►
//! ```
//!
//! 早期页表布局（3 × 4KiB）：
//!
//! ```text
//!   PGD[0]   → PUD_identity    PUD_identity[0] = 1G leaf → 0x0 (MMIO, RW)
//!   PGD[511] → PUD_kernel      PUD_identity[2] = 1G leaf → 0x8000_0000 (RWXAD)
//!                              PUD_identity[3] = 1G leaf → 0xC000_0000 (RWXAD)
//!                              PUD_kernel[2]   = 1G leaf → 0x8000_0000 (RWXAD)
//!                              PUD_kernel[3]   = 1G leaf → 0xC000_0000 (RWXAD)
//! ```

use core::arch::naked_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::clear_bss;

// ── 启动参数 ──────────────────────────────────────────────────────────────────

/// 引导 hart ID（调度器用于识别 BSP）。
pub static BOOT_HART_ID: AtomicUsize = AtomicUsize::new(0);

/// DTB 物理地址。
pub static BOOT_DTB_ADDR: AtomicUsize = AtomicUsize::new(0);

// ── 早期页表常量 ──────────────────────────────────────────────────────────────
// 以下常量记录汇编中使用的立即数含义，naked_asm! 无法直接引用 Rust const，
// 因此汇编中仍使用字面值，这里仅作文档用途。

/// Sv48 模式的 MODE 字段（satp[63:60] = 9）。
#[allow(dead_code)]
const SATP_MODE_SV48: u64 = 9 << 60;

/// 内核虚拟地址偏移的高 32 位，必须与 `addr.rs::KERNEL_VA_OFFSET` 一致。
#[allow(dead_code)]
const VA_OFFSET_HI32: u64 = 0xFFFF_FF80;

// PTE flags（Sv48 叶节点）
/// MMIO 区域：V|R|W|A|D（无 X，防止投机执行 MMIO）
#[allow(dead_code)]
const PTE_MMIO_LEAF: u64 = 0xC7; // 1100_0111
/// RAM 区域：V|R|W|X|A|D
#[allow(dead_code)]
const PTE_RAM_LEAF: u64 = 0xCF; // 1100_1111
/// 非叶 PTE：仅 V 位
#[allow(dead_code)]
const PTE_NONLEAF_V: u64 = 0x01;

/// 1GiB RAM 区域（物理地址 0x8000_0000）的 PPN 高位部分。
/// 计算方式：(0x8000_0000 >> 12) = 0x20000，通过 lui 指令加载。
#[allow(dead_code)]
const RAM_1G_PPN_LUI: u64 = 0x20000;

// ── 早期页表存储 ──────────────────────────────────────────────────────────────

/// 早期启动页表——3 × 4KiB 数组，由 `_start` 汇编代码在物理地址空间填充。
#[repr(C, align(4096))]
struct EarlyPageTable([u64; 512 * 3]);

#[unsafe(link_section = ".data.prepage")]
static EARLY_PT: EarlyPageTable = EarlyPageTable([0u64; 512 * 3]);

// ── _start ────────────────────────────────────────────────────────────────────

/// 内核物理地址入口（由 OpenSBI 跳入）。
///
/// # Safety
///
/// 前置条件：satp=0, a0=hartid, a1=dtb_paddr, S-mode。
/// 仅由硬件/固件在引导时调用一次。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
pub unsafe extern "C" fn _start() {
    naked_asm!(
        // ── 保存启动参数 ──
        "mv s0, a0",
        "mv s1, a1",

        // ── 防御性屏障：关中断 + 清 sscratch ──
        // 避免页表构建期间意外 trap 误入 from_user 路径
        "csrci sstatus, 2",
        "csrw sscratch, zero",

        // ── 构建 Sv48 最小页表 ──
        // la 是 PC-relative，VMA 差值 == PA 差值，物理空间有效
        "la t0, {early_pt}",
        "lui t1, 0x1",
        "add t1, t0, t1",           // t1 = PUD_identity (t0 + 4K)
        "lui t3, 0x2",
        "add t3, t0, t3",           // t3 = PUD_kernel  (t0 + 8K)

        // 清零 12KiB（.data 段不保证零初始化）
        "mv t5, t0",
        "lui t6, 0x3",
        "add t6, t0, t6",
        "2: sd zero, 0(t5)",
        "addi t5, t5, 8",
        "bne t5, t6, 2b",

        // PGD[0] → PUD_identity（非叶：PPN | V）
        "srli t2, t1, 2",
        "ori t2, t2, 1",
        "sd t2, 0(t0)",

        // PGD[511] → PUD_kernel
        "srli t2, t3, 2",
        "ori t2, t2, 1",
        "li t4, 511 * 8",
        "add t4, t0, t4",
        "sd t2, 0(t4)",

        // PUD_identity[0] = 1G → PA 0 (MMIO, RW, no X)
        "li t2, 0xC7",
        "sd t2, 0(t1)",

        // PUD_identity[2] = 1G → PA 0x8000_0000 (identity)
        "lui t2, 0x20000",
        "addi t2, t2, 0xCF",
        "sd t2, 16(t1)",

        // PUD_identity[3] = 1G → PA 0xC000_0000 (identity, 覆盖 DTB)
        "lui t2, 0x30000",
        "addi t2, t2, 0xCF",
        "sd t2, 24(t1)",

        // PUD_kernel[2] = 1G → PA 0x8000_0000 (高半区)
        "lui t2, 0x20000",
        "addi t2, t2, 0xCF",
        "sd t2, 16(t3)",

        // PUD_kernel[3] = 1G → PA 0xC000_0000 (高半区, 覆盖 DTB)
        "lui t2, 0x30000",
        "addi t2, t2, 0xCF",
        "sd t2, 24(t3)",

        // ── 激活 Sv48 ──
        "srli t2, t0, 12",           // PPN
        "li t3, 9",
        "slli t3, t3, 60",           // MODE = Sv48
        "or t2, t2, t3",

        // 计算 _start_virtualized 的虚拟地址
        "la t0, {virt_entry}",
        "li t1, {va_hi32}",
        "slli t1, t1, 32",
        "add t0, t0, t1",

        // 切换地址空间（identity 映射保证此处取指不 fault）
        "csrw 0x180, t2",           // satp
        "sfence.vma",
        "jr t0",

        virt_entry = sym _start_virtualized,
        early_pt = sym EARLY_PT,
        va_hi32 = const 0xFFFFFF80u64,
    )
}

// ── _start_virtualized ────────────────────────────────────────────────────────

/// 虚拟地址空间入口，承接 `_start`，准备 Rust 执行环境。
///
/// # Safety
///
/// 前置条件：Sv48 已激活，s0=hartid，s1=dtb_paddr，PC 在 VA 空间。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start_virtualized() {
    naked_asm!(
        "mv ra, zero",               // 标记调用栈终点
        "la sp, __tmp_stack_top",    // 临时栈（链接脚本定义）

        // 使能 FPU：设置 SSTATUS.FS = Dirty（3 << 13）
        // LLVM 在 release 模式下可能生成浮点指令，必须在调用 Rust 代码前开启 FPU 支持
        "li t0, 3 << 13",
        "csrs sstatus, t0",

        // pre_boot_init(hartid, dtb_paddr)
        "mv a0, s0",
        "mv a1, s1",
        "la t0, pre_boot_init",
        "jalr t0",

        // __kernel_arch_loader(hartid, dtb_paddr) — 不返回
        "mv a0, s0",
        "mv a1, s1",
        "la t0, __kernel_arch_loader",
        "jr t0",
    )
}

// ── pre_boot_init ─────────────────────────────────────────────────────────────

/// 采集启动信息、清零 BSS、初始化 per-hart 数据。
///
/// 必须先 clear_bss 再写静态变量，否则 BSS 清零会覆盖已写入的值。
#[unsafe(no_mangle)]
unsafe extern "C" fn pre_boot_init(hartid: usize, dtb_addr: usize) {
    use crate::riscv64::specific::{IRQ_STACKS, IRQ_STACK_SIZE};

    unsafe { clear_bss() };

    crate::riscv64::early_console::e_print(format_args!("R\n"));

    BOOT_HART_ID.store(hartid, Ordering::Release);
    BOOT_DTB_ADDR.store(dtb_addr, Ordering::Release);

    // 初始化 boot hart（index 0）的 per-hart 数据
    unsafe {
        let hl = crate::riscv64::specific::boot_hart_local_ptr();
        (*hl).hart_id = hartid;
        // 中断栈栈顶 = IRQ_STACKS[0] 末尾（栈向低地址增长）
        (*hl).irq_stack_top = core::ptr::addr_of!(IRQ_STACKS) as usize + IRQ_STACK_SIZE;
        core::sync::atomic::compiler_fence(Ordering::Release);
        core::arch::asm!("mv tp, {}", in(reg) hl as usize, options(nomem, nostack));
    }
}
