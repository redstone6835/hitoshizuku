pub mod blockfs;
pub mod device_files;
pub mod devpts;
pub mod devtmpfs;
pub mod mount_source;
pub mod mqueue;
pub mod pidfd;
pub mod procfs;
pub mod sysfs;
pub mod tmpfs;
pub mod user_api;

use alloc::string::String;

pub use ::vfs::*;
pub use blockfs::{
    BlockFsDriver, BlockFsProbe, mount_block_device_auto, mount_block_source_auto,
    register_block_filesystems, register_block_fs_driver,
};
pub use devpts::DevPtsDriver;
pub use devtmpfs::DevTmpfsDriver;
pub use mount_source::{MountSource, resolve_block_mount_source};
pub use mqueue::{
    MqFsDriver, MqNotifyDispatcher, dispatch_mq_notification, mq_registry,
    register_mq_notify_dispatcher,
};
pub use procfs::ProcFsDriver;
pub use sysfs::SysFsDriver;
pub use tmpfs::TmpfsDriver;
pub use user_api::shm::mount_standard_shm_tmpfs;

pub use ::vfs::cred::Credentials;
pub use ::vfs::dentry::{Dentry, VfsRoot};
pub use ::vfs::fdtable::FdTable;
pub use ::vfs::limits::VfsLimits;
pub use ::vfs::mount::{Mount, MountFlags, MountNamespace};
pub use ::vfs::stat::FileMode;

/// 设备目录的标准挂载路径。
pub const DEV_DIR_PATH: &str = "/dev";
const DEV_DIR_MODE: FileMode = FileMode::new(0o755);

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

/// 返回 `dentry` 在 `ctx` 挂载命名空间中的绝对路径。
#[kernel_symbols::export(
    name = "general.vfs.namespace_path",
    contract = "kernel.vfs.path@1",
    version = 1,
    capabilities = kernel_symbols::capability::VFS_QUERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
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
