//! 常驻网络协议栈 broker。
//!
//! 常驻 host 负责 `net.stack` generation 生命周期和 worker-turn pinned batch 调用；
//! packet ownership 只在完整 sidecar 通过校验后由调用方移动。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use net::stack::{
    NET_STACK_CALL_RUST_ABI, NET_STACK_CALL_STATUS_OK, NET_STACK_OP_PROBE,
    NET_STACK_OP_WORKER_TURN, NetStackCallV1, NetStackEndpoint, NetStackHandle, NetStackLifecycle,
    NetStackRegisterError, NetStackRegisterErrorKind, NetStackRegistrar, NetStackRegistration,
    NetStackRemoveError, NetStackSnapshot, NetStackState, NetStackWorkerTurnV1,
};
use sched::sync::Spinlock;

static HOST_STARTED: AtomicBool = AtomicBool::new(false);
static BROKER: Spinlock<KernelNetStackBroker> = Spinlock::new(KernelNetStackBroker::new());

struct KernelNetStackRegistrar;

enum StackCall {
    Integrated(net::stack::IntegratedNetStackCall),
    Pinned(Arc<Spinlock<crate::elm::PinnedNativeCall>>),
}

impl Clone for StackCall {
    fn clone(&self) -> Self {
        match self {
            Self::Integrated(call) => Self::Integrated(*call),
            Self::Pinned(call) => Self::Pinned(Arc::clone(call)),
        }
    }
}

impl StackCall {
    fn invoke(
        &self,
        frame: &mut NetStackCallV1,
        host_ranges: &[(usize, usize)],
    ) -> Result<i32, i32> {
        match self {
            Self::Integrated(call) => Ok(call(frame)),
            Self::Pinned(call) => {
                let deadline = sched::now_ns_public().saturating_add(2_000_000);
                crate::elm::invoke_pinned_native(&call.lock(), frame, host_ranges, deadline)
            }
        }
    }
}

struct StackRecord {
    handle: NetStackHandle,
    generation: u64,
    call: StackCall,
}

struct KernelNetStackBroker {
    lifecycle: NetStackLifecycle,
    record: Option<StackRecord>,
}

impl KernelNetStackBroker {
    const fn new() -> Self {
        Self {
            lifecycle: NetStackLifecycle::new(),
            record: None,
        }
    }

    fn build_call(endpoint: &NetStackEndpoint) -> Result<StackCall, ()> {
        match endpoint {
            NetStackEndpoint::Integrated(call) if *call as usize != 0 => {
                Ok(StackCall::Integrated(*call))
            }
            NetStackEndpoint::Integrated(_) => Err(()),
            NetStackEndpoint::Pinned(endpoint) => {
                let call = crate::elm::PinnedNativeCall::new(
                    elm_model::ElmId(endpoint.owner_cell()),
                    elm_model::Generation(endpoint.owner_generation()),
                    endpoint.export_name(),
                    endpoint.export_contract(),
                    endpoint.export_version(),
                    NET_STACK_CALL_RUST_ABI,
                )
                .map_err(|_| ())?;
                Ok(StackCall::Pinned(Arc::new(Spinlock::new(call))))
            }
        }
    }
}

impl NetStackRegistrar for KernelNetStackRegistrar {
    fn register_stack(
        &self,
        registration: NetStackRegistration,
    ) -> Result<NetStackHandle, NetStackRegisterError> {
        let handle = registration.handle();
        let owner_cell = registration.owner_cell();
        let generation = registration.generation();
        let mut broker = BROKER.lock();
        if broker.lifecycle.snapshot().state != net::stack::NetStackState::Absent {
            return Err(NetStackRegisterError {
                kind: NetStackRegisterErrorKind::AlreadyActive,
                registration,
            });
        }
        let call = match KernelNetStackBroker::build_call(registration.endpoint()) {
            Ok(call) => call,
            Err(()) => {
                return Err(NetStackRegisterError {
                    kind: NetStackRegisterErrorKind::ResourceExhausted,
                    registration,
                });
            }
        };
        if let Err(kind) = broker.lifecycle.activate(handle, owner_cell, generation) {
            return Err(NetStackRegisterError { kind, registration });
        }
        broker.record = Some(StackRecord {
            handle,
            generation,
            call,
        });
        log::info!(
            "[net-stack] registered generation: cell={} generation={} handle={}",
            owner_cell,
            generation,
            handle.0
        );
        Ok(handle)
    }

    fn begin_remove(
        &self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRemoveError> {
        let mut broker = BROKER.lock();
        broker
            .lifecycle
            .begin_remove(handle, owner_cell, generation)?;
        if !broker.lifecycle.begin_drain(handle) {
            return Err(NetStackRemoveError::Busy);
        }
        broker.record = None;
        if !broker.lifecycle.finish_remove(handle) {
            return Err(NetStackRemoveError::Busy);
        }
        log::info!(
            "[net-stack] removed generation: cell={} generation={} handle={}",
            owner_cell,
            generation,
            handle.0
        );
        Ok(())
    }

    fn snapshot(&self) -> NetStackSnapshot {
        BROKER.lock().lifecycle.snapshot()
    }
}

pub(crate) fn registrar() -> &'static dyn NetStackRegistrar {
    &KernelNetStackRegistrar
}

fn on_elm_lifecycle_event(event: crate::elm::ElmLifecycleEvent) {
    match event {
        crate::elm::ElmLifecycleEvent::CellLoaded { .. } => probe_active(),
    }
}

/// 启动允许 stack 缺席的常驻 host，并探测已经由 BuildBound 激活的 generation。
pub(crate) fn start_host() {
    if HOST_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    assert!(
        crate::elm::register_lifecycle_observer("net-stack", on_elm_lifecycle_event),
        "无法注册 net.stack 生命周期观察者"
    );
    if net::stack::stack_snapshot().state == net::stack::NetStackState::Absent {
        log::info!("[net-stack] host started without stack generation");
        return;
    }
    probe_active();
}

fn probe_active() {
    if !HOST_STARTED.load(Ordering::Acquire) {
        return;
    }
    let (handle, generation, call) = {
        let broker = BROKER.lock();
        let Some(record) = broker.record.as_ref() else {
            return;
        };
        (record.handle, record.generation, record.call.clone())
    };
    let mut frame = NetStackCallV1::new(NET_STACK_OP_PROBE, generation);
    let result = call.invoke(&mut frame, &[]);
    let success = matches!(result, Ok(NET_STACK_CALL_STATUS_OK))
        && frame.valid(NET_STACK_OP_PROBE, generation)
        && frame.ready == 1
        && frame.quiesced == 0;
    let mut broker = BROKER.lock();
    if success {
        if broker.lifecycle.mark_probed(handle) {
            log::info!(
                "[net-stack] generation probe succeeded: generation={} handle={}",
                generation,
                handle.0
            );
        }
    } else if broker.lifecycle.mark_faulted(handle) {
        log::error!(
            "[net-stack] generation probe failed: generation={} handle={} result={:?}",
            generation,
            handle.0,
            result
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerTurnError {
    StackUnavailable,
    CallFailed,
}

/// 在当前 active generation 中执行一次只读 RX batch turn。
pub(crate) fn worker_turn(
    input: &net::buf::PacketBatch,
    interface: net::InterfaceId,
    config: &net::control::ConfigSnapshot,
) -> Result<NetStackWorkerTurnV1, WorkerTurnError> {
    let (handle, generation, call) = {
        let broker = BROKER.lock();
        let snapshot = broker.lifecycle.snapshot();
        if snapshot.state != NetStackState::Active || !snapshot.probed {
            return Err(WorkerTurnError::StackUnavailable);
        }
        let Some(record) = broker.record.as_ref() else {
            return Err(WorkerTurnError::StackUnavailable);
        };
        (record.handle, record.generation, record.call.clone())
    };

    let input_pointer = input as *const net::buf::PacketBatch;
    let input_count = input.len() as u8;
    let local_addresses = config.stack_local_addresses();
    let Ok(local_address_count) = u32::try_from(local_addresses.len()) else {
        return Err(WorkerTurnError::StackUnavailable);
    };
    if interface.0 == 0
        || !local_addresses
            .iter()
            .all(net::stack::NetStackLocalAddressV1::valid)
    {
        return Err(WorkerTurnError::StackUnavailable);
    }
    let local_address_pointer = local_addresses.as_ptr();
    let mut turn = NetStackWorkerTurnV1::new(
        generation,
        config.generation,
        interface.0,
        local_addresses,
        input,
    );
    let turn_pointer = &mut turn as *mut NetStackWorkerTurnV1;
    let mut frame = NetStackCallV1::new(NET_STACK_OP_WORKER_TURN, generation);
    frame.worker_turn = turn_pointer;
    let Some(turn_range) = host_range(turn_pointer) else {
        return Err(WorkerTurnError::CallFailed);
    };
    let Some(input_range) = host_range(input_pointer) else {
        return Err(WorkerTurnError::CallFailed);
    };
    let mut host_ranges = [(0usize, 0usize); 3];
    host_ranges[0] = turn_range;
    host_ranges[1] = input_range;
    let range_count = if local_addresses.is_empty() {
        2
    } else {
        let Some(address_range) = host_slice_range(local_addresses) else {
            return Err(WorkerTurnError::CallFailed);
        };
        host_ranges[2] = address_range;
        3
    };
    let result = call.invoke(&mut frame, &host_ranges[..range_count]);
    let valid = matches!(result, Ok(NET_STACK_CALL_STATUS_OK))
        && frame.valid(NET_STACK_OP_WORKER_TURN, generation)
        && frame.worker_turn == turn_pointer
        && frame.ready == 0
        && frame.quiesced == 0
        && turn.valid_header(
            generation,
            config.generation,
            interface.0,
            input_pointer,
            local_address_pointer,
            local_address_count,
        )
        && turn.input_count == input_count
        && input.len() == usize::from(input_count)
        && local_addresses
            .iter()
            .all(net::stack::NetStackLocalAddressV1::valid)
        && turn.fully_committed(input);
    if valid {
        return Ok(turn);
    }

    let mut broker = BROKER.lock();
    let current = broker
        .record
        .as_ref()
        .is_some_and(|record| record.handle == handle && record.generation == generation);
    if current && broker.lifecycle.mark_faulted(handle) {
        log::error!(
            "[net-stack] worker turn failed: generation={} handle={} result={:?}",
            generation,
            handle.0,
            result
        );
    }
    Err(WorkerTurnError::CallFailed)
}

fn host_range<T>(pointer: *const T) -> Option<(usize, usize)> {
    let start = pointer as usize;
    let end = start.checked_add(core::mem::size_of::<T>())?;
    (start != 0 && start < end).then_some((start, end))
}

fn host_slice_range<T>(slice: &[T]) -> Option<(usize, usize)> {
    let start = slice.as_ptr() as usize;
    let bytes = core::mem::size_of::<T>().checked_mul(slice.len())?;
    let end = start.checked_add(bytes)?;
    (start != 0 && start < end).then_some((start, end))
}
