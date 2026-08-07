//! pidfd 兼容文件对象。
//!
//! pidfd 是 Linux ABI 对稳定进程身份的 fd 投影；这里使用 `Arc<ThreadGroup>`
//! 保持跨 exec 的身份连续性，并把它包装成 VFS `FileOps`。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;
use sched::{PidT, ProcessExitObserver, Task, ThreadGroup};
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::Dentry;
use vfs::error::{VfsError, VfsResult};
use vfs::fdtable::{Fd, FdFlags, FdTable};
use vfs::file::{DirEntry, File, FileOps, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::{Mount, MountFlags};
use vfs::poll_source::PollSource;
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
    shared: Arc<PidfdShared>,
}

struct PidfdShared {
    /// pidfd 代表进程身份，不代表一次 exec 前的具体执行线程。
    group: Arc<ThreadGroup>,
    poll_source: PollSource,
    subscription_id: AtomicU64,
}

impl PidfdFileOps {
    fn new(group: Arc<ThreadGroup>) -> Result<Self, Errno> {
        let initial = if group.is_terminated() {
            PollEvents::POLLIN
        } else {
            PollEvents::default()
        };
        let shared = Arc::new(PidfdShared {
            group: Arc::clone(&group),
            poll_source: PollSource::new(initial),
            subscription_id: AtomicU64::new(0),
        });
        let observer: Arc<dyn ProcessExitObserver> = shared.clone();
        let subscription_id = group
            .try_subscribe_process_exit(Arc::downgrade(&observer))
            .ok_or(Errno::ENOMEM)?;
        shared
            .subscription_id
            .store(subscription_id, Ordering::Release);
        Ok(Self { shared })
    }

    pub fn group(&self) -> Arc<ThreadGroup> {
        Arc::clone(&self.shared.group)
    }
}

impl ProcessExitObserver for PidfdShared {
    fn process_exited(&self) {
        self.poll_source.publish(PollEvents::POLLIN);
    }
}

impl Drop for PidfdShared {
    fn drop(&mut self) {
        let subscription_id = self.subscription_id.load(Ordering::Acquire);
        if subscription_id != 0 {
            self.group.unsubscribe_process_exit(subscription_id);
        }
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
        self.shared.poll_source.snapshot().0.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if !interest.has(PollEvents::POLLIN) {
            return false;
        }
        if self.shared.group.is_terminated() {
            return false;
        }
        self.shared.group.process_exit_waiters().enqueue(task);
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.shared.group.process_exit_waiters().remove(task);
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

pub fn create(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    group: Arc<ThreadGroup>,
    nonblock: bool,
) -> Result<Fd, Errno> {
    let (mount, inode, dentry) = get_or_init_pidfd_fs();
    let file = Arc::new(File::new(
        inode,
        OpenOptions {
            nonblock,
            ..OpenOptions::default()
        },
        cred,
        Box::new(PidfdFileOps::new(group)?),
        dentry,
        Arc::clone(&mount),
    ));
    mount.inc_open();
    fdt.alloc_fd(file, FdFlags::CLOEXEC)
        .map_err(|err| err.to_errno())
}

/// 把 Linux pid 参数解析为稳定进程身份；线程 TID 不得隐式提升为 pidfd。
pub fn group_for_process_pid(pid: PidT, task: &Arc<Task>) -> Result<Arc<ThreadGroup>, Errno> {
    let group = task.thread_group();
    if group.tgid() != pid {
        return Err(Errno::EINVAL);
    }
    Ok(group)
}

pub fn group_from_file(file: &Arc<File>) -> Option<Arc<ThreadGroup>> {
    file.downcast_ops::<PidfdFileOps>().map(PidfdFileOps::group)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::{Arc, Weak};
    use core::sync::atomic::Ordering;

    use errno::Errno;
    use sched::{ArchContextOps, ProcessGroup, SchedParams, Session, Task, TaskState, ThreadGroup};
    use vfs::cred::Credentials;
    use vfs::epoll::{EPOLL_CTL_ADD, EpollEvent};
    use vfs::fdtable::FdTable;
    use vfs::file::{FileOps, PollEvents};

    use super::{PidfdFileOps, group_for_process_pid};

    unsafe fn init_context(
        _ctx: core::ptr::NonNull<u8>,
        _stack_top: usize,
        _entry: sched::KernelEntry,
        _arg: usize,
    ) {
    }

    unsafe extern "C" fn switch_context(
        _prev: core::ptr::NonNull<u8>,
        _next: core::ptr::NonNull<u8>,
        _prev_on_cpu: core::ptr::NonNull<core::sync::atomic::AtomicUsize>,
    ) {
    }

    static TEST_ARCH_CONTEXT_OPS: ArchContextOps = ArchContextOps {
        context_size: 16,
        context_align: 16,
        init_kernel_context: init_context,
        switch_context,
    };

    fn make_task(group: Arc<ThreadGroup>, process_group: Arc<ProcessGroup>) -> Arc<Task> {
        sched::arch_hooks::register(&TEST_ARCH_CONTEXT_OPS);
        Task::new(
            SchedParams::default_fair(),
            Weak::new(),
            group,
            process_group,
        )
    }

    #[test]
    fn pidfd_keeps_thread_group_identity_after_leader_replacement() {
        let session = Session::new();
        let process_group = ProcessGroup::new(&session);
        session.register_group(&process_group);
        let group = ThreadGroup::new();
        let old_leader = make_task(Arc::clone(&group), Arc::clone(&process_group));
        let executor = make_task(Arc::clone(&group), Arc::clone(&process_group));
        group.set_leader(&old_leader);
        let ops = PidfdFileOps::new(Arc::clone(&group)).expect("创建 pidfd ops");

        group.set_leader(&executor);

        assert!(Arc::ptr_eq(&ops.group(), &group));
        assert!(
            group
                .leader()
                .is_some_and(|leader| Arc::ptr_eq(&leader, &executor))
        );
    }

    #[test]
    fn closing_pidfd_removes_process_exit_subscription() {
        let group = ThreadGroup::new();

        for _ in 0..64 {
            let ops = PidfdFileOps::new(Arc::clone(&group)).expect("创建 pidfd ops");
            let subscription = ops.shared.subscription_id.load(Ordering::Acquire);
            drop(ops);

            assert!(
                !group.unsubscribe_process_exit(subscription),
                "pidfd 释放后不得留下退出订阅"
            );
        }
    }

    #[test]
    fn process_pidfd_rejects_nonleader_tid() {
        let session = Session::new();
        let process_group = ProcessGroup::new(&session);
        session.register_group(&process_group);
        let group = ThreadGroup::new();
        let leader = make_task(Arc::clone(&group), Arc::clone(&process_group));
        let thread = make_task(Arc::clone(&group), Arc::clone(&process_group));
        group.set_leader(&leader);
        group.set_tgid(41);

        assert!(matches!(
            group_for_process_pid(42, &thread),
            Err(Errno::EINVAL)
        ));
        let resolved = group_for_process_pid(41, &leader).expect("TGID 应解析为进程身份");
        assert!(Arc::ptr_eq(&resolved, &group));
    }

    #[test]
    fn pidfd_poll_source_publishes_process_exit() {
        let session = Session::new();
        let process_group = ProcessGroup::new(&session);
        session.register_group(&process_group);
        let group = ThreadGroup::new();
        let leader = make_task(Arc::clone(&group), Arc::clone(&process_group));
        group.set_leader(&leader);
        group.add_member(&leader);
        let ops = PidfdFileOps::new(Arc::clone(&group)).expect("创建 pidfd ops");
        let source = ops.poll_source().expect("pidfd 必须提供 epoll 发布源");
        assert!(source.snapshot().0.is_empty());

        assert!(leader.cas_state(TaskState::New, TaskState::Zombie));
        assert!(group.mark_terminated_if_all_members_terminal());

        assert!(source.snapshot().0.has(PollEvents::POLLIN));
    }

    #[test]
    fn pidfd_process_exit_is_visible_through_epoll() {
        let session = Session::new();
        let process_group = ProcessGroup::new(&session);
        session.register_group(&process_group);
        let group = ThreadGroup::new();
        let leader = make_task(Arc::clone(&group), Arc::clone(&process_group));
        group.set_leader(&leader);
        group.add_member(&leader);
        let fdt = FdTable::new_default();
        let cred = Arc::new(Credentials::root());
        let pidfd =
            super::create(&fdt, Arc::clone(&cred), Arc::clone(&group), false).expect("创建 pidfd");
        let epfd = vfs::epoll::create(&fdt, cred, true).expect("创建 epoll");
        vfs::epoll::ctl(
            &fdt,
            epfd,
            EPOLL_CTL_ADD,
            pidfd,
            Some(EpollEvent {
                events: u32::from(PollEvents::POLLIN.0),
                data: 0x7069_6466,
            }),
        )
        .expect("pidfd 应可加入 epoll");

        assert!(leader.cas_state(TaskState::New, TaskState::Zombie));
        assert!(group.mark_terminated_if_all_members_terminal());
        let events = vfs::epoll::wait(&fdt, epfd, 1, 0).expect("读取 pidfd 退出事件");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, 0x7069_6466);
        assert_ne!(events[0].events & u32::from(PollEvents::POLLIN.0), 0);
    }
}
