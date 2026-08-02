//! 共享 Superblock 的挂载与卸载生命周期测试。

use alloc::boxed::Box;
use alloc::sync::Arc;

use ktest::ktest;

use crate::cred::{Credentials, Gid, Uid};
use crate::dentry::Dentry;
use crate::error::{VfsError, VfsResult};
use crate::file::{FileOps, OpenOptions};
use crate::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::mount::{Mount, MountFlags, MountNamespace};
use crate::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use crate::superblock::{InodeCache, Superblock, SuperblockOps};

struct EmptyInodeOps;

impl InodeOps for EmptyInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotFound)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct EmptySuperblockOps;

impl SuperblockOps for EmptySuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn statfs(&self, _sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Err(VfsError::NotSupported)
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }
}

fn inode_meta() -> InodeMeta {
    InodeMeta {
        size: 0,
        nlink: 2,
        mode: FileMode::new(0o755),
        uid: Uid(0),
        gid: Gid(0),
        atime: Timespec::ZERO,
        mtime: Timespec::ZERO,
        ctime: Timespec::ZERO,
        blocks: 0,
    }
}

fn test_superblock(id: u64) -> Arc<Superblock> {
    let fs_id = FsId::new(id);
    Superblock::new(move |weak| {
        let root_inode = Inode::new(
            InodeId { fs_id, ino: 1 },
            FileType::Directory,
            DevId::new(0, 0),
            4096,
            None,
            inode_meta(),
            Arc::new(EmptyInodeOps),
            weak.clone(),
        );
        let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
        Superblock {
            fs_type: "mount-test",
            fs_id,
            dev_id: None,
            block_size: 4096,
            name_max: 255,
            root_inode,
            root_dentry,
            inode_cache: InodeCache::new(),
            ops: Box::new(EmptySuperblockOps),
            self_weak: weak,
        }
    })
}

fn mountpoint(parent: &Arc<Dentry>, sb: &Arc<Superblock>, name: &str, ino: u64) -> Arc<Dentry> {
    let inode = Inode::new(
        InodeId {
            fs_id: sb.fs_id,
            ino,
        },
        FileType::Directory,
        DevId::new(0, 0),
        4096,
        None,
        inode_meta(),
        Arc::new(EmptyInodeOps),
        Arc::downgrade(sb),
    );
    Dentry::new_positive(name, Some(Arc::clone(parent)), inode)
}

fn namespace_with_root(id: u64) -> (Arc<MountNamespace>, Arc<Mount>, Arc<Superblock>) {
    let root_sb = test_superblock(id);
    let root = Arc::clone(&root_sb.root_dentry);
    let root_mount = Mount::new(
        Arc::clone(&root_sb),
        Arc::clone(&root),
        root,
        MountFlags::default(),
        None,
    );
    (
        MountNamespace::new(id, Arc::clone(&root_mount)),
        root_mount,
        root_sb,
    )
}

/// 同一 namespace 中卸载一个共享实例，不得让另一个挂载点的根目录失效。
#[ktest]
fn shared_superblock_survives_partial_umount() {
    let (namespace, root_mount, root_sb) = namespace_with_root(0x5100);
    let shared_sb = test_superblock(0x5200);
    let first_point = mountpoint(&root_sb.root_dentry, &root_sb, "first", 2);
    let second_point = mountpoint(&root_sb.root_dentry, &root_sb, "second", 3);

    let first = namespace
        .mount_at(
            first_point,
            Arc::clone(&root_mount),
            Arc::clone(&shared_sb),
            MountFlags::default(),
        )
        .unwrap();
    namespace
        .mount_at(
            Arc::clone(&second_point),
            root_mount,
            Arc::clone(&shared_sb),
            MountFlags::default(),
        )
        .unwrap();

    namespace.umount(&second_point, false).unwrap();

    assert!(shared_sb.root_dentry.is_positive());
    let indexed = namespace
        .find_mount_for_root(&shared_sb.root_dentry)
        .expect("共享根仍应保留第一个挂载索引");
    assert!(Arc::ptr_eq(&indexed, &first));
}

/// clone namespace 共享同一 Superblock；原 namespace 卸载后克隆仍必须可用。
#[ktest]
fn shared_superblock_survives_umount_in_another_namespace() {
    let (namespace, root_mount, root_sb) = namespace_with_root(0x5300);
    let shared_sb = test_superblock(0x5400);
    let point = mountpoint(&root_sb.root_dentry, &root_sb, "shared", 2);

    namespace
        .mount_at(
            Arc::clone(&point),
            root_mount,
            Arc::clone(&shared_sb),
            MountFlags::default(),
        )
        .unwrap();
    let cloned = namespace.clone_namespace().unwrap();

    namespace.umount(&point, false).unwrap();
    assert!(shared_sb.root_dentry.is_positive());

    drop(cloned);
    assert!(shared_sb.root_dentry.is_invalid());
}
