//! mqueue 伪文件系统：POSIX 消息队列的 `/dev/mqueue` 视图与 fd 语义。
//!
//! 职责划分与 Linux `ipc/mqueue.c` 一致：
//!
//! - 队列本体（消息、属性、通知状态、阻塞等待者）由 `general::ipc::mqueue`
//!   管理；本模块提供 mqueue 文件系统（目录视图 + 打开导航）和 mq fd 的
//!   [`FileOps`]（`read`/`write`/`poll`/epoll）；
//! - `read(2)`/`write(2)` 以优先级 0 收发消息（Linux 语义），`WouldBlock`
//!   交给 syscall 层的 readiness 等待协议；
//! - 队列状态变化通过 [`MqStateObserver`] 发布到各 fd 的 [`PollSource`]，
//!   使 `poll`/`select`/`epoll` 正确唤醒；
//! - 通知（`SIGEV_SIGNAL`/`SIGEV_THREAD`）触发动作由内核注册的
//!   [`MqNotifyDispatcher`] 执行，本模块只负责在消息到达时取出一次性通知。

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;
use sched::Task;
use vfs::cred::Credentials;
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeOps};
use vfs::mount::MountFlags;
use vfs::poll_source::PollSource;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use crate::ipc::mqueue::{
    MqNotification, MqObject, MqRegistry, MqStateObserver, MqAttr,
};

static MQ_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static MQ_REGISTRY: Spinlock<Option<Arc<MqRegistry>>> = Spinlock::new(None);

/// 通知触发动作的分发器；内核在启动期注册（投递信号 / 创建 SIGEV_THREAD 线程）。
pub type MqNotifyDispatcher = fn(&MqNotification);
static MQ_NOTIFY_DISPATCHER: Spinlock<Option<MqNotifyDispatcher>> = Spinlock::new(None);

/// 取全局 mqueue 注册表（syscall 层与伪文件系统共用）。
pub fn mq_registry() -> Arc<MqRegistry> {
    let mut slot = MQ_REGISTRY.lock();
    if let Some(registry) = slot.as_ref() {
        return Arc::clone(registry);
    }
    let registry = Arc::new(MqRegistry::new());
    *slot = Some(Arc::clone(&registry));
    registry
}

/// 注册通知分发器（`SIGEV_SIGNAL`/`SIGEV_THREAD` 的触发动作）。
pub fn register_mq_notify_dispatcher(dispatcher: MqNotifyDispatcher) {
    let mut slot = MQ_NOTIFY_DISPATCHER.lock();
    assert!(slot.is_none(), "mq notify dispatcher 只能注册一次");
    *slot = Some(dispatcher);
}

/// 触发一次队列通知（消息到达且队列从空变非空时由收发路径调用）。
pub fn dispatch_mq_notification(notification: &MqNotification) {
    let slot = MQ_NOTIFY_DISPATCHER.lock();
    if let Some(dispatcher) = slot.as_ref() {
        dispatcher(notification);
    }
}

// ── mq fd 的 FileOps ─────────────────────────────────────────────────────────

struct MqFileShared {
    queue: Arc<MqObject>,
    poll_source: PollSource,
    nonblock: bool,
    /// 防止 fd 与队列互相保活；观察者注册用 Weak。
    _self_weak: Weak<MqFileShared>,
}

impl MqFileShared {
    fn readiness(&self) -> PollEvents {
        let mut ready = PollEvents(0);
        if self.queue.has_messages() {
            ready = ready.with(PollEvents::POLLIN);
        }
        if self.queue.has_space() {
            ready = ready.with(PollEvents::POLLOUT);
        }
        if self.queue.removed() {
            ready = ready.with(PollEvents::POLLHUP);
        }
        ready
    }
}

impl MqStateObserver for MqFileShared {
    fn mq_state_changed(&self) {
        self.poll_source.publish(self.readiness());
    }
}

/// mq fd 的打开文件描述。
pub struct MqFileOps {
    shared: Arc<MqFileShared>,
}

impl MqFileOps {
    pub fn new(queue: Arc<MqObject>, nonblock: bool) -> Self {
        let shared = Arc::new_cyclic(|weak| MqFileShared {
            queue: Arc::clone(&queue),
            poll_source: PollSource::new(PollEvents::default()),
            nonblock,
            _self_weak: weak.clone(),
        });
        let observer_arc: Arc<dyn MqStateObserver> = shared.clone();
        queue.subscribe(Arc::downgrade(&observer_arc));
        Self { shared }
    }

    pub fn queue(&self) -> &Arc<MqObject> {
        &self.shared.queue
    }
}

impl FileOps for MqFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let message = self
            .shared
            .queue
            .try_receive(buf.len(), true)
            .map_err(errno_to_vfs)?;
        let Some(message) = message else {
            return Err(VfsError::WouldBlock);
        };
        let n = message.data.len().min(buf.len());
        buf[..n].copy_from_slice(&message.data[..n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // Linux：write(2) 以优先级 0 发送整段缓冲区。
        let (sent, notify) = self
            .shared
            .queue
            .try_send(0, buf, 0, true)
            .map_err(errno_to_vfs)?;
        if !sent {
            return Err(VfsError::WouldBlock);
        }
        if let Some(notification) = notify {
            dispatch_mq_notification(&notification);
        }
        Ok(buf.len())
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
        let ready = self.shared.readiness();
        self.shared.poll_source.publish(ready);
        ready.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        // 读兴趣挂接收等待队列（消息到达唤醒），写兴趣挂发送等待队列（空间
        // 释放唤醒）。队列状态变化同时经 MqStateObserver 通知 poll_source。
        if interest.has(PollEvents::POLLIN) {
            self.shared.queue.receivers().enqueue(task);
        }
        if interest.has(PollEvents::POLLOUT) {
            self.shared.queue.senders().enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.shared.queue.receivers().remove(task);
        self.shared.queue.senders().remove(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.shared.poll_source)
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
        self.shared.queue.senders().wake_all();
        self.shared.queue.receivers().wake_all();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn errno_to_vfs(error: Errno) -> VfsError {
    match error {
        Errno::EAGAIN => VfsError::WouldBlock,
        Errno::EMSGSIZE => VfsError::MessageTooLong,
        Errno::EINVAL => VfsError::InvalidArgument,
        Errno::EACCES => VfsError::PermissionDenied,
        Errno::ENOENT => VfsError::NotFound,
        Errno::EEXIST => VfsError::AlreadyExists,
        Errno::ENAMETOOLONG => VfsError::NameTooLong,
        Errno::EMFILE => VfsError::TooManyOpenFiles,
        Errno::EPERM => VfsError::OperationNotPermitted,
        _ => VfsError::Io,
    }
}

// ── mqueue 文件系统 ──────────────────────────────────────────────────────────

/// `mqueue` 文件系统驱动（挂载点惯例 `/dev/mqueue`）。
pub struct MqFsDriver;

impl FsDriver for MqFsDriver {
    fn name(&self) -> &'static str {
        "mqueue"
    }
    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV.with(FsDriverFlags::SINGLE)
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let fs_id = FsId::new(MQ_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));
        Ok(Superblock::new(|weak_sb| {
            let root_inode = mk_mq_inode(
                fs_id,
                &weak_sb,
                1,
                FileType::Directory,
                0o777,
                Arc::new(MqRootOps {
                    fs_id,
                    weak_sb: weak_sb.clone(),
                }),
            );
            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
            Superblock {
                fs_type: "mqueue",
                fs_id,
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(MqSuperblockOps),
                self_weak: weak_sb,
            }
        }))
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct MqSuperblockOps;

impl SuperblockOps for MqSuperblockOps {
    fn alloc_inode(&self, _: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn write_inode(&self, _: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }
    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x19800202, // MQUEUE_MAGIC
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
    fn sync_fs(&self, _: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }
    fn remount(&self, _: &Arc<Superblock>, _: MountFlags) -> VfsResult<()> {
        Ok(())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn mk_mq_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    kind: FileType,
    mode: u16,
    ops: Arc<dyn InodeOps + Send + Sync>,
) -> Arc<Inode> {
    Inode::new(
        InodeId { fs_id, ino },
        kind,
        DevId::new(0, 0),
        4096,
        None,
        vfs::inode::InodeMeta {
            size: 0,
            nlink: 1,
            mode: FileMode::new(mode),
            uid: vfs::cred::Uid(0),
            gid: vfs::cred::Gid(0),
            atime: Timespec::now(),
            mtime: Timespec::now(),
            ctime: Timespec::now(),
            blocks: 0,
        },
        ops,
        weak_sb.clone(),
    )
}

/// 根目录：列出全部队列名；`lookup`/`create` 导航到队列文件 inode。
struct MqRootOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl MqRootOps {
    fn queue_inode(&self, name: &str) -> VfsResult<Arc<Inode>> {
        // inode 身份按名字哈希，同一队列的所有路径/打开共享同一 inode。
        let ino = 2 + (stable_name_hash(name) % (u64::MAX - 2));
        Ok(mk_mq_inode(
            self.fs_id,
            &self.weak_sb,
            ino,
            FileType::Regular,
            0o600,
            Arc::new(MqFileInodeOps { name: name.to_string() }),
        ))
    }
}

impl InodeOps for MqRootOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        if name == "." || name == ".." {
            return Err(VfsError::NotFound);
        }
        let registry = mq_registry();
        registry
            .open(name, false, false, None, &Credentials::root())
            .map_err(errno_to_vfs)?;
        self.queue_inode(name)
    }

    fn create(
        &self,
        _: &Inode,
        name: &str,
        _mode: FileMode,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        let registry = mq_registry();
        registry
            .open(name, true, true, None, cred)
            .map_err(errno_to_vfs)?;
        self.queue_inode(name)
    }

    fn open(
        &self,
        _: &Inode,
        options: &OpenOptions,
        cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        // 目录本身以 ProcDirFile 风格的快照列出队列名。
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(64)
            .map_err(|_| VfsError::NoSpace)?;
        for name in mq_registry().names() {
            snapshot.push(DirEntry {
                ino: 2 + stable_name_hash(&name),
                name: SmallStr::new(&name),
                kind: FileType::Regular,
            });
        }
        let _ = options;
        let _ = cred;
        Ok(Box::new(MqDirFile { entries: snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }

    fn unlink(&self, _: &Inode, name: &str, _child: &Inode) -> VfsResult<()> {
        mq_registry().unlink(name).map_err(errno_to_vfs)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 队列文件 inode：`open` 时取出队列并构造 mq fd。
struct MqFileInodeOps {
    name: String,
}

impl InodeOps for MqFileInodeOps {
    fn lookup(&self, _: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _: &Inode,
        options: &OpenOptions,
        cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let registry = mq_registry();
        let queue = registry
            .open(&self.name, false, false, None, cred)
            .map_err(errno_to_vfs)?;
        // 按打开方式校验权限（Linux：mq_open 时按 O_RDONLY/O_WRONLY/O_RDWR）。
        let readable = options.readable();
        let writable = options.writable();
        if !readable && !writable {
            // O_PATH 等非数据访问模式不允许用于队列。
            return Err(VfsError::BadFileDescriptor);
        }
        if readable {
            queue.check_access(false, cred).map_err(errno_to_vfs)?;
        }
        if writable {
            queue.check_access(true, cred).map_err(errno_to_vfs)?;
        }
        Ok(Box::new(MqFileOps::new(queue, options.nonblock)))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 根目录的只读快照视图。
struct MqDirFile {
    entries: Vec<DirEntry>,
}

impl FileOps for MqDirFile {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        let mut index = pos as usize;
        while index < self.entries.len() {
            let entry = self.entries[index].clone();
            index += 1;
            if sink(entry).is_break() {
                break;
            }
        }
        Ok(index as u64)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
    }
    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn stable_name_hash(name: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 以默认属性打开一个 mq（供 syscall 层复用 FileOps 构造）。
pub fn open_mq_fd(queue: Arc<MqObject>, nonblock: bool) -> MqFileOps {
    MqFileOps::new(queue, nonblock)
}
