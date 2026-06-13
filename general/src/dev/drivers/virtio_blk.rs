//! VirtIO MMIO 块设备驱动。
//!
//! 实现 VirtIO 1.0 规范中的 MMIO 传输层块设备，支持：
//! - MMIO 传输层
//! - 异步 I/O 完成回调
//! - 读、写、flush 操作
//! - 多队列支持（当前实现单队列）
//!
//! PnP 适配层只负责匹配固件枚举的 `virtio,mmio` / `LNRO0005` 设备，并把成功
//! 初始化的块设备封装成通用 function 注册给设备 core。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;
use core::mem;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::mutex::Mutex;

use super::{VIRTIO_BLK_SECTOR_SIZE, alloc_virtio_blk_dev_name, virtio_blk_limits};
use crate::dev::bio::{Bio, BioBuffer, BioIoError, BioOp, BioReqError, SubmitError};
use crate::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures,
    BlockGeometry,
};
use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use crate::dev::function::BlockFunction;
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    PnpResourceKind, register_driver_factory,
};
use crate::dev::virtio::{SplitVirtQueue, VIRTQ_DESC_F_WRITE, choose_split_queue_size};
use crate::dev::virtio_mmio::{
    self, VirtioMmioTransport,
    VIRTIO_F_VERSION_1,
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER,
    VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FEATURES_OK, VIRTIO_STATUS_FAILED,
};

// VirtIO Block 特性位
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

// VirtIO Block 设备配置空间偏移（相对于 0x100）
const BLK_CFG_CAPACITY: usize = 0x100;
const BLK_CFG_BLK_SIZE: usize = 0x114;

// VirtIO Block 请求类型
const VIRTIO_BLK_T_IN: u32 = 0; // Read
const VIRTIO_BLK_T_OUT: u32 = 1; // Write
const VIRTIO_BLK_T_FLUSH: u32 = 4; // Flush

// VirtIO Block 请求状态
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;
/// virtio-blk 普通 I/O 至少需要 header/data/status 三个描述符，实际队列不能小于 4。
const VIRTIO_BLK_MIN_QUEUE_SIZE: u16 = 4;

// ───────── VirtIO 数据结构 ─────────

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

struct PendingVirtioRequest {
    head: u16,
    bio: Bio,
    meta_dma: DmaBuffer,
    data_dma: Option<DmaBuffer>,
}

// ───────── 驱动内部状态 ─────────

struct VirtioBlkQueue {
    /// 公共 split virtqueue 负责 DMA 布局、描述符状态和 ring 索引维护。
    queue: SplitVirtQueue,
    /// 待处理请求队列。
    pending: VecDeque<PendingVirtioRequest>,
}

// Safety: VirtioBlkQueue 的裸指针指向 DMA 内存，由 Mutex 保护并发访问
unsafe impl Send for VirtioBlkQueue {}
unsafe impl Sync for VirtioBlkQueue {}

struct VirtioBlkInner {
    transport: Box<dyn VirtioMmioTransport>,
    /// 设备容量（扇区数）
    capacity: u64,
    /// 逻辑块大小
    block_size: u32,
    /// 队列
    queue: Mutex<VirtioBlkQueue>,
    /// 中断计数（用于轮询模式）
    irq_count: AtomicUsize,
}

pub struct VirtioBlk {
    inner: Arc<VirtioBlkInner>,
}

impl Drop for VirtioBlk {
    fn drop(&mut self) {
        self.inner.transport.write_status(0);
    }
}

// ───────── 驱动初始化 ─────────

impl VirtioBlk {
    /// 创建并初始化 VirtIO Block 设备驱动
    ///
    /// # 参数
    /// - `mmio_base`: MMIO 寄存器基地址（虚拟地址）
    ///
    /// # 返回
    /// 成功时返回驱动实例，失败时返回错误信息
    pub fn new(transport: Box<dyn VirtioMmioTransport>, dma_context: DmaContext) -> Result<Self, &'static str> {
        let is_legacy = transport.is_legacy();

        // 1. 验证 Device ID
        let device_id = unsafe { transport.read_reg(0x008) };
        if device_id != 2 {
            return Err("Not a VirtIO block device");
        }

        // 2. 重置设备
        transport.write_status(0);

        // 3. 设置 ACKNOWLEDGE 和 DRIVER 状态位
        transport.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
        transport.add_status(VIRTIO_STATUS_DRIVER);

        // 4. 协商特性
        let device_features = transport.read_device_features();
        let mut driver_features = 0u64;

        if !is_legacy {
            if device_features & VIRTIO_F_VERSION_1 == 0 {
                transport.write_status(VIRTIO_STATUS_FAILED);
                return Err("VirtIO 1.0 VERSION_1 feature is missing");
            }
            driver_features |= VIRTIO_F_VERSION_1;
        }

        if device_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            driver_features |= VIRTIO_BLK_F_BLK_SIZE;
        }
        if device_features & VIRTIO_BLK_F_FLUSH != 0 {
            driver_features |= VIRTIO_BLK_F_FLUSH;
        }
        if device_features & VIRTIO_BLK_F_RO != 0 {
            driver_features |= VIRTIO_BLK_F_RO;
        }

        transport.write_driver_features(driver_features);
        transport.add_status(VIRTIO_STATUS_FEATURES_OK);

        let status = transport.read_status();
        if status & VIRTIO_STATUS_FEATURES_OK == 0 {
            transport.write_status(VIRTIO_STATUS_FAILED);
            return Err("Feature negotiation failed");
        }

        // 5. 读取设备配置
        let capacity = unsafe { transport.read_reg64(BLK_CFG_CAPACITY) };
        let block_size = if driver_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            unsafe { transport.read_reg(BLK_CFG_BLK_SIZE) }
        } else {
            VIRTIO_BLK_SECTOR_SIZE
        };
        if block_size < VIRTIO_BLK_SECTOR_SIZE
            || !block_size.is_power_of_two()
            || !block_size.is_multiple_of(VIRTIO_BLK_SECTOR_SIZE)
        {
            transport.write_status(VIRTIO_STATUS_FAILED);
            return Err("Invalid VirtIO block size");
        }

        // 6. 设置队列
        transport.select_queue(0);
        let max_queue_size = transport.read_queue_max_size() as u16;
        if max_queue_size == 0 {
            transport.write_status(VIRTIO_STATUS_FAILED);
            return Err("Queue size is zero");
        }
        let queue_size = choose_split_queue_size(max_queue_size, None)
            .map_err(|_| "Invalid VirtIO queue size")?;
        if queue_size < VIRTIO_BLK_MIN_QUEUE_SIZE {
            transport.write_status(VIRTIO_STATUS_FAILED);
            return Err("VirtIO queue size is too small");
        }
        let split_queue = if is_legacy {
            SplitVirtQueue::new_legacy(queue_size)
                .map_err(|_| "Failed to allocate VirtIO queue")?
        } else {
            SplitVirtQueue::new_in(dma_context, queue_size)
                .map_err(|_| "Failed to allocate VirtIO queue")?
        };

        transport.write_queue_size(queue_size as u32);
        transport.configure_queue_addresses(
            split_queue.desc_dma_addr() as u64,
            split_queue.avail_dma_addr() as u64,
            split_queue.used_dma_addr() as u64,
        );
        transport.enable_queue();

        // 7. 设置 DRIVER_OK
        transport.add_status(VIRTIO_STATUS_DRIVER_OK);

        let inner = Arc::new(VirtioBlkInner {
            transport,
            capacity,
            block_size,
            queue: Mutex::new(VirtioBlkQueue {
                queue: split_queue,
                pending: VecDeque::new(),
            }),
            irq_count: AtomicUsize::new(0),
        });

        Ok(Self { inner })
    }

    fn complete_failed_requests(mut pending: VecDeque<PendingVirtioRequest>, error: BioIoError) {
        while let Some(pending) = pending.pop_front() {
            pending.bio.complete(Err(error));
        }
    }

    fn fail_queue_locked(
        &self,
        queue: &mut VirtioBlkQueue,
        reason: &'static str,
    ) -> VecDeque<PendingVirtioRequest> {
        log::printk!("[virtio-mmio-blk] queue failed: {}", reason);
        let status = self.inner.transport.read_status();
        self.inner.transport.write_status(status | VIRTIO_STATUS_FAILED);

        let mut failed = VecDeque::new();
        mem::swap(&mut failed, &mut queue.pending);
        failed
    }

    /// 轮询并处理已完成的请求
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
            // 查找对应的待处理请求
            let Some(pos) = queue
                .pending
                .iter()
                .position(|pending| pending.head == desc_head)
            else {
                log::printk!(
                    "[virtio-mmio-blk] used head {} has no pending request",
                    desc_head
                );
                if queue.queue.free_chain_from_head(desc_head).is_err() {
                    let failed =
                        self.fail_queue_locked(&mut queue, "unknown used head cannot be freed");
                    drop(queue);
                    Self::complete_failed_requests(failed, BioIoError::Unavailable);
                    return;
                }
                continue;
            };
            let PendingVirtioRequest {
                mut bio,
                meta_dma,
                data_dma,
                ..
            } = match queue.pending.remove(pos) {
                Some(pending) => pending,
                None => continue,
            };
            core::sync::atomic::fence(Ordering::Acquire);
            meta_dma.sync_for_cpu();
            let status = unsafe {
                let meta = &*(meta_dma.vaddr() as *const VirtioBlkReqMeta);
                meta.status
            };
            let result = match status {
                VIRTIO_BLK_S_OK => Ok(()),
                VIRTIO_BLK_S_UNSUPP => Err(BioIoError::Unsupported),
                _ => Err(BioIoError::MediaError),
            };
            // 读请求成功时把 DMA 区数据回拷到 Bio buffer
            if result.is_ok() && bio.op == BioOp::Read {
                if let (BioBuffer::Owned(buf), Some(dma)) = (&mut bio.buffer, data_dma.as_ref()) {
                    dma.sync_for_cpu();
                    let take = buf.len().min(dma.as_slice().len());
                    buf[..take].copy_from_slice(&dma.as_slice()[..take]);
                }
            }

            if queue.queue.free_chain_from_head(desc_head).is_err() {
                let failed =
                    self.fail_queue_locked(&mut queue, "completed descriptor chain corrupt");
                drop(queue);
                bio.complete(Err(BioIoError::Unavailable));
                Self::complete_failed_requests(failed, BioIoError::Unavailable);
                return;
            }

            // 释放 queue 锁后再 complete bio——避免 completion 路径
            // （包括 Waker::wake 和 WaitQueue::wake_all）持队列锁重入。
            drop(queue);
            bio.complete(result);
            queue = self.inner.queue.lock();
        }
    }

    /// 处理中断（由中断处理程序调用）
    pub fn handle_interrupt(&self) {
        // 确认中断
        let status = self.inner.transport.read_interrupt_status();
        self.inner.transport.acknowledge_interrupt(status);
        self.inner.irq_count.fetch_add(1, Ordering::Relaxed);

        // 轮询完成的请求
        self.poll();
    }

    /// 创建 BlockDev 包装
    pub fn into_block_dev(self, name: &str) -> Result<Arc<BlockDevice>, &'static str> {
        let capacity = self.inner.capacity;
        let block_size = self.inner.block_size;
        let sector_scale = u64::from(block_size / VIRTIO_BLK_SECTOR_SIZE);
        if sector_scale == 0 || capacity % sector_scale != 0 {
            return Err("Invalid capacity for logical block size");
        }
        let logical_blocks = capacity / sector_scale;
        if logical_blocks == 0 {
            return Err("Invalid capacity");
        }

        let logical_size = NonZeroU32::new(block_size).ok_or("Invalid block size")?;
        let geometry = BlockGeometry::new(logical_size, logical_size, Some(logical_blocks))
            .ok_or("Invalid geometry")?;

        let limits = virtio_blk_limits(block_size);
        let queue_depth = u32::from(self.inner.queue.lock().queue.queue_size());
        let attributes = BlockAttributes::new(false, false, NonZeroU32::new(queue_depth), None);

        let mut features = BlockFeatures(0);
        if self.inner.transport.read_device_features() & VIRTIO_BLK_F_FLUSH != 0 {
            features |= BlockFeatures::FLUSH;
        }
        if self.inner.transport.read_device_features() & VIRTIO_BLK_F_RO != 0 {
            features |= BlockFeatures::READ_ONLY;
        }

        let io = Arc::new(VirtioBlkIo {
            driver: Arc::new(self),
        });

        Ok(Arc::new(BlockDevice::new(
            BlockDeviceInit {
                name,
                subsystem: "virtio-blk",
                class: BlockClass::Whole,
                geometry,
                limits,
                attributes,
                features,
            },
            io,
            None,
        )))
    }
}

// ───────── BlockDriver 实现 ─────────

/// VirtIO-MMIO 块设备的 [`BlockDriver`] 包装。
///
/// 接受 `Bio` 请求，构造 VirtIO 描述符链，提交后立即返回；完成由 `poll`
/// 路径触发 `bio.complete(...)`。
struct VirtioBlkIo {
    driver: Arc<VirtioBlk>,
}

impl BlockDriver for VirtioBlkIo {
    fn queue_bio(&self, bio: Bio) -> Result<(), (SubmitError, Bio)> {
        // 进来先尝试 drain 一下硬件已完成的请求，给后面的提交腾描述符。
        self.driver.poll();
        let mut queue = self.driver.inner.queue.lock();

        // 根据请求类型确定描述符数量
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

        // 元数据 DMA 缓冲区（请求头 + 状态字节），方向设为双向以覆盖设备写 status。
        let meta_dma = match DmaBuffer::new_in(
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

        // 数据 DMA 缓冲区（仅 Read/Write 需要）
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
                        return Err((SubmitError::OutOfMemory, bio));
                    }
                };
                if bio.op == BioOp::Write {
                    dma.as_mut_slice().copy_from_slice(bio.buffer.as_slice());
                }
                dma.sync_for_device();
                Some(dma)
            }
            _ => None,
        };

        let header_dma = meta_dma.dma_addr() as u64;
        let status_dma = meta_dma.dma_addr() as u64 + mem::size_of::<VirtioBlkReqHeader>() as u64;
        let head_idx = chain.head();

        // 构造描述符链
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
                    return Err((SubmitError::QueueFull, bio));
                }
            }
            _ => {
                let _ = queue.queue.free_chain(chain);
                return Err((SubmitError::Unsupported, bio));
            }
        }

        // 提交到设备
        if queue.queue.push_avail(head_idx).is_err() {
            let _ = queue.queue.free_chain(chain);
            return Err((SubmitError::QueueFull, bio));
        }
        queue.pending.push_back(PendingVirtioRequest {
            head: head_idx,
            bio,
            meta_dma,
            data_dma,
        });

        // 通知设备
        drop(queue);
        self.driver.inner.transport.notify_queue(0);
        Ok(())
    }

    fn drain(&self) {
        self.driver.poll();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ───────── Platform PnP 绑定 ─────────

/// VirtIO-MMIO platform PnP 驱动。
///
/// probe 时根据固件资源读取 MMIO 基址，初始化 VirtIO 队列，并把块设备包装成
/// 通用 function 注册到设备 core。
pub struct VirtioMmioBlkDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl VirtioMmioBlkDriver {
    /// 创建 VirtIO-MMIO PnP 驱动。
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id("virtio,mmio") || info.has_id("LNRO0005")
    }
}

impl PnpDriver for VirtioMmioBlkDriver {
    fn name(&self) -> &'static str {
        "platform-virtio-mmio-blk"
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
        let Some((phys, _size)) = info.first_mmio() else {
            return Err(PnpError::missing(
                PnpResourceKind::Mmio,
                "virtio-mmio reg missing",
            ));
        };
        let virt_base = (self.device_mmio_to_virt)(phys);
        let transport = virtio_mmio::detect(virt_base).map_err(|msg| {
            log::printk!("[platform-virtio-mmio-blk] probe failed: {}", msg);
            PnpError::HardwareFailure { detail: "virtio-mmio block init failed" }
        })?;
        let driver = VirtioBlk::new(transport, info.dma_context()).map_err(|msg| {
            log::printk!("[platform-virtio-mmio-blk] probe failed: {}", msg);
            PnpError::hardware_failure("virtio-mmio block init failed")
        })?;
        let dev_name = alloc_virtio_blk_dev_name(&dev.name)?;
        let block_dev = driver.into_block_dev(&dev_name).map_err(|_| {
            PnpError::registration_failed(PnpResourceKind::Function, "block function")
        })?;
        dev.register_function(Arc::new(BlockFunction::with_devnode(
            &dev.name, &dev_name, block_dev,
        )))?;
        log::printk!(
            "[platform-virtio-mmio-blk] bound {} phys={:#x} -> /dev/{}",
            dev.id,
            phys,
            dev_name
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        log::printk!("[platform-virtio-mmio-blk] removed {}", dev.id);
    }
}

struct VirtioMmioBlkFactory;

impl DriverFactory for VirtioMmioBlkFactory {
    fn name(&self) -> &'static str {
        "platform-virtio-mmio-blk"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioMmioBlkDriver::new(ctx.device_mmio_to_virt)))
    }
}

/// 注册 VirtIO-MMIO block 内建驱动 factory。
pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(VirtioMmioBlkFactory)).map(|_| ())
}
