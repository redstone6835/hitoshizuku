//! VirtIO-Net PCI 驱动。
//!
//! 通过 PCI capability chain 定位 common/notify/device config 区域，
//! 使用两个 virtqueue（RX queue 0 + TX queue 1）实现以太网帧收发。
//! 实现 `net::NetDriver` trait，可直接接入 smoltcp 协议栈。

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use allocator::{KERNEL_ALLOCATOR, PAGE_SIZE, PhysicalAllocRequest, PhysicalAllocation};
use core::any::Any;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering, fence};

use spin::Mutex;

use crate::dev::net::NetFunction;
use crate::dev::pci::{PciDevice, PciInfo};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};

static NEXT_ETH_INDEX: AtomicUsize = AtomicUsize::new(0);

// ── VirtIO PCI capability 类型 ──────────────────────────────────────────

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// ── common_cfg 寄存器偏移 ────────────────────────────────────────────────

const CC_DEVICE_FEATURE_SELECT: usize = 0x00;
const CC_DEVICE_FEATURE: usize = 0x04;
const CC_DRIVER_FEATURE_SELECT: usize = 0x08;
const CC_DRIVER_FEATURE: usize = 0x0c;
const CC_DEVICE_STATUS: usize = 0x14;
const CC_QUEUE_SELECT: usize = 0x16;
const CC_QUEUE_SIZE: usize = 0x18;
const CC_QUEUE_ENABLE: usize = 0x1c;
const CC_QUEUE_NOTIFY_OFF: usize = 0x1e;
const CC_QUEUE_DESC: usize = 0x20;
const CC_QUEUE_DRIVER: usize = 0x28;
const CC_QUEUE_DEVICE: usize = 0x30;

// ── device status bits ─────────────────────────────────────────────────

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;

// ── VirtIO-Net feature bits ────────────────────────────────────────────

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// ── VirtIO-Net device config offsets ───────────────────────────────────

const NET_CFG_MAC: usize = 0x00;
#[allow(dead_code)]
const NET_CFG_STATUS: usize = 0x06;

// ── Virtqueue 常量 ─────────────────────────────────────────────────────

const QUEUE_SIZE: u16 = 128;
const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_NET_HDR_SIZE: usize = 12;
const RX_BUF_SIZE: usize = 1526; // ETH_FRAME_LEN + virtio_net_hdr

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
    ring: [u16; QUEUE_SIZE as usize],
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
    ring: [VirtqUsedElem; QUEUE_SIZE as usize],
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

// ── Capability 信息 ────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct VirtioCap {
    vaddr: usize,
    #[allow(dead_code)]
    length: u32,
    notify_off_multiplier: u32,
}

struct VirtioPciCaps {
    common: VirtioCap,
    notify: VirtioCap,
    device: Option<VirtioCap>,
}

// ── 队列状态 ───────────────────────────────────────────────────────────

struct VirtioNetQueue {
    desc_alloc: Option<PhysicalAllocation>,
    avail_alloc: Option<PhysicalAllocation>,
    used_alloc: Option<PhysicalAllocation>,
    desc_table: *mut VirtqDesc,
    avail_ring: *mut VirtqAvail,
    used_ring: *mut VirtqUsed,
    queue_size: u16,
    last_used_idx: u16,
    notify_addr: usize,
}

unsafe impl Send for VirtioNetQueue {}
unsafe impl Sync for VirtioNetQueue {}

impl Drop for VirtioNetQueue {
    fn drop(&mut self) {
        if let Some(a) = self.desc_alloc.take() {
            let _ = KERNEL_ALLOCATOR.free_physical(a);
        }
        if let Some(a) = self.avail_alloc.take() {
            let _ = KERNEL_ALLOCATOR.free_physical(a);
        }
        if let Some(a) = self.used_alloc.take() {
            let _ = KERNEL_ALLOCATOR.free_physical(a);
        }
    }
}

// ── RX 缓冲区管理 ─────────────────────────────────────────────────────

struct RxBufferPool {
    buffers: Vec<PhysicalAllocation>,
}

// ── 驱动主结构 ─────────────────────────────────────────────────────────

struct VirtioNetInner {
    caps: VirtioPciCaps,
    rx_queue: VirtioNetQueue,
    tx_queue: VirtioNetQueue,
    rx_buffers: Vec<PhysicalAllocation>,
    tx_free_descs: VecDeque<u16>,
    pending_tx: VecDeque<(u16, PhysicalAllocation)>,
    virt_to_phys: fn(usize) -> usize,
}

unsafe impl Send for VirtioNetInner {}
unsafe impl Sync for VirtioNetInner {}

pub struct VirtioNetPci {
    inner: Mutex<VirtioNetInner>,
    mac: [u8; 6],
    has_status: bool,
}

// ── MMIO 原子访问助手 ─────────────────────────────────────────────────

#[inline]
fn rd_u8(addr: usize) -> u8 {
    unsafe { read_volatile(addr as *const u8) }
}
#[inline]
fn wr_u8(addr: usize, v: u8) {
    unsafe { write_volatile(addr as *mut u8, v) }
}
#[inline]
fn rd_u16(addr: usize) -> u16 {
    unsafe { read_volatile(addr as *const u16) }
}
#[inline]
fn wr_u16(addr: usize, v: u16) {
    unsafe { write_volatile(addr as *mut u16, v) }
}
#[inline]
fn rd_u32(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}
#[inline]
fn wr_u32(addr: usize, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}
#[inline]
fn wr_u64(addr: usize, v: u64) {
    wr_u32(addr, v as u32);
    wr_u32(addr + 4, (v >> 32) as u32);
}

// ── capability 解析 ────────────────────────────────────────────────────

fn parse_virtio_caps(pci: &PciDevice) -> Option<VirtioPciCaps> {
    let mut common: Option<VirtioCap> = None;
    let mut notify: Option<VirtioCap> = None;
    let mut isr: Option<VirtioCap> = None;
    let mut device: Option<VirtioCap> = None;

    let mut ptr = pci.capabilities_offset()?;
    let mut hops = 0u32;
    while ptr != 0 && hops < 64 {
        let cap_id = pci.read_config_u8(ptr);
        let next = pci.read_config_u8(ptr + 1) as u16 & 0xFC;
        if cap_id == 0x09 {
            let cfg_type = pci.read_config_u8(ptr + 3);
            let bar_idx = pci.read_config_u8(ptr + 4) & 0x7;
            let off_lo = pci.read_config_u16(ptr + 8) as u32;
            let off_hi = pci.read_config_u16(ptr + 10) as u32;
            let offset = off_lo | (off_hi << 16);
            let len_lo = pci.read_config_u16(ptr + 12) as u32;
            let len_hi = pci.read_config_u16(ptr + 14) as u32;
            let length = len_lo | (len_hi << 16);

            if let Some((_bar, bar_vaddr)) = pci.map_bar_virt(bar_idx as usize) {
                let vaddr = bar_vaddr.wrapping_add(offset as usize);
                let cap = VirtioCap {
                    vaddr,
                    length,
                    notify_off_multiplier: 0,
                };
                match cfg_type {
                    VIRTIO_PCI_CAP_COMMON_CFG => common = Some(cap),
                    VIRTIO_PCI_CAP_NOTIFY_CFG => {
                        let mult = pci.read_config_u32(ptr + 16);
                        notify = Some(VirtioCap {
                            vaddr,
                            length,
                            notify_off_multiplier: mult,
                        });
                    }
                    VIRTIO_PCI_CAP_ISR_CFG => isr = Some(cap),
                    VIRTIO_PCI_CAP_DEVICE_CFG => device = Some(cap),
                    _ => {}
                }
            }
        }
        ptr = next;
        hops += 1;
    }

    let _ = isr?;
    Some(VirtioPciCaps {
        common: common?,
        notify: notify?,
        device,
    })
}

// ── DMA 分配助手 ──────────────────────────────────────────────────────

fn alloc_dma_page() -> Result<PhysicalAllocation, &'static str> {
    KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .map_err(|_| "virtio-net: DMA page alloc failed")
}

fn dma_vaddr(alloc: PhysicalAllocation) -> Result<usize, &'static str> {
    allocator::KERNEL_ALLOCATOR
        .load_phys_to_virt()
        .map(|phys_to_virt| phys_to_virt(alloc.paddr))
        .ok_or("virtio-net: phys_to_virt hook is not installed")
}

// ── 状态助手 ──────────────────────────────────────────────────────────

fn cc_status(caps: &VirtioPciCaps) -> u8 {
    rd_u8(caps.common.vaddr + CC_DEVICE_STATUS)
}
fn cc_set_status(caps: &VirtioPciCaps, v: u8) {
    wr_u8(caps.common.vaddr + CC_DEVICE_STATUS, v);
}
fn cc_add_status(caps: &VirtioPciCaps, bit: u8) {
    cc_set_status(caps, cc_status(caps) | bit);
}
fn cc_device_features(caps: &VirtioPciCaps) -> u64 {
    wr_u32(caps.common.vaddr + CC_DEVICE_FEATURE_SELECT, 0);
    let lo = rd_u32(caps.common.vaddr + CC_DEVICE_FEATURE) as u64;
    wr_u32(caps.common.vaddr + CC_DEVICE_FEATURE_SELECT, 1);
    let hi = rd_u32(caps.common.vaddr + CC_DEVICE_FEATURE) as u64;
    (hi << 32) | lo
}
fn cc_set_driver_features(caps: &VirtioPciCaps, f: u64) {
    wr_u32(caps.common.vaddr + CC_DRIVER_FEATURE_SELECT, 0);
    wr_u32(caps.common.vaddr + CC_DRIVER_FEATURE, f as u32);
    wr_u32(caps.common.vaddr + CC_DRIVER_FEATURE_SELECT, 1);
    wr_u32(caps.common.vaddr + CC_DRIVER_FEATURE, (f >> 32) as u32);
}

// ── 队列设置助手 ──────────────────────────────────────────────────────

fn setup_queue(caps: &VirtioPciCaps, queue_idx: u16) -> Result<VirtioNetQueue, &'static str> {
    wr_u16(caps.common.vaddr + CC_QUEUE_SELECT, queue_idx);
    let max_qsz = rd_u16(caps.common.vaddr + CC_QUEUE_SIZE);
    if max_qsz == 0 {
        return Err("virtio-net: queue size is zero");
    }
    let qsz = max_qsz.min(QUEUE_SIZE);
    wr_u16(caps.common.vaddr + CC_QUEUE_SIZE, qsz);

    let desc_alloc = alloc_dma_page()?;
    let avail_alloc = alloc_dma_page()?;
    let used_alloc = alloc_dma_page()?;
    let desc_table = dma_vaddr(desc_alloc)? as *mut VirtqDesc;
    let avail_ring = dma_vaddr(avail_alloc)? as *mut VirtqAvail;
    let used_ring = dma_vaddr(used_alloc)? as *mut VirtqUsed;
    unsafe {
        core::ptr::write_bytes(desc_table.cast::<u8>(), 0, PAGE_SIZE);
        core::ptr::write_bytes(avail_ring.cast::<u8>(), 0, PAGE_SIZE);
        core::ptr::write_bytes(used_ring.cast::<u8>(), 0, PAGE_SIZE);
    }

    wr_u64(caps.common.vaddr + CC_QUEUE_DESC, desc_alloc.paddr as u64);
    wr_u64(
        caps.common.vaddr + CC_QUEUE_DRIVER,
        avail_alloc.paddr as u64,
    );
    wr_u64(caps.common.vaddr + CC_QUEUE_DEVICE, used_alloc.paddr as u64);

    let notify_off = rd_u16(caps.common.vaddr + CC_QUEUE_NOTIFY_OFF) as usize;
    let notify_addr = caps.notify.vaddr + notify_off * caps.notify.notify_off_multiplier as usize;

    wr_u16(caps.common.vaddr + CC_QUEUE_ENABLE, 1);

    Ok(VirtioNetQueue {
        desc_alloc: Some(desc_alloc),
        avail_alloc: Some(avail_alloc),
        used_alloc: Some(used_alloc),
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
    pub fn probe(pci: &PciDevice, virt_to_phys: fn(usize) -> usize) -> Result<Self, &'static str> {
        pci.enable_mmio();
        pci.enable_bus_master();

        let caps = parse_virtio_caps(pci).ok_or("virtio-net: missing caps")?;

        // Reset
        cc_set_status(&caps, 0);
        let mut spin_cnt = 0u32;
        while cc_status(&caps) != 0 {
            core::hint::spin_loop();
            spin_cnt += 1;
            if spin_cnt >= 1_000_000 {
                return Err("virtio-net: reset timeout");
            }
        }

        // ACKNOWLEDGE + DRIVER
        cc_add_status(&caps, STATUS_ACKNOWLEDGE);
        cc_add_status(&caps, STATUS_DRIVER);

        // Feature negotiation
        let dev_features = cc_device_features(&caps);
        if dev_features & VIRTIO_F_VERSION_1 == 0 {
            cc_set_status(&caps, STATUS_FAILED);
            return Err("virtio-net: lacks VERSION_1");
        }
        let mut drv_features = VIRTIO_F_VERSION_1;
        if dev_features & VIRTIO_NET_F_MAC != 0 {
            drv_features |= VIRTIO_NET_F_MAC;
        }
        if dev_features & VIRTIO_NET_F_STATUS != 0 {
            drv_features |= VIRTIO_NET_F_STATUS;
        }
        cc_set_driver_features(&caps, drv_features);
        cc_add_status(&caps, STATUS_FEATURES_OK);
        if cc_status(&caps) & STATUS_FEATURES_OK == 0 {
            cc_set_status(&caps, STATUS_FAILED);
            return Err("virtio-net: FEATURES_OK rejected");
        }

        // Read MAC address
        let mut mac = [0u8; 6];
        if let Some(dev_cfg) = caps.device {
            for i in 0..6 {
                mac[i] = rd_u8(dev_cfg.vaddr + NET_CFG_MAC + i);
            }
        }

        // Setup RX and TX queues
        let rx_queue = setup_queue(&caps, RX_QUEUE)?;
        let tx_queue = setup_queue(&caps, TX_QUEUE)?;

        // Pre-allocate RX buffers and fill the RX queue
        let mut rx_buffers = Vec::with_capacity(rx_queue.queue_size as usize);
        for i in 0..rx_queue.queue_size {
            let buf_alloc = alloc_dma_page()?;
            let buf_paddr = buf_alloc.paddr as u64;
            rx_buffers.push(buf_alloc);
            // Write descriptor: device-writable buffer
            unsafe {
                let desc = &mut *rx_queue.desc_table.add(i as usize);
                desc.addr = buf_paddr;
                desc.len = RX_BUF_SIZE as u32;
                desc.flags = VIRTQ_DESC_F_WRITE;
                desc.next = 0;
            }
            // Add to available ring
            unsafe {
                let avail = &mut *rx_queue.avail_ring;
                avail.ring[i as usize] = i;
            }
        }
        // Update avail idx
        unsafe {
            fence(Ordering::Release);
            (*rx_queue.avail_ring).idx = rx_queue.queue_size;
        }
        // Notify device of RX buffers
        wr_u16(rx_queue.notify_addr, RX_QUEUE);

        // TX free descriptors
        let mut tx_free_descs = VecDeque::with_capacity(tx_queue.queue_size as usize);
        for i in (0..tx_queue.queue_size).rev() {
            tx_free_descs.push_back(i);
        }

        // DRIVER_OK
        cc_add_status(&caps, STATUS_DRIVER_OK);

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
                caps,
                rx_queue,
                tx_queue,
                rx_buffers,
                tx_free_descs,
                pending_tx: VecDeque::new(),
                virt_to_phys,
            }),
            mac,
            has_status,
        })
    }
}

// ── NetDriver 实现 ────────────────────────────────────────────────────

impl net::NetDriver for VirtioNetPci {
    fn poll_rx(&self) -> Option<net::RxBuf> {
        let mut inner = self.inner.lock();
        Self::reclaim_tx(&mut inner);
        let rq = &mut inner.rx_queue;
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
            return None;
        }
        let total_len = (elem.len as usize).min(RX_BUF_SIZE);
        if total_len <= VIRTIO_NET_HDR_SIZE {
            Self::repost_rx_buffer(&mut inner, desc_idx);
            return None;
        }

        let buf_vaddr = match dma_vaddr(inner.rx_buffers[desc_idx]) {
            Ok(vaddr) => vaddr,
            Err(_) => {
                Self::repost_rx_buffer(&mut inner, desc_idx);
                return None;
            }
        };
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
        Some(net::RxBuf::new(frame, frame_len))
    }

    fn alloc_tx(&self, len: usize) -> Option<net::TxBuf> {
        if len > 1514 {
            return None;
        }
        let inner = self.inner.lock();
        if inner.tx_free_descs.is_empty() {
            return None;
        }
        let buf = alloc::vec![0u8; len].into_boxed_slice();
        Some(net::TxBuf::new(buf))
    }

    fn commit_tx(&self, buf: net::TxBuf) {
        let mut inner = self.inner.lock();
        let Some(desc_idx) = inner.tx_free_descs.pop_front() else {
            return;
        };

        // 分配 DMA buffer 并写入 virtio_net_hdr + frame
        let frame_data = buf.as_slice();
        let total_len = VIRTIO_NET_HDR_SIZE + frame_data.len();
        let tx_buf_alloc = match alloc_dma_page() {
            Ok(a) => a,
            Err(_) => {
                inner.tx_free_descs.push_front(desc_idx);
                return;
            }
        };
        let tx_vaddr = match dma_vaddr(tx_buf_alloc) {
            Ok(vaddr) => vaddr,
            Err(_) => {
                inner.tx_free_descs.push_front(desc_idx);
                let _ = KERNEL_ALLOCATOR.free_physical(tx_buf_alloc);
                return;
            }
        };
        unsafe {
            // 清零 virtio_net_hdr
            core::ptr::write_bytes(tx_vaddr as *mut u8, 0, VIRTIO_NET_HDR_SIZE);
            // 拷贝帧数据
            core::ptr::copy_nonoverlapping(
                frame_data.as_ptr(),
                (tx_vaddr + VIRTIO_NET_HDR_SIZE) as *mut u8,
                frame_data.len(),
            );
        }

        let tq = &mut inner.tx_queue;
        let paddr = tx_buf_alloc.paddr as u64;

        // 写描述符
        unsafe {
            let desc = &mut *tq.desc_table.add(desc_idx as usize);
            desc.addr = paddr;
            desc.len = total_len as u32;
            desc.flags = 0; // device-readable
            desc.next = 0;
        }

        // 添加到 available ring
        unsafe {
            let avail = &mut *tq.avail_ring;
            let avail_idx = avail.idx;
            avail.ring[avail_idx as usize % tq.queue_size as usize] = desc_idx;
            fence(Ordering::Release);
            avail.idx = avail_idx.wrapping_add(1);
        }

        // 通知设备
        wr_u16(tq.notify_addr, TX_QUEUE);

        // 延迟回收：把 pending TX 入队，等 used ring 通知后再释放
        inner.pending_tx.push_back((desc_idx, tx_buf_alloc));
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn link_state(&self) -> net::LinkState {
        let inner = self.inner.lock();
        if self.has_status {
            if let Some(dev_cfg) = inner.caps.device {
                let status = rd_u16(dev_cfg.vaddr + NET_CFG_STATUS);
                if status & 1 != 0 {
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
}

impl VirtioNetPci {
    fn repost_rx_buffer(inner: &mut VirtioNetInner, desc_idx: usize) {
        let rq = &mut inner.rx_queue;
        unsafe {
            let avail = &mut *rq.avail_ring;
            let avail_idx = avail.idx;
            avail.ring[avail_idx as usize % rq.queue_size as usize] = desc_idx as u16;
            fence(Ordering::Release);
            avail.idx = avail_idx.wrapping_add(1);
        }
        wr_u16(rq.notify_addr, RX_QUEUE);
    }

    fn reclaim_tx(inner: &mut VirtioNetInner) {
        let tq = &mut inner.tx_queue;
        loop {
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
                continue;
            }
            // 在 pending_tx 中找到并释放对应的 DMA buffer
            if let Some(pos) = inner.pending_tx.iter().position(|(d, _)| *d == desc_idx) {
                if let Some((_, alloc)) = inner.pending_tx.remove(pos) {
                    let _ = KERNEL_ALLOCATOR.free_physical(alloc);
                }
                inner.tx_free_descs.push_back(desc_idx);
            }
        }
    }
}

impl Drop for VirtioNetPci {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        cc_set_status(&inner.caps, 0);
        for alloc in inner.rx_buffers.drain(..) {
            let _ = KERNEL_ALLOCATOR.free_physical(alloc);
        }
        while let Some((_, alloc)) = inner.pending_tx.pop_front() {
            let _ = KERNEL_ALLOCATOR.free_physical(alloc);
        }
    }
}

// ── PnP 驱动绑定 ──────────────────────────────────────────────────────

pub struct VirtioNetPciDriver {
    virt_to_phys: fn(usize) -> usize,
}

struct VirtioNetPciBinding {
    iface_id: net::InterfaceId,
    net_dev: Arc<net::NetDevice>,
    _driver: Arc<VirtioNetPci>,
}

impl VirtioNetPciDriver {
    pub const fn new(virt_to_phys: fn(usize) -> usize) -> Self {
        Self { virt_to_phys }
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
        // VirtIO Network: legacy 0x1000, modern 0x1041
        pci_info.vendor == 0x1af4 && (pci_info.device_id == 0x1000 || pci_info.device_id == 0x1041)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(dev).ok_or(PnpError::InvalidState)?;

        let driver = VirtioNetPci::probe(&pci, self.virt_to_phys).map_err(|msg| {
            log::printk!("[virtio-net] probe failed: {}", msg);
            PnpError::ProbeFailed
        })?;

        let idx = NEXT_ETH_INDEX.fetch_add(1, Ordering::Relaxed);
        let name = alloc::format!("eth{}", idx);
        let mac = driver.mac;
        let driver = Arc::new(driver);
        let net_dev = Arc::new(net::NetDevice::new(&name, driver.clone()));
        let config = net::IfConfig::auto();
        net::stack()
            .attach(Arc::clone(&net_dev), config)
            .map_err(|_| PnpError::ProbeFailed)?;
        if let Err(err) =
            dev.register_function(Arc::new(NetFunction::new(&name, Arc::clone(&net_dev))))
        {
            net_dev.mark_gone();
            let _ = net::stack().detach(net_dev.id());
            return Err(err);
        }
        dev.set_driver_data(Arc::new(VirtioNetPciBinding {
            iface_id: net_dev.id(),
            net_dev: Arc::clone(&net_dev),
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

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioNetPciDriver::new(ctx.virt_to_phys)))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(VirtioNetPciFactory)).map(|_| ())
}
