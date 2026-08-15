//! Ethernet/IP 批量解析、校验和与流分类。

use alloc::boxed::Box;

#[cfg(test)]
use crate::InterfaceId;
use crate::buf::{DropReason, NetBufPoolError, PacketBatch, PacketChain, PacketMetadata};
#[cfg(test)]
use crate::control::ConfigSnapshot;
use crate::flow::FlowKey;
#[cfg(test)]
use crate::flow::rss_hash;
use crate::stack::{
    NET_STACK_ADDRESS_FAMILY_IPV4, NET_STACK_ADDRESS_FAMILY_IPV6, NET_STACK_DROP_IPV4_CHECKSUM,
    NET_STACK_DROP_IPV6_EXTENSION_LIMIT, NET_STACK_DROP_MALFORMED_ARP,
    NET_STACK_DROP_MALFORMED_IPV4, NET_STACK_DROP_MALFORMED_IPV6, NET_STACK_DROP_MALFORMED_TCP,
    NET_STACK_DROP_MALFORMED_UDP, NET_STACK_DROP_NOT_LOCAL, NET_STACK_DROP_TCP_CHECKSUM,
    NET_STACK_DROP_UDP_CHECKSUM, NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL,
    NET_STACK_ETHERNET_ACCEPTED, NET_STACK_ETHERNET_TRUNCATED, NET_STACK_ETHERNET_UNSUPPORTED,
    NET_STACK_ETHERNET_VLAN_UNSUPPORTED, NET_STACK_NETWORK_ARP, NET_STACK_NETWORK_DROP,
    NET_STACK_NETWORK_FLAG_FRAGMENT, NET_STACK_NETWORK_FLAG_IPV6_PROBLEM,
    NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS, NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST,
    NET_STACK_NETWORK_IP, NET_STACK_NETWORK_SKIPPED, NET_STACK_TCP_OPTION_MSS,
    NET_STACK_TCP_OPTION_SACK_PERMITTED, NET_STACK_TCP_OPTION_TIMESTAMP,
    NET_STACK_TCP_OPTION_WINDOW_SCALE, NET_STACK_TRANSPORT_DROP, NET_STACK_TRANSPORT_ICMP,
    NET_STACK_TRANSPORT_RAW, NET_STACK_TRANSPORT_SKIPPED, NET_STACK_TRANSPORT_TCP,
    NET_STACK_TRANSPORT_UDP, NetStackEthernet, NetStackNetwork, NetStackTcpOptions,
    NetStackTransport,
};
#[cfg(test)]
use crate::transport::{TCP_PROTOCOL_NUMBER, parse_tcp_packet, parse_tcp_packet_trusted};
use crate::transport::{TcpFlags, TcpOptions, TcpPacket, TcpSackBlock, TcpSequence, TcpTimestamp};
use crate::tuning::PACKET_BATCH_CAPACITY;
use crate::{Endpoint, IpAddr, Ipv4Addr, Ipv6Addr, TransportProtocol};

#[cfg(test)]
const ETHERTYPE_IPV4: u16 = 0x0800;
#[cfg(test)]
const ETHERTYPE_ARP: u16 = 0x0806;
#[cfg(test)]
const ETHERTYPE_IPV6: u16 = 0x86dd;
#[cfg(test)]
const ETHERTYPE_VLAN: u16 = 0x8100;
#[cfg(test)]
const ETHERTYPE_PROVIDER_VLAN: u16 = 0x88a8;
#[cfg(test)]
const IP_PROTOCOL_ICMP: u8 = 1;
#[cfg(test)]
const IP_PROTOCOL_UDP: u8 = 17;
const IP_PROTOCOL_ICMPV6: u8 = 58;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EthernetHeader {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArpPacket {
    pub operation: u16,
    pub sender_mac: [u8; 6],
    pub sender_ip: Ipv4Addr,
    pub target_mac: [u8; 6],
    pub target_ip: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpPacket {
    pub source: IpAddr,
    pub destination: IpAddr,
    pub next_header: u8,
    pub header_len: u16,
    pub payload_offset: u16,
    pub payload_len: u32,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub fragment: Option<IpFragment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpFragment {
    pub identification: u32,
    pub offset: u16,
    pub more: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpPacket {
    pub source_port: u16,
    pub destination_port: u16,
    pub payload_offset: u16,
    pub payload_len: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlPacket {
    Arp(ArpPacket),
    Icmp {
        ipv6: bool,
        packet_offset: u16,
        packet_len: u32,
    },
    Fragment(IpPacket),
    Ipv6ParameterProblem {
        pointer: u32,
        suppress_for_multicast: bool,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ipv6OptionProblem {
    pointer: u32,
    suppress_for_multicast: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontendDisposition {
    Tcp,
    Udp,
    Raw,
    Control(ControlPacket),
    Drop(DropReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedPacket {
    pub ethernet: EthernetHeader,
    pub ip: Option<IpPacket>,
    pub tcp: Option<TcpPacket>,
    pub udp: Option<UdpPacket>,
    pub flow: Option<FlowKey>,
    pub rss_hash: Option<u32>,
    pub disposition: FrontendDisposition,
}

pub struct FrontendPacket {
    pub chain: PacketChain,
    pub metadata: PacketMetadata,
    pub parsed: ParsedPacket,
}

pub struct FrontendBatch {
    packets: Box<[Option<FrontendPacket>]>,
    len: u8,
}

impl FrontendBatch {
    pub fn new() -> Self {
        let mut packets = alloc::vec::Vec::with_capacity(PACKET_BATCH_CAPACITY);
        packets.resize_with(PACKET_BATCH_CAPACITY, || None);
        Self {
            packets: packets.into_boxed_slice(),
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn packet(&self, index: usize) -> Option<&FrontendPacket> {
        (index < self.len())
            .then(|| self.packets[index].as_ref())
            .flatten()
    }

    pub fn packet_mut(&mut self, index: usize) -> Option<&mut FrontendPacket> {
        (index < self.len())
            .then(|| self.packets[index].as_mut())
            .flatten()
    }

    pub fn take(&mut self, index: usize) -> Option<FrontendPacket> {
        if index >= self.len() {
            return None;
        }
        let packet = self.packets[index].take();
        while self.len != 0 && self.packets[self.len as usize - 1].is_none() {
            self.len -= 1;
        }
        packet
    }

    pub fn clear(&mut self) {
        for index in 0..self.len() {
            self.packets[index] = None;
        }
        self.len = 0;
    }

    pub fn push(&mut self, packet: FrontendPacket) {
        assert!(self.len as usize != self.packets.len());
        self.packets[self.len as usize] = Some(packet);
        self.len += 1;
    }
}

impl Default for FrontendBatch {
    fn default() -> Self {
        Self::new()
    }
}

pub struct VectorFrontend {
    #[cfg(test)]
    rss_key: [u8; 40],
    #[cfg(test)]
    rss_generation: u32,
}

impl VectorFrontend {
    pub const fn new(rss_key: [u8; 40], rss_generation: u32) -> Self {
        #[cfg(not(test))]
        let _ = (rss_key, rss_generation);
        Self {
            #[cfg(test)]
            rss_key,
            #[cfg(test)]
            rss_generation,
        }
    }

    /// 单元测试使用的 host 参考路径；生产数据面必须从 ELM Ethernet sidecar 进入。
    #[cfg(test)]
    pub fn process(
        &self,
        interface: InterfaceId,
        config: &ConfigSnapshot,
        input: &mut PacketBatch,
        output: &mut FrontendBatch,
    ) {
        output.clear();
        let input_len = input.len();
        for index in 0..input_len {
            let Some((chain, metadata)) = input.take(index) else {
                continue;
            };
            let ethernet = EthernetHeader {
                destination: [0; 6],
                source: [0; 6],
                ethertype: 0,
            };
            output.push(FrontendPacket {
                chain,
                metadata,
                parsed: ParsedPacket {
                    ethernet,
                    ip: None,
                    tcp: None,
                    udp: None,
                    flow: None,
                    rss_hash: None,
                    disposition: FrontendDisposition::Drop(DropReason::TruncatedFrame),
                },
            });
        }
        self.parse_ethernet(output);
        self.parse_network(interface, config, output);
        self.parse_transport(output);
        self.classify_hash(output);
    }

    /// 使用 `net.stack` 已提交的 Ethernet、网络层和传输层 sidecar 构造 frontend batch。
    ///
    /// 调用方必须先完成整个 worker-turn 帧校验；本函数开始后才会移动 packet
    /// ownership，因而 ELM fault 不会留下半消费 batch。
    pub fn process_with_stack_sidecars(
        &self,
        input: &mut PacketBatch,
        ethernet: &[NetStackEthernet],
        network: &[NetStackNetwork],
        transport: &[NetStackTransport],
        output: &mut FrontendBatch,
    ) {
        assert_eq!(input.len(), ethernet.len());
        assert_eq!(input.len(), network.len());
        assert_eq!(input.len(), transport.len());
        output.clear();
        let input_len = input.len();
        for index in 0..input_len {
            let ethernet_sidecar = ethernet[index];
            let network_sidecar = network[index];
            let transport_sidecar = transport[index];
            let Some((chain, metadata)) = input.take(index) else {
                continue;
            };
            let frame_len = chain.total_len() as u32;
            assert!(ethernet_sidecar.valid());
            assert!(network_sidecar.valid(frame_len, &ethernet_sidecar));
            assert!(transport_sidecar.valid(frame_len, &network_sidecar));
            let disposition = match ethernet_sidecar.status {
                NET_STACK_ETHERNET_ACCEPTED => {
                    FrontendDisposition::Drop(DropReason::UnsupportedIpProtocol)
                }
                NET_STACK_ETHERNET_TRUNCATED => {
                    FrontendDisposition::Drop(DropReason::TruncatedFrame)
                }
                NET_STACK_ETHERNET_UNSUPPORTED => {
                    FrontendDisposition::Drop(DropReason::UnsupportedEthernet)
                }
                NET_STACK_ETHERNET_VLAN_UNSUPPORTED => {
                    FrontendDisposition::Drop(DropReason::VlanUnsupported)
                }
                _ => unreachable!("worker-turn sidecar 必须在移动 packet 前完成校验"),
            };
            output.push(FrontendPacket {
                chain,
                metadata,
                parsed: ParsedPacket {
                    ethernet: EthernetHeader {
                        destination: ethernet_sidecar.destination,
                        source: ethernet_sidecar.source,
                        ethertype: ethernet_sidecar.ethertype,
                    },
                    ip: None,
                    tcp: None,
                    udp: None,
                    flow: None,
                    rss_hash: None,
                    disposition,
                },
            });
            let output_index = output.len() - 1;
            let packet = output
                .packet_mut(output_index)
                .expect("刚提交的 frontend packet 必须存在");
            apply_network_sidecar(packet, network_sidecar);
            apply_transport_sidecar(packet, transport_sidecar);
        }
    }

    #[cfg(test)]
    fn parse_ethernet(&self, batch: &mut FrontendBatch) {
        for index in 0..batch.len() {
            let Some(packet) = batch.packet_mut(index) else {
                continue;
            };
            let mut header = [0u8; 14];
            if packet.chain.copy_out(0, &mut header).is_err() {
                set_drop(packet, DropReason::TruncatedFrame);
                continue;
            }
            packet
                .parsed
                .ethernet
                .destination
                .copy_from_slice(&header[0..6]);
            packet
                .parsed
                .ethernet
                .source
                .copy_from_slice(&header[6..12]);
            packet.parsed.ethernet.ethertype = u16::from_be_bytes([header[12], header[13]]);
            packet.parsed.disposition = match packet.parsed.ethernet.ethertype {
                ETHERTYPE_ARP | ETHERTYPE_IPV4 | ETHERTYPE_IPV6 => {
                    FrontendDisposition::Drop(DropReason::UnsupportedIpProtocol)
                }
                ETHERTYPE_VLAN | ETHERTYPE_PROVIDER_VLAN => {
                    FrontendDisposition::Drop(DropReason::VlanUnsupported)
                }
                _ => FrontendDisposition::Drop(DropReason::UnsupportedEthernet),
            };
        }
    }

    #[cfg(test)]
    fn parse_network(
        &self,
        interface: InterfaceId,
        config: &ConfigSnapshot,
        batch: &mut FrontendBatch,
    ) {
        for index in 0..batch.len() {
            let Some(packet) = batch.packet_mut(index) else {
                continue;
            };
            match packet.parsed.ethernet.ethertype {
                ETHERTYPE_ARP => match parse_arp(&packet.chain) {
                    Ok(arp) if is_local_ipv4(config, interface, arp.target_ip) => {
                        packet.parsed.disposition =
                            FrontendDisposition::Control(ControlPacket::Arp(arp));
                    }
                    Ok(_) => set_drop(packet, DropReason::NotLocal),
                    Err(reason) => set_drop(packet, reason),
                },
                ETHERTYPE_IPV4 => {
                    match parse_ipv4(&packet.chain, !packet.metadata.checksums_validated) {
                        Ok(ip) if is_local_ip(config, interface, ip.destination) => {
                            packet.parsed.ip = Some(ip);
                            packet.parsed.disposition = if ip.fragment.is_some() {
                                FrontendDisposition::Control(ControlPacket::Fragment(ip))
                            } else {
                                FrontendDisposition::Drop(DropReason::UnsupportedIpProtocol)
                            };
                        }
                        Ok(_) => set_drop(packet, DropReason::NotLocal),
                        Err(reason) => set_drop(packet, reason),
                    }
                }
                ETHERTYPE_IPV6 => match parse_ipv6(&packet.chain) {
                    Ok((ip, problem)) if is_local_ip(config, interface, ip.destination) => {
                        packet.parsed.ip = Some(ip);
                        packet.parsed.disposition = if let Some(problem) = problem {
                            FrontendDisposition::Control(ControlPacket::Ipv6ParameterProblem {
                                pointer: problem.pointer,
                                suppress_for_multicast: problem.suppress_for_multicast,
                            })
                        } else if ip.fragment.is_some() {
                            FrontendDisposition::Control(ControlPacket::Fragment(ip))
                        } else {
                            FrontendDisposition::Drop(DropReason::UnsupportedIpProtocol)
                        };
                    }
                    Ok(_) => set_drop(packet, DropReason::NotLocal),
                    Err(reason) => set_drop(packet, reason),
                },
                _ => {}
            }
        }
    }

    #[cfg(test)]
    fn parse_transport(&self, batch: &mut FrontendBatch) {
        for index in 0..batch.len() {
            let Some(packet) = batch.packet_mut(index) else {
                continue;
            };
            if matches!(
                packet.parsed.disposition,
                FrontendDisposition::Control(ControlPacket::Ipv6ParameterProblem { .. })
            ) {
                continue;
            }
            let Some(ip) = packet.parsed.ip else {
                continue;
            };
            if ip.fragment.is_some() {
                continue;
            }
            match ip.next_header {
                TCP_PROTOCOL_NUMBER => match if packet.metadata.checksums_validated {
                    parse_tcp_packet_trusted(&packet.chain, ip)
                } else {
                    parse_tcp_packet(&packet.chain, ip)
                } {
                    Ok(tcp) => {
                        packet.parsed.tcp = Some(tcp);
                        packet.parsed.flow = FlowKey::new(
                            Endpoint {
                                addr: ip.source,
                                port: tcp.source_port,
                            },
                            Endpoint {
                                addr: ip.destination,
                                port: tcp.destination_port,
                            },
                            TransportProtocol::Tcp,
                        );
                        if packet.parsed.flow.is_some() {
                            packet.parsed.disposition = FrontendDisposition::Tcp;
                        } else {
                            set_drop(packet, DropReason::MalformedTcp);
                        }
                    }
                    Err(reason) => set_drop(packet, reason),
                },
                IP_PROTOCOL_UDP => {
                    match parse_udp(&packet.chain, ip, !packet.metadata.checksums_validated) {
                        Ok(udp) => {
                            packet.parsed.udp = Some(udp);
                            packet.parsed.flow = FlowKey::new(
                                Endpoint {
                                    addr: ip.source,
                                    port: udp.source_port,
                                },
                                Endpoint {
                                    addr: ip.destination,
                                    port: udp.destination_port,
                                },
                                TransportProtocol::Udp,
                            );
                            if packet.parsed.flow.is_some() {
                                packet.parsed.disposition = FrontendDisposition::Udp;
                            } else {
                                set_drop(packet, DropReason::MalformedUdp);
                            }
                        }
                        Err(reason) => set_drop(packet, reason),
                    }
                }
                IP_PROTOCOL_ICMP | IP_PROTOCOL_ICMPV6 => {
                    packet.parsed.disposition = FrontendDisposition::Control(ControlPacket::Icmp {
                        ipv6: ip.next_header == IP_PROTOCOL_ICMPV6,
                        packet_offset: ip.payload_offset,
                        packet_len: ip.payload_len,
                    });
                }
                _ => packet.parsed.disposition = FrontendDisposition::Raw,
            }
        }
    }

    #[cfg(test)]
    fn classify_hash(&self, batch: &mut FrontendBatch) {
        for index in 0..batch.len() {
            let Some(packet) = batch.packet_mut(index) else {
                continue;
            };
            let Some(flow) = packet.parsed.flow else {
                continue;
            };
            let hash = if packet.metadata.rss_generation == self.rss_generation {
                packet
                    .metadata
                    .rss_hash
                    .unwrap_or_else(|| rss_hash(&self.rss_key, &flow))
            } else {
                rss_hash(&self.rss_key, &flow)
            };
            packet.parsed.rss_hash = Some(hash);
        }
    }
}

fn apply_transport_sidecar(packet: &mut FrontendPacket, sidecar: NetStackTransport) {
    match sidecar.outcome {
        NET_STACK_TRANSPORT_SKIPPED => {}
        NET_STACK_TRANSPORT_DROP => set_drop(packet, transport_drop_reason(sidecar.drop_reason)),
        NET_STACK_TRANSPORT_TCP => {
            let ip = packet
                .parsed
                .ip
                .expect("TCP sidecar 必须对应已提交的 IP sidecar");
            let tcp = TcpPacket {
                source_port: sidecar.source_port,
                destination_port: sidecar.destination_port,
                sequence: TcpSequence(sidecar.tcp_sequence),
                acknowledgement: TcpSequence(sidecar.tcp_acknowledgement),
                flags: TcpFlags::from_bits(sidecar.tcp_flags),
                window: sidecar.tcp_window,
                urgent_pointer: sidecar.tcp_urgent_pointer,
                header_len: sidecar.header_len,
                payload_offset: sidecar.payload_offset,
                payload_len: sidecar.payload_len,
                options: tcp_options(sidecar.tcp_options),
            };
            packet.parsed.tcp = Some(tcp);
            packet.parsed.flow = FlowKey::new(
                Endpoint {
                    addr: ip.source,
                    port: tcp.source_port,
                },
                Endpoint {
                    addr: ip.destination,
                    port: tcp.destination_port,
                },
                TransportProtocol::Tcp,
            );
            packet.parsed.rss_hash = Some(sidecar.rss_hash);
            packet.parsed.disposition = FrontendDisposition::Tcp;
            packet.metadata.checksums_validated = true;
        }
        NET_STACK_TRANSPORT_UDP => {
            let ip = packet
                .parsed
                .ip
                .expect("UDP sidecar 必须对应已提交的 IP sidecar");
            let udp = UdpPacket {
                source_port: sidecar.source_port,
                destination_port: sidecar.destination_port,
                payload_offset: sidecar.payload_offset,
                payload_len: sidecar.payload_len as u16,
            };
            packet.parsed.udp = Some(udp);
            packet.parsed.flow = FlowKey::new(
                Endpoint {
                    addr: ip.source,
                    port: udp.source_port,
                },
                Endpoint {
                    addr: ip.destination,
                    port: udp.destination_port,
                },
                TransportProtocol::Udp,
            );
            packet.parsed.rss_hash = Some(sidecar.rss_hash);
            packet.parsed.disposition = FrontendDisposition::Udp;
            packet.metadata.checksums_validated = true;
        }
        NET_STACK_TRANSPORT_ICMP => {
            packet.parsed.disposition = FrontendDisposition::Control(ControlPacket::Icmp {
                ipv6: sidecar.protocol == IP_PROTOCOL_ICMPV6,
                packet_offset: sidecar.payload_offset,
                packet_len: sidecar.payload_len,
            });
            packet.metadata.checksums_validated = true;
        }
        NET_STACK_TRANSPORT_RAW => packet.parsed.disposition = FrontendDisposition::Raw,
        _ => unreachable!("worker-turn sidecar 必须在移动 packet 前完成校验"),
    }
}

fn tcp_options(sidecar: NetStackTcpOptions) -> TcpOptions {
    let mut sack_blocks = [None; 4];
    for (index, block) in sack_blocks
        .iter_mut()
        .enumerate()
        .take(usize::from(sidecar.sack_count))
    {
        *block = Some(TcpSackBlock {
            left: TcpSequence(sidecar.sack_left[index]),
            right: TcpSequence(sidecar.sack_right[index]),
        });
    }
    TcpOptions {
        maximum_segment_size: (sidecar.flags & NET_STACK_TCP_OPTION_MSS != 0)
            .then_some(sidecar.maximum_segment_size),
        window_scale: (sidecar.flags & NET_STACK_TCP_OPTION_WINDOW_SCALE != 0)
            .then_some(sidecar.window_scale),
        sack_permitted: sidecar.flags & NET_STACK_TCP_OPTION_SACK_PERMITTED != 0,
        sack_blocks,
        fastopen_cookie: None,
        timestamp: (sidecar.flags & NET_STACK_TCP_OPTION_TIMESTAMP != 0).then_some(TcpTimestamp {
            value: sidecar.timestamp_value,
            echo_reply: sidecar.timestamp_echo_reply,
        }),
    }
}

fn apply_network_sidecar(packet: &mut FrontendPacket, sidecar: NetStackNetwork) {
    match sidecar.outcome {
        NET_STACK_NETWORK_SKIPPED => {}
        NET_STACK_NETWORK_DROP => set_drop(packet, network_drop_reason(sidecar.drop_reason)),
        NET_STACK_NETWORK_ARP => {
            packet.parsed.disposition =
                FrontendDisposition::Control(ControlPacket::Arp(ArpPacket {
                    operation: sidecar.arp_operation,
                    sender_mac: sidecar.arp_sender_mac,
                    sender_ip: Ipv4Addr(sidecar.source[..4].try_into().unwrap()),
                    target_mac: sidecar.arp_target_mac,
                    target_ip: Ipv4Addr(sidecar.destination[..4].try_into().unwrap()),
                }));
        }
        NET_STACK_NETWORK_IP => {
            let source = network_ip(sidecar.family, sidecar.source);
            let destination = network_ip(sidecar.family, sidecar.destination);
            let fragment =
                (sidecar.flags & NET_STACK_NETWORK_FLAG_FRAGMENT != 0).then_some(IpFragment {
                    identification: sidecar.fragment_identification,
                    offset: sidecar.fragment_offset,
                    more: sidecar.flags & NET_STACK_NETWORK_FLAG_MORE_FRAGMENTS != 0,
                });
            let ip = IpPacket {
                source,
                destination,
                next_header: sidecar.next_header,
                header_len: sidecar.header_len,
                payload_offset: sidecar.payload_offset,
                payload_len: sidecar.payload_len,
                hop_limit: sidecar.hop_limit,
                traffic_class: sidecar.traffic_class,
                fragment,
            };
            packet.parsed.ip = Some(ip);
            packet.parsed.disposition = if sidecar.flags & NET_STACK_NETWORK_FLAG_IPV6_PROBLEM != 0
            {
                FrontendDisposition::Control(ControlPacket::Ipv6ParameterProblem {
                    pointer: sidecar.problem_pointer,
                    suppress_for_multicast: sidecar.flags
                        & NET_STACK_NETWORK_FLAG_SUPPRESS_MULTICAST
                        != 0,
                })
            } else if fragment.is_some() {
                FrontendDisposition::Control(ControlPacket::Fragment(ip))
            } else {
                FrontendDisposition::Drop(DropReason::UnsupportedIpProtocol)
            };
        }
        _ => unreachable!("worker-turn sidecar 必须在移动 packet 前完成校验"),
    }
}

fn network_ip(family: u8, address: [u8; 16]) -> IpAddr {
    match family {
        NET_STACK_ADDRESS_FAMILY_IPV4 => IpAddr::V4(Ipv4Addr(address[..4].try_into().unwrap())),
        NET_STACK_ADDRESS_FAMILY_IPV6 => IpAddr::V6(Ipv6Addr(address)),
        _ => unreachable!("worker-turn sidecar 必须在移动 packet 前完成校验"),
    }
}

fn network_drop_reason(reason: u8) -> DropReason {
    match reason {
        NET_STACK_DROP_MALFORMED_ARP => DropReason::MalformedArp,
        NET_STACK_DROP_NOT_LOCAL => DropReason::NotLocal,
        NET_STACK_DROP_MALFORMED_IPV4 => DropReason::MalformedIpv4,
        NET_STACK_DROP_IPV4_CHECKSUM => DropReason::Ipv4Checksum,
        NET_STACK_DROP_MALFORMED_IPV6 => DropReason::MalformedIpv6,
        NET_STACK_DROP_IPV6_EXTENSION_LIMIT => DropReason::Ipv6ExtensionLimit,
        NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL => DropReason::UnsupportedIpProtocol,
        _ => unreachable!("worker-turn sidecar 必须在移动 packet 前完成校验"),
    }
}

fn transport_drop_reason(reason: u8) -> DropReason {
    match reason {
        NET_STACK_DROP_MALFORMED_IPV6 => DropReason::MalformedIpv6,
        NET_STACK_DROP_UNSUPPORTED_IP_PROTOCOL => DropReason::UnsupportedIpProtocol,
        NET_STACK_DROP_MALFORMED_UDP => DropReason::MalformedUdp,
        NET_STACK_DROP_UDP_CHECKSUM => DropReason::UdpChecksum,
        NET_STACK_DROP_MALFORMED_TCP => DropReason::MalformedTcp,
        NET_STACK_DROP_TCP_CHECKSUM => DropReason::TcpChecksum,
        _ => unreachable!("worker-turn sidecar 必须在移动 packet 前完成校验"),
    }
}

fn set_drop(packet: &mut FrontendPacket, reason: DropReason) {
    packet.metadata.drop_reason = reason;
    packet.parsed.disposition = FrontendDisposition::Drop(reason);
}

#[cfg(test)]
fn parse_arp(chain: &PacketChain) -> Result<ArpPacket, DropReason> {
    let mut bytes = [0u8; 28];
    chain
        .copy_out(14, &mut bytes)
        .map_err(|_| DropReason::MalformedArp)?;
    if u16::from_be_bytes([bytes[0], bytes[1]]) != 1
        || u16::from_be_bytes([bytes[2], bytes[3]]) != ETHERTYPE_IPV4
        || bytes[4] != 6
        || bytes[5] != 4
    {
        return Err(DropReason::MalformedArp);
    }
    let operation = u16::from_be_bytes([bytes[6], bytes[7]]);
    if operation != 1 && operation != 2 {
        return Err(DropReason::MalformedArp);
    }
    Ok(ArpPacket {
        operation,
        sender_mac: bytes[8..14].try_into().unwrap(),
        sender_ip: Ipv4Addr(bytes[14..18].try_into().unwrap()),
        target_mac: bytes[18..24].try_into().unwrap(),
        target_ip: Ipv4Addr(bytes[24..28].try_into().unwrap()),
    })
}

#[cfg(test)]
fn parse_ipv4(chain: &PacketChain, verify_checksum: bool) -> Result<IpPacket, DropReason> {
    let mut base = [0u8; 20];
    chain
        .copy_out(14, &mut base)
        .map_err(|_| DropReason::MalformedIpv4)?;
    if base[0] >> 4 != 4 {
        return Err(DropReason::MalformedIpv4);
    }
    let header_len = usize::from(base[0] & 0x0f) * 4;
    let total_len = usize::from(u16::from_be_bytes([base[2], base[3]]));
    if !(20..=60).contains(&header_len)
        || total_len < header_len
        || 14usize.saturating_add(total_len) > chain.total_len()
    {
        return Err(DropReason::MalformedIpv4);
    }
    if verify_checksum
        && checksum_packet(chain, 14, header_len, &[]).map_err(|_| DropReason::MalformedIpv4)? != 0
    {
        return Err(DropReason::Ipv4Checksum);
    }
    let fragment_field = u16::from_be_bytes([base[6], base[7]]);
    let fragment_offset = fragment_field & 0x1fff;
    let more = fragment_field & 0x2000 != 0;
    let fragment = (fragment_offset != 0 || more).then_some(IpFragment {
        identification: u32::from(u16::from_be_bytes([base[4], base[5]])),
        offset: fragment_offset,
        more,
    });
    Ok(IpPacket {
        source: IpAddr::V4(Ipv4Addr(base[12..16].try_into().unwrap())),
        destination: IpAddr::V4(Ipv4Addr(base[16..20].try_into().unwrap())),
        next_header: base[9],
        header_len: header_len as u16,
        payload_offset: (14 + header_len) as u16,
        payload_len: (total_len - header_len) as u32,
        hop_limit: base[8],
        traffic_class: base[1],
        fragment,
    })
}

#[cfg(test)]
fn parse_ipv6(chain: &PacketChain) -> Result<(IpPacket, Option<Ipv6OptionProblem>), DropReason> {
    let mut base = [0u8; 40];
    chain
        .copy_out(14, &mut base)
        .map_err(|_| DropReason::MalformedIpv6)?;
    if base[0] >> 4 != 6 {
        return Err(DropReason::MalformedIpv6);
    }
    let payload_len = usize::from(u16::from_be_bytes([base[4], base[5]]));
    if 54usize.saturating_add(payload_len) > chain.total_len() {
        return Err(DropReason::MalformedIpv6);
    }
    let source = IpAddr::V6(Ipv6Addr(base[8..24].try_into().unwrap()));
    let destination = IpAddr::V6(Ipv6Addr(base[24..40].try_into().unwrap()));
    let mut next_header = base[6];
    let mut offset = 54usize;
    let end = 54 + payload_len;
    let mut extension_count = 0usize;
    let mut extension_bytes = 0usize;
    let mut fragment = None;
    let mut option_problem = None;
    loop {
        match next_header {
            0 | 60 => {
                extension_count += 1;
                let mut extension = [0u8; 2];
                chain
                    .copy_out(offset, &mut extension)
                    .map_err(|_| DropReason::MalformedIpv6)?;
                let len = (usize::from(extension[1]) + 1) * 8;
                if offset.saturating_add(len) > end {
                    return Err(DropReason::MalformedIpv6);
                }
                match validate_ipv6_options(chain, offset + 2, len - 2) {
                    Ok(()) => {}
                    Err(Ipv6OptionError::Malformed) => {
                        return Err(DropReason::MalformedIpv6);
                    }
                    Err(Ipv6OptionError::Silent) => {
                        return Err(DropReason::UnsupportedIpProtocol);
                    }
                    Err(Ipv6OptionError::Parameter(problem)) => {
                        option_problem = Some(problem);
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
                    return Err(DropReason::Ipv6ExtensionLimit);
                }
                let mut header = [0u8; 8];
                chain
                    .copy_out(offset, &mut header)
                    .map_err(|_| DropReason::MalformedIpv6)?;
                let field = u16::from_be_bytes([header[2], header[3]]);
                let offset_field = field >> 3;
                let more = field & 1 != 0;
                fragment = (offset_field != 0 || more).then_some(IpFragment {
                    identification: u32::from_be_bytes(header[4..8].try_into().unwrap()),
                    offset: offset_field,
                    more,
                });
                next_header = header[0];
                offset += 8;
                break;
            }
            43 | 50 | 51 => break,
            _ => break,
        }
        if extension_count > 8 || extension_bytes > 256 {
            return Err(DropReason::Ipv6ExtensionLimit);
        }
    }
    if offset > end {
        return Err(DropReason::MalformedIpv6);
    }
    Ok((
        IpPacket {
            source,
            destination,
            next_header,
            header_len: (offset - 14) as u16,
            payload_offset: offset as u16,
            payload_len: (end - offset) as u32,
            hop_limit: base[7],
            traffic_class: ((u16::from(base[0] & 0x0f) << 4) | u16::from(base[1] >> 4)) as u8,
            fragment,
        },
        option_problem,
    ))
}

#[cfg(test)]
enum Ipv6OptionError {
    Malformed,
    Silent,
    Parameter(Ipv6OptionProblem),
}

#[cfg(test)]
fn validate_ipv6_options(
    chain: &PacketChain,
    mut offset: usize,
    mut remaining: usize,
) -> Result<(), Ipv6OptionError> {
    while remaining != 0 {
        let mut kind = [0u8; 1];
        chain
            .copy_out(offset, &mut kind)
            .map_err(|_| Ipv6OptionError::Malformed)?;
        if kind[0] == 0 {
            offset += 1;
            remaining -= 1;
            continue;
        }
        if remaining < 2 {
            return Err(Ipv6OptionError::Malformed);
        }
        let mut header = [0u8; 2];
        chain
            .copy_out(offset, &mut header)
            .map_err(|_| Ipv6OptionError::Malformed)?;
        let option_len = usize::from(header[1]) + 2;
        if option_len > remaining {
            return Err(Ipv6OptionError::Malformed);
        }
        if header[0] != 1 {
            match header[0] >> 6 {
                0 => {}
                1 => return Err(Ipv6OptionError::Silent),
                2 => {
                    return Err(Ipv6OptionError::Parameter(Ipv6OptionProblem {
                        pointer: offset.saturating_sub(14) as u32,
                        suppress_for_multicast: false,
                    }));
                }
                _ => {
                    return Err(Ipv6OptionError::Parameter(Ipv6OptionProblem {
                        pointer: offset.saturating_sub(14) as u32,
                        suppress_for_multicast: true,
                    }));
                }
            }
        }
        offset += option_len;
        remaining -= option_len;
    }
    Ok(())
}

#[cfg(test)]
fn parse_udp(
    chain: &PacketChain,
    ip: IpPacket,
    verify_checksum: bool,
) -> Result<UdpPacket, DropReason> {
    if ip.payload_len < 8 {
        return Err(DropReason::MalformedUdp);
    }
    let mut header = [0u8; 8];
    chain
        .copy_out(usize::from(ip.payload_offset), &mut header)
        .map_err(|_| DropReason::MalformedUdp)?;
    let udp_len = usize::from(u16::from_be_bytes([header[4], header[5]]));
    if udp_len < 8 || udp_len > ip.payload_len as usize {
        return Err(DropReason::MalformedUdp);
    }
    let checksum = u16::from_be_bytes([header[6], header[7]]);
    if checksum == 0 && matches!(ip.source, IpAddr::V6(_)) {
        return Err(DropReason::UdpChecksum);
    }
    if verify_checksum && checksum != 0 && !verify_udp_checksum(chain, ip, udp_len)? {
        return Err(DropReason::UdpChecksum);
    }
    Ok(UdpPacket {
        source_port: u16::from_be_bytes([header[0], header[1]]),
        destination_port: u16::from_be_bytes([header[2], header[3]]),
        payload_offset: ip.payload_offset + 8,
        payload_len: (udp_len - 8) as u16,
    })
}

#[cfg(test)]
fn verify_udp_checksum(
    chain: &PacketChain,
    ip: IpPacket,
    udp_len: usize,
) -> Result<bool, DropReason> {
    let mut checksum = InternetChecksum::new();
    match (ip.source, ip.destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&[0, IP_PROTOCOL_UDP]);
            checksum.add(&(udp_len as u16).to_be_bytes());
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&(udp_len as u32).to_be_bytes());
            checksum.add(&[0, 0, 0, IP_PROTOCOL_UDP]);
        }
        _ => return Err(DropReason::MalformedUdp),
    }
    chain
        .for_each_slice(usize::from(ip.payload_offset), udp_len, |slice| {
            checksum.add(slice);
            Ok::<_, ()>(())
        })
        .map_err(|_| DropReason::MalformedUdp)?;
    Ok(checksum.finish() == 0)
}

#[cfg(test)]
fn is_local_ip(config: &ConfigSnapshot, interface: InterfaceId, address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_local_ipv4(config, interface, address),
        IpAddr::V6(address) => {
            address.is_multicast() || config.is_local_address(interface, IpAddr::V6(address))
        }
    }
}

#[cfg(test)]
fn is_local_ipv4(config: &ConfigSnapshot, interface: InterfaceId, address: Ipv4Addr) -> bool {
    address.is_broadcast()
        || address.is_multicast()
        || config.is_local_address(interface, IpAddr::V4(address))
        || config.addresses.iter().any(|entry| {
            let IpAddr::V4(local) = entry.address else {
                return false;
            };
            if entry.interface != interface || entry.prefix_len == 0 || entry.prefix_len == 32 {
                return false;
            }
            let mask = u32::MAX << (32 - entry.prefix_len);
            address.as_u32() == local.as_u32() | !mask
        })
}

pub fn checksum_bytes(bytes: &[u8]) -> u16 {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::NetChecksum).bytes(bytes.len());
    let mut checksum = InternetChecksum::new();
    checksum.add(bytes);
    checksum.finish()
}

pub fn packet_checksum(
    chain: &PacketChain,
    offset: usize,
    len: usize,
) -> Result<u16, NetBufPoolError> {
    checksum_packet(chain, offset, len, &[])
}

pub fn transport_checksum(
    chain: &PacketChain,
    offset: usize,
    len: usize,
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
) -> Result<u16, NetBufPoolError> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::NetChecksum).bytes(len);
    let mut checksum = InternetChecksum::new();
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            if len > u16::MAX as usize {
                return Err(NetBufPoolError::InvalidRange);
            }
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&[0, protocol]);
            checksum.add(&(len as u16).to_be_bytes());
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            if len > u32::MAX as usize {
                return Err(NetBufPoolError::InvalidRange);
            }
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&(len as u32).to_be_bytes());
            checksum.add(&[0, 0, 0, protocol]);
        }
        _ => return Err(NetBufPoolError::InvalidRange),
    }
    chain
        .for_each_slice(offset, len, |slice| {
            checksum.add(slice);
            Ok::<_, ()>(())
        })
        .map_err(|error| match error {
            crate::buf::PacketRangeError::Buffer(error) => error,
            _ => NetBufPoolError::InvalidRange,
        })?;
    Ok(checksum.finish())
}

/// 生成 CHECKSUM_PARTIAL 字段中预置的 pseudo-header 累加值。
///
/// 设备随后从 transport header 起累加至报文末尾并写回反码；这里返回的是未取反的
/// folded sum，因此不会扫描 payload。
pub fn partial_transport_checksum(
    source: IpAddr,
    destination: IpAddr,
    len: usize,
    protocol: u8,
) -> Result<u16, NetBufPoolError> {
    let mut checksum = InternetChecksum::new();
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let len = u16::try_from(len).map_err(|_| NetBufPoolError::InvalidRange)?;
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&[0, protocol]);
            checksum.add(&len.to_be_bytes());
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            let len = u32::try_from(len).map_err(|_| NetBufPoolError::InvalidRange)?;
            checksum.add(&source.0);
            checksum.add(&destination.0);
            checksum.add(&len.to_be_bytes());
            checksum.add(&[0, 0, 0, protocol]);
        }
        _ => return Err(NetBufPoolError::InvalidRange),
    }
    Ok(!checksum.finish())
}

fn checksum_packet(
    chain: &PacketChain,
    offset: usize,
    len: usize,
    prefix: &[&[u8]],
) -> Result<u16, NetBufPoolError> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::NetChecksum).bytes(len);
    let mut checksum = InternetChecksum::new();
    for bytes in prefix {
        checksum.add(bytes);
    }
    chain
        .for_each_slice(offset, len, |slice| {
            checksum.add(slice);
            Ok::<_, ()>(())
        })
        .map_err(|error| match error {
            crate::buf::PacketRangeError::Buffer(error) => error,
            _ => NetBufPoolError::InvalidRange,
        })?;
    Ok(checksum.finish())
}

struct InternetChecksum {
    sum: u64,
    odd: Option<u8>,
}

impl InternetChecksum {
    const fn new() -> Self {
        Self { sum: 0, odd: None }
    }

    fn add(&mut self, mut bytes: &[u8]) {
        if let Some(high) = self.odd.take() {
            if let Some((&low, rest)) = bytes.split_first() {
                self.sum += u64::from(u16::from_be_bytes([high, low]));
                bytes = rest;
            } else {
                self.odd = Some(high);
                return;
            }
        }
        let mut wide = bytes.chunks_exact(8);
        for chunk in &mut wide {
            self.sum += u64::from(u16::from_be_bytes([chunk[0], chunk[1]]))
                + u64::from(u16::from_be_bytes([chunk[2], chunk[3]]))
                + u64::from(u16::from_be_bytes([chunk[4], chunk[5]]))
                + u64::from(u16::from_be_bytes([chunk[6], chunk[7]]));
        }
        let mut words = wide.remainder().chunks_exact(2);
        for chunk in &mut words {
            self.sum += u64::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        }
        if let Some(&last) = words.remainder().first() {
            self.odd = Some(last);
        }
    }

    fn finish(mut self) -> u16 {
        if let Some(high) = self.odd {
            self.sum += u64::from(u16::from_be_bytes([high, 0]));
        }
        while self.sum >> 16 != 0 {
            self.sum = (self.sum & 0xffff) + (self.sum >> 16);
        }
        !(self.sum as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn frontend_batch_keeps_packet_storage_off_stack() {
        assert!(core::mem::size_of::<FrontendBatch>() <= 4 * core::mem::size_of::<usize>());
        assert_eq!(FrontendBatch::new().packets.len(), PACKET_BATCH_CAPACITY);
    }

    fn reference_checksum(bytes: &[u8]) -> u16 {
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

    #[test]
    fn checksum_unrolling_matches_reference_across_fragment_boundaries() {
        let bytes = (0..257)
            .map(|index| (index as u8).wrapping_mul(37).wrapping_add(11))
            .collect::<Vec<_>>();
        for len in 0..=bytes.len() {
            assert_eq!(
                checksum_bytes(&bytes[..len]),
                reference_checksum(&bytes[..len])
            );
            for split in [0, 1, 2, 3, 7, 8, 9, len / 2, len] {
                let split = split.min(len);
                let mut checksum = InternetChecksum::new();
                checksum.add(&bytes[..split]);
                checksum.add(&bytes[split..len]);
                assert_eq!(checksum.finish(), reference_checksum(&bytes[..len]));
            }
        }
    }

    #[test]
    fn partial_checksum_seed_matches_full_transport_checksum() {
        let cases = [
            (
                IpAddr::V4(crate::Ipv4Addr::new(10, 0, 2, 15)),
                IpAddr::V4(crate::Ipv4Addr::new(10, 0, 2, 2)),
            ),
            (
                IpAddr::V6(crate::Ipv6Addr::LOCALHOST),
                IpAddr::V6(crate::Ipv6Addr([
                    0x20, 1, 0xdb, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
                ])),
            ),
        ];
        for (source, destination) in cases {
            let mut transport = alloc::vec![0u8; 8 + 257];
            let transport_len = transport.len() as u16;
            transport[0..2].copy_from_slice(&1000u16.to_be_bytes());
            transport[2..4].copy_from_slice(&9000u16.to_be_bytes());
            transport[4..6].copy_from_slice(&transport_len.to_be_bytes());
            for (index, byte) in transport[8..].iter_mut().enumerate() {
                *byte = index.wrapping_mul(17) as u8;
            }
            let seed = partial_transport_checksum(source, destination, transport.len(), 17)
                .expect("地址族匹配");
            transport[6..8].copy_from_slice(&seed.to_be_bytes());
            let device_checksum = checksum_bytes(&transport);
            transport[6..8].copy_from_slice(
                &(if device_checksum == 0 {
                    0xffff
                } else {
                    device_checksum
                })
                .to_be_bytes(),
            );
            let chain = PacketChain::from_owned(transport);
            assert_eq!(
                transport_checksum(&chain, 0, chain.total_len(), source, destination, 17),
                Ok(0)
            );
        }
    }
    use core::ptr::NonNull;

    use crate::NetDeviceId;
    use crate::buf::{NetBufPool, NetBufStorage};
    use crate::control::{AddressEntry, InterfaceSnapshot, RouteEntry};
    use crate::transport::{TcpFlags, TcpSequence};

    struct Storage {
        bytes: Box<[u8]>,
    }

    impl NetBufStorage for Storage {
        fn capacity(&self) -> usize {
            self.bytes.len()
        }

        fn base_ptr(&self) -> NonNull<u8> {
            NonNull::new(self.bytes.as_ptr() as *mut u8).unwrap()
        }

        fn dma_addr(&self) -> Option<u64> {
            None
        }

        fn sync_for_cpu(&self, _offset: usize, _len: usize) {}
        fn sync_for_device(&self, _offset: usize, _len: usize) {}
    }

    fn config() -> ConfigSnapshot {
        ConfigSnapshot::new(
            1,
            vec![InterfaceSnapshot {
                id: InterfaceId(1),
                device: NetDeviceId(1),
                mac_address: [2; 6],
                mtu: 1500,
                running: true,
                loopback: false,
            }],
            vec![AddressEntry {
                interface: InterfaceId(1),
                address: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
                prefix_len: 24,
                primary: true,
            }],
            vec![RouteEntry {
                table: 0,
                network: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 0)),
                prefix_len: 24,
                gateway: None,
                interface: InterfaceId(1),
                metric: 0,
                mtu: None,
            }],
            vec![],
        )
        .unwrap()
    }

    fn udp_frame() -> [u8; 46] {
        let mut frame = [0u8; 46];
        frame[0..6].copy_from_slice(&[2; 6]);
        frame[6..12].copy_from_slice(&[1; 6]);
        frame[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&32u16.to_be_bytes());
        frame[22] = 64;
        frame[23] = IP_PROTOCOL_UDP;
        frame[26..30].copy_from_slice(&[10, 0, 2, 2]);
        frame[30..34].copy_from_slice(&[10, 0, 2, 15]);
        let ip_checksum = checksum_bytes(&frame[14..34]);
        frame[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
        frame[34..36].copy_from_slice(&1000u16.to_be_bytes());
        frame[36..38].copy_from_slice(&9000u16.to_be_bytes());
        frame[38..40].copy_from_slice(&12u16.to_be_bytes());
        frame[42..46].copy_from_slice(b"test");
        frame
    }

    fn tcp_frame() -> [u8; 54] {
        let source = Ipv4Addr::new(10, 0, 2, 2);
        let destination = Ipv4Addr::new(10, 0, 2, 15);
        let mut frame = [0u8; 54];
        frame[0..6].copy_from_slice(&[2; 6]);
        frame[6..12].copy_from_slice(&[1; 6]);
        frame[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&40u16.to_be_bytes());
        frame[22] = 64;
        frame[23] = TCP_PROTOCOL_NUMBER;
        frame[26..30].copy_from_slice(&source.0);
        frame[30..34].copy_from_slice(&destination.0);
        frame[34..36].copy_from_slice(&1000u16.to_be_bytes());
        frame[36..38].copy_from_slice(&9000u16.to_be_bytes());
        frame[38..42].copy_from_slice(&123u32.to_be_bytes());
        frame[46] = 5 << 4;
        frame[47] = TcpFlags::SYN.bits() as u8;
        frame[48..50].copy_from_slice(&32768u16.to_be_bytes());

        let mut tcp_checksum = InternetChecksum::new();
        tcp_checksum.add(&source.0);
        tcp_checksum.add(&destination.0);
        tcp_checksum.add(&[0, TCP_PROTOCOL_NUMBER]);
        tcp_checksum.add(&20u16.to_be_bytes());
        tcp_checksum.add(&frame[34..54]);
        frame[50..52].copy_from_slice(&tcp_checksum.finish().to_be_bytes());
        let ip_checksum = checksum_bytes(&frame[14..34]);
        frame[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
        frame
    }

    #[test]
    fn batch_frontend_parses_ipv4_udp_without_linearizing() {
        let storage = (0..2)
            .map(|_| {
                Box::new(Storage {
                    bytes: vec![0; 128].into_boxed_slice(),
                }) as Box<dyn NetBufStorage>
            })
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        let frame = udp_frame();
        let mut first = owner.lease(0, 23, PacketMetadata::default()).unwrap();
        first.as_mut_slice().unwrap().copy_from_slice(&frame[..23]);
        let mut second = owner.lease(0, 23, PacketMetadata::default()).unwrap();
        second.as_mut_slice().unwrap().copy_from_slice(&frame[23..]);
        let mut chain = PacketChain::from_lease(first);
        assert!(
            chain
                .push(crate::buf::PacketFragment::Exclusive(second))
                .is_ok()
        );
        let mut input = PacketBatch::new();
        assert!(input.push(chain, PacketMetadata::default()).is_ok());
        let mut output = FrontendBatch::new();
        VectorFrontend::new([1; 40], 1).process(InterfaceId(1), &config(), &mut input, &mut output);
        let packet = output.packet(0).unwrap();
        assert_eq!(packet.parsed.disposition, FrontendDisposition::Udp);
        assert_eq!(packet.parsed.udp.unwrap().payload_len, 4);
        assert!(packet.parsed.rss_hash.is_some());
    }

    #[test]
    fn batch_frontend_reuses_only_current_generation_hardware_rss_hash() {
        let key = [1; 40];
        let mut matching = PacketBatch::new();
        matching
            .push(
                PacketChain::from_owned(udp_frame().to_vec()),
                PacketMetadata {
                    rss_hash: Some(0x1234_5678),
                    rss_generation: 7,
                    ..PacketMetadata::default()
                },
            )
            .unwrap_or_else(|_| unreachable!());
        let mut output = FrontendBatch::new();
        VectorFrontend::new(key, 7).process(InterfaceId(1), &config(), &mut matching, &mut output);
        assert_eq!(output.packet(0).unwrap().parsed.rss_hash, Some(0x1234_5678));

        let mut stale = PacketBatch::new();
        stale
            .push(
                PacketChain::from_owned(udp_frame().to_vec()),
                PacketMetadata {
                    rss_hash: Some(0x1234_5678),
                    rss_generation: 6,
                    ..PacketMetadata::default()
                },
            )
            .unwrap_or_else(|_| unreachable!());
        VectorFrontend::new(key, 7).process(InterfaceId(1), &config(), &mut stale, &mut output);
        let packet = output.packet(0).unwrap();
        assert_eq!(
            packet.parsed.rss_hash,
            Some(rss_hash(&key, &packet.parsed.flow.unwrap()))
        );
    }

    #[test]
    fn worker_turn_l2_l3_l4_sidecars_are_not_reparsed_by_host() {
        let mut frame = udp_frame();
        frame[12..14].copy_from_slice(&0xffffu16.to_be_bytes());
        frame[14] = 0;
        frame[38..40].fill(0);
        let mut input = PacketBatch::new();
        input
            .push(
                PacketChain::from_owned(frame.to_vec()),
                PacketMetadata {
                    checksums_validated: true,
                    ..PacketMetadata::default()
                },
            )
            .unwrap_or_else(|_| unreachable!());
        let ethernet = NetStackEthernet {
            destination: [2; 6],
            source: [1; 6],
            ethertype: ETHERTYPE_IPV4,
            status: NET_STACK_ETHERNET_ACCEPTED,
            reserved: [0; 5],
        };
        let mut source = [0; 16];
        source[..4].copy_from_slice(&[10, 0, 2, 2]);
        let mut destination = [0; 16];
        destination[..4].copy_from_slice(&[10, 0, 2, 15]);
        let network = NetStackNetwork {
            outcome: NET_STACK_NETWORK_IP,
            family: NET_STACK_ADDRESS_FAMILY_IPV4,
            next_header: IP_PROTOCOL_UDP,
            flags: 0,
            drop_reason: 0,
            traffic_class: 0,
            hop_limit: 64,
            reserved0: 0,
            header_len: 20,
            payload_offset: 34,
            fragment_offset: 0,
            arp_operation: 0,
            payload_len: 12,
            fragment_identification: 0,
            problem_pointer: 0,
            source,
            destination,
            arp_sender_mac: [0; 6],
            arp_target_mac: [0; 6],
            reserved1: [0; 8],
        };
        let transport = NetStackTransport {
            outcome: NET_STACK_TRANSPORT_UDP,
            protocol: IP_PROTOCOL_UDP,
            drop_reason: 0,
            reserved0: 0,
            source_port: 1000,
            destination_port: 9000,
            header_len: 8,
            payload_offset: 42,
            tcp_flags: 0,
            tcp_window: 0,
            tcp_urgent_pointer: 0,
            reserved1: 0,
            payload_len: 4,
            rss_hash: 123,
            tcp_sequence: 0,
            tcp_acknowledgement: 0,
            tcp_options: NetStackTcpOptions::empty(),
            reserved2: [0; 2],
        };
        let mut output = FrontendBatch::new();
        VectorFrontend::new([1; 40], 1).process_with_stack_sidecars(
            &mut input,
            &[ethernet],
            &[network],
            &[transport],
            &mut output,
        );
        assert_eq!(
            output.packet(0).unwrap().parsed.disposition,
            FrontendDisposition::Udp
        );
        assert_eq!(output.packet(0).unwrap().parsed.rss_hash, Some(123));
        assert!(input.is_empty());
    }

    #[test]
    fn worker_turn_tcp_options_are_rebuilt_without_host_reparse() {
        let mut frame = tcp_frame().to_vec();
        frame.extend_from_slice(&[0; 4]);
        frame[12..14].copy_from_slice(&0xffffu16.to_be_bytes());
        frame[14] = 0;
        frame[46] = 0;
        frame[50..52].fill(0);
        let mut input = PacketBatch::new();
        input
            .push(PacketChain::from_owned(frame), PacketMetadata::default())
            .unwrap_or_else(|_| unreachable!());
        let ethernet = NetStackEthernet {
            destination: [2; 6],
            source: [1; 6],
            ethertype: ETHERTYPE_IPV4,
            status: NET_STACK_ETHERNET_ACCEPTED,
            reserved: [0; 5],
        };
        let mut source = [0; 16];
        source[..4].copy_from_slice(&[10, 0, 2, 2]);
        let mut destination = [0; 16];
        destination[..4].copy_from_slice(&[10, 0, 2, 15]);
        let network = NetStackNetwork {
            outcome: NET_STACK_NETWORK_IP,
            family: NET_STACK_ADDRESS_FAMILY_IPV4,
            next_header: TCP_PROTOCOL_NUMBER,
            flags: 0,
            drop_reason: 0,
            traffic_class: 0,
            hop_limit: 64,
            reserved0: 0,
            header_len: 20,
            payload_offset: 34,
            fragment_offset: 0,
            arp_operation: 0,
            payload_len: 24,
            fragment_identification: 0,
            problem_pointer: 0,
            source,
            destination,
            arp_sender_mac: [0; 6],
            arp_target_mac: [0; 6],
            reserved1: [0; 8],
        };
        let transport = NetStackTransport {
            outcome: NET_STACK_TRANSPORT_TCP,
            protocol: TCP_PROTOCOL_NUMBER,
            source_port: 1000,
            destination_port: 9000,
            header_len: 24,
            payload_offset: 58,
            tcp_flags: TcpFlags::SYN.bits(),
            tcp_window: 32768,
            payload_len: 0,
            rss_hash: 456,
            tcp_sequence: 123,
            tcp_options: NetStackTcpOptions {
                flags: NET_STACK_TCP_OPTION_MSS,
                maximum_segment_size: 1460,
                ..NetStackTcpOptions::empty()
            },
            ..NetStackTransport::empty()
        };
        assert!(transport.valid(58, &network));

        let mut output = FrontendBatch::new();
        VectorFrontend::new([1; 40], 1).process_with_stack_sidecars(
            &mut input,
            &[ethernet],
            &[network],
            &[transport],
            &mut output,
        );
        let packet = output.packet(0).unwrap();
        assert_eq!(packet.parsed.disposition, FrontendDisposition::Tcp);
        assert_eq!(packet.parsed.rss_hash, Some(456));
        assert_eq!(
            packet.parsed.tcp.unwrap().options.maximum_segment_size,
            Some(1460)
        );
        assert!(input.is_empty());
    }

    #[test]
    fn batch_frontend_classifies_ipv4_tcp_without_linearizing() {
        let storage = (0..2)
            .map(|_| {
                Box::new(Storage {
                    bytes: vec![0; 128].into_boxed_slice(),
                }) as Box<dyn NetBufStorage>
            })
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        let frame = tcp_frame();
        let mut first = owner.lease(0, 37, PacketMetadata::default()).unwrap();
        first.as_mut_slice().unwrap().copy_from_slice(&frame[..37]);
        let mut second = owner
            .lease(0, (frame.len() - 37) as u16, PacketMetadata::default())
            .unwrap();
        second.as_mut_slice().unwrap().copy_from_slice(&frame[37..]);
        let mut chain = PacketChain::from_lease(first);
        assert!(
            chain
                .push(crate::buf::PacketFragment::Exclusive(second))
                .is_ok()
        );
        let mut input = PacketBatch::new();
        assert!(input.push(chain, PacketMetadata::default()).is_ok());
        let mut output = FrontendBatch::new();
        VectorFrontend::new([1; 40], 1).process(InterfaceId(1), &config(), &mut input, &mut output);
        let packet = output.packet(0).unwrap();
        assert_eq!(packet.parsed.disposition, FrontendDisposition::Tcp);
        assert_eq!(packet.parsed.tcp.unwrap().sequence, TcpSequence(123));
        assert_eq!(packet.parsed.flow.unwrap().protocol, TransportProtocol::Tcp);
        assert!(packet.parsed.rss_hash.is_some());
    }

    #[test]
    fn trusted_local_packet_skips_checksum_verification() {
        let mut frame = tcp_frame();
        frame[24] ^= 0xff;
        frame[50] ^= 0xff;
        let mut input = PacketBatch::new();
        assert!(
            input
                .push(
                    PacketChain::from_owned(frame.to_vec()),
                    PacketMetadata {
                        checksums_validated: true,
                        ..PacketMetadata::default()
                    },
                )
                .is_ok()
        );
        let mut output = FrontendBatch::new();
        VectorFrontend::new([1; 40], 1).process(InterfaceId(1), &config(), &mut input, &mut output);
        assert_eq!(
            output.packet(0).unwrap().parsed.disposition,
            FrontendDisposition::Tcp
        );
    }

    #[test]
    fn vlan_is_rejected_with_stable_reason() {
        let storage = vec![Box::new(Storage {
            bytes: vec![0; 64].into_boxed_slice(),
        }) as Box<dyn NetBufStorage>]
        .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        let mut lease = owner.lease(0, 14, PacketMetadata::default()).unwrap();
        lease.as_mut_slice().unwrap()[12..14].copy_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
        let mut input = PacketBatch::new();
        assert!(
            input
                .push(PacketChain::from_lease(lease), PacketMetadata::default())
                .is_ok()
        );
        let mut output = FrontendBatch::new();
        VectorFrontend::new([0; 40], 1).process(InterfaceId(1), &config(), &mut input, &mut output);
        assert_eq!(
            output.packet(0).unwrap().parsed.disposition,
            FrontendDisposition::Drop(DropReason::VlanUnsupported)
        );
    }

    #[test]
    fn ipv6_unknown_option_action_requests_parameter_problem() {
        let source = Ipv6Addr([0x20, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let destination = Ipv6Addr([0x20, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let mut frame = alloc::vec![0; 14 + 40 + 8 + 8];
        frame[12..14].copy_from_slice(&ETHERTYPE_IPV6.to_be_bytes());
        frame[14..18].copy_from_slice(&0x6000_0000u32.to_be_bytes());
        frame[18..20].copy_from_slice(&16u16.to_be_bytes());
        frame[20] = 0;
        frame[21] = 64;
        frame[22..38].copy_from_slice(&source.0);
        frame[38..54].copy_from_slice(&destination.0);
        frame[54] = IP_PROTOCOL_UDP;
        frame[56] = 0x80;
        frame[57] = 0;
        let config = ConfigSnapshot::new(
            1,
            alloc::vec![InterfaceSnapshot {
                id: InterfaceId(1),
                device: NetDeviceId(1),
                mac_address: [2; 6],
                mtu: 1500,
                running: true,
                loopback: false,
            }],
            alloc::vec![AddressEntry {
                interface: InterfaceId(1),
                address: IpAddr::V6(destination),
                prefix_len: 64,
                primary: true,
            }],
            alloc::vec![],
            alloc::vec![],
        )
        .unwrap();
        let mut input = PacketBatch::new();
        assert!(
            input
                .push(PacketChain::from_owned(frame), PacketMetadata::default())
                .is_ok()
        );
        let mut output = FrontendBatch::new();
        VectorFrontend::new([3; 40], 1).process(InterfaceId(1), &config, &mut input, &mut output);
        assert_eq!(
            output.packet(0).unwrap().parsed.disposition,
            FrontendDisposition::Control(ControlPacket::Ipv6ParameterProblem {
                pointer: 42,
                suppress_for_multicast: false,
            })
        );
    }

    #[test]
    fn every_ipv4_udp_truncation_is_bounded() {
        let frame = udp_frame();
        let storage = (0..frame.len())
            .map(|_| {
                Box::new(Storage {
                    bytes: vec![0; 64].into_boxed_slice(),
                }) as Box<dyn NetBufStorage>
            })
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        for len in 0..frame.len() {
            let mut lease = owner
                .lease(0, len as u16, PacketMetadata::default())
                .unwrap();
            lease.as_mut_slice().unwrap().copy_from_slice(&frame[..len]);
            let mut input = PacketBatch::new();
            assert!(
                input
                    .push(PacketChain::from_lease(lease), PacketMetadata::default())
                    .is_ok()
            );
            let mut output = FrontendBatch::new();
            VectorFrontend::new([1; 40], 1).process(
                InterfaceId(1),
                &config(),
                &mut input,
                &mut output,
            );
            assert!(matches!(
                output.packet(0).unwrap().parsed.disposition,
                FrontendDisposition::Drop(_)
            ));
            output.clear();
            owner.drain_remote();
        }
    }

    #[test]
    fn every_ipv4_tcp_truncation_is_bounded() {
        let frame = tcp_frame();
        let storage = (0..frame.len())
            .map(|_| {
                Box::new(Storage {
                    bytes: vec![0; 64].into_boxed_slice(),
                }) as Box<dyn NetBufStorage>
            })
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        for len in 0..frame.len() {
            let mut lease = owner
                .lease(0, len as u16, PacketMetadata::default())
                .unwrap();
            lease.as_mut_slice().unwrap().copy_from_slice(&frame[..len]);
            let mut input = PacketBatch::new();
            assert!(
                input
                    .push(PacketChain::from_lease(lease), PacketMetadata::default())
                    .is_ok()
            );
            let mut output = FrontendBatch::new();
            VectorFrontend::new([1; 40], 1).process(
                InterfaceId(1),
                &config(),
                &mut input,
                &mut output,
            );
            assert!(matches!(
                output.packet(0).unwrap().parsed.disposition,
                FrontendDisposition::Drop(_)
            ));
            output.clear();
            owner.drain_remote();
        }
    }

    #[test]
    fn ipv6_udp_requires_nonzero_checksum() {
        let mut frame = [0u8; 66];
        frame[0..6].copy_from_slice(&[2; 6]);
        frame[6..12].copy_from_slice(&[1; 6]);
        frame[12..14].copy_from_slice(&ETHERTYPE_IPV6.to_be_bytes());
        frame[14] = 0x60;
        frame[18..20].copy_from_slice(&12u16.to_be_bytes());
        frame[20] = IP_PROTOCOL_UDP;
        frame[21] = 64;
        frame[22..38].copy_from_slice(&Ipv6Addr::LOCALHOST.0);
        frame[38..54].copy_from_slice(&Ipv6Addr::LOCALHOST.0);
        frame[54..56].copy_from_slice(&1000u16.to_be_bytes());
        frame[56..58].copy_from_slice(&9000u16.to_be_bytes());
        frame[58..60].copy_from_slice(&12u16.to_be_bytes());
        frame[62..66].copy_from_slice(b"test");
        let storage = alloc::vec![Box::new(Storage {
            bytes: vec![0; 128].into_boxed_slice(),
        }) as Box<dyn NetBufStorage>]
        .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        let mut lease = owner
            .lease(0, frame.len() as u16, PacketMetadata::default())
            .unwrap();
        lease.as_mut_slice().unwrap().copy_from_slice(&frame);
        let mut input = PacketBatch::new();
        assert!(
            input
                .push(PacketChain::from_lease(lease), PacketMetadata::default())
                .is_ok()
        );
        let ipv6_config = ConfigSnapshot::new(
            1,
            vec![InterfaceSnapshot {
                id: InterfaceId(1),
                device: NetDeviceId(1),
                mac_address: [2; 6],
                mtu: 1500,
                running: true,
                loopback: true,
            }],
            vec![AddressEntry {
                interface: InterfaceId(1),
                address: IpAddr::V6(Ipv6Addr::LOCALHOST),
                prefix_len: 128,
                primary: true,
            }],
            vec![RouteEntry {
                table: 0,
                network: IpAddr::V6(Ipv6Addr::LOCALHOST),
                prefix_len: 128,
                gateway: None,
                interface: InterfaceId(1),
                metric: 0,
                mtu: None,
            }],
            vec![],
        )
        .unwrap();
        let mut output = FrontendBatch::new();
        VectorFrontend::new([1; 40], 1).process(
            InterfaceId(1),
            &ipv6_config,
            &mut input,
            &mut output,
        );
        assert_eq!(
            output.packet(0).unwrap().parsed.disposition,
            FrontendDisposition::Drop(DropReason::UdpChecksum)
        );
    }
}
