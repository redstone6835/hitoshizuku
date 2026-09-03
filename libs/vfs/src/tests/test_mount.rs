//! 共享 Superblock 的挂载与卸载生命周期测试。

use alloc::boxed::Box;
use alloc::sync::Arc;

use ktest::ktest;

use crate::cred::{Credentials, Gid, Uid};
use crate::dentry::Dentry;
use crate::error::{VfsError, VfsResult};
use crate::file::{FileOps, OpenOptions};
use crate::fs_context::{FsContext, land_mount};
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

/// `fspick`/`open_tree` contexts retain the per-mount access constraints rather
/// than silently reverting to the default read-write flags.
#[ktest]
fn fs_context_from_mount_preserves_mount_flags() {
    let (_namespace, root_mount, _root_sb) = namespace_with_root(0x5500);
    let flags = MountFlags::RDONLY
        .with(MountFlags::NOSUID)
        .with(MountFlags::NOEXEC);
    root_mount.set_flags(flags);

    let ctx = FsContext::from_mount(&root_mount);
    assert_eq!(ctx.flags(), flags);
}

/// A detached mount context is consumed by the first successful move_mount;
/// reusing its fd must not attach a second instance.
#[ktest]
fn land_mount_consumes_context_after_attach() {
    let (namespace, root_mount, root_sb) = namespace_with_root(0x5600);
    let source_sb = test_superblock(0x5700);
    let source_point = mountpoint(&root_sb.root_dentry, &root_sb, "source", 2);
    let source_mount = namespace
        .mount_at(
            source_point,
            Arc::clone(&root_mount),
            Arc::clone(&source_sb),
            MountFlags::RDONLY,
        )
        .unwrap();
    let target = mountpoint(&root_sb.root_dentry, &root_sb, "target", 3);
    let ctx = FsContext::from_mount(&source_mount);

    land_mount(&namespace, &ctx, Arc::clone(&target), &root_mount).unwrap();
    assert!(ctx.is_consumed());

    let second = land_mount(&namespace, &ctx, target, &root_mount);
    assert_eq!(second, Err(VfsError::BadFileDescriptor));
}

/// Moving a mount onto itself must be rejected before its indexes or location
/// are changed.
#[ktest]
fn move_mount_rejects_self_parent() {
    let (namespace, root_mount, root_sb) = namespace_with_root(0x5800);
    let source_sb = test_superblock(0x5900);
    let source_point = mountpoint(&root_sb.root_dentry, &root_sb, "source", 2);
    let source_mount = namespace
        .mount_at(
            Arc::clone(&source_point),
            Arc::clone(&root_mount),
            source_sb,
            MountFlags::default(),
        )
        .unwrap();
    let target = mountpoint(&root_sb.root_dentry, &root_sb, "target", 3);

    assert_eq!(
        namespace.move_mount_at(&source_mount, target, Arc::clone(&source_mount)),
        Err(VfsError::InvalidArgument)
    );
    assert!(Arc::ptr_eq(&source_mount.mountpoint(), &source_point));
    assert!(
        source_mount
            .location
            .lock()
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .is_some_and(|parent| Arc::ptr_eq(&parent, &root_mount))
    );
    assert!(
        namespace
            .lookup_mount(&source_point)
            .is_some_and(|mount| Arc::ptr_eq(&mount, &source_mount))
    );
}

/// Moving a mount below one of its descendants must be rejected; otherwise
/// the descendant's parent chain would eventually point back to the source.
#[ktest]
fn move_mount_rejects_descendant_parent() {
    let (namespace, root_mount, root_sb) = namespace_with_root(0x5a00);
    let source_sb = test_superblock(0x5b00);
    let source_point = mountpoint(&root_sb.root_dentry, &root_sb, "source", 2);
    let source_mount = namespace
        .mount_at(
            source_point,
            Arc::clone(&root_mount),
            Arc::clone(&source_sb),
            MountFlags::default(),
        )
        .unwrap();
    let child_sb = test_superblock(0x5c00);
    let child_point = mountpoint(&source_sb.root_dentry, &source_sb, "child", 2);
    let child_mount = namespace
        .mount_at(
            child_point,
            Arc::clone(&source_mount),
            Arc::clone(&child_sb),
            MountFlags::default(),
        )
        .unwrap();
    let target = mountpoint(&child_sb.root_dentry, &child_sb, "target", 2);

    assert_eq!(
        namespace.move_mount_at(&source_mount, target, Arc::clone(&child_mount)),
        Err(VfsError::InvalidArgument)
    );
    assert!(
        source_mount
            .location
            .lock()
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .is_some_and(|parent| Arc::ptr_eq(&parent, &root_mount))
    );
    assert!(
        child_mount
            .location
            .lock()
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .is_some_and(|parent| Arc::ptr_eq(&parent, &source_mount))
    );
}

/// A malformed parent cycle must terminate the validation with `EINVAL`
/// instead of hanging in the move operation.
#[ktest]
fn move_mount_rejects_preexisting_parent_cycle() {
    let (namespace, root_mount, root_sb) = namespace_with_root(0x5d00);
    let source_sb = test_superblock(0x5e00);
    let source_point = mountpoint(&root_sb.root_dentry, &root_sb, "source", 2);
    let source_mount = namespace
        .mount_at(
            source_point,
            Arc::clone(&root_mount),
            source_sb,
            MountFlags::default(),
        )
        .unwrap();
    let target = mountpoint(&root_sb.root_dentry, &root_sb, "target", 3);

    // Keep the cycle in weak links only so dropping the test namespace remains
    // well-founded after restoring the root's normal parent.
    root_mount.location.lock().parent = Some(Arc::downgrade(&root_mount));
    let result = namespace.move_mount_at(&source_mount, target, root_mount.clone());
    root_mount.location.lock().parent = None;

    assert_eq!(result, Err(VfsError::InvalidArgument));
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
