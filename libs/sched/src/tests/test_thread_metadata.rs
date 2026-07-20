//! 线程元数据测试。

extern crate alloc;
extern crate std;

use alloc::sync::Weak;
use core::ptr::NonNull;

use ktest::ktest;

use crate::runqueue::Runqueue;
use crate::{
    ArchContextOps, CpuMask, NR_CPUS, ProcessGroup, RobustListState, RseqEvent, RseqRegistration,
    SchedAttr, SchedClass, SchedParams, SchedPolicy, Session, TASK_COMM_LEN, Task, ThreadGroup,
    supported_cpu_mask,
};

unsafe fn init_test_context(
    _ctx: NonNull<u8>,
    _stack_top: usize,
    _entry: crate::KernelEntry,
    _arg: usize,
) {
}

unsafe extern "C" fn switch_test_context(_prev: NonNull<u8>, _next: NonNull<u8>) {}

static TEST_ARCH_CONTEXT_OPS: ArchContextOps = ArchContextOps {
    context_size: 16,
    context_align: 16,
    init_kernel_context: init_test_context,
    switch_context: switch_test_context,
};

pub(super) fn make_task() -> alloc::sync::Arc<Task> {
    crate::arch_hooks::register(&TEST_ARCH_CONTEXT_OPS);
    let session = Session::new();
    let pg = ProcessGroup::new(&session);
    session.register_group(&pg);
    let tg = ThreadGroup::new();
    let task = Task::new(SchedParams::default_fair(), Weak::new(), tg, pg);
    task.adopt_current_context();
    task
}

#[ktest]
fn task_comm_is_nul_padded_and_truncated() {
    let task = make_task();
    assert_eq!(&task.comm()[..5], b"mygo\0");

    task.set_comm(b"abcdefghijklmnopqr");
    let comm = task.comm();
    assert_eq!(&comm[..TASK_COMM_LEN - 1], b"abcdefghijklmno");
    assert_eq!(comm[TASK_COMM_LEN - 1], 0);
}

#[ktest]
fn robust_list_and_rseq_state_roundtrip() {
    let task = make_task();
    task.set_robust_list(0x1000, 24);
    assert_eq!(
        task.robust_list(),
        RobustListState {
            head: 0x1000,
            len: 24,
        }
    );

    let rseq = RseqRegistration {
        ptr: 0x2000,
        len: 32,
        signature: 0x5305_5305,
        registered: true,
    };
    task.set_rseq_registration(rseq);
    assert_eq!(task.rseq_registration(), rseq);
    task.mark_rseq_event(RseqEvent::Preempt);
    assert!(task.rseq_events().contains(RseqEvent::Preempt));
    task.publish_rseq_cpu(0);
    task.publish_rseq_cpu(1);
    assert!(task.rseq_events().contains(RseqEvent::Migrate));
    task.clear_rseq_registration();
    assert_eq!(task.rseq_registration(), RseqRegistration::default());
    assert!(task.rseq_events().is_empty());
}

#[ktest]
fn supported_cpu_mask_matches_configured_capacity() {
    let expected = if NR_CPUS >= 64 {
        u64::MAX
    } else {
        (1u64 << NR_CPUS) - 1
    };
    assert_eq!(supported_cpu_mask(), expected);

    let task = make_task();
    task.set_cpu_affinity(0);
    assert_eq!(task.cpu_affinity(), 1);
    task.set_cpu_affinity(u64::MAX);
    assert_eq!(task.cpu_affinity(), supported_cpu_mask());
}

#[ktest]
fn runqueue_pick_respects_cpu_affinity_mask() {
    let task0 = make_task();
    let task1 = make_task();
    task0.set_cpu_affinity(CpuMask::single_raw(0).bits());
    task1.set_cpu_affinity(CpuMask::single_raw(1).bits());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&task0), 1);
    rq.enqueue(alloc::sync::Arc::clone(&task1), 1);

    let picked = rq
        .pick_next_on(2, CpuMask::single_raw(1).bits())
        .expect("cpu1 should find an allowed task");
    assert!(alloc::sync::Arc::ptr_eq(&picked, &task1));

    assert!(rq.dequeue(&task0, 3));
    assert!(rq.dequeue(&task1, 3));
}

#[ktest]
fn runqueue_migratable_load_excludes_idle_class() {
    let fair = make_task();
    let idle = make_task();
    idle.sched.set_sched_attr(SchedAttr::idle());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    rq.enqueue(alloc::sync::Arc::clone(&idle), 1);

    assert_eq!(rq.nr_running(), 2);
    assert_eq!(rq.migratable_load(), 1);

    assert!(rq.dequeue(&fair, 2));
    assert!(rq.dequeue(&idle, 2));
}

#[ktest]
fn runqueue_migratable_load_filters_cpu_affinity() {
    let cpu0 = make_task();
    let cpu1 = make_task();
    cpu0.set_cpu_affinity(CpuMask::single_raw(0).bits());
    cpu1.set_cpu_affinity(CpuMask::single_raw(1).bits());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&cpu0), 1);
    rq.enqueue(alloc::sync::Arc::clone(&cpu1), 1);

    assert_eq!(rq.migratable_load(), 2);
    assert_eq!(rq.migratable_load_for(CpuMask::single_raw(0).bits()), 1);
    assert_eq!(rq.migratable_load_for(CpuMask::single_raw(1).bits()), 1);
    assert_eq!(rq.migratable_load_for(CpuMask::single_raw(2).bits()), 0);

    assert!(rq.dequeue(&cpu0, 2));
    assert!(rq.dequeue(&cpu1, 2));
}

#[ktest]
fn runqueue_reports_migratable_load_by_sched_class() {
    let fair = make_task();
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(20));
    let deadline = make_task();
    deadline
        .sched
        .set_sched_attr(SchedAttr::deadline(1_000_000, 4_000_000, 4_000_000));

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    rq.enqueue(alloc::sync::Arc::clone(&realtime), 1);
    rq.enqueue(alloc::sync::Arc::clone(&deadline), 1);

    let load = rq.migratable_class_load();
    assert_eq!(load.fair, 1);
    assert_eq!(load.realtime, 1);
    assert_eq!(load.deadline, 1);
    assert_eq!(load.deadline_utilization, 256);
    assert_eq!(load.fair_weight, 1024);
    assert_eq!(load.total(), 3);

    assert!(rq.dequeue(&fair, 2));
    assert!(rq.dequeue(&realtime, 2));
    assert!(rq.dequeue(&deadline, 2));
}

#[ktest]
fn runqueue_takes_migratable_task_from_requested_class() {
    let fair = make_task();
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(20));

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    rq.enqueue(alloc::sync::Arc::clone(&realtime), 1);

    let pulled = rq
        .take_migratable_from_class(SchedClass::Realtime, CpuMask::single_raw(0).bits(), 2)
        .expect("realtime task should be migratable");
    assert!(alloc::sync::Arc::ptr_eq(&pulled, &realtime));
    assert_eq!(rq.migratable_class_load().fair, 1);
    assert_eq!(rq.migratable_class_load().realtime, 0);

    assert!(rq.dequeue(&fair, 3));
}

#[ktest]
fn runqueue_class_load_includes_current_task() {
    let fair = make_task();
    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    let current = rq.pick_next(2).expect("current task");

    assert_eq!(rq.migratable_class_load().fair, 0);
    assert_eq!(rq.class_load().fair, 1);

    assert!(rq.dequeue(&current, 3));
}

#[ktest]
fn runqueue_drain_queued_keeps_current_and_idle_tasks() {
    let current = make_task();
    let queued = make_task();
    let idle = make_task();
    idle.sched.set_sched_attr(SchedAttr::idle());

    let rq = Runqueue::new();
    rq.set_current(alloc::sync::Arc::clone(&current));
    rq.enqueue(alloc::sync::Arc::clone(&queued), 1);
    rq.enqueue(alloc::sync::Arc::clone(&idle), 1);

    let drained = rq.drain_queued(2);

    assert_eq!(drained.len(), 1);
    assert!(alloc::sync::Arc::ptr_eq(&drained[0], &queued));
    assert!(rq.is_current(&current));
    assert_eq!(rq.nr_running(), 2);
    assert!(rq.dequeue(&current, 3));
    assert!(rq.dequeue(&idle, 3));
}

#[ktest]
fn runqueue_take_migratable_respects_cpu_affinity() {
    let cpu0 = make_task();
    let cpu1 = make_task();
    cpu0.set_cpu_affinity(CpuMask::single_raw(0).bits());
    cpu1.set_cpu_affinity(CpuMask::single_raw(1).bits());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&cpu0), 1);
    rq.enqueue(alloc::sync::Arc::clone(&cpu1), 1);

    let pulled = rq
        .take_migratable(CpuMask::single_raw(1).bits(), 2)
        .expect("cpu1 should pull an affinity-compatible task");
    assert!(alloc::sync::Arc::ptr_eq(&pulled, &cpu1));
    assert!(!pulled.sched.on_rq());
    assert_eq!(rq.migratable_load(), 1);
    assert!(
        rq.take_migratable(CpuMask::single_raw(2).bits(), 3)
            .is_none()
    );

    assert!(rq.dequeue(&cpu0, 4));
}

#[ktest]
fn runqueue_take_migratable_preserves_fair_lag() {
    let early = make_task();
    let late = make_task();
    early.sched.store_vruntime(100);
    late.sched.store_vruntime(300);

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&early), 1);
    rq.enqueue(alloc::sync::Arc::clone(&late), 1);

    let pulled = rq
        .take_migratable(CpuMask::single_raw(0).bits(), 2)
        .expect("fair task should be migratable");
    assert!(alloc::sync::Arc::ptr_eq(&pulled, &late));
    assert_eq!(pulled.sched.lag(), -200);

    assert!(rq.dequeue(&early, 3));
}

#[ktest]
fn runqueue_update_nice_keeps_policy_and_slice() {
    let task = make_task();
    task.sched
        .set_sched_attr(SchedAttr::rt_round_robin(20, 50_000_000));

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&task), 1);
    rq.update_nice(&task, 10, 2);

    let attr = task.sched.sched_attr();
    assert_eq!(attr.policy, SchedPolicy::RtRoundRobin);
    assert_eq!(attr.priority, 20);
    assert_eq!(attr.slice_ns, 50_000_000);
    assert_eq!(attr.nice, 10);

    assert!(rq.dequeue(&task, 3));
}

#[ktest]
fn runqueue_update_wrong_queue_does_not_mutate_entity() {
    let task = make_task();
    task.sched.set_on_rq(true);
    task.set_current_cpu(1);

    let rq = Runqueue::new();
    assert!(!rq.update_nice(&task, -10, 1));
    assert_eq!(task.sched.nice(), 0);
    assert!(task.sched.on_rq());
}
