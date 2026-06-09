//! 线程元数据测试。

extern crate alloc;
extern crate std;

use alloc::sync::Weak;

use ktest::ktest;

use crate::{
    CpuMask, NR_CPUS, ProcessGroup, RobustListState, RseqRegistration, Runqueue, SchedAttr,
    SchedParams, Session, TASK_COMM_LEN, Task, ThreadGroup, supported_cpu_mask,
};

fn make_task() -> alloc::sync::Arc<Task> {
    let session = Session::new();
    let pg = ProcessGroup::new(&session);
    session.register_group(&pg);
    let tg = ThreadGroup::new();
    Task::new(SchedParams::default_fair(), Weak::new(), tg, pg)
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
    task.clear_rseq_registration();
    assert_eq!(task.rseq_registration(), RseqRegistration::default());
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
