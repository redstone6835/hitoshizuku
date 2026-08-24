#![no_std]
#![warn(missing_docs)]

//! `language-runtime` ELM 使用的稳定、语言无关 ABI。
//!
//! 本 crate 只定义固定布局协议，不依赖内核、`elm` crate、分配器或任何具体语言运行时。
//! V1 及兼容扩展只允许固定宽度整数、枚举值、状态码、opaque handle 和定长字节缓冲区跨越边界；
//! Rust 引用、指针、容器、trait object 以及托管语言对象都不属于该 ABI。
//! IRQ 也只通过设备层预授权的 opaque source 和有界事件计数暴露，硬件 IRQ 编号、内核
//! handler 与函数指针不会进入 wire。
//!
//! 每个版本的结构尺寸要求精确匹配。后续扩展必须发布新 ABI 版本或新结构类型，消费者不得
//! 读取未经当前版本定义的额外字节。运行时必须先调用对应 `validate` 方法，再使用任何输入。

pub mod backend;
pub mod delegation;
pub mod ids;
pub mod request;
pub mod resource;
pub mod status;
pub mod validation;
pub mod wire;

pub use backend::*;
pub use delegation::*;
pub use ids::*;
pub use request::*;
pub use resource::*;
pub use status::*;
pub use validation::{LanguageValidationError, ValidationResult};
pub use wire::{LanguageWire, LanguageWireError, decode, encode};

#[cfg(test)]
mod tests;
