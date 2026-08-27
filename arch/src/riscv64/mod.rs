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

pub use elm_native::{
    call_elm_native, call_elm_native_current_stack, elm_native_recovery_address, resume_elm_panic,
};
pub use heap_vm::activate_kernel_page_table;
pub use mm::user_copy::set_sum;
pub use random_source::register as register_entropy_source;
pub use sched_ctx::register as register_sched_ctx;
pub use smp::{SecondaryCpuReport, start_secondary_cpus};
pub use specific::*;
pub use task::Riscv64TaskOps;
