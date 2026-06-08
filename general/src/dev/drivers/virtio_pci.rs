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

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::mem;
use core::num::NonZeroU32;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::mutex::Mutex;

use super::{VIRTIO_BLK_SECTOR_SIZE, alloc_virtio_blk_dev_name, virtio_blk_limits};
use crate::dev::bio::{Bio, BioBuffer, BioIoError, BioOp, BioReqError, SubmitError};
use crate::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures,
    BlockGeometry,
};
use crate::dev::dma::{DmaBuffer, DmaDirection};
use crate::dev::function::BlockFunction;
use crate::dev::pci::{PciBar, PciBarType, PciDevice, PciInfo};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};
use crate::dev::virtio::{SplitVirtQueue, VIRTQ_DESC_F_WRITE, choose_split_queue_size};

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

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;
/// virtio-blk 单个普通 I/O 至少需要 header/data/status 三个描述符，取 2 的幂后为 4。
const VIRTIO_BLK_MIN_QUEUE_SIZE: u16 = 4;

// ── 结构体 ──────────────────────────────────────────────────────────────

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
    queue: SplitVirtQueue,
    pending: VecDeque<PendingVirtioPciRequest>,
}

struct PendingVirtioPciRequest {
    head: u16,
    bio: Bio,
    meta_dma: DmaBuffer,
    data_dma: Option<DmaBuffer>,
}

// Safety: DMA 指针由 Mutex 串行化;没有共享可变别名。
unsafe impl Send for VirtioBlkQueue {}
unsafe impl Sync for VirtioBlkQueue {}

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

    for cap_header in pci.capabilities().filter(|cap| cap.id == 0x09) {
        let ptr = cap_header.offset;
        let cap_len = pci.read_config_u8(ptr + 2);
        let cfg_type = pci.read_config_u8(ptr + 3);
        let min_len = if cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG {
            20
        } else {
            16
        };
        if cap_len < min_len {
            continue;
        }

        let bar_idx = pci.read_config_u8(ptr + 4) & 0x7;
        let offset = pci.read_config_u32(ptr + 8);
        let length = pci.read_config_u32(ptr + 12);
        if length == 0 {
            continue;
        }

        let Some((bar, bar_vaddr)) = pci.map_bar_virt(bar_idx as usize) else {
            continue;
        };
        if !matches!(bar.bar_type, PciBarType::Memory) {
            continue;
        }
        let Some(end) = (offset as u64).checked_add(length as u64) else {
            continue;
        };
        if end > bar.size {
            continue;
        }

        let Some(vaddr) = bar_vaddr.checked_add(offset as usize) else {
            continue;
        };
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

    Some(VirtioPciCaps {
        common: common?,
        notify: notify?,
        _isr: isr?,
        device,
    })
}

fn cap_covers(cap: &VirtioCap, offset: usize, len: usize) -> bool {
    offset
        .checked_add(len)
        .is_some_and(|end| end <= cap.length as usize)
}

fn validate_cap_range(
    cap: &VirtioCap,
    offset: usize,
    len: usize,
    error: &'static str,
) -> Result<(), &'static str> {
    if cap_covers(cap, offset, len) {
        Ok(())
    } else {
        Err(error)
    }
}

fn validate_base_caps(caps: &VirtioPciCaps) -> Result<(), &'static str> {
    // PCI capability 的 length 是传输层给出的 MMIO 窗口边界；
    // 所有寄存器访问前先验证覆盖范围，避免坏固件/坏设备把硬编码偏移变成越界 MMIO。
    validate_cap_range(
        &caps.common,
        0,
        CC_QUEUE_DEVICE + mem::size_of::<u64>(),
        "virtio-pci: common_cfg capability too short",
    )?;
    validate_cap_range(
        &caps.notify,
        0,
        mem::size_of::<u16>(),
        "virtio-pci: notify_cfg capability too short",
    )
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

// ── 初始化序列 ─────────────────────────────────────────────────────────

impl VirtioBlkPci {
    /// 在已绑定 PCI capabilities 的前提下完成 VirtIO 1.0+ probe 流程。
    pub fn probe(pci: &PciDevice) -> Result<Self, &'static str> {
        // 先打开 bus master + memory space decode —— 没这两个 BAR 根本不响应。
        pci.enable_mmio();
        pci.enable_bus_master();

        let caps = parse_virtio_caps(pci).ok_or("virtio-pci: missing VIRTIO caps")?;
        validate_base_caps(&caps)?;
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
        validate_cap_range(
            &device_cap,
            BLK_CFG_CAPACITY,
            mem::size_of::<u64>(),
            "virtio-pci: device_cfg capacity out of range",
        )?;
        let capacity = unsafe {
            let lo = read_volatile((device_cap.vaddr + BLK_CFG_CAPACITY) as *const u32) as u64;
            let hi = read_volatile((device_cap.vaddr + BLK_CFG_CAPACITY + 4) as *const u32) as u64;
            (hi << 32) | lo
        };
        let block_size = if driver_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            validate_cap_range(
                &device_cap,
                BLK_CFG_BLK_SIZE,
                mem::size_of::<u32>(),
                "virtio-pci: device_cfg block size out of range",
            )?;
            unsafe { read_volatile((device_cap.vaddr + BLK_CFG_BLK_SIZE) as *const u32) }
        } else {
            VIRTIO_BLK_SECTOR_SIZE
        };
        if block_size < VIRTIO_BLK_SECTOR_SIZE
            || !block_size.is_power_of_two()
            || !block_size.is_multiple_of(VIRTIO_BLK_SECTOR_SIZE)
        {
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
        let qsz =
            choose_split_queue_size(max_qsz, None).map_err(|_| "virtio-pci: invalid queue size")?;
        if qsz < VIRTIO_BLK_MIN_QUEUE_SIZE {
            cc_set_status(&caps, STATUS_FAILED);
            return Err("virtio-pci: queue 0 too small");
        }
        wr_u16(caps.common.vaddr + CC_QUEUE_SIZE, qsz);

        let split_queue =
            SplitVirtQueue::new(qsz).map_err(|_| "virtio-pci: queue allocation failed")?;

        // 写 queue_desc/driver/device 物理地址
        wr_u64(
            caps.common.vaddr + CC_QUEUE_DESC,
            split_queue.desc_paddr() as u64,
        );
        wr_u64(
            caps.common.vaddr + CC_QUEUE_DRIVER,
            split_queue.avail_paddr() as u64,
        );
        wr_u64(
            caps.common.vaddr + CC_QUEUE_DEVICE,
            split_queue.used_paddr() as u64,
        );

        // notify offset
        let notify_off = rd_u16(caps.common.vaddr + CC_QUEUE_NOTIFY_OFF) as usize;
        let notify_offset = notify_off
            .checked_mul(caps.notify.notify_off_multiplier as usize)
            .ok_or("virtio-pci: notify offset overflow")?;
        validate_cap_range(
            &caps.notify,
            notify_offset,
            mem::size_of::<u16>(),
            "virtio-pci: notify offset out of range",
        )?;
        let notify_addr = caps
            .notify
            .vaddr
            .checked_add(notify_offset)
            .ok_or("virtio-pci: notify address overflow")?;

        // 启用队列
        wr_u16(caps.common.vaddr + CC_QUEUE_ENABLE, 1);

        // 6. DRIVER_OK
        cc_add_status(&caps, STATUS_DRIVER_OK);

        let read_only = driver_features & VIRTIO_BLK_F_RO != 0;
        let has_flush = driver_features & VIRTIO_BLK_F_FLUSH != 0;

        let queue = VirtioBlkQueue {
            queue: split_queue,
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

        Ok(Self { inner })
    }

    fn complete_failed_requests(mut pending: VecDeque<PendingVirtioPciRequest>, error: BioIoError) {
        while let Some(pending) = pending.pop_front() {
            pending.bio.complete(Err(error));
        }
    }

    fn fail_queue_locked(
        &self,
        queue: &mut VirtioBlkQueue,
        reason: &'static str,
    ) -> VecDeque<PendingVirtioPciRequest> {
        log::printk!("[virtio-pci-blk] queue failed: {}", reason);
        cc_set_status(
            &self.inner.caps,
            cc_status(&self.inner.caps) | STATUS_FAILED,
        );

        let mut failed = VecDeque::new();
        mem::swap(&mut failed, &mut queue.pending);
        failed
    }

    /// 轮询并处理已完成的请求。与 MMIO 版对称。
    pub fn poll(&self) {
        let mut queue = self.inner.queue.lock();
        loop {
            let used = match queue.queue.pop_used() {
                Ok(Some(used)) => used,
                Ok(None) => break,
                Err(_) => {
                    let failed = self.fail_queue_locked(&mut queue, "used ring corruption");
                    drop(queue);
                    Self::complete_failed_requests(failed, BioIoError::Unavailable);
                    return;
                }
            };
            let desc_head = used.head;
            if let Some(pos) = queue
                .pending
                .iter()
                .position(|pending| pending.head == desc_head)
            {
                let Some(mut pending) = queue.pending.remove(pos) else {
                    continue;
                };
                core::sync::atomic::fence(Ordering::Acquire);
                pending.meta_dma.sync_for_cpu();
                let status = unsafe {
                    let meta = &*(pending.meta_dma.vaddr() as *const VirtioBlkReqMeta);
                    meta.status
                };
                let result = match status {
                    VIRTIO_BLK_S_OK => Ok(()),
                    VIRTIO_BLK_S_UNSUPP => Err(BioIoError::Unsupported),
                    _ => Err(BioIoError::MediaError),
                };

                if result.is_ok() && pending.bio.op == BioOp::Read {
                    if let (BioBuffer::Owned(buf), Some(data_dma)) =
                        (&mut pending.bio.buffer, pending.data_dma.as_ref())
                    {
                        data_dma.sync_for_cpu();
                        let take = buf.len().min(data_dma.as_slice().len());
                        buf[..take].copy_from_slice(&data_dma.as_slice()[..take]);
                    }
                }

                if queue.queue.free_chain_from_head(desc_head).is_err() {
                    let failed =
                        self.fail_queue_locked(&mut queue, "completed descriptor chain corrupt");
                    drop(queue);
                    pending.bio.complete(Err(BioIoError::Unavailable));
                    Self::complete_failed_requests(failed, BioIoError::Unavailable);
                    return;
                }

                drop(queue);
                pending.bio.complete(result);
                queue = self.inner.queue.lock();
            } else {
                log::printk!(
                    "[virtio-pci-blk] used head {} has no pending request",
                    desc_head
                );
                if queue.queue.free_chain_from_head(desc_head).is_err() {
                    let failed =
                        self.fail_queue_locked(&mut queue, "unknown used head cannot be freed");
                    drop(queue);
                    Self::complete_failed_requests(failed, BioIoError::Unavailable);
                    return;
                }
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
        let sector_scale = u64::from(block_size / VIRTIO_BLK_SECTOR_SIZE);
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
        let limits = virtio_blk_limits(block_size);
        let queue_depth = self.inner.queue.lock().queue.queue_size() as u32;
        let attributes = BlockAttributes::new(false, false, NonZeroU32::new(queue_depth), None);
        let mut features = BlockFeatures(0);
        if self.inner.has_flush {
            features |= BlockFeatures::FLUSH;
        }
        if self.inner.read_only {
            features |= BlockFeatures::READ_ONLY;
        }
        let io = Arc::new(VirtioBlkPciIo {
            driver: Arc::new(self),
        });
        let init = BlockDeviceInit {
            name,
            subsystem: "virtio-blk",
            class: BlockClass::Whole,
            geometry,
            limits,
            attributes,
            features,
        };
        Ok(Arc::new(BlockDevice::new(init, io, None)))
    }
}

// ── BlockIo 实现 ────────────────────────────────────────────────────────

struct VirtioBlkPciIo {
    driver: Arc<VirtioBlkPci>,
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
        if queue.queue.free_descriptor_count() < desc_count {
            return Err((SubmitError::QueueFull, bio));
        }
        let data_len = bio.buffer.len();
        let data_len_u32 = match u32::try_from(data_len) {
            Ok(len) => len,
            Err(_) => return Err((SubmitError::InvalidRequest(BioReqError::TooLarge), bio)),
        };

        let sector_scale = u64::from(self.driver.inner.block_size / VIRTIO_BLK_SECTOR_SIZE);
        let req_type = match bio.op {
            BioOp::Read => VIRTIO_BLK_T_IN,
            BioOp::Write => VIRTIO_BLK_T_OUT,
            BioOp::Flush => VIRTIO_BLK_T_FLUSH,
            _ => return Err((SubmitError::Unsupported, bio)),
        };
        let sector = match bio.op {
            BioOp::Flush => 0,
            _ => match bio.range.lba.checked_mul(sector_scale) {
                Some(sector) => sector,
                None => return Err((SubmitError::InvalidRequest(BioReqError::OutOfBounds), bio)),
            },
        };

        let chain = match queue.queue.alloc_chain(desc_count) {
            Ok(chain) => chain,
            Err(_) => return Err((SubmitError::QueueFull, bio)),
        };

        let meta_dma = match DmaBuffer::new(
            mem::size_of::<VirtioBlkReqMeta>(),
            mem::align_of::<VirtioBlkReqMeta>(),
            DmaDirection::Bidirectional,
        ) {
            Ok(buffer) => buffer,
            Err(_) => {
                let _ = queue.queue.free_chain(chain);
                return Err((SubmitError::OutOfMemory, bio));
            }
        };
        let meta = VirtioBlkReqMeta {
            header: VirtioBlkReqHeader {
                req_type,
                reserved: 0,
                sector,
            },
            status: 0xff,
            _pad: [0; 7],
        };
        unsafe {
            core::ptr::write(meta_dma.vaddr() as *mut VirtioBlkReqMeta, meta);
        }
        meta_dma.sync_for_device();

        let data_dma = match bio.op {
            BioOp::Read | BioOp::Write => {
                let direction = if bio.op == BioOp::Read {
                    DmaDirection::FromDevice
                } else {
                    DmaDirection::ToDevice
                };
                let mut dma = match DmaBuffer::new(data_len, 1, direction) {
                    Ok(buffer) => buffer,
                    Err(_) => {
                        let _ = queue.queue.free_chain(chain);
                        return Err((SubmitError::OutOfMemory, bio));
                    }
                };
                if bio.op == BioOp::Write {
                    dma.as_mut_slice().copy_from_slice(bio.buffer.as_slice());
                }
                dma.sync_for_device();
                Some(dma)
            }
            BioOp::Flush => None,
            _ => {
                let _ = queue.queue.free_chain(chain);
                return Err((SubmitError::Unsupported, bio));
            }
        };

        let header_phys = meta_dma.paddr() as u64;
        let status_phys = meta_dma.paddr() as u64 + mem::size_of::<VirtioBlkReqHeader>() as u64;
        let head_idx = chain.head();

        match bio.op {
            BioOp::Read => {
                let Some(data_dma) = data_dma.as_ref() else {
                    let _ = queue.queue.free_chain(chain);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                let (Some(d0), Some(d1), Some(d2)) = (chain.get(0), chain.get(1), chain.get(2))
                else {
                    let _ = queue.queue.free_chain(chain);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                if queue
                    .queue
                    .write_desc(
                        d0,
                        header_phys,
                        mem::size_of::<VirtioBlkReqHeader>() as u32,
                        0,
                        Some(d1),
                    )
                    .and_then(|_| {
                        queue.queue.write_desc(
                            d1,
                            data_dma.paddr() as u64,
                            data_len_u32,
                            VIRTQ_DESC_F_WRITE,
                            Some(d2),
                        )
                    })
                    .and_then(|_| {
                        queue
                            .queue
                            .write_desc(d2, status_phys, 1, VIRTQ_DESC_F_WRITE, None)
                    })
                    .is_err()
                {
                    let _ = queue.queue.free_chain(chain);
                    return Err((SubmitError::QueueFull, bio));
                }
            }
            BioOp::Write => {
                let Some(data_dma) = data_dma.as_ref() else {
                    let _ = queue.queue.free_chain(chain);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                let (Some(d0), Some(d1), Some(d2)) = (chain.get(0), chain.get(1), chain.get(2))
                else {
                    let _ = queue.queue.free_chain(chain);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                if queue
                    .queue
                    .write_desc(
                        d0,
                        header_phys,
                        mem::size_of::<VirtioBlkReqHeader>() as u32,
                        0,
                        Some(d1),
                    )
                    .and_then(|_| {
                        queue.queue.write_desc(
                            d1,
                            data_dma.paddr() as u64,
                            data_len_u32,
                            0,
                            Some(d2),
                        )
                    })
                    .and_then(|_| {
                        queue
                            .queue
                            .write_desc(d2, status_phys, 1, VIRTQ_DESC_F_WRITE, None)
                    })
                    .is_err()
                {
                    let _ = queue.queue.free_chain(chain);
                    return Err((SubmitError::QueueFull, bio));
                }
            }
            BioOp::Flush => {
                let (Some(d0), Some(d1)) = (chain.get(0), chain.get(1)) else {
                    let _ = queue.queue.free_chain(chain);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                if queue
                    .queue
                    .write_desc(
                        d0,
                        header_phys,
                        mem::size_of::<VirtioBlkReqHeader>() as u32,
                        0,
                        Some(d1),
                    )
                    .and_then(|_| {
                        queue
                            .queue
                            .write_desc(d1, status_phys, 1, VIRTQ_DESC_F_WRITE, None)
                    })
                    .is_err()
                {
                    let _ = queue.queue.free_chain(chain);
                    return Err((SubmitError::QueueFull, bio));
                }
            }
            _ => {
                let _ = queue.queue.free_chain(chain);
                return Err((SubmitError::Unsupported, bio));
            }
        }

        // submit to available ring
        if queue.queue.push_avail(head_idx).is_err() {
            let _ = queue.queue.free_chain(chain);
            return Err((SubmitError::QueueFull, bio));
        }

        queue.pending.push_back(PendingVirtioPciRequest {
            head: head_idx,
            bio,
            meta_dma,
            data_dma,
        });
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
pub struct VirtioPciBlkDriver {}

impl VirtioPciBlkDriver {
    /// 创建 VirtIO-PCI block PnP 驱动。
    pub const fn new() -> Self {
        Self {}
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

        let driver = VirtioBlkPci::probe(&pci).map_err(|msg| {
            log::printk!("[virtio-pci] probe failed: {}", msg);
            PnpError::ProbeFailed
        })?;

        let dev_name = alloc_virtio_blk_dev_name();
        let block_dev = driver
            .into_block_dev(&dev_name)
            .map_err(|_| PnpError::ProbeFailed)?;

        dev.register_function(Arc::new(BlockFunction::with_devnode(
            &dev.name, &dev_name, block_dev,
        )))?;
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

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioPciBlkDriver::new()))
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
