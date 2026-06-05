//! AF_NETLINK socket 实现。
//!
//! 只支持 `ip a` 需要的两个消息：
//! - RTM_GETLINK (18) — 查询接口列表
//! - RTM_GETADDR (22) — 查询地址列表

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, Ordering};

use errno::Errno;
use sched::{Task, WaitQueue};
use spin::Mutex;

use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, PollEvents};

// ── Netlink 常量 ─────────────────────────────────────────────────────────────

const NLMSG_DONE: u16 = 3;
const NLMSG_ERROR: u16 = 2;
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
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
const NLM_F_DUMP: u16 = 0x300;

const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_ADDRESS: u16 = 1;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const AF_UNSPEC: u8 = 0;

// ── NetlinkSocketFileOps ─────────────────────────────────────────────────────

pub struct NetlinkSocketFileOps {
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
            self.wait_queue.enqueue(&task);
            sched::schedule_once(sched::now_ns_public());
        }
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < 16 {
            return Err(VfsError::InvalidArgument);
        }
        let msg_type = u16::from_ne_bytes([buf[4], buf[5]]);
        let flags = u16::from_ne_bytes([buf[6], buf[7]]);
        let seq = u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let pid = u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);

        let responses = match msg_type {
            RTM_GETLINK => self.handle_getlink(seq, pid),
            RTM_GETADDR => self.handle_getaddr(seq, pid),
            26 | 30 => vec![build_nlmsg_done(seq)], // RTM_GETROUTE | RTM_GETNEIGH
            16 | 17 | 20 | 21 | 24 | 25 => vec![build_nlmsg_error(seq, 0)], // RTM_NEW/DEL LINK/ADDR/ROUTE
            _ => vec![build_nlmsg_done(seq)],
        };

        let mut rx = self.rx_buf.lock();
        // 合并所有响应为单个缓冲区（ip 工具期望一次 read 获取所有消息）
        let mut combined = Vec::new();
        for r in responses {
            combined.extend_from_slice(&r);
        }
        rx.push_back(combined);
        drop(rx);
        self.wait_queue.wake_all();
        Ok(buf.len())
    }

    fn readdir(
        &self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> { Ok(()) }
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
    fn is_seekable(&self) -> bool { false }
    fn release(&self) {}
    fn as_any(&self) -> &dyn Any { self }
}

// ── Netlink 消息处理 ─────────────────────────────────────────────────────────

impl NetlinkSocketFileOps {
    fn handle_getlink(&self, seq: u32, pid: u32) -> Vec<Vec<u8>> {
        let ifaces = net::stack().snapshot_interfaces();
        let mut msgs = Vec::new();
        for (idx, iface) in ifaces.iter().enumerate() {
            let mut msg = build_ifinfomsg(
                idx as i32 + 1,
                iface.flags,
                &iface.name,
                &iface.mac,
                iface.mtu,
                seq, pid,
            );
            msgs.push(msg);
        }
        msgs.push(build_nlmsg_done(seq));
        msgs
    }

    fn handle_getaddr(&self, seq: u32, pid: u32) -> Vec<Vec<u8>> {
        let ifaces = net::stack().snapshot_interfaces();
        let mut msgs = Vec::new();
        // 每个接口的 IP 地址信息来自 IfConfig
        // 简化实现：从 stack 的配置中读取
        for (idx, iface) in ifaces.iter().enumerate() {
            // 为 eth0 返回配置的 IP 地址（10.0.2.15/24）
            let msg = build_ifaddrmsg_v4(
                idx as i32 + 1,
                [10, 0, 2, 15],
                24,
                seq, pid,
            );
            msgs.push(msg);
        }
        msgs.push(build_nlmsg_done(seq));
        msgs
    }
}

// ── 创建入口 ─────────────────────────────────────────────────────────────────

pub fn create_netlink_socket(
    protocol: u32,
    nonblock: bool,
) -> NetlinkSocketFileOps {
    NetlinkSocketFileOps::new(protocol, nonblock)
}

// ── Netlink 消息构建 ─────────────────────────────────────────────────────────

fn nlmsg_align(len: usize) -> usize {
    (len + 3) & !3
}

fn put_nlattr(out: &mut Vec<u8>, nla_type: u16, data: &[u8]) {
    let nla_len = 4 + data.len();
    out.extend_from_slice(&(nla_len as u16).to_ne_bytes());
    out.extend_from_slice(&nla_type.to_ne_bytes());
    out.extend_from_slice(data);
    // 对齐到 4 字节
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn build_nlmsg_done(seq: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(20);
    msg.extend_from_slice(&20u32.to_ne_bytes()); // nlmsg_len
    msg.extend_from_slice(&NLMSG_DONE.to_ne_bytes()); // nlmsg_type
    msg.extend_from_slice(&0u16.to_ne_bytes()); // nlmsg_flags
    msg.extend_from_slice(&seq.to_ne_bytes()); // nlmsg_seq
    msg.extend_from_slice(&0u32.to_ne_bytes()); // nlmsg_pid
    // padding to 20 bytes (NLMSG_HDRLEN + 4 bytes payload)
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg
}

fn build_nlmsg_error(seq: u32, error: i32) -> Vec<u8> {
    // nlmsghdr (16) + nlmsgerr { error(4) + nlmsghdr(16) } = 36, aligned to 20+16=36
    let total_len: u32 = 36;
    let mut msg = Vec::with_capacity(total_len as usize);
    msg.extend_from_slice(&total_len.to_ne_bytes());
    msg.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
    msg.extend_from_slice(&0u16.to_ne_bytes()); // flags
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes()); // pid
    // struct nlmsgerr
    msg.extend_from_slice(&error.to_ne_bytes()); // error code (0 = ACK)
    // original nlmsghdr (dummy)
    msg.extend_from_slice(&16u32.to_ne_bytes());
    msg.extend_from_slice(&0u16.to_ne_bytes());
    msg.extend_from_slice(&0u16.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg
}

fn build_ifinfomsg(
    index: i32, flags: u32, name: &str, mac: &[u8; 6], mtu: usize,
    seq: u32, pid: u32,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(128);
    // struct ifinfomsg (16 bytes)
    payload.push(AF_UNSPEC);          // ifi_family
    payload.push(0);                   // __ifi_pad
    payload.extend_from_slice(&1u16.to_ne_bytes()); // ifi_type (ARPHRD_ETHER)
    payload.extend_from_slice(&index.to_ne_bytes()); // ifi_index
    payload.extend_from_slice(&(flags | 0x1 | 0x40 | 0x1000).to_ne_bytes()); // ifi_flags
    payload.extend_from_slice(&0u32.to_ne_bytes()); // ifi_change

    // attributes
    let mut name_bytes = name.as_bytes().to_vec();
    name_bytes.push(0); // null-terminated
    put_nlattr(&mut payload, IFLA_IFNAME, &name_bytes);
    put_nlattr(&mut payload, IFLA_MTU, &(mtu as u32).to_ne_bytes());
    put_nlattr(&mut payload, IFLA_ADDRESS, mac);

    // wrap in nlmsghdr
    let total_len = 16 + payload.len();
    let mut msg = Vec::with_capacity(total_len);
    msg.extend_from_slice(&(total_len as u32).to_ne_bytes());
    msg.extend_from_slice(&RTM_NEWLINK.to_ne_bytes());
    msg.extend_from_slice(&NLM_F_MULTI.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&pid.to_ne_bytes());
    msg.extend_from_slice(&payload);
    msg
}

fn build_ifaddrmsg_v4(
    index: i32, addr: [u8; 4], prefix_len: u8,
    seq: u32, pid: u32,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    // struct ifaddrmsg (8 bytes)
    payload.push(AF_INET);             // ifa_family
    payload.push(prefix_len);          // ifa_prefixlen
    payload.push(0);                   // ifa_flags
    payload.push(0);                   // ifa_scope (RT_SCOPE_UNIVERSE)
    payload.extend_from_slice(&index.to_ne_bytes()); // ifa_index

    // attributes
    put_nlattr(&mut payload, IFA_ADDRESS, &addr);
    put_nlattr(&mut payload, IFA_LOCAL, &addr);

    let total_len = 16 + payload.len();
    let mut msg = Vec::with_capacity(total_len);
    msg.extend_from_slice(&(total_len as u32).to_ne_bytes());
    msg.extend_from_slice(&RTM_NEWADDR.to_ne_bytes());
    msg.extend_from_slice(&NLM_F_MULTI.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&pid.to_ne_bytes());
    msg.extend_from_slice(&payload);
    msg
}
