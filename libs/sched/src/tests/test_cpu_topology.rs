//! CPU 位图与调度域拓扑测试。

use ktest::ktest;

use crate::cpu::{CpuId, CpuMask, MAX_CPUS, ROOT_SCHED_DOMAIN_ID, SchedDomain, SchedTopology};
use crate::scheduler::select_balance_source;
use crate::{NR_CPUS, supported_cpu_mask};

#[ktest]
fn cpu_mask_truncates_to_supported_capacity() {
    let all = CpuMask::from_bits_truncate(u64::MAX);
    assert_eq!(all.bits(), supported_cpu_mask());
    assert_eq!(all.count(), NR_CPUS.min(u64::BITS as usize));
    assert_eq!(
        CpuMask::supported_storage_bytes(),
        MAX_CPUS.min(u64::BITS as usize).div_ceil(8).max(1)
    );

    let out_of_range = CpuMask::single_raw(NR_CPUS + 1);
    assert!(out_of_range.is_empty());

    let fallback = CpuMask::from_bits_or_boot(0);
    assert!(fallback.contains(CpuId::boot()));
}

#[ktest]
fn sched_topology_rejects_invalid_domains() {
    let child = SchedDomain::new(1, CpuMask::single_raw(0), 1, None).expect("child domain");
    assert!(SchedTopology::from_domains(&[SchedDomain::root(), child]).is_err());

    let bad_root = SchedDomain::new(ROOT_SCHED_DOMAIN_ID, CpuMask::single_raw(0), 0, None)
        .expect("narrow root");
    assert!(SchedTopology::from_domains(&[bad_root]).is_err());

    let loop_a = SchedDomain::new(1, CpuMask::single_raw(0), 1, Some(2)).expect("loop a");
    let loop_b = SchedDomain::new(2, CpuMask::single_raw(0), 2, Some(1)).expect("loop b");
    assert!(SchedTopology::from_domains(&[SchedDomain::root(), loop_a, loop_b]).is_err());

    let bad_level = SchedDomain::new(1, CpuMask::single_raw(0), 0, Some(0)).expect("bad level");
    assert!(SchedTopology::from_domains(&[SchedDomain::root(), bad_level]).is_err());
}

#[ktest]
fn sched_topology_selects_inside_local_domain_first() {
    let root = SchedDomain::root();
    let local = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("local domain");
    let remote = SchedDomain::new(
        2,
        CpuMask::single_raw(2).union(CpuMask::single_raw(3)),
        1,
        Some(0),
    )
    .expect("remote domain");
    let topology = SchedTopology::from_domains(&[root, local, remote]).expect("topology");

    let allowed = CpuMask::single_raw(0)
        .union(CpuMask::single_raw(1))
        .union(CpuMask::single_raw(2));
    let chosen = topology
        .select_cpu(allowed, allowed, CpuId::new(0), false, |cpu| {
            match cpu.get() {
                0 => 10,
                1 => 1,
                2 => 0,
                _ => 99,
            }
        })
        .expect("selected cpu");

    assert_eq!(chosen.get(), 1);
    let sources = topology.balance_sources(CpuId::new(0).unwrap(), allowed);
    assert!(sources.contains_raw(1));
    assert!(!sources.contains_raw(2));
}

#[ktest]
fn sched_topology_falls_back_to_root_when_local_domain_unusable() {
    let root = SchedDomain::root();
    let local = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("local domain");
    let remote = SchedDomain::new(
        2,
        CpuMask::single_raw(2).union(CpuMask::single_raw(3)),
        1,
        Some(0),
    )
    .expect("remote domain");
    let topology = SchedTopology::from_domains(&[root, local, remote]).expect("topology");

    let allowed = CpuMask::single_raw(2).union(CpuMask::single_raw(3));
    let chosen = topology
        .select_cpu(allowed, allowed, CpuId::new(0), false, |cpu| {
            if cpu.get() == 3 { 0 } else { 5 }
        })
        .expect("selected remote cpu");
    assert_eq!(chosen.get(), 3);
}

#[ktest]
fn balance_source_allows_single_remote_task_when_local_is_idle() {
    let topology = SchedTopology::bootstrap();
    let online = CpuMask::single_raw(0).union(CpuMask::single_raw(1));

    let source = select_balance_source(topology, CpuId::new(1).unwrap(), online, 0, |cpu| {
        if cpu.get() == 0 { 1 } else { 0 }
    })
    .expect("idle cpu should pull the only remote task");
    assert_eq!(source.get(), 0);

    let no_pull = select_balance_source(topology, CpuId::new(1).unwrap(), online, 1, |cpu| {
        if cpu.get() == 0 { 2 } else { 1 }
    });
    assert!(no_pull.is_none());
}
