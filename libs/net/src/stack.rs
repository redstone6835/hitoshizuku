//! 全局网络协议栈管理。
//!
//! [`NetStack`] 是本 crate 的核心调度器，管理所有活跃网络接口和
//! 它们上面的 TCP/UDP socket。
//!
//! # 并发模型（面向高强度 I/O）
//!
//! 采用 **读写锁 + per-interface 锁** 双层架构：
//!
//! 1. 接口注册表用 [`RwLock`]——读路径（poll / socket 操作）取读锁，写路径
//!    （attach / detach）取写锁。读路径互不阻塞，多核可同时进入。
//! 2. 每个 [`ManagedInterface`] 内部自带独立 [`Mutex`]——同一接口的多个并发
//!    操作仍然串行（smoltcp 不可并发），但不同接口完全并行。
//!
//! # Socket 操作 API
//!
//! 所有 socket 操作为**非阻塞**——返回 `WouldBlock` 表示"稍后重试"。
//! 阻塞语义由上层（kernel 的 WaitQueue / poll/epoll）在 `libs/net` 之上实现。

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use spin::{Mutex, RwLock};
use smoltcp::time::Instant;
use smoltcp::wire::{IpEndpoint, IpAddress, Ipv4Address, Ipv6Address};

use crate::config::{Endpoint, IfConfig, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::device::{InterfaceId, NetDevice};
use crate::error::NetError;
use crate::interface::ManagedInterface;
use crate::socket::{NetSocketHandle, SocketState, SocketType};

// ── 常量 ─────────────────────────────────────────────────────────────────────

const TCP_RX_BUF_SIZE: usize = 65535;
const TCP_TX_BUF_SIZE: usize = 65535;
const UDP_RX_BUF_SIZE: usize = 8192;
const UDP_TX_BUF_SIZE: usize = 8192;
const UDP_META_COUNT: usize = 16;

// ── 全局单例 ─────────────────────────────────────────────────────────────────

static STACK: NetStack = NetStack::new();

/// 获取全局网络协议栈实例。
pub fn stack() -> &'static NetStack {
    &STACK
}


// ── NetStack ─────────────────────────────────────────────────────────────────

/// 全局网络协议栈。
pub struct NetStack {
    /// 接口注册表。读锁路径包含所有 I/O 操作，写锁仅 attach/detach 时持有。
    interfaces: RwLock<BTreeMap<InterfaceId, Arc<Mutex<ManagedInterface>>>>,
}

impl NetStack {
    const fn new() -> Self {
        Self {
            interfaces: RwLock::new(BTreeMap::new()),
        }
    }

    /// 注册一个新的网络接口。
    ///
    /// 设备驱动 probe 成功后调用。创建 smoltcp `Interface` 并配置网络参数。
    /// 此操作短暂持有写锁，会暂停所有正在进行的 poll。
    pub fn attach(&self, dev: Arc<NetDevice>, config: IfConfig) -> Result<(), NetError> {
        let id = dev.id();
        let managed = ManagedInterface::new(dev, config);
        let mut table = self.interfaces.write();
        if table.contains_key(&id) {
            return Err(NetError::InterfaceExists);
        }
        table.insert(id, Arc::new(Mutex::new(managed)));
        Ok(())
    }

    /// 移除一个网络接口。
    ///
    /// 设备热移除或驱动卸载时调用。流程：
    /// 1. 取写锁，从注册表移除接口（防止新的 poll 进入）
    /// 2. `Arc<Mutex<ManagedInterface>>` 引用计数归零时 drop——smoltcp 接口
    ///    和 socket set 自动释放
    ///
    /// `NetDevice::mark_gone` 应当由驱动 PnP remove 路径在调用本方法
    /// **之前**调用，使正在进行的 RX/TX 立即看到设备失效。
    pub fn detach(&self, id: InterfaceId) -> Result<(), NetError> {
        let mut table = self.interfaces.write();
        table.remove(&id).map(|_| ()).ok_or(NetError::InterfaceNotFound)
    }

    /// 驱动所有活跃接口进行一轮收发。
    ///
    /// 高强度 I/O 路径——零分配。读锁覆盖整轮 poll，期间无法 attach/detach
    /// （这是性能/一致性权衡：避免每次 poll 一次 `Vec` 分配）。
    ///
    /// 内核应周期性调用（定时器中断 / softirq / 网络线程）。
    pub fn poll(&self, timestamp: Instant) {
        let table = self.interfaces.read();
        for iface_lock in table.values() {
            let mut managed = iface_lock.lock();
            managed.poll(timestamp);
        }
    }

    /// 仅 poll 指定接口（中断驱动的快速路径，零分配）。
    ///
    /// 比 `poll()` 更轻量——只锁定单个接口，其他接口完全不受影响。
    /// 推荐网卡 IRQ handler 调用此方法而非全局 `poll`。
    pub fn poll_interface(&self, id: InterfaceId, timestamp: Instant) {
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&id) {
            iface_lock.lock().poll(timestamp);
        }
    }

    /// 查询已注册接口数量。
    pub fn interface_count(&self) -> usize {
        self.interfaces.read().len()
    }

    /// 检查指定接口是否存在且活跃。
    pub fn has_interface(&self, id: InterfaceId) -> bool {
        self.interfaces.read().contains_key(&id)
    }

    // ── Socket 创建 ──────────────────────────────────────────────────────

    /// 在默认接口上创建一个 TCP socket。
    ///
    /// 返回句柄用于后续操作。socket 初始处于 Closed 状态。
    /// 如果没有任何已注册接口，返回 `InterfaceNotFound`。
    pub fn socket_tcp(&self) -> Result<NetSocketHandle, NetError> {
        self.socket_tcp_on(self.default_iface_id()?)
    }

    /// 在指定接口上创建 TCP socket。
    pub fn socket_tcp_on(&self, iface_id: InterfaceId) -> Result<NetSocketHandle, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_tcp_socket(TCP_RX_BUF_SIZE, TCP_TX_BUF_SIZE);
        Ok(NetSocketHandle {
            iface_id,
            inner,
            sock_type: SocketType::Tcp,
        })
    }

    /// 在默认接口上创建一个 UDP socket。
    pub fn socket_udp(&self) -> Result<NetSocketHandle, NetError> {
        self.socket_udp_on(self.default_iface_id()?)
    }

    /// 在指定接口上创建 UDP socket。
    pub fn socket_udp_on(&self, iface_id: InterfaceId) -> Result<NetSocketHandle, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_udp_socket(
            UDP_RX_BUF_SIZE, UDP_TX_BUF_SIZE, UDP_META_COUNT, UDP_META_COUNT,
        );
        Ok(NetSocketHandle {
            iface_id,
            inner,
            sock_type: SocketType::Udp,
        })
    }

    // ── TCP 操作 ─────────────────────────────────────────────────────────

    /// TCP connect（非阻塞）。发起三次握手。
    ///
    /// 调用后 socket 进入 `Connecting` 状态，需要 poll 驱动握手完成。
    /// 上层用 `socket_state()` 轮询直到 `Established` 或超时。
    pub fn tcp_connect(&self, handle: NetSocketHandle, remote: Endpoint) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let remote_ep = endpoint_to_smoltcp(&remote);
        managed
            .tcp_connect(handle.inner, remote_ep, pick_ephemeral_port())
            .map_err(|_| NetError::ConnectionRefused)
    }

    /// TCP listen（开始监听）。
    pub fn tcp_listen(&self, handle: NetSocketHandle, local: Endpoint) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let socket = managed.tcp_socket_mut(handle.inner);
        let local_ep = endpoint_to_smoltcp(&local);
        socket.listen(local_ep).map_err(|_| NetError::AddressInUse)
    }

    /// TCP send（非阻塞）。尽可能多地发送数据。
    ///
    /// 返回实际写入发送缓冲区的字节数。`WouldBlock` 表示缓冲区满。
    pub fn tcp_send(&self, handle: NetSocketHandle, data: &[u8]) -> Result<usize, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let socket = managed.tcp_socket_mut(handle.inner);
        if !socket.may_send() {
            return Err(NetError::Closed);
        }
        socket.send_slice(data).map_err(|_| NetError::WouldBlock)
    }

    /// TCP recv（非阻塞）。
    ///
    /// 返回值语义（对齐 POSIX `read(2)`）：
    /// - `Ok(n)` where n > 0：成功读取 n 字节
    /// - `Ok(0)`：对端优雅关闭（EOF），不会再有数据
    /// - `Err(WouldBlock)`：当前无数据可读，稍后重试
    /// - `Err(ConnectionReset)`：连接被远端重置
    /// - `Err(Closed)`：socket 已关闭
    pub fn tcp_recv(
        &self,
        handle: NetSocketHandle,
        buf: &mut [u8],
    ) -> Result<usize, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        let socket = managed.tcp_socket_mut(handle.inner);
        match socket.recv_slice(buf) {
            Ok(0) => Ok(0),
            Ok(n) => Ok(n),
            Err(smoltcp::socket::tcp::RecvError::Finished) => Ok(0),
            Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                use smoltcp::socket::tcp::State;
                match socket.state() {
                    State::CloseWait | State::LastAck | State::TimeWait => Ok(0),
                    State::Closed => Err(NetError::Closed),
                    State::SynSent | State::SynReceived => Err(NetError::WouldBlock),
                    _ => Err(NetError::ConnectionReset),
                }
            }
        }
    }

    /// TCP close（发起优雅关闭）。
    pub fn tcp_close(&self, handle: NetSocketHandle) {
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            managed.tcp_socket_mut(handle.inner).close();
        }
    }

    // ── UDP 操作 ─────────────────────────────────────────────────────────

    /// UDP bind。绑定本地端口后可收发数据报。
    pub fn udp_bind(&self, handle: NetSocketHandle, local: Endpoint) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let socket = managed.udp_socket_mut(handle.inner);
        let local_ep = endpoint_to_smoltcp(&local);
        socket.bind(local_ep).map_err(|_| NetError::AddressInUse)
    }

    /// UDP sendto（非阻塞）。
    pub fn udp_send_to(
        &self,
        handle: NetSocketHandle,
        data: &[u8],
        remote: Endpoint,
    ) -> Result<usize, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let socket = managed.udp_socket_mut(handle.inner);
        let remote_ep = endpoint_to_smoltcp(&remote);
        socket
            .send_slice(data, remote_ep)
            .map(|()| data.len())
            .map_err(|_| NetError::WouldBlock)
    }

    /// UDP recvfrom（非阻塞）。
    pub fn udp_recv_from(
        &self,
        handle: NetSocketHandle,
        buf: &mut [u8],
    ) -> Result<(usize, Endpoint), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let socket = managed.udp_socket_mut(handle.inner);
        let (len, meta) = socket.recv_slice(buf).map_err(|_| NetError::WouldBlock)?;
        let remote = endpoint_from_smoltcp(meta.endpoint);
        Ok((len, remote))
    }

    /// UDP close。解绑并释放 socket。
    pub fn udp_close(&self, handle: NetSocketHandle) {
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            managed.udp_socket_mut(handle.inner).close();
        }
    }

    // ── Socket 释放 ──────────────────────────────────────────────────────

    /// Soft-close：标记 socket 为已移除。
    ///
    /// 不会立即从 SocketSet 移除——延迟到下一轮 poll 时清理。
    /// 这保证正在进行的并发操作（持有同一把锁的下一个获取者）看到
    /// `Closed` 错误而非 panic。
    pub fn socket_remove(&self, handle: NetSocketHandle) {
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let managed = iface_lock.lock();
            managed.soft_remove_socket(handle.inner);
        }
    }

    // ── Socket 就绪查询（为 poll/epoll 准备）──────────────────────────────

    /// 查询 socket 是否有数据可读。
    pub fn socket_can_recv(&self, handle: NetSocketHandle) -> bool {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else { return false };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) { return false; }
        match handle.sock_type {
            SocketType::Tcp => managed.tcp_socket(handle.inner).can_recv(),
            SocketType::Udp => managed.udp_socket(handle.inner).can_recv(),
        }
    }

    /// 查询 socket 是否可以发送数据。
    pub fn socket_can_send(&self, handle: NetSocketHandle) -> bool {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else { return false };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) { return false; }
        match handle.sock_type {
            SocketType::Tcp => managed.tcp_socket(handle.inner).can_send(),
            SocketType::Udp => managed.udp_socket(handle.inner).can_send(),
        }
    }

    // ── TCP accept ───────────────────────────────────────────────────────

    /// TCP accept（非阻塞）。
    ///
    /// 从 listen socket 同端口的 backlog 中找到已 Established 的连接，
    /// 返回新连接的 handle 并补位一个新的 listen socket。
    ///
    /// 返回 `WouldBlock` 表示没有待接受的连接。
    pub fn tcp_accept(&self, handle: NetSocketHandle) -> Result<NetSocketHandle, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        // 获取 listen socket 的本地端口
        let listen_port = managed.tcp_socket(handle.inner).listen_endpoint().port;
        // 检查 listen socket 自身是否已进入 Established（简单的单连接模式）
        let state = managed.tcp_socket(handle.inner).state();
        if state == smoltcp::socket::tcp::State::Established {
            // listen socket 本身接受了连接——创建新 socket 补位 listen
            let new_handle = managed.add_tcp_socket(TCP_RX_BUF_SIZE, TCP_TX_BUF_SIZE);
            let _ = managed.tcp_socket_mut(new_handle)
                .listen(listen_port)
                .ok();
            return Ok(NetSocketHandle {
                iface_id: handle.iface_id,
                inner: handle.inner,
                sock_type: SocketType::Tcp,
            });
        }
        Err(NetError::WouldBlock)
    }

    // ── 可配置缓冲区 ─────────────────────────────────────────────────────

    /// 创建 TCP socket 并指定 RX/TX 缓冲区大小。
    pub fn socket_tcp_with_bufs(
        &self,
        rx_size: usize,
        tx_size: usize,
    ) -> Result<NetSocketHandle, NetError> {
        let iface_id = self.default_iface_id()?;
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_tcp_socket(rx_size, tx_size);
        Ok(NetSocketHandle { iface_id, inner, sock_type: SocketType::Tcp })
    }

    /// 创建 UDP socket 并指定缓冲区大小。
    pub fn socket_udp_with_bufs(
        &self,
        rx_size: usize,
        tx_size: usize,
        meta_count: usize,
    ) -> Result<NetSocketHandle, NetError> {
        let iface_id = self.default_iface_id()?;
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_udp_socket(rx_size, tx_size, meta_count, meta_count);
        Ok(NetSocketHandle { iface_id, inner, sock_type: SocketType::Udp })
    }

    // ── Socket 状态查询 ──────────────────────────────────────────────────

    /// 查询 socket 当前状态。
    pub fn socket_state(&self, handle: NetSocketHandle) -> SocketState {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return SocketState::Closed;
        };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return SocketState::Closed;
        }
        match handle.sock_type {
            SocketType::Tcp => {
                let socket = managed.tcp_socket(handle.inner);
                tcp_state_to_socket_state(socket.state())
            }
            SocketType::Udp => {
                let socket = managed.udp_socket(handle.inner);
                if socket.is_open() {
                    SocketState::Established
                } else {
                    SocketState::Closed
                }
            }
        }
    }

    // ── 内部辅助 ─────────────────────────────────────────────────────────

    fn default_iface_id(&self) -> Result<InterfaceId, NetError> {
        let table = self.interfaces.read();
        table
            .keys()
            .next()
            .copied()
            .ok_or(NetError::InterfaceNotFound)
    }
}

// ── 辅助函数 ─────────────────────────────────────────────────────────────────

fn endpoint_to_smoltcp(ep: &Endpoint) -> IpEndpoint {
    let addr = match ep.addr {
        IpAddr::V4(v4) => IpAddress::Ipv4(Ipv4Address::new(v4.0[0], v4.0[1], v4.0[2], v4.0[3])),
        IpAddr::V6(v6) => {
            let o = &v6.0;
            IpAddress::Ipv6(Ipv6Address::new(
                u16::from_be_bytes([o[0], o[1]]),
                u16::from_be_bytes([o[2], o[3]]),
                u16::from_be_bytes([o[4], o[5]]),
                u16::from_be_bytes([o[6], o[7]]),
                u16::from_be_bytes([o[8], o[9]]),
                u16::from_be_bytes([o[10], o[11]]),
                u16::from_be_bytes([o[12], o[13]]),
                u16::from_be_bytes([o[14], o[15]]),
            ))
        }
    };
    IpEndpoint::new(addr, ep.port)
}

fn endpoint_from_smoltcp(ep: IpEndpoint) -> Endpoint {
    let addr = match ep.addr {
        IpAddress::Ipv4(v4) => {
            let o = v4.octets();
            IpAddr::V4(Ipv4Addr(o))
        }
        IpAddress::Ipv6(v6) => {
            IpAddr::V6(Ipv6Addr(v6.octets()))
        }
    };
    Endpoint { addr, port: ep.port }
}

fn tcp_state_to_socket_state(state: smoltcp::socket::tcp::State) -> SocketState {
    use smoltcp::socket::tcp::State as S;
    match state {
        S::Closed | S::TimeWait => SocketState::Closed,
        S::Listen => SocketState::Listen,
        S::SynSent | S::SynReceived => SocketState::Connecting,
        S::Established => SocketState::Established,
        S::FinWait1 | S::FinWait2 | S::Closing | S::CloseWait | S::LastAck => {
            SocketState::Closing
        }
    }
}

use core::sync::atomic::{AtomicU16, Ordering};

fn pick_ephemeral_port() -> u16 {
    static PORT: AtomicU16 = AtomicU16::new(49152);
    let p = PORT.fetch_add(1, Ordering::Relaxed);
    if p == 0 {
        PORT.store(49153, Ordering::Relaxed);
        49152
    } else {
        p
    }
}
