//! 调度器与每 CPU 运行状态所有权。
//!
//! 本模块只收拢调度系统的运行期状态，不改变 EEVDF、RT、Deadline 或任务迁移
//! 策略。所有策略入口仍由 `scheduler` 门面提供，但真实状态只能通过
//! [`Scheduler`] 访问，避免 topology、CPU 生命周期位图与多组 per-CPU 数组
//! 分别演进。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::cpu::{
    CpuId, CpuMask, MAX_CPUS, MAX_SCHED_DOMAINS, SCHED_CAPACITY_SCALE, SchedTopology,
};
use crate::runqueue::{Runqueue, RunqueueClassLoad};
use crate::sched_class::SchedClass;
use crate::sync::Spinlock;
use crate::task::Task;

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
    need_balance: AtomicBool,
    post_syscall_handoff: AtomicU8,
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
            need_balance: AtomicBool::new(false),
            post_syscall_handoff: AtomicU8::new(0),
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
    pub fn publish_current(&self, task: Arc<Task>) {
        let raw = Arc::into_raw(Arc::clone(&task)) as *mut Task;
        *self.current.lock() = Some(task);
        let old = self.current_raw.swap(raw, Ordering::AcqRel);
        if !old.is_null() {
            // Safety: old 只能由本方法中的 Arc::into_raw 产生，每次 swap 恰好
            // 释放槽位此前持有的一份强引用。
            unsafe { drop(Arc::from_raw(old)) };
        }
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

    pub fn request_resched(&self) {
        self.need_resched.store(true, Ordering::Release);
    }

    pub fn take_resched(&self) -> bool {
        self.need_resched.swap(false, Ordering::AcqRel)
    }

    pub fn request_balance(&self) {
        self.need_balance.store(true, Ordering::Release);
    }

    pub fn take_balance(&self) -> bool {
        self.need_balance.swap(false, Ordering::AcqRel)
    }

    pub fn request_post_syscall_handoff(&self, rounds: u8) {
        let mut old = self.post_syscall_handoff.load(Ordering::Acquire);
        loop {
            if old >= rounds {
                return;
            }
            match self.post_syscall_handoff.compare_exchange_weak(
                old,
                rounds,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => old = actual,
            }
        }
    }

    pub fn take_post_syscall_handoff(&self) -> u8 {
        self.post_syscall_handoff.swap(0, Ordering::AcqRel)
    }

    pub fn clear_scheduling_requests(&self) {
        self.need_resched.store(false, Ordering::Release);
        self.need_balance.store(false, Ordering::Release);
        self.post_syscall_handoff.store(0, Ordering::Release);
    }

    pub(crate) fn begin_enqueue(&self) -> CpuEnqueueGuard<'_> {
        self.enqueue_in_progress.fetch_add(1, Ordering::AcqRel);
        CpuEnqueueGuard { cpu: self }
    }

    fn wait_for_enqueues(&self) {
        while self.enqueue_in_progress.load(Ordering::Acquire) != 0 {
            core::hint::spin_loop();
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
    online: AtomicU64,
    active: AtomicU64,
    topology: Spinlock<TopologyState>,
    domain_stats: Spinlock<[SchedDomainStats; MAX_SCHED_DOMAINS]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TopologyState {
    topology: SchedTopology,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopologySnapshot {
    pub topology: SchedTopology,
    pub generation: u64,
    pub active: CpuMask,
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
            online: AtomicU64::new(CpuMask::BOOT.bits()),
            active: AtomicU64::new(CpuMask::BOOT.bits()),
            topology: Spinlock::new(TopologyState {
                topology: SchedTopology::bootstrap(),
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
        self.topology.lock().topology
    }

    pub fn topology_snapshot(&self) -> TopologySnapshot {
        let state = *self.topology.lock();
        TopologySnapshot {
            topology: state.topology,
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
        for domain_id in 0..snapshot.topology.len() {
            let Some(domain) = snapshot.topology.domain(domain_id) else {
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
                capacity: domain.effective_capacity(snapshot.active),
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
        state.topology = topology;
        state.generation = state.generation.wrapping_add(1).max(1);
        drop(state);
        *self.domain_stats.lock() = [SchedDomainStats::empty(); MAX_SCHED_DOMAINS];
    }
}

pub static SCHEDULER: Scheduler = Scheduler::new();
