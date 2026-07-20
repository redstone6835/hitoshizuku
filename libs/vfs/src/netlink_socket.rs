//! AF_NETLINK/NETLINK_ROUTE 的链路信息实现。
//!
//! 设备信息来自网络设备快照；尚未实现的地址、路由和邻居查询返回空 dump，
//! 修改请求明确返回 `EAFNOSUPPORT`。

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
use crate::poll_source::PollSource;

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
const NLM_F_MULTI: u16 = 2;
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const AF_UNSPEC: u8 = 0;
const IFF_UP: u32 = 1;
const IFF_BROADCAST: u32 = 2;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;

pub struct NetlinkSocketFileOps {
    #[allow(dead_code)]
    protocol: u32,
    rx_buf: Mutex<VecDeque<Vec<u8>>>,
    wait_queue: WaitQueue,
    nonblock: AtomicBool,
    bound: AtomicBool,
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
            poll_source: PollSource::new(PollEvents::POLLOUT),
        }
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
            let message = { self.rx_buf.lock().pop_front() };
            if let Some(msg) = message {
                let len = msg.len().min(buf.len());
                buf[..len].copy_from_slice(&msg[..len]);
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
        let msg_type = u16::from_ne_bytes([buf[4], buf[5]]);
        let seq = u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let pid = u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let responses = dispatch_message(msg_type, seq, pid);
        let mut combined = Vec::new();
        for response in responses {
            combined.extend_from_slice(&response);
        }
        self.rx_buf.lock().push_back(combined);
        self.refresh_readiness();
        self.wait_queue.wake_all();
        Ok(buf.len())
    }
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

    fn release(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn create_netlink_socket(protocol: u32, nonblock: bool) -> NetlinkSocketFileOps {
    NetlinkSocketFileOps::new(protocol, nonblock)
}

fn dispatch_message(msg_type: u16, seq: u32, pid: u32) -> Vec<Vec<u8>> {
    match msg_type {
        RTM_GETLINK => {
            let mut messages = net::device::snapshot_devices()
                .into_iter()
                .map(|device| build_ifinfomsg(&device, seq, pid))
                .collect::<Vec<_>>();
            messages.push(build_nlmsg_done(seq));
            messages
        }
        RTM_GETADDR | RTM_GETROUTE | RTM_GETNEIGH => vec![build_nlmsg_done(seq)],
        RTM_NEWLINK | RTM_DELLINK | RTM_NEWADDR | RTM_DELADDR | RTM_NEWROUTE | RTM_DELROUTE => {
            vec![build_nlmsg_error(seq, -i32::from(Errno::EAFNOSUPPORT))]
        }
        _ => vec![build_nlmsg_error(seq, -i32::from(Errno::EOPNOTSUPP))],
    }
}

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

fn build_nlmsg_done(seq: u32) -> Vec<u8> {
    wrap_nlmsg(NLMSG_DONE, 0, seq, 0, &0u32.to_ne_bytes())
}

fn build_nlmsg_error(seq: u32, error: i32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&error.to_ne_bytes());
    payload.extend_from_slice(&16u32.to_ne_bytes());
    payload.extend_from_slice(&0u16.to_ne_bytes());
    payload.extend_from_slice(&0u16.to_ne_bytes());
    payload.extend_from_slice(&seq.to_ne_bytes());
    payload.extend_from_slice(&0u32.to_ne_bytes());
    wrap_nlmsg(NLMSG_ERROR, 0, seq, 0, &payload)
}
