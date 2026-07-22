//! 网络协议栈 ELM 与常驻 host 之间的生命周期契约。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::Mutex;

use crate::boot::NetStackBootConfig;
use crate::buf::{
    CompletionToken, DropReason, PacketBatch, PacketChain, PacketLayout, TxBatch, TxChecksum,
    TxPacket,
};
use crate::control::{
    BindError, BindRegistry, BindRequest, BindToken, ConfigSnapshot, NeighborKey,
};
use crate::flow::MAX_PENDING_NEIGHBOR_PACKETS_PER_INTERFACE;
use crate::flow::{FlowKey, FlowShard, FlowShardStats, UdpSendFailure};
pub use crate::flow::{NeighborEnqueueOutput, NeighborTimerOutput, PendingNeighborTx};
use crate::pipeline::FrontendPacket;
use crate::transport::{
    ControlErrorTarget, LocalUdpIngressError, PreparedRawTx, PreparedTcpTx, PreparedUdpTx,
    RawBindError, TcpBindError, TcpFlags, TcpIngressError, TcpPacket, TcpPath,
    TransportControlError, UdpBindError, UdpDatagram,
};
use crate::tuning::PACKET_BATCH_CAPACITY;
use crate::{
    Endpoint, FlowId, InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr, ListenGroup, ListenGroupId,
    MulticastMembership, ShardId, SocketError, SocketFacade, SocketId, TcpTxLease,
    TransportProtocol, UdpTxLease,
};

static NEXT_STACK_HANDLE: AtomicU64 = AtomicU64::new(1);
static STACK_BOOT_CONFIG: Mutex<Option<NetStackBootConfig>> = Mutex::new(None);
static STACK_REGISTRAR: Mutex<Option<&'static dyn NetStackRegistrar>> = Mutex::new(None);

pub const NET_STACK_SHARD_TURN_RUST_ABI: &str = "fn(&mutnet::stack::NetStackShardTurn)->i32";
pub const NET_STACK_SHARD_TURN_STATUS_OK: i32 = 0;
pub const NET_STACK_SHARD_TURN_STATUS_INVALID: i32 = -22;

pub const NET_STACK_SHARD_TURN_COMMAND_CAPACITY: usize = 1024;
pub const NET_STACK_TX_HEADER_CAPACITY: usize = 128;
pub const NET_STACK_TX_PLAN_CAPACITY: usize = 256;
pub const TX_FRAGMENT_UDP: u8 = 3;
pub const TX_FRAGMENT_RAW_IPV4: u8 = 4;
pub const NET_STACK_ETHERNET_ACCEPTED: u8 = 1;
pub const NET_STACK_ETHERNET_TRUNCATED: u8 = 2;
pub const NET_STACK_ETHERNET_UNSUPPORTED: u8 = 3;
pub const NET_STACK_ETHERNET_VLAN_UNSUPPORTED: u8 = 4;

pub const NET_STACK_ADDRESS_FAMILY_IPV4: u8 = 4;
pub const NET_STACK_ADDRESS_FAMILY_IPV6: u8 = 6;

pub const NET_STACK_NETWORK_SKIPPED: u8 = 1;
pub const NET_STACK_NETWORK_ARP: u8 = 2;
pub const NET_STACK_NETWORK_IP: u8 = 3;
pub const NET_STACK_NETWORK_DROP: u8 = 4;

pub const NET_STACK_DROP_MALFORMED_ARP: u8 = DropReason::MalformedArp as u8;
pub const NET_STACK_DROP_NOT_LOCAL: u8 = DropReason::NotLocal as u8;
pub const NET_STACK_DROP_MALFORMED_IPV4: u8 = DropReason::MalformedIpv4 as u8;
pub const NET_STACK_DROP_IPV4_CHECKSUM: u8 = DropReason::Ipv4Checksum as u8;
pub const NET_STACK_DROP_MALFORMED_IPV6: u8 = DropReason::MalformedIpv6 as u8;
pub const NET_STACK_DROP_IPV6_EXTENSION_LIMIT: u8 = DropReason::Ipv6ExtensionLimit as u8;
pub const NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL: u8 = DropReason::UnsupportedIpProtocol as u8;
pub const NET_STACK_DROP_MALFORMED_UDP: u8 = DropReason::MalformedUdp as u8;
pub const NET_STACK_DROP_UDP_CHECKSUM: u8 = DropReason::UdpChecksum as u8;
pub const NET_STACK_DROP_MALFORMED_TCP: u8 = DropReason::MalformedTcp as u8;
pub const NET_STACK_DROP_TCP_CHECKSUM: u8 = DropReason::TcpChecksum as u8;

pub const NET_STACK_NETWORK_FLAG_FRAGMENT: u8 = 1 << 0;
pub const NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS: u8 = 1 << 1;
pub const NET_STACK_NETWORK_FLAG_IPV6_PROBLEM: u8 = 1 << 2;
pub const NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST: u8 = 1 << 3;
const NET_STACK_NETWORK_FLAGS: u8 = NET_STACK_NETWORK_FLAG_FRAGMENT
    | NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS
    | NET_STACK_NETWORK_FLAG_IPV6_PROBLEM
    | NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST;

pub const NET_STACK_TRANSPORT_SKIPPED: u8 = 1;
pub const NET_STACK_TRANSPORT_TCP: u8 = 2;
pub const NET_STACK_TRANSPORT_UDP: u8 = 3;
pub const NET_STACK_TRANSPORT_ICMP: u8 = 4;
pub const NET_STACK_TRANSPORT_RAW: u8 = 5;
pub const NET_STACK_TRANSPORT_DROP: u8 = 6;

pub const NET_STACK_TCP_OPTION_MSS: u8 = 1 << 0;
pub const NET_STACK_TCP_OPTION_WINDOW_SCALE: u8 = 1 << 1;
pub const NET_STACK_TCP_OPTION_SACK_PERMITTED: u8 = 1 << 2;
pub const NET_STACK_TCP_OPTION_TIMESTAMP: u8 = 1 << 3;
const NET_STACK_TCP_OPTION_FLAGS: u8 = NET_STACK_TCP_OPTION_MSS
    | NET_STACK_TCP_OPTION_WINDOW_SCALE
    | NET_STACK_TCP_OPTION_SACK_PERMITTED
    | NET_STACK_TCP_OPTION_TIMESTAMP;

/// 配置快照提供给 stack ELM 的扁平本地地址投影。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackLocalAddress {
    pub interface: u32,
    pub family: u8,
    pub prefix_len: u8,
    pub reserved0: [u8; 2],
    pub address: [u8; 16],
    pub reserved1: [u8; 8],
}

impl NetStackLocalAddress {
    pub fn valid(&self) -> bool {
        self.interface != 0
            && matches!(
                (self.family, self.prefix_len),
                (NET_STACK_ADDRESS_FAMILY_IPV4, 0..=32) | (NET_STACK_ADDRESS_FAMILY_IPV6, 0..=128)
            )
            && (self.family != NET_STACK_ADDRESS_FAMILY_IPV4 || self.address[4..] == [0; 12])
            && self.reserved0 == [0; 2]
            && self.reserved1 == [0; 8]
    }
}

/// host 为一个只读输入 packet 固定的事实，ELM 不得修改。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackPacketInput {
    pub frame_len: u32,
    pub rss_hash: u32,
    pub rss_generation: u32,
    pub present: u8,
    pub checksums_validated: u8,
    pub rss_hash_present: u8,
    pub reserved: u8,
}

impl NetStackPacketInput {
    pub const fn empty() -> Self {
        Self {
            frame_len: 0,
            rss_hash: 0,
            rss_generation: 0,
            present: 0,
            checksums_validated: 0,
            rss_hash_present: 0,
            reserved: 0,
        }
    }

    fn matches_packet(&self, input: &PacketBatch, index: usize) -> bool {
        match (input.packet(index), input.metadata(index)) {
            (Some(packet), Some(metadata)) => {
                self.frame_len == packet.total_len() as u32
                    && self.rss_hash == metadata.rss_hash.unwrap_or(0)
                    && self.rss_generation == metadata.rss_generation
                    && self.present == 1
                    && self.checksums_validated == u8::from(metadata.checksums_validated)
                    && self.rss_hash_present == u8::from(metadata.rss_hash.is_some())
                    && self.reserved == 0
            }
            (None, None) => *self == Self::empty(),
            _ => false,
        }
    }
}

#[kernel_symbols::export(
    name = "net.stack.packet_batch_inputs",
    contract = "kernel.net.stack-packet-read@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK
)]
pub fn packet_batch_inputs(input: &PacketBatch) -> [NetStackPacketInput; PACKET_BATCH_CAPACITY] {
    let mut inputs = [NetStackPacketInput::empty(); PACKET_BATCH_CAPACITY];
    for (index, slot) in inputs.iter_mut().enumerate().take(input.len()) {
        if let (Some(packet), Some(metadata)) = (input.packet(index), input.metadata(index)) {
            *slot = NetStackPacketInput {
                frame_len: packet.total_len() as u32,
                rss_hash: metadata.rss_hash.unwrap_or(0),
                rss_generation: metadata.rss_generation,
                present: 1,
                checksums_validated: u8::from(metadata.checksums_validated),
                rss_hash_present: u8::from(metadata.rss_hash.is_some()),
                reserved: 0,
            };
        }
    }
    inputs
}

/// `net.stack` 为一个 RX packet 生成的只读 Ethernet 解析 sidecar。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackEthernet {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
    pub status: u8,
    pub reserved: [u8; 5],
}

impl NetStackEthernet {
    pub const fn empty() -> Self {
        Self {
            destination: [0; 6],
            source: [0; 6],
            ethertype: 0,
            status: 0,
            reserved: [0; 5],
        }
    }

    pub fn valid(&self) -> bool {
        matches!(
            self.status,
            NET_STACK_ETHERNET_ACCEPTED
                | NET_STACK_ETHERNET_TRUNCATED
                | NET_STACK_ETHERNET_UNSUPPORTED
                | NET_STACK_ETHERNET_VLAN_UNSUPPORTED
        ) && self.reserved == [0; 5]
    }
}

/// `net.stack` 为一个 RX packet 生成的网络层解析 sidecar。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackNetwork {
    pub outcome: u8,
    pub family: u8,
    pub next_header: u8,
    pub flags: u8,
    pub drop_reason: u8,
    pub traffic_class: u8,
    pub hop_limit: u8,
    pub reserved0: u8,
    pub header_len: u16,
    pub payload_offset: u16,
    pub fragment_offset: u16,
    pub arp_operation: u16,
    pub payload_len: u32,
    pub fragment_identification: u32,
    pub problem_pointer: u32,
    pub source: [u8; 16],
    pub destination: [u8; 16],
    pub arp_sender_mac: [u8; 6],
    pub arp_target_mac: [u8; 6],
    pub reserved1: [u8; 8],
}

impl NetStackNetwork {
    pub const fn empty() -> Self {
        Self {
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

    pub const fn skipped() -> Self {
        Self {
            outcome: NET_STACK_NETWORK_SKIPPED,
            ..Self::empty()
        }
    }

    pub fn valid(&self, frame_len: u32, ethernet: &NetStackEthernet) -> bool {
        if self.flags & !NET_STACK_NETWORK_FLAGS != 0
            || self.reserved0 != 0
            || self.reserved1 != [0; 8]
        {
            return false;
        }
        if ethernet.status != NET_STACK_ETHERNET_ACCEPTED {
            return *self == Self::skipped();
        }
        match self.outcome {
            NET_STACK_NETWORK_SKIPPED => false,
            NET_STACK_NETWORK_DROP => {
                valid_network_drop_reason(self.drop_reason)
                    && *self
                        == Self {
                            outcome: NET_STACK_NETWORK_DROP,
                            drop_reason: self.drop_reason,
                            ..Self::empty()
                        }
            }
            NET_STACK_NETWORK_ARP => {
                ethernet.ethertype == 0x0806
                    && self.family == NET_STACK_ADDRESS_FAMILY_IPV4
                    && matches!(self.arp_operation, 1 | 2)
                    && self.drop_reason == 0
                    && self.next_header == 0
                    && self.flags == 0
                    && self.source[4..] == [0; 12]
                    && self.destination[4..] == [0; 12]
                    && self.traffic_class == 0
                    && self.hop_limit == 0
                    && self.header_len == 0
                    && self.payload_offset == 0
                    && self.fragment_offset == 0
                    && self.payload_len == 0
                    && self.fragment_identification == 0
                    && self.problem_pointer == 0
            }
            NET_STACK_NETWORK_IP => {
                let family_valid = match self.family {
                    NET_STACK_ADDRESS_FAMILY_IPV4 => {
                        ethernet.ethertype == 0x0800
                            && (20..=60).contains(&self.header_len)
                            && self.source[4..] == [0; 12]
                            && self.destination[4..] == [0; 12]
                    }
                    NET_STACK_ADDRESS_FAMILY_IPV6 => {
                        ethernet.ethertype == 0x86dd && self.header_len >= 40
                    }
                    _ => false,
                };
                let fragment_valid = if self.flags & NET_STACK_NETWORK_FLAG_FRAGMENT != 0 {
                    self.fragment_identification != 0
                        || self.fragment_offset != 0
                        || self.flags & NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS != 0
                } else {
                    self.fragment_identification == 0
                        && self.fragment_offset == 0
                        && self.flags & NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS == 0
                };
                let problem_valid = if self.flags & NET_STACK_NETWORK_FLAG_IPV6_PROBLEM != 0 {
                    self.family == NET_STACK_ADDRESS_FAMILY_IPV6
                        && self.flags & NET_STACK_NETWORK_FLAG_FRAGMENT == 0
                        && self.problem_pointer != 0
                } else {
                    self.problem_pointer == 0
                        && self.flags & NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST == 0
                };
                family_valid
                    && fragment_valid
                    && problem_valid
                    && self.drop_reason == 0
                    && self.arp_operation == 0
                    && self.arp_sender_mac == [0; 6]
                    && self.arp_target_mac == [0; 6]
                    && u32::from(self.payload_offset) == 14 + u32::from(self.header_len)
                    && u32::from(self.payload_offset).saturating_add(self.payload_len) <= frame_len
            }
            _ => false,
        }
    }
}

fn valid_network_drop_reason(reason: u8) -> bool {
    matches!(
        reason,
        NET_STACK_DROP_MALFORMED_ARP
            | NET_STACK_DROP_NOT_LOCAL
            | NET_STACK_DROP_MALFORMED_IPV4
            | NET_STACK_DROP_IPV4_CHECKSUM
            | NET_STACK_DROP_MALFORMED_IPV6
            | NET_STACK_DROP_IPV6_EXTENSION_LIMIT
            | NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL
    )
}

/// `net.stack` 输出的 TCP option 投影。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackTcpOptions {
    pub flags: u8,
    pub window_scale: u8,
    pub sack_count: u8,
    pub reserved0: u8,
    pub maximum_segment_size: u16,
    pub reserved1: u16,
    pub sack_left: [u32; 4],
    pub sack_right: [u32; 4],
    pub timestamp_value: u32,
    pub timestamp_echo_reply: u32,
}

impl NetStackTcpOptions {
    pub const fn empty() -> Self {
        Self {
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
        }
    }

    fn valid(&self) -> bool {
        if self.flags & !NET_STACK_TCP_OPTION_FLAGS != 0
            || self.sack_count > 4
            || self.reserved0 != 0
            || self.reserved1 != 0
        {
            return false;
        }
        let mss_valid = if self.flags & NET_STACK_TCP_OPTION_MSS != 0 {
            self.maximum_segment_size != 0
        } else {
            self.maximum_segment_size == 0
        };
        let window_scale_valid = if self.flags & NET_STACK_TCP_OPTION_WINDOW_SCALE != 0 {
            self.window_scale <= 14
        } else {
            self.window_scale == 0
        };
        let timestamp_valid = if self.flags & NET_STACK_TCP_OPTION_TIMESTAMP != 0 {
            true
        } else {
            self.timestamp_value == 0 && self.timestamp_echo_reply == 0
        };
        let count = usize::from(self.sack_count);
        mss_valid
            && window_scale_valid
            && timestamp_valid
            && self.sack_left[count..] == [0; 4][count..]
            && self.sack_right[count..] == [0; 4][count..]
    }

    fn minimum_wire_len(&self) -> u16 {
        let mut len = 0;
        if self.flags & NET_STACK_TCP_OPTION_MSS != 0 {
            len += 4;
        }
        if self.flags & NET_STACK_TCP_OPTION_WINDOW_SCALE != 0 {
            len += 3;
        }
        if self.flags & NET_STACK_TCP_OPTION_SACK_PERMITTED != 0 {
            len += 2;
        }
        if self.sack_count != 0 {
            len += 2 + u16::from(self.sack_count) * 8;
        }
        if self.flags & NET_STACK_TCP_OPTION_TIMESTAMP != 0 {
            len += 10;
        }
        len
    }
}

/// `net.stack` 为一个 RX packet 生成的传输层解析与流分类 sidecar。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackTransport {
    pub outcome: u8,
    pub protocol: u8,
    pub drop_reason: u8,
    pub reserved0: u8,
    pub source_port: u16,
    pub destination_port: u16,
    pub header_len: u16,
    pub payload_offset: u16,
    pub tcp_flags: u16,
    pub tcp_window: u16,
    pub tcp_urgent_pointer: u16,
    pub reserved1: u16,
    pub payload_len: u32,
    pub rss_hash: u32,
    pub tcp_sequence: u32,
    pub tcp_acknowledgement: u32,
    pub tcp_options: NetStackTcpOptions,
    pub reserved2: [u64; 2],
}

impl NetStackTransport {
    pub const fn empty() -> Self {
        Self {
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
            tcp_options: NetStackTcpOptions::empty(),
            reserved2: [0; 2],
        }
    }

    pub const fn skipped() -> Self {
        Self {
            outcome: NET_STACK_TRANSPORT_SKIPPED,
            ..Self::empty()
        }
    }

    pub fn valid(&self, frame_len: u32, network: &NetStackNetwork) -> bool {
        if self.reserved0 != 0 || self.reserved1 != 0 || self.reserved2 != [0; 2] {
            return false;
        }
        let network_stops_transport = network.outcome != NET_STACK_NETWORK_IP
            || network.flags
                & (NET_STACK_NETWORK_FLAG_FRAGMENT | NET_STACK_NETWORK_FLAG_IPV6_PROBLEM)
                != 0;
        if network_stops_transport {
            return *self == Self::skipped();
        }
        match self.outcome {
            NET_STACK_TRANSPORT_SKIPPED => false,
            NET_STACK_TRANSPORT_DROP => {
                valid_transport_drop_reason(self.drop_reason)
                    && *self
                        == Self {
                            outcome: NET_STACK_TRANSPORT_DROP,
                            drop_reason: self.drop_reason,
                            ..Self::empty()
                        }
            }
            NET_STACK_TRANSPORT_TCP => {
                let header_len = u32::from(self.header_len);
                let expected_payload_offset =
                    u32::from(network.payload_offset).checked_add(header_len);
                let segment_len = self.payload_len.checked_add(header_len);
                let payload_end = u32::from(self.payload_offset).checked_add(self.payload_len);
                self.protocol == 6
                    && network.next_header == 6
                    && self.destination_port != 0
                    && (20..=60).contains(&self.header_len)
                    && self.header_len % 4 == 0
                    && self.drop_reason == 0
                    && self.tcp_flags & !0x01ff == 0
                    && self.tcp_options.valid()
                    && self.tcp_options.minimum_wire_len() <= self.header_len - 20
                    && expected_payload_offset == Some(u32::from(self.payload_offset))
                    && segment_len == Some(network.payload_len)
                    && payload_end.is_some_and(|end| end <= frame_len)
            }
            NET_STACK_TRANSPORT_UDP => {
                let expected_payload_offset = u32::from(network.payload_offset).checked_add(8);
                let network_end =
                    u32::from(network.payload_offset).checked_add(network.payload_len);
                let payload_end = u32::from(self.payload_offset).checked_add(self.payload_len);
                self.protocol == 17
                    && network.next_header == 17
                    && self.destination_port != 0
                    && self.header_len == 8
                    && self.payload_len <= u32::from(u16::MAX - 8)
                    && self.drop_reason == 0
                    && self.tcp_fields_empty()
                    && expected_payload_offset == Some(u32::from(self.payload_offset))
                    && payload_end
                        .zip(network_end)
                        .is_some_and(|(payload_end, network_end)| {
                            payload_end <= network_end && payload_end <= frame_len
                        })
            }
            NET_STACK_TRANSPORT_ICMP => {
                matches!(
                    (network.family, network.next_header),
                    (NET_STACK_ADDRESS_FAMILY_IPV4, 1) | (NET_STACK_ADDRESS_FAMILY_IPV6, 58)
                ) && self.protocol == network.next_header
                    && self.drop_reason == 0
                    && self.common_non_flow_fields_empty()
                    && self.payload_offset == network.payload_offset
                    && self.payload_len == network.payload_len
            }
            NET_STACK_TRANSPORT_RAW => {
                !matches!(network.next_header, 1 | 6 | 17 | 58)
                    && self.protocol == network.next_header
                    && self.drop_reason == 0
                    && self.common_non_flow_fields_empty()
                    && self.payload_offset == network.payload_offset
                    && self.payload_len == network.payload_len
            }
            _ => false,
        }
    }

    fn tcp_fields_empty(&self) -> bool {
        self.tcp_flags == 0
            && self.tcp_window == 0
            && self.tcp_urgent_pointer == 0
            && self.tcp_sequence == 0
            && self.tcp_acknowledgement == 0
            && self.tcp_options == NetStackTcpOptions::empty()
    }

    fn common_non_flow_fields_empty(&self) -> bool {
        self.source_port == 0
            && self.destination_port == 0
            && self.header_len == 0
            && self.rss_hash == 0
            && self.tcp_fields_empty()
    }
}

fn valid_transport_drop_reason(reason: u8) -> bool {
    matches!(
        reason,
        NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL
            | NET_STACK_DROP_MALFORMED_IPV6
            | NET_STACK_DROP_MALFORMED_UDP
            | NET_STACK_DROP_UDP_CHECKSUM
            | NET_STACK_DROP_MALFORMED_TCP
            | NET_STACK_DROP_TCP_CHECKSUM
    )
}

/// host 提交给 `net.stack` 的单个分片 header 构造输入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct TxFragmentInput {
    pub kind: u8,
    pub family: u8,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub source_mac: [u8; 6],
    pub destination_mac: [u8; 6],
    pub source_port: u16,
    pub destination_port: u16,
    pub payload_len: u32,
    pub mtu: u32,
    pub identification: u32,
    pub fragment_offset: u32,
    pub raw_header_len: u16,
    pub raw_flags: u16,
    pub source: [u8; 16],
    pub destination: [u8; 16],
    pub reserved: [u64; 2],
}

fn tx_addresses(
    source: crate::IpAddr,
    destination: crate::IpAddr,
) -> Option<(u8, [u8; 16], [u8; 16])> {
    match (source, destination) {
        (crate::IpAddr::V4(source), crate::IpAddr::V4(destination)) => {
            let mut source_bytes = [0; 16];
            source_bytes[..4].copy_from_slice(&source.0);
            let mut destination_bytes = [0; 16];
            destination_bytes[..4].copy_from_slice(&destination.0);
            Some((
                NET_STACK_ADDRESS_FAMILY_IPV4,
                source_bytes,
                destination_bytes,
            ))
        }
        (crate::IpAddr::V6(source), crate::IpAddr::V6(destination)) => {
            Some((NET_STACK_ADDRESS_FAMILY_IPV6, source.0, destination.0))
        }
        _ => None,
    }
}

impl TxFragmentInput {
    #[allow(clippy::too_many_arguments)]
    pub fn udp(
        source: crate::IpAddr,
        destination: crate::IpAddr,
        source_port: u16,
        destination_port: u16,
        source_mac: [u8; 6],
        destination_mac: [u8; 6],
        hop_limit: u8,
        traffic_class: u8,
        payload_len: u32,
        mtu: u32,
        identification: u32,
        fragment_offset: u32,
    ) -> Option<Self> {
        let (family, source, destination) = tx_addresses(source, destination)?;
        Some(Self {
            kind: TX_FRAGMENT_UDP,
            family,
            hop_limit,
            traffic_class,
            source_mac,
            destination_mac,
            source_port,
            destination_port,
            payload_len,
            mtu,
            identification,
            fragment_offset,
            raw_header_len: 0,
            raw_flags: 0,
            source,
            destination,
            reserved: [0; 2],
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn raw_ipv4(
        source: crate::IpAddr,
        destination: crate::IpAddr,
        source_mac: [u8; 6],
        destination_mac: [u8; 6],
        payload_len: u32,
        mtu: u32,
        identification: u32,
        fragment_offset: u32,
        raw_header_len: u16,
        raw_flags: u16,
    ) -> Option<Self> {
        let (family, source, destination) = tx_addresses(source, destination)?;
        (family == NET_STACK_ADDRESS_FAMILY_IPV4).then_some(Self {
            kind: TX_FRAGMENT_RAW_IPV4,
            family,
            hop_limit: 0,
            traffic_class: 0,
            source_mac,
            destination_mac,
            source_port: 0,
            destination_port: 0,
            payload_len,
            mtu,
            identification,
            fragment_offset,
            raw_header_len,
            raw_flags,
            source,
            destination,
            reserved: [0; 2],
        })
    }

    pub fn valid(&self) -> bool {
        if self.reserved != [0; 2]
            || self.mtu == 0
            || self.identification == 0
            || self.family != NET_STACK_ADDRESS_FAMILY_IPV4
                && self.family != NET_STACK_ADDRESS_FAMILY_IPV6
            || (self.family == NET_STACK_ADDRESS_FAMILY_IPV4
                && (self.source[4..] != [0; 12] || self.destination[4..] != [0; 12]))
        {
            return false;
        }
        match self.kind {
            TX_FRAGMENT_UDP => {
                self.source_port != 0
                    && self.destination_port != 0
                    && self.raw_header_len == 0
                    && self.raw_flags == 0
                    && self.fragment_offset <= self.payload_len
                    && self.fragment_offset % 8 == 0
                    && self.payload_len <= u32::from(u16::MAX - 8)
            }
            TX_FRAGMENT_RAW_IPV4 => {
                self.family == NET_STACK_ADDRESS_FAMILY_IPV4
                    && self.source_port == 0
                    && self.destination_port == 0
                    && self.payload_len <= u32::from(u16::MAX)
                    && self.fragment_offset % 8 == 0
                    && (self.raw_header_len == 0
                        || ((20..=60).contains(&self.raw_header_len)
                            && self.raw_header_len % 4 == 0
                            && u32::from(self.raw_header_len) <= self.payload_len))
            }
            _ => false,
        }
    }
}

/// `net.stack` 返回给 host 的单个分片 header 与 payload 范围。
pub struct TxFragmentPlan {
    pub input: TxFragmentInput,
    pub more_fragments: u8,
    pub header_len: u16,
    pub header: [u8; NET_STACK_TX_HEADER_CAPACITY],
    pub payload_offset: u32,
    pub payload_len: u32,
    pub next_fragment_offset: u32,
}

/// 协议栈生成的 TX plan 所引用的常驻 payload 所有权。
///
/// ELM 构造 checksum 和 header 时可以读取 payload；提交队列前，host 仍负责将选定
/// 范围 pin 住或复制到 DMA buffer。
pub enum TxPlanPayload {
    None,
    Tcp(Arc<TcpTxLease>),
    Datagram(Arc<UdpTxLease>),
}

impl TxPlanPayload {
    pub fn len(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Tcp(payload) => usize::from(payload.len),
            Self::Datagram(payload) => usize::from(payload.len),
        }
    }

    pub fn copy_range(&self, offset: usize, output: &mut [u8]) -> Result<(), SocketError> {
        match self {
            Self::None if offset == 0 && output.is_empty() => Ok(()),
            Self::None => Err(SocketError::Buffer),
            Self::Tcp(payload) => payload.copy_range(offset, output),
            Self::Datagram(payload) => payload.copy_range(offset, output),
        }
    }

    pub fn packet_chain(&self) -> Result<Option<PacketChain>, SocketError> {
        match self {
            Self::None => Ok(Some(PacketChain::new())),
            Self::Tcp(payload) => payload.packet_chain(),
            Self::Datagram(payload) => payload.packet_chain(),
        }
    }
}

/// `net.stack.shard-turn` 返回的一份完整报文发送计划。
pub struct TxPlan {
    pub interface: InterfaceId,
    pub facade: Arc<SocketFacade>,
    pub payload: TxPlanPayload,
    pub payload_offset: u32,
    pub payload_len: u32,
    pub header_len: u16,
    pub header: [u8; NET_STACK_TX_HEADER_CAPACITY],
    pub completion: CompletionToken,
    pub checksum: TxChecksum,
    pub layout: PacketLayout,
    pub low_latency: bool,
}

impl TxPlan {
    pub fn header_bytes(&self) -> &[u8] {
        &self.header[..usize::from(self.header_len)]
    }

    fn valid(&self) -> bool {
        self.interface.0 != 0
            && usize::from(self.header_len) <= self.header.len()
            && self.header[self.header_len as usize..]
                == [0; NET_STACK_TX_HEADER_CAPACITY][self.header_len as usize..]
            && self
                .payload_offset
                .checked_add(self.payload_len)
                .is_some_and(|end| end <= self.payload.len() as u32)
    }
}

/// 由 host 持有、单次 shard-turn 填充的固定容量输出区。
pub struct TxPlanBatch {
    slots: Box<[Option<TxPlan>]>,
    len: u16,
}

impl TxPlanBatch {
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(NET_STACK_TX_PLAN_CAPACITY);
        slots.resize_with(NET_STACK_TX_PLAN_CAPACITY, || None);
        Self {
            slots: slots.into_boxed_slice(),
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn remaining(&self) -> usize {
        self.slots.len().saturating_sub(self.len())
    }

    pub fn push(&mut self, plan: TxPlan) -> Result<(), TxPlan> {
        if self.len() == self.slots.len() {
            return Err(plan);
        }
        self.slots[self.len()] = Some(plan);
        self.len += 1;
        Ok(())
    }

    pub fn take(&mut self, index: usize) -> Option<TxPlan> {
        if index >= self.len() {
            return None;
        }
        let plan = self.slots[index].take();
        while self.len != 0 && self.slots[self.len as usize - 1].is_none() {
            self.len -= 1;
        }
        plan
    }

    pub fn clear(&mut self) {
        let len = self.len();
        for slot in self.slots.iter_mut().take(len) {
            *slot = None;
        }
        self.len = 0;
    }

    pub fn slots(&mut self) -> &mut [Option<TxPlan>] {
        &mut self.slots
    }

    fn valid(&self) -> bool {
        self.slots.len() == NET_STACK_TX_PLAN_CAPACITY
            && self.len() <= self.slots.len()
            && self.slots[..self.len()]
                .iter()
                .all(|slot| slot.as_ref().is_some_and(TxPlan::valid))
    }
}

impl Default for TxPlanBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl TxFragmentPlan {
    fn new(input: TxFragmentInput) -> Self {
        Self {
            input,
            more_fragments: 0,
            header_len: 0,
            header: [0; NET_STACK_TX_HEADER_CAPACITY],
            payload_offset: 0,
            payload_len: 0,
            next_fragment_offset: 0,
        }
    }

    pub fn valid(&self, payload: &PacketChain) -> bool {
        let end = self.payload_offset.checked_add(self.payload_len);
        self.more_fragments <= 1
            && end.is_some_and(|end| end <= payload.total_len() as u32)
            && self.payload_offset <= payload.total_len() as u32
            && self.header_len as usize <= self.header.len()
            && self.header[self.header_len as usize..]
                == [0; NET_STACK_TX_HEADER_CAPACITY][self.header_len as usize..]
            && if self.more_fragments != 0 {
                self.next_fragment_offset > self.input.fragment_offset
                    && self.next_fragment_offset <= self.input.payload_len
            } else {
                self.next_fragment_offset == 0
            }
            && self.output_valid(payload)
    }

    pub fn header_bytes(&self) -> &[u8] {
        &self.header[..usize::from(self.header_len)]
    }

    fn output_valid(&self, payload: &PacketChain) -> bool {
        let header = self.header_bytes();
        if header.len() < 34
            || header[..6] != self.input.destination_mac
            || header[6..12] != self.input.source_mac
        {
            return false;
        }
        match self.input.kind {
            TX_FRAGMENT_UDP => {
                let fragment_offset = match self.input.family {
                    NET_STACK_ADDRESS_FAMILY_IPV4 => {
                        if header.len() < 34 || header[12..14] != 0x0800u16.to_be_bytes() {
                            return false;
                        }
                        let expected_ip_len = 20
                            + usize::from(self.input.fragment_offset == 0) * 8
                            + self.payload_len as usize;
                        if header[14] != 0x45
                            || header[15] != self.input.traffic_class
                            || header[16..18] != (expected_ip_len as u16).to_be_bytes()
                            || header[18..20] != (self.input.identification as u16).to_be_bytes()
                            || header[22] != self.input.hop_limit
                            || header[23] != 17
                            || header[26..30] != self.input.source[..4]
                            || header[30..34] != self.input.destination[..4]
                            || checksum_bytes(&header[14..34]) != 0
                        {
                            return false;
                        }
                        let field = u16::from_be_bytes([header[20], header[21]]);
                        if u8::from(field & 0x2000 != 0) != self.more_fragments {
                            return false;
                        }
                        usize::from(field & 0x1fff) * 8
                    }
                    NET_STACK_ADDRESS_FAMILY_IPV6 => {
                        let expected_payload_len = 8
                            + usize::from(self.input.fragment_offset == 0) * 8
                            + self.payload_len as usize;
                        if header.len() < 62
                            || header[12..14] != 0x86ddu16.to_be_bytes()
                            || header[14..18]
                                != (0x6000_0000u32 | (u32::from(self.input.traffic_class) << 20))
                                    .to_be_bytes()
                            || header[18..20] != (expected_payload_len as u16).to_be_bytes()
                            || header[20] != 44
                            || header[21] != self.input.hop_limit
                            || header[22..38] != self.input.source
                            || header[38..54] != self.input.destination
                            || header[54] != 17
                            || header[55] != 0
                            || header[58..62] != self.input.identification.to_be_bytes()
                        {
                            return false;
                        }
                        let field = u16::from_be_bytes([header[56], header[57]]);
                        if u8::from(field & 1 != 0) != self.more_fragments || field & 6 != 0 {
                            return false;
                        }
                        usize::from(field >> 3) * 8
                    }
                    _ => return false,
                };
                let expected_fragment_offset = if self.input.fragment_offset == 0 {
                    0
                } else {
                    self.input.fragment_offset as usize + 8
                };
                if fragment_offset != expected_fragment_offset {
                    return false;
                }
                if self.input.fragment_offset == 0 {
                    let transport_offset = if self.input.family == NET_STACK_ADDRESS_FAMILY_IPV4 {
                        if self.header_len != 42 {
                            return false;
                        }
                        34
                    } else {
                        if self.header_len != 70 {
                            return false;
                        }
                        62
                    };
                    let udp = &header[transport_offset..transport_offset + 8];
                    udp[..2] == self.input.source_port.to_be_bytes()
                        && udp[2..4] == self.input.destination_port.to_be_bytes()
                        && udp[4..6] == ((self.input.payload_len + 8) as u16).to_be_bytes()
                        && udp[6..8] != [0; 2]
                } else {
                    self.header_len
                        == if self.input.family == NET_STACK_ADDRESS_FAMILY_IPV4 {
                            34
                        } else {
                            62
                        }
                }
            }
            TX_FRAGMENT_RAW_IPV4 => {
                let mut original = [0u8; 60];
                if payload.copy_out(0, &mut original[..20]).is_err() {
                    return false;
                }
                let original_header_len = usize::from(original[0] & 0x0f) * 4;
                if !(20..=60).contains(&original_header_len)
                    || payload
                        .copy_out(0, &mut original[..original_header_len])
                        .is_err()
                {
                    return false;
                }
                let expected_source = if original[12..16] == [0; 4] {
                    &self.input.source[..4]
                } else {
                    &original[12..16]
                };
                if self.input.family != NET_STACK_ADDRESS_FAMILY_IPV4
                    || usize::from(self.header_len) != 14 + original_header_len
                    || header[12..14] != 0x0800u16.to_be_bytes()
                    || header[14] != original[0]
                    || header[15] != original[1]
                    || header[16..18]
                        != ((original_header_len + self.payload_len as usize) as u16).to_be_bytes()
                    || header[18..20] != original[4..6]
                    || header[22..24] != original[8..10]
                    || header[26..30] != *expected_source
                    || header[30..34] != self.input.destination[..4]
                    || header[34..usize::from(self.header_len)] != original[20..original_header_len]
                    || checksum_bytes(&header[14..usize::from(self.header_len)]) != 0
                {
                    return false;
                }
                let field = u16::from_be_bytes([header[20], header[21]]);
                usize::from(field & 0x1fff) * 8 == self.input.fragment_offset as usize
                    && u8::from(field & 0x2000 != 0) == self.more_fragments
                    && field & 0x8000 == u16::from_be_bytes([original[6], original[7]]) & 0x8000
            }
            _ => false,
        }
    }
}

fn checksum_bytes(bytes: &[u8]) -> u16 {
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

#[derive(Clone, Copy)]
struct FragmentChecksum {
    sum: u64,
    pending: Option<u8>,
}

impl FragmentChecksum {
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

fn add_plan_payload(
    checksum: &mut FragmentChecksum,
    payload: &TxPlanPayload,
    offset: usize,
    len: usize,
) -> bool {
    let Some(end) = offset.checked_add(len) else {
        return false;
    };
    if end > payload.len() {
        return false;
    }
    let mut scratch = [0u8; 1024];
    let mut copied = 0usize;
    while copied < len {
        let chunk = (len - copied).min(scratch.len());
        if payload
            .copy_range(offset + copied, &mut scratch[..chunk])
            .is_err()
        {
            return false;
        }
        checksum.add(&scratch[..chunk]);
        copied += chunk;
    }
    true
}

fn add_pseudo_header(
    checksum: &mut FragmentChecksum,
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    len: usize,
) -> bool {
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let Ok(len) = u16::try_from(len) else {
                return false;
            };
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&[0, protocol]);
            checksum.add(&len.to_be_bytes());
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            let Ok(len) = u32::try_from(len) else {
                return false;
            };
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&len.to_be_bytes());
            checksum.add(&[0, 0, 0, protocol]);
        }
        _ => return false,
    }
    true
}

fn plan_transport_checksum(
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    transport_header: &[u8],
    payload: &TxPlanPayload,
) -> Option<u16> {
    let transport_len = transport_header.len().checked_add(payload.len())?;
    let mut checksum = FragmentChecksum::new();
    add_pseudo_header(&mut checksum, source, destination, protocol, transport_len).then_some(())?;
    checksum.add(transport_header);
    add_plan_payload(&mut checksum, payload, 0, payload.len()).then_some(())?;
    Some(checksum.finish())
}

fn build_tcp_tx_plan(work: PreparedTcpTx) -> Result<TxPlan, SocketError> {
    let payload = work
        .payload
        .map(|payload| TxPlanPayload::Tcp(Arc::new(payload)))
        .unwrap_or(TxPlanPayload::None);
    let options_len = usize::from(work.options_len);
    let tcp_header_len = 20usize
        .checked_add(options_len)
        .ok_or(SocketError::MessageTooLarge)?;
    let mut tcp = [0u8; 60];
    tcp[0..2].copy_from_slice(&work.local_port.to_be_bytes());
    tcp[2..4].copy_from_slice(&work.remote.port.to_be_bytes());
    tcp[4..8].copy_from_slice(&work.sequence.0.to_be_bytes());
    tcp[8..12].copy_from_slice(&work.acknowledgement.0.to_be_bytes());
    tcp[12] = ((tcp_header_len / 4) as u8) << 4 | u8::from(work.flags.contains(TcpFlags::NS));
    tcp[13] = work.flags.bits() as u8;
    tcp[14..16].copy_from_slice(&work.window.to_be_bytes());
    tcp[20..tcp_header_len].copy_from_slice(&work.options[..options_len]);
    let checksum = plan_transport_checksum(
        work.path.route.source,
        work.remote.addr,
        6,
        &tcp[..tcp_header_len],
        &payload,
    )
    .ok_or(SocketError::Buffer)?;
    tcp[16..18].copy_from_slice(&checksum.to_be_bytes());

    let mut header = [0u8; NET_STACK_TX_HEADER_CAPACITY];
    header[..6].copy_from_slice(&work.path.destination_mac);
    header[6..12].copy_from_slice(&work.path.source_mac);
    let header_len = match (work.path.route.source, work.remote.addr) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let total_len = 20usize
                .checked_add(tcp_header_len)
                .and_then(|len| len.checked_add(payload.len()))
                .and_then(|len| u16::try_from(len).ok())
                .ok_or(SocketError::MessageTooLarge)?;
            header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let ip = &mut header[14..34];
            ip[0] = 0x45;
            ip[2..4].copy_from_slice(&total_len.to_be_bytes());
            ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            ip[8] = 64;
            ip[9] = 6;
            ip[12..16].copy_from_slice(&source.0);
            ip[16..20].copy_from_slice(&destination.0);
            let checksum = checksum_bytes(ip);
            ip[10..12].copy_from_slice(&checksum.to_be_bytes());
            header[34..34 + tcp_header_len].copy_from_slice(&tcp[..tcp_header_len]);
            34 + tcp_header_len
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            let transport_len = tcp_header_len
                .checked_add(payload.len())
                .and_then(|len| u16::try_from(len).ok())
                .ok_or(SocketError::MessageTooLarge)?;
            header[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let ip = &mut header[14..54];
            ip[0] = 0x60;
            ip[4..6].copy_from_slice(&transport_len.to_be_bytes());
            ip[6] = 6;
            ip[7] = 64;
            ip[8..24].copy_from_slice(&source.0);
            ip[24..40].copy_from_slice(&destination.0);
            header[54..54 + tcp_header_len].copy_from_slice(&tcp[..tcp_header_len]);
            54 + tcp_header_len
        }
        _ => return Err(SocketError::InvalidState),
    };
    Ok(TxPlan {
        interface: work.path.route.interface,
        facade: work.facade,
        payload_len: payload.len() as u32,
        payload,
        payload_offset: 0,
        header_len: header_len as u16,
        header,
        completion: CompletionToken(work.completion),
        checksum: TxChecksum::Complete,
        layout: PacketLayout::Plain,
        low_latency: work.low_latency,
    })
}

fn build_udp_tx_plan(work: PreparedUdpTx) -> Result<TxPlan, SocketError> {
    let facade = work.payload.facade();
    let payload = TxPlanPayload::Datagram(Arc::new(work.payload));
    let udp_len = payload
        .len()
        .checked_add(8)
        .and_then(|len| u16::try_from(len).ok())
        .ok_or(SocketError::MessageTooLarge)?;
    let mut udp = [0u8; 8];
    udp[0..2].copy_from_slice(&work.source_port.to_be_bytes());
    udp[2..4].copy_from_slice(&work.destination.port.to_be_bytes());
    udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
    let checksum =
        plan_transport_checksum(work.route.source, work.destination.addr, 17, &udp, &payload)
            .ok_or(SocketError::Buffer)?;
    udp[6..8].copy_from_slice(&if checksum == 0 { 0xffff } else { checksum }.to_be_bytes());

    let mut header = [0u8; NET_STACK_TX_HEADER_CAPACITY];
    header[..6].copy_from_slice(&work.destination_mac);
    header[6..12].copy_from_slice(&work.source_mac);
    let header_len = match (work.route.source, work.destination.addr) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let total_len = payload
                .len()
                .checked_add(28)
                .and_then(|len| u16::try_from(len).ok())
                .ok_or(SocketError::MessageTooLarge)?;
            header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let ip = &mut header[14..34];
            ip[0] = 0x45;
            ip[1] = work.traffic_class;
            ip[2..4].copy_from_slice(&total_len.to_be_bytes());
            ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
            ip[8] = work.hop_limit;
            ip[9] = 17;
            ip[12..16].copy_from_slice(&source.0);
            ip[16..20].copy_from_slice(&destination.0);
            let checksum = checksum_bytes(ip);
            ip[10..12].copy_from_slice(&checksum.to_be_bytes());
            header[34..42].copy_from_slice(&udp);
            42
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            header[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let ip = &mut header[14..54];
            ip[..4].copy_from_slice(
                &(0x6000_0000u32 | (u32::from(work.traffic_class) << 20)).to_be_bytes(),
            );
            ip[4..6].copy_from_slice(&udp_len.to_be_bytes());
            ip[6] = 17;
            ip[7] = work.hop_limit;
            ip[8..24].copy_from_slice(&source.0);
            ip[24..40].copy_from_slice(&destination.0);
            header[54..62].copy_from_slice(&udp);
            62
        }
        _ => return Err(SocketError::InvalidState),
    };
    Ok(TxPlan {
        interface: work.route.interface,
        facade,
        payload_len: payload.len() as u32,
        payload,
        payload_offset: 0,
        header_len: header_len as u16,
        header,
        completion: work.completion,
        checksum: TxChecksum::Complete,
        layout: PacketLayout::Plain,
        low_latency: false,
    })
}

fn copy_datagram_payload(payload: &UdpTxLease) -> Result<PacketChain, SocketError> {
    let mut bytes = Vec::new();
    bytes.resize(usize::from(payload.len), 0);
    payload.copy_out(&mut bytes)?;
    Ok(PacketChain::from_owned(bytes))
}

fn udp_fragment_count(work: &PreparedUdpTx) -> Option<usize> {
    let ip_header_len = match work.route.source {
        IpAddr::V4(_) => 20usize,
        IpAddr::V6(_) => 40usize,
    };
    if ip_header_len + 8 + usize::from(work.payload.len) <= work.route.mtu as usize {
        return Some(1);
    }
    let capacity = match work.route.source {
        IpAddr::V4(_) => (work.route.mtu as usize).checked_sub(20)?,
        IpAddr::V6(_) => (work.route.mtu as usize).checked_sub(48)?,
    } & !7;
    if capacity < 8 {
        return None;
    }
    let datagram_len = usize::from(work.payload.len) + 8;
    Some(datagram_len.div_ceil(capacity))
}

fn append_udp_fragment_plans(
    shard: &mut FlowShard,
    work: PreparedUdpTx,
    output: &mut TxPlanBatch,
) -> Result<(), SocketError> {
    let payload_chain = copy_datagram_payload(&work.payload)?;
    let payload = TxPlanPayload::Datagram(Arc::new(work.payload));
    let facade = payload_facade(&payload).ok_or(SocketError::InvalidState)?;
    let identification = shard.allocate_fragment_id();
    let mut fragment_offset = 0u32;
    loop {
        let input = TxFragmentInput::udp(
            work.route.source,
            work.destination.addr,
            work.source_port,
            work.destination.port,
            work.source_mac,
            work.destination_mac,
            work.hop_limit,
            work.traffic_class,
            payload.len() as u32,
            work.route.mtu,
            identification,
            fragment_offset,
        )
        .ok_or(SocketError::InvalidState)?;
        let fragment = build_tx_fragment_plan(&payload_chain, input)
            .map_err(|_| SocketError::MessageTooLarge)?;
        output
            .push(TxPlan {
                interface: work.route.interface,
                facade: Arc::clone(&facade),
                payload: match &payload {
                    TxPlanPayload::Datagram(payload) => {
                        TxPlanPayload::Datagram(Arc::clone(payload))
                    }
                    _ => unreachable!(),
                },
                payload_offset: fragment.payload_offset,
                payload_len: fragment.payload_len,
                header_len: fragment.header_len,
                header: fragment.header,
                completion: work.completion,
                checksum: TxChecksum::Complete,
                layout: PacketLayout::Plain,
                low_latency: false,
            })
            .map_err(|_| SocketError::WouldBlock)?;
        if fragment.more_fragments == 0 {
            break;
        }
        fragment_offset = fragment.next_fragment_offset;
    }
    Ok(())
}

fn payload_facade(payload: &TxPlanPayload) -> Option<Arc<SocketFacade>> {
    match payload {
        TxPlanPayload::None => None,
        TxPlanPayload::Tcp(payload) => Some(payload.facade()),
        TxPlanPayload::Datagram(payload) => Some(payload.facade()),
    }
}

fn raw_fragment_count(work: &PreparedRawTx) -> Option<usize> {
    if !work.header_included || usize::from(work.payload.len) <= work.route.mtu as usize {
        return Some(1);
    }
    let mut header = [0u8; 60];
    work.payload.copy_range(0, &mut header[..20]).ok()?;
    let header_len = usize::from(header[0] & 0x0f) * 4;
    if header[0] >> 4 != 4
        || !(20..=60).contains(&header_len)
        || usize::from(work.payload.len) < header_len
    {
        return None;
    }
    let capacity = (work.route.mtu as usize).checked_sub(header_len)? & !7;
    if capacity < 8 {
        return None;
    }
    Some((usize::from(work.payload.len) - header_len).div_ceil(capacity))
}

fn build_raw_tx_plan(work: PreparedRawTx) -> Result<TxPlan, SocketError> {
    let facade = work.payload.facade();
    let mut header = [0u8; NET_STACK_TX_HEADER_CAPACITY];
    header[..6].copy_from_slice(&work.destination_mac);
    header[6..12].copy_from_slice(&work.source_mac);
    let (header_len, payload_offset, payload_len) = if work.header_included {
        let mut ip = [0u8; 60];
        work.payload.copy_range(0, &mut ip[..20])?;
        let ip_header_len = usize::from(ip[0] & 0x0f) * 4;
        if ip[0] >> 4 != 4
            || !(20..=60).contains(&ip_header_len)
            || usize::from(work.payload.len) < ip_header_len
        {
            return Err(SocketError::InvalidState);
        }
        work.payload.copy_range(0, &mut ip[..ip_header_len])?;
        let (IpAddr::V4(route_source), IpAddr::V4(route_destination)) =
            (work.route.source, work.destination)
        else {
            return Err(SocketError::InvalidState);
        };
        let destination = Ipv4Addr(ip[16..20].try_into().unwrap());
        if !destination.is_unspecified() && destination != route_destination {
            return Err(SocketError::InvalidState);
        }
        if usize::from(work.payload.len) > work.route.mtu as usize {
            return Err(SocketError::MessageTooLarge);
        }
        if ip[12..16] == [0; 4] {
            ip[12..16].copy_from_slice(&route_source.0);
        }
        ip[16..20].copy_from_slice(&route_destination.0);
        ip[2..4].copy_from_slice(&work.payload.len.to_be_bytes());
        ip[10..12].fill(0);
        let checksum = checksum_bytes(&ip[..ip_header_len]);
        ip[10..12].copy_from_slice(&checksum.to_be_bytes());
        header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        header[14..14 + ip_header_len].copy_from_slice(&ip[..ip_header_len]);
        (
            14 + ip_header_len,
            ip_header_len as u32,
            u32::from(work.payload.len) - ip_header_len as u32,
        )
    } else {
        let payload_len = usize::from(work.payload.len);
        match (work.route.source, work.destination) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => {
                let total_len = payload_len
                    .checked_add(20)
                    .and_then(|len| u16::try_from(len).ok())
                    .ok_or(SocketError::MessageTooLarge)?;
                if payload_len + 20 > work.route.mtu as usize {
                    return Err(SocketError::MessageTooLarge);
                }
                header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
                let ip = &mut header[14..34];
                ip[0] = 0x45;
                ip[1] = work.traffic_class;
                ip[2..4].copy_from_slice(&total_len.to_be_bytes());
                ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
                ip[8] = work.hop_limit;
                ip[9] = work.protocol;
                ip[12..16].copy_from_slice(&source.0);
                ip[16..20].copy_from_slice(&destination.0);
                let checksum = checksum_bytes(ip);
                ip[10..12].copy_from_slice(&checksum.to_be_bytes());
                (34, 0, payload_len as u32)
            }
            (IpAddr::V6(source), IpAddr::V6(destination)) => {
                let payload_len_u16 =
                    u16::try_from(payload_len).map_err(|_| SocketError::MessageTooLarge)?;
                if payload_len + 40 > work.route.mtu as usize {
                    return Err(SocketError::MessageTooLarge);
                }
                header[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
                let ip = &mut header[14..54];
                ip[..4].copy_from_slice(
                    &(0x6000_0000u32 | (u32::from(work.traffic_class) << 20)).to_be_bytes(),
                );
                ip[4..6].copy_from_slice(&payload_len_u16.to_be_bytes());
                ip[6] = work.protocol;
                ip[7] = work.hop_limit;
                ip[8..24].copy_from_slice(&source.0);
                ip[24..40].copy_from_slice(&destination.0);
                (54, 0, payload_len as u32)
            }
            _ => return Err(SocketError::InvalidState),
        }
    };
    Ok(TxPlan {
        interface: work.route.interface,
        facade,
        payload: TxPlanPayload::Datagram(Arc::new(work.payload)),
        payload_offset,
        payload_len,
        header_len: header_len as u16,
        header,
        completion: work.completion,
        checksum: TxChecksum::Complete,
        layout: PacketLayout::Plain,
        low_latency: false,
    })
}

fn append_raw_fragment_plans(
    shard: &mut FlowShard,
    work: PreparedRawTx,
    output: &mut TxPlanBatch,
) -> Result<(), SocketError> {
    let payload_chain = copy_datagram_payload(&work.payload)?;
    let payload = TxPlanPayload::Datagram(Arc::new(work.payload));
    let facade = payload_facade(&payload).ok_or(SocketError::InvalidState)?;
    let identification = shard.allocate_fragment_id();
    let mut fragment_offset = 0u32;
    loop {
        let input = TxFragmentInput::raw_ipv4(
            work.route.source,
            work.destination,
            work.source_mac,
            work.destination_mac,
            payload.len() as u32,
            work.route.mtu,
            identification,
            fragment_offset,
            0,
            0,
        )
        .ok_or(SocketError::InvalidState)?;
        let fragment = build_tx_fragment_plan(&payload_chain, input)
            .map_err(|_| SocketError::MessageTooLarge)?;
        output
            .push(TxPlan {
                interface: work.route.interface,
                facade: Arc::clone(&facade),
                payload: match &payload {
                    TxPlanPayload::Datagram(payload) => {
                        TxPlanPayload::Datagram(Arc::clone(payload))
                    }
                    _ => unreachable!(),
                },
                payload_offset: fragment.payload_offset,
                payload_len: fragment.payload_len,
                header_len: fragment.header_len,
                header: fragment.header,
                completion: work.completion,
                checksum: TxChecksum::Complete,
                layout: PacketLayout::Plain,
                low_latency: false,
            })
            .map_err(|_| SocketError::WouldBlock)?;
        if fragment.more_fragments == 0 {
            break;
        }
        fragment_offset = fragment.next_fragment_offset;
    }
    Ok(())
}

pub enum TxPlanAppendResult {
    Appended,
    Deferred(PendingNeighborTx),
}

/// 将一项已完成路由解析的协议 TX 工作转换为完整报文发送计划。
pub fn append_tx_plans(
    shard: &mut FlowShard,
    work: PendingNeighborTx,
    output: &mut TxPlanBatch,
) -> TxPlanAppendResult {
    if work.key_opt().is_some() {
        return TxPlanAppendResult::Deferred(work);
    }
    let required = match &work {
        PendingNeighborTx::Tcp(_) => Some(1),
        PendingNeighborTx::Udp(work) => udp_fragment_count(work),
        PendingNeighborTx::Raw(work) => raw_fragment_count(work),
    };
    let Some(required) = required else {
        work.facade()
            .set_pending_error(SocketError::MessageTooLarge);
        return TxPlanAppendResult::Appended;
    };
    if required > NET_STACK_TX_PLAN_CAPACITY {
        work.facade()
            .set_pending_error(SocketError::MessageTooLarge);
        return TxPlanAppendResult::Appended;
    }
    if required > output.remaining() {
        return TxPlanAppendResult::Deferred(work);
    }
    let facade = work.facade();
    let result = match work {
        PendingNeighborTx::Tcp(work) => build_tcp_tx_plan(work)
            .and_then(|plan| output.push(plan).map_err(|_| SocketError::WouldBlock)),
        PendingNeighborTx::Udp(work) if required == 1 => build_udp_tx_plan(work)
            .and_then(|plan| output.push(plan).map_err(|_| SocketError::WouldBlock)),
        PendingNeighborTx::Udp(work) => append_udp_fragment_plans(shard, work, output),
        PendingNeighborTx::Raw(work) if required == 1 => build_raw_tx_plan(work)
            .and_then(|plan| output.push(plan).map_err(|_| SocketError::WouldBlock)),
        PendingNeighborTx::Raw(work) => append_raw_fragment_plans(shard, work, output),
    };
    if let Err(error) = result {
        facade.set_pending_error(error);
    }
    TxPlanAppendResult::Appended
}

fn fragment_udp_checksum(
    payload: &PacketChain,
    input: &TxFragmentInput,
    header: &[u8; 8],
) -> Option<u16> {
    let total_len = 8usize.checked_add(input.payload_len as usize)?;
    let mut checksum = FragmentChecksum::new();
    match input.family {
        NET_STACK_ADDRESS_FAMILY_IPV4 => {
            checksum.add(&input.source[..4]);
            checksum.add(&input.destination[..4]);
            checksum.add(&[0, 17]);
            checksum.add(&u16::try_from(total_len).ok()?.to_be_bytes());
        }
        NET_STACK_ADDRESS_FAMILY_IPV6 => {
            checksum.add(&input.source);
            checksum.add(&input.destination);
            checksum.add(&(total_len as u32).to_be_bytes());
            checksum.add(&[0, 0, 0, 17]);
        }
        _ => return None,
    }
    checksum.add(header);
    payload
        .for_each_slice(0, input.payload_len as usize, |slice| {
            checksum.add(slice);
            Ok::<_, ()>(())
        })
        .ok()?;
    Some(checksum.finish())
}

pub fn build_tx_fragment_plan(
    payload: &PacketChain,
    input: TxFragmentInput,
) -> Result<TxFragmentPlan, NetStackTxError> {
    if !input.valid() || input.payload_len as usize > payload.total_len() {
        return Err(NetStackTxError::InvalidInput);
    }
    let mut output = TxFragmentPlan::new(input);
    let mut header = [0u8; NET_STACK_TX_HEADER_CAPACITY];
    header[..6].copy_from_slice(&input.destination_mac);
    header[6..12].copy_from_slice(&input.source_mac);
    let (header_len, payload_offset, payload_len, next_offset, more) = match input.kind {
        TX_FRAGMENT_UDP => {
            let datagram_len = 8usize
                .checked_add(input.payload_len as usize)
                .ok_or(NetStackTxError::InvalidInput)?;
            let fragment_capacity = match input.family {
                NET_STACK_ADDRESS_FAMILY_IPV4 => (input.mtu as usize)
                    .checked_sub(20)
                    .map(|capacity| capacity & !7),
                NET_STACK_ADDRESS_FAMILY_IPV6 => (input.mtu as usize)
                    .checked_sub(48)
                    .map(|capacity| capacity & !7),
                _ => None,
            }
            .filter(|capacity| *capacity >= 8)
            .ok_or(NetStackTxError::InvalidInput)?;
            let datagram_offset = if input.fragment_offset == 0 {
                0usize
            } else {
                8usize
                    .checked_add(input.fragment_offset as usize)
                    .ok_or(NetStackTxError::InvalidInput)?
            };
            if datagram_offset >= datagram_len {
                return Err(NetStackTxError::InvalidInput);
            }
            let chunk_len = fragment_capacity.min(datagram_len - datagram_offset);
            let first = input.fragment_offset == 0;
            let payload_offset = input.fragment_offset;
            let payload_len = chunk_len
                .checked_sub(usize::from(first) * 8)
                .and_then(|len| u32::try_from(len).ok())
                .ok_or(NetStackTxError::InvalidInput)?;
            let next_offset = payload_offset
                .checked_add(payload_len)
                .ok_or(NetStackTxError::InvalidInput)?;
            let more = next_offset < input.payload_len;
            let fragment_field =
                u16::try_from(datagram_offset / 8).map_err(|_| NetStackTxError::InvalidInput)?;
            match input.family {
                NET_STACK_ADDRESS_FAMILY_IPV4 => {
                    header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
                    header[14] = 0x45;
                    header[15] = input.traffic_class;
                    header[16..18].copy_from_slice(
                        &u16::try_from(20 + chunk_len)
                            .map_err(|_| NetStackTxError::InvalidInput)?
                            .to_be_bytes(),
                    );
                    header[18..20].copy_from_slice(&(input.identification as u16).to_be_bytes());
                    header[20..22].copy_from_slice(
                        &(fragment_field | if more { 0x2000 } else { 0 }).to_be_bytes(),
                    );
                    header[22] = input.hop_limit;
                    header[23] = 17;
                    header[26..30].copy_from_slice(&input.source[..4]);
                    header[30..34].copy_from_slice(&input.destination[..4]);
                    let checksum = checksum_bytes(&header[14..34]);
                    header[24..26].copy_from_slice(&checksum.to_be_bytes());
                    if first {
                        let udp = &mut header[34..42];
                        udp[..2].copy_from_slice(&input.source_port.to_be_bytes());
                        udp[2..4].copy_from_slice(&input.destination_port.to_be_bytes());
                        udp[4..6].copy_from_slice(
                            &u16::try_from(datagram_len)
                                .map_err(|_| NetStackTxError::InvalidInput)?
                                .to_be_bytes(),
                        );
                        let mut checksum_header = [0u8; 8];
                        checksum_header.copy_from_slice(udp);
                        let value = fragment_udp_checksum(payload, &input, &checksum_header)
                            .ok_or(NetStackTxError::InvalidInput)?;
                        udp[6..8].copy_from_slice(
                            &(if value == 0 { 0xffff } else { value }).to_be_bytes(),
                        );
                    }
                    (
                        34 + usize::from(first) * 8,
                        payload_offset,
                        payload_len,
                        next_offset,
                        more,
                    )
                }
                NET_STACK_ADDRESS_FAMILY_IPV6 => {
                    header[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
                    header[14..18].copy_from_slice(
                        &(0x6000_0000u32 | (u32::from(input.traffic_class) << 20)).to_be_bytes(),
                    );
                    header[18..20].copy_from_slice(
                        &u16::try_from(8 + chunk_len)
                            .map_err(|_| NetStackTxError::InvalidInput)?
                            .to_be_bytes(),
                    );
                    header[20] = 44;
                    header[21] = input.hop_limit;
                    header[22..38].copy_from_slice(&input.source);
                    header[38..54].copy_from_slice(&input.destination);
                    header[54] = 17;
                    header[56..58]
                        .copy_from_slice(&((fragment_field << 3) | u16::from(more)).to_be_bytes());
                    header[58..62].copy_from_slice(&input.identification.to_be_bytes());
                    if first {
                        let udp = &mut header[62..70];
                        udp[..2].copy_from_slice(&input.source_port.to_be_bytes());
                        udp[2..4].copy_from_slice(&input.destination_port.to_be_bytes());
                        udp[4..6].copy_from_slice(
                            &u16::try_from(datagram_len)
                                .map_err(|_| NetStackTxError::InvalidInput)?
                                .to_be_bytes(),
                        );
                        let mut checksum_header = [0u8; 8];
                        checksum_header.copy_from_slice(udp);
                        let value = fragment_udp_checksum(payload, &input, &checksum_header)
                            .ok_or(NetStackTxError::InvalidInput)?;
                        udp[6..8].copy_from_slice(
                            &(if value == 0 { 0xffff } else { value }).to_be_bytes(),
                        );
                    }
                    (
                        62 + usize::from(first) * 8,
                        payload_offset,
                        payload_len,
                        next_offset,
                        more,
                    )
                }
                _ => return Err(NetStackTxError::InvalidInput),
            }
        }
        TX_FRAGMENT_RAW_IPV4 => {
            if payload.total_len() != input.payload_len as usize || payload.total_len() < 20 {
                return Err(NetStackTxError::InvalidInput);
            }
            let mut ip = [0u8; 60];
            payload
                .copy_out(0, &mut ip[..20])
                .map_err(|_| NetStackTxError::InvalidInput)?;
            let ip_header_len = usize::from(ip[0] & 0x0f) * 4;
            if ip[0] >> 4 != 4
                || !(20..=60).contains(&ip_header_len)
                || ip_header_len % 4 != 0
                || input.raw_header_len != 0 && usize::from(input.raw_header_len) != ip_header_len
            {
                return Err(NetStackTxError::InvalidInput);
            }
            payload
                .copy_out(0, &mut ip[..ip_header_len])
                .map_err(|_| NetStackTxError::InvalidInput)?;
            let body_len = payload
                .total_len()
                .checked_sub(ip_header_len)
                .ok_or(NetStackTxError::InvalidInput)?;
            let capacity = (input.mtu as usize)
                .checked_sub(ip_header_len)
                .map(|capacity| capacity & !7)
                .filter(|capacity| *capacity >= 8)
                .ok_or(NetStackTxError::InvalidInput)?;
            if input.fragment_offset as usize >= body_len {
                return Err(NetStackTxError::InvalidInput);
            }
            let chunk_len = capacity.min(body_len - input.fragment_offset as usize);
            let more = input.fragment_offset as usize + chunk_len < body_len;
            let flags = u16::from_be_bytes([ip[6], ip[7]]);
            if flags & 0x4000 != 0 {
                return Err(NetStackTxError::InvalidInput);
            }
            ip[2..4].copy_from_slice(
                &u16::try_from(ip_header_len + chunk_len)
                    .map_err(|_| NetStackTxError::InvalidInput)?
                    .to_be_bytes(),
            );
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
            let checksum = checksum_bytes(&ip[..ip_header_len]);
            ip[10..12].copy_from_slice(&checksum.to_be_bytes());
            header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            header[14..14 + ip_header_len].copy_from_slice(&ip[..ip_header_len]);
            let next_offset = input.fragment_offset.saturating_add(chunk_len as u32);
            (
                14 + ip_header_len,
                ip_header_len as u32 + input.fragment_offset,
                chunk_len as u32,
                next_offset,
                more,
            )
        }
        _ => return Err(NetStackTxError::InvalidInput),
    };
    output.header_len = header_len as u16;
    output.header = header;
    output.payload_offset = payload_offset;
    output.payload_len = payload_len;
    output.next_fragment_offset = if more { next_offset } else { 0 };
    output.more_fragments = u8::from(more);
    if output.valid(payload) {
        Ok(output)
    } else {
        Err(NetStackTxError::InvalidInput)
    }
}

/// 常驻 worker 与 `net.stack` 间一次批调用的数据帧。
///
/// `input` 在同步调用期间始终归 host 所有。ELM 只能读取它，并逐项提交固定容量
/// sidecar；只有调用成功且 host 完成全帧校验后，packet ownership 才会移动。
#[repr(C)]
pub struct NetStackPacketParse {
    pub struct_size: u16,
    pub generation: u64,
    pub config_generation: u64,
    pub input: *const PacketBatch,
    pub local_addresses: *const NetStackLocalAddress,
    pub interface: u32,
    pub local_address_count: u32,
    pub rss_key: [u8; 40],
    pub rss_generation: u32,
    pub input_count: u8,
    pub committed: u8,
    pub reserved0: [u8; 6],
    pub packet_inputs: [NetStackPacketInput; PACKET_BATCH_CAPACITY],
    pub ethernet: [NetStackEthernet; PACKET_BATCH_CAPACITY],
    pub network: [NetStackNetwork; PACKET_BATCH_CAPACITY],
    pub transport: [NetStackTransport; PACKET_BATCH_CAPACITY],
    pub reserved1: [u64; 2],
}

impl NetStackPacketParse {
    pub fn new(
        generation: u64,
        config_generation: u64,
        interface: u32,
        local_addresses: &[NetStackLocalAddress],
        rss_key: [u8; 40],
        rss_generation: u32,
        input: &PacketBatch,
    ) -> Self {
        let mut turn = Self {
            struct_size: core::mem::size_of::<Self>() as u16,
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
            packet_inputs: [NetStackPacketInput::empty(); PACKET_BATCH_CAPACITY],
            ethernet: [NetStackEthernet::empty(); PACKET_BATCH_CAPACITY],
            network: [NetStackNetwork::empty(); PACKET_BATCH_CAPACITY],
            transport: [NetStackTransport::empty(); PACKET_BATCH_CAPACITY],
            reserved1: [0; 2],
        };
        for index in 0..input.len() {
            if let (Some(packet), Some(metadata)) = (input.packet(index), input.metadata(index)) {
                turn.packet_inputs[index] = NetStackPacketInput {
                    frame_len: packet.total_len() as u32,
                    rss_hash: metadata.rss_hash.unwrap_or(0),
                    rss_generation: metadata.rss_generation,
                    present: 1,
                    checksums_validated: u8::from(metadata.checksums_validated),
                    rss_hash_present: u8::from(metadata.rss_hash.is_some()),
                    reserved: 0,
                };
            }
        }
        turn
    }

    pub fn valid_header(
        &self,
        generation: u64,
        config_generation: u64,
        interface: u32,
        input: *const PacketBatch,
        local_addresses: *const NetStackLocalAddress,
        local_address_count: u32,
        rss_key: &[u8; 40],
        rss_generation: u32,
    ) -> bool {
        self.struct_size as usize == core::mem::size_of::<Self>()
            && self.generation == generation
            && self.config_generation == config_generation
            && self.input == input
            && !self.input.is_null()
            && self.local_addresses == local_addresses
            && self.interface == interface
            && interface != 0
            && self.local_address_count == local_address_count
            && &self.rss_key == rss_key
            && self.rss_generation == rss_generation
            && rss_generation != 0
            && usize::from(self.input_count) <= PACKET_BATCH_CAPACITY
            && self.reserved0 == [0; 6]
            && self.reserved1 == [0; 2]
    }

    pub fn fully_committed(&self, input: &PacketBatch) -> bool {
        self.committed == self.input_count
            && self.packet_inputs[..usize::from(self.input_count)]
                .iter()
                .enumerate()
                .all(|(index, facts)| facts.matches_packet(input, index))
            && self.packet_inputs[usize::from(self.input_count)..]
                .iter()
                .all(|facts| *facts == NetStackPacketInput::empty())
            && self.ethernet[..usize::from(self.input_count)]
                .iter()
                .all(NetStackEthernet::valid)
            && self.ethernet[usize::from(self.input_count)..]
                .iter()
                .all(|sidecar| *sidecar == NetStackEthernet::empty())
            && self.network[..usize::from(self.input_count)]
                .iter()
                .enumerate()
                .all(|(index, sidecar)| {
                    sidecar.valid(self.packet_inputs[index].frame_len, &self.ethernet[index])
                })
            && self.network[usize::from(self.input_count)..]
                .iter()
                .all(|sidecar| *sidecar == NetStackNetwork::empty())
            && self.transport[..usize::from(self.input_count)]
                .iter()
                .enumerate()
                .all(|(index, sidecar)| {
                    sidecar.valid(self.packet_inputs[index].frame_len, &self.network[index])
                })
            && self.transport[usize::from(self.input_count)..]
                .iter()
                .all(|sidecar| *sidecar == NetStackTransport::empty())
    }

    pub fn ethernet(&self) -> &[NetStackEthernet] {
        &self.ethernet[..usize::from(self.input_count)]
    }

    pub fn network(&self) -> &[NetStackNetwork] {
        &self.network[..usize::from(self.input_count)]
    }

    pub fn transport(&self) -> &[NetStackTransport] {
        &self.transport[..usize::from(self.input_count)]
    }
}

/// host 与 `net.stack` ELM 之间的 shard 状态操作。
///
/// 该枚举只在与内核 build-bound 的 Rust ABI 调用中使用；拥有所有权的输入放在
/// `Option` 中，ELM 仅在确认 generation 与 shard 后取走。
pub enum NetStackFlowCommand {
    Stats {
        output: Option<FlowShardStats>,
    },
    RunDueTimers {
        now_ns: u64,
    },
    NextTimerDeadline {
        output: Option<Option<u64>>,
    },
    BindUdp {
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        output: Option<Result<FlowId, UdpBindError>>,
    },
    BindUdpFacade {
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
        free_bind: bool,
        accepts_ipv4: bool,
        output: Option<Result<FlowId, UdpBindError>>,
    },
    ReconnectUdpFacade {
        flow: FlowId,
        local: Endpoint,
        peer: Endpoint,
        facade: Arc<SocketFacade>,
        output: Option<Result<FlowId, UdpBindError>>,
    },
    CloseUdp {
        flow: FlowId,
    },
    BindRawFacade {
        local: IpAddr,
        interface: Option<InterfaceId>,
        facade: Arc<SocketFacade>,
        free_bind: bool,
        output: Option<Result<FlowId, RawBindError>>,
    },
    CloseRaw {
        flow: FlowId,
    },
    ListenTcp {
        local: Endpoint,
        interface: Option<InterfaceId>,
        group: Arc<ListenGroup>,
        output: Option<Result<(), TcpBindError>>,
    },
    ConnectTcp {
        local: Endpoint,
        remote: Endpoint,
        path: TcpPath,
        facade: Arc<SocketFacade>,
        control_sequence: u64,
        now_ns: u64,
        output: Option<Result<FlowId, TcpBindError>>,
    },
    CloseTcp {
        flow: FlowId,
        now_ns: u64,
    },
    AbortTcp {
        flow: FlowId,
        now_ns: u64,
    },
    ShutdownTcpWrite {
        flow: FlowId,
        now_ns: u64,
    },
    CloseTcpListener {
        group: ListenGroupId,
        output: Option<bool>,
    },
    DrainTcpSend {
        flow: FlowId,
        now_ns: u64,
    },
    TakeTcpOutputBatch {
        output: Option<Vec<PreparedTcpTx>>,
        inline_pool_installs: Option<Vec<(Arc<SocketFacade>, InterfaceId)>>,
        needs_resume: Option<bool>,
        limit: u16,
        resume_budget: u16,
        inline_local_tcp: bool,
        config: *const ConfigSnapshot,
        now_ns: u64,
    },
    ResolveTcpPath {
        destination: IpAddr,
        bound_source: Option<IpAddr>,
        interface: Option<InterfaceId>,
        config: *const ConfigSnapshot,
        now_ns: u64,
        free_bind: bool,
        output: Option<Result<TcpPath, SocketError>>,
    },
    ProcessLocalTcpWork {
        interface: InterfaceId,
        work: Option<PreparedTcpTx>,
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<Result<FlowId, TcpIngressError>>,
    },
    ProcessLocalUdpWork {
        interface: InterfaceId,
        work: Option<PreparedUdpTx>,
        now_ns: u64,
        output: Option<Result<FlowId, LocalUdpIngressError>>,
    },
    PlanTxWork {
        work: Option<PendingNeighborTx>,
    },
    InvalidateInterface {
        interface: InterfaceId,
        output: Option<usize>,
    },
    EnqueueNeighbor {
        work: Option<PendingNeighborTx>,
        now_ns: u64,
        interface_limit: u16,
        output: Option<NeighborEnqueueOutput>,
    },
    ObserveAndResolveNeighbor {
        key: NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
        output: Option<Vec<PendingNeighborTx>>,
    },
    FailInterfaceNeighbors {
        interface: InterfaceId,
        output: Option<Vec<PendingNeighborTx>>,
    },
    RunNeighborTimers {
        now_ns: u64,
        output: Option<NeighborTimerOutput>,
    },
    PrepareUdpTx {
        flow: FlowId,
        payload: Option<UdpTxLease>,
        mark: u32,
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<Result<Option<PreparedUdpTx>, (SocketError, UdpTxLease)>>,
    },
    PrepareRawTx {
        flow: FlowId,
        payload: Option<UdpTxLease>,
        mark: u32,
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<Result<Option<PreparedRawTx>, (SocketError, UdpTxLease)>>,
    },
    FormUdpPacket {
        flow: FlowId,
        destination: Option<Endpoint>,
        payload: Option<PacketChain>,
        mark: u32,
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<Result<TxPacket, UdpSendFailure>>,
    },
    RecvUdp {
        flow: FlowId,
        output: Option<Option<UdpDatagram>>,
    },
    ParsePacketBatch {
        input: Option<PacketBatch>,
        interface: InterfaceId,
        config: *const ConfigSnapshot,
        output: Option<crate::pipeline::FrontendBatch>,
    },
    ProcessFrontendBatch {
        packets: Option<Vec<FrontendPacket>>,
        interface: InterfaceId,
        local_mac: [u8; 6],
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<(TxBatch, PacketBatch)>,
        drop_counts: [u32; DropReason::COUNT],
        stats: Option<FlowShardStats>,
    },
    DrainReassembly {
        interface: InterfaceId,
        config: *const ConfigSnapshot,
        packets: Vec<FrontendPacket>,
        errors: Vec<(InterfaceId, ControlErrorTarget, TransportControlError, u64)>,
    },
    ApplyTransportError {
        interface: InterfaceId,
        target: ControlErrorTarget,
        error: TransportControlError,
        now_ns: u64,
        output: Option<bool>,
    },
}

/// 常驻 host 对 ELM 全局网络控制面的同步操作。
///
/// 命令与返回值只在一次 build-bound Rust ABI 调用中存活，不允许 ELM 保存其中引用。
pub enum NetStackControlCommand {
    ConfigureActiveShards {
        count: u16,
        output: Option<bool>,
    },
    InitializeAutoconfig {
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<bool>,
    },
    RunDad {
        now_ns: u64,
        output: Option<DadRunOutput>,
    },
    ObserveDadConflict {
        interface: InterfaceId,
        address: Ipv6Addr,
    },
    RunDhcp {
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<DhcpRunOutput>,
    },
    HandleDhcpPacket {
        interface: InterfaceId,
        packet: Option<FrontendPacket>,
        now_ns: u64,
        output: Option<DhcpPacketOutput>,
    },
    RemoveAutoconfigInterface {
        interface: InterfaceId,
        output: Option<Option<DhcpLeaseChange>>,
    },
    ReserveBinding {
        socket: SocketId,
        request: BindRequest,
        shard: ShardId,
        output: Option<Result<BindToken, BindError>>,
    },
    ReleaseBinding {
        socket: SocketId,
        output: Option<bool>,
    },
    AllocateListener {
        output: Option<ListenGroupId>,
    },
    InstallListener {
        group: ListenGroupId,
        output: Option<bool>,
    },
    RemoveListener {
        group: ListenGroupId,
        output: Option<bool>,
    },
    FlowShard {
        remote: Endpoint,
        local: Endpoint,
        protocol: TransportProtocol,
        output: Option<ShardId>,
    },
    NeighborOwner {
        key: NeighborKey,
        output: Option<ShardId>,
    },
    JoinMulticast {
        socket: SocketId,
        membership: MulticastMembership,
        interface: InterfaceId,
        output: Option<Option<bool>>,
    },
    LeaveMulticast {
        socket: SocketId,
        membership: MulticastMembership,
        output: Option<Option<(InterfaceId, bool)>>,
    },
    MulticastGroups {
        interface: InterfaceId,
        output: Option<Vec<IpAddr>>,
    },
    RemoveInterfaceMulticast {
        interface: InterfaceId,
    },
    RemoveSocketMulticast {
        socket: SocketId,
        output: Option<Vec<(InterfaceId, IpAddr)>>,
    },
}

struct DadState {
    interface: InterfaceId,
    address: Ipv6Addr,
    probe_sent: bool,
    conflict: bool,
    deadline_ns: u64,
}

pub struct DadRunOutput {
    pub probes: Vec<NeighborKey>,
    pub ready: Vec<(InterfaceId, Ipv6Addr)>,
    pub next_deadline_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhcpLease {
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub lease_seconds: u32,
}

enum DhcpPhase {
    Discovering,
    Requesting {
        lease: DhcpLease,
        server: Ipv4Addr,
    },
    Bound {
        lease: DhcpLease,
        server: Ipv4Addr,
        renew_ns: u64,
        rebind_ns: u64,
        expires_ns: u64,
    },
}

struct DhcpClient {
    interface: InterfaceId,
    mac_address: [u8; 6],
    transaction_id: u32,
    phase: DhcpPhase,
    next_action_ns: u64,
    retry_seconds: u32,
    installed: Option<DhcpLease>,
}

struct DhcpReply {
    message_type: u8,
    transaction_id: u32,
    client_mac: [u8; 6],
    offered: Ipv4Addr,
    server: Option<Ipv4Addr>,
    subnet_mask: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    lease_seconds: Option<u32>,
    renewal_seconds: Option<u32>,
    rebinding_seconds: Option<u32>,
}

pub struct DhcpLeaseChange {
    pub interface: InterfaceId,
    pub old: Option<DhcpLease>,
    pub new: Option<DhcpLease>,
    pub retained_dns: Vec<Ipv4Addr>,
}

pub struct DhcpRunOutput {
    pub frames: Vec<(InterfaceId, Vec<u8>)>,
    pub lease_changes: Vec<DhcpLeaseChange>,
    pub next_deadline_ns: Option<u64>,
}

pub struct DhcpPacketOutput {
    pub handled: bool,
    pub lease_change: Option<DhcpLeaseChange>,
}

/// 由 `net.stack` ELM 独占的全局控制面状态。
pub struct NetStackControlPlane {
    bind_registry: BindRegistry,
    bindings: Mutex<BTreeMap<SocketId, BindToken>>,
    listeners: Mutex<BTreeSet<ListenGroupId>>,
    next_listener: AtomicU64,
    rss_key: [u8; 40],
    shard_capacity: usize,
    active_shards: AtomicUsize,
    dad: Mutex<Vec<DadState>>,
    dad_errors: Mutex<BTreeMap<InterfaceId, SocketError>>,
    dhcp: Mutex<Vec<DhcpClient>>,
    multicast_refs: Mutex<BTreeMap<(InterfaceId, IpAddr), usize>>,
    multicast_bindings: Mutex<BTreeMap<(SocketId, MulticastMembership), InterfaceId>>,
}

impl NetStackControlPlane {
    fn new(shard_count: usize, rss_key: [u8; 40], hash_seed: &[u8; 16]) -> Self {
        Self {
            bind_registry: BindRegistry::new(shard_count, hash_seed),
            bindings: Mutex::new(BTreeMap::new()),
            listeners: Mutex::new(BTreeSet::new()),
            next_listener: AtomicU64::new(1),
            rss_key,
            shard_capacity: shard_count.max(1),
            active_shards: AtomicUsize::new(shard_count.max(1)),
            dad: Mutex::new(Vec::new()),
            dad_errors: Mutex::new(BTreeMap::new()),
            dhcp: Mutex::new(Vec::new()),
            multicast_refs: Mutex::new(BTreeMap::new()),
            multicast_bindings: Mutex::new(BTreeMap::new()),
        }
    }

    fn configure_active_shards(&self, count: usize) -> bool {
        if count == 0
            || count > self.shard_capacity
            || !self.bindings.lock().is_empty()
            || !self.listeners.lock().is_empty()
        {
            return false;
        }
        self.active_shards.store(count, Ordering::Release);
        true
    }

    fn reserve_binding(
        &self,
        socket: SocketId,
        request: BindRequest,
        shard: ShardId,
    ) -> Result<BindToken, BindError> {
        let token = if request.port == 0 {
            self.bind_registry.reserve_ephemeral(request, shard)?
        } else {
            self.bind_registry.reserve(request)?
        };
        if let Some(previous) = self.bindings.lock().insert(socket, token) {
            let _ = self.bind_registry.release(previous);
        }
        Ok(token)
    }

    fn release_binding(&self, socket: SocketId) -> bool {
        self.bindings
            .lock()
            .remove(&socket)
            .is_some_and(|token| self.bind_registry.release(token).is_ok())
    }

    fn flow_shard(
        &self,
        remote: Endpoint,
        local: Endpoint,
        protocol: TransportProtocol,
    ) -> ShardId {
        let key = FlowKey::new(remote, local, protocol).expect("协议 flow 端点必须属于同一地址族");
        let shard_count = self.active_shards.load(Ordering::Acquire);
        ShardId((crate::flow::rss_hash(&self.rss_key, &key) as usize % shard_count) as u16)
    }

    fn neighbor_owner(&self, key: NeighborKey) -> ShardId {
        let mut hash = u64::from(key.interface.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let bytes: &[u8] = match &key.address {
            IpAddr::V4(address) => &address.0,
            IpAddr::V6(address) => &address.0,
        };
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
        }
        let shard_count = self.active_shards.load(Ordering::Acquire);
        ShardId((hash as usize % shard_count) as u16)
    }

    fn initialize_autoconfig(&self, config: &ConfigSnapshot, now_ns: u64) {
        *self.dad.lock() = initial_dad_states(config, now_ns);
        self.dad_errors.lock().clear();
        *self.dhcp.lock() = initial_dhcp_clients(config, now_ns);
    }

    fn run_dad(&self, now_ns: u64) -> DadRunOutput {
        let mut dad = self.dad.lock();
        let mut probes = Vec::new();
        for state in dad.iter_mut().filter(|state| !state.probe_sent) {
            probes.push(NeighborKey {
                interface: state.interface,
                address: IpAddr::V6(state.address),
            });
            state.probe_sent = true;
        }
        let mut ready = Vec::new();
        let mut conflicts = Vec::new();
        let mut index = 0;
        while index < dad.len() {
            if dad[index].deadline_ns > now_ns {
                index += 1;
                continue;
            }
            let state = dad.swap_remove(index);
            if state.conflict {
                conflicts.push(state.interface);
            } else {
                ready.push((state.interface, state.address));
            }
        }
        let next_deadline_ns = dad.iter().map(|state| state.deadline_ns).min();
        drop(dad);
        let mut errors = self.dad_errors.lock();
        for interface in conflicts {
            errors.insert(interface, SocketError::AddressInUse);
        }
        DadRunOutput {
            probes,
            ready,
            next_deadline_ns,
        }
    }

    fn observe_dad_conflict(&self, interface: InterfaceId, address: Ipv6Addr) {
        for state in self.dad.lock().iter_mut() {
            if state.interface == interface && state.address == address {
                state.conflict = true;
            }
        }
    }

    fn run_dhcp(&self, config: &ConfigSnapshot, now_ns: u64) -> DhcpRunOutput {
        let mut dhcp = self.dhcp.lock();
        dhcp.retain(|client| {
            let configured = config.addresses.iter().find_map(|entry| {
                (entry.interface == client.interface)
                    .then_some(entry.address)
                    .and_then(|address| match address {
                        IpAddr::V4(address) => Some(address),
                        IpAddr::V6(_) => None,
                    })
            });
            match (&client.installed, configured) {
                (Some(lease), Some(address)) => lease.address == address,
                (None, None) => true,
                _ => false,
            }
        });
        let mut frames = Vec::new();
        let mut changes = Vec::new();
        for index in 0..dhcp.len() {
            let expired = matches!(
                &dhcp[index].phase,
                DhcpPhase::Bound { expires_ns, .. } if *expires_ns <= now_ns
            );
            if expired {
                let interface = dhcp[index].interface;
                let old = dhcp[index].installed.take();
                dhcp[index].phase = DhcpPhase::Discovering;
                dhcp[index].next_action_ns = now_ns;
                dhcp[index].retry_seconds = 1;
                changes.push((interface, old, None));
            }
            if dhcp[index].next_action_ns > now_ns {
                continue;
            }
            let frame = match &dhcp[index].phase {
                DhcpPhase::Discovering => build_dhcp_frame(&dhcp[index], 1, None, None),
                DhcpPhase::Requesting { lease, server } => {
                    build_dhcp_frame(&dhcp[index], 3, Some(lease.address), Some(*server))
                }
                DhcpPhase::Bound {
                    lease,
                    server,
                    renew_ns,
                    rebind_ns,
                    ..
                } if *renew_ns <= now_ns => build_dhcp_frame(
                    &dhcp[index],
                    3,
                    Some(lease.address),
                    (*rebind_ns > now_ns).then_some(*server),
                ),
                DhcpPhase::Bound { renew_ns, .. } => {
                    dhcp[index].next_action_ns = *renew_ns;
                    continue;
                }
            };
            frames.push((dhcp[index].interface, frame));
            let retry = dhcp[index].retry_seconds.clamp(1, 64);
            dhcp[index].next_action_ns =
                now_ns.saturating_add(u64::from(retry).saturating_mul(1_000_000_000));
            dhcp[index].retry_seconds = retry.saturating_mul(2).min(64);
        }
        let retained_dns = installed_dns(&dhcp);
        let lease_changes = changes
            .into_iter()
            .map(|(interface, old, new)| DhcpLeaseChange {
                interface,
                old,
                new,
                retained_dns: retained_dns.clone(),
            })
            .collect();
        DhcpRunOutput {
            frames,
            lease_changes,
            next_deadline_ns: dhcp.iter().map(|client| client.next_action_ns).min(),
        }
    }

    fn handle_dhcp_packet(
        &self,
        interface: InterfaceId,
        packet: &FrontendPacket,
        now_ns: u64,
    ) -> DhcpPacketOutput {
        let Some(reply) = parse_dhcp_reply(packet) else {
            return DhcpPacketOutput {
                handled: false,
                lease_change: None,
            };
        };
        let mut dhcp = self.dhcp.lock();
        let Some(index) = dhcp.iter().position(|client| {
            client.interface == interface
                && client.transaction_id == reply.transaction_id
                && client.mac_address == reply.client_mac
        }) else {
            return DhcpPacketOutput {
                handled: false,
                lease_change: None,
            };
        };
        let mut change = None;
        match reply.message_type {
            2 => {
                let Some(server) = reply.server else {
                    return DhcpPacketOutput {
                        handled: true,
                        lease_change: None,
                    };
                };
                let prefix_len = reply.subnet_mask.and_then(ipv4_mask_prefix).unwrap_or(24);
                dhcp[index].phase = DhcpPhase::Requesting {
                    lease: DhcpLease {
                        address: reply.offered,
                        prefix_len,
                        router: reply.router,
                        dns: reply.dns,
                        lease_seconds: reply.lease_seconds.unwrap_or(3600).max(60),
                    },
                    server,
                };
                dhcp[index].next_action_ns = now_ns;
                dhcp[index].retry_seconds = 1;
            }
            5 => {
                let (requested, previous_server) = match &dhcp[index].phase {
                    DhcpPhase::Requesting { lease, server }
                    | DhcpPhase::Bound { lease, server, .. } => {
                        (Some(lease.clone()), Some(*server))
                    }
                    DhcpPhase::Discovering => (None, None),
                };
                let address = if reply.offered == Ipv4Addr::UNSPECIFIED {
                    requested
                        .as_ref()
                        .map(|lease| lease.address)
                        .unwrap_or(reply.offered)
                } else {
                    reply.offered
                };
                if address == Ipv4Addr::UNSPECIFIED {
                    return DhcpPacketOutput {
                        handled: true,
                        lease_change: None,
                    };
                }
                let lease = DhcpLease {
                    address,
                    prefix_len: reply
                        .subnet_mask
                        .and_then(ipv4_mask_prefix)
                        .or_else(|| requested.as_ref().map(|lease| lease.prefix_len))
                        .unwrap_or(24),
                    router: reply
                        .router
                        .or_else(|| requested.as_ref().and_then(|lease| lease.router)),
                    dns: if reply.dns.is_empty() {
                        requested
                            .as_ref()
                            .map(|lease| lease.dns.clone())
                            .unwrap_or_default()
                    } else {
                        reply.dns
                    },
                    lease_seconds: reply
                        .lease_seconds
                        .or_else(|| requested.as_ref().map(|lease| lease.lease_seconds))
                        .unwrap_or(3600)
                        .max(60),
                };
                let server = reply
                    .server
                    .or(previous_server)
                    .unwrap_or(Ipv4Addr::UNSPECIFIED);
                let old = dhcp[index].installed.clone();
                let renew_seconds = reply
                    .renewal_seconds
                    .unwrap_or(lease.lease_seconds / 2)
                    .clamp(1, lease.lease_seconds.saturating_sub(1));
                let renew_ns =
                    now_ns.saturating_add(u64::from(renew_seconds).saturating_mul(1_000_000_000));
                let rebind_seconds = dhcp_rebind_seconds(
                    lease.lease_seconds,
                    renew_seconds,
                    reply.rebinding_seconds,
                );
                let rebind_ns =
                    now_ns.saturating_add(u64::from(rebind_seconds).saturating_mul(1_000_000_000));
                let expires_ns = now_ns
                    .saturating_add(u64::from(lease.lease_seconds).saturating_mul(1_000_000_000));
                dhcp[index].installed = Some(lease.clone());
                dhcp[index].phase = DhcpPhase::Bound {
                    lease: lease.clone(),
                    server,
                    renew_ns,
                    rebind_ns,
                    expires_ns,
                };
                dhcp[index].next_action_ns = renew_ns;
                dhcp[index].retry_seconds = 1;
                change = Some((old, Some(lease)));
            }
            6 => {
                let old = dhcp[index].installed.take();
                dhcp[index].phase = DhcpPhase::Discovering;
                dhcp[index].next_action_ns = now_ns;
                dhcp[index].retry_seconds = 1;
                change = Some((old, None));
            }
            _ => {}
        }
        let retained_dns = installed_dns(&dhcp);
        DhcpPacketOutput {
            handled: true,
            lease_change: change.map(|(old, new)| DhcpLeaseChange {
                interface,
                old,
                new,
                retained_dns,
            }),
        }
    }

    fn remove_autoconfig_interface(&self, interface: InterfaceId) -> Option<DhcpLeaseChange> {
        self.dad.lock().retain(|state| state.interface != interface);
        self.dad_errors.lock().remove(&interface);
        let mut dhcp = self.dhcp.lock();
        let index = dhcp
            .iter()
            .position(|client| client.interface == interface)?;
        let client = dhcp.remove(index);
        let retained_dns = installed_dns(&dhcp);
        client.installed.map(|old| DhcpLeaseChange {
            interface,
            old: Some(old),
            new: None,
            retained_dns,
        })
    }

    fn join_multicast(
        &self,
        socket: SocketId,
        membership: MulticastMembership,
        interface: InterfaceId,
    ) -> Option<bool> {
        let key = (socket, membership);
        if self.multicast_bindings.lock().contains_key(&key) {
            return None;
        }
        self.multicast_bindings.lock().insert(key, interface);
        let mut refs = self.multicast_refs.lock();
        let count = refs.entry((interface, membership.group)).or_default();
        *count += 1;
        Some(*count == 1)
    }

    fn leave_multicast(
        &self,
        socket: SocketId,
        membership: MulticastMembership,
    ) -> Option<(InterfaceId, bool)> {
        let interface = self
            .multicast_bindings
            .lock()
            .remove(&(socket, membership))?;
        let mut refs = self.multicast_refs.lock();
        let key = (interface, membership.group);
        let count = refs.get_mut(&key)?;
        *count = count.saturating_sub(1);
        let last = *count == 0;
        if last {
            refs.remove(&key);
        }
        Some((interface, last))
    }

    fn remove_socket_multicast(&self, socket: SocketId) -> Vec<(InterfaceId, IpAddr)> {
        let memberships = self
            .multicast_bindings
            .lock()
            .keys()
            .filter_map(|(candidate, membership)| (*candidate == socket).then_some(*membership))
            .collect::<Vec<_>>();
        let mut removed = Vec::new();
        for membership in memberships {
            if let Some((interface, true)) = self.leave_multicast(socket, membership) {
                removed.push((interface, membership.group));
            }
        }
        removed
    }
}

fn initial_dad_states(config: &ConfigSnapshot, now_ns: u64) -> Vec<DadState> {
    config
        .interfaces
        .iter()
        .filter(|interface| {
            !interface.loopback && interface.running && interface.mac_address != [0; 6]
        })
        .map(|interface| {
            let mac = interface.mac_address;
            DadState {
                interface: interface.id,
                address: Ipv6Addr([
                    0xfe,
                    0x80,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    mac[0] ^ 0x02,
                    mac[1],
                    mac[2],
                    0xff,
                    0xfe,
                    mac[3],
                    mac[4],
                    mac[5],
                ]),
                probe_sent: false,
                conflict: false,
                deadline_ns: now_ns.saturating_add(1_000_000_000),
            }
        })
        .collect()
}

fn initial_dhcp_clients(config: &ConfigSnapshot, now_ns: u64) -> Vec<DhcpClient> {
    config
        .interfaces
        .iter()
        .filter(|interface| {
            !interface.loopback
                && interface.running
                && interface.mac_address != [0; 6]
                && !config.addresses.iter().any(|entry| {
                    entry.interface == interface.id && matches!(entry.address, IpAddr::V4(_))
                })
        })
        .map(|interface| {
            let mut transaction_id = interface.id.0.wrapping_mul(0x9e37_79b9);
            for byte in interface.mac_address {
                transaction_id = transaction_id.rotate_left(5) ^ u32::from(byte);
            }
            DhcpClient {
                interface: interface.id,
                mac_address: interface.mac_address,
                transaction_id: transaction_id.max(1),
                phase: DhcpPhase::Discovering,
                next_action_ns: now_ns,
                retry_seconds: 1,
                installed: None,
            }
        })
        .collect()
}

fn installed_dns(clients: &[DhcpClient]) -> Vec<Ipv4Addr> {
    let mut dns = Vec::new();
    for server in clients
        .iter()
        .filter_map(|client| client.installed.as_ref())
        .flat_map(|lease| lease.dns.iter().copied())
    {
        if !dns.contains(&server) {
            dns.push(server);
        }
    }
    dns
}

fn build_dhcp_frame(
    client: &DhcpClient,
    message_type: u8,
    requested: Option<Ipv4Addr>,
    server: Option<Ipv4Addr>,
) -> Vec<u8> {
    let mut payload = alloc::vec![0; 300];
    payload[0] = 1;
    payload[1] = 1;
    payload[2] = 6;
    payload[4..8].copy_from_slice(&client.transaction_id.to_be_bytes());
    payload[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
    if matches!(&client.phase, DhcpPhase::Bound { .. })
        && let Some(address) = requested
    {
        payload[12..16].copy_from_slice(&address.0);
    }
    payload[28..34].copy_from_slice(&client.mac_address);
    payload[236..240].copy_from_slice(&[99, 130, 83, 99]);
    let mut offset = 240;
    payload[offset..offset + 3].copy_from_slice(&[53, 1, message_type]);
    offset += 3;
    payload[offset..offset + 9].copy_from_slice(&[
        61,
        7,
        1,
        client.mac_address[0],
        client.mac_address[1],
        client.mac_address[2],
        client.mac_address[3],
        client.mac_address[4],
        client.mac_address[5],
    ]);
    offset += 9;
    if let Some(address) = requested {
        payload[offset..offset + 6].copy_from_slice(&[
            50,
            4,
            address.0[0],
            address.0[1],
            address.0[2],
            address.0[3],
        ]);
        offset += 6;
    }
    if let Some(server) = server {
        payload[offset..offset + 6].copy_from_slice(&[
            54,
            4,
            server.0[0],
            server.0[1],
            server.0[2],
            server.0[3],
        ]);
        offset += 6;
    }
    payload[offset..offset + 8].copy_from_slice(&[55, 6, 1, 3, 6, 51, 58, 59]);
    offset += 8;
    payload[offset] = 255;
    payload.truncate(offset + 1);

    let udp_len = 8 + payload.len();
    let mut frame = alloc::vec![0; 14 + 20 + udp_len];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&client.mac_address);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[14] = 0x45;
    frame[16..18].copy_from_slice(&((20 + udp_len) as u16).to_be_bytes());
    frame[18..20].copy_from_slice(&(client.transaction_id as u16).to_be_bytes());
    frame[20..22].copy_from_slice(&0x4000u16.to_be_bytes());
    frame[22] = 64;
    frame[23] = 17;
    frame[30..34].fill(0xff);
    let checksum = crate::pipeline::checksum_bytes(&frame[14..34]);
    frame[24..26].copy_from_slice(&checksum.to_be_bytes());
    frame[34..36].copy_from_slice(&68u16.to_be_bytes());
    frame[36..38].copy_from_slice(&67u16.to_be_bytes());
    frame[38..40].copy_from_slice(&(udp_len as u16).to_be_bytes());
    frame[42..].copy_from_slice(&payload);
    frame
}

fn parse_dhcp_reply(packet: &FrontendPacket) -> Option<DhcpReply> {
    let udp = packet.parsed.udp?;
    if udp.source_port != 67 || udp.destination_port != 68 || udp.payload_len < 240 {
        return None;
    }
    let mut payload = alloc::vec![0; usize::from(udp.payload_len)];
    packet
        .chain
        .copy_out(usize::from(udp.payload_offset), &mut payload)
        .ok()?;
    if payload[0] != 2
        || payload[1] != 1
        || payload[2] != 6
        || payload[236..240] != [99, 130, 83, 99]
    {
        return None;
    }
    let mut reply = DhcpReply {
        message_type: 0,
        transaction_id: u32::from_be_bytes(payload[4..8].try_into().ok()?),
        client_mac: payload[28..34].try_into().ok()?,
        offered: Ipv4Addr(payload[16..20].try_into().ok()?),
        server: None,
        subnet_mask: None,
        router: None,
        dns: Vec::new(),
        lease_seconds: None,
        renewal_seconds: None,
        rebinding_seconds: None,
    };
    let mut offset = 240usize;
    while offset < payload.len() {
        let kind = payload[offset];
        offset += 1;
        if kind == 0 {
            continue;
        }
        if kind == 255 {
            break;
        }
        let len = usize::from(*payload.get(offset)?);
        offset += 1;
        let value = payload.get(offset..offset.checked_add(len)?)?;
        match (kind, len) {
            (53, 1) => reply.message_type = value[0],
            (54, 4) => reply.server = Some(Ipv4Addr(value.try_into().ok()?)),
            (1, 4) => reply.subnet_mask = Some(Ipv4Addr(value.try_into().ok()?)),
            (3, len) if len >= 4 => reply.router = Some(Ipv4Addr(value[..4].try_into().ok()?)),
            (6, len) if len >= 4 => {
                reply.dns.extend(
                    value
                        .chunks_exact(4)
                        .take(4)
                        .map(|entry| Ipv4Addr(entry.try_into().unwrap())),
                );
            }
            (51, 4) => reply.lease_seconds = Some(u32::from_be_bytes(value.try_into().ok()?)),
            (58, 4) => reply.renewal_seconds = Some(u32::from_be_bytes(value.try_into().ok()?)),
            (59, 4) => reply.rebinding_seconds = Some(u32::from_be_bytes(value.try_into().ok()?)),
            _ => {}
        }
        offset += len;
    }
    (reply.message_type != 0).then_some(reply)
}

fn ipv4_mask_prefix(mask: Ipv4Addr) -> Option<u8> {
    let value = mask.as_u32();
    let prefix = value.leading_ones() as u8;
    let expected = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (value == expected).then_some(prefix)
}

pub fn dhcp_rebind_seconds(lease_seconds: u32, renew_seconds: u32, offered: Option<u32>) -> u32 {
    let latest = lease_seconds.saturating_sub(1);
    let earliest = renew_seconds.saturating_add(1).min(latest);
    offered
        .unwrap_or(lease_seconds.saturating_mul(7) / 8)
        .clamp(earliest, latest)
}

#[kernel_symbols::export(
    name = "net.stack.create_control_plane",
    contract = "kernel.net.stack-control-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn create_control_plane(
    shard_count: usize,
    rss_key: [u8; 40],
    hash_seed: &[u8; 16],
) -> NetStackControlPlane {
    NetStackControlPlane::new(shard_count, rss_key, hash_seed)
}

#[kernel_symbols::export(
    name = "net.stack.destroy_control_plane",
    contract = "kernel.net.stack-control-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn destroy_control_plane(plane: NetStackControlPlane) {
    drop(plane);
}

#[kernel_symbols::export(
    name = "net.stack.dispatch_control_plane_call",
    contract = "kernel.net.stack-control-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn dispatch_control_plane_call(
    plane: &NetStackControlPlane,
    command: &mut NetStackControlCommand,
) {
    match command {
        NetStackControlCommand::ConfigureActiveShards { count, output } => {
            *output = Some(plane.configure_active_shards(usize::from(*count)));
        }
        NetStackControlCommand::InitializeAutoconfig {
            config,
            now_ns,
            output,
        } => {
            if config.is_null() || !config.is_aligned() {
                return;
            }
            // Safety: config 只在同步 control-call 期间借用。
            let config = unsafe { &**config };
            plane.initialize_autoconfig(config, *now_ns);
            *output = Some(true);
        }
        NetStackControlCommand::RunDad { now_ns, output } => {
            *output = Some(plane.run_dad(*now_ns));
        }
        NetStackControlCommand::ObserveDadConflict { interface, address } => {
            plane.observe_dad_conflict(*interface, *address);
        }
        NetStackControlCommand::RunDhcp {
            config,
            now_ns,
            output,
        } => {
            if config.is_null() || !config.is_aligned() {
                return;
            }
            // Safety: config 只在同步 control-call 期间借用。
            let config = unsafe { &**config };
            *output = Some(plane.run_dhcp(config, *now_ns));
        }
        NetStackControlCommand::HandleDhcpPacket {
            interface,
            packet,
            now_ns,
            output,
        } => {
            let Some(packet) = packet.as_ref() else {
                return;
            };
            *output = Some(plane.handle_dhcp_packet(*interface, packet, *now_ns));
        }
        NetStackControlCommand::RemoveAutoconfigInterface { interface, output } => {
            *output = Some(plane.remove_autoconfig_interface(*interface));
        }
        NetStackControlCommand::ReserveBinding {
            socket,
            request,
            shard,
            output,
        } => *output = Some(plane.reserve_binding(*socket, *request, *shard)),
        NetStackControlCommand::ReleaseBinding { socket, output } => {
            *output = Some(plane.release_binding(*socket));
        }
        NetStackControlCommand::AllocateListener { output } => {
            let id = plane.next_listener.fetch_add(1, Ordering::Relaxed);
            assert!(id != 0, "ListenGroupId 已耗尽");
            *output = Some(ListenGroupId(id));
        }
        NetStackControlCommand::InstallListener { group, output } => {
            *output = Some(plane.listeners.lock().insert(*group));
        }
        NetStackControlCommand::RemoveListener { group, output } => {
            *output = Some(plane.listeners.lock().remove(group));
        }
        NetStackControlCommand::FlowShard {
            remote,
            local,
            protocol,
            output,
        } => *output = Some(plane.flow_shard(*remote, *local, *protocol)),
        NetStackControlCommand::NeighborOwner { key, output } => {
            *output = Some(plane.neighbor_owner(*key));
        }
        NetStackControlCommand::JoinMulticast {
            socket,
            membership,
            interface,
            output,
        } => *output = Some(plane.join_multicast(*socket, *membership, *interface)),
        NetStackControlCommand::LeaveMulticast {
            socket,
            membership,
            output,
        } => *output = Some(plane.leave_multicast(*socket, *membership)),
        NetStackControlCommand::MulticastGroups { interface, output } => {
            *output = Some(
                plane
                    .multicast_refs
                    .lock()
                    .keys()
                    .filter_map(|(candidate, group)| (*candidate == *interface).then_some(*group))
                    .collect(),
            );
        }
        NetStackControlCommand::RemoveInterfaceMulticast { interface } => {
            plane
                .multicast_bindings
                .lock()
                .retain(|_, bound| *bound != *interface);
            plane
                .multicast_refs
                .lock()
                .retain(|(bound, _), _| *bound != *interface);
        }
        NetStackControlCommand::RemoveSocketMulticast { socket, output } => {
            *output = Some(plane.remove_socket_multicast(*socket));
        }
    }
}

/// 单次 `FlowShard` 调用使用的固定容量命令批次。
pub struct NetStackCommandBatch<T> {
    slots: Box<[Option<T>]>,
    len: u16,
}

impl<T> NetStackCommandBatch<T> {
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(NET_STACK_SHARD_TURN_COMMAND_CAPACITY);
        slots.resize_with(NET_STACK_SHARD_TURN_COMMAND_CAPACITY, || None);
        Self {
            slots: slots.into_boxed_slice(),
            len: 0,
        }
    }

    pub fn try_from_vec(mut values: Vec<T>) -> Result<Self, Vec<T>> {
        if values.len() > NET_STACK_SHARD_TURN_COMMAND_CAPACITY {
            return Err(values);
        }
        let mut batch = Self::new();
        for value in values.drain(..) {
            batch.push(value).unwrap_or_else(|_| unreachable!());
        }
        Ok(batch)
    }

    pub fn move_from_vec(&mut self, values: &mut Vec<T>) -> Result<(), ()> {
        if !self.is_empty() || values.len() > self.slots.len() {
            return Err(());
        }
        for value in values.drain(..) {
            self.push(value).unwrap_or_else(|_| unreachable!());
        }
        Ok(())
    }

    pub fn drain_into_vec(&mut self, values: &mut Vec<T>) {
        values.reserve(self.len());
        for index in 0..self.len() {
            values.push(
                self.slots[index]
                    .take()
                    .expect("command batch slot 必须存在"),
            );
        }
        self.len = 0;
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, value: T) -> Result<(), T> {
        let index = self.len();
        if index == self.slots.len() {
            return Err(value);
        }
        self.slots[index] = Some(value);
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        let index = self.len().checked_sub(1)?;
        self.len -= 1;
        self.slots[index].take()
    }

    pub fn clear(&mut self) {
        let len = self.len();
        for slot in self.slots.iter_mut().take(len) {
            *slot = None;
        }
        self.len = 0;
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        (index < self.len())
            .then(|| self.slots[index].as_mut())
            .flatten()
    }

    pub fn slots(&self) -> &[Option<T>] {
        &self.slots
    }

    pub fn into_vec(mut self) -> Vec<T> {
        let mut values = Vec::with_capacity(self.len());
        for index in 0..self.len() {
            values.push(
                self.slots[index]
                    .take()
                    .expect("command batch slot 必须存在"),
            );
        }
        values
    }

    fn valid(&self) -> bool {
        self.slots.len() == NET_STACK_SHARD_TURN_COMMAND_CAPACITY
            && self.len() <= self.slots.len()
            && self.slots[..self.len()].iter().all(Option::is_some)
    }
}

impl<T> Default for NetStackCommandBatch<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
pub struct NetStackShardTurn {
    pub struct_size: u32,
    pub generation: u64,
    pub shard: ShardId,
    pub committed: u8,
    pub reserved1: [u8; 5],
    pub control_commands: NetStackCommandBatch<NetStackControlCommand>,
    pub commands: NetStackCommandBatch<NetStackFlowCommand>,
    pub tx_plans: TxPlanBatch,
}

#[kernel_symbols::export]
impl NetStackShardTurn {
    pub fn new(generation: u64, shard: ShardId, command: NetStackFlowCommand) -> Self {
        let mut commands = NetStackCommandBatch::new();
        commands.push(command).unwrap_or_else(|_| unreachable!());
        Self::batch(generation, shard, NetStackCommandBatch::new(), commands)
    }

    pub fn control(generation: u64, command: NetStackControlCommand) -> Self {
        let mut control_commands = NetStackCommandBatch::new();
        control_commands
            .push(command)
            .unwrap_or_else(|_| unreachable!());
        Self::batch(
            generation,
            ShardId(0),
            control_commands,
            NetStackCommandBatch::new(),
        )
    }

    pub fn batch(
        generation: u64,
        shard: ShardId,
        control_commands: NetStackCommandBatch<NetStackControlCommand>,
        commands: NetStackCommandBatch<NetStackFlowCommand>,
    ) -> Self {
        Self::batch_with_output(
            generation,
            shard,
            control_commands,
            commands,
            TxPlanBatch::new(),
        )
    }

    pub fn batch_with_output(
        generation: u64,
        shard: ShardId,
        control_commands: NetStackCommandBatch<NetStackControlCommand>,
        commands: NetStackCommandBatch<NetStackFlowCommand>,
        tx_plans: TxPlanBatch,
    ) -> Self {
        assert!(tx_plans.is_empty(), "shard turn output scratch 必须为空");
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            generation,
            shard,
            committed: 0,
            reserved1: [0; 5],
            control_commands,
            commands,
            tx_plans,
        }
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackShardTurn.valid_header",
        contract = "kernel.net.stack-flow-state@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn valid_header(&self, generation: u64) -> bool {
        self.valid(generation, 0)
    }

    pub fn valid_committed(&self, generation: u64) -> bool {
        self.valid(generation, 1)
    }

    fn valid(&self, generation: u64, committed: u8) -> bool {
        self.struct_size as usize == core::mem::size_of::<Self>()
            && self.generation == generation
            && self.committed == committed
            && self.reserved1 == [0; 5]
            && self.control_commands.valid()
            && self.commands.valid()
            && self.tx_plans.valid()
            && (!self.control_commands.is_empty() || !self.commands.is_empty())
            && self
                .control_commands
                .len()
                .saturating_add(self.commands.len())
                <= NET_STACK_SHARD_TURN_COMMAND_CAPACITY
            && (self.control_commands.is_empty() || self.shard == ShardId(0))
    }
}

#[kernel_symbols::export(
    name = "net.stack.create_flow_shard",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn create_flow_shard(id: ShardId, boot: NetStackBootConfig, now_ns: u64) -> FlowShard {
    let mut generation_bytes = [0; 4];
    generation_bytes.copy_from_slice(&boot.generation_nonce()[..4]);
    FlowShard::new(
        id,
        *boot.rss_key(),
        u32::from_le_bytes(generation_bytes).max(1),
        *boot.hash_seed(),
        *boot.tcp_isn_key(),
        now_ns,
    )
}

#[kernel_symbols::export(
    name = "net.stack.destroy_flow_shard",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn destroy_flow_shard(shard: FlowShard) {
    drop(shard);
}

fn process_local_tcp_work(
    shard: &mut FlowShard,
    interface: InterfaceId,
    work: &PreparedTcpTx,
    config: &ConfigSnapshot,
    now_ns: u64,
) -> Result<FlowId, TcpIngressError> {
    let source = Endpoint {
        addr: work.path.route.source,
        port: work.local_port,
    };
    let Some(key) = FlowKey::new(source, work.remote, TransportProtocol::Tcp) else {
        return Err(TcpIngressError::Malformed);
    };
    let path = TcpPath {
        route: crate::control::RouteDecision {
            interface,
            source: work.remote.addr,
            next_hop: source.addr,
            mtu: work.path.route.mtu,
            table: work.path.route.table,
        },
        source_mac: work.path.destination_mac,
        destination_mac: work.path.source_mac,
        unresolved_neighbor: None,
        config_generation: config.generation,
    };
    let packet = TcpPacket {
        source_port: work.local_port,
        destination_port: work.remote.port,
        sequence: work.sequence,
        acknowledgement: work.acknowledgement,
        flags: work.flags,
        window: work.window,
        urgent_pointer: 0,
        header_len: 20 + u16::from(work.options_len),
        payload_offset: 0,
        payload_len: work
            .payload
            .as_ref()
            .map_or(0, |payload| u32::from(payload.len)),
        options: work.parsed_options,
    };
    shard.process_local_tcp(interface, path, key, packet, work.payload.as_ref(), now_ns)
}

#[kernel_symbols::export(
    name = "net.stack.dispatch_flow_shard_turn",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn dispatch_flow_shard_turn(shard: &mut FlowShard, call: &mut NetStackShardTurn) -> bool {
    for index in 0..call.commands.len() {
        let command = call
            .commands
            .get_mut(index)
            .expect("flow command batch 索引有效");
        if !dispatch_flow_shard_command(shard, command) {
            return false;
        }
    }
    call.committed = 1;
    true
}

#[kernel_symbols::export(
    name = "net.stack.dispatch_flow_shard_command",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn dispatch_flow_shard_command(
    shard: &mut FlowShard,
    command: &mut NetStackFlowCommand,
) -> bool {
    match command {
        NetStackFlowCommand::ParsePacketBatch { output, .. } => {
            return output.is_some();
        }
        NetStackFlowCommand::Stats { output } => *output = Some(shard.stats()),
        NetStackFlowCommand::RunDueTimers { now_ns } => shard.run_due_timers(*now_ns),
        NetStackFlowCommand::NextTimerDeadline { output } => {
            *output = Some(shard.next_timer_deadline_ns());
        }
        NetStackFlowCommand::BindUdp {
            local,
            peer,
            interface,
            output,
        } => *output = Some(shard.bind_udp(*local, *peer, *interface)),
        NetStackFlowCommand::BindUdpFacade {
            local,
            peer,
            interface,
            facade,
            free_bind,
            accepts_ipv4,
            output,
        } => {
            *output = Some(shard.bind_udp_facade(
                *local,
                *peer,
                *interface,
                Arc::clone(facade),
                *free_bind,
                *accepts_ipv4,
            ));
        }
        NetStackFlowCommand::ReconnectUdpFacade {
            flow,
            local,
            peer,
            facade,
            output,
        } => {
            *output = Some(shard.reconnect_udp_facade(*flow, *local, *peer, Arc::clone(facade)));
        }
        NetStackFlowCommand::CloseUdp { flow } => shard.close_udp(*flow),
        NetStackFlowCommand::BindRawFacade {
            local,
            interface,
            facade,
            free_bind,
            output,
        } => {
            *output =
                Some(shard.bind_raw_facade(*local, *interface, Arc::clone(facade), *free_bind));
        }
        NetStackFlowCommand::CloseRaw { flow } => shard.close_raw(*flow),
        NetStackFlowCommand::ListenTcp {
            local,
            interface,
            group,
            output,
        } => {
            *output = Some(shard.listen_tcp(*local, *interface, Arc::clone(group)));
        }
        NetStackFlowCommand::ConnectTcp {
            local,
            remote,
            path,
            facade,
            control_sequence,
            now_ns,
            output,
        } => {
            *output = Some(shard.connect_tcp(
                *local,
                *remote,
                *path,
                Arc::clone(facade),
                *control_sequence,
                *now_ns,
            ));
        }
        NetStackFlowCommand::CloseTcp { flow, now_ns } => shard.close_tcp(*flow, *now_ns),
        NetStackFlowCommand::AbortTcp { flow, now_ns } => shard.abort_tcp(*flow, *now_ns),
        NetStackFlowCommand::ShutdownTcpWrite { flow, now_ns } => {
            shard.shutdown_tcp_write(*flow, *now_ns);
        }
        NetStackFlowCommand::CloseTcpListener { group, output } => {
            *output = Some(shard.close_tcp_listener(*group));
        }
        NetStackFlowCommand::DrainTcpSend { flow, now_ns } => {
            shard.drain_tcp_send(*flow, *now_ns);
        }
        NetStackFlowCommand::TakeTcpOutputBatch {
            output,
            inline_pool_installs,
            needs_resume,
            limit,
            resume_budget,
            inline_local_tcp,
            config,
            now_ns,
        } => {
            let (Some(batch), Some(pool_installs)) =
                (output.as_mut(), inline_pool_installs.as_mut())
            else {
                return false;
            };
            if !batch.is_empty()
                || !pool_installs.is_empty()
                || needs_resume.is_some()
                || *limit == 0
                || *limit > 256
                || *resume_budget > 256
                || config.is_null()
                || !config.is_aligned()
            {
                return false;
            }
            // Safety: config 只在同步 shard-turn 期间借用。
            let config = unsafe { &**config };
            let mut remaining_resume = usize::from(*resume_budget);
            let mut processed = 0usize;
            while batch.len() < usize::from(*limit) && processed < usize::from(*limit) {
                if let Some(mut work) = shard.take_tcp_output() {
                    processed += 1;
                    if let Err(error) = shard.refresh_tcp_tx_path(&mut work, config, *now_ns) {
                        work.facade.set_pending_error(error);
                    } else if *inline_local_tcp
                        && config.interfaces.iter().any(|interface| {
                            interface.id == work.path.route.interface && interface.loopback
                        })
                    {
                        let passive_open = work.flags.contains(TcpFlags::SYN)
                            && !work.flags.contains(TcpFlags::ACK);
                        match process_local_tcp_work(
                            shard,
                            work.path.route.interface,
                            &work,
                            config,
                            *now_ns,
                        ) {
                            Ok(flow) if passive_open => {
                                if let Some(facade) = shard.tcp_facade(flow) {
                                    pool_installs.push((facade, work.path.route.interface));
                                }
                            }
                            Ok(_) => {}
                            Err(TcpIngressError::NoEndpoint) => batch.push(work),
                            Err(_) => {}
                        }
                    } else {
                        batch.push(work);
                    }
                    continue;
                }
                if remaining_resume == 0 {
                    break;
                }
                let resumed = shard.resume_tcp_output(*now_ns, remaining_resume.min(32));
                if resumed == 0 {
                    break;
                }
                remaining_resume = remaining_resume.saturating_sub(resumed);
            }
            let pending = shard.has_blocked_tcp_output();
            let exhausted = processed == usize::from(*limit)
                || batch.len() == usize::from(*limit)
                || (remaining_resume == 0 && pending);
            *needs_resume = Some(exhausted && pending);
        }
        NetStackFlowCommand::ResolveTcpPath {
            destination,
            bound_source,
            interface,
            config,
            now_ns,
            free_bind,
            output,
        } => {
            if config.is_null() || !config.is_aligned() {
                return false;
            }
            // Safety: config 只在同步 state-call 期间借用。
            let config = unsafe { &**config };
            *output = Some(shard.resolve_tcp_path(
                *destination,
                *bound_source,
                *interface,
                config,
                *now_ns,
                *free_bind,
            ));
        }
        NetStackFlowCommand::ProcessLocalTcpWork {
            interface,
            work,
            config,
            now_ns,
            output,
        } => {
            let Some(work) = work.as_ref() else {
                return false;
            };
            if config.is_null() || !config.is_aligned() || output.is_some() {
                return false;
            }
            // Safety: config 只在同步 shard-turn 期间借用。
            let config = unsafe { &**config };
            *output = Some(process_local_tcp_work(
                shard, *interface, work, config, *now_ns,
            ));
        }
        NetStackFlowCommand::ProcessLocalUdpWork {
            interface,
            work,
            now_ns,
            output,
        } => {
            let Some(work) = work.as_ref() else {
                return false;
            };
            if output.is_some() {
                return false;
            }
            let source = Endpoint {
                addr: work.route.source,
                port: work.source_port,
            };
            *output = Some(shard.process_local_udp(
                *interface,
                source,
                work.destination,
                &work.payload,
                work.hop_limit,
                work.traffic_class,
                *now_ns,
            ));
        }
        NetStackFlowCommand::PlanTxWork { work } => {
            if work.is_none() {
                return false;
            }
        }
        NetStackFlowCommand::InvalidateInterface { interface, output } => {
            *output = Some(shard.invalidate_interface(*interface));
        }
        NetStackFlowCommand::EnqueueNeighbor {
            work,
            now_ns,
            interface_limit,
            output,
        } => {
            let Some(owned) = work.take() else {
                return false;
            };
            if *interface_limit == 0
                || usize::from(*interface_limit) > MAX_PENDING_NEIGHBOR_PACKETS_PER_INTERFACE
            {
                *work = Some(owned);
                return false;
            }
            *output = Some(shard.enqueue_neighbor(owned, *now_ns, usize::from(*interface_limit)));
        }
        NetStackFlowCommand::ObserveAndResolveNeighbor {
            key,
            mac_address,
            now_ns,
            output,
        } => {
            *output = Some(shard.observe_and_resolve_neighbor(*key, *mac_address, *now_ns));
        }
        NetStackFlowCommand::FailInterfaceNeighbors { interface, output } => {
            *output = Some(shard.fail_interface_neighbors(*interface));
        }
        NetStackFlowCommand::RunNeighborTimers { now_ns, output } => {
            *output = Some(shard.run_neighbor_timers(*now_ns));
        }
        NetStackFlowCommand::PrepareUdpTx {
            flow,
            payload,
            mark,
            config,
            now_ns,
            output,
        } => {
            let Some(owned) = payload.take() else {
                return false;
            };
            if config.is_null() || !config.is_aligned() {
                *payload = Some(owned);
                return false;
            }
            // Safety: config 只在同步 state-call 期间借用。
            let config = unsafe { &**config };
            *output = Some(
                shard
                    .prepare_udp_tx(*flow, owned, *mark, config, *now_ns)
                    .map(Some),
            );
        }
        NetStackFlowCommand::PrepareRawTx {
            flow,
            payload,
            mark,
            config,
            now_ns,
            output,
        } => {
            let Some(owned) = payload.take() else {
                return false;
            };
            if config.is_null() || !config.is_aligned() {
                *payload = Some(owned);
                return false;
            }
            // Safety: config 只在同步 state-call 期间借用。
            let config = unsafe { &**config };
            *output = Some(
                shard
                    .prepare_raw_tx(*flow, owned, *mark, config, *now_ns)
                    .map(Some),
            );
        }
        NetStackFlowCommand::FormUdpPacket {
            flow,
            destination,
            payload,
            mark,
            config,
            now_ns,
            output,
        } => {
            let Some(owned) = payload.take() else {
                return false;
            };
            if config.is_null() || !config.is_aligned() {
                *payload = Some(owned);
                return false;
            }
            // Safety: config 只在同步 state-call 期间借用。
            let config = unsafe { &**config };
            *output =
                Some(shard.form_udp_packet(*flow, *destination, owned, *mark, config, *now_ns));
        }
        NetStackFlowCommand::RecvUdp { flow, output } => {
            *output = Some(shard.recv_udp(*flow));
        }
        NetStackFlowCommand::ProcessFrontendBatch {
            packets,
            interface,
            local_mac,
            config,
            now_ns,
            output,
            drop_counts,
            stats,
        } => {
            let Some(packets) = packets.as_mut() else {
                return false;
            };
            if config.is_null() || !config.is_aligned() || output.is_some() {
                return false;
            }
            // Safety: config 只在同步 state-call 期间借用。
            let config = unsafe { &**config };
            let mut tx = TxBatch::new();
            let mut recycle = PacketBatch::new();
            shard.push_frontend_batch(packets);
            shard.process_frontend_batch(
                crate::FlowTurnContext {
                    interface: *interface,
                    local_mac: *local_mac,
                    config,
                    now_ns: *now_ns,
                },
                &mut tx,
                &mut recycle,
                |reason| {
                    drop_counts[reason.index()] = drop_counts[reason.index()].saturating_add(1);
                },
            );
            *output = Some((tx, recycle));
            *stats = Some(shard.stats());
        }
        NetStackFlowCommand::DrainReassembly { .. } => return false,
        NetStackFlowCommand::ApplyTransportError {
            interface,
            target,
            error,
            now_ns,
            output,
        } => {
            *output = Some(shard.apply_transport_error(*interface, *target, *error, *now_ns));
        }
    }
    true
}

#[kernel_symbols::export(
    name = "net.stack.finalize_shard_turn_tx",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn finalize_shard_turn_tx(
    shard: &mut FlowShard,
    commands: &mut NetStackCommandBatch<NetStackFlowCommand>,
    tx_plans: &mut TxPlanBatch,
) -> bool {
    for index in 0..commands.len() {
        let command = commands
            .get_mut(index)
            .expect("flow command batch 索引有效");
        match command {
            NetStackFlowCommand::TakeTcpOutputBatch {
                output: Some(output),
                ..
            } => {
                let mut deferred = Vec::new();
                for work in core::mem::take(output) {
                    if work.path.unresolved_neighbor.is_some() {
                        deferred.push(work);
                        continue;
                    }
                    match append_tx_plans(shard, PendingNeighborTx::Tcp(work), tx_plans) {
                        TxPlanAppendResult::Appended => {}
                        TxPlanAppendResult::Deferred(PendingNeighborTx::Tcp(work)) => {
                            deferred.push(work)
                        }
                        TxPlanAppendResult::Deferred(_) => return false,
                    }
                }
                *output = deferred;
            }
            NetStackFlowCommand::PrepareUdpTx { config, output, .. } => {
                let Some(result) = output.take() else {
                    return false;
                };
                *output = Some(match result {
                    Ok(Some(work)) => {
                        let loopback = if config.is_null() || !config.is_aligned() {
                            return false;
                        } else {
                            // Safety: config 位于同步 shard-turn 登记的 host 地址范围内。
                            unsafe { &**config }.interfaces.iter().any(|interface| {
                                interface.id == work.route.interface && interface.loopback
                            })
                        };
                        if loopback || work.unresolved_neighbor.is_some() {
                            Ok(Some(work))
                        } else {
                            match append_tx_plans(shard, PendingNeighborTx::Udp(work), tx_plans) {
                                TxPlanAppendResult::Appended => Ok(None),
                                TxPlanAppendResult::Deferred(PendingNeighborTx::Udp(work)) => {
                                    Ok(Some(work))
                                }
                                TxPlanAppendResult::Deferred(_) => return false,
                            }
                        }
                    }
                    other => other,
                });
            }
            NetStackFlowCommand::PrepareRawTx { output, .. } => {
                let Some(result) = output.take() else {
                    return false;
                };
                *output = Some(match result {
                    Ok(Some(work)) if work.unresolved_neighbor.is_none() => {
                        match append_tx_plans(shard, PendingNeighborTx::Raw(work), tx_plans) {
                            TxPlanAppendResult::Appended => Ok(None),
                            TxPlanAppendResult::Deferred(PendingNeighborTx::Raw(work)) => {
                                Ok(Some(work))
                            }
                            TxPlanAppendResult::Deferred(_) => return false,
                        }
                    }
                    other => other,
                });
            }
            NetStackFlowCommand::PlanTxWork { work } => {
                let Some(owned) = work.take() else {
                    return false;
                };
                if let TxPlanAppendResult::Deferred(owned) = append_tx_plans(shard, owned, tx_plans)
                {
                    *work = Some(owned);
                }
            }
            NetStackFlowCommand::EnqueueNeighbor {
                output: Some(output),
                ..
            } => {
                if matches!(output, NeighborEnqueueOutput::Resolved(_)) {
                    let NeighborEnqueueOutput::Resolved(work) =
                        core::mem::replace(output, NeighborEnqueueOutput::Queued)
                    else {
                        unreachable!()
                    };
                    if let TxPlanAppendResult::Deferred(work) =
                        append_tx_plans(shard, work, tx_plans)
                    {
                        *output = NeighborEnqueueOutput::Resolved(work);
                    }
                }
            }
            NetStackFlowCommand::ObserveAndResolveNeighbor {
                output: Some(output),
                ..
            } => {
                let mut deferred = Vec::new();
                for work in core::mem::take(output) {
                    if let TxPlanAppendResult::Deferred(work) =
                        append_tx_plans(shard, work, tx_plans)
                    {
                        deferred.push(work);
                    }
                }
                *output = deferred;
            }
            _ => {}
        }
    }
    true
}

#[kernel_symbols::export(
    name = "net.stack.parse_frontend_packet_batch",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn parse_frontend_packet_batch(
    input: &mut PacketBatch,
    ethernet: &[NetStackEthernet],
    network: &[NetStackNetwork],
    transport: &[NetStackTransport],
) -> Option<crate::pipeline::FrontendBatch> {
    if input.len() != ethernet.len()
        || input.len() != network.len()
        || input.len() != transport.len()
    {
        return None;
    }
    let mut output = crate::pipeline::FrontendBatch::new();
    crate::pipeline::VectorFrontend::new([0; 40], 1).process_with_stack_sidecars(
        input,
        ethernet,
        network,
        transport,
        &mut output,
    );
    Some(output)
}

#[kernel_symbols::export(
    name = "net.stack.flow_shard_take_reassembled_input",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn flow_shard_take_reassembled_input(shard: &mut FlowShard) -> Option<PacketBatch> {
    shard.take_reassembled_input()
}

#[kernel_symbols::export(
    name = "net.stack.flow_shard_parse_reassembled_batch",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn flow_shard_parse_reassembled_batch(
    shard: &mut FlowShard,
    input: PacketBatch,
    ethernet: &[NetStackEthernet],
    network: &[NetStackNetwork],
    transport: &[NetStackTransport],
) -> Result<(), PacketBatch> {
    shard.parse_reassembled_batch(input, ethernet, network, transport)
}

#[kernel_symbols::export(
    name = "net.stack.flow_shard_take_reassembled",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn flow_shard_take_reassembled(shard: &mut FlowShard) -> Option<FrontendPacket> {
    shard.take_reassembled()
}

#[kernel_symbols::export(
    name = "net.stack.flow_shard_take_forwarded_error",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn flow_shard_take_forwarded_error(
    shard: &mut FlowShard,
) -> Option<(InterfaceId, ControlErrorTarget, TransportControlError, u64)> {
    shard.take_forwarded_error()
}

/// 动态 `net.stack` 的代际固定 export 描述。
pub struct PinnedNetStackShardTurnEndpoint {
    owner_cell: u64,
    owner_generation: u64,
    export_name: Box<str>,
    export_contract: Box<str>,
    export_version: u32,
}

#[kernel_symbols::export]
impl PinnedNetStackShardTurnEndpoint {
    #[kernel_symbols::export(
        name = "net.stack.PinnedNetStackShardTurnEndpoint.current",
        contract = "kernel.net.stack-shard-turn-endpoint@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn current(export_name: &str, export_contract: &str, export_version: u32) -> Option<Self> {
        let context = elm_model::current_context()?;
        if export_name.is_empty()
            || export_contract.is_empty()
            || export_version == 0
            || elm_model::FlowContract::new(export_contract).is_err()
        {
            return None;
        }
        Some(Self {
            owner_cell: context.cell_id.0,
            owner_generation: context.generation.0,
            export_name: export_name.into(),
            export_contract: export_contract.into(),
            export_version,
        })
    }

    pub const fn owner_cell(&self) -> u64 {
        self.owner_cell
    }

    pub const fn owner_generation(&self) -> u64 {
        self.owner_generation
    }

    pub fn export_name(&self) -> &str {
        &self.export_name
    }

    pub fn export_contract(&self) -> &str {
        &self.export_contract
    }

    pub const fn export_version(&self) -> u32 {
        self.export_version
    }
}

pub type IntegratedNetStackShardTurn = fn(&mut NetStackShardTurn) -> i32;

pub enum NetStackEndpoint {
    Integrated(IntegratedNetStackShardTurn),
    Pinned(PinnedNetStackShardTurnEndpoint),
}

/// 一个 stack generation 的原子注册单元。
pub struct NetStackRegistration {
    handle: NetStackHandle,
    endpoint: NetStackEndpoint,
}

#[kernel_symbols::export]
impl NetStackRegistration {
    pub fn integrated(call: IntegratedNetStackShardTurn) -> Option<Self> {
        if elm_model::current_context().is_some() || call as usize == 0 {
            return None;
        }
        Some(Self {
            handle: next_stack_handle(),
            endpoint: NetStackEndpoint::Integrated(call),
        })
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackRegistration.pinned",
        contract = "kernel.net.stack-registration@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn pinned(endpoint: PinnedNetStackShardTurnEndpoint) -> Option<Self> {
        Some(Self {
            handle: next_stack_handle(),
            endpoint: NetStackEndpoint::Pinned(endpoint),
        })
    }

    pub const fn handle(&self) -> NetStackHandle {
        self.handle
    }

    pub fn owner_cell(&self) -> u64 {
        match &self.endpoint {
            NetStackEndpoint::Integrated(_) => 0,
            NetStackEndpoint::Pinned(endpoint) => endpoint.owner_cell(),
        }
    }

    pub fn generation(&self) -> u64 {
        match &self.endpoint {
            NetStackEndpoint::Integrated(_) => 1,
            NetStackEndpoint::Pinned(endpoint) => endpoint.owner_generation(),
        }
    }

    pub fn endpoint(&self) -> &NetStackEndpoint {
        &self.endpoint
    }

    fn valid_for_current_context(&self) -> bool {
        match (&self.endpoint, elm_model::current_context()) {
            (NetStackEndpoint::Integrated(_), None) => true,
            (NetStackEndpoint::Pinned(endpoint), Some(context)) => {
                endpoint.owner_cell() == context.cell_id.0
                    && endpoint.owner_generation() == context.generation.0
            }
            _ => false,
        }
    }
}

fn next_stack_handle() -> NetStackHandle {
    let raw = NEXT_STACK_HANDLE.fetch_add(1, Ordering::Relaxed);
    assert!(raw != 0, "NetStackHandle 已耗尽");
    NetStackHandle(raw)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NetStackHandle(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NetStackState {
    Absent = 0,
    Active = 1,
    Quiescing = 2,
    Draining = 3,
    Faulted = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStackSnapshot {
    pub state: NetStackState,
    pub handle: Option<NetStackHandle>,
    pub owner_cell: u64,
    pub generation: u64,
    pub ready: bool,
}

impl NetStackSnapshot {
    pub const fn absent() -> Self {
        Self {
            state: NetStackState::Absent,
            handle: None,
            owner_cell: 0,
            generation: 0,
            ready: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackRegisterErrorKind {
    RegistrarNotReady,
    AlreadyActive,
    InvalidRegistration,
    ResourceExhausted,
}

pub struct NetStackRegisterError {
    pub kind: NetStackRegisterErrorKind,
    pub registration: NetStackRegistration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackRemoveError {
    NoStack,
    OwnerMismatch,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackTxError {
    InvalidInput,
}

pub trait NetStackRegistrar: Send + Sync {
    fn register_stack(
        &self,
        registration: NetStackRegistration,
    ) -> Result<NetStackHandle, NetStackRegisterError>;

    fn begin_remove(
        &self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRemoveError>;

    fn snapshot(&self) -> NetStackSnapshot;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallNetStackRuntimeError {
    AlreadyInstalled,
}

/// 在任何 `net.stack` 初始化前一次性安装常驻 broker。
pub fn install_stack_runtime(
    config: NetStackBootConfig,
    registrar: &'static dyn NetStackRegistrar,
) -> Result<(), InstallNetStackRuntimeError> {
    let mut config_slot = STACK_BOOT_CONFIG.lock();
    let mut slot = STACK_REGISTRAR.lock();
    if config_slot.is_some() || slot.is_some() {
        return Err(InstallNetStackRuntimeError::AlreadyInstalled);
    }
    *config_slot = Some(config);
    *slot = Some(registrar);
    Ok(())
}

#[kernel_symbols::export(
    name = "net.stack.boot_config",
    contract = "kernel.net.stack-boot-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK
)]
pub fn boot_config() -> Option<NetStackBootConfig> {
    *STACK_BOOT_CONFIG.lock()
}

#[kernel_symbols::export(
    name = "net.stack.register_stack",
    contract = "kernel.net.stack-registration@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn register_stack(
    registration: NetStackRegistration,
) -> Result<NetStackHandle, NetStackRegisterError> {
    if !registration.valid_for_current_context() {
        return Err(NetStackRegisterError {
            kind: NetStackRegisterErrorKind::InvalidRegistration,
            registration,
        });
    }
    let registrar = *STACK_REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetStackRegisterError {
            kind: NetStackRegisterErrorKind::RegistrarNotReady,
            registration,
        });
    };
    registrar.register_stack(registration)
}

#[kernel_symbols::export(
    name = "net.stack.begin_remove",
    contract = "kernel.net.stack-registration@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn begin_remove(handle: NetStackHandle) -> Result<(), NetStackRemoveError> {
    let registrar = *STACK_REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetStackRemoveError::NoStack);
    };
    let (owner_cell, generation) = elm_model::current_context()
        .map(|context| (context.cell_id.0, context.generation.0))
        .unwrap_or((0, 1));
    registrar.begin_remove(handle, owner_cell, generation)
}

pub fn stack_snapshot() -> NetStackSnapshot {
    let registrar = *STACK_REGISTRAR.lock();
    registrar
        .map(NetStackRegistrar::snapshot)
        .unwrap_or_else(NetStackSnapshot::absent)
}

/// 由常驻 broker 使用的严格生命周期状态机。
pub struct NetStackLifecycle {
    snapshot: NetStackSnapshot,
}

impl NetStackLifecycle {
    pub const fn new() -> Self {
        Self {
            snapshot: NetStackSnapshot::absent(),
        }
    }

    pub const fn snapshot(&self) -> NetStackSnapshot {
        self.snapshot
    }

    pub fn activate(
        &mut self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRegisterErrorKind> {
        if self.snapshot.state != NetStackState::Absent {
            return Err(NetStackRegisterErrorKind::AlreadyActive);
        }
        if handle.0 == 0 || generation == 0 {
            return Err(NetStackRegisterErrorKind::InvalidRegistration);
        }
        self.snapshot = NetStackSnapshot {
            state: NetStackState::Active,
            handle: Some(handle),
            owner_cell,
            generation,
            ready: false,
        };
        Ok(())
    }

    pub fn mark_ready(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle) || self.snapshot.state != NetStackState::Active {
            return false;
        }
        self.snapshot.ready = true;
        true
    }

    pub fn mark_faulted(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle)
            || !matches!(
                self.snapshot.state,
                NetStackState::Active | NetStackState::Faulted
            )
        {
            return false;
        }
        self.snapshot.state = NetStackState::Faulted;
        self.snapshot.ready = false;
        true
    }

    pub fn begin_remove(
        &mut self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRemoveError> {
        if self.snapshot.state == NetStackState::Absent {
            return Err(NetStackRemoveError::NoStack);
        }
        if self.snapshot.handle != Some(handle)
            || self.snapshot.owner_cell != owner_cell
            || self.snapshot.generation != generation
        {
            return Err(NetStackRemoveError::OwnerMismatch);
        }
        if !matches!(
            self.snapshot.state,
            NetStackState::Active | NetStackState::Faulted
        ) {
            return Err(NetStackRemoveError::Busy);
        }
        self.snapshot.state = NetStackState::Quiescing;
        Ok(())
    }

    pub fn begin_drain(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle) || self.snapshot.state != NetStackState::Quiescing {
            return false;
        }
        self.snapshot.state = NetStackState::Draining;
        true
    }

    pub fn finish_remove(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle) || self.snapshot.state != NetStackState::Draining {
            return false;
        }
        self.snapshot = NetStackSnapshot::absent();
        true
    }
}

impl Default for NetStackLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::{PacketChain, PacketMetadata};

    #[test]
    fn tcp_tx_plan_contains_complete_valid_headers() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter: 1,
            },
            crate::AddressFamily::Ipv4,
            crate::SocketKind::Stream,
        ));
        let source = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let destination = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let plan = build_tcp_tx_plan(PreparedTcpTx {
            facade,
            payload: None,
            path: TcpPath {
                route: crate::control::RouteDecision {
                    interface: InterfaceId(1),
                    source,
                    next_hop: destination,
                    mtu: 65_535,
                    table: 0,
                },
                source_mac: [0x02, 0, 0, 0, 0, 1],
                destination_mac: [0x02, 0, 0, 0, 0, 2],
                unresolved_neighbor: None,
                config_generation: 1,
            },
            remote: Endpoint {
                addr: destination,
                port: 19_004,
            },
            local_port: 40_000,
            sequence: crate::transport::TcpSequence(10),
            acknowledgement: crate::transport::TcpSequence(20),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: 32_768,
            options: [0; 40],
            options_len: 0,
            parsed_options: crate::transport::TcpOptions::default(),
            completion: 7,
            low_latency: true,
        })
        .unwrap();
        assert_eq!(plan.header_len, 54);
        assert_eq!(crate::pipeline::checksum_bytes(&plan.header[14..34]), 0);
        let packet = PacketChain::from_owned(plan.header_bytes().to_vec());
        assert_eq!(
            crate::pipeline::transport_checksum(&packet, 34, 20, source, destination, 6),
            Ok(0)
        );
    }

    #[test]
    fn control_plane_uses_negotiated_active_shard_count() {
        let plane = NetStackControlPlane::new(4, [7; 40], &[11; 16]);
        assert!(plane.configure_active_shards(1));
        for port in 1..=64 {
            assert_eq!(
                plane.flow_shard(
                    Endpoint {
                        addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                        port,
                    },
                    Endpoint {
                        addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                        port: port + 1024,
                    },
                    TransportProtocol::Tcp,
                ),
                ShardId(0)
            );
        }
        assert!(!plane.configure_active_shards(5));
    }

    #[test]
    fn frontend_shard_turn_returns_empty_allocation_and_output() {
        let mut shard = FlowShard::new(ShardId(0), [1; 40], 1, [2; 16], [3; 16], 0);
        let packets = Vec::with_capacity(32);
        let allocation = packets.as_ptr();
        let config = ConfigSnapshot::empty();
        let mut call = NetStackShardTurn::new(
            7,
            ShardId(0),
            NetStackFlowCommand::ProcessFrontendBatch {
                packets: Some(packets),
                interface: InterfaceId(1),
                local_mac: [0; 6],
                config: &config,
                now_ns: 0,
                output: None,
                drop_counts: [0; DropReason::COUNT],
                stats: None,
            },
        );

        assert!(dispatch_flow_shard_turn(&mut shard, &mut call));
        let NetStackFlowCommand::ProcessFrontendBatch {
            packets: Some(packets),
            output: Some((tx, recycle)),
            ..
        } = call.commands.pop().expect("single shard-turn command")
        else {
            panic!("frontend batch allocation 未归还");
        };
        assert!(packets.is_empty());
        assert_eq!(packets.capacity(), 32);
        assert_eq!(packets.as_ptr(), allocation);
        assert!(tx.is_empty());
        assert!(recycle.is_empty());
    }

    #[test]
    fn empty_tcp_output_batch_reuses_host_allocations_without_forcing_resume() {
        let mut shard = FlowShard::new(ShardId(0), [1; 40], 1, [2; 16], [3; 16], 0);
        let output = Vec::with_capacity(32);
        let output_allocation = output.as_ptr();
        let pool_installs = Vec::with_capacity(4);
        let pool_install_allocation = pool_installs.as_ptr();
        let config = ConfigSnapshot::empty();
        let mut command = NetStackFlowCommand::TakeTcpOutputBatch {
            output: Some(output),
            inline_pool_installs: Some(pool_installs),
            needs_resume: None,
            limit: 256,
            resume_budget: 256,
            inline_local_tcp: true,
            config: &config,
            now_ns: 0,
        };

        assert!(dispatch_flow_shard_command(&mut shard, &mut command));
        let NetStackFlowCommand::TakeTcpOutputBatch {
            output: Some(output),
            inline_pool_installs: Some(pool_installs),
            needs_resume: Some(needs_resume),
            ..
        } = command
        else {
            panic!("TCP output batch 未归还 host allocation");
        };
        assert!(output.is_empty());
        assert_eq!(output.as_ptr(), output_allocation);
        assert!(pool_installs.is_empty());
        assert_eq!(pool_installs.as_ptr(), pool_install_allocation);
        assert!(!needs_resume);
    }

    #[test]
    fn inline_loopback_handshake_returns_passive_child_for_pool_install() {
        let interface = InterfaceId(1);
        let local_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let listener_endpoint = Endpoint {
            addr: local_addr,
            port: 9000,
        };
        let client_endpoint = Endpoint {
            addr: local_addr,
            port: 40_000,
        };
        let listener = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter: 1,
            },
            crate::AddressFamily::Ipv4,
            crate::SocketKind::Stream,
        ));
        let group = ListenGroup::new(ListenGroupId(1), &listener, 1, 8);
        listener.install_listen_group(Arc::clone(&group));
        let client = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter: 2,
            },
            crate::AddressFamily::Ipv4,
            crate::SocketKind::Stream,
        ));
        let config = ConfigSnapshot::new(
            1,
            alloc::vec![crate::control::InterfaceSnapshot {
                id: interface,
                device: crate::NetDeviceId(1),
                mac_address: [0; 6],
                mtu: 65_535,
                running: true,
                loopback: true,
            }],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let path = crate::transport::TcpPath {
            route: crate::control::RouteDecision {
                interface,
                source: client_endpoint.addr,
                next_hop: listener_endpoint.addr,
                mtu: 65_535,
                table: 0,
            },
            source_mac: [0; 6],
            destination_mac: [0; 6],
            unresolved_neighbor: None,
            config_generation: config.generation,
        };
        let mut shard = FlowShard::new(ShardId(0), [1; 40], 1, [2; 16], [3; 16], 0);
        shard
            .listen_tcp(listener_endpoint, Some(interface), group)
            .unwrap();
        shard
            .connect_tcp(
                client_endpoint,
                listener_endpoint,
                path,
                Arc::clone(&client),
                1,
                1_000,
            )
            .unwrap();
        let mut command = NetStackFlowCommand::TakeTcpOutputBatch {
            output: Some(Vec::with_capacity(32)),
            inline_pool_installs: Some(Vec::with_capacity(4)),
            needs_resume: None,
            limit: 256,
            resume_budget: 256,
            inline_local_tcp: true,
            config: &config,
            now_ns: 2_000,
        };

        assert!(dispatch_flow_shard_command(&mut shard, &mut command));
        let NetStackFlowCommand::TakeTcpOutputBatch {
            output: Some(output),
            inline_pool_installs: Some(pool_installs),
            needs_resume: Some(needs_resume),
            ..
        } = command
        else {
            panic!("inline TCP output batch 未归还结果");
        };
        assert!(output.is_empty());
        assert_eq!(pool_installs.len(), 1);
        assert_eq!(pool_installs[0].1, interface);
        assert!(!Arc::ptr_eq(&pool_installs[0].0, &listener));
        assert!(!needs_resume);
        assert!(listener.readiness().0.contains(crate::Readiness::READABLE));
        assert!(client.readiness().0.contains(crate::Readiness::WRITABLE));
    }

    #[test]
    fn control_plane_owns_binding_routing_and_multicast_lifecycles() {
        let plane = NetStackControlPlane::new(2, [7; 40], &[11; 16]);
        let socket_a = SocketId {
            boot_nonce: 1,
            counter: 1,
        };
        let socket_b = SocketId {
            boot_nonce: 1,
            counter: 2,
        };
        let address = IpAddr::V4(crate::Ipv4Addr::new(10, 0, 2, 15));
        let request = BindRequest {
            owner: socket_a.counter,
            family: crate::AddressFamily::Ipv4,
            protocol: TransportProtocol::Udp,
            address: crate::control::BindAddress::Specified(address),
            port: 9000,
            interface: Some(InterfaceId(1)),
            options: crate::control::BindOptions::default(),
        };
        assert_eq!(
            plane.reserve_binding(socket_a, request, ShardId(0)),
            Ok(BindToken { id: 1, port: 9000 })
        );
        assert_eq!(
            plane.reserve_binding(
                socket_b,
                BindRequest {
                    owner: socket_b.counter,
                    ..request
                },
                ShardId(1),
            ),
            Err(BindError::AddressInUse)
        );
        assert!(plane.release_binding(socket_a));
        assert!(
            plane
                .reserve_binding(
                    socket_b,
                    BindRequest {
                        owner: socket_b.counter,
                        ..request
                    },
                    ShardId(1),
                )
                .is_ok()
        );

        let neighbor = NeighborKey {
            interface: InterfaceId(1),
            address,
        };
        assert_eq!(plane.neighbor_owner(neighbor).0 < 2, true);

        let membership = MulticastMembership {
            group: IpAddr::V4(crate::Ipv4Addr::new(239, 1, 2, 3)),
            interface: Some(InterfaceId(1)),
        };
        assert_eq!(
            plane.join_multicast(socket_a, membership, InterfaceId(1)),
            Some(true)
        );
        assert_eq!(
            plane.join_multicast(socket_b, membership, InterfaceId(1)),
            Some(false)
        );
        assert_eq!(
            plane.leave_multicast(socket_a, membership),
            Some((InterfaceId(1), false))
        );
        assert_eq!(
            plane.remove_socket_multicast(socket_b),
            alloc::vec![(InterfaceId(1), membership.group)]
        );
    }

    #[test]
    fn control_plane_owns_dad_and_dhcp_timers() {
        let config = ConfigSnapshot::new(
            1,
            alloc::vec![crate::control::InterfaceSnapshot {
                id: InterfaceId(1),
                device: crate::NetDeviceId(1),
                mac_address: [0x02, 0, 0, 0, 0, 1],
                mtu: 1500,
                running: true,
                loopback: false,
            }],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let plane = NetStackControlPlane::new(1, [3; 40], &[5; 16]);
        plane.initialize_autoconfig(&config, 100);

        let first_dad = plane.run_dad(100);
        assert_eq!(first_dad.probes.len(), 1);
        assert!(first_dad.ready.is_empty());
        let address = first_dad.probes[0].address;
        let IpAddr::V6(address) = address else {
            unreachable!();
        };
        plane.observe_dad_conflict(InterfaceId(1), address);
        let conflict = plane.run_dad(1_100_000_100);
        assert!(conflict.ready.is_empty());

        plane.initialize_autoconfig(&config, 200);
        let discover = plane.run_dhcp(&config, 200);
        assert_eq!(discover.frames.len(), 1);
        assert_eq!(discover.frames[0].0, InterfaceId(1));
        assert_eq!(discover.frames[0].1[34..38], [0, 68, 0, 67]);
        assert!(plane.run_dhcp(&config, 200).frames.is_empty());

        let (transaction_id, client_mac) = {
            let dhcp = plane.dhcp.lock();
            (dhcp[0].transaction_id, dhcp[0].mac_address)
        };
        let reply = |message_type: u8| {
            let mut payload = alloc::vec![0; 256];
            payload[0] = 2;
            payload[1] = 1;
            payload[2] = 6;
            payload[4..8].copy_from_slice(&transaction_id.to_be_bytes());
            payload[16..20].copy_from_slice(&[10, 0, 2, 20]);
            payload[28..34].copy_from_slice(&client_mac);
            payload[236..240].copy_from_slice(&[99, 130, 83, 99]);
            payload[240..243].copy_from_slice(&[53, 1, message_type]);
            payload[243..249].copy_from_slice(&[54, 4, 10, 0, 2, 2]);
            payload[249..255].copy_from_slice(&[51, 4, 0, 0, 0, 60]);
            payload[255] = 255;
            FrontendPacket {
                chain: PacketChain::from_owned(payload),
                metadata: PacketMetadata::default(),
                parsed: crate::pipeline::ParsedPacket {
                    ethernet: crate::pipeline::EthernetHeader {
                        destination: client_mac,
                        source: [1; 6],
                        ethertype: 0x0800,
                    },
                    ip: None,
                    tcp: None,
                    udp: Some(crate::pipeline::UdpPacket {
                        source_port: 67,
                        destination_port: 68,
                        payload_offset: 0,
                        payload_len: 256,
                    }),
                    flow: None,
                    rss_hash: None,
                    disposition: crate::pipeline::FrontendDisposition::Udp,
                },
            }
        };
        let offer = plane.handle_dhcp_packet(InterfaceId(1), &reply(2), 300);
        assert!(offer.handled);
        assert!(offer.lease_change.is_none());
        assert_eq!(plane.run_dhcp(&config, 300).frames.len(), 1);
        let ack = plane.handle_dhcp_packet(InterfaceId(1), &reply(5), 400);
        let lease = ack.lease_change.unwrap().new.unwrap();
        assert_eq!(lease.address, Ipv4Addr::new(10, 0, 2, 20));
        assert_eq!(lease.lease_seconds, 60);
        let configured = ConfigSnapshot::new(
            2,
            config.interfaces.clone(),
            alloc::vec![crate::control::AddressEntry {
                interface: InterfaceId(1),
                address: IpAddr::V4(lease.address),
                prefix_len: lease.prefix_len,
                primary: true,
            }],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            plane.run_dhcp(&configured, 400).next_deadline_ns,
            Some(30_000_000_400)
        );
        assert_eq!(dhcp_rebind_seconds(800, 400, Some(1)), 401);
    }

    #[test]
    fn shard_turn_rejects_stale_generation_and_reserved_bits() {
        let mut turn =
            NetStackShardTurn::new(7, ShardId(0), NetStackFlowCommand::Stats { output: None });
        assert!(turn.valid_header(7));
        assert!(!turn.valid_header(8));
        turn.reserved1[0] = 1;
        assert!(!turn.valid_header(7));
    }

    #[test]
    fn packet_parse_requires_complete_committed_prefix() {
        let mut input = PacketBatch::new();
        input
            .push(
                PacketChain::from_owned(alloc::vec![0; 14]),
                PacketMetadata {
                    rss_hash: Some(0x1234_5678),
                    rss_generation: 11,
                    ..PacketMetadata::default()
                },
            )
            .unwrap_or_else(|_| unreachable!());
        input
            .push(
                PacketChain::from_owned(alloc::vec![0; 14]),
                PacketMetadata::default(),
            )
            .unwrap_or_else(|_| unreachable!());
        let addresses = [NetStackLocalAddress {
            interface: 1,
            family: NET_STACK_ADDRESS_FAMILY_IPV4,
            prefix_len: 24,
            reserved0: [0; 2],
            address: [10, 0, 2, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            reserved1: [0; 8],
        }];
        let rss_key = [3; 40];
        let input_pointer = &input as *const PacketBatch;
        let mut turn = NetStackPacketParse::new(7, 9, 1, &addresses, rss_key, 11, &input);
        assert!(turn.valid_header(7, 9, 1, input_pointer, addresses.as_ptr(), 1, &rss_key, 11,));
        assert!(!turn.valid_header(8, 9, 1, input_pointer, addresses.as_ptr(), 1, &rss_key, 11,));
        assert!(!turn.fully_committed(&input));

        turn.ethernet[0].status = NET_STACK_ETHERNET_TRUNCATED;
        turn.network[0] = NetStackNetwork::skipped();
        turn.transport[0] = NetStackTransport::skipped();
        turn.committed = 1;
        assert!(!turn.fully_committed(&input));
        turn.ethernet[1].status = NET_STACK_ETHERNET_TRUNCATED;
        turn.network[1] = NetStackNetwork::skipped();
        turn.transport[1] = NetStackTransport::skipped();
        turn.committed = 2;
        assert!(turn.fully_committed(&input));
        turn.packet_inputs[0].frame_len += 1;
        assert!(!turn.fully_committed(&input));
        turn.packet_inputs[0].frame_len -= 1;
        turn.packet_inputs[1].checksums_validated = 1;
        assert!(!turn.fully_committed(&input));
        turn.packet_inputs[1].checksums_validated = 0;
        assert!(turn.fully_committed(&input));
        turn.packet_inputs[0].rss_hash ^= 1;
        assert!(!turn.fully_committed(&input));
        turn.packet_inputs[0].rss_hash ^= 1;
        turn.packet_inputs[0].rss_generation += 1;
        assert!(!turn.fully_committed(&input));
        turn.packet_inputs[0].rss_generation -= 1;
        turn.packet_inputs[1].rss_hash_present = 1;
        assert!(!turn.fully_committed(&input));
        turn.packet_inputs[1].rss_hash_present = 0;
        assert!(turn.fully_committed(&input));
        turn.ethernet[2].reserved[0] = 1;
        assert!(!turn.fully_committed(&input));
        assert_eq!(input.len(), 2, "sidecar 提交不得移动 packet ownership");
    }

    #[test]
    fn transport_sidecar_rejects_noncanonical_lengths_and_tcp_options() {
        let mut network = NetStackNetwork {
            outcome: NET_STACK_NETWORK_IP,
            family: NET_STACK_ADDRESS_FAMILY_IPV4,
            next_header: 6,
            flags: 0,
            drop_reason: 0,
            traffic_class: 0,
            hop_limit: 64,
            reserved0: 0,
            header_len: 20,
            payload_offset: 34,
            fragment_offset: 0,
            arp_operation: 0,
            payload_len: 40,
            fragment_identification: 0,
            problem_pointer: 0,
            source: [0; 16],
            destination: [0; 16],
            arp_sender_mac: [0; 6],
            arp_target_mac: [0; 6],
            reserved1: [0; 8],
        };
        let mut tcp = NetStackTransport {
            outcome: NET_STACK_TRANSPORT_TCP,
            protocol: 6,
            source_port: 1000,
            destination_port: 9000,
            header_len: 24,
            payload_offset: 58,
            tcp_flags: 0x002,
            payload_len: 16,
            rss_hash: 123,
            tcp_options: NetStackTcpOptions {
                flags: NET_STACK_TCP_OPTION_MSS,
                maximum_segment_size: 1460,
                ..NetStackTcpOptions::empty()
            },
            ..NetStackTransport::empty()
        };
        assert!(tcp.valid(74, &network));

        tcp.header_len = 22;
        assert!(!tcp.valid(74, &network));
        tcp.header_len = 24;
        tcp.tcp_options.maximum_segment_size = 0;
        assert!(!tcp.valid(74, &network));
        tcp.tcp_options.maximum_segment_size = 1460;
        tcp.tcp_options.sack_left[1] = 1;
        assert!(!tcp.valid(74, &network));
        tcp.tcp_options.sack_left[1] = 0;

        tcp.tcp_options = NetStackTcpOptions {
            flags: NET_STACK_TCP_OPTION_MSS
                | NET_STACK_TCP_OPTION_WINDOW_SCALE
                | NET_STACK_TCP_OPTION_SACK_PERMITTED
                | NET_STACK_TCP_OPTION_TIMESTAMP,
            window_scale: 7,
            sack_count: 4,
            maximum_segment_size: 1460,
            sack_left: [1, 2, 3, 4],
            sack_right: [2, 3, 4, 5],
            timestamp_value: 9,
            timestamp_echo_reply: 3,
            ..NetStackTcpOptions::empty()
        };
        tcp.header_len = 60;
        tcp.payload_offset = 94;
        tcp.payload_len = 0;
        network.payload_len = 60;
        assert!(!tcp.valid(94, &network));

        network.next_header = 17;
        network.payload_len = u32::from(u16::MAX) + 1;
        let udp = NetStackTransport {
            outcome: NET_STACK_TRANSPORT_UDP,
            protocol: 17,
            source_port: 1000,
            destination_port: 9000,
            header_len: 8,
            payload_offset: 42,
            payload_len: u32::from(u16::MAX - 8) + 1,
            rss_hash: 123,
            ..NetStackTransport::empty()
        };
        assert!(!udp.valid(70_000, &network));
    }

    #[test]
    fn lifecycle_requires_owned_quiesce_and_drain() {
        let handle = NetStackHandle(9);
        let mut lifecycle = NetStackLifecycle::new();
        lifecycle.activate(handle, 3, 4).unwrap();
        assert_eq!(
            lifecycle.begin_remove(handle, 3, 5),
            Err(NetStackRemoveError::OwnerMismatch)
        );
        lifecycle.begin_remove(handle, 3, 4).unwrap();
        assert_eq!(lifecycle.snapshot().state, NetStackState::Quiescing);
        assert!(lifecycle.begin_drain(handle));
        assert!(lifecycle.finish_remove(handle));
        assert_eq!(lifecycle.snapshot(), NetStackSnapshot::absent());
    }

    #[test]
    fn lifecycle_rejects_two_active_generations() {
        let mut lifecycle = NetStackLifecycle::new();
        lifecycle.activate(NetStackHandle(1), 10, 1).unwrap();
        assert_eq!(
            lifecycle.activate(NetStackHandle(2), 11, 2),
            Err(NetStackRegisterErrorKind::AlreadyActive)
        );
    }
}
