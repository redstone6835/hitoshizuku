use alloc::vec::Vec;

use hashbrown::HashTable;

use crate::{Endpoint, FlowId, IpAddr, TransportProtocol};

const MAX_FLOWS: usize = 4096;
const FLOW_HASH_MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

pub const DIRTY_INGRESS: u32 = 1 << 0;
pub const DIRTY_TX: u32 = 1 << 1;
pub const DIRTY_CONTROL: u32 = 1 << 2;
pub const DIRTY_TIMER: u32 = 1 << 3;
pub const DIRTY_ROUTE: u32 = 1 << 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub remote: Endpoint,
    pub local: Endpoint,
    pub protocol: TransportProtocol,
}

impl FlowKey {
    pub fn new(remote: Endpoint, local: Endpoint, protocol: TransportProtocol) -> Option<Self> {
        if local.port == 0
            || !matches!(
                (remote.addr, local.addr),
                (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
            )
        {
            return None;
        }
        Some(Self {
            remote,
            local,
            protocol,
        })
    }
}

#[derive(Clone, Copy)]
struct FlowSlot {
    id: FlowId,
    generation: u32,
    cell_index: u16,
}

struct FlowEntry {
    hash: u64,
    key: FlowKey,
    slot: FlowSlot,
}

struct FlowCell<T> {
    id: FlowId,
    generation: u32,
    key: FlowKey,
    value: T,
    dirty_bits: u32,
    dirty_queued: bool,
    dirty_next: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowInsertError {
    Duplicate,
    Full,
    IdExhausted,
}

pub struct FlowTable<T> {
    index: HashTable<FlowEntry>,
    cells: Vec<Option<FlowCell<T>>>,
    free: Vec<u16>,
    next_generation: u32,
    dirty_head: Option<u16>,
    dirty_tail: Option<u16>,
}

impl<T> FlowTable<T> {
    pub fn new() -> Self {
        let mut cells = Vec::with_capacity(MAX_FLOWS);
        cells.resize_with(MAX_FLOWS, || None);
        let mut free = Vec::with_capacity(MAX_FLOWS);
        free.extend((0..MAX_FLOWS as u16).rev());
        Self {
            index: HashTable::with_capacity(MAX_FLOWS * 4 / 3 + 1),
            cells,
            free,
            next_generation: 1,
            dirty_head: None,
            dirty_tail: None,
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn insert(&mut self, key: FlowKey, value: T) -> Result<FlowId, FlowInsertError> {
        let hash = flow_hash64(rss_hash(&[0; 40], &key));
        self.insert_prehashed(key, hash, value)
    }

    pub fn insert_prehashed(
        &mut self,
        key: FlowKey,
        hash: u64,
        value: T,
    ) -> Result<FlowId, FlowInsertError> {
        if self.index.find(hash, |entry| entry.key == key).is_some() {
            return Err(FlowInsertError::Duplicate);
        }
        let cell_index = self.free.pop().ok_or(FlowInsertError::Full)?;
        let id = FlowId(u32::from(cell_index) + 1);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.cells[usize::from(cell_index)] = Some(FlowCell {
            id,
            generation,
            key,
            value,
            dirty_bits: 0,
            dirty_queued: false,
            dirty_next: None,
        });
        self.index.insert_unique(
            hash,
            FlowEntry {
                hash,
                key,
                slot: FlowSlot {
                    id,
                    generation,
                    cell_index,
                },
            },
            |entry| entry.hash,
        );
        Ok(id)
    }

    pub fn find(&self, key: &FlowKey, hash: u64) -> Option<FlowId> {
        self.index
            .find(hash, |entry| entry.key == *key)
            .filter(|entry| {
                self.cells[usize::from(entry.slot.cell_index)]
                    .as_ref()
                    .is_some_and(|cell| cell.generation == entry.slot.generation)
            })
            .map(|entry| entry.slot.id)
    }

    pub fn get(&self, id: FlowId) -> Option<&T> {
        self.cell(id).map(|cell| &cell.value)
    }

    pub fn get_mut(&mut self, id: FlowId) -> Option<&mut T> {
        self.cell_mut(id).map(|cell| &mut cell.value)
    }

    pub fn key(&self, id: FlowId) -> Option<FlowKey> {
        self.cell(id).map(|cell| cell.key)
    }

    pub fn generation(&self, id: FlowId) -> Option<u32> {
        self.cell(id).map(|cell| cell.generation)
    }

    pub fn remove(&mut self, key: &FlowKey, hash: u64) -> Option<T> {
        let (entry, _) = self
            .index
            .find_entry(hash, |entry| entry.key == *key)
            .ok()?
            .remove();
        let index = usize::from(entry.slot.cell_index);
        self.unlink_dirty(entry.slot.cell_index);
        let cell = self.cells[index].take()?;
        self.free.push(entry.slot.cell_index);
        Some(cell.value)
    }

    pub fn mark_dirty(&mut self, id: FlowId, bits: u32) -> bool {
        let Some(index) = self.cell_index(id) else {
            return false;
        };
        let cell = self.cells[usize::from(index)].as_mut().unwrap();
        cell.dirty_bits |= bits;
        if cell.dirty_queued {
            return false;
        }
        cell.dirty_queued = true;
        cell.dirty_next = None;
        match self.dirty_tail {
            Some(tail) => {
                self.cells[usize::from(tail)].as_mut().unwrap().dirty_next = Some(index);
            }
            None => self.dirty_head = Some(index),
        }
        self.dirty_tail = Some(index);
        true
    }

    /// 取得队首工作并在处理前解除 queued；处理中再次标记会重新排到队尾。
    pub fn pop_dirty(&mut self) -> Option<(FlowId, u32)> {
        let index = self.dirty_head?;
        let cell = self.cells[usize::from(index)].as_mut().unwrap();
        self.dirty_head = cell.dirty_next.take();
        if self.dirty_head.is_none() {
            self.dirty_tail = None;
        }
        cell.dirty_queued = false;
        let bits = core::mem::take(&mut cell.dirty_bits);
        Some((cell.id, bits))
    }

    pub fn cell_index(&self, id: FlowId) -> Option<u16> {
        let index = id.0.checked_sub(1)? as usize;
        self.cells
            .get(index)
            .and_then(Option::as_ref)
            .filter(|cell| cell.id == id)
            .map(|_| index as u16)
    }

    fn cell(&self, id: FlowId) -> Option<&FlowCell<T>> {
        let index = usize::from(self.cell_index(id)?);
        self.cells[index].as_ref()
    }

    fn cell_mut(&mut self, id: FlowId) -> Option<&mut FlowCell<T>> {
        let index = usize::from(self.cell_index(id)?);
        self.cells[index].as_mut()
    }

    fn unlink_dirty(&mut self, target: u16) {
        let mut previous = None;
        let mut current = self.dirty_head;
        while let Some(index) = current {
            let next = self.cells[usize::from(index)]
                .as_ref()
                .and_then(|cell| cell.dirty_next);
            if index == target {
                match previous {
                    Some(previous) => {
                        self.cells[usize::from(previous)]
                            .as_mut()
                            .unwrap()
                            .dirty_next = next;
                    }
                    None => self.dirty_head = next,
                }
                if self.dirty_tail == Some(index) {
                    self.dirty_tail = previous;
                }
                break;
            }
            previous = current;
            current = next;
        }
    }
}

impl<T> Default for FlowTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

pub const fn flow_hash64(hash: u32) -> u64 {
    (hash as u64).wrapping_mul(FLOW_HASH_MULTIPLIER)
}

/// 使用硬件 RSS 相同的地址和端口顺序计算 Toeplitz hash。
pub fn rss_hash(key: &[u8; 40], flow: &FlowKey) -> u32 {
    let mut input = [0u8; 36];
    let len = match (flow.remote.addr, flow.local.addr) {
        (IpAddr::V4(remote), IpAddr::V4(local)) => {
            input[0..4].copy_from_slice(&remote.0);
            input[4..8].copy_from_slice(&local.0);
            input[8..10].copy_from_slice(&flow.remote.port.to_be_bytes());
            input[10..12].copy_from_slice(&flow.local.port.to_be_bytes());
            12
        }
        (IpAddr::V6(remote), IpAddr::V6(local)) => {
            input[0..16].copy_from_slice(&remote.0);
            input[16..32].copy_from_slice(&local.0);
            input[32..34].copy_from_slice(&flow.remote.port.to_be_bytes());
            input[34..36].copy_from_slice(&flow.local.port.to_be_bytes());
            36
        }
        _ => return 0,
    };
    toeplitz(key, &input[..len])
}

fn toeplitz(key: &[u8; 40], input: &[u8]) -> u32 {
    let mut result = 0u32;
    let mut window = u32::from_be_bytes(key[0..4].try_into().unwrap());
    for bit_index in 0..input.len() * 8 {
        if input[bit_index / 8] & (0x80 >> (bit_index % 8)) != 0 {
            result ^= window;
        }
        let next_bit = bit_index + 32;
        let incoming = if next_bit < key.len() * 8 {
            u32::from((key[next_bit / 8] >> (7 - next_bit % 8)) & 1)
        } else {
            0
        };
        window = window << 1 | incoming;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ipv4Addr, TransportProtocol};

    fn flow(remote_port: u16) -> FlowKey {
        FlowKey::new(
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
                port: remote_port,
            },
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
                port: 9000,
            },
            TransportProtocol::Udp,
        )
        .unwrap()
    }

    #[test]
    fn table_uses_full_key_after_prehashed_lookup() {
        let mut table = FlowTable::new();
        let first = flow(1000);
        let second = flow(1001);
        table.insert_prehashed(first, 7, 10).unwrap();
        table.insert_prehashed(second, 7, 20).unwrap();
        let id = table.find(&second, 7).unwrap();
        assert_eq!(table.get(id), Some(&20));
    }

    #[test]
    fn dirty_publication_deduplicates_and_requeues() {
        let mut table = FlowTable::new();
        let id = table.insert_prehashed(flow(1000), 1, ()).unwrap();
        assert!(table.mark_dirty(id, DIRTY_INGRESS));
        assert!(!table.mark_dirty(id, DIRTY_TX));
        assert_eq!(table.pop_dirty(), Some((id, DIRTY_INGRESS | DIRTY_TX)));
        assert!(table.mark_dirty(id, DIRTY_TIMER));
        assert_eq!(table.pop_dirty(), Some((id, DIRTY_TIMER)));
    }

    #[test]
    fn toeplitz_matches_standard_ipv4_four_tuple_vector() {
        let key = [
            0x6d, 0x5a, 0x56, 0xda, 0x25, 0x5b, 0x0e, 0xc2, 0x41, 0x67, 0x25, 0x3d, 0x43, 0xa3,
            0x8f, 0xb0, 0xd0, 0xca, 0x2b, 0xcb, 0xae, 0x7b, 0x30, 0xb4, 0x77, 0xcb, 0x2d, 0xa3,
            0x80, 0x30, 0xf2, 0x0c, 0x6a, 0x42, 0xb7, 0x3b, 0xbe, 0xac, 0x01, 0xfa,
        ];
        let flow = FlowKey::new(
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::new(66, 9, 149, 187)),
                port: 2794,
            },
            Endpoint {
                addr: IpAddr::V4(Ipv4Addr::new(161, 142, 100, 80)),
                port: 1766,
            },
            TransportProtocol::Tcp,
        )
        .unwrap();
        assert_eq!(rss_hash(&key, &flow), 0x51cc_c178);
    }
}
