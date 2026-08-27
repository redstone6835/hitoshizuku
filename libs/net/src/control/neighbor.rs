use alloc::vec::Vec;
use hashbrown::HashTable;
use spin::Mutex;

use crate::{InterfaceId, IpAddr};

const MAX_NEIGHBORS: usize = 512;

/// 邻居镜像表条目（跨 shard 聚合快照，供 netlink/procfs 观测）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeighborSnapshotEntry {
    pub interface: InterfaceId,
    pub address: IpAddr,
    pub mac: [u8; 6],
    /// NUD 状态位：REACHABLE=0x02 / STALE=0x04。
    pub nud_state: u16,
}

/// 全局邻居镜像：observe/confirm/invalidate 时与 per-shard 表双写。
/// 只服务于观测接口（RTM_GETNEIGH、/proc/net/arp），不做转发决策。
static NEIGHBOR_MIRROR: Mutex<Vec<NeighborSnapshotEntry>> = Mutex::new(Vec::new());

/// 返回全部 shard 的邻居镜像快照。
pub fn neighbor_snapshot() -> Vec<NeighborSnapshotEntry> {
    NEIGHBOR_MIRROR.lock().clone()
}

/// 清空当前网络栈代际发布的邻居镜像，并释放其后备存储。
///
/// 镜像条目来自动态 net.stack，但由常驻 netlink/procfs 观测接口持有。代际卸载时必须
/// 同时丢弃旧条目和 Vec 容量，避免旧邻居泄漏到新代际，也避免 allocator 继续记账到
/// 已卸载的 ELM owner。
pub fn clear_neighbor_snapshot() -> usize {
    let retired = {
        let mut mirror = NEIGHBOR_MIRROR.lock();
        core::mem::take(&mut *mirror)
    };
    let removed = retired.len();
    drop(retired);
    removed
}

fn mirror_observe(key: NeighborKey, mac_address: [u8; 6], _now_ns: u64, reachable: bool) {
    // 邻居镜像属于常驻内核，不能把延迟扩容记到发起观察的可卸载 ELM。
    let _accounting = allocator::suspend_implicit_allocation_accounting();
    let mut mirror = NEIGHBOR_MIRROR.lock();
    let entry = NeighborSnapshotEntry {
        interface: key.interface,
        address: key.address,
        mac: mac_address,
        nud_state: if reachable { 0x02 } else { 0x04 },
    };
    if let Some(existing) = mirror
        .iter_mut()
        .find(|candidate| candidate.interface == key.interface && candidate.address == key.address)
    {
        *existing = entry;
        return;
    }
    mirror.push(entry);
}

fn mirror_remove(interface: InterfaceId) {
    NEIGHBOR_MIRROR
        .lock()
        .retain(|entry| entry.interface != interface);
}

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
            mirror_observe(key, mac_address, now_ns, true);
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
        mirror_observe(key, mac_address, now_ns, true);
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
        mirror_observe(key, entry.mac_address, now_ns, true);
        true
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.key.interface != interface);
        mirror_remove(interface);
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

    static NEIGHBOR_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn reachable_entry_becomes_stale_then_expires() {
        let _guard = NEIGHBOR_TEST_LOCK.lock();
        NEIGHBOR_MIRROR.lock().clear();
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

    #[test]
    fn clearing_neighbor_snapshot_removes_previous_generation_entries() {
        let _guard = NEIGHBOR_TEST_LOCK.lock();
        NEIGHBOR_MIRROR.lock().clear();
        let mut table = NeighborTable::new([5; 16]);
        let key = NeighborKey {
            interface: InterfaceId(2),
            address: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 3)),
        };

        table.observe(key, [2, 0, 0, 0, 0, 2], 0).unwrap();
        assert_eq!(neighbor_snapshot().len(), 1);
        assert_eq!(clear_neighbor_snapshot(), 1);
        assert!(neighbor_snapshot().is_empty());
        assert_eq!(NEIGHBOR_MIRROR.lock().capacity(), 0);
    }
}
