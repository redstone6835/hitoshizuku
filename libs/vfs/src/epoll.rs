use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, current_task};

use crate::vfs::cred::Credentials;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::vfs::mount::{Mount, MountFlags};
use crate::vfs::stat::{DevId, FileMode, FileType, FsId, Timespec};
use crate::vfs::superblock::{InodeCache, Superblock, SuperblockOps};
use crate::vfs::sync::Spinlock;

pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLL_CTL_MOD: i32 = 3;
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub const EPOLLET: u32 = 1 << 31;
pub const EPOLLONESHOT: u32 = 1 << 30;

#[derive(Debug, Clone, Copy)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

struct EpollFs {
    mount: Arc<Mount>,
    inode: Arc<Inode>,
    dentry: Arc<Dentry>,
}

static EPOLL_FS: Spinlock<Option<EpollFs>> = Spinlock::new(None);

fn get_or_init_epoll_fs() -> (Arc<Mount>, Arc<Inode>, Arc<Dentry>) {
    let mut guard = EPOLL_FS.lock();
    if guard.is_none() {
        let sb = Superblock::new(|weak| {
            let root_inode = Inode::new(
                InodeId {
                    fs_id: FsId::new(0x65706f6c6c667300),
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
                Arc::new(EpollInodeOps),
                weak.clone(),
            );
            let root_dentry = Dentry::new_positive("", None, root_inode.clone());
            Superblock {
                fs_type: "epollfs",
                fs_id: FsId::new(0x65706f6c6c667300),
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: InodeCache::new(),
                ops: Box::new(EpollSuperblockOps),
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

        *guard = Some(EpollFs {
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

struct EpollInodeOps;

impl InodeOps for EpollInodeOps {
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

struct EpollSuperblockOps;

impl SuperblockOps for EpollSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn statfs(&self, _sb: &Arc<Superblock>) -> VfsResult<crate::vfs::stat::FsStat> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct EpollWatch {
    fd: Fd,
    file: Arc<File>,
    interest: PollEvents,
    data: u64,
    edge_triggered: bool,
    oneshot: bool,
    disabled: bool,
    last_ready: PollEvents,
}

struct EpollState {
    watches: Vec<EpollWatch>,
}

pub struct EpollFileOps {
    state: Spinlock<EpollState>,
}

impl EpollFileOps {
    fn new() -> Self {
        Self {
            state: Spinlock::new(EpollState {
                watches: Vec::new(),
            }),
        }
    }

    fn ctl_add(&self, fd: Fd, file: Arc<File>, event: EpollEvent) -> Result<(), Errno> {
        if (event.events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let mut state = self.state.lock();
        if state.watches.iter().any(|watch| watch.fd == fd) {
            return Err(Errno::EEXIST);
        }
        state.watches.push(EpollWatch {
            fd,
            file,
            interest: PollEvents(event.events as u16),
            data: event.data,
            edge_triggered: (event.events & EPOLLET) != 0,
            oneshot: (event.events & EPOLLONESHOT) != 0,
            disabled: false,
            last_ready: PollEvents::default(),
        });
        Ok(())
    }

    fn ctl_mod(&self, fd: Fd, event: EpollEvent) -> Result<(), Errno> {
        if (event.events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let mut state = self.state.lock();
        let Some(watch) = state.watches.iter_mut().find(|watch| watch.fd == fd) else {
            return Err(Errno::ENOENT);
        };
        watch.interest = PollEvents(event.events as u16);
        watch.data = event.data;
        watch.edge_triggered = (event.events & EPOLLET) != 0;
        watch.oneshot = (event.events & EPOLLONESHOT) != 0;
        watch.disabled = false;
        watch.last_ready = PollEvents::default();
        Ok(())
    }

    fn ctl_del(&self, fd: Fd) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let Some(index) = state.watches.iter().position(|watch| watch.fd == fd) else {
            return Err(Errno::ENOENT);
        };
        state.watches.remove(index);
        Ok(())
    }

    fn any_ready(&self) -> bool {
        let state = self.state.lock();
        state.watches.iter().any(|watch| peek_watch_ready(watch))
    }

    fn collect_ready(&self, maxevents: usize) -> Vec<EpollEvent> {
        let mut out = Vec::new();
        let mut state = self.state.lock();
        for watch in state.watches.iter_mut() {
            if watch.disabled {
                continue;
            }
            let current = watch.file.poll(watch.interest);
            let ready = if watch.edge_triggered {
                PollEvents(current.raw() & !watch.last_ready.raw())
            } else {
                current
            };
            watch.last_ready = current;
            if ready.is_empty() {
                continue;
            }
            out.push(EpollEvent {
                events: ready.raw() as u32,
                data: watch.data,
            });
            if watch.oneshot {
                watch.disabled = true;
            }
            if out.len() >= maxevents {
                break;
            }
        }
        out
    }

    fn wait_sources(&self) -> Vec<(Arc<File>, PollEvents)> {
        let state = self.state.lock();
        state
            .watches
            .iter()
            .filter(|watch| !watch.disabled)
            .map(|watch| (Arc::clone(&watch.file), watch.interest))
            .collect()
    }

    fn contains_file_recursive(&self, needle: &Arc<File>, visited: &mut Vec<usize>) -> bool {
        let state = self.state.lock();
        for watch in &state.watches {
            if Arc::ptr_eq(&watch.file, needle) {
                return true;
            }
            let ptr = Arc::as_ptr(&watch.file) as usize;
            if visited.contains(&ptr) {
                continue;
            }
            if let Some(child) = watch.file.downcast_ops::<EpollFileOps>() {
                visited.push(ptr);
                if child.contains_file_recursive(needle, visited) {
                    return true;
                }
            }
        }
        false
    }

    fn wait(&self, maxevents: usize, timeout_ms: i64) -> Result<Vec<EpollEvent>, Errno> {
        let deadline = timeout_deadline(timeout_ms);
        loop {
            let ready = self.collect_ready(maxevents);
            if !ready.is_empty() {
                return Ok(ready);
            }
            if timeout_expired(deadline) || timeout_ms == 0 {
                return Ok(Vec::new());
            }
            let sources = self.wait_sources();
            wait_on_sources(&sources, deadline)?;
        }
    }
}

impl FileOps for EpollFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
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
        if self.any_ready() {
            PollEvents::POLLIN
                .intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
        } else {
            PollEvents::default()
        }
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if !(interest.has(PollEvents::POLLIN)
            || interest.has(PollEvents::POLLPRI)
            || interest.has(PollEvents::POLLHUP)
            || interest.has(PollEvents::POLLERR))
        {
            return false;
        }
        let sources = self.wait_sources();
        for (file, source_interest) in &sources {
            let _ = file.poll_add_waiter(task, *source_interest);
        }
        !sources.is_empty()
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        let sources = self.wait_sources();
        for (file, _) in &sources {
            file.poll_remove_waiter(task);
        }
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn peek_watch_ready(watch: &EpollWatch) -> bool {
    if watch.disabled {
        return false;
    }
    let current = watch.file.poll(watch.interest);
    if watch.edge_triggered {
        (current.raw() & !watch.last_ready.raw()) != 0
    } else {
        !current.is_empty()
    }
}

fn timeout_deadline(timeout_ms: i64) -> Option<u64> {
    if timeout_ms >= 0 {
        Some(sched::now_ns_public() + (timeout_ms as u64) * 1_000_000)
    } else {
        None
    }
}

fn timeout_expired(deadline: Option<u64>) -> bool {
    deadline.is_some_and(|dl| sched::now_ns_public() >= dl)
}

fn wait_on_sources(
    sources: &[(Arc<File>, PollEvents)],
    deadline: Option<u64>,
) -> Result<(), Errno> {
    let task = current_task();
    if has_unblocked_signal(&task) {
        return Err(Errno::EINTR);
    }
    if timeout_expired(deadline) {
        return Ok(());
    }

    let _ = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping);
    let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);

    let mut registered_waiter = false;
    for (file, interest) in sources {
        registered_waiter |= file.poll_add_waiter(&task, *interest);
    }
    let deadline_armed =
        deadline.is_some_and(|deadline| sched::register_sleep_deadline(&task, deadline));

    if sources
        .iter()
        .any(|(file, interest)| !file.poll(*interest).is_empty())
    {
        for (file, _) in sources {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Runnable);
        return Ok(());
    }
    if timeout_expired(deadline) {
        for (file, _) in sources {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Runnable);
        return Ok(());
    }

    if !registered_waiter && !deadline_armed {
        let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Runnable);
        return sched::operation::sched_yield();
    }

    sched::schedule_once(0);
    for (file, _) in sources {
        file.poll_remove_waiter(&task);
    }
    if deadline_armed {
        sched::cancel_sleep_deadline(&task);
    }
    if has_unblocked_signal(&task) {
        return Err(Errno::EINTR);
    }
    Ok(())
}

fn has_unblocked_signal(task: &Arc<sched::Task>) -> bool {
    let blocked = task.signal.blocked_snapshot().raw();
    let pending =
        task.signal.pending_snapshot().raw() | task.shared_signal().pending_snapshot().raw();
    (pending & !blocked) != 0
}

fn new_epoll_file(cred: Arc<Credentials>) -> Arc<File> {
    let (mount, inode, dentry) = get_or_init_epoll_fs();
    let file = File::new(
        inode,
        OpenOptions {
            access: crate::vfs::file::AccessMode::ReadWrite,
            ..Default::default()
        },
        cred,
        Box::new(EpollFileOps::new()),
        dentry,
        Arc::clone(&mount),
    );
    mount.inc_open();
    Arc::new(file)
}

fn epoll_ops_from_fd<'a>(file: &'a Arc<File>) -> Result<&'a EpollFileOps, Errno> {
    file.downcast_ops::<EpollFileOps>().ok_or(Errno::EINVAL)
}

pub fn create(fdt: &FdTable, cred: Arc<Credentials>, cloexec: bool) -> Result<Fd, Errno> {
    let flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    fdt.alloc_fd(new_epoll_file(cred), flags)
        .map_err(|e| e.to_errno())
}

pub fn ctl(
    fdt: &FdTable,
    epfd: Fd,
    op: i32,
    fd: Fd,
    event: Option<EpollEvent>,
) -> Result<(), Errno> {
    let epoll_file = fdt.get_file(epfd).ok_or(Errno::EBADF)?;
    let ops = epoll_ops_from_fd(&epoll_file)?;
    let target = fdt.get_file(fd).ok_or(Errno::EBADF)?;
    if Arc::ptr_eq(&epoll_file, &target) {
        return Err(Errno::EINVAL);
    }
    if let Some(target_epoll) = target.downcast_ops::<EpollFileOps>() {
        let mut visited = vec![Arc::as_ptr(&target) as usize];
        if target_epoll.contains_file_recursive(&epoll_file, &mut visited) {
            return Err(Errno::ELOOP);
        }
    }
    match op {
        EPOLL_CTL_ADD => ops.ctl_add(fd, target, event.ok_or(Errno::EINVAL)?),
        EPOLL_CTL_MOD => ops.ctl_mod(fd, event.ok_or(Errno::EINVAL)?),
        EPOLL_CTL_DEL => ops.ctl_del(fd),
        _ => Err(Errno::EINVAL),
    }
}

pub fn wait(
    fdt: &FdTable,
    epfd: Fd,
    maxevents: usize,
    timeout_ms: i64,
) -> Result<Vec<EpollEvent>, Errno> {
    let epoll_file = fdt.get_file(epfd).ok_or(Errno::EBADF)?;
    let ops = epoll_ops_from_fd(&epoll_file)?;
    ops.wait(maxevents, timeout_ms)
}
