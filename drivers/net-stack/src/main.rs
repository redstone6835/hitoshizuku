#![no_std]
#![no_main]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, Ordering};

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
#[cfg(not(feature = "elm-integrated"))]
use net::stack::PinnedNetStackEndpoint;
use net::stack::{
    NET_STACK_ADDRESS_FAMILY_IPV4, NET_STACK_ADDRESS_FAMILY_IPV6, NET_STACK_CALL_STATUS_BUSY,
    NET_STACK_CALL_STATUS_INVALID, NET_STACK_CALL_STATUS_OK, NET_STACK_DROP_IPV4_CHECKSUM,
    NET_STACK_DROP_IPV6_EXTENSION_LIMIT, NET_STACK_DROP_MALFORMED_ARP,
    NET_STACK_DROP_MALFORMED_IPV4, NET_STACK_DROP_MALFORMED_IPV6, NET_STACK_DROP_MALFORMED_TCP,
    NET_STACK_DROP_MALFORMED_UDP, NET_STACK_DROP_NOT_LOCAL, NET_STACK_DROP_TCP_CHECKSUM,
    NET_STACK_DROP_UDP_CHECKSUM,
    NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL, NET_STACK_ETHERNET_ACCEPTED,
    NET_STACK_ETHERNET_TRUNCATED, NET_STACK_ETHERNET_UNSUPPORTED,
    NET_STACK_ETHERNET_VLAN_UNSUPPORTED, NET_STACK_NETWORK_ARP, NET_STACK_NETWORK_DROP,
    NET_STACK_NETWORK_FLAG_FRAGMENT, NET_STACK_NETWORK_FLAG_IPV6_PROBLEM,
    NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS, NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST,
    NET_STACK_NETWORK_IP, NET_STACK_OP_FLOW_CALL, NET_STACK_OP_PROBE, NET_STACK_OP_QUIESCE,
    NET_STACK_OP_TX_FRAGMENT_HEADER, NET_STACK_OP_TX_HEADER, NET_STACK_OP_WORKER_TURN,
    NET_STACK_SOCKET_OP_PROBE, NET_STACK_TCP_OPTION_MSS, NET_STACK_TCP_OPTION_SACK_PERMITTED,
    NET_STACK_TCP_OPTION_TIMESTAMP, NET_STACK_TCP_OPTION_WINDOW_SCALE, NET_STACK_TRANSPORT_DROP,
    NET_STACK_TRANSPORT_ICMP, NET_STACK_TRANSPORT_RAW, NET_STACK_TRANSPORT_SKIPPED,
    NET_STACK_TRANSPORT_TCP, NET_STACK_TRANSPORT_UDP, NET_STACK_TX_HEADER_ABI_VERSION,
    NET_STACK_TX_HEADER_CAPACITY, NET_STACK_TX_RAW_FRAGMENT, NET_STACK_TX_TCP,
    NET_STACK_TX_UDP, NET_STACK_TX_UDP_FRAGMENT, NetStackEthernetV1,
    NetStackControlPlane, NetStackFlowCallV1, NetStackHandle, NetStackLocalAddressV1,
    NetStackNetworkV1,
    NetStackRegisterErrorKind, NetStackRegistration, NetStackRemoveError, NetStackTcpOptionsV1,
    NetStackTransportV1, NetStackTxFragmentHeaderV1, NetStackTxFragmentInputV1,
    NetStackTxHeaderV1, NetStackTxInputV1,
};
use net::{FlowShard, ShardId};
use sched::sync::Spinlock;

use allocator as _;

static QUIESCED: AtomicBool = AtomicBool::new(false);
static FLOW_SHARDS: Spinlock<Vec<Option<Arc<Spinlock<ManuallyDrop<FlowShard>>>>>> =
    Spinlock::new(Vec::new());
static CONTROL_PLANE: Spinlock<Option<Arc<Spinlock<ManuallyDrop<NetStackControlPlane>>>>> =
    Spinlock::new(None);
static SOCKET_TABLE: Spinlock<Option<ManuallyDrop<net::stack::NetStackSocketTable>>> =
    Spinlock::new(None);

const fn empty_ethernet() -> NetStackEthernetV1 {
    NetStackEthernetV1 {
        destination: [0; 6],
        source: [0; 6],
        ethertype: 0,
        status: 0,
        reserved: [0; 5],
    }
}

fn ethernet_is_empty(sidecar: &NetStackEthernetV1) -> bool {
    sidecar.destination == [0; 6]
        && sidecar.source == [0; 6]
        && sidecar.ethertype == 0
        && sidecar.status == 0
        && sidecar.reserved == [0; 5]
}

fn worker_turn_header_valid(turn: &net::stack::NetStackWorkerTurnV1, generation: u64) -> bool {
    turn.abi_version == net::stack::NET_STACK_WORKER_TURN_ABI_VERSION
        && turn.struct_size as usize == core::mem::size_of::<net::stack::NetStackWorkerTurnV1>()
        && turn.generation == generation
        && turn.config_generation != 0
        && !turn.input.is_null()
        && !turn.local_addresses.is_null()
        && turn.interface != 0
        && turn.rss_generation != 0
        && usize::from(turn.input_count) <= turn.ethernet.len()
        && turn.reserved0 == [0; 6]
        && turn.reserved1 == [0; 2]
}

const fn empty_network() -> NetStackNetworkV1 {
    NetStackNetworkV1 {
        outcome: 0,
        family: 0,
        next_header: 0,
        flags: 0,
        drop_reason: 0,
        traffic_class: 0,
        hop_limit: 0,
        reserved0: 0,
        header_len: 0,
        payload_offset: 0,
        fragment_offset: 0,
        arp_operation: 0,
        payload_len: 0,
        fragment_identification: 0,
        problem_pointer: 0,
        source: [0; 16],
        destination: [0; 16],
        arp_sender_mac: [0; 6],
        arp_target_mac: [0; 6],
        reserved1: [0; 8],
    }
}

fn network_is_empty(sidecar: &NetStackNetworkV1) -> bool {
    *sidecar == empty_network()
}

const fn network_drop(reason: u8) -> NetStackNetworkV1 {
    NetStackNetworkV1 {
        outcome: NET_STACK_NETWORK_DROP,
        drop_reason: reason,
        ..empty_network()
    }
}

fn address_projection_valid(address: &NetStackLocalAddressV1) -> bool {
    address.interface != 0
        && matches!(
            (address.family, address.prefix_len),
            (NET_STACK_ADDRESS_FAMILY_IPV4, 0..=32) | (NET_STACK_ADDRESS_FAMILY_IPV6, 0..=128)
        )
        && (address.family != NET_STACK_ADDRESS_FAMILY_IPV4 || address.address[4..] == [0; 12])
        && address.reserved0 == [0; 2]
        && address.reserved1 == [0; 8]
}

fn packet_inputs_valid(turn: &net::stack::NetStackWorkerTurnV1) -> bool {
    let count = usize::from(turn.input_count);
    turn.packet_inputs[..count].iter().all(|facts| {
        matches!(facts.present, 0 | 1)
            && matches!(facts.checksums_validated, 0 | 1)
            && matches!(facts.rss_hash_present, 0 | 1)
            && facts.reserved == 0
            && (facts.rss_hash_present == 1 || facts.rss_hash == 0)
            && (facts.present == 1
                || (facts.frame_len == 0
                    && facts.rss_hash == 0
                    && facts.rss_generation == 0
                    && facts.checksums_validated == 0
                    && facts.rss_hash_present == 0))
    }) && turn.packet_inputs[count..]
        .iter()
        .all(|facts| *facts == net::stack::NetStackPacketInputV1::empty())
}

fn tx_input_valid(input: &NetStackTxInputV1) -> bool {
    if !matches!(
        input.family,
        NET_STACK_ADDRESS_FAMILY_IPV4 | NET_STACK_ADDRESS_FAMILY_IPV6
    ) || input.destination_port == 0
        || input.reserved0 != [0; 3]
        || input.reserved1 != [0; 2]
        || (input.family == NET_STACK_ADDRESS_FAMILY_IPV4
            && (input.source[4..] != [0; 12] || input.destination[4..] != [0; 12]))
    {
        return false;
    }
    let transport_len = match input.kind {
        NET_STACK_TX_UDP => {
            let tcp_empty = input.tcp_flags == 0
                && input.tcp_window == 0
                && input.tcp_options_len == 0
                && input.tcp_sequence == 0
                && input.tcp_acknowledgement == 0
                && input.tcp_options == [0; 40];
            tcp_empty
                .then(|| input.payload_len.checked_add(8))
                .flatten()
        }
        NET_STACK_TX_TCP => {
            let options_len = usize::from(input.tcp_options_len);
            if input.source_port == 0
                || input.tcp_flags & !0x01ff != 0
                || options_len > input.tcp_options.len()
                || options_len % 4 != 0
                || input.tcp_options[options_len..] != [0; 40][options_len..]
            {
                None
            } else {
                input
                    .payload_len
                    .checked_add(20 + u32::from(input.tcp_options_len))
            }
        }
        _ => None,
    };
    transport_len.is_some_and(|transport_len| {
        transport_len <= u32::from(u16::MAX)
            && (input.family != NET_STACK_ADDRESS_FAMILY_IPV4
                || transport_len <= u32::from(u16::MAX - 20))
    })
}

fn tx_header_frame_valid(frame: &NetStackTxHeaderV1, generation: u64) -> bool {
    frame.abi_version == NET_STACK_TX_HEADER_ABI_VERSION
        && frame.struct_size as usize == core::mem::size_of::<NetStackTxHeaderV1>()
        && frame.generation == generation
        && !frame.payload.is_null()
        && frame.payload.is_aligned()
        && tx_input_valid(&frame.input)
        && frame.committed == 0
        && frame.reserved0 == 0
        && frame.header_len == 0
        && frame.header == [0; NET_STACK_TX_HEADER_CAPACITY]
        && frame.reserved1 == [0; 2]
}

fn tx_fragment_input_valid(input: &NetStackTxFragmentInputV1) -> bool {
    if input.reserved != [0; 2]
        || input.mtu == 0
        || input.identification == 0
        || !matches!(
            input.family,
            NET_STACK_ADDRESS_FAMILY_IPV4 | NET_STACK_ADDRESS_FAMILY_IPV6
        )
        || (input.family == NET_STACK_ADDRESS_FAMILY_IPV4
            && (input.source[4..] != [0; 12] || input.destination[4..] != [0; 12]))
    {
        return false;
    }
    match input.kind {
        NET_STACK_TX_UDP_FRAGMENT => {
            input.source_port != 0
                && input.destination_port != 0
                && input.raw_header_len == 0
                && input.raw_flags == 0
                && input.fragment_offset <= input.payload_len
                && input.fragment_offset % 8 == 0
                && input.payload_len <= u32::from(u16::MAX - 8)
        }
        NET_STACK_TX_RAW_FRAGMENT => {
            input.family == NET_STACK_ADDRESS_FAMILY_IPV4
                && input.source_port == 0
                && input.destination_port == 0
                && input.fragment_offset % 8 == 0
                && (input.raw_header_len == 0
                    || ((20..=60).contains(&input.raw_header_len)
                        && input.raw_header_len % 4 == 0
                        && u32::from(input.raw_header_len) <= input.payload_len))
        }
        _ => false,
    }
}

fn tx_fragment_frame_valid(frame: &NetStackTxFragmentHeaderV1, generation: u64) -> bool {
    frame.abi_version == NET_STACK_TX_HEADER_ABI_VERSION
        && frame.struct_size as usize == core::mem::size_of::<NetStackTxFragmentHeaderV1>()
        && frame.generation == generation
        && !frame.payload.is_null()
        && frame.payload.is_aligned()
        && tx_fragment_input_valid(&frame.input)
        && frame.committed == 0
        && frame.more_fragments == 0
        && frame.reserved0 == [0; 2]
        && frame.header_len == 0
        && frame.header == [0; NET_STACK_TX_HEADER_CAPACITY]
        && frame.payload_offset == 0
        && frame.payload_len == 0
        && frame.next_fragment_offset == 0
        && frame.reserved1 == [0; 2]
}

fn is_local_ipv4(interface: u32, addresses: &[NetStackLocalAddressV1], address: [u8; 4]) -> bool {
    if address == [255; 4] || (224..=239).contains(&address[0]) {
        return true;
    }
    let address = u32::from_be_bytes(address);
    addresses.iter().any(|entry| {
        if entry.interface != interface || entry.family != NET_STACK_ADDRESS_FAMILY_IPV4 {
            return false;
        }
        let local = u32::from_be_bytes(entry.address[..4].try_into().unwrap());
        if address == local {
            return true;
        }
        if entry.prefix_len == 0 || entry.prefix_len == 32 {
            return false;
        }
        let mask = u32::MAX << (32 - entry.prefix_len);
        address == local | !mask
    })
}

fn is_local_ipv6(interface: u32, addresses: &[NetStackLocalAddressV1], address: [u8; 16]) -> bool {
    address[0] == 0xff
        || addresses.iter().any(|entry| {
            entry.interface == interface
                && entry.family == NET_STACK_ADDRESS_FAMILY_IPV6
                && entry.address == address
        })
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u64;
    let mut words = bytes.chunks_exact(2);
    for word in &mut words {
        sum += u64::from(u16::from_be_bytes([word[0], word[1]]));
    }
    if let Some(&last) = words.remainder().first() {
        sum += u64::from(u16::from_be_bytes([last, 0]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn parse_arp(
    input: &net::buf::PacketBatch,
    index: usize,
    interface: u32,
    addresses: &[NetStackLocalAddressV1],
) -> NetStackNetworkV1 {
    let mut bytes = [0u8; 28];
    if !input.copy_packet_out(index, 14, &mut bytes)
        || u16::from_be_bytes([bytes[0], bytes[1]]) != 1
        || u16::from_be_bytes([bytes[2], bytes[3]]) != 0x0800
        || bytes[4] != 6
        || bytes[5] != 4
    {
        return network_drop(NET_STACK_DROP_MALFORMED_ARP);
    }
    let operation = u16::from_be_bytes([bytes[6], bytes[7]]);
    if !matches!(operation, 1 | 2) {
        return network_drop(NET_STACK_DROP_MALFORMED_ARP);
    }
    let target_ip: [u8; 4] = bytes[24..28].try_into().unwrap();
    if !is_local_ipv4(interface, addresses, target_ip) {
        return network_drop(NET_STACK_DROP_NOT_LOCAL);
    }
    let mut source = [0; 16];
    source[..4].copy_from_slice(&bytes[14..18]);
    let mut destination = [0; 16];
    destination[..4].copy_from_slice(&target_ip);
    NetStackNetworkV1 {
        outcome: NET_STACK_NETWORK_ARP,
        family: NET_STACK_ADDRESS_FAMILY_IPV4,
        arp_operation: operation,
        source,
        destination,
        arp_sender_mac: bytes[8..14].try_into().unwrap(),
        arp_target_mac: bytes[18..24].try_into().unwrap(),
        ..empty_network()
    }
}

fn parse_ipv4(
    input: &net::buf::PacketBatch,
    index: usize,
    interface: u32,
    addresses: &[NetStackLocalAddressV1],
    frame_len: u32,
    checksums_validated: bool,
) -> NetStackNetworkV1 {
    let mut base = [0u8; 20];
    if !input.copy_packet_out(index, 14, &mut base) || base[0] >> 4 != 4 {
        return network_drop(NET_STACK_DROP_MALFORMED_IPV4);
    }
    let header_len = usize::from(base[0] & 0x0f) * 4;
    let total_len = usize::from(u16::from_be_bytes([base[2], base[3]]));
    if !(20..=60).contains(&header_len)
        || total_len < header_len
        || 14usize.saturating_add(total_len) > frame_len as usize
    {
        return network_drop(NET_STACK_DROP_MALFORMED_IPV4);
    }
    if !checksums_validated {
        let mut header = [0u8; 60];
        if !input.copy_packet_out(index, 14, &mut header[..header_len]) {
            return network_drop(NET_STACK_DROP_MALFORMED_IPV4);
        }
        if checksum(&header[..header_len]) != 0 {
            return network_drop(NET_STACK_DROP_IPV4_CHECKSUM);
        }
    }
    let destination_v4: [u8; 4] = base[16..20].try_into().unwrap();
    if !is_local_ipv4(interface, addresses, destination_v4) {
        return network_drop(NET_STACK_DROP_NOT_LOCAL);
    }
    let mut source = [0; 16];
    source[..4].copy_from_slice(&base[12..16]);
    let mut destination = [0; 16];
    destination[..4].copy_from_slice(&destination_v4);
    let fragment_field = u16::from_be_bytes([base[6], base[7]]);
    let fragment_offset = fragment_field & 0x1fff;
    let more = fragment_field & 0x2000 != 0;
    let fragmented = fragment_offset != 0 || more;
    NetStackNetworkV1 {
        outcome: NET_STACK_NETWORK_IP,
        family: NET_STACK_ADDRESS_FAMILY_IPV4,
        next_header: base[9],
        flags: if fragmented {
            NET_STACK_NETWORK_FLAG_FRAGMENT
                | if more {
                    NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS
                } else {
                    0
                }
        } else {
            0
        },
        traffic_class: base[1],
        hop_limit: base[8],
        header_len: header_len as u16,
        payload_offset: (14 + header_len) as u16,
        fragment_offset,
        payload_len: (total_len - header_len) as u32,
        fragment_identification: if fragmented {
            u32::from(u16::from_be_bytes([base[4], base[5]]))
        } else {
            0
        },
        source,
        destination,
        ..empty_network()
    }
}

enum Ipv6OptionResult {
    Valid,
    Drop(u8),
    Problem {
        pointer: u32,
        suppress_multicast: bool,
    },
}

fn validate_ipv6_options(
    input: &net::buf::PacketBatch,
    index: usize,
    mut offset: usize,
    mut remaining: usize,
) -> Ipv6OptionResult {
    while remaining != 0 {
        let mut kind = [0u8; 1];
        if !input.copy_packet_out(index, offset, &mut kind) {
            return Ipv6OptionResult::Drop(NET_STACK_DROP_MALFORMED_IPV6);
        }
        if kind[0] == 0 {
            offset += 1;
            remaining -= 1;
            continue;
        }
        if remaining < 2 {
            return Ipv6OptionResult::Drop(NET_STACK_DROP_MALFORMED_IPV6);
        }
        let mut header = [0u8; 2];
        if !input.copy_packet_out(index, offset, &mut header) {
            return Ipv6OptionResult::Drop(NET_STACK_DROP_MALFORMED_IPV6);
        }
        let option_len = usize::from(header[1]) + 2;
        if option_len > remaining {
            return Ipv6OptionResult::Drop(NET_STACK_DROP_MALFORMED_IPV6);
        }
        if header[0] != 1 {
            match header[0] >> 6 {
                0 => {}
                1 => return Ipv6OptionResult::Drop(NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL),
                2 => {
                    return Ipv6OptionResult::Problem {
                        pointer: offset.saturating_sub(14) as u32,
                        suppress_multicast: false,
                    };
                }
                _ => {
                    return Ipv6OptionResult::Problem {
                        pointer: offset.saturating_sub(14) as u32,
                        suppress_multicast: true,
                    };
                }
            }
        }
        offset += option_len;
        remaining -= option_len;
    }
    Ipv6OptionResult::Valid
}

fn parse_ipv6(
    input: &net::buf::PacketBatch,
    index: usize,
    interface: u32,
    addresses: &[NetStackLocalAddressV1],
    frame_len: u32,
) -> NetStackNetworkV1 {
    let mut base = [0u8; 40];
    if !input.copy_packet_out(index, 14, &mut base) || base[0] >> 4 != 6 {
        return network_drop(NET_STACK_DROP_MALFORMED_IPV6);
    }
    let payload_len = usize::from(u16::from_be_bytes([base[4], base[5]]));
    if 54usize.saturating_add(payload_len) > frame_len as usize {
        return network_drop(NET_STACK_DROP_MALFORMED_IPV6);
    }
    let destination: [u8; 16] = base[24..40].try_into().unwrap();
    let mut next_header = base[6];
    let mut offset = 54usize;
    let end = 54 + payload_len;
    let mut extension_count = 0usize;
    let mut extension_bytes = 0usize;
    let mut fragment_identification = 0u32;
    let mut fragment_offset = 0u16;
    let mut more = false;
    let mut fragmented = false;
    let mut problem_pointer = 0u32;
    let mut problem_flags = 0u8;
    loop {
        match next_header {
            0 | 60 => {
                extension_count += 1;
                let mut extension = [0u8; 2];
                if !input.copy_packet_out(index, offset, &mut extension) {
                    return network_drop(NET_STACK_DROP_MALFORMED_IPV6);
                }
                let len = (usize::from(extension[1]) + 1) * 8;
                if offset.saturating_add(len) > end {
                    return network_drop(NET_STACK_DROP_MALFORMED_IPV6);
                }
                match validate_ipv6_options(input, index, offset + 2, len - 2) {
                    Ipv6OptionResult::Valid => {}
                    Ipv6OptionResult::Drop(reason) => return network_drop(reason),
                    Ipv6OptionResult::Problem {
                        pointer,
                        suppress_multicast,
                    } => {
                        problem_pointer = pointer;
                        problem_flags = NET_STACK_NETWORK_FLAG_IPV6_PROBLEM
                            | if suppress_multicast {
                                NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST
                            } else {
                                0
                            };
                        next_header = extension[0];
                        offset += len;
                        break;
                    }
                }
                next_header = extension[0];
                offset += len;
                extension_bytes += len;
            }
            44 => {
                if extension_count >= 8 || extension_bytes + 8 > 256 {
                    return network_drop(NET_STACK_DROP_IPV6_EXTENSION_LIMIT);
                }
                let mut header = [0u8; 8];
                if !input.copy_packet_out(index, offset, &mut header) {
                    return network_drop(NET_STACK_DROP_MALFORMED_IPV6);
                }
                let field = u16::from_be_bytes([header[2], header[3]]);
                fragment_offset = field >> 3;
                more = field & 1 != 0;
                fragmented = fragment_offset != 0 || more;
                fragment_identification = if fragmented {
                    u32::from_be_bytes(header[4..8].try_into().unwrap())
                } else {
                    0
                };
                next_header = header[0];
                offset += 8;
                break;
            }
            43 | 50 | 51 => break,
            _ => break,
        }
        if extension_count > 8 || extension_bytes > 256 {
            return network_drop(NET_STACK_DROP_IPV6_EXTENSION_LIMIT);
        }
    }
    if offset > end {
        return network_drop(NET_STACK_DROP_MALFORMED_IPV6);
    }
    if !is_local_ipv6(interface, addresses, destination) {
        return network_drop(NET_STACK_DROP_NOT_LOCAL);
    }
    let mut flags = problem_flags;
    if fragmented {
        flags |= NET_STACK_NETWORK_FLAG_FRAGMENT;
        if more {
            flags |= NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS;
        }
    }
    NetStackNetworkV1 {
        outcome: NET_STACK_NETWORK_IP,
        family: NET_STACK_ADDRESS_FAMILY_IPV6,
        next_header,
        flags,
        traffic_class: ((u16::from(base[0] & 0x0f) << 4) | u16::from(base[1] >> 4)) as u8,
        hop_limit: base[7],
        header_len: (offset - 14) as u16,
        payload_offset: offset as u16,
        fragment_offset,
        payload_len: (end - offset) as u32,
        fragment_identification,
        problem_pointer,
        source: base[8..24].try_into().unwrap(),
        destination,
        ..empty_network()
    }
}

const fn empty_transport() -> NetStackTransportV1 {
    NetStackTransportV1 {
        outcome: 0,
        protocol: 0,
        drop_reason: 0,
        reserved0: 0,
        source_port: 0,
        destination_port: 0,
        header_len: 0,
        payload_offset: 0,
        tcp_flags: 0,
        tcp_window: 0,
        tcp_urgent_pointer: 0,
        reserved1: 0,
        payload_len: 0,
        rss_hash: 0,
        tcp_sequence: 0,
        tcp_acknowledgement: 0,
        tcp_options: NetStackTcpOptionsV1 {
            flags: 0,
            window_scale: 0,
            sack_count: 0,
            reserved0: 0,
            maximum_segment_size: 0,
            reserved1: 0,
            sack_left: [0; 4],
            sack_right: [0; 4],
            timestamp_value: 0,
            timestamp_echo_reply: 0,
        },
        reserved2: [0; 2],
    }
}

fn transport_is_empty(sidecar: &NetStackTransportV1) -> bool {
    *sidecar == empty_transport()
}

const fn transport_skipped() -> NetStackTransportV1 {
    NetStackTransportV1 {
        outcome: NET_STACK_TRANSPORT_SKIPPED,
        ..empty_transport()
    }
}

const fn transport_drop(reason: u8) -> NetStackTransportV1 {
    NetStackTransportV1 {
        outcome: NET_STACK_TRANSPORT_DROP,
        drop_reason: reason,
        ..empty_transport()
    }
}

#[derive(Clone, Copy)]
struct InternetChecksum {
    sum: u64,
    pending: Option<u8>,
}

impl InternetChecksum {
    const fn new() -> Self {
        Self {
            sum: 0,
            pending: None,
        }
    }

    fn add(&mut self, mut bytes: &[u8]) {
        if let Some(high) = self.pending.take() {
            if let Some((&low, rest)) = bytes.split_first() {
                self.sum += u64::from(u16::from_be_bytes([high, low]));
                bytes = rest;
            } else {
                self.pending = Some(high);
                return;
            }
        }
        let mut words = bytes.chunks_exact(2);
        for word in &mut words {
            self.sum += u64::from(u16::from_be_bytes([word[0], word[1]]));
        }
        self.pending = words.remainder().first().copied();
    }

    fn finish(mut self) -> u16 {
        if let Some(high) = self.pending {
            self.sum += u64::from(u16::from_be_bytes([high, 0]));
        }
        while self.sum >> 16 != 0 {
            self.sum = (self.sum & 0xffff) + (self.sum >> 16);
        }
        !(self.sum as u16)
    }
}

fn checksum_packet_range(
    input: &net::buf::PacketBatch,
    index: usize,
    offset: usize,
    len: usize,
    checksum: &mut InternetChecksum,
) -> bool {
    let mut copied = 0usize;
    let mut buffer = [0u8; 128];
    while copied < len {
        let chunk = (len - copied).min(buffer.len());
        if !input.copy_packet_out(index, offset + copied, &mut buffer[..chunk]) {
            return false;
        }
        checksum.add(&buffer[..chunk]);
        copied += chunk;
    }
    true
}

fn checksum_chain_range(
    payload: &net::buf::PacketChain,
    offset: usize,
    len: usize,
    checksum: &mut InternetChecksum,
) -> bool {
    let mut copied = 0usize;
    let mut buffer = [0u8; 128];
    while copied < len {
        let chunk = (len - copied).min(buffer.len());
        if payload
            .copy_out(offset + copied, &mut buffer[..chunk])
            .is_err()
        {
            return false;
        }
        checksum.add(&buffer[..chunk]);
        copied += chunk;
    }
    true
}

fn tx_transport_checksum(
    payload: &net::buf::PacketChain,
    input: &NetStackTxInputV1,
    protocol: u8,
    transport_header: &[u8],
) -> Option<u16> {
    let transport_len = transport_header
        .len()
        .checked_add(input.payload_len as usize)?;
    let mut checksum = InternetChecksum::new();
    match input.family {
        NET_STACK_ADDRESS_FAMILY_IPV4 => {
            let len = u16::try_from(transport_len).ok()?;
            checksum.add(&input.source[..4]);
            checksum.add(&input.destination[..4]);
            checksum.add(&[0, protocol]);
            checksum.add(&len.to_be_bytes());
        }
        NET_STACK_ADDRESS_FAMILY_IPV6 => {
            let len = u32::try_from(transport_len).ok()?;
            checksum.add(&input.source);
            checksum.add(&input.destination);
            checksum.add(&len.to_be_bytes());
            checksum.add(&[0, 0, 0, protocol]);
        }
        _ => return None,
    }
    checksum.add(transport_header);
    checksum_chain_range(
        payload,
        input.payload_offset as usize,
        input.payload_len as usize,
        &mut checksum,
    )
    .then(|| checksum.finish())
}

fn build_tx_header(
    payload: &net::buf::PacketChain,
    input: &NetStackTxInputV1,
) -> Option<(u16, [u8; NET_STACK_TX_HEADER_CAPACITY])> {
    if !tx_input_valid(input)
        || input
            .payload_offset
            .checked_add(input.payload_len)
            .is_none_or(|end| end > payload.total_len() as u32)
    {
        return None;
    }
    let ip_header_len = if input.family == NET_STACK_ADDRESS_FAMILY_IPV4 {
        20usize
    } else {
        40usize
    };
    let transport_header_len = if input.kind == NET_STACK_TX_UDP {
        8usize
    } else {
        20 + usize::from(input.tcp_options_len)
    };
    let header_len = 14 + ip_header_len + transport_header_len;
    let mut header = [0u8; NET_STACK_TX_HEADER_CAPACITY];
    header[..6].copy_from_slice(&input.destination_mac);
    header[6..12].copy_from_slice(&input.source_mac);
    let protocol = if input.kind == NET_STACK_TX_UDP {
        17
    } else {
        6
    };
    let transport_offset = 14 + ip_header_len;

    match input.family {
        NET_STACK_ADDRESS_FAMILY_IPV4 => {
            header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let total_len = ip_header_len
                .checked_add(transport_header_len)?
                .checked_add(input.payload_len as usize)?;
            let total_len = u16::try_from(total_len).ok()?;
            header[14] = 0x45;
            header[15] = input.traffic_class;
            header[16..18].copy_from_slice(&total_len.to_be_bytes());
            header[20..22].copy_from_slice(&0x4000u16.to_be_bytes());
            header[22] = input.hop_limit;
            header[23] = protocol;
            header[26..30].copy_from_slice(&input.source[..4]);
            header[30..34].copy_from_slice(&input.destination[..4]);
            let ip_checksum = checksum(&header[14..34]);
            header[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
        }
        NET_STACK_ADDRESS_FAMILY_IPV6 => {
            header[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            header[14..18].copy_from_slice(
                &(0x6000_0000u32 | (u32::from(input.traffic_class) << 20)).to_be_bytes(),
            );
            let transport_len = transport_header_len.checked_add(input.payload_len as usize)?;
            header[18..20].copy_from_slice(&u16::try_from(transport_len).ok()?.to_be_bytes());
            header[20] = protocol;
            header[21] = input.hop_limit;
            header[22..38].copy_from_slice(&input.source);
            header[38..54].copy_from_slice(&input.destination);
        }
        _ => return None,
    }

    if input.kind == NET_STACK_TX_UDP {
        let udp_len = u16::try_from(8usize.checked_add(input.payload_len as usize)?).ok()?;
        let udp = &mut header[transport_offset..transport_offset + 8];
        udp[..2].copy_from_slice(&input.source_port.to_be_bytes());
        udp[2..4].copy_from_slice(&input.destination_port.to_be_bytes());
        udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
        let mut checksum_header = [0u8; 8];
        checksum_header.copy_from_slice(udp);
        let checksum = tx_transport_checksum(payload, input, protocol, &checksum_header)?;
        udp[6..8].copy_from_slice(&(if checksum == 0 { 0xffff } else { checksum }).to_be_bytes());
    } else {
        let tcp = &mut header[transport_offset..transport_offset + transport_header_len];
        tcp[..2].copy_from_slice(&input.source_port.to_be_bytes());
        tcp[2..4].copy_from_slice(&input.destination_port.to_be_bytes());
        tcp[4..8].copy_from_slice(&input.tcp_sequence.to_be_bytes());
        tcp[8..12].copy_from_slice(&input.tcp_acknowledgement.to_be_bytes());
        tcp[12] = ((transport_header_len / 4) as u8) << 4 | u8::from(input.tcp_flags & 0x100 != 0);
        tcp[13] = input.tcp_flags as u8;
        tcp[14..16].copy_from_slice(&input.tcp_window.to_be_bytes());
        tcp[20..].copy_from_slice(&input.tcp_options[..usize::from(input.tcp_options_len)]);
        let mut checksum_header = [0u8; 60];
        checksum_header[..transport_header_len].copy_from_slice(tcp);
        let checksum = tx_transport_checksum(
            payload,
            input,
            protocol,
            &checksum_header[..transport_header_len],
        )?;
        tcp[16..18].copy_from_slice(&checksum.to_be_bytes());
    }
    Some((header_len as u16, header))
}

fn fragment_udp_checksum(
    payload: &net::buf::PacketChain,
    input: &NetStackTxFragmentInputV1,
    header: &[u8; 8],
) -> Option<u16> {
    let total_len = 8usize.checked_add(input.payload_len as usize)?;
    let mut checksum = InternetChecksum::new();
    match input.family {
        NET_STACK_ADDRESS_FAMILY_IPV4 => {
            let length = u16::try_from(total_len).ok()?;
            checksum.add(&input.source[..4]);
            checksum.add(&input.destination[..4]);
            checksum.add(&[0, 17]);
            checksum.add(&length.to_be_bytes());
        }
        NET_STACK_ADDRESS_FAMILY_IPV6 => {
            let length = u32::try_from(total_len).ok()?;
            checksum.add(&input.source);
            checksum.add(&input.destination);
            checksum.add(&length.to_be_bytes());
            checksum.add(&[0, 0, 0, 17]);
        }
        _ => return None,
    }
    checksum.add(header);
    checksum_chain_range(payload, 0, input.payload_len as usize, &mut checksum)
        .then(|| checksum.finish())
}

fn build_tx_fragment_header(
    payload: &net::buf::PacketChain,
    input: &NetStackTxFragmentInputV1,
) -> Option<(
    u16,
    [u8; NET_STACK_TX_HEADER_CAPACITY],
    u32,
    u32,
    u32,
    bool,
)> {
    if !tx_fragment_input_valid(input) || input.payload_len as usize > payload.total_len() {
        return None;
    }
    let mut header = [0u8; NET_STACK_TX_HEADER_CAPACITY];
    header[..6].copy_from_slice(&input.destination_mac);
    header[6..12].copy_from_slice(&input.source_mac);
    match input.kind {
        NET_STACK_TX_UDP_FRAGMENT => {
            let datagram_len = 8usize.checked_add(input.payload_len as usize)?;
            let (_ip_header_len, _fragment_header_len, fragment_capacity) = match input.family {
                NET_STACK_ADDRESS_FAMILY_IPV4 => (
                    20usize,
                    0usize,
                    (input.mtu as usize).checked_sub(20)? & !7,
                ),
                NET_STACK_ADDRESS_FAMILY_IPV6 => (
                    40usize,
                    8usize,
                    (input.mtu as usize).checked_sub(48)? & !7,
                ),
                _ => return None,
            };
            if fragment_capacity < 8 {
                return None;
            }
            let datagram_offset = if input.fragment_offset == 0 {
                0usize
            } else {
                8usize.checked_add(input.fragment_offset as usize)?
            };
            if datagram_offset >= datagram_len {
                return None;
            }
            let chunk_len = fragment_capacity.min(datagram_len - datagram_offset);
            let first = input.fragment_offset == 0;
            let payload_offset = input.fragment_offset;
            let payload_len = chunk_len.checked_sub(if first { 8 } else { 0 })? as u32;
            let next_offset = payload_offset.checked_add(payload_len)?;
            let more = next_offset < input.payload_len;
            let fragment_field = (datagram_offset / 8) as u16;
            match input.family {
                NET_STACK_ADDRESS_FAMILY_IPV4 => {
                    header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
                    header[14] = 0x45;
                    header[15] = input.traffic_class;
                    header[16..18]
                        .copy_from_slice(&u16::try_from(20 + chunk_len).ok()?.to_be_bytes());
                    header[18..20].copy_from_slice(&(input.identification as u16).to_be_bytes());
                    header[20..22].copy_from_slice(
                        &(fragment_field | if more { 0x2000 } else { 0 }).to_be_bytes(),
                    );
                    header[22] = input.hop_limit;
                    header[23] = 17;
                    header[26..30].copy_from_slice(&input.source[..4]);
                    header[30..34].copy_from_slice(&input.destination[..4]);
                    let checksum = checksum(&header[14..34]);
                    header[24..26].copy_from_slice(&checksum.to_be_bytes());
                    if first {
                        let udp = &mut header[34..42];
                        udp[..2].copy_from_slice(&input.source_port.to_be_bytes());
                        udp[2..4].copy_from_slice(&input.destination_port.to_be_bytes());
                        udp[4..6].copy_from_slice(&u16::try_from(datagram_len).ok()?.to_be_bytes());
                        let mut checksum_header = [0u8; 8];
                        checksum_header.copy_from_slice(udp);
                        let value = fragment_udp_checksum(payload, input, &checksum_header)?;
                        udp[6..8].copy_from_slice(&(if value == 0 { 0xffff } else { value }).to_be_bytes());
                    }
                    Some((
                        (34 + if first { 8 } else { 0 }) as u16,
                        header,
                        payload_offset,
                        payload_len,
                        next_offset,
                        more,
                    ))
                }
                NET_STACK_ADDRESS_FAMILY_IPV6 => {
                    header[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
                    header[14..18].copy_from_slice(
                        &(0x6000_0000u32 | (u32::from(input.traffic_class) << 20)).to_be_bytes(),
                    );
                    let ipv6_payload_len = 8usize + chunk_len;
                    header[18..20].copy_from_slice(&u16::try_from(ipv6_payload_len).ok()?.to_be_bytes());
                    header[20] = 44;
                    header[21] = input.hop_limit;
                    header[22..38].copy_from_slice(&input.source);
                    header[38..54].copy_from_slice(&input.destination);
                    header[54] = 17;
                    header[56..58].copy_from_slice(
                        &(((fragment_field) << 3) | u16::from(more)).to_be_bytes(),
                    );
                    header[58..62].copy_from_slice(&input.identification.to_be_bytes());
                    if first {
                        let udp = &mut header[62..70];
                        udp[..2].copy_from_slice(&input.source_port.to_be_bytes());
                        udp[2..4].copy_from_slice(&input.destination_port.to_be_bytes());
                        udp[4..6].copy_from_slice(&u16::try_from(datagram_len).ok()?.to_be_bytes());
                        let mut checksum_header = [0u8; 8];
                        checksum_header.copy_from_slice(udp);
                        let value = fragment_udp_checksum(payload, input, &checksum_header)?;
                        udp[6..8].copy_from_slice(&(if value == 0 { 0xffff } else { value }).to_be_bytes());
                    }
                    Some((
                        (62 + if first { 8 } else { 0 }) as u16,
                        header,
                        payload_offset,
                        payload_len,
                        next_offset,
                        more,
                    ))
                }
                _ => None,
            }
        }
        NET_STACK_TX_RAW_FRAGMENT => {
            if payload.total_len() != input.payload_len as usize || payload.total_len() < 20 {
                return None;
            }
            let mut ip = [0u8; 60];
            payload.copy_out(0, &mut ip[..20]).ok()?;
            let header_len = usize::from(ip[0] & 0x0f) * 4;
            if ip[0] >> 4 != 4
                || !(20..=60).contains(&header_len)
                || header_len % 4 != 0
                || input.raw_header_len != 0 && usize::from(input.raw_header_len) != header_len
            {
                return None;
            }
            payload.copy_out(0, &mut ip[..header_len]).ok()?;
            let body_len = payload.total_len().checked_sub(header_len)?;
            let capacity = (input.mtu as usize).checked_sub(header_len)? & !7;
            if capacity < 8 || input.fragment_offset as usize >= body_len {
                return None;
            }
            let chunk_len = capacity.min(body_len - input.fragment_offset as usize);
            let more = input.fragment_offset as usize + chunk_len < body_len;
            let flags = u16::from_be_bytes([ip[6], ip[7]]);
            if flags & 0x4000 != 0 {
                return None;
            }
            ip[2..4].copy_from_slice(&u16::try_from(header_len + chunk_len).ok()?.to_be_bytes());
            ip[6..8].copy_from_slice(
                &((flags & 0x8000)
                    | ((input.fragment_offset / 8) as u16)
                    | if more { 0x2000 } else { 0 })
                    .to_be_bytes(),
            );
            if ip[12..16] == [0; 4] {
                ip[12..16].copy_from_slice(&input.source[..4]);
            }
            ip[16..20].copy_from_slice(&input.destination[..4]);
            ip[10..12].fill(0);
            let ip_checksum = checksum(&ip[..header_len]);
            ip[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
            header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            header[14..14 + header_len].copy_from_slice(&ip[..header_len]);
            let next_offset = input.fragment_offset.saturating_add(chunk_len as u32);
            Some((
                (14 + header_len) as u16,
                header,
                header_len as u32 + input.fragment_offset,
                chunk_len as u32,
                next_offset,
                more,
            ))
        }
        _ => None,
    }
}

fn transport_checksum_valid(
    input: &net::buf::PacketBatch,
    index: usize,
    network: &NetStackNetworkV1,
    protocol: u8,
    len: usize,
) -> bool {
    let mut checksum = InternetChecksum::new();
    match network.family {
        NET_STACK_ADDRESS_FAMILY_IPV4 => {
            let Ok(len) = u16::try_from(len) else {
                return false;
            };
            checksum.add(&network.source[..4]);
            checksum.add(&network.destination[..4]);
            checksum.add(&[0, protocol]);
            checksum.add(&len.to_be_bytes());
        }
        NET_STACK_ADDRESS_FAMILY_IPV6 => {
            let Ok(len) = u32::try_from(len) else {
                return false;
            };
            checksum.add(&network.source);
            checksum.add(&network.destination);
            checksum.add(&len.to_be_bytes());
            checksum.add(&[0, 0, 0, protocol]);
        }
        _ => return false,
    }
    checksum_packet_range(
        input,
        index,
        usize::from(network.payload_offset),
        len,
        &mut checksum,
    ) && checksum.finish() == 0
}

fn icmpv4_checksum_valid(
    input: &net::buf::PacketBatch,
    index: usize,
    network: &NetStackNetworkV1,
) -> bool {
    let mut checksum = InternetChecksum::new();
    checksum_packet_range(
        input,
        index,
        usize::from(network.payload_offset),
        network.payload_len as usize,
        &mut checksum,
    ) && checksum.finish() == 0
}

fn flow_hash(
    rss_key: &[u8; 40],
    rss_generation: u32,
    facts: net::stack::NetStackPacketInputV1,
    network: &NetStackNetworkV1,
    source_port: u16,
    destination_port: u16,
) -> Option<u32> {
    if destination_port == 0 {
        return None;
    }
    Some(
        if facts.rss_hash_present != 0 && facts.rss_generation == rss_generation {
            facts.rss_hash
        } else {
            let mut input = [0u8; 36];
            let len = match network.family {
                NET_STACK_ADDRESS_FAMILY_IPV4 => {
                    input[0..4].copy_from_slice(&network.source[..4]);
                    input[4..8].copy_from_slice(&network.destination[..4]);
                    input[8..10].copy_from_slice(&source_port.to_be_bytes());
                    input[10..12].copy_from_slice(&destination_port.to_be_bytes());
                    12
                }
                NET_STACK_ADDRESS_FAMILY_IPV6 => {
                    input[0..16].copy_from_slice(&network.source);
                    input[16..32].copy_from_slice(&network.destination);
                    input[32..34].copy_from_slice(&source_port.to_be_bytes());
                    input[34..36].copy_from_slice(&destination_port.to_be_bytes());
                    36
                }
                _ => return None,
            };
            toeplitz(rss_key, &input[..len])
        },
    )
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

fn parse_tcp_options(
    input: &net::buf::PacketBatch,
    index: usize,
    offset: usize,
    header_len: usize,
) -> Result<NetStackTcpOptionsV1, u8> {
    let options_len = header_len - 20;
    let mut bytes = [0u8; 40];
    if !input.copy_packet_out(index, offset, &mut bytes[..options_len]) {
        return Err(NET_STACK_DROP_MALFORMED_TCP);
    }
    let mut parsed = NetStackTcpOptionsV1::empty();
    let mut sack_seen = false;
    let mut cursor = 0usize;
    while cursor < options_len {
        let kind = bytes[cursor];
        match kind {
            0 => {
                if bytes[cursor..options_len].iter().any(|byte| *byte != 0) {
                    return Err(NET_STACK_DROP_MALFORMED_TCP);
                }
                break;
            }
            1 => {
                cursor += 1;
                continue;
            }
            _ => {}
        }
        if cursor + 2 > options_len {
            return Err(NET_STACK_DROP_MALFORMED_TCP);
        }
        let len = usize::from(bytes[cursor + 1]);
        if len < 2 || cursor + len > options_len {
            return Err(NET_STACK_DROP_MALFORMED_TCP);
        }
        let option = &bytes[cursor..cursor + len];
        match kind {
            2 => {
                if len != 4 || parsed.flags & NET_STACK_TCP_OPTION_MSS != 0 {
                    return Err(NET_STACK_DROP_MALFORMED_TCP);
                }
                let mss = u16::from_be_bytes([option[2], option[3]]);
                if mss == 0 {
                    return Err(NET_STACK_DROP_MALFORMED_TCP);
                }
                parsed.flags |= NET_STACK_TCP_OPTION_MSS;
                parsed.maximum_segment_size = mss;
            }
            3 => {
                if len != 3 || parsed.flags & NET_STACK_TCP_OPTION_WINDOW_SCALE != 0 {
                    return Err(NET_STACK_DROP_MALFORMED_TCP);
                }
                parsed.flags |= NET_STACK_TCP_OPTION_WINDOW_SCALE;
                parsed.window_scale = option[2].min(14);
            }
            4 => {
                if len != 2 || parsed.flags & NET_STACK_TCP_OPTION_SACK_PERMITTED != 0 {
                    return Err(NET_STACK_DROP_MALFORMED_TCP);
                }
                parsed.flags |= NET_STACK_TCP_OPTION_SACK_PERMITTED;
            }
            5 => {
                if sack_seen || len < 10 || len > 34 || (len - 2) % 8 != 0 {
                    return Err(NET_STACK_DROP_MALFORMED_TCP);
                }
                sack_seen = true;
                for (block_index, block) in option[2..].chunks_exact(8).enumerate() {
                    parsed.sack_left[block_index] =
                        u32::from_be_bytes(block[0..4].try_into().unwrap());
                    parsed.sack_right[block_index] =
                        u32::from_be_bytes(block[4..8].try_into().unwrap());
                    parsed.sack_count += 1;
                }
            }
            8 => {
                if len != 10 || parsed.flags & NET_STACK_TCP_OPTION_TIMESTAMP != 0 {
                    return Err(NET_STACK_DROP_MALFORMED_TCP);
                }
                parsed.flags |= NET_STACK_TCP_OPTION_TIMESTAMP;
                parsed.timestamp_value = u32::from_be_bytes(option[2..6].try_into().unwrap());
                parsed.timestamp_echo_reply = u32::from_be_bytes(option[6..10].try_into().unwrap());
            }
            _ => {}
        }
        cursor += len;
    }
    Ok(parsed)
}

fn parse_tcp(
    input: &net::buf::PacketBatch,
    index: usize,
    network: &NetStackNetworkV1,
    facts: net::stack::NetStackPacketInputV1,
    rss_key: &[u8; 40],
    rss_generation: u32,
) -> NetStackTransportV1 {
    if network.payload_len < 20 {
        return transport_drop(NET_STACK_DROP_MALFORMED_TCP);
    }
    let mut header = [0u8; 20];
    if !input.copy_packet_out(index, usize::from(network.payload_offset), &mut header) {
        return transport_drop(NET_STACK_DROP_MALFORMED_TCP);
    }
    let header_len = usize::from(header[12] >> 4) * 4;
    if !(20..=60).contains(&header_len)
        || header_len > network.payload_len as usize
        || header[12] & 0x0e != 0
    {
        return transport_drop(NET_STACK_DROP_MALFORMED_TCP);
    }
    let options = match parse_tcp_options(
        input,
        index,
        usize::from(network.payload_offset) + 20,
        header_len,
    ) {
        Ok(options) => options,
        Err(reason) => return transport_drop(reason),
    };
    if facts.checksums_validated == 0
        && !transport_checksum_valid(input, index, network, 6, network.payload_len as usize)
    {
        return transport_drop(NET_STACK_DROP_TCP_CHECKSUM);
    }
    let source_port = u16::from_be_bytes([header[0], header[1]]);
    let destination_port = u16::from_be_bytes([header[2], header[3]]);
    let Some(rss_hash) = flow_hash(
        rss_key,
        rss_generation,
        facts,
        network,
        source_port,
        destination_port,
    ) else {
        return transport_drop(NET_STACK_DROP_MALFORMED_TCP);
    };
    NetStackTransportV1 {
        outcome: NET_STACK_TRANSPORT_TCP,
        protocol: 6,
        source_port,
        destination_port,
        header_len: header_len as u16,
        payload_offset: network.payload_offset + header_len as u16,
        tcp_flags: u16::from(header[12] & 1) << 8 | u16::from(header[13]),
        tcp_window: u16::from_be_bytes([header[14], header[15]]),
        tcp_urgent_pointer: u16::from_be_bytes([header[18], header[19]]),
        payload_len: network.payload_len - header_len as u32,
        rss_hash,
        tcp_sequence: u32::from_be_bytes(header[4..8].try_into().unwrap()),
        tcp_acknowledgement: u32::from_be_bytes(header[8..12].try_into().unwrap()),
        tcp_options: options,
        ..empty_transport()
    }
}

fn parse_udp(
    input: &net::buf::PacketBatch,
    index: usize,
    network: &NetStackNetworkV1,
    facts: net::stack::NetStackPacketInputV1,
    rss_key: &[u8; 40],
    rss_generation: u32,
) -> NetStackTransportV1 {
    if network.payload_len < 8 {
        return transport_drop(NET_STACK_DROP_MALFORMED_UDP);
    }
    let mut header = [0u8; 8];
    if !input.copy_packet_out(index, usize::from(network.payload_offset), &mut header) {
        return transport_drop(NET_STACK_DROP_MALFORMED_UDP);
    }
    let udp_len = usize::from(u16::from_be_bytes([header[4], header[5]]));
    if udp_len < 8 || udp_len > network.payload_len as usize {
        return transport_drop(NET_STACK_DROP_MALFORMED_UDP);
    }
    let checksum = u16::from_be_bytes([header[6], header[7]]);
    if checksum == 0 && network.family == NET_STACK_ADDRESS_FAMILY_IPV6 {
        return transport_drop(NET_STACK_DROP_UDP_CHECKSUM);
    }
    if facts.checksums_validated == 0
        && checksum != 0
        && !transport_checksum_valid(input, index, network, 17, udp_len)
    {
        return transport_drop(NET_STACK_DROP_UDP_CHECKSUM);
    }
    let source_port = u16::from_be_bytes([header[0], header[1]]);
    let destination_port = u16::from_be_bytes([header[2], header[3]]);
    let Some(rss_hash) = flow_hash(
        rss_key,
        rss_generation,
        facts,
        network,
        source_port,
        destination_port,
    ) else {
        return transport_drop(NET_STACK_DROP_MALFORMED_UDP);
    };
    NetStackTransportV1 {
        outcome: NET_STACK_TRANSPORT_UDP,
        protocol: 17,
        source_port,
        destination_port,
        header_len: 8,
        payload_offset: network.payload_offset + 8,
        payload_len: (udp_len - 8) as u32,
        rss_hash,
        ..empty_transport()
    }
}

fn parse_icmp(
    input: &net::buf::PacketBatch,
    index: usize,
    network: &NetStackNetworkV1,
    facts: net::stack::NetStackPacketInputV1,
) -> NetStackTransportV1 {
    if network.payload_len < 8 {
        return transport_drop(if network.family == NET_STACK_ADDRESS_FAMILY_IPV6 {
            NET_STACK_DROP_MALFORMED_IPV6
        } else {
            NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL
        });
    }
    let checksum_valid = facts.checksums_validated != 0
        || if network.family == NET_STACK_ADDRESS_FAMILY_IPV6 {
            transport_checksum_valid(
                input,
                index,
                network,
                network.next_header,
                network.payload_len as usize,
            )
        } else {
            icmpv4_checksum_valid(input, index, network)
        };
    if !checksum_valid {
        return transport_drop(NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL);
    }
    NetStackTransportV1 {
        outcome: NET_STACK_TRANSPORT_ICMP,
        protocol: network.next_header,
        payload_offset: network.payload_offset,
        payload_len: network.payload_len,
        ..empty_transport()
    }
}

fn parse_transport(
    input: &net::buf::PacketBatch,
    index: usize,
    network: &NetStackNetworkV1,
    facts: net::stack::NetStackPacketInputV1,
    rss_key: &[u8; 40],
    rss_generation: u32,
) -> NetStackTransportV1 {
    if network.outcome != NET_STACK_NETWORK_IP
        || network.flags & (NET_STACK_NETWORK_FLAG_FRAGMENT | NET_STACK_NETWORK_FLAG_IPV6_PROBLEM)
            != 0
    {
        return transport_skipped();
    }
    match (network.family, network.next_header) {
        (_, 6) => parse_tcp(input, index, network, facts, rss_key, rss_generation),
        (_, 17) => parse_udp(input, index, network, facts, rss_key, rss_generation),
        (NET_STACK_ADDRESS_FAMILY_IPV4, 1) | (NET_STACK_ADDRESS_FAMILY_IPV6, 58) => {
            parse_icmp(input, index, network, facts)
        }
        _ => NetStackTransportV1 {
            outcome: NET_STACK_TRANSPORT_RAW,
            protocol: network.next_header,
            payload_offset: network.payload_offset,
            payload_len: network.payload_len,
            ..empty_transport()
        },
    }
}

fn initialize_flow_shards(boot: net::boot::NetStackBootConfig) -> bool {
    let count = usize::from(boot.active_cpu_count());
    if count == 0 || count > sched::NR_CPUS {
        return false;
    }
    let now_ns = sched::now_ns_public();
    let shards = (0..count)
        .map(|index| {
            Some(Arc::new(Spinlock::new(ManuallyDrop::new(
                net::stack::create_flow_shard(ShardId(index as u16), boot, now_ns),
            ))))
        })
        .collect::<Vec<_>>();
    *CONTROL_PLANE.lock() = Some(Arc::new(Spinlock::new(ManuallyDrop::new(
        net::stack::create_control_plane(count, *boot.rss_key(), boot.hash_seed()),
    ))));
    *FLOW_SHARDS.lock() = shards;
    true
}

fn flow_shard(id: ShardId) -> Option<Arc<Spinlock<ManuallyDrop<FlowShard>>>> {
    FLOW_SHARDS
        .lock()
        .get(usize::from(id.0))
        .and_then(Option::as_ref)
        .cloned()
}

fn dispatch_flow_call(call: &mut NetStackFlowCallV1) -> bool {
    if let net::stack::NetStackFlowCommand::Control { command } = &mut call.command {
        let Some(control) = CONTROL_PLANE.lock().as_ref().cloned() else {
            return false;
        };
        net::stack::dispatch_control_plane_call(&mut control.lock(), command);
        call.committed = 1;
        return true;
    }
    let Some(shard) = flow_shard(call.shard) else {
        return false;
    };
    net::stack::dispatch_flow_shard_call(&mut shard.lock(), call)
}

fn initialize_socket_table(boot: net::boot::NetStackBootConfig, generation: u64) -> bool {
    let boot_nonce = u64::from_le_bytes(
        boot.generation_nonce()[..8]
            .try_into()
            .expect("generation nonce 长度固定"),
    );
    let Some(table) = net::stack::create_socket_table(boot_nonce, generation) else {
        return false;
    };
    let mut slot = SOCKET_TABLE.lock();
    if slot.is_some() {
        net::stack::destroy_socket_table(table);
        return false;
    }
    *slot = Some(ManuallyDrop::new(table));
    true
}

fn destroy_generation_state() {
    if let Some(mut table) = SOCKET_TABLE.lock().take() {
        // Safety: 套接字表只在这里从 ManuallyDrop 中取出一次，随后交给受
        // capability 约束的宿主析构函数。
        let state = unsafe { ManuallyDrop::take(&mut table) };
        net::stack::destroy_socket_table(state);
    }
    let shards = core::mem::take(&mut *FLOW_SHARDS.lock());
    for shard in shards.into_iter().flatten() {
        let mut guard = shard.lock();
        // Safety: 每个 shard 只在这里从 ManuallyDrop 中取出一次，随后交给受
        // capability 约束的宿主析构函数。
        let state = unsafe { ManuallyDrop::take(&mut guard) };
        drop(guard);
        net::stack::destroy_flow_shard(state);
    }
    if let Some(control) = CONTROL_PLANE.lock().take() {
        let mut guard = control.lock();
        // Safety: 控制面只在这里从 ManuallyDrop 中取出一次，随后交给受
        // capability 约束的宿主析构函数。
        let state = unsafe { ManuallyDrop::take(&mut guard) };
        drop(guard);
        net::stack::destroy_control_plane(state);
    }
}

struct NetStackElm {
    handle: Option<NetStackHandle>,
    boot: Option<net::boot::NetStackBootConfig>,
}

fn map_register_error(error: NetStackRegisterErrorKind) -> HookError {
    let code = match error {
        NetStackRegisterErrorKind::RegistrarNotReady => -19,
        NetStackRegisterErrorKind::AlreadyActive => -16,
        NetStackRegisterErrorKind::InvalidRegistration => -22,
        NetStackRegisterErrorKind::ResourceExhausted => -12,
    };
    HookError::new(code)
}

fn map_remove_error(error: NetStackRemoveError) -> HookError {
    let code = match error {
        NetStackRemoveError::NoStack => -19,
        NetStackRemoveError::OwnerMismatch => -1,
        NetStackRemoveError::Busy => -16,
    };
    HookError::new(code)
}

#[elm::module]
impl ElmModule for NetStackElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self {
            handle: None,
            boot: None,
        })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if self.handle.is_some() {
            return Err(HookError::new(-16));
        }
        let boot = net::stack::boot_config().ok_or(HookError::new(-19))?;
        if boot.active_cpu_count() == 0 || usize::from(boot.active_cpu_count()) > sched::NR_CPUS {
            return Err(HookError::new(-22));
        }
        if !initialize_flow_shards(boot) {
            return Err(HookError::new(-12));
        }
        QUIESCED.store(false, Ordering::Release);
        #[cfg(not(feature = "elm-integrated"))]
        let (registration, generation) = {
            let endpoint =
                PinnedNetStackEndpoint::current("net.stack.call", "mygo.net.stack-call@1", 1)
                    .ok_or(HookError::new(-22))?;
            let generation = endpoint.owner_generation();
            let socket_endpoint = PinnedNetStackEndpoint::current(
                "net.stack.socket",
                "mygo.net.stack-socket-call@1",
                1,
            )
            .ok_or(HookError::new(-22))?;
            (
                NetStackRegistration::pinned(endpoint, socket_endpoint)
                    .ok_or(HookError::new(-22))?,
                generation,
            )
        };
        #[cfg(feature = "elm-integrated")]
        let (registration, generation) = (
            NetStackRegistration::integrated(net_stack_call, net_stack_socket_call)
                .ok_or(HookError::new(-22))?,
            1,
        );
        if !initialize_socket_table(boot, generation) {
            destroy_generation_state();
            return Err(HookError::new(-12));
        }
        let handle = match net::stack::register_stack(registration) {
            Ok(handle) => handle,
            Err(error) => {
                destroy_generation_state();
                return Err(map_register_error(error.kind));
            }
        };
        self.boot = Some(boot);
        self.handle = Some(handle);
        Ok(())
    }

    fn quiesce(&mut self, _context: &LifecycleContext) -> HookResult {
        QUIESCED.store(true, Ordering::Release);
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        let Some(handle) = self.handle else {
            return Ok(());
        };
        match net::stack::begin_remove(handle) {
            Ok(()) | Err(NetStackRemoveError::NoStack) => {
                destroy_generation_state();
                self.handle = None;
                self.boot = None;
                Ok(())
            }
            Err(error) => Err(map_remove_error(error)),
        }
    }
}

#[elm::export(
    name = "net.stack.call",
    contract = "mygo.net.stack-call@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "private"
)]
fn net_stack_call(frame: &mut net::stack::NetStackCallV1) -> i32 {
    if !frame.valid(frame.opcode, frame.generation) || frame.generation == 0 {
        return NET_STACK_CALL_STATUS_INVALID;
    }
    match frame.opcode {
        NET_STACK_OP_PROBE => {
            let quiesced = QUIESCED.load(Ordering::Acquire);
            frame.ready = u8::from(!quiesced);
            frame.quiesced = u8::from(quiesced);
        }
        NET_STACK_OP_WORKER_TURN => {
            if QUIESCED.load(Ordering::Acquire) {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 已把 worker-turn 帧声明为本次 pinned call 的可访问范围；
            // 指针只在同步调用期间借用，ELM 不保存它。
            let turn = unsafe { &mut *frame.worker_turn };
            if !worker_turn_header_valid(turn, frame.generation) || turn.committed != 0 {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 同时声明了只读 PacketBatch 外壳范围，实际 fragment backing
            // 只会由受能力约束的 copy_packet_out 内核符号读取。
            let input = unsafe { &*turn.input };
            let Ok(address_count) = usize::try_from(turn.local_address_count) else {
                return NET_STACK_CALL_STATUS_INVALID;
            };
            let Some(address_bytes) =
                address_count.checked_mul(core::mem::size_of::<NetStackLocalAddressV1>())
            else {
                return NET_STACK_CALL_STATUS_INVALID;
            };
            let address_pointer = turn.local_addresses;
            if address_bytes > isize::MAX as usize || !address_pointer.is_aligned() {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 已把地址投影声明为本次 pinned call 的只读范围；长度和对齐
            // 已在上面校验，slice 只在同步调用期间借用，ELM 不保存它。
            let addresses = unsafe { core::slice::from_raw_parts(address_pointer, address_count) };
            let rss_key = turn.rss_key;
            let rss_generation = turn.rss_generation;
            if input.len() != usize::from(turn.input_count)
                || !packet_inputs_valid(turn)
                || !addresses.iter().all(address_projection_valid)
            {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            for index in 0..usize::from(turn.input_count) {
                if !ethernet_is_empty(&turn.ethernet[index])
                    || !network_is_empty(&turn.network[index])
                    || !transport_is_empty(&turn.transport[index])
                {
                    return NET_STACK_CALL_STATUS_INVALID;
                }
                let mut header = [0u8; 14];
                let sidecar = if !input.copy_packet_out(index, 0, &mut header) {
                    NetStackEthernetV1 {
                        status: NET_STACK_ETHERNET_TRUNCATED,
                        ..empty_ethernet()
                    }
                } else {
                    let ethertype = u16::from_be_bytes([header[12], header[13]]);
                    let status = match ethertype {
                        0x0800 | 0x0806 | 0x86dd => NET_STACK_ETHERNET_ACCEPTED,
                        0x8100 | 0x88a8 => NET_STACK_ETHERNET_VLAN_UNSUPPORTED,
                        _ => NET_STACK_ETHERNET_UNSUPPORTED,
                    };
                    NetStackEthernetV1 {
                        destination: header[0..6].try_into().unwrap(),
                        source: header[6..12].try_into().unwrap(),
                        ethertype,
                        status,
                        reserved: [0; 5],
                    }
                };
                let network = if sidecar.status != NET_STACK_ETHERNET_ACCEPTED {
                    NetStackNetworkV1 {
                        outcome: net::stack::NET_STACK_NETWORK_SKIPPED,
                        ..empty_network()
                    }
                } else {
                    let facts = turn.packet_inputs[index];
                    match sidecar.ethertype {
                        0x0806 => parse_arp(input, index, turn.interface, addresses),
                        0x0800 => parse_ipv4(
                            input,
                            index,
                            turn.interface,
                            addresses,
                            facts.frame_len,
                            facts.checksums_validated != 0,
                        ),
                        0x86dd => {
                            parse_ipv6(input, index, turn.interface, addresses, facts.frame_len)
                        }
                        _ => return NET_STACK_CALL_STATUS_INVALID,
                    }
                };
                let transport = parse_transport(
                    input,
                    index,
                    &network,
                    turn.packet_inputs[index],
                    &rss_key,
                    rss_generation,
                );
                turn.ethernet[index] = sidecar;
                turn.network[index] = network;
                turn.transport[index] = transport;
                turn.committed = (index + 1) as u8;
            }
        }
        NET_STACK_OP_TX_HEADER => {
            if QUIESCED.load(Ordering::Acquire) {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 已把 TX 帧声明为本次 pinned call 的可访问范围；指针只在
            // 同步调用期间借用，ELM 不保存它。
            let output = unsafe { &mut *frame.tx_header };
            if !tx_header_frame_valid(output, frame.generation) {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 同时声明了只读 PacketChain 外壳范围，payload backing 只会
            // 由受能力约束的 copy_out 内核符号读取。
            let payload = unsafe { &*output.payload };
            let Some((header_len, header)) = build_tx_header(payload, &output.input) else {
                return NET_STACK_CALL_STATUS_INVALID;
            };
            output.header_len = header_len;
            output.header = header;
            output.committed = 1;
        }
        NET_STACK_OP_TX_FRAGMENT_HEADER => {
            if QUIESCED.load(Ordering::Acquire) || frame.reserved1[0] == 0 {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            let output_pointer =
                frame.reserved1[0] as usize as *mut NetStackTxFragmentHeaderV1;
            if !output_pointer.is_aligned() {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 将分片输出帧声明为本次 pinned call 的可访问范围；
            // 指针只在同步调用期间借用，ELM 不保存它。
            let output = unsafe { &mut *output_pointer };
            if !tx_fragment_frame_valid(output, frame.generation) {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 同时声明了只读 PacketChain 外壳范围，payload backing 只会
            // 由受能力约束的 copy_out 内核符号读取。
            let payload = unsafe { &*output.payload };
            let Some((header_len, header, payload_offset, payload_len, next_offset, more)) =
                build_tx_fragment_header(payload, &output.input)
            else {
                return NET_STACK_CALL_STATUS_INVALID;
            };
            output.header_len = header_len;
            output.header = header;
            output.payload_offset = payload_offset;
            output.payload_len = payload_len;
            output.next_fragment_offset = if more { next_offset } else { 0 };
            output.more_fragments = u8::from(more);
            output.committed = 1;
        }
        NET_STACK_OP_FLOW_CALL => {
            if QUIESCED.load(Ordering::Acquire) || frame.reserved1[0] == 0 {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            let call_pointer = frame.reserved1[0] as usize as *mut NetStackFlowCallV1;
            if !call_pointer.is_aligned() {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 将 state-call 帧声明为本次 pinned call 的可写范围；ELM
            // 只在同步调用期间访问并按 Option 所有权协议取放值。
            let call = unsafe { &mut *call_pointer };
            if !call.valid_header(frame.generation) || !dispatch_flow_call(call) {
                return NET_STACK_CALL_STATUS_INVALID;
            }
        }
        NET_STACK_OP_QUIESCE => {
            QUIESCED.store(true, Ordering::Release);
            frame.ready = 0;
            frame.quiesced = 1;
        }
        _ => return NET_STACK_CALL_STATUS_INVALID,
    }
    NET_STACK_CALL_STATUS_OK
}

#[elm::export(
    name = "net.stack.socket",
    contract = "mygo.net.stack-socket-call@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "private"
)]
fn net_stack_socket_call(frame: &mut net::stack::NetStackSocketCallV1) -> i32 {
    let request_pointer = frame.request;
    if !frame.valid(frame.opcode, frame.stack_generation, request_pointer) || frame.committed != 0 {
        return NET_STACK_CALL_STATUS_INVALID;
    }
    match frame.opcode {
        NET_STACK_SOCKET_OP_PROBE => {
            let quiesced = QUIESCED.load(Ordering::Acquire);
            frame.ready = u8::from(!quiesced);
            frame.quiesced = u8::from(quiesced);
            frame.committed = 1;
        }
        _ => {
            if request_pointer.is_null() || !request_pointer.is_aligned() {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: 宿主将请求声明为本次 pinned call 的可写范围；指针只在
            // 同步调用期间使用，ELM 不保存它。
            let request = unsafe { &mut *request_pointer };
            // socket 操作可能在宿主 control/lifecycle 队列回压时让出 CPU。此时同核
            // 后续调用不能自旋等待，否则持锁任务无法恢复；由宿主退出 ELM 后重试。
            let Some(mut table) = SOCKET_TABLE.try_lock() else {
                return NET_STACK_CALL_STATUS_BUSY;
            };
            let Some(table) = table.as_mut() else {
                return NET_STACK_CALL_STATUS_INVALID;
            };
            if !net::stack::dispatch_socket_table_call(
                table,
                request,
                QUIESCED.load(Ordering::Acquire),
            ) {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            frame.committed = 1;
        }
    }
    NET_STACK_CALL_STATUS_OK
}

#[cfg(not(feature = "elm-integrated"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
