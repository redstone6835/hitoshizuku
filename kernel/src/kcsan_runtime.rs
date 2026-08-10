//! KCSAN 调试运行时的内核接入。
//!
//! 检测器在调度器与全部 AP 就绪后安装，因此访问 hook 可以安全读取
//! per-CPU 状态和稳定的任务标识。随后派生低权重报告线程，周期性
//! 排空固定大小的报告 ring。

use core::sync::atomic::{AtomicBool, Ordering};

const REPORT_INTERVAL_NS: u64 = 100_000_000;
static REPORTER_STARTED: AtomicBool = AtomicBool::new(false);

/// 在调度器和全部 AP 初始化后安装 KCSAN 平台回调。
pub fn install() {
    let installed = kcsan::install(
        kcsan::RuntimeHooks {
            current_task: sched::current_task_id,
            timestamp: hal::time::stable_counter_raw,
        },
        kcsan::Config::default(),
    );
    let _disabled = kcsan::disable();
    if installed {
        log::info!("[kcsan] detector enabled after SMP startup");
    } else {
        log::warning!("[kcsan] detector install was requested more than once");
    }
}

/// 在所有 AP 启动后派生低优先级报告线程。
pub fn start_reporter() {
    if REPORTER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let reporter = sched::kthread_spawn(
        report_worker,
        0,
        sched::SchedParams {
            nice: 19,
            slice_ns: 0,
        },
    );
    if reporter.state() == sched::TaskState::Dead {
        kcsan::set_enabled(false);
        REPORTER_STARTED.store(false, Ordering::Release);
        let _disabled = kcsan::disable();
        log::warning!("[kcsan] reporter spawn failed; detector disabled");
    }
}

unsafe extern "C" fn report_worker(_arg: usize) -> ! {
    {
        let _disabled = kcsan::disable();
        log::info!("[kcsan] reporter online");
    }
    let initial = kcsan::report_window();
    let mut next_sequence = initial.first_sequence;
    let mut dropped_reports = kcsan::stats().dropped_reports;
    if initial.overwritten != 0 {
        log_overwrite(initial.overwritten, initial.overwritten);
    }

    loop {
        sleep_report_interval();
        let window = kcsan::report_window();
        if next_sequence < window.first_sequence {
            let lost = window.first_sequence.saturating_sub(next_sequence);
            log_overwrite(lost, window.overwritten);
            next_sequence = window.first_sequence;
        }

        while next_sequence < window.next_sequence {
            let Some(report) = kcsan::report(next_sequence) else {
                // 读取窗口可能在这里被新报告覆盖，下一轮会依据新的
                // first_sequence 统计丢失量并推进游标。
                break;
            };
            log_report(report);
            next_sequence = next_sequence.saturating_add(1);
        }

        let current_dropped = kcsan::stats().dropped_reports;
        if current_dropped != dropped_reports {
            log_drop(
                current_dropped.saturating_sub(dropped_reports),
                current_dropped,
            );
            dropped_reports = current_dropped;
        }
    }
}

fn log_report(report: kcsan::Report) {
    let first = report.first;
    let second = report.second;
    let overlap = first.address.max(second.address);
    let _disabled = kcsan::disable();
    log::error!(
        "[kcsan] data race seq={} address={:#x} first(kind={} size={} cpu={} task={} pc={:#x}) second(kind={} size={} cpu={} task={} pc={:#x})",
        report.sequence,
        overlap,
        first.kind.name(),
        first.size,
        first.cpu,
        first.task,
        first.pc,
        second.kind.name(),
        second.size,
        second.cpu,
        second.task,
        second.pc,
    );
}

fn log_overwrite(lost: u64, total: u64) {
    let _disabled = kcsan::disable();
    log::warning!(
        "[kcsan] report ring overwritten: lost={} total_overwritten={}",
        lost,
        total,
    );
}

fn log_drop(lost: u64, total: u64) {
    let _disabled = kcsan::disable();
    log::warning!(
        "[kcsan] report publication busy: dropped={} total_dropped={}",
        lost,
        total,
    );
}

fn sleep_report_interval() {
    let task = sched::current_task_direct();
    let now = sched::now_ns_direct();
    let deadline = now.saturating_add(REPORT_INTERVAL_NS);

    if !task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping)
        && !task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping)
    {
        let _ = sched::operation::sched_yield();
        return;
    }
    if !sched::register_sleep_deadline(&task, deadline) {
        restore_running_state(&task);
        let _ = sched::operation::sched_yield();
        return;
    }

    sched::schedule_once(now);
    sched::cancel_sleep_deadline(&task);
    restore_running_state(&task);
}

fn restore_running_state(task: &alloc::sync::Arc<sched::Task>) {
    if !task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running) {
        let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
    }
}
