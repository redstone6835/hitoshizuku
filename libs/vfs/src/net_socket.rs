//! 尚未启用 INET 协议实现时的 socket 边界。
//!
//! AF_INET/AF_INET6 在创建点固定返回 `EAFNOSUPPORT`。本文件只保留通用
//! socket syscall 编译所需的数据结构，不能持有协议 handle、waiter 或网络状态。

use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use errno::Errno;
use sched::Task;
use spin::Mutex;

use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};

const SOCK_STREAM: u16 = 1;
const SOCK_DGRAM: u16 = 2;

static NET_IOCTL_HANDLER: Mutex<Option<fn(u32, usize) -> Result<usize, Errno>>> = Mutex::new(None);

pub fn install_net_ioctl_handler(handler: fn(u32, usize) -> Result<usize, Errno>) {
    *NET_IOCTL_HANDLER.lock() = Some(handler);
}

pub const SOCK_STREAM_PUB: u16 = SOCK_STREAM;
#[allow(dead_code)]
pub const SOCK_DGRAM_PUB: u16 = SOCK_DGRAM;

#[derive(Debug, Clone, Copy)]
pub struct InetSendOptions {
    pub nonblocking: bool,
    pub deadline_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct InetRecvOptions {
    pub nonblocking: bool,
    pub peek: bool,
    pub wait_all: bool,
    pub trunc: bool,
    pub deadline_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct InetRecvResult {
    pub len: usize,
    pub remote: Option<net::Endpoint>,
    pub local: Option<net::Endpoint>,
    pub interface_id: Option<net::NetDeviceId>,
    pub hop_limit: Option<u8>,
    pub traffic_class: Option<u8>,
    pub msg_flags: usize,
}

/// 协议 socket 接入前用于编译通用 option 代码的不可达类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InetSocketType {
    Tcp,
    Udp,
    Icmp,
    Raw,
}

/// 不携带协议状态的值句柄。当前实现不会构造该类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InetSocketHandle {
    socket_type: InetSocketType,
    device: net::NetDeviceId,
}

impl InetSocketHandle {
    pub const fn socket_type(self) -> InetSocketType {
        self.socket_type
    }

    pub const fn interface_id(self) -> net::NetDeviceId {
        self.device
    }
}

#[derive(Debug, Clone)]
pub struct SocketOptions {
    pub keepalive: bool,
    pub broadcast: bool,
    pub reuseaddr: bool,
    pub reuseport: bool,
    pub linger_on: bool,
    pub linger_secs: u32,
    pub sndbuf: i32,
    pub rcvbuf: i32,
    pub passcred: bool,
    pub priority: i32,
    pub mark: u32,
    pub timestamp: bool,
    pub dontroute: bool,
    pub rxq_ovfl: bool,
    pub oobinline: bool,
    pub nodelay: bool,
    pub cork: bool,
    pub keepidle: u32,
    pub keepintvl: u32,
    pub keepcnt: u32,
    pub defer_accept: u32,
    pub quickack: bool,
    pub user_timeout: u32,
    pub ttl: u8,
    pub tos: u8,
    pub mcast_ttl: u8,
    pub mcast_loop: bool,
    pub recvttl: bool,
    pub recvtos: bool,
    pub recverr: bool,
    pub pktinfo: bool,
    pub freebind: bool,
    pub hdrincl: bool,
    pub v6only: bool,
    pub hops_v6: u8,
    pub mcast_hops_v6: u8,
    pub recv_pktinfo_v6: bool,
    pub recv_hoplimit_v6: bool,
    pub recverr_v6: bool,
    pub tclass: i32,
    pub read_shutdown: bool,
    pub write_shutdown: bool,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            keepalive: false,
            broadcast: false,
            reuseaddr: false,
            reuseport: false,
            linger_on: false,
            linger_secs: 0,
            sndbuf: 212_992,
            rcvbuf: 212_992,
            passcred: false,
            priority: 0,
            mark: 0,
            timestamp: false,
            dontroute: false,
            rxq_ovfl: false,
            oobinline: false,
            nodelay: false,
            cork: false,
            keepidle: 7200,
            keepintvl: 75,
            keepcnt: 9,
            defer_accept: 0,
            quickack: false,
            user_timeout: 0,
            ttl: 64,
            tos: 0,
            mcast_ttl: 1,
            mcast_loop: true,
            recvttl: false,
            recvtos: false,
            recverr: false,
            pktinfo: false,
            freebind: false,
            hdrincl: false,
            v6only: false,
            hops_v6: 64,
            mcast_hops_v6: 1,
            recv_pktinfo_v6: false,
            recv_hoplimit_v6: false,
            recverr_v6: false,
            tclass: 0,
            read_shutdown: false,
            write_shutdown: false,
        }
    }
}

/// 不可达的 INET 文件操作对象，用于尚未安装协议实现时的 syscall 分派。
pub struct NetSocketFileOps {
    family: u16,
    sock_type: u16,
    nonblock: AtomicBool,
    recv_timeout_ns: AtomicU64,
    send_timeout_ns: AtomicU64,
    last_error: AtomicI32,
    options: Mutex<SocketOptions>,
}

impl NetSocketFileOps {
    pub fn family(&self) -> u16 {
        self.family
    }

    pub fn sock_type(&self) -> u16 {
        self.sock_type
    }

    pub fn options(&self) -> &Mutex<SocketOptions> {
        &self.options
    }

    pub fn take_last_error_code(&self) -> i32 {
        self.last_error.swap(0, Ordering::AcqRel)
    }

    pub fn recv_timeout_ns(&self) -> &AtomicU64 {
        &self.recv_timeout_ns
    }

    pub fn send_timeout_ns(&self) -> &AtomicU64 {
        &self.send_timeout_ns
    }

    pub fn get_handle_for_opts(&self) -> Option<InetSocketHandle> {
        None
    }

    pub fn bind(&self, _sockaddr: &[u8]) -> Result<(), Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn listen(&self, _backlog: u32) -> Result<(), Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn accept(&self, _nonblock: bool) -> Result<Self, Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn connect(&self, _sockaddr: &[u8], _nonblocking: bool) -> Result<(), Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn shutdown(&self, _how: u32) -> Result<(), Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn sendto(
        &self,
        _data: &[u8],
        _sockaddr: Option<&[u8]>,
        _opts: InetSendOptions,
    ) -> Result<usize, Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn recvfrom(
        &self,
        _buf: &mut [u8],
        _opts: InetRecvOptions,
    ) -> Result<InetRecvResult, Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn getsockname(&self, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno::EAFNOSUPPORT)
    }

    pub fn getpeername(&self, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno::EAFNOSUPPORT)
    }
}

impl FileOps for NetSocketFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::Io)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::Io)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents::POLLHUP
    }

    fn poll_add_waiter(&self, _task: &Arc<Task>, _interest: PollEvents) -> bool {
        false
    }

    fn poll_remove_waiter(&self, _task: &Arc<Task>) {}

    fn is_seekable(&self) -> bool {
        false
    }

    fn set_status_flags(&self, flags: OpenOptions) {
        self.nonblock.store(flags.nonblock, Ordering::Relaxed);
    }

    fn release(&self) {}

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        let Some(handler) = *NET_IOCTL_HANDLER.lock() else {
            return Err(Errno::ENOTTY);
        };
        handler(cmd.raw() as u32, arg)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn create_net_socket(
    _family: u16,
    _sock_type: u16,
    _protocol: u16,
    _nonblock: bool,
) -> Result<NetSocketFileOps, Errno> {
    Err(Errno::EAFNOSUPPORT)
}
