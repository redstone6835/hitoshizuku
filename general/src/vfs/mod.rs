pub mod blockfs;
pub mod devtmpfs;
pub mod procfs;
pub mod sysfs;
pub mod tmpfs;

use alloc::string::String;

pub use ::vfs::*;
pub use blockfs::register_block_filesystems;
pub use devtmpfs::DevTmpfsDriver;
pub use procfs::ProcFsDriver;
pub use sysfs::SysFsDriver;
pub use tmpfs::TmpfsDriver;

pub use ::vfs::cred::Credentials;
pub use ::vfs::dentry::{Dentry, VfsRoot};
pub use ::vfs::fdtable::FdTable;
pub use ::vfs::limits::VfsLimits;
pub use ::vfs::mount::{Mount, MountNamespace};
pub use ::vfs::stat::FileMode;

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
