//! 新 mount API 的 fs_context 状态机（Linux fsopen/fsconfig/fsmount/fspick）。
//!
//! `fsopen` 创建一个 fs_context（文件系统类型 + 参数累积），`fsconfig` 配置
//! source/挂载选项，`FSCONFIG_CMD_CREATE` 创建 superblock，`fsmount` 标记挂载
//! 就绪并返回挂载 fd，最后由 `move_mount`（MOVE_MOUNT_F_EMPTY_PATH）把挂载
//! 落到目标路径。`fspick`/`open_tree` 从已挂载路径构造可复用/克隆的上下文。

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;

use crate::mount::Mount;
use crate::superblock::Superblock;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::{DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::mount::MountFlags;
use crate::vfs::sync::Spinlock;

/// fsopen 标志。
pub const FSOPEN_CLOEXEC: u32 = 1;
/// fsmount 标志。
pub const FSMOUNT_CLOEXEC: u32 = 1;
/// open_tree 标志。
pub const OPEN_TREE_CLONE: u32 = 1;
/// fspick 标志。
pub const FSPICK_CLOEXEC: u32 = 1;
pub const FSPICK_EMPTY_PATH: u32 = 8;

/// fsconfig 命令。
pub const FSCONFIG_SET_FLAG: u32 = 0;
pub const FSCONFIG_SET_STRING: u32 = 1;
pub const FSCONFIG_CMD_CREATE: u32 = 6;
pub const FSCONFIG_CMD_RECONFIGURE: u32 = 7;

/// move_mount 标志。
pub const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x4;

/// fs_context 配置参数（fsconfig 累积）。
#[derive(Default)]
struct FsConfig {
    source: Option<String>,
    data: String,
    flags: MountFlags,
}

/// fs_context 状态机。
pub struct FsContext {
    /// 文件系统类型名（fsopen 参数或 fspick 从挂载点取）。
    fs_type: String,
    /// 配置（source/data/flags），fsconfig 写入。
    config: Spinlock<FsConfig>,
    /// FSCONFIG_CMD_CREATE 后创建的 superblock。
    superblock: Spinlock<Option<Arc<Superblock>>>,
    /// fsmount 后标记挂载就绪。
    mount_ready: Spinlock<bool>,
    /// open_tree 克隆模式：挂载根 dentry（供 move_mount 落位）。
    clone_root: Spinlock<Option<Arc<crate::vfs::dentry::Dentry>>>,
}

impl FsContext {
    pub fn new(fs_type: String) -> Arc<Self> {
        Arc::new(Self {
            fs_type,
            config: Spinlock::new(FsConfig::default()),
            superblock: Spinlock::new(None),
            mount_ready: Spinlock::new(false),
            clone_root: Spinlock::new(None),
        })
    }

    pub fn fs_type(&self) -> &str {
        &self.fs_type
    }

    pub fn source(&self) -> Option<String> {
        self.config.lock().source.clone()
    }

    pub fn data(&self) -> String {
        self.config.lock().data.clone()
    }

    pub fn flags(&self) -> MountFlags {
        self.config.lock().flags
    }

    /// FSCONFIG_SET_FLAG：把挂载选项名映射到 MountFlags 位。
    pub fn set_flag(&self, key: &str) -> Result<(), Errno> {
        let flag = match key {
            "ro" => MountFlags::RDONLY,
            "nosuid" => MountFlags::NOSUID,
            "nodev" => MountFlags::NODEV,
            "noexec" => MountFlags::NOEXEC,
            "sync" => MountFlags::SYNCHRONOUS,
            "noatime" => MountFlags::NOATIME,
            "nodiratime" => MountFlags::NODIRATIME,
            _ => return Err(Errno::EOPNOTSUPP),
        };
        let mut cfg = self.config.lock();
        cfg.flags = cfg.flags.with(flag);
        Ok(())
    }

    /// FSCONFIG_SET_STRING：source 或数据参数。
    pub fn set_string(&self, key: &str, value: &str) -> Result<(), Errno> {
        let mut cfg = self.config.lock();
        if key == "source" {
            cfg.source = Some(value.to_string());
        } else if key == "fscontext" || key == "subtype" {
            return Err(Errno::EOPNOTSUPP);
        } else {
            if !cfg.data.is_empty() {
                cfg.data.push(',');
            }
            cfg.data.push_str(key);
            if !value.is_empty() {
                cfg.data.push('=');
                cfg.data.push_str(value);
            }
        }
        Ok(())
    }

    /// FSCONFIG_CMD_CREATE：通过驱动创建 superblock。
    pub fn create_superblock(&self) -> Result<(), Errno> {
        if self.superblock.lock().is_some() {
            return Err(Errno::EPERM);
        }
        let driver = crate::FS_REGISTRY
            .find(&self.fs_type)
            .ok_or(Errno::ENODEV)?;
        let dev = self.source();
        let data = self.data();
        let sb = driver
            .mount(dev.as_deref(), &data)
            .map_err(|e| e.to_errno())?;
        *self.superblock.lock() = Some(sb);
        Ok(())
    }

    /// fsmount：校验 superblock 就绪并标记。
    pub fn mark_mount_ready(&self) -> Result<(), Errno> {
        if self.superblock.lock().is_none() {
            return Err(Errno::EINVAL);
        }
        *self.mount_ready.lock() = true;
        Ok(())
    }

    pub fn take_superblock(&self) -> Option<Arc<Superblock>> {
        self.superblock.lock().clone()
    }

    pub fn is_mount_ready(&self) -> bool {
        *self.mount_ready.lock()
    }

    /// open_tree 克隆：记录挂载根 dentry。
    pub fn set_clone_root(&self, root: Arc<crate::vfs::dentry::Dentry>) {
        *self.clone_root.lock() = Some(root);
    }

    pub fn clone_root(&self) -> Option<Arc<crate::vfs::dentry::Dentry>> {
        self.clone_root.lock().clone()
    }

    /// 从已挂载路径初始化（fspick）。
    pub fn from_mount(mount: &Arc<Mount>) -> Arc<Self> {
        let ctx = Self::new(mount.superblock.fs_type.to_string());
        *ctx.superblock.lock() = Some(Arc::clone(&mount.superblock));
        *ctx.clone_root.lock() = Some(Arc::clone(&mount.mount_root));
        ctx
    }
}

/// fs_context 匿名 fd 的文件操作。
pub struct FsContextFileOps {
    pub(crate) ctx: Arc<FsContext>,
}

impl FsContextFileOps {
    /// 从 File 中取出 fs_context。
    pub fn from_file(file: &File) -> Option<Arc<FsContext>> {
        file.downcast_ops::<FsContextFileOps>()
            .map(|ops| Arc::clone(&ops.ctx))
    }
}

impl crate::file::FileOps for FsContextFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents::default()
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {}

    fn show_fdinfo(&self, out: &mut String) {
        use core::fmt::Write;
        let _ = writeln!(
            out,
            "fscontext fs_type:{} source:{:?}",
            self.ctx.fs_type,
            self.ctx.source()
        );
        let _ = writeln!(
            out,
            "fscontext flags:{:x} ready:{}",
            self.ctx.flags().raw(),
            *self.ctx.mount_ready.lock()
        );
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 创建 fs_context 匿名 fd。
pub fn create_fs_context_fd(
    fdt: &crate::vfs::fdtable::FdTable,
    cred: Arc<crate::vfs::cred::Credentials>,
    ctx: Arc<FsContext>,
    cloexec: bool,
) -> Result<crate::vfs::fdtable::Fd, Errno> {
    let flags = OpenOptions {
        access: crate::vfs::file::AccessMode::ReadWrite,
        ..Default::default()
    };
    let fd_flags = if cloexec {
        crate::vfs::fdtable::FdFlags::CLOEXEC
    } else {
        crate::vfs::fdtable::FdFlags::default()
    };
    crate::vfs::anon::create_fd(
        fdt,
        cred,
        flags,
        fd_flags,
        Box::new(FsContextFileOps { ctx }),
    )
    .map_err(|e| e.to_errno())
}

/// 挂载到目标路径（move_mount 落位；`clone_root` 存在时用克隆根作为挂载根）。
pub fn land_mount(
    mount_ns: &crate::mount::MountNamespace,
    ctx: &Arc<FsContext>,
    target: Arc<crate::vfs::dentry::Dentry>,
    target_mount: &Arc<Mount>,
) -> VfsResult<()> {
    let sb = ctx.take_superblock().ok_or(VfsError::InvalidArgument)?;
    let mount_root = match ctx.clone_root() {
        Some(root) => root,
        None => Arc::clone(&sb.root_dentry),
    };
    let new_mount = Mount::new(
        sb,
        target,
        mount_root,
        ctx.flags(),
        Some(Arc::downgrade(target_mount)),
    );
    mount_ns.attach_mount(new_mount, target_mount);
    Ok(())
}
