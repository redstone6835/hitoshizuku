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
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use crate::pid::{PID_INVALID, PidT};
use crate::rlimit::Rlimits;
use crate::signal::SharedSignal;
use crate::sync::Spinlock;
use crate::task::{Task, TaskUsage};

// ── ThreadGroup ─────────────────────────────────────────────────────────────

/// 线程组：共享同一 address space / fd table 的任务集合。
pub struct ThreadGroup {
    /// 稳定 TGID。首次分配 leader pid 后写入；leader 退出/reap 后不改变。
    tgid: AtomicI32,
    /// leader 任务的 Weak。leader 退出时 upgrade 失败，由上层决定是否重选。
    leader: Spinlock<Weak<Task>>,
    /// 成员表。成员任务持 `Arc<ThreadGroup>`，此处用 Weak 避免循环保活。
    members: Spinlock<Vec<Weak<Task>>>,
    /// 线程组共享的信号表（sigaction + shared pending）。
    shared_signal: Arc<SharedSignal>,
    /// 进程级资源限制（per-tg 共享；fork 时复制一份）。
    rlimits: Spinlock<Rlimits>,
    /// 已退出线程的 usage 累计，供进程记账在最后一个线程退出时汇总。
    exited_usage: Spinlock<TaskUsage>,
    /// 尚未执行退出清理的成员数，保证最后一个线程汇总时其余 usage 已经入账。
    acct_live_members: AtomicUsize,
    /// 防止并发退出路径重复输出同一条 acct 记录。
    acct_emitted: AtomicBool,
}

impl ThreadGroup {
    /// 创建一个空的线程组，自带新的 SharedSignal。
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tgid: AtomicI32::new(PID_INVALID),
            leader: Spinlock::new(Weak::new()),
            members: Spinlock::new(Vec::new()),
            shared_signal: Arc::new(SharedSignal::new()),
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
            exited_usage: Spinlock::new(TaskUsage::default()),
            acct_live_members: AtomicUsize::new(0),
            acct_emitted: AtomicBool::new(false),
        })
    }

    /// 创建一个线程组并共享给定的 SharedSignal（CLONE_SIGHAND 语义）。
    pub fn new_sharing_signal(shared: Arc<SharedSignal>) -> Arc<Self> {
        Arc::new(Self {
            tgid: AtomicI32::new(PID_INVALID),
            leader: Spinlock::new(Weak::new()),
            members: Spinlock::new(Vec::new()),
            shared_signal: shared,
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
            exited_usage: Spinlock::new(TaskUsage::default()),
            acct_live_members: AtomicUsize::new(0),
            acct_emitted: AtomicBool::new(false),
        })
    }

    pub fn set_leader(&self, leader: &Arc<Task>) {
        if let Some(pid) = leader.pid_root() {
            self.set_tgid(pid);
        }
        *self.leader.lock() = Arc::downgrade(leader);
    }

    pub fn leader(&self) -> Option<Arc<Task>> {
        self.leader.lock().upgrade()
    }

    pub fn add_member(&self, task: &Arc<Task>) {
        if self.tgid() == PID_INVALID {
            if let Some(leader) = self.leader() {
                if let Some(pid) = leader.pid_root() {
                    self.set_tgid(pid);
                }
            } else if let Some(pid) = task.pid_root() {
                self.set_tgid(pid);
            }
        }
        self.members.lock().push(Arc::downgrade(task));
        self.acct_live_members.fetch_add(1, Ordering::Release);
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

    pub fn tgid(&self) -> PidT {
        self.tgid.load(Ordering::Acquire)
    }

    /// 计入一个成员的最终 usage；返回 true 表示它是最后完成退出清理的成员。
    pub fn account_member_exit(&self, usage: TaskUsage) -> bool {
        self.exited_usage.lock().add_assign(usage);
        match self
            .acct_live_members
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |members| {
                members.checked_sub(1)
            }) {
            Ok(previous) => previous == 1,
            Err(_) => {
                debug_assert!(false, "thread-group accounting member underflow");
                false
            }
        }
    }

    /// 撤销尚未启动成功的成员，不把它计入进程退出 usage。
    pub fn cancel_member_accounting(&self) {
        let _ =
            self.acct_live_members
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |members| {
                    members.checked_sub(1)
                });
    }

    pub fn exited_usage_snapshot(&self) -> TaskUsage {
        *self.exited_usage.lock()
    }

    pub fn try_claim_acct_record(&self) -> bool {
        self.acct_emitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn set_tgid(&self, pid: PidT) {
        if pid <= PID_INVALID {
            return;
        }
        match self
            .tgid
            .compare_exchange(PID_INVALID, pid, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            Err(prev) => {
                debug_assert_eq!(prev, pid, "[sched][group] TGID changed after publication");
            }
        }
    }
}

impl Default for ThreadGroup {
    fn default() -> Self {
        Self {
            tgid: AtomicI32::new(PID_INVALID),
            leader: Spinlock::new(Weak::new()),
            members: Spinlock::new(Vec::new()),
            shared_signal: Arc::new(SharedSignal::new()),
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
            exited_usage: Spinlock::new(TaskUsage::default()),
            acct_live_members: AtomicUsize::new(0),
            acct_emitted: AtomicBool::new(false),
        }
    }
}

// ── ProcessGroup ────────────────────────────────────────────────────────────

/// 进程组：作业控制的边界。
pub struct ProcessGroup {
    /// 稳定 PGID。首次成为一个有 pid 成员的进程组时写入。
    pgid: AtomicI32,
    session: Spinlock<Arc<Session>>,
    members: Spinlock<Vec<Weak<Task>>>,
}

impl ProcessGroup {
    pub fn new(session: &Arc<Session>) -> Arc<Self> {
        Arc::new(Self {
            pgid: AtomicI32::new(PID_INVALID),
            session: Spinlock::new(Arc::clone(session)),
            members: Spinlock::new(Vec::new()),
        })
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        Some(Arc::clone(&self.session.lock()))
    }

    pub fn add_member(&self, task: &Arc<Task>) {
        if self.pgid() == PID_INVALID {
            if let Some(pid) = task.pid_root() {
                self.set_pgid(pid);
            }
        }
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

    pub fn pgid(&self) -> PidT {
        self.pgid.load(Ordering::Acquire)
    }

    pub fn set_pgid(&self, pid: PidT) {
        if pid <= PID_INVALID {
            return;
        }
        match self
            .pgid
            .compare_exchange(PID_INVALID, pid, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            Err(prev) => {
                debug_assert_eq!(prev, pid, "[sched][group] PGID changed after publication");
            }
        }
    }
}

// ── Session ─────────────────────────────────────────────────────────────────

/// 登录会话。控制终端字段后续再接入 TTY 子系统。
pub struct Session {
    /// 稳定 SID。首次设置有 pid 的 session leader 时写入。
    sid: AtomicI32,
    leader: Spinlock<Weak<Task>>,
    groups: Spinlock<Vec<Weak<ProcessGroup>>>,
}

impl Session {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sid: AtomicI32::new(PID_INVALID),
            leader: Spinlock::new(Weak::new()),
            groups: Spinlock::new(Vec::new()),
        })
    }

    pub fn set_leader(&self, leader: &Arc<Task>) {
        if let Some(pid) = leader.pid_root() {
            self.set_sid(pid);
        }
        *self.leader.lock() = Arc::downgrade(leader);
    }

    pub fn leader(&self) -> Option<Arc<Task>> {
        self.leader.lock().upgrade()
    }

    pub fn register_group(&self, pg: &Arc<ProcessGroup>) {
        let mut groups = self.groups.lock();
        groups.retain(|w| w.upgrade().is_some());
        if groups.iter().any(|w| {
            w.upgrade()
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, pg))
        }) {
            return;
        }
        groups.push(Arc::downgrade(pg));
    }

    pub fn snapshot_groups(&self) -> Vec<Arc<ProcessGroup>> {
        let mut groups = self.groups.lock();
        groups.retain(|w| w.upgrade().is_some());
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

    pub fn sid(&self) -> PidT {
        self.sid.load(Ordering::Acquire)
    }

    pub fn set_sid(&self, pid: PidT) {
        if pid <= PID_INVALID {
            return;
        }
        match self
            .sid
            .compare_exchange(PID_INVALID, pid, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            Err(prev) => {
                debug_assert_eq!(prev, pid, "[sched][group] SID changed after publication");
            }
        }
    }
}
