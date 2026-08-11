use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ktest::ktest;

static MEMBARRIER_START: AtomicBool = AtomicBool::new(false);
static MEMBARRIER_REMOTE_STATE: AtomicUsize = AtomicUsize::new(0);

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
    if sched::active_cpu_mask().count_ones() < 2 {
        return;
    }

    MEMBARRIER_START.store(false, Ordering::Release);
    MEMBARRIER_REMOTE_STATE.store(0, Ordering::Release);
    let _worker = sched::kthread_spawn_on_cpu(
        concurrent_membarrier_worker,
        0,
        sched::SchedParams {
            nice: 0,
            slice_ns: 0,
        },
        1,
    )
    .expect("无法在 CPU1 启动 membarrier 测试线程");

    let ready_deadline = sched::now_ns_direct().saturating_add(2_000_000_000);
    while MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire) == 0
        && sched::now_ns_direct() < ready_deadline
    {
        core::hint::spin_loop();
    }
    assert_eq!(MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire), 1);

    MEMBARRIER_START.store(true, Ordering::Release);
    sched::synchronize_cpus().expect("CPU0 membarrier rendezvous 失败");

    let completion_deadline = sched::now_ns_direct().saturating_add(2_000_000_000);
    while MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire) == 1
        && sched::now_ns_direct() < completion_deadline
    {
        // CPU1 可能在本地中断关闭时发布反向请求；等待期间继续服务本 CPU，
        // 与实际 syscall rendezvous 的主动进展规则保持一致。
        sched::handle_membarrier_ipi();
        core::hint::spin_loop();
    }
    assert_eq!(MEMBARRIER_REMOTE_STATE.load(Ordering::Acquire), 2);
}
