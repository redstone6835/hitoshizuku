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
//! [`RUNQUEUES`] / [`CURRENT_TASKS`] / [`IDLE_TASKS`] / [`NEED_RESCHED`] 都是
//! 长度 [`NR_CPUS`] 的数组；用 [`current_cpu_id`] 选当前槽。AP 启动尚未落地，
//! 当前永远只有 CPU 0 被填充。

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch_hooks;
use crate::eevdf::SchedParams;
use crate::group::{ProcessGroup, Session, ThreadGroup};
use crate::pid::PidNamespace;
use crate::runqueue::Runqueue;
use crate::sched_class::SchedAttr;
use crate::signal::{SigInfo, SignalNumber};
use crate::sync::Spinlock;
use crate::task::Task;
use crate::{ExitCode, TaskState};

// ── per-CPU 容量 ──────────────────────────────────────────────────────────────

/// 支持的最大 CPU 数。SMP 启动落地之前只有 CPU 0 真正被使用；保留更大数组
/// 是为了让锁顺序、索引代码一次到位，AP 启动接入时无需重排数据结构。
pub const NR_CPUS: usize = 8;

// ── 全局锚点 ──────────────────────────────────────────────────────────────────

/// 每核运行队列。
static RUNQUEUES: [Runqueue; NR_CPUS] = [const { Runqueue::new() }; NR_CPUS];

/// 每核当前正在执行的任务。
static CURRENT_TASKS: [Spinlock<Option<Arc<Task>>>; NR_CPUS] =
    [const { Spinlock::new(None) }; NR_CPUS];

/// 每核 idle 任务句柄。`pick_next` 返 None 时 `schedule_once` 切到这里。
static IDLE_TASKS: [Spinlock<Option<Arc<Task>>>; NR_CPUS] =
    [const { Spinlock::new(None) }; NR_CPUS];

/// 每核抢占请求标志。定时器发现 `Runqueue::tick` 需要抢占时置位；trap 返回
/// 路径 / 主动 yield 入口读到 true 即调一次 [`schedule_once`]。
static NEED_RESCHED: [AtomicBool; NR_CPUS] = [const { AtomicBool::new(false) }; NR_CPUS];

/// 已上线 CPU 位图。CPU0 在 init 前后始终视为 online；AP 启动后通过
/// [`register_cpu`] 打开对应 bit。
static CPU_ONLINE: AtomicU64 = AtomicU64::new(1);

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
    let init_pid = root_ns
        .registry()
        .allocate(&init_task)
        .expect("[sched][init] failed to allocate pid for init");
    debug_assert_eq!(init_pid, 1, "[sched][init] init pid must be 1");
    init_task.register_pid(Arc::clone(&root_ns), init_pid);
    root_ns.set_ns_init_pid(init_pid);

    // 6) 登记为 CPU 0 的 current。其它核的 RUNQUEUES/CURRENT_TASKS 保持空槽，
    //    直到 AP 启动路径落地时各自 `adopt_current_context`。
    RUNQUEUES[0].set_current(Arc::clone(&init_task));

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
    *CURRENT_TASKS[0].lock() = Some(Arc::clone(&init_task));

    log::info!(
        "[sched][init] init task created pid={} nr_running={} weight={}",
        init_pid,
        RUNQUEUES[0].nr_running(),
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
pub fn runqueue() -> &'static Runqueue {
    &RUNQUEUES[cpu()]
}

/// 指定 CPU 的 runqueue。
pub fn runqueue_of(cpu_id: usize) -> &'static Runqueue {
    assert!(cpu_id < NR_CPUS, "[sched] runqueue cpu id out of range");
    &RUNQUEUES[cpu_id]
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
    CURRENT_TASKS[id]
        .lock()
        .clone()
        .expect("[sched] current_task called before sched::init() on this CPU")
}

/// 查询指定 CPU 上的 current；未登记时返回 None。
pub fn current_task_on(cpu_id: usize) -> Option<Arc<Task>> {
    if cpu_id >= NR_CPUS {
        return None;
    }
    CURRENT_TASKS[cpu_id].lock().clone()
}

/// 指定 CPU 上的 idle 任务句柄。
pub fn idle_task(cpu_id: usize) -> Option<Arc<Task>> {
    if cpu_id >= NR_CPUS {
        return None;
    }
    IDLE_TASKS[cpu_id].lock().clone()
}

/// 是否已完成 init（避免有人在早期路径误调 current_task）。
pub fn is_ready() -> bool {
    INIT_READY.load(Ordering::Acquire)
}

pub fn online_cpu_mask() -> u64 {
    CPU_ONLINE.load(Ordering::Acquire) & cpu_mask_all()
}

pub fn is_cpu_online(cpu_id: usize) -> bool {
    cpu_id < NR_CPUS && (online_cpu_mask() & cpu_bit(cpu_id)) != 0
}

pub fn register_cpu(cpu_id: usize) -> Result<(), errno::Errno> {
    if cpu_id >= NR_CPUS {
        return Err(errno::Errno::EINVAL);
    }
    CPU_ONLINE.fetch_or(cpu_bit(cpu_id), Ordering::AcqRel);
    Ok(())
}

/// AP 启动路径的调度接入口框架：把当前 CPU 正在执行的 task 登记为该 CPU 的
/// current。当前 kernel 启动链路不调用它；AP bring-up 落地时可直接接入。
pub fn adopt_cpu_current(cpu_id: usize, task: Arc<Task>) -> Result<(), errno::Errno> {
    if cpu_id >= NR_CPUS {
        return Err(errno::Errno::EINVAL);
    }
    register_cpu(cpu_id)?;
    task.set_current_cpu(cpu_id);
    if task.arch_context().is_none() {
        task.adopt_current_context();
    }
    RUNQUEUES[cpu_id].set_current(Arc::clone(&task));
    *CURRENT_TASKS[cpu_id].lock() = Some(task);
    Ok(())
}

pub fn needs_resched(cpu_id: usize) -> bool {
    cpu_id < NR_CPUS && NEED_RESCHED[cpu_id].load(Ordering::Acquire)
}

pub fn request_resched(cpu_id: usize) {
    if cpu_id >= NR_CPUS {
        return;
    }
    NEED_RESCHED[cpu_id].store(true, Ordering::Release);
    if cpu_id != cpu() {
        if let Some(ops) = arch_hooks::cpu_control() {
            if (ops.is_online)(cpu_id) {
                (ops.send_resched)(cpu_id);
            }
        }
    }
}

/// 按亲和性和当前负载选择目标 CPU。没有匹配 online CPU 时回退到 CPU0。
pub fn select_task_cpu(task: &Arc<Task>) -> usize {
    let allowed = task.cpu_affinity() & online_cpu_mask();
    let allowed = if allowed == 0 { 1 } else { allowed };
    let current = task.current_cpu();
    if task.state() != TaskState::New && current < NR_CPUS && (allowed & cpu_bit(current)) != 0 {
        return current;
    }
    let mut best_cpu = 0usize;
    let mut best_load = usize::MAX;
    for cpu_id in 0..NR_CPUS {
        if (allowed & cpu_bit(cpu_id)) == 0 {
            continue;
        }
        let load = RUNQUEUES[cpu_id].nr_running();
        if load < best_load {
            best_cpu = cpu_id;
            best_load = load;
        }
    }
    best_cpu
}

/// 统一入队入口：设置任务 CPU 归属、入目标 runqueue、请求该 CPU 调度。
pub fn enqueue_task(task: Arc<Task>, now_ns: u64) -> usize {
    if task.sched.on_rq() {
        let cpu_id = task.current_cpu().min(NR_CPUS - 1);
        request_resched(cpu_id);
        return cpu_id;
    }
    let cpu_id = select_task_cpu(&task);
    task.set_current_cpu(cpu_id);
    RUNQUEUES[cpu_id].enqueue(Arc::clone(&task), now_ns);
    request_resched(cpu_id);
    cpu_id
}

pub fn migrate_task(task: &Arc<Task>, target_cpu: usize) -> Result<(), errno::Errno> {
    if target_cpu >= NR_CPUS || !is_cpu_online(target_cpu) {
        return Err(errno::Errno::EINVAL);
    }
    if (task.cpu_affinity() & cpu_bit(target_cpu)) == 0 {
        return Err(errno::Errno::EINVAL);
    }
    if task.state() == TaskState::Running {
        return Err(errno::Errno::EBUSY);
    }
    if !task.sched.on_rq() {
        task.set_current_cpu(target_cpu);
        return Ok(());
    }
    for rq in RUNQUEUES.iter() {
        let _ = rq.dequeue(task, now_ns_internal());
    }
    task.set_current_cpu(target_cpu);
    RUNQUEUES[target_cpu].enqueue(Arc::clone(task), now_ns_internal());
    request_resched(target_cpu);
    Ok(())
}

/// 从最忙 CPU 拉一个任务到 `cpu_id`。AP 启动后可由 idle/tick 路径周期调用。
pub fn balance_once(cpu_id: usize) -> bool {
    if !is_cpu_online(cpu_id) {
        return false;
    }
    let local_load = RUNQUEUES[cpu_id].nr_running();
    let mut busiest = None;
    let mut busiest_load = local_load;
    for other in 0..NR_CPUS {
        if other == cpu_id || !is_cpu_online(other) {
            continue;
        }
        let load = RUNQUEUES[other].nr_running();
        if load > busiest_load + 1 {
            busiest = Some(other);
            busiest_load = load;
        }
    }
    let Some(src) = busiest else {
        return false;
    };
    let allowed = cpu_bit(cpu_id);
    let Some(task) = RUNQUEUES[src].take_migratable(allowed, now_ns_internal()) else {
        return false;
    };
    task.set_current_cpu(cpu_id);
    RUNQUEUES[cpu_id].enqueue(task, now_ns_internal());
    request_resched(cpu_id);
    true
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
    if target.cas_state(TaskState::Sleeping, TaskState::Runnable) {
        enqueue_task(Arc::clone(target), now_ns_internal());
    }
    // Running / Runnable：pending 位已经设好；下一轮 schedule 自然会检查。
    // Stopped：只有 SIGCONT 可以恢复；其它信号保持 pending。
    // Uninterruptible / Zombie / Dead：什么都不做。
}

/// 把任务切入停止态：从 runqueue/current 中摘掉并记录可等待的 stopped 事件。
pub(crate) fn mark_task_stopped(task: &Arc<Task>, sig: SignalNumber) -> bool {
    let mut removed = false;
    for rq in RUNQUEUES.iter() {
        if rq.dequeue(task, now_ns_internal()) {
            removed = true;
            break;
        }
    }
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

    // 1. 取 prev（不持 CURRENT_TASKS 锁跨切换）。
    let Some(prev) = CURRENT_TASKS[cpu_id].lock().clone() else {
        return;
    };

    // 在调度边界消费当前任务的 pending signal。默认 Term/Core 会把 prev 标成
    // Zombie；后续 pick_next 看到它不再 runnable，就不会放回 runqueue。
    if prev.state() == TaskState::Running {
        let _ = crate::operation::deliver_pending_signals();
    }

    // 2. 挑下一个；pick_next 会把 prev 放回 tree（若仍 runnable）。
    let next = match RUNQUEUES[cpu_id].pick_next(now_ns) {
        Some(t) => t,
        None => {
            // 队列空：回落到本核 idle。idle 未安装则保持 prev 不切。
            let idle = IDLE_TASKS[cpu_id].lock().clone();
            let Some(idle) = idle else {
                return;
            };
            if Arc::ptr_eq(&idle, &prev) {
                return;
            }
            idle.set_state(TaskState::Running);
            idle.sched.set_on_rq(false);
            idle
        }
    };

    // 3. 自己被选回：继续跑即可。
    if Arc::ptr_eq(&prev, &next) {
        return;
    }

    // 4. 更新 CURRENT_TASKS。
    *CURRENT_TASKS[cpu_id].lock() = Some(Arc::clone(&next));
    next.set_current_cpu(cpu_id);

    // 5. 取 ctx。
    let prev_ctx = prev
        .arch_context()
        .expect("[sched] prev task has no arch context");
    let next_ctx = next
        .arch_context()
        .expect("[sched] next task has no arch context");

    // 6. 切换前先把"内核 trap 入口栈"指向 next 的内核栈顶。这一步必须在
    //    switch_context 调用之前完成——否则 switch_context 期间若发生中断，
    //    硬件会把现场写到 prev 的栈上，破坏 prev 的待恢复状态。idle 与
    //    kthread 必有栈；init 用 adopt_current_context 没分配栈，跳过。
    if let Some(top) = next.kernel_stack_top() {
        if let Some(trap) = arch_hooks::trap() {
            // Safety: top 来自 next.kstack 的栈顶（高地址），仍由 next 持有；
            //         arch 实现保证只写硬件寄存器、不解引用栈内容。
            unsafe { (trap.set_kernel_trap_stack)(top) };
        }
    }

    // 7. 切换用户地址空间。sched 不认识 VmSpace；由 kernel 启动期注册回调，
    //    回调内部做 ext_lookup + downcast + activate。必须在 switch_context
    //    之前完成，否则新任务返回用户态时仍可能使用 prev 的页表。
    if let Some(sw) = crate::arch_hooks::vm_switch() {
        (sw.on_switch)(&next);
    }

    // 8. 切换。
    // Safety: 两侧 ctx 都已初始化；调用前所有锁已释放；调用期间不触发重入。
    unsafe {
        (crate::arch_hooks::ops_or_panic().switch_context)(prev_ctx, next_ctx);
    }
    // 被切回后正常返回。
}

// ── 定时器 / 抢占 ─────────────────────────────────────────────────────────────

/// 定时器中断回调。推进 current 的虚拟时间，若时间片用完则置 NEED_RESCHED。
/// 真正的切换由 trap 返回路径上的 [`preempt_if_needed`] 完成——本函数仅
/// 记录意图，避免在 IRQ 上下文里持锁切换造成栈污染。
pub fn on_timer_tick(now_ns: u64) {
    if !INIT_READY.load(Ordering::Acquire) {
        return;
    }
    let cpu_id = cpu();
    if RUNQUEUES[cpu_id].tick(now_ns) {
        request_resched(cpu_id);
    }
}

/// 在合适的边界（trap 返回前、syscall 返回前、显式让渡入口）调用。读到
/// NEED_RESCHED 即清零并切一次。
pub fn preempt_if_needed(now_ns: u64) {
    if !INIT_READY.load(Ordering::Acquire) {
        return;
    }
    let cpu_id = cpu();
    if NEED_RESCHED[cpu_id].swap(false, Ordering::AcqRel) {
        schedule_once(now_ns);
    }
}

// ── idle 任务 ────────────────────────────────────────────────────────────────

/// idle 入口。空 runqueue 回落到这里；本身只主动让渡 + spin_loop hint，把
/// "硬件 wfi"等节能指令留到后续接 arch 钩子。
unsafe extern "C" fn idle_entry(_cpu_arg: usize) -> ! {
    loop {
        let _ = balance_once(cpu());
        // 队列空时 schedule_once 也会走"idle == prev"的快路径直接返回；
        // 一旦有 runnable 任务进来，下一次 schedule_once 会把 idle 切走。
        schedule_once(now_ns_internal());
        core::hint::spin_loop();
    }
}

/// 把 idle 任务装到指定 CPU 的槽位。一个 CPU 同时只能有一个 idle。
pub fn install_idle(cpu_id: usize, t: Arc<Task>) {
    register_cpu(cpu_id).expect("[sched] invalid idle cpu id");
    t.set_current_cpu(cpu_id);
    let mut slot = IDLE_TASKS[cpu_id].lock();
    debug_assert!(
        slot.is_none(),
        "[sched] idle slot for cpu {} already filled",
        cpu_id
    );
    *slot = Some(t);
}

/// 派生指定 CPU 的 idle 任务并装入 IDLE_TASKS。返回任务句柄。
pub fn spawn_idle_for(cpu_id: usize) -> Arc<Task> {
    // 权重最低（nice=19）；slice 用默认值。这样任何 runnable 任务都会先于
    // idle 被选中，idle 仅在无人可跑时占位。
    let params = SchedParams {
        nice: 19,
        slice_ns: 0,
    };
    let t = crate::spawn::kthread_create(idle_entry, cpu_id, params);
    t.sched.set_sched_attr(SchedAttr::idle());
    if cpu_id < 64 {
        t.set_cpu_affinity(1u64 << cpu_id);
    }
    install_idle(cpu_id, Arc::clone(&t));
    log::info!(
        "[sched][idle] cpu={} pid={:?} weight={}",
        cpu_id,
        t.pid_root(),
        t.sched.weight(),
    );
    t
}

/// [`spawn_idle_for`] 的显式 SMP 命名别名，供未来 AP bring-up 使用。
pub fn spawn_idle_for_cpu(cpu_id: usize) -> Arc<Task> {
    spawn_idle_for(cpu_id)
}

/// AP 调度循环框架。当前启动链路不接入；真实 AP 代码应先调用
/// [`adopt_cpu_current`] / [`spawn_idle_for_cpu`] 完成本 CPU 槽位初始化。
pub fn cpu_start_scheduling(cpu_id: usize) -> ! {
    register_cpu(cpu_id).expect("[sched] invalid CPU id for scheduling loop");
    loop {
        let _ = balance_once(cpu_id);
        schedule_once(now_ns_internal());
        core::hint::spin_loop();
    }
}

// ── exit 辅助 ────────────────────────────────────────────────────────────────

/// 触发一次同步退出：标记 exit_code + Zombie + wake exit_waiters。
/// 不切换 CPU；调用方随后自己调 [`schedule_once`]。
pub(crate) fn mark_task_exited(task: &Arc<Task>, code: ExitCode) {
    // 任务可能登记在任一 CPU 的 rq 上；目前只有 CPU 0，逐核 dequeue 保证
    // 未来 SMP 接入也不用改。
    let mut removed = false;
    for rq in RUNQUEUES.iter() {
        if rq.dequeue(task, 0) {
            removed = true;
            break;
        }
    }
    task.mark_exited(code);
    log::debug!(
        "[sched][exit] pid={:?} code={} on_rq={} state={:?}",
        task.pid_root(),
        code.0,
        removed,
        task.state(),
    );
}

const fn cpu_mask_all() -> u64 {
    if NR_CPUS >= 64 {
        u64::MAX
    } else {
        (1u64 << NR_CPUS) - 1
    }
}

const fn cpu_bit(cpu_id: usize) -> u64 {
    if cpu_id >= 64 { 0 } else { 1u64 << cpu_id }
}
