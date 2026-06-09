//! 线程元数据测试。

extern crate alloc;
extern crate std;

use alloc::sync::Weak;

use ktest::ktest;

use crate::{
    NR_CPUS, ProcessGroup, RobustListState, RseqRegistration, SchedParams, Session, TASK_COMM_LEN,
    Task, ThreadGroup, supported_cpu_mask,
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
