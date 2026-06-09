//! RISC-V64 架构共享定义（重导出 hub）。
//!
//! 实际实现拆分在 csr.rs / trap_frame.rs / addr.rs / time.rs 中。
//! 本模块统一重导出所有公共符号，保持 `use crate::riscv64::specific::*` 的
//! 对外接口不变。HartLocal（per-hart 数据）因跨模块性质保留在此处。

// 子模块重导出
pub use crate::riscv64::csr::*;
pub use crate::riscv64::trap_frame::*;
pub use crate::riscv64::addr::*;
pub use crate::riscv64::time::*;

// ── Per-hart 数据 ─────────────────────────────────────────────────────────────
//
// 每个 hart 通过 tp 寄存器指向自己的 HartLocal 实例。boot 阶段在
// pre_boot_init 中初始化 boot hart 的实例并写入 tp。
// SMP 时 secondary hart 由 sbi_hart_start 唤醒，进入 secondary_entry 后
// 初始化各自的 HartLocal 并写入 tp。

/// 支持的最大 hart 数。SMP 唤醒时不得超过此值。
pub const MAX_HARTS: usize = 8;

/// 每个 hart 的本地数据，通过 tp 寄存器寻址。
#[repr(C)]
pub struct HartLocal {
    pub hart_id: usize,
    pub kernel_stack_top: usize,
    pub preempt_count: usize,
    /// 中断栈栈顶（高地址端）。trap entry from_kernel 路径切换到此栈。
    pub irq_stack_top: usize,
}

/// 中断栈大小：8 KiB（足够处理嵌套中断 + Rust handler 栈帧）。
pub const IRQ_STACK_SIZE: usize = 8192;

/// HartLocal.irq_stack_top 在结构体中的字节偏移。
pub const HART_LOCAL_IRQ_STACK_TOP_OFF: usize = 24; // 3 * size_of::<usize>()

/// 全部 hart 的中断栈（静态分配，按 hart index 索引）。
#[repr(C, align(16))]
pub(crate) struct IrqStack(pub(crate) [u8; IRQ_STACK_SIZE]);
pub(crate) static mut IRQ_STACKS: [IrqStack; MAX_HARTS] = {
    const EMPTY: IrqStack = IrqStack([0; IRQ_STACK_SIZE]);
    [EMPTY; MAX_HARTS]
};

/// 全部 hart 的 HartLocal 实例（按 hart index 索引）。
/// 启动 hart 使用 index 0，辅助 hart 使用 1..MAX_HARTS。
///
/// # Safety
///
/// 仅在 boot 阶段由 boot hart 初始化，之后通过 tp 寄存器只读访问。
pub(crate) static mut HART_LOCALS: [HartLocal; MAX_HARTS] = {
    const EMPTY: HartLocal = HartLocal {
        hart_id: 0,
        kernel_stack_top: 0,
        preempt_count: 0,
        irq_stack_top: 0,
    };
    [EMPTY; MAX_HARTS]
};

/// 向后兼容：boot hart 的 HartLocal 引用（实际指向 HART_LOCALS[0]）。
///
/// # Safety
///
/// 仅在 boot 阶段单核初始化时写入（pre_boot_init），之后通过 tp 寄存器只读访问。
#[inline]
pub(crate) unsafe fn boot_hart_local_ptr() -> *mut HartLocal {
    // Safety: 调用方保证在单核 boot 阶段调用（pre_boot_init），此时无并发访问 HART_LOCALS。
    unsafe { core::ptr::addr_of_mut!(HART_LOCALS) as *mut HartLocal }
}

#[inline]
pub fn current_hart() -> &'static HartLocal {
    let ptr: *const HartLocal;
    unsafe { core::arch::asm!("mv {}, tp", out(reg) ptr, options(nomem, nostack)) };
    debug_assert!(!ptr.is_null(), "tp not initialized: current_hart() called before pre_boot_init");
    unsafe { &*ptr }
}

#[inline]
pub fn current_cpu_id() -> usize {
    current_hart().hart_id
}

// ── ISA 扩展能力检测 ──────────────────────────────────────────────────────────

use core::sync::atomic::{AtomicBool, AtomicUsize};

/// 硬件是否支持 Zicboz（cbo.zero 指令）。由 loader DTB 解析设置。
pub static HAS_ZICBOZ: AtomicBool = AtomicBool::new(false);

/// cbo.zero 操作的 cache block 大小（字节）。Zicboz spec 默认 64。
pub static CBO_BLOCK_SIZE: AtomicUsize = AtomicUsize::new(64);

/// 高效清零一页内存。
///
/// 如果硬件支持 Zicboz，使用 `cbo.zero` 逐 cache block 清零（跳过 read-for-ownership）；
/// 否则 fallback 到 sd 批量写零。
///
/// # Safety
///
/// `vaddr` 必须是有效的、已映射的、页对齐的虚拟地址，且 `len` 是 8 的倍数。
#[inline]
pub unsafe fn zero_memory_fast(vaddr: usize, len: usize) {
    if HAS_ZICBOZ.load(core::sync::atomic::Ordering::Relaxed) {
        let block_size = CBO_BLOCK_SIZE.load(core::sync::atomic::Ordering::Relaxed);
        let mut offset = 0;
        while offset < len {
            unsafe {
                // cbo.zero rs1 编码：imm[11:0]=0x004, funct3=010, rd=x0, opcode=0x0F
                core::arch::asm!(
                    ".insn i 0x0F, 0x2, x0, {addr}, 0x004",
                    addr = in(reg) vaddr + offset,
                    options(nostack, preserves_flags)
                );
            }
            offset += block_size;
        }
    } else {
        // 回退方案：sd 批量写零
        let mut ptr = vaddr as *mut u64;
        let end = (vaddr + len) as *mut u64;
        while ptr < end {
            unsafe { core::ptr::write_volatile(ptr, 0) };
            ptr = unsafe { ptr.add(1) };
        }
    }
}


