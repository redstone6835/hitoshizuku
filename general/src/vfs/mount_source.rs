//! VFS 挂载源解析。
//!
//! `libs/vfs` 只把 mount 源作为 ABI 字符串传给文件系统驱动。这里是 general 层的
//! 适配边界：把路径、符号链接和兼容设备名解析成具体内核设备对象，避免 blockfs 或
//! 文件系统驱动各自硬编码 `/dev` 命名策略。

use alloc::boxed::Box;
use alloc::sync::Arc;

use vfs::error::{VfsError, VfsResult};
use vfs::path::{self, Dirfd, LookupFlags};

use crate::dev::block::BlockDevice;
use crate::dev::enumerate::DEVICES;
use crate::vfs::device_files::projection::lookup_block_device_by_node;

use super::current_vfs_context;
use super::devtmpfs::block_device_from_inode;

/// 用户传入的挂载源在 general 层的类型化表达。
#[derive(Clone)]
pub enum MountSource {
    None,
    Path(Box<str>),
    Block(Arc<BlockDevice>),
    DeviceName(Box<str>),
}

/// 把 mount 源解析为块设备对象。
///
/// 解析顺序：
/// 1. 如果当前有 VFS 上下文，按调用方提供的路径原样走 path lookup。符号链接由
///    VFS path walker 标准处理。
/// 2. 如果源是单个名字且路径解析失败，作为兼容 fallback 查询设备 function registry。
///
/// 这里刻意不把 `foo` 改写成 `/dev/foo`。是否在 `/dev` 下创建什么名字，属于
/// devtmpfs/用户态策略，不属于 blockfs。
pub fn resolve_block_mount_source(source: &str) -> VfsResult<Arc<BlockDevice>> {
    if source.is_empty() {
        return Err(VfsError::NoDevice);
    }

    if let Some(dev) = resolve_block_mount_source_from_vfs(source)? {
        return Ok(dev);
    }

    if is_plain_device_name(source) {
        return lookup_block_device_by_node(&DEVICES.functions, source).ok_or(VfsError::NotFound);
    }

    Err(VfsError::NotFound)
}

fn resolve_block_mount_source_from_vfs(source: &str) -> VfsResult<Option<Arc<BlockDevice>>> {
    let Some(ctx) = current_vfs_context() else {
        return Ok(None);
    };

    match path::lookup(ctx.as_ref(), &Dirfd::Cwd, source, LookupFlags::default()) {
        Ok(result) => {
            let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;
            block_device_from_inode(&inode)
                .map(Some)
                .ok_or(VfsError::NoDevice)
        }
        Err(VfsError::NotFound) => Ok(None),
        Err(err) => Err(err),
    }
}

fn is_plain_device_name(source: &str) -> bool {
    !source.is_empty()
        && !source.starts_with('/')
        && !source.contains('/')
        && !source.contains('\0')
}
