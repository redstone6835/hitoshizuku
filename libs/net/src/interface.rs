//! 单个受管理网络接口的内部状态。
//!
//! [`ManagedInterface`] 封装了一个 smoltcp `Interface` 实例、
//! 对应的 [`NetDeviceAdapter`](crate::adapter::NetDeviceAdapter)
//! 以及该接口上的 socket 集合。
//!
//! 本模块是 smoltcp 类型转换的集中点——把 `config.rs` 中的通用 IP 类型
//! 映射为 smoltcp 的 `wire::*` 类型。支持 IPv4 + IPv6 双栈。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use smoltcp::iface::{self, SocketHandle, SocketSet};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr,
};

use crate::adapter::NetDeviceAdapter;
use crate::config::{CidrAddress, Gateway, IfConfig, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::device::{InterfaceId, NetDevice};
use crate::socket::SocketMeta;

// ── ManagedInterface ─────────────────────────────────────────────────────────

/// 一个受管理的网络接口。
pub(crate) struct ManagedInterface {
    iface: iface::Interface,
    device: NetDeviceAdapter,
    sockets: SocketSet<'static>,
    /// 每个 socket 的元数据（soft-close 标志）。
    pub(crate) meta: BTreeMap<SocketHandle, SocketMeta>,
    #[allow(dead_code)]
    config: IfConfig,
    #[allow(dead_code)]
    net_device: Arc<NetDevice>,
}

impl ManagedInterface {
    /// 根据设备和配置创建受管理接口。
    pub fn new(net_device: Arc<NetDevice>, config: IfConfig) -> Self {
        let driver = Arc::clone(net_device.driver());
        let mac = driver.mac_address();
        let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));

        let mut iface_config = iface::Config::new(hw_addr);
        iface_config.random_seed = generate_seed(net_device.id());

        let mut device = NetDeviceAdapter::new(driver, Arc::clone(&net_device));
        let now = Instant::from_millis(0);
        let mut iface = iface::Interface::new(iface_config, &mut device, now);

        // 配置 IP 地址（支持 IPv4 + IPv6 混合）
        // TODO: IfMode::Auto 当前不会触发 DHCP/SLAAC，这里只会把已有
        // config.addresses 静态写入 smoltcp。
        let cidrs: Vec<IpCidr> = config.addresses.iter().map(cidr_to_smoltcp).collect();
        iface.update_ip_addrs(|addrs| {
            for cidr in cidrs {
                let _ = addrs.push(cidr);
            }
        });

        // 配置默认网关
        if let Some(ref gw) = config.gateway {
            match gw {
                Gateway::V4(v4) => {
                    iface
                        .routes_mut()
                        .add_default_ipv4_route(ipv4_to_smoltcp(*v4))
                        .ok();
                }
                Gateway::V6(v6) => {
                    iface
                        .routes_mut()
                        .add_default_ipv6_route(ipv6_to_smoltcp(*v6))
                        .ok();
                }
                Gateway::DualStack { v4, v6 } => {
                    iface
                        .routes_mut()
                        .add_default_ipv4_route(ipv4_to_smoltcp(*v4))
                        .ok();
                    iface
                        .routes_mut()
                        .add_default_ipv6_route(ipv6_to_smoltcp(*v6))
                        .ok();
                }
            }
        }

        Self {
            iface,
            device,
            sockets: SocketSet::new(Vec::new()),
            meta: BTreeMap::new(),
            config,
            net_device,
        }
    }

    /// 执行一轮协议栈 poll（处理 RX/TX + TCP 状态机推进）。
    ///
    /// 同时清理已标记为 removed 的 socket。
    pub fn poll(&mut self, timestamp: Instant) {
        self.iface
            .poll(timestamp, &mut self.device, &mut self.sockets);
        // 延迟清理：移除标记为 removed 的 socket。
        //
        // 旧实现先 `sockets.remove` 再 `meta.remove`——若这两个动作之间
        // 有任何其它持锁代码路径访问 `self.sockets[handle]`，会看到一个
        // `None` 槽位从而 panic。新实现走 `remove_socket_locked` 同步删
        // 两者，并且保证顺序。
        let to_remove: Vec<SocketHandle> = self
            .meta
            .iter()
            .filter(|(_, m)| m.is_removed())
            .map(|(h, _)| *h)
            .collect();
        for h in to_remove {
            self.remove_socket_locked(h);
        }
    }

    /// 接口名称。
    pub fn name(&self) -> alloc::string::String {
        self.net_device.name().into()
    }

    /// MAC 地址。
    pub fn mac(&self) -> [u8; 6] {
        self.net_device.driver().mac_address()
    }

    pub fn config(&self) -> &IfConfig {
        &self.config
    }

    /// 底层 NetDevice 引用（用于查询 driver 状态：MTU、链路状态等）。
    pub fn net_device(&self) -> &Arc<NetDevice> {
        &self.net_device
    }

    /// 当前驱动报告的 MTU。
    pub fn mtu(&self) -> usize {
        self.net_device.driver().mtu()
    }

    /// 添加或替换默认 IPv4 路由。
    pub fn add_default_route_v4(&mut self, gateway: Ipv4Addr) {
        self.iface
            .routes_mut()
            .add_default_ipv4_route(ipv4_to_smoltcp(gateway))
            .ok();
    }

    /// 移除默认 IPv4 路由。
    pub fn remove_default_route_v4(&mut self) {
        self.iface.routes_mut().remove_default_ipv4_route();
    }

    // ── 运行时配置（供 ioctl / netlink 写操作使用）─────────────────────

    /// 替换接口上的所有 IPv4 地址为指定 CIDR 块。
    pub fn set_ipv4_addr(&mut self, addr: Ipv4Addr, prefix_len: u8) {
        self.iface.update_ip_addrs(|addrs| {
            addrs.retain(|c| !matches!(c, smoltcp::wire::IpCidr::Ipv4(_)));
            let _ = addrs.push(smoltcp::wire::IpCidr::Ipv4(
                smoltcp::wire::Ipv4Cidr::new(ipv4_to_smoltcp(addr), prefix_len),
            ));
        });
    }

    /// 设置接口标志。当前仅记录 IFF_UP 状态变更。
    pub fn set_flags(&mut self, _flags: u32) {
        // smoltcp 无接口 UP/DOWN 切换 API，标志仅通过 ioctl 返回。
    }

    /// 添加 IPv4 路由（smoltcp 仅支持默认路由）。
    pub fn add_route_v4(&mut self, dest: Ipv4Addr, mask: Ipv4Addr, gateway: Ipv4Addr) {
        let prefix_len = mask_to_prefix_len(mask);
        if prefix_len == 0 {
            self.iface
                .routes_mut()
                .add_default_ipv4_route(ipv4_to_smoltcp(gateway))
                .ok();
        } else {
            // smoltcp 不支持非默认路由，降级为默认网关
            self.iface
                .routes_mut()
                .add_default_ipv4_route(ipv4_to_smoltcp(gateway))
                .ok();
            let _ = (dest, prefix_len);
        }
    }

    /// 删除 IPv4 路由。
    pub fn remove_route_v4(&mut self, dest: Ipv4Addr, mask: Ipv4Addr) {
        let prefix_len = mask_to_prefix_len(mask);
        if prefix_len == 0 {
            self.iface.routes_mut().remove_default_ipv4_route();
        }
        let _ = (dest, prefix_len);
    }

    /// 返回邻居缓存中所有条目（ARP/NDP 表查询）。
    pub fn neighbor_entries(&self) -> Vec<crate::stack::NeighborEntry> {
        use smoltcp::wire::{HardwareAddress, IpAddress};
        let mut out = Vec::new();
        for (ip, neigh) in self.iface.neighbor_cache().iter() {
            let hw = match neigh.hardware_addr {
                HardwareAddress::Ethernet(eth) => eth.0,
                _ => [0u8; 6],
            };
            let ip_addr = match ip {
                IpAddress::Ipv4(v4) => crate::config::IpAddr::V4(crate::Ipv4Addr(v4.octets())),
                IpAddress::Ipv6(v6) => crate::config::IpAddr::V6(crate::Ipv6Addr(v6.octets())),
            };
            out.push(crate::stack::NeighborEntry {
                ip_addr,
                hw_addr: hw,
                expires_at_ms: neigh.expires_at.total_millis(),
            });
        }
        out
    }

    /// 检查 socket 是否已被 soft-close 标记为移除。
    pub fn is_socket_removed(&self, handle: SocketHandle) -> bool {
        self.meta.get(&handle).map_or(true, |m| m.is_removed())
    }

    /// 检查 socket handle 当前是否仍存在于元数据表中。
    pub fn has_socket(&self, handle: SocketHandle) -> bool {
        self.meta.contains_key(&handle)
    }

    /// Soft-close：标记 socket 为已移除（延迟到下一轮 poll 时真正释放）。
    ///
    /// **本接口仅供内部 soft-close 协议使用**——上层关闭文件 fd 时应直接
    /// 调用 [`Self::remove_socket_locked`]。soft_remove_socket 适合"延迟
    /// 到下一次 poll"的场景（例如某条路径上需要立刻释放对端资源但又想避开
    /// 持锁移除的复杂性）。
    pub fn soft_remove_socket(&self, handle: SocketHandle) {
        if let Some(meta) = self.meta.get(&handle) {
            meta.mark_removed();
        }
    }

    /// 同步从 `SocketSet` 中移除一个 socket（不依赖 poll 触发）。
    ///
    /// 这是文件描述符 `release` 路径应当使用的入口：调用方已确认 socket
    /// 不再有并发访问，移除动作必须立即生效，否则同 index 会被后续新建的
    /// 其它类型 socket 占用（smoltcp 的 `add` 不会复用 `Some(_)` 的槽位，
    /// 但 `poll` 之后 `soft_remove_socket` 标记的 socket 也只是 `None`
    /// 而真正释放——必须**自己**调本接口才安全）。
    pub fn remove_socket_locked(&mut self, handle: SocketHandle) {
        // 必须先删 meta，否则 `poll` 路径的 to_remove 列表里还会保留旧条目，
        // 下次 poll 会对一个已经在 `SocketSet` 里被替换的 handle 再调
        // `sockets.remove`，触发 "handle does not refer to a valid socket"。
        self.meta.remove(&handle);
        self.sockets.remove(handle);
    }

    /// TCP connect：内部同时访问 socket 和 iface context 避免借用冲突。
    pub fn tcp_connect(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
        remote: smoltcp::wire::IpEndpoint,
        local_port: u16,
    ) -> Result<(), smoltcp::socket::tcp::ConnectError> {
        let cx = self.iface.context();
        let socket = self.sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
        socket.connect(cx, remote, local_port)
    }

    // ── Socket 管理 ──────────────────────────────────────────────────────

    /// 创建一个 TCP socket 并加入本接口的 SocketSet。
    pub fn add_tcp_socket(&mut self, rx_buf_size: usize, tx_buf_size: usize) -> SocketHandle {
        // TODO: 缓冲区大小由调用方固定传入，缺少 SO_SNDBUF/SO_RCVBUF
        // 动态调整和内存压力下的 backpressure 策略。
        let rx_buf = smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; rx_buf_size]);
        let tx_buf = smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; tx_buf_size]);
        let socket = smoltcp::socket::tcp::Socket::new(rx_buf, tx_buf);
        let handle = self.sockets.add(socket);
        self.meta.insert(handle, SocketMeta::new());
        handle
    }

    /// 创建一个 UDP socket 并加入本接口的 SocketSet。
    pub fn add_udp_socket(
        &mut self,
        rx_buf_size: usize,
        tx_buf_size: usize,
        rx_meta_count: usize,
        tx_meta_count: usize,
    ) -> SocketHandle {
        // TODO: UDP packet metadata 数量固定，队列满时只能 WouldBlock；
        // 还没有按 socket option 或负载自动调节。
        let rx_buf = smoltcp::socket::udp::PacketBuffer::new(
            alloc::vec![smoltcp::socket::udp::PacketMetadata::EMPTY; rx_meta_count],
            alloc::vec![0u8; rx_buf_size],
        );
        let tx_buf = smoltcp::socket::udp::PacketBuffer::new(
            alloc::vec![smoltcp::socket::udp::PacketMetadata::EMPTY; tx_meta_count],
            alloc::vec![0u8; tx_buf_size],
        );
        let socket = smoltcp::socket::udp::Socket::new(rx_buf, tx_buf);
        let handle = self.sockets.add(socket);
        self.meta.insert(handle, SocketMeta::new());
        handle
    }

    /// 从 SocketSet 移除一个 socket。
    ///
    /// 旧实现只删 SocketSet 不删 meta，会导致下一次 `poll` 看到 `meta`
    /// 里残留的 `is_removed=true` 条目然后去 `sockets.remove` 一个已经被
    /// 释放的槽位，触发 smoltcp 的 "handle does not refer to a valid
    /// socket" panic。统一走 `remove_socket_locked`。
    pub fn remove_socket(&mut self, handle: smoltcp::iface::SocketHandle) {
        self.remove_socket_locked(handle);
    }

    /// 获取 TCP socket 的可变引用（内部操作用）。
    pub fn tcp_socket_mut(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &mut smoltcp::socket::tcp::Socket<'static> {
        // FIXME: typed accessor 直接下转 SocketSet handle，依赖外层先检查
        // meta 和 socket 类型；旧 handle 误用仍可能触发 smoltcp panic。
        self.sockets.get_mut(handle)
    }

    /// 获取 TCP socket 的只读引用。
    pub fn tcp_socket(
        &self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &smoltcp::socket::tcp::Socket<'static> {
        self.sockets.get(handle)
    }

    /// 获取 UDP socket 的可变引用。
    pub fn udp_socket_mut(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &mut smoltcp::socket::udp::Socket<'static> {
        // FIXME: 同 tcp_socket_mut，缺少 generation/type guard。
        self.sockets.get_mut(handle)
    }

    /// 获取 UDP socket 的只读引用。
    pub fn udp_socket(
        &self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &smoltcp::socket::udp::Socket<'static> {
        self.sockets.get(handle)
    }

    /// 创建一个 raw IP socket（指定 IP 版本和协议号）。
    pub fn add_raw_socket(&mut self, ip_version: u8, protocol: u8) -> SocketHandle {
        use smoltcp::socket::raw;
        use smoltcp::wire::{IpProtocol, IpVersion};
        // TODO: raw buffer/meta 容量写死，且缺少协议过滤以外的 socket option
        // 支持，例如 IP_HDRINCL、TTL/TOS 和接收控制消息。
        let ip_ver = if ip_version == 6 {
            IpVersion::Ipv6
        } else {
            IpVersion::Ipv4
        };
        let proto = IpProtocol::from(protocol);
        let rx_buf = raw::PacketBuffer::new(
            alloc::vec![raw::PacketMetadata::EMPTY; 8],
            alloc::vec![0u8; 8192],
        );
        let tx_buf = raw::PacketBuffer::new(
            alloc::vec![raw::PacketMetadata::EMPTY; 8],
            alloc::vec![0u8; 8192],
        );
        let socket = raw::Socket::new(Some(ip_ver), Some(proto), rx_buf, tx_buf);
        let handle = self.sockets.add(socket);
        self.meta.insert(handle, SocketMeta::new());
        handle
    }

    /// 创建一个 ICMP socket。
    pub fn add_icmp_socket(&mut self) -> SocketHandle {
        use smoltcp::socket::icmp;
        // TODO: ICMP 当前是最小 echo 能力，未建模 identifier、sequence
        // 分发、错误报文队列和 IPv6 ICMP 差异。
        let rx_buf = icmp::PacketBuffer::new(
            alloc::vec![icmp::PacketMetadata::EMPTY; 8],
            alloc::vec![0u8; 8192],
        );
        let tx_buf = icmp::PacketBuffer::new(
            alloc::vec![icmp::PacketMetadata::EMPTY; 8],
            alloc::vec![0u8; 8192],
        );
        let socket = icmp::Socket::new(rx_buf, tx_buf);
        let handle = self.sockets.add(socket);
        self.meta.insert(handle, SocketMeta::new());
        handle
    }

    /// 获取 raw socket 的可变引用。
    pub fn raw_socket_mut(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &mut smoltcp::socket::raw::Socket<'static> {
        self.sockets.get_mut(handle)
    }

    /// 获取 raw socket 的只读引用。
    pub fn raw_socket(
        &self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &smoltcp::socket::raw::Socket<'static> {
        self.sockets.get(handle)
    }

    /// 获取 ICMP socket 的可变引用。
    pub fn icmp_socket_mut(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &mut smoltcp::socket::icmp::Socket<'static> {
        self.sockets.get_mut(handle)
    }

    /// 获取 ICMP socket 的只读引用。
    pub fn icmp_socket(
        &self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &smoltcp::socket::icmp::Socket<'static> {
        self.sockets.get(handle)
    }

    // ── Socket 快照遍历（供 /proc/net/ 使用）──────────────────────────

    /// 遍历所有非监听 TCP socket，产出快照列表。
    pub fn tcp_connection_snapshots(
        &self,
        _iface_id: super::device::InterfaceId,
    ) -> Vec<super::socket::TcpConnSnapshot> {
        let mut out = Vec::new();
        for (handle, socket) in self.sockets.iter() {
            if let smoltcp::socket::Socket::Tcp(tcp_socket) = socket {
                if self
                    .meta
                    .get(&handle)
                    .map_or(true, |m| m.is_removed())
                {
                    continue;
                }
                use smoltcp::socket::tcp::State;
                if matches!(tcp_socket.state(), State::Listen) {
                    continue;
                }
                let local = tcp_socket
                    .local_endpoint()
                    .map(|ep| crate::stack::endpoint_from_smoltcp(ep));
                let remote = tcp_socket
                    .remote_endpoint()
                    .map(|ep| crate::stack::endpoint_from_smoltcp(ep));
                if let (Some(local), Some(remote)) = (local, remote) {
                    out.push(super::socket::TcpConnSnapshot {
                        local,
                        remote,
                        state: tcp_state_to_u8(tcp_socket.state()),
                        tx_queue: tcp_socket.send_queue(),
                        rx_queue: tcp_socket.recv_queue(),
                        inode: 0, // 由 procfs 渲染时填入 slot 号
                    });
                }
            }
        }
        out
    }

    /// 遍历所有 UDP socket，产出快照列表。
    pub fn udp_socket_snapshots(
        &self,
        _iface_id: super::device::InterfaceId,
    ) -> Vec<super::socket::UdpSockSnapshot> {
        let mut out = Vec::new();
        for (handle, socket) in self.sockets.iter() {
            if let smoltcp::socket::Socket::Udp(udp_socket) = socket {
                if self
                    .meta
                    .get(&handle)
                    .map_or(true, |m| m.is_removed())
                {
                    continue;
                }
                let listen_ep = udp_socket.endpoint();
                if let Some(smoltcp_addr) = listen_ep.addr {
                    let local_addr = match smoltcp_addr {
                        smoltcp::wire::IpAddress::Ipv4(v4) => {
                            crate::config::IpAddr::V4(crate::config::Ipv4Addr(v4.octets()))
                        }
                        smoltcp::wire::IpAddress::Ipv6(v6) => {
                            crate::config::IpAddr::V6(crate::config::Ipv6Addr(v6.octets()))
                        }
                    };
                    let local = crate::Endpoint {
                        addr: local_addr,
                        port: listen_ep.port,
                    };
                    out.push(super::socket::UdpSockSnapshot {
                        local,
                        remote: None,
                        inode: 0, // 由 procfs 渲染时填入 slot 号
                    });
                }
            }
        }
        out
    }
}

// ── TCP 状态→Linux 数值映射 ─────────────────────────────────────────────────

fn tcp_state_to_u8(state: smoltcp::socket::tcp::State) -> u8 {
    use smoltcp::socket::tcp::State;
    match state {
        State::Closed => 7,
        State::Listen => 10,
        State::SynSent => 2,
        State::SynReceived => 3,
        State::Established => 1,
        State::FinWait1 => 4,
        State::FinWait2 => 5,
        State::CloseWait => 8,
        State::Closing => 6,
        State::LastAck => 9,
        State::TimeWait => 7,
    }
}

// ── 类型转换 ─────────────────────────────────────────────────────────────────

fn ipv4_to_smoltcp(addr: Ipv4Addr) -> Ipv4Address {
    Ipv4Address::new(addr.0[0], addr.0[1], addr.0[2], addr.0[3])
}

fn ipv6_to_smoltcp(addr: Ipv6Addr) -> Ipv6Address {
    let o = &addr.0;
    Ipv6Address::new(
        u16::from_be_bytes([o[0], o[1]]),
        u16::from_be_bytes([o[2], o[3]]),
        u16::from_be_bytes([o[4], o[5]]),
        u16::from_be_bytes([o[6], o[7]]),
        u16::from_be_bytes([o[8], o[9]]),
        u16::from_be_bytes([o[10], o[11]]),
        u16::from_be_bytes([o[12], o[13]]),
        u16::from_be_bytes([o[14], o[15]]),
    )
}

fn cidr_to_smoltcp(cidr: &CidrAddress) -> IpCidr {
    match cidr.addr {
        IpAddr::V4(v4) => IpCidr::Ipv4(Ipv4Cidr::new(ipv4_to_smoltcp(v4), cidr.prefix_len)),
        IpAddr::V6(v6) => IpCidr::Ipv6(Ipv6Cidr::new(ipv6_to_smoltcp(v6), cidr.prefix_len)),
    }
}

fn generate_seed(id: InterfaceId) -> u64 {
    let raw = id.raw() as u64;
    raw.wrapping_mul(6364136223846793005).wrapping_add(1)
}

fn mask_to_prefix_len(mask: Ipv4Addr) -> u8 {
    u32::from_be_bytes(mask.0).leading_ones() as u8
}
