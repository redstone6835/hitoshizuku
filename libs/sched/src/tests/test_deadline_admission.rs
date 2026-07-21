//! SCHED_DEADLINE 独立带宽预留测试。

extern crate alloc;
extern crate std;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ktest::ktest;

use super::std::sync::Barrier;
use super::std::thread;
use super::test_thread_metadata::make_task;
use crate::deadline_admission::DeadlineAdmission;
use crate::{CpuId, SCHED_CAPACITY_SCALE, SchedAttr, TaskState};

fn cpu(id: usize) -> CpuId {
    CpuId::new(id).expect("有效 CPU")
}

fn deadline(parts: u64) -> SchedAttr {
    SchedAttr::deadline(parts, parts, 1_000).normalized()
}

#[ktest]
fn deadline_admission_rejects_single_cpu_overcommit() {
    let admission = DeadlineAdmission::new();
    let first = make_task();
    let second = make_task();
    let first_attr = deadline(600);
    let second_attr = deadline(500);

    admission
        .update_attr(&first, cpu(0), first_attr, SCHED_CAPACITY_SCALE, || {
            first.sched.set_sched_attr(first_attr);
            true
        })
        .expect("首个任务应准入");
    assert!(
        admission
            .update_attr(&second, cpu(0), second_attr, SCHED_CAPACITY_SCALE, || {
                second.sched.set_sched_attr(second_attr);
                true
            },)
            .is_err()
    );
    assert_eq!(admission.reserved(cpu(0)), 614);
}

#[ktest]
fn sleeping_deadline_task_keeps_its_reservation() {
    let admission = DeadlineAdmission::new();
    let task = make_task();
    let attr = deadline(500);
    admission
        .update_attr(&task, cpu(0), attr, SCHED_CAPACITY_SCALE, || {
            task.sched.set_sched_attr(attr);
            true
        })
        .expect("Deadline 任务应准入");

    task.set_state(TaskState::Sleeping);
    assert_eq!(admission.reserved(cpu(0)), 512);
    assert_eq!(admission.tasks_on_cpu(cpu(0)).len(), 1);
}

#[ktest]
fn shrinking_and_leaving_deadline_release_capacity() {
    let admission = DeadlineAdmission::new();
    let task = make_task();
    let large = deadline(750);
    let small = deadline(250);
    admission
        .update_attr(&task, cpu(0), large, SCHED_CAPACITY_SCALE, || {
            task.sched.set_sched_attr(large);
            true
        })
        .expect("大预留应准入");
    admission
        .update_attr(&task, cpu(0), small, SCHED_CAPACITY_SCALE, || {
            task.sched.set_sched_attr(small);
            true
        })
        .expect("缩小预留应成功");
    assert_eq!(admission.reserved(cpu(0)), 256);

    let fair = SchedAttr::fair(0, 0).normalized();
    admission
        .update_attr(&task, cpu(0), fair, SCHED_CAPACITY_SCALE, || {
            task.sched.set_sched_attr(fair);
            true
        })
        .expect("退出 Deadline 应成功");
    assert_eq!(admission.reserved(cpu(0)), 0);
    assert_eq!(admission.reservation_of(&task), None);
}

#[ktest]
fn failed_migration_keeps_source_reservation() {
    let admission = DeadlineAdmission::new();
    let source = make_task();
    let blocker = make_task();
    let source_attr = deadline(500);
    let blocker_attr = deadline(600);
    for (task, target, attr) in [
        (&source, cpu(0), source_attr),
        (&blocker, cpu(1), blocker_attr),
    ] {
        admission
            .update_attr(task, target, attr, SCHED_CAPACITY_SCALE, || {
                task.sched.set_sched_attr(attr);
                true
            })
            .expect("初始预留应成功");
    }

    let applied = AtomicBool::new(false);
    assert!(
        admission
            .migrate(&source, cpu(0), cpu(1), SCHED_CAPACITY_SCALE, || {
                applied.store(true, Ordering::Release);
                Ok(())
            },)
            .is_err()
    );
    assert!(!applied.load(Ordering::Acquire));
    assert_eq!(admission.reservation_of(&source), Some((cpu(0), 512)));
}

#[ktest]
fn successful_migration_moves_reservation_between_cpus() {
    let admission = DeadlineAdmission::new();
    let task = make_task();
    let attr = deadline(500);
    admission
        .update_attr(&task, cpu(0), attr, SCHED_CAPACITY_SCALE, || {
            task.sched.set_sched_attr(attr);
            true
        })
        .expect("初始预留应成功");

    admission
        .migrate(&task, cpu(0), cpu(1), SCHED_CAPACITY_SCALE, || Ok(()))
        .expect("目标 CPU 容量充足时应迁移");

    assert_eq!(admission.reserved(cpu(0)), 0);
    assert_eq!(admission.reserved(cpu(1)), 512);
    assert_eq!(admission.reservation_of(&task), Some((cpu(1), 512)));
}

#[ktest]
fn failed_attribute_apply_keeps_old_reservation() {
    let admission = DeadlineAdmission::new();
    let task = make_task();
    let old = deadline(500);
    let new = deadline(250);
    admission
        .update_attr(&task, cpu(0), old, SCHED_CAPACITY_SCALE, || {
            task.sched.set_sched_attr(old);
            true
        })
        .expect("初始预留应成功");

    assert!(
        admission
            .update_attr(&task, cpu(0), new, SCHED_CAPACITY_SCALE, || false)
            .is_err()
    );
    assert_eq!(admission.reserved(cpu(0)), 512);
    assert_eq!(admission.reservation_of(&task), Some((cpu(0), 512)));
}

#[ktest]
fn capacity_reduction_rejects_existing_overcommit() {
    let admission = DeadlineAdmission::new();
    let task = make_task();
    let attr = deadline(750);
    admission
        .update_attr(&task, cpu(0), attr, SCHED_CAPACITY_SCALE, || {
            task.sched.set_sched_attr(attr);
            true
        })
        .expect("初始预留应成功");

    let mut capacities = [SCHED_CAPACITY_SCALE; crate::NR_CPUS];
    assert!(admission.fits_capacities(capacities));
    capacities[0] = 512;
    assert!(!admission.fits_capacities(capacities));
}

#[ktest]
fn concurrent_admission_never_exceeds_cpu_capacity() {
    const WORKERS: usize = 8;
    let admission = Arc::new(DeadlineAdmission::new());
    let barrier = Arc::new(Barrier::new(WORKERS));
    let accepted = Arc::new(AtomicUsize::new(0));
    let tasks: alloc::vec::Vec<_> = (0..WORKERS).map(|_| make_task()).collect();
    let mut workers = alloc::vec::Vec::new();

    for task in &tasks {
        let admission = Arc::clone(&admission);
        let barrier = Arc::clone(&barrier);
        let accepted = Arc::clone(&accepted);
        let task = Arc::clone(task);
        workers.push(thread::spawn(move || {
            let attr = deadline(200);
            barrier.wait();
            if admission
                .update_attr(&task, cpu(0), attr, SCHED_CAPACITY_SCALE, || {
                    task.sched.set_sched_attr(attr);
                    true
                })
                .is_ok()
            {
                accepted.fetch_add(1, Ordering::AcqRel);
                thread::yield_now();
                assert!(admission.reserved(cpu(0)) <= SCHED_CAPACITY_SCALE);
            }
        }));
    }
    for worker in workers {
        worker.join().expect("并发准入线程不应 panic");
    }

    assert!(accepted.load(Ordering::Acquire) <= 5);
    assert!(admission.reserved(cpu(0)) <= SCHED_CAPACITY_SCALE);
}
