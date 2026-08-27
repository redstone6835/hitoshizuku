//! O_DIRECT 打开校验的宿主单测：支持/不支持直接 I/O 的文件系统。

use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::mount::MountFlags;
use crate::stat::{DevId, FileType};
use crate::stat::{FsId, FsStat};
use crate::superblock::{FsDriverFlags, Superblock, SuperblockOps};
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};

#[derive(Default)]
struct EmptyInodeOps;

impl InodeOps for EmptyInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotFound)
    }
    fn open(
        &self,
        _inode: &Inode,
        _opts: &crate::file::OpenOptions,
        _cred: &crate::cred::Credentials,
    ) -> VfsResult<Box<dyn crate::file::FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 默认（不支持 O_DIRECT）超级块操作。
struct PlainSuperblockOps;

impl SuperblockOps for PlainSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
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

/// 支持 O_DIRECT 的超级块操作（模拟 ext4）。
struct DirectSuperblockOps;

impl SuperblockOps for DirectSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
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
    fn supports_direct_io(&self) -> bool {
        true
    }
}

fn inode_meta(kind: FileType) -> InodeMeta {
    InodeMeta {
        size: 0,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        mode: crate::stat::FileMode::new(0o755),
        uid: crate::cred::Uid(0),
        gid: crate::cred::Gid(0),
        atime: crate::stat::Timespec::ZERO,
        mtime: crate::stat::Timespec::ZERO,
        ctime: crate::stat::Timespec::ZERO,
        blocks: 0,
    }
}

fn make_sb(ops: Box<dyn SuperblockOps + Send + Sync>) -> Arc<Superblock> {
    let fs_id = FsId::new(0x1234_5678);
    let dev_id = Some(DevId::new(0, 0));
    Superblock::new(move |weak| {
        let root_inode = Inode::new(
            InodeId { fs_id, ino: 1 },
            FileType::Directory,
            DevId::new(0, 0),
            4096,
            dev_id,
            inode_meta(FileType::Directory),
            Arc::new(EmptyInodeOps),
            weak.clone(),
        );
        let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
        Superblock {
            fs_type: "direct-io-test",
            fs_id,
            dev_id,
            block_size: 4096,
            name_max: 255,
            root_inode,
            root_dentry,
            inode_cache: crate::superblock::InodeCache::new(),
            ops,
            self_weak: weak,
        }
    })
}

fn make_file(sb: &Arc<Superblock>, ino: u64) -> Arc<Inode> {
    Inode::new(
        InodeId {
            fs_id: sb.fs_id,
            ino,
        },
        FileType::Regular,
        DevId::new(0, 0),
        4096,
        sb.dev_id,
        inode_meta(FileType::Regular),
        Arc::new(EmptyInodeOps),
        Arc::downgrade(sb),
    )
}

#[test]
fn plain_fs_rejects_o_direct() {
    let sb = make_sb(Box::new(PlainSuperblockOps));
    assert!(!sb.ops.supports_direct_io());
    let file = make_file(&sb, 2);
    let err = crate::operation::check_direct_io_supported(&file, true).unwrap_err();
    assert_eq!(err, VfsError::InvalidArgument);
    // 非 O_DIRECT 打开不受影响。
    assert!(crate::operation::check_direct_io_supported(&file, false).is_ok());
}

#[test]
fn direct_fs_accepts_o_direct() {
    let sb = make_sb(Box::new(DirectSuperblockOps));
    assert!(sb.ops.supports_direct_io());
    let file = make_file(&sb, 2);
    assert!(crate::operation::check_direct_io_supported(&file, true).is_ok());
}

#[test]
fn driver_flags_default_has_no_direct_bit() {
    // 防回归：新增的能力位不应出现在默认标志里。
    let flags = FsDriverFlags::default();
    assert!(!flags.has(FsDriverFlags(1 << 31)));
}
