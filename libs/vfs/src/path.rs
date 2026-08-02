//! 路径解析（Path Resolution）。
//!
//! 路径解析是将字符串形式的路径（如 `"/etc/passwd"` 或 `"../foo/bar"`）逐分量
//! 转换为 [`Dentry`] 的过程，是 `open`/`stat`/`mkdir` 等几乎所有系统调用的
//! 共同入口。正确、安全地实现路径解析是 VFS 层最关键的工作之一。
//!
//! ### 安全性考量
//!
//! 1. **符号链接循环检测**：通过 [`VfsContext::limits`](crate::vfs::VfsContext::limits) 中的
//!    `symlink_max_depth` 限制解析深度，防止恶意
//!    构造的符号链接环导致内核无限递归。
//!
//! 2. **根目录逃逸防护**：解析 `..` 时，若当前 Dentry 已是进程可见根（`VfsContext::root`）
//!    则不再继续向上，确保 `chroot`/`pivot_root` 的隔离性不被 `../../..` 绕过。
//!
//! 3. **挂载点穿越**：每次 lookup 到一个 Dentry 后，检查它是否是某个挂载点
//!    （[`MountNamespace::lookup_mount`]），若是则切换到被挂载文件系统的根 Dentry
//!    再继续解析。这个过程对调用方透明。
//!
//! 4. **`O_NOFOLLOW` 支持**：当设置了 [`LookupFlags::NO_FOLLOW`] 时，路径最后一个
//!    分量如果是符号链接，直接返回链接本身的 Dentry，而不跟随解析，以防止
//!    TOCTOU 攻击。
//!
//! 5. **`dirfd` 基准点**：所有解析函数接受 [`Dirfd`] 而非全局 cwd，支持
//!    `openat(2)`/`mkdirat(2)` 等 `*at` 系列调用。这类调用在内核中是防 TOCTOU
//!    的标准做法：用 fd 锁定一个目录后，后续对该目录的操作不受并发 rename 影响。

use alloc::string::String;
use alloc::sync::Arc;

use crate::vfs::VfsContext;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::File;
use crate::vfs::mount::Mount;

/// 符号链接最大跟随深度的默认值，在 [`crate::vfs::limits::VfsLimits::symlink_max_depth`]
/// 未显式配置时作为后备值。
///
/// 实际深度限制由传入 [`lookup`] 的 [`VfsContext::limits`](crate::vfs::VfsContext::limits)
/// 中的 `symlink_max_depth` 字段决定；此常量仅保留供文档参考。
pub const SYMLINK_MAX_DEPTH_DEFAULT: usize = 40;

/// 路径解析的基准目录，对应 `openat(2)` 的 `dirfd` 参数。
pub enum Dirfd {
    /// 以调用进程的当前工作目录（cwd）为基准（对应 `AT_FDCWD = -100`）。
    Cwd,
    /// 以指定的已打开目录描述符为基准。
    ///
    /// 此处的 [`File`] 必须指向目录（`FileType::Directory`）。`O_PATH` 描述符
    /// 也是合法的——`*at` 系列调用是 `O_PATH` fd 的主要用途之一。
    Fd(Arc<File>),
}

/// 路径解析的控制标志。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LookupFlags(pub u32);

impl LookupFlags {
    /// 不跟随最终路径分量的符号链接（`O_NOFOLLOW`）。
    ///
    /// 默认行为是跟随符号链接（无需设置任何标志）。设置此标志后，若最终分量
    /// 是符号链接，直接返回链接本身的 Dentry，不解析目标。
    pub const NO_FOLLOW: Self = Self(1 << 1);
    /// 要求最终分量必须是目录（`O_DIRECTORY`）。
    pub const DIRECTORY: Self = Self(1 << 2);
    /// 允许最终分量不存在（用于 `open(O_CREAT)` 的父目录查找）。
    pub const ALLOW_MISSING_LAST: Self = Self(1 << 3);
    /// 路径中间分量也不跟随符号链接（`O_RESOLVE_NO_SYMLINKS` 风格，更安全）。
    pub const NO_SYMLINKS: Self = Self(1 << 4);
    /// 不穿越最终路径分量上的挂载点。
    ///
    /// `mount(2)`/`umount(2)`/`rmdir(2)`/`rename(2)` 需要看到被覆盖的挂载点
    /// dentry 本身，而不是自动进入覆盖在它上面的挂载根。
    pub const NO_MOUNT_LAST: Self = Self(1 << 5);

    pub const fn has(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
    pub const fn without(self, flag: Self) -> Self {
        Self(self.0 & !flag.0)
    }
    pub const fn raw(self) -> u32 {
        self.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// 路径解析的中间状态，每处理一个分量后更新。
struct WalkState<'ctx> {
    /// 当前解析到的 Dentry（"当前目录"）。
    current: Arc<Dentry>,
    /// 当前 Dentry 所在的挂载点。
    ///
    /// 路径解析穿越挂载边界时同步更新，用于：
    /// 1. 检查 `MountFlags::RDONLY`（写操作前拦截，由 VFS 入口调用）；
    /// 2. `Mount::inc_open` / `dec_open` 维护引用计数，使 `is_busy()` 正确；
    /// 3. 返回给 VFS 入口层，写操作前执行 RDONLY 检查。
    current_mount: Arc<Mount>,
    /// 剩余可跟随的符号链接次数。
    symlink_remaining: usize,
    /// 当前 VFS 上下文（用于访问 cwd、root、mount namespace）。
    ctx: &'ctx VfsContext,
}

/// 路径解析的完整结果：最终 Dentry 及其所在 Mount。
///
/// `mount` 字段供 VFS 入口层使用：
/// - 写操作前检查 `mount.is_rdonly()`；
/// - `File` 打开时调用 `mount.inc_open()`，关闭时调用 `mount.dec_open()`。
pub struct LookupResult {
    /// 解析到的最终 Dentry。
    pub dentry: Arc<Dentry>,
    /// 该 Dentry 所在的挂载点。
    pub mount: Arc<crate::vfs::mount::Mount>,
}

/// 将路径字符串按 `'/'` 分割为分量迭代器，忽略连续斜杠和末尾斜杠。
///
/// 示例：`"/etc//passwd/"` → `["etc", "passwd"]`
///        `"foo/../bar"` → `["foo", "..", "bar"]`
pub struct PathComponents<'a> {
    rest: &'a str,
}

impl<'a> PathComponents<'a> {
    /// 构造分量迭代器。
    pub fn new(path: &'a str) -> Self {
        Self {
            rest: path.trim_start_matches('/'),
        }
    }

    /// 判断路径是否为绝对路径（以 `'/'` 开头）。
    pub fn is_absolute(path: &str) -> bool {
        path.starts_with('/')
    }
}

impl<'a> Iterator for PathComponents<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        // 跳过连续的斜杠。
        let s = self.rest.trim_start_matches('/');
        if s.is_empty() {
            return None;
        }
        // 找到下一个斜杠，截取当前分量。
        match s.find('/') {
            Some(i) => {
                self.rest = &s[i..];
                Some(&s[..i])
            }
            None => {
                self.rest = "";
                Some(s)
            }
        }
    }
}

// ── 核心路径解析函数 ──────────────────────────────────────────────────────────

fn symlink_not_allowed(state: &WalkState<'_>) -> VfsError {
    let limit = state.ctx.limits.symlink_max_depth;
    VfsError::SymlinkLoop {
        depth: limit.saturating_sub(state.symlink_remaining) + 1,
        limit,
    }
}

fn has_trailing_slash(path: &str) -> bool {
    path.ends_with('/')
}

fn requires_final_directory(path: &str, flags: LookupFlags) -> bool {
    flags.has(LookupFlags::DIRECTORY) || has_trailing_slash(path)
}

fn ensure_directory(dentry: &Arc<Dentry>) -> VfsResult<()> {
    let inode = dentry.inode().ok_or(VfsError::NotFound)?;
    if inode.kind != crate::vfs::stat::FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    Ok(())
}

fn validate_dirfd(file: &Arc<File>) -> VfsResult<()> {
    let inode = file.dentry.inode().ok_or(VfsError::NotFound)?;
    if inode.kind != crate::vfs::stat::FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    Ok(())
}

fn lookup_start(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
) -> VfsResult<(Arc<Dentry>, Arc<Mount>)> {
    if PathComponents::is_absolute(path) {
        return Ok((ctx.root.root(), ctx.root.mount()));
    }

    match dirfd {
        Dirfd::Cwd => Ok((ctx.cwd(), ctx.cwd_mount())),
        Dirfd::Fd(file) => {
            validate_dirfd(file)?;
            Ok((Arc::clone(&file.dentry), Arc::clone(&file.mount)))
        }
    }
}

fn validate_basename(name: &str) -> VfsResult<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }
    Ok(())
}

/// 将文件系统返回的符号链接目标转换为可解析路径。
///
/// Linux 的链接跟随路径以 C pathname 语义消费目标，首个 NUL 后的填充不会进入
/// 分量解析。磁盘文件系统返回的内容仍可由 `readlink(2)` 原样读取；这里只规范化
/// 实际参与路径遍历的视图。空目标没有可解析对象，按悬空链接处理。
pub(crate) fn symlink_target_path(target: &str) -> VfsResult<&str> {
    let path = target.split_once('\0').map_or(target, |(prefix, _)| prefix);
    if path.is_empty() {
        return Err(VfsError::NotFound);
    }
    Ok(path)
}

fn check_name_max(parent: &Arc<Dentry>, name: &str) -> VfsResult<()> {
    let parent_inode = parent.inode().ok_or(VfsError::NotFound)?;
    if let Some(sb) = parent_inode.superblock.upgrade()
        && name.len() > sb.name_max as usize
    {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

/// 解析路径，返回最终分量对应的 [`LookupResult`]（Dentry + 所在 Mount）。
///
/// 这是所有 `*at` 系统调用的基础，实现了完整的路径解析语义：
/// - 绝对路径：从进程可见根（`ctx.root`）开始；
/// - 相对路径：从 `dirfd`（cwd 或指定 fd 目录）开始；
/// - 每个分量：通过 dentry 缓存或 `InodeOps::lookup` 解析；
/// - 挂载穿越：每次解析后检查并跳过挂载边界，同步更新 `current_mount`；
/// - 符号链接跟随：受 `flags` 和深度限制控制；
/// - `..` 处理：向上回溯但不超过进程根。
///
/// # 错误
///
/// - [`VfsError::NotFound`]：路径中某分量不存在（且 flags 不允许缺失）；
/// - [`VfsError::NotADirectory`]：中间分量不是目录；
/// - [`VfsError::SymlinkLoop`]：符号链接深度超过 `ctx.limits.symlink_max_depth`；
/// - [`VfsError::NameTooLong`]：某分量字节数超过文件系统的 `name_max`。
#[kernel_symbols::export(
    name = "vfs.path.lookup",
    contract = "kernel.vfs.path@1",
    version = 1,
    capabilities = kernel_symbols::capability::VFS_QUERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn lookup(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
    flags: LookupFlags,
) -> VfsResult<LookupResult> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::VfsLookup).bytes(path.len());
    if path.is_empty() {
        return Err(VfsError::NotFound);
    }
    // 拒绝包含 NUL 字节的路径（防止字符串截断攻击）。
    if path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }

    // 确定解析起点及对应的挂载点
    let (start, start_mount) = lookup_start(ctx, dirfd, path)?;

    let mut state = WalkState {
        current: start,
        current_mount: start_mount,
        symlink_remaining: ctx.limits.symlink_max_depth,
        ctx,
    };
    walk_path(&mut state, path, flags)?;

    // 检查 DIRECTORY 标志和尾随斜杠语义。尾随斜杠等价于要求最终对象是目录。
    if requires_final_directory(path, flags) {
        ensure_directory(&state.current)?;
    }

    Ok(LookupResult {
        dentry: state.current,
        mount: state.current_mount,
    })
}

fn step(state: &mut WalkState<'_>, name: &str, traverse_mounts: bool) -> VfsResult<()> {
    let (dentry, new_mount) = walk_component(state, name, traverse_mounts)?;
    state.current = dentry;
    if let Some(m) = new_mount {
        state.current_mount = m;
    }
    Ok(())
}

fn walk_path(state: &mut WalkState<'_>, path: &str, flags: LookupFlags) -> VfsResult<()> {
    if path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }
    let mut components = PathComponents::new(path).peekable();
    let requires_dir = requires_final_directory(path, flags);
    let cred = state.ctx.cred();

    while let Some(component) = components.next() {
        let is_last = components.peek().is_none();

        if !is_last {
            step(state, component, true)?;
            if let Some(inode) = state.current.inode() {
                if inode.kind == crate::vfs::stat::FileType::Symlink {
                    if flags.has(LookupFlags::NO_SYMLINKS) {
                        return Err(symlink_not_allowed(state));
                    }
                    let link = Arc::clone(&state.current);
                    let link_flags = flags
                        .without(LookupFlags::NO_FOLLOW)
                        .without(LookupFlags::DIRECTORY)
                        .without(LookupFlags::ALLOW_MISSING_LAST)
                        .without(LookupFlags::NO_MOUNT_LAST);
                    state.current = follow_symlink(state, &link, link_flags)?;
                } else if inode.kind != crate::vfs::stat::FileType::Directory {
                    return Err(VfsError::NotADirectory);
                }
            }
            let inode = state.current.inode().ok_or(VfsError::NotFound)?;
            if inode.kind != crate::vfs::stat::FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
            let meta = inode.meta_snapshot();
            if !cred.can_exec(meta.uid, meta.gid, meta.mode, true) {
                return Err(VfsError::PermissionDenied);
            }
            continue;
        }

        if let Some(parent_inode) = state.current.inode() {
            let meta = parent_inode.meta_snapshot();
            if !cred.can_exec(meta.uid, meta.gid, meta.mode, true) {
                return Err(VfsError::PermissionDenied);
            }
        }

        let traverse_mounts = !flags.has(LookupFlags::NO_MOUNT_LAST);
        match step(state, component, traverse_mounts) {
            Ok(()) => {
                if let Some(inode) = state.current.inode()
                    && inode.kind == crate::vfs::stat::FileType::Symlink
                {
                    if flags.has(LookupFlags::NO_SYMLINKS) {
                        return Err(symlink_not_allowed(state));
                    }
                    if !flags.has(LookupFlags::NO_FOLLOW) {
                        let link = Arc::clone(&state.current);
                        state.current =
                            follow_symlink(state, &link, flags.without(LookupFlags::NO_FOLLOW))?;
                    }
                }
            }
            Err(VfsError::NotFound)
                if flags.has(LookupFlags::ALLOW_MISSING_LAST) && !requires_dir => {}
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// 解析路径直到最后一个分量的 **父目录**，同时返回最后分量的名称字符串。
///
/// 用于需要在父目录中操作（创建、删除、重命名）的系统调用：
/// - `open(O_CREAT)`、`mkdir`、`mknod`、`symlink`、`link`：需要父目录 + 新文件名；
/// - `unlink`、`rmdir`：需要父目录 + 目标名称；
/// - `rename`：需要两个父目录 + 两个名称。
///
/// 返回值为 `(LookupResult { dentry: 父目录, mount: 父目录所在 Mount }, 最后分量名称)`。
/// 调用方应使用返回的 `mount` 做只读检查和 `inc_open` 统计，而不是重新查询
/// `mount_ns.lookup_mount`——后者会返回覆盖在父目录上的挂载，而非父目录本身所在的挂载。
pub fn lookup_parent<'p>(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &'p str,
) -> VfsResult<(LookupResult, &'p str)> {
    lookup_parent_inner(ctx, dirfd, path, false)
}

/// 与 [`lookup_parent`] 相同，但允许路径带尾随斜杠。
///
/// 仅目录类操作应使用此入口。POSIX 路径语义中尾随斜杠表示最终对象必须是目录；
/// 对 `mkdir("foo/")` 和 `rmdir("foo/")`，最终对象本身就是目录，所以需要把尾随
/// 斜杠规约掉后再提取父目录和叶子名。普通文件创建、硬链接和符号链接仍应使用
/// [`lookup_parent`]，避免错误接受 `file/` 形式的目标。
pub fn lookup_parent_dir_leaf<'p>(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &'p str,
) -> VfsResult<(LookupResult, &'p str)> {
    lookup_parent_inner(ctx, dirfd, path, true)
}

fn lookup_parent_inner<'p>(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &'p str,
    allow_trailing_slash: bool,
) -> VfsResult<(LookupResult, &'p str)> {
    if path.is_empty() || path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }
    if !allow_trailing_slash && path.ends_with('/') {
        return Err(VfsError::InvalidArgument);
    }

    let path = if allow_trailing_slash {
        trim_trailing_slashes_preserving_root(path)
    } else {
        path
    };

    let components: alloc::vec::Vec<&str> = PathComponents::new(path).collect();
    if components.is_empty() {
        // 纯 "/"：根目录本身没有有意义的父目录和名称，任何试图在根上
        // 执行 create/unlink/mkdir 的操作都应被拒绝。
        return Err(VfsError::InvalidArgument);
    }

    let last_name: &'p str = components.last().ok_or(VfsError::InvalidArgument)?;
    validate_basename(last_name)?;

    let result = if components.len() == 1 {
        // 单分量路径，父目录为 dirfd 指定的目录
        let (parent, parent_mount) = lookup_start(ctx, dirfd, path)?;
        LookupResult {
            dentry: parent,
            mount: parent_mount,
        }
    } else {
        // 构造父目录路径（去掉最后一个分量）
        let parent_path = {
            // 这里保留 trim 是为了兼容重复斜杠场景。
            let trimmed = path.trim_end_matches('/');
            // 找最后一个 '/' 的位置
            match trimmed.rfind('/') {
                Some(0) => "/",
                Some(pos) => &trimmed[..pos],
                None => "",
            }
        };

        lookup(ctx, dirfd, parent_path, LookupFlags::DIRECTORY)?
    };
    ensure_directory(&result.dentry)?;
    check_name_max(&result.dentry, last_name)?;
    Ok((result, last_name))
}

fn trim_trailing_slashes_preserving_root(path: &str) -> &str {
    let mut end = path.len();
    let bytes = path.as_bytes();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

/// 解析单个路径分量 `name`（`.`、`..` 或普通名称），在 `parent` 目录下查找。
///
/// 返回 `(dentry, new_mount)`：若穿越了挂载边界则 `new_mount` 为 `Some(新挂载)`，
/// 否则为 `None`（挂载不变）。调用方负责更新 `state.current_mount`。
fn walk_component(
    state: &WalkState<'_>,
    name: &str,
    traverse_mounts: bool,
) -> VfsResult<(Arc<Dentry>, Option<Arc<crate::vfs::mount::Mount>>)> {
    use crate::vfs::DCACHE;

    if name == "." {
        return Ok((Arc::clone(&state.current), None));
    }

    if name == ".." {
        // 不超过进程可见根
        if state
            .ctx
            .root
            .is_at_root_in_mount(&state.current, &state.current_mount)
        {
            return Ok((Arc::clone(&state.current), None));
        }

        // 若当前处于某挂载文件系统的根（mount_root），`..` 先跨回父 mount 的
        // mountpoint，再在父 mount 中向上走一级。返回的 dentry 和 mount 必须
        // 同步更新，否则后续权限检查和 busy 统计会把父 FS 的 dentry 归到子 mount。
        if Arc::ptr_eq(&state.current, &state.current_mount.mount_root) {
            let (mountpoint, parent_mount) = {
                let location = state.current_mount.location.lock();
                let Some(parent) = location.parent.as_ref().and_then(|p| p.upgrade()) else {
                    return Ok((Arc::clone(&state.current), None));
                };
                (Arc::clone(&location.mountpoint), parent)
            };

            if state
                .ctx
                .root
                .is_at_root_in_mount(&mountpoint, &parent_mount)
            {
                return Ok((mountpoint, Some(parent_mount)));
            }

            let parent = {
                let meta = mountpoint.meta.lock();
                meta.parent
                    .as_ref()
                    .map(Arc::clone)
                    .unwrap_or_else(|| Arc::clone(&mountpoint))
            };
            return Ok((parent, Some(parent_mount)));
        }

        let parent = {
            let meta = state.current.meta.lock();
            meta.parent
                .as_ref()
                .map(Arc::clone)
                .unwrap_or_else(|| Arc::clone(&state.current))
        };
        return Ok((parent, None));
    }

    if !state.current.is_positive() {
        return Err(VfsError::NotFound);
    }

    // 检查 name_max 后再查缓存，确保超长名称不会因为命中 dcache 绕过限制。
    let parent_inode = state.current.inode().ok_or(VfsError::NotFound)?;
    if let Some(sb) = parent_inode.superblock.upgrade()
        && name.len() > sb.name_max as usize
    {
        return Err(VfsError::NameTooLong);
    }

    // 1. 查 dentry 缓存
    if let Some(cached) = DCACHE.get(&state.current, name) {
        // 负向 dentry
        if !cached.is_positive() {
            return Err(VfsError::NotFound);
        }
        // 穿越挂载点
        if traverse_mounts && let Some(mount) = state.ctx.mount_ns.lookup_mount(&cached) {
            let root = Arc::clone(&mount.mount_root);
            return Ok((root, Some(mount)));
        }
        return Ok((cached, None));
    }

    // 2. 缓存未命中：调用 InodeOps::lookup
    match parent_inode.ops.lookup(&parent_inode, name) {
        Ok(child_inode) => {
            let dentry = crate::vfs::dentry::Dentry::new_positive(
                name,
                Some(Arc::clone(&state.current)),
                child_inode,
            );
            let canonical = DCACHE.insert(dentry);
            // 穿越挂载点
            if traverse_mounts && let Some(mount) = state.ctx.mount_ns.lookup_mount(&canonical) {
                let root = Arc::clone(&mount.mount_root);
                return Ok((root, Some(mount)));
            }
            Ok((canonical, None))
        }
        Err(VfsError::NotFound) => {
            // 缓存负向 dentry 避免重复访问磁盘
            let neg =
                crate::vfs::dentry::Dentry::new_negative(name, Some(Arc::clone(&state.current)));
            DCACHE.insert(neg);
            Err(VfsError::NotFound)
        }
        Err(e) => Err(e),
    }
}

/// 跟随符号链接，将链接目标字符串重新交给路径解析循环处理。
///
/// 每次调用将 `state.symlink_remaining` 减一；若已为零则返回
/// [`VfsError::SymlinkLoop`]，以此限制链接嵌套深度。
///
/// 链接目标若以 `'/'` 开头，则从进程根重新开始解析（绝对链接）；否则
/// 以链接本身所在目录为基准（相对链接）。
fn follow_symlink(
    state: &mut WalkState<'_>,
    link_dentry: &Arc<Dentry>,
    flags: LookupFlags,
) -> VfsResult<Arc<Dentry>> {
    if state.symlink_remaining == 0 {
        let limit = state.ctx.limits.symlink_max_depth;
        return Err(VfsError::SymlinkLoop {
            depth: limit,
            limit,
        });
    }
    state.symlink_remaining -= 1;

    let inode = link_dentry.inode().ok_or(VfsError::NotFound)?;
    let target = inode.ops.readlink(&inode)?;
    let target_path = symlink_target_path(&target)?;

    if PathComponents::is_absolute(target_path) {
        // 绝对链接：从进程根重新开始，同步重置挂载上下文
        state.current = state.ctx.root.root();
        state.current_mount = state.ctx.root.mount();
    } else {
        // 相对链接：从链接所在目录（链接的父目录）开始解析，而非链接本身。
        // 调用方在 step() 后 state.current 已被设为链接 dentry，若不修正，
        // 则 "../x" 会少跳一级（从链接出发而非从链接的父目录出发）。
        let parent = {
            let meta = link_dentry.meta.lock();
            meta.parent.clone()
        };
        if let Some(p) = parent {
            state.current = p;
        }
        // 若 link_dentry 无父（根 dentry 不应是符号链接），维持不变
    }

    walk_path(state, target_path, flags)?;

    Ok(Arc::clone(&state.current))
}

/// 路径规范化：将路径中的 `//`、`/./`、末尾 `/` 等多余元素清理，返回
/// 规范形式。此函数仅用于用户空间路径的预处理，不访问文件系统。
///
/// 注意：这里**不**解析 `..`（因为 `..` 的语义依赖文件系统状态和符号链接），
/// 仅做纯字符串层面的化简。
#[kernel_symbols::export(
    name = "vfs.path.normalize_path",
    contract = "kernel.vfs.path@1",
    version = 1,
    capabilities = kernel_symbols::capability::CORE_SAFE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn normalize_path(path: &str) -> String {
    let absolute = PathComponents::is_absolute(path);
    // 过滤空分量和 "." 分量，重新组装
    let components: alloc::vec::Vec<&str> = PathComponents::new(path)
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();

    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    for (i, c) in components.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}
