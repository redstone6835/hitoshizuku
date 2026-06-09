pub mod blockfs;
pub mod device_numbers;
pub mod devtmpfs;
pub mod mount_source;
pub mod net_ioctl;
pub mod procfs;
pub mod rtc_devnode;
pub mod sysfs;
pub mod tmpfs;

use alloc::string::String;

pub use ::vfs::*;
pub use blockfs::{
    BlockFsDriver, BlockFsProbe, mount_block_device_auto, mount_block_source_auto,
    register_block_filesystems, register_block_fs_driver,
};
pub use devtmpfs::DevTmpfsDriver;
pub use mount_source::{MountSource, resolve_block_mount_source};
pub use procfs::ProcFsDriver;
pub use sysfs::SysFsDriver;
pub use tmpfs::TmpfsDriver;

pub use ::vfs::cred::Credentials;
pub use ::vfs::dentry::{Dentry, VfsRoot};
pub use ::vfs::fdtable::FdTable;
pub use ::vfs::limits::VfsLimits;
pub use ::vfs::mount::{Mount, MountFlags, MountNamespace};
pub use ::vfs::stat::FileMode;

/// 设备目录的标准挂载路径。
pub const DEV_DIR_PATH: &str = "/dev";
/// POSIX 共享内存目录名；集中定义，避免启动路径散落字符串字面量。
pub const POSIX_SHM_DIR_NAME: &str = "shm";
/// POSIX 共享内存目录的标准路径。
pub const POSIX_SHM_DIR_PATH: &str = "/dev/shm";
const TMPFS_FS_TYPE: &str = "tmpfs";
const DEV_DIR_MODE: FileMode = FileMode::new(0o755);
const POSIX_SHM_DIR_MODE: FileMode = FileMode::new(0o1777);

/// 取当前任务的 [`VfsContext`]（通过 sched 的 ext 侧表）。
///
/// 没有装载或当前不在 sched 调度的语境（启动早期）时返回 None。
pub fn current_vfs_context() -> Option<Arc<VfsContext>> {
    if !sched::is_ready() {
        return None;
    }
    let payload = sched::current_task().ext_lookup(sched::TASKEXT_VFS_CONTEXT)?;
    payload.downcast::<VfsContext>().ok()
}

/// 取当前任务的 [`FdTable`]。语义同上。
pub fn current_fdtable() -> Option<Arc<FdTable>> {
    if !sched::is_ready() {
        return None;
    }
    let payload = sched::current_task().ext_lookup(sched::TASKEXT_VFS_FDTABLE)?;
    payload.downcast::<FdTable>().ok()
}

/// Return an absolute path for `dentry` in `ctx`'s mount namespace.
pub fn namespace_path(
    ctx: &VfsContext,
    dentry: &Arc<Dentry>,
    mount: &Arc<Mount>,
) -> Option<String> {
    let root_mount = ctx.root.mount();
    let visible_root = ctx.root.root();
    let mut current_mount = Arc::clone(mount);
    let mut path = if Arc::ptr_eq(&current_mount, &root_mount) {
        dentry.full_path(&visible_root)?
    } else {
        dentry.full_path(&current_mount.mount_root)?
    };

    while !Arc::ptr_eq(&current_mount, &root_mount) {
        let (mountpoint, parent) = {
            let location = current_mount.location.lock();
            let parent = location.parent.as_ref()?.upgrade()?;
            (Arc::clone(&location.mountpoint), parent)
        };
        let prefix = if Arc::ptr_eq(&parent, &root_mount) {
            mountpoint.full_path(&visible_root)?
        } else {
            mountpoint.full_path(&parent.mount_root)?
        };
        path = join_abs_paths(&prefix, &path);
        current_mount = parent;
    }

    Some(path)
}

pub fn join_abs_paths(prefix: &str, suffix: &str) -> String {
    if prefix == "/" {
        let mut out = String::with_capacity(1 + suffix.len());
        out.push('/');
        out.push_str(suffix.trim_start_matches('/'));
        return out;
    }
    if suffix == "/" {
        return String::from(prefix);
    }
    let mut out = String::with_capacity(prefix.len() + 1 + suffix.len());
    out.push_str(prefix.trim_end_matches('/'));
    out.push('/');
    out.push_str(suffix.trim_start_matches('/'));
    out
}

/// kernel 启动期构造 init 任务用的 VfsContext 所需要的 7 元组。
///
/// 把 acpi/dtb 两条启动路径里重复的字段集中起来，便于后续把"装到 init 上"
/// 这一步集中管理。返回顺序与 [`VfsContext::new`] 参数一一对应。
pub fn build_boot_vfs_parts(
    cwd: Arc<Dentry>,
    cwd_mount: Arc<Mount>,
    mount_ns: Arc<MountNamespace>,
    cred: Arc<Credentials>,
) -> (
    Arc<Dentry>,
    Arc<Mount>,
    VfsRoot,
    Arc<MountNamespace>,
    Arc<Credentials>,
    FileMode,
    Arc<VfsLimits>,
) {
    let root = VfsRoot::new(Arc::clone(&cwd), Arc::clone(&cwd_mount));
    (
        cwd,
        cwd_mount,
        root,
        mount_ns,
        cred,
        FileMode::new(0),
        VfsLimits::default_arc(),
    )
}

/// 确保绝对路径上的目录存在。
///
/// 启动期多条固件路径都会准备标准挂载点；把逻辑放在 general 层可以避免
/// ACPI/DTB 互相引用，也让"已存在即成功"的幂等语义保持一致。
pub fn ensure_dir(ctx: &VfsContext, target: &str, mode: FileMode) -> error::VfsResult<()> {
    match path::lookup(ctx, &path::Dirfd::Cwd, target, path::LookupFlags::DIRECTORY) {
        Ok(_) => Ok(()),
        Err(error::VfsError::NotFound) => operation::mkdirat(ctx, &path::Dirfd::Cwd, target, mode),
        Err(err) => Err(err),
    }
}

/// 在 `/dev/shm` 上挂载 tmpfs，为 POSIX shm_open/ftruncate/mmap(MAP_SHARED) 提供普通文件后端。
///
/// 这里不引入专用 POSIX shm 文件系统：`/dev/shm` 只是一个 tmpfs 挂载点，后续语义
/// 由常规 VFS open/truncate/mmap 路径处理。调用前应已经注册 tmpfs，并把 devtmpfs
/// 挂载到 `/dev`，这样挂载点目录会创建在 devtmpfs 内。
pub fn mount_posix_shm_tmpfs(ctx: &VfsContext) -> error::VfsResult<Arc<Mount>> {
    ensure_dir(ctx, DEV_DIR_PATH, DEV_DIR_MODE)?;
    ensure_dir(ctx, POSIX_SHM_DIR_PATH, POSIX_SHM_DIR_MODE)?;

    if let Ok(existing) = path::lookup(
        ctx,
        &path::Dirfd::Cwd,
        POSIX_SHM_DIR_PATH,
        path::LookupFlags::DIRECTORY,
    ) {
        if existing.mount.superblock.fs_type == TMPFS_FS_TYPE
            && Arc::ptr_eq(&existing.dentry, &existing.mount.mount_root)
        {
            // 已经是 tmpfs 覆盖的 /dev/shm 时不重复叠加挂载，只补齐标准权限。
            operation::fchmodat(
                ctx,
                &path::Dirfd::Cwd,
                POSIX_SHM_DIR_PATH,
                POSIX_SHM_DIR_MODE,
                false,
            )?;
            return Ok(existing.mount);
        }
    }

    let mountpoint = path::lookup(
        ctx,
        &path::Dirfd::Cwd,
        POSIX_SHM_DIR_PATH,
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

    // tmpfs 根目录默认按驱动模式创建，挂载后再设置成 /dev/shm 的 01777 语义。
    operation::fchmodat(
        ctx,
        &path::Dirfd::Cwd,
        POSIX_SHM_DIR_PATH,
        POSIX_SHM_DIR_MODE,
        false,
    )?;
    Ok(shm_mount)
}
