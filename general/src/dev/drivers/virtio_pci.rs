//! VirtIO PCI 块设备驱动（modern VirtIO over PCI，VirtIO 1.0+）。
//!
//! 与 [`virtio_blk::VirtioBlk`](super::virtio_blk::VirtioBlk)(MMIO 版本)互补。
//! 本驱动通过 PCI capability list 定位 `common_cfg`/`notify_cfg`/`isr_cfg`/
//! `device_cfg` 四个能力,在 probe 时完成:
//!
//! 1. 读取 [`PciInfo`]，按 VirtIO PCI 传输层设备类型匹配 block function。
//! 2. 映射 BARs,把各 capability 偏移换算为寄存器虚拟地址。
//! 3. reset → ACKNOWLEDGE → DRIVER → negotiate features → FEATURES_OK →
//!    分配 DMA 页构造 virtqueue → DRIVER_OK。
//! 4. 按现有 [`virtio_blk`] 的 `VirtqDesc/VirtqAvail/VirtqUsed` 布局提交请求。
//! 5. 封装成 [`BlockIo`](crate::dev::block::BlockIo) 并通过
//!    [`PnpDevice::register_function`](crate::dev::pnp::PnpDevice::register_function)
//!    以 `/dev/vd*` 形式对外暴露。
//!
//! 内建注册入口只提交 factory；PCI host 初始化和总线扫描仍由启动路径负责。
//!
//! remove 路径把 device status 写 0，释放队列 DMA 页，`BlockDev` 的
//! `mark_gone` 由 PnP 框架统一处理。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem;
use core::num::NonZeroU32;
use core::ptr::read_volatile;
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
use crate::dev::irq::{self, IrqError, IrqHandler, IrqLine, IrqStatus};
use crate::dev::pci::{PciDevice, PciInfo, PciMsiPnpResource};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    PnpResourceKind, register_driver_factory,
};
use crate::dev::virtio::{
    SplitVirtQueue, VIRTIO_F_VERSION_1, VIRTIO_PCI_FUNCTION_BLOCK, VIRTIO_PCI_RESET_SPIN_LIMIT,
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FAILED,
    VIRTIO_STATUS_FEATURES_OK, VIRTQ_DESC_F_WRITE, VirtioPciTransport, choose_split_queue_size,
    parse_virtio_pci_caps,
};

// ── feature bits ───────────────────────────────────────────────────────

const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

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

// ── 队列状态 ────────────────────────────────────────────────────────────

struct VirtioBlkQueue {
    queue: SplitVirtQueue,
    /// 请求头/status 的小 DMA 缓冲复用池。
    ///
    /// 每个 BIO 都需要一段 header/status DMA 内存。该池只在设备发布 used ring
    /// 之后回收对应缓冲，避免与在途请求共享，同时减少 L1/L2 小 I/O 的分配成本。
    meta_pool: Vec<DmaBuffer>,
    /// descriptor head 到在途 BIO 的直接映射。
    ///
    /// L1/L2 块设备 bench 关注每个 I/O 的提交与完成成本。设备完成时已经返回 head，
    /// 因此用固定槽位表 O(1) 找回请求，避免在轮询/中断路径按队列深度线性扫描。
    pending: Vec<Option<PendingVirtioPciRequest>>,
    /// 队列协议错误后不再接受新请求，保持与 FAILED 设备状态一致。
    failed: bool,
}

struct PendingVirtioPciRequest {
    bio: Bio,
    meta_dma: DmaBuffer,
    data_dma: Option<DmaBuffer>,
}

// Safety: DMA 指针由 Mutex 串行化;没有共享可变别名。
unsafe impl Send for VirtioBlkQueue {}
unsafe impl Sync for VirtioBlkQueue {}

impl VirtioBlkQueue {
    fn new(queue: SplitVirtQueue) -> Self {
        let mut pending = Vec::with_capacity(usize::from(queue.queue_size()));
        pending.resize_with(usize::from(queue.queue_size()), || None);
        let meta_pool = Vec::with_capacity(usize::from(queue.queue_size()));
        Self {
            queue,
            meta_pool,
            pending,
            failed: false,
        }
    }

    fn take_meta_dma(&mut self) -> Option<DmaBuffer> {
        self.meta_pool.pop()
    }

    fn recycle_meta_dma(&mut self, meta_dma: DmaBuffer) {
        if self.meta_pool.len() < usize::from(self.queue.queue_size()) {
            self.meta_pool.push(meta_dma);
        }
    }

    fn take_pending(&mut self, head: u16) -> Option<PendingVirtioPciRequest> {
        self.pending
            .get_mut(usize::from(head))
            .and_then(Option::take)
    }

    fn set_pending(&mut self, head: u16, pending: PendingVirtioPciRequest) {
        let slot = self
            .pending
            .get_mut(usize::from(head))
            .expect("virtio pci block pending head out of range");
        debug_assert!(slot.is_none());
        *slot = Some(pending);
    }

    fn mark_failed_and_take_pending(&mut self) -> Vec<Option<PendingVirtioPciRequest>> {
        self.failed = true;
        let mut failed = Vec::new();
        mem::swap(&mut failed, &mut self.pending);
        failed
    }
}

// ── 驱动主结构 ──────────────────────────────────────────────────────────

struct VirtioBlkInner {
    transport: VirtioPciTransport,
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
        self.inner.transport.set_status(0);
    }
}

// ── 初始化序列 ─────────────────────────────────────────────────────────

impl VirtioBlkPci {
    /// 在已绑定 PCI capabilities 的前提下完成 VirtIO 1.0+ probe 流程。
    pub fn probe(pci: &PciDevice) -> Result<Self, &'static str> {
        // 先打开 bus master + memory space decode —— 没这两个 BAR 根本不响应。
        pci.try_enable_mmio()
            .map_err(|_| "virtio-pci: failed to enable MMIO decode")?;
        pci.try_enable_bus_master()
            .map_err(|_| "virtio-pci: failed to enable bus master")?;

        let raw_caps = parse_virtio_pci_caps(pci).ok_or("virtio-pci: missing VIRTIO caps")?;
        let transport =
            VirtioPciTransport::new(raw_caps).map_err(|_| "virtio-pci: invalid VIRTIO caps")?;
        let caps = transport.caps();
        log::printk!(
            "[virtio-pci] caps: common vaddr={:#x} notify vaddr={:#x} mult={} device={}",
            caps.common.vaddr,
            caps.notify.vaddr,
            caps.notify.notify_off_multiplier,
            caps.device.is_some()
        );

        // 1. reset
        if !transport.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT) {
            log::printk!(
                "[virtio-pci] reset stuck: status still {:#x} after spin",
                transport.status()
            );
            return Err("virtio-pci: reset timeout");
        }

        // 2. ACKNOWLEDGE + DRIVER
        transport.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
        transport.add_status(VIRTIO_STATUS_DRIVER);

        // 3. 协商 feature
        let device_features = transport.device_features();
        log::printk!(
            "[virtio-pci] device_features={:#x} (status={:#x})",
            device_features,
            transport.status()
        );
        if device_features & VIRTIO_F_VERSION_1 == 0 {
            transport.set_status(VIRTIO_STATUS_FAILED);
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
        transport.set_driver_features(driver_features);
        transport.add_status(VIRTIO_STATUS_FEATURES_OK);
        if transport.status() & VIRTIO_STATUS_FEATURES_OK == 0 {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-pci: FEATURES_OK rejected");
        }

        // 4. 读设备配置(capacity / block_size)
        let device_cap = caps
            .device
            .ok_or("virtio-pci: missing device_cfg capability")?;
        if !device_cap.covers(BLK_CFG_CAPACITY, mem::size_of::<u64>()) {
            return Err("virtio-pci: device_cfg capacity out of range");
        }
        let capacity = unsafe {
            let lo = read_volatile((device_cap.vaddr + BLK_CFG_CAPACITY) as *const u32) as u64;
            let hi = read_volatile((device_cap.vaddr + BLK_CFG_CAPACITY + 4) as *const u32) as u64;
            (hi << 32) | lo
        };
        let block_size = if driver_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            if !device_cap.covers(BLK_CFG_BLK_SIZE, mem::size_of::<u32>()) {
                return Err("virtio-pci: device_cfg block size out of range");
            }
            unsafe { read_volatile((device_cap.vaddr + BLK_CFG_BLK_SIZE) as *const u32) }
        } else {
            VIRTIO_BLK_SECTOR_SIZE
        };
        if block_size < VIRTIO_BLK_SECTOR_SIZE
            || !block_size.is_power_of_two()
            || !block_size.is_multiple_of(VIRTIO_BLK_SECTOR_SIZE)
        {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-pci: invalid block size");
        }

        // 5. 设置队列 0
        transport.select_queue(0);
        let max_qsz = transport.selected_queue_size();
        if max_qsz == 0 {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-pci: queue 0 size is zero");
        }
        let qsz =
            choose_split_queue_size(max_qsz, None).map_err(|_| "virtio-pci: invalid queue size")?;
        if qsz < VIRTIO_BLK_MIN_QUEUE_SIZE {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-pci: queue 0 too small");
        }
        transport.set_selected_queue_size(qsz);

        let dma_context = pci.dma_context();
        let split_queue = SplitVirtQueue::new_in(dma_context, qsz)
            .map_err(|_| "virtio-pci: queue allocation failed")?;

        // 写入设备可见的 queue_desc/driver/device DMA 地址。
        transport.set_selected_queue_addresses(
            split_queue.desc_dma_addr() as u64,
            split_queue.avail_dma_addr() as u64,
            split_queue.used_dma_addr() as u64,
        );

        let notify_addr = transport
            .selected_queue_notify_addr()
            .map_err(|_| "virtio-pci: notify address invalid")?;

        // 启用队列
        transport.enable_selected_queue();

        // 6. DRIVER_OK
        transport.add_status(VIRTIO_STATUS_DRIVER_OK);

        let read_only = driver_features & VIRTIO_BLK_F_RO != 0;
        let has_flush = driver_features & VIRTIO_BLK_F_FLUSH != 0;

        let queue = VirtioBlkQueue::new(split_queue);

        let inner = Arc::new(VirtioBlkInner {
            transport,
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

    fn complete_failed_requests(pending: Vec<Option<PendingVirtioPciRequest>>, error: BioIoError) {
        for pending in pending.into_iter().flatten() {
            pending.bio.complete(Err(error));
        }
    }

    fn fail_queue_locked(
        &self,
        queue: &mut VirtioBlkQueue,
        reason: &'static str,
    ) -> Vec<Option<PendingVirtioPciRequest>> {
        log::printk!("[virtio-pci-blk] queue failed: {}", reason);
        self.inner
            .transport
            .set_status(self.inner.transport.status() | VIRTIO_STATUS_FAILED);
        queue.mark_failed_and_take_pending()
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
            if let Some(mut pending) = queue.take_pending(desc_head) {
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
                queue.recycle_meta_dma(pending.meta_dma);

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

    fn handle_interrupt(&self) -> bool {
        let isr_status = self.inner.transport.isr_status();
        if isr_status == 0 {
            return false;
        }
        self.inner.irq_count.fetch_add(1, Ordering::Relaxed);
        self.poll();
        true
    }

    pub fn into_block_dev(self, name: &str) -> Result<(Arc<BlockDevice>, Arc<Self>), &'static str> {
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
        let driver = Arc::new(self);
        let io = Arc::new(VirtioBlkPciIo {
            driver: Arc::clone(&driver),
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
        Ok((Arc::new(BlockDevice::new(init, io, None)), driver))
    }
}

struct VirtioBlkPciIrqHandler {
    driver: Arc<VirtioBlkPci>,
}

impl IrqHandler for VirtioBlkPciIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.driver.handle_interrupt() {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

// ── BlockIo 实现 ────────────────────────────────────────────────────────

struct VirtioBlkPciIo {
    driver: Arc<VirtioBlkPci>,
}

impl VirtioBlkPciIo {
    fn notify_queue(&self) {
        self.driver
            .inner
            .transport
            .notify_queue(self.driver.inner.notify_addr, 0);
    }
}

impl BlockDriver for VirtioBlkPciIo {
    fn queue_bio(&self, bio: Bio) -> Result<(), (SubmitError, Bio)> {
        self.driver.poll();
        let mut queue = self.driver.inner.queue.lock();
        if queue.failed {
            return Err((SubmitError::DeviceGone, bio));
        }

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
        let dma_context = queue.queue.dma_context();

        let meta_dma = match queue.take_meta_dma() {
            Some(buffer) => buffer,
            None => match DmaBuffer::new_in(
                dma_context,
                mem::size_of::<VirtioBlkReqMeta>(),
                mem::align_of::<VirtioBlkReqMeta>(),
                DmaDirection::Bidirectional,
            ) {
                Ok(buffer) => buffer,
                Err(_) => {
                    let _ = queue.queue.free_chain(chain);
                    return Err((SubmitError::OutOfMemory, bio));
                }
            },
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
                let mut dma = match DmaBuffer::new_in(dma_context, data_len, 1, direction) {
                    Ok(buffer) => buffer,
                    Err(_) => {
                        let _ = queue.queue.free_chain(chain);
                        queue.recycle_meta_dma(meta_dma);
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
                queue.recycle_meta_dma(meta_dma);
                return Err((SubmitError::Unsupported, bio));
            }
        };

        let header_dma = meta_dma.dma_addr() as u64;
        let status_dma = meta_dma.dma_addr() as u64 + mem::size_of::<VirtioBlkReqHeader>() as u64;
        let head_idx = chain.head();

        match bio.op {
            BioOp::Read => {
                let Some(data_dma) = data_dma.as_ref() else {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                let (Some(d0), Some(d1), Some(d2)) = (chain.get(0), chain.get(1), chain.get(2))
                else {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                if queue
                    .queue
                    .write_desc(
                        d0,
                        header_dma,
                        mem::size_of::<VirtioBlkReqHeader>() as u32,
                        0,
                        Some(d1),
                    )
                    .and_then(|_| {
                        queue.queue.write_desc(
                            d1,
                            data_dma.dma_addr() as u64,
                            data_len_u32,
                            VIRTQ_DESC_F_WRITE,
                            Some(d2),
                        )
                    })
                    .and_then(|_| {
                        queue
                            .queue
                            .write_desc(d2, status_dma, 1, VIRTQ_DESC_F_WRITE, None)
                    })
                    .is_err()
                {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((SubmitError::QueueFull, bio));
                }
            }
            BioOp::Write => {
                let Some(data_dma) = data_dma.as_ref() else {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                let (Some(d0), Some(d1), Some(d2)) = (chain.get(0), chain.get(1), chain.get(2))
                else {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                if queue
                    .queue
                    .write_desc(
                        d0,
                        header_dma,
                        mem::size_of::<VirtioBlkReqHeader>() as u32,
                        0,
                        Some(d1),
                    )
                    .and_then(|_| {
                        queue.queue.write_desc(
                            d1,
                            data_dma.dma_addr() as u64,
                            data_len_u32,
                            0,
                            Some(d2),
                        )
                    })
                    .and_then(|_| {
                        queue
                            .queue
                            .write_desc(d2, status_dma, 1, VIRTQ_DESC_F_WRITE, None)
                    })
                    .is_err()
                {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((SubmitError::QueueFull, bio));
                }
            }
            BioOp::Flush => {
                let (Some(d0), Some(d1)) = (chain.get(0), chain.get(1)) else {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                };
                if queue
                    .queue
                    .write_desc(
                        d0,
                        header_dma,
                        mem::size_of::<VirtioBlkReqHeader>() as u32,
                        0,
                        Some(d1),
                    )
                    .and_then(|_| {
                        queue
                            .queue
                            .write_desc(d1, status_dma, 1, VIRTQ_DESC_F_WRITE, None)
                    })
                    .is_err()
                {
                    let _ = queue.queue.free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err((SubmitError::QueueFull, bio));
                }
            }
            _ => {
                let _ = queue.queue.free_chain(chain);
                queue.recycle_meta_dma(meta_dma);
                return Err((SubmitError::Unsupported, bio));
            }
        }

        queue.set_pending(
            head_idx,
            PendingVirtioPciRequest {
                bio,
                meta_dma,
                data_dma,
            },
        );

        // 先登记 pending，再发布到 available ring；设备完成时按同一 head O(1) 找回 BIO。
        if queue.queue.push_avail(head_idx).is_err() {
            let pending = queue
                .take_pending(head_idx)
                .expect("virtio pci block pending disappeared before publish failure");
            let PendingVirtioPciRequest {
                bio,
                meta_dma,
                data_dma: _,
            } = pending;
            let _ = queue.queue.free_chain(chain);
            queue.recycle_meta_dma(meta_dma);
            return Err((SubmitError::QueueFull, bio));
        }
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
/// 匹配 VirtIO PCI block function；具体 vendor/device id 由 VirtIO 公共层维护。
pub struct VirtioPciBlkDriver {}

struct VirtioPciBlkBinding {
    irq: Option<VirtioPciIrqRegistration>,
}

#[derive(Clone, Copy)]
struct VirtioPciIrqRegistration {
    using_msi: bool,
}

impl VirtioPciBlkDriver {
    /// 创建 VirtIO-PCI block PnP 驱动。
    pub const fn new() -> Self {
        Self {}
    }
}

fn map_irq_error(err: IrqError) -> &'static str {
    match err {
        IrqError::OutOfMemory => "out of memory",
        IrqError::NotFound => "not found",
        IrqError::AlreadyRegistered => "already registered",
    }
}

fn register_virtio_pci_irq(
    dev: &Arc<PnpDevice>,
    pci: &PciDevice,
    driver: Arc<VirtioBlkPci>,
) -> Result<Option<VirtioPciIrqRegistration>, PnpError> {
    let handler: Arc<dyn IrqHandler> = Arc::new(VirtioBlkPciIrqHandler { driver });

    if let Ok(msi_handle) = pci.try_configure_single_msi() {
        let line = msi_handle.line();
        match irq::register_irq_handler(line, Arc::clone(&handler)) {
            Ok(irq_handle) => {
                if pci.try_enable_configured_msi(msi_handle).is_ok() {
                    // MSI 已启用；同时屏蔽 INTx，避免同一设备双路上报。
                    pci.disable_interrupts();
                    if let Err(err) = dev.own_resource(PciMsiPnpResource::new(
                        pci.clone(),
                        msi_handle,
                        "virtio-pci-blk-msi",
                    )) {
                        let _ = irq::unregister_irq_handler(irq_handle);
                        pci.release_configured_msi(msi_handle);
                        return Err(err);
                    }
                    if let Err(err) = dev.own_resource(irq::irq_handler_pnp_resource(
                        irq_handle,
                        "virtio-pci-blk-msi-irq",
                    )) {
                        let _ = irq::unregister_irq_handler(irq_handle);
                        return Err(err);
                    }
                    return Ok(Some(VirtioPciIrqRegistration { using_msi: true }));
                }
                let _ = irq::unregister_irq_handler(irq_handle);
                pci.release_configured_msi(msi_handle);
            }
            Err(err) => {
                log::printk!(
                    "[virtio-pci] failed to register MSI irq {:?}: {}",
                    line,
                    map_irq_error(err)
                );
                pci.release_configured_msi(msi_handle);
            }
        }
    }

    let Some(line) = pci.routed_irq_line() else {
        pci.disable_interrupts();
        return Ok(None);
    };
    match irq::register_irq_handler(line, handler) {
        Ok(handle) => {
            pci.enable_interrupts();
            if let Err(err) =
                dev.own_resource(irq::irq_handler_pnp_resource(handle, "virtio-pci-blk-intx"))
            {
                let _ = irq::unregister_irq_handler(handle);
                pci.disable_interrupts();
                return Err(err);
            }
            Ok(Some(VirtioPciIrqRegistration { using_msi: false }))
        }
        Err(err) => {
            log::printk!(
                "[virtio-pci] failed to register irq {:?}: {}",
                line,
                map_irq_error(err)
            );
            pci.disable_interrupts();
            Ok(None)
        }
    }
}

fn unregister_virtio_pci_irq(pci: &PciDevice, registration: VirtioPciIrqRegistration) {
    if !registration.using_msi {
        pci.disable_interrupts();
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
        VIRTIO_PCI_FUNCTION_BLOCK.matches_pci_ids(pci_info.vendor, pci_info.device_id)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(dev).ok_or(PnpError::InvalidState)?;

        let driver = VirtioBlkPci::probe(&pci).map_err(|msg| {
            log::printk!("[virtio-pci] probe failed: {}", msg);
            PnpError::hardware_failure("virtio-pci block init failed")
        })?;

        let dev_name = alloc_virtio_blk_dev_name(&dev.name)?;
        let (block_dev, driver) = driver.into_block_dev(&dev_name).map_err(|_| {
            PnpError::registration_failed(PnpResourceKind::Function, "block function")
        })?;
        let irq = register_virtio_pci_irq(dev, &pci, Arc::clone(&driver))?;

        let func = Arc::new(BlockFunction::with_devnode(&dev.name, &dev_name, block_dev));
        if let Err(err) = dev.register_function(func) {
            if let Some(registration) = irq {
                unregister_virtio_pci_irq(&pci, registration);
            }
            return Err(err);
        }
        dev.set_driver_data(Arc::new(VirtioPciBlkBinding { irq }));
        log::printk!("[virtio-pci] bound {} → /dev/{}", dev.id, dev_name);
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = Arc::downcast::<VirtioPciBlkBinding>(data)
        {
            if let Some(registration) = binding.irq {
                if let Some(pci) = PciDevice::from_pnp(dev) {
                    unregister_virtio_pci_irq(&pci, registration);
                }
            }
        }
        // VirtioBlkPci 的 Drop 会 reset device；PnpDevice::remove_device 在
        // function drain 之后调用本 remove，因此这里先撤销 IRQ 入口，再让
        // 设备对象按引用计数自然释放。
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
