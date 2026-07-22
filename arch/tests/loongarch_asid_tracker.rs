#[path = "../src/loongarch64/asid_tracker.rs"]
mod asid_tracker;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Barrier};
use std::thread;

use asid_tracker::{CurrentAsidTracker, KERNEL_LOGICAL_ASID};

const TARGET_ASID: usize = 41;

#[test]
fn target_mask_keeps_only_current_matching_logical_asids() {
    let tracker = CurrentAsidTracker::<5>::new();
    let historical = AtomicUsize::new(0b1_1111);

    tracker.publish_before_full_flush(0, TARGET_ASID);
    tracker.publish_before_full_flush(1, KERNEL_LOGICAL_ASID);
    tracker.publish_before_full_flush(2, TARGET_ASID + 1);
    tracker.publish_before_full_flush(3, TARGET_ASID);
    // 软件逻辑 ASID 不按 10 位硬件字段截断，避免把复用同一硬件 tag 的其它
    // 地址空间误当成本地址空间。
    tracker.publish_before_full_flush(4, TARGET_ASID + (1 << 10));

    assert_eq!(tracker.current(0), Some(TARGET_ASID));
    assert_eq!(tracker.current(4), Some(TARGET_ASID + (1 << 10)));
    assert_eq!(tracker.current(5), None);

    assert_eq!(
        tracker.target_mask_after_pte_update(&historical, TARGET_ASID),
        0b0_1001
    );

    historical.store(0b0_1000, Ordering::SeqCst);
    assert_eq!(
        tracker.target_mask_after_pte_update(&historical, TARGET_ASID),
        0b0_1000
    );
}

#[test]
fn publishing_kernel_asid_removes_cpu_from_user_targets() {
    let tracker = CurrentAsidTracker::<1>::new();
    let historical = AtomicUsize::new(1);

    tracker.publish_before_full_flush(0, TARGET_ASID);
    assert_eq!(
        tracker.target_mask_after_pte_update(&historical, TARGET_ASID),
        1
    );

    tracker.publish_before_full_flush(0, KERNEL_LOGICAL_ASID);
    assert_eq!(
        tracker.target_mask_after_pte_update(&historical, TARGET_ASID),
        0
    );
}

#[test]
fn concurrent_switch_or_scan_always_covers_the_pte_update() {
    for _ in 0..128 {
        let tracker = Arc::new(CurrentAsidTracker::<1>::new());
        let historical = Arc::new(AtomicUsize::new(1));
        let pte_published = Arc::new(AtomicBool::new(false));
        let start = Arc::new(Barrier::new(3));

        let activate = {
            let tracker = Arc::clone(&tracker);
            let pte_published = Arc::clone(&pte_published);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                tracker.publish_before_full_flush(0, TARGET_ASID);
                // 模拟 activate_with_asid_roots 开头的 dbar：若扫描漏掉本 CPU，
                // 随后的完整 invtlb 必须位于 PTE 发布之后。
                fence(Ordering::SeqCst);
                pte_published.load(Ordering::SeqCst)
            })
        };

        let invalidate = {
            let tracker = Arc::clone(&tracker);
            let historical = Arc::clone(&historical);
            let pte_published = Arc::clone(&pte_published);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                pte_published.store(true, Ordering::SeqCst);
                tracker.target_mask_after_pte_update(&historical, TARGET_ASID) & 1 != 0
            })
        };

        start.wait();
        let activation_flush_covers_update = activate.join().expect("activate worker");
        let shootdown_targets_cpu = invalidate.join().expect("invalidate worker");
        assert!(activation_flush_covers_update || shootdown_targets_cpu);
    }
}
