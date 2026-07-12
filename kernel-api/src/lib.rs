#![no_std]
#![warn(missing_docs)]

//! ELM 可访问的内核运行期 API 门面。
//!
//! 本 crate 只保存稳定契约、固定布局函数表和安全客户端，不包含内核子系统实现。
//! 外部 ELM 通过 [`ApiImport`] 在初始化阶段取得一个版本化命名空间，后续调用直接
//! 使用缓存函数表，不经过 elm-mgr 管理通道。

mod import;
pub mod memory;
mod table;

pub use import::{ApiImport, ApiImportError, ApiTableRef};
pub use table::{ApiGrantTokenV1, ApiTableHeaderV1, KernelApiLayoutV1, KernelApiTable};

/// Kernel API 命名空间 identifier 允许的最大字节数。
pub const KERNEL_API_IDENTIFIER_MAX_LEN: usize = elm::ELM_KERNEL_API_IDENTIFIER_MAX_LEN;

/// 返回内建 Kernel API 表的规范布局摘要。
///
/// 每个后续子系统批次会把已经完整实现的表加入该目录。未知命名空间不会得到零摘要，
/// 而是明确返回 `None`，避免打包器生成不可验证的依赖记录。
pub fn layout(identifier: &str, version: u16) -> Option<KernelApiLayoutV1> {
    table::layout(identifier, version)
}
