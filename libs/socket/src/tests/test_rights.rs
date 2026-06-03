use alloc::sync::Arc;

use ktest::ktest;

use crate::{HandleIdentity, SocketType};

use super::support::{TestHandle, handle, pair, recv_nb, send_nb, socket_handle};

#[ktest]
fn stream_transfers_empty_data_with_rights() {
    let (a, b) = pair(SocketType::Stream, 400);
    let handles = [handle(11), handle(12)];
    let mut buf = [0u8; 1];

    assert_eq!(a.send(&[], &handles, None, send_nb()), Ok(0));
    let out = b.receive(&mut buf, recv_nb()).expect("recv rights");
    assert_eq!(out.length, 0);
    assert_eq!(out.handles.len(), 2);
    assert_eq!(out.handles[0].as_any().downcast_ref::<TestHandle>().unwrap().tag, 11);
    assert_eq!(out.handles[1].as_any().downcast_ref::<TestHandle>().unwrap().tag, 12);
}

#[ktest]
fn datagram_transfers_multiple_rights() {
    let (a, b) = pair(SocketType::Datagram, 401);
    let handles = [handle(21), handle(22), handle(23)];
    let mut buf = [0u8; 4];

    assert_eq!(a.send(b"x", &handles, None, send_nb()), Ok(1));
    let out = b.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.handles.len(), 3);
    assert_eq!(out.handles[2].as_any().downcast_ref::<TestHandle>().unwrap().tag, 23);
}

#[ktest]
fn direct_socket_handle_cycle_is_detected() {
    let (a, _b) = pair(SocketType::Stream, 402);

    assert!(a.would_create_handle_cycle(HandleIdentity::Socket(a.id())));
    assert!(!a.would_create_handle_cycle(HandleIdentity::Socket(u64::MAX)));
}

#[ktest]
fn indirect_socket_handle_cycle_is_detected_through_stream_queue() {
    let (source, _source_peer) = pair(SocketType::Stream, 403);
    let (holder, holder_peer) = pair(SocketType::Stream, 404);
    let handle_to_source = [socket_handle(&source)];

    assert_eq!(holder_peer.send(b"x", &handle_to_source, None, send_nb()), Ok(1));
    assert!(source.would_create_handle_cycle(HandleIdentity::Socket(holder.id())));
}

#[ktest]
fn indirect_socket_handle_cycle_is_detected_through_datagram_queue() {
    let (source, _source_peer) = pair(SocketType::Datagram, 405);
    let (holder, holder_peer) = pair(SocketType::Datagram, 406);
    let handle_to_source = [socket_handle(&source)];

    assert_eq!(holder_peer.send(b"x", &handle_to_source, None, send_nb()), Ok(1));
    assert!(source.would_create_handle_cycle(HandleIdentity::Socket(holder.id())));
}

#[ktest]
fn non_socket_handle_identity_does_not_create_cycle() {
    let (a, b) = pair(SocketType::Stream, 407);
    let non_socket = Arc::new(TestHandle {
        tag: 99,
        identity: None,
    }) as crate::SharedHandle;

    assert_eq!(a.send(b"x", &[non_socket], None, send_nb()), Ok(1));
    assert!(!b.would_create_handle_cycle(HandleIdentity::Socket(a.id())));
}

#[ktest]
fn rights_are_removed_from_stream_chunk_after_first_consuming_read() {
    let (a, b) = pair(SocketType::Stream, 408);
    let handles = [handle(31)];
    let mut one = [0u8; 1];
    let mut rest = [0u8; 3];

    assert_eq!(a.send(b"abcd", &handles, None, send_nb()), Ok(4));
    let first = b.receive(&mut one, recv_nb()).expect("first");
    assert_eq!(first.length, 1);
    assert_eq!(first.handles.len(), 1);
    let second = b.receive(&mut rest, recv_nb()).expect("second");
    assert_eq!(second.length, 3);
    assert!(second.handles.is_empty());
}
