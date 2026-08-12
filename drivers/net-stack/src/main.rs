#![no_std]
#![no_main]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, Ordering};

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
#[cfg(not(feature = "elm-integrated"))]
use net::stack::PinnedNetStackShardTurnEndpoint;
use net::stack::{
    NET_STACK_ADDRESS_FAMILY_IPV4, NET_STACK_ADDRESS_FAMILY_IPV6,
    NET_STACK_DROP_IPV4_CHECKSUM,
    NET_STACK_DROP_IPV6_EXTENSION_LIMIT, NET_STACK_DROP_MALFORMED_ARP,
    NET_STACK_DROP_MALFORMED_IPV4, NET_STACK_DROP_MALFORMED_IPV6, NET_STACK_DROP_MALFORMED_TCP,
    NET_STACK_DROP_MALFORMED_UDP, NET_STACK_DROP_NOT_LOCAL, NET_STACK_DROP_TCP_CHECKSUM,
    NET_STACK_DROP_UDP_CHECKSUM,
    NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL, NET_STACK_ETHERNET_ACCEPTED,
    NET_STACK_ETHERNET_TRUNCATED, NET_STACK_ETHERNET_UNSUPPORTED,
    NET_STACK_ETHERNET_VLAN_UNSUPPORTED, NET_STACK_NETWORK_ARP, NET_STACK_NETWORK_DROP,
    NET_STACK_NETWORK_FLAG_FRAGMENT, NET_STACK_NETWORK_FLAG_IPV6_PROBLEM,
    NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS, NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST,
    NET_STACK_NETWORK_IP, NET_STACK_SHARD_TURN_STATUS_BUSY, NET_STACK_SHARD_TURN_STATUS_INVALID,
    NET_STACK_SHARD_TURN_STATUS_OK,
    NET_STACK_TCP_OPTION_MSS, NET_STACK_TCP_OPTION_SACK_PERMITTED,
    NET_STACK_TCP_OPTION_TIMESTAMP, NET_STACK_TCP_OPTION_WINDOW_SCALE, NET_STACK_TRANSPORT_DROP,
    NET_STACK_TRANSPORT_ICMP, NET_STACK_TRANSPORT_RAW, NET_STACK_TRANSPORT_SKIPPED,
    NET_STACK_TRANSPORT_TCP, NET_STACK_TRANSPORT_UDP, NetStackEthernet,
    NetStackControlPlane, NetStackHandle, NetStackLocalAddress, NetStackLocalTurn, NetStackNetwork,
    NetStackShardTurn,
    NetStackRegisterErrorKind, NetStackRegistration, NetStackRemoveError, NetStackTcpOptions,
    NetStackTransport,
};
use net::{FlowExecution, FlowExecutorKind, FlowShard, ShardId};
use sched::sync::Spinlock;

use allocator as _;

static QUIESCED: AtomicBool = AtomicBool::new(false);
struct FlowShardSlot(UnsafeCell<Option<ManuallyDrop<FlowShard>>>);

unsafe impl Sync for FlowShardSlot {}

impl FlowShardSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }
}

static FLOW_SHARDS: [FlowShardSlot; sched::NR_CPUS] =
    [const { FlowShardSlot::new() }; sched::NR_CPUS];
static FLOW_EXECUTIONS: [FlowExecution; sched::NR_CPUS] =
    [const { FlowExecution::new() }; sched::NR_CPUS];
static CONTROL_PLANE: Spinlock<Option<Arc<Spinlock<ManuallyDrop<NetStackControlPlane>>>>> =
    Spinlock::new(None);

const fn empty_ethernet() -> NetStackEthernet {
    NetStackEthernet {
        destination: [0; 6],
        source: [0; 6],
        ethertype: 0,
        status: 0,
        reserved: [0; 5],
    }
}

fn ethernet_is_empty(sidecar: &NetStackEthernet) -> bool {
    sidecar.destination == [0; 6]
        && sidecar.source == [0; 6]
        && sidecar.ethertype == 0
        && sidecar.status == 0
        && sidecar.reserved == [0; 5]
}

fn packet_parse_header_valid(turn: &net::stack::NetStackPacketParse, generation: u64) -> bool {
    turn.struct_size as usize == core::mem::size_of::<net::stack::NetStackPacketParse>()
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

const fn empty_network() -> NetStackNetwork {
    NetStackNetwork {
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

fn network_is_empty(sidecar: &NetStackNetwork) -> bool {
    *sidecar == empty_network()
}

const fn network_drop(reason: u8) -> NetStackNetwork {
    NetStackNetwork {
        outcome: NET_STACK_NETWORK_DROP,
        drop_reason: reason,
        ..empty_network()
    }
}

fn address_projection_valid(address: &NetStackLocalAddress) -> bool {
    address.interface != 0
        && matches!(
            (address.family, address.prefix_len),
            (NET_STACK_ADDRESS_FAMILY_IPV4, 0..=32) | (NET_STACK_ADDRESS_FAMILY_IPV6, 0..=128)
        )
        && (address.family != NET_STACK_ADDRESS_FAMILY_IPV4 || address.address[4..] == [0; 12])
        && address.reserved0 == [0; 2]
        && address.reserved1 == [0; 8]
}

fn packet_inputs_valid(turn: &net::stack::NetStackPacketParse) -> bool {
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
        .all(|facts| *facts == net::stack::NetStackPacketInput::empty())
}

fn new_packet_parse(
    generation: u64,
    config_generation: u64,
    interface: u32,
    local_addresses: &[NetStackLocalAddress],
    rss_key: [u8; 40],
    rss_generation: u32,
    input: &net::buf::PacketBatch,
) -> net::stack::NetStackPacketParse {
    let turn = net::stack::NetStackPacketParse {
        struct_size: core::mem::size_of::<net::stack::NetStackPacketParse>() as u16,
        generation,
        config_generation,
        input,
        local_addresses: local_addresses.as_ptr(),
        interface,
        local_address_count: local_addresses.len() as u32,
        rss_key,
        rss_generation,
        input_count: input.len() as u8,
        committed: 0,
        reserved0: [0; 6],
        packet_inputs: net::stack::packet_batch_inputs(input),
        ethernet: [empty_ethernet(); net::tuning::PACKET_BATCH_CAPACITY],
        network: [empty_network(); net::tuning::PACKET_BATCH_CAPACITY],
        transport: [empty_transport(); net::tuning::PACKET_BATCH_CAPACITY],
        reserved1: [0; 2],
    };
    turn
}

fn packet_parse_committed(
    turn: &net::stack::NetStackPacketParse,
    input: &net::buf::PacketBatch,
) -> bool {
    turn.committed == turn.input_count
        && input.len() == usize::from(turn.input_count)
        && packet_inputs_valid(turn)
}

fn packet_parse_sidecars(
    turn: &net::stack::NetStackPacketParse,
) -> (
    &[NetStackEthernet],
    &[NetStackNetwork],
    &[NetStackTransport],
) {
    let count = usize::from(turn.input_count);
    (
        &turn.ethernet[..count],
        &turn.network[..count],
        &turn.transport[..count],
    )
}

fn parse_packet_batch(turn: &mut net::stack::NetStackPacketParse) -> bool {
    if !packet_parse_header_valid(turn, turn.generation) || turn.committed != 0 {
        return false;
    }
    // Safety: shard-turn 在调用此函数前已校验 host 持有的报文批次和地址投影；
    // 两个借用都会在同步调用返回前结束。
    let input = unsafe { &*turn.input };
    let Ok(address_count) = usize::try_from(turn.local_address_count) else {
        return false;
    };
    let Some(address_bytes) =
        address_count.checked_mul(core::mem::size_of::<NetStackLocalAddress>())
    else {
        return false;
    };
    let address_pointer = turn.local_addresses;
    if address_bytes > isize::MAX as usize || !address_pointer.is_aligned() {
        return false;
    }
    // Safety: 上文已校验长度和对齐，且地址投影只在解析当前有界批次时借用。
    let addresses = unsafe { core::slice::from_raw_parts(address_pointer, address_count) };
    let rss_key = turn.rss_key;
    let rss_generation = turn.rss_generation;
    if input.len() != usize::from(turn.input_count)
        || !packet_inputs_valid(turn)
        || !addresses.iter().all(address_projection_valid)
    {
        return false;
    }
    for index in 0..usize::from(turn.input_count) {
        if !ethernet_is_empty(&turn.ethernet[index])
            || !network_is_empty(&turn.network[index])
            || !transport_is_empty(&turn.transport[index])
        {
            return false;
        }
        let mut header = [0u8; 14];
        let sidecar = if !input.copy_packet_out(index, 0, &mut header) {
            NetStackEthernet {
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
            NetStackEthernet {
                destination: header[0..6].try_into().unwrap(),
                source: header[6..12].try_into().unwrap(),
                ethertype,
                status,
                reserved: [0; 5],
            }
        };
        let network = if sidecar.status != NET_STACK_ETHERNET_ACCEPTED {
            NetStackNetwork {
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
                0x86dd => parse_ipv6(input, index, turn.interface, addresses, facts.frame_len),
                _ => return false,
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
    true
}

fn is_local_ipv4(interface: u32, addresses: &[NetStackLocalAddress], address: [u8; 4]) -> bool {
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

fn is_local_ipv6(interface: u32, addresses: &[NetStackLocalAddress], address: [u8; 16]) -> bool {
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
    addresses: &[NetStackLocalAddress],
) -> NetStackNetwork {
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
    NetStackNetwork {
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
    addresses: &[NetStackLocalAddress],
    frame_len: u32,
    checksums_validated: bool,
) -> NetStackNetwork {
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
    NetStackNetwork {
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
    addresses: &[NetStackLocalAddress],
    frame_len: u32,
) -> NetStackNetwork {
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
    NetStackNetwork {
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

const fn empty_transport() -> NetStackTransport {
    NetStackTransport {
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
        tcp_options: NetStackTcpOptions {
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

fn transport_is_empty(sidecar: &NetStackTransport) -> bool {
    *sidecar == empty_transport()
}

const fn transport_skipped() -> NetStackTransport {
    NetStackTransport {
        outcome: NET_STACK_TRANSPORT_SKIPPED,
        ..empty_transport()
    }
}

const fn transport_drop(reason: u8) -> NetStackTransport {
    NetStackTransport {
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

fn transport_checksum_valid(
    input: &net::buf::PacketBatch,
    index: usize,
    network: &NetStackNetwork,
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
    network: &NetStackNetwork,
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
    facts: net::stack::NetStackPacketInput,
    network: &NetStackNetwork,
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
) -> Result<NetStackTcpOptions, u8> {
    let options_len = header_len - 20;
    let mut bytes = [0u8; 40];
    if !input.copy_packet_out(index, offset, &mut bytes[..options_len]) {
        return Err(NET_STACK_DROP_MALFORMED_TCP);
    }
    let mut parsed = NetStackTcpOptions::empty();
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
    network: &NetStackNetwork,
    facts: net::stack::NetStackPacketInput,
    rss_key: &[u8; 40],
    rss_generation: u32,
) -> NetStackTransport {
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
    NetStackTransport {
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
    network: &NetStackNetwork,
    facts: net::stack::NetStackPacketInput,
    rss_key: &[u8; 40],
    rss_generation: u32,
) -> NetStackTransport {
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
    NetStackTransport {
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
    network: &NetStackNetwork,
    facts: net::stack::NetStackPacketInput,
) -> NetStackTransport {
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
    NetStackTransport {
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
    network: &NetStackNetwork,
    facts: net::stack::NetStackPacketInput,
    rss_key: &[u8; 40],
    rss_generation: u32,
) -> NetStackTransport {
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
        _ => NetStackTransport {
            outcome: NET_STACK_TRANSPORT_RAW,
            protocol: network.next_header,
            payload_offset: network.payload_offset,
            payload_len: network.payload_len,
            ..empty_transport()
        },
    }
}

fn initialize_flow_shards(boot: net::boot::NetStackBootConfig, generation: u64) -> bool {
    let count = usize::from(boot.active_cpu_count());
    if count == 0 || count > sched::NR_CPUS {
        return false;
    }
    let now_ns = sched::now_ns_public();
    if count > FLOW_SHARDS.len() {
        return false;
    }
    for slot in &FLOW_SHARDS {
        // Safety: 初始化发生在发布任何 stack 调用之前。
        unsafe { *slot.0.get() = None };
    }
    for index in 0..count {
        if !FLOW_EXECUTIONS[index].install_generation(generation) {
            return false;
        }
        // Safety: 每个槽位在 generation 激活前只初始化一次。
        unsafe {
            *FLOW_SHARDS[index].0.get() = Some(ManuallyDrop::new(
                net::stack::create_flow_shard(ShardId(index as u16), boot, now_ns),
            ));
        }
    }
    *CONTROL_PLANE.lock() = Some(Arc::new(Spinlock::new(ManuallyDrop::new(
        net::stack::create_control_plane(count, *boot.rss_key(), boot.hash_seed()),
    ))));
    true
}

#[inline(never)]
fn parse_packet_batch_command(
    generation: u64,
    input: &mut Option<net::buf::PacketBatch>,
    interface: net::InterfaceId,
    config: *const net::control::ConfigSnapshot,
    output: &mut Option<net::pipeline::FrontendBatch>,
) -> bool {
    if output.is_some() || config.is_null() || !config.is_aligned() || interface.0 == 0 {
        return false;
    }
    let Some(mut input_batch) = input.take() else {
        return false;
    };
    // Safety: config 位于同步 shard-turn 登记的 host 地址范围内。
    let config = unsafe { &*config };
    let local_addresses = config.stack_local_addresses();
    let Some(boot) = net::stack::boot_config() else {
        *input = Some(input_batch);
        return false;
    };
    let mut rss_generation_bytes = [0; 4];
    rss_generation_bytes.copy_from_slice(&boot.generation_nonce()[..4]);
    let rss_generation = u32::from_le_bytes(rss_generation_bytes).max(1);
    let rss_key = *boot.rss_key();
    let mut turn = new_packet_parse(
        generation,
        config.generation,
        interface.0,
        &local_addresses,
        rss_key,
        rss_generation,
        &input_batch,
    );
    if !parse_packet_batch(&mut turn) || !packet_parse_committed(&turn, &input_batch) {
        *input = Some(input_batch);
        return false;
    }
    let (ethernet, network, transport) = packet_parse_sidecars(&turn);
    let Some(frontend) = net::stack::parse_frontend_packet_batch(
        &mut input_batch,
        ethernet,
        network,
        transport,
    ) else {
        *input = Some(input_batch);
        return false;
    };
    *input = Some(input_batch);
    *output = Some(frontend);
    true
}

#[inline(never)]
fn drain_reassembly_command(
    shard: &mut FlowShard,
    generation: u64,
    interface: net::InterfaceId,
    config: *const net::control::ConfigSnapshot,
    packets: &mut Vec<net::pipeline::FrontendPacket>,
    errors: &mut Vec<(
        net::InterfaceId,
        net::transport::ControlErrorTarget,
        net::transport::TransportControlError,
        u64,
    )>,
) -> bool {
    if config.is_null() || !config.is_aligned() || interface.0 == 0 {
        return false;
    }
    // Safety: config 位于同步 shard-turn 登记的 host 地址范围内。
    let config = unsafe { &*config };
    let local_addresses = config.stack_local_addresses();
    let Some(boot) = net::stack::boot_config() else {
        return false;
    };
    let mut rss_generation_bytes = [0; 4];
    rss_generation_bytes.copy_from_slice(&boot.generation_nonce()[..4]);
    let rss_generation = u32::from_le_bytes(rss_generation_bytes).max(1);
    let rss_key = *boot.rss_key();
    for _ in 0..net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY {
        let Some(input) = net::stack::flow_shard_take_reassembled_input(shard) else {
            break;
        };
        let mut turn = new_packet_parse(
            generation,
            config.generation,
            interface.0,
            &local_addresses,
            rss_key,
            rss_generation,
            &input,
        );
        if !parse_packet_batch(&mut turn) || !packet_parse_committed(&turn, &input) {
            drop(input);
            continue;
        }
        let (ethernet, network, transport) = packet_parse_sidecars(&turn);
        let _ = net::stack::flow_shard_parse_reassembled_batch(
            shard,
            input,
            ethernet,
            network,
            transport,
        );
        while let Some(packet) = net::stack::flow_shard_take_reassembled(shard) {
            packets.push(packet);
        }
        while let Some(error) = net::stack::flow_shard_take_forwarded_error(shard) {
            errors.push(error);
        }
    }
    while let Some(packet) = net::stack::flow_shard_take_reassembled(shard) {
        packets.push(packet);
    }
    while let Some(error) = net::stack::flow_shard_take_forwarded_error(shard) {
        errors.push(error);
    }
    true
}

fn dispatch_shard_turn(call: &mut NetStackShardTurn) -> i32 {
    let index = usize::from(call.shard.0);
    if (!call.commands.is_empty() || !call.control_commands.is_empty())
        && (index != sched::current_cpu_id() || index >= FLOW_SHARDS.len())
    {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    }
    if !call.control_commands.is_empty() && call.shard != net::ShardId(0) {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    }
    let lease = if call.commands.is_empty() {
        None
    } else {
        let Some(lease) = FLOW_EXECUTIONS[index].try_acquire(
            call.generation,
            FlowExecutorKind::Worker,
            sched::current_cpu_id(),
        ) else {
            FLOW_EXECUTIONS[index].mark_pending();
            return NET_STACK_SHARD_TURN_STATUS_BUSY;
        };
        Some(lease)
    };
    if !call.control_commands.is_empty() {
        let Some(control) = CONTROL_PLANE.lock().as_ref().cloned() else {
            return NET_STACK_SHARD_TURN_STATUS_INVALID;
        };
        let mut control = control.lock();
        for index in 0..call.control_commands.len() {
            let command = call
                .control_commands
                .get_mut(index)
                .expect("control command batch 索引有效");
            net::stack::dispatch_control_plane_call(&mut control, command);
        }
    }
    if call.commands.is_empty() {
        call.committed = 1;
        return NET_STACK_SHARD_TURN_STATUS_OK;
    }
    // Safety: 执行租约覆盖整个 shard turn，因此这是本轮对 shard 的唯一可变访问。
    let Some(shard) = (unsafe { &mut *FLOW_SHARDS[index].0.get() }).as_mut() else {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    };
    for index in 0..call.commands.len() {
        let command = call
            .commands
            .get_mut(index)
            .expect("flow command batch 索引有效");
        let valid = match command {
            net::stack::NetStackFlowCommand::ParsePacketBatch {
                input,
                interface,
                config,
                output,
            } => parse_packet_batch_command(
                call.generation,
                input,
                *interface,
                *config,
                output,
            ),
            net::stack::NetStackFlowCommand::DrainReassembly {
                interface,
                config,
                packets,
                errors,
            } => drain_reassembly_command(
                shard,
                call.generation,
                *interface,
                *config,
                packets,
                errors,
            ),
            command => net::stack::dispatch_flow_shard_command(shard, command),
        };
        if !valid {
            return NET_STACK_SHARD_TURN_STATUS_INVALID;
        }
    }
    if !net::stack::finalize_shard_turn_tx(shard, &mut call.commands, &mut call.tx_plans) {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    }
    call.committed = 1;
    let _ = lease
        .expect("非空 flow turn 必须持有执行租约")
        .release_and_recheck();
    NET_STACK_SHARD_TURN_STATUS_OK
}

fn dispatch_local_turn(turn: &mut NetStackLocalTurn) -> i32 {
    let index = usize::from(turn.shard.0);
    if index >= FLOW_SHARDS.len() || !turn.valid_header(turn.generation) {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    }
    let Some(lease) = FLOW_EXECUTIONS[index].try_acquire(
        turn.generation,
        FlowExecutorKind::Syscall,
        sched::current_cpu_id(),
    ) else {
        FLOW_EXECUTIONS[index].mark_pending();
        return NET_STACK_SHARD_TURN_STATUS_BUSY;
    };
    // Safety: syscall 与 owner worker 竞争同一租约，因而同步调用期间只有当前任务
    // 能修改这个 shard。turn 的命令所有权也只在本次调用期间借给动态模块。
    let Some(shard) = (unsafe { &mut *FLOW_SHARDS[index].0.get() }).as_mut() else {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    };
    let Some(command) = turn.command.as_mut() else {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    };
    if !net::stack::dispatch_flow_shard_command(shard, command) {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    }
    turn.committed = 1;
    let _ = lease.release_and_recheck();
    NET_STACK_SHARD_TURN_STATUS_OK
}

fn destroy_generation_state() {
    for (index, slot) in FLOW_SHARDS.iter().enumerate() {
        assert!(
            !FLOW_EXECUTIONS[index].snapshot().busy,
            "销毁协议 generation 时仍有执行者持有 shard 租约"
        );
        // Safety: quiesce 已在销毁 generation 前停止全部 owner 调用。
        let state = unsafe { (*slot.0.get()).take().map(|mut shard| ManuallyDrop::take(&mut shard)) };
        if let Some(state) = state {
            net::stack::destroy_flow_shard(state);
        }
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

    fn initialize(&mut self, context: &LifecycleContext) -> HookResult {
        if self.handle.is_some() {
            return Err(HookError::new(-16));
        }
        let boot = net::stack::boot_config().ok_or(HookError::new(-19))?;
        if boot.active_cpu_count() == 0 || usize::from(boot.active_cpu_count()) > sched::NR_CPUS {
            return Err(HookError::new(-22));
        }
        if !initialize_flow_shards(boot, context.generation().max(1)) {
            return Err(HookError::new(-12));
        }
        QUIESCED.store(false, Ordering::Release);
        #[cfg(not(feature = "elm-integrated"))]
        let registration = {
            let endpoint = PinnedNetStackShardTurnEndpoint::current(
                "net.stack.shard-turn",
                "mygo.net.stack-shard-turn@1",
                1,
            )
            .ok_or(HookError::new(-22))?;
            let local_endpoint = PinnedNetStackShardTurnEndpoint::current(
                "net.stack.local-turn",
                "mygo.net.stack-local-turn@1",
                1,
            )
            .ok_or(HookError::new(-22))?;
            NetStackRegistration::pinned_with_local(endpoint, local_endpoint)
                .ok_or(HookError::new(-22))?
        };
        #[cfg(feature = "elm-integrated")]
        let registration = NetStackRegistration::integrated_with_local(
            net_stack_shard_turn,
            net_stack_local_turn,
        )
        .ok_or(HookError::new(-22))?;
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
    name = "net.stack.shard-turn",
    contract = "mygo.net.stack-shard-turn@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "private"
)]
fn net_stack_shard_turn(turn: &mut net::stack::NetStackShardTurn) -> i32 {
    if QUIESCED.load(Ordering::Acquire)
        || turn.generation == 0
        || !turn.valid_header(turn.generation)
    {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    }
    dispatch_shard_turn(turn)
}

#[elm::export(
    name = "net.stack.local-turn",
    contract = "mygo.net.stack-local-turn@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "private"
)]
fn net_stack_local_turn(turn: &mut net::stack::NetStackLocalTurn) -> i32 {
    if QUIESCED.load(Ordering::Acquire)
        || turn.generation == 0
        || !turn.valid_header(turn.generation)
    {
        return NET_STACK_SHARD_TURN_STATUS_INVALID;
    }
    dispatch_local_turn(turn)
}

#[cfg(not(feature = "elm-integrated"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
