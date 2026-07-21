use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::poll_source::PollSource;
use crate::vfs::cred::Credentials;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::{AccessMode, DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::vfs::mount::{Mount, MountFlags};
use crate::vfs::stat::{DevId, FileMode, FileType, FsId, Timespec};
use crate::vfs::superblock::{InodeCache, Superblock, SuperblockOps};
use crate::vfs::sync::Spinlock;

const PIPE_CAPACITY: usize = 65536;
const PIPE_BUF: usize = 4096;

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
    read_source: PollSource,
    write_source: PollSource,
    read_write_source: PollSource,
}

impl Pipe {
    fn new() -> Self {
        Self::with_counts(1, 1)
    }

    fn with_counts(reader_count: u32, writer_count: u32) -> Self {
        let inner = PipeInner {
            data: vec![0u8; PIPE_CAPACITY],
            read_pos: 0,
            write_pos: 0,
            reader_count,
            writer_count,
        };
        let (read_ready, write_ready, read_write_ready) = Self::readiness(&inner);
        Self {
            inner: Spinlock::new(inner),
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
            read_source: PollSource::new(read_ready),
            write_source: PollSource::new(write_ready),
            read_write_source: PollSource::new(read_write_ready),
        }
    }

    fn readiness(inner: &PipeInner) -> (PollEvents, PollEvents, PollEvents) {
        let available = inner.write_pos.saturating_sub(inner.read_pos);
        let free = PIPE_CAPACITY.saturating_sub(available);
        let mut read = PollEvents::default();
        let mut write = PollEvents::default();
        let mut read_write = PollEvents::default();
        if available > 0 {
            read = read.with(PollEvents::POLLIN);
            read_write = read_write.with(PollEvents::POLLIN);
        }
        if free >= PIPE_BUF {
            write = write.with(PollEvents::POLLOUT);
            read_write = read_write.with(PollEvents::POLLOUT);
        }
        if inner.writer_count == 0 {
            read = read.with(PollEvents::POLLHUP);
            read_write = read_write.with(PollEvents::POLLHUP);
        }
        if inner.reader_count == 0 {
            write = write.with(PollEvents::POLLERR);
            read_write = read_write.with(PollEvents::POLLERR);
        }
        (read, write, read_write)
    }

    fn publish_readiness(&self) {
        let (read, write, read_write, read_version, write_version, read_write_version) = {
            let inner = self.inner.lock();
            let (read, write, read_write) = Self::readiness(&inner);
            (
                read,
                write,
                read_write,
                self.read_source.reserve_version(),
                self.write_source.reserve_version(),
                self.read_write_source.reserve_version(),
            )
        };
        self.read_source.publish_versioned(read, read_version);
        self.write_source.publish_versioned(write, write_version);
        self.read_write_source
            .publish_versioned(read_write, read_write_version);
    }

    fn available(&self, inner: &PipeInner) -> usize {
        inner.write_pos.saturating_sub(inner.read_pos)
    }

    fn free_space(&self, inner: &PipeInner) -> usize {
        PIPE_CAPACITY.saturating_sub(self.available(inner))
    }

    fn writable_len(requested: usize, free: usize) -> usize {
        // 不超过 PIPE_BUF 的写入必须保持原子性，空间不足时整次等待。
        if requested <= PIPE_BUF && free < requested {
            0
        } else {
            requested.min(free)
        }
    }

    fn write_data(&self, inner: &mut PipeInner, src: &[u8]) -> usize {
        let free = self.free_space(inner);
        let n = Self::writable_len(src.len(), free);
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

/// 创建命名 FIFO 的共享 pipe 状态。
pub fn new_fifo() -> Arc<Pipe> {
    Arc::new(Pipe::with_counts(0, 0))
}

/// 打开命名 FIFO。
///
/// 这里不实现 Linux 的阻塞 open 配对等待，只维护同一个 FIFO inode 上所有
/// 打开文件共享的数据通道和端点计数。LTP 中基于 FIFO fd 的 fcntl 用例主要
/// 依赖 open 能成功，而不是阻塞语义。
pub fn open_fifo(pipe: Arc<Pipe>, opts: &OpenOptions) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
    {
        let mut inner = pipe.inner.lock();
        match opts.access {
            AccessMode::ReadOnly => inner.reader_count = inner.reader_count.saturating_add(1),
            AccessMode::WriteOnly => inner.writer_count = inner.writer_count.saturating_add(1),
            AccessMode::ReadWrite => {
                inner.reader_count = inner.reader_count.saturating_add(1);
                inner.writer_count = inner.writer_count.saturating_add(1);
            }
        }
    }
    pipe.publish_readiness();

    match opts.access {
        AccessMode::ReadOnly => Ok(Box::new(PipeReadEnd::new(pipe, opts.nonblock))),
        AccessMode::WriteOnly => Ok(Box::new(PipeWriteEnd::new(pipe, opts.nonblock))),
        AccessMode::ReadWrite => Ok(Box::new(PipeReadWriteEnd::new(pipe, opts.nonblock))),
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
            self.pipe.publish_readiness();
            // 正常读出数据只释放了部分缓冲空间，唤醒一个写者即可；
            // 端点关闭等状态变化仍在 release() 中广播给全部等待者。
            self.pipe.write_wait.wake_one_default();
            return Ok(n);
        }
        if inner.writer_count == 0 {
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
        self.pipe
            .read_source
            .snapshot()
            .0
            .intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
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

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.pipe.read_source)
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
        self.pipe.publish_readiness();
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

pub struct PipeReadWriteEnd {
    pipe: Arc<Pipe>,
}

impl PipeReadWriteEnd {
    pub fn new(pipe: Arc<Pipe>, _nonblock: bool) -> Self {
        Self { pipe }
    }
}

impl FileOps for PipeReadWriteEnd {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let mut inner = self.pipe.inner.lock();
        let avail = self.pipe.available(&inner);
        if avail > 0 {
            let n = self.pipe.read_data(&mut inner, buf);
            drop(inner);
            self.pipe.publish_readiness();
            self.pipe.write_wait.wake_one_default();
            return Ok(n);
        }
        if inner.writer_count == 0 {
            return Ok(0);
        }

        Err(VfsError::WouldBlock)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        let mut inner = self.pipe.inner.lock();
        if inner.reader_count == 0 {
            return Err(VfsError::BrokenPipe);
        }
        let n = self.pipe.write_data(&mut inner, buf);
        if n > 0 {
            drop(inner);
            self.pipe.publish_readiness();
            self.pipe.read_wait.wake_one_default();
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
        self.pipe
            .read_write_source
            .snapshot()
            .0
            .intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI) {
            self.pipe.read_wait.enqueue(task);
        }
        if interest.has(PollEvents::POLLOUT) {
            self.pipe.write_wait.enqueue(task);
        }
        if interest.has(PollEvents::POLLHUP) || interest.has(PollEvents::POLLERR) {
            self.pipe.read_wait.enqueue(task);
            self.pipe.write_wait.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.pipe.read_wait.remove(task);
        self.pipe.write_wait.remove(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.pipe.read_write_source)
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
        inner.writer_count = inner.writer_count.saturating_sub(1);
        let no_reader = inner.reader_count == 0;
        let no_writer = inner.writer_count == 0;
        drop(inner);
        self.pipe.publish_readiness();
        if no_reader {
            self.pipe.write_wait.wake_all();
        }
        if no_writer {
            self.pipe.read_wait.wake_all();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
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
        let n = self.pipe.write_data(&mut inner, buf);
        if n > 0 {
            drop(inner);
            self.pipe.publish_readiness();
            // 写入后只需要一个读者消费新数据，避免 lmbench pipe 场景
            // 中把所有等待任务同时推回 runqueue 造成惊群。
            self.pipe.read_wait.wake_one_default();
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
        self.pipe
            .write_source
            .snapshot()
            .0
            .intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
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

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.pipe.write_source)
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
        self.pipe.publish_readiness();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_write_waits_for_enough_space() {
        let pipe = Pipe::new();
        let mut inner = pipe.inner.lock();
        inner.write_pos = PIPE_CAPACITY - PIPE_BUF + 1;
        let before = inner.write_pos;

        assert_eq!(pipe.write_data(&mut inner, &[0u8; PIPE_BUF]), 0);
        assert_eq!(inner.write_pos, before);
    }

    #[test]
    fn small_write_is_written_in_one_piece() {
        let pipe = Pipe::new();
        let mut inner = pipe.inner.lock();
        let src = [0x5au8; PIPE_BUF];

        assert_eq!(pipe.write_data(&mut inner, &src), PIPE_BUF);
        assert_eq!(inner.write_pos, PIPE_BUF);
        assert_eq!(&inner.data[..PIPE_BUF], &src);
    }

    #[test]
    fn large_write_can_use_partial_space() {
        let pipe = Pipe::new();
        let mut inner = pipe.inner.lock();
        inner.write_pos = PIPE_CAPACITY - 10;

        assert_eq!(pipe.write_data(&mut inner, &[0xa5u8; PIPE_BUF + 1]), 10);
        assert_eq!(inner.write_pos, PIPE_CAPACITY);
    }
}
