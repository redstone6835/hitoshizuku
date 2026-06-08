//! VirtIO-Net PCI 驱动。
//!
//! 通过 PCI capability chain 定位 common/notify/device config 区域，
//! 使用两个 virtqueue（RX queue 0 + TX queue 1）实现以太网帧收发。
//! 实现 `net::NetDriver` trait，可直接接入 smoltcp 协议栈。

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::mem;
use core::ptr::read_volatile;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use spin::Mutex;

use crate::dev::dma::{DmaBuffer, DmaDirection};
use crate::dev::irq::{self, IrqError, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use crate::dev::naming::StableNameAllocator;
use crate::dev::net::NetFunction;
use crate::dev::pci::{PciDevice, PciInfo};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};
use crate::dev::virtio::{
    VIRTIO_F_VERSION_1, VIRTIO_PCI_FUNCTION_NETWORK, VIRTIO_PCI_RESET_SPIN_LIMIT,
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FAILED,
    VIRTIO_STATUS_FEATURES_OK, VIRTQ_DESC_F_WRITE, VirtioPciTransport, choose_split_queue_size,
    parse_virtio_pci_caps,
};

/// 网络接口短名由网络设备类别统一分配。
///
/// 这不是 devtmpfs 节点名，也不参与 POSIX 设备号投影；它只是网络栈和用户态
/// ioctl 看到的接口名。使用 PnP 硬件名作为 key 可以让同一网卡重绑后继续使用
/// 原接口名，避免 probe 顺序影响网络配置。
static NET_IFACE_NAMES: StableNameAllocator = StableNameAllocator::new("eth");

// ── VirtIO-Net feature bits ────────────────────────────────────────────

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;

// ── VirtIO-Net device config offsets ───────────────────────────────────

const NET_CFG_MAC: usize = 0x00;
#[allow(dead_code)]
const NET_CFG_STATUS: usize = 0x06;

// ── Virtqueue 常量 ─────────────────────────────────────────────────────

const VIRTIO_NET_QUEUE_LIMIT: u16 = 128;
const VIRTIO_NET_QUEUE_LIMIT_USIZE: usize = VIRTIO_NET_QUEUE_LIMIT as usize;
const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;

const VIRTIO_NET_HDR_SIZE: usize = 12;
const ETHERNET_MAX_FRAME_LEN: usize = 1514;
const RX_BUF_SIZE: usize = VIRTIO_NET_HDR_SIZE + ETHERNET_MAX_FRAME_LEN;
const VIRTIO_NET_MAC_LEN: usize = 6;
const VIRTIO_NET_STATUS_LINK_UP: u16 = 1;

// ── Virtqueue 结构体 ───────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; VIRTIO_NET_QUEUE_LIMIT_USIZE],
    used_event: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; VIRTIO_NET_QUEUE_LIMIT_USIZE],
    avail_event: u16,
}

// ── VirtIO-Net header ──────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioNetHdr {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16,
    // 补到 12 字节
}

// ── 队列状态 ───────────────────────────────────────────────────────────

struct VirtioNetQueue {
    desc_dma: DmaBuffer,
    avail_dma: DmaBuffer,
    used_dma: DmaBuffer,
    desc_table: *mut VirtqDesc,
    avail_ring: *mut VirtqAvail,
    used_ring: *mut VirtqUsed,
    queue_size: u16,
    last_used_idx: u16,
    notify_addr: usize,
}

unsafe impl Send for VirtioNetQueue {}
unsafe impl Sync for VirtioNetQueue {}

// ── 驱动主结构 ─────────────────────────────────────────────────────────

struct VirtioNetInner {
    transport: VirtioPciTransport,
    rx_queue: VirtioNetQueue,
    tx_queue: VirtioNetQueue,
    rx_buffers: Vec<DmaBuffer>,
    tx_free_descs: VecDeque<u16>,
    pending_tx: VecDeque<(u16, DmaBuffer)>,
}

unsafe impl Send for VirtioNetInner {}
unsafe impl Sync for VirtioNetInner {}

pub struct VirtioNetPci {
    inner: Mutex<VirtioNetInner>,
    mac: [u8; 6],
    has_status: bool,
    stats: VirtioNetStats,
}

struct VirtioNetStats {
    rx_packets: AtomicU64,
    rx_bytes: AtomicU64,
    rx_errors: AtomicU64,
    rx_dropped: AtomicU64,
    tx_packets: AtomicU64,
    tx_bytes: AtomicU64,
    tx_errors: AtomicU64,
    tx_dropped: AtomicU64,
}

impl VirtioNetStats {
    const fn new() -> Self {
        Self {
            rx_packets: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            rx_errors: AtomicU64::new(0),
            rx_dropped: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_errors: AtomicU64::new(0),
            tx_dropped: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> net::NetStats {
        net::NetStats {
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            rx_errors: self.rx_errors.load(Ordering::Relaxed),
            rx_dropped: self.rx_dropped.load(Ordering::Relaxed),
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            tx_errors: self.tx_errors.load(Ordering::Relaxed),
            tx_dropped: self.tx_dropped.load(Ordering::Relaxed),
        }
    }
}

// ── MMIO 原子访问助手 ─────────────────────────────────────────────────

#[inline]
fn rd_u8(addr: usize) -> u8 {
    unsafe { read_volatile(addr as *const u8) }
}
#[inline]
fn rd_u16(addr: usize) -> u16 {
    unsafe { read_volatile(addr as *const u16) }
}

// ── 队列设置助手 ──────────────────────────────────────────────────────

fn setup_queue(
    transport: &VirtioPciTransport,
    queue_idx: u16,
) -> Result<VirtioNetQueue, &'static str> {
    transport.select_queue(queue_idx);
    let max_qsz = transport.selected_queue_size();
    if max_qsz == 0 {
        return Err("virtio-net: queue size is zero");
    }
    let qsz = choose_split_queue_size(max_qsz, Some(VIRTIO_NET_QUEUE_LIMIT))
        .map_err(|_| "virtio-net: invalid queue size")?;
    if usize::from(qsz) > VIRTIO_NET_QUEUE_LIMIT_USIZE {
        return Err("virtio-net: queue size exceeds local queue storage");
    }
    transport.set_selected_queue_size(qsz);

    let desc_dma = DmaBuffer::page(DmaDirection::ToDevice)?;
    let avail_dma = DmaBuffer::page(DmaDirection::ToDevice)?;
    let used_dma = DmaBuffer::page(DmaDirection::FromDevice)?;
    let desc_table = desc_dma.vaddr() as *mut VirtqDesc;
    let avail_ring = avail_dma.vaddr() as *mut VirtqAvail;
    let used_ring = used_dma.vaddr() as *mut VirtqUsed;
    desc_dma.sync_for_device();
    avail_dma.sync_for_device();

    transport.set_selected_queue_addresses(
        desc_dma.dma_addr() as u64,
        avail_dma.dma_addr() as u64,
        used_dma.dma_addr() as u64,
    );

    let notify_addr = transport
        .selected_queue_notify_addr()
        .map_err(|_| "virtio-net: notify address invalid")?;
    transport.enable_selected_queue();

    Ok(VirtioNetQueue {
        desc_dma,
        avail_dma,
        used_dma,
        desc_table,
        avail_ring,
        used_ring,
        queue_size: qsz,
        last_used_idx: 0,
        notify_addr,
    })
}

// ── Probe ─────────────────────────────────────────────────────────────

impl VirtioNetPci {
    pub fn probe(pci: &PciDevice) -> Result<Self, &'static str> {
        pci.try_enable_mmio()
            .map_err(|_| "virtio-net: failed to enable MMIO decode")?;
        pci.try_enable_bus_master()
            .map_err(|_| "virtio-net: failed to enable bus master")?;

        let raw_caps = parse_virtio_pci_caps(pci).ok_or("virtio-net: missing caps")?;
        let transport =
            VirtioPciTransport::new(raw_caps).map_err(|_| "virtio-net: invalid caps")?;
        let caps = transport.caps();

        // Reset
        if !transport.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT) {
            return Err("virtio-net: reset timeout");
        }

        // ACKNOWLEDGE + DRIVER
        transport.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
        transport.add_status(VIRTIO_STATUS_DRIVER);

        // Feature negotiation
        let dev_features = transport.device_features();
        if dev_features & VIRTIO_F_VERSION_1 == 0 {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-net: lacks VERSION_1");
        }
        let mut drv_features = VIRTIO_F_VERSION_1;
        if dev_features & VIRTIO_NET_F_MAC != 0 {
            drv_features |= VIRTIO_NET_F_MAC;
        }
        if dev_features & VIRTIO_NET_F_STATUS != 0 {
            drv_features |= VIRTIO_NET_F_STATUS;
        }
        transport.set_driver_features(drv_features);
        transport.add_status(VIRTIO_STATUS_FEATURES_OK);
        if transport.status() & VIRTIO_STATUS_FEATURES_OK == 0 {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-net: FEATURES_OK rejected");
        }

        // 读取设备配置区里的 MAC 地址。
        let mut mac = [0u8; VIRTIO_NET_MAC_LEN];
        if let Some(dev_cfg) = caps.device
            && dev_cfg.covers(NET_CFG_MAC, mac.len())
        {
            for i in 0..VIRTIO_NET_MAC_LEN {
                mac[i] = rd_u8(dev_cfg.vaddr + NET_CFG_MAC + i);
            }
        }

        // 建立 RX/TX 两个 virtqueue。
        let rx_queue = setup_queue(&transport, RX_QUEUE)?;
        let tx_queue = setup_queue(&transport, TX_QUEUE)?;

        // 预分配 RX DMA 缓冲区，并把设备可写描述符填入 RX 队列。
        let mut rx_buffers = Vec::with_capacity(rx_queue.queue_size as usize);
        for i in 0..rx_queue.queue_size {
            let buf = DmaBuffer::page(DmaDirection::FromDevice)?;
            let buf_dma = buf.dma_addr() as u64;
            buf.sync_for_device();
            rx_buffers.push(buf);
            unsafe {
                let desc = &mut *rx_queue.desc_table.add(i as usize);
                desc.addr = buf_dma;
                desc.len = RX_BUF_SIZE as u32;
                desc.flags = VIRTQ_DESC_F_WRITE;
                desc.next = 0;
            }
            // 把刚填好的描述符发布到 available ring。
            unsafe {
                let avail = &mut *rx_queue.avail_ring;
                avail.ring[i as usize] = i;
            }
        }
        // 更新 available idx，使设备能看到整批 RX 缓冲区。
        unsafe {
            fence(Ordering::Release);
            (*rx_queue.avail_ring).idx = rx_queue.queue_size;
        }
        rx_queue.desc_dma.sync_for_device();
        rx_queue.avail_dma.sync_for_device();
        // 通知设备可以开始使用 RX 缓冲区。
        transport.notify_queue(rx_queue.notify_addr, RX_QUEUE);

        // TX 队列的空闲描述符池。
        let mut tx_free_descs = VecDeque::with_capacity(tx_queue.queue_size as usize);
        for i in (0..tx_queue.queue_size).rev() {
            tx_free_descs.push_back(i);
        }

        // 完成设备初始化握手。
        transport.add_status(VIRTIO_STATUS_DRIVER_OK);

        log::printk!(
            "[virtio-net] probe ok: mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );

        let has_status = drv_features & VIRTIO_NET_F_STATUS != 0;

        Ok(Self {
            inner: Mutex::new(VirtioNetInner {
                transport,
                rx_queue,
                tx_queue,
                rx_buffers,
                tx_free_descs,
                pending_tx: VecDeque::new(),
            }),
            mac,
            has_status,
            stats: VirtioNetStats::new(),
        })
    }
}

// ── NetDriver 实现 ────────────────────────────────────────────────────

impl net::NetDriver for VirtioNetPci {
    fn poll_rx(&self) -> Option<net::RxBuf> {
        let mut inner = self.inner.lock();
        self.reclaim_tx(&mut inner);
        let rq = &mut inner.rx_queue;
        rq.used_dma.sync_for_cpu();
        fence(Ordering::Acquire);
        let used_idx = unsafe { read_volatile(&(*rq.used_ring).idx) };
        if rq.last_used_idx == used_idx {
            return None;
        }
        let elem =
            unsafe { (*rq.used_ring).ring[rq.last_used_idx as usize % rq.queue_size as usize] };
        rq.last_used_idx = rq.last_used_idx.wrapping_add(1);

        let desc_idx = elem.id as usize;
        if desc_idx >= rq.queue_size as usize || desc_idx >= inner.rx_buffers.len() {
            self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let total_len = (elem.len as usize).min(RX_BUF_SIZE);
        if total_len <= VIRTIO_NET_HDR_SIZE {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            Self::repost_rx_buffer(&mut inner, desc_idx);
            return None;
        }

        let rx_buf = &inner.rx_buffers[desc_idx];
        rx_buf.sync_for_cpu();
        let buf_vaddr = rx_buf.vaddr();
        let frame_len = total_len - VIRTIO_NET_HDR_SIZE;
        let mut frame = alloc::vec![0u8; frame_len].into_boxed_slice();
        unsafe {
            core::ptr::copy_nonoverlapping(
                (buf_vaddr + VIRTIO_NET_HDR_SIZE) as *const u8,
                frame.as_mut_ptr(),
                frame_len,
            );
        }

        Self::repost_rx_buffer(&mut inner, desc_idx);
        self.stats.rx_packets.fetch_add(1, Ordering::Relaxed);
        self.stats
            .rx_bytes
            .fetch_add(frame_len as u64, Ordering::Relaxed);
        Some(net::RxBuf::new(frame, frame_len))
    }

    fn alloc_tx(&self, len: usize) -> Option<net::TxBuf> {
        if len > ETHERNET_MAX_FRAME_LEN {
            self.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let inner = self.inner.lock();
        if inner.tx_free_descs.is_empty() {
            self.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let buf = alloc::vec![0u8; len].into_boxed_slice();
        Some(net::TxBuf::new(buf))
    }

    fn commit_tx(&self, buf: net::TxBuf) {
        let mut inner = self.inner.lock();
        let transport = inner.transport;
        let Some(desc_idx) = inner.tx_free_descs.pop_front() else {
            return;
        };

        // 分配 DMA buffer 并写入 virtio_net_hdr + frame
        let frame_data = buf.as_slice();
        let total_len = VIRTIO_NET_HDR_SIZE + frame_data.len();
        let tx_buf = match DmaBuffer::page(DmaDirection::ToDevice) {
            Ok(a) => a,
            Err(_) => {
                inner.tx_free_descs.push_front(desc_idx);
                self.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let tx_vaddr = tx_buf.vaddr();
        unsafe {
            // 写入默认 virtio-net header；所有 offload 功能未协商时字段必须为 0。
            debug_assert_eq!(core::mem::size_of::<VirtioNetHdr>(), VIRTIO_NET_HDR_SIZE);
            let hdr = VirtioNetHdr::default();
            core::ptr::copy_nonoverlapping(
                (&hdr as *const VirtioNetHdr).cast::<u8>(),
                tx_vaddr as *mut u8,
                VIRTIO_NET_HDR_SIZE,
            );
            // 拷贝帧数据
            core::ptr::copy_nonoverlapping(
                frame_data.as_ptr(),
                (tx_vaddr + VIRTIO_NET_HDR_SIZE) as *mut u8,
                frame_data.len(),
            );
        }
        tx_buf.sync_for_device();

        let tq = &mut inner.tx_queue;
        let tx_dma = tx_buf.dma_addr() as u64;

        // 写入设备可读的 TX 描述符。
        unsafe {
            let desc = &mut *tq.desc_table.add(desc_idx as usize);
            desc.addr = tx_dma;
            desc.len = total_len as u32;
            desc.flags = 0; // 设备只读。
            desc.next = 0;
        }
        tq.desc_dma.sync_for_device();

        // 添加到 available ring。
        unsafe {
            let avail = &mut *tq.avail_ring;
            let avail_idx = avail.idx;
            avail.ring[avail_idx as usize % tq.queue_size as usize] = desc_idx;
            fence(Ordering::Release);
            avail.idx = avail_idx.wrapping_add(1);
        }
        tq.avail_dma.sync_for_device();
        let notify_addr = tq.notify_addr;

        // 延迟回收：先登记 pending，再通知设备，避免设备快速完成时 used ring
        // 先于 pending_tx 可见。
        inner.pending_tx.push_back((desc_idx, tx_buf));
        self.stats.tx_packets.fetch_add(1, Ordering::Relaxed);
        self.stats
            .tx_bytes
            .fetch_add(frame_data.len() as u64, Ordering::Relaxed);
        transport.notify_queue(notify_addr, TX_QUEUE);
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn link_state(&self) -> net::LinkState {
        let inner = self.inner.lock();
        if self.has_status {
            if let Some(dev_cfg) = inner.transport.caps().device
                && dev_cfg.covers(NET_CFG_STATUS, mem::size_of::<u16>())
            {
                let status = rd_u16(dev_cfg.vaddr + NET_CFG_STATUS);
                if status & VIRTIO_NET_STATUS_LINK_UP != 0 {
                    return net::LinkState::Up {
                        speed_mbps: None,
                        duplex: net::Duplex::Full,
                    };
                }
                return net::LinkState::Down;
            }
        }
        net::LinkState::Up {
            speed_mbps: None,
            duplex: net::Duplex::Full,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn stats(&self) -> net::NetStats {
        self.stats.snapshot()
    }
}

impl VirtioNetPci {
    fn repost_rx_buffer(inner: &mut VirtioNetInner, desc_idx: usize) {
        let transport = inner.transport;
        let Some(buf) = inner.rx_buffers.get(desc_idx) else {
            return;
        };
        buf.sync_for_device();
        let rq = &mut inner.rx_queue;
        unsafe {
            let avail = &mut *rq.avail_ring;
            let avail_idx = avail.idx;
            avail.ring[avail_idx as usize % rq.queue_size as usize] = desc_idx as u16;
            fence(Ordering::Release);
            avail.idx = avail_idx.wrapping_add(1);
        }
        rq.avail_dma.sync_for_device();
        let notify_addr = rq.notify_addr;
        transport.notify_queue(notify_addr, RX_QUEUE);
    }

    fn reclaim_tx(&self, inner: &mut VirtioNetInner) {
        let tq = &mut inner.tx_queue;
        loop {
            tq.used_dma.sync_for_cpu();
            fence(Ordering::Acquire);
            let used_idx = unsafe { (*tq.used_ring).idx };
            if tq.last_used_idx == used_idx {
                break;
            }
            let elem =
                unsafe { (*tq.used_ring).ring[tq.last_used_idx as usize % tq.queue_size as usize] };
            tq.last_used_idx = tq.last_used_idx.wrapping_add(1);
            let desc_idx = elem.id as u16;
            if desc_idx >= tq.queue_size {
                self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // 在 pending_tx 中找到并释放对应的 DMA buffer
            if let Some(pos) = inner.pending_tx.iter().position(|(d, _)| *d == desc_idx) {
                let _ = inner.pending_tx.remove(pos);
                inner.tx_free_descs.push_back(desc_idx);
            } else {
                self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn handle_interrupt(&self) -> bool {
        let mut inner = self.inner.lock();
        let isr_status = inner.transport.isr_status();
        if isr_status == 0 {
            return false;
        }
        // 读 ISR capability 同时完成设备侧 ack。这里先回收 TX 描述符，
        // RX 数据由 net stack 的 poll_interface 路径拉取并重新投递 buffer。
        self.reclaim_tx(&mut inner);
        true
    }
}

impl Drop for VirtioNetPci {
    fn drop(&mut self) {
        let inner = self.inner.lock();
        inner.transport.set_status(0);
    }
}

// ── PnP 驱动绑定 ──────────────────────────────────────────────────────

pub struct VirtioNetPciDriver {}

struct VirtioNetPciBinding {
    iface_id: net::InterfaceId,
    net_dev: Arc<net::NetDevice>,
    irq_handle: Option<IrqHandle>,
    _driver: Arc<VirtioNetPci>,
}

impl VirtioNetPciDriver {
    pub const fn new() -> Self {
        Self {}
    }
}

struct VirtioNetPciIrqHandler {
    driver: Arc<VirtioNetPci>,
    iface_id: net::InterfaceId,
}

impl IrqHandler for VirtioNetPciIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if !self.driver.handle_interrupt() {
            return IrqStatus::Unhandled;
        }
        let millis = (sched::now_ns_public() / 1_000_000) as i64;
        net::stack().poll_interface_ms(self.iface_id, millis);
        IrqStatus::Handled
    }
}

fn map_irq_error(err: IrqError) -> &'static str {
    match err {
        IrqError::OutOfMemory => "out of memory",
        IrqError::NotFound => "not found",
        IrqError::AlreadyRegistered => "already registered",
    }
}

fn register_virtio_net_irq(
    pci: &PciDevice,
    driver: Arc<VirtioNetPci>,
    iface_id: net::InterfaceId,
) -> Option<IrqHandle> {
    let Some(line) = pci.routed_irq_line() else {
        pci.disable_interrupts();
        return None;
    };
    let handler: Arc<dyn IrqHandler> = Arc::new(VirtioNetPciIrqHandler { driver, iface_id });
    match irq::register_irq_handler(line, handler) {
        Ok(handle) => {
            pci.enable_interrupts();
            Some(handle)
        }
        Err(err) => {
            log::printk!(
                "[virtio-net] failed to register irq {:?}: {}",
                line,
                map_irq_error(err)
            );
            pci.disable_interrupts();
            None
        }
    }
}

impl PnpDriver for VirtioNetPciDriver {
    fn name(&self) -> &'static str {
        "virtio-pci-net"
    }

    fn bus_type(&self) -> BusType {
        BusType::PCI
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        let PnpId::Pci { .. } = id else { return false };
        let Some(pci_info) = info.as_any().downcast_ref::<PciInfo>() else {
            return false;
        };
        VIRTIO_PCI_FUNCTION_NETWORK.matches_pci_ids(pci_info.vendor, pci_info.device_id)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(dev).ok_or(PnpError::InvalidState)?;

        let driver = VirtioNetPci::probe(&pci).map_err(|msg| {
            log::printk!("[virtio-net] probe failed: {}", msg);
            PnpError::ProbeFailed
        })?;

        let name = NET_IFACE_NAMES.try_alloc_stable(&dev.name)?.into_string();
        let mac = driver.mac;
        let driver = Arc::new(driver);
        let net_dev = Arc::new(net::NetDevice::new(&name, driver.clone()));
        let config = net::IfConfig::auto();
        net::stack()
            .attach(Arc::clone(&net_dev), config)
            .map_err(|_| PnpError::ProbeFailed)?;
        let irq_handle = register_virtio_net_irq(&pci, Arc::clone(&driver), net_dev.id());
        if let Err(err) =
            dev.register_function(Arc::new(NetFunction::new(&name, Arc::clone(&net_dev))))
        {
            if let Some(handle) = irq_handle {
                let _ = irq::unregister_irq_handler(handle);
            }
            pci.disable_interrupts();
            net_dev.mark_gone();
            let _ = net::stack().detach(net_dev.id());
            return Err(err);
        }
        dev.set_driver_data(Arc::new(VirtioNetPciBinding {
            iface_id: net_dev.id(),
            net_dev: Arc::clone(&net_dev),
            irq_handle,
            _driver: Arc::clone(&driver),
        }));

        log::printk!(
            "[virtio-net] attached {} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            name,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data() {
            if let Ok(binding) = data.downcast::<VirtioNetPciBinding>() {
                if let Some(handle) = binding.irq_handle {
                    let _ = irq::unregister_irq_handler(handle);
                }
                binding.net_dev.mark_gone();
                let _ = net::stack().detach(binding.iface_id);
            }
        }
        log::printk!("[virtio-net] remove {}", dev.id);
    }
}

struct VirtioNetPciFactory;

impl DriverFactory for VirtioNetPciFactory {
    fn name(&self) -> &'static str {
        "virtio-pci-net"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioNetPciDriver::new()))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(VirtioNetPciFactory)).map(|_| ())
}
