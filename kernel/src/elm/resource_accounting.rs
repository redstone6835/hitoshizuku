//! ELM 运行时资源账本。
//!
//! 该账本与 `ElmCore` 的拓扑锁分离，allocator 热路径和原生调用门只能在这里执行
//! 不分配内存的定长更新。allocator 通过通用回调表接入，因此不会反向依赖 ELM。

use alloc::vec::Vec;

use allocator::{AllocationAccountingOps, register_allocation_accounting_ops};
use elm_model::{ElmId, ElmResourceBudget};
use sched::sync::Spinlock;

const RESOURCE_ACCOUNTING_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResourceAccountingSnapshot {
    pub dynamic_alloc_bytes: u64,
    pub peak_dynamic_alloc_bytes: u64,
    pub max_dynamic_alloc_bytes: u64,
    pub native_stack_bytes: u64,
    pub active_native_calls: u32,
    pub cpu_time_ns_total: u64,
    pub cpu_time_ns_period: u64,
    pub cpu_period_started_at_ns: u64,
    pub cpu_call_overruns: u64,
    pub cpu_period_overruns: u64,
    pub quota_denials: u64,
    pub accounting_errors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeCallAdmissionError {
    CellNotRegistered,
    StackQuota,
    ConcurrentQuota,
    CounterOverflow,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NativeCallAccountingResult {
    pub cpu_time_ns: u64,
    pub watchdog_expired: bool,
}

#[derive(Debug, Clone, Copy)]
struct ResourceEntry {
    cell: u64,
    budget: ElmResourceBudget,
    dynamic_alloc_bytes: u64,
    peak_dynamic_alloc_bytes: u64,
    native_stack_bytes: u64,
    active_native_calls: u32,
    cpu_time_ns_total: u64,
    cpu_time_ns_period: u64,
    cpu_period_started_at_ns: u64,
    cpu_call_overruns: u64,
    cpu_period_overruns: u64,
    quota_denials: u64,
    accounting_errors: u64,
}

impl ResourceEntry {
    const fn new(cell: ElmId, budget: ElmResourceBudget) -> Self {
        Self {
            cell: cell.0,
            budget,
            dynamic_alloc_bytes: 0,
            peak_dynamic_alloc_bytes: 0,
            native_stack_bytes: 0,
            active_native_calls: 0,
            cpu_time_ns_total: 0,
            cpu_time_ns_period: 0,
            cpu_period_started_at_ns: 0,
            cpu_call_overruns: 0,
            cpu_period_overruns: 0,
            quota_denials: 0,
            accounting_errors: 0,
        }
    }

    fn snapshot(self) -> ResourceAccountingSnapshot {
        ResourceAccountingSnapshot {
            dynamic_alloc_bytes: self.dynamic_alloc_bytes,
            peak_dynamic_alloc_bytes: self.peak_dynamic_alloc_bytes,
            max_dynamic_alloc_bytes: self.budget.max_dynamic_alloc_bytes,
            native_stack_bytes: self.native_stack_bytes,
            active_native_calls: self.active_native_calls,
            cpu_time_ns_total: self.cpu_time_ns_total,
            cpu_time_ns_period: self.cpu_time_ns_period,
            cpu_period_started_at_ns: self.cpu_period_started_at_ns,
            cpu_call_overruns: self.cpu_call_overruns,
            cpu_period_overruns: self.cpu_period_overruns,
            quota_denials: self.quota_denials,
            accounting_errors: self.accounting_errors,
        }
    }

    fn refresh_cpu_period(&mut self, now_ns: u64) {
        if self.budget.cpu_period_ns == 0 {
            self.cpu_period_started_at_ns = now_ns;
            self.cpu_time_ns_period = 0;
            return;
        }
        if self.cpu_period_started_at_ns == 0 {
            self.cpu_period_started_at_ns = now_ns;
            return;
        }
        let elapsed = now_ns.saturating_sub(self.cpu_period_started_at_ns);
        if elapsed < self.budget.cpu_period_ns {
            return;
        }
        let periods = elapsed / self.budget.cpu_period_ns;
        self.cpu_period_started_at_ns = self
            .cpu_period_started_at_ns
            .saturating_add(periods.saturating_mul(self.budget.cpu_period_ns));
        self.cpu_time_ns_period = 0;
    }

    fn is_idle(self) -> bool {
        self.dynamic_alloc_bytes == 0
            && self.native_stack_bytes == 0
            && self.active_native_calls == 0
    }
}

struct ResourceRegistry {
    entries: Vec<ResourceEntry>,
}

impl ResourceRegistry {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn index(&self, cell: u64) -> Option<usize> {
        self.entries.iter().position(|entry| entry.cell == cell)
    }

    fn entry_mut(&mut self, cell: u64) -> Option<&mut ResourceEntry> {
        let index = self.index(cell)?;
        self.entries.get_mut(index)
    }
}

static RESOURCE_REGISTRY: Spinlock<ResourceRegistry> = Spinlock::new(ResourceRegistry::new());

static ALLOCATION_OPS: AllocationAccountingOps = AllocationAccountingOps {
    current_owner: allocation_current_owner,
    try_reserve: allocation_try_reserve,
    try_resize: allocation_try_resize,
    release: allocation_release,
};

pub(crate) fn init() -> bool {
    {
        let mut registry = RESOURCE_REGISTRY.lock();
        let capacity = registry.entries.capacity();
        if capacity < RESOURCE_ACCOUNTING_CAPACITY
            && registry
                .entries
                .try_reserve_exact(RESOURCE_ACCOUNTING_CAPACITY - capacity)
                .is_err()
        {
            return false;
        }
    }
    register_allocation_accounting_ops(&ALLOCATION_OPS)
}

pub(crate) fn register_cell(cell: ElmId, budget: ElmResourceBudget) -> bool {
    if cell.0 == 0 || !budget_is_valid(budget) {
        return false;
    }
    let mut registry = RESOURCE_REGISTRY.lock();
    if let Some(entry) = registry.entry_mut(cell.0) {
        if !entry.is_idle() {
            return false;
        }
        *entry = ResourceEntry::new(cell, budget);
        return true;
    }
    if registry.entries.len() >= RESOURCE_ACCOUNTING_CAPACITY {
        return false;
    }
    registry.entries.push(ResourceEntry::new(cell, budget));
    true
}

pub(crate) fn update_budget(cell: ElmId, budget: ElmResourceBudget) -> bool {
    if !budget_is_valid(budget) {
        return false;
    }
    let mut registry = RESOURCE_REGISTRY.lock();
    let Some(entry) = registry.entry_mut(cell.0) else {
        return false;
    };
    if !usage_fits_budget(*entry, budget) {
        return false;
    }
    entry.budget = budget;
    true
}

pub(crate) fn retire_cell(cell: ElmId) -> bool {
    let mut registry = RESOURCE_REGISTRY.lock();
    let Some(index) = registry.index(cell.0) else {
        return true;
    };
    if !registry.entries[index].is_idle() {
        return false;
    }
    registry.entries.swap_remove(index);
    true
}

pub(crate) fn snapshot(cell: ElmId, now_ns: u64) -> ResourceAccountingSnapshot {
    let mut registry = RESOURCE_REGISTRY.lock();
    let Some(entry) = registry.entry_mut(cell.0) else {
        return ResourceAccountingSnapshot::default();
    };
    entry.refresh_cpu_period(now_ns);
    entry.snapshot()
}

pub(crate) fn registered(cell: ElmId) -> bool {
    RESOURCE_REGISTRY.lock().index(cell.0).is_some()
}

pub(crate) fn registered_budget(cell: ElmId) -> Option<ElmResourceBudget> {
    let registry = RESOURCE_REGISTRY.lock();
    registry
        .entries
        .iter()
        .find(|entry| entry.cell == cell.0)
        .map(|entry| entry.budget)
}

pub(crate) fn first_orphaned_cell(mut is_known: impl FnMut(ElmId) -> bool) -> Option<ElmId> {
    let registry = RESOURCE_REGISTRY.lock();
    registry
        .entries
        .iter()
        // 账本会跨局部 ElmCore 测试实例保留统计历史；只有仍持有运行时资源的未知项才是泄漏。
        .filter(|entry| !entry.is_idle())
        .map(|entry| ElmId(entry.cell))
        .find(|cell| !is_known(*cell))
}

pub(crate) fn has_live_allocations(cell: ElmId) -> bool {
    let registry = RESOURCE_REGISTRY.lock();
    registry
        .entries
        .iter()
        .find(|entry| entry.cell == cell.0)
        .is_some_and(|entry| entry.dynamic_alloc_bytes != 0 || entry.native_stack_bytes != 0)
}

pub(crate) struct NativeCallPermit {
    cell: ElmId,
    stack_bytes: u64,
    started_cpu_time_ns: u64,
    watchdog_deadline_ns: u64,
    finished: bool,
}

impl NativeCallPermit {
    pub(crate) const fn watchdog_deadline_ns(&self) -> u64 {
        self.watchdog_deadline_ns
    }

    pub(crate) fn finish(mut self, now_ns: u64) -> NativeCallAccountingResult {
        let now_cpu_time_ns = sched::current_task_cpu_time_ns();
        let result = finish_native_call(
            self.cell,
            self.stack_bytes,
            self.started_cpu_time_ns,
            now_cpu_time_ns,
            self.watchdog_deadline_ns,
            now_ns,
        );
        self.finished = true;
        result
    }
}

impl Drop for NativeCallPermit {
    fn drop(&mut self) {
        if !self.finished {
            let now_ns = sched::now_ns_direct();
            let _ = finish_native_call(
                self.cell,
                self.stack_bytes,
                self.started_cpu_time_ns,
                sched::current_task_cpu_time_ns(),
                self.watchdog_deadline_ns,
                now_ns,
            );
            self.finished = true;
        }
    }
}

pub(crate) fn begin_native_call(
    cell: ElmId,
    stack_bytes: u64,
    requested_deadline_ns: u64,
    now_ns: u64,
) -> Result<NativeCallPermit, NativeCallAdmissionError> {
    let started_cpu_time_ns = sched::current_task_cpu_time_ns();
    let mut registry = RESOURCE_REGISTRY.lock();
    begin_native_call_locked(
        &mut registry,
        cell,
        stack_bytes,
        requested_deadline_ns,
        now_ns,
        started_cpu_time_ns,
    )
}

fn begin_native_call_locked(
    registry: &mut ResourceRegistry,
    cell: ElmId,
    stack_bytes: u64,
    requested_deadline_ns: u64,
    now_ns: u64,
    started_cpu_time_ns: u64,
) -> Result<NativeCallPermit, NativeCallAdmissionError> {
    let Some(entry) = registry.entry_mut(cell.0) else {
        return Err(NativeCallAdmissionError::CellNotRegistered);
    };
    entry.refresh_cpu_period(now_ns);

    let Some(next_stack) = entry.native_stack_bytes.checked_add(stack_bytes) else {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return Err(NativeCallAdmissionError::CounterOverflow);
    };
    if next_stack > entry.budget.max_native_stack_bytes {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return Err(NativeCallAdmissionError::StackQuota);
    }
    // CPU 阈值只做真实运行时间记账。调度器尚未提供可等待的 ELM 节流队列，
    // 因此不能在同步调用门返回提供方故障；栈和并发仍按硬配额拒绝。
    let Some(active_native_calls) = entry.active_native_calls.checked_add(1) else {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return Err(NativeCallAdmissionError::CounterOverflow);
    };
    if active_native_calls > u32::from(entry.budget.max_concurrent_calls) {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return Err(NativeCallAdmissionError::ConcurrentQuota);
    }

    entry.native_stack_bytes = next_stack;
    entry.active_native_calls = active_native_calls;
    Ok(NativeCallPermit {
        cell,
        stack_bytes,
        started_cpu_time_ns,
        watchdog_deadline_ns: requested_deadline_ns,
        finished: false,
    })
}

fn finish_native_call(
    cell: ElmId,
    stack_bytes: u64,
    started_cpu_time_ns: u64,
    now_cpu_time_ns: u64,
    watchdog_deadline_ns: u64,
    now_ns: u64,
) -> NativeCallAccountingResult {
    let cpu_time_ns = now_cpu_time_ns.saturating_sub(started_cpu_time_ns);
    let mut registry = RESOURCE_REGISTRY.lock();
    let Some(entry) = registry.entry_mut(cell.0) else {
        return NativeCallAccountingResult {
            cpu_time_ns,
            watchdog_expired: watchdog_deadline_ns != 0 && now_ns > watchdog_deadline_ns,
        };
    };
    entry.refresh_cpu_period(now_ns);
    if entry.native_stack_bytes < stack_bytes || entry.active_native_calls == 0 {
        entry.accounting_errors = entry.accounting_errors.saturating_add(1);
    }
    entry.native_stack_bytes = entry.native_stack_bytes.saturating_sub(stack_bytes);
    entry.active_native_calls = entry.active_native_calls.saturating_sub(1);
    let period_was_over_budget = entry.cpu_time_ns_period > entry.budget.cpu_budget_ns_per_period;
    entry.cpu_time_ns_total = entry.cpu_time_ns_total.saturating_add(cpu_time_ns);
    entry.cpu_time_ns_period = entry.cpu_time_ns_period.saturating_add(cpu_time_ns);
    if cpu_time_ns > entry.budget.max_cpu_time_ns_per_call {
        entry.cpu_call_overruns = entry.cpu_call_overruns.saturating_add(1);
    }
    if !period_was_over_budget && entry.cpu_time_ns_period > entry.budget.cpu_budget_ns_per_period {
        entry.cpu_period_overruns = entry.cpu_period_overruns.saturating_add(1);
    }
    NativeCallAccountingResult {
        cpu_time_ns,
        watchdog_expired: watchdog_deadline_ns != 0 && now_ns > watchdog_deadline_ns,
    }
}

fn allocation_current_owner() -> u64 {
    if elm_model::current_context().is_some_and(|context| context.cell_id.0 == 0) {
        return 0;
    }
    general::elm_guard::active_cell()
}

fn allocation_try_reserve(owner: u64, bytes: u64) -> bool {
    if owner == 0 {
        return true;
    }
    let mut registry = RESOURCE_REGISTRY.lock();
    let Some(entry) = registry.entry_mut(owner) else {
        return false;
    };
    let Some(next) = entry.dynamic_alloc_bytes.checked_add(bytes) else {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return false;
    };
    if next > entry.budget.max_dynamic_alloc_bytes {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return false;
    }
    entry.dynamic_alloc_bytes = next;
    entry.peak_dynamic_alloc_bytes = entry.peak_dynamic_alloc_bytes.max(next);
    true
}

fn allocation_try_resize(owner: u64, old_bytes: u64, new_bytes: u64) -> bool {
    if owner == 0 || old_bytes == new_bytes {
        return true;
    }
    let mut registry = RESOURCE_REGISTRY.lock();
    let Some(entry) = registry.entry_mut(owner) else {
        return false;
    };
    if new_bytes < old_bytes {
        let released = old_bytes - new_bytes;
        if entry.dynamic_alloc_bytes < released {
            entry.accounting_errors = entry.accounting_errors.saturating_add(1);
        }
        entry.dynamic_alloc_bytes = entry.dynamic_alloc_bytes.saturating_sub(released);
        return true;
    }
    let growth = new_bytes - old_bytes;
    let Some(next) = entry.dynamic_alloc_bytes.checked_add(growth) else {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return false;
    };
    if next > entry.budget.max_dynamic_alloc_bytes {
        entry.quota_denials = entry.quota_denials.saturating_add(1);
        return false;
    }
    entry.dynamic_alloc_bytes = next;
    entry.peak_dynamic_alloc_bytes = entry.peak_dynamic_alloc_bytes.max(next);
    true
}

fn allocation_release(owner: u64, bytes: u64) {
    if owner == 0 {
        return;
    }
    let mut registry = RESOURCE_REGISTRY.lock();
    let Some(entry) = registry.entry_mut(owner) else {
        return;
    };
    if entry.dynamic_alloc_bytes < bytes {
        entry.accounting_errors = entry.accounting_errors.saturating_add(1);
    }
    entry.dynamic_alloc_bytes = entry.dynamic_alloc_bytes.saturating_sub(bytes);
}

fn usage_fits_budget(entry: ResourceEntry, budget: ElmResourceBudget) -> bool {
    entry.dynamic_alloc_bytes <= budget.max_dynamic_alloc_bytes
        && entry.native_stack_bytes <= budget.max_native_stack_bytes
}

pub(crate) const fn budget_is_valid(budget: ElmResourceBudget) -> bool {
    (budget.cpu_budget_ns_per_period == 0 || budget.cpu_period_ns != 0)
        && (budget.max_cpu_time_ns_per_call == 0 || budget.cpu_period_ns != 0)
}
