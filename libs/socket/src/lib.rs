//! Unix 套接字实现。
//!
//! 支持三种套接字类型:
//! - Stream (SOCK_STREAM): 面向连接的字节流,保证按序交付
//! - Datagram (SOCK_DGRAM): 无连接的消息报文,保留消息边界
//! - Sequenced (SOCK_SEQPACKET): 面向连接的消息报文,兼具可靠性和消息边界
//!
//! 所有通信均发生在内核内存中(无需网络栈),通过命名绑定或 socketpair 建立连接。

#![no_std]

extern crate alloc;

mod connection;
mod io;
mod state;
mod types;
mod wait;

pub use state::{Socket, snapshot_sockets, unregister_path_socket};
pub use types::{
    HandleIdentity, PathKey, PeerIdentity, Readiness, ReceiveOptions, ReceiveResult, SendOptions,
    SharedHandle, SocketError, SocketHandle, SocketLinger, SocketShutdown, SocketTimeval,
    SocketType, UnixAddress,
};
pub use wait::SocketReadinessObserver;

#[cfg(test)]
mod tests;
