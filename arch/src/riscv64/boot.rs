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
//! 早期页表布局（4 × 4KiB）：
//!
//! ```text
//!   PGD[0]   → PUD_identity    PUD_identity[0] = 1G leaf → 0x0 (MMIO, RW+NX)
//!   PGD[511] → PUD_kernel      PUD_identity[2] ─┐
//!                              PUD_kernel[2]   ──┴→ PMD_ram（512×2MiB，RX/R/RW）
//!                              PUD_identity[3] = 1G leaf → 0xC000_0000 (RW+NX)
//!                              PUD_kernel[3]   = 1G leaf → 0xC000_0000 (RW+NX)
//!                              PUD[* >= 4]     = 仅高端 DTB 所在 1GiB leaf（R+NX，临时）
//! ```

use core::arch::naked_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::clear_bss;
use crate::riscv64::csr::SATP_MODE_SV48;

// ── 启动参数 ──────────────────────────────────────────────────────────────────

/// 引导 hart ID（调度器用于识别 BSP）。
pub static BOOT_HART_ID: AtomicUsize = AtomicUsize::new(0);

/// DTB 物理地址。
pub static BOOT_DTB_ADDR: AtomicUsize = AtomicUsize::new(0);

// ── 早期页表常量 ──────────────────────────────────────────────────────────────
// 这些值通过 `naked_asm!` 的 const operand 传入启动汇编，避免 Rust 注释常量
// 与真正执行的立即数发生漂移。

/// 内核虚拟地址偏移的高 32 位，必须与 `addr.rs::KERNEL_VA_OFFSET` 一致。
const VA_OFFSET_HI32: usize = 0xFFFF_FF80;

// PTE flags（Sv48 叶节点）
/// MMIO 区域：V|R|W|A|D（无 X，防止投机执行 MMIO）
const PTE_MMIO_LEAF: usize = 0xC7; // 1100_0111
/// RAM 数据区域：V|R|W|A|D（NX）。
const PTE_RAM_RW_LEAF: usize = 0xC7;
/// 内核 text：V|R|X|A。
const PTE_RAM_RX_LEAF: usize = 0x4B;
/// 内核 rodata：V|R|A。
const PTE_RAM_R_LEAF: usize = 0x43;
/// 非叶 PTE：仅 V 位
const PTE_NONLEAF_V: usize = 0x01;

/// 2 MiB 物理步长换算为 PTE PPN 字段后的增量：2MiB >> 2。
const PMD_PTE_STEP: usize = 0x8_0000;
/// loader 的 DTB 固定缓冲区上限；早期临时 leaf 至少要覆盖这么多字节。
const EARLY_DTB_MAX_SIZE: usize = 2 * 1024 * 1024;
/// 高端 DTB 临时映射使用 1 GiB PUD leaf，当前逻辑最多补映射相邻的一个 leaf。
const EARLY_DTB_LEAF_SIZE: usize = 1 << 30;
const _: () = assert!(EARLY_DTB_MAX_SIZE <= EARLY_DTB_LEAF_SIZE);
/// DTB 在 leaf 内的偏移大于该值时，还需要映射下一个 leaf。
const EARLY_DTB_NEXT_LEAF_THRESHOLD: usize = EARLY_DTB_LEAF_SIZE - EARLY_DTB_MAX_SIZE;

/// 后续 1GiB RAM 区域（物理地址 0xC000_0000）的 PPN 高位部分。
const RAM_UPPER_PPN_LUI: usize = 0x30000;

// ── 早期页表存储 ──────────────────────────────────────────────────────────────

/// 早期启动页表——PGD、identity PUD、kernel PUD、共享 RAM PMD。
#[repr(C, align(4096))]
struct EarlyPageTable([u64; 512 * 4]);

#[unsafe(link_section = ".data.prepage")]
static EARLY_PT: EarlyPageTable = EarlyPageTable([0u64; 512 * 4]);

// ── _start ────────────────────────────────────────────────────────────────────

/// 内核物理地址入口（由 OpenSBI 跳入）。
///
/// # Safety
///
/// 前置条件：satp=0, a0=hartid, a1=dtb_paddr, S-mode。
/// 仅由硬件/固件在引导时调用一次。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry.boot")]
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
        "lui t4, 0x3",
        "add t4, t0, t4",           // t4 = PMD_ram     (t0 + 12K)

        // 清零 16KiB（.data 段不保证零初始化）
        "mv t5, t0",
        "lui t6, 0x4",
        "add t6, t0, t6",
        "2: sd zero, 0(t5)",
        "addi t5, t5, 8",
        "bne t5, t6, 2b",

        // PGD[0] → PUD_identity（非叶：PPN | V）
        "srli t2, t1, 2",
        "ori t2, t2, {pte_nonleaf_v}",
        "sd t2, 0(t0)",

        // PGD[511] → PUD_kernel
        "srli t2, t3, 2",
        "ori t2, t2, {pte_nonleaf_v}",
        "li t5, 511 * 8",
        "add t5, t0, t5",
        "sd t2, 0(t5)",

        // PUD_identity[0] = 1G → PA 0 (MMIO, RW, no X)
        "li t2, {pte_mmio_leaf}",
        "sd t2, 0(t1)",

        // identity 与高半区共享同一张 RAM PMD；叶项仍映射相同 PA。
        "srli t2, t4, 2",
        "ori t2, t2, {pte_nonleaf_v}",
        "sd t2, 16(t1)",
        "sd t2, 16(t3)",

        // PUD[3] = 1G → PA 0xC000_0000，启动期数据/DTB 只需 RW+NX。
        "lui t2, {ram_upper_ppn_lui}",
        "addi t2, t2, {pte_ram_rw_leaf}",
        "sd t2, 24(t1)",
        "sd t2, 24(t3)",

        // QEMU 在大内存配置下会把 DTB 放到 4GiB 以上。loader 会先用
        // phys_to_virt 复制 DTB，因此在正式 direct map 建好前临时映射 DTB 所在
        // 1GiB leaf。该映射只读、不可执行，且 heap_vm 发布正式页表时会清除。
        "srli t5, s1, 39",
        "bnez t5, 99f",           // 当前早期 PGD 只覆盖低 512GiB PA
        "srli t5, s1, 30",        // DTB 所在 1GiB leaf 索引
        "li t6, 4",
        "bltu t5, t6, 8f",        // PA < 4GiB 已由上述固定映射覆盖
        "srli t2, s1, 30",
        "slli t2, t2, 28",        // 1GiB-aligned PA >> 2（PTE PPN 字段）
        "ori t2, t2, {pte_ram_r_leaf}",
        "slli t6, t5, 3",
        "add t0, t1, t6",
        "sd t2, 0(t0)",
        "add t0, t3, t6",
        "sd t2, 0(t0)",

        // DTB 缓冲区最大 2MiB；若起点过于靠近 1GiB 边界，同时映射
        // 下一个 leaf。跨越当前 512GiB 早期窗口时无法安全继续。
        "li t6, 0x3fffffff",
        "and t6, s1, t6",
        "li t0, {early_dtb_next_leaf_threshold}",
        "bleu t6, t0, 8f",
        "li t6, 511",
        "beq t5, t6, 99f",
        "addi t5, t5, 1",
        "li t6, 0x10000000",      // 1GiB >> 2
        "add t2, t2, t6",
        "slli t6, t5, 3",
        "add t0, t1, t6",
        "sd t2, 0(t0)",
        "add t0, t3, t6",
        "sd t2, 0(t0)",
        "8:",

        // 默认把 PA 0x8000_0000..0xC000_0000 建成 512 个 2MiB RW+NX leaf。
        "li t2, 0x20000000 + {pte_ram_rw_leaf}",
        "mv t5, t4",
        "li t6, 512",
        "3: sd t2, 0(t5)",
        "addi t5, t5, 8",
        "li t0, {pmd_pte_step}",
        "add t2, t2, t0",
        "addi t6, t6, -1",
        "bnez t6, 3b",

        // text 所在 2MiB leaves 改成 RX。链接脚本保证 etext 2MiB 对齐。
        "la t0, {stext}",
        "li t1, 0x80000000",
        "sub t0, t0, t1",
        "srli t0, t0, 21",
        "la t2, {etext}",
        "sub t2, t2, t1",
        "srli t2, t2, 21",
        "4: bgeu t0, t2, 5f",
        "slli t5, t0, 3",
        "add t5, t4, t5",
        "ld t6, 0(t5)",
        "andi t6, t6, -1024",
        "ori t6, t6, {pte_ram_rx_leaf}",
        "sd t6, 0(t5)",
        "addi t0, t0, 1",
        "j 4b",

        // rodata 所在 leaves 改成 R+NX；erodata 同样按 2MiB 对齐。
        "5:",
        "mv t0, t2",
        "la t2, {erodata}",
        "sub t2, t2, t1",
        "srli t2, t2, 21",
        "6: bgeu t0, t2, 7f",
        "slli t5, t0, 3",
        "add t5, t4, t5",
        "ld t6, 0(t5)",
        "andi t6, t6, -1024",
        "ori t6, t6, {pte_ram_r_leaf}",
        "sd t6, 0(t5)",
        "addi t0, t0, 1",
        "j 6b",
        "7:",

        // ── 激活 Sv48 ──
        "la t0, {early_pt}",
        "srli t2, t0, 12",           // PPN
        "li t3, {satp_mode}",
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

        // DTB 超出当前早期 Sv48 窗口时不能冒险越界写页表。
        "99: csrci sstatus, 2",
        "100: wfi",
        "j 100b",

        virt_entry = sym _start_virtualized,
        early_pt = sym EARLY_PT,
        stext = sym stext,
        etext = sym etext,
        erodata = sym erodata,
        pte_nonleaf_v = const PTE_NONLEAF_V,
        pte_mmio_leaf = const PTE_MMIO_LEAF,
        pte_ram_rw_leaf = const PTE_RAM_RW_LEAF,
        pte_ram_rx_leaf = const PTE_RAM_RX_LEAF,
        pte_ram_r_leaf = const PTE_RAM_R_LEAF,
        pmd_pte_step = const PMD_PTE_STEP,
        early_dtb_next_leaf_threshold = const EARLY_DTB_NEXT_LEAF_THRESHOLD,
        ram_upper_ppn_lui = const RAM_UPPER_PPN_LUI,
        satp_mode = const (SATP_MODE_SV48 >> 60),
        va_hi32 = const VA_OFFSET_HI32,
    )
}

unsafe extern "C" {
    fn stext();
    fn etext();
    fn erodata();
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
        "mv ra, zero",            // 标记调用栈终点
        "la sp, __tmp_stack_top", // 临时栈（链接脚本定义）
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
    use crate::riscv64::specific::{IRQ_STACK_ALLOCATION_SIZE, IRQ_STACKS};

    unsafe { clear_bss() };

    crate::riscv64::early_console::e_print(format_args!("R\n"));

    BOOT_HART_ID.store(hartid, Ordering::Release);
    BOOT_DTB_ADDR.store(dtb_addr, Ordering::Release);

    // 初始化 boot hart（index 0）的 per-hart 数据
    unsafe {
        let hl = crate::riscv64::specific::boot_hart_local_ptr();
        let kernel_gp: usize;
        core::arch::asm!("mv {}, gp", out(reg) kernel_gp, options(nomem, nostack));
        (*hl).hart_id = hartid;
        (*hl).logical_id = 0;
        (*hl).kernel_gp = kernel_gp;
        // 中断栈栈顶 = IRQ_STACKS[0] 末尾（栈向低地址增长）
        (*hl).irq_stack_top = core::ptr::addr_of!(IRQ_STACKS) as usize + IRQ_STACK_ALLOCATION_SIZE;
        core::sync::atomic::compiler_fence(Ordering::Release);
        core::arch::asm!("mv tp, {}", in(reg) hl as usize, options(nomem, nostack));
    }
}
