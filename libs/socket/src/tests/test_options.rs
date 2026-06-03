use ktest::ktest;

use crate::{SocketError, SocketLinger, SocketTimeval, SocketType};

use super::support::{ident, pair, socket};

#[ktest]
fn default_buffer_sizes_match_socket_kind() {
    let stream = socket(SocketType::Stream, 10);
    let dgram = socket(SocketType::Datagram, 11);
    let seq = socket(SocketType::Sequenced, 12);

    assert_eq!(stream.send_buffer_size(), 64 * 1024);
    assert_eq!(stream.recv_buffer_size(), 64 * 1024);
    assert_eq!(dgram.send_buffer_size(), 256 * 1024);
    assert_eq!(dgram.recv_buffer_size(), 256 * 1024);
    assert_eq!(seq.send_buffer_size(), 256 * 1024);
    assert_eq!(seq.recv_buffer_size(), 256 * 1024);
}

#[ktest]
fn options_setters_are_observable() {
    let sock = socket(SocketType::Stream, 13);
    let linger = SocketLinger {
        enabled: true,
        seconds: 4,
    };
    let recv_timeout = SocketTimeval { secs: 1, micros: 2 };
    let send_timeout = SocketTimeval { secs: 3, micros: 4 };

    sock.set_send_buffer_size(0);
    sock.set_recv_buffer_size(0);
    sock.set_reuse_addr(true);
    sock.set_reuse_port(true);
    sock.set_passcred(true);
    sock.set_linger(linger);
    sock.set_recv_timeout(Some(recv_timeout));
    sock.set_send_timeout(Some(send_timeout));

    assert_eq!(sock.send_buffer_size(), 1);
    assert_eq!(sock.recv_buffer_size(), 1);
    assert!(sock.reuse_addr());
    assert!(sock.reuse_port());
    assert!(sock.passcred_enabled());
    assert_eq!(sock.linger(), linger);
    assert_eq!(sock.recv_timeout(), Some(recv_timeout));
    assert_eq!(sock.send_timeout(), Some(send_timeout));
}

#[ktest]
fn owner_identity_is_stable() {
    let owner = ident(14);
    let sock = crate::Socket::new_unix(SocketType::Stream, owner).expect("socket");

    assert_eq!(sock.owner_identity(), owner);
}

#[ktest]
fn last_error_latches_and_clears_only_latchable_errors() {
    let sock = socket(SocketType::Stream, 15);

    assert_eq!(
        sock.connect(
            crate::UnixAddress::Unnamed,
            ident(16),
            super::support::send_nb()
        ),
        Err(SocketError::InvalidInput)
    );
    assert_eq!(sock.take_last_error(), None);

    assert_eq!(
        sock.connect(
            crate::UnixAddress::Abstract(b"missing-last-error".to_vec()),
            ident(16),
            super::support::send_nb()
        ),
        Err(SocketError::ConnectionRejected)
    );
    assert_eq!(
        sock.take_last_error(),
        Some(SocketError::ConnectionRejected)
    );
    assert_eq!(sock.take_last_error(), None);
}

#[ktest]
fn recv_buffer_resize_updates_connected_stream_capacity() {
    let (a, b) = pair(SocketType::Stream, 17);
    b.set_recv_buffer_size(3);

    assert_eq!(a.send(b"abc", &[], None, super::support::send_nb()), Ok(3));
    assert_eq!(
        a.send(b"d", &[], None, super::support::send_nb()),
        Err(SocketError::TemporaryUnavailable)
    );
}
