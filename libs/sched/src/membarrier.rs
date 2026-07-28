//! SMP 内存屏障 rendezvous。
//!
//! 发起方先发布目标 CPU 的请求序号，再通过架构 IPI 通知目标。目标 CPU 在
//! IPI handler 中执行全内存屏障，最后发布完成序号。请求和完成使用逐 CPU
//! 单调序号，因此多个并发请求可以由一次远端屏障合并完成。

use core::sync::atomic::{AtomicUsize, Ordering, fence};

use errno::Errno;

use crate::arch_hooks;
use crate::cpu::MAX_CPUS;

struct CpuMembarrier {
    requested: [AtomicUsize; MAX_CPUS],
    completed: [AtomicUsize; MAX_CPUS],
}

impl CpuMembarrier {
    const fn new() -> Self {
        Self {
            requested: [const { AtomicUsize::new(0) }; MAX_CPUS],
            completed: [const { AtomicUsize::new(0) }; MAX_CPUS],
        }
    }

    fn service_cpu(&self, cpu_id: usize) {
        let Some(requested) = self.requested.get(cpu_id) else {
            return;
        };
        let Some(completed) = self.completed.get(cpu_id) else {
            return;
        };
        let target = requested.load(Ordering::Acquire);
        if sequence_reached(completed.load(Ordering::Relaxed), target) {
            return;
        }

        fence(Ordering::SeqCst);
        completed.store(target, Ordering::Release);
    }

    fn pending(&self, cpu_id: usize) -> bool {
        let Some(requested) = self.requested.get(cpu_id) else {
            return false;
        };
        let Some(completed) = self.completed.get(cpu_id) else {
            return false;
        };
        !sequence_reached(
            completed.load(Ordering::Relaxed),
            requested.load(Ordering::Acquire),
        )
    }

    fn synchronize_with(
        &self,
        source_cpu: usize,
        targets: usize,
        mut is_online: impl FnMut(usize) -> bool,
        mut send_ipi: impl FnMut(usize) -> bool,
    ) -> Result<(), Errno> {
        fence(Ordering::SeqCst);
        if targets == 0 {
            return Ok(());
        }

        let mut expected = [0usize; MAX_CPUS];
        for cpu_id in 0..MAX_CPUS {
            if targets & cpu_bit(cpu_id) == 0 {
                continue;
            }
            if !is_online(cpu_id) {
                return Err(Errno::EIO);
            }
            expected[cpu_id] = self.requested[cpu_id]
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1);
        }

        for cpu_id in 0..MAX_CPUS {
            if targets & cpu_bit(cpu_id) != 0 && !send_ipi(cpu_id) {
                return Err(Errno::EIO);
            }
        }

        for cpu_id in 0..MAX_CPUS {
            if targets & cpu_bit(cpu_id) == 0 {
                continue;
            }
            while !sequence_reached(
                self.completed[cpu_id].load(Ordering::Acquire),
                expected[cpu_id],
            ) {
                // syscall/trap 路径可能保持本地中断关闭。并发 rendezvous 时主动
                // 服务发给本 CPU 的请求，避免两个发起方相互等待 IPI handler。
                self.service_cpu(source_cpu);
                core::hint::spin_loop();
            }
        }

        // 远端发布 completed 之前已经发布了它自己的反向请求；在返回前再服务
        // 一次本 CPU，可覆盖“最后一次循环检查后才到达”的并发请求。
        self.service_cpu(source_cpu);
        fence(Ordering::SeqCst);
        Ok(())
    }
}

const fn cpu_bit(cpu_id: usize) -> usize {
    if cpu_id < usize::BITS as usize {
        1usize << cpu_id
    } else {
        0
    }
}

const fn sequence_reached(completed: usize, expected: usize) -> bool {
    completed.wrapping_sub(expected) <= usize::MAX / 2
}

static CPU_MEMBARRIER: CpuMembarrier = CpuMembarrier::new();

/// 判断指定 CPU 是否有尚未确认的 membarrier 请求。
pub fn pending_on(cpu_id: usize) -> bool {
    CPU_MEMBARRIER.pending(cpu_id)
}

/// 处理指定 CPU 的请求；架构已知逻辑 CPU ID 时使用该入口，避免重复查询。
pub fn handle_ipi_on(cpu_id: usize) {
    CPU_MEMBARRIER.service_cpu(cpu_id);
}

/// 让当前所有 active CPU 执行一次全内存屏障，并等待远端完成。
pub fn synchronize_cpus() -> Result<(), Errno> {
    let source_cpu = crate::scheduler::current_cpu_id();
    let source_bit = cpu_bit(source_cpu) as u64;
    let targets = (crate::scheduler::active_cpu_mask() & !source_bit) as usize;
    if targets == 0 {
        fence(Ordering::SeqCst);
        return Ok(());
    }
    let ops = arch_hooks::cpu_control().ok_or(Errno::EOPNOTSUPP)?;
    CPU_MEMBARRIER.synchronize_with(source_cpu, targets, ops.is_online, ops.send_membarrier)
}

/// 架构 IPI handler 调用：处理当前 CPU 尚未确认的 membarrier 请求。
pub fn handle_ipi() {
    handle_ipi_on(crate::scheduler::current_cpu_id());
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    #[test]
    fn one_remote_barrier_can_complete_coalesced_requests() {
        let state = CpuMembarrier::new();
        let first = state.requested[1]
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let second = state.requested[1]
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);

        state.service_cpu(1);

        let completed = state.completed[1].load(Ordering::Acquire);
        assert!(sequence_reached(completed, first));
        assert!(sequence_reached(completed, second));
    }

    #[test]
    fn pending_state_clears_after_service() {
        let state = CpuMembarrier::new();
        state.requested[2].store(1, Ordering::Release);
        assert!(state.pending(2));
        state.service_cpu(2);
        assert!(!state.pending(2));
    }

    #[test]
    fn simultaneous_callers_service_each_other_without_interrupts() {
        let state = Arc::new(CpuMembarrier::new());
        let start = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();

        for cpu_id in 0..2 {
            let state = Arc::clone(&state);
            let start = Arc::clone(&start);
            workers.push(thread::spawn(move || {
                start.wait();
                state.synchronize_with(cpu_id, cpu_bit(cpu_id ^ 1), |_| true, |_| true)
            }));
        }

        for worker in workers {
            worker.join().expect("membarrier worker").unwrap();
        }
    }

    #[test]
    fn offline_target_is_rejected_before_ipi_send() {
        let state = CpuMembarrier::new();
        let mut sent = false;
        let result = state.synchronize_with(
            0,
            cpu_bit(1),
            |_| false,
            |_| {
                sent = true;
                true
            },
        );

        assert_eq!(result, Err(Errno::EIO));
        assert!(!sent);
    }
}
