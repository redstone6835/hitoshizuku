//! 常驻 VFS socket 代理与 ELM readiness 路由。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use sched::{Task, TaskState, WaitQueue};
use spin::RwLock;

use super::{
    MulticastMembership, OwnerRef, Readiness, ReadinessObserver, SocketError, SocketErrorRecord,
    SocketKind, TcpInfoSnapshot, UdpReceive, socket_runtime,
};
use crate::control::BindOptions;
use crate::stack::{
    NET_STACK_SOCKET_FAMILY_IPV4, NET_STACK_SOCKET_FAMILY_IPV6, NET_STACK_SOCKET_KIND_DATAGRAM,
    NET_STACK_SOCKET_KIND_RAW, NET_STACK_SOCKET_KIND_STREAM, NetStackSocketCommandV1,
    NetStackSocketDescriptorV1, NetStackSocketErrorV1, NetStackSocketMulticastActionV1,
    NetStackSocketOptionV1, NetStackSocketOptionValueV1, NetStackSocketRecvV1, NetStackSocketRefV1,
    NetStackSocketSnapshotV1,
};
use crate::{AddressFamily, Endpoint, InterfaceId};

static NEXT_PROXY_GENERATION: AtomicU64 = AtomicU64::new(1);
static PROXY_STATES: RwLock<Vec<Weak<ProxyState>>> = RwLock::new(Vec::new());

struct ProxyState {
    socket: NetStackSocketRefV1,
    stack_generation: u64,
    stack_instance: u64,
    proxy_generation: u64,
    readiness: AtomicU16,
    readiness_generation: AtomicU64,
    detached: AtomicBool,
    observer: RwLock<Option<Weak<dyn ReadinessObserver>>>,
    read_wait: WaitQueue,
    write_wait: WaitQueue,
    state_wait: WaitQueue,
}

impl ProxyState {
    fn new(
        descriptor: NetStackSocketDescriptorV1,
        stack_generation: u64,
        stack_instance: u64,
        proxy_generation: u64,
    ) -> Self {
        Self {
            socket: descriptor.socket,
            stack_generation,
            stack_instance,
            proxy_generation,
            readiness: AtomicU16::new(descriptor.readiness),
            readiness_generation: AtomicU64::new(descriptor.readiness_generation),
            detached: AtomicBool::new(false),
            observer: RwLock::new(None),
            read_wait: WaitQueue::new_with_reason(sched::WaitReason::SocketRead),
            write_wait: WaitQueue::new_with_reason(sched::WaitReason::SocketWrite),
            state_wait: WaitQueue::new_with_reason(sched::WaitReason::Poll),
        }
    }

    fn readiness(&self) -> (Readiness, u64) {
        if self.detached.load(Ordering::Acquire) {
            return (
                Readiness::ERROR | Readiness::HANGUP | Readiness::READ_HANGUP,
                self.readiness_generation.load(Ordering::Acquire),
            );
        }
        (
            Readiness(self.readiness.load(Ordering::Acquire)),
            self.readiness_generation.load(Ordering::Acquire),
        )
    }

    fn publish(&self, readiness: Readiness, generation: u64) {
        let current = self.readiness_generation.load(Ordering::Acquire);
        if generation < current || self.detached.load(Ordering::Acquire) {
            return;
        }
        self.readiness.store(readiness.raw(), Ordering::Release);
        self.readiness_generation
            .store(generation.max(current), Ordering::Release);
        if readiness.contains(Readiness::READABLE) || readiness.contains(Readiness::ACCEPTABLE) {
            self.read_wait.wake_one_default();
        }
        if readiness.contains(Readiness::WRITABLE) {
            self.write_wait.wake_one_default();
        }
        if readiness.contains(Readiness::ERROR)
            || readiness.contains(Readiness::HANGUP)
            || readiness.contains(Readiness::READ_HANGUP)
        {
            self.read_wait.wake_all();
            self.write_wait.wake_all();
        }
        self.state_wait.wake_all();
        if let Some(observer) = self.observer.read().as_ref().and_then(Weak::upgrade) {
            observer.readiness_changed(readiness, generation.max(current));
        }
    }

    fn detach(&self) {
        if self.detached.swap(true, Ordering::AcqRel) {
            return;
        }
        let generation = self.readiness_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let readiness = Readiness::ERROR | Readiness::HANGUP | Readiness::READ_HANGUP;
        self.readiness.store(readiness.raw(), Ordering::Release);
        self.read_wait.wake_all();
        self.write_wait.wake_all();
        self.state_wait.wake_all();
        if let Some(observer) = self.observer.read().as_ref().and_then(Weak::upgrade) {
            observer.readiness_changed(readiness, generation);
        }
    }
}

struct SocketReadinessRelay {
    socket: NetStackSocketRefV1,
    stack_generation: u64,
}

impl ReadinessObserver for SocketReadinessRelay {
    fn readiness_changed(&self, readiness: Readiness, generation: u64) {
        publish_proxy_readiness(self.socket, self.stack_generation, readiness, generation);
    }
}

pub(crate) fn new_socket_readiness_relay(
    socket: NetStackSocketRefV1,
    stack_generation: u64,
) -> Arc<dyn ReadinessObserver> {
    Arc::new(SocketReadinessRelay {
        socket,
        stack_generation,
    })
}

fn publish_proxy_readiness(
    socket: NetStackSocketRefV1,
    stack_generation: u64,
    readiness: Readiness,
    generation: u64,
) {
    let states = {
        let mut registry = PROXY_STATES.write();
        let mut states = Vec::new();
        registry.retain(|entry| {
            let Some(state) = entry.upgrade() else {
                return false;
            };
            if state.socket == socket && state.stack_generation == stack_generation {
                states.push(state);
            }
            true
        });
        states
    };
    for state in states {
        state.publish(readiness, generation);
    }
}

/// 使一个 stack 注册实例的全部常驻 proxy 失效并唤醒 waiter。
pub fn detach_proxy_stack(instance: u64) -> usize {
    if instance == 0 {
        return 0;
    }
    let states = {
        let mut registry = PROXY_STATES.write();
        let mut states = Vec::new();
        registry.retain(|entry| {
            let Some(state) = entry.upgrade() else {
                return false;
            };
            if state.stack_instance == instance {
                states.push(state);
            }
            true
        });
        states
    };
    for state in &states {
        state.detach();
    }
    states.len()
}

/// VFS 持有的常驻 socket 身份，不包含协议状态或指向 ELM 对象的引用。
pub struct NetSocketProxy {
    family: AddressFamily,
    kind: SocketKind,
    protocol: u8,
    socket: NetStackSocketRefV1,
    stack_generation: u64,
    stack_instance: u64,
    proxy_generation: u64,
    state: Arc<ProxyState>,
}

impl NetSocketProxy {
    pub fn create(
        family: AddressFamily,
        kind: SocketKind,
        protocol: u8,
        stack_generation: u64,
        stack_instance: u64,
    ) -> Result<Self, SocketError> {
        if stack_generation == 0 || stack_instance == 0 {
            return Err(SocketError::RuntimeUnavailable);
        }
        let command = submit_stack_command(NetStackSocketCommandV1::Create {
            family: family_raw(family),
            kind: kind_raw(kind),
            protocol,
            output: None,
        })?;
        let descriptor = match command {
            NetStackSocketCommandV1::Create {
                output: Some(Ok(descriptor)),
                ..
            } => descriptor,
            NetStackSocketCommandV1::Create {
                output: Some(Err(error)),
                ..
            } => return Err(map_stack_error(error)),
            _ => return Err(SocketError::RuntimeBusy),
        };
        if descriptor.family != family_raw(family)
            || descriptor.kind != kind_raw(kind)
            || descriptor.protocol == 0
        {
            close_stack_socket(descriptor.socket);
            return Err(SocketError::RuntimeBusy);
        }
        Self::register(descriptor, family, kind, stack_generation, stack_instance)
    }

    fn register(
        descriptor: NetStackSocketDescriptorV1,
        family: AddressFamily,
        kind: SocketKind,
        stack_generation: u64,
        stack_instance: u64,
    ) -> Result<Self, SocketError> {
        let proxy_generation = NEXT_PROXY_GENERATION.fetch_add(1, Ordering::Relaxed);
        if proxy_generation == 0 {
            close_stack_socket(descriptor.socket);
            return Err(SocketError::Buffer);
        }
        let state = Arc::new(ProxyState::new(
            descriptor,
            stack_generation,
            stack_instance,
            proxy_generation,
        ));
        let mut registry = PROXY_STATES.write();
        registry.retain(|entry| entry.strong_count() != 0);
        if registry.iter().filter_map(Weak::upgrade).any(|entry| {
            entry.socket == descriptor.socket && entry.stack_instance == stack_instance
        }) {
            drop(registry);
            close_stack_socket(descriptor.socket);
            return Err(SocketError::RuntimeBusy);
        }
        registry.push(Arc::downgrade(&state));
        drop(registry);
        Ok(Self {
            family,
            kind,
            protocol: descriptor.protocol,
            socket: descriptor.socket,
            stack_generation,
            stack_instance,
            proxy_generation,
            state,
        })
    }

    pub const fn family(&self) -> AddressFamily {
        self.family
    }

    pub const fn kind(&self) -> SocketKind {
        self.kind
    }

    pub const fn protocol(&self) -> u8 {
        self.protocol
    }

    pub const fn socket_ref(&self) -> NetStackSocketRefV1 {
        self.socket
    }

    pub const fn stack_generation(&self) -> u64 {
        self.stack_generation
    }

    pub const fn proxy_generation(&self) -> u64 {
        self.proxy_generation
    }

    fn ensure_backend(&self) -> Result<(), SocketError> {
        if self.state.detached.load(Ordering::Acquire) {
            return Err(SocketError::NetworkDown);
        }
        let snapshot = crate::stack::stack_snapshot();
        if snapshot.state != crate::stack::NetStackState::Active
            || !snapshot.probed
            || snapshot.generation != self.stack_generation
            || snapshot
                .handle
                .is_none_or(|handle| handle.0 != self.stack_instance)
        {
            self.state.detach();
            return Err(SocketError::NetworkDown);
        }
        Ok(())
    }

    pub fn backend_error(&self) -> Option<SocketError> {
        self.ensure_backend().err()
    }

    fn observe_descriptor(&self, descriptor: NetStackSocketDescriptorV1) {
        if descriptor.socket == self.socket {
            self.state.publish(
                Readiness(descriptor.readiness),
                descriptor.readiness_generation,
            );
        }
    }

    fn query(&self) -> Result<NetStackSocketSnapshotV1, SocketError> {
        self.ensure_backend()?;
        match submit_stack_command(NetStackSocketCommandV1::Query {
            socket: self.socket,
            output: None,
        })? {
            NetStackSocketCommandV1::Query {
                output: Some(Ok(snapshot)),
                ..
            } => {
                self.observe_descriptor(snapshot.descriptor);
                Ok(snapshot)
            }
            NetStackSocketCommandV1::Query {
                output: Some(Err(error)),
                ..
            } => Err(map_stack_error(error)),
            _ => Err(SocketError::RuntimeBusy),
        }
    }

    pub fn bind(
        &self,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<(), SocketError> {
        loop {
            self.ensure_backend()?;
            match submit_stack_command(NetStackSocketCommandV1::Bind {
                socket: self.socket,
                local,
                interface,
                options,
                output: None,
            })? {
                NetStackSocketCommandV1::Bind {
                    output: Some(Ok(_)),
                    ..
                } => {
                    let _ = self.query();
                    return Ok(());
                }
                NetStackSocketCommandV1::Bind {
                    output: Some(Err(NetStackSocketErrorV1::InProgress)),
                    ..
                } => self.wait_io(
                    &self.state.state_wait,
                    Readiness::ERROR | Readiness::HANGUP,
                    None,
                )?,
                NetStackSocketCommandV1::Bind {
                    output: Some(Err(error)),
                    ..
                } => return Err(map_stack_error(error)),
                _ => return Err(SocketError::RuntimeBusy),
            }
        }
    }

    pub fn connect_with_mode(
        &self,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        nonblocking: bool,
    ) -> Result<(), SocketError> {
        self.ensure_backend()?;
        let result = match submit_stack_command(NetStackSocketCommandV1::Connect {
            socket: self.socket,
            peer,
            interface,
            options,
            output: None,
        })? {
            NetStackSocketCommandV1::Connect {
                output: Some(Ok(snapshot)),
                ..
            } => {
                self.observe_descriptor(snapshot.descriptor);
                return Ok(());
            }
            NetStackSocketCommandV1::Connect {
                output: Some(Err(error)),
                ..
            } => map_stack_error(error),
            _ => SocketError::RuntimeBusy,
        };
        if result != SocketError::InProgress || nonblocking {
            return Err(result);
        }
        loop {
            self.wait_io(
                &self.state.write_wait,
                Readiness::WRITABLE | Readiness::ERROR | Readiness::HANGUP,
                None,
            )?;
            if let Some(error) = self.take_pending_error() {
                return Err(error);
            }
            let snapshot = self.query()?;
            if matches!(snapshot.owner, OwnerRef::Flow { .. }) {
                return Ok(());
            }
            if snapshot.descriptor.readiness & (Readiness::ERROR | Readiness::HANGUP).raw() != 0 {
                return Err(SocketError::NotConnected);
            }
        }
    }

    pub fn listen(&self, backlog: u32) -> Result<(), SocketError> {
        loop {
            self.ensure_backend()?;
            match submit_stack_command(NetStackSocketCommandV1::Listen {
                socket: self.socket,
                backlog,
                output: None,
            })? {
                NetStackSocketCommandV1::Listen {
                    output: Some(Ok(snapshot)),
                    ..
                } => {
                    self.observe_descriptor(snapshot.descriptor);
                    return Ok(());
                }
                NetStackSocketCommandV1::Listen {
                    output: Some(Err(NetStackSocketErrorV1::InProgress)),
                    ..
                } => self.wait_io(
                    &self.state.state_wait,
                    Readiness::ERROR | Readiness::HANGUP,
                    None,
                )?,
                NetStackSocketCommandV1::Listen {
                    output: Some(Err(error)),
                    ..
                } => return Err(map_stack_error(error)),
                _ => return Err(SocketError::RuntimeBusy),
            }
        }
    }

    pub fn accept(&self, nonblocking: bool, deadline_ns: Option<u64>) -> Result<Self, SocketError> {
        loop {
            self.ensure_backend()?;
            let result = match submit_stack_command(NetStackSocketCommandV1::Accept {
                socket: self.socket,
                output: None,
            })? {
                NetStackSocketCommandV1::Accept {
                    output: Some(Ok(descriptor)),
                    ..
                } => {
                    return Self::register(
                        descriptor,
                        self.family,
                        self.kind,
                        self.stack_generation,
                        self.stack_instance,
                    );
                }
                NetStackSocketCommandV1::Accept {
                    output: Some(Err(error)),
                    ..
                } => map_stack_error(error),
                _ => SocketError::RuntimeBusy,
            };
            if result != SocketError::WouldBlock || nonblocking {
                return Err(result);
            }
            self.wait_io(&self.state.read_wait, Readiness::ACCEPTABLE, deadline_ns)?;
        }
    }

    pub fn shutdown(&self, read: bool, write: bool) -> Result<(), SocketError> {
        self.ensure_backend()?;
        match submit_stack_command(NetStackSocketCommandV1::Shutdown {
            socket: self.socket,
            read,
            write,
            output: None,
        })? {
            NetStackSocketCommandV1::Shutdown {
                output: Some(Ok(snapshot)),
                ..
            } => {
                self.observe_descriptor(snapshot.descriptor);
                Ok(())
            }
            NetStackSocketCommandV1::Shutdown {
                output: Some(Err(error)),
                ..
            } => Err(map_stack_error(error)),
            _ => Err(SocketError::RuntimeBusy),
        }
    }

    pub fn send_datagram(
        &self,
        payload: &[u8],
        destination: Option<Endpoint>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        dont_route: bool,
        confirm: bool,
    ) -> Result<usize, SocketError> {
        loop {
            self.ensure_backend()?;
            let command = NetStackSocketCommandV1::Send {
                socket: self.socket,
                data: payload.as_ptr(),
                len: payload.len().min(u32::MAX as usize) as u32,
                destination,
                dont_route,
                confirm,
                output: None,
            };
            match submit_stack_command(command)? {
                NetStackSocketCommandV1::Send {
                    output: Some(Ok(length)),
                    ..
                } => return Ok(length as usize),
                NetStackSocketCommandV1::Send {
                    output: Some(Err(NetStackSocketErrorV1::WouldBlock)),
                    ..
                } if !nonblocking => {
                    self.wait_io(&self.state.write_wait, Readiness::WRITABLE, deadline_ns)?;
                }
                NetStackSocketCommandV1::Send {
                    output: Some(Err(error)),
                    ..
                } => return Err(map_stack_error(error)),
                _ => return Err(SocketError::RuntimeBusy),
            }
        }
    }

    pub fn send_stream(
        &self,
        payload: &[u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
        let mut written = 0usize;
        loop {
            let result =
                self.send_datagram(&payload[written..], None, true, deadline_ns, false, false);
            match result {
                Ok(length) => {
                    written += length;
                    if written == payload.len() || nonblocking || length == 0 {
                        return Ok(written);
                    }
                }
                Err(SocketError::WouldBlock) if !nonblocking => {
                    self.wait_io(&self.state.write_wait, Readiness::WRITABLE, deadline_ns)?;
                }
                Err(_) if written != 0 => return Ok(written),
                Err(error) => return Err(error),
            }
        }
    }

    fn recv_once(
        &self,
        output: &mut [u8],
        peek: bool,
        truncate: bool,
    ) -> Result<NetStackSocketRecvV1, SocketError> {
        let command = NetStackSocketCommandV1::Recv {
            socket: self.socket,
            data: output.as_mut_ptr(),
            capacity: output.len().min(u32::MAX as usize) as u32,
            peek,
            truncate,
            output: None,
        };
        match submit_stack_command(command)? {
            NetStackSocketCommandV1::Recv {
                output: Some(Ok(received)),
                ..
            } => Ok(received),
            NetStackSocketCommandV1::Recv {
                output: Some(Err(error)),
                ..
            } => Err(map_stack_error(error)),
            _ => Err(SocketError::RuntimeBusy),
        }
    }

    pub fn recv(
        &self,
        output: &mut [u8],
        peek: bool,
        truncate: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<UdpReceive, SocketError> {
        loop {
            self.ensure_backend()?;
            match self.recv_once(output, peek, truncate) {
                Ok(received) => {
                    let unspecified = Endpoint {
                        addr: match self.family {
                            AddressFamily::Ipv4 => crate::IpAddr::V4(crate::Ipv4Addr::UNSPECIFIED),
                            AddressFamily::Ipv6 => crate::IpAddr::V6(crate::Ipv6Addr::UNSPECIFIED),
                        },
                        port: 0,
                    };
                    return Ok(UdpReceive {
                        len: received.len as usize,
                        original_len: received.original_len as usize,
                        source: received.source.unwrap_or(unspecified),
                        destination: received.destination.unwrap_or(unspecified),
                        ingress_interface: received.interface.unwrap_or(InterfaceId(0)),
                        hop_limit: received.hop_limit,
                        traffic_class: received.traffic_class,
                        rx_timestamp_ns: received.rx_timestamp_ns,
                        truncated: received.truncated,
                    });
                }
                Err(SocketError::WouldBlock) if !nonblocking => {
                    self.wait_io(&self.state.read_wait, Readiness::READABLE, deadline_ns)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn recv_stream(
        &self,
        output: &mut [u8],
        peek: bool,
        wait_all: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
        let mut total = 0usize;
        loop {
            self.ensure_backend()?;
            match self.recv_once(&mut output[total..], peek, false) {
                Ok(received) => {
                    let length = received.len as usize;
                    total += length;
                    if length == 0 || total == output.len() || !wait_all || peek || nonblocking {
                        return Ok(total);
                    }
                }
                Err(SocketError::WouldBlock) if !nonblocking => {
                    if total != 0 && !wait_all {
                        return Ok(total);
                    }
                    self.wait_io(&self.state.read_wait, Readiness::READABLE, deadline_ns)?;
                }
                Err(_) if total != 0 => return Ok(total),
                Err(error) => return Err(error),
            }
        }
    }

    pub fn close(&self) {
        self.close_with_deadline(None);
    }

    pub fn close_with_deadline(&self, _deadline_ns: Option<u64>) {
        if !self.state.detached.load(Ordering::Acquire) {
            close_stack_socket(self.socket);
        }
        self.state.detach();
        PROXY_STATES.write().retain(|entry| {
            entry.upgrade().is_some_and(|state| {
                !(state.socket == self.socket
                    && state.stack_instance == self.stack_instance
                    && state.proxy_generation == self.proxy_generation)
            })
        });
    }

    fn get_option(
        &self,
        option: NetStackSocketOptionV1,
    ) -> Result<NetStackSocketOptionValueV1, SocketError> {
        self.ensure_backend()?;
        match submit_stack_command(NetStackSocketCommandV1::GetOption {
            socket: self.socket,
            option,
            output: None,
        })? {
            NetStackSocketCommandV1::GetOption {
                output: Some(Ok(value)),
                ..
            } => Ok(value),
            NetStackSocketCommandV1::GetOption {
                output: Some(Err(error)),
                ..
            } => Err(map_stack_error(error)),
            _ => Err(SocketError::RuntimeBusy),
        }
    }

    pub fn set_option(
        &self,
        option: NetStackSocketOptionV1,
        value: NetStackSocketOptionValueV1,
    ) -> Result<(), SocketError> {
        self.ensure_backend()?;
        match submit_stack_command(NetStackSocketCommandV1::SetOption {
            socket: self.socket,
            option,
            value,
            output: None,
        })? {
            NetStackSocketCommandV1::SetOption {
                output: Some(Ok(())),
                ..
            } => Ok(()),
            NetStackSocketCommandV1::SetOption {
                output: Some(Err(error)),
                ..
            } => Err(map_stack_error(error)),
            _ => Err(SocketError::RuntimeBusy),
        }
    }

    pub fn owner(&self) -> OwnerRef {
        self.query()
            .map(|snapshot| snapshot.owner)
            .unwrap_or(OwnerRef::Closed { generation: 0 })
    }

    pub fn local_endpoint(&self) -> Option<Endpoint> {
        self.query().ok().and_then(|snapshot| snapshot.local)
    }

    pub fn peer_endpoint(&self) -> Option<Endpoint> {
        self.query().ok().and_then(|snapshot| snapshot.peer)
    }

    pub fn readiness(&self) -> (Readiness, u64) {
        if self.ensure_backend().is_ok() {
            let _ = self.query();
        }
        self.state.readiness()
    }

    pub fn set_observer(&self, observer: Weak<dyn ReadinessObserver>) {
        *self.state.observer.write() = Some(observer);
        let (readiness, generation) = self.state.readiness();
        if let Some(observer) = self.state.observer.read().as_ref().and_then(Weak::upgrade) {
            observer.readiness_changed(readiness, generation);
        }
    }

    pub fn add_poll_waiter(&self, task: &Arc<Task>, read: bool, write: bool, state: bool) -> bool {
        if read {
            self.state.read_wait.enqueue(task);
        }
        if write {
            self.state.write_wait.enqueue(task);
        }
        if state {
            self.state.state_wait.enqueue(task);
        }
        read || write || state
    }

    pub fn remove_poll_waiter(&self, task: &Arc<Task>) {
        self.state.read_wait.remove(task);
        self.state.write_wait.remove(task);
        self.state.state_wait.remove(task);
    }

    fn wait_io(
        &self,
        queue: &WaitQueue,
        readiness: Readiness,
        deadline_ns: Option<u64>,
    ) -> Result<(), SocketError> {
        self.ensure_backend()?;
        let task = sched::current_task();
        let (_, observed_generation) = self.state.readiness();
        let entry = queue.prepare_to_wait(&task, TaskState::Sleeping);
        if let Err(error) = self.ensure_backend() {
            queue.finish_wait(&entry);
            return Err(error);
        }
        let (current, generation) = self.readiness();
        if current.0 & readiness.0 != 0 || generation != observed_generation {
            queue.finish_wait(&entry);
            return Ok(());
        }
        if sched::operation::has_interrupting_signal(&task) {
            queue.finish_wait(&entry);
            return Err(SocketError::Interrupted);
        }
        if deadline_ns.is_some_and(|deadline| sched::now_ns_public() >= deadline) {
            queue.finish_wait(&entry);
            return Err(SocketError::TimedOut);
        }
        let armed =
            deadline_ns.is_some_and(|deadline| sched::register_sleep_deadline(&task, deadline));
        drop(task);
        sched::schedule_once(sched::now_ns_public());
        let task = sched::current_task();
        queue.finish_wait(&entry);
        if armed {
            sched::cancel_sleep_deadline(&task);
        }
        self.ensure_backend()?;
        if sched::operation::has_interrupting_signal(&task) {
            return Err(SocketError::Interrupted);
        }
        if deadline_ns.is_some_and(|deadline| sched::now_ns_public() >= deadline) {
            return Err(SocketError::TimedOut);
        }
        Ok(())
    }

    pub fn take_pending_error(&self) -> Option<SocketError> {
        let result = submit_stack_command(NetStackSocketCommandV1::TakeError {
            socket: self.socket,
            output: None,
        });
        match result {
            Ok(NetStackSocketCommandV1::TakeError {
                output: Some(Ok(error)),
                ..
            }) => error.or_else(|| self.backend_error()),
            _ => self.backend_error(),
        }
    }

    pub fn take_error_record(&self) -> Option<SocketErrorRecord> {
        match submit_stack_command(NetStackSocketCommandV1::TakeErrorRecord {
            socket: self.socket,
            output: None,
        }) {
            Ok(NetStackSocketCommandV1::TakeErrorRecord {
                output: Some(Ok(record)),
                ..
            }) => record,
            _ => None,
        }
    }

    pub fn set_tcp_more(&self, enabled: bool) {
        let _ = self.set_option(
            NetStackSocketOptionV1::TcpMore,
            NetStackSocketOptionValueV1::Bool(enabled),
        );
    }

    pub fn request_abortive_close(&self) {
        let _ = self.set_option(
            NetStackSocketOptionV1::AbortiveClose,
            NetStackSocketOptionValueV1::Bool(true),
        );
    }

    pub fn has_multicast_memberships(&self) -> bool {
        self.multicast(NetStackSocketMulticastActionV1::Query, None)
            .unwrap_or(false)
    }

    pub fn add_multicast_membership(
        &self,
        membership: MulticastMembership,
    ) -> Result<(), SocketError> {
        self.multicast(NetStackSocketMulticastActionV1::Add, Some(membership))
            .map(|_| ())
    }

    pub fn drop_multicast_membership(
        &self,
        membership: MulticastMembership,
    ) -> Result<(), SocketError> {
        self.multicast(NetStackSocketMulticastActionV1::Drop, Some(membership))
            .map(|_| ())
    }

    fn multicast(
        &self,
        action: NetStackSocketMulticastActionV1,
        membership: Option<MulticastMembership>,
    ) -> Result<bool, SocketError> {
        match submit_stack_command(NetStackSocketCommandV1::Multicast {
            socket: self.socket,
            action,
            membership,
            output: None,
        })? {
            NetStackSocketCommandV1::Multicast {
                output: Some(Ok(value)),
                ..
            } => Ok(value),
            NetStackSocketCommandV1::Multicast {
                output: Some(Err(error)),
                ..
            } => Err(map_stack_error(error)),
            _ => Err(SocketError::RuntimeBusy),
        }
    }

    pub fn buffer_limits(&self) -> (usize, usize) {
        let send = option_u32(self.get_option(NetStackSocketOptionV1::SendBuffer)).unwrap_or(0);
        let receive =
            option_u32(self.get_option(NetStackSocketOptionV1::ReceiveBuffer)).unwrap_or(0);
        (send as usize, receive as usize)
    }

    pub fn set_buffer_limits(&self, send: Option<usize>, receive: Option<usize>) {
        if let Some(send) = send {
            let _ = self.set_option(
                NetStackSocketOptionV1::SendBuffer,
                NetStackSocketOptionValueV1::U32(send.min(u32::MAX as usize) as u32),
            );
        }
        if let Some(receive) = receive {
            let _ = self.set_option(
                NetStackSocketOptionV1::ReceiveBuffer,
                NetStackSocketOptionValueV1::U32(receive.min(u32::MAX as usize) as u32),
            );
        }
    }

    pub fn tcp_info(&self) -> TcpInfoSnapshot {
        match submit_stack_command(NetStackSocketCommandV1::TcpInfo {
            socket: self.socket,
            output: None,
        }) {
            Ok(NetStackSocketCommandV1::TcpInfo {
                output: Some(Ok(info)),
                ..
            }) => info,
            _ => TcpInfoSnapshot::default(),
        }
    }

    pub fn take_rx_overflow(&self) -> u32 {
        match submit_stack_command(NetStackSocketCommandV1::TakeRxOverflow {
            socket: self.socket,
            output: None,
        }) {
            Ok(NetStackSocketCommandV1::TakeRxOverflow {
                output: Some(Ok(value)),
                ..
            }) => value,
            _ => 0,
        }
    }
}

macro_rules! proxy_bool_value {
    ($name:ident, $option:ident) => {
        pub fn $name(&self) -> bool {
            matches!(
                self.get_option(NetStackSocketOptionV1::$option),
                Ok(NetStackSocketOptionValueV1::Bool(true))
            )
        }
    };
}

macro_rules! proxy_u32_value {
    ($name:ident, $ty:ty, $option:ident) => {
        pub fn $name(&self) -> $ty {
            option_u32(self.get_option(NetStackSocketOptionV1::$option)).unwrap_or(0) as $ty
        }
    };
}

macro_rules! proxy_u64_value {
    ($name:ident, $option:ident) => {
        pub fn $name(&self) -> u64 {
            match self.get_option(NetStackSocketOptionV1::$option) {
                Ok(NetStackSocketOptionValueV1::U64(value)) => value,
                _ => 0,
            }
        }
    };
}

macro_rules! proxy_setter {
    ($name:ident, $ty:ty, $option:ident, $value:expr) => {
        pub fn $name(&self, value: $ty) {
            let _ = self.set_option(NetStackSocketOptionV1::$option, $value(value));
        }
    };
}

impl NetSocketProxy {
    proxy_bool_value!(tcp_nodelay, TcpNoDelay);
    proxy_bool_value!(tcp_cork, TcpCork);
    proxy_bool_value!(tcp_keepalive_enabled, TcpKeepAlive);
    proxy_u64_value!(tcp_keepidle_ns, TcpKeepIdleNs);
    proxy_u64_value!(tcp_keepintvl_ns, TcpKeepIntervalNs);
    proxy_u32_value!(tcp_keepcount, u8, TcpKeepCount);
    proxy_u32_value!(tcp_maxseg, u16, TcpMaxSegment);
    proxy_u64_value!(tcp_user_timeout_ns, TcpUserTimeoutNs);
    proxy_u64_value!(tcp_defer_accept_ns, TcpDeferAcceptNs);
    proxy_u32_value!(tcp_notsent_lowat, u32, TcpNotSentLowat);

    proxy_setter!(
        set_socket_mark,
        u32,
        SocketMark,
        NetStackSocketOptionValueV1::U32
    );
    proxy_setter!(
        set_socket_priority,
        i32,
        SocketPriority,
        NetStackSocketOptionValueV1::I32
    );
    proxy_setter!(set_ip_hop_limit, u8, IpHopLimit, |value| {
        NetStackSocketOptionValueV1::U32(u32::from(value))
    });
    proxy_setter!(set_ip_traffic_class, u8, IpTrafficClass, |value| {
        NetStackSocketOptionValueV1::U32(u32::from(value))
    });
    proxy_setter!(
        set_raw_header_included,
        bool,
        RawHeaderIncluded,
        NetStackSocketOptionValueV1::Bool
    );
    proxy_setter!(
        set_free_bind,
        bool,
        FreeBind,
        NetStackSocketOptionValueV1::Bool
    );
    proxy_setter!(
        set_multicast_interface,
        Option<InterfaceId>,
        MulticastInterface,
        NetStackSocketOptionValueV1::Interface
    );
    proxy_setter!(set_multicast_hops, u8, MulticastHops, |value| {
        NetStackSocketOptionValueV1::U32(u32::from(value))
    });
    proxy_setter!(
        set_multicast_loop,
        bool,
        MulticastLoop,
        NetStackSocketOptionValueV1::Bool
    );
    proxy_setter!(set_v6_only, bool, V6Only, NetStackSocketOptionValueV1::Bool);
    proxy_setter!(
        set_tcp_nodelay,
        bool,
        TcpNoDelay,
        NetStackSocketOptionValueV1::Bool
    );
    proxy_setter!(
        set_tcp_cork,
        bool,
        TcpCork,
        NetStackSocketOptionValueV1::Bool
    );
    proxy_setter!(
        set_tcp_keepalive,
        bool,
        TcpKeepAlive,
        NetStackSocketOptionValueV1::Bool
    );
    proxy_setter!(
        set_tcp_keepidle_ns,
        u64,
        TcpKeepIdleNs,
        NetStackSocketOptionValueV1::U64
    );
    proxy_setter!(
        set_tcp_keepintvl_ns,
        u64,
        TcpKeepIntervalNs,
        NetStackSocketOptionValueV1::U64
    );
    proxy_setter!(set_tcp_keepcount, u16, TcpKeepCount, |value| {
        NetStackSocketOptionValueV1::U32(u32::from(value))
    });
    proxy_setter!(set_tcp_maxseg, u16, TcpMaxSegment, |value| {
        NetStackSocketOptionValueV1::U32(u32::from(value))
    });
    proxy_setter!(
        set_tcp_user_timeout_ns,
        u64,
        TcpUserTimeoutNs,
        NetStackSocketOptionValueV1::U64
    );
    proxy_setter!(
        set_tcp_defer_accept_ns,
        u64,
        TcpDeferAcceptNs,
        NetStackSocketOptionValueV1::U64
    );
    proxy_setter!(
        set_tcp_notsent_lowat,
        u32,
        TcpNotSentLowat,
        NetStackSocketOptionValueV1::U32
    );

    pub fn request_quick_ack(&self) {
        let _ = self.set_option(
            NetStackSocketOptionV1::TcpQuickAck,
            NetStackSocketOptionValueV1::Bool(true),
        );
    }
}

fn option_u32(value: Result<NetStackSocketOptionValueV1, SocketError>) -> Option<u32> {
    match value {
        Ok(NetStackSocketOptionValueV1::U32(value)) => Some(value),
        _ => None,
    }
}

fn submit_stack_command(
    command: NetStackSocketCommandV1,
) -> Result<NetStackSocketCommandV1, SocketError> {
    socket_runtime()?
        .submit_stack_socket(command)
        .map_err(|_| SocketError::NetworkDown)
}

fn close_stack_socket(socket: NetStackSocketRefV1) {
    let _ = submit_stack_command(NetStackSocketCommandV1::Close {
        socket,
        output: None,
    });
}

const fn family_raw(family: AddressFamily) -> u8 {
    match family {
        AddressFamily::Ipv4 => NET_STACK_SOCKET_FAMILY_IPV4,
        AddressFamily::Ipv6 => NET_STACK_SOCKET_FAMILY_IPV6,
    }
}

const fn kind_raw(kind: SocketKind) -> u8 {
    match kind {
        SocketKind::Datagram => NET_STACK_SOCKET_KIND_DATAGRAM,
        SocketKind::Stream => NET_STACK_SOCKET_KIND_STREAM,
        SocketKind::Raw => NET_STACK_SOCKET_KIND_RAW,
    }
}

const fn map_stack_error(error: NetStackSocketErrorV1) -> SocketError {
    match error {
        NetStackSocketErrorV1::InvalidArgument | NetStackSocketErrorV1::NotSupported => {
            SocketError::InvalidState
        }
        NetStackSocketErrorV1::NotFound => SocketError::Closed,
        NetStackSocketErrorV1::StaleGeneration | NetStackSocketErrorV1::Quiesced => {
            SocketError::NetworkDown
        }
        NetStackSocketErrorV1::InvalidState => SocketError::InvalidState,
        NetStackSocketErrorV1::AddressInUse => SocketError::AddressInUse,
        NetStackSocketErrorV1::AddressUnavailable => SocketError::AddressUnavailable,
        NetStackSocketErrorV1::NotConnected => SocketError::NotConnected,
        NetStackSocketErrorV1::DestinationRequired => SocketError::DestinationRequired,
        NetStackSocketErrorV1::AlreadyConnected => SocketError::AlreadyConnected,
        NetStackSocketErrorV1::InProgress => SocketError::InProgress,
        NetStackSocketErrorV1::AlreadyInProgress => SocketError::AlreadyInProgress,
        NetStackSocketErrorV1::WouldBlock => SocketError::WouldBlock,
        NetStackSocketErrorV1::MessageTooLarge => SocketError::MessageTooLarge,
        NetStackSocketErrorV1::BufferFull => SocketError::Buffer,
        NetStackSocketErrorV1::ReadShutdown => SocketError::ReadShutdown,
        NetStackSocketErrorV1::WriteShutdown => SocketError::WriteShutdown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> NetStackSocketDescriptorV1 {
        NetStackSocketDescriptorV1 {
            socket: NetStackSocketRefV1 {
                id: crate::SocketId {
                    boot_nonce: 1,
                    counter: 2,
                },
                generation: 3,
            },
            family: NET_STACK_SOCKET_FAMILY_IPV4,
            kind: NET_STACK_SOCKET_KIND_DATAGRAM,
            protocol: 17,
            state: crate::stack::NetStackSocketStateV1::Unbound,
            readiness: Readiness::WRITABLE.raw(),
            readiness_generation: 4,
        }
    }

    #[test]
    fn stack_errors_map_to_stable_proxy_errors() {
        assert_eq!(
            map_stack_error(NetStackSocketErrorV1::StaleGeneration),
            SocketError::NetworkDown
        );
        assert_eq!(
            map_stack_error(NetStackSocketErrorV1::WouldBlock),
            SocketError::WouldBlock
        );
        assert_eq!(
            map_stack_error(NetStackSocketErrorV1::BufferFull),
            SocketError::Buffer
        );
    }

    #[test]
    fn proxy_state_rejects_stale_readiness_and_detach_is_terminal() {
        let state = ProxyState::new(descriptor(), 7, 9, 8);
        state.publish(Readiness::READABLE, 3);
        assert_eq!(state.readiness(), (Readiness::WRITABLE, 4));

        state.publish(Readiness::READABLE, 5);
        assert_eq!(state.readiness(), (Readiness::READABLE, 5));

        state.detach();
        assert!(state.readiness().0.contains(Readiness::ERROR));
        state.publish(Readiness::WRITABLE, 99);
        assert!(state.readiness().0.contains(Readiness::ERROR));
    }
}
