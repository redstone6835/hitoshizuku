//! 网络 Socket 文件操作适配器。
//!
//! 本模块把 `libs/net` 的 `NetSocketHandle` 包装为 VFS 层的 `FileOps`，
//! 使用户进程可以通过标准 POSIX socket syscall 访问 TCP/UDP 网络。

use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use errno::Errno;
use net::{self, Endpoint, NetError, NetSocketHandle, SocketType};
use sched::{Task, WaitQueue};
use spin::Mutex;

use crate::addr;
use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, IoctlCmd, PollEvents};

const SOCK_STREAM: u16 = 1;
const SOCK_DGRAM: u16 = 2;
const SOCK_RAW: u16 = 3;

/// 暴露给 socket.rs 用于 sock_type 比较的常量。
pub const SOCK_STREAM_PUB: u16 = SOCK_STREAM;
#[allow(dead_code)]
pub const SOCK_DGRAM_PUB: u16 = SOCK_DGRAM;

// ── Per-socket 选项存储 ─────────────────────────────────────────────────────

/// 持久化所有可由 setsockopt 设置的运行时选项。默认值与 Linux 一致。
#[derive(Debug, Clone)]
pub struct SocketOptions {
    // SOL_SOCKET
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
    pub oobinline: bool,
    // SOL_TCP
    pub nodelay: bool,
    pub cork: bool,
    pub keepidle: u32,   // 秒
    pub keepintvl: u32,  // 秒
    pub keepcnt: u32,
    pub defer_accept: u32,
    pub quickack: bool,
    pub user_timeout: u32,
    // SOL_IP
    pub ttl: u8,
    pub tos: u8,
    pub mcast_ttl: u8,
    pub mcast_loop: bool,
    pub recvttl: bool,
    pub recvtos: bool,
    pub pktinfo: bool,
    pub freebind: bool,
    pub hdrincl: bool,
    // SOL_IPV6
    pub v6only: bool,
    pub hops_v6: u8,
    pub mcast_hops_v6: u8,
    pub recv_pktinfo_v6: bool,
    pub recv_hoplimit_v6: bool,
    pub tclass: i32,
    // 半关闭状态（SHUT_RD 由上层模拟，smoltcp 没有原生支持）
    pub read_shutdown: bool,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            keepalive: false, broadcast: false, reuseaddr: false, reuseport: false,
            linger_on: false, linger_secs: 0,
            sndbuf: 212992, rcvbuf: 212992,
            passcred: false, priority: 0, mark: 0, timestamp: false, oobinline: false,
            nodelay: false, cork: false,
            keepidle: 7200, keepintvl: 75, keepcnt: 9,
            defer_accept: 0, quickack: true, user_timeout: 0,
            ttl: 64, tos: 0, mcast_ttl: 1, mcast_loop: true,
            recvttl: false, recvtos: false, pktinfo: false, freebind: false, hdrincl: false,
            v6only: false, hops_v6: 64, mcast_hops_v6: 1,
            recv_pktinfo_v6: false, recv_hoplimit_v6: false, tclass: 0,
            read_shutdown: false,
        }
    }
}

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
    /// SO_ERROR — POSIX 要求读取后清零
    last_error: AtomicI32,
    /// 持久化的 setsockopt 状态
    options: Mutex<SocketOptions>,
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
            last_error: AtomicI32::new(0),
            options: Mutex::new(SocketOptions::default()),
        }
    }

    fn get_handle(&self) -> Result<NetSocketHandle, Errno> {
        // 关键：handle 在用户关闭 fd 之前一直可用（self.handle: Mutex<Option<...>>）。
        // 这里把"用户持有的 handle"经过 listen_redirect 表重定向到"当前真
        // 正在 Listen 状态的 socket"——smoltcp 的 listen socket 一旦握手
        // 成功就被吃掉并新建一个顶替，user 侧 fd 上的 handle 仍指向老
        // index，必须查表拿到真正可用的 listen handle。
        let raw = self.handle.lock().ok_or(Errno::EBADF)?;
        Ok(net::stack().resolve_listen_handle(raw))
    }

    fn is_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    pub fn family(&self) -> u16 {
        self.family
    }

    pub fn sock_type(&self) -> u16 {
        self.sock_type
    }

    pub fn options(&self) -> &Mutex<SocketOptions> {
        &self.options
    }

    pub fn last_error(&self) -> &AtomicI32 {
        &self.last_error
    }

    pub fn recv_timeout_ns(&self) -> &AtomicU64 {
        &self.recv_timeout_ns
    }

    pub fn send_timeout_ns(&self) -> &AtomicU64 {
        &self.send_timeout_ns
    }

    pub fn get_handle_for_opts(&self) -> Option<NetSocketHandle> {
        // 与 get_handle 一样走 listen_redirect 解析——setsockopt/getsockopt
        // 路径上若不解析，操作 listen socket 时会撞上已被 smoltcp 转成
        // Established 的同 index。
        self.handle.lock().map(|h| net::stack().resolve_listen_handle(h))
    }

    fn yield_wait(&self) {
        // yield_wait 是"无超时无 deadline"的快速路径，只在
        // accept 阻塞回退时用过（read/write 走 wait_with_deadline）。
        let task = sched::current_task();
        self.wait_queue.enqueue(&task);
        // 同时挂到全局 socket 事件通知队列——下次 NetStack::poll() 完
        // 成后会唤醒，让任务重新检查 socket 状态。
        net::stack().enqueue_socket_waiter(&task);
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
        // 走 get_handle：让 listen handle 走重定向
        let handle = match self.get_handle() {
            Ok(h) => h,
            Err(_) => return PollEvents::POLLHUP,
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
        // 必须 take 完再调 net::stack()——否则在某些极端顺序下
        // （如 release 路径里 wake 的 task 立刻再 open 新 socket），
        // 旧 handle 的下转可能撞上新插入的同名 index。
        if let Some(handle) = self.handle.lock().take() {
            // 走"关 + 立即摘"的同步路径，**不要** soft_remove 后等
            // 下次 poll——见 NetStack::socket_close_and_remove 的注释。
            net::stack().socket_close_and_remove(handle);
            // 把 listen_redirect 表里以本 handle 为起点的整条链清掉。
            // 这条链存在的前提是用户曾经在本 listen fd 上成功 accept 过；
            // 即使没 accept，clear_listen_redirect 是 no-op，安全。
            net::stack().clear_listen_redirect(handle);
        }
    }

    fn ioctl(&self, cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        const FIONREAD: usize = 0x541B;
        const FIONBIO: usize = 0x5421;
        const SIOCATMARK: usize = 0x8905;
        match cmd.raw() {
            FIONREAD => {
                let handle = self.get_handle()?;
                let n = match handle.socket_type() {
                    SocketType::Tcp => net::stack().tcp_recv_queue(handle),
                    SocketType::Udp => 0,
                    SocketType::Raw | SocketType::Icmp => {
                        if net::stack().raw_can_recv(handle) { 1 } else { 0 }
                    }
                };
                Ok(n)
            }
            FIONBIO => {
                // arg 非零 → nonblock; 零 → blocking
                self.nonblock.store(_arg != 0, Ordering::Relaxed);
                Ok(0)
            }
            SIOCATMARK => {
                // TODO: TCP OOB 标记检测
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
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
            SocketType::Raw | SocketType::Icmp => Ok(()),
        }
    }

    pub fn listen(&self, _backlog: u32) -> Result<(), Errno> {
        let handle = self.get_handle()?;
        if handle.socket_type() != SocketType::Tcp {
            return Err(Errno::EOPNOTSUPP);
        }
        let local = self.local.lock().ok_or(Errno::EINVAL)?;
        net::stack().tcp_listen(handle, local).map_err(map_net_error)
    }

    pub fn accept(&self, nonblock: bool) -> Result<NetSocketFileOps, Errno> {
        let handle = self.get_handle()?;
        if handle.socket_type() != SocketType::Tcp {
            return Err(Errno::EOPNOTSUPP);
        }
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
        match handle.socket_type() {
            SocketType::Tcp => {
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
            SocketType::Udp | SocketType::Raw | SocketType::Icmp => Ok(()),
        }
    }

    pub fn shutdown(&self, how: u32) -> Result<(), Errno> {
        const SHUT_RD: u32 = 0;
        const SHUT_WR: u32 = 1;
        const SHUT_RDWR: u32 = 2;
        let handle = self.get_handle()?;
        match how {
            SHUT_RD => {
                self.options.lock().read_shutdown = true;
                self.wait_queue.wake_all();
                Ok(())
            }
            SHUT_WR => {
                if matches!(handle.socket_type(), SocketType::Tcp) {
                    net::stack().tcp_close(handle);
                }
                Ok(())
            }
            SHUT_RDWR => {
                self.options.lock().read_shutdown = true;
                if matches!(handle.socket_type(), SocketType::Tcp) {
                    net::stack().tcp_close(handle);
                }
                self.wait_queue.wake_all();
                Ok(())
            }
            _ => Err(Errno::EINVAL),
        }
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
            SocketType::Raw | SocketType::Icmp => {
                let n = self.do_recv(buf)?;
                Ok((n, None))
            }
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
        let deadline = self.send_deadline();
        match handle.socket_type() {
            SocketType::Tcp => loop {
                match net::stack().tcp_send(handle, data) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() { return Err(Errno::EAGAIN); }
                        if self.deadline_expired(deadline) { return Err(Errno::EAGAIN); }
                        self.wait_with_deadline(deadline);
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Udp => {
                let remote = self.remote.lock().ok_or(Errno::EDESTADDRREQ)?;
                net::stack().udp_send_to(handle, data, remote).map_err(map_net_error)
            }
            SocketType::Raw => {
                net::stack().raw_send(handle, data, None).map_err(map_net_error)
            }
            SocketType::Icmp => {
                let remote = (*self.remote.lock()).ok_or(Errno::EDESTADDRREQ)?;
                net::stack().raw_send(handle, data, Some(remote)).map_err(map_net_error)
            }
        }
    }

    fn recv_deadline(&self) -> Option<u64> {
        let ns = self.recv_timeout_ns.load(Ordering::Relaxed);
        if ns == 0 { None } else { Some(sched::now_ns_public().saturating_add(ns)) }
    }

    fn send_deadline(&self) -> Option<u64> {
        let ns = self.send_timeout_ns.load(Ordering::Relaxed);
        if ns == 0 { None } else { Some(sched::now_ns_public().saturating_add(ns)) }
    }

    fn deadline_expired(&self, deadline: Option<u64>) -> bool {
        deadline.is_some_and(|dl| sched::now_ns_public() >= dl)
    }

    fn wait_with_deadline(&self, deadline: Option<u64>) {
        let task = sched::current_task();
        self.wait_queue.enqueue(&task);
        // 同时挂到全局 socket 事件通知队列——下次 NetStack::poll() 完
        // 成后会唤醒，让任务重新检查 socket 状态。
        net::stack().enqueue_socket_waiter(&task);
        let armed = deadline
            .map(|dl| sched::register_sleep_deadline(&task, dl))
            .unwrap_or(false);
        sched::schedule_once(sched::now_ns_public());
        if armed {
            sched::cancel_sleep_deadline(&task);
        }
    }

    fn do_recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.options.lock().read_shutdown {
            return Ok(0); // EOF — 读方向已关闭
        }
        let handle = self.get_handle()?;
        let deadline = self.recv_deadline();
        match handle.socket_type() {
            SocketType::Tcp => loop {
                match net::stack().tcp_recv(handle, buf) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() { return Err(Errno::EAGAIN); }
                        if self.deadline_expired(deadline) { return Err(Errno::EAGAIN); }
                        self.wait_with_deadline(deadline);
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Udp => loop {
                match net::stack().udp_recv_from(handle, buf) {
                    Ok((n, _)) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() { return Err(Errno::EAGAIN); }
                        if self.deadline_expired(deadline) { return Err(Errno::EAGAIN); }
                        self.wait_with_deadline(deadline);
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Raw | SocketType::Icmp => loop {
                match net::stack().raw_recv(handle, buf) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if self.is_nonblock() { return Err(Errno::EAGAIN); }
                        if self.deadline_expired(deadline) { return Err(Errno::EAGAIN); }
                        self.wait_with_deadline(deadline);
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
        }
    }

    pub fn do_peek(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let handle = self.get_handle()?;
        match handle.socket_type() {
            SocketType::Tcp => {
                net::stack().tcp_peek(handle, buf).map_err(map_net_error)
            }
            SocketType::Udp => {
                let (n, _) = net::stack().udp_peek_from(handle, buf).map_err(map_net_error)?;
                Ok(n)
            }
            SocketType::Raw | SocketType::Icmp => {
                Err(Errno::EOPNOTSUPP)
            }
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
        SOCK_RAW => {
            let ip_ver = if family == 10 { 6u8 } else { 4u8 };
            let proto = _protocol as u8;
            if proto == 1 {
                // IPPROTO_ICMP → 使用 ICMP socket
                net::stack().socket_icmp().map_err(map_net_error)?
            } else {
                net::stack().socket_raw(ip_ver, proto).map_err(map_net_error)?
            }
        }
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
