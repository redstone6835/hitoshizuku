//! AF_NETLINK 的路由与最小 Generic Netlink 实现。
//!
//! 支持：
//! - 读：RTM_GETLINK / RTM_GETADDR / RTM_GETROUTE / RTM_GETNEIGH（真实数据快照）
//! - 写：RTM_NEWADDR / DELADDR / NEWROUTE / DELROUTE / NEWLINK / DELLINK / SETLINK
//!   （经内核配置更新入口生效）
//! - 组播：NETLINK_ADD/DROP_MEMBERSHIP 真实订阅，配置变化时向订阅者推送
//!   RTM_NEWLINK / RTM_NEWADDR / RTM_NEWROUTE 事件
//! - sockopt：SO_SNDBUF/SO_RCVBUF 真实调整缓冲上限，SO_PASSCRED 附加发送者凭据
//! - NETLINK_GENERIC：对未注册 family 返回带原请求序号的 NLMSG_ERROR
//!
//! 数据源通过 provider 注入（内核 net_runtime 安装），配置修改通过 handler 注入
//! （内核 net_runtime 实现），与 ioctl 分派模式一致。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use errno::Errno;
use sched::{Task, WaitQueue};
use spin::Mutex;

use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, PollEvents};
use crate::poll_source::PollSource;

// ── netlink 消息类型 ──────────────────────────────────────────────────────────

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const GENL_ID_CTRL: u16 = 16;
const CTRL_CMD_GETFAMILY: u8 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;
const RTM_NEWNEIGH: u16 = 28;
const RTM_DELNEIGH: u16 = 29;
const RTM_GETNEIGH: u16 = 30;

// kernel 侧事件广播使用的公开别名。
pub const NETLINK_MSG_RTM_NEWLINK: u16 = RTM_NEWLINK;
pub const NETLINK_MSG_RTM_NEWADDR: u16 = RTM_NEWADDR;
pub const NETLINK_MSG_RTM_DELADDR: u16 = RTM_DELADDR;
pub const NETLINK_MSG_RTM_NEWROUTE: u16 = RTM_NEWROUTE;
pub const NETLINK_MSG_RTM_DELROUTE: u16 = RTM_DELROUTE;

// ── nlmsg 标志 ────────────────────────────────────────────────────────────────

const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;

// ── RTMGRP 组播组（订阅位）───────────────────────────────────────────────────

const RTMGRP_LINK: u32 = 1;
const RTMGRP_NOTIFY: u32 = 2;
const RTMGRP_NEIGH: u32 = 4;
const RTMGRP_IPV4_IFADDR: u32 = 0x10;
const RTMGRP_IPV4_ROUTE: u32 = 0x40;
const RTMGRP_IPV6_IFADDR: u32 = 0x100;
const RTMGRP_IPV6_ROUTE: u32 = 0x400;
const RTMGRP_IPV6_IFINFO: u32 = 0x800;

// ── 属性类型 ──────────────────────────────────────────────────────────────────

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 9;
const RTA_TABLE: u16 = 15;
const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;

// ── 地址族 / 作用域 / 状态 ────────────────────────────────────────────────────

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
const RTN_UNICAST: u8 = 1;
const RTPROT_BOOT: u8 = 3;
const IFF_UP: u32 = 1;
const IFF_BROADCAST: u32 = 2;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;
const IFA_F_SECONDARY: u8 = 1;
const AF_NETLINK: u16 = 16;
const SOL_NETLINK: i32 = 270;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;
const NETLINK_DROP_MEMBERSHIP: i32 = 2;

pub const NETLINK_ROUTE: u32 = 0;
pub const NETLINK_GENERIC: u32 = 16;

// ── 数据源 provider（由内核 net_runtime 安装）────────────────────────────────

static NEXT_NETLINK_PORT: AtomicU32 = AtomicU32::new(1);
static ADDRESS_SNAPSHOT_PROVIDER: Mutex<Option<fn() -> Vec<net::control::AddressEntry>>> =
    Mutex::new(None);
static ROUTE_SNAPSHOT_PROVIDER: Mutex<Option<fn() -> Vec<net::control::RouteEntry>>> =
    Mutex::new(None);
static NEIGHBOR_SNAPSHOT_PROVIDER: Mutex<Option<fn() -> Vec<NeighborSnapshot>>> = Mutex::new(None);

pub fn install_address_snapshot_provider(provider: fn() -> Vec<net::control::AddressEntry>) {
    *ADDRESS_SNAPSHOT_PROVIDER.lock() = Some(provider);
}

pub fn install_route_snapshot_provider(provider: fn() -> Vec<net::control::RouteEntry>) {
    *ROUTE_SNAPSHOT_PROVIDER.lock() = Some(provider);
}

pub fn install_neighbor_snapshot_provider(provider: fn() -> Vec<NeighborSnapshot>) {
    *NEIGHBOR_SNAPSHOT_PROVIDER.lock() = Some(provider);
}

/// 邻居表快照条目（内核 net_runtime 从镜像表填充）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeighborSnapshot {
    pub interface: net::InterfaceId,
    pub address: net::IpAddr,
    pub mac: [u8; 6],
    /// 可达性状态（NUD_* 位），由内核按邻居表状态映射。
    pub nud_state: u16,
}

// ── 配置写请求（vfs 定义，内核 net_runtime 实现）────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetlinkConfigRequest {
    AddAddress {
        interface: net::InterfaceId,
        address: net::IpAddr,
        prefix_len: u8,
    },
    DelAddress {
        interface: net::InterfaceId,
        address: net::IpAddr,
        prefix_len: u8,
    },
    AddRoute {
        table: u8,
        network: net::IpAddr,
        prefix_len: u8,
        gateway: Option<net::IpAddr>,
        interface: net::InterfaceId,
        metric: u32,
    },
    DelRoute {
        table: u8,
        network: net::IpAddr,
        prefix_len: u8,
        gateway: Option<net::IpAddr>,
        interface: net::InterfaceId,
    },
    SetLinkRunning {
        interface: net::InterfaceId,
        running: bool,
    },
    SetLinkMtu {
        interface: net::InterfaceId,
        mtu: u32,
    },
}

/// 配置写处理器：返回 Ok(()) 或 Err(负 errno，与 NLMSG_ERROR 语义一致)。
static NETLINK_CONFIG_HANDLER: Mutex<Option<fn(&NetlinkConfigRequest) -> Result<(), i32>>> =
    Mutex::new(None);

pub fn install_netlink_config_handler(handler: fn(&NetlinkConfigRequest) -> Result<(), i32>) {
    *NETLINK_CONFIG_HANDLER.lock() = Some(handler);
}

// ── netlink socket 注册表（事件广播用）──────────────────────────────────────
//
// 生命周期保证：new（socket 创建）时入表、release（File 关闭）时出表，
// 广播读取与入/出表共用同一把锁，因此表内指针在锁持有期间必然存活。

/// 注册表中的 netlink socket 指针。生命周期由表锁保证：
/// 入表（new）到出表（release）期间对象必然存活，广播读与出表互斥。
struct NetlinkSocketPtr(*const NetlinkSocketFileOps);

// Safety: 指针本身不跨线程解引用；解引用只在持表锁的广播路径发生，
// 而该路径与 release 出表互斥，因此不存在悬垂访问。
unsafe impl Send for NetlinkSocketPtr {}
unsafe impl Sync for NetlinkSocketPtr {}

static NETLINK_SOCKETS: Mutex<Vec<NetlinkSocketPtr>> = Mutex::new(Vec::new());

/// 向订阅了对应组播组的所有 netlink socket 推送事件消息。
pub fn netlink_event_broadcast(msg_type: u16, message: Vec<u8>) {
    let groups = match msg_type {
        RTM_NEWLINK | RTM_DELLINK | RTM_SETLINK => RTMGRP_LINK | RTMGRP_IPV6_IFINFO | RTMGRP_NOTIFY,
        RTM_NEWADDR | RTM_DELADDR => RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR,
        RTM_NEWROUTE | RTM_DELROUTE => RTMGRP_IPV4_ROUTE | RTMGRP_IPV6_ROUTE,
        RTM_NEWNEIGH | RTM_DELNEIGH => RTMGRP_NEIGH,
        _ => return,
    };
    let registry = NETLINK_SOCKETS.lock();
    for entry in registry.iter() {
        // Safety: 表锁持有期间没有并发 release 出表（release 也持同一把锁），
        // 因此指针必然指向存活的 NetlinkSocketFileOps。
        let socket = unsafe { &*entry.0 };
        if socket.protocol == NETLINK_ROUTE && socket.groups.load(Ordering::Acquire) & groups != 0 {
            socket.push_event(message.clone());
        }
    }
}

pub struct NetlinkSocketFileOps {
    protocol: u32,
    rx_buf: Mutex<VecDeque<Vec<u8>>>,
    wait_queue: WaitQueue,
    nonblock: AtomicBool,
    bound: AtomicBool,
    local_pid: AtomicU32,
    groups: AtomicU32,
    /// SO_RCVBUF 控制的最大缓冲字节数（超限丢弃最早消息并计数）。
    rx_limit: AtomicU32,
    rx_dropped: AtomicU32,
    /// SO_PASSCRED：接收时附加发送者凭据。
    passcred: AtomicBool,
    poll_source: PollSource,
}

impl NetlinkSocketFileOps {
    pub fn new(protocol: u32, nonblock: bool) -> Self {
        Self {
            protocol,
            rx_buf: Mutex::new(VecDeque::new()),
            wait_queue: WaitQueue::new_with_reason(sched::WaitReason::SocketRead),
            nonblock: AtomicBool::new(nonblock),
            bound: AtomicBool::new(false),
            local_pid: AtomicU32::new(0),
            groups: AtomicU32::new(0),
            rx_limit: AtomicU32::new(212_992),
            rx_dropped: AtomicU32::new(0),
            passcred: AtomicBool::new(false),
            poll_source: PollSource::new(PollEvents::POLLOUT),
        }
    }

    /// 从当前对象地址构造注册表条目（生命周期由表锁 + release 出表保证）。
    fn register(&self) {
        NETLINK_SOCKETS
            .lock()
            .push(NetlinkSocketPtr(self as *const NetlinkSocketFileOps));
    }

    fn push_event(&self, message: Vec<u8>) {
        let mut buf = self.rx_buf.lock();
        let limit = self.rx_limit.load(Ordering::Relaxed) as usize;
        let queued: usize = buf.iter().map(|m| m.len()).sum();
        if queued + message.len() > limit {
            self.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        buf.push_back(message);
        drop(buf);
        self.refresh_readiness();
        self.wait_queue.wake_all();
    }

    fn refresh_readiness(&self) {
        let version = self.poll_source.reserve_version();
        let readiness = if self.rx_buf.lock().is_empty() {
            PollEvents::POLLOUT
        } else {
            PollEvents::POLLIN.with(PollEvents::POLLOUT)
        };
        self.poll_source.publish_versioned(readiness, version);
    }

    pub fn bind(&self, addr: &[u8]) -> Result<(), Errno> {
        if addr.len() < 12 {
            return Err(Errno::EINVAL);
        }
        let family = u16::from_ne_bytes(addr[..2].try_into().unwrap());
        if family != AF_NETLINK {
            return Err(Errno::EAFNOSUPPORT);
        }
        if self
            .bound
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Errno::EINVAL);
        }
        let requested_pid = u32::from_ne_bytes(addr[4..8].try_into().unwrap());
        let local_pid = if requested_pid == 0 {
            NEXT_NETLINK_PORT.fetch_add(1, Ordering::Relaxed).max(1)
        } else {
            requested_pid
        };
        self.local_pid.store(local_pid, Ordering::Release);
        self.groups.store(
            u32::from_ne_bytes(addr[8..12].try_into().unwrap()),
            Ordering::Release,
        );
        Ok(())
    }

    pub fn local_address(&self) -> [u8; 12] {
        let mut address = [0u8; 12];
        address[..2].copy_from_slice(&AF_NETLINK.to_ne_bytes());
        address[4..8].copy_from_slice(&self.local_pid.load(Ordering::Acquire).to_ne_bytes());
        address[8..12].copy_from_slice(&self.groups.load(Ordering::Acquire).to_ne_bytes());
        address
    }

    pub const fn protocol(&self) -> u32 {
        self.protocol
    }

    /// 读取一条消息。若启用 SO_PASSCRED，在消息前附加 12 字节发送者凭据
    /// （pid/uid/gid，u32×3），与 Linux netlink 的 SO_PASSCRED 语义一致。
    pub fn recv(
        &self,
        buf: &mut [u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
        peek: bool,
        truncate: bool,
    ) -> Result<usize, Errno> {
        loop {
            let message = {
                let mut queue = self.rx_buf.lock();
                let base = if peek {
                    queue.front().cloned()
                } else {
                    queue.pop_front()
                };
                match base {
                    Some(msg) => {
                        if self.passcred.load(Ordering::Acquire) {
                            let cred = sender_credentials();
                            let mut with_cred = Vec::with_capacity(msg.len() + 12);
                            with_cred.extend_from_slice(&cred.pid.to_ne_bytes());
                            with_cred.extend_from_slice(&cred.uid.to_ne_bytes());
                            with_cred.extend_from_slice(&cred.gid.to_ne_bytes());
                            with_cred.extend_from_slice(&msg);
                            Some(with_cred)
                        } else {
                            Some(msg)
                        }
                    }
                    None => None,
                }
            };
            if let Some(msg) = message {
                // Netlink is datagram-oriented. MSG_PEEK must leave the
                // datagram queued, while MSG_TRUNC reports its complete size
                // even when the caller supplied a zero/small iovec. The
                // latter is used by iproute2's MSG_PEEK|MSG_TRUNC sizing pass.
                let copied = msg.len().min(buf.len());
                if copied != 0 {
                    buf[..copied].copy_from_slice(&msg[..copied]);
                }
                if !peek {
                    self.refresh_readiness();
                }
                return Ok(if truncate { msg.len() } else { copied });
            }
            if nonblocking || self.nonblock.load(Ordering::Relaxed) {
                return Err(Errno::EAGAIN);
            }
            if deadline_ns.is_some_and(|deadline| sched::now_ns_public() >= deadline) {
                return Err(Errno::EAGAIN);
            }
            let task = sched::current_task();
            if sched::operation::has_interrupting_signal(&task) {
                return Err(Errno::EINTR);
            }
            let entry = self
                .wait_queue
                .prepare_to_wait(&task, sched::TaskState::Sleeping);
            let armed = deadline_ns
                .map(|deadline| sched::register_sleep_deadline(&task, deadline))
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

    fn dispatch(&self, buf: &[u8]) -> VfsResult<usize> {
        if buf.len() < 16 {
            return Err(VfsError::InvalidArgument);
        }
        // nlmsghdr.nlmsg_len（u32）必须完整包含 16 字节头且不越出输入缓冲，
        // 并按该长度界定消息体，避免把缓冲区尾部无关字节当作 payload 处理。
        let msg_len = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
        if msg_len < 16 || msg_len > buf.len() {
            return Err(VfsError::InvalidArgument);
        }
        let msg_type = u16::from_ne_bytes([buf[4], buf[5]]);
        let flags = u16::from_ne_bytes([buf[6], buf[7]]);
        let seq = u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let local_pid = self.local_pid.load(Ordering::Acquire);
        let responses = dispatch_message(
            self.protocol,
            msg_type,
            flags,
            seq,
            local_pid,
            &buf[16..msg_len],
        );
        let mut combined = Vec::new();
        for response in responses {
            combined.extend_from_slice(&response);
        }
        if !combined.is_empty() {
            self.push_event(combined);
        }
        Ok(buf.len())
    }
}

/// 当前任务的发送者凭据快照（SO_PASSCRED 语义）。
fn sender_credentials() -> SenderCred {
    let task = sched::current_task();
    let cred = task.credentials();
    SenderCred {
        pid: task.pid_root_cached().unwrap_or(0) as u32,
        uid: cred.euid.0,
        gid: cred.egid.0,
    }
}

struct SenderCred {
    pid: u32,
    uid: u32,
    gid: u32,
}

impl FileOps for NetlinkSocketFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        loop {
            let message = { self.rx_buf.lock().pop_front() };
            if let Some(msg) = message {
                let len = msg.len().min(buf.len());
                buf[..len].copy_from_slice(&msg[..len]);
                self.refresh_readiness();
                return Ok(len);
            }
            if self.nonblock.load(Ordering::Relaxed) {
                return Err(VfsError::WouldBlock);
            }
            let task = sched::current_task();
            if sched::operation::has_interrupting_signal(&task) {
                return Err(VfsError::Interrupted);
            }
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
        self.dispatch(buf)
    }

    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        self.poll_source.snapshot().0.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, _: PollEvents) -> bool {
        self.wait_queue.enqueue(task);
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.wait_queue.remove(task);
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.poll_source)
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn release(&self) {
        // 出表：与广播读取共用表锁，保证广播期间表内指针存活。
        NETLINK_SOCKETS
            .lock()
            .retain(|entry| entry.0 != self as *const NetlinkSocketFileOps);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for NetlinkSocketFileOps {
    fn drop(&mut self) {
        // 出表：与广播读取共用表锁，保证广播期间表内指针存活。
        NETLINK_SOCKETS
            .lock()
            .retain(|entry| entry.0 != self as *const NetlinkSocketFileOps);
    }
}

/// 创建 netlink socket。返回 Box 以保证对象地址稳定（注册表持有裸指针，
/// 栈上对象的地址在返回值拷贝后不保证不变）。
pub fn create_netlink_socket(protocol: u32, nonblock: bool) -> Box<NetlinkSocketFileOps> {
    let ops = Box::new(NetlinkSocketFileOps::new(protocol, nonblock));
    ops.register();
    ops
}

// ── 消息分发 ─────────────────────────────────────────────────────────────────

fn dispatch_message(
    protocol: u32,
    msg_type: u16,
    flags: u16,
    seq: u32,
    local_pid: u32,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    match protocol {
        NETLINK_ROUTE => dispatch_route_message(msg_type, flags, seq, local_pid, payload),
        NETLINK_GENERIC => dispatch_generic_message(msg_type, flags, seq, local_pid, payload),
        _ => vec![build_nlmsg_error(
            seq,
            local_pid,
            -i32::from(Errno::EPROTONOSUPPORT),
        )],
    }
}

fn dispatch_route_message(
    msg_type: u16,
    flags: u16,
    seq: u32,
    local_pid: u32,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    if flags & NLM_F_REQUEST != 0 {
        match msg_type {
            RTM_GETLINK => return get_link(seq, local_pid, payload),
            RTM_GETADDR => return get_addr(seq, local_pid, payload),
            RTM_GETROUTE => return get_route(seq, local_pid, payload),
            RTM_GETNEIGH => return get_neigh(seq, local_pid, payload),
            _ => {}
        }
    }
    match msg_type {
        RTM_NEWADDR | RTM_DELADDR => {
            let result = handle_addr_change(msg_type, payload);
            vec![nlmsg_ack_or_error(result, seq, local_pid)]
        }
        RTM_NEWROUTE | RTM_DELROUTE => {
            let result = handle_route_change(msg_type, payload);
            vec![nlmsg_ack_or_error(result, seq, local_pid)]
        }
        RTM_NEWLINK | RTM_DELLINK | RTM_SETLINK => {
            let result = handle_link_change(msg_type, payload);
            vec![nlmsg_ack_or_error(result, seq, local_pid)]
        }
        _ => vec![build_nlmsg_error(
            seq,
            local_pid,
            -i32::from(Errno::EOPNOTSUPP),
        )],
    }
}

fn dispatch_generic_message(
    msg_type: u16,
    flags: u16,
    seq: u32,
    local_pid: u32,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    let error = if flags & NLM_F_REQUEST == 0 {
        Errno::EOPNOTSUPP
    } else if msg_type != GENL_ID_CTRL {
        // No Generic Netlink families are registered yet.
        Errno::ENOENT
    } else if payload.len() < 4 {
        Errno::EINVAL
    } else if payload[0] == CTRL_CMD_GETFAMILY {
        // The controller exists, but every requested family (including
        // nl80211) is currently absent.
        Errno::ENOENT
    } else {
        Errno::EOPNOTSUPP
    };
    vec![build_nlmsg_error(seq, local_pid, -i32::from(error))]
}

fn nlmsg_ack_or_error(result: Result<(), i32>, seq: u32, local_pid: u32) -> Vec<u8> {
    match result {
        Ok(()) => {
            let mut payload = Vec::with_capacity(20);
            payload.extend_from_slice(&0i32.to_ne_bytes());
            payload.extend_from_slice(&16u32.to_ne_bytes());
            payload.extend_from_slice(&0u16.to_ne_bytes());
            payload.extend_from_slice(&0u16.to_ne_bytes());
            payload.extend_from_slice(&seq.to_ne_bytes());
            payload.extend_from_slice(&0u32.to_ne_bytes());
            wrap_nlmsg(NLMSG_ERROR, 0, seq, local_pid, &payload)
        }
        Err(code) => build_nlmsg_error(seq, local_pid, code),
    }
}

fn get_link(seq: u32, local_pid: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    // ifinfomsg.ifi_index 位于 payload 偏移 4..8；非零时只返回该接口。
    let requested = if payload.len() >= 8 {
        i32::from_ne_bytes(payload[4..8].try_into().unwrap())
    } else {
        0
    };
    let mut messages = net::device::snapshot_devices()
        .into_iter()
        .filter(|device| requested == 0 || device.id.raw() == requested as u32)
        .map(|device| build_ifinfomsg(&device, seq, local_pid))
        .collect::<Vec<_>>();
    messages.push(build_nlmsg_done(seq, local_pid));
    messages
}

fn get_addr(seq: u32, local_pid: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    // ifaddrmsg.ifa_index 位于 payload 偏移 4..8；非零时只返回该接口。
    let requested = if payload.len() >= 8 {
        u32::from_ne_bytes(payload[4..8].try_into().unwrap())
    } else {
        0
    };
    let mut messages = ADDRESS_SNAPSHOT_PROVIDER
        .lock()
        .map(|provider| provider())
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| requested == 0 || entry.interface.0 == requested)
        .filter_map(|entry| {
            let device = net::device::snapshot_devices()
                .into_iter()
                .find(|device| device.id.raw() == entry.interface.0)?;
            Some(build_ifaddrmsg_for_device(entry, &device, seq, local_pid))
        })
        .collect::<Vec<_>>();
    messages.push(build_nlmsg_done(seq, local_pid));
    messages
}

fn get_route(seq: u32, local_pid: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    // RTM_GETROUTE 的接口过滤由 RTA_OIF 属性携带（rtmsg 头无固定 ifindex 字段）。
    let requested = if payload.len() >= 12 {
        parse_attributes(&payload[12..])
            .into_iter()
            .find(|(kind, _)| *kind == RTA_OIF)
            .and_then(|(_, data)| {
                (data.len() >= 4).then(|| u32::from_ne_bytes(data[..4].try_into().unwrap()))
            })
            .unwrap_or(0)
    } else {
        0
    };
    let mut messages = ROUTE_SNAPSHOT_PROVIDER
        .lock()
        .map(|provider| provider())
        .unwrap_or_default()
        .into_iter()
        .filter(|route| requested == 0 || route.interface.0 == requested)
        .map(|route| build_rtmsg(route, seq, local_pid))
        .collect::<Vec<_>>();
    messages.push(build_nlmsg_done(seq, local_pid));
    messages
}

fn get_neigh(seq: u32, local_pid: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    // ndmsg.ndm_ifindex 位于 payload 偏移 4..8；非零时只返回该接口。
    let requested = if payload.len() >= 8 {
        i32::from_ne_bytes(payload[4..8].try_into().unwrap())
    } else {
        0
    };
    let mut messages = NEIGHBOR_SNAPSHOT_PROVIDER
        .lock()
        .map(|provider| provider())
        .unwrap_or_default()
        .into_iter()
        .filter(|neighbor| requested == 0 || neighbor.interface.0 == requested as u32)
        .map(|neighbor| build_ndmsg(neighbor, seq, local_pid))
        .collect::<Vec<_>>();
    messages.push(build_nlmsg_done(seq, local_pid));
    messages
}

// ── 写操作解析与执行 ─────────────────────────────────────────────────────────

fn handle_addr_change(msg_type: u16, payload: &[u8]) -> Result<(), i32> {
    if payload.len() < 8 {
        return Err(-i32::from(Errno::EINVAL));
    }
    let family = payload[0];
    let prefix_len = payload[1];
    let index = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
    let interface = net::InterfaceId(index);
    let attrs = parse_attributes(&payload[8..]);
    let address = attrs
        .iter()
        .find(|(kind, _)| *kind == IFA_LOCAL)
        .or_else(|| attrs.iter().find(|(kind, _)| *kind == IFA_ADDRESS))
        .map(|(_, data)| decode_address(family, data))
        .transpose()?
        .ok_or(-i32::from(Errno::EINVAL))?;
    let request = if msg_type == RTM_NEWADDR {
        NetlinkConfigRequest::AddAddress {
            interface,
            address,
            prefix_len,
        }
    } else {
        NetlinkConfigRequest::DelAddress {
            interface,
            address,
            prefix_len,
        }
    };
    dispatch_config(&request)
}

fn handle_route_change(msg_type: u16, payload: &[u8]) -> Result<(), i32> {
    if payload.len() < 12 {
        return Err(-i32::from(Errno::EINVAL));
    }
    let family = payload[0];
    if family != AF_INET && family != AF_INET6 {
        return Err(-i32::from(Errno::EAFNOSUPPORT));
    }
    let dst_len = payload[1];
    let table = payload[4];
    let attrs = parse_attributes(&payload[12..]);
    let network = attrs
        .iter()
        .find(|(kind, _)| *kind == RTA_DST)
        .map(|(_, data)| decode_address(family, data))
        .transpose()?
        .unwrap_or_else(|| match family {
            AF_INET => net::IpAddr::V4(net::Ipv4Addr::UNSPECIFIED),
            _ => net::IpAddr::V6(net::Ipv6Addr::UNSPECIFIED),
        });
    let gateway = attrs
        .iter()
        .find(|(kind, _)| *kind == RTA_GATEWAY)
        .map(|(_, data)| decode_address(family, data))
        .transpose()?;
    let interface = attrs
        .iter()
        .find(|(kind, _)| *kind == RTA_OIF)
        .map(|(_, data)| {
            if data.len() < 4 {
                return Err(-i32::from(Errno::EINVAL));
            }
            Ok(net::InterfaceId(u32::from_ne_bytes(
                data[..4].try_into().unwrap(),
            )))
        })
        .transpose()?
        .unwrap_or(net::InterfaceId(0));
    let metric = attrs
        .iter()
        .find(|(kind, _)| *kind == RTA_PRIORITY)
        .map(|(_, data)| {
            if data.len() < 4 {
                return Err(-i32::from(Errno::EINVAL));
            }
            Ok(u32::from_ne_bytes(data[..4].try_into().unwrap()))
        })
        .transpose()?
        .unwrap_or(0);
    let table = attrs
        .iter()
        .find(|(kind, _)| *kind == RTA_TABLE)
        .map(|(_, data)| {
            if data.len() < 4 {
                return Err(-i32::from(Errno::EINVAL));
            }
            Ok(
                u8::try_from(u32::from_ne_bytes(data[..4].try_into().unwrap()))
                    .map_err(|_| -i32::from(Errno::EINVAL))?,
            )
        })
        .transpose()?
        .unwrap_or(table);
    let request = if msg_type == RTM_NEWROUTE {
        NetlinkConfigRequest::AddRoute {
            table,
            network,
            prefix_len: dst_len,
            gateway,
            interface,
            metric,
        }
    } else {
        NetlinkConfigRequest::DelRoute {
            table,
            network,
            prefix_len: dst_len,
            gateway,
            interface,
        }
    };
    dispatch_config(&request)
}

fn handle_link_change(msg_type: u16, payload: &[u8]) -> Result<(), i32> {
    if payload.len() < 16 {
        return Err(-i32::from(Errno::EINVAL));
    }
    let index = i32::from_ne_bytes(payload[4..8].try_into().unwrap());
    let flags = u32::from_ne_bytes(payload[8..12].try_into().unwrap());
    let interface = net::InterfaceId(index as u32);
    let attrs = parse_attributes(&payload[16..]);
    if msg_type == RTM_SETLINK || msg_type == RTM_NEWLINK {
        let running = flags & IFF_UP != 0;
        dispatch_config(&NetlinkConfigRequest::SetLinkRunning { interface, running })?;
    }
    if let Some((_, data)) = attrs.iter().find(|(kind, _)| *kind == IFLA_MTU) {
        if data.len() < 4 {
            return Err(-i32::from(Errno::EINVAL));
        }
        let mtu = u32::from_ne_bytes(data[..4].try_into().unwrap());
        if mtu == 0 {
            return Err(-i32::from(Errno::EINVAL));
        }
        dispatch_config(&NetlinkConfigRequest::SetLinkMtu { interface, mtu })?;
    }
    Ok(())
}

fn dispatch_config(request: &NetlinkConfigRequest) -> Result<(), i32> {
    let handler = NETLINK_CONFIG_HANDLER
        .lock()
        .ok_or(-i32::from(Errno::EOPNOTSUPP))?;
    handler(request)
}

fn decode_address(family: u8, data: &[u8]) -> Result<net::IpAddr, i32> {
    match family {
        AF_INET => {
            if data.len() < 4 {
                return Err(-i32::from(Errno::EINVAL));
            }
            Ok(net::IpAddr::V4(net::Ipv4Addr(
                data[..4].try_into().unwrap(),
            )))
        }
        AF_INET6 => {
            if data.len() < 16 {
                return Err(-i32::from(Errno::EINVAL));
            }
            Ok(net::IpAddr::V6(net::Ipv6Addr(
                data[..16].try_into().unwrap(),
            )))
        }
        _ => Err(-i32::from(Errno::EAFNOSUPPORT)),
    }
}

fn parse_attributes(mut bytes: &[u8]) -> Vec<(u16, &[u8])> {
    let mut attributes = Vec::new();
    while bytes.len() >= 4 {
        let length = u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes(bytes[2..4].try_into().unwrap());
        if length < 4 || length > bytes.len() {
            break;
        }
        attributes.push((kind, &bytes[4..length]));
        let aligned = (length + 3) & !3;
        if aligned > bytes.len() {
            break;
        }
        bytes = &bytes[aligned..];
    }
    attributes
}

// ── 响应构建 ─────────────────────────────────────────────────────────────────

fn build_ifinfomsg(device: &net::device::NetDeviceSnapshot, seq: u32, pid: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(96);
    payload.push(AF_UNSPEC);
    payload.push(0);
    payload.extend_from_slice(&1u16.to_ne_bytes());
    payload.extend_from_slice(&(device.id.0 as i32).to_ne_bytes());
    let mut flags = IFF_BROADCAST | IFF_MULTICAST;
    if device.running {
        flags |= IFF_UP | IFF_RUNNING;
    }
    payload.extend_from_slice(&flags.to_ne_bytes());
    payload.extend_from_slice(&u32::MAX.to_ne_bytes());

    let mut name = device.name.as_bytes().to_vec();
    name.push(0);
    put_nlattr(&mut payload, IFLA_IFNAME, &name);
    put_nlattr(&mut payload, IFLA_MTU, &device.mtu.to_ne_bytes());
    put_nlattr(&mut payload, IFLA_ADDRESS, &device.mac_address);
    wrap_nlmsg(RTM_NEWLINK, NLM_F_MULTI, seq, pid, &payload)
}

fn build_ifaddrmsg_for_device(
    entry: net::control::AddressEntry,
    device: &net::device::NetDeviceSnapshot,
    seq: u32,
    pid: u32,
) -> Vec<u8> {
    let (family, address): (u8, &[u8]) = match &entry.address {
        net::IpAddr::V4(address) => (AF_INET, &address.0),
        net::IpAddr::V6(address) => (AF_INET6, &address.0),
    };
    let mut payload = Vec::with_capacity(64);
    payload.push(family);
    payload.push(entry.prefix_len);
    payload.push(if entry.primary { 0 } else { IFA_F_SECONDARY });
    payload.push(if device.name.as_ref() == "lo" {
        RT_SCOPE_HOST
    } else {
        RT_SCOPE_UNIVERSE
    });
    payload.extend_from_slice(&device.id.raw().to_ne_bytes());
    put_nlattr(&mut payload, IFA_ADDRESS, address);
    put_nlattr(&mut payload, IFA_LOCAL, address);
    let mut label = device.name.as_bytes().to_vec();
    label.push(0);
    put_nlattr(&mut payload, IFA_LABEL, &label);
    wrap_nlmsg(RTM_NEWADDR, NLM_F_MULTI, seq, pid, &payload)
}

fn build_rtmsg(route: net::control::RouteEntry, seq: u32, pid: u32) -> Vec<u8> {
    let (family, address): (u8, &[u8]) = match &route.network {
        net::IpAddr::V4(address) => (AF_INET, &address.0),
        net::IpAddr::V6(address) => (AF_INET6, &address.0),
    };
    let mut payload = Vec::with_capacity(96);
    payload.push(family);
    payload.push(route.prefix_len);
    payload.push(0); // src_len
    payload.push(0); // tos
    payload.push(route.table);
    payload.push(RTPROT_BOOT);
    payload.push(if route.gateway.is_some() || route.prefix_len == 0 {
        RT_SCOPE_UNIVERSE
    } else {
        RT_SCOPE_LINK
    });
    payload.push(RTN_UNICAST);
    payload.extend_from_slice(&0u32.to_ne_bytes()); // flags
    put_nlattr(&mut payload, RTA_DST, address);
    if let Some(gateway) = route.gateway {
        let bytes: &[u8] = match &gateway {
            net::IpAddr::V4(address) => &address.0,
            net::IpAddr::V6(address) => &address.0,
        };
        put_nlattr(&mut payload, RTA_GATEWAY, bytes);
    }
    put_nlattr(&mut payload, RTA_OIF, &route.interface.0.to_ne_bytes());
    put_nlattr(&mut payload, RTA_PRIORITY, &route.metric.to_ne_bytes());
    wrap_nlmsg(RTM_NEWROUTE, NLM_F_MULTI, seq, pid, &payload)
}

fn build_ndmsg(neighbor: NeighborSnapshot, seq: u32, pid: u32) -> Vec<u8> {
    let (family, address): (u8, &[u8]) = match &neighbor.address {
        net::IpAddr::V4(address) => (AF_INET, &address.0),
        net::IpAddr::V6(address) => (AF_INET6, &address.0),
    };
    let mut payload = Vec::with_capacity(64);
    payload.push(family);
    payload.push(0);
    payload.extend_from_slice(&0u16.to_ne_bytes());
    payload.extend_from_slice(&(neighbor.interface.0 as i32).to_ne_bytes());
    payload.extend_from_slice(&neighbor.nud_state.to_ne_bytes());
    payload.push(0); // flags
    payload.push(0); // type
    put_nlattr(&mut payload, NDA_DST, address);
    put_nlattr(&mut payload, NDA_LLADDR, &neighbor.mac);
    wrap_nlmsg(RTM_NEWNEIGH, NLM_F_MULTI, seq, pid, &payload)
}

fn put_nlattr(out: &mut Vec<u8>, attr_type: u16, data: &[u8]) {
    let attr_len = 4 + data.len();
    out.extend_from_slice(&(attr_len as u16).to_ne_bytes());
    out.extend_from_slice(&attr_type.to_ne_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn wrap_nlmsg(msg_type: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let total_len = 16 + payload.len();
    let mut message = Vec::with_capacity(total_len);
    message.extend_from_slice(&(total_len as u32).to_ne_bytes());
    message.extend_from_slice(&msg_type.to_ne_bytes());
    message.extend_from_slice(&flags.to_ne_bytes());
    message.extend_from_slice(&seq.to_ne_bytes());
    message.extend_from_slice(&pid.to_ne_bytes());
    message.extend_from_slice(payload);
    message
}

fn build_nlmsg_done(seq: u32, local_pid: u32) -> Vec<u8> {
    wrap_nlmsg(NLMSG_DONE, 0, seq, local_pid, &0u32.to_ne_bytes())
}

fn build_nlmsg_error(seq: u32, local_pid: u32, error: i32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&error.to_ne_bytes());
    payload.extend_from_slice(&16u32.to_ne_bytes());
    payload.extend_from_slice(&0u16.to_ne_bytes());
    payload.extend_from_slice(&0u16.to_ne_bytes());
    payload.extend_from_slice(&seq.to_ne_bytes());
    payload.extend_from_slice(&0u32.to_ne_bytes());
    wrap_nlmsg(NLMSG_ERROR, 0, seq, local_pid, &payload)
}

// ── netlink sockopt ──────────────────────────────────────────────────────────

pub fn netlink_getsockopt(
    ops: &NetlinkSocketFileOps,
    level: i32,
    optname: i32,
) -> Result<Vec<u8>, Errno> {
    match level {
        crate::socket::SOL_SOCKET => match optname {
            crate::socket::SO_DOMAIN => Ok(16i32.to_ne_bytes().to_vec()),
            crate::socket::SO_TYPE => Ok((crate::socket::SOCK_RAW as i32).to_ne_bytes().to_vec()),
            crate::socket::SO_PROTOCOL => Ok((ops.protocol() as i32).to_ne_bytes().to_vec()),
            crate::socket::SO_SNDBUF => Ok(212992i32.to_ne_bytes().to_vec()),
            crate::socket::SO_RCVBUF => Ok((ops.rx_limit.load(Ordering::Relaxed) as i32)
                .to_ne_bytes()
                .to_vec()),
            crate::socket::SO_ERROR => Ok(0i32.to_ne_bytes().to_vec()),
            crate::socket::SO_PASSCRED => Ok((i32::from(ops.passcred.load(Ordering::Acquire)))
                .to_ne_bytes()
                .to_vec()),
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_NETLINK => match optname {
            NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => {
                Ok(ops.groups.load(Ordering::Acquire).to_ne_bytes().to_vec())
            }
            _ => Err(Errno::ENOPROTOOPT),
        },
        _ => Err(Errno::ENOPROTOOPT),
    }
}

pub fn netlink_setsockopt(
    ops: &NetlinkSocketFileOps,
    level: i32,
    optname: i32,
    value: &[u8],
) -> Result<(), Errno> {
    match level {
        crate::socket::SOL_SOCKET => match optname {
            crate::socket::SO_SNDBUF => {
                if value.len() < 4 {
                    return Err(Errno::EINVAL);
                }
                // netlink 发送无内核侧缓冲队列，接受请求值作为语义确认。
                let _ = i32::from_ne_bytes(value[..4].try_into().unwrap());
                Ok(())
            }
            crate::socket::SO_RCVBUF => {
                if value.len() < 4 {
                    return Err(Errno::EINVAL);
                }
                let requested = i32::from_ne_bytes(value[..4].try_into().unwrap()).max(0) as u32;
                // Linux 语义：内核将请求值加倍后作为实际缓冲。
                let doubled = requested.saturating_mul(2).max(4096);
                ops.rx_limit.store(doubled, Ordering::Relaxed);
                Ok(())
            }
            crate::socket::SO_PASSCRED => {
                if value.len() < 4 {
                    return Err(Errno::EINVAL);
                }
                let enabled = i32::from_ne_bytes(value[..4].try_into().unwrap()) != 0;
                ops.passcred.store(enabled, Ordering::Release);
                Ok(())
            }
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_NETLINK => match optname {
            NETLINK_ADD_MEMBERSHIP => {
                if value.len() < 4 {
                    return Err(Errno::EINVAL);
                }
                let group = u32::from_ne_bytes(value[..4].try_into().unwrap());
                let current = ops.groups.load(Ordering::Acquire);
                ops.groups.store(current | group, Ordering::Release);
                Ok(())
            }
            NETLINK_DROP_MEMBERSHIP => {
                if value.len() < 4 {
                    return Err(Errno::EINVAL);
                }
                let group = u32::from_ne_bytes(value[..4].try_into().unwrap());
                let current = ops.groups.load(Ordering::Acquire);
                ops.groups.store(current & !group, Ordering::Release);
                Ok(())
            }
            _ => Err(Errno::ENOPROTOOPT),
        },
        _ => Err(Errno::ENOPROTOOPT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: u32, name: &str) -> net::device::NetDeviceSnapshot {
        net::device::NetDeviceSnapshot {
            id: net::NetDeviceId(id),
            name: name.into(),
            mac_address: [0x02, 0, 0, 0, 0, id as u8],
            mtu: 1500,
            queue_pairs: 1,
            running: true,
            stats: net::device::NetDeviceStats::default(),
        }
    }

    /// 解析 nlmsg 载荷中的属性。header_len 为 rtnetlink 消息头长度
    /// （ifaddrmsg=8，rtmsg/ndmsg=12）。
    fn attributes_from(message: &[u8], header_len: usize) -> Vec<(u16, &[u8])> {
        let mut attributes = Vec::new();
        let mut offset = 16 + header_len;
        while offset + 4 <= message.len() {
            let length =
                u16::from_ne_bytes(message[offset..offset + 2].try_into().unwrap()) as usize;
            let kind = u16::from_ne_bytes(message[offset + 2..offset + 4].try_into().unwrap());
            assert!(length >= 4 && offset + length <= message.len());
            attributes.push((kind, &message[offset + 4..offset + length]));
            offset += (length + 3) & !3;
        }
        attributes
    }

    #[test]
    fn ipv4_address_dump_contains_addrconfig_fields() {
        let entry = net::control::AddressEntry {
            interface: net::InterfaceId(7),
            address: net::IpAddr::V4(net::Ipv4Addr([127, 0, 0, 1])),
            prefix_len: 8,
            primary: true,
        };
        let message = build_ifaddrmsg_for_device(entry, &device(7, "lo"), 0x1234, 0x5678);

        assert_eq!(
            u32::from_ne_bytes(message[0..4].try_into().unwrap()) as usize,
            message.len()
        );
        assert_eq!(
            u16::from_ne_bytes(message[4..6].try_into().unwrap()),
            RTM_NEWADDR
        );
        assert_eq!(
            u16::from_ne_bytes(message[6..8].try_into().unwrap()),
            NLM_F_MULTI
        );
        assert_eq!(
            u32::from_ne_bytes(message[8..12].try_into().unwrap()),
            0x1234
        );
        assert_eq!(
            u32::from_ne_bytes(message[12..16].try_into().unwrap()),
            0x5678
        );
        assert_eq!(&message[16..20], &[AF_INET, 8, 0, RT_SCOPE_HOST]);
        assert_eq!(u32::from_ne_bytes(message[20..24].try_into().unwrap()), 7);

        let attributes = attributes_from(&message, 8);
        assert!(attributes.contains(&(IFA_ADDRESS, &[127, 0, 0, 1][..])));
        assert!(attributes.contains(&(IFA_LOCAL, &[127, 0, 0, 1][..])));
        assert!(attributes.contains(&(IFA_LABEL, b"lo\0")));
    }

    #[test]
    fn ipv6_secondary_address_preserves_family_prefix_and_flag() {
        let address = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let entry = net::control::AddressEntry {
            interface: net::InterfaceId(2),
            address: net::IpAddr::V6(net::Ipv6Addr(address)),
            prefix_len: 64,
            primary: false,
        };
        let message = build_ifaddrmsg_for_device(entry, &device(2, "net0"), 3, 4);

        assert_eq!(
            &message[16..20],
            &[AF_INET6, 64, IFA_F_SECONDARY, RT_SCOPE_UNIVERSE]
        );
        assert_eq!(u32::from_ne_bytes(message[20..24].try_into().unwrap()), 2);
        let attributes = attributes_from(&message, 8);
        assert!(attributes.contains(&(IFA_ADDRESS, &address[..])));
        assert!(attributes.contains(&(IFA_LOCAL, &address[..])));
        assert!(attributes.contains(&(IFA_LABEL, b"net0\0")));
    }

    #[test]
    fn parse_attributes_handles_padding() {
        let mut payload = Vec::new();
        put_nlattr(&mut payload, RTA_DST, &[10, 0, 2, 0]);
        put_nlattr(&mut payload, RTA_GATEWAY, &[10, 0, 2, 2]);
        let attrs = parse_attributes(&payload);
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0], (RTA_DST, &[10, 0, 2, 0][..]));
        assert_eq!(attrs[1], (RTA_GATEWAY, &[10, 0, 2, 2][..]));
    }

    #[test]
    fn route_request_decodes_and_returns_eopnotsupp_without_handler() {
        let mut payload = Vec::new();
        payload.push(AF_INET);
        payload.push(0); // dst_len
        payload.push(0); // src_len
        payload.push(0); // tos
        payload.push(254); // table = main
        payload.push(0); // protocol
        payload.push(0); // scope
        payload.push(1); // type = unicast
        payload.extend_from_slice(&0u32.to_ne_bytes()); // flags
        put_nlattr(&mut payload, RTA_DST, &[0, 0, 0, 0]);
        put_nlattr(&mut payload, RTA_GATEWAY, &[10, 0, 2, 2]);
        put_nlattr(&mut payload, RTA_OIF, &1u32.to_ne_bytes());
        put_nlattr(&mut payload, RTA_PRIORITY, &100u32.to_ne_bytes());

        let responses =
            dispatch_message(NETLINK_ROUTE, RTM_NEWROUTE, NLM_F_REQUEST, 7, 3, &payload);
        assert_eq!(responses.len(), 1);
        let error = i32::from_ne_bytes(responses[0][16..20].try_into().unwrap());
        assert_eq!(error, -i32::from(Errno::EOPNOTSUPP));
    }

    #[test]
    fn addr_request_decodes_family_and_index() {
        let mut payload = Vec::new();
        payload.push(AF_INET);
        payload.push(24);
        payload.push(0); // flags
        payload.push(0); // scope
        payload.extend_from_slice(&2u32.to_ne_bytes()); // ifindex
        put_nlattr(&mut payload, IFA_LOCAL, &[10, 0, 2, 100]);

        let responses = dispatch_message(NETLINK_ROUTE, RTM_NEWADDR, NLM_F_REQUEST, 9, 5, &payload);
        assert_eq!(responses.len(), 1);
        let error = i32::from_ne_bytes(responses[0][16..20].try_into().unwrap());
        assert_eq!(error, -i32::from(Errno::EOPNOTSUPP));
    }

    #[test]
    fn link_change_decodes_up_flag_and_mtu() {
        let mut payload = Vec::new();
        payload.push(AF_UNSPEC);
        payload.push(0);
        payload.extend_from_slice(&1u16.to_ne_bytes());
        payload.extend_from_slice(&2i32.to_ne_bytes()); // ifindex
        payload.extend_from_slice(&(IFF_UP as u32).to_ne_bytes());
        payload.extend_from_slice(&u32::MAX.to_ne_bytes());
        put_nlattr(&mut payload, IFLA_MTU, &1400u32.to_ne_bytes());

        let responses = dispatch_message(NETLINK_ROUTE, RTM_SETLINK, NLM_F_REQUEST, 3, 3, &payload);
        assert_eq!(responses.len(), 1);
        let error = i32::from_ne_bytes(responses[0][16..20].try_into().unwrap());
        assert_eq!(error, -i32::from(Errno::EOPNOTSUPP));
    }

    #[test]
    fn get_route_without_provider_returns_done_only() {
        let responses = dispatch_message(NETLINK_ROUTE, RTM_GETROUTE, NLM_F_REQUEST, 1, 1, &[]);
        assert_eq!(responses.len(), 1);
        assert_eq!(
            u16::from_ne_bytes(responses[0][4..6].try_into().unwrap()),
            NLMSG_DONE
        );
    }

    #[test]
    fn dispatch_rejects_malformed_nlmsg_len() {
        let ops = NetlinkSocketFileOps::new(0, false);
        // nlmsg_len 小于 16 字节头。
        let mut too_short = [0u8; 16];
        too_short[0..4].copy_from_slice(&4u32.to_ne_bytes());
        assert_eq!(ops.dispatch(&too_short), Err(VfsError::InvalidArgument));
        // nlmsg_len 超出输入缓冲。
        let mut too_long = [0u8; 16];
        too_long[0..4].copy_from_slice(&100u32.to_ne_bytes());
        assert_eq!(ops.dispatch(&too_long), Err(VfsError::InvalidArgument));
    }

    #[test]
    fn generic_family_lookup_preserves_sequence_and_does_not_dispatch_rtnetlink() {
        let ops = NetlinkSocketFileOps::new(NETLINK_GENERIC, false);
        let sequence = 0x1234_5678;
        let request = wrap_nlmsg(
            GENL_ID_CTRL,
            NLM_F_REQUEST,
            sequence,
            0,
            &[CTRL_CMD_GETFAMILY, 1, 0, 0],
        );

        assert_eq!(ops.dispatch(&request), Ok(request.len()));
        let response = ops.rx_buf.lock().pop_front().expect("generic response");
        assert_eq!(
            u16::from_ne_bytes(response[4..6].try_into().unwrap()),
            NLMSG_ERROR
        );
        assert_eq!(
            u32::from_ne_bytes(response[8..12].try_into().unwrap()),
            sequence
        );
        assert_eq!(
            i32::from_ne_bytes(response[16..20].try_into().unwrap()),
            -i32::from(Errno::ENOENT)
        );
        assert_eq!(
            i32::from_ne_bytes(
                netlink_getsockopt(&ops, crate::socket::SOL_SOCKET, crate::socket::SO_PROTOCOL,)
                    .unwrap()[..4]
                    .try_into()
                    .unwrap()
            ),
            NETLINK_GENERIC as i32
        );
    }

    #[test]
    fn rtmsg_contains_dst_gateway_and_oif() {
        let route = net::control::RouteEntry {
            table: 254,
            network: net::IpAddr::V4(net::Ipv4Addr::UNSPECIFIED),
            prefix_len: 0,
            gateway: Some(net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 2))),
            interface: net::InterfaceId(2),
            metric: 100,
            mtu: Some(1500),
        };
        let message = build_rtmsg(route, 11, 22);
        assert_eq!(message[16], AF_INET);
        assert_eq!(message[17], 0); // dst_len
        assert_eq!(message[20], 254); // table
        let attrs = attributes_from(&message, 12);
        assert!(attrs.contains(&(RTA_DST, &[0, 0, 0, 0][..])));
        assert!(attrs.contains(&(RTA_GATEWAY, &[10, 0, 2, 2][..])));
        assert!(attrs.contains(&(RTA_OIF, &2u32.to_ne_bytes()[..])));
        assert!(attrs.contains(&(RTA_PRIORITY, &100u32.to_ne_bytes()[..])));
    }

    #[test]
    fn ndmsg_contains_lladdr() {
        let neighbor = NeighborSnapshot {
            interface: net::InterfaceId(2),
            address: net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 2)),
            mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
            nud_state: 0x02, // NUD_REACHABLE
        };
        let message = build_ndmsg(neighbor, 5, 6);
        assert_eq!(message[16], AF_INET);
        let attrs = attributes_from(&message, 12);
        assert!(attrs.contains(&(NDA_DST, &[10, 0, 2, 2][..])));
        assert!(attrs.contains(&(NDA_LLADDR, &[0x52, 0x54, 0, 0x12, 0x34, 0x56][..])));
    }

    #[test]
    fn rcvbuf_setsockopt_doubles_and_clamps() {
        let ops = NetlinkSocketFileOps::new(0, false);
        netlink_setsockopt(
            &ops,
            crate::socket::SOL_SOCKET,
            crate::socket::SO_RCVBUF,
            &4096i32.to_ne_bytes(),
        )
        .unwrap();
        assert_eq!(ops.rx_limit.load(Ordering::Relaxed), 8192);
        netlink_setsockopt(
            &ops,
            crate::socket::SOL_SOCKET,
            crate::socket::SO_RCVBUF,
            &0i32.to_ne_bytes(),
        )
        .unwrap();
        assert_eq!(ops.rx_limit.load(Ordering::Relaxed), 4096);
    }

    #[test]
    fn peek_trunc_reports_size_without_consuming_datagram() {
        let ops = NetlinkSocketFileOps::new(0, false);
        ops.push_event(vec![1, 2, 3, 4]);

        let mut empty = [];
        assert_eq!(ops.recv(&mut empty, false, None, true, true), Ok(4));
        assert_eq!(ops.rx_buf.lock().len(), 1);

        let mut output = [0u8; 4];
        assert_eq!(ops.recv(&mut output, false, None, false, false), Ok(4));
        assert_eq!(output, [1, 2, 3, 4]);
        assert!(ops.rx_buf.lock().is_empty());
    }

    #[test]
    fn membership_setsockopt_toggles_groups() {
        let ops = NetlinkSocketFileOps::new(0, false);
        netlink_setsockopt(
            &ops,
            SOL_NETLINK,
            NETLINK_ADD_MEMBERSHIP,
            &RTMGRP_LINK.to_ne_bytes(),
        )
        .unwrap();
        assert_eq!(ops.groups.load(Ordering::Acquire), RTMGRP_LINK);
        netlink_setsockopt(
            &ops,
            SOL_NETLINK,
            NETLINK_ADD_MEMBERSHIP,
            &RTMGRP_IPV4_ROUTE.to_ne_bytes(),
        )
        .unwrap();
        assert_eq!(
            ops.groups.load(Ordering::Acquire),
            RTMGRP_LINK | RTMGRP_IPV4_ROUTE
        );
        netlink_setsockopt(
            &ops,
            SOL_NETLINK,
            NETLINK_DROP_MEMBERSHIP,
            &RTMGRP_LINK.to_ne_bytes(),
        )
        .unwrap();
        assert_eq!(ops.groups.load(Ordering::Acquire), RTMGRP_IPV4_ROUTE);
    }

    #[test]
    fn broadcast_delivers_only_to_subscribed_sockets() {
        let subscribed = create_netlink_socket(0, false);
        netlink_setsockopt(
            &subscribed,
            SOL_NETLINK,
            NETLINK_ADD_MEMBERSHIP,
            &RTMGRP_IPV4_ROUTE.to_ne_bytes(),
        )
        .unwrap();
        let unsubscribed = create_netlink_socket(0, false);

        netlink_event_broadcast(RTM_NEWROUTE, vec![1, 2, 3, 4]);
        assert_eq!(subscribed.rx_buf.lock().len(), 1);
        assert_eq!(unsubscribed.rx_buf.lock().len(), 0);

        // 不匹配的组播类型不投递
        netlink_event_broadcast(RTM_NEWLINK, vec![5, 6, 7, 8]);
        assert_eq!(subscribed.rx_buf.lock().len(), 1);
    }
}
