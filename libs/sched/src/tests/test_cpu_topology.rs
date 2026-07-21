//! CPU 位图与调度域拓扑测试。

use core::cell::Cell;

use ktest::ktest;

use crate::cpu::{CpuId, CpuMask, MAX_CPUS, ROOT_SCHED_DOMAIN_ID, SchedDomain, SchedTopology};
use crate::scheduler::{RunqueueLoadSnapshot, select_balance_source_for_class};
use crate::{NR_CPUS, RunqueueClassLoad, SCHED_CAPACITY_SCALE, SchedClass, supported_cpu_mask};

fn class_load(class: SchedClass, tasks: usize) -> RunqueueClassLoad {
    let mut load = RunqueueClassLoad::default();
    match class {
        SchedClass::Deadline => {
            load.deadline = tasks;
            load.deadline_utilization = tasks as u64 * (SCHED_CAPACITY_SCALE / 4);
        }
        SchedClass::Realtime => load.realtime = tasks,
        SchedClass::Fair => {
            load.fair = tasks;
            load.fair_weight = tasks as u64 * 1024;
        }
        SchedClass::Idle => {}
    }
    load
}

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

    let left = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("left sibling");
    let right = SchedDomain::new(
        2,
        CpuMask::single_raw(1).union(CpuMask::single_raw(2)),
        1,
        Some(0),
    )
    .expect("right sibling");
    assert!(SchedTopology::from_domains(&[SchedDomain::root(), left, right]).is_err());
}

#[ktest]
fn sched_topology_allows_nested_overlapping_domains() {
    let cluster = SchedDomain::new(
        1,
        CpuMask::single_raw(0)
            .union(CpuMask::single_raw(1))
            .union(CpuMask::single_raw(2)),
        1,
        Some(0),
    )
    .expect("cluster domain");
    let core_pair = SchedDomain::new(
        2,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        2,
        Some(1),
    )
    .expect("nested domain");

    let topology =
        SchedTopology::from_domains(&[SchedDomain::root(), cluster, core_pair]).expect("topology");
    assert_eq!(
        topology
            .domain_for_cpu(CpuId::new(0).unwrap())
            .unwrap()
            .id(),
        2
    );
    assert_eq!(
        topology
            .domain_for_cpu(CpuId::new(2).unwrap())
            .unwrap()
            .id(),
        1
    );
}

#[ktest]
fn sched_domain_capacity_tracks_online_cpus() {
    let domain = SchedDomain::with_capacity(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
        1536,
    )
    .expect("domain with explicit capacity");

    assert_eq!(domain.capacity(), 1536);
    assert_eq!(domain.effective_capacity(CpuMask::single_raw(0)), 768);
    assert_eq!(domain.effective_capacity(CpuMask::EMPTY), 0);
}

#[ktest]
fn synthetic_topology_assigns_each_cpu_to_its_own_domain() {
    let topology = SchedTopology::with_cpu_domains();

    assert_eq!(topology.len(), MAX_CPUS + 1);
    for cpu_id in 0..MAX_CPUS {
        let cpu = CpuId::new(cpu_id).expect("cpu");
        let domain = topology.domain_for_cpu(cpu).expect("cpu domain");
        assert_eq!(domain.span(), CpuMask::single(cpu));
        assert_eq!(domain.parent(), Some(ROOT_SCHED_DOMAIN_ID));
    }
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
fn sched_topology_balance_sources_fall_back_to_parent_domain() {
    let root = SchedDomain::root();
    let cluster = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("cluster domain");
    let leaf = SchedDomain::new(2, CpuMask::single_raw(0), 2, Some(1)).expect("leaf domain");
    let remote = SchedDomain::new(3, CpuMask::single_raw(2), 1, Some(0)).expect("remote domain");
    let topology = SchedTopology::from_domains(&[root, cluster, leaf, remote]).expect("topology");
    let online = CpuMask::single_raw(0)
        .union(CpuMask::single_raw(1))
        .union(CpuMask::single_raw(2));

    let sources = topology.balance_sources(CpuId::new(0).unwrap(), online);

    assert!(sources.contains_raw(1));
    assert!(!sources.contains_raw(0));
    assert!(!sources.contains_raw(2));

    let root_sources = topology.balance_sources(
        CpuId::new(0).unwrap(),
        CpuMask::single_raw(0).union(CpuMask::single_raw(2)),
    );
    assert!(root_sources.contains_raw(2));
    assert!(!root_sources.contains_raw(1));
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
fn sched_topology_describes_task_placement_snapshot() {
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
    let affinity = CpuMask::single_raw(1).union(CpuMask::single_raw(2));
    let online = CpuMask::single_raw(0)
        .union(CpuMask::single_raw(1))
        .union(CpuMask::single_raw(2));

    let placement = topology.describe_placement(affinity, online, CpuId::new(0), false, |cpu| {
        match cpu.get() {
            1 => 1,
            2 => 0,
            _ => 9,
        }
    });

    assert_eq!(placement.current_cpu, CpuId::new(0));
    assert_eq!(placement.current_domain, Some(1));
    assert_eq!(placement.affinity.bits(), affinity.bits());
    assert_eq!(
        placement.effective.bits(),
        affinity.intersection(online).bits()
    );
    assert_eq!(placement.preferred_cpu, CpuId::new(1));
}

#[ktest]
fn runqueue_load_snapshot_samples_each_cpu_once() {
    let mask = CpuMask::single_raw(0)
        .union(CpuMask::single_raw(2))
        .union(CpuMask::single_raw(NR_CPUS + 1));
    let calls = Cell::new(0usize);

    let snapshot = RunqueueLoadSnapshot::collect(mask, |cpu| {
        calls.set(calls.get() + 1);
        cpu.get() + 7
    });

    assert_eq!(calls.get(), mask.intersection(CpuMask::SUPPORTED).count());
    assert_eq!(snapshot.load_of(CpuId::new(0).unwrap()), 7);
    assert_eq!(snapshot.load_of(CpuId::new(2).unwrap()), 9);
    assert_eq!(snapshot.load_of(CpuId::new(1).unwrap()), 0);
}

#[ktest]
fn balance_source_allows_single_remote_task_when_local_is_idle() {
    let topology = SchedTopology::bootstrap();
    let online = CpuMask::single_raw(0).union(CpuMask::single_raw(1));

    let source = select_balance_source_for_class(
        topology,
        CpuId::new(1).unwrap(),
        online,
        SchedClass::Fair,
        |cpu| class_load(SchedClass::Fair, usize::from(cpu.get() == 0)),
    )
    .expect("idle cpu should pull the only remote task");
    assert_eq!(source.get(), 0);

    let no_pull = select_balance_source_for_class(
        topology,
        CpuId::new(1).unwrap(),
        online,
        SchedClass::Fair,
        |cpu| class_load(SchedClass::Fair, if cpu.get() == 0 { 2 } else { 1 }),
    );
    assert!(no_pull.is_none());
}

#[ktest]
fn sched_topology_prefers_capacity_when_load_is_equal() {
    let root = SchedDomain::root();
    let slow = SchedDomain::with_capacity(1, CpuMask::single_raw(0), 1, Some(0), 512)
        .expect("slow cpu domain");
    let fast = SchedDomain::with_capacity(2, CpuMask::single_raw(1), 1, Some(0), 1024)
        .expect("fast cpu domain");
    let topology = SchedTopology::from_domains(&[root, slow, fast]).expect("topology");
    let online = CpuMask::single_raw(0).union(CpuMask::single_raw(1));

    let chosen = topology
        .select_cpu(online, online, None, false, |_| 1)
        .expect("selected cpu");
    assert_eq!(chosen.get(), 1);
}

#[ktest]
fn balance_source_checks_parent_when_near_domain_is_balanced() {
    let root = SchedDomain::root();
    let cluster = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("cluster domain");
    let leaf = SchedDomain::new(2, CpuMask::single_raw(0), 2, Some(1)).expect("leaf domain");
    let remote = SchedDomain::new(3, CpuMask::single_raw(2), 1, Some(0)).expect("remote domain");
    let topology = SchedTopology::from_domains(&[root, cluster, leaf, remote]).expect("topology");
    let online = CpuMask::single_raw(0)
        .union(CpuMask::single_raw(1))
        .union(CpuMask::single_raw(2));

    let source = select_balance_source_for_class(
        topology,
        CpuId::new(0).unwrap(),
        online,
        SchedClass::Fair,
        |cpu| {
            class_load(
                SchedClass::Fair,
                match cpu.get() {
                    0 | 1 => 1,
                    2 => 4,
                    _ => 0,
                },
            )
        },
    )
    .expect("parent domain should provide a source");
    assert_eq!(source.get(), 2);
}

#[ktest]
fn deadline_balance_does_not_move_single_task() {
    let topology = SchedTopology::bootstrap();
    let online = CpuMask::single_raw(0).union(CpuMask::single_raw(1));

    let single = select_balance_source_for_class(
        topology,
        CpuId::new(1).unwrap(),
        online,
        SchedClass::Deadline,
        |cpu| class_load(SchedClass::Deadline, usize::from(cpu.get() == 0)),
    );
    assert!(single.is_none());

    let overloaded = select_balance_source_for_class(
        topology,
        CpuId::new(1).unwrap(),
        online,
        SchedClass::Deadline,
        |cpu| class_load(SchedClass::Deadline, if cpu.get() == 0 { 2 } else { 0 }),
    );
    assert_eq!(overloaded, CpuId::new(0));
}

#[ktest]
fn fair_balance_uses_weight_instead_of_task_count() {
    let topology = SchedTopology::bootstrap();
    let online = CpuMask::single_raw(0).union(CpuMask::single_raw(1));
    let mut local = class_load(SchedClass::Fair, 1);
    local.fair_weight = 4096;
    let mut source = class_load(SchedClass::Fair, 3);
    source.fair_weight = 3072;

    let no_pull = select_balance_source_for_class(
        topology,
        CpuId::new(1).unwrap(),
        online,
        SchedClass::Fair,
        |cpu| if cpu.get() == 0 { source } else { local },
    );
    assert!(no_pull.is_none());

    source.fair_weight = 6144;
    let pull = select_balance_source_for_class(
        topology,
        CpuId::new(1).unwrap(),
        online,
        SchedClass::Fair,
        |cpu| if cpu.get() == 0 { source } else { local },
    );
    assert_eq!(pull, CpuId::new(0));
}

#[ktest]
fn balance_source_normalizes_load_by_cpu_capacity() {
    let root = SchedDomain::root();
    let slow = SchedDomain::with_capacity(1, CpuMask::single_raw(0), 1, Some(0), 512)
        .expect("slow cpu domain");
    let fast = SchedDomain::with_capacity(2, CpuMask::single_raw(1), 1, Some(0), 1024)
        .expect("fast cpu domain");
    let local = SchedDomain::new(3, CpuMask::single_raw(2), 1, Some(0)).expect("local cpu domain");
    let topology = SchedTopology::from_domains(&[root, slow, fast, local]).expect("topology");
    let online = CpuMask::single_raw(0)
        .union(CpuMask::single_raw(1))
        .union(CpuMask::single_raw(2));

    let source = select_balance_source_for_class(
        topology,
        CpuId::new(2).unwrap(),
        online,
        SchedClass::Fair,
        |cpu| class_load(SchedClass::Fair, usize::from(cpu.get() != 2)),
    );
    assert_eq!(source, CpuId::new(0));
}
