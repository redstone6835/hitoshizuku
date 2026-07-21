use ktest::ktest;

use crate::{PathKey, SocketError, SocketType, UnixAddress, unregister_path_socket};

use super::support::{ident, recv_nb, send_nb, socket, unique_abstract, unique_path};

#[ktest]
fn abstract_bind_rejects_duplicates_and_rebind() {
    let first = socket(SocketType::Stream, 20);
    let second = socket(SocketType::Stream, 21);
    let addr = unique_abstract("dup", &first);

    assert_eq!(first.bind(addr.clone()), Ok(()));
    assert_eq!(second.bind(addr), Err(SocketError::NameAlreadyBound));
    assert_eq!(
        first.bind(UnixAddress::Abstract(b"other".to_vec())),
        Err(SocketError::StateMismatch)
    );
}

#[ktest]
fn close_unregisters_abstract_listener() {
    let listener = socket(SocketType::Stream, 22);
    let addr = unique_abstract("close-unregister", &listener);
    listener.bind(addr.clone()).expect("bind");
    listener.listen(1).expect("listen");
    listener.close();

    let client = socket(SocketType::Stream, 23);
    assert_eq!(
        client.connect(addr, ident(23), send_nb()),
        Err(SocketError::ConnectionRejected)
    );
}

#[ktest]
fn explicit_path_unregister_allows_rebind() {
    let first = socket(SocketType::Datagram, 24);
    let second = socket(SocketType::Datagram, 25);
    let key = PathKey { fs: 101, ino: 202 };
    let addr = UnixAddress::Path {
        key: key.clone(),
        display: b"/tmp/rebind.sock".to_vec(),
    };

    assert_eq!(first.bind(addr.clone()), Ok(()));
    assert_eq!(
        second.bind(addr.clone()),
        Err(SocketError::NameAlreadyBound)
    );
    unregister_path_socket(key);
    assert_eq!(second.bind(addr), Ok(()));
}

#[ktest]
fn path_datagram_connect_uses_registry_key() {
    let server = socket(SocketType::Datagram, 26);
    let client = socket(SocketType::Datagram, 27);
    let addr = unique_path("dgram-registry", &server);
    server.bind(addr.clone()).expect("bind");

    assert_eq!(client.connect(addr.clone(), ident(27), send_nb()), Ok(()));
    assert_eq!(client.peer_address(), Ok(addr));
    assert_eq!(client.send(b"ping", &[], None, send_nb()), Ok(4));

    let mut buf = [0u8; 8];
    let out = server.receive(&mut buf, recv_nb()).expect("recv");
    assert_eq!(out.length, 4);
    assert_eq!(&buf[..4], b"ping");
}

#[ktest]
fn wrong_socket_type_connects_fail() {
    let stream_listener = socket(SocketType::Stream, 28);
    let dgram = socket(SocketType::Datagram, 29);
    let addr = unique_abstract("wrong-type", &stream_listener);
    stream_listener.bind(addr.clone()).expect("bind");
    stream_listener.listen(1).expect("listen");

    assert_eq!(
        dgram.connect(addr, ident(29), send_nb()),
        Err(SocketError::StateMismatch)
    );
}

#[ktest]
fn name_length_limit_is_enforced() {
    let sock = socket(SocketType::Stream, 30);
    let too_long = UnixAddress::Abstract(alloc::vec![b'x'; 108]);

    assert_eq!(sock.bind(too_long), Err(SocketError::NameTooLong));
}
