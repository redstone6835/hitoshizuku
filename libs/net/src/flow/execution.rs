//! 协议状态的可转移执行租约。
//!
//! 本模块只负责保证同一份协议状态同一时刻最多有一个写者。执行者可以是固定 worker、
//! 普通系统调用任务或故障恢复路径；租约不提供等待接口，竞争者只能发布 pending 并退回
//! 原有调度路径。

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering, fence};

const BUSY: u64 = 1 << 0;
const PENDING: u64 = 1 << 1;
const EXECUTOR_KIND_SHIFT: u32 = 8;
const EXECUTOR_CPU_SHIFT: u32 = 16;
const GENERATION_SHIFT: u32 = 32;
const EXECUTOR_KIND_MASK: u64 = 0xff << EXECUTOR_KIND_SHIFT;
const EXECUTOR_CPU_MASK: u64 = 0xff << EXECUTOR_CPU_SHIFT;
const GENERATION_MASK: u64 = (u32::MAX as u64) << GENERATION_SHIFT;

/// 当前持有协议状态执行权的上下文类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FlowExecutorKind {
    Worker = 1,
    Syscall = 2,
    Recovery = 3,
}

/// 一份只能用于观测、不能用于授权的执行状态快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlowExecutionSnapshot {
    pub generation: u32,
    pub executor_kind: Option<FlowExecutorKind>,
    pub executor_cpu: Option<u8>,
    pub busy: bool,
    pub pending: bool,
}

/// 绑定 generation 的原子执行状态。
///
/// generation、busy、pending 和执行者元数据位于同一个原子字中，因此代际切换不会与
/// try-acquire 产生撕裂。调用方只能在旧 generation 已经停止接收新调用后安装新代际。
pub struct FlowExecution {
    state: AtomicU64,
}

impl FlowExecution {
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
        }
    }

    /// 在协议状态尚未发布或已经完成排空时安装 generation。
    #[inline(always)]
    pub fn install_generation(&self, generation: u64) -> bool {
        let Ok(generation) = u32::try_from(generation) else {
            return false;
        };
        if generation == 0 {
            return false;
        }
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            if observed & BUSY != 0 {
                return false;
            }
            let next = u64::from(generation) << GENERATION_SHIFT;
            match self.state.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => observed = actual,
            }
        }
    }

    /// 尝试取得执行权；失败时不会等待或改变当前执行者。
    #[inline(always)]
    pub fn try_acquire(
        &self,
        expected_generation: u64,
        executor_kind: FlowExecutorKind,
        executor_cpu: usize,
    ) -> Option<FlowExecLease<'_>> {
        let generation = u32::try_from(expected_generation).ok()?;
        let cpu = u8::try_from(executor_cpu).ok()?;
        if generation == 0 {
            return None;
        }
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            if observed & BUSY != 0 || generation_of(observed) != generation {
                return None;
            }
            // 取得租约时消费此前的 pending；持有期间新到达的 work 会重新置位。
            let next = (observed & GENERATION_MASK)
                | BUSY
                | ((executor_kind as u64) << EXECUTOR_KIND_SHIFT)
                | (u64::from(cpu) << EXECUTOR_CPU_SHIFT);
            match self.state.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(FlowExecLease {
                        execution: self,
                        generation,
                        released: false,
                        _not_send: PhantomData,
                    });
                }
                Err(actual) => observed = actual,
            }
        }
    }

    /// 发布执行期间或执行间隙到达的新工作。
    #[inline(always)]
    pub fn mark_pending(&self) {
        self.state.fetch_or(PENDING, Ordering::Release);
    }

    #[inline(always)]
    pub fn has_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) & PENDING != 0
    }

    #[inline(always)]
    pub fn snapshot(&self) -> FlowExecutionSnapshot {
        let state = self.state.load(Ordering::Acquire);
        let busy = state & BUSY != 0;
        let kind = ((state & EXECUTOR_KIND_MASK) >> EXECUTOR_KIND_SHIFT) as u8;
        FlowExecutionSnapshot {
            generation: generation_of(state),
            executor_kind: busy.then(|| match kind {
                1 => FlowExecutorKind::Worker,
                2 => FlowExecutorKind::Syscall,
                3 => FlowExecutorKind::Recovery,
                _ => unreachable!("busy 执行状态必须携带合法执行者类型"),
            }),
            executor_cpu: busy.then(|| ((state & EXECUTOR_CPU_MASK) >> EXECUTOR_CPU_SHIFT) as u8),
            busy,
            pending: state & PENDING != 0,
        }
    }

    #[inline(always)]
    fn release(&self, generation: u32) -> bool {
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            assert_eq!(
                generation_of(observed),
                generation,
                "执行租约不能跨 generation 释放"
            );
            assert!(observed & BUSY != 0, "执行租约不能重复释放");
            let next = (observed & GENERATION_MASK) | (observed & PENDING);
            match self.state.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // 与 mark_pending 的 Release 发布配对，覆盖其发生在释放 CAS 前后的两种时序。
                    fence(Ordering::Acquire);
                    return self.state.load(Ordering::Acquire) & PENDING != 0;
                }
                Err(actual) => observed = actual,
            }
        }
    }
}

impl Default for FlowExecution {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn generation_of(state: u64) -> u32 {
    ((state & GENERATION_MASK) >> GENERATION_SHIFT) as u32
}

/// try-acquire 成功后得到的不可跨线程 RAII 租约。
pub struct FlowExecLease<'a> {
    execution: &'a FlowExecution,
    generation: u32,
    released: bool,
    _not_send: PhantomData<*mut ()>,
}

impl FlowExecLease<'_> {
    /// 显式释放并返回释放边界之后是否仍有待处理工作。
    #[inline(always)]
    pub fn release_and_recheck(mut self) -> bool {
        let pending = self.execution.release(self.generation);
        self.released = true;
        pending
    }
}

impl Drop for FlowExecLease<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        if !self.released {
            let _ = self.execution.release(self.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn lease_rejects_competing_executor_and_releases_on_drop() {
        let execution = FlowExecution::new();
        assert!(execution.install_generation(7));
        let lease = execution
            .try_acquire(7, FlowExecutorKind::Worker, 0)
            .expect("首个执行者应取得租约");
        assert!(
            execution
                .try_acquire(7, FlowExecutorKind::Syscall, 1)
                .is_none()
        );
        assert_eq!(
            execution.snapshot(),
            FlowExecutionSnapshot {
                generation: 7,
                executor_kind: Some(FlowExecutorKind::Worker),
                executor_cpu: Some(0),
                busy: true,
                pending: false,
            }
        );
        drop(lease);
        assert!(
            execution
                .try_acquire(7, FlowExecutorKind::Syscall, 1)
                .is_some()
        );
    }

    #[test]
    fn pending_is_consumed_on_acquire_and_rechecked_on_release() {
        let execution = FlowExecution::new();
        assert!(execution.install_generation(11));
        execution.mark_pending();
        let lease = execution
            .try_acquire(11, FlowExecutorKind::Worker, 0)
            .expect("pending 状态不应阻止执行者取得租约");
        assert!(!execution.has_pending());
        execution.mark_pending();
        assert!(lease.release_and_recheck());
        assert!(execution.has_pending());
        drop(
            execution
                .try_acquire(11, FlowExecutorKind::Worker, 0)
                .expect("下一轮应消费 pending"),
        );
        assert!(!execution.has_pending());
    }

    #[test]
    fn generation_switch_rejects_stale_executor() {
        let execution = FlowExecution::new();
        assert!(execution.install_generation(3));
        let lease = execution
            .try_acquire(3, FlowExecutorKind::Worker, 0)
            .expect("当前 generation 应可执行");
        assert!(!execution.install_generation(4));
        drop(lease);
        assert!(execution.install_generation(4));
        assert!(
            execution
                .try_acquire(3, FlowExecutorKind::Worker, 0)
                .is_none()
        );
        assert!(
            execution
                .try_acquire(4, FlowExecutorKind::Recovery, 2)
                .is_some()
        );
    }

    #[test]
    fn concurrent_executors_never_hold_the_same_generation() {
        let execution = Arc::new(FlowExecution::new());
        assert!(execution.install_generation(17));
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = std::vec::Vec::new();
        for cpu in 0..2 {
            let execution = Arc::clone(&execution);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                let lease = execution.try_acquire(17, FlowExecutorKind::Syscall, cpu);
                let acquired = lease.is_some();
                barrier.wait();
                drop(lease);
                acquired
            }));
        }
        barrier.wait();
        barrier.wait();
        let acquired = workers
            .into_iter()
            .map(|worker| worker.join().expect("并发租约测试线程不得失败"))
            .filter(|acquired| *acquired)
            .count();
        assert_eq!(acquired, 1);
        assert!(!execution.snapshot().busy);
    }

    #[test]
    fn fault_unwind_releases_execution_lease() {
        let execution = FlowExecution::new();
        assert!(execution.install_generation(23));
        let fault = std::panic::catch_unwind(|| {
            let _lease = execution
                .try_acquire(23, FlowExecutorKind::Syscall, 0)
                .expect("故障前应取得租约");
            panic!("模拟同步调用故障");
        });
        assert!(fault.is_err());
        assert!(!execution.snapshot().busy);
        assert!(
            execution
                .try_acquire(23, FlowExecutorKind::Recovery, 0)
                .is_some()
        );
    }

    #[test]
    fn generation_replacement_waits_for_close_holder_to_release() {
        let execution = FlowExecution::new();
        assert!(execution.install_generation(29));
        let lease = execution
            .try_acquire(29, FlowExecutorKind::Worker, 0)
            .expect("close 路径应取得当前代租约");
        assert!(!execution.install_generation(30));
        drop(lease);
        assert!(execution.install_generation(30));
        assert!(
            execution
                .try_acquire(29, FlowExecutorKind::Recovery, 0)
                .is_none()
        );
    }
}
