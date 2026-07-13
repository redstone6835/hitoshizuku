//! 调度域核心与每 CPU 运行状态所有权。
//!
//! 本模块只收拢调度系统的运行期状态，不改变 EEVDF、RT、Deadline 或任务迁移
//! 策略。所有策略入口仍由 `scheduler` 门面提供，但真实状态只能通过
//! [`Scheduler`] 访问，避免 topology、online mask 与多组 per-CPU 数组
//! 分别演进。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering};

use crate::cpu::{CpuId, CpuMask, MAX_CPUS, SchedTopology};
use crate::runqueue::Runqueue;
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
        }
    }

    pub fn runqueue(&self) -> &Runqueue {
        &self.runqueue
    }

    pub fn current(&self) -> Option<Arc<Task>> {
        self.current.lock().clone()
    }

    pub fn current_raw(&self) -> *mut Task {
        self.current_raw.load(Ordering::Acquire)
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

/// 调度器运行期状态的唯一所有者。
///
/// 第一阶段由它统一拥有 topology、online CPU 集和所有 CPU 运行状态。后续的
/// placement、域负载和迁移事务继续挂到该对象上，而不是重新引入旁路全局状态。
pub struct Scheduler {
    cpus: [CpuSchedState; MAX_CPUS],
    online: AtomicU64,
    topology: Spinlock<TopologyState>,
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
    pub online: CpuMask,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            cpus: [const { CpuSchedState::new() }; MAX_CPUS],
            online: AtomicU64::new(CpuMask::BOOT.bits()),
            topology: Spinlock::new(TopologyState {
                topology: SchedTopology::bootstrap(),
                generation: 1,
            }),
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

    pub fn register_cpu(&self, cpu: CpuId) {
        self.online.fetch_or(cpu.mask().bits(), Ordering::AcqRel);
    }

    pub fn topology(&self) -> SchedTopology {
        self.topology.lock().topology
    }

    pub fn topology_snapshot(&self) -> TopologySnapshot {
        let state = *self.topology.lock();
        TopologySnapshot {
            topology: state.topology,
            generation: state.generation,
            online: self.online_set(),
        }
    }

    pub fn install_topology(&self, topology: SchedTopology) {
        let mut state = self.topology.lock();
        state.topology = topology;
        state.generation = state.generation.wrapping_add(1).max(1);
    }
}

pub static SCHEDULER: Scheduler = Scheduler::new();
