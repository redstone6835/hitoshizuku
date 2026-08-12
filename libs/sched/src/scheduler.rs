//! 全局调度入口：init / per-CPU 状态 / schedule_once / on_timer_tick / idle。
//!
//! 这层把 sched crate 的"系统级状态"集中起来。kernel 启动期调用 [`init`]
//! 建立 init 任务；之后所有 syscall / 内核线程调度都走这里的
//! [`schedule_once`]，定时器中断走 [`on_timer_tick`]。
//!
//! ## 启动期契约
//!
//! [`init`] 必须在 [`crate::arch_hooks::register`] 之后调用——否则 `Task::new`
//! 在分配 ArchContextSlot 时会 panic。本层不强制 arch 注入（保持 libs 层不
//! 依赖 arch crate），由上层 `kernel::sched::boot_init` 负责次序。
//!
//! ## per-CPU 结构
//!
//! [`Scheduler`](crate::scheduler_state::Scheduler) 统一拥有 topology、
//! online/active CPU 集和每 CPU 运行状态；用 [`current_cpu_id`] 选择本地 CPU 槽。
//! AP 启动尚未落地，当前永远只有 CPU 0 被填充。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, Ordering};

use crate::arch_hooks;
use crate::cpu::{CpuId, CpuMask, MAX_CPUS, SchedPlacement, SchedTopology};
use crate::deadline_admission::{DeadlineAdmission, utilization_of};
use crate::eevdf::{NICE_0_WEIGHT, SchedParams};
use crate::group::{ProcessGroup, Session, ThreadGroup};
use crate::ids::Uid;
use crate::migration::MigrationContext;
use crate::pid::PidNamespace;
use crate::placement::PlacementState;
use crate::runqueue::{Runqueue, RunqueueClassLoad};
use crate::sched_class::{
    DEFAULT_RR_SLICE_NS, DEFAULT_RT_PERIOD_NS, DEFAULT_RT_RUNTIME_NS, SchedAttr, SchedClass,
};
use crate::scheduler_state::{HandoffReason, HandoffTarget, SCHEDULER, TopologySnapshot};
use crate::signal::{DefaultAction, SigHandler, SigInfo, SignalNumber, default_action};
use crate::sync::Spinlock;
use crate::task::Task;
use crate::{ExitCode, TaskState};

// ── per-CPU 容量 ──────────────────────────────────────────────────────────────

/// 支持的最大 CPU 数。SMP 启动落地之前只有 CPU 0 真正被使用；保留更大数组
/// 是为了让锁顺序、索引代码一次到位，AP 启动接入时无需重排数据结构。
pub const NR_CPUS: usize = MAX_CPUS;

/// 一次调度决策内使用的 runqueue 负载快照。
///
/// 采样时按 CPU 逐个短暂持有 rq 锁，后续选择过程只读本地数组，避免在拓扑
/// 遍历时反复进入不同 rq 锁。快照只保证单次决策内部自洽，不表示全局静态状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunqueueLoadSnapshot {
    loads: [usize; NR_CPUS],
}

impl RunqueueLoadSnapshot {
    pub(crate) fn collect<F>(cpus: CpuMask, mut load_of: F) -> Self
    where
        F: FnMut(CpuId) -> usize,
    {
        let sampled = cpus.intersection(CpuMask::SUPPORTED);
        let mut loads = [0; NR_CPUS];
        for cpu in sampled.iter() {
            loads[cpu.get()] = load_of(cpu);
        }
        Self { loads }
    }

    pub(crate) fn load_of(&self, cpu: CpuId) -> usize {
        self.loads[cpu.get()]
    }

    fn add_task(&mut self, cpu: CpuId) {
        self.loads[cpu.get()] = self.loads[cpu.get()].saturating_add(1);
    }
}

/// 一次域平衡决策内使用的各调度类负载快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunqueueClassLoadSnapshot {
    loads: [RunqueueClassLoad; NR_CPUS],
}

impl RunqueueClassLoadSnapshot {
    pub(crate) fn collect<F>(cpus: CpuMask, mut load_of: F) -> Self
    where
        F: FnMut(CpuId) -> RunqueueClassLoad,
    {
        let sampled = cpus.intersection(CpuMask::SUPPORTED);
        let mut loads = [RunqueueClassLoad::default(); NR_CPUS];
        for cpu in sampled.iter() {
            loads[cpu.get()] = load_of(cpu);
        }
        Self { loads }
    }

    pub(crate) fn load_of(&self, cpu: CpuId) -> RunqueueClassLoad {
        self.loads[cpu.get()]
    }

    pub(crate) fn loads(&self) -> &[RunqueueClassLoad; NR_CPUS] {
        &self.loads
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SchedulerDiag {
    pub current_slots: usize,
    pub current_zombie_or_dead: usize,
    pub rq_current_slots: usize,
    pub rq_current_zombie_or_dead: usize,
    pub rq_queued_slots: usize,
    pub rq_queued_zombie_or_dead: usize,
    pub retired_tasks: usize,
    pub pid_count: usize,
    pub init_children: usize,
    pub init_zombies: usize,
}

/// 单次 clone 后让渡的轮数。这里只需要一次安全边界交接：
/// 再多轮只会放大 fork/daemon 一类短任务的 syscall 收尾开销；pthread
/// 创建不走这里，线程会自然运行到 futex 等阻塞点再交给被唤醒者。
const POST_SYSCALL_HANDOFF_ROUNDS: u8 = 1;

#[cfg(feature = "performance-profile")]
static CONTEXT_SWITCH_STARTED: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];

struct TimedSleeper {
    deadline_ns: u64,
    cpu_id: usize,
    task: Weak<Task>,
}

static TIMED_SLEEPERS: Spinlock<Vec<TimedSleeper>> = Spinlock::new(Vec::new());
static HAS_TIMED_SLEEPERS: AtomicBool = AtomicBool::new(false);

/// 由调度定时器在指定 deadline 到期后定向通知的对象。
pub trait DeadlineObserver: Send + Sync {
    fn deadline_expired(&self, registration: u64, now_ns: u64) -> Option<u64>;
}

struct DeadlineEntry {
    id: u64,
    deadline_ns: u64,
    observer: Weak<dyn DeadlineObserver>,
    firing: bool,
}

struct DeadlineObservers {
    next_id: u64,
    entries: Vec<DeadlineEntry>,
}

static DEADLINE_OBSERVERS: Spinlock<DeadlineObservers> = Spinlock::new(DeadlineObservers {
    next_id: 1,
    entries: Vec::new(),
});
static HAS_DEADLINE_OBSERVERS: AtomicBool = AtomicBool::new(false);

/// POSIX `ITIMER_REAL` 的纳秒级内核表示。
///
/// `value_ns == 0` 表示当前未 armed；`interval_ns != 0` 表示到期后按该周期重装。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RealtimeItimerSpec {
    pub value_ns: u64,
    pub interval_ns: u64,
}

struct RealtimeItimer {
    deadline_ns: u64,
    interval_ns: u64,
    cpu_id: usize,
    thread_group: Weak<ThreadGroup>,
}

static REALTIME_ITIMERS: Spinlock<Vec<RealtimeItimer>> = Spinlock::new(Vec::new());
static HAS_REALTIME_ITIMERS: AtomicBool = AtomicBool::new(false);
static DEADLINE_STATE_GENERATION: AtomicU64 = AtomicU64::new(1);
static DEADLINE_CACHE_GENERATION: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];
static DEADLINE_CACHE_VALUE: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];
static DEADLINE_CACHE_PRESENT: [AtomicBool; NR_CPUS] = [const { AtomicBool::new(false) }; NR_CPUS];
static PROGRAMMED_DEADLINE_VALUE: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];
static PROGRAMMED_DEADLINE_PRESENT: [AtomicBool; NR_CPUS] =
    [const { AtomicBool::new(false) }; NR_CPUS];
static PROGRAMMED_DEADLINE_VALID: [AtomicBool; NR_CPUS] =
    [const { AtomicBool::new(false) }; NR_CPUS];

#[inline]
fn invalidate_deadline_cache() {
    DEADLINE_STATE_GENERATION.fetch_add(1, Ordering::Release);
}
static CPU_HOTPLUG_LOCK: Spinlock<()> = Spinlock::new(());
// 跨多个 runqueue 采样时统一取得这把锁，保证所有采样者以同一顺序观察
// CPU 队列。单个 runqueue 的调度操作不取得它，避免把普通切换路径串行化。
static RUNQUEUE_SNAPSHOT_LOCK: Spinlock<()> = Spinlock::new(());

const NSEC_PER_USEC: u64 = 1_000;
const NSEC_PER_MSEC: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RtSchedulingConfig {
    period_us: i32,
    runtime_us: i32,
}

impl RtSchedulingConfig {
    pub(crate) const DEFAULT: Self = Self {
        period_us: (DEFAULT_RT_PERIOD_NS / NSEC_PER_USEC) as i32,
        runtime_us: (DEFAULT_RT_RUNTIME_NS / NSEC_PER_USEC) as i32,
    };

    pub(crate) fn with_period_us(self, value: i64) -> Result<Self, errno::Errno> {
        if !(1..=i32::MAX as i64).contains(&value)
            || (self.runtime_us >= 0 && self.runtime_us as i64 > value)
        {
            return Err(errno::Errno::EINVAL);
        }
        Ok(Self {
            period_us: value as i32,
            ..self
        })
    }

    pub(crate) fn with_runtime_us(self, value: i64) -> Result<Self, errno::Errno> {
        if !(-1..=i32::MAX as i64).contains(&value) || (value >= 0 && value > self.period_us as i64)
        {
            return Err(errno::Errno::EINVAL);
        }
        Ok(Self {
            runtime_us: value as i32,
            ..self
        })
    }

    fn bandwidth_ns(self) -> (u64, u64) {
        let period_ns = self.period_us as u64 * NSEC_PER_USEC;
        let runtime_ns = if self.runtime_us < 0 {
            period_ns
        } else {
            self.runtime_us as u64 * NSEC_PER_USEC
        };
        (period_ns, runtime_ns)
    }
}

static RT_SCHEDULING_CONFIG: Spinlock<RtSchedulingConfig> =
    Spinlock::new(RtSchedulingConfig::DEFAULT);
static SCHED_RR_TIMESLICE_MS: AtomicI32 =
    AtomicI32::new((DEFAULT_RR_SLICE_NS / NSEC_PER_MSEC) as i32);

/// init 任务全局锚点。槽位永久持有一个 Arc 强引用，读取路径无需加锁。
static INIT_TASK: AtomicPtr<Task> = AtomicPtr::new(core::ptr::null_mut());
/// init 的稳定进程身份；非 leader exec 后通过线程组 leader 动态解析。
static INIT_THREAD_GROUP: AtomicPtr<ThreadGroup> = AtomicPtr::new(core::ptr::null_mut());
/// 根 PID namespace。所有任务在分配 pid 时至少在该 ns 中登记一次。
static ROOT_PID_NS: AtomicPtr<PidNamespace> = AtomicPtr::new(core::ptr::null_mut());
static INIT_READY: AtomicBool = AtomicBool::new(false);
static DEFERRED_TIMER_TICK_NS: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];
static DEFERRED_TASK_WAKES: [AtomicPtr<Task>; NR_CPUS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; NR_CPUS];
/// 每 CPU current task 的发布代际，仅供低扰动性能影子模型识别调度插入。
#[cfg(feature = "performance-profile")]
static CURRENT_TASK_EPOCH: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];

// ── 内部辅助 ──────────────────────────────────────────────────────────────────

/// 当前 CPU id。arch_hooks 未注入时退化为 0（单核场景）。
#[inline]
fn cpu() -> usize {
    let id = arch_hooks::current_cpu_id_or_boot();
    debug_assert!(id < NR_CPUS, "[sched] cpu id {} >= NR_CPUS", id);
    if id < NR_CPUS { id } else { 0 }
}

fn publish_current_task(cpu_id: usize, task: Arc<Task>) {
    debug_assert_eq!(cpu_id, cpu(), "publishing current task for a remote CPU");
    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    let task_ptr = cpu_state.publish_current(task) as usize;
    let cpu_work_ptr = cpu_state.user_return_work_ptr() as usize;
    if let Some(trap) = arch_hooks::trap() {
        // Safety: publish_current 已经让 current/current_raw 持有 task，且该函数
        // 只在正在完成切换的本 CPU 上发布架构本地指针。CpuSchedState 位于静态
        // Scheduler 中，其 user_return_work 地址在内核运行期保持稳定。
        unsafe { (trap.set_current_task)(task_ptr, cpu_work_ptr) };
    }
    #[cfg(feature = "performance-profile")]
    {
        let epoch = &CURRENT_TASK_EPOCH[cpu_id.min(NR_CPUS - 1)];
        let value = epoch.load(Ordering::Relaxed);
        epoch.store(value.wrapping_add(1), Ordering::Relaxed);
    }
}

fn bind_task_to_cpu(task: &Task, cpu_id: usize) {
    bind_task_to_cpu_on(&SCHEDULER, task, cpu_id);
}

fn bind_task_to_cpu_on(scheduler: &crate::Scheduler, task: &Task, cpu_id: usize) {
    let cpu = CpuId::new(cpu_id).unwrap_or_else(CpuId::boot);
    let placement = task.placement();
    if placement.state == PlacementState::Bound
        && placement.cpu == Some(cpu)
        && placement.topology_generation == scheduler.topology_generation()
    {
        if task.current_cpu() != cpu.get() {
            task.set_current_cpu(cpu.get());
        }
        return;
    }
    let snapshot = scheduler.topology_snapshot();
    let domain_id = snapshot
        .topology
        .domain_for_cpu(cpu)
        .unwrap_or_else(|| snapshot.topology.root_domain())
        .id();
    task.bind_placement(cpu, domain_id, snapshot.generation);
}

#[kernel_symbols::export(name = "sched.scheduler.current_cpu_id", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn current_cpu_id() -> usize {
    cpu()
}

/// 当前 CPU 的 task 发布代际。
///
/// 每次 current task 发布后递增；per-CPU 单写允许影子性能模型用 relaxed 读取
/// 判断窗口之间是否发生过调度、迁移或其它任务插入，而不在切换路径增加原子 RMW。
#[cfg(feature = "performance-profile")]
pub fn current_task_epoch() -> u64 {
    CURRENT_TASK_EPOCH[cpu()].load(Ordering::Relaxed)
}

/// 当前纳秒时间戳。未注入时返回 0，表示"不推进虚拟时间"。
#[inline]
fn now_ns_internal() -> u64 {
    arch_hooks::time().map_or(0, |o| (o.now_ns)())
}

/// 内核内部时间戳入口，不经过 ELM 导出路由。
#[inline]
pub fn now_ns_direct() -> u64 {
    now_ns_internal()
}

/// 对外导出的时间戳访问器。上层 idle / main loop 要喂 `schedule_once` 用。
#[kernel_symbols::export(name = "sched.scheduler.now_ns_public", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn now_ns_public() -> u64 {
    now_ns_direct()
}

// ── 初始化 ────────────────────────────────────────────────────────────────────

/// 构造 init 任务并登记为 CPU 0 的 current。必须在分配器可用之后、
/// [`crate::arch_hooks::register`] 之后调用，且仅调用一次。
pub fn init() -> Arc<Task> {
    assert!(
        !INIT_READY.load(Ordering::Acquire),
        "[sched] init() called more than once"
    );
    assert!(
        crate::arch_hooks::ops().is_some(),
        "[sched] init() before arch_hooks::register — call it first"
    );

    SCHEDULER.install_topology(SchedTopology::with_cpu_domains());

    // 给每个 per-CPU runqueue 回填自己的 CPU 编号，武装双重入队检测。
    // Runqueue::new() 是 const fn、在静态数组里构造，拿不到自己的下标。
    for (cpu_id, cpu_state) in SCHEDULER.cpus().iter().enumerate() {
        cpu_state.runqueue().set_owner_cpu(cpu_id);
    }

    // 1) 身份骨架：session / pgroup / thread_group 互指但未填 leader。
    let session = Session::new();
    let pgroup = ProcessGroup::new(&session);
    session.register_group(&pgroup);
    let tgroup = ThreadGroup::new();
    let root_ns = PidNamespace::new_root();

    // 2) init 没有父。
    let params = SchedParams::default_fair();
    let init_task = Task::new(
        params,
        Weak::new(),
        Arc::clone(&tgroup),
        Arc::clone(&pgroup),
    );

    // 3) 反向登记。
    tgroup.set_leader(&init_task);
    tgroup.add_member(&init_task);
    pgroup.add_member(&init_task);
    session.set_leader(&init_task);

    // 4) init 已经在 boot CPU 上执行；给它分配空 arch ctx 槽，下次切换时
    //    由汇编写入真实寄存器。
    init_task.adopt_current_context();

    // 5) 在根 ns 分配 pid=1。
    let init_pid = match root_ns.registry().allocate(&init_task) {
        Some(pid) => {
            debug_assert_eq!(pid, 1, "[sched][init] init pid must be 1");
            init_task.register_pid(Arc::clone(&root_ns), pid);
            root_ns.set_ns_init_pid(pid);
            tgroup.set_tgid(pid);
            init_task.set_tgid_cache(pid);
            pgroup.set_pgid(pid);
            session.set_sid(pid);
            pid
        }
        None => {
            log::error!("[sched][init] failed to allocate pid for init");
            crate::pid::PID_INVALID
        }
    };

    // 6) 登记为 CPU 0 的 current。其它核的 CpuSchedState 保持空槽，
    //    直到 AP 启动路径落地时各自 `adopt_current_context`。
    bind_task_to_cpu(&init_task, 0);
    assert!(
        init_task.try_claim_cpu(0),
        "[sched][init] init task CPU ownership already claimed"
    );
    SCHEDULER
        .cpu_or_boot(0)
        .runqueue()
        .set_current(Arc::clone(&init_task));

    // 7) 发布全局锚点。AtomicPtr 持有的强引用与内核同寿命，不参与常规回收。
    let init_ptr = Arc::into_raw(Arc::clone(&init_task)).cast_mut();
    let init_group_ptr = Arc::into_raw(Arc::clone(&tgroup)).cast_mut();
    let root_ptr = Arc::into_raw(Arc::clone(&root_ns)).cast_mut();
    assert!(
        INIT_TASK
            .compare_exchange(
                core::ptr::null_mut(),
                init_ptr,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
    );
    assert!(
        INIT_THREAD_GROUP
            .compare_exchange(
                core::ptr::null_mut(),
                init_group_ptr,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
    );
    assert!(
        ROOT_PID_NS
            .compare_exchange(
                core::ptr::null_mut(),
                root_ptr,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
    );
    // 8) 先完整发布 CPU 0 的 current，再对外发布调度器就绪状态。timer 可能在任意
    // 两条指令之间到来，若先设置 INIT_READY，早期观测路径会看到空 current。
    publish_current_task(0, Arc::clone(&init_task));
    init_task.account_switch_in(now_ns_internal());
    INIT_READY.store(true, Ordering::Release);
    log::info!(
        "[sched][init] init task created pid={} nr_running={} weight={}",
        init_pid,
        SCHEDULER.cpu_or_boot(0).runqueue().nr_running(),
        init_task.sched.weight(),
    );

    init_task
}

// ── 全局访问器 ────────────────────────────────────────────────────────────────

/// 获取 init 任务句柄。init 建立前调用会 panic。
#[kernel_symbols::export(name = "sched.scheduler.init_task", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn init_task() -> Arc<Task> {
    assert!(
        INIT_READY.load(Ordering::Acquire),
        "[sched] init_task() called before sched::init()"
    );
    let group = clone_global_arc(
        &INIT_THREAD_GROUP,
        "[sched] INIT_THREAD_GROUP flag set but slot empty",
    );
    group
        .leader()
        .or_else(|| {
            Some(clone_global_arc(
                &INIT_TASK,
                "[sched] INIT_TASK flag set but slot empty",
            ))
        })
        .expect("[sched] init thread group leader missing")
}

/// 当前 CPU 的 runqueue。
#[cfg(any(test, debug_assertions))]
pub(crate) fn runqueue() -> &'static Runqueue {
    SCHEDULER.cpu_or_boot(cpu()).runqueue()
}

/// 指定 CPU 的 runqueue。
pub(crate) fn runqueue_of(cpu_id: usize) -> &'static Runqueue {
    assert!(cpu_id < NR_CPUS, "[sched] runqueue cpu id out of range");
    SCHEDULER.cpu_or_boot(cpu_id).runqueue()
}

/// 根 PID namespace。
#[kernel_symbols::export(name = "sched.scheduler.root_pid_ns", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn root_pid_ns() -> Arc<PidNamespace> {
    assert!(
        INIT_READY.load(Ordering::Acquire),
        "[sched] root_pid_ns() called before sched::init()"
    );
    clone_global_arc(&ROOT_PID_NS, "[sched] ROOT_PID_NS flag set but slot empty")
}

fn clone_global_arc<T>(slot: &AtomicPtr<T>, empty_message: &str) -> Arc<T> {
    let ptr = slot.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "{}", empty_message);
    unsafe {
        // Safety: 槽位永久保留 `Arc::into_raw` 产生的强引用，因此 ptr 在内核运行期
        // 始终有效；先增加强计数，再用 from_raw 构造本次调用拥有的 Arc。
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
}

/// 统计：当前根 ns 已占用的 pid 数（含 init）。
#[kernel_symbols::export(name = "sched.scheduler.pid_count", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
pub fn pid_count() -> usize {
    root_pid_ns().registry().allocated()
}

/// 当前 CPU 上正在执行的任务。
///
/// [`init`] 之后，在 CPU 0 上必然非空。AP 启动路径落地前，其它 CPU 调用此
/// 函数会 panic（目前不会发生，因为只有 CPU 0 跑代码）。
#[inline]
pub fn current_task_direct() -> Arc<Task> {
    let id = cpu();
    SCHEDULER
        .cpu_or_boot(id)
        .current()
        .expect("[sched] current_task called before sched::init() on this CPU")
}

/// ELM 模块使用的当前任务导出入口。
#[kernel_symbols::export(name = "sched.scheduler.current_task", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn current_task() -> Arc<Task> {
    current_task_direct()
}

/// 尝试借用当前 CPU 上正在执行的任务。
///
/// 该接口不增加引用计数，也不加锁；调用方不能把返回引用保存到可能调度之后。
pub fn try_current_task_ref() -> Option<&'static Task> {
    let id = cpu();
    let ptr = SCHEDULER.cpu_or_boot(id).current_raw();
    if ptr.is_null() {
        return None;
    }
    // Safety: raw 指针由 `publish_current_task` 的 `Arc::into_raw` 产生，并由
    // CpuSchedState 的 raw current 槽位持有强引用。
    Some(unsafe { &*ptr })
}

/// 当前 CPU 上必须存在的任务引用。
///
/// 启动期、中断早期等允许尚无 current 的观测路径应使用 [`try_current_task_ref`]。
pub fn current_task_ref() -> &'static Task {
    try_current_task_ref().unwrap_or_else(|| {
        panic!("[sched] current_task_ref called before sched::init() on this CPU")
    })
}

/// 当前执行栈对调度器 current 槽中 `Arc<Task>` 的非拥有型视图。
///
/// 这等价于 Linux/C 热路径中的 borrowed `current`：构造和析构都不修改强引用
/// 计数。句柄可以随所属内核栈整体阻塞、迁移和恢复，但不可异步移交给另一执行
/// 栈或被其它 CPU 并发访问；若对象需要进入等待队列、异步工作或其它拥有型容器，
/// 调用方必须通过 [`BorrowedCurrentTask::to_arc`] 显式提升。持有借用期间禁止对
/// 该 Task 使用 `Arc::get_mut/make_mut/try_unwrap/into_inner` 等唯一所有权 API。
pub struct BorrowedCurrentTask {
    task: ManuallyDrop<Arc<Task>>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl BorrowedCurrentTask {
    /// 从调度器已经持有强引用的 current 裸指针创建借用句柄。
    ///
    /// # Safety
    /// - `ptr` 必须由本 CPU current 槽中的 `Arc::into_raw` 指针镜像得到；
    /// - 当前内核执行栈结束前，调度器必须持续保证该任务至少有一份强引用；
    /// - 返回值不得异步移交给另一执行栈或保存到并发上下文。
    #[inline(always)]
    pub unsafe fn from_current_raw(ptr: *const Task) -> Self {
        debug_assert!(!ptr.is_null(), "[sched] borrowed current pointer is null");
        Self {
            // Safety: 调用方保证 ptr 来自 current 槽持有的 Arc。ManuallyDrop
            // 只构造 Arc 的借用视图，绝不会消费该槽拥有的强引用。
            task: ManuallyDrop::new(unsafe { Arc::from_raw(ptr) }),
            _not_send_or_sync: PhantomData,
        }
    }

    /// 以现有 syscall/调度接口使用的 `&Arc<Task>` 形式借用 current。
    #[inline(always)]
    pub fn as_arc(&self) -> &Arc<Task> {
        &self.task
    }

    /// 只有跨当前同步执行边界时才取得一份真正拥有的强引用。
    #[inline]
    pub fn to_arc(&self) -> Arc<Task> {
        Arc::clone(self.as_arc())
    }
}

impl core::ops::Deref for BorrowedCurrentTask {
    type Target = Task;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.as_arc()
    }
}

/// 通过通用 per-CPU current 槽借用当前任务。
#[inline]
pub fn borrow_current_task_internal() -> BorrowedCurrentTask {
    let id = cpu();
    let ptr = SCHEDULER.cpu_or_boot(id).current_raw();
    // Safety: current_raw 指向槽位持有的 Arc；句柄不可跨 CPU，当前执行栈在
    // 调度切走期间本身也持有任务生命周期，恢复后仍回到同一任务。
    unsafe { BorrowedCurrentTask::from_current_raw(ptr) }
}

/// 当前 CPU 上正在执行的任务句柄，热路径版本。
///
/// 与 [`current_task`] 语义相同，但不进入 owning current 锁。
#[kernel_symbols::export(name = "sched.scheduler.current_task_fast", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn current_task_fast() -> Arc<Task> {
    current_task_fast_internal()
}

/// 内核常驻代码使用的 current 快入口，不经过 ELM 导出包装。
#[inline]
pub fn current_task_fast_internal() -> Arc<Task> {
    let id = cpu();
    let ptr = SCHEDULER.cpu_or_boot(id).current_raw();
    if ptr.is_null() {
        panic!("[sched] current_task_fast called before sched::init() on this CPU");
    }
    // Safety: raw 指针由 `Arc::into_raw` 发布且槽位强引用仍有效；先增加强引用，
    // 再用 `from_raw` 接管新增的这一份。
    unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
}

/// 兼容现有内核直调入口。
#[inline]
pub fn current_task_fast_direct() -> Arc<Task> {
    current_task_fast_internal()
}

/// 当前 CPU task 的根 namespace tid；启动早期没有 current 时返回 0。
pub fn current_task_id() -> u64 {
    if !INIT_READY.load(Ordering::Acquire) {
        return 0;
    }
    let ptr = SCHEDULER.cpu_or_boot(cpu()).current_raw();
    if ptr.is_null() {
        return 0;
    }
    // Safety: 非空 raw current 由 CpuSchedState 持有强引用，读取期间不会失效。
    unsafe { &*ptr }.pid_root_cached().unwrap_or(0) as u64
}

#[cfg(feature = "performance-profile")]
pub fn current_profile_session_id() -> u64 {
    if !INIT_READY.load(Ordering::Acquire) {
        return 0;
    }
    let ptr = SCHEDULER.cpu_or_boot(cpu()).current_raw();
    if ptr.is_null() {
        return 0;
    }
    // Safety: 非空 raw current 由 CpuSchedState 持有强引用，读取期间不会失效。
    unsafe { &*ptr }.profile_session_id()
}

#[cfg(feature = "performance-profile")]
pub fn current_profile_image(pc: usize) -> (u64, usize) {
    if !INIT_READY.load(Ordering::Acquire) {
        return (0, 0);
    }
    let ptr = SCHEDULER.cpu_or_boot(cpu()).current_raw();
    if ptr.is_null() {
        return (0, 0);
    }
    // Safety: 非空 raw current 由 CpuSchedState 持有强引用，读取期间不会失效。
    unsafe { &*ptr }.profile_image_for_pc(pc)
}

#[cfg(feature = "performance-profile")]
pub fn current_profile_span_id() -> u64 {
    if !INIT_READY.load(Ordering::Acquire) {
        return 0;
    }
    let ptr = SCHEDULER.cpu_or_boot(cpu()).current_raw();
    if ptr.is_null() {
        return 0;
    }
    // Safety: 非空 raw current 由 CpuSchedState 持有强引用，读取期间不会失效。
    unsafe { &*ptr }.profile_span_id()
}

#[cfg(feature = "performance-profile")]
pub fn set_current_profile_span_id(span_id: u64) {
    if !INIT_READY.load(Ordering::Acquire) {
        return;
    }
    let ptr = SCHEDULER.cpu_or_boot(cpu()).current_raw();
    if ptr.is_null() {
        return;
    }
    // Safety: 非空 raw current 由 CpuSchedState 持有强引用，写入期间不会失效。
    unsafe { &*ptr }.set_profile_span_id(span_id);
}

/// 查询指定 CPU 上的 current；未登记时返回 None。
#[kernel_symbols::export(name = "sched.scheduler.current_task_on", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn current_task_on(cpu_id: usize) -> Option<Arc<Task>> {
    SCHEDULER.cpu(cpu_id)?.current()
}

#[kernel_symbols::export(name = "sched.scheduler.scheduler_diag", contract = "kernel.sched.diagnostic@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
pub fn scheduler_diag() -> SchedulerDiag {
    let mut diag = SchedulerDiag::default();
    for cpu_state in SCHEDULER.cpus() {
        if let Some(task) = cpu_state.current() {
            diag.current_slots += 1;
            if matches!(task.state(), TaskState::Zombie | TaskState::Dead) {
                diag.current_zombie_or_dead += 1;
            }
        }
    }
    for cpu_state in SCHEDULER.cpus() {
        let rq = cpu_state.runqueue();
        if let Some(task) = rq.current() {
            diag.rq_current_slots += 1;
            if matches!(task.state(), TaskState::Zombie | TaskState::Dead) {
                diag.rq_current_zombie_or_dead += 1;
            }
        }
        let queued = rq.snapshot_runnable();
        diag.rq_queued_slots += queued.len();
        diag.rq_queued_zombie_or_dead += queued
            .iter()
            .filter(|task| matches!(task.state(), TaskState::Zombie | TaskState::Dead))
            .count();
    }
    for cpu_state in SCHEDULER.cpus() {
        diag.retired_tasks += cpu_state.retired_len();
    }
    if INIT_READY.load(Ordering::Acquire) {
        diag.pid_count = pid_count();
        let init = init_task();
        let children = init.snapshot_children();
        diag.init_children = children.len();
        diag.init_zombies = children
            .iter()
            .filter(|child| child.state() == TaskState::Zombie)
            .count();
    }
    diag
}

pub(crate) fn is_current_on_any_cpu(task: &Arc<Task>) -> bool {
    for cpu_state in SCHEDULER.cpus() {
        if cpu_state
            .current()
            .is_some_and(|current| Arc::ptr_eq(&current, task))
        {
            return true;
        }
    }
    false
}

/// 指定 CPU 上的 idle 任务句柄。
pub fn idle_task(cpu_id: usize) -> Option<Arc<Task>> {
    SCHEDULER.cpu(cpu_id)?.idle()
}

/// ELM 模块使用的调度器就绪状态导出入口。
#[kernel_symbols::export(name = "sched.scheduler.is_ready", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn is_ready() -> bool {
    is_ready_internal()
}

/// 内核常驻代码使用的初始化状态快入口，不经过 ELM 导出包装。
#[inline]
pub fn is_ready_internal() -> bool {
    INIT_READY.load(Ordering::Acquire)
}

/// 兼容现有内核直调入口。
#[inline]
pub fn is_ready_direct() -> bool {
    is_ready_internal()
}

#[kernel_symbols::export(name = "sched.scheduler.online_cpu_mask", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn online_cpu_mask() -> u64 {
    SCHEDULER.online_set().bits()
}

#[kernel_symbols::export(name = "sched.scheduler.active_cpu_mask", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn active_cpu_mask() -> u64 {
    SCHEDULER.active_set().bits()
}

#[kernel_symbols::export(name = "sched.scheduler.supported_cpu_mask", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn supported_cpu_mask() -> u64 {
    CpuMask::SUPPORTED.bits()
}

pub fn sched_rt_period_us() -> i32 {
    RT_SCHEDULING_CONFIG.lock().period_us
}

pub fn sched_rt_runtime_us() -> i32 {
    RT_SCHEDULING_CONFIG.lock().runtime_us
}

pub fn sched_rr_timeslice_ms() -> i32 {
    SCHED_RR_TIMESLICE_MS.load(Ordering::Acquire)
}

pub fn sched_rr_timeslice_ns() -> u64 {
    sched_rr_timeslice_ms() as u64 * NSEC_PER_MSEC
}

pub fn set_sched_rt_period_us(value: i64) -> Result<(), errno::Errno> {
    update_rt_bandwidth(|config| config.with_period_us(value))
}

pub fn set_sched_rt_runtime_us(value: i64) -> Result<(), errno::Errno> {
    update_rt_bandwidth(|config| config.with_runtime_us(value))
}

pub fn set_sched_rr_timeslice_ms(value: i64) -> Result<(), errno::Errno> {
    let value = normalize_rr_timeslice_ms(value)?;
    SCHED_RR_TIMESLICE_MS.store(value, Ordering::Release);
    Ok(())
}

pub(crate) fn normalize_rr_timeslice_ms(value: i64) -> Result<i32, errno::Errno> {
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&value) {
        return Err(errno::Errno::EINVAL);
    }
    Ok(if value <= 0 {
        (DEFAULT_RR_SLICE_NS / NSEC_PER_MSEC) as i32
    } else {
        value as i32
    })
}

fn update_rt_bandwidth(
    update: impl FnOnce(RtSchedulingConfig) -> Result<RtSchedulingConfig, errno::Errno>,
) -> Result<(), errno::Errno> {
    let mut config = RT_SCHEDULING_CONFIG.lock();
    let next = update(*config)?;
    let (period_ns, runtime_ns) = next.bandwidth_ns();
    let now_ns = now_ns_internal();
    for cpu_id in 0..NR_CPUS {
        runqueue_of(cpu_id).set_rt_bandwidth(period_ns, runtime_ns, now_ns);
    }
    *config = next;
    drop(config);

    let online = online_cpu_mask();
    for cpu_id in 0..NR_CPUS {
        if online & (1u64 << cpu_id) != 0 {
            request_resched(cpu_id);
        }
    }
    Ok(())
}

fn active_cpu_set() -> CpuMask {
    SCHEDULER.active_set()
}

pub(crate) fn cpu_capacity(cpu: CpuId) -> u64 {
    SCHEDULER.topology_snapshot().topology.cpu_capacity(cpu)
}

pub(crate) fn deadline_admission() -> &'static DeadlineAdmission {
    SCHEDULER.deadline_admission()
}

#[kernel_symbols::export(name = "sched.scheduler.sched_topology", contract = "kernel.sched.topology@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn sched_topology() -> SchedTopology {
    SCHEDULER.topology()
}

#[kernel_symbols::export(name = "sched.scheduler.install_sched_topology", contract = "kernel.sched.topology@1", version = 1, capabilities = kernel_symbols::capability::SCHED_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn install_sched_topology(topology: SchedTopology) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    if !topology
        .root_domain()
        .span()
        .contains_mask(CpuMask::SUPPORTED)
    {
        return Err(errno::Errno::EINVAL);
    }
    let mut capacities = [0; NR_CPUS];
    for cpu in CpuMask::SUPPORTED.iter() {
        capacities[cpu.get()] = topology.cpu_capacity(cpu);
    }
    if !SCHEDULER.deadline_admission().fits_capacities(capacities) {
        return Err(errno::Errno::EBUSY);
    }
    SCHEDULER.install_topology(topology);
    Ok(())
}

pub(crate) fn refresh_task_placement(scheduler: &crate::Scheduler, task: &Task) -> bool {
    let source = task.placement();
    if source.state != crate::PlacementState::Bound {
        return false;
    }
    let Some(cpu) = source.cpu else {
        return false;
    };
    let snapshot = scheduler.topology_snapshot();
    if !snapshot.active.contains(cpu)
        || !CpuMask::from_bits_or_boot(task.cpu_affinity()).contains(cpu)
    {
        return false;
    }
    let domain_id = snapshot
        .topology
        .domain_for_cpu(cpu)
        .unwrap_or_else(|| snapshot.topology.root_domain())
        .id();
    if source.topology_generation == snapshot.generation && source.domain_id == domain_id {
        return true;
    }
    task.refresh_placement_topology(source, domain_id, snapshot.generation)
}

/// 按已提交的 placement 返回任务所属 runqueue。
///
/// placement 是 runqueue 所有权的唯一依据。拓扑代际变化时先刷新 domain 信息；
/// `current_cpu` 只保留为兼容旧查询路径的镜像，不参与任务定位。
pub(crate) fn task_runqueue_cpu_on(scheduler: &crate::Scheduler, task: &Task) -> Option<CpuId> {
    let _ = refresh_task_placement(scheduler, task);
    let placement = task.placement();
    if placement.state == crate::PlacementState::Unbound {
        return None;
    }
    let cpu = placement.cpu?;
    if task.current_cpu() != cpu.get() {
        task.set_current_cpu(cpu.get());
    }
    Some(cpu)
}

pub(crate) fn task_runqueue_cpu(task: &Task) -> Option<CpuId> {
    task_runqueue_cpu_on(&SCHEDULER, task)
}

#[kernel_symbols::export(name = "sched.scheduler.current_sched_domain_id", contract = "kernel.sched.topology@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn current_sched_domain_id(cpu_id: usize) -> Option<usize> {
    let cpu = CpuId::new(cpu_id)?;
    sched_topology()
        .domain_for_cpu(cpu)
        .map(|domain| domain.id())
}

/// 刷新并返回指定调度域的负载统计。
#[kernel_symbols::export(name = "sched.scheduler.sched_domain_stats", contract = "kernel.sched.topology@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
pub fn sched_domain_stats(domain_id: usize) -> Option<crate::SchedDomainStats> {
    refresh_domain_stats();
    SCHEDULER.domain_stats(domain_id)
}

/// 查询某个任务在当前拓扑下的调度放置状态。
///
/// 返回值只是快照：函数不会迁移任务，也不承诺下一次入队一定选中同一 CPU。
/// 调用方可用它向用户态或诊断路径解释“为什么这个任务能在哪些 CPU 上运行”。
#[kernel_symbols::export(name = "sched.scheduler.task_sched_placement", contract = "kernel.sched.topology@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn task_sched_placement(task: &Arc<Task>) -> SchedPlacement {
    task_sched_placement_on(&SCHEDULER, task)
}

fn task_sched_placement_on(scheduler: &crate::Scheduler, task: &Arc<Task>) -> SchedPlacement {
    // 未绑定任务的 current_cpu 只是兼容查询镜像，不能把它误当作真实的
    // 放置位置；否则每 CPU 独立调度域会把所有新任务固定到启动 CPU。
    let current = task_runqueue_cpu_on(scheduler, task);
    let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    let active = scheduler.active_set();
    let prefer_current = task.state() != TaskState::New;

    // 已经有稳定放置位置的任务在唤醒、信号和超时路径中占绝大多数。
    // `prefer_current` 的契约本来就要求保留该 CPU，因此无需为这类任务
    // 读取所有 runqueue 的负载；这也避免在 timer 路径中制造跨队列锁竞争。
    if prefer_current && current.is_some_and(|cpu| affinity.intersection(active).contains(cpu)) {
        return scheduler
            .topology()
            .describe_placement(affinity, active, current, true, |_| 0);
    }

    let load_snapshot = collect_rq_load_snapshot_for(scheduler, affinity.intersection(active));

    scheduler
        .topology()
        .describe_placement(affinity, active, current, prefer_current, |cpu| {
            load_snapshot.load_of(cpu)
        })
}

fn collect_rq_load_snapshot_for(
    scheduler: &crate::Scheduler,
    cpus: CpuMask,
) -> RunqueueLoadSnapshot {
    let _snapshot_guard = RUNQUEUE_SNAPSHOT_LOCK.lock();
    RunqueueLoadSnapshot::collect(cpus, |cpu| {
        scheduler.cpu_or_boot(cpu.get()).runqueue().nr_running()
    })
}

fn collect_class_load_snapshot_for(
    scheduler: &crate::Scheduler,
    cpus: CpuMask,
) -> RunqueueClassLoadSnapshot {
    let _snapshot_guard = RUNQUEUE_SNAPSHOT_LOCK.lock();
    RunqueueClassLoadSnapshot::collect(cpus, |cpu| {
        scheduler.cpu_or_boot(cpu.get()).runqueue().class_load()
    })
}

fn refresh_domain_stats() {
    let snapshot = SCHEDULER.topology_snapshot();
    let loads = collect_class_load_snapshot_for(&SCHEDULER, snapshot.active);
    SCHEDULER.update_domain_stats(snapshot, loads.loads());
}

/// 按当前拓扑、在线 CPU 和一次性 rq 负载快照选择目标 CPU。
pub(crate) fn select_cpu_for_mask(
    allowed: CpuMask,
    current: Option<CpuId>,
    prefer_current: bool,
) -> Option<CpuId> {
    select_cpu_for_mask_on(&SCHEDULER, allowed, current, prefer_current)
}

fn select_cpu_for_mask_on(
    scheduler: &crate::Scheduler,
    allowed: CpuMask,
    current: Option<CpuId>,
    prefer_current: bool,
) -> Option<CpuId> {
    let active = scheduler.active_set();
    let eligible = allowed.intersection(active);
    let load_snapshot = collect_rq_load_snapshot_for(scheduler, eligible);
    scheduler
        .topology()
        .select_cpu(allowed, active, current, prefer_current, |cpu| {
            load_snapshot.load_of(cpu)
        })
}

#[kernel_symbols::export(name = "sched.scheduler.is_cpu_online", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn is_cpu_online(cpu_id: usize) -> bool {
    CpuId::new(cpu_id).is_some_and(|cpu| SCHEDULER.online_set().contains(cpu))
}

#[kernel_symbols::export(name = "sched.scheduler.is_cpu_active", contract = "kernel.sched.query@1", version = 1, capabilities = kernel_symbols::capability::SCHED_QUERY)]
pub fn is_cpu_active(cpu_id: usize) -> bool {
    CpuId::new(cpu_id).is_some_and(|cpu| SCHEDULER.active_set().contains(cpu))
}

#[kernel_symbols::export(name = "sched.scheduler.mark_cpu_online", contract = "kernel.sched.cpu-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn mark_cpu_online(cpu_id: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    let cpu = CpuId::new(cpu_id).ok_or(errno::Errno::EINVAL)?;
    let _ = SCHEDULER.mark_cpu_online(cpu);
    PROGRAMMED_DEADLINE_VALID[cpu_id.min(NR_CPUS - 1)].store(false, Ordering::Release);
    Ok(())
}

/// TODO(smp): AP 只能在 current 和 idle 都安装完成后激活；底层通常应通过
/// [`cpu_start_scheduling`] 完成这一步，而不是提前直接调用本函数。
#[kernel_symbols::export(name = "sched.scheduler.activate_cpu", contract = "kernel.sched.cpu-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn activate_cpu(cpu_id: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    let cpu = CpuId::new(cpu_id).ok_or(errno::Errno::EINVAL)?;
    if !cpu_ready_for_activation(&SCHEDULER, cpu_id) {
        return Err(errno::Errno::EBUSY);
    }
    if !SCHEDULER.activate_cpu(cpu) {
        return Err(errno::Errno::EINVAL);
    }
    PROGRAMMED_DEADLINE_VALID[cpu_id.min(NR_CPUS - 1)].store(false, Ordering::Release);
    Ok(())
}

pub(crate) fn cpu_ready_for_activation(scheduler: &crate::Scheduler, cpu_id: usize) -> bool {
    scheduler
        .cpu(cpu_id)
        .is_some_and(|cpu_state| cpu_state.current().is_some() && cpu_state.idle().is_some())
}

/// 兼容旧调用：一次完成 online 和 active 发布。
///
/// TODO(smp): AP 启动路径不得调用该兼容接口；它不会检查 per-CPU current、idle
/// 和架构本地状态是否准备完成。
pub fn register_cpu(cpu_id: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    register_cpu_locked(cpu_id)
}

fn register_cpu_locked(cpu_id: usize) -> Result<(), errno::Errno> {
    let Some(cpu) = CpuId::new(cpu_id) else {
        return Err(errno::Errno::EINVAL);
    };
    let _ = SCHEDULER.mark_cpu_online(cpu);
    PROGRAMMED_DEADLINE_VALID[cpu_id].store(false, Ordering::Release);
    if !SCHEDULER.activate_cpu(cpu) {
        return Err(errno::Errno::EINVAL);
    }
    Ok(())
}

struct CpuOfflineTask {
    task: Arc<Task>,
    source: crate::PlacementSnapshot,
    target_cpu: CpuId,
    target_domain: usize,
    was_queued: bool,
}

/// 将一个非启动 CPU 下线并迁移其排队任务。
#[kernel_symbols::export(name = "sched.scheduler.offline_cpu", contract = "kernel.sched.cpu-lifecycle@1", version = 1, capabilities = kernel_symbols::capability::SCHED_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn offline_cpu(cpu_id: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    offline_cpu_with_scheduler(&SCHEDULER, cpu_id, now_ns_internal())
}

pub(crate) fn offline_cpu_with_scheduler(
    scheduler: &crate::Scheduler,
    cpu_id: usize,
    now_ns: u64,
) -> Result<(), errno::Errno> {
    let executing_cpu_id = cpu();
    let cpu = CpuId::new(cpu_id).ok_or(errno::Errno::EINVAL)?;
    if cpu == CpuId::boot() {
        return Err(errno::Errno::EBUSY);
    }
    if !scheduler.online_set().contains(cpu) {
        return Ok(());
    }

    let cpu_state = scheduler.cpu_or_boot(cpu_id);
    if cpu_state
        .current()
        .is_some_and(|current| !current.is_idle_task())
    {
        return Err(errno::Errno::EBUSY);
    }

    if scheduler.active_set().contains(cpu) && !scheduler.deactivate_cpu(cpu) {
        return Err(errno::Errno::EBUSY);
    }
    let drained = cpu_state.runqueue().drain_queued(now_ns);
    let mut candidates = drained.clone();
    for task in scheduler.deadline_admission().tasks_on_cpu(cpu) {
        if !candidates.iter().any(|queued| Arc::ptr_eq(queued, &task)) {
            candidates.push(task);
        }
    }
    let snapshot = scheduler.topology_snapshot();
    let mut planned_load = collect_rq_load_snapshot_for(scheduler, snapshot.active);
    let mut planned_deadline = scheduler.deadline_admission().totals();
    let mut tasks = Vec::new();
    for task in &candidates {
        let _ = refresh_task_placement(scheduler, task);
        let source = task.placement();
        if source.state != crate::PlacementState::Bound
            || source.cpu != Some(cpu)
            || task.arch_context().is_none()
            || matches!(task.state(), TaskState::Zombie | TaskState::Dead)
        {
            restore_drained_tasks(scheduler, cpu, &drained, now_ns);
            return Err(errno::Errno::EBUSY);
        }
        let allowed = CpuMask::from_bits_or_boot(task.cpu_affinity()).intersection(snapshot.active);
        let deadline_utilization = utilization_of(task.sched.sched_attr());
        let target_cpu = if deadline_utilization != 0 {
            allowed
                .iter()
                .filter(|target| {
                    planned_deadline[target.get()].saturating_add(deadline_utilization)
                        <= snapshot.topology.cpu_capacity(*target)
                })
                .min_by_key(|target| planned_load.load_of(*target))
        } else {
            snapshot
                .topology
                .select_cpu(allowed, snapshot.active, None, false, |target| {
                    planned_load.load_of(target)
                })
        };
        let Some(target_cpu) = target_cpu else {
            restore_drained_tasks(scheduler, cpu, &drained, now_ns);
            return Err(errno::Errno::EBUSY);
        };
        planned_load.add_task(target_cpu);
        if deadline_utilization != 0 {
            planned_deadline[cpu.get()] =
                planned_deadline[cpu.get()].saturating_sub(deadline_utilization);
            planned_deadline[target_cpu.get()] =
                planned_deadline[target_cpu.get()].saturating_add(deadline_utilization);
        }
        let target_domain = snapshot
            .topology
            .domain_for_cpu(target_cpu)
            .unwrap_or_else(|| snapshot.topology.root_domain())
            .id();
        tasks.push(CpuOfflineTask {
            task: Arc::clone(task),
            source,
            target_cpu,
            target_domain,
            was_queued: drained.iter().any(|queued| Arc::ptr_eq(queued, task)),
        });
    }

    let mut prepared = 0usize;
    for item in &tasks {
        if !item.task.begin_offline_repair(item.source) {
            restore_prepared_tasks(scheduler, cpu, &tasks, prepared, now_ns);
            return Err(errno::Errno::EBUSY);
        }
        prepared += 1;
    }

    for (index, item) in tasks.iter().enumerate() {
        let capacity = snapshot.topology.cpu_capacity(item.target_cpu);
        if scheduler
            .deadline_admission()
            .migrate(&item.task, cpu, item.target_cpu, capacity, || {
                item.task.commit_migration(
                    item.target_cpu,
                    item.target_domain,
                    snapshot.generation,
                );
                if item.was_queued
                    && !scheduler
                        .cpu_or_boot(item.target_cpu.get())
                        .runqueue()
                        .enqueue_migrated(Arc::clone(&item.task), now_ns)
                {
                    item.task.rollback_migration(item.source);
                    return Err(errno::Errno::EIO);
                }
                if !item.was_queued && item.task.sched.is_migrating() {
                    // 只在 deadline reservation 里、未排队的任务也必须结束事务。
                    item.task.sched.finish_migrating(false);
                }
                Ok(())
            })
            .is_err()
        {
            restore_unmoved_tasks(scheduler, cpu, &tasks, index, now_ns);
            return Err(errno::Errno::EBUSY);
        }
        scheduler
            .cpu_or_boot(item.target_cpu.get())
            .request_resched();
    }

    let late = cpu_state.runqueue().drain_queued(now_ns);
    if !late.is_empty()
        || !scheduler.deadline_admission().tasks_on_cpu(cpu).is_empty()
        || cpu_state
            .current()
            .is_some_and(|current| !current.is_idle_task())
    {
        restore_drained_tasks(scheduler, cpu, &late, now_ns);
        return Err(errno::Errno::EBUSY);
    }

    let current = cpu_state.clear_current();
    let idle = cpu_state.clear_idle();
    if let Some(task) = current.as_ref() {
        stop_cpu_task(cpu_state, task, now_ns);
    }
    if let Some(task) = idle.as_ref()
        && current
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, task))
    {
        stop_cpu_task(cpu_state, task, now_ns);
    }
    cpu_state.clear_scheduling_requests();
    if !scheduler.mark_cpu_offline(cpu) {
        return Err(errno::Errno::EBUSY);
    }
    let current_cpu =
        CpuId::new(executing_cpu_id).filter(|current| scheduler.active_set().contains(*current));
    let timer_target = current_cpu
        .or_else(|| scheduler.active_set().iter().next())
        .unwrap_or_else(CpuId::boot);
    migrate_deadline_owners(cpu_id, timer_target.get());
    if timer_target.get() == executing_cpu_id {
        reprogram_deadline_timer();
    }
    Ok(())
}

fn stop_cpu_task(cpu_state: &crate::CpuSchedState, task: &Arc<Task>, now_ns: u64) {
    let _ = cpu_state.runqueue().dequeue(task, now_ns);
    task.set_state(TaskState::Stopped);
    task.unbind_placement();
}

fn restore_drained_tasks(
    scheduler: &crate::Scheduler,
    cpu: CpuId,
    tasks: &[Arc<Task>],
    now_ns: u64,
) {
    let _ = scheduler.activate_cpu(cpu);
    for task in tasks {
        let _ = refresh_task_placement(scheduler, task);
        // drain_queued 把这些任务置为 MIGRATING，回滚必须经由提交入口收尾。
        let _ = scheduler
            .cpu_or_boot(cpu.get())
            .runqueue()
            .enqueue_migrated(Arc::clone(task), now_ns);
    }
}

fn restore_prepared_tasks(
    scheduler: &crate::Scheduler,
    cpu: CpuId,
    tasks: &[CpuOfflineTask],
    prepared: usize,
    now_ns: u64,
) {
    let _ = scheduler.activate_cpu(cpu);
    for (index, item) in tasks.iter().enumerate() {
        if index < prepared {
            item.task.rollback_migration(item.source);
        }
        let _ = refresh_task_placement(scheduler, &item.task);
        if item.was_queued {
            let _ = scheduler
                .cpu_or_boot(cpu.get())
                .runqueue()
                .enqueue_migrated(Arc::clone(&item.task), now_ns);
        } else if item.task.sched.is_migrating() {
            item.task.sched.finish_migrating(false);
        }
    }
}

fn restore_unmoved_tasks(
    scheduler: &crate::Scheduler,
    cpu: CpuId,
    tasks: &[CpuOfflineTask],
    first_unmoved: usize,
    now_ns: u64,
) {
    let _ = scheduler.activate_cpu(cpu);
    for item in &tasks[first_unmoved..] {
        item.task.rollback_migration(item.source);
        let _ = refresh_task_placement(scheduler, &item.task);
        if item.was_queued {
            let _ = scheduler
                .cpu_or_boot(cpu.get())
                .runqueue()
                .enqueue_migrated(Arc::clone(&item.task), now_ns);
        } else if item.task.sched.is_migrating() {
            item.task.sched.finish_migrating(false);
        }
    }
}

/// AP 启动路径的调度接入口：把当前 CPU 正在执行的 task 登记为该 CPU 的 current。
///
/// AP 完成 per-CPU 栈、trap、页表和本地数据初始化后调用。
/// 本 CPU idle task，最后进入 [`cpu_start_scheduling`]。
pub fn adopt_cpu_current(cpu_id: usize, task: Arc<Task>) -> Result<(), errno::Errno> {
    if CpuId::new(cpu_id).is_none() || cpu_id != cpu() {
        return Err(errno::Errno::EINVAL);
    }
    mark_cpu_online(cpu_id)?;
    bind_task_to_cpu(&task, cpu_id);
    if !task.try_claim_cpu(cpu_id) {
        return Err(errno::Errno::EBUSY);
    }
    if task.arch_context().is_none() {
        task.adopt_current_context();
    }
    SCHEDULER
        .cpu_or_boot(cpu_id)
        .runqueue()
        .set_current(Arc::clone(&task));
    task.account_switch_in(now_ns_internal());
    publish_current_task(cpu_id, task);
    Ok(())
}

pub fn needs_resched(cpu_id: usize) -> bool {
    SCHEDULER
        .cpu(cpu_id)
        .is_some_and(|cpu_state| cpu_state.needs_resched())
}

#[inline]
pub fn needs_resched_current() -> bool {
    if !INIT_READY.load(Ordering::Acquire) {
        return false;
    }
    SCHEDULER.cpu_or_boot(cpu()).needs_resched()
}

/// RISC-V 返回用户态热路径对本 CPU 调度工作的无栅栏预检。
///
/// `cpu_id` 来自架构本地 HartLocal，非零结果只能用于决定进入慢路径；真正消费
/// timer/handoff/resched 时仍使用原有 Acquire/RMW 接口。
#[inline(always)]
pub fn user_return_work_pending_on(cpu_id: usize) -> bool {
    if cpu_id >= NR_CPUS {
        return true;
    }
    SCHEDULER
        .cpu_or_boot(cpu_id)
        .user_return_work_pending_relaxed()
}

/// 清除并重建指定 CPU 的返回工作 hint。
///
/// AcqRel swap 与生产者的 Release fetch_or 配对。并发生产若发生在 swap 前，后续
/// Acquire 权威复查可见；若发生在 swap 后，生产者留下的 true 不会被覆盖。
pub fn refresh_user_return_work_on(cpu_id: usize) -> bool {
    if cpu_id >= NR_CPUS {
        return true;
    }
    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    let _ = cpu_state.take_user_return_work();
    let pending = DEFERRED_TIMER_TICK_NS[cpu_id].load(Ordering::Acquire) != 0
        || cpu_state.user_return_work_authoritative();
    if pending {
        cpu_state.mark_user_return_work();
    }
    pending || cpu_state.user_return_work_pending_acquire()
}

#[kernel_symbols::export(name = "sched.scheduler.request_resched", contract = "kernel.sched.control@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn request_resched(cpu_id: usize) {
    if cpu_id >= NR_CPUS {
        return;
    }
    SCHEDULER.cpu_or_boot(cpu_id).request_resched();
    notify_resched(cpu_id);
}

fn notify_resched(cpu_id: usize) {
    if cpu_id != cpu() {
        if let Some(ops) = arch_hooks::cpu_control() {
            let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
            if (ops.is_online)(cpu_id) && cpu_state.claim_resched_notification() {
                (ops.send_resched)(cpu_id);
            }
        }
    }
}

/// 确认当前 CPU 已经接收远端调度通知。
///
/// 硬件 IPI 被中断入口消费后，通知合并位必须立即释放；否则目标 CPU 尚未
/// 进入 `schedule_once` 时再次加入任务会被误认为已有未处理 IPI，从而永久
/// 丢失唤醒。真正的 `need_resched` 请求仍保留到安全调度边界处理。
pub fn acknowledge_resched_notification() {
    SCHEDULER.cpu_or_boot(cpu()).clear_resched_notification();
}

pub fn request_balance(cpu_id: usize) {
    if cpu_id >= NR_CPUS {
        return;
    }
    SCHEDULER.cpu_or_boot(cpu_id).request_balance();
    request_resched(cpu_id);
}

pub fn request_post_syscall_handoff() {
    let cpu_id = cpu();
    SCHEDULER
        .cpu_or_boot(cpu_id)
        .request_post_syscall_handoff(POST_SYSCALL_HANDOFF_ROUNDS);
    request_resched(cpu_id);
}

/// 为刚进入就绪队列的任务建立稳定交接目标。
///
/// 返回值可以短暂保存在事件源中，直到批次达到交接条件。公共调度边界会复查
/// 任务的 CPU、队列成员、状态和执行所有权，拒绝已经迁移或再次阻塞的旧请求。
pub fn enqueue_task_preferred_for_handoff(
    task: Arc<Task>,
    now_ns: u64,
    reason: HandoffReason,
    woke_from_sleep: bool,
    request_reschedule: bool,
) -> HandoffTarget {
    let cpu_id =
        enqueue_task_locked_with_preference(Arc::clone(&task), now_ns, true, request_reschedule);
    if request_reschedule {
        notify_resched(cpu_id);
    }
    HandoffTarget::new(task, cpu_id, reason, woke_from_sleep)
}

/// 保存当前任务作为后续 syscall 返回边界的交接目标。
///
/// 该接口只记录稳定任务身份，不改变任务状态或调度优先级。使用方可以在同一
/// 生产者/消费者关系的后续 syscall 中请求交接，调度边界仍会复查任务是否处于
/// 本 CPU 的公平就绪队列中。
pub fn current_task_handoff_target(reason: HandoffReason) -> Option<HandoffTarget> {
    if !INIT_READY.load(Ordering::Acquire) {
        return None;
    }
    let cpu_id = cpu();
    let task = current_task_fast();
    if task.state() != TaskState::Running || task.current_cpu() != cpu_id {
        return None;
    }
    Some(HandoffTarget::new(task, cpu_id, reason, false))
}

/// 请求在公共 syscall 返回边界精确交给指定任务。
///
/// 同 CPU 请求保存完整目标并合并重复事件；跨 CPU 请求只通知目标 runqueue，
/// 当前 CPU 不等待确认，也不会把远端任务拉回本地。
pub fn request_post_syscall_handoff_to(target: HandoffTarget) {
    if !INIT_READY.load(Ordering::Acquire) || target.preferred_cpu >= NR_CPUS {
        return;
    }
    let target_cpu = target.preferred_cpu;
    if target_cpu == cpu() {
        let cpu_state = SCHEDULER.cpu_or_boot(target_cpu);
        cpu_state.request_targeted_handoff(target);
        cpu_state.request_resched();
    } else {
        request_resched(target_cpu);
    }
}

/// 硬 IRQ 使用的无锁任务唤醒入口。
///
/// IRQ 可能打断持有 topology 或 runqueue 普通自旋锁的路径，不能原地调用
/// `activate_task`。这里仅发布一份 Arc 到目标 CPU 的 Treiber 链表并请求调度；
/// 真正入队由 `schedule_once` 在无调度锁边界完成。
pub fn defer_task_wake(task: &Arc<Task>) {
    if !INIT_READY.load(Ordering::Acquire) || !task.try_begin_deferred_wake() {
        return;
    }
    // 先挂到当前 IRQ 所在 CPU，确保该 CPU 返回安全调度边界时一定能消费；
    // 真正入队仍按任务 placement 选择目标 CPU，并在需要时发送远端 IPI。
    let target = cpu();
    let raw = Arc::into_raw(Arc::clone(task)).cast_mut();
    let head = &DEFERRED_TASK_WAKES[target];
    let mut observed = head.load(Ordering::Acquire);
    loop {
        task.set_deferred_wake_next(observed);
        match head.compare_exchange_weak(observed, raw, Ordering::Release, Ordering::Acquire) {
            Ok(_) => break,
            Err(actual) => observed = actual,
        }
    }
    request_resched(target);
}

/// 把 PI 有效属性更新推迟到安全调度边界，供不可分配的清理路径使用。
pub fn defer_pi_effective_update(task: &Arc<Task>) {
    task.request_deferred_pi_update();
    defer_task_wake(task);
}

fn drain_deferred_task_wakes() {
    let mut raw = DEFERRED_TASK_WAKES[cpu()].swap(core::ptr::null_mut(), Ordering::AcqRel);
    while !raw.is_null() {
        // Safety: 节点由 defer_task_wake 的 Arc::into_raw 创建，本消费者对从
        // per-CPU head 摘下的链表拥有对应强引用。
        let task = unsafe { Arc::from_raw(raw) };
        raw = task.take_deferred_wake_next();
        task.finish_deferred_wake();
        if let Some(attr) = task.take_deferred_pi_effective_attr() {
            let _ = crate::operation::pi_apply_effective_attr(&task, attr);
        }
        if task.arch_context().is_some()
            && matches!(
                task.state(),
                TaskState::New | TaskState::Runnable | TaskState::Sleeping
            )
        {
            #[cfg(feature = "performance-profile")]
            if task.state() == TaskState::Sleeping {
                task.mark_profile_woken(now_ns_internal());
            }
            let _ = enqueue_task_on_scheduler(&SCHEDULER, task, now_ns_internal(), false, true);
        }
    }
}

fn cleanup_retired_tasks(cpu_id: usize) {
    let retired = SCHEDULER.cpu_or_boot(cpu_id).take_retired();
    for task in retired.iter() {
        task.cleanup_exit_extensions();
        task.retire_execution();
    }
    drop(retired);
}

fn retire_final_task(cpu_id: usize, task: Arc<Task>) {
    SCHEDULER.cpu_or_boot(cpu_id).retire(task);
}

pub fn run_post_syscall_handoff(now_ns: u64) {
    if !INIT_READY.load(Ordering::Acquire) {
        return;
    }
    let cpu_id = cpu();
    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    let (rounds, target) = cpu_state.take_post_syscall_handoff();
    if rounds == 0 && target.is_none() {
        return;
    }

    // request_post_syscall_handoff() 会把普通 resched 位作为 trap 返回调度的兜底。
    // 这里消费了专用交接请求后，如果继续保留 resched 位，新任务第一次 syscall
    // 返回时会立刻再次进入调度器，而父任务还停在 clone 的 syscall 收尾路径上。
    // 因此先清掉兜底位；下面有界交接循环会完成预期的调度工作。
    let _ = cpu_state.take_resched();
    if cpu_state.take_balance() {
        let _ = balance_once(cpu_id);
    }
    // syscall 返回后的父子任务交接是 clone/vfork 热路径；时间戳只需采样一次，
    // 避免有界循环里重复读取时钟。
    let handoff_now = now_ns_internal().max(now_ns);
    if let Some(target) = target.as_ref() {
        schedule_once_inner(handoff_now, Some(target));
    }
    for _ in 0..rounds {
        if cpu_state.runqueue().nr_running() <= 1 {
            break;
        }
        schedule_once(handoff_now);
    }
}

pub fn run_post_syscall_handoff_lazy() {
    if !INIT_READY.load(Ordering::Acquire) {
        return;
    }
    let cpu_id = cpu();
    drain_deferred_timer_tick_on(cpu_id);
    if !SCHEDULER.cpu_or_boot(cpu_id).has_post_syscall_handoff() {
        return;
    }
    run_post_syscall_handoff(now_ns_internal());
}

/// 按调度域、亲和性和当前负载选择目标 CPU。
pub fn select_task_cpu(task: &Arc<Task>) -> usize {
    if let Some((cpu, _)) = SCHEDULER.deadline_admission().reservation_of(task) {
        return cpu.get();
    }
    task_sched_placement(task)
        .preferred_cpu
        .unwrap_or_else(CpuId::boot)
        .get()
}

/// 统一入队入口：设置任务 CPU 归属、入目标 runqueue、请求该 CPU 调度。
#[kernel_symbols::export(name = "sched.scheduler.enqueue_task", contract = "kernel.sched.task@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn enqueue_task(task: Arc<Task>, now_ns: u64) -> usize {
    let cpu_id = enqueue_task_locked(task, now_ns, true);
    notify_resched(cpu_id);
    cpu_id
}

/// 将尚未运行的任务原子绑定并首次加入指定活动 CPU。
///
/// active 状态检查、亲和性安装和入队受同一热插拔锁保护，避免 CPU 在检查后、
/// 任务真正进入 runqueue 前被下线。该接口只供创建路径使用。
pub(crate) fn activate_task_on_cpu(
    task: &Arc<Task>,
    cpu_id: usize,
    now_ns: u64,
) -> Result<usize, errno::Errno> {
    if task.arch_context().is_none()
        || !matches!(
            task.state(),
            TaskState::New | TaskState::Runnable | TaskState::Sleeping
        )
    {
        return Err(errno::Errno::EINVAL);
    }

    // 本入口只服务尚未暴露给任何 runqueue 的新任务，因此不可能处于迁移中。
    // 这一点必须成立：下面在 CPU_HOTPLUG_LOCK 内调用 enqueue_task_on_scheduler，
    // 而后者会自旋等待 MIGRATING 退出；若真有迁移在进行，它需要同一把锁才能
    // 收尾，就会形成死锁。
    debug_assert!(
        !task.sched.is_migrating(),
        "[sched] activate_task_on_cpu on migrating task",
    );
    if task.sched.is_migrating() {
        return Err(errno::Errno::EBUSY);
    }

    let selected = {
        let _guard = CPU_HOTPLUG_LOCK.lock();
        let cpu = CpuId::new(cpu_id).ok_or(errno::Errno::EINVAL)?;
        if !SCHEDULER.active_set().contains(cpu) {
            return Err(errno::Errno::EINVAL);
        }
        task.set_cpu_affinity(cpu.mask().bits());
        task.set_current_cpu(cpu_id);
        let selected = enqueue_task_on_scheduler(&SCHEDULER, Arc::clone(task), now_ns, false, true);
        debug_assert_eq!(selected, cpu_id);
        selected
    };
    notify_resched(selected);
    Ok(selected)
}

/// futex 唤醒热路径入口：入队后让下一次本地调度优先尝试运行该任务。
///
/// 这不是长期优先级，也不会绕过调度 class。runqueue 在真正 pick 时仍会
/// 复查任务状态、CPU 亲和性和 class；提示只消费一次，用于避免 pthread
/// join / condvar / mutex 这类短等待路径退化到等待下一个 tick。
pub fn enqueue_task_preferred(task: Arc<Task>, now_ns: u64) -> usize {
    let cpu_id = enqueue_task_locked_with_preference(task, now_ns, true, true);
    notify_resched(cpu_id);
    cpu_id
}

/// 按一次性 CPU 提示入队；提示不在线或不在 affinity 中时退回正常放置。
pub fn enqueue_task_with_hint(task: Arc<Task>, cpu_hint: usize, now_ns: u64) -> usize {
    let cpu_id = enqueue_task_locked_with_hint(task, now_ns, cpu_hint);
    request_resched(cpu_id);
    cpu_id
}

/// 只把任务放回 runqueue，不主动抢占当前任务。
///
/// vfork child 在 exec 完成时需要唤醒父进程，但 child 自己也刚拿到新的用户态
/// 镜像；若立刻抢占，父 shell 会继续执行下一条命令，而后台 daemon 还没机会
/// bind/listen。这个入口仅用于这类“父可运行，但当前 child 应先返回用户态”的
/// 场景。
pub fn enqueue_task_deferred(task: Arc<Task>, now_ns: u64) -> usize {
    let local_cpu = cpu();
    let cpu_id = enqueue_task_locked(task, now_ns, false);
    if cpu_id != local_cpu {
        // 本核上的 child 仍然优先返回用户态；父任务若落在远端睡眠 CPU，
        // 则必须设置 need_resched 并发送 IPI，否则它只能等到下一次无关的
        // 时钟或调度事件，vfork/exec 的等待可能永久卡住。
        SCHEDULER.cpu_or_boot(cpu_id).request_resched();
        notify_resched(cpu_id);
    }
    cpu_id
}

fn enqueue_task_locked(task: Arc<Task>, now_ns: u64, request_reschedule: bool) -> usize {
    enqueue_task_locked_with_preference(task, now_ns, false, request_reschedule)
}

fn enqueue_task_locked_with_preference(
    task: Arc<Task>,
    now_ns: u64,
    preferred: bool,
    request_reschedule: bool,
) -> usize {
    enqueue_task_on_scheduler(&SCHEDULER, task, now_ns, preferred, request_reschedule)
}

/// 等待任务离开迁移事务窗口。
///
/// 迁移中的任务已经从源 rq 摘除、尚未进入目标 rq，此时它的 placement 仍指向
/// 源 CPU。任何按 placement 定位 rq 的路径（唤醒、入队、状态变更、属性更新）
/// 若在这个窗口内动手，都会把任务写到错误的 rq 上，进而造成双 rq 登记——同一
/// 份内核栈和 arch context 被两个 CPU 同时切入。
///
/// 迁移临界区只包含两次 rq 加锁和若干原子操作，不会睡眠也不会等待远端 CPU，
/// 因此有界自旋是安全的。自旋期间轮询 urgent work，避免与 TLB shootdown 或
/// membarrier 的同步发起方形成互等。
/// 调用约束：**绝不能在持有 `CPU_HOTPLUG_LOCK` 时调用**。迁移事务需要该锁才能
/// 收尾，在锁内等待会与 `balance_once` 形成死锁。锁内路径应改为返回 `EBUSY`，
/// 由调用方在锁外重试。
pub(crate) fn wait_for_migration_to_settle(task: &Task) {
    // 迁移临界区是有界的（两次 rq 加锁），正常情况下几十次自旋内必然结束。
    // 上界只用来把"迁移方漏掉某条收尾路径"这类 bug 变成一条可诊断的日志，
    // 而不是让整个内核在这里静默死锁——SIGSTOP / exit / setaffinity 都会经过
    // 这个等待点。
    const MIGRATION_SETTLE_SPIN_LIMIT: usize = 1 << 22;
    let mut spins = 0usize;
    while task.sched.is_migrating() {
        // 每轮都协作处理 shootdown / membarrier，而不是每 64 轮一次：迁移方
        // 很可能正等本 CPU 应答一次 TLB shootdown 才能完成事务，两边互等时
        // 延迟应答会直接变成死锁。
        crate::poll_urgent_work();
        core::hint::spin_loop();
        spins = spins.wrapping_add(1);
        if spins >= MIGRATION_SETTLE_SPIN_LIMIT {
            log::error!(
                "[sched] migration failed to settle: pid={:?} placement={:?}; \
                 forcing transaction end (this indicates a missing finish_migrating)",
                task.pid_root(),
                task.placement().cpu.map(CpuId::get),
            );
            // 强制结束事务，让调用方按"不在 rq 上"继续处理。宁可丢一次负载均衡
            // 迁移，也不能让状态变更路径永久卡住。
            task.sched.force_finish_migrating();
            return;
        }
    }
}

pub(crate) fn enqueue_task_on_scheduler(
    scheduler: &crate::Scheduler,
    task: Arc<Task>,
    now_ns: u64,
    preferred: bool,
    request_reschedule: bool,
) -> usize {
    if task.arch_context().is_none() || matches!(task.state(), TaskState::Zombie | TaskState::Dead)
    {
        // 只有拥有执行体的活任务才能进入 rq。退出清理和 wait/reap 会释放
        // arch context；若这些任务被等待队列或信号路径误唤醒，必须在统一
        // 入队口截断，避免后续切换到已经回收的上下文。
        task.sched.set_on_rq(false);
        log::warning!(
            "[sched] reject enqueue without runnable context pid={:?} state={:?} has_ctx={}",
            task.pid_root(),
            task.state(),
            task.arch_context().is_some(),
        );
        let cpu_id = task.current_cpu().min(NR_CPUS - 1);
        if request_reschedule {
            scheduler.cpu_or_boot(cpu_id).request_resched();
        }
        return cpu_id;
    }
    let Some(_task_enqueue_guard) = task.sched.try_begin_enqueue() else {
        let cpu_id = task.current_cpu().min(NR_CPUS - 1);
        if request_reschedule {
            scheduler.cpu_or_boot(cpu_id).request_resched();
        }
        return cpu_id;
    };

    // 普通睡眠任务的 placement 已经提交，且有效时拓扑规则必然保留原 CPU。
    // IRQ/timer 唤醒不能为了得到同一结论再读取 topology 和所有远端 rq 锁；
    // 这些普通 Spinlock 可能正被被中断路径持有。Deadline 任务仍由准入表决定。
    if task.sched.class() != SchedClass::Deadline {
        let placement = task.placement();
        let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
        if placement.state == crate::PlacementState::Bound
            && let Some(cpu) = placement.cpu
            && scheduler.active_set().contains(cpu)
            && affinity.contains(cpu)
        {
            let cpu_state = scheduler.cpu_or_boot(cpu.get());
            let _enqueue_guard = cpu_state.begin_enqueue();
            if scheduler.active_set().contains(cpu)
                && CpuMask::from_bits_or_boot(task.cpu_affinity()).contains(cpu)
                && task.placement() == placement
            {
                let queued = if preferred {
                    cpu_state
                        .runqueue()
                        .enqueue_preferred(Arc::clone(&task), now_ns)
                } else {
                    cpu_state.runqueue().enqueue(Arc::clone(&task), now_ns)
                };
                if queued && request_reschedule {
                    cpu_state.request_resched();
                }
                return cpu.get();
            }
        }
    }

    for _ in 0..NR_CPUS {
        // placement 是 rq 归属的唯一依据，但它在迁移窗口内指向源 CPU。必须等
        // 事务落地后再读，否则会把任务入队到刚被摘空的源 rq，与目标 rq 形成
        // 双登记。
        wait_for_migration_to_settle(&task);
        let cpu_id = scheduler
            .deadline_admission()
            .reservation_of(&task)
            .map(|(cpu, _)| cpu)
            .or_else(|| task_sched_placement_on(scheduler, &task).preferred_cpu)
            .unwrap_or_else(CpuId::boot)
            .get();
        let cpu = CpuId::new(cpu_id).unwrap_or_else(CpuId::boot);
        let cpu_state = scheduler.cpu_or_boot(cpu_id);
        let _enqueue_guard = cpu_state.begin_enqueue();
        if !scheduler.active_set().contains(cpu) {
            continue;
        }

        bind_task_to_cpu_on(scheduler, &task, cpu_id);
        let queued = if preferred {
            cpu_state
                .runqueue()
                .enqueue_preferred(Arc::clone(&task), now_ns)
        } else {
            cpu_state.runqueue().enqueue(Arc::clone(&task), now_ns)
        };
        if queued && request_reschedule {
            cpu_state.request_resched();
        }
        return cpu_id;
    }

    unreachable!("[sched] boot CPU must accept task enqueue")
}

fn enqueue_task_locked_with_hint(task: Arc<Task>, now_ns: u64, cpu_hint: usize) -> usize {
    if task.arch_context().is_none() || matches!(task.state(), TaskState::Zombie | TaskState::Dead)
    {
        task.sched.set_on_rq(false);
        return task.current_cpu().min(NR_CPUS - 1);
    }
    // 与 enqueue_task_on_scheduler 同理：迁移窗口内的 placement 不可信。
    wait_for_migration_to_settle(&task);
    if task.sched.on_rq() {
        return task.current_cpu().min(NR_CPUS - 1);
    }
    let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    let active = active_cpu_set();
    let reserved = SCHEDULER
        .deadline_admission()
        .reservation_of(&task)
        .map(|(cpu, _)| cpu)
        .filter(|cpu| affinity.contains(*cpu) && active.contains(*cpu));
    let hinted = CpuId::new(cpu_hint)
        .filter(|cpu| reserved.is_none() && affinity.contains(*cpu) && active.contains(*cpu));
    let cpu_id = reserved
        .or(hinted)
        .or_else(|| select_cpu_for_mask(affinity, None, false))
        .unwrap_or_else(CpuId::boot)
        .get();
    task.set_current_cpu(cpu_id);
    bind_task_to_cpu(&task, cpu_id);
    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    if cpu_state.runqueue().enqueue(Arc::clone(&task), now_ns) {
        cpu_state.request_resched();
    }
    cpu_id
}

/// Register a sleeping task for deadline-based wakeup.
///
/// The caller owns the actual sleep transition and must cancel the registration
/// after it resumes. This helper only records the timeout side channel used by
/// timer ticks to move an expired sleeper back to Runnable.
pub fn register_sleep_deadline(task: &Arc<Task>, deadline_ns: u64) -> bool {
    let cpu_id = cpu();
    if !register_sleep_deadline_on_cpu(task, deadline_ns, cpu_id) {
        return false;
    }
    reprogram_deadline_timer();
    true
}

fn register_sleep_deadline_on_cpu(task: &Arc<Task>, deadline_ns: u64, cpu_id: usize) -> bool {
    if now_ns_internal() >= deadline_ns {
        return false;
    }
    {
        let mut sleepers = TIMED_SLEEPERS.lock();
        sleepers.retain(|entry| entry.task.upgrade().is_some());
        if let Some(entry) = sleepers.iter_mut().find(|entry| {
            entry
                .task
                .upgrade()
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, task))
        }) {
            entry.deadline_ns = entry.deadline_ns.min(deadline_ns);
            entry.cpu_id = cpu_id;
        } else {
            sleepers.push(TimedSleeper {
                deadline_ns,
                cpu_id,
                task: Arc::downgrade(task),
            });
        }
        HAS_TIMED_SLEEPERS.store(true, Ordering::Release);
        invalidate_deadline_cache();
    }
    true
}

#[cfg(test)]
pub(crate) fn register_sleep_deadline_for_test(
    task: &Arc<Task>,
    deadline_ns: u64,
    cpu_id: usize,
) -> bool {
    register_sleep_deadline_on_cpu(task, deadline_ns, cpu_id)
}

/// 移除 `task` 登记的全部 deadline wakeup。
///
/// 返回 `true` 表示队列内容确实发生变化。已经由 timer 消费的登记项不再触发
/// 冗余硬件重编程，缩短超时 syscall 的返回热路径。
pub fn cancel_sleep_deadline(task: &Arc<Task>) -> bool {
    let (changed, nonempty) = {
        let mut sleepers = TIMED_SLEEPERS.lock();
        let old_len = sleepers.len();
        sleepers.retain(|entry| {
            entry
                .task
                .upgrade()
                .as_ref()
                .is_some_and(|queued| !Arc::ptr_eq(queued, task))
        });
        (sleepers.len() != old_len, !sleepers.is_empty())
    };
    HAS_TIMED_SLEEPERS.store(nonempty, Ordering::Release);
    if changed {
        invalidate_deadline_cache();
        reprogram_deadline_timer();
    }
    changed
}

/// 预留一个不重复的 deadline 注册号，供调用方在入队前发布身份。
pub fn reserve_deadline_observer_id() -> u64 {
    let mut observers = DEADLINE_OBSERVERS.lock();
    let id = observers.next_id;
    observers.next_id = observers.next_id.wrapping_add(1).max(1);
    assert!(id != 0, "deadline observer id 已耗尽");
    id
}

/// 将 observer 按 deadline 放入调度定时器；deadline 已过期时返回 `false`。
pub fn register_deadline_observer(
    registration: u64,
    deadline_ns: u64,
    observer: Weak<dyn DeadlineObserver>,
) -> bool {
    try_register_deadline_observer(registration, deadline_ns, observer)
        .expect("deadline observer 分配失败")
}

pub fn try_register_deadline_observer(
    registration: u64,
    deadline_ns: u64,
    observer: Weak<dyn DeadlineObserver>,
) -> Result<bool, ()> {
    if now_ns_internal() >= deadline_ns {
        return Ok(false);
    }
    try_register_deadline_observer_deferred(registration, deadline_ns, observer)?;
    Ok(true)
}

/// 注册一个允许已经到期的 deadline。调用方已同步发布首次过期事件时，用它
/// 重装周期 observer，下一次 timer 边界会完成追赶而不会在调用路径自旋。
pub fn register_deadline_observer_deferred(
    registration: u64,
    deadline_ns: u64,
    observer: Weak<dyn DeadlineObserver>,
) {
    try_register_deadline_observer_deferred(registration, deadline_ns, observer)
        .expect("deadline observer 分配失败");
}

pub fn try_register_deadline_observer_deferred(
    registration: u64,
    deadline_ns: u64,
    observer: Weak<dyn DeadlineObserver>,
) -> Result<(), ()> {
    let mut observers = DEADLINE_OBSERVERS.lock();
    observers.entries.try_reserve(1).map_err(|_| ())?;
    observers.entries.push(DeadlineEntry {
        id: registration,
        deadline_ns,
        observer,
        firing: false,
    });
    observers
        .entries
        .sort_unstable_by_key(|entry| (entry.deadline_ns, entry.id));
    HAS_DEADLINE_OBSERVERS.store(true, Ordering::Release);
    Ok(())
}

/// 取消尚未到期的 observer 注册。
pub fn cancel_deadline_observer(registration: u64) {
    let mut observers = DEADLINE_OBSERVERS.lock();
    observers.entries.retain(|entry| entry.id != registration);
    if observers.entries.is_empty() {
        HAS_DEADLINE_OBSERVERS.store(false, Ordering::Release);
    }
}

fn fire_expired_deadline_observers(now_ns: u64) {
    if !HAS_DEADLINE_OBSERVERS.load(Ordering::Acquire) {
        return;
    }
    loop {
        let expired = {
            let mut observers = DEADLINE_OBSERVERS.lock();
            let Some(entry) = observers.entries.first_mut() else {
                HAS_DEADLINE_OBSERVERS.store(false, Ordering::Release);
                return;
            };
            if entry.deadline_ns > now_ns || entry.firing {
                return;
            }
            entry.firing = true;
            (entry.id, entry.observer.clone())
        };
        let next = expired
            .1
            .upgrade()
            .and_then(|observer| observer.deadline_expired(expired.0, now_ns));
        let mut observers = DEADLINE_OBSERVERS.lock();
        let Some(index) = observers
            .entries
            .iter()
            .position(|entry| entry.id == expired.0)
        else {
            continue;
        };
        if let Some(deadline_ns) = next.filter(|deadline| *deadline > now_ns) {
            let entry = &mut observers.entries[index];
            entry.deadline_ns = deadline_ns;
            entry.firing = false;
            observers
                .entries
                .sort_unstable_by_key(|entry| (entry.deadline_ns, entry.id));
        } else {
            observers.entries.remove(index);
            if observers.entries.is_empty() {
                HAS_DEADLINE_OBSERVERS.store(false, Ordering::Release);
            }
        }
    }
}

/// 查询当前线程组的 `ITIMER_REAL`。
pub fn get_realtime_itimer(task: &Arc<Task>) -> RealtimeItimerSpec {
    let tg = task.thread_group();
    let now_ns = now_ns_internal();
    let mut timers = REALTIME_ITIMERS.lock();
    let old_len = timers.len();
    timers.retain(|entry| entry.thread_group.upgrade().is_some());
    if timers.len() != old_len {
        invalidate_deadline_cache();
    }
    timers
        .iter()
        .find_map(|entry| {
            let queued = entry.thread_group.upgrade()?;
            if Arc::ptr_eq(&queued, &tg) {
                Some(RealtimeItimerSpec {
                    value_ns: entry.deadline_ns.saturating_sub(now_ns),
                    interval_ns: entry.interval_ns,
                })
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// 设置当前线程组的 `ITIMER_REAL`，返回旧值。
pub fn set_realtime_itimer(
    task: &Arc<Task>,
    value_ns: u64,
    interval_ns: u64,
) -> RealtimeItimerSpec {
    let tg = task.thread_group();
    let now_ns = now_ns_internal();
    let cpu_id = cpu();
    let old = {
        let mut timers = REALTIME_ITIMERS.lock();
        timers.retain(|entry| entry.thread_group.upgrade().is_some());

        let mut old = RealtimeItimerSpec::default();
        if let Some(pos) = timers.iter().position(|entry| {
            entry
                .thread_group
                .upgrade()
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, &tg))
        }) {
            let entry = timers.swap_remove(pos);
            old = RealtimeItimerSpec {
                value_ns: entry.deadline_ns.saturating_sub(now_ns),
                interval_ns: entry.interval_ns,
            };
        }

        // POSIX: value 为 0 时取消计时器；interval 仅在 value 非 0 时有意义。
        if value_ns != 0 {
            timers.push(RealtimeItimer {
                deadline_ns: now_ns.saturating_add(value_ns),
                interval_ns,
                cpu_id,
                thread_group: Arc::downgrade(&tg),
            });
        }
        HAS_REALTIME_ITIMERS.store(!timers.is_empty(), Ordering::Release);
        invalidate_deadline_cache();
        old
    };
    reprogram_deadline_timer();
    old
}

fn earliest_state_deadline_uncached(cpu_id: usize) -> Option<u64> {
    let sleeper_deadline = HAS_TIMED_SLEEPERS
        .load(Ordering::Acquire)
        .then(|| {
            let mut sleepers = TIMED_SLEEPERS.lock();
            let old_len = sleepers.len();
            sleepers.retain(|entry| entry.task.upgrade().is_some());
            if sleepers.len() != old_len {
                invalidate_deadline_cache();
            }
            HAS_TIMED_SLEEPERS.store(!sleepers.is_empty(), Ordering::Release);
            sleepers
                .iter()
                .filter(|entry| entry.cpu_id == cpu_id)
                .map(|entry| entry.deadline_ns)
                .min()
        })
        .flatten();
    let itimer_deadline = HAS_REALTIME_ITIMERS
        .load(Ordering::Acquire)
        .then(|| {
            let mut timers = REALTIME_ITIMERS.lock();
            let old_len = timers.len();
            timers.retain(|entry| entry.thread_group.upgrade().is_some());
            if timers.len() != old_len {
                invalidate_deadline_cache();
            }
            HAS_REALTIME_ITIMERS.store(!timers.is_empty(), Ordering::Release);
            timers
                .iter()
                .filter(|entry| entry.cpu_id == cpu_id)
                .map(|entry| entry.deadline_ns)
                .min()
        })
        .flatten();
    [sleeper_deadline, itimer_deadline]
        .into_iter()
        .flatten()
        .min()
}

fn cached_state_deadline(cpu_id: usize) -> Option<u64> {
    loop {
        let generation = DEADLINE_STATE_GENERATION.load(Ordering::Acquire);
        if DEADLINE_CACHE_GENERATION[cpu_id].load(Ordering::Acquire) == generation {
            return DEADLINE_CACHE_PRESENT[cpu_id]
                .load(Ordering::Relaxed)
                .then(|| DEADLINE_CACHE_VALUE[cpu_id].load(Ordering::Relaxed));
        }

        let deadline = earliest_state_deadline_uncached(cpu_id);
        if DEADLINE_STATE_GENERATION.load(Ordering::Acquire) != generation {
            continue;
        }
        if let Some(deadline) = deadline {
            DEADLINE_CACHE_VALUE[cpu_id].store(deadline, Ordering::Relaxed);
        }
        DEADLINE_CACHE_PRESENT[cpu_id].store(deadline.is_some(), Ordering::Relaxed);
        DEADLINE_CACHE_GENERATION[cpu_id].store(generation, Ordering::Release);
        return deadline;
    }
}

fn earliest_deadline(cpu_id: usize) -> Option<u64> {
    let cpu_id = cpu_id.min(NR_CPUS - 1);
    let state_deadline = cached_state_deadline(cpu_id);
    #[cfg(feature = "performance-profile")]
    let profile_deadline = profiling::next_sample_deadline_ns(cpu_id, now_ns_internal());
    #[cfg(not(feature = "performance-profile"))]
    let profile_deadline = None;
    [state_deadline, profile_deadline]
        .into_iter()
        .flatten()
        .min()
}

#[cfg(test)]
pub(crate) fn earliest_deadline_for_test(cpu_id: usize) -> Option<u64> {
    earliest_deadline(cpu_id)
}

/// 合并调用方的临时截止时间与当前 CPU 的调度截止时间，并重编程本地定时器。
///
/// 原生扩展等不会进入睡眠队列的受限执行区间可借此复用同一架构定时器契约。
/// `None` 仅撤销调用方的临时约束，不会覆盖已经登记的睡眠或实时定时器。
pub fn reprogram_current_deadline(external_deadline: Option<u64>) {
    let cpu_id = cpu();
    let scheduler_deadline = earliest_deadline(cpu_id);
    let deadline = match (scheduler_deadline, external_deadline) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    };
    if PROGRAMMED_DEADLINE_VALID[cpu_id].load(Ordering::Acquire)
        && PROGRAMMED_DEADLINE_PRESENT[cpu_id].load(Ordering::Relaxed) == deadline.is_some()
        && deadline
            .is_none_or(|value| PROGRAMMED_DEADLINE_VALUE[cpu_id].load(Ordering::Relaxed) == value)
    {
        return;
    }
    if let Some(ops) = arch_hooks::deadline_timer() {
        (ops.reprogram)(deadline);
        if let Some(value) = deadline {
            PROGRAMMED_DEADLINE_VALUE[cpu_id].store(value, Ordering::Relaxed);
        }
        PROGRAMMED_DEADLINE_PRESENT[cpu_id].store(deadline.is_some(), Ordering::Relaxed);
        PROGRAMMED_DEADLINE_VALID[cpu_id].store(true, Ordering::Release);
    }
}

/// 标记当前 CPU 的 one-shot 定时器已经触发。
///
/// 架构中断入口必须在重装常规 tick 前调用。后续即使最早软件截止时间没有变化，
/// 调度器也会重新写入硬件，不能复用已经被消费的编程状态。
#[inline]
pub fn deadline_timer_fired() {
    PROGRAMMED_DEADLINE_VALID[cpu()].store(false, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn timer_fired_for_test() {
    deadline_timer_fired();
}

fn reprogram_deadline_timer() {
    reprogram_current_deadline(None);
}

/// CPU 下线时把其尚未到期的软件计时器迁移到仍在线的 CPU。
///
/// sleeper 的任务放置与 timer 所有权彼此独立；新所有者只负责在期限到达时触发
/// 常规 enqueue，最终 runqueue 仍由任务亲和性和放置快照决定。
fn migrate_deadline_owners(source_cpu: usize, target_cpu: usize) {
    {
        let mut sleepers = TIMED_SLEEPERS.lock();
        for entry in sleepers.iter_mut() {
            if entry.cpu_id == source_cpu {
                entry.cpu_id = target_cpu;
            }
        }
    }
    let mut timers = REALTIME_ITIMERS.lock();
    for entry in timers.iter_mut() {
        if entry.cpu_id == source_cpu {
            entry.cpu_id = target_cpu;
        }
    }
    invalidate_deadline_cache();
}

fn take_expired_sleepers(now_ns: u64, cpu_id: usize) -> Vec<Arc<Task>> {
    if !HAS_TIMED_SLEEPERS.load(Ordering::Acquire) {
        return Vec::new();
    }
    let mut expired = Vec::new();
    let mut sleepers = TIMED_SLEEPERS.lock();
    let mut index = 0;
    let mut changed = false;
    while index < sleepers.len() {
        let Some(task) = sleepers[index].task.upgrade() else {
            sleepers.swap_remove(index);
            changed = true;
            continue;
        };
        if sleepers[index].cpu_id == cpu_id && sleepers[index].deadline_ns <= now_ns {
            sleepers.swap_remove(index);
            expired.push(task);
            changed = true;
            continue;
        }
        index += 1;
    }
    HAS_TIMED_SLEEPERS.store(!sleepers.is_empty(), Ordering::Release);
    drop(sleepers);
    if changed {
        invalidate_deadline_cache();
    }
    expired
}

#[cfg(test)]
pub(crate) fn take_expired_sleepers_for_test(now_ns: u64, cpu_id: usize) -> Vec<Arc<Task>> {
    take_expired_sleepers(now_ns, cpu_id)
}

fn wake_expired_sleepers(now_ns: u64, cpu_id: usize) -> bool {
    let mut woke = false;
    for task in take_expired_sleepers(now_ns, cpu_id) {
        if task.cas_state(TaskState::Sleeping, TaskState::Runnable) {
            task.mark_profile_woken(now_ns);
            enqueue_task_preferred(task, now_ns);
            woke = true;
        }
    }
    woke
}

fn fire_expired_realtime_itimers(now_ns: u64, cpu_id: usize) -> bool {
    if !HAS_REALTIME_ITIMERS.load(Ordering::Acquire) {
        return false;
    }
    let expired = {
        let mut timers = REALTIME_ITIMERS.lock();
        let mut expired = Vec::new();
        let mut idx = 0;
        let mut changed = false;
        while idx < timers.len() {
            let Some(tg) = timers[idx].thread_group.upgrade() else {
                timers.swap_remove(idx);
                changed = true;
                continue;
            };
            if timers[idx].cpu_id != cpu_id || timers[idx].deadline_ns > now_ns {
                idx += 1;
                continue;
            }

            expired.push(tg);
            let interval_ns = timers[idx].interval_ns;
            if interval_ns == 0 {
                timers.swap_remove(idx);
                changed = true;
                continue;
            }

            let mut next_deadline = timers[idx].deadline_ns.saturating_add(interval_ns);
            while next_deadline <= now_ns {
                let advanced = next_deadline.saturating_add(interval_ns);
                if advanced == next_deadline {
                    break;
                }
                next_deadline = advanced;
            }
            timers[idx].deadline_ns = next_deadline;
            changed = true;
            idx += 1;
        }
        HAS_REALTIME_ITIMERS.store(!timers.is_empty(), Ordering::Release);
        if changed {
            invalidate_deadline_cache();
        }
        expired
    };

    let fired = !expired.is_empty();
    for tg in expired {
        deliver_sigalrm_to_thread_group(&tg);
    }
    fired
}

fn deliver_sigalrm_to_thread_group(tg: &Arc<ThreadGroup>) {
    let info = SigInfo {
        sig: SignalNumber::SIGALRM,
        // SI_KERNEL 的精简编码；当前 SigInfo 只保留最小字段集。
        code: 128,
        sender_pid: 0,
        sender_uid: Uid::ROOT,
        raw: None,
    };
    deliver_shared_signal_to_group(tg, info);
}

fn migration_context(
    task: &Arc<Task>,
    target_cpu: usize,
) -> Result<MigrationContext, errno::Errno> {
    let Some(target) = CpuId::new(target_cpu) else {
        return Err(errno::Errno::EINVAL);
    };
    if !active_cpu_set().contains(target) {
        return Err(errno::Errno::EINVAL);
    }
    let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    if !affinity.contains(target) {
        return Err(errno::Errno::EINVAL);
    }
    if task.state() == TaskState::Running {
        return Err(errno::Errno::EBUSY);
    }
    // 此处已持有 CPU_HOTPLUG_LOCK；balance_once 在锁外置 MIGRATING，在锁内
    // 清除，因此不能在锁内自旋等待——那会与 balance_once 相互死锁。若任务正处
    // 于迁移中，直接返回 EBUSY，让调用方（setaffinity 等）在锁外自旋后重试。
    if task.sched.is_migrating() {
        return Err(errno::Errno::EBUSY);
    }
    let _ = refresh_task_placement(&SCHEDULER, task);
    let mut source = task.placement();
    if source.state == crate::PlacementState::Unbound {
        bind_task_to_cpu(task, task.current_cpu().min(NR_CPUS - 1));
        source = task.placement();
    }
    if source.state != crate::PlacementState::Bound {
        return Err(errno::Errno::EBUSY);
    }
    let topology = SCHEDULER.topology_snapshot();
    if source.topology_generation != topology.generation {
        return Err(errno::Errno::EAGAIN);
    }
    let target_domain = topology
        .topology
        .domain_for_cpu(target)
        .unwrap_or_else(|| topology.topology.root_domain())
        .id();
    if !task.begin_migration(source) {
        return Err(errno::Errno::EBUSY);
    }
    Ok(MigrationContext {
        source,
        target_cpu: target,
        target_domain,
        topology_generation: topology.generation,
    })
}

fn rollback_migration(task: &Arc<Task>, context: MigrationContext, requeue_source: bool) {
    task.rollback_migration(context.source);
    let _ = refresh_task_placement(&SCHEDULER, task);
    if requeue_source {
        // 任务此刻仍处于 MIGRATING，普通 enqueue 会被 on_rq 门禁拒绝并把它永久
        // 留在迁移态，让所有唤醒方无限自旋。必须走迁移提交入口收尾。
        SCHEDULER
            .cpu_or_boot(context.source.cpu.map_or(0, CpuId::get))
            .runqueue()
            .enqueue_migrated(Arc::clone(task), now_ns_internal());
    } else if task.sched.is_migrating() {
        // 没有重新入队的回滚路径同样必须结束事务，否则任务停在 MIGRATING 却
        // 不在任何 rq 上，等待方永远等不到落地。
        task.sched.finish_migrating(false);
    }
}

pub(crate) fn validate_migration_target(
    context: MigrationContext,
    topology: TopologySnapshot,
    affinity: CpuMask,
) -> Result<(), errno::Errno> {
    if topology.generation != context.topology_generation {
        return Err(errno::Errno::EAGAIN);
    }
    if !topology.active.contains(context.target_cpu) || !affinity.contains(context.target_cpu) {
        return Err(errno::Errno::EINVAL);
    }
    Ok(())
}

fn attach_migrated_task(
    task: &Arc<Task>,
    context: MigrationContext,
    source_detached: bool,
) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    attach_migrated_task_locked(task, context, source_detached)
}

fn attach_migrated_task_locked(
    task: &Arc<Task>,
    context: MigrationContext,
    source_detached: bool,
) -> Result<(), errno::Errno> {
    let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    let topology = SCHEDULER.topology_snapshot();
    if let Err(error) = validate_migration_target(context, topology, affinity) {
        rollback_migration(task, context, source_detached);
        return Err(error);
    }
    if !source_detached {
        let source_cpu = context.source.cpu.map_or(0, CpuId::get);
        // 摘除与 MIGRATING 的发布必须在源 rq 的同一临界区内完成，否则中间会
        // 出现 on_rq==NONE 的窗口，让并发唤醒把任务重新塞回源 rq。
        if !SCHEDULER
            .cpu_or_boot(source_cpu)
            .runqueue()
            .detach_queued_for_migration(task, now_ns_internal())
        {
            rollback_migration(task, context, false);
            return Err(errno::Errno::EBUSY);
        }
    }
    let commit_topology = SCHEDULER.topology_snapshot();
    let commit_affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    if validate_migration_target(context, commit_topology, commit_affinity).is_err() {
        rollback_migration(task, context, true);
        return Err(errno::Errno::EAGAIN);
    }
    let source_cpu = context.source.cpu.unwrap_or_else(CpuId::boot);
    let target_capacity = topology.topology.cpu_capacity(context.target_cpu);
    let result = SCHEDULER.deadline_admission().migrate(
        task,
        source_cpu,
        context.target_cpu,
        target_capacity,
        || {
            task.commit_migration(
                context.target_cpu,
                context.target_domain,
                context.topology_generation,
            );
            if !SCHEDULER
                .cpu_or_boot(context.target_cpu.get())
                .runqueue()
                .enqueue_migrated(Arc::clone(task), now_ns_internal())
            {
                rollback_migration(task, context, true);
                return Err(errno::Errno::EIO);
            }
            Ok(())
        },
    );
    if let Err(error) = result {
        // deadline_admission 的带宽检查可能在根本没调用闭包时就失败；此时任务
        // 仍停在 MIGRATING 且不在任何 rq 上。必须显式回滚，否则每个等待它落地
        // 的唤醒方都会永久自旋——SIGSTOP、exit、setaffinity 全部卡死。
        if task.sched.is_migrating() {
            rollback_migration(task, context, true);
        }
        return Err(error);
    }
    request_resched(context.target_cpu.get());
    Ok(())
}

#[kernel_symbols::export(name = "sched.scheduler.migrate_task", contract = "kernel.sched.task@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn migrate_task(task: &Arc<Task>, target_cpu: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    migrate_task_locked(task, target_cpu)
}

fn migrate_task_locked(task: &Arc<Task>, target_cpu: usize) -> Result<(), errno::Errno> {
    if !task.sched.on_rq() {
        let target = CpuId::new(target_cpu).ok_or(errno::Errno::EINVAL)?;
        if !active_cpu_set().contains(target)
            || !CpuMask::from_bits_or_boot(task.cpu_affinity()).contains(target)
        {
            return Err(errno::Errno::EINVAL);
        }
        let source = task.placement().cpu.unwrap_or_else(CpuId::boot);
        let capacity = cpu_capacity(target);
        return SCHEDULER
            .deadline_admission()
            .migrate(task, source, target, capacity, || {
                bind_task_to_cpu(task, target_cpu);
                Ok(())
            });
    }
    let context = migration_context(task, target_cpu)?;
    attach_migrated_task_locked(task, context, false)
}

/// 从最忙 CPU 拉一个任务到 `cpu_id`。AP 启动后可由 idle/tick 路径周期调用。
pub fn balance_once(cpu_id: usize) -> bool {
    let Some(local_cpu) = CpuId::new(cpu_id) else {
        return false;
    };
    let active = active_cpu_set();
    if !active.contains(local_cpu) {
        return false;
    }

    // 便宜的前置检查：本 CPU 已经有活干就没必要偷。
    //
    // 下面的亲和性敏感全 CPU 负载快照需要取全局 RUNQUEUE_SNAPSHOT_LOCK，是
    // 这个函数的主要开销。域统计只供诊断查询使用，不能让每次迁移尝试都
    // 重复构建一份不会参与本次决策的聚合统计。
    // 先计算本地 runqueue 深度，能把绝大部分无收益的全局扫描挡在门外。
    if SCHEDULER.cpu_or_boot(cpu_id).runqueue().nr_running() > 1 {
        return false;
    }

    let topology_snapshot = SCHEDULER.topology_snapshot();
    let topology = &topology_snapshot.topology;
    let allowed = CpuMask::single(local_cpu).bits();
    let load_snapshot = {
        let _snapshot_guard = RUNQUEUE_SNAPSHOT_LOCK.lock();
        RunqueueClassLoadSnapshot::collect(active, |cpu| {
            SCHEDULER
                .cpu_or_boot(cpu.get())
                .runqueue()
                .migratable_class_load_for(allowed)
        })
    };
    let classes = [SchedClass::Deadline, SchedClass::Realtime, SchedClass::Fair];
    let Some((src, class)) = classes.into_iter().find_map(|class| {
        select_balance_source_for_class(topology, local_cpu, active, class, |cpu| {
            load_snapshot.load_of(cpu)
        })
        .map(|source| (source.get(), class))
    }) else {
        return false;
    };
    let Some(task) = SCHEDULER
        .cpu_or_boot(src)
        .runqueue()
        .take_migratable_from_class(class, allowed, now_ns_internal())
    else {
        return false;
    };
    if class == SchedClass::Deadline {
        if !SCHEDULER.deadline_admission().can_migrate(
            &task,
            local_cpu,
            topology.cpu_capacity(local_cpu),
        ) {
            let target = requeue_balance_task_on(&SCHEDULER, task, src, now_ns_internal());
            notify_resched(target);
            return false;
        }
    }
    let source = task.placement();
    let topology_snapshot = SCHEDULER.topology_snapshot();
    let target_domain = topology_snapshot
        .topology
        .domain_for_cpu(local_cpu)
        .unwrap_or_else(|| topology_snapshot.topology.root_domain())
        .id();
    if source.state != crate::PlacementState::Bound
        || source.topology_generation != topology_snapshot.generation
        || !task.begin_migration(source)
    {
        let target = requeue_balance_task_on(&SCHEDULER, task, src, now_ns_internal());
        notify_resched(target);
        return false;
    }
    let context = MigrationContext {
        source,
        target_cpu: local_cpu,
        target_domain,
        topology_generation: topology_snapshot.generation,
    };
    attach_migrated_task(&task, context, true).is_ok()
}

pub(crate) fn requeue_balance_task_on(
    scheduler: &crate::Scheduler,
    task: Arc<Task>,
    source_cpu: usize,
    now_ns: u64,
) -> usize {
    if let Some(source) = CpuId::new(source_cpu) {
        let cpu_state = scheduler.cpu_or_boot(source_cpu);
        let _enqueue_guard = cpu_state.begin_enqueue();
        if scheduler.active_set().contains(source) {
            // task 可能带着 MIGRATING 状态进来（来自失败的 balance_once 回滚）；
            // 普通 enqueue 的 on_rq 门禁会拒绝它，需要走提交入口。
            let queued = if task.sched.is_migrating() {
                cpu_state
                    .runqueue()
                    .enqueue_migrated(Arc::clone(&task), now_ns)
            } else {
                cpu_state.runqueue().enqueue(Arc::clone(&task), now_ns)
            };
            if queued {
                cpu_state.request_resched();
            }
            return source_cpu;
        }
    }
    // 源 CPU 已下线；任务仍可能处于 MIGRATING，等它落地后交给通用入队路径。
    if task.sched.is_migrating() {
        task.sched.finish_migrating(false);
    }
    enqueue_task_on_scheduler(scheduler, task, now_ns, false, true)
}

pub(crate) fn select_balance_source_for_class<F>(
    topology: &SchedTopology,
    local_cpu: CpuId,
    active: CpuMask,
    class: SchedClass,
    mut load_of: F,
) -> Option<CpuId>
where
    F: FnMut(CpuId) -> RunqueueClassLoad,
{
    if class == SchedClass::Idle {
        return None;
    }
    let local_load = load_of(local_cpu);
    let local_capacity = topology.cpu_capacity(local_cpu);
    let local_utilization = normalized_load(local_load.balance_load(class), local_capacity);
    let mut domain = topology
        .domain_for_cpu(local_cpu)
        .unwrap_or_else(|| topology.root_domain());

    loop {
        let mut busiest = None;
        let mut busiest_utilization = local_utilization;
        for other in domain.span().intersection(active).without(local_cpu).iter() {
            let load = load_of(other);
            if !class_allows_pull(class, local_load, load) {
                continue;
            }
            let utilization =
                normalized_load(load.balance_load(class), topology.cpu_capacity(other));
            if utilization > busiest_utilization {
                busiest = Some(other);
                busiest_utilization = utilization;
            }
        }
        if busiest.is_some() {
            return busiest;
        }
        let Some(parent) = domain.parent() else {
            return None;
        };
        domain = topology.domain(parent)?;
    }
}

fn normalized_load(load: u64, capacity: u64) -> u64 {
    if capacity == 0 {
        return 0;
    }
    load.saturating_mul(crate::SCHED_CAPACITY_SCALE) / capacity
}

fn class_allows_pull(
    class: SchedClass,
    local_load: RunqueueClassLoad,
    source_load: RunqueueClassLoad,
) -> bool {
    match class {
        // 迁移单个 Deadline 任务不会增加并行度，还会破坏已有 CPU 局部性。
        SchedClass::Deadline => {
            source_load.deadline > local_load.deadline.saturating_add(1)
                && source_load.deadline_utilization > local_load.deadline_utilization
        }
        SchedClass::Realtime => {
            if local_load.realtime == 0 {
                source_load.realtime > 0
            } else {
                source_load.realtime > local_load.realtime.saturating_add(1)
            }
        }
        SchedClass::Fair => {
            if local_load.fair == 0 {
                source_load.fair > 0
            } else {
                source_load.fair_weight > local_load.fair_weight.saturating_add(NICE_0_WEIGHT)
            }
        }
        SchedClass::Idle => false,
    }
}

fn migrate_local_ineligible_or_request_balance(task: &Arc<Task>, source_cpu: usize) {
    let Some(source) = CpuId::new(source_cpu) else {
        return;
    };
    if !task.sched.on_rq() || task.state() != TaskState::Runnable {
        return;
    }

    let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    if affinity.contains(source) {
        return;
    }

    let active = active_cpu_set();
    let allowed = affinity.intersection(active).without(source);
    if allowed.is_empty() {
        return;
    }

    let target = select_cpu_for_mask(allowed, Some(source), false);
    if let Some(cpu) = target {
        if migrate_task(task, cpu.get()).is_err() {
            request_balance(cpu.get());
        }
    }
}

// ── 信号唤醒 ─────────────────────────────────────────────────────────────────

/// 向线程组发布共享信号，并为所有用户成员设置任务级返回工作 hint。
///
/// 共享 pending 最终只会被一个成员消费。发布与成员修改 blocked mask 没有共同
/// 锁，因此不能用发布时的 mask 快照筛掉 hint；给全部用户成员置位可封闭并发
/// unblock 窗口。硬件唤醒仍只发送给第一个当前可投递候选，避免 IPI 风暴。
pub(crate) fn deliver_shared_signal_to_group(tg: &Arc<ThreadGroup>, info: SigInfo) {
    tg.shared_signal().deliver(info);
    let mut woke = false;
    for task in tg.snapshot() {
        if task.is_kernel_task() {
            continue;
        }
        task.mark_user_return_work();
        let eligible = !task.signal.blocked_snapshot().has(info.sig)
            || task.signal.sigtimedwait_wants(info.sig);
        if !woke && eligible {
            signal_wakeup(&task, &info);
            woke = true;
        }
    }
}

/// 把一条信号投给 `target`，并在可行时把它从 Sleeping 拉回 Runnable。
///
/// 调用方已经把 `info` 放进了 target 的 per-task 或共享 pending 队列，这里
/// 只负责"是否需要唤醒"。`Uninterruptible` 任务不会被打断（Linux 语义）。
#[kernel_symbols::export(name = "sched.scheduler.signal_wakeup", contract = "kernel.sched.task@1", version = 1, capabilities = kernel_symbols::capability::SCHED_TASK, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn signal_wakeup(target: &Arc<Task>, info: &SigInfo) {
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[sched][signal] wake-enter target={:?} signal={:?} state={:?} on_rq={} running_cpu={:?}",
        target.pid_root(),
        info.sig,
        target.state(),
        target.sched.on_rq(),
        target.running_cpu(),
    );
    target.mark_user_return_work();
    if info.sig == SignalNumber::SIGCONT && continue_task(target) {
        return;
    }
    if target.state() == TaskState::Stopped && stopped_signal_is_fatal(target, info) {
        // 只恢复运行，由目标在自己的调度/syscall 边界消费
        // 致命信号。这里不发布 WCONTINUED，也不远程废弃其内核栈。
        if target.resume_for_fatal_exit() {
            enqueue_task(Arc::clone(target), now_ns_internal());
        }
        return;
    }
    if target.cas_state(TaskState::Sleeping, TaskState::Runnable) {
        #[cfg(feature = "performance-profile")]
        target.mark_profile_woken(now_ns_internal());
        enqueue_task(Arc::clone(target), now_ns_internal());
    }
    if let Some(target_cpu) = target.running_cpu() {
        let target_cpu = target_cpu.min(NR_CPUS - 1);
        if target_cpu != cpu() {
            // signal 可能在目标返回用户态的最后一次 flags 读取之后发布；与 Linux
            // 的 task-work kick 相同，用 resched IPI 封闭这个最终检查窗口。
            request_resched(target_cpu);
        }
    }
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[sched][signal] wake-leave target={:?} signal={:?} state={:?} on_rq={} running_cpu={:?}",
        target.pid_root(),
        info.sig,
        target.state(),
        target.sched.on_rq(),
        target.running_cpu(),
    );
    // Running：远端执行者已收到 resched IPI；Runnable 会在恢复内核栈后检查。
    // Stopped：只有 SIGCONT 可以恢复；其它信号保持 pending。
    // Uninterruptible / Zombie / Dead：什么都不做。
}

/// 唤醒收到协作式 exit_group 请求的线程，但不在发送者上下文远程终止它。
///
/// 目标必须恢复自己的内核调用栈并在 syscall / 用户返回边界完成退出，才能让
/// 栈上 Arc 正常析构。Stopped 任务先恢复，Running 任务通过 resched IPI 尽快
/// 到达安全边界。
pub fn group_exit_wakeup(target: &Arc<Task>) {
    target.publish_group_exit_wakeup();
    forced_exit_wakeup(target);
}

/// 唤醒被 exec 发起者要求自行退出的兄弟线程。
pub fn exec_sibling_exit_wakeup(target: &Arc<Task>, preserve_leader_identity: bool) {
    target.publish_exec_sibling_exit_wakeup(preserve_leader_identity);
    forced_exit_wakeup(target);
}

/// 唤醒收到 Native 单线程终止请求的任务。目标在自己的安全边界完成退出，
/// 发送者不会远程释放目标内核栈上的资源。
pub fn native_thread_exit_wakeup(target: &Arc<Task>, code: i32) -> i32 {
    let selected = target.publish_native_thread_exit_wakeup(code);
    forced_exit_wakeup(target);
    selected
}

fn forced_exit_wakeup(target: &Arc<Task>) {
    let resumed = target.resume_for_fatal_exit()
        || target.cas_state(TaskState::Sleeping, TaskState::Runnable)
        || target.cas_state(TaskState::Uninterruptible, TaskState::Runnable);
    if resumed {
        #[cfg(feature = "performance-profile")]
        target.mark_profile_woken(now_ns_internal());
        enqueue_task(Arc::clone(target), now_ns_internal());
    }
    request_resched(target.current_cpu().min(NR_CPUS - 1));
}

/// 把任务切入停止态：从 runqueue/current 中摘掉并记录可等待的 stopped 事件。
pub(crate) fn mark_task_stopped(task: &Arc<Task>, sig: SignalNumber) -> bool {
    let removed = dequeue_for_state_change(task, now_ns_internal());
    let stopped = task.mark_stopped(sig);
    if stopped
        && let Some(target_cpu) = task.running_cpu()
        && target_cpu != cpu()
    {
        request_resched(target_cpu.min(NR_CPUS - 1));
    }
    log::debug!(
        "[sched][signal] stop pid={:?} sig={} on_rq={} state={:?}",
        task.pid_root(),
        sig.raw(),
        removed,
        task.state(),
    );
    stopped
}

/// 恢复一个停止任务，并记录可被 `wait(WCONTINUED)` 观察的 continued 事件。
pub(crate) fn continue_task(task: &Arc<Task>) -> bool {
    if task.arch_context().is_none() {
        return false;
    }
    if !task.mark_continued() {
        return false;
    }
    enqueue_task(Arc::clone(task), now_ns_internal());
    log::debug!(
        "[sched][signal] continue pid={:?} state={:?}",
        task.pid_root(),
        task.state(),
    );
    true
}

// ── schedule_once ────────────────────────────────────────────────────────────

/// 主动让出 CPU：从当前核的 runqueue 挑出下一个 eligible 任务，若与当前不同
/// 则切换。
///
/// `now_ns` 是当前时间戳（纳秒）；传 0 表示"不推进虚拟时间"（适合启动期、
/// 主动 yield 之类无法测时间的路径）。返回时调用方已经重新获得 CPU。
#[kernel_symbols::export(
    name = "sched.scheduler.schedule_once",
    contract = "kernel.sched.control@1",
    version = 1,
    capabilities = kernel_symbols::capability::SCHED_TASK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn schedule_once(now_ns: u64) {
    schedule_once_inner(now_ns, None);
}

fn schedule_once_inner(now_ns: u64, target: Option<&HandoffTarget>) {
    // 保护器保存在当前任务自己的调用栈上。任务被切回（即使迁移到另一 CPU）后，
    // 会恢复它进入本次调度前的中断状态，而不是继承此前运行任务留下的硬件状态。
    let _local_interrupt_guard = arch_hooks::disable_local_interrupts();
    #[cfg(feature = "performance-profile")]
    let schedule_start = profiling::read_counter();
    // LoongArch syscall 路径可能在本地中断暂时关闭时主动调度。即使本次仍选回
    // 当前任务，也必须先消费 TLB/membarrier 请求，避免同步发起方永久等待。
    crate::poll_urgent_work();
    drain_deferred_timer_tick();
    let cpu_id = cpu();
    drain_deferred_task_wakes();
    // 用户 syscall 进入 S-mode 后会保持本地中断关闭。若多个可运行任务持续在
    // 阻塞 syscall 与内核 worker 之间切换，CPU 可能长期不回 idle/user，硬件
    // timer 只能保持 pending。调度边界本身已经是无锁安全点，因此用单调时钟
    // 主动推进 sleeper/ITIMER_REAL，不能只依赖已经实际进入过的 timer IRQ。
    let deferred_ns = take_deferred_timer_tick(&DEFERRED_TIMER_TICK_NS[cpu_id]);
    let timer_now_ns = now_ns_internal().max(now_ns).max(deferred_ns);
    service_expired_timer_events(timer_now_ns, cpu_id);
    cleanup_retired_tasks(cpu_id);
    #[cfg(feature = "performance-profile")]
    let pick_start = {
        let now = profiling::read_counter();
        profiling::observe(
            profiling::Metric::SchedTimerCycles,
            now.wrapping_sub(schedule_start),
        );
        now
    };

    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    // 主动调度和 IPI 返回都会经过这里。进入选取过程即表示本 CPU 已经观察到
    // runnable 状态，可以允许后续新请求再次发送远端通知。
    cpu_state.clear_resched_notification();

    // 1. 取 prev（不持 owning current 锁跨切换）。
    let Some(prev) = cpu_state.current() else {
        return;
    };

    // 调度入口是所有阻塞路径共用的睡眠提交边界。若组退出
    // 已在调用者的条件检查后发布，这里再次撤销睡眠，使其
    // 恢复阻塞调用栈并返回 syscall 安全边界。
    let _ = prev.abort_group_exit_sleep();

    // Tomori 在调度边界消费当前任务的 pending signal。Native 必须保留
    // 到调用或用户返回的安全边界，避免在深层调度栈中执行外部控制。
    // 默认 Term/Core 会把 prev 标成 Zombie；后续 pick_next 不再将它放回 runqueue。
    if prev.user_abi_kind() == crate::UserAbiKind::TomoriLinux
        && prev.state() == TaskState::Running
        && (prev.signal.has_any_pending() || prev.shared_signal_pending_bits_quick() != 0)
    {
        let _ = crate::operation::deliver_pending_signals();
    }

    // 2. 挑下一个；pick_next 会把 prev 放回 tree（若仍 runnable）。若 prev 的
    //    亲和性已经排除本 CPU，此时它已稳定停在旧 rq，可通知目标 CPU 拉取。
    let cpu_mask = CpuMask::single_raw(cpu_id).bits();
    let targeted = target.filter(|target| targeted_handoff_is_valid(target, cpu_id));
    let picked = targeted
        .and_then(|target| {
            cpu_state
                .runqueue()
                .pick_target_on(&target.task, now_ns, cpu_mask)
        })
        .or_else(|| cpu_state.runqueue().pick_next_on(now_ns, cpu_mask));
    let next = match picked {
        Some(t) => t,
        None => {
            // 队列空：回落到本核 idle。idle 未安装则保持 prev 不切。
            let idle = cpu_state.idle();
            let Some(idle) = idle else {
                return;
            };
            if Arc::ptr_eq(&idle, &prev) {
                if cpu_state.runqueue().has_ownership_blocked(cpu_mask) {
                    cpu_state.request_resched();
                }
                return;
            }
            assert!(
                idle.arch_context().is_some(),
                "[sched] idle task lost arch context"
            );
            idle.set_state(TaskState::Running);
            idle.sched.set_on_rq(false);
            idle
        }
    };
    if cpu_state.runqueue().has_ownership_blocked(cpu_mask) {
        // 迁移任务可能先进入目标 rq，源 CPU 随后才在切换汇编中释放执行所有权。
        // 保留 resched 可让目标 idle 在这个极短窗口内重试，避免一次 IPI 被提前
        // 消费后任务永久滞留在目标 rq。
        cpu_state.request_resched();
    }
    migrate_local_ineligible_or_request_balance(&prev, cpu_id);

    // 3. 自己被选回：继续跑即可。
    if Arc::ptr_eq(&prev, &next) {
        return;
    }
    #[cfg(feature = "performance-profile")]
    let prepare_start = {
        let now = profiling::read_counter();
        profiling::observe(
            profiling::Metric::SchedPickCycles,
            now.wrapping_sub(pick_start),
        );
        now
    };
    assert!(
        next.try_claim_cpu(cpu_id),
        "[sched] selected task is still owned by cpu {:?}: pid={:?} target_cpu={}",
        next.running_cpu(),
        next.pid_root(),
        cpu_id,
    );
    prev.mark_rseq_event(crate::rseq::RseqEvent::Preempt);
    let account_now_ns = if now_ns == 0 {
        now_ns_internal()
    } else {
        now_ns
    };
    #[cfg(feature = "performance-profile")]
    {
        prev.mark_profile_blocked();
        let prev_id = prev.pid_root_cached().unwrap_or(0) as u64;
        let next_id = next.pid_root_cached().unwrap_or(0) as u64;
        profiling::record(profiling::Event::SchedSwitch, 0, 0, 1);
        profiling::trace_task_event_with_span(
            profiling::TraceKind::SchedSwitch,
            profiling::Event::SchedSwitch,
            prev_id,
            prev.profile_span_id(),
            prev_id,
            next_id,
        );
    }
    prev.account_switch_out(account_now_ns);
    next.account_switch_in(account_now_ns);
    prev.record_involuntary_context_switch();
    let final_prev = matches!(prev.state(), TaskState::Zombie | TaskState::Dead);
    #[cfg(feature = "performance-profile")]
    let publish_start = {
        let now = profiling::read_counter();
        profiling::observe(
            profiling::Metric::SchedPrepareAccountingCycles,
            now.wrapping_sub(prepare_start),
        );
        now
    };

    // 4. 发布下一任务为本 CPU current。
    //
    // 后续 vm_switch 会激活 next 的页表，task_cpu_state 可能通过 copy_to_user
    // 刷新 rseq。内核态 uaccess 的缺页处理依赖 current_task()->VmSpace，所以
    // 必须先发布 current，再触碰 next 的用户地址空间。
    bind_task_to_cpu(&next, cpu_id);
    publish_current_task(cpu_id, Arc::clone(&next));

    // 5. 取 ctx。
    let prev_ctx = prev
        .arch_context()
        .expect("[sched] prev task has no arch context");
    let next_ctx = next
        .arch_context()
        .expect("[sched] next task has no arch context");
    #[cfg(feature = "performance-profile")]
    let vm_start = {
        let now = profiling::read_counter();
        profiling::observe(
            profiling::Metric::SchedPreparePublishCycles,
            now.wrapping_sub(publish_start),
        );
        now
    };

    // 6. 切换前先把"内核 trap 入口栈"指向 next 的内核栈顶。
    if let Some(top) = next.kernel_stack_top() {
        if let Some(trap) = arch_hooks::trap() {
            unsafe { (trap.set_kernel_trap_stack)(top) };
        }
    }

    // 7. 切换用户地址空间。sched 不认识 VmSpace；由 kernel 启动期注册回调，
    //    回调内部做 ext_lookup + downcast + activate。必须在 switch_context
    //    之前完成，否则新任务返回用户态时仍可能使用 prev 的页表。
    if let Some(sw) = crate::arch_hooks::vm_switch() {
        (sw.on_switch)(&next);
    }
    #[cfg(feature = "performance-profile")]
    let cpu_state_start = {
        let now = profiling::read_counter();
        profiling::observe(
            profiling::Metric::SchedPrepareVmCycles,
            now.wrapping_sub(vm_start),
        );
        now
    };

    // 8. 发布用户态可观察的 CPU 状态。此时 next 的地址空间已经激活，且未
    //    进入 switch_context；回调不能影响调度决策，失败只会让用户态走保守路径。
    if let Some(cpu_state) = crate::arch_hooks::task_cpu_state() {
        (cpu_state.publish_current_cpu)(&next, cpu_id);
    }
    #[cfg(feature = "performance-profile")]
    {
        let now = profiling::read_counter();
        profiling::observe(
            profiling::Metric::SchedPrepareCpuStateCycles,
            now.wrapping_sub(cpu_state_start),
        );
        profiling::observe(
            profiling::Metric::SchedPrepareCycles,
            now.wrapping_sub(prepare_start),
        );
        CONTEXT_SWITCH_STARTED[cpu_id].store(now, Ordering::Release);
    }

    // 9. 切换。
    // Safety: 两侧 ctx 都已初始化；调用前所有锁已释放；调用期间不触发重入。
    let prev_on_cpu = prev.on_cpu_slot();
    #[cfg(feature = "syscall-model-markers")]
    profiling::syscall_model_task_switch(
        prev.profile_session_id(),
        prev.syscall_model_task_id(),
        false,
    );
    unsafe {
        if final_prev {
            drop(next);
            retire_final_task(cpu_id, prev);
            (crate::arch_hooks::ops_or_panic().switch_context)(prev_ctx, next_ctx, prev_on_cpu);
            core::hint::unreachable_unchecked();
        } else {
            (crate::arch_hooks::ops_or_panic().switch_context)(prev_ctx, next_ctx, prev_on_cpu);
        }
    }
    #[cfg(feature = "syscall-model-markers")]
    profiling::syscall_model_task_switch(
        prev.profile_session_id(),
        prev.syscall_model_task_id(),
        true,
    );
    #[cfg(feature = "performance-profile")]
    {
        let started = CONTEXT_SWITCH_STARTED[cpu_id].swap(0, Ordering::AcqRel);
        if started != 0 {
            profiling::observe(
                profiling::Metric::SchedContextCycles,
                profiling::read_counter().wrapping_sub(started),
            );
        }
    }
    // 被切回后正常返回。
}

fn targeted_handoff_is_valid(target: &HandoffTarget, cpu_id: usize) -> bool {
    let reason_valid = matches!(
        (target.reason, target.woke_from_sleep),
        (HandoffReason::SocketRead, true) | (HandoffReason::SocketReadContinuation, false)
    );
    target.request_generation != 0
        && reason_valid
        && target.preferred_cpu == cpu_id
        && target.task.current_cpu() == cpu_id
        && target.task.state() == TaskState::Runnable
        && target.task.sched.on_rq()
        && target.task.arch_context().is_some()
        && target.task.running_cpu().is_none()
        && target.task.cpu_affinity() & CpuMask::single_raw(cpu_id).bits() != 0
        && target.task.sched.class() == SchedClass::Fair
        && !target.task.signal.has_any_pending()
        && target.task.shared_signal_pending_bits_quick() == 0
}

// ── 定时器 / 抢占 ─────────────────────────────────────────────────────────────

/// 记录一次发生在内核临界区中的 timer tick，等待下一处安全调度边界处理。
pub fn defer_timer_tick(now_ns: u64) {
    if INIT_READY.load(Ordering::Acquire) {
        let cpu_id = cpu();
        record_deferred_timer_tick(&DEFERRED_TIMER_TICK_NS[cpu_id], now_ns);
        SCHEDULER.cpu_or_boot(cpu_id).mark_user_return_work();
    }
}

/// 在不持有调度器内部锁的边界消费本 CPU 延迟的 timer tick。
pub fn drain_deferred_timer_tick() {
    drain_deferred_timer_tick_on(cpu());
}

#[inline]
fn drain_deferred_timer_tick_on(cpu_id: usize) {
    let now_ns = take_deferred_timer_tick(&DEFERRED_TIMER_TICK_NS[cpu_id]);
    if now_ns != 0 {
        let _ = on_timer_tick_inner(now_ns);
    }
}

/// 定时器中断回调。推进 current 的虚拟时间，若时间片用完则请求 reschedule。
///
/// 真正的切换由 trap 返回路径上的 [`preempt_if_needed`] 完成。返回值表示本次
/// tick 是否唤醒 deadline sleeper 或触发实时计时器，供架构层优先调度。
///
/// TODO(smp): 每个 AP 的本地 timer 中断都必须调用该接口。
pub fn on_timer_tick(now_ns: u64) -> bool {
    if !INIT_READY.load(Ordering::Acquire) {
        return false;
    }
    let deferred_ns = take_deferred_timer_tick(&DEFERRED_TIMER_TICK_NS[cpu()]);
    on_timer_tick_inner(now_ns.max(deferred_ns))
}

pub(crate) fn record_deferred_timer_tick(slot: &AtomicU64, now_ns: u64) {
    slot.fetch_max(now_ns, Ordering::AcqRel);
}

pub(crate) fn take_deferred_timer_tick(slot: &AtomicU64) -> u64 {
    if slot.load(Ordering::Acquire) == 0 {
        0
    } else {
        slot.swap(0, Ordering::AcqRel)
    }
}

fn on_timer_tick_inner(now_ns: u64) -> bool {
    let cpu_id = cpu();
    let fired = service_expired_timer_events(now_ns, cpu_id);
    if SCHEDULER.cpu_or_boot(cpu_id).runqueue().tick(now_ns) {
        request_resched(cpu_id);
    }
    request_periodic_balance(cpu_id, now_ns);
    fired
}

fn service_expired_timer_events(now_ns: u64, cpu_id: usize) -> bool {
    let had_scheduler_deadline =
        HAS_TIMED_SLEEPERS.load(Ordering::Acquire) || HAS_REALTIME_ITIMERS.load(Ordering::Acquire);
    fire_expired_deadline_observers(now_ns);
    let deadline_fired = wake_expired_sleepers(now_ns, cpu_id);
    let realtime_fired = fire_expired_realtime_itimers(now_ns, cpu_id);
    #[cfg(feature = "performance-profile")]
    let profile_deadline = profiling::enabled() && profiling::sampling_enabled();
    #[cfg(not(feature = "performance-profile"))]
    let profile_deadline = false;
    if had_scheduler_deadline || profile_deadline {
        reprogram_deadline_timer();
    }
    deadline_fired || realtime_fired
}

/// 周期性负载均衡的最小间隔。
///
/// 对应 Linux `scheduler_tick()` 里的 `trigger_load_balance()`，但节流更保守：
/// `balance_once` 需要取全局 `RUNQUEUE_SNAPSHOT_LOCK` 并扫描所有 rq，若每个
/// tick 都在每个 CPU 上执行，多核空闲系统会把这条全局串行路径变成新的瓶颈。
/// 4 ms 实测过于激进：8 核每核每 4 ms 触发一次全 rq 扫描，光是 balance_once
/// 自身就吃掉约 9% 的内核指令，而其中绝大多数调用并没有偷到任务。16 ms 与
/// Linux 各级 sched domain 的 balance interval 量级相当，同时保留了把任务铺开
/// 到所有核所需的频度（work-stealing 主要靠 idle 路径即时触发，tick 只是兜底）。
const PERIODIC_BALANCE_INTERVAL_NS: u64 = 16_000_000;

/// 每 CPU 的下一次允许均衡时间点。
static NEXT_BALANCE_NS: [AtomicU64; NR_CPUS] = [const { AtomicU64::new(0) }; NR_CPUS];

/// 在 timer tick 上按间隔请求一次负载均衡。
///
/// 只在本 CPU 明显吃不满（runnable 不足以占满自己）时才请求：balance_once 的
/// 拉取条件本身也要求源 CPU 更忙，所以繁忙 CPU 上的请求只会白跑一次全 rq 扫描。
/// 真正的迁移动作留给安全调度边界（`preempt_if_needed` / idle 循环）消费
/// `take_balance()`，本函数不在中断上下文里直接迁移任务。
fn request_periodic_balance(cpu_id: usize, now_ns: u64) {
    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    if cpu_state.runqueue().nr_running() > 1 {
        return;
    }
    let slot = &NEXT_BALANCE_NS[cpu_id.min(NR_CPUS - 1)];
    let next = slot.load(Ordering::Relaxed);
    if now_ns < next {
        return;
    }
    if slot
        .compare_exchange(
            next,
            now_ns.saturating_add(PERIODIC_BALANCE_INTERVAL_NS),
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    // 只置本地标志位，不发 IPI：请求的目标就是本 CPU，它马上就会走到自己的
    // 调度边界。发自 IPI 只会在 idle 唤醒路径上制造额外往返。
    cpu_state.request_balance();
    cpu_state.request_resched();
}

/// 读取当前任务真实 CPU 时间的无分配回调。
pub fn current_task_cpu_time_ns() -> u64 {
    if !INIT_READY.load(Ordering::Acquire) {
        return 0;
    }
    current_task_ref().cpu_runtime_ns(now_ns_internal())
}

/// 在合适的边界（trap 返回前、syscall 返回前、显式让渡入口）调用。读到
/// CPU 的 reschedule 意图被置位时即清零并切一次。
///
/// TODO(smp): AP 的 timer、reschedule IPI 和 syscall/trap 返回路径都必须在
/// 恢复用户态前经过该接口；硬中断处理程序不得直接切换上下文。
pub fn preempt_if_needed(now_ns: u64) {
    if !INIT_READY.load(Ordering::Acquire) {
        return;
    }
    let cpu_id = cpu();
    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    if cpu_state.take_resched() {
        if cpu_state.take_balance() {
            let _ = balance_once(cpu_id);
        }
        schedule_once(now_ns);
    }
}

// ── idle 任务 ────────────────────────────────────────────────────────────────

/// idle 入口。空 runqueue 回落到这里；本身只主动让渡 + spin_loop hint，把
/// "硬件 wfi"等节能指令留到后续接 arch 钩子。
unsafe extern "C" fn idle_entry(_cpu_arg: usize) -> ! {
    loop {
        service_idle_scheduler_requests(cpu());
        // 队列空时 schedule_once 也会走"idle == prev"的快路径直接返回；
        // 一旦有 runnable 任务进来，下一次 schedule_once 会把 idle 切走。
        let now_ns = now_ns_internal();
        schedule_once(now_ns);
        if let Some(ops) = arch_hooks::idle() {
            (ops.idle_relax)();
        } else {
            core::hint::spin_loop();
        }
    }
}

/// 把 idle 任务装到指定 CPU 的槽位。一个 CPU 同时只能有一个 idle。
pub fn install_idle(cpu_id: usize, t: Arc<Task>) {
    mark_cpu_online(cpu_id).expect("[sched] invalid idle cpu id");
    bind_task_to_cpu(&t, cpu_id);
    let installed = SCHEDULER.cpu_or_boot(cpu_id).install_idle(t);
    assert!(
        installed.is_ok(),
        "[sched] idle slot for cpu {} already filled",
        cpu_id
    );
}

/// 派生指定 CPU 的 idle 任务并装入对应 CpuSchedState。返回任务句柄。
pub fn spawn_idle_for(cpu_id: usize) -> Arc<Task> {
    // 权重最低（nice=19）；slice 用默认值。这样任何 runnable 任务都会先于
    // idle 被选中，idle 仅在无人可跑时占位。
    let params = SchedParams {
        nice: 19,
        slice_ns: 0,
    };
    let t = crate::spawn::kthread_create(idle_entry, cpu_id, params);
    t.mark_idle_task();
    t.set_sched_attr(SchedAttr::idle());
    t.set_cpu_affinity(CpuMask::single_raw(cpu_id).bits());
    install_idle(cpu_id, Arc::clone(&t));
    log::info!(
        "[sched][idle] cpu={} pid={:?} weight={}",
        cpu_id,
        t.pid_root(),
        t.sched.weight(),
    );
    t
}

/// [`spawn_idle_for`] 的显式 SMP 命名别名，供架构 AP 启动路径使用。
///
/// AP 启动前由 boot CPU 创建并安装本 CPU 的 idle task。
pub fn spawn_idle_for_cpu(cpu_id: usize) -> Arc<Task> {
    spawn_idle_for(cpu_id)
}

/// AP 调度循环。架构代码应先调用
/// [`adopt_cpu_current`] / [`spawn_idle_for_cpu`] 完成本 CPU 槽位初始化。
///
/// secondary entry 完成 per-CPU 初始化后以此作为调度入口；
/// 本函数会检查初始化状态、激活 CPU，并开始消费本地 runqueue。
pub fn cpu_start_scheduling(cpu_id: usize) -> ! {
    activate_cpu(cpu_id).expect("[sched] CPU must be online before scheduling loop");
    loop {
        service_idle_scheduler_requests(cpu_id);
        let now_ns = now_ns_internal();
        schedule_once(now_ns);
        if let Some(ops) = arch_hooks::idle() {
            (ops.idle_relax)();
        } else {
            core::hint::spin_loop();
        }
    }
}

fn service_idle_scheduler_requests(cpu_id: usize) {
    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);
    // idle 循环本身每轮都会执行一次 schedule_once，因此先消费通知位，
    // 防止已处理的请求让 idle_relax 永久跳过硬件等待。空闲核每次从硬件等待
    // 返回后主动拉取一次；本地已经出现普通任务时直接交给 schedule_once。
    let _ = cpu_state.take_resched();
    let balance_requested = cpu_state.take_balance();
    let local_nr_running = cpu_state.runqueue().nr_running();
    let now_ns = now_ns_internal();
    let next = &NEXT_BALANCE_NS[cpu_id.min(NR_CPUS - 1)];
    let _ = run_idle_balance_if_due(
        local_nr_running,
        balance_requested,
        now_ns,
        next,
        cpu_id,
        balance_once,
    );
}

#[inline]
fn run_idle_balance_if_due<F>(
    local_nr_running: usize,
    requested: bool,
    now_ns: u64,
    next_allowed: &AtomicU64,
    cpu_id: usize,
    balance: F,
) -> bool
where
    F: FnOnce(usize) -> bool,
{
    if local_nr_running > 1 {
        return false;
    }

    let next = now_ns.saturating_add(PERIODIC_BALANCE_INTERVAL_NS);
    if requested {
        next_allowed.store(next, Ordering::Relaxed);
        return balance(cpu_id);
    }

    let previous = next_allowed.load(Ordering::Relaxed);
    if now_ns < previous
        || next_allowed
            .compare_exchange(previous, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return false;
    }
    balance(cpu_id)
}

// ── exit 辅助 ────────────────────────────────────────────────────────────────

/// 触发一次同步退出：标记 exit_code + Zombie + wake exit_waiters。
/// 不切换 CPU；调用方随后自己调 [`schedule_once`]。
pub(crate) fn mark_task_exited(task: &Arc<Task>, code: ExitCode) {
    // 任务可能登记在任一 CPU 的 rq 上；远端 current 不能被本 CPU 直接摘掉，
    // 只能请求对方在调度边界观察到新状态后自行切走。
    #[cfg(feature = "trace-task-lifecycle")]
    let removed = dequeue_for_state_change(task, now_ns_internal());
    #[cfg(not(feature = "trace-task-lifecycle"))]
    let _ = dequeue_for_state_change(task, now_ns_internal());
    task.mark_exited(code);
    #[cfg(feature = "trace-task-lifecycle")]
    log::info!(
        "[sched][exit] pid={:?} code={} on_rq={} state={:?}",
        task.pid_root(),
        code.0,
        removed,
        task.state(),
    );
}

fn dequeue_for_state_change(task: &Arc<Task>, now_ns: u64) -> bool {
    dequeue_for_state_change_on(&SCHEDULER, task, cpu(), now_ns)
}

pub(crate) fn dequeue_for_state_change_on(
    scheduler: &crate::Scheduler,
    task: &Arc<Task>,
    local_cpu: usize,
    now_ns: u64,
) -> bool {
    // exit / stop / signal 路径必须看到稳定的 rq 归属，否则会去源 rq 摘一个
    // 已经不在那里的任务，让它带着 Zombie 状态留在目标 rq 上。
    wait_for_migration_to_settle(task);
    let Some(owner) = task_runqueue_cpu_on(scheduler, task) else {
        return false;
    };
    dequeue_on_cpu_for_state_change(scheduler, task, owner.get(), local_cpu, now_ns)
}

fn dequeue_on_cpu_for_state_change(
    scheduler: &crate::Scheduler,
    task: &Arc<Task>,
    cpu_id: usize,
    local_cpu: usize,
    now_ns: u64,
) -> bool {
    let rq = scheduler.cpu_or_boot(cpu_id).runqueue();
    if cpu_id == local_cpu {
        return rq.dequeue(task, now_ns);
    }
    if rq.dequeue_queued(task, now_ns) {
        return true;
    }
    if rq.is_current(task) {
        scheduler.cpu_or_boot(cpu_id).request_resched();
    }
    false
}

fn stopped_signal_is_fatal(target: &Arc<Task>, info: &SigInfo) -> bool {
    if info.sig == SignalNumber::SIGKILL {
        return true;
    }
    if target.signal.blocked_snapshot().has(info.sig) {
        return false;
    }
    let action = target.shared_signal().get_action(info.sig);
    if action.handler != SigHandler::Default {
        return false;
    }
    matches!(
        default_action(info.sig),
        DefaultAction::Term | DefaultAction::Core
    )
}

#[cfg(test)]
mod deadline_observer_tests {
    use super::*;
    use core::sync::atomic::AtomicU64;

    struct RearmObserver(AtomicU64);

    impl DeadlineObserver for RearmObserver {
        fn deadline_expired(&self, _registration: u64, now_ns: u64) -> Option<u64> {
            let calls = self.0.fetch_add(1, Ordering::AcqRel) + 1;
            (calls == 1).then_some(now_ns.saturating_add(10))
        }
    }

    #[test]
    fn periodic_deadline_reuses_registration_until_observer_stops() {
        let observer = Arc::new(RearmObserver(AtomicU64::new(0)));
        let subscriber: Arc<dyn DeadlineObserver> = observer.clone();
        let registration = reserve_deadline_observer_id();
        let deadline = u64::MAX - 100;
        assert!(register_deadline_observer(
            registration,
            deadline,
            Arc::downgrade(&subscriber),
        ));
        fire_expired_deadline_observers(deadline - 1);
        assert_eq!(observer.0.load(Ordering::Acquire), 0);
        fire_expired_deadline_observers(deadline);
        assert_eq!(observer.0.load(Ordering::Acquire), 1);
        fire_expired_deadline_observers(deadline + 10);
        assert_eq!(observer.0.load(Ordering::Acquire), 2);
    }

    #[cfg(feature = "performance-profile")]
    #[test]
    fn profiling_deadline_refreshes_after_sampling_rate_change() {
        let cpu_id = NR_CPUS - 1;
        profiling::start();
        profiling::set_sampling_enabled(true);
        assert!(profiling::set_sample_hz(250));
        invalidate_deadline_cache();

        let first = earliest_deadline(cpu_id).expect("首次采样截止时间");
        assert!(profiling::set_sample_hz(500));
        let second = earliest_deadline(cpu_id).expect("更新后的采样截止时间");

        profiling::set_sampling_enabled(false);
        profiling::stop();
        assert_ne!(first, second);
        assert!(second < first);
    }
}

#[cfg(test)]
mod idle_balance_tests {
    use super::{PERIODIC_BALANCE_INTERVAL_NS, run_idle_balance_if_due};
    use core::cell::Cell;
    use core::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn requested_idle_balance_runs_immediately() {
        let calls = Cell::new(0usize);
        let next = AtomicU64::new(1_000);
        let run = |cpu_id| {
            assert_eq!(cpu_id, 3);
            calls.set(calls.get() + 1);
            true
        };

        assert!(run_idle_balance_if_due(1, true, 100, &next, 3, run));
        assert_eq!(calls.get(), 1);
        assert_eq!(
            next.load(Ordering::Relaxed),
            100 + PERIODIC_BALANCE_INTERVAL_NS
        );
    }

    #[test]
    fn idle_balance_without_request_is_rate_limited() {
        let calls = Cell::new(0usize);
        let next = AtomicU64::new(200);
        let run = |_| {
            calls.set(calls.get() + 1);
            true
        };

        assert!(!run_idle_balance_if_due(1, false, 199, &next, 3, run));
        assert_eq!(calls.get(), 0);
        assert!(run_idle_balance_if_due(1, false, 200, &next, 3, run));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn idle_balance_skips_scan_when_local_queue_has_work() {
        let calls = Cell::new(0usize);
        let next = AtomicU64::new(0);
        let run = |_| {
            calls.set(calls.get() + 1);
            true
        };

        assert!(!run_idle_balance_if_due(2, true, 0, &next, 3, run));
        assert_eq!(calls.get(), 0);
    }
}
