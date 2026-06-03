use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use crate::{
    HandleIdentity, PathKey, PeerIdentity, Readiness, ReceiveOptions, SendOptions, SharedHandle,
    Socket, SocketError, SocketHandle, SocketType, UnixAddress,
};

pub fn ident(seed: u32) -> PeerIdentity {
    PeerIdentity {
        process: seed,
        user: 1000 + seed,
        group: 2000 + seed,
    }
}

pub fn socket(kind: SocketType, seed: u32) -> Socket {
    Socket::new_unix(kind, ident(seed)).expect("socket create")
}

pub fn pair(kind: SocketType, seed: u32) -> (Socket, Socket) {
    Socket::pair_unix(kind, ident(seed)).expect("socketpair")
}

pub fn send_nb() -> SendOptions {
    SendOptions {
        nonblocking: true,
        ..SendOptions::default()
    }
}

pub fn recv_nb() -> ReceiveOptions {
    ReceiveOptions {
        nonblocking: true,
        ..ReceiveOptions::default()
    }
}

pub fn recv_peek() -> ReceiveOptions {
    ReceiveOptions {
        nonblocking: true,
        peek: true,
        ..ReceiveOptions::default()
    }
}

pub fn recv_wait_all() -> ReceiveOptions {
    ReceiveOptions {
        nonblocking: true,
        wait_all: true,
        ..ReceiveOptions::default()
    }
}

pub fn unique_abstract(prefix: &str, socket: &Socket) -> UnixAddress {
    UnixAddress::Abstract(format!("{}-{}", prefix, socket.id()).into_bytes())
}

pub fn unique_path(prefix: &str, socket: &Socket) -> UnixAddress {
    UnixAddress::Path {
        key: PathKey {
            fs: 0x51_0000 + socket.id(),
            ino: 0x77_0000 + socket.id(),
        },
        display: format!("/tmp/{}-{}", prefix, socket.id()).into_bytes(),
    }
}

pub fn recv_bytes(socket: &Socket, len: usize) -> (Vec<u8>, crate::ReceiveResult) {
    let mut buf = alloc::vec![0u8; len];
    let result = socket
        .receive(&mut buf, recv_nb())
        .expect("receive should succeed");
    buf.truncate(result.length);
    (buf, result)
}

pub fn assert_ready_has(socket: &Socket, event: Readiness) {
    assert!(socket.readiness().has(event), "missing readiness event");
}

pub fn assert_ready_lacks(socket: &Socket, event: Readiness) {
    assert!(
        !socket.readiness().has(event),
        "unexpected readiness event"
    );
}

pub fn assert_socket_err<T>(result: Result<T, SocketError>, expected: SocketError) {
    assert!(
        matches!(result, Err(err) if err == expected),
        "unexpected socket result"
    );
}

#[derive(Debug)]
pub struct TestHandle {
    pub tag: u32,
    pub identity: Option<HandleIdentity>,
}

impl SocketHandle for TestHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn identity(&self) -> Option<HandleIdentity> {
        self.identity
    }
}

pub fn handle(tag: u32) -> SharedHandle {
    Arc::new(TestHandle {
        tag,
        identity: None,
    })
}

pub fn socket_handle(socket: &Socket) -> SharedHandle {
    Arc::new(TestHandle {
        tag: socket.id() as u32,
        identity: Some(HandleIdentity::Socket(socket.id())),
    })
}
