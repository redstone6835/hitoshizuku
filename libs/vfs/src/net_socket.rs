//! 网络 Socket 文件操作适配器。
//!
//! 本模块把 `libs/net` 的 `NetSocketHandle` 包装为 VFS 层的 `FileOps`，
//! 使用户进程可以通过标准 POSIX socket syscall 访问 TCP/UDP 网络。

use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use errno::Errno;
use net::{self, Endpoint, NetError, NetSocketHandle, SocketType};
use sched::{Task, WaitQueue};
use spin::Mutex;

use crate::addr;
use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, IoctlCmd, PollEvents};

const SOCK_STREAM: u16 = 1;
const SOCK_DGRAM: u16 = 2;

// ── NetSocketFileOps ─────────────────────────────────────────────────────────

pub struct NetSocketFileOps {
    handle: Mutex<Option<NetSocketHandle>>,
    family: u16,
    sock_type: u16,
    nonblock: AtomicBool,
    wait_queue: WaitQueue,
    local: Mutex<Option<Endpoint>>,
    remote: Mutex<Option<Endpoint>>,
    recv_timeout_ns: AtomicU64,
    send_timeout_ns: AtomicU64,
}

impl NetSocketFileOps {
    pub fn new(handle: NetSocketHandle, family: u16, sock_type: u16, nonblock: bool) -> Self {
        Self {
            handle: Mutex::new(Some(handle)),
            family,
            sock_type,
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::new(),
            local: Mutex::new(None),
            remote: Mutex::new(None),
            recv_timeout_ns: AtomicU64::new(0),
            send_timeout_ns: AtomicU64::new(0),
        }
    }

    fn get_handle(&self) -> Result<NetSocketHandle, Errno> {
        self.handle.lock().ok_or(Errno::EBADF)
    }

    fn is_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    pub fn family(&self) -> u16 {
        self.family
    }

    fn yield_wait(&self) {
        let task = sched::current_task();
        self.wait_queue.enqueue(&task);
        sched::schedule_once(sched::now_ns_public());
    }
}

// ── FileOps ──────────────────────────────────────────────────────────────────

impl FileOps for NetSocketFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.do_recv(buf).map_err(errno_to_vfs)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.do_send(buf).map_err(errno_to_vfs)
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

    fn poll(&self, interest: PollEvents) -> PollEvents {
        let handle = match *self.handle.lock() {
            Some(h) => h,
            None => return PollEvents::POLLHUP,
        };
        let mut events = PollEvents(0);
        if interest.has(PollEvents::POLLIN) && net::stack().socket_can_recv(handle) {
            events = events.with(PollEvents::POLLIN);
        }
        if interest.has(PollEvents::POLLOUT) && net::stack().socket_can_send(handle) {
            events = events.with(PollEvents::POLLOUT);
        }
        events
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, _interest: PollEvents) -> bool {
        self.wait_queue.enqueue(task);
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.wait_queue.remove(task);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn release(&self) {
        if let Some(handle) = self.handle.lock().take() {
            match handle.socket_type() {
                SocketType::Tcp => net::stack().tcp_close(handle),
                SocketType::Udp => net::stack().udp_close(handle),
            }
            net::stack().socket_remove(handle);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── 网络操作（由 socket.rs dispatch 调用）────────────────────────────────────

impl NetSocketFileOps {
    pub fn bind(&self, sockaddr: &[u8]) -> Result<(), Errno> {
        let ep = addr::parse_inet_sockaddr(sockaddr)?;
        let handle = self.get_handle()?;
        *self.local.lock() = Some(ep);
        match handle.socket_type() {
            SocketType::Udp => net::stack().udp_bind(handle, ep).map_err(map_net_error),
            SocketType::Tcp => Ok(()),
        }
    }

    pub fn listen(&self, _backlog: u32) -> Result<(), Errno> {
        let handle = self.get_handle()?;
        let local = self.local.lock().ok_or(Errno::EINVAL)?;
        net::stack().tcp_listen(handle, local).map_err(map_net_error)
    }

    pub fn accept(&self, nonblock: bool) -> Result<NetSocketFileOps, Errno> {
        let handle = self.get_handle()?;
        loop {
            match net::stack().tcp_accept(handle) {
                Ok(new_handle) => {
                    return Ok(Self::new(
                        new_handle, self.family, self.sock_type, nonblock,
                    ));
                }
                Err(NetError::WouldBlock) => {
                    if self.is_nonblock() || nonblock {
                        return Err(Errno::EAGAIN);
                    }
                    self.yield_wait();
                }
                Err(e) => return Err(map_net_error(e)),
            }
        }
    }

    pub fn connect(&self, sockaddr: &[u8]) -> Result<(), Errno> {
        let ep = addr::parse_inet_sockaddr(sockaddr)?;
        let handle = self.get_handle()?;
        *self.remote.lock() = Some(ep);
        net::stack().tcp_connect(handle, ep).map_err(map_net_error)?;
        if !self.is_nonblock() {
            loop {
                let state = net::stack().socket_state(handle);
                match state {
                    net::SocketState::Established => return Ok(()),
                    net::SocketState::Closed => return Err(Errno::ECONNREFUSED),
                    _ => self.yield_wait(),
                }
            }
        }
        Ok(())
    }

    pub fn shutdown(&self, _how: u32) -> Result<(), Errno> {
        let handle = self.get_handle()?;
        net::stack().tcp_close(handle);
        Ok(())
    }

    pub fn sendto(&self, data: &[u8], addr: Option<&[u8]>) -> Result<usize, Errno> {
        if let Some(sockaddr) = addr {
            let ep = addr::parse_inet_sockaddr(sockaddr)?;
            *self.remote.lock() = Some(ep);
        }
        self.do_send(data)
    }

    pub fn recvfrom(&self, buf: &mut [u8]) -> Result<(usize, Option<Endpoint>), Errno> {
        let handle = self.get_handle()?;
        match handle.socket_type() {
            SocketType::Tcp => {
                let n = self.do_recv(buf)?;
                Ok((n, *self.remote.lock()))
            }
            SocketType::Udp => loop {
                match net::stack().udp_recv_from(handle, buf) {
                    Ok((n, remote)) => return Ok((n, Some(remote))),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() {
                            return Err(Errno::EAGAIN);
                        }
                        self.yield_wait();
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
        }
    }

    pub fn set_nonblock(&self, nonblock: bool) {
        self.nonblock.store(nonblock, Ordering::Relaxed);
    }

    pub fn getsockname(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let local = *self.local.lock();
        match local {
            Some(ep) => addr::encode_inet_sockaddr(&ep, self.family, buf),
            None => {
                let zero = Endpoint {
                    addr: net::IpAddr::V4(net::Ipv4Addr([0, 0, 0, 0])),
                    port: 0,
                };
                addr::encode_inet_sockaddr(&zero, self.family, buf)
            }
        }
    }

    pub fn getpeername(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let remote = *self.remote.lock();
        match remote {
            Some(ep) => addr::encode_inet_sockaddr(&ep, self.family, buf),
            None => Err(Errno::ENOTCONN),
        }
    }
}

// ── 内部 I/O ─────────────────────────────────────────────────────────────────

impl NetSocketFileOps {
    fn do_send(&self, data: &[u8]) -> Result<usize, Errno> {
        let handle = self.get_handle()?;
        match handle.socket_type() {
            SocketType::Tcp => loop {
                match net::stack().tcp_send(handle, data) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() { return Err(Errno::EAGAIN); }
                        self.yield_wait();
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Udp => {
                let remote = self.remote.lock().ok_or(Errno::EDESTADDRREQ)?;
                net::stack().udp_send_to(handle, data, remote).map_err(map_net_error)
            }
        }
    }

    fn do_recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let handle = self.get_handle()?;
        match handle.socket_type() {
            SocketType::Tcp => loop {
                match net::stack().tcp_recv(handle, buf) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() { return Err(Errno::EAGAIN); }
                        self.yield_wait();
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Udp => loop {
                match net::stack().udp_recv_from(handle, buf) {
                    Ok((n, _)) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() { return Err(Errno::EAGAIN); }
                        self.yield_wait();
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
        }
    }
}

// ── 创建入口 ─────────────────────────────────────────────────────────────────

pub fn create_net_socket(
    family: u16,
    sock_type: u16,
    _protocol: u16,
    nonblock: bool,
) -> Result<NetSocketFileOps, Errno> {
    let handle = match sock_type & 0xf {
        SOCK_STREAM => net::stack().socket_tcp().map_err(map_net_error)?,
        SOCK_DGRAM => net::stack().socket_udp().map_err(map_net_error)?,
        _ => return Err(Errno::EINVAL),
    };
    Ok(NetSocketFileOps::new(handle, family, sock_type & 0xf, nonblock))
}

// ── 错误映射 ─────────────────────────────────────────────────────────────────

fn map_net_error(e: NetError) -> Errno {
    match e {
        NetError::WouldBlock => Errno::EAGAIN,
        NetError::ConnectionRefused => Errno::ECONNREFUSED,
        NetError::ConnectionReset => Errno::ECONNRESET,
        NetError::Closed => Errno::EPIPE,
        NetError::AddressInUse => Errno::EADDRINUSE,
        NetError::TimedOut => Errno::ETIMEDOUT,
        NetError::InvalidArgument => Errno::EINVAL,
        _ => Errno::EINVAL,
    }
}

fn errno_to_vfs(e: Errno) -> VfsError {
    VfsError::Io
}
