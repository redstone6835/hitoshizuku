//! 线程组、进程组、会话。
//!
//! 这三个概念在 Linux 内核里分别对应 TGID / PGID / SID，在**无 PID**的模型里
//! 它们全部变成独立的 `Arc<...>` 对象，`Task` 持有各自的强引用。组成员索引
//! 统一使用 `Weak<Task>`：成员死亡时 upgrade 失败，不影响组对象本身；组对象
//! 由仍活着的成员或上层（会话 leader、shell）保活。
//!
//! ## ThreadGroup（线程组）
//!
//! 对应 `CLONE_THREAD`：同一程序的多个执行流共享地址空间、fd 表、信号处理等，
//! 但各自有独立的调度实体。`getpid()` 返回的实际是 `tgid`，即 leader 的 tid。
//!
//! ## ProcessGroup（进程组）
//!
//! `setpgid` 的作用域。主要用于作业控制（`kill -PGID`、Ctrl-C 给前台组）。
//! 一个 `ProcessGroup` 通常包含多个独立 `ThreadGroup` 的 leader。
//!
//! ## Session（会话）
//!
//! 一个登录会话。包含若干进程组，最多一个前台组，可关联一个控制终端
//! （此处只预留字段）。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::rlimit::Rlimits;
use crate::signal::SharedSignal;
use crate::sync::Spinlock;
use crate::task::Task;

// ── ThreadGroup ─────────────────────────────────────────────────────────────

/// 线程组：共享同一 address space / fd table 的任务集合。
pub struct ThreadGroup {
    /// leader 任务的 Weak。leader 退出时 upgrade 失败，由上层决定是否重选。
    leader: Spinlock<Weak<Task>>,
    /// 成员表。成员任务持 `Arc<ThreadGroup>`，此处用 Weak 避免循环保活。
    members: Spinlock<Vec<Weak<Task>>>,
    /// 线程组共享的信号表（sigaction + shared pending）。
    shared_signal: Arc<SharedSignal>,
    /// 进程级资源限制（per-tg 共享；fork 时复制一份）。
    rlimits: Spinlock<Rlimits>,
}

impl ThreadGroup {
    /// 创建一个空的线程组，自带新的 SharedSignal。
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            leader: Spinlock::new(Weak::new()),
            members: Spinlock::new(Vec::new()),
            shared_signal: Arc::new(SharedSignal::new()),
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
        })
    }

    /// 创建一个线程组并共享给定的 SharedSignal（CLONE_SIGHAND 语义）。
    pub fn new_sharing_signal(shared: Arc<SharedSignal>) -> Arc<Self> {
        Arc::new(Self {
            leader: Spinlock::new(Weak::new()),
            members: Spinlock::new(Vec::new()),
            shared_signal: shared,
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
        })
    }

    pub fn set_leader(&self, leader: &Arc<Task>) {
        *self.leader.lock() = Arc::downgrade(leader);
    }

    pub fn leader(&self) -> Option<Arc<Task>> {
        self.leader.lock().upgrade()
    }

    pub fn add_member(&self, task: &Arc<Task>) {
        self.members.lock().push(Arc::downgrade(task));
    }

    /// 移除一个成员，同时顺带清理已死的 Weak。
    pub fn remove_member(&self, task: &Arc<Task>) {
        let mut members = self.members.lock();
        members.retain(|w| match w.upgrade() {
            Some(t) => !Arc::ptr_eq(&t, task),
            None => false,
        });
    }

    /// 列出当前线程组的活成员快照。
    pub fn snapshot(&self) -> Vec<Arc<Task>> {
        let members = self.members.lock();
        members.iter().filter_map(|w| w.upgrade()).collect()
    }

    pub fn shared_signal(&self) -> &Arc<SharedSignal> {
        &self.shared_signal
    }

    /// 访问 rlimit 表。锁顺序与 `shared_signal`/`members` 平行，不嵌套。
    pub fn rlimits(&self) -> &Spinlock<Rlimits> {
        &self.rlimits
    }
}

impl Default for ThreadGroup {
    fn default() -> Self {
        Self {
            leader: Spinlock::new(Weak::new()),
            members: Spinlock::new(Vec::new()),
            shared_signal: Arc::new(SharedSignal::new()),
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
        }
    }
}

// ── ProcessGroup ────────────────────────────────────────────────────────────

/// 进程组：作业控制的边界。
pub struct ProcessGroup {
    session: Spinlock<Arc<Session>>,
    members: Spinlock<Vec<Weak<Task>>>,
}

impl ProcessGroup {
    pub fn new(session: &Arc<Session>) -> Arc<Self> {
        Arc::new(Self {
            session: Spinlock::new(Arc::clone(session)),
            members: Spinlock::new(Vec::new()),
        })
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        Some(Arc::clone(&self.session.lock()))
    }

    pub fn add_member(&self, task: &Arc<Task>) {
        self.members.lock().push(Arc::downgrade(task));
    }

    pub fn remove_member(&self, task: &Arc<Task>) {
        let mut members = self.members.lock();
        members.retain(|w| match w.upgrade() {
            Some(t) => !Arc::ptr_eq(&t, task),
            None => false,
        });
    }

    pub fn snapshot(&self) -> Vec<Arc<Task>> {
        let members = self.members.lock();
        members.iter().filter_map(|w| w.upgrade()).collect()
    }

    /// 重新绑定本进程组到另一个 session。`setsid` / shell 重新归属时使用。
    pub fn set_session(&self, session: &Arc<Session>) {
        *self.session.lock() = Arc::clone(session);
    }
}

// ── Session ─────────────────────────────────────────────────────────────────

/// 登录会话。控制终端字段后续再接入 TTY 子系统。
pub struct Session {
    leader: Spinlock<Weak<Task>>,
    groups: Spinlock<Vec<Weak<ProcessGroup>>>,
}

impl Session {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            leader: Spinlock::new(Weak::new()),
            groups: Spinlock::new(Vec::new()),
        })
    }

    pub fn set_leader(&self, leader: &Arc<Task>) {
        *self.leader.lock() = Arc::downgrade(leader);
    }

    pub fn leader(&self) -> Option<Arc<Task>> {
        self.leader.lock().upgrade()
    }

    pub fn register_group(&self, pg: &Arc<ProcessGroup>) {
        self.groups.lock().push(Arc::downgrade(pg));
    }

    pub fn snapshot_groups(&self) -> Vec<Arc<ProcessGroup>> {
        let groups = self.groups.lock();
        groups.iter().filter_map(|w| w.upgrade()).collect()
    }

    /// 把某个 ProcessGroup 从 session 中摘掉（也清理失效 Weak）。
    pub fn remove_group(&self, pg: &Arc<ProcessGroup>) {
        let mut groups = self.groups.lock();
        groups.retain(|w| match w.upgrade() {
            Some(g) => !Arc::ptr_eq(&g, pg),
            None => false,
        });
    }
}
