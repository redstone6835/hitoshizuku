//! pidfd 兼容文件对象。
//!
//! pidfd 是 Linux ABI 对任务句柄的 fd 投影；sched core 仍然只认识 `Arc<Task>`，
//! 这里负责把它包装成 VFS `FileOps`，供 clone3/clone 和 waitid(P_PIDFD) 使用。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, TaskState};
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::Dentry;
use vfs::error::{VfsError, VfsResult};
use vfs::fdtable::{Fd, FdFlags, FdTable};
use vfs::file::{DirEntry, File, FileOps, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::{Mount, MountFlags};
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{InodeCache, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

struct PidfdFs {
    mount: Arc<Mount>,
    inode: Arc<Inode>,
    dentry: Arc<Dentry>,
}

static PIDFD_FS: Spinlock<Option<PidfdFs>> = Spinlock::new(None);

pub struct PidfdFileOps {
    task: Arc<Task>,
}

impl PidfdFileOps {
    fn new(task: Arc<Task>) -> Self {
        Self { task }
    }

    pub fn task(&self) -> Arc<Task> {
        Arc::clone(&self.task)
    }
}

impl FileOps for PidfdFileOps {
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

    fn poll(&self, interest: PollEvents) -> PollEvents {
        if matches!(self.task.state(), TaskState::Zombie | TaskState::Dead) {
            interest.intersect(PollEvents::POLLIN)
        } else {
            PollEvents::default()
        }
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct PidfdInodeOps;

impl InodeOps for PidfdInodeOps {
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

struct PidfdSuperblockOps;

impl SuperblockOps for PidfdSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x70696466,
            block_size: sb.block_size as u64,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: 1,
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: sb.name_max,
        })
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn get_or_init_pidfd_fs() -> (Arc<Mount>, Arc<Inode>, Arc<Dentry>) {
    let mut guard = PIDFD_FS.lock();
    if guard.is_none() {
        let sb = Superblock::new(|weak| {
            let root_inode = Inode::new(
                InodeId {
                    fs_id: FsId::new(0x7069646664667300),
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
                    uid: Uid(0),
                    gid: Gid(0),
                    atime: Timespec::ZERO,
                    mtime: Timespec::ZERO,
                    ctime: Timespec::ZERO,
                    blocks: 0,
                },
                Arc::new(PidfdInodeOps),
                weak.clone(),
            );
            let root_dentry = Dentry::new_positive("", None, root_inode.clone());
            Superblock {
                fs_type: "pidfdfs",
                fs_id: FsId::new(0x7069646664667300),
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: InodeCache::new(),
                ops: Box::new(PidfdSuperblockOps),
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
        *guard = Some(PidfdFs {
            mount: Arc::clone(&mount),
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

pub fn create(fdt: &FdTable, cred: Arc<Credentials>, task: Arc<Task>) -> Result<Fd, Errno> {
    let (mount, inode, dentry) = get_or_init_pidfd_fs();
    let file = Arc::new(File::new(
        inode,
        OpenOptions::default(),
        cred,
        Box::new(PidfdFileOps::new(task)),
        dentry,
        Arc::clone(&mount),
    ));
    mount.inc_open();
    fdt.alloc_fd(file, FdFlags::CLOEXEC)
        .map_err(|err| err.to_errno())
}

pub fn task_from_file(file: &Arc<File>) -> Option<Arc<Task>> {
    file.downcast_ops::<PidfdFileOps>().map(PidfdFileOps::task)
}
