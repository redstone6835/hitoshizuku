//! INET socket 的 VFS 适配层。
//!
//! 本层只保存稳定的 `SocketFacade` 引用和 VFS 可见 option，不保存协议状态。

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
        }
    }
}

pub struct NetSocketFileOps {
    family: u16,
    sock_type: u16,
    protocol: u16,
    facade: Arc<net::SocketFacade>,
    poll_source: Arc<PollSource>,
    nonblock: AtomicBool,
    recv_timeout_ns: AtomicU64,
    send_timeout_ns: AtomicU64,
    options: Mutex<SocketOptions>,
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
        self.facade
            .take_pending_error()
            .map(map_socket_error)
            .map(i32::from)
            .unwrap_or(0)
    }

    pub fn recv_timeout_ns(&self) -> &AtomicU64 {
        &self.recv_timeout_ns
    }

    pub fn send_timeout_ns(&self) -> &AtomicU64 {
        &self.send_timeout_ns
    }

    pub fn bind(&self, sockaddr: &[u8]) -> Result<(), Errno> {
        let local = crate::addr::parse_inet_sockaddr_for_socket(sockaddr, self.family)?;
        let options = self.bind_options();
        self.facade
            .bind(local, None, options)
            .map_err(map_socket_error)
    }

    pub fn listen(&self, _backlog: u32) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    pub fn accept(&self, _nonblock: bool) -> Result<Self, Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    pub fn connect(&self, sockaddr: &[u8], _nonblocking: bool) -> Result<(), Errno> {
        let peer = crate::addr::parse_inet_sockaddr_for_socket(sockaddr, self.family)?;
        self.facade
            .connect(peer, None, self.bind_options())
            .map_err(map_socket_error)
    }

    pub fn shutdown(&self, how: u32) -> Result<(), Errno> {
        let (read, write) = match how {
            0 => (true, false),
            1 => (false, true),
            2 => (true, true),
            _ => return Err(Errno::EINVAL),
        };
        self.facade.shutdown(read, write).map_err(map_socket_error)
    }

    pub fn sendto(
        &self,
        data: &[u8],
        sockaddr: Option<&[u8]>,
        opts: InetSendOptions,
    ) -> Result<usize, Errno> {
        self.ensure_bound()?;
        let destination = sockaddr
            .map(|raw| crate::addr::parse_inet_sockaddr_for_socket(raw, self.family))
            .transpose()?;
        let deadline = opts.deadline_ns.or_else(|| self.send_deadline());
        self.facade
            .send(data, destination, opts.nonblocking, deadline)
            .map_err(map_socket_error)
    }

    pub fn recvfrom(&self, buf: &mut [u8], opts: InetRecvOptions) -> Result<InetRecvResult, Errno> {
        self.ensure_bound()?;
        let deadline = opts.deadline_ns.or_else(|| self.recv_deadline());
        let received = self
            .facade
            .recv(buf, opts.peek, opts.trunc, opts.nonblocking, deadline)
            .map_err(map_socket_error)?;
        Ok(InetRecvResult {
            len: received.len,
            remote: Some(received.source),
            local: Some(received.destination),
            interface_id: Some(net::NetDeviceId(received.ingress_interface.0)),
            hop_limit: Some(received.hop_limit),
            traffic_class: None,
            msg_flags: usize::from(received.truncated) * 0x20,
        })
    }

    pub fn getsockname(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let endpoint = self.facade.local_endpoint().unwrap_or(net::Endpoint {
            addr: unspecified(self.family),
            port: 0,
        });
        crate::addr::encode_inet_sockaddr(&endpoint, self.family, buf)
    }

    pub fn getpeername(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let endpoint = self.facade.peer_endpoint().ok_or(Errno::ENOTCONN)?;
        crate::addr::encode_inet_sockaddr(&endpoint, self.family, buf)
    }

    pub fn protocol(&self) -> u16 {
        self.protocol
    }

    pub fn facade(&self) -> &Arc<net::SocketFacade> {
        &self.facade
    }

    fn ensure_bound(&self) -> Result<(), Errno> {
        if matches!(self.facade.owner(), net::OwnerRef::Unassigned) {
            let result = self.facade.bind(
                net::Endpoint {
                    addr: unspecified(self.family),
                    port: 0,
                },
                None,
                self.bind_options(),
            );
            if let Err(error) = result
                && !(error == net::SocketError::InvalidState
                    && matches!(self.facade.owner(), net::OwnerRef::Flow { .. }))
            {
                return Err(map_socket_error(error));
            }
        }
        Ok(())
    }

    fn bind_options(&self) -> net::control::BindOptions {
        let options = self.options.lock();
        net::control::BindOptions {
            reuse_address: options.reuseaddr,
            reuse_port: options.reuseport,
            v6_only: options.v6only,
            multicast_or_broadcast: false,
        }
    }

    fn recv_deadline(&self) -> Option<u64> {
        timeout_deadline(self.recv_timeout_ns.load(Ordering::Relaxed))
    }

    fn send_deadline(&self) -> Option<u64> {
        timeout_deadline(self.send_timeout_ns.load(Ordering::Relaxed))
    }
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
        let readiness = self.facade.readiness().0;
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
        self.facade.add_poll_waiter(
            task,
            interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI),
            interest.has(PollEvents::POLLOUT),
            interest.has(PollEvents::POLLERR)
                || interest.has(PollEvents::POLLHUP)
                || interest.has(PollEvents::POLLRDHUP),
        )
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.facade.remove_poll_waiter(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.poll_source)
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn set_status_flags(&self, flags: OpenOptions) {
        self.nonblock.store(flags.nonblock, Ordering::Relaxed);
    }

    fn release(&self) {
        self.facade.close();
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
    family: u16,
    sock_type: u16,
    protocol: u16,
    nonblock: bool,
) -> Result<NetSocketFileOps, Errno> {
    let address_family = match family {
        crate::addr::AF_INET => net::AddressFamily::Ipv4,
        crate::addr::AF_INET6 => net::AddressFamily::Ipv6,
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    if sock_type != SOCK_DGRAM || !matches!(protocol, 0 | 17) {
        return Err(Errno::Other(93));
    }
    let facade = net::new_socket_facade(address_family).map_err(map_socket_error)?;
    let poll_source = Arc::new(PollSource::new(PollEvents::POLLOUT));
    let observer: Arc<dyn net::ReadinessObserver> = poll_source.clone();
    facade.set_observer(Arc::downgrade(&observer));
    Ok(NetSocketFileOps {
        family,
        sock_type,
        protocol: 17,
        facade,
        poll_source,
        nonblock: AtomicBool::new(nonblock),
        recv_timeout_ns: AtomicU64::new(0),
        send_timeout_ns: AtomicU64::new(0),
        options: Mutex::new(SocketOptions {
            sndbuf: 128 * 1024,
            rcvbuf: 128 * 1024,
            ..SocketOptions::default()
        }),
    })
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

fn map_socket_error(error: net::SocketError) -> Errno {
    match error {
        net::SocketError::RuntimeUnavailable => Errno::ENODEV,
        net::SocketError::RuntimeBusy | net::SocketError::WouldBlock => Errno::EAGAIN,
        net::SocketError::InvalidState => Errno::EINVAL,
        net::SocketError::AddressInUse => Errno::EADDRINUSE,
        net::SocketError::AddressUnavailable => Errno::EADDRNOTAVAIL,
        net::SocketError::NotConnected => Errno::ENOTCONN,
        net::SocketError::DestinationRequired => Errno::EDESTADDRREQ,
        net::SocketError::AlreadyConnected => Errno::EISCONN,
        net::SocketError::Interrupted => Errno::EINTR,
        net::SocketError::TimedOut => Errno::EAGAIN,
        net::SocketError::MessageTooLarge => Errno::EMSGSIZE,
        net::SocketError::ReadShutdown => Errno::ENOTCONN,
        net::SocketError::WriteShutdown => Errno::EPIPE,
        net::SocketError::Closed => Errno::EBADF,
        net::SocketError::NetworkUnreachable => Errno::Other(101),
        net::SocketError::HostUnreachable => Errno::Other(113),
        net::SocketError::Buffer => Errno::ENOMEM,
    }
}

fn map_errno_to_vfs(error: Errno) -> VfsError {
    match error {
        Errno::EAGAIN => VfsError::WouldBlock,
        Errno::EINVAL => VfsError::InvalidArgument,
        Errno::EBADF => VfsError::BadFileDescriptor,
        _ => VfsError::Io,
    }
}
