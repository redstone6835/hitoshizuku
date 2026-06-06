//! VirtIO PCI 块设备驱动（modern VirtIO over PCI，VirtIO 1.0+）。
//!
//! 与 [`virtio_blk::VirtioBlk`](super::virtio_blk::VirtioBlk)(MMIO 版本)互补。
//! 本驱动通过 PCI capability list 定位 `common_cfg`/`notify_cfg`/`isr_cfg`/
//! `device_cfg` 四个能力,在 probe 时完成:
//!
//! 1. 读取 [`PciInfo`],匹配 Red Hat vendor `0x1af4` + block device(ID 0x1001
//!    legacy transitional 或 0x1042 modern non-transitional)。
//! 2. 映射 BARs,把各 capability 偏移换算为寄存器虚拟地址。
//! 3. reset → ACKNOWLEDGE → DRIVER → negotiate features → FEATURES_OK →
//!    分配 DMA 物理页构造 virtqueue → DRIVER_OK。
//! 4. 按现有 [`virtio_blk`] 的 `VirtqDesc/VirtqAvail/VirtqUsed` 布局提交请求。
//! 5. 封装成 [`BlockIo`](crate::dev::block::BlockIo) 并通过
//!    [`PnpDevice::register_function`](crate::dev::pnp::PnpDevice::register_function)
//!    以 `/dev/vd*` 形式对外暴露。
//!
//! 内建注册入口只提交 factory；PCI host 初始化和总线扫描仍由启动路径负责。
//!
//! remove 路径把 device status 写 0,释放队列 DMA 页,`BlockDev` 的
//! `mark_gone` 由 PnP 框架统一处理。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use allocator::{KERNEL_ALLOCATOR, PAGE_SIZE, PhysicalAllocRequest, PhysicalAllocation};
use core::mem;
use core::num::NonZeroU32;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::mutex::Mutex;

use crate::dev::bio::{Bio, BioIoError, BioOp, SubmitError};
use crate::dev::block::{
    BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures, BlockGeometry,
    BlockLimits,
};
use crate::dev::function::BlockFunction;
use crate::dev::pci::{PciBar, PciBarType, PciDevice, PciInfo};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};

// ── VirtIO PCI capability 类型 ──────────────────────────────────────────

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// ── common_cfg 寄存器布局(VIRTIO 1.2 §4.1.4.3) ────────────────────────

const CC_DEVICE_FEATURE_SELECT: usize = 0x00; // u32 rw
const CC_DEVICE_FEATURE: usize = 0x04; // u32 ro
const CC_DRIVER_FEATURE_SELECT: usize = 0x08; // u32 rw
const CC_DRIVER_FEATURE: usize = 0x0c; // u32 rw
#[allow(dead_code)]
const CC_CONFIG_MSIX_VECTOR: usize = 0x10; // u16 rw
#[allow(dead_code)]
const CC_NUM_QUEUES: usize = 0x12; // u16 ro
const CC_DEVICE_STATUS: usize = 0x14; // u8 rw
#[allow(dead_code)]
const CC_CONFIG_GENERATION: usize = 0x15; // u8 ro
const CC_QUEUE_SELECT: usize = 0x16; // u16 rw
const CC_QUEUE_SIZE: usize = 0x18; // u16 rw
#[allow(dead_code)]
const CC_QUEUE_MSIX_VECTOR: usize = 0x1a; // u16 rw
const CC_QUEUE_ENABLE: usize = 0x1c; // u16 rw
const CC_QUEUE_NOTIFY_OFF: usize = 0x1e; // u16 ro
const CC_QUEUE_DESC: usize = 0x20; // u64 rw
const CC_QUEUE_DRIVER: usize = 0x28; // u64 rw
const CC_QUEUE_DEVICE: usize = 0x30; // u64 rw

// ── device status bits ─────────────────────────────────────────────────

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;

// ── feature bits ───────────────────────────────────────────────────────

const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// ── device config(block) offsets(相对 device_cfg BAR 区域) ──────────

const BLK_CFG_CAPACITY: usize = 0x00;
const BLK_CFG_BLK_SIZE: usize = 0x14;

// ── virtqueue 描述 ─────────────────────────────────────────────────────

const QUEUE_SIZE: u16 = 128;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// ── 结构体 ──────────────────────────────────────────────────────────────

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

#[repr(C)]
struct VirtioBlkReqHeader {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
struct VirtioBlkReqMeta {
    header: VirtioBlkReqHeader,
    status: u8,
    _pad: [u8; 7],
}

// ── 解析出的 capability 定位信息 ────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct VirtioCap {
    /// 虚拟地址基址(BAR 的 MMIO 映射 + cap.offset)。
    vaddr: usize,
    /// 该 capability 在 BAR 内部可访问的长度。
    length: u32,
    /// notify 专用:notify_off_multiplier(其它 cap 忽略)。
    notify_off_multiplier: u32,
}

struct VirtioPciCaps {
    common: VirtioCap,
    notify: VirtioCap,
    _isr: VirtioCap,
    device: Option<VirtioCap>,
}

// ── 队列状态 ────────────────────────────────────────────────────────────

struct VirtioBlkQueue {
    desc_alloc: Option<PhysicalAllocation>,
    avail_alloc: Option<PhysicalAllocation>,
    used_alloc: Option<PhysicalAllocation>,
    desc_table: *mut VirtqDesc,
    avail_ring: *mut VirtqAvail,
    used_ring: *mut VirtqUsed,
    queue_size: u16,
    last_used_idx: u16,
    free_desc: Vec<u16>,
    pending: VecDeque<(u16, Bio, Box<VirtioBlkReqMeta>)>,
}

// Safety: DMA 指针由 Mutex 串行化;没有共享可变别名。
unsafe impl Send for VirtioBlkQueue {}
unsafe impl Sync for VirtioBlkQueue {}

impl Drop for VirtioBlkQueue {
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

// ── 驱动主结构 ──────────────────────────────────────────────────────────

struct VirtioBlkInner {
    caps: VirtioPciCaps,
    /// 队列 0 的 notify 写地址。
    notify_addr: usize,
    capacity: u64,
    block_size: u32,
    read_only: bool,
    has_flush: bool,
    queue: Mutex<VirtioBlkQueue>,
    irq_count: AtomicUsize,
}

pub struct VirtioBlkPci {
    inner: Arc<VirtioBlkInner>,
    virt_to_phys: fn(usize) -> usize,
}

impl Drop for VirtioBlkPci {
    fn drop(&mut self) {
        // reset device
        unsafe {
            write_volatile(
                (self.inner.caps.common.vaddr + CC_DEVICE_STATUS) as *mut u8,
                0,
            );
        }
    }
}

// ── capability 遍历 & 解析 ─────────────────────────────────────────────

/// 在 PCI 能力链里找所有 VIRTIO 类型的 vendor-specific capability,按
/// cfg_type 路由。
fn parse_virtio_caps(pci: &PciDevice) -> Option<VirtioPciCaps> {
    let mut common: Option<VirtioCap> = None;
    let mut notify: Option<VirtioCap> = None;
    let mut isr: Option<VirtioCap> = None;
    let mut device: Option<VirtioCap> = None;

    // 手动遍历 PCI capability list,只认 vendor-specific(ID=0x09)。
    let mut ptr = pci.capabilities_offset()?;
    let mut hops = 0u32;
    while ptr != 0 && hops < 64 {
        let cap_id = pci.read_config_u8(ptr);
        let next = pci.read_config_u8(ptr + 1) as u16 & 0xFC;
        if cap_id == 0x09 {
            // struct virtio_pci_cap {
            //   u8 cap_vndr;        // offset 0 = 0x09
            //   u8 cap_next;        // offset 1
            //   u8 cap_len;         // offset 2
            //   u8 cfg_type;        // offset 3
            //   u8 bar;             // offset 4
            //   u8 id;              // offset 5
            //   u8 padding[2];      // 6..8
            //   le32 offset;        // 8
            //   le32 length;        // 12
            //   // notify only: le32 notify_off_multiplier; // 16
            // };
            let cfg_type = pci.read_config_u8(ptr + 3);
            let bar_idx = pci.read_config_u8(ptr + 4) & 0x7;
            let off_lo = pci.read_config_u16(ptr + 8) as u32;
            let off_hi = pci.read_config_u16(ptr + 10) as u32;
            let offset = off_lo | (off_hi << 16);
            let len_lo = pci.read_config_u16(ptr + 12) as u32;
            let len_hi = pci.read_config_u16(ptr + 14) as u32;
            let length = len_lo | (len_hi << 16);

            // 映射 BAR 到虚拟地址
            if let Some((bar, bar_vaddr)) = pci.map_bar_virt(bar_idx as usize) {
                if matches!(bar.bar_type, PciBarType::Memory) {
                    let vaddr = bar_vaddr.wrapping_add(offset as usize);
                    let cap = VirtioCap {
                        vaddr,
                        length,
                        notify_off_multiplier: 0,
                    };
                    match cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => common = Some(cap),
                        VIRTIO_PCI_CAP_NOTIFY_CFG => {
                            let notify_off_multiplier = pci.read_config_u32(ptr + 16);
                            notify = Some(VirtioCap {
                                vaddr,
                                length,
                                notify_off_multiplier,
                            });
                        }
                        VIRTIO_PCI_CAP_ISR_CFG => isr = Some(cap),
                        VIRTIO_PCI_CAP_DEVICE_CFG => device = Some(cap),
                        _ => {}
                    }
                }
            }
        }
        ptr = next;
        hops += 1;
    }

    Some(VirtioPciCaps {
        common: common?,
        notify: notify?,
        _isr: isr?,
        device,
    })
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
    // VirtIO 允许 64 位 BAR 也按 2×u32 写(低位先),兼容更多 IOMMU 实现。
    wr_u32(addr, v as u32);
    wr_u32(addr + 4, (v >> 32) as u32);
}

fn cc_status(caps: &VirtioPciCaps) -> u8 {
    rd_u8(caps.common.vaddr + CC_DEVICE_STATUS)
}
fn cc_set_status(caps: &VirtioPciCaps, v: u8) {
    wr_u8(caps.common.vaddr + CC_DEVICE_STATUS, v);
}
fn cc_add_status(caps: &VirtioPciCaps, bit: u8) {
    let cur = cc_status(caps);
    cc_set_status(caps, cur | bit);
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

// ── DMA 分配助手 ──────────────────────────────────────────────────────

fn alloc_dma_page() -> Result<PhysicalAllocation, &'static str> {
    KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .map_err(|_| "virtio-pci: DMA page alloc failed")
}

fn dma_vaddr(allocation: PhysicalAllocation) -> Result<usize, &'static str> {
    allocator::KERNEL_ALLOCATOR
        .load_phys_to_virt()
        .map(|phys_to_virt| phys_to_virt(allocation.paddr))
        .ok_or("virtio-pci: phys_to_virt hook is not installed")
}

// ── 初始化序列 ─────────────────────────────────────────────────────────

impl VirtioBlkPci {
    /// 在已绑定 PCI capabilities 的前提下完成 VirtIO 1.0+ probe 流程。
    pub fn probe(pci: &PciDevice, virt_to_phys: fn(usize) -> usize) -> Result<Self, &'static str> {
        // 先打开 bus master + memory space decode —— 没这两个 BAR 根本不响应。
        pci.enable_mmio();
        pci.enable_bus_master();

        let caps = parse_virtio_caps(pci).ok_or("virtio-pci: missing VIRTIO caps")?;
        log::printk!(
            "[virtio-pci] caps: common vaddr={:#x} notify vaddr={:#x} mult={} device={}",
            caps.common.vaddr,
            caps.notify.vaddr,
            caps.notify.notify_off_multiplier,
            caps.device.is_some()
        );

        // 1. reset
        cc_set_status(&caps, 0);
        // 自旋等 reset 生效。
        let mut spin_cnt: u32 = 0;
        while cc_status(&caps) != 0 {
            core::hint::spin_loop();
            spin_cnt = spin_cnt.wrapping_add(1);
            if spin_cnt >= 1_000_000 {
                log::printk!(
                    "[virtio-pci] reset stuck: status still {:#x} after spin",
                    cc_status(&caps)
                );
                return Err("virtio-pci: reset timeout");
            }
        }

        // 2. ACKNOWLEDGE + DRIVER
        cc_add_status(&caps, STATUS_ACKNOWLEDGE);
        cc_add_status(&caps, STATUS_DRIVER);

        // 3. 协商 feature
        let device_features = cc_device_features(&caps);
        log::printk!(
            "[virtio-pci] device_features={:#x} (status={:#x})",
            device_features,
            cc_status(&caps)
        );
        if device_features & VIRTIO_F_VERSION_1 == 0 {
            cc_set_status(&caps, STATUS_FAILED);
            return Err("virtio-pci: device lacks VERSION_1");
        }
        let mut driver_features = VIRTIO_F_VERSION_1;
        if device_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            driver_features |= VIRTIO_BLK_F_BLK_SIZE;
        }
        if device_features & VIRTIO_BLK_F_FLUSH != 0 {
            driver_features |= VIRTIO_BLK_F_FLUSH;
        }
        if device_features & VIRTIO_BLK_F_RO != 0 {
            driver_features |= VIRTIO_BLK_F_RO;
        }
        cc_set_driver_features(&caps, driver_features);
        cc_add_status(&caps, STATUS_FEATURES_OK);
        if cc_status(&caps) & STATUS_FEATURES_OK == 0 {
            cc_set_status(&caps, STATUS_FAILED);
            return Err("virtio-pci: FEATURES_OK rejected");
        }

        // 4. 读设备配置(capacity / block_size)
        let device_cap = caps
            .device
            .ok_or("virtio-pci: missing device_cfg capability")?;
        let capacity = unsafe {
            let lo = read_volatile((device_cap.vaddr + BLK_CFG_CAPACITY) as *const u32) as u64;
            let hi = read_volatile((device_cap.vaddr + BLK_CFG_CAPACITY + 4) as *const u32) as u64;
            (hi << 32) | lo
        };
        let block_size = if driver_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            unsafe { read_volatile((device_cap.vaddr + BLK_CFG_BLK_SIZE) as *const u32) }
        } else {
            512
        };
        if block_size < 512 || !block_size.is_power_of_two() || !block_size.is_multiple_of(512) {
            cc_set_status(&caps, STATUS_FAILED);
            return Err("virtio-pci: invalid block size");
        }

        // 5. 设置队列 0
        wr_u16(caps.common.vaddr + CC_QUEUE_SELECT, 0);
        let max_qsz = rd_u16(caps.common.vaddr + CC_QUEUE_SIZE);
        if max_qsz == 0 {
            cc_set_status(&caps, STATUS_FAILED);
            return Err("virtio-pci: queue 0 size is zero");
        }
        let qsz = max_qsz.min(QUEUE_SIZE);
        wr_u16(caps.common.vaddr + CC_QUEUE_SIZE, qsz);

        // 分配 DMA 页
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

        // 写 queue_desc/driver/device 物理地址
        wr_u64(caps.common.vaddr + CC_QUEUE_DESC, desc_alloc.paddr as u64);
        wr_u64(
            caps.common.vaddr + CC_QUEUE_DRIVER,
            avail_alloc.paddr as u64,
        );
        wr_u64(caps.common.vaddr + CC_QUEUE_DEVICE, used_alloc.paddr as u64);

        // notify offset
        let notify_off = rd_u16(caps.common.vaddr + CC_QUEUE_NOTIFY_OFF) as usize;
        let notify_addr =
            caps.notify.vaddr + notify_off * caps.notify.notify_off_multiplier as usize;

        // 启用队列
        wr_u16(caps.common.vaddr + CC_QUEUE_ENABLE, 1);

        // 初始化空闲描述符栈(按 LIFO,低编号优先,便于诊断)
        let mut free_desc = Vec::with_capacity(qsz as usize);
        for i in (0..qsz).rev() {
            free_desc.push(i);
        }

        // 6. DRIVER_OK
        cc_add_status(&caps, STATUS_DRIVER_OK);

        let read_only = driver_features & VIRTIO_BLK_F_RO != 0;
        let has_flush = driver_features & VIRTIO_BLK_F_FLUSH != 0;

        let queue = VirtioBlkQueue {
            desc_alloc: Some(desc_alloc),
            avail_alloc: Some(avail_alloc),
            used_alloc: Some(used_alloc),
            desc_table,
            avail_ring,
            used_ring,
            queue_size: qsz,
            last_used_idx: 0,
            free_desc,
            pending: VecDeque::new(),
        };

        let inner = Arc::new(VirtioBlkInner {
            caps,
            notify_addr,
            capacity,
            block_size,
            read_only,
            has_flush,
            queue: Mutex::new(queue),
            irq_count: AtomicUsize::new(0),
        });

        Ok(Self {
            inner,
            virt_to_phys,
        })
    }

    /// 轮询并处理已完成的请求。与 MMIO 版对称。
    pub fn poll(&self) {
        let mut queue = self.inner.queue.lock();
        loop {
            let next = unsafe {
                core::sync::atomic::fence(Ordering::Acquire);
                let used = &*queue.used_ring;
                let used_idx = used.idx;
                if queue.last_used_idx == used_idx {
                    break;
                }
                let elem = used.ring[queue.last_used_idx as usize % queue.queue_size as usize];
                queue.last_used_idx = queue.last_used_idx.wrapping_add(1);
                (elem.id as u16, elem.len)
            };
            let (desc_head, _len) = next;
            if let Some(pos) = queue
                .pending
                .iter()
                .position(|(head, _, _)| *head == desc_head)
            {
                let Some((_, bio, meta)) = queue.pending.remove(pos) else {
                    continue;
                };
                core::sync::atomic::fence(Ordering::Acquire);
                let result = match meta.status {
                    VIRTIO_BLK_S_OK => Ok(()),
                    VIRTIO_BLK_S_UNSUPP => Err(BioIoError::Unsupported),
                    _ => Err(BioIoError::MediaError),
                };

                // 释放整条描述符链
                let mut chain = Vec::new();
                let mut idx = desc_head;
                loop {
                    chain.push(idx);
                    unsafe {
                        let desc = &*queue.desc_table.add(idx as usize);
                        if desc.flags & VIRTQ_DESC_F_NEXT != 0 {
                            idx = desc.next;
                        } else {
                            break;
                        }
                    }
                }
                for i in chain {
                    queue.free_desc.push(i);
                }

                drop(queue);
                bio.complete(result);
                queue = self.inner.queue.lock();
            }
        }
    }

    fn handle_interrupt(&self) {
        self.inner.irq_count.fetch_add(1, Ordering::Relaxed);
        self.poll();
    }

    pub fn into_block_dev(self, name: &str) -> Result<Arc<BlockDevice>, &'static str> {
        let capacity = self.inner.capacity;
        let block_size = self.inner.block_size;
        let sector_scale = (block_size / 512) as u64;
        if sector_scale == 0 || capacity % sector_scale != 0 {
            return Err("virtio-pci: invalid capacity for logical block size");
        }
        let logical_blocks = capacity / sector_scale;
        if logical_blocks == 0 {
            return Err("virtio-pci: invalid capacity");
        }
        let logical = NonZeroU32::new(block_size).ok_or("virtio-pci: invalid block size")?;
        let geometry = BlockGeometry::new(logical, logical, Some(logical_blocks))
            .ok_or("virtio-pci: invalid geometry")?;
        let limits = BlockLimits::unrestricted();
        let mut features = BlockFeatures(0);
        if self.inner.has_flush {
            features |= BlockFeatures::FLUSH;
        }
        if self.inner.read_only {
            features |= BlockFeatures::READ_ONLY;
        }
        let virt_to_phys = self.virt_to_phys;
        let io = Arc::new(VirtioBlkPciIo {
            driver: Arc::new(self),
            virt_to_phys,
        });
        let init = BlockDeviceInit {
            name,
            subsystem: "virtio-blk",
            class: BlockClass::Whole,
            geometry,
            limits,
            features,
        };
        Ok(Arc::new(BlockDevice::new(init, io, None)))
    }
}

// ── BlockIo 实现 ────────────────────────────────────────────────────────

struct VirtioBlkPciIo {
    driver: Arc<VirtioBlkPci>,
    virt_to_phys: fn(usize) -> usize,
}

impl VirtioBlkPciIo {
    fn notify_queue(&self) {
        // Notify register is a u16 write at calculated address.
        wr_u16(self.driver.inner.notify_addr, 0);
    }
}

impl BlockDriver for VirtioBlkPciIo {
    fn queue_bio(&self, bio: Bio) -> Result<(), (SubmitError, Bio)> {
        self.driver.poll();
        let mut queue = self.driver.inner.queue.lock();

        let desc_count = match bio.op {
            BioOp::Read | BioOp::Write => 3,
            BioOp::Flush => 2,
            _ => return Err((SubmitError::Unsupported, bio)),
        };
        if queue.free_desc.len() < desc_count {
            return Err((SubmitError::QueueFull, bio));
        }

        let sector_scale = self.driver.inner.block_size as u64 / 512;
        let req_type = match bio.op {
            BioOp::Read => VIRTIO_BLK_T_IN,
            BioOp::Write => VIRTIO_BLK_T_OUT,
            BioOp::Flush => VIRTIO_BLK_T_FLUSH,
            _ => unreachable!(),
        };
        let sector = match bio.op {
            BioOp::Flush => 0,
            _ => bio.range.lba.saturating_mul(sector_scale),
        };
        let meta = Box::new(VirtioBlkReqMeta {
            header: VirtioBlkReqHeader {
                req_type,
                reserved: 0,
                sector,
            },
            status: 0xff,
            _pad: [0; 7],
        });

        let header_phys = (self.virt_to_phys)(&meta.header as *const _ as usize) as u64;
        let status_phys = (self.virt_to_phys)(&meta.status as *const _ as usize) as u64;

        let d0 = queue.free_desc.pop().unwrap();
        let d1 = queue.free_desc.pop().unwrap();
        let d2 = if desc_count == 3 {
            Some(queue.free_desc.pop().unwrap())
        } else {
            None
        };

        let head_idx = d0;

        match bio.op {
            BioOp::Read => {
                let d2 = d2.unwrap();
                let buf_phys = (self.virt_to_phys)(bio.buffer.as_slice().as_ptr() as usize) as u64;
                unsafe {
                    *queue.desc_table.add(d0 as usize) = VirtqDesc {
                        addr: header_phys,
                        len: mem::size_of::<VirtioBlkReqHeader>() as u32,
                        flags: VIRTQ_DESC_F_NEXT,
                        next: d1,
                    };
                    *queue.desc_table.add(d1 as usize) = VirtqDesc {
                        addr: buf_phys,
                        len: bio.buffer.len() as u32,
                        flags: VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                        next: d2,
                    };
                    *queue.desc_table.add(d2 as usize) = VirtqDesc {
                        addr: status_phys,
                        len: 1,
                        flags: VIRTQ_DESC_F_WRITE,
                        next: 0,
                    };
                }
            }
            BioOp::Write => {
                let d2 = d2.unwrap();
                let buf_phys = (self.virt_to_phys)(bio.buffer.as_slice().as_ptr() as usize) as u64;
                unsafe {
                    *queue.desc_table.add(d0 as usize) = VirtqDesc {
                        addr: header_phys,
                        len: mem::size_of::<VirtioBlkReqHeader>() as u32,
                        flags: VIRTQ_DESC_F_NEXT,
                        next: d1,
                    };
                    *queue.desc_table.add(d1 as usize) = VirtqDesc {
                        addr: buf_phys,
                        len: bio.buffer.len() as u32,
                        flags: VIRTQ_DESC_F_NEXT,
                        next: d2,
                    };
                    *queue.desc_table.add(d2 as usize) = VirtqDesc {
                        addr: status_phys,
                        len: 1,
                        flags: VIRTQ_DESC_F_WRITE,
                        next: 0,
                    };
                }
            }
            BioOp::Flush => unsafe {
                *queue.desc_table.add(d0 as usize) = VirtqDesc {
                    addr: header_phys,
                    len: mem::size_of::<VirtioBlkReqHeader>() as u32,
                    flags: VIRTQ_DESC_F_NEXT,
                    next: d1,
                };
                *queue.desc_table.add(d1 as usize) = VirtqDesc {
                    addr: status_phys,
                    len: 1,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                };
            },
            _ => unreachable!(),
        }

        // submit to available ring
        unsafe {
            let avail = &mut *queue.avail_ring;
            let idx = avail.idx;
            avail.ring[idx as usize % queue.queue_size as usize] = head_idx;
            core::sync::atomic::fence(Ordering::Release);
            avail.idx = idx.wrapping_add(1);
        }

        queue.pending.push_back((head_idx, bio, meta));
        drop(queue);
        self.notify_queue();
        Ok(())
    }

    fn drain(&self) {
        self.driver.poll();
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── PnpDriver 绑定 ──────────────────────────────────────────────────────

/// VirtIO over PCI(modern)block 设备驱动。
///
/// 匹配 Red Hat vendor `0x1af4`:
/// - `0x1001`: legacy/transitional virtio-blk(仍可用 modern cap)
/// - `0x1042`: modern non-transitional virtio-blk
pub struct VirtioPciBlkDriver {
    /// virt↔phys 转换(由 arch 提供,在驱动注册时绑定)。
    virt_to_phys: fn(usize) -> usize,
    /// 已分配的 /dev/vd* 计数,用来生成下一个名字。
    next_index: core::sync::atomic::AtomicUsize,
}

impl VirtioPciBlkDriver {
    /// 创建 VirtIO-PCI block PnP 驱动。
    pub const fn new(virt_to_phys: fn(usize) -> usize) -> Self {
        Self {
            virt_to_phys,
            next_index: core::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl PnpDriver for VirtioPciBlkDriver {
    fn name(&self) -> &'static str {
        "virtio-pci-blk"
    }

    fn bus_type(&self) -> BusType {
        BusType::PCI
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        let PnpId::Pci { .. } = id else {
            return false;
        };
        let Some(pci_info) = info.as_any().downcast_ref::<PciInfo>() else {
            return false;
        };
        pci_info.vendor == 0x1af4 && (pci_info.device_id == 0x1001 || pci_info.device_id == 0x1042)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(dev).ok_or(PnpError::InvalidState)?;

        let driver = VirtioBlkPci::probe(&pci, self.virt_to_phys).map_err(|msg| {
            log::printk!("[virtio-pci] probe failed: {}", msg);
            PnpError::ProbeFailed
        })?;

        let idx = self.next_index.fetch_add(1, Ordering::Relaxed);
        let dev_name: alloc::string::String = alloc::format!("vd{}", idx);
        let block_dev = driver
            .into_block_dev(&dev_name)
            .map_err(|_| PnpError::ProbeFailed)?;

        dev.register_function(Arc::new(BlockFunction::new(&dev_name, block_dev)))?;
        log::printk!("[virtio-pci] bound {} → /dev/{}", dev.id, dev_name);
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        // VirtioBlkPci 的 Drop 会 reset device;PnpDevice::remove_device 在
        // 清理 functions 后会调用驱动的 remove,然后释放 driver_data;这里
        // 不持有 driver_data,只做日志。
        log::printk!("[virtio-pci] remove {}", dev.id);
    }
}

struct VirtioPciBlkFactory;

impl DriverFactory for VirtioPciBlkFactory {
    fn name(&self) -> &'static str {
        "virtio-pci-blk"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioPciBlkDriver::new(ctx.virt_to_phys)))
    }
}

/// 注册 VirtIO-PCI block 内建驱动 factory。
pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(VirtioPciBlkFactory)).map(|_| ())
}

// 避免 BAR 解析时 PciBar 字段被优化掉；保留显式静默引用。
#[allow(dead_code)]
fn _keep_prefetchable(bar: &PciBar) -> bool {
    bar.prefetchable
}
