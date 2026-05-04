//! EEVDF 调度算法的核心参数与每任务状态。
//!
//! Earliest Eligible Virtual Deadline First (EEVDF) 是 CFS 的继任者，
//! 在保留权重公平性的同时引入"延迟敏感"维度：
//!
//! - **vruntime**：虚拟时间轴上的累计运行量，权重越大走得越慢；
//! - **avg_vruntime**：当前 rq 上所有就绪任务的加权平均，用作"eligible"基线；
//! - **eligible**：`vruntime <= avg_vruntime` 的任务才允许调度；
//! - **deadline**：`vruntime + slice * NICE_0_WEIGHT / weight`，越小越紧迫。
//!
//! 调度决策 = 在所有 eligible 任务中选 deadline 最小者。
//!
//! 离开 rq 时保存 `lag = avg_vruntime - vruntime`，回到 rq 时恢复
//! `vruntime = avg_vruntime - lag`，避免长睡眠任务获得不公平的额度。
//!
//! 本模块只关心**单个任务的 EEVDF 状态**与**算法参数**；rq 级别的
//! `avg_vruntime` 推进与队列结构在 [`crate::runqueue`] 中实现。

use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};

use crate::sched_class::{SchedAttr, SchedClass, SchedPolicy};

/// EEVDF 内部使用的权重类型。Linux 中 nice=0 对应 1024。
pub type Weight = u64;

/// `nice == 0` 对应的基准权重。所有相对计算（vruntime 推进、deadline）以此为单位。
pub const NICE_0_WEIGHT: Weight = 1024;

/// 默认基础时间片：6ms（与 Linux `sysctl_sched_base_slice` 同量级）。
pub const DEFAULT_BASE_SLICE_NS: u64 = 6_000_000;

/// nice 值合法范围 `[-20, 19]`。
pub const NICE_MIN: i8 = -20;
pub const NICE_MAX: i8 = 19;

/// 由 nice 推导的权重表。来自 Linux `kernel/sched/core.c::sched_prio_to_weight`。
/// 每相邻两级比率约为 1.25，使得"一级 nice 差"对应大约 10% 的 CPU 份额差异。
const NICE_TO_WEIGHT: [Weight; 40] = [
    88761, 71755, 56483, 46273, 36291, /* nice -20..-16 */
    29154, 23254, 18705, 14949, 11916, /* nice -15..-11 */
    9548, 7620, 6100, 4904, 3906, /* nice -10..-6 */
    3121, 2501, 1991, 1586, 1277, /* nice -5..-1 */
    1024, /* nice 0 */
    820, 655, 526, 423, 335, /* nice 1..5 */
    272, 215, 172, 137, 110, /* nice 6..10 */
    87, 70, 56, 45, 36, /* nice 11..15 */
    29, 23, 18, 15, /* nice 16..19 */
];

/// 把 nice 值钳制到合法区间并换算为 EEVDF 权重。
pub fn weight_from_nice(nice: i8) -> Weight {
    let clamped = nice.clamp(NICE_MIN, NICE_MAX);
    NICE_TO_WEIGHT[(clamped - NICE_MIN) as usize]
}

/// 任务创建 / 属性变更时传入的调度参数。
#[derive(Debug, Clone, Copy)]
pub struct SchedParams {
    /// POSIX nice 值，决定权重。
    pub nice: i8,
    /// 基础时间片（ns）。为 0 时使用 [`DEFAULT_BASE_SLICE_NS`]。
    pub slice_ns: u64,
}

impl SchedParams {
    pub const fn default_fair() -> Self {
        Self {
            nice: 0,
            slice_ns: DEFAULT_BASE_SLICE_NS,
        }
    }

    pub fn weight(&self) -> Weight {
        weight_from_nice(self.nice)
    }

    pub fn slice(&self) -> u64 {
        if self.slice_ns == 0 {
            DEFAULT_BASE_SLICE_NS
        } else {
            self.slice_ns
        }
    }
}

impl From<SchedParams> for SchedAttr {
    fn from(value: SchedParams) -> Self {
        SchedAttr::fair(value.nice, value.slice_ns).normalized()
    }
}

/// 任务的 EEVDF 调度实体状态。
///
/// 字段约定：
///
/// - `weight` 一次性决定后续所有"虚拟时间"推进倍率；调整 nice 时通过
///   [`SchedEntity::set_weight`] 在锁外安全更新（Release，由 rq 重算时 Acquire 读）。
/// - `vruntime` / `deadline` 只在 rq 锁持有下写入，但也提供原子读接口供统计。
/// - `lag` 离开 rq 时保存；重新入队时恢复 `vruntime = avg_vruntime - lag`。
/// - `on_rq` 是观测性标记；权威状态由所在 rq 的索引决定。
pub struct SchedEntity {
    policy: AtomicU8,
    nice: AtomicI64,
    weight: AtomicU64,
    slice_ns: AtomicU64,
    rt_priority: AtomicU8,
    dl_runtime_ns: AtomicU64,
    dl_deadline_ns: AtomicU64,
    dl_period_ns: AtomicU64,
    dl_abs_deadline_ns: AtomicU64,
    dl_budget_ns: AtomicU64,
    rr_remaining_ns: AtomicU64,
    rq_vruntime: AtomicU64,
    rq_weight: AtomicU64,
    vruntime: AtomicU64,
    deadline: AtomicU64,
    lag: AtomicI64,
    on_rq: AtomicBool,
}

impl SchedEntity {
    pub fn new(params: SchedParams) -> Self {
        let attr = SchedAttr::from(params);
        Self {
            policy: AtomicU8::new(attr.policy as u8),
            nice: AtomicI64::new(attr.nice as i64),
            weight: AtomicU64::new(weight_from_nice(attr.nice)),
            slice_ns: AtomicU64::new(attr.slice_ns),
            rt_priority: AtomicU8::new(attr.priority),
            dl_runtime_ns: AtomicU64::new(attr.runtime_ns),
            dl_deadline_ns: AtomicU64::new(attr.deadline_ns),
            dl_period_ns: AtomicU64::new(attr.period_ns),
            dl_abs_deadline_ns: AtomicU64::new(0),
            dl_budget_ns: AtomicU64::new(attr.runtime_ns),
            rr_remaining_ns: AtomicU64::new(attr.slice_ns),
            rq_vruntime: AtomicU64::new(0),
            rq_weight: AtomicU64::new(0),
            vruntime: AtomicU64::new(0),
            deadline: AtomicU64::new(0),
            lag: AtomicI64::new(0),
            on_rq: AtomicBool::new(false),
        }
    }

    pub fn policy(&self) -> SchedPolicy {
        SchedPolicy::from_raw(self.policy.load(Ordering::Acquire)).unwrap_or(SchedPolicy::Fair)
    }

    pub fn class(&self) -> SchedClass {
        self.policy().class()
    }

    pub fn nice(&self) -> i8 {
        self.nice.load(Ordering::Acquire) as i8
    }

    pub fn rt_priority(&self) -> u8 {
        self.rt_priority.load(Ordering::Acquire)
    }

    pub fn sched_attr(&self) -> SchedAttr {
        SchedAttr {
            policy: self.policy(),
            nice: self.nice(),
            slice_ns: self.slice_ns(),
            priority: self.rt_priority(),
            runtime_ns: self.dl_runtime_ns.load(Ordering::Acquire),
            deadline_ns: self.dl_deadline_ns.load(Ordering::Acquire),
            period_ns: self.dl_period_ns.load(Ordering::Acquire),
        }
    }

    pub fn set_sched_attr(&self, attr: SchedAttr) {
        let attr = attr.normalized();
        self.policy.store(attr.policy as u8, Ordering::Release);
        self.nice.store(attr.nice as i64, Ordering::Release);
        self.weight
            .store(weight_from_nice(attr.nice).max(1), Ordering::Release);
        self.slice_ns.store(attr.slice_ns, Ordering::Release);
        self.rt_priority.store(attr.priority, Ordering::Release);
        self.dl_runtime_ns.store(attr.runtime_ns, Ordering::Release);
        self.dl_deadline_ns
            .store(attr.deadline_ns, Ordering::Release);
        self.dl_period_ns.store(attr.period_ns, Ordering::Release);
        self.dl_budget_ns.store(attr.runtime_ns, Ordering::Release);
        self.rr_remaining_ns.store(attr.slice_ns, Ordering::Release);
    }

    pub fn weight(&self) -> Weight {
        self.weight.load(Ordering::Acquire)
    }

    pub fn set_weight(&self, w: Weight) {
        self.weight.store(w.max(1), Ordering::Release);
    }

    pub fn slice_ns(&self) -> u64 {
        self.slice_ns.load(Ordering::Acquire)
    }

    pub fn set_slice_ns(&self, slice: u64) {
        let s = if slice == 0 {
            DEFAULT_BASE_SLICE_NS
        } else {
            slice
        };
        self.slice_ns.store(s, Ordering::Release);
        if self.policy() == SchedPolicy::RtRoundRobin {
            self.rr_remaining_ns.store(s, Ordering::Release);
        }
    }

    pub fn vruntime(&self) -> u64 {
        self.vruntime.load(Ordering::Acquire)
    }

    pub(crate) fn store_vruntime(&self, v: u64) {
        self.vruntime.store(v, Ordering::Release);
    }

    pub fn deadline(&self) -> u64 {
        self.deadline.load(Ordering::Acquire)
    }

    pub(crate) fn store_deadline(&self, d: u64) {
        self.deadline.store(d, Ordering::Release);
    }

    pub fn lag(&self) -> i64 {
        self.lag.load(Ordering::Acquire)
    }

    pub(crate) fn store_lag(&self, l: i64) {
        self.lag.store(l, Ordering::Release);
    }

    pub fn on_rq(&self) -> bool {
        self.on_rq.load(Ordering::Acquire)
    }

    pub(crate) fn set_on_rq(&self, on: bool) {
        self.on_rq.store(on, Ordering::Release);
    }

    pub(crate) fn store_rq_account(&self, vruntime: u64, weight: Weight) {
        self.rq_vruntime.store(vruntime, Ordering::Release);
        self.rq_weight.store(weight, Ordering::Release);
    }

    pub(crate) fn rq_account(&self) -> (u64, Weight) {
        (
            self.rq_vruntime.load(Ordering::Acquire),
            self.rq_weight.load(Ordering::Acquire),
        )
    }

    pub(crate) fn clear_rq_account(&self) {
        self.rq_weight.store(0, Ordering::Release);
        self.rq_vruntime.store(0, Ordering::Release);
    }

    /// `vruntime_delta = delta_exec * NICE_0_WEIGHT / weight`。
    pub fn scale_delta(&self, delta_exec_ns: u64) -> u64 {
        let w = self.weight().max(1);
        // 使用 128 位中间值避免溢出。NICE_0_WEIGHT 最大 88761，delta_exec_ns 可能较大。
        ((delta_exec_ns as u128 * NICE_0_WEIGHT as u128) / w as u128) as u64
    }

    /// 根据当前 vruntime 与 slice 计算新的 deadline。
    pub fn recalc_deadline(&self) -> u64 {
        let vr = self.vruntime();
        let scaled_slice = self.scale_delta(self.slice_ns());
        vr.saturating_add(scaled_slice)
    }

    /// 一次性更新 weight + slice。调用方随后应当调 [`crate::Runqueue::resort_after_weight_change`]
    /// 把 task 在 tree 中的位置重算。
    pub fn set_params(&self, params: SchedParams) {
        let nice = params.nice.clamp(NICE_MIN, NICE_MAX);
        self.nice.store(nice as i64, Ordering::Release);
        self.weight
            .store(weight_from_nice(nice).max(1), Ordering::Release);
        self.slice_ns.store(params.slice(), Ordering::Release);
        if self.policy() == SchedPolicy::RtRoundRobin {
            self.rr_remaining_ns
                .store(params.slice().max(1), Ordering::Release);
        }
    }

    pub fn deadline_runtime_ns(&self) -> u64 {
        self.dl_runtime_ns.load(Ordering::Acquire)
    }

    pub fn deadline_relative_ns(&self) -> u64 {
        self.dl_deadline_ns.load(Ordering::Acquire)
    }

    pub fn deadline_period_ns(&self) -> u64 {
        self.dl_period_ns.load(Ordering::Acquire)
    }

    pub fn absolute_deadline_ns(&self) -> u64 {
        self.dl_abs_deadline_ns.load(Ordering::Acquire)
    }

    pub(crate) fn replenish_deadline(&self, now_ns: u64) {
        let runtime = self.deadline_runtime_ns();
        let relative_deadline = self.deadline_relative_ns();
        self.dl_budget_ns.store(runtime, Ordering::Release);
        self.dl_abs_deadline_ns
            .store(now_ns.saturating_add(relative_deadline), Ordering::Release);
    }

    pub(crate) fn deadline_budget_ns(&self) -> u64 {
        self.dl_budget_ns.load(Ordering::Acquire)
    }

    pub(crate) fn charge_deadline_runtime(&self, delta_ns: u64) -> bool {
        let mut old = self.dl_budget_ns.load(Ordering::Acquire);
        loop {
            let next = old.saturating_sub(delta_ns);
            match self
                .dl_budget_ns
                .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return next == 0,
                Err(actual) => old = actual,
            }
        }
    }

    pub(crate) fn reset_rr_slice(&self) {
        self.rr_remaining_ns
            .store(self.slice_ns().max(1), Ordering::Release);
    }

    pub(crate) fn rr_remaining_ns(&self) -> u64 {
        self.rr_remaining_ns.load(Ordering::Acquire)
    }

    pub(crate) fn charge_rr_runtime(&self, delta_ns: u64) -> bool {
        let mut old = self.rr_remaining_ns.load(Ordering::Acquire);
        loop {
            let next = old.saturating_sub(delta_ns);
            match self.rr_remaining_ns.compare_exchange(
                old,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return next == 0,
                Err(actual) => old = actual,
            }
        }
    }
}
