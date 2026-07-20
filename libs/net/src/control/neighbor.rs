use hashbrown::HashTable;

use crate::{InterfaceId, IpAddr};

const MAX_NEIGHBORS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NeighborKey {
    pub interface: InterfaceId,
    pub address: IpAddr,
}

struct NeighborEntry {
    hash: u64,
    key: NeighborKey,
    mac_address: [u8; 6],
    generation: u32,
    reachable_until_ns: u64,
    stale_until_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeighborError {
    TableFull,
    InvalidAddress,
}

pub struct NeighborTable {
    seed: [u8; 16],
    entries: HashTable<NeighborEntry>,
    next_generation: u32,
}

impl NeighborTable {
    pub fn new(seed: [u8; 16]) -> Self {
        Self {
            seed,
            entries: HashTable::with_capacity(MAX_NEIGHBORS * 4 / 3 + 1),
            next_generation: 1,
        }
    }

    pub fn observe(
        &mut self,
        key: NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
    ) -> Result<u32, NeighborError> {
        if key.address.is_unspecified() || mac_address == [0; 6] || mac_address[0] & 1 != 0 {
            return Err(NeighborError::InvalidAddress);
        }
        let hash = neighbor_hash(&self.seed, key);
        if let Some(entry) = self.entries.find_mut(hash, |entry| entry.key == key) {
            entry.mac_address = mac_address;
            entry.reachable_until_ns = now_ns.saturating_add(30_000_000_000);
            entry.stale_until_ns = now_ns.saturating_add(90_000_000_000);
            return Ok(entry.generation);
        }
        if self.entries.len() == MAX_NEIGHBORS {
            return Err(NeighborError::TableFull);
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.entries.insert_unique(
            hash,
            NeighborEntry {
                hash,
                key,
                mac_address,
                generation,
                reachable_until_ns: now_ns.saturating_add(30_000_000_000),
                stale_until_ns: now_ns.saturating_add(90_000_000_000),
            },
            |entry| entry.hash,
        );
        Ok(generation)
    }

    /// stale 项仍可用于发送，但调用方应异步重新确认；过期项不再命中。
    pub fn lookup(&self, key: NeighborKey, now_ns: u64) -> Option<([u8; 6], u32, bool)> {
        let hash = neighbor_hash(&self.seed, key);
        let entry = self.entries.find(hash, |entry| entry.key == key)?;
        if now_ns >= entry.stale_until_ns {
            return None;
        }
        Some((
            entry.mac_address,
            entry.generation,
            now_ns >= entry.reachable_until_ns,
        ))
    }

    pub fn confirm(&mut self, key: NeighborKey, now_ns: u64) -> bool {
        let hash = neighbor_hash(&self.seed, key);
        let Some(entry) = self.entries.find_mut(hash, |entry| entry.key == key) else {
            return false;
        };
        if now_ns >= entry.stale_until_ns {
            return false;
        }
        entry.reachable_until_ns = now_ns.saturating_add(30_000_000_000);
        entry.stale_until_ns = now_ns.saturating_add(90_000_000_000);
        true
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.key.interface != interface);
        before - self.entries.len()
    }
}

fn neighbor_hash(seed: &[u8; 16], key: NeighborKey) -> u64 {
    let mut hash = u64::from_le_bytes(seed[..8].try_into().unwrap())
        ^ u64::from(key.interface.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let bytes: &[u8] = match &key.address {
        IpAddr::V4(address) => &address.0,
        IpAddr::V6(address) => &address.0,
    };
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ipv4Addr;

    #[test]
    fn reachable_entry_becomes_stale_then_expires() {
        let mut table = NeighborTable::new([4; 16]);
        let key = NeighborKey {
            interface: InterfaceId(1),
            address: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
        };
        table.observe(key, [2, 0, 0, 0, 0, 1], 0).unwrap();
        assert!(!table.lookup(key, 1).unwrap().2);
        assert!(table.lookup(key, 31_000_000_000).unwrap().2);
        assert!(table.lookup(key, 91_000_000_000).is_none());
    }
}
