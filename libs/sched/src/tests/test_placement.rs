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
