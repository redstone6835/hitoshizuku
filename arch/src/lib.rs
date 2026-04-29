#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

//! 架构实现层。
//!
//! 本 crate 按 ISA 组织具体实现，负责汇编引导、CSR 操作、异常入口、
//! 页表激活和中断控制等机制。

pub mod loongarch64;
pub mod riscv64;
