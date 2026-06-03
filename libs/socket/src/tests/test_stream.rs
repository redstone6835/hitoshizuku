use ktest::ktest;

use crate::{Readiness, SocketError, SocketShutdown, SocketType};

use super::support::{
    assert_ready_has, assert_ready_lacks, assert_socket_err, ident, pair, recv_bytes, recv_nb,
    recv_peek, recv_wait_all, send_nb, socket, unique_abstract,
};

#[ktest]
fn stream_socketpair_transfers_ordered_bytes_bidirectionally() {
    let (a, b) = pair(SocketType::Stream, 100);

    assert_ready_has(&a, Readiness::WRITABLE);
    assert_ready_has(&b, Readiness::WRITABLE);
    assert_eq!(a.send(b"hello", &[], None, send_nb()), Ok(5));
    assert_eq!(a.send(b" world", &[], None, send_nb()), Ok(6));
    assert_ready_has(&b, Readiness::READABLE);

    let (bytes, out) = recv_bytes(&b, 11);
    assert_eq!(out.length, 11);
    assert_eq!(bytes, b"hello world");

    assert_eq!(b.send(b"reply", &[], None, send_nb()), Ok(5));
    let (reply, _) = recv_bytes(&a, 8);
    assert_eq!(reply, b"reply");
}

#[ktest]
fn stream_partial_reads_consume_only_returned_bytes() {
    let (a, b) = pair(SocketType::Stream, 101);

    assert_eq!(a.send(b"abcdef", &[], None, send_nb()), Ok(6));
    let (first, _) = recv_bytes(&b, 2);
    let (second, _) = recv_bytes(&b, 4);

    assert_eq!(first, b"ab");
    assert_eq!(second, b"cdef");
}

#[ktest]
fn stream_peek_does_not_consume() {
    let (a, b) = pair(SocketType::Stream, 102);
    let mut buf = [0u8; 4];

    assert_eq!(a.send(b"peek", &[], None, send_nb()), Ok(4));
    let peeked = b.receive(&mut buf, recv_peek()).expect("peek");
    assert_eq!(peeked.length, 4);
    assert_eq!(&buf, b"peek");

    let (actual, _) = recv_bytes(&b, 4);
    assert_eq!(actual, b"peek");
}

#[ktest]
fn stream_waitall_nonblocking_returns_available_prefix() {
    let (a, b) = pair(SocketType::Stream, 103);
    let mut buf = [0u8; 8];

    assert_eq!(a.send(b"abc", &[], None, send_nb()), Ok(3));
    let out = b.receive(&mut buf, recv_wait_all()).expect("recv");

    assert_eq!(out.length, 3);
    assert_eq!(&buf[..3], b"abc");
}

#[ktest]
fn stream_empty_nonblocking_recv_returns_eagain() {
    let (_a, b) = pair(SocketType::Stream, 104);
    let mut buf = [0u8; 1];

    assert_socket_err(
        b.receive(&mut buf, recv_nb()),
        SocketError::TemporaryUnavailable,
    );
}

#[ktest]
fn stream_listen_connect_accept_roundtrip() {
    let listener = socket(SocketType::Stream, 105);
    let client = socket(SocketType::Stream, 106);
    let addr = unique_abstract("stream-listen", &listener);

    listener.bind(addr.clone()).expect("bind");
    assert_eq!(listener.listen(0), Ok(()));
    assert!(listener.is_listener());
    assert_eq!(client.connect(addr.clone(), ident(106), send_nb()), Ok(()));
    assert_ready_has(&listener, Readiness::READABLE);

    let accepted = listener.accept(recv_nb()).expect("accept");
    assert_eq!(client.peer_address(), Ok(addr.clone()));
    assert_eq!(accepted.peer_identity(), Ok(ident(106)));
    assert_eq!(accepted.local_address(), addr);

    assert_eq!(client.send(b"client", &[], None, send_nb()), Ok(6));
    let (bytes, _) = recv_bytes(&accepted, 8);
    assert_eq!(bytes, b"client");
    assert_eq!(accepted.send(b"server", &[], None, send_nb()), Ok(6));
    let (reply, _) = recv_bytes(&client, 8);
    assert_eq!(reply, b"server");
}

#[ktest]
fn stream_accept_without_pending_is_nonblocking_eagain() {
    let listener = socket(SocketType::Stream, 107);
    listener.listen(1).expect("listen");

    assert_socket_err(listener.accept(recv_nb()), SocketError::TemporaryUnavailable);
}

#[ktest]
fn stream_backlog_full_rejects_nonblocking_connect_until_accept() {
    let listener = socket(SocketType::Stream, 108);
    let first = socket(SocketType::Stream, 109);
    let second = socket(SocketType::Stream, 110);
    let addr = unique_abstract("stream-backlog", &listener);

    listener.bind(addr.clone()).expect("bind");
    listener.listen(1).expect("listen");
    assert_eq!(first.connect(addr.clone(), ident(109), send_nb()), Ok(()));
    assert_eq!(
        second.connect(addr.clone(), ident(110), send_nb()),
        Err(SocketError::TemporaryUnavailable)
    );
    let _accepted = listener.accept(recv_nb()).expect("accept");
    assert_eq!(second.connect(addr, ident(110), send_nb()), Ok(()));
}

#[ktest]
fn stream_shutdown_write_reports_peer_rdhup_and_eof() {
    let (a, b) = pair(SocketType::Stream, 111);
    let mut buf = [0u8; 1];

    assert_eq!(a.shutdown(SocketShutdown::Write), Ok(()));
    assert_ready_has(&b, Readiness::READABLE);
    assert_ready_has(&b, Readiness::HANGUP);
    assert_ready_has(&b, Readiness::READ_HANGUP);

    let out = b.receive(&mut buf, recv_nb()).expect("eof");
    assert_eq!(out.length, 0);
    assert_eq!(a.send(b"x", &[], None, send_nb()), Err(SocketError::PeerClosed));
}

#[ktest]
fn stream_shutdown_read_makes_peer_write_fail() {
    let (a, b) = pair(SocketType::Stream, 112);

    assert_eq!(b.shutdown(SocketShutdown::Read), Ok(()));
    assert_ready_has(&a, Readiness::HANGUP);
    assert_ready_has(&a, Readiness::FAULT);
    assert_eq!(a.send(b"x", &[], None, send_nb()), Err(SocketError::PeerClosed));
}

#[ktest]
fn stream_close_propagates_hup_fault_and_eof() {
    let (a, b) = pair(SocketType::Stream, 113);
    let mut buf = [0u8; 1];

    a.close();
    assert_ready_has(&b, Readiness::HANGUP);
    assert_ready_has(&b, Readiness::READ_HANGUP);
    assert_ready_has(&b, Readiness::FAULT);
    assert_eq!(b.receive(&mut buf, recv_nb()).expect("eof").length, 0);
    assert_eq!(b.send(b"x", &[], None, send_nb()), Err(SocketError::PeerClosed));
}

#[ktest]
fn stream_unconnected_operations_fail_cleanly() {
    let sock = socket(SocketType::Stream, 114);
    let mut buf = [0u8; 1];

    assert_ready_lacks(&sock, Readiness::READABLE);
    assert_eq!(
        sock.send(b"x", &[], None, send_nb()),
        Err(SocketError::ConnectionMissing)
    );
    assert_socket_err(sock.receive(&mut buf, recv_nb()), SocketError::ConnectionMissing);
    assert_socket_err(sock.accept(recv_nb()), SocketError::ListenerRequired);
}
