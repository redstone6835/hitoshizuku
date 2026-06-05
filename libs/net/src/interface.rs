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
    EthernetAddress, HardwareAddress, IpCidr,
    Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr,
};

use crate::adapter::NetDeviceAdapter;
use crate::config::{
    CidrAddress, Gateway, IfConfig, IpAddr, Ipv4Addr, Ipv6Addr,
};
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
        let mut iface = iface::Interface::new(
            iface_config, &mut device, now,
        );

        // 配置 IP 地址（支持 IPv4 + IPv6 混合）
        let cidrs: Vec<IpCidr> = config
            .addresses
            .iter()
            .map(cidr_to_smoltcp)
            .collect();
        iface.update_ip_addrs(|addrs| {
            for cidr in cidrs {
                let _ = addrs.push(cidr);
            }
        });

        // 配置默认网关
        if let Some(ref gw) = config.gateway {
            match gw {
                Gateway::V4(v4) => {
                    iface.routes_mut()
                        .add_default_ipv4_route(ipv4_to_smoltcp(*v4))
                        .ok();
                }
                Gateway::V6(v6) => {
                    iface.routes_mut()
                        .add_default_ipv6_route(ipv6_to_smoltcp(*v6))
                        .ok();
                }
                Gateway::DualStack { v4, v6 } => {
                    iface.routes_mut()
                        .add_default_ipv4_route(ipv4_to_smoltcp(*v4))
                        .ok();
                    iface.routes_mut()
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
        self.iface.poll(
            timestamp, &mut self.device, &mut self.sockets,
        );
        // 延迟清理：移除标记为 removed 的 socket
        let to_remove: Vec<SocketHandle> = self.meta
            .iter()
            .filter(|(_, m)| m.is_removed())
            .map(|(h, _)| *h)
            .collect();
        for h in to_remove {
            self.sockets.remove(h);
            self.meta.remove(&h);
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
        self.meta.get(&handle).map_or(false, |m| m.is_removed())
    }

    /// Soft-close：标记 socket 为已移除（延迟到下一轮 poll 时真正释放）。
    pub fn soft_remove_socket(&self, handle: SocketHandle) {
        if let Some(meta) = self.meta.get(&handle) {
            meta.mark_removed();
        }
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
    pub fn add_tcp_socket(
        &mut self,
        rx_buf_size: usize,
        tx_buf_size: usize,
    ) -> SocketHandle {
        let rx_buf = smoltcp::socket::tcp::SocketBuffer::new(
            alloc::vec![0u8; rx_buf_size],
        );
        let tx_buf = smoltcp::socket::tcp::SocketBuffer::new(
            alloc::vec![0u8; tx_buf_size],
        );
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
    pub fn remove_socket(&mut self, handle: smoltcp::iface::SocketHandle) {
        self.sockets.remove(handle);
    }

    /// 获取 TCP socket 的可变引用（内部操作用）。
    pub fn tcp_socket_mut(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
    ) -> &mut smoltcp::socket::tcp::Socket<'static> {
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
        use smoltcp::wire::{IpVersion, IpProtocol};
        let ip_ver = if ip_version == 6 { IpVersion::Ipv6 } else { IpVersion::Ipv4 };
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
        &mut self, handle: smoltcp::iface::SocketHandle,
    ) -> &mut smoltcp::socket::raw::Socket<'static> {
        self.sockets.get_mut(handle)
    }

    /// 获取 raw socket 的只读引用。
    pub fn raw_socket(
        &self, handle: smoltcp::iface::SocketHandle,
    ) -> &smoltcp::socket::raw::Socket<'static> {
        self.sockets.get(handle)
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
        IpAddr::V4(v4) => {
            IpCidr::Ipv4(Ipv4Cidr::new(ipv4_to_smoltcp(v4), cidr.prefix_len))
        }
        IpAddr::V6(v6) => {
            IpCidr::Ipv6(Ipv6Cidr::new(ipv6_to_smoltcp(v6), cidr.prefix_len))
        }
    }
}

fn generate_seed(id: InterfaceId) -> u64 {
    let raw = id.raw() as u64;
    raw.wrapping_mul(6364136223846793005).wrapping_add(1)
}
