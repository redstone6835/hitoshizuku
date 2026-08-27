//! LoongArch64 架构支持总入口。
//!
//! 这个模块把 LoongArch64 相关的子模块组织成一个完整的平台实现，供上层
//! `kernel`、`general` 和各类子系统调用。阅读这个目录时，可以把它理解为
//! “从硬件启动到进入通用内核逻辑”的一条连续链路，主要包含以下几层：
//!
//! 1. `boot`：最早期入口。负责在汇编环境下建立 DMW、切换到 Rust 初始化逻辑。
//! 2. `init`：平台初始化主流程。负责解析固件数据、初始化分配器、建立正式页表、
//!    注册串口与控制台，并最终把系统带入可运行状态。
//! 3. `specific`：LoongArch64 的 CSR、异常码、寄存器位定义，以及一些地址转换
//!    辅助函数。这里集中保存“硬件位布局知识”，避免散落到其它模块。
//! 4. `paging` 与 `heap_vm`：分页相关实现。前者负责页表格式和 CSR 配置，后者
//!    负责内核堆虚拟地址空间的具体映射与解除映射。
//! 5. `trap`：异常、中断和 TLB refill 相关逻辑，连接汇编入口和 Rust 处理函数。
//! 6. `early_console`：正式控制台建立前的最小输出路径，用于启动阶段调试。
//! 7. `abi`：平台 ABI 转换层，用于把 LoongArch64/Linux 风格的整数参数翻译为
//!    内核内部的类型语义。
pub mod abi;
mod asid_tracker;
mod boot;
mod early_console;
mod elm_native;
mod heap_vm;
mod loader;
mod mem;
mod mm;
mod paging;
mod ptrace;
mod random_source;
mod sched_ctx;
mod smp;
mod specific;
pub mod syscall;
mod task;
mod tlb_shootdown;
pub mod trap;
mod user_abi;
pub mod vdso;

pub use boot::*;
pub use early_console::*;
pub use elm_native::{
    call_elm_native, call_elm_native_current_stack, elm_native_recovery_address, resume_elm_panic,
};
pub use heap_vm::*;
pub use loader::*;
pub use paging::*;
pub use ptrace::{
    BREAKPOINT_INSN as USER_BREAKPOINT_INSN, LINUX_FPREGSET_SIZE,
    read_linux_fpregs as read_user_linux_fpregs, task_frame as ptrace_task_frame,
    write_linux_fpregs as write_user_linux_fpregs,
};
pub use random_source::register as register_entropy_source;
pub use sched_ctx::register as register_sched_ctx;
pub use smp::{SecondaryCpuReport, start_secondary_cpus};
pub use specific::*;
pub use task::*;
pub use trap::*;
pub use user_abi::patch_interpreter_image;

/// 保存并关闭当前 CPU 的本地可屏蔽中断。
pub fn save_and_disable_local_interrupts() -> usize {
    // Safety: 该操作只修改当前 CPU 的 CRMD.IE，返回值由配对恢复入口消费。
    unsafe {
        let state = LoongArch64InterruptOps::save_interrupt_state();
        LoongArch64InterruptOps::disable_interrupts();
        state
    }
}

/// 恢复同一 CPU 上先前保存的本地中断状态。
pub fn restore_local_interrupts(state: usize) {
    // Safety: HAL 契约要求 state 来自同一 CPU 最近的配对保存操作。
    unsafe { LoongArch64InterruptOps::restore_interrupt_state(state) }
}

/// LoongArch 当前只向 coherent DMA 设备暴露该 HAL，因而无需额外 CMO。
pub unsafe fn dma_clean_range(_vaddr: usize, _len: usize) -> bool {
    true
}

/// LoongArch 当前只向 coherent DMA 设备暴露该 HAL，因而无需额外 CMO。
pub unsafe fn dma_invalidate_range(_vaddr: usize, _len: usize) -> bool {
    true
}

/// 固件没有提供 PCI 地址窗口时使用的平台默认 MMIO 范围。
pub fn default_pci_mmio_window() -> Option<core::ops::Range<u64>> {
    Some(0x4000_0000..0x8000_0000)
}

/// ACPI 不能单独证明 PCI DMA 一致性；LoongArch 平台默认保持关闭。
pub const fn acpi_pci_dma_coherent_default() -> bool {
    false
}

/// 未建立 IOMMU/地址转换契约前，不假定 PCI DMA 地址与物理地址恒等。
pub const fn acpi_pci_identity_dma_default() -> bool {
    false
}

/// 绕过通用日志路径输出一个板级启动标记。
pub fn raw_debug_byte(byte: u8) {
    // Safety: dbg_char 只使用板级调试 UART；参数寄存器约定由同模块启动汇编定义。
    unsafe {
        core::arch::asm!(
            "move $a3, {byte}",
            "bl {dbg_char}",
            byte = in(reg) byte as u64,
            dbg_char = sym dbg_char,
            options(nostack),
            clobber_abi("C"),
        );
    }
}

/// 绕过通用日志路径输出一个 16 位十六进制板级启动标记。
pub fn raw_debug_hex16(value: usize) {
    // Safety: dbg_hex16 只使用板级调试 UART；参数寄存器约定由同模块启动汇编定义。
    unsafe {
        core::arch::asm!(
            "move $a0, {value}",
            "bl {dbg_hex16}",
            value = in(reg) value as u64,
            dbg_hex16 = sym dbg_hex16,
            options(nostack),
            clobber_abi("C"),
        );
    }
}

/// LoongArch 当前没有通过任务扩展侧表保存的额外用户寄存器状态。
pub fn clone_user_task_extension(
    _key: sched::TaskExtKey,
    _src: &alloc::sync::Arc<dyn core::any::Any + Send + Sync>,
) -> Option<alloc::sync::Arc<dyn core::any::Any + Send + Sync>> {
    None
}

/// LoongArch 当前没有通过任务扩展侧表保存的额外用户寄存器状态。
pub fn reset_user_task_state(_task: &sched::Task) {}

/// LoongArch 当前的信号上下文已完整包含在基础 trap frame 中。
pub fn push_user_signal_state(
    _task: &alloc::sync::Arc<sched::Task>,
    _context: usize,
) -> Result<(), ()> {
    Ok(())
}

/// LoongArch 当前的信号上下文已完整包含在基础 trap frame 中。
pub fn pop_user_signal_state(_task: &alloc::sync::Arc<sched::Task>, _context: usize) {}

/// 用户线程入口意外返回时使用的最小 `exit(0)` 指令序列。
const _: () = assert!(syscall::nr::SYS_EXIT == 93);

pub const fn user_exit_stub_code() -> &'static [u8] {
    &[0x0b, 0x74, 0x81, 0x03, 0x00, 0x00, 0x2b, 0x00]
}

/// Linux 64 位 `epoll_event.data` 在本架构上的字节偏移。
pub const fn linux_epoll_event_data_offset() -> usize {
    8
}
