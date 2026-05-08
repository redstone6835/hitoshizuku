//! 虚拟文件系统（VFS）层。
//!
//! VFS 是内核与具体文件系统驱动之间的抽象层，提供了一套统一的接口（trait），
//! 使得上层代码（系统调用、内核模块）无需关心底层究竟是 ext4、tmpfs 还是 procfs。

#![no_std]

extern crate alloc;

pub use alloc::sync::Arc;

pub mod cred;
pub mod dentry;
pub mod error;
pub mod fdtable;
pub mod file;
pub mod inode;
pub mod limits;
pub mod mount;
pub mod pipe;
pub mod operation;
pub mod path;
pub mod stat;
pub mod superblock;
pub mod sync;

use cred::Credentials;
use dentry::{Dentry, DentryCache, VfsRoot};
use error::{VfsError, VfsResult};
use limits::VfsLimits;
use mount::MountNamespace;
use stat::{FileMode, FileType};

/// 为沿用旧的 `crate::vfs::...` 路径提供兼容别名。
mod vfs {
    pub use crate::Arc;
    pub use crate::cred;
    pub use crate::dentry;
    pub use crate::error;
    pub use crate::error::VfsResult;
    pub use crate::fdtable;
    pub use crate::file;
    pub use crate::inode;
    pub use crate::limits;
    pub use crate::mount;
    pub use crate::path;
    pub use crate::stat;
    pub use crate::stat::FileMode;
    pub use crate::superblock;
    pub use crate::sync;
    pub use crate::{DCACHE, FS_REGISTRY, VfsContext};
}

/// 内核全局 Dentry 缓存实例。
pub static DCACHE: DentryCache = DentryCache::new();

/// 内核全局文件系统驱动注册表。
pub static FS_REGISTRY: superblock::FsRegistry = superblock::FsRegistry::new();

struct CwdState {
    cwd: Arc<Dentry>,
    cwd_mount: Arc<mount::Mount>,
}

/// 进程的 VFS 上下文，封装了路径解析所需的全部进程级状态。
pub struct VfsContext {
    cwd_state: sync::Spinlock<CwdState>,
    root_state: sync::Spinlock<VfsRoot>,
    pub mount_ns: Arc<MountNamespace>,
    pub cred: Arc<Credentials>,
    umask: sync::Spinlock<FileMode>,
    pub limits: Arc<VfsLimits>,
}

impl VfsContext {
    pub fn new(
        cwd: Arc<Dentry>,
        cwd_mount: Arc<mount::Mount>,
        root: VfsRoot,
        mount_ns: Arc<MountNamespace>,
        cred: Arc<Credentials>,
        umask: FileMode,
        limits: Arc<VfsLimits>,
    ) -> Self {
        Self {
            cwd_state: sync::Spinlock::new(CwdState { cwd, cwd_mount }),
            root_state: sync::Spinlock::new(root),
            mount_ns,
            cred,
            umask: sync::Spinlock::new(umask),
            limits,
        }
    }

    pub fn cwd(&self) -> Arc<Dentry> {
        Arc::clone(&self.cwd_state.lock().cwd)
    }

    pub fn cwd_mount(&self) -> Arc<mount::Mount> {
        Arc::clone(&self.cwd_state.lock().cwd_mount)
    }

    pub fn set_cwd(&self, new_cwd: Arc<Dentry>, new_mount: Arc<mount::Mount>) -> VfsResult<()> {
        if let Some(inode) = new_cwd.inode() {
            if inode.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
        } else {
            return Err(VfsError::NotFound);
        }
        let mut state = self.cwd_state.lock();
        state.cwd = new_cwd;
        state.cwd_mount = new_mount;
        Ok(())
    }

    pub fn set_root(&self, new_root: Arc<Dentry>, new_mount: Arc<mount::Mount>) -> VfsResult<()> {
        if let Some(inode) = new_root.inode() {
            if inode.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
        } else {
            return Err(VfsError::NotFound);
        }
        let mut state = self.root_state.lock();
        state.root_dentry = new_root;
        state.mount = new_mount;
        Ok(())
    }

    pub fn root_dentry(&self) -> Arc<Dentry> {
        Arc::clone(&self.root_state.lock().root_dentry)
    }

    pub fn root_mount(&self) -> Arc<mount::Mount> {
        Arc::clone(&self.root_state.lock().mount)
    }

    pub fn set_cred(&mut self, new_cred: Arc<Credentials>) {
        self.cred = new_cred;
    }

    pub fn set_umask(&self, new_mask: FileMode) -> FileMode {
        let mut umask = self.umask.lock();
        let old = *umask;
        *umask = new_mask.mask(FileMode::PERM_MASK);
        old
    }

    pub fn apply_umask(&self, requested: FileMode) -> FileMode {
        requested.without(*self.umask.lock())
    }

    pub fn fork(&self) -> VfsResult<Self> {
        let cwd_st = self.cwd_state.lock();
        let root_dentry = self.root_dentry();
        Ok(Self {
            cwd_state: sync::Spinlock::new(CwdState {
                cwd: Arc::clone(&cwd_st.cwd),
                cwd_mount: Arc::clone(&cwd_st.cwd_mount),
            }),
            root_state: sync::Spinlock::new(VfsRoot {
                root_dentry,
                mount: self.root_mount(),
            }),
            mount_ns: Arc::clone(&self.mount_ns),
            cred: Arc::clone(&self.cred),
            umask: sync::Spinlock::new(*self.umask.lock()),
            limits: Arc::clone(&self.limits),
        })
    }

    pub fn clone_with_new_ns(&self) -> VfsResult<Self> {
        let new_ns = self.mount_ns.clone_namespace()?;
        let cwd_st = self.cwd_state.lock();
        let root_dentry = self.root_dentry();
        let new_cwd_mount = new_ns
            .find_mount_for_root(&cwd_st.cwd_mount.mount_root)
            .unwrap_or_else(|| Arc::clone(&new_ns.root.lock()));
        Ok(Self {
            cwd_state: sync::Spinlock::new(CwdState {
                cwd: Arc::clone(&cwd_st.cwd),
                cwd_mount: new_cwd_mount,
            }),
            root_state: sync::Spinlock::new(VfsRoot {
                root_dentry,
                mount: self.root_mount(),
            }),
            mount_ns: new_ns,
            cred: Arc::clone(&self.cred),
            umask: sync::Spinlock::new(*self.umask.lock()),
            limits: Arc::clone(&self.limits),
        })
    }
}
