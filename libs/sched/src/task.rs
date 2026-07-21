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
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

use crate::arch_hooks;
use crate::arch_hooks::KernelEntry;
use crate::clone_flags::CloneFlags;
use crate::cpu::CpuMask;
use crate::eevdf::{SchedEntity, SchedParams};
use crate::group::{ProcessGroup, ThreadGroup};
use crate::ids::Credentials;
use crate::pid::{PidNamespace, PidT};
use crate::placement::{PlacementSnapshot, TaskPlacement};
use crate::rseq::{RseqEvent, RseqEvents};
use crate::sched_class::{RT_PRIO_MAX, SchedAttr, SchedPolicy};
use crate::signal::{SharedSignal, SignalNumber, SignalState};
use crate::sync::Spinlock;
use crate::wait::WaitQueue;
use crate::wait_flags::WaitStatus;

static TASK_LIVE: AtomicUsize = AtomicUsize::new(0);
static TASK_CREATED: AtomicUsize = AtomicUsize::new(0);
static TASK_DROPPED: AtomicUsize = AtomicUsize::new(0);
static TASK_TRACKER: Spinlock<Vec<Weak<Task>>> = Spinlock::new(Vec::new());

#[derive(Debug, Clone, Copy, Default)]
pub struct TaskDiag {
    pub live: usize,
    pub created: usize,
    pub dropped: usize,
    pub tracked_alive: usize,
    pub zombie: usize,
    pub dead: usize,
    pub pidless: usize,
    pub child_links: usize,
    pub dead_child_links: usize,
    pub max_external_refs: usize,
    pub dead_external_refs: usize,
    pub shared_pending_infos: usize,
    pub max_shared_pending_infos: usize,
    pub dead_ref_sample_pid: PidT,
    pub dead_ref_sample_parent_pid: PidT,
    pub dead_ref_sample_refs: usize,
    pub dead_ref_sample_on_rq: bool,
    pub dead_ref_sample_has_ctx: bool,
    pub dead_ref_sample_has_kstack: bool,
    pub dead_ref_sample_exts: usize,
    pub dead_ref_sample_comm: [u8; TASK_COMM_LEN],
}

pub fn task_diag() -> TaskDiag {
    let mut diag = TaskDiag {
        live: TASK_LIVE.load(Ordering::Acquire),
        created: TASK_CREATED.load(Ordering::Acquire),
        dropped: TASK_DROPPED.load(Ordering::Acquire),
        ..TaskDiag::default()
    };
    let mut tracker = TASK_TRACKER.lock();
    tracker.retain(|weak| weak.strong_count() != 0);
    for weak in tracker.iter() {
        let Some(task) = weak.upgrade() else {
            continue;
        };
        diag.tracked_alive += 1;
        let external_refs = Arc::strong_count(&task).saturating_sub(1);
        diag.max_external_refs = diag.max_external_refs.max(external_refs);
        if task.pid_root().is_none() {
            diag.pidless += 1;
        }
        let rel = task.rel.lock();
        diag.child_links = diag.child_links.saturating_add(rel.children.len());
        diag.dead_child_links = diag.dead_child_links.saturating_add(
            rel.children
                .iter()
                .filter(|child| matches!(child.state(), TaskState::Zombie | TaskState::Dead))
                .count(),
        );
        drop(rel);
        let shared_pending = task.shared_signal().pending_len_hint();
        diag.shared_pending_infos = diag.shared_pending_infos.saturating_add(shared_pending);
        diag.max_shared_pending_infos = diag.max_shared_pending_infos.max(shared_pending);
        match task.state() {
            TaskState::Zombie => diag.zombie += 1,
            TaskState::Dead => {
                diag.dead += 1;
                diag.dead_external_refs = diag.dead_external_refs.saturating_add(external_refs);
                if external_refs > diag.dead_ref_sample_refs {
                    diag.dead_ref_sample_pid = task.pid_root().unwrap_or(0);
                    diag.dead_ref_sample_parent_pid =
                        task.parent().and_then(|p| p.pid_root()).unwrap_or(0);
                    diag.dead_ref_sample_refs = external_refs;
                    diag.dead_ref_sample_on_rq = task.sched.on_rq();
                    diag.dead_ref_sample_has_ctx = task.ctx.lock().is_some();
                    diag.dead_ref_sample_has_kstack = task.kstack.lock().is_some();
                    diag.dead_ref_sample_exts = task.ext.lock().len();
                    diag.dead_ref_sample_comm = *task.comm.lock();
                }
            }
            _ => {}
        }
    }
    diag
}

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

#[derive(Clone, Copy)]
struct PiDonation {
    token: usize,
    attr: SchedAttr,
}

struct PiState {
    base: SchedAttr,
    donations: Vec<PiDonation>,
}

impl PiState {
    fn new(base: SchedAttr) -> Self {
        Self {
            base: base.normalized(),
            donations: Vec::new(),
        }
    }

    fn effective(&self) -> SchedAttr {
        let mut effective = self.base;
        for donation in &self.donations {
            effective = more_urgent(effective, donation.attr);
        }
        effective
    }
}

/// 比较 PI donation 的调度紧迫程度。
///
/// RT waiter 可以把普通任务提升到 RT；同为 fair 时继承更高权重（更小
/// nice）。Deadline donation 暂时折算为最高 RT，避免把 owner 的 deadline
/// 带宽状态伪造成另一份 admission reservation。
fn more_urgent(current: SchedAttr, donated: SchedAttr) -> SchedAttr {
    match donated.policy {
        SchedPolicy::Deadline => {
            if current.policy == SchedPolicy::Deadline {
                current
            } else {
                SchedAttr::rt_fifo(RT_PRIO_MAX)
            }
        }
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => {
            let donated_prio = donated.priority;
            match current.policy {
                SchedPolicy::Deadline => current,
                SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin
                    if current.priority >= donated_prio =>
                {
                    current
                }
                SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => SchedAttr::rt_fifo(donated_prio),
                _ => SchedAttr::rt_fifo(donated_prio),
            }
        }
        SchedPolicy::Fair => {
            if current.policy == SchedPolicy::Fair && donated.nice < current.nice {
                let mut boosted = current;
                boosted.nice = donated.nice;
                boosted
            } else {
                current
            }
        }
        SchedPolicy::Idle => current,
    }
}

/// 每线程 rseq 注册状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RseqRegistration {
    pub ptr: usize,
    pub len: u32,
    pub signature: u32,
    pub registered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigAltStack {
    pub sp: usize,
    pub size: usize,
    pub disabled: bool,
}

impl SigAltStack {
    pub const fn disabled() -> Self {
        Self {
            sp: 0,
            size: 0,
            disabled: true,
        }
    }

    pub fn contains(self, sp: usize) -> bool {
        !self.disabled
            && self
                .sp
                .checked_add(self.size)
                .is_some_and(|end| sp >= self.sp && sp < end)
    }
}

impl Default for SigAltStack {
    fn default() -> Self {
        Self::disabled()
    }
}

/// 任务资源使用快照。
///
/// 当前调度器尚未区分用户态/内核态执行时间，因此先把任务生命周期内的可运行
/// 时间计入 `user_ns`，`system_ns` 保持 0。接口按结构化字段提供给 syscall
/// 兼容层，后续接入更细的 trap/syscall 记账时无需改动 wait/getrusage ABI。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskUsage {
    pub user_ns: u64,
    pub system_ns: u64,
    pub minflt: u64,
    pub majflt: u64,
    pub voluntary_ctxt_switches: u64,
    pub involuntary_ctxt_switches: u64,
}

impl TaskUsage {
    pub fn add_assign(&mut self, rhs: Self) {
        self.user_ns = self.user_ns.saturating_add(rhs.user_ns);
        self.system_ns = self.system_ns.saturating_add(rhs.system_ns);
        self.minflt = self.minflt.saturating_add(rhs.minflt);
        self.majflt = self.majflt.saturating_add(rhs.majflt);
        self.voluntary_ctxt_switches = self
            .voluntary_ctxt_switches
            .saturating_add(rhs.voluntary_ctxt_switches);
        self.involuntary_ctxt_switches = self
            .involuntary_ctxt_switches
            .saturating_add(rhs.involuntary_ctxt_switches);
    }
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

/// 性能剖析使用的阻塞原因。它属于调度语义，不依赖具体等待队列实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WaitReason {
    SocketRead = 0,
    SocketWrite,
    Poll,
    Mutex,
    Futex,
    Timer,
    Yield,
    Other,
}

impl WaitReason {
    #[cfg(feature = "performance-profile")]
    fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::SocketRead,
            1 => Self::SocketWrite,
            2 => Self::Poll,
            3 => Self::Mutex,
            4 => Self::Futex,
            5 => Self::Timer,
            6 => Self::Yield,
            _ => Self::Other,
        }
    }

    #[cfg(feature = "performance-profile")]
    fn profile_event(self) -> profiling::Event {
        match self {
            Self::SocketRead => profiling::Event::WaitSocketRead,
            Self::SocketWrite => profiling::Event::WaitSocketWrite,
            Self::Poll => profiling::Event::WaitPoll,
            Self::Mutex => profiling::Event::WaitMutex,
            Self::Futex => profiling::Event::WaitFutex,
            Self::Timer => profiling::Event::WaitTimer,
            Self::Yield => profiling::Event::WaitYield,
            Self::Other => profiling::Event::WaitOther,
        }
    }
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

/// 任务来源类型。
///
/// 用户态任务参与 POSIX 信号、进程组和 wait 语义；内核线程/idle 只属于调度器
/// 内部，不应被用户态 signal/exit_group/wait 路径影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TaskKind {
    User = 0,
    KernelThread = 1,
    Idle = 2,
}

impl TaskKind {
    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::KernelThread,
            2 => Self::Idle,
            _ => Self::User,
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

    /// 逻辑栈底（低地址端）。
    pub fn start(&self) -> usize {
        self.base.as_ptr() as usize
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
    /// 用户任务和内核任务的生命周期域不同。该字段用于把内核线程从 POSIX
    /// signal/exit_group/wait 模型中隔离出去。
    kind: AtomicU8,
    state: AtomicU8,
    exit_code: AtomicI32,
    has_exit_code: AtomicU8,
    exit_reason: AtomicU8,
    exit_signal_number: AtomicI32,
    wait_stop_sig: AtomicI32,
    wait_stop_pending: AtomicU8,
    wait_continue_pending: AtomicU8,
    root_pid_cache: AtomicI32,
    tgid_cache: AtomicI32,
    /// ptrace 的最小状态位。当前只区分任务是否处于 traced 模式，用于把
    /// 信号投递转换成父进程可 wait 的 signal-delivery-stop。
    ptrace_traced: AtomicU8,
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
    /// PI futex 的基础调度属性和当前 donation。该状态与 sched entity 分开保存，
    /// 这样解除最后一个 donation 时可以恢复用户实际设置的基础属性。
    pi: Spinlock<PiState>,
    /// `rseq` 注册状态与尚未在返回用户态边界消费的调度事件。
    rseq: Spinlock<RseqRegistration>,
    rseq_events: AtomicU8,
    /// 最近一次实际向用户态发布的 CPU；`usize::MAX` 表示尚未发布。
    rseq_cpu: AtomicUsize,
    /// `sigaltstack(2)` 的 per-thread alternate signal stack。
    sigaltstack: Spinlock<SigAltStack>,
    /// `PR_SET_NAME` / `PR_GET_NAME` 暴露的 per-thread comm。
    comm: Spinlock<[u8; TASK_COMM_LEN]>,
    /// 调度器累计的真实 CPU 运行时间；剖析开关只控制事件采样，不改变记账语义。
    cpu_runtime_ns: AtomicU64,
    running_since_ns: AtomicU64,
    /// 任务退出时冻结的自身 usage。非 Zombie 任务按当前时间动态计算。
    exited_usage_ns: AtomicU64,
    /// 当前阻塞起点、被唤醒时刻和原因；只存在于剖析构建。
    #[cfg(feature = "performance-profile")]
    wait_started_ns: AtomicU64,
    #[cfg(feature = "performance-profile")]
    wakeup_ns: AtomicU64,
    #[cfg(feature = "performance-profile")]
    wait_reason: AtomicU8,
    /// 已被本任务 reap 的子任务 usage 累计。
    child_usage: Spinlock<TaskUsage>,
    voluntary_ctxt_switches: AtomicU64,
    involuntary_ctxt_switches: AtomicU64,
    /// SCHED_RESET_ON_FORK 标志。fork/clone 子进程继承时由 spawn 路径消费。
    sched_reset_on_fork: AtomicBool,
    /// CPU 亲和性位图。预留单 word，单 CPU 原型暂不强制。
    cpu_affinity: AtomicU64,
    /// 最近一次绑定或运行的 CPU。迁移/唤醒选择使用，默认 0。
    current_cpu: AtomicUsize,
    /// CPU、调度域、拓扑代际和迁移状态的一致调度归属快照。
    placement: TaskPlacement,
    /// Linux ioprio ABI 保存值。调度器暂不消费，但 syscall get/set 需保持一致。
    ioprio: AtomicU32,
    /// 当前任务挂载的运行时执行状态裸指针。
    ///
    /// 指针的所有权始终由 `TASKEXT_ELM_EXECUTION` 对应的 `Arc` 持有；这里仅为
    /// trap/IRQ 热路径提供无锁查询，避免中断打断 TaskExt 自旋锁后再次取锁。
    elm_execution_ptr: AtomicUsize,
    /// 子系统侧表：VFS context / fdtable 等通过 [`TaskExtKey`] 挂载。
    /// 详见模块级 [`TaskExtCloneHook`]。
    ///
    /// lmbench 的 read/write/stat/mmap 热路径会极高频查询 fdtable/vfs/vm。
    /// 这些固定 key 走独立槽位，避免每次 syscall 都锁 Vec 并线性查找；
    /// 其它低频扩展仍保留在 ext 表中，兼容原有 hook 机制。
    hot_ext: HotTaskExt,
    ext: Spinlock<Vec<TaskExt>>,
    /// 退出清理是否已经运行。exit、最终切换和 wait/reap 都可能观察到同一个任务，
    /// 必须只让上层 ext hook 执行一次。
    ext_exit_cleaned: AtomicBool,
    /// 进入退出态前的用户态线程 ABI 清理是否已经运行。该阶段仍可访问用户地址空间，
    /// 用于 clear-child-tid、robust futex 等必须在释放 VM 前完成的动作。
    pre_exit_cleaned: AtomicBool,
}

#[kernel_symbols::export]
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
        TASK_CREATED.fetch_add(1, Ordering::Relaxed);
        TASK_LIVE.fetch_add(1, Ordering::Relaxed);
        let task = Arc::new(Self {
            sched: SchedEntity::new(params),
            kind: AtomicU8::new(TaskKind::User as u8),
            state: AtomicU8::new(TaskState::New as u8),
            exit_code: AtomicI32::new(0),
            has_exit_code: AtomicU8::new(0),
            exit_reason: AtomicU8::new(EXIT_REASON_NONE),
            exit_signal_number: AtomicI32::new(0),
            wait_stop_sig: AtomicI32::new(0),
            wait_stop_pending: AtomicU8::new(0),
            wait_continue_pending: AtomicU8::new(0),
            root_pid_cache: AtomicI32::new(crate::pid::PID_INVALID),
            tgid_cache: AtomicI32::new(crate::pid::PID_INVALID),
            ptrace_traced: AtomicU8::new(0),
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
            pi: Spinlock::new(PiState::new(SchedAttr::from(params))),
            rseq: Spinlock::new(RseqRegistration::default()),
            rseq_events: AtomicU8::new(0),
            rseq_cpu: AtomicUsize::new(usize::MAX),
            sigaltstack: Spinlock::new(SigAltStack::default()),
            comm: Spinlock::new(DEFAULT_COMM),
            cpu_runtime_ns: AtomicU64::new(0),
            running_since_ns: AtomicU64::new(0),
            exited_usage_ns: AtomicU64::new(0),
            #[cfg(feature = "performance-profile")]
            wait_started_ns: AtomicU64::new(0),
            #[cfg(feature = "performance-profile")]
            wakeup_ns: AtomicU64::new(0),
            #[cfg(feature = "performance-profile")]
            wait_reason: AtomicU8::new(WaitReason::Other as u8),
            child_usage: Spinlock::new(TaskUsage::default()),
            voluntary_ctxt_switches: AtomicU64::new(0),
            involuntary_ctxt_switches: AtomicU64::new(0),
            sched_reset_on_fork: AtomicBool::new(false),
            cpu_affinity: AtomicU64::new(u64::MAX),
            current_cpu: AtomicUsize::new(0),
            placement: TaskPlacement::unbound(),
            ioprio: AtomicU32::new(0),
            elm_execution_ptr: AtomicUsize::new(0),
            hot_ext: HotTaskExt::new(),
            ext: Spinlock::new(Vec::new()),
            ext_exit_cleaned: AtomicBool::new(false),
            pre_exit_cleaned: AtomicBool::new(false),
        });
        TASK_TRACKER.lock().push(Arc::downgrade(&task));
        task
    }

    pub fn state(&self) -> TaskState {
        TaskState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn kind(&self) -> TaskKind {
        TaskKind::from_u8(self.kind.load(Ordering::Acquire))
    }

    pub fn is_user_task(&self) -> bool {
        self.kind() == TaskKind::User
    }

    pub fn is_kernel_task(&self) -> bool {
        !self.is_user_task()
    }

    pub fn is_idle_task(&self) -> bool {
        self.kind() == TaskKind::Idle
    }

    pub(crate) fn mark_kernel_thread(&self) {
        self.kind
            .store(TaskKind::KernelThread as u8, Ordering::Release);
    }

    pub(crate) fn mark_idle_task(&self) {
        self.kind.store(TaskKind::Idle as u8, Ordering::Release);
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

    pub fn enable_ptrace_traced(&self) -> bool {
        self.ptrace_traced
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn clear_ptrace_traced(&self) {
        self.ptrace_traced.store(0, Ordering::Release);
    }

    pub fn is_ptrace_traced(&self) -> bool {
        self.ptrace_traced.load(Ordering::Acquire) != 0
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
        self.exited_usage_ns.store(
            self.elapsed_usage_ns(crate::scheduler::now_ns_public()),
            Ordering::Release,
        );
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
        loop {
            let candidate = self
                .snapshot_children()
                .into_iter()
                .find(|c| c.state() == TaskState::Zombie && pred(c))?;
            let mut rel = self.rel.lock();
            let Some(pos) = rel.children.iter().position(|c| Arc::ptr_eq(c, &candidate)) else {
                continue;
            };
            if rel.children[pos].state() != TaskState::Zombie {
                continue;
            }
            let zombie = rel.children.swap_remove(pos);
            zombie.set_state(TaskState::Dead);
            return Some(zombie);
        }
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

    /// 当前任务拥有的完整内核栈地址范围。
    pub fn kernel_stack_bounds(&self) -> Option<(usize, usize)> {
        self.kstack
            .lock()
            .as_ref()
            .map(|stack| (stack.start(), stack.top()))
    }

    /// 释放已经不会再运行的任务执行上下文。
    ///
    /// 只能在任务最终切离 CPU 后调用。Zombie 仍需保留 wait/proc 可见的轻量状态，
    /// 但内核栈和 arch context 已不再需要，继续挂在 Task 上会让父进程 wait 前
    /// 每个 zombie 至少保留一段内核栈。
    pub(crate) fn retire_execution(&self) {
        *self.ctx.lock() = None;
        *self.kstack.lock() = None;
    }

    /// 释放退出任务不再需要的上层扩展状态。
    ///
    /// 这个动作必须幂等：当前任务最终切离 CPU、父进程 wait/reap、以及非当前任务被
    /// exit_group 杀掉时都可能来到这里。wait 可见的 pid/exit status/comm 等轻量状态
    /// 保留在 Task 本体中，VM/FDT/VFS 等重量级状态交给 kernel 注册的 hook 移除。
    pub(crate) fn cleanup_exit_extensions(self: &Arc<Self>) {
        if self.ext_exit_cleaned.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(hook) = ext_exit_hook() {
            hook.cleanup_on_exit(self);
        }
    }

    /// 在任务进入 Zombie/Dead 前运行一次上层用户态退出清理。
    ///
    /// 与 [`cleanup_exit_extensions`] 不同，这个 hook 必须在 VmSpace/FdTable 等
    /// 上层扩展仍挂在 task 上时执行；robust futex 和 CLONE_CHILD_CLEARTID 都依赖
    /// 这个时序。
    pub(crate) fn cleanup_before_exit(self: &Arc<Self>) {
        if self.pre_exit_cleaned.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(hook) = pre_exit_hook() {
            hook.cleanup_before_exit(self);
        }
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
        if rel.pid_in_ns.len() == 1 {
            self.root_pid_cache.store(pid, Ordering::Release);
        }
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
    #[kernel_symbols::export(
        name = "sched.task.Task.pid_root",
        contract = "kernel.sched.task-query@1",
        version = 1,
        capabilities = kernel_symbols::capability::SCHED_QUERY
    )]
    pub fn pid_root(&self) -> Option<PidT> {
        self.rel.lock().pid_in_ns.first().map(|(_, pid)| *pid)
    }

    /// 任务在根 ns 中的 pid 快照。热路径使用，避免只读查询进入亲缘锁。
    pub fn pid_root_cached(&self) -> Option<PidT> {
        let pid = self.root_pid_cache.load(Ordering::Acquire);
        if pid > crate::pid::PID_INVALID {
            Some(pid)
        } else {
            None
        }
    }

    pub fn set_tgid_cache(&self, pid: PidT) {
        if pid > crate::pid::PID_INVALID {
            self.tgid_cache.store(pid, Ordering::Release);
        }
    }

    pub fn tgid_cached(&self) -> Option<PidT> {
        let pid = self.tgid_cache.load(Ordering::Acquire);
        if pid > crate::pid::PID_INVALID {
            Some(pid)
        } else {
            None
        }
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

    pub fn shared_signal_pending_bits_quick(&self) -> u64 {
        self.shared_signal.lock().pending_snapshot().raw()
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
        self.rseq_events.store(0, Ordering::Release);
        self.rseq_cpu.store(usize::MAX, Ordering::Release);
        *self.rseq.lock() = registration;
    }

    pub fn clear_rseq_registration(&self) {
        *self.rseq.lock() = RseqRegistration::default();
        self.rseq_events.store(0, Ordering::Release);
        self.rseq_cpu.store(usize::MAX, Ordering::Release);
    }

    pub fn mark_rseq_event(&self, event: RseqEvent) {
        if self.rseq_registration().registered {
            self.rseq_events.fetch_or(event as u8, Ordering::AcqRel);
        }
    }

    pub fn rseq_events(&self) -> RseqEvents {
        RseqEvents::from_bits(self.rseq_events.load(Ordering::Acquire))
    }

    pub fn clear_rseq_events(&self, events: RseqEvents) {
        self.rseq_events.fetch_and(!events.bits(), Ordering::AcqRel);
    }

    pub fn publish_rseq_cpu(&self, cpu_id: usize) {
        if !self.rseq_registration().registered {
            return;
        }
        let previous = self.rseq_cpu.swap(cpu_id, Ordering::AcqRel);
        if previous != usize::MAX && previous != cpu_id {
            self.mark_rseq_event(RseqEvent::Migrate);
        }
    }

    pub fn sigaltstack(&self) -> SigAltStack {
        *self.sigaltstack.lock()
    }

    pub fn set_sigaltstack(&self, stack: SigAltStack) {
        *self.sigaltstack.lock() = stack;
    }

    pub fn clear_sigaltstack(&self) {
        *self.sigaltstack.lock() = SigAltStack::disabled();
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

    fn elapsed_usage_ns(&self, now_ns: u64) -> u64 {
        let frozen = self.exited_usage_ns.load(Ordering::Acquire);
        if frozen != 0 {
            return frozen;
        }
        self.cpu_runtime_ns(now_ns)
    }

    /// 当前任务的真实 CPU 执行时间快照。
    pub fn cpu_runtime_ns(&self, now_ns: u64) -> u64 {
        let accumulated = self.cpu_runtime_ns.load(Ordering::Acquire);
        let encoded = self.running_since_ns.load(Ordering::Acquire);
        if encoded == 0 {
            accumulated
        } else {
            accumulated.saturating_add(now_ns.saturating_sub(encoded - 1))
        }
    }

    #[inline]
    pub(crate) fn account_switch_in(&self, now_ns: u64) {
        self.running_since_ns
            .store(now_ns.saturating_add(1), Ordering::Release);
        #[cfg(feature = "performance-profile")]
        {
            let encoded = self.wakeup_ns.swap(0, Ordering::AcqRel);
            if encoded != 0 {
                profiling::record_duration(
                    profiling::Event::WakeupLatency,
                    now_ns.saturating_sub(encoded - 1),
                );
            }
        }
    }

    #[inline]
    pub(crate) fn account_switch_out(&self, now_ns: u64) {
        let encoded = self.running_since_ns.swap(0, Ordering::AcqRel);
        if encoded != 0 {
            self.cpu_runtime_ns
                .fetch_add(now_ns.saturating_sub(encoded - 1), Ordering::AcqRel);
        }
    }

    #[inline]
    pub fn begin_profile_wait(&self, reason: WaitReason, now_ns: u64) {
        #[cfg(feature = "performance-profile")]
        {
            self.wait_reason.store(reason as u8, Ordering::Release);
            self.wakeup_ns.store(0, Ordering::Release);
            self.wait_started_ns
                .store(now_ns.saturating_add(1), Ordering::Release);
        }
        #[cfg(not(feature = "performance-profile"))]
        let _ = (reason, now_ns);
    }

    #[inline]
    pub fn cancel_profile_wait(&self) {
        #[cfg(feature = "performance-profile")]
        {
            self.wait_started_ns.store(0, Ordering::Release);
            self.wakeup_ns.store(0, Ordering::Release);
        }
    }

    #[inline]
    pub fn mark_profile_woken(&self, now_ns: u64) {
        #[cfg(feature = "performance-profile")]
        {
            let encoded = self.wait_started_ns.swap(0, Ordering::AcqRel);
            if encoded == 0 {
                return;
            }
            #[cfg(feature = "performance-profile")]
            profiling::record_duration(
                WaitReason::from_u8(self.wait_reason.load(Ordering::Acquire)).profile_event(),
                now_ns.saturating_sub(encoded - 1),
            );
            self.wakeup_ns
                .store(now_ns.saturating_add(1), Ordering::Release);
        }
        #[cfg(not(feature = "performance-profile"))]
        let _ = now_ns;
    }

    pub fn usage_snapshot(&self, now_ns: u64) -> TaskUsage {
        TaskUsage {
            user_ns: self.elapsed_usage_ns(now_ns),
            system_ns: 0,
            minflt: 0,
            majflt: 0,
            voluntary_ctxt_switches: self.voluntary_ctxt_switches.load(Ordering::Acquire),
            involuntary_ctxt_switches: self.involuntary_ctxt_switches.load(Ordering::Acquire),
        }
    }

    pub fn child_usage_snapshot(&self) -> TaskUsage {
        *self.child_usage.lock()
    }

    pub fn add_child_usage(&self, usage: TaskUsage) {
        self.child_usage.lock().add_assign(usage);
    }

    pub fn record_voluntary_context_switch(&self) {
        self.voluntary_ctxt_switches.fetch_add(1, Ordering::AcqRel);
    }

    pub fn record_involuntary_context_switch(&self) {
        self.involuntary_ctxt_switches
            .fetch_add(1, Ordering::AcqRel);
    }

    pub fn sched_reset_on_fork(&self) -> bool {
        self.sched_reset_on_fork.load(Ordering::Acquire)
    }

    pub fn set_sched_reset_on_fork(&self, enabled: bool) {
        self.sched_reset_on_fork.store(enabled, Ordering::Release);
    }

    /// 设置用户请求的基础调度属性，同时重新计算 PI donation 后的有效属性。
    /// runqueue 锁由调用方持有；这里不直接改变队列索引。
    pub(crate) fn set_sched_attr(&self, attr: SchedAttr) {
        let effective = {
            let mut pi = self.pi.lock();
            pi.base = attr.normalized();
            pi.effective()
        };
        self.sched.set_sched_attr(effective);
    }

    pub(crate) fn set_sched_params(&self, params: SchedParams) {
        let effective = {
            let mut pi = self.pi.lock();
            if pi.donations.is_empty() {
                pi.base = self.sched.sched_attr();
            }
            pi.base.nice = params.nice;
            pi.base.slice_ns = params.slice();
            pi.base = pi.base.normalized();
            pi.effective()
        };
        self.sched.set_sched_attr(effective);
    }

    pub(crate) fn set_nice(&self, nice: i8) {
        let effective = {
            let mut pi = self.pi.lock();
            if pi.donations.is_empty() {
                pi.base = self.sched.sched_attr();
            }
            pi.base.nice = nice;
            pi.base = pi.base.normalized();
            pi.effective()
        };
        self.sched.set_sched_attr(effective);
    }

    /// 登记一个 PI waiter 的 donation，返回 owner 应采用的有效属性。
    pub fn pi_add_donation(&self, token: usize, attr: SchedAttr) -> SchedAttr {
        let mut pi = self.pi.lock();
        if let Some(existing) = pi.donations.iter_mut().find(|d| d.token == token) {
            existing.attr = attr.normalized();
        } else {
            pi.donations.push(PiDonation {
                token,
                attr: attr.normalized(),
            });
        }
        pi.effective()
    }

    /// 移除一个 PI waiter 的 donation，返回 owner 恢复后的有效属性。
    pub fn pi_remove_donation(&self, token: usize) -> SchedAttr {
        let mut pi = self.pi.lock();
        pi.donations.retain(|donation| donation.token != token);
        pi.effective()
    }

    pub fn pi_effective_attr(&self) -> SchedAttr {
        self.pi.lock().effective()
    }

    pub fn pi_base_attr(&self) -> SchedAttr {
        self.pi.lock().base
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

    pub fn placement(&self) -> PlacementSnapshot {
        self.placement.snapshot()
    }

    pub(crate) fn bind_placement(
        &self,
        cpu: crate::CpuId,
        domain_id: usize,
        topology_generation: u64,
    ) {
        self.placement.bind(cpu, domain_id, topology_generation);
        self.set_current_cpu(cpu.get());
    }

    pub(crate) fn begin_migration(&self, source: PlacementSnapshot) -> bool {
        self.placement.begin_migration(source)
    }

    pub(crate) fn begin_offline_repair(&self, source: PlacementSnapshot) -> bool {
        self.placement.begin_offline_repair(source)
    }

    pub(crate) fn commit_migration(
        &self,
        cpu: crate::CpuId,
        domain_id: usize,
        topology_generation: u64,
    ) {
        self.placement
            .store_bound(cpu, domain_id, topology_generation);
        self.set_current_cpu(cpu.get());
    }

    pub(crate) fn refresh_placement_topology(
        &self,
        source: PlacementSnapshot,
        domain_id: usize,
        topology_generation: u64,
    ) -> bool {
        self.placement
            .refresh_topology(source, domain_id, topology_generation)
    }

    pub(crate) fn rollback_migration(&self, source: PlacementSnapshot) {
        self.placement.rollback(source);
    }

    pub(crate) fn unbind_placement(&self) {
        self.placement.unbind();
    }

    pub fn ioprio(&self) -> u16 {
        self.ioprio.load(Ordering::Acquire) as u16
    }

    pub fn set_ioprio(&self, value: u16) {
        self.ioprio.store(value as u32, Ordering::Release);
    }

    // ── 子系统侧表（VFS / FdTable 等） ───────────────────────────────────

    /// 安装一个子系统状态。同 key 重复装入视作配置错误（debug_assert）。
    pub fn ext_install(&self, key: TaskExtKey, payload: Arc<dyn Any + Send + Sync>) {
        let execution_ptr = if key == TASKEXT_ELM_EXECUTION {
            Arc::as_ptr(&payload) as *const () as usize
        } else {
            0
        };
        if self.hot_ext.install(key, &payload) {
            if execution_ptr != 0 {
                self.elm_execution_ptr
                    .store(execution_ptr, Ordering::Release);
            }
            return;
        }
        let mut ext = self.ext.lock();
        debug_assert!(
            !ext.iter().any(|e| e.key == key),
            "[sched][ext] key 0x{:x} already installed",
            key,
        );
        ext.push(TaskExt { key, payload });
        if execution_ptr != 0 {
            self.elm_execution_ptr
                .store(execution_ptr, Ordering::Release);
        }
    }

    /// 查询某个子系统状态；不存在返回 `None`。
    pub fn ext_lookup(&self, key: TaskExtKey) -> Option<Arc<dyn Any + Send + Sync>> {
        if let Some(payload) = self.hot_ext.lookup(key) {
            return Some(payload);
        }
        self.ext
            .lock()
            .iter()
            .find(|e| e.key == key)
            .map(|e| Arc::clone(&e.payload))
    }

    /// 移除并返回某个子系统状态。
    pub fn ext_remove(&self, key: TaskExtKey) -> Option<Arc<dyn Any + Send + Sync>> {
        if key == TASKEXT_ELM_EXECUTION {
            self.elm_execution_ptr.store(0, Ordering::Release);
        }
        if let Some(payload) = self.hot_ext.remove(key) {
            return Some(payload);
        }
        let mut ext = self.ext.lock();
        let pos = ext.iter().position(|e| e.key == key)?;
        Some(ext.swap_remove(pos).payload)
    }

    /// 列出当前所有子系统挂载，便于 fork 时遍历。
    pub fn ext_snapshot(&self) -> Vec<(TaskExtKey, Arc<dyn Any + Send + Sync>)> {
        let mut out = self.hot_ext.snapshot();
        out.extend(
            self.ext
                .lock()
                .iter()
                .map(|e| (e.key, Arc::clone(&e.payload))),
        );
        out
    }

    /// 返回当前任务的 ELM 执行状态地址。
    ///
    /// 返回值只在当前任务仍持有 `TASKEXT_ELM_EXECUTION` 时有效。调用方只能在
    /// 当前任务上下文或 trap/IRQ 现场临时解引用，不能跨调度点保存。
    pub fn elm_execution_ptr(&self) -> usize {
        self.elm_execution_ptr.load(Ordering::Acquire)
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        TASK_DROPPED.fetch_add(1, Ordering::Relaxed);
        TASK_LIVE.fetch_sub(1, Ordering::Relaxed);
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
/// RISC-V64 用户态 Vector 上下文（arch 专用，按线程独立保存）。
pub const TASKEXT_RISCV_VECTOR_STATE: TaskExtKey = 0x0001_0004;
/// RISC-V64 信号投递期间暂存的 Vector 上下文栈。
pub const TASKEXT_RISCV_VECTOR_SIGNAL_STACK: TaskExtKey = 0x0001_0005;
/// 当前任务的可执行路径（kernel execve 安装，procfs `/proc/self/exe` 读取）。
pub const TASKEXT_EXEC_PATH: TaskExtKey = 0x0002_0000;
/// 当前任务的 argv 快照（kernel execve 安装，procfs `/proc/[pid]/cmdline` 读取）。
pub const TASKEXT_EXEC_ARGS: TaskExtKey = 0x0002_0001;
/// 当前任务的 envp 快照（kernel execve 安装，procfs `/proc/[pid]/environ` 读取）。
pub const TASKEXT_EXEC_ENVP: TaskExtKey = 0x0002_0002;
/// ELM 当前执行域、恢复帧和生命周期上下文。
pub const TASKEXT_ELM_EXECUTION: TaskExtKey = 0x0003_0000;

struct HotTaskExt {
    vfs_context: Spinlock<Option<Arc<dyn Any + Send + Sync>>>,
    fdtable: Spinlock<Option<Arc<dyn Any + Send + Sync>>>,
    vm_space: Spinlock<Option<Arc<dyn Any + Send + Sync>>>,
    user_trap_frame: Spinlock<Option<Arc<dyn Any + Send + Sync>>>,
}

impl HotTaskExt {
    const fn new() -> Self {
        Self {
            vfs_context: Spinlock::new(None),
            fdtable: Spinlock::new(None),
            vm_space: Spinlock::new(None),
            user_trap_frame: Spinlock::new(None),
        }
    }

    fn slot(&self, key: TaskExtKey) -> Option<&Spinlock<Option<Arc<dyn Any + Send + Sync>>>> {
        match key {
            TASKEXT_VFS_CONTEXT => Some(&self.vfs_context),
            TASKEXT_VFS_FDTABLE => Some(&self.fdtable),
            TASKEXT_VM_SPACE => Some(&self.vm_space),
            TASKEXT_USER_TRAP_FRAME => Some(&self.user_trap_frame),
            _ => None,
        }
    }

    fn install(&self, key: TaskExtKey, payload: &Arc<dyn Any + Send + Sync>) -> bool {
        let Some(slot) = self.slot(key) else {
            return false;
        };
        let mut guard = slot.lock();
        debug_assert!(
            guard.is_none(),
            "[sched][ext] key 0x{:x} already installed",
            key
        );
        *guard = Some(Arc::clone(payload));
        true
    }

    fn lookup(&self, key: TaskExtKey) -> Option<Arc<dyn Any + Send + Sync>> {
        self.slot(key)
            .and_then(|slot| slot.lock().as_ref().map(Arc::clone))
    }

    fn remove(&self, key: TaskExtKey) -> Option<Arc<dyn Any + Send + Sync>> {
        self.slot(key).and_then(|slot| slot.lock().take())
    }

    fn snapshot(&self) -> Vec<(TaskExtKey, Arc<dyn Any + Send + Sync>)> {
        let mut out = Vec::new();
        for (key, slot) in [
            (TASKEXT_VFS_CONTEXT, &self.vfs_context),
            (TASKEXT_VFS_FDTABLE, &self.fdtable),
            (TASKEXT_VM_SPACE, &self.vm_space),
            (TASKEXT_USER_TRAP_FRAME, &self.user_trap_frame),
        ] {
            if let Some(payload) = slot.lock().as_ref() {
                out.push((key, Arc::clone(payload)));
            }
        }
        out
    }
}

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

/// task 进入退出态时由上层释放跨子系统的大对象。
///
/// sched crate 不认识 VmSpace / FdTable / VfsContext 的具体类型，只负责在
/// `exit_task` 的安全边界调用此 hook。hook 不应释放内核栈和 arch context：
/// 当前任务可能正运行在这段栈上，最终释放由调度切换后的 Arc drop 完成。
pub trait TaskExtExitHook: Send + Sync {
    fn cleanup_on_exit(&self, task: &Arc<Task>);
}

/// task 进入退出态前由上层执行用户态 ABI 清理。
pub trait TaskPreExitHook: Send + Sync {
    fn cleanup_before_exit(&self, task: &Arc<Task>);
}

static EXT_CLONE_HOOK: Spinlock<Option<&'static dyn TaskExtCloneHook>> = Spinlock::new(None);
static EXT_EXIT_HOOK: Spinlock<Option<&'static dyn TaskExtExitHook>> = Spinlock::new(None);
static PRE_EXIT_HOOK: Spinlock<Option<&'static dyn TaskPreExitHook>> = Spinlock::new(None);

/// 注册全局 ext clone hook。kernel 启动期调用一次。
pub fn register_ext_clone_hook(hook: &'static dyn TaskExtCloneHook) {
    *EXT_CLONE_HOOK.lock() = Some(hook);
}

/// 取已注册的 hook；未注册返回 `None`。
pub fn ext_clone_hook() -> Option<&'static dyn TaskExtCloneHook> {
    *EXT_CLONE_HOOK.lock()
}

/// 注册全局 ext exit hook。kernel 启动期调用一次。
pub fn register_ext_exit_hook(hook: &'static dyn TaskExtExitHook) {
    *EXT_EXIT_HOOK.lock() = Some(hook);
}

/// 取已注册的 exit hook；未注册返回 `None`。
pub fn ext_exit_hook() -> Option<&'static dyn TaskExtExitHook> {
    *EXT_EXIT_HOOK.lock()
}

/// 注册全局 pre-exit hook。kernel 启动期调用一次。
pub fn register_pre_exit_hook(hook: &'static dyn TaskPreExitHook) {
    *PRE_EXIT_HOOK.lock() = Some(hook);
}

/// 取已注册的 pre-exit hook；未注册返回 `None`。
pub fn pre_exit_hook() -> Option<&'static dyn TaskPreExitHook> {
    *PRE_EXIT_HOOK.lock()
}
