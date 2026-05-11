//! 块设备同步 I/O 适配层。
//!
//! 本模块定义 [`BlockBackend`] 与驱动对接时共享的工具;目前主要职责是把
//! [`BlockBackendError`] 规范化映射到 [`vfs::error::VfsError`]。

use crate::state::BlockBackendError;

/// 将 [`BlockBackendError`] 映射到 VFS 错误。
#[inline]
pub(crate) fn backend_to_vfs(err: BlockBackendError) -> vfs::error::VfsError {
    match err {
        BlockBackendError::Io => vfs::error::VfsError::Io,
        BlockBackendError::OutOfRange => vfs::error::VfsError::InvalidArgument,
        BlockBackendError::Unsupported => vfs::error::VfsError::NotSupported,
    }
}
