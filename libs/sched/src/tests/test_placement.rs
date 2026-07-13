//! 任务 placement 原子快照测试。

use ktest::ktest;

use crate::{CpuId, PlacementSnapshot, PlacementState, TaskPlacement};

#[ktest]
fn task_placement_starts_unbound() {
    let placement = TaskPlacement::unbound();

    assert_eq!(placement.snapshot(), PlacementSnapshot::unbound());
}

#[ktest]
fn task_placement_publishes_bound_fields_together() {
    let placement = TaskPlacement::unbound();
    let cpu = CpuId::new(3).expect("cpu3");

    placement.bind(cpu, 2, 17);

    assert_eq!(
        placement.snapshot(),
        PlacementSnapshot {
            cpu: Some(cpu),
            domain_id: 2,
            topology_generation: 17,
            state: PlacementState::Bound,
        }
    );
}

#[ktest]
fn task_placement_allows_only_one_migration_owner() {
    let placement = TaskPlacement::unbound();
    let cpu = CpuId::new(1).expect("cpu1");
    placement.bind(cpu, 1, 9);
    let source = placement.snapshot();

    assert!(placement.begin_migration(source));
    assert!(!placement.begin_migration(source));
    assert_eq!(placement.snapshot().state, PlacementState::Migrating);

    placement.rollback(source);
    assert_eq!(placement.snapshot(), source);
}

#[ktest]
fn task_placement_commits_migration_as_one_snapshot() {
    let placement = TaskPlacement::unbound();
    let source_cpu = CpuId::new(0).unwrap();
    let target_cpu = CpuId::new(2).unwrap();
    placement.bind(source_cpu, 0, 4);
    assert!(placement.begin_migration(placement.snapshot()));

    placement.store_bound(target_cpu, 2, 4);

    assert_eq!(
        placement.snapshot(),
        PlacementSnapshot {
            cpu: Some(target_cpu),
            domain_id: 2,
            topology_generation: 4,
            state: PlacementState::Bound,
        }
    );
}
