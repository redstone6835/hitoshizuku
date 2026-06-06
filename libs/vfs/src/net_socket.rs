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
use sched::{Task, TaskState, WaitQueue};
use spin::Mutex;

use crate::addr;
use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, IoctlCmd, PollEvents};

const SOCK_STREAM: u16 = 1;
const SOCK_DGRAM: u16 = 2;
const SOCK_RAW: u16 = 3;
const MSG_TRUNC_FLAG: usize = 0x0020;

const EALREADY: Errno = Errno::Other(114);
const EINPROGRESS: Errno = Errno::Other(115);

/// 暴露给 socket.rs 用于 sock_type 比较的常量。
pub const SOCK_STREAM_PUB: u16 = SOCK_STREAM;
#[allow(dead_code)]
pub const SOCK_DGRAM_PUB: u16 = SOCK_DGRAM;

#[derive(Clone, Copy, Debug, Default)]
pub struct InetSendOptions {
    pub nonblocking: bool,
    pub deadline_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InetRecvOptions {
    pub nonblocking: bool,
    pub peek: bool,
    pub wait_all: bool,
    pub trunc: bool,
    pub deadline_ns: Option<u64>,
}

pub struct InetRecvResult {
    pub len: usize,
    pub remote: Option<Endpoint>,
    pub msg_flags: usize,
}

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
    pub keepidle: u32,  // 秒
    pub keepintvl: u32, // 秒
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
            keepalive: false,
            broadcast: false,
            reuseaddr: false,
            reuseport: false,
            linger_on: false,
            linger_secs: 0,
            sndbuf: 212992,
            rcvbuf: 212992,
            passcred: false,
            priority: 0,
            mark: 0,
            timestamp: false,
            oobinline: false,
            nodelay: false,
            cork: false,
            keepidle: 7200,
            keepintvl: 75,
            keepcnt: 9,
            defer_accept: 0,
            quickack: true,
            user_timeout: 0,
            ttl: 64,
            tos: 0,
            mcast_ttl: 1,
            mcast_loop: true,
            recvttl: false,
            recvtos: false,
            pktinfo: false,
            freebind: false,
            hdrincl: false,
            v6only: false,
            hops_v6: 64,
            mcast_hops_v6: 1,
            recv_pktinfo_v6: false,
            recv_hoplimit_v6: false,
            tclass: 0,
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
        self.handle.lock().ok_or(Errno::EBADF)
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
        *self.handle.lock()
    }

    pub fn take_last_error_code(&self) -> i32 {
        let latched = self.last_error.swap(0, Ordering::Relaxed);
        if latched != 0 {
            if self.sock_type == SOCK_STREAM {
                *self.remote.lock() = None;
            }
            return latched;
        }
        if self.sock_type == SOCK_STREAM {
            if let Ok(handle) = self.get_handle() {
                let mut remote = self.remote.lock();
                if remote.is_some()
                    && matches!(net::stack().socket_state(handle), net::SocketState::Closed)
                {
                    *remote = None;
                    return Errno::ECONNREFUSED.as_i32();
                }
            }
        }
        0
    }

    fn effective_nonblock(&self, per_call_nonblock: bool) -> bool {
        self.is_nonblock() || per_call_nonblock
    }

    fn latch_error(&self, errno: Errno) {
        if errno != Errno::ESUCCESS {
            self.last_error.store(errno.as_i32(), Ordering::Relaxed);
        }
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
        let handle = match self.get_handle() {
            Ok(h) => h,
            Err(_) => return PollEvents::POLLHUP,
        };
        let mut events = PollEvents(0);
        if interest.has(PollEvents::POLLIN) {
            let is_listening_tcp = matches!(handle.socket_type(), SocketType::Tcp)
                && self.local.lock().is_some()
                && self.remote.lock().is_none();
            let readable = if is_listening_tcp {
                matches!(
                    net::stack().socket_state(handle),
                    net::SocketState::Established
                )
            } else {
                net::stack().socket_can_recv(handle)
            };
            if readable {
                events = events.with(PollEvents::POLLIN);
            }
        }
        if interest.has(PollEvents::POLLOUT) && net::stack().socket_can_send(handle) {
            events = events.with(PollEvents::POLLOUT);
        }
        events
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, _interest: PollEvents) -> bool {
        self.wait_queue.enqueue(task);
        net::stack().enqueue_socket_waiter(task);
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.wait_queue.remove(task);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn io_timeout_deadline(&self, interest: PollEvents) -> Option<u64> {
        if interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI) {
            self.recv_deadline(None, false)
        } else if interest.has(PollEvents::POLLOUT) {
            self.send_deadline(None, false)
        } else {
            None
        }
    }

    fn release(&self) {
        // 必须 take 完再调 net::stack()——否则在某些极端顺序下
        // （如 release 路径里 wake 的 task 立刻再 open 新 socket），
        // 旧 handle 的下转可能撞上新插入的同名 index。
        if let Some(handle) = self.handle.lock().take() {
            // 走"关 + 立即摘"的同步路径，**不要** soft_remove 后等
            // 下次 poll——见 NetStack::socket_close_and_remove 的注释。
            net::stack().socket_close_and_remove(handle);
        }
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        const FIONREAD: usize = 0x541B;
        const FIONBIO: usize = 0x5421;
        const SIOCATMARK: usize = 0x8905;
        match cmd.raw() {
            FIONREAD => {
                let handle = self.get_handle()?;
                let n = match handle.socket_type() {
                    SocketType::Tcp => net::stack().tcp_recv_queue(handle),
                    // TODO: UDP 应返回下一个 datagram 的长度或队列中可读字节数，
                    // 当前固定 0 会误导 ioctl(FIONREAD) 调用者。
                    SocketType::Udp => 0,
                    SocketType::Raw | SocketType::Icmp => {
                        // TODO: raw/icmp 这里只返回是否可读，未暴露实际 packet 长度。
                        if net::stack().raw_can_recv(handle) {
                            1
                        } else {
                            0
                        }
                    }
                };
                Ok(n)
            }
            FIONBIO => {
                self.nonblock.store(arg != 0, Ordering::Relaxed);
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
        let ep = addr::parse_inet_sockaddr_for_socket(sockaddr, self.family)?;
        let handle = self.get_handle()?;
        // FIXME: local 是 VFS 层缓存，TCP bind 暂不下沉到协议栈，UDP/RAW/ICMP
        // 的 getsockname 也不一定反映 smoltcp 真实 endpoint。
        *self.local.lock() = Some(ep);
        match handle.socket_type() {
            SocketType::Udp => net::stack().udp_bind(handle, ep).map_err(map_net_error),
            SocketType::Tcp => Ok(()),
            SocketType::Raw | SocketType::Icmp => Ok(()),
        }
    }

    pub fn listen(&self, _backlog: u32) -> Result<(), Errno> {
        // FIXME: backlog 参数被完全忽略；底层也没有真正的 pending accept 队列。
        let handle = self.get_handle()?;
        if handle.socket_type() != SocketType::Tcp {
            return Err(Errno::EOPNOTSUPP);
        }
        let local = self.local.lock().ok_or(Errno::EINVAL)?;
        net::stack()
            .tcp_listen(handle, local)
            .map_err(map_net_error)
    }

    pub fn accept(&self, nonblock: bool) -> Result<NetSocketFileOps, Errno> {
        let handle = self.get_handle()?;
        if handle.socket_type() != SocketType::Tcp {
            return Err(Errno::EOPNOTSUPP);
        }
        loop {
            match net::stack().tcp_accept(handle) {
                Ok((accepted_handle, new_listen_handle)) => {
                    // FIXME: listen fd 的 handle 在 accept 成功时被替换；并发
                    // accept/poll 路径必须依赖外层文件锁与调度顺序，网络层没有
                    // 独立 generation 校验。
                    *self.handle.lock() = Some(new_listen_handle);
                    let accepted =
                        Self::new(accepted_handle, self.family, self.sock_type, nonblock);
                    *accepted.local.lock() = net::stack().tcp_local_endpoint(accepted_handle);
                    *accepted.remote.lock() = net::stack().tcp_remote_endpoint(accepted_handle);
                    return Ok(accepted);
                }
                Err(NetError::WouldBlock) => {
                    if self.is_nonblock() || nonblock {
                        return Err(Errno::EAGAIN);
                    }
                    self.wait_with_deadline(None, || {
                        matches!(
                            net::stack().socket_state(handle),
                            net::SocketState::Established
                        )
                    })?;
                }
                Err(e) => return Err(map_net_error(e)),
            }
        }
    }

    pub fn connect(&self, sockaddr: &[u8], nonblocking: bool) -> Result<(), Errno> {
        let ep = addr::parse_inet_sockaddr_for_socket(sockaddr, self.family)?;
        let handle = self.get_handle()?;
        let nonblocking = self.effective_nonblock(nonblocking);
        match handle.socket_type() {
            SocketType::Tcp => {
                let had_remote = self.remote.lock().is_some();
                match net::stack().socket_state(handle) {
                    net::SocketState::Established => return Err(Errno::EISCONN),
                    net::SocketState::Connecting => {
                        if nonblocking {
                            return Err(EALREADY);
                        }
                    }
                    net::SocketState::Closed if had_remote => {
                        self.latch_error(Errno::ECONNREFUSED);
                        return Err(Errno::ECONNREFUSED);
                    }
                    _ => {
                        if let Err(err) =
                            net::stack().tcp_connect(handle, ep).map_err(map_net_error)
                        {
                            self.latch_error(err);
                            return Err(err);
                        }
                        *self.remote.lock() = Some(ep);
                        if nonblocking {
                            return Err(EINPROGRESS);
                        }
                    }
                }
                if !nonblocking {
                    let deadline = self.send_deadline(None, false);
                    loop {
                        let state = net::stack().socket_state(handle);
                        match state {
                            net::SocketState::Established => return Ok(()),
                            net::SocketState::Closed => {
                                self.latch_error(Errno::ECONNREFUSED);
                                return Err(Errno::ECONNREFUSED);
                            }
                            // FIXME: 阻塞 connect 没有独立超时或错误队列检查，
                            // 依赖全局 poll 唤醒后再次读取粗粒度 SocketState。
                            _ => self.wait_with_deadline(deadline, || {
                                matches!(
                                    net::stack().socket_state(handle),
                                    net::SocketState::Established | net::SocketState::Closed
                                )
                            })?,
                        }
                    }
                }
                Ok(())
            }
            SocketType::Udp | SocketType::Raw | SocketType::Icmp => {
                // UDP/RAW/ICMP connect 只缓存默认 remote，不改变协议栈状态。
                *self.remote.lock() = Some(ep);
                Ok(())
            }
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

    pub fn sendto(
        &self,
        data: &[u8],
        addr: Option<&[u8]>,
        options: InetSendOptions,
    ) -> Result<usize, Errno> {
        let explicit_remote = addr
            .map(|sockaddr| addr::parse_inet_sockaddr_for_socket(sockaddr, self.family))
            .transpose()?;
        self.do_send_with(data, explicit_remote, options)
    }

    pub fn recvfrom(
        &self,
        buf: &mut [u8],
        options: InetRecvOptions,
    ) -> Result<InetRecvResult, Errno> {
        let handle = self.get_handle()?;
        match handle.socket_type() {
            SocketType::Tcp => {
                let n = self.do_recv_with(buf, options)?;
                Ok(InetRecvResult {
                    len: n,
                    remote: *self.remote.lock(),
                    msg_flags: 0,
                })
            }
            SocketType::Udp => {
                let nonblocking = self.effective_nonblock(options.nonblocking);
                let deadline = self.recv_deadline(options.deadline_ns, nonblocking);
                loop {
                    let recv = if options.peek {
                        net::stack().udp_peek_from(handle, buf)
                    } else {
                        net::stack().udp_recv_from(handle, buf)
                    };
                    match recv {
                        Ok((n, remote)) => {
                            let mut msg_flags = 0usize;
                            if options.trunc && n == buf.len() && !buf.is_empty() {
                                msg_flags |= MSG_TRUNC_FLAG;
                            }
                            return Ok(InetRecvResult {
                                len: n,
                                remote: Some(remote),
                                msg_flags,
                            });
                        }
                        Err(NetError::WouldBlock) => {
                            if nonblocking {
                                return Err(Errno::EAGAIN);
                            }
                            if self.deadline_expired(deadline) {
                                return Err(Errno::EAGAIN);
                            }
                            self.wait_with_deadline(deadline, || {
                                net::stack().socket_can_recv(handle)
                            })?;
                        }
                        Err(e) => {
                            let errno = map_net_error(e);
                            self.latch_error(errno);
                            return Err(errno);
                        }
                    }
                }
            }
            SocketType::Raw | SocketType::Icmp => {
                let n = self.do_recv_with(buf, options)?;
                // FIXME: raw/icmp 接收路径丢弃来源 endpoint，recvfrom 无法返回
                // remote 地址或入接口信息。
                Ok(InetRecvResult {
                    len: n,
                    remote: None,
                    msg_flags: 0,
                })
            }
        }
    }

    pub fn set_nonblock(&self, nonblock: bool) {
        self.nonblock.store(nonblock, Ordering::Relaxed);
    }

    pub fn getsockname(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let local = *self.local.lock();
        match local {
            // FIXME: 对 TCP 已连接 socket 应优先查询协议栈真实 local endpoint；
            // 对 UDP 自动绑定端口也不能只看 VFS 缓存。
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
            // FIXME: TCP 应返回协议栈真实 peer；UDP/RAW 当前只是 connect/sendto
            // 写入的缓存值，不能代表底层连通性或路由结果。
            Some(ep) => addr::encode_inet_sockaddr(&ep, self.family, buf),
            None => Err(Errno::ENOTCONN),
        }
    }
}

// ── 内部 I/O ─────────────────────────────────────────────────────────────────

impl NetSocketFileOps {
    fn do_send(&self, data: &[u8]) -> Result<usize, Errno> {
        self.do_send_with(data, None, InetSendOptions::default())
    }

    fn do_send_with(
        &self,
        data: &[u8],
        explicit_remote: Option<Endpoint>,
        options: InetSendOptions,
    ) -> Result<usize, Errno> {
        let handle = self.get_handle()?;
        let nonblocking = self.effective_nonblock(options.nonblocking);
        let deadline = self.send_deadline(options.deadline_ns, nonblocking);
        match handle.socket_type() {
            SocketType::Tcp => loop {
                match net::stack().tcp_send(handle, data) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        self.wait_with_deadline(deadline, || net::stack().socket_can_send(handle))?;
                    }
                    Err(e) => {
                        let errno = map_net_send_error(e);
                        self.latch_error(errno);
                        return Err(errno);
                    }
                }
            },
            SocketType::Udp => {
                let remote = explicit_remote
                    .or_else(|| *self.remote.lock())
                    .ok_or(Errno::EDESTADDRREQ)?;
                // FIXME: connected UDP 由 VFS remote 缓存模拟，底层 UDP socket
                // 没有 connect 状态，也不会过滤非 peer datagram。
                self.send_udp_wait(handle, data, remote, nonblocking, deadline)
            }
            SocketType::Raw => {
                // TODO: raw send 缺少目的地址、IP_HDRINCL、MSG_DONTROUTE 等语义。
                net::stack().raw_send(handle, data, None).map_err(|e| {
                    let errno = map_net_send_error(e);
                    self.latch_error(errno);
                    errno
                })
            }
            SocketType::Icmp => {
                let remote = explicit_remote
                    .or_else(|| *self.remote.lock())
                    .ok_or(Errno::EDESTADDRREQ)?;
                net::stack()
                    .raw_send(handle, data, Some(remote))
                    .map_err(|e| {
                        let errno = map_net_send_error(e);
                        self.latch_error(errno);
                        errno
                    })
            }
        }
    }

    fn recv_deadline(&self, explicit: Option<u64>, nonblocking: bool) -> Option<u64> {
        if nonblocking {
            return None;
        }
        if explicit.is_some() {
            return explicit;
        }
        let ns = self.recv_timeout_ns.load(Ordering::Relaxed);
        if ns == 0 {
            None
        } else {
            Some(sched::now_ns_public().saturating_add(ns))
        }
    }

    fn send_deadline(&self, explicit: Option<u64>, nonblocking: bool) -> Option<u64> {
        if nonblocking {
            return None;
        }
        if explicit.is_some() {
            return explicit;
        }
        let ns = self.send_timeout_ns.load(Ordering::Relaxed);
        if ns == 0 {
            None
        } else {
            Some(sched::now_ns_public().saturating_add(ns))
        }
    }

    fn deadline_expired(&self, deadline: Option<u64>) -> bool {
        deadline.is_some_and(|dl| sched::now_ns_public() >= dl)
    }

    fn wait_with_deadline(
        &self,
        deadline: Option<u64>,
        ready: impl Fn() -> bool,
    ) -> Result<(), Errno> {
        let task = sched::current_task();
        if has_unblocked_signal(&task) {
            return Err(Errno::EINTR);
        }
        if self.deadline_expired(deadline) {
            return Err(Errno::EAGAIN);
        }
        let _ = task.cas_state(TaskState::Running, TaskState::Sleeping);
        let _ = task.cas_state(TaskState::Runnable, TaskState::Sleeping);
        self.wait_queue.enqueue(&task);
        // 同时挂到全局 socket 事件通知队列——下次 NetStack::poll() 完
        // 成后会唤醒，让任务重新检查 socket 状态。
        net::stack().enqueue_socket_waiter(&task);
        let armed = deadline
            .map(|dl| sched::register_sleep_deadline(&task, dl))
            .unwrap_or(false);
        if ready() {
            self.wait_queue.remove(&task);
            if armed {
                sched::cancel_sleep_deadline(&task);
            }
            let _ = task.cas_state(TaskState::Sleeping, TaskState::Runnable);
            return Ok(());
        }
        if self.deadline_expired(deadline) {
            self.wait_queue.remove(&task);
            if armed {
                sched::cancel_sleep_deadline(&task);
            }
            let _ = task.cas_state(TaskState::Sleeping, TaskState::Runnable);
            return Err(Errno::EAGAIN);
        }
        sched::schedule_once(0);
        self.wait_queue.remove(&task);
        if armed {
            sched::cancel_sleep_deadline(&task);
        }
        if has_unblocked_signal(&task) {
            return Err(Errno::EINTR);
        }
        if self.deadline_expired(deadline) {
            return Err(Errno::EAGAIN);
        }
        Ok(())
    }

    fn send_udp_wait(
        &self,
        handle: NetSocketHandle,
        data: &[u8],
        remote: Endpoint,
        nonblocking: bool,
        deadline: Option<u64>,
    ) -> Result<usize, Errno> {
        loop {
            match net::stack().udp_send_to(handle, data, remote) {
                Ok(n) => return Ok(n),
                Err(NetError::WouldBlock) => {
                    if nonblocking {
                        return Err(Errno::EAGAIN);
                    }
                    if self.deadline_expired(deadline) {
                        return Err(Errno::EAGAIN);
                    }
                    self.wait_with_deadline(deadline, || net::stack().socket_can_send(handle))?;
                }
                Err(e) => {
                    let errno = map_net_send_error(e);
                    self.latch_error(errno);
                    return Err(errno);
                }
            }
        }
    }

    fn do_recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.do_recv_with(buf, InetRecvOptions::default())
    }

    fn do_recv_with(&self, buf: &mut [u8], options: InetRecvOptions) -> Result<usize, Errno> {
        if self.options.lock().read_shutdown {
            return Ok(0); // EOF — 读方向已关闭
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let handle = self.get_handle()?;
        let nonblocking = self.effective_nonblock(options.nonblocking);
        let deadline = self.recv_deadline(options.deadline_ns, nonblocking);
        match handle.socket_type() {
            SocketType::Tcp => {
                let mut copied = 0usize;
                loop {
                    let target = &mut buf[copied..];
                    let recv = if options.peek {
                        net::stack().tcp_peek(handle, target)
                    } else {
                        net::stack().tcp_recv(handle, target)
                    };
                    match recv {
                        Ok(0) => return Ok(copied),
                        Ok(n) => {
                            copied += n;
                            if options.peek || !options.wait_all || copied == buf.len() {
                                return Ok(copied);
                            }
                        }
                        Err(NetError::WouldBlock) if copied != 0 => return Ok(copied),
                        Err(NetError::WouldBlock) => {
                            if nonblocking {
                                return Err(Errno::EAGAIN);
                            }
                            if self.deadline_expired(deadline) {
                                return Err(Errno::EAGAIN);
                            }
                            self.wait_with_deadline(deadline, || {
                                net::stack().socket_can_recv(handle)
                                    || matches!(
                                        net::stack().socket_state(handle),
                                        net::SocketState::Closed
                                    )
                            })?;
                        }
                        Err(e) => {
                            let errno = map_net_error(e);
                            self.latch_error(errno);
                            return Err(errno);
                        }
                    }
                }
            }
            SocketType::Udp => loop {
                let recv = if options.peek {
                    net::stack().udp_peek_from(handle, buf)
                } else {
                    net::stack().udp_recv_from(handle, buf)
                };
                match recv {
                    Ok((n, _)) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        self.wait_with_deadline(deadline, || {
                            net::stack().socket_can_recv(handle)
                        })?;
                    }
                    Err(e) => {
                        let errno = map_net_error(e);
                        self.latch_error(errno);
                        return Err(errno);
                    }
                }
            },
            SocketType::Raw | SocketType::Icmp => loop {
                if options.peek {
                    return Err(Errno::EOPNOTSUPP);
                }
                match net::stack().raw_recv(handle, buf) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        self.wait_with_deadline(deadline, || net::stack().raw_can_recv(handle))?;
                    }
                    Err(e) => {
                        let errno = map_net_error(e);
                        self.latch_error(errno);
                        return Err(errno);
                    }
                }
            },
        }
    }

    pub fn do_peek(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let handle = self.get_handle()?;
        match handle.socket_type() {
            SocketType::Tcp => net::stack().tcp_peek(handle, buf).map_err(map_net_error),
            SocketType::Udp => {
                let (n, _) = net::stack()
                    .udp_peek_from(handle, buf)
                    .map_err(map_net_error)?;
                Ok(n)
            }
            SocketType::Raw | SocketType::Icmp => Err(Errno::EOPNOTSUPP),
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
                net::stack()
                    .socket_raw(ip_ver, proto)
                    .map_err(map_net_error)?
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    Ok(NetSocketFileOps::new(
        handle,
        family,
        sock_type & 0xf,
        nonblock,
    ))
}

// ── 错误映射 ─────────────────────────────────────────────────────────────────

fn map_net_error(e: NetError) -> Errno {
    match e {
        NetError::InterfaceNotFound => Errno::ENODEV,
        NetError::InterfaceExists => Errno::EEXIST,
        NetError::LinkDown => Errno::Other(100), // ENETDOWN
        NetError::WouldBlock => Errno::EAGAIN,
        NetError::ConnectionRefused => Errno::ECONNREFUSED,
        NetError::ConnectionReset => Errno::ECONNRESET,
        NetError::Closed => Errno::ENOTCONN,
        NetError::AddressInUse => Errno::EADDRINUSE,
        NetError::TimedOut => Errno::ETIMEDOUT,
        NetError::Unreachable => Errno::Other(113), // EHOSTUNREACH
        NetError::BufferTooSmall => Errno::EMSGSIZE,
        NetError::InvalidArgument => Errno::EINVAL,
        NetError::ResourceExhausted => Errno::ENOMEM,
    }
}

fn map_net_send_error(e: NetError) -> Errno {
    match e {
        NetError::Closed => Errno::EPIPE,
        _ => map_net_error(e),
    }
}

fn errno_to_vfs(e: Errno) -> VfsError {
    match e {
        Errno::ENOENT => VfsError::NotFound,
        Errno::EINTR => VfsError::Interrupted,
        Errno::EIO => VfsError::Io,
        Errno::EAGAIN | EALREADY | EINPROGRESS => VfsError::WouldBlock,
        Errno::ENOMEM => VfsError::OutOfMemory,
        Errno::EACCES => VfsError::PermissionDenied,
        Errno::EPERM => VfsError::OperationNotPermitted,
        Errno::EBADF => VfsError::BadFileDescriptor,
        Errno::EBUSY => VfsError::DeviceBusy,
        Errno::EXDEV => VfsError::CrossDevice,
        Errno::EEXIST | Errno::EADDRINUSE => VfsError::AlreadyExists,
        Errno::ENODEV => VfsError::NoDevice,
        Errno::ENOTDIR => VfsError::NotADirectory,
        Errno::EISDIR => VfsError::IsADirectory,
        Errno::ENFILE => VfsError::TooManyOpenFilesSystem,
        Errno::EMFILE => VfsError::TooManyOpenFiles,
        Errno::EMLINK => VfsError::TooManyLinks,
        Errno::EINVAL
        | Errno::EFAULT
        | Errno::EAFNOSUPPORT
        | Errno::EADDRNOTAVAIL
        | Errno::EISCONN
        | Errno::ENOTCONN
        | Errno::EDESTADDRREQ
        | Errno::ENOTSOCK => VfsError::InvalidArgument,
        Errno::EFBIG | Errno::EMSGSIZE => VfsError::FileTooLarge,
        Errno::ENOSPC => VfsError::NoSpace,
        Errno::ESPIPE => VfsError::IllegalSeek,
        Errno::EROFS => VfsError::ReadOnlyFilesystem,
        Errno::EPIPE | Errno::ECONNRESET | Errno::ECONNREFUSED => VfsError::BrokenPipe,
        Errno::ENAMETOOLONG => VfsError::NameTooLong,
        Errno::ENOTEMPTY => VfsError::DirectoryNotEmpty,
        Errno::ELOOP => VfsError::SymlinkLoop { depth: 0, limit: 0 },
        Errno::ENOSYS | Errno::EOPNOTSUPP | Errno::ENOPROTOOPT | Errno::ENOTTY => {
            VfsError::NotSupported
        }
        Errno::ETIMEDOUT => VfsError::TimedOut,
        Errno::ERANGE | Errno::ECHILD | Errno::ENOEXEC | Errno::ESRCH | Errno::ESUCCESS => {
            VfsError::InvalidArgument
        }
        Errno::Other(_) => VfsError::Io,
    }
}

fn has_unblocked_signal(task: &Arc<Task>) -> bool {
    let blocked = task.signal.blocked_snapshot().raw();
    let pending =
        task.signal.pending_snapshot().raw() | task.shared_signal().pending_snapshot().raw();
    (pending & !blocked) != 0
}
