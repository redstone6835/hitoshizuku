//! ELM 后台执行器。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static PROVIDER_WORKER_STARTED: [AtomicBool; sched::NR_CPUS] =
    [const { AtomicBool::new(false) }; sched::NR_CPUS];
static PROVIDER_WORKER_BUSY: [AtomicBool; sched::NR_CPUS] =
    [const { AtomicBool::new(false) }; sched::NR_CPUS];
static PROVIDER_WORKER_COMPLETED: [AtomicU64; sched::NR_CPUS] =
    [const { AtomicU64::new(0) }; sched::NR_CPUS];
static PROVIDER_WORKER_WAITS: [AtomicU64; sched::NR_CPUS] =
    [const { AtomicU64::new(0) }; sched::NR_CPUS];
static PROVIDER_WORKER_WAKEUPS: AtomicU64 = AtomicU64::new(0);
static PROVIDER_WORK_QUEUE: sched::WaitQueue = sched::WaitQueue::new();

/// 单次连续处理的异步工作上限。
///
/// provider worker 是内核线程，内核态 timer trap 只记录延迟 tick，不会在任意
/// 自旋锁临界区强行切换。因此即使工作队列持续非空，也必须主动回到调度器，
/// 否则低优先级 worker 仍可能长期占住一个 CPU。
const PROVIDER_WORK_BUDGET: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProviderWorkerSnapshot {
    pub online_mask: u64,
    pub active_mask: u64,
    pub worker_mask: u64,
    pub busy_mask: u64,
    pub wakeups: u64,
    pub completed: [u64; sched::NR_CPUS],
    pub waits: [u64; sched::NR_CPUS],
}

pub(crate) const fn missing_provider_worker_mask(active_mask: u64, worker_mask: u64) -> u64 {
    let supported = if sched::NR_CPUS >= u64::BITS as usize {
        u64::MAX
    } else {
        (1u64 << sched::NR_CPUS) - 1
    };
    active_mask & supported & !worker_mask
}

fn worker_mask() -> u64 {
    let mut mask = 0u64;
    let mut cpu_id = 0usize;
    while cpu_id < sched::NR_CPUS {
        if PROVIDER_WORKER_STARTED[cpu_id].load(Ordering::Acquire) {
            mask |= 1u64 << cpu_id;
        }
        cpu_id += 1;
    }
    mask
}

fn busy_mask() -> u64 {
    let mut mask = 0u64;
    let mut cpu_id = 0usize;
    while cpu_id < sched::NR_CPUS {
        if PROVIDER_WORKER_BUSY[cpu_id].load(Ordering::Acquire) {
            mask |= 1u64 << cpu_id;
        }
        cpu_id += 1;
    }
    mask
}

pub(crate) fn snapshot() -> ProviderWorkerSnapshot {
    let mut completed = [0u64; sched::NR_CPUS];
    let mut waits = [0u64; sched::NR_CPUS];
    for cpu_id in 0..sched::NR_CPUS {
        completed[cpu_id] = PROVIDER_WORKER_COMPLETED[cpu_id].load(Ordering::Acquire);
        waits[cpu_id] = PROVIDER_WORKER_WAITS[cpu_id].load(Ordering::Acquire);
    }
    ProviderWorkerSnapshot {
        online_mask: sched::online_cpu_mask(),
        active_mask: sched::active_cpu_mask(),
        worker_mask: worker_mask(),
        busy_mask: busy_mask(),
        wakeups: PROVIDER_WORKER_WAKEUPS.load(Ordering::Acquire),
        completed,
        waits,
    }
}

/// 为尚未配置执行器的 active CPU 创建固定后台线程。
///
/// 函数可在启动 CPU 初始化和 AP 全部激活后重复调用。每个 CPU 的原子预留保证
/// 并发调用不会派生重复 worker。
pub(crate) fn reconcile_provider_workers() -> usize {
    let active_mask = sched::active_cpu_mask();
    let mut missing = missing_provider_worker_mask(active_mask, worker_mask());
    let mut started = 0usize;
    while missing != 0 {
        let cpu_id = missing.trailing_zeros() as usize;
        missing &= !(1u64 << cpu_id);
        if PROVIDER_WORKER_STARTED[cpu_id]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        match sched::kthread_spawn_on_cpu(
            provider_worker,
            cpu_id,
            sched::SchedParams {
                nice: 19,
                slice_ns: 0,
            },
            cpu_id,
        ) {
            Ok(_) => {
                started += 1;
            }
            Err(error) => {
                PROVIDER_WORKER_STARTED[cpu_id].store(false, Ordering::Release);
                log::error!(
                    "[elm][executor] 无法在 CPU {} 启动 provider worker: {:?}",
                    cpu_id,
                    error
                );
            }
        }
    }
    started
}

pub(crate) fn wake_provider_worker() {
    PROVIDER_WORKER_WAKEUPS.fetch_add(1, Ordering::Relaxed);
    let _ = PROVIDER_WORK_QUEUE.wake_one_default();
}

unsafe extern "C" fn provider_worker(cpu_id: usize) -> ! {
    log::info!("[elm][executor] provider worker ready on CPU {}", cpu_id);
    loop {
        let mut budget = 0usize;
        loop {
            PROVIDER_WORKER_BUSY[cpu_id].store(true, Ordering::Release);
            let handled = super::core::run_one_async_provider_job_unlocked(sched::now_ns_direct());
            PROVIDER_WORKER_BUSY[cpu_id].store(false, Ordering::Release);
            if !handled {
                break;
            }
            PROVIDER_WORKER_COMPLETED[cpu_id].fetch_add(1, Ordering::Relaxed);
            budget += 1;
            if budget >= PROVIDER_WORK_BUDGET {
                // 内核线程不会走用户态 trap 返回的抢占收尾；有界主动调度既
                // 响应 need_resched，也让同一 CPU 上的用户任务及时获得机会。
                budget = 0;
                sched::schedule_once(sched::now_ns_direct());
            }
        }

        let current = sched::current_task_direct();
        PROVIDER_WORKER_WAITS[cpu_id].fetch_add(1, Ordering::Relaxed);
        PROVIDER_WORK_QUEUE.wait_event(&current, || {
            super::core::with_core(|core| core.has_provider_async_work())
        });
    }
}
