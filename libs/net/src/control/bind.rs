use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use spin::RwLock;

use crate::{AddressFamily, InterfaceId, IpAddr, ShardId, TransportProtocol};

const EPHEMERAL_START: u16 = 49_152;
const EPHEMERAL_COUNT: u16 = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindAddress {
    Any,
    Specified(IpAddr),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BindOptions {
    pub reuse_address: bool,
    pub reuse_port: bool,
    pub v6_only: bool,
    pub multicast_or_broadcast: bool,
    pub free_bind: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindRequest {
    pub owner: u64,
    pub family: AddressFamily,
    pub protocol: TransportProtocol,
    pub address: BindAddress,
    pub port: u16,
    pub interface: Option<InterfaceId>,
    pub options: BindOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindToken {
    pub id: u64,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindError {
    InvalidAddress,
    AddressInUse,
    NoPorts,
    UnknownReservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BindKey {
    family: AddressFamily,
    protocol: TransportProtocol,
    port: u16,
    interface: Option<InterfaceId>,
}

#[derive(Clone, Copy)]
struct Binding {
    token: BindToken,
    request: BindRequest,
}

struct BindState {
    groups: BTreeMap<BindKey, Vec<Binding>>,
    next_token: u64,
    cursors: Vec<u16>,
    strides: Vec<u16>,
}

pub struct BindRegistry {
    state: RwLock<BindState>,
}

impl BindRegistry {
    pub fn new(shards: usize, seed: &[u8; 16]) -> Self {
        let shards = shards.max(1);
        let mut cursors = Vec::with_capacity(shards);
        let mut strides = Vec::with_capacity(shards);
        for shard in 0..shards {
            let offset = (shard * 2) % seed.len();
            let raw = u16::from_le_bytes([seed[offset], seed[(offset + 1) % seed.len()]]);
            cursors.push(raw % EPHEMERAL_COUNT);
            strides.push(((raw.rotate_left(5) | 1) % EPHEMERAL_COUNT) | 1);
        }
        Self {
            state: RwLock::new(BindState {
                groups: BTreeMap::new(),
                next_token: 1,
                cursors,
                strides,
            }),
        }
    }

    pub fn reserve(&self, request: BindRequest) -> Result<BindToken, BindError> {
        if !address_matches_family(request.family, request.address) || request.port == 0 {
            return Err(BindError::InvalidAddress);
        }
        let mut state = self.state.write();
        reserve_locked(&mut state, request)
    }

    pub fn reserve_ephemeral(
        &self,
        mut request: BindRequest,
        shard: ShardId,
    ) -> Result<BindToken, BindError> {
        if !address_matches_family(request.family, request.address) || request.port != 0 {
            return Err(BindError::InvalidAddress);
        }
        let mut state = self.state.write();
        let shard_index = usize::from(shard.0) % state.cursors.len();
        let mut cursor = state.cursors[shard_index];
        let stride = state.strides[shard_index];
        for _ in 0..EPHEMERAL_COUNT {
            request.port = EPHEMERAL_START + cursor;
            if let Ok(token) = reserve_locked(&mut state, request) {
                state.cursors[shard_index] = cursor.wrapping_add(stride) % EPHEMERAL_COUNT;
                return Ok(token);
            }
            cursor = cursor.wrapping_add(stride) % EPHEMERAL_COUNT;
        }
        Err(BindError::NoPorts)
    }

    pub fn release(&self, token: BindToken) -> Result<(), BindError> {
        let mut state = self.state.write();
        let key = state.groups.iter().find_map(|(key, group)| {
            group
                .iter()
                .any(|entry| entry.token == token)
                .then_some(*key)
        });
        let Some(key) = key else {
            return Err(BindError::UnknownReservation);
        };
        let group = state.groups.get_mut(&key).expect("bind group 存在");
        group.retain(|entry| entry.token != token);
        if group.is_empty() {
            state.groups.remove(&key);
        }
        Ok(())
    }
}

fn reserve_locked(state: &mut BindState, request: BindRequest) -> Result<BindToken, BindError> {
    let key = BindKey {
        family: request.family,
        protocol: request.protocol,
        port: request.port,
        interface: request.interface,
    };
    for (existing_key, group) in state.groups.iter() {
        if existing_key.protocol != key.protocol
            || existing_key.port != key.port
            || !interface_overlaps(existing_key.interface, key.interface)
            || !family_overlaps(*existing_key, group, key, request)
        {
            continue;
        }
        for existing in group {
            if address_overlaps(existing.request.address, request.address)
                && !can_share(existing.request, request)
            {
                return Err(BindError::AddressInUse);
            }
        }
    }
    let token = BindToken {
        id: state.next_token,
        port: request.port,
    };
    state.next_token = state.next_token.checked_add(1).expect("BindToken 已耗尽");
    state
        .groups
        .entry(key)
        .or_default()
        .push(Binding { token, request });
    Ok(token)
}

fn can_share(left: BindRequest, right: BindRequest) -> bool {
    if left.protocol == TransportProtocol::Udp
        && left.options.multicast_or_broadcast
        && right.options.multicast_or_broadcast
    {
        return left.options.reuse_address && right.options.reuse_address;
    }
    left.options.reuse_port && right.options.reuse_port
}

fn address_overlaps(left: BindAddress, right: BindAddress) -> bool {
    matches!(left, BindAddress::Any) || matches!(right, BindAddress::Any) || left == right
}

fn interface_overlaps(left: Option<InterfaceId>, right: Option<InterfaceId>) -> bool {
    left.is_none() || right.is_none() || left == right
}

fn family_overlaps(
    left_key: BindKey,
    left_group: &[Binding],
    right_key: BindKey,
    right: BindRequest,
) -> bool {
    if left_key.family == right_key.family {
        return true;
    }
    let cross_family_wildcard = |request: BindRequest| {
        request.family == AddressFamily::Ipv6
            && request.address == BindAddress::Any
            && !request.options.v6_only
    };
    cross_family_wildcard(right)
        || left_group
            .iter()
            .any(|entry| cross_family_wildcard(entry.request))
}

fn address_matches_family(family: AddressFamily, address: BindAddress) -> bool {
    matches!(
        (family, address),
        (_, BindAddress::Any)
            | (AddressFamily::Ipv4, BindAddress::Specified(IpAddr::V4(_)))
            | (AddressFamily::Ipv6, BindAddress::Specified(IpAddr::V6(_)))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ipv4Addr;

    fn udp(owner: u64, address: BindAddress, options: BindOptions) -> BindRequest {
        BindRequest {
            owner,
            family: AddressFamily::Ipv4,
            protocol: TransportProtocol::Udp,
            address,
            port: 9000,
            interface: None,
            options,
        }
    }

    #[test]
    fn wildcard_conflicts_with_specific_unicast() {
        let registry = BindRegistry::new(1, &[7; 16]);
        registry
            .reserve(udp(1, BindAddress::Any, BindOptions::default()))
            .unwrap();
        assert_eq!(
            registry.reserve(udp(
                2,
                BindAddress::Specified(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                BindOptions::default(),
            )),
            Err(BindError::AddressInUse)
        );
    }

    #[test]
    fn reuse_port_group_can_share_unicast() {
        let registry = BindRegistry::new(1, &[9; 16]);
        let options = BindOptions {
            reuse_port: true,
            ..BindOptions::default()
        };
        registry.reserve(udp(1, BindAddress::Any, options)).unwrap();
        registry.reserve(udp(2, BindAddress::Any, options)).unwrap();
    }

    #[test]
    fn ephemeral_port_is_reserved_atomically() {
        let registry = BindRegistry::new(2, &[11; 16]);
        let mut request = udp(1, BindAddress::Any, BindOptions::default());
        request.port = 0;
        let token = registry.reserve_ephemeral(request, ShardId(1)).unwrap();
        assert!((EPHEMERAL_START..=u16::MAX).contains(&token.port));
        registry.release(token).unwrap();
    }
}
