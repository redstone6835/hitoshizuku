//! 协议无关路由表。
//!
//! 本模块维护内核网络层自己的接口选路状态，不依赖当前底层协议引擎。底层
//! 协议实现可能只支持默认路由，但 socket 创建、出站接口选择和未来转发表
//! 都应从这里获取统一结果。

use alloc::vec::Vec;

use crate::config::{CidrAddress, Gateway, IpAddr, Ipv4Addr, Ipv6Addr};
use crate::device::InterfaceId;

/// 路由条目来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSource {
    /// 接口地址自动生成的直连路由。
    Connected,
    /// 管理接口显式添加的路由。
    Static,
    /// 配置中的默认网关路由。
    Gateway,
}

/// 下一跳信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextHop {
    /// 目标地址在直连网络内，不需要网关。
    Direct,
    /// 通过指定网关转发。
    Gateway(IpAddr),
}

/// 单条路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteEntry {
    /// 目标网络前缀。
    pub destination: CidrAddress,
    /// 出口接口。
    pub iface: InterfaceId,
    /// 下一跳。
    pub next_hop: NextHop,
    /// 路由来源。
    pub source: RouteSource,
    /// 路由优先级。数值越小优先级越高；前缀长度仍是第一排序键。
    pub metric: u32,
}

impl RouteEntry {
    /// 构造直连路由。
    pub const fn connected(destination: CidrAddress, iface: InterfaceId) -> Self {
        Self {
            destination,
            iface,
            next_hop: NextHop::Direct,
            source: RouteSource::Connected,
            metric: 0,
        }
    }

    /// 构造静态 IPv4 路由。
    pub fn static_v4(
        destination: Ipv4Addr,
        prefix_len: u8,
        gateway: Ipv4Addr,
        iface: InterfaceId,
    ) -> Self {
        Self {
            destination: CidrAddress::new_v4(destination, prefix_len),
            iface,
            next_hop: NextHop::Gateway(IpAddr::V4(gateway)),
            source: RouteSource::Static,
            metric: 10,
        }
    }
}

/// 路由查询结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteLookup {
    pub iface: InterfaceId,
    pub next_hop: NextHop,
    pub prefix_len: u8,
    pub source: RouteSource,
}

/// 网络层路由表。
#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    entries: Vec<RouteEntry>,
}

impl RouteTable {
    /// 创建空路由表。
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 返回路由条目快照。
    pub fn entries(&self) -> &[RouteEntry] {
        &self.entries
    }

    /// 移除指定接口的所有路由。
    pub fn remove_iface(&mut self, iface: InterfaceId) {
        self.entries.retain(|entry| entry.iface != iface);
    }

    /// 替换指定接口的所有直连路由。
    pub fn replace_connected(&mut self, iface: InterfaceId, addresses: &[CidrAddress]) {
        self.entries
            .retain(|entry| !(entry.iface == iface && entry.source == RouteSource::Connected));
        for &address in addresses {
            self.upsert(RouteEntry::connected(normalize_cidr(address), iface));
        }
    }

    /// 替换指定接口的配置网关路由。
    pub fn replace_gateway(&mut self, iface: InterfaceId, gateway: Option<Gateway>) {
        self.entries
            .retain(|entry| !(entry.iface == iface && entry.source == RouteSource::Gateway));
        match gateway {
            Some(Gateway::V4(gateway)) => self.upsert(RouteEntry {
                destination: CidrAddress::new_v4(Ipv4Addr::UNSPECIFIED, 0),
                iface,
                next_hop: NextHop::Gateway(IpAddr::V4(gateway)),
                source: RouteSource::Gateway,
                metric: 100,
            }),
            Some(Gateway::V6(gateway)) => self.upsert(RouteEntry {
                destination: CidrAddress::new_v6(Ipv6Addr::UNSPECIFIED, 0),
                iface,
                next_hop: NextHop::Gateway(IpAddr::V6(gateway)),
                source: RouteSource::Gateway,
                metric: 100,
            }),
            Some(Gateway::DualStack { v4, v6 }) => {
                self.upsert(RouteEntry {
                    destination: CidrAddress::new_v4(Ipv4Addr::UNSPECIFIED, 0),
                    iface,
                    next_hop: NextHop::Gateway(IpAddr::V4(v4)),
                    source: RouteSource::Gateway,
                    metric: 100,
                });
                self.upsert(RouteEntry {
                    destination: CidrAddress::new_v6(Ipv6Addr::UNSPECIFIED, 0),
                    iface,
                    next_hop: NextHop::Gateway(IpAddr::V6(v6)),
                    source: RouteSource::Gateway,
                    metric: 100,
                });
            }
            None => {}
        }
    }

    /// 替换指定接口的 IPv4 默认网关，不影响同接口 IPv6 默认网关。
    pub fn replace_gateway_v4(&mut self, iface: InterfaceId, gateway: Option<Ipv4Addr>) {
        self.entries.retain(|entry| {
            !(entry.iface == iface
                && entry.source == RouteSource::Gateway
                && matches!(entry.destination.addr, IpAddr::V4(_)))
        });
        if let Some(gateway) = gateway {
            self.upsert(RouteEntry {
                destination: CidrAddress::new_v4(Ipv4Addr::UNSPECIFIED, 0),
                iface,
                next_hop: NextHop::Gateway(IpAddr::V4(gateway)),
                source: RouteSource::Gateway,
                metric: 100,
            });
        }
    }

    /// 添加或替换静态路由。
    pub fn upsert(&mut self, entry: RouteEntry) {
        let entry = RouteEntry {
            destination: normalize_cidr(entry.destination),
            ..entry
        };
        if let Some(existing) = self.entries.iter_mut().find(|existing| {
            existing.iface == entry.iface
                && existing.source == entry.source
                && existing.destination == entry.destination
        }) {
            *existing = entry;
            self.sort_entries();
            return;
        }
        self.entries.push(entry);
        self.sort_entries();
    }

    /// 删除静态路由。
    pub fn remove_static(&mut self, iface: InterfaceId, destination: CidrAddress) {
        let destination = normalize_cidr(destination);
        self.entries.retain(|entry| {
            !(entry.iface == iface
                && entry.source == RouteSource::Static
                && entry.destination == destination)
        });
    }

    /// 查询目标地址的最佳路由。
    pub fn lookup(&self, remote: &IpAddr) -> Option<RouteLookup> {
        self.entries
            .iter()
            .find(|entry| cidr_contains(&entry.destination, remote))
            .map(|entry| RouteLookup {
                iface: entry.iface,
                next_hop: entry.next_hop,
                prefix_len: entry.destination.prefix_len,
                source: entry.source,
            })
    }

    fn sort_entries(&mut self) {
        self.entries
            .sort_by(|a, b| route_rank(b).cmp(&route_rank(a)));
    }
}

fn route_rank(entry: &RouteEntry) -> (u8, core::cmp::Reverse<u32>, u8) {
    let source_rank = match entry.source {
        RouteSource::Connected => 3,
        RouteSource::Static => 2,
        RouteSource::Gateway => 1,
    };
    (
        entry.destination.prefix_len,
        core::cmp::Reverse(entry.metric),
        source_rank,
    )
}

fn normalize_cidr(cidr: CidrAddress) -> CidrAddress {
    let prefix_len = match cidr.addr {
        IpAddr::V4(_) => cidr.prefix_len.min(32),
        IpAddr::V6(_) => cidr.prefix_len.min(128),
    };
    CidrAddress {
        addr: mask_addr(cidr.addr, prefix_len),
        prefix_len,
    }
}

fn mask_addr(addr: IpAddr, prefix_len: u8) -> IpAddr {
    match addr {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr(
            (u32::from_be_bytes(v4.0) & mask32(prefix_len)).to_be_bytes(),
        )),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr(
            (u128::from_be_bytes(v6.0) & mask128(prefix_len)).to_be_bytes(),
        )),
    }
}

fn cidr_contains(cidr: &CidrAddress, addr: &IpAddr) -> bool {
    match (cidr.addr, *addr) {
        (IpAddr::V4(network), IpAddr::V4(addr)) => {
            let mask = mask32(cidr.prefix_len.min(32));
            (u32::from_be_bytes(network.0) & mask) == (u32::from_be_bytes(addr.0) & mask)
        }
        (IpAddr::V6(network), IpAddr::V6(addr)) => {
            let mask = mask128(cidr.prefix_len.min(128));
            (u128::from_be_bytes(network.0) & mask) == (u128::from_be_bytes(addr.0) & mask)
        }
        _ => false,
    }
}

fn mask32(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    }
}

fn mask128(prefix_len: u8) -> u128 {
    if prefix_len == 0 {
        0
    } else {
        u128::MAX << (128 - prefix_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(raw: u32) -> InterfaceId {
        InterfaceId(raw)
    }

    #[test]
    fn lookup_prefers_longest_prefix() {
        let mut table = RouteTable::new();
        table.upsert(RouteEntry::static_v4(
            Ipv4Addr::UNSPECIFIED,
            0,
            Ipv4Addr::new(10, 0, 0, 1),
            iface(1),
        ));
        table.upsert(RouteEntry::static_v4(
            Ipv4Addr::new(10, 1, 0, 0),
            16,
            Ipv4Addr::new(10, 1, 0, 1),
            iface(2),
        ));

        let route = table
            .lookup(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)))
            .unwrap();
        assert_eq!(route.iface, iface(2));
        assert_eq!(route.prefix_len, 16);
    }

    #[test]
    fn entries_are_kept_in_lookup_priority_order() {
        let mut table = RouteTable::new();
        table.upsert(RouteEntry {
            destination: CidrAddress::new_v4(Ipv4Addr::UNSPECIFIED, 0),
            iface: iface(1),
            next_hop: NextHop::Gateway(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            source: RouteSource::Gateway,
            metric: 100,
        });
        table.upsert(RouteEntry::static_v4(
            Ipv4Addr::new(10, 1, 0, 0),
            16,
            Ipv4Addr::new(10, 1, 0, 1),
            iface(2),
        ));
        table.upsert(RouteEntry::connected(
            CidrAddress::new_v4(Ipv4Addr::new(10, 1, 2, 0), 24),
            iface(3),
        ));

        assert_eq!(table.entries()[0].iface, iface(3));
        assert_eq!(table.entries()[1].iface, iface(2));
        assert_eq!(table.entries()[2].iface, iface(1));

        table.upsert(RouteEntry::static_v4(
            Ipv4Addr::new(10, 1, 0, 0),
            16,
            Ipv4Addr::new(10, 1, 0, 254),
            iface(2),
        ));
        assert_eq!(
            table.entries()[1].next_hop,
            NextHop::Gateway(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 254)))
        );
    }

    #[test]
    fn connected_routes_are_replaced_per_interface() {
        let mut table = RouteTable::new();
        table.replace_connected(
            iface(7),
            &[CidrAddress::new_v4(Ipv4Addr::new(192, 168, 1, 10), 24)],
        );
        table.replace_connected(
            iface(7),
            &[CidrAddress::new_v4(Ipv4Addr::new(172, 16, 5, 9), 16)],
        );

        assert!(
            table
                .lookup(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)))
                .is_none()
        );
        assert_eq!(
            table
                .lookup(&IpAddr::V4(Ipv4Addr::new(172, 16, 1, 2)))
                .unwrap()
                .iface,
            iface(7)
        );
    }

    #[test]
    fn ipv4_gateway_replace_preserves_ipv6_default_route() {
        let mut table = RouteTable::new();
        table.replace_gateway(
            iface(3),
            Some(Gateway::DualStack {
                v4: Ipv4Addr::new(10, 0, 0, 1),
                v6: Ipv6Addr::LOCALHOST,
            }),
        );
        table.replace_gateway_v4(iface(3), Some(Ipv4Addr::new(10, 0, 0, 254)));

        let v4 = table
            .lookup(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))
            .unwrap();
        let v6 = table.lookup(&IpAddr::V6(Ipv6Addr::LOCALHOST)).unwrap();

        assert_eq!(v4.next_hop, NextHop::Gateway(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 254))));
        assert_eq!(v6.next_hop, NextHop::Gateway(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }
}
