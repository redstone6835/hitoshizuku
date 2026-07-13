//! 调度域核心所有权与 per-CPU 状态隔离测试。

use ktest::ktest;

use crate::{CpuId, CpuMask, SchedDomain, SchedTopology, Scheduler};

#[ktest]
fn scheduler_state_bootstraps_root_and_boot_cpu() {
    let core = Scheduler::new();

    assert_eq!(core.online_set(), CpuMask::BOOT);
    assert_eq!(core.topology(), SchedTopology::bootstrap());
    assert!(core.cpu(CpuId::boot().get()).is_some());
    assert!(core.cpu(crate::scheduler::NR_CPUS).is_none());
}

#[ktest]
fn scheduler_state_keeps_cpu_intents_isolated() {
    let core = Scheduler::new();
    let cpu0 = core.cpu(0).expect("cpu0 state");
    let cpu1 = core.cpu(1).expect("cpu1 state");

    cpu1.request_resched();
    cpu1.request_balance();
    cpu1.request_post_syscall_handoff(1);
    cpu1.request_post_syscall_handoff(1);

    assert!(!cpu0.needs_resched());
    assert!(!cpu0.take_balance());
    assert!(!cpu0.has_post_syscall_handoff());
    assert!(cpu1.needs_resched());
    assert!(cpu1.take_balance());
    assert_eq!(cpu1.take_post_syscall_handoff(), 1);
    assert!(cpu1.take_resched());
    assert!(!cpu1.needs_resched());
}

#[ktest]
fn scheduler_state_installs_topology_and_online_cpu_together() {
    let core = Scheduler::new();
    let old_generation = core.topology_snapshot().generation;
    let local = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("local domain");
    let topology =
        SchedTopology::from_domains(&[SchedDomain::root(), local]).expect("valid topology");

    core.install_topology(topology);
    core.register_cpu(CpuId::new(1).expect("cpu1"));

    assert_eq!(core.topology(), topology);
    assert!(core.topology_snapshot().generation > old_generation);
    assert!(core.online_set().contains(CpuId::boot()));
    assert!(core.online_set().contains(CpuId::new(1).unwrap()));
}
