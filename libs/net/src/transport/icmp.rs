use crate::buf::{DropReason, PacketChain};
use crate::control::{ConfigSnapshot, NeighborKey, NeighborTable};
use crate::flow::FlowKey;
use crate::pipeline::{ControlPacket, FrontendPacket, packet_checksum, transport_checksum};
use crate::{Endpoint, InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr, TransportProtocol};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportControlError {
    PortUnreachable,
    PacketTooBig { mtu: u32 },
    TimeExceeded,
    ParameterProblem,
}

pub enum ControlPacketResult {
    Consumed(PacketChain),
    Reply(PacketChain),
    Drop(DropReason, PacketChain),
    TransportError {
        flow: FlowKey,
        error: TransportControlError,
        packet: PacketChain,
    },
}

pub fn handle_control_packet(
    interface: InterfaceId,
    local_mac: [u8; 6],
    config: &ConfigSnapshot,
    neighbors: &mut NeighborTable,
    now_ns: u64,
    mut packet: FrontendPacket,
) -> ControlPacketResult {
    match packet.parsed.disposition {
        crate::pipeline::FrontendDisposition::Control(ControlPacket::Arp(arp)) => {
            let _ = neighbors.observe(
                NeighborKey {
                    interface,
                    address: IpAddr::V4(arp.sender_ip),
                },
                arp.sender_mac,
                now_ns,
            );
            if arp.operation != 1 || !config.is_local_address(interface, IpAddr::V4(arp.target_ip))
            {
                return ControlPacketResult::Consumed(packet.chain);
            }
            let mut ethernet = [0u8; 14];
            ethernet[0..6].copy_from_slice(&arp.sender_mac);
            ethernet[6..12].copy_from_slice(&local_mac);
            ethernet[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
            let mut reply = [0u8; 28];
            reply[0..2].copy_from_slice(&1u16.to_be_bytes());
            reply[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
            reply[4] = 6;
            reply[5] = 4;
            reply[6..8].copy_from_slice(&2u16.to_be_bytes());
            reply[8..14].copy_from_slice(&local_mac);
            reply[14..18].copy_from_slice(&arp.target_ip.0);
            reply[18..24].copy_from_slice(&arp.sender_mac);
            reply[24..28].copy_from_slice(&arp.sender_ip.0);
            if packet.chain.copy_in(0, &ethernet).is_err()
                || packet.chain.copy_in(14, &reply).is_err()
            {
                return ControlPacketResult::Drop(DropReason::MalformedArp, packet.chain);
            }
            ControlPacketResult::Reply(packet.chain)
        }
        crate::pipeline::FrontendDisposition::Control(ControlPacket::Icmp {
            ipv6,
            packet_offset,
            packet_len,
        }) => {
            if ipv6 {
                handle_icmpv6(packet, packet_offset, packet_len)
            } else {
                handle_icmpv4(packet, packet_offset, packet_len)
            }
        }
        crate::pipeline::FrontendDisposition::Control(ControlPacket::Fragment(_)) => {
            ControlPacketResult::Drop(DropReason::UnsupportedIpProtocol, packet.chain)
        }
        _ => ControlPacketResult::Drop(DropReason::UnsupportedIpProtocol, packet.chain),
    }
}

fn handle_icmpv4(mut packet: FrontendPacket, offset: u16, len: u32) -> ControlPacketResult {
    if len < 8 || packet_checksum(&packet.chain, usize::from(offset), len as usize).ok() != Some(0)
    {
        return ControlPacketResult::Drop(DropReason::UnsupportedIpProtocol, packet.chain);
    }
    let mut icmp = [0u8; 4];
    if packet
        .chain
        .copy_out(usize::from(offset), &mut icmp)
        .is_err()
        || icmp[0] != 8
        || icmp[1] != 0
    {
        return match parse_icmpv4_error(&packet.chain, offset, len) {
            Some((flow, error)) => ControlPacketResult::TransportError {
                flow,
                error,
                packet: packet.chain,
            },
            None => ControlPacketResult::Consumed(packet.chain),
        };
    }
    let ip = packet.parsed.ip.unwrap();
    let ethernet = packet.parsed.ethernet;
    let mut ethernet_reply = [0u8; 12];
    ethernet_reply[0..6].copy_from_slice(&ethernet.source);
    ethernet_reply[6..12].copy_from_slice(&ethernet.destination);
    let (IpAddr::V4(source), IpAddr::V4(destination)) = (ip.source, ip.destination) else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv4, packet.chain);
    };
    if packet.chain.copy_in(0, &ethernet_reply).is_err()
        || packet.chain.copy_in(22, &[64]).is_err()
        || packet.chain.copy_in(26, &destination.0).is_err()
        || packet.chain.copy_in(30, &source.0).is_err()
        || packet
            .chain
            .copy_in(usize::from(offset), &[0, 0, 0, 0])
            .is_err()
        || packet.chain.copy_in(24, &[0, 0]).is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv4, packet.chain);
    }
    let Ok(icmp_checksum) = packet_checksum(&packet.chain, usize::from(offset), len as usize)
    else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv4, packet.chain);
    };
    if packet
        .chain
        .copy_in(usize::from(offset) + 2, &icmp_checksum.to_be_bytes())
        .is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv4, packet.chain);
    }
    let mut header = [0u8; 60];
    let header_len = usize::from(ip.header_len);
    if header_len > header.len()
        || packet
            .chain
            .copy_out(14, &mut header[..header_len])
            .is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv4, packet.chain);
    }
    let checksum = crate::pipeline::checksum_bytes(&header[..header_len]);
    if packet.chain.copy_in(24, &checksum.to_be_bytes()).is_err() {
        return ControlPacketResult::Drop(DropReason::MalformedIpv4, packet.chain);
    }
    ControlPacketResult::Reply(packet.chain)
}

fn handle_icmpv6(mut packet: FrontendPacket, offset: u16, len: u32) -> ControlPacketResult {
    if len < 8 {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let ip = packet.parsed.ip.unwrap();
    if transport_checksum(
        &packet.chain,
        usize::from(offset),
        len as usize,
        ip.source,
        ip.destination,
        58,
    )
    .ok()
        != Some(0)
    {
        return ControlPacketResult::Drop(DropReason::UnsupportedIpProtocol, packet.chain);
    }
    let mut icmp = [0u8; 4];
    if packet
        .chain
        .copy_out(usize::from(offset), &mut icmp)
        .is_err()
        || icmp[0] != 128
        || icmp[1] != 0
    {
        return match parse_icmpv6_error(&packet.chain, offset, len) {
            Some((flow, error)) => ControlPacketResult::TransportError {
                flow,
                error,
                packet: packet.chain,
            },
            None => ControlPacketResult::Consumed(packet.chain),
        };
    }
    let ethernet = packet.parsed.ethernet;
    let mut ethernet_reply = [0u8; 12];
    ethernet_reply[0..6].copy_from_slice(&ethernet.source);
    ethernet_reply[6..12].copy_from_slice(&ethernet.destination);
    let (IpAddr::V6(source), IpAddr::V6(destination)) = (ip.source, ip.destination) else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    };
    if packet.chain.copy_in(0, &ethernet_reply).is_err()
        || packet.chain.copy_in(21, &[64]).is_err()
        || packet.chain.copy_in(22, &destination.0).is_err()
        || packet.chain.copy_in(38, &source.0).is_err()
        || packet
            .chain
            .copy_in(usize::from(offset), &[129, 0, 0, 0])
            .is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let Ok(checksum) = transport_checksum(
        &packet.chain,
        usize::from(offset),
        len as usize,
        ip.destination,
        ip.source,
        58,
    ) else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    };
    if packet
        .chain
        .copy_in(usize::from(offset) + 2, &checksum.to_be_bytes())
        .is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    ControlPacketResult::Reply(packet.chain)
}

fn parse_icmpv4_error(
    packet: &PacketChain,
    offset: u16,
    len: u32,
) -> Option<(FlowKey, TransportControlError)> {
    if len < 8 + 20 + 8 {
        return None;
    }
    let mut icmp = [0u8; 8];
    packet.copy_out(usize::from(offset), &mut icmp).ok()?;
    let error = match (icmp[0], icmp[1]) {
        (3, 3) => TransportControlError::PortUnreachable,
        (3, 4) => TransportControlError::PacketTooBig {
            mtu: u32::from(u16::from_be_bytes([icmp[6], icmp[7]])),
        },
        (11, _) => TransportControlError::TimeExceeded,
        (12, _) => TransportControlError::ParameterProblem,
        _ => return None,
    };
    let inner = usize::from(offset) + 8;
    let mut ipv4 = [0u8; 20];
    packet.copy_out(inner, &mut ipv4).ok()?;
    let header_len = usize::from(ipv4[0] & 0x0f) * 4;
    if ipv4[0] >> 4 != 4 || !(20..=60).contains(&header_len) || !matches!(ipv4[9], 6 | 17) {
        return None;
    }
    quoted_transport_flow(
        packet,
        inner + header_len,
        IpAddr::V4(Ipv4Addr(ipv4[12..16].try_into().ok()?)),
        IpAddr::V4(Ipv4Addr(ipv4[16..20].try_into().ok()?)),
        ipv4[9],
    )
    .map(|flow| (flow, error))
}

fn parse_icmpv6_error(
    packet: &PacketChain,
    offset: u16,
    len: u32,
) -> Option<(FlowKey, TransportControlError)> {
    if len < 8 + 40 + 8 {
        return None;
    }
    let mut icmp = [0u8; 8];
    packet.copy_out(usize::from(offset), &mut icmp).ok()?;
    let error = match (icmp[0], icmp[1]) {
        (1, 4) => TransportControlError::PortUnreachable,
        (2, _) => TransportControlError::PacketTooBig {
            mtu: u32::from_be_bytes(icmp[4..8].try_into().ok()?),
        },
        (3, _) => TransportControlError::TimeExceeded,
        (4, _) => TransportControlError::ParameterProblem,
        _ => return None,
    };
    let inner = usize::from(offset) + 8;
    let mut ipv6 = [0u8; 40];
    packet.copy_out(inner, &mut ipv6).ok()?;
    if ipv6[0] >> 4 != 6 || !matches!(ipv6[6], 6 | 17) {
        return None;
    }
    quoted_transport_flow(
        packet,
        inner + 40,
        IpAddr::V6(Ipv6Addr(ipv6[8..24].try_into().ok()?)),
        IpAddr::V6(Ipv6Addr(ipv6[24..40].try_into().ok()?)),
        ipv6[6],
    )
    .map(|flow| (flow, error))
}

fn quoted_transport_flow(
    packet: &PacketChain,
    udp_offset: usize,
    local_address: IpAddr,
    remote_address: IpAddr,
    protocol: u8,
) -> Option<FlowKey> {
    let mut ports = [0u8; 4];
    packet.copy_out(udp_offset, &mut ports).ok()?;
    FlowKey::new(
        Endpoint {
            addr: remote_address,
            port: u16::from_be_bytes([ports[2], ports[3]]),
        },
        Endpoint {
            addr: local_address,
            port: u16::from_be_bytes([ports[0], ports[1]]),
        },
        if protocol == 6 {
            TransportProtocol::Tcp
        } else {
            TransportProtocol::Udp
        },
    )
}
