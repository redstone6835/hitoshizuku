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
use crate::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};

const SOCK_STREAM: u16 = 1;
const SOCK_DGRAM: u16 = 2;
const SOCK_RAW: u16 = 3;

static NET_IOCTL_HANDLER: Mutex<Option<fn(u32, usize) -> Result<usize, Errno>>> = Mutex::new(None);

pub fn install_net_ioctl_handler(handler: fn(u32, usize) -> Result<usize, Errno>) {
    *NET_IOCTL_HANDLER.lock() = Some(handler);
}

fn dispatch_registered_net_ioctl(cmd: u32, arg: usize) -> Result<usize, Errno> {
    let Some(handler) = *NET_IOCTL_HANDLER.lock() else {
        return Err(Errno::ENOTTY);
    };
    handler(cmd, arg)
}

/// 暴露给 socket.rs 用于 sock_type 比较的常量。
pub const SOCK_STREAM_PUB: u16 = SOCK_STREAM;
#[allow(dead_code)]
pub const SOCK_DGRAM_PUB: u16 = SOCK_DGRAM;

/// inet socket 发送路径的调用参数。
#[derive(Debug, Clone, Copy)]
pub struct InetSendOptions {
    pub nonblocking: bool,
    /// 绝对超时时刻（纳秒）；`None` 表示使用 socket 自身的 SO_SNDTIMEO。
    pub deadline_ns: Option<u64>,
}

/// inet socket 接收路径的调用参数。
#[derive(Debug, Clone, Copy)]
pub struct InetRecvOptions {
    pub nonblocking: bool,
    pub peek: bool,
    pub wait_all: bool,
    pub trunc: bool,
    /// 绝对超时时刻（纳秒）；`None` 表示使用 socket 自身的 SO_RCVTIMEO。
    pub deadline_ns: Option<u64>,
}

/// inet socket 接收结果，供 POSIX socket 层编码 msghdr/sockaddr。
#[derive(Debug, Clone, Copy)]
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
    pub dontroute: bool,
    pub rxq_ovfl: bool,
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
    pub recverr: bool,
    pub pktinfo: bool,
    pub freebind: bool,
    pub hdrincl: bool,
    // SOL_IPV6
    pub v6only: bool,
    pub hops_v6: u8,
    pub mcast_hops_v6: u8,
    pub recv_pktinfo_v6: bool,
    pub recv_hoplimit_v6: bool,
    pub recverr_v6: bool,
    pub tclass: i32,
    // 半关闭状态由 VFS 层补齐：smoltcp 只有 TCP 写半关闭，没有通用的
    // socket 级 SHUT_RD/SHUT_WR 标志。
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
            sndbuf: 212992,
            rcvbuf: 212992,
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
            quickack: true,
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

// ── NetSocketFileOps ─────────────────────────────────────────────────────────

pub struct NetSocketFileOps {
    handle: Mutex<Option<NetSocketHandle>>,
    family: u16,
    sock_type: u16,
    protocol: u16,
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
    pub fn new(
        handle: NetSocketHandle,
        family: u16,
        sock_type: u16,
        protocol: u16,
        nonblock: bool,
    ) -> Self {
        Self {
            handle: Mutex::new(Some(handle)),
            family,
            sock_type,
            protocol,
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

    pub fn take_last_error_code(&self) -> i32 {
        self.last_error.swap(0, Ordering::AcqRel)
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

    fn yield_wait_until<F>(&self, ready: F) -> Result<(), Errno>
    where
        F: FnMut() -> bool,
    {
        // yield_wait 是"无超时无 deadline"的快速路径，只在
        // accept 阻塞回退时用过（read/write 走 wait_with_deadline）。
        self.wait_with_deadline_until(None, ready)
    }

    fn finish_current_wait(&self, task: &Arc<Task>, armed_deadline: bool) {
        self.wait_queue.remove(task);
        if armed_deadline {
            sched::cancel_sleep_deadline(task);
        }
        if !task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running) {
            let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
        }
    }

    fn wait_with_deadline_until<F>(&self, deadline: Option<u64>, mut ready: F) -> Result<(), Errno>
    where
        F: FnMut() -> bool,
    {
        if ready() {
            return Ok(());
        }
        let task = sched::current_task();
        if has_unblocked_signal(&task) {
            return Err(Errno::EINTR);
        }
        if self.deadline_expired(deadline) {
            return Err(Errno::EAGAIN);
        }
        let _ = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping);
        let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);
        self.wait_queue.enqueue(&task);
        // 同时挂到全局 socket 事件通知队列——下次 NetStack::poll() 完
        // 成后会唤醒，让任务重新检查 socket 状态。
        net::stack().enqueue_socket_waiter(&task);
        let armed = deadline
            .map(|dl| sched::register_sleep_deadline(&task, dl))
            .unwrap_or(false);
        if ready() {
            self.finish_current_wait(&task, armed);
            return Ok(());
        }
        if self.deadline_expired(deadline) {
            self.finish_current_wait(&task, armed);
            return Err(Errno::EAGAIN);
        }
        sched::schedule_once(sched::now_ns_public());
        self.finish_current_wait(&task, armed);
        if has_unblocked_signal(&task) {
            return Err(Errno::EINTR);
        }
        Ok(())
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
            let local = *self.local.lock();
            let remote = *self.remote.lock();
            let is_listening_tcp = matches!(handle.socket_type(), SocketType::Tcp)
                && local.is_some()
                && remote.is_none();
            let readable = if is_listening_tcp {
                net::stack().tcp_has_pending_accept(handle, local)
            } else {
                net::stack().socket_can_recv(handle)
            };
            if readable {
                events = events.with(PollEvents::POLLIN);
            }
        }
        if interest.has(PollEvents::POLLOUT)
            && !self.options.lock().write_shutdown
            && net::stack().socket_can_send(handle)
        {
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

    fn set_status_flags(&self, flags: OpenOptions) {
        self.set_nonblock(flags.nonblock);
    }

    fn release(&self) {
        if let Some(handle) = self.handle.lock().take() {
            let linger_secs = {
                let options = self.options.lock();
                if options.linger_on
                    && options.linger_secs > 0
                    && matches!(handle.socket_type(), SocketType::Tcp)
                {
                    net::stack().tcp_close(handle);
                    options.linger_secs
                } else {
                    0
                }
            };
            if linger_secs > 0 {
                let deadline =
                    sched::now_ns_public().saturating_add((linger_secs as u64) * 1_000_000_000);
                while sched::now_ns_public() < deadline && net::stack().tcp_recv_queue(handle) > 0 {
                    sched::schedule_once(sched::now_ns_public());
                }
            }
            if matches!(handle.socket_type(), SocketType::Tcp) {
                net::stack().socket_close_detach(handle);
                // TCP_CRR 这类短连接在 close 后立即发起下一轮 connect。
                // 这里让对端马上运行一次，及时消费 FIN/EOF，避免只有额外
                // timer sleeper 存在时协议收尾才继续推进。
                sched::schedule_once(sched::now_ns_public());
            } else {
                net::stack().socket_close_and_remove(handle);
            }
        }
    }

    fn ioctl(&self, cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        const FIONREAD: usize = 0x541B;
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
            SIOCATMARK => {
                // TODO: TCP OOB 标记检测
                Ok(0)
            }
            _ => dispatch_registered_net_ioctl(cmd.raw() as u32, _arg),
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
        let handle = self.rehome_for_endpoint(self.get_handle()?, &ep)?;
        match handle.socket_type() {
            SocketType::Udp => {
                let bound = net::stack().udp_bind(handle, ep).map_err(map_net_error)?;
                *self.local.lock() = Some(bound);
                Ok(())
            }
            SocketType::Tcp => {
                // FIXME: TCP bind 暂不下沉到协议栈，listen/connect 时才应用。
                *self.local.lock() = Some(ep);
                Ok(())
            }
            SocketType::Raw | SocketType::Icmp => {
                *self.local.lock() = Some(ep);
                Ok(())
            }
        }
    }

    pub fn listen(&self, _backlog: u32) -> Result<(), Errno> {
        // FIXME: backlog 参数被完全忽略；底层也没有真正的 pending accept 队列。
        let handle = self.get_handle()?;
        if handle.socket_type() != SocketType::Tcp {
            return Err(Errno::EOPNOTSUPP);
        }
        let local = self.local.lock().ok_or(Errno::EINVAL)?;
        let bound = net::stack()
            .tcp_listen(handle, local)
            .map_err(map_net_error)?;
        *self.local.lock() = Some(bound);
        Ok(())
    }

    pub fn accept(&self, nonblock: bool) -> Result<NetSocketFileOps, Errno> {
        let handle = self.get_handle()?;
        if handle.socket_type() != SocketType::Tcp {
            return Err(Errno::EOPNOTSUPP);
        }
        loop {
            let handle = self.get_handle()?;
            let local = *self.local.lock();
            match net::stack().tcp_accept(handle, local) {
                Ok(info) => {
                    // FIXME: listen fd 的 handle 在 accept 成功时被替换；并发
                    // accept/poll 路径必须依赖外层文件锁与调度顺序，网络层没有
                    // 独立 generation 校验。
                    *self.handle.lock() = Some(info.listener);
                    let accepted = Self::new(
                        info.accepted,
                        self.family,
                        self.sock_type,
                        self.protocol,
                        nonblock,
                    );
                    // accept 交付的是已经 Established 的 TCP socket；端点在
                    // net 层同一把接口锁下取了快照，优先使用快照，避免后续
                    // 查询时连接状态变化导致 getpeername/getsockname 为空。
                    *accepted.local.lock() = info
                        .local
                        .or_else(|| net::stack().tcp_local_endpoint(info.accepted));
                    *accepted.remote.lock() = info
                        .remote
                        .or_else(|| net::stack().tcp_remote_endpoint(info.accepted));
                    return Ok(accepted);
                }
                Err(NetError::WouldBlock) => {
                    if self.is_nonblock() || nonblock {
                        return Err(Errno::EAGAIN);
                    }
                    net::stack().poll_now();
                    // FIXME: 阻塞 accept 只依赖 timer poll 后的全局唤醒，
                    // 若协议栈 poll 被节流或中断路径漏调，用户态可能长期睡眠。
                    self.yield_wait_until(|| {
                        self.get_handle().is_ok_and(|h| {
                            net::stack().tcp_has_pending_accept(h, *self.local.lock())
                        })
                    })?;
                }
                Err(e) => return Err(map_net_error(e)),
            }
        }
    }

    pub fn connect(&self, sockaddr: &[u8], nonblocking: bool) -> Result<(), Errno> {
        let ep = addr::parse_inet_sockaddr(sockaddr)?;
        let mut handle = self.get_handle()?;
        if self
            .local
            .lock()
            .is_none_or(|local| endpoint_addr_is_unspecified(&local))
        {
            handle = self.rehome_for_endpoint(handle, &ep)?;
        }
        // FIXME: remote 是 VFS 层缓存；UDP/RAW/ICMP connect 不会同步到底层
        // socket，getpeername 可能返回一个协议栈并未验证过的地址。
        *self.remote.lock() = Some(ep);
        match handle.socket_type() {
            SocketType::Tcp => {
                net::stack()
                    .tcp_connect(handle, ep)
                    .map_err(map_net_error)?;
                if !(self.is_nonblock() || nonblocking) {
                    loop {
                        let state = net::stack().socket_state(handle);
                        match state {
                            net::SocketState::Established => {
                                net::stack().poll_now();
                                sched::schedule_once(sched::now_ns_public());
                                return Ok(());
                            }
                            net::SocketState::Closed => return Err(Errno::ECONNREFUSED),
                            // FIXME: 阻塞 connect 没有独立超时或错误队列检查，
                            // 依赖全局 poll 唤醒后再次读取粗粒度 SocketState。
                            _ => self.yield_wait_until(|| {
                                matches!(
                                    net::stack().socket_state(handle),
                                    net::SocketState::Established | net::SocketState::Closed
                                )
                            })?,
                        }
                    }
                }
                // 非阻塞 connect：不等待握手完成，返回 EINPROGRESS
                Err(Errno::EINPROGRESS)
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
                self.options.lock().write_shutdown = true;
                if matches!(handle.socket_type(), SocketType::Tcp) {
                    net::stack().tcp_close(handle);
                }
                Ok(())
            }
            SHUT_RDWR => {
                let mut options = self.options.lock();
                options.read_shutdown = true;
                options.write_shutdown = true;
                drop(options);
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
        opts: InetSendOptions,
    ) -> Result<usize, Errno> {
        let remote = match addr {
            Some(sockaddr) => Some(addr::parse_inet_sockaddr(sockaddr)?),
            None => None,
        };
        if let Some(ep) = remote
            && self
                .local
                .lock()
                .is_none_or(|local| endpoint_addr_is_unspecified(&local))
        {
            let _ = self.rehome_for_endpoint(self.get_handle()?, &ep)?;
        }
        self.do_send_with(data, remote, opts)
    }

    pub fn recvfrom(&self, buf: &mut [u8], opts: InetRecvOptions) -> Result<InetRecvResult, Errno> {
        let handle = self.get_handle()?;
        match handle.socket_type() {
            SocketType::Tcp => {
                if opts.peek {
                    loop {
                        match net::stack().tcp_peek(handle, buf) {
                            Ok(n) => {
                                return Ok(InetRecvResult {
                                    len: n,
                                    remote: *self.remote.lock(),
                                    msg_flags: 0,
                                });
                            }
                            Err(NetError::WouldBlock) => {
                                if opts.nonblocking || self.is_nonblock() {
                                    return Err(Errno::EAGAIN);
                                }
                                let deadline = self.recv_wait_deadline(opts.deadline_ns);
                                if self.deadline_expired(deadline) {
                                    return Err(Errno::EAGAIN);
                                }
                                if net::stack().socket_can_recv(handle) {
                                    continue;
                                }
                                self.wait_with_deadline_until(deadline, || {
                                    net::stack().socket_can_recv(handle)
                                })?;
                            }
                            Err(e) => return Err(map_net_error(e)),
                        }
                    }
                }
                if opts.wait_all {
                    let mut total = 0;
                    loop {
                        match net::stack().tcp_recv(handle, &mut buf[total..]) {
                            Ok(0) => {
                                return Ok(InetRecvResult {
                                    len: total,
                                    remote: *self.remote.lock(),
                                    msg_flags: 0,
                                });
                            }
                            Ok(n) => {
                                total += n;
                                if total >= buf.len() {
                                    return Ok(InetRecvResult {
                                        len: total,
                                        remote: *self.remote.lock(),
                                        msg_flags: 0,
                                    });
                                }
                            }
                            Err(NetError::WouldBlock) => {
                                if opts.nonblocking || self.is_nonblock() {
                                    return if total > 0 {
                                        Ok(InetRecvResult {
                                            len: total,
                                            remote: *self.remote.lock(),
                                            msg_flags: 0,
                                        })
                                    } else {
                                        Err(Errno::EAGAIN)
                                    };
                                }
                                let deadline = self.recv_wait_deadline(opts.deadline_ns);
                                if self.deadline_expired(deadline) {
                                    return if total > 0 {
                                        Ok(InetRecvResult {
                                            len: total,
                                            remote: *self.remote.lock(),
                                            msg_flags: 0,
                                        })
                                    } else {
                                        Err(Errno::EAGAIN)
                                    };
                                }
                                if net::stack().socket_can_recv(handle) {
                                    continue;
                                }
                                self.wait_with_deadline_until(deadline, || {
                                    net::stack().socket_can_recv(handle)
                                })?;
                            }
                            Err(e) => return Err(map_net_error(e)),
                        }
                    }
                }
                let n = self.do_recv_with(buf, opts.nonblocking, opts.deadline_ns)?;
                Ok(InetRecvResult {
                    len: n,
                    remote: *self.remote.lock(),
                    msg_flags: 0,
                })
            }
            SocketType::Udp => {
                if opts.peek {
                    loop {
                        let peer = *self.remote.lock();
                        match net::stack().udp_peek_from(handle, buf) {
                            Ok((n, remote)) => {
                                if !udp_peer_matches(peer, remote) {
                                    let _ = net::stack().udp_recv_from(handle, buf);
                                    continue;
                                }
                                return Ok(InetRecvResult {
                                    len: n,
                                    remote: Some(remote),
                                    msg_flags: datagram_msg_flags(n, buf.len(), opts.trunc),
                                });
                            }
                            Err(NetError::WouldBlock) => {
                                if opts.nonblocking || self.is_nonblock() {
                                    return Err(Errno::EAGAIN);
                                }
                                let deadline = self.recv_wait_deadline(opts.deadline_ns);
                                if self.deadline_expired(deadline) {
                                    return Err(Errno::EAGAIN);
                                }
                                self.wait_with_deadline_until(deadline, || {
                                    net::stack().socket_can_recv(handle)
                                })?;
                            }
                            Err(e) => return Err(map_net_error(e)),
                        }
                    }
                }
                loop {
                    let peer = *self.remote.lock();
                    match net::stack().udp_recv_from(handle, buf) {
                        Ok((n, remote)) => {
                            if !udp_peer_matches(peer, remote) {
                                continue;
                            }
                            return Ok(InetRecvResult {
                                len: n,
                                remote: Some(remote),
                                msg_flags: datagram_msg_flags(n, buf.len(), opts.trunc),
                            });
                        }
                        Err(NetError::WouldBlock) => {
                            if opts.nonblocking || self.is_nonblock() {
                                return Err(Errno::EAGAIN);
                            }
                            let deadline = self.recv_wait_deadline(opts.deadline_ns);
                            if self.deadline_expired(deadline) {
                                return Err(Errno::EAGAIN);
                            }
                            self.wait_with_deadline_until(deadline, || {
                                net::stack().socket_can_recv(handle)
                            })?;
                        }
                        Err(e) => return Err(map_net_error(e)),
                    }
                }
            }
            SocketType::Raw | SocketType::Icmp => {
                let n = self.do_recv_with(buf, opts.nonblocking, opts.deadline_ns)?;
                Ok(InetRecvResult {
                    len: n,
                    remote: None,
                    msg_flags: datagram_msg_flags(n, buf.len(), opts.trunc),
                })
            }
        }
    }

    pub fn set_nonblock(&self, nonblock: bool) {
        self.nonblock.store(nonblock, Ordering::Relaxed);
    }

    pub fn getsockname(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if let Ok(handle) = self.get_handle() {
            match handle.socket_type() {
                SocketType::Tcp => {
                    if let Some(ep) = net::stack().tcp_local_endpoint(handle) {
                        return addr::encode_inet_sockaddr(&ep, self.family, buf);
                    }
                }
                SocketType::Udp => {
                    if let Some(mut ep) = net::stack().udp_local_endpoint(handle) {
                        if let Some(cached) = *self.local.lock() {
                            ep.addr = cached.addr;
                        }
                        return addr::encode_inet_sockaddr(&ep, self.family, buf);
                    }
                }
                SocketType::Raw | SocketType::Icmp => {}
            }
        }
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
        if let Ok(handle) = self.get_handle() {
            if handle.socket_type() == SocketType::Tcp {
                if let Some(ep) = net::stack().tcp_remote_endpoint(handle) {
                    return addr::encode_inet_sockaddr(&ep, self.family, buf);
                }
            }
        }
        let remote = *self.remote.lock();
        match remote {
            // UDP/RAW 当前只是 connect/sendto 写入的缓存值，不能代表
            // 底层连通性或路由结果；TCP 在上面已经优先查真实 peer。
            Some(ep) => addr::encode_inet_sockaddr(&ep, self.family, buf),
            None => Err(Errno::ENOTCONN),
        }
    }
}

// ── 内部 I/O ─────────────────────────────────────────────────────────────────

impl NetSocketFileOps {
    fn rehome_for_endpoint(
        &self,
        handle: NetSocketHandle,
        ep: &Endpoint,
    ) -> Result<NetSocketHandle, Errno> {
        let Some(target_iface) = net::stack().resolve_iface_for_addr(&ep.addr) else {
            return Ok(handle);
        };
        if target_iface == handle.interface_id() {
            return Ok(handle);
        }

        let new_handle = match handle.socket_type() {
            SocketType::Tcp | SocketType::Udp => {
                if !matches!(net::stack().socket_state(handle), net::SocketState::Closed) {
                    return Ok(handle);
                }
                // socket() 创建时还没有 bind/connect 地址，只能先落到默认接口。
                // 第一次拿到本地或远端地址后，如果它明确指向 loopback/其它接口，
                // 就把尚未使用的 TCP/UDP socket 迁移到正确接口，避免本地流量
                // 错误地走物理网卡。
                match handle.socket_type() {
                    SocketType::Tcp => net::stack()
                        .socket_tcp_on(target_iface)
                        .map_err(map_net_error)?,
                    SocketType::Udp => net::stack()
                        .socket_udp_on(target_iface)
                        .map_err(map_net_error)?,
                    SocketType::Raw | SocketType::Icmp => unreachable!(),
                }
            }
            SocketType::Raw => {
                if net::stack().raw_can_recv(handle) {
                    return Ok(handle);
                }
                let ip_ver = if self.family == 10 { 6u8 } else { 4u8 };
                net::stack()
                    .socket_raw_on(target_iface, ip_ver, self.protocol as u8)
                    .map_err(map_net_error)?
            }
            SocketType::Icmp => {
                if net::stack().raw_can_recv(handle) {
                    return Ok(handle);
                }
                net::stack()
                    .socket_icmp_on(target_iface)
                    .map_err(map_net_error)?
            }
        };
        net::stack().socket_close_and_remove(handle);
        *self.handle.lock() = Some(new_handle);
        Ok(new_handle)
    }

    fn do_send(&self, data: &[u8]) -> Result<usize, Errno> {
        self.do_send_with(
            data,
            None,
            InetSendOptions {
                nonblocking: self.is_nonblock(),
                deadline_ns: None,
            },
        )
    }

    fn do_send_with(
        &self,
        data: &[u8],
        remote_override: Option<Endpoint>,
        opts: InetSendOptions,
    ) -> Result<usize, Errno> {
        let handle = self.get_handle()?;
        if self.options.lock().write_shutdown {
            return Err(Errno::EPIPE);
        }
        let deadline = self.send_wait_deadline(opts.deadline_ns);
        let nonblocking = opts.nonblocking || self.is_nonblock();
        match handle.socket_type() {
            SocketType::Tcp => loop {
                match net::stack().tcp_send(handle, data) {
                    Ok(n) => {
                        // TCP send 已经主动 poll 协议栈；这里再让出一次 CPU，
                        // 让 loopback 对端及时运行并消费刚送达的数据。netperf
                        // TCP_RR/TCP_CRR 这类短请求响应会在 write 后立刻 read，
                        // 如果当前任务连续运行，响应端可能先睡进 recv，客户端
                        // 还没机会处理已唤醒的可读事件。
                        sched::schedule_once(sched::now_ns_public());
                        return Ok(n);
                    }
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        if net::stack().socket_can_send(handle) {
                            continue;
                        }
                        self.wait_with_deadline_until(deadline, || {
                            net::stack().socket_can_send(handle)
                        })?;
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Udp => loop {
                let remote = remote_override
                    .or_else(|| *self.remote.lock())
                    .ok_or(Errno::EDESTADDRREQ)?;
                let handle = self.ensure_udp_bound(handle, Some(remote))?;
                // FIXME: connected UDP 由 VFS remote 缓存模拟，底层 UDP socket
                // 没有 connect 状态，也不会过滤非 peer datagram。
                match net::stack().udp_send_to(handle, data, remote) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        self.wait_with_deadline_until(deadline, || {
                            net::stack().socket_can_send(handle)
                        })?;
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Raw => {
                let mut handle = handle;
                loop {
                    // TODO: raw send 缺少目的地址、IP_HDRINCL、MSG_DONTROUTE 等语义。
                    if let Some(remote) = remote_override {
                        handle = self.rehome_for_endpoint(handle, &remote)?;
                    }
                    match net::stack().raw_send(handle, data, remote_override) {
                        Ok(n) => return Ok(n),
                        Err(NetError::WouldBlock) => {
                            if nonblocking {
                                return Err(Errno::EAGAIN);
                            }
                            if self.deadline_expired(deadline) {
                                return Err(Errno::EAGAIN);
                            }
                            self.wait_with_deadline_until(deadline, || {
                                net::stack().raw_can_send(handle)
                            })?;
                        }
                        Err(e) => return Err(map_net_error(e)),
                    }
                }
            }
            SocketType::Icmp => {
                let mut handle = handle;
                loop {
                    let remote = remote_override
                        .or_else(|| *self.remote.lock())
                        .ok_or(Errno::EDESTADDRREQ)?;
                    handle = self.rehome_for_endpoint(handle, &remote)?;
                    match net::stack().raw_send(handle, data, Some(remote)) {
                        Ok(n) => return Ok(n),
                        Err(NetError::WouldBlock) => {
                            if nonblocking {
                                return Err(Errno::EAGAIN);
                            }
                            if self.deadline_expired(deadline) {
                                return Err(Errno::EAGAIN);
                            }
                            self.wait_with_deadline_until(deadline, || {
                                net::stack().raw_can_send(handle)
                            })?;
                        }
                        Err(e) => return Err(map_net_error(e)),
                    }
                }
            }
        }
    }

    fn recv_deadline(&self) -> Option<u64> {
        let ns = self.recv_timeout_ns.load(Ordering::Relaxed);
        if ns == 0 {
            None
        } else {
            Some(sched::now_ns_public().saturating_add(ns))
        }
    }

    fn send_deadline(&self) -> Option<u64> {
        let ns = self.send_timeout_ns.load(Ordering::Relaxed);
        if ns == 0 {
            None
        } else {
            Some(sched::now_ns_public().saturating_add(ns))
        }
    }

    fn recv_wait_deadline(&self, call_deadline: Option<u64>) -> Option<u64> {
        call_deadline.or_else(|| self.recv_deadline())
    }

    fn send_wait_deadline(&self, call_deadline: Option<u64>) -> Option<u64> {
        call_deadline.or_else(|| self.send_deadline())
    }

    fn deadline_expired(&self, deadline: Option<u64>) -> bool {
        deadline.is_some_and(|dl| sched::now_ns_public() >= dl)
    }

    fn do_recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.do_recv_with(buf, self.is_nonblock(), None)
    }

    fn ensure_udp_bound(
        &self,
        handle: NetSocketHandle,
        hint: Option<Endpoint>,
    ) -> Result<NetSocketHandle, Errno> {
        if handle.socket_type() != SocketType::Udp {
            return Ok(handle);
        }
        if self.local.lock().is_some_and(|ep| ep.port != 0) {
            return Ok(handle);
        }

        let addr = self
            .local
            .lock()
            .map(|ep| ep.addr)
            .or_else(|| hint.map(|ep| unspecified_for_addr(&ep.addr)))
            .unwrap_or_else(|| {
                if self.family == addr::AF_INET6 {
                    net::IpAddr::V6(net::Ipv6Addr::UNSPECIFIED)
                } else {
                    net::IpAddr::V4(net::Ipv4Addr::UNSPECIFIED)
                }
            });
        let bound = net::stack()
            .udp_bind(handle, Endpoint { addr, port: 0 })
            .map_err(map_net_error)?;
        *self.local.lock() = Some(bound);
        Ok(handle)
    }

    fn do_recv_with(
        &self,
        buf: &mut [u8],
        nonblocking: bool,
        call_deadline: Option<u64>,
    ) -> Result<usize, Errno> {
        if self.options.lock().read_shutdown {
            return Ok(0); // EOF — 读方向已关闭
        }
        let handle = self.get_handle()?;
        let deadline = self.recv_wait_deadline(call_deadline);
        let nonblocking = nonblocking || self.is_nonblock();
        match handle.socket_type() {
            SocketType::Tcp => loop {
                match net::stack().tcp_recv(handle, buf) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        if net::stack().socket_can_recv(handle) {
                            continue;
                        }
                        // FIXME: 阻塞 I/O 等待由全局 net poll 唤醒，缺少精确
                        // socket readiness 订阅。
                        self.wait_with_deadline_until(deadline, || {
                            net::stack().socket_can_recv(handle)
                        })?;
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Udp => loop {
                let peer = *self.remote.lock();
                match net::stack().udp_recv_from(handle, buf) {
                    Ok((n, remote)) => {
                        if !udp_peer_matches(peer, remote) {
                            continue;
                        }
                        return Ok(n);
                    }
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        self.wait_with_deadline_until(deadline, || {
                            net::stack().socket_can_recv(handle)
                        })?;
                    }
                    Err(e) => return Err(map_net_error(e)),
                }
            },
            SocketType::Raw | SocketType::Icmp => loop {
                match net::stack().raw_recv(handle, buf) {
                    Ok(n) => return Ok(n),
                    Err(NetError::WouldBlock) => {
                        if nonblocking {
                            return Err(Errno::EAGAIN);
                        }
                        if self.deadline_expired(deadline) {
                            return Err(Errno::EAGAIN);
                        }
                        self.wait_with_deadline_until(deadline, || {
                            net::stack().raw_can_recv(handle)
                        })?;
                    }
                    Err(e) => return Err(map_net_error(e)),
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
        _protocol,
        nonblock,
    ))
}

// ── 错误映射 ─────────────────────────────────────────────────────────────────

fn map_net_error(e: NetError) -> Errno {
    // TODO: 错误映射过粗，缺少 ENOTCONN/EHOSTUNREACH/ENETUNREACH/EISCONN
    // 等网络语义；Closed 也不应在所有路径都映射成 EPIPE。
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

fn datagram_msg_flags(len: usize, buf_len: usize, trunc_requested: bool) -> usize {
    if trunc_requested && buf_len != 0 && len == buf_len {
        crate::socket::MSG_TRUNC
    } else {
        0
    }
}

fn endpoint_addr_is_unspecified(ep: &Endpoint) -> bool {
    match ep.addr {
        net::IpAddr::V4(v4) => v4 == net::Ipv4Addr::UNSPECIFIED,
        net::IpAddr::V6(v6) => v6 == net::Ipv6Addr::UNSPECIFIED,
    }
}

fn unspecified_for_addr(addr: &net::IpAddr) -> net::IpAddr {
    match addr {
        net::IpAddr::V4(_) => net::IpAddr::V4(net::Ipv4Addr::UNSPECIFIED),
        net::IpAddr::V6(_) => net::IpAddr::V6(net::Ipv6Addr::UNSPECIFIED),
    }
}

fn udp_peer_matches(peer: Option<Endpoint>, remote: Endpoint) -> bool {
    peer.is_none_or(|expected| expected == remote)
}

fn has_unblocked_signal(task: &Arc<Task>) -> bool {
    sched::operation::has_interrupting_signal(task)
}

fn errno_to_vfs(e: Errno) -> VfsError {
    match e {
        Errno::EAGAIN => VfsError::WouldBlock,
        Errno::EINTR => VfsError::Interrupted,
        Errno::EINVAL => VfsError::InvalidArgument,
        Errno::ETIMEDOUT => VfsError::TimedOut,
        Errno::EPIPE => VfsError::BrokenPipe,
        Errno::ECONNRESET => VfsError::ConnectionReset,
        _ => VfsError::Io,
    }
}
