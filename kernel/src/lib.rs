#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

//! 内核策略与集成层。
//!
//! 本 crate 负责内核入口、子系统编排、系统调用分发和运行期策略。
//! 架构能力只能通过 `hal` 进入本层。
