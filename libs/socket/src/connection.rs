//! 连接管理:listen / accept / connect 以及 socketpair 的建立逻辑。
//!
//! 本模块包含面向连接类型(Stream、Sequenced)的状态机操作 trait,
//! 以及具体的连接建立、接受和 pair 初始化函数。

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use sched::WaitQueue;
use sched::sync::Spinlock;

use crate::state::{
    ConnectedState, DatagramPeer, DatagramSocket, ListenerState, PacketQueue,
    SeqpacketConnectedState, SequencedSocket, SequencedState, Socket, SocketKind, SocketOptions,
    StreamQueue, StreamSocket, StreamState, registry_lookup,
};
use crate::types::{
    PeerIdentity, ReceiveOptions, SendOptions, SocketError, SocketType, UnixAddress,
};
use crate::wait::{wait_while, wake_task};

/// 连接型套接字的通用状态机操作 trait。
/// Stream 和 Sequenced 两种类型各自实现此 trait。
pub(crate) trait ConnectionStateOps {
    type Connected;
    type Accepted;

    /// 构造 Listening 状态
    fn listening(listener: ListenerState<Self::Accepted>) -> Self;
    /// 获取已连接状态的可变引用
    fn connected_mut(&mut self) -> Option<&mut Self::Connected>;
    /// 获取已连接状态的只读引用
    fn connected_ref(&self) -> Option<&Self::Connected>;
    /// 获取监听状态的可变引用
    fn listener_mut(&mut self) -> Option<&mut ListenerState<Self::Accepted>>;
    /// 获取监听状态的只读引用
    fn listener_ref(&self) -> Option<&ListenerState<Self::Accepted>>;
    /// 是否处于初始状态
    fn is_init(&self) -> bool;
    /// 是否已关闭
    fn is_closed(&self) -> bool;
    /// 关闭并返回旧状态(用于析构资源)
    fn close_replace(&mut self) -> Self;
}

impl ConnectionStateOps for StreamState {
    type Connected = ConnectedState;
    type Accepted = Socket;

    fn listening(listener: ListenerState<Self::Accepted>) -> Self {
        Self::Listening(listener)
    }

    fn connected_mut(&mut self) -> Option<&mut ConnectedState> {
        match self {
            Self::Connected(conn) => Some(conn),
            _ => None,
        }
    }

    fn connected_ref(&self) -> Option<&ConnectedState> {
        match self {
            Self::Connected(conn) => Some(conn),
            _ => None,
        }
    }

    fn listener_mut(&mut self) -> Option<&mut ListenerState<Self::Accepted>> {
        match self {
            Self::Listening(listener) => Some(listener),
            _ => None,
        }
    }

    fn listener_ref(&self) -> Option<&ListenerState<Self::Accepted>> {
        match self {
            Self::Listening(listener) => Some(listener),
            _ => None,
        }
    }

    fn is_init(&self) -> bool {
        matches!(self, Self::Init)
    }

    fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }

    fn close_replace(&mut self) -> Self {
        core::mem::replace(self, Self::Closed)
    }
}

impl ConnectionStateOps for SequencedState {
    type Connected = SeqpacketConnectedState;
    type Accepted = Socket;

    fn listening(listener: ListenerState<Self::Accepted>) -> Self {
        Self::Listening(listener)
    }

    fn connected_mut(&mut self) -> Option<&mut SeqpacketConnectedState> {
        match self {
            Self::Connected(conn) => Some(conn),
            _ => None,
        }
    }

    fn connected_ref(&self) -> Option<&SeqpacketConnectedState> {
        match self {
            Self::Connected(conn) => Some(conn),
            _ => None,
        }
    }

    fn listener_mut(&mut self) -> Option<&mut ListenerState<Self::Accepted>> {
        match self {
            Self::Listening(listener) => Some(listener),
            _ => None,
        }
    }

    fn listener_ref(&self) -> Option<&ListenerState<Self::Accepted>> {
        match self {
            Self::Listening(listener) => Some(listener),
            _ => None,
        }
    }

    fn is_init(&self) -> bool {
        matches!(self, Self::Init)
    }

    fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }

    fn close_replace(&mut self) -> Self {
        core::mem::replace(self, Self::Closed)
    }
}

/// 将套接字转为监听状态,或更新已有监听队列的 backlog 值。
pub(crate) fn listen_connection_state<S: ConnectionStateOps>(
    state_lock: &Spinlock<S>,
    backlog: usize,
) -> Result<(), SocketError> {
    let mut state = state_lock.lock();
    if state.connected_ref().is_some() {
        return Err(SocketError::StateMismatch);
    }
    if state.is_closed() {
        return Err(SocketError::PeerClosed);
    }
    if let Some(listener) = state.listener_mut() {
        listener.backlog = backlog.max(1);
        return Ok(());
    }
    *state = S::listening(ListenerState {
        backlog: backlog.max(1),
        pending: VecDeque::new(),
    });
    Ok(())
}

/// 从监听队列中取出一个待处理连接(accept 系统调用核心逻辑)。
/// 若队列为空则阻塞等待,直到有新连接到来、超时或被信号中断。
pub(crate) fn accept_connection_socket<S: ConnectionStateOps<Accepted = Socket>>(
    state_lock: &Spinlock<S>,
    accept_wait: &WaitQueue,
    connect_wait: &WaitQueue,
    options: ReceiveOptions,
) -> Result<Socket, SocketError> {
    loop {
        let mut state = state_lock.lock();
        let Some(listener) = state.listener_mut() else {
            return if state.is_closed() {
                Err(SocketError::PeerClosed)
            } else {
                Err(SocketError::ListenerRequired)
            };
        };
        if let Some(next) = listener.pending.pop_front() {
            drop(state);
            connect_wait.wake_one_with(wake_task);
            return Ok(next);
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(state);
        wait_while(
            accept_wait,
            || {
                let state = state_lock.lock();
                state
                    .listener_ref()
                    .is_some_and(|listener| listener.pending.is_empty())
            },
            options.deadline_ns,
        )?;
    }
}

/// 创建一对 Stream 连接状态(双向管道):a.rx = b.tx, a.tx = b.rx。
fn make_connected_states(
    a_local: Option<UnixAddress>,
    a_peer: PeerIdentity,
    b_local: Option<UnixAddress>,
    b_peer: PeerIdentity,
    a_recv_limit: usize,
    b_recv_limit: usize,
) -> (ConnectedState, ConnectedState) {
    let a_rx = Arc::new(StreamQueue::new(a_recv_limit));
    let b_rx = Arc::new(StreamQueue::new(b_recv_limit));
    let a = ConnectedState {
        rx: Arc::clone(&a_rx),
        tx: Arc::clone(&b_rx),
        peer_name: b_local.clone(),
        peer_identity: a_peer,
        read_shutdown: false,
        write_shutdown: false,
    };
    let b = ConnectedState {
        rx: b_rx,
        tx: a_rx,
        peer_name: a_local,
        peer_identity: b_peer,
        read_shutdown: false,
        write_shutdown: false,
    };
    (a, b)
}

/// 创建一对 Sequenced 连接状态(双向消息管道)。
fn make_seqpacket_states(
    a_local: Option<UnixAddress>,
    a_peer: PeerIdentity,
    b_local: Option<UnixAddress>,
    b_peer: PeerIdentity,
    a_recv_limit: usize,
    b_recv_limit: usize,
) -> (SeqpacketConnectedState, SeqpacketConnectedState) {
    let a_rx = Arc::new(PacketQueue::new(a_recv_limit));
    let b_rx = Arc::new(PacketQueue::new(b_recv_limit));
    let a = SeqpacketConnectedState {
        local_name: a_local.clone(),
        rx: Arc::clone(&a_rx),
        tx: Arc::clone(&b_rx),
        peer_name: b_local.clone(),
        peer_identity: a_peer,
        read_shutdown: false,
        write_shutdown: false,
    };
    let b = SeqpacketConnectedState {
        local_name: b_local.clone(),
        rx: b_rx,
        tx: a_rx,
        peer_name: a_local,
        peer_identity: b_peer,
        read_shutdown: false,
        write_shutdown: false,
    };
    (a, b)
}

/// 创建 Stream 连接:生成客户端连接状态和服务端 Socket 对象。
fn make_stream_connection(
    client_local: Option<UnixAddress>,
    server_identity: PeerIdentity,
    server_local: Option<UnixAddress>,
    client_identity: PeerIdentity,
    client_recv_limit: usize,
    server_name: Option<UnixAddress>,
    server_passcred: bool,
    server_options: SocketOptions,
) -> Result<(ConnectedState, Socket), SocketError> {
    let server_recv_limit = server_options.recv_buffer_size;
    let (client_state, server_state) = make_connected_states(
        client_local.clone(),
        server_identity,
        server_local.clone(),
        client_identity,
        client_recv_limit,
        server_recv_limit,
    );
    let server = Socket::new_unix(SocketType::Stream, server_identity)?;
    *server.inner.local_name.lock() = server_name;
    server.set_passcred(server_passcred);
    *server.inner.options.lock() = server_options;
    let SocketKind::Stream(server_stream) = &server.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    *server_stream.state.lock() = StreamState::Connected(server_state);
    Ok((client_state, server))
}

/// 创建 Sequenced 连接:生成客户端连接状态和服务端 Socket 对象。
fn make_seqpacket_connection(
    client_local: Option<UnixAddress>,
    server_identity: PeerIdentity,
    server_local: Option<UnixAddress>,
    client_identity: PeerIdentity,
    client_recv_limit: usize,
    server_name: Option<UnixAddress>,
    server_passcred: bool,
    server_options: SocketOptions,
) -> Result<(SeqpacketConnectedState, Socket), SocketError> {
    let server_recv_limit = server_options.recv_buffer_size;
    let (client_state, server_state) = make_seqpacket_states(
        client_local.clone(),
        server_identity,
        server_local.clone(),
        client_identity,
        client_recv_limit,
        server_recv_limit,
    );
    let server = Socket::new_unix(SocketType::Sequenced, server_identity)?;
    *server.inner.local_name.lock() = server_name;
    server.set_passcred(server_passcred);
    *server.inner.options.lock() = server_options;
    let SocketKind::Sequenced(server_seq) = &server.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    *server_seq.state.lock() = SequencedState::Connected(server_state);
    Ok((client_state, server))
}

/// 为 socketpair 初始化两个 Stream 套接字的已连接状态。
pub(crate) fn install_stream_pair(a: &Socket, b: &Socket) -> Result<(), SocketError> {
    let a_local = a.inner.local_name.lock().clone();
    let b_local = b.inner.local_name.lock().clone();
    let a_recv_limit = a.inner.options.lock().recv_buffer_size;
    let b_recv_limit = b.inner.options.lock().recv_buffer_size;
    let (a_conn, b_conn) = make_connected_states(
        a_local,
        b.inner.owner,
        b_local,
        a.inner.owner,
        a_recv_limit,
        b_recv_limit,
    );
    let SocketKind::Stream(a_stream) = &a.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    let SocketKind::Stream(b_stream) = &b.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    *a_stream.state.lock() = StreamState::Connected(a_conn);
    *b_stream.state.lock() = StreamState::Connected(b_conn);
    Ok(())
}

/// 为 socketpair 初始化两个 Datagram 套接字的双向绑定。
pub(crate) fn install_dgram_pair(a: &Socket, b: &Socket) -> Result<(), SocketError> {
    let SocketKind::Datagram(a_dgram) = &a.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    let SocketKind::Datagram(b_dgram) = &b.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    let a_peer = DatagramPeer::Bound {
        address: UnixAddress::Unnamed,
        target: Arc::downgrade(&b.inner),
    };
    let b_peer = DatagramPeer::Bound {
        address: UnixAddress::Unnamed,
        target: Arc::downgrade(&a.inner),
    };
    {
        let mut state = a_dgram.state.lock();
        state.connected = Some(a_peer);
        state.peer_identity = Some(b.inner.owner);
    }
    {
        let mut state = b_dgram.state.lock();
        state.connected = Some(b_peer);
        state.peer_identity = Some(a.inner.owner);
    }
    Ok(())
}

/// 为 socketpair 初始化两个 Sequenced 套接字的已连接状态。
pub(crate) fn install_seqpacket_pair(a: &Socket, b: &Socket) -> Result<(), SocketError> {
    let a_local = a.inner.local_name.lock().clone();
    let b_local = b.inner.local_name.lock().clone();
    let a_recv_limit = a.inner.options.lock().recv_buffer_size;
    let b_recv_limit = b.inner.options.lock().recv_buffer_size;
    let (a_conn, b_conn) = make_seqpacket_states(
        a_local,
        b.inner.owner,
        b_local,
        a.inner.owner,
        a_recv_limit,
        b_recv_limit,
    );
    let SocketKind::Sequenced(a_seq) = &a.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    let SocketKind::Sequenced(b_seq) = &b.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    *a_seq.state.lock() = SequencedState::Connected(a_conn);
    *b_seq.state.lock() = SequencedState::Connected(b_conn);
    Ok(())
}

/// Stream 套接字主动连接:查找目标监听者,建立双向通道。
/// 若监听队列已满则阻塞等待空位。
pub(crate) fn connect_stream(
    socket: &Socket,
    stream: &StreamSocket,
    address: UnixAddress,
    caller: PeerIdentity,
    options: SendOptions,
) -> Result<(), SocketError> {
    let key = address.binding_key().ok_or(SocketError::InvalidInput)?;
    loop {
        let peer = registry_lookup(&key).ok_or(SocketError::ConnectionRejected)?;
        let SocketKind::Stream(peer_stream) = &peer.inner.kind_impl else {
            return Err(SocketError::StateMismatch);
        };
        let mut peer_state = peer_stream.state.lock();
        let Some(listener) = peer_state.listener_mut() else {
            return Err(SocketError::ConnectionRejected);
        };
        if listener.pending.len() < listener.backlog {
            let mut my_state = stream.state.lock();
            if !my_state.is_init() {
                return if my_state.connected_ref().is_some() {
                    Err(SocketError::AlreadyConnected)
                } else {
                    Err(SocketError::StateMismatch)
                };
            }
            let local = socket.inner.local_name.lock().clone();
            let remote = peer.inner.local_name.lock().clone();
            let client_recv_limit = socket.inner.options.lock().recv_buffer_size;
            let peer_options = *peer.inner.options.lock();
            let (client_conn, server_socket) = make_stream_connection(
                local,
                peer.inner.owner,
                remote.clone(),
                caller,
                client_recv_limit,
                remote,
                peer.passcred_enabled(),
                peer_options,
            )?;
            *my_state = StreamState::Connected(client_conn);
            listener.pending.push_back(server_socket);
            drop(my_state);
            drop(peer_state);
            peer_stream.accept_wait.wake_one_with(wake_task);
            return Ok(());
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(peer_state);
        wait_while(
            &peer_stream.connect_wait,
            || {
                let state = peer_stream.state.lock();
                state
                    .listener_ref()
                    .is_some_and(|listener| listener.pending.len() >= listener.backlog)
            },
            options.deadline_ns,
        )?;
    }
}

/// Sequenced 套接字主动连接:查找目标监听者,建立双向消息通道。
pub(crate) fn connect_seqpacket(
    socket: &Socket,
    seq: &SequencedSocket,
    address: UnixAddress,
    caller: PeerIdentity,
    options: SendOptions,
) -> Result<(), SocketError> {
    let key = address.binding_key().ok_or(SocketError::InvalidInput)?;
    loop {
        let peer = registry_lookup(&key).ok_or(SocketError::ConnectionRejected)?;
        let SocketKind::Sequenced(peer_seq) = &peer.inner.kind_impl else {
            return Err(SocketError::StateMismatch);
        };
        let mut peer_state = peer_seq.state.lock();
        let Some(listener) = peer_state.listener_mut() else {
            return Err(SocketError::ConnectionRejected);
        };
        if listener.pending.len() < listener.backlog {
            let mut my_state = seq.state.lock();
            if !my_state.is_init() {
                return if my_state.connected_ref().is_some() {
                    Err(SocketError::AlreadyConnected)
                } else {
                    Err(SocketError::StateMismatch)
                };
            }
            let local = socket.inner.local_name.lock().clone();
            let remote = peer.inner.local_name.lock().clone();
            let client_recv_limit = socket.inner.options.lock().recv_buffer_size;
            let peer_options = *peer.inner.options.lock();
            let (client_conn, server_socket) = make_seqpacket_connection(
                local,
                peer.inner.owner,
                remote.clone(),
                caller,
                client_recv_limit,
                remote,
                peer.passcred_enabled(),
                peer_options,
            )?;
            *my_state = SequencedState::Connected(client_conn);
            listener.pending.push_back(server_socket);
            drop(my_state);
            drop(peer_state);
            peer_seq.accept_wait.wake_one_with(wake_task);
            return Ok(());
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(peer_state);
        wait_while(
            &peer_seq.connect_wait,
            || {
                let state = peer_seq.state.lock();
                state
                    .listener_ref()
                    .is_some_and(|listener| listener.pending.len() >= listener.backlog)
            },
            options.deadline_ns,
        )?;
    }
}

/// Datagram 套接字连接:记录目标地址,使后续 send 无需指定目标。
/// 传入 Unnamed 地址则断开已有连接。
pub(crate) fn connect_datagram(
    dgram: &DatagramSocket,
    address: UnixAddress,
    _options: SendOptions,
) -> Result<(), SocketError> {
    if matches!(address, UnixAddress::Unnamed) {
        let mut state = dgram.state.lock();
        state.connected = None;
        state.peer_identity = None;
        return Ok(());
    }
    let key = address.binding_key().ok_or(SocketError::InvalidInput)?;
    let peer = registry_lookup(&key).ok_or(SocketError::ConnectionRejected)?;
    if peer.inner.kind != SocketType::Datagram {
        return Err(SocketError::StateMismatch);
    }
    let mut state = dgram.state.lock();
    if state.write_shutdown {
        return Err(SocketError::PeerClosed);
    }
    state.connected = Some(DatagramPeer::Bound {
        address,
        target: Arc::downgrade(&peer.inner),
    });
    state.peer_identity = Some(peer.inner.owner);
    Ok(())
}
