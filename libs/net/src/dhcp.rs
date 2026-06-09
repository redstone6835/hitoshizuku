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

use crate::config::{IfConfig, Ipv4Addr};
use crate::time::{NetDuration, NetInstant};
use alloc::vec::Vec;

const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;

const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_END: u8 = 255;

const DHCPDISCOVER: u8 = 1;
const DHCPOFFER: u8 = 2;
const DHCPREQUEST: u8 = 3;
const DHCPACK: u8 = 5;

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
        let mut pkt = Vec::new();
        fill_dhcp_header(&mut pkt, OP_REQUEST, mac, self.xid);
        pkt.extend_from_slice(&[OPT_MESSAGE_TYPE, 1, DHCPDISCOVER]);
        pkt.push(OPT_END);
        pkt
    }

    /// 构建 DHCPREQUEST 消息。
    pub fn build_request(&mut self, mac: &[u8; 6], server_id: Ipv4Addr) -> Vec<u8> {
        self.state = DhcpState::Requesting;
        let mut pkt = Vec::new();
        fill_dhcp_header(&mut pkt, OP_REQUEST, mac, self.xid);
        pkt.extend_from_slice(&[OPT_MESSAGE_TYPE, 1, DHCPREQUEST]);
        pkt.extend_from_slice(&[
            OPT_SERVER_ID,
            4,
            server_id.0[0],
            server_id.0[1],
            server_id.0[2],
            server_id.0[3],
        ]);
        if let Some(ip) = self.offered_ip {
            pkt.extend_from_slice(&[OPT_REQUESTED_IP, 4, ip.0[0], ip.0[1], ip.0[2], ip.0[3]]);
        }
        pkt.push(OPT_END);
        pkt
    }

    /// 解析 DHCP 响应，返回获得的网络配置（仅当收到 ACK 时）。
    pub fn parse_response(&mut self, data: &[u8], now: NetInstant) -> Option<IfConfig> {
        if data.len() < 240 || data[0] != OP_REPLY {
            return None;
        }
        let recv_xid = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if recv_xid != self.xid {
            return None;
        }
        let yiaddr = Ipv4Addr([data[16], data[17], data[18], data[19]]);
        let siaddr = Ipv4Addr([data[20], data[21], data[22], data[23]]);
        let mut msg_type: Option<u8> = None;
        let mut lease_time: Option<u32> = None;
        let mut server_id: Option<Ipv4Addr> = None;
        let mut subnet: Option<Ipv4Addr> = None;
        let mut router: Option<Ipv4Addr> = None;
        let mut dns: Option<Ipv4Addr> = None;

        if data.len() > 236 + 4 && data[236..240] == MAGIC_COOKIE {
            let mut i = 240;
            while i < data.len() {
                let opt = data[i];
                if opt == OPT_END {
                    break;
                }
                if i + 1 >= data.len() {
                    break;
                }
                let len = data[i + 1] as usize;
                if i + 2 + len > data.len() {
                    break;
                }
                match opt {
                    OPT_MESSAGE_TYPE if len >= 1 => msg_type = Some(data[i + 2]),
                    OPT_LEASE_TIME if len >= 4 => {
                        lease_time = Some(u32::from_be_bytes([
                            data[i + 2],
                            data[i + 3],
                            data[i + 4],
                            data[i + 5],
                        ]))
                    }
                    OPT_SERVER_ID if len >= 4 => {
                        server_id = Some(Ipv4Addr([
                            data[i + 2],
                            data[i + 3],
                            data[i + 4],
                            data[i + 5],
                        ]))
                    }
                    OPT_SUBNET_MASK if len >= 4 => {
                        subnet = Some(Ipv4Addr([
                            data[i + 2],
                            data[i + 3],
                            data[i + 4],
                            data[i + 5],
                        ]))
                    }
                    OPT_ROUTER if len >= 4 => {
                        router = Some(Ipv4Addr([
                            data[i + 2],
                            data[i + 3],
                            data[i + 4],
                            data[i + 5],
                        ]))
                    }
                    OPT_DNS if len >= 4 => {
                        dns = Some(Ipv4Addr([
                            data[i + 2],
                            data[i + 3],
                            data[i + 4],
                            data[i + 5],
                        ]))
                    }
                    _ => {}
                }
                i += 2 + len;
            }
        }

        self.offered_ip = Some(yiaddr);
        self.offered_gateway = router.or(Some(siaddr));
        self.offered_dns = dns;
        self.subnet_mask = subnet;

        match msg_type? {
            DHCPOFFER => None, // 需要继续发送 REQUEST
            DHCPACK => {
                let lease_secs = lease_time.unwrap_or(86400) as u64;
                self.state = DhcpState::Bound {
                    lease_expires: now + NetDuration::from_secs(lease_secs),
                    server_id: server_id?,
                };
                let prefix = subnet
                    .map(|m| u32::from_be_bytes(m.0).leading_ones() as u8)
                    .unwrap_or(24);
                let mut config = IfConfig::static_v4(yiaddr, prefix, router);
                config.mode = crate::config::IfMode::Static;
                Some(config)
            }
            _ => None,
        }
    }
}

fn fill_dhcp_header(pkt: &mut Vec<u8>, op: u8, mac: &[u8; 6], xid: u32) {
    pkt.push(op); // 0
    pkt.push(1); // 1 htype: Ethernet
    pkt.push(6); // 2 hlen
    pkt.push(0); // 3 hops
    pkt.extend_from_slice(&xid.to_be_bytes()); // 4-7
    pkt.extend_from_slice(&[0u8; 4]); // 8-11 secs + flags
    pkt.extend_from_slice(&[0u8; 16]); // 12-27 ciaddr+yiaddr+siaddr+giaddr
    pkt.extend_from_slice(mac); // 28-33 chaddr[0..6]
    pkt.extend_from_slice(&[0u8; 10]); // 34-43 chaddr[6..16]
    pkt.extend_from_slice(&[0u8; 192]); // 44-235 sname(64)+file(128)
    pkt.extend_from_slice(&MAGIC_COOKIE); // 236-239
}
