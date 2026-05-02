//! Linux ELF64 解析子模块。
//!
//! 对外只暴露 [`LinuxElfImage`]：它的 `parse`、`segments_typed` 与其他
//! 静态方法供已知格式的调用方直接用；`impl Image` 让 [`crate::parse`] 能
//! 返回 `Box<dyn Image>`。其它子文件全部 `pub(super)` 或更严格，对外
//! 完全透明。
//!
//! 本子模块**不**处理装载、动态链接、`PT_INTERP` 跟进——那是 loader 的事；
//! 这里只解析、切片、报告。

mod header;
mod parse;
mod program_header;
mod raw;

pub use parse::LinuxElfImage;
