//! 协议引擎 `phy::Device` 适配层。
//!
//! 本模块只负责把内核网络驱动抽象接到当前协议引擎的设备接口。IP/socket
//! 语义和时间转换分别收敛在 `interface.rs`、`stack.rs` 和 `time.rs`，避免
//! 驱动层直接感知具体协议栈实现。
//!
//! # 性能与安全的平衡
//!
//! 安全检查放在**冷路径入口**而非每帧热路径，避免性能税：
//!
//! - `receive()` / `transmit()` 入口各检查一次 `is_active()`（一次原子读）。
//!   hot-unplug 时驱动 `mark_gone()` 立即让后续 RX/TX 失败。
//! - `TxToken` 改用 `&'a Arc<dyn NetDriver>` 引用而非 `Arc::clone`——
//!   消除每帧一次的原子计数增减，万兆网卡场景下显著降低缓存争用。
//! - `TxToken::consume` 在 alloc 失败时递增 `tx_dropped` 统计计数器，
//!   不静默丢失诊断信息。
//! - `TxToken::consume` 用 `assert!` 防止驱动返回小于 `len` 的 buffer
//!   导致越界（驱动 bug 变成确定性 panic 而非 UB）。

use alloc::sync::Arc;

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use crate::device::NetDevice;
use crate::driver::{LinkMedium, NetDriver, RxBuf};

// ── 适配器 ───────────────────────────────────────────────────────────────────

/// 将 [`NetDriver`] 适配为 smoltcp 的 [`phy::Device`]。
pub struct NetDeviceAdapter {
    driver: Arc<dyn NetDriver>,
    device: Arc<NetDevice>,
}

impl NetDeviceAdapter {
    pub fn new(driver: Arc<dyn NetDriver>, device: Arc<NetDevice>) -> Self {
        Self { driver, device }
    }
}

impl Device for NetDeviceAdapter {
    type RxToken<'a> = AdapterRxToken<'a>;
    type TxToken<'a> = AdapterTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if !self.device.is_active() {
            return None;
        }
        let rx_buf = self.driver.poll_rx()?;
        let tx_token = AdapterTxToken {
            driver: &self.driver,
            device: &self.device,
        };
        Some((
            AdapterRxToken {
                buf: rx_buf,
                driver: &self.driver,
            },
            tx_token,
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if !self.device.is_active() {
            return None;
        }
        Some(AdapterTxToken {
            driver: &self.driver,
            device: &self.device,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = match self.driver.medium() {
            LinkMedium::Ethernet => Medium::Ethernet,
            LinkMedium::Ip => Medium::Ip,
        };
        // Ethernet medium 的 MTU 是完整链路帧大小；IP medium 没有二层头，
        // 直接把 driver 暴露的 IP MTU 交给 smoltcp。
        caps.max_transmission_unit = match self.driver.medium() {
            LinkMedium::Ethernet => self.device.mtu() + 14,
            LinkMedium::Ip => self.device.mtu(),
        };
        caps
    }
}

// ── RxToken ──────────────────────────────────────────────────────────────────

/// smoltcp 接收令牌——零拷贝包装一个已接收的 [`RxBuf`]。
pub struct AdapterRxToken<'a> {
    buf: RxBuf,
    driver: &'a Arc<dyn NetDriver>,
}

impl<'a> phy::RxToken for AdapterRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let result = f(self.buf.as_slice());
        self.driver.recycle_rx(self.buf);
        result
    }
}

// ── TxToken ──────────────────────────────────────────────────────────────────

/// smoltcp 发送令牌——持有 adapter 字段引用，避免每帧 Arc clone。
pub struct AdapterTxToken<'a> {
    driver: &'a Arc<dyn NetDriver>,
    device: &'a Arc<NetDevice>,
}

impl<'a> phy::TxToken for AdapterTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // 热路径：smoltcp 已保证 len ≤ MTU；设备状态已在 transmit() 入口验证
        let Some(mut tx_buf) = self.driver.alloc_tx(len) else {
            // TX 队列满或驱动 teardown——记录丢帧统计后返回零长 buffer
            self.device.inc_tx_dropped();
            let mut discard = [0u8; 0];
            return f(&mut discard);
        };
        // 防御性 assert：驱动 bug 触发越界写时确定性 panic 而非 UB
        assert!(
            tx_buf.capacity() >= len,
            "alloc_tx returned undersized buffer: capacity={} requested={}",
            tx_buf.capacity(),
            len
        );
        let result = f(&mut tx_buf.as_mut_slice()[..len]);
        tx_buf.set_len(len);
        self.driver.commit_tx(tx_buf);
        result
    }
}
