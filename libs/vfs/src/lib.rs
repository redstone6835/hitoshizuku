//! 虚拟文件系统（VFS）层。
//!
//! VFS 是内核与具体文件系统驱动之间的抽象层，提供了一套统一的接口（trait），
//! 使得上层代码（系统调用、内核模块）无需关心底层究竟是 ext4、tmpfs 还是 procfs。

#![no_std]

extern crate alloc;

pub use alloc::sync::Arc;

pub mod addr;
pub mod anon;
pub mod cred;
pub mod dentry;
pub mod elm;
pub mod epoll;
pub mod error;
pub mod eventfd;
pub mod fdtable;
pub mod file;
pub mod flock;
pub mod inode;
pub mod lease;
pub mod limits;
pub mod memfd;
pub mod mount;
pub mod net_socket;
pub mod netlink_socket;
pub mod operation;
pub mod path;
pub mod pipe;
pub mod poll_source;
pub mod record_lock;
pub mod signalfd;
pub mod socket;
pub mod stat;
pub mod superblock;
pub mod sync;
pub mod sysctl;
pub mod timerfd;

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use cred::Credentials;
use dentry::{Dentry, DentryCache, VfsRoot};
use error::{VfsError, VfsResult};
use limits::VfsLimits;
use mount::MountNamespace;
use stat::{FileMode, FileType};

/// 为沿用旧的 `crate::vfs::...` 路径提供兼容别名。
mod vfs {
    pub use crate::Arc;
    pub use crate::anon;
    pub use crate::cred;
    pub use crate::dentry;
    pub use crate::error;
    pub use crate::error::VfsResult;
    pub use crate::fdtable;
    pub use crate::file;
    pub use crate::flock;
    pub use crate::inode;
    pub use crate::lease;
    pub use crate::limits;
    pub use crate::mount;
    pub use crate::path;
    pub use crate::record_lock;
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

/// 强制链接器保留 VFS 直接符号所在的代码生成单元。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    vfs_context_diag as usize
        ^ file::file_diag as usize
        ^ path::normalize_path as usize
        ^ operation::openat as usize
        ^ operation::close_for_owner as usize
        ^ superblock::FsRegistry::register as usize
}

static VFS_CONTEXT_LIVE: AtomicUsize = AtomicUsize::new(0);
static VFS_CONTEXT_CREATED: AtomicUsize = AtomicUsize::new(0);
static VFS_CONTEXT_DROPPED: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, Default)]
pub struct VfsContextDiag {
    pub live: usize,
    pub created: usize,
    pub dropped: usize,
}

#[kernel_symbols::export(
    name = "vfs.vfs_context_diag",
    contract = "kernel.vfs.context-diagnostic@1",
    version = 1,
    capabilities = kernel_symbols::capability::VFS_QUERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn vfs_context_diag() -> VfsContextDiag {
    VfsContextDiag {
        live: VFS_CONTEXT_LIVE.load(Ordering::Acquire),
        created: VFS_CONTEXT_CREATED.load(Ordering::Acquire),
        dropped: VFS_CONTEXT_DROPPED.load(Ordering::Acquire),
    }
}

struct CwdState {
    cwd: Arc<Dentry>,
    cwd_mount: Arc<mount::Mount>,
}

/// 进程的 VFS 上下文，封装了路径解析所需的全部进程级状态。
pub struct VfsContext {
    cwd_state: sync::Spinlock<CwdState>,
    pub root: VfsRoot,
    pub mount_ns: Arc<MountNamespace>,
    cred: sync::Spinlock<Arc<Credentials>>,
    umask: sync::Spinlock<FileMode>,
    /// exec 快照租约与 cwd/root/cred/umask 更新共用的变更门。
    mutation_gate: sync::Spinlock<()>,
    /// cwd/root/cred/umask 变化的代际，供 exec 事务重验共享 CLONE_FS 状态。
    generation: AtomicU64,
    pub limits: Arc<VfsLimits>,
}

/// exec 持有期间禁止共享 `CLONE_FS` 方修改 VFS 上下文。
pub struct VfsExecLease<'a> {
    _gate: sync::SpinlockGuard<'a, ()>,
}

#[kernel_symbols::export]
impl VfsContext {
    #[kernel_symbols::export(
        name = "vfs.VfsContext.new",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_ADMIN,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn new(
        cwd: Arc<Dentry>,
        cwd_mount: Arc<mount::Mount>,
        root: VfsRoot,
        mount_ns: Arc<MountNamespace>,
        cred: Arc<Credentials>,
        umask: FileMode,
        limits: Arc<VfsLimits>,
    ) -> Self {
        VFS_CONTEXT_CREATED.fetch_add(1, Ordering::Relaxed);
        VFS_CONTEXT_LIVE.fetch_add(1, Ordering::Relaxed);
        cwd_mount.inc_open();
        Self {
            cwd_state: sync::Spinlock::new(CwdState { cwd, cwd_mount }),
            root,
            mount_ns,
            cred: sync::Spinlock::new(cred),
            umask: sync::Spinlock::new(umask),
            mutation_gate: sync::Spinlock::new(()),
            generation: AtomicU64::new(0),
            limits,
        }
    }

    /// 返回当前 VFS 权限检查使用的凭据快照。
    #[kernel_symbols::export(
        name = "vfs.VfsContext.cred",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn cred(&self) -> Arc<Credentials> {
        Arc::clone(&self.cred.lock())
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.cwd",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn cwd(&self) -> Arc<Dentry> {
        Arc::clone(&self.cwd_state.lock().cwd)
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.cwd_mount",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn cwd_mount(&self) -> Arc<mount::Mount> {
        Arc::clone(&self.cwd_state.lock().cwd_mount)
    }

    /// 返回当前 VFS 上下文代际。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn lock_for_exec(&self) -> VfsExecLease<'_> {
        VfsExecLease {
            _gate: self.mutation_gate.lock(),
        }
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.set_cwd",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_ADMIN,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_cwd(&self, new_cwd: Arc<Dentry>, new_mount: Arc<mount::Mount>) -> VfsResult<()> {
        let _gate = self.mutation_gate.lock();
        if let Some(inode) = new_cwd.inode() {
            if inode.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
        } else {
            return Err(VfsError::NotFound);
        }
        new_mount.inc_open();
        let mut state = self.cwd_state.lock();
        let old_mount = Arc::clone(&state.cwd_mount);
        state.cwd = new_cwd;
        state.cwd_mount = new_mount;
        old_mount.dec_open();
        self.bump_generation();
        Ok(())
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.set_root",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_ADMIN,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_root(&self, new_root: Arc<Dentry>, new_mount: Arc<mount::Mount>) -> VfsResult<()> {
        let _gate = self.mutation_gate.lock();
        if let Some(inode) = new_root.inode() {
            if inode.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
        } else {
            return Err(VfsError::NotFound);
        }
        self.root.set(new_root, new_mount);
        self.bump_generation();
        Ok(())
    }

    /// 更新当前任务的 VFS 凭据。
    ///
    /// `setuid`/`capset` 等 syscall 会先替换调度层凭据，再把派生出的 VFS 凭据同步到
    /// 这里；路径解析和文件创建随后读取该快照，避免继续使用旧的 root 权限。
    #[kernel_symbols::export(
        name = "vfs.VfsContext.set_cred",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_ADMIN,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_cred(&self, new_cred: Arc<Credentials>) {
        let _gate = self.mutation_gate.lock();
        *self.cred.lock() = new_cred;
        self.bump_generation();
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.set_umask",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_ADMIN,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_umask(&self, new_mask: FileMode) -> FileMode {
        let _gate = self.mutation_gate.lock();
        let mut umask = self.umask.lock();
        let old = *umask;
        *umask = new_mask.mask(FileMode::PERM_MASK);
        self.bump_generation();
        old
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.apply_umask",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn apply_umask(&self, requested: FileMode) -> FileMode {
        requested.without(*self.umask.lock())
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.fork",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_ADMIN,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn fork(&self) -> VfsResult<Self> {
        let cwd_st = self.cwd_state.lock();
        let cwd = Arc::clone(&cwd_st.cwd);
        let cwd_mount = Arc::clone(&cwd_st.cwd_mount);
        cwd_mount.inc_open();
        VFS_CONTEXT_CREATED.fetch_add(1, Ordering::Relaxed);
        VFS_CONTEXT_LIVE.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            cwd_state: sync::Spinlock::new(CwdState { cwd, cwd_mount }),
            root: VfsRoot::new(self.root.root(), self.root.mount()),
            mount_ns: Arc::clone(&self.mount_ns),
            cred: sync::Spinlock::new(self.cred()),
            umask: sync::Spinlock::new(*self.umask.lock()),
            mutation_gate: sync::Spinlock::new(()),
            generation: AtomicU64::new(0),
            limits: Arc::clone(&self.limits),
        })
    }

    #[kernel_symbols::export(
        name = "vfs.VfsContext.clone_with_new_ns",
        contract = "kernel.vfs.context@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_ADMIN,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn clone_with_new_ns(&self) -> VfsResult<Self> {
        let new_ns = self.mount_ns.clone_namespace()?;
        let cwd_st = self.cwd_state.lock();
        let new_cwd_mount = new_ns
            .find_mount_for_root(&cwd_st.cwd_mount.mount_root)
            .unwrap_or_else(|| Arc::clone(&new_ns.root.lock()));
        new_cwd_mount.inc_open();
        let root_dentry = self.root.root();
        let new_root_mount = new_ns
            .find_mount_for_root(&self.root.mount().mount_root)
            .unwrap_or_else(|| Arc::clone(&new_ns.root.lock()));
        VFS_CONTEXT_CREATED.fetch_add(1, Ordering::Relaxed);
        VFS_CONTEXT_LIVE.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            cwd_state: sync::Spinlock::new(CwdState {
                cwd: Arc::clone(&cwd_st.cwd),
                cwd_mount: new_cwd_mount,
            }),
            root: VfsRoot::new(root_dentry, new_root_mount),
            mount_ns: new_ns,
            cred: sync::Spinlock::new(self.cred()),
            umask: sync::Spinlock::new(*self.umask.lock()),
            mutation_gate: sync::Spinlock::new(()),
            generation: AtomicU64::new(0),
            limits: Arc::clone(&self.limits),
        })
    }
}

impl Drop for VfsContext {
    fn drop(&mut self) {
        VFS_CONTEXT_DROPPED.fetch_add(1, Ordering::Relaxed);
        VFS_CONTEXT_LIVE.fetch_sub(1, Ordering::Relaxed);
        self.cwd_state.lock().cwd_mount.dec_open();
    }
}

#[cfg(test)]
mod tests;
