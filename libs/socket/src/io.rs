//! 套接字 I/O 操作:收发数据、就绪状态查询、等待注册、shutdown 和 close。
//!
//! 每种套接字类型(Stream/Datagram/Sequenced)各有独立的 send/recv/shutdown/close 实现。
//! 所有阻塞操作通过 [`wait_while`] 实现条件等待,支持非阻塞模式和超时。

use alloc::sync::Arc;
use alloc::vec::Vec;

use sched::Task;

use crate::connection::ConnectionStateOps;
use crate::state::{
    DatagramPeer, DatagramSocket, MESSAGE_BUFFER_LIMIT, MESSAGE_PACKET_LIMIT, MessagePacket,
    STREAM_CHUNK_LIMIT, SequencedSocket, SequencedState, Socket, SocketInner, SocketKind,
    StreamChunk, StreamSocket, StreamState,
};
use crate::types::{
    SocketError, PeerIdentity, Readiness, ReceiveOptions, ReceiveResult, SendOptions, SharedHandle,
    SocketShutdown, UnixAddress,
};
use crate::wait::{wait_while, wake_task};

/// 查询 Stream 套接字的就绪状态。
pub(crate) fn stream_readiness(stream: &StreamSocket) -> Readiness {
    let state = stream.state.lock();
    match &*state {
        StreamState::Init => Readiness::empty(),
        StreamState::Listening(listener) => {
            if listener.pending.is_empty() {
                Readiness::empty()
            } else {
                Readiness::READABLE
            }
        }
        StreamState::Connected(conn) => conn.readiness(),
        StreamState::Closed => Readiness::HANGUP.with(Readiness::FAULT),
    }
}

/// 查询 Datagram 套接字的就绪状态。
pub(crate) fn datagram_readiness(dgram: &DatagramSocket) -> Readiness {
    let state = dgram.state.lock();
    let mut ready = Readiness::empty();
    if !state.queue.is_empty() || state.read_shutdown {
        ready = ready.with(Readiness::READABLE);
    }
    if !state.write_shutdown && state.queued_bytes < state.queue_limit_bytes {
        ready = ready.with(Readiness::WRITABLE);
    }
    if state.read_shutdown || state.write_shutdown {
        ready = ready.with(Readiness::HANGUP);
    }
    ready
}

/// 查询 Sequenced 套接字的就绪状态。
pub(crate) fn seqpacket_readiness(seq: &SequencedSocket) -> Readiness {
    let state = seq.state.lock();
    match &*state {
        SequencedState::Init => Readiness::empty(),
        SequencedState::Listening(listener) => {
            if listener.pending.is_empty() {
                Readiness::empty()
            } else {
                Readiness::READABLE
            }
        }
        SequencedState::Connected(conn) => conn.readiness(),
        SequencedState::Closed => Readiness::HANGUP.with(Readiness::FAULT),
    }
}

/// 注册 Stream 等待者:将 task 加入对应的等待队列。
pub(crate) fn register_stream_waiter(
    stream: &StreamSocket,
    task: &Arc<Task>,
    interest: Readiness,
) -> bool {
    let state = stream.state.lock();
    match &*state {
        StreamState::Listening(_) => {
            if interest.has(Readiness::READABLE) || interest.has(Readiness::HANGUP) {
                stream.accept_wait.enqueue(task);
            }
            true
        }
        StreamState::Connected(conn) => {
            if interest.has(Readiness::READABLE) || interest.has(Readiness::HANGUP) {
                conn.rx.read_wait.enqueue(task);
            }
            if interest.has(Readiness::WRITABLE)
                || interest.has(Readiness::HANGUP)
                || interest.has(Readiness::FAULT)
            {
                conn.tx.write_wait.enqueue(task);
            }
            true
        }
        StreamState::Init | StreamState::Closed => false,
    }
}

pub(crate) fn unregister_stream_waiter(stream: &StreamSocket, task: &Arc<Task>) {
    let state = stream.state.lock();
    match &*state {
        StreamState::Listening(_) => stream.accept_wait.remove(task),
        StreamState::Connected(conn) => {
            conn.rx.read_wait.remove(task);
            conn.tx.write_wait.remove(task);
        }
        StreamState::Init | StreamState::Closed => {}
    }
}

pub(crate) fn register_seqpacket_waiter(
    seq: &SequencedSocket,
    task: &Arc<Task>,
    interest: Readiness,
) -> bool {
    let state = seq.state.lock();
    match &*state {
        SequencedState::Listening(_) => {
            if interest.has(Readiness::READABLE) || interest.has(Readiness::HANGUP) {
                seq.accept_wait.enqueue(task);
            }
            true
        }
        SequencedState::Connected(conn) => {
            if interest.has(Readiness::READABLE) || interest.has(Readiness::HANGUP) {
                conn.rx.read_wait.enqueue(task);
            }
            if interest.has(Readiness::WRITABLE)
                || interest.has(Readiness::HANGUP)
                || interest.has(Readiness::FAULT)
            {
                conn.tx.write_wait.enqueue(task);
            }
            true
        }
        SequencedState::Init | SequencedState::Closed => false,
    }
}

pub(crate) fn unregister_seqpacket_waiter(seq: &SequencedSocket, task: &Arc<Task>) {
    let state = seq.state.lock();
    match &*state {
        SequencedState::Listening(_) => seq.accept_wait.remove(task),
        SequencedState::Connected(conn) => {
            conn.rx.read_wait.remove(task);
            conn.tx.write_wait.remove(task);
        }
        SequencedState::Init | SequencedState::Closed => {}
    }
}

pub(crate) fn register_datagram_waiter(
    dgram: &DatagramSocket,
    task: &Arc<Task>,
    interest: Readiness,
) -> bool {
    let state = dgram.state.lock();
    if interest.has(Readiness::READABLE) || interest.has(Readiness::HANGUP) {
        dgram.read_wait.enqueue(task);
    }
    if interest.has(Readiness::WRITABLE) || interest.has(Readiness::FAULT) {
        if let Some(DatagramPeer::Bound { target, .. }) = &state.connected
            && let Some(inner) = target.upgrade()
            && let SocketKind::Datagram(peer_dgram) = &inner.kind_impl
        {
            peer_dgram.write_wait.enqueue(task);
            return true;
        }
        dgram.write_wait.enqueue(task);
    }
    true
}

pub(crate) fn unregister_datagram_waiter(dgram: &DatagramSocket, task: &Arc<Task>) {
    dgram.read_wait.remove(task);
    dgram.write_wait.remove(task);
    let state = dgram.state.lock();
    if let Some(DatagramPeer::Bound { target, .. }) = &state.connected
        && let Some(inner) = target.upgrade()
        && let SocketKind::Datagram(peer_dgram) = &inner.kind_impl
    {
        peer_dgram.write_wait.remove(task);
    }
}

/// 关闭 Stream 套接字的读/写端。
pub(crate) fn shutdown_stream(stream: &StreamSocket, how: SocketShutdown) -> Result<(), SocketError> {
    let mut state = stream.state.lock();
    let Some(conn) = state.connected_mut() else {
        return if state.is_closed() {
            Ok(())
        } else {
            Err(SocketError::ConnectionMissing)
        };
    };
    if matches!(how, SocketShutdown::Read | SocketShutdown::Both) && !conn.read_shutdown {
        conn.read_shutdown = true;
        let mut rx = conn.rx.state.lock();
        rx.read_closed = true;
        drop(rx);
        conn.rx.write_wait.wake_all_with(wake_task);
    }
    if matches!(how, SocketShutdown::Write | SocketShutdown::Both) && !conn.write_shutdown {
        conn.write_shutdown = true;
        let mut peer_rx = conn.tx.state.lock();
        peer_rx.write_closed = true;
        drop(peer_rx);
        conn.tx.read_wait.wake_all_with(wake_task);
    }
    Ok(())
}

/// 关闭 Datagram 套接字的读/写端。
pub(crate) fn shutdown_datagram(dgram: &DatagramSocket, how: SocketShutdown) -> Result<(), SocketError> {
    let mut state = dgram.state.lock();
    if matches!(how, SocketShutdown::Read | SocketShutdown::Both) {
        state.read_shutdown = true;
    }
    if matches!(how, SocketShutdown::Write | SocketShutdown::Both) {
        state.write_shutdown = true;
    }
    drop(state);
    dgram.read_wait.wake_all_with(wake_task);
    dgram.write_wait.wake_all_with(wake_task);
    Ok(())
}

/// 关闭 Sequenced 套接字的读/写端。
pub(crate) fn shutdown_seqpacket(seq: &SequencedSocket, how: SocketShutdown) -> Result<(), SocketError> {
    let mut state = seq.state.lock();
    let Some(conn) = state.connected_mut() else {
        return if state.is_closed() {
            Ok(())
        } else {
            Err(SocketError::ConnectionMissing)
        };
    };
    if matches!(how, SocketShutdown::Read | SocketShutdown::Both) && !conn.read_shutdown {
        conn.read_shutdown = true;
        let mut rx = conn.rx.state.lock();
        rx.read_closed = true;
        drop(rx);
        conn.rx.write_wait.wake_all_with(wake_task);
    }
    if matches!(how, SocketShutdown::Write | SocketShutdown::Both) && !conn.write_shutdown {
        conn.write_shutdown = true;
        let mut tx = conn.tx.state.lock();
        tx.write_closed = true;
        drop(tx);
        conn.tx.read_wait.wake_all_with(wake_task);
    }
    Ok(())
}

/// 关闭 Stream 套接字:释放连接资源,唤醒所有等待者。
pub(crate) fn close_stream(stream: &StreamSocket) {
    let mut state = stream.state.lock();
    let old = state.close_replace();
    drop(state);
    match old {
        StreamState::Connected(mut conn) => {
            if !conn.read_shutdown {
                let mut rx = conn.rx.state.lock();
                rx.read_closed = true;
                drop(rx);
                conn.rx.write_wait.wake_all_with(wake_task);
                conn.read_shutdown = true;
            }
            if !conn.write_shutdown {
                let mut tx = conn.tx.state.lock();
                tx.write_closed = true;
                drop(tx);
                conn.tx.read_wait.wake_all_with(wake_task);
                conn.write_shutdown = true;
            }
        }
        StreamState::Listening(listener) => {
            for pending in listener.pending {
                pending.close();
            }
            stream.accept_wait.wake_all_with(wake_task);
            stream.connect_wait.wake_all_with(wake_task);
        }
        StreamState::Init | StreamState::Closed => {}
    }
}

/// 关闭 Datagram 套接字:清空队列,唤醒所有等待者。
pub(crate) fn close_datagram(dgram: &DatagramSocket) {
    let mut state = dgram.state.lock();
    state.read_shutdown = true;
    state.write_shutdown = true;
    state.queue.clear();
    state.queued_bytes = 0;
    drop(state);
    dgram.read_wait.wake_all_with(wake_task);
    dgram.write_wait.wake_all_with(wake_task);
}

/// 关闭 Sequenced 套接字:释放连接资源,唤醒所有等待者。
pub(crate) fn close_seqpacket(seq: &SequencedSocket) {
    let mut state = seq.state.lock();
    let old = state.close_replace();
    drop(state);
    match old {
        SequencedState::Connected(mut conn) => {
            if !conn.read_shutdown {
                let mut rx = conn.rx.state.lock();
                rx.read_closed = true;
                drop(rx);
                conn.rx.write_wait.wake_all_with(wake_task);
                conn.read_shutdown = true;
            }
            if !conn.write_shutdown {
                let mut tx = conn.tx.state.lock();
                tx.write_closed = true;
                drop(tx);
                conn.tx.read_wait.wake_all_with(wake_task);
                conn.write_shutdown = true;
            }
        }
        SequencedState::Listening(listener) => {
            for pending in listener.pending {
                pending.close();
            }
            seq.accept_wait.wake_all_with(wake_task);
            seq.connect_wait.wake_all_with(wake_task);
        }
        SequencedState::Init | SequencedState::Closed => {}
    }
}

/// Stream 发送:将数据写入对端接收队列,支持阻塞/非阻塞和超时。
pub(crate) fn send_stream(
    stream: &StreamSocket,
    data: &[u8],
    handles: &[SharedHandle],
    sender_identity: PeerIdentity,
    options: SendOptions,
) -> Result<usize, SocketError> {
    loop {
        let (tx, write_shutdown) = {
            let state = stream.state.lock();
            match &*state {
                StreamState::Connected(conn) => (Arc::clone(&conn.tx), conn.write_shutdown),
                StreamState::Closed => return Err(SocketError::PeerClosed),
                _ => return Err(SocketError::ConnectionMissing),
            }
        };
        if write_shutdown {
            return Err(SocketError::PeerClosed);
        }
        let mut queue = tx.state.lock();
        if queue.read_closed {
            return Err(SocketError::PeerClosed);
        }
        let free_bytes = queue.limit_bytes.saturating_sub(queue.bytes);
        let can_enqueue_empty = !handles.is_empty() && queue.chunks.len() < STREAM_CHUNK_LIMIT;
        let take = if data.is_empty() {
            0
        } else {
            free_bytes.min(data.len())
        };
        if take > 0 || can_enqueue_empty {
            let payload = if take == 0 {
                Vec::new()
            } else {
                data[..take].to_vec()
            };
            queue.bytes = queue.bytes.saturating_add(take);
            queue.chunks.push_back(StreamChunk {
                bytes: payload,
                offset: 0,
                handles: handles.iter().cloned().collect(),
                sender_identity,
                control_identity: options.explicit_credentials.then_some(sender_identity),
            });
            drop(queue);
            tx.read_wait.wake_one_with(wake_task);
            return Ok(take);
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(queue);
        wait_while(
            &tx.write_wait,
            || {
                let queue = tx.state.lock();
                !queue.read_closed
                    && (queue.bytes >= queue.limit_bytes
                        || queue.chunks.len() >= STREAM_CHUNK_LIMIT)
            },
            options.deadline_ns,
        )?;
    }
}

/// Stream 接收:从本端接收队列读取数据,支持 peek 和 MSG_WAITALL。
pub(crate) fn recv_stream(
    stream: &StreamSocket,
    buffer: &mut [u8],
    options: ReceiveOptions,
    passcred: bool,
) -> Result<ReceiveResult, SocketError> {
    let (rx, peer_name, peer_identity, read_shutdown) = {
        let state = stream.state.lock();
        match &*state {
            StreamState::Connected(conn) => (
                Arc::clone(&conn.rx),
                conn.peer_name.clone(),
                conn.peer_identity,
                conn.read_shutdown,
            ),
            StreamState::Closed => {
                return Ok(ReceiveResult {
                    length: 0,
                    sender: None,
                    sender_identity: None,
                    handles: Vec::new(),
                    data_truncated: false,
                });
            }
            _ => return Err(SocketError::ConnectionMissing),
        }
    };
    if read_shutdown {
        return Ok(ReceiveResult {
            length: 0,
            sender: peer_name,
            sender_identity: None,
            handles: Vec::new(),
            data_truncated: false,
        });
    }

    let mut out_handles = Vec::new();
    let mut copied = 0usize;
    let mut control_identity = None;
    let mut chunk_sender_identity = peer_identity;

    loop {
        let mut queue = rx.state.lock();
        if queue.chunks.is_empty() {
            if queue.write_closed {
                break;
            }
            if copied != 0 && (!options.wait_all || options.peek) {
                break;
            }
            if options.nonblocking {
                if copied != 0 {
                    break;
                }
                return Err(SocketError::TemporaryUnavailable);
            }
            drop(queue);
            match wait_while(
                &rx.read_wait,
                || {
                    let queue = rx.state.lock();
                    queue.chunks.is_empty() && !queue.write_closed
                },
                options.deadline_ns,
            ) {
                Ok(()) => continue,
                Err(SocketError::Interrupted | SocketError::TemporaryUnavailable) if copied != 0 => break,
                Err(err) => return Err(err),
            }
        }

        while let Some(front) = queue.chunks.front_mut() {
            let remaining = front.remaining();
            if remaining == 0 {
                if copied == 0 {
                    out_handles = front.handles.to_vec();
                    control_identity = front.control_identity;
                    chunk_sender_identity = front.sender_identity;
                    if !options.peek {
                        queue.chunks.pop_front();
                    }
                }
                break;
            }
            if copied == buffer.len() {
                break;
            }
            let take = (buffer.len() - copied).min(remaining);
            buffer[copied..copied + take]
                .copy_from_slice(&front.bytes[front.offset..front.offset + take]);
            if copied == 0 && !front.handles.is_empty() {
                out_handles = front.handles.to_vec();
            }
            if copied == 0 {
                control_identity = front.control_identity;
                chunk_sender_identity = front.sender_identity;
            }
            copied += take;
            if options.peek {
                if copied == buffer.len() || take < remaining || take == remaining {
                    break;
                }
            } else {
                let remove_front = {
                    front.offset += take;
                    front.handles.clear();
                    front.offset == front.bytes.len()
                };
                queue.bytes = queue.bytes.saturating_sub(take);
                if remove_front {
                    queue.chunks.pop_front();
                }
                if copied == buffer.len() || take < remaining {
                    break;
                }
            }
        }
        drop(queue);
        if !options.peek && copied != 0 {
            rx.write_wait.wake_all_with(wake_task);
        }
        if copied == 0 || copied == buffer.len() || !options.wait_all || options.peek {
            break;
        }
    }

    let sender_identity = control_identity.or_else(|| {
        if passcred && (copied != 0 || !out_handles.is_empty()) {
            Some(chunk_sender_identity)
        } else {
            None
        }
    });
    Ok(ReceiveResult {
        length: copied,
        sender: peer_name,
        sender_identity,
        handles: out_handles,
        data_truncated: false,
    })
}

/// Datagram 发送:将整条消息投递到目标套接字的接收队列。
pub(crate) fn send_datagram(
    sender: &Arc<SocketInner>,
    dgram: &DatagramSocket,
    data: &[u8],
    handles: &[SharedHandle],
    sender_identity: PeerIdentity,
    target: Option<UnixAddress>,
    options: SendOptions,
) -> Result<usize, SocketError> {
    if data.len() > MESSAGE_BUFFER_LIMIT {
        return Err(SocketError::PayloadTooLarge);
    }
    let destination = if let Some(target) = target {
        target
    } else {
        let state = dgram.state.lock();
        state
            .connected
            .as_ref()
            .map(DatagramPeer::address)
            .ok_or(SocketError::DestinationRequired)?
    };
    let target_socket = match destination.binding_key() {
        Some(key) => crate::state::registry_lookup(&key).ok_or(SocketError::ConnectionRejected)?,
        None => {
            let state = dgram.state.lock();
            let Some(DatagramPeer::Bound { target, .. }) = &state.connected else {
                return Err(SocketError::DestinationRequired);
            };
            let Some(inner) = target.upgrade() else {
                return Err(SocketError::ConnectionRejected);
            };
            Socket { inner }
        }
    };
    let SocketKind::Datagram(target_dgram) = &target_socket.inner.kind_impl else {
        return Err(SocketError::StateMismatch);
    };
    loop {
        let mut state = target_dgram.state.lock();
        if state.read_shutdown {
            return Err(SocketError::PeerClosed);
        }
        let can_queue = state.queued_bytes + data.len() <= state.queue_limit_bytes
            && state.queue.len() < MESSAGE_PACKET_LIMIT;
        if can_queue {
            state.queued_bytes += data.len();
            state.queue.push_back(MessagePacket {
                bytes: data.to_vec(),
                handles: handles.iter().cloned().collect(),
                sender: sender.local_name.lock().clone(),
                sender_identity,
                control_identity: options.explicit_credentials.then_some(sender_identity),
            });
            drop(state);
            target_dgram.read_wait.wake_one_with(wake_task);
            return Ok(data.len());
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(state);
        wait_while(
            &target_dgram.write_wait,
            || {
                let state = target_dgram.state.lock();
                !state.read_shutdown
                    && (state.queued_bytes + data.len() > state.queue_limit_bytes
                        || state.queue.len() >= MESSAGE_PACKET_LIMIT)
            },
            options.deadline_ns,
        )?;
    }
}

/// Datagram 接收:取出队列头部的一条消息。
pub(crate) fn recv_datagram(
    dgram: &DatagramSocket,
    buffer: &mut [u8],
    options: ReceiveOptions,
    passcred: bool,
) -> Result<ReceiveResult, SocketError> {
    loop {
        let mut state = dgram.state.lock();
        if let Some(packet) = state.queue.front() {
            let copy_len = buffer.len().min(packet.bytes.len());
            if copy_len != 0 {
                buffer[..copy_len].copy_from_slice(&packet.bytes[..copy_len]);
            }
            let sender_identity = packet.control_identity.or_else(|| {
                if passcred {
                    Some(packet.sender_identity)
                } else {
                    None
                }
            });
            let sender = packet.sender.clone();
            let handles = packet.handles.to_vec();
            let data_truncated = copy_len < packet.bytes.len();
            if !options.peek {
                let packet = state.queue.pop_front().unwrap();
                state.queued_bytes = state.queued_bytes.saturating_sub(packet.bytes.len());
            }
            drop(state);
            if !options.peek {
                dgram.write_wait.wake_all_with(wake_task);
            }
            return Ok(ReceiveResult {
                length: copy_len,
                sender,
                sender_identity,
                handles,
                data_truncated,
            });
        }
        if state.read_shutdown {
            return Ok(ReceiveResult {
                length: 0,
                sender: None,
                sender_identity: None,
                handles: Vec::new(),
                data_truncated: false,
            });
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(state);
        wait_while(
            &dgram.read_wait,
            || {
                let state = dgram.state.lock();
                state.queue.is_empty() && !state.read_shutdown
            },
            options.deadline_ns,
        )?;
    }
}

/// Sequenced 发送:将整条消息写入对端接收队列。
pub(crate) fn send_seqpacket(
    seq: &SequencedSocket,
    data: &[u8],
    handles: &[SharedHandle],
    sender_identity: PeerIdentity,
    options: SendOptions,
) -> Result<usize, SocketError> {
    if data.len() > MESSAGE_BUFFER_LIMIT {
        return Err(SocketError::PayloadTooLarge);
    }
    loop {
        let (tx, local_name, write_shutdown) = {
            let state = seq.state.lock();
            match &*state {
                SequencedState::Connected(conn) => (
                    Arc::clone(&conn.tx),
                    conn.local_name.clone(),
                    conn.write_shutdown,
                ),
                SequencedState::Closed => return Err(SocketError::PeerClosed),
                _ => return Err(SocketError::ConnectionMissing),
            }
        };
        if write_shutdown {
            return Err(SocketError::PeerClosed);
        }
        let mut queue = tx.state.lock();
        if queue.read_closed {
            return Err(SocketError::PeerClosed);
        }
        let can_queue = queue.bytes + data.len() <= queue.limit_bytes
            && queue.packets.len() < MESSAGE_PACKET_LIMIT;
        if can_queue {
            queue.bytes += data.len();
            queue.packets.push_back(MessagePacket {
                bytes: data.to_vec(),
                handles: handles.iter().cloned().collect(),
                sender: local_name,
                sender_identity,
                control_identity: options.explicit_credentials.then_some(sender_identity),
            });
            drop(queue);
            tx.read_wait.wake_one_with(wake_task);
            return Ok(data.len());
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(queue);
        wait_while(
            &tx.write_wait,
            || {
                let queue = tx.state.lock();
                !queue.read_closed
                    && (queue.bytes + data.len() > queue.limit_bytes
                        || queue.packets.len() >= MESSAGE_PACKET_LIMIT)
            },
            options.deadline_ns,
        )?;
    }
}

/// Sequenced 接收:取出对端发来的一条消息。
pub(crate) fn recv_seqpacket(
    seq: &SequencedSocket,
    buffer: &mut [u8],
    options: ReceiveOptions,
    passcred: bool,
) -> Result<ReceiveResult, SocketError> {
    loop {
        let (rx, peer_name, read_shutdown) = {
            let state = seq.state.lock();
            match &*state {
                SequencedState::Connected(conn) => (
                    Arc::clone(&conn.rx),
                    conn.peer_name.clone(),
                    conn.read_shutdown,
                ),
                SequencedState::Closed => {
                    return Ok(ReceiveResult {
                        length: 0,
                        sender: None,
                        sender_identity: None,
                        handles: Vec::new(),
                        data_truncated: false,
                    });
                }
                _ => return Err(SocketError::ConnectionMissing),
            }
        };
        if read_shutdown {
            return Ok(ReceiveResult {
                length: 0,
                sender: peer_name,
                sender_identity: None,
                handles: Vec::new(),
                data_truncated: false,
            });
        }

        let mut queue = rx.state.lock();
        if let Some(packet) = queue.packets.front() {
            let copy_len = buffer.len().min(packet.bytes.len());
            if copy_len != 0 {
                buffer[..copy_len].copy_from_slice(&packet.bytes[..copy_len]);
            }
            let sender_identity = packet.control_identity.or_else(|| {
                if passcred {
                    Some(packet.sender_identity)
                } else {
                    None
                }
            });
            let sender = packet.sender.clone();
            let handles = packet.handles.to_vec();
            let data_truncated = copy_len < packet.bytes.len();
            if !options.peek {
                let packet = queue.packets.pop_front().unwrap();
                queue.bytes = queue.bytes.saturating_sub(packet.bytes.len());
            }
            drop(queue);
            if !options.peek {
                rx.write_wait.wake_all_with(wake_task);
            }
            return Ok(ReceiveResult {
                length: copy_len,
                sender,
                sender_identity,
                handles,
                data_truncated,
            });
        }
        if queue.write_closed {
            return Ok(ReceiveResult {
                length: 0,
                sender: peer_name,
                sender_identity: None,
                handles: Vec::new(),
                data_truncated: false,
            });
        }
        if options.nonblocking {
            return Err(SocketError::TemporaryUnavailable);
        }
        drop(queue);
        wait_while(
            &rx.read_wait,
            || {
                let queue = rx.state.lock();
                queue.packets.is_empty() && !queue.write_closed
            },
            options.deadline_ns,
        )?;
    }
}
