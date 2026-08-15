//! inotify 实例：监视表、事件队列与 fd 语义。
//!
//! 实例语义（Linux）：
//! - `add_watch` 返回单调递增的 wd；同一 (实例, inode) 复用 wd，`IN_MASK_ADD`
//!   时按位或合并掩码，否则整体替换；
//! - 队列上限 16384 条，溢出丢弃并置位，读取时在队首合成 `IN_Q_OVERFLOW`；
//! - `IN_ONESHOT` 监视投递一次后自动移除并补发 `IN_IGNORED`；
//! - 被监视 inode 删除时投递 `IN_DELETE_SELF` + `IN_IGNORED`（
//!   `IN_EXCL_UNLINK` 例外）；
//! - read 缓冲小于首事件大小时返回 `EINVAL`；空队列非阻塞 `EAGAIN`、
//!   阻塞时挂到 PollSource 等待（syscall 层的 poll 等待机制）；
//! - 实例关闭时移除全部监视。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicI32, Ordering};

use errno::Errno;

use crate::fsnotify::{self, NotifyEvent, Watch, WatchTarget};
use crate::poll_source::PollSource;
use crate::vfs::anon;
use crate::vfs::cred::Credentials;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{AccessMode, DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::Inode;
use crate::vfs::stat::FileType;
use crate::vfs::sync::Spinlock;
use sched::{Task, WaitQueue};

/// `struct inotify_event` 头大小（不含名字）。
const INOTIFY_EVENT_HEADER: usize = 16;

struct InotifyEvent {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

pub struct InotifyInstance {
    /// 全局实例序号（fdinfo/诊断用）。
    id: u64,
    queue: Spinlock<VecDeque<InotifyEvent>>,
    overflow: core::sync::atomic::AtomicBool,
    /// wd → 监视（实例持有 Arc，保证监视在实例生命周期内有效）。
    watches: Spinlock<BTreeMap<i32, Arc<Watch>>>,
    next_wd: AtomicI32,
    waiters: WaitQueue,
    poll_source: PollSource,
    self_weak: Spinlock<Weak<InotifyInstance>>,
}

pub struct InotifyFileOps {
    instance: Arc<InotifyInstance>,
}

impl InotifyInstance {
    #[cfg(test)]
    pub(crate) fn new_for_test() -> Arc<Self> {
        Self::new()
    }

    fn new() -> Arc<Self> {
        Arc::new_cyclic(|self_weak| InotifyInstance {
            id: next_instance_id(),
            queue: Spinlock::new(VecDeque::new()),
            overflow: core::sync::atomic::AtomicBool::new(false),
            watches: Spinlock::new(BTreeMap::new()),
            next_wd: AtomicI32::new(1),
            waiters: WaitQueue::new(),
            poll_source: PollSource::new(PollEvents::default()),
            self_weak: Spinlock::new(self_weak.clone()),
        })
    }

    fn wake(&self) {
        self.poll_source.publish(PollEvents::POLLIN);
        self.waiters.wake_all();
    }

    /// 添加监视；返回 wd。
    pub fn add_watch(&self, inode: &Arc<Inode>, mask: u32, flags: u32) -> Result<i32, Errno> {
        if flags & fsnotify::IN_ONLYDIR != 0 && inode.kind() != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        let mut watches = self.watches.lock();
        // 同一 (实例, inode)：复用 wd。
        for watch in watches.values() {
            if watch
                .inode
                .upgrade()
                .map(|w| Arc::ptr_eq(&w, inode))
                .unwrap_or(false)
            {
                if flags & fsnotify::IN_MASK_ADD != 0 {
                    watch.mask.fetch_or(mask, Ordering::AcqRel);
                } else {
                    watch.mask.store(mask, Ordering::Release);
                }
                return Ok(watch.wd);
            }
        }
        let wd = self.next_wd.fetch_add(1, Ordering::Relaxed);
        if wd <= 0 {
            return Err(Errno::ENOSPC);
        }
        let watch = Arc::new(Watch {
            wd,
            mask: core::sync::atomic::AtomicU32::new(mask),
            flags,
            unlinked: core::sync::atomic::AtomicBool::new(false),
            inode: Arc::downgrade(inode),
            target: self.self_weak.lock().clone(),
        });
        watches.insert(wd, Arc::clone(&watch));
        drop(watches);
        fsnotify::register(inode, Arc::downgrade(&watch));
        Ok(wd)
    }

    /// 移除监视；未知 wd 返回 `EINVAL`。
    pub fn rm_watch(&self, wd: i32) -> Result<(), Errno> {
        let watch = self.watches.lock().remove(&wd);
        let Some(watch) = watch else {
            return Err(Errno::EINVAL);
        };
        if let Some(inode) = watch.inode.upgrade() {
            fsnotify::unregister(&inode, &watch);
        }
        // 排队 IN_IGNORED（Linux 语义）。
        let mut queue = self.queue.lock();
        if queue.len() < fsnotify::INOTIFY_QUEUE_LIMIT {
            queue.push_back(InotifyEvent {
                wd,
                mask: fsnotify::IN_IGNORED,
                cookie: 0,
                name: Vec::new(),
            });
        }
        drop(queue);
        self.wake();
        Ok(())
    }

    /// 移除实例的全部监视（实例关闭）。
    fn remove_all_watches(&self) {
        let watches: Vec<Arc<Watch>> = self.watches.lock().values().cloned().collect();
        self.watches.lock().clear();
        for watch in watches {
            if let Some(inode) = watch.inode.upgrade() {
                fsnotify::unregister(&inode, &watch);
            }
        }
    }

    /// fdinfo 输出。
    fn render_fdinfo(&self, out: &mut alloc::string::String) {
        use core::fmt::Write;
        let watches = self.watches.lock();
        for watch in watches.values() {
            let ino = watch
                .inode
                .upgrade()
                .map(|i| i.ino())
                .unwrap_or(0);
            let _ = writeln!(
                out,
                "inotify wd:{} ino:{:x} sdev:00000000 mask:{:08x} ignored_mask:00000000",
                watch.wd, ino, watch.mask.load(Ordering::Acquire)
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn read_events_for_test(&self, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_events(buf, true)
    }

    fn read_events(&self, buf: &mut [u8], nonblock: bool) -> VfsResult<usize> {
        if buf.len() < INOTIFY_EVENT_HEADER {
            return Err(VfsError::InvalidArgument);
        }
        loop {
            let mut queue = self.queue.lock();
            if queue.is_empty() {
                if nonblock {
                    return Err(VfsError::WouldBlock);
                }
                // 阻塞：由 syscall 层经 poll_add_waiter 挂到 waiters。
                return Err(VfsError::WouldBlock);
            }
            // 溢出事件优先合成在队首。
            if self.overflow.swap(false, Ordering::AcqRel) {
                queue.push_front(InotifyEvent {
                    wd: -1,
                    mask: fsnotify::IN_Q_OVERFLOW,
                    cookie: 0,
                    name: Vec::new(),
                });
            }
            let event = queue.front().unwrap();
            let total = INOTIFY_EVENT_HEADER + event.name.len();
            if buf.len() < total {
                return Err(VfsError::InvalidArgument);
            }
            let event = queue.pop_front().unwrap();
            let mut out = [0u8; INOTIFY_EVENT_HEADER];
            out[0..4].copy_from_slice(&event.wd.to_le_bytes());
            out[4..8].copy_from_slice(&event.mask.to_le_bytes());
            out[8..12].copy_from_slice(&event.cookie.to_le_bytes());
            out[12..16].copy_from_slice(&(event.name.len() as u32).to_le_bytes());
            buf[..INOTIFY_EVENT_HEADER].copy_from_slice(&out);
            buf[INOTIFY_EVENT_HEADER..total].copy_from_slice(&event.name);
            drop(queue);
            self.poll_source.publish(if self.queue.lock().is_empty() {
                PollEvents::default()
            } else {
                PollEvents::POLLIN
            });
            return Ok(total);
        }
    }
}

impl WatchTarget for InotifyInstance {
    fn deliver(&self, event: &NotifyEvent) {
        let mut queue = self.queue.lock();
        if queue.len() >= fsnotify::INOTIFY_QUEUE_LIMIT {
            self.overflow.store(true, Ordering::Release);
            return;
        }
        queue.push_back(InotifyEvent {
            wd: event.wd,
            mask: event.mask,
            cookie: event.cookie,
            name: event.name.clone(),
        });
        drop(queue);
        self.wake();
    }

    fn on_watch_removed(&self, wd: i32, ignored: bool) {
        self.watches.lock().remove(&wd);
        if ignored {
            let mut queue = self.queue.lock();
            if queue.len() < fsnotify::INOTIFY_QUEUE_LIMIT {
                queue.push_back(InotifyEvent {
                    wd,
                    mask: fsnotify::IN_IGNORED,
                    cookie: 0,
                    name: Vec::new(),
                });
            }
            drop(queue);
            self.wake();
        }
    }
}

impl FileOps for InotifyFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.instance.read_events(buf, false)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidArgument)
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
        self.instance.poll_source.snapshot().0.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) {
            self.instance.waiters.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.instance.waiters.remove(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.instance.poll_source)
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.instance.remove_all_watches();
        self.instance.waiters.wake_all();
    }

    fn show_fdinfo(&self, out: &mut alloc::string::String) {
        self.instance.render_fdinfo(out);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 创建 inotify 实例 fd。
pub fn create(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    nonblock: bool,
    cloexec: bool,
) -> Result<Fd, Errno> {
    let file_flags = OpenOptions {
        access: AccessMode::ReadOnly,
        nonblock,
        ..Default::default()
    };
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    anon::create_fd(
        fdt,
        cred,
        file_flags,
        fd_flags,
        Box::new(InotifyFileOps {
            instance: InotifyInstance::new(),
        }),
    )
    .map_err(|err| err.to_errno())
}

/// 按 fd 取实例（add_watch/rm_watch 用）。
pub fn instance_from_file(file: &crate::vfs::file::File) -> Option<Arc<InotifyInstance>> {
    file.downcast_ops::<InotifyFileOps>()
        .map(|ops| Arc::clone(&ops.instance))
}

fn next_instance_id() -> u64 {
    use core::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
