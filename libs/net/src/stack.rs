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

use smoltcp::wire::{IpAddress, IpListenEndpoint, IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet};
use spin::{Mutex, RwLock};

use crate::config::{CidrAddress, Endpoint, Gateway, IfConfig, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::device::{InterfaceId, NetDevice};
use crate::engine::{
    ProtocolSocketHandle, endpoint_from_smoltcp, endpoint_to_smoltcp, endpoint_to_smoltcp_listen,
    tcp_state_is_read_eof, tcp_state_to_socket_state,
};
use crate::error::NetError;
use crate::interface::{InterfacePollResult, ManagedInterface};
use crate::route::{RouteEntry, RouteTable};
use crate::socket::{NetSocketHandle, SocketState, SocketType};
use crate::time::{NetDuration, NetInstant};
use crate::tuning::{EphemeralPortRange, NetTuning, PacketBufferTuning, TcpBufferTuning};

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

/// Raw/ICMP 接收结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawRecvInfo {
    pub len: usize,
    pub remote: Option<Endpoint>,
}

/// Raw IP 出站包头摘要。
///
/// Raw socket 发送路径要求调用方提供完整 IP 包；网络层从包头提取路由和
/// 过滤所需的字段，避免 remote 参数与包头目的地址分叉。
#[derive(Debug, Clone, Copy)]
struct RawPacketMeta {
    version: IpVersion,
    protocol: IpProtocol,
    destination: IpAddr,
}

impl RawPacketMeta {
    fn matches_socket(
        &self,
        socket_version: Option<IpVersion>,
        socket_protocol: Option<IpProtocol>,
    ) -> bool {
        socket_version.is_none_or(|version| version == self.version)
            && socket_protocol.is_none_or(|protocol| protocol == self.protocol)
    }
}

// ── NetStack ─────────────────────────────────────────────────────────────────

/// 全局网络协议栈。
pub struct NetStack {
    /// 网络栈资源和主动调度预算。
    tuning: NetTuning,
    /// 接口注册表。读锁路径包含所有 I/O 操作，写锁仅 attach/detach 时持有。
    interfaces: RwLock<BTreeMap<InterfaceId, Arc<Mutex<ManagedInterface>>>>,
    /// 协议无关路由表。接口配置和管理路由更新时同步维护。
    routes: RwLock<RouteTable>,
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
            tuning: NetTuning::defaults(),
            interfaces: RwLock::new(BTreeMap::new()),
            routes: RwLock::new(RouteTable::new()),
            notify_waiters: sched::WaitQueue::new(),
        }
    }

    /// 返回当前网络栈调优参数快照。
    pub fn tuning(&self) -> NetTuning {
        self.tuning
    }

    /// 注册一个新的网络接口。
    ///
    /// 设备驱动 probe 成功后调用。创建 smoltcp `Interface` 并配置网络参数。
    /// 此操作短暂持有写锁，会暂停所有正在进行的 poll。
    pub fn attach(&self, dev: Arc<NetDevice>, config: IfConfig) -> Result<(), NetError> {
        let id = dev.id();
        {
            let table = self.interfaces.read();
            if table.contains_key(&id) {
                return Err(NetError::InterfaceExists);
            }
        }
        let managed =
            ManagedInterface::new(dev, config.clone(), self.tuning.tcp, self.tuning.tcp_listen)?;
        {
            let mut table = self.interfaces.write();
            if table.contains_key(&id) {
                return Err(NetError::InterfaceExists);
            }
            table.insert(id, Arc::new(Mutex::new(managed)));
        }
        let mut routes = self.routes.write();
        routes.replace_connected(id, &config.addresses);
        routes.replace_gateway(id, config.gateway);
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
        let removed = table
            .remove(&id)
            .map(|_| ())
            .ok_or(NetError::InterfaceNotFound);
        if removed.is_ok() {
            self.routes.write().remove_iface(id);
        }
        removed
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
    pub fn poll(&self, timestamp: NetInstant) {
        let table = self.interfaces.read();
        for (&id, iface_lock) in table.iter() {
            if let Some(mut managed) = iface_lock.try_lock() {
                let result = managed.poll(timestamp);
                drop(managed);
                self.apply_poll_result(id, result);
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
        self.poll(NetInstant::from_millis(millis));
    }

    /// 以纳秒时间戳驱动所有活跃接口。
    ///
    /// 内核调度器原生提供纳秒时间；这里统一转换到网络层自有时间类型，避免
    /// timer/IRQ 路径在进入协议栈前先损失毫秒以下精度。
    pub fn poll_ns(&self, nanos: u64) {
        self.poll(NetInstant::from_nanos(nanos));
    }

    /// 立即驱动一次协议栈。
    ///
    /// 这用于 socket 操作刚刚排入出站数据之后主动推进 smoltcp 状态机。
    /// 尤其是 loopback：TX 会直接回灌到同一接口的 RX 队列，如果只等
    /// timer tick，阻塞 connect/accept/read 路径容易出现不必要的长等待。
    pub fn poll_now(&self) {
        let now_ns = sched::now_ns_public();
        let timestamp = NetInstant::from_nanos(now_ns);
        let table = self.interfaces.read();
        let mut rounds = 0usize;
        while rounds < self.tuning.active_poll.max_rounds {
            let mut changed = false;
            for (&id, iface_lock) in table.iter() {
                let result = iface_lock.lock().poll(timestamp);
                changed |= result.socket_changed;
                self.apply_poll_result(id, result);
            }
            rounds += 1;

            // 至少执行两轮：第一轮常负责发送，第二轮处理 loopback 或同接口
            // 回灌的接收包；之后若没有 socket 状态变化，就停止主动空转。
            if rounds >= 2 && !changed {
                break;
            }
        }
        drop(table);
        self.wake_socket_waiters();
    }

    /// 仅 poll 指定接口（中断驱动的快速路径，零分配）。
    ///
    /// 比 `poll()` 更轻量——只锁定单个接口，其他接口完全不受影响。
    /// 推荐网卡 IRQ handler 调用此方法而非全局 `poll`。
    pub fn poll_interface(&self, id: InterfaceId, timestamp: NetInstant) {
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&id) {
            let result = iface_lock.lock().poll(timestamp);
            self.apply_poll_result(id, result);
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
        self.poll_interface(id, NetInstant::from_millis(millis));
    }

    /// 以纳秒时间戳 poll 指定接口。
    pub fn poll_interface_ns(&self, id: InterfaceId, nanos: u64) {
        self.poll_interface(id, NetInstant::from_nanos(nanos));
    }

    fn apply_poll_result(&self, id: InterfaceId, result: InterfacePollResult) {
        if let Some(config) = result.config_changed {
            let mut routes = self.routes.write();
            routes.replace_connected(id, &config.addresses);
            routes.replace_gateway(id, config.gateway);
        }
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
    pub fn socket_tcp(&self) -> Result<NetSocketHandle, NetError> {
        self.socket_tcp_on(self.default_iface_id()?)
    }

    /// 按远端地址选择接口并创建 TCP socket。
    ///
    /// 这是协议无关的底层接口选择入口；上层兼容层可以在 connect 前调用它，
    /// 避免 socket 已经绑定到错误接口后再尝试修正。
    pub fn socket_tcp_for_remote(&self, remote: IpAddr) -> Result<NetSocketHandle, NetError> {
        let iface_id = self
            .resolve_iface_for_remote(&remote)
            .ok_or(NetError::InterfaceNotFound)?;
        self.socket_tcp_on(iface_id)
    }

    /// 在指定接口上创建 TCP socket。
    pub fn socket_tcp_on(&self, iface_id: InterfaceId) -> Result<NetSocketHandle, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_tcp_socket(self.tuning.tcp);
        make_net_handle(&managed, iface_id, inner, SocketType::Tcp)
    }

    /// 在默认接口上创建一个 UDP socket。
    pub fn socket_udp(&self) -> Result<NetSocketHandle, NetError> {
        self.socket_udp_on(self.default_iface_id()?)
    }

    /// 按远端地址选择接口并创建 UDP socket。
    pub fn socket_udp_for_remote(&self, remote: IpAddr) -> Result<NetSocketHandle, NetError> {
        let iface_id = self
            .resolve_iface_for_remote(&remote)
            .ok_or(NetError::InterfaceNotFound)?;
        self.socket_udp_on(iface_id)
    }

    /// 在指定接口上创建 UDP socket。
    pub fn socket_udp_on(&self, iface_id: InterfaceId) -> Result<NetSocketHandle, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_udp_socket(self.tuning.udp);
        make_net_handle(&managed, iface_id, inner, SocketType::Udp)
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
        let ip_version = raw_ip_version_from_u8(ip_version)?;
        let protocol = IpProtocol::from(protocol);
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        let inner = managed.add_raw_socket(ip_version, protocol, self.tuning.raw);
        make_net_handle(&managed, iface_id, inner, SocketType::Raw)
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
        let inner = managed.add_icmp_socket(self.tuning.icmp);
        make_net_handle(&managed, iface_id, inner, SocketType::Icmp)
    }

    // ── Raw / ICMP 操作 ─────────────────────────────────────────────────

    /// 将 ICMP socket 绑定到 echo identifier。
    ///
    /// 绑定后只接收 identifier 匹配的 echo request/reply。identifier 本身是
    /// ICMP 协议字段，不在这里模拟 POSIX 端口语义。
    pub fn icmp_bind_identifier(
        &self,
        handle: NetSocketHandle,
        identifier: u16,
    ) -> Result<(), NetError> {
        if handle.sock_type != SocketType::Icmp {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        managed
            .icmp_socket_mut(handle.inner)
            .bind(smoltcp::socket::icmp::Endpoint::Ident(identifier))
            .map_err(map_icmp_bind_error)
    }

    /// 将 ICMP socket 绑定到 UDP 本地端点，用于接收该 UDP 端点关联的 ICMP 错误。
    pub fn icmp_bind_udp(&self, handle: NetSocketHandle, local: Endpoint) -> Result<(), NetError> {
        if handle.sock_type != SocketType::Icmp {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        ensure_local_addr_available(&managed, &local.addr)?;
        managed
            .icmp_socket_mut(handle.inner)
            .bind(smoltcp::socket::icmp::Endpoint::Udp(
                endpoint_to_smoltcp_listen(&local),
            ))
            .map_err(map_icmp_bind_error)
    }

    pub fn raw_send(
        &self,
        handle: NetSocketHandle,
        data: &[u8],
        remote: Option<Endpoint>,
    ) -> Result<usize, NetError> {
        let raw_meta = match handle.sock_type {
            SocketType::Raw => {
                let meta = parse_raw_packet_meta(data)?;
                if is_unspecified_ip(&meta.destination) {
                    return Err(NetError::InvalidArgument);
                }
                if remote.is_some_and(|remote| remote.addr != meta.destination) {
                    return Err(NetError::InvalidArgument);
                }
                self.ensure_socket_route_matches(handle.iface_id, &meta.destination)?;
                Some(meta)
            }
            SocketType::Icmp => {
                let remote = remote.ok_or(NetError::InvalidArgument)?;
                validate_specified_remote_addr(&remote.addr)?;
                self.ensure_socket_route_matches(handle.iface_id, &remote.addr)?;
                None
            }
            _ => return Err(NetError::InvalidArgument),
        };
        let sent = {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();
            if managed.handle_is_closed(handle) {
                return Err(NetError::Closed);
            }
            match handle.sock_type {
                SocketType::Raw => {
                    let raw_meta = raw_meta.ok_or(NetError::InvalidArgument)?;
                    let socket = managed.raw_socket_mut(handle.inner);
                    if !raw_meta.matches_socket(socket.ip_version(), socket.ip_protocol()) {
                        return Err(NetError::InvalidArgument);
                    }
                    let tx_buf = socket.send(data.len()).map_err(|_| NetError::WouldBlock)?;
                    tx_buf.copy_from_slice(data);
                    data.len()
                }
                SocketType::Icmp => {
                    let remote = remote.ok_or(NetError::InvalidArgument)?;
                    // ICMP payload 由调用方按已绑定 endpoint 构造；网络层只负责
                    // 目的地址、发送缓冲和协议栈驱动。
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

    pub fn raw_recv_from(
        &self,
        handle: NetSocketHandle,
        buf: &mut [u8],
    ) -> Result<RawRecvInfo, NetError> {
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        match handle.sock_type {
            SocketType::Raw => {
                let socket = managed.raw_socket_mut(handle.inner);
                // raw::Socket 当前不携带来源元信息；调用方需要 None 区分
                // “协议栈没有给出”与“远端确实是未指定地址”。recv_slice 在
                // 缓冲区不足时会丢弃当前包并返回 Truncated，统一映射为
                // NetError::BufferTooSmall，避免底层 API 静默截断数据报。
                let n = socket.recv_slice(buf).map_err(map_raw_recv_error)?;
                Ok(RawRecvInfo {
                    len: n,
                    remote: None,
                })
            }
            SocketType::Icmp => {
                let socket = managed.icmp_socket_mut(handle.inner);
                // ICMP 与 raw 一样按完整数据报交付；缓冲区不足时返回明确错误，
                // 不把半包伪装成完整控制报文。
                let (n, remote) = socket.recv_slice(buf).map_err(map_icmp_recv_error)?;
                Ok(RawRecvInfo {
                    len: n,
                    remote: Some(endpoint_from_ip_address(remote)),
                })
            }
            _ => Err(NetError::InvalidArgument),
        }
    }

    pub fn raw_recv(&self, handle: NetSocketHandle, buf: &mut [u8]) -> Result<usize, NetError> {
        self.raw_recv_from(handle, buf).map(|info| info.len)
    }

    pub fn raw_can_recv(&self, handle: NetSocketHandle) -> bool {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return false;
        };
        let managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
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
        if managed.handle_is_closed(handle) {
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
        // 旧 handle 不在这里跨接口迁移；调用方应尽量用
        // `socket_tcp_for_remote` 创建 socket。这里先保证不会从错误接口发 SYN。
        if handle.sock_type != SocketType::Tcp {
            return Err(NetError::InvalidArgument);
        }
        validate_transport_remote(&remote)?;
        self.ensure_socket_route_matches(handle.iface_id, &remote.addr)?;
        {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();
            if managed.handle_is_closed(handle) {
                return Err(NetError::Closed);
            }
            let remote_ep = endpoint_to_smoltcp(&remote);
            let local_port =
                select_tcp_ephemeral_port(&managed, handle.inner, self.tuning.ephemeral_ports)?;
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
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        ensure_local_addr_available(&managed, &local.addr)?;
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
        for candidate in EphemeralPortCursor::new(self.tuning.ephemeral_ports) {
            local.port = candidate;
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
            if managed.handle_is_closed(handle) {
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
        if managed.handle_is_closed(handle) {
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
                if managed.handle_is_live(handle) {
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
            if managed.handle_is_live(handle) {
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
                Some(NetDuration::from_secs(secs).into_smoltcp())
            };
            if managed.handle_is_live(handle) {
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
                Some(NetDuration::from_secs(secs).into_smoltcp())
            };
            if managed.handle_is_live(handle) {
                managed.tcp_socket_mut(handle.inner).set_timeout(timeout);
            }
        }
    }

    /// 设置 TCP 出站 hop limit（IPv4 TTL / IPv6 hop limit）。
    pub fn tcp_set_hop_limit(&self, handle: NetSocketHandle, ttl: Option<u8>) {
        if handle.sock_type != SocketType::Tcp {
            return;
        }
        let _ = self.socket_set_hop_limit(handle, ttl);
    }

    /// 设置由协议栈生成 IP 头的 socket 出站 hop limit。
    ///
    /// Raw socket 发送路径由调用方提供完整 IP 包头，不能在这里重写 TTL/hop limit。
    pub fn socket_set_hop_limit(
        &self,
        handle: NetSocketHandle,
        hop_limit: Option<u8>,
    ) -> Result<(), NetError> {
        if matches!(hop_limit, Some(0)) {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        match handle.sock_type {
            SocketType::Tcp => managed
                .tcp_socket_mut(handle.inner)
                .set_hop_limit(hop_limit),
            SocketType::Udp => managed
                .udp_socket_mut(handle.inner)
                .set_hop_limit(hop_limit),
            SocketType::Icmp => managed
                .icmp_socket_mut(handle.inner)
                .set_hop_limit(hop_limit),
            SocketType::Raw => return Err(NetError::InvalidArgument),
        }
        Ok(())
    }

    /// 查询 TCP 接收缓冲区可读字节数（FIONREAD）。
    pub fn tcp_recv_queue(&self, handle: NetSocketHandle) -> usize {
        if handle.sock_type != SocketType::Tcp {
            return 0;
        }
        self.socket_recv_queue(handle)
    }

    /// 查询 TCP 发送缓冲区已排队字节数。
    pub fn tcp_send_queue(&self, handle: NetSocketHandle) -> usize {
        if handle.sock_type != SocketType::Tcp {
            return 0;
        }
        self.socket_send_queue(handle)
    }

    /// 查询 socket 接收队列中已缓存的字节数。
    ///
    /// 这是协议无关的底层队列视图：TCP 表示连续字节流可读字节数，
    /// UDP/Raw/ICMP 表示所有已排队数据报 payload 字节总量。上层若需要
    /// POSIX 兼容的“下一次接收长度”，应调用 [`socket_recv_next_len`](Self::socket_recv_next_len)。
    pub fn socket_recv_queue(&self, handle: NetSocketHandle) -> usize {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return 0;
        };
        let managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return 0;
        }
        match handle.sock_type {
            SocketType::Tcp => managed.tcp_socket(handle.inner).recv_queue(),
            SocketType::Udp => managed.udp_socket(handle.inner).recv_queue(),
            SocketType::Raw => managed.raw_socket(handle.inner).recv_queue(),
            SocketType::Icmp => managed.icmp_socket(handle.inner).recv_queue(),
        }
    }

    /// 查询 socket 发送队列中已缓存、等待协议栈发送的字节数。
    pub fn socket_send_queue(&self, handle: NetSocketHandle) -> usize {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return 0;
        };
        let managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return 0;
        }
        match handle.sock_type {
            SocketType::Tcp => managed.tcp_socket(handle.inner).send_queue(),
            SocketType::Udp => managed.udp_socket(handle.inner).send_queue(),
            SocketType::Raw => managed.raw_socket(handle.inner).send_queue(),
            SocketType::Icmp => managed.icmp_socket(handle.inner).send_queue(),
        }
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
        if managed.handle_is_closed(handle) {
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
        if managed.handle_is_closed(handle) {
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
        if managed.handle_is_closed(handle) {
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
        let _ = self.socket_set_hop_limit(handle, ttl);
    }

    /// 设置 ICMP 出站 hop limit。
    pub fn icmp_set_hop_limit(&self, handle: NetSocketHandle, ttl: Option<u8>) {
        if handle.sock_type != SocketType::Icmp {
            return;
        }
        let _ = self.socket_set_hop_limit(handle, ttl);
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
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        let peer = managed.udp_peer(handle.inner);
        let socket = managed.udp_socket_mut(handle.inner);
        udp_recv_filtered(socket, peer, buf, true)
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
        managed.add_default_route_v4(gateway)?;
        drop(managed);
        drop(table);
        self.routes
            .write()
            .replace_gateway_v4(iface_id, Some(gateway));
        Ok(())
    }

    /// 移除指定接口上的默认 IPv4 路由。
    pub fn remove_default_route_v4(&self, iface_id: InterfaceId) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.remove_default_route_v4();
        drop(managed);
        drop(table);
        self.routes.write().replace_gateway_v4(iface_id, None);
        Ok(())
    }

    /// 添加默认 IPv6 路由（gateway）到指定接口。
    pub fn add_default_route_v6(
        &self,
        iface_id: InterfaceId,
        gateway: crate::Ipv6Addr,
    ) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.add_default_route_v6(gateway)?;
        drop(managed);
        drop(table);
        self.routes
            .write()
            .replace_gateway_v6(iface_id, Some(gateway));
        Ok(())
    }

    /// 移除指定接口上的默认 IPv6 路由。
    pub fn remove_default_route_v6(&self, iface_id: InterfaceId) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&iface_id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.remove_default_route_v6();
        drop(managed);
        drop(table);
        self.routes.write().replace_gateway_v6(iface_id, None);
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
        managed.set_ipv4_addr(addr, prefix)?;
        let addresses = managed.config().addresses.clone();
        drop(managed);
        drop(table);
        self.routes.write().replace_connected(id, &addresses);
        Ok(())
    }

    /// 设置指定接口的 IPv6 地址。
    pub fn set_iface_ipv6_addr(
        &self,
        id: InterfaceId,
        addr: crate::Ipv6Addr,
        prefix: u8,
    ) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.set_ipv6_addr(addr, prefix)?;
        let addresses = managed.config().addresses.clone();
        drop(managed);
        drop(table);
        self.routes.write().replace_connected(id, &addresses);
        Ok(())
    }

    /// 一次性替换指定接口的完整网络配置。
    ///
    /// 该入口用于 DHCP/SLAAC 或管理面拿到完整配置后的提交阶段。它同时更新
    /// 协议引擎中的地址/默认网关、接口配置快照和全局路由表，避免上层分别
    /// 调用地址与路由接口时观察到不一致的中间状态。
    pub fn apply_iface_config(&self, id: InterfaceId, config: IfConfig) -> Result<(), NetError> {
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.apply_config(config)?;
        let addresses = managed.config().addresses.clone();
        let gateway = managed.config().gateway;
        drop(managed);
        drop(table);
        let mut routes = self.routes.write();
        routes.replace_connected(id, &addresses);
        routes.replace_gateway(id, gateway);
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
        if mask_to_prefix_len(mask) == 0 {
            return self.add_default_route_v4(id, gw);
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.add_route_v4(dest, mask, gw)?;
        drop(managed);
        drop(table);
        self.routes.write().upsert(RouteEntry::static_v4(
            dest,
            mask_to_prefix_len(mask),
            gw,
            id,
        ));
        Ok(())
    }

    /// 在指定接口上删除 IPv4 路由。
    pub fn remove_route(
        &self,
        id: InterfaceId,
        dest: crate::Ipv4Addr,
        mask: crate::Ipv4Addr,
    ) -> Result<(), NetError> {
        if mask_to_prefix_len(mask) == 0 {
            return self.remove_default_route_v4(id);
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.remove_route_v4(dest, mask);
        drop(managed);
        drop(table);
        self.routes
            .write()
            .remove_static(id, CidrAddress::new_v4(dest, mask_to_prefix_len(mask)));
        Ok(())
    }

    /// 在指定接口上添加 IPv6 路由。
    pub fn add_route_v6(
        &self,
        id: InterfaceId,
        dest: crate::Ipv6Addr,
        prefix_len: u8,
        gw: crate::Ipv6Addr,
    ) -> Result<(), NetError> {
        let prefix_len = prefix_len.min(128);
        if prefix_len == 0 {
            return self.add_default_route_v6(id, gw);
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.add_route_v6(dest, prefix_len, gw)?;
        drop(managed);
        drop(table);
        self.routes
            .write()
            .upsert(RouteEntry::static_v6(dest, prefix_len, gw, id));
        Ok(())
    }

    /// 在指定接口上删除 IPv6 路由。
    pub fn remove_route_v6(
        &self,
        id: InterfaceId,
        dest: crate::Ipv6Addr,
        prefix_len: u8,
    ) -> Result<(), NetError> {
        let prefix_len = prefix_len.min(128);
        if prefix_len == 0 {
            return self.remove_default_route_v6(id);
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&id).ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        managed.remove_route_v6(dest, prefix_len);
        drop(managed);
        drop(table);
        self.routes
            .write()
            .remove_static(id, CidrAddress::new_v6(dest, prefix_len));
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
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        ensure_local_addr_available(&managed, &local.addr)?;
        if local.port != 0 {
            let local_ep = endpoint_to_smoltcp_listen(&local);
            if managed.udp_endpoint_in_use(handle.inner, local_ep) {
                return Err(NetError::AddressInUse);
            }
            let socket = managed.udp_socket_mut(handle.inner);
            socket.bind(local_ep).map_err(|_| NetError::AddressInUse)?;
            return Ok(local);
        }

        // port=0 表示由内核网络层自动分配本地端口。分配和占用检测在同一
        // 把接口锁内完成，避免并发 bind/sendto 选到同一个端口。
        for candidate in EphemeralPortCursor::new(self.tuning.ephemeral_ports) {
            local.port = candidate;
            let local_ep = endpoint_to_smoltcp_listen(&local);
            if managed.udp_endpoint_in_use(handle.inner, local_ep) {
                continue;
            }
            let socket = managed.udp_socket_mut(handle.inner);
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
        if handle.sock_type != SocketType::Udp {
            return Err(NetError::InvalidArgument);
        }
        validate_transport_remote(&remote)?;
        self.ensure_socket_route_matches(handle.iface_id, &remote.addr)?;
        {
            let table = self.interfaces.read();
            let iface_lock = table
                .get(&handle.iface_id)
                .ok_or(NetError::InterfaceNotFound)?;
            let mut managed = iface_lock.lock();
            if managed.handle_is_closed(handle) {
                return Err(NetError::Closed);
            }
            if managed
                .udp_peer(handle.inner)
                .is_some_and(|peer| peer != remote)
            {
                return Err(NetError::InvalidArgument);
            }
            ensure_udp_bound_locked(&mut managed, handle.inner, self.tuning.ephemeral_ports)?;
            let remote_ep = endpoint_to_smoltcp(&remote);
            let socket = managed.udp_socket_mut(handle.inner);
            socket
                .send_slice(data, remote_ep)
                .map_err(|_| NetError::WouldBlock)?;
        }
        self.poll_now();
        Ok(data.len())
    }

    /// UDP connect。记录默认远端 peer，并在需要时自动绑定本地端口。
    pub fn udp_connect(
        &self,
        handle: NetSocketHandle,
        remote: Endpoint,
    ) -> Result<Endpoint, NetError> {
        if handle.sock_type != SocketType::Udp {
            return Err(NetError::InvalidArgument);
        }
        validate_transport_remote(&remote)?;
        self.ensure_socket_route_matches(handle.iface_id, &remote.addr)?;
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        ensure_udp_bound_locked(&mut managed, handle.inner, self.tuning.ephemeral_ports)?;
        managed.set_udp_peer(handle.inner, remote);
        udp_listen_endpoint_to_endpoint(
            managed.udp_socket(handle.inner).endpoint(),
            Some(&remote.addr),
        )
        .ok_or(NetError::Closed)
    }

    /// UDP disconnect。清除默认远端 peer，保留已有本地绑定。
    pub fn udp_disconnect(&self, handle: NetSocketHandle) -> Result<(), NetError> {
        if handle.sock_type != SocketType::Udp {
            return Err(NetError::InvalidArgument);
        }
        let table = self.interfaces.read();
        let iface_lock = table
            .get(&handle.iface_id)
            .ok_or(NetError::InterfaceNotFound)?;
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        managed.clear_udp_peer(handle.inner);
        Ok(())
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
        if managed.handle_is_closed(handle) {
            return Err(NetError::Closed);
        }
        let peer = managed.udp_peer(handle.inner);
        let socket = managed.udp_socket_mut(handle.inner);
        udp_recv_filtered(socket, peer, buf, false)
    }

    /// 查询 UDP socket 的本地端点（getsockname 真实值）。
    pub fn udp_local_endpoint(&self, handle: NetSocketHandle) -> Option<Endpoint> {
        if handle.sock_type != SocketType::Udp {
            return None;
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id)?;
        let managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return None;
        }
        udp_listen_endpoint_to_endpoint(managed.udp_socket(handle.inner).endpoint(), None)
    }

    /// 查询 UDP socket 的远端 peer。
    pub fn udp_remote_endpoint(&self, handle: NetSocketHandle) -> Option<Endpoint> {
        if handle.sock_type != SocketType::Udp {
            return None;
        }
        let table = self.interfaces.read();
        let iface_lock = table.get(&handle.iface_id)?;
        let managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return None;
        }
        managed.udp_peer(handle.inner)
    }

    /// UDP close。解绑并释放 socket。
    pub fn udp_close(&self, handle: NetSocketHandle) {
        if handle.sock_type != SocketType::Udp {
            return;
        }
        let table = self.interfaces.read();
        if let Some(iface_lock) = table.get(&handle.iface_id) {
            let mut managed = iface_lock.lock();
            if managed.handle_is_live(handle) {
                managed.clear_udp_peer(handle.inner);
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
            let mut managed = iface_lock.lock();
            if managed.handle_is_live(handle) {
                managed.soft_remove_socket(handle.inner);
            }
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
        // generation 校验保证旧 handle 不会误关新 socket；同一生命周期内的
        // 并发访问仍由 VFS 文件对象的 handle 所有权约束。
        // 关：让 smoltcp 把内部状态重置（TCP: Closed；UDP: 清 endpoint；RAW/ICMP:
        // release rx/tx buffer）。即使 socket 即将被删，也走一遍以释放 driver
        // 持有的 DMA / 缓冲区。
        match handle.sock_type {
            SocketType::Tcp => {
                let table = self.interfaces.read();
                if let Some(iface_lock) = table.get(&handle.iface_id) {
                    let mut managed = iface_lock.lock();
                    if managed.handle_is_live(handle) {
                        managed.tcp_socket_mut(handle.inner).close();
                    }
                }
            }
            SocketType::Udp => {
                let table = self.interfaces.read();
                if let Some(iface_lock) = table.get(&handle.iface_id) {
                    let mut managed = iface_lock.lock();
                    if managed.handle_is_live(handle) {
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
            if managed.handle_is_live(handle) {
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
                if managed.handle_is_live(handle) {
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
        // TODO: readiness 当前仍缺少精确 socket 事件订阅；TCP EOF 先在这里补齐。
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return false;
        };
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return false;
        }
        match handle.sock_type {
            SocketType::Tcp => {
                let socket = managed.tcp_socket(handle.inner);
                socket.can_recv() || tcp_state_is_read_eof(socket.state())
            }
            SocketType::Udp => {
                let peer = managed.udp_peer(handle.inner);
                udp_next_recv_len_filtered(managed.udp_socket_mut(handle.inner), peer).is_some()
            }
            SocketType::Raw => managed.raw_socket(handle.inner).can_recv(),
            SocketType::Icmp => managed.icmp_socket(handle.inner).can_recv(),
        }
    }

    /// 查询下一次接收能返回的字节数。
    ///
    /// TCP 是字节流，返回当前可读队列总字节数；数据报 socket 返回下一条可被
    /// 当前 socket 接收的数据报长度。UDP 已连接 socket 会丢弃队首非 peer
    /// 数据报，这与实际 recv 路径保持一致。
    pub fn socket_recv_next_len(&self, handle: NetSocketHandle) -> usize {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return 0;
        };
        let mut managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
            return 0;
        }
        match handle.sock_type {
            SocketType::Tcp => managed.tcp_socket(handle.inner).recv_queue(),
            SocketType::Udp => {
                let peer = managed.udp_peer(handle.inner);
                udp_next_recv_len_filtered(managed.udp_socket_mut(handle.inner), peer).unwrap_or(0)
            }
            SocketType::Raw => managed
                .raw_socket_mut(handle.inner)
                .peek()
                .map(|packet| packet.len())
                .unwrap_or(0),
            SocketType::Icmp => managed
                .icmp_socket_mut(handle.inner)
                .peek()
                .map(|(packet, _)| packet.len())
                .unwrap_or(0),
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
        if managed.handle_is_closed(handle) {
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
            let mut managed = iface_lock.lock();
            if !managed.handle_is_live(handle) {
                return Err(NetError::WouldBlock);
            }
            let target = if let Some(local) = local_hint {
                endpoint_to_smoltcp_listen(&local)
            } else {
                managed.tcp_socket(handle.inner).listen_endpoint()
            };
            managed
                .pending_tcp_accept(handle.inner, target)
                .and_then(|inner| managed.make_handle(handle.iface_id, inner, SocketType::Tcp))
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
        let mut managed = iface_lock.lock();
        if !managed.handle_is_live(handle) {
            return false;
        }
        let target = if let Some(local) = local_hint {
            endpoint_to_smoltcp_listen(&local)
        } else {
            managed.tcp_socket(handle.inner).listen_endpoint()
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
            if managed.handle_is_closed(listen_handle) {
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

            let new_listen_handle =
                if let Some(successor) = managed.take_accept_successor(listen_handle.inner) {
                    successor
                } else {
                    let new_listen_handle = managed.add_tcp_socket(self.tuning.tcp);
                    managed
                        .tcp_socket_mut(new_listen_handle)
                        .listen(listen_endpoint)
                        .map_err(|_| NetError::AddressInUse)?;
                    new_listen_handle
                };
            let new_listen = make_net_handle(
                &managed,
                listen_handle.iface_id,
                new_listen_handle,
                SocketType::Tcp,
            )?;
            let accepted = listen_handle;
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
        let inner = managed.add_tcp_socket(TcpBufferTuning {
            rx_bytes: rx_size,
            tx_bytes: tx_size,
        });
        make_net_handle(&managed, iface_id, inner, SocketType::Tcp)
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
        let inner = managed.add_udp_socket(PacketBufferTuning {
            rx_bytes: rx_size,
            tx_bytes: tx_size,
            rx_meta: meta_count,
            tx_meta: meta_count,
        });
        make_net_handle(&managed, iface_id, inner, SocketType::Udp)
    }

    // ── Socket 状态查询 ──────────────────────────────────────────────────

    /// 查询 socket 当前状态。
    pub fn socket_state(&self, handle: NetSocketHandle) -> SocketState {
        let table = self.interfaces.read();
        let Some(iface_lock) = table.get(&handle.iface_id) else {
            return SocketState::Closed;
        };
        let managed = iface_lock.lock();
        if managed.handle_is_closed(handle) {
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

    /// 按目标地址做最长前缀匹配选择出口接口。
    pub fn resolve_iface_for_remote(&self, remote: &crate::IpAddr) -> Option<InterfaceId> {
        if is_loopback_ip(remote) {
            return self.loopback_iface_id();
        }
        let route = self.routes.read().lookup(remote)?;
        let table = self.interfaces.read();
        let iface = table.get(&route.iface)?;
        let managed = iface.lock();
        if managed.is_admin_up() {
            Some(route.iface)
        } else {
            None
        }
    }

    fn ensure_socket_route_matches(
        &self,
        socket_iface: InterfaceId,
        remote: &crate::IpAddr,
    ) -> Result<(), NetError> {
        let routed_iface = self
            .resolve_iface_for_remote(remote)
            .ok_or(NetError::Unreachable)?;
        if routed_iface == socket_iface {
            Ok(())
        } else {
            Err(NetError::Unreachable)
        }
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

fn make_net_handle(
    managed: &ManagedInterface,
    iface_id: InterfaceId,
    inner: ProtocolSocketHandle,
    sock_type: SocketType,
) -> Result<NetSocketHandle, NetError> {
    managed
        .make_handle(iface_id, inner, sock_type)
        .ok_or(NetError::Closed)
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

fn validate_transport_remote(remote: &Endpoint) -> Result<(), NetError> {
    validate_specified_remote_addr(&remote.addr)?;
    if remote.port == 0 {
        return Err(NetError::InvalidArgument);
    }
    Ok(())
}

fn validate_specified_remote_addr(addr: &IpAddr) -> Result<(), NetError> {
    if is_unspecified_ip(addr) {
        return Err(NetError::InvalidArgument);
    }
    Ok(())
}

fn ensure_local_addr_available(managed: &ManagedInterface, addr: &IpAddr) -> Result<(), NetError> {
    if is_unspecified_ip(addr)
        || managed
            .config()
            .addresses
            .iter()
            .any(|assigned| assigned.addr == *addr)
    {
        return Ok(());
    }
    Err(NetError::InvalidArgument)
}

fn endpoint_from_ip_address(addr: IpAddress) -> Endpoint {
    let addr = match addr {
        IpAddress::Ipv4(v4) => IpAddr::V4(Ipv4Addr(v4.octets())),
        IpAddress::Ipv6(v6) => IpAddr::V6(Ipv6Addr(v6.octets())),
    };
    Endpoint { addr, port: 0 }
}

fn parse_raw_packet_meta(data: &[u8]) -> Result<RawPacketMeta, NetError> {
    if data.is_empty() {
        return Err(NetError::InvalidArgument);
    }
    match IpVersion::of_packet(data).map_err(|_| NetError::InvalidArgument)? {
        IpVersion::Ipv4 => {
            let packet = Ipv4Packet::new_checked(data).map_err(|_| NetError::InvalidArgument)?;
            if packet.total_len() as usize != data.len() {
                return Err(NetError::InvalidArgument);
            }
            Ok(RawPacketMeta {
                version: IpVersion::Ipv4,
                protocol: packet.next_header(),
                destination: IpAddr::V4(Ipv4Addr(packet.dst_addr().octets())),
            })
        }
        IpVersion::Ipv6 => {
            let packet = Ipv6Packet::new_checked(data).map_err(|_| NetError::InvalidArgument)?;
            if packet.total_len() != data.len() {
                return Err(NetError::InvalidArgument);
            }
            Ok(RawPacketMeta {
                version: IpVersion::Ipv6,
                protocol: packet.next_header(),
                destination: IpAddr::V6(Ipv6Addr(packet.dst_addr().octets())),
            })
        }
    }
}

fn raw_ip_version_from_u8(version: u8) -> Result<IpVersion, NetError> {
    match version {
        4 => Ok(IpVersion::Ipv4),
        6 => Ok(IpVersion::Ipv6),
        _ => Err(NetError::InvalidArgument),
    }
}

fn ensure_udp_bound_locked(
    managed: &mut ManagedInterface,
    handle: ProtocolSocketHandle,
    range: EphemeralPortRange,
) -> Result<(), NetError> {
    if managed.udp_socket(handle).endpoint().port != 0 {
        return Ok(());
    }
    let local_ep = select_udp_ephemeral_endpoint(managed, handle, range)?;
    managed
        .udp_socket_mut(handle)
        .bind(local_ep)
        .map_err(|_| NetError::AddressInUse)
}

fn udp_recv_filtered(
    socket: &mut smoltcp::socket::udp::Socket<'static>,
    peer: Option<Endpoint>,
    buf: &mut [u8],
    peek: bool,
) -> Result<(usize, Endpoint), NetError> {
    loop {
        if peek {
            let drop_current = {
                let (data, meta) = socket.peek().map_err(|_| NetError::WouldBlock)?;
                let remote = endpoint_from_smoltcp(meta.endpoint);
                if udp_peer_matches(peer, remote) {
                    if buf.len() < data.len() {
                        return Err(NetError::BufferTooSmall);
                    }
                    buf[..data.len()].copy_from_slice(data);
                    return Ok((data.len(), remote));
                }
                true
            };
            if drop_current {
                let _ = socket.recv().map_err(|_| NetError::WouldBlock)?;
            }
            continue;
        }

        let (data, meta) = socket.recv().map_err(|_| NetError::WouldBlock)?;
        let remote = endpoint_from_smoltcp(meta.endpoint);
        if udp_peer_matches(peer, remote) {
            // 普通 recv 已经把数据报出队，缓冲区不足时与 smoltcp::recv_slice 保持一致。
            if buf.len() < data.len() {
                return Err(NetError::BufferTooSmall);
            }
            buf[..data.len()].copy_from_slice(data);
            return Ok((data.len(), remote));
        }
    }
}

fn udp_next_recv_len_filtered(
    socket: &mut smoltcp::socket::udp::Socket<'static>,
    peer: Option<Endpoint>,
) -> Option<usize> {
    loop {
        let drop_current = {
            let (data, meta) = socket.peek().ok()?;
            let remote = endpoint_from_smoltcp(meta.endpoint);
            if udp_peer_matches(peer, remote) {
                return Some(data.len());
            }
            true
        };
        if drop_current {
            let _ = socket.recv().ok()?;
        }
    }
}

fn udp_peer_matches(peer: Option<Endpoint>, remote: Endpoint) -> bool {
    peer.is_none_or(|expected| expected == remote)
}

fn map_raw_recv_error(err: smoltcp::socket::raw::RecvError) -> NetError {
    match err {
        smoltcp::socket::raw::RecvError::Exhausted => NetError::WouldBlock,
        smoltcp::socket::raw::RecvError::Truncated => NetError::BufferTooSmall,
    }
}

fn map_icmp_recv_error(err: smoltcp::socket::icmp::RecvError) -> NetError {
    match err {
        smoltcp::socket::icmp::RecvError::Exhausted => NetError::WouldBlock,
        smoltcp::socket::icmp::RecvError::Truncated => NetError::BufferTooSmall,
    }
}

fn udp_listen_endpoint_to_endpoint(
    ep: IpListenEndpoint,
    family_hint: Option<&IpAddr>,
) -> Option<Endpoint> {
    if ep.port == 0 {
        return None;
    }
    let addr = match ep.addr {
        Some(IpAddress::Ipv4(v4)) => IpAddr::V4(Ipv4Addr(v4.octets())),
        Some(IpAddress::Ipv6(v6)) => IpAddr::V6(Ipv6Addr(v6.octets())),
        None => match family_hint {
            Some(IpAddr::V6(_)) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            _ => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        },
    };
    Some(Endpoint {
        addr,
        port: ep.port,
    })
}

fn map_icmp_bind_error(err: smoltcp::socket::icmp::BindError) -> NetError {
    match err {
        smoltcp::socket::icmp::BindError::InvalidState => NetError::AddressInUse,
        smoltcp::socket::icmp::BindError::Unaddressable => NetError::InvalidArgument,
    }
}

fn mask_to_prefix_len(mask: Ipv4Addr) -> u8 {
    u32::from_be_bytes(mask.0).leading_ones() as u8
}

fn select_tcp_ephemeral_port(
    managed: &ManagedInterface,
    exclude: ProtocolSocketHandle,
    range: EphemeralPortRange,
) -> Result<u16, NetError> {
    for port in EphemeralPortCursor::new(range) {
        if !managed.tcp_local_port_in_use(exclude, port) {
            return Ok(port);
        }
    }
    Err(NetError::AddressInUse)
}

fn select_udp_ephemeral_endpoint(
    managed: &ManagedInterface,
    exclude: ProtocolSocketHandle,
    range: EphemeralPortRange,
) -> Result<IpListenEndpoint, NetError> {
    for port in EphemeralPortCursor::new(range) {
        let endpoint = IpListenEndpoint { addr: None, port };
        if !managed.udp_endpoint_in_use(exclude, endpoint) {
            return Ok(endpoint);
        }
    }
    Err(NetError::AddressInUse)
}

/// 临时端口候选游标。
///
/// 游标从一个混合了时间和原子序列的偏移开始，随后完整扫描端口范围一次。
/// 这样并发请求不会集中争抢范围起点，同时仍能在端口接近耗尽时确定性地找到
/// 可用端口或返回 `AddressInUse`。
struct EphemeralPortCursor {
    range: EphemeralPortRange,
    offset: u32,
    scanned: u32,
    total: u32,
}

impl EphemeralPortCursor {
    fn new(range: EphemeralPortRange) -> Self {
        static SEQ: AtomicU16 = AtomicU16::new(0);
        let total = range.len();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) as u64;
        let now = sched::now_ns_public();
        let mixed = now ^ seq.rotate_left(7) ^ (seq << 17);
        Self {
            range,
            offset: (mixed % total as u64) as u32,
            scanned: 0,
            total,
        }
    }
}

impl Iterator for EphemeralPortCursor {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        if self.scanned >= self.total {
            return None;
        }
        let index = (self.offset + self.scanned) % self.total;
        self.scanned += 1;
        Some(self.range.start + index as u16)
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::any::Any;
    use spin::Mutex;

    use super::*;
    use crate::driver::{Duplex, LinkMedium, LinkState, NetDriver, RxBuf, TxBuf};

    #[derive(Default)]
    struct TestDriver {
        rx: Mutex<Vec<Vec<u8>>>,
        tx: Mutex<Vec<Vec<u8>>>,
    }

    impl TestDriver {
        fn push_rx(&self, packet: Vec<u8>) {
            self.rx.lock().push(packet);
        }

        fn last_tx(&self) -> Option<Vec<u8>> {
            self.tx.lock().last().cloned()
        }
    }

    impl NetDriver for TestDriver {
        fn medium(&self) -> LinkMedium {
            LinkMedium::Ip
        }

        fn poll_rx(&self) -> Option<RxBuf> {
            let packet = self.rx.lock().pop()?;
            let len = packet.len();
            Some(RxBuf::new(packet.into_boxed_slice(), len))
        }

        fn alloc_tx(&self, len: usize) -> Option<TxBuf> {
            Some(TxBuf::new(alloc::vec![0u8; len].into_boxed_slice()))
        }

        fn commit_tx(&self, buf: TxBuf) {
            self.tx.lock().push(buf.as_slice().to_vec());
        }

        fn link_state(&self) -> LinkState {
            LinkState::Up {
                speed_mbps: None,
                duplex: Duplex::Full,
            }
        }

        fn mac_address(&self) -> [u8; 6] {
            [0; 6]
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn attach_test_iface(stack: &NetStack, name: &str, config: IfConfig) -> InterfaceId {
        attach_test_iface_with_driver(stack, name, config).0
    }

    fn attach_test_iface_with_driver(
        stack: &NetStack,
        name: &str,
        config: IfConfig,
    ) -> (InterfaceId, Arc<TestDriver>) {
        let driver = Arc::new(TestDriver::default());
        let dev = Arc::new(NetDevice::new(name, driver.clone()));
        let id = dev.id();
        stack.attach(dev, config).unwrap();
        (id, driver)
    }

    fn build_icmpv4_echo_reply(
        src: smoltcp::wire::Ipv4Address,
        dst: smoltcp::wire::Ipv4Address,
        ident: u16,
        seq_no: u16,
        payload: &'static [u8],
    ) -> Vec<u8> {
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr, IpProtocol, Ipv4Packet, Ipv4Repr};

        let icmp = Icmpv4Repr::EchoReply {
            ident,
            seq_no,
            data: payload,
        };
        let ip = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Icmp,
            payload_len: icmp.buffer_len(),
            hop_limit: 64,
        };
        let mut bytes = alloc::vec![0u8; ip.buffer_len() + icmp.buffer_len()];
        {
            let mut ip_packet = Ipv4Packet::new_unchecked(&mut bytes);
            ip.emit(&mut ip_packet, &ChecksumCapabilities::default());
            let mut icmp_packet = Icmpv4Packet::new_unchecked(ip_packet.payload_mut());
            icmp.emit(&mut icmp_packet, &ChecksumCapabilities::default());
        }
        bytes
    }

    fn build_icmpv4_echo_request_payload(
        ident: u16,
        seq_no: u16,
        payload: &'static [u8],
    ) -> Vec<u8> {
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};

        let icmp = Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data: payload,
        };
        let mut bytes = alloc::vec![0u8; icmp.buffer_len()];
        let mut packet = Icmpv4Packet::new_unchecked(&mut bytes);
        icmp.emit(&mut packet, &ChecksumCapabilities::default());
        bytes
    }

    fn build_raw_ipv4_packet(
        src: smoltcp::wire::Ipv4Address,
        dst: smoltcp::wire::Ipv4Address,
        protocol: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{IpProtocol, Ipv4Packet, Ipv4Repr};

        let ip = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::from(protocol),
            payload_len: payload.len(),
            hop_limit: 64,
        };
        let mut bytes = alloc::vec![0u8; ip.buffer_len() + payload.len()];
        {
            let mut packet = Ipv4Packet::new_unchecked(&mut bytes);
            ip.emit(&mut packet, &ChecksumCapabilities::default());
            packet.payload_mut().copy_from_slice(payload);
        }
        bytes
    }

    fn build_raw_ipv6_packet(
        src: smoltcp::wire::Ipv6Address,
        dst: smoltcp::wire::Ipv6Address,
        protocol: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        use smoltcp::wire::{IpProtocol, Ipv6Packet, Ipv6Repr};

        let ip = Ipv6Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::from(protocol),
            payload_len: payload.len(),
            hop_limit: 64,
        };
        let mut bytes = alloc::vec![0u8; ip.buffer_len() + payload.len()];
        {
            let mut packet = Ipv6Packet::new_unchecked(&mut bytes);
            ip.emit(&mut packet);
            packet.payload_mut().copy_from_slice(payload);
        }
        bytes
    }

    fn build_udpv4_packet(
        src: smoltcp::wire::Ipv4Address,
        dst: smoltcp::wire::Ipv4Address,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr};

        let udp = UdpRepr { src_port, dst_port };
        let ip = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Udp,
            payload_len: udp.header_len() + payload.len(),
            hop_limit: 64,
        };
        let mut bytes = alloc::vec![0u8; ip.buffer_len() + ip.payload_len];
        {
            let mut ip_packet = Ipv4Packet::new_unchecked(&mut bytes);
            ip.emit(&mut ip_packet, &ChecksumCapabilities::default());
            let mut udp_packet = UdpPacket::new_unchecked(ip_packet.payload_mut());
            udp.emit(
                &mut udp_packet,
                &IpAddress::Ipv4(src),
                &IpAddress::Ipv4(dst),
                payload.len(),
                |buf| buf.copy_from_slice(payload),
                &ChecksumCapabilities::default(),
            );
        }
        bytes
    }

    #[test]
    fn ephemeral_cursor_scans_each_port_once() {
        let range = EphemeralPortRange {
            start: 61_000,
            end: 61_015,
        };
        let mut ports: Vec<u16> = EphemeralPortCursor::new(range).collect();

        assert_eq!(ports.len(), range.len() as usize);
        assert!(EphemeralPortCursor::new(range).all(|port| range.contains(port)));

        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), range.len() as usize);
        assert_eq!(ports[0], range.start);
        assert_eq!(*ports.last().unwrap(), range.end);
    }

    #[test]
    fn udp_send_route_check_accepts_socket_on_routed_interface() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-route-ok",
            IfConfig::static_v4(Ipv4Addr::new(10, 1, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();

        assert_eq!(
            stack.ensure_socket_route_matches(
                handle.iface_id,
                &IpAddr::V4(Ipv4Addr::new(10, 1, 0, 77)),
            ),
            Ok(())
        );
    }

    #[test]
    fn udp_send_route_check_rejects_socket_on_wrong_interface() {
        let stack = NetStack::new();
        let first = attach_test_iface(
            &stack,
            "eth-route-a",
            IfConfig::static_v4(Ipv4Addr::new(10, 2, 0, 2), 24, None),
        );
        attach_test_iface(
            &stack,
            "eth-route-b",
            IfConfig::static_v4(Ipv4Addr::new(10, 3, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(first).unwrap();

        assert_eq!(
            stack.ensure_socket_route_matches(
                handle.iface_id,
                &IpAddr::V4(Ipv4Addr::new(10, 3, 0, 77)),
            ),
            Err(NetError::Unreachable)
        );
    }

    #[test]
    fn udp_send_to_rejects_wrong_route_before_auto_bind() {
        let stack = NetStack::new();
        let first = attach_test_iface(
            &stack,
            "eth-send-a",
            IfConfig::static_v4(Ipv4Addr::new(10, 5, 0, 2), 24, None),
        );
        attach_test_iface(
            &stack,
            "eth-send-b",
            IfConfig::static_v4(Ipv4Addr::new(10, 6, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(first).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 6, 0, 77)),
            port: 53,
        };

        assert_eq!(
            stack.udp_send_to(handle, b"route-check", remote),
            Err(NetError::Unreachable)
        );
        assert_eq!(stack.udp_local_endpoint(handle), None);
    }

    #[test]
    fn udp_send_to_rejects_invalid_remote_before_auto_bind() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-send-invalid",
            IfConfig::static_v4(Ipv4Addr::new(10, 7, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();

        assert_eq!(
            stack.udp_send_to(
                handle,
                b"bad-port",
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 7, 0, 77)),
                    port: 0,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(
            stack.udp_send_to(
                handle,
                b"bad-addr",
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 53,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(stack.udp_local_endpoint(handle), None);
    }

    #[test]
    fn udp_bind_rejects_nonlocal_address() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-udp-bind-local",
            IfConfig::static_v4(Ipv4Addr::new(10, 7, 1, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();

        assert_eq!(
            stack.udp_bind(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 7, 2, 2)),
                    port: 7777,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(
            stack.udp_bind(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 7, 2, 2)),
                    port: 0,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(stack.udp_local_endpoint(handle), None);
    }

    #[test]
    fn udp_connect_auto_binds_and_records_peer() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-udp-connect",
            IfConfig::static_v4(Ipv4Addr::new(10, 29, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 29, 0, 77)),
            port: 9000,
        };

        assert_eq!(stack.udp_local_endpoint(handle), None);
        let local = stack.udp_connect(handle, remote).unwrap();
        assert_ne!(local.port, 0);
        assert_eq!(local.addr, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(stack.udp_local_endpoint(handle), Some(local));
        assert_eq!(stack.udp_remote_endpoint(handle), Some(remote));
    }

    #[test]
    fn udp_send_to_rejects_non_peer_after_connect() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-udp-peer-send",
            IfConfig::static_v4(Ipv4Addr::new(10, 30, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 30, 0, 77)),
            port: 9000,
        };
        let other = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 30, 0, 88)),
            port: 9000,
        };

        assert!(stack.udp_connect(handle, peer).is_ok());
        assert_eq!(
            stack.udp_send_to(handle, b"wrong-peer", other),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(stack.udp_send_to(handle, b"right-peer", peer), Ok(10));
    }

    #[test]
    fn udp_disconnect_allows_sending_to_new_remote() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-udp-disconnect",
            IfConfig::static_v4(Ipv4Addr::new(10, 31, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 31, 0, 77)),
            port: 9000,
        };
        let other = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 31, 0, 88)),
            port: 9001,
        };

        assert!(stack.udp_connect(handle, peer).is_ok());
        assert_eq!(stack.udp_disconnect(handle), Ok(()));
        assert_eq!(stack.udp_remote_endpoint(handle), None);
        assert_eq!(stack.udp_send_to(handle, b"new-peer", other), Ok(8));
    }

    #[test]
    fn udp_recv_from_filters_non_peer_after_connect() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-udp-peer-recv",
            IfConfig::static_v4(Ipv4Addr::new(10, 32, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 7777,
        };
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 32, 0, 77)),
            port: 9000,
        };
        assert_eq!(stack.udp_bind(handle, local), Ok(local));
        assert!(stack.udp_connect(handle, peer).is_ok());

        let dst = smoltcp::wire::Ipv4Address::new(10, 32, 0, 2);
        driver.push_rx(build_udpv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 32, 0, 88),
            dst,
            9000,
            7777,
            b"wrong",
        ));
        stack.poll_interface(iface, NetInstant::ZERO);
        driver.push_rx(build_udpv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 32, 0, 77),
            dst,
            9000,
            7777,
            b"right",
        ));
        stack.poll_interface(iface, NetInstant::ZERO);

        assert_eq!(stack.socket_recv_queue(handle), 10);
        assert_eq!(stack.socket_recv_next_len(handle), 5);
        assert_eq!(stack.socket_recv_queue(handle), 5);
        assert!(stack.socket_can_recv(handle));

        let mut buf = [0u8; 16];
        let (len, remote) = stack.udp_recv_from(handle, &mut buf).unwrap();
        assert_eq!(len, 5);
        assert_eq!(&buf[..len], b"right");
        assert_eq!(remote, peer);
        assert_eq!(
            stack.udp_recv_from(handle, &mut buf),
            Err(NetError::WouldBlock)
        );
    }

    #[test]
    fn udp_peek_from_filters_peer_without_consuming_match() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-udp-peer-peek",
            IfConfig::static_v4(Ipv4Addr::new(10, 33, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 7778,
        };
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 33, 0, 77)),
            port: 9000,
        };
        assert_eq!(stack.udp_bind(handle, local), Ok(local));
        assert!(stack.udp_connect(handle, peer).is_ok());

        driver.push_rx(build_udpv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 33, 0, 77),
            smoltcp::wire::Ipv4Address::new(10, 33, 0, 2),
            9000,
            7778,
            b"peek",
        ));
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut buf = [0u8; 16];
        let (len, remote) = stack.udp_peek_from(handle, &mut buf).unwrap();
        assert_eq!(len, 4);
        assert_eq!(&buf[..len], b"peek");
        assert_eq!(remote, peer);

        let (len, remote) = stack.udp_recv_from(handle, &mut buf).unwrap();
        assert_eq!(len, 4);
        assert_eq!(&buf[..len], b"peek");
        assert_eq!(remote, peer);
    }

    #[test]
    fn udp_recv_from_rejects_small_buffer_and_drops_datagram() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-udp-small-recv",
            IfConfig::static_v4(Ipv4Addr::new(10, 35, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 7779,
        };
        assert_eq!(stack.udp_bind(handle, local), Ok(local));

        driver.push_rx(build_udpv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 35, 0, 77),
            smoltcp::wire::Ipv4Address::new(10, 35, 0, 2),
            9000,
            7779,
            b"too-large",
        ));
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut small = [0u8; 4];
        assert_eq!(
            stack.udp_recv_from(handle, &mut small),
            Err(NetError::BufferTooSmall)
        );
        let mut large = [0u8; 16];
        assert_eq!(
            stack.udp_recv_from(handle, &mut large),
            Err(NetError::WouldBlock)
        );
    }

    #[test]
    fn udp_peek_from_rejects_small_buffer_without_consuming_datagram() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-udp-small-peek",
            IfConfig::static_v4(Ipv4Addr::new(10, 36, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 7780,
        };
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 36, 0, 77)),
            port: 9000,
        };
        assert_eq!(stack.udp_bind(handle, local), Ok(local));
        assert!(stack.udp_connect(handle, peer).is_ok());

        driver.push_rx(build_udpv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 36, 0, 77),
            smoltcp::wire::Ipv4Address::new(10, 36, 0, 2),
            9000,
            7780,
            b"too-large",
        ));
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut small = [0u8; 4];
        assert_eq!(
            stack.udp_peek_from(handle, &mut small),
            Err(NetError::BufferTooSmall)
        );
        assert_eq!(small, [0u8; 4]);

        let mut large = [0u8; 16];
        let (len, remote) = stack.udp_recv_from(handle, &mut large).unwrap();
        assert_eq!(len, 9);
        assert_eq!(&large[..len], b"too-large");
        assert_eq!(remote, peer);
    }

    #[test]
    fn socket_recv_queue_reports_udp_payload_bytes() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-udp-queue",
            IfConfig::static_v4(Ipv4Addr::new(10, 39, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 7781,
        };
        assert_eq!(stack.udp_bind(handle, local), Ok(local));
        assert_eq!(stack.socket_recv_queue(handle), 0);
        assert_eq!(stack.socket_recv_next_len(handle), 0);

        driver.push_rx(build_udpv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 39, 0, 77),
            smoltcp::wire::Ipv4Address::new(10, 39, 0, 2),
            9000,
            7781,
            b"queued",
        ));
        stack.poll_interface(iface, NetInstant::ZERO);

        assert_eq!(stack.socket_recv_queue(handle), 6);
        assert_eq!(stack.socket_recv_next_len(handle), 6);
        let mut buf = [0u8; 16];
        let (len, _) = stack.udp_recv_from(handle, &mut buf).unwrap();
        assert_eq!(len, 6);
        assert_eq!(stack.socket_recv_queue(handle), 0);
        assert_eq!(stack.socket_recv_next_len(handle), 0);
    }

    #[test]
    fn tcp_connect_rejects_wrong_route_before_syn() {
        let stack = NetStack::new();
        let first = attach_test_iface(
            &stack,
            "eth-tcp-a",
            IfConfig::static_v4(Ipv4Addr::new(10, 8, 0, 2), 24, None),
        );
        attach_test_iface(
            &stack,
            "eth-tcp-b",
            IfConfig::static_v4(Ipv4Addr::new(10, 9, 0, 2), 24, None),
        );
        let handle = stack.socket_tcp_on(first).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 9, 0, 77)),
            port: 80,
        };

        assert_eq!(
            stack.tcp_connect(handle, remote),
            Err(NetError::Unreachable)
        );
        assert_eq!(stack.socket_state(handle), SocketState::Closed);
    }

    #[test]
    fn tcp_connect_rejects_invalid_remote_before_syn() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-tcp-invalid",
            IfConfig::static_v4(Ipv4Addr::new(10, 11, 0, 2), 24, None),
        );
        let handle = stack.socket_tcp_on(iface).unwrap();

        assert_eq!(
            stack.tcp_connect(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 11, 0, 77)),
                    port: 0,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(
            stack.tcp_connect(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 80,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(stack.socket_state(handle), SocketState::Closed);
    }

    #[test]
    fn tcp_listen_rejects_nonlocal_address() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-tcp-listen-local",
            IfConfig::static_v4(Ipv4Addr::new(10, 11, 1, 2), 24, None),
        );
        let handle = stack.socket_tcp_on(iface).unwrap();

        assert_eq!(
            stack.tcp_listen(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 11, 2, 2)),
                    port: 8080,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(
            stack.tcp_listen(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 11, 2, 2)),
                    port: 0,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(stack.socket_state(handle), SocketState::Closed);
    }

    #[test]
    fn raw_send_rejects_wrong_route_before_enqueue() {
        let stack = NetStack::new();
        let first = attach_test_iface(
            &stack,
            "eth-raw-a",
            IfConfig::static_v4(Ipv4Addr::new(10, 13, 0, 2), 24, None),
        );
        attach_test_iface(
            &stack,
            "eth-raw-b",
            IfConfig::static_v4(Ipv4Addr::new(10, 14, 0, 2), 24, None),
        );
        let handle = stack.socket_raw_on(first, 4, 253).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 14, 0, 77)),
            port: 0,
        };
        let packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 13, 0, 2),
            smoltcp::wire::Ipv4Address::new(10, 14, 0, 77),
            253,
            b"raw-route",
        );

        assert_eq!(
            stack.raw_send(handle, &packet, Some(remote)),
            Err(NetError::Unreachable)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn socket_raw_rejects_invalid_ip_version() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-raw-version-invalid",
            IfConfig::static_v4(Ipv4Addr::new(10, 34, 0, 2), 24, None),
        );

        assert_eq!(
            stack.socket_raw_on(iface, 5, 253),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(
            stack.socket_raw_on(iface, 0, 253),
            Err(NetError::InvalidArgument)
        );
        assert!(stack.socket_raw_on(iface, 6, 253).is_ok());
    }

    #[test]
    fn raw_send_accepts_packet_destination_without_remote() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-raw-header-ok",
            IfConfig::static_v4(Ipv4Addr::new(10, 24, 0, 2), 24, None),
        );
        let handle = stack.socket_raw_on(iface, 4, 253).unwrap();
        let packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 24, 0, 2),
            smoltcp::wire::Ipv4Address::new(10, 24, 0, 77),
            253,
            b"raw-payload",
        );

        assert_eq!(stack.raw_send(handle, &packet, None), Ok(packet.len()));
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn raw_send_without_remote_uses_packet_destination_route() {
        let stack = NetStack::new();
        let first = attach_test_iface(
            &stack,
            "eth-raw-header-a",
            IfConfig::static_v4(Ipv4Addr::new(10, 25, 0, 2), 24, None),
        );
        attach_test_iface(
            &stack,
            "eth-raw-header-b",
            IfConfig::static_v4(Ipv4Addr::new(10, 26, 0, 2), 24, None),
        );
        let handle = stack.socket_raw_on(first, 4, 253).unwrap();
        let packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 25, 0, 2),
            smoltcp::wire::Ipv4Address::new(10, 26, 0, 77),
            253,
            b"raw-route",
        );

        assert_eq!(
            stack.raw_send(handle, &packet, None),
            Err(NetError::Unreachable)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn raw_send_rejects_packet_ip_version_that_does_not_match_socket() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-raw-header-version",
            IfConfig::static_v6(
                Ipv6Addr::new([0x2001, 0x0db8, 0x0034, 0, 0, 0, 0, 2]),
                64,
                None,
            ),
        );
        let handle = stack.socket_raw_on(iface, 4, 253).unwrap();
        let packet = build_raw_ipv6_packet(
            smoltcp::wire::Ipv6Address::new(0x2001, 0x0db8, 0x0034, 0, 0, 0, 0, 2),
            smoltcp::wire::Ipv6Address::new(0x2001, 0x0db8, 0x0034, 0, 0, 0, 0, 77),
            253,
            b"raw-ipv6",
        );

        assert_eq!(
            stack.raw_send(handle, &packet, None),
            Err(NetError::InvalidArgument)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn raw_send_rejects_remote_that_differs_from_packet_destination() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-raw-header-remote",
            IfConfig::static_v4(Ipv4Addr::new(10, 27, 0, 2), 24, None),
        );
        let handle = stack.socket_raw_on(iface, 4, 253).unwrap();
        let packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 27, 0, 2),
            smoltcp::wire::Ipv4Address::new(10, 27, 0, 77),
            253,
            b"raw-route",
        );
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 27, 0, 88)),
            port: 0,
        };

        assert_eq!(
            stack.raw_send(handle, &packet, Some(remote)),
            Err(NetError::InvalidArgument)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn raw_send_rejects_packet_protocol_that_does_not_match_socket() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-raw-header-proto",
            IfConfig::static_v4(Ipv4Addr::new(10, 28, 0, 2), 24, None),
        );
        let handle = stack.socket_raw_on(iface, 4, 253).unwrap();
        let packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 28, 0, 2),
            smoltcp::wire::Ipv4Address::new(10, 28, 0, 77),
            254,
            b"raw-route",
        );

        assert_eq!(
            stack.raw_send(handle, &packet, None),
            Err(NetError::InvalidArgument)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn socket_set_hop_limit_rejects_invalid_inputs() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-hop-invalid",
            IfConfig::static_v4(Ipv4Addr::new(10, 28, 1, 2), 24, None),
        );
        let udp = stack.socket_udp_on(iface).unwrap();
        let raw = stack.socket_raw_on(iface, 4, 253).unwrap();

        assert_eq!(
            stack.socket_set_hop_limit(udp, Some(0)),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(
            stack.socket_set_hop_limit(raw, Some(42)),
            Err(NetError::InvalidArgument)
        );
    }

    #[test]
    fn icmp_send_rejects_wrong_route_before_enqueue() {
        let stack = NetStack::new();
        let first = attach_test_iface(
            &stack,
            "eth-icmp-a",
            IfConfig::static_v4(Ipv4Addr::new(10, 15, 0, 2), 24, None),
        );
        attach_test_iface(
            &stack,
            "eth-icmp-b",
            IfConfig::static_v4(Ipv4Addr::new(10, 16, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(first).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 16, 0, 77)),
            port: 0,
        };

        assert_eq!(
            stack.raw_send(handle, b"icmp-route", Some(remote)),
            Err(NetError::Unreachable)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn icmp_set_hop_limit_updates_outgoing_ip_header() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-icmp-hop",
            IfConfig::static_v4(Ipv4Addr::new(10, 15, 1, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 15, 1, 77)),
            port: 0,
        };
        let payload = build_icmpv4_echo_request_payload(0x1234, 0x5678, b"hop");

        stack.icmp_set_hop_limit(handle, Some(42));
        assert_eq!(
            stack.raw_send(handle, &payload, Some(remote)),
            Ok(payload.len())
        );

        let frame = driver.last_tx().unwrap();
        let ip_packet = smoltcp::wire::Ipv4Packet::new_checked(frame.as_slice()).unwrap();
        assert_eq!(ip_packet.hop_limit(), 42);
    }

    #[test]
    fn udp_send_route_check_rejects_remote_without_route() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-route-none",
            IfConfig::static_v4(Ipv4Addr::new(10, 4, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();

        assert_eq!(
            stack.ensure_socket_route_matches(
                handle.iface_id,
                &IpAddr::V4(Ipv4Addr::new(192, 0, 2, 77)),
            ),
            Err(NetError::Unreachable)
        );
    }

    #[test]
    fn udp_send_to_rejects_missing_route_before_auto_bind() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-send-none",
            IfConfig::static_v4(Ipv4Addr::new(10, 7, 0, 2), 24, None),
        );
        let handle = stack.socket_udp_on(iface).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 77)),
            port: 53,
        };

        assert_eq!(
            stack.udp_send_to(handle, b"route-check", remote),
            Err(NetError::Unreachable)
        );
        assert_eq!(stack.udp_local_endpoint(handle), None);
    }

    #[test]
    fn apply_iface_config_replaces_addresses_gateway_and_routes() {
        let stack = NetStack::new();
        let iface = attach_test_iface(&stack, "eth-apply-config", IfConfig::auto());
        let initial = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert!(initial.addresses.is_empty());
        assert_eq!(initial.gateway, None);

        let mut dhcp_config = IfConfig::static_v4(
            Ipv4Addr::new(10, 42, 0, 22),
            24,
            Some(Ipv4Addr::new(10, 42, 0, 1)),
        );
        dhcp_config.mode = crate::IfMode::Auto;
        stack
            .apply_iface_config(iface, dhcp_config.clone())
            .unwrap();

        let configured = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(configured.addresses, dhcp_config.addresses);
        assert_eq!(configured.gateway, dhcp_config.gateway);
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(10, 42, 0, 99)),),
            Ok(())
        );
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),),
            Ok(())
        );

        let v6_config = IfConfig::static_v6(
            Ipv6Addr::new([0x2001, 0x0db8, 0x0042, 0, 0, 0, 0, 2]),
            64,
            None,
        );
        stack.apply_iface_config(iface, v6_config.clone()).unwrap();

        let replaced = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(replaced.addresses, v6_config.addresses);
        assert_eq!(replaced.gateway, None);
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(10, 42, 0, 99)),),
            Err(NetError::Unreachable)
        );
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),),
            Err(NetError::Unreachable)
        );
        assert_eq!(
            stack.ensure_socket_route_matches(
                iface,
                &IpAddr::V6(Ipv6Addr::new([0x2001, 0x0db8, 0x0042, 0, 0, 0, 0, 99])),
            ),
            Ok(())
        );
    }

    #[test]
    fn attach_rejects_invalid_interface_address() {
        let stack = NetStack::new();
        let driver = Arc::new(TestDriver::default());
        let dev = Arc::new(NetDevice::new("eth-invalid-addr", driver));
        assert_eq!(
            stack.attach(
                dev,
                IfConfig::static_v4(Ipv4Addr::new(224, 0, 0, 1), 24, None)
            ),
            Err(NetError::InvalidArgument)
        );
        assert!(stack.snapshot_interfaces().is_empty());
        assert_eq!(
            stack.ensure_socket_route_matches(
                InterfaceId(0),
                &IpAddr::V4(Ipv4Addr::new(224, 0, 0, 2)),
            ),
            Err(NetError::Unreachable)
        );
    }

    #[test]
    fn set_iface_ipv6_addr_preserves_ipv4_and_replaces_v6_route() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-set-v6",
            IfConfig::static_v4(Ipv4Addr::new(10, 45, 0, 2), 24, None),
        );
        let first_v6 = Ipv6Addr::new([0x2001, 0x0db8, 0x0045, 0, 0, 0, 0, 2]);
        let second_v6 = Ipv6Addr::new([0x2001, 0x0db8, 0x0046, 0, 0, 0, 0, 2]);
        let v4_remote = IpAddr::V4(Ipv4Addr::new(10, 45, 0, 99));
        let first_remote = IpAddr::V6(Ipv6Addr::new([0x2001, 0x0db8, 0x0045, 0, 0, 0, 0, 99]));
        let second_remote = IpAddr::V6(Ipv6Addr::new([0x2001, 0x0db8, 0x0046, 0, 0, 0, 0, 99]));

        stack.set_iface_ipv6_addr(iface, first_v6, 64).unwrap();
        let configured = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(
            configured.addresses,
            alloc::vec![
                CidrAddress::new_v4(Ipv4Addr::new(10, 45, 0, 2), 24),
                CidrAddress::new_v6(first_v6, 64),
            ]
        );
        assert_eq!(stack.ensure_socket_route_matches(iface, &v4_remote), Ok(()));
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &first_remote),
            Ok(())
        );

        stack.set_iface_ipv6_addr(iface, second_v6, 64).unwrap();
        assert_eq!(stack.ensure_socket_route_matches(iface, &v4_remote), Ok(()));
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &first_remote),
            Err(NetError::Unreachable)
        );
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &second_remote),
            Ok(())
        );
    }

    #[test]
    fn set_iface_ipv6_addr_does_not_update_routes_on_address_capacity_error() {
        let stack = NetStack::new();
        let mut addresses = Vec::new();
        for i in 0..smoltcp::config::IFACE_MAX_ADDR_COUNT {
            addresses.push(CidrAddress::new_v4(Ipv4Addr::new(10, 47, i as u8, 2), 24));
        }
        let iface = attach_test_iface(
            &stack,
            "eth-set-v6-full",
            IfConfig {
                addresses: addresses.clone(),
                gateway: None,
                mode: crate::IfMode::Static,
            },
        );
        let v4_remote = IpAddr::V4(Ipv4Addr::new(10, 47, 0, 99));
        let v6 = Ipv6Addr::new([0x2001, 0x0db8, 0x0047, 0, 0, 0, 0, 2]);
        let v6_remote = IpAddr::V6(Ipv6Addr::new([0x2001, 0x0db8, 0x0047, 0, 0, 0, 0, 99]));

        assert_eq!(
            stack.set_iface_ipv6_addr(iface, v6, 64),
            Err(NetError::ResourceExhausted)
        );
        let snapshot = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(snapshot.addresses, addresses);
        assert_eq!(stack.ensure_socket_route_matches(iface, &v4_remote), Ok(()));
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &v6_remote),
            Err(NetError::Unreachable)
        );
    }

    #[test]
    fn ipv6_default_route_updates_route_table_without_dropping_ipv4() {
        let stack = NetStack::new();
        let config = IfConfig {
            addresses: alloc::vec![
                CidrAddress::new_v4(Ipv4Addr::new(10, 44, 0, 2), 24),
                CidrAddress::new_v6(Ipv6Addr::new([0x2001, 0x0db8, 0x0044, 0, 0, 0, 0, 2]), 64,),
            ],
            gateway: Some(Gateway::V4(Ipv4Addr::new(10, 44, 0, 1))),
            mode: crate::IfMode::Static,
        };
        let iface = attach_test_iface(&stack, "eth-v6-default", config);
        let v4_remote = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 44));
        let v6_remote = IpAddr::V6(Ipv6Addr::new([0x2001, 0x0db8, 0x9900, 0, 0, 0, 0, 44]));

        assert_eq!(stack.ensure_socket_route_matches(iface, &v4_remote), Ok(()));
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &v6_remote),
            Err(NetError::Unreachable)
        );
        let initial = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(
            initial.gateway,
            Some(Gateway::V4(Ipv4Addr::new(10, 44, 0, 1)))
        );

        let v6_gateway = Ipv6Addr::new([0x2001, 0x0db8, 0x0044, 0, 0, 0, 0, 1]);
        stack.add_default_route_v6(iface, v6_gateway).unwrap();
        assert_eq!(stack.ensure_socket_route_matches(iface, &v4_remote), Ok(()));
        assert_eq!(stack.ensure_socket_route_matches(iface, &v6_remote), Ok(()));
        let dual_stack = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(
            dual_stack.gateway,
            Some(Gateway::DualStack {
                v4: Ipv4Addr::new(10, 44, 0, 1),
                v6: v6_gateway,
            })
        );

        stack.remove_default_route_v6(iface).unwrap();
        assert_eq!(stack.ensure_socket_route_matches(iface, &v4_remote), Ok(()));
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &v6_remote),
            Err(NetError::Unreachable)
        );
        let v4_only = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(
            v4_only.gateway,
            Some(Gateway::V4(Ipv4Addr::new(10, 44, 0, 1)))
        );
    }

    #[test]
    fn zero_prefix_static_route_uses_default_gateway_state() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-zero-default",
            IfConfig::static_v4(Ipv4Addr::new(10, 46, 0, 2), 24, None),
        );
        let gateway = Ipv4Addr::new(10, 46, 0, 1);
        let remote = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 46));

        stack
            .add_route(iface, Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED, gateway)
            .unwrap();

        let snapshot = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(snapshot.gateway, Some(Gateway::V4(gateway)));
        let lookup = stack.routes.read().lookup(&remote).unwrap();
        assert_eq!(lookup.source, crate::route::RouteSource::Gateway);
        assert_eq!(stack.ensure_socket_route_matches(iface, &remote), Ok(()));

        stack
            .remove_route(iface, Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED)
            .unwrap();
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &remote),
            Err(NetError::Unreachable)
        );
        let snapshot = stack
            .snapshot_interfaces()
            .into_iter()
            .find(|snapshot| snapshot.id == iface)
            .unwrap();
        assert_eq!(snapshot.gateway, None);
    }

    #[test]
    fn ipv6_static_route_selects_interface_and_can_be_removed() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-v6-static",
            IfConfig::static_v6(
                Ipv6Addr::new([0x2001, 0x0db8, 0x0055, 0, 0, 0, 0, 2]),
                64,
                None,
            ),
        );
        let dest = Ipv6Addr::new([0x2001, 0x0db8, 0x9955, 0, 0, 0, 0, 0]);
        let remote = IpAddr::V6(Ipv6Addr::new([0x2001, 0x0db8, 0x9955, 0x1234, 0, 0, 0, 77]));
        let gateway = Ipv6Addr::new([0x2001, 0x0db8, 0x0055, 0, 0, 0, 0, 1]);

        assert_eq!(
            stack.ensure_socket_route_matches(iface, &remote),
            Err(NetError::Unreachable)
        );

        stack.add_route_v6(iface, dest, 48, gateway).unwrap();
        assert_eq!(stack.ensure_socket_route_matches(iface, &remote), Ok(()));

        stack.remove_route_v6(iface, dest, 48).unwrap();
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &remote),
            Err(NetError::Unreachable)
        );
    }

    #[test]
    fn poll_result_config_change_replaces_routes() {
        let stack = NetStack::new();
        let iface = attach_test_iface(&stack, "eth-poll-config", IfConfig::auto());
        let mut dhcp_config = IfConfig::static_v4(
            Ipv4Addr::new(10, 43, 0, 22),
            24,
            Some(Ipv4Addr::new(10, 43, 0, 1)),
        );
        dhcp_config.mode = crate::IfMode::Auto;

        stack.apply_poll_result(
            iface,
            InterfacePollResult {
                socket_changed: true,
                config_changed: Some(dhcp_config),
            },
        );

        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(10, 43, 0, 99)),),
            Ok(())
        );
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),),
            Ok(())
        );

        stack.apply_poll_result(
            iface,
            InterfacePollResult {
                socket_changed: true,
                config_changed: Some(IfConfig::auto()),
            },
        );

        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(10, 43, 0, 99)),),
            Err(NetError::Unreachable)
        );
        assert_eq!(
            stack.ensure_socket_route_matches(iface, &IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),),
            Err(NetError::Unreachable)
        );
    }

    #[test]
    fn tcp_connect_rejects_missing_route_before_syn() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-tcp-none",
            IfConfig::static_v4(Ipv4Addr::new(10, 10, 0, 2), 24, None),
        );
        let handle = stack.socket_tcp_on(iface).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 88)),
            port: 80,
        };

        assert_eq!(
            stack.tcp_connect(handle, remote),
            Err(NetError::Unreachable)
        );
        assert_eq!(stack.socket_state(handle), SocketState::Closed);
    }

    #[test]
    fn raw_send_rejects_missing_route_before_enqueue() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-raw-none",
            IfConfig::static_v4(Ipv4Addr::new(10, 17, 0, 2), 24, None),
        );
        let handle = stack.socket_raw_on(iface, 4, 253).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 99)),
            port: 0,
        };
        let packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 17, 0, 2),
            smoltcp::wire::Ipv4Address::new(192, 0, 2, 99),
            253,
            b"raw-route",
        );

        assert_eq!(
            stack.raw_send(handle, &packet, Some(remote)),
            Err(NetError::Unreachable)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn icmp_send_rejects_missing_route_before_enqueue() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-icmp-none",
            IfConfig::static_v4(Ipv4Addr::new(10, 18, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();
        let remote = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 99)),
            port: 0,
        };

        assert_eq!(
            stack.raw_send(handle, b"icmp-route", Some(remote)),
            Err(NetError::Unreachable)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn icmp_send_rejects_unspecified_remote_before_route() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-icmp-unspecified",
            IfConfig::static_v4(Ipv4Addr::new(10, 18, 1, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();

        assert_eq!(
            stack.raw_send(
                handle,
                b"icmp-route",
                Some(Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 0,
                }),
            ),
            Err(NetError::InvalidArgument)
        );
        assert!(stack.raw_can_send(handle));
    }

    #[test]
    fn icmp_bind_identifier_rejects_wrong_socket_type() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-icmp-bind-wrong",
            IfConfig::static_v4(Ipv4Addr::new(10, 19, 0, 2), 24, None),
        );
        let udp = stack.socket_udp_on(iface).unwrap();

        assert_eq!(
            stack.icmp_bind_identifier(udp, 0x1234),
            Err(NetError::InvalidArgument)
        );
    }

    #[test]
    fn icmp_bind_identifier_rejects_second_bind() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-icmp-bind-twice",
            IfConfig::static_v4(Ipv4Addr::new(10, 20, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();

        assert_eq!(stack.icmp_bind_identifier(handle, 0x1234), Ok(()));
        assert_eq!(
            stack.icmp_bind_identifier(handle, 0x4321),
            Err(NetError::AddressInUse)
        );
    }

    #[test]
    fn icmp_bind_udp_rejects_zero_port() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-icmp-bind-udp-zero",
            IfConfig::static_v4(Ipv4Addr::new(10, 21, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();

        assert_eq!(
            stack.icmp_bind_udp(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 0,
                },
            ),
            Err(NetError::InvalidArgument)
        );
    }

    #[test]
    fn icmp_bind_udp_rejects_nonlocal_address() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-icmp-bind-udp-local",
            IfConfig::static_v4(Ipv4Addr::new(10, 21, 1, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();

        assert_eq!(
            stack.icmp_bind_udp(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::new(10, 21, 2, 2)),
                    port: 33434,
                },
            ),
            Err(NetError::InvalidArgument)
        );
        assert_eq!(
            stack.icmp_bind_udp(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 33434,
                },
            ),
            Ok(())
        );
    }

    #[test]
    fn icmp_bind_udp_accepts_udp_error_endpoint() {
        let stack = NetStack::new();
        let iface = attach_test_iface(
            &stack,
            "eth-icmp-bind-udp",
            IfConfig::static_v4(Ipv4Addr::new(10, 22, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();

        assert_eq!(
            stack.icmp_bind_udp(
                handle,
                Endpoint {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 33434,
                },
            ),
            Ok(())
        );
    }

    #[test]
    fn icmp_bind_identifier_filters_echo_replies() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-icmp-filter",
            IfConfig::static_v4(Ipv4Addr::new(10, 23, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();
        let src = smoltcp::wire::Ipv4Address::new(10, 23, 0, 77);
        let dst = smoltcp::wire::Ipv4Address::new(10, 23, 0, 2);

        assert_eq!(stack.icmp_bind_identifier(handle, 0x1234), Ok(()));
        driver.push_rx(build_icmpv4_echo_reply(src, dst, 0x4321, 0x5678, b"bad"));
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut buf = [0u8; 16];
        assert!(!stack.raw_can_recv(handle));
        assert_eq!(stack.raw_recv(handle, &mut buf), Err(NetError::WouldBlock));

        driver.push_rx(build_icmpv4_echo_reply(src, dst, 0x1234, 0x5678, b"good"));
        stack.poll_interface(iface, NetInstant::ZERO);

        let info = stack.raw_recv_from(handle, &mut buf).unwrap();
        assert_eq!(info.len, 12);
        assert_eq!(&buf[8..info.len], b"good");
    }

    #[test]
    fn raw_recv_from_preserves_icmp_remote_address() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-icmp-recv",
            IfConfig::static_v4(Ipv4Addr::new(10, 11, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();
        let src = smoltcp::wire::Ipv4Address::new(10, 11, 0, 77);
        let dst = smoltcp::wire::Ipv4Address::new(10, 11, 0, 2);

        assert_eq!(stack.icmp_bind_identifier(handle, 0x1234), Ok(()));
        driver.push_rx(build_icmpv4_echo_reply(src, dst, 0x1234, 0x5678, b"pong"));
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut buf = [0u8; 16];
        let info = stack.raw_recv_from(handle, &mut buf).unwrap();
        assert_eq!(info.len, 12);
        assert_eq!(&buf[8..info.len], b"pong");
        assert_eq!(
            info.remote,
            Some(Endpoint {
                addr: IpAddr::V4(Ipv4Addr::new(10, 11, 0, 77)),
                port: 0,
            })
        );
    }

    #[test]
    fn raw_recv_rejects_small_buffer_and_drops_datagram() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-raw-small-recv",
            IfConfig::static_v4(Ipv4Addr::new(10, 37, 0, 2), 24, None),
        );
        let handle = stack.socket_raw_on(iface, 4, 253).unwrap();
        let packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 37, 0, 77),
            smoltcp::wire::Ipv4Address::new(10, 37, 0, 2),
            253,
            b"raw-too-large",
        );

        driver.push_rx(packet);
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut small = [0u8; 8];
        assert_eq!(
            stack.raw_recv(handle, &mut small),
            Err(NetError::BufferTooSmall)
        );
        let mut large = [0u8; 64];
        assert_eq!(
            stack.raw_recv(handle, &mut large),
            Err(NetError::WouldBlock)
        );
    }

    #[test]
    fn icmp_recv_rejects_small_buffer_and_drops_datagram() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-icmp-small-recv",
            IfConfig::static_v4(Ipv4Addr::new(10, 38, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();
        let src = smoltcp::wire::Ipv4Address::new(10, 38, 0, 77);
        let dst = smoltcp::wire::Ipv4Address::new(10, 38, 0, 2);

        assert_eq!(stack.icmp_bind_identifier(handle, 0x1234), Ok(()));
        driver.push_rx(build_icmpv4_echo_reply(
            src,
            dst,
            0x1234,
            0x5678,
            b"icmp-too-large",
        ));
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut small = [0u8; 4];
        assert_eq!(
            stack.raw_recv(handle, &mut small),
            Err(NetError::BufferTooSmall)
        );
        let mut large = [0u8; 64];
        assert_eq!(
            stack.raw_recv(handle, &mut large),
            Err(NetError::WouldBlock)
        );
    }

    #[test]
    fn socket_recv_queue_reports_raw_and_icmp_datagram_bytes() {
        let stack = NetStack::new();
        let (raw_iface, raw_driver) = attach_test_iface_with_driver(
            &stack,
            "eth-raw-queue",
            IfConfig::static_v4(Ipv4Addr::new(10, 40, 0, 2), 24, None),
        );
        let raw_handle = stack.socket_raw_on(raw_iface, 4, 253).unwrap();
        let raw_packet = build_raw_ipv4_packet(
            smoltcp::wire::Ipv4Address::new(10, 40, 0, 77),
            smoltcp::wire::Ipv4Address::new(10, 40, 0, 2),
            253,
            b"raw-queued",
        );
        let raw_len = raw_packet.len();

        raw_driver.push_rx(raw_packet);
        stack.poll_interface(raw_iface, NetInstant::ZERO);

        assert_eq!(stack.socket_recv_queue(raw_handle), raw_len);
        assert_eq!(stack.socket_recv_next_len(raw_handle), raw_len);
        assert_eq!(stack.socket_recv_next_len(raw_handle), raw_len);
        let mut raw_buf = [0u8; 64];
        assert_eq!(stack.raw_recv(raw_handle, &mut raw_buf), Ok(raw_len));
        assert_eq!(stack.socket_recv_queue(raw_handle), 0);
        assert_eq!(stack.socket_recv_next_len(raw_handle), 0);

        let (icmp_iface, icmp_driver) = attach_test_iface_with_driver(
            &stack,
            "eth-icmp-queue",
            IfConfig::static_v4(Ipv4Addr::new(10, 41, 0, 2), 24, None),
        );
        let icmp_handle = stack.socket_icmp_on(icmp_iface).unwrap();
        let src = smoltcp::wire::Ipv4Address::new(10, 41, 0, 77);
        let dst = smoltcp::wire::Ipv4Address::new(10, 41, 0, 2);

        assert_eq!(stack.icmp_bind_identifier(icmp_handle, 0x1234), Ok(()));
        icmp_driver.push_rx(build_icmpv4_echo_reply(
            src,
            dst,
            0x1234,
            0x5678,
            b"icmp-queued",
        ));
        stack.poll_interface(icmp_iface, NetInstant::ZERO);

        let icmp_len = 8 + b"icmp-queued".len();
        assert_eq!(stack.socket_recv_queue(icmp_handle), icmp_len);
        assert_eq!(stack.socket_recv_next_len(icmp_handle), icmp_len);
        assert_eq!(stack.socket_recv_next_len(icmp_handle), icmp_len);
        let mut icmp_buf = [0u8; 64];
        assert_eq!(stack.raw_recv(icmp_handle, &mut icmp_buf), Ok(icmp_len));
        assert_eq!(stack.socket_recv_queue(icmp_handle), 0);
        assert_eq!(stack.socket_recv_next_len(icmp_handle), 0);
    }

    #[test]
    fn raw_recv_keeps_length_only_compatibility() {
        let stack = NetStack::new();
        let (iface, driver) = attach_test_iface_with_driver(
            &stack,
            "eth-icmp-compat",
            IfConfig::static_v4(Ipv4Addr::new(10, 12, 0, 2), 24, None),
        );
        let handle = stack.socket_icmp_on(iface).unwrap();
        let src = smoltcp::wire::Ipv4Address::new(10, 12, 0, 77);
        let dst = smoltcp::wire::Ipv4Address::new(10, 12, 0, 2);

        assert_eq!(stack.icmp_bind_identifier(handle, 0x1234), Ok(()));
        driver.push_rx(build_icmpv4_echo_reply(src, dst, 0x1234, 0x5678, b"old"));
        stack.poll_interface(iface, NetInstant::ZERO);

        let mut buf = [0u8; 16];
        assert_eq!(stack.raw_recv(handle, &mut buf), Ok(11));
        assert_eq!(&buf[8..11], b"old");
    }
}
