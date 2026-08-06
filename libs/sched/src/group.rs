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
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use native_abi::{ExecPhase, UserAbiKind};

use crate::pid::{PID_INVALID, PidT};
use crate::rlimit::Rlimits;
use crate::signal::{SharedSignal, SignalNumber};
use crate::sync::{Spinlock, SpinlockGuard};
use crate::task::{Task, TaskUsage};

// ── ThreadGroup ─────────────────────────────────────────────────────────────

const GROUP_EXIT_PRESENT: u64 = 1 << 63;
const GROUP_EXIT_SIGNALED: u64 = 1 << 62;
const GROUP_EXIT_CORE_DUMPED: u64 = 1 << 61;

/// 线程组权威的用户态 personality。
///
/// Native payload 由上层内核定义，sched 只负责其进程级所有权与原子发布。
#[derive(Clone)]
pub enum ProcessPersonalityState {
    TomoriLinux,
    MygoNative(Arc<dyn Any + Send + Sync>),
}

impl ProcessPersonalityState {
    pub fn user_abi_kind(&self) -> UserAbiKind {
        match self {
            Self::TomoriLinux => UserAbiKind::TomoriLinux,
            Self::MygoNative(_) => UserAbiKind::MygoNative,
        }
    }
}

/// 线程组整体退出的权威状态。
///
/// 该状态只能由第一个 `exit_group` / 致命组信号请求发布，
/// 后续成员均使用它退出，父进程也以它生成 wait status。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupExitStatus {
    Exited(i32),
    Signaled {
        signal: SignalNumber,
        core_dumped: bool,
    },
}

impl GroupExitStatus {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Exited(code) => code,
            Self::Signaled { signal, .. } => signal.raw() as i32,
        }
    }

    fn encode(self) -> u64 {
        match self {
            Self::Exited(code) => GROUP_EXIT_PRESENT | u64::from(code as u32),
            Self::Signaled {
                signal,
                core_dumped,
            } => {
                GROUP_EXIT_PRESENT
                    | GROUP_EXIT_SIGNALED
                    | if core_dumped {
                        GROUP_EXIT_CORE_DUMPED
                    } else {
                        0
                    }
                    | u64::from(signal.raw() as u32)
            }
        }
    }

    fn decode(encoded: u64) -> Option<Self> {
        if encoded & GROUP_EXIT_PRESENT == 0 {
            return None;
        }
        if encoded & GROUP_EXIT_SIGNALED == 0 {
            return Some(Self::Exited((encoded as u32) as i32));
        }
        let signal = SignalNumber::from_raw((encoded as u32) as i32)?;
        Some(Self::Signaled {
            signal,
            core_dumped: encoded & GROUP_EXIT_CORE_DUMPED != 0,
        })
    }
}

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
    /// 线程组已开始整体退出；置位后不再接纳新的 CLONE_THREAD 成员。
    closing: AtomicBool,
    /// 所有已接纳成员均已进入 Zombie/Dead，leader 此后才可被父进程回收。
    terminated: AtomicBool,
    /// 协作式组退出请求；编码同时保存普通退出或信号退出原因。
    group_exit: AtomicU64,
    /// 串行化同一线程组的 exec prepare/revalidate/commit。
    exec_lock: Spinlock<()>,
    /// 每次成功安装新映像后推进，供 prepare 阶段做乐观快照。
    exec_generation: AtomicU64,
    /// 信号 consumer 与 exec 提交路径共享的阶段判定。
    exec_phase: AtomicU8,
    /// 把 consumer 的 phase 检查与实际出队组成同一个临界区。
    signal_consumer_lock: Spinlock<()>,
    /// 线程组权威 personality 的无锁 discriminator。
    user_abi_kind: AtomicU8,
    /// 进程级 personality payload；Tomori 默认态不分配 Native 状态。
    personality: Spinlock<ProcessPersonalityState>,
}

/// 持有线程组 exec 锁时可修改的状态视图。
pub struct ThreadGroupExecGuard<'a> {
    group: &'a ThreadGroup,
    _lock: SpinlockGuard<'a, ()>,
}

/// signal consumer 的短临界区；producer 不取得此锁。
pub(crate) struct SignalConsumerGuard<'a> {
    _lock: SpinlockGuard<'a, ()>,
}

impl ThreadGroupExecGuard<'_> {
    pub fn phase(&self) -> ExecPhase {
        self.group.exec_phase()
    }

    pub fn set_phase(&mut self, phase: ExecPhase) {
        let _consumer = self.group.signal_consumer_lock.lock();
        self.group.exec_phase.store(phase as u8, Ordering::Release);
    }

    pub fn generation(&self) -> u64 {
        self.group.exec_generation()
    }

    pub fn advance_generation(&mut self) -> u64 {
        let next = self
            .generation()
            .checked_add(1)
            .expect("线程组 exec generation 已耗尽");
        self.group.exec_generation.store(next, Ordering::Release);
        next
    }

    pub fn install_personality(&mut self, personality: ProcessPersonalityState) {
        let kind = personality.user_abi_kind();
        let members = self.group.members.lock();
        *self.group.personality.lock() = personality;
        self.group
            .user_abi_kind
            .store(kind as u8, Ordering::Release);
        for member in members.iter().filter_map(Weak::upgrade) {
            member.publish_user_abi_kind(kind);
        }
    }
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
            closing: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            group_exit: AtomicU64::new(0),
            exec_lock: Spinlock::new(()),
            exec_generation: AtomicU64::new(0),
            exec_phase: AtomicU8::new(ExecPhase::Running as u8),
            signal_consumer_lock: Spinlock::new(()),
            user_abi_kind: AtomicU8::new(UserAbiKind::TomoriLinux as u8),
            personality: Spinlock::new(ProcessPersonalityState::TomoriLinux),
        })
    }

    /// 创建一个线程组并安装给定的线程组信号状态。
    ///
    /// `CLONE_SIGHAND` 调用方应传入通过 `clone_sighand()` 构造的新状态，
    /// 仅让其中的 handler 表共享；不能直接复用父线程组 pending 队列。
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
            closing: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            group_exit: AtomicU64::new(0),
            exec_lock: Spinlock::new(()),
            exec_generation: AtomicU64::new(0),
            exec_phase: AtomicU8::new(ExecPhase::Running as u8),
            signal_consumer_lock: Spinlock::new(()),
            user_abi_kind: AtomicU8::new(UserAbiKind::TomoriLinux as u8),
            personality: Spinlock::new(ProcessPersonalityState::TomoriLinux),
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

    /// 尝试接纳一个成员。整体退出与成员登记通过 `members` 锁排序：
    ///
    /// - 若登记先完成，随后退出路径的 snapshot 必然包含该成员；
    /// - 若退出先发布，登记会被拒绝，避免 leader 可回收后又出现新线程。
    pub fn try_add_member(&self, task: &Arc<Task>) -> bool {
        if self.tgid() == PID_INVALID {
            if let Some(leader) = self.leader() {
                if let Some(pid) = leader.pid_root() {
                    self.set_tgid(pid);
                }
            } else if let Some(pid) = task.pid_root() {
                self.set_tgid(pid);
            }
        }
        let mut members = self.members.lock();
        if self.closing.load(Ordering::Acquire) || self.terminated.load(Ordering::Acquire) {
            return false;
        }
        task.publish_user_abi_kind(self.user_abi_kind());
        members.push(Arc::downgrade(task));
        self.acct_live_members.fetch_add(1, Ordering::Release);
        true
    }

    pub fn add_member(&self, task: &Arc<Task>) {
        assert!(
            self.try_add_member(task),
            "[sched][group] adding member to closing thread group"
        );
    }

    /// 移除一个成员，同时顺带清理已死的 Weak。
    pub fn remove_member(&self, task: &Arc<Task>) -> bool {
        let mut members = self.members.lock();
        let mut removed = false;
        members.retain(|w| match w.upgrade() {
            Some(t) if Arc::ptr_eq(&t, task) => {
                removed = true;
                false
            }
            Some(_) => true,
            None => false,
        });
        removed
    }

    /// 列出当前线程组的活成员快照。
    pub fn snapshot(&self) -> Vec<Arc<Task>> {
        let members = self.members.lock();
        members.iter().filter_map(|w| w.upgrade()).collect()
    }

    pub fn shared_signal(&self) -> &Arc<SharedSignal> {
        &self.shared_signal
    }

    pub fn lock_exec(&self) -> ThreadGroupExecGuard<'_> {
        ThreadGroupExecGuard {
            group: self,
            _lock: self.exec_lock.lock(),
        }
    }

    pub fn exec_generation(&self) -> u64 {
        self.exec_generation.load(Ordering::Acquire)
    }

    pub fn exec_phase(&self) -> ExecPhase {
        ExecPhase::from_raw(self.exec_phase.load(Ordering::Acquire))
    }

    pub fn user_abi_kind(&self) -> UserAbiKind {
        UserAbiKind::from_raw(self.user_abi_kind.load(Ordering::Acquire))
    }

    pub fn native_personality_payload(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        match &*self.personality.lock() {
            ProcessPersonalityState::TomoriLinux => None,
            ProcessPersonalityState::MygoNative(payload) => Some(Arc::clone(payload)),
        }
    }

    pub(crate) fn lock_signal_consumer(&self) -> Option<SignalConsumerGuard<'_>> {
        let lock = self.signal_consumer_lock.lock();
        (self.exec_phase() == ExecPhase::Running).then_some(SignalConsumerGuard { _lock: lock })
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

    /// 禁止新成员加入；调用方随后必须在成员锁之后取得 snapshot 并逐一唤醒。
    fn begin_group_exit(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// 发布线程组退出请求。并发请求遵循 first-wins。
    fn request_group_status(&self, status: GroupExitStatus) -> GroupExitStatus {
        let encoded = status.encode();
        self.begin_group_exit();
        let selected =
            match self
                .group_exit
                .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => encoded,
                Err(existing) => existing,
            };
        GroupExitStatus::decode(selected).expect("published group-exit status must decode")
    }

    /// 发布 `exit_group` 退出码并返回最终采用的退出码。
    pub fn request_group_exit(&self, code: i32) -> i32 {
        self.request_group_status(GroupExitStatus::Exited(code))
            .exit_code()
    }

    /// 发布致命信号退出原因并返回最终采用的组状态。
    pub fn request_group_signal(&self, signal: SignalNumber, core_dumped: bool) -> GroupExitStatus {
        self.request_group_status(GroupExitStatus::Signaled {
            signal,
            core_dumped,
        })
    }

    /// 返回已经发布的线程组退出状态。
    pub fn group_exit_status(&self) -> Option<GroupExitStatus> {
        GroupExitStatus::decode(self.group_exit.load(Ordering::Acquire))
    }

    /// 返回已经发布的线程组退出码。
    pub fn group_exit_code(&self) -> Option<i32> {
        self.group_exit_status().map(GroupExitStatus::exit_code)
    }

    /// 在成员完成各自的退出前清理后，尝试发布“线程组已经完全终止”。
    ///
    /// 该判断与成员加入共用 `members` 锁，因此一旦成功便不会再有新成员出现。
    /// leader 可能较早进入 Zombie；最终转换必须再次唤醒其 pidfd 等等待者。
    pub fn mark_terminated_if_all_members_terminal(&self) -> bool {
        let became_terminated = {
            let mut members = self.members.lock();
            let mut all_terminal = true;
            members.retain(|weak| {
                let Some(task) = weak.upgrade() else {
                    return false;
                };
                if !matches!(
                    task.state(),
                    crate::TaskState::Zombie | crate::TaskState::Dead
                ) {
                    all_terminal = false;
                }
                true
            });
            all_terminal
                && self
                    .terminated
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
        };
        if became_terminated && let Some(leader) = self.leader() {
            leader.exit_waiters.wake_all();
        }
        became_terminated
    }

    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
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
            closing: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            group_exit: AtomicU64::new(0),
            exec_lock: Spinlock::new(()),
            exec_generation: AtomicU64::new(0),
            exec_phase: AtomicU8::new(ExecPhase::Running as u8),
            signal_consumer_lock: Spinlock::new(()),
            user_abi_kind: AtomicU8::new(UserAbiKind::TomoriLinux as u8),
            personality: Spinlock::new(ProcessPersonalityState::TomoriLinux),
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

#[cfg(test)]
mod tests {
    use super::{GroupExitStatus, ThreadGroup};
    use crate::SignalNumber;

    #[test]
    fn group_exit_request_is_first_writer_wins() {
        let group = ThreadGroup::new();
        assert_eq!(group.group_exit_code(), None);

        assert_eq!(group.request_group_exit(-17), -17);
        assert_eq!(group.request_group_exit(23), -17);

        assert_eq!(group.group_exit_code(), Some(-17));
    }

    #[test]
    fn signal_and_exit_requests_share_one_first_writer_slot() {
        let exit_first = ThreadGroup::new();
        assert_eq!(exit_first.request_group_exit(42), 42);
        assert_eq!(
            exit_first.request_group_signal(SignalNumber::SIGKILL, false),
            GroupExitStatus::Exited(42)
        );

        let signal_first = ThreadGroup::new();
        assert_eq!(
            signal_first.request_group_signal(SignalNumber::SIGKILL, false),
            GroupExitStatus::Signaled {
                signal: SignalNumber::SIGKILL,
                core_dumped: false,
            }
        );
        assert_eq!(signal_first.request_group_exit(42), 9);
    }
}
