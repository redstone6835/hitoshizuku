//! 任务迁移事务测试。

use alloc::sync::Arc;

use ktest::ktest;

use super::test_thread_metadata::make_task;
use crate::scheduler::validate_migration_target;
use crate::{
    CpuId, CpuMask, MigrationContext, PlacementSnapshot, PlacementState, SchedTopology,
    TopologySnapshot, enqueue_task, migrate_task, register_cpu, runqueue_of,
};

#[ktest]
fn queued_task_migration_moves_task_to_target_runqueue() {
    let task = make_task();
    register_cpu(1).expect("register cpu1");
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    let source_cpu = enqueue_task(Arc::clone(&task), 1);
    let target_cpu = if source_cpu == 0 { 1 } else { 0 };

    migrate_task(&task, target_cpu).expect("migrate queued task");

    let placement = task.placement();
    assert_eq!(placement.cpu, CpuId::new(target_cpu));
    assert_eq!(placement.state, PlacementState::Bound);
    assert!(runqueue_of(target_cpu).dequeue(&task, 2));
    assert!(!task.sched.on_rq());
}

#[ktest]
fn migration_rejects_target_outside_affinity() {
    let task = make_task();
    register_cpu(1).expect("register cpu1");
    task.set_cpu_affinity(CpuMask::single_raw(0).bits());

    assert!(migrate_task(&task, 1).is_err());
    assert_eq!(task.placement().state, PlacementState::Unbound);
}

fn migration_context(generation: u64) -> MigrationContext {
    MigrationContext {
        source: PlacementSnapshot {
            cpu: CpuId::new(0),
            domain_id: 0,
            topology_generation: generation,
            state: PlacementState::Bound,
        },
        target_cpu: CpuId::new(1).unwrap(),
        target_domain: 0,
        topology_generation: generation,
    }
}

#[ktest]
fn migration_rejects_stale_topology_generation() {
    let context = migration_context(4);
    let topology = TopologySnapshot {
        topology: SchedTopology::bootstrap(),
        generation: 5,
        active: CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
    };

    assert!(validate_migration_target(context, topology, CpuMask::SUPPORTED).is_err());
}

#[ktest]
fn migration_rejects_offline_target_cpu() {
    let context = migration_context(4);
    let topology = TopologySnapshot {
        topology: SchedTopology::bootstrap(),
        generation: 4,
        active: CpuMask::BOOT,
    };

    assert!(validate_migration_target(context, topology, CpuMask::SUPPORTED).is_err());
}
