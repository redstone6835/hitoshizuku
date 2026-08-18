//! 多调度类运行队列。
//!
//! runqueue 对外仍是单 CPU 队列；内部按 `Deadline > Realtime > Fair > Idle`
//! 分层。Fair class 使用 EEVDF；RT class 提供 FIFO/RR 队列骨架；Deadline
//! class 提供 EDF + runtime budget 框架。AP 启动和真实 IPI 不在本模块内。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::cpu::SCHED_CAPACITY_SCALE;
use crate::eevdf::{SchedParams, scale_delta_by_weight};
use crate::sched_class::{
    DEFAULT_RT_PERIOD_NS, DEFAULT_RT_RUNTIME_NS, RT_PRIO_MAX, SchedAttr, SchedClass, SchedPolicy,
};
use crate::sync::Spinlock;
use crate::task::{Task, TaskState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FairKey {
    deadline: u64,
    addr: usize,
}

impl FairKey {
    fn of(task: &Arc<Task>) -> Self {
        Self {
            deadline: task.sched.deadline(),
            addr: task_addr(task),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RtKey {
    prio_key: u8,
    seq: u64,
    addr: usize,
}

impl RtKey {
    fn of(task: &Arc<Task>, seq: u64) -> Self {
        Self {
            prio_key: RT_PRIO_MAX.saturating_sub(task.sched.rt_priority()),
            seq,
            addr: task_addr(task),
        }
    }

    fn idle(task: &Arc<Task>, seq: u64) -> Self {
        Self {
            prio_key: u8::MAX,
            seq,
            addr: task_addr(task),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DeadlineKey {
    abs_deadline: u64,
    seq: u64,
    addr: usize,
}

impl DeadlineKey {
    fn of(task: &Arc<Task>, seq: u64) -> Self {
        Self {
            abs_deadline: task.sched.absolute_deadline_ns(),
            seq,
            addr: task_addr(task),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DeadlineThrottleKey {
    replenish_ns: u64,
    seq: u64,
    addr: usize,
}

impl DeadlineThrottleKey {
    fn of(task: &Arc<Task>, seq: u64) -> Self {
        Self {
            replenish_ns: task.sched.deadline_replenish_ns(),
            seq,
            addr: task_addr(task),
        }
    }
}

struct RqInner {
    fair_tree: BTreeMap<FairKey, Arc<Task>>,
    rt_tree: BTreeMap<RtKey, Arc<Task>>,
    deadline_tree: BTreeMap<DeadlineKey, Arc<Task>>,
    deadline_throttled: BTreeMap<DeadlineThrottleKey, Arc<Task>>,
    idle_tree: BTreeMap<RtKey, Arc<Task>>,
    queued_deadline_utilization: u64,
    total_weight: u128,
    weighted_vruntime_sum: u128,
    min_vruntime: u64,
    current: Option<Arc<Task>>,
    last_update_ns: u64,
    enqueue_seq: u64,
    preferred_fair_key: Option<FairKey>,
    rt_period_ns: u64,
    rt_runtime_ns: u64,
    rt_period_start_ns: u64,
    rt_runtime_used_ns: u64,
    rt_throttled: bool,
    /// 本 rq 所属 CPU 编号，仅用于双重入队检测；`RQ_OWNER_NONE` 表示未回填。
    owner_cpu: usize,
}

/// 单 CPU 运行队列。每个 online CPU 持有一份。
pub(crate) struct Runqueue {
    inner: Spinlock<RqInner>,
    published_deadline: AtomicUsize,
    published_deadline_utilization: AtomicU64,
    published_realtime: AtomicUsize,
    published_fair: AtomicUsize,
    published_fair_weight: AtomicU64,
}

/// 记录任务当前登记在哪个 CPU 的 rq 上，用于捕获双重入队。
///
/// 这是本次 SMP 修复的回归护栏：如果哪条路径绕过 `MIGRATING` 门禁把同一任务
/// 挂到两个 rq 上，会在这里立刻 panic 并指出两个 CPU 编号，而不是等到两个核
/// 各自切入同一份 arch context 后，在 `user_clone_entry` 里报出一个与根因相距
/// 甚远的 "missing saved trap frame"。
const RQ_OWNER_NONE: usize = usize::MAX;

fn assert_rq_ownership_acquired(task: &Arc<Task>, owner_cpu: usize, site: &str) {
    if owner_cpu == RQ_OWNER_NONE {
        return;
    }
    let previous = task
        .rq_owner
        .swap(owner_cpu, core::sync::atomic::Ordering::AcqRel);
    assert!(
        previous == RQ_OWNER_NONE || previous == owner_cpu,
        "[sched] double enqueue at {}: pid={:?} already queued on cpu{} while enqueuing on cpu{} \
         state={:?} on_rq={} migrating={}",
        site,
        task.pid_root(),
        previous,
        owner_cpu,
        task.state(),
        task.sched.on_rq_state(),
        task.sched.is_migrating(),
    );
}

fn release_rq_ownership(task: &Arc<Task>) {
    task.rq_owner
        .store(RQ_OWNER_NONE, core::sync::atomic::Ordering::Release);
}

/// 运行队列中各调度类的可运行任务数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunqueueClassLoad {
    pub deadline: usize,
    pub deadline_utilization: u64,
    pub realtime: usize,
    pub fair: usize,
    pub fair_weight: u64,
}

impl RunqueueClassLoad {
    pub const fn total(self) -> usize {
        self.deadline + self.realtime + self.fair
    }

    pub const fn get(self, class: SchedClass) -> usize {
        match class {
            SchedClass::Deadline => self.deadline,
            SchedClass::Realtime => self.realtime,
            SchedClass::Fair => self.fair,
            SchedClass::Idle => 0,
        }
    }

    /// 返回用于 capacity 归一化的调度类负载。
    pub const fn balance_load(self, class: SchedClass) -> u64 {
        match class {
            SchedClass::Deadline => self.deadline_utilization,
            SchedClass::Realtime => self.realtime as u64 * SCHED_CAPACITY_SCALE,
            SchedClass::Fair => self.fair_weight,
            SchedClass::Idle => 0,
        }
    }

    pub const fn add(self, other: Self) -> Self {
        Self {
            deadline: self.deadline + other.deadline,
            deadline_utilization: self
                .deadline_utilization
                .saturating_add(other.deadline_utilization),
            realtime: self.realtime + other.realtime,
            fair: self.fair + other.fair,
            fair_weight: self.fair_weight.saturating_add(other.fair_weight),
        }
    }

    fn add_task(&mut self, task: &Arc<Task>) {
        match task.sched.class() {
            SchedClass::Deadline => {
                self.deadline += 1;
                self.deadline_utilization = self
                    .deadline_utilization
                    .saturating_add(deadline_utilization(task));
            }
            SchedClass::Realtime => self.realtime += 1,
            SchedClass::Fair => {
                self.fair += 1;
                self.fair_weight = self.fair_weight.saturating_add(task.sched.weight());
            }
            SchedClass::Idle => {}
        }
    }
}

impl Runqueue {
    pub(crate) const fn new() -> Self {
        Self {
            inner: Spinlock::new(RqInner {
                fair_tree: BTreeMap::new(),
                rt_tree: BTreeMap::new(),
                deadline_tree: BTreeMap::new(),
                deadline_throttled: BTreeMap::new(),
                idle_tree: BTreeMap::new(),
                queued_deadline_utilization: 0,
                total_weight: 0,
                weighted_vruntime_sum: 0,
                min_vruntime: 0,
                current: None,
                last_update_ns: 0,
                enqueue_seq: 0,
                preferred_fair_key: None,
                rt_period_ns: DEFAULT_RT_PERIOD_NS,
                rt_runtime_ns: DEFAULT_RT_RUNTIME_NS,
                rt_period_start_ns: 0,
                rt_runtime_used_ns: 0,
                rt_throttled: false,
                owner_cpu: RQ_OWNER_NONE,
            }),
            published_deadline: AtomicUsize::new(0),
            published_deadline_utilization: AtomicU64::new(0),
            published_realtime: AtomicUsize::new(0),
            published_fair: AtomicUsize::new(0),
            published_fair_weight: AtomicU64::new(0),
        }
    }

    /// 回填本 rq 所属 CPU 编号，启用双重入队检测。
    pub(crate) fn set_owner_cpu(&self, cpu_id: usize) {
        self.inner.lock().owner_cpu = cpu_id;
    }

    fn publish_load_locked(&self, inner: &RqInner) {
        // 这些字段只用于负载均衡的无锁提示；真正摘取任务时仍在目标 rq 锁内
        // 复核状态、亲和性和 MIGRATING。分字段发布允许读者看到相邻操作的混合
        // 快照，但不会影响正确性，只可能让一次 steal 尝试提前或延后一轮。
        self.published_deadline
            .store(inner.deadline_tree.len(), Ordering::Release);
        self.published_deadline_utilization
            .store(inner.queued_deadline_utilization, Ordering::Release);
        self.published_realtime
            .store(inner.rt_tree.len(), Ordering::Release);
        self.published_fair
            .store(inner.fair_tree.len(), Ordering::Release);
        self.published_fair_weight.store(
            inner.total_weight.min(u64::MAX as u128) as u64,
            Ordering::Release,
        );
    }

    /// 返回本 rq 的可迁移负载提示，不取得 rq 锁。
    ///
    /// 这是 balance_once 的快路径摘要。它故意不声称 affinity 精确，避免在每次
    /// 周期 balance 时锁住并扫描所有队列；候选摘取入口会再次执行严格过滤。
    pub(crate) fn migratable_class_load_hint(&self) -> RunqueueClassLoad {
        RunqueueClassLoad {
            deadline: self.published_deadline.load(Ordering::Acquire),
            deadline_utilization: self.published_deadline_utilization.load(Ordering::Acquire),
            realtime: self.published_realtime.load(Ordering::Acquire),
            fair: self.published_fair.load(Ordering::Acquire),
            fair_weight: self.published_fair_weight.load(Ordering::Acquire),
        }
    }

    /// 创建带有指定 RT bandwidth 的运行队列，仅供调度器初始化和测试使用。
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn new_with_rt_bandwidth(period_ns: u64, runtime_ns: u64) -> Self {
        let period_ns = period_ns.max(1);
        Self {
            inner: Spinlock::new(RqInner {
                fair_tree: BTreeMap::new(),
                rt_tree: BTreeMap::new(),
                deadline_tree: BTreeMap::new(),
                deadline_throttled: BTreeMap::new(),
                idle_tree: BTreeMap::new(),
                queued_deadline_utilization: 0,
                total_weight: 0,
                weighted_vruntime_sum: 0,
                min_vruntime: 0,
                current: None,
                last_update_ns: 0,
                enqueue_seq: 0,
                preferred_fair_key: None,
                rt_period_ns: period_ns,
                rt_runtime_ns: runtime_ns.min(period_ns),
                rt_period_start_ns: 0,
                rt_runtime_used_ns: 0,
                rt_throttled: false,
                owner_cpu: RQ_OWNER_NONE,
            }),
            published_deadline: AtomicUsize::new(0),
            published_deadline_utilization: AtomicU64::new(0),
            published_realtime: AtomicUsize::new(0),
            published_fair: AtomicUsize::new(0),
            published_fair_weight: AtomicU64::new(0),
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn avg_vruntime(&self) -> u64 {
        let inner = self.inner.lock();
        avg_vruntime_locked(&inner)
    }

    pub(crate) fn nr_running(&self) -> usize {
        let inner = self.inner.lock();
        inner.fair_tree.len()
            + inner.rt_tree.len()
            + inner.deadline_tree.len()
            + inner.deadline_throttled.len()
            + inner.idle_tree.len()
            + usize::from(inner.current.is_some())
    }

    /// 原子替换本 CPU 的 RT bandwidth 参数并重新开始记账周期。
    pub(crate) fn set_rt_bandwidth(&self, period_ns: u64, runtime_ns: u64, now_ns: u64) {
        let period_ns = period_ns.max(1);
        let runtime_ns = runtime_ns.min(period_ns);
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        inner.rt_period_ns = period_ns;
        inner.rt_runtime_ns = runtime_ns;
        inner.rt_period_start_ns = now_ns - now_ns % period_ns;
        inner.rt_runtime_used_ns = 0;
        inner.rt_throttled = runtime_ns == 0;
        self.publish_load_locked(&inner);
    }

    /// 可跨 CPU 迁移的就绪负载。
    ///
    /// idle 任务只属于本 CPU，deadline throttled 任务当前不可运行，current 任务
    /// 不能被远端直接摘走；负载均衡只看能通过 [`take_migratable`] 拉走的队列。
    #[cfg(test)]
    pub(crate) fn migratable_load(&self) -> usize {
        self.migratable_class_load().total()
    }

    /// 按调度类统计可跨 CPU 迁移的就绪负载。
    #[cfg(test)]
    pub(crate) fn migratable_class_load(&self) -> RunqueueClassLoad {
        let inner = self.inner.lock();
        class_load_locked(&inner, None, false)
    }

    /// 按调度类统计队列和 current 上的可运行负载。
    pub(crate) fn class_load(&self) -> RunqueueClassLoad {
        let inner = self.inner.lock();
        class_load_locked(&inner, None, true)
    }

    /// 是否存在已允许在目标 CPU 集合运行、但集合外的源 CPU 尚未完成上下文
    /// 保存的就绪任务。
    ///
    /// 同一 CPU 主动切换时，刚放回本地队列的前一任务也会短暂保留执行所有权，
    /// 但切换汇编会在恢复下一任务前释放它。这种本地过渡不能再次请求调度，否则
    /// 下一任务会在返回用户态前被立即切走。只有跨 CPU 迁移才需要保留重调度意图。
    pub(crate) fn has_ownership_blocked(&self, cpu_mask: u64) -> bool {
        let inner = self.inner.lock();
        let blocked = |task: &Arc<Task>| {
            task_can_enter_runqueue(task)
                && task.cpu_affinity() & cpu_mask != 0
                && task
                    .running_cpu()
                    .is_some_and(|owner| cpu_mask & (1u64 << owner) == 0)
        };
        inner.fair_tree.values().any(blocked)
            || inner.rt_tree.values().any(blocked)
            || inner.deadline_tree.values().any(blocked)
    }

    /// 对指定 CPU 许可位可迁移的就绪负载。
    ///
    /// 亲和性收窄后，任务可能短暂留在旧 CPU 的 rq 中；负载均衡只应选择
    /// 确实能被目标 CPU 拉走的源队列。
    #[cfg(test)]
    pub(crate) fn migratable_load_for(&self, allowed_cpu_mask: u64) -> usize {
        self.migratable_class_load_for(allowed_cpu_mask).total()
    }

    /// 按调度类统计允许迁移到指定 CPU 集的就绪负载。
    pub(crate) fn migratable_class_load_for(&self, allowed_cpu_mask: u64) -> RunqueueClassLoad {
        let inner = self.inner.lock();
        class_load_locked(&inner, Some(allowed_cpu_mask), false)
    }

    pub(crate) fn set_current(&self, task: Arc<Task>) {
        let mut inner = self.inner.lock();
        if let Some(old) = inner.current.take() {
            release_rq_ownership(&old);
            old.sched.set_on_rq(false);
        }
        prepare_running_locked(&mut inner, &task, 0);
        inner.current = Some(task);
    }

    pub(crate) fn enqueue(&self, task: Arc<Task>, now_ns: u64) -> bool {
        self.enqueue_with_preference(task, now_ns, false)
    }

    pub(crate) fn enqueue_preferred(&self, task: Arc<Task>, now_ns: u64) -> bool {
        self.enqueue_with_preference(task, now_ns, true)
    }

    fn enqueue_with_preference(&self, task: Arc<Task>, now_ns: u64, preferred: bool) -> bool {
        let mut inner = self.inner.lock();
        if inner
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &task))
            && matches!(
                task.state(),
                TaskState::Running | TaskState::Runnable | TaskState::Sleeping
            )
        {
            // 睡眠准备与并发唤醒之间允许出现“任务仍是 current，但状态已是
            // Sleeping”的窗口。此时唤醒必须原地撤销睡眠，不能把同一任务同时
            // 放进 current 和就绪队列，否则下一次 pick 会留下重复队列节点。
            //
            // 任务作为 current 仍然归属本 rq，因此 on_rq 必须保持为真。此前
            // 这里错误地清成 false：那会让紧随其后的一次远端唤醒通过
            // `enqueue` 的 on_rq 门禁，把同一个任务挂进另一个 CPU 的就绪队列，
            // 而它同时还是本 CPU 的 current——两个核随后各自切入同一份内核栈
            // 和 arch context。
            task.set_state(TaskState::Running);
            task.sched.set_on_rq(true);
            return false;
        }
        if !task_can_enter_runqueue(&task) {
            // 迁移中的任务不属于本 rq，其 on_rq 状态由迁移方负责收尾；这里
            // 只能拒绝入队，不能顺手清成 NONE 而破坏对方的事务。
            if !task.sched.is_migrating() {
                task.sched.set_on_rq(false);
            }
            log::warning!(
                "[sched] reject non-executable task enqueue pid={:?} state={:?}",
                task.pid_root(),
                task.state(),
            );
            return false;
        }
        // MIGRATING 也在此被拒绝：on_rq() 对迁移中的任务返回 true。调用方
        // （enqueue_task_on_scheduler）应先等待迁移结束再重新定位归属 CPU。
        if task.sched.on_rq() {
            return false;
        }
        let _ = update_curr_locked(&mut inner, now_ns);
        task.set_state(TaskState::Runnable);
        task.sched.set_on_rq(true);
        let preferred_fair = preferred && task.sched.policy() == SchedPolicy::Fair;
        let preferred_task = preferred_fair.then(|| Arc::clone(&task));
        enqueue_queued_locked_at(&mut inner, task, now_ns, "enqueue");
        if let Some(task) = preferred_task {
            // futex 唤醒是短等待热路径。只记录一次性候选，真正 pick 时仍会
            // 复查状态、亲和性和 class，避免破坏长期公平性。
            inner.preferred_fair_key = Some(FairKey {
                deadline: task.sched.rq_fair_deadline(),
                addr: task_addr(&task),
            });
        }
        self.publish_load_locked(&inner);
        true
    }

    pub(crate) fn dequeue(&self, task: &Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        let removed = dequeue_locked(&mut inner, task);
        self.publish_load_locked(&inner);
        removed
    }

    pub(crate) fn dequeue_queued(&self, task: &Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        let removed = remove_queued_any_locked(&mut inner, task).is_some();
        self.publish_load_locked(&inner);
        removed
    }

    /// 为迁移事务从本 rq 摘除任务，并在同一临界区内发布 `MIGRATING`。
    ///
    /// 与 [`dequeue_queued`] 的区别只在最终 `on_rq` 状态：这里绝不允许出现
    /// 短暂的 `NONE`，否则并发唤醒会在窗口内把任务重新入队到源 rq，最终让
    /// 同一任务同时挂在源和目标两个 rq 上。
    pub(crate) fn detach_queued_for_migration(&self, task: &Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        let Some(task) = remove_queued_any_locked(&mut inner, task) else {
            return false;
        };
        task.sched.set_migrating();
        self.publish_load_locked(&inner);
        true
    }

    /// 迁移事务的提交端：把任务放进本 rq 并结束 `MIGRATING` 状态。
    ///
    /// 普通 [`enqueue`] 会因为 `on_rq()` 在迁移期间为真而拒绝入队，因此提交
    /// 路径必须走这个入口。返回是否真正入队；无论成功与否，任务都会离开
    /// `MIGRATING` 状态。
    pub(crate) fn enqueue_migrated(&self, task: Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        // 迁移事务的结束必须与入队发生在同一个 rq 临界区内：若先清 MIGRATING
        // 再入队，中间窗口会让并发唤醒抢先把任务登记到同一个 rq，随后本次入队
        // 被拒、调用方误判失败并回滚到源 rq，重新制造双 rq 登记。
        let was_migrating = task.sched.is_migrating();
        if !task_can_enter_runqueue(&task) {
            if was_migrating {
                task.sched.finish_migrating(false);
            }
            log::warning!(
                "[sched] reject non-executable migrated task pid={:?} state={:?}",
                task.pid_root(),
                task.state(),
            );
            return false;
        }
        // 失败回滚路径可能在任务已经离开 MIGRATING 之后才走到这里；此时按普通
        // 入队处理，并保持“已在 rq 上就不重复登记”的幂等语义。
        if !was_migrating && task.sched.on_rq() {
            return false;
        }
        // 迁移期间任务可能被 SIGSTOP / exit 改成 Stopped 或 Sleeping。绝不能
        // 无条件写回 Runnable：那会把一个已经停止或正在退出的任务复活到目标
        // rq 上，让 SIGSTOP 永远等不到它进入 Stopped。
        let state = task.state();
        if !matches!(state, TaskState::Runnable | TaskState::Running) {
            if was_migrating {
                task.sched.finish_migrating(false);
            }
            return false;
        }
        let _ = update_curr_locked(&mut inner, now_ns);
        task.set_state(TaskState::Runnable);
        if was_migrating {
            task.sched.finish_migrating(true);
        } else {
            task.sched.set_on_rq(true);
        }
        enqueue_queued_locked_at(&mut inner, task, now_ns, "enqueue_migrated");
        self.publish_load_locked(&inner);
        true
    }

    pub(crate) fn is_current(&self, task: &Arc<Task>) -> bool {
        self.inner
            .lock()
            .current
            .as_ref()
            .is_some_and(|curr| Arc::ptr_eq(curr, task))
    }

    pub(crate) fn tick(&self, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let replenished = update_curr_locked(&mut inner, now_ns);
        self.publish_load_locked(&inner);
        let Some(curr) = inner.current.as_ref().map(Arc::clone) else {
            return replenished;
        };
        // 时间片到期本身不要求切换：当本 rq 没有其它可运行任务时，把 current
        // 重新插入再从同一棵树取回只会制造锁、引用计数和上下文切换开销。真正的
        // 竞争者到达时，入队路径会单独发布重调度请求。
        let has_runnable_peer = !inner.deadline_tree.is_empty()
            || !inner.rt_tree.is_empty()
            || !inner.fair_tree.is_empty()
            || !inner.idle_tree.is_empty();
        replenished
            || match curr.sched.policy() {
                SchedPolicy::Deadline => {
                    curr.sched.deadline_budget_ns() == 0
                        || now_ns > curr.sched.absolute_deadline_ns()
                }
                SchedPolicy::RtRoundRobin => {
                    (inner.rt_throttled && !curr.pi_is_boosted())
                        || ((!inner.rt_throttled || curr.pi_is_boosted())
                            && curr.sched.rr_remaining_ns() == 0
                            && has_rt_peer_locked(&inner, curr.sched.rt_priority()))
                }
                SchedPolicy::RtFifo => inner.rt_throttled && !curr.pi_is_boosted(),
                SchedPolicy::Fair | SchedPolicy::Idle => {
                    has_runnable_peer && curr.sched.vruntime() >= curr.sched.deadline()
                }
            }
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn pick_next(&self, now_ns: u64) -> Option<Arc<Task>> {
        self.pick_next_on(now_ns, u64::MAX)
    }

    /// 在指定 CPU 许可位下挑选下一个任务。
    ///
    /// 迁移或亲和性更新可能让某个任务暂时留在不再允许它运行的 rq 中；这里
    /// 只跳过这类任务，不在持有本 rq 锁时跨 CPU 迁移，避免形成跨 rq 锁顺序。
    pub(crate) fn pick_next_on(&self, now_ns: u64, cpu_mask: u64) -> Option<Arc<Task>> {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);

        let mut prev_addr = None;
        let mut fair_prev_key = None;
        let mut fair_prev_task = None;
        let mut kernel_idle = None;
        if let Some(prev) = inner.current.take() {
            prev_addr = Some(task_addr(&prev));
            if prev.is_idle_task() {
                // 内核 idle task 由每 CPU idle 槽提供，不属于可排队的
                // SCHED_IDLE 任务。把它每个 tick 插入再移出 BTreeMap 会在
                // rq 锁内反复分配节点，并让多核空闲系统争用全局分配器。
                release_rq_ownership(&prev);
                prev.sched.set_on_rq(false);
                kernel_idle = Some(prev);
            } else if task_can_enter_runqueue(&prev)
                && (prev.state() == TaskState::Running || prev.state() == TaskState::Runnable)
            {
                prev.set_state(TaskState::Runnable);
                if prev.sched.policy() == SchedPolicy::Fair {
                    fair_prev_task = Some(Arc::clone(&prev));
                }
                enqueue_queued_locked_at(&mut inner, prev, now_ns, "pick_next_requeue_prev");
                fair_prev_key = fair_prev_task.map(|task| FairKey {
                    deadline: task.sched.rq_fair_deadline(),
                    addr: task_addr(&task),
                });
            } else {
                if prev.arch_context().is_none()
                    && !matches!(prev.state(), TaskState::Zombie | TaskState::Dead)
                {
                    log::warning!(
                        "[sched] drop current without arch context pid={:?} state={:?}",
                        prev.pid_root(),
                        prev.state(),
                    );
                    prev.set_state(TaskState::Dead);
                }
                release_rq_ownership(&prev);
                prev.sched.set_on_rq(false);
            }
        }

        let preferred_fair_key = inner.preferred_fair_key.take();
        // 被选中的任务从索引树移入 current 槽，仍归属本 rq，因此不释放归属标记。
        let picked = pick_queued_locked(
            &mut inner,
            prev_addr,
            fair_prev_key,
            preferred_fair_key,
            cpu_mask,
        )
        .or(kernel_idle);

        if let Some(ref task) = picked {
            // current 槽同样是"归属本 rq"。这里复查一次：若该任务此刻还被别的
            // CPU 的 rq 记着，说明存在一条绕过入队门禁的双重登记路径——必须在
            // 切入它的内核栈之前就地暴露，而不是等到 user_clone_entry 里报出
            // "missing saved trap frame"。
            assert_rq_ownership_acquired(task, inner.owner_cpu, "pick_next_current");
        }

        if let Some(ref task) = picked {
            prepare_running_locked(&mut inner, task, now_ns);
            inner.current = Some(Arc::clone(task));
        }
        self.publish_load_locked(&inner);
        picked
    }

    /// 精确摘取一个已经在本队列中的普通公平类任务。
    ///
    /// 该入口只省略公平类候选遍历，不改变运行时间、lag 或 class precedence。
    /// 目标状态或所有权不再匹配、队列中存在更高调度类任务时返回 `None`，调用方
    /// 必须回到完整的 `pick_next_on`。
    pub(crate) fn pick_target_on(
        &self,
        target: &Arc<Task>,
        now_ns: u64,
        cpu_mask: u64,
    ) -> Option<Arc<Task>> {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);

        if target.sched.policy() != SchedPolicy::Fair
            || !task_can_run_on(target, cpu_mask)
            // 精确交接目标不可能是 current（下一项已显式排除），因此只要仍有
            // CPU 执行所有权，就尚未完成上下文保存。调用方的无锁预检与这里拿
            // rq 锁之间可能跨过远端 claim，必须在摘除队列节点前再次拒绝。
            || target.running_cpu().is_some()
            || inner
                .current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, target))
            || inner
                .deadline_tree
                .values()
                .any(|task| task_can_run_on(task, cpu_mask))
            || inner.rt_tree.values().any(|task| {
                task_can_run_on(task, cpu_mask) && (!inner.rt_throttled || task.pi_is_boosted())
            })
        {
            return None;
        }

        let key = FairKey {
            deadline: target.sched.rq_fair_deadline(),
            addr: task_addr(target),
        };
        if !inner
            .fair_tree
            .get(&key)
            .is_some_and(|queued| Arc::ptr_eq(queued, target))
        {
            return None;
        }

        requeue_current_locked(&mut inner, now_ns);
        let task = remove_fair_by_key_locked(&mut inner, key)?;
        if inner
            .preferred_fair_key
            .is_some_and(|key| key.addr == task_addr(&task))
        {
            inner.preferred_fair_key = None;
        }
        prepare_running_locked(&mut inner, &task, now_ns);
        inner.current = Some(Arc::clone(&task));
        self.publish_load_locked(&inner);
        Some(task)
    }

    #[cfg(test)]
    pub(crate) fn take_migratable(&self, allowed_cpu_mask: u64, now_ns: u64) -> Option<Arc<Task>> {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);

        if let Some(task) = take_fair_migratable_locked(&mut inner, allowed_cpu_mask) {
            self.publish_load_locked(&inner);
            return Some(task);
        }
        if let Some(task) = take_rt_migratable_locked(&mut inner, allowed_cpu_mask) {
            self.publish_load_locked(&inner);
            return Some(task);
        }
        let task = take_deadline_migratable_locked(&mut inner, allowed_cpu_mask);
        self.publish_load_locked(&inner);
        task
    }

    /// 从指定调度类中取出一个允许迁移到目标 CPU 集的任务。
    pub(crate) fn take_migratable_from_class(
        &self,
        class: SchedClass,
        allowed_cpu_mask: u64,
        now_ns: u64,
    ) -> Option<Arc<Task>> {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        let task = match class {
            SchedClass::Deadline => take_deadline_migratable_locked(&mut inner, allowed_cpu_mask),
            SchedClass::Realtime => take_rt_migratable_locked(&mut inner, allowed_cpu_mask),
            SchedClass::Fair => take_fair_migratable_locked(&mut inner, allowed_cpu_mask),
            SchedClass::Idle => None,
        };
        self.publish_load_locked(&inner);
        task
    }

    /// 排空 CPU 下线时需要迁移的队列任务。
    ///
    /// current 和 Idle 类任务由 CPU 生命周期代码单独处理；其余已排队任务
    /// 都从本 runqueue 摘除并清除 `on_rq`。
    /// 排空 CPU 下线时需要迁移的队列任务，并把它们标记为迁移中。
    ///
    /// 排空后的任务归 CPU 下线流程独占，直到它被重新入队到目标 CPU 或回滚到
    /// 源 CPU。因此这里和 load balance 一样发布 `MIGRATING`：下线过程中并发的
    /// 唤醒必须等待事务落地，否则会把任务登记到某个活动 CPU 上，而下线流程
    /// 随后又把同一任务放到它选定的目标 CPU，形成双 rq 登记。
    pub(crate) fn drain_queued(&self, now_ns: u64) -> Vec<Arc<Task>> {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        let mut drained = Vec::new();

        while let Some(task) = inner.fair_tree.values().next().cloned() {
            if let Some(task) = remove_fair_locked(&mut inner, &task) {
                drained.push(task);
            }
        }
        while let Some(task) = inner.rt_tree.values().next().cloned() {
            if let Some(task) = remove_rt_locked(&mut inner, &task) {
                drained.push(task);
            }
        }
        while let Some(task) = inner.deadline_tree.values().next().cloned() {
            if let Some(task) = remove_deadline_locked(&mut inner, &task) {
                drained.push(task);
            }
        }
        while let Some(task) = inner.deadline_throttled.values().next().cloned() {
            if let Some(task) = remove_deadline_throttled_locked(&mut inner, &task) {
                drained.push(task);
            }
        }
        for task in drained.iter() {
            task.sched.set_migrating();
        }
        self.publish_load_locked(&inner);
        drained
    }

    pub(crate) fn current(&self) -> Option<Arc<Task>> {
        self.inner.lock().current.as_ref().map(Arc::clone)
    }

    /// 在 rq 锁内更新 nice / slice，并按旧属性先完成出队记账。
    pub(crate) fn update_params(&self, task: &Arc<Task>, params: SchedParams, now_ns: u64) -> bool {
        self.update_sched_entity(task, now_ns, |task| task.set_sched_params(params))
    }

    /// 在 rq 锁内只更新 nice/weight，保持策略与时间片不变。
    pub(crate) fn update_nice(&self, task: &Arc<Task>, nice: i8, now_ns: u64) -> bool {
        self.update_sched_entity(task, now_ns, |task| task.set_nice(nice))
    }

    /// 在 rq 锁内更新完整调度属性，并按旧 class / 权重完成出队记账。
    pub(crate) fn update_sched_attr(&self, task: &Arc<Task>, attr: SchedAttr, now_ns: u64) -> bool {
        self.update_sched_entity(task, now_ns, |task| task.set_sched_attr(attr))
    }

    /// 只应用 PI 计算出的有效属性，不改写任务保存的用户基础属性。
    pub(crate) fn update_sched_attr_raw(
        &self,
        task: &Arc<Task>,
        attr: SchedAttr,
        now_ns: u64,
    ) -> bool {
        self.update_sched_entity(task, now_ns, |task| task.sched.set_sched_attr(attr))
    }

    fn update_sched_entity<F>(&self, task: &Arc<Task>, now_ns: u64, update: F) -> bool
    where
        F: FnOnce(&Arc<Task>),
    {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        let mut update = Some(update);

        if let Some(curr) = inner.current.as_ref() {
            if Arc::ptr_eq(curr, task) {
                let apply = update.take().expect("[sched] update closure consumed");
                apply(task);
                let now = inner.last_update_ns;
                prepare_running_locked(&mut inner, task, now);
                self.publish_load_locked(&inner);
                return true;
            }
        }

        if let Some(owned) = remove_queued_any_locked(&mut inner, task) {
            let apply = update.take().expect("[sched] update closure consumed");
            apply(&owned);
            owned.sched.set_on_rq(true);
            owned.set_state(TaskState::Runnable);
            let now = inner.last_update_ns;
            enqueue_queued_locked_at(&mut inner, owned, now, "update_sched_entity");
            self.publish_load_locked(&inner);
            return true;
        }

        // 任务声称仍在某个 rq 上，但不属于当前 rq。调用方应重新按 CPU 归属
        // 定位或扫描其它 rq，避免在迁移窗口里只更新实体而留下旧索引。
        if task.sched.on_rq() {
            return false;
        }

        let apply = update.take().expect("[sched] update closure consumed");
        apply(task);
        let now = inner.last_update_ns;
        prepare_sleeping_locked(&mut inner, task, now);
        self.publish_load_locked(&inner);
        true
    }

    pub(crate) fn snapshot_runnable(&self) -> Vec<Arc<Task>> {
        let inner = self.inner.lock();
        let mut out = Vec::new();
        out.extend(inner.deadline_tree.values().cloned());
        out.extend(inner.rt_tree.values().cloned());
        out.extend(inner.fair_tree.values().cloned());
        out.extend(inner.idle_tree.values().cloned());
        out
    }
}

impl Default for Runqueue {
    fn default() -> Self {
        Self::new()
    }
}

fn class_load_locked(
    inner: &RqInner,
    allowed_cpu_mask: Option<u64>,
    include_current: bool,
) -> RunqueueClassLoad {
    let allowed =
        |task: &Arc<Task>| allowed_cpu_mask.is_none_or(|mask| task_allowed_on(task, mask));
    let mut load = RunqueueClassLoad::default();
    for task in inner.deadline_tree.values().filter(|task| allowed(task)) {
        load.add_task(task);
    }
    for task in inner.rt_tree.values().filter(|task| allowed(task)) {
        load.add_task(task);
    }
    for task in inner.fair_tree.values().filter(|task| allowed(task)) {
        load.add_task(task);
    }
    if include_current
        && let Some(current) = inner.current.as_ref()
        && matches!(current.state(), TaskState::Running | TaskState::Runnable)
        && allowed(current)
    {
        load.add_task(current);
    }
    load
}

fn deadline_utilization(task: &Arc<Task>) -> u64 {
    let runtime = task.sched.deadline_runtime_ns();
    let period = task.sched.deadline_period_ns();
    if runtime == 0 || period == 0 {
        return 0;
    }
    ((runtime as u128 * SCHED_CAPACITY_SCALE as u128) / period as u128).min(u64::MAX as u128) as u64
}

fn requeue_current_locked(inner: &mut RqInner, now_ns: u64) {
    if let Some(prev) = inner.current.take() {
        if prev.is_idle_task() {
            // kernel idle 由每 CPU idle 槽持有，targeted pick 时同样不能把它
            // 塞回普通 SCHED_IDLE 队列。
            release_rq_ownership(&prev);
            prev.sched.set_on_rq(false);
        } else if task_can_enter_runqueue(&prev)
            && matches!(prev.state(), TaskState::Running | TaskState::Runnable)
        {
            prev.set_state(TaskState::Runnable);
            enqueue_queued_locked_at(inner, prev, now_ns, "requeue_current");
        } else {
            if prev.arch_context().is_none()
                && !matches!(prev.state(), TaskState::Zombie | TaskState::Dead)
            {
                log::warning!(
                    "[sched] drop current without arch context pid={:?} state={:?}",
                    prev.pid_root(),
                    prev.state(),
                );
                prev.set_state(TaskState::Dead);
            }
            release_rq_ownership(&prev);
            prev.sched.set_on_rq(false);
        }
    }
}

/// `site` 只用于双重入队断言的诊断信息，标识调用来源。
fn enqueue_queued_locked_at(inner: &mut RqInner, task: Arc<Task>, now_ns: u64, site: &str) {
    assert_rq_ownership_acquired(&task, inner.owner_cpu, site);
    match task.sched.policy() {
        SchedPolicy::Deadline => {
            let replenish_at = task.sched.deadline_replenish_ns();
            if task.sched.absolute_deadline_ns() == 0
                || (replenish_at != 0 && now_ns >= replenish_at)
            {
                task.sched.replenish_deadline(now_ns);
            }
            if task.sched.deadline_budget_ns() == 0 {
                let replenish_at = task.sched.deadline_replenish_ns();
                if replenish_at == 0 || now_ns >= replenish_at {
                    task.sched.replenish_deadline(now_ns);
                } else {
                    let seq = next_seq(inner);
                    inner
                        .deadline_throttled
                        .insert(DeadlineThrottleKey::of(&task, seq), task);
                    return;
                }
            }
            let seq = next_seq(inner);
            insert_deadline_locked(inner, DeadlineKey::of(&task, seq), task);
        }
        SchedPolicy::RtFifo | SchedPolicy::RtRoundRobin => {
            if task.sched.policy() == SchedPolicy::RtRoundRobin && task.sched.rr_remaining_ns() == 0
            {
                task.sched.reset_rr_slice();
            }
            let seq = next_seq(inner);
            inner.rt_tree.insert(RtKey::of(&task, seq), task);
        }
        SchedPolicy::Fair => enqueue_fair_locked(inner, task),
        SchedPolicy::Idle => {
            let seq = next_seq(inner);
            inner.idle_tree.insert(RtKey::idle(&task, seq), task);
        }
    }
}

fn enqueue_fair_locked(inner: &mut RqInner, task: Arc<Task>) {
    let weight = task.sched.weight() as u128;
    let avg = avg_vruntime_locked(inner);
    let new_vr = if task.sched.lag() != 0 {
        let lag = task.sched.lag();
        if lag >= 0 {
            avg.saturating_sub(lag as u64)
        } else {
            avg.saturating_add((-lag) as u64)
        }
    } else {
        task.sched.vruntime().max(inner.min_vruntime)
    };

    task.sched.store_vruntime(new_vr);
    task.sched.store_lag(0);
    task.sched.store_rq_account(new_vr, weight as u64);
    let new_dl = task.sched.recalc_deadline();
    task.sched.store_deadline(new_dl);

    inner.total_weight += weight;
    inner.weighted_vruntime_sum += new_vr as u128 * weight;
    let key = FairKey::of(&task);
    task.sched.store_rq_fair_deadline(key.deadline);
    let replaced_task = inner.fair_tree.insert(key, task);
    debug_assert!(replaced_task.is_none(), "[sched] duplicate fair tree key");
}

fn pick_queued_locked(
    inner: &mut RqInner,
    prev_addr: Option<usize>,
    mut fair_prev_key: Option<FairKey>,
    mut preferred_fair_key: Option<FairKey>,
    cpu_mask: u64,
) -> Option<Arc<Task>> {
    loop {
        let task = pick_queued_candidate_locked(
            inner,
            prev_addr,
            fair_prev_key,
            preferred_fair_key,
            cpu_mask,
        )?;
        if task_can_run_on(&task, cpu_mask) {
            return Some(task);
        }
        discard_non_runnable_pick(&task);
        if fair_prev_key.is_some_and(|key| key.addr == task_addr(&task)) {
            fair_prev_key = None;
        }
        if preferred_fair_key.is_some_and(|key| key.addr == task_addr(&task)) {
            preferred_fair_key = None;
        }
    }
}

fn pick_queued_candidate_locked(
    inner: &mut RqInner,
    prev_addr: Option<usize>,
    fair_prev_key: Option<FairKey>,
    preferred_fair_key: Option<FairKey>,
    cpu_mask: u64,
) -> Option<Arc<Task>> {
    if let Some(key) = inner
        .deadline_tree
        .iter()
        .find(|(_, task)| task_pickable_on(task, cpu_mask, prev_addr))
        .map(|(key, _)| *key)
    {
        return remove_deadline_by_key_locked(inner, key);
    }
    if let Some(key) = inner
        .rt_tree
        .iter()
        .find(|(_, task)| {
            task_pickable_on(task, cpu_mask, prev_addr)
                && (!inner.rt_throttled || task.pi_is_boosted())
        })
        .map(|(key, _)| *key)
    {
        return inner.rt_tree.remove(&key);
    }
    if let Some(task) = pick_fair_locked(
        inner,
        prev_addr,
        fair_prev_key,
        preferred_fair_key,
        cpu_mask,
    ) {
        return Some(task);
    }
    let key = inner
        .idle_tree
        .iter()
        .find(|(_, task)| task_pickable_on(task, cpu_mask, prev_addr))
        .map(|(key, _)| *key)?;
    inner.idle_tree.remove(&key)
}

fn pick_fair_locked(
    inner: &mut RqInner,
    prev_addr: Option<usize>,
    skip_key: Option<FairKey>,
    preferred_key: Option<FairKey>,
    cpu_mask: u64,
) -> Option<Arc<Task>> {
    let avg = avg_vruntime_locked(inner);

    // EEVDF 按 fair deadline 排序。唤醒密集的普通任务通常让队首任务已经
    // eligible；先检查这个候选可以把常见的 nice=0 调度从整棵 BTreeMap
    // 扫描降为一次首元素查询。亲和性、buddy-skip 或 preferred 候选存在时
    // 仍走完整筛选，因而不会改变受限 CPU 集合和公平性语义。
    if skip_key.is_none()
        && preferred_key.is_none()
        && let Some((&key, task)) = inner.fair_tree.iter().next()
        && task_pickable_on(task, cpu_mask, prev_addr)
        && task.sched.vruntime() <= avg
    {
        return remove_fair_by_key_locked(inner, key);
    }

    let mut preferred = None;
    let mut first_allowed = None;
    let mut first_eligible = None;
    let mut min_non_skip: Option<(FairKey, u64)> = None;

    for (key, task) in inner.fair_tree.iter() {
        if !task_pickable_on(task, cpu_mask, prev_addr) {
            continue;
        }
        if preferred_key == Some(*key) {
            preferred = Some(*key);
        }
        first_allowed.get_or_insert(*key);
        if skip_key == Some(*key) {
            continue;
        }
        let vruntime = task.sched.vruntime();
        if vruntime <= avg {
            first_eligible.get_or_insert(*key);
        }
        if min_non_skip
            .as_ref()
            .is_none_or(|(_, current_min)| vruntime < *current_min)
        {
            min_non_skip = Some((*key, vruntime));
        }
    }

    let key = if skip_key.is_some() {
        first_eligible
            .or_else(|| min_non_skip.map(|(key, _)| key))
            .or(first_allowed)
    } else {
        first_eligible.or(first_allowed)
    };
    let key = preferred.or(key)?;
    remove_fair_by_key_locked(inner, key)
}

fn remove_fair_by_key_locked(inner: &mut RqInner, key: FairKey) -> Option<Arc<Task>> {
    let task = inner.fair_tree.remove(&key)?;
    account_fair_remove_locked(inner, &task);
    Some(task)
}

fn task_allowed_on(task: &Arc<Task>, cpu_mask: u64) -> bool {
    (task.cpu_affinity() & cpu_mask) != 0
        && task
            .running_cpu()
            .is_none_or(|cpu| cpu < u64::BITS as usize && (cpu_mask & (1u64 << cpu)) != 0)
}

fn task_pickable_on(task: &Arc<Task>, cpu_mask: u64, prev_addr: Option<usize>) -> bool {
    task_allowed_on(task, cpu_mask)
        && task
            .running_cpu()
            .is_none_or(|_| prev_addr == Some(task_addr(task)))
}

fn task_can_enter_runqueue(task: &Arc<Task>) -> bool {
    task.arch_context().is_some() && !matches!(task.state(), TaskState::Zombie | TaskState::Dead)
}

fn task_can_run_on(task: &Arc<Task>, cpu_mask: u64) -> bool {
    task_allowed_on(task, cpu_mask)
        && task.state() == TaskState::Runnable
        && task.arch_context().is_some()
}

fn discard_non_runnable_pick(task: &Arc<Task>) {
    let state = task.state();
    // 已从索引摘出且不会放回 current，归属结束。
    release_rq_ownership(task);
    task.sched.set_on_rq(false);
    if task.arch_context().is_none() && !matches!(state, TaskState::Zombie | TaskState::Dead) {
        // 已释放执行体的任务绝不能重新进入调度。这里把异常残留终结掉，
        // 避免调度器在同一个坏任务上反复自旋或切到无效上下文。
        task.set_state(TaskState::Dead);
    }
    log::warning!(
        "[sched] discard non-runnable queued task pid={:?} state={:?} has_ctx={}",
        task.pid_root(),
        state,
        task.arch_context().is_some(),
    );
}

fn prepare_running_locked(inner: &mut RqInner, task: &Arc<Task>, now_ns: u64) {
    task.set_state(TaskState::Running);
    task.sched.set_on_rq(true);
    match task.sched.policy() {
        SchedPolicy::Fair | SchedPolicy::Idle => {
            let new_vr = task.sched.vruntime().max(inner.min_vruntime);
            task.sched.store_vruntime(new_vr);
            task.sched.store_lag(0);
            let new_dl = task.sched.recalc_deadline();
            task.sched.store_deadline(new_dl);
        }
        SchedPolicy::RtRoundRobin => {
            if task.sched.rr_remaining_ns() == 0 {
                task.sched.reset_rr_slice();
            }
        }
        SchedPolicy::RtFifo => {}
        SchedPolicy::Deadline => {
            let replenish_at = task.sched.deadline_replenish_ns();
            if task.sched.absolute_deadline_ns() == 0
                || (replenish_at != 0 && now_ns >= replenish_at)
                || (task.sched.deadline_budget_ns() == 0
                    && (replenish_at == 0 || now_ns >= replenish_at))
            {
                task.sched.replenish_deadline(now_ns);
            }
        }
    }
}

fn prepare_sleeping_locked(inner: &mut RqInner, task: &Arc<Task>, _now_ns: u64) {
    if task.sched.policy() == SchedPolicy::Fair {
        let new_dl = task.sched.recalc_deadline();
        task.sched.store_deadline(new_dl);
    } else if task.sched.policy() == SchedPolicy::RtRoundRobin && task.sched.rr_remaining_ns() == 0
    {
        task.sched.reset_rr_slice();
    } else if task.sched.policy() == SchedPolicy::Deadline
        && (task.sched.absolute_deadline_ns() == 0
            || (task.sched.deadline_replenish_ns() != 0
                && inner.last_update_ns >= task.sched.deadline_replenish_ns()))
    {
        task.sched.replenish_deadline(inner.last_update_ns);
    }
}

fn dequeue_locked(inner: &mut RqInner, task: &Arc<Task>) -> bool {
    if let Some(curr) = inner.current.as_ref() {
        if Arc::ptr_eq(curr, task) {
            if task.sched.policy() == SchedPolicy::Fair {
                store_fair_lag_locked(inner, task);
            }
            release_rq_ownership(task);
            task.sched.set_on_rq(false);
            inner.current = None;
            return true;
        }
    }

    remove_queued_any_locked(inner, task).is_some()
}

fn remove_queued_any_locked(inner: &mut RqInner, task: &Arc<Task>) -> Option<Arc<Task>> {
    if let Some(task) = remove_fair_locked(inner, task) {
        return Some(task);
    }
    if let Some(task) = remove_rt_locked(inner, task) {
        return Some(task);
    }
    if let Some(task) = remove_deadline_locked(inner, task) {
        return Some(task);
    }
    if let Some(task) = remove_deadline_throttled_locked(inner, task) {
        return Some(task);
    }
    remove_idle_locked(inner, task)
}

fn remove_fair_locked(inner: &mut RqInner, task: &Arc<Task>) -> Option<Arc<Task>> {
    let key = FairKey {
        deadline: task.sched.rq_fair_deadline(),
        addr: task_addr(task),
    };
    if !inner
        .fair_tree
        .get(&key)
        .is_some_and(|value| Arc::ptr_eq(value, task))
    {
        return None;
    }
    let task = remove_fair_by_key_locked(inner, key)?;
    store_fair_lag_locked(inner, &task);
    release_rq_ownership(&task);
    task.sched.set_on_rq(false);
    Some(task)
}

fn remove_rt_locked(inner: &mut RqInner, task: &Arc<Task>) -> Option<Arc<Task>> {
    let key = inner
        .rt_tree
        .iter()
        .find(|(_, value)| Arc::ptr_eq(value, task))
        .map(|(key, _)| *key)?;
    let task = inner.rt_tree.remove(&key)?;
    release_rq_ownership(&task);
    task.sched.set_on_rq(false);
    Some(task)
}

fn insert_deadline_locked(inner: &mut RqInner, key: DeadlineKey, task: Arc<Task>) {
    inner.queued_deadline_utilization = inner
        .queued_deadline_utilization
        .saturating_add(deadline_utilization(&task));
    let replaced = inner.deadline_tree.insert(key, task);
    debug_assert!(replaced.is_none(), "[sched] duplicate deadline tree key");
}

fn remove_deadline_by_key_locked(inner: &mut RqInner, key: DeadlineKey) -> Option<Arc<Task>> {
    let task = inner.deadline_tree.remove(&key)?;
    inner.queued_deadline_utilization = inner
        .queued_deadline_utilization
        .saturating_sub(deadline_utilization(&task));
    Some(task)
}

fn remove_deadline_locked(inner: &mut RqInner, task: &Arc<Task>) -> Option<Arc<Task>> {
    let key = inner
        .deadline_tree
        .iter()
        .find(|(_, value)| Arc::ptr_eq(value, task))
        .map(|(key, _)| *key)?;
    let task = remove_deadline_by_key_locked(inner, key)?;
    release_rq_ownership(&task);
    task.sched.set_on_rq(false);
    Some(task)
}

fn remove_deadline_throttled_locked(inner: &mut RqInner, task: &Arc<Task>) -> Option<Arc<Task>> {
    let key = inner
        .deadline_throttled
        .iter()
        .find(|(_, value)| Arc::ptr_eq(value, task))
        .map(|(key, _)| *key)?;
    let task = inner.deadline_throttled.remove(&key)?;
    release_rq_ownership(&task);
    task.sched.set_on_rq(false);
    Some(task)
}

fn remove_idle_locked(inner: &mut RqInner, task: &Arc<Task>) -> Option<Arc<Task>> {
    let key = inner
        .idle_tree
        .iter()
        .find(|(_, value)| Arc::ptr_eq(value, task))
        .map(|(key, _)| *key)?;
    let task = inner.idle_tree.remove(&key)?;
    release_rq_ownership(&task);
    task.sched.set_on_rq(false);
    Some(task)
}

/// 迁移候选必须真正可运行。
///
/// 仅按亲和性筛选是不够的：正在被 SIGSTOP/exit 摘出的任务可能短暂仍在树里，
/// 一旦被偷走，`enqueue_migrated` 会把它强行改回 Runnable，等于把一个已经
/// Stopped 或 Zombie 的任务复活到另一个 CPU 上。
fn migratable_candidate(task: &Arc<Task>, allowed_cpu_mask: u64) -> bool {
    (task.cpu_affinity() & allowed_cpu_mask) != 0
        && task.state() == TaskState::Runnable
        && task.arch_context().is_some()
}

fn take_fair_migratable_locked(inner: &mut RqInner, allowed_cpu_mask: u64) -> Option<Arc<Task>> {
    let key = inner
        .fair_tree
        .iter()
        .rev()
        .find(|(_, task)| {
            task.running_cpu().is_none() && migratable_candidate(task, allowed_cpu_mask)
        })
        .map(|(key, _)| *key)?;
    let task = remove_fair_by_key_locked(inner, key)?;
    store_fair_lag_locked(inner, &task);
    // 迁移事务开始：任务已不在本 rq 索引中，但归属权归迁移方独占。绝不能置
    // NONE，否则并发唤醒会把它重新塞回源 rq，造成双 rq 登记。
    release_rq_ownership(&task);
    task.sched.set_migrating();
    Some(task)
}

fn take_rt_migratable_locked(inner: &mut RqInner, allowed_cpu_mask: u64) -> Option<Arc<Task>> {
    let key = inner
        .rt_tree
        .iter()
        .rev()
        .find(|(_, task)| {
            task.running_cpu().is_none() && migratable_candidate(task, allowed_cpu_mask)
        })
        .map(|(key, _)| *key)?;
    let task = inner.rt_tree.remove(&key)?;
    release_rq_ownership(&task);
    task.sched.set_migrating();
    Some(task)
}

fn take_deadline_migratable_locked(
    inner: &mut RqInner,
    allowed_cpu_mask: u64,
) -> Option<Arc<Task>> {
    let key = inner
        .deadline_tree
        .iter()
        .rev()
        .find(|(_, task)| {
            task.running_cpu().is_none() && migratable_candidate(task, allowed_cpu_mask)
        })
        .map(|(key, _)| *key)?;
    let task = remove_deadline_by_key_locked(inner, key)?;
    release_rq_ownership(&task);
    task.sched.set_migrating();
    Some(task)
}

fn update_curr_locked(inner: &mut RqInner, now_ns: u64) -> bool {
    let mut rt_replenished = refresh_rt_bandwidth_locked(inner, now_ns);
    if now_ns > inner.last_update_ns {
        let previous_ns = inner.last_update_ns;
        let delta = now_ns - inner.last_update_ns;
        inner.last_update_ns = now_ns;
        if let Some(curr) = inner.current.as_ref().map(Arc::clone) {
            curr.account_cpu_runtime(delta, now_ns);
            match curr.sched.policy() {
                SchedPolicy::Fair | SchedPolicy::Idle => {
                    update_fair_curr_locked(inner, &curr, delta)
                }
                SchedPolicy::RtRoundRobin => {
                    let _ = curr.sched.charge_rr_runtime(delta);
                    charge_rt_bandwidth_locked(inner, previous_ns, now_ns);
                }
                SchedPolicy::Deadline => {
                    let _ = curr.sched.charge_deadline_runtime(delta);
                }
                SchedPolicy::RtFifo => charge_rt_bandwidth_locked(inner, previous_ns, now_ns),
            }
        }
    }
    rt_replenished |= requeue_ready_deadline_locked(inner, now_ns);
    rt_replenished
}

fn refresh_rt_bandwidth_locked(inner: &mut RqInner, now_ns: u64) -> bool {
    let period_start = now_ns - (now_ns % inner.rt_period_ns);
    if period_start == inner.rt_period_start_ns {
        return false;
    }
    let was_throttled = inner.rt_throttled;
    inner.rt_period_start_ns = period_start;
    inner.rt_runtime_used_ns = 0;
    inner.rt_throttled = inner.rt_runtime_ns == 0;
    was_throttled && !inner.rt_throttled
}

fn charge_rt_bandwidth_locked(inner: &mut RqInner, previous_ns: u64, now_ns: u64) {
    if inner.rt_throttled || inner.rt_runtime_ns == inner.rt_period_ns {
        return;
    }
    let charge_from = previous_ns.max(inner.rt_period_start_ns);
    let delta = now_ns.saturating_sub(charge_from);
    inner.rt_runtime_used_ns = inner.rt_runtime_used_ns.saturating_add(delta);
    if inner.rt_runtime_used_ns >= inner.rt_runtime_ns {
        inner.rt_throttled = true;
    }
}

fn requeue_ready_deadline_locked(inner: &mut RqInner, now_ns: u64) -> bool {
    let mut moved = false;
    loop {
        let Some(key) = inner.deadline_throttled.keys().next().copied() else {
            break;
        };
        if key.replenish_ns > now_ns {
            break;
        }
        let Some(task) = inner.deadline_throttled.remove(&key) else {
            break;
        };
        if task.state() != TaskState::Runnable {
            release_rq_ownership(&task);
            task.sched.set_on_rq(false);
            continue;
        }
        task.sched.replenish_deadline(now_ns);
        let seq = next_seq(inner);
        insert_deadline_locked(inner, DeadlineKey::of(&task, seq), task);
        moved = true;
    }
    moved
}

fn update_fair_curr_locked(inner: &mut RqInner, curr: &Arc<Task>, delta_ns: u64) {
    let weight = curr.sched.weight();
    if weight == 0 {
        return;
    }
    let delta_vr = scale_delta_by_weight(delta_ns, weight);
    if delta_vr == 0 {
        return;
    }
    let old_vr = curr.sched.vruntime();
    let new_vr = old_vr.saturating_add(delta_vr);
    curr.sched.store_vruntime(new_vr);
    if new_vr > inner.min_vruntime {
        let tree_min = inner
            .fair_tree
            .values()
            .map(|task| task.sched.vruntime())
            .min()
            .unwrap_or(new_vr);
        let candidate = tree_min.min(new_vr);
        if candidate > inner.min_vruntime {
            inner.min_vruntime = candidate;
        }
    }
}

fn avg_vruntime_locked(inner: &RqInner) -> u64 {
    let (w_sum, vw_sum) = match inner.current.as_ref() {
        Some(curr) if curr.sched.policy() == SchedPolicy::Fair => {
            let w = curr.sched.weight() as u128;
            let vr = curr.sched.vruntime() as u128;
            (inner.total_weight + w, inner.weighted_vruntime_sum + vr * w)
        }
        _ => (inner.total_weight, inner.weighted_vruntime_sum),
    };
    if w_sum == 0 {
        inner.min_vruntime
    } else if let (Ok(vw_sum), Ok(w_sum)) = (u64::try_from(vw_sum), u64::try_from(w_sum)) {
        vw_sum / w_sum
    } else {
        (vw_sum / w_sum) as u64
    }
}

fn account_fair_remove_locked(inner: &mut RqInner, task: &Arc<Task>) {
    let (vr, w) = task.sched.rq_account();
    let w = w as u128;
    let vr = vr as u128;
    inner.total_weight = inner.total_weight.saturating_sub(w);
    inner.weighted_vruntime_sum = inner.weighted_vruntime_sum.saturating_sub(vr * w);
    task.sched.clear_rq_account();
}

fn store_fair_lag_locked(inner: &RqInner, task: &Arc<Task>) {
    let avg = avg_vruntime_locked(inner);
    let vr = task.sched.vruntime();
    let lag = avg as i128 - vr as i128;
    let lag = lag.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    task.sched.store_lag(lag);
}

fn has_rt_peer_locked(inner: &RqInner, priority: u8) -> bool {
    inner
        .rt_tree
        .values()
        .any(|task| task.sched.rt_priority() >= priority)
}

fn next_seq(inner: &mut RqInner) -> u64 {
    let seq = inner.enqueue_seq;
    inner.enqueue_seq = inner.enqueue_seq.wrapping_add(1);
    seq
}

fn task_addr(task: &Arc<Task>) -> usize {
    Arc::as_ptr(task) as *const () as usize
}
