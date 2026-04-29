#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

//! 通用基础设施与标准接口层。
//!
//! 本 crate 定义平台无关 trait，并承载依赖这些 trait 的通用算法。
//! 具体 ISA 实现只能通过 trait 契约接入。
