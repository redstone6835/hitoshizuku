//! 常驻网络协议栈 broker。
//!
//! 当前阶段只负责 `net.stack` generation 的注册、探测与撤销；真实 packet 和
//! socket 数据面仍由 `net_runtime` 独占，后续迁移 worker turn 时再接入调用帧。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use net::stack::{
    NET_STACK_CALL_RUST_ABI, NET_STACK_CALL_STATUS_OK, NET_STACK_OP_PROBE, NetStackCallV1,
    NetStackEndpoint, NetStackHandle, NetStackLifecycle, NetStackRegisterError,
    NetStackRegisterErrorKind, NetStackRegistrar, NetStackRegistration, NetStackRemoveError,
    NetStackSnapshot,
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
    fn invoke(&self, frame: &mut NetStackCallV1) -> Result<i32, i32> {
        match self {
            Self::Integrated(call) => Ok(call(frame)),
            Self::Pinned(call) => {
                let deadline = sched::now_ns_public().saturating_add(2_000_000);
                crate::elm::invoke_pinned_native(&call.lock(), frame, &[], deadline)
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
    let result = call.invoke(&mut frame);
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
