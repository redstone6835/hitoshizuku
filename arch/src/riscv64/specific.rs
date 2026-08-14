//! RISC-V64 架构共享定义（重导出 hub）。
//!
//! 实际实现拆分在 csr.rs / trap_frame.rs / addr.rs / time.rs 中。
//! 本模块统一重导出所有公共符号，保持 `use crate::riscv64::specific::*` 的
//! 对外接口不变。HartLocal（per-hart 数据）因跨模块性质保留在此处。

use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, Ordering};

/// 对普通内存、DMA 访问与设备 MMIO 执行一次保守的全顺序屏障。
///
/// 驱动在发布 DMA descriptor 后敲 doorbell，或观察到设备完成标志后读取 DMA
/// 数据时使用该入口；ISA 细节留在 arch 层，ELM 驱动不嵌入目标专属汇编。
#[inline]
pub fn device_io_barrier() {
    // Safety: `fence iorw, iorw` 只约束当前 hart 的访存/设备访问顺序，不访问
    // 任意地址，也不改变寄存器或特权状态。
    unsafe { core::arch::asm!("fence iorw, iorw", options(nostack)) };
}

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
pub const MAX_HARTS: usize = 12;
/// context-switch token 的 hart 编码步长；低 4 位保留 logical hart id。
pub const CONTEXT_SWITCH_TOKEN_STRIDE: usize = 16;
const _: () = assert!(MAX_HARTS <= CONTEXT_SWITCH_TOKEN_STRIDE);

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
    /// 每次内核任务上下文切换按固定步长递增，低位编码 logical hart id。
    /// syscall fast return 用它判断 live FPU 是否仍属于入口任务；跨 hart 迁移时
    /// token 也必然变化。仅由当前 hart 在关中断调度路径写入。
    pub context_switch_seq: usize,
    /// 调度器 current 槽所持 `Arc<Task>` 的裸指针镜像。
    ///
    /// 与 Linux 的 `tp -> current/thread_info` 相同，只允许当前 hart 借用；
    /// 跨执行边界持有时必须在 sched 层显式提升为拥有型 `Arc`。
    pub current_task: usize,
    /// 当前 CpuSchedState 中聚合返回工作 hint 的稳定地址。
    pub cpu_user_return_work: usize,
}
const _: () = assert!(offset_of!(HartLocal, logical_id) == size_of::<usize>());
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
pub const HART_LOCAL_CURRENT_TASK_OFF: usize = offset_of!(HartLocal, current_task);
pub const HART_LOCAL_CPU_USER_RETURN_WORK_OFF: usize = offset_of!(HartLocal, cpu_user_return_work);

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
        current_task: 0,
        cpu_user_return_work: 0,
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
            context_switch_seq: logical_id,
            current_task: 0,
            cpu_user_return_work: 0,
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
#[cfg(feature = "performance-profile")]
pub(crate) fn current_context_switch_sequence() -> usize {
    let ptr = current_hart_ptr();
    unsafe { core::ptr::addr_of!((*ptr).context_switch_seq).read_volatile() }
}

/// 读取当前 hart 的 borrowed-current 裸指针。
#[inline(always)]
pub(crate) fn current_task_ptr() -> *const sched::Task {
    let ptr = current_hart_ptr();
    unsafe { core::ptr::addr_of!((*ptr).current_task).read_volatile() as *const sched::Task }
}

#[inline(always)]
fn current_cpu_user_return_work_ptr() -> *const AtomicU32 {
    let ptr: usize;
    // Safety: tp 始终指向当前 hart 的 HartLocal；固定偏移落在其中已由调度发布
    // 路径写入的 usize 槽。直接以 tp 为基址可避免编译器额外生成一次 mv。
    unsafe {
        core::arch::asm!(
            "ld {ptr}, {offset}(tp)",
            ptr = out(reg) ptr,
            offset = const HART_LOCAL_CPU_USER_RETURN_WORK_OFF,
            options(nostack, readonly),
        );
    }
    ptr as *const AtomicU32
}

/// 普通 syscall 返回热路径读取本 CPU 聚合工作 hint。
#[inline(always)]
pub(crate) fn current_cpu_user_return_work_pending_relaxed() -> bool {
    let ptr = current_cpu_user_return_work_ptr();
    debug_assert!(!ptr.is_null(), "CPU user-return work pointer is null");
    // Safety: publish_current_task 在任务进入用户态前安装指向静态 CpuSchedState
    // AtomicU32 的非空地址，且该对象在内核运行期不会移动或释放。
    unsafe { (*ptr).load(Ordering::Relaxed) != 0 }
}

/// 返回慢路径与 CPU 工作生产者建立 Acquire/Release 同步。
#[inline]
pub(crate) fn current_cpu_user_return_work_pending_acquire() -> bool {
    let ptr = current_cpu_user_return_work_ptr();
    debug_assert!(!ptr.is_null(), "CPU user-return work pointer is null");
    // Safety: 与 relaxed 读取相同，ptr 指向内核运行期稳定的 AtomicU32；Acquire
    // 只用于和返回工作生产者的 Release 发布建立同步。
    unsafe { (*ptr).load(Ordering::Acquire) != 0 }
}

/// 更新当前 hart 的 borrowed-current 裸指针。
///
/// # Safety
/// `task_ptr` 必须指向调度器 current 槽仍持有强引用的任务；`cpu_work_ptr` 必须
/// 指向静态 CpuSchedState 内的 AtomicU32。只能由当前 hart 在发布 next 时调用。
#[inline(always)]
pub(crate) unsafe fn set_current_task_ptr(task_ptr: usize, cpu_work_ptr: usize) {
    let ptr = current_hart_ptr();
    // Safety: 调用方保证当前 hart 独占写自己的 HartLocal，两个值均已由调度器
    // 发布且生命周期覆盖 next 的整个运行区间。
    unsafe {
        core::ptr::addr_of_mut!((*ptr).current_task).write_volatile(task_ptr);
        core::ptr::addr_of_mut!((*ptr).cpu_user_return_work).write_volatile(cpu_work_ptr);
    }
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
    // logical_id 只在 boot/AP 初始化阶段写入固定 HartLocal，运行期与 Linux 的
    // thread_info.cpu 一样视为可信 per-CPU 字段。
    logical_id
}

// ── ISA 扩展能力检测 ──────────────────────────────────────────────────────────

use core::sync::atomic::{AtomicBool, AtomicUsize};

/// 硬件是否支持 Zicboz（cbo.zero 指令）。由 loader DTB 解析设置。
pub(crate) static HAS_ZICBOZ: AtomicBool = AtomicBool::new(false);

/// 硬件是否在所有可用 hart 上支持 Zicbom。
pub static HAS_ZICBOM: AtomicBool = AtomicBool::new(false);

/// 硬件是否在所有可用 hart 上支持 Zicbop。
pub static HAS_ZICBOP: AtomicBool = AtomicBool::new(false);

/// `cbo.zero` 操作的 cache block 大小（字节）。
///
/// 只有 [`HAS_ZICBOZ`] 发布为 `true` 后该值才可消费；loader 会在
/// 发布前校验 DT 中所有 hart 的块大小一致且可安全用于页清零。
pub static CBO_BLOCK_SIZE: AtomicUsize = AtomicUsize::new(64);

/// `cbo.clean/flush/inval` 的 cache block 大小（字节）。
pub static CBOM_BLOCK_SIZE: AtomicUsize = AtomicUsize::new(0);

/// `prefetch.*` 的 cache block 大小（字节）。
pub static CBOP_BLOCK_SIZE: AtomicUsize = AtomicUsize::new(0);

/// 高效清零一页内存。
///
/// Linux `clear_page` 采用同样的 16 块展开，避免每个 cache block 都执行一次
/// 地址计算和循环分支。
#[inline(always)]
unsafe fn cbo_zero_16(mut addr: usize, block_size: usize) -> usize {
    // Safety: 调用方保证从 addr 开始的 16 个 cache block 均在独占可写范围内。
    unsafe {
        core::arch::asm!(
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
            "add {addr}, {addr}, {block_size}",
            addr = inout(reg) addr,
            block_size = in(reg) block_size,
            options(nostack, preserves_flags),
        );
    }
    addr
}

/// 高效清零一段完整页面内存。
///
/// 如果所有 hart 都支持 Zicboz，使用 `cbo.zero` 跳过 read-for-ownership；
/// 否则使用八路展开的普通 `sd`。loader 在发布能力位前已经验证所有启用 hart
/// 的 cache block 大小一致、合法且不超过基础页，因此热路径不重复做除法校验。
///
/// # Safety
///
/// `vaddr` 必须是有效的、已映射的、基础页对齐虚拟地址，`len` 必须是基础页
/// 大小的整数倍，且 `[vaddr, vaddr + len)` 不得回绕。
#[inline]
pub unsafe fn zero_memory_fast(vaddr: usize, len: usize) {
    let end = vaddr + len;
    if HAS_ZICBOZ.load(core::sync::atomic::Ordering::Acquire) {
        let block_size = CBO_BLOCK_SIZE.load(core::sync::atomic::Ordering::Relaxed);
        debug_assert!(block_size.is_power_of_two());
        debug_assert!(block_size <= allocator::PAGE_SIZE);
        debug_assert_eq!(vaddr % block_size, 0);
        debug_assert_eq!(len % allocator::PAGE_SIZE, 0);
        let batch_size = block_size * 16;
        let mut addr = vaddr;
        while end - addr >= batch_size {
            // Safety: 本轮 16 个 cache block 全部落在独占范围内。
            addr = unsafe { cbo_zero_16(addr, block_size) };
        }
        while addr < end {
            // Safety: loader 不变量与基础页对齐契约保证完整 cache block 未越界。
            unsafe {
                core::arch::asm!(
                    ".insn i 0x0f, 0x2, x0, {addr}, 0x004",
                    addr = in(reg) addr,
                    options(nostack, preserves_flags),
                );
            }
            addr += block_size;
        }
        return;
    }

    // 显式汇编防止编译器把展开循环重新收缩为通用 memset。
    let mut ptr = vaddr as *mut u64;
    while end - ptr as usize >= 64 {
        // Safety: 循环条件保证 8 个 u64 存储都位于调用方提供的可写范围内。
        unsafe {
            core::arch::asm!(
                "sd zero, 0({ptr})",
                "sd zero, 8({ptr})",
                "sd zero, 16({ptr})",
                "sd zero, 24({ptr})",
                "sd zero, 32({ptr})",
                "sd zero, 40({ptr})",
                "sd zero, 48({ptr})",
                "sd zero, 56({ptr})",
                "addi {ptr}, {ptr}, 64",
                ptr = inout(reg) ptr,
                options(nostack, preserves_flags),
            );
        }
    }
    while (ptr as usize) < end {
        // Safety: 剩余范围按 u64 对齐且长度是 8 的倍数。
        unsafe {
            core::ptr::write_volatile(ptr, 0);
            ptr = ptr.add(1);
        }
    }
}
