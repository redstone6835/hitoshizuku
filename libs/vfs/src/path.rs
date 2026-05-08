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
use crate::vfs::stat::FileType;

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

/// 解析路径，返回最终分量对应的 [`LookupResult`]（Dentry + 所在 Mount）。
///
/// 这是所有 `*at` 系统调用的基础，实现了完整的路径解析语义：
/// - 绝对路径：从进程可见根（`ctx.root_dentry()`）开始；
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
pub fn lookup(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
    flags: LookupFlags,
) -> VfsResult<LookupResult> {
    if path.is_empty() {
        return Err(VfsError::NotFound);
    }
    if path.len() > ctx.limits.path_max {
        return Err(VfsError::NameTooLong);
    }
    // 拒绝包含 NUL 字节的路径（防止字符串截断攻击）。
    if path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }

    // 确定解析起点及对应的挂载点
    let (start, start_mount) = if PathComponents::is_absolute(path) {
        (ctx.root_dentry(), Arc::clone(&ctx.mount_ns.root.lock()))
    } else {
        match dirfd {
            Dirfd::Cwd => (ctx.cwd(), ctx.cwd_mount()),
            // File 已在 open 时记录了所在挂载点，直接复用
            Dirfd::Fd(file) => {
                if file.inode.kind() != FileType::Directory {
                    return Err(VfsError::NotADirectory);
                }
                (Arc::clone(&file.dentry), Arc::clone(&file.mount))
            }
        }
    };

    let mut state = WalkState {
        current: start,
        current_mount: start_mount,
        symlink_remaining: ctx.limits.symlink_max_depth,
        ctx,
    };

    /// 辅助：处理 walk_component 返回的 (dentry, Option<mount>)，更新 state
    fn step(state: &mut WalkState<'_>, name: &str) -> VfsResult<()> {
        let (dentry, new_mount) = walk_component(state, name)?;
        state.current = dentry;
        if let Some(m) = new_mount {
            state.current_mount = m;
        }
        Ok(())
    }

    let mut components = PathComponents::new(path).peekable();

    while let Some(component) = components.next() {
        let is_last = components.peek().is_none();

        if !is_last {
            step(&mut state, component)?;
            // 检查中间分量类型，并验证执行（搜索）权限
            if let Some(inode) = state.current.inode() {
                // 每个中间目录分量必须有执行（搜索）权限。
                // 缺少此检查将导致 DAC 绕过：攻击者可穿越无 x 权限的目录。
                {
                    let meta = inode.meta_snapshot();
                    if !state.ctx.cred.can_exec(meta.uid, meta.gid, meta.mode, true) {
                        return Err(VfsError::PermissionDenied);
                    }
                }
                if inode.kind == crate::vfs::stat::FileType::Symlink {
                    if flags.has(LookupFlags::NO_SYMLINKS) {
                        return Err(VfsError::NotADirectory);
                    }
                    let link = Arc::clone(&state.current);
                    state.current = follow_symlink(&mut state, &link)?;
                } else if inode.kind != crate::vfs::stat::FileType::Directory {
                    return Err(VfsError::NotADirectory);
                }
            }
        } else {
            match step(&mut state, component) {
                Ok(()) => {
                    // 最后分量若是符号链接，且未设置 NO_FOLLOW，则跟随
                    if !flags.has(LookupFlags::NO_FOLLOW)
                        && let Some(inode) = state.current.inode()
                        && inode.kind == crate::vfs::stat::FileType::Symlink
                    {
                        let link = Arc::clone(&state.current);
                        state.current = follow_symlink(&mut state, &link)?;
                    }
                }
                Err(VfsError::NotFound) if flags.has(LookupFlags::ALLOW_MISSING_LAST) => {
                    // 允许最后分量不存在（open(O_CREAT) 等场景）：
                    // state.current 此时仍为父目录，直接 break 跳出循环，
                    // 后续返回父目录的 LookupResult。
                }
                Err(e) => return Err(e),
            }
        }
    }

    // 检查 DIRECTORY 标志
    if flags.has(LookupFlags::DIRECTORY)
        && let Some(inode) = state.current.inode()
        && inode.kind != crate::vfs::stat::FileType::Directory
    {
        return Err(VfsError::NotADirectory);
    }

    Ok(LookupResult {
        dentry: state.current,
        mount: state.current_mount,
    })
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
    if path.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    if path.len() > ctx.limits.path_max {
        return Err(VfsError::NameTooLong);
    }
    if path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }

    let components: alloc::vec::Vec<&str> = PathComponents::new(path).collect();
    if components.is_empty() {
        // 纯 "/"：根目录本身没有有意义的父目录和名称，任何试图在根上
        // 执行 create/unlink/mkdir 的操作都应被拒绝。
        return Err(VfsError::InvalidArgument);
    }

    let last_name: &'p str = components.last().ok_or(VfsError::InvalidArgument)?;

    if components.len() == 1 {
        // 单分量路径，父目录为 dirfd 指定的目录
        let (parent, parent_mount) = if PathComponents::is_absolute(path) {
            (ctx.root_dentry(), Arc::clone(&ctx.mount_ns.root.lock()))
        } else {
            match dirfd {
                Dirfd::Cwd => (ctx.cwd(), ctx.cwd_mount()),
                Dirfd::Fd(f) => {
                    if f.inode.kind() != FileType::Directory {
                        return Err(VfsError::NotADirectory);
                    }
                    (Arc::clone(&f.dentry), Arc::clone(&f.mount))
                }
            }
        };
        let result = LookupResult {
            dentry: parent,
            mount: parent_mount,
        };
        validate_basename(ctx, &result.dentry, last_name)?;
        return Ok((result, last_name));
    }

    // 构造父目录路径（去掉最后一个分量）
    let parent_path = {
        let trimmed = path.trim_end_matches('/');
        // 找最后一个 '/' 的位置
        match trimmed.rfind('/') {
            Some(0) => "/",
            Some(pos) => &trimmed[..pos],
            None => "",
        }
    };

    let result = lookup(ctx, dirfd, parent_path, LookupFlags::default())?;
    validate_basename(ctx, &result.dentry, last_name)?;
    Ok((result, last_name))
}

fn validate_basename(ctx: &VfsContext, parent: &Arc<Dentry>, name: &str) -> VfsResult<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }
    if name.len() > ctx.limits.path_max {
        return Err(VfsError::NameTooLong);
    }
    let parent_inode = parent.inode().ok_or(VfsError::NotFound)?;
    if parent_inode.kind() != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    if let Some(sb) = parent_inode.superblock()
        && name.len() > sb.name_max as usize
    {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

fn checked_start_from_dirfd(
    ctx: &VfsContext,
    dirfd: &Dirfd,
) -> VfsResult<(Arc<Dentry>, Arc<Mount>)> {
    match dirfd {
        Dirfd::Cwd => Ok((ctx.cwd(), ctx.cwd_mount())),
        Dirfd::Fd(file) => {
            if file.inode.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
            Ok((Arc::clone(&file.dentry), Arc::clone(&file.mount)))
        }
    }
}

fn searchable_directory(
    state: &WalkState<'_>,
    dentry: &Arc<Dentry>,
) -> VfsResult<Arc<crate::vfs::inode::Inode>> {
    let inode = dentry.inode().ok_or(VfsError::NotFound)?;
    if inode.kind() != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    let meta = inode.meta_snapshot();
    if !state.ctx.cred.can_exec(meta.uid, meta.gid, meta.mode, true) {
        return Err(VfsError::PermissionDenied);
    }
    Ok(inode)
}

pub(crate) fn lookup_mountpoint(
    ctx: &VfsContext,
    dirfd: &Dirfd,
    path: &str,
) -> VfsResult<LookupResult> {
    if path.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    if path.len() > ctx.limits.path_max {
        return Err(VfsError::NameTooLong);
    }
    if path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }

    let components: alloc::vec::Vec<&str> = PathComponents::new(path).collect();
    if components.is_empty() {
        return Ok(LookupResult {
            dentry: ctx.root_dentry(),
            mount: Arc::clone(&ctx.mount_ns.root.lock()),
        });
    }
    let name = *components.last().ok_or(VfsError::InvalidArgument)?;
    if name == "." {
        let (dentry, mount) = if PathComponents::is_absolute(path) {
            (ctx.root_dentry(), Arc::clone(&ctx.mount_ns.root.lock()))
        } else {
            checked_start_from_dirfd(ctx, dirfd)?
        };
        return Ok(LookupResult { dentry, mount });
    }

    let (parent_result, name) = lookup_parent(ctx, dirfd, path)?;
    let parent_inode = parent_result.dentry.inode().ok_or(VfsError::NotFound)?;
    let child = match crate::vfs::DCACHE.get(&parent_result.dentry, name) {
        Some(cached) if cached.is_positive() => cached,
        _ => {
            let child_inode = parent_inode.lookup(name)?;
            let dentry = crate::vfs::dentry::Dentry::new_positive(
                name,
                Some(Arc::clone(&parent_result.dentry)),
                child_inode,
            );
            crate::vfs::DCACHE.insert(dentry)
        }
    };
    Ok(LookupResult {
        dentry: child,
        mount: parent_result.mount,
    })
}

/// 解析单个路径分量 `name`（`.`、`..` 或普通名称），在 `parent` 目录下查找。
///
/// 返回 `(dentry, new_mount)`：若穿越了挂载边界则 `new_mount` 为 `Some(新挂载)`，
/// 否则为 `None`（挂载不变）。调用方负责更新 `state.current_mount`。
fn walk_component(
    state: &WalkState<'_>,
    name: &str,
) -> VfsResult<(Arc<Dentry>, Option<Arc<crate::vfs::mount::Mount>>)> {
    use crate::vfs::DCACHE;

    if name == "." {
        let _ = searchable_directory(state, &state.current)?;
        return Ok((Arc::clone(&state.current), None));
    }

    if name == ".." {
        let _ = searchable_directory(state, &state.current)?;
        let root = state.ctx.root_dentry();
        // 不超过进程可见根
        if Arc::ptr_eq(&state.current, &root) {
            return Ok((Arc::clone(&state.current), None));
        }
        // 若当前处于某挂载文件系统的根（mount_root），需先跨越挂载边界回到
        // 该挂载在父 FS 中的落脚点（mountpoint）。嵌套挂载时可能需要多次跨越，
        // 直到落脚在非挂载根的 dentry 上。每次 find_mountpoint 单独持锁，不嵌套。
        let mut effective = Arc::clone(&state.current);
        while let Some(mp) = state.ctx.mount_ns.find_mountpoint(&effective) {
            // 已跨到进程可见根，不能再向上
            if Arc::ptr_eq(&mp, &root) {
                return Ok((Arc::clone(&mp), None));
            }
            effective = mp;
        }
        let parent = {
            let meta = effective.meta.lock();
            meta.parent
                .as_ref()
                .map(Arc::clone)
                .unwrap_or_else(|| Arc::clone(&effective))
        };
        // 检查 parent 本身是否也是某个挂载点（进入被挂载 FS）
        let new_mount = state.ctx.mount_ns.lookup_mount(&parent);
        return Ok((parent, new_mount));
    }

    if !state.current.is_positive() {
        return Err(VfsError::NotFound);
    }
    let parent_inode = searchable_directory(state, &state.current)?;

    // 1. 查 dentry 缓存
    if let Some(cached) = DCACHE.get(&state.current, name) {
        // 穿越挂载点
        if let Some(mount) = state.ctx.mount_ns.lookup_mount(&cached) {
            let root = Arc::clone(&mount.mount_root);
            return Ok((root, Some(mount)));
        }
        // 负向 dentry
        if !cached.is_positive() {
            return Err(VfsError::NotFound);
        }
        return Ok((cached, None));
    }

    // 2. 缓存未命中：检查 name_max 并调用 InodeOps::lookup
    // 检查文件名长度
    if let Some(sb) = parent_inode.superblock()
        && name.len() > sb.name_max as usize
    {
        return Err(VfsError::NameTooLong);
    }

    match parent_inode.ops.lookup(&parent_inode, name) {
        Ok(child_inode) => {
            let dentry = crate::vfs::dentry::Dentry::new_positive(
                name,
                Some(Arc::clone(&state.current)),
                child_inode,
            );
            let canonical = DCACHE.insert(dentry);
            // 穿越挂载点
            if let Some(mount) = state.ctx.mount_ns.lookup_mount(&canonical) {
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
fn follow_symlink(state: &mut WalkState<'_>, link_dentry: &Arc<Dentry>) -> VfsResult<Arc<Dentry>> {
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

    if PathComponents::is_absolute(&target) {
        // 绝对链接：从进程根重新开始，同步重置挂载上下文
        state.current = state.ctx.root_dentry();
        state.current_mount = Arc::clone(&state.ctx.mount_ns.root.lock());
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

    for component in PathComponents::new(&target) {
        let (dentry, new_mount) = walk_component(state, component)?;
        state.current = dentry;
        if let Some(m) = new_mount {
            state.current_mount = m;
        }
        if let Some(inode) = state.current.inode()
            && inode.kind == FileType::Symlink
        {
            let link = Arc::clone(&state.current);
            state.current = follow_symlink(state, &link)?;
        }
    }

    Ok(Arc::clone(&state.current))
}

/// 路径规范化：将路径中的 `//`、`/./`、末尾 `/` 等多余元素清理，返回
/// 规范形式。此函数仅用于用户空间路径的预处理，不访问文件系统。
///
/// 注意：这里**不**解析 `..`（因为 `..` 的语义依赖文件系统状态和符号链接），
/// 仅做纯字符串层面的化简。
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
