use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::RwLock;

use crate::{InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr, NetDeviceId};

const MAIN_ROUTE_TABLE: u8 = 0;
const MAX_ROUTE_TABLES: usize = 8;
const MAX_POLICY_RULES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceSnapshot {
    pub id: InterfaceId,
    pub device: NetDeviceId,
    pub mac_address: [u8; 6],
    pub mtu: u32,
    pub running: bool,
    pub loopback: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressEntry {
    pub interface: InterfaceId,
    pub address: IpAddr,
    pub prefix_len: u8,
    pub primary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    pub table: u8,
    pub network: IpAddr,
    pub prefix_len: u8,
    pub gateway: Option<IpAddr>,
    pub interface: InterfaceId,
    pub metric: u32,
    pub mtu: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyRule {
    pub mark: u32,
    pub mask: u32,
    pub table: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteDecision {
    pub interface: InterfaceId,
    pub source: IpAddr,
    pub next_hop: IpAddr,
    pub mtu: u32,
    pub table: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidInterface,
    InvalidAddress,
    InvalidRoute,
    TooManyRouteTables,
    TooManyPolicyRules,
    MissingMainRouteTable,
    GatewayUnreachable,
    GenerationNotIncreasing,
    NoRoute,
    NoSourceAddress,
}

#[derive(Clone)]
struct RadixNode {
    children: [Option<usize>; 2],
    route: Option<RouteEntry>,
}

impl RadixNode {
    const fn new() -> Self {
        Self {
            children: [None, None],
            route: None,
        }
    }
}

#[derive(Clone)]
struct RouteTrie {
    nodes: Vec<RadixNode>,
    bit_len: u8,
}

impl RouteTrie {
    fn new(bit_len: u8) -> Self {
        Self {
            nodes: alloc::vec![RadixNode::new()],
            bit_len,
        }
    }

    fn insert(&mut self, route: RouteEntry) -> Result<(), ConfigError> {
        if route.prefix_len > self.bit_len {
            return Err(ConfigError::InvalidRoute);
        }
        let mut node = 0usize;
        for bit_index in 0..route.prefix_len {
            let bit = address_bit(route.network, bit_index).ok_or(ConfigError::InvalidRoute)?;
            let next = match self.nodes[node].children[bit] {
                Some(next) => next,
                None => {
                    let next = self.nodes.len();
                    self.nodes.push(RadixNode::new());
                    self.nodes[node].children[bit] = Some(next);
                    next
                }
            };
            node = next;
        }
        let replace = self.nodes[node].route.is_none_or(|current| {
            (route.metric, route.interface) < (current.metric, current.interface)
        });
        if replace {
            self.nodes[node].route = Some(route);
        }
        Ok(())
    }

    fn lookup(&self, address: IpAddr) -> Option<RouteEntry> {
        let mut node = 0usize;
        let mut best = self.nodes[0].route;
        for bit_index in 0..self.bit_len {
            let bit = address_bit(address, bit_index)?;
            let Some(next) = self.nodes[node].children[bit] else {
                break;
            };
            node = next;
            if self.nodes[node].route.is_some() {
                best = self.nodes[node].route;
            }
        }
        best
    }
}

#[derive(Clone)]
struct RouteTable {
    id: u8,
    ipv4: RouteTrie,
    ipv6: RouteTrie,
}

impl RouteTable {
    fn new(id: u8) -> Self {
        Self {
            id,
            ipv4: RouteTrie::new(32),
            ipv6: RouteTrie::new(128),
        }
    }

    fn insert(&mut self, route: RouteEntry) -> Result<(), ConfigError> {
        match route.network {
            IpAddr::V4(_) => self.ipv4.insert(route),
            IpAddr::V6(_) => self.ipv6.insert(route),
        }
    }

    fn lookup(&self, address: IpAddr) -> Option<RouteEntry> {
        match address {
            IpAddr::V4(_) => self.ipv4.lookup(address),
            IpAddr::V6(_) => self.ipv6.lookup(address),
        }
    }
}

#[derive(Clone)]
pub struct RouteSnapshot {
    tables: Vec<RouteTable>,
}

impl RouteSnapshot {
    pub fn build(routes: &[RouteEntry]) -> Result<Self, ConfigError> {
        let mut table_ids = Vec::new();
        for route in routes {
            validate_route(*route)?;
            if !table_ids.contains(&route.table) {
                if table_ids.len() == MAX_ROUTE_TABLES {
                    return Err(ConfigError::TooManyRouteTables);
                }
                table_ids.push(route.table);
            }
        }
        if !table_ids.contains(&MAIN_ROUTE_TABLE) {
            if table_ids.len() == MAX_ROUTE_TABLES {
                return Err(ConfigError::TooManyRouteTables);
            }
            table_ids.push(MAIN_ROUTE_TABLE);
        }
        table_ids.sort_unstable();
        let mut tables = table_ids
            .into_iter()
            .map(RouteTable::new)
            .collect::<Vec<_>>();
        for route in routes {
            tables
                .iter_mut()
                .find(|table| table.id == route.table)
                .expect("route table 已创建")
                .insert(*route)?;
        }
        Ok(Self { tables })
    }

    pub fn lookup(&self, table: u8, address: IpAddr) -> Option<RouteEntry> {
        self.tables
            .iter()
            .find(|candidate| candidate.id == table)
            .and_then(|candidate| candidate.lookup(address))
    }
}

#[derive(Clone)]
pub struct ConfigSnapshot {
    pub generation: u64,
    pub interfaces: Vec<InterfaceSnapshot>,
    pub addresses: Vec<AddressEntry>,
    pub routes: RouteSnapshot,
    pub policy: Vec<PolicyRule>,
}

impl ConfigSnapshot {
    pub fn new(
        generation: u64,
        interfaces: Vec<InterfaceSnapshot>,
        addresses: Vec<AddressEntry>,
        routes: Vec<RouteEntry>,
        policy: Vec<PolicyRule>,
    ) -> Result<Self, ConfigError> {
        if policy.len() > MAX_POLICY_RULES {
            return Err(ConfigError::TooManyPolicyRules);
        }
        for interface in &interfaces {
            if interface.id.0 == 0 || interface.mtu == 0 {
                return Err(ConfigError::InvalidInterface);
            }
        }
        for address in &addresses {
            let max_prefix = match address.address {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            if address.prefix_len > max_prefix
                || !interfaces
                    .iter()
                    .any(|interface| interface.id == address.interface)
            {
                return Err(ConfigError::InvalidAddress);
            }
        }
        for rule in &policy {
            if rule.table >= MAX_ROUTE_TABLES as u8 {
                return Err(ConfigError::InvalidRoute);
            }
        }
        let route_snapshot = RouteSnapshot::build(&routes)?;
        for route in &routes {
            if !interfaces
                .iter()
                .any(|interface| interface.id == route.interface)
            {
                return Err(ConfigError::InvalidRoute);
            }
            if let Some(gateway) = route.gateway {
                let reachable = addresses.iter().any(|address| {
                    address.interface == route.interface
                        && same_family(address.address, gateway)
                        && prefix_matches(gateway, address.address, address.prefix_len)
                }) || routes.iter().any(|candidate| {
                    candidate.table == route.table
                        && candidate.interface == route.interface
                        && candidate.gateway.is_none()
                        && same_family(candidate.network, gateway)
                        && prefix_matches(gateway, candidate.network, candidate.prefix_len)
                });
                if !reachable {
                    return Err(ConfigError::GatewayUnreachable);
                }
            }
        }
        Ok(Self {
            generation,
            interfaces,
            addresses,
            routes: route_snapshot,
            policy,
        })
    }

    pub fn empty() -> Self {
        Self {
            generation: 0,
            interfaces: Vec::new(),
            addresses: Vec::new(),
            routes: RouteSnapshot::build(&[]).expect("空路由表有效"),
            policy: Vec::new(),
        }
    }

    pub fn is_local_address(&self, interface: InterfaceId, address: IpAddr) -> bool {
        self.addresses
            .iter()
            .any(|entry| entry.interface == interface && entry.address == address)
    }

    pub fn route(
        &self,
        destination: IpAddr,
        mark: u32,
        bound_source: Option<IpAddr>,
        interface_scope: Option<InterfaceId>,
    ) -> Result<RouteDecision, ConfigError> {
        let table = self
            .policy
            .iter()
            .find(|rule| mark & rule.mask == rule.mark & rule.mask)
            .map(|rule| rule.table)
            .unwrap_or(MAIN_ROUTE_TABLE);
        let route = self
            .routes
            .lookup(table, destination)
            .ok_or(ConfigError::NoRoute)?;
        if interface_scope.is_some_and(|scope| scope != route.interface) {
            return Err(ConfigError::NoRoute);
        }
        let interface = self
            .interfaces
            .iter()
            .find(|interface| interface.id == route.interface && interface.running)
            .ok_or(ConfigError::NoRoute)?;
        let source = match bound_source {
            Some(source)
                if self.is_local_address(route.interface, source)
                    && same_family(source, destination) =>
            {
                source
            }
            Some(_) => return Err(ConfigError::NoSourceAddress),
            None => self.select_source(route.interface, destination)?,
        };
        Ok(RouteDecision {
            interface: route.interface,
            source,
            next_hop: route.gateway.unwrap_or(destination),
            mtu: route.mtu.unwrap_or(interface.mtu).min(interface.mtu),
            table,
        })
    }

    fn select_source(
        &self,
        interface: InterfaceId,
        destination: IpAddr,
    ) -> Result<IpAddr, ConfigError> {
        self.addresses
            .iter()
            .filter(|entry| entry.interface == interface && same_family(entry.address, destination))
            .max_by_key(|entry| {
                (
                    common_prefix_len(entry.address, destination),
                    u8::from(entry.primary),
                    core::cmp::Reverse(entry.address),
                )
            })
            .map(|entry| entry.address)
            .ok_or(ConfigError::NoSourceAddress)
    }
}

pub struct ConfigStore {
    current: RwLock<Arc<ConfigSnapshot>>,
}

impl ConfigStore {
    pub fn new(initial: ConfigSnapshot) -> Self {
        Self {
            current: RwLock::new(Arc::new(initial)),
        }
    }

    pub fn snapshot(&self) -> Arc<ConfigSnapshot> {
        Arc::clone(&self.current.read())
    }

    pub fn publish(&self, next: ConfigSnapshot) -> Result<(), ConfigError> {
        let next = Arc::new(next);
        let mut current = self.current.write();
        if next.generation <= current.generation {
            return Err(ConfigError::GenerationNotIncreasing);
        }
        *current = next;
        Ok(())
    }
}

fn validate_route(route: RouteEntry) -> Result<(), ConfigError> {
    let max_prefix = match route.network {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if route.table >= MAX_ROUTE_TABLES as u8
        || route.prefix_len > max_prefix
        || route
            .gateway
            .is_some_and(|gateway| !same_family(route.network, gateway))
    {
        return Err(ConfigError::InvalidRoute);
    }
    Ok(())
}

fn same_family(left: IpAddr, right: IpAddr) -> bool {
    matches!(
        (left, right),
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
    )
}

fn address_bit(address: IpAddr, bit_index: u8) -> Option<usize> {
    let byte_index = usize::from(bit_index / 8);
    let shift = 7 - bit_index % 8;
    match address {
        IpAddr::V4(Ipv4Addr(bytes)) => bytes
            .get(byte_index)
            .map(|byte| usize::from(byte >> shift & 1)),
        IpAddr::V6(Ipv6Addr(bytes)) => bytes
            .get(byte_index)
            .map(|byte| usize::from(byte >> shift & 1)),
    }
}

fn prefix_matches(address: IpAddr, network: IpAddr, prefix_len: u8) -> bool {
    if !same_family(address, network) {
        return false;
    }
    (0..prefix_len).all(|bit| address_bit(address, bit) == address_bit(network, bit))
}

fn common_prefix_len(left: IpAddr, right: IpAddr) -> u8 {
    if !same_family(left, right) {
        return 0;
    }
    let bits = match left {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    (0..bits)
        .take_while(|bit| address_bit(left, *bit) == address_bit(right, *bit))
        .count() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(id: u32) -> InterfaceSnapshot {
        InterfaceSnapshot {
            id: InterfaceId(id),
            device: NetDeviceId(id),
            mac_address: [id as u8; 6],
            mtu: 1500,
            running: true,
            loopback: false,
        }
    }

    #[test]
    fn route_lookup_uses_longest_prefix_then_metric() {
        let config = ConfigSnapshot::new(
            1,
            alloc::vec![interface(1)],
            alloc::vec![AddressEntry {
                interface: InterfaceId(1),
                address: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
                prefix_len: 24,
                primary: true,
            }],
            alloc::vec![
                RouteEntry {
                    table: 0,
                    network: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    prefix_len: 0,
                    gateway: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2))),
                    interface: InterfaceId(1),
                    metric: 100,
                    mtu: None,
                },
                RouteEntry {
                    table: 0,
                    network: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 0)),
                    prefix_len: 24,
                    gateway: None,
                    interface: InterfaceId(1),
                    metric: 0,
                    mtu: None,
                },
            ],
            Vec::new(),
        )
        .unwrap();
        let decision = config
            .route(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 99)), 0, None, None)
            .unwrap();
        assert_eq!(decision.next_hop, IpAddr::V4(Ipv4Addr::new(10, 0, 2, 99)));
        assert_eq!(decision.source, IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)));
    }

    #[test]
    fn config_publication_is_monotonic() {
        let store = ConfigStore::new(ConfigSnapshot::empty());
        assert_eq!(
            store.publish(ConfigSnapshot::empty()),
            Err(ConfigError::GenerationNotIncreasing)
        );
    }
}
