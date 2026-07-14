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
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch_hooks;
use crate::cpu::{CpuId, CpuMask, MAX_CPUS, SchedPlacement, SchedTopology};
use crate::eevdf::{NICE_0_WEIGHT, SchedParams};
use crate::group::{ProcessGroup, Session, ThreadGroup};
use crate::ids::Uid;
use crate::migration::MigrationContext;
use crate::pid::PidNamespace;
use crate::runqueue::{Runqueue, RunqueueClassLoad};
use crate::sched_class::{SchedAttr, SchedClass};
use crate::scheduler_state::{SCHEDULER, TopologySnapshot};
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

    pub(crate) fn load_of(self, cpu: CpuId) -> usize {
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

    pub(crate) fn load_of(self, cpu: CpuId) -> RunqueueClassLoad {
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

struct TimedSleeper {
    deadline_ns: u64,
    task: Weak<Task>,
}

static TIMED_SLEEPERS: Spinlock<Vec<TimedSleeper>> = Spinlock::new(Vec::new());

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
    thread_group: Weak<ThreadGroup>,
}

static REALTIME_ITIMERS: Spinlock<Vec<RealtimeItimer>> = Spinlock::new(Vec::new());
static CPU_HOTPLUG_LOCK: Spinlock<()> = Spinlock::new(());

/// init 任务全局锚点。写入即 Release，读取必须 Acquire。
static mut INIT_TASK: Option<Arc<Task>> = None;
/// 根 PID namespace。所有任务在分配 pid 时至少在该 ns 中登记一次。
static mut ROOT_PID_NS: Option<Arc<PidNamespace>> = None;
static INIT_READY: AtomicBool = AtomicBool::new(false);

// ── 内部辅助 ──────────────────────────────────────────────────────────────────

/// 当前 CPU id。arch_hooks 未注入时退化为 0（单核场景）。
#[inline]
fn cpu() -> usize {
    let id = arch_hooks::time().map_or(0, |o| (o.current_cpu_id)());
    debug_assert!(id < NR_CPUS, "[sched] cpu id {} >= NR_CPUS", id);
    if id < NR_CPUS { id } else { 0 }
}

fn publish_current_task(cpu_id: usize, task: Arc<Task>) {
    SCHEDULER.cpu_or_boot(cpu_id).publish_current(task);
}

fn bind_task_to_cpu(task: &Task, cpu_id: usize) {
    bind_task_to_cpu_on(&SCHEDULER, task, cpu_id);
}

fn bind_task_to_cpu_on(scheduler: &crate::Scheduler, task: &Task, cpu_id: usize) {
    let cpu = CpuId::new(cpu_id).unwrap_or_else(CpuId::boot);
    let snapshot = scheduler.topology_snapshot();
    let domain_id = snapshot
        .topology
        .domain_for_cpu(cpu)
        .unwrap_or_else(|| snapshot.topology.root_domain())
        .id();
    task.bind_placement(cpu, domain_id, snapshot.generation);
}

pub fn current_cpu_id() -> usize {
    cpu()
}

/// 当前纳秒时间戳。未注入时返回 0，表示"不推进虚拟时间"。
#[inline]
fn now_ns_internal() -> u64 {
    arch_hooks::time().map_or(0, |o| (o.now_ns)())
}

/// 对外导出的时间戳访问器。上层 idle / main loop 要喂 `schedule_once` 用。
pub fn now_ns_public() -> u64 {
    now_ns_internal()
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
    SCHEDULER
        .cpu_or_boot(0)
        .runqueue()
        .set_current(Arc::clone(&init_task));

    // 7) 发布全局锚点。Release 保证其它核能看到上面所有字段的写入。
    // Safety: INIT_READY 的 assert 已保证此函数全程仅此一次进入；写入期间
    // 没有其它代码路径能读取 INIT_TASK / ROOT_PID_NS（它们必须先见到
    // INIT_READY=true）。
    unsafe {
        core::ptr::addr_of_mut!(INIT_TASK).write(Some(Arc::clone(&init_task)));
        core::ptr::addr_of_mut!(ROOT_PID_NS).write(Some(Arc::clone(&root_ns)));
    }
    INIT_READY.store(true, Ordering::Release);

    // 8) CPU 0 的 current 指向 init。
    publish_current_task(0, Arc::clone(&init_task));
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
pub fn init_task() -> Arc<Task> {
    assert!(
        INIT_READY.load(Ordering::Acquire),
        "[sched] init_task() called before sched::init()"
    );
    // Safety: INIT_READY=true 时 INIT_TASK 已写入且永不再变；Acquire load 与
    // init() 中的 Release store 配对。
    let slot = unsafe { &*core::ptr::addr_of!(INIT_TASK) };
    Arc::clone(
        slot.as_ref()
            .expect("[sched] INIT_TASK flag set but slot empty"),
    )
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
pub fn root_pid_ns() -> Arc<PidNamespace> {
    assert!(
        INIT_READY.load(Ordering::Acquire),
        "[sched] root_pid_ns() called before sched::init()"
    );
    // Safety: INIT_READY=true 时 ROOT_PID_NS 已写入且永不再变。
    let slot = unsafe { &*core::ptr::addr_of!(ROOT_PID_NS) };
    Arc::clone(
        slot.as_ref()
            .expect("[sched] ROOT_PID_NS flag set but slot empty"),
    )
}

/// 统计：当前根 ns 已占用的 pid 数（含 init）。
pub fn pid_count() -> usize {
    root_pid_ns().registry().allocated()
}

/// 当前 CPU 上正在执行的任务。
///
/// [`init`] 之后，在 CPU 0 上必然非空。AP 启动路径落地前，其它 CPU 调用此
/// 函数会 panic（目前不会发生，因为只有 CPU 0 跑代码）。
pub fn current_task() -> Arc<Task> {
    let id = cpu();
    SCHEDULER
        .cpu_or_boot(id)
        .current()
        .expect("[sched] current_task called before sched::init() on this CPU")
}

/// 当前 CPU 上正在执行的任务引用。
///
/// 该接口不增加引用计数，也不加锁；调用方不能把返回引用保存到可能调度之后。
pub fn current_task_ref() -> &'static Task {
    let id = cpu();
    let ptr = SCHEDULER.cpu_or_boot(id).current_raw();
    if ptr.is_null() {
        panic!("[sched] current_task_ref called before sched::init() on this CPU");
    }
    // Safety: raw 指针由 `publish_current_task` 的 `Arc::into_raw` 产生，并由
    // CpuSchedState 的 raw current 槽位持有强引用。
    unsafe { &*ptr }
}

/// 当前 CPU 上正在执行的任务句柄，热路径版本。
///
/// 与 [`current_task`] 语义相同，但不进入 owning current 锁。
pub fn current_task_fast() -> Arc<Task> {
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

/// 查询指定 CPU 上的 current；未登记时返回 None。
pub fn current_task_on(cpu_id: usize) -> Option<Arc<Task>> {
    SCHEDULER.cpu(cpu_id)?.current()
}

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

/// 是否已完成 init（避免有人在早期路径误调 current_task）。
pub fn is_ready() -> bool {
    INIT_READY.load(Ordering::Acquire)
}

pub fn online_cpu_mask() -> u64 {
    SCHEDULER.online_set().bits()
}

pub fn active_cpu_mask() -> u64 {
    SCHEDULER.active_set().bits()
}

pub const fn supported_cpu_mask() -> u64 {
    CpuMask::SUPPORTED.bits()
}

fn active_cpu_set() -> CpuMask {
    SCHEDULER.active_set()
}

pub fn sched_topology() -> SchedTopology {
    SCHEDULER.topology()
}

pub fn install_sched_topology(topology: SchedTopology) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    if !topology
        .root_domain()
        .span()
        .contains_mask(CpuMask::SUPPORTED)
    {
        return Err(errno::Errno::EINVAL);
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

pub fn current_sched_domain_id(cpu_id: usize) -> Option<usize> {
    let cpu = CpuId::new(cpu_id)?;
    sched_topology()
        .domain_for_cpu(cpu)
        .map(|domain| domain.id())
}

/// 刷新并返回指定调度域的负载统计。
pub fn sched_domain_stats(domain_id: usize) -> Option<crate::SchedDomainStats> {
    refresh_domain_stats();
    SCHEDULER.domain_stats(domain_id)
}

/// 查询某个任务在当前拓扑下的调度放置状态。
///
/// 返回值只是快照：函数不会迁移任务，也不承诺下一次入队一定选中同一 CPU。
/// 调用方可用它向用户态或诊断路径解释“为什么这个任务能在哪些 CPU 上运行”。
pub fn task_sched_placement(task: &Arc<Task>) -> SchedPlacement {
    task_sched_placement_on(&SCHEDULER, task)
}

fn task_sched_placement_on(scheduler: &crate::Scheduler, task: &Arc<Task>) -> SchedPlacement {
    let _ = refresh_task_placement(scheduler, task);
    let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    let active = scheduler.active_set();
    let current = CpuId::new(task.current_cpu());
    let prefer_current = task.state() != TaskState::New;
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
    RunqueueLoadSnapshot::collect(cpus, |cpu| {
        scheduler.cpu_or_boot(cpu.get()).runqueue().nr_running()
    })
}

fn refresh_domain_stats() {
    let snapshot = SCHEDULER.topology_snapshot();
    let loads = RunqueueClassLoadSnapshot::collect(snapshot.active, |cpu| {
        SCHEDULER.cpu_or_boot(cpu.get()).runqueue().class_load()
    });
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

pub fn is_cpu_online(cpu_id: usize) -> bool {
    CpuId::new(cpu_id).is_some_and(|cpu| SCHEDULER.online_set().contains(cpu))
}

pub fn is_cpu_active(cpu_id: usize) -> bool {
    CpuId::new(cpu_id).is_some_and(|cpu| SCHEDULER.active_set().contains(cpu))
}

pub fn mark_cpu_online(cpu_id: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    let cpu = CpuId::new(cpu_id).ok_or(errno::Errno::EINVAL)?;
    let _ = SCHEDULER.mark_cpu_online(cpu);
    Ok(())
}

/// TODO(smp): AP 只能在 current 和 idle 都安装完成后激活；底层通常应通过
/// [`cpu_start_scheduling`] 完成这一步，而不是提前直接调用本函数。
pub fn activate_cpu(cpu_id: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    let cpu = CpuId::new(cpu_id).ok_or(errno::Errno::EINVAL)?;
    if !cpu_ready_for_activation(&SCHEDULER, cpu_id) {
        return Err(errno::Errno::EBUSY);
    }
    if !SCHEDULER.activate_cpu(cpu) {
        return Err(errno::Errno::EINVAL);
    }
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
}

/// 将一个非启动 CPU 下线并迁移其排队任务。
pub fn offline_cpu(cpu_id: usize) -> Result<(), errno::Errno> {
    let _guard = CPU_HOTPLUG_LOCK.lock();
    offline_cpu_with_scheduler(&SCHEDULER, cpu_id, now_ns_internal())
}

pub(crate) fn offline_cpu_with_scheduler(
    scheduler: &crate::Scheduler,
    cpu_id: usize,
    now_ns: u64,
) -> Result<(), errno::Errno> {
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
    let snapshot = scheduler.topology_snapshot();
    let mut planned_load = collect_rq_load_snapshot_for(scheduler, snapshot.active);
    let mut tasks = Vec::new();
    for task in &drained {
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
        let Some(target_cpu) =
            snapshot
                .topology
                .select_cpu(allowed, snapshot.active, None, false, |cpu| {
                    planned_load.load_of(cpu)
                })
        else {
            restore_drained_tasks(scheduler, cpu, &drained, now_ns);
            return Err(errno::Errno::EBUSY);
        };
        planned_load.add_task(target_cpu);
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
        item.task
            .commit_migration(item.target_cpu, item.target_domain, snapshot.generation);
        if !scheduler
            .cpu_or_boot(item.target_cpu.get())
            .runqueue()
            .enqueue(Arc::clone(&item.task), now_ns)
        {
            restore_unmoved_tasks(scheduler, cpu, &tasks, index, now_ns);
            return Err(errno::Errno::EIO);
        }
        scheduler
            .cpu_or_boot(item.target_cpu.get())
            .request_resched();
    }

    let late = cpu_state.runqueue().drain_queued(now_ns);
    if !late.is_empty()
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
        let _ = scheduler
            .cpu_or_boot(cpu.get())
            .runqueue()
            .enqueue(Arc::clone(task), now_ns);
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
        let _ = scheduler
            .cpu_or_boot(cpu.get())
            .runqueue()
            .enqueue(Arc::clone(&item.task), now_ns);
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
        let _ = scheduler
            .cpu_or_boot(cpu.get())
            .runqueue()
            .enqueue(Arc::clone(&item.task), now_ns);
    }
}

/// AP 启动路径的调度接入口：把当前 CPU 正在执行的 task 登记为该 CPU 的 current。
///
/// AP 完成 per-CPU 栈、trap、页表和本地数据初始化后调用。
/// 本 CPU idle task，最后进入 [`cpu_start_scheduling`]。
pub fn adopt_cpu_current(cpu_id: usize, task: Arc<Task>) -> Result<(), errno::Errno> {
    if CpuId::new(cpu_id).is_none() {
        return Err(errno::Errno::EINVAL);
    }
    mark_cpu_online(cpu_id)?;
    bind_task_to_cpu(&task, cpu_id);
    if task.arch_context().is_none() {
        task.adopt_current_context();
    }
    SCHEDULER
        .cpu_or_boot(cpu_id)
        .runqueue()
        .set_current(Arc::clone(&task));
    publish_current_task(cpu_id, task);
    Ok(())
}

pub fn needs_resched(cpu_id: usize) -> bool {
    SCHEDULER
        .cpu(cpu_id)
        .is_some_and(|cpu_state| cpu_state.needs_resched())
}

pub fn needs_resched_current() -> bool {
    if !INIT_READY.load(Ordering::Acquire) {
        return false;
    }
    SCHEDULER.cpu_or_boot(cpu()).needs_resched()
}

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
            if (ops.is_online)(cpu_id) {
                (ops.send_resched)(cpu_id);
            }
        }
    }
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
    let rounds = cpu_state.take_post_syscall_handoff();
    if rounds == 0 {
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
    if !SCHEDULER.cpu_or_boot(cpu_id).has_post_syscall_handoff() {
        return;
    }
    run_post_syscall_handoff(now_ns_internal());
}

/// 按调度域、亲和性和当前负载选择目标 CPU。
pub fn select_task_cpu(task: &Arc<Task>) -> usize {
    task_sched_placement(task)
        .preferred_cpu
        .unwrap_or_else(CpuId::boot)
        .get()
}

/// 统一入队入口：设置任务 CPU 归属、入目标 runqueue、请求该 CPU 调度。
pub fn enqueue_task(task: Arc<Task>, now_ns: u64) -> usize {
    let cpu_id = enqueue_task_locked(task, now_ns, true);
    notify_resched(cpu_id);
    cpu_id
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

/// 只把任务放回 runqueue，不主动抢占当前任务。
///
/// vfork child 在 exec 完成时需要唤醒父进程，但 child 自己也刚拿到新的用户态
/// 镜像；若立刻抢占，父 shell 会继续执行下一条命令，而后台 daemon 还没机会
/// bind/listen。这个入口仅用于这类“父可运行，但当前 child 应先返回用户态”的
/// 场景。
pub fn enqueue_task_deferred(task: Arc<Task>, now_ns: u64) -> usize {
    enqueue_task_locked(task, now_ns, false)
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

    for _ in 0..NR_CPUS {
        let cpu_id = task_sched_placement_on(scheduler, &task)
            .preferred_cpu
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

/// Register a sleeping task for deadline-based wakeup.
///
/// The caller owns the actual sleep transition and must cancel the registration
/// after it resumes. This helper only records the timeout side channel used by
/// timer ticks to move an expired sleeper back to Runnable.
pub fn register_sleep_deadline(task: &Arc<Task>, deadline_ns: u64) -> bool {
    if now_ns_internal() >= deadline_ns {
        return false;
    }
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
        return true;
    }
    sleepers.push(TimedSleeper {
        deadline_ns,
        task: Arc::downgrade(task),
    });
    true
}

/// Remove all deadline wakeups registered for `task`.
pub fn cancel_sleep_deadline(task: &Arc<Task>) {
    let mut sleepers = TIMED_SLEEPERS.lock();
    sleepers.retain(|entry| {
        entry
            .task
            .upgrade()
            .as_ref()
            .is_some_and(|queued| !Arc::ptr_eq(queued, task))
    });
}

/// 查询当前线程组的 `ITIMER_REAL`。
pub fn get_realtime_itimer(task: &Arc<Task>) -> RealtimeItimerSpec {
    let tg = task.thread_group();
    let now_ns = now_ns_internal();
    let mut timers = REALTIME_ITIMERS.lock();
    timers.retain(|entry| entry.thread_group.upgrade().is_some());
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
            thread_group: Arc::downgrade(&tg),
        });
    }
    old
}

fn wake_expired_sleepers(now_ns: u64) {
    let expired = {
        let mut sleepers = TIMED_SLEEPERS.lock();
        let mut expired = Vec::new();
        sleepers.retain(|entry| {
            let Some(task) = entry.task.upgrade() else {
                return false;
            };
            if entry.deadline_ns <= now_ns {
                expired.push(task);
                false
            } else {
                true
            }
        });
        expired
    };
    for task in expired {
        if task.cas_state(TaskState::Sleeping, TaskState::Runnable) {
            enqueue_task(task, now_ns);
        }
    }
}

fn fire_expired_realtime_itimers(now_ns: u64) {
    let expired = {
        let mut timers = REALTIME_ITIMERS.lock();
        let mut expired = Vec::new();
        let mut idx = 0;
        while idx < timers.len() {
            let Some(tg) = timers[idx].thread_group.upgrade() else {
                timers.swap_remove(idx);
                continue;
            };
            if timers[idx].deadline_ns > now_ns {
                idx += 1;
                continue;
            }

            expired.push(tg);
            let interval_ns = timers[idx].interval_ns;
            if interval_ns == 0 {
                timers.swap_remove(idx);
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
            idx += 1;
        }
        expired
    };

    for tg in expired {
        deliver_sigalrm_to_thread_group(&tg);
    }
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
    tg.shared_signal().deliver(info);
    for task in tg.snapshot() {
        if !task.signal.blocked_snapshot().has(SignalNumber::SIGALRM)
            || task.signal.sigtimedwait_wants(SignalNumber::SIGALRM)
        {
            signal_wakeup(&task, &info);
            break;
        }
    }
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
        SCHEDULER
            .cpu_or_boot(context.source.cpu.map_or(0, CpuId::get))
            .runqueue()
            .enqueue(Arc::clone(task), now_ns_internal());
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
    let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
    let topology = SCHEDULER.topology_snapshot();
    if let Err(error) = validate_migration_target(context, topology, affinity) {
        rollback_migration(task, context, source_detached);
        return Err(error);
    }
    if !source_detached {
        let source_cpu = context.source.cpu.map_or(0, CpuId::get);
        if !SCHEDULER
            .cpu_or_boot(source_cpu)
            .runqueue()
            .dequeue_queued(task, now_ns_internal())
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
    task.commit_migration(
        context.target_cpu,
        context.target_domain,
        context.topology_generation,
    );
    if !SCHEDULER
        .cpu_or_boot(context.target_cpu.get())
        .runqueue()
        .enqueue(Arc::clone(task), now_ns_internal())
    {
        rollback_migration(task, context, true);
        return Err(errno::Errno::EIO);
    }
    request_resched(context.target_cpu.get());
    Ok(())
}

pub fn migrate_task(task: &Arc<Task>, target_cpu: usize) -> Result<(), errno::Errno> {
    if !task.sched.on_rq() {
        let target = CpuId::new(target_cpu).ok_or(errno::Errno::EINVAL)?;
        if !active_cpu_set().contains(target)
            || !CpuMask::from_bits_or_boot(task.cpu_affinity()).contains(target)
        {
            return Err(errno::Errno::EINVAL);
        }
        bind_task_to_cpu(task, target_cpu);
        return Ok(());
    }
    let context = migration_context(task, target_cpu)?;
    attach_migrated_task(task, context, false)
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

    let topology_snapshot = SCHEDULER.topology_snapshot();
    let topology = topology_snapshot.topology;
    let allowed = CpuMask::single(local_cpu).bits();
    let domain_loads = RunqueueClassLoadSnapshot::collect(active, |cpu| {
        SCHEDULER.cpu_or_boot(cpu.get()).runqueue().class_load()
    });
    SCHEDULER.update_domain_stats(topology_snapshot, domain_loads.loads());

    let load_snapshot = RunqueueClassLoadSnapshot::collect(active, |cpu| {
        SCHEDULER
            .cpu_or_boot(cpu.get())
            .runqueue()
            .migratable_class_load_for(allowed)
    });
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
        let target_load = domain_loads.load_of(local_cpu);
        let task_utilization = deadline_task_utilization(&task);
        if !deadline_has_capacity(
            target_load,
            task_utilization,
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
            if cpu_state.runqueue().enqueue(Arc::clone(&task), now_ns) {
                cpu_state.request_resched();
            }
            return source_cpu;
        }
    }
    enqueue_task_on_scheduler(scheduler, task, now_ns, false, true)
}

pub(crate) fn select_balance_source_for_class<F>(
    topology: SchedTopology,
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

fn deadline_task_utilization(task: &Arc<Task>) -> u64 {
    let runtime = task.sched.deadline_runtime_ns();
    let period = task.sched.deadline_period_ns();
    if runtime == 0 || period == 0 {
        return 0;
    }
    ((runtime as u128 * crate::SCHED_CAPACITY_SCALE as u128) / period as u128).min(u64::MAX as u128)
        as u64
}

pub(crate) fn deadline_has_capacity(
    target_load: RunqueueClassLoad,
    task_utilization: u64,
    capacity: u64,
) -> bool {
    target_load
        .deadline_utilization
        .saturating_add(task_utilization)
        <= capacity
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

/// 把一条信号投给 `target`，并在可行时把它从 Sleeping 拉回 Runnable。
///
/// 调用方已经把 `info` 放进了 target 的 per-task 或共享 pending 队列，这里
/// 只负责"是否需要唤醒"。`Uninterruptible` 任务不会被打断（Linux 语义）。
pub fn signal_wakeup(target: &Arc<Task>, info: &SigInfo) {
    if info.sig == SignalNumber::SIGCONT && continue_task(target) {
        return;
    }
    if target.state() == TaskState::Stopped && stopped_signal_is_fatal(target, info) {
        crate::spawn::exit_task(target, ExitCode((info.sig.raw() as i32) & 0x7f));
        return;
    }
    if target.cas_state(TaskState::Sleeping, TaskState::Runnable) {
        enqueue_task(Arc::clone(target), now_ns_internal());
    }
    // Running / Runnable：pending 位已经设好；下一轮 schedule 自然会检查。
    // Stopped：只有 SIGCONT 可以恢复；其它信号保持 pending。
    // Uninterruptible / Zombie / Dead：什么都不做。
}

/// 把任务切入停止态：从 runqueue/current 中摘掉并记录可等待的 stopped 事件。
pub(crate) fn mark_task_stopped(task: &Arc<Task>, sig: SignalNumber) -> bool {
    let removed = dequeue_for_state_change(task, now_ns_internal());
    let stopped = task.mark_stopped(sig);
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
pub fn schedule_once(now_ns: u64) {
    let cpu_id = cpu();
    cleanup_retired_tasks(cpu_id);

    let cpu_state = SCHEDULER.cpu_or_boot(cpu_id);

    // 1. 取 prev（不持 owning current 锁跨切换）。
    let Some(prev) = cpu_state.current() else {
        return;
    };

    // 在调度边界消费当前任务的 pending signal。默认 Term/Core 会把 prev 标成
    // Zombie；后续 pick_next 看到它不再 runnable，就不会放回 runqueue。
    if prev.state() == TaskState::Running
        && (prev.signal.has_any_pending() || prev.shared_signal_pending_bits_quick() != 0)
    {
        let _ = crate::operation::deliver_pending_signals();
    }

    // 2. 挑下一个；pick_next 会把 prev 放回 tree（若仍 runnable）。若 prev 的
    //    亲和性已经排除本 CPU，此时它已稳定停在旧 rq，可通知目标 CPU 拉取。
    let next = match cpu_state
        .runqueue()
        .pick_next_on(now_ns, CpuMask::single_raw(cpu_id).bits())
    {
        Some(t) => t,
        None => {
            // 队列空：回落到本核 idle。idle 未安装则保持 prev 不切。
            let idle = cpu_state.idle();
            let Some(idle) = idle else {
                return;
            };
            if Arc::ptr_eq(&idle, &prev) {
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
    migrate_local_ineligible_or_request_balance(&prev, cpu_id);

    // 3. 自己被选回：继续跑即可。
    if Arc::ptr_eq(&prev, &next) {
        return;
    }
    prev.record_involuntary_context_switch();
    let final_prev = matches!(prev.state(), TaskState::Zombie | TaskState::Dead);

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

    // 8. 发布用户态可观察的 CPU 状态。此时 next 的地址空间已经激活，且未
    //    进入 switch_context；回调不能影响调度决策，失败只会让用户态走保守路径。
    if let Some(cpu_state) = crate::arch_hooks::task_cpu_state() {
        (cpu_state.publish_current_cpu)(&next, cpu_id);
    }

    // 9. 切换。
    // Safety: 两侧 ctx 都已初始化；调用前所有锁已释放；调用期间不触发重入。
    unsafe {
        if final_prev {
            drop(next);
            retire_final_task(cpu_id, prev);
            (crate::arch_hooks::ops_or_panic().switch_context)(prev_ctx, next_ctx);
            core::hint::unreachable_unchecked();
        } else {
            (crate::arch_hooks::ops_or_panic().switch_context)(prev_ctx, next_ctx);
        }
    }
    // 被切回后正常返回。
}

// ── 定时器 / 抢占 ─────────────────────────────────────────────────────────────

/// 定时器中断回调。推进 current 的虚拟时间，若时间片用完则请求 reschedule。
/// 真正的切换由 trap 返回路径上的 [`preempt_if_needed`] 完成——本函数仅
/// 记录意图，避免在 IRQ 上下文里持锁切换造成栈污染。
///
/// TODO(smp): 每个 AP 的本地 timer 中断都必须调用该接口。
pub fn on_timer_tick(now_ns: u64) {
    if !INIT_READY.load(Ordering::Acquire) {
        return;
    }
    wake_expired_sleepers(now_ns);
    fire_expired_realtime_itimers(now_ns);
    let cpu_id = cpu();
    if SCHEDULER.cpu_or_boot(cpu_id).runqueue().tick(now_ns) {
        request_resched(cpu_id);
    }
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
        let _ = SCHEDULER.cpu_or_boot(cpu()).take_balance();
        let _ = balance_once(cpu());
        // 队列空时 schedule_once 也会走"idle == prev"的快路径直接返回；
        // 一旦有 runnable 任务进来，下一次 schedule_once 会把 idle 切走。
        schedule_once(now_ns_internal());
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
    t.sched.set_sched_attr(SchedAttr::idle());
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
        schedule_once(now_ns_internal());
        if let Some(ops) = arch_hooks::idle() {
            (ops.idle_relax)();
        } else {
            core::hint::spin_loop();
        }
    }
}

// ── exit 辅助 ────────────────────────────────────────────────────────────────

/// 触发一次同步退出：标记 exit_code + Zombie + wake exit_waiters。
/// 不切换 CPU；调用方随后自己调 [`schedule_once`]。
pub(crate) fn mark_task_exited(task: &Arc<Task>, code: ExitCode) {
    // 任务可能登记在任一 CPU 的 rq 上；远端 current 不能被本 CPU 直接摘掉，
    // 只能请求对方在调度边界观察到新状态后自行切走。
    #[cfg(feature = "trace-task-lifecycle")]
    let removed = dequeue_for_state_change(task, 0);
    #[cfg(not(feature = "trace-task-lifecycle"))]
    let _ = dequeue_for_state_change(task, 0);
    task.mark_exited(code);
    #[cfg(feature = "trace-task-lifecycle")]
    log::debug!(
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
