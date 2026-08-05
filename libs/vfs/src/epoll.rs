use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use errno::Errno;
use sched::{Task, TaskState, WaitQueue, current_task};

use crate::poll_source::{PollSource, PollSubscriber};
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
pub const EPOLLRDHUP: u32 = PollEvents::POLLRDHUP.0 as u32;
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
    file: Arc<File>,
    source_id: u64,
    subscription_id: AtomicU64,
    interest: AtomicU16,
    data: AtomicU64,
    edge_triggered: AtomicBool,
    oneshot: AtomicBool,
    disabled: AtomicBool,
    queued: AtomicBool,
    queued_ready: AtomicU16,
    last_ready: AtomicU16,
    generation: AtomicU64,
    queued_generation: AtomicU64,
    source_generation: AtomicU64,
    self_weak: Weak<EpollWatch>,
    epoll: Weak<EpollCore>,
}

struct EpollState {
    watches: Vec<Arc<EpollWatch>>,
    ready: VecDeque<Arc<EpollWatch>>,
}

struct EpollCore {
    state: Spinlock<EpollState>,
    waiters: WaitQueue,
    poll_source: PollSource,
}

pub struct EpollFileOps {
    core: Arc<EpollCore>,
}

impl EpollFileOps {
    fn new() -> Self {
        Self {
            core: Arc::new(EpollCore {
                state: Spinlock::new(EpollState {
                    watches: Vec::new(),
                    ready: VecDeque::new(),
                }),
                waiters: WaitQueue::new_with_reason(sched::WaitReason::Poll),
                poll_source: PollSource::new(PollEvents::default()),
            }),
        }
    }

    fn ctl_add(&self, _fd: Fd, file: Arc<File>, event: EpollEvent) -> Result<(), Errno> {
        if (event.events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let source = file.poll_source().ok_or(Errno::EOPNOTSUPP)?;
        let core = Arc::downgrade(&self.core);
        let source_id = source.id();
        let watch_file = Arc::clone(&file);
        let watch = Arc::new_cyclic(|self_weak| EpollWatch {
            file: watch_file,
            source_id,
            subscription_id: AtomicU64::new(0),
            interest: AtomicU16::new(event.events as u16),
            data: AtomicU64::new(event.data),
            edge_triggered: AtomicBool::new((event.events & EPOLLET) != 0),
            oneshot: AtomicBool::new((event.events & EPOLLONESHOT) != 0),
            disabled: AtomicBool::new(true),
            queued: AtomicBool::new(false),
            queued_ready: AtomicU16::new(0),
            last_ready: AtomicU16::new(0),
            generation: AtomicU64::new(1),
            queued_generation: AtomicU64::new(0),
            source_generation: AtomicU64::new(0),
            self_weak: self_weak.clone(),
            epoll: core,
        });
        {
            let mut state = self.core.state.lock();
            if state
                .watches
                .iter()
                .any(|candidate| Arc::ptr_eq(&candidate.file, &file))
            {
                return Err(Errno::EEXIST);
            }
            state.watches.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
            let required = state.watches.len().saturating_add(1);
            let additional = required.saturating_sub(state.ready.len());
            state
                .ready
                .try_reserve(additional)
                .map_err(|_| Errno::ENOMEM)?;
            state.watches.push(Arc::clone(&watch));
        }
        let subscriber: Arc<dyn PollSubscriber> = watch.clone();
        let subscription = source.subscribe(Arc::downgrade(&subscriber));
        watch.subscription_id.store(subscription, Ordering::Release);
        watch.disabled.store(false, Ordering::Release);
        let (readiness, generation) = source.snapshot();
        watch.readiness_changed(source_id, readiness, generation);
        Ok(())
    }

    fn ctl_mod(&self, file: &Arc<File>, event: EpollEvent) -> Result<(), Errno> {
        if (event.events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let watch = {
            let state = self.core.state.lock();
            state
                .watches
                .iter()
                .find(|watch| Arc::ptr_eq(&watch.file, file))
                .cloned()
                .ok_or(Errno::ENOENT)?
        };
        watch.disabled.store(true, Ordering::Release);
        watch.generation.fetch_add(1, Ordering::AcqRel);
        let ready_empty = {
            let mut state = self.core.state.lock();
            state.ready.retain(|queued| !Arc::ptr_eq(queued, &watch));
            state.ready.is_empty()
        };
        watch.queued.store(false, Ordering::Release);
        watch.queued_generation.store(0, Ordering::Release);
        if ready_empty {
            self.core.refresh_poll_source();
        }
        watch.interest.store(event.events as u16, Ordering::Release);
        watch.data.store(event.data, Ordering::Release);
        watch
            .edge_triggered
            .store((event.events & EPOLLET) != 0, Ordering::Release);
        watch
            .oneshot
            .store((event.events & EPOLLONESHOT) != 0, Ordering::Release);
        watch.last_ready.store(0, Ordering::Release);
        watch.queued_ready.store(0, Ordering::Release);
        watch.source_generation.store(0, Ordering::Release);
        watch.disabled.store(false, Ordering::Release);
        let source = file.poll_source().ok_or(Errno::EOPNOTSUPP)?;
        let (readiness, generation) = source.snapshot();
        watch.readiness_changed(source.id(), readiness, generation);
        Ok(())
    }

    fn ctl_del(&self, file: &Arc<File>) -> Result<(), Errno> {
        let watch = {
            let state = self.core.state.lock();
            state
                .watches
                .iter()
                .find(|watch| Arc::ptr_eq(&watch.file, file))
                .cloned()
                .ok_or(Errno::ENOENT)?
        };
        watch.disabled.store(true, Ordering::Release);
        watch.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(source) = watch.file.poll_source() {
            source.unsubscribe(watch.subscription_id.load(Ordering::Acquire));
        }
        let ready_empty = {
            let mut state = self.core.state.lock();
            if let Some(index) = state
                .watches
                .iter()
                .position(|candidate| Arc::ptr_eq(candidate, &watch))
            {
                state.watches.remove(index);
            }
            state.ready.retain(|queued| !Arc::ptr_eq(queued, &watch));
            state.ready.is_empty()
        };
        watch.queued.store(false, Ordering::Release);
        watch.queued_generation.store(0, Ordering::Release);
        if ready_empty {
            self.core.refresh_poll_source();
        }
        Ok(())
    }

    fn remove_closed_file(&self, file: &Arc<File>) {
        let _ = self.ctl_del(file);
    }

    fn clear_watches(&self) {
        let watches = {
            let mut state = self.core.state.lock();
            state.ready.clear();
            core::mem::take(&mut state.watches)
        };
        for watch in watches {
            watch.disabled.store(true, Ordering::Release);
            if let Some(source) = watch.file.poll_source() {
                source.unsubscribe(watch.subscription_id.load(Ordering::Acquire));
            }
        }
        self.core.refresh_poll_source();
    }

    fn any_ready(&self) -> bool {
        !self.core.state.lock().ready.is_empty()
    }

    fn collect_ready(&self, maxevents: usize) -> Vec<EpollEvent> {
        let mut out = Vec::with_capacity(maxevents);
        let mut requeue = Vec::with_capacity(maxevents);
        while out.len() < maxevents {
            let watch = {
                let mut state = self.core.state.lock();
                state.ready.pop_front()
            };
            let Some(watch) = watch else {
                break;
            };
            let queued_generation = watch.queued_generation.swap(0, Ordering::AcqRel);
            watch.queued.store(false, Ordering::Release);
            if watch.disabled.load(Ordering::Acquire)
                || queued_generation != watch.generation.load(Ordering::Acquire)
            {
                continue;
            }
            let interest = PollEvents(watch.interest.load(Ordering::Acquire));
            let ready = if watch.edge_triggered.load(Ordering::Acquire) {
                PollEvents(watch.queued_ready.swap(0, Ordering::AcqRel))
            } else {
                let Some(source) = watch.file.poll_source() else {
                    continue;
                };
                source
                    .snapshot()
                    .0
                    .intersect(interest.with(always_events()))
            };
            if ready.is_empty() {
                continue;
            }
            out.push(EpollEvent {
                events: ready.raw() as u32,
                data: watch.data.load(Ordering::Acquire),
            });
            if watch.oneshot.load(Ordering::Acquire) {
                watch.disabled.store(true, Ordering::Release);
            } else if !watch.edge_triggered.load(Ordering::Acquire) {
                requeue.push(watch);
            }
        }
        for watch in requeue {
            let generation = watch.generation.load(Ordering::Acquire);
            if let Some(source) = watch.file.poll_source() {
                let readiness = source.snapshot().0.intersect(
                    PollEvents(watch.interest.load(Ordering::Acquire)).with(always_events()),
                );
                watch.enqueue(readiness, generation);
            }
        }
        self.core.refresh_poll_source();
        out
    }

    fn contains_file_recursive(&self, needle: &Arc<File>, visited: &mut Vec<usize>) -> bool {
        let watches = self.core.state.lock().watches.clone();
        for watch in watches {
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

    fn nesting_depth_recursive(&self, visited: &mut Vec<usize>) -> usize {
        let watches = self.core.state.lock().watches.clone();
        let mut max_child_depth = 0;
        for watch in watches {
            let ptr = Arc::as_ptr(&watch.file) as usize;
            if visited.contains(&ptr) {
                continue;
            }
            let Some(child) = watch.file.downcast_ops::<EpollFileOps>() else {
                continue;
            };
            visited.push(ptr);
            max_child_depth = max_child_depth.max(child.nesting_depth_recursive(visited));
            visited.pop();
        }
        1 + max_child_depth
    }

    fn wait_until(
        &self,
        maxevents: usize,
        deadline: Option<u64>,
    ) -> Result<Vec<EpollEvent>, Errno> {
        loop {
            let ready = self.collect_ready(maxevents);
            if !ready.is_empty() || timeout_expired(deadline) {
                return Ok(ready);
            }
            let task = current_task();
            if has_unblocked_signal(&task) {
                return Err(Errno::EINTR);
            }
            let entry = self
                .core
                .waiters
                .prepare_to_wait(&task, TaskState::Sleeping);
            if self.any_ready() || timeout_expired(deadline) {
                self.core.waiters.finish_wait(&entry);
                continue;
            }
            let armed =
                deadline.is_some_and(|deadline| sched::register_sleep_deadline(&task, deadline));
            drop(task);
            sched::schedule_once(sched::now_ns_direct());
            let task = current_task();
            self.core.waiters.finish_wait(&entry);
            if armed {
                sched::cancel_sleep_deadline(&task);
            }
            if has_unblocked_signal(&task) {
                return Err(Errno::EINTR);
            }
        }
    }
}

impl EpollCore {
    fn refresh_poll_source(&self) {
        let version = self.poll_source.reserve_version();
        let readiness = if self.state.lock().ready.is_empty() {
            PollEvents::default()
        } else {
            PollEvents::POLLIN
        };
        self.poll_source.publish_versioned(readiness, version);
    }
}

impl EpollWatch {
    fn enqueue(&self, ready: PollEvents, generation: u64) {
        if ready.is_empty()
            || self.disabled.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) != generation
        {
            return;
        }
        self.queued_ready.fetch_or(ready.raw(), Ordering::AcqRel);
        if self
            .queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.queued_generation.store(generation, Ordering::Release);
        let Some(watch) = self.self_weak.upgrade() else {
            self.clear_queued(generation);
            return;
        };
        let Some(epoll) = self.epoll.upgrade() else {
            self.clear_queued(generation);
            return;
        };
        let wake = {
            let mut state = epoll.state.lock();
            if self.disabled.load(Ordering::Acquire)
                || self.generation.load(Ordering::Acquire) != generation
            {
                drop(state);
                self.clear_queued(generation);
                return;
            }
            let wake = state.ready.is_empty();
            state.ready.push_back(watch);
            wake
        };
        if wake {
            epoll.refresh_poll_source();
            epoll.waiters.wake_one_default();
        }
    }

    fn clear_queued(&self, generation: u64) {
        if self
            .queued_generation
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.queued.store(false, Ordering::Release);
        }
    }
}

impl PollSubscriber for EpollWatch {
    fn readiness_changed(&self, source: u64, readiness: PollEvents, source_generation: u64) {
        if source != self.source_id || self.disabled.load(Ordering::Acquire) {
            return;
        }
        let mut observed = self.source_generation.load(Ordering::Acquire);
        loop {
            if source_generation <= observed {
                return;
            }
            match self.source_generation.compare_exchange_weak(
                observed,
                source_generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => observed = current,
            }
        }
        let generation = self.generation.load(Ordering::Acquire);
        let interest = PollEvents(self.interest.load(Ordering::Acquire));
        let current = readiness.intersect(interest.with(always_events()));
        let ready = if self.edge_triggered.load(Ordering::Acquire) {
            let previous = self.last_ready.swap(current.raw(), Ordering::AcqRel);
            PollEvents(current.raw() & !previous)
        } else {
            current
        };
        self.enqueue(ready, generation);
    }
}

fn always_events() -> PollEvents {
    PollEvents::POLLERR
        .with(PollEvents::POLLHUP)
        .with(PollEvents::POLLRDHUP)
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
        self.core.poll_source.snapshot().0.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) {
            self.core.waiters.enqueue(task);
            true
        } else {
            false
        }
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.core.waiters.remove(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.core.poll_source)
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn on_file_description_closed(&self, file: &Arc<File>) {
        self.remove_closed_file(file);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.clear_watches();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn timeout_deadline(timeout_ms: i64) -> Option<u64> {
    if timeout_ms >= 0 {
        Some(sched::now_ns_direct().saturating_add((timeout_ms as u64).saturating_mul(1_000_000)))
    } else {
        None
    }
}

fn timeout_expired(deadline: Option<u64>) -> bool {
    deadline.is_some_and(|dl| sched::now_ns_direct() >= dl)
}

#[cfg(test)]
pub(crate) fn wait_recheck_deadline(
    now_ns: u64,
    deadline: Option<u64>,
    sources_empty: bool,
    has_unregistered_source: bool,
) -> Option<u64> {
    const EPOLL_RECHECK_NS: u64 = 10_000_000;
    const SHORT_EMPTY_WAIT_SPIN_TAIL_NS: u64 = 500_000;
    const LONG_EMPTY_WAIT_SPIN_TAIL_NS: u64 = 2_000_000;
    const LONG_EMPTY_WAIT_THRESHOLD_NS: u64 = 20_000_000;

    if sources_empty && let Some(deadline) = deadline {
        let remaining = deadline.saturating_sub(now_ns);
        let spin_tail = if remaining >= LONG_EMPTY_WAIT_THRESHOLD_NS {
            LONG_EMPTY_WAIT_SPIN_TAIL_NS
        } else {
            SHORT_EMPTY_WAIT_SPIN_TAIL_NS
        };
        return Some(deadline.saturating_sub(spin_tail).max(now_ns));
    }
    if !has_unregistered_source && (deadline.is_some() || !sources_empty) {
        return deadline;
    }
    let quantum = now_ns.saturating_add(EPOLL_RECHECK_NS);
    Some(deadline.map_or(quantum, |dl| dl.min(quantum)))
}

fn has_unblocked_signal(task: &Arc<sched::Task>) -> bool {
    sched::operation::has_interrupting_signal(task)
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

fn epoll_ops_from_fd(file: &Arc<File>) -> Result<&EpollFileOps, Errno> {
    file.downcast_ops::<EpollFileOps>().ok_or(Errno::EINVAL)
}

pub fn create(fdt: &FdTable, cred: Arc<Credentials>, cloexec: bool) -> Result<Fd, Errno> {
    let flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    let file = new_epoll_file(cred);
    let fd = fdt
        .alloc_fd(Arc::clone(&file), flags)
        .map_err(|e| e.to_errno())?;
    Ok(fd)
}

pub fn ctl(
    fdt: &FdTable,
    epfd: Fd,
    op: i32,
    fd: Fd,
    event: Option<EpollEvent>,
) -> Result<(), Errno> {
    const MAX_EPOLL_NESTING_DEPTH: usize = 5;

    let epoll_file = fdt.get_file(epfd).ok_or(Errno::EBADF)?;
    let ops = epoll_ops_from_fd(&epoll_file)?;
    let target = fdt.get_file(fd).ok_or(Errno::EBADF)?;

    if op == EPOLL_CTL_ADD {
        if Arc::ptr_eq(&epoll_file, &target) {
            return Err(Errno::EINVAL);
        }
        if !target.is_epollable() {
            return Err(Errno::EPERM);
        }
        if let Some(target_epoll) = target.downcast_ops::<EpollFileOps>() {
            let mut visited = vec![Arc::as_ptr(&target) as usize];
            if target_epoll.contains_file_recursive(&epoll_file, &mut visited) {
                return Err(Errno::ELOOP);
            }
            let mut depth_visited = vec![Arc::as_ptr(&target) as usize];
            if target_epoll.nesting_depth_recursive(&mut depth_visited) >= MAX_EPOLL_NESTING_DEPTH {
                return Err(Errno::EINVAL);
            }
        }
        target.register_description_close_observer(&epoll_file);
    }
    match op {
        EPOLL_CTL_ADD => ops.ctl_add(fd, target, event.ok_or(Errno::EINVAL)?),
        EPOLL_CTL_MOD => ops.ctl_mod(&target, event.ok_or(Errno::EINVAL)?),
        EPOLL_CTL_DEL => ops.ctl_del(&target),
        _ => Err(Errno::EINVAL),
    }
}

pub fn wait(
    fdt: &FdTable,
    epfd: Fd,
    maxevents: usize,
    timeout_ms: i64,
) -> Result<Vec<EpollEvent>, Errno> {
    wait_until(fdt, epfd, maxevents, timeout_deadline(timeout_ms))
}

/// 使用调用方建立的绝对单调时钟截止时间等待 epoll 事件。
///
/// syscall 层应尽早建立 deadline，使 fd 查找与临时信号掩码安装等固定前处理
/// 计入等待时间，同时避免 `epoll_pwait2` 把纳秒超时向上取整为毫秒。
pub fn wait_until(
    fdt: &FdTable,
    epfd: Fd,
    maxevents: usize,
    deadline: Option<u64>,
) -> Result<Vec<EpollEvent>, Errno> {
    let epoll_file = fdt.get_file(epfd).ok_or(Errno::EBADF)?;
    let ops = epoll_ops_from_fd(&epoll_file)?;
    ops.wait_until(maxevents, deadline)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::anon;
    use crate::eventfd::EventfdFileOps;
    use crate::vfs::file::AccessMode;
    use std::sync::Barrier;
    use std::thread;

    struct SourceOnlyFileOps {
        source: PollSource,
    }

    impl FileOps for SourceOnlyFileOps {
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

        fn poll(&self, _interest: PollEvents) -> PollEvents {
            panic!("epoll ready queue 不得回退调用 file.poll")
        }

        fn poll_source(&self) -> Option<&PollSource> {
            Some(&self.source)
        }

        fn release(&self) {}

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn eventfd_file() -> Arc<File> {
        anon::new_file(
            Arc::new(Credentials::root()),
            OpenOptions {
                access: AccessMode::ReadWrite,
                ..Default::default()
            },
            Box::new(EventfdFileOps::new(0, false)),
        )
    }

    fn publish_event(file: &Arc<File>) {
        file.write(&1u64.to_ne_bytes()).unwrap();
    }

    fn consume_event(file: &Arc<File>) {
        let mut value = [0u8; 8];
        file.read(&mut value).unwrap();
    }

    fn add(epoll: &EpollFileOps, file: Arc<File>, events: u32, data: u64) {
        epoll
            .ctl_add(Fd::from_raw(1), file, EpollEvent { events, data })
            .unwrap();
    }

    #[test]
    fn level_triggered_watch_requeues_while_level_remains_ready() {
        let epoll = EpollFileOps::new();
        let file = eventfd_file();
        add(
            &epoll,
            Arc::clone(&file),
            PollEvents::POLLIN.raw() as u32,
            7,
        );
        publish_event(&file);
        assert_eq!(epoll.collect_ready(1)[0].data, 7);
        assert_eq!(epoll.collect_ready(1)[0].data, 7);
        consume_event(&file);
        assert!(epoll.collect_ready(1).is_empty());
    }

    #[test]
    fn edge_triggered_watch_requires_a_new_readiness_edge() {
        let epoll = EpollFileOps::new();
        let file = eventfd_file();
        add(
            &epoll,
            Arc::clone(&file),
            PollEvents::POLLIN.raw() as u32 | EPOLLET,
            11,
        );
        publish_event(&file);
        assert_eq!(epoll.collect_ready(1).len(), 1);
        assert!(epoll.collect_ready(1).is_empty());
        consume_event(&file);
        publish_event(&file);
        assert_eq!(epoll.collect_ready(1)[0].data, 11);
    }

    #[test]
    fn oneshot_watch_is_rearmed_by_mod() {
        let epoll = EpollFileOps::new();
        let file = eventfd_file();
        add(
            &epoll,
            Arc::clone(&file),
            PollEvents::POLLIN.raw() as u32 | EPOLLONESHOT,
            13,
        );
        publish_event(&file);
        assert_eq!(epoll.collect_ready(1).len(), 1);
        assert!(epoll.collect_ready(1).is_empty());
        epoll
            .ctl_mod(
                &file,
                EpollEvent {
                    events: PollEvents::POLLIN.raw() as u32 | EPOLLONESHOT,
                    data: 17,
                },
            )
            .unwrap();
        assert_eq!(epoll.collect_ready(1)[0].data, 17);
    }

    #[test]
    fn deleting_a_queued_watch_discards_the_pending_event() {
        let epoll = EpollFileOps::new();
        let file = eventfd_file();
        add(
            &epoll,
            Arc::clone(&file),
            PollEvents::POLLIN.raw() as u32,
            19,
        );
        publish_event(&file);
        epoll.ctl_del(&file).unwrap();
        assert!(epoll.collect_ready(1).is_empty());
    }

    #[test]
    fn nested_epoll_is_driven_by_the_child_ready_source() {
        let child_file = anon::new_file(
            Arc::new(Credentials::root()),
            OpenOptions {
                access: AccessMode::ReadWrite,
                ..Default::default()
            },
            Box::new(EpollFileOps::new()),
        );
        let child = child_file.downcast_ops::<EpollFileOps>().unwrap();
        let event = eventfd_file();
        add(
            child,
            Arc::clone(&event),
            PollEvents::POLLIN.raw() as u32,
            23,
        );

        let parent = EpollFileOps::new();
        add(
            &parent,
            Arc::clone(&child_file),
            PollEvents::POLLIN.raw() as u32,
            29,
        );
        publish_event(&event);
        assert_eq!(parent.collect_ready(1)[0].data, 29);
        assert_eq!(child.collect_ready(1)[0].data, 23);
    }

    #[test]
    fn ready_delivery_does_not_scan_file_poll_methods() {
        let file = anon::new_file(
            Arc::new(Credentials::root()),
            OpenOptions {
                access: AccessMode::ReadWrite,
                ..Default::default()
            },
            Box::new(SourceOnlyFileOps {
                source: PollSource::new(PollEvents::default()),
            }),
        );
        let epoll = EpollFileOps::new();
        add(
            &epoll,
            Arc::clone(&file),
            PollEvents::POLLIN.raw() as u32,
            31,
        );
        file.downcast_ops::<SourceOnlyFileOps>()
            .unwrap()
            .source
            .publish(PollEvents::POLLIN);
        assert_eq!(epoll.collect_ready(1)[0].data, 31);
    }

    #[test]
    fn concurrent_add_of_same_file_has_single_winner() {
        let epoll = Arc::new(EpollFileOps::new());
        let file = eventfd_file();
        let barrier = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();
        for data in [37, 41] {
            let epoll = Arc::clone(&epoll);
            let file = Arc::clone(&file);
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                barrier.wait();
                epoll.ctl_add(
                    Fd::from_raw(1),
                    file,
                    EpollEvent {
                        events: PollEvents::POLLIN.raw() as u32,
                        data,
                    },
                )
            }));
        }
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(Errno::EEXIST))
                .count(),
            1
        );
        assert_eq!(epoll.core.state.lock().watches.len(), 1);
    }
}
