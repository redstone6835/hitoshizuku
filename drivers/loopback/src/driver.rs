//! 本地回环网络设备 ELM 驱动。
//!
//! 回环设备不依赖任何物理总线：发送帧进入有界队列后重新作为接收帧提供给
//! 同一个网络接口。协议栈仍然负责 IP/TCP/UDP 处理，驱动只实现网络设备契约。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;

use spin::Mutex;

use net::config::{CidrAddress, IfConfig, IfMode, Ipv4Addr, Ipv6Addr};
use net::device::NetDevice;
use net::driver::{Duplex, LinkMedium, LinkState, NetDriver, NetStats, RxBuf, TxBuf};

const LOOPBACK_NAME: &str = "lo";
const LOOPBACK_IPV4_PREFIX: u8 = 8;
const LOOPBACK_IPV6_PREFIX: u8 = 128;
const LOOPBACK_MTU: usize = 65_536;
const MAX_LOOPBACK_QUEUE_FRAMES: usize = 1024;
const MAX_LOOPBACK_FREE_FRAMES: usize = 1024;

struct LoopbackDriver {
    queue: Mutex<VecDeque<(Box<[u8]>, usize)>>,
    free: Mutex<VecDeque<Box<[u8]>>>,
    stats: Mutex<NetStats>,
}

impl LoopbackDriver {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            free: Mutex::new(VecDeque::new()),
            stats: Mutex::new(NetStats::default()),
        }
    }

    fn recycle(&self, storage: Box<[u8]>) {
        let mut free = self.free.lock();
        if free.len() < MAX_LOOPBACK_FREE_FRAMES {
            free.push_back(storage);
        }
    }
}

impl NetDriver for LoopbackDriver {
    fn medium(&self) -> LinkMedium {
        // 回环接口直接传递 IP 包，不需要 Ethernet 头或邻居解析。
        LinkMedium::Ip
    }

    fn poll_rx(&self) -> Option<RxBuf> {
        let frame = self.queue.lock().pop_front()?;
        let (_, len) = &frame;
        {
            let mut stats = self.stats.lock();
            stats.rx_packets += 1;
            stats.rx_bytes += *len as u64;
        }
        let (storage, len) = frame;
        Some(RxBuf::new(storage, len))
    }

    fn alloc_tx(&self, len: usize) -> Option<TxBuf> {
        let storage = {
            let mut free = self.free.lock();
            free.iter()
                .position(|storage| storage.len() >= len)
                .and_then(|index| free.remove(index))
        }
        .unwrap_or_else(|| alloc::vec![0u8; len].into_boxed_slice());
        Some(TxBuf::new_heap(storage))
    }

    fn commit_tx(&self, buf: TxBuf) {
        let len = buf.len();
        let storage = buf.into_heap();
        if len == 0 {
            self.recycle(storage);
            return;
        }

        {
            let mut stats = self.stats.lock();
            stats.tx_packets += 1;
            stats.tx_bytes += len as u64;
        }

        let mut queue = self.queue.lock();
        if queue.len() >= MAX_LOOPBACK_QUEUE_FRAMES {
            drop(queue);
            self.stats.lock().tx_dropped += 1;
            self.recycle(storage);
            return;
        }
        queue.push_back((storage, len));
    }

    fn recycle_rx(&self, buf: RxBuf) {
        self.recycle(buf.into_storage());
    }

    fn link_state(&self) -> LinkState {
        LinkState::Up {
            speed_mbps: None,
            duplex: Duplex::Full,
        }
    }

    fn mac_address(&self) -> [u8; 6] {
        [0; 6]
    }

    fn mtu(&self) -> usize {
        LOOPBACK_MTU
    }

    fn stats(&self) -> NetStats {
        *self.stats.lock()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) struct LoopbackHandle {
    device: Arc<NetDevice>,
    id: net::InterfaceId,
}

pub(crate) fn register() -> Result<LoopbackHandle, net::NetError> {
    let stack = net::stack();
    if stack.find_interface_by_name(LOOPBACK_NAME).is_some() {
        return Err(net::NetError::InterfaceExists);
    }

    let driver: Arc<dyn NetDriver> = Arc::new(LoopbackDriver::new());
    let device = Arc::new(NetDevice::new(LOOPBACK_NAME, driver));
    let id = device.id();
    let config = IfConfig {
        addresses: alloc::vec![
            CidrAddress::new_v4(Ipv4Addr::LOCALHOST, LOOPBACK_IPV4_PREFIX),
            CidrAddress::new_v6(Ipv6Addr::LOCALHOST, LOOPBACK_IPV6_PREFIX),
        ],
        gateway: None,
        mode: IfMode::Static,
    };
    stack.attach(Arc::clone(&device), config)?;
    log::printk!("[loopback] attached lo 127.0.0.1/8 ::1/128");
    Ok(LoopbackHandle { device, id })
}

impl LoopbackHandle {
    pub(crate) fn unregister(&self) -> Result<(), net::NetError> {
        // 先让正在进行的设备访问看到失效，再从协议栈注册表摘除接口。
        self.device.mark_gone();
        match net::stack().detach(self.id) {
            Ok(()) | Err(net::NetError::InterfaceNotFound) => {
                log::printk!("[loopback] detached lo");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}
