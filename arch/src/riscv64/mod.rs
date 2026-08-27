//! RISC-V64 架构实现。
//!
//! 本模块是 RISC-V64 平台的顶层命名空间，组织以下子模块：
//!
//! - **启动**：`boot`（汇编入口）→ `loader`（Rust 初始化）→ `efi_stub`（UEFI panic stub）
//! - **异常与中断**：`trap`（入口/分发）、`trap_frame`（寄存器帧）、`syscall`（ecall 处理）
//! - **内存管理**：`addr`（地址常量）、`mm`（用户页表）、`paging`（页表操作）、`heap_vm`（内核堆映射）
//! - **任务**：`task`（上下文切换）、`sched_ctx`（调度器上下文）
//! - **平台服务**：`time`（时钟）、`vdso`、`early_console`（早期串口）、`specific`（per-hart 状态）
//! - **辅助**：`abi`（Linux ABI 编解码）、`csr`（CSR 常量）、`random_source`（熵源）

// ── 启动 ──────────────────────────────────────────────────────────────────────

// CSR 访问宏必须在其他模块之前定义，确保 macro_use 对后续模块可见
#[macro_use]
pub mod csr;

pub mod boot;
pub(crate) mod efi_stub;
mod external_irq;
pub mod loader; // RISC-V 不走 UEFI，仅提供 panic stub

// ── 异常与中断 ────────────────────────────────────────────────────────────────

mod elm_native;
pub mod syscall;
pub mod trap;
pub mod trap_frame;

// ── 内存管理 ──────────────────────────────────────────────────────────────────

pub mod addr;
pub mod heap_vm;
mod mem;
pub mod mm;
pub mod paging;
mod ptrace;

// ── 任务 ──────────────────────────────────────────────────────────────────────

pub mod sched_ctx;
pub mod smp;
pub mod task;

// ── 平台服务 ──────────────────────────────────────────────────────────────────

pub mod early_console;
pub mod sbi;
pub mod specific;
pub mod time;
pub mod vdso;
pub mod vector;

// ── 辅助 ──────────────────────────────────────────────────────────────────────

pub mod abi;
mod random_source;

// ── Re-exports ────────────────────────────────────────────────────────────────
//
// `specific` 聚合了 csr / trap_frame / addr / time 的全量符号并追加别名常量，
// 通过 glob re-export 让上层可以 `use crate::riscv64::*` 统一引用 arch 符号。

pub use early_console::e_write_bytes;
pub use elm_native::{
    call_elm_native, call_elm_native_current_stack, elm_native_recovery_address, resume_elm_panic,
};
pub use heap_vm::activate_kernel_page_table;
pub use mm::user_copy::set_sum;
pub use ptrace::{
    BREAKPOINT_INSN as USER_BREAKPOINT_INSN, LINUX_FPREGSET_SIZE,
    read_linux_fpregs as read_user_linux_fpregs, task_frame as ptrace_task_frame,
    write_linux_fpregs as write_user_linux_fpregs,
};
pub use random_source::register as register_entropy_source;
pub use sched_ctx::register as register_sched_ctx;
pub use smp::{SecondaryCpuReport, start_secondary_cpus};
pub use specific::*;
pub use task::Riscv64TaskOps;
pub use vector::user_hwcap;

/// 保存并关闭当前 hart 的本地可屏蔽中断。
pub fn save_and_disable_local_interrupts() -> usize {
    // Safety: 该操作只修改当前 hart 的 SIE 状态，返回值由配对恢复入口消费。
    unsafe { trap::Riscv64InterruptOps::save_and_disable() }
}

/// 恢复同一 hart 上先前保存的本地中断状态。
pub fn restore_local_interrupts(state: usize) {
    // Safety: HAL 契约要求 state 来自同一 hart 最近的配对保存操作。
    unsafe { trap::Riscv64InterruptOps::restore_interrupt_state(state) }
}

/// 为设备读取发布 CPU 写入的 DMA 缓冲区。
pub unsafe fn dma_clean_range(vaddr: usize, len: usize) -> bool {
    // Safety: 由调用方保证范围属于有效、可访问的内核映射。
    unsafe { clean_dcache_range(vaddr, len) }
}

/// 在设备写入前后失效 CPU 对 DMA 缓冲区的缓存副本。
pub unsafe fn dma_invalidate_range(vaddr: usize, len: usize) -> bool {
    // Safety: 由调用方保证范围有效，且没有必须保留的 CPU 脏数据。
    unsafe { invalidate_dcache_range(vaddr, len) }
}

/// 固件没有提供 PCI 地址窗口时使用的平台默认 MMIO 范围。
pub fn default_pci_mmio_window() -> Option<core::ops::Range<u64>> {
    Some(0x4000_0000..0x8000_0000)
}

/// ACPI 不能单独证明 PCI DMA 一致性；RISC-V 平台默认保持关闭。
pub const fn acpi_pci_dma_coherent_default() -> bool {
    false
}

/// 未建立 IOMMU/地址转换契约前，不假定 PCI DMA 地址与物理地址恒等。
pub const fn acpi_pci_identity_dma_default() -> bool {
    false
}

/// 板级裸调试输出；RISC-V 平台当前没有独立于早期控制台的通道。
pub fn raw_debug_byte(_byte: u8) {}

/// 板级裸调试输出；RISC-V 平台当前没有独立于早期控制台的通道。
pub fn raw_debug_hex16(_value: usize) {}

/// fork 时处理仅由架构后端理解的任务扩展状态。
pub fn clone_user_task_extension(
    key: sched::TaskExtKey,
    src: &alloc::sync::Arc<dyn core::any::Any + Send + Sync>,
) -> Option<alloc::sync::Arc<dyn core::any::Any + Send + Sync>> {
    vector::clone_task_extension(key, src)
}

/// exec/exit 时清理仅由架构后端拥有的用户任务状态。
pub fn reset_user_task_state(task: &sched::Task) {
    vector::clear_for_task(task);
}

/// 在构造 Linux 信号帧前保存架构扩展用户状态。
pub fn push_user_signal_state(
    task: &alloc::sync::Arc<sched::Task>,
    context: usize,
) -> Result<(), ()> {
    vector::push_signal_snapshot(task, context)
}

/// 从 Linux 信号帧恢复后重新安装架构扩展用户状态。
pub fn pop_user_signal_state(task: &alloc::sync::Arc<sched::Task>, context: usize) {
    vector::pop_signal_snapshot(task, context);
}

/// RISC-V64 动态链接器不需要架构兼容补丁。
pub fn patch_interpreter_image(_interp: &str, _bytes: &mut [u8]) {}

/// 用户线程入口意外返回时使用的最小 `exit(0)` 指令序列。
const _: () = assert!(syscall::nr::SYS_EXIT == 93);

pub const fn user_exit_stub_code() -> &'static [u8] {
    &[0x93, 0x08, 0xd0, 0x05, 0x73, 0x00, 0x00, 0x00]
}

/// Linux 64 位 `epoll_event.data` 在本架构上的字节偏移。
pub const fn linux_epoll_event_data_offset() -> usize {
    8
}
