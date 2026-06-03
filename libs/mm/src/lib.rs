#![no_std]
//!
//! 进程地址空间的数据模型与纯算法。
//!
//! 本 crate 只负责"描述一个地址空间长什么样"：VMA（虚拟内存区域）数据结构、
//! 权限位、按地址排序的 VMA 集合、插入 / 查找 / 分裂 / 合并 / 裁剪算法。
//! **不**触碰物理页、**不**调页表、**不**依赖 VFS——file-backed VMA 通过
//! 本 crate 定义的轻量 [`FileLike`] trait 对象承载，`libs/vfs::File` 在 vfs 侧
//! 提供 impl，依赖方向永远是 `libs/vfs → libs/mm`。
//!
//! ## 设计动机
//!
//! ELF loader / mmap / fork / mprotect 都要用到同一份 VMA 代数。把它抽出来
//! 让 `general::mm::VmSpace` 只组合这里的算法 + 注入的 `UserPgdOps`，避免在
//! arch / kernel 层重复 rebuild 一整套"按地址查 VMA".的代码。
//!
extern crate alloc;

pub mod area;
pub mod error;
pub mod file_like;
pub mod flags;
pub mod set;

pub use area::{VmArea, VmBacking};
pub use error::UserAccessError;
pub use file_like::FileLike;
pub use flags::VmFlags;
pub use set::VmaSet;

#[cfg(any(test, feature = "ktest-kernel"))]
mod tests;
