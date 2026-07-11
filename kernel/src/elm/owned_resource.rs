//! ELM 子系统资源所有权注册表。
//!
//! 注册表只保存所有权和退役操作表。回调始终在释放注册表锁后执行，避免子系统
//! 回调重入 Core 或睡眠时持有自旋锁。单元一旦进入排空阶段就永久关闭该代际的
//! 新资源登记；失败单元保持隔离，不能重新激活旧回调。

use alloc::vec::Vec;

use elm_model::{
    ElmId, ElmOwnedResourceKind, ElmOwnedResourceOpsV1, ElmOwnedResourceSnapshotV1,
    ElmOwnedResourceState, Generation,
};
use sched::sync::Spinlock;

const OWNED_RESOURCE_CAPACITY: usize = 4096;
const OWNED_RESOURCE_OWNER_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnedResourceError {
    Invalid,
    NotFound,
    Duplicate,
    StaleGeneration,
    OwnerQuiescing,
    Busy,
    Capacity,
    Callback(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OwnedResourceDrainReport {
    pub drained: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OwnedResourceOwnerSnapshot {
    pub owner: ElmId,
    pub generation: Generation,
    pub accepting: bool,
    pub resource_count: usize,
}

#[derive(Clone, Copy)]
struct OwnerGate {
    owner: ElmId,
    generation: Generation,
    accepting: bool,
}

#[derive(Clone, Copy)]
struct ResourceRecord {
    id: u64,
    owner: ElmId,
    generation: Generation,
    kind: ElmOwnedResourceKind,
    handle: u64,
    state: ElmOwnedResourceState,
    last_status: i32,
    ops: ElmOwnedResourceOpsV1,
}

#[derive(Clone, Copy)]
struct DrainWork {
    id: u64,
    owner: ElmId,
    generation: Generation,
    handle: u64,
    ops: ElmOwnedResourceOpsV1,
}

struct OwnedResourceRegistry {
    next_id: u64,
    owners: Vec<OwnerGate>,
    resources: Vec<ResourceRecord>,
}

impl OwnedResourceRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            owners: Vec::new(),
            resources: Vec::new(),
        }
    }

    fn owner_index(&self, owner: ElmId) -> Option<usize> {
        self.owners.iter().position(|entry| entry.owner == owner)
    }

    fn resource_index(&self, id: u64) -> Option<usize> {
        self.resources.iter().position(|entry| entry.id == id)
    }
}

static OWNED_RESOURCES: Spinlock<OwnedResourceRegistry> =
    Spinlock::new(OwnedResourceRegistry::new());

pub(crate) fn init() -> bool {
    let mut registry = OWNED_RESOURCES.lock();
    let owner_capacity = registry.owners.capacity();
    if owner_capacity < OWNED_RESOURCE_OWNER_CAPACITY
        && registry
            .owners
            .try_reserve_exact(OWNED_RESOURCE_OWNER_CAPACITY - owner_capacity)
            .is_err()
    {
        return false;
    }
    let resource_capacity = registry.resources.capacity();
    resource_capacity >= OWNED_RESOURCE_CAPACITY
        || registry
            .resources
            .try_reserve_exact(OWNED_RESOURCE_CAPACITY - resource_capacity)
            .is_ok()
}

pub(crate) fn register_owner(owner: ElmId, generation: Generation) -> bool {
    if owner.0 == 0 || generation.0 == 0 {
        return false;
    }
    let mut registry = OWNED_RESOURCES.lock();
    if let Some(index) = registry.owner_index(owner) {
        if registry
            .resources
            .iter()
            .any(|resource| resource.owner == owner)
        {
            return false;
        }
        registry.owners[index] = OwnerGate {
            owner,
            generation,
            accepting: true,
        };
        return true;
    }
    if registry.owners.len() >= OWNED_RESOURCE_OWNER_CAPACITY {
        return false;
    }
    registry.owners.push(OwnerGate {
        owner,
        generation,
        accepting: true,
    });
    true
}

pub(crate) fn replace_owner_generation(
    owner: ElmId,
    old_generation: Generation,
    new_generation: Generation,
) -> bool {
    if owner.0 == 0 || old_generation.0 == 0 || new_generation.0 == 0 {
        return false;
    }
    let mut registry = OWNED_RESOURCES.lock();
    let Some(index) = registry.owner_index(owner) else {
        return false;
    };
    if registry.owners[index].generation != old_generation
        || !registry.owners[index].accepting
        || registry
            .resources
            .iter()
            .any(|resource| resource.owner == owner)
    {
        return false;
    }
    registry.owners[index].generation = new_generation;
    true
}

pub(crate) fn retire_owner(owner: ElmId, generation: Generation) -> bool {
    let mut registry = OWNED_RESOURCES.lock();
    let Some(index) = registry.owner_index(owner) else {
        return true;
    };
    if registry.owners[index].generation != generation
        || registry
            .resources
            .iter()
            .any(|resource| resource.owner == owner)
    {
        return false;
    }
    registry.owners.swap_remove(index);
    true
}

pub(crate) fn owner_snapshot(owner: ElmId) -> Option<OwnedResourceOwnerSnapshot> {
    let registry = OWNED_RESOURCES.lock();
    let gate = registry.owners.iter().find(|entry| entry.owner == owner)?;
    Some(OwnedResourceOwnerSnapshot {
        owner,
        generation: gate.generation,
        accepting: gate.accepting,
        resource_count: registry
            .resources
            .iter()
            .filter(|resource| resource.owner == owner && resource.generation == gate.generation)
            .count(),
    })
}

pub(crate) fn register(
    owner: ElmId,
    generation: Generation,
    kind: ElmOwnedResourceKind,
    handle: u64,
    ops: ElmOwnedResourceOpsV1,
) -> Result<u64, OwnedResourceError> {
    if owner.0 == 0 || generation.0 == 0 || handle == 0 || !ops.valid() {
        return Err(OwnedResourceError::Invalid);
    }
    let mut registry = OWNED_RESOURCES.lock();
    let Some(owner_index) = registry.owner_index(owner) else {
        return Err(OwnedResourceError::NotFound);
    };
    let gate = registry.owners[owner_index];
    if gate.generation != generation {
        return Err(OwnedResourceError::StaleGeneration);
    }
    if !gate.accepting {
        return Err(OwnedResourceError::OwnerQuiescing);
    }
    if registry.resources.iter().any(|resource| {
        resource.owner == owner
            && resource.generation == generation
            && resource.kind == kind
            && resource.handle == handle
    }) {
        return Err(OwnedResourceError::Duplicate);
    }
    if registry.resources.len() >= OWNED_RESOURCE_CAPACITY {
        return Err(OwnedResourceError::Capacity);
    }
    let id = registry.next_id;
    registry.next_id = registry
        .next_id
        .checked_add(1)
        .ok_or(OwnedResourceError::Capacity)?;
    registry.resources.push(ResourceRecord {
        id,
        owner,
        generation,
        kind,
        handle,
        state: ElmOwnedResourceState::Active,
        last_status: 0,
        ops,
    });
    Ok(id)
}

pub(crate) fn release(
    resource_id: u64,
    owner: ElmId,
    generation: Generation,
) -> Result<(), OwnedResourceError> {
    let mut registry = OWNED_RESOURCES.lock();
    let Some(index) = registry.resource_index(resource_id) else {
        return Err(OwnedResourceError::NotFound);
    };
    let resource = registry.resources[index];
    if resource.owner != owner || resource.generation != generation {
        return Err(OwnedResourceError::StaleGeneration);
    }
    if resource.state != ElmOwnedResourceState::Active {
        return Err(OwnedResourceError::Busy);
    }
    registry.resources.swap_remove(index);
    Ok(())
}

pub(crate) fn count_owned_by(owner: ElmId, generation: Generation) -> usize {
    OWNED_RESOURCES
        .lock()
        .resources
        .iter()
        .filter(|resource| resource.owner == owner && resource.generation == generation)
        .count()
}

pub(crate) fn stop_accepting(
    owner: ElmId,
    generation: Generation,
) -> Result<(), OwnedResourceError> {
    let mut registry = OWNED_RESOURCES.lock();
    let Some(index) = registry.owner_index(owner) else {
        return Err(OwnedResourceError::NotFound);
    };
    if registry.owners[index].generation != generation {
        return Err(OwnedResourceError::StaleGeneration);
    }
    registry.owners[index].accepting = false;
    Ok(())
}

pub(crate) fn drain_owner(
    owner: ElmId,
    generation: Generation,
) -> Result<OwnedResourceDrainReport, OwnedResourceError> {
    let work = {
        let mut registry = OWNED_RESOURCES.lock();
        let Some(owner_index) = registry.owner_index(owner) else {
            return Err(OwnedResourceError::NotFound);
        };
        if registry.owners[owner_index].generation != generation {
            return Err(OwnedResourceError::StaleGeneration);
        }
        if registry.resources.iter().any(|resource| {
            resource.owner == owner
                && resource.generation == generation
                && resource.state != ElmOwnedResourceState::Active
        }) {
            return Err(OwnedResourceError::Busy);
        }
        registry.owners[owner_index].accepting = false;
        let count = registry
            .resources
            .iter()
            .filter(|resource| resource.owner == owner && resource.generation == generation)
            .count();
        let mut work = Vec::new();
        work.try_reserve_exact(count)
            .map_err(|_| OwnedResourceError::Capacity)?;
        for resource in registry
            .resources
            .iter_mut()
            .rev()
            .filter(|resource| resource.owner == owner && resource.generation == generation)
        {
            resource.state = ElmOwnedResourceState::Quiescing;
            resource.last_status = 0;
            work.push(DrainWork {
                id: resource.id,
                owner,
                generation,
                handle: resource.handle,
                ops: resource.ops,
            });
        }
        work
    };

    let mut first_error = None;
    let stages: [(
        ElmOwnedResourceState,
        fn(&DrainWork) -> elm_model::ElmOwnedResourceOp,
    ); 4] = [
        (ElmOwnedResourceState::Quiescing, |item: &DrainWork| {
            item.ops.quiesce
        }),
        (ElmOwnedResourceState::Canceling, |item: &DrainWork| {
            item.ops.cancel
        }),
        (ElmOwnedResourceState::Draining, |item: &DrainWork| {
            item.ops.drain
        }),
        (ElmOwnedResourceState::Releasing, |item: &DrainWork| {
            item.ops.release
        }),
    ];
    for (state, callback) in stages {
        for item in &work {
            if resource_failed(item.id) {
                continue;
            }
            if let Err(err) = run_stage(item, state, callback(item)) {
                first_error.get_or_insert(err);
            }
        }
    }

    let mut registry = OWNED_RESOURCES.lock();
    let before = registry.resources.len();
    registry.resources.retain(|resource| {
        resource.owner != owner
            || resource.generation != generation
            || resource.state != ElmOwnedResourceState::Releasing
            || resource.last_status != 0
    });
    let drained = before.saturating_sub(registry.resources.len());
    if let Some(err) = first_error {
        return Err(err);
    }
    if drained != work.len() {
        return Err(OwnedResourceError::Busy);
    }
    Ok(OwnedResourceDrainReport { drained })
}

fn resource_failed(resource_id: u64) -> bool {
    OWNED_RESOURCES
        .lock()
        .resources
        .iter()
        .find(|resource| resource.id == resource_id)
        .is_none_or(|resource| resource.state == ElmOwnedResourceState::Failed)
}

fn run_stage(
    item: &DrainWork,
    state: ElmOwnedResourceState,
    callback: elm_model::ElmOwnedResourceOp,
) -> Result<(), OwnedResourceError> {
    {
        let mut registry = OWNED_RESOURCES.lock();
        let Some(index) = registry.resource_index(item.id) else {
            return Err(OwnedResourceError::NotFound);
        };
        let resource = &mut registry.resources[index];
        if resource.owner != item.owner || resource.generation != item.generation {
            return Err(OwnedResourceError::StaleGeneration);
        }
        resource.state = state;
        resource.last_status = 0;
    }
    match callback(item.owner, item.generation, item.handle) {
        Ok(()) => Ok(()),
        Err(status) => {
            let mut registry = OWNED_RESOURCES.lock();
            if let Some(index) = registry.resource_index(item.id) {
                registry.resources[index].state = ElmOwnedResourceState::Failed;
                registry.resources[index].last_status = status;
            }
            Err(OwnedResourceError::Callback(status))
        }
    }
}

pub(crate) fn snapshots() -> Result<Vec<ElmOwnedResourceSnapshotV1>, OwnedResourceError> {
    let registry = OWNED_RESOURCES.lock();
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(registry.resources.len())
        .map_err(|_| OwnedResourceError::Capacity)?;
    snapshots.extend(
        registry
            .resources
            .iter()
            .map(|resource| ElmOwnedResourceSnapshotV1 {
                abi_version: elm_model::ELM_OWNED_RESOURCE_ABI_VERSION,
                struct_size: core::mem::size_of::<ElmOwnedResourceSnapshotV1>() as u16,
                state: resource.state as u32,
                resource_id: resource.id,
                owner_cell_id: resource.owner.0,
                owner_generation: resource.generation.0,
                handle: resource.handle,
                kind: resource.kind as u32,
                last_status: resource.last_status,
            }),
    );
    Ok(snapshots)
}

pub(crate) fn first_orphaned_owner(
    mut is_known: impl FnMut(ElmId, Generation) -> bool,
) -> Option<(ElmId, Generation)> {
    let registry = OWNED_RESOURCES.lock();
    registry
        .resources
        .iter()
        .map(|resource| (resource.owner, resource.generation))
        .find(|(owner, generation)| !is_known(*owner, *generation))
}
