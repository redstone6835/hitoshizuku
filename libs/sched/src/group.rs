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
use crate::wait::WaitQueue;

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

/// Native 用户异常的稳定进程级记录。只保存首个故障，避免并发异常覆盖诊断现场。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFaultInfo {
    pub kind: u32,
    pub exception_code: u64,
    pub address: u64,
}

/// 订阅稳定进程身份的终止事件；实现方不得在回调中反向进入进程身份事务。
pub trait ProcessExitObserver: Send + Sync {
    fn process_exited(&self);
}

struct ProcessExitSubscription {
    id: u64,
    observer: Weak<dyn ProcessExitObserver>,
}

struct ProcessExitObservers {
    has_entries: AtomicBool,
    next_id: AtomicU64,
    entries: Spinlock<Vec<ProcessExitSubscription>>,
}

impl ProcessExitObservers {
    fn new() -> Self {
        Self {
            has_entries: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            entries: Spinlock::new(Vec::new()),
        }
    }

    fn subscribe(&self, observer: Weak<dyn ProcessExitObserver>) -> Option<u64> {
        let mut entries = self.entries.lock();
        if entries.try_reserve(1).is_err() {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        assert!(id != 0, "process-exit observer id 已耗尽");
        entries.push(ProcessExitSubscription { id, observer });
        self.has_entries.store(true, Ordering::Release);
        Some(id)
    }

    fn unsubscribe(&self, id: u64) -> bool {
        let mut entries = self.entries.lock();
        let old_len = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.is_empty() {
            self.has_entries.store(false, Ordering::Release);
        }
        entries.len() != old_len
    }

    fn notify(&self) {
        if !self.has_entries.load(Ordering::Acquire) {
            return;
        }
        {
            let mut entries = self.entries.lock();
            entries.retain(|entry| entry.observer.strong_count() != 0);
            if entries.is_empty() {
                self.has_entries.store(false, Ordering::Release);
                return;
            }
        }
        let mut after = 0u64;
        loop {
            let next = {
                let entries = self.entries.lock();
                let index = entries.partition_point(|entry| entry.id <= after);
                entries
                    .get(index)
                    .map(|entry| (entry.id, entry.observer.clone()))
            };
            let Some((id, observer)) = next else {
                break;
            };
            after = id;
            if let Some(observer) = observer.upgrade() {
                observer.process_exited();
            }
        }
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
    /// Native 子进程的线程组级 owner 表。它独立于 Task 的 POSIX parent/children
    /// 关系，因此 owner 线程组中的任意线程都可以 wait/reap，leader 提前退出也
    /// 不会改变所有权。
    native_children: Spinlock<Vec<Arc<Task>>>,
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
    /// 首个 Native 用户异常；故障路径不属于正常调度热路径。
    native_fault: Spinlock<Option<NativeFaultInfo>>,
    /// 串行化同一线程组的 exec prepare/revalidate/commit。
    exec_lock: Spinlock<()>,
    /// 每次成功安装新映像后推进，供 prepare 阶段做乐观快照。
    exec_generation: AtomicU64,
    /// 信号 consumer 与 exec 提交路径共享的阶段判定。
    exec_phase: AtomicU8,
    /// 把 consumer 的 phase 检查与实际出队组成同一个临界区。
    signal_consumer_lock: Spinlock<()>,
    /// 进程级 pidfd / wait 观察者队列。它不绑定某一个可被 exec 替换的 Task。
    process_exit_waiters: WaitQueue,
    /// pidfd/epoll 等非任务等待者的弱订阅表；无订阅时退出路径只读一个原子位。
    process_exit_observers: ProcessExitObservers,
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

    pub fn has_only_member(&self, task: &Arc<Task>) -> bool {
        self.group.has_only_member_locked(task)
    }

    pub(crate) fn try_add_member(&self, task: &Arc<Task>) -> bool {
        self.group.try_add_member_locked(task)
    }

    /// 构造受 exec 锁保护的成员快照，允许调用方在 PONR 前传播分配失败。
    pub fn try_member_snapshot(
        &self,
    ) -> Result<Vec<Arc<Task>>, alloc::collections::TryReserveError> {
        let members = self.group.members.lock();
        let mut snapshot = Vec::new();
        snapshot.try_reserve(members.len())?;
        snapshot.extend(members.iter().filter_map(Weak::upgrade));
        Ok(snapshot)
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
            native_children: Spinlock::new(Vec::new()),
            shared_signal: Arc::new(SharedSignal::new()),
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
            exited_usage: Spinlock::new(TaskUsage::default()),
            acct_live_members: AtomicUsize::new(0),
            acct_emitted: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            group_exit: AtomicU64::new(0),
            native_fault: Spinlock::new(None),
            exec_lock: Spinlock::new(()),
            exec_generation: AtomicU64::new(0),
            exec_phase: AtomicU8::new(ExecPhase::Running as u8),
            signal_consumer_lock: Spinlock::new(()),
            process_exit_waiters: WaitQueue::new(),
            process_exit_observers: ProcessExitObservers::new(),
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
            native_children: Spinlock::new(Vec::new()),
            shared_signal: shared,
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
            exited_usage: Spinlock::new(TaskUsage::default()),
            acct_live_members: AtomicUsize::new(0),
            acct_emitted: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            group_exit: AtomicU64::new(0),
            native_fault: Spinlock::new(None),
            exec_lock: Spinlock::new(()),
            exec_generation: AtomicU64::new(0),
            exec_phase: AtomicU8::new(ExecPhase::Running as u8),
            signal_consumer_lock: Spinlock::new(()),
            process_exit_waiters: WaitQueue::new(),
            process_exit_observers: ProcessExitObservers::new(),
            user_abi_kind: AtomicU8::new(UserAbiKind::TomoriLinux as u8),
            personality: Spinlock::new(ProcessPersonalityState::TomoriLinux),
        })
    }

    pub fn set_leader(&self, leader: &Arc<Task>) {
        let identity = crate::pid::lock_process_identity();
        self.set_leader_in(&identity, leader);
    }

    pub(crate) fn set_leader_in(
        &self,
        _identity: &crate::pid::ProcessIdentityGuard,
        leader: &Arc<Task>,
    ) {
        if let Some(pid) = leader.pid_root_in(_identity) {
            self.set_tgid(pid);
        }
        *self.leader.lock() = Arc::downgrade(leader);
    }

    pub fn leader(&self) -> Option<Arc<Task>> {
        self.leader.lock().upgrade()
    }

    /// 在进程仍处于 Running 时，以稳定身份访问当前 leader。
    ///
    /// 临界区按 `exec_lock -> PROCESS_IDENTITY_LOCK` 排序，保证检查期间既不能开始
    /// exec 资源清理，也不能把进程身份迁移给另一个 Task。回调必须保持短小，
    /// 不得阻塞，也不得再次进入进程身份事务。
    pub fn with_running_leader<R>(&self, inspect: impl FnOnce(&Arc<Task>) -> R) -> Option<R> {
        let _exec = self.exec_lock.lock();
        if self.exec_phase() != ExecPhase::Running {
            return None;
        }
        let _identity = crate::pid::lock_process_identity();
        let leader = self.leader.lock().upgrade()?;
        Some(inspect(&leader))
    }

    /// 尝试接纳一个成员。整体退出与成员登记通过 `members` 锁排序：
    ///
    /// - 若登记先完成，随后退出路径的 snapshot 必然包含该成员；
    /// - 若退出先发布，登记会被拒绝，避免 leader 可回收后又出现新线程。
    fn try_add_member_locked(&self, task: &Arc<Task>) -> bool {
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
        if self.exec_phase() != ExecPhase::Running
            || self.closing.load(Ordering::Acquire)
            || self.terminated.load(Ordering::Acquire)
        {
            return false;
        }
        task.publish_user_abi_kind(self.user_abi_kind());
        members.push(Arc::downgrade(task));
        self.acct_live_members.fetch_add(1, Ordering::Release);
        drop(members);

        // shared pending 可能在本成员加入前已经发布。发布方与成员登记都经过
        // members 锁排序：若 snapshot 先发生，这里能看到 pending；若登记先
        // 发生，发布方 snapshot 会覆盖本任务。屏蔽状态变化自身也会重新置 hint。
        if task.is_user_task() && task.shared_signal_pending_bits_quick() != 0 {
            task.mark_user_return_work();
        }
        true
    }

    pub fn try_add_member(&self, task: &Arc<Task>) -> bool {
        // 与 exec 共用同一把锁，避免 exec 重验后又插入兄弟线程。
        let _exec = self.exec_lock.lock();
        self.try_add_member_locked(task)
    }

    pub fn add_member(&self, task: &Arc<Task>) {
        assert!(
            self.try_add_member(task),
            "[sched][group] adding member to closing thread group"
        );
    }

    /// 把 Native child 登记到线程组 owner，而不是调用线程的 POSIX child 表。
    ///
    /// 成员索引和终止状态共用 `members` 锁，保证“整体终止已发布”后不会再
    /// 接纳新的 Native child。返回 `Ok(false)` 表示 owner 已进入退出流程。
    pub fn try_add_native_child(
        &self,
        child: Arc<Task>,
    ) -> Result<bool, alloc::collections::TryReserveError> {
        let members = self.members.lock();
        if self.closing.load(Ordering::Acquire) || self.terminated.load(Ordering::Acquire) {
            return Ok(false);
        }
        let mut children = self.native_children.lock();
        children.try_reserve(1)?;
        children.push(child);
        drop(children);
        drop(members);
        Ok(true)
    }

    /// 从 owner 表中移除尚未激活或被回滚的 Native child。
    pub fn remove_native_child(&self, child: &Arc<Task>) -> bool {
        let mut children = self.native_children.lock();
        let Some(index) = children
            .iter()
            .position(|candidate| Arc::ptr_eq(candidate, child))
        else {
            return false;
        };
        children.swap_remove(index);
        true
    }

    /// 仅供诊断和调度器边界构造稳定快照。
    pub fn snapshot_native_children(&self) -> Vec<Arc<Task>> {
        self.native_children.lock().clone()
    }

    pub(crate) fn has_native_children(&self) -> bool {
        !self.native_children.lock().is_empty()
    }

    /// 领取一个已经完全终止的 Native child。调用者负责释放 pid 和其它组索引。
    pub fn reap_native_child<F>(&self, mut pred: F) -> Option<Arc<Task>>
    where
        F: FnMut(&Arc<Task>) -> bool,
    {
        let mut children = self.native_children.lock();
        let index = children
            .iter()
            .position(|child| child.is_user_task() && child.is_waitable_zombie() && pred(child))?;
        Some(children.swap_remove(index))
    }

    /// owner 线程组整体终止时把未回收 Native child 交给系统 reaper。
    pub(crate) fn take_native_children_for_reparent(&self) -> Vec<Arc<Task>> {
        core::mem::take(&mut *self.native_children.lock())
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

    fn has_only_member_locked(&self, task: &Arc<Task>) -> bool {
        let members = self.members.lock();
        let mut found = false;
        for member in members.iter().filter_map(Weak::upgrade) {
            if Arc::ptr_eq(&member, task) {
                found = true;
            } else {
                return false;
            }
        }
        found
    }

    /// 判断当前线程组是否只有指定任务；检查与成员加入通过 exec 锁串行化。
    pub fn has_only_member(&self, task: &Arc<Task>) -> bool {
        let _exec = self.exec_lock.lock();
        self.has_only_member_locked(task)
    }

    /// 判断线程组是否恰好只保留 exec 执行者与待替换的旧 leader。
    pub(crate) fn has_only_exec_members(&self, executor: &Arc<Task>, leader: &Arc<Task>) -> bool {
        let members = self.members.lock();
        let mut saw_executor = false;
        let mut saw_leader = false;
        for member in members.iter().filter_map(Weak::upgrade) {
            if Arc::ptr_eq(&member, executor) {
                saw_executor = true;
            } else if Arc::ptr_eq(&member, leader) {
                saw_leader = true;
            } else {
                return false;
            }
        }
        saw_executor && saw_leader
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

    /// fork/clone 从读取父进程状态到完成子任务登记期间持有此 guard。
    pub(crate) fn lock_for_clone(&self) -> Option<ThreadGroupExecGuard<'_>> {
        let guard = self.lock_exec();
        (guard.phase() == ExecPhase::Running).then_some(guard)
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

    /// 记录首个 Native 用户异常。该状态独立于兼容 Linux 的 signal/wait 编码。
    pub fn record_native_fault(&self, info: NativeFaultInfo) {
        if info.kind != 0 {
            let mut fault = self.native_fault.lock();
            if fault.is_none() {
                *fault = Some(info);
            }
        }
    }

    pub fn native_fault(&self) -> Option<NativeFaultInfo> {
        *self.native_fault.lock()
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
        if became_terminated {
            self.process_exit_waiters.wake_all();
            self.process_exit_observers.notify();
        }
        became_terminated
    }

    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    /// 返回稳定的进程级退出观察队列，调用方不得将其替换为某个 Task 的队列。
    pub fn process_exit_waiters(&self) -> &WaitQueue {
        &self.process_exit_waiters
    }

    /// 订阅进程终止事件；订阅与终止并发时允许幂等补发，但不会漏失事件。
    pub fn try_subscribe_process_exit(
        &self,
        observer: Weak<dyn ProcessExitObserver>,
    ) -> Option<u64> {
        let id = self.process_exit_observers.subscribe(observer.clone())?;
        if self.is_terminated()
            && let Some(observer) = observer.upgrade()
        {
            observer.process_exited();
        }
        Some(id)
    }

    /// 取消进程终止事件订阅，返回该 ID 是否仍在订阅表中。
    pub fn unsubscribe_process_exit(&self, id: u64) -> bool {
        self.process_exit_observers.unsubscribe(id)
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
            native_children: Spinlock::new(Vec::new()),
            shared_signal: Arc::new(SharedSignal::new()),
            rlimits: Spinlock::new(Rlimits::new_with_defaults()),
            exited_usage: Spinlock::new(TaskUsage::default()),
            acct_live_members: AtomicUsize::new(0),
            acct_emitted: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            group_exit: AtomicU64::new(0),
            native_fault: Spinlock::new(None),
            exec_lock: Spinlock::new(()),
            exec_generation: AtomicU64::new(0),
            exec_phase: AtomicU8::new(ExecPhase::Running as u8),
            signal_consumer_lock: Spinlock::new(()),
            process_exit_waiters: WaitQueue::new(),
            process_exit_observers: ProcessExitObservers::new(),
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
    /// 会话 leader 绑定线程组身份；线程组内 exec 替换 leader 时无需搬运引用。
    leader: Spinlock<Weak<ThreadGroup>>,
    groups: Spinlock<Vec<Weak<ProcessGroup>>>,
    /// 控制终端句柄(不透明 cookie,由 TTY 层解析;sched 不依赖 general)。
    ctty: Spinlock<Option<u64>>,
}

impl Session {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sid: AtomicI32::new(PID_INVALID),
            leader: Spinlock::new(Weak::new()),
            groups: Spinlock::new(Vec::new()),
            ctty: Spinlock::new(None),
        })
    }

    pub fn set_leader(&self, leader: &Arc<Task>) {
        if let Some(pid) = leader.pid_root() {
            self.set_sid(pid);
        }
        *self.leader.lock() = Arc::downgrade(&leader.thread_group());
    }

    pub fn leader(&self) -> Option<Arc<Task>> {
        self.leader
            .lock()
            .upgrade()
            .and_then(|group| group.leader())
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

    /// 控制终端 cookie(TTY 层登记的不透明句柄)。
    pub fn ctty(&self) -> Option<u64> {
        *self.ctty.lock()
    }

    pub fn set_ctty(&self, cookie: Option<u64>) {
        *self.ctty.lock() = cookie;
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
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::{GroupExitStatus, ProcessExitObserver, ThreadGroup};
    use crate::SignalNumber;

    struct CountingExitObserver {
        calls: AtomicUsize,
    }

    impl ProcessExitObserver for CountingExitObserver {
        fn process_exited(&self) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

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

    #[test]
    fn cancelled_process_exit_subscription_is_not_notified() {
        let group = ThreadGroup::new();
        let observer = Arc::new(CountingExitObserver {
            calls: AtomicUsize::new(0),
        });
        let erased: Arc<dyn ProcessExitObserver> = observer.clone();
        let subscription = group
            .try_subscribe_process_exit(Arc::downgrade(&erased))
            .expect("订阅进程退出事件");

        group.unsubscribe_process_exit(subscription);
        assert!(group.mark_terminated_if_all_members_terminal());

        assert_eq!(observer.calls.load(Ordering::Relaxed), 0);
    }
}
