//! SOYO 动态组件的依赖事务、映射与生命周期 continuation。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::mem::size_of;
use core::ops::Range;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use general::mm::{VmSpace, copy_from_user, copy_to_user};
use general::syscall::NativeCallOutcome;
use mm::VmFlags;
use native_abi::wire::{
    ComponentCallState, ComponentCapabilityRecord, ComponentContext, ComponentInterfaceGate,
    ComponentLifecycle, ComponentLoadRequest, ComponentQuery, HandleTransfer, InterfaceRequest,
    ProcessArrayRef,
};
use native_abi::{
    BoundCallSlot, ComponentLifecycleMachine, ComponentState, ComponentTlsAllocator,
    ComponentTlsReservation, NativeBindingPlan, NativeHandle, ObjectInterface, OperationId, Rights,
    operation_by_id, status, wire,
};
use sched::sync::Spinlock;
use sched::{Task, TaskState, WaitQueue};
use soyo::registry::{DynamicRelocationKind, RelocationKind, SegmentKind, SegmentPermissions};
use soyo::{
    ComponentGraphError, ComponentGraphIdentity, ComponentGraphNode, DynamicRelocation, Relocation,
    SymbolExport, plan_component_graph,
};

use super::dispatch::native_return;
use super::memory::{InternalMemoryMapping, release_internal_mapping};
use super::{ImageObject, KernelNativeObject, NativeProcessState};

const MAX_PROCESS_CALL_SLOTS: usize = 4096;
pub(crate) const DYNAMIC_TLS_ARENA_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComponentKey {
    component_id: [u8; 16],
    abi_id: [u8; 16],
    build_id: [u8; 32],
    content_hash: [u8; 32],
}

pub(crate) struct ComponentObject {
    key: ComponentKey,
    metadata: Arc<soyo::SoyoMetadata>,
    vm: Arc<VmSpace>,
    base: usize,
    image_range: Range<usize>,
    context_range: Range<usize>,
    call_state_range: Range<usize>,
    tls: TlsAllocation,
    owned_slots: Vec<u32>,
    lifecycle: Spinlock<ComponentLifecycleMachine>,
    dependencies: Vec<Arc<ComponentObject>>,
    dependents: AtomicU32,
    interface_views: Spinlock<Vec<InterfaceView>>,
    marker_handle: AtomicU64,
    capability_handles: Vec<NativeHandle>,
    drain_waiters: WaitQueue,
    threads: Spinlock<Vec<Weak<Task>>>,
}

impl ComponentObject {
    fn component(&self) -> &soyo::ComponentMetadata {
        self.metadata
            .component
            .as_ref()
            .expect("ComponentObject 只能由 shared component 构造")
    }

    fn state(&self) -> ComponentState {
        self.lifecycle.lock().state()
    }

    fn generation(&self) -> u64 {
        self.lifecycle.lock().generation()
    }

    fn marker_handle(&self) -> Result<NativeHandle, u32> {
        let handle = NativeHandle::from_raw(self.marker_handle.load(Ordering::Acquire));
        (handle.raw() != 0)
            .then_some(handle)
            .ok_or(status::COMPONENT_INVALID_TRANSACTION)
    }

    fn lifecycle_record(&self, action: u32) -> ComponentLifecycle {
        let lifecycle = self.lifecycle.lock();
        ComponentLifecycle {
            action,
            state: lifecycle.state() as u32,
            component: self.marker_handle.load(Ordering::Acquire),
            entry: match action {
                wire::COMPONENT_ACTION_INITIALIZE => self
                    .base
                    .saturating_add(self.component().info.init_offset as usize)
                    as u64,
                wire::COMPONENT_ACTION_FINALIZE => self
                    .base
                    .saturating_add(self.component().info.fini_offset as usize)
                    as u64,
                _ => 0,
            },
            context: self.context_range.start as u64,
            tls_identity: self.tls.identity,
            generation: lifecycle.generation(),
            call_state: self.call_state_range.start as u64,
            ..ComponentLifecycle::default()
        }
    }

    fn export(&self, interface_id: [u8; 16], signature_hash: [u8; 32]) -> Option<&SymbolExport> {
        self.component().symbol_exports.iter().find(|export| {
            export.interface_id == interface_id && export.signature_hash == signature_hash
        })
    }

    pub(crate) fn register_thread(&self, task: &Arc<Task>) -> Result<(), u32> {
        let mut threads = self.threads.lock();
        threads
            .try_reserve(1)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        threads.push(Arc::downgrade(task));
        Ok(())
    }

    pub(crate) fn unregister_thread(&self, task: &Arc<Task>) {
        let mut threads = self.threads.lock();
        threads.retain(|candidate| {
            candidate
                .upgrade()
                .is_some_and(|candidate| !Arc::ptr_eq(&candidate, task))
        });
    }

    fn component_threads(&self) -> Result<Vec<Arc<Task>>, u32> {
        let mut threads = self.threads.lock();
        let mut active = Vec::new();
        active
            .try_reserve(threads.len())
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        threads.retain(|thread| {
            let Some(thread) = thread.upgrade() else {
                return false;
            };
            if matches!(thread.state(), TaskState::Zombie | TaskState::Dead) {
                false
            } else {
                active.push(thread);
                true
            }
        });
        Ok(active)
    }

    pub(crate) fn wake_drain_waiters(&self) {
        self.drain_waiters.wake_all();
    }
}

impl Drop for ComponentObject {
    fn drop(&mut self) {
        let interface_views = {
            let mut views = self.interface_views.lock();
            core::mem::take(&mut *views)
        };
        for view in interface_views {
            let _ = self.vm.unmap(view.vtable_range);
        }
        let _ = self.vm.unmap(self.call_state_range.clone());
    }
}

#[derive(Debug, Clone)]
struct InterfaceView {
    interface_id: [u8; 16],
    signature_hash: [u8; 32],
    vtable_range: Range<usize>,
}

pub(crate) struct InterfaceObject {
    component: Arc<ComponentObject>,
    vtable: usize,
}

pub(crate) struct ComponentTransaction {
    manager: Arc<ComponentManager>,
    inner: Spinlock<TransactionState>,
}

enum TransactionState {
    Loading(LoadTransaction),
    RollingBack(LoadRollback),
    Unloading(UnloadTransaction),
    Complete,
}

struct LoadTransaction {
    nodes: Vec<Arc<ComponentObject>>,
    root: Arc<ComponentObject>,
    sources: Vec<ImageInput>,
    next_init: usize,
    initialized: Vec<usize>,
    owned_slots: Vec<u32>,
    slot_publish_end: u32,
}

struct LoadRollback {
    nodes: Vec<Arc<ComponentObject>>,
    initialized: Vec<usize>,
    next_fini: usize,
    owned_slots: Vec<u32>,
    failure_status: u32,
}

struct UnloadTransaction {
    component: Arc<ComponentObject>,
}

impl Drop for ComponentTransaction {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        match core::mem::replace(&mut *state, TransactionState::Complete) {
            TransactionState::Loading(load) => {
                self.manager.rollback_nodes(&load.nodes, &load.owned_slots);
            }
            TransactionState::RollingBack(rollback) => {
                self.manager
                    .rollback_nodes(&rollback.nodes, &rollback.owned_slots);
            }
            TransactionState::Unloading(unload) => {
                let _ = finish_unload(
                    &self.manager,
                    &unload.component,
                    status::COMPONENT_LIFECYCLE_FAILED,
                );
                return;
            }
            TransactionState::Complete => return,
        }
        self.manager.end_transaction();
    }
}

struct ComponentRegistry {
    transaction_active: bool,
    preparing_threads: u32,
    thread_retirement_slots: usize,
    retired_thread_tls: Vec<RetiredThreadTls>,
    instances: Vec<Arc<ComponentObject>>,
}

struct RetiredThreadTls {
    state: Weak<NativeProcessState>,
    mapping: InternalMemoryMapping,
}

struct ProcessTlsArena {
    arenas: Vec<Range<usize>>,
    arena_size: usize,
    allocator: Option<ComponentTlsAllocator>,
    installed: bool,
}

#[derive(Debug, Clone)]
struct TlsAllocation {
    offset: u64,
    size: usize,
    identity: u64,
    reservation: Option<ComponentTlsReservation>,
    template: Vec<u8>,
}

impl TlsAllocation {
    const EMPTY: Self = Self {
        offset: 0,
        size: 0,
        identity: 0,
        reservation: None,
        template: Vec::new(),
    };
}

pub(crate) struct ThreadTlsPrepare<'a> {
    manager: &'a ComponentManager,
}

impl Drop for ThreadTlsPrepare<'_> {
    fn drop(&mut self) {
        let mut registry = self.manager.registry.lock();
        registry.preparing_threads = registry.preparing_threads.saturating_sub(1);
    }
}

struct DynamicCallSlots {
    base: u32,
    published: AtomicU32,
    operations: Vec<AtomicU32>,
    references: Vec<AtomicU32>,
}

impl DynamicCallSlots {
    fn new(base: usize) -> Result<Self, ()> {
        if base > MAX_PROCESS_CALL_SLOTS {
            return Err(());
        }
        let capacity = MAX_PROCESS_CALL_SLOTS - base;
        let mut operations = Vec::new();
        operations.try_reserve_exact(capacity).map_err(|_| ())?;
        operations.extend((0..capacity).map(|_| AtomicU32::new(0)));
        let mut references = Vec::new();
        references.try_reserve_exact(capacity).map_err(|_| ())?;
        references.extend((0..capacity).map(|_| AtomicU32::new(0)));
        Ok(Self {
            base: base as u32,
            published: AtomicU32::new(0),
            operations,
            references,
        })
    }

    fn resolve(&self, slot: u32) -> Option<OperationId> {
        let index = slot.checked_sub(self.base)?;
        if index >= self.published.load(Ordering::Acquire) {
            return None;
        }
        let raw = self.operations[index as usize].load(Ordering::Acquire);
        operation_by_id(raw).map(|spec| spec.id)
    }

    fn published(&self) -> u32 {
        self.published.load(Ordering::Acquire)
    }

    fn prepare(&self, cursor: &mut u32, operation: OperationId) -> Option<u32> {
        let end = (*cursor).max(self.published());
        for index in 0..end {
            if self.operations.get(index as usize)?.load(Ordering::Acquire) == operation as u32 {
                self.references[index as usize].fetch_add(1, Ordering::AcqRel);
                return Some(self.base + index);
            }
        }
        for index in 0..end {
            if self.references[index as usize].load(Ordering::Acquire) == 0 {
                self.operations[index as usize].store(operation as u32, Ordering::Release);
                self.references[index as usize].store(1, Ordering::Release);
                return Some(self.base + index);
            }
        }
        let index = end;
        self.operations
            .get(index as usize)?
            .store(operation as u32, Ordering::Release);
        self.references
            .get(index as usize)?
            .store(1, Ordering::Release);
        *cursor = index.checked_add(1)?;
        Some(self.base + index)
    }

    fn publish(&self, end: u32) {
        self.published.store(end, Ordering::Release);
    }

    fn tombstone(&self, slots: &[u32]) {
        for slot in slots {
            if let Some(index) = slot.checked_sub(self.base) {
                let Some(references) = self.references.get(index as usize) else {
                    continue;
                };
                let mut current = references.load(Ordering::Acquire);
                while current != 0 {
                    match references.compare_exchange_weak(
                        current,
                        current - 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(1) => {
                            self.operations[index as usize].store(0, Ordering::Release);
                            break;
                        }
                        Ok(_) => break,
                        Err(actual) => current = actual,
                    }
                }
            }
        }
    }
}

#[cfg(feature = "kernel-tests")]
#[ktest::ktest]
fn dynamic_call_slots_share_operations_and_reuse_released_slots() {
    let slots = DynamicCallSlots::new(2).expect("测试 dynamic slots 应建立成功");
    let mut publish_end = 0;
    let first = slots
        .prepare(&mut publish_end, OperationId::ClockRead)
        .expect("第一个 dynamic slot 应分配成功");
    slots.publish(publish_end);

    let duplicate = slots
        .prepare(&mut publish_end, OperationId::ClockRead)
        .expect("重复 operation 应复用 slot");
    assert_eq!(duplicate, first);
    slots.tombstone(&[first]);
    assert_eq!(slots.resolve(first), Some(OperationId::ClockRead));
    slots.tombstone(&[duplicate]);
    assert_eq!(slots.resolve(first), None);

    let reused = slots
        .prepare(&mut publish_end, OperationId::ThreadYield)
        .expect("已释放 slot 应再次分配");
    assert_eq!(reused, first);
}

pub(crate) struct ComponentManager {
    vm: Arc<VmSpace>,
    handles: Arc<Spinlock<native_abi::NativeHandleTable<KernelNativeObject>>>,
    registry: Spinlock<ComponentRegistry>,
    dynamic_slots: DynamicCallSlots,
    initial_operations: Vec<Option<OperationId>>,
    tls: Spinlock<ProcessTlsArena>,
}

impl ComponentManager {
    pub(crate) fn new(
        vm: Arc<VmSpace>,
        binding: &NativeBindingPlan,
        handles: Arc<Spinlock<native_abi::NativeHandleTable<KernelNativeObject>>>,
    ) -> Result<Arc<Self>, ()> {
        let mut initial_operations = Vec::new();
        initial_operations
            .try_reserve_exact(binding.call_slots.len())
            .map_err(|_| ())?;
        initial_operations.extend(binding.call_slots.iter().map(|slot| slot.operation));
        Ok(Arc::new(Self {
            vm,
            handles,
            registry: Spinlock::new(ComponentRegistry {
                transaction_active: false,
                preparing_threads: 0,
                thread_retirement_slots: 0,
                retired_thread_tls: Vec::new(),
                instances: Vec::new(),
            }),
            dynamic_slots: DynamicCallSlots::new(binding.call_slots.len())?,
            initial_operations,
            tls: Spinlock::new(ProcessTlsArena {
                arenas: Vec::new(),
                arena_size: 0,
                allocator: None,
                installed: false,
            }),
        }))
    }

    pub(crate) fn install_tls_arena(
        &self,
        range: Option<Range<usize>>,
        initial_used: usize,
    ) -> Result<(), ()> {
        let mut tls = self.tls.lock();
        if tls.installed {
            return Err(());
        }
        let allocator = match range.as_ref() {
            Some(range) => Some(ComponentTlsAllocator::new(range.len(), initial_used).ok_or(())?),
            None if initial_used == 0 => None,
            None => return Err(()),
        };
        if let Some(range) = range {
            tls.arenas.try_reserve(1).map_err(|_| ())?;
            tls.arena_size = range.len();
            tls.arenas.push(range);
        }
        tls.allocator = allocator;
        tls.installed = true;
        Ok(())
    }

    fn reserve_tls(&self, image: &ImageObject) -> Result<TlsAllocation, u32> {
        let Some(template) = image
            .metadata
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::TlsTemplate)
        else {
            return Ok(TlsAllocation::EMPTY);
        };
        let memory_size =
            usize::try_from(template.memory_size).map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
        let alignment =
            usize::try_from(template.alignment).map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
        let mut payload = Vec::new();
        let file_start =
            usize::try_from(template.file_offset).map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
        let file_size =
            usize::try_from(template.file_size).map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
        let file_end = file_start
            .checked_add(file_size)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        let source = image
            .bytes()
            .get(file_start..file_end)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        payload
            .try_reserve_exact(source.len())
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        payload.extend_from_slice(source);
        let (reservation, arenas) = {
            let mut tls = self.tls.lock();
            let reservation = tls
                .allocator
                .as_mut()
                .ok_or(status::COMPONENT_INVALID_IMAGE)?
                .reserve(memory_size, alignment)
                .ok_or(status::CORE_RESOURCE_EXHAUSTED)?;
            let mut arenas = Vec::new();
            if arenas.try_reserve_exact(tls.arenas.len()).is_err() {
                if let Some(allocator) = tls.allocator.as_mut() {
                    let _ = allocator.rollback(reservation);
                }
                return Err(status::CORE_RESOURCE_EXHAUSTED);
            }
            arenas.extend(tls.arenas.iter().cloned());
            (reservation, arenas)
        };
        let offset = reservation.offset();
        let allocation = TlsAllocation {
            offset: offset as u64,
            size: reservation.size(),
            identity: reservation.identity(),
            reservation: Some(reservation),
            template: payload,
        };
        if let Err(error) = self.initialize_tls_allocation(&allocation, &arenas) {
            self.rollback_tls(&allocation);
            return Err(error);
        }
        Ok(allocation)
    }

    fn rollback_tls(&self, allocation: &TlsAllocation) {
        let Some(reservation) = allocation.reservation else {
            return;
        };
        let mut tls = self.tls.lock();
        if let Some(allocator) = tls.allocator.as_mut() {
            let _ = allocator.rollback(reservation);
        }
    }

    fn clear_tls(&self, allocation: &TlsAllocation) {
        if allocation.size == 0 {
            return;
        }
        let arenas = {
            let tls = self.tls.lock();
            let mut arenas = Vec::new();
            if arenas.try_reserve_exact(tls.arenas.len()).is_err() {
                return;
            }
            arenas.extend(tls.arenas.iter().cloned());
            arenas
        };
        let zeroes = [0u8; 4096];
        for arena in arenas {
            let Some(mut cursor) = arena.start.checked_add(allocation.offset as usize) else {
                continue;
            };
            let Some(end) = cursor.checked_add(allocation.size) else {
                continue;
            };
            while cursor < end {
                let length = zeroes.len().min(end - cursor);
                if write_vm_bytes(&self.vm, cursor, &zeroes[..length]).is_err() {
                    break;
                }
                cursor += length;
            }
        }
    }

    fn initialize_tls_allocation(
        &self,
        allocation: &TlsAllocation,
        arenas: &[Range<usize>],
    ) -> Result<(), u32> {
        let zeroes = [0u8; 4096];
        for arena in arenas {
            let start = arena
                .start
                .checked_add(allocation.offset as usize)
                .ok_or(status::COMPONENT_INVALID_IMAGE)?;
            let end = start
                .checked_add(allocation.size)
                .ok_or(status::COMPONENT_INVALID_IMAGE)?;
            if end > arena.end {
                return Err(status::COMPONENT_INVALID_IMAGE);
            }
            let mut cursor = start;
            while cursor < end {
                let length = zeroes.len().min(end - cursor);
                write_vm_bytes(&self.vm, cursor, &zeroes[..length])?;
                cursor += length;
            }
            if !allocation.template.is_empty() {
                write_vm_bytes(&self.vm, start, &allocation.template)?;
            }
        }
        Ok(())
    }

    pub(crate) fn begin_thread_prepare(&self) -> Result<ThreadTlsPrepare<'_>, u32> {
        let mut registry = self.registry.lock();
        if registry.transaction_active || registry.preparing_threads != 0 {
            return Err(status::THREAD_WOULD_BLOCK);
        }
        registry.preparing_threads = registry
            .preparing_threads
            .checked_add(1)
            .ok_or(status::CORE_RESOURCE_EXHAUSTED)?;
        Ok(ThreadTlsPrepare { manager: self })
    }

    pub(crate) fn install_thread_tls(&self, range: Option<Range<usize>>) -> Result<bool, u32> {
        let components = {
            let registry = self.registry.lock();
            let mut components = Vec::new();
            components
                .try_reserve_exact(registry.instances.len())
                .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
            components.extend(registry.instances.iter().cloned());
            components
        };
        let mut tls = self.tls.lock();
        if tls.arena_size == 0 {
            return Ok(false);
        }
        let range = range.ok_or(status::COMPONENT_INVALID_IMAGE)?;
        if range.len() < tls.arena_size {
            return Err(status::MEMORY_INVALID_RANGE);
        }
        tls.arenas
            .try_reserve(1)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        drop(tls);
        for component in &components {
            self.initialize_tls_allocation(&component.tls, core::slice::from_ref(&range))?;
        }
        let mut registry = self.registry.lock();
        debug_assert!(!registry.transaction_active && registry.preparing_threads != 0);
        let additional = registry
            .thread_retirement_slots
            .checked_add(1)
            .ok_or(status::CORE_RESOURCE_EXHAUSTED)?;
        registry
            .retired_thread_tls
            .try_reserve_exact(additional)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        let mut tls = self.tls.lock();
        tls.arenas.push(range);
        registry.thread_retirement_slots += 1;
        Ok(true)
    }

    pub(crate) fn unregister_thread_tls(&self, range: &Range<usize>) {
        let mut registry = self.registry.lock();
        debug_assert!(!registry.transaction_active);
        debug_assert!(registry.thread_retirement_slots != 0);
        registry.thread_retirement_slots = registry.thread_retirement_slots.saturating_sub(1);
        let mut tls = self.tls.lock();
        if let Some(index) = tls.arenas.iter().position(|arena| arena == range) {
            tls.arenas.swap_remove(index);
        }
    }

    pub(super) fn retire_thread_tls(
        &self,
        state: &Arc<NativeProcessState>,
        mapping: InternalMemoryMapping,
    ) {
        let immediate = {
            let mut registry = self.registry.lock();
            debug_assert!(registry.thread_retirement_slots != 0);
            registry.thread_retirement_slots = registry.thread_retirement_slots.saturating_sub(1);
            if registry.transaction_active {
                debug_assert!(
                    registry.retired_thread_tls.len() < registry.retired_thread_tls.capacity()
                );
                registry.retired_thread_tls.push(RetiredThreadTls {
                    state: Arc::downgrade(state),
                    mapping,
                });
                None
            } else {
                self.remove_tls_arena_locked(&mapping.range);
                Some(mapping)
            }
        };
        if let Some(mapping) = immediate {
            release_internal_mapping(state, mapping);
        }
    }

    fn remove_tls_arena_locked(&self, range: &Range<usize>) {
        let mut tls = self.tls.lock();
        if let Some(index) = tls.arenas.iter().position(|arena| arena == range) {
            tls.arenas.swap_remove(index);
        }
    }

    pub(crate) fn resolve_slot(&self, slot: usize) -> Option<BoundCallSlot> {
        if let Some(operation) = self.initial_operations.get(slot).copied().flatten() {
            let spec = native_abi::operation(operation)?;
            return Some(BoundCallSlot {
                slot: slot as u32,
                operation: Some(operation),
                interface: spec.interface,
                required_rights: spec.required_rights,
            });
        }
        let operation = self.dynamic_slots.resolve(slot as u32)?;
        let spec = native_abi::operation(operation)?;
        Some(BoundCallSlot {
            slot: slot as u32,
            operation: Some(operation),
            interface: spec.interface,
            required_rights: spec.required_rights,
        })
    }

    pub(crate) fn resolve_component_marker(
        &self,
        raw: u64,
    ) -> Result<Option<Arc<ComponentObject>>, u32> {
        if raw == 0 {
            return Ok(None);
        }
        let handles = self.handles.lock();
        let entry = handles.lookup(
            NativeHandle::from_raw(raw),
            Some(ObjectInterface::Component),
            Rights::NONE,
        )?;
        let KernelNativeObject::Component(component) = entry.object else {
            return Err(status::HANDLE_WRONG_INTERFACE);
        };
        if matches!(
            component.state(),
            ComponentState::Unloaded | ComponentState::Failed
        ) {
            return Err(status::COMPONENT_UNLOADED);
        }
        Ok(Some(Arc::clone(component)))
    }

    fn begin_transaction(&self) -> Result<(), u32> {
        let mut registry = self.registry.lock();
        if registry.transaction_active || registry.preparing_threads != 0 {
            return Err(status::COMPONENT_INITIALIZING);
        }
        registry.transaction_active = true;
        Ok(())
    }

    fn end_transaction(&self) {
        loop {
            let retired = {
                let mut registry = self.registry.lock();
                match registry.retired_thread_tls.pop() {
                    Some(retired) => {
                        self.remove_tls_arena_locked(&retired.mapping.range);
                        Some(retired)
                    }
                    None => {
                        registry.transaction_active = false;
                        return;
                    }
                }
            };
            if let Some(retired) = retired
                && let Some(state) = retired.state.upgrade()
            {
                release_internal_mapping(&state, retired.mapping);
            }
        }
    }

    fn find_exact(&self, key: ComponentKey) -> Option<Arc<ComponentObject>> {
        self.registry
            .lock()
            .instances
            .iter()
            .find(|component| component.key == key)
            .cloned()
    }

    fn prepare_publish(&self, node_count: usize) -> Result<(), u32> {
        self.registry
            .lock()
            .instances
            .try_reserve(node_count)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)
    }

    fn publish(&self, nodes: &[Arc<ComponentObject>], slot_end: u32) {
        {
            let mut registry = self.registry.lock();
            debug_assert!(registry.instances.capacity() - registry.instances.len() >= nodes.len());
            for node in nodes {
                for dependency in &node.dependencies {
                    dependency.dependents.fetch_add(1, Ordering::AcqRel);
                }
                registry.instances.push(Arc::clone(node));
            }
            self.dynamic_slots.publish(slot_end);
        }
        self.end_transaction();
    }

    fn remove_instance(&self, component: &Arc<ComponentObject>) {
        let mut registry = self.registry.lock();
        if let Some(index) = registry
            .instances
            .iter()
            .position(|item| Arc::ptr_eq(item, component))
        {
            registry.instances.swap_remove(index);
        }
    }

    fn rollback_nodes(&self, nodes: &[Arc<ComponentObject>], owned_slots: &[u32]) {
        self.dynamic_slots.tombstone(owned_slots);
        for node in nodes.iter().rev() {
            let mut handles = self.handles.lock();
            for handle in &node.capability_handles {
                let _ = handles.close(*handle);
            }
            if let Ok(marker) = node.marker_handle() {
                let _ = handles.close(marker);
            }
            drop(handles);
            self.clear_tls(&node.tls);
            self.rollback_tls(&node.tls);
            let _ = self.vm.unmap(node.image_range.clone());
            let _ = self.vm.unmap(node.context_range.clone());
        }
    }

    fn append_call_slots(
        &self,
        binding: &NativeBindingPlan,
        cursor: &mut u32,
    ) -> Result<(Vec<u32>, Vec<u32>), u32> {
        let mut actual = Vec::new();
        actual
            .try_reserve_exact(binding.call_slots.len())
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(binding.call_slots.len())
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        for binding in &binding.call_slots {
            let Some(operation) = binding.operation else {
                actual.push(u32::MAX);
                continue;
            };
            if let Some(slot) = self
                .initial_operations
                .iter()
                .position(|candidate| *candidate == Some(operation))
            {
                actual.push(slot as u32);
                continue;
            }
            let Some(slot) = self.dynamic_slots.prepare(cursor, operation) else {
                self.dynamic_slots.tombstone(&owned);
                return Err(status::CORE_RESOURCE_EXHAUSTED);
            };
            actual.push(slot);
            owned.push(slot);
        }
        Ok((actual, owned))
    }
}

#[derive(Clone)]
struct ImageInput {
    handle: NativeHandle,
    image: Arc<ImageObject>,
    dependencies: Vec<usize>,
    capabilities: Vec<PreparedComponentBinding>,
}

#[derive(Clone)]
struct PreparedComponentBinding {
    requirement_id: u32,
    source_handle: NativeHandle,
    interface: ObjectInterface,
    rights: Rights,
}

struct PreparedResources<'a> {
    manager: &'a ComponentManager,
    nodes: Vec<Arc<ComponentObject>>,
    owned_slots: Vec<u32>,
    armed: bool,
}

impl PreparedResources<'_> {
    fn disarm(mut self) -> (Vec<Arc<ComponentObject>>, Vec<u32>) {
        self.armed = false;
        (
            core::mem::take(&mut self.nodes),
            core::mem::take(&mut self.owned_slots),
        )
    }
}

impl Drop for PreparedResources<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.manager.rollback_nodes(&self.nodes, &self.owned_slots);
        }
    }
}

pub(super) fn component_load(
    state: &NativeProcessState,
    request_user: u64,
    lifecycle_user: u64,
) -> NativeCallOutcome {
    let request: ComponentLoadRequest = match copy_user_value(request_user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    if request.flags != 0
        || request.reserved != [0; 2]
        || request.images.reserved != 0
        || request.bindings.reserved != 0
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    if let Err(error) = state.components.begin_transaction() {
        return native_return(error, 0, 0);
    }
    let result = prepare_load(state, request, lifecycle_user);
    if result.is_err() {
        state.components.end_transaction();
    }
    match result {
        Ok(outcome) => outcome,
        Err(error) => native_return(error, 0, 0),
    }
}

fn prepare_load(
    state: &NativeProcessState,
    request: ComponentLoadRequest,
    lifecycle_user: u64,
) -> Result<NativeCallOutcome, u32> {
    let handles = read_image_handles(request.root_image, request.images)?;
    let mut inputs = clone_images(state, &handles)?;
    let bindings = read_component_bindings(state, request.bindings)?;
    attach_component_bindings(&mut inputs, &bindings)?;
    let order = build_dependency_graph(&mut inputs)?;
    revalidate_images(state, &inputs)?;

    let root_key = component_key(&inputs[0].image)?;
    if let Some(existing) = state.components.find_exact(root_key) {
        if existing.state() != ComponentState::Active {
            return Err(status::COMPONENT_INITIALIZING);
        }
        let handle = insert_component_handle(state, existing)?;
        if let Err(error) = write_user_value(lifecycle_user, &ComponentLifecycle::default()) {
            let _ = state.handles.lock().close(handle);
            return Err(error);
        }
        state.components.end_transaction();
        return Ok(native_return(status::OK, handle.raw(), 0));
    }

    let mut prepared: Vec<Option<Arc<ComponentObject>>> = Vec::new();
    prepared
        .try_reserve_exact(inputs.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    prepared.resize_with(inputs.len(), || None);
    let mut resources = PreparedResources {
        manager: &state.components,
        nodes: Vec::new(),
        owned_slots: Vec::new(),
        armed: true,
    };
    resources
        .nodes
        .try_reserve_exact(inputs.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let total_imports = inputs.iter().try_fold(0usize, |total, input| {
        total.checked_add(input.image.binding.call_slots.len())
    });
    resources
        .owned_slots
        .try_reserve_exact(total_imports.ok_or(status::CORE_RESOURCE_EXHAUSTED)?)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let mut slot_publish_end = state.components.dynamic_slots.published();

    for index in order {
        let key = component_key(&inputs[index].image)?;
        if let Some(existing) = state.components.find_exact(key) {
            if existing.state() != ComponentState::Active {
                return Err(status::COMPONENT_INITIALIZING);
            }
            prepared[index] = Some(existing);
            continue;
        }
        let (slots, new_slots) = state
            .components
            .append_call_slots(&inputs[index].image.binding, &mut slot_publish_end)?;
        resources.owned_slots.extend(new_slots.iter().copied());
        let mut dependencies = Vec::new();
        dependencies
            .try_reserve_exact(inputs[index].dependencies.len())
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        for dependency in &inputs[index].dependencies {
            dependencies.push(
                prepared[*dependency]
                    .as_ref()
                    .cloned()
                    .ok_or(status::COMPONENT_DEPENDENCY_CYCLE)?,
            );
        }
        let node = match map_component(
            &state.components,
            &inputs[index].image,
            dependencies,
            &slots,
            new_slots,
            &inputs[index].capabilities,
        ) {
            Ok(node) => node,
            Err(error) => return Err(error),
        };
        prepared[index] = Some(Arc::clone(&node));
        resources.nodes.push(node);
    }
    let root = prepared[0]
        .as_ref()
        .cloned()
        .ok_or(status::COMPONENT_INVALID_IMAGE)?;
    let mut initialized = Vec::new();
    initialized
        .try_reserve_exact(resources.nodes.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let (new_nodes, owned_slots) = resources.disarm();
    let mut load = LoadTransaction {
        nodes: new_nodes,
        root,
        sources: inputs,
        next_init: 0,
        initialized,
        owned_slots,
        slot_publish_end,
    };
    let next = match next_initialization(&mut load) {
        Ok(next) => next,
        Err(error) => {
            state
                .components
                .rollback_nodes(&load.nodes, &load.owned_slots);
            return Err(error);
        }
    };
    if let Some(lifecycle) = next {
        if let Err(error) = write_user_value(lifecycle_user, &lifecycle) {
            state
                .components
                .rollback_nodes(&load.nodes, &load.owned_slots);
            return Err(error);
        }
        let transaction = Arc::new(ComponentTransaction {
            manager: Arc::clone(&state.components),
            inner: Spinlock::new(TransactionState::Loading(load)),
        });
        let handle = state.handles.lock().insert(
            KernelNativeObject::ComponentTransaction(transaction),
            ObjectInterface::ComponentTransaction,
            Rights::LOAD | Rights::UNLOAD,
        )?;
        return Ok(native_return(status::OK, handle.raw(), 0));
    }
    if let Err(error) = revalidate_images(state, &load.sources) {
        state
            .components
            .rollback_nodes(&load.nodes, &load.owned_slots);
        return Err(error);
    }
    if let Err(error) = write_user_value(lifecycle_user, &ComponentLifecycle::default()) {
        state
            .components
            .rollback_nodes(&load.nodes, &load.owned_slots);
        return Err(error);
    }
    if let Err(error) = state.components.prepare_publish(load.nodes.len()) {
        state
            .components
            .rollback_nodes(&load.nodes, &load.owned_slots);
        return Err(error);
    }
    let component = match insert_component_handle(state, load.root) {
        Ok(component) => component,
        Err(error) => {
            state
                .components
                .rollback_nodes(&load.nodes, &load.owned_slots);
            return Err(error);
        }
    };
    state.components.publish(&load.nodes, load.slot_publish_end);
    Ok(native_return(status::OK, component.raw(), 0))
}

fn start_load_rollback(
    state: &NativeProcessState,
    transaction: &ComponentTransaction,
    transaction_handle: NativeHandle,
    inner: &mut TransactionState,
    load: LoadTransaction,
    failure_status: u32,
    lifecycle_user: u64,
) -> NativeCallOutcome {
    let initialized_len = load.initialized.len();
    let mut rollback = LoadRollback {
        nodes: load.nodes,
        initialized: load.initialized,
        next_fini: initialized_len,
        owned_slots: load.owned_slots,
        failure_status,
    };
    if let Some(lifecycle) = next_rollback_finalizer(&mut rollback) {
        if let Err(error) = write_user_value(lifecycle_user, &lifecycle) {
            transaction
                .manager
                .rollback_nodes(&rollback.nodes, &rollback.owned_slots);
            transaction.manager.end_transaction();
            let _ = state.handles.lock().close(transaction_handle);
            return native_return(error, 0, 0);
        }
        *inner = TransactionState::RollingBack(rollback);
        return native_return(status::OK, transaction_handle.raw(), 0);
    }
    transaction
        .manager
        .rollback_nodes(&rollback.nodes, &rollback.owned_slots);
    transaction.manager.end_transaction();
    let _ = state.handles.lock().close(transaction_handle);
    let _ = write_user_value(lifecycle_user, &ComponentLifecycle::default());
    native_return(failure_status, 0, 0)
}

pub(super) fn component_activate(
    state: &NativeProcessState,
    transaction: &Arc<ComponentTransaction>,
    transaction_handle: NativeHandle,
    lifecycle_status: u64,
    lifecycle_user: u64,
) -> NativeCallOutcome {
    let Ok(lifecycle_status) = u32::try_from(lifecycle_status) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    let mut inner = transaction.inner.lock();
    let TransactionState::Loading(mut load) =
        core::mem::replace(&mut *inner, TransactionState::Complete)
    else {
        return native_return(status::COMPONENT_INVALID_TRANSACTION, 0, 0);
    };
    let current = load.next_init.saturating_sub(1);
    let Some(component) = load.nodes.get(current) else {
        *inner = TransactionState::Loading(load);
        return native_return(status::COMPONENT_INVALID_TRANSACTION, 0, 0);
    };
    if component
        .lifecycle
        .lock()
        .activate(lifecycle_status)
        .is_err()
    {
        return start_load_rollback(
            state,
            transaction,
            transaction_handle,
            &mut inner,
            load,
            status::COMPONENT_LIFECYCLE_FAILED,
            lifecycle_user,
        );
    }
    if write_call_state(
        &transaction.manager.vm,
        component.call_state_range.start,
        ComponentState::Active,
        component.generation(),
    )
    .is_err()
    {
        transaction
            .manager
            .rollback_nodes(&load.nodes, &load.owned_slots);
        transaction.manager.end_transaction();
        return NativeCallOutcome::ExitGroup(127);
    }
    load.initialized.push(current);
    match next_initialization(&mut load) {
        Ok(Some(lifecycle)) => {
            if let Err(error) = write_user_value(lifecycle_user, &lifecycle) {
                return start_load_rollback(
                    state,
                    transaction,
                    transaction_handle,
                    &mut inner,
                    load,
                    error,
                    lifecycle_user,
                );
            }
            *inner = TransactionState::Loading(load);
            native_return(status::OK, transaction_handle.raw(), 0)
        }
        Ok(None) => {
            if let Err(error) = revalidate_images(state, &load.sources) {
                return start_load_rollback(
                    state,
                    transaction,
                    transaction_handle,
                    &mut inner,
                    load,
                    error,
                    lifecycle_user,
                );
            }
            if let Err(error) = transaction.manager.prepare_publish(load.nodes.len()) {
                return start_load_rollback(
                    state,
                    transaction,
                    transaction_handle,
                    &mut inner,
                    load,
                    error,
                    lifecycle_user,
                );
            }
            if let Err(error) = write_user_value(lifecycle_user, &ComponentLifecycle::default()) {
                return start_load_rollback(
                    state,
                    transaction,
                    transaction_handle,
                    &mut inner,
                    load,
                    error,
                    lifecycle_user,
                );
            }
            let component = match replace_transaction_with_component_handle(
                state,
                transaction,
                transaction_handle,
                Arc::clone(&load.root),
            ) {
                Ok(component) => component,
                Err(error) => {
                    return start_load_rollback(
                        state,
                        transaction,
                        transaction_handle,
                        &mut inner,
                        load,
                        error,
                        lifecycle_user,
                    );
                }
            };
            transaction
                .manager
                .publish(&load.nodes, load.slot_publish_end);
            native_return(status::OK, component.raw(), 0)
        }
        Err(error) => start_load_rollback(
            state,
            transaction,
            transaction_handle,
            &mut inner,
            load,
            error,
            lifecycle_user,
        ),
    }
}

fn next_initialization(load: &mut LoadTransaction) -> Result<Option<ComponentLifecycle>, u32> {
    while load.next_init < load.nodes.len() {
        let index = load.next_init;
        load.next_init += 1;
        let component = &load.nodes[index];
        component.lifecycle.lock().begin_initialization()?;
        write_call_state(
            &component_context_vm(component),
            component.call_state_range.start,
            ComponentState::Initializing,
            component.generation(),
        )?;
        if component.component().info.init_offset != 0 {
            return Ok(Some(
                component.lifecycle_record(wire::COMPONENT_ACTION_INITIALIZE),
            ));
        }
        component.lifecycle.lock().activate(status::OK)?;
        write_call_state(
            &component_context_vm(component),
            component.call_state_range.start,
            ComponentState::Active,
            component.generation(),
        )?;
        load.initialized.push(index);
    }
    Ok(None)
}

fn component_context_vm(component: &ComponentObject) -> &Arc<VmSpace> {
    &component.vm
}

fn next_rollback_finalizer(rollback: &mut LoadRollback) -> Option<ComponentLifecycle> {
    while rollback.next_fini != 0 {
        rollback.next_fini -= 1;
        let index = rollback.initialized[rollback.next_fini];
        let component = &rollback.nodes[index];
        if component.component().info.fini_offset != 0 {
            return Some(component.lifecycle_record(wire::COMPONENT_ACTION_FINALIZE));
        }
    }
    None
}

pub(super) fn component_query(component: &Arc<ComponentObject>, user: u64) -> NativeCallOutcome {
    let (state, generation) = {
        let lifecycle = component.lifecycle.lock();
        (lifecycle.state(), lifecycle.generation())
    };
    let active_calls = match read_active_calls(component) {
        Ok(active_calls) => active_calls,
        Err(error) => return native_return(error, 0, 0),
    };
    let query = ComponentQuery {
        state: state as u32,
        generation,
        component_identity: component.key.component_id,
        abi_identity: component.key.abi_id,
        active_calls,
        dependent_count: component.dependents.load(Ordering::Acquire),
        ..ComponentQuery::default()
    };
    match write_user_value(user, &query) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

pub(super) fn component_interface(
    state: &NativeProcessState,
    component: &Arc<ComponentObject>,
    _component_handle: NativeHandle,
    request_user: u64,
) -> NativeCallOutcome {
    if component.state() != ComponentState::Active {
        return native_return(status::COMPONENT_UNLOADED, 0, 0);
    }
    let request: InterfaceRequest = match copy_user_value(request_user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    let Some(export) = component.export(request.interface_identity, request.signature_hash) else {
        return native_return(status::COMPONENT_DEPENDENCY_MISSING, 0, 0);
    };
    let view = {
        let views = component.interface_views.lock();
        views
            .iter()
            .find(|view| {
                view.interface_id == request.interface_identity
                    && view.signature_hash == request.signature_hash
            })
            .cloned()
    };
    let view = match view {
        Some(view) => view,
        None => match create_interface_view(state, component, export, request) {
            Ok(view) => view,
            Err(error) => return native_return(error, 0, 0),
        },
    };
    let object = Arc::new(InterfaceObject {
        component: Arc::clone(component),
        vtable: view.vtable_range.start,
    });
    let handle = match state.handles.lock().insert(
        KernelNativeObject::Interface(object),
        ObjectInterface::Interface,
        Rights::EXECUTE | Rights::DUPLICATE | Rights::INSPECT,
    ) {
        Ok(handle) => handle,
        Err(error) => return native_return(error, 0, 0),
    };
    native_return(status::OK, handle.raw(), view.vtable_range.start as u64)
}

fn create_interface_view(
    state: &NativeProcessState,
    component: &Arc<ComponentObject>,
    export: &SymbolExport,
    request: InterfaceRequest,
) -> Result<InterfaceView, u32> {
    let flags = VmFlags::EMPTY
        .with(VmFlags::USER)
        .with(VmFlags::READ)
        .with(VmFlags::WRITE);
    let range = state
        .components
        .vm
        .map_anon_any_aligned(
            native_abi::PAGE_SIZE as usize,
            native_abi::PAGE_SIZE as usize,
            flags,
        )
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let entry = component
        .base
        .checked_add(export.entry_offset as usize)
        .ok_or(status::COMPONENT_INVALID_IMAGE)? as u64;
    let marker_handle = component.marker_handle()?;
    let gate = ComponentInterfaceGate {
        call_state: component.call_state_range.start as u64,
        target: entry,
        component: marker_handle.raw(),
        generation: component.generation(),
    };
    if let Err(error) = write_vm_value(&state.components.vm, range.start, &gate) {
        let _ = state.components.vm.unmap(range);
        return Err(error);
    }
    if state
        .components
        .vm
        .mprotect(range.clone(), VmFlags::EMPTY.with(VmFlags::READ))
        .is_err()
    {
        let _ = state.components.vm.unmap(range);
        return Err(status::CORE_RESOURCE_EXHAUSTED);
    }
    let view = InterfaceView {
        interface_id: request.interface_identity,
        signature_hash: request.signature_hash,
        vtable_range: range,
    };
    let mut views = component.interface_views.lock();
    if let Some(existing) = views
        .iter()
        .find(|existing| {
            existing.interface_id == view.interface_id
                && existing.signature_hash == view.signature_hash
        })
        .cloned()
    {
        drop(views);
        let _ = state.components.vm.unmap(view.vtable_range);
        return Ok(existing);
    }
    if views.try_reserve(1).is_err() {
        drop(views);
        let _ = state.components.vm.unmap(view.vtable_range.clone());
        return Err(status::CORE_RESOURCE_EXHAUSTED);
    }
    views.push(view.clone());
    Ok(view)
}

pub(super) fn component_unload(
    task: &Arc<sched::Task>,
    state: &NativeProcessState,
    component: &Arc<ComponentObject>,
    _component_handle: NativeHandle,
    deadline_ns: u64,
    lifecycle_user: u64,
    caller_component: u64,
) -> NativeCallOutcome {
    if let Err(error) = state.components.begin_transaction() {
        return native_return(error, 0, 0);
    }
    let active = match read_active_calls(component) {
        Ok(active) => active,
        Err(error) => {
            state.components.end_transaction();
            return native_return(error, 0, 0);
        }
    };
    let mut component_threads = match component.component_threads() {
        Ok(threads) => threads,
        Err(error) => {
            state.components.end_transaction();
            return native_return(error, 0, 0);
        }
    };
    let old_state = component.state();
    let self_active = if let Some(owner) = super::thread::component_for_task(task) {
        Arc::ptr_eq(&owner, component)
    } else {
        match state.components.resolve_component_marker(caller_component) {
            Ok(Some(caller)) => Arc::ptr_eq(&caller, component),
            Ok(None) => false,
            Err(error) => {
                state.components.end_transaction();
                return native_return(error, 0, 0);
            }
        }
    };
    let begin = component.lifecycle.lock().begin_unload(
        component.dependents.load(Ordering::Acquire),
        self_active,
        active,
    );
    if let Err(error) = begin {
        state.components.end_transaction();
        return native_return(error, 0, 0);
    }
    if old_state == ComponentState::Active {
        let _ = write_call_state(
            &state.components.vm,
            component.call_state_range.start,
            ComponentState::Draining,
            component.generation(),
        );
    }
    for thread in &component_threads {
        if !Arc::ptr_eq(thread, task) {
            let _ = sched::native_thread_exit_wakeup(thread, 0);
        }
    }
    loop {
        let active = match read_active_calls(component) {
            Ok(active) => active,
            Err(error) => {
                state.components.end_transaction();
                return native_return(error, 0, 0);
            }
        };
        component_threads
            .retain(|thread| !matches!(thread.state(), TaskState::Zombie | TaskState::Dead));
        if active == 0 && component_threads.is_empty() {
            break;
        }
        if super::operations::has_native_external_control(task) {
            state.components.end_transaction();
            return NativeCallOutcome::RetryExternalControl;
        }
        if deadline_ns != 0 && sched::now_ns_public() >= deadline_ns {
            state.components.end_transaction();
            return native_return(component.lifecycle.lock().timeout(), 0, 0);
        }
        let entry = component
            .drain_waiters
            .prepare_to_wait(task, TaskState::Sleeping);
        let deadline_armed = deadline_ns != 0 && sched::register_sleep_deadline(task, deadline_ns);
        if deadline_ns != 0 && !deadline_armed {
            component.drain_waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            state.components.end_transaction();
            return native_return(component.lifecycle.lock().timeout(), 0, 0);
        }
        component_threads
            .retain(|thread| !matches!(thread.state(), TaskState::Zombie | TaskState::Dead));
        match read_active_calls(component) {
            Ok(0) if component_threads.is_empty() => {
                if deadline_armed {
                    sched::cancel_sleep_deadline(task);
                }
                component.drain_waiters.finish_wait(&entry);
                super::operations::restore_native_task_after_wait(task);
                break;
            }
            Ok(_) => {}
            Err(error) => {
                if deadline_armed {
                    sched::cancel_sleep_deadline(task);
                }
                component.drain_waiters.finish_wait(&entry);
                super::operations::restore_native_task_after_wait(task);
                state.components.end_transaction();
                return native_return(error, 0, 0);
            }
        }
        if super::operations::has_native_external_control(task) {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            component.drain_waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            state.components.end_transaction();
            return NativeCallOutcome::RetryExternalControl;
        }
        sched::schedule_once(sched::now_ns_public());
        if deadline_armed {
            sched::cancel_sleep_deadline(task);
        }
        component.drain_waiters.finish_wait(&entry);
        super::operations::restore_native_task_after_wait(task);
    }
    if component.state() == ComponentState::Draining
        && component.lifecycle.lock().calls_drained(0).is_err()
    {
        state.components.end_transaction();
        return native_return(status::COMPONENT_INVALID_TRANSACTION, 0, 0);
    }
    if component.component().info.fini_offset == 0 {
        let result = finish_unload(&state.components, component, status::OK);
        let _ = write_user_value(lifecycle_user, &ComponentLifecycle::default());
        return native_return(result, 0, 0);
    }
    let lifecycle = component.lifecycle_record(wire::COMPONENT_ACTION_FINALIZE);
    if let Err(error) = write_user_value(lifecycle_user, &lifecycle) {
        let _ = finish_unload(
            &state.components,
            component,
            status::COMPONENT_LIFECYCLE_FAILED,
        );
        return native_return(error, 0, 0);
    }
    let transaction = Arc::new(ComponentTransaction {
        manager: Arc::clone(&state.components),
        inner: Spinlock::new(TransactionState::Unloading(UnloadTransaction {
            component: Arc::clone(component),
        })),
    });
    match state.handles.lock().insert(
        KernelNativeObject::ComponentTransaction(transaction),
        ObjectInterface::ComponentTransaction,
        Rights::UNLOAD,
    ) {
        Ok(handle) => native_return(status::OK, handle.raw(), 0),
        Err(error) => {
            let _ = finish_unload(
                &state.components,
                component,
                status::COMPONENT_LIFECYCLE_FAILED,
            );
            native_return(error, 0, 0)
        }
    }
}

pub(super) fn component_finish(
    state: &NativeProcessState,
    transaction: &Arc<ComponentTransaction>,
    transaction_handle: NativeHandle,
    lifecycle_status: u64,
    lifecycle_user: u64,
) -> NativeCallOutcome {
    let Ok(lifecycle_status) = u32::try_from(lifecycle_status) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    let mut inner = transaction.inner.lock();
    match core::mem::replace(&mut *inner, TransactionState::Complete) {
        TransactionState::RollingBack(mut rollback) => {
            if let Some(lifecycle) = next_rollback_finalizer(&mut rollback) {
                if let Err(error) = write_user_value(lifecycle_user, &lifecycle) {
                    transaction
                        .manager
                        .rollback_nodes(&rollback.nodes, &rollback.owned_slots);
                    transaction.manager.end_transaction();
                    return native_return(error, 0, 0);
                }
                *inner = TransactionState::RollingBack(rollback);
                native_return(status::OK, transaction_handle.raw(), 0)
            } else {
                let failure_status = rollback.failure_status;
                transaction
                    .manager
                    .rollback_nodes(&rollback.nodes, &rollback.owned_slots);
                transaction.manager.end_transaction();
                let _ = state.handles.lock().close(transaction_handle);
                let _ = write_user_value(lifecycle_user, &ComponentLifecycle::default());
                native_return(failure_status, 0, 0)
            }
        }
        TransactionState::Unloading(unload) => {
            let result = finish_unload(&transaction.manager, &unload.component, lifecycle_status);
            let _ = state.handles.lock().close(transaction_handle);
            let _ = write_user_value(lifecycle_user, &ComponentLifecycle::default());
            native_return(result, 0, 0)
        }
        other => {
            *inner = other;
            native_return(status::COMPONENT_INVALID_TRANSACTION, 0, 0)
        }
    }
}

fn finish_unload(
    manager: &ComponentManager,
    component: &Arc<ComponentObject>,
    lifecycle_status: u32,
) -> u32 {
    let result = component.lifecycle.lock().finish(lifecycle_status);
    let _ = write_call_state(
        &manager.vm,
        component.call_state_range.start,
        ComponentState::Unloaded,
        component.generation(),
    );
    let _ = manager.vm.unmap(component.image_range.clone());
    let _ = manager.vm.unmap(component.context_range.clone());
    manager.clear_tls(&component.tls);
    manager.rollback_tls(&component.tls);
    {
        let mut handles = manager.handles.lock();
        for handle in &component.capability_handles {
            let _ = handles.close(*handle);
        }
        if let Ok(marker) = component.marker_handle() {
            let _ = handles.close(marker);
        }
    }
    manager.dynamic_slots.tombstone(&component.owned_slots);
    for dependency in &component.dependencies {
        dependency.dependents.fetch_sub(1, Ordering::AcqRel);
    }
    manager.remove_instance(component);
    manager.end_transaction();
    result
}

pub(super) fn component_wake(
    component: &Arc<ComponentObject>,
    generation: u64,
) -> NativeCallOutcome {
    if generation != component.generation() {
        return native_return(status::COMPONENT_INVALID_TRANSACTION, 0, 0);
    }
    if component.state() != ComponentState::Draining {
        return native_return(status::COMPONENT_INVALID_TRANSACTION, 0, 0);
    }
    let _ = component.drain_waiters.wake_one_default();
    native_return(status::OK, 0, 0)
}

fn read_image_handles(root: u64, images: ProcessArrayRef) -> Result<Vec<NativeHandle>, u32> {
    if root == 0
        || images.count > wire::MAX_COMPONENT_IMAGES
        || (images.count != 0 && images.ptr == 0)
    {
        return Err(status::CORE_OUT_OF_RANGE);
    }
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(images.count as usize + 1)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    handles.push(NativeHandle::from_raw(root));
    for index in 0..images.count {
        let address = images
            .ptr
            .checked_add(u64::from(index) * size_of::<u64>() as u64)
            .ok_or(status::CORE_OUT_OF_RANGE)?;
        let raw: u64 = copy_user_value(address)?;
        let handle = NativeHandle::from_raw(raw);
        if handles.contains(&handle) {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
        handles.push(handle);
    }
    Ok(handles)
}

fn read_component_bindings(
    state: &NativeProcessState,
    bindings: ProcessArrayRef,
) -> Result<Vec<PreparedComponentBinding>, u32> {
    if bindings.count > soyo::registry::MAX_CAPABILITIES
        || (bindings.count != 0 && bindings.ptr == 0)
    {
        return Err(status::CORE_OUT_OF_RANGE);
    }
    let mut prepared = Vec::new();
    prepared
        .try_reserve_exact(bindings.count as usize)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    for index in 0..bindings.count {
        let address = bindings
            .ptr
            .checked_add(u64::from(index) * size_of::<HandleTransfer>() as u64)
            .ok_or(status::CORE_OUT_OF_RANGE)?;
        let transfer: HandleTransfer = copy_user_value(address)?;
        if transfer.reserved != 0 || transfer.flags != 0 {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
        if prepared.iter().any(|binding: &PreparedComponentBinding| {
            binding.requirement_id == transfer.requirement_id
        }) {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
        let requirement = native_abi::requirement_by_id(transfer.requirement_id)
            .ok_or(status::CORE_INVALID_ARGUMENT)?;
        let source_handle = NativeHandle::from_raw(transfer.source_handle);
        let rights = Rights::from_bits(transfer.requested_rights);
        if !rights.is_subset_of(requirement.max_rights) {
            return Err(status::SECURITY_RIGHTS_DENIED);
        }
        state
            .handles
            .lock()
            .lookup(source_handle, Some(requirement.interface), rights)?;
        prepared.push(PreparedComponentBinding {
            requirement_id: transfer.requirement_id,
            source_handle,
            interface: requirement.interface,
            rights,
        });
    }
    Ok(prepared)
}

fn attach_component_bindings(
    images: &mut [ImageInput],
    bindings: &[PreparedComponentBinding],
) -> Result<(), u32> {
    for binding in bindings {
        if !images.iter().any(|image| {
            image
                .image
                .metadata
                .capabilities
                .iter()
                .any(|requirement| requirement.requirement_id == binding.requirement_id)
        }) {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
    }
    for image in images {
        image
            .capabilities
            .try_reserve_exact(image.image.metadata.capabilities.len())
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        for requirement in &image.image.metadata.capabilities {
            let registered = native_abi::requirement_by_id(requirement.requirement_id)
                .ok_or(status::COMPONENT_INVALID_IMAGE)?;
            if registered.interface as u16 != requirement.object_interface {
                return Err(status::COMPONENT_INVALID_IMAGE);
            }
            let Some(binding) = bindings
                .iter()
                .find(|binding| binding.requirement_id == requirement.requirement_id)
            else {
                if requirement.required() {
                    return Err(status::SECURITY_RIGHTS_DENIED);
                }
                continue;
            };
            let required = Rights::from_bits(requirement.required_rights);
            if binding.interface != registered.interface || !required.is_subset_of(binding.rights) {
                return Err(status::SECURITY_RIGHTS_DENIED);
            }
            image.capabilities.push(binding.clone());
        }
    }
    Ok(())
}

fn clone_images(
    state: &NativeProcessState,
    handles: &[NativeHandle],
) -> Result<Vec<ImageInput>, u32> {
    let table = state.handles.lock();
    let mut images = Vec::new();
    images
        .try_reserve_exact(handles.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    for handle in handles {
        let entry = table.lookup(*handle, Some(ObjectInterface::Image), Rights::LOAD)?;
        let KernelNativeObject::Image(image) = entry.object else {
            return Err(status::HANDLE_WRONG_INTERFACE);
        };
        if image.kind() != soyo::registry::ArtifactKind::SharedComponent {
            return Err(status::COMPONENT_INVALID_IMAGE);
        }
        images.push(ImageInput {
            handle: *handle,
            image: Arc::clone(image),
            dependencies: Vec::new(),
            capabilities: Vec::new(),
        });
    }
    Ok(images)
}

fn revalidate_images(state: &NativeProcessState, images: &[ImageInput]) -> Result<(), u32> {
    let handles = state.handles.lock();
    for image in images {
        let entry = handles.lookup(image.handle, Some(ObjectInterface::Image), Rights::LOAD)?;
        let KernelNativeObject::Image(current) = entry.object else {
            return Err(status::HANDLE_WRONG_INTERFACE);
        };
        if !Arc::ptr_eq(current, &image.image) {
            return Err(status::HANDLE_STALE);
        }
    }
    Ok(())
}

fn build_dependency_graph(images: &mut [ImageInput]) -> Result<Vec<usize>, u32> {
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(images.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    for image in images.iter() {
        let component = image
            .image
            .metadata
            .component
            .as_ref()
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        nodes.push(ComponentGraphNode {
            identity: ComponentGraphIdentity {
                component_id: component.info.component_id,
                abi_id: component.info.abi_id,
                build_id: image.image.metadata.header.build_id,
                content_hash: image.image.metadata.header.content_hash,
            },
            dependencies: &component.dependencies,
        });
    }
    let plan = plan_component_graph(&nodes).map_err(|error| match error {
        ComponentGraphError::Missing => status::COMPONENT_DEPENDENCY_MISSING,
        ComponentGraphError::Conflict => status::COMPONENT_DEPENDENCY_CONFLICT,
        ComponentGraphError::Cycle => status::COMPONENT_DEPENDENCY_CYCLE,
        ComponentGraphError::ResourceExhausted => status::CORE_RESOURCE_EXHAUSTED,
    })?;
    for (node_index, representative) in plan.representatives.iter().enumerate() {
        let mut dependencies = Vec::new();
        dependencies
            .try_reserve_exact(plan.dependencies[node_index].len())
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        dependencies.extend(
            plan.dependencies[node_index]
                .iter()
                .map(|dependency| plan.representatives[*dependency]),
        );
        images[*representative].dependencies = dependencies;
    }
    let mut order = Vec::new();
    order
        .try_reserve_exact(plan.topological_order.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    order.extend(
        plan.topological_order
            .iter()
            .map(|node| plan.representatives[*node]),
    );
    Ok(order)
}

fn component_key(image: &ImageObject) -> Result<ComponentKey, u32> {
    let component = image
        .metadata
        .component
        .as_ref()
        .ok_or(status::COMPONENT_INVALID_IMAGE)?;
    Ok(ComponentKey {
        component_id: component.info.component_id,
        abi_id: component.info.abi_id,
        build_id: image.metadata.header.build_id,
        content_hash: image.metadata.header.content_hash,
    })
}

fn install_component_capabilities(
    manager: &ComponentManager,
    bindings: &[PreparedComponentBinding],
) -> Result<(Vec<NativeHandle>, Vec<ComponentCapabilityRecord>), u32> {
    let mut installed = Vec::new();
    installed
        .try_reserve_exact(bindings.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(bindings.len())
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let mut handles = manager.handles.lock();
    for binding in bindings {
        let (object, interface) = {
            let source = handles.lookup(
                binding.source_handle,
                Some(binding.interface),
                binding.rights,
            )?;
            (source.object.clone(), source.interface)
        };
        let handle = match handles.insert(object, interface, binding.rights) {
            Ok(handle) => handle,
            Err(error) => {
                for handle in installed.drain(..) {
                    let _ = handles.close(handle);
                }
                return Err(error);
            }
        };
        installed.push(handle);
        records.push(ComponentCapabilityRecord {
            requirement_id: binding.requirement_id,
            handle: handle.raw(),
            granted_rights: binding.rights.bits(),
            ..ComponentCapabilityRecord::default()
        });
    }
    Ok((installed, records))
}

fn map_component(
    manager: &Arc<ComponentManager>,
    image: &Arc<ImageObject>,
    dependencies: Vec<Arc<ComponentObject>>,
    slots: &[u32],
    owned_slots: Vec<u32>,
    bindings: &[PreparedComponentBinding],
) -> Result<Arc<ComponentObject>, u32> {
    let key = component_key(image)?;
    let vm = &manager.vm;
    let tls = manager.reserve_tls(image)?;
    let image_size = usize::try_from(image.metadata.header.image_virtual_size)
        .map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
    let rw = VmFlags::EMPTY
        .with(VmFlags::USER)
        .with(VmFlags::READ)
        .with(VmFlags::WRITE);
    let image_range = vm
        .map_anon_any_aligned(image_size, native_abi::PAGE_SIZE as usize, rw)
        .map_err(|_| {
            manager.rollback_tls(&tls);
            status::CORE_RESOURCE_EXHAUSTED
        })?;
    let call_state_range = match vm.map_anon_any_aligned(
        native_abi::PAGE_SIZE as usize,
        native_abi::PAGE_SIZE as usize,
        rw,
    ) {
        Ok(range) => range,
        Err(_) => {
            let _ = vm.unmap(image_range.clone());
            manager.rollback_tls(&tls);
            return Err(status::CORE_RESOURCE_EXHAUSTED);
        }
    };
    let context_range = match vm.map_anon_any_aligned(
        native_abi::PAGE_SIZE as usize,
        native_abi::PAGE_SIZE as usize,
        rw,
    ) {
        Ok(range) => range,
        Err(_) => {
            let _ = vm.unmap(image_range.clone());
            let _ = vm.unmap(call_state_range.clone());
            manager.rollback_tls(&tls);
            return Err(status::CORE_RESOURCE_EXHAUSTED);
        }
    };
    let (capability_handles, capability_records) =
        match install_component_capabilities(manager, bindings) {
            Ok(capabilities) => capabilities,
            Err(error) => {
                let _ = vm.unmap(image_range);
                let _ = vm.unmap(call_state_range);
                let _ = vm.unmap(context_range);
                manager.rollback_tls(&tls);
                return Err(error);
            }
        };
    let result = populate_component(
        vm,
        image,
        &dependencies,
        slots,
        &image_range,
        &call_state_range,
        &context_range,
        &tls,
        &capability_records,
    );
    if let Err(error) = result {
        let _ = vm.unmap(image_range);
        let _ = vm.unmap(call_state_range);
        let _ = vm.unmap(context_range);
        manager.clear_tls(&tls);
        manager.rollback_tls(&tls);
        let mut handles = manager.handles.lock();
        for handle in capability_handles {
            let _ = handles.close(handle);
        }
        return Err(error);
    }
    let component = Arc::new(ComponentObject {
        key,
        metadata: Arc::clone(&image.metadata),
        vm: Arc::clone(vm),
        base: image_range.start,
        image_range,
        context_range,
        call_state_range,
        tls,
        owned_slots,
        lifecycle: Spinlock::new(ComponentLifecycleMachine::new()),
        dependencies,
        dependents: AtomicU32::new(0),
        interface_views: Spinlock::new(Vec::new()),
        marker_handle: AtomicU64::new(0),
        capability_handles,
        drain_waiters: WaitQueue::new(),
        threads: Spinlock::new(Vec::new()),
    });
    let marker_result = {
        manager.handles.lock().insert(
            KernelNativeObject::Component(Arc::clone(&component)),
            ObjectInterface::Component,
            Rights::NONE,
        )
    };
    let marker = match marker_result {
        Ok(handle) => handle,
        Err(error) => {
            let _ = vm.unmap(component.image_range.clone());
            let _ = vm.unmap(component.context_range.clone());
            manager.clear_tls(&component.tls);
            manager.rollback_tls(&component.tls);
            for handle in &component.capability_handles {
                let _ = manager.handles.lock().close(*handle);
            }
            return Err(error);
        }
    };
    component
        .marker_handle
        .store(marker.raw(), Ordering::Release);
    Ok(component)
}

fn populate_component(
    vm: &VmSpace,
    image: &ImageObject,
    dependencies: &[Arc<ComponentObject>],
    slots: &[u32],
    image_range: &Range<usize>,
    call_state_range: &Range<usize>,
    context_range: &Range<usize>,
    tls: &TlsAllocation,
    capabilities: &[ComponentCapabilityRecord],
) -> Result<(), u32> {
    let backing = image.file_backing();
    for (index, segment) in image.metadata.segments.iter().enumerate() {
        if segment.kind == SegmentKind::TlsTemplate || segment.file_size == 0 {
            continue;
        }
        let file_size =
            usize::try_from(segment.file_size).map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
        let target = image_range
            .start
            .checked_add(segment.virtual_offset as usize)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        if segment_requires_eager_backing(image, index) {
            let file_start = usize::try_from(segment.file_offset)
                .map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
            let file_end = file_start
                .checked_add(file_size)
                .ok_or(status::COMPONENT_INVALID_IMAGE)?;
            let payload = image
                .bytes()
                .get(file_start..file_end)
                .ok_or(status::COMPONENT_INVALID_IMAGE)?;
            write_vm_bytes(vm, target, payload)?;
        } else {
            let memory_size = usize::try_from(segment.memory_size)
                .map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
            vm.commit_file_segment_fixed(
                target,
                memory_size,
                segment.file_offset,
                file_size,
                Arc::clone(&backing),
                segment_vm_flags(segment.permissions),
            )
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        }
    }
    apply_image_relocations(vm, image_range.start, image, &image.metadata.relocations)?;
    apply_dynamic_relocations(
        vm,
        image_range.start,
        image,
        dependencies,
        slots,
        tls,
        &image
            .metadata
            .component
            .as_ref()
            .ok_or(status::COMPONENT_INVALID_IMAGE)?
            .dynamic_relocations,
    )?;
    let component = image
        .metadata
        .component
        .as_ref()
        .ok_or(status::COMPONENT_INVALID_IMAGE)?;
    let call_state = ComponentCallState {
        state: ComponentState::Preparing as u32,
        ..ComponentCallState::default()
    };
    write_vm_value(vm, call_state_range.start, &call_state)?;
    let capabilities_address = if capabilities.is_empty() {
        0
    } else {
        context_range
            .start
            .checked_add(size_of::<ComponentContext>())
            .ok_or(status::COMPONENT_INVALID_IMAGE)?
    };
    if !capabilities.is_empty() {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                capabilities.as_ptr().cast::<u8>(),
                core::mem::size_of_val(capabilities),
            )
        };
        if capabilities_address
            .checked_add(bytes.len())
            .is_none_or(|end| end > context_range.end)
        {
            return Err(status::COMPONENT_INVALID_IMAGE);
        }
        write_vm_bytes(vm, capabilities_address, bytes)?;
    }
    let context = ComponentContext {
        image_base: image_range.start as u64,
        call_state: call_state_range.start as u64,
        tls_base: tls.offset,
        tls_identity: tls.identity,
        call_slot_count: slots.len() as u32,
        interface_count: component.info.interface_count,
        capability_count: capabilities.len() as u32,
        capabilities: capabilities_address as u64,
        ..ComponentContext::default()
    };
    write_vm_value(vm, context_range.start, &context)?;
    vm.mprotect(image_range.clone(), VmFlags::EMPTY)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    for segment in &image.metadata.segments {
        if segment.kind == SegmentKind::TlsTemplate {
            continue;
        }
        let start = image_range
            .start
            .checked_add(segment.virtual_offset as usize)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        let length = align_up(segment.memory_size as usize, native_abi::PAGE_SIZE as usize)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        let mut flags = VmFlags::EMPTY;
        if segment.permissions & SegmentPermissions::READ.bits() != 0 {
            flags = flags.with(VmFlags::READ);
        }
        if segment.permissions & SegmentPermissions::WRITE.bits() != 0 {
            flags = flags.with(VmFlags::WRITE);
        }
        if segment.permissions & SegmentPermissions::EXECUTE.bits() != 0 {
            flags = flags.with(VmFlags::EXEC);
        }
        vm.mprotect(start..start + length, flags)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    }
    vm.mprotect(context_range.clone(), VmFlags::EMPTY.with(VmFlags::READ))
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    Ok(())
}

fn segment_requires_eager_backing(image: &ImageObject, index: usize) -> bool {
    image
        .metadata
        .relocations
        .iter()
        .any(|relocation| relocation.target_segment_index as usize == index)
        || image.metadata.component.as_ref().is_some_and(|component| {
            component
                .dynamic_relocations
                .iter()
                .any(|relocation| relocation.target_segment_index as usize == index)
        })
}

fn segment_vm_flags(permissions: u16) -> VmFlags {
    let mut flags = VmFlags::EMPTY.with(VmFlags::USER);
    if permissions & SegmentPermissions::READ.bits() != 0 {
        flags = flags.with(VmFlags::READ);
    }
    if permissions & SegmentPermissions::WRITE.bits() != 0 {
        flags = flags.with(VmFlags::WRITE);
    }
    if permissions & SegmentPermissions::EXECUTE.bits() != 0 {
        flags = flags.with(VmFlags::EXEC);
    }
    flags
}

fn apply_image_relocations(
    vm: &VmSpace,
    base: usize,
    image: &ImageObject,
    relocations: &[Relocation],
) -> Result<(), u32> {
    for relocation in relocations {
        let target_segment = image
            .metadata
            .segments
            .get(relocation.target_segment_index as usize)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        let target = base
            .checked_add(target_segment.virtual_offset as usize)
            .and_then(|value| value.checked_add(relocation.target_offset as usize))
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        let value = match relocation.kind {
            RelocationKind::ImageBase64 => base
                .checked_add(relocation.addend as usize)
                .ok_or(status::COMPONENT_INVALID_IMAGE)?,
            RelocationKind::SegmentBase64 => {
                let source = image
                    .metadata
                    .segments
                    .get(relocation.source_segment_index as usize)
                    .ok_or(status::COMPONENT_INVALID_IMAGE)?;
                base.checked_add(source.virtual_offset as usize)
                    .and_then(|value| value.checked_add(relocation.addend as usize))
                    .ok_or(status::COMPONENT_INVALID_IMAGE)?
            }
        } as u64;
        write_vm_bytes(vm, target, &value.to_le_bytes())?;
    }
    Ok(())
}

fn apply_dynamic_relocations(
    vm: &VmSpace,
    base: usize,
    image: &ImageObject,
    dependencies: &[Arc<ComponentObject>],
    slots: &[u32],
    tls: &TlsAllocation,
    relocations: &[DynamicRelocation],
) -> Result<(), u32> {
    let component = image
        .metadata
        .component
        .as_ref()
        .ok_or(status::COMPONENT_INVALID_IMAGE)?;
    for relocation in relocations {
        let target_segment = image
            .metadata
            .segments
            .get(relocation.target_segment_index as usize)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        let target = base
            .checked_add(target_segment.virtual_offset as usize)
            .and_then(|value| value.checked_add(relocation.target_offset as usize))
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        let raw = match relocation.kind {
            DynamicRelocationKind::AbiSlot32 | DynamicRelocationKind::AbiSlot64 => u64::from(
                *slots
                    .get(relocation.source_index as usize)
                    .ok_or(status::COMPONENT_INVALID_IMAGE)?,
            ),
            DynamicRelocationKind::InterfaceGate => {
                let import = component
                    .symbol_imports
                    .get(relocation.source_index as usize)
                    .ok_or(status::COMPONENT_INVALID_IMAGE)?;
                let dependency = dependencies
                    .get(import.dependency_index as usize)
                    .ok_or(status::COMPONENT_DEPENDENCY_MISSING)?;
                let export = dependency
                    .component()
                    .symbol_exports
                    .iter()
                    .find(|export| {
                        export.interface_id == import.interface_id
                            && export.symbol_id == import.symbol_id
                            && export.signature_hash == import.signature_hash
                    })
                    .ok_or(status::COMPONENT_DEPENDENCY_CONFLICT)?;
                let entry = dependency
                    .base
                    .checked_add(export.entry_offset as usize)
                    .ok_or(status::COMPONENT_INVALID_IMAGE)? as u64;
                let gate = ComponentInterfaceGate {
                    call_state: dependency.call_state_range.start as u64,
                    target: entry,
                    component: dependency.marker_handle()?.raw(),
                    generation: dependency.generation(),
                };
                write_vm_value(vm, target, &gate)?;
                continue;
            }
            DynamicRelocationKind::TlsOffset64 => {
                if tls.size == 0 {
                    return Err(status::COMPONENT_INVALID_IMAGE);
                }
                tls.offset
            }
        };
        let value = if relocation.addend >= 0 {
            raw.checked_add(relocation.addend as u64)
        } else {
            raw.checked_sub(relocation.addend.unsigned_abs())
        }
        .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        match relocation.kind {
            DynamicRelocationKind::AbiSlot32 => {
                let value = u32::try_from(value).map_err(|_| status::COMPONENT_INVALID_IMAGE)?;
                write_vm_bytes(vm, target, &value.to_le_bytes())?;
            }
            DynamicRelocationKind::InterfaceGate => unreachable!(),
            _ => write_vm_bytes(vm, target, &value.to_le_bytes())?,
        }
    }
    Ok(())
}

fn insert_component_handle(
    state: &NativeProcessState,
    component: Arc<ComponentObject>,
) -> Result<NativeHandle, u32> {
    state.handles.lock().insert(
        KernelNativeObject::Component(component),
        ObjectInterface::Component,
        Rights::INSPECT | Rights::BIND | Rights::UNLOAD | Rights::DUPLICATE,
    )
}

fn replace_transaction_with_component_handle(
    state: &NativeProcessState,
    transaction: &Arc<ComponentTransaction>,
    transaction_handle: NativeHandle,
    component: Arc<ComponentObject>,
) -> Result<NativeHandle, u32> {
    let mut handles = state.handles.lock();
    let removed = handles.close(transaction_handle)?;
    let KernelNativeObject::ComponentTransaction(removed) = removed else {
        return Err(status::COMPONENT_INVALID_TRANSACTION);
    };
    if !Arc::ptr_eq(&removed, transaction) {
        return Err(status::COMPONENT_INVALID_TRANSACTION);
    }
    handles.insert(
        KernelNativeObject::Component(component),
        ObjectInterface::Component,
        Rights::INSPECT | Rights::BIND | Rights::UNLOAD | Rights::DUPLICATE,
    )
}

fn read_active_calls(component: &ComponentObject) -> Result<u64, u32> {
    let vm = component_context_vm(component);
    let mut bytes = [0; 8];
    vm.copy_user_bytes_in(
        component.call_state_range.start + wire::component_call_state::ACTIVE_CALLS,
        &mut bytes,
    )
    .map_err(|_| status::COMPONENT_INVALID_TRANSACTION)?;
    Ok(u64::from_ne_bytes(bytes))
}

fn write_call_state(
    vm: &VmSpace,
    address: usize,
    state: ComponentState,
    generation: u64,
) -> Result<(), u32> {
    write_vm_bytes(
        vm,
        address + wire::component_call_state::STATE,
        &(state as u32).to_ne_bytes(),
    )?;
    write_vm_bytes(
        vm,
        address + wire::component_call_state::GENERATION,
        &generation.to_ne_bytes(),
    )
}

fn copy_user_value<T: Copy + Default>(user: u64) -> Result<T, u32> {
    let user = usize::try_from(user).map_err(|_| status::STREAM_FAULT)?;
    if user == 0 {
        return Err(status::STREAM_FAULT);
    }
    let mut value = T::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>())
    };
    copy_from_user(user, bytes).map_err(|_| status::STREAM_FAULT)?;
    Ok(value)
}

fn write_user_value<T: Copy>(user: u64, value: &T) -> Result<(), u32> {
    let user = usize::try_from(user).map_err(|_| status::STREAM_FAULT)?;
    if user == 0 {
        return Err(status::STREAM_FAULT);
    }
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user(user, bytes).map_err(|_| status::STREAM_FAULT)
}

fn write_vm_value<T: Copy>(vm: &VmSpace, user: usize, value: &T) -> Result<(), u32> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    write_vm_bytes(vm, user, bytes)
}

fn write_vm_bytes(vm: &VmSpace, mut user: usize, mut bytes: &[u8]) -> Result<(), u32> {
    while !bytes.is_empty() {
        let copied = unsafe {
            vm.with_user_write_slice(user, bytes.len(), |target| {
                target.copy_from_slice(&bytes[..target.len()]);
                target.len()
            })
        }
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        user = user
            .checked_add(copied)
            .ok_or(status::COMPONENT_INVALID_IMAGE)?;
        bytes = &bytes[copied..];
    }
    Ok(())
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

impl InterfaceObject {
    pub(crate) fn component(&self) -> &Arc<ComponentObject> {
        &self.component
    }

    pub(crate) const fn vtable(&self) -> usize {
        self.vtable
    }
}
