//! LoongArch64 架构相关的启动代码实现。
//!
//! LoongArch64 的启动约定规定了 CPU 启动时寄存器的状态和内存布局，该模块实现
//! 了 [`_start`] 入口函数，负责完成最小的启动初始化并跳转到内核主函数。
//!
//! 这个阶段的约束非常硬：正式页表、正式栈和正式设备模型都还不存在。因此代码必须先
//! 建立最小可运行环境，包括 DMW 访问窗口、异常入口和临时栈，然后才能把控制权安全地
//! 转给 Rust 初始化逻辑。

use core::arch::naked_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::clear_bss;
use crate::loongarch64::*;

/// EFI BOOT 使能标志。
pub static EFI_BOOT: AtomicUsize = AtomicUsize::new(0);

/// cmdline 字符串地址。
///
/// 注意：cmdline 的基地址位于 DMW0 窗口下。
pub static CMDLINE_PTR: AtomicUsize = AtomicUsize::new(0);

/// EFI 系统表地址。
pub static EFI_SYSTEM_TABLE_PTR: AtomicUsize = AtomicUsize::new(0);

/// 返回 `_start` 保存的完整启动寄存器参数（架构归一化）。
///
/// 三个字段恰好对应 `$a0/$a1/$a2`：`pre_boot_init` 已把原始值存入
/// [`EFI_BOOT`] / [`CMDLINE_PTR`] / [`EFI_SYSTEM_TABLE_PTR`]。协议抽象层
/// （`crate::boot_protocol`）只消费此视图做识别与决策，不触碰早期控制台等已由
/// 各 atomic 驱动的子系统。
pub(crate) fn boot_registers() -> crate::boot_protocol::BootRegisters {
    crate::boot_protocol::BootRegisters::new(
        EFI_BOOT.load(Ordering::Acquire),
        CMDLINE_PTR.load(Ordering::Acquire),
        EFI_SYSTEM_TABLE_PTR.load(Ordering::Acquire),
    )
}

/// # 内核入口点
///
/// 该函数是 LoongArch64 架构的内核入口点，按照 LoongArch64 的启动约定执行。
///
/// 设置一些基本的寄存器状态和内存映射，然后跳转到 [`_start_virtualized`] 继续
/// 执行内核的初始化逻辑。
///
/// # Safety
///
/// 该函数不应当被以除计算机引导时转移硬件控制权之外的任何调用者直接调用。
///
/// 从硬件语义上看，这个入口仍处在“主要靠 DMW 直映访问内存”的阶段，因此先配置 DMW，
/// 再安装异常入口，最后才跳向更高层初始化。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
pub unsafe extern "C" fn _start() {
    naked_asm!(
        // QEMU LoongArch 直启路径下，`$a0 / $a1 / $a2`（即 `$r4 / $r5 / $r6`）
        // 分别携带 efi_boot 标志、命令行指针和 EFI system table 指针。

        // 在 `pre_boot_init` 完成采集之前必须保持这三个参数寄存器不被破坏。

        // 通过设置 DMW0 的参数，使得 CPU 可以直接访问以 0x8000_0000_0000_0000
        // 开始的虚拟内存区域，便于在早期分页还没有建立的时候访问外设。同时将
        // DMW0 设置为非缓存模式，因为外设寄存器通常不能够被 CPU 缓存。
        "ori $r12, $r0, 0x1",
        "lu52i.d $r12, $r12, -2048",
        "csrwr $r12, 0x180",

        // 设置 DMW1 的参数，将以 0x9000_0000_0000_0000 开始的内存区域映射到物理
        // 地址空间。0x9000_0000_0000_0000 是 LoongArch64 的虚拟地址空间的起始地
        // 址，通过将其直接映射到物理内存，可以便于后续分页的各种操作顺利进行。
        "ori $r12, $r0, 0x11",
        "lu52i.d $r12, $r12, -1792",
        "csrwr $r12, 0x181",

        // 强制 ECFG.VS=0，确保异常入口寻址语义与内核异常入口代码一致。
        "ori $r13, $r0, 0x7",
        "slli.d $r13, $r13, 16",
        "csrxchg $r0, $r13, 0x4",

        // 设置异常入口地址和 TLB refill 入口地址。根据 LoongArch64 的调用约定，
        // CPU 在捕获到异常时或者需要进行 TLB refill 时会自动分别跳转到 CSR 0xc
        // 和 CSR 0x88 中设置的地址，因此这里需要将它们设置为正确的函数入口地址。
        "la.abs $r12, {exception_entry}",
        "csrwr $r12, 0xc",
        "la.abs $r12, {tlbrentry}",
        // TLBR 入口在 DA=1,PG=0 模式取址，需写入物理地址（清除 DMW 顶部窗口位）。
        "slli.d $r12, $r12, 4",
        "srli.d $r12, $r12, 4",
        "csrwr $r12, 0x88",
        "la.abs $r12, {merrentry}",
        // MERR 入口同样在 DA=1,PG=0 模式取址，需写入物理地址。
        "slli.d $r12, $r12, 4",
        "srli.d $r12, $r12, 4",
        "csrwr $r12, 0x93",

        // 清零 CSR 0x182 和 0x183，确保 CPU 的一些特定功能处于默认状态，避免在
        // 后续操作中产生意外的行为。
        "csrwr $r0, 0x182",
        "csrwr $r0, 0x183",

        // 使能 FPU (FPE) 和 SIMD (SXE)。
        // LLVM 在 release 模式下会生成 LSX 向量指令。
        // EUEN_FPE = 0x1, EUEN_SXE = 0x2
        "ori $r12, $r0, 0x3",
        "csrwr $r12, 0x2",

        // 跳转到虚拟化启动入口，继续执行内核的初始化逻辑。
        "la.abs $r12, {entry}",
        "jirl $r0, $r12, 0",

        exception_entry = sym __loongarch_exception_entry,
        tlbrentry = sym __loongarch_tlb_refill_entry,
        merrentry = sym __loongarch_machine_error_entry,
        entry = sym _start_virtualized,
    )
}

/// LoongArch64 的虚拟化启动入口，接收 [`_start`] 的控制权，完成最小启动初始化。
///
/// # Safety
///
/// 该函数不应当被以除 [`_start`] 之外的任何调用者直接调用。
///
/// 与 `_start` 相比，这里不再新增硬件映射，而是为 Rust 代码准备一个可信执行环境：
/// 至少有一个临时栈，至少要把引导 ABI 参数完整传给常规函数调用约定。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start_virtualized() {
    naked_asm!(
        // 清零返回地址寄存器（$r1）和帧指针寄存器（$r22）。一方面是标记调用栈
        // 的终点，如果在后续发生了 panic 不至于使得 CPU 的运行逻辑跳出内核的
        // 控制范围，为了跳出异常而转移到计算机启动早期产生的 $r1 或 $r22 的垃
        // 圾值所对应的未知的内存地址；另一方面也便于在调试过程中观察调用栈的
        // 状态。
        "move $r1, $r0",
        "move $r22, $r0",

        // 设置临时栈。将 $r3 寄存器（栈指针寄存器，sp）设置为我们提前预留的一
        // 块内存区域地址。在 LoongArch64 的启动约定中，CPU 启动时并没有提前预
        // 留一个有效的栈，而后期的任何函数都对栈空间有要求，因此我们需要在这里
        // 预先设置一个临时栈，以便后续的函数调用能够正常进行。这个临时栈的地址
        // 是通过链接脚本预先定义的符号 `__tmp_stack_top`，它指向我们预留的栈空
        // 间的顶部。
        "la.abs $r3, __tmp_stack_top",

        // 跳转到预启动初始化函数，完成启动信息采集和 BSS 清零。
        // 此时 `$a0/$a1/$a2` 仍保持为启动 ABI 传入值，作为 `pre_boot_init` 的前
        // 三个形参直接传递，避免在 Rust 函数体内再去读取调用者保存寄存器。
        "bl {pre_boot_init}",

        // 跳转到内核加载器函数，继续执行内核的初始化逻辑。并且在跳转时不保留返
        // 回地址，防止后续的任何函数调用试图返回到这个入口点，确保内核的控制流
        // 程始终在内核的代码范围内。
        "la.abs $r12, {kernel_loader}",
        "jirl $r0, $r12, 0",

        pre_boot_init = sym pre_boot_init,
        kernel_loader = sym __kernel_arch_loader,
    )
}

/// 预启动初始化函数，负责采集启动信息并清零 BSS 段。
///
/// # Safety
///
/// 该函数不应当被以除 [`_start_virtualized`] 之外的任何调用者直接调用。
///
/// 这里必须先清零 BSS，再写入这些静态变量，否则后续 BSS 清理会把刚写入的启动信息
/// 覆盖掉，导致内核误判固件没有传参。
unsafe extern "C" fn pre_boot_init(efi_boot: usize, cmdline_ptr: usize, system_table_ptr: usize) {
    // 必须先清零 BSS，再写入静态变量，防止早期垃圾值产生干扰
    unsafe { clear_bss() };
    EFI_SYSTEM_TABLE_PTR.store(system_table_ptr, Ordering::Release);
    EFI_BOOT.store(efi_boot, Ordering::Release);
    CMDLINE_PTR.store(cmdline_ptr, Ordering::Release);
    crate::loongarch64::smp::init_boot_cpu_mapping();
}
