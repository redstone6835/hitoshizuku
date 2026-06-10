//! 单个任务的核心抽象。
//!
//! [`Task`] 是调度器管理的最小实体，对应 Linux 的 `task_struct`。本设计**不分配
//! PID**：任务身份即 `Arc<Task>`，父子关系通过 `Weak` / `Arc` 直接互引。
//!
//! ```text
//!   parent ── Weak ──▶  Task                     (父可能先死 → upgrade 失败 → reparent)
//!     ▲                  │
//!     │                  │ children: Vec<Arc<Task>>
//!     │                  ▼
//!   Task ◀── 子的 parent: Weak ──
//! ```
//!
//! **生命周期规则**：
//!
//! - 活跃任务的 `Arc` 由两处保活：其父的 `children` 列表、以及运行队列 / 等待
//!   队列中的强引用之一。
//! - `exit` 阶段任务仍留在父的 `children` 中（状态置 `Zombie`），等待父执行
//!   "reap"（删除数组项）后 `Arc` 才会归零。
//! - 父先死时，子被移交给 init（由更上层调用 [`Task::reparent_to`]）。
//!
//! 信号传递、IPC、等待都基于句柄（`Arc<Task>`），不经过全局整数表。

use alloc::alloc::{Layout, alloc, dealloc};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::arch_hooks;
use crate::arch_hooks::KernelEntry;
use crate::clone_flags::CloneFlags;
use crate::cpu::CpuMask;
use crate::eevdf::{SchedEntity, SchedParams};
use crate::group::{ProcessGroup, ThreadGroup};
use crate::ids::Credentials;
use crate::pid::{PidNamespace, PidT};
use crate::signal::{SharedSignal, SignalNumber, SignalState};
use crate::sync::Spinlock;
use crate::wait::WaitQueue;
use crate::wait_flags::WaitStatus;

/// Linux 线程名长度，包含结尾 NUL。
pub const TASK_COMM_LEN: usize = 16;
const DEFAULT_COMM: [u8; TASK_COMM_LEN] =
    [b'm', b'y', b'g', b'o', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// 每线程 robust futex 链表注册状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RobustListState {
    pub head: usize,
    pub len: usize,
}

/// 每线程 rseq 注册状态。当前仅作为 ABI 占位，不执行 rseq 快路径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RseqRegistration {
    pub ptr: usize,
    pub len: u32,
    pub signature: u32,
    pub registered: bool,
}

/// 任务状态机。
///
/// 状态转换由调度器内部原子 CAS 驱动，避免为"取状态"再持 rq 锁。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TaskState {
    /// 已创建但尚未入 runqueue。
    New = 0,
    /// 在运行队列中等待被调度。
    Runnable = 1,
    /// 当前正在某个 CPU 上运行。
    Running = 2,
    /// 可中断睡眠：阻塞在等待队列上，可被信号唤醒。
    Sleeping = 3,
    /// 不可中断睡眠：通常等待同步 I/O。
    Uninterruptible = 4,
    /// 被 job-control stop 信号暂停，不应留在 runqueue。
    Stopped = 5,
    /// 刚收到 SIGCONT，等待调度器把它重新放回 runqueue 的短暂状态。
    Continued = 6,
    /// 已调用 exit，等待父 reap。
    Zombie = 7,
    /// 已被父 reap，等最后一个 Arc 释放。
    Dead = 8,
}

impl TaskState {
    fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::New,
            1 => Self::Runnable,
            2 => Self::Running,
            3 => Self::Sleeping,
            4 => Self::Uninterruptible,
            5 => Self::Stopped,
            6 => Self::Continued,
            7 => Self::Zombie,
            _ => Self::Dead,
        }
    }
}

/// 退出码，承载正常 exit(code) 的原始数值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(pub i32);

const EXIT_REASON_NONE: u8 = 0;
const EXIT_REASON_EXITED: u8 = 1;
const EXIT_REASON_SIGNALED: u8 = 2;
const EXIT_REASON_CORE_DUMPED: u8 = 3;

/// 默认内核栈大小：64 KiB。fork 等深层调用需要足够空间，避免栈溢出损坏堆。
pub const DEFAULT_KERNEL_STACK_SIZE: usize = 64 * 1024;
/// 内核栈对齐：16 字节，匹配所有现用 ISA 的 ABI 栈对齐要求。
pub const KERNEL_STACK_ALIGN: usize = 16;

/// 一段按 ABI 对齐、可直接交给 arch 侧使用的内核栈。
///
/// 用 `NonNull<u8> + Layout` 而非 `Box<[u8]>`：`Box` 的对齐只到 `u8`，切换线程
/// 时栈顶必须满足更严格的 `KERNEL_STACK_ALIGN`，裸 `alloc/dealloc` 才能保证。
pub struct KernelStack {
    base: NonNull<u8>,
    layout: Layout,
}

impl KernelStack {
    /// 新建一段默认大小的内核栈。
    pub fn new() -> Self {
        Self::with_size(DEFAULT_KERNEL_STACK_SIZE)
    }

    /// 按指定字节数新建内核栈。`size` 会被向上对齐到 [`KERNEL_STACK_ALIGN`]。
    pub fn with_size(size: usize) -> Self {
        let size = size
            .max(KERNEL_STACK_ALIGN)
            .next_multiple_of(KERNEL_STACK_ALIGN);
        let layout = Layout::from_size_align(size, KERNEL_STACK_ALIGN)
            .expect("[sched][task] invalid kernel-stack layout");
        // Safety: size 非 0、align 合法；分配失败会返回 null，下方显式处理。
        let raw = unsafe { alloc(layout) };
        let base = NonNull::new(raw).expect("[sched][task] kernel stack OOM");
        Self { base, layout }
    }

    /// 逻辑栈顶（高地址端）。首次 context 初始化用到。
    pub fn top(&self) -> usize {
        self.base.as_ptr() as usize + self.layout.size()
    }

    pub fn size(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // Safety: base/layout 由 Self::with_size 同一次 alloc 记录，配对释放。
        unsafe { dealloc(self.base.as_ptr(), self.layout) };
    }
}

// Safety: KernelStack 是一段裸字节；它自身不持有任何线程局部状态。
unsafe impl Send for KernelStack {}
unsafe impl Sync for KernelStack {}

/// 每任务的 arch 上下文保存区。
///
/// 大小/对齐来自 [`arch_hooks::ops`] 注入的契约。为了让 `Task` 保持 arch 无关，
/// 这里只保留一段裸字节缓冲 + 指向它的 `NonNull`。真实布局由 arch 侧的
/// `switch_context` 汇编例程解读。
pub struct ArchContextSlot {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl ArchContextSlot {
    /// 根据已注入的 [`ArchContextOps`] 分配缓冲。未注入则 panic。
    pub fn new() -> Self {
        let ops = arch_hooks::ops_or_panic();
        let layout = Layout::from_size_align(ops.context_size, ops.context_align)
            .expect("[sched][task] arch context layout invalid");
        // Safety: layout 有效且 size 已被 ops 保证非 0（最少含 ra+sp）。
        let raw = unsafe { alloc(layout) };
        let ptr = NonNull::new(raw).expect("[sched][task] ArchContextSlot OOM");
        // 初始化为 0，便于调试 —— arch 侧随后会在 init_kernel_context 中覆盖。
        // Safety: 独占新分配的缓冲。
        unsafe {
            core::ptr::write_bytes(ptr.as_ptr(), 0, layout.size());
        }
        Self { ptr, layout }
    }

    /// 给 arch 侧 `switch_context` / `init_kernel_context` 用的裸指针。
    pub fn as_nonnull(&self) -> NonNull<u8> {
        self.ptr
    }
}

impl Drop for ArchContextSlot {
    fn drop(&mut self) {
        // Safety: ptr/layout 来自同一次 alloc，配对释放。
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

// Safety: 缓冲仅由汇编在持有调度锁时读写，本结构本身不暴露内部引用。
unsafe impl Send for ArchContextSlot {}
unsafe impl Sync for ArchContextSlot {}

/// 亲缘关系字段集中放在一把锁下，避免父/子两端各持一把锁带来的反序风险。
struct Relations {
    /// 父任务弱引用。父先死时 upgrade 失败，由更上层把 Weak 替换成 init。
    parent: Weak<Task>,
    /// 直接子任务的强引用列表。Zombie 子在被 reap 之前一直留在这里。
    children: Vec<Arc<Task>>,
    /// 所属线程组（CLONE_THREAD 共享同一 group，新进程则各自独立）。
    thread_group: Arc<ThreadGroup>,
    /// 所属进程组（setpgid 可改）。
    process_group: Arc<ProcessGroup>,
    /// 任务在各 namespace 中的 pid。从最外层祖先到自身所在 ns 依序排列，
    /// `pid_in_ns[0]` 是根 ns 的 pid（对应 Linux `task->pid`）。
    /// 无 pid 注册时可保持空 —— 调度核心完全不依赖该字段。
    pid_in_ns: Vec<(Arc<PidNamespace>, PidT)>,
}

/// 内核任务。
///
/// 整个结构使用内部可变性：稳定字段（`sched`、`exit_waiters`、`signal`、
/// `vfork_done`）放在外层；状态字段用原子；亲缘字段集中在 [`Relations`] 内，
/// 由一把 [`Spinlock`] 保护；其它跨子系统状态（凭据、共享信号表、内核栈、
/// arch ctx、ext 侧表）各自独立小锁。
pub struct Task {
    pub sched: SchedEntity,
    state: AtomicU8,
    exit_code: AtomicI32,
    has_exit_code: AtomicU8,
    exit_reason: AtomicU8,
    exit_signal_number: AtomicI32,
    wait_stop_sig: AtomicI32,
    wait_stop_pending: AtomicU8,
    wait_continue_pending: AtomicU8,
    pub exit_waiters: WaitQueue,
    rel: Spinlock<Relations>,
    kstack: Spinlock<Option<KernelStack>>,
    ctx: Spinlock<Option<ArchContextSlot>>,

    /// 凭据。setuid 时以"整个 Arc 替换"方式更新，避免改写时的撕裂读。
    creds: Spinlock<Arc<Credentials>>,
    /// per-task 信号 pending / blocked。
    pub signal: SignalState,
    /// 与同 thread-group 共享的 sigaction + shared pending。
    /// CLONE_SIGHAND 时 `Arc::clone`，否则 fork 时深拷一份。
    shared_signal: Spinlock<Arc<SharedSignal>>,
    /// 退出时给父发的信号号码（默认 SIGCHLD=17）；clone 低 8 位指定。
    /// 值 0 表示"不发信号"（CLONE_THREAD 等情况）。
    exit_signal: AtomicI32,
    /// CLONE_VFORK：父在此队列阻塞，子调 execve / exit 时唤醒。
    pub vfork_done: WaitQueue,
    /// 子尚未"通过 exec 或 exit 释放父"时为 true。
    vforking: AtomicBool,
    /// `CLONE_CHILD_CLEARTID` / `set_tid_address` 指定的用户态 TID 地址。
    clear_child_tid: AtomicUsize,
    /// `set_robust_list` 注册的每线程 robust futex 链表。
    robust_list: Spinlock<RobustListState>,
    /// `rseq` 注册状态。完整 restartable sequence 语义后续由 arch/trap 接入。
    rseq: Spinlock<RseqRegistration>,
    /// `PR_SET_NAME` / `PR_GET_NAME` 暴露的 per-thread comm。
    comm: Spinlock<[u8; TASK_COMM_LEN]>,
    /// CPU 亲和性位图。预留单 word，单 CPU 原型暂不强制。
    cpu_affinity: AtomicU64,
    /// 最近一次绑定或运行的 CPU。迁移/唤醒选择使用，默认 0。
    current_cpu: AtomicUsize,
    /// 子系统侧表：VFS context / fdtable 等通过 [`TaskExtKey`] 挂载。
    /// 详见模块级 [`TaskExtCloneHook`]。
    ext: Spinlock<Vec<TaskExt>>,
}

impl Task {
    /// 创建一个新任务。`thread_group` / `process_group` / `parent` 由调用方给出，
    /// 调用方负责在返回 `Arc` 之后把它登记进父的 `children` 与组成员表。
    ///
    /// `creds` / `shared_signal` 默认从 `thread_group` 复制：thread group 内
    /// 必然共享 shared_signal。调用方若需覆盖（如 CLONE_SIGHAND），可随后
    /// 调 [`Task::install_shared_signal`] 替换。
    pub fn new(
        params: SchedParams,
        parent: Weak<Task>,
        thread_group: Arc<ThreadGroup>,
        process_group: Arc<ProcessGroup>,
    ) -> Arc<Self> {
        let shared = Arc::clone(thread_group.shared_signal());
        Arc::new(Self {
            sched: SchedEntity::new(params),
            state: AtomicU8::new(TaskState::New as u8),
            exit_code: AtomicI32::new(0),
            has_exit_code: AtomicU8::new(0),
            exit_reason: AtomicU8::new(EXIT_REASON_NONE),
            exit_signal_number: AtomicI32::new(0),
            wait_stop_sig: AtomicI32::new(0),
            wait_stop_pending: AtomicU8::new(0),
            wait_continue_pending: AtomicU8::new(0),
            exit_waiters: WaitQueue::new(),
            rel: Spinlock::new(Relations {
                parent,
                children: Vec::new(),
                thread_group,
                process_group,
                pid_in_ns: Vec::new(),
            }),
            kstack: Spinlock::new(None),
            ctx: Spinlock::new(None),
            creds: Spinlock::new(Arc::new(Credentials::root())),
            signal: SignalState::new(),
            shared_signal: Spinlock::new(shared),
            exit_signal: AtomicI32::new(SignalNumber::SIGCHLD.raw() as i32),
            vfork_done: WaitQueue::new(),
            vforking: AtomicBool::new(false),
            clear_child_tid: AtomicUsize::new(0),
            robust_list: Spinlock::new(RobustListState::default()),
            rseq: Spinlock::new(RseqRegistration::default()),
            comm: Spinlock::new(DEFAULT_COMM),
            cpu_affinity: AtomicU64::new(u64::MAX),
            current_cpu: AtomicUsize::new(0),
            ext: Spinlock::new(Vec::new()),
        })
    }

    pub fn state(&self) -> TaskState {
        TaskState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// 直接覆盖状态。仅供调度器内部在已经建立同步关系（持有 rq 锁或 CAS 成功）后使用。
    pub(crate) fn set_state(&self, new_state: TaskState) {
        self.state.store(new_state as u8, Ordering::Release);
    }

    /// CAS 状态，成功时返回 `true`。用于 wakeup 路径无锁切换。
    pub fn cas_state(&self, expect: TaskState, new: TaskState) -> bool {
        self.state
            .compare_exchange(expect as u8, new as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn parent(&self) -> Option<Arc<Task>> {
        self.rel.lock().parent.upgrade()
    }

    /// 把 `child` 登记为本任务的子。调用方持有 `child` 的 `Arc` 副本即可。
    pub fn add_child(&self, child: Arc<Task>) {
        self.rel.lock().children.push(child);
    }

    /// 从直接子表中移除指定任务。用于 clone/exec 安装失败时回滚尚未运行的子任务。
    pub(crate) fn remove_child(&self, child: &Arc<Task>) -> bool {
        let mut rel = self.rel.lock();
        let Some(pos) = rel.children.iter().position(|c| Arc::ptr_eq(c, child)) else {
            return false;
        };
        rel.children.swap_remove(pos);
        true
    }

    /// 父先死时，把所有子转交给 `new_parent`（通常为 init）。
    pub fn reparent_children_to(&self, new_parent: &Arc<Task>) {
        let drained: Vec<Arc<Task>> = {
            let mut rel = self.rel.lock();
            core::mem::take(&mut rel.children)
        };
        let new_parent_weak = Arc::downgrade(new_parent);
        for child in drained.iter() {
            child.rel.lock().parent = new_parent_weak.clone();
        }
        let mut np = new_parent.rel.lock();
        np.children.extend(drained);
    }

    /// 替换本任务在亲缘表里的"父"指向，常见于 reparent。
    pub fn reparent_to(&self, new_parent: &Arc<Task>) {
        self.rel.lock().parent = Arc::downgrade(new_parent);
    }

    pub fn thread_group(&self) -> Arc<ThreadGroup> {
        Arc::clone(&self.rel.lock().thread_group)
    }

    pub fn process_group(&self) -> Arc<ProcessGroup> {
        Arc::clone(&self.rel.lock().process_group)
    }

    /// 加入新的进程组（setpgid 等价物）。调用方负责更新组成员表。
    pub fn set_process_group(&self, pg: Arc<ProcessGroup>) {
        self.rel.lock().process_group = pg;
    }

    /// 标记任务退出：写入退出码、置 Zombie、唤醒 `exit_waiters`。
    /// 调用方负责把任务从 runqueue 移除并向父投递 SIGCHLD。
    pub fn mark_exited(&self, code: ExitCode) {
        self.exit_code.store(code.0, Ordering::Release);
        if self.exit_reason.load(Ordering::Acquire) == EXIT_REASON_NONE {
            self.exit_reason
                .store(EXIT_REASON_EXITED, Ordering::Release);
        }
        self.has_exit_code.store(1, Ordering::Release);
        self.wait_stop_pending.store(0, Ordering::Release);
        self.wait_continue_pending.store(0, Ordering::Release);
        self.set_state(TaskState::Zombie);
        self.exit_waiters.wake_all();
    }

    pub fn exit_code(&self) -> Option<ExitCode> {
        if self.has_exit_code.load(Ordering::Acquire) == 0 {
            None
        } else {
            Some(ExitCode(self.exit_code.load(Ordering::Acquire)))
        }
    }

    /// 标记下一次退出是信号终止。随后 [`mark_exited`] 会保留该原因，
    /// wait4/waitid 按 WIFSIGNALED/WCOREDUMP 编码。
    pub(crate) fn mark_signaled_exit(&self, sig: SignalNumber, core_dumped: bool) {
        self.exit_signal_number
            .store(sig.raw() as i32, Ordering::Release);
        self.exit_reason.store(
            if core_dumped {
                EXIT_REASON_CORE_DUMPED
            } else {
                EXIT_REASON_SIGNALED
            },
            Ordering::Release,
        );
    }

    /// 把已记录的退出原因转换成 wait4/waitid 的 `wstatus`。
    pub fn exit_wait_status(&self) -> Option<WaitStatus> {
        let code = self.exit_code()?;
        match self.exit_reason.load(Ordering::Acquire) {
            EXIT_REASON_SIGNALED | EXIT_REASON_CORE_DUMPED => {
                let sig = SignalNumber::from_raw(self.exit_signal_number.load(Ordering::Acquire))
                    .unwrap_or(SignalNumber::SIGTERM);
                if self.exit_reason.load(Ordering::Acquire) == EXIT_REASON_CORE_DUMPED {
                    Some(WaitStatus::from_signal_core(sig))
                } else {
                    Some(WaitStatus::from_signal(sig))
                }
            }
            _ => Some(WaitStatus::from_exit(code.0)),
        }
    }

    /// 标记任务因 stop 信号进入停止态，并记录一次可被 `wait(WUNTRACED)` 观察的事件。
    pub(crate) fn mark_stopped(&self, sig: SignalNumber) -> bool {
        loop {
            let state = self.state();
            match state {
                TaskState::New | TaskState::Zombie | TaskState::Dead => return false,
                TaskState::Stopped => break,
                _ => {
                    if self.cas_state(state, TaskState::Stopped) {
                        break;
                    }
                }
            }
        }

        self.wait_stop_sig
            .store(sig.raw() as i32, Ordering::Release);
        self.wait_continue_pending.store(0, Ordering::Release);
        self.wait_stop_pending.store(1, Ordering::Release);
        if let Some(parent) = self.parent() {
            parent.exit_waiters.wake_all();
        }
        true
    }

    /// 标记停止任务收到 SIGCONT。调度器随后会把 `Continued` 任务重新入队。
    pub(crate) fn mark_continued(&self) -> bool {
        if !self.cas_state(TaskState::Stopped, TaskState::Continued) {
            return false;
        }
        self.wait_stop_pending.store(0, Ordering::Release);
        self.wait_continue_pending.store(1, Ordering::Release);
        if let Some(parent) = self.parent() {
            parent.exit_waiters.wake_all();
        }
        true
    }

    /// 返回并按需消费一次 stopped wait 事件。
    pub(crate) fn wait_stopped_status(&self, nowait: bool) -> Option<WaitStatus> {
        let pending = if nowait {
            self.wait_stop_pending.load(Ordering::Acquire)
        } else {
            self.wait_stop_pending.swap(0, Ordering::AcqRel)
        };
        if pending == 0 {
            return None;
        }
        let sig = SignalNumber::from_raw(self.wait_stop_sig.load(Ordering::Acquire))
            .unwrap_or(SignalNumber::SIGSTOP);
        Some(WaitStatus::from_stop(sig))
    }

    /// 返回并按需消费一次 continued wait 事件。
    pub(crate) fn wait_continued_status(&self, nowait: bool) -> Option<WaitStatus> {
        let pending = if nowait {
            self.wait_continue_pending.load(Ordering::Acquire)
        } else {
            self.wait_continue_pending.swap(0, Ordering::AcqRel)
        };
        if pending == 0 {
            return None;
        }
        Some(WaitStatus::continued())
    }

    /// 在 `children` 中查找首个 `Zombie` 子并取走（reap）。
    /// 返回 `None` 表示当前没有可回收的退出子。
    pub fn reap_any_zombie(&self) -> Option<Arc<Task>> {
        let mut rel = self.rel.lock();
        let pos = rel
            .children
            .iter()
            .position(|c| c.state() == TaskState::Zombie)?;
        let zombie = rel.children.swap_remove(pos);
        zombie.set_state(TaskState::Dead);
        Some(zombie)
    }

    /// 在 `children` 中查找匹配 `pred` 的子并取走。
    /// 通用钩子，供上层实现 `waitpid(specific_pid)`、`waitpid(pgid)` 等变体。
    pub fn reap_matching<F>(&self, mut pred: F) -> Option<Arc<Task>>
    where
        F: FnMut(&Arc<Task>) -> bool,
    {
        let mut rel = self.rel.lock();
        let pos = rel
            .children
            .iter()
            .position(|c| c.state() == TaskState::Zombie && pred(c))?;
        let zombie = rel.children.swap_remove(pos);
        zombie.set_state(TaskState::Dead);
        Some(zombie)
    }

    /// 列出当前直接子任务的强引用快照；释放锁后再消费，避免持锁回调。
    pub fn snapshot_children(&self) -> Vec<Arc<Task>> {
        self.rel.lock().children.clone()
    }

    // ── 执行体：内核栈 + arch 上下文 ──────────────────────────────────────

    /// 把一段**已经准备好**的内核栈与 arch 上下文挂到 Task 上。
    ///
    /// 通常调用链：`Task::new(...) → with_kernel_thread_context(...)`；后者
    /// 分配一段栈、调 [`arch_hooks::ops`] 的 `init_kernel_context` 把首次
    /// `switch_context` 的恢复点指向 `entry`。见 [`Task::into_kernel_thread`]。
    pub fn install_execution(&self, stack: KernelStack, ctx: ArchContextSlot) {
        *self.kstack.lock() = Some(stack);
        *self.ctx.lock() = Some(ctx);
    }

    /// 为 Task 构造一段内核执行体：分配默认大小的内核栈、分配 ctx 缓冲、调
    /// `init_kernel_context` 把首次 resume 恢复点指向 `entry`。
    ///
    /// 仅在已经调用 [`arch_hooks::register`] 之后可用。
    pub fn into_kernel_thread(self: &Arc<Task>, entry: KernelEntry, arg: usize) {
        let stack = KernelStack::new();
        let ctx = ArchContextSlot::new();
        let stack_top = stack.top();
        let ctx_ptr = ctx.as_nonnull();
        let ops = arch_hooks::ops_or_panic();
        // Safety: stack/ctx 都是本函数新分配、尚未被任何核引用；本函数为唯一写者。
        unsafe {
            (ops.init_kernel_context)(ctx_ptr, stack_top, entry, arg);
        }
        self.install_execution(stack, ctx);
    }

    /// 把当前正在执行的上下文"认领"给本 Task：分配一个空 ctx 缓冲，不填入
    /// entry —— 因为本线程已经在执行。下一次 `switch_context(prev=self, next=...)`
    /// 时 arch 汇编会把真实寄存器写入这块缓冲。
    ///
    /// 这是 init / idle / boot cpu 首次把自己登记为 Task 时的唯一方式，
    /// 与 [`into_kernel_thread`] 互斥。
    ///
    /// 仅在已经调用 [`arch_hooks::register`] 之后可用。
    pub fn adopt_current_context(&self) {
        let ctx = ArchContextSlot::new();
        *self.ctx.lock() = Some(ctx);
        // 不分配内核栈：当前线程已经在某个栈上执行，由启动路径 / 调用方保有。
    }

    /// 取出 Task 的 arch 上下文裸指针，供调度器调用 `switch_context`。
    ///
    /// 返回 `None` 表示该 Task 没有挂执行体（例如"仅作记账用途"的 init placeholder）。
    pub fn arch_context(&self) -> Option<NonNull<u8>> {
        self.ctx.lock().as_ref().map(|c| c.as_nonnull())
    }

    /// 内核栈栈顶。trap 处理程序需要用它重设当前架构的内核 trap 栈寄存器。
    pub fn kernel_stack_top(&self) -> Option<usize> {
        self.kstack.lock().as_ref().map(|s| s.top())
    }

    /// 确保当前任务拥有一段内核 trap 栈，并返回栈顶。
    ///
    /// boot init 任务是通过 [`Task::adopt_current_context`] 接管当前上下文的，
    /// 初始没有归属自己的内核栈；当它被转换成第一个用户进程时需要补上这段栈。
    pub fn ensure_kernel_stack(&self) -> usize {
        let mut stack = self.kstack.lock();
        if stack.is_none() {
            *stack = Some(KernelStack::new());
        }
        stack
            .as_ref()
            .expect("[sched][task] kernel stack installation failed")
            .top()
    }

    // ── PID 命名层接口 ───────────────────────────────────────────────────

    /// 把任务在某个 namespace 中的 pid 登记到本任务上。
    ///
    /// 多 ns 共享同一任务时多次调用：祖先 ns 在前、自身 ns 在后。重复登记
    /// 同一 namespace 视作配置错误（debug_assert）。
    pub fn register_pid(&self, ns: Arc<PidNamespace>, pid: PidT) {
        let mut rel = self.rel.lock();
        debug_assert!(
            !rel.pid_in_ns.iter().any(|(n, _)| Arc::ptr_eq(n, &ns)),
            "[sched][pid] task already registered in namespace"
        );
        rel.pid_in_ns.push((ns, pid));
    }

    /// 在指定 ns 中查询任务的 pid。从外向内顺序匹配。
    pub fn pid_in(&self, ns: &Arc<PidNamespace>) -> Option<PidT> {
        let rel = self.rel.lock();
        rel.pid_in_ns
            .iter()
            .find(|(n, _)| Arc::ptr_eq(n, ns))
            .map(|(_, pid)| *pid)
    }

    /// 任务在自身最深 ns 中的 pid（对应 Linux `gettid` 的近似语义）。
    pub fn pid_local(&self) -> Option<PidT> {
        self.rel.lock().pid_in_ns.last().map(|(_, pid)| *pid)
    }

    /// 任务在根 ns 中的 pid（对应 Linux `task->pid`）。
    pub fn pid_root(&self) -> Option<PidT> {
        self.rel.lock().pid_in_ns.first().map(|(_, pid)| *pid)
    }

    /// 取出所有 (ns, pid) 登记副本，便于 exit 时反向调用 `release`。
    pub fn pid_namespaces_snapshot(&self) -> Vec<(Arc<PidNamespace>, PidT)> {
        self.rel.lock().pid_in_ns.clone()
    }

    // ── 凭据 ─────────────────────────────────────────────────────────────

    /// 当前凭据快照。读期间持锁极短，调用方拿到 `Arc` 后即可释放。
    pub fn credentials(&self) -> Arc<Credentials> {
        Arc::clone(&self.creds.lock())
    }

    /// 整体替换凭据。setuid / setgid / capset 路径调用。
    pub fn set_credentials(&self, new: Arc<Credentials>) {
        *self.creds.lock() = new;
    }

    // ── 信号 ─────────────────────────────────────────────────────────────

    /// 取本任务当前的 SharedSignal（thread-group 共享部分）。
    pub fn shared_signal(&self) -> Arc<SharedSignal> {
        Arc::clone(&self.shared_signal.lock())
    }

    /// 替换 SharedSignal —— 仅供 spawn 时根据 CLONE_SIGHAND 设定使用。
    pub fn install_shared_signal(&self, shared: Arc<SharedSignal>) {
        *self.shared_signal.lock() = shared;
    }

    /// exit 时给父发的信号号码（0 表示不发）。
    pub fn exit_signal(&self) -> i32 {
        self.exit_signal.load(Ordering::Acquire)
    }

    pub fn set_exit_signal(&self, sig: i32) {
        self.exit_signal.store(sig, Ordering::Release);
    }

    // ── vfork 同步 ───────────────────────────────────────────────────────

    pub fn is_vforking(&self) -> bool {
        self.vforking.load(Ordering::Acquire)
    }

    pub fn set_vforking(&self, v: bool) {
        self.vforking.store(v, Ordering::Release);
    }

    pub fn clear_child_tid(&self) -> usize {
        self.clear_child_tid.load(Ordering::Acquire)
    }

    pub fn set_clear_child_tid(&self, user_addr: usize) {
        self.clear_child_tid.store(user_addr, Ordering::Release);
    }

    pub fn robust_list(&self) -> RobustListState {
        *self.robust_list.lock()
    }

    pub fn set_robust_list(&self, head: usize, len: usize) {
        *self.robust_list.lock() = RobustListState { head, len };
    }

    pub fn rseq_registration(&self) -> RseqRegistration {
        *self.rseq.lock()
    }

    pub fn set_rseq_registration(&self, registration: RseqRegistration) {
        *self.rseq.lock() = registration;
    }

    pub fn clear_rseq_registration(&self) {
        *self.rseq.lock() = RseqRegistration::default();
    }

    pub fn comm(&self) -> [u8; TASK_COMM_LEN] {
        *self.comm.lock()
    }

    pub fn set_comm(&self, name: &[u8]) {
        let mut comm = [0u8; TASK_COMM_LEN];
        let n = name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(name.len())
            .min(TASK_COMM_LEN - 1);
        comm[..n].copy_from_slice(&name[..n]);
        *self.comm.lock() = comm;
    }

    // ── CPU 亲和性 ───────────────────────────────────────────────────────

    pub fn cpu_affinity(&self) -> u64 {
        self.cpu_affinity.load(Ordering::Acquire)
    }

    pub fn set_cpu_affinity(&self, mask: u64) {
        self.cpu_affinity
            .store(CpuMask::from_bits_or_boot(mask).bits(), Ordering::Release);
    }

    pub fn current_cpu(&self) -> usize {
        self.current_cpu.load(Ordering::Acquire)
    }

    pub(crate) fn set_current_cpu(&self, cpu_id: usize) {
        self.current_cpu.store(cpu_id, Ordering::Release);
    }

    // ── 子系统侧表（VFS / FdTable 等） ───────────────────────────────────

    /// 安装一个子系统状态。同 key 重复装入视作配置错误（debug_assert）。
    pub fn ext_install(&self, key: TaskExtKey, payload: Arc<dyn Any + Send + Sync>) {
        let mut ext = self.ext.lock();
        debug_assert!(
            !ext.iter().any(|e| e.key == key),
            "[sched][ext] key 0x{:x} already installed",
            key,
        );
        ext.push(TaskExt { key, payload });
    }

    /// 查询某个子系统状态；不存在返回 `None`。
    pub fn ext_lookup(&self, key: TaskExtKey) -> Option<Arc<dyn Any + Send + Sync>> {
        self.ext
            .lock()
            .iter()
            .find(|e| e.key == key)
            .map(|e| Arc::clone(&e.payload))
    }

    /// 移除并返回某个子系统状态。
    pub fn ext_remove(&self, key: TaskExtKey) -> Option<Arc<dyn Any + Send + Sync>> {
        let mut ext = self.ext.lock();
        let pos = ext.iter().position(|e| e.key == key)?;
        Some(ext.swap_remove(pos).payload)
    }

    /// 列出当前所有子系统挂载，便于 fork 时遍历。
    pub fn ext_snapshot(&self) -> Vec<(TaskExtKey, Arc<dyn Any + Send + Sync>)> {
        self.ext
            .lock()
            .iter()
            .map(|e| (e.key, Arc::clone(&e.payload)))
            .collect()
    }
}

// ── TaskExt 侧表 ────────────────────────────────────────────────────────────

/// 子系统标识：每个上层子系统申请一个唯一 key 来挂状态。
pub type TaskExtKey = u64;

/// VFS 进程上下文（cwd / root / umask）。
pub const TASKEXT_VFS_CONTEXT: TaskExtKey = 0x0001_0000;
/// VFS 文件描述符表。
pub const TASKEXT_VFS_FDTABLE: TaskExtKey = 0x0001_0001;
/// 进程地址空间（general::mm::VmSpace 通过此键挂在 Task 的 ext 表上）。
pub const TASKEXT_VM_SPACE: TaskExtKey = 0x0001_0002;
/// 已保存的用户 trap frame（kernel/hal 通过此键挂在 Task 的 ext 表上）。
pub const TASKEXT_USER_TRAP_FRAME: TaskExtKey = 0x0001_0003;
/// 当前任务的可执行路径（kernel execve 安装，procfs `/proc/self/exe` 读取）。
pub const TASKEXT_EXEC_PATH: TaskExtKey = 0x0002_0000;
/// 当前任务的 argv 快照（kernel execve 安装，procfs `/proc/[pid]/cmdline` 读取）。
pub const TASKEXT_EXEC_ARGS: TaskExtKey = 0x0002_0001;
/// 当前任务的 envp 快照（kernel execve 安装，procfs `/proc/[pid]/environ` 读取）。
pub const TASKEXT_EXEC_ENVP: TaskExtKey = 0x0002_0002;

/// 单条子系统挂载。
pub struct TaskExt {
    pub key: TaskExtKey,
    pub payload: Arc<dyn Any + Send + Sync>,
}

/// fork/clone 时由上层提供的拷贝策略。
///
/// sched 不知道 `payload` 内部该怎么 fork（深拷？共享？按 CLONE_FS / CLONE_FILES？），
/// 把决策权委托给上层（kernel）注册的实现。
pub trait TaskExtCloneHook: Send + Sync {
    fn clone_for(
        &self,
        key: TaskExtKey,
        src: &Arc<dyn Any + Send + Sync>,
        flags: CloneFlags,
    ) -> Arc<dyn Any + Send + Sync>;
}

static EXT_CLONE_HOOK: Spinlock<Option<&'static dyn TaskExtCloneHook>> = Spinlock::new(None);

/// 注册全局 ext clone hook。kernel 启动期调用一次。
pub fn register_ext_clone_hook(hook: &'static dyn TaskExtCloneHook) {
    *EXT_CLONE_HOOK.lock() = Some(hook);
}

/// 取已注册的 hook；未注册返回 `None`。
pub fn ext_clone_hook() -> Option<&'static dyn TaskExtCloneHook> {
    *EXT_CLONE_HOOK.lock()
}
