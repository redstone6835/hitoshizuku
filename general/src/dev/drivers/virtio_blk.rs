//! VirtIO MMIO 块设备驱动。
//!
//! 实现 VirtIO 1.0 规范中的 MMIO 传输层块设备，支持：
//! - MMIO 传输层
//! - 异步 I/O 完成回调
//! - 读、写、flush、discard、write-zeroes 请求规划
//! - typed I/O queue 身份，后续多队列策略可以在不改提交协议的情况下接入
//!
//! PnP 适配层只负责匹配固件枚举的 `virtio,mmio` / `LNRO0005` 设备，并把成功
//! 初始化的块设备封装成通用 function 注册给设备 core。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::mem;
use core::num::NonZeroU32;
use core::ptr::read_volatile;
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::mutex::Mutex;

use super::virtio_block_common::{
    DmaBufferPool, DmaBufferPoolProfile, MIN_QUEUE_SIZE as VIRTIO_BLK_MIN_QUEUE_SIZE,
    VirtioBlkAllocatedRequest, VirtioBlkCapabilities, VirtioBlkConfigReader, VirtioBlkDmaQueue,
    VirtioBlkQueueId, VirtioBlkReqMeta, VirtioBlkRequestPlan, allocate_request, block_limits,
    free_allocated_request, negotiate_supported_features, read_device_config, status_to_result,
    validate_bio_buffer_for_plan, validate_used_write_len, write_allocated_request_descriptors,
    write_data_payload,
};
use super::{VIRTIO_BLK_SECTOR_SIZE, alloc_virtio_blk_dev_name};
use crate::dev::bio::{Bio, BioBuffer, BioIoError, BioOp, SubmitError};
use crate::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockGeometry,
};
use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use crate::dev::function::BlockFunction;
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    PnpResourceKind, register_driver_factory,
};
use crate::dev::virtio::{SplitVirtQueue, choose_split_queue_size};
use crate::dev::virtio_mmio::{
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FAILED,
    VIRTIO_STATUS_FEATURES_OK, VirtioMmioTransport, detect as detect_virtio_mmio,
};

// ───────── VirtIO MMIO 寄存器布局 ─────────

const VIRTIO_MMIO_MAGIC_VALUE: u32 = 0x74726976; // "virt"
const VIRTIO_MMIO_DEVICE_ID_BLOCK: u32 = 2;

// MMIO 寄存器偏移
const MMIO_MAGIC: usize = 0x000;
const MMIO_VERSION: usize = 0x004;
const MMIO_DEVICE_ID: usize = 0x008;

// MMIO 传输层中设备类型 config 的起始偏移；字段语义由 virtio-blk 公共层解析。
const BLK_CFG_BASE: usize = 0x100;

// ───────── VirtIO 数据结构 ─────────

struct PendingVirtioRequest {
    bio: Bio,
    meta_dma: DmaBuffer,
    data_dma: Option<DmaBuffer>,
    /// 设备完成时至少应写回的字节数，用于发现 used ring 短写。
    expected_device_write_len: u32,
}

// ───────── 驱动内部状态 ─────────

struct VirtioBlkQueue {
    /// 公共 split virtqueue 负责 DMA 布局、描述符状态和 ring 索引维护。
    queue: SplitVirtQueue,
    /// 请求头/status 的小 DMA 缓冲复用池。
    ///
    /// L1/L2 小 I/O 会把每次 BIO 的元数据 DMA 分配成本放大。metadata 只在设备把
    /// used ring 发布后才回收，且所有访问都在队列锁内完成，因此不会与在途请求共享。
    meta_pool: Vec<DmaBuffer>,
    /// 数据 DMA 缓冲复用池。只缓存已经完成或尚未发布给设备的缓冲。
    data_pool: DmaBufferPool,
    /// 描述符 head 到在途请求的直接映射。
    ///
    /// bench 的 L1/L2 裸块设备测试会放大每次完成的 CPU 开销。used ring 已经返回
    /// descriptor head，用固定表 O(1) 取回请求，避免中断/轮询热路径随队列深度线性扫描。
    pending: Vec<Option<PendingVirtioRequest>>,
    /// 队列协议错误后拒绝新请求；此时设备已被标记 FAILED。
    failed: bool,
}

// Safety: VirtioBlkQueue 的裸指针指向 DMA 内存，由 Mutex 保护并发访问
unsafe impl Send for VirtioBlkQueue {}
unsafe impl Sync for VirtioBlkQueue {}

impl VirtioBlkQueue {
    fn new(queue: SplitVirtQueue) -> Self {
        let mut pending = Vec::with_capacity(usize::from(queue.queue_size()));
        pending.resize_with(usize::from(queue.queue_size()), || None);
        let meta_pool = Vec::with_capacity(usize::from(queue.queue_size()));
        let dma_context = queue.dma_context();
        Self {
            queue,
            meta_pool,
            data_pool: DmaBufferPool::new(
                pending.len() as u16,
                dma_context,
                DmaBufferPoolProfile::virtio_block_default(),
            ),
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

    fn take_data_dma(
        &mut self,
        len: usize,
        align: usize,
        direction: DmaDirection,
    ) -> Option<DmaBuffer> {
        self.data_pool.take(len, align, direction)
    }

    fn recycle_data_dma(&mut self, data_dma: DmaBuffer) {
        self.data_pool.recycle(data_dma);
    }

    fn recycle_request_dma(&mut self, meta_dma: DmaBuffer, data_dma: Option<DmaBuffer>) {
        self.recycle_meta_dma(meta_dma);
        if let Some(data_dma) = data_dma {
            self.recycle_data_dma(data_dma);
        }
    }

    fn take_pending(&mut self, head: u16) -> Option<PendingVirtioRequest> {
        self.pending
            .get_mut(usize::from(head))
            .and_then(Option::take)
    }

    fn set_pending(
        &mut self,
        head: u16,
        pending: PendingVirtioRequest,
    ) -> Result<(), PendingVirtioRequest> {
        let Some(slot) = self.pending.get_mut(usize::from(head)) else {
            return Err(pending);
        };
        if slot.is_some() {
            return Err(pending);
        }
        *slot = Some(pending);
        Ok(())
    }

    fn mark_failed_and_take_pending(&mut self) -> Vec<Option<PendingVirtioRequest>> {
        self.failed = true;
        let mut failed = Vec::new();
        mem::swap(&mut failed, &mut self.pending);
        failed
    }
}

impl VirtioBlkDmaQueue for VirtioBlkQueue {
    fn split_queue(&mut self) -> &mut SplitVirtQueue {
        &mut self.queue
    }

    fn take_meta_dma(&mut self) -> Option<DmaBuffer> {
        Self::take_meta_dma(self)
    }

    fn recycle_meta_dma(&mut self, meta_dma: DmaBuffer) {
        Self::recycle_meta_dma(self, meta_dma);
    }

    fn take_data_dma(
        &mut self,
        len: usize,
        align: usize,
        direction: DmaDirection,
    ) -> Option<DmaBuffer> {
        Self::take_data_dma(self, len, align, direction)
    }

    fn recycle_data_dma(&mut self, data_dma: DmaBuffer) {
        Self::recycle_data_dma(self, data_dma);
    }
}

struct VirtioBlkInner {
    /// MMIO 传输层，封装 legacy/modern 寄存器差异。
    transport: Box<dyn VirtioMmioTransport>,
    /// 设备容量（扇区数）
    capacity: u64,
    /// 逻辑块大小
    block_size: u32,
    /// 已完成协商的块设备能力。
    capabilities: VirtioBlkCapabilities,
    /// 当前用于通用块 I/O 的 virtqueue 编号。
    queue_id: VirtioBlkQueueId,
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

struct VirtioMmioRegs {
    base: usize,
}

// ───────── MMIO 寄存器访问辅助函数 ─────────

impl VirtioMmioRegs {
    #[inline]
    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    #[inline]
    fn read_reg64(&self, offset: usize) -> u64 {
        let low = self.read_reg(offset) as u64;
        let high = self.read_reg(offset + 4) as u64;
        (high << 32) | low
    }

    #[inline]
    fn read_config_u8(&self, offset: usize) -> u8 {
        unsafe { read_volatile((self.base + BLK_CFG_BASE + offset) as *const u8) }
    }

    #[inline]
    fn read_config_u32(&self, offset: usize) -> u32 {
        self.read_reg(BLK_CFG_BASE + offset)
    }

    #[inline]
    fn read_config_u64(&self, offset: usize) -> u64 {
        self.read_reg64(BLK_CFG_BASE + offset)
    }
}

impl VirtioBlkConfigReader for VirtioMmioRegs {
    fn read_u8(&self, offset: usize) -> Option<u8> {
        Some(self.read_config_u8(offset))
    }

    fn read_u32(&self, offset: usize) -> Option<u32> {
        Some(self.read_config_u32(offset))
    }

    fn read_u64(&self, offset: usize) -> Option<u64> {
        Some(self.read_config_u64(offset))
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
    pub fn new(mmio_base: usize, dma_context: DmaContext) -> Result<Self, &'static str> {
        let regs = VirtioMmioRegs { base: mmio_base };

        // 1. 验证 Magic、Version 和设备类型。具体 v1/v2 差异交给 transport 处理。
        let transport = detect_virtio_mmio(mmio_base)?;
        if regs.read_reg(MMIO_DEVICE_ID) != VIRTIO_MMIO_DEVICE_ID_BLOCK {
            return Err("Not a VirtIO block device");
        }
        let is_legacy = transport.is_legacy();

        // 2. 重置设备
        transport.write_status(0);

        // 3. 设置 ACKNOWLEDGE 和 DRIVER 状态位
        transport.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
        transport.add_status(VIRTIO_STATUS_DRIVER);

        // 4. 协商特性
        let device_features = transport.read_device_features();
        let driver_features = match negotiate_supported_features(device_features, !is_legacy) {
            Ok(features) => features,
            Err(msg) => {
                transport.write_status(VIRTIO_STATUS_FAILED);
                return Err(msg);
            }
        };

        transport.write_driver_features(driver_features);
        transport.add_status(VIRTIO_STATUS_FEATURES_OK);

        // 验证特性协商是否成功
        if transport.read_status() & VIRTIO_STATUS_FEATURES_OK == 0 {
            transport.write_status(VIRTIO_STATUS_FAILED);
            return Err("Feature negotiation failed");
        }

        // 5. 读取并解释设备类型 config。字段语义由 virtio-blk 公共层统一维护。
        let config = match read_device_config(&regs, driver_features) {
            Ok(config) => config,
            Err(err) => {
                transport.write_status(VIRTIO_STATUS_FAILED);
                return Err(err.message());
            }
        };
        let capacity = config.capacity_sectors;
        let block_size = config.logical_block_size;
        let capabilities = config.capabilities;

        // 6. 设置队列
        let queue_id = VirtioBlkQueueId::DEFAULT_IO;
        transport.select_queue(queue_id.raw());
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
            SplitVirtQueue::new_legacy_in(dma_context, queue_size)
        } else {
            SplitVirtQueue::new_in(dma_context, queue_size)
        }
        .map_err(|_| "Failed to allocate VirtIO queue")?;

        // 队列 DMA 布局由公共 SplitVirtQueue 维护，MMIO 传输层只负责把设备
        // 可见 DMA 地址写入寄存器，不直接假设 DMA 地址等于物理地址。
        transport.write_queue_size(u32::from(queue_size));
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
            capabilities,
            queue_id,
            queue: Mutex::new(VirtioBlkQueue::new(split_queue)),
            irq_count: AtomicUsize::new(0),
        });

        Ok(Self { inner })
    }

    fn complete_failed_requests(pending: Vec<Option<PendingVirtioRequest>>, error: BioIoError) {
        for pending in pending.into_iter().flatten() {
            pending.bio.complete(Err(error));
        }
    }

    fn fail_queue_locked(
        &self,
        queue: &mut VirtioBlkQueue,
        reason: &'static str,
    ) -> Vec<Option<PendingVirtioRequest>> {
        log::printk!("[virtio-mmio-blk] queue failed: {}", reason);
        self.inner.transport.add_status(VIRTIO_STATUS_FAILED);
        queue.mark_failed_and_take_pending()
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
            // used ring 回报 descriptor head，pending 表按同一编号直接索引。
            let Some(pending) = queue.take_pending(desc_head) else {
                log::printk!(
                    "[virtio-mmio-blk] used head {} has no pending request",
                    desc_head
                );
                // used ring 返回了当前队列内的 descriptor head，但驱动没有对应的
                // BIO 记录。继续运行可能释放到其它请求的描述符链，因此直接把队列
                // 标记为失败，让上层重新发现设备状态，而不是尝试局部恢复。
                let failed =
                    self.fail_queue_locked(&mut queue, "used head without pending request");
                drop(queue);
                Self::complete_failed_requests(failed, BioIoError::Unavailable);
                return;
            };
            let PendingVirtioRequest {
                mut bio,
                meta_dma,
                mut data_dma,
                expected_device_write_len,
            } = pending;
            core::sync::atomic::fence(Ordering::Acquire);
            meta_dma.sync_for_cpu();
            let status = unsafe {
                let meta = &*(meta_dma.vaddr() as *const VirtioBlkReqMeta);
                meta.status
            };
            let mut result = status_to_result(status);
            if result.is_ok() {
                result = validate_used_write_len(expected_device_write_len, used.len);
            }
            queue.recycle_meta_dma(meta_dma);

            if queue.queue.free_chain_from_head(desc_head).is_err() {
                let failed =
                    self.fail_queue_locked(&mut queue, "completed descriptor chain corrupt");
                drop(queue);
                bio.complete(Err(BioIoError::Unavailable));
                Self::complete_failed_requests(failed, BioIoError::Unavailable);
                return;
            }

            let copy_read_data = result.is_ok() && bio.op == BioOp::Read;
            if !copy_read_data && let Some(dma) = data_dma.take() {
                queue.recycle_data_dma(dma);
            }

            // 大块顺序读的回拷放到队列锁外,避免 L1/L5 1MiB 请求长时间阻塞提交路径。
            drop(queue);
            if copy_read_data {
                if let (BioBuffer::Owned(buf), Some(dma)) = (&mut bio.buffer, data_dma.as_ref()) {
                    if dma.as_slice().len() < buf.len() {
                        result = Err(BioIoError::Unavailable);
                    } else {
                        dma.sync_for_cpu();
                        buf.copy_from_slice(&dma.as_slice()[..buf.len()]);
                    }
                } else {
                    result = Err(BioIoError::Unavailable);
                }
                if let Some(dma) = data_dma.take() {
                    queue = self.inner.queue.lock();
                    queue.recycle_data_dma(dma);
                    drop(queue);
                }
            }

            // 释放 queue 锁后再 complete bio——避免 completion 路径
            // （包括 Waker::wake 和 WaitQueue::wake_all）持队列锁重入。
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

        let queue_guard = self.inner.queue.lock();
        let limits = block_limits(
            block_size,
            queue_guard.queue.dma_context(),
            self.inner.capabilities,
        )?;
        let queue_depth = u32::from(queue_guard.queue.queue_size());
        drop(queue_guard);
        let attributes = BlockAttributes::new(false, false, NonZeroU32::new(queue_depth), None);
        let features = self.inner.capabilities.block_features(block_size);

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
        if queue.failed {
            return Err((SubmitError::DeviceGone, bio));
        }

        let plan = match VirtioBlkRequestPlan::from_bio(
            bio.op,
            bio.range.lba,
            bio.range.blocks,
            bio.fua,
            self.driver.inner.block_size,
            self.driver.inner.capabilities,
        ) {
            Ok(plan) => plan,
            Err(err) => return Err((err, bio)),
        };
        if let Err(err) = validate_bio_buffer_for_plan(plan, &bio) {
            return Err((err, bio));
        }
        let mut request = match allocate_request(&mut *queue, plan) {
            Ok(request) => request,
            Err(err) => return Err((err, bio)),
        };

        if request.data_dma.is_some() {
            // 大块顺序写和 range payload 初始化放到队列锁外，避免阻塞完成路径。
            drop(queue);
            if let Err(err) = write_data_payload(plan, &bio, &mut request.data_dma) {
                queue = self.driver.inner.queue.lock();
                free_allocated_request(&mut *queue, request);
                return Err((err, bio));
            }
            queue = self.driver.inner.queue.lock();
            if queue.failed {
                free_allocated_request(&mut *queue, request);
                return Err((SubmitError::DeviceGone, bio));
            }
        }

        // 描述符链形状由 virtio-blk 公共层统一维护，MMIO 传输层只负责发布 head。
        if let Err(err) = write_allocated_request_descriptors(&mut *queue, &request, plan) {
            free_allocated_request(&mut *queue, request);
            return Err((err, bio));
        }
        let expected_device_write_len = match plan.expected_device_write_len() {
            Ok(len) => len,
            Err(err) => {
                free_allocated_request(&mut *queue, request);
                return Err((err, bio));
            }
        };

        let VirtioBlkAllocatedRequest {
            chain,
            head: head_idx,
            meta_dma,
            data_dma,
        } = request;
        let pending = PendingVirtioRequest {
            bio,
            meta_dma,
            data_dma,
            expected_device_write_len,
        };
        if let Err(pending) = queue.set_pending(head_idx, pending) {
            let PendingVirtioRequest {
                bio,
                meta_dma,
                data_dma,
                ..
            } = pending;
            let _ = queue.queue.free_chain(chain);
            queue.recycle_request_dma(meta_dma, data_dma);
            return Err((SubmitError::QueueFull, bio));
        }

        // 先登记 pending，再把 head 发布到 available ring，避免设备极快完成时找不到请求。
        if queue.queue.push_avail(head_idx).is_err() {
            let pending = match queue.take_pending(head_idx) {
                Some(pending) => pending,
                None => {
                    let failed = self
                        .driver
                        .fail_queue_locked(&mut queue, "pending lost before publish failure");
                    drop(queue);
                    VirtioBlk::complete_failed_requests(failed, BioIoError::Unavailable);
                    return Ok(());
                }
            };
            let PendingVirtioRequest {
                bio,
                meta_dma,
                data_dma,
                ..
            } = pending;
            let _ = queue.queue.free_chain(chain);
            queue.recycle_meta_dma(meta_dma);
            if let Some(data_dma) = data_dma {
                queue.recycle_data_dma(data_dma);
            }
            return Err((SubmitError::QueueFull, bio));
        }

        // 通知设备
        drop(queue);
        self.driver
            .inner
            .transport
            .notify_queue(u32::from(self.driver.inner.queue_id.raw()));
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

    fn matches_block_device(&self, info: &PlatformDeviceInfo) -> bool {
        if !Self::matches_platform(info) {
            return false;
        }
        let Some((phys, _size)) = info.first_mmio() else {
            return false;
        };
        let base = (self.device_mmio_to_virt)(phys);
        let magic = unsafe { read_volatile((base + MMIO_MAGIC) as *const u32) };
        if magic != VIRTIO_MMIO_MAGIC_VALUE {
            return false;
        }
        let version = unsafe { read_volatile((base + MMIO_VERSION) as *const u32) };
        if !matches!(version, 1 | 2) {
            return false;
        }
        let device_id = unsafe { read_volatile((base + MMIO_DEVICE_ID) as *const u32) };
        device_id == VIRTIO_MMIO_DEVICE_ID_BLOCK
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
            .is_some_and(|info| self.matches_block_device(info))
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
        if !self.matches_block_device(info) {
            return Err(PnpError::NoDriver);
        }
        let virt_base = (self.device_mmio_to_virt)(phys);
        let driver = VirtioBlk::new(virt_base, info.dma_context()).map_err(|msg| {
            log::printk!("[platform-virtio-mmio-blk] probe failed: {}", msg);
            PnpError::hardware_failure("virtio-mmio block init failed")
        })?;
        let dev_name = alloc_virtio_blk_dev_name(&dev.name)?;
        let block_dev = driver.into_block_dev(&dev_name).map_err(|_| {
            PnpError::registration_failed(PnpResourceKind::Function, "block function")
        })?;
        dev.register_function(Arc::new(BlockFunction::with_projection_name(
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
