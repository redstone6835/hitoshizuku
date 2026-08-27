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
//! 5. 封装成 [`BlockDevice`](crate::dev::block::BlockDevice) 并通过
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
use general::dev::function::BlockFunction;
use general::dev::irq::{self, IrqError, IrqHandler, IrqLine, IrqStatus};
use general::dev::pci::{PciDevice, PciInfo, PciMsiPnpResource};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use virtio::{
    SplitVirtQueue, VIRTIO_PCI_RESET_SPIN_LIMIT, VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER,
    VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FAILED, VIRTIO_STATUS_FEATURES_OK, VirtioPciCap,
    VirtioPciFunction, VirtioPciTransport, choose_split_queue_size, parse_virtio_pci_caps,
};

const VIRTIO_PCI_FUNCTION_BLOCK: VirtioPciFunction =
    VirtioPciFunction::new("block", 0x1001, 0x1042);

// ── 队列状态 ────────────────────────────────────────────────────────────

/// PCI device_cfg capability 的 virtio-blk config reader。
///
/// capability 长度是 PCI 传输层给出的访问边界；所有字段读取都通过 `checked_addr`
/// 校验后再执行 volatile load，避免驱动在损坏或截短的 device_cfg 上越界访问。
struct VirtioPciBlkConfigReader {
    cap: VirtioPciCap,
}

impl VirtioBlkConfigReader for VirtioPciBlkConfigReader {
    fn read_u8(&self, offset: usize) -> Option<u8> {
        let addr = self.cap.checked_addr(offset, mem::size_of::<u8>())?;
        Some(unsafe { read_volatile(addr as *const u8) })
    }

    fn read_u32(&self, offset: usize) -> Option<u32> {
        let addr = self.cap.checked_addr(offset, mem::size_of::<u32>())?;
        Some(unsafe { read_volatile(addr as *const u32) })
    }
}

// ── 驱动主结构 ──────────────────────────────────────────────────────────

struct VirtioBlkInner {
    transport: VirtioPciTransport,
    /// 当前用于通用块 I/O 的 virtqueue 编号。
    queue_id: VirtioBlkQueueId,
    /// 当前 I/O 队列的 notify 写地址。
    notify_addr: usize,
    capacity: u64,
    block_size: u32,
    capabilities: VirtioBlkCapabilities,
    queue: IrqSafeMutex<Option<VirtioBlkQueueCore>>,
    operations: VirtioBlkOperationGate,
    irq_count: AtomicUsize,
    poll_irq_mark: AtomicUsize,
    /// probe 完成后是否已注册 MSI/INTx IRQ handler。
    irq_registered: AtomicBool,
    #[cfg(feature = "block-profile")]
    profile: VirtioBlkProfile,
}

pub struct VirtioBlkPci {
    inner: Arc<VirtioBlkInner>,
}

impl Drop for VirtioBlkPci {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            // reset 未完成时设备仍可能访问 queue/pending DMA。额外保留一份 inner
            // 所有权，保证即使 panic 策略允许展开，也不会释放仍可被设备访问的页。
            core::mem::forget(Arc::clone(&self.inner));
            panic!("virtio-pci-blk: device reset timed out during teardown")
        }
    }
}

// ── 初始化序列 ─────────────────────────────────────────────────────────

impl VirtioBlkPci {
    /// 幂等关闭设备，并在 reset 完成后主动撤销全部队列 DMA。
    fn shutdown(&self) -> Result<(), &'static str> {
        if !self.inner.operations.quiesce(VIRTIO_PCI_RESET_SPIN_LIMIT) {
            return Err("virtio-pci-blk: data path did not quiesce during shutdown");
        }
        let (queue, pending) = {
            let mut slot = self.inner.queue.lock();
            let Some(queue) = slot.as_mut() else {
                return Ok(());
            };
            queue.mark_failed();
            if !self.inner.transport.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT) {
                return Err("virtio-pci-blk: device reset timed out during shutdown");
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
        // 协商和队列必须使用同一个 per-function DMA/IOMMU domain 快照；重复查询
        // 既增加 registry 竞争，也可能跨越 provider 热替换边界。
        let dma_context = pci.dma_context();
        let driver_features = match negotiate_supported_features(
            device_features,
            true,
            dma_context.requires_access_platform(),
        ) {
            Ok(features) => features,
            Err(msg) => {
                transport.set_status(VIRTIO_STATUS_FAILED);
                return Err(msg);
            }
        };
        transport.set_driver_features(driver_features);
        transport.add_status(VIRTIO_STATUS_FEATURES_OK);
        if transport.status() & VIRTIO_STATUS_FEATURES_OK == 0 {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-pci: FEATURES_OK rejected");
        }

        // 4. 读设备类型 config。字段语义由 virtio-blk 公共层统一维护。
        let device_cap = caps
            .device
            .ok_or("virtio-pci: missing device_cfg capability")?;
        let config_reader = VirtioPciBlkConfigReader { cap: device_cap };
        let config = match read_device_config(&config_reader, driver_features) {
            Ok(config) => config,
            Err(err) => {
                transport.set_status(VIRTIO_STATUS_FAILED);
                return Err(err.message());
            }
        };
        let capacity = config.capacity_sectors;
        let block_size = config.logical_block_size;
        let capabilities = config.capabilities;

        // 5. 设置默认 I/O 队列。
        let queue_id = VirtioBlkQueueId::DEFAULT_IO;
        transport.select_queue(queue_id.raw());
        let max_qsz = transport.selected_queue_size();
        if max_qsz == 0 {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-pci: selected queue size is zero");
        }
        let qsz =
            choose_split_queue_size(max_qsz, None).map_err(|_| "virtio-pci: invalid queue size")?;
        if qsz < VIRTIO_BLK_MIN_QUEUE_SIZE {
            transport.set_status(VIRTIO_STATUS_FAILED);
            return Err("virtio-pci: selected queue too small");
        }
        transport.set_selected_queue_size(qsz);

        let dma_context = dma_context.with_scatter_gather(
            usize::from(qsz.saturating_sub(2))
                .min(capabilities.max_data_segments)
                .min(BIO_MAX_BORROWED_SEGMENTS),
        );
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

        pci.disable_interrupts();

        // 6. DRIVER_OK
        transport.add_status(VIRTIO_STATUS_DRIVER_OK);

        let queue = VirtioBlkQueueCore::new(split_queue);

        let inner = Arc::new(VirtioBlkInner {
            transport,
            queue_id,
            notify_addr,
            capacity,
            block_size,
            capabilities,
            queue: IrqSafeMutex::new(Some(queue)),
            operations: VirtioBlkOperationGate::new(),
            irq_count: AtomicUsize::new(0),
            poll_irq_mark: AtomicUsize::new(0),
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
        self.inner.profile.format_text("virtio-pci")
    }

    fn fail_queue_locked(
        &self,
        queue: &mut VirtioBlkQueueCore,
        reason: &'static str,
    ) -> Vec<Option<VirtioBlkPendingRequest>> {
        log::printk!("[virtio-pci-blk] queue failed: {}", reason);
        queue.mark_failed();
        if !self.inner.transport.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT) {
            panic!("virtio-pci-blk: device reset timed out after fatal queue error");
        }
        queue.take_all_pending()
    }

    /// 轮询并处理已完成的请求。与 MMIO 版对称。
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
            if let Some(mut pending) = queue.take_pending(desc_head) {
                #[cfg(feature = "block-profile")]
                if pending.profile_published_ns != 0 {
                    let now = sched::now_ns_public();
                    self.inner
                        .profile
                        .record_publish_to_used(now.saturating_sub(pending.profile_published_ns));
                    if pending.profile_notified_ns != 0 {
                        self.inner
                            .profile
                            .record_notify_to_used(now.saturating_sub(pending.profile_notified_ns));
                    }
                }
                core::sync::atomic::fence(Ordering::Acquire);
                pending.meta_dma.sync_for_cpu();
                let status = unsafe {
                    let meta = &*(pending.meta_dma.vaddr() as *const VirtioBlkReqMeta);
                    meta.status
                };
                let mut result = status_to_result(status);
                if result.is_ok() {
                    result = validate_used_write_len(pending.expected_device_write_len, used.len);
                }
                queue.recycle_meta_dma(pending.meta_dma);

                if queue
                    .split_queue_mut()
                    .free_chain_from_head(desc_head)
                    .is_err()
                {
                    let failed =
                        self.fail_queue_locked(queue, "completed descriptor chain corrupt");
                    drop(queue_guard);
                    reclaim_request_payload_for_cpu(
                        pending.data_dma.as_ref(),
                        pending.direct_bio_mappings.as_ref(),
                    );
                    drop(pending.direct_bio_mappings.take());
                    pending.bio.complete(Err(BioIoError::Unavailable));
                    Self::complete_failed_requests(failed, BioIoError::Unavailable);
                    return;
                }

                drop(queue_guard);
                reclaim_request_payload_for_cpu(
                    pending.data_dma.as_ref(),
                    pending.direct_bio_mappings.as_ref(),
                );
                if result.is_ok() && pending.bio.op == BioOp::Read {
                    if let Err(error) = copy_completed_read_payload(
                        &mut pending.bio,
                        pending.data_dma.as_ref(),
                        pending.direct_bio_mappings.as_ref(),
                    ) {
                        result = Err(error);
                    }
                }
                if let Some(data_dma) = pending.data_dma.take() {
                    let mut queue_guard = self.inner.queue.lock();
                    if let Some(queue) = queue_guard.as_mut() {
                        queue.recycle_data_dma(data_dma);
                    }
                }
                drop(pending.direct_bio_mappings.take());

                pending.bio.complete(result);
            } else {
                log::printk!(
                    "[virtio-pci-blk] used head {} has no pending request",
                    desc_head
                );
                // used ring 返回了当前队列内的 descriptor head，但驱动没有对应的
                // BIO 记录。继续运行可能释放到其它请求的描述符链，因此直接把队列
                // 标记为失败，让上层重新发现设备状态，而不是尝试局部恢复。
                let failed = self.fail_queue_locked(queue, "used head without pending request");
                drop(queue_guard);
                Self::complete_failed_requests(failed, BioIoError::Unavailable);
                return;
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
        self.inner.poll_irq_mark.store(
            self.inner.irq_count.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
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
        let queue_guard = self.inner.queue.lock();
        let queue = queue_guard
            .as_ref()
            .ok_or("virtio-pci: queue is already shut down")?;
        let limits = block_limits(
            block_size,
            queue.split_queue().dma_context(),
            self.inner.capabilities,
        )?;
        let queue_depth = queue.split_queue().queue_size() as u32;
        drop(queue_guard);
        let attributes = BlockAttributes::new(false, false, NonZeroU32::new(queue_depth), None);
        let features = self.inner.capabilities.block_features(block_size);
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

// ── BlockDriver 实现 ────────────────────────────────────────────────────

struct VirtioBlkPciIo {
    driver: Arc<VirtioBlkPci>,
}

impl VirtioBlkPciIo {
    fn notify_queue(&self) {
        self.driver.inner.transport.notify_queue(
            self.driver.inner.notify_addr,
            self.driver.inner.queue_id.raw(),
        );
    }
}

impl BlockDriver for VirtioBlkPciIo {
    fn queue_bio(&self, bio: Bio) -> Result<(), (SubmitError, Bio)> {
        let Some(_operation) = self.driver.inner.operations.enter() else {
            return Err((SubmitError::DeviceGone, bio));
        };
        // 先回收设备已发布的完成项，避免并发提交在中断合并时丢失进度。
        // Only poll if new IRQs arrived since last poll (IRQ-gated completion check)
        let current_irq = self.driver.inner.irq_count.load(Ordering::Relaxed);
        if current_irq != self.driver.inner.poll_irq_mark.load(Ordering::Relaxed) {
            self.driver
                .inner
                .poll_irq_mark
                .store(current_irq, Ordering::Relaxed);
            self.driver.poll();
        }
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

        // 描述符链形状由 virtio-blk 公共层统一维护，PCI 传输层只负责 queue notify。
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

        // 先登记 pending，再发布到 available ring；设备完成时按同一 head O(1) 找回 BIO。
        if queue.split_queue_mut().push_avail(head_idx).is_err() {
            let pending = match queue.take_pending(head_idx) {
                Some(pending) => pending,
                None => {
                    let failed = self
                        .driver
                        .fail_queue_locked(queue, "pending lost before publish failure");
                    drop(queue_guard);
                    VirtioBlkPci::complete_failed_requests(failed, BioIoError::Unavailable);
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
        // 与 MMIO 路径相同，publish/notify 不跨 queue lock 边界。
        self.notify_queue();
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
    driver: Arc<VirtioBlkPci>,
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
                    if let Err(err) = dev.own_boxed_resource(PciMsiPnpResource::boxed(
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

    let Some(route) = pci.routed_irq() else {
        pci.disable_interrupts();
        return Ok(None);
    };
    let line = route.line;
    match irq::register_irq_request(route.request("virtio-pci-blk-intx", handler)) {
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
        driver.set_irq_registered(irq.is_some());

        let func = BlockFunction::with_projection_name_arc(&dev.name, &dev_name, block_dev);
        if let Err(err) = dev.register_function(func) {
            if let Some(registration) = irq {
                unregister_virtio_pci_irq(&pci, registration);
            }
            return Err(err);
        }
        dev.set_driver_data(Arc::new(VirtioPciBlkBinding { driver }));
        log::printk!("[virtio-pci] bound {} → /dev/{}", dev.name, dev_name);
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Err(error) = self.try_remove(dev) {
            log::error!("[virtio-pci] remove failed for {}: {:?}", dev.name, error);
        }
    }

    fn try_remove(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(dev).ok_or(PnpError::InvalidState)?;
        let data = dev.take_driver_data().ok_or(PnpError::InvalidState)?;
        let binding =
            Arc::downcast::<VirtioPciBlkBinding>(data).map_err(|_| PnpError::InvalidState)?;

        // 先阻止 INTx 继续进入；MSI handler/resource 仍由 PnP core 在 reset
        // 成功后正式释放。VirtIO reset 完成后设备不再产生 MSI。
        pci.disable_interrupts();
        let shutdown_result = binding.driver.shutdown();
        // reset 失败时也尽力关闭 requester，阻断设备继续发起 DMA；queue/pending
        // 仍保留到永久，不能把 bus-master disable 当成 reset 完成的替代证明。
        let bus_master_result = pci.try_disable_bus_master();
        if let Err(error) = shutdown_result {
            log::error!("[virtio-pci] shutdown failed for {}: {}", dev.name, error);
            if bus_master_result.is_err() {
                log::error!(
                    "[virtio-pci] also failed to disable bus master for {}",
                    dev.name
                );
            }
            // PnP commit 已不可回滚；保留 binding，避免最后一个 driver Arc 再次
            // reset 并析构仍可能被设备访问的 DMA。
            core::mem::forget(binding);
            return Err(PnpError::hardware_failure(
                "virtio-pci block shutdown failed",
            ));
        }
        bus_master_result.map_err(|_| {
            PnpError::hardware_failure("virtio-pci block bus master disable failed")
        })?;
        log::printk!("[virtio-pci] remove {}", dev.name);
        Ok(())
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

/// 注册 VirtIO-PCI block 驱动 factory。
pub(super) fn register_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(VirtioPciBlkFactory))
}
