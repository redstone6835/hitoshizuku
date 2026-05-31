#![no_std]
//!
//! ELF 与后续自制二进制格式的统一解析层。
//!
//! 本 crate **只**做"字节 → 结构化视图"一件事：读 ELF / mygo header、切出
//! 段描述、报告入口地址与解释器提示。具体的装载（分配物理页、建 VmArea、
//! 布置 auxv、跳用户态）由上层 loader 完成。
//!
//! ## 两层分派
//!
//! - [`Image`] trait：调用方已知具体格式时走静态分派（`LinuxElfImage` 直接 impl）。
//! - [`parse`] 函数：magic 嗅探 → `Box<dyn Image>`，调用方把判断权交给本 crate。
//!
//! ## 前向兼容
//!
//! [`detect`] 模块的 `Kind` 枚举预留未来自制 "mygo 格式" 的分支；新增格式
//! 只需加子模块 + 一条 magic arm，[`Image`] 契约与 [`parse`] 签名不变。

extern crate alloc;

mod detect;
mod error;
mod image;
mod linux;
mod types;

pub use detect::parse;
pub use error::ElfError;
pub use image::Image;
pub use linux::LinuxElfImage;
pub use types::{AddressWidth, Arch, Segment, SegmentPerms};

#[cfg(any(test, feature = "ktest-kernel"))]
mod tests;
