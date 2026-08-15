//! INET socket 的 VFS 适配层。
//!
//! 本层只保存稳定的 `NetSocketProxy`、`PollSource` 和 VFS 可见 option，不保存协议状态。

use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use errno::Errno;
use sched::Task;
use spin::Mutex;

use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::poll_source::PollSource;

const SOCK_STREAM: u16 = 1;
const SOCK_DGRAM: u16 = 2;
const SOCK_RAW: u16 = 3;

static NET_IOCTL_HANDLER: Mutex<Option<fn(u32, usize) -> Result<usize, Errno>>> = Mutex::new(None);
static NET_REALTIME_CLOCK: Mutex<Option<fn() -> u64>> = Mutex::new(None);

pub fn install_net_ioctl_handler(handler: fn(u32, usize) -> Result<usize, Errno>) {
    *NET_IOCTL_HANDLER.lock() = Some(handler);
}

/// 将 socket ioctl 交给当前网络运行时处理。
///
/// Linux 的接口查询 ioctl 并不要求文件描述符来自 INET socket；glibc 的
/// `if_nametoindex()` 等接口通常会使用 AF_UNIX 数据报 socket。因此所有 socket
/// 类型必须共享同一个网络 ioctl 分派入口。
pub fn dispatch_net_ioctl(cmd: u32, arg: usize) -> Result<usize, Errno> {
    let Some(handler) = *NET_IOCTL_HANDLER.lock() else {
        return Err(Errno::ENOTTY);
    };
    handler(cmd, arg)
}

pub fn install_net_realtime_clock(clock: fn() -> u64) {
    *NET_REALTIME_CLOCK.lock() = Some(clock);
}

pub fn packet_realtime_ns(monotonic_ns: u64) -> u64 {
    NET_REALTIME_CLOCK
        .lock()
        .map(|clock| {
            let now_monotonic = sched::now_ns_public();
            clock().saturating_sub(now_monotonic.saturating_sub(monotonic_ns))
        })
        .unwrap_or(monotonic_ns)
}

pub const SOCK_STREAM_PUB: u16 = SOCK_STREAM;
#[allow(dead_code)]
pub const SOCK_DGRAM_PUB: u16 = SOCK_DGRAM;

const TCP_DEFAULT_SEND_BUFFER: i32 = 16 * 1024;
const TCP_DEFAULT_RECEIVE_BUFFER: i32 = 128 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct InetSendOptions {
    pub nonblocking: bool,
    pub more: bool,
    pub dont_route: bool,
    pub confirm: bool,
    pub deadline_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct InetRecvOptions {
    pub nonblocking: bool,
    pub peek: bool,
    pub wait_all: bool,
    pub trunc: bool,
    pub defer_window_update: bool,
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
    pub rx_timestamp_ns: u64,
    pub msg_flags: usize,
}

#[derive(Debug, Clone)]
pub struct SocketOptions {
    pub reuseaddr: bool,
    pub reuseport: bool,
    pub sndbuf: i32,
    pub rcvbuf: i32,
    pub timestamp: bool,
    pub recvttl: bool,
    pub recvtos: bool,
    pub pktinfo: bool,
    pub v6only: bool,
    pub recv_pktinfo_v6: bool,
    pub recv_hoplimit_v6: bool,
    pub ttl: u8,
    pub traffic_class: u8,
    pub header_included: bool,
    pub ipv6_hops: u8,
    pub ipv6_traffic_class: u8,
    pub multicast_interface: Option<net::InterfaceId>,
    pub multicast_hops: u8,
    pub multicast_loop: bool,
    pub receive_errors_v4: bool,
    pub receive_errors_v6: bool,
    pub broadcast: bool,
    pub dont_route: bool,
    pub bind_interface: Option<net::InterfaceId>,
    pub bind_device_name: alloc::vec::Vec<u8>,
    pub mark: u32,
    pub priority: i32,
    pub receive_overflow: bool,
    pub free_bind: bool,
    pub linger_enabled: bool,
    pub linger_seconds: u32,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            reuseaddr: false,
            reuseport: false,
            sndbuf: 212_992,
            rcvbuf: 212_992,
            timestamp: false,
            recvttl: false,
            recvtos: false,
            pktinfo: false,
            v6only: false,
            recv_pktinfo_v6: false,
            recv_hoplimit_v6: false,
            ttl: 64,
            traffic_class: 0,
            header_included: false,
            ipv6_hops: 64,
            ipv6_traffic_class: 0,
            multicast_interface: None,
            multicast_hops: 1,
            multicast_loop: true,
            receive_errors_v4: false,
            receive_errors_v6: false,
            broadcast: false,
            dont_route: false,
            bind_interface: None,
            bind_device_name: alloc::vec::Vec::new(),
            mark: 0,
            priority: 0,
            receive_overflow: false,
            free_bind: false,
            linger_enabled: false,
            linger_seconds: 0,
        }
    }
}

pub struct NetSocketFileOps {
    proxy: net::NetSocketProxy,
    poll_source: Arc<PollSource>,
    nonblock: AtomicBool,
    recv_timeout_ns: AtomicU64,
    send_timeout_ns: AtomicU64,
    options: Mutex<SocketOptions>,
    /// SO_ATTACH_FILTER 安装的 cBPF 程序（作用于网络层报文）。
    filter: Mutex<Option<alloc::sync::Arc<net::bpf::CbpfProgram>>>,
    /// SO_LOCK_FILTER：禁止替换/卸载过滤器。
    filter_locked: AtomicBool,
}

impl net::ReadinessObserver for PollSource {
    fn readiness_changed(&self, readiness: net::Readiness, generation: u64) {
        let mut events = PollEvents::default();
        if readiness.contains(net::Readiness::READABLE) {
            events = events.with(PollEvents::POLLIN);
        }
        if readiness.contains(net::Readiness::WRITABLE) {
            events = events.with(PollEvents::POLLOUT);
        }
        if readiness.contains(net::Readiness::ERROR) {
            events = events.with(PollEvents::POLLERR);
        }
        if readiness.contains(net::Readiness::HANGUP) {
            events = events.with(PollEvents::POLLHUP);
        }
        if readiness.contains(net::Readiness::READ_HANGUP) {
            events = events.with(PollEvents::POLLRDHUP);
        }
        self.publish_versioned(events, generation);
    }

    fn readiness_updates_required(&self) -> bool {
        self.tracking_enabled()
    }
}

impl NetSocketFileOps {
    pub fn family(&self) -> u16 {
        match self.proxy.family() {
            net::AddressFamily::Ipv4 => crate::addr::AF_INET,
            net::AddressFamily::Ipv6 => crate::addr::AF_INET6,
        }
    }

    pub fn sock_type(&self) -> u16 {
        match self.proxy.kind() {
            net::SocketKind::Datagram => SOCK_DGRAM,
            net::SocketKind::Stream => SOCK_STREAM,
            net::SocketKind::Raw => SOCK_RAW,
        }
    }

    /// 结束由 syscall 用户页窗口组成的一次流发送，并发布此前接受的全部数据。
    pub fn finish_stream_send(&self) {
        if self.sock_type() == SOCK_STREAM {
            self.proxy.set_tcp_more(false);
        }
    }

    /// 结束由 syscall 用户页窗口组成的一次流接收，并发布累计的窗口变化。
    pub fn finish_stream_receive(&self) {
        if self.sock_type() == SOCK_STREAM {
            self.proxy.finish_stream_receive();
        }
    }

    pub fn options(&self) -> &Mutex<SocketOptions> {
        &self.options
    }

    pub fn take_last_error_code(&self) -> i32 {
        self.proxy
            .take_pending_error()
            .map(map_socket_error)
            .map(i32::from)
            .unwrap_or(0)
    }

    pub fn take_error_record(&self) -> Result<net::SocketErrorRecord, Errno> {
        let options = self.options.lock();
        let enabled = if self.family() == crate::addr::AF_INET {
            options.receive_errors_v4
        } else {
            options.receive_errors_v6
        };
        drop(options);
        if !enabled {
            return Err(Errno::EAGAIN);
        }
        self.proxy.take_error_record().ok_or(Errno::EAGAIN)
    }

    pub fn recv_timeout_ns(&self) -> &AtomicU64 {
        &self.recv_timeout_ns
    }

    pub fn send_timeout_ns(&self) -> &AtomicU64 {
        &self.send_timeout_ns
    }

    pub fn bind(&self, sockaddr: &[u8], allow_privileged_port: bool) -> Result<(), Errno> {
        self.ensure_backend()?;
        let local = crate::addr::parse_inet_sockaddr_for_socket(sockaddr, self.family())?;
        validate_bind_permission(&local, allow_privileged_port)?;
        let options = self.bind_options_for(local.addr);
        let interface = self.options.lock().bind_interface;
        self.proxy
            .bind(local, interface, options)
            .map_err(map_socket_error)
    }

    pub fn listen(&self, backlog: u32) -> Result<(), Errno> {
        self.ensure_backend()?;
        if self.sock_type() != SOCK_STREAM {
            return Err(Errno::EOPNOTSUPP);
        }
        self.proxy.listen(backlog).map_err(map_socket_error)
    }

    pub fn accept(
        &self,
        wait_nonblocking: bool,
        accepted_nonblocking: bool,
    ) -> Result<Self, Errno> {
        self.ensure_backend()?;
        if self.sock_type() != SOCK_STREAM {
            return Err(Errno::EOPNOTSUPP);
        }
        let proxy = self
            .proxy
            .accept(wait_nonblocking, self.recv_deadline())
            .map_err(map_socket_error)?;
        Ok(Self::from_proxy(
            proxy,
            accepted_nonblocking,
            self.options.lock().clone(),
        ))
    }

    pub fn connect(&self, sockaddr: &[u8], nonblocking: bool) -> Result<(), Errno> {
        self.ensure_backend()?;
        let peer = normalize_connect_endpoint(crate::addr::parse_inet_sockaddr_for_socket(
            sockaddr,
            self.family(),
        )?);
        let interface = self.options.lock().bind_interface;
        let options = self.bind_options();
        self.proxy
            .connect_with_mode(peer, interface, options, nonblocking)
            .map_err(map_socket_error)
    }

    pub fn shutdown(&self, how: u32) -> Result<(), Errno> {
        self.ensure_backend()?;
        let (read, write) = match how {
            0 => (true, false),
            1 => (false, true),
            2 => (true, true),
            _ => return Err(Errno::EINVAL),
        };
        self.proxy.shutdown(read, write).map_err(map_socket_error)
    }

    pub fn sendto(
        &self,
        data: &[u8],
        sockaddr: Option<&[u8]>,
        opts: InetSendOptions,
    ) -> Result<usize, Errno> {
        self.ensure_backend()?;
        if self.sock_type() == SOCK_STREAM {
            if sockaddr.is_some() {
                return Err(Errno::EISCONN);
            }
            let deadline = opts.deadline_ns.or_else(|| self.send_deadline());
            return self
                .proxy
                .send_stream(data, opts.nonblocking, deadline, opts.more)
                .map_err(map_socket_error);
        }
        if opts.more {
            return Err(Errno::EOPNOTSUPP);
        }
        self.ensure_bound()?;
        let destination = sockaddr
            .map(|raw| crate::addr::parse_inet_sockaddr_for_socket(raw, self.family()))
            .transpose()?;
        let socket_options = self.options.lock().clone();
        if destination.is_some_and(
            |endpoint| matches!(endpoint.addr, net::IpAddr::V4(address) if address.is_broadcast()),
        ) && !socket_options.broadcast
        {
            return Err(Errno::EACCES);
        }
        let deadline = opts.deadline_ns.or_else(|| self.send_deadline());
        self.proxy
            .send_datagram(
                data,
                destination,
                opts.nonblocking,
                deadline,
                opts.dont_route || socket_options.dont_route,
                opts.confirm,
            )
            .map_err(map_socket_error)
    }

    /// 直接把外部复制源写入 UDP 发送槽；复制错误保持原 errno，且不会提交半个数据报。
    pub fn sendto_from(
        &self,
        payload_len: usize,
        sockaddr: Option<&[u8]>,
        opts: InetSendOptions,
        copy: impl FnMut(usize, &mut [u8]) -> Result<(), Errno>,
    ) -> Result<usize, Errno> {
        self.ensure_backend()?;
        if self.sock_type() != SOCK_DGRAM {
            return Err(Errno::EINVAL);
        }
        if opts.more {
            return Err(Errno::EOPNOTSUPP);
        }
        self.ensure_bound()?;
        let destination = sockaddr
            .map(|raw| crate::addr::parse_inet_sockaddr_for_socket(raw, self.family()))
            .transpose()?;
        let socket_options = self.options.lock().clone();
        if destination.is_some_and(
            |endpoint| matches!(endpoint.addr, net::IpAddr::V4(address) if address.is_broadcast()),
        ) && !socket_options.broadcast
        {
            return Err(Errno::EACCES);
        }
        let deadline = opts.deadline_ns.or_else(|| self.send_deadline());
        self.proxy
            .send_datagram_from(
                payload_len,
                destination,
                opts.nonblocking,
                deadline,
                opts.dont_route || socket_options.dont_route,
                opts.confirm,
                copy,
            )
            .map_err(|error| match error {
                net::DatagramCopyError::Socket(error) => map_socket_error(error),
                net::DatagramCopyError::Copy(error) => error,
            })
    }

    pub fn send_stream_page(
        &self,
        data: &[u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
        more: bool,
    ) -> Result<usize, Errno> {
        self.proxy
            .send_stream(data, nonblocking, deadline_ns, more)
            .map_err(map_socket_error)
    }

    pub fn send_stream_from(
        &self,
        payload_len: usize,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        more: bool,
        copy: impl FnMut(usize, &mut [u8]),
    ) -> Result<usize, Errno> {
        self.proxy
            .send_stream_from(payload_len, nonblocking, deadline_ns, more, copy)
            .map_err(map_socket_error)
    }

    pub fn stream_send_deadline(&self) -> Option<u64> {
        self.send_deadline()
    }

    pub fn recvfrom(&self, buf: &mut [u8], opts: InetRecvOptions) -> Result<InetRecvResult, Errno> {
        self.ensure_backend()?;
        let deadline = opts.deadline_ns.or_else(|| self.recv_deadline());
        if self.sock_type() == SOCK_STREAM {
            let result = self
                .proxy
                .recv_stream(
                    buf,
                    opts.peek,
                    opts.wait_all,
                    opts.defer_window_update,
                    opts.nonblocking,
                    deadline,
                )
                .map_err(map_socket_error);
            if !opts.defer_window_update {
                self.proxy.finish_stream_receive();
            }
            let len = result?;
            return Ok(InetRecvResult {
                len,
                remote: self.proxy.peer_endpoint(),
                local: self.proxy.local_endpoint(),
                interface_id: None,
                hop_limit: None,
                traffic_class: None,
                rx_timestamp_ns: 0,
                msg_flags: 0,
            });
        }
        self.ensure_bound()?;
        let received = self
            .proxy
            .recv(buf, opts.peek, opts.trunc, opts.nonblocking, deadline)
            .map_err(map_socket_error)?;
        Ok(InetRecvResult {
            len: received.len,
            remote: Some(received.source),
            local: Some(received.destination),
            interface_id: Some(net::NetDeviceId(received.ingress_interface.0)),
            hop_limit: Some(received.hop_limit),
            traffic_class: Some(received.traffic_class),
            rx_timestamp_ns: received.rx_timestamp_ns,
            msg_flags: usize::from(received.truncated) * 0x20,
        })
    }

    pub fn recv_stream_to(
        &self,
        output_len: usize,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        defer_window_update: bool,
        copy: impl FnMut(usize, &[u8]),
    ) -> Result<usize, Errno> {
        self.proxy
            .recv_stream_to(
                output_len,
                false,
                false,
                defer_window_update,
                nonblocking,
                deadline_ns,
                copy,
            )
            .map_err(map_socket_error)
    }

    /// 等待 UDP 数据报就绪但不消费，供 syscall 在等待结束后再固定用户目标页。
    pub fn wait_datagram_readable(&self, opts: InetRecvOptions) -> Result<Option<usize>, Errno> {
        self.ensure_backend()?;
        if self.sock_type() != SOCK_DGRAM {
            return Err(Errno::EINVAL);
        }
        self.ensure_bound()?;
        let deadline = opts.deadline_ns.or_else(|| self.recv_deadline());
        self.proxy
            .wait_datagram_readable(opts.nonblocking, deadline)
            .map_err(map_socket_error)
    }

    /// 将队首本地 UDP 数据报直接复制到外部目标；非本地数据返回 `None`。
    pub fn recv_local_datagram_from(
        &self,
        output_len: usize,
        copy_capacity: usize,
        opts: InetRecvOptions,
        copy: impl FnMut(usize, &[u8]) -> Result<(), Errno>,
    ) -> Result<Option<InetRecvResult>, Errno> {
        self.ensure_backend()?;
        if self.sock_type() != SOCK_DGRAM || opts.peek || opts.wait_all {
            return Ok(None);
        }
        let received = self
            .proxy
            .recv_local_datagram_from(output_len, copy_capacity, opts.trunc, copy)
            .map_err(|error| match error {
                net::DatagramCopyError::Socket(error) => map_socket_error(error),
                net::DatagramCopyError::Copy(error) => error,
            })?;
        Ok(received.map(|received| InetRecvResult {
            len: received.len,
            remote: Some(received.source),
            local: Some(received.destination),
            interface_id: Some(net::NetDeviceId(received.ingress_interface.0)),
            hop_limit: Some(received.hop_limit),
            traffic_class: Some(received.traffic_class),
            rx_timestamp_ns: received.rx_timestamp_ns,
            msg_flags: usize::from(received.truncated) * 0x20,
        }))
    }

    pub fn recv_stream_page(
        &self,
        buf: &mut [u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
        more: bool,
    ) -> Result<usize, Errno> {
        self.proxy
            .recv_stream(buf, false, false, more, nonblocking, deadline_ns)
            .map_err(map_socket_error)
    }

    pub fn stream_recv_deadline(&self) -> Option<u64> {
        self.recv_deadline()
    }

    pub fn getsockname(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.ensure_backend()?;
        let endpoint = self.proxy.local_endpoint().unwrap_or(net::Endpoint {
            addr: unspecified(self.family()),
            port: 0,
        });
        crate::addr::encode_inet_sockaddr(&endpoint, self.family(), buf)
    }

    pub fn getpeername(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.ensure_backend()?;
        let endpoint = self.proxy.peer_endpoint().ok_or(Errno::ENOTCONN)?;
        crate::addr::encode_inet_sockaddr(&endpoint, self.family(), buf)
    }

    pub fn protocol(&self) -> u16 {
        u16::from(self.proxy.protocol())
    }

    pub fn proxy(&self) -> &net::NetSocketProxy {
        &self.proxy
    }

    fn ensure_bound(&self) -> Result<(), Errno> {
        self.ensure_backend()?;
        if matches!(self.proxy.owner(), net::OwnerRef::Unassigned) {
            let interface = self.options.lock().bind_interface;
            let options = self.bind_options();
            let result = self.proxy.bind(
                net::Endpoint {
                    addr: unspecified(self.family()),
                    port: 0,
                },
                interface,
                options,
            );
            if let Err(error) = result
                && !(error == net::SocketError::InvalidState
                    && matches!(self.proxy.owner(), net::OwnerRef::Flow { .. }))
            {
                return Err(map_socket_error(error));
            }
        }
        Ok(())
    }

    fn ensure_backend(&self) -> Result<(), Errno> {
        self.proxy
            .backend_error()
            .map_or(Ok(()), |error| Err(map_socket_error(error)))
    }

    fn bind_options(&self) -> net::control::BindOptions {
        let options = self.options.lock();
        net::control::BindOptions {
            reuse_address: options.reuseaddr,
            reuse_port: options.reuseport,
            v6_only: options.v6only,
            multicast_or_broadcast: self.proxy.has_multicast_memberships(),
            free_bind: options.free_bind,
        }
    }

    fn bind_options_for(&self, address: net::IpAddr) -> net::control::BindOptions {
        let mut options = self.bind_options();
        options.multicast_or_broadcast |= address.is_multicast()
            || matches!(address, net::IpAddr::V4(address) if address.is_broadcast());
        options
    }

    fn recv_deadline(&self) -> Option<u64> {
        timeout_deadline(self.recv_timeout_ns.load(Ordering::Relaxed))
    }

    fn send_deadline(&self) -> Option<u64> {
        timeout_deadline(self.send_timeout_ns.load(Ordering::Relaxed))
    }

    fn from_proxy(proxy: net::NetSocketProxy, nonblock: bool, options: SocketOptions) -> Self {
        let readiness = proxy.readiness();
        let poll_source = Arc::new(PollSource::new(PollEvents::default()));
        let observer: Arc<dyn net::ReadinessObserver> = poll_source.clone();
        proxy.set_observer(Arc::downgrade(&observer));
        observer.readiness_changed(readiness.0, readiness.1);
        Self {
            proxy,
            poll_source,
            nonblock: AtomicBool::new(nonblock),
            recv_timeout_ns: AtomicU64::new(0),
            send_timeout_ns: AtomicU64::new(0),
            options: Mutex::new(options),
            filter: Mutex::new(None),
            filter_locked: AtomicBool::new(false),
        }
    }

    /// SO_ATTACH_FILTER：安装 cBPF 过滤器（已锁定则 EPERM）。
    pub fn attach_filter(&self, program: net::bpf::CbpfProgram) -> Result<(), Errno> {
        if self.filter_locked.load(Ordering::Acquire) {
            return Err(Errno::EPERM);
        }
        *self.filter.lock() = Some(alloc::sync::Arc::new(program));
        Ok(())
    }

    /// SO_DETACH_FILTER：卸载过滤器（已锁定则 EPERM）。
    pub fn detach_filter(&self) -> Result<(), Errno> {
        if self.filter_locked.load(Ordering::Acquire) {
            return Err(Errno::EPERM);
        }
        *self.filter.lock() = None;
        Ok(())
    }

    /// SO_LOCK_FILTER：锁定当前过滤器，禁止后续替换/卸载。
    pub fn lock_filter(&self) -> Result<(), Errno> {
        self.filter_locked.store(true, Ordering::Release);
        Ok(())
    }

    /// SO_GET_FILTER：读取已安装的过滤器指令（未安装返回空）。
    pub fn get_filter(&self) -> alloc::vec::Vec<net::bpf::CbpfInsn> {
        self.filter
            .lock()
            .as_ref()
            .map(|program| program.instructions().to_vec())
            .unwrap_or_default()
    }

    /// 在接收路径执行过滤器（数据为网络层报文）。返回 false 表示丢弃。
    pub fn filter_accepts(&self, packet: &[u8]) -> bool {
        let Some(filter) = self.filter.lock().as_ref().cloned() else {
            return true;
        };
        filter.run(packet) != 0
    }
}

pub(crate) fn normalize_connect_endpoint(mut endpoint: net::Endpoint) -> net::Endpoint {
    endpoint.addr = match endpoint.addr {
        net::IpAddr::V4(address) if address == net::Ipv4Addr::UNSPECIFIED => {
            net::IpAddr::V4(net::Ipv4Addr::LOCALHOST)
        }
        net::IpAddr::V6(address) if address == net::Ipv6Addr::UNSPECIFIED => {
            net::IpAddr::V6(net::Ipv6Addr::LOCALHOST)
        }
        address => address,
    };
    endpoint
}

pub(crate) fn validate_bind_permission(
    endpoint: &net::Endpoint,
    allow_privileged_port: bool,
) -> Result<(), Errno> {
    if endpoint.port != 0 && endpoint.port < 1024 && !allow_privileged_port {
        return Err(Errno::EACCES);
    }
    Ok(())
}

impl FileOps for NetSocketFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.recvfrom(
            buf,
            InetRecvOptions {
                nonblocking: self.nonblock.load(Ordering::Relaxed),
                peek: false,
                wait_all: false,
                trunc: false,
                defer_window_update: false,
                deadline_ns: None,
            },
        )
        .map(|result| result.len)
        .map_err(map_errno_to_vfs)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.sendto(
            buf,
            None,
            InetSendOptions {
                nonblocking: self.nonblock.load(Ordering::Relaxed),
                more: false,
                dont_route: false,
                confirm: false,
                deadline_ns: None,
            },
        )
        .map_err(map_errno_to_vfs)
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
        let readiness = self.proxy.readiness().0;
        let mut ready = PollEvents::default();
        if readiness.contains(net::Readiness::READABLE) {
            ready = ready.with(PollEvents::POLLIN);
        }
        if readiness.contains(net::Readiness::WRITABLE) {
            ready = ready.with(PollEvents::POLLOUT);
        }
        if readiness.contains(net::Readiness::ERROR) {
            ready = ready.with(PollEvents::POLLERR);
        }
        if readiness.contains(net::Readiness::HANGUP) {
            ready = ready.with(PollEvents::POLLHUP);
        }
        if readiness.contains(net::Readiness::READ_HANGUP) {
            ready = ready.with(PollEvents::POLLRDHUP);
        }
        ready.intersect(
            interest
                .with(PollEvents::POLLERR)
                .with(PollEvents::POLLHUP)
                .with(PollEvents::POLLRDHUP),
        )
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        self.proxy.add_poll_waiter(
            task,
            interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI),
            interest.has(PollEvents::POLLOUT),
            interest.has(PollEvents::POLLERR)
                || interest.has(PollEvents::POLLHUP)
                || interest.has(PollEvents::POLLRDHUP),
        )
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.proxy.remove_poll_waiter(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        self.poll_source.enable_tracking();
        let readiness = self.proxy.readiness();
        net::ReadinessObserver::readiness_changed(
            self.poll_source.as_ref(),
            readiness.0,
            readiness.1,
        );
        Some(&self.poll_source)
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn set_status_flags(&self, flags: OpenOptions) {
        self.nonblock.store(flags.nonblock, Ordering::Relaxed);
    }

    fn release(&self) {
        let options = self.options.lock().clone();
        if self.sock_type() == SOCK_STREAM && options.linger_enabled && options.linger_seconds == 0
        {
            self.proxy.request_abortive_close();
        }
        let deadline = (self.sock_type() == SOCK_STREAM
            && options.linger_enabled
            && options.linger_seconds != 0)
            .then(|| {
                sched::now_ns_public()
                    .saturating_add(u64::from(options.linger_seconds).saturating_mul(1_000_000_000))
            });
        self.proxy.close_with_deadline(deadline);
    }

    fn io_timeout_deadline(&self, interest: PollEvents) -> Option<u64> {
        if interest.has(PollEvents::POLLIN) {
            self.recv_deadline()
        } else if interest.has(PollEvents::POLLOUT) {
            self.send_deadline()
        } else {
            None
        }
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        dispatch_net_ioctl(cmd.raw() as u32, arg)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn create_net_socket(
    family: u16,
    sock_type: u16,
    protocol: u16,
    nonblock: bool,
) -> Result<NetSocketFileOps, Errno> {
    let stack = net::stack::stack_snapshot();
    if stack.state != net::stack::NetStackState::Active || !stack.ready {
        return Err(Errno::EAFNOSUPPORT);
    }
    let stack_instance = stack.handle.ok_or(Errno::EAFNOSUPPORT)?.0;
    let address_family = match family {
        crate::addr::AF_INET => net::AddressFamily::Ipv4,
        crate::addr::AF_INET6 => net::AddressFamily::Ipv6,
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    match sock_type {
        SOCK_DGRAM if matches!(protocol, 0 | 17) => {
            let proxy = net::NetSocketProxy::create(
                address_family,
                net::SocketKind::Datagram,
                17,
                stack.generation,
                stack_instance,
            )
            .map_err(map_socket_error)?;
            Ok(NetSocketFileOps::from_proxy(
                proxy,
                nonblock,
                SocketOptions {
                    sndbuf: 128 * 1024,
                    rcvbuf: 128 * 1024,
                    ..SocketOptions::default()
                },
            ))
        }
        SOCK_STREAM if matches!(protocol, 0 | 6) => {
            let proxy = net::NetSocketProxy::create(
                address_family,
                net::SocketKind::Stream,
                6,
                stack.generation,
                stack_instance,
            )
            .map_err(map_socket_error)?;
            Ok(NetSocketFileOps::from_proxy(
                proxy,
                nonblock,
                SocketOptions {
                    // 默认可见值保持常见 Linux socket ABI 行为；facade 内部保留
                    // 有界自动调节余量，显式 setsockopt 后两者会重新同步。
                    sndbuf: TCP_DEFAULT_SEND_BUFFER,
                    rcvbuf: TCP_DEFAULT_RECEIVE_BUFFER,
                    ..SocketOptions::default()
                },
            ))
        }
        SOCK_RAW if (1..=u8::MAX as u16).contains(&protocol) => {
            let proxy = net::NetSocketProxy::create(
                address_family,
                net::SocketKind::Raw,
                protocol as u8,
                stack.generation,
                stack_instance,
            )
            .map_err(map_socket_error)?;
            proxy.set_buffer_limits(Some(64 * 1024), Some(64 * 1024));
            Ok(NetSocketFileOps::from_proxy(
                proxy,
                nonblock,
                SocketOptions {
                    sndbuf: 64 * 1024,
                    rcvbuf: 64 * 1024,
                    ..SocketOptions::default()
                },
            ))
        }
        _ => Err(Errno::Other(93)),
    }
}

fn unspecified(family: u16) -> net::IpAddr {
    match family {
        crate::addr::AF_INET => net::IpAddr::V4(net::Ipv4Addr::UNSPECIFIED),
        crate::addr::AF_INET6 => net::IpAddr::V6(net::Ipv6Addr::UNSPECIFIED),
        _ => unreachable!(),
    }
}

fn timeout_deadline(timeout_ns: u64) -> Option<u64> {
    (timeout_ns != 0).then(|| sched::now_ns_public().saturating_add(timeout_ns))
}

pub(crate) fn map_socket_error_public(error: net::SocketError) -> Errno {
    match error {
        net::SocketError::RuntimeUnavailable => Errno::ENODEV,
        net::SocketError::RuntimeBusy | net::SocketError::WouldBlock => Errno::EAGAIN,
        net::SocketError::InvalidState => Errno::EINVAL,
        net::SocketError::AddressInUse => Errno::EADDRINUSE,
        net::SocketError::AddressUnavailable => Errno::EADDRNOTAVAIL,
        net::SocketError::NotConnected => Errno::ENOTCONN,
        net::SocketError::DestinationRequired => Errno::EDESTADDRREQ,
        net::SocketError::AlreadyConnected => Errno::EISCONN,
        net::SocketError::AlreadyInProgress => Errno::EALREADY,
        net::SocketError::InProgress => Errno::EINPROGRESS,
        net::SocketError::Interrupted => Errno::EINTR,
        net::SocketError::TimedOut => Errno::ETIMEDOUT,
        net::SocketError::MessageTooLarge => Errno::EMSGSIZE,
        net::SocketError::ReadShutdown => Errno::ENOTCONN,
        net::SocketError::WriteShutdown => Errno::EPIPE,
        net::SocketError::Closed => Errno::EBADF,
        net::SocketError::NetworkUnreachable => Errno::Other(101),
        net::SocketError::HostUnreachable => Errno::Other(113),
        net::SocketError::Buffer => Errno::ENOMEM,
        net::SocketError::ConnectionRefused => Errno::ECONNREFUSED,
        net::SocketError::ConnectionReset => Errno::ECONNRESET,
        net::SocketError::NetworkDown => Errno::ENETDOWN,
    }
}

fn map_socket_error(error: net::SocketError) -> Errno {
    map_socket_error_public(error)
}

fn map_errno_to_vfs(error: Errno) -> VfsError {
    match error {
        Errno::EAGAIN => VfsError::WouldBlock,
        Errno::EINVAL => VfsError::InvalidArgument,
        Errno::EBADF => VfsError::BadFileDescriptor,
        _ => VfsError::Io,
    }
}
