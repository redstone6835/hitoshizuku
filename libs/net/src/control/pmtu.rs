use alloc::collections::BTreeMap;

use crate::{InterfaceId, IpAddr};

const PMTU_LIFETIME_NS: u64 = 600_000_000_000;
const MAX_PMTU_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PmtuKey {
    pub interface: InterfaceId,
    pub destination: IpAddr,
}

#[derive(Clone, Copy)]
struct PmtuEntry {
    mtu: u32,
    expires_ns: u64,
}

/// 分片本地的有界 PMTU 缓存；过期项在命中或插入时惰性清理。
pub struct PmtuCache {
    entries: BTreeMap<PmtuKey, PmtuEntry>,
}

impl PmtuCache {
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn observe(&mut self, key: PmtuKey, mtu: u32, now_ns: u64) -> u32 {
        self.expire(now_ns);
        let minimum = match key.destination {
            IpAddr::V4(_) => 68,
            IpAddr::V6(_) => 1280,
        };
        let mtu = mtu.max(minimum);
        if self.entries.len() == MAX_PMTU_ENTRIES && !self.entries.contains_key(&key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_ns)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            PmtuEntry {
                mtu,
                expires_ns: now_ns.saturating_add(PMTU_LIFETIME_NS),
            },
        );
        mtu
    }

    pub fn effective_mtu(&mut self, key: PmtuKey, route_mtu: u32, now_ns: u64) -> u32 {
        let Some(entry) = self.entries.get(&key).copied() else {
            return route_mtu;
        };
        if entry.expires_ns <= now_ns {
            self.entries.remove(&key);
            route_mtu
        } else {
            route_mtu.min(entry.mtu)
        }
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        let before = self.entries.len();
        self.entries.retain(|key, _| key.interface != interface);
        before - self.entries.len()
    }

    fn expire(&mut self, now_ns: u64) {
        self.entries.retain(|_, entry| entry.expires_ns > now_ns);
    }
}

impl Default for PmtuCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ipv4Addr;

    #[test]
    fn pmtu_is_clamped_and_expires_after_ten_minutes() {
        let mut cache = PmtuCache::new();
        let key = PmtuKey {
            interface: InterfaceId(1),
            destination: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        };
        assert_eq!(cache.observe(key, 40, 0), 68);
        assert_eq!(cache.effective_mtu(key, 1500, 1), 68);
        assert_eq!(cache.effective_mtu(key, 1500, PMTU_LIFETIME_NS), 1500);
    }
}
