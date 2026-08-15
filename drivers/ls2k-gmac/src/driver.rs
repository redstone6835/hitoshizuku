//! LS2K1000 GMAC（snps,dwmac-3.70a / ls,ls-gmac）platform ELM 驱动。
//!
//! 驱动匹配 2K1000LA 板工厂 DTB 的两个以太网节点（ethernet@40040000 /
//! ethernet@40050000，RGMII，各带一个 Motorcomm YT8511 PHY 的 mdio
//! 子节点），实现旧式单通道 DMA 核（normal 描述符、32 位地址、DMA
//! 寄存器块位于 MAC 基址 + 0x1000）的完整收发路径：
//!
//! - MAC/DMA 初始化与软复位（位定义对照 Linux stmmac dwmac1000）；
//! - RX/TX 描述符环：RX 缓冲直接来自 net 栈的 RxRefillBatch lease，
//!   TX 把多 fragment 线性化进每槽固定 DMA 缓冲（本核描述符只有两个
//!   缓冲区且 ring 模式不支持跨描述符分片）；
//! - MDIO（Loongson MII 布局）+ YT8511 C22 自动协商与链路轮询；
//! - macirq 中断：RI/TI 唤醒 net runtime worker，RU/UNF 等异常记录并
//!   触发 poll demand 重踢。
//!
//! 网卡注册为 [`NetQueueRegistration`]，队列实现 [`NetQueuePair`]，中断
//! 语义通过自实现的 [`QueueIrqControl`] 映射到 DMA_INTR_ENA/DMA_STATUS
//! （状态寄存器 RW1C）。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::sync::atomic::compiler_fence;

use spin::mutex::Mutex;

use general::dev::dma::{
    DmaBuffer, DmaContext, DmaDirection, DmaSyncRegion, new_netbuf_pool, new_shared_netbuf_pool,
    sync_for_device as dma_sync_for_device,
};
use general::dev::irq::{self, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use general::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use net::QueuePairId;
use net::buf::{
    CompletionBatch, CompletionToken, NetBufLease, NetBufPoolOwner, PacketBatch, PacketChain,
    PacketLayout, PacketMetadata, RxRefillBatch, TxBatch,
};
use net::device::{
    NetDeviceHandle, NetDeviceRegistration, NetQueueEndpoint, NetQueueRegistration,
    QueueIrqControl, QueueIrqError, QueueIrqStats, QueueWakeHandle,
};
use net::queue::{
    NetQueueCaps, NetQueuePair, QueueFatalError, RxBudget, RxPollResult, RxRefillResult,
    TxReclaimResult, TxSubmitResult,
};

use crate::regs::*;

const COMPAT_LS2K_GMAC: &str = "snps,dwmac-3.70a";
const COMPAT_LS_GMAC: &str = "ls,ls-gmac";

/// 每个方向描述符环大小（net 栈要求 16..=256 且为 2 的幂）。
const RING_SIZE: usize = 64;
/// RX 缓冲长度：MTU 1500 帧（1514 字节）加 VLAN 头余量，小于 2047 上限。
const RX_BUFFER_SIZE: usize = 1536;
/// TX 槽缓冲长度：线性化后单描述符承载。
const TX_BUFFER_SIZE: usize = 1536;
const MAX_BATCH: usize = 32;
const MAX_RX_REFILL_PER_CALL: usize = 32;
const PHY_ADDR: u8 = 0;

const MDIO_TIMEOUT_LOOPS: u32 = 10_000;
const DMA_RESET_TIMEOUT_LOOPS: u32 = 10_000;
const AN_TIMEOUT_NS: u64 = 3_000_000_000;
const AN_POLL_INTERVAL_NS: u64 = 20_000_000;

fn delay_ns(duration_ns: u64) {
    let deadline = sched::now_ns_public().saturating_add(duration_ns);
    while sched::now_ns_public() < deadline {
        core::hint::spin_loop();
    }
}

fn hash_mac_suffix(path: &str) -> [u8; 4] {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    [
        (hash >> 24) as u8,
        (hash >> 16) as u8,
        (hash >> 8) as u8,
        hash as u8,
    ]
}

/// 10/100/1000 与双工的 GMAC_CONTROL 编码（Linux stmmac link 映射）。
fn speed_duplex_bits(speed_mbps: u32, full_duplex: bool) -> u32 {
    let mut bits = match speed_mbps {
        1000 => 0,
        100 => GMAC_CONTROL_PS | GMAC_CONTROL_FES,
        _ => GMAC_CONTROL_PS,
    };
    if full_duplex {
        bits |= GMAC_CONTROL_DM;
    }
    bits
}

// ─────────────────────────── 队列中断控制 ───────────────────────────

/// 把 GMAC 的 macirq 语义映射成 net runtime 的 [`QueueIrqControl`]。
///
/// 硬件只有一条 DMA 中断线：DMA_STATUS 为 RW1C（写回已读值确认），
/// DMA_INTR_ENA 控制中断使能。mask = 清 INTR_ENA，unmask = 恢复默认
/// 使能掩码；IRQ handler 在观察到 NIS/AIS 后唤醒 worker。
pub struct GmacQueueIrq {
    base: usize,
    pending: AtomicBool,
    masked: AtomicBool,
    /// RU/OVF 异常后置位，refill 时重踢 RX poll demand。
    rx_need_kick: AtomicBool,
    waker: Mutex<Option<Arc<dyn QueueWakeHandle>>>,
    irq_total: AtomicU64,
    irq_mask: AtomicU64,
    irq_unmask: AtomicU64,
}

impl GmacQueueIrq {
    fn new(base: usize) -> Self {
        Self {
            base,
            pending: AtomicBool::new(false),
            masked: AtomicBool::new(true),
            rx_need_kick: AtomicBool::new(false),
            waker: Mutex::new(None),
            irq_total: AtomicU64::new(0),
            irq_mask: AtomicU64::new(0),
            irq_unmask: AtomicU64::new(0),
        }
    }

    fn mask_hardware(&self) {
        // Safety: base 由已完成 probe 的 GMAC MMIO 窗口提供，DMA_INTR_ENA
        // 为对齐的 32 位寄存器。
        unsafe { core::ptr::write_volatile((self.base + DMA_INTR_ENA) as *mut u32, 0) };
    }

    fn unmask_hardware(&self) {
        // Safety: 同 mask_hardware。
        unsafe {
            core::ptr::write_volatile(
                (self.base + DMA_INTR_ENA) as *mut u32,
                DMA_INTR_DEFAULT_MASK,
            )
        };
    }

    fn read_status(&self) -> u32 {
        // Safety: 同 mask_hardware；读状态寄存器同时采样全部中断位。
        unsafe { core::ptr::read_volatile((self.base + DMA_STATUS) as *const u32) }
    }

    fn wake(&self) {
        if let Some(waker) = self.waker.lock().as_ref() {
            waker.wake();
        }
    }

    /// 消费 RU/OVF 后的重踢标记。
    pub fn take_rx_need_kick(&self) -> bool {
        self.rx_need_kick.swap(false, Ordering::AcqRel)
    }
}

impl QueueIrqControl for GmacQueueIrq {
    fn ack_and_mask(&self) -> bool {
        self.mask_hardware();
        self.masked.store(true, Ordering::Release);
        self.irq_mask.fetch_add(1, Ordering::Relaxed);
        let status = self.read_status();
        let observed = status & (DMA_STATUS_NIS | DMA_STATUS_AIS) != 0;
        self.pending.swap(false, Ordering::AcqRel) || observed
    }

    fn unmask(&self) {
        self.pending.store(false, Ordering::Release);
        self.unmask_hardware();
        self.masked.store(false, Ordering::Release);
        self.irq_unmask.fetch_add(1, Ordering::Relaxed);
        if self.pending.load(Ordering::Acquire) {
            self.wake();
        }
    }

    fn set_waker(&self, waker: Arc<dyn QueueWakeHandle>) -> Result<(), QueueIrqError> {
        let mut slot = self.waker.lock();
        if slot.is_some() {
            return Err(QueueIrqError::WakerAlreadyInstalled);
        }
        *slot = Some(waker);
        Ok(())
    }

    fn clear_waker(&self) {
        *self.waker.lock() = None;
    }

    fn stats(&self) -> QueueIrqStats {
        QueueIrqStats {
            irq_total: self.irq_total.load(Ordering::Relaxed),
            irq_mask: self.irq_mask.load(Ordering::Relaxed),
            irq_unmask: self.irq_unmask.load(Ordering::Relaxed),
        }
    }
}

impl IrqHandler for GmacQueueIrq {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        let status = self.read_status();
        if status & DMA_STATUS_INTR_BITS == 0 {
            return IrqStatus::Unhandled;
        }
        // RW1C：写回已观察的中断位确认。
        // Safety: 同 mask_hardware。
        unsafe {
            core::ptr::write_volatile(
                (self.base + DMA_STATUS) as *mut u32,
                status & DMA_STATUS_INTR_BITS,
            )
        };
        self.irq_total.fetch_add(1, Ordering::Relaxed);
        if status & (DMA_STATUS_NIS | DMA_STATUS_AIS) != 0 {
            if status & DMA_STATUS_AIS != 0 {
                if status & (DMA_STATUS_RU | DMA_STATUS_OVF) != 0 {
                    // 缓冲不足：refill 后需要 poll demand 重踢 RX。
                    self.rx_need_kick.store(true, Ordering::Release);
                }
                if status & (DMA_STATUS_UNF | DMA_STATUS_TU) != 0 {
                    // TX 下溢/缓冲不可用：kick 一次 TX poll demand。
                    // Safety: 同 mask_hardware。
                    unsafe {
                        core::ptr::write_volatile(
                            (self.base + DMA_XMT_POLL_DEMAND) as *mut u32,
                            0,
                        )
                    };
                }
            }
            self.mask_hardware();
            self.masked.store(true, Ordering::Release);
            self.irq_mask.fetch_add(1, Ordering::Relaxed);
            if !self.pending.swap(true, Ordering::AcqRel) {
                self.wake();
            }
        }
        IrqStatus::Handled
    }
}

// ─────────────────────────── MAC / DMA 硬件 ───────────────────────────

/// 一个 GMAC 实例共享的 MMIO、描述符环与 DMA 缓冲。
pub struct GmacMac {
    base: usize,
    rx_ring: DmaBuffer,
    tx_ring: DmaBuffer,
    tx_buffers: DmaBuffer,
    irq: Arc<GmacQueueIrq>,
}

impl GmacMac {
    fn new(base: usize, context: DmaContext) -> Result<Self, &'static str> {
        let rx_ring = DmaBuffer::new_in(
            context.clone(),
            RING_SIZE * core::mem::size_of::<DmaDesc>(),
            32,
            DmaDirection::Bidirectional,
        )?;
        let tx_ring = DmaBuffer::new_in(
            context.clone(),
            RING_SIZE * core::mem::size_of::<DmaDesc>(),
            32,
            DmaDirection::Bidirectional,
        )?;
        let tx_buffers = DmaBuffer::new_in(
            context,
            RING_SIZE * TX_BUFFER_SIZE,
            64,
            DmaDirection::ToDevice,
        )?;
        Ok(Self {
            base,
            rx_ring,
            tx_ring,
            tx_buffers,
            irq: Arc::new(GmacQueueIrq::new(base)),
        })
    }

    pub fn irq(&self) -> Arc<GmacQueueIrq> {
        Arc::clone(&self.irq)
    }

    fn read32(&self, offset: usize) -> u32 {
        // Safety: offset 是受控的固定寄存器偏移，base 由 platform probe 映射。
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // Safety: 同 read32，目标寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }

    fn rx_desc(&self, slot: usize) -> &DmaDesc {
        // Safety: slot < RING_SIZE，描述符数组常驻且与 DMA 共享。
        let bytes = self.rx_ring.as_slice();
        unsafe {
            &*(bytes.as_ptr().add(slot * core::mem::size_of::<DmaDesc>()) as *const DmaDesc)
        }
    }

    fn rx_desc_mut(&self, slot: usize) -> &mut DmaDesc {
        // Safety: 单 worker 独占 queue 调用，slot < RING_SIZE。
        let bytes = self.rx_ring.as_slice();
        unsafe {
            &mut *(bytes.as_ptr().add(slot * core::mem::size_of::<DmaDesc>()) as *mut DmaDesc)
        }
    }

    fn tx_desc(&self, slot: usize) -> &DmaDesc {
        // Safety: 同 rx_desc。
        let bytes = self.tx_ring.as_slice();
        unsafe {
            &*(bytes.as_ptr().add(slot * core::mem::size_of::<DmaDesc>()) as *const DmaDesc)
        }
    }

    fn tx_desc_mut(&self, slot: usize) -> &mut DmaDesc {
        // Safety: 单 worker 独占 queue 调用，slot < RING_SIZE。
        let bytes = self.tx_ring.as_slice();
        unsafe {
            &mut *(bytes.as_ptr().add(slot * core::mem::size_of::<DmaDesc>()) as *mut DmaDesc)
        }
    }

    fn tx_slot_mut(&self, slot: usize) -> &mut [u8] {
        // Safety: slot < RING_SIZE；可变访问由上层 queue 的 Mutex 串行化，
        // tx_buffers 常驻且按槽对齐。
        let base = self.tx_buffers.vaddr() as *mut u8;
        unsafe {
            core::slice::from_raw_parts_mut(base.add(slot * TX_BUFFER_SIZE), TX_BUFFER_SIZE)
        }
    }

    fn tx_slot_dma(&self, slot: usize) -> usize {
        self.tx_buffers.dma_addr() + slot * TX_BUFFER_SIZE
    }

    fn sync_tx_slot(&self, slot: usize, len: usize) {
        dma_sync_for_device(DmaSyncRegion {
            paddr: self.tx_buffers.paddr() + slot * TX_BUFFER_SIZE,
            vaddr: self.tx_buffers.vaddr() + slot * TX_BUFFER_SIZE,
            len,
            direction: DmaDirection::ToDevice,
        });
    }

    fn sync_rings_for_device(&self) {
        self.rx_ring.sync_for_device();
        self.tx_ring.sync_for_device();
    }

    /// 初始化描述符环（全部 CPU 所有，末项置 END_RING）。
    fn init_rings(&self) {
        for slot in 0..RING_SIZE {
            let tx = self.tx_desc_mut(slot);
            *tx = DmaDesc::default();
            if slot == RING_SIZE - 1 {
                tx.des1 |= TDES1_END_RING;
            }
        }
        for slot in 0..RING_SIZE {
            let rx = self.rx_desc_mut(slot);
            *rx = DmaDesc::default();
            if slot == RING_SIZE - 1 {
                rx.des1 |= RDES1_END_RING;
            }
        }
        self.sync_rings_for_device();
    }

    /// DMA 软复位（Linux dwmac_dma_reset）。
    fn dma_reset(&self) -> Result<(), &'static str> {
        self.write32(DMA_BUS_MODE, self.read32(DMA_BUS_MODE) | DMA_BUS_MODE_SFT_RESET);
        for _ in 0..DMA_RESET_TIMEOUT_LOOPS {
            if self.read32(DMA_BUS_MODE) & DMA_BUS_MODE_SFT_RESET == 0 {
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err("GMAC DMA soft reset timed out")
    }

    /// DMA 总线模式与环基址、操作模式、中断掩码（Linux dwmac1000_dma_init）。
    fn dma_init(&self) {
        let pbl: u32 = 0x20;
        self.write32(
            DMA_BUS_MODE,
            (pbl << DMA_BUS_MODE_PBL_SHIFT)
                | (pbl << DMA_BUS_MODE_RPBL_SHIFT)
                | DMA_BUS_MODE_FB
                | DMA_BUS_MODE_USP
                | DMA_BUS_MODE_MAXPBL,
        );
        self.write32(DMA_RCV_BASE_ADDR, self.rx_ring.dma_addr() as u32);
        self.write32(DMA_TX_BASE_ADDR, self.tx_ring.dma_addr() as u32);
        // store-and-forward 收/发，关闭流控（RX FIFO 未知且偏小）。
        self.write32(DMA_CONTROL, DMA_CONTROL_TSF | DMA_CONTROL_RSF);
        self.write32(DMA_INTR_ENA, DMA_INTR_DEFAULT_MASK);
    }

    fn start_dma(&self) {
        self.write32(DMA_CONTROL, self.read32(DMA_CONTROL) | DMA_CONTROL_ST | DMA_CONTROL_SR);
    }

    fn stop_dma(&self) {
        self.write32(
            DMA_CONTROL,
            self.read32(DMA_CONTROL) & !(DMA_CONTROL_ST | DMA_CONTROL_SR),
        );
    }

    /// 设置 MAC 地址、速度/双工与收发使能。
    fn mac_init(&self, mac_address: [u8; 6], speed_mbps: u32, full_duplex: bool) {
        let mut control = GMAC_CORE_INIT;
        control |= speed_duplex_bits(speed_mbps, full_duplex);
        control |= GMAC_CONTROL_TE | GMAC_CONTROL_RE;
        self.write32(GMAC_CONTROL, control);
        self.write32(GMAC_FRAME_FILTER, 0);
        self.write32(
            GMAC_ADDR0_HIGH,
            (u32::from(mac_address[0]) << 8) | u32::from(mac_address[1]),
        );
        self.write32(
            GMAC_ADDR0_LOW,
            (u32::from(mac_address[2]) << 24)
                | (u32::from(mac_address[3]) << 16)
                | (u32::from(mac_address[4]) << 8)
                | u32::from(mac_address[5]),
        );
    }

    /// 运行期更新链路速度/双工（保留 TE/RE 等其它控制位）。
    pub fn set_link(&self, speed_mbps: u32, full_duplex: bool) {
        let mut control = self.read32(GMAC_CONTROL);
        control &= !(GMAC_CONTROL_PS | GMAC_CONTROL_FES | GMAC_CONTROL_DM);
        control |= speed_duplex_bits(speed_mbps, full_duplex);
        self.write32(GMAC_CONTROL, control);
    }

    fn kick_tx(&self) {
        self.write32(DMA_XMT_POLL_DEMAND, 0);
    }

    fn kick_rx(&self) {
        self.write32(DMA_RCV_POLL_DEMAND, 0);
    }

    // ── MDIO / PHY ──

    fn mdio_wait_idle(&self) -> Result<(), &'static str> {
        for _ in 0..MDIO_TIMEOUT_LOOPS {
            if self.read32(GMAC_MII_ADDR) & MII_ADDR_GBUSY == 0 {
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err("GMAC MDIO busy timeout")
    }

    pub fn mdio_read(&self, phy_addr: u8, reg: u16) -> Result<u16, &'static str> {
        self.mdio_wait_idle()?;
        self.write32(GMAC_MII_DATA, 0);
        self.write32(
            GMAC_MII_ADDR,
            (u32::from(phy_addr) << MII_PA_SHIFT)
                | (u32::from(reg) << MII_RDA_SHIFT)
                | (MII_CR_100_150M << MII_CR_SHIFT)
                | MII_ADDR_GBUSY,
        );
        self.mdio_wait_idle()?;
        Ok((self.read32(GMAC_MII_DATA) & 0xffff) as u16)
    }

    pub fn mdio_write(&self, phy_addr: u8, reg: u16, value: u16) -> Result<(), &'static str> {
        self.mdio_wait_idle()?;
        self.write32(GMAC_MII_DATA, u32::from(value));
        self.write32(
            GMAC_MII_ADDR,
            (u32::from(phy_addr) << MII_PA_SHIFT)
                | (u32::from(reg) << MII_RDA_SHIFT)
                | (MII_CR_100_150M << MII_CR_SHIFT)
                | MII_ADDR_GWRITE
                | MII_ADDR_GBUSY,
        );
        self.mdio_wait_idle()
    }

    /// 复位 PHY 并启动自动协商，等待完成并解析链路速度/双工。
    ///
    /// 返回 (speed_mbps, full_duplex)。RGMII 无延迟属性时保持硬件默认
    /// 延迟配置（与板级 5.10 内核一致）；真机联调可在此处按 PHY 页寄存器
    /// 调整 rx/tx delay。
    pub fn phy_bringup(&self) -> Result<(u32, bool), &'static str> {
        self.mdio_write(PHY_ADDR, MII_BMCR, BMCR_RESET)?;
        delay_ns(20_000_000);
        let id1 = self.mdio_read(PHY_ADDR, MII_PHYIDR1)?;
        let id2 = self.mdio_read(PHY_ADDR, MII_PHYIDR2)?;
        log::printk!(
            "[ls2k-gmac] phy at {}: PHYIDR1={:#06x} PHYIDR2={:#06x}",
            PHY_ADDR,
            id1,
            id2
        );

        self.mdio_write(PHY_ADDR, MII_CTRL1000, CTRL1000_1000FULL)?;
        self.mdio_write(PHY_ADDR, MII_ADVERTISE, ADVERTISE_ALL)?;
        self.mdio_write(PHY_ADDR, MII_BMCR, BMCR_ANENABLE | BMCR_ANRESTART)?;

        let deadline = sched::now_ns_public().saturating_add(AN_TIMEOUT_NS);
        loop {
            let bmsr = self.mdio_read(PHY_ADDR, MII_BMSR)?;
            if bmsr & BMSR_ANEGCOMPLETE != 0 && bmsr & BMSR_LINKSTATUS != 0 {
                break;
            }
            if sched::now_ns_public() >= deadline {
                log::printk!(
                    "[ls2k-gmac] phy autoneg/link timeout (BMSR={:#06x}); using last known state",
                    bmsr
                );
                break;
            }
            delay_ns(AN_POLL_INTERVAL_NS);
        }

        let stat1000 = self.mdio_read(PHY_ADDR, MII_STAT1000)?;
        let lpa = self.mdio_read(PHY_ADDR, MII_LPA)?;
        let (speed, full_duplex) = if stat1000 & STAT1000_1000FULL != 0 {
            (1000, true)
        } else if lpa & LPA_100FD != 0 {
            (100, true)
        } else if lpa & LPA_100HD != 0 {
            (100, false)
        } else if lpa & LPA_10FD != 0 {
            (10, true)
        } else if lpa & LPA_10HD != 0 {
            (10, false)
        } else {
            (1000, true)
        };
        log::printk!(
            "[ls2k-gmac] phy link: {} Mbps {} (stat1000={:#06x} lpa={:#06x})",
            speed,
            if full_duplex { "full" } else { "half" },
            stat1000,
            lpa
        );
        Ok((speed, full_duplex))
    }

    /// 轻量链路检查：用于 worker 空闲路径，不阻塞太久。
    pub fn check_link(&self) -> Option<(u32, bool)> {
        match self.mdio_read(PHY_ADDR, MII_BMSR) {
            Ok(bmsr) if bmsr & BMSR_LINKSTATUS != 0 => {
                let stat1000 = self.mdio_read(PHY_ADDR, MII_STAT1000).unwrap_or(0);
                let lpa = self.mdio_read(PHY_ADDR, MII_LPA).unwrap_or(0);
                let (speed, full_duplex) = if stat1000 & STAT1000_1000FULL != 0 {
                    (1000, true)
                } else if lpa & LPA_100FD != 0 {
                    (100, true)
                } else if lpa & LPA_100HD != 0 {
                    (100, false)
                } else if lpa & LPA_10FD != 0 {
                    (10, true)
                } else if lpa & LPA_10HD != 0 {
                    (10, false)
                } else {
                    (1000, true)
                };
                Some((speed, full_duplex))
            }
            _ => None,
        }
    }
}

// ─────────────────────────── 队列对实现 ───────────────────────────

struct TxPending {
    completion: CompletionToken,
}

/// 单个 GMAC 的 net queue pair（本驱动固定一个数据队列）。
pub struct GmacQueue {
    id: QueuePairId,
    mac: Arc<GmacMac>,
    quiesced: bool,
    rx_pending: [Option<NetBufLease>; RING_SIZE],
    tx_pending: [Option<TxPending>; RING_SIZE],
    rx_refill_index: usize,
    rx_poll_index: usize,
    tx_submit_index: usize,
    tx_reclaim_index: usize,
    link_checked: bool,
}

impl GmacQueue {
    fn new(id: QueuePairId, mac: Arc<GmacMac>) -> Self {
        Self {
            id,
            mac,
            quiesced: false,
            rx_pending: core::array::from_fn(|_| None),
            tx_pending: core::array::from_fn(|_| None),
            rx_refill_index: 0,
            rx_poll_index: 0,
            tx_submit_index: 0,
            tx_reclaim_index: 0,
            link_checked: false,
        }
    }

    fn caps_value(queue_size: u16) -> NetQueueCaps {
        NetQueueCaps {
            queue_size,
            scatter_gather: false,
            max_tx_descriptors: 1,
            max_rx_batch: MAX_BATCH as u8,
            max_tx_batch: MAX_BATCH as u8,
            tx_checksum: false,
            udp_segmentation: false,
            max_udp_segments: 0,
        }
    }

    fn poll_link_once(&mut self) {
        if self.link_checked {
            return;
        }
        self.link_checked = true;
        if let Some((speed, full_duplex)) = self.mac.check_link() {
            self.mac.set_link(speed, full_duplex);
        }
    }

    fn clear_pending(&mut self) {
        for slot in self.rx_pending.iter_mut() {
            *slot = None;
        }
        for slot in self.tx_pending.iter_mut() {
            *slot = None;
        }
    }
}

impl NetQueuePair for GmacQueue {
    fn id(&self) -> QueuePairId {
        self.id
    }

    fn caps(&self) -> NetQueueCaps {
        Self::caps_value(RING_SIZE as u16)
    }

    fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> RxRefillResult {
        if self.quiesced {
            return RxRefillResult {
                posted: 0,
                descriptor_starved: false,
                fatal: Some(QueueFatalError::DeviceGone),
            };
        }
        let original_len = batch.len();
        let mut posted = 0usize;
        for index in 0..original_len.min(MAX_RX_REFILL_PER_CALL) {
            let slot = self.rx_refill_index;
            if self.rx_pending[slot].is_some() {
                // 环已满（poll 尚未消费），停止本次 refill。
                break;
            }
            let Some(lease) = batch.take(index) else {
                continue;
            };
            if lease.len() > RX_BUFFER_SIZE {
                let _ = batch.put(index, lease);
                continue;
            }
            if lease.sync_for_device().is_err() {
                let _ = batch.put(index, lease);
                return RxRefillResult {
                    posted: posted as u16,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::DmaFault),
                };
            }
            let Some(dma) = lease.dma_addr().ok().flatten() else {
                let _ = batch.put(index, lease);
                break;
            };
            let desc = self.mac.rx_desc_mut(slot);
            desc.des2 = dma as u32;
            desc.des1 = (lease.len() as u32) & RDES1_BUFFER1_SIZE_MASK;
            // des1 的 END_RING 由 init_rings 置位，这里只清 DISABLE_IC。
            desc.des1 &= !RDES1_DISABLE_IC;
            compiler_fence(Ordering::SeqCst);
            desc.des0 = RDES0_OWN;
            self.rx_pending[slot] = Some(lease);
            posted += 1;
            self.rx_refill_index = (slot + 1) % RING_SIZE;
        }
        if posted != 0 && self.mac.irq().take_rx_need_kick() {
            self.mac.kick_rx();
        }
        RxRefillResult {
            posted: posted as u16,
            descriptor_starved: !batch.is_empty()
                && self.rx_pending.iter().all(Option::is_some),
            fatal: None,
        }
    }

    fn poll_rx_batch(&mut self, budget: RxBudget, out: &mut PacketBatch) -> RxPollResult {
        if self.quiesced {
            return RxPollResult {
                packets: 0,
                bytes: 0,
                ring_empty: true,
                descriptor_starved: false,
                fatal: Some(QueueFatalError::DeviceGone),
            };
        }
        let mut packets = 0u16;
        let mut bytes = 0u32;
        while usize::from(packets) < MAX_BATCH {
            let slot = self.rx_poll_index;
            let Some(mut lease) = self.rx_pending[slot].take() else {
                break;
            };
            let rdes0 = self.mac.rx_desc(slot).des0;
            if rdes0 & RDES0_OWN != 0 {
                break;
            }
            self.rx_poll_index = (slot + 1) % RING_SIZE;
            if rdes0 & RDES0_ERROR_SUMMARY != 0 || rdes0 & RDES0_LAST_DESCRIPTOR == 0 {
                // 错误帧：把 lease 归还 pool（drop 即归还），槽位等待 refill。
                drop(lease);
                continue;
            }
            let frame_len = ((rdes0 & RDES0_FRAME_LEN_MASK) >> RDES0_FRAME_LEN_SHIFT) as usize;
            if frame_len == 0 || frame_len > lease.len() {
                drop(lease);
                continue;
            }
            if lease.set_data_range(0, frame_len as u16).is_err() {
                drop(lease);
                continue;
            }
            let metadata = PacketMetadata {
                frame_len: frame_len as u32,
                checksums_validated: false,
                layout: PacketLayout::Plain,
                ..PacketMetadata::default()
            };
            let chain = PacketChain::from_lease(lease);
            if out.push(chain, metadata).is_err() {
                return RxPollResult {
                    packets,
                    bytes,
                    ring_empty: false,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            packets += 1;
            bytes = bytes.saturating_add(frame_len as u32);
            if usize::from(packets) >= usize::from(budget.packets) || bytes >= budget.bytes {
                break;
            }
        }
        let ring_empty = match self.rx_pending[self.rx_poll_index].as_ref() {
            None => true,
            Some(_) => self.mac.rx_desc(self.rx_poll_index).des0 & RDES0_OWN != 0,
        };
        RxPollResult {
            packets,
            bytes,
            ring_empty,
            descriptor_starved: false,
            fatal: None,
        }
    }

    fn submit_tx_batch(&mut self, batch: &mut TxBatch, _header_pool: &mut NetBufPoolOwner) -> TxSubmitResult {
        if self.quiesced {
            return TxSubmitResult {
                packets: 0,
                descriptors: 0,
                bytes: 0,
                queue_full: false,
                fatal: Some(QueueFatalError::DeviceGone),
            };
        }
        let original_len = batch.len();
        let mut submitted = 0usize;
        let mut byte_total = 0u32;
        for index in 0..original_len.min(MAX_BATCH) {
            let Some(candidate) = batch.packet(index) else {
                continue;
            };
            let total = candidate.chain.total_len();
            if !candidate.checksum.valid_for(false, total) || total > TX_BUFFER_SIZE {
                // 本核不做校验和卸载、单缓冲承载，超限帧留待上层处理。
                break;
            }
            let slot = self.tx_submit_index;
            if self.tx_pending[slot].is_some() {
                break;
            }
            // 线性化多 fragment 到槽缓冲。
            let slot_bytes = self.mac.tx_slot_mut(slot);
            let mut offset = 0usize;
            for fragment_index in 0..candidate.chain.fragment_count() {
                let Some(fragment) = candidate.chain.fragment(fragment_index) else {
                    break;
                };
                let Ok(bytes) = fragment.as_slice() else {
                    return TxSubmitResult {
                        packets: submitted as u16,
                        descriptors: 0,
                        bytes: byte_total,
                        queue_full: false,
                        fatal: Some(QueueFatalError::DmaFault),
                    };
                };
                slot_bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
                offset += bytes.len();
            }
            if offset != total {
                return TxSubmitResult {
                    packets: submitted as u16,
                    descriptors: 0,
                    bytes: byte_total,
                    queue_full: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            let Some(packet) = batch.take(index) else {
                break;
            };
            self.mac.sync_tx_slot(slot, total);
            let desc = self.mac.tx_desc_mut(slot);
            desc.des2 = self.mac.tx_slot_dma(slot) as u32;
            desc.des1 = ((total as u32) & TDES1_BUFFER1_SIZE_MASK)
                | TDES1_FIRST_SEGMENT
                | TDES1_LAST_SEGMENT
                | TDES1_INTERRUPT;
            compiler_fence(Ordering::SeqCst);
            desc.des0 = TDES0_OWN;
            self.tx_pending[slot] = Some(TxPending {
                completion: packet.completion,
            });
            drop(packet);
            submitted += 1;
            byte_total = byte_total.saturating_add(total as u32);
            self.tx_submit_index = (slot + 1) % RING_SIZE;
        }
        if submitted != 0 {
            self.mac.kick_tx();
        }
        TxSubmitResult {
            packets: submitted as u16,
            descriptors: submitted as u16,
            bytes: byte_total,
            queue_full: submitted != original_len,
            fatal: None,
        }
    }

    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult {
        let mut completions = 0u16;
        while usize::from(completions) < MAX_BATCH {
            let slot = self.tx_reclaim_index;
            let Some(pending) = self.tx_pending[slot].take() else {
                break;
            };
            if self.mac.tx_desc(slot).des0 & TDES0_OWN != 0 {
                break;
            }
            self.tx_pending[slot] = None;
            self.tx_reclaim_index = (slot + 1) % RING_SIZE;
            if out.push(pending.completion).is_err() {
                return TxReclaimResult {
                    completions,
                    descriptors: completions,
                    ring_empty: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            completions += 1;
        }
        TxReclaimResult {
            completions,
            descriptors: completions,
            ring_empty: self.tx_pending[self.tx_reclaim_index].is_none(),
            fatal: None,
        }
    }

    fn has_pending_work(&mut self) -> bool {
        if self.quiesced {
            return false;
        }
        self.poll_link_once();
        if let Some(_lease) = self.rx_pending[self.rx_poll_index].as_ref()
            && self.mac.rx_desc(self.rx_poll_index).des0 & RDES0_OWN == 0
        {
            return true;
        }
        if self.tx_pending[self.tx_reclaim_index].is_some()
            && self.mac.tx_desc(self.tx_reclaim_index).des0 & TDES0_OWN == 0
        {
            return true;
        }
        false
    }

    fn quiesce(&mut self) -> Result<(), QueueFatalError> {
        self.quiesced = true;
        self.mac.stop_dma();
        self.mac.write32(DMA_INTR_ENA, 0);
        self.clear_pending();
        Ok(())
    }
}

struct SharedGmacQueue {
    inner: Arc<Mutex<GmacQueue>>,
}

impl NetQueuePair for SharedGmacQueue {
    fn id(&self) -> QueuePairId {
        self.inner.lock().id()
    }

    fn caps(&self) -> NetQueueCaps {
        self.inner.lock().caps()
    }

    fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> RxRefillResult {
        self.inner.lock().refill_rx_batch(batch)
    }

    fn poll_rx_batch(&mut self, budget: RxBudget, out: &mut PacketBatch) -> RxPollResult {
        self.inner.lock().poll_rx_batch(budget, out)
    }

    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult {
        self.inner.lock().reclaim_tx_batch(out)
    }

    fn submit_tx_batch(
        &mut self,
        batch: &mut TxBatch,
        header_pool: &mut NetBufPoolOwner,
    ) -> TxSubmitResult {
        self.inner.lock().submit_tx_batch(batch, header_pool)
    }

    fn has_pending_work(&mut self) -> bool {
        self.inner.lock().has_pending_work()
    }

    fn quiesce(&mut self) -> Result<(), QueueFatalError> {
        self.inner.lock().quiesce()
    }
}

// ─────────────────────────── PnP 驱动 ───────────────────────────

struct Ls2kGmacBinding {
    mac: Arc<GmacMac>,
    queue: Arc<Mutex<GmacQueue>>,
    handle: NetDeviceHandle,
    irq_handle: IrqHandle,
}

pub struct Ls2kGmacDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2kGmacDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self { device_mmio_to_virt }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LS2K_GMAC) || info.has_id(COMPAT_LS_GMAC)
    }

    fn register_irq(
        &self,
        mac: &Arc<GmacMac>,
        info: &PlatformDeviceInfo,
    ) -> Result<IrqHandle, PnpError> {
        let handler: Arc<dyn IrqHandler> = mac.irq();
        match info.register_first_irq_handler(handler) {
            Ok(handle) => Ok(handle),
            Err(PlatformIrqRegistrationError::NoResource) => {
                Err(PnpError::missing(PnpResourceKind::Irq, "gmac macirq missing"))
            }
            Err(PlatformIrqRegistrationError::Unresolved) => Err(PnpError::dependency(
                info.irq_resources()
                    .find_map(|irq| irq.controller())
                    .map(PnpDependency::IrqController)
                    .unwrap_or(PnpDependency::DefaultIrqDomain),
            )),
            Err(PlatformIrqRegistrationError::RegistrationFailed { line, err }) => {
                log::printk!("[ls2k-gmac] failed to register macirq {:?}: {:?}", line, err);
                Err(PnpError::registration_failed(
                    PnpResourceKind::Irq,
                    "gmac macirq registration failed",
                ))
            }
        }
    }
}

impl PnpDriver for Ls2kGmacDriver {
    fn name(&self) -> &'static str {
        "platform-ls2k-gmac"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        if !matches!(id, PnpId::Platform { .. }) {
            return false;
        }
        info.as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "gmac reg missing"));
        };
        if size < 0x1100 {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "gmac register window too small",
            ));
        }
        let bus_id = info.u32_property("bus_id").unwrap_or(0);
        let mut mac_address = [0u8; 6];
        mac_address[0] = 0x02; // locally administered
        let suffix = hash_mac_suffix(info.fw_path.as_deref().unwrap_or("ls2k-gmac"));
        mac_address[1..].copy_from_slice(&suffix);
        mac_address[5] ^= bus_id as u8;

        let mac = Arc::new(
            GmacMac::new((self.device_mmio_to_virt)(phys), info.dma_context())
                .map_err(|_| PnpError::OutOfMemory)?,
        );
        mac.init_rings();
        mac.dma_reset()
            .map_err(|_| PnpError::hardware_failure("gmac dma reset failed"))?;
        mac.dma_init();
        let (speed, full_duplex) = mac
            .phy_bringup()
            .map_err(|err| PnpError::hardware_failure(err))?;
        mac.mac_init(mac_address, speed, full_duplex);
        mac.start_dma();

        let irq_handle = self.register_irq(&mac, info)?;

        let queue = Arc::new(Mutex::new(GmacQueue::new(QueuePairId(0), Arc::clone(&mac))));
        let rx_pool = new_netbuf_pool(
            info.dma_context(),
            RING_SIZE,
            RX_BUFFER_SIZE,
            64,
            DmaDirection::FromDevice,
        )
        .map_err(|_| PnpError::OutOfMemory)?;
        let tx_header_pool = new_netbuf_pool(
            info.dma_context(),
            8,
            64,
            64,
            DmaDirection::ToDevice,
        )
        .map_err(|_| PnpError::OutOfMemory)?;
        let tx_payload_pool = new_shared_netbuf_pool(
            info.dma_context(),
            RING_SIZE,
            TX_BUFFER_SIZE,
            64,
            DmaDirection::ToDevice,
        )
        .map_err(|_| PnpError::OutOfMemory)?;
        let socket_tx_pool = new_shared_netbuf_pool(
            info.dma_context(),
            RING_SIZE.saturating_mul(net::tuning::SOCKET_TX_POOL_DEPTH_MULTIPLIER),
            TX_BUFFER_SIZE,
            64,
            DmaDirection::ToDevice,
        )
        .map_err(|_| PnpError::OutOfMemory)?;

        let endpoint = NetQueueEndpoint::Integrated(Box::new(SharedGmacQueue {
            inner: Arc::clone(&queue),
        }));
        let registration = NetQueueRegistration {
            id: QueuePairId(0),
            queue: endpoint,
            rx_pool,
            tx_header_pool,
            tx_payload_pool,
            socket_tx_pool,
            irq: mac.irq(),
        };
        let name = alloc::format!("eth{}", bus_id).into_boxed_str();
        let handle = net::device::register_device(NetDeviceRegistration::new(
            name,
            mac_address,
            1500,
            true,
            alloc::vec![registration].into_boxed_slice(),
        ))
        .map_err(|_error| PnpError::hardware_failure("gmac net device registration failed"))?;

        log::printk!(
            "[ls2k-gmac] bound {} phys={:#x} size={:#x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} irq={:?} speed={}Mbps {}",
            dev.id,
            phys,
            size,
            mac_address[0],
            mac_address[1],
            mac_address[2],
            mac_address[3],
            mac_address[4],
            mac_address[5],
            irq_handle.line(),
            speed,
            if full_duplex { "full" } else { "half" },
        );

        dev.set_driver_data(Arc::new(Ls2kGmacBinding {
            mac,
            queue,
            handle,
            irq_handle,
        }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<Ls2kGmacBinding>()
        {
            let _ = binding.queue.lock().quiesce();
            let _ = binding
                .mac
                .irq()
                .clear_waker();
            let _ = irq::unregister_irq_handler(binding.irq_handle);
            let _ = net::device::begin_remove(binding.handle);
        }
        log::printk!("[ls2k-gmac] removed {}", dev.id);
    }
}

struct Ls2kGmacFactory;

impl DriverFactory for Ls2kGmacFactory {
    fn name(&self) -> &'static str {
        "platform-ls2k-gmac"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2kGmacDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls2kGmacFactory))
}
