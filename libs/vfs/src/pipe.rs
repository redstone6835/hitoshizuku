use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::vfs::cred::Credentials;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::{DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::vfs::mount::{Mount, MountFlags};
use crate::vfs::stat::{DevId, FileMode, FileType, FsId, Timespec};
use crate::vfs::superblock::{InodeCache, Superblock, SuperblockOps};
use crate::vfs::sync::Spinlock;

const PIPE_CAPACITY: usize = 65536;

struct PipeInner {
    data: Vec<u8>,
    read_pos: usize,
    write_pos: usize,
    reader_count: u32,
    writer_count: u32,
}

pub struct Pipe {
    inner: Spinlock<PipeInner>,
    read_wait: WaitQueue,
    write_wait: WaitQueue,
}

impl Pipe {
    fn new() -> Self {
        Self {
            inner: Spinlock::new(PipeInner {
                data: vec![0u8; PIPE_CAPACITY],
                read_pos: 0,
                write_pos: 0,
                reader_count: 1,
                writer_count: 1,
            }),
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
        }
    }

    fn available(&self, inner: &PipeInner) -> usize {
        inner.write_pos.saturating_sub(inner.read_pos)
    }

    fn free_space(&self, inner: &PipeInner) -> usize {
        PIPE_CAPACITY.saturating_sub(self.available(inner))
    }

    fn write_data(&self, inner: &mut PipeInner, src: &[u8]) -> usize {
        let free = self.free_space(inner);
        let n = src.len().min(free);
        if n == 0 {
            return 0;
        }
        let cap = PIPE_CAPACITY;
        let start = inner.write_pos % cap;
        let first = (cap - start).min(n);
        inner.data[start..start + first].copy_from_slice(&src[..first]);
        if first < n {
            let second = n - first;
            inner.data[..second].copy_from_slice(&src[first..n]);
        }
        inner.write_pos = inner.write_pos.wrapping_add(n);
        n
    }

    fn read_data(&self, inner: &mut PipeInner, dst: &mut [u8]) -> usize {
        let avail = self.available(inner);
        let n = dst.len().min(avail);
        if n == 0 {
            return 0;
        }
        let cap = PIPE_CAPACITY;
        let start = inner.read_pos % cap;
        let first = (cap - start).min(n);
        dst[..first].copy_from_slice(&inner.data[start..start + first]);
        if first < n {
            let second = n - first;
            dst[first..n].copy_from_slice(&inner.data[..second]);
        }
        inner.read_pos = inner.read_pos.wrapping_add(n);
        n
    }
}

pub struct PipeReadEnd {
    pipe: Arc<Pipe>,
}

impl PipeReadEnd {
    pub fn new(pipe: Arc<Pipe>, _nonblock: bool) -> Self {
        Self { pipe }
    }
}

impl FileOps for PipeReadEnd {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let mut inner = self.pipe.inner.lock();
        let avail = self.pipe.available(&inner);
        if avail > 0 {
            let n = self.pipe.read_data(&mut inner, buf);
            drop(inner);
            self.pipe.write_wait.wake_all();
            return Ok(n);
        }
        if inner.writer_count == 0 {
            return Ok(0);
        }

        // FIXME: 当前基于 write_pos 的启发式会误判流水线写入场景会产生错误
        if inner.write_pos > 0 {
            inner.writer_count = 0;
            return Ok(0);
        }
        Err(VfsError::WouldBlock)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
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
        let inner = self.pipe.inner.lock();
        let mut ready = PollEvents::default();
        let avail = self.pipe.available(&inner);
        if avail > 0 {
            ready = ready.with(PollEvents::POLLIN);
        }
        // FIXME: 此处的 write_pos>0 启发式与 read_at 中的自旋死锁检测一致，
        if inner.writer_count == 0
            || (avail == 0 && inner.write_pos > 0)
        {
            ready = ready.with(PollEvents::POLLHUP);
        }
        ready.intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI) {
            self.pipe.read_wait.enqueue(task);
        }
        if interest.has(PollEvents::POLLHUP) || interest.has(PollEvents::POLLERR) {
            self.pipe.read_wait.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.pipe.read_wait.remove(task);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        let mut inner = self.pipe.inner.lock();
        inner.reader_count = inner.reader_count.saturating_sub(1);
        let last = inner.reader_count == 0;
        drop(inner);
        if last {
            self.pipe.write_wait.wake_all();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct PipeWriteEnd {
    pipe: Arc<Pipe>,
}

impl PipeWriteEnd {
    pub fn new(pipe: Arc<Pipe>, _nonblock: bool) -> Self {
        Self { pipe }
    }
}

impl FileOps for PipeWriteEnd {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        let mut inner = self.pipe.inner.lock();
        if inner.reader_count == 0 {
            return Err(VfsError::BrokenPipe);
        }
        let free = self.pipe.free_space(&inner);
        if free > 0 {
            let n = self.pipe.write_data(&mut inner, buf);
            drop(inner);
            self.pipe.read_wait.wake_all();
            return Ok(n);
        }
        Err(VfsError::WouldBlock)
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
        let inner = self.pipe.inner.lock();
        let mut ready = PollEvents::default();
        if self.pipe.free_space(&inner) > 0 {
            ready = ready.with(PollEvents::POLLOUT);
        }
        if inner.reader_count == 0 {
            ready = ready.with(PollEvents::POLLERR);
        }
        ready.intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLOUT) {
            self.pipe.write_wait.enqueue(task);
        }
        if interest.has(PollEvents::POLLHUP) || interest.has(PollEvents::POLLERR) {
            self.pipe.write_wait.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.pipe.write_wait.remove(task);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        let mut inner = self.pipe.inner.lock();
        inner.writer_count = inner.writer_count.saturating_sub(1);
        let last = inner.writer_count == 0;
        drop(inner);
        if last {
            self.pipe.read_wait.wake_all();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct PipeInodeOps;

impl InodeOps for PipeInodeOps {
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

struct PipeSuperblockOps;

impl SuperblockOps for PipeSuperblockOps {
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

struct PipeFs {
    mount: Arc<Mount>,
    inode: Arc<Inode>,
    dentry: Arc<Dentry>,
}

static PIPE_FS: Spinlock<Option<PipeFs>> = Spinlock::new(None);

fn get_or_init_pipe_fs() -> (Arc<Mount>, Arc<Inode>, Arc<Dentry>) {
    let mut guard = PIPE_FS.lock();
    if guard.is_none() {
        let sb = Superblock::new(|weak| {
            let root_inode = Inode::new(
                InodeId {
                    fs_id: FsId::new(0x7069706566730000),
                    ino: 1,
                },
                FileType::Fifo,
                DevId::new(0, 0),
                4096,
                None,
                InodeMeta {
                    size: 0,
                    nlink: 1,
                    mode: FileMode::new(0o644),
                    uid: crate::vfs::cred::Uid(0),
                    gid: crate::vfs::cred::Gid(0),
                    atime: Timespec::ZERO,
                    mtime: Timespec::ZERO,
                    ctime: Timespec::ZERO,
                    blocks: 0,
                },
                Arc::new(PipeInodeOps),
                weak.clone(),
            );
            let root_dentry = Dentry::new_positive("", None, root_inode.clone());
            Superblock {
                fs_type: "pipefs",
                fs_id: FsId::new(0x7069706566730000),
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: InodeCache::new(),
                ops: Box::new(PipeSuperblockOps),
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

        let pipe_fs = PipeFs {
            mount: Arc::clone(&mount),
            inode: Arc::clone(&sb.root_inode),
            dentry: Arc::clone(&sb.root_dentry),
        };
        *guard = Some(pipe_fs);
    }
    let fs = guard.as_ref().unwrap();
    (
        Arc::clone(&fs.mount),
        Arc::clone(&fs.inode),
        Arc::clone(&fs.dentry),
    )
}

pub fn new_pipe(cred: Arc<Credentials>, nonblock: bool) -> VfsResult<(Arc<File>, Arc<File>)> {
    let (mount, inode, dentry) = get_or_init_pipe_fs();
    let pipe = Arc::new(Pipe::new());

    let read_flags = OpenOptions {
        nonblock,
        ..Default::default()
    };
    let write_flags = OpenOptions {
        access: crate::vfs::file::AccessMode::WriteOnly,
        nonblock,
        ..Default::default()
    };

    let read_end = File::new(
        Arc::clone(&inode),
        read_flags,
        Arc::clone(&cred),
        Box::new(PipeReadEnd::new(Arc::clone(&pipe), nonblock)),
        Arc::clone(&dentry),
        Arc::clone(&mount),
    );

    let write_end = File::new(
        Arc::clone(&inode),
        write_flags,
        cred,
        Box::new(PipeWriteEnd::new(pipe, nonblock)),
        Arc::clone(&dentry),
        Arc::clone(&mount),
    );

    mount.inc_open();
    mount.inc_open();

    Ok((Arc::new(read_end), Arc::new(write_end)))
}
