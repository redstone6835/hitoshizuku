use ktest::ktest;

use crate::{Readiness, SocketError, SocketShutdown, SocketType, UnixAddress};

use super::support::{
    assert_ready_has, assert_socket_err, handle, ident, pair, recv_nb, recv_peek, send_nb, socket,
    unique_abstract,
};

#[ktest]
fn datagram_socketpair_preserves_message_boundaries() {
    let (a, b) = pair(SocketType::Datagram, 200);
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
fn datagram_truncates_and_consumes_whole_packet() {
    let (a, b) = pair(SocketType::Datagram, 201);
    let mut small = [0u8; 3];
    let mut rest = [0u8; 8];

    assert_eq!(a.send(b"abcdef", &[], None, send_nb()), Ok(6));
    let out = b.receive(&mut small, recv_nb()).expect("truncated");
    assert_eq!(out.length, 3);
    assert!(out.data_truncated);
    assert_eq!(&small, b"abc");
    assert_socket_err(
        b.receive(&mut rest, recv_nb()),
        SocketError::TemporaryUnavailable,
    );
}

#[ktest]
fn datagram_peek_keeps_packet_available() {
    let (a, b) = pair(SocketType::Datagram, 202);
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
fn datagram_bind_connect_send_and_disconnect() {
    let server = socket(SocketType::Datagram, 203);
    let client = socket(SocketType::Datagram, 204);
    let addr = unique_abstract("dgram-connect", &server);
    let mut buf = [0u8; 8];

    server.bind(addr.clone()).expect("bind");
    assert_eq!(client.connect(addr.clone(), ident(204), send_nb()), Ok(()));
    assert_eq!(client.peer_address(), Ok(addr));
    assert_eq!(client.peer_identity(), Ok(ident(203)));
    assert_eq!(client.send(b"ping", &[], None, send_nb()), Ok(4));
    let out = server.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.length, 4);
    assert_eq!(&buf[..4], b"ping");

    assert_eq!(client.connect(UnixAddress::Unnamed, ident(204), send_nb()), Ok(()));
    assert_eq!(
        client.send(b"again", &[], None, send_nb()),
        Err(SocketError::DestinationRequired)
    );
}

#[ktest]
fn datagram_sendto_uses_explicit_target_without_connect() {
    let server = socket(SocketType::Datagram, 205);
    let client = socket(SocketType::Datagram, 206);
    let addr = unique_abstract("dgram-sendto", &server);
    let mut buf = [0u8; 8];

    server.bind(addr.clone()).expect("bind");
    assert_eq!(client.send(b"pkt", &[], Some(addr), send_nb()), Ok(3));
    let out = server.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.length, 3);
    assert_eq!(&buf[..3], b"pkt");
}

#[ktest]
fn datagram_passcred_and_explicit_credentials() {
    let (a, b) = pair(SocketType::Datagram, 207);
    let mut buf = [0u8; 8];

    b.set_passcred(true);
    assert_eq!(a.send(b"cred", &[], None, send_nb()), Ok(4));
    let out = b.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.sender_identity, Some(ident(207)));

    b.set_passcred(false);
    let options = crate::SendOptions {
        nonblocking: true,
        sender_identity: Some(ident(999)),
        explicit_credentials: true,
        ..crate::SendOptions::default()
    };
    assert_eq!(a.send(b"explicit", &[], None, options), Ok(8));
    let out = b.receive(&mut buf, recv_nb()).expect("recv explicit");
    assert_eq!(out.sender_identity, Some(ident(999)));
}

#[ktest]
fn datagram_transfers_handles_with_packet() {
    let (a, b) = pair(SocketType::Datagram, 208);
    let handles = [handle(1), handle(2)];
    let mut buf = [0u8; 8];

    assert_eq!(a.send(b"h", &handles, None, send_nb()), Ok(1));
    let out = b.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.handles.len(), 2);
    assert_eq!(
        out.handles[0]
            .as_any()
            .downcast_ref::<super::support::TestHandle>()
            .unwrap()
            .tag,
        1
    );
}

#[ktest]
fn datagram_receive_buffer_limit_backpressures_sender() {
    let (a, b) = pair(SocketType::Datagram, 209);
    let mut buf = [0u8; 8];
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
fn datagram_shutdown_read_causes_peer_closed_for_senders() {
    let (a, b) = pair(SocketType::Datagram, 210);

    assert_eq!(b.shutdown(SocketShutdown::Read), Ok(()));
    assert_ready_has(&b, Readiness::READABLE);
    assert_ready_has(&b, Readiness::HANGUP);
    assert_eq!(a.send(b"x", &[], None, send_nb()), Err(SocketError::PeerClosed));
}

#[ktest]
fn datagram_oversized_payload_is_rejected() {
    let (a, _b) = pair(SocketType::Datagram, 211);
    let payload = alloc::vec![0u8; 256 * 1024 + 1];

    assert_eq!(
        a.send(&payload, &[], None, send_nb()),
        Err(SocketError::PayloadTooLarge)
    );
}
