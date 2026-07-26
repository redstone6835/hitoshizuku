//! 常驻 VFS socket 代理与 ELM readiness 路由。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use sched::{Task, WaitQueue};
use spin::RwLock;

use super::{
    MulticastMembership, OwnerRef, Readiness, ReadinessObserver, SocketError, SocketErrorRecord,
    SocketFacade, SocketKind, TcpInfoSnapshot, UdpReceive, new_raw_socket_facade,
    new_socket_facade, new_tcp_socket_facade, track_socket_facade,
};
use crate::control::BindOptions;
use crate::{AddressFamily, Endpoint, InterfaceId, SocketId};

static NEXT_PROXY_GENERATION: AtomicU64 = AtomicU64::new(1);
static PROXY_STATES: RwLock<Vec<Weak<ProxyState>>> = RwLock::new(Vec::new());

struct ProxyState {
    socket: SocketId,
    stack_instance: u64,
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
        socket: SocketId,
        readiness: Readiness,
        readiness_generation: u64,
        stack_instance: u64,
    ) -> Self {
        Self {
            socket,
            stack_instance,
            readiness: AtomicU16::new(readiness.raw()),
            readiness_generation: AtomicU64::new(readiness_generation),
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

impl ReadinessObserver for ProxyState {
    fn readiness_changed(&self, readiness: Readiness, generation: u64) {
        self.publish(readiness, generation);
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
    stack_generation: u64,
    stack_instance: u64,
    proxy_generation: u64,
    facade: Arc<SocketFacade>,
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
        let facade = match (kind, protocol) {
            (SocketKind::Datagram, 0 | 17) => new_socket_facade(family),
            (SocketKind::Stream, 0 | 6) => new_tcp_socket_facade(family),
            (SocketKind::Raw, 1..=u8::MAX) => new_raw_socket_facade(family, protocol),
            _ => return Err(SocketError::InvalidState),
        }?;
        track_socket_facade(&facade, stack_generation);
        if facade.stack_generation() != stack_generation {
            return Err(SocketError::NetworkDown);
        }
        Self::register(facade, family, kind, stack_generation, stack_instance)
    }

    fn register(
        facade: Arc<SocketFacade>,
        family: AddressFamily,
        kind: SocketKind,
        stack_generation: u64,
        stack_instance: u64,
    ) -> Result<Self, SocketError> {
        let proxy_generation = NEXT_PROXY_GENERATION.fetch_add(1, Ordering::Relaxed);
        if proxy_generation == 0 {
            facade.close();
            return Err(SocketError::Buffer);
        }
        if facade.family() != family || facade.kind() != kind || facade.protocol() == 0 {
            facade.close();
            return Err(SocketError::RuntimeBusy);
        }
        track_socket_facade(&facade, stack_generation);
        let socket = facade.id();
        let (readiness, readiness_generation) = facade.readiness();
        let state = Arc::new(ProxyState::new(
            socket,
            readiness,
            readiness_generation,
            stack_instance,
        ));
        let mut registry = PROXY_STATES.write();
        registry.retain(|entry| entry.strong_count() != 0);
        if registry
            .iter()
            .filter_map(Weak::upgrade)
            .any(|entry| entry.socket == socket && entry.stack_instance == stack_instance)
        {
            drop(registry);
            facade.close();
            return Err(SocketError::RuntimeBusy);
        }
        registry.push(Arc::downgrade(&state));
        drop(registry);
        let readiness_observer: Arc<dyn ReadinessObserver> = state.clone();
        facade.set_observer(Arc::downgrade(&readiness_observer));
        Ok(Self {
            family,
            kind,
            protocol: facade.protocol(),
            stack_generation,
            stack_instance,
            proxy_generation,
            facade,
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
        if let Some(error) = self.facade.backend_error() {
            self.state.detach();
            return Err(error);
        }
        let snapshot = crate::stack::stack_snapshot();
        if snapshot.state != crate::stack::NetStackState::Active
            || !snapshot.ready
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

    pub fn bind(
        &self,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<(), SocketError> {
        self.ensure_backend()?;
        self.facade.bind(local, interface, options)
    }

    pub fn connect_with_mode(
        &self,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        nonblocking: bool,
    ) -> Result<(), SocketError> {
        self.ensure_backend()?;
        self.facade
            .connect_with_mode(peer, interface, options, nonblocking)
    }

    pub fn listen(&self, backlog: u32) -> Result<(), SocketError> {
        self.ensure_backend()?;
        self.facade.listen(backlog)
    }

    pub fn accept(&self, nonblocking: bool, deadline_ns: Option<u64>) -> Result<Self, SocketError> {
        loop {
            self.ensure_backend()?;
            let facade = self.facade.accept(nonblocking, deadline_ns)?;
            return Self::register(
                facade,
                self.family,
                self.kind,
                self.stack_generation,
                self.stack_instance,
            );
        }
    }

    pub fn shutdown(&self, read: bool, write: bool) -> Result<(), SocketError> {
        self.ensure_backend()?;
        self.facade.shutdown(read, write)
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
        self.ensure_backend()?;
        self.facade.send_datagram(
            payload,
            destination,
            nonblocking,
            deadline_ns,
            dont_route,
            confirm,
        )
    }

    pub fn send_datagram_from<E>(
        &self,
        payload_len: usize,
        destination: Option<Endpoint>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        dont_route: bool,
        confirm: bool,
        copy: impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<usize, super::DatagramCopyError<E>> {
        self.ensure_backend()
            .map_err(super::DatagramCopyError::Socket)?;
        self.facade.send_datagram_from(
            payload_len,
            destination,
            nonblocking,
            deadline_ns,
            dont_route,
            confirm,
            copy,
        )
    }

    pub fn send_stream(
        &self,
        payload: &[u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
        more: bool,
    ) -> Result<usize, SocketError> {
        self.ensure_backend()?;
        self.facade
            .send_stream(payload, nonblocking, deadline_ns, more)
    }

    pub fn recv(
        &self,
        output: &mut [u8],
        peek: bool,
        truncate: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<UdpReceive, SocketError> {
        self.ensure_backend()?;
        self.facade
            .recv(output, peek, truncate, nonblocking, deadline_ns)
    }

    pub fn wait_datagram_readable(
        &self,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<Option<usize>, SocketError> {
        self.ensure_backend()?;
        self.facade.wait_datagram_readable(nonblocking, deadline_ns)
    }

    pub fn recv_local_datagram_from<E>(
        &self,
        output_len: usize,
        copy_capacity: usize,
        report_original_len: bool,
        copy: impl FnMut(usize, &[u8]) -> Result<(), E>,
    ) -> Result<Option<super::UdpReceive>, super::DatagramCopyError<E>> {
        self.ensure_backend()
            .map_err(super::DatagramCopyError::Socket)?;
        self.facade
            .recv_local_datagram_from(output_len, copy_capacity, report_original_len, copy)
    }

    pub fn recv_stream(
        &self,
        output: &mut [u8],
        peek: bool,
        wait_all: bool,
        defer_window_update: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
        self.ensure_backend()?;
        self.facade.recv_stream(
            output,
            peek,
            wait_all,
            defer_window_update,
            nonblocking,
            deadline_ns,
        )
    }

    pub fn finish_stream_receive(&self) {
        self.facade.finish_stream_receive();
    }

    pub fn close(&self) {
        self.close_with_deadline(None);
    }

    pub fn close_with_deadline(&self, _deadline_ns: Option<u64>) {
        if !self.state.detached.load(Ordering::Acquire) {
            self.facade.close();
        }
        self.state.detach();
    }

    pub fn owner(&self) -> OwnerRef {
        self.facade.owner()
    }

    pub fn local_endpoint(&self) -> Option<Endpoint> {
        self.facade.local_endpoint()
    }

    pub fn peer_endpoint(&self) -> Option<Endpoint> {
        self.facade.peer_endpoint()
    }

    pub fn readiness(&self) -> (Readiness, u64) {
        if self.ensure_backend().is_ok() {
            let (readiness, generation) = self.facade.readiness();
            self.state.publish(readiness, generation);
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

    pub fn take_pending_error(&self) -> Option<SocketError> {
        self.facade
            .take_pending_error()
            .or_else(|| self.backend_error())
    }

    pub fn take_error_record(&self) -> Option<SocketErrorRecord> {
        self.facade.take_error_record()
    }

    pub fn set_tcp_more(&self, enabled: bool) {
        self.facade.set_tcp_more(enabled);
    }

    pub fn request_abortive_close(&self) {
        self.facade.request_abortive_close();
    }

    pub fn has_multicast_memberships(&self) -> bool {
        self.facade.has_multicast_memberships()
    }

    pub fn add_multicast_membership(
        &self,
        membership: MulticastMembership,
    ) -> Result<(), SocketError> {
        self.ensure_backend()?;
        self.facade.add_multicast_membership(membership)
    }

    pub fn drop_multicast_membership(
        &self,
        membership: MulticastMembership,
    ) -> Result<(), SocketError> {
        self.ensure_backend()?;
        self.facade.drop_multicast_membership(membership)
    }

    pub fn buffer_limits(&self) -> (usize, usize) {
        self.facade.buffer_limits()
    }

    pub fn set_buffer_limits(&self, send: Option<usize>, receive: Option<usize>) {
        self.facade.set_buffer_limits(send, receive);
    }

    pub fn tcp_info(&self) -> TcpInfoSnapshot {
        self.facade.tcp_info()
    }

    pub fn take_rx_overflow(&self) -> u32 {
        self.facade.take_rx_overflow()
    }
}

impl NetSocketProxy {
    pub fn tcp_nodelay(&self) -> bool {
        self.facade.tcp_nodelay()
    }

    pub fn tcp_cork(&self) -> bool {
        self.facade.tcp_cork()
    }

    pub fn tcp_keepalive_enabled(&self) -> bool {
        self.facade.tcp_keepalive_enabled()
    }

    pub fn tcp_keepidle_ns(&self) -> u64 {
        self.facade.tcp_keepidle_ns()
    }

    pub fn tcp_keepintvl_ns(&self) -> u64 {
        self.facade.tcp_keepintvl_ns()
    }

    pub fn tcp_keepcount(&self) -> u8 {
        self.facade.tcp_keepcount()
    }

    pub fn tcp_maxseg(&self) -> u16 {
        self.facade.tcp_maxseg()
    }

    pub fn tcp_user_timeout_ns(&self) -> u64 {
        self.facade.tcp_user_timeout_ns()
    }

    pub fn tcp_defer_accept_ns(&self) -> u64 {
        self.facade.tcp_defer_accept_ns()
    }

    pub fn tcp_notsent_lowat(&self) -> u32 {
        self.facade.tcp_notsent_lowat()
    }

    pub fn set_socket_mark(&self, value: u32) {
        self.facade.set_socket_mark(value);
    }

    pub fn set_socket_priority(&self, value: i32) {
        self.facade.set_socket_priority(value);
    }

    pub fn set_ip_hop_limit(&self, value: u8) {
        self.facade.set_ip_hop_limit(value);
    }

    pub fn set_ip_traffic_class(&self, value: u8) {
        self.facade.set_ip_traffic_class(value);
    }

    pub fn set_raw_header_included(&self, enabled: bool) {
        self.facade.set_raw_header_included(enabled);
    }

    pub fn set_free_bind(&self, enabled: bool) {
        self.facade.set_free_bind(enabled);
    }

    pub fn set_multicast_interface(&self, interface: Option<InterfaceId>) {
        self.facade.set_multicast_interface(interface);
    }

    pub fn set_multicast_hops(&self, value: u8) {
        self.facade.set_multicast_hops(value);
    }

    pub fn set_multicast_loop(&self, enabled: bool) {
        self.facade.set_multicast_loop(enabled);
    }

    pub fn set_v6_only(&self, enabled: bool) {
        self.facade.set_v6_only(enabled);
    }

    pub fn set_tcp_nodelay(&self, enabled: bool) {
        self.facade.set_tcp_nodelay(enabled);
    }

    pub fn set_tcp_cork(&self, enabled: bool) {
        self.facade.set_tcp_cork(enabled);
    }

    pub fn set_tcp_keepalive(&self, enabled: bool) {
        self.facade.set_tcp_keepalive(enabled);
    }

    pub fn set_tcp_keepidle_ns(&self, value: u64) {
        self.facade.set_tcp_keepidle_ns(value);
    }

    pub fn set_tcp_keepintvl_ns(&self, value: u64) {
        self.facade.set_tcp_keepintvl_ns(value);
    }

    pub fn set_tcp_keepcount(&self, value: u16) {
        self.facade.set_tcp_keepcount(value);
    }

    pub fn set_tcp_maxseg(&self, value: u16) {
        self.facade.set_tcp_maxseg(value);
    }

    pub fn set_tcp_user_timeout_ns(&self, value: u64) {
        self.facade.set_tcp_user_timeout_ns(value);
    }

    pub fn set_tcp_defer_accept_ns(&self, value: u64) {
        self.facade.set_tcp_defer_accept_ns(value);
    }

    pub fn set_tcp_notsent_lowat(&self, value: u32) {
        self.facade.set_tcp_notsent_lowat(value);
    }

    pub fn request_quick_ack(&self) {
        self.facade.request_quick_ack();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_id() -> SocketId {
        SocketId {
            boot_nonce: 1,
            counter: 2,
        }
    }

    #[test]
    fn proxy_state_rejects_stale_readiness_and_detach_is_terminal() {
        let state = ProxyState::new(socket_id(), Readiness::WRITABLE, 4, 9);
        state.publish(Readiness::READABLE, 3);
        assert_eq!(state.readiness(), (Readiness::WRITABLE, 4));

        state.publish(Readiness::READABLE, 5);
        assert_eq!(state.readiness(), (Readiness::READABLE, 5));

        state.detach();
        assert!(state.readiness().0.contains(Readiness::ERROR));
        state.publish(Readiness::WRITABLE, 99);
        assert!(state.readiness().0.contains(Readiness::ERROR));
    }

    #[test]
    fn facade_readiness_callback_updates_only_its_proxy_state() {
        let state = Arc::new(ProxyState::new(socket_id(), Readiness::WRITABLE, 4, 9));
        let observer: Arc<dyn ReadinessObserver> = state.clone();

        observer.readiness_changed(Readiness::READABLE, 5);

        assert_eq!(state.readiness(), (Readiness::READABLE, 5));
    }
}
