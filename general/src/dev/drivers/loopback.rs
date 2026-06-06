//! Loopback 网络接口驱动。
//!
//! TX 帧直接回环到 RX 队列，用于 127.0.0.1 本地通信。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;

use spin::Mutex;

use net::config::{IfConfig, Ipv4Addr};
use net::device::NetDevice;
use net::driver::{Duplex, LinkState, NetDriver, NetStats, RxBuf, TxBuf};

use crate::dev::pnp::PnpError;

const MAX_LOOPBACK_QUEUE_FRAMES: usize = 1024;
/// Linux 兼容的 loopback MTU。
const LOOPBACK_MTU: usize = 65_536;
/// 保留传统 loopback 网段：lo 固定为 127.0.0.1/8。
const LOOPBACK_IPV4_PREFIX: u8 = 8;

struct LoopbackDriver {
    queue: Mutex<VecDeque<Box<[u8]>>>,
    stats: Mutex<NetStats>,
}

impl LoopbackDriver {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            stats: Mutex::new(NetStats::default()),
        }
    }
}

impl NetDriver for LoopbackDriver {
    fn poll_rx(&self) -> Option<RxBuf> {
        let mut q = self.queue.lock();
        let frame = q.pop_front()?;
        let len = frame.len();
        self.stats.lock().rx_packets += 1;
        self.stats.lock().rx_bytes += len as u64;
        Some(RxBuf::new(frame, len))
    }

    fn alloc_tx(&self, len: usize) -> Option<TxBuf> {
        let buf = alloc::vec![0u8; len].into_boxed_slice();
        Some(TxBuf::new(buf))
    }

    fn commit_tx(&self, buf: TxBuf) {
        let len = buf.len();
        if len == 0 {
            return;
        }
        let data = buf.into_storage();
        {
            let mut stats = self.stats.lock();
            stats.tx_packets += 1;
            stats.tx_bytes += len as u64;
        }
        let mut queue = self.queue.lock();
        if queue.len() >= MAX_LOOPBACK_QUEUE_FRAMES {
            self.stats.lock().tx_dropped += 1;
            return;
        }
        queue.push_back(data);
    }

    fn link_state(&self) -> LinkState {
        LinkState::Up {
            speed_mbps: None,
            duplex: Duplex::Full,
        }
    }

    fn mac_address(&self) -> [u8; 6] {
        [0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
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

pub fn register_builtin_driver() -> Result<(), PnpError> {
    let driver: Arc<dyn NetDriver> = Arc::new(LoopbackDriver::new());
    let dev = Arc::new(NetDevice::new("lo", driver));
    // lo 按 POSIX/Linux 习惯固定使用 127.0.0.1/8；这里不扩展完整路由策略。
    let config = IfConfig::static_v4(Ipv4Addr::LOCALHOST, LOOPBACK_IPV4_PREFIX, None);
    net::stack()
        .attach(dev, config)
        .map_err(|_| PnpError::ProbeFailed)?;
    log::printk!("[loopback] attached lo 127.0.0.1/8");
    Ok(())
}
