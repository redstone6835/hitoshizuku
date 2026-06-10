//! DHCP (RFC 2131) 客户端实现。
//!
//! 使用 UDP socket 发送 DISCOVER/REQUEST 消息到端口 67，
//! 在端口 68 接收 OFFER/ACK 响应。
//!
//! # 状态机
//! ```text
//! INIT → DISCOVER(广播) → OFFER → REQUEST(广播) → ACK → BOUND
//! BOUND → (租期过半) → RENEW(单播) → ACK → BOUND
//! BOUND → (RELEASE) → INIT
//! ```

use crate::config::{IfConfig, IfMode, Ipv4Addr};
use crate::time::{NetDuration, NetInstant};
use alloc::vec::Vec;

const BOOTP_HEADER_LEN: usize = 236;
const DHCP_OPTIONS_OFFSET: usize = BOOTP_HEADER_LEN + MAGIC_COOKIE.len();
const BOOTP_OP_OFFSET: usize = 0;
const BOOTP_XID_OFFSET: usize = 4;
const BOOTP_SECS_OFFSET: usize = 8;
const BOOTP_FLAGS_OFFSET: usize = 10;
const BOOTP_YIADDR_OFFSET: usize = 16;
const BOOTP_CHADDR_OFFSET: usize = 28;
const BOOTP_CHADDR_LEN: usize = 16;
const BOOTP_LEGACY_AREA_LEN: usize = 192;

const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;

const HTYPE_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;
const BOOTP_BROADCAST_FLAG: u16 = 0x8000;

const OPT_PAD: u8 = 0;

const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAMETER_REQUEST_LIST: u8 = 55;
const OPT_END: u8 = 255;

const REQUESTED_PARAMETERS: [u8; 4] = [OPT_SUBNET_MASK, OPT_ROUTER, OPT_DNS, OPT_LEASE_TIME];
const DEFAULT_DHCP_LEASE_SECS: u32 = 24 * 60 * 60;
const ADDRESS_ONLY_PREFIX_LEN: u8 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhcpState {
    Init,
    Selecting,
    Requesting,
    Bound {
        lease_expires: NetInstant,
        server_id: Ipv4Addr,
    },
}

pub struct DhcpClient {
    pub state: DhcpState,
    pub offered_ip: Option<Ipv4Addr>,
    pub offered_server_id: Option<Ipv4Addr>,
    pub offered_gateway: Option<Ipv4Addr>,
    pub offered_dns: Option<Ipv4Addr>,
    pub subnet_mask: Option<Ipv4Addr>,
    pub xid: u32,
    retry_count: u32,
}

impl DhcpClient {
    pub const fn new() -> Self {
        Self {
            state: DhcpState::Init,
            offered_ip: None,
            offered_server_id: None,
            offered_gateway: None,
            offered_dns: None,
            subnet_mask: None,
            xid: 0,
            retry_count: 0,
        }
    }

    /// 构建 DHCPDISCOVER 消息。
    pub fn build_discover(&mut self, mac: &[u8; 6]) -> Vec<u8> {
        self.xid = self.xid.wrapping_add(1);
        self.state = DhcpState::Selecting;
        self.retry_count = 0;
        self.clear_offer();
        let mut pkt = Vec::new();
        fill_dhcp_header(&mut pkt, OP_REQUEST, mac, self.xid, BOOTP_BROADCAST_FLAG);
        push_message_type(&mut pkt, DhcpMessageType::Discover);
        push_parameter_request_list(&mut pkt);
        finish_options(&mut pkt);
        pkt
    }

    /// 构建 DHCPREQUEST 消息。
    pub fn build_request(&mut self, mac: &[u8; 6], server_id: Ipv4Addr) -> Vec<u8> {
        self.state = DhcpState::Requesting;
        self.offered_server_id = Some(server_id);
        let mut pkt = Vec::new();
        fill_dhcp_header(&mut pkt, OP_REQUEST, mac, self.xid, BOOTP_BROADCAST_FLAG);
        push_message_type(&mut pkt, DhcpMessageType::Request);
        push_ipv4_option(&mut pkt, OPT_SERVER_ID, server_id);
        if let Some(ip) = self.offered_ip {
            push_ipv4_option(&mut pkt, OPT_REQUESTED_IP, ip);
        }
        push_parameter_request_list(&mut pkt);
        finish_options(&mut pkt);
        pkt
    }

    /// 解析 DHCP 响应，返回获得的网络配置（仅当收到 ACK 时）。
    pub fn parse_response(&mut self, data: &[u8], now: NetInstant) -> Option<IfConfig> {
        let message = DhcpMessage::parse(data)?;
        if message.op() != OP_REPLY {
            return None;
        }
        if message.xid() != self.xid {
            return None;
        }

        let options = parse_response_options(message.options())?;
        let msg_type = options.message_type?;

        match msg_type {
            DhcpMessageType::Nak => {
                self.state = DhcpState::Init;
                self.clear_offer();
                None
            }
            DhcpMessageType::Offer => {
                let yiaddr = valid_yiaddr(message.yiaddr())?;
                let server_id = options.server_id?;
                self.remember_offer(
                    yiaddr,
                    server_id,
                    options.router,
                    options.dns,
                    options.subnet,
                );
                None
            }
            DhcpMessageType::Ack => {
                let yiaddr = valid_yiaddr(message.yiaddr())?;
                let server_id = options.server_id?;
                self.remember_offer(
                    yiaddr,
                    server_id,
                    options.router,
                    options.dns,
                    options.subnet,
                );
                let lease_secs = options.lease_time.unwrap_or(DEFAULT_DHCP_LEASE_SECS) as u64;
                self.state = DhcpState::Bound {
                    lease_expires: now + NetDuration::from_secs(lease_secs),
                    server_id,
                };
                let prefix = options
                    .subnet
                    .and_then(ipv4_mask_prefix_len)
                    .unwrap_or(ADDRESS_ONLY_PREFIX_LEN);
                let mut config = IfConfig::static_v4(yiaddr, prefix, options.router);
                config.mode = IfMode::Auto;
                Some(config)
            }
            _ => None,
        }
    }

    fn clear_offer(&mut self) {
        self.offered_ip = None;
        self.offered_server_id = None;
        self.offered_gateway = None;
        self.offered_dns = None;
        self.subnet_mask = None;
    }

    fn remember_offer(
        &mut self,
        ip: Ipv4Addr,
        server_id: Ipv4Addr,
        router: Option<Ipv4Addr>,
        dns: Option<Ipv4Addr>,
        subnet: Option<Ipv4Addr>,
    ) {
        self.offered_ip = Some(ip);
        self.offered_server_id = Some(server_id);
        self.offered_gateway = router;
        self.offered_dns = dns;
        self.subnet_mask = subnet;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DhcpMessageType {
    Discover,
    Offer,
    Request,
    Ack,
    Nak,
}

impl DhcpMessageType {
    const fn code(self) -> u8 {
        match self {
            Self::Discover => 1,
            Self::Offer => 2,
            Self::Request => 3,
            Self::Ack => 5,
            Self::Nak => 6,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Discover),
            2 => Some(Self::Offer),
            3 => Some(Self::Request),
            5 => Some(Self::Ack),
            6 => Some(Self::Nak),
            _ => None,
        }
    }
}

struct DhcpMessage<'a> {
    data: &'a [u8],
}

impl<'a> DhcpMessage<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < DHCP_OPTIONS_OFFSET {
            return None;
        }
        if data[BOOTP_HEADER_LEN..DHCP_OPTIONS_OFFSET] != MAGIC_COOKIE {
            return None;
        }
        Some(Self { data })
    }

    fn op(&self) -> u8 {
        self.data[BOOTP_OP_OFFSET]
    }

    fn xid(&self) -> u32 {
        read_u32_at(self.data, BOOTP_XID_OFFSET)
    }

    fn yiaddr(&self) -> Ipv4Addr {
        read_ipv4_at(self.data, BOOTP_YIADDR_OFFSET)
    }

    fn options(&self) -> &'a [u8] {
        &self.data[DHCP_OPTIONS_OFFSET..]
    }
}

#[derive(Debug, Clone, Copy)]
struct DhcpOption<'a> {
    code: u8,
    value: &'a [u8],
}

#[derive(Default)]
struct DhcpResponseOptions {
    message_type: Option<DhcpMessageType>,
    lease_time: Option<u32>,
    server_id: Option<Ipv4Addr>,
    subnet: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Option<Ipv4Addr>,
}

fn parse_response_options(data: &[u8]) -> Option<DhcpResponseOptions> {
    let mut parsed = DhcpResponseOptions::default();
    parse_options(data, |option| match option.code {
        OPT_MESSAGE_TYPE => {
            if let Some(message_type) =
                read_u8_option(option.value).and_then(DhcpMessageType::from_code)
            {
                parsed.message_type = Some(message_type);
            }
        }
        OPT_LEASE_TIME => {
            if let Some(lease_time) = read_u32_option(option.value) {
                parsed.lease_time = Some(lease_time);
            }
        }
        OPT_SERVER_ID => {
            if let Some(server_id) = read_ipv4_option(option.value) {
                parsed.server_id = Some(server_id);
            }
        }
        OPT_SUBNET_MASK => {
            if let Some(subnet) = read_ipv4_option(option.value) {
                parsed.subnet = Some(subnet);
            }
        }
        OPT_ROUTER => {
            if let Some(router) = read_ipv4_list_first(option.value) {
                parsed.router = Some(router);
            }
        }
        OPT_DNS => {
            if let Some(dns) = read_ipv4_list_first(option.value) {
                parsed.dns = Some(dns);
            }
        }
        _ => {}
    })?;
    Some(parsed)
}

fn parse_options<'a, F>(mut data: &'a [u8], mut visit: F) -> Option<()>
where
    F: FnMut(DhcpOption<'a>),
{
    while let Some((&code, rest)) = data.split_first() {
        data = rest;
        match code {
            OPT_PAD => {}
            OPT_END => return Some(()),
            _ => {
                let (&len, rest) = data.split_first()?;
                let len = len as usize;
                if rest.len() < len {
                    return None;
                }
                let (value, remaining) = rest.split_at(len);
                visit(DhcpOption { code, value });
                data = remaining;
            }
        }
    }
    None
}

fn fill_dhcp_header(pkt: &mut Vec<u8>, op: u8, mac: &[u8; 6], xid: u32, flags: u16) {
    pkt.push(op);
    pkt.push(HTYPE_ETHERNET);
    pkt.push(HLEN_ETHERNET);
    pkt.push(0);
    pkt.extend_from_slice(&xid.to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&flags.to_be_bytes());
    pkt.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(pkt.len(), BOOTP_CHADDR_OFFSET);
    pkt.extend_from_slice(mac);
    pkt.extend_from_slice(&[0u8; BOOTP_CHADDR_LEN - HLEN_ETHERNET as usize]);
    pkt.extend_from_slice(&[0u8; BOOTP_LEGACY_AREA_LEN]);
    pkt.extend_from_slice(&MAGIC_COOKIE);
    debug_assert_eq!(pkt.len(), DHCP_OPTIONS_OFFSET);
    debug_assert_eq!(read_u16_at(pkt, BOOTP_FLAGS_OFFSET), flags);
    debug_assert_eq!(read_u16_at(pkt, BOOTP_SECS_OFFSET), 0);
}

fn push_message_type(pkt: &mut Vec<u8>, message_type: DhcpMessageType) {
    push_option(pkt, OPT_MESSAGE_TYPE, &[message_type.code()]);
}

fn push_parameter_request_list(pkt: &mut Vec<u8>) {
    push_option(pkt, OPT_PARAMETER_REQUEST_LIST, &REQUESTED_PARAMETERS);
}

fn push_ipv4_option(pkt: &mut Vec<u8>, code: u8, addr: Ipv4Addr) {
    push_option(pkt, code, &addr.0);
}

fn push_option(pkt: &mut Vec<u8>, code: u8, value: &[u8]) {
    if value.len() > u8::MAX as usize {
        return;
    }
    pkt.push(code);
    pkt.push(value.len() as u8);
    pkt.extend_from_slice(value);
}

fn finish_options(pkt: &mut Vec<u8>) {
    pkt.push(OPT_END);
}

fn read_u8_option(value: &[u8]) -> Option<u8> {
    if value.len() == 1 {
        Some(value[0])
    } else {
        None
    }
}

fn read_u32_option(value: &[u8]) -> Option<u32> {
    if value.len() == 4 {
        Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
    } else {
        None
    }
}

fn read_ipv4_option(value: &[u8]) -> Option<Ipv4Addr> {
    if value.len() == 4 {
        Some(Ipv4Addr([value[0], value[1], value[2], value[3]]))
    } else {
        None
    }
}

fn read_ipv4_list_first(value: &[u8]) -> Option<Ipv4Addr> {
    if value.len() >= 4 && value.len() % 4 == 0 {
        Some(Ipv4Addr([value[0], value[1], value[2], value[3]]))
    } else {
        None
    }
}

fn read_ipv4_at(data: &[u8], offset: usize) -> Ipv4Addr {
    Ipv4Addr([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn valid_yiaddr(addr: Ipv4Addr) -> Option<Ipv4Addr> {
    if addr == Ipv4Addr::UNSPECIFIED {
        None
    } else {
        Some(addr)
    }
}

fn read_u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}

fn read_u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn ipv4_mask_prefix_len(mask: Ipv4Addr) -> Option<u8> {
    let bits = u32::from_be_bytes(mask.0);
    let ones = bits.leading_ones() as u8;
    let tail_bits = 32 - ones;
    let canonical = if tail_bits == 32 {
        0
    } else {
        u32::MAX << tail_bits
    };
    if bits == canonical { Some(ones) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CidrAddress, Gateway, IpAddr};

    const MAC: [u8; 6] = [0x02, 0x34, 0x56, 0x78, 0x9a, 0xbc];

    #[test]
    fn discover_contains_structured_bootp_header_and_requested_options() {
        let mut client = DhcpClient::new();
        let packet = client.build_discover(&MAC);

        assert_eq!(client.state, DhcpState::Selecting);
        assert_eq!(packet.len(), DHCP_OPTIONS_OFFSET + 10);
        assert_eq!(packet[BOOTP_OP_OFFSET], OP_REQUEST);
        assert_eq!(packet[1], HTYPE_ETHERNET);
        assert_eq!(packet[2], HLEN_ETHERNET);
        assert_eq!(read_u32_at(&packet, BOOTP_XID_OFFSET), client.xid);
        assert_eq!(
            read_u16_at(&packet, BOOTP_FLAGS_OFFSET),
            BOOTP_BROADCAST_FLAG
        );
        assert_eq!(
            &packet[BOOTP_CHADDR_OFFSET..BOOTP_CHADDR_OFFSET + MAC.len()],
            &MAC
        );
        assert_eq!(packet[BOOTP_HEADER_LEN..DHCP_OPTIONS_OFFSET], MAGIC_COOKIE);

        let options = parse_response_options(&packet[DHCP_OPTIONS_OFFSET..]).unwrap();
        assert_eq!(options.message_type, Some(DhcpMessageType::Discover));
        assert!(contains_option_value(
            &packet[DHCP_OPTIONS_OFFSET..],
            OPT_PARAMETER_REQUEST_LIST,
            &REQUESTED_PARAMETERS
        ));
    }

    #[test]
    fn request_reuses_offer_and_server_identifier() {
        let server = Ipv4Addr::new(192, 168, 10, 1);
        let offered = Ipv4Addr::new(192, 168, 10, 42);
        let mut client = DhcpClient::new();
        client.build_discover(&MAC);
        client.offered_ip = Some(offered);

        let packet = client.build_request(&MAC, server);
        assert_eq!(client.state, DhcpState::Requesting);
        assert_eq!(client.offered_server_id, Some(server));

        let options = parse_response_options(&packet[DHCP_OPTIONS_OFFSET..]).unwrap();
        assert_eq!(options.message_type, Some(DhcpMessageType::Request));
        assert_eq!(options.server_id, Some(server));
        assert!(contains_option_value(
            &packet[DHCP_OPTIONS_OFFSET..],
            OPT_REQUESTED_IP,
            &offered.0
        ));
        assert!(contains_option_value(
            &packet[DHCP_OPTIONS_OFFSET..],
            OPT_PARAMETER_REQUEST_LIST,
            &REQUESTED_PARAMETERS
        ));
    }

    #[test]
    fn offer_is_recorded_without_binding_interface() {
        let mut client = DhcpClient::new();
        client.build_discover(&MAC);
        let packet = build_reply(
            client.xid,
            DhcpMessageType::Offer,
            Ipv4Addr::new(10, 0, 2, 15),
            Ipv4Addr::new(10, 0, 2, 1),
            &[
                (OPT_PAD, alloc::vec![]),
                (OPT_SUBNET_MASK, alloc::vec![255, 255, 255, 0]),
                (OPT_ROUTER, alloc::vec![10, 0, 2, 1, 10, 0, 2, 254]),
                (OPT_DNS, alloc::vec![1, 1, 1, 1, 8, 8, 8, 8]),
            ],
        );

        assert!(client.parse_response(&packet, NetInstant::ZERO).is_none());
        assert_eq!(client.state, DhcpState::Selecting);
        assert_eq!(client.offered_ip, Some(Ipv4Addr::new(10, 0, 2, 15)));
        assert_eq!(client.offered_server_id, Some(Ipv4Addr::new(10, 0, 2, 1)));
        assert_eq!(client.offered_gateway, Some(Ipv4Addr::new(10, 0, 2, 1)));
        assert_eq!(client.offered_dns, Some(Ipv4Addr::new(1, 1, 1, 1)));
        assert_eq!(client.subnet_mask, Some(Ipv4Addr::new(255, 255, 255, 0)));
    }

    #[test]
    fn ack_returns_auto_interface_config_with_valid_prefix() {
        let now = NetInstant::from_secs(100);
        let mut client = DhcpClient::new();
        client.build_discover(&MAC);
        let packet = build_reply(
            client.xid,
            DhcpMessageType::Ack,
            Ipv4Addr::new(172, 16, 5, 20),
            Ipv4Addr::new(172, 16, 5, 1),
            &[
                (OPT_SUBNET_MASK, alloc::vec![255, 255, 254, 0]),
                (OPT_ROUTER, alloc::vec![172, 16, 4, 1]),
                (OPT_DNS, alloc::vec![9, 9, 9, 9]),
                (OPT_LEASE_TIME, 7200u32.to_be_bytes().to_vec()),
            ],
        );

        let config = client.parse_response(&packet, now).unwrap();
        assert_eq!(config.mode, IfMode::Auto);
        assert_eq!(
            config.addresses,
            alloc::vec![CidrAddress::new_v4(Ipv4Addr::new(172, 16, 5, 20), 23)]
        );
        assert_eq!(
            config.gateway,
            Some(Gateway::V4(Ipv4Addr::new(172, 16, 4, 1)))
        );
        assert_eq!(client.offered_dns, Some(Ipv4Addr::new(9, 9, 9, 9)));
        assert_eq!(
            client.state,
            DhcpState::Bound {
                lease_expires: now + NetDuration::from_secs(7200),
                server_id: Ipv4Addr::new(172, 16, 5, 1),
            }
        );
    }

    #[test]
    fn invalid_options_do_not_mutate_offer_state() {
        let mut client = DhcpClient::new();
        client.build_discover(&MAC);
        client.offered_ip = Some(Ipv4Addr::new(192, 0, 2, 10));
        let old_offer = client.offered_ip;

        let mut packet = build_reply(
            client.xid,
            DhcpMessageType::Ack,
            Ipv4Addr::new(192, 0, 2, 20),
            Ipv4Addr::new(192, 0, 2, 1),
            &[],
        );
        packet.pop();
        packet.extend_from_slice(&[OPT_ROUTER, 4, 192, 0]);

        assert!(client.parse_response(&packet, NetInstant::ZERO).is_none());
        assert_eq!(client.offered_ip, old_offer);
        assert_eq!(client.state, DhcpState::Selecting);
    }

    #[test]
    fn options_without_end_marker_are_rejected() {
        let mut client = DhcpClient::new();
        client.build_discover(&MAC);
        client.offered_ip = Some(Ipv4Addr::new(203, 0, 113, 9));
        let old_offer = client.offered_ip;

        let mut packet = build_reply(
            client.xid,
            DhcpMessageType::Offer,
            Ipv4Addr::new(203, 0, 113, 20),
            Ipv4Addr::new(203, 0, 113, 1),
            &[],
        );
        packet.pop();

        assert!(client.parse_response(&packet, NetInstant::ZERO).is_none());
        assert_eq!(client.offered_ip, old_offer);
        assert_eq!(client.state, DhcpState::Selecting);
    }

    #[test]
    fn nak_resets_offer_and_returns_to_init() {
        let mut client = DhcpClient::new();
        client.build_discover(&MAC);
        client.offered_ip = Some(Ipv4Addr::new(10, 0, 2, 15));
        client.offered_server_id = Some(Ipv4Addr::new(10, 0, 2, 1));
        client.offered_gateway = Some(Ipv4Addr::new(10, 0, 2, 1));
        client.state = DhcpState::Requesting;
        let packet = build_reply(
            client.xid,
            DhcpMessageType::Nak,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(10, 0, 2, 1),
            &[],
        );

        assert!(client.parse_response(&packet, NetInstant::ZERO).is_none());
        assert_eq!(client.state, DhcpState::Init);
        assert_eq!(client.offered_ip, None);
        assert_eq!(client.offered_server_id, None);
        assert_eq!(client.offered_gateway, None);
    }

    #[test]
    fn non_contiguous_mask_falls_back_to_host_prefix() {
        let mut client = DhcpClient::new();
        client.build_discover(&MAC);
        let packet = build_reply(
            client.xid,
            DhcpMessageType::Ack,
            Ipv4Addr::new(198, 51, 100, 10),
            Ipv4Addr::new(198, 51, 100, 1),
            &[(OPT_SUBNET_MASK, alloc::vec![255, 0, 255, 0])],
        );

        let config = client.parse_response(&packet, NetInstant::ZERO).unwrap();
        assert_eq!(
            config.addresses,
            alloc::vec![CidrAddress {
                addr: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 10)),
                prefix_len: ADDRESS_ONLY_PREFIX_LEN,
            }]
        );
    }

    fn build_reply(
        xid: u32,
        message_type: DhcpMessageType,
        yiaddr: Ipv4Addr,
        server_id: Ipv4Addr,
        extra_options: &[(u8, Vec<u8>)],
    ) -> Vec<u8> {
        let mut packet = Vec::new();
        fill_dhcp_header(&mut packet, OP_REPLY, &MAC, xid, 0);
        packet[BOOTP_YIADDR_OFFSET..BOOTP_YIADDR_OFFSET + 4].copy_from_slice(&yiaddr.0);
        push_message_type(&mut packet, message_type);
        push_ipv4_option(&mut packet, OPT_SERVER_ID, server_id);
        for (code, value) in extra_options {
            if *code == OPT_PAD {
                packet.push(OPT_PAD);
            } else {
                push_option(&mut packet, *code, value);
            }
        }
        finish_options(&mut packet);
        packet
    }

    fn contains_option_value(data: &[u8], code: u8, expected: &[u8]) -> bool {
        let mut found = false;
        parse_options(data, |option| {
            if option.code == code && option.value == expected {
                found = true;
            }
        })
        .unwrap();
        found
    }
}
