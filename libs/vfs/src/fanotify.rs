//! fanotify 组：标记（inode/mount/filesystem）、事件队列与权限事件响应。
//!
//! Linux 语义要点（对照 fs/notify/fanotify/）：
//! - 每个 FAN_CLASS_* 只允许一个组；CONTENT/PRE_CONTENT 与 UNLIMITED_* 需要
//!   CAP_SYS_ADMIN；
//! - 标记分 inode/mount/filesystem 三类，经 fsnotify 核心注册（WatchTarget）；
//!   FAN_MARK_IGNORED_MASK 命中时抑制投递；命名（子对象）事件要求掩码带
//!   FAN_EVENT_ON_CHILD；
//! - 事件元数据 `fanotify_event_metadata`（24 字节，vers=2），对象 fd 在
//!   **读取时**按 event_f_flags 打开并分配（权限事件回填 fd 供响应匹配）；
//! - 权限事件（FAN_OPEN_PERM 等）：注入点阻塞当前任务直到用户态向组 fd
//!   写 `fanotify_response { fd, response, flags }`；FAN_DENY → 操作返回
//!   EACCES；信号中断 → 事件保留、操作返回 EINTR（Linux ERESTARTSYS）。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::fsnotify::{self, NotifyEvent, Watch, WatchScope, WatchTarget};
use crate::poll_source::PollSource;
use crate::vfs::anon;
use crate::vfs::cred::{Capability, Credentials};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{AccessMode, DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::Inode;
use crate::vfs::mount::Mount;
use crate::vfs::sync::Spinlock;

// ── UAPI 常量（Linux 值）───────────────────────────────────────────────────

pub const FAN_ACCESS: u32 = 0x1;
pub const FAN_MODIFY: u32 = 0x2;
pub const FAN_ATTRIB: u32 = 0x4;
pub const FAN_CLOSE_WRITE: u32 = 0x8;
pub const FAN_CLOSE_NOWRITE: u32 = 0x10;
pub const FAN_OPEN: u32 = 0x20;
pub const FAN_MOVED_FROM: u32 = 0x40;
pub const FAN_MOVED_TO: u32 = 0x80;
pub const FAN_CREATE: u32 = 0x100;
pub const FAN_DELETE: u32 = 0x200;
pub const FAN_DELETE_SELF: u32 = 0x400;
pub const FAN_MOVE_SELF: u32 = 0x800;
pub const FAN_OPEN_EXEC: u32 = 0x1000;
pub const FAN_Q_OVERFLOW: u32 = 0x4000;
pub const FAN_OPEN_PERM: u32 = 0x1_0000;
pub const FAN_ACCESS_PERM: u32 = 0x2_0000;
pub const FAN_OPEN_EXEC_PERM: u32 = 0x4_0000;
pub const FAN_ONDIR: u32 = 0x4000_0000;
pub const FAN_EVENT_ON_CHILD: u32 = 0x0800_0000;

/// 事件掩码（非标记标志位）。
pub const FAN_EVENT_MASK: u32 = FAN_ACCESS
    | FAN_MODIFY
    | FAN_ATTRIB
    | FAN_CLOSE_WRITE
    | FAN_CLOSE_NOWRITE
    | FAN_OPEN
    | FAN_MOVED_FROM
    | FAN_MOVED_TO
    | FAN_CREATE
    | FAN_DELETE
    | FAN_DELETE_SELF
    | FAN_MOVE_SELF
    | FAN_OPEN_EXEC
    | FAN_OPEN_PERM
    | FAN_ACCESS_PERM
    | FAN_OPEN_EXEC_PERM
    | FAN_EVENT_ON_CHILD;

pub const FAN_CLASS_NOTIF: u32 = 0x0;
pub const FAN_CLASS_CONTENT: u32 = 0x4;
pub const FAN_CLASS_PRE_CONTENT: u32 = 0x8;
pub const FAN_CLASS_MASK: u32 = FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT;
pub const FAN_UNLIMITED_QUEUE: u32 = 0x10;
pub const FAN_UNLIMITED_MARKS: u32 = 0x20;
pub const FAN_CLOEXEC: u32 = 0x1;
pub const FAN_NONBLOCK: u32 = 0x2;

pub const FAN_MARK_ADD: u32 = 0x1;
pub const FAN_MARK_REMOVE: u32 = 0x2;
pub const FAN_MARK_DONT_FOLLOW: u32 = 0x4;
pub const FAN_MARK_ONLYDIR: u32 = 0x8;
pub const FAN_MARK_MOUNT: u32 = 0x10;
pub const FAN_MARK_IGNORED_MASK: u32 = 0x20;
pub const FAN_MARK_FLUSH: u32 = 0x80;
pub const FAN_MARK_FILESYSTEM: u32 = 0x100;
pub const FAN_MARK_FLAGS: u32 = FAN_MARK_ADD
    | FAN_MARK_REMOVE
    | FAN_MARK_DONT_FOLLOW
    | FAN_MARK_ONLYDIR
    | FAN_MARK_MOUNT
    | FAN_MARK_IGNORED_MASK
    | FAN_MARK_FLUSH
    | FAN_MARK_FILESYSTEM;

pub const FAN_ALLOW: u32 = 0x1;
pub const FAN_DENY: u32 = 0x2;

/// `struct fanotify_event_metadata` 头大小。
pub const FANOTIFY_METADATA_LEN: usize = 24;

/// 权限事件响应 `struct fanotify_response`。
const RESPONSE_SIZE: usize = 12; // fd i32 + response u32 + flags u32

/// 每类只允许一个组。
static GROUPS: Spinlock<BTreeMap<u32, Weak<FanotifyGroup>>> = Spinlock::new(BTreeMap::new());

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FanClass {
    Notif,
    Content,
    PreContent,
}

impl FanClass {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw & FAN_CLASS_MASK {
            FAN_CLASS_NOTIF => Some(Self::Notif),
            FAN_CLASS_CONTENT => Some(Self::Content),
            FAN_CLASS_PRE_CONTENT => Some(Self::PreContent),
            _ => None,
        }
    }

    fn raw(self) -> u32 {
        match self {
            Self::Notif => FAN_CLASS_NOTIF,
            Self::Content => FAN_CLASS_CONTENT,
            Self::PreContent => FAN_CLASS_PRE_CONTENT,
        }
    }

    fn allows_perm(self) -> bool {
        !matches!(self, Self::Notif)
    }
}

/// 组内一条标记（持有注册进 fsnotify 核心的监视）。
struct FanMark {
    watch: Arc<Watch>,
    key: fsnotify::WatchKey,
}

/// 队列中的事件（权限事件带 perm_id 与响应等待者）。
struct QueuedEvent {
    perm_id: Option<u64>,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
    inode: Weak<Inode>,
    dentry: Weak<crate::vfs::dentry::Dentry>,
    mount: Weak<Mount>,
}

/// 权限事件等待状态。
struct PermPending {
    responded: AtomicU32,
    waiters: WaitQueue,
}

pub struct FanotifyGroup {
    id: u64,
    class: FanClass,
    nonblock: bool,
    event_f_flags: u32,
    queue_limit: usize,
    queue: Spinlock<VecDeque<QueuedEvent>>,
    overflow: core::sync::atomic::AtomicBool,
    marks: Spinlock<Vec<Arc<FanMark>>>,
    /// 读取时分配的 fd → perm_id（响应匹配）。
    fd_to_perm: Spinlock<BTreeMap<i32, u64>>,
    /// perm_id → 等待者。
    pending_perm: Spinlock<BTreeMap<u64, Arc<PermPending>>>,
    next_perm_id: AtomicU64,
    waiters: WaitQueue,
    poll_source: PollSource,
    self_weak: Spinlock<Weak<FanotifyGroup>>,
}

pub struct FanotifyFileOps {
    group: Arc<FanotifyGroup>,
}

impl FanotifyGroup {
    fn new(
        class: FanClass,
        nonblock: bool,
        event_f_flags: u32,
        unlimited_queue: bool,
        unlimited_marks: bool,
    ) -> Arc<Self> {
        let _ = unlimited_marks;
        Arc::new_cyclic(|self_weak| FanotifyGroup {
            id: next_group_id(),
            class,
            nonblock,
            event_f_flags,
            queue_limit: if unlimited_queue { 1_048_576 } else { 16_384 },
            queue: Spinlock::new(VecDeque::new()),
            overflow: core::sync::atomic::AtomicBool::new(false),
            marks: Spinlock::new(Vec::new()),
            fd_to_perm: Spinlock::new(BTreeMap::new()),
            pending_perm: Spinlock::new(BTreeMap::new()),
            next_perm_id: AtomicU64::new(1),
            waiters: WaitQueue::new(),
            poll_source: PollSource::new(PollEvents::default()),
            self_weak: Spinlock::new(self_weak.clone()),
        })
    }

    fn wake(&self) {
        self.poll_source.publish(PollEvents::POLLIN);
        self.waiters.wake_all();
    }

    fn enqueue(&self, event: QueuedEvent) {
        let mut queue = self.queue.lock();
        if queue.len() >= self.queue_limit {
            self.overflow.store(true, Ordering::Release);
            return;
        }
        queue.push_back(event);
        drop(queue);
        self.wake();
    }

    /// 添加/更新标记。`mask` 为事件位（不含标记标志）；`ignored` 为
    /// FAN_MARK_IGNORED_MASK 位。
    fn add_mark(
        &self,
        scope: WatchScope,
        inode: Option<&Arc<Inode>>,
        dentry: Option<&Arc<crate::vfs::dentry::Dentry>>,
        mount: Option<&Arc<Mount>>,
        sb_id: u64,
        mask: u32,
        ignored: u32,
        onlydir: bool,
    ) -> Result<(), Errno> {
        if onlydir
            && inode
                .map(|i| i.kind() != crate::vfs::stat::FileType::Directory)
                .unwrap_or(false)
        {
            return Err(Errno::ENOTDIR);
        }
        let key = fsnotify::WatchKey::from_scope(scope, inode, mount, sb_id);
        let mut marks = self.marks.lock();
        // 同 (组, key) 标记：事件掩码与 ignored 掩码按位 OR 合并
        // （Linux fanotify_add_mark 对已存在标记累加事件位）。
        for mark in marks.iter() {
            if mark.key == key {
                let old_mask = mark.watch.mask.load(Ordering::Acquire);
                let old_ignored = mark.watch.ignored_mask.load(Ordering::Acquire);
                mark.watch.mask.store(old_mask | mask, Ordering::Release);
                mark.watch
                    .ignored_mask
                    .store(old_ignored | ignored, Ordering::Release);
                return Ok(());
            }
        }
        let perm = mask & (FAN_OPEN_PERM | FAN_ACCESS_PERM | FAN_OPEN_EXEC_PERM) != 0;
        let wd = 1 + marks.len() as i32;
        let watch = Arc::new(Watch {
            wd,
            mask: AtomicU32::new(mask),
            flags: 0,
            unlinked: core::sync::atomic::AtomicBool::new(false),
            inode: inode.map(Arc::downgrade).unwrap_or_else(|| Weak::new()),
            dentry: dentry.map(Arc::downgrade).unwrap_or_else(|| Weak::new()),
            mount: mount.map(Arc::downgrade).unwrap_or_else(|| Weak::new()),
            target: self.self_weak.lock().clone(),
            scope,
            ignored_mask: AtomicU32::new(ignored),
            perm,
            named_requires_echild: true,
        });
        fsnotify::register_key(key, Arc::downgrade(&watch), perm);
        marks.push(Arc::new(FanMark { watch, key }));
        Ok(())
    }

    /// 移除标记（mask=0 整体移除；否则清除对应事件位）。
    fn remove_mark(&self, key: fsnotify::WatchKey, mask: u32) -> Result<(), Errno> {
        let mut marks = self.marks.lock();
        let Some(pos) = marks.iter().position(|m| m.key == key) else {
            return Err(Errno::ENOENT);
        };
        if mask == 0 {
            let watch = Arc::clone(&marks.swap_remove(pos).watch);
            fsnotify::unregister_key(key, &watch);
            return Ok(());
        }
        let mark = &marks[pos];
        let new_mask = mark.watch.mask.load(Ordering::Acquire) & !mask;
        if new_mask == 0 {
            let watch = Arc::clone(&marks.swap_remove(pos).watch);
            fsnotify::unregister_key(key, &watch);
        } else {
            mark.watch.mask.store(new_mask, Ordering::Release);
        }
        Ok(())
    }

    /// 清除全部标记（组关闭 / FAN_MARK_FLUSH）。
    fn remove_all_marks(&self) {
        let marks: Vec<Arc<FanMark>> = self.marks.lock().drain(..).collect();
        for mark in marks {
            fsnotify::unregister_key(mark.key, &mark.watch);
        }
    }

    /// 权限事件：生成事件并阻塞当前任务直到用户态响应。
    /// 返回 `Ok(true)`=允许 / `Ok(false)`=拒绝 / `Err(EINTR)`=信号中断。
    fn await_permission_impl(
        &self,
        inode: &Arc<Inode>,
        mount: Option<&Arc<Mount>>,
        mask: u32,
    ) -> Result<bool, Errno> {
        let perm_id = self.next_perm_id.fetch_add(1, Ordering::Relaxed);
        let pending = Arc::new(PermPending {
            responded: AtomicU32::new(0),
            waiters: WaitQueue::new(),
        });
        self.pending_perm
            .lock()
            .insert(perm_id, Arc::clone(&pending));
        // 权限事件不受队列上限约束（丢弃会导致死锁）。
        self.queue.lock().push_back(QueuedEvent {
            perm_id: Some(perm_id),
            mask,
            cookie: 0,
            name: Vec::new(),
            inode: Arc::downgrade(inode),
            dentry: Weak::new(),
            mount: mount.map(Arc::downgrade).unwrap_or_else(Weak::new),
        });
        self.wake();

        let task = sched::current_task();
        loop {
            if pending.responded.load(Ordering::Acquire) != 0 {
                break;
            }
            // 信号中断：per-task 或线程组共享 pending 均可打断（kill 投递的是
            // 共享信号）。事件保留（响应到达后丢弃），操作返回 EINTR。
            if task.signal.has_pending_in(u64::MAX) || task.shared_signal_pending_bits_quick() != 0
            {
                self.pending_perm.lock().remove(&perm_id);
                return Err(Errno::EINTR);
            }
            let entry = pending
                .waiters
                .prepare_to_wait(&task, sched::TaskState::Sleeping);
            if pending.responded.load(Ordering::Acquire) != 0 {
                pending.waiters.finish_wait(&entry);
                break;
            }
            sched::schedule_once(sched::now_ns_public());
            pending.waiters.finish_wait(&entry);
        }
        let resp = pending.responded.load(Ordering::Acquire);
        self.pending_perm.lock().remove(&perm_id);
        Ok(resp == FAN_ALLOW)
    }

    /// 读取时把权限事件元数据中的 fd 与 perm_id 挂钩。
    fn bind_perm_fd(&self, fd: i32, perm_id: u64) {
        self.fd_to_perm.lock().insert(fd, perm_id);
    }

    /// 处理响应写。
    fn process_response(&self, fd: i32, response: u32) -> Result<(), Errno> {
        if response & !(FAN_ALLOW | FAN_DENY) != 0 {
            return Err(Errno::EINVAL);
        }
        let perm_id = {
            let map = self.fd_to_perm.lock();
            match map.get(&fd) {
                Some(id) => *id,
                None => return Err(Errno::ENOENT),
            }
        };
        self.fd_to_perm.lock().remove(&fd);
        let pending = self.pending_perm.lock().get(&perm_id).map(Arc::clone);
        let Some(pending) = pending else {
            return Err(Errno::ENOENT);
        };
        pending.responded.store(response, Ordering::Release);
        pending.waiters.wake_all();
        Ok(())
    }

    /// 读取事件：填充元数据 + 分配对象 fd。
    fn read_events(
        &self,
        buf: &mut [u8],
        nonblock: bool,
        fdt: &FdTable,
        cred: &Arc<Credentials>,
    ) -> VfsResult<usize> {
        if buf.len() < FANOTIFY_METADATA_LEN {
            return Err(VfsError::InvalidArgument);
        }
        // 等待队列非空（阻塞模式睡眠在组 waiters 上；非阻塞直接 EAGAIN）。
        loop {
            if !self.queue.lock().is_empty() {
                break;
            }
            if nonblock {
                return Err(VfsError::WouldBlock);
            }
            let task = sched::current_task();
            let entry = self
                .waiters
                .prepare_to_wait(&task, sched::TaskState::Sleeping);
            if !self.queue.lock().is_empty() {
                self.waiters.finish_wait(&entry);
                break;
            }
            sched::schedule_once(sched::now_ns_public());
            self.waiters.finish_wait(&entry);
        }
        {
            let mut queue = self.queue.lock();
            if self.overflow.swap(false, Ordering::AcqRel) {
                queue.push_front(QueuedEvent {
                    perm_id: None,
                    mask: FAN_Q_OVERFLOW,
                    cookie: 0,
                    name: Vec::new(),
                    inode: Weak::new(),
                    dentry: Weak::new(),
                    mount: Weak::new(),
                });
            }
            let event = queue.front().unwrap();
            let total = FANOTIFY_METADATA_LEN + event.name.len();
            if buf.len() < total {
                return Err(VfsError::InvalidArgument);
            }
            let event = queue.pop_front().unwrap();
            drop(queue);

            // 分配对象 fd（权限事件回填 fd 供响应匹配；失败则 fd=-1）。
            let fd = if event.inode.upgrade().is_some() {
                self.open_event_fd(&event, fdt, cred).unwrap_or(-1)
            } else {
                -1
            };
            if let Some(perm_id) = event.perm_id {
                if fd >= 0 {
                    self.bind_perm_fd(fd, perm_id);
                } else {
                    // 无法提供 fd 的权限事件无法被响应：拒绝操作。
                    self.pending_perm.lock().remove(&perm_id);
                }
            }

            let mut out = [0u8; FANOTIFY_METADATA_LEN];
            out[0..4].copy_from_slice(&(total as u32).to_le_bytes());
            out[4] = 2; // vers
            out[5] = 0;
            out[6..8].copy_from_slice(&(FANOTIFY_METADATA_LEN as u16).to_le_bytes());
            out[8..12].copy_from_slice(&event.mask.to_le_bytes());
            out[12..16].fill(0); // mask 高 32 位
            out[16..20].copy_from_slice(&fd.to_le_bytes());
            let pid = sched::current_task().pid_root_cached().unwrap_or(0) as i32;
            out[20..24].copy_from_slice(&pid.to_le_bytes());
            buf[..FANOTIFY_METADATA_LEN].copy_from_slice(&out);
            buf[FANOTIFY_METADATA_LEN..total].copy_from_slice(&event.name);
            self.poll_source.publish(if self.queue.lock().is_empty() {
                PollEvents::default()
            } else {
                PollEvents::POLLIN
            });
            return Ok(total);
        }
    }

    fn open_event_fd(
        &self,
        event: &QueuedEvent,
        fdt: &FdTable,
        cred: &Arc<Credentials>,
    ) -> Result<i32, Errno> {
        let inode = event.inode.upgrade().ok_or(Errno::ENOENT)?;
        let opts = OpenOptions {
            access: self.event_access_mode(),
            nonblock: false,
            ..Default::default()
        };
        let ops = inode
            .ops
            .open(&inode, &opts, cred)
            .map_err(|e| e.to_errno())?;
        // 事件对象 dentry 可能已失效（如已被 unlink）：用临时空名 dentry 兜底，
        // 保证事件 fd 仍可打开（Linux 中该 fd 的路径显示为空）。
        let dentry = match event.dentry.upgrade() {
            Some(d) => d,
            None => crate::vfs::dentry::Dentry::new_positive("", None, Arc::clone(&inode)),
        };
        let mount = event.mount.upgrade().ok_or(Errno::ENOENT)?;
        let file = Arc::new(File::new(inode, opts, Arc::clone(cred), ops, dentry, mount));
        let fd = fdt
            .alloc_fd(file, FdFlags::default())
            .map_err(|e| e.to_errno())?;
        Ok(fd.as_raw() as i32)
    }

    fn event_access_mode(&self) -> AccessMode {
        match self.event_f_flags & 0o3 {
            1 => AccessMode::WriteOnly,
            2 => AccessMode::ReadWrite,
            _ => AccessMode::ReadOnly,
        }
    }

    fn render_fdinfo(&self, out: &mut String) {
        use core::fmt::Write;
        let _ = writeln!(out, "fanotify flags:{:x}", self.event_f_flags);
        let marks = self.marks.lock();
        for mark in marks.iter() {
            let (ino, sdev) = match mark.key {
                fsnotify::WatchKey::Inode(key) => (key.1, key.0 as u32),
                _ => (0, 0),
            };
            let _ = writeln!(
                out,
                "fanotify ino:{:x} sdev:{:x} mflags:{:x} mask:{:x} ignored_mask:{:x}",
                ino,
                sdev,
                mark.watch.scope as u32,
                mark.watch.mask.load(Ordering::Acquire),
                mark.watch.ignored_mask.load(Ordering::Acquire),
            );
        }
    }
}

impl WatchTarget for FanotifyGroup {
    fn deliver(&self, event: &NotifyEvent) {
        self.enqueue(QueuedEvent {
            perm_id: None,
            mask: event.mask,
            cookie: event.cookie,
            name: event.name.clone(),
            inode: event.obj_inode.clone().unwrap_or_else(Weak::new),
            dentry: Weak::new(),
            mount: event.obj_mount.clone().unwrap_or_else(Weak::new),
        });
    }

    fn on_watch_removed(&self, wd: i32, _ignored: bool) {
        self.marks.lock().retain(|m| m.watch.wd != wd);
    }

    fn await_permission(
        &self,
        inode: &Arc<Inode>,
        mount: Option<&Arc<Mount>>,
        mask: u32,
    ) -> Result<bool, errno::Errno> {
        self.await_permission_impl(inode, mount, mask)
    }
}

impl FileOps for FanotifyFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        // 从当前任务取 fd 表 + 凭据（读事件时分配对象 fd）。
        let (fdt, cred) = crate::fdtable::current_vfs_state().ok_or(VfsError::BadFileDescriptor)?;
        self.group
            .read_events(buf, self.group.nonblock, &fdt, &cred)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < RESPONSE_SIZE {
            return Err(VfsError::InvalidArgument);
        }
        let fd = i32::from_le_bytes(buf[0..4].try_into().unwrap());
        let response = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        self.group
            .process_response(fd, response)
            .map_err(|e| match e {
                Errno::EINVAL => VfsError::InvalidArgument,
                _ => VfsError::NotFound,
            })?;
        Ok(RESPONSE_SIZE)
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
        self.group.poll_source.snapshot().0.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) {
            self.group.waiters.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.group.waiters.remove(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.group.poll_source)
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
        self.group.remove_all_marks();
        self.group.waiters.wake_all();
    }

    fn show_fdinfo(&self, out: &mut String) {
        self.group.render_fdinfo(out);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// fanotify_init：创建组 fd。
pub fn create_group(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    flags: u32,
    event_f_flags: u32,
) -> Result<Fd, Errno> {
    if flags
        & !(FAN_CLASS_MASK | FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS | FAN_CLOEXEC | FAN_NONBLOCK)
        != 0
    {
        return Err(Errno::EINVAL);
    }
    let class = FanClass::from_raw(flags).ok_or(Errno::EINVAL)?;
    // 每类一个组。
    {
        let groups = GROUPS.lock();
        if let Some(g) = groups.get(&class.raw()).and_then(|w| w.upgrade()) {
            let _ = g;
            return Err(Errno::EMFILE);
        }
    }
    let needs_cap = class.allows_perm() || flags & (FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS) != 0;
    if needs_cap && !cred.has_cap(Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    // event_f_flags：仅允许访问模式 + O_CLOEXEC。
    if event_f_flags & !(0o3 | 0o2000000) != 0 {
        return Err(Errno::EINVAL);
    }
    let group = FanotifyGroup::new(
        class,
        flags & FAN_NONBLOCK != 0,
        event_f_flags,
        flags & FAN_UNLIMITED_QUEUE != 0,
        flags & FAN_UNLIMITED_MARKS != 0,
    );
    GROUPS.lock().insert(class.raw(), Arc::downgrade(&group));
    let file_flags = OpenOptions {
        access: AccessMode::ReadWrite,
        nonblock: flags & FAN_NONBLOCK != 0,
        ..Default::default()
    };
    let fd_flags = if flags & FAN_CLOEXEC != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    anon::create_fd(
        fdt,
        cred,
        file_flags,
        fd_flags,
        Box::new(FanotifyFileOps { group }),
    )
    .map_err(|err| err.to_errno())
}

/// 按 fd 取组（fanotify_mark 用）。
pub fn group_from_file(file: &File) -> Option<Arc<FanotifyGroup>> {
    file.downcast_ops::<FanotifyFileOps>()
        .map(|ops| Arc::clone(&ops.group))
}

/// fanotify_mark：解析后的 inode/mount/sb 上执行 ADD/REMOVE/FLUSH。
///
/// `has_sysadmin` 由内核胶水层按当前凭据计算（FAN_MARK_FILESYSTEM 需要）。
pub fn mark(
    group: &FanotifyGroup,
    flags: u32,
    mask: u32,
    inode: Option<&Arc<Inode>>,
    dentry: Option<&Arc<crate::vfs::dentry::Dentry>>,
    mount: Option<&Arc<Mount>>,
    sb_id: u64,
    has_sysadmin: bool,
) -> Result<(), Errno> {
    if flags & !FAN_MARK_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let scope = if flags & FAN_MARK_MOUNT != 0 {
        if flags & FAN_MARK_FILESYSTEM != 0 {
            return Err(Errno::EINVAL);
        }
        WatchScope::Mount
    } else if flags & FAN_MARK_FILESYSTEM != 0 {
        WatchScope::Filesystem
    } else {
        WatchScope::Inode
    };
    if flags & FAN_MARK_FILESYSTEM != 0 && !has_sysadmin {
        return Err(Errno::EPERM);
    }
    // 掩码校验：事件位 + 标记标志位；NOTIF 类不允许 PERM 位。
    if mask & !FAN_EVENT_MASK != 0 {
        return Err(Errno::EINVAL);
    }
    if !group.class.allows_perm()
        && mask & (FAN_OPEN_PERM | FAN_ACCESS_PERM | FAN_OPEN_EXEC_PERM) != 0
    {
        return Err(Errno::EINVAL);
    }
    let ignored = if flags & FAN_MARK_IGNORED_MASK != 0 {
        mask
    } else {
        0
    };
    let event_mask = if flags & FAN_MARK_IGNORED_MASK != 0 {
        0
    } else {
        mask
    };
    let key = fsnotify::WatchKey::from_scope(scope, inode, mount, sb_id);
    if flags & FAN_MARK_FLUSH != 0 {
        group.remove_all_marks();
        return Ok(());
    }
    if flags & FAN_MARK_ADD != 0 {
        group.add_mark(
            scope,
            inode,
            dentry,
            mount,
            sb_id,
            event_mask,
            ignored,
            flags & FAN_MARK_ONLYDIR != 0,
        )
    } else if flags & FAN_MARK_REMOVE != 0 {
        group.remove_mark(key, event_mask)
    } else {
        Err(Errno::EINVAL)
    }
}

fn next_group_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
