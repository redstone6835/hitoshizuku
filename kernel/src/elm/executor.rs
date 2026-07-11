//! ELM 后台执行器。

use core::sync::atomic::{AtomicBool, Ordering};

static PROVIDER_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static PROVIDER_WORK_QUEUE: sched::WaitQueue = sched::WaitQueue::new();

pub(crate) fn start_provider_worker() {
    if PROVIDER_WORKER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = sched::kthread_spawn(
        provider_worker,
        0,
        sched::SchedParams {
            nice: 19,
            slice_ns: 0,
        },
    );
}

pub(crate) fn wake_provider_worker() {
    let _ = PROVIDER_WORK_QUEUE.wake_one_default();
}

unsafe extern "C" fn provider_worker(_arg: usize) -> ! {
    loop {
        while super::core::run_one_async_provider_job_unlocked(sched::now_ns_public()) {}

        let current = sched::current_task();
        PROVIDER_WORK_QUEUE.wait_event(&current, || {
            super::core::with_core(|core| core.has_provider_async_work())
        });
    }
}
