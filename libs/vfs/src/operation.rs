use crate::vfs::*;

use error::VfsError;
use fdtable::{Fd, FdFlags, FdTable};
use file::{File, OpenOptions};
use inode::Inode;
use mount::MountFlags;
use path::{Dirfd, LookupFlags};

// ── 内部辅助函数 ─────────────────────────────────────────────────────────────

/// 对有 sticky bit 的目录，只允许目录所有者、文件所有者或持有 CAP_FOWNER 的
/// 进程删除/重命名其中的条目。
fn check_sticky(
    ctx: &VfsContext,
    parent_meta: &inode::InodeMeta,
    child_uid: cred::Uid,
) -> VfsResult<()> {
    if parent_meta.mode.sticky()
        && ctx.cred.euid != parent_meta.uid
        && ctx.cred.euid != child_uid
        && !ctx.cred.has_cap(cred::Capability::FOwner)
    {
        return Err(VfsError::OperationNotPermitted);
    }
    Ok(())
}

/// 检查父目录的写+执行权限，可选 sticky bit 检查。
///
/// `sticky_child_uid`：若为 `Some(uid)`，在父目录有 sticky bit 时额外检查
/// 当前进程是否有权操作该 uid 的文件。
fn check_parent_perm(
    ctx: &VfsContext,
    parent_inode: &Arc<Inode>,
    sticky_child_uid: Option<cred::Uid>,
) -> VfsResult<()> {
    let pmeta = parent_inode.meta_snapshot();
    if !ctx.cred.can_write(pmeta.uid, pmeta.gid, pmeta.mode) {
        return Err(VfsError::PermissionDenied);
    }
    if !ctx.cred.can_exec(pmeta.uid, pmeta.gid, pmeta.mode, true) {
        return Err(VfsError::PermissionDenied);
    }
    if let Some(child_uid) = sticky_child_uid {
        check_sticky(ctx, &pmeta, child_uid)?;
    }
    Ok(())
}

/// 应用 umask 并根据 CAP_FSETID 清除 SUID/SGID 位。
///
/// POSIX：不持有 CAP_FSETID 的进程不得创建 setuid/setgid 文件，
/// VFS 层在传入驱动之前统一清除特殊位，防止权限提升。
fn apply_create_mode(ctx: &VfsContext, mode: FileMode) -> FileMode {
    let mut m = ctx.apply_umask(mode);
    if !ctx.cred.has_cap(cred::Capability::FSetId) {
        m = m.without(FileMode::SUID_SGID);
    }
    m
}

/// 将新创建的 inode 插入 inode cache 和 dentry cache。
///
/// 返回经过 DCACHE 去重后的规范 Dentry（若并发插入，返回先到的那个）。
fn cache_new_inode(
    parent: &Arc<dentry::Dentry>,
    name: &str,
    inode: Arc<Inode>,
) -> Arc<dentry::Dentry> {
    if let Some(sb) = inode.superblock.upgrade() {
        sb.insert_inode(Arc::clone(&inode));
    }
    let new_dentry = dentry::Dentry::new_positive(name, Some(Arc::clone(parent)), inode);
    DCACHE.insert(new_dentry)
}

/// 将已从命名空间摘除的 inode 标记为"待回收"。
///
/// 真正的 `InodeOps::evict` 由 `Inode::drop` 在最后一个强引用释放时触发。
/// 这样可以避免对仍被打开文件持有的 inode 过早释放底层资源。
fn retire_inode(inode: Arc<Inode>) {
    let _ = inode.retire_if_unlinked();
}

// ── open / creat ──────────────────────────────────────────────────────────────

/// `openat(2)` — 打开或创建文件，返回 fd。
///
/// 将路径解析、权限检查、`InodeOps::open`、挂载引用计数、fd 分配串成完整流程。
pub fn openat(
    ctx: &VfsContext,
    fdt: &FdTable,
    dirfd: &Dirfd,
    path: &str,
    flags: OpenOptions,
    mode: FileMode,
) -> VfsResult<Fd> {
    let lookup_flags = {
        let mut f = LookupFlags::default();
        if flags.nofollow {
            f = f.with(LookupFlags::NO_FOLLOW);
        }
        if flags.directory {
            f = f.with(LookupFlags::DIRECTORY);
        }
        // O_CREAT：路径解析正常进行；若最后分量不存在，lookup 返回 Err(NotFound)，
        // 由下方 Err(NotFound) 分支处理创建逻辑。
        // 不使用 ALLOW_MISSING_LAST：该标志会使 lookup 在文件不存在时返回 Ok(parent)，
        // 导致 Err(NotFound) 分支永远无法触发，O_CREAT 完全失效。
        f
    };

    let result = path::lookup(ctx, dirfd, path, lookup_flags);

    let (dentry, mount) = match result {
        Ok(r) => {
            // 文件存在但设置了 O_CREAT | O_EXCL：返回 AlreadyExists
            if flags.create && flags.exclusive {
                return Err(VfsError::AlreadyExists);
            }
            (r.dentry, r.mount)
        }
        Err(VfsError::NotFound) if flags.create => {
            // ── O_CREAT 路径：文件不存在，在父目录创建 ──
            let (parent_result, name) = path::lookup_parent(ctx, dirfd, path)?;
            let parent_dentry = parent_result.dentry;
            let parent_mount = parent_result.mount;
            let parent_inode = parent_dentry.inode().ok_or(VfsError::NotFound)?;

            // 父目录所在 mount 的只读检查
            parent_mount.check_writable()?;

            // DAC：对父目录需要写+执行权限
            check_parent_perm(ctx, &parent_inode, None)?;

            // 驱动的 create 负责原子地检查 O_EXCL（若文件已并发创建，返回 AlreadyExists）
            let effective_mode = apply_create_mode(ctx, mode);
            let new_inode =
                parent_inode
                    .ops
                    .create(&parent_inode, name, effective_mode, &ctx.cred)?;

            let canonical = cache_new_inode(&parent_dentry, name, new_inode);

            (canonical, parent_mount)
        }
        Err(e) => return Err(e),
    };

    let inode = dentry.inode().ok_or(VfsError::NotFound)?;

    // ── 类型检查 ──
    if flags.directory && inode.kind != stat::FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    if inode.kind == stat::FileType::Directory && (flags.writable() || flags.truncate) {
        return Err(VfsError::IsADirectory);
    }
    if matches!(
        inode.kind,
        stat::FileType::BlockDevice | stat::FileType::CharDevice
    ) && mount.flags_snapshot().has(MountFlags::NODEV)
    {
        return Err(VfsError::OperationNotPermitted);
    }

    // ── 只读挂载检查 ──
    if flags.writable() {
        mount.check_writable()?;
    }

    // ── DAC 权限检查 ──
    {
        let meta = inode.meta_snapshot();
        if flags.readable() && !ctx.cred.can_read(meta.uid, meta.gid, meta.mode) {
            return Err(VfsError::PermissionDenied);
        }
        if flags.writable() && !ctx.cred.can_write(meta.uid, meta.gid, meta.mode) {
            return Err(VfsError::PermissionDenied);
        }
    }

    // ── O_TRUNC ──
    if flags.truncate && flags.writable() && inode.kind == stat::FileType::Regular {
        inode.ops.truncate(&inode, 0)?;
    }

    // ── 调用驱动 open，VFS 层组装 File（含 Mount，Drop 时自动 dec_open）──
    let ops = inode.ops.open(&inode, &flags, &ctx.cred)?;
    let file = File::new(
        Arc::clone(&inode),
        flags,
        Arc::clone(&ctx.cred),
        ops,
        Arc::clone(&dentry),
        Arc::clone(&mount), // File::drop 时自动 dec_open
    );

    // ── 挂载引用计数：inc_open 在 File 构造之后，alloc_fd 之前 ──
    mount.inc_open();

    // ── fd 分配：失败时 drop(file) 触发 dec_open 抵消上面的 inc_open ──
    let fd_flags = if flags.cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    fdt.alloc_fd(Arc::new(file), fd_flags)
}

// ── close ─────────────────────────────────────────────────────────────────────

/// `close(2)` — 关闭 fd。
///
/// `Arc<File>` 引用计数归零时 `File::drop` 自动调用 `FileOps::release` 并 `dec_open`，
/// 无需在此处手动处理。
pub fn close(fdt: &FdTable, fd: Fd) -> VfsResult<()> {
    fdt.close_fd(fd)
}

// ── mkdir ─────────────────────────────────────────────────────────────────────

/// `mkdirat(2)` — 创建目录。
pub fn mkdirat(ctx: &VfsContext, dirfd: &Dirfd, path: &str, mode: FileMode) -> VfsResult<()> {
    if !path.is_empty() && path.as_bytes().iter().all(|&b| b == b'/') {
        return Err(VfsError::AlreadyExists);
    }

    let (parent_result, name) = path::lookup_parent(ctx, dirfd, path)?;
    let parent_dentry = parent_result.dentry;
    let parent_mount = parent_result.mount;
    let parent_inode = parent_dentry.inode().ok_or(VfsError::NotFound)?;

    if parent_mount.is_rdonly() {
        return Err(VfsError::ReadOnlyFilesystem);
    }

    check_parent_perm(ctx, &parent_inode, None)?;

    let effective_mode = apply_create_mode(ctx, mode);
    let new_inode = parent_inode
        .ops
        .mkdir(&parent_inode, name, effective_mode, &ctx.cred)?;

    cache_new_inode(&parent_dentry, name, new_inode);
    Ok(())
}

// ── rmdir ─────────────────────────────────────────────────────────────────────

/// `unlinkat(AT_REMOVEDIR)` — 删除空目录。
pub fn rmdir(ctx: &VfsContext, dirfd: &Dirfd, path: &str) -> VfsResult<()> {
    let (parent_result, name) = path::lookup_parent(ctx, dirfd, path)?;
    let parent_dentry = parent_result.dentry;
    let parent_inode = parent_dentry.inode().ok_or(VfsError::NotFound)?;

    let target = path::lookup(ctx, dirfd, path, LookupFlags::default())?;
    if target.mount.is_rdonly() {
        return Err(VfsError::ReadOnlyFilesystem);
    }

    let child_inode = target.dentry.inode().ok_or(VfsError::NotFound)?;
    if child_inode.kind != stat::FileType::Directory {
        return Err(VfsError::NotADirectory);
    }

    let child_uid = child_inode.meta_snapshot().uid;
    check_parent_perm(ctx, &parent_inode, Some(child_uid))?;

    parent_inode.ops.rmdir(&parent_inode, name, &child_inode)?;
    DCACHE.invalidate_subtree(&target.dentry);
    retire_inode(child_inode);
    Ok(())
}

// ── unlink ────────────────────────────────────────────────────────────────────

/// `unlinkat(2)` — 删除文件（减少 nlink）。
pub fn unlink(ctx: &VfsContext, dirfd: &Dirfd, path: &str) -> VfsResult<()> {
    let (parent_result, name) = path::lookup_parent(ctx, dirfd, path)?;
    let parent_dentry = parent_result.dentry;
    let parent_inode = parent_dentry.inode().ok_or(VfsError::NotFound)?;

    let target = path::lookup(ctx, dirfd, path, LookupFlags::NO_FOLLOW)?;
    if target.mount.is_rdonly() {
        return Err(VfsError::ReadOnlyFilesystem);
    }

    let child_inode = target.dentry.inode().ok_or(VfsError::NotFound)?;
    if child_inode.kind == stat::FileType::Directory {
        return Err(VfsError::IsADirectory);
    }

    let child_uid = child_inode.meta_snapshot().uid;
    check_parent_perm(ctx, &parent_inode, Some(child_uid))?;

    parent_inode.ops.unlink(&parent_inode, name, &child_inode)?;
    DCACHE.invalidate_dentry(&target.dentry);
    target.dentry.invalidate();
    retire_inode(child_inode);
    Ok(())
}

// ── rename ────────────────────────────────────────────────────────────────────

/// `renameat2(2)` — 重命名/移动文件或目录。
pub fn renameat(
    ctx: &VfsContext,
    old_dirfd: &Dirfd,
    old_path: &str,
    new_dirfd: &Dirfd,
    new_path: &str,
) -> VfsResult<()> {
    let (old_parent_result, old_name) = path::lookup_parent(ctx, old_dirfd, old_path)?;
    let (new_parent_result, new_name) = path::lookup_parent(ctx, new_dirfd, new_path)?;

    let old_parent_dentry = old_parent_result.dentry;
    let new_parent_dentry = new_parent_result.dentry;
    let new_mount = new_parent_result.mount;

    if Arc::ptr_eq(&old_parent_dentry, &new_parent_dentry) && old_name == new_name {
        return Ok(());
    }

    let old_parent_inode = old_parent_dentry.inode().ok_or(VfsError::NotFound)?;
    let new_parent_inode = new_parent_dentry.inode().ok_or(VfsError::NotFound)?;

    // 跨设备检查（必须在同一 FS 内）
    if old_parent_inode.id.fs_id != new_parent_inode.id.fs_id {
        return Err(VfsError::CrossDevice);
    }

    let old_result = path::lookup(ctx, old_dirfd, old_path, LookupFlags::NO_FOLLOW)?;
    let old_inode = old_result.dentry.inode().ok_or(VfsError::NotFound)?;

    // 双端只读检查
    old_result.mount.check_writable()?;
    new_mount.check_writable()?;

    // 写权限 + sticky bit 检查（双端父目录）
    let old_inode_uid = old_inode.meta_snapshot().uid;
    let new_existing: Option<(Arc<dentry::Dentry>, Arc<inode::Inode>)> =
        path::lookup(ctx, new_dirfd, new_path, LookupFlags::NO_FOLLOW)
            .ok()
            .and_then(|r| r.dentry.inode().map(|inode| (r.dentry, inode)));
    let new_existing_uid: Option<cred::Uid> =
        new_existing.as_ref().map(|(_, i)| i.meta_snapshot().uid);
    {
        let m = old_parent_inode.meta_snapshot();
        if !ctx.cred.can_write(m.uid, m.gid, m.mode) {
            return Err(VfsError::PermissionDenied);
        }
        check_sticky(ctx, &m, old_inode_uid)?;
    }
    {
        let m = new_parent_inode.meta_snapshot();
        if !ctx.cred.can_write(m.uid, m.gid, m.mode) {
            return Err(VfsError::PermissionDenied);
        }
        if let Some(existing_uid) = new_existing_uid {
            check_sticky(ctx, &m, existing_uid)?;
        }
    }

    old_parent_inode.ops.rename(
        &old_parent_inode,
        old_name,
        &old_inode,
        &new_parent_inode,
        new_name,
    )?;

    if let Some((replaced_dentry, replaced_inode)) = &new_existing {
        if replaced_inode.kind == stat::FileType::Directory {
            DCACHE.invalidate_subtree(replaced_dentry);
        } else {
            DCACHE.invalidate_dentry(replaced_dentry);
            replaced_dentry.invalidate();
        }
    }

    // 更新 dentry cache：从旧键迁移到新键
    DCACHE.rename_dentry(&old_result.dentry, &new_parent_dentry, new_name);

    // 若替换了已有文件，清理被替换的 inode
    if let Some((_, replaced_inode)) = new_existing {
        retire_inode(replaced_inode);
    }
    Ok(())
}

// ── stat ──────────────────────────────────────────────────────────────────────

/// `fstatat(2)` — 通过路径获取文件元数据。
pub fn fstatat(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
    no_follow: bool,
) -> VfsResult<stat::FileStat> {
    let flags = if no_follow {
        LookupFlags::NO_FOLLOW
    } else {
        LookupFlags::default()
    };
    let result = path::lookup(ctx, dirfd, path, flags)?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;
    inode.stat()
}

/// `fstat(2)` — 通过已打开的 fd 获取文件元数据。
pub fn fstat(fdt: &FdTable, fd: Fd) -> VfsResult<stat::FileStat> {
    let file = fdt.get_file(fd).ok_or(VfsError::BadFileDescriptor)?;
    file.stat()
}

// ── chdir ─────────────────────────────────────────────────────────────────────

/// `chdir(2)` / `fchdir(2)` — 修改当前工作目录。
pub fn chdir(ctx: &mut VfsContext, dirfd: &Dirfd, path: &str) -> VfsResult<()> {
    let result = path::lookup(ctx, dirfd, path, LookupFlags::DIRECTORY)?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;
    // 需要对目标目录有执行（搜索）权限
    let meta = inode.meta_snapshot();
    if !ctx.cred.can_exec(meta.uid, meta.gid, meta.mode, true) {
        return Err(VfsError::PermissionDenied);
    }
    ctx.set_cwd(result.dentry, result.mount)
}

// ── chmod ─────────────────────────────────────────────────────────────────────

/// `fchmodat(2)` — 修改文件权限位。
pub fn fchmodat(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
    mut mode: FileMode,
    no_follow: bool,
) -> VfsResult<()> {
    let flags = if no_follow {
        LookupFlags::NO_FOLLOW
    } else {
        LookupFlags::default()
    };
    let result = path::lookup(ctx, dirfd, path, flags)?;
    result.mount.check_writable()?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;

    let inode_uid = inode.meta_snapshot().uid;
    if !ctx.cred.is_owner(inode_uid) {
        return Err(VfsError::OperationNotPermitted);
    }

    // POSIX：非特权进程 chmod 时必须清除 setuid/setgid 位，防止权限提升
    if !ctx.cred.has_cap(cred::Capability::FSetId) {
        mode = mode.without(FileMode::SUID_SGID);
    }

    inode.ops.chmod(&inode, mode)
}

// ── chown ─────────────────────────────────────────────────────────────────────

/// `fchownat(2)` — 修改文件所有者/组。
pub fn fchownat(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
    uid: Option<cred::Uid>,
    gid: Option<cred::Gid>,
    no_follow: bool,
) -> VfsResult<()> {
    let flags = if no_follow {
        LookupFlags::NO_FOLLOW
    } else {
        LookupFlags::default()
    };
    let result = path::lookup(ctx, dirfd, path, flags)?;
    result.mount.check_writable()?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;

    let (inode_uid, inode_gid) = {
        let m = inode.meta_snapshot();
        (m.uid, m.gid)
    };

    // uid 修改：需要 CAP_CHOWN
    if let Some(new_uid) = uid
        && new_uid != inode_uid
        && !ctx.cred.has_cap(cred::Capability::Chown)
    {
        return Err(VfsError::OperationNotPermitted);
    }
    // gid 修改：CAP_CHOWN，或者进程是文件所有者且新 gid 是进程的 egid 或附加组之一
    if let Some(new_gid) = gid
        && new_gid != inode_gid
    {
        let is_owner = ctx.cred.euid == inode_uid;
        let gid_ok = ctx.cred.egid == new_gid || ctx.cred.groups.contains(&new_gid);
        if !(ctx.cred.has_cap(cred::Capability::Chown) || is_owner && gid_ok) {
            return Err(VfsError::OperationNotPermitted);
        }
    }

    inode.ops.chown(&inode, uid, gid)?;

    // POSIX：chown 后必须清除 SUID/SGID，防止权限提升（除非 CAP_FSETID）
    if (uid.is_some() || gid.is_some()) && !ctx.cred.has_cap(cred::Capability::FSetId) {
        let current_mode = inode.meta_snapshot().mode;
        let new_mode = current_mode.without(FileMode::SUID_SGID);
        if new_mode != current_mode {
            inode.ops.chmod(&inode, new_mode)?;
        }
    }

    Ok(())
}

// ── mount / umount ────────────────────────────────────────────────────────────

/// 挂载文件系统：查找驱动 → 创建 Superblock → 挂载到挂载点。
///
/// `dev` 为块设备路径（如 `"/dev/sda1"`），内存文件系统（tmpfs 等）传 `None`。
pub fn mount(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    mountpoint_path: &str,
    fs_type: &str,
    mount_flags: MountFlags,
    dev: Option<&str>,
    data: &str,
) -> VfsResult<Arc<mount::Mount>> {
    if !ctx.cred.has_cap(cred::Capability::SysAdmin) {
        return Err(VfsError::OperationNotPermitted);
    }
    // 空 fstype 时按常见块文件系统顺序尝试自动检测
    const CANDIDATES: &[&str] = &["ext2", "ext3", "ext4", "vfat"];
    let candidates: &[&str] = if fs_type.is_empty() {
        CANDIDATES
    } else {
        core::slice::from_ref(&fs_type)
    };

    let mut last_error = VfsError::NoDevice;
    for &candidate in candidates {
        let Some(driver) = FS_REGISTRY.find(candidate) else {
            continue;
        };
        match driver.mount(dev, data) {
            Ok(superblock) => {
                let mountpoint =
                    path::lookup(ctx, dirfd, mountpoint_path, LookupFlags::DIRECTORY)?.dentry;
                return ctx.mount_ns.mount(mountpoint, superblock, mount_flags);
            }
            Err(e) => {
                last_error = e;
                continue;
            }
        }
    }
    Err(last_error)
}

/// 卸载文件系统。
pub fn umount(ctx: &VfsContext, dirfd: &Dirfd, path: &str, force: bool) -> VfsResult<()> {
    if !ctx.cred.has_cap(cred::Capability::SysAdmin) {
        return Err(VfsError::OperationNotPermitted);
    }
    let mountpoint = path::lookup(ctx, dirfd, path, LookupFlags::default())?.dentry;
    ctx.mount_ns.umount(&mountpoint, force)
}

/// `chroot(2)` — 修改当前进程可见根目录。
pub fn chroot(ctx: &VfsContext, dirfd: &Dirfd, path: &str) -> VfsResult<()> {
    if !ctx.cred.has_cap(cred::Capability::SysAdmin) {
        return Err(VfsError::OperationNotPermitted);
    }
    let result = path::lookup(ctx, dirfd, path, LookupFlags::DIRECTORY)?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;
    let meta = inode.meta_snapshot();
    if !ctx.cred.can_exec(meta.uid, meta.gid, meta.mode, true) {
        return Err(VfsError::PermissionDenied);
    }
    ctx.set_root(result.dentry, result.mount)
}

/// `pivot_root(2)` — 将当前命名空间根挂载切换到 `new_root`。
///
/// 当前 VFS 的进程根不是对命名空间根的裸引用，因此 pivot 成功后必须同步更新
/// 调用进程的 root/cwd，后续绝对路径解析才会从新根开始。
pub fn pivot_root(ctx: &VfsContext, new_root_path: &str, put_old_path: &str) -> VfsResult<()> {
    if !ctx.cred.has_cap(cred::Capability::SysAdmin) {
        return Err(VfsError::OperationNotPermitted);
    }

    let new_root = path::lookup(ctx, &Dirfd::Cwd, new_root_path, LookupFlags::DIRECTORY)?;
    let put_old = path::lookup(ctx, &Dirfd::Cwd, put_old_path, LookupFlags::DIRECTORY)?;

    ctx.mount_ns
        .pivot_root(Arc::clone(&new_root.dentry), put_old.dentry)?;
    let new_root_mount = ctx
        .mount_ns
        .find_mount_for_root(&new_root.dentry)
        .ok_or(VfsError::InvalidArgument)?;
    ctx.set_root(Arc::clone(&new_root.dentry), Arc::clone(&new_root_mount))?;
    ctx.set_cwd(new_root.dentry, new_root_mount)
}

// ── symlink / readlink ────────────────────────────────────────────────────────

/// `symlinkat(2)` — 创建符号链接。
pub fn symlinkat(ctx: &VfsContext, target: &str, dirfd: &Dirfd, link_path: &str) -> VfsResult<()> {
    // 防止 NUL 字节注入与超长目标路径（上限由 ctx.limits.path_max 决定）
    if target.is_empty() || target.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }
    if target.len() > ctx.limits.path_max {
        return Err(VfsError::NameTooLong);
    }

    let (parent_result, name) = path::lookup_parent(ctx, dirfd, link_path)?;
    let parent_dentry = parent_result.dentry;
    let parent_mount = parent_result.mount;
    let parent_inode = parent_dentry.inode().ok_or(VfsError::NotFound)?;

    if parent_mount.is_rdonly() {
        return Err(VfsError::ReadOnlyFilesystem);
    }

    check_parent_perm(ctx, &parent_inode, None)?;

    let new_inode = parent_inode
        .ops
        .symlink(&parent_inode, name, target, &ctx.cred)?;
    cache_new_inode(&parent_dentry, name, new_inode);
    Ok(())
}

/// `readlinkat(2)` — 读取符号链接目标路径。
pub fn readlinkat(ctx: &VfsContext, dirfd: &Dirfd, path: &str) -> VfsResult<alloc::string::String> {
    let result = path::lookup(ctx, dirfd, path, LookupFlags::NO_FOLLOW)?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;
    if inode.kind != stat::FileType::Symlink {
        return Err(VfsError::InvalidArgument);
    }
    // POSIX：符号链接的权限位（rwxrwxrwx）无意义，readlink 不检查 DAC 权限。
    inode.ops.readlink(&inode)
}

// ── truncate ──────────────────────────────────────────────────────────────────

/// `truncate(2)` — 通过路径截断文件大小。
pub fn truncate(ctx: &VfsContext, dirfd: &Dirfd, path: &str, size: u64) -> VfsResult<()> {
    let result = path::lookup(ctx, dirfd, path, LookupFlags::default())?;
    result.mount.check_writable()?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;
    if inode.kind == stat::FileType::Directory {
        return Err(VfsError::IsADirectory);
    }
    {
        let meta = inode.meta_snapshot();
        if !ctx.cred.can_write(meta.uid, meta.gid, meta.mode) {
            return Err(VfsError::PermissionDenied);
        }
    }
    inode.ops.truncate(&inode, size)
}

// ── utimes ────────────────────────────────────────────────────────────────────

/// `utimensat(2)` — 设置文件时间戳。
pub fn utimensat(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
    atime: Option<stat::Timespec>,
    mtime: Option<stat::Timespec>,
    no_follow: bool,
) -> VfsResult<()> {
    let flags = if no_follow {
        LookupFlags::NO_FOLLOW
    } else {
        LookupFlags::default()
    };
    let result = path::lookup(ctx, dirfd, path, flags)?;
    result.mount.check_writable()?;
    let inode = result.dentry.inode().ok_or(VfsError::NotFound)?;
    {
        let meta = inode.meta_snapshot();
        // 文件所有者或有写权限的进程可以设置时间戳；任意进程设置为当前时间
        // 此处实现：所有者或有写权限
        if !ctx.cred.is_owner(meta.uid) && !ctx.cred.can_write(meta.uid, meta.gid, meta.mode) {
            return Err(VfsError::PermissionDenied);
        }
    }
    inode.ops.utimes(&inode, atime, mtime)
}

// ── link ──────────────────────────────────────────────────────────────────────

/// `linkat(2)` — 创建硬链接。
pub fn linkat(
    ctx: &VfsContext,
    old_dirfd: &Dirfd,
    old_path: &str,
    new_dirfd: &Dirfd,
    new_path: &str,
    no_follow: bool,
) -> VfsResult<()> {
    let old_flags = if no_follow {
        LookupFlags::NO_FOLLOW
    } else {
        LookupFlags::default()
    };
    let old_result = path::lookup(ctx, old_dirfd, old_path, old_flags)?;
    let old_inode = old_result.dentry.inode().ok_or(VfsError::NotFound)?;

    // 禁止对目录创建硬链接（会破坏目录树 DAG，即使 root 也不允许）
    if old_inode.kind == stat::FileType::Directory {
        return Err(VfsError::OperationNotPermitted);
    }

    let (new_parent_result, new_name) = path::lookup_parent(ctx, new_dirfd, new_path)?;
    let new_parent_dentry = new_parent_result.dentry;
    let new_parent_mount = new_parent_result.mount;
    let new_parent_inode = new_parent_dentry.inode().ok_or(VfsError::NotFound)?;

    // 跨设备检查
    if old_inode.id.fs_id != new_parent_inode.id.fs_id {
        return Err(VfsError::CrossDevice);
    }

    // 旧文件所在 mount 只读检查
    old_result.mount.check_writable()?;
    // 新 parent 所在 mount 只读检查（硬链接写入新 parent 目录）
    new_parent_mount.check_writable()?;

    // protected_hardlinks（Linux fs.protected_hardlinks 语义）：
    // 只对普通文件检查；目录已在上方拒绝。
    // 不持有 CAP_DAC_READ_SEARCH 时，满足以下任意一个条件就拒绝：
    //   1. 进程不是文件所有者（euid != inode.uid）
    //   2. 文件设置了 setuid 位
    //   3. 文件设置了 setgid 位且有执行权限（可执行的 setgid 文件）
    // 条件 2/3 是防止攻击者通过硬链接维持对 setuid/setgid 可执行文件的访问。
    // CAP_FOWNER 同样绕过 protected_hardlinks（Linux may_linkat 语义：
    // inode_owner_or_capable 检查 owner OR CAP_FOWNER）。
    if old_inode.kind == stat::FileType::Regular
        && !ctx.cred.has_cap(cred::Capability::DacReadSearch)
        && !ctx.cred.has_cap(cred::Capability::FOwner)
    {
        let meta = old_inode.meta_snapshot();
        let setgid_exec = meta.mode.setgid() && meta.mode.group_exec();
        if ctx.cred.euid != meta.uid || meta.mode.setuid() || setgid_exec {
            return Err(VfsError::OperationNotPermitted);
        }
    }

    // 对新 parent 目录需要写权限（及执行权限）
    check_parent_perm(ctx, &new_parent_inode, None)?;

    // nlink 溢出检查：防止 nlink + 1 环绕为 0 导致 iput 误判文件已删除。
    // 当 nlink == u32::MAX 时再加 1 就会溢出，因此此处拒绝即可。
    if old_inode.nlink() == u32::MAX {
        return Err(VfsError::TooManyLinks);
    }

    new_parent_inode
        .ops
        .link(&new_parent_inode, &old_inode, new_name)?;

    let new_dentry = dentry::Dentry::new_positive(
        new_name,
        Some(Arc::clone(&new_parent_dentry)),
        Arc::clone(&old_inode),
    );
    DCACHE.insert(new_dentry);
    Ok(())
}

// ── mknod ─────────────────────────────────────────────────────────────────────

/// `mknodat(2)` — 创建特殊文件（设备节点、FIFO、socket）。
pub fn mknodat(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
    kind: stat::FileType,
    mode: FileMode,
    dev: stat::DevId,
) -> VfsResult<()> {
    // 创建块/字符设备节点需要 CAP_MKNOD
    if matches!(
        kind,
        stat::FileType::BlockDevice | stat::FileType::CharDevice
    ) && !ctx.cred.has_cap(cred::Capability::MkNod)
    {
        return Err(VfsError::OperationNotPermitted);
    }

    let (parent_result, name) = path::lookup_parent(ctx, dirfd, path)?;
    let parent_dentry = parent_result.dentry;
    let parent_mount = parent_result.mount;
    let parent_inode = parent_dentry.inode().ok_or(VfsError::NotFound)?;

    if parent_mount.is_rdonly() {
        return Err(VfsError::ReadOnlyFilesystem);
    }

    // MS_NODEV：挂载标志禁止在此挂载点创建/访问块设备和字符设备节点。
    if matches!(
        kind,
        stat::FileType::BlockDevice | stat::FileType::CharDevice
    ) && parent_mount
        .flags_snapshot()
        .has(crate::vfs::mount::MountFlags::NODEV)
    {
        return Err(VfsError::OperationNotPermitted);
    }

    check_parent_perm(ctx, &parent_inode, None)?;

    let effective_mode = apply_create_mode(ctx, mode);
    let new_inode =
        parent_inode
            .ops
            .mknod(&parent_inode, name, kind, effective_mode, dev, &ctx.cred)?;

    cache_new_inode(&parent_dentry, name, new_inode);
    Ok(())
}
