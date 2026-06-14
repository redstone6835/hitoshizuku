//! 标准共享内存挂载点适配。
//!
//! `/dev/shm` 对用户态表现为约定路径，但内核内部不需要专用文件系统。这里仅把
//! 该路径准备为 tmpfs 挂载点，实际文件语义仍由普通 VFS/tmpfs 路径处理。

use alloc::sync::Arc;

use super::super::{DEV_DIR_MODE, DEV_DIR_PATH, ensure_dir};
use vfs::error;
use vfs::mount::{Mount, MountFlags};
use vfs::operation;
use vfs::path;
use vfs::stat::FileMode;
use vfs::{FS_REGISTRY, VfsContext};

/// 共享内存目录名；集中定义，避免启动路径散落字符串字面量。
pub const STANDARD_SHM_DIR_NAME: &str = "shm";
/// 共享内存目录的标准路径。
pub const STANDARD_SHM_DIR_PATH: &str = "/dev/shm";

const TMPFS_FS_TYPE: &str = "tmpfs";
const STANDARD_SHM_DIR_MODE: FileMode = FileMode::new(0o1777);

/// 在 `/dev/shm` 上挂载 tmpfs，为 shm_open/ftruncate/mmap(MAP_SHARED) 提供普通文件后端。
///
/// 这里不引入专用共享内存文件系统：`/dev/shm` 只是一个 tmpfs 挂载点，后续语义
/// 由常规 VFS open/truncate/mmap 路径处理。调用前应已经注册 tmpfs，并把 devtmpfs
/// 挂载到 `/dev`，这样挂载点目录会创建在 devtmpfs 内。
pub fn mount_standard_shm_tmpfs(ctx: &VfsContext) -> error::VfsResult<Arc<Mount>> {
    ensure_dir(ctx, DEV_DIR_PATH, DEV_DIR_MODE)?;
    ensure_dir(ctx, STANDARD_SHM_DIR_PATH, STANDARD_SHM_DIR_MODE)?;

    if let Ok(existing) = path::lookup(
        ctx,
        &path::Dirfd::Cwd,
        STANDARD_SHM_DIR_PATH,
        path::LookupFlags::DIRECTORY,
    ) {
        if existing.mount.superblock.fs_type == TMPFS_FS_TYPE
            && Arc::ptr_eq(&existing.dentry, &existing.mount.mount_root)
        {
            // 已经是 tmpfs 覆盖的 /dev/shm 时不重复叠加挂载，只补齐标准权限。
            operation::fchmodat(
                ctx,
                &path::Dirfd::Cwd,
                STANDARD_SHM_DIR_PATH,
                STANDARD_SHM_DIR_MODE,
                false,
            )?;
            return Ok(existing.mount);
        }
    }

    let mountpoint = path::lookup(
        ctx,
        &path::Dirfd::Cwd,
        STANDARD_SHM_DIR_PATH,
        path::LookupFlags::DIRECTORY.with(path::LookupFlags::NO_MOUNT_LAST),
    )?;
    let shm_sb = FS_REGISTRY
        .find(TMPFS_FS_TYPE)
        .ok_or(error::VfsError::NoDevice)?
        .mount(None, "")?;
    let shm_mount = ctx.mount_ns.mount(
        mountpoint.dentry,
        shm_sb,
        MountFlags::NOSUID.with(MountFlags::NODEV),
    )?;

    // tmpfs 根目录默认按驱动模式创建，挂载后再设置成共享内存目录的 01777 语义。
    operation::fchmodat(
        ctx,
        &path::Dirfd::Cwd,
        STANDARD_SHM_DIR_PATH,
        STANDARD_SHM_DIR_MODE,
        false,
    )?;
    Ok(shm_mount)
}
