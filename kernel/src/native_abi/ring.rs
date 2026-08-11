//! Native SubmissionRing：固定 descriptor 的批量控制面。

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use general::mm::VmSpace;
use general::syscall::{NativeCallOutcome, NativeCallReturn};
use mm::{SharedAnonObject, VmFlags};
use native_abi::wire::{CompletionRecord, RingInfo, RingSharedState, SubmissionDescriptor};
use native_abi::{
    NativeHandle, ObjectInterface, OperationId, Rights, SubmissionMode, status, wire,
};
use sched::sync::Spinlock;
use sched::{Task, TaskState, WaitQueue};
use vfs::file::PollEvents;
use vfs::poll_source::{PollSource, PollSubscriber};

use super::dispatch::native_return;
use super::memory::MemoryObject;
use super::operations::PinnedNativeHandle;
use super::{KernelNativeObject, NativeProcessState};

#[derive(Clone)]
struct Registration {
    token: u64,
    memory: Arc<MemoryObject>,
    offset: u64,
    length: u64,
    rights: Rights,
    memory_generation: u64,
    in_use: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InFlightPhase {
    Queued,
    Running,
}

struct InFlight {
    user_data: u64,
    phase: InFlightPhase,
}

struct PreparedCall {
    descriptor: SubmissionDescriptor,
    operation: OperationId,
    pinned: PinnedNativeHandle,
    memory: Option<(Arc<MemoryObject>, u64, u64)>,
    address: Option<Arc<MemoryObject>>,
    address_offset: u64,
    registration_tokens: [u64; 2],
    registration_count: u8,
    buffer: Vec<u8>,
    observer: Arc<RingRequestObserver>,
    subscription: Option<u64>,
}

struct RingState {
    capacity: usize,
    registrations: Vec<Registration>,
    deferred_completions: VecDeque<CompletionRecord>,
    pending: VecDeque<PreparedCall>,
    in_flight: Vec<InFlight>,
    reserved_completions: usize,
    next_registration: u64,
    generation: u64,
    completed: u64,
    cancelled: u64,
    #[cfg(feature = "kernel-tests")]
    worker_paused: bool,
}

pub(crate) struct SubmissionRingObject {
    state: Spinlock<RingState>,
    capacity: usize,
    vm: Arc<VmSpace>,
    mapping: Range<usize>,
    _backing: Arc<SharedAnonObject>,
    sq_offset: usize,
    cq_offset: usize,
    waiters: WaitQueue,
    wait_claimed: AtomicBool,
    submit_claimed: AtomicBool,
    poll_source: PollSource,
    worker: Arc<RingWorkerSignal>,
}

struct RingWorkerSignal {
    ring: Weak<SubmissionRingObject>,
    waiters: WaitQueue,
    notified: AtomicBool,
    stopped: AtomicBool,
}

struct RingRequestObserver {
    worker: Weak<RingWorkerSignal>,
    interest: PollEvents,
    source: AtomicU64,
    generation: AtomicU64,
    ready: AtomicBool,
}

struct RingWaitClaim<'a> {
    claimed: &'a AtomicBool,
}

struct RingSubmitClaim<'a> {
    claimed: &'a AtomicBool,
}

impl Drop for RingWaitClaim<'_> {
    fn drop(&mut self) {
        self.claimed.store(false, Ordering::Release);
    }
}

impl Drop for RingSubmitClaim<'_> {
    fn drop(&mut self) {
        self.claimed.store(false, Ordering::Release);
    }
}

impl RingWorkerSignal {
    fn notify(&self) {
        self.notified.store(true, Ordering::Release);
        self.waiters.wake_all();
    }
}

impl PollSubscriber for RingRequestObserver {
    fn readiness_changed(&self, source: u64, readiness: PollEvents, generation: u64) {
        let expected = self.source.load(Ordering::Acquire);
        if expected != 0 && expected != source {
            return;
        }
        let previous = self.generation.fetch_max(generation, Ordering::AcqRel);
        if generation < previous {
            return;
        }
        let terminal = PollEvents::POLLERR
            .with(PollEvents::POLLHUP)
            .with(PollEvents::POLLRDHUP);
        if readiness.intersect(self.interest.with(terminal)).is_empty() {
            return;
        }
        self.ready.store(true, Ordering::Release);
        if let Some(worker) = self.worker.upgrade() {
            worker.notify();
        }
    }
}

impl PreparedCall {
    fn poll_source(&self) -> Option<&PollSource> {
        match &self.pinned.object {
            KernelNativeObject::Stream(file) => file.poll_source(),
            KernelNativeObject::File(file) => file.file.poll_source(),
            KernelNativeObject::Channel(channel) => Some(channel.poll_source()),
            KernelNativeObject::Socket(socket) => socket.poll_source(),
            _ => None,
        }
    }

    fn clear_subscription(&mut self) {
        let Some(subscription) = self.subscription.take() else {
            return;
        };
        if let Some(source) = self.poll_source() {
            source.unsubscribe(subscription);
        }
        self.observer.source.store(0, Ordering::Release);
        self.observer.ready.store(false, Ordering::Release);
    }
}

impl Drop for PreparedCall {
    fn drop(&mut self) {
        self.clear_subscription();
    }
}

impl Drop for SubmissionRingObject {
    fn drop(&mut self) {
        self.worker.stopped.store(true, Ordering::Release);
        self.worker.notify();
        let _ = self.vm.unmap_existing(self.mapping.clone());
    }
}

impl SubmissionRingObject {
    pub(crate) fn poll_source(&self) -> &PollSource {
        self.refresh_completion_readiness();
        &self.poll_source
    }

    fn refresh_completion_readiness(&self) {
        let readiness = match available_completions(self) {
            Ok(0) => PollEvents::default(),
            Ok(_) => PollEvents::POLLIN,
            Err(_) => PollEvents::POLLERR,
        };
        let version = self.poll_source.reserve_version();
        self.poll_source.publish_versioned(readiness, version);
    }

    fn claim_wait(&self) -> Option<RingWaitClaim<'_>> {
        self.wait_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RingWaitClaim {
                claimed: &self.wait_claimed,
            })
    }

    fn claim_submit(&self) -> Option<RingSubmitClaim<'_>> {
        self.submit_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RingSubmitClaim {
                claimed: &self.submit_claimed,
            })
    }

    fn shared_address(&self, offset: usize) -> Result<usize, u32> {
        self.mapping
            .start
            .checked_add(offset)
            .filter(|address| *address < self.mapping.end)
            .ok_or(status::RING_INVALID_DESCRIPTOR)
    }

    fn read_index(&self, offset: usize) -> Result<u32, u32> {
        let address = self.shared_address(offset)?;
        self.vm
            .read_user_u32_nofault(address)
            .map_err(|_| status::RING_INVALID_DESCRIPTOR)
    }

    fn write_index(&self, offset: usize, value: u32) -> Result<(), u32> {
        let address = self.shared_address(offset)?;
        self.vm
            .store_user_u32_nofault(address, value)
            .map_err(|_| status::RING_INVALID_DESCRIPTOR)
    }

    fn queue_len(&self, head_offset: usize, tail_offset: usize) -> Result<(u32, u32, u32), u32> {
        let head = self.read_index(head_offset)?;
        let tail = self.read_index(tail_offset)?;
        let queued = wire::ring_queue_len(head, tail, self.capacity as u32)
            .ok_or(status::RING_INVALID_DESCRIPTOR)?;
        Ok((head, tail, queued))
    }

    fn read_shared<T: Copy + Default>(&self, offset: usize) -> Result<T, u32> {
        let address = self.shared_address(offset)?;
        let mut value = T::default();
        let output = unsafe {
            core::slice::from_raw_parts_mut(
                (&mut value as *mut T).cast::<u8>(),
                core::mem::size_of::<T>(),
            )
        };
        self.vm
            .copy_user_bytes_in(address, output)
            .map_err(|_| status::RING_INVALID_DESCRIPTOR)?;
        Ok(value)
    }

    fn write_shared<T: Copy>(&self, offset: usize, value: &T) -> Result<(), u32> {
        let address = self.shared_address(offset)?;
        let input = unsafe {
            core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
        };
        self.vm
            .copy_user_bytes_out(address, input)
            .map_err(|_| status::RING_INVALID_DESCRIPTOR)
    }

    fn submission(&self, position: u32) -> Result<SubmissionDescriptor, u32> {
        let index = (position as usize) & (self.capacity - 1);
        let offset = self
            .sq_offset
            .checked_add(index * core::mem::size_of::<SubmissionDescriptor>())
            .ok_or(status::RING_INVALID_DESCRIPTOR)?;
        self.read_shared(offset)
    }

    fn completion(&self, position: u32) -> Result<CompletionRecord, u32> {
        let index = (position as usize) & (self.capacity - 1);
        let offset = self
            .cq_offset
            .checked_add(index * core::mem::size_of::<CompletionRecord>())
            .ok_or(status::RING_INVALID_DESCRIPTOR)?;
        self.read_shared(offset)
    }

    fn write_completion(&self, position: u32, completion: &CompletionRecord) -> Result<(), u32> {
        let index = (position as usize) & (self.capacity - 1);
        let offset = self
            .cq_offset
            .checked_add(index * core::mem::size_of::<CompletionRecord>())
            .ok_or(status::RING_INVALID_DESCRIPTOR)?;
        self.write_shared(offset, completion)
    }

    fn publish_completion_state(&self, version: u64, ready: bool) {
        let readiness = if ready {
            PollEvents::POLLIN
        } else {
            PollEvents::default()
        };
        self.poll_source.publish_versioned(readiness, version);
        if ready {
            self.waiters.wake_all();
        }
    }
}

pub(super) fn ring_create(
    task: &Arc<Task>,
    state: &NativeProcessState,
    object: &KernelNativeObject,
    entries: u64,
) -> NativeCallOutcome {
    if !matches!(object, KernelNativeObject::SelfProcess)
        || entries < 2
        || entries > wire::MAX_RING_ENTRIES as u64
        || !entries.is_power_of_two()
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let entries = entries as usize;
    let mut registrations = Vec::new();
    if registrations.try_reserve_exact(entries).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let mut pending = VecDeque::new();
    if pending.try_reserve_exact(entries).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let mut deferred_completions = VecDeque::new();
    if deferred_completions.try_reserve_exact(entries).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let mut in_flight = Vec::new();
    if in_flight.try_reserve_exact(entries).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let Some((vm, mapping, backing, sq_offset, cq_offset)) = prepare_shared_ring(task, entries)
    else {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    };
    let shared_base = mapping.start as u64;
    let ring = Arc::new_cyclic(|ring| SubmissionRingObject {
        state: Spinlock::new(RingState {
            capacity: entries,
            registrations,
            deferred_completions,
            pending,
            in_flight,
            reserved_completions: 0,
            next_registration: 1,
            generation: 1,
            completed: 0,
            cancelled: 0,
            #[cfg(feature = "kernel-tests")]
            worker_paused: false,
        }),
        capacity: entries,
        vm,
        mapping,
        _backing: backing,
        sq_offset,
        cq_offset,
        waiters: WaitQueue::new_with_reason(sched::WaitReason::Poll),
        wait_claimed: AtomicBool::new(false),
        submit_claimed: AtomicBool::new(false),
        poll_source: PollSource::new(PollEvents::default()),
        worker: Arc::new(RingWorkerSignal {
            ring: ring.clone(),
            waiters: WaitQueue::new_with_reason(sched::WaitReason::BlockIo),
            notified: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }),
    });
    let worker_arg = Arc::into_raw(Arc::clone(&ring.worker)) as usize;
    let worker_task = sched::kthread_create(
        ring_worker,
        worker_arg,
        sched::SchedParams {
            nice: 10,
            slice_ns: 0,
        },
    );
    if worker_task.state() == TaskState::Dead || sched::activate_task(&worker_task).is_err() {
        sched::abort_new_task(&worker_task);
        unsafe {
            drop(Arc::from_raw(worker_arg as *const RingWorkerSignal));
        }
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    match state.handles.lock().insert(
        KernelNativeObject::SubmissionRing(ring),
        ObjectInterface::SubmissionRing,
        Rights::REGISTER
            | Rights::SUBMIT
            | Rights::CANCEL
            | Rights::OBSERVE
            | Rights::INSPECT
            | Rights::DUPLICATE,
    ) {
        Ok(handle) => native_return(status::OK, handle.raw(), shared_base),
        Err(error) => native_return(error, 0, 0),
    }
}

fn prepare_shared_ring(
    task: &Arc<Task>,
    entries: usize,
) -> Option<(
    Arc<VmSpace>,
    Range<usize>,
    Arc<SharedAnonObject>,
    usize,
    usize,
)> {
    let vm = task
        .ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())?;
    let page = general::mm::page_size();
    let sq_offset = page;
    let sq_bytes = entries.checked_mul(core::mem::size_of::<SubmissionDescriptor>())?;
    let cq_offset = sq_offset.checked_add(sq_bytes)?.next_multiple_of(page);
    let cq_bytes = entries.checked_mul(core::mem::size_of::<CompletionRecord>())?;
    let length = cq_offset.checked_add(cq_bytes)?.next_multiple_of(page);
    let backing = Arc::new(SharedAnonObject::new());
    let flags =
        VmFlags::from_bits(VmFlags::USER | VmFlags::READ | VmFlags::WRITE | VmFlags::SHARED);
    let mapping = vm
        .map_shared_anon_any_aligned(length, page, Arc::clone(&backing), 0, flags)
        .ok()?;
    if vm.prefault_user_range(mapping.clone(), true).is_err() {
        let _ = vm.unmap_existing(mapping);
        return None;
    }
    let header = RingSharedState {
        magic: wire::RING_SHARED_MAGIC,
        version: wire::RING_SHARED_VERSION,
        flags: 0,
        entries: entries as u32,
        mask: entries as u32 - 1,
        sq_head: 0,
        sq_tail: 0,
        cq_head: 0,
        cq_tail: 0,
        sq_offset: sq_offset as u64,
        cq_offset: cq_offset as u64,
        generation: 1,
        reserved: 0,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&header as *const RingSharedState).cast::<u8>(),
            core::mem::size_of::<RingSharedState>(),
        )
    };
    if general::mm::write_shared_anon(&backing, 0, bytes).is_err() {
        let _ = vm.unmap_existing(mapping);
        return None;
    }
    Some((vm, mapping, backing, sq_offset, cq_offset))
}

pub(super) fn ring_register(
    state: &NativeProcessState,
    ring: &SubmissionRingObject,
    memory_raw: u64,
    offset: u64,
    length: u64,
) -> NativeCallOutcome {
    if length == 0 || offset % native_abi::PAGE_SIZE != 0 || length % native_abi::PAGE_SIZE != 0 {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    }
    let memory = NativeHandle::from_raw(memory_raw);
    let (object, rights) = {
        let handles = state.handles.lock();
        let entry = match handles.lookup(memory, Some(ObjectInterface::MemoryObject), Rights::MAP) {
            Ok(entry) => entry,
            Err(error) => return native_return(error, 0, 0),
        };
        let KernelNativeObject::MemoryObject(object) = entry.object else {
            return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
        };
        if offset
            .checked_add(length)
            .is_none_or(|end| end > object.size())
        {
            return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
        }
        (Arc::clone(object), entry.rights)
    };
    let memory_generation = match object.active_generation() {
        Ok(generation) => generation,
        Err(error) => return native_return(error, 0, 0),
    };
    let mut state = ring.state.lock();
    if state.registrations.len() >= state.capacity {
        return native_return(status::RING_FULL, 0, 0);
    }
    if state.registrations.try_reserve(1).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let token = state.next_registration;
    state.next_registration = state.next_registration.wrapping_add(1).max(1);
    state.registrations.push(Registration {
        token,
        memory: object,
        offset,
        length,
        rights,
        memory_generation,
        in_use: 0,
    });
    native_return(status::OK, token, 0)
}

pub(super) fn ring_unregister(ring: &SubmissionRingObject, token: u64) -> NativeCallOutcome {
    let mut state = ring.state.lock();
    let Some(index) = state
        .registrations
        .iter()
        .position(|registration| registration.token == token)
    else {
        return native_return(status::RING_TOKEN_STALE, 0, 0);
    };
    if state.registrations[index].in_use != 0 {
        return native_return(status::RING_BUSY, 0, 0);
    }
    state.registrations.swap_remove(index);
    native_return(status::OK, 0, 0)
}

pub(super) fn ring_kick(
    state: &Arc<NativeProcessState>,
    ring: &SubmissionRingObject,
    count: u64,
) -> NativeCallOutcome {
    if count == 0 || count > wire::MAX_RING_BATCH as u64 {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let Some(_claim) = ring.claim_submit() else {
        return native_return(status::RING_BUSY, 0, 0);
    };
    let (sq_head, sq_tail, queued) = match ring.queue_len(
        wire::ring_shared_state::SQ_HEAD,
        wire::ring_shared_state::SQ_TAIL,
    ) {
        Ok(state) => state,
        Err(error) => return native_return(error, 0, 0),
    };
    if queued != count as u32 {
        return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
    }
    let mut descriptors = Vec::new();
    if descriptors.try_reserve_exact(queued as usize).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    for index in 0..queued {
        match ring.submission(sq_head.wrapping_add(index)) {
            Ok(descriptor) => descriptors.push(descriptor),
            Err(error) => return native_return(error, 0, 0),
        }
    }

    struct PinnedCall {
        descriptor: SubmissionDescriptor,
        operation: OperationId,
        pinned: PinnedNativeHandle,
        buffer: Vec<u8>,
        observer: Arc<RingRequestObserver>,
    }

    let mut pinned_calls = Vec::new();
    if pinned_calls.try_reserve_exact(descriptors.len()).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    for descriptor in descriptors {
        if descriptor.user_data == 0 || descriptor.slot > u32::MAX as u64 {
            return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
        }
        let Some(binding) = state
            .binding
            .call_slots
            .get(descriptor.slot as usize)
            .copied()
            .or_else(|| state.components.resolve_slot(descriptor.slot as usize))
        else {
            return native_return(status::ABI_BAD_SLOT, 0, 0);
        };
        let Some(operation) = binding.operation else {
            return native_return(status::ABI_UNSUPPORTED_OPERATION, 0, 0);
        };
        if native_abi::operation(operation)
            .is_none_or(|spec| spec.submission() == SubmissionMode::DirectOnly)
        {
            return native_return(status::RING_UNSUPPORTED, 0, 0);
        }
        let handle = NativeHandle::from_raw(descriptor.handle);
        let pinned = {
            let handles = state.handles.lock();
            let entry = match handles.lookup(handle, binding.interface, binding.required_rights) {
                Ok(entry) => entry,
                Err(error) => return native_return(error, 0, 0),
            };
            PinnedNativeHandle {
                object: entry.object.clone(),
                interface: entry.interface,
                rights: entry.rights,
            }
        };
        let buffer_length = if operation_uses_memory(operation) {
            let Some(buffer_length) = operation_buffer_length(operation, &descriptor) else {
                return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
            };
            if buffer_length > u64::from(wire::MAX_RING_IO_BYTES) {
                return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
            }
            buffer_length as usize
        } else {
            0
        };
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(buffer_length).is_err() {
            return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
        }
        buffer.resize(buffer_length, 0);
        pinned_calls.push(PinnedCall {
            descriptor,
            operation,
            pinned,
            buffer,
            observer: Arc::new(RingRequestObserver {
                worker: Arc::downgrade(&ring.worker),
                interest: operation_interest(operation),
                source: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                ready: AtomicBool::new(false),
            }),
        });
    }

    let mut prepared = Vec::new();
    if prepared.try_reserve_exact(pinned_calls.len()).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let mut ring_state = ring.state.lock();
    let current_sq = match ring.queue_len(
        wire::ring_shared_state::SQ_HEAD,
        wire::ring_shared_state::SQ_TAIL,
    ) {
        Ok(current) => current,
        Err(error) => return native_return(error, 0, 0),
    };
    if current_sq != (sq_head, sq_tail, queued) {
        return native_return(status::RING_BUSY, 0, 0);
    }
    let (cq_head, _cq_tail, cq_queued) = match ring.queue_len(
        wire::ring_shared_state::CQ_HEAD,
        wire::ring_shared_state::CQ_TAIL,
    ) {
        Ok(current) => current,
        Err(error) => return native_return(error, 0, 0),
    };
    if cq_queued as usize
        + ring_state.deferred_completions.len()
        + ring_state.reserved_completions
        + pinned_calls.len()
        > ring_state.capacity
    {
        return native_return(status::RING_FULL, 0, 0);
    }
    for pinned_call in pinned_calls {
        let descriptor = pinned_call.descriptor;
        if prepared
            .iter()
            .any(|candidate: &PreparedCall| candidate.descriptor.user_data == descriptor.user_data)
            || match completion_contains(ring, cq_head, cq_queued, descriptor.user_data) {
                Ok(found) => found,
                Err(error) => return native_return(error, 0, 0),
            }
            || ring_state
                .deferred_completions
                .iter()
                .any(|entry| entry.user_data == descriptor.user_data)
            || ring_state
                .in_flight
                .iter()
                .any(|entry| entry.user_data == descriptor.user_data)
        {
            return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
        }
        let operation = pinned_call.operation;
        let (memory, address, address_offset, registration_tokens, registration_count) =
            match operation {
                OperationId::ClockRead => {
                    if descriptor.arg0 != 0
                        || descriptor.arg1 != 0
                        || descriptor.arg2 != 0
                        || descriptor.arg3 != 0
                        || descriptor.arg4 != 0
                    {
                        return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
                    }
                    (None, None, 0, [0; 2], 0)
                }
                OperationId::StreamRead
                | OperationId::StreamWrite
                | OperationId::FileRead
                | OperationId::FileWrite
                | OperationId::SocketSend
                | OperationId::SocketReceive => {
                    let required = if matches!(
                        operation,
                        OperationId::StreamRead
                            | OperationId::FileRead
                            | OperationId::SocketReceive
                    ) {
                        Rights::WRITE
                    } else {
                        Rights::READ
                    };
                    let Some(registration) = ring_state
                        .registrations
                        .iter()
                        .find(|registration| registration.token == descriptor.arg0)
                    else {
                        return native_return(status::RING_TOKEN_STALE, 0, 0);
                    };
                    if !required.is_subset_of(registration.rights)
                        || registration.memory_generation != registration.memory.generation()
                        || descriptor
                            .arg1
                            .checked_add(descriptor.arg2)
                            .is_none_or(|end| end > registration.length)
                        || (matches!(
                            operation,
                            OperationId::StreamRead | OperationId::StreamWrite
                        ) && (descriptor.arg3 != 0 || descriptor.arg4 != 0))
                        || (matches!(operation, OperationId::FileRead | OperationId::FileWrite)
                            && descriptor.arg4 != 0)
                    {
                        return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
                    }
                    let Some(object_offset) = registration.offset.checked_add(descriptor.arg1)
                    else {
                        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                    };
                    let address_registration = if matches!(
                        operation,
                        OperationId::SocketSend | OperationId::SocketReceive
                    ) && descriptor.arg3 != 0
                    {
                        let Some(address) = ring_state
                            .registrations
                            .iter()
                            .find(|registration| registration.token == descriptor.arg3)
                        else {
                            return native_return(status::RING_TOKEN_STALE, 0, 0);
                        };
                        let address_right = if operation == OperationId::SocketSend {
                            Rights::READ
                        } else {
                            Rights::WRITE
                        };
                        if !address_right.is_subset_of(address.rights)
                            || address.memory_generation != address.memory.generation()
                            || address.length
                                < core::mem::size_of::<native_abi::wire::NetworkAddress>() as u64
                        {
                            return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
                        }
                        Some(address.clone())
                    } else {
                        None
                    };
                    if matches!(
                        operation,
                        OperationId::SocketSend | OperationId::SocketReceive
                    ) {
                        (
                            Some((
                                Arc::clone(&registration.memory),
                                object_offset,
                                descriptor.arg2,
                            )),
                            address_registration
                                .as_ref()
                                .map(|address| Arc::clone(&address.memory)),
                            address_registration
                                .as_ref()
                                .map_or(0, |address| address.offset),
                            [descriptor.arg0, descriptor.arg3],
                            if address_registration.is_some() { 2 } else { 1 },
                        )
                    } else {
                        (
                            Some((
                                Arc::clone(&registration.memory),
                                object_offset,
                                descriptor.arg2,
                            )),
                            None,
                            0,
                            [descriptor.arg0, 0],
                            1,
                        )
                    }
                }
                OperationId::ChannelSend | OperationId::ChannelReceive => {
                    let required = if operation == OperationId::ChannelSend {
                        Rights::READ
                    } else {
                        Rights::WRITE
                    };
                    let Some(registration) = ring_state
                        .registrations
                        .iter()
                        .find(|registration| registration.token == descriptor.arg0)
                    else {
                        return native_return(status::RING_TOKEN_STALE, 0, 0);
                    };
                    if !required.is_subset_of(registration.rights)
                        || registration.memory_generation != registration.memory.generation()
                        || descriptor
                            .arg1
                            .checked_add(descriptor.arg2)
                            .is_none_or(|end| end > registration.length)
                        || descriptor.arg3 != 0
                        || descriptor.arg4 != 0
                    {
                        return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
                    }
                    let Some(object_offset) = registration.offset.checked_add(descriptor.arg1)
                    else {
                        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
                    };
                    (
                        Some((
                            Arc::clone(&registration.memory),
                            object_offset,
                            descriptor.arg2,
                        )),
                        None,
                        0,
                        [descriptor.arg0, 0],
                        1,
                    )
                }
                OperationId::DeviceInvoke => {
                    if descriptor.arg0 > u32::MAX as u64
                        || (descriptor.arg1 == 0) != (descriptor.arg3 == 0)
                        || (descriptor.arg2 == 0) != (descriptor.arg4 == 0)
                    {
                        return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
                    }
                    let input = if descriptor.arg1 == 0 {
                        None
                    } else {
                        let Some(registration) = ring_state
                            .registrations
                            .iter()
                            .find(|registration| registration.token == descriptor.arg1)
                            .cloned()
                        else {
                            return native_return(status::RING_TOKEN_STALE, 0, 0);
                        };
                        if !Rights::READ.is_subset_of(registration.rights)
                            || registration.memory_generation != registration.memory.generation()
                            || descriptor.arg3 > registration.length
                        {
                            return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
                        }
                        Some(registration)
                    };
                    let output = if descriptor.arg2 == 0 {
                        None
                    } else {
                        let Some(registration) = ring_state
                            .registrations
                            .iter()
                            .find(|registration| registration.token == descriptor.arg2)
                            .cloned()
                        else {
                            return native_return(status::RING_TOKEN_STALE, 0, 0);
                        };
                        if !Rights::WRITE.is_subset_of(registration.rights)
                            || registration.memory_generation != registration.memory.generation()
                            || descriptor.arg4 > registration.length
                        {
                            return native_return(status::RING_INVALID_DESCRIPTOR, 0, 0);
                        }
                        Some(registration)
                    };
                    let (registration_tokens, registration_count) = match (&input, &output) {
                        (Some(input), Some(output)) => ([input.token, output.token], 2),
                        (Some(input), None) => ([input.token, 0], 1),
                        (None, Some(output)) => ([output.token, 0], 1),
                        (None, None) => ([0; 2], 0),
                    };
                    (
                        input.as_ref().map(|registration| {
                            (
                                Arc::clone(&registration.memory),
                                registration.offset,
                                descriptor.arg3,
                            )
                        }),
                        output
                            .as_ref()
                            .map(|registration| Arc::clone(&registration.memory)),
                        output
                            .as_ref()
                            .map_or(0, |registration| registration.offset),
                        registration_tokens,
                        registration_count,
                    )
                }
                _ => (None, None, 0, [0; 2], 0),
            };
        let mut call = PreparedCall {
            descriptor,
            operation,
            pinned: pinned_call.pinned,
            memory,
            address,
            address_offset,
            registration_tokens,
            registration_count,
            buffer: pinned_call.buffer,
            observer: pinned_call.observer,
            subscription: None,
        };
        if let Err(error) = subscribe_call(&mut call) {
            return native_return(error, 0, 0);
        }
        prepared.push(call);
    }
    if let Err(error) = ring.write_index(wire::ring_shared_state::SQ_HEAD, sq_tail) {
        return native_return(error, 0, 0);
    }
    for call in prepared {
        for token in call.registration_tokens[..call.registration_count as usize]
            .iter()
            .copied()
        {
            let registration = ring_state
                .registrations
                .iter_mut()
                .find(|registration| registration.token == token)
                .expect("已验证的 Ring registration 必须仍存在");
            registration.in_use = registration.in_use.saturating_add(1);
        }
        ring_state.in_flight.push(InFlight {
            user_data: call.descriptor.user_data,
            phase: InFlightPhase::Queued,
        });
        ring_state.pending.push_back(call);
    }
    ring_state.reserved_completions += count as usize;
    drop(ring_state);
    ring.worker.notify();
    native_return(status::OK, count, 0)
}

fn completion_contains(
    ring: &SubmissionRingObject,
    head: u32,
    count: u32,
    user_data: u64,
) -> Result<bool, u32> {
    for index in 0..count {
        let completion = ring.completion(head.wrapping_add(index))?;
        if completion.reserved != 0 {
            return Err(status::RING_INVALID_DESCRIPTOR);
        }
        if completion.user_data == user_data {
            return Ok(true);
        }
    }
    Ok(false)
}

fn execute_prepared_call(call: &mut PreparedCall) -> NativeCallReturn {
    let unsupported = || NativeCallReturn {
        status: status::RING_UNSUPPORTED,
        value0: 0,
        value1: 0,
    };
    let outcome = match call.operation {
        OperationId::ClockRead => {
            if !matches!(call.pinned.object, KernelNativeObject::MonotonicClock) {
                native_return(status::HANDLE_WRONG_INTERFACE, 0, 0)
            } else {
                native_return(status::OK, hal::time::monotonic_ns(), 0)
            }
        }
        OperationId::StreamRead | OperationId::StreamWrite => {
            let KernelNativeObject::Stream(file) = &call.pinned.object else {
                return unsupported();
            };
            let Some((memory, offset, length)) = call.memory.as_ref() else {
                return unsupported();
            };
            debug_assert_eq!(call.buffer.len(), *length as usize);
            if call.operation == OperationId::StreamRead {
                super::operations::stream_read_memory_buffered(
                    file,
                    memory,
                    *offset,
                    &mut call.buffer,
                )
            } else {
                super::operations::stream_write_memory_buffered(
                    file,
                    memory,
                    *offset,
                    &mut call.buffer,
                )
            }
        }
        OperationId::FileRead | OperationId::FileWrite => {
            let KernelNativeObject::File(file) = &call.pinned.object else {
                return unsupported();
            };
            let Some((memory, offset, length)) = call.memory.as_ref() else {
                return unsupported();
            };
            debug_assert_eq!(call.buffer.len(), *length as usize);
            if call.operation == OperationId::FileRead {
                super::fs::file_read_memory_buffered(
                    file,
                    memory,
                    *offset,
                    call.descriptor.arg3,
                    &mut call.buffer,
                )
            } else {
                super::fs::file_write_memory_buffered(
                    file,
                    memory,
                    *offset,
                    call.descriptor.arg3,
                    &mut call.buffer,
                )
            }
        }
        OperationId::ChannelSend | OperationId::ChannelReceive => {
            let KernelNativeObject::Channel(channel) = &call.pinned.object else {
                return unsupported();
            };
            let Some((memory, offset, length)) = call.memory.as_ref() else {
                return unsupported();
            };
            if call.operation == OperationId::ChannelSend {
                super::channel::channel_send_memory_buffered(
                    channel,
                    memory,
                    *offset,
                    &mut call.buffer,
                )
            } else {
                super::channel::channel_receive_memory(channel, memory, *offset, *length)
            }
        }
        OperationId::SocketSend | OperationId::SocketReceive => {
            let KernelNativeObject::Socket(socket) = &call.pinned.object else {
                return unsupported();
            };
            let Some((memory, offset, length)) = call.memory.as_ref() else {
                return unsupported();
            };
            let address = call
                .address
                .as_deref()
                .map(|address| (address, call.address_offset));
            if call.operation == OperationId::SocketSend {
                super::socket::socket_send_memory_buffered(
                    socket,
                    memory,
                    *offset,
                    address,
                    &mut call.buffer,
                )
            } else {
                super::socket::socket_receive_memory_buffered(
                    socket,
                    memory,
                    *offset,
                    address,
                    &mut call.buffer,
                )
            }
        }
        OperationId::DeviceInvoke => {
            let KernelNativeObject::DeviceFunction(device) = &call.pinned.object else {
                return unsupported();
            };
            let input = call
                .memory
                .as_ref()
                .map(|(memory, offset, length)| (memory.as_ref(), *offset, *length as usize));
            let output = call.address.as_ref().map(|memory| {
                (
                    memory.as_ref(),
                    call.address_offset,
                    call.descriptor.arg4 as usize,
                )
            });
            super::device::device_invoke_memory_buffered(
                device,
                call.descriptor.arg0 as u32,
                input,
                output,
                &mut call.buffer,
            )
        }
        _ => return unsupported(),
    };
    match outcome {
        NativeCallOutcome::Return(value) => value,
        _ => unsupported(),
    }
}

fn operation_uses_memory(operation: OperationId) -> bool {
    matches!(
        operation,
        OperationId::StreamRead
            | OperationId::StreamWrite
            | OperationId::FileRead
            | OperationId::FileWrite
            | OperationId::ChannelSend
            | OperationId::ChannelReceive
            | OperationId::SocketSend
            | OperationId::SocketReceive
            | OperationId::DeviceInvoke
    )
}

fn operation_buffer_length(
    operation: OperationId,
    descriptor: &SubmissionDescriptor,
) -> Option<u64> {
    if operation == OperationId::DeviceInvoke {
        descriptor.arg3.checked_add(descriptor.arg4)
    } else {
        Some(descriptor.arg2)
    }
}

fn operation_interest(operation: OperationId) -> PollEvents {
    match operation {
        OperationId::StreamRead
        | OperationId::FileRead
        | OperationId::ChannelReceive
        | OperationId::SocketReceive => PollEvents::POLLIN,
        OperationId::StreamWrite
        | OperationId::FileWrite
        | OperationId::ChannelSend
        | OperationId::SocketSend => PollEvents::POLLOUT,
        _ => PollEvents::default(),
    }
}

fn is_retryable(result: NativeCallReturn) -> bool {
    matches!(
        result.status,
        status::STREAM_WOULD_BLOCK
            | status::SOCKET_WOULD_BLOCK
            | status::CHANNEL_EMPTY
            | status::CHANNEL_FULL
    )
}

fn subscribe_call(call: &mut PreparedCall) -> Result<(), u32> {
    if call.observer.interest.is_empty() || call.subscription.is_some() {
        return Ok(());
    }
    let Some(source) = call.poll_source() else {
        return Ok(());
    };
    let source_id = source.id();
    call.observer.source.store(source_id, Ordering::Release);
    let erased: Arc<dyn PollSubscriber> = call.observer.clone();
    let subscription = source
        .try_subscribe(Arc::downgrade(&erased))
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let (readiness, generation) = source.snapshot();
    call.observer
        .generation
        .store(generation, Ordering::Release);
    call.observer.ready.store(
        !readiness.intersect(call.observer.interest).is_empty(),
        Ordering::Release,
    );
    call.subscription = Some(subscription);
    Ok(())
}

fn prepare_retry(call: &mut PreparedCall) -> bool {
    if call.subscription.is_none() {
        return false;
    }
    if call.observer.ready.swap(false, Ordering::AcqRel) {
        return true;
    }
    let Some(source) = call.poll_source() else {
        return false;
    };
    let (readiness, generation) = source.snapshot();
    call.observer
        .generation
        .fetch_max(generation, Ordering::AcqRel);
    let terminal = PollEvents::POLLERR
        .with(PollEvents::POLLHUP)
        .with(PollEvents::POLLRDHUP);
    !readiness
        .intersect(call.observer.interest.with(terminal))
        .is_empty()
}

fn release_registrations(state: &mut RingState, call: &PreparedCall) {
    for token in call.registration_tokens[..call.registration_count as usize]
        .iter()
        .copied()
    {
        let registration = state
            .registrations
            .iter_mut()
            .find(|registration| registration.token == token)
            .expect("执行中的 Ring registration 不得消失");
        registration.in_use = registration
            .in_use
            .checked_sub(1)
            .expect("Ring registration 引用计数下溢");
    }
}

fn take_pending_call(ring: &SubmissionRingObject) -> Option<PreparedCall> {
    let mut state = ring.state.lock();
    let call = state.pending.pop_front()?;
    let in_flight = state
        .in_flight
        .iter_mut()
        .find(|entry| entry.user_data == call.descriptor.user_data)
        .expect("Ring 队列项必须存在 in-flight 状态");
    in_flight.phase = InFlightPhase::Running;
    Some(call)
}

fn complete_call(ring: &SubmissionRingObject, mut call: PreparedCall, result: NativeCallReturn) {
    call.clear_subscription();
    let mut state = ring.state.lock();
    let Some(index) = state
        .in_flight
        .iter()
        .position(|entry| entry.user_data == call.descriptor.user_data)
    else {
        return;
    };
    state.in_flight.swap_remove(index);
    release_registrations(&mut state, &call);
    state.reserved_completions -= 1;
    state.completed = state.completed.saturating_add(1);
    state.deferred_completions.push_back(CompletionRecord {
        user_data: call.descriptor.user_data,
        status: result.status,
        reserved: 0,
        value0: result.value0,
        value1: result.value1,
    });
    let completion_ready =
        flush_deferred_completions(ring, &mut state).is_ok_and(|queued| queued != 0);
    let readiness_version = ring.poll_source.reserve_version();
    drop(state);
    ring.publish_completion_state(readiness_version, completion_ready);
}

fn flush_deferred_completions(
    ring: &SubmissionRingObject,
    state: &mut RingState,
) -> Result<u32, u32> {
    let (_head, mut tail, queued) = ring.queue_len(
        wire::ring_shared_state::CQ_HEAD,
        wire::ring_shared_state::CQ_TAIL,
    )?;
    let available = state.capacity.saturating_sub(queued as usize);
    let publish = available.min(state.deferred_completions.len());
    for _ in 0..publish {
        let completion = *state
            .deferred_completions
            .front()
            .expect("已计算的 deferred completion 必须存在");
        ring.write_completion(tail, &completion)?;
        let next_tail = tail.wrapping_add(1);
        ring.write_index(wire::ring_shared_state::CQ_TAIL, next_tail)?;
        state.deferred_completions.pop_front();
        tail = next_tail;
    }
    Ok(queued.saturating_add(publish as u32))
}

fn requeue_call(ring: &SubmissionRingObject, call: PreparedCall, ready: bool) {
    let mut state = ring.state.lock();
    let Some(index) = state
        .in_flight
        .iter()
        .position(|entry| entry.user_data == call.descriptor.user_data)
    else {
        return;
    };
    state.in_flight[index].phase = InFlightPhase::Queued;
    state.pending.push_back(call);
    drop(state);
    if ready {
        ring.worker.notify();
    }
}

unsafe extern "C" fn ring_worker(argument: usize) -> ! {
    let signal = unsafe { Arc::from_raw(argument as *const RingWorkerSignal) };
    loop {
        if signal.stopped.load(Ordering::Acquire) {
            break;
        }
        let Some(ring) = signal.ring.upgrade() else {
            break;
        };
        let budget = {
            let state = ring.state.lock();
            #[cfg(feature = "kernel-tests")]
            if state.worker_paused {
                0
            } else {
                state.pending.len()
            }
            #[cfg(not(feature = "kernel-tests"))]
            state.pending.len()
        };
        if budget == 0 {
            drop(ring);
            wait_for_worker(&signal, false);
            continue;
        }
        for _ in 0..budget {
            let Some(mut call) = take_pending_call(&ring) else {
                break;
            };
            call.observer.ready.store(false, Ordering::Release);
            let result = execute_prepared_call(&mut call);
            if !is_retryable(result) {
                complete_call(&ring, call, result);
                continue;
            }
            let deadline = call.descriptor.arg4;
            if deadline != 0 && sched::now_ns_public() >= deadline {
                complete_call(
                    &ring,
                    call,
                    NativeCallReturn {
                        status: status::RING_TIMEOUT,
                        value0: 0,
                        value1: 0,
                    },
                );
                continue;
            }
            let ready = prepare_retry(&mut call);
            requeue_call(&ring, call, ready);
        }
        let has_pending = !ring.state.lock().pending.is_empty();
        drop(ring);
        wait_for_worker(&signal, has_pending);
    }
    drop(signal);
    sched::kthread_finish(sched::ExitCode(0));
}

#[cfg(feature = "kernel-tests")]
pub(super) fn pause_worker_for_test(ring: &SubmissionRingObject) {
    ring.state.lock().worker_paused = true;
}

fn wait_for_worker(signal: &RingWorkerSignal, has_pending: bool) {
    if signal.stopped.load(Ordering::Acquire) || signal.notified.swap(false, Ordering::AcqRel) {
        return;
    }
    let task = sched::current_task();
    if !has_pending {
        signal.waiters.wait_event(&task, || {
            signal.stopped.load(Ordering::Acquire) || signal.notified.swap(false, Ordering::AcqRel)
        });
        return;
    }

    const RECHECK_NS: u64 = 10_000_000;
    let deadline = sched::now_ns_public().saturating_add(RECHECK_NS);
    let entry = signal.waiters.prepare_to_wait(&task, TaskState::Sleeping);
    let deadline_armed = sched::register_sleep_deadline(&task, deadline);
    if signal.stopped.load(Ordering::Acquire)
        || signal.notified.swap(false, Ordering::AcqRel)
        || !deadline_armed
    {
        signal.waiters.finish_wait(&entry);
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        return;
    }
    sched::schedule_once(sched::now_ns_public());
    sched::cancel_sleep_deadline(&task);
    signal.waiters.finish_wait(&entry);
}

pub(super) fn ring_cancel(ring: &SubmissionRingObject, user_data: u64) -> NativeCallOutcome {
    let mut state = ring.state.lock();
    let Some(index) = state
        .in_flight
        .iter()
        .position(|entry| entry.user_data == user_data)
    else {
        return native_return(status::RING_NOT_FOUND, 0, 0);
    };
    if state.in_flight[index].phase == InFlightPhase::Running {
        return native_return(status::RING_BUSY, 0, 0);
    }
    let Some(pending_index) = state
        .pending
        .iter()
        .position(|call| call.descriptor.user_data == user_data)
    else {
        return native_return(status::RING_BUSY, 0, 0);
    };
    let call = state
        .pending
        .remove(pending_index)
        .expect("已定位的 Ring 请求必须仍在队列中");
    state.in_flight.swap_remove(index);
    release_registrations(&mut state, &call);
    state.reserved_completions -= 1;
    state.cancelled = state.cancelled.saturating_add(1);
    state.completed = state.completed.saturating_add(1);
    state.deferred_completions.push_back(CompletionRecord {
        user_data,
        status: status::RING_CANCELLED,
        reserved: 0,
        value0: 0,
        value1: 0,
    });
    let completion_ready =
        flush_deferred_completions(ring, &mut state).is_ok_and(|queued| queued != 0);
    let readiness_version = ring.poll_source.reserve_version();
    drop(state);
    drop(call);
    ring.publish_completion_state(readiness_version, completion_ready);
    native_return(status::OK, 0, 0)
}

pub(super) fn ring_wait(
    task: &Arc<Task>,
    ring: &SubmissionRingObject,
    minimum: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    if minimum == 0 || minimum > wire::MAX_RING_BATCH as u64 || minimum > ring.capacity as u64 {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let Some(_claim) = ring.claim_wait() else {
        return native_return(status::RING_BUSY, 0, 0);
    };
    let queued = match wait_for_completion(task, ring, minimum as u32, deadline_ns) {
        Ok(queued) => queued,
        Err(outcome) => return outcome,
    };
    let readiness_version = ring.poll_source.reserve_version();
    ring.publish_completion_state(readiness_version, queued != 0);
    native_return(status::OK, u64::from(queued), 0)
}

fn wait_for_completion(
    task: &Arc<Task>,
    ring: &SubmissionRingObject,
    minimum: u32,
    deadline_ns: u64,
) -> Result<u32, NativeCallOutcome> {
    loop {
        match available_completions(ring) {
            Ok(queued) if queued >= minimum => return Ok(queued),
            Ok(_) => {}
            Err(error) => return Err(native_return(error, 0, 0)),
        }
        if super::operations::has_native_external_control(task) {
            return Err(NativeCallOutcome::RetryExternalControl);
        }
        if deadline_ns != 0 && sched::now_ns_public() >= deadline_ns {
            return Err(native_return(status::RING_TIMEOUT, 0, 0));
        }
        let entry = ring.waiters.prepare_to_wait(task, TaskState::Sleeping);
        let deadline_armed = deadline_ns != 0 && sched::register_sleep_deadline(task, deadline_ns);
        if deadline_ns != 0 && !deadline_armed {
            ring.waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            return Err(native_return(status::RING_TIMEOUT, 0, 0));
        }
        let ready = match available_completions(ring) {
            Ok(queued) => queued >= minimum,
            Err(error) => {
                if deadline_armed {
                    sched::cancel_sleep_deadline(task);
                }
                ring.waiters.finish_wait(&entry);
                super::operations::restore_native_task_after_wait(task);
                return Err(native_return(error, 0, 0));
            }
        };
        if ready {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            ring.waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            continue;
        }
        if super::operations::has_native_external_control(task) {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            ring.waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            return Err(NativeCallOutcome::RetryExternalControl);
        }
        sched::schedule_once(sched::now_ns_public());
        if deadline_armed {
            sched::cancel_sleep_deadline(task);
        }
        ring.waiters.finish_wait(&entry);
        super::operations::restore_native_task_after_wait(task);
    }
}

fn available_completions(ring: &SubmissionRingObject) -> Result<u32, u32> {
    let mut state = ring.state.lock();
    flush_deferred_completions(ring, &mut state)
}

pub(super) fn ring_query(
    task: &Arc<Task>,
    ring: &SubmissionRingObject,
    user: u64,
) -> NativeCallOutcome {
    let mut state = ring.state.lock();
    let published = match flush_deferred_completions(ring, &mut state) {
        Ok(queued) => queued,
        Err(error) => return native_return(error, 0, 0),
    };
    let info = RingInfo {
        capacity: state.capacity as u32,
        reserved0: 0,
        queued: published
            .saturating_add(state.deferred_completions.len() as u32)
            .saturating_add(state.reserved_completions as u32),
        registered: state.registrations.len() as u32,
        generation: state.generation,
        completed: state.completed,
        cancelled: state.cancelled,
        reserved: [0; 3],
    };
    drop(state);
    if copy_user_value_out(task, user, &info).is_err() {
        return native_return(status::STREAM_FAULT, 0, 0);
    }
    native_return(status::OK, 0, 0)
}

fn copy_user_bytes_out<T: Copy>(task: &Arc<Task>, user: u64, values: &[T]) -> Result<(), ()> {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            values.as_ptr().cast::<u8>(),
            values.len() * core::mem::size_of::<T>(),
        )
    };
    super::copy_user_bytes_out(task, user, bytes).map_err(|_| ())
}

fn copy_user_value_out<T: Copy>(task: &Arc<Task>, user: u64, value: &T) -> Result<(), ()> {
    copy_user_bytes_out(task, user, core::slice::from_ref(value))
}
