//! VirtIO block 设备类型的公共协议逻辑。
//!
//! MMIO 与 PCI 只是传输层不同，块请求格式、状态码、描述符链形状和 DMA
//! 缓存策略完全相同。把这些内容集中到这里，避免两个驱动各自维护一份魔数和
//! 错误映射，后续新增传输层时也能复用同一套请求规划。

use core::mem;
use core::num::NonZeroU32;
#[cfg(feature = "block-profile")]
use core::sync::atomic::{AtomicU64, Ordering};

use crate::dev::bio::{Bio, BioBuffer, BioIoError, BioOp, BioReqError, SubmitError};
use crate::dev::block::{BlockFeatures, BlockLimits, BlockRangeLimits};
use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use crate::dev::virtio::{
    DescriptorChain, SplitVirtQueue, VIRTIO_F_VERSION_1, VIRTQ_DESC_F_WRITE, VirtqDescUpdate,
};

use super::VIRTIO_BLK_SECTOR_SIZE;

#[cfg(feature = "block-profile")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VirtioBlkProfileSnapshot {
    pub queue_calls: u64,
    pub sample_period: u64,
    pub sampled_requests: u64,
    pub completed_samples: u64,
    pub publish_to_used_ns: u64,
    pub publish_to_used_min_ns: u64,
    pub publish_to_used_max_ns: u64,
    pub publish_to_notify_ns: u64,
    pub publish_to_notify_samples: u64,
    pub notify_to_used_ns: u64,
    pub notify_to_used_samples: u64,
    pub empty_polls_while_sampled: u64,
    pub empty_poll_since_publish_ns: u64,
    pub empty_poll_since_publish_max_ns: u64,
    pub empty_poll_cost_ns: u64,
    pub empty_poll_cost_samples: u64,
    pub used_poll_cost_ns: u64,
    pub used_poll_cost_samples: u64,
}

#[cfg(feature = "block-profile")]
#[derive(Default)]
pub(crate) struct VirtioBlkProfile {
    queue_calls: AtomicU64,
    sampled_requests: AtomicU64,
    completed_samples: AtomicU64,
    publish_to_used_ns: AtomicU64,
    publish_to_used_min_ns: AtomicU64,
    publish_to_used_max_ns: AtomicU64,
    publish_to_notify_ns: AtomicU64,
    publish_to_notify_samples: AtomicU64,
    notify_to_used_ns: AtomicU64,
    notify_to_used_samples: AtomicU64,
    empty_polls_while_sampled: AtomicU64,
    empty_poll_since_publish_ns: AtomicU64,
    empty_poll_since_publish_max_ns: AtomicU64,
    empty_poll_cost_ns: AtomicU64,
    empty_poll_cost_samples: AtomicU64,
    used_poll_cost_ns: AtomicU64,
    used_poll_cost_samples: AtomicU64,
}

#[cfg(feature = "block-profile")]
impl VirtioBlkProfile {
    const SAMPLE_PERIOD: u64 = 64;

    pub(crate) const fn new() -> Self {
        Self {
            queue_calls: AtomicU64::new(0),
            sampled_requests: AtomicU64::new(0),
            completed_samples: AtomicU64::new(0),
            publish_to_used_ns: AtomicU64::new(0),
            publish_to_used_min_ns: AtomicU64::new(u64::MAX),
            publish_to_used_max_ns: AtomicU64::new(0),
            publish_to_notify_ns: AtomicU64::new(0),
            publish_to_notify_samples: AtomicU64::new(0),
            notify_to_used_ns: AtomicU64::new(0),
            notify_to_used_samples: AtomicU64::new(0),
            empty_polls_while_sampled: AtomicU64::new(0),
            empty_poll_since_publish_ns: AtomicU64::new(0),
            empty_poll_since_publish_max_ns: AtomicU64::new(0),
            empty_poll_cost_ns: AtomicU64::new(0),
            empty_poll_cost_samples: AtomicU64::new(0),
            used_poll_cost_ns: AtomicU64::new(0),
            used_poll_cost_samples: AtomicU64::new(0),
        }
    }

    pub(crate) fn should_sample_request(&self) -> bool {
        let seq = self
            .queue_calls
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if seq % Self::SAMPLE_PERIOD == 0 {
            self.sampled_requests.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub(crate) fn record_publish_to_used(&self, elapsed_ns: u64) {
        self.completed_samples.fetch_add(1, Ordering::Relaxed);
        self.publish_to_used_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        self.publish_to_used_min_ns
            .fetch_min(elapsed_ns, Ordering::Relaxed);
        self.publish_to_used_max_ns
            .fetch_max(elapsed_ns, Ordering::Relaxed);
    }

    pub(crate) fn record_publish_to_notify(&self, elapsed_ns: u64) {
        self.publish_to_notify_samples
            .fetch_add(1, Ordering::Relaxed);
        self.publish_to_notify_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    pub(crate) fn record_notify_to_used(&self, elapsed_ns: u64) {
        self.notify_to_used_samples.fetch_add(1, Ordering::Relaxed);
        self.notify_to_used_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    pub(crate) fn record_empty_poll_since_publish(&self, elapsed_ns: u64) {
        self.empty_polls_while_sampled
            .fetch_add(1, Ordering::Relaxed);
        self.empty_poll_since_publish_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        self.empty_poll_since_publish_max_ns
            .fetch_max(elapsed_ns, Ordering::Relaxed);
    }

    pub(crate) fn record_empty_poll_cost(&self, elapsed_ns: u64) {
        self.empty_poll_cost_samples.fetch_add(1, Ordering::Relaxed);
        self.empty_poll_cost_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    pub(crate) fn record_used_poll_cost(&self, elapsed_ns: u64) {
        self.used_poll_cost_samples.fetch_add(1, Ordering::Relaxed);
        self.used_poll_cost_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> VirtioBlkProfileSnapshot {
        let min = self.publish_to_used_min_ns.load(Ordering::Relaxed);
        VirtioBlkProfileSnapshot {
            queue_calls: self.queue_calls.load(Ordering::Relaxed),
            sample_period: Self::SAMPLE_PERIOD,
            sampled_requests: self.sampled_requests.load(Ordering::Relaxed),
            completed_samples: self.completed_samples.load(Ordering::Relaxed),
            publish_to_used_ns: self.publish_to_used_ns.load(Ordering::Relaxed),
            publish_to_used_min_ns: if min == u64::MAX { 0 } else { min },
            publish_to_used_max_ns: self.publish_to_used_max_ns.load(Ordering::Relaxed),
            publish_to_notify_ns: self.publish_to_notify_ns.load(Ordering::Relaxed),
            publish_to_notify_samples: self.publish_to_notify_samples.load(Ordering::Relaxed),
            notify_to_used_ns: self.notify_to_used_ns.load(Ordering::Relaxed),
            notify_to_used_samples: self.notify_to_used_samples.load(Ordering::Relaxed),
            empty_polls_while_sampled: self.empty_polls_while_sampled.load(Ordering::Relaxed),
            empty_poll_since_publish_ns: self.empty_poll_since_publish_ns.load(Ordering::Relaxed),
            empty_poll_since_publish_max_ns: self
                .empty_poll_since_publish_max_ns
                .load(Ordering::Relaxed),
            empty_poll_cost_ns: self.empty_poll_cost_ns.load(Ordering::Relaxed),
            empty_poll_cost_samples: self.empty_poll_cost_samples.load(Ordering::Relaxed),
            used_poll_cost_ns: self.used_poll_cost_ns.load(Ordering::Relaxed),
            used_poll_cost_samples: self.used_poll_cost_samples.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn format_text(&self, transport: &str) -> alloc::string::String {
        let s = self.snapshot();
        let completed = s.completed_samples.max(1);
        let notify_samples = s.publish_to_notify_samples.max(1);
        let notify_to_used_samples = s.notify_to_used_samples.max(1);
        let empty_polls = s.empty_polls_while_sampled.max(1);
        let empty_cost_samples = s.empty_poll_cost_samples.max(1);
        let used_cost_samples = s.used_poll_cost_samples.max(1);
        alloc::format!(
            "{} queue_n={} sample_period={} sampled={} completed={} publish_to_used_avg={} publish_to_used_min={} publish_to_used_max={} publish_to_notify_avg={} notify_samples={} notify_to_used_avg={} notify_to_used_samples={} empty_polls={} empty_since_publish_avg={} empty_since_publish_max={} empty_poll_cost_avg={} empty_poll_cost_samples={} used_poll_cost_avg={} used_poll_cost_samples={}",
            transport,
            s.queue_calls,
            s.sample_period,
            s.sampled_requests,
            s.completed_samples,
            s.publish_to_used_ns / completed,
            s.publish_to_used_min_ns,
            s.publish_to_used_max_ns,
            s.publish_to_notify_ns / notify_samples,
            s.publish_to_notify_samples,
            s.notify_to_used_ns / notify_to_used_samples,
            s.notify_to_used_samples,
            s.empty_polls_while_sampled,
            s.empty_poll_since_publish_ns / empty_polls,
            s.empty_poll_since_publish_max_ns,
            s.empty_poll_cost_ns / empty_cost_samples,
            s.empty_poll_cost_samples,
            s.used_poll_cost_ns / used_cost_samples,
            s.used_poll_cost_samples,
        )
    }
}

/// virtio-blk 普通 I/O 至少需要 header/data/status 三个描述符。
pub(super) const MIN_QUEUE_SIZE: u16 = 4;
/// 设备 status 初始填充值。任何非 OK/UNSUPP 的最终值都按设备错误处理。
pub(super) const STATUS_PENDING: u8 = 0xff;

/// 一次在途 virtio-blk BIO 的驱动私有状态。
///
/// 传输层只负责把请求提交给具体总线并通知设备；descriptor head 到 BIO、DMA
/// 缓冲与 used ring 校验状态的所有权由公共队列核心维护，避免 MMIO/PCI 两条路径
/// 在错误恢复和资源回收上产生语义差异。
pub(super) struct VirtioBlkPendingRequest {
    pub bio: Bio,
    pub meta_dma: DmaBuffer,
    pub data_dma: Option<DmaBuffer>,
    /// 设备完成时至少应写回的字节数，用于发现 used ring 短写。
    pub expected_device_write_len: u32,
    #[cfg(feature = "block-profile")]
    pub profile_published_ns: u64,
    #[cfg(feature = "block-profile")]
    pub profile_notified_ns: u64,
}

/// virtio-blk 的传输无关队列核心。
///
/// 这个结构只包含 split virtqueue、DMA 缓冲池和 pending 表；真正的寄存器访问、
/// notify 地址和中断 ack 仍留在各传输层驱动中。这样块协议热路径可以复用同一套
/// O(1) 完成查找、DMA 复用和失败回收逻辑，同时不把 PCI/MMIO 细节混进公共层。
pub(super) struct VirtioBlkQueueCore {
    queue: SplitVirtQueue,
    /// 请求头/status 的小 DMA 缓冲复用池。
    meta_pool: alloc::vec::Vec<DmaBuffer>,
    /// 数据 DMA 缓冲复用池。只缓存已经完成或尚未发布给设备的缓冲。
    data_pool: DmaBufferPool,
    /// descriptor head 到在途 BIO 的直接映射。
    pending: alloc::vec::Vec<Option<VirtioBlkPendingRequest>>,
    #[cfg(feature = "block-profile")]
    profile_sampled_pending: u16,
    #[cfg(feature = "block-profile")]
    profile_first_sampled_published_ns: u64,
    /// 队列协议错误后不再接受新请求。
    failed: bool,
}

// Safety: 队列内部的 DMA 指针和 pending 表必须由外层队列锁串行访问；该类型本身
// 不提供并发可变入口，传输层以 Mutex 包裹后跨 CPU 使用。
unsafe impl Send for VirtioBlkQueueCore {}
unsafe impl Sync for VirtioBlkQueueCore {}

impl VirtioBlkQueueCore {
    pub fn new(queue: SplitVirtQueue) -> Self {
        let mut pending = alloc::vec::Vec::with_capacity(usize::from(queue.queue_size()));
        pending.resize_with(usize::from(queue.queue_size()), || None);
        let meta_pool = alloc::vec::Vec::with_capacity(usize::from(queue.queue_size()));
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
            #[cfg(feature = "block-profile")]
            profile_sampled_pending: 0,
            #[cfg(feature = "block-profile")]
            profile_first_sampled_published_ns: 0,
            failed: false,
        }
    }

    pub const fn split_queue(&self) -> &SplitVirtQueue {
        &self.queue
    }

    pub fn split_queue_mut(&mut self) -> &mut SplitVirtQueue {
        &mut self.queue
    }

    pub const fn is_failed(&self) -> bool {
        self.failed
    }

    pub fn take_pending(&mut self, head: u16) -> Option<VirtioBlkPendingRequest> {
        let pending = self
            .pending
            .get_mut(usize::from(head))
            .and_then(Option::take);
        #[cfg(feature = "block-profile")]
        if pending
            .as_ref()
            .is_some_and(|pending| pending.profile_published_ns != 0)
        {
            self.profile_sampled_pending = self.profile_sampled_pending.saturating_sub(1);
            if self.profile_sampled_pending == 0 {
                self.profile_first_sampled_published_ns = 0;
            }
        }
        pending
    }

    pub fn set_pending(
        &mut self,
        head: u16,
        pending: VirtioBlkPendingRequest,
    ) -> Result<(), VirtioBlkPendingRequest> {
        let Some(slot) = self.pending.get_mut(usize::from(head)) else {
            return Err(pending);
        };
        if slot.is_some() {
            return Err(pending);
        }
        *slot = Some(pending);
        Ok(())
    }

    #[cfg(feature = "block-profile")]
    pub fn set_pending_profile_published_ns(&mut self, head: u16, ns: u64) {
        if let Some(Some(pending)) = self.pending.get_mut(usize::from(head)) {
            if pending.profile_published_ns == 0 {
                self.profile_sampled_pending = self.profile_sampled_pending.saturating_add(1);
                if self.profile_first_sampled_published_ns == 0 {
                    self.profile_first_sampled_published_ns = ns;
                }
                pending.profile_published_ns = ns;
            }
        }
    }

    #[cfg(feature = "block-profile")]
    pub fn set_pending_profile_notified_ns(&mut self, head: u16, ns: u64) -> bool {
        if let Some(Some(pending)) = self.pending.get_mut(usize::from(head)) {
            pending.profile_notified_ns = ns;
            true
        } else {
            false
        }
    }

    #[cfg(feature = "block-profile")]
    pub const fn sampled_pending_published_ns(&self) -> Option<u64> {
        if self.profile_sampled_pending == 0 || self.profile_first_sampled_published_ns == 0 {
            None
        } else {
            Some(self.profile_first_sampled_published_ns)
        }
    }

    pub fn mark_failed_and_take_pending(
        &mut self,
    ) -> alloc::vec::Vec<Option<VirtioBlkPendingRequest>> {
        self.failed = true;
        let mut failed = alloc::vec::Vec::new();
        core::mem::swap(&mut failed, &mut self.pending);
        failed
    }

    pub fn recycle_request_dma(&mut self, meta_dma: DmaBuffer, data_dma: Option<DmaBuffer>) {
        self.recycle_meta_dma(meta_dma);
        if let Some(data_dma) = data_dma {
            self.recycle_data_dma(data_dma);
        }
    }

    fn take_meta_dma(&mut self) -> Option<DmaBuffer> {
        self.meta_pool.pop()
    }

    pub fn recycle_meta_dma(&mut self, meta_dma: DmaBuffer) {
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

    pub fn recycle_data_dma(&mut self, data_dma: DmaBuffer) {
        self.data_pool.recycle(data_dma);
    }
}

impl VirtioBlkDmaQueue for VirtioBlkQueueCore {
    fn split_queue(&mut self) -> &mut SplitVirtQueue {
        self.split_queue_mut()
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

/// virtio-blk 队列编号。
///
/// 单队列设备使用默认 I/O 队列；后续接入多队列时，传输层只需要替换队列选择
/// 策略，提交路径仍通过这个类型传递队列身份，避免把裸 `u16` 编号散落在驱动里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VirtioBlkQueueId(u16);

impl VirtioBlkQueueId {
    pub const DEFAULT_IO: Self = Self(0);

    pub const fn raw(self) -> u16 {
        self.0
    }
}

const REQ_TYPE_IN: u32 = 0;
const REQ_TYPE_OUT: u32 = 1;
const REQ_TYPE_FLUSH: u32 = 4;
const REQ_TYPE_DISCARD: u32 = 11;
const REQ_TYPE_WRITE_ZEROES: u32 = 13;

const STATUS_OK: u8 = 0;
const STATUS_UNSUPP: u8 = 2;

/// write-zeroes range segment 的 UNMAP 标志；只有设备声明允许时才置位。
const WRITE_ZEROES_FLAG_UNMAP: u32 = 1;

/// virtio-blk 设备能力位。
const FEATURE_RO: u64 = 1 << 5;
const FEATURE_BLK_SIZE: u64 = 1 << 6;
const FEATURE_FLUSH: u64 = 1 << 9;
const FEATURE_DISCARD: u64 = 1 << 13;
const FEATURE_WRITE_ZEROES: u64 = 1 << 14;

/// 本驱动完整支持并愿意协商的 virtio-blk feature 集合。
///
/// 传输层只读取设备 feature 并写回协商结果；具体哪些 feature 属于块协议能力，
/// 统一在这里维护，避免 MMIO/PCI 路径出现不同的能力选择策略。
const SUPPORTED_FEATURES: u64 = VIRTIO_F_VERSION_1
    | FEATURE_RO
    | FEATURE_BLK_SIZE
    | FEATURE_FLUSH
    | FEATURE_DISCARD
    | FEATURE_WRITE_ZEROES;

/// virtio-blk 配置空间字段偏移，均相对于设备类型 config 起始地址。
const CONFIG_CAPACITY_OFFSET: usize = 0x00;
const CONFIG_BLK_SIZE_OFFSET: usize = 0x14;
const CONFIG_MAX_DISCARD_SECTORS_OFFSET: usize = 0x28;
const CONFIG_MAX_DISCARD_SEG_OFFSET: usize = 0x2c;
const CONFIG_DISCARD_SECTOR_ALIGNMENT_OFFSET: usize = 0x30;
const CONFIG_MAX_WRITE_ZEROES_SECTORS_OFFSET: usize = 0x34;
const CONFIG_MAX_WRITE_ZEROES_SEG_OFFSET: usize = 0x38;
const CONFIG_WRITE_ZEROES_MAY_UNMAP_OFFSET: usize = 0x3c;

/// 默认 DMA 缓冲池最多缓存的 buffer 数。
///
/// 这是驱动策略参数，不是协议常量；集中命名后，后续可以按设备队列深度或
/// 平台 DMA 约束替换 profile，而不会把策略数字散落到提交路径。
const DEFAULT_DMA_POOL_TARGET_BUFFERS: usize = 4;
/// 默认 DMA 缓冲池最多缓存的总字节数。
const DEFAULT_DMA_POOL_TARGET_BYTES: usize = 2 * 1024 * 1024;

/// DMA 缓冲复用池的策略参数。
///
/// 策略对象把“缓存多少完成缓冲”从驱动流程里抽出来；当前默认值偏向小队列和
/// bench 常见的大块顺序 I/O，后续如需按设备类型调整，只替换 profile 即可。
#[derive(Clone, Copy, Debug)]
pub(super) struct DmaBufferPoolProfile {
    target_buffers: usize,
    target_bytes: usize,
}

impl DmaBufferPoolProfile {
    pub const fn new(target_buffers: usize, target_bytes: usize) -> Self {
        Self {
            target_buffers,
            target_bytes,
        }
    }

    pub const fn virtio_block_default() -> Self {
        Self::new(
            DEFAULT_DMA_POOL_TARGET_BUFFERS,
            DEFAULT_DMA_POOL_TARGET_BYTES,
        )
    }

    fn max_buffers(self, queue_size: u16) -> usize {
        self.target_buffers.min(usize::from(queue_size)).max(1)
    }

    fn max_bytes(self, dma_context: DmaContext) -> usize {
        self.target_bytes
            .min(dma_context.constraints().max_segment_size)
            .max(1)
    }
}

/// 已协商完成的 virtio-blk 能力摘要。
///
/// 具体传输层只负责读写寄存器，块层暴露能力统一从这里转换，避免 MMIO/PCI
/// 路径各自重新读取设备 feature 或维护不同的能力判断。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VirtioBlkNegotiatedFeatures {
    bits: u64,
}

impl VirtioBlkNegotiatedFeatures {
    pub const fn new(bits: u64) -> Self {
        Self { bits }
    }

    pub const fn contains(self, feature: u64) -> bool {
        self.bits & feature != 0
    }

    pub const fn read_only(self) -> bool {
        self.contains(FEATURE_RO)
    }

    pub const fn has_flush(self) -> bool {
        self.contains(FEATURE_FLUSH)
    }
}

/// 从设备 feature 中选择驱动支持的集合。
pub(super) fn negotiate_supported_features(
    device_features: u64,
    require_version_1: bool,
) -> Result<u64, &'static str> {
    if require_version_1 && device_features & VIRTIO_F_VERSION_1 == 0 {
        return Err("virtio-blk: VERSION_1 feature is missing");
    }
    let supported = if require_version_1 {
        SUPPORTED_FEATURES
    } else {
        SUPPORTED_FEATURES & !VIRTIO_F_VERSION_1
    };
    Ok(device_features & supported)
}

/// virtio-blk 设备 config 空间读取接口。
///
/// MMIO 与 PCI 的边界检查方式不同：MMIO config 空间由设备寄存器窗口固定给出，
/// PCI device_cfg 则必须按 capability 长度逐项校验。公共层只依赖这个 typed
/// 读取接口，集中解释块设备 config 字段的语义。
pub(super) trait VirtioBlkConfigReader {
    fn read_u8(&self, offset: usize) -> Option<u8>;
    fn read_u32(&self, offset: usize) -> Option<u32>;

    fn read_u64(&self, offset: usize) -> Option<u64> {
        let lo = self.read_u32(offset)? as u64;
        let hi = self.read_u32(offset.checked_add(mem::size_of::<u32>())?)? as u64;
        Some((hi << 32) | lo)
    }
}

/// 已解析并校验过的 virtio-blk config。
pub(super) struct VirtioBlkDeviceConfig {
    pub capacity_sectors: u64,
    pub logical_block_size: u32,
    pub capabilities: VirtioBlkCapabilities,
}

/// config 解析失败原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VirtioBlkConfigError {
    MissingCapacity,
    MissingBlockSize,
    MissingDiscardLimits,
    MissingWriteZeroesLimits,
    MissingWriteZeroesUnmap,
    InvalidBlockSize,
}

impl VirtioBlkConfigError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::MissingCapacity => "virtio-blk: device_cfg capacity is missing",
            Self::MissingBlockSize => "virtio-blk: device_cfg block size is missing",
            Self::MissingDiscardLimits => "virtio-blk: device_cfg discard limits are missing",
            Self::MissingWriteZeroesLimits => {
                "virtio-blk: device_cfg write-zeroes limits are missing"
            }
            Self::MissingWriteZeroesUnmap => {
                "virtio-blk: device_cfg write-zeroes unmap flag is missing"
            }
            Self::InvalidBlockSize => "virtio-blk: invalid logical block size",
        }
    }
}

/// 单段 discard/write-zeroes 类命令的设备限制。
#[derive(Clone, Copy, Debug)]
pub(super) struct VirtioBlkRangeOpLimits {
    pub max_sectors: u32,
    pub max_segments: u32,
    pub sector_alignment: NonZeroU32,
}

impl VirtioBlkRangeOpLimits {
    pub const fn new(
        max_sectors: u32,
        max_segments: u32,
        sector_alignment: NonZeroU32,
    ) -> Option<Self> {
        if max_sectors == 0 || max_segments == 0 {
            return None;
        }
        Some(Self {
            max_sectors,
            max_segments,
            sector_alignment,
        })
    }

    pub const fn supports_single_segment_count(self, sectors: u32) -> bool {
        self.max_segments >= 1 && sectors <= self.max_sectors
    }

    pub fn sector_aligned(self, sector: u64) -> bool {
        sector.is_multiple_of(u64::from(self.sector_alignment.get()))
    }

    pub fn block_range_limits(self, logical_block_size: u32) -> Option<BlockRangeLimits> {
        let sector_scale = logical_block_size.checked_div(VIRTIO_BLK_SECTOR_SIZE)?;
        if sector_scale == 0 || !logical_block_size.is_multiple_of(VIRTIO_BLK_SECTOR_SIZE) {
            return None;
        }
        let max_blocks = NonZeroU32::new(self.max_sectors / sector_scale)?;
        let alignment_blocks =
            logical_lba_alignment_blocks(self.sector_alignment.get(), sector_scale);
        Some(BlockRangeLimits::new(
            Some(max_blocks),
            NonZeroU32::new(alignment_blocks),
        ))
    }
}

fn logical_lba_alignment_blocks(sector_alignment: u32, sector_scale: u32) -> u32 {
    let gcd = gcd_u32(sector_alignment.max(1), sector_scale.max(1));
    (sector_alignment.max(1) / gcd).max(1)
}

fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let next = a % b;
        a = b;
        b = next;
    }
    a.max(1)
}

/// virtio-blk 完成协商并校验后的块设备能力。
#[derive(Clone, Copy, Debug)]
pub(super) struct VirtioBlkCapabilities {
    pub features: VirtioBlkNegotiatedFeatures,
    pub discard: Option<VirtioBlkRangeOpLimits>,
    pub write_zeroes: Option<VirtioBlkRangeOpLimits>,
    pub write_zeroes_may_unmap: bool,
}

impl VirtioBlkCapabilities {
    pub const fn new(
        features: VirtioBlkNegotiatedFeatures,
        discard: Option<VirtioBlkRangeOpLimits>,
        write_zeroes: Option<VirtioBlkRangeOpLimits>,
        write_zeroes_may_unmap: bool,
    ) -> Self {
        Self {
            features,
            discard,
            write_zeroes,
            write_zeroes_may_unmap,
        }
    }

    pub fn block_features(self, logical_block_size: u32) -> BlockFeatures {
        let mut features = BlockFeatures(0);
        if self.features.read_only() {
            features |= BlockFeatures::READ_ONLY;
        }
        if self.features.has_flush() {
            features |= BlockFeatures::FLUSH;
        }
        if self
            .discard
            .and_then(|limits| limits.block_range_limits(logical_block_size))
            .is_some()
        {
            features |= BlockFeatures::DISCARD;
        }
        if self
            .write_zeroes
            .and_then(|limits| limits.block_range_limits(logical_block_size))
            .is_some()
        {
            features |= BlockFeatures::WRITE_ZEROES;
        }
        features
    }
}

/// 解析并校验 virtio-blk device config。
pub(super) fn read_device_config<R: VirtioBlkConfigReader>(
    reader: &R,
    driver_features: u64,
) -> Result<VirtioBlkDeviceConfig, VirtioBlkConfigError> {
    let capacity_sectors = reader
        .read_u64(CONFIG_CAPACITY_OFFSET)
        .ok_or(VirtioBlkConfigError::MissingCapacity)?;
    let negotiated_features = VirtioBlkNegotiatedFeatures::new(driver_features);
    let logical_block_size = if negotiated_features.contains(FEATURE_BLK_SIZE) {
        reader
            .read_u32(CONFIG_BLK_SIZE_OFFSET)
            .ok_or(VirtioBlkConfigError::MissingBlockSize)?
    } else {
        VIRTIO_BLK_SECTOR_SIZE
    };
    if logical_block_size < VIRTIO_BLK_SECTOR_SIZE
        || !logical_block_size.is_power_of_two()
        || !logical_block_size.is_multiple_of(VIRTIO_BLK_SECTOR_SIZE)
    {
        return Err(VirtioBlkConfigError::InvalidBlockSize);
    }

    let discard = if negotiated_features.contains(FEATURE_DISCARD) {
        let max_sectors = reader
            .read_u32(CONFIG_MAX_DISCARD_SECTORS_OFFSET)
            .ok_or(VirtioBlkConfigError::MissingDiscardLimits)?;
        let max_segments = reader
            .read_u32(CONFIG_MAX_DISCARD_SEG_OFFSET)
            .ok_or(VirtioBlkConfigError::MissingDiscardLimits)?;
        let raw_alignment = reader
            .read_u32(CONFIG_DISCARD_SECTOR_ALIGNMENT_OFFSET)
            .ok_or(VirtioBlkConfigError::MissingDiscardLimits)?;
        let alignment = NonZeroU32::new(raw_alignment).unwrap_or(NonZeroU32::MIN);
        VirtioBlkRangeOpLimits::new(max_sectors, max_segments, alignment)
    } else {
        None
    };

    let write_zeroes = if negotiated_features.contains(FEATURE_WRITE_ZEROES) {
        let max_sectors = reader
            .read_u32(CONFIG_MAX_WRITE_ZEROES_SECTORS_OFFSET)
            .ok_or(VirtioBlkConfigError::MissingWriteZeroesLimits)?;
        let max_segments = reader
            .read_u32(CONFIG_MAX_WRITE_ZEROES_SEG_OFFSET)
            .ok_or(VirtioBlkConfigError::MissingWriteZeroesLimits)?;
        // virtio-blk 没有单独的 write-zeroes alignment 字段；协议扇区对齐即可。
        VirtioBlkRangeOpLimits::new(max_sectors, max_segments, NonZeroU32::MIN)
    } else {
        None
    };

    let write_zeroes_may_unmap = if negotiated_features.contains(FEATURE_WRITE_ZEROES) {
        reader
            .read_u8(CONFIG_WRITE_ZEROES_MAY_UNMAP_OFFSET)
            .ok_or(VirtioBlkConfigError::MissingWriteZeroesUnmap)?
            != 0
    } else {
        false
    };
    let capabilities = VirtioBlkCapabilities::new(
        negotiated_features,
        discard,
        write_zeroes,
        write_zeroes_may_unmap,
    );

    Ok(VirtioBlkDeviceConfig {
        capacity_sectors,
        logical_block_size,
        capabilities,
    })
}

/// 根据设备 DMA 能力和 virtio 描述符格式构造块层 I/O 限制。
///
/// 单个数据描述符的长度字段是 u32，同时 DMA 子系统可能给设备声明更小的单段
/// 上限；块层应看到二者的交集，避免提交后才因为 DMA 约束被驱动拒绝。
pub(super) fn block_limits(
    block_size: u32,
    dma_context: DmaContext,
    capabilities: VirtioBlkCapabilities,
) -> Result<BlockLimits, &'static str> {
    if block_size == 0 {
        return Err("virtio-blk: block size is zero");
    }
    let descriptor_limit = u32::MAX as usize;
    let dma_limit = dma_context.constraints().max_segment_size;
    let max_bytes = descriptor_limit.min(dma_limit);
    if max_bytes < block_size as usize {
        return Err("virtio-blk: DMA segment is smaller than one logical block");
    }
    let max_blocks =
        NonZeroU32::new((max_bytes / block_size as usize).min(u32::MAX as usize) as u32);
    let limits = BlockLimits::new(max_blocks, max_blocks, NonZeroU32::new(1)).map(|limits| {
        limits
            .with_discard_limits(
                capabilities
                    .discard
                    .and_then(|limits| limits.block_range_limits(block_size)),
            )
            .with_write_zeroes_limits(
                capabilities
                    .write_zeroes
                    .and_then(|limits| limits.block_range_limits(block_size)),
            )
    });
    match limits {
        Some(limits) => Ok(limits),
        None => Err("virtio-blk: invalid block limits"),
    }
}

#[repr(C)]
pub(super) struct VirtioBlkReqHeader {
    pub req_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

#[repr(C)]
pub(super) struct VirtioBlkReqMeta {
    pub header: VirtioBlkReqHeader,
    pub status: u8,
    pub _pad: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct VirtioBlkRangeSegment {
    sector: u64,
    num_sectors: u32,
    flags: u32,
}

impl VirtioBlkRangeSegment {
    pub const fn new(sector: u64, num_sectors: u32, flags: u32) -> Self {
        Self {
            sector,
            num_sectors,
            flags,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum VirtioBlkDataPayload {
    None,
    BioBuffer,
    RangeSegment(VirtioBlkRangeSegment),
}

/// 可提交给 virtio-blk 设备的一次 BIO 规划。
#[derive(Clone, Copy, Debug)]
pub(super) struct VirtioBlkRequestPlan {
    pub req_type: u32,
    pub sector: u64,
    pub descriptor_count: usize,
    pub data_len: usize,
    /// 数据 descriptor 对应 DMA 缓冲的 CPU 访问对齐。
    ///
    /// 普通 BIO 数据按字节拷贝，1 字节对齐即可；range descriptor 由 CPU 以
    /// 协议结构体写入，必须满足结构体对齐，避免把 allocator 当前行为当作契约。
    pub data_align: usize,
    pub data_direction: Option<DmaDirection>,
    pub data_device_writable: bool,
    pub data_payload: VirtioBlkDataPayload,
}

impl VirtioBlkRequestPlan {
    /// 从通用 BIO 操作构造 virtio-blk 请求规划。
    ///
    /// `logical_block_size` 是设备暴露给块层的逻辑块大小；virtio 请求头中的
    /// sector 字段始终以 512 字节协议扇区计数，因此这里统一完成换算和溢出检查。
    pub fn from_bio(
        op: BioOp,
        lba: u64,
        blocks: u32,
        fua: bool,
        logical_block_size: u32,
        capabilities: VirtioBlkCapabilities,
    ) -> Result<Self, SubmitError> {
        if fua {
            // virtio-blk 的现代协议用独立 FLUSH 命令表达持久化屏障，没有本驱动可
            // 协商的 per-request FUA 位。直接拒绝可以避免把上层的写入持久性要求
            // 静默降级成普通写。
            return Err(SubmitError::Unsupported);
        }
        if logical_block_size < VIRTIO_BLK_SECTOR_SIZE
            || !logical_block_size.is_power_of_two()
            || !logical_block_size.is_multiple_of(VIRTIO_BLK_SECTOR_SIZE)
        {
            return Err(SubmitError::InvalidRequest(BioReqError::Misaligned));
        }
        if capabilities.features.read_only() && op.is_write() {
            return Err(SubmitError::ReadOnly);
        }
        if op != BioOp::Flush && blocks == 0 {
            return Err(SubmitError::InvalidRequest(BioReqError::EmptyRange));
        }
        let sector_scale = u64::from(logical_block_size / VIRTIO_BLK_SECTOR_SIZE);
        let num_sectors = u64::from(blocks)
            .checked_mul(sector_scale)
            .ok_or(SubmitError::InvalidRequest(BioReqError::OutOfBounds))?;
        let sector = match op {
            BioOp::Flush => 0,
            _ => lba
                .checked_mul(sector_scale)
                .ok_or(SubmitError::InvalidRequest(BioReqError::OutOfBounds))?,
        };
        match op {
            BioOp::Read => Ok(Self {
                req_type: REQ_TYPE_IN,
                sector,
                descriptor_count: 3,
                data_len: checked_bio_payload_len(blocks, logical_block_size)?,
                data_align: 1,
                data_direction: Some(DmaDirection::FromDevice),
                data_device_writable: true,
                data_payload: VirtioBlkDataPayload::BioBuffer,
            }),
            BioOp::Write => Ok(Self {
                req_type: REQ_TYPE_OUT,
                sector,
                descriptor_count: 3,
                data_len: checked_bio_payload_len(blocks, logical_block_size)?,
                data_align: 1,
                data_direction: Some(DmaDirection::ToDevice),
                data_device_writable: false,
                data_payload: VirtioBlkDataPayload::BioBuffer,
            }),
            BioOp::Flush => {
                if !capabilities.features.has_flush() {
                    return Err(SubmitError::Unsupported);
                }
                Ok(Self {
                    req_type: REQ_TYPE_FLUSH,
                    sector,
                    descriptor_count: 2,
                    data_len: 0,
                    data_align: 1,
                    data_direction: None,
                    data_device_writable: false,
                    data_payload: VirtioBlkDataPayload::None,
                })
            }
            BioOp::Discard => {
                let num_sectors = u32::try_from(num_sectors)
                    .map_err(|_| SubmitError::InvalidRequest(BioReqError::TooLarge))?;
                let limits = capabilities.discard.ok_or(SubmitError::Unsupported)?;
                if !limits.sector_aligned(sector) {
                    return Err(SubmitError::InvalidRequest(BioReqError::Misaligned));
                }
                if !limits.supports_single_segment_count(num_sectors) {
                    return Err(SubmitError::InvalidRequest(BioReqError::TooLarge));
                }
                Ok(Self {
                    req_type: REQ_TYPE_DISCARD,
                    sector: 0,
                    descriptor_count: 3,
                    data_len: mem::size_of::<VirtioBlkRangeSegment>(),
                    data_align: mem::align_of::<VirtioBlkRangeSegment>(),
                    data_direction: Some(DmaDirection::ToDevice),
                    data_device_writable: false,
                    data_payload: VirtioBlkDataPayload::RangeSegment(VirtioBlkRangeSegment::new(
                        sector,
                        num_sectors,
                        0,
                    )),
                })
            }
            BioOp::WriteZeroes => {
                let num_sectors = u32::try_from(num_sectors)
                    .map_err(|_| SubmitError::InvalidRequest(BioReqError::TooLarge))?;
                let limits = capabilities.write_zeroes.ok_or(SubmitError::Unsupported)?;
                if !limits.sector_aligned(sector) {
                    return Err(SubmitError::InvalidRequest(BioReqError::Misaligned));
                }
                if !limits.supports_single_segment_count(num_sectors) {
                    return Err(SubmitError::InvalidRequest(BioReqError::TooLarge));
                }
                let flags = if capabilities.write_zeroes_may_unmap {
                    WRITE_ZEROES_FLAG_UNMAP
                } else {
                    0
                };
                Ok(Self {
                    req_type: REQ_TYPE_WRITE_ZEROES,
                    sector: 0,
                    descriptor_count: 3,
                    data_len: mem::size_of::<VirtioBlkRangeSegment>(),
                    data_align: mem::align_of::<VirtioBlkRangeSegment>(),
                    data_direction: Some(DmaDirection::ToDevice),
                    data_device_writable: false,
                    data_payload: VirtioBlkDataPayload::RangeSegment(VirtioBlkRangeSegment::new(
                        sector,
                        num_sectors,
                        flags,
                    )),
                })
            }
        }
    }

    pub fn meta(&self) -> VirtioBlkReqMeta {
        VirtioBlkReqMeta {
            header: VirtioBlkReqHeader {
                req_type: self.req_type,
                reserved: 0,
                sector: self.sector,
            },
            status: STATUS_PENDING,
            _pad: [0; 7],
        }
    }

    pub const fn has_data_descriptor(&self) -> bool {
        !matches!(self.data_payload, VirtioBlkDataPayload::None)
    }

    pub const fn expects_bio_buffer(&self) -> bool {
        matches!(self.data_payload, VirtioBlkDataPayload::BioBuffer)
    }

    /// 设备在 used ring 中至少应声明写回的字节数。
    ///
    /// virtio-blk 每个请求都有一个 status 字节由设备写回；读请求还会把数据
    /// descriptor 一并写回。完成路径用这个值识别短写，避免状态字节为 OK 但
    /// 数据缓冲未完整写入时误报成功。
    pub fn expected_device_write_len(&self) -> Result<u32, SubmitError> {
        let data_len = if self.data_device_writable {
            self.data_len
        } else {
            0
        };
        data_len
            .checked_add(mem::size_of::<u8>())
            .and_then(|len| u32::try_from(len).ok())
            .ok_or(SubmitError::InvalidRequest(BioReqError::TooLarge))
    }
}

fn checked_bio_payload_len(blocks: u32, logical_block_size: u32) -> Result<usize, SubmitError> {
    (blocks as usize)
        .checked_mul(logical_block_size as usize)
        .ok_or(SubmitError::InvalidRequest(BioReqError::TooLarge))
}

pub(super) const fn req_header_size() -> u32 {
    mem::size_of::<VirtioBlkReqHeader>() as u32
}

pub(super) const fn req_status_offset() -> u64 {
    mem::size_of::<VirtioBlkReqHeader>() as u64
}

pub(super) fn status_to_result(status: u8) -> Result<(), BioIoError> {
    match status {
        STATUS_OK => Ok(()),
        STATUS_UNSUPP => Err(BioIoError::Unsupported),
        _ => Err(BioIoError::MediaError),
    }
}

/// 校验 used ring 中设备声明的写回长度。
///
/// 这里接受比预期更大的长度：部分设备会按内部合并后的 in descriptor 长度回报。
/// 但长度小于 status/data 的最小写回需求时，请求不能被视为成功。
pub(super) fn validate_used_write_len(expected: u32, actual: u32) -> Result<(), BioIoError> {
    if actual < expected {
        Err(BioIoError::Unavailable)
    } else {
        Ok(())
    }
}

/// 校验 BIO 自带缓冲区是否符合请求规划。
///
/// 读写请求必须携带精确长度的数据缓冲；flush/discard/write-zeroes 的协议 payload
/// 由驱动自己构造，调用方不能额外塞入数据缓冲，否则容易隐藏上层参数错误。
pub(super) fn validate_bio_buffer_for_plan(
    plan: VirtioBlkRequestPlan,
    bio: &Bio,
) -> Result<(), SubmitError> {
    if plan.expects_bio_buffer() {
        if bio.buffer.len() != plan.data_len {
            return Err(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch));
        }
    } else if !matches!(&bio.buffer, BioBuffer::None) {
        return Err(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch));
    }
    Ok(())
}

/// virtio-blk 提交路径需要的队列能力。
///
/// MMIO 与 PCI 队列结构各自保存 pending 表和失败状态，但 DMA 缓冲池、split
/// queue 访问方式一致。用这个小接口把“协议层如何准备一次请求”从传输层拆出。
pub(super) trait VirtioBlkDmaQueue {
    fn split_queue(&mut self) -> &mut SplitVirtQueue;
    fn take_meta_dma(&mut self) -> Option<DmaBuffer>;
    fn recycle_meta_dma(&mut self, meta_dma: DmaBuffer);
    fn take_data_dma(
        &mut self,
        len: usize,
        align: usize,
        direction: DmaDirection,
    ) -> Option<DmaBuffer>;
    fn recycle_data_dma(&mut self, data_dma: DmaBuffer);

    fn recycle_request_dma(&mut self, meta_dma: DmaBuffer, data_dma: Option<DmaBuffer>) {
        self.recycle_meta_dma(meta_dma);
        if let Some(data_dma) = data_dma {
            self.recycle_data_dma(data_dma);
        }
    }
}

/// 已分配但尚未发布到 available ring 的请求资源。
pub(super) struct VirtioBlkAllocatedRequest {
    pub chain: DescriptorChain,
    pub head: u16,
    pub meta_dma: DmaBuffer,
    pub data_dma: Option<DmaBuffer>,
}

/// 为一次请求分配描述符链和 DMA 缓冲，并写入请求头。
pub(super) fn allocate_request<Q: VirtioBlkDmaQueue>(
    queue: &mut Q,
    plan: VirtioBlkRequestPlan,
) -> Result<VirtioBlkAllocatedRequest, SubmitError> {
    if queue.split_queue().free_descriptor_count() < plan.descriptor_count {
        return Err(SubmitError::QueueFull);
    }
    let chain = queue
        .split_queue()
        .alloc_chain(plan.descriptor_count)
        .map_err(|_| SubmitError::QueueFull)?;
    let head = chain.head();
    let dma_context = queue.split_queue().dma_context();

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
                let _ = queue.split_queue().free_chain(chain);
                return Err(SubmitError::OutOfMemory);
            }
        },
    };
    let meta = plan.meta();
    unsafe {
        core::ptr::write(meta_dma.vaddr() as *mut VirtioBlkReqMeta, meta);
    }
    meta_dma.sync_for_device();

    let data_dma = if plan.has_data_descriptor() {
        let direction = match plan.data_direction {
            Some(direction) => direction,
            None => {
                let _ = queue.split_queue().free_chain(chain);
                queue.recycle_meta_dma(meta_dma);
                return Err(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch));
            }
        };
        match queue.take_data_dma(plan.data_len, plan.data_align, direction) {
            Some(buffer) => Some(buffer),
            None => match DmaBuffer::new_in(dma_context, plan.data_len, plan.data_align, direction)
            {
                Ok(buffer) => Some(buffer),
                Err(_) => {
                    let _ = queue.split_queue().free_chain(chain);
                    queue.recycle_meta_dma(meta_dma);
                    return Err(SubmitError::OutOfMemory);
                }
            },
        }
    } else {
        None
    };

    Ok(VirtioBlkAllocatedRequest {
        chain,
        head,
        meta_dma,
        data_dma,
    })
}

/// 释放尚未发布给设备的请求资源。
pub(super) fn free_allocated_request<Q: VirtioBlkDmaQueue>(
    queue: &mut Q,
    request: VirtioBlkAllocatedRequest,
) {
    let _ = queue.split_queue().free_chain(request.chain);
    queue.recycle_request_dma(request.meta_dma, request.data_dma);
}

/// 在提交锁外写入数据 DMA payload。
///
/// Read 请求的数据缓冲由设备填充，不在这里写；Write、Discard、WriteZeroes 的
/// payload 由 CPU 先写入 DMA 缓冲，再统一同步给设备。
pub(super) fn write_data_payload(
    plan: VirtioBlkRequestPlan,
    bio: &Bio,
    data_dma: &mut Option<DmaBuffer>,
) -> Result<(), SubmitError> {
    let Some(dma) = data_dma.as_mut() else {
        return Ok(());
    };
    match plan.data_payload {
        VirtioBlkDataPayload::BioBuffer if plan.data_direction == Some(DmaDirection::ToDevice) => {
            if bio.buffer.len() != plan.data_len {
                return Err(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch));
            }
            dma.as_mut_slice()[..plan.data_len].copy_from_slice(bio.buffer.as_slice());
        }
        VirtioBlkDataPayload::RangeSegment(segment) => unsafe {
            core::ptr::write(dma.vaddr() as *mut VirtioBlkRangeSegment, segment);
        },
        _ => {}
    }
    dma.sync_for_device();
    Ok(())
}

/// 写入描述符链，但不发布 available ring。
pub(super) fn write_allocated_request_descriptors<Q: VirtioBlkDmaQueue>(
    queue: &mut Q,
    request: &VirtioBlkAllocatedRequest,
    plan: VirtioBlkRequestPlan,
) -> Result<(), SubmitError> {
    let data_len = u32::try_from(plan.data_len)
        .map_err(|_| SubmitError::InvalidRequest(BioReqError::TooLarge))?;
    write_request_descriptors(
        queue.split_queue(),
        &request.chain,
        plan,
        request.meta_dma.dma_addr() as u64,
        request.data_dma.as_ref().map(|dma| dma.dma_addr() as u64),
        data_len,
        request.meta_dma.dma_addr() as u64 + req_status_offset(),
    )
}

/// 按 virtio-blk 协议写入一次请求的 split virtqueue 描述符链。
///
/// 传输层只提供已经分配好的链和 DMA 地址；这里统一处理 header/data/status 的
/// 排列、设备写入方向标记以及 flush 的两段链，避免 MMIO/PCI 驱动各自维护一份
/// 容易漂移的描述符构造逻辑。
pub(super) fn write_request_descriptors(
    queue: &mut SplitVirtQueue,
    chain: &DescriptorChain,
    plan: VirtioBlkRequestPlan,
    header_dma: u64,
    data_dma: Option<u64>,
    data_len: u32,
    status_dma: u64,
) -> Result<(), SubmitError> {
    let d0 = chain
        .get(0)
        .ok_or(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch))?;

    if plan.has_data_descriptor() {
        let d1 = chain
            .get(1)
            .ok_or(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch))?;
        let d2 = chain
            .get(2)
            .ok_or(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch))?;
        let data_dma =
            data_dma.ok_or(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch))?;
        let data_flags = if plan.data_device_writable {
            VIRTQ_DESC_F_WRITE
        } else {
            0
        };
        let updates = [
            VirtqDescUpdate::new(d0, header_dma, req_header_size(), 0, Some(d1)),
            VirtqDescUpdate::new(d1, data_dma, data_len, data_flags, Some(d2)),
            VirtqDescUpdate::new(d2, status_dma, 1, VIRTQ_DESC_F_WRITE, None),
        ];
        queue
            .write_descs(&updates)
            .map_err(|_| SubmitError::QueueFull)
    } else {
        let d1 = chain
            .get(1)
            .ok_or(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch))?;
        let updates = [
            VirtqDescUpdate::new(d0, header_dma, req_header_size(), 0, Some(d1)),
            VirtqDescUpdate::new(d1, status_dma, 1, VIRTQ_DESC_F_WRITE, None),
        ];
        queue
            .write_descs(&updates)
            .map_err(|_| SubmitError::QueueFull)
    }
}

/// 简单的最佳适配 DMA 缓冲池。
///
/// 池只缓存已经完成或尚未发布给设备的缓冲；在途请求的所有权由 pending 表持有。
/// 这样可以降低小 I/O 热路径的分配成本，同时不会在设备 DMA 期间复用同一缓冲。
pub(super) struct DmaBufferPool {
    buffers: alloc::vec::Vec<DmaBuffer>,
    cached_bytes: usize,
    max_buffers: usize,
    max_bytes: usize,
}

impl DmaBufferPool {
    pub fn new(queue_size: u16, dma_context: DmaContext, profile: DmaBufferPoolProfile) -> Self {
        let max_buffers = profile.max_buffers(queue_size);
        Self {
            buffers: alloc::vec::Vec::with_capacity(max_buffers),
            cached_bytes: 0,
            max_buffers,
            max_bytes: profile.max_bytes(dma_context),
        }
    }

    pub fn take(&mut self, len: usize, align: usize, direction: DmaDirection) -> Option<DmaBuffer> {
        let align = align.max(1);
        let pos = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, buffer)| {
                buffer.direction() == direction
                    && buffer.len() >= len
                    && buffer.vaddr().is_multiple_of(align)
            })
            .min_by_key(|(_, buffer)| buffer.len())
            .map(|(pos, _)| pos)?;
        // 缓冲池没有稳定顺序语义；best-fit 位置已经选定后，用 swap_remove
        // 避免 Vec::remove 在热路径移动后续元素。
        let buffer = self.buffers.swap_remove(pos);
        self.cached_bytes = self.cached_bytes.saturating_sub(buffer.len());
        Some(buffer)
    }

    pub fn recycle(&mut self, buffer: DmaBuffer) {
        let len = buffer.len();
        if len > self.max_bytes
            || self.buffers.len() >= self.max_buffers
            || self.cached_bytes.saturating_add(len) > self.max_bytes
        {
            return;
        }
        self.cached_bytes += len;
        self.buffers.push(buffer);
    }
}
