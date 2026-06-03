use ktest::ktest;

use crate::{Readiness, SocketError, SocketShutdown, SocketType};

use super::support::{
    assert_ready_has, assert_socket_err, handle, ident, pair, recv_nb, recv_peek, send_nb, socket,
    unique_abstract,
};

#[ktest]
fn seqpacket_socketpair_preserves_records() {
    let (a, b) = pair(SocketType::Sequenced, 300);
    let mut buf = [0u8; 16];

    assert_eq!(a.send(b"one", &[], None, send_nb()), Ok(3));
    assert_eq!(a.send(b"two-two", &[], None, send_nb()), Ok(7));

    let first = b.receive(&mut buf, recv_nb()).expect("first");
    assert_eq!(first.length, 3);
    assert_eq!(&buf[..3], b"one");
    let second = b.receive(&mut buf, recv_nb()).expect("second");
    assert_eq!(second.length, 7);
    assert_eq!(&buf[..7], b"two-two");
}

#[ktest]
fn seqpacket_truncation_consumes_record() {
    let (a, b) = pair(SocketType::Sequenced, 301);
    let mut small = [0u8; 2];
    let mut rest = [0u8; 8];

    assert_eq!(a.send(b"abcdef", &[], None, send_nb()), Ok(6));
    let out = b.receive(&mut small, recv_nb()).expect("truncated");
    assert_eq!(out.length, 2);
    assert!(out.data_truncated);
    assert_eq!(&small, b"ab");
    assert_socket_err(
        b.receive(&mut rest, recv_nb()),
        SocketError::TemporaryUnavailable,
    );
}

#[ktest]
fn seqpacket_peek_does_not_consume_record() {
    let (a, b) = pair(SocketType::Sequenced, 302);
    let mut buf = [0u8; 8];

    assert_eq!(a.send(b"peek", &[], None, send_nb()), Ok(4));
    let peeked = b.receive(&mut buf, recv_peek()).expect("peek");
    assert_eq!(peeked.length, 4);
    assert_eq!(&buf[..4], b"peek");
    let actual = b.receive(&mut buf, recv_nb()).expect("actual");
    assert_eq!(actual.length, 4);
    assert_eq!(&buf[..4], b"peek");
}

#[ktest]
fn seqpacket_listen_connect_accept_roundtrip() {
    let listener = socket(SocketType::Sequenced, 303);
    let client = socket(SocketType::Sequenced, 304);
    let addr = unique_abstract("seq-listen", &listener);
    let mut buf = [0u8; 16];

    listener.bind(addr.clone()).expect("bind");
    listener.listen(1).expect("listen");
    assert_eq!(client.connect(addr.clone(), ident(304), send_nb()), Ok(()));
    let accepted = listener.accept(recv_nb()).expect("accept");

    assert_eq!(client.peer_address(), Ok(addr.clone()));
    assert_eq!(accepted.local_address(), addr);
    assert_eq!(accepted.peer_identity(), Ok(ident(304)));
    assert_eq!(client.send(b"client", &[], None, send_nb()), Ok(6));
    let out = accepted.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.length, 6);
    assert_eq!(&buf[..6], b"client");
}

#[ktest]
fn seqpacket_nonblocking_errors_match_state() {
    let listener = socket(SocketType::Sequenced, 305);
    let client = socket(SocketType::Sequenced, 306);
    let missing = unique_abstract("seq-missing", &listener);
    let mut buf = [0u8; 1];

    assert_eq!(
        client.connect(missing, ident(306), send_nb()),
        Err(SocketError::ConnectionRejected)
    );
    assert_socket_err(listener.accept(recv_nb()), SocketError::ListenerRequired);
    assert_socket_err(client.receive(&mut buf, recv_nb()), SocketError::ConnectionMissing);
}

#[ktest]
fn seqpacket_shutdown_write_reports_peer_rdhup_and_eof() {
    let (a, b) = pair(SocketType::Sequenced, 307);
    let mut buf = [0u8; 1];

    assert_eq!(a.shutdown(SocketShutdown::Write), Ok(()));
    assert_ready_has(&b, Readiness::READABLE);
    assert_ready_has(&b, Readiness::READ_HANGUP);
    assert_eq!(b.receive(&mut buf, recv_nb()).expect("eof").length, 0);
    assert_eq!(a.send(b"x", &[], None, send_nb()), Err(SocketError::PeerClosed));
}

#[ktest]
fn seqpacket_shutdown_read_makes_peer_write_fail() {
    let (a, b) = pair(SocketType::Sequenced, 308);

    assert_eq!(b.shutdown(SocketShutdown::Read), Ok(()));
    assert_ready_has(&a, Readiness::FAULT);
    assert_eq!(a.send(b"x", &[], None, send_nb()), Err(SocketError::PeerClosed));
}

#[ktest]
fn seqpacket_close_propagates_peer_closed() {
    let (a, b) = pair(SocketType::Sequenced, 309);
    let mut buf = [0u8; 1];

    a.close();
    assert_ready_has(&b, Readiness::HANGUP);
    assert_ready_has(&b, Readiness::READ_HANGUP);
    assert_ready_has(&b, Readiness::FAULT);
    assert_eq!(b.receive(&mut buf, recv_nb()).expect("eof").length, 0);
    assert_eq!(b.send(b"x", &[], None, send_nb()), Err(SocketError::PeerClosed));
}

#[ktest]
fn seqpacket_transfers_handles_and_credentials() {
    let (a, b) = pair(SocketType::Sequenced, 310);
    let handles = [handle(7)];
    let mut buf = [0u8; 8];
    b.set_passcred(true);

    assert_eq!(a.send(b"h", &handles, None, send_nb()), Ok(1));
    let out = b.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.length, 1);
    assert_eq!(out.handles.len(), 1);
    assert_eq!(out.sender_identity, Some(ident(310)));
}

#[ktest]
fn seqpacket_buffer_limit_backpressures_sender() {
    let (a, b) = pair(SocketType::Sequenced, 311);
    let mut buf = [0u8; 4];
    b.set_recv_buffer_size(3);

    assert_eq!(a.send(b"abc", &[], None, send_nb()), Ok(3));
    assert_eq!(
        a.send(b"d", &[], None, send_nb()),
        Err(SocketError::TemporaryUnavailable)
    );
    assert_eq!(b.receive(&mut buf, recv_nb()).expect("recv").length, 3);
    assert_eq!(a.send(b"d", &[], None, send_nb()), Ok(1));
}

#[ktest]
fn seqpacket_oversized_payload_is_rejected() {
    let (a, _b) = pair(SocketType::Sequenced, 312);
    let payload = alloc::vec![0u8; 256 * 1024 + 1];

    assert_eq!(
        a.send(&payload, &[], None, send_nb()),
        Err(SocketError::PayloadTooLarge)
    );
}
