//! Anonymous file helpers for fd-backed kernel objects.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::vfs::cred::Credentials;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{File, FileOps, OpenOptions};
use crate::vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::vfs::mount::{Mount, MountFlags};
use crate::vfs::stat::{DevId, FileMode, FileType, FsId, Timespec};
use crate::vfs::superblock::{InodeCache, Superblock, SuperblockOps};
use crate::vfs::sync::Spinlock;

struct AnonFs {
    mount: Arc<Mount>,
    inode: Arc<Inode>,
    dentry: Arc<Dentry>,
}

static ANON_FS: Spinlock<Option<AnonFs>> = Spinlock::new(None);
static NEXT_ANON_INO: AtomicU64 = AtomicU64::new(2);

fn get_or_init_anon_fs() -> (Arc<Mount>, Arc<Inode>, Arc<Dentry>) {
    let mut guard = ANON_FS.lock();
    if guard.is_none() {
        let sb = Superblock::new(|weak| {
            let root_inode = Inode::new(
                InodeId {
                    fs_id: FsId::new(0x616e6f6e66730000),
                    ino: 1,
                },
                FileType::Regular,
                DevId::new(0, 0),
                4096,
                None,
                InodeMeta {
                    size: 0,
                    nlink: 1,
                    mode: FileMode::new(0o600),
                    uid: crate::vfs::cred::Uid(0),
                    gid: crate::vfs::cred::Gid(0),
                    atime: Timespec::ZERO,
                    mtime: Timespec::ZERO,
                    ctime: Timespec::ZERO,
                    blocks: 0,
                },
                Arc::new(AnonInodeOps),
                weak.clone(),
            );
            let root_dentry = Dentry::new_positive("", None, root_inode.clone());
            Superblock {
                fs_type: "anonfs",
                fs_id: FsId::new(0x616e6f6e66730000),
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: InodeCache::new(),
                ops: Box::new(AnonSuperblockOps),
                self_weak: weak.clone(),
            }
        });
        let mount = Mount::new(
            Arc::clone(&sb),
            Arc::clone(&sb.root_dentry),
            Arc::clone(&sb.root_dentry),
            MountFlags::default(),
            None,
        );
        *guard = Some(AnonFs {
            mount,
            inode: Arc::clone(&sb.root_inode),
            dentry: Arc::clone(&sb.root_dentry),
        });
    }

    let fs = guard.as_ref().unwrap();
    (
        Arc::clone(&fs.mount),
        Arc::clone(&fs.inode),
        Arc::clone(&fs.dentry),
    )
}

struct AnonInodeOps;

impl InodeOps for AnonInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct AnonSuperblockOps;

impl SuperblockOps for AnonSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn statfs(&self, _sb: &Arc<Superblock>) -> VfsResult<crate::vfs::stat::FsStat> {
        Err(VfsError::NotSupported)
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }
}

pub fn new_file(
    cred: Arc<Credentials>,
    flags: OpenOptions,
    ops: Box<dyn FileOps + Send + Sync>,
) -> Arc<File> {
    let (mount, inode, dentry) = get_or_init_anon_fs();
    let file = Arc::new(File::new(
        inode,
        flags,
        cred,
        ops,
        dentry,
        Arc::clone(&mount),
    ));
    mount.inc_open();
    file
}

/// 创建带有私有 inode 的匿名文件。
///
/// 纯事件对象可以共享 anonfs 根 inode；需要稳定 inode 身份、文件大小或私有 inode
/// 操作的对象（例如 memfd）则通过这里分配独立 inode。这样对象身份属于 VFS 语义，
/// 不需要泄漏到 syscall 兼容层里手工维护。
pub fn new_private_file(
    cred: Arc<Credentials>,
    flags: OpenOptions,
    kind: FileType,
    mode: FileMode,
    size: u64,
    inode_ops: Arc<dyn InodeOps + Send + Sync>,
    file_ops: Box<dyn FileOps + Send + Sync>,
) -> Arc<File> {
    let (mount, _root_inode, root_dentry) = get_or_init_anon_fs();
    let ino = NEXT_ANON_INO.fetch_add(1, Ordering::Relaxed);
    let inode = Inode::new(
        InodeId {
            fs_id: mount.superblock.fs_id,
            ino,
        },
        kind,
        DevId::new(0, 0),
        mount.superblock.block_size,
        mount.superblock.dev_id,
        InodeMeta {
            size,
            nlink: 1,
            mode,
            uid: cred.uid,
            gid: cred.gid,
            atime: Timespec::ZERO,
            mtime: Timespec::ZERO,
            ctime: Timespec::ZERO,
            blocks: size.div_ceil(512),
        },
        inode_ops,
        Arc::downgrade(&mount.superblock),
    );
    let dentry = Dentry::new_positive("anon", Some(root_dentry), Arc::clone(&inode));
    let file = Arc::new(File::new(
        inode,
        flags,
        cred,
        file_ops,
        dentry,
        Arc::clone(&mount),
    ));
    mount.inc_open();
    file
}

pub fn create_fd(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    file_flags: OpenOptions,
    fd_flags: FdFlags,
    ops: Box<dyn FileOps + Send + Sync>,
) -> VfsResult<Fd> {
    let file = new_file(cred, file_flags, ops);
    fdt.alloc_fd(file, fd_flags)
}

/// 创建带有私有 inode 的匿名 fd。
pub fn create_private_fd(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    file_flags: OpenOptions,
    fd_flags: FdFlags,
    kind: FileType,
    mode: FileMode,
    size: u64,
    inode_ops: Arc<dyn InodeOps + Send + Sync>,
    file_ops: Box<dyn FileOps + Send + Sync>,
) -> VfsResult<Fd> {
    let file = new_private_file(cred, file_flags, kind, mode, size, inode_ops, file_ops);
    fdt.alloc_fd(file, fd_flags)
}
