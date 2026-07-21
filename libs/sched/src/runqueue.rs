//! 多调度类运行队列。
//!
//! runqueue 对外仍是单 CPU 队列；内部按 `Deadline > Realtime > Fair > Idle`
//! 分层。Fair class 使用 EEVDF；RT class 提供 FIFO/RR 队列骨架；Deadline
//! class 提供 EDF + runtime budget 框架。AP 启动和真实 IPI 不在本模块内。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::cpu::SCHED_CAPACITY_SCALE;
use crate::eevdf::{NICE_0_WEIGHT, SchedParams};
use crate::sched_class::{RT_PRIO_MAX, SchedAttr, SchedClass, SchedPolicy};
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
    total_weight: u128,
    weighted_vruntime_sum: u128,
    min_vruntime: u64,
    current: Option<Arc<Task>>,
    last_update_ns: u64,
    enqueue_seq: u64,
    preferred_fair_addr: Option<usize>,
}

/// 单 CPU 运行队列。每个 online CPU 持有一份。
pub(crate) struct Runqueue {
    inner: Spinlock<RqInner>,
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
                total_weight: 0,
                weighted_vruntime_sum: 0,
                min_vruntime: 0,
                current: None,
                last_update_ns: 0,
                enqueue_seq: 0,
                preferred_fair_addr: None,
            }),
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
            task.set_state(TaskState::Running);
            task.sched.set_on_rq(false);
            return false;
        }
        if !task_can_enter_runqueue(&task) {
            task.sched.set_on_rq(false);
            log::warning!(
                "[sched] reject non-executable task enqueue pid={:?} state={:?}",
                task.pid_root(),
                task.state(),
            );
            return false;
        }
        if task.sched.on_rq() {
            return false;
        }
        let _ = update_curr_locked(&mut inner, now_ns);
        task.set_state(TaskState::Runnable);
        task.sched.set_on_rq(true);
        if preferred && task.sched.policy() == SchedPolicy::Fair {
            // futex 唤醒是短等待热路径。只记录一次性候选，
            // 真正 pick 时仍会复查状态、亲和性和 class，避免破坏长期公平性。
            inner.preferred_fair_addr = Some(task_addr(&task));
        }
        enqueue_queued_locked(&mut inner, task, now_ns);
        true
    }

    pub(crate) fn dequeue(&self, task: &Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        dequeue_locked(&mut inner, task)
    }

    pub(crate) fn dequeue_queued(&self, task: &Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        remove_queued_any_locked(&mut inner, task).is_some()
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
        let Some(curr) = inner.current.as_ref() else {
            return replenished;
        };
        replenished
            || match curr.sched.policy() {
                SchedPolicy::Deadline => {
                    curr.sched.deadline_budget_ns() == 0
                        || now_ns > curr.sched.absolute_deadline_ns()
                }
                SchedPolicy::RtRoundRobin => {
                    curr.sched.rr_remaining_ns() == 0
                        && has_rt_peer_locked(&inner, curr.sched.rt_priority())
                }
                SchedPolicy::RtFifo => false,
                SchedPolicy::Fair | SchedPolicy::Idle => {
                    curr.sched.vruntime() >= curr.sched.deadline()
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

        let mut fair_prev_addr = None;
        if let Some(prev) = inner.current.take() {
            if task_can_enter_runqueue(&prev)
                && (prev.state() == TaskState::Running || prev.state() == TaskState::Runnable)
            {
                prev.set_state(TaskState::Runnable);
                if prev.sched.policy() == SchedPolicy::Fair {
                    fair_prev_addr = Some(task_addr(&prev));
                }
                enqueue_queued_locked(&mut inner, prev, now_ns);
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
                prev.sched.set_on_rq(false);
            }
        }

        let preferred_fair_addr = inner.preferred_fair_addr.take();
        let picked = pick_queued_locked(&mut inner, fair_prev_addr, preferred_fair_addr, cpu_mask);
        if let Some(ref task) = picked {
            prepare_running_locked(&mut inner, task, now_ns);
            inner.current = Some(Arc::clone(task));
        }
        picked
    }

    #[cfg(test)]
    pub(crate) fn take_migratable(&self, allowed_cpu_mask: u64, now_ns: u64) -> Option<Arc<Task>> {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);

        if let Some(task) = take_fair_migratable_locked(&mut inner, allowed_cpu_mask) {
            return Some(task);
        }
        if let Some(task) = take_rt_migratable_locked(&mut inner, allowed_cpu_mask) {
            return Some(task);
        }
        take_deadline_migratable_locked(&mut inner, allowed_cpu_mask)
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
        match class {
            SchedClass::Deadline => take_deadline_migratable_locked(&mut inner, allowed_cpu_mask),
            SchedClass::Realtime => take_rt_migratable_locked(&mut inner, allowed_cpu_mask),
            SchedClass::Fair => take_fair_migratable_locked(&mut inner, allowed_cpu_mask),
            SchedClass::Idle => None,
        }
    }

    /// 排空 CPU 下线时需要迁移的队列任务。
    ///
    /// current 和 Idle 类任务由 CPU 生命周期代码单独处理；其余已排队任务
    /// 都从本 runqueue 摘除并清除 `on_rq`。
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
        drained
    }

    pub(crate) fn current(&self) -> Option<Arc<Task>> {
        self.inner.lock().current.as_ref().map(Arc::clone)
    }

    /// 在 rq 锁内更新 nice / slice，并按旧属性先完成出队记账。
    pub(crate) fn update_params(&self, task: &Arc<Task>, params: SchedParams, now_ns: u64) -> bool {
        self.update_sched_entity(task, now_ns, |task| task.sched.set_params(params))
    }

    /// 在 rq 锁内只更新 nice/weight，保持策略与时间片不变。
    pub(crate) fn update_nice(&self, task: &Arc<Task>, nice: i8, now_ns: u64) -> bool {
        self.update_sched_entity(task, now_ns, |task| task.sched.set_nice(nice))
    }

    /// 在 rq 锁内更新完整调度属性，并按旧 class / 权重完成出队记账。
    pub(crate) fn update_sched_attr(&self, task: &Arc<Task>, attr: SchedAttr, now_ns: u64) -> bool {
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
                return true;
            }
        }

        if let Some(owned) = remove_queued_any_locked(&mut inner, task) {
            let apply = update.take().expect("[sched] update closure consumed");
            apply(&owned);
            owned.sched.set_on_rq(true);
            owned.set_state(TaskState::Runnable);
            let now = inner.last_update_ns;
            enqueue_queued_locked(&mut inner, owned, now);
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

fn enqueue_queued_locked(inner: &mut RqInner, task: Arc<Task>, now_ns: u64) {
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
            inner
                .deadline_tree
                .insert(DeadlineKey::of(&task, seq), task);
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
    inner.fair_tree.insert(FairKey::of(&task), task);
}

fn pick_queued_locked(
    inner: &mut RqInner,
    mut fair_prev_addr: Option<usize>,
    mut preferred_fair_addr: Option<usize>,
    cpu_mask: u64,
) -> Option<Arc<Task>> {
    loop {
        let task =
            pick_queued_candidate_locked(inner, fair_prev_addr, preferred_fair_addr, cpu_mask)?;
        if task_can_run_on(&task, cpu_mask) {
            return Some(task);
        }
        discard_non_runnable_pick(&task);
        if fair_prev_addr.is_some_and(|addr| addr == task_addr(&task)) {
            fair_prev_addr = None;
        }
        if preferred_fair_addr.is_some_and(|addr| addr == task_addr(&task)) {
            preferred_fair_addr = None;
        }
    }
}

fn pick_queued_candidate_locked(
    inner: &mut RqInner,
    fair_prev_addr: Option<usize>,
    preferred_fair_addr: Option<usize>,
    cpu_mask: u64,
) -> Option<Arc<Task>> {
    if let Some(key) = inner
        .deadline_tree
        .iter()
        .find(|(_, task)| task_allowed_on(task, cpu_mask))
        .map(|(key, _)| *key)
    {
        return inner.deadline_tree.remove(&key);
    }
    if let Some(key) = inner
        .rt_tree
        .iter()
        .find(|(_, task)| task_allowed_on(task, cpu_mask))
        .map(|(key, _)| *key)
    {
        return inner.rt_tree.remove(&key);
    }
    if let Some(task) = pick_fair_locked(inner, fair_prev_addr, preferred_fair_addr, cpu_mask) {
        return Some(task);
    }
    let key = inner
        .idle_tree
        .iter()
        .find(|(_, task)| task_allowed_on(task, cpu_mask))
        .map(|(key, _)| *key)?;
    inner.idle_tree.remove(&key)
}

fn pick_fair_locked(
    inner: &mut RqInner,
    skip_addr: Option<usize>,
    preferred_addr: Option<usize>,
    cpu_mask: u64,
) -> Option<Arc<Task>> {
    let avg = avg_vruntime_locked(inner);
    let mut preferred = None;
    let mut first_allowed = None;
    let mut first_eligible = None;
    let mut min_non_skip: Option<(FairKey, u64)> = None;

    // 调度器持 rq 锁选择下一个 fair 任务。原实现为了保留“优先不选刚让出
    // CPU 的任务，再退化到最小 vruntime，最后才允许选回它”的语义会多次
    // 扫描 BTreeMap；这里一次遍历同时收集候选，减少 lmbench syscall/context
    // switch 热路径里的锁内扫描成本。
    for (key, task) in inner.fair_tree.iter() {
        if !task_allowed_on(task, cpu_mask) {
            continue;
        }

        if preferred_addr.is_some_and(|addr| task_addr(task) == addr) {
            preferred = Some(*key);
        }
        first_allowed.get_or_insert(*key);

        let is_skip = skip_addr.is_some_and(|skip| task_addr(task) == skip);
        if !is_skip {
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
        } else if skip_addr.is_none() && first_eligible.is_none() && task.sched.vruntime() <= avg {
            first_eligible = Some(*key);
        }
    }

    let key = if skip_addr.is_some() {
        first_eligible
            .or_else(|| min_non_skip.map(|(key, _)| key))
            .or(first_allowed)
    } else {
        first_eligible.or(first_allowed)
    };
    let key = preferred.or(key)?;
    let task = inner.fair_tree.remove(&key)?;
    account_fair_remove_locked(inner, &task);
    Some(task)
}

fn task_allowed_on(task: &Arc<Task>, cpu_mask: u64) -> bool {
    (task.cpu_affinity() & cpu_mask) != 0
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
    let key = inner
        .fair_tree
        .iter()
        .find(|(_, value)| Arc::ptr_eq(value, task))
        .map(|(key, _)| *key)?;
    let task = inner.fair_tree.remove(&key)?;
    account_fair_remove_locked(inner, &task);
    store_fair_lag_locked(inner, &task);
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
    task.sched.set_on_rq(false);
    Some(task)
}

fn remove_deadline_locked(inner: &mut RqInner, task: &Arc<Task>) -> Option<Arc<Task>> {
    let key = inner
        .deadline_tree
        .iter()
        .find(|(_, value)| Arc::ptr_eq(value, task))
        .map(|(key, _)| *key)?;
    let task = inner.deadline_tree.remove(&key)?;
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
    task.sched.set_on_rq(false);
    Some(task)
}

fn take_fair_migratable_locked(inner: &mut RqInner, allowed_cpu_mask: u64) -> Option<Arc<Task>> {
    let key = inner
        .fair_tree
        .iter()
        .rev()
        .find(|(_, task)| (task.cpu_affinity() & allowed_cpu_mask) != 0)
        .map(|(key, _)| *key)?;
    let task = inner.fair_tree.remove(&key)?;
    account_fair_remove_locked(inner, &task);
    store_fair_lag_locked(inner, &task);
    task.sched.set_on_rq(false);
    Some(task)
}

fn take_rt_migratable_locked(inner: &mut RqInner, allowed_cpu_mask: u64) -> Option<Arc<Task>> {
    let key = inner
        .rt_tree
        .iter()
        .rev()
        .find(|(_, task)| (task.cpu_affinity() & allowed_cpu_mask) != 0)
        .map(|(key, _)| *key)?;
    let task = inner.rt_tree.remove(&key)?;
    task.sched.set_on_rq(false);
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
        .find(|(_, task)| (task.cpu_affinity() & allowed_cpu_mask) != 0)
        .map(|(key, _)| *key)?;
    let task = inner.deadline_tree.remove(&key)?;
    task.sched.set_on_rq(false);
    Some(task)
}

fn update_curr_locked(inner: &mut RqInner, now_ns: u64) -> bool {
    if now_ns > inner.last_update_ns {
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
                }
                SchedPolicy::Deadline => {
                    let _ = curr.sched.charge_deadline_runtime(delta);
                }
                SchedPolicy::RtFifo => {}
            }
        }
    }
    requeue_ready_deadline_locked(inner, now_ns)
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
            task.sched.set_on_rq(false);
            continue;
        }
        task.sched.replenish_deadline(now_ns);
        let seq = next_seq(inner);
        inner
            .deadline_tree
            .insert(DeadlineKey::of(&task, seq), task);
        moved = true;
    }
    moved
}

fn update_fair_curr_locked(inner: &mut RqInner, curr: &Arc<Task>, delta_ns: u64) {
    let weight = curr.sched.weight() as u128;
    if weight == 0 {
        return;
    }
    let delta_vr = (delta_ns as u128 * NICE_0_WEIGHT as u128 / weight) as u64;
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
