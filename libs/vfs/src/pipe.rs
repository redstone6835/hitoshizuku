use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::vfs::cred::Credentials;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::{AccessMode, DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::vfs::mount::{Mount, MountFlags};
use crate::vfs::stat::{DevId, FileMode, FileType, FsId, Timespec};
use crate::vfs::superblock::{InodeCache, Superblock, SuperblockOps};
use crate::vfs::sync::Spinlock;

pub const PIPE_CAPACITY: usize = 65536;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NamedFifoKey {
    fs_id: u64,
    ino: u64,
}

struct PipeInner {
    data: Vec<u8>,
    read_pos: usize,
    write_pos: usize,
    reader_count: u32,
    writer_count: u32,
    opening_readers: u32,
    opening_writers: u32,
}

pub struct Pipe {
    inner: Spinlock<PipeInner>,
    read_wait: WaitQueue,
    write_wait: WaitQueue,
}

impl Pipe {
    fn new() -> Self {
        Self::new_with_counts(1, 1)
    }

    fn new_with_counts(reader_count: u32, writer_count: u32) -> Self {
        Self {
            inner: Spinlock::new(PipeInner {
                data: vec![0u8; PIPE_CAPACITY],
                read_pos: 0,
                write_pos: 0,
                reader_count,
                writer_count,
                opening_readers: 0,
                opening_writers: 0,
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

fn pipe_fcntl(cmd: usize, arg: usize) -> Result<usize, Errno> {
    const F_SETPIPE_SZ: usize = 1031;
    const F_GETPIPE_SZ: usize = 1032;

    match cmd {
        F_GETPIPE_SZ => Ok(PIPE_CAPACITY),
        F_SETPIPE_SZ if arg <= PIPE_CAPACITY => Ok(PIPE_CAPACITY),
        F_SETPIPE_SZ => Err(Errno::EPERM),
        _ => Err(Errno::EINVAL),
    }
}

fn pipe_ioctl(cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
    match pipe_fcntl(cmd.raw(), arg) {
        Err(Errno::EINVAL) => Err(Errno::ENOTTY),
        other => other,
    }
}

impl FileOps for PipeReadEnd {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let mut inner = self.pipe.inner.lock();
        let avail = self.pipe.available(&inner);
        if avail > 0 {
            let n = self.pipe.read_data(&mut inner, buf);
            drop(inner);
            // 正常读出数据只释放了部分缓冲空间，唤醒一个写者即可；
            // 端点关闭等状态变化仍在 release() 中广播给全部等待者。
            self.pipe.write_wait.wake_one_default();
            return Ok(n);
        }
        if inner.writer_count == 0 && inner.opening_writers == 0 {
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
        if inner.writer_count == 0 && inner.opening_writers == 0 {
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

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        pipe_ioctl(cmd, arg)
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
            return if inner.opening_readers == 0 {
                Err(VfsError::BrokenPipe)
            } else {
                Err(VfsError::WouldBlock)
            };
        }
        let free = self.pipe.free_space(&inner);
        if free > 0 {
            let n = self.pipe.write_data(&mut inner, buf);
            drop(inner);
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
        let inner = self.pipe.inner.lock();
        let mut ready = PollEvents::default();
        if inner.reader_count != 0 && self.pipe.free_space(&inner) > 0 {
            ready = ready.with(PollEvents::POLLOUT);
        }
        if inner.reader_count == 0 && inner.opening_readers == 0 {
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

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        pipe_ioctl(cmd, arg)
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

pub struct FifoFileOps {
    key: NamedFifoKey,
    pipe: Arc<Pipe>,
    readable: bool,
    writable: bool,
}

impl FifoFileOps {
    fn new(key: NamedFifoKey, pipe: Arc<Pipe>, readable: bool, writable: bool) -> Self {
        Self {
            key,
            pipe,
            readable,
            writable,
        }
    }

    pub fn is_writable(&self) -> bool {
        self.writable
    }
}

impl FileOps for FifoFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if !self.readable {
            return Err(VfsError::BadFileDescriptor);
        }
        let mut inner = self.pipe.inner.lock();
        let avail = self.pipe.available(&inner);
        if avail > 0 {
            let n = self.pipe.read_data(&mut inner, buf);
            drop(inner);
            self.pipe.write_wait.wake_one_default();
            return Ok(n);
        }
        if inner.writer_count == 0 && inner.opening_writers == 0 {
            return Ok(0);
        }
        Err(VfsError::WouldBlock)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if !self.writable {
            return Err(VfsError::BadFileDescriptor);
        }
        let mut inner = self.pipe.inner.lock();
        if inner.reader_count == 0 {
            return if inner.opening_readers == 0 {
                Err(VfsError::BrokenPipe)
            } else {
                Err(VfsError::WouldBlock)
            };
        }
        let free = self.pipe.free_space(&inner);
        if free > 0 {
            let n = self.pipe.write_data(&mut inner, buf);
            drop(inner);
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
        let inner = self.pipe.inner.lock();
        let mut ready = PollEvents::default();
        if self.readable && self.pipe.available(&inner) > 0 {
            ready = ready.with(PollEvents::POLLIN);
        }
        if self.writable && inner.reader_count != 0 && self.pipe.free_space(&inner) > 0 {
            ready = ready.with(PollEvents::POLLOUT);
        }
        if inner.writer_count == 0 && inner.opening_writers == 0 {
            ready = ready.with(PollEvents::POLLHUP);
        }
        if inner.reader_count == 0 && inner.opening_readers == 0 {
            ready = ready.with(PollEvents::POLLERR);
        }
        ready.intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if self.readable && (interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI))
        {
            self.pipe.read_wait.enqueue(task);
        }
        if self.writable && interest.has(PollEvents::POLLOUT) {
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

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        pipe_ioctl(cmd, arg)
    }

    fn release(&self) {
        let mut inner = self.pipe.inner.lock();
        if self.readable {
            inner.reader_count = inner.reader_count.saturating_sub(1);
        }
        if self.writable {
            inner.writer_count = inner.writer_count.saturating_sub(1);
        }
        let last_reader = inner.reader_count == 0;
        let last_writer = inner.writer_count == 0;
        drop(inner);
        if last_reader {
            self.pipe.write_wait.wake_all();
        }
        if last_writer {
            self.pipe.read_wait.wake_all();
        }
        cleanup_named_fifo(self.key);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

static NAMED_FIFOS: Spinlock<BTreeMap<NamedFifoKey, Weak<Pipe>>> = Spinlock::new(BTreeMap::new());

fn fifo_key(inode: &Inode) -> NamedFifoKey {
    NamedFifoKey {
        fs_id: inode.fs_id().raw(),
        ino: inode.ino(),
    }
}

fn get_named_fifo(key: NamedFifoKey) -> Arc<Pipe> {
    let mut table = NAMED_FIFOS.lock();
    if let Some(pipe) = table.get(&key).and_then(Weak::upgrade) {
        return pipe;
    }
    table.remove(&key);
    let pipe = Arc::new(Pipe::new_with_counts(0, 0));
    table.insert(key, Arc::downgrade(&pipe));
    pipe
}

fn cleanup_named_fifo(key: NamedFifoKey) {
    let mut table = NAMED_FIFOS.lock();
    if table.get(&key).is_some_and(|weak| weak.upgrade().is_none()) {
        table.remove(&key);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FifoOpenRegistration {
    reader: bool,
    writer: bool,
}

impl FifoOpenRegistration {
    fn is_registered(self) -> bool {
        self.reader || self.writer
    }
}

fn finish_fifo_open_wait(pipe: &Arc<Pipe>, task: &Arc<Task>) {
    pipe.read_wait.finish_wait(task);
    pipe.write_wait.finish_wait(task);
}

fn rollback_fifo_open_registration_locked(
    inner: &mut PipeInner,
    registration: &mut FifoOpenRegistration,
) {
    if registration.reader {
        inner.opening_readers = inner.opening_readers.saturating_sub(1);
        registration.reader = false;
    }
    if registration.writer {
        inner.opening_writers = inner.opening_writers.saturating_sub(1);
        registration.writer = false;
    }
}

fn rollback_fifo_open_registration(pipe: &Arc<Pipe>, registration: &mut FifoOpenRegistration) {
    if !registration.is_registered() {
        return;
    }
    let mut inner = pipe.inner.lock();
    rollback_fifo_open_registration_locked(&mut inner, registration);
}

pub fn open_fifo(inode: &Inode, opts: &OpenOptions) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
    let readable = opts.readable();
    let writable = opts.writable();
    if !readable && !writable {
        return Err(VfsError::BadFileDescriptor);
    }
    let key = fifo_key(inode);
    let pipe = get_named_fifo(key);
    let task = sched::current_task();
    let mut registration = FifoOpenRegistration::default();

    loop {
        let opened = {
            let mut inner = pipe.inner.lock();

            if readable && writable {
                rollback_fifo_open_registration_locked(&mut inner, &mut registration);
                inner.reader_count = inner.reader_count.saturating_add(1);
                inner.writer_count = inner.writer_count.saturating_add(1);
                true
            } else if writable && inner.reader_count == 0 && inner.opening_readers == 0 {
                if opts.nonblock {
                    cleanup_named_fifo(key);
                    return Err(VfsError::NoSuchDeviceOrAddress);
                }
                if !registration.writer {
                    inner.opening_writers = inner.opening_writers.saturating_add(1);
                    registration.writer = true;
                    pipe.read_wait.wake_all();
                }
                pipe.write_wait
                    .prepare_to_wait(&task, sched::TaskState::Sleeping);
                false
            } else if readable && inner.writer_count == 0 && inner.opening_writers == 0 {
                if opts.nonblock {
                    inner.reader_count = inner.reader_count.saturating_add(1);
                    true
                } else {
                    if !registration.reader {
                        inner.opening_readers = inner.opening_readers.saturating_add(1);
                        registration.reader = true;
                        pipe.write_wait.wake_all();
                    }
                    pipe.read_wait
                        .prepare_to_wait(&task, sched::TaskState::Sleeping);
                    false
                }
            } else {
                rollback_fifo_open_registration_locked(&mut inner, &mut registration);
                if readable {
                    inner.reader_count = inner.reader_count.saturating_add(1);
                }
                if writable {
                    inner.writer_count = inner.writer_count.saturating_add(1);
                }
                true
            }
        };

        if opened {
            if readable {
                pipe.write_wait.wake_all();
            }
            if writable {
                pipe.read_wait.wake_all();
            }
            return Ok(Box::new(FifoFileOps::new(key, pipe, readable, writable)));
        }

        let retry_without_sleep = {
            let inner = pipe.inner.lock();
            (writable && !readable && inner.reader_count != 0)
                || (readable && !writable && inner.writer_count != 0)
        };
        if retry_without_sleep {
            finish_fifo_open_wait(&pipe, &task);
            continue;
        }
        if sched::operation::has_interrupting_signal(&task) {
            finish_fifo_open_wait(&pipe, &task);
            rollback_fifo_open_registration(&pipe, &mut registration);
            pipe.read_wait.wake_all();
            pipe.write_wait.wake_all();
            cleanup_named_fifo(key);
            return Err(VfsError::Interrupted);
        }

        sched::schedule_once(sched::now_ns_public());
        finish_fifo_open_wait(&pipe, &task);
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

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<crate::vfs::stat::FsStat> {
        Ok(crate::vfs::stat::FsStat {
            fs_type: 0x5049_5045,
            block_size: sb.block_size as u64,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: 0,
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: sb.name_max,
        })
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
        access: AccessMode::WriteOnly,
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
