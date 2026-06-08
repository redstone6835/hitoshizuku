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
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};

use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address};
use spin::{Mutex, RwLock};

use crate::config::{CidrAddress, Endpoint, Gateway, IfConfig, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::device::{InterfaceId, NetDevice};
use crate::error::NetError;
use crate::interface::ManagedInterface;
use crate::socket::{NetSocketHandle, SocketState, SocketType};

// ── 常量 ─────────────────────────────────────────────────────────────────────

const TCP_RX_BUF_SIZE: usize = 65535;
const TCP_TX_BUF_SIZE: usize = 65535;
const UDP_RX_BUF_SIZE: usize = 512 * 1024;
const UDP_TX_BUF_SIZE: usize = 512 * 1024;
const UDP_META_COUNT: usize = 512;
/// 主动 poll 的短循环次数。
///
/// smoltcp 的一次 `Interface::poll()` 顺序是先处理 ingress，再处理 egress。
/// loopback 的 TX 会直接回灌到同一接口 RX 队列，因此 TCP 三次握手至少需要
/// 多轮 poll 才能把 SYN、SYN-ACK、ACK 全部从队列里读回并投递给 socket。
const ACTIVE_POLL_ROUNDS: usize = 64;

// ── 全局单例 ─────────────────────────────────────────────────────────────────

static STACK: NetStack = NetStack::new();

/// 获取全局网络协议栈实例。
pub fn stack() -> &'static NetStack {
    &STACK
}

/// TCP accept 的结果快照。
///
/// smoltcp 的监听 socket 会被原地转换为已连接 socket；VFS 接过 handle 后
/// 还要立即填充 accept/getpeername/getsockname 的 POSIX 地址信息。这里把
/// endpoint 在同一把接口锁内取出，避免稍后 socket 状态变化导致缓存为空。
#[derive(Debug, Clone, Copy)]
pub struct TcpAcceptInfo {
    pub accepted: NetSocketHandle,
    pub listener: NetSocketHandle,
    pub local: Option<Endpoint>,
    pub remote: Option<Endpoint>,
}

// ── NetStack ─────────────────────────────────────────────────────────────────

/// 全局网络协议栈。
pub struct NetStack {
    /// 接口注册表。读锁路径包含所有 I/O 操作，写锁仅 attach/detach 时持有。
    interfaces: RwLock<BTreeMap<InterfaceId, Arc<Mutex<ManagedInterface>>>>,
    /// 全局 socket 事件通知队列。
    ///
    /// 所有阻塞在 recv/send/accept/connect 的 `NetSocketFileOps` 在
    /// [`crate::vfs::net_socket::NetSocketFileOps::yield_wait`] 里
    /// 同时把自己挂到 (a) 自己文件级的 wait_queue 和 (b) 这个全局
    /// notify 队列。内核的 `poll()` 完成后调 [`Self::wake_socket_waiters`]
    /// 把队列里所有任务唤醒——任务醒来后会重新走一遍 `socket_can_recv` /
    /// `socket_state` 等检查，决定是否真的可读/可写/可 accept。
    ///
    /// 简单的"全唤醒"策略在大量任务阻塞时会有一次惊群，但内核场景
    /// （几十~几百个 task）完全可接受；比 smoltcp Waker 路径少一组
    /// RawWaker/VTable，零外部依赖。
    ///
    /// FIXME: 这里是粗粒度全局唤醒，不能区分具体 socket/事件类型；
    /// 高并发网络负载下会惊群，也会掩盖遗漏精确 readiness 通知的问题。
    notify_waiters: sched::WaitQueue,
}

impl NetStack {
    const fn new() -> Self {
        Self {
            interfaces: RwLock::new(BTreeMap::new()),
            notify_waiters: sched::WaitQueue::new(),
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
        table
            .remove(&id)
            .map(|_| ())
            .ok_or(NetError::InterfaceNotFound)
    }

    /// 驱动所有活跃接口进行一轮收发。
    ///
    /// 高强度 I/O 路径——零分配。读锁覆盖整轮 poll，期间无法 attach/detach
    /// （这是性能/一致性权衡：避免每次 poll 一次 `Vec` 分配）。
    ///
    /// 内核应周期性调用（定时器中断 / softirq / 网络线程）。
    ///
    /// 收尾会唤醒 [`Self::enqueue_socket_waiter`] 里所有挂着的任务
    /// （即所有阻塞在 recv/send/accept/connect 的用户进程），让它们重
    /// 新检查 socket 状态。
    pub fn poll(&self, timestamp: Instant) {
        let table = self.interfaces.read();
        for iface_lock in table.values() {
            if let Some(mut managed) = iface_lock.try_lock() {
                managed.poll(timestamp);
            }
        }
        drop(table);
        // FIXME: 当前每次协议栈 poll 都唤醒所有 socket 等待者，应该改为
        // 按 socket handle 和事件类型精确唤醒，避免 accept/connect/recv/send
        // 互相误唤醒。
        self.wake_socket_waiters();
    }

    /// 以毫秒时间戳驱动所有活跃接口——供 kernel timer tick 使用，
    /// 无需直接依赖 smoltcp。
    pub fn poll_ms(&self, millis: i64) {
        self.poll(Instant::from_millis(millis));
    }

    /// 立即驱动一次协议栈。
    ///
    /// 这用于 socket 操作刚刚排入出站数据之后主动推进 smoltcp 状态机。
    /// 尤其是 loopback：TX 会直接回灌到同一接口的 RX 队列，如果只等
    /// timer tick，阻塞 connect/accept/read 路径容易出现不必要的长等待。
    pub fn poll_now(&self) {
        let now_ns = sched::now_ns_public();
        let millis = (now_ns / 1_000_000) as i64;
        let table = self.interfaces.read();
        for _ in 0..ACTIVE_POLL_ROUNDS {
            for iface_lock in table.values() {
                iface_lock.lock().poll(Instant::from_millis(millis));
            }
            self.wake_socket_waiters();
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
        drop(table);
        // FIXME: 中断快速路径也走全局唤醒，多个接口/多个 socket 时会把
        // 与本接口无关的等待任务一并唤醒。
        self.wake_socket_waiters();
    }

    /// 以毫秒时间戳 poll 指定接口。
    ///
    /// 设备驱动层不应该直接暴露或依赖 smoltcp 的时间类型；中断处理路径只需
    /// 传入调度器/时钟层提供的单调毫秒值，由协议栈内部完成类型转换。
    pub fn poll_interface_ms(&self, id: InterfaceId, millis: i64) {
        self.poll_interface(id, Instant::from_millis(millis));
    }

    /// 把当前任务挂到全局 socket 事件通知队列。
    ///
    /// 阻塞在 `recv/send/accept/connect` 的任务在
    /// [`crate::vfs::net_socket::NetSocketFileOps`] 里调 `yield_wait` 时
    /// 同时把自己挂到文件级 wait_queue 和本队列；下一次
    /// [`Self::poll`] 完成后所有挂在本队列上的任务都会被唤醒，醒来后
    /// 重新检查 socket 状态以决定是否真的可读/可写/可 accept。
    pub fn enqueue_socket_waiter(&self, task: &alloc::sync::Arc<sched::Task>) {
        self.notify_waiters.enqueue(task);
    }

    /// 唤醒所有等待 socket 事件的任务。
    ///
    /// 由 [`Self::poll`] / [`Self::poll_interface`] 在收尾自动调用——
    /// 上层无需手动调。直接调用是合法的（例如内核想强制一次"全部任务
    /// 重检"），但常态下不要。
    pub fn wake_socket_waiters(&self) {
        let had_waiters = self.notify_waiters.len_hint() != 0;
        // wake_all 已经把 task 转到 runnable 并标记 NEED_RESCHED。
        self.notify_waiters.wake_all();
        if had_waiters {
            // 网络事件常在 timer poll 或 socket 快速路径里发生；显式请求
            // 当前 CPU 重调度，保证刚唤醒的 accept/recv/send 对端不会等到
            // 其它 unrelated timer sleeper 才获得运行机会。
            sched::request_resched(sched::current_cpu_id());
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
    /// FIXME: 默认接口当前由 `default_iface_id()` 取注册表第一个条目，
    /// 没有根据目标地址、bind 地址或路由表选接口。
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
            UDP_RX_BUF_SIZE,
            UDP_TX_BUF_SIZE,
            UDP_META_COUNT,
            UDP_META_COUNT,
        );
        Ok(NetSocketHandle {
            iface_id,
            inner,
            sock_type: SocketType::Udp,
        })
    }

    /// 创建一个 raw IP socket（指定 IP 协议号）。
    pub fn socket_raw(&self, ip_version: u8, protocol: u8) -> Result<NetSocketHandle, NetError> {
        let iface_id = self.default_iface_id()?;
        self.socket_raw_on(iface_id, ip_version, protocol)
    }

    /// 在指定接口上创建 raw IP socket。
    pub fn socket_raw_on(
        &self,
        iface_id: InterfaceId,
        ip_version: u8,
        protocol: u8,
    ) -> Result<NetSocketHandle, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_raw_socket(ip_version, protocol);
        Ok(NetSocketHandle {
            iface_id,
            inner,
            sock_type: SocketType::Raw,
        })
    }

    /// 创建一个 ICMP socket。
    pub fn socket_icmp(&self) -> Result<NetSocketHandle, NetError> {
        let iface_id = self.default_iface_id()?;
        self.socket_icmp_on(iface_id)
    }

    /// 在指定接口上创建 ICMP socket。
    pub fn socket_icmp_on(&self, iface_id: InterfaceId) -> Result<NetSocketHandle, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_icmp_socket();
        Ok(NetSocketHandle {
            iface_id,
            inner,
            sock_type: SocketType::Icmp,
        })
    }

    // ── Raw / ICMP 操作 ─────────────────────────────────────────────────

    pub fn raw_send(
        &self,
        handle: NetSocketHandle,
        data: &[u8],
        remote: Option<Endpoint>,
    ) -> Result<usize, NetError> {
        let sent = {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();
            if managed.is_socket_removed(handle.inner) {
                return Err(NetError::Closed);
            }
            match handle.sock_type {
                SocketType::Raw => {
                    // TODO: raw IP 发送目前忽略 remote，缺少 per-packet 目的地址、
                    // 路由元信息和 raw socket 头部包含语义。
                    let socket = managed.raw_socket_mut(handle.inner);
                    let tx_buf = socket.send(data.len()).map_err(|_| NetError::WouldBlock)?;
                    tx_buf.copy_from_slice(data);
                    data.len()
                }
                SocketType::Icmp => {
                    let remote = remote.ok_or(NetError::InvalidArgument)?;
                    // TODO: ICMP 仅把 remote addr 传给 smoltcp，尚未支持 identifier
                    // 绑定、IPv6 ICMP 细分语义和 socket 级过滤。
                    let socket = managed.icmp_socket_mut(handle.inner);
                    socket
                        .send_slice(data, endpoint_to_smoltcp(&remote).addr)
                        .map_err(|err| match err {
                            smoltcp::socket::icmp::SendError::BufferFull => NetError::WouldBlock,
                            smoltcp::socket::icmp::SendError::Unaddressable => {
                                NetError::InvalidArgument
                            }
                        })?;
                    data.len()
                }
                _ => return Err(NetError::InvalidArgument),
            }
        };
        self.poll_now();
        Ok(sent)
    }

    pub fn raw_recv(&self, handle: NetSocketHandle, buf: &mut [u8]) -> Result<usize, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        match handle.sock_type {
            SocketType::Raw => {
                let socket = managed.raw_socket_mut(handle.inner);
                let data = socket.recv().map_err(|_| NetError::WouldBlock)?;
                // FIXME: raw::Socket::recv() 的来源/接口元信息在这里被丢弃，
                // VFS recvfrom 无法返回 peer 地址。
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
            SocketType::Icmp => {
                let socket = managed.icmp_socket_mut(handle.inner);
                let (data, _) = socket.recv().map_err(|_| NetError::WouldBlock)?;
                // FIXME: ICMP recv 丢弃 endpoint，导致 recvfrom 只能返回 None。
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
            _ => Err(NetError::InvalidArgument),
        }
    }

    pub fn raw_can_recv(&self, handle: NetSocketHandle) -> bool {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return false;
        };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return false;
        }
        match handle.sock_type {
            SocketType::Raw => managed.raw_socket(handle.inner).can_recv(),
            SocketType::Icmp => managed.icmp_socket(handle.inner).can_recv(),
            _ => false,
        }
    }

    pub fn raw_can_send(&self, handle: NetSocketHandle) -> bool {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return false;
        };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return false;
        }
        match handle.sock_type {
            SocketType::Raw => managed.raw_socket(handle.inner).can_send(),
            SocketType::Icmp => managed.icmp_socket(handle.inner).can_send(),
            _ => false,
        }
    }

    // ── TCP 操作 ─────────────────────────────────────────────────────────

    /// TCP connect（非阻塞）。发起三次握手。
    ///
    /// 调用后 socket 进入 `Connecting` 状态，需要 poll 驱动握手完成。
    /// 上层用 `socket_state()` 轮询直到 `Established` 或超时。
    pub fn tcp_connect(&self, handle: NetSocketHandle, remote: Endpoint) -> Result<(), NetError> {
        // FIXME: connect 不做路由选择，也没有处理本地地址/端口冲突；
        // ephemeral port 只是全局递增，可能与已有连接或监听端口碰撞。
        if handle.sock_type != SocketType::Tcp {
            return Err(NetError::InvalidArgument);
        }
        {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();
            if managed.is_socket_removed(handle.inner) {
                return Err(NetError::Closed);
            }
            let remote_ep = endpoint_to_smoltcp(&remote);
            let local_port = pick_ephemeral_port();
            managed
                .tcp_connect(handle.inner, remote_ep, local_port)
                .map_err(|_| NetError::ConnectionRefused)?;
        }
        self.poll_now();
        Ok(())
    }

    /// TCP listen（开始监听）。
    pub fn tcp_listen(
        &self,
        handle: NetSocketHandle,
        mut local: Endpoint,
    ) -> Result<Endpoint, NetError> {
        if handle.sock_type != SocketType::Tcp {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        if local.port != 0 {
            let local_ep = endpoint_to_smoltcp_listen(&local);
            if managed.tcp_listen_endpoint_in_use(handle.inner, local_ep) {
                return Err(NetError::AddressInUse);
            }
            let socket = managed.tcp_socket_mut(handle.inner);
            socket
                .listen(local_ep)
                .map_err(|_| NetError::AddressInUse)?;
            return Ok(local);
        }

        // POSIX 语义要求 bind(port=0)+listen 自动分配临时端口。smoltcp
        // 明确拒绝监听 0 端口；netperf TCP_STREAM 的服务端数据 socket
        // 依赖 getsockname 读回这个自动端口，再通过控制连接通知客户端。
        for _ in 0..128 {
            local.port = pick_ephemeral_port();
            let local_ep = endpoint_to_smoltcp_listen(&local);
            if managed.tcp_listen_endpoint_in_use(handle.inner, local_ep) {
                continue;
            }
            let socket = managed.tcp_socket_mut(handle.inner);
            if socket.listen(local_ep).is_ok() {
                return Ok(local);
            }
        }
        Err(NetError::AddressInUse)
    }

    /// TCP send（非阻塞）。尽可能多地发送数据。
    ///
    /// 返回实际写入发送缓冲区的字节数。`WouldBlock` 表示缓冲区满。
    pub fn tcp_send(&self, handle: NetSocketHandle, data: &[u8]) -> Result<usize, NetError> {
        if handle.sock_type != SocketType::Tcp {
            return Err(NetError::InvalidArgument);
        }
        let sent = {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();
            if managed.is_socket_removed(handle.inner) {
                return Err(NetError::Closed);
            }
            let socket = managed.tcp_socket_mut(handle.inner);
            if !socket.may_send() {
                return Err(NetError::Closed);
            }
            let n = socket.send_slice(data).map_err(|_| NetError::WouldBlock)?;
            if n == 0 && !data.is_empty() {
                return Err(NetError::WouldBlock);
            }
            n
        };
        self.poll_now();
        Ok(sent)
    }

    /// TCP recv（非阻塞）。
    ///
    /// 返回值语义（对齐 POSIX `read(2)`）：
    /// - `Ok(n)` where n > 0：成功读取 n 字节
    /// - `Ok(0)`：对端优雅关闭（EOF），不会再有数据
    /// - `Err(WouldBlock)`：当前无数据可读，稍后重试
    /// - `Err(ConnectionReset)`：连接被远端重置
    /// - `Err(Closed)`：socket 已关闭
    pub fn tcp_recv(&self, handle: NetSocketHandle, buf: &mut [u8]) -> Result<usize, NetError> {
        if handle.sock_type != SocketType::Tcp {
            return Err(NetError::InvalidArgument);
        }
        self.poll_now();
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        let socket = managed.tcp_socket_mut(handle.inner);
        match socket.recv_slice(buf) {
            Ok(0) if buf.is_empty() => Ok(0),
            Ok(0) if socket.may_recv() => Err(NetError::WouldBlock),
            Ok(0) => Ok(0),
            Ok(n) => Ok(n),
            Err(smoltcp::socket::tcp::RecvError::Finished) => Ok(0),
            Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                use smoltcp::socket::tcp::State;
                match socket.state() {
                    State::CloseWait | State::Closing | State::LastAck | State::TimeWait => Ok(0),
                    State::Closed => Err(NetError::Closed),
                    State::SynSent | State::SynReceived => Err(NetError::WouldBlock),
                    _ => Err(NetError::ConnectionReset),
                }
            }
        }
    }

    /// TCP close（发起优雅关闭）。
    pub fn tcp_close(&self, handle: NetSocketHandle) {
        if handle.sock_type != SocketType::Tcp {
            return;
        }
        {
            let table = self.interfaces.read();
            if let Some(iface_lock) = table.get(&handle.iface_id) {
                let mut managed = iface_lock.lock();
                if !managed.is_socket_removed(handle.inner) {
                    managed.tcp_socket_mut(handle.inner).close();
                }
            }
        }
        self.poll_now();
    }

    // ── TCP 选项与状态查询 (供 socket option 路径使用) ────────────────────

    /// 设置 TCP_NODELAY（禁用/启用 Nagle 算法）。
    pub fn tcp_set_nodelay(&self, handle: NetSocketHandle, nodelay: bool) {
        if handle.sock_type != SocketType::Tcp {
            return;
        }
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            if !managed.is_socket_removed(handle.inner) {
                managed
                    .tcp_socket_mut(handle.inner)
                    .set_nagle_enabled(!nodelay);
            }
        }
    }

    /// 设置 TCP keep-alive 间隔（秒，0 表示禁用）。
    pub fn tcp_set_keepalive(&self, handle: NetSocketHandle, secs: u64) {
        if handle.sock_type != SocketType::Tcp {
            return;
        }
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            let interval = if secs == 0 {
                None
            } else {
                Some(smoltcp::time::Duration::from_secs(secs))
            };
            if !managed.is_socket_removed(handle.inner) {
                managed
                    .tcp_socket_mut(handle.inner)
                    .set_keep_alive(interval);
            }
        }
    }

    /// 设置 TCP idle abort 超时（秒，0 表示禁用）。
    pub fn tcp_set_timeout(&self, handle: NetSocketHandle, secs: u64) {
        if handle.sock_type != SocketType::Tcp {
            return;
        }
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            let timeout = if secs == 0 {
                None
            } else {
                Some(smoltcp::time::Duration::from_secs(secs))
            };
            if !managed.is_socket_removed(handle.inner) {
                managed.tcp_socket_mut(handle.inner).set_timeout(timeout);
            }
        }
    }

    /// 设置 TCP 出站 hop limit（IPv4 TTL / IPv6 hop limit）。
    pub fn tcp_set_hop_limit(&self, handle: NetSocketHandle, ttl: Option<u8>) {
        if handle.sock_type != SocketType::Tcp {
            return;
        }
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            if !managed.is_socket_removed(handle.inner) {
                managed.tcp_socket_mut(handle.inner).set_hop_limit(ttl);
            }
        }
    }

    /// 查询 TCP 接收缓冲区可读字节数（FIONREAD）。
    pub fn tcp_recv_queue(&self, handle: NetSocketHandle) -> usize {
        if handle.sock_type != SocketType::Tcp {
            return 0;
        }
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return 0;
        };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return 0;
        }
        managed.tcp_socket(handle.inner).recv_queue()
    }

    /// 查询 TCP 发送缓冲区已排队字节数。
    pub fn tcp_send_queue(&self, handle: NetSocketHandle) -> usize {
        if handle.sock_type != SocketType::Tcp {
            return 0;
        }
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return 0;
        };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return 0;
        }
        managed.tcp_socket(handle.inner).send_queue()
    }

    /// TCP peek（窥视，不消费数据）。
    pub fn tcp_peek(&self, handle: NetSocketHandle, buf: &mut [u8]) -> Result<usize, NetError> {
        if handle.sock_type != SocketType::Tcp {
            return Err(NetError::InvalidArgument);
        }
        self.poll_now();
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        let socket = managed.tcp_socket_mut(handle.inner);
        match socket.peek_slice(buf) {
            Ok(0) if buf.is_empty() => Ok(0),
            Ok(0) if socket.may_recv() => Err(NetError::WouldBlock),
            Ok(n) => Ok(n),
            Err(_) => Err(NetError::WouldBlock),
        }
    }

    /// 查询 TCP socket 的本地端点（getsockname 真实值）。
    pub fn tcp_local_endpoint(&self, handle: NetSocketHandle) -> Option<Endpoint> {
        if handle.sock_type != SocketType::Tcp {
            return None;
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id)?;
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return None;
        }
        let ep = managed.tcp_socket(handle.inner).local_endpoint()?;
        Some(endpoint_from_smoltcp(ep))
    }

    /// 查询 TCP socket 的远端端点（getpeername 真实值）。
    pub fn tcp_remote_endpoint(&self, handle: NetSocketHandle) -> Option<Endpoint> {
        if handle.sock_type != SocketType::Tcp {
            return None;
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id)?;
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return None;
        }
        let ep = managed.tcp_socket(handle.inner).remote_endpoint()?;
        Some(endpoint_from_smoltcp(ep))
    }

    /// 设置 UDP 出站 hop limit。
    pub fn udp_set_hop_limit(&self, handle: NetSocketHandle, ttl: Option<u8>) {
        if handle.sock_type != SocketType::Udp {
            return;
        }
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            if !managed.is_socket_removed(handle.inner) {
                managed.udp_socket_mut(handle.inner).set_hop_limit(ttl);
            }
        }
    }

    /// UDP peek（窥视一个数据报，不消费）。
    pub fn udp_peek_from(
        &self,
        handle: NetSocketHandle,
        buf: &mut [u8],
    ) -> Result<(usize, Endpoint), NetError> {
        if handle.sock_type != SocketType::Udp {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        let socket = managed.udp_socket_mut(handle.inner);
        let (n, meta) = socket.peek_slice(buf).map_err(|_| NetError::WouldBlock)?;
        Ok((n, endpoint_from_smoltcp(meta.endpoint)))
    }

    // ── 路由管理 ─────────────────────────────────────────────────────────

    /// 添加默认 IPv4 路由（gateway）到指定接口。
    pub fn add_default_route_v4(
        &self,
        iface_id: InterfaceId,
        gateway: crate::Ipv4Addr,
    ) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.add_default_route_v4(gateway);
        Ok(())
    }

    /// 移除指定接口上的默认 IPv4 路由。
    pub fn remove_default_route_v4(&self, iface_id: InterfaceId) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.remove_default_route_v4();
        Ok(())
    }

    // ── 运行时配置（供 ioctl / netlink 写操作使用）──────────────────────

    /// 设置指定接口的 IPv4 地址。
    pub fn set_iface_ipv4_addr(
        &self,
        id: InterfaceId,
        addr: crate::Ipv4Addr,
        prefix: u8,
    ) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.set_ipv4_addr(addr, prefix);
        Ok(())
    }

    /// 设置指定接口的管理态 UP/DOWN。
    ///
    /// 这只表达“协议栈是否应使用该接口”，不销毁底层设备，也不参与驱动
    /// probe/remove 生命周期。
    pub fn set_iface_admin_up(&self, id: InterfaceId, up: bool) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.set_admin_up(up);
        Ok(())
    }

    /// 设置指定接口的运行期 MTU。
    ///
    /// 这是协议栈侧的软件 MTU 限制，不能超过底层设备声明的硬件上限。
    pub fn set_iface_mtu(&self, id: InterfaceId, mtu: usize) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.set_mtu(mtu)
    }

    /// 在指定接口上添加 IPv4 路由。
    pub fn add_route(
        &self,
        id: InterfaceId,
        dest: crate::Ipv4Addr,
        mask: crate::Ipv4Addr,
        gw: crate::Ipv4Addr,
    ) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.add_route_v4(dest, mask, gw);
        Ok(())
    }

    /// 在指定接口上删除 IPv4 路由。
    pub fn remove_route(
        &self,
        id: InterfaceId,
        dest: crate::Ipv4Addr,
        mask: crate::Ipv4Addr,
    ) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.remove_route_v4(dest, mask);
        Ok(())
    }

    // ── 邻居表查询 ───────────────────────────────────────────────────────

    /// 查询指定接口的 ARP/NDP 邻居表。
    pub fn neighbor_table(&self, iface_id: InterfaceId) -> Result<Vec<NeighborEntry>, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let managed = iface_lock.lock();
        Ok(managed.neighbor_entries())
    }

    /// 查询所有接口的邻居表。
    pub fn all_neighbors(&self) -> Vec<(InterfaceId, Vec<NeighborEntry>)> {
        let table = self.interfaces.read();
        let mut out = Vec::new();
        for (&id, iface_lock) in table.iter() {
            let managed = iface_lock.lock();
            let entries = managed.neighbor_entries();
            if !entries.is_empty() {
                out.push((id, entries));
            }
        }
        out
    }

    // ── UDP 操作 ─────────────────────────────────────────────────────────

    /// UDP bind。绑定本地端口后可收发数据报。
    pub fn udp_bind(
        &self,
        handle: NetSocketHandle,
        mut local: Endpoint,
    ) -> Result<Endpoint, NetError> {
        if handle.sock_type != SocketType::Udp {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        let socket = managed.udp_socket_mut(handle.inner);
        if local.port != 0 {
            let local_ep = endpoint_to_smoltcp_listen(&local);
            socket.bind(local_ep).map_err(|_| NetError::AddressInUse)?;
            return Ok(local);
        }

        // POSIX 语义要求 bind(port=0) 自动分配临时端口。smoltcp 会把
        // port=0 视为不可寻址，因此这里在进入 smoltcp 前完成端口选择。
        // 当前 smoltcp UDP socket 自身不做全局端口冲突检测；这里保留多次
        // 尝试结构，便于后续接入端口占用表时直接复用。
        for _ in 0..128 {
            local.port = pick_ephemeral_port();
            let local_ep = endpoint_to_smoltcp_listen(&local);
            if socket.bind(local_ep).is_ok() {
                return Ok(local);
            }
        }
        Err(NetError::AddressInUse)
    }

    /// UDP sendto（非阻塞）。
    pub fn udp_send_to(
        &self,
        handle: NetSocketHandle,
        data: &[u8],
        remote: Endpoint,
    ) -> Result<usize, NetError> {
        // FIXME: UDP 发送沿用 socket 创建时的接口，缺少按 remote 路由选路
        // 和 connected UDP 的协议栈状态校验。
        if handle.sock_type != SocketType::Udp {
            return Err(NetError::InvalidArgument);
        }
        {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();
            if managed.is_socket_removed(handle.inner) {
                return Err(NetError::Closed);
            }
            let socket = managed.udp_socket_mut(handle.inner);
            if socket.endpoint().port == 0 {
                let local_ep = IpListenEndpoint {
                    addr: None,
                    port: pick_ephemeral_port(),
                };
                socket.bind(local_ep).map_err(|_| NetError::AddressInUse)?;
            }
            let remote_ep = endpoint_to_smoltcp(&remote);
            socket
                .send_slice(data, remote_ep)
                .map_err(|_| NetError::WouldBlock)?;
        }
        self.poll_now();
        Ok(data.len())
    }

    /// UDP recvfrom（非阻塞）。
    pub fn udp_recv_from(
        &self,
        handle: NetSocketHandle,
        buf: &mut [u8],
    ) -> Result<(usize, Endpoint), NetError> {
        if handle.sock_type != SocketType::Udp {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return Err(NetError::Closed);
        }
        let socket = managed.udp_socket_mut(handle.inner);
        let (len, meta) = socket.recv_slice(buf).map_err(|_| NetError::WouldBlock)?;
        let remote = endpoint_from_smoltcp(meta.endpoint);
        Ok((len, remote))
    }

    /// 查询 UDP socket 的本地端点（getsockname 真实值）。
    pub fn udp_local_endpoint(&self, handle: NetSocketHandle) -> Option<Endpoint> {
        if handle.sock_type != SocketType::Udp {
            return None;
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id)?;
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return None;
        }
        let ep = managed.udp_socket(handle.inner).endpoint();
        if ep.port == 0 {
            return None;
        }
        let addr = match ep.addr {
            Some(IpAddress::Ipv4(v4)) => IpAddr::V4(Ipv4Addr(v4.octets())),
            Some(IpAddress::Ipv6(v6)) => IpAddr::V6(Ipv6Addr(v6.octets())),
            None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };
        Some(Endpoint {
            addr,
            port: ep.port,
        })
    }

    /// UDP close。解绑并释放 socket。
    pub fn udp_close(&self, handle: NetSocketHandle) {
        if handle.sock_type != SocketType::Udp {
            return;
        }
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            if !managed.is_socket_removed(handle.inner) {
                managed.udp_socket_mut(handle.inner).close();
            }
        }
    }

    // ── Socket 释放 ──────────────────────────────────────────────────────

    /// Soft-close：标记 socket 为已移除。
    ///
    /// **不会立即从 SocketSet 移除**——延迟到下一轮 poll 时清理。
    /// 这保证正在进行的并发操作（持有同一把锁的下一个获取者）看到
    /// `Closed` 错误而非 panic。
    ///
    /// **新代码请优先用 [`Self::socket_close_and_remove`]**——它会同步
    /// 从 SocketSet 摘掉 socket，避免同 index 被后续新建的其它类型
    /// socket 占用。
    pub fn socket_remove(&self, handle: NetSocketHandle) {
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let managed = iface_lock.lock();
            managed.soft_remove_socket(handle.inner);
        }
    }

    /// 关闭并立即从 SocketSet 移除一个 socket。
    ///
    /// 这是文件描述符 `release` 路径应当使用的入口。`socket_remove` 走
    /// soft-remove 必须等下次 `poll` 才真正释放——但本内核里 poll 触发是
    /// 异步的，且 smoltcp 自己也只用 `is_removed` 标记做软引用——一旦同
    /// index 被 `add` 重复插入新类型 socket，旧 handle 的下转就会失败。
    /// 因此"先关 + 立即摘掉"是唯一安全选择。
    ///
    /// 调用者需要保证此刻没有其它代码路径仍持有该 handle 的引用（典型
    /// 场景是文件 fd 即将被 drop、`NetSocketFileOps::handle` 已被 take）。
    pub fn socket_close_and_remove(&self, handle: NetSocketHandle) {
        // FIXME: 这里把"没有其它并发 handle 使用者"作为调用方约束，
        // 网络层本身没有引用计数或 generation 校验来防止旧 handle 误用。
        // 关：让 smoltcp 把内部状态重置（TCP: Closed；UDP: 清 endpoint；RAW/ICMP:
        // release rx/tx buffer）。即使 socket 即将被删，也走一遍以释放 driver
        // 持有的 DMA / 缓冲区。
        match handle.sock_type {
            SocketType::Tcp => {
                let table = self.interfaces.read();
                if let Some(iface_lock) = table.get(&handle.iface_id) {
                    let mut managed = iface_lock.lock();
                    if managed.has_socket(handle.inner) && !managed.is_socket_removed(handle.inner)
                    {
                        managed.tcp_socket_mut(handle.inner).close();
                    }
                }
            }
            SocketType::Udp => {
                let table = self.interfaces.read();
                if let Some(iface_lock) = table.get(&handle.iface_id) {
                    let mut managed = iface_lock.lock();
                    if managed.has_socket(handle.inner) && !managed.is_socket_removed(handle.inner)
                    {
                        managed.udp_socket_mut(handle.inner).close();
                    }
                }
            }
            SocketType::Raw | SocketType::Icmp => {
                // raw/icmp 没有语义意义上的 close，直接走 remove 释放 buffer。
            }
        }
        if matches!(handle.sock_type, SocketType::Tcp) {
            // TCP close 只是把状态机切到发 FIN 的状态；如果立刻摘掉 socket，
            // 对端永远看不到 EOF。移除前主动 poll，至少把当前 FIN/已排队数据
            // 推到设备队列里，避免 netperf 这类控制连接收尾永久等待。
            self.poll_now();
        }
        // 真正从 SocketSet 摘掉
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            if managed.has_socket(handle.inner) {
                managed.remove_socket_locked(handle.inner);
            }
        }
    }

    /// 关闭 fd 持有的 socket，但让 TCP 在后台完成协议收尾。
    ///
    /// TCP_STREAM/netperf 这类程序在数据 fd close 后，还会等服务端从数据
    /// 连接读到 EOF，再通过控制连接回传结果。如果这里像普通资源一样立刻
    /// 从 SocketSet 移除 TCP socket，尚未发完的尾部数据或 FIN 会直接丢失，
    /// 对端就会永久阻塞在 read/recv。detach 后旧 fd 已不可再访问，但
    /// smoltcp 仍会在 timer/主动 poll 中推进 FIN/ACK，最终由接口 poll 回收。
    pub fn socket_close_detach(&self, handle: NetSocketHandle) {
        if !matches!(handle.sock_type, SocketType::Tcp) {
            self.socket_close_and_remove(handle);
            return;
        }

        {
            let table = self.interfaces.read();
            if let Some(iface_lock) = table.get(&handle.iface_id) {
                let mut managed = iface_lock.lock();
                if managed.has_socket(handle.inner) && !managed.is_socket_removed(handle.inner) {
                    managed.tcp_socket_mut(handle.inner).close();
                    managed.orphan_socket(handle.inner);
                }
            }
        }
        self.poll_now();
    }

    // ── Socket 就绪查询（为 poll/epoll 准备）──────────────────────────────

    /// 查询 socket 是否有数据可读。
    pub fn socket_can_recv(&self, handle: NetSocketHandle) -> bool {
        // TODO: readiness 当前仍缺少精确 socket 事件订阅、UDP datagram 长度、
        // raw/icmp 元信息等 POSIX poll 语义；TCP EOF 先在这里补齐。
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return false;
        };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return false;
        }
        match handle.sock_type {
            SocketType::Tcp => {
                let socket = managed.tcp_socket(handle.inner);
                socket.can_recv() || tcp_state_is_read_eof(socket.state())
            }
            SocketType::Udp => managed.udp_socket(handle.inner).can_recv(),
            SocketType::Raw => managed.raw_socket(handle.inner).can_recv(),
            SocketType::Icmp => managed.icmp_socket(handle.inner).can_recv(),
        }
    }

    /// 查询 socket 是否可以发送数据。
    pub fn socket_can_send(&self, handle: NetSocketHandle) -> bool {
        // TODO: can_send 只表示内部发送缓冲区可写，不等价于 connect 完成、
        // 对端仍存活或错误队列为空。
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return false;
        };
        let managed = iface_lock.lock();
        if managed.is_socket_removed(handle.inner) {
            return false;
        }
        match handle.sock_type {
            SocketType::Tcp => managed.tcp_socket(handle.inner).can_send(),
            SocketType::Udp => managed.udp_socket(handle.inner).can_send(),
            SocketType::Raw => managed.raw_socket(handle.inner).can_send(),
            SocketType::Icmp => managed.icmp_socket(handle.inner).can_send(),
        }
    }

    // ── TCP accept ───────────────────────────────────────────────────────

    /// TCP accept（非阻塞）。
    ///
    /// 解决原实现的两个 bug：
    ///
    /// 1. **原实现复用 listen socket 的 `inner` 作为新连接的 handle**——
    ///    smoltcp 把 listen socket 自己转成 Established，原 listen socket
    ///    文件描述符的 `NetSocketFileOps::handle` 还指向老 index，但该
    ///    index 已经是"已建立连接"了。用户再次 `accept` 会被内核误判
    ///    为"listen socket 第二次进入 Established"，于是又新建一个
    ///    listen socket 替换；累积下来 listen socket 飘走、fd 上的
    ///    handle 完全错位。
    /// 2. **listen socket 自身从 Listen 状态被吃掉**——smoltcp 没有把
    ///    listen 与已连接分离的 API，必须靠"接受一次就把 listen socket
    ///    让出去、新建一个 listen socket 顶替"的模式；为此
    ///    `NetSocketFileOps` 侧需要**总是拿到当前真正的 listen handle**。
    ///
    /// 本函数新行为：
    ///
    /// - 检查传入 handle 指向的 socket 是否已经处于 Established；
    /// - 若是：保留这个已经 Established 的老 socket，直接把它作为 accepted
    ///   connection 返回；
    /// - 同时新建一个 socket 顶替 listen 角色，并把新的 listen handle 返回
    ///   给调用方，由 VFS 层直接替换原 listen fd 持有的 handle。
    ///
    /// FIXME: 这是 smoltcp listen socket 被连接原地转换后的补救模式，
    /// 没有真正 backlog 队列；accept 和重新 listen 之间存在监听空窗。
    pub fn tcp_accept(
        &self,
        handle: NetSocketHandle,
        local_hint: Option<Endpoint>,
    ) -> Result<TcpAcceptInfo, NetError> {
        if handle.sock_type != SocketType::Tcp {
            return Err(NetError::InvalidArgument);
        }
        // 先短持锁查找 pending 连接——检查完立即释放，再走
        // accept_in_place 路径（后者会自己重新加锁）。**绝不能**持锁跨调
        // accept_in_place，否则会触发 `Mutex` 的递归 deadlock。
        let pending = {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let managed = iface_lock.lock();
            let target = if let Some(local) = local_hint {
                endpoint_to_smoltcp_listen(&local)
            } else if managed.has_socket(handle.inner) && !managed.is_socket_removed(handle.inner) {
                managed.tcp_socket(handle.inner).listen_endpoint()
            } else {
                return Err(NetError::WouldBlock);
            };
            managed
                .pending_tcp_accept(handle.inner, target)
                .map(|inner| NetSocketHandle {
                    iface_id: handle.iface_id,
                    inner,
                    sock_type: SocketType::Tcp,
                })
        };
        if let Some(pending) = pending {
            // smoltcp 把被命中的 listen socket 原地转成已连接；accept 时把
            // 这条连接交给用户，再另起一个 listener 顶替同一端点。
            return self.accept_in_place_connection(pending);
        }
        Err(NetError::WouldBlock)
    }

    /// 检查指定监听 fd 是否已有待 accept 的 TCP 连接。
    pub fn tcp_has_pending_accept(
        &self,
        handle: NetSocketHandle,
        local_hint: Option<Endpoint>,
    ) -> bool {
        if handle.sock_type != SocketType::Tcp {
            return false;
        }
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return false;
        };
        let managed = iface_lock.lock();
        let target = if let Some(local) = local_hint {
            endpoint_to_smoltcp_listen(&local)
        } else if managed.has_socket(handle.inner) && !managed.is_socket_removed(handle.inner) {
            managed.tcp_socket(handle.inner).listen_endpoint()
        } else {
            return false;
        };
        managed.pending_tcp_accept(handle.inner, target).is_some()
    }

    /// 接受一条已经被 smoltcp 装到 listen socket 上的连接：
    /// 1. 保留原 listen socket——它现在就是已建立的连接本体；
    /// 2. 另起一个 listen socket 顶替（重新 listen 同端口）；
    /// 3. 返回 `(accepted, new_listen)` 给上层，让上层把监听 fd 直接切到新
    ///    listen handle。
    fn accept_in_place_connection(
        &self,
        listen_handle: NetSocketHandle,
    ) -> Result<TcpAcceptInfo, NetError> {
        // FIXME: 替换监听 socket 不是原子化对外可见的 accept 队列模型；
        // 多个 accept 任务并发时仍依赖上层 handle 交换顺序保持一致。
        let info = {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&listen_handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();

            // 重新检查状态（前面 tcp_accept 的状态检查完就释放了锁，到
            // 这里可能已经被其它 accept 处理掉）。
            if managed.is_socket_removed(listen_handle.inner) {
                return Err(NetError::WouldBlock);
            }
            if managed.tcp_socket(listen_handle.inner).state()
                != smoltcp::socket::tcp::State::Established
            {
                return Err(NetError::WouldBlock);
            }

            let listen_endpoint = managed.tcp_socket(listen_handle.inner).listen_endpoint();
            if managed.pending_tcp_accept(listen_handle.inner, listen_endpoint)
                != Some(listen_handle.inner)
            {
                return Err(NetError::WouldBlock);
            }
            let local = managed
                .tcp_socket(listen_handle.inner)
                .local_endpoint()
                .map(endpoint_from_smoltcp);
            let remote = managed
                .tcp_socket(listen_handle.inner)
                .remote_endpoint()
                .map(endpoint_from_smoltcp);

            let new_listen_handle = managed.add_tcp_socket(TCP_RX_BUF_SIZE, TCP_TX_BUF_SIZE);
            let new_listen = NetSocketHandle {
                iface_id: listen_handle.iface_id,
                inner: new_listen_handle,
                sock_type: SocketType::Tcp,
            };
            managed
                .tcp_socket_mut(new_listen_handle)
                .listen(listen_endpoint)
                .map_err(|_| NetError::AddressInUse)?;
            let accepted = NetSocketHandle {
                iface_id: listen_handle.iface_id,
                inner: listen_handle.inner,
                sock_type: SocketType::Tcp,
            };
            managed.mark_socket_accepted(listen_handle.inner);
            TcpAcceptInfo {
                accepted,
                listener: new_listen,
                local,
                remote,
            }
        };
        Ok(info)
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
        Ok(NetSocketHandle {
            iface_id,
            inner,
            sock_type: SocketType::Tcp,
        })
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
        Ok(NetSocketHandle {
            iface_id,
            inner,
            sock_type: SocketType::Udp,
        })
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
            SocketType::Raw => {
                // TODO: raw socket 没有连接态，这里用 can_recv/can_send 推导
                // Established 只是为了兼容上层等待逻辑。
                if managed.raw_socket(handle.inner).can_recv()
                    || managed.raw_socket(handle.inner).can_send()
                {
                    SocketState::Established
                } else {
                    SocketState::Closed
                }
            }
            SocketType::Icmp => {
                // TODO: ICMP socket 也没有严格连接态，这里返回 Established
                // 只是近似 readiness，不代表 peer/identifier 已绑定。
                if managed.icmp_socket(handle.inner).can_recv()
                    || managed.icmp_socket(handle.inner).can_send()
                {
                    SocketState::Established
                } else {
                    SocketState::Closed
                }
            }
        }
    }

    // ── 内部辅助 ─────────────────────────────────────────────────────────

    /// 按目标地址做最长前缀匹配选择出口接口（跳过 loopback）。
    pub fn resolve_iface_for_remote(&self, remote: &crate::IpAddr) -> Option<InterfaceId> {
        if is_loopback_ip(remote) {
            return self.loopback_iface_id();
        }
        let table = self.interfaces.read();
        let mut best: Option<(InterfaceId, u8)> = None;
        for (&id, iface_lock) in table.iter() {
            let managed = iface_lock.lock();
            if managed.name() == "lo" {
                continue;
            }
            for cidr in &managed.config().addresses {
                if ip_matches_cidr(remote, cidr) {
                    if best.map_or(true, |(_, p)| cidr.prefix_len > p) {
                        best = Some((id, cidr.prefix_len));
                    }
                }
            }
        }
        if let Some((id, _)) = best {
            return Some(id);
        }
        // 回退：第一个非 lo 接口
        table
            .iter()
            .find(|&(_, lock)| lock.lock().name() != "lo")
            .map(|(&id, _)| id)
    }

    /// 按单个地址选择接口。未指定地址保留现有默认选择；loopback 地址强制走 lo。
    pub fn resolve_iface_for_addr(&self, addr: &crate::IpAddr) -> Option<InterfaceId> {
        if is_unspecified_ip(addr) {
            return self.default_iface_id().ok();
        }
        self.resolve_iface_for_remote(addr)
    }

    // ── Socket 快照查询（供 /proc/net/ 使用）──────────────────────────────

    /// 快照所有接口上所有非监听 TCP 连接的 socket 信息。
    pub fn snapshot_tcp_connections(
        &self,
    ) -> Vec<(InterfaceId, Vec<crate::socket::TcpConnSnapshot>)> {
        let table = self.interfaces.read();
        let mut out = Vec::new();
        for (&id, iface_lock) in table.iter() {
            let managed = iface_lock.lock();
            let snapshots = managed.tcp_connection_snapshots(id);
            if !snapshots.is_empty() {
                out.push((id, snapshots));
            }
        }
        out
    }

    /// 快照所有接口上所有 UDP socket 的绑定信息。
    pub fn snapshot_udp_sockets(&self) -> Vec<(InterfaceId, Vec<crate::socket::UdpSockSnapshot>)> {
        let table = self.interfaces.read();
        let mut out = Vec::new();
        for (&id, iface_lock) in table.iter() {
            let managed = iface_lock.lock();
            let snapshots = managed.udp_socket_snapshots(id);
            if !snapshots.is_empty() {
                out.push((id, snapshots));
            }
        }
        out
    }

    fn default_iface_id(&self) -> Result<InterfaceId, NetError> {
        let table = self.interfaces.read();
        for (&id, iface_lock) in table.iter() {
            if iface_lock.lock().name() == "lo" {
                return Ok(id);
            }
        }
        table
            .keys()
            .next()
            .copied()
            .ok_or(NetError::InterfaceNotFound)
    }

    fn loopback_iface_id(&self) -> Option<InterfaceId> {
        let table = self.interfaces.read();
        table
            .iter()
            .find(|&(_, lock)| lock.lock().name() == "lo")
            .map(|(&id, _)| id)
    }

    // ── 接口信息查询（供 procfs / netlink 使用）─────────────────────────

    /// 快照所有已注册接口的信息。
    pub fn snapshot_interfaces(&self) -> Vec<InterfaceSnapshot> {
        let table = self.interfaces.read();
        let mut out = Vec::with_capacity(table.len());
        for (&id, iface_lock) in table.iter() {
            let managed = iface_lock.lock();
            out.push(InterfaceSnapshot {
                id,
                name: managed.name(),
                mac: managed.mac(),
                mtu: managed.mtu(),
                flags: compute_iface_flags(&managed),
                addresses: managed.config().addresses.clone(),
                gateway: managed.config().gateway.clone(),
                stats: managed.net_device().driver().stats(),
            });
        }
        out
    }
}

// ── 接口快照类型 ─────────────────────────────────────────────────────────────

/// ARP/NDP 邻居表条目。
#[derive(Debug, Clone)]
pub struct NeighborEntry {
    pub ip_addr: IpAddr,
    pub hw_addr: [u8; 6],
    pub expires_at_ms: i64,
}

pub const IFF_UP: u32 = 0x1;
pub const IFF_RUNNING: u32 = 0x40;
pub const IFF_BROADCAST: u32 = 0x2;
pub const IFF_MULTICAST: u32 = 0x1000;

fn compute_iface_flags(managed: &ManagedInterface) -> u32 {
    let mut flags = IFF_BROADCAST | IFF_MULTICAST;
    if !managed.is_admin_up() {
        return flags;
    }
    flags |= IFF_UP;
    match managed.net_device().driver().link_state() {
        crate::driver::LinkState::Up { .. } => {
            flags |= IFF_RUNNING;
        }
        crate::driver::LinkState::Down => {}
    }
    flags
}

pub struct InterfaceSnapshot {
    pub id: InterfaceId,
    pub name: alloc::string::String,
    pub mac: [u8; 6],
    pub mtu: usize,
    pub flags: u32,
    pub addresses: Vec<CidrAddress>,
    pub gateway: Option<Gateway>,
    pub stats: crate::driver::NetStats,
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

fn endpoint_to_smoltcp_listen(ep: &Endpoint) -> IpListenEndpoint {
    if is_unspecified_ip(&ep.addr) {
        return IpListenEndpoint {
            addr: None,
            port: ep.port,
        };
    }
    IpListenEndpoint {
        addr: Some(endpoint_to_smoltcp(ep).addr),
        port: ep.port,
    }
}

pub(crate) fn endpoint_from_smoltcp(ep: IpEndpoint) -> Endpoint {
    let addr = match ep.addr {
        IpAddress::Ipv4(v4) => {
            let o = v4.octets();
            IpAddr::V4(Ipv4Addr(o))
        }
        IpAddress::Ipv6(v6) => IpAddr::V6(Ipv6Addr(v6.octets())),
    };
    Endpoint {
        addr,
        port: ep.port,
    }
}

fn tcp_state_to_socket_state(state: smoltcp::socket::tcp::State) -> SocketState {
    use smoltcp::socket::tcp::State as S;
    match state {
        S::Closed | S::TimeWait => SocketState::Closed,
        S::Listen => SocketState::Listen,
        S::SynSent | S::SynReceived => SocketState::Connecting,
        S::Established => SocketState::Established,
        S::FinWait1 | S::FinWait2 | S::Closing | S::CloseWait | S::LastAck => SocketState::Closing,
    }
}

fn tcp_state_is_read_eof(state: smoltcp::socket::tcp::State) -> bool {
    use smoltcp::socket::tcp::State as S;
    // 对端 FIN 后，即使接收缓冲为空，poll/read 也必须能观察到 EOF。
    matches!(state, S::CloseWait | S::Closing | S::LastAck | S::TimeWait)
}

fn ip_matches_cidr(addr: &crate::IpAddr, cidr: &crate::config::CidrAddress) -> bool {
    match (addr, &cidr.addr) {
        (crate::IpAddr::V4(a), crate::IpAddr::V4(c)) => {
            if cidr.prefix_len == 0 {
                return true;
            }
            let mask = !0u32 << (32 - cidr.prefix_len);
            let a_int = u32::from_be_bytes(a.0);
            let c_int = u32::from_be_bytes(c.0);
            (a_int & mask) == (c_int & mask)
        }
        _ => false,
    }
}

fn is_loopback_ip(addr: &crate::IpAddr) -> bool {
    match addr {
        crate::IpAddr::V4(v4) => v4.0[0] == 127,
        crate::IpAddr::V6(v6) => *v6 == crate::Ipv6Addr::LOCALHOST,
    }
}

fn is_unspecified_ip(addr: &crate::IpAddr) -> bool {
    match addr {
        crate::IpAddr::V4(v4) => *v4 == crate::Ipv4Addr::UNSPECIFIED,
        crate::IpAddr::V6(v6) => *v6 == crate::Ipv6Addr::UNSPECIFIED,
    }
}

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
