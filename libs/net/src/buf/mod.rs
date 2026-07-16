//! 固定容量网络 buffer 与 packet 所有权。

mod packet;
mod pool;

pub use packet::{
    CompletionBatch, CompletionToken, PacketBatch, PacketChain, PacketFragment, PacketRangeError,
    RxRefillBatch, TxBatch, TxPacket,
};
pub use pool::{
    ChunkRef, NetBufGeneration, NetBufId, NetBufLease, NetBufPool, NetBufPoolError, NetBufPoolId,
    NetBufPoolOwner, NetBufPoolStats, NetBufStorage, PoolGeneration,
};

use crate::{NetDeviceId, QueuePairId};

/// 数据面丢包原因。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum DropReason {
    #[default]
    None = 0,
    NoConsumer,
    MalformedDescriptor,
    LoopbackRingFull,
    DeviceGone,
    TruncatedFrame,
    UnsupportedEthernet,
    VlanUnsupported,
    MalformedArp,
    NotLocal,
    MalformedIpv4,
    Ipv4Checksum,
    MalformedIpv6,
    Ipv6ExtensionLimit,
    UnsupportedIpProtocol,
    MalformedUdp,
    UdpChecksum,
    UdpNoEndpoint,
    UdpRingFull,
    FlowTableFull,
    RouteUnavailable,
    TxQueueFull,
    IngressRingFull,
    MalformedTcp,
    TcpChecksum,
    TcpNoEndpoint,
    TcpRingFull,
    FragmentMalformed,
    FragmentOverlap,
    FragmentLimit,
    FragmentTimeout,
    RawNoEndpoint,
    RawRingFull,
}

impl DropReason {
    pub const ALL: [Self; 33] = [
        Self::None,
        Self::NoConsumer,
        Self::MalformedDescriptor,
        Self::LoopbackRingFull,
        Self::DeviceGone,
        Self::TruncatedFrame,
        Self::UnsupportedEthernet,
        Self::VlanUnsupported,
        Self::MalformedArp,
        Self::NotLocal,
        Self::MalformedIpv4,
        Self::Ipv4Checksum,
        Self::MalformedIpv6,
        Self::Ipv6ExtensionLimit,
        Self::UnsupportedIpProtocol,
        Self::MalformedUdp,
        Self::UdpChecksum,
        Self::UdpNoEndpoint,
        Self::UdpRingFull,
        Self::FlowTableFull,
        Self::RouteUnavailable,
        Self::TxQueueFull,
        Self::IngressRingFull,
        Self::MalformedTcp,
        Self::TcpChecksum,
        Self::TcpNoEndpoint,
        Self::TcpRingFull,
        Self::FragmentMalformed,
        Self::FragmentOverlap,
        Self::FragmentLimit,
        Self::FragmentTimeout,
        Self::RawNoEndpoint,
        Self::RawRingFull,
    ];

    pub const COUNT: usize = 33;

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn stat_key(self) -> &'static str {
        match self {
            Self::None => "drop_none",
            Self::NoConsumer => "drop_no_consumer",
            Self::MalformedDescriptor => "drop_malformed_descriptor",
            Self::LoopbackRingFull => "drop_loopback_ring_full",
            Self::IngressRingFull => "drop_ingress_ring_full",
            Self::DeviceGone => "drop_device_gone",
            Self::TruncatedFrame => "drop_truncated_frame",
            Self::UnsupportedEthernet => "drop_unsupported_ethernet",
            Self::VlanUnsupported => "drop_vlan_unsupported",
            Self::MalformedArp => "drop_malformed_arp",
            Self::NotLocal => "drop_not_local",
            Self::MalformedIpv4 => "drop_malformed_ipv4",
            Self::Ipv4Checksum => "drop_ipv4_checksum",
            Self::MalformedIpv6 => "drop_malformed_ipv6",
            Self::Ipv6ExtensionLimit => "drop_ipv6_extension_limit",
            Self::UnsupportedIpProtocol => "drop_unsupported_ip_protocol",
            Self::MalformedUdp => "drop_malformed_udp",
            Self::UdpChecksum => "drop_udp_checksum",
            Self::UdpNoEndpoint => "drop_udp_no_endpoint",
            Self::UdpRingFull => "drop_udp_ring_full",
            Self::MalformedTcp => "drop_malformed_tcp",
            Self::TcpChecksum => "drop_tcp_checksum",
            Self::TcpNoEndpoint => "drop_tcp_no_endpoint",
            Self::TcpRingFull => "drop_tcp_ring_full",
            Self::FragmentMalformed => "drop_fragment_malformed",
            Self::FragmentOverlap => "drop_fragment_overlap",
            Self::FragmentLimit => "drop_fragment_limit",
            Self::FragmentTimeout => "drop_fragment_timeout",
            Self::RawNoEndpoint => "drop_raw_no_endpoint",
            Self::RawRingFull => "drop_raw_ring_full",
            Self::FlowTableFull => "drop_flow_table_full",
            Self::RouteUnavailable => "drop_route_unavailable",
            Self::TxQueueFull => "drop_tx_queue_full",
        }
    }
}

/// packet 随所有权移动的最小元数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketMetadata {
    pub ingress_device: NetDeviceId,
    pub queue_pair: QueuePairId,
    pub rx_timestamp_ns: u64,
    pub rss_hash: Option<u32>,
    pub rss_generation: u32,
    pub frame_len: u32,
    pub drop_reason: DropReason,
}

impl Default for PacketMetadata {
    fn default() -> Self {
        Self {
            ingress_device: NetDeviceId(0),
            queue_pair: QueuePairId(0),
            rx_timestamp_ns: 0,
            rss_hash: None,
            rss_generation: 0,
            frame_len: 0,
            drop_reason: DropReason::None,
        }
    }
}
