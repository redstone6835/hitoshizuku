use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ktest::ktest;

static MEMBARRIER_START: AtomicBool = AtomicBool::new(false);
static MEMBARRIER_REMOTE_STATE: AtomicUsize = AtomicUsize::new(0);

struct TestAffinityRestore {
    task: alloc::sync::Arc<sched::Task>,
    previous: u64,
}

impl Drop for TestAffinityRestore {
    fn drop(&mut self) {
        self.task.set_cpu_affinity(self.previous);
    }
}

unsafe extern "C" fn concurrent_membarrier_worker(_arg: usize) -> ! {
    MEMBARRIER_REMOTE_STATE.store(1, Ordering::Release);
    while !MEMBARRIER_START.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    let completed = sched::synchronize_cpus().is_ok();
    MEMBARRIER_REMOTE_STATE.store(if completed { 2 } else { 3 }, Ordering::Release);
    sched::kthread_finish(sched::ExitCode(0));
}

#[ktest]
fn concurrent_membarrier_rendezvous_completes_on_smp() {
    let active_cpus = sched::active_cpu_mask();
    if active_cpus.count_ones() < 2 {
        return;
    }
    let current_cpu = sched::current_cpu_id();
    let target_cpu = (0..u64::BITS as usize)
        .find(|cpu| *cpu != current_cpu && active_cpus & (1u64 << cpu) != 0)
        .expect("在线 CPU 集合应包含当前 CPU 之外的处理器");
    let task = sched::current_task_direct();
    let previous = task.cpu_affinity();
    task.set_cpu_affinity(1u64 << current_cpu);
    let _affinity = TestAffinityRestore { task, previous };

    MEMBARRIER_START.store(false, Ordering::Release);
    MEMBARRIER_REMOTE_STATE.store(0, Ordering::Release);
    let _worker = sched::kthread_spawn_on_cpu(
        concurrent_membarrier_worker,
        0,
        sched::SchedParams {
            nice: 0,
            slice_ns: 0,
        },
        target_cpu,
    )
    .expect("无法在辅助 CPU 启动 membarrier 测试线程");

    let ready_deadline = sched::now_ns_direct().saturating_add(2_000_000_000);
    while MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire) == 0
        && sched::now_ns_direct() < ready_deadline
    {
        let _ = sched::operation::sched_yield();
    }
    assert_eq!(MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire), 1);

    MEMBARRIER_START.store(true, Ordering::Release);
    sched::synchronize_cpus().expect("CPU0 membarrier rendezvous 失败");

    let completion_deadline = sched::now_ns_direct().saturating_add(2_000_000_000);
    while MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire) == 1
        && sched::now_ns_direct() < completion_deadline
    {
        // 辅助 CPU 可能在本地中断关闭时发布反向请求；等待期间继续服务本 CPU，
        // 与实际 syscall rendezvous 的主动进展规则保持一致。
        sched::handle_membarrier_ipi();
        let _ = sched::operation::sched_yield();
    }
    assert_eq!(MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire), 2);
}
