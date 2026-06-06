//! Loopback 网络接口驱动。
//!
//! TX 帧直接回环到 RX 队列，用于 127.0.0.1 本地通信。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;

use spin::Mutex;

use net::driver::{Duplex, LinkState, NetDriver, NetStats, RxBuf, TxBuf};
use net::config::{IfConfig, Ipv4Addr};
use net::device::NetDevice;

use crate::dev::pnp::PnpError;

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
        // FIXME: loopback 队列没有容量上限或 backpressure，持续发送会让
        // 内存无界增长。
        self.queue.lock().push_back(data);
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
        65536
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
    let config = IfConfig::static_v4(Ipv4Addr::LOCALHOST, 8, None);
    net::stack()
        .attach(dev, config)
        .map_err(|_| PnpError::ProbeFailed)?;
    log::printk!("[loopback] attached lo 127.0.0.1/8");
    Ok(())
}
