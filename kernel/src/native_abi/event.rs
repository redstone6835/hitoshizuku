//! 固定容量 EventPort 与进程、流、定时器事件源。

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use general::mm::copy_to_user;
use general::syscall::NativeCallOutcome;
use native_abi::wire::EventRecord;
use native_abi::{NativeHandle, ObjectInterface, Rights, status, wire};
use sched::{DeadlineObserver, ProcessExitObserver, Task, TaskState, WaitQueue};
use vfs::file::{File, PollEvents};
use vfs::poll_source::PollSubscriber;

use super::dispatch::native_return;
use super::operations::{
    has_native_external_control, insert_native_handle, restore_native_task_after_wait,
};
use super::{KernelNativeObject, NativeProcessState, ProcessObject};

struct QueueEntry {
    token: u64,
    record: EventRecord,
    claimed: bool,
}

enum SubscriptionKind {
    Process {
        process: Arc<ProcessObject>,
        backend: u64,
    },
    Stream {
        source_id: u64,
        file: Arc<File>,
        backend: u64,
        interest: PollEvents,
        last_generation: u64,
    },
    Timer {
        backend: u64,
        deadline_ns: u64,
        interval_ns: u64,
        expirations: u64,
    },
}

struct Subscription {
    token: u64,
    source_handle: u64,
    user_data: u64,
    observer: Arc<EventObserver>,
    kind: SubscriptionKind,
    terminal: Option<EventRecord>,
    terminal_claimed: bool,
}

struct EventState {
    next_token: u64,
    next_sequence: u64,
    subscriptions: Vec<Subscription>,
    queue: VecDeque<QueueEntry>,
}

pub(crate) struct EventPort {
    capacity: usize,
    state: sched::sync::Spinlock<EventState>,
    waiters: WaitQueue,
    wait_claimed: AtomicBool,
}

struct EventObserver {
    port: Weak<EventPort>,
    token: u64,
}

impl EventPort {
    fn new(capacity: u32) -> Result<Arc<Self>, u32> {
        if capacity == 0 || capacity > wire::MAX_EVENT_PORT_CAPACITY {
            return Err(status::CORE_OUT_OF_RANGE);
        }
        let capacity = capacity as usize;
        let mut subscriptions = Vec::new();
        subscriptions
            .try_reserve_exact(capacity)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(capacity)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        Ok(Arc::new(Self {
            capacity,
            state: sched::sync::Spinlock::new(EventState {
                next_token: 1,
                next_sequence: 1,
                subscriptions,
                queue,
            }),
            waiters: WaitQueue::new_with_reason(sched::WaitReason::Poll),
            wait_claimed: AtomicBool::new(false),
        }))
    }

    fn claim_wait(&self) -> Option<EventWaitClaim<'_>> {
        self.wait_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| EventWaitClaim {
                claimed: &self.wait_claimed,
            })
    }

    fn reserve_token(&self) -> Result<u64, u32> {
        let mut state = self.state.lock();
        if state.subscriptions.len() >= self.capacity {
            return Err(status::EVENT_QUEUE_EXHAUSTED);
        }
        let token = state.next_token;
        state.next_token = state.next_token.checked_add(1).unwrap_or(1);
        if token == 0 {
            return Err(status::CORE_RESOURCE_EXHAUSTED);
        }
        Ok(token)
    }

    fn install_subscription(&self, subscription: Subscription) -> Result<(), (u32, Subscription)> {
        let mut state = self.state.lock();
        if state.subscriptions.len() >= self.capacity
            || state
                .subscriptions
                .iter()
                .any(|entry| entry.token == subscription.token)
        {
            return Err((status::EVENT_QUEUE_EXHAUSTED, subscription));
        }
        state.subscriptions.push(subscription);
        Ok(())
    }

    fn publish_process(&self, token: u64) {
        let mut state = self.state.lock();
        let Some(index) = state
            .subscriptions
            .iter()
            .position(|entry| entry.token == token)
        else {
            return;
        };
        let (source_handle, user_data, result) = {
            let subscription = &state.subscriptions[index];
            let SubscriptionKind::Process { process, .. } = &subscription.kind else {
                return;
            };
            (
                subscription.source_handle,
                subscription.user_data,
                process.result(),
            )
        };
        if state.subscriptions[index].terminal.is_some() {
            return;
        }
        let sequence = take_sequence(&mut state);
        let event_kind = if result.fault_kind != 0 {
            wire::EVENT_KIND_PROCESS_FAULT
        } else {
            wire::EVENT_KIND_PROCESS_EXITED
        };
        state.subscriptions[index].terminal = Some(EventRecord {
            event_kind,
            status: status::OK,
            source_handle,
            sequence,
            value0: u64::from(result.exit_code) | (u64::from(result.fault_kind) << 32),
            value1: user_data,
        });
        drop(state);
        self.waiters.wake_all();
    }

    /// 处理 PollSource 的异步就绪通知。只有来源匹配且 generation 严格前进的
    /// callback 才能改变 subscription 的已观察状态，防止并发发布乱序复活旧事件。
    fn publish_stream(&self, token: u64, source_id: u64, readiness: PollEvents, generation: u64) {
        let mut state = self.state.lock();
        let published = Self::publish_stream_locked(
            &mut state,
            token,
            source_id,
            readiness,
            generation,
            false,
            self.capacity,
        );
        drop(state);
        if published {
            self.waiters.wake_all();
        }
    }

    /// 将 Source snapshot 送入相同的 generation 门控。snapshot 与已经观察到的
    /// generation 相等时仍可重新投递，这正是 level readiness 的重投递条件。
    fn publish_stream_snapshot(
        &self,
        token: u64,
        source_id: u64,
        readiness: PollEvents,
        generation: u64,
    ) {
        let mut state = self.state.lock();
        let published = Self::publish_stream_locked(
            &mut state,
            token,
            source_id,
            readiness,
            generation,
            true,
            self.capacity,
        );
        drop(state);
        if published {
            self.waiters.wake_all();
        }
    }

    fn publish_stream_locked(
        state: &mut EventState,
        token: u64,
        source_id: u64,
        readiness: PollEvents,
        generation: u64,
        allow_current_generation: bool,
        capacity: usize,
    ) -> bool {
        let (previous_generation, interest, source_handle, user_data) = {
            let Some(subscription) = state
                .subscriptions
                .iter_mut()
                .find(|entry| entry.token == token)
            else {
                return false;
            };
            let SubscriptionKind::Stream {
                source_id: expected_source,
                interest,
                last_generation,
                ..
            } = &mut subscription.kind
            else {
                return false;
            };
            if *expected_source != source_id
                || generation < *last_generation
                || (!allow_current_generation && generation == *last_generation)
            {
                return false;
            }
            let previous_generation = *last_generation;
            *last_generation = generation;
            (
                previous_generation,
                *interest,
                subscription.source_handle,
                subscription.user_data,
            )
        };
        let ready = readiness.intersect(interest);
        let queued = state.queue.iter().position(|entry| {
            entry.token == token && entry.record.event_kind == wire::EVENT_KIND_STREAM_READY
        });

        if ready.is_empty() {
            if let Some(index) = queued {
                state.queue.remove(index);
            }
            return false;
        }

        if let Some(index) = queued {
            state.queue[index].record.value0 = u64::from(native_stream_events(ready));
            if generation > previous_generation {
                let sequence = take_sequence(state);
                state.queue[index].record.sequence = sequence;
            }
            return false;
        }
        if state.queue.len() >= capacity {
            return false;
        }
        let sequence = take_sequence(state);
        state.queue.push_back(QueueEntry {
            token,
            record: EventRecord {
                event_kind: wire::EVENT_KIND_STREAM_READY,
                status: status::OK,
                source_handle,
                sequence,
                value0: u64::from(native_stream_events(ready)),
                value1: user_data,
            },
            claimed: false,
        });
        true
    }

    fn publish_timer(&self, token: u64, registration: u64, now_ns: u64) -> Option<u64> {
        let mut state = self.state.lock();
        let Some(index) = state
            .subscriptions
            .iter()
            .position(|entry| entry.token == token)
        else {
            return None;
        };
        let (source_handle, user_data, expirations, next) = {
            let subscription = &mut state.subscriptions[index];
            let SubscriptionKind::Timer {
                backend,
                deadline_ns,
                interval_ns,
                expirations,
            } = &mut subscription.kind
            else {
                return None;
            };
            if *backend != registration {
                return None;
            }
            let (elapsed_periods, next) = if *interval_ns == 0 {
                (1, None)
            } else {
                let elapsed = now_ns.saturating_sub(*deadline_ns);
                let periods = elapsed / *interval_ns + 1;
                *deadline_ns = deadline_ns.saturating_add(interval_ns.saturating_mul(periods));
                (periods, Some(*deadline_ns))
            };
            *expirations = expirations.saturating_add(elapsed_periods);
            (
                subscription.source_handle,
                subscription.user_data,
                *expirations,
                next,
            )
        };
        if let Some(index) = state.queue.iter().position(|entry| {
            entry.token == token && entry.record.event_kind == wire::EVENT_KIND_TIMER_EXPIRED
        }) {
            let claimed = state.queue[index].claimed;
            state.queue[index].record.value0 = expirations;
            if claimed {
                let Some(mut queued) = state.queue.remove(index) else {
                    return next;
                };
                queued.record.sequence = take_sequence(&mut state);
                state.queue.push_back(queued);
            }
            return next;
        }
        if state.queue.len() < self.capacity {
            let sequence = take_sequence(&mut state);
            state.queue.push_back(QueueEntry {
                token,
                record: EventRecord {
                    event_kind: wire::EVENT_KIND_TIMER_EXPIRED,
                    status: status::OK,
                    source_handle,
                    sequence,
                    value0: expirations,
                    value1: user_data,
                },
                claimed: false,
            });
            drop(state);
            self.waiters.wake_all();
        }
        next
    }

    fn cancel(&self, token: u64) -> Result<Subscription, u32> {
        let mut state = self.state.lock();
        let Some(index) = state
            .subscriptions
            .iter()
            .position(|entry| entry.token == token)
        else {
            return Err(status::EVENT_INVALID_TOKEN);
        };
        if state
            .queue
            .iter()
            .any(|entry| entry.token == token && entry.claimed)
            || state.subscriptions[index].terminal_claimed
        {
            return Err(status::EVENT_WOULD_BLOCK);
        }
        state.queue.retain(|entry| entry.token != token);
        Ok(state.subscriptions.swap_remove(index))
    }

    fn has_events(&self) -> bool {
        let state = self.state.lock();
        !state.queue.is_empty()
            || state
                .subscriptions
                .iter()
                .any(|entry| entry.terminal.is_some())
    }

    fn peek(&self, output: &mut [EventRecord]) -> BatchSelection {
        let mut state = self.state.lock();
        let mut selected_tokens = [0u64; wire::MAX_EVENT_BATCH as usize];
        let mut queue_sequences = [0u64; wire::MAX_EVENT_BATCH as usize];
        let mut queue_tokens = [0u64; wire::MAX_EVENT_BATCH as usize];
        let mut queue_kinds = [0u32; wire::MAX_EVENT_BATCH as usize];
        let mut queue_values = [0u64; wire::MAX_EVENT_BATCH as usize];
        let mut terminal_count = 0usize;
        let mut queue_count = 0usize;
        let mut count = 0usize;
        while count < output.len() {
            let queue = state
                .queue
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    !entry.claimed
                        && !queue_sequences[..queue_count].contains(&entry.record.sequence)
                })
                .min_by_key(|(_, entry)| entry.record.sequence)
                .map(|(index, entry)| (index, entry.record));
            let terminal = state
                .subscriptions
                .iter()
                .filter_map(|entry| entry.terminal.map(|record| (entry.token, record)))
                .filter(|(token, _)| {
                    !selected_tokens[..terminal_count].contains(token)
                        && !state
                            .subscriptions
                            .iter()
                            .find(|entry| entry.token == *token)
                            .is_some_and(|entry| entry.terminal_claimed)
                })
                .min_by_key(|(_, record)| record.sequence);
            match (queue, terminal) {
                (None, None) => break,
                (Some((index, record)), None) => {
                    output[count] = record;
                    queue_tokens[queue_count] = state.queue[index].token;
                    queue_kinds[queue_count] = record.event_kind;
                    queue_sequences[queue_count] = record.sequence;
                    queue_values[queue_count] = record.value0;
                    queue_count += 1;
                }
                (None, Some((token, record))) => {
                    output[count] = record;
                    selected_tokens[terminal_count] = token;
                    terminal_count += 1;
                }
                (Some((index, queue_record)), Some((token, terminal_record))) => {
                    if queue_record.sequence <= terminal_record.sequence {
                        output[count] = queue_record;
                        queue_tokens[queue_count] = state.queue[index].token;
                        queue_kinds[queue_count] = queue_record.event_kind;
                        queue_sequences[queue_count] = queue_record.sequence;
                        queue_values[queue_count] = queue_record.value0;
                        queue_count += 1;
                    } else {
                        output[count] = terminal_record;
                        selected_tokens[terminal_count] = token;
                        terminal_count += 1;
                    }
                }
            }
            count += 1;
        }
        for sequence in &queue_sequences[..queue_count] {
            if let Some(entry) = state
                .queue
                .iter_mut()
                .find(|entry| entry.record.sequence == *sequence)
            {
                entry.claimed = true;
            }
        }
        for token in &selected_tokens[..terminal_count] {
            if let Some(entry) = state
                .subscriptions
                .iter_mut()
                .find(|entry| entry.token == *token)
            {
                entry.terminal_claimed = true;
            }
        }
        BatchSelection {
            count,
            queue_count,
            terminal_count,
            terminal_tokens: selected_tokens,
            queue_tokens,
            queue_kinds,
            queue_sequences,
            queue_values,
        }
    }

    fn commit_peek(&self, selection: &BatchSelection) {
        let mut stream_snapshots = [None; wire::MAX_EVENT_BATCH as usize];
        let mut stream_snapshot_count = 0usize;
        for index in 0..selection.queue_count {
            if selection.queue_kinds[index] == wire::EVENT_KIND_STREAM_READY
                && let Some(snapshot) = self.snapshot_stream_level(selection.queue_tokens[index])
            {
                stream_snapshots[stream_snapshot_count] = Some(snapshot);
                stream_snapshot_count += 1;
            }
        }

        let mut state = self.state.lock();
        for index in 0..selection.queue_count {
            let Some(queue_index) = state.queue.iter().position(|entry| {
                entry.token == selection.queue_tokens[index]
                    && entry.record.event_kind == selection.queue_kinds[index]
            }) else {
                continue;
            };
            if state.queue[queue_index].record.sequence == selection.queue_sequences[index] {
                let Some(entry) = state.queue.remove(queue_index) else {
                    continue;
                };
                if entry.record.event_kind == wire::EVENT_KIND_TIMER_EXPIRED
                    && let Some(subscription) = state
                        .subscriptions
                        .iter_mut()
                        .find(|subscription| subscription.token == selection.queue_tokens[index])
                    && let SubscriptionKind::Timer { expirations, .. } = &mut subscription.kind
                {
                    *expirations = expirations.saturating_sub(selection.queue_values[index]);
                }
            } else {
                state.queue[queue_index].claimed = false;
                if state.queue[queue_index].record.event_kind == wire::EVENT_KIND_TIMER_EXPIRED
                    && let Some(subscription) = state
                        .subscriptions
                        .iter_mut()
                        .find(|subscription| subscription.token == selection.queue_tokens[index])
                    && let SubscriptionKind::Timer { expirations, .. } = &mut subscription.kind
                {
                    *expirations = expirations.saturating_sub(selection.queue_values[index]);
                    state.queue[queue_index].record.value0 = *expirations;
                }
            }
        }
        for snapshot in stream_snapshots[..stream_snapshot_count].iter().flatten() {
            Self::publish_stream_locked(
                &mut state,
                snapshot.token,
                snapshot.source_id,
                snapshot.readiness,
                snapshot.generation,
                true,
                self.capacity,
            );
        }
        for token in &selection.terminal_tokens[..selection.terminal_count] {
            if let Some(subscription) = state
                .subscriptions
                .iter_mut()
                .find(|entry| entry.token == *token)
            {
                subscription.terminal = None;
                subscription.terminal_claimed = false;
            }
        }
    }

    /// PollSource 可能在查询时同步发布 readiness，因此只在短临界区内克隆 File，
    /// 实际查询必须位于 EventPort 锁外。commit 阶段会重新验证 token、source 与 generation。
    fn snapshot_stream_level(&self, token: u64) -> Option<StreamSnapshot> {
        let file = {
            let state = self.state.lock();
            state
                .subscriptions
                .iter()
                .find(|entry| entry.token == token)
                .and_then(|entry| match &entry.kind {
                    SubscriptionKind::Stream { file, .. } => Some(Arc::clone(file)),
                    _ => None,
                })
        };
        let file = file?;
        let source = file.poll_source()?;
        let (readiness, generation) = source.snapshot();
        Some(StreamSnapshot {
            token,
            source_id: source.id(),
            readiness,
            generation,
        })
    }

    fn rollback_peek(&self, selection: &BatchSelection) {
        let mut state = self.state.lock();
        for index in 0..selection.queue_count {
            if let Some(entry) = state.queue.iter_mut().find(|entry| {
                entry.token == selection.queue_tokens[index]
                    && entry.record.event_kind == selection.queue_kinds[index]
            }) {
                entry.claimed = false;
            }
        }
        for token in &selection.terminal_tokens[..selection.terminal_count] {
            if let Some(subscription) = state
                .subscriptions
                .iter_mut()
                .find(|entry| entry.token == *token)
            {
                subscription.terminal_claimed = false;
            }
        }
    }
}

impl Drop for EventPort {
    fn drop(&mut self) {
        let subscriptions = core::mem::take(&mut self.state.lock().subscriptions);
        for subscription in subscriptions {
            cancel_backend(subscription);
        }
    }
}

impl ProcessExitObserver for EventObserver {
    fn process_exited(&self) {
        if let Some(port) = self.port.upgrade() {
            port.publish_process(self.token);
        }
    }
}

impl PollSubscriber for EventObserver {
    fn readiness_changed(&self, source: u64, readiness: PollEvents, generation: u64) {
        if let Some(port) = self.port.upgrade() {
            port.publish_stream(self.token, source, readiness, generation);
        }
    }
}

impl DeadlineObserver for EventObserver {
    fn deadline_expired(&self, registration: u64, now_ns: u64) -> Option<u64> {
        self.port
            .upgrade()
            .and_then(|port| port.publish_timer(self.token, registration, now_ns))
    }
}

struct BatchSelection {
    count: usize,
    queue_count: usize,
    terminal_count: usize,
    terminal_tokens: [u64; wire::MAX_EVENT_BATCH as usize],
    queue_tokens: [u64; wire::MAX_EVENT_BATCH as usize],
    queue_kinds: [u32; wire::MAX_EVENT_BATCH as usize],
    queue_sequences: [u64; wire::MAX_EVENT_BATCH as usize],
    queue_values: [u64; wire::MAX_EVENT_BATCH as usize],
}

#[derive(Clone, Copy)]
struct StreamSnapshot {
    token: u64,
    source_id: u64,
    readiness: PollEvents,
    generation: u64,
}

struct EventWaitClaim<'a> {
    claimed: &'a AtomicBool,
}

impl Drop for EventWaitClaim<'_> {
    fn drop(&mut self) {
        self.claimed.store(false, Ordering::Release);
    }
}

pub(super) fn event_create(
    state: &NativeProcessState,
    object: &KernelNativeObject,
    capacity: u64,
) -> NativeCallOutcome {
    if !matches!(object, KernelNativeObject::SelfProcess) || capacity > u32::MAX as u64 {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let port = match EventPort::new(capacity as u32) {
        Ok(port) => port,
        Err(error) => return native_return(error, 0, 0),
    };
    insert_native_handle(
        state,
        KernelNativeObject::EventPort(port),
        ObjectInterface::EventPort,
        Rights::OBSERVE | Rights::BIND | Rights::DUPLICATE,
    )
}

pub(super) fn event_bind(
    state: &NativeProcessState,
    port_object: &KernelNativeObject,
    source_raw: u64,
    mask: u64,
    user_data: u64,
) -> NativeCallOutcome {
    let KernelNativeObject::EventPort(port) = port_object else {
        return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
    };
    if mask > u32::MAX as u64 {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let source = {
        let handles = state.handles.lock();
        match handles.lookup(NativeHandle::from_raw(source_raw), None, Rights::OBSERVE) {
            Ok(entry) => entry.object.clone(),
            Err(error) => return native_return(error, 0, 0),
        }
    };
    let token = match port.reserve_token() {
        Ok(token) => token,
        Err(error) => return native_return(error, 0, 0),
    };
    let observer = Arc::new(EventObserver {
        port: Arc::downgrade(port),
        token,
    });
    let kind = match source {
        KernelNativeObject::Process(process) => {
            if mask != 1 {
                return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
            }
            let erased: Arc<dyn ProcessExitObserver> = observer.clone();
            let Some(backend) = process
                .group()
                .try_subscribe_process_exit(Arc::downgrade(&erased))
            else {
                return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
            };
            SubscriptionKind::Process { process, backend }
        }
        KernelNativeObject::Stream(file) => {
            let Some(interest) = stream_interest(mask as u32) else {
                return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
            };
            let Some(source) = file.poll_source() else {
                return native_return(status::EVENT_SOURCE_UNSUPPORTED, 0, 0);
            };
            source.enable_tracking();
            let erased: Arc<dyn PollSubscriber> = observer.clone();
            let backend = match source.try_subscribe(Arc::downgrade(&erased)) {
                Ok(backend) => backend,
                Err(()) => return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0),
            };
            SubscriptionKind::Stream {
                source_id: source.id(),
                file,
                backend,
                interest,
                last_generation: 0,
            }
        }
        _ => return native_return(status::EVENT_SOURCE_UNSUPPORTED, 0, 0),
    };
    let subscription = Subscription {
        token,
        source_handle: source_raw,
        user_data,
        observer,
        kind,
        terminal: None,
        terminal_claimed: false,
    };
    if let Err((error, subscription)) = port.install_subscription(subscription) {
        cancel_backend(subscription);
        return native_return(error, 0, 0);
    }
    // 初始 snapshot 与异步回调共用同一 generation 门控，避免 bind 期间的并发
    // readiness 更新被旧值覆盖。
    let initial_stream = {
        let state = port.state.lock();
        state
            .subscriptions
            .iter()
            .find(|entry| entry.token == token)
            .and_then(|entry| match &entry.kind {
                SubscriptionKind::Stream { file, .. } => Some(Arc::clone(file)),
                _ => None,
            })
    };
    if let Some(file) = initial_stream
        && let Some(source) = file.poll_source()
    {
        let (readiness, generation) = source.snapshot();
        port.publish_stream_snapshot(token, source.id(), readiness, generation);
    }
    let source_terminated = {
        let state = port.state.lock();
        state.subscriptions.iter().any(|entry| {
            entry.token == token
                && matches!(&entry.kind, SubscriptionKind::Process { process, .. } if process.group().is_terminated())
        })
    };
    if source_terminated {
        port.publish_process(token);
    }
    native_return(status::OK, token, 0)
}

pub(super) fn event_timer(
    port_object: &KernelNativeObject,
    deadline_ns: u64,
    interval_ns: u64,
    user_data: u64,
) -> NativeCallOutcome {
    let KernelNativeObject::EventPort(port) = port_object else {
        return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
    };
    if deadline_ns == 0 {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let token = match port.reserve_token() {
        Ok(token) => token,
        Err(error) => return native_return(error, 0, 0),
    };
    let observer = Arc::new(EventObserver {
        port: Arc::downgrade(port),
        token,
    });
    let backend = sched::reserve_deadline_observer_id();
    let subscription = Subscription {
        token,
        source_handle: token,
        user_data,
        observer: Arc::clone(&observer),
        kind: SubscriptionKind::Timer {
            backend,
            deadline_ns,
            interval_ns,
            expirations: 0,
        },
        terminal: None,
        terminal_claimed: false,
    };
    if let Err((error, subscription)) = port.install_subscription(subscription) {
        cancel_backend(subscription);
        return native_return(error, 0, 0);
    }
    let erased: Arc<dyn DeadlineObserver> = observer;
    let weak = Arc::downgrade(&erased);
    let registered = match sched::try_register_deadline_observer(backend, deadline_ns, weak.clone())
    {
        Ok(registered) => registered,
        Err(()) => {
            if let Ok(subscription) = port.cancel(token) {
                cancel_backend(subscription);
            }
            return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
        }
    };
    if !registered
        && let Some(next) = port.publish_timer(token, backend, sched::now_ns_public())
        && sched::try_register_deadline_observer_deferred(backend, next, weak).is_err()
    {
        if let Ok(subscription) = port.cancel(token) {
            cancel_backend(subscription);
        }
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    native_return(status::OK, token, 0)
}

pub(super) fn event_cancel(port_object: &KernelNativeObject, token: u64) -> NativeCallOutcome {
    let KernelNativeObject::EventPort(port) = port_object else {
        return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
    };
    match port.cancel(token) {
        Ok(subscription) => {
            cancel_backend(subscription);
            native_return(status::OK, 0, 0)
        }
        Err(error) => native_return(error, 0, 0),
    }
}

pub(super) fn event_wait(
    task: &Arc<Task>,
    port_object: &KernelNativeObject,
    user: u64,
    capacity: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let KernelNativeObject::EventPort(port) = port_object else {
        return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
    };
    if capacity == 0 || capacity > wire::MAX_EVENT_BATCH as u64 || user == 0 {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let Some(_claim) = port.claim_wait() else {
        return native_return(status::EVENT_WOULD_BLOCK, 0, 0);
    };
    let user = match usize::try_from(user) {
        Ok(user) => user,
        Err(_) => return native_return(status::STREAM_FAULT, 0, 0),
    };
    loop {
        if let Err(outcome) = wait_for_event_port(task, port, deadline_ns) {
            return outcome;
        }
        let mut records = [EventRecord::default(); wire::MAX_EVENT_BATCH as usize];
        let selection = port.peek(&mut records[..capacity as usize]);
        if selection.count == 0 {
            continue;
        }
        let bytes = unsafe {
            core::slice::from_raw_parts(
                records.as_ptr().cast::<u8>(),
                selection.count * size_of::<EventRecord>(),
            )
        };
        if copy_to_user(user, bytes).is_err() {
            port.rollback_peek(&selection);
            return native_return(status::STREAM_FAULT, 0, 0);
        }
        port.commit_peek(&selection);
        return native_return(status::OK, selection.count as u64, 0);
    }
}

fn wait_for_event_port(
    task: &Arc<Task>,
    port: &EventPort,
    deadline_ns: u64,
) -> Result<(), NativeCallOutcome> {
    while !port.has_events() {
        if has_native_external_control(task) {
            return Err(NativeCallOutcome::RetryExternalControl);
        }
        if deadline_ns != 0 && sched::now_ns_public() >= deadline_ns {
            return Err(native_return(status::EVENT_TIMEOUT, 0, 0));
        }
        let entry = port.waiters.prepare_to_wait(task, TaskState::Sleeping);
        let deadline_armed = deadline_ns != 0 && sched::register_sleep_deadline(task, deadline_ns);
        if deadline_ns != 0 && !deadline_armed {
            port.waiters.finish_wait(&entry);
            restore_native_task_after_wait(task);
            return Err(native_return(status::EVENT_TIMEOUT, 0, 0));
        }
        if port.has_events() {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            port.waiters.finish_wait(&entry);
            restore_native_task_after_wait(task);
            break;
        }
        if has_native_external_control(task) {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            port.waiters.finish_wait(&entry);
            restore_native_task_after_wait(task);
            return Err(NativeCallOutcome::RetryExternalControl);
        }
        sched::schedule_once(sched::now_ns_public());
        if deadline_armed {
            sched::cancel_sleep_deadline(task);
        }
        port.waiters.finish_wait(&entry);
        restore_native_task_after_wait(task);
    }
    Ok(())
}

fn take_sequence(state: &mut EventState) -> u64 {
    let sequence = state.next_sequence;
    state.next_sequence = state.next_sequence.checked_add(1).unwrap_or(1);
    sequence
}

fn stream_interest(mask: u32) -> Option<PollEvents> {
    if mask == 0
        || mask
            & !(wire::EVENT_STREAM_READABLE
                | wire::EVENT_STREAM_WRITABLE
                | wire::EVENT_STREAM_ERROR
                | wire::EVENT_STREAM_CLOSED)
            != 0
    {
        return None;
    }
    let mut events = PollEvents::default();
    if mask & wire::EVENT_STREAM_READABLE != 0 {
        events = events.with(PollEvents::POLLIN);
    }
    if mask & wire::EVENT_STREAM_WRITABLE != 0 {
        events = events.with(PollEvents::POLLOUT);
    }
    if mask & wire::EVENT_STREAM_ERROR != 0 {
        events = events.with(PollEvents::POLLERR);
    }
    if mask & wire::EVENT_STREAM_CLOSED != 0 {
        events = events.with(PollEvents::POLLHUP).with(PollEvents::POLLRDHUP);
    }
    Some(events)
}

fn native_stream_events(events: PollEvents) -> u32 {
    let mut mask = 0;
    if events.has(PollEvents::POLLIN) {
        mask |= wire::EVENT_STREAM_READABLE;
    }
    if events.has(PollEvents::POLLOUT) {
        mask |= wire::EVENT_STREAM_WRITABLE;
    }
    if events.has(PollEvents::POLLERR) {
        mask |= wire::EVENT_STREAM_ERROR;
    }
    if events.has(PollEvents::POLLHUP) || events.has(PollEvents::POLLRDHUP) {
        mask |= wire::EVENT_STREAM_CLOSED;
    }
    mask
}

fn cancel_backend(subscription: Subscription) {
    match subscription.kind {
        SubscriptionKind::Process {
            process, backend, ..
        } => {
            process.group().unsubscribe_process_exit(backend);
        }
        SubscriptionKind::Stream { file, backend, .. } => {
            if let Some(source) = file.poll_source() {
                source.unsubscribe(backend);
            }
        }
        SubscriptionKind::Timer { backend, .. } => {
            sched::cancel_deadline_observer(backend);
        }
    }
    drop(subscription.observer);
}

#[cfg(any(feature = "kernel-tests", feature = "soyo-tests"))]
mod tests {
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::any::Any;
    use core::ops::ControlFlow;
    use core::sync::atomic::{AtomicBool, Ordering};

    use ktest::ktest;
    use vfs::anon;
    use vfs::cred::Credentials;
    use vfs::error::{VfsError, VfsResult};
    use vfs::eventfd::EventfdFileOps;
    use vfs::file::{AccessMode, DirEntry, FileOps, OpenOptions};
    use vfs::poll_source::PollSource;

    use super::*;

    fn timer_subscription(port: &Arc<EventPort>, token: u64, interval_ns: u64) -> Subscription {
        let observer = Arc::new(EventObserver {
            port: Arc::downgrade(port),
            token,
        });
        Subscription {
            token,
            source_handle: token,
            user_data: 0x55aa,
            observer,
            kind: SubscriptionKind::Timer {
                backend: token,
                deadline_ns: 100,
                interval_ns,
                expirations: 0,
            },
            terminal: None,
            terminal_claimed: false,
        }
    }

    fn stream_subscription(port: &Arc<EventPort>, token: u64, file: Arc<File>) -> Subscription {
        stream_subscription_with_interest(port, token, file, PollEvents::POLLIN)
    }

    fn stream_subscription_with_interest(
        port: &Arc<EventPort>,
        token: u64,
        file: Arc<File>,
        interest: PollEvents,
    ) -> Subscription {
        let source = file.poll_source().expect("测试流必须暴露 PollSource");
        let observer = Arc::new(EventObserver {
            port: Arc::downgrade(port),
            token,
        });
        Subscription {
            token,
            source_handle: token,
            user_data: 0x77,
            observer,
            kind: SubscriptionKind::Stream {
                source_id: source.id(),
                file,
                backend: 0,
                interest,
                last_generation: 0,
            },
            terminal: None,
            terminal_claimed: false,
        }
    }

    fn readable_eventfd() -> Arc<File> {
        anon::new_file(
            Arc::new(Credentials::root()),
            OpenOptions {
                access: AccessMode::ReadWrite,
                ..Default::default()
            },
            Box::new(EventfdFileOps::new(1, false)),
        )
    }

    struct LockProbeFileOps {
        source: PollSource,
        port: Weak<EventPort>,
        lock_was_free: Arc<AtomicBool>,
    }

    impl FileOps for LockProbeFileOps {
        fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
            Err(VfsError::BadFileDescriptor)
        }

        fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
            Err(VfsError::BadFileDescriptor)
        }

        fn readdir(
            &self,
            _pos: u64,
            _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
        ) -> VfsResult<u64> {
            Err(VfsError::NotADirectory)
        }

        fn sync(&self) -> VfsResult<()> {
            Ok(())
        }

        fn poll(&self, _interest: PollEvents) -> PollEvents {
            self.source.snapshot().0
        }

        fn poll_source(&self) -> Option<&PollSource> {
            let lock_was_free = self
                .port
                .upgrade()
                .is_some_and(|port| port.state.try_lock().is_some());
            self.lock_was_free.store(lock_was_free, Ordering::Release);
            Some(&self.source)
        }

        fn release(&self) {}

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn lock_probe_file(port: &Arc<EventPort>, lock_was_free: Arc<AtomicBool>) -> Arc<File> {
        anon::new_file(
            Arc::new(Credentials::root()),
            OpenOptions {
                access: AccessMode::ReadWrite,
                ..Default::default()
            },
            Box::new(LockProbeFileOps {
                source: PollSource::new(PollEvents::POLLIN),
                port: Arc::downgrade(port),
                lock_was_free,
            }),
        )
    }

    #[ktest]
    fn stale_stream_generation_cannot_requeue_old_readiness() {
        let port = EventPort::new(2).expect("EventPort 应创建成功");
        let file = readable_eventfd();
        let source = file.poll_source().expect("测试流必须暴露 PollSource");
        let token = port.reserve_token().expect("token 应可分配");
        assert!(
            port.install_subscription(stream_subscription(&port, token, Arc::clone(&file)))
                .is_ok()
        );

        port.publish_stream(token, source.id(), PollEvents::default(), 2);
        port.publish_stream(token, source.id(), PollEvents::POLLIN, 1);

        assert!(!port.has_events());
    }

    #[ktest]
    fn newer_stream_generation_replaces_queued_readiness() {
        let port = EventPort::new(2).expect("EventPort 应创建成功");
        let file = readable_eventfd();
        let source = file.poll_source().expect("测试流必须暴露 PollSource");
        let token = port.reserve_token().expect("token 应可分配");
        assert!(
            port.install_subscription(stream_subscription_with_interest(
                &port,
                token,
                Arc::clone(&file),
                PollEvents::POLLIN.with(PollEvents::POLLOUT),
            ))
            .is_ok()
        );

        port.publish_stream(token, source.id(), PollEvents::POLLIN, 2);
        port.publish_stream(token, source.id(), PollEvents::POLLOUT, 3);

        let mut records = [EventRecord::default(); 1];
        let selection = port.peek(&mut records);
        assert_eq!(selection.count, 1);
        assert_eq!(records[0].value0, u64::from(wire::EVENT_STREAM_WRITABLE));
    }

    #[ktest]
    fn newer_non_ready_generation_revokes_queued_readiness() {
        let port = EventPort::new(2).expect("EventPort 应创建成功");
        let file = readable_eventfd();
        let source = file.poll_source().expect("测试流必须暴露 PollSource");
        let token = port.reserve_token().expect("token 应可分配");
        assert!(
            port.install_subscription(stream_subscription(&port, token, Arc::clone(&file)))
                .is_ok()
        );

        port.publish_stream(token, source.id(), PollEvents::POLLIN, 2);
        port.publish_stream(token, source.id(), PollEvents::default(), 3);

        assert!(!port.has_events());
    }

    #[ktest]
    fn level_stream_readiness_is_requeued_after_delivery() {
        let port = EventPort::new(2).expect("EventPort 应创建成功");
        let file = readable_eventfd();
        let source = file.poll_source().expect("测试流必须暴露 PollSource");
        let token = port.reserve_token().expect("token 应可分配");
        assert!(
            port.install_subscription(stream_subscription(&port, token, Arc::clone(&file)))
                .is_ok()
        );
        let (ready, generation) = source.snapshot();
        port.publish_stream(token, source.id(), ready, generation);

        let mut records = [EventRecord::default(); 1];
        let first = port.peek(&mut records);
        assert_eq!(first.count, 1);
        port.commit_peek(&first);

        let second = port.peek(&mut records);
        assert_eq!(second.count, 1);
        assert_eq!(records[0].event_kind, wire::EVENT_KIND_STREAM_READY);
    }

    #[ktest]
    fn level_refresh_reads_poll_source_without_holding_event_state_lock() {
        let port = EventPort::new(2).expect("EventPort 应创建成功");
        let lock_was_free = Arc::new(AtomicBool::new(false));
        let file = lock_probe_file(&port, Arc::clone(&lock_was_free));
        let source = file.poll_source().expect("测试流必须暴露 PollSource");
        let token = port.reserve_token().expect("token 应可分配");
        assert!(
            port.install_subscription(stream_subscription(&port, token, Arc::clone(&file)))
                .is_ok()
        );
        let (ready, generation) = source.snapshot();
        port.publish_stream(token, source.id(), ready, generation);

        let mut records = [EventRecord::default(); 1];
        let selection = port.peek(&mut records);
        assert_eq!(selection.count, 1);
        lock_was_free.store(false, Ordering::Release);
        port.commit_peek(&selection);

        assert!(lock_was_free.load(Ordering::Acquire));
    }

    #[ktest]
    fn timer_events_coalesce_without_advancing_sequence_twice() {
        let port = EventPort::new(1).expect("EventPort 应创建成功");
        let token = port.reserve_token().expect("token 应可分配");
        assert!(
            port.install_subscription(timer_subscription(&port, token, 10))
                .is_ok()
        );

        assert_eq!(port.publish_timer(token, token, 100), Some(110));
        assert_eq!(port.publish_timer(token, token, 110), Some(120));

        let mut records = [EventRecord::default(); 1];
        let selection = port.peek(&mut records);
        assert_eq!(selection.count, 1);
        assert_eq!(records[0].event_kind, wire::EVENT_KIND_TIMER_EXPIRED);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[0].value0, 2);
        assert_eq!(records[0].value1, 0x55aa);
    }

    #[ktest]
    fn process_terminal_event_survives_a_full_regular_queue() {
        let port = EventPort::new(1).expect("EventPort 应创建成功");
        let owner = sched::ThreadGroup::new();
        let child = sched::ThreadGroup::new();
        child.request_group_exit(0x7654_3210u32 as i32);
        assert!(child.mark_terminated_if_all_members_terminal());
        let process = ProcessObject::new(child, &owner);
        let token = port.reserve_token().expect("token 应可分配");
        let observer = Arc::new(EventObserver {
            port: Arc::downgrade(&port),
            token,
        });
        assert!(
            port.install_subscription(Subscription {
                token,
                source_handle: 0x1234,
                user_data: 0x99,
                observer,
                kind: SubscriptionKind::Process {
                    process,
                    backend: 0,
                },
                terminal: None,
                terminal_claimed: false,
            })
            .is_ok()
        );
        {
            let mut state = port.state.lock();
            let sequence = take_sequence(&mut state);
            state.queue.push_back(QueueEntry {
                token: u64::MAX,
                record: EventRecord {
                    event_kind: wire::EVENT_KIND_STREAM_READY,
                    sequence,
                    ..EventRecord::default()
                },
                claimed: false,
            });
        }

        port.publish_process(token);

        let mut records = [EventRecord::default(); 2];
        let selection = port.peek(&mut records);
        assert_eq!(selection.count, 2);
        assert_eq!(records[0].event_kind, wire::EVENT_KIND_STREAM_READY);
        assert_eq!(records[1].event_kind, wire::EVENT_KIND_PROCESS_EXITED);
        assert_eq!(records[1].value0 as u32, 0x7654_3210);
    }
}
