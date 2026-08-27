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
use core::num::NonZeroU32;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "block-profile")]
use super::common::VirtioBlkProfile;
use super::common::{
    IrqSafeMutex, MIN_QUEUE_SIZE as VIRTIO_BLK_MIN_QUEUE_SIZE, VirtioBlkAllocatedRequest,
    VirtioBlkCapabilities, VirtioBlkConfigReader, VirtioBlkOperationGate, VirtioBlkPendingRequest,
    VirtioBlkQueueCore, VirtioBlkQueueId, VirtioBlkReqMeta, VirtioBlkRequestPlan,
    abandon_allocated_request, allocate_request, block_limits, copy_completed_read_payload,
    free_allocated_request, negotiate_supported_features, read_device_config,
    reclaim_request_payload_for_cpu, status_to_result, validate_bio_buffer_for_plan,
    validate_used_write_len, write_allocated_request_descriptors, write_data_payload,
};
use super::{VIRTIO_BLK_SECTOR_SIZE, alloc_virtio_blk_dev_name};
use general::dev::bio::{BIO_MAX_BORROWED_SEGMENTS, Bio, BioIoError, BioOp, SubmitError};
use general::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockGeometry,
};
#[cfg(feature = "block-profile")]
use general::dev::control::{BlockControlRequest, BlockControlResponse, ControlError};
use general::dev::dma::DmaContext;
use general::dev::function::BlockFunction;
use general::dev::irq::{self, IrqError, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use general::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use virtio::virtio_mmio::{
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FAILED,
    VIRTIO_STATUS_FEATURES_OK, VirtioMmioTransport, detect as detect_virtio_mmio,
};
use virtio::{SplitVirtQueue, choose_split_queue_size};

// ───────── VirtIO MMIO 寄存器布局 ─────────

const VIRTIO_MMIO_MAGIC_VALUE: u32 = 0x74726976; // "virt"
const VIRTIO_MMIO_DEVICE_ID_BLOCK: u32 = 2;
const VIRTIO_MMIO_RESET_SPIN_LIMIT: u32 = 1_000_000;

// MMIO 寄存器偏移
const MMIO_MAGIC: usize = 0x000;
const MMIO_VERSION: usize = 0x004;
const MMIO_DEVICE_ID: usize = 0x008;
const MMIO_QUEUE_NOTIFY: usize = 0x050;
const MMIO_INTERRUPT_STATUS: usize = 0x060;
const MMIO_INTERRUPT_ACK: usize = 0x064;

// MMIO 传输层中设备类型 config 的起始偏移；字段语义由 virtio-blk 公共层解析。
const BLK_CFG_BASE: usize = 0x100;

// ───────── 驱动内部状态 ─────────

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
    /// 热路径缓存：MMIO queue notify 寄存器地址。
    notify_addr: usize,
    /// 热路径缓存：写入 notify 寄存器的队列号。
    notify_value: u32,
    /// 中断状态/ACK 寄存器地址。中断路径也避免 dyn transport 调用。
    interrupt_status_addr: usize,
    interrupt_ack_addr: usize,
    /// 队列
    queue: IrqSafeMutex<Option<VirtioBlkQueueCore>>,
    operations: VirtioBlkOperationGate,
    /// 中断计数（用于轮询模式）
    irq_count: AtomicUsize,
    /// probe 完成后是否已注册 PLIC/父级 IRQ handler。
    irq_registered: AtomicBool,
    #[cfg(feature = "block-profile")]
    profile: VirtioBlkProfile,
}

pub struct VirtioBlk {
    inner: Arc<VirtioBlkInner>,
}

impl VirtioBlkInner {
    fn reset_wait(&self) -> bool {
        self.transport.write_status(0);
        for _ in 0..VIRTIO_MMIO_RESET_SPIN_LIMIT {
            if self.transport.read_status() == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        self.transport.read_status() == 0
    }
}

impl Drop for VirtioBlk {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            // reset 未完成时设备仍可能访问 queue/pending DMA。额外保留一份 inner
            // 所有权，保证即使 panic 策略允许展开，也不会释放仍可被设备访问的页。
            core::mem::forget(Arc::clone(&self.inner));
            panic!("virtio-mmio-blk: device reset timed out during teardown")
        }
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
    /// 幂等关闭设备，并在 reset 完成后主动撤销全部队列 DMA。
    fn shutdown(&self) -> Result<(), &'static str> {
        if !self.inner.operations.quiesce(VIRTIO_MMIO_RESET_SPIN_LIMIT) {
            return Err("virtio-mmio-blk: data path did not quiesce during shutdown");
        }
        let (queue, pending) = {
            let mut slot = self.inner.queue.lock();
            let Some(queue) = slot.as_mut() else {
                return Ok(());
            };
            queue.mark_failed();
            if !self.inner.reset_wait() {
                return Err("virtio-mmio-blk: device reset timed out during shutdown");
            }
            let mut queue = slot.take().expect("live queue checked before reset");
            let pending = queue.take_all_pending();
            (queue, pending)
        };

        // reset 已确认后，先撤销 queue/pool 映射，再归还 pending BIO。后者会在
        // 唤醒等待者前撤销 direct 映射，并在本函数返回前释放请求 DMA。
        drop(queue);
        Self::complete_failed_requests(pending, BioIoError::Unavailable);
        Ok(())
    }

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
        let driver_features = match negotiate_supported_features(
            device_features,
            !is_legacy,
            dma_context.requires_access_platform(),
        ) {
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
        let dma_context = dma_context.with_scatter_gather(
            usize::from(queue_size.saturating_sub(2))
                .min(capabilities.max_data_segments)
                .min(BIO_MAX_BORROWED_SEGMENTS),
        );
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
            notify_addr: mmio_base + MMIO_QUEUE_NOTIFY,
            notify_value: u32::from(queue_id.raw()),
            interrupt_status_addr: mmio_base + MMIO_INTERRUPT_STATUS,
            interrupt_ack_addr: mmio_base + MMIO_INTERRUPT_ACK,
            queue: IrqSafeMutex::new(Some(VirtioBlkQueueCore::new(split_queue))),
            operations: VirtioBlkOperationGate::new(),
            irq_count: AtomicUsize::new(0),
            irq_registered: AtomicBool::new(false),
            #[cfg(feature = "block-profile")]
            profile: VirtioBlkProfile::new(),
        });

        Ok(Self { inner })
    }

    fn complete_failed_requests(pending: Vec<Option<VirtioBlkPendingRequest>>, error: BioIoError) {
        for mut pending in pending.into_iter().flatten() {
            pending.meta_dma.sync_for_cpu();
            reclaim_request_payload_for_cpu(
                pending.data_dma.as_ref(),
                pending.direct_bio_mappings.as_ref(),
            );
            drop(pending.direct_bio_mappings.take());
            pending.bio.complete(Err(error));
        }
    }

    #[cfg(feature = "block-profile")]
    fn profile_text(&self) -> alloc::string::String {
        self.inner.profile.format_text("virtio-mmio")
    }

    fn fail_queue_locked(
        &self,
        queue: &mut VirtioBlkQueueCore,
        reason: &'static str,
    ) -> Vec<Option<VirtioBlkPendingRequest>> {
        log::printk!("[virtio-mmio-blk] queue failed: {}", reason);
        queue.mark_failed();
        // 借用页可能仍是设备的 DMA 目标。fatal queue 路径必须观察到 device reset
        // 完成后才能把 pending BIO 归还给调用方；坏设备不响应 reset 时宁可停在此处，
        // 也不能制造 DMA-after-free。
        if self.inner.reset_wait() {
            return queue.take_all_pending();
        }
        panic!("virtio-mmio-blk: device reset timed out after fatal queue error")
    }

    /// 轮询并处理已完成的请求
    pub fn poll(&self) {
        let Some(_operation) = self.inner.operations.enter() else {
            return;
        };
        loop {
            let mut queue_guard = self.inner.queue.lock();
            let Some(queue) = queue_guard.as_mut() else {
                return;
            };
            if queue.is_failed() {
                return;
            }
            #[cfg(feature = "block-profile")]
            let profile_poll_start = queue
                .sampled_pending_published_ns()
                .map(|_| sched::now_ns_public());
            let used = match queue.split_queue_mut().pop_used() {
                Ok(Some(used)) => {
                    #[cfg(feature = "block-profile")]
                    if let Some(start) = profile_poll_start {
                        self.inner
                            .profile
                            .record_used_poll_cost(sched::now_ns_public().saturating_sub(start));
                    }
                    used
                }
                Ok(None) => {
                    #[cfg(feature = "block-profile")]
                    if let Some(published_ns) = queue.sampled_pending_published_ns() {
                        if let Some(start) = profile_poll_start {
                            self.inner.profile.record_empty_poll_cost(
                                sched::now_ns_public().saturating_sub(start),
                            );
                        }
                        self.inner.profile.record_empty_poll_since_publish(
                            sched::now_ns_public().saturating_sub(published_ns),
                        );
                    }
                    break;
                }
                Err(_) => {
                    let failed = self.fail_queue_locked(queue, "used ring corruption");
                    drop(queue_guard);
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
                let failed = self.fail_queue_locked(queue, "used head without pending request");
                drop(queue_guard);
                Self::complete_failed_requests(failed, BioIoError::Unavailable);
                return;
            };
            let VirtioBlkPendingRequest {
                mut bio,
                meta_dma,
                mut data_dma,
                direct_bio_mappings,
                expected_device_write_len,
                #[cfg(feature = "block-profile")]
                profile_published_ns,
                #[cfg(feature = "block-profile")]
                profile_notified_ns,
            } = pending;
            #[cfg(feature = "block-profile")]
            if profile_published_ns != 0 {
                let now = sched::now_ns_public();
                self.inner
                    .profile
                    .record_publish_to_used(now.saturating_sub(profile_published_ns));
                if profile_notified_ns != 0 {
                    self.inner
                        .profile
                        .record_notify_to_used(now.saturating_sub(profile_notified_ns));
                }
            }
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

            if queue
                .split_queue_mut()
                .free_chain_from_head(desc_head)
                .is_err()
            {
                let failed = self.fail_queue_locked(queue, "completed descriptor chain corrupt");
                drop(queue_guard);
                reclaim_request_payload_for_cpu(data_dma.as_ref(), direct_bio_mappings.as_ref());
                drop(direct_bio_mappings);
                bio.complete(Err(BioIoError::Unavailable));
                Self::complete_failed_requests(failed, BioIoError::Unavailable);
                return;
            }

            // 大块顺序读的数据同步/回拷放到队列锁外，避免长时间阻塞提交路径。
            drop(queue_guard);
            reclaim_request_payload_for_cpu(data_dma.as_ref(), direct_bio_mappings.as_ref());
            if result.is_ok() && bio.op == BioOp::Read {
                if let Err(error) = copy_completed_read_payload(
                    &mut bio,
                    data_dma.as_ref(),
                    direct_bio_mappings.as_ref(),
                ) {
                    result = Err(error);
                }
            }
            if let Some(dma) = data_dma.take() {
                let mut queue_guard = self.inner.queue.lock();
                if let Some(queue) = queue_guard.as_mut() {
                    queue.recycle_data_dma(dma);
                }
            }
            drop(direct_bio_mappings);

            // 释放 queue 锁后再 complete bio——避免 completion 路径
            // （包括 Waker::wake 和 WaitQueue::wake_all）持队列锁重入。
            bio.complete(result);
        }
    }

    /// 处理中断（由中断处理程序调用）
    pub fn handle_interrupt(&self) -> bool {
        // 确认中断
        let status = unsafe { read_volatile(self.inner.interrupt_status_addr as *const u32) };
        if status == 0 {
            return false;
        }
        unsafe { write_volatile(self.inner.interrupt_ack_addr as *mut u32, status) };
        self.inner.irq_count.fetch_add(1, Ordering::Relaxed);

        // 轮询完成的请求
        self.poll();
        true
    }

    pub fn set_irq_registered(&self, registered: bool) {
        self.inner
            .irq_registered
            .store(registered, Ordering::Release);
    }

    fn completion_is_interrupt_driven(&self) -> bool {
        self.inner.irq_registered.load(Ordering::Acquire)
    }

    /// 创建 BlockDev 包装
    pub fn into_block_dev(self, name: &str) -> Result<(Arc<BlockDevice>, Arc<Self>), &'static str> {
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
        let queue = queue_guard
            .as_ref()
            .ok_or("VirtIO block queue is already shut down")?;
        let limits = block_limits(
            block_size,
            queue.split_queue().dma_context(),
            self.inner.capabilities,
        )?;
        let queue_depth = u32::from(queue.split_queue().queue_size());
        drop(queue_guard);
        let attributes = BlockAttributes::new(false, false, NonZeroU32::new(queue_depth), None);
        let features = self.inner.capabilities.block_features(block_size);

        let driver = Arc::new(self);
        let io = Arc::new(VirtioBlkIo {
            driver: Arc::clone(&driver),
        });

        Ok((
            Arc::new(BlockDevice::new(
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
            )),
            driver,
        ))
    }
}

struct VirtioBlkIrqHandler {
    driver: Arc<VirtioBlk>,
}

impl IrqHandler for VirtioBlkIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.driver.handle_interrupt() {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
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
        let Some(_operation) = self.driver.inner.operations.enter() else {
            return Err((SubmitError::DeviceGone, bio));
        };
        // 先回收设备已发布的完成项，避免并发提交在中断合并时丢失进度。
        self.driver.poll();
        let mut queue_guard = self.driver.inner.queue.lock();
        let Some(queue) = queue_guard.as_mut() else {
            return Err((SubmitError::DeviceGone, bio));
        };
        if queue.is_failed() {
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
        let mut request = match allocate_request(queue, plan, &bio, self.driver.inner.capabilities)
        {
            Ok(request) => request,
            Err(err) => return Err((err, bio)),
        };

        if request.data_dma.is_some() {
            // 大块顺序写和 range payload 初始化放到队列锁外，避免阻塞完成路径。
            drop(queue_guard);
            if let Err(err) = write_data_payload(plan, &bio, &mut request.data_dma) {
                let mut queue_guard = self.driver.inner.queue.lock();
                if let Some(queue) = queue_guard.as_mut() {
                    free_allocated_request(queue, request);
                    return Err((err, bio));
                }
                drop(queue_guard);
                abandon_allocated_request(request);
                return Err((SubmitError::DeviceGone, bio));
            }
            queue_guard = self.driver.inner.queue.lock();
            let Some(queue) = queue_guard.as_mut() else {
                drop(queue_guard);
                abandon_allocated_request(request);
                return Err((SubmitError::DeviceGone, bio));
            };
            if queue.is_failed() {
                free_allocated_request(queue, request);
                return Err((SubmitError::DeviceGone, bio));
            }
        }
        let queue = queue_guard
            .as_mut()
            .expect("queue presence checked after every unlocked staging window");

        // 描述符链形状由 virtio-blk 公共层统一维护，MMIO 传输层只负责发布 head。
        if let Err(err) = write_allocated_request_descriptors(queue, &request, plan, &bio) {
            free_allocated_request(queue, request);
            return Err((err, bio));
        }
        let expected_device_write_len = match plan.expected_device_write_len() {
            Ok(len) => len,
            Err(err) => {
                free_allocated_request(queue, request);
                return Err((err, bio));
            }
        };

        let VirtioBlkAllocatedRequest {
            chain,
            head: head_idx,
            meta_dma,
            data_dma,
            direct_bio_mappings,
        } = request;
        let pending = VirtioBlkPendingRequest {
            bio,
            meta_dma,
            data_dma,
            direct_bio_mappings,
            expected_device_write_len,
            #[cfg(feature = "block-profile")]
            profile_published_ns: 0,
            #[cfg(feature = "block-profile")]
            profile_notified_ns: 0,
        };
        if let Err(pending) = queue.set_pending(head_idx, pending) {
            let VirtioBlkPendingRequest {
                bio,
                meta_dma,
                data_dma,
                direct_bio_mappings,
                ..
            } = pending;
            meta_dma.sync_for_cpu();
            reclaim_request_payload_for_cpu(data_dma.as_ref(), direct_bio_mappings.as_ref());
            let _ = queue.split_queue_mut().free_chain(chain);
            queue.recycle_request_dma(meta_dma, data_dma);
            return Err((SubmitError::QueueFull, bio));
        }
        #[cfg(feature = "block-profile")]
        let profile_sample = self.driver.inner.profile.should_sample_request();

        // 先登记 pending，再把 head 发布到 available ring，避免设备极快完成时找不到请求。
        if queue.split_queue_mut().push_avail(head_idx).is_err() {
            let pending = match queue.take_pending(head_idx) {
                Some(pending) => pending,
                None => {
                    let failed = self
                        .driver
                        .fail_queue_locked(queue, "pending lost before publish failure");
                    drop(queue_guard);
                    VirtioBlk::complete_failed_requests(failed, BioIoError::Unavailable);
                    return Ok(());
                }
            };
            let VirtioBlkPendingRequest {
                bio,
                meta_dma,
                data_dma,
                direct_bio_mappings,
                ..
            } = pending;
            meta_dma.sync_for_cpu();
            reclaim_request_payload_for_cpu(data_dma.as_ref(), direct_bio_mappings.as_ref());
            let _ = queue.split_queue_mut().free_chain(chain);
            queue.recycle_meta_dma(meta_dma);
            if let Some(data_dma) = data_dma {
                queue.recycle_data_dma(data_dma);
            }
            return Err((SubmitError::QueueFull, bio));
        }
        #[cfg(feature = "block-profile")]
        let profile_published_ns = if profile_sample {
            let ns = sched::now_ns_public();
            queue.set_pending_profile_published_ns(head_idx, ns);
            ns
        } else {
            0
        };

        // publish 与 notify 保持在同一 queue guard 内，避免 fatal reset 与陈旧 notify
        // 交错；IrqSafeMutex 同时阻止本 CPU 的完成中断重入。
        // Safety: notify_addr 来自已校验的 virtio-mmio 寄存器窗口，写入值是当前队列号。
        unsafe {
            write_volatile(
                self.driver.inner.notify_addr as *mut u32,
                self.driver.inner.notify_value,
            );
        }
        #[cfg(feature = "block-profile")]
        if profile_sample {
            let notified_ns = sched::now_ns_public();
            self.driver
                .inner
                .profile
                .record_publish_to_notify(notified_ns.saturating_sub(profile_published_ns));
            let _ = queue.set_pending_profile_notified_ns(head_idx, notified_ns);
        }
        drop(queue_guard);
        Ok(())
    }

    fn drain(&self) {
        self.driver.poll();
    }

    fn completion_is_interrupt_driven(&self) -> bool {
        self.driver.completion_is_interrupt_driven()
    }

    #[cfg(feature = "block-profile")]
    fn control(
        &self,
        req: BlockControlRequest,
    ) -> Option<Result<BlockControlResponse, ControlError>> {
        match req {
            BlockControlRequest::GetDebugProfile => Some(Ok(BlockControlResponse::DebugText(
                self.driver.profile_text(),
            ))),
            _ => None,
        }
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

struct VirtioMmioBlkBinding {
    driver: Arc<VirtioBlk>,
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

fn map_irq_error(err: IrqError) -> PnpError {
    match err {
        IrqError::OutOfMemory => PnpError::OutOfMemory,
        IrqError::AlreadyRegistered => PnpError::registration_failed(
            PnpResourceKind::Irq,
            "virtio-mmio block irq already registered",
        ),
        IrqError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::Irq, "virtio-mmio block irq not found")
        }
    }
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
}

fn register_virtio_mmio_blk_irq(
    info: &PlatformDeviceInfo,
    driver: Arc<VirtioBlk>,
) -> Result<Option<IrqHandle>, PnpError> {
    let handler: Arc<dyn IrqHandler> = Arc::new(VirtioBlkIrqHandler { driver });
    match info.register_first_irq_handler(handler) {
        Ok(handle) => Ok(Some(handle)),
        Err(PlatformIrqRegistrationError::NoResource) => Ok(None),
        Err(PlatformIrqRegistrationError::Unresolved) => {
            Err(PnpError::dependency(first_irq_dependency(info)))
        }
        Err(PlatformIrqRegistrationError::RegistrationFailed { line, err }) => {
            log::printk!(
                "[platform-virtio-mmio-blk] failed to register irq {:?}: {:?}",
                line,
                err
            );
            Err(map_irq_error(err))
        }
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
        let (block_dev, driver) = driver.into_block_dev(&dev_name).map_err(|_| {
            PnpError::registration_failed(PnpResourceKind::Function, "block function")
        })?;
        let irq_handle = register_virtio_mmio_blk_irq(info, Arc::clone(&driver))?;
        let irq_registered = irq_handle.is_some();
        if let Some(handle) = irq_handle
            && let Err(err) =
                dev.own_resource(irq::irq_handler_pnp_resource(handle, "virtio-mmio-blk-irq"))
        {
            let _ = irq::unregister_irq_handler(handle);
            return Err(err);
        }
        driver.set_irq_registered(irq_registered);
        dev.register_function(BlockFunction::with_projection_name_arc(
            &dev.name, &dev_name, block_dev,
        ))?;
        dev.set_driver_data(Arc::new(VirtioMmioBlkBinding { driver }));
        log::printk!(
            "[platform-virtio-mmio-blk] bound {} phys={:#x} -> /dev/{}",
            dev.name,
            phys,
            dev_name
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Err(error) = self.try_remove(dev) {
            log::error!(
                "[platform-virtio-mmio-blk] remove failed for {}: {:?}",
                dev.name,
                error
            );
        }
    }

    fn try_remove(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let data = dev.take_driver_data().ok_or(PnpError::InvalidState)?;
        let binding =
            Arc::downcast::<VirtioMmioBlkBinding>(data).map_err(|_| PnpError::InvalidState)?;
        if let Err(error) = binding.driver.shutdown() {
            log::error!(
                "[platform-virtio-mmio-blk] shutdown failed for {}: {}",
                dev.name,
                error
            );
            // PnP commit 已进入不可逆阶段，binding 不能重新挂回 Removing 设备。
            // 保留 driver 可确保仍可能被设备访问的 queue/pending DMA 不会析构。
            core::mem::forget(binding);
            return Err(PnpError::hardware_failure(
                "virtio-mmio block shutdown failed",
            ));
        }
        log::printk!("[platform-virtio-mmio-blk] removed {}", dev.name);
        Ok(())
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

/// 注册 VirtIO-MMIO block 驱动 factory。
pub(super) fn register_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(VirtioMmioBlkFactory))
}
