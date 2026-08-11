//! Native Channel：保留消息边界的双向 capability 通道。

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use general::syscall::NativeCallOutcome;
use native_abi::wire::{ChannelHandleTransfer, ChannelMessage};
use native_abi::{NativeHandle, ObjectInterface, Rights, status, wire};
use sched::sync::Spinlock;
use sched::{Task, TaskState, WaitQueue};
use vfs::file::PollEvents;
use vfs::poll_source::PollSource;

use super::dispatch::native_return;
use super::memory::MemoryObject;
use super::{
    KernelNativeObject, NativeProcessState, copy_user_bytes_in, copy_user_bytes_out,
    copy_user_value,
};

struct QueuedTransfer {
    object: KernelNativeObject,
    interface: ObjectInterface,
    rights: Rights,
    source_handle: NativeHandle,
    move_source: bool,
}

struct QueuedMessage {
    data: Vec<u8>,
    transfers: Vec<QueuedTransfer>,
}

struct ChannelState {
    queues: [VecDeque<QueuedMessage>; 2],
    capacity: [usize; 2],
    receiving: [bool; 2],
    closed: [AtomicBool; 2],
    sources: [Arc<PollSource>; 2],
    waiters: [Arc<WaitQueue>; 2],
}

pub(crate) struct ChannelObject {
    state: Arc<Spinlock<ChannelState>>,
    side: usize,
    source: Arc<PollSource>,
    receive: Arc<sched::mutex::Mutex<()>>,
}

impl ChannelObject {
    fn peer_closed(&self, state: &ChannelState) -> bool {
        state.closed[1 - self.side].load(Ordering::Acquire)
    }

    pub(crate) fn poll_source(&self) -> &PollSource {
        &self.source
    }
}

impl Drop for ChannelObject {
    fn drop(&mut self) {
        let (updates, peer_waiters) = {
            let state = self.state.lock();
            state.closed[self.side].store(true, Ordering::Release);
            (
                reserve_readiness_updates(&state),
                Arc::clone(&state.waiters[1 - self.side]),
            )
        };
        publish_readiness_updates(updates);
        peer_waiters.wake_all();
    }
}

pub(super) fn channel_create(
    state: &NativeProcessState,
    object: &KernelNativeObject,
    capacity: u64,
) -> NativeCallOutcome {
    if !matches!(object, KernelNativeObject::SelfProcess)
        || capacity == 0
        || capacity > wire::MAX_CHANNEL_QUEUE_MESSAGES as u64
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let mut left_queue = VecDeque::new();
    let mut right_queue = VecDeque::new();
    if left_queue.try_reserve_exact(capacity as usize).is_err()
        || right_queue.try_reserve_exact(capacity as usize).is_err()
    {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let sources = [
        Arc::new(PollSource::new(PollEvents::POLLOUT)),
        Arc::new(PollSource::new(PollEvents::POLLOUT)),
    ];
    let shared = Arc::new(Spinlock::new(ChannelState {
        queues: [left_queue, right_queue],
        capacity: [capacity as usize; 2],
        receiving: [false; 2],
        closed: [AtomicBool::new(false), AtomicBool::new(false)],
        sources: [Arc::clone(&sources[0]), Arc::clone(&sources[1])],
        waiters: [Arc::new(WaitQueue::new()), Arc::new(WaitQueue::new())],
    }));
    let left = KernelNativeObject::Channel(Arc::new(ChannelObject {
        state: Arc::clone(&shared),
        side: 0,
        source: Arc::clone(&sources[0]),
        receive: Arc::new(sched::mutex::Mutex::new(())),
    }));
    let right = KernelNativeObject::Channel(Arc::new(ChannelObject {
        state: shared,
        side: 1,
        source: Arc::clone(&sources[1]),
        receive: Arc::new(sched::mutex::Mutex::new(())),
    }));
    let mut handles = state.handles.lock();
    let left = match handles.insert(
        left,
        ObjectInterface::Channel,
        Rights::SEND | Rights::RECEIVE | Rights::OBSERVE | Rights::DUPLICATE,
    ) {
        Ok(handle) => handle,
        Err(error) => return native_return(error, 0, 0),
    };
    let right = match handles.insert(
        right,
        ObjectInterface::Channel,
        Rights::SEND | Rights::RECEIVE | Rights::OBSERVE | Rights::DUPLICATE,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = handles.close(left);
            return native_return(error, 0, 0);
        }
    };
    native_return(status::OK, left.raw(), right.raw())
}

pub(super) fn channel_send(
    task: &Arc<Task>,
    state: &NativeProcessState,
    channel: &ChannelObject,
    user: u64,
) -> NativeCallOutcome {
    let message = match copy_user_value::<ChannelMessage>(task, user) {
        Ok(message) => message,
        Err(error) => return native_return(error, 0, 0),
    };
    if message.reserved != [0; 3]
        || message.flags != 0
        || message.data_size > message.data_capacity
        || message.data_size > wire::MAX_CHANNEL_MESSAGE_BYTES
        || message.handle_count > message.handle_capacity
        || message.handle_count > wire::MAX_CHANNEL_MESSAGE_HANDLES
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let data = match copy_user_bytes(task, message.data_ptr, message.data_size as usize) {
        Ok(data) => data,
        Err(error) => return native_return(error, 0, 0),
    };
    let transfer_refs = match copy_user_array::<ChannelHandleTransfer>(
        task,
        message.handles_ptr,
        message.handle_count,
    ) {
        Ok(transfers) => transfers,
        Err(error) => return native_return(error, 0, 0),
    };

    let mut queued = Vec::new();
    if queued.try_reserve(transfer_refs.len()).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    let mut handles = state.handles.lock();
    for transfer in transfer_refs {
        if transfer.reserved != 0 || transfer.flags & !wire::CHANNEL_TRANSFER_MOVE != 0 {
            return native_return(status::CHANNEL_TRANSFER_INVALID, 0, 0);
        }
        let source_handle = NativeHandle::from_raw(transfer.source_handle);
        let requested = Rights::from_bits(transfer.requested_rights);
        if transfer.flags == wire::CHANNEL_TRANSFER_MOVE
            && queued.iter().any(|item: &QueuedTransfer| {
                item.move_source && item.source_handle == source_handle
            })
        {
            return native_return(status::CHANNEL_TRANSFER_INVALID, 0, 0);
        }
        let entry = match handles.lookup(source_handle, None, requested) {
            Ok(entry) => entry,
            Err(error) => return native_return(error, 0, 0),
        };
        queued.push(QueuedTransfer {
            object: entry.object.clone(),
            interface: entry.interface,
            rights: requested,
            source_handle,
            move_source: transfer.flags == wire::CHANNEL_TRANSFER_MOVE,
        });
    }
    let mut shared = channel.state.lock();
    if channel.peer_closed(&shared) {
        return native_return(status::CHANNEL_PEER_CLOSED, 0, 0);
    }
    if shared.queues[channel.side].len() + usize::from(shared.receiving[channel.side])
        >= shared.capacity[channel.side]
    {
        return native_return(status::CHANNEL_FULL, 0, 0);
    }
    shared.queues[channel.side].push_back(QueuedMessage {
        data,
        transfers: queued,
    });
    let message = shared.queues[channel.side]
        .back()
        .expect("刚入队的消息必须存在");
    let mut removed = Vec::new();
    if removed.try_reserve(message.transfers.len()).is_err() {
        let _ = shared.queues[channel.side].pop_back();
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    for transfer in message
        .transfers
        .iter()
        .filter(|transfer| transfer.move_source)
    {
        match handles.close(transfer.source_handle) {
            Ok(object) => removed.push(object),
            Err(_) => {
                let _ = shared.queues[channel.side].pop_back();
                return native_return(status::HANDLE_STALE, 0, 0);
            }
        }
    }
    let updates = reserve_readiness_updates(&shared);
    let receiver_waiters = Arc::clone(&shared.waiters[1 - channel.side]);
    drop(handles);
    drop(shared);
    drop(removed);
    publish_readiness_updates(updates);
    receiver_waiters.wake_one_default();
    native_return(status::OK, 0, 0)
}

pub(super) fn channel_receive(
    task: &Arc<Task>,
    state: &NativeProcessState,
    channel: &ChannelObject,
    user: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let message = match copy_user_value::<ChannelMessage>(task, user) {
        Ok(message) => message,
        Err(error) => return native_return(error, 0, 0),
    };
    if message.reserved != [0; 3]
        || message.flags != 0
        || message.data_capacity > wire::MAX_CHANNEL_MESSAGE_BYTES
        || message.handle_capacity > wire::MAX_CHANNEL_MESSAGE_HANDLES
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }

    let _receive = channel.receive.lock();
    loop {
        if let Err(outcome) = wait_for_message(task, channel, deadline_ns) {
            return outcome;
        }
        let queue_side = 1 - channel.side;
        let queued = {
            let mut shared = channel.state.lock();
            let Some(queued) = shared.queues[queue_side].front() else {
                if channel.peer_closed(&shared) {
                    return native_return(status::CHANNEL_PEER_CLOSED, 0, 0);
                }
                drop(shared);
                continue;
            };
            if message.data_capacity < queued.data.len() as u32
                || message.handle_capacity < queued.transfers.len() as u32
            {
                return native_return(
                    status::CHANNEL_BUFFER_TOO_SMALL,
                    queued.data.len() as u64,
                    queued.transfers.len() as u64,
                );
            }
            let queued = shared.queues[queue_side]
                .pop_front()
                .expect("已检查的 Channel 消息必须存在");
            shared.receiving[queue_side] = true;
            queued
        };

        if !queued.data.is_empty() {
            if copy_user_bytes_out(task, message.data_ptr, &queued.data).is_err() {
                restore_received_message(channel, queue_side, queued);
                return native_return(status::STREAM_FAULT, 0, 0);
            }
        }

        let mut handles = state.handles.lock();
        let mut inserted = Vec::new();
        if inserted.try_reserve(queued.transfers.len()).is_err() {
            drop(handles);
            restore_received_message(channel, queue_side, queued);
            return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
        }
        for transfer in &queued.transfers {
            let handle = match handles.insert(
                transfer.object.clone(),
                transfer.interface,
                transfer.rights,
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    for inserted in inserted.drain(..) {
                        let _ = handles.close(inserted);
                    }
                    drop(handles);
                    restore_received_message(channel, queue_side, queued);
                    return native_return(error, 0, 0);
                }
            };
            inserted.push(handle);
        }
        if !inserted.is_empty() {
            let mut transfers = Vec::new();
            if transfers.try_reserve_exact(inserted.len()).is_err() {
                for inserted in inserted.drain(..) {
                    let _ = handles.close(inserted);
                }
                drop(handles);
                restore_received_message(channel, queue_side, queued);
                return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
            }
            for (handle, source) in inserted.iter().zip(queued.transfers.iter()) {
                transfers.push(ChannelHandleTransfer {
                    source_handle: handle.raw(),
                    requested_rights: source.rights.bits(),
                    flags: 0,
                    reserved: 0,
                });
            }
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    transfers.as_ptr().cast::<u8>(),
                    transfers.len() * core::mem::size_of::<ChannelHandleTransfer>(),
                )
            };
            if copy_user_bytes_out(task, message.handles_ptr, bytes).is_err() {
                for inserted in inserted.drain(..) {
                    let _ = handles.close(inserted);
                }
                drop(handles);
                restore_received_message(channel, queue_side, queued);
                return native_return(status::STREAM_FAULT, 0, 0);
            }
        }
        let result = (queued.data.len() as u64, queued.transfers.len() as u64);
        drop(handles);
        let updates = {
            let mut shared = channel.state.lock();
            shared.receiving[queue_side] = false;
            reserve_readiness_updates(&shared)
        };
        publish_readiness_updates(updates);
        return native_return(status::OK, result.0, result.1);
    }
}

fn restore_received_message(channel: &ChannelObject, queue_side: usize, message: QueuedMessage) {
    let waiters = {
        let mut shared = channel.state.lock();
        shared.queues[queue_side].push_front(message);
        shared.receiving[queue_side] = false;
        Arc::clone(&shared.waiters[1 - queue_side])
    };
    waiters.wake_one_default();
}

fn wait_for_message(
    task: &Arc<Task>,
    channel: &ChannelObject,
    deadline_ns: u64,
) -> Result<(), NativeCallOutcome> {
    loop {
        let (ready, waiters) = {
            let state = channel.state.lock();
            (
                !state.queues[1 - channel.side].is_empty() || channel.peer_closed(&state),
                Arc::clone(&state.waiters[channel.side]),
            )
        };
        if ready {
            return Ok(());
        }
        if deadline_ns == 0 || sched::now_ns_public() >= deadline_ns {
            return Err(native_return(status::CHANNEL_EMPTY, 0, 0));
        }
        if super::operations::has_native_external_control(task) {
            return Err(NativeCallOutcome::RetryExternalControl);
        }

        let entry = waiters.prepare_to_wait(task, TaskState::Sleeping);
        let infinite = deadline_ns == u64::MAX;
        let deadline_armed = !infinite && sched::register_sleep_deadline(task, deadline_ns);
        if !infinite && !deadline_armed {
            waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            return Err(native_return(status::CHANNEL_EMPTY, 0, 0));
        }
        let ready = {
            let state = channel.state.lock();
            !state.queues[1 - channel.side].is_empty() || channel.peer_closed(&state)
        };
        if ready {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            return Ok(());
        }
        if super::operations::has_native_external_control(task) {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            return Err(NativeCallOutcome::RetryExternalControl);
        }
        sched::schedule_once(sched::now_ns_public());
        if deadline_armed {
            sched::cancel_sleep_deadline(task);
        }
        waiters.finish_wait(&entry);
        super::operations::restore_native_task_after_wait(task);
    }
}

pub(super) fn channel_send_memory(
    channel: &ChannelObject,
    memory: &Arc<MemoryObject>,
    offset: u64,
    length: u64,
) -> NativeCallOutcome {
    if length > u64::from(wire::MAX_CHANNEL_MESSAGE_BYTES) {
        return native_return(status::CHANNEL_MESSAGE_TOO_LARGE, 0, 0);
    }
    let mut data = if length == 0 {
        Vec::new()
    } else {
        let Ok(length) = usize::try_from(length) else {
            return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
        };
        let mut data = Vec::new();
        if data.try_reserve_exact(length).is_err() {
            return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
        }
        data.resize(length, 0);
        data
    };
    channel_send_memory_buffered(channel, memory, offset, &mut data)
}

pub(super) fn channel_send_memory_buffered(
    channel: &ChannelObject,
    memory: &Arc<MemoryObject>,
    offset: u64,
    data: &mut Vec<u8>,
) -> NativeCallOutcome {
    let length = data.len() as u64;
    if length > u64::from(wire::MAX_CHANNEL_MESSAGE_BYTES) {
        return native_return(status::CHANNEL_MESSAGE_TOO_LARGE, 0, 0);
    }
    if let Err(error) = memory.read_into(offset, data) {
        return native_return(error, 0, 0);
    }
    let mut shared = channel.state.lock();
    if channel.peer_closed(&shared) {
        return native_return(status::CHANNEL_PEER_CLOSED, 0, 0);
    }
    if shared.queues[channel.side].len() + usize::from(shared.receiving[channel.side])
        >= shared.capacity[channel.side]
    {
        return native_return(status::CHANNEL_FULL, 0, 0);
    }
    shared.queues[channel.side].push_back(QueuedMessage {
        data: core::mem::take(data),
        transfers: Vec::new(),
    });
    let updates = reserve_readiness_updates(&shared);
    let receiver_waiters = Arc::clone(&shared.waiters[1 - channel.side]);
    drop(shared);
    publish_readiness_updates(updates);
    receiver_waiters.wake_one_default();
    native_return(status::OK, length, 0)
}

pub(super) fn channel_receive_memory(
    channel: &ChannelObject,
    memory: &Arc<MemoryObject>,
    offset: u64,
    capacity: u64,
) -> NativeCallOutcome {
    if capacity > u64::from(wire::MAX_CHANNEL_MESSAGE_BYTES) {
        return native_return(status::CHANNEL_MESSAGE_TOO_LARGE, 0, 0);
    }
    let _receive = channel.receive.lock();
    let queue_side = 1 - channel.side;
    let queued = {
        let mut shared = channel.state.lock();
        let Some(queued) = shared.queues[queue_side].front() else {
            return if channel.peer_closed(&shared) {
                native_return(status::CHANNEL_PEER_CLOSED, 0, 0)
            } else {
                native_return(status::CHANNEL_EMPTY, 0, 0)
            };
        };
        if !queued.transfers.is_empty() || queued.data.len() as u64 > capacity {
            return native_return(
                status::CHANNEL_BUFFER_TOO_SMALL,
                queued.data.len() as u64,
                queued.transfers.len() as u64,
            );
        }
        let queued = shared.queues[queue_side]
            .pop_front()
            .expect("已检查的 Channel 消息必须存在");
        shared.receiving[queue_side] = true;
        queued
    };
    if let Err(error) = memory.write_from(offset, &queued.data) {
        restore_received_message(channel, queue_side, queued);
        return native_return(error, 0, 0);
    }
    let count = queued.data.len() as u64;
    let updates = {
        let mut shared = channel.state.lock();
        shared.receiving[queue_side] = false;
        reserve_readiness_updates(&shared)
    };
    publish_readiness_updates(updates);
    native_return(status::OK, count, 0)
}

fn channel_readiness(state: &ChannelState, side: usize) -> PollEvents {
    let mut readiness = PollEvents::default();
    if !state.queues[1 - side].is_empty() {
        readiness = readiness.with(PollEvents::POLLIN);
    }
    if !state.closed[1 - side].load(Ordering::Acquire)
        && state.queues[side].len() + usize::from(state.receiving[side]) < state.capacity[side]
    {
        readiness = readiness.with(PollEvents::POLLOUT);
    }
    if state.closed[1 - side].load(Ordering::Acquire) {
        readiness = readiness.with(PollEvents::POLLHUP);
    }
    readiness
}

fn reserve_readiness_updates(state: &ChannelState) -> [(Arc<PollSource>, u64, PollEvents); 2] {
    [
        (
            Arc::clone(&state.sources[0]),
            state.sources[0].reserve_version(),
            channel_readiness(state, 0),
        ),
        (
            Arc::clone(&state.sources[1]),
            state.sources[1].reserve_version(),
            channel_readiness(state, 1),
        ),
    ]
}

fn publish_readiness_updates(updates: [(Arc<PollSource>, u64, PollEvents); 2]) {
    for (source, version, readiness) in updates {
        source.publish_versioned(readiness, version);
    }
}

fn copy_user_bytes(task: &Arc<Task>, user: u64, length: usize) -> Result<Vec<u8>, u32> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    bytes.resize(length, 0);
    copy_user_bytes_in(task, user, &mut bytes)?;
    Ok(bytes)
}

fn copy_user_array<T: Copy + Default>(
    task: &Arc<Task>,
    user: u64,
    count: u32,
) -> Result<Vec<T>, u32> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(count as usize)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let stride = core::mem::size_of::<T>() as u64;
    for index in 0..count {
        let address = user
            .checked_add(
                u64::from(index)
                    .checked_mul(stride)
                    .ok_or(status::CORE_OUT_OF_RANGE)?,
            )
            .ok_or(status::CORE_OUT_OF_RANGE)?;
        output.push(copy_user_value(task, address)?);
    }
    Ok(output)
}
