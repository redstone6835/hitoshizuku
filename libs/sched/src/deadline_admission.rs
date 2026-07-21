//! SCHED_DEADLINE 带宽准入与预留账本。
//!
//! runqueue 只能看到正在运行或可运行的任务，睡眠任务仍然持有的 Deadline
//! 带宽不能放在 rq 负载里记账。本模块按 CPU 保存独立预留，并用任务弱引用
//! 关联预留所有者；属性更新、迁移和退出都通过同一把短临界区自旋锁串行化。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use errno::Errno;

use crate::cpu::{CpuId, MAX_CPUS, SCHED_CAPACITY_SCALE};
use crate::sched_class::{SchedAttr, SchedPolicy};
use crate::sync::Spinlock;
use crate::task::Task;

#[derive(Clone)]
struct Reservation {
    task: Weak<Task>,
    cpu: CpuId,
    utilization: u64,
}

struct AdmissionState {
    totals: [u64; MAX_CPUS],
    reservations: Vec<Reservation>,
}

impl AdmissionState {
    const fn new() -> Self {
        Self {
            totals: [0; MAX_CPUS],
            reservations: Vec::new(),
        }
    }

    fn prune_dead(&mut self) {
        let mut index = 0;
        while index < self.reservations.len() {
            if self.reservations[index].task.strong_count() != 0 {
                index += 1;
                continue;
            }
            let dead = self.reservations.swap_remove(index);
            self.totals[dead.cpu.get()] =
                self.totals[dead.cpu.get()].saturating_sub(dead.utilization);
        }
    }

    fn find(&self, task: &Arc<Task>) -> Option<usize> {
        let weak = Arc::downgrade(task);
        self.reservations
            .iter()
            .position(|entry| entry.task.ptr_eq(&weak))
    }

    fn fits(&self, cpu: CpuId, replacing: u64, requested: u64, capacity: u64) -> bool {
        self.totals[cpu.get()]
            .saturating_sub(replacing)
            .saturating_add(requested)
            <= capacity
    }

    fn replace(&mut self, task: &Arc<Task>, cpu: CpuId, utilization: u64) {
        if let Some(index) = self.find(task) {
            let old = &self.reservations[index];
            self.totals[old.cpu.get()] = self.totals[old.cpu.get()].saturating_sub(old.utilization);
            if utilization == 0 {
                self.reservations.swap_remove(index);
                return;
            }
            self.reservations[index].cpu = cpu;
            self.reservations[index].utilization = utilization;
        } else if utilization != 0 {
            self.reservations.push(Reservation {
                task: Arc::downgrade(task),
                cpu,
                utilization,
            });
        }
        if utilization != 0 {
            self.totals[cpu.get()] = self.totals[cpu.get()].saturating_add(utilization);
        }
    }
}

/// 独立的 Deadline 准入控制器。每个 [`crate::Scheduler`] 拥有一个实例，
/// 因而预留账本与对应的拓扑及 runqueue 具有相同生命周期。
pub(crate) struct DeadlineAdmission {
    state: Spinlock<AdmissionState>,
}

impl DeadlineAdmission {
    pub(crate) const fn new() -> Self {
        Self {
            state: Spinlock::new(AdmissionState::new()),
        }
    }

    /// 在持有准入锁期间更新任务属性，确保检查和 rq 更新之间没有超配窗口。
    pub(crate) fn update_attr(
        &self,
        task: &Arc<Task>,
        owner_cpu: CpuId,
        attr: SchedAttr,
        capacity: u64,
        apply: impl FnOnce() -> bool,
    ) -> Result<(), Errno> {
        let requested = utilization_of(attr);
        let mut state = self.state.lock();
        state.prune_dead();
        let old_index = state.find(task);
        let (target_cpu, replacing) = old_index
            .map(|index| {
                let old = &state.reservations[index];
                (old.cpu, old.utilization)
            })
            .unwrap_or((owner_cpu, 0));

        if !state.fits(target_cpu, replacing, requested, capacity) {
            return Err(Errno::EBUSY);
        }
        if !apply() {
            return Err(Errno::EBUSY);
        }
        state.replace(task, target_cpu, requested);
        Ok(())
    }

    /// 迁移任务及其预留。调用方负责在闭包中事务化更新 placement/rq。
    pub(crate) fn migrate(
        &self,
        task: &Arc<Task>,
        source_cpu: CpuId,
        target_cpu: CpuId,
        target_capacity: u64,
        apply: impl FnOnce() -> Result<(), Errno>,
    ) -> Result<(), Errno> {
        let mut state = self.state.lock();
        state.prune_dead();
        let index = state.find(task);
        let utilization = index
            .map(|index| state.reservations[index].utilization)
            .unwrap_or_else(|| utilization_of(task.sched.sched_attr()));
        let replacing = index
            .filter(|index| state.reservations[*index].cpu == target_cpu)
            .map(|index| state.reservations[index].utilization)
            .unwrap_or(0);

        if utilization != 0 && !state.fits(target_cpu, replacing, utilization, target_capacity) {
            return Err(Errno::EBUSY);
        }
        apply()?;
        if utilization != 0 {
            let accounted_source = index
                .map(|index| state.reservations[index].cpu)
                .unwrap_or(source_cpu);
            debug_assert_eq!(accounted_source, source_cpu);
            state.replace(task, target_cpu, utilization);
        }
        Ok(())
    }

    pub(crate) fn can_migrate(
        &self,
        task: &Arc<Task>,
        target_cpu: CpuId,
        target_capacity: u64,
    ) -> bool {
        let mut state = self.state.lock();
        state.prune_dead();
        let index = state.find(task);
        let utilization = index
            .map(|index| state.reservations[index].utilization)
            .unwrap_or_else(|| utilization_of(task.sched.sched_attr()));
        let replacing = index
            .filter(|index| state.reservations[*index].cpu == target_cpu)
            .map(|index| state.reservations[index].utilization)
            .unwrap_or(0);
        state.fits(target_cpu, replacing, utilization, target_capacity)
    }

    pub(crate) fn release(&self, task: &Arc<Task>) {
        let mut state = self.state.lock();
        state.prune_dead();
        if let Some(index) = state.find(task) {
            let entry = state.reservations.swap_remove(index);
            state.totals[entry.cpu.get()] =
                state.totals[entry.cpu.get()].saturating_sub(entry.utilization);
        }
    }

    pub(crate) fn reserved(&self, cpu: CpuId) -> u64 {
        let mut state = self.state.lock();
        state.prune_dead();
        state.totals[cpu.get()]
    }

    pub(crate) fn totals(&self) -> [u64; MAX_CPUS] {
        let mut state = self.state.lock();
        state.prune_dead();
        state.totals
    }

    pub(crate) fn fits_capacities(&self, capacities: [u64; MAX_CPUS]) -> bool {
        let mut state = self.state.lock();
        state.prune_dead();
        state
            .totals
            .iter()
            .zip(capacities)
            .all(|(reserved, capacity)| *reserved <= capacity)
    }

    pub(crate) fn tasks_on_cpu(&self, cpu: CpuId) -> Vec<Arc<Task>> {
        let mut state = self.state.lock();
        state.prune_dead();
        state
            .reservations
            .iter()
            .filter(|entry| entry.cpu == cpu)
            .filter_map(|entry| entry.task.upgrade())
            .collect()
    }

    pub(crate) fn reservation_of(&self, task: &Arc<Task>) -> Option<(CpuId, u64)> {
        let mut state = self.state.lock();
        state.prune_dead();
        state.find(task).map(|index| {
            let entry = &state.reservations[index];
            (entry.cpu, entry.utilization)
        })
    }
}

pub(crate) fn utilization_of(attr: SchedAttr) -> u64 {
    if attr.policy != SchedPolicy::Deadline || attr.runtime_ns == 0 || attr.period_ns == 0 {
        return 0;
    }
    ((attr.runtime_ns as u128 * SCHED_CAPACITY_SCALE as u128) / attr.period_ns as u128)
        .min(u64::MAX as u128) as u64
}
