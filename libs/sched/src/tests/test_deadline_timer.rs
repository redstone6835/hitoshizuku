//! 软件截止时间与架构定时器重编程测试。

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ktest::ktest;

use super::test_thread_metadata::make_task;
use crate::ArchDeadlineTimerOps;
use crate::scheduler::{
    cancel_sleep_deadline, earliest_deadline_for_test, register_sleep_deadline,
    register_sleep_deadline_for_test, reprogram_current_deadline,
    reset_timer_event_scan_counts_for_test, service_expired_timer_events_for_test,
    set_realtime_itimer, take_expired_sleepers_for_test, timer_event_scan_counts_for_test,
    timer_fired_for_test,
};
use crate::sync::Spinlock;

const NO_DEADLINE: u64 = u64::MAX;
static LAST_DEADLINE: AtomicU64 = AtomicU64::new(NO_DEADLINE);
static REPROGRAM_COUNT: AtomicUsize = AtomicUsize::new(0);
/// 这些测试共享全局软件定时器表和唯一的架构定时器钩子，必须串行执行。
static DEADLINE_TEST_LOCK: Spinlock<()> = Spinlock::new(());

fn record_deadline(deadline_ns: Option<u64>) {
    LAST_DEADLINE.store(deadline_ns.unwrap_or(NO_DEADLINE), Ordering::Release);
    REPROGRAM_COUNT.fetch_add(1, Ordering::Relaxed);
}

static TEST_DEADLINE_TIMER_OPS: ArchDeadlineTimerOps = ArchDeadlineTimerOps {
    reprogram: record_deadline,
};

#[ktest]
fn deadline_timer_tracks_earliest_source_and_cancellation() {
    let _test_guard = DEADLINE_TEST_LOCK.lock();
    crate::arch_hooks::register_deadline_timer(&TEST_DEADLINE_TIMER_OPS);
    LAST_DEADLINE.store(NO_DEADLINE, Ordering::Release);
    REPROGRAM_COUNT.store(0, Ordering::Release);
    let first = make_task();
    let second = make_task();

    assert!(register_sleep_deadline(&first, 300));
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 300);
    assert_eq!(REPROGRAM_COUNT.load(Ordering::Acquire), 1);

    reprogram_current_deadline(None);
    assert_eq!(REPROGRAM_COUNT.load(Ordering::Acquire), 1);

    timer_fired_for_test();
    reprogram_current_deadline(None);
    assert_eq!(REPROGRAM_COUNT.load(Ordering::Acquire), 2);

    reprogram_current_deadline(Some(250));
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 250);
    reprogram_current_deadline(Some(350));
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 300);
    reprogram_current_deadline(None);
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 300);

    assert!(register_sleep_deadline_for_test(&second, 200, 1));
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 300);

    // CPU 0 的本地 timer 只能看见归属于 CPU 0 的等待，不能被 CPU 1 的
    // 更早 deadline 拉成跨核定时器惊群。
    assert_eq!(earliest_deadline_for_test(0), Some(300));
    assert_eq!(earliest_deadline_for_test(1), Some(200));

    // 同一任务重复登记只允许收紧截止时间，不能意外推迟已经 armed 的等待。
    assert!(register_sleep_deadline_for_test(&second, 400, 1));
    assert_eq!(earliest_deadline_for_test(1), Some(200));

    cancel_sleep_deadline(&second);
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 300);

    let _ = set_realtime_itimer(&second, 100, 0);
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 100);

    let _ = set_realtime_itimer(&second, 0, 0);
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), 300);

    cancel_sleep_deadline(&first);
    assert_eq!(LAST_DEADLINE.load(Ordering::Acquire), NO_DEADLINE);
}

#[ktest]
fn expired_sleepers_are_taken_in_one_cpu_local_batch() {
    let _test_guard = DEADLINE_TEST_LOCK.lock();
    let first = make_task();
    let second = make_task();
    let future = make_task();
    let remote = make_task();

    assert!(register_sleep_deadline_for_test(&first, 100, 0));
    assert!(register_sleep_deadline_for_test(&second, 120, 0));
    assert!(register_sleep_deadline_for_test(&future, 300, 0));
    assert!(register_sleep_deadline_for_test(&remote, 80, 1));

    let expired = take_expired_sleepers_for_test(150, 0);
    assert_eq!(expired.len(), 2);
    assert!(
        expired
            .iter()
            .any(|task| alloc::sync::Arc::ptr_eq(task, &first))
    );
    assert!(
        expired
            .iter()
            .any(|task| alloc::sync::Arc::ptr_eq(task, &second))
    );
    assert_eq!(earliest_deadline_for_test(0), Some(300));
    assert_eq!(earliest_deadline_for_test(1), Some(80));

    cancel_sleep_deadline(&future);
    cancel_sleep_deadline(&remote);
}

#[ktest]
fn future_deadline_skips_event_table_scans_until_expiry() {
    let _test_guard = DEADLINE_TEST_LOCK.lock();
    let sleeper = make_task();

    assert!(register_sleep_deadline_for_test(&sleeper, 300, 0));
    reset_timer_event_scan_counts_for_test();

    assert!(!service_expired_timer_events_for_test(100, 0));
    assert!(!service_expired_timer_events_for_test(299, 0));
    assert_eq!(timer_event_scan_counts_for_test(), (0, 0));
    assert_eq!(earliest_deadline_for_test(0), Some(300));

    assert!(!service_expired_timer_events_for_test(300, 0));
    assert_eq!(timer_event_scan_counts_for_test(), (1, 0));
    assert_eq!(earliest_deadline_for_test(0), None);

    let _ = set_realtime_itimer(&sleeper, 200, 0);
    reset_timer_event_scan_counts_for_test();

    assert!(!service_expired_timer_events_for_test(199, 0));
    assert_eq!(timer_event_scan_counts_for_test(), (0, 0));

    let _ = set_realtime_itimer(&sleeper, 0, 0);
}
