//! RISC-V64 架构共享定义（重导出 hub）。
//!
//! 实际实现拆分在 csr.rs / trap_frame.rs / addr.rs / time.rs 中。
//! 本模块统一重导出所有公共符号，保持 `use crate::riscv64::specific::*` 的
//! 对外接口不变。HartLocal（per-hart 数据）因跨模块性质保留在此处。

use core::mem::offset_of;

// 子模块重导出
pub use crate::riscv64::addr::*;
pub use crate::riscv64::csr::*;
pub use crate::riscv64::time::*;
pub use crate::riscv64::trap_frame::*;

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
    pub logical_id: usize,
    pub kernel_stack_top: usize,
    pub preempt_count: usize,
    pub kernel_gp: usize,
    /// 紧急栈栈顶（高地址端）。仅最终 return-to-user 窗口中的 S-mode trap 使用。
    pub irq_stack_top: usize,
    /// trap 入口在使用临时寄存器前保存原始 t4。
    pub trap_entry_t4: usize,
    /// trap 入口在读取 SPP 前暂存原始 t5；仅由当前 hart、关中断窗口访问。
    pub trap_entry_t5: usize,
    /// `csrrw t6, sscratch, t6` 后暂存被覆盖的原始 t6。
    pub trap_entry_t6: usize,
    /// 仅标记 trap 入口/返回的脆弱汇编窗口：0=稳定，1=窗口内，2=已进入 fatal。
    /// Rust handler 执行前必须清零，避免调度切换期间产生伪嵌套。
    pub trap_entry_state: usize,
    /// 每次内核任务上下文切换递增。syscall fast return 用它判断 live FPU
    /// 寄存器是否仍属于入口时的任务；仅由当前 hart 在关中断调度路径写入。
    pub context_switch_seq: usize,
}
/// 最终 return-to-user 窗口使用的紧急栈可用大小。
///
/// debug 构建中的 trap/scheduler/allocator 调用链可能超过 8 KiB，因此保留 32 KiB；
/// 普通 kernel-origin trap 继续使用任务自身的 64 KiB 内核栈。
pub const IRQ_STACK_SIZE: usize = 32 * 1024;
/// 紧急栈低地址端的缓冲区，避免轻微越界直接覆盖相邻静态对象。
pub const IRQ_STACK_GUARD_SIZE: usize = 4 * 1024;
pub const IRQ_STACK_ALLOCATION_SIZE: usize = IRQ_STACK_GUARD_SIZE + IRQ_STACK_SIZE;

/// 汇编入口使用的 HartLocal 字段偏移。
pub const HART_LOCAL_KERNEL_STACK_TOP_OFF: usize = offset_of!(HartLocal, kernel_stack_top);
pub const HART_LOCAL_KERNEL_GP_OFF: usize = offset_of!(HartLocal, kernel_gp);
pub const HART_LOCAL_IRQ_STACK_TOP_OFF: usize = offset_of!(HartLocal, irq_stack_top);
pub const HART_LOCAL_TRAP_ENTRY_T4_OFF: usize = offset_of!(HartLocal, trap_entry_t4);
pub const HART_LOCAL_TRAP_ENTRY_T5_OFF: usize = offset_of!(HartLocal, trap_entry_t5);
pub const HART_LOCAL_TRAP_ENTRY_T6_OFF: usize = offset_of!(HartLocal, trap_entry_t6);
pub const HART_LOCAL_TRAP_ENTRY_STATE_OFF: usize = offset_of!(HartLocal, trap_entry_state);
pub const HART_LOCAL_CONTEXT_SWITCH_SEQ_OFF: usize = offset_of!(HartLocal, context_switch_seq);

/// 全部 hart 的紧急栈（静态分配，按 hart index 索引）。低地址端第一页仅作缓冲。
#[repr(C, align(4096))]
pub(crate) struct IrqStack(pub(crate) [u8; IRQ_STACK_ALLOCATION_SIZE]);
pub(crate) static mut IRQ_STACKS: [IrqStack; MAX_HARTS] = {
    const EMPTY: IrqStack = IrqStack([0; IRQ_STACK_ALLOCATION_SIZE]);
    [EMPTY; MAX_HARTS]
};

/// 全部 hart 的 HartLocal 实例（按 hart index 索引）。
/// 启动 hart 使用 index 0，辅助 hart 使用 1..MAX_HARTS。
///
/// # Safety
///
/// boot 阶段初始化固定字段；运行期仅当前 hart 通过 tp 裸指针或 trap 汇编更新
/// `kernel_stack_top` / `trap_entry_t4..t6` / `trap_entry_state`，不得为整块对象构造
/// 长期共享引用。
pub(crate) static mut HART_LOCALS: [HartLocal; MAX_HARTS] = {
    const EMPTY: HartLocal = HartLocal {
        hart_id: 0,
        logical_id: 0,
        kernel_stack_top: 0,
        preempt_count: 0,
        kernel_gp: 0,
        irq_stack_top: 0,
        trap_entry_t4: 0,
        trap_entry_t5: 0,
        trap_entry_t6: 0,
        trap_entry_state: 0,
        context_switch_seq: 0,
    };
    [EMPTY; MAX_HARTS]
};

/// boot hart 的 HartLocal 裸指针（实际指向 HART_LOCALS[0]）。
///
/// # Safety
///
/// 仅在 boot 阶段单核初始化时取得；运行期访问改由 tp 裸指针完成。
#[inline]
pub(crate) unsafe fn boot_hart_local_ptr() -> *mut HartLocal {
    // Safety: 调用方保证在单核 boot 阶段调用（pre_boot_init），此时无并发访问 HART_LOCALS。
    core::ptr::addr_of_mut!(HART_LOCALS) as *mut HartLocal
}

/// 初始化辅助 hart 的本地状态并返回应写入 `tp` 的指针。
///
/// # Safety
///
/// `logical_id` 必须唯一且当前尚未被其它 hart 使用；调用期间该槽位不可并发访问。
pub(crate) unsafe fn init_secondary_hart_local(
    logical_id: usize,
    hart_id: usize,
    kernel_gp: usize,
) -> *mut HartLocal {
    assert!(logical_id > 0 && logical_id < MAX_HARTS);
    let locals = core::ptr::addr_of_mut!(HART_LOCALS) as *mut HartLocal;
    let local = unsafe { locals.add(logical_id) };
    let irq_stacks = core::ptr::addr_of!(IRQ_STACKS) as usize;
    unsafe {
        local.write(HartLocal {
            hart_id,
            logical_id,
            kernel_stack_top: 0,
            preempt_count: 0,
            kernel_gp,
            irq_stack_top: irq_stacks + (logical_id + 1) * IRQ_STACK_ALLOCATION_SIZE,
            trap_entry_t4: 0,
            trap_entry_t5: 0,
            trap_entry_t6: 0,
            trap_entry_state: 0,
            context_switch_seq: 0,
        });
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
    local
}
/// 读取 tp 中的当前 HartLocal 裸指针。
///
/// `kernel_stack_top` 和 `trap_entry_t5` 会被汇编入口异步修改，因此不能为整块
/// HartLocal 构造长期 `&'static` 共享引用；所有运行期访问都通过裸指针完成。
#[inline]
pub(crate) fn current_hart_ptr() -> *mut HartLocal {
    let ptr: *mut HartLocal;
    unsafe { core::arch::asm!("mv {}, tp", out(reg) ptr, options(nomem, nostack)) };
    debug_assert!(
        !ptr.is_null(),
        "tp not initialized: HartLocal accessed before pre_boot_init"
    );
    ptr
}

#[inline]
pub fn current_kernel_stack_top() -> usize {
    let ptr = current_hart_ptr();
    unsafe { core::ptr::addr_of!((*ptr).kernel_stack_top).read_volatile() }
}

/// 读取当前 hart 已完成的内核任务切换序列。
///
/// 该值只用于判断一次 trap 处理期间是否失去过当前 hart 的执行所有权，不能作为
/// 跨 hart 的全局顺序号。调用方必须保留入口值并只比较是否相等。
#[inline]
pub(crate) fn current_context_switch_sequence() -> usize {
    let ptr = current_hart_ptr();
    unsafe { core::ptr::addr_of!((*ptr).context_switch_seq).read_volatile() }
}

/// 更新当前 hart 上正在运行任务的内核栈顶。
///
/// # Safety
///
/// 只能由当前 hart 在调度切换或用户上下文安装期间调用；调用方必须保证不会由其它
/// hart 并发写当前 HartLocal。
#[inline]
pub unsafe fn set_current_kernel_stack_top(stack_top: usize) {
    let ptr = current_hart_ptr();
    unsafe { core::ptr::addr_of_mut!((*ptr).kernel_stack_top).write_volatile(stack_top) };
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
}

/// 最终返回窗口的紧急栈使用完毕后检查当前 SP 是否仍位于映射区。
/// 低地址 guard page 在正式页表建立后保持未映射，不能再通过读取零缓冲检查。
#[unsafe(no_mangle)]
pub extern "C" fn riscv64_check_irq_stack_guard() {
    let ptr = current_hart_ptr();
    let top = unsafe { core::ptr::addr_of!((*ptr).irq_stack_top).read_volatile() };
    if top == 0 {
        return;
    }

    let sp: usize;
    unsafe { core::arch::asm!("mv {}, sp", out(reg) sp, options(nomem, nostack)) };
    let bottom = top.saturating_sub(IRQ_STACK_SIZE);
    // 向下增长的栈在尚未压入任何内容时合法 SP 就等于 `top`。RISC-V 的
    // `call` 只写 ra，不会隐式压栈，因此本叶函数可能原样观察到该边界值。
    // 低地址端则指向最后一段可用栈空间，仍属于映射范围。
    if sp < bottom || sp > top {
        riscv64_double_fault();
    }
}

/// trap 入口或最终返回窗口再次 fault 时使用的最小停机路径。
/// 不格式化、不分配、不获取调度器或普通日志锁。
#[unsafe(no_mangle)]
pub extern "C" fn riscv64_double_fault() -> ! {
    crate::riscv64::early_console::e_write_bytes(b"\n[arch][trap] fatal nested trap\n");
    crate::riscv64::sbi::emergency_shutdown()
}

#[unsafe(no_mangle)]
pub extern "C" fn riscv64_fatal_trap_shutdown() -> ! {
    crate::riscv64::early_console::e_write_bytes(b"\n[arch][trap] fatal trap shutdown\n");
    crate::riscv64::sbi::emergency_shutdown()
}

#[inline]
pub fn current_cpu_id() -> usize {
    let logical_id: usize;
    unsafe {
        core::arch::asm!(
            "ld {logical_id}, {offset}(tp)",
            logical_id = out(reg) logical_id,
            offset = const offset_of!(HartLocal, logical_id),
            options(readonly, nostack, preserves_flags),
        );
    }
    debug_assert!(logical_id < sched::NR_CPUS);
    logical_id.min(sched::NR_CPUS - 1)
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
