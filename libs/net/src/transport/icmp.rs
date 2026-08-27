use crate::buf::{DropReason, PacketChain};
use crate::control::{ConfigSnapshot, NeighborKey, NeighborTable};
use crate::flow::FlowKey;
use crate::pipeline::{
    ControlPacket, FrontendPacket, ParsedPacket, packet_checksum, transport_checksum,
};
use crate::{Endpoint, InterfaceId, IpAddr, Ipv4Addr, Ipv6Addr, TransportProtocol};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportControlError {
    NetworkUnreachable,
    HostUnreachable,
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
        target: ControlErrorTarget,
        error: TransportControlError,
        packet: PacketChain,
    },
}

pub fn build_port_unreachable(packet: &PacketChain, parsed: ParsedPacket) -> Option<PacketChain> {
    let ip = parsed.ip?;
    if ip.destination.is_multicast()
        || matches!(ip.destination, IpAddr::V4(address) if address.is_broadcast())
        || ip.fragment.is_some_and(|fragment| fragment.offset != 0)
        || ip.source.is_unspecified()
    {
        return None;
    }
    match (ip.source, ip.destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let quote_len = usize::from(ip.header_len)
                .saturating_add(8)
                .min(usize::from(ip.header_len) + ip.payload_len as usize);
            let mut frame = alloc::vec![0; 14 + 20 + 8 + quote_len];
            frame[0..6].copy_from_slice(&parsed.ethernet.source);
            frame[6..12].copy_from_slice(&parsed.ethernet.destination);
            frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            frame[14] = 0x45;
            frame[16..18].copy_from_slice(&((20 + 8 + quote_len) as u16).to_be_bytes());
            frame[22] = 64;
            frame[23] = 1;
            frame[26..30].copy_from_slice(&destination.0);
            frame[30..34].copy_from_slice(&source.0);
            let ip_checksum = crate::pipeline::checksum_bytes(&frame[14..34]);
            frame[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
            frame[34] = 3;
            frame[35] = 3;
            packet.copy_out(14, &mut frame[42..]).ok()?;
            let icmp_checksum = crate::pipeline::checksum_bytes(&frame[34..]);
            frame[36..38].copy_from_slice(&icmp_checksum.to_be_bytes());
            Some(PacketChain::from_owned(frame))
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            let quote_len = (40usize + 8).min(40 + ip.payload_len as usize);
            let icmp_len = 8 + quote_len;
            let mut frame = alloc::vec![0; 14 + 40 + icmp_len];
            frame[0..6].copy_from_slice(&parsed.ethernet.source);
            frame[6..12].copy_from_slice(&parsed.ethernet.destination);
            frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            frame[14..18].copy_from_slice(&0x6000_0000u32.to_be_bytes());
            frame[18..20].copy_from_slice(&(icmp_len as u16).to_be_bytes());
            frame[20] = 58;
            frame[21] = 64;
            frame[22..38].copy_from_slice(&destination.0);
            frame[38..54].copy_from_slice(&source.0);
            frame[54] = 1;
            frame[55] = 4;
            packet.copy_out(14, &mut frame[62..]).ok()?;
            let chain = PacketChain::from_owned(frame);
            let checksum = transport_checksum(
                &chain,
                54,
                icmp_len,
                IpAddr::V6(destination),
                IpAddr::V6(source),
                58,
            )
            .ok()?;
            let mut frame = alloc::vec![0; chain.total_len()];
            chain.copy_out(0, &mut frame).ok()?;
            frame[56..58].copy_from_slice(&checksum.to_be_bytes());
            Some(PacketChain::from_owned(frame))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlErrorTarget {
    Flow(FlowKey),
    Raw {
        local: IpAddr,
        remote: IpAddr,
        protocol: u8,
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
                handle_icmpv6(
                    packet,
                    packet_offset,
                    packet_len,
                    interface,
                    local_mac,
                    config,
                    neighbors,
                    now_ns,
                )
            } else {
                handle_icmpv4(packet, packet_offset, packet_len)
            }
        }
        crate::pipeline::FrontendDisposition::Control(ControlPacket::Fragment(_)) => {
            ControlPacketResult::Drop(DropReason::UnsupportedIpProtocol, packet.chain)
        }
        crate::pipeline::FrontendDisposition::Control(ControlPacket::Ipv6ParameterProblem {
            pointer,
            suppress_for_multicast,
        }) => build_ipv6_parameter_problem(packet, pointer, suppress_for_multicast),
        _ => ControlPacketResult::Drop(DropReason::UnsupportedIpProtocol, packet.chain),
    }
}

fn build_ipv6_parameter_problem(
    packet: FrontendPacket,
    pointer: u32,
    suppress_for_multicast: bool,
) -> ControlPacketResult {
    let Some(ip) = packet.parsed.ip else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    };
    let (IpAddr::V6(source), IpAddr::V6(destination)) = (ip.source, ip.destination) else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    };
    let mut icmp_type = [0u8; 1];
    let responds_to_error = ip.next_header == 58
        && packet
            .chain
            .copy_out(usize::from(ip.payload_offset), &mut icmp_type)
            .is_ok()
        && icmp_type[0] < 128;
    if source.is_unspecified()
        || source.is_multicast()
        || responds_to_error
        || (suppress_for_multicast && destination.is_multicast())
    {
        return ControlPacketResult::Consumed(packet.chain);
    }
    let quote_len = packet.chain.total_len().saturating_sub(14).min(1232);
    let mut frame = alloc::vec![0; 14 + 40 + 8 + quote_len];
    frame[0..6].copy_from_slice(&packet.parsed.ethernet.source);
    frame[6..12].copy_from_slice(&packet.parsed.ethernet.destination);
    frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
    frame[14..18].copy_from_slice(&0x6000_0000u32.to_be_bytes());
    frame[18..20].copy_from_slice(&((8 + quote_len) as u16).to_be_bytes());
    frame[20] = 58;
    frame[21] = 64;
    frame[22..38].copy_from_slice(&destination.0);
    frame[38..54].copy_from_slice(&source.0);
    frame[54] = 4;
    frame[55] = 2;
    frame[58..62].copy_from_slice(&pointer.to_be_bytes());
    if packet.chain.copy_out(14, &mut frame[62..]).is_err() {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let chain = PacketChain::from_owned(frame);
    let Ok(checksum) = transport_checksum(
        &chain,
        54,
        8 + quote_len,
        IpAddr::V6(destination),
        IpAddr::V6(source),
        58,
    ) else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    };
    let mut frame = alloc::vec![0; chain.total_len()];
    if chain.copy_out(0, &mut frame).is_err() {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    frame[56..58].copy_from_slice(&checksum.to_be_bytes());
    ControlPacketResult::Reply(PacketChain::from_owned(frame))
}

fn handle_icmpv4(mut packet: FrontendPacket, offset: u16, len: u32) -> ControlPacketResult {
    if len < 8
        || (!packet.metadata.checksums_validated
            && packet_checksum(&packet.chain, usize::from(offset), len as usize).ok() != Some(0))
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
            Some((target, error)) => ControlPacketResult::TransportError {
                target,
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

fn handle_icmpv6(
    mut packet: FrontendPacket,
    offset: u16,
    len: u32,
    interface: InterfaceId,
    local_mac: [u8; 6],
    config: &ConfigSnapshot,
    neighbors: &mut NeighborTable,
    now_ns: u64,
) -> ControlPacketResult {
    if len < 8 {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let ip = packet.parsed.ip.unwrap();
    if !packet.metadata.checksums_validated
        && transport_checksum(
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
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    if icmp[0] == 135 {
        if icmp[1] != 0 {
            return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
        }
        return handle_neighbor_solicitation(packet, offset, len, interface, local_mac, config);
    }
    // Neighbor Advertisement（RFC 4861 §4.4）：观测发送者的链路层地址，
    // 用于解析待发送队列（ping6 等目标在链路本地时依赖 NS/NA 交换）。
    if icmp[0] == 136 {
        return handle_neighbor_advertisement(
            packet, offset, len, interface, config, neighbors, now_ns,
        );
    }
    if icmp[0] != 128 {
        return match parse_icmpv6_error(&packet.chain, offset, len) {
            Some((target, error)) => ControlPacketResult::TransportError {
                target,
                error,
                packet: packet.chain,
            },
            None => ControlPacketResult::Consumed(packet.chain),
        };
    }
    if icmp[1] != 0 {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
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

fn handle_neighbor_solicitation(
    mut packet: FrontendPacket,
    offset: u16,
    len: u32,
    interface: InterfaceId,
    local_mac: [u8; 6],
    config: &ConfigSnapshot,
) -> ControlPacketResult {
    if len < 24 {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let mut solicitation = [0u8; 24];
    if packet
        .chain
        .copy_out(usize::from(offset), &mut solicitation)
        .is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let target = Ipv6Addr(solicitation[8..24].try_into().unwrap());
    if !config.is_local_address(interface, IpAddr::V6(target)) {
        return ControlPacketResult::Consumed(packet.chain);
    }
    let ip = packet.parsed.ip.unwrap();
    let IpAddr::V6(source) = ip.source else {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    };
    let destination = if source.is_unspecified() {
        Ipv6Addr([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
    } else {
        source
    };
    let destination_mac = if source.is_unspecified() {
        [0x33, 0x33, 0, 0, 0, 1]
    } else {
        packet.parsed.ethernet.source
    };
    let reply_len = if len >= 32 { 32usize } else { 24usize };
    let mut ethernet = [0u8; 12];
    ethernet[..6].copy_from_slice(&destination_mac);
    ethernet[6..].copy_from_slice(&local_mac);
    let mut reply = [0u8; 32];
    reply[0] = 136;
    reply[4..8].copy_from_slice(&0x6000_0000u32.to_be_bytes());
    reply[8..24].copy_from_slice(&target.0);
    if reply_len == 32 {
        reply[24] = 2;
        reply[25] = 1;
        reply[26..32].copy_from_slice(&local_mac);
    }
    if packet.chain.copy_in(0, &ethernet).is_err()
        || packet
            .chain
            .copy_in(18, &(reply_len as u16).to_be_bytes())
            .is_err()
        || packet.chain.copy_in(22, &target.0).is_err()
        || packet.chain.copy_in(38, &destination.0).is_err()
        || packet
            .chain
            .copy_in(usize::from(offset), &reply[..reply_len])
            .is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let Ok(checksum) = transport_checksum(
        &packet.chain,
        usize::from(offset),
        reply_len,
        IpAddr::V6(target),
        IpAddr::V6(destination),
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

fn handle_neighbor_advertisement(
    packet: FrontendPacket,
    offset: u16,
    len: u32,
    interface: InterfaceId,
    config: &ConfigSnapshot,
    neighbors: &mut NeighborTable,
    now_ns: u64,
) -> ControlPacketResult {
    if len < 24 {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let mut advertisement = [0u8; 32];
    let readable = (len as usize).min(advertisement.len());
    if packet
        .chain
        .copy_out(usize::from(offset), &mut advertisement[..readable])
        .is_err()
    {
        return ControlPacketResult::Drop(DropReason::MalformedIpv6, packet.chain);
    }
    let target = Ipv6Addr(advertisement[8..24].try_into().unwrap());
    // 目标为本机地址的 NA 不写入邻居表（DAD/地址冲突由控制面另行处理）。
    if config.is_local_address(interface, IpAddr::V6(target)) {
        return ControlPacketResult::Consumed(packet.chain);
    }
    // 链路层地址选项（RFC 4861 §4.6.1）：type=2 length=1，MAC 占 6 字节。
    let has_link_layer = readable >= 32 && advertisement[24] == 2 && advertisement[25] == 1;
    if has_link_layer {
        let mac = advertisement[26..32].try_into().unwrap();
        let _ = neighbors.observe(
            NeighborKey {
                interface,
                address: IpAddr::V6(target),
            },
            mac,
            now_ns,
        );
    }
    ControlPacketResult::Consumed(packet.chain)
}

fn parse_icmpv4_error(
    packet: &PacketChain,
    offset: u16,
    len: u32,
) -> Option<(ControlErrorTarget, TransportControlError)> {
    if len < 8 + 20 {
        return None;
    }
    let mut icmp = [0u8; 8];
    packet.copy_out(usize::from(offset), &mut icmp).ok()?;
    let error = match (icmp[0], icmp[1]) {
        (3, 0) => TransportControlError::NetworkUnreachable,
        (3, 1 | 2 | 9 | 10 | 13) => TransportControlError::HostUnreachable,
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
    if ipv4[0] >> 4 != 4 || !(20..=60).contains(&header_len) {
        return None;
    }
    quoted_error_target(
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
) -> Option<(ControlErrorTarget, TransportControlError)> {
    if len < 8 + 40 {
        return None;
    }
    let mut icmp = [0u8; 8];
    packet.copy_out(usize::from(offset), &mut icmp).ok()?;
    let error = match (icmp[0], icmp[1]) {
        (1, 0) => TransportControlError::NetworkUnreachable,
        (1, 1 | 2 | 3) => TransportControlError::HostUnreachable,
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
    if ipv6[0] >> 4 != 6 {
        return None;
    }
    quoted_error_target(
        packet,
        inner + 40,
        IpAddr::V6(Ipv6Addr(ipv6[8..24].try_into().ok()?)),
        IpAddr::V6(Ipv6Addr(ipv6[24..40].try_into().ok()?)),
        ipv6[6],
    )
    .map(|flow| (flow, error))
}

fn quoted_error_target(
    packet: &PacketChain,
    udp_offset: usize,
    local_address: IpAddr,
    remote_address: IpAddr,
    protocol: u8,
) -> Option<ControlErrorTarget> {
    if !matches!(protocol, 6 | 17) {
        return Some(ControlErrorTarget::Raw {
            local: local_address,
            remote: remote_address,
            protocol,
        });
    }
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
    .map(ControlErrorTarget::Flow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{AddressEntry, InterfaceSnapshot, RouteEntry};

    /// 构造一个发给本机的 IPv6 NA 帧（type 136，目标 fe80::2，带链路层选项）。
    fn build_neighbor_advertisement_packet(
        target: Ipv6Addr,
        advertiser_mac: [u8; 6],
    ) -> FrontendPacket {
        let router = Ipv6Addr([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let mut frame = alloc::vec![0u8; 14 + 40 + 32];
        frame[0..6].copy_from_slice(&advertiser_mac);
        frame[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x01]);
        frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
        frame[14..18].copy_from_slice(&0x6000_0000u32.to_be_bytes());
        frame[18..20].copy_from_slice(&32u16.to_be_bytes());
        frame[20] = 58;
        frame[21] = 255;
        frame[22..38].copy_from_slice(&router.0);
        frame[38..54].copy_from_slice(&target.0);
        frame[54] = 136;
        frame[55] = 0x40; // Solicited 标志。
        frame[62..78].copy_from_slice(&target.0);
        frame[78] = 2;
        frame[79] = 1;
        frame[80..86].copy_from_slice(&advertiser_mac);
        // 计算 ICMPv6 校验和，否则入口校验会丢弃。
        let chain = crate::buf::PacketChain::from_owned(frame);
        let checksum = crate::pipeline::transport_checksum(
            &chain,
            54,
            32,
            IpAddr::V6(router),
            IpAddr::V6(target),
            58,
        )
        .expect("校验和应可计算");
        let mut frame = alloc::vec![0u8; chain.total_len()];
        chain.copy_out(0, &mut frame).unwrap();
        frame[56..58].copy_from_slice(&checksum.to_be_bytes());
        let chain = crate::buf::PacketChain::from_owned(frame);
        let parsed = crate::pipeline::ParsedPacket {
            ethernet: crate::pipeline::EthernetHeader {
                destination: [0x02, 0, 0, 0, 0, 0x01],
                source: advertiser_mac,
                ethertype: 0x86dd,
            },
            ip: Some(crate::pipeline::IpPacket {
                source: IpAddr::V6(router),
                destination: IpAddr::V6(target),
                next_header: 58,
                header_len: 40,
                payload_offset: 54,
                payload_len: 32,
                hop_limit: 255,
                traffic_class: 0,
                fragment: None,
            }),
            tcp: None,
            udp: None,
            flow: None,
            rss_hash: None,
            disposition: crate::pipeline::FrontendDisposition::Control(
                crate::pipeline::ControlPacket::Icmp {
                    ipv6: true,
                    packet_offset: 54,
                    packet_len: 32,
                },
            ),
        };
        crate::pipeline::FrontendPacket {
            chain,
            parsed,
            metadata: crate::buf::PacketMetadata::default(),
        }
    }

    fn local_config() -> ConfigSnapshot {
        ConfigSnapshot::new(
            1,
            alloc::vec![InterfaceSnapshot {
                id: InterfaceId(1),
                device: crate::NetDeviceId(1),
                mac_address: [0x02, 0, 0, 0, 0, 0x01],
                mtu: 1500,
                running: true,
                loopback: false,
            }],
            alloc::vec![AddressEntry {
                interface: InterfaceId(1),
                address: IpAddr::V6(Ipv6Addr([
                    0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
                ])),
                prefix_len: 64,
                primary: true,
            }],
            alloc::vec![RouteEntry {
                table: 0,
                network: IpAddr::V6(Ipv6Addr([
                    0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ])),
                prefix_len: 64,
                gateway: None,
                interface: InterfaceId(1),
                metric: 0,
                mtu: Some(1500),
            }],
            alloc::vec::Vec::new(),
        )
        .expect("配置应合法")
    }

    /// NA 携带链路层选项时应观测邻居，待解析队列即可继续发送。
    #[test]
    fn neighbor_advertisement_observes_link_layer_address() {
        let target = Ipv6Addr([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let router_mac = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
        let packet = build_neighbor_advertisement_packet(target, router_mac);
        let config = local_config();
        let mut neighbors = NeighborTable::new([7; 16]);
        let result = handle_icmpv6(
            packet,
            54,
            32,
            InterfaceId(1),
            config.interfaces[0].mac_address,
            &config,
            &mut neighbors,
            1_000_000,
        );
        assert!(matches!(result, ControlPacketResult::Consumed(_)));
        let observed = neighbors
            .lookup(
                NeighborKey {
                    interface: InterfaceId(1),
                    address: IpAddr::V6(target),
                },
                1_100_000,
            )
            .expect("NA 应写入邻居表");
        assert_eq!(observed.0, router_mac);
    }

    /// 目标为本机地址的 NA 不应污染邻居表。
    #[test]
    fn neighbor_advertisement_for_local_address_is_ignored() {
        let local = Ipv6Addr([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let other_mac = [0xde, 0xad, 0xbe, 0xef, 0, 1];
        let packet = build_neighbor_advertisement_packet(local, other_mac);
        let config = local_config();
        let mut neighbors = NeighborTable::new([8; 16]);
        let result = handle_icmpv6(
            packet,
            54,
            32,
            InterfaceId(1),
            config.interfaces[0].mac_address,
            &config,
            &mut neighbors,
            1_000_000,
        );
        assert!(matches!(result, ControlPacketResult::Consumed(_)));
        assert!(
            neighbors
                .lookup(
                    NeighborKey {
                        interface: InterfaceId(1),
                        address: IpAddr::V6(local),
                    },
                    1_100_000,
                )
                .is_none()
        );
    }
}
