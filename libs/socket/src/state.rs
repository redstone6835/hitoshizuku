//! 套接字核心状态与全局注册表。
//!
//! 本模块定义了 [`Socket`] 的完整生命周期:创建、绑定、连接、收发、关闭。
//! 所有活跃套接字通过全局 `REGISTRY`(按绑定名称索引)和 `SOCKETS`(按 ID 索引)追踪。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sched::Task;
use sched::sync::Spinlock;

use crate::connection::{self, ConnectionStateOps};
use crate::io;
use crate::types::{
    BindingKey, HandleIdentity, PathKey, PeerIdentity, Readiness, ReceiveOptions, ReceiveResult,
    SendOptions, SharedHandle, SocketError, SocketLinger, SocketShutdown, SocketTimeval,
    SocketType, UnixAddress,
};
use crate::wait::wake_task;

/// Stream 单次发送缓冲区总容量(64 KiB)
pub(crate) const STREAM_BUFFER_LIMIT: usize = 64 * 1024;
/// Stream 队列最大 chunk 数
pub(crate) const STREAM_CHUNK_LIMIT: usize = 1024;
/// 消息报文单条最大字节数(256 KiB)
pub(crate) const MESSAGE_BUFFER_LIMIT: usize = 256 * 1024;
/// 消息队列最大报文数
pub(crate) const MESSAGE_PACKET_LIMIT: usize = 1024;

pub(crate) const DEFAULT_STREAM_BUFFER_SIZE: usize = STREAM_BUFFER_LIMIT;
pub(crate) const DEFAULT_MESSAGE_BUFFER_SIZE: usize = MESSAGE_BUFFER_LIMIT;

/// 套接字选项(对应 getsockopt/setsockopt)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SocketOptions {
    pub(crate) send_buffer_size: usize,
    pub(crate) recv_buffer_size: usize,
    pub(crate) reuse_addr: bool,
    pub(crate) reuse_port: bool,
    pub(crate) linger: SocketLinger,
    pub(crate) send_timeout: Option<SocketTimeval>,
    pub(crate) recv_timeout: Option<SocketTimeval>,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            send_buffer_size: DEFAULT_STREAM_BUFFER_SIZE,
            recv_buffer_size: DEFAULT_STREAM_BUFFER_SIZE,
            reuse_addr: false,
            reuse_port: false,
            linger: SocketLinger {
                enabled: false,
                seconds: 0,
            },
            send_timeout: None,
            recv_timeout: None,
        }
    }
}

/// Unix 域套接字核心句柄。
/// 持有内部状态的引用计数指针,Clone 即共享同一套接字。
#[derive(Clone)]
pub struct Socket {
    pub(crate) inner: Arc<SocketInner>,
}

#[kernel_symbols::export]
impl Socket {
    #[kernel_symbols::export(
        name = "socket.Socket.new_unix",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn new_unix(kind: SocketType, owner: PeerIdentity) -> Result<Self, SocketError> {
        let options = default_socket_options(kind);
        let kind_impl = match kind {
            SocketType::Stream => SocketKind::Stream(StreamSocket {
                state: Spinlock::new(StreamState::Init),
                accept_wait: sched::WaitQueue::new(),
                connect_wait: sched::WaitQueue::new(),
            }),
            SocketType::Datagram => SocketKind::Datagram(DatagramSocket {
                state: Spinlock::new(DatagramState {
                    queue: VecDeque::new(),
                    queued_bytes: 0,
                    queue_limit_bytes: DEFAULT_MESSAGE_BUFFER_SIZE,
                    connected: None,
                    peer_identity: None,
                    read_shutdown: false,
                    write_shutdown: false,
                }),
                read_wait: sched::WaitQueue::new(),
                write_wait: sched::WaitQueue::new(),
            }),
            SocketType::Sequenced => SocketKind::Sequenced(SequencedSocket {
                state: Spinlock::new(SequencedState::Init),
                accept_wait: sched::WaitQueue::new(),
                connect_wait: sched::WaitQueue::new(),
            }),
            SocketType::Raw => return Err(SocketError::InvalidInput),
        };
        let socket = Self {
            inner: Arc::new(SocketInner {
                id: NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed),
                kind,
                owner,
                passcred: AtomicBool::new(false),
                local_name: Spinlock::new(None),
                closed: Spinlock::new(false),
                last_error: Spinlock::new(None),
                options: Spinlock::new(options),
                kind_impl,
            }),
        };
        register_socket(&socket.inner);
        Ok(socket)
    }

    #[kernel_symbols::export(
        name = "socket.Socket.pair_unix",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn pair_unix(kind: SocketType, owner: PeerIdentity) -> Result<(Self, Self), SocketError> {
        match kind {
            SocketType::Stream => {
                let a = Self::new_unix(SocketType::Stream, owner)?;
                let b = Self::new_unix(SocketType::Stream, owner)?;
                connection::install_stream_pair(&a, &b)?;
                Ok((a, b))
            }
            SocketType::Datagram => {
                let a = Self::new_unix(SocketType::Datagram, owner)?;
                let b = Self::new_unix(SocketType::Datagram, owner)?;
                connection::install_dgram_pair(&a, &b)?;
                Ok((a, b))
            }
            SocketType::Sequenced => {
                let a = Self::new_unix(SocketType::Sequenced, owner)?;
                let b = Self::new_unix(SocketType::Sequenced, owner)?;
                connection::install_seqpacket_pair(&a, &b)?;
                Ok((a, b))
            }
            SocketType::Raw => Err(SocketError::InvalidInput),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.socket_type",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn socket_type(&self) -> SocketType {
        self.inner.kind
    }

    #[kernel_symbols::export(
        name = "socket.Socket.id",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    #[kernel_symbols::export(
        name = "socket.Socket.owner_identity",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn owner_identity(&self) -> PeerIdentity {
        self.inner.owner
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_passcred",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_passcred(&self, enabled: bool) {
        self.inner.passcred.store(enabled, Ordering::Release);
    }

    #[kernel_symbols::export(
        name = "socket.Socket.passcred_enabled",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn passcred_enabled(&self) -> bool {
        self.inner.passcred.load(Ordering::Acquire)
    }

    #[kernel_symbols::export(
        name = "socket.Socket.bind",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn bind(&self, address: UnixAddress) -> Result<(), SocketError> {
        let key = address.binding_key().ok_or(SocketError::InvalidInput)?;
        ensure_name_len(&address)?;
        {
            let current = self.inner.local_name.lock();
            if current.is_some() {
                return Err(SocketError::NameAlreadyBound);
            }
        }
        registry_insert(key, &self.inner)?;
        *self.inner.local_name.lock() = Some(address);
        Ok(())
    }

    #[kernel_symbols::export(
        name = "socket.Socket.local_address",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn local_address(&self) -> UnixAddress {
        self.inner
            .local_name
            .lock()
            .clone()
            .unwrap_or(UnixAddress::Unnamed)
    }

    #[kernel_symbols::export(
        name = "socket.Socket.peer_address",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn peer_address(&self) -> Result<UnixAddress, SocketError> {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => {
                let state = stream.state.lock();
                match &*state {
                    StreamState::Connected(conn) => {
                        Ok(conn.peer_name.clone().unwrap_or(UnixAddress::Unnamed))
                    }
                    _ => Err(SocketError::ConnectionMissing),
                }
            }
            SocketKind::Datagram(dgram) => {
                let state = dgram.state.lock();
                match &state.connected {
                    Some(peer) => Ok(peer.address()),
                    None => Err(SocketError::ConnectionMissing),
                }
            }
            SocketKind::Sequenced(seq) => {
                let state = seq.state.lock();
                match &*state {
                    SequencedState::Connected(conn) => {
                        Ok(conn.peer_name.clone().unwrap_or(UnixAddress::Unnamed))
                    }
                    _ => Err(SocketError::ConnectionMissing),
                }
            }
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.peer_identity",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn peer_identity(&self) -> Result<PeerIdentity, SocketError> {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => {
                let state = stream.state.lock();
                match &*state {
                    StreamState::Connected(conn) => Ok(conn.peer_identity),
                    _ => Err(SocketError::ConnectionMissing),
                }
            }
            SocketKind::Datagram(dgram) => {
                let state = dgram.state.lock();
                state.peer_identity.ok_or(SocketError::ConnectionMissing)
            }
            SocketKind::Sequenced(seq) => {
                let state = seq.state.lock();
                match &*state {
                    SequencedState::Connected(conn) => Ok(conn.peer_identity),
                    _ => Err(SocketError::ConnectionMissing),
                }
            }
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.listen",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => {
                connection::listen_connection_state(&stream.state, backlog)
            }
            SocketKind::Datagram(_) => Err(SocketError::Unsupported),
            SocketKind::Sequenced(seq) => connection::listen_connection_state(&seq.state, backlog),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.accept",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn accept(&self, options: ReceiveOptions) -> Result<Self, SocketError> {
        let options = ReceiveOptions {
            deadline_ns: effective_recv_deadline(
                &self.inner,
                options.deadline_ns,
                options.nonblocking,
            ),
            ..options
        };
        let result = match &self.inner.kind_impl {
            SocketKind::Stream(stream) => connection::accept_connection_socket(
                &stream.state,
                &stream.accept_wait,
                &stream.connect_wait,
                options,
            ),
            SocketKind::Datagram(_) => Err(SocketError::ListenerRequired),
            SocketKind::Sequenced(seq) => connection::accept_connection_socket(
                &seq.state,
                &seq.accept_wait,
                &seq.connect_wait,
                options,
            ),
        };
        self.finish_result(result)
    }

    #[kernel_symbols::export(
        name = "socket.Socket.connect",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn connect(
        &self,
        address: UnixAddress,
        caller: PeerIdentity,
        options: SendOptions,
    ) -> Result<(), SocketError> {
        let options = SendOptions {
            deadline_ns: effective_send_deadline(
                &self.inner,
                options.deadline_ns,
                options.nonblocking,
            ),
            ..options
        };
        let result = match &self.inner.kind_impl {
            SocketKind::Stream(stream) => {
                connection::connect_stream(self, stream, address, caller, options)
            }
            SocketKind::Datagram(dgram) => connection::connect_datagram(dgram, address, options),
            SocketKind::Sequenced(seq) => {
                connection::connect_seqpacket(self, seq, address, caller, options)
            }
        };
        self.finish_result(result)
    }

    #[kernel_symbols::export(
        name = "socket.Socket.validate_connect_ready",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn validate_connect_ready(&self) -> Result<(), SocketError> {
        if *self.inner.closed.lock() {
            return Err(SocketError::PeerClosed);
        }
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => {
                let state = stream.state.lock();
                match &*state {
                    StreamState::Init => Ok(()),
                    StreamState::Connected(_) => Err(SocketError::AlreadyConnected),
                    StreamState::Listening(_) | StreamState::Closed => {
                        Err(SocketError::StateMismatch)
                    }
                }
            }
            SocketKind::Datagram(dgram) => {
                let state = dgram.state.lock();
                if state.write_shutdown {
                    Err(SocketError::PeerClosed)
                } else {
                    Ok(())
                }
            }
            SocketKind::Sequenced(seq) => {
                let state = seq.state.lock();
                match &*state {
                    SequencedState::Init => Ok(()),
                    SequencedState::Connected(_) => Err(SocketError::AlreadyConnected),
                    SequencedState::Listening(_) | SequencedState::Closed => {
                        Err(SocketError::StateMismatch)
                    }
                }
            }
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.shutdown",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn shutdown(&self, how: SocketShutdown) -> Result<(), SocketError> {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => io::shutdown_stream(stream, how),
            SocketKind::Datagram(dgram) => io::shutdown_datagram(dgram, how),
            SocketKind::Sequenced(seq) => io::shutdown_seqpacket(seq, how),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.readiness",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn readiness(&self) -> Readiness {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => io::stream_readiness(stream),
            SocketKind::Datagram(dgram) => io::datagram_readiness(dgram),
            SocketKind::Sequenced(seq) => io::seqpacket_readiness(seq),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.register_waiter",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
        retained_args = 1 << 1
    )]
    pub fn register_waiter(&self, task: &Arc<Task>, interest: Readiness) -> bool {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => io::register_stream_waiter(stream, task, interest),
            SocketKind::Datagram(dgram) => io::register_datagram_waiter(dgram, task, interest),
            SocketKind::Sequenced(seq) => io::register_seqpacket_waiter(seq, task, interest),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.unregister_waiter",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn unregister_waiter(&self, task: &Arc<Task>) {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => io::unregister_stream_waiter(stream, task),
            SocketKind::Datagram(dgram) => io::unregister_datagram_waiter(dgram, task),
            SocketKind::Sequenced(seq) => io::unregister_seqpacket_waiter(seq, task),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.is_listener",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn is_listener(&self) -> bool {
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => matches!(*stream.state.lock(), StreamState::Listening(_)),
            SocketKind::Datagram(_) => false,
            SocketKind::Sequenced(seq) => matches!(*seq.state.lock(), SequencedState::Listening(_)),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.send",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
        retained_args = 1 << 2
    )]
    pub fn send(
        &self,
        data: &[u8],
        handles: &[SharedHandle],
        target: Option<UnixAddress>,
        options: SendOptions,
    ) -> Result<usize, SocketError> {
        let options = SendOptions {
            deadline_ns: effective_send_deadline(
                &self.inner,
                options.deadline_ns,
                options.nonblocking,
            ),
            ..options
        };
        let result = match &self.inner.kind_impl {
            SocketKind::Stream(stream) => {
                if target.is_some() {
                    Err(SocketError::InvalidInput)
                } else {
                    let sender = options.sender_identity.unwrap_or(self.inner.owner);
                    io::send_stream(stream, data, handles, sender, options)
                }
            }
            SocketKind::Datagram(dgram) => {
                let sender = options.sender_identity.unwrap_or(self.inner.owner);
                io::send_datagram(&self.inner, dgram, data, handles, sender, target, options)
            }
            SocketKind::Sequenced(seq) => {
                if target.is_some() {
                    Err(SocketError::InvalidInput)
                } else {
                    let sender = options.sender_identity.unwrap_or(self.inner.owner);
                    io::send_seqpacket(seq, data, handles, sender, options)
                }
            }
        };
        self.finish_result(result)
    }

    #[kernel_symbols::export(
        name = "socket.Socket.receive",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn receive(
        &self,
        buffer: &mut [u8],
        options: ReceiveOptions,
    ) -> Result<ReceiveResult, SocketError> {
        let options = ReceiveOptions {
            deadline_ns: effective_recv_deadline(
                &self.inner,
                options.deadline_ns,
                options.nonblocking,
            ),
            ..options
        };
        let passcred = self.passcred_enabled();
        let result = match &self.inner.kind_impl {
            SocketKind::Stream(stream) => io::recv_stream(stream, buffer, options, passcred),
            SocketKind::Datagram(dgram) => io::recv_datagram(dgram, buffer, options, passcred),
            SocketKind::Sequenced(seq) => io::recv_seqpacket(seq, buffer, options, passcred),
        };
        self.finish_result(result)
    }

    #[kernel_symbols::export(
        name = "socket.Socket.close",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn close(&self) {
        let mut closed = self.inner.closed.lock();
        if *closed {
            return;
        }
        *closed = true;
        drop(closed);
        if let Some(key) = self
            .inner
            .local_name
            .lock()
            .as_ref()
            .and_then(UnixAddress::binding_key)
        {
            registry_remove(&key, &self.inner);
        }
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => io::close_stream(stream),
            SocketKind::Datagram(dgram) => io::close_datagram(dgram),
            SocketKind::Sequenced(seq) => io::close_seqpacket(seq),
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_send_buffer_size",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_send_buffer_size(&self, size: usize) {
        self.inner.options.lock().send_buffer_size = size.max(1);
    }

    #[kernel_symbols::export(
        name = "socket.Socket.send_buffer_size",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn send_buffer_size(&self) -> usize {
        self.inner.options.lock().send_buffer_size
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_recv_buffer_size",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_recv_buffer_size(&self, size: usize) {
        let size = size.max(1);
        self.inner.options.lock().recv_buffer_size = size;
        match &self.inner.kind_impl {
            SocketKind::Stream(stream) => {
                let state = stream.state.lock();
                if let Some(conn) = state.connected_ref() {
                    conn.rx.state.lock().limit_bytes = size;
                    conn.rx.write_wait.wake_all_with(wake_task);
                }
            }
            SocketKind::Datagram(dgram) => {
                dgram.state.lock().queue_limit_bytes = size;
                dgram.write_wait.wake_all_with(wake_task);
            }
            SocketKind::Sequenced(seq) => {
                let state = seq.state.lock();
                if let Some(conn) = state.connected_ref() {
                    conn.rx.state.lock().limit_bytes = size;
                    conn.rx.write_wait.wake_all_with(wake_task);
                }
            }
        }
    }

    #[kernel_symbols::export(
        name = "socket.Socket.recv_buffer_size",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn recv_buffer_size(&self) -> usize {
        self.inner.options.lock().recv_buffer_size
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_reuse_addr",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_reuse_addr(&self, enabled: bool) {
        self.inner.options.lock().reuse_addr = enabled;
    }

    #[kernel_symbols::export(
        name = "socket.Socket.reuse_addr",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn reuse_addr(&self) -> bool {
        self.inner.options.lock().reuse_addr
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_reuse_port",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_reuse_port(&self, enabled: bool) {
        self.inner.options.lock().reuse_port = enabled;
    }

    #[kernel_symbols::export(
        name = "socket.Socket.reuse_port",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn reuse_port(&self) -> bool {
        self.inner.options.lock().reuse_port
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_linger",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_linger(&self, linger: SocketLinger) {
        self.inner.options.lock().linger = linger;
    }

    #[kernel_symbols::export(
        name = "socket.Socket.linger",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn linger(&self) -> SocketLinger {
        self.inner.options.lock().linger
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_send_timeout",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_send_timeout(&self, timeout: Option<SocketTimeval>) {
        self.inner.options.lock().send_timeout = timeout;
    }

    #[kernel_symbols::export(
        name = "socket.Socket.send_timeout",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn send_timeout(&self) -> Option<SocketTimeval> {
        self.inner.options.lock().send_timeout
    }

    #[kernel_symbols::export(
        name = "socket.Socket.set_recv_timeout",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_recv_timeout(&self, timeout: Option<SocketTimeval>) {
        self.inner.options.lock().recv_timeout = timeout;
    }

    #[kernel_symbols::export(
        name = "socket.Socket.recv_timeout",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn recv_timeout(&self) -> Option<SocketTimeval> {
        self.inner.options.lock().recv_timeout
    }

    #[kernel_symbols::export(
        name = "socket.Socket.take_last_error",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn take_last_error(&self) -> Option<SocketError> {
        self.inner.last_error.lock().take()
    }

    #[kernel_symbols::export(
        name = "socket.Socket.sock_at_mark",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn sock_at_mark(&self) -> bool {
        false
    }

    #[kernel_symbols::export(
        name = "socket.Socket.would_create_handle_cycle",
        contract = "kernel.ipc.unix-socket@1",
        version = 1,
        capabilities = kernel_symbols::capability::IPC
    )]
    pub fn would_create_handle_cycle(&self, identity: HandleIdentity) -> bool {
        match identity {
            HandleIdentity::Socket(id) => {
                id == self.inner.id || socket_inflight_reaches_socket(id, self.inner.id)
            }
        }
    }

    fn finish_result<T>(&self, result: Result<T, SocketError>) -> Result<T, SocketError> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                if should_latch_error(err) {
                    *self.inner.last_error.lock() = Some(err);
                }
                Err(err)
            }
        }
    }
}

/// 套接字内部共享状态。
pub(crate) struct SocketInner {
    /// 全局唯一 ID
    pub(crate) id: u64,
    /// 套接字类型(Stream/Datagram/Sequenced)
    pub(crate) kind: SocketType,
    /// 创建者身份凭证
    pub(crate) owner: PeerIdentity,
    /// SO_PASSCRED 开关
    pub(crate) passcred: AtomicBool,
    /// 绑定的本地地址
    pub(crate) local_name: Spinlock<Option<UnixAddress>>,
    /// 是否已关闭
    pub(crate) closed: Spinlock<bool>,
    /// 最近一次可缓存的错误(SO_ERROR)
    pub(crate) last_error: Spinlock<Option<SocketError>>,
    /// 套接字选项
    pub(crate) options: Spinlock<SocketOptions>,
    /// 按类型分发的具体实现
    pub(crate) kind_impl: SocketKind,
}

/// 按套接字类型分发的具体状态。
pub(crate) enum SocketKind {
    Stream(StreamSocket),
    Datagram(DatagramSocket),
    Sequenced(SequencedSocket),
}

/// Stream 套接字:状态机 + accept/connect 等待队列。
pub(crate) struct StreamSocket {
    pub(crate) state: Spinlock<StreamState>,
    pub(crate) accept_wait: sched::WaitQueue,
    pub(crate) connect_wait: sched::WaitQueue,
}

/// Stream 套接字状态机。
pub(crate) enum StreamState {
    Init,
    Listening(ListenerState<Socket>),
    Connected(ConnectedState),
    Closed,
}

/// Stream 已连接状态:持有收发两端的共享队列引用。
pub(crate) struct ConnectedState {
    /// 接收队列(对端写入,本端读取)
    pub(crate) rx: Arc<StreamQueue>,
    /// 发送队列(本端写入,对端读取)
    pub(crate) tx: Arc<StreamQueue>,
    /// 对端绑定地址
    pub(crate) peer_name: Option<UnixAddress>,
    /// 对端身份凭证
    pub(crate) peer_identity: PeerIdentity,
    /// 本端读半关闭
    pub(crate) read_shutdown: bool,
    /// 本端写半关闭
    pub(crate) write_shutdown: bool,
}

impl ConnectedState {
    pub(crate) fn readiness(&self) -> Readiness {
        let rx = self.rx.state.lock();
        let tx = self.tx.state.lock();
        let mut ready = Readiness::empty();
        if !self.read_shutdown && (!rx.chunks.is_empty() || rx.write_closed) {
            ready = ready.with(Readiness::READABLE);
        }
        if !self.write_shutdown
            && !tx.read_closed
            && tx.bytes < tx.limit_bytes
            && tx.chunks.len() < STREAM_CHUNK_LIMIT
        {
            ready = ready.with(Readiness::WRITABLE);
        }
        if rx.write_closed || tx.read_closed || self.read_shutdown || self.write_shutdown {
            ready = ready.with(Readiness::HANGUP);
        }
        if rx.write_closed {
            ready = ready.with(Readiness::READ_HANGUP);
        }
        if tx.read_closed {
            ready = ready.with(Readiness::FAULT);
        }
        ready
    }
}

/// 监听状态:backlog 容量 + 待 accept 的连接队列。
pub(crate) struct ListenerState<T> {
    pub(crate) backlog: usize,
    pub(crate) pending: VecDeque<T>,
}

/// Stream 字节流队列:双端各持有一个 Arc<StreamQueue>。
/// read_wait 在数据到达时唤醒读者,write_wait 在空间释放时唤醒写者。
pub(crate) struct StreamQueue {
    pub(crate) state: Spinlock<StreamQueueState>,
    pub(crate) read_wait: sched::WaitQueue,
    pub(crate) write_wait: sched::WaitQueue,
}

impl StreamQueue {
    pub(crate) fn new(limit_bytes: usize) -> Self {
        Self {
            state: Spinlock::new(StreamQueueState {
                chunks: VecDeque::new(),
                bytes: 0,
                limit_bytes: limit_bytes.max(1),
                write_closed: false,
                read_closed: false,
            }),
            read_wait: sched::WaitQueue::new(),
            write_wait: sched::WaitQueue::new(),
        }
    }
}

pub(crate) struct StreamQueueState {
    pub(crate) chunks: VecDeque<StreamChunk>,
    pub(crate) bytes: usize,
    pub(crate) limit_bytes: usize,
    pub(crate) write_closed: bool,
    pub(crate) read_closed: bool,
}

/// Stream 字节流中的一个数据片段。
pub(crate) struct StreamChunk {
    /// 数据负载
    pub(crate) bytes: Vec<u8>,
    /// 当前读取偏移(部分消费后前移)
    pub(crate) offset: usize,
    /// 附带传递的句柄(SCM_RIGHTS)
    pub(crate) handles: Vec<SharedHandle>,
    /// 发送方身份
    pub(crate) sender_identity: PeerIdentity,
    /// 显式凭证(SO_PASSCRED 时填充)
    pub(crate) control_identity: Option<PeerIdentity>,
}

impl StreamChunk {
    /// 返回尚未消费的字节数。
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
}

/// Datagram 套接字:无连接消息收发。
pub(crate) struct DatagramSocket {
    pub(crate) state: Spinlock<DatagramState>,
    pub(crate) read_wait: sched::WaitQueue,
    pub(crate) write_wait: sched::WaitQueue,
}

/// Datagram 状态:消息队列 + 流控。
pub(crate) struct DatagramState {
    /// 接收队列
    pub(crate) queue: VecDeque<MessagePacket>,
    /// 队列中已缓存的总字节数
    pub(crate) queued_bytes: usize,
    /// 接收缓冲区容量上限
    pub(crate) queue_limit_bytes: usize,
    /// 已 connect 的对端(若有)
    pub(crate) connected: Option<DatagramPeer>,
    /// 对端身份
    pub(crate) peer_identity: Option<PeerIdentity>,
    /// 读半关闭
    pub(crate) read_shutdown: bool,
    /// 写半关闭
    pub(crate) write_shutdown: bool,
}

/// Datagram 对端引用。
pub(crate) enum DatagramPeer {
    Bound {
        address: UnixAddress,
        target: Weak<SocketInner>,
    },
}

impl DatagramPeer {
    pub(crate) fn address(&self) -> UnixAddress {
        match self {
            Self::Bound { address, .. } => address.clone(),
        }
    }
}

/// Sequenced 套接字:面向连接的消息报文。
pub(crate) struct SequencedSocket {
    pub(crate) state: Spinlock<SequencedState>,
    pub(crate) accept_wait: sched::WaitQueue,
    pub(crate) connect_wait: sched::WaitQueue,
}

/// Sequenced 套接字状态机。
pub(crate) enum SequencedState {
    Init,
    Listening(ListenerState<Socket>),
    Connected(SeqpacketConnectedState),
    Closed,
}

/// Sequenced 已连接状态:持有收发两端的消息队列。
pub(crate) struct SeqpacketConnectedState {
    pub(crate) local_name: Option<UnixAddress>,
    pub(crate) rx: Arc<PacketQueue>,
    pub(crate) tx: Arc<PacketQueue>,
    pub(crate) peer_name: Option<UnixAddress>,
    pub(crate) peer_identity: PeerIdentity,
    pub(crate) read_shutdown: bool,
    pub(crate) write_shutdown: bool,
}

impl SeqpacketConnectedState {
    pub(crate) fn readiness(&self) -> Readiness {
        let rx = self.rx.state.lock();
        let tx = self.tx.state.lock();
        let mut ready = Readiness::empty();
        if !self.read_shutdown && (!rx.packets.is_empty() || rx.write_closed) {
            ready = ready.with(Readiness::READABLE);
        }
        if !self.write_shutdown
            && !tx.read_closed
            && tx.bytes < tx.limit_bytes
            && tx.packets.len() < MESSAGE_PACKET_LIMIT
        {
            ready = ready.with(Readiness::WRITABLE);
        }
        if rx.write_closed || tx.read_closed || self.read_shutdown || self.write_shutdown {
            ready = ready.with(Readiness::HANGUP);
        }
        if rx.write_closed {
            ready = ready.with(Readiness::READ_HANGUP);
        }
        if tx.read_closed {
            ready = ready.with(Readiness::FAULT);
        }
        ready
    }
}

/// Sequenced/Datagram 消息队列:保留消息边界。
pub(crate) struct PacketQueue {
    pub(crate) state: Spinlock<PacketQueueState>,
    pub(crate) read_wait: sched::WaitQueue,
    pub(crate) write_wait: sched::WaitQueue,
}

impl PacketQueue {
    pub(crate) fn new(limit_bytes: usize) -> Self {
        Self {
            state: Spinlock::new(PacketQueueState {
                packets: VecDeque::new(),
                bytes: 0,
                limit_bytes: limit_bytes.max(1),
                write_closed: false,
                read_closed: false,
            }),
            read_wait: sched::WaitQueue::new(),
            write_wait: sched::WaitQueue::new(),
        }
    }
}

pub(crate) struct PacketQueueState {
    pub(crate) packets: VecDeque<MessagePacket>,
    pub(crate) bytes: usize,
    pub(crate) limit_bytes: usize,
    pub(crate) write_closed: bool,
    pub(crate) read_closed: bool,
}

/// 消息报文:Datagram 和 Sequenced 共用。
pub(crate) struct MessagePacket {
    /// 报文数据
    pub(crate) bytes: Vec<u8>,
    /// 附带传递的句柄
    pub(crate) handles: Vec<SharedHandle>,
    /// 发送方地址
    pub(crate) sender: Option<UnixAddress>,
    /// 发送方身份
    pub(crate) sender_identity: PeerIdentity,
    /// 显式凭证
    pub(crate) control_identity: Option<PeerIdentity>,
}

static REGISTRY: Spinlock<BTreeMap<BindingKey, Weak<SocketInner>>> = Spinlock::new(BTreeMap::new());
static SOCKETS: Spinlock<BTreeMap<u64, Weak<SocketInner>>> = Spinlock::new(BTreeMap::new());
static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);

fn register_socket(socket: &Arc<SocketInner>) {
    SOCKETS.lock().insert(socket.id, Arc::downgrade(socket));
}

#[kernel_symbols::export(
    name = "socket.snapshot_sockets",
    contract = "kernel.ipc.unix-socket@1",
    version = 1,
    capabilities = kernel_symbols::capability::IPC,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
        | kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn snapshot_sockets() -> Vec<Socket> {
    let mut sockets = SOCKETS.lock();
    let ids: Vec<u64> = sockets.keys().copied().collect();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(found) = sockets.get(&id).cloned() else {
            continue;
        };
        if let Some(inner) = found.upgrade() {
            out.push(Socket { inner });
        } else {
            sockets.remove(&id);
        }
    }
    out
}

pub(crate) fn ensure_name_len(address: &UnixAddress) -> Result<(), SocketError> {
    let len = match address {
        UnixAddress::Unnamed => 0,
        UnixAddress::Abstract(name) => name.len(),
        UnixAddress::Path { display, .. } => display.len(),
    };
    if len > 107 {
        return Err(SocketError::NameTooLong);
    }
    Ok(())
}

pub(crate) fn registry_insert(
    key: BindingKey,
    socket: &Arc<SocketInner>,
) -> Result<(), SocketError> {
    let mut registry = REGISTRY.lock();
    if let Some(existing) = registry.get(&key).and_then(Weak::upgrade) {
        if Arc::ptr_eq(&existing, socket) {
            return Ok(());
        }
        return Err(SocketError::NameAlreadyBound);
    }
    registry.insert(key, Arc::downgrade(socket));
    Ok(())
}

pub(crate) fn registry_lookup(key: &BindingKey) -> Option<Socket> {
    let mut registry = REGISTRY.lock();
    let Some(found) = registry.get(key).cloned() else {
        return None;
    };
    if let Some(inner) = found.upgrade() {
        return Some(Socket { inner });
    }
    registry.remove(key);
    None
}

pub(crate) fn registry_remove(key: &BindingKey, socket: &Arc<SocketInner>) {
    let mut registry = REGISTRY.lock();
    let should_remove = registry
        .get(key)
        .and_then(Weak::upgrade)
        .is_some_and(|current| Arc::ptr_eq(&current, socket));
    if should_remove {
        registry.remove(key);
    }
}

#[kernel_symbols::export(
    name = "socket.unregister_path_socket",
    contract = "kernel.ipc.unix-socket@1",
    version = 1,
    capabilities = kernel_symbols::capability::IPC,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_path_socket(key: PathKey) {
    REGISTRY.lock().remove(&BindingKey::Path(key));
}

fn socket_inflight_reaches_socket(start_id: u64, needle_id: u64) -> bool {
    fn lookup_socket(id: u64) -> Option<Arc<SocketInner>> {
        let mut sockets = SOCKETS.lock();
        let Some(found) = sockets.get(&id).cloned() else {
            return None;
        };
        if let Some(inner) = found.upgrade() {
            Some(inner)
        } else {
            sockets.remove(&id);
            None
        }
    }

    fn push_handle(handle: &SharedHandle, needle_id: u64, stack: &mut Vec<u64>) -> bool {
        let Some(HandleIdentity::Socket(id)) = handle.identity() else {
            return false;
        };
        if id == needle_id {
            return true;
        }
        stack.push(id);
        false
    }

    fn push_stream_queue(queue: &StreamQueue, needle_id: u64, stack: &mut Vec<u64>) -> bool {
        let state = queue.state.lock();
        for chunk in &state.chunks {
            for handle in &chunk.handles {
                if push_handle(handle, needle_id, stack) {
                    return true;
                }
            }
        }
        false
    }

    fn push_packet_queue(queue: &PacketQueue, needle_id: u64, stack: &mut Vec<u64>) -> bool {
        let state = queue.state.lock();
        for packet in &state.packets {
            for handle in &packet.handles {
                if push_handle(handle, needle_id, stack) {
                    return true;
                }
            }
        }
        false
    }

    fn push_socket_edges(inner: &Arc<SocketInner>, needle_id: u64, stack: &mut Vec<u64>) -> bool {
        match &inner.kind_impl {
            SocketKind::Stream(stream) => {
                let state = stream.state.lock();
                match &*state {
                    StreamState::Connected(conn) => {
                        let rx = Arc::clone(&conn.rx);
                        let tx = Arc::clone(&conn.tx);
                        drop(state);
                        push_stream_queue(&rx, needle_id, stack)
                            || push_stream_queue(&tx, needle_id, stack)
                    }
                    StreamState::Listening(listener) => {
                        for socket in &listener.pending {
                            if socket.inner.id == needle_id {
                                return true;
                            }
                            stack.push(socket.inner.id);
                        }
                        false
                    }
                    StreamState::Init | StreamState::Closed => false,
                }
            }
            SocketKind::Datagram(dgram) => {
                let state = dgram.state.lock();
                for packet in &state.queue {
                    for handle in &packet.handles {
                        if push_handle(handle, needle_id, stack) {
                            return true;
                        }
                    }
                }
                false
            }
            SocketKind::Sequenced(seq) => {
                let state = seq.state.lock();
                match &*state {
                    SequencedState::Connected(conn) => {
                        let rx = Arc::clone(&conn.rx);
                        let tx = Arc::clone(&conn.tx);
                        drop(state);
                        push_packet_queue(&rx, needle_id, stack)
                            || push_packet_queue(&tx, needle_id, stack)
                    }
                    SequencedState::Listening(listener) => {
                        for socket in &listener.pending {
                            if socket.inner.id == needle_id {
                                return true;
                            }
                            stack.push(socket.inner.id);
                        }
                        false
                    }
                    SequencedState::Init | SequencedState::Closed => false,
                }
            }
        }
    }

    let mut visited = Vec::new();
    let mut stack = vec![start_id];
    while let Some(id) = stack.pop() {
        if id == needle_id {
            return true;
        }
        if visited.contains(&id) {
            continue;
        }
        visited.push(id);
        let Some(inner) = lookup_socket(id) else {
            continue;
        };
        if push_socket_edges(&inner, needle_id, &mut stack) {
            return true;
        }
    }
    false
}

pub(crate) fn default_socket_options(kind: SocketType) -> SocketOptions {
    match kind {
        SocketType::Stream => SocketOptions::default(),
        SocketType::Datagram | SocketType::Sequenced | SocketType::Raw => SocketOptions {
            send_buffer_size: DEFAULT_MESSAGE_BUFFER_SIZE,
            recv_buffer_size: DEFAULT_MESSAGE_BUFFER_SIZE,
            ..SocketOptions::default()
        },
    }
}

pub(crate) fn timeout_to_deadline(timeout: Option<SocketTimeval>) -> Option<u64> {
    let timeout = timeout?;
    let nanos = (timeout.secs as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add((timeout.micros as u64).saturating_mul(1_000));
    Some(sched::now_ns_public().saturating_add(nanos))
}

pub(crate) fn effective_recv_deadline(
    socket: &Arc<SocketInner>,
    explicit: Option<u64>,
    nonblocking: bool,
) -> Option<u64> {
    if nonblocking {
        None
    } else {
        explicit.or_else(|| timeout_to_deadline(socket.options.lock().recv_timeout))
    }
}

pub(crate) fn effective_send_deadline(
    socket: &Arc<SocketInner>,
    explicit: Option<u64>,
    nonblocking: bool,
) -> Option<u64> {
    if nonblocking {
        None
    } else {
        explicit.or_else(|| timeout_to_deadline(socket.options.lock().send_timeout))
    }
}

pub(crate) fn should_latch_error(err: SocketError) -> bool {
    !matches!(
        err,
        SocketError::Unsupported
            | SocketError::UnsupportedAddressSpace
            | SocketError::UnsupportedType
            | SocketError::InvalidInput
            | SocketError::NameTooLong
            | SocketError::NameAlreadyBound
            | SocketError::NameUnavailable
            | SocketError::StateMismatch
            | SocketError::AlreadyConnected
            | SocketError::ListenerRequired
            | SocketError::DestinationRequired
            | SocketError::TemporaryUnavailable
            | SocketError::Interrupted
    )
}
