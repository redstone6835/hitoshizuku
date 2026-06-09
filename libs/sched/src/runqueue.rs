//! 多调度类运行队列。
//!
//! runqueue 对外仍是单 CPU 队列；内部按 `Deadline > Realtime > Fair > Idle`
//! 分层。Fair class 使用 EEVDF；RT class 提供 FIFO/RR 队列骨架；Deadline
//! class 提供 EDF + runtime budget 框架。AP 启动和真实 IPI 不在本模块内。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::eevdf::{NICE_0_WEIGHT, SchedParams};
use crate::sched_class::{RT_PRIO_MAX, SchedAttr, SchedPolicy};
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
}

/// 单 CPU 运行队列。每个 online CPU 持有一份。
pub struct Runqueue {
    inner: Spinlock<RqInner>,
}

impl Runqueue {
    pub const fn new() -> Self {
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
            }),
        }
    }

    pub fn avg_vruntime(&self) -> u64 {
        let inner = self.inner.lock();
        avg_vruntime_locked(&inner)
    }

    pub fn min_vruntime(&self) -> u64 {
        self.inner.lock().min_vruntime
    }

    pub fn nr_running(&self) -> usize {
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
    pub fn migratable_load(&self) -> usize {
        let inner = self.inner.lock();
        inner.fair_tree.len() + inner.rt_tree.len() + inner.deadline_tree.len()
    }

    /// 对指定 CPU 许可位可迁移的就绪负载。
    ///
    /// 亲和性收窄后，任务可能短暂留在旧 CPU 的 rq 中；负载均衡只应选择
    /// 确实能被目标 CPU 拉走的源队列。
    pub fn migratable_load_for(&self, allowed_cpu_mask: u64) -> usize {
        let inner = self.inner.lock();
        inner
            .fair_tree
            .values()
            .filter(|task| task_allowed_on(task, allowed_cpu_mask))
            .count()
            + inner
                .rt_tree
                .values()
                .filter(|task| task_allowed_on(task, allowed_cpu_mask))
                .count()
            + inner
                .deadline_tree
                .values()
                .filter(|task| task_allowed_on(task, allowed_cpu_mask))
                .count()
    }

    pub fn set_current(&self, task: Arc<Task>) {
        let mut inner = self.inner.lock();
        if let Some(old) = inner.current.take() {
            old.sched.set_on_rq(false);
        }
        prepare_running_locked(&mut inner, &task, 0);
        inner.current = Some(task);
    }

    pub fn enqueue(&self, task: Arc<Task>, now_ns: u64) {
        let mut inner = self.inner.lock();
        if task.sched.on_rq() {
            return;
        }
        let _ = update_curr_locked(&mut inner, now_ns);
        task.set_state(TaskState::Runnable);
        task.sched.set_on_rq(true);
        enqueue_queued_locked(&mut inner, task, now_ns);
    }

    pub fn dequeue(&self, task: &Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        dequeue_locked(&mut inner, task)
    }

    pub fn dequeue_queued(&self, task: &Arc<Task>, now_ns: u64) -> bool {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);
        remove_queued_any_locked(&mut inner, task).is_some()
    }

    pub fn is_current(&self, task: &Arc<Task>) -> bool {
        self.inner
            .lock()
            .current
            .as_ref()
            .is_some_and(|curr| Arc::ptr_eq(curr, task))
    }

    pub fn tick(&self, now_ns: u64) -> bool {
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

    pub fn pick_next(&self, now_ns: u64) -> Option<Arc<Task>> {
        self.pick_next_on(now_ns, u64::MAX)
    }

    /// 在指定 CPU 许可位下挑选下一个任务。
    ///
    /// 迁移或亲和性更新可能让某个任务暂时留在不再允许它运行的 rq 中；这里
    /// 只跳过这类任务，不在持有本 rq 锁时跨 CPU 迁移，避免形成跨 rq 锁顺序。
    pub fn pick_next_on(&self, now_ns: u64, cpu_mask: u64) -> Option<Arc<Task>> {
        let mut inner = self.inner.lock();
        let _ = update_curr_locked(&mut inner, now_ns);

        let mut fair_prev_addr = None;
        if let Some(prev) = inner.current.take() {
            if prev.state() == TaskState::Running || prev.state() == TaskState::Runnable {
                prev.set_state(TaskState::Runnable);
                if prev.sched.policy() == SchedPolicy::Fair {
                    fair_prev_addr = Some(task_addr(&prev));
                }
                enqueue_queued_locked(&mut inner, prev, now_ns);
            } else {
                prev.sched.set_on_rq(false);
            }
        }

        let picked = pick_queued_locked(&mut inner, fair_prev_addr, cpu_mask);
        if let Some(ref task) = picked {
            prepare_running_locked(&mut inner, task, now_ns);
            inner.current = Some(Arc::clone(task));
        }
        picked
    }

    pub fn take_migratable(&self, allowed_cpu_mask: u64, now_ns: u64) -> Option<Arc<Task>> {
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

    pub fn current(&self) -> Option<Arc<Task>> {
        self.inner.lock().current.as_ref().map(Arc::clone)
    }

    pub fn resort_after_weight_change(&self, task: &Arc<Task>) {
        let mut inner = self.inner.lock();
        if let Some(curr) = inner.current.as_ref() {
            if Arc::ptr_eq(curr, task) {
                let now = inner.last_update_ns;
                prepare_running_locked(&mut inner, task, now);
                return;
            }
        }

        let Some(owned) = remove_queued_any_locked(&mut inner, task) else {
            let now = inner.last_update_ns;
            prepare_sleeping_locked(&mut inner, task, now);
            return;
        };
        owned.sched.set_on_rq(true);
        owned.set_state(TaskState::Runnable);
        let now = inner.last_update_ns;
        enqueue_queued_locked(&mut inner, owned, now);
    }

    /// 在 rq 锁内更新 nice / slice，并按旧属性先完成出队记账。
    pub fn update_params(&self, task: &Arc<Task>, params: SchedParams, now_ns: u64) {
        self.update_sched_entity(task, now_ns, |task| task.sched.set_params(params));
    }

    /// 在 rq 锁内更新完整调度属性，并按旧 class / 权重完成出队记账。
    pub fn update_sched_attr(&self, task: &Arc<Task>, attr: SchedAttr, now_ns: u64) {
        self.update_sched_entity(task, now_ns, |task| task.sched.set_sched_attr(attr));
    }

    fn update_sched_entity<F>(&self, task: &Arc<Task>, now_ns: u64, update: F)
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
                return;
            }
        }

        if let Some(owned) = remove_queued_any_locked(&mut inner, task) {
            let apply = update.take().expect("[sched] update closure consumed");
            apply(&owned);
            owned.sched.set_on_rq(true);
            owned.set_state(TaskState::Runnable);
            let now = inner.last_update_ns;
            enqueue_queued_locked(&mut inner, owned, now);
            return;
        }

        let apply = update.take().expect("[sched] update closure consumed");
        apply(task);
        let now = inner.last_update_ns;
        prepare_sleeping_locked(&mut inner, task, now);
    }

    pub fn snapshot_runnable(&self) -> Vec<Arc<Task>> {
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
    fair_prev_addr: Option<usize>,
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
    if let Some(task) = pick_fair_locked(inner, fair_prev_addr, cpu_mask) {
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
    cpu_mask: u64,
) -> Option<Arc<Task>> {
    let avg = avg_vruntime_locked(inner);
    let key = if let Some(skip) = skip_addr {
        inner
            .fair_tree
            .iter()
            .find(|(_, task)| {
                task_addr(task) != skip
                    && task_allowed_on(task, cpu_mask)
                    && task.sched.vruntime() <= avg
            })
            .map(|(key, _)| *key)
            .or_else(|| {
                inner
                    .fair_tree
                    .iter()
                    .filter(|(_, task)| task_addr(task) != skip && task_allowed_on(task, cpu_mask))
                    .min_by_key(|(_, task)| task.sched.vruntime())
                    .map(|(key, _)| *key)
            })
            .or_else(|| {
                inner
                    .fair_tree
                    .iter()
                    .find(|(_, task)| task_allowed_on(task, cpu_mask))
                    .map(|(key, _)| *key)
            })
    } else {
        inner
            .fair_tree
            .iter()
            .find(|(_, task)| task_allowed_on(task, cpu_mask) && task.sched.vruntime() <= avg)
            .map(|(key, _)| *key)
            .or_else(|| {
                inner
                    .fair_tree
                    .iter()
                    .find(|(_, task)| task_allowed_on(task, cpu_mask))
                    .map(|(key, _)| *key)
            })
    }?;
    let task = inner.fair_tree.remove(&key)?;
    account_fair_remove_locked(inner, &task);
    Some(task)
}

fn task_allowed_on(task: &Arc<Task>, cpu_mask: u64) -> bool {
    (task.cpu_affinity() & cpu_mask) != 0
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
