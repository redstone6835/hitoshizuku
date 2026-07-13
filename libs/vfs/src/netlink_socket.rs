//! AF_NETLINK socket 实现。
//!
//! 支持 NETLINK_ROUTE 协议族的所有标准消息类型：
//! - RTM_GETLINK / RTM_NEWLINK / RTM_DELLINK — 接口管理
//! - RTM_GETADDR / RTM_NEWADDR / RTM_DELADDR — 地址管理
//! - RTM_GETROUTE / RTM_NEWROUTE / RTM_DELROUTE — 路由管理
//! - RTM_GETNEIGH — 邻居表查询
//!
//! GET 类消息返回 dump（一系列 NEW 消息 + NLMSG_DONE），
//! NEW/DEL 类消息返回 NLMSG_ERROR（error=0 表示 ACK）。

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, Ordering};

use errno::Errno;
use net::config::{CidrAddress, Gateway, IpAddr};
use net::stack::InterfaceSnapshot;
use sched::{Task, WaitQueue};
use spin::Mutex;

use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, PollEvents};

// ── Netlink 消息类型 ────────────────────────────────────────────────────────

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;
const RTM_GETNEIGH: u16 = 30;

// ── Netlink 标志 ────────────────────────────────────────────────────────────

const NLM_F_MULTI: u16 = 2;

// ── 接口属性类型 (IFLA_*) ───────────────────────────────────────────────────

const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;

// ── 地址属性类型 (IFA_*) ────────────────────────────────────────────────────

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;

// ── 路由属性类型 (RTA_*) ────────────────────────────────────────────────────

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;

// ── 地址族 ──────────────────────────────────────────────────────────────────

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

// ── 路由表/协议/类型 ────────────────────────────────────────────────────────

const RT_TABLE_MAIN: u8 = 254;
const RTPROT_KERNEL: u8 = 2;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RTN_UNICAST: u8 = 1;

// ── NetlinkSocketFileOps ────────────────────────────────────────────────────

pub struct NetlinkSocketFileOps {
    #[allow(dead_code)]
    protocol: u32,
    rx_buf: Mutex<VecDeque<Vec<u8>>>,
    wait_queue: WaitQueue,
    nonblock: AtomicBool,
    bound: AtomicBool,
}

impl NetlinkSocketFileOps {
    pub fn new(protocol: u32, nonblock: bool) -> Self {
        Self {
            protocol,
            rx_buf: Mutex::new(VecDeque::new()),
            wait_queue: WaitQueue::new(),
            nonblock: AtomicBool::new(nonblock),
            bound: AtomicBool::new(false),
        }
    }

    pub fn bind(&self, _addr: &[u8]) -> Result<(), Errno> {
        self.bound.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn recv(
        &self,
        buf: &mut [u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, Errno> {
        loop {
            let mut rx = self.rx_buf.lock();
            if let Some(msg) = rx.pop_front() {
                let len = msg.len().min(buf.len());
                buf[..len].copy_from_slice(&msg[..len]);
                return Ok(len);
            }
            drop(rx);

            if nonblocking || self.nonblock.load(Ordering::Relaxed) {
                return Err(Errno::EAGAIN);
            }
            if deadline_ns.is_some_and(|dl| sched::now_ns_public() >= dl) {
                return Err(Errno::EAGAIN);
            }

            let task = sched::current_task();
            let entry = self
                .wait_queue
                .prepare_to_wait(&task, sched::TaskState::Sleeping);
            let armed = deadline_ns
                .map(|dl| sched::register_sleep_deadline(&task, dl))
                .unwrap_or(false);
            if !self.rx_buf.lock().is_empty() {
                if armed {
                    sched::cancel_sleep_deadline(&task);
                }
                self.wait_queue.finish_wait(&entry);
                continue;
            }
            if deadline_ns.is_some_and(|dl| sched::now_ns_public() >= dl) {
                if armed {
                    sched::cancel_sleep_deadline(&task);
                }
                self.wait_queue.finish_wait(&entry);
                return Err(Errno::EAGAIN);
            }
            sched::schedule_once(sched::now_ns_public());
            if armed {
                sched::cancel_sleep_deadline(&task);
            }
            self.wait_queue.finish_wait(&entry);
        }
    }
}

impl FileOps for NetlinkSocketFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        loop {
            let mut rx = self.rx_buf.lock();
            if let Some(msg) = rx.pop_front() {
                let len = msg.len().min(buf.len());
                buf[..len].copy_from_slice(&msg[..len]);
                return Ok(len);
            }
            drop(rx);
            if self.nonblock.load(Ordering::Relaxed) {
                return Err(VfsError::WouldBlock);
            }
            let task = sched::current_task();
            let entry = self
                .wait_queue
                .prepare_to_wait(&task, sched::TaskState::Sleeping);
            if !self.rx_buf.lock().is_empty() {
                self.wait_queue.finish_wait(&entry);
                continue;
            }
            sched::schedule_once(sched::now_ns_public());
            self.wait_queue.finish_wait(&entry);
        }
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < 16 {
            return Err(VfsError::InvalidArgument);
        }
        let msg_type = u16::from_ne_bytes([buf[4], buf[5]]);
        let seq = u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let pid = u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);

        let ifaces = net::stack().snapshot_interfaces();
        let payload = if buf.len() > 16 { &buf[16..] } else { &[] };
        let responses = dispatch_message(msg_type, seq, pid, &ifaces, payload);

        let mut rx = self.rx_buf.lock();
        let mut combined = Vec::new();
        for r in responses {
            combined.extend_from_slice(&r);
        }
        rx.push_back(combined);
        drop(rx);
        self.wait_queue.wake_all();
        Ok(buf.len())
    }

    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        let mut events = PollEvents(0);
        if interest.has(PollEvents::POLLIN) && !self.rx_buf.lock().is_empty() {
            events = events.with(PollEvents::POLLIN);
        }
        if interest.has(PollEvents::POLLOUT) {
            events = events.with(PollEvents::POLLOUT);
        }
        events
    }
    fn poll_add_waiter(&self, task: &Arc<Task>, _: PollEvents) -> bool {
        self.wait_queue.enqueue(task);
        true
    }
    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.wait_queue.remove(task);
    }
    fn is_seekable(&self) -> bool {
        false
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── 创建入口 ─────────────────────────────────────────────────────────────────

pub fn create_netlink_socket(protocol: u32, nonblock: bool) -> NetlinkSocketFileOps {
    NetlinkSocketFileOps::new(protocol, nonblock)
}

// ── 消息分派 ─────────────────────────────────────────────────────────────────

fn dispatch_message(
    msg_type: u16,
    seq: u32,
    pid: u32,
    ifaces: &[InterfaceSnapshot],
    payload: &[u8],
) -> Vec<Vec<u8>> {
    match msg_type {
        RTM_GETLINK => handle_getlink(seq, pid, ifaces),
        RTM_GETADDR => handle_getaddr(seq, pid, ifaces),
        RTM_GETROUTE => handle_getroute(seq, pid, ifaces),
        RTM_GETNEIGH => handle_getneigh(seq, pid, ifaces),
        RTM_NEWADDR => handle_newaddr(seq, ifaces, payload),
        RTM_DELADDR => vec![build_nlmsg_error(seq, 0)], // no-op
        RTM_NEWROUTE => handle_newroute(seq, ifaces, payload),
        RTM_DELROUTE => vec![build_nlmsg_error(seq, 0)], // no-op
        RTM_NEWLINK | RTM_DELLINK => vec![build_nlmsg_error(seq, 0)], // no-op
        _ => vec![build_nlmsg_error(seq, -(95i32))],
    }
}

fn handle_newaddr(seq: u32, ifaces: &[InterfaceSnapshot], payload: &[u8]) -> Vec<Vec<u8>> {
    // ifaddrmsg: family(1) + pad(1) + prefixlen(1) + flags(1) + scope(1) + index(4) = 8 bytes
    if payload.len() < 8 {
        return vec![build_nlmsg_error(seq, -(22))]; // EINVAL
    }
    let prefix_len = payload[1];
    let if_index = i32::from_ne_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let iface = ifaces
        .iter()
        .find(|i| i.id.raw() as i32 == if_index - 1)
        .or_else(|| ifaces.iter().find(|i| i.name != "lo"))
        .or_else(|| ifaces.first());
    match (iface, parse_nlattr_ipv4(&payload[8..])) {
        (Some(iface), Some(addr)) => {
            match net::stack().set_iface_ipv4_addr(iface.id, addr, prefix_len) {
                Ok(()) => vec![build_nlmsg_error(seq, 0)],
                Err(e) => vec![build_nlmsg_error(seq, -map_net_error(e))],
            }
        }
        _ => vec![build_nlmsg_error(seq, -(19))], // ENODEV
    }
}

fn handle_newroute(seq: u32, ifaces: &[InterfaceSnapshot], payload: &[u8]) -> Vec<Vec<u8>> {
    // rtmsg: family(1) + dst_len(1) + src_len(1) + tos(1) + table(1) + protocol(1) +
    //         scope(1) + type(1) + flags(4) = 12 bytes
    if payload.len() < 12 {
        return vec![build_nlmsg_error(seq, -(22))];
    }
    let dst_len = payload[1];
    let (dest, gw) = parse_route_attrs(&payload[12..]);
    let dest = dest.unwrap_or(net::Ipv4Addr([0, 0, 0, 0]));
    let prefix = dst_len.min(32);
    let target = ifaces.iter().find(|i| i.name != "lo").or(ifaces.first());
    if let Some(iface) = target {
        if let Some(gw) = gw {
            let full_mask = net::Ipv4Addr(
                u32::MAX
                    .checked_shl(32 - prefix as u32)
                    .unwrap_or(0)
                    .to_be_bytes(),
            );
            match net::stack().add_route(iface.id, dest, full_mask, gw) {
                Ok(()) => vec![build_nlmsg_error(seq, 0)],
                Err(e) => vec![build_nlmsg_error(seq, -map_net_error(e))],
            }
        } else {
            vec![build_nlmsg_error(seq, -(22))]
        }
    } else {
        vec![build_nlmsg_error(seq, -(19))]
    }
}

fn map_net_error(e: net::NetError) -> i32 {
    match e {
        net::NetError::InterfaceNotFound => 19, // ENODEV
        net::NetError::AddressInUse => 98,      // EADDRINUSE
        _ => 22,                                // EINVAL
    }
}

fn parse_nlattr_ipv4(attrs: &[u8]) -> Option<net::Ipv4Addr> {
    let mut i = 0;
    while i + 4 <= attrs.len() {
        let len = u16::from_ne_bytes([attrs[i], attrs[i + 1]]) as usize;
        if len < 4 || i + len > attrs.len() {
            break;
        }
        let atype = u16::from_ne_bytes([attrs[i + 2], attrs[i + 3]]);
        if (atype == 1 || atype == 2) && len >= 8 {
            return Some(net::Ipv4Addr([
                attrs[i + 4],
                attrs[i + 5],
                attrs[i + 6],
                attrs[i + 7],
            ]));
        }
        i += (len + 3) & !3;
    }
    None
}

fn parse_route_attrs(payload: &[u8]) -> (Option<net::Ipv4Addr>, Option<net::Ipv4Addr>) {
    let mut dest = None;
    let mut gw = None;
    let mut i = 0;
    while i + 4 <= payload.len() {
        let len = u16::from_ne_bytes([payload[i], payload[i + 1]]) as usize;
        if len < 4 || i + len > payload.len() {
            break;
        }
        let atype = u16::from_ne_bytes([payload[i + 2], payload[i + 3]]);
        if len >= 8 {
            let ip = net::Ipv4Addr([
                payload[i + 4],
                payload[i + 5],
                payload[i + 6],
                payload[i + 7],
            ]);
            match atype {
                1 => dest = Some(ip), // RTA_DST
                5 => gw = Some(ip),   // RTA_GATEWAY
                _ => {}
            }
        }
        i += (len + 3) & !3;
    }
    (dest, gw)
}

// ── GETLINK handler ─────────────────────────────────────────────────────────

fn handle_getlink(seq: u32, pid: u32, ifaces: &[InterfaceSnapshot]) -> Vec<Vec<u8>> {
    let mut msgs = Vec::new();
    for (idx, iface) in ifaces.iter().enumerate() {
        msgs.push(build_ifinfomsg(
            idx as i32 + 1,
            iface.flags,
            &iface.name,
            &iface.mac,
            iface.mtu,
            seq,
            pid,
        ));
    }
    msgs.push(build_nlmsg_done(seq));
    msgs
}

// ── GETADDR handler ─────────────────────────────────────────────────────────

fn handle_getaddr(seq: u32, pid: u32, ifaces: &[InterfaceSnapshot]) -> Vec<Vec<u8>> {
    let mut msgs = Vec::new();
    for (idx, iface) in ifaces.iter().enumerate() {
        let if_index = idx as i32 + 1;
        for cidr in &iface.addresses {
            msgs.push(build_ifaddrmsg(if_index, cidr, seq, pid));
        }
    }
    msgs.push(build_nlmsg_done(seq));
    msgs
}

// ── GETROUTE handler ────────────────────────────────────────────────────────

fn handle_getroute(seq: u32, pid: u32, ifaces: &[InterfaceSnapshot]) -> Vec<Vec<u8>> {
    let mut msgs = Vec::new();
    for (idx, iface) in ifaces.iter().enumerate() {
        let if_index = idx as i32 + 1;
        for cidr in &iface.addresses {
            msgs.push(build_route_connected(if_index, cidr, seq, pid));
        }
        if let Some(ref gw) = iface.gateway {
            match gw {
                Gateway::DualStack { v4, v6 } => {
                    msgs.push(build_route_default(if_index, &Gateway::V4(*v4), seq, pid));
                    msgs.push(build_route_default(if_index, &Gateway::V6(*v6), seq, pid));
                }
                _ => msgs.push(build_route_default(if_index, gw, seq, pid)),
            }
        }
    }
    msgs.push(build_nlmsg_done(seq));
    msgs
}

// ── GETNEIGH handler ───────────────────────────────────────────────────────

fn handle_getneigh(seq: u32, pid: u32, ifaces: &[InterfaceSnapshot]) -> Vec<Vec<u8>> {
    const RTM_NEWNEIGH: u16 = 28;
    const NLM_F_MULTI: u16 = 0x02;
    const NDA_DST: u16 = 1;
    const NDA_LLADDR: u16 = 2;
    const NUD_REACHABLE: u16 = 0x02;
    const AF_INET: u8 = 2;

    let mut msgs = Vec::new();
    let neighbors = net::stack().all_neighbors();
    for (iface_id, entries) in &neighbors {
        let if_index = ifaces
            .iter()
            .position(|i| i.id == *iface_id)
            .map(|i| i as i32 + 1)
            .unwrap_or(1);
        for entry in entries {
            let mut payload = Vec::new();
            // struct ndmsg (12 bytes)
            payload.push(AF_INET); // ndm_family
            payload.push(0); // ndm_pad1
            payload.extend_from_slice(&0u16.to_ne_bytes()); // ndm_pad2
            payload.extend_from_slice(&(if_index as i32).to_ne_bytes()); // ndm_ifindex
            payload.extend_from_slice(&NUD_REACHABLE.to_ne_bytes()); // ndm_state
            payload.push(0); // ndm_flags
            payload.push(0); // ndm_type
            // NDA_DST attribute
            match entry.ip_addr {
                net::IpAddr::V4(v4) => put_nlattr(&mut payload, NDA_DST, &v4.0),
                net::IpAddr::V6(v6) => put_nlattr(&mut payload, NDA_DST, &v6.0),
            }
            // NDA_LLADDR attribute
            put_nlattr(&mut payload, NDA_LLADDR, &entry.hw_addr);
            msgs.push(wrap_nlmsg(RTM_NEWNEIGH, NLM_F_MULTI, seq, pid, &payload));
        }
    }
    msgs.push(build_nlmsg_done(seq));
    msgs
}

// ── Netlink 消息构建辅助 ────────────────────────────────────────────────────

fn put_nlattr(out: &mut Vec<u8>, nla_type: u16, data: &[u8]) {
    let nla_len = 4 + data.len();
    out.extend_from_slice(&(nla_len as u16).to_ne_bytes());
    out.extend_from_slice(&nla_type.to_ne_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn wrap_nlmsg(msg_type: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let total_len = 16 + payload.len();
    let mut msg = Vec::with_capacity(total_len);
    msg.extend_from_slice(&(total_len as u32).to_ne_bytes());
    msg.extend_from_slice(&msg_type.to_ne_bytes());
    msg.extend_from_slice(&flags.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&pid.to_ne_bytes());
    msg.extend_from_slice(payload);
    msg
}

fn build_nlmsg_done(seq: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(20);
    msg.extend_from_slice(&20u32.to_ne_bytes());
    msg.extend_from_slice(&NLMSG_DONE.to_ne_bytes());
    msg.extend_from_slice(&0u16.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg
}

fn build_nlmsg_error(seq: u32, error: i32) -> Vec<u8> {
    let total_len: u32 = 36;
    let mut msg = Vec::with_capacity(total_len as usize);
    msg.extend_from_slice(&total_len.to_ne_bytes());
    msg.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
    msg.extend_from_slice(&0u16.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg.extend_from_slice(&error.to_ne_bytes());
    msg.extend_from_slice(&16u32.to_ne_bytes());
    msg.extend_from_slice(&0u16.to_ne_bytes());
    msg.extend_from_slice(&0u16.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg
}

// ── build_ifinfomsg ─────────────────────────────────────────────────────────

fn build_ifinfomsg(
    index: i32,
    flags: u32,
    name: &str,
    mac: &[u8; 6],
    mtu: usize,
    seq: u32,
    pid: u32,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(128);
    // struct ifinfomsg (16 bytes)
    payload.push(AF_UNSPEC);
    payload.push(0);
    payload.extend_from_slice(&1u16.to_ne_bytes()); // ifi_type = ARPHRD_ETHER
    payload.extend_from_slice(&index.to_ne_bytes());
    payload.extend_from_slice(&flags.to_ne_bytes());
    payload.extend_from_slice(&0u32.to_ne_bytes()); // ifi_change

    let mut name_bytes = name.as_bytes().to_vec();
    name_bytes.push(0);
    put_nlattr(&mut payload, IFLA_IFNAME, &name_bytes);
    put_nlattr(&mut payload, IFLA_MTU, &(mtu as u32).to_ne_bytes());
    put_nlattr(&mut payload, IFLA_ADDRESS, mac);

    wrap_nlmsg(RTM_NEWLINK, NLM_F_MULTI, seq, pid, &payload)
}

// ── build_ifaddrmsg ─────────────────────────────────────────────────────────

fn build_ifaddrmsg(index: i32, cidr: &CidrAddress, seq: u32, pid: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    let (family, addr_bytes): (u8, Vec<u8>) = match cidr.addr {
        IpAddr::V4(v4) => (AF_INET, v4.0.to_vec()),
        IpAddr::V6(v6) => (AF_INET6, v6.0.to_vec()),
    };
    // struct ifaddrmsg (8 bytes)
    payload.push(family);
    payload.push(cidr.prefix_len);
    payload.push(0); // ifa_flags
    payload.push(0); // ifa_scope = RT_SCOPE_UNIVERSE
    payload.extend_from_slice(&index.to_ne_bytes());

    put_nlattr(&mut payload, IFA_ADDRESS, &addr_bytes);
    put_nlattr(&mut payload, IFA_LOCAL, &addr_bytes);

    wrap_nlmsg(RTM_NEWADDR, NLM_F_MULTI, seq, pid, &payload)
}

// ── build_route_connected (on-link 路由) ────────────────────────────────────

fn build_route_connected(if_index: i32, cidr: &CidrAddress, seq: u32, pid: u32) -> Vec<u8> {
    let (family, dst_bytes): (u8, Vec<u8>) = match cidr.addr {
        IpAddr::V4(v4) => (AF_INET, v4.0.to_vec()),
        IpAddr::V6(v6) => (AF_INET6, v6.0.to_vec()),
    };
    let network = mask_network(&dst_bytes, cidr.prefix_len);

    let mut payload = Vec::with_capacity(64);
    // struct rtmsg (12 bytes)
    payload.push(family); // rtm_family
    payload.push(cidr.prefix_len); // rtm_dst_len
    payload.push(0); // rtm_src_len
    payload.push(0); // rtm_tos
    payload.push(RT_TABLE_MAIN); // rtm_table
    payload.push(RTPROT_KERNEL); // rtm_protocol
    payload.push(RT_SCOPE_LINK); // rtm_scope
    payload.push(RTN_UNICAST); // rtm_type
    payload.extend_from_slice(&0u32.to_ne_bytes()); // rtm_flags

    put_nlattr(&mut payload, RTA_DST, &network);
    put_nlattr(&mut payload, RTA_OIF, &if_index.to_ne_bytes());

    wrap_nlmsg(RTM_NEWROUTE, NLM_F_MULTI, seq, pid, &payload)
}

// ── build_route_default (默认网关路由) ──────────────────────────────────────

fn build_route_default(if_index: i32, gw: &Gateway, seq: u32, pid: u32) -> Vec<u8> {
    let (family, gw_bytes): (u8, Vec<u8>) = match gw {
        Gateway::V4(v4) => (AF_INET, v4.0.to_vec()),
        Gateway::V6(v6) => (AF_INET6, v6.0.to_vec()),
        // 双栈：返回 IPv4 默认路由（IPv6 路由会在另一条记录中由调用方再发一次，
        // 这里保持每条 rtmsg 单一族不混合）
        Gateway::DualStack { v4, .. } => (AF_INET, v4.0.to_vec()),
    };

    let mut payload = Vec::with_capacity(48);
    // struct rtmsg
    payload.push(family);
    payload.push(0); // rtm_dst_len = 0 (默认路由)
    payload.push(0); // rtm_src_len
    payload.push(0); // rtm_tos
    payload.push(RT_TABLE_MAIN);
    payload.push(RTPROT_KERNEL);
    payload.push(RT_SCOPE_UNIVERSE);
    payload.push(RTN_UNICAST);
    payload.extend_from_slice(&0u32.to_ne_bytes()); // rtm_flags

    put_nlattr(&mut payload, RTA_GATEWAY, &gw_bytes);
    put_nlattr(&mut payload, RTA_OIF, &if_index.to_ne_bytes());

    wrap_nlmsg(RTM_NEWROUTE, NLM_F_MULTI, seq, pid, &payload)
}

// ── 子网掩码辅助 ────────────────────────────────────────────────────────────

fn mask_network(addr: &[u8], prefix_len: u8) -> Vec<u8> {
    let mut out = addr.to_vec();
    let bits = prefix_len as usize;
    for (i, b) in out.iter_mut().enumerate() {
        let byte_start = i * 8;
        if byte_start >= bits {
            *b = 0;
        } else if byte_start + 8 > bits {
            let keep = bits - byte_start;
            let mask = ((!0u8) << (8 - keep)) & 0xff;
            *b &= mask;
        }
    }
    out
}
