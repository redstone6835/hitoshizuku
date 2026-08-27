//! 调度器与每 CPU 运行状态所有权。
//!
//! 本模块只收拢调度系统的运行期状态，不改变 EEVDF、RT、Deadline 或任务迁移
//! 策略。所有策略入口仍由 `scheduler` 门面提供，但真实状态只能通过
//! [`Scheduler`] 访问，避免 topology、CPU 生命周期位图与多组 per-CPU 数组
//! 分别演进。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::null_mut;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

use crate::cpu::{
    CpuId, CpuMask, MAX_CPUS, MAX_SCHED_DOMAINS, SCHED_CAPACITY_SCALE, SchedTopology,
};
use crate::deadline_admission::DeadlineAdmission;
use crate::runqueue::{Runqueue, RunqueueClassLoad};
use crate::sched_class::SchedClass;
use crate::sync::Spinlock;
use crate::task::Task;

const TARGETED_HANDOFF_PENDING: u8 = u8::MAX;

/// 精确交接的来源。调度器只用它做诊断与合并，不改变对应事件的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffReason {
    SocketRead,
    SocketReadContinuation,
}

/// 从唤醒点传到公共 syscall 返回边界的稳定任务身份。
///
/// 强引用保证任务对象在请求存续期间不会被复用；调度边界还会复查请求代际、
/// CPU、队列成员、状态和执行所有权，避免依赖裸地址或可复用 tid。
#[derive(Clone)]
pub struct HandoffTarget {
    pub(crate) task: Arc<Task>,
    pub(crate) preferred_cpu: usize,
    pub(crate) request_generation: u64,
    pub(crate) reason: HandoffReason,
    pub(crate) woke_from_sleep: bool,
}

impl HandoffTarget {
    pub(crate) fn new(
        task: Arc<Task>,
        preferred_cpu: usize,
        reason: HandoffReason,
        woke_from_sleep: bool,
    ) -> Self {
        Self {
            task,
            preferred_cpu,
            request_generation: 0,
            reason,
            woke_from_sleep,
        }
    }

    pub fn preferred_cpu(&self) -> usize {
        self.preferred_cpu
    }
}

/// 单个 CPU 的完整调度运行状态。
///
/// `Runqueue`、current、idle 和调度意图必须作为一个所有权单元存在。跨 CPU
/// 决策由 [`Scheduler`] 完成，本对象不扫描或操作其它 CPU。
pub struct CpuSchedState {
    runqueue: Runqueue,
    current: Spinlock<Option<Arc<Task>>>,
    current_raw: AtomicPtr<Task>,
    idle: Spinlock<Option<Arc<Task>>>,
    retired: Spinlock<Vec<Arc<Task>>>,
    retired_nonempty: AtomicBool,
    need_resched: AtomicBool,
    /// 本 CPU 存在返回用户态前必须处理的调度工作。
    ///
    /// 这是 Linux TIF 风格的粘性提示，不替代 need_resched/handoff 等权威字段。
    user_return_work: AtomicU32,
    resched_notification_pending: AtomicBool,
    need_balance: AtomicBool,
    post_syscall_handoff: AtomicU8,
    targeted_handoff: Spinlock<Option<HandoffTarget>>,
    targeted_handoff_generation: AtomicU64,
    enqueue_in_progress: AtomicUsize,
}

impl CpuSchedState {
    pub const fn new() -> Self {
        Self {
            runqueue: Runqueue::new(),
            current: Spinlock::new(None),
            current_raw: AtomicPtr::new(null_mut()),
            idle: Spinlock::new(None),
            retired: Spinlock::new(Vec::new()),
            retired_nonempty: AtomicBool::new(false),
            need_resched: AtomicBool::new(false),
            user_return_work: AtomicU32::new(0),
            resched_notification_pending: AtomicBool::new(false),
            need_balance: AtomicBool::new(false),
            post_syscall_handoff: AtomicU8::new(0),
            targeted_handoff: Spinlock::new(None),
            targeted_handoff_generation: AtomicU64::new(0),
            enqueue_in_progress: AtomicUsize::new(0),
        }
    }

    pub(crate) fn runqueue(&self) -> &Runqueue {
        &self.runqueue
    }

    pub fn current(&self) -> Option<Arc<Task>> {
        self.current.lock().clone()
    }

    pub fn current_raw(&self) -> *mut Task {
        self.current_raw.load(Ordering::Acquire)
    }

    pub fn clear_current(&self) -> Option<Arc<Task>> {
        let current = self.current.lock().take();
        let raw = self.current_raw.swap(null_mut(), Ordering::AcqRel);
        if !raw.is_null() {
            // Safety: raw 由 publish_current 中的 Arc::into_raw 产生。
            unsafe { drop(Arc::from_raw(raw)) };
        }
        current
    }

    /// 同时发布 owning current 槽和无锁热路径指针。
    pub fn publish_current(&self, task: Arc<Task>) -> *mut Task {
        let raw = Arc::into_raw(Arc::clone(&task)) as *mut Task;
        *self.current.lock() = Some(task);
        let old = self.current_raw.swap(raw, Ordering::AcqRel);
        if !old.is_null() {
            // Safety: old 只能由本方法中的 Arc::into_raw 产生，每次 swap 恰好
            // 释放槽位此前持有的一份强引用。
            unsafe { drop(Arc::from_raw(old)) };
        }
        raw
    }

    pub fn idle(&self) -> Option<Arc<Task>> {
        self.idle.lock().clone()
    }

    pub fn clear_idle(&self) -> Option<Arc<Task>> {
        self.idle.lock().take()
    }

    pub fn install_idle(&self, task: Arc<Task>) -> Result<(), Arc<Task>> {
        let mut slot = self.idle.lock();
        if slot.is_some() {
            return Err(task);
        }
        *slot = Some(task);
        Ok(())
    }

    pub fn needs_resched(&self) -> bool {
        self.need_resched.load(Ordering::Acquire)
    }

    /// 返回可供架构 HartLocal 缓存的稳定 hint 地址。
    #[inline]
    pub(crate) fn user_return_work_ptr(&self) -> *const AtomicU32 {
        &self.user_return_work
    }

    /// 当前 CPU 返回用户态前的无栅栏 hint 读取。
    #[inline(always)]
    pub(crate) fn user_return_work_pending_relaxed(&self) -> bool {
        self.user_return_work.load(Ordering::Relaxed) != 0
    }

    /// 慢路径与 CPU 工作生产者建立 Acquire/Release 同步。
    #[inline]
    pub(crate) fn user_return_work_pending_acquire(&self) -> bool {
        self.user_return_work.load(Ordering::Acquire) != 0
    }

    #[inline]
    pub(crate) fn mark_user_return_work(&self) {
        self.user_return_work.fetch_or(1, Ordering::Release);
    }

    #[inline]
    pub(crate) fn take_user_return_work(&self) -> bool {
        self.user_return_work.swap(0, Ordering::AcqRel) != 0
    }

    #[inline]
    pub(crate) fn user_return_work_authoritative(&self) -> bool {
        self.need_resched.load(Ordering::Acquire)
            || self.need_balance.load(Ordering::Acquire)
            || self.post_syscall_handoff.load(Ordering::Acquire) != 0
    }

    pub fn request_resched(&self) {
        self.need_resched.store(true, Ordering::Release);
        self.mark_user_return_work();
    }

    /// 尝试取得一次远端调度通知的发送权。
    ///
    /// `need_resched` 可以在同一目标 CPU 真正处理前被多个生产者重复设置；这个
    /// 独立状态只合并硬件 IPI，不改变调度请求本身。返回 `true` 的调用者负责
    /// 实际发送通知。
    pub(crate) fn claim_resched_notification(&self) -> bool {
        !self
            .resched_notification_pending
            .swap(true, Ordering::AcqRel)
    }

    /// 清除已经消费或即将在本调度边界处理的远端通知状态。
    pub(crate) fn clear_resched_notification(&self) {
        self.resched_notification_pending
            .store(false, Ordering::Release);
    }

    pub fn take_resched(&self) -> bool {
        let requested = self.need_resched.swap(false, Ordering::AcqRel);
        self.clear_resched_notification();
        requested
    }

    pub fn request_balance(&self) {
        self.need_balance.store(true, Ordering::Release);
        // balance 请求也必须唤醒返回用户态前的慢路径；否则忙 CPU 在 timer
        // tick 上只置 need_balance、却没有 need_resched 时永远不会消费请求。
        self.mark_user_return_work();
    }

    pub fn take_balance(&self) -> bool {
        self.need_balance.swap(false, Ordering::AcqRel)
    }

    pub fn request_post_syscall_handoff(&self, rounds: u8) {
        let mut old = self.post_syscall_handoff.load(Ordering::Acquire);
        loop {
            if old >= rounds {
                self.mark_user_return_work();
                return;
            }
            match self.post_syscall_handoff.compare_exchange_weak(
                old,
                rounds,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.mark_user_return_work();
                    return;
                }
                Err(actual) => old = actual,
            }
        }
    }

    pub fn take_post_syscall_handoff(&self) -> (u8, Option<HandoffTarget>) {
        let state = self.post_syscall_handoff.swap(0, Ordering::AcqRel);
        if state == TARGETED_HANDOFF_PENDING {
            (0, self.targeted_handoff.lock().take())
        } else {
            (state, None)
        }
    }

    pub fn request_targeted_handoff(&self, mut target: HandoffTarget) {
        target.request_generation = self
            .targeted_handoff_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let mut pending = self.targeted_handoff.lock();
        if pending
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.task, &target.task))
        {
            self.post_syscall_handoff
                .store(TARGETED_HANDOFF_PENDING, Ordering::Release);
            self.mark_user_return_work();
            return;
        }
        *pending = Some(target);
        drop(pending);
        self.post_syscall_handoff
            .store(TARGETED_HANDOFF_PENDING, Ordering::Release);
        self.mark_user_return_work();
    }

    /// 清理已静止、即将下线 CPU 的调度请求。
    ///
    /// 调用方必须先禁止新任务进入该 CPU，并摘除 current/idle。此接口不是并发
    /// 消费协议。聚合 hint 故意保留：deferred timer payload 位于调度器外部，CPU
    /// 重新上线后的第一次返回慢路径会统一复查并清掉可能的陈旧提示。
    pub(crate) fn clear_scheduling_requests(&self) {
        debug_assert!(self.current_raw.load(Ordering::Acquire).is_null());
        self.need_resched.store(false, Ordering::Release);
        self.clear_resched_notification();
        self.need_balance.store(false, Ordering::Release);
        self.post_syscall_handoff.store(0, Ordering::Release);
        self.targeted_handoff.lock().take();
    }

    pub(crate) fn begin_enqueue(&self) -> CpuEnqueueGuard<'_> {
        self.enqueue_in_progress.fetch_add(1, Ordering::AcqRel);
        CpuEnqueueGuard { cpu: self }
    }

    fn wait_for_enqueues(&self) {
        // 等待期间必须协作处理 TLB shootdown / membarrier：入队方可能正阻塞在
        // 一个需要本 CPU 应答 shootdown 才能推进的路径上，纯自旋会让双方互等，
        // 表现为远端 CPU 报告 "shootdown 确认等待过长"。
        let mut spins = 0usize;
        while self.enqueue_in_progress.load(Ordering::Acquire) != 0 {
            core::hint::spin_loop();
            spins = spins.wrapping_add(1);
            if spins.is_multiple_of(64) {
                crate::poll_urgent_work();
            }
        }
    }

    pub fn has_post_syscall_handoff(&self) -> bool {
        self.post_syscall_handoff.load(Ordering::Acquire) != 0
    }

    pub fn retire(&self, task: Arc<Task>) {
        self.retired.lock().push(task);
        self.retired_nonempty.store(true, Ordering::Release);
    }

    pub fn take_retired(&self) -> Vec<Arc<Task>> {
        if !self.retired_nonempty.load(Ordering::Acquire) {
            return Vec::new();
        }
        let mut slot = self.retired.lock();
        let retired = core::mem::take(&mut *slot);
        self.retired_nonempty.store(false, Ordering::Release);
        retired
    }

    pub fn retired_len(&self) -> usize {
        self.retired.lock().len()
    }
}

pub(crate) struct CpuEnqueueGuard<'a> {
    cpu: &'a CpuSchedState,
}

impl Drop for CpuEnqueueGuard<'_> {
    fn drop(&mut self) {
        self.cpu.enqueue_in_progress.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 调度器运行期状态的唯一所有者。
///
/// 由它统一拥有 topology、online/active CPU 集和所有 CPU 运行状态。后续的
/// placement、域负载和迁移事务继续挂到该对象上，而不是重新引入旁路全局状态。
pub struct Scheduler {
    cpus: [CpuSchedState; MAX_CPUS],
    deadline_admission: DeadlineAdmission,
    online: AtomicU64,
    active: AtomicU64,
    /// 当前拓扑代际的无锁镜像。归属未变化时无需读取完整拓扑。
    topology_generation: AtomicU64,
    topology: Spinlock<TopologyState>,
    domain_stats: Spinlock<[SchedDomainStats; MAX_SCHED_DOMAINS]>,
}

#[derive(Debug, Clone)]
struct TopologyState {
    topology: TopologyStorage,
    generation: u64,
}

/// 启动期拓扑必须保持 const 初始化；运行时安装的拓扑用 Arc 保活，使拿到旧快照的
/// 迁移和诊断路径不必复制整棵固定容量拓扑，也不会看到已释放的旧配置。
#[derive(Debug, Clone)]
enum TopologyStorage {
    Bootstrap,
    Installed(Arc<SchedTopology>),
}

impl TopologyStorage {
    #[inline]
    fn topology(&self) -> &SchedTopology {
        match self {
            Self::Bootstrap => &BOOTSTRAP_TOPOLOGY,
            Self::Installed(topology) => topology,
        }
    }

    #[cfg(test)]
    fn same_storage(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bootstrap, Self::Bootstrap) => true,
            (Self::Installed(left), Self::Installed(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

static BOOTSTRAP_TOPOLOGY: SchedTopology = SchedTopology::bootstrap();

#[derive(Debug, Clone)]
pub struct TopologySnapshot {
    topology: TopologyStorage,
    pub generation: u64,
    pub active: CpuMask,
}

impl TopologySnapshot {
    #[inline]
    pub fn topology(&self) -> &SchedTopology {
        self.topology.topology()
    }

    #[cfg(test)]
    pub(crate) fn for_test(topology: SchedTopology, generation: u64, active: CpuMask) -> Self {
        Self {
            topology: TopologyStorage::Installed(Arc::new(topology)),
            generation,
            active,
        }
    }

    #[cfg(test)]
    pub(crate) fn shares_topology_storage_with(&self, other: &Self) -> bool {
        self.topology.same_storage(&other.topology)
    }
}

/// 一个调度域在指定拓扑代际下的聚合负载与有效容量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedDomainStats {
    pub generation: u64,
    pub active: CpuMask,
    pub capacity: u64,
    pub load: RunqueueClassLoad,
}

impl SchedDomainStats {
    pub const fn empty() -> Self {
        Self {
            generation: 0,
            active: CpuMask::EMPTY,
            capacity: 0,
            load: RunqueueClassLoad {
                deadline: 0,
                deadline_utilization: 0,
                realtime: 0,
                fair: 0,
                fair_weight: 0,
            },
        }
    }

    /// 以 [`SCHED_CAPACITY_SCALE`] 为 100% 计算指定调度类的平均利用率。
    pub fn utilization(self, class: SchedClass) -> u64 {
        if self.capacity == 0 {
            return 0;
        }
        self.load
            .balance_load(class)
            .saturating_mul(SCHED_CAPACITY_SCALE)
            / self.capacity
    }
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            cpus: [const { CpuSchedState::new() }; MAX_CPUS],
            deadline_admission: DeadlineAdmission::new(),
            online: AtomicU64::new(CpuMask::BOOT.bits()),
            active: AtomicU64::new(CpuMask::BOOT.bits()),
            topology_generation: AtomicU64::new(1),
            topology: Spinlock::new(TopologyState {
                topology: TopologyStorage::Bootstrap,
                generation: 1,
            }),
            domain_stats: Spinlock::new([SchedDomainStats::empty(); MAX_SCHED_DOMAINS]),
        }
    }

    pub fn cpu(&self, cpu_id: usize) -> Option<&CpuSchedState> {
        self.cpus.get(cpu_id)
    }

    pub fn cpu_or_boot(&self, cpu_id: usize) -> &CpuSchedState {
        self.cpu(cpu_id).unwrap_or(&self.cpus[CpuId::boot().get()])
    }

    pub fn cpus(&self) -> &[CpuSchedState; MAX_CPUS] {
        &self.cpus
    }

    pub(crate) fn deadline_admission(&self) -> &DeadlineAdmission {
        &self.deadline_admission
    }

    pub fn online_set(&self) -> CpuMask {
        CpuMask::from_bits_truncate(self.online.load(Ordering::Acquire)).or_boot()
    }

    pub fn active_set(&self) -> CpuMask {
        CpuMask::from_bits_truncate(self.active.load(Ordering::Acquire)).or_boot()
    }

    pub fn mark_cpu_online(&self, cpu: CpuId) -> bool {
        let old = self.online.fetch_or(cpu.mask().bits(), Ordering::AcqRel);
        *self.domain_stats.lock() = [SchedDomainStats::empty(); MAX_SCHED_DOMAINS];
        (old & cpu.mask().bits()) == 0
    }

    pub fn register_cpu(&self, cpu: CpuId) {
        let _ = self.mark_cpu_online(cpu);
        let _ = self.activate_cpu(cpu);
    }

    pub fn deactivate_cpu(&self, cpu: CpuId) -> bool {
        if cpu == CpuId::boot() {
            return false;
        }
        let old = self.active.fetch_and(!cpu.mask().bits(), Ordering::AcqRel);
        if old & cpu.mask().bits() == 0 {
            return false;
        }
        self.cpu_or_boot(cpu.get()).wait_for_enqueues();
        *self.domain_stats.lock() = [SchedDomainStats::empty(); MAX_SCHED_DOMAINS];
        true
    }

    pub fn activate_cpu(&self, cpu: CpuId) -> bool {
        if !self.online_set().contains(cpu) {
            return false;
        }
        self.active.fetch_or(cpu.mask().bits(), Ordering::AcqRel);
        *self.domain_stats.lock() = [SchedDomainStats::empty(); MAX_SCHED_DOMAINS];
        true
    }

    pub fn mark_cpu_offline(&self, cpu: CpuId) -> bool {
        if cpu == CpuId::boot() || self.active_set().contains(cpu) {
            return false;
        }
        self.cpu_or_boot(cpu.get()).wait_for_enqueues();
        let old = self.online.fetch_and(!cpu.mask().bits(), Ordering::AcqRel);
        *self.domain_stats.lock() = [SchedDomainStats::empty(); MAX_SCHED_DOMAINS];
        (old & cpu.mask().bits()) != 0
    }

    pub fn unregister_cpu(&self, cpu: CpuId) -> bool {
        let was_online = self.online_set().contains(cpu);
        let _ = self.deactivate_cpu(cpu);
        let _ = self.mark_cpu_offline(cpu);
        was_online && !self.online_set().contains(cpu)
    }

    pub fn topology(&self) -> SchedTopology {
        self.topology.lock().topology.topology().clone()
    }

    pub fn topology_generation(&self) -> u64 {
        self.topology_generation.load(Ordering::Acquire)
    }

    pub fn topology_snapshot(&self) -> TopologySnapshot {
        let state = self.topology.lock();
        TopologySnapshot {
            topology: state.topology.clone(),
            generation: state.generation,
            active: self.active_set(),
        }
    }

    pub fn update_domain_stats(
        &self,
        snapshot: TopologySnapshot,
        cpu_loads: &[RunqueueClassLoad; MAX_CPUS],
    ) {
        let mut stats = [SchedDomainStats::empty(); MAX_SCHED_DOMAINS];
        for domain_id in 0..snapshot.topology().len() {
            let Some(domain) = snapshot.topology().domain(domain_id) else {
                continue;
            };
            let cpus = domain.span().intersection(snapshot.active);
            let mut load = RunqueueClassLoad::default();
            for cpu in cpus.iter() {
                load = load.add(cpu_loads[cpu.get()]);
            }
            stats[domain_id] = SchedDomainStats {
                generation: snapshot.generation,
                active: snapshot.active,
                capacity: snapshot.topology().capacity_of(cpus),
                load,
            };
        }
        let state = self.topology.lock();
        if state.generation == snapshot.generation && self.active_set() == snapshot.active {
            *self.domain_stats.lock() = stats;
        }
    }

    pub fn domain_stats(&self, domain_id: usize) -> Option<SchedDomainStats> {
        let snapshot = self.topology_snapshot();
        let stats = self.domain_stats.lock();
        let value = *stats.get(domain_id)?;
        (value.capacity != 0
            && value.generation == snapshot.generation
            && value.active == snapshot.active)
            .then_some(value)
    }

    pub fn install_topology(&self, topology: SchedTopology) {
        let mut state = self.topology.lock();
        let generation = state.generation.wrapping_add(1).max(1);
        // 先发布新代际，让并发观察者在旧拓扑失效后进入带锁慢路径。
        self.topology_generation
            .store(generation, Ordering::Release);
        state.topology = TopologyStorage::Installed(Arc::new(topology));
        state.generation = generation;
        drop(state);
        *self.domain_stats.lock() = [SchedDomainStats::empty(); MAX_SCHED_DOMAINS];
    }
}

pub static SCHEDULER: Scheduler = Scheduler::new();

#[cfg(test)]
mod tests {
    use super::CpuSchedState;

    #[test]
    fn balance_only_return_work_can_be_fully_consumed() {
        let cpu = CpuSchedState::new();
        cpu.request_balance();
        assert!(cpu.user_return_work_authoritative());
        assert!(!cpu.needs_resched());

        // 模拟 RISC-V syscall 返回慢路径：先在安全调度边界消费 balance，
        // 随后清除并权威复查聚合 hint。不能因没有 need_resched 而跳过前者。
        assert!(cpu.take_balance());
        assert!(cpu.take_user_return_work());
        assert!(!cpu.user_return_work_authoritative());
        assert!(!cpu.user_return_work_pending_acquire());
    }
}
