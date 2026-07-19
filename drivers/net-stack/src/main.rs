#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
#[cfg(not(feature = "elm-integrated"))]
use net::stack::PinnedNetStackEndpoint;
use net::stack::{
    NET_STACK_ADDRESS_FAMILY_IPV4, NET_STACK_ADDRESS_FAMILY_IPV6, NET_STACK_CALL_STATUS_INVALID,
    NET_STACK_CALL_STATUS_OK, NET_STACK_DROP_IPV4_CHECKSUM, NET_STACK_DROP_IPV6_EXTENSION_LIMIT,
    NET_STACK_DROP_MALFORMED_ARP, NET_STACK_DROP_MALFORMED_IPV4,
    NET_STACK_DROP_MALFORMED_IPV6, NET_STACK_DROP_NOT_LOCAL,
    NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL, NET_STACK_ETHERNET_ACCEPTED,
    NET_STACK_ETHERNET_TRUNCATED, NET_STACK_ETHERNET_UNSUPPORTED,
    NET_STACK_ETHERNET_VLAN_UNSUPPORTED, NET_STACK_NETWORK_ARP, NET_STACK_NETWORK_DROP,
    NET_STACK_NETWORK_FLAG_FRAGMENT, NET_STACK_NETWORK_FLAG_IPV6_PROBLEM,
    NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS, NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST,
    NET_STACK_NETWORK_IP, NET_STACK_OP_PROBE, NET_STACK_OP_QUIESCE, NET_STACK_OP_WORKER_TURN,
    NetStackEthernetV1, NetStackHandle, NetStackLocalAddressV1, NetStackNetworkV1,
    NetStackRegisterErrorKind, NetStackRegistration, NetStackRemoveError,
};

use allocator as _;

static QUIESCED: AtomicBool = AtomicBool::new(false);

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

fn worker_turn_header_valid(
    turn: &net::stack::NetStackWorkerTurnV1,
    generation: u64,
) -> bool {
    turn.abi_version == net::stack::NET_STACK_WORKER_TURN_ABI_VERSION
        && turn.struct_size as usize == core::mem::size_of::<net::stack::NetStackWorkerTurnV1>()
        && turn.generation == generation
        && turn.config_generation != 0
        && !turn.input.is_null()
        && !turn.local_addresses.is_null()
        && turn.interface != 0
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
            (NET_STACK_ADDRESS_FAMILY_IPV4, 0..=32)
                | (NET_STACK_ADDRESS_FAMILY_IPV6, 0..=128)
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
            && facts.reserved == [0; 2]
            && (facts.present == 1 || facts.frame_len == 0)
    }) && turn.packet_inputs[count..]
        .iter()
        .all(|facts| facts.frame_len == 0 && facts.present == 0 && facts.checksums_validated == 0)
}

fn is_local_ipv4(
    interface: u32,
    addresses: &[NetStackLocalAddressV1],
    address: [u8; 4],
) -> bool {
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

fn is_local_ipv6(
    interface: u32,
    addresses: &[NetStackLocalAddressV1],
    address: [u8; 16],
) -> bool {
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
        QUIESCED.store(false, Ordering::Release);
        #[cfg(not(feature = "elm-integrated"))]
        let registration = {
            let endpoint =
                PinnedNetStackEndpoint::current("net.stack.call", "mygo.net.stack-call@1", 1)
                    .ok_or(HookError::new(-22))?;
            NetStackRegistration::pinned(endpoint)
        };
        #[cfg(feature = "elm-integrated")]
        let registration =
            NetStackRegistration::integrated(net_stack_call).ok_or(HookError::new(-22))?;
        let handle = net::stack::register_stack(registration)
            .map_err(|error| map_register_error(error.kind))?;
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
            let Some(address_bytes) = address_count
                .checked_mul(core::mem::size_of::<NetStackLocalAddressV1>())
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
            if input.len() != usize::from(turn.input_count)
                || !packet_inputs_valid(turn)
                || !addresses.iter().all(address_projection_valid)
            {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            for index in 0..usize::from(turn.input_count) {
                if !ethernet_is_empty(&turn.ethernet[index])
                    || !network_is_empty(&turn.network[index])
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
                        0x86dd => parse_ipv6(
                            input,
                            index,
                            turn.interface,
                            addresses,
                            facts.frame_len,
                        ),
                        _ => return NET_STACK_CALL_STATUS_INVALID,
                    }
                };
                turn.ethernet[index] = sidecar;
                turn.network[index] = network;
                turn.committed = (index + 1) as u8;
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

#[cfg(not(feature = "elm-integrated"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
