//! 单个受管理网络接口的内部状态。
//!
//! [`ManagedInterface`] 封装了一个 smoltcp `Interface` 实例、
//! 对应的 [`NetDeviceAdapter`](crate::adapter::NetDeviceAdapter)
//! 以及该接口上的 socket 集合。
//!
//! 本模块是 smoltcp 类型转换的集中点——把 `config.rs` 中的通用 IP 类型
//! 映射为 smoltcp 的 `wire::*` 类型。支持 IPv4 + IPv6 双栈。

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use smoltcp::iface::{self, SocketHandle, SocketSet};
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpCidr, IpListenEndpoint, Ipv4Address, Ipv4Cidr, Ipv6Address,
    Ipv6Cidr,
};

use crate::adapter::NetDeviceAdapter;
use crate::config::{CidrAddress, Gateway, IfConfig, IfMode, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::device::{InterfaceId, NetDevice};
use crate::driver::LinkMedium;
use crate::engine::{ProtocolSocketHandle, endpoint_from_smoltcp};
use crate::socket::{NetSocketHandle, SocketMeta, SocketType};
use crate::time::NetInstant;
use crate::tuning::{PacketBufferTuning, TcpBufferTuning, TcpListenTuning};

// ── ManagedInterface ─────────────────────────────────────────────────────────

/// 一个受管理的网络接口。
pub(crate) struct ManagedInterface {
    iface: iface::Interface,
    device: NetDeviceAdapter,
    sockets: SocketSet<'static>,
    /// 每个 socket 的元数据（soft-close 标志）。
    pub(crate) meta: BTreeMap<SocketHandle, SocketMeta>,
    /// 已完成握手的 TCP socket 队列（accept backlog）。
    pending_accepted: Vec<SocketHandle>,
    /// 已建立连接对应的补位监听 socket。
    accept_successors: BTreeMap<SocketHandle, SocketHandle>,
    /// 已交给上层监听 fd 持有的补位 listener。
    accept_published_listeners: BTreeSet<SocketHandle>,
    max_backlog: usize,
    tcp_tuning: TcpBufferTuning,
    inode_counter: core::sync::atomic::AtomicU64,
    /// 每次分配 socket 时递增，用于给对外 handle 打生命周期标记。
    handle_generation: u64,
    config: IfConfig,
    #[allow(dead_code)]
    net_device: Arc<NetDevice>,
    /// 管理态开关。链路是否真的可用仍由驱动的 link_state 决定；这里记录
    /// 用户/管理接口希望该接口参与协议栈收发。
    admin_up: bool,
}

impl ManagedInterface {
    /// 根据设备和配置创建受管理接口。
    pub fn new(
        net_device: Arc<NetDevice>,
        config: IfConfig,
        tcp_tuning: TcpBufferTuning,
        listen_tuning: TcpListenTuning,
    ) -> Self {
        let driver = Arc::clone(net_device.driver());
        let hw_addr = match driver.medium() {
            LinkMedium::Ethernet => {
                HardwareAddress::Ethernet(EthernetAddress(driver.mac_address()))
            }
            LinkMedium::Ip => HardwareAddress::Ip,
        };

        let mut iface_config = iface::Config::new(hw_addr);
        iface_config.random_seed = generate_seed(net_device.id());

        let mut device = NetDeviceAdapter::new(driver, Arc::clone(&net_device));
        let now = NetInstant::ZERO.into_smoltcp();
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
            pending_accepted: Vec::with_capacity(listen_tuning.accept_backlog),
            accept_successors: BTreeMap::new(),
            accept_published_listeners: BTreeSet::new(),
            max_backlog: listen_tuning.accept_backlog,
            tcp_tuning,
            inode_counter: core::sync::atomic::AtomicU64::new(1),
            handle_generation: 1,
            config,
            net_device,
            admin_up: true,
        }
    }

    fn next_inode(&self) -> u64 {
        self.inode_counter
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }

    fn next_generation(&mut self) -> u64 {
        let generation = self.handle_generation;
        self.handle_generation = self.handle_generation.wrapping_add(1).max(1);
        generation
    }

    fn install_socket_meta(&mut self, handle: SocketHandle, sock_type: SocketType) -> u64 {
        let generation = self.next_generation();
        self.meta
            .insert(handle, SocketMeta::new(generation, sock_type));
        generation
    }

    /// 构造对外暴露的 socket handle。
    pub fn make_handle(
        &self,
        iface_id: InterfaceId,
        inner: ProtocolSocketHandle,
        sock_type: SocketType,
    ) -> Option<NetSocketHandle> {
        let raw = inner.into_smoltcp();
        let meta = self.meta.get(&raw)?;
        if meta.is_removed() || meta.is_orphaned() || meta.socket_type() != sock_type {
            return None;
        }
        Some(NetSocketHandle {
            iface_id,
            inner,
            generation: meta.generation(),
            sock_type,
        })
    }

    /// 检查外部 handle 是否仍指向当前生命周期的 socket。
    pub fn handle_is_live(&self, handle: NetSocketHandle) -> bool {
        self.meta
            .get(&handle.inner.into_smoltcp())
            .is_some_and(|meta| {
                meta.matches_handle(handle.generation, handle.sock_type)
                    && !meta.is_removed()
                    && !meta.is_orphaned()
            })
    }

    /// 检查外部 handle 是否存在但已被标记为移除或 orphan。
    pub fn handle_is_closed(&self, handle: NetSocketHandle) -> bool {
        !self.handle_is_live(handle)
    }

    /// 执行一轮协议栈 poll（处理 RX/TX + TCP 状态机推进）。
    ///
    /// 同时清理已标记为 removed 的 socket。
    pub fn poll(&mut self, timestamp: NetInstant) -> bool {
        if !self.admin_up {
            return false;
        }
        let changed = matches!(
            self.iface.poll(
                timestamp.into_smoltcp(),
                &mut self.device,
                &mut self.sockets
            ),
            iface::PollResult::SocketStateChanged
        );
        // 延迟清理：移除标记为 removed 的 socket，或已完成 TCP 收尾的
        // orphan socket。orphan 用于 fd 已释放但 FIN/ACK 仍需继续推进的
        // 场景，不能像普通 remove 一样在 close 后立刻摘掉。
        //
        // 旧实现先 `sockets.remove` 再 `meta.remove`——若这两个动作之间
        // 有任何其它持锁代码路径访问 `self.sockets[handle]`，会看到一个
        // `None` 槽位从而 panic。新实现走 `remove_socket_locked` 同步删
        // 两者，并且保证顺序。
        let to_remove: Vec<SocketHandle> = self
            .sockets
            .iter()
            .filter_map(|(h, socket)| {
                let meta = self.meta.get(&h)?;
                if meta.is_removed() || (meta.is_orphaned() && socket_can_reap_orphan(socket)) {
                    Some(h)
                } else {
                    None
                }
            })
            .collect();
        for h in to_remove {
            self.remove_smoltcp_socket_locked(h);
        }
        if changed {
            self.refresh_pending_tcp_accepts();
            self.ensure_pending_accept_listeners();
        } else if !self.pending_accepted.is_empty() {
            self.prune_pending_tcp_accepts();
        }
        changed
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

    /// 管理态是否允许接口收发。
    pub fn is_admin_up(&self) -> bool {
        self.admin_up
    }

    /// 设置接口管理态。关闭后协议栈不再从该接口收发帧，但底层设备对象仍保持
    /// 注册状态，便于随后重新打开。
    pub fn set_admin_up(&mut self, up: bool) {
        self.admin_up = up;
    }

    /// 底层 NetDevice 引用（用于查询 driver 状态：MTU、链路状态等）。
    pub fn net_device(&self) -> &Arc<NetDevice> {
        &self.net_device
    }

    /// 当前接口生效的 MTU。
    pub fn mtu(&self) -> usize {
        self.net_device.mtu()
    }

    /// 设置接口运行期 MTU。
    ///
    /// 实际校验由 [`NetDevice`] 完成，确保不会超过驱动声明的硬件上限。
    pub fn set_mtu(&mut self, mtu: usize) -> Result<(), crate::NetError> {
        self.net_device.set_mtu(mtu)
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
        let prefix_len = prefix_len.min(32);
        self.iface.update_ip_addrs(|addrs| {
            addrs.retain(|c| !matches!(c, smoltcp::wire::IpCidr::Ipv4(_)));
            let _ = addrs.push(smoltcp::wire::IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                ipv4_to_smoltcp(addr),
                prefix_len,
            )));
        });
        self.config
            .addresses
            .retain(|cidr| !matches!(cidr.addr, IpAddr::V4(_)));
        self.config
            .addresses
            .push(CidrAddress::new_v4(addr, prefix_len));
        self.config.mode = IfMode::Static;
    }

    /// 添加 IPv4 路由到当前协议引擎。
    ///
    /// 内核网络层的完整选路由 [`crate::route`] 维护；这里仅把当前底层协议引擎
    /// 能理解的默认网关同步进去。
    pub fn add_route_v4(&mut self, dest: Ipv4Addr, mask: Ipv4Addr, gateway: Ipv4Addr) {
        let prefix_len = mask_to_prefix_len(mask);
        if prefix_len == 0 {
            self.iface
                .routes_mut()
                .add_default_ipv4_route(ipv4_to_smoltcp(gateway))
                .ok();
        } else {
            // 当前协议引擎没有非默认路由接口，保留内核路由表中的真实条目。
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

    /// 检查同一监听端点是否已经被其它 TCP socket 占用。
    pub fn tcp_listen_endpoint_in_use(
        &self,
        exclude: ProtocolSocketHandle,
        target: IpListenEndpoint,
    ) -> bool {
        let exclude = exclude.into_smoltcp();
        for (handle, socket) in self.sockets.iter() {
            if handle == exclude {
                continue;
            }
            let Some(meta) = self.meta.get(&handle) else {
                continue;
            };
            if meta.is_removed() || meta.is_orphaned() {
                continue;
            }
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                continue;
            };
            if tcp.state() == smoltcp::socket::tcp::State::Listen
                && listen_endpoint_matches(tcp.listen_endpoint(), target)
            {
                return true;
            }
            if self.tcp_socket_is_pending_accept_with_socket(handle, tcp, target) {
                return true;
            }
        }
        false
    }

    /// 检查 TCP 本地端口是否已被当前接口上的其它 socket 占用。
    ///
    /// 主动连接自动选择本地端口时必须在同一把接口锁内检查占用，避免两个
    /// 并发连接拿到相同端口。这里保守地把监听、正在连接、已建立以及关闭
    /// 尾部状态的 socket 都视为占用者。
    pub fn tcp_local_port_in_use(&self, exclude: ProtocolSocketHandle, port: u16) -> bool {
        let exclude = exclude.into_smoltcp();
        for (handle, socket) in self.sockets.iter() {
            if handle == exclude {
                continue;
            }
            let Some(meta) = self.meta.get(&handle) else {
                continue;
            };
            if meta.is_removed() || meta.is_orphaned() {
                continue;
            }
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                continue;
            };
            if tcp.listen_endpoint().port == port {
                return true;
            }
            if tcp.local_endpoint().is_some_and(|ep| ep.port == port) {
                return true;
            }
        }
        false
    }

    /// 检查 UDP 监听端点是否已被当前接口上的其它 socket 占用。
    pub fn udp_endpoint_in_use(
        &self,
        exclude: ProtocolSocketHandle,
        target: IpListenEndpoint,
    ) -> bool {
        let exclude = exclude.into_smoltcp();
        for (handle, socket) in self.sockets.iter() {
            if handle == exclude {
                continue;
            }
            let Some(meta) = self.meta.get(&handle) else {
                continue;
            };
            if meta.is_removed() || meta.is_orphaned() {
                continue;
            }
            let smoltcp::socket::Socket::Udp(udp) = socket else {
                continue;
            };
            let endpoint = udp.endpoint();
            if endpoint.port != 0 && listen_endpoint_matches(endpoint, target) {
                return true;
            }
        }
        false
    }

    /// Soft-close：标记 socket 为已移除（延迟到下一轮 poll 时真正释放）。
    ///
    /// **本接口仅供内部 soft-close 协议使用**——上层关闭文件 fd 时应直接
    /// 调用 [`Self::remove_socket_locked`]。soft_remove_socket 适合"延迟
    /// 到下一次 poll"的场景（例如某条路径上需要立刻释放对端资源但又想避开
    /// 持锁移除的复杂性）。
    pub fn soft_remove_socket(&mut self, handle: ProtocolSocketHandle) {
        let handle = handle.into_smoltcp();
        if let Some(meta) = self.meta.get(&handle) {
            meta.mark_removed();
        }
        self.remove_pending_tcp_accept(handle);
    }

    /// 将 socket 标记为 orphan：上层 fd 已释放，但协议栈继续负责 TCP 收尾。
    pub fn orphan_socket(&mut self, handle: ProtocolSocketHandle) {
        let handle = handle.into_smoltcp();
        if let Some(meta) = self.meta.get(&handle) {
            meta.mark_orphaned();
        }
        self.remove_pending_tcp_accept(handle);
    }

    /// 标记 TCP 连接已被 accept 交付给 VFS。
    pub fn mark_socket_accepted(&mut self, handle: ProtocolSocketHandle) {
        let handle = handle.into_smoltcp();
        if let Some(meta) = self.meta.get(&handle) {
            meta.mark_accepted();
        }
        self.remove_pending_tcp_accept(handle);
    }

    /// 按监听端点查找一个已经由 smoltcp 原地转换为 Established、
    /// 但尚未被 VFS accept 交付的 TCP socket。
    ///
    /// smoltcp 的 listen socket 会在收到 SYN 后直接变成连接 socket；
    /// 内核随后再新建一个 listen socket 顶替。fork/close/handle 复用下，
    /// 仅检查 fd 当前保存的单个 handle 容易漏掉这个已建立连接，因此这里
    /// 允许按端点扫描整个 SocketSet。
    pub fn pending_tcp_accept(
        &mut self,
        preferred: ProtocolSocketHandle,
        target: IpListenEndpoint,
    ) -> Option<ProtocolSocketHandle> {
        let preferred = preferred.into_smoltcp();
        self.prune_pending_tcp_accepts();
        let preferred_pending = self.tcp_socket_is_pending_accept(preferred, target);
        if preferred_pending {
            self.enqueue_pending_tcp_accept(preferred);
        }
        if let Some(handle) = self.queued_pending_tcp_accept(target) {
            return Some(ProtocolSocketHandle::from_smoltcp(handle));
        }
        if preferred_pending {
            return Some(ProtocolSocketHandle::from_smoltcp(preferred));
        }
        let mut found = None;
        for (handle, socket) in self.sockets.iter() {
            if handle == preferred {
                continue;
            }
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                continue;
            };
            if self.tcp_socket_is_pending_accept_with_socket(handle, tcp, target) {
                found = Some(handle);
                break;
            }
        }
        if let Some(handle) = found {
            self.enqueue_pending_tcp_accept(handle);
            Some(ProtocolSocketHandle::from_smoltcp(handle))
        } else {
            None
        }
    }

    fn refresh_pending_tcp_accepts(&mut self) {
        self.prune_pending_tcp_accepts();
        if self.max_backlog == 0 || self.pending_accepted.len() >= self.max_backlog {
            return;
        }
        let meta = &self.meta;
        let pending = &mut self.pending_accepted;
        let max_backlog = self.max_backlog;
        for (handle, socket) in self.sockets.iter() {
            if pending.len() >= max_backlog {
                break;
            }
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                continue;
            };
            if tcp_socket_is_pending_accept_candidate(meta, handle, tcp, None)
                && !pending.contains(&handle)
            {
                pending.push(handle);
            }
        }
    }

    fn ensure_pending_accept_listeners(&mut self) {
        if self.max_backlog == 0 || self.pending_accepted.len() >= self.max_backlog {
            return;
        }
        let mut to_replace = Vec::new();
        for accepted in self.pending_accepted.iter().copied() {
            if self.accept_successors.contains_key(&accepted) {
                continue;
            }
            let Some((_, socket)) = self.sockets.iter().find(|(h, _)| *h == accepted) else {
                continue;
            };
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                continue;
            };
            let endpoint = tcp.listen_endpoint();
            if endpoint.port != 0 {
                to_replace.push((accepted, endpoint));
            }
        }
        for (accepted, endpoint) in to_replace {
            if self.accept_successors.contains_key(&accepted) {
                continue;
            }
            if self.tcp_listen_socket_exists(accepted, endpoint) {
                continue;
            }
            let successor = self.add_tcp_socket(self.tcp_tuning);
            let successor_raw = successor.into_smoltcp();
            if self.tcp_socket_mut(successor).listen(endpoint).is_ok() {
                self.accept_successors.insert(accepted, successor_raw);
            } else {
                self.remove_smoltcp_socket_locked(successor_raw);
            }
        }
    }

    fn prune_pending_tcp_accepts(&mut self) {
        let meta = &self.meta;
        let sockets = &self.sockets;
        self.pending_accepted.retain(|handle| {
            let mut found = None;
            for (socket_handle, socket) in sockets.iter() {
                if socket_handle == *handle {
                    found = Some((socket_handle, socket));
                    break;
                }
            }
            let Some((socket_handle, socket)) = found else {
                return false;
            };
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                return false;
            };
            tcp_socket_is_pending_accept_candidate(meta, socket_handle, tcp, None)
        });
        self.accept_successors.retain(|accepted, successor| {
            self.pending_accepted.contains(accepted) && socket_handle_is_live_tcp(meta, *successor)
        });
    }

    fn queued_pending_tcp_accept(&self, target: IpListenEndpoint) -> Option<SocketHandle> {
        self.pending_accepted
            .iter()
            .copied()
            .find(|handle| self.tcp_socket_is_pending_accept(*handle, target))
    }

    fn tcp_listen_socket_exists(&self, exclude: SocketHandle, target: IpListenEndpoint) -> bool {
        for (handle, socket) in self.sockets.iter() {
            if handle == exclude {
                continue;
            }
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                continue;
            };
            if socket_handle_is_live_tcp(&self.meta, handle)
                && tcp.state() == smoltcp::socket::tcp::State::Listen
                && listen_endpoint_matches(tcp.listen_endpoint(), target)
            {
                return true;
            }
        }
        false
    }

    fn enqueue_pending_tcp_accept(&mut self, handle: SocketHandle) {
        if self.max_backlog == 0
            || self.pending_accepted.len() >= self.max_backlog
            || self.pending_accepted.contains(&handle)
        {
            return;
        }
        self.pending_accepted.push(handle);
    }

    fn remove_pending_tcp_accept(&mut self, handle: SocketHandle) {
        self.pending_accepted.retain(|queued| *queued != handle);
        if let Some(successor) = self.accept_successors.remove(&handle) {
            if !self.accept_published_listeners.remove(&successor)
                && socket_handle_is_live_tcp(&self.meta, successor)
            {
                self.remove_smoltcp_socket_locked(successor);
            }
        }
        let stale_accepted: Vec<SocketHandle> = self
            .accept_successors
            .iter()
            .filter_map(|(accepted, successor)| (*successor == handle).then_some(*accepted))
            .collect();
        for accepted in stale_accepted {
            self.accept_successors.remove(&accepted);
        }
    }

    /// 取出指定已建立连接对应的补位监听 socket。
    pub fn take_accept_successor(
        &mut self,
        accepted: ProtocolSocketHandle,
    ) -> Option<ProtocolSocketHandle> {
        let accepted = accepted.into_smoltcp();
        let mut successor = self.accept_successors.remove(&accepted)?;
        let mut remaining = self.accept_successors.len() + 1;
        while remaining != 0 {
            if self.tcp_socket_is_live_listen(successor) {
                self.accept_published_listeners.insert(successor);
                return Some(ProtocolSocketHandle::from_smoltcp(successor));
            }
            let Some(next) = self.accept_successors.get(&successor).copied() else {
                return None;
            };
            successor = next;
            remaining -= 1;
        }
        None
    }

    fn tcp_socket_is_live_listen(&self, handle: SocketHandle) -> bool {
        if !socket_handle_is_live_tcp(&self.meta, handle) {
            return false;
        }
        let Some((_, socket)) = self.sockets.iter().find(|(h, _)| *h == handle) else {
            return false;
        };
        let smoltcp::socket::Socket::Tcp(tcp) = socket else {
            return false;
        };
        tcp.state() == smoltcp::socket::tcp::State::Listen
    }

    fn tcp_socket_is_pending_accept(&self, handle: SocketHandle, target: IpListenEndpoint) -> bool {
        if !self.meta.contains_key(&handle) {
            return false;
        }
        let Some((socket_handle, socket)) = self.sockets.iter().find(|(h, _)| *h == handle) else {
            return false;
        };
        let smoltcp::socket::Socket::Tcp(tcp) = socket else {
            return false;
        };
        self.tcp_socket_is_pending_accept_with_socket(socket_handle, tcp, target)
    }

    fn tcp_socket_is_pending_accept_with_socket(
        &self,
        handle: SocketHandle,
        tcp: &smoltcp::socket::tcp::Socket<'static>,
        target: IpListenEndpoint,
    ) -> bool {
        tcp_socket_is_pending_accept_candidate(&self.meta, handle, tcp, Some(target))
    }

    /// 同步从 `SocketSet` 中移除一个 socket（不依赖 poll 触发）。
    ///
    /// 这是文件描述符 `release` 路径应当使用的入口：调用方已确认 socket
    /// 不再有并发访问，移除动作必须立即生效，否则同 index 会被后续新建的
    /// 其它类型 socket 占用（smoltcp 的 `add` 不会复用 `Some(_)` 的槽位，
    /// 但 `poll` 之后 `soft_remove_socket` 标记的 socket 也只是 `None`
    /// 而真正释放——必须**自己**调本接口才安全）。
    pub fn remove_socket_locked(&mut self, handle: ProtocolSocketHandle) {
        self.remove_smoltcp_socket_locked(handle.into_smoltcp());
    }

    fn remove_smoltcp_socket_locked(&mut self, handle: SocketHandle) {
        // 必须先删 meta，否则 `poll` 路径的 to_remove 列表里还会保留旧条目，
        // 下次 poll 会对一个已经在 `SocketSet` 里被替换的 handle 再调
        // `sockets.remove`，触发 "handle does not refer to a valid socket"。
        self.meta.remove(&handle);
        self.remove_pending_tcp_accept(handle);
        self.accept_published_listeners.remove(&handle);
        self.sockets.remove(handle);
    }

    /// TCP connect：内部同时访问 socket 和 iface context 避免借用冲突。
    pub fn tcp_connect(
        &mut self,
        handle: ProtocolSocketHandle,
        remote: smoltcp::wire::IpEndpoint,
        local_port: u16,
    ) -> Result<(), smoltcp::socket::tcp::ConnectError> {
        let cx = self.iface.context();
        let socket = self
            .sockets
            .get_mut::<smoltcp::socket::tcp::Socket>(handle.into_smoltcp());
        socket.connect(cx, remote, local_port)
    }

    // ── Socket 管理 ──────────────────────────────────────────────────────

    /// 创建一个 TCP socket 并加入本接口的 SocketSet。
    pub fn add_tcp_socket(&mut self, tuning: TcpBufferTuning) -> ProtocolSocketHandle {
        // 缓冲容量由网络栈调优配置统一提供，后续接入 per-socket option 时只需
        // 在 stack 层选择不同配置，不再修改协议适配层。
        let rx_buf = smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; tuning.rx_bytes]);
        let tx_buf = smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; tuning.tx_bytes]);
        let mut socket = smoltcp::socket::tcp::Socket::new(rx_buf, tx_buf);
        // 当前 syscall 层会分块搬运用户缓冲；在 loopback 大 MTU 下这些块
        // 往往小于 MSS，若默认启用 Nagle，iperf/netperf 这类连续写会很快
        // 被未确认的小段压住。先默认关闭 Nagle，后续可由 TCP_NODELAY
        // setsockopt 再精细控制。
        socket.set_nagle_enabled(false);
        let handle = self.sockets.add(socket);
        self.install_socket_meta(handle, SocketType::Tcp);
        ProtocolSocketHandle::from_smoltcp(handle)
    }

    /// 创建一个 UDP socket 并加入本接口的 SocketSet。
    pub fn add_udp_socket(&mut self, tuning: PacketBufferTuning) -> ProtocolSocketHandle {
        let rx_buf = smoltcp::socket::udp::PacketBuffer::new(
            alloc::vec![smoltcp::socket::udp::PacketMetadata::EMPTY; tuning.rx_meta],
            alloc::vec![0u8; tuning.rx_bytes],
        );
        let tx_buf = smoltcp::socket::udp::PacketBuffer::new(
            alloc::vec![smoltcp::socket::udp::PacketMetadata::EMPTY; tuning.tx_meta],
            alloc::vec![0u8; tuning.tx_bytes],
        );
        let socket = smoltcp::socket::udp::Socket::new(rx_buf, tx_buf);
        let handle = self.sockets.add(socket);
        self.install_socket_meta(handle, SocketType::Udp);
        ProtocolSocketHandle::from_smoltcp(handle)
    }

    /// 获取 TCP socket 的可变引用（内部操作用）。
    pub fn tcp_socket_mut(
        &mut self,
        handle: ProtocolSocketHandle,
    ) -> &mut smoltcp::socket::tcp::Socket<'static> {
        // 内部 typed accessor 仍直接下转 SocketSet handle；对外入口必须先通过
        // handle_is_live 做 generation/type 校验，避免旧 handle 触发下转 panic。
        self.sockets.get_mut(handle.into_smoltcp())
    }

    /// 获取 TCP socket 的只读引用。
    pub fn tcp_socket(
        &self,
        handle: ProtocolSocketHandle,
    ) -> &smoltcp::socket::tcp::Socket<'static> {
        self.sockets.get(handle.into_smoltcp())
    }

    /// 获取 UDP socket 的可变引用。
    pub fn udp_socket_mut(
        &mut self,
        handle: ProtocolSocketHandle,
    ) -> &mut smoltcp::socket::udp::Socket<'static> {
        // 同 tcp_socket_mut：调用方必须先完成 generation/type 校验。
        self.sockets.get_mut(handle.into_smoltcp())
    }

    /// 获取 UDP socket 的只读引用。
    pub fn udp_socket(
        &self,
        handle: ProtocolSocketHandle,
    ) -> &smoltcp::socket::udp::Socket<'static> {
        self.sockets.get(handle.into_smoltcp())
    }

    /// 创建一个 raw IP socket（指定 IP 版本和协议号）。
    pub fn add_raw_socket(
        &mut self,
        ip_version: u8,
        protocol: u8,
        tuning: PacketBufferTuning,
    ) -> ProtocolSocketHandle {
        use smoltcp::socket::raw;
        use smoltcp::wire::{IpProtocol, IpVersion};
        // TODO: raw socket 仍缺少协议过滤以外的 option 支持，例如头部包含
        // 语义、TTL/TOS 和接收控制消息；容量本身已由调优配置统一管理。
        let ip_ver = if ip_version == 6 {
            IpVersion::Ipv6
        } else {
            IpVersion::Ipv4
        };
        let proto = IpProtocol::from(protocol);
        let rx_buf = raw::PacketBuffer::new(
            alloc::vec![raw::PacketMetadata::EMPTY; tuning.rx_meta],
            alloc::vec![0u8; tuning.rx_bytes],
        );
        let tx_buf = raw::PacketBuffer::new(
            alloc::vec![raw::PacketMetadata::EMPTY; tuning.tx_meta],
            alloc::vec![0u8; tuning.tx_bytes],
        );
        let socket = raw::Socket::new(Some(ip_ver), Some(proto), rx_buf, tx_buf);
        let handle = self.sockets.add(socket);
        self.install_socket_meta(handle, SocketType::Raw);
        ProtocolSocketHandle::from_smoltcp(handle)
    }

    /// 创建一个 ICMP socket。
    pub fn add_icmp_socket(&mut self, tuning: PacketBufferTuning) -> ProtocolSocketHandle {
        use smoltcp::socket::icmp;
        // TODO: ICMP identifier 已由 NetStack 绑定接口暴露；仍缺少 sequence
        // 分发策略、错误报文队列和 IPv6 ICMP 细分语义。
        let rx_buf = icmp::PacketBuffer::new(
            alloc::vec![icmp::PacketMetadata::EMPTY; tuning.rx_meta],
            alloc::vec![0u8; tuning.rx_bytes],
        );
        let tx_buf = icmp::PacketBuffer::new(
            alloc::vec![icmp::PacketMetadata::EMPTY; tuning.tx_meta],
            alloc::vec![0u8; tuning.tx_bytes],
        );
        let socket = icmp::Socket::new(rx_buf, tx_buf);
        let handle = self.sockets.add(socket);
        self.install_socket_meta(handle, SocketType::Icmp);
        ProtocolSocketHandle::from_smoltcp(handle)
    }

    /// 获取 raw socket 的可变引用。
    pub fn raw_socket_mut(
        &mut self,
        handle: ProtocolSocketHandle,
    ) -> &mut smoltcp::socket::raw::Socket<'static> {
        self.sockets.get_mut(handle.into_smoltcp())
    }

    /// 获取 raw socket 的只读引用。
    pub fn raw_socket(
        &self,
        handle: ProtocolSocketHandle,
    ) -> &smoltcp::socket::raw::Socket<'static> {
        self.sockets.get(handle.into_smoltcp())
    }

    /// 获取 ICMP socket 的可变引用。
    pub fn icmp_socket_mut(
        &mut self,
        handle: ProtocolSocketHandle,
    ) -> &mut smoltcp::socket::icmp::Socket<'static> {
        self.sockets.get_mut(handle.into_smoltcp())
    }

    /// 获取 ICMP socket 的只读引用。
    pub fn icmp_socket(
        &self,
        handle: ProtocolSocketHandle,
    ) -> &smoltcp::socket::icmp::Socket<'static> {
        self.sockets.get(handle.into_smoltcp())
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
                    .map_or(true, |m| m.is_removed() || m.is_orphaned())
                {
                    continue;
                }
                use smoltcp::socket::tcp::State;
                if matches!(tcp_socket.state(), State::Listen) {
                    continue;
                }
                let local = tcp_socket.local_endpoint().map(endpoint_from_smoltcp);
                let remote = tcp_socket.remote_endpoint().map(endpoint_from_smoltcp);
                if let (Some(local), Some(remote)) = (local, remote) {
                    out.push(super::socket::TcpConnSnapshot {
                        local,
                        remote,
                        state: tcp_state_to_u8(tcp_socket.state()),
                        tx_queue: tcp_socket.send_queue(),
                        rx_queue: tcp_socket.recv_queue(),
                        inode: self.next_inode(),
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
                    .map_or(true, |m| m.is_removed() || m.is_orphaned())
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
                        inode: self.next_inode(),
                    });
                }
            }
        }
        out
    }
}

// ── TCP 状态→用户态兼容数值映射 ─────────────────────────────────────────────

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
        State::TimeWait => 6, // TCP_TIME_WAIT
        State::Closing => 11, // TCP_CLOSING
        State::LastAck => 9,
    }
}

fn socket_can_reap_orphan(socket: &smoltcp::socket::Socket<'_>) -> bool {
    match socket {
        smoltcp::socket::Socket::Tcp(tcp) => {
            use smoltcp::socket::tcp::State;
            // orphan TCP 必须至少推进到最终态再回收，否则 netperf 这类
            // 依赖 EOF/控制连接结果回传的程序会因为 FIN 丢失而永久等待。
            matches!(tcp.state(), State::Closed | State::TimeWait)
        }
        // 非 TCP 没有四次挥手，fd 释放后可以在下一轮 poll 直接回收。
        _ => true,
    }
}

fn listen_endpoint_matches(actual: IpListenEndpoint, target: IpListenEndpoint) -> bool {
    if actual.port != target.port {
        return false;
    }
    match (actual.addr, target.addr) {
        // 监听端点为通配地址时，任意本地地址都属于同一个 listen fd。
        (None, _) | (_, None) => true,
        (Some(a), Some(b)) => a == b,
    }
}

fn tcp_socket_is_pending_accept_candidate(
    meta: &BTreeMap<SocketHandle, SocketMeta>,
    handle: SocketHandle,
    tcp: &smoltcp::socket::tcp::Socket<'static>,
    target: Option<IpListenEndpoint>,
) -> bool {
    let Some(meta) = meta.get(&handle) else {
        return false;
    };
    if meta.socket_type() != SocketType::Tcp
        || meta.is_removed()
        || meta.is_orphaned()
        || meta.is_accepted()
    {
        return false;
    }
    let listen_endpoint = tcp.listen_endpoint();
    if listen_endpoint.port == 0 || tcp.state() != smoltcp::socket::tcp::State::Established {
        return false;
    }
    target.map_or(true, |target| {
        listen_endpoint_matches(listen_endpoint, target)
    })
}

fn socket_handle_is_live_tcp(
    meta: &BTreeMap<SocketHandle, SocketMeta>,
    handle: SocketHandle,
) -> bool {
    meta.get(&handle).is_some_and(|meta| {
        meta.socket_type() == SocketType::Tcp && !meta.is_removed() && !meta.is_orphaned()
    })
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

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use core::any::Any;
    use spin::Mutex;

    use super::*;
    use crate::driver::{Duplex, LinkState, NetDriver, RxBuf, TxBuf};
    use crate::tuning::{NetTuning, TcpListenTuning};

    #[derive(Default)]
    struct TestDriver {
        rx: Mutex<Vec<Vec<u8>>>,
        tx: Mutex<Vec<Vec<u8>>>,
    }

    impl TestDriver {
        fn push_rx(&self, packet: Vec<u8>) {
            self.rx.lock().push(packet);
        }

        fn last_tx(&self) -> Vec<u8> {
            self.tx.lock().last().cloned().unwrap()
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

    fn test_interface() -> (InterfaceId, ManagedInterface) {
        test_interface_with_backlog(NetTuning::defaults().tcp_listen.accept_backlog)
    }

    fn test_interface_with_backlog(backlog: usize) -> (InterfaceId, ManagedInterface) {
        let driver: Arc<dyn NetDriver> = Arc::new(TestDriver::default());
        test_interface_with_driver(driver, backlog)
    }

    fn test_interface_with_driver(
        driver: Arc<dyn NetDriver>,
        backlog: usize,
    ) -> (InterfaceId, ManagedInterface) {
        let dev = Arc::new(NetDevice::new("test-net", driver));
        let id = dev.id();
        let config = IfConfig::static_v4(Ipv4Addr::LOCALHOST, 8, None);
        (
            id,
            ManagedInterface::new(
                dev,
                config,
                NetTuning::defaults().tcp,
                TcpListenTuning {
                    accept_backlog: backlog,
                },
            ),
        )
    }

    fn build_tcp_packet(
        src: smoltcp::wire::Ipv4Address,
        dst: smoltcp::wire::Ipv4Address,
        src_port: u16,
        dst_port: u16,
        seq: i32,
        ack: Option<i32>,
        control: smoltcp::wire::TcpControl,
    ) -> Vec<u8> {
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{
            IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, TcpPacket, TcpRepr, TcpSeqNumber,
        };

        let tcp = TcpRepr {
            src_port,
            dst_port,
            control,
            seq_number: TcpSeqNumber(seq),
            ack_number: ack.map(TcpSeqNumber),
            window_len: 4096,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };
        let ip = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Tcp,
            payload_len: tcp.buffer_len(),
            hop_limit: 64,
        };
        let mut bytes = vec![0u8; ip.buffer_len() + tcp.buffer_len()];
        {
            let mut ip_packet = Ipv4Packet::new_unchecked(&mut bytes);
            ip.emit(&mut ip_packet, &ChecksumCapabilities::default());
            let mut tcp_packet = TcpPacket::new_unchecked(ip_packet.payload_mut());
            tcp.emit(
                &mut tcp_packet,
                &IpAddress::Ipv4(src),
                &IpAddress::Ipv4(dst),
                &ChecksumCapabilities::default(),
            );
        }
        bytes
    }

    fn parse_tcp_seq(packet: &[u8]) -> i32 {
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{IpAddress, Ipv4Packet, Ipv4Repr, TcpPacket, TcpRepr};

        let ip_packet = Ipv4Packet::new_checked(packet).unwrap();
        let ip = Ipv4Repr::parse(&ip_packet, &ChecksumCapabilities::default()).unwrap();
        let tcp_packet = TcpPacket::new_checked(ip_packet.payload()).unwrap();
        let tcp = TcpRepr::parse(
            &tcp_packet,
            &IpAddress::Ipv4(ip.src_addr),
            &IpAddress::Ipv4(ip.dst_addr),
            &ChecksumCapabilities::default(),
        )
        .unwrap();
        tcp.seq_number.0
    }

    fn drive_inbound_handshake(
        iface: &mut ManagedInterface,
        driver: &TestDriver,
        listener: ProtocolSocketHandle,
        client_port: u16,
        server_port: u16,
    ) {
        let server = smoltcp::wire::Ipv4Address::new(127, 0, 0, 1);
        let client = smoltcp::wire::Ipv4Address::new(127, 0, 0, 2);
        driver.push_rx(build_tcp_packet(
            client,
            server,
            client_port,
            server_port,
            10_000,
            None,
            smoltcp::wire::TcpControl::Syn,
        ));
        assert!(iface.poll(NetInstant::ZERO));
        assert_eq!(
            iface.tcp_socket(listener).state(),
            smoltcp::socket::tcp::State::SynReceived
        );
        let server_seq = parse_tcp_seq(&driver.last_tx());

        driver.push_rx(build_tcp_packet(
            client,
            server,
            client_port,
            server_port,
            10_001,
            Some(server_seq.wrapping_add(1)),
            smoltcp::wire::TcpControl::None,
        ));
        assert!(iface.poll(NetInstant::from_millis(1)));
        assert_eq!(
            iface.tcp_socket(listener).state(),
            smoltcp::socket::tcp::State::Established
        );
    }

    #[test]
    fn stale_socket_handle_is_rejected_after_slot_reuse() {
        let (iface_id, mut iface) = test_interface();
        let tuning = NetTuning::defaults();

        let first_inner = iface.add_udp_socket(tuning.udp);
        let first = iface
            .make_handle(iface_id, first_inner, SocketType::Udp)
            .unwrap();
        iface.remove_socket_locked(first_inner);

        let second_inner = iface.add_udp_socket(tuning.udp);
        let second = iface
            .make_handle(iface_id, second_inner, SocketType::Udp)
            .unwrap();

        // smoltcp 会复用空槽位；generation 必须让旧 handle 失效，防止旧 fd
        // 误关或误读新 socket。
        assert_eq!(first.inner.into_smoltcp(), second.inner.into_smoltcp());
        assert_ne!(first.generation, second.generation);
        assert!(iface.handle_is_closed(first));
        assert!(iface.handle_is_live(second));
    }

    #[test]
    fn wrong_socket_type_handle_is_rejected_before_downcast() {
        let (iface_id, mut iface) = test_interface();
        let tuning = NetTuning::defaults();

        let inner = iface.add_tcp_socket(tuning.tcp);
        let tcp = iface.make_handle(iface_id, inner, SocketType::Tcp).unwrap();
        let forged_udp = NetSocketHandle {
            sock_type: SocketType::Udp,
            ..tcp
        };

        assert!(iface.handle_is_live(tcp));
        assert!(iface.handle_is_closed(forged_udp));
    }

    #[test]
    fn pending_accept_queue_records_established_listener_once() {
        let driver = Arc::new(TestDriver::default());
        let (iface_id, mut iface) = test_interface_with_driver(driver.clone(), 8);
        let tuning = NetTuning::defaults();
        let listener = iface.add_tcp_socket(tuning.tcp);
        let listen_endpoint = IpListenEndpoint {
            addr: None,
            port: 8080,
        };
        iface
            .tcp_socket_mut(listener)
            .listen(listen_endpoint)
            .unwrap();

        drive_inbound_handshake(&mut iface, &driver, listener, 40_000, 8080);
        iface.refresh_pending_tcp_accepts();
        iface.refresh_pending_tcp_accepts();

        assert_eq!(iface.pending_accepted.len(), 1);
        assert_eq!(
            iface.pending_tcp_accept(listener, listen_endpoint),
            Some(listener)
        );
        let accepted = iface
            .make_handle(iface_id, listener, SocketType::Tcp)
            .unwrap();
        assert!(iface.handle_is_live(accepted));
    }

    #[test]
    fn pending_accept_queue_prunes_accepted_and_removed_sockets() {
        let driver = Arc::new(TestDriver::default());
        let (_iface_id, mut iface) = test_interface_with_driver(driver.clone(), 8);
        let tuning = NetTuning::defaults();
        let listener = iface.add_tcp_socket(tuning.tcp);
        let listen_endpoint = IpListenEndpoint {
            addr: None,
            port: 8080,
        };
        iface
            .tcp_socket_mut(listener)
            .listen(listen_endpoint)
            .unwrap();

        drive_inbound_handshake(&mut iface, &driver, listener, 40_001, 8080);
        assert_eq!(iface.pending_accepted.len(), 1);
        let successor = iface.take_accept_successor(listener).unwrap();
        iface.mark_socket_accepted(listener);
        assert!(iface.pending_accepted.is_empty());
        assert_eq!(iface.pending_tcp_accept(listener, listen_endpoint), None);

        drive_inbound_handshake(&mut iface, &driver, successor, 40_002, 8080);
        assert_eq!(iface.pending_accepted.len(), 1);
        iface.remove_socket_locked(successor);
        assert!(iface.pending_accepted.is_empty());
    }

    #[test]
    fn pending_accept_poll_installs_successor_listener() {
        let driver = Arc::new(TestDriver::default());
        let (_iface_id, mut iface) = test_interface_with_driver(driver.clone(), 8);
        let tuning = NetTuning::defaults();
        let listener = iface.add_tcp_socket(tuning.tcp);
        let listen_endpoint = IpListenEndpoint {
            addr: None,
            port: 8080,
        };
        iface
            .tcp_socket_mut(listener)
            .listen(listen_endpoint)
            .unwrap();

        drive_inbound_handshake(&mut iface, &driver, listener, 40_005, 8080);

        let successor = iface.accept_successors[&listener.into_smoltcp()];
        assert!(iface.tcp_socket_is_live_listen(successor));
        assert_eq!(
            iface
                .tcp_socket(ProtocolSocketHandle::from_smoltcp(successor))
                .listen_endpoint(),
            listen_endpoint
        );
    }

    #[test]
    fn accept_successor_follows_converted_listener_chain() {
        let driver = Arc::new(TestDriver::default());
        let (_iface_id, mut iface) = test_interface_with_driver(driver.clone(), 8);
        let tuning = NetTuning::defaults();
        let first = iface.add_tcp_socket(tuning.tcp);
        let listen_endpoint = IpListenEndpoint {
            addr: None,
            port: 8080,
        };
        iface.tcp_socket_mut(first).listen(listen_endpoint).unwrap();

        drive_inbound_handshake(&mut iface, &driver, first, 40_006, 8080);
        let second_raw = iface.accept_successors[&first.into_smoltcp()];
        let second = ProtocolSocketHandle::from_smoltcp(second_raw);
        drive_inbound_handshake(&mut iface, &driver, second, 40_007, 8080);

        let current_listener = iface.take_accept_successor(first).unwrap();
        assert!(iface.tcp_socket_is_live_listen(current_listener.into_smoltcp()));
        assert_ne!(current_listener.into_smoltcp(), second_raw);
        assert_eq!(
            iface.pending_tcp_accept(first, listen_endpoint),
            Some(first)
        );
        iface.mark_socket_accepted(first);
        assert_eq!(
            iface.pending_tcp_accept(second, listen_endpoint),
            Some(second)
        );
    }

    #[test]
    fn pending_accept_queue_respects_backlog_limit() {
        let (_iface_id, mut iface) = test_interface_with_backlog(1);
        let tuning = NetTuning::defaults();

        let first = iface.add_tcp_socket(tuning.tcp);
        let second = iface.add_tcp_socket(tuning.tcp);
        let first_raw = first.into_smoltcp();
        let second_raw = second.into_smoltcp();
        iface.pending_accepted.push(first_raw);
        iface.enqueue_pending_tcp_accept(second_raw);

        assert_eq!(iface.pending_accepted.len(), 1);
        assert_eq!(iface.pending_accepted[0], first_raw);
        iface.remove_pending_tcp_accept(first_raw);
        iface.enqueue_pending_tcp_accept(second_raw);
        assert_eq!(iface.pending_accepted, vec![second_raw]);
    }

    #[test]
    fn pending_accept_query_keeps_preferred_visible_when_queue_is_full() {
        let driver = Arc::new(TestDriver::default());
        let (_iface_id, mut iface) = test_interface_with_driver(driver.clone(), 1);
        let tuning = NetTuning::defaults();
        let first_endpoint = IpListenEndpoint {
            addr: None,
            port: 8080,
        };
        let second_endpoint = IpListenEndpoint {
            addr: None,
            port: 8081,
        };

        let first = iface.add_tcp_socket(tuning.tcp);
        iface.tcp_socket_mut(first).listen(first_endpoint).unwrap();
        drive_inbound_handshake(&mut iface, &driver, first, 40_003, first_endpoint.port);
        assert_eq!(iface.pending_accepted.len(), 1);

        let second = iface.add_tcp_socket(tuning.tcp);
        iface
            .tcp_socket_mut(second)
            .listen(second_endpoint)
            .unwrap();
        drive_inbound_handshake(&mut iface, &driver, second, 40_004, second_endpoint.port);
        assert_eq!(iface.pending_accepted.len(), 1);
        assert_eq!(
            iface.pending_tcp_accept(second, second_endpoint),
            Some(second)
        );
    }
}
