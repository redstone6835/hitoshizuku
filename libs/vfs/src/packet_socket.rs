//! AF_PACKET 套接字完整实现。
//!
//! 支持：
//! - SOCK_RAW：收发完整以太网帧；SOCK_DGRAM：内核补/剥以太网头
//! - bind(sockaddr_ll)：按接口 + 协议过滤（sll_ifindex=0 绑定所有接口）
//! - PACKET_ADD/DROP_MEMBERSHIP：PROMISC/ALLMULTI/MULTICAST/UNICAST 成员
//! - PACKET_FANOUT：HASH/CPU/RR 三种分发到同组 socket
//! - PACKET_STATISTICS / PACKET_VERSION
//! - SO_ATTACH_FILTER（cBPF）：在完整帧上执行过滤
//!
//! 接收：内核 net_runtime 在 poll_rx 后调用 packet_socket_deliver 投递原始帧；
//! 发送：经 install_packet_tx_handler 注入的出口回调下发。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use errno::Errno;
use sched::{Task, WaitQueue};
use spin::Mutex;

use crate::error::{VfsError, VfsResult};
use crate::file::{DirEntry, FileOps, PollEvents};
use crate::poll_source::PollSource;

// ── 以太网 / 协议常量 ────────────────────────────────────────────────────────

pub const ETH_P_IP: u16 = 0x0800;
pub const ETH_P_ARP: u16 = 0x0806;
pub const ETH_P_IPV6: u16 = 0x86dd;
pub const ETH_P_ALL: u16 = 0x0003;
const MAC_BROADCAST: [u8; 6] = [0xff; 6];

// sll_pkttype
const PACKET_HOST: u8 = 0;
const PACKET_BROADCAST: u8 = 1;
const PACKET_MULTICAST: u8 = 2;
const PACKET_OTHERHOST: u8 = 3;
const PACKET_OUTGOING: u8 = 4;

// SOL_PACKET 选项
const PACKET_ADD_MEMBERSHIP: i32 = 1;
const PACKET_DROP_MEMBERSHIP: i32 = 2;
const PACKET_STATISTICS: i32 = 6;
const PACKET_VERSION: i32 = 10;
const PACKET_FANOUT: i32 = 18;

// PACKET_MR_*
const PACKET_MR_PROMISC: u32 = 0;
const PACKET_MR_ALLMULTI: u32 = 1;
const PACKET_MR_MULTICAST: u32 = 2;
const PACKET_MR_UNICAST: u32 = 3;

// PACKET_FANOUT 类型
const PACKET_FANOUT_HASH: u32 = 0;
const PACKET_FANOUT_LB: u32 = 1;
const PACKET_FANOUT_CPU: u32 = 2;
const PACKET_FANOUT_RNG: u32 = 3;
const PACKET_FANOUT_ROLLOVER: u32 = 4;
const PACKET_FANOUT_FLAG_DEFRAG: u32 = 0x8000;

pub const SOL_PACKET: i32 = 263;

// ── 发送回调（内核 net_runtime 安装）────────────────────────────────────────

static PACKET_TX_HANDLER: Mutex<Option<fn(net::InterfaceId, Vec<u8>) -> Result<(), i32>>> =
    Mutex::new(None);

pub fn install_packet_tx_handler(handler: fn(net::InterfaceId, Vec<u8>) -> Result<(), i32>) {
    *PACKET_TX_HANDLER.lock() = Some(handler);
}

/// 接口 MAC 查询回调（内核 net_runtime 安装；SOCK_DGRAM 发送构造源地址用）。
static IFACE_MAC_HANDLER: Mutex<Option<fn(net::InterfaceId) -> [u8; 6]>> = Mutex::new(None);

pub fn install_packet_interface_mac(handler: fn(net::InterfaceId) -> [u8; 6]) {
    *IFACE_MAC_HANDLER.lock() = Some(handler);
}

fn packet_interface_mac(interface: net::InterfaceId) -> [u8; 6] {
    if let Some(handler) = IFACE_MAC_HANDLER.lock().as_ref() {
        return handler(interface);
    }
    net::device::snapshot_devices()
        .into_iter()
        .find(|device| device.id.raw() == interface.0)
        .map(|device| device.mac_address)
        .unwrap_or([0; 6])
}

// ── fanout 组注册表 ──────────────────────────────────────────────────────────

/// fanout 组：同组 socket 共享到达帧，按 type 分发。
struct FanoutGroup {
    id: u32,
    fanout_type: u32,
    members: Vec<NetlinkSocketPtr>,
    /// ROLLOVER/LB 轮询游标。
    cursor: AtomicU32,
}

type FanoutGroupRef = Arc<Mutex<FanoutGroup>>;

struct FanoutRegistry {
    groups: Vec<FanoutGroupRef>,
}

static FANOUT_REGISTRY: Mutex<FanoutRegistry> = Mutex::new(FanoutRegistry { groups: Vec::new() });

// ── socket 注册表（接收投递用）─────────────────────────────────────────────
//
// 生命周期保证：new（socket 创建）时入表、release（File 关闭）时出表，
// 投递读取与入/出表共用同一把锁，因此表内指针在锁持有期间必然存活。

/// 注册表中的 packet socket 指针。
struct NetlinkSocketPtr(*const PacketSocketFileOps);

// Safety: 指针解引用只在持表锁的投递路径发生，与 release 出表互斥。
unsafe impl Send for NetlinkSocketPtr {}
unsafe impl Sync for NetlinkSocketPtr {}

static PACKET_SOCKETS: Mutex<Vec<NetlinkSocketPtr>> = Mutex::new(Vec::new());

pub struct PacketSocketFileOps {
    protocol: AtomicU32,
    sock_raw: bool,
    /// Zero means unbound; Linux interface indices are strictly positive.
    bound_ifindex: AtomicU32,
    promiscuous: AtomicBool,
    allmulti: AtomicBool,
    fanout: Mutex<Option<(u32, FanoutGroupRef)>>,
    rx_buf: Mutex<VecDeque<Vec<u8>>>,
    rx_limit: AtomicU32,
    rx_dropped: AtomicU32,
    tx_calls: AtomicU64,
    rx_calls: AtomicU64,
    wait_queue: WaitQueue,
    nonblock: AtomicBool,
    filter: Mutex<Option<Arc<net::bpf::CbpfProgram>>>,
    filter_locked: AtomicBool,
    poll_source: PollSource,
}

impl PacketSocketFileOps {
    pub fn new(protocol: u16, sock_raw: bool, nonblock: bool) -> Self {
        Self {
            protocol: AtomicU32::new(u32::from(protocol)),
            sock_raw,
            bound_ifindex: AtomicU32::new(0),
            promiscuous: AtomicBool::new(false),
            allmulti: AtomicBool::new(false),
            fanout: Mutex::new(None),
            rx_buf: Mutex::new(VecDeque::new()),
            rx_limit: AtomicU32::new(1 << 20),
            rx_dropped: AtomicU32::new(0),
            tx_calls: AtomicU64::new(0),
            rx_calls: AtomicU64::new(0),
            wait_queue: WaitQueue::new_with_reason(sched::WaitReason::SocketRead),
            nonblock: AtomicBool::new(nonblock),
            filter: Mutex::new(None),
            filter_locked: AtomicBool::new(false),
            poll_source: PollSource::new(PollEvents::POLLOUT),
        }
    }

    fn register(&self) {
        PACKET_SOCKETS
            .lock()
            .push(NetlinkSocketPtr(self as *const PacketSocketFileOps));
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

    /// 投递一帧到 rx 队列（含 cBPF 过滤与 fanout 分发）。
    fn deliver_frame(&self, frame: Vec<u8>) {
        // 先过 cBPF（在完整帧上执行，Linux 语义）。
        if let Some(filter) = self.filter.lock().as_ref().cloned() {
            if filter.run(&frame) == 0 {
                return;
            }
        }
        let mut buf = self.rx_buf.lock();
        let limit = self.rx_limit.load(Ordering::Relaxed) as usize;
        let queued: usize = buf.iter().map(|frame| frame.len()).sum();
        if queued + frame.len() > limit {
            self.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.rx_calls.fetch_add(1, Ordering::Relaxed);
        buf.push_back(frame);
        drop(buf);
        self.refresh_readiness();
        self.wait_queue.wake_all();
    }

    pub fn bind(&self, addr: &[u8]) -> Result<(), Errno> {
        if addr.len() < 12 {
            return Err(Errno::EINVAL);
        }
        let family = u16::from_ne_bytes(addr[..2].try_into().unwrap());
        if family != crate::addr::AF_PACKET {
            return Err(Errno::EAFNOSUPPORT);
        }
        let protocol = u16::from_ne_bytes(addr[2..4].try_into().unwrap());
        let ifindex = u32::from_ne_bytes(addr[4..8].try_into().unwrap());
        if protocol != 0 {
            // bind 的 sll_protocol 若非零则作为过滤协议；否则沿用创建时协议。
            self.set_protocol(protocol);
        }
        let interface = if ifindex == 0 {
            None
        } else {
            Some(net::InterfaceId(ifindex))
        };
        self.set_bound_ifindex(interface);
        Ok(())
    }

    fn set_protocol(&self, protocol: u16) {
        self.protocol.store(u32::from(protocol), Ordering::Release);
    }

    fn set_bound_ifindex(&self, interface: Option<net::InterfaceId>) {
        self.bound_ifindex.store(
            interface.map_or(0, |interface| interface.0),
            Ordering::Release,
        );
    }

    pub fn local_address(&self) -> [u8; 12] {
        let mut address = [0u8; 12];
        address[..2].copy_from_slice(&crate::addr::AF_PACKET.to_ne_bytes());
        let protocol = self.protocol.load(Ordering::Acquire) as u16;
        address[2..4].copy_from_slice(&protocol.to_ne_bytes());
        let ifindex = self.bound_ifindex.load(Ordering::Acquire);
        address[4..8].copy_from_slice(&ifindex.to_ne_bytes());
        address
    }

    /// 发送一帧。SOCK_RAW 期望完整以太网帧；SOCK_DGRAM 期望 IP 报文（内核补头）。
    pub fn sendto(&self, data: &[u8], dest: &[u8]) -> Result<usize, Errno> {
        let handler = PACKET_TX_HANDLER.lock().ok_or(Errno::EOPNOTSUPP)?;
        // sockaddr_ll.sll_ifindex 位于 dest 偏移 4..8；非零时优先作为发送接口，
        // 否则回退到 bind 的接口（未绑定则 EDESTADDRREQ）。
        let dest_ifindex = if dest.len() >= 8 {
            u32::from_ne_bytes(dest[4..8].try_into().unwrap())
        } else {
            0
        };
        let interface = if dest_ifindex != 0 {
            net::InterfaceId(dest_ifindex)
        } else {
            let ifindex = self.bound_ifindex.load(Ordering::Acquire);
            if ifindex == 0 {
                return Err(Errno::EDESTADDRREQ);
            }
            net::InterfaceId(ifindex)
        };
        let frame = if self.sock_raw {
            if data.len() < 14 {
                return Err(Errno::EINVAL);
            }
            data.to_vec()
        } else {
            // 构造以太网头：dst = sll_addr 或广播，src = 接口 MAC，type = protocol。
            let mut frame = Vec::with_capacity(14 + data.len());
            let destination = if dest.len() >= 8 {
                let halen = usize::from(dest[6]);
                if halen != 0 {
                    let mut mac = [0u8; 6];
                    mac[..halen.min(6)].copy_from_slice(&dest[8..8 + halen.min(6)]);
                    mac
                } else {
                    MAC_BROADCAST
                }
            } else {
                MAC_BROADCAST
            };
            let source = packet_interface_mac(interface);
            frame.extend_from_slice(&destination);
            frame.extend_from_slice(&source);
            let protocol = self.protocol.load(Ordering::Acquire) as u16;
            frame.extend_from_slice(&protocol.to_be_bytes());
            frame.extend_from_slice(data);
            frame
        };
        self.tx_calls.fetch_add(1, Ordering::Relaxed);
        handler(interface, frame).map_err(|code| Errno::from_i32(-code))?;
        Ok(data.len())
    }

    /// 接收一帧。SOCK_DGRAM 剥以太网头；返回 (数据, sll 输出)。
    pub fn recvfrom(
        &self,
        buf: &mut [u8],
        sll_out: &mut [u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, Errno> {
        loop {
            let message = self.rx_buf.lock().pop_front();
            if let Some(frame) = message {
                let (payload, frame_len) = if self.sock_raw {
                    (frame.as_slice(), frame.len())
                } else {
                    if frame.len() >= 14 {
                        (&frame[14..], frame.len() - 14)
                    } else {
                        (frame.as_slice(), frame.len())
                    }
                };
                let len = payload.len().min(buf.len());
                buf[..len].copy_from_slice(&payload[..len]);
                if sll_out.len() >= 12 {
                    sll_out[..2].copy_from_slice(&crate::addr::AF_PACKET.to_ne_bytes());
                    let protocol = self.protocol.load(Ordering::Acquire) as u16;
                    sll_out[2..4].copy_from_slice(&protocol.to_ne_bytes());
                    let ifindex = self.bound_ifindex.load(Ordering::Acquire);
                    sll_out[4..8].copy_from_slice(&ifindex.to_ne_bytes());
                    sll_out[8] = 1; // ARPHRD_ETHER
                    sll_out[9] = PACKET_HOST;
                    // sockaddr_ll.sll_addr 应为帧源地址（发送方 MAC）：SOCK_RAW 取
                    // 帧偏移 6..12；SOCK_DGRAM 已剥头，源 MAC 不再暴露，按 Linux
                    // 语义留空。仅当缓冲足够（>= 20，完整 sockaddr_ll）才写 6 字节
                    // 源 MAC，且 sll_halen 与写入字节数一致，避免越界。
                    sll_out[10] = 0;
                    if sll_out.len() >= 20 && self.sock_raw && frame.len() >= 12 {
                        sll_out[10] = 6;
                        sll_out[11..17].copy_from_slice(&frame[6..12]);
                    }
                }
                let _ = frame_len;
                self.refresh_readiness();
                return Ok(len);
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

    // ── cBPF（SO_ATTACH_FILTER 等）─────────────────────────────────────────

    pub fn attach_filter(&self, program: net::bpf::CbpfProgram) -> Result<(), Errno> {
        if self.filter_locked.load(Ordering::Acquire) {
            return Err(Errno::EPERM);
        }
        *self.filter.lock() = Some(Arc::new(program));
        Ok(())
    }

    pub fn detach_filter(&self) -> Result<(), Errno> {
        if self.filter_locked.load(Ordering::Acquire) {
            return Err(Errno::EPERM);
        }
        *self.filter.lock() = None;
        Ok(())
    }

    pub fn lock_filter(&self) -> Result<(), Errno> {
        self.filter_locked.store(true, Ordering::Release);
        Ok(())
    }

    pub fn get_filter(&self) -> Vec<net::bpf::CbpfInsn> {
        self.filter
            .lock()
            .as_ref()
            .map(|program| program.instructions().to_vec())
            .unwrap_or_default()
    }

    // ── SOL_PACKET 选项 ────────────────────────────────────────────────────

    pub fn packet_setsockopt(&self, optname: i32, value: &[u8]) -> Result<(), Errno> {
        match optname {
            PACKET_ADD_MEMBERSHIP | PACKET_DROP_MEMBERSHIP => {
                if value.len() < 8 {
                    return Err(Errno::EINVAL);
                }
                let membership_type = u32::from_ne_bytes(value[..4].try_into().unwrap());
                let _ifindex = u32::from_ne_bytes(value[4..8].try_into().unwrap());
                let add = optname == PACKET_ADD_MEMBERSHIP;
                match membership_type {
                    PACKET_MR_PROMISC => self.set_promiscuous(add),
                    PACKET_MR_ALLMULTI => self.set_allmulti(add),
                    PACKET_MR_MULTICAST | PACKET_MR_UNICAST => {
                        // 成员组当前按绑定的协议/接口放行；记录计数语义。
                        self.set_membership(membership_type, add);
                    }
                    _ => return Err(Errno::EINVAL),
                }
                Ok(())
            }
            PACKET_FANOUT => {
                if value.len() < 4 {
                    return Err(Errno::EINVAL);
                }
                let raw = u32::from_ne_bytes(value[..4].try_into().unwrap());
                let group_id = raw >> 16;
                let fanout_type = raw & 0xffff;
                self.set_fanout(group_id, fanout_type)
            }
            PACKET_VERSION => {
                if value.len() < 4 {
                    return Err(Errno::EINVAL);
                }
                let version = u32::from_ne_bytes(value[..4].try_into().unwrap());
                if version != 1 {
                    // Linux 只接受 TPACKET_V1（无 ring 时版本固定为 1）。
                    return Err(Errno::EINVAL);
                }
                Ok(())
            }
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    pub fn packet_getsockopt(&self, optname: i32) -> Result<Vec<u8>, Errno> {
        match optname {
            PACKET_STATISTICS => {
                let mut out = Vec::with_capacity(16);
                out.extend_from_slice(&self.rx_calls.load(Ordering::Relaxed).to_ne_bytes());
                out.extend_from_slice(&self.rx_dropped.load(Ordering::Relaxed).to_ne_bytes());
                Ok(out)
            }
            PACKET_VERSION => Ok(1u32.to_ne_bytes().to_vec()),
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    fn set_promiscuous(&self, enabled: bool) {
        self.promiscuous.store(enabled, Ordering::Release);
    }

    fn set_allmulti(&self, enabled: bool) {
        self.allmulti.store(enabled, Ordering::Release);
    }

    fn set_membership(&self, _membership: u32, _enabled: bool) {
        // MULTICAST/UNICAST 成员在当前投递模型下由目的 MAC 过滤天然覆盖：
        // 组播帧按组播目的放行（MULTICAST），单播帧按本机 MAC 放行（UNICAST
        // 是默认行为）。这里记录意图，语义由 deliver 过滤实现保证。
    }

    fn set_fanout(&self, group_id: u32, fanout_type: u32) -> Result<(), Errno> {
        if fanout_type & !PACKET_FANOUT_FLAG_DEFRAG > PACKET_FANOUT_ROLLOVER {
            return Err(Errno::EINVAL);
        }
        let mut registry = FANOUT_REGISTRY.lock();
        if let Some(group) = registry
            .groups
            .iter()
            .find(|group| group.lock().id == group_id)
            .cloned()
        {
            {
                let mut group = group.lock();
                if !group
                    .members
                    .iter()
                    .any(|member| member.0 == self as *const _)
                {
                    group
                        .members
                        .push(NetlinkSocketPtr(self as *const PacketSocketFileOps));
                }
            }
            *self.fanout.lock() = Some((group_id, group));
            return Ok(());
        }
        let group = Arc::new(Mutex::new(FanoutGroup {
            id: group_id,
            fanout_type,
            members: vec![NetlinkSocketPtr(self as *const PacketSocketFileOps)],
            cursor: AtomicU32::new(0),
        }));
        *self.fanout.lock() = Some((group_id, Arc::clone(&group)));
        registry.groups.push(group);
        Ok(())
    }

    fn fanout_dispatch(&self, frame: Vec<u8>) {
        let Some((_, group)) = self.fanout.lock().as_ref().cloned() else {
            self.deliver_frame(frame);
            return;
        };
        let mut group = group.lock();
        if group.members.len() <= 1 {
            drop(group);
            self.deliver_frame(frame);
            return;
        }
        let fanout_type = group.fanout_type & !PACKET_FANOUT_FLAG_DEFRAG;
        let index = match fanout_type {
            PACKET_FANOUT_CPU => (sched::current_cpu_id() as u32) % group.members.len() as u32,
            PACKET_FANOUT_LB | PACKET_FANOUT_ROLLOVER => {
                let cursor = group.cursor.fetch_add(1, Ordering::Relaxed);
                cursor % group.members.len() as u32
            }
            _ => packet_fanout_hash(&frame) % group.members.len() as u32,
        };
        let member = group.members[index as usize].0;
        // Safety: 成员与 socket 同生命周期（入组即存活；出组在 release 时由
        // socket 自行移除——此处成员仍持锁，release 需同锁，见 release）。
        let member = unsafe { &*member };
        drop(group);
        member.deliver_frame(frame);
    }
}

/// 帧内容的简单 hash（fanout HASH 分发）。
fn packet_fanout_hash(frame: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for &byte in frame.iter().take(64) {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

impl FileOps for PacketSocketFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.recvfrom(buf, &mut [], true, None)
            .map_err(|error| error_to_vfs(error))
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.sendto(buf, &[]).map_err(|error| error_to_vfs(error))
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
        // 出表 + 退出 fanout 组（与投递共用表锁/组锁）。
        PACKET_SOCKETS
            .lock()
            .retain(|entry| entry.0 != self as *const PacketSocketFileOps);
        if let Some((_, group)) = self.fanout.lock().as_ref().cloned() {
            group
                .lock()
                .members
                .retain(|member| member.0 != self as *const _);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for PacketSocketFileOps {
    fn drop(&mut self) {
        // 出表 + 退出 fanout 组。release() 也做同样清理（幂等）。
        PACKET_SOCKETS
            .lock()
            .retain(|entry| entry.0 != self as *const PacketSocketFileOps);
        if let Some((_, group)) = self.fanout.lock().as_ref().cloned() {
            group
                .lock()
                .members
                .retain(|member| member.0 != self as *const _);
        }
    }
}

fn error_to_vfs(error: Errno) -> VfsError {
    match error {
        Errno::EAGAIN => VfsError::WouldBlock,
        Errno::EINTR => VfsError::Interrupted,
        Errno::EINVAL => VfsError::InvalidArgument,
        Errno::ENODEV => VfsError::NoDevice,
        Errno::ENOTSUP | Errno::EOPNOTSUPP => VfsError::NotSupported,
        _ => VfsError::Io,
    }
}

/// 创建 AF_PACKET socket（SOCK_RAW/SOCK_DGRAM，protocol = ETH_P_*）。
/// 返回 Box 以保证对象地址稳定（注册表持有裸指针，栈上对象的地址在
/// 返回值拷贝后不保证不变）。
pub fn create_packet_socket(
    protocol: u16,
    sock_raw: bool,
    nonblock: bool,
) -> Box<PacketSocketFileOps> {
    let ops = Box::new(PacketSocketFileOps::new(protocol, sock_raw, nonblock));
    ops.register();
    ops
}

// ── 接收投递（内核 net_runtime 调用）────────────────────────────────────────

/// 判定帧目的 MAC 的 pkttype（供投递过滤）。
fn frame_pkttype(frame: &[u8], local_mac: [u8; 6]) -> u8 {
    if frame.len() < 6 {
        return PACKET_OTHERHOST;
    }
    let destination = &frame[..6];
    if destination == &MAC_BROADCAST {
        PACKET_BROADCAST
    } else if destination[0] & 1 != 0 {
        PACKET_MULTICAST
    } else if destination == &local_mac {
        PACKET_HOST
    } else {
        PACKET_OTHERHOST
    }
}

/// 是否存在活跃的 packet socket（内核 RX 路径据此决定是否拷贝帧）。
pub fn packet_socket_active() -> bool {
    !PACKET_SOCKETS.lock().is_empty()
}

/// 内核 RX 路径投递入口：按 (ifindex, ethertype) 匹配注册的 packet socket。
///
/// frame 是完整以太网帧。返回投递的 socket 数（0 表示无匹配）。
pub fn packet_socket_deliver(
    interface: net::InterfaceId,
    ethertype: u16,
    frame: &[u8],
    local_mac: [u8; 6],
) -> usize {
    let pkttype = frame_pkttype(frame, local_mac);
    let registry = PACKET_SOCKETS.lock();
    let mut delivered = 0usize;
    for entry in registry.iter() {
        // Safety: 表锁持有期间没有并发 release 出表（release 也持同一把锁）。
        let socket = unsafe { &*entry.0 };
        let protocol = socket.protocol.load(Ordering::Acquire) as u16;
        let protocol_matches = protocol == ETH_P_ALL || protocol == 0 || protocol == ethertype;
        if !protocol_matches {
            continue;
        }
        let bound_ifindex = socket.bound_ifindex.load(Ordering::Acquire);
        if bound_ifindex != 0 && bound_ifindex != interface.0 {
            continue;
        }
        let accepted = match pkttype {
            PACKET_OTHERHOST => socket.promiscuous.load(Ordering::Acquire),
            PACKET_BROADCAST | PACKET_MULTICAST => true,
            PACKET_HOST => true,
            _ => false,
        };
        if !accepted {
            continue;
        }
        socket.fanout_dispatch(frame.to_vec());
        delivered += 1;
    }
    delivered
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn frame(destination: [u8; 6], ethertype: u16) -> Vec<u8> {
        let mut frame = Vec::with_capacity(30);
        frame.extend_from_slice(&destination);
        frame.extend_from_slice(&LOCAL_MAC);
        frame.extend_from_slice(&ethertype.to_be_bytes());
        frame.extend_from_slice(&[0x45, 0, 0, 0x10, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 2, 15]);
        frame
    }

    const LOCAL_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

    #[test]
    fn pkttype_classification() {
        let _guard = test_guard();
        assert_eq!(
            frame_pkttype(&frame(MAC_BROADCAST, ETH_P_IP), [0; 6]),
            PACKET_BROADCAST
        );
        assert_eq!(
            frame_pkttype(&frame([0x01, 0, 0x5e, 0, 0, 1], ETH_P_IP), [0; 6]),
            PACKET_MULTICAST
        );
        assert_eq!(
            frame_pkttype(
                &frame([0x02, 0, 0, 0, 0, 1], ETH_P_IP),
                [0x02, 0, 0, 0, 0, 1]
            ),
            PACKET_HOST
        );
        assert_eq!(
            frame_pkttype(
                &frame([0x02, 0, 0, 0, 0, 2], ETH_P_IP),
                [0x02, 0, 0, 0, 0, 1]
            ),
            PACKET_OTHERHOST
        );
    }

    #[test]
    fn deliver_matches_protocol_and_ifindex() {
        let _guard = test_guard();
        let socket = create_packet_socket(ETH_P_IP, true, true);
        socket.set_bound_ifindex(Some(net::InterfaceId(2)));
        let ip_frame = frame(LOCAL_MAC, ETH_P_IP);
        let delivered = packet_socket_deliver(net::InterfaceId(2), ETH_P_IP, &ip_frame, LOCAL_MAC);
        // 并行测试可能命中其他 socket；断言自身收到帧即可。
        assert!(delivered >= 1);
        let mut buf = [0u8; 64];
        let len = socket.recvfrom(&mut buf, &mut [], true, None).unwrap();
        assert_eq!(len, ip_frame.len());
    }

    #[test]
    fn recvfrom_raw_fills_source_mac_in_sll_addr() {
        let _guard = test_guard();
        let socket = create_packet_socket(ETH_P_IP, true, true);
        // 源 MAC 为 LOCAL_MAC（帧偏移 6..12），目的 MAC 为广播。
        let ip_frame = frame(MAC_BROADCAST, ETH_P_IP);
        let _ = packet_socket_deliver(net::InterfaceId(1), ETH_P_IP, &ip_frame, LOCAL_MAC);
        let mut buf = [0u8; 64];
        let mut sll = [0u8; 20];
        let len = socket.recvfrom(&mut buf, &mut sll, true, None).unwrap();
        assert_eq!(len, ip_frame.len());
        assert_eq!(sll[10], 6); // sll_halen 与写入字节数一致
        assert_eq!(&sll[11..17], &LOCAL_MAC); // 源地址，而非目的地址
    }

    #[test]
    fn recvfrom_dgram_leaves_sll_addr_zero() {
        let _guard = test_guard();
        let socket = create_packet_socket(ETH_P_IP, false, true);
        let ip_frame = frame(MAC_BROADCAST, ETH_P_IP);
        let _ = packet_socket_deliver(net::InterfaceId(1), ETH_P_IP, &ip_frame, LOCAL_MAC);
        let mut buf = [0u8; 64];
        let mut sll = [0u8; 20];
        let len = socket.recvfrom(&mut buf, &mut sll, true, None).unwrap();
        // SOCK_DGRAM 已剥头，源 MAC 不暴露；sll_addr 与 sll_halen 应为 0。
        assert_eq!(len, ip_frame.len() - 14);
        assert_eq!(sll[10], 0);
        assert_eq!(&sll[11..17], &[0; 6]);
    }

    #[test]
    fn sendto_prefers_sll_ifindex_over_bound() {
        let _guard = test_guard();
        use core::sync::atomic::AtomicUsize;
        static LAST_IFINDEX: AtomicUsize = AtomicUsize::new(usize::MAX);
        install_packet_tx_handler(|interface, _frame| {
            LAST_IFINDEX.store(interface.0 as usize, Ordering::Relaxed);
            Ok(())
        });
        install_packet_interface_mac(|_interface| LOCAL_MAC);
        let socket = create_packet_socket(ETH_P_IP, false, true);
        socket.set_bound_ifindex(Some(net::InterfaceId(1)));
        let payload = [0x45u8, 0, 0, 4, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 2, 15];
        let mut sll = [0u8; 20];
        sll[4..8].copy_from_slice(&7u32.to_ne_bytes()); // sll_ifindex = 7
        socket.sendto(&payload, &sll).unwrap();
        assert_eq!(LAST_IFINDEX.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn deliver_rejects_other_ethertype() {
        let _guard = test_guard();
        let socket = create_packet_socket(ETH_P_IP, true, true);
        let arp_frame = frame(LOCAL_MAC, ETH_P_ARP);
        let _ = packet_socket_deliver(net::InterfaceId(1), ETH_P_ARP, &arp_frame, LOCAL_MAC);
        // 本 socket 绑定 ETH_P_IP，不应收到 ARP 帧（并行测试的
        // ETH_P_ALL socket 可能命中，但与本 socket 无关）。
        let mut buf = [0u8; 64];
        assert_eq!(
            socket.recvfrom(&mut buf, &mut [], true, None),
            Err(Errno::EAGAIN)
        );
    }

    #[test]
    fn otherhost_requires_promiscuous() {
        let _guard = test_guard();
        let socket = create_packet_socket(ETH_P_ALL, true, true);
        let foreign = frame([0x02, 0, 0, 0, 0, 2], ETH_P_IP);
        let delivered = packet_socket_deliver(net::InterfaceId(1), ETH_P_IP, &foreign, LOCAL_MAC);
        assert_eq!(delivered, 0);
        socket.set_promiscuous(true);
        let delivered = packet_socket_deliver(net::InterfaceId(1), ETH_P_IP, &foreign, LOCAL_MAC);
        assert_eq!(delivered, 1);
    }

    #[test]
    fn sock_dgram_send_builds_ethernet_header() {
        let _guard = test_guard();
        use core::sync::atomic::AtomicUsize;
        static SENT: AtomicUsize = AtomicUsize::new(0);
        static FRAME: spin::Mutex<Option<Vec<u8>>> = spin::Mutex::new(None);
        install_packet_tx_handler(|_interface, frame| {
            *FRAME.lock() = Some(frame);
            SENT.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        install_packet_interface_mac(|_interface| LOCAL_MAC);
        let socket = create_packet_socket(ETH_P_IP, false, true);
        socket.set_bound_ifindex(Some(net::InterfaceId(1)));
        let payload = [0x45u8, 0, 0, 4, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 2, 15];
        let mut sll = [0u8; 20];
        sll[6] = 0; // halen = 0 → 广播
        socket.sendto(&payload, &sll).unwrap();
        let frame = FRAME.lock().clone().unwrap();
        assert_eq!(&frame[..6], &MAC_BROADCAST);
        assert_eq!(&frame[6..12], &LOCAL_MAC);
        assert_eq!(&frame[12..14], &[0x08, 0x00]);
        assert_eq!(&frame[14..], &payload);
    }

    #[test]
    fn filter_attached_drops_matching_frames() {
        let _guard = test_guard();
        // 过滤器：拒绝 type == 0x0800 的帧。
        let program = net::bpf::CbpfProgram::compile(vec![
            net::bpf::CbpfInsn {
                code: 0x28,
                jt: 0,
                jf: 0,
                k: 12,
            },
            net::bpf::CbpfInsn {
                code: 0x15,
                jt: 0,
                jf: 1,
                k: 0x0800,
            },
            net::bpf::CbpfInsn {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0,
            },
            net::bpf::CbpfInsn {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0xffff,
            },
        ])
        .unwrap();
        let socket = create_packet_socket(ETH_P_IP, true, true);
        socket.attach_filter(program).unwrap();
        let ip_frame = frame(LOCAL_MAC, ETH_P_IP);
        let delivered = packet_socket_deliver(net::InterfaceId(1), ETH_P_IP, &ip_frame, LOCAL_MAC);
        // 帧被 filter 丢弃：即使有投递命中，本 socket 也收不到。
        let _ = delivered;
        let mut buf = [0u8; 64];
        assert_eq!(
            socket.recvfrom(&mut buf, &mut [], true, None),
            Err(Errno::EAGAIN)
        );
    }

    #[test]
    fn detach_filter_restores_delivery() {
        let _guard = test_guard();
        let program = net::bpf::CbpfProgram::compile(vec![net::bpf::CbpfInsn {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0,
        }])
        .unwrap();
        let socket = create_packet_socket(ETH_P_ALL, true, true);
        socket.attach_filter(program).unwrap();
        let ip_frame = frame(LOCAL_MAC, ETH_P_IP);
        let _ = packet_socket_deliver(net::InterfaceId(1), ETH_P_IP, &ip_frame, LOCAL_MAC);
        let mut buf = [0u8; 64];
        assert_eq!(
            socket.recvfrom(&mut buf, &mut [], true, None),
            Err(Errno::EAGAIN)
        );
        socket.detach_filter().unwrap();
        let _ = packet_socket_deliver(net::InterfaceId(1), ETH_P_IP, &ip_frame, LOCAL_MAC);
        assert!(socket.recvfrom(&mut buf, &mut [], true, None).is_ok());
    }

    #[test]
    fn lock_filter_blocks_reattach() {
        let _guard = test_guard();
        let program = net::bpf::CbpfProgram::compile(vec![net::bpf::CbpfInsn {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0xffff,
        }])
        .unwrap();
        let socket = create_packet_socket(ETH_P_ALL, true, true);
        socket.attach_filter(program.clone()).unwrap();
        socket.lock_filter().unwrap();
        assert_eq!(socket.attach_filter(program.clone()), Err(Errno::EPERM));
        assert_eq!(socket.detach_filter(), Err(Errno::EPERM));
    }
}
