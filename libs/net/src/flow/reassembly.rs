use alloc::vec::Vec;

use crate::buf::{DropReason, PacketChain, PacketMetadata};
use crate::pipeline::FrontendPacket;
use crate::{InterfaceId, IpAddr};

const MAX_DATAGRAMS_PER_INTERFACE: usize = 64;
const MAX_BYTES_PER_INTERFACE: usize = 256 * 1024;
const MAX_DATAGRAMS_PER_SOURCE: usize = 8;
const REASSEMBLY_TIMEOUT_NS: u64 = 30_000_000_000;
const MAX_PACKET_BYTES: usize = 65_535 + 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReassemblyKey {
    interface: InterfaceId,
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    identification: u32,
}

struct FragmentBytes {
    offset: usize,
    bytes: Vec<u8>,
}

struct Datagram {
    key: ReassemblyKey,
    created_ns: u64,
    updated_ns: u64,
    header: Option<Vec<u8>>,
    metadata: Option<PacketMetadata>,
    fragments: Vec<FragmentBytes>,
    final_len: Option<usize>,
    payload_bytes: usize,
    ipv6: bool,
}

pub enum ReassemblyResult {
    Pending,
    Complete(PacketChain, PacketMetadata),
    Drop(DropReason),
}

pub struct ReassemblyTable {
    datagrams: Vec<Datagram>,
    payload_bytes: usize,
}

impl ReassemblyTable {
    pub const fn new() -> Self {
        Self {
            datagrams: Vec::new(),
            payload_bytes: 0,
        }
    }

    pub fn ingest(
        &mut self,
        interface: InterfaceId,
        now_ns: u64,
        packet: FrontendPacket,
    ) -> ReassemblyResult {
        self.expire(now_ns);
        let Some(ip) = packet.parsed.ip else {
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        };
        let Some(fragment) = ip.fragment else {
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        };
        let payload_len = ip.payload_len as usize;
        let offset = usize::from(fragment.offset).saturating_mul(8);
        if payload_len == 0
            || (fragment.more && payload_len % 8 != 0)
            || offset
                .checked_add(payload_len)
                .is_none_or(|end| end > MAX_PACKET_BYTES)
        {
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        }
        let key = ReassemblyKey {
            interface,
            source: ip.source,
            destination: ip.destination,
            protocol: ip.next_header,
            identification: fragment.identification,
        };
        let index = match self.datagrams.iter().position(|entry| entry.key == key) {
            Some(index) => index,
            None => match self.insert_datagram(key, now_ns, matches!(ip.source, IpAddr::V6(_))) {
                Some(index) => index,
                None => return ReassemblyResult::Drop(DropReason::FragmentLimit),
            },
        };
        let end = offset + payload_len;
        if self.datagrams[index]
            .fragments
            .iter()
            .any(|part| offset < part.offset + part.bytes.len() && part.offset < end)
        {
            self.remove(index);
            return ReassemblyResult::Drop(DropReason::FragmentOverlap);
        }
        if !self.ensure_payload_capacity(key, payload_len) {
            return ReassemblyResult::Drop(DropReason::FragmentLimit);
        }
        let Some(index) = self.datagrams.iter().position(|entry| entry.key == key) else {
            return ReassemblyResult::Drop(DropReason::FragmentLimit);
        };
        let mut bytes = alloc::vec![0; payload_len];
        if packet
            .chain
            .copy_out(usize::from(ip.payload_offset), &mut bytes)
            .is_err()
        {
            self.remove(index);
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        }
        if offset == 0 {
            let mut header = alloc::vec![0; usize::from(ip.payload_offset)];
            if packet.chain.copy_out(0, &mut header).is_err() {
                self.remove(index);
                return ReassemblyResult::Drop(DropReason::FragmentMalformed);
            }
            self.datagrams[index].header = Some(header);
            self.datagrams[index].metadata = Some(packet.metadata);
        }
        if !fragment.more {
            if self.datagrams[index]
                .final_len
                .is_some_and(|current| current != end)
            {
                self.remove(index);
                return ReassemblyResult::Drop(DropReason::FragmentMalformed);
            }
            self.datagrams[index].final_len = Some(end);
        }
        let entry = &mut self.datagrams[index];
        entry.fragments.push(FragmentBytes { offset, bytes });
        entry.fragments.sort_unstable_by_key(|part| part.offset);
        entry.payload_bytes += payload_len;
        entry.updated_ns = now_ns;
        self.payload_bytes += payload_len;

        if !is_complete(entry) {
            return ReassemblyResult::Pending;
        }
        let datagram = self.take(index);
        build_packet(datagram)
    }

    pub fn expire(&mut self, now_ns: u64) -> usize {
        let before = self.datagrams.len();
        let mut index = 0;
        while index < self.datagrams.len() {
            if now_ns.saturating_sub(self.datagrams[index].updated_ns) >= REASSEMBLY_TIMEOUT_NS {
                self.remove(index);
            } else {
                index += 1;
            }
        }
        before - self.datagrams.len()
    }

    pub fn invalidate_interface(&mut self, interface: InterfaceId) -> usize {
        let before = self.datagrams.len();
        let mut index = 0;
        while index < self.datagrams.len() {
            if self.datagrams[index].key.interface == interface {
                self.remove(index);
            } else {
                index += 1;
            }
        }
        before - self.datagrams.len()
    }

    fn insert_datagram(&mut self, key: ReassemblyKey, now_ns: u64, ipv6: bool) -> Option<usize> {
        while self
            .datagrams
            .iter()
            .filter(|entry| entry.key.interface == key.interface)
            .count()
            >= MAX_DATAGRAMS_PER_INTERFACE
            || self
                .datagrams
                .iter()
                .filter(|entry| {
                    entry.key.interface == key.interface && entry.key.source == key.source
                })
                .count()
                >= MAX_DATAGRAMS_PER_SOURCE
        {
            let source_full = self
                .datagrams
                .iter()
                .filter(|entry| {
                    entry.key.interface == key.interface && entry.key.source == key.source
                })
                .count()
                >= MAX_DATAGRAMS_PER_SOURCE;
            if !self.evict_oldest_matching(|entry| {
                entry.key.interface == key.interface
                    && (!source_full || entry.key.source == key.source)
            }) {
                return None;
            }
        }
        self.datagrams.push(Datagram {
            key,
            created_ns: now_ns,
            updated_ns: now_ns,
            header: None,
            metadata: None,
            fragments: Vec::new(),
            final_len: None,
            payload_bytes: 0,
            ipv6,
        });
        Some(self.datagrams.len() - 1)
    }

    fn ensure_payload_capacity(&mut self, key: ReassemblyKey, additional: usize) -> bool {
        loop {
            let used = self
                .datagrams
                .iter()
                .filter(|entry| entry.key.interface == key.interface)
                .map(|entry| entry.payload_bytes)
                .sum::<usize>();
            if used.saturating_add(additional) <= MAX_BYTES_PER_INTERFACE {
                return true;
            }
            if !self.evict_oldest_matching(|entry| {
                entry.key.interface == key.interface && entry.key != key
            }) {
                self.remove_key(key);
                return false;
            }
        }
    }

    fn evict_oldest_matching(&mut self, mut predicate: impl FnMut(&Datagram) -> bool) -> bool {
        if let Some((index, _)) = self
            .datagrams
            .iter()
            .enumerate()
            .filter(|(_, entry)| predicate(entry))
            .min_by_key(|(_, entry)| (entry.updated_ns, entry.created_ns))
        {
            self.remove(index);
            true
        } else {
            false
        }
    }

    fn remove_key(&mut self, key: ReassemblyKey) {
        if let Some(index) = self.datagrams.iter().position(|entry| entry.key == key) {
            self.remove(index);
        }
    }

    fn remove(&mut self, index: usize) {
        let entry = self.datagrams.swap_remove(index);
        self.payload_bytes = self.payload_bytes.saturating_sub(entry.payload_bytes);
    }

    fn take(&mut self, index: usize) -> Datagram {
        let entry = self.datagrams.swap_remove(index);
        self.payload_bytes = self.payload_bytes.saturating_sub(entry.payload_bytes);
        entry
    }
}

impl Default for ReassemblyTable {
    fn default() -> Self {
        Self::new()
    }
}

fn is_complete(entry: &Datagram) -> bool {
    let (Some(final_len), Some(_)) = (entry.final_len, entry.header.as_ref()) else {
        return false;
    };
    let mut expected = 0;
    for fragment in &entry.fragments {
        if fragment.offset != expected {
            return false;
        }
        expected += fragment.bytes.len();
    }
    expected == final_len
}

fn build_packet(mut datagram: Datagram) -> ReassemblyResult {
    let Some(mut bytes) = datagram.header.take() else {
        return ReassemblyResult::Drop(DropReason::FragmentMalformed);
    };
    let Some(final_len) = datagram.final_len else {
        return ReassemblyResult::Drop(DropReason::FragmentMalformed);
    };
    bytes.reserve(final_len);
    for fragment in datagram.fragments {
        bytes.extend_from_slice(&fragment.bytes);
    }
    if datagram.ipv6 {
        if bytes.len() < 62 || bytes.len() - 54 > u16::MAX as usize {
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        }
        let payload_len = (bytes.len() - 54) as u16;
        bytes[18..20].copy_from_slice(&payload_len.to_be_bytes());
        let field = bytes.len().checked_sub(final_len + 6);
        let Some(field) = field.filter(|field| field + 2 <= bytes.len()) else {
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        };
        bytes[field..field + 2].fill(0);
    } else {
        let ip_len = bytes.len().saturating_sub(14);
        if ip_len > u16::MAX as usize || bytes.len() < 34 {
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        }
        bytes[16..18].copy_from_slice(&(ip_len as u16).to_be_bytes());
        bytes[20..22].fill(0);
        bytes[24..26].fill(0);
        let header_len = usize::from(bytes[14] & 0x0f) * 4;
        if !(20..=60).contains(&header_len) || 14 + header_len > bytes.len() {
            return ReassemblyResult::Drop(DropReason::FragmentMalformed);
        }
        let checksum = crate::pipeline::checksum_bytes(&bytes[14..14 + header_len]);
        bytes[24..26].copy_from_slice(&checksum.to_be_bytes());
    }
    let mut metadata = datagram.metadata.unwrap_or_default();
    metadata.frame_len = bytes.len() as u32;
    metadata.rss_hash = None;
    ReassemblyResult::Complete(PacketChain::from_owned(bytes), metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{
        ControlPacket, EthernetHeader, FrontendDisposition, IpFragment, IpPacket, ParsedPacket,
    };
    use crate::{Ipv4Addr, NetDeviceId, QueuePairId};

    fn fragment(offset: u16, more: bool, payload: &[u8]) -> FrontendPacket {
        let header_len = 34usize;
        let mut bytes = alloc::vec![0; header_len + payload.len()];
        bytes[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        bytes[14] = 0x45;
        bytes[16..18].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
        bytes[18..20].copy_from_slice(&7u16.to_be_bytes());
        let field = offset | if more { 0x2000 } else { 0 };
        bytes[20..22].copy_from_slice(&field.to_be_bytes());
        bytes[22] = 64;
        bytes[23] = 17;
        bytes[26..30].copy_from_slice(&[10, 0, 0, 1]);
        bytes[30..34].copy_from_slice(&[10, 0, 0, 2]);
        let checksum = crate::pipeline::checksum_bytes(&bytes[14..34]);
        bytes[24..26].copy_from_slice(&checksum.to_be_bytes());
        bytes[34..].copy_from_slice(payload);
        let ip = IpPacket {
            source: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            destination: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            next_header: 17,
            header_len: 20,
            payload_offset: 34,
            payload_len: payload.len() as u32,
            hop_limit: 64,
            traffic_class: 0,
            fragment: Some(IpFragment {
                identification: 7,
                offset,
                more,
            }),
        };
        FrontendPacket {
            chain: PacketChain::from_owned(bytes),
            metadata: PacketMetadata {
                ingress_device: NetDeviceId(3),
                queue_pair: QueuePairId(1),
                rx_timestamp_ns: 99,
                frame_len: (header_len + payload.len()) as u32,
                ..PacketMetadata::default()
            },
            parsed: ParsedPacket {
                ethernet: EthernetHeader {
                    destination: [2; 6],
                    source: [1; 6],
                    ethertype: 0x0800,
                },
                ip: Some(ip),
                tcp: None,
                udp: None,
                flow: None,
                rss_hash: None,
                disposition: FrontendDisposition::Control(ControlPacket::Fragment(ip)),
            },
        }
    }

    #[test]
    fn out_of_order_ipv4_fragments_reassemble_once() {
        let mut table = ReassemblyTable::new();
        let interface = InterfaceId(1);
        assert!(matches!(
            table.ingest(interface, 1, fragment(1, false, b"ijkl")),
            ReassemblyResult::Pending
        ));
        let ReassemblyResult::Complete(packet, metadata) =
            table.ingest(interface, 2, fragment(0, true, b"abcdefgh"))
        else {
            panic!("应完成重组");
        };
        let mut payload = [0u8; 12];
        packet.copy_out(34, &mut payload).unwrap();
        assert_eq!(&payload, b"abcdefghijkl");
        assert_eq!(metadata.ingress_device, NetDeviceId(3));
        let mut fragment_field = [0u8; 2];
        packet.copy_out(20, &mut fragment_field).unwrap();
        assert_eq!(fragment_field, [0, 0]);
    }

    #[test]
    fn overlap_invalidates_the_whole_datagram() {
        let mut table = ReassemblyTable::new();
        let interface = InterfaceId(1);
        assert!(matches!(
            table.ingest(interface, 1, fragment(0, true, b"abcdefgh")),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            table.ingest(interface, 2, fragment(0, false, b"abcd")),
            ReassemblyResult::Drop(DropReason::FragmentOverlap)
        ));
        assert!(table.datagrams.is_empty());
    }

    #[test]
    fn incomplete_datagram_expires_after_thirty_seconds() {
        let mut table = ReassemblyTable::new();
        assert!(matches!(
            table.ingest(InterfaceId(1), 1, fragment(0, true, b"abcdefgh")),
            ReassemblyResult::Pending
        ));
        assert_eq!(table.expire(30_000_000_001), 1);
        assert_eq!(table.payload_bytes, 0);
    }
}
