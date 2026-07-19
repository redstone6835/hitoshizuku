//! 网络协议栈 ELM 与常驻 host 之间的生命周期契约。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;

use crate::boot::NetStackBootConfig;
use crate::buf::{DropReason, PacketBatch, PacketChain, TxBatch, TxPacket};
use crate::control::{ConfigSnapshot, NeighborKey};
use crate::flow::{FlowKey, FlowShard, FlowShardStats, UdpSendFailure};
use crate::pipeline::FrontendPacket;
use crate::transport::{
    ControlErrorTarget, LocalUdpIngressError, PreparedRawTx, PreparedTcpTx, PreparedUdpTx,
    RawBindError, TcpBindError, TcpIngressError, TcpPacket, TcpPath, TransportControlError,
    UdpBindError, UdpDatagram,
};
use crate::tuning::PACKET_BATCH_CAPACITY;
use crate::{
    Endpoint, FlowId, InterfaceId, IpAddr, ListenGroup, ListenGroupId, ShardId, SocketError,
    SocketFacade, TcpTxLease, UdpTxLease,
};

static NEXT_STACK_HANDLE: AtomicU64 = AtomicU64::new(1);
static STACK_BOOT_CONFIG: Mutex<Option<NetStackBootConfig>> = Mutex::new(None);
static STACK_REGISTRAR: Mutex<Option<&'static dyn NetStackRegistrar>> = Mutex::new(None);

pub const NET_STACK_CALL_ABI_VERSION: u16 = 1;
pub const NET_STACK_CALL_RUST_ABI: &str = "fn(&mutnet::stack::NetStackCallV1)->i32";
pub const NET_STACK_CALL_STATUS_OK: i32 = 0;
pub const NET_STACK_CALL_STATUS_INVALID: i32 = -22;

pub const NET_STACK_OP_PROBE: u32 = 1;
pub const NET_STACK_OP_WORKER_TURN: u32 = 2;
pub const NET_STACK_OP_QUIESCE: u32 = 3;
pub const NET_STACK_OP_TX_HEADER: u32 = 4;
pub const NET_STACK_OP_TX_FRAGMENT_HEADER: u32 = 5;
pub const NET_STACK_OP_FLOW_CALL: u32 = 6;

pub const NET_STACK_WORKER_TURN_ABI_VERSION: u16 = 1;
pub const NET_STACK_TX_HEADER_ABI_VERSION: u16 = 1;
pub const NET_STACK_FLOW_CALL_ABI_VERSION: u16 = 1;
pub const NET_STACK_TX_HEADER_CAPACITY: usize = 128;
pub const NET_STACK_TX_UDP: u8 = 1;
pub const NET_STACK_TX_TCP: u8 = 2;
pub const NET_STACK_TX_UDP_FRAGMENT: u8 = 3;
pub const NET_STACK_TX_RAW_FRAGMENT: u8 = 4;
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
pub struct NetStackLocalAddressV1 {
    pub interface: u32,
    pub family: u8,
    pub prefix_len: u8,
    pub reserved0: [u8; 2],
    pub address: [u8; 16],
    pub reserved1: [u8; 8],
}

impl NetStackLocalAddressV1 {
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
pub struct NetStackPacketInputV1 {
    pub frame_len: u32,
    pub rss_hash: u32,
    pub rss_generation: u32,
    pub present: u8,
    pub checksums_validated: u8,
    pub rss_hash_present: u8,
    pub reserved: u8,
}

impl NetStackPacketInputV1 {
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

/// `net.stack` 为一个 RX packet 生成的只读 Ethernet 解析 sidecar。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackEthernetV1 {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
    pub status: u8,
    pub reserved: [u8; 5],
}

impl NetStackEthernetV1 {
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
pub struct NetStackNetworkV1 {
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

impl NetStackNetworkV1 {
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

    pub fn valid(&self, frame_len: u32, ethernet: &NetStackEthernetV1) -> bool {
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
pub struct NetStackTcpOptionsV1 {
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

impl NetStackTcpOptionsV1 {
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
pub struct NetStackTransportV1 {
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
    pub tcp_options: NetStackTcpOptionsV1,
    pub reserved2: [u64; 2],
}

impl NetStackTransportV1 {
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
            tcp_options: NetStackTcpOptionsV1::empty(),
            reserved2: [0; 2],
        }
    }

    pub const fn skipped() -> Self {
        Self {
            outcome: NET_STACK_TRANSPORT_SKIPPED,
            ..Self::empty()
        }
    }

    pub fn valid(&self, frame_len: u32, network: &NetStackNetworkV1) -> bool {
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
            && self.tcp_options == NetStackTcpOptionsV1::empty()
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

/// host 提交给 `net.stack` 的单个 TX header 构造输入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackTxInputV1 {
    pub kind: u8,
    pub family: u8,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub source_mac: [u8; 6],
    pub destination_mac: [u8; 6],
    pub source_port: u16,
    pub destination_port: u16,
    pub tcp_flags: u16,
    pub tcp_window: u16,
    pub tcp_options_len: u8,
    pub reserved0: [u8; 3],
    pub payload_offset: u32,
    pub payload_len: u32,
    pub tcp_sequence: u32,
    pub tcp_acknowledgement: u32,
    pub source: [u8; 16],
    pub destination: [u8; 16],
    pub tcp_options: [u8; 40],
    pub reserved1: [u64; 2],
}

/// host 提交给 `net.stack` 的单个分片 header 构造输入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NetStackTxFragmentInputV1 {
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

impl NetStackTxFragmentInputV1 {
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
            kind: NET_STACK_TX_UDP_FRAGMENT,
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
            kind: NET_STACK_TX_RAW_FRAGMENT,
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
            NET_STACK_TX_UDP_FRAGMENT => {
                self.source_port != 0
                    && self.destination_port != 0
                    && self.raw_header_len == 0
                    && self.raw_flags == 0
                    && self.fragment_offset <= self.payload_len
                    && self.fragment_offset % 8 == 0
                    && self.payload_len <= u32::from(u16::MAX - 8)
            }
            NET_STACK_TX_RAW_FRAGMENT => {
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
#[repr(C)]
pub struct NetStackTxFragmentHeaderV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub generation: u64,
    pub payload: *const PacketChain,
    pub input: NetStackTxFragmentInputV1,
    pub committed: u8,
    pub more_fragments: u8,
    pub reserved0: [u8; 2],
    pub header_len: u16,
    pub header: [u8; NET_STACK_TX_HEADER_CAPACITY],
    pub payload_offset: u32,
    pub payload_len: u32,
    pub next_fragment_offset: u32,
    pub reserved1: [u64; 2],
}

impl NetStackTxFragmentHeaderV1 {
    pub fn new(generation: u64, payload: &PacketChain, input: NetStackTxFragmentInputV1) -> Self {
        Self {
            abi_version: NET_STACK_TX_HEADER_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            generation,
            payload,
            input,
            committed: 0,
            more_fragments: 0,
            reserved0: [0; 2],
            header_len: 0,
            header: [0; NET_STACK_TX_HEADER_CAPACITY],
            payload_offset: 0,
            payload_len: 0,
            next_fragment_offset: 0,
            reserved1: [0; 2],
        }
    }

    pub fn valid_header(
        &self,
        generation: u64,
        payload: *const PacketChain,
        input: &NetStackTxFragmentInputV1,
    ) -> bool {
        self.abi_version == NET_STACK_TX_HEADER_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.generation == generation
            && self.payload == payload
            && !self.payload.is_null()
            && &self.input == input
            && input.valid()
            && self.reserved0 == [0; 2]
            && self.reserved1 == [0; 2]
    }

    pub fn fully_committed(&self, payload: &PacketChain) -> bool {
        let end = self.payload_offset.checked_add(self.payload_len);
        self.committed == 1
            && self.more_fragments <= 1
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
            NET_STACK_TX_UDP_FRAGMENT => {
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
            NET_STACK_TX_RAW_FRAGMENT => {
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

impl NetStackTxInputV1 {
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
    ) -> Option<Self> {
        let (family, source, destination) = tx_addresses(source, destination)?;
        Some(Self {
            kind: NET_STACK_TX_UDP,
            family,
            hop_limit,
            traffic_class,
            source_mac,
            destination_mac,
            source_port,
            destination_port,
            tcp_flags: 0,
            tcp_window: 0,
            tcp_options_len: 0,
            reserved0: [0; 3],
            payload_offset: 0,
            payload_len,
            tcp_sequence: 0,
            tcp_acknowledgement: 0,
            source,
            destination,
            tcp_options: [0; 40],
            reserved1: [0; 2],
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn tcp(
        source: crate::IpAddr,
        destination: crate::IpAddr,
        source_port: u16,
        destination_port: u16,
        source_mac: [u8; 6],
        destination_mac: [u8; 6],
        sequence: u32,
        acknowledgement: u32,
        flags: u16,
        window: u16,
        options: &[u8],
        payload_len: u32,
    ) -> Option<Self> {
        let (family, source, destination) = tx_addresses(source, destination)?;
        if options.len() > 40 || options.len() % 4 != 0 {
            return None;
        }
        let mut tcp_options = [0; 40];
        tcp_options[..options.len()].copy_from_slice(options);
        Some(Self {
            kind: NET_STACK_TX_TCP,
            family,
            hop_limit: 64,
            traffic_class: 0,
            source_mac,
            destination_mac,
            source_port,
            destination_port,
            tcp_flags: flags,
            tcp_window: window,
            tcp_options_len: options.len() as u8,
            reserved0: [0; 3],
            payload_offset: 0,
            payload_len,
            tcp_sequence: sequence,
            tcp_acknowledgement: acknowledgement,
            source,
            destination,
            tcp_options,
            reserved1: [0; 2],
        })
    }

    pub fn valid(&self) -> bool {
        if !matches!(
            self.family,
            NET_STACK_ADDRESS_FAMILY_IPV4 | NET_STACK_ADDRESS_FAMILY_IPV6
        ) || self.destination_port == 0
            || self.reserved0 != [0; 3]
            || self.reserved1 != [0; 2]
            || (self.family == NET_STACK_ADDRESS_FAMILY_IPV4
                && (self.source[4..] != [0; 12] || self.destination[4..] != [0; 12]))
        {
            return false;
        }
        let transport_len = match self.kind {
            NET_STACK_TX_UDP => {
                if self.tcp_fields_empty() {
                    self.payload_len.checked_add(8)
                } else {
                    None
                }
            }
            NET_STACK_TX_TCP => {
                let options_len = usize::from(self.tcp_options_len);
                if self.source_port == 0
                    || self.tcp_flags & !0x01ff != 0
                    || options_len > self.tcp_options.len()
                    || options_len % 4 != 0
                    || self.tcp_options[options_len..] != [0; 40][options_len..]
                {
                    None
                } else {
                    self.payload_len
                        .checked_add(20 + u32::from(self.tcp_options_len))
                }
            }
            _ => None,
        };
        transport_len.is_some_and(|transport_len| {
            transport_len <= u32::from(u16::MAX)
                && (self.family != NET_STACK_ADDRESS_FAMILY_IPV4
                    || transport_len <= u32::from(u16::MAX - 20))
        })
    }

    pub fn expected_header_len(&self) -> Option<u16> {
        if !self.valid() {
            return None;
        }
        let ip_len = if self.family == NET_STACK_ADDRESS_FAMILY_IPV4 {
            20
        } else {
            40
        };
        let transport_len = if self.kind == NET_STACK_TX_UDP {
            8
        } else {
            20 + u16::from(self.tcp_options_len)
        };
        Some(14 + ip_len + transport_len)
    }

    fn tcp_fields_empty(&self) -> bool {
        self.tcp_flags == 0
            && self.tcp_window == 0
            && self.tcp_options_len == 0
            && self.tcp_sequence == 0
            && self.tcp_acknowledgement == 0
            && self.tcp_options == [0; 40]
    }
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

/// `net.stack` 返回给 host 的固定容量 TX header。
#[repr(C)]
pub struct NetStackTxHeaderV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub generation: u64,
    pub payload: *const PacketChain,
    pub input: NetStackTxInputV1,
    pub committed: u8,
    pub reserved0: u8,
    pub header_len: u16,
    pub header: [u8; NET_STACK_TX_HEADER_CAPACITY],
    pub reserved1: [u64; 2],
}

impl NetStackTxHeaderV1 {
    pub fn new(generation: u64, payload: &PacketChain, input: NetStackTxInputV1) -> Self {
        Self {
            abi_version: NET_STACK_TX_HEADER_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            generation,
            payload,
            input,
            committed: 0,
            reserved0: 0,
            header_len: 0,
            header: [0; NET_STACK_TX_HEADER_CAPACITY],
            reserved1: [0; 2],
        }
    }

    pub fn valid_header(
        &self,
        generation: u64,
        payload: *const PacketChain,
        input: &NetStackTxInputV1,
    ) -> bool {
        self.abi_version == NET_STACK_TX_HEADER_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.generation == generation
            && self.payload == payload
            && !self.payload.is_null()
            && &self.input == input
            && self.input.valid()
            && self.reserved0 == 0
            && self.reserved1 == [0; 2]
    }

    pub fn fully_committed(&self, payload: &PacketChain) -> bool {
        if self.committed != 1
            || self
                .input
                .payload_offset
                .checked_add(self.input.payload_len)
                .is_none_or(|end| end > payload.total_len() as u32)
            || self.input.expected_header_len() != Some(self.header_len)
            || usize::from(self.header_len) > self.header.len()
            || self.header[usize::from(self.header_len)..]
                != [0; NET_STACK_TX_HEADER_CAPACITY][usize::from(self.header_len)..]
        {
            return false;
        }
        self.output_valid()
    }

    pub fn header_bytes(&self) -> &[u8] {
        &self.header[..usize::from(self.header_len)]
    }

    fn output_valid(&self) -> bool {
        let header = self.header_bytes();
        if header[..6] != self.input.destination_mac || header[6..12] != self.input.source_mac {
            return false;
        }
        let transport_offset = match self.input.family {
            NET_STACK_ADDRESS_FAMILY_IPV4 => {
                let transport_len = self.header_len as u32 - 34 + self.input.payload_len;
                let total_len = 20 + transport_len;
                if header[12..14] != 0x0800u16.to_be_bytes() || header[14] != 0x45 {
                    return false;
                }
                if header[15] != self.input.traffic_class
                    || header[16..18] != (total_len as u16).to_be_bytes()
                    || header[18..20] != [0; 2]
                    || header[20..22] != 0x4000u16.to_be_bytes()
                    || header[22] != self.input.hop_limit
                    || header[23]
                        != if self.input.kind == NET_STACK_TX_UDP {
                            17
                        } else {
                            6
                        }
                    || header[26..30] != self.input.source[..4]
                    || header[30..34] != self.input.destination[..4]
                    || checksum_bytes(&header[14..34]) != 0
                {
                    return false;
                }
                34
            }
            NET_STACK_ADDRESS_FAMILY_IPV6 => {
                let transport_len = self.header_len as u32 - 54 + self.input.payload_len;
                let version_class = 0x6000_0000u32 | (u32::from(self.input.traffic_class) << 20);
                if header[12..14] != 0x86ddu16.to_be_bytes()
                    || header[14..18] != version_class.to_be_bytes()
                    || header[18..20] != (transport_len as u16).to_be_bytes()
                    || header[20]
                        != if self.input.kind == NET_STACK_TX_UDP {
                            17
                        } else {
                            6
                        }
                    || header[21] != self.input.hop_limit
                    || header[22..38] != self.input.source
                    || header[38..54] != self.input.destination
                {
                    return false;
                }
                54
            }
            _ => return false,
        };
        match self.input.kind {
            NET_STACK_TX_UDP => {
                let udp = &header[transport_offset..transport_offset + 8];
                let udp_len = self.input.payload_len + 8;
                udp[..2] == self.input.source_port.to_be_bytes()
                    && udp[2..4] == self.input.destination_port.to_be_bytes()
                    && udp[4..6] == (udp_len as u16).to_be_bytes()
                    && udp[6..8] != [0; 2]
            }
            NET_STACK_TX_TCP => {
                let tcp_len = 20 + usize::from(self.input.tcp_options_len);
                let tcp = &header[transport_offset..transport_offset + tcp_len];
                tcp[..2] == self.input.source_port.to_be_bytes()
                    && tcp[2..4] == self.input.destination_port.to_be_bytes()
                    && tcp[4..8] == self.input.tcp_sequence.to_be_bytes()
                    && tcp[8..12] == self.input.tcp_acknowledgement.to_be_bytes()
                    && tcp[12]
                        == ((tcp_len / 4) as u8) << 4 | u8::from(self.input.tcp_flags & 0x100 != 0)
                    && tcp[13] == self.input.tcp_flags as u8
                    && tcp[14..16] == self.input.tcp_window.to_be_bytes()
                    && tcp[18..20] == [0; 2]
                    && tcp[20..]
                        == self.input.tcp_options[..usize::from(self.input.tcp_options_len)]
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

/// 常驻 worker 与 `net.stack` 间一次批调用的数据帧。
///
/// `input` 在同步调用期间始终归 host 所有。ELM 只能读取它，并逐项提交固定容量
/// sidecar；只有调用成功且 host 完成全帧校验后，packet ownership 才会移动。
#[repr(C)]
pub struct NetStackWorkerTurnV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub generation: u64,
    pub config_generation: u64,
    pub input: *const PacketBatch,
    pub local_addresses: *const NetStackLocalAddressV1,
    pub interface: u32,
    pub local_address_count: u32,
    pub rss_key: [u8; 40],
    pub rss_generation: u32,
    pub input_count: u8,
    pub committed: u8,
    pub reserved0: [u8; 6],
    pub packet_inputs: [NetStackPacketInputV1; PACKET_BATCH_CAPACITY],
    pub ethernet: [NetStackEthernetV1; PACKET_BATCH_CAPACITY],
    pub network: [NetStackNetworkV1; PACKET_BATCH_CAPACITY],
    pub transport: [NetStackTransportV1; PACKET_BATCH_CAPACITY],
    pub reserved1: [u64; 2],
}

impl NetStackWorkerTurnV1 {
    pub fn new(
        generation: u64,
        config_generation: u64,
        interface: u32,
        local_addresses: &[NetStackLocalAddressV1],
        rss_key: [u8; 40],
        rss_generation: u32,
        input: &PacketBatch,
    ) -> Self {
        let mut turn = Self {
            abi_version: NET_STACK_WORKER_TURN_ABI_VERSION,
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
            packet_inputs: [NetStackPacketInputV1::empty(); PACKET_BATCH_CAPACITY],
            ethernet: [NetStackEthernetV1::empty(); PACKET_BATCH_CAPACITY],
            network: [NetStackNetworkV1::empty(); PACKET_BATCH_CAPACITY],
            transport: [NetStackTransportV1::empty(); PACKET_BATCH_CAPACITY],
            reserved1: [0; 2],
        };
        for index in 0..input.len() {
            if let (Some(packet), Some(metadata)) = (input.packet(index), input.metadata(index)) {
                turn.packet_inputs[index] = NetStackPacketInputV1 {
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
        local_addresses: *const NetStackLocalAddressV1,
        local_address_count: u32,
        rss_key: &[u8; 40],
        rss_generation: u32,
    ) -> bool {
        self.abi_version == NET_STACK_WORKER_TURN_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
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
                .all(|facts| *facts == NetStackPacketInputV1::empty())
            && self.ethernet[..usize::from(self.input_count)]
                .iter()
                .all(NetStackEthernetV1::valid)
            && self.ethernet[usize::from(self.input_count)..]
                .iter()
                .all(|sidecar| *sidecar == NetStackEthernetV1::empty())
            && self.network[..usize::from(self.input_count)]
                .iter()
                .enumerate()
                .all(|(index, sidecar)| {
                    sidecar.valid(self.packet_inputs[index].frame_len, &self.ethernet[index])
                })
            && self.network[usize::from(self.input_count)..]
                .iter()
                .all(|sidecar| *sidecar == NetStackNetworkV1::empty())
            && self.transport[..usize::from(self.input_count)]
                .iter()
                .enumerate()
                .all(|(index, sidecar)| {
                    sidecar.valid(self.packet_inputs[index].frame_len, &self.network[index])
                })
            && self.transport[usize::from(self.input_count)..]
                .iter()
                .all(|sidecar| *sidecar == NetStackTransportV1::empty())
    }

    pub fn ethernet(&self) -> &[NetStackEthernetV1] {
        &self.ethernet[..usize::from(self.input_count)]
    }

    pub fn network(&self) -> &[NetStackNetworkV1] {
        &self.network[..usize::from(self.input_count)]
    }

    pub fn transport(&self) -> &[NetStackTransportV1] {
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
    HasBlockedTcpOutput {
        output: Option<bool>,
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
    TakeTcpOutput {
        output: Option<Option<PreparedTcpTx>>,
    },
    ResumeTcpOutput {
        now_ns: u64,
        budget: usize,
        output: Option<usize>,
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
    RefreshTcpTxPath {
        work: *mut PreparedTcpTx,
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<Result<(), SocketError>>,
    },
    ProcessLocalTcp {
        interface: InterfaceId,
        path: TcpPath,
        key: FlowKey,
        packet: TcpPacket,
        payload: *const TcpTxLease,
        now_ns: u64,
        output: Option<Result<FlowId, TcpIngressError>>,
    },
    ProcessLocalUdp {
        interface: InterfaceId,
        source: Endpoint,
        destination: Endpoint,
        payload: *const UdpTxLease,
        hop_limit: u8,
        traffic_class: u8,
        now_ns: u64,
        output: Option<Result<FlowId, LocalUdpIngressError>>,
    },
    InvalidateInterface {
        interface: InterfaceId,
        output: Option<usize>,
    },
    ObserveNeighbor {
        key: NeighborKey,
        mac_address: [u8; 6],
        now_ns: u64,
        output: Option<bool>,
    },
    LookupNeighbor {
        key: NeighborKey,
        now_ns: u64,
        output: Option<Option<[u8; 6]>>,
    },
    PrepareUdpTx {
        flow: FlowId,
        payload: Option<UdpTxLease>,
        mark: u32,
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<Result<PreparedUdpTx, (SocketError, UdpTxLease)>>,
    },
    PrepareRawTx {
        flow: FlowId,
        payload: Option<UdpTxLease>,
        mark: u32,
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<Result<PreparedRawTx, (SocketError, UdpTxLease)>>,
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
    PushFrontendBatch {
        packets: Option<Vec<FrontendPacket>>,
    },
    ProcessFrontendBatch {
        interface: InterfaceId,
        local_mac: [u8; 6],
        config: *const ConfigSnapshot,
        now_ns: u64,
        output: Option<(TxBatch, PacketBatch)>,
        drop_counts: [u32; DropReason::COUNT],
    },
    TakeReassembledInput {
        output: Option<Option<PacketBatch>>,
    },
    ParseReassembled {
        input: Option<PacketBatch>,
        ethernet: Vec<NetStackEthernetV1>,
        network: Vec<NetStackNetworkV1>,
        transport: Vec<NetStackTransportV1>,
        output: Option<Result<(), PacketBatch>>,
    },
    TakeReassembled {
        output: Option<Option<FrontendPacket>>,
    },
    TakeForwardedError {
        output: Option<Option<(InterfaceId, ControlErrorTarget, TransportControlError, u64)>>,
    },
    ApplyTransportError {
        interface: InterfaceId,
        target: ControlErrorTarget,
        error: TransportControlError,
        now_ns: u64,
        output: Option<bool>,
    },
}

/// 一次代际固定的 `FlowShard` 状态调用。
#[repr(C)]
pub struct NetStackFlowCallV1 {
    pub abi_version: u16,
    pub reserved0: u16,
    pub struct_size: u32,
    pub generation: u64,
    pub shard: ShardId,
    pub committed: u8,
    pub reserved1: [u8; 5],
    pub command: NetStackFlowCommand,
}

#[kernel_symbols::export]
impl NetStackFlowCallV1 {
    pub fn new(generation: u64, shard: ShardId, command: NetStackFlowCommand) -> Self {
        Self {
            abi_version: NET_STACK_FLOW_CALL_ABI_VERSION,
            reserved0: 0,
            struct_size: core::mem::size_of::<Self>() as u32,
            generation,
            shard,
            committed: 0,
            reserved1: [0; 5],
            command,
        }
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackFlowCallV1.valid_header",
        contract = "kernel.net.stack-flow-state@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn valid_header(&self, generation: u64) -> bool {
        self.abi_version == NET_STACK_FLOW_CALL_ABI_VERSION
            && self.reserved0 == 0
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.generation == generation
            && self.committed == 0
            && self.reserved1 == [0; 5]
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

#[kernel_symbols::export(
    name = "net.stack.dispatch_flow_shard_call",
    contract = "kernel.net.stack-flow-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn dispatch_flow_shard_call(shard: &mut FlowShard, call: &mut NetStackFlowCallV1) -> bool {
    match &mut call.command {
        NetStackFlowCommand::Stats { output } => *output = Some(shard.stats()),
        NetStackFlowCommand::RunDueTimers { now_ns } => shard.run_due_timers(*now_ns),
        NetStackFlowCommand::NextTimerDeadline { output } => {
            *output = Some(shard.next_timer_deadline_ns());
        }
        NetStackFlowCommand::HasBlockedTcpOutput { output } => {
            *output = Some(shard.has_blocked_tcp_output());
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
        NetStackFlowCommand::TakeTcpOutput { output } => {
            *output = Some(shard.take_tcp_output());
        }
        NetStackFlowCommand::ResumeTcpOutput {
            now_ns,
            budget,
            output,
        } => *output = Some(shard.resume_tcp_output(*now_ns, *budget)),
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
        NetStackFlowCommand::RefreshTcpTxPath {
            work,
            config,
            now_ns,
            output,
        } => {
            if work.is_null() || !work.is_aligned() || config.is_null() || !config.is_aligned() {
                return false;
            }
            // Safety: ELM 已校验 state-call 中 work 的可写范围，host 不保存指针。
            let work = unsafe { &mut **work };
            // Safety: config 只在同步 state-call 期间借用。
            let config = unsafe { &**config };
            *output = Some(shard.refresh_tcp_tx_path(work, config, *now_ns));
        }
        NetStackFlowCommand::ProcessLocalTcp {
            interface,
            path,
            key,
            packet,
            payload,
            now_ns,
            output,
        } => {
            if !payload.is_null() && !payload.is_aligned() {
                return false;
            }
            // Safety: 非空 payload 只在同步 state-call 期间借用。
            let payload = unsafe { payload.as_ref() };
            *output =
                Some(shard.process_local_tcp(*interface, *path, *key, *packet, payload, *now_ns));
        }
        NetStackFlowCommand::ProcessLocalUdp {
            interface,
            source,
            destination,
            payload,
            hop_limit,
            traffic_class,
            now_ns,
            output,
        } => {
            if payload.is_null() || !payload.is_aligned() {
                return false;
            }
            // Safety: payload 只在同步 state-call 期间借用。
            let payload = unsafe { &**payload };
            *output = Some(shard.process_local_udp(
                *interface,
                *source,
                *destination,
                payload,
                *hop_limit,
                *traffic_class,
                *now_ns,
            ));
        }
        NetStackFlowCommand::InvalidateInterface { interface, output } => {
            *output = Some(shard.invalidate_interface(*interface));
        }
        NetStackFlowCommand::ObserveNeighbor {
            key,
            mac_address,
            now_ns,
            output,
        } => *output = Some(shard.observe_neighbor(*key, *mac_address, *now_ns)),
        NetStackFlowCommand::LookupNeighbor {
            key,
            now_ns,
            output,
        } => *output = Some(shard.lookup_neighbor(*key, *now_ns)),
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
            *output = Some(shard.prepare_udp_tx(*flow, owned, *mark, config, *now_ns));
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
            *output = Some(shard.prepare_raw_tx(*flow, owned, *mark, config, *now_ns));
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
        NetStackFlowCommand::PushFrontendBatch { packets } => {
            let Some(packets) = packets.take() else {
                return false;
            };
            shard.push_frontend_batch(packets);
        }
        NetStackFlowCommand::ProcessFrontendBatch {
            interface,
            local_mac,
            config,
            now_ns,
            output,
            drop_counts,
        } => {
            if config.is_null() || !config.is_aligned() || output.is_some() {
                return false;
            }
            // Safety: config 只在同步 state-call 期间借用。
            let config = unsafe { &**config };
            let mut tx = TxBatch::new();
            let mut recycle = PacketBatch::new();
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
        }
        NetStackFlowCommand::TakeReassembledInput { output } => {
            *output = Some(shard.take_reassembled_input());
        }
        NetStackFlowCommand::ParseReassembled {
            input,
            ethernet,
            network,
            transport,
            output,
        } => {
            let Some(input) = input.take() else {
                return false;
            };
            *output = Some(shard.parse_reassembled_batch(input, ethernet, network, transport));
        }
        NetStackFlowCommand::TakeReassembled { output } => {
            *output = Some(shard.take_reassembled());
        }
        NetStackFlowCommand::TakeForwardedError { output } => {
            *output = Some(shard.take_forwarded_error());
        }
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
    call.committed = 1;
    true
}

/// 常驻 worker shell 与 `net.stack` 间一次同步调用的固定帧。
#[repr(C)]
pub struct NetStackCallV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub opcode: u32,
    pub generation: u64,
    pub ready: u8,
    pub quiesced: u8,
    pub reserved0: [u8; 6],
    pub worker_turn: *mut NetStackWorkerTurnV1,
    pub tx_header: *mut NetStackTxHeaderV1,
    pub reserved1: [u64; 2],
}

#[kernel_symbols::export]
impl NetStackCallV1 {
    pub fn new(opcode: u32, generation: u64) -> Self {
        Self {
            abi_version: NET_STACK_CALL_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            opcode,
            generation,
            ready: 0,
            quiesced: 0,
            reserved0: [0; 6],
            worker_turn: core::ptr::null_mut(),
            tx_header: core::ptr::null_mut(),
            reserved1: [0; 2],
        }
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackCallV1.valid",
        contract = "kernel.net.stack-call-frame@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn valid(&self, opcode: u32, generation: u64) -> bool {
        self.abi_version == NET_STACK_CALL_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.opcode == opcode
            && self.generation == generation
            && self.reserved0 == [0; 6]
            && self.reserved1[1] == 0
            && (matches!(
                opcode,
                NET_STACK_OP_TX_FRAGMENT_HEADER | NET_STACK_OP_FLOW_CALL
            ) || self.reserved1[0] == 0)
            && match opcode {
                NET_STACK_OP_WORKER_TURN => !self.worker_turn.is_null() && self.tx_header.is_null(),
                NET_STACK_OP_TX_HEADER => self.worker_turn.is_null() && !self.tx_header.is_null(),
                NET_STACK_OP_TX_FRAGMENT_HEADER => {
                    self.worker_turn.is_null() && self.tx_header.is_null() && self.reserved1[0] != 0
                }
                NET_STACK_OP_FLOW_CALL => {
                    self.worker_turn.is_null() && self.tx_header.is_null() && self.reserved1[0] != 0
                }
                _ => self.worker_turn.is_null() && self.tx_header.is_null(),
            }
    }
}

/// 动态 `net.stack` 的代际固定 export 描述。
pub struct PinnedNetStackEndpoint {
    owner_cell: u64,
    owner_generation: u64,
    export_name: Box<str>,
    export_contract: Box<str>,
    export_version: u32,
}

#[kernel_symbols::export]
impl PinnedNetStackEndpoint {
    #[kernel_symbols::export(
        name = "net.stack.PinnedNetStackEndpoint.current",
        contract = "kernel.net.stack-endpoint@1",
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

pub type IntegratedNetStackCall = fn(&mut NetStackCallV1) -> i32;

pub enum NetStackEndpoint {
    Integrated(IntegratedNetStackCall),
    Pinned(PinnedNetStackEndpoint),
}

/// 一个 stack generation 的原子注册单元。
pub struct NetStackRegistration {
    handle: NetStackHandle,
    endpoint: NetStackEndpoint,
}

#[kernel_symbols::export]
impl NetStackRegistration {
    pub fn integrated(call: IntegratedNetStackCall) -> Option<Self> {
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
    pub fn pinned(endpoint: PinnedNetStackEndpoint) -> Self {
        Self {
            handle: next_stack_handle(),
            endpoint: NetStackEndpoint::Pinned(endpoint),
        }
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
    pub probed: bool,
}

impl NetStackSnapshot {
    pub const fn absent() -> Self {
        Self {
            state: NetStackState::Absent,
            handle: None,
            owner_cell: 0,
            generation: 0,
            probed: false,
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
    StackUnavailable,
    CallFailed,
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

    fn build_tx_header(
        &self,
        payload: &PacketChain,
        input: NetStackTxInputV1,
    ) -> Result<NetStackTxHeaderV1, NetStackTxError>;

    fn build_tx_fragment_header(
        &self,
        payload: &PacketChain,
        input: NetStackTxFragmentInputV1,
    ) -> Result<NetStackTxFragmentHeaderV1, NetStackTxError>;

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

pub fn build_tx_header(
    payload: &PacketChain,
    input: NetStackTxInputV1,
) -> Result<NetStackTxHeaderV1, NetStackTxError> {
    if !input.valid()
        || input
            .payload_offset
            .checked_add(input.payload_len)
            .is_none_or(|end| end > payload.total_len() as u32)
    {
        return Err(NetStackTxError::InvalidInput);
    }
    let registrar = *STACK_REGISTRAR.lock();
    registrar
        .ok_or(NetStackTxError::StackUnavailable)?
        .build_tx_header(payload, input)
}

pub fn build_tx_fragment_header(
    payload: &PacketChain,
    input: NetStackTxFragmentInputV1,
) -> Result<NetStackTxFragmentHeaderV1, NetStackTxError> {
    if !input.valid() || input.payload_len as usize > payload.total_len() {
        return Err(NetStackTxError::InvalidInput);
    }
    let registrar = *STACK_REGISTRAR.lock();
    registrar
        .ok_or(NetStackTxError::StackUnavailable)?
        .build_tx_fragment_header(payload, input)
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
            probed: false,
        };
        Ok(())
    }

    pub fn mark_probed(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle) || self.snapshot.state != NetStackState::Active {
            return false;
        }
        self.snapshot.probed = true;
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
        self.snapshot.probed = false;
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
    fn call_frame_rejects_stale_generation_and_reserved_bits() {
        let mut frame = NetStackCallV1::new(NET_STACK_OP_PROBE, 7);
        assert!(frame.valid(NET_STACK_OP_PROBE, 7));
        assert!(!frame.valid(NET_STACK_OP_PROBE, 8));
        frame.reserved1[0] = 1;
        assert!(!frame.valid(NET_STACK_OP_PROBE, 7));
    }

    #[test]
    fn tx_header_frame_binds_input_payload_and_normalized_output() {
        let source = crate::IpAddr::V4(crate::Ipv4Addr::new(10, 0, 2, 2));
        let destination = crate::IpAddr::V4(crate::Ipv4Addr::new(10, 0, 2, 15));
        let payload = PacketChain::from_owned(b"test".to_vec());
        let input =
            NetStackTxInputV1::udp(source, destination, 1000, 9000, [1; 6], [2; 6], 64, 7, 4)
                .unwrap();
        let payload_pointer = &payload as *const PacketChain;
        let mut output = NetStackTxHeaderV1::new(9, &payload, input);
        assert!(output.valid_header(9, payload_pointer, &input));
        assert!(!output.fully_committed(&payload));

        output.header_len = 42;
        output.header[..6].copy_from_slice(&[2; 6]);
        output.header[6..12].copy_from_slice(&[1; 6]);
        output.header[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        output.header[14] = 0x45;
        output.header[15] = 7;
        output.header[16..18].copy_from_slice(&32u16.to_be_bytes());
        output.header[20..22].copy_from_slice(&0x4000u16.to_be_bytes());
        output.header[22] = 64;
        output.header[23] = 17;
        output.header[26..30].copy_from_slice(&[10, 0, 2, 2]);
        output.header[30..34].copy_from_slice(&[10, 0, 2, 15]);
        let ip_checksum = checksum_bytes(&output.header[14..34]);
        output.header[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
        output.header[34..36].copy_from_slice(&1000u16.to_be_bytes());
        output.header[36..38].copy_from_slice(&9000u16.to_be_bytes());
        output.header[38..40].copy_from_slice(&12u16.to_be_bytes());

        let mut full_packet = output.header[..42].to_vec();
        full_packet.extend_from_slice(b"test");
        let checksum_packet = PacketChain::from_owned(full_packet);
        let udp_checksum =
            crate::pipeline::transport_checksum(&checksum_packet, 34, 12, source, destination, 17)
                .unwrap();
        output.header[40..42].copy_from_slice(&udp_checksum.to_be_bytes());
        output.committed = 1;
        assert!(output.fully_committed(&payload));

        output.header[15] ^= 1;
        assert!(!output.fully_committed(&payload));
        output.header[15] ^= 1;
        assert!(output.fully_committed(&payload));
        output.input.payload_len += 1;
        assert!(!output.valid_header(9, payload_pointer, &input));

        let mut call = NetStackCallV1::new(NET_STACK_OP_TX_HEADER, 9);
        assert!(!call.valid(NET_STACK_OP_TX_HEADER, 9));
        call.tx_header = &mut output;
        assert!(call.valid(NET_STACK_OP_TX_HEADER, 9));
    }

    #[test]
    fn worker_turn_requires_complete_committed_prefix() {
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
        let addresses = [NetStackLocalAddressV1 {
            interface: 1,
            family: NET_STACK_ADDRESS_FAMILY_IPV4,
            prefix_len: 24,
            reserved0: [0; 2],
            address: [10, 0, 2, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            reserved1: [0; 8],
        }];
        let rss_key = [3; 40];
        let input_pointer = &input as *const PacketBatch;
        let mut turn = NetStackWorkerTurnV1::new(7, 9, 1, &addresses, rss_key, 11, &input);
        assert!(turn.valid_header(7, 9, 1, input_pointer, addresses.as_ptr(), 1, &rss_key, 11,));
        assert!(!turn.valid_header(8, 9, 1, input_pointer, addresses.as_ptr(), 1, &rss_key, 11,));
        assert!(!turn.fully_committed(&input));

        turn.ethernet[0].status = NET_STACK_ETHERNET_TRUNCATED;
        turn.network[0] = NetStackNetworkV1::skipped();
        turn.transport[0] = NetStackTransportV1::skipped();
        turn.committed = 1;
        assert!(!turn.fully_committed(&input));
        turn.ethernet[1].status = NET_STACK_ETHERNET_TRUNCATED;
        turn.network[1] = NetStackNetworkV1::skipped();
        turn.transport[1] = NetStackTransportV1::skipped();
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
        let mut network = NetStackNetworkV1 {
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
        let mut tcp = NetStackTransportV1 {
            outcome: NET_STACK_TRANSPORT_TCP,
            protocol: 6,
            source_port: 1000,
            destination_port: 9000,
            header_len: 24,
            payload_offset: 58,
            tcp_flags: 0x002,
            payload_len: 16,
            rss_hash: 123,
            tcp_options: NetStackTcpOptionsV1 {
                flags: NET_STACK_TCP_OPTION_MSS,
                maximum_segment_size: 1460,
                ..NetStackTcpOptionsV1::empty()
            },
            ..NetStackTransportV1::empty()
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

        tcp.tcp_options = NetStackTcpOptionsV1 {
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
            ..NetStackTcpOptionsV1::empty()
        };
        tcp.header_len = 60;
        tcp.payload_offset = 94;
        tcp.payload_len = 0;
        network.payload_len = 60;
        assert!(!tcp.valid(94, &network));

        network.next_header = 17;
        network.payload_len = u32::from(u16::MAX) + 1;
        let udp = NetStackTransportV1 {
            outcome: NET_STACK_TRANSPORT_UDP,
            protocol: 17,
            source_port: 1000,
            destination_port: 9000,
            header_len: 8,
            payload_offset: 42,
            payload_len: u32::from(u16::MAX - 8) + 1,
            rss_hash: 123,
            ..NetStackTransportV1::empty()
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
