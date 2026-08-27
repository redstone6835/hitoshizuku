//! 内核侧命名空间集成。
//!
//! - [`NsProxy`]：任务的命名空间引用集（uts/ipc/time/cgroup + pid），经
//!   `TASKEXT_NS` 挂载；
//! - [`IpcNamespace`]：SysV IPC 对象按命名空间隔离（shm/sem/msg 各自独立
//!   管理器）；
//! - `unshare(2)`/`setns(2)` 的语义实现；
//! - 启动期为 init 建立根命名空间。

use alloc::sync::Arc;

use errno::Errno;
use general::ipc::msg::MsgManager;
use general::ipc::sem::SemManager;
use general::ipc::shm::ShmManager;
use sched::Task;
use sched::sync::Spinlock;

/// SysV IPC 命名空间：三个管理器各一份。
pub struct IpcNamespace {
    inum: u64,
    pub shm: Arc<ShmManager>,
    pub sem: Arc<SemManager>,
    pub msg: Arc<MsgManager>,
}

impl IpcNamespace {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inum: ns::allocate_ns_inum(),
            shm: Arc::new(ShmManager::default()),
            sem: Arc::new(SemManager::default()),
            msg: Arc::new(MsgManager::default()),
        })
    }
}

impl ns::Namespace for IpcNamespace {
    fn ns_type(&self) -> ns::NsType {
        ns::NsType::Ipc
    }

    fn inum(&self) -> u64 {
        self.inum
    }
}

/// 任务的命名空间引用集。
pub struct NsProxy {
    pub uts: Arc<ns::UtsNamespace>,
    pub ipc: Arc<IpcNamespace>,
    pub time: Arc<ns::TimeNamespace>,
    pub cgroup: Arc<ns::CgroupNamespace>,
    pub pid: Arc<sched::pid::PidNamespace>,
    /// `setns(CLONE_NEWPID)`/`unshare(CLONE_NEWPID)` 的待生效 pid 命名空间
    /// （Linux 语义：调用者自身不迁移，下一次 `fork` 的子进程进入）。
    pub pending_pid: Spinlock<Option<Arc<sched::pid::PidNamespace>>>,
}

impl NsProxy {
    pub fn root() -> Arc<Self> {
        Arc::new(Self {
            uts: ns::UtsNamespace::new(b"mygo", b"localdomain"),
            ipc: IpcNamespace::new(),
            time: ns::TimeNamespace::new(),
            cgroup: ns::CgroupNamespace::new(),
            pid: sched::root_pid_ns(),
            pending_pid: Spinlock::new(None),
        })
    }
}

impl ns::Namespace for NsProxy {
    fn ns_type(&self) -> ns::NsType {
        ns::NsType::Uts
    }

    fn inum(&self) -> u64 {
        self.uts.inum()
    }
}

/// `TASKEXT_NS` 键：任务命名空间引用集（`Arc<NsProxy>`）。
pub const TASKEXT_NS: sched::TaskExtKey = 0x0004_0003;

/// 取任务的命名空间引用集（无则返回根引用，不挂载）。
pub fn task_ns(task: &Arc<Task>) -> Arc<NsProxy> {
    task.ext_lookup(TASKEXT_NS)
        .and_then(|payload| payload.downcast::<NsProxy>().ok())
        .unwrap_or_else(NsProxy::root)
}

/// 取当前任务的命名空间引用集（惰性挂载）。
pub fn current_ns() -> Arc<NsProxy> {
    let me = sched::current_task_direct();
    if let Some(ns) = me
        .ext_lookup(TASKEXT_NS)
        .and_then(|payload| payload.downcast::<NsProxy>().ok())
    {
        return ns;
    }
    let ns = NsProxy::root();
    let erased: Arc<dyn core::any::Any + Send + Sync> = ns.clone();
    me.ext_install(TASKEXT_NS, erased);
    ns
}

/// `unshare(2)`：按 flags 创建并切换命名空间。
///
/// Linux 语义：
/// - 所有 flags 需要 `CAP_SYS_ADMIN`（用户命名空间简化模型：全局能力）；
/// - `CLONE_NEWPID` 只影响**之后 fork 的子进程**（调用者留在原 pid ns）；
/// - `CLONE_NEWTIME` 仅允许与 unshare 一起使用（本函数即 unshare）。
pub fn unshare(task: &Arc<Task>, flags: u64) -> Result<(), Errno> {
    const CLONE_NEWNS: u64 = 0x0002_0000;
    const CLONE_NEWCGROUP: u64 = 0x0200_0000;
    const CLONE_NEWUTS: u64 = 0x0400_0000;
    const CLONE_NEWIPC: u64 = 0x0800_0000;
    const CLONE_NEWUSER: u64 = 0x1000_0000;
    const CLONE_NEWPID: u64 = 0x2000_0000;
    const CLONE_NEWNET: u64 = 0x4000_0000;
    const CLONE_NEWTIME: u64 = 0x0000_0080;
    const SUPPORTED: u64 =
        CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID | CLONE_NEWTIME;
    const UNSUPPORTED: u64 = CLONE_NEWUSER | CLONE_NEWNET;

    if flags & UNSUPPORTED != 0 {
        // 用户/网络命名空间未实现（需要全局能力语义改造 / 网络栈 per-ns 化）。
        return Err(Errno::EOPNOTSUPP);
    }
    if flags == 0 {
        return Ok(());
    }
    if flags & !(SUPPORTED | UNSUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    if !task.credentials().has_cap(sched::ids::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    if flags & CLONE_NEWTIME != 0 && flags & (CLONE_NEWNS | CLONE_NEWPID) != 0 {
        // Linux：CLONE_NEWTIME 与 CLONE_NEWNS/CLONE_NEWPID 组合受限
        // （时间偏移语义与挂载/pid 迁移冲突）。
        return Err(Errno::EINVAL);
    }

    let ns = task_ns(task);
    if flags & CLONE_NEWNS != 0 {
        let vfs_ctx = task
            .ext_lookup(sched::TASKEXT_VFS_CONTEXT)
            .and_then(|payload| payload.downcast::<general::vfs::VfsContext>().ok())
            .ok_or(Errno::EINVAL)?;
        let forked = vfs_ctx.clone_with_new_ns().map_err(|e| e.to_errno())?;
        let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(forked);
        task.ext_install(sched::TASKEXT_VFS_CONTEXT, erased);
    }
    if flags & CLONE_NEWUTS != 0 {
        let new_uts = ns::UtsNamespace::new(&ns.uts.hostname(), &ns.uts.domainname());
        let mut proxy = (*ns).clone_proxy();
        proxy.uts = new_uts;
        install_proxy(task, proxy);
    }
    if flags & CLONE_NEWIPC != 0 {
        let mut proxy = (*ns).clone_proxy();
        proxy.ipc = IpcNamespace::new();
        install_proxy(task, proxy);
    }
    if flags & CLONE_NEWTIME != 0 {
        let mut proxy = (*ns).clone_proxy();
        proxy.time = ns::TimeNamespace::new();
        install_proxy(task, proxy);
    }
    if flags & CLONE_NEWCGROUP != 0 {
        let mut proxy = (*ns).clone_proxy();
        proxy.cgroup = ns::CgroupNamespace::new();
        install_proxy(task, proxy);
    }
    if flags & CLONE_NEWPID != 0 {
        *ns.pending_pid.lock() = Some(sched::pid::PidNamespace::new_child(&ns.pid));
    }
    Ok(())
}

/// `setns(fd, nstype)`：加入 `fd` 指向的命名空间。
///
/// `nstype == 0` 时按命名空间自身类型；`CLONE_NEWPID` 同样只对后续子进程
/// 生效。要求 `CAP_SYS_ADMIN`。
pub fn setns(
    task: &Arc<Task>,
    namespace: Arc<dyn ns::Namespace>,
    nstype: u64,
) -> Result<(), Errno> {
    if nstype != 0 && nstype != namespace.ns_type() as u64 {
        return Err(Errno::EINVAL);
    }
    if !task.credentials().has_cap(sched::ids::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let ns = task_ns(task);
    match namespace.ns_type() {
        ns::NsType::Mount => {
            let mount_ns =
                ns::downcast_arc::<general::vfs::MountNamespace>(namespace).ok_or(Errno::EINVAL)?;
            let vfs_ctx = task
                .ext_lookup(sched::TASKEXT_VFS_CONTEXT)
                .and_then(|payload| payload.downcast::<general::vfs::VfsContext>().ok())
                .ok_or(Errno::EINVAL)?;
            let joined = vfs_ctx.with_mount_ns(mount_ns).map_err(|e| e.to_errno())?;
            let erased: Arc<dyn core::any::Any + Send + Sync> = joined;
            task.ext_install(sched::TASKEXT_VFS_CONTEXT, erased);
        }
        ns::NsType::Uts => {
            let uts = ns::downcast_arc::<ns::UtsNamespace>(namespace).ok_or(Errno::EINVAL)?;
            let mut proxy = (*ns).clone_proxy();
            proxy.uts = uts;
            install_proxy(task, proxy);
        }
        ns::NsType::Ipc => {
            let ipc = ns::downcast_arc::<IpcNamespace>(namespace).ok_or(Errno::EINVAL)?;
            let mut proxy = (*ns).clone_proxy();
            proxy.ipc = ipc;
            install_proxy(task, proxy);
        }
        ns::NsType::Time => {
            let time = ns::downcast_arc::<ns::TimeNamespace>(namespace).ok_or(Errno::EINVAL)?;
            let mut proxy = (*ns).clone_proxy();
            proxy.time = time;
            install_proxy(task, proxy);
        }
        ns::NsType::Cgroup => {
            let cgroup = ns::downcast_arc::<ns::CgroupNamespace>(namespace).ok_or(Errno::EINVAL)?;
            let mut proxy = (*ns).clone_proxy();
            proxy.cgroup = cgroup;
            install_proxy(task, proxy);
        }
        ns::NsType::Pid => {
            let pid =
                ns::downcast_arc::<sched::pid::PidNamespace>(namespace).ok_or(Errno::EINVAL)?;
            *ns.pending_pid.lock() = Some(pid);
        }
        ns::NsType::User | ns::NsType::Net => return Err(Errno::EOPNOTSUPP),
    }
    Ok(())
}

impl NsProxy {
    fn clone_proxy(&self) -> NsProxy {
        NsProxy {
            uts: Arc::clone(&self.uts),
            ipc: Arc::clone(&self.ipc),
            time: Arc::clone(&self.time),
            cgroup: Arc::clone(&self.cgroup),
            pid: Arc::clone(&self.pid),
            pending_pid: Spinlock::new(None),
        }
    }
}

fn install_proxy(task: &Arc<Task>, proxy: NsProxy) {
    let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(proxy);
    // 通用 ext 的 install 不允许同 key 重复挂载（debug 断言 + lookup 取首项），
    // 命名空间切换必须先把旧 proxy 摘掉再装新的。
    let _ = task.ext_remove(TASKEXT_NS);
    task.ext_install(TASKEXT_NS, erased);
}

/// 取子进程应使用的 pid 命名空间（`pending_pid` 消费一次）。
pub fn child_pid_namespace(parent: &Arc<Task>) -> Arc<sched::pid::PidNamespace> {
    let ns = task_ns(parent);
    if let Some(pending) = ns.pending_pid.lock().take() {
        return pending;
    }
    Arc::clone(&ns.pid)
}
