#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

//! 统一架构接口层。
//!
//! 本 crate 包装 `arch` 的具体实现，向 `kernel` 暴露统一的架构能力接口。
