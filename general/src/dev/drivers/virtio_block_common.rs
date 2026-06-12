//! VirtIO block 设备类型的公共协议逻辑。
//!
//! MMIO 与 PCI 只是传输层不同，块请求格式、状态码、描述符链形状和 DMA
//! 缓存策略完全相同。把这些内容集中到这里，避免两个驱动各自维护一份魔数和
//! 错误映射，后续新增传输层时也能复用同一套请求规划。

use core::mem;
use core::num::NonZeroU32;

use crate::dev::bio::{BioIoError, BioOp, BioReqError, SubmitError};
use crate::dev::block::{BlockFeatures, BlockLimits};
use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use crate::dev::virtio::{DescriptorChain, SplitVirtQueue, VIRTQ_DESC_F_WRITE};

use super::VIRTIO_BLK_SECTOR_SIZE;

/// virtio-blk 普通 I/O 至少需要 header/data/status 三个描述符。
pub(super) const MIN_QUEUE_SIZE: u16 = 4;
/// 设备 status 初始填充值。任何非 OK/UNSUPP 的最终值都按设备错误处理。
pub(super) const STATUS_PENDING: u8 = 0xff;

const REQ_TYPE_IN: u32 = 0;
const REQ_TYPE_OUT: u32 = 1;
const REQ_TYPE_FLUSH: u32 = 4;
const REQ_TYPE_DISCARD: u32 = 11;
const REQ_TYPE_WRITE_ZEROES: u32 = 13;

const STATUS_OK: u8 = 0;
const STATUS_UNSUPP: u8 = 2;

/// virtio-blk 设备能力位。
pub(super) const FEATURE_RO: u64 = 1 << 5;
pub(super) const FEATURE_BLK_SIZE: u64 = 1 << 6;
pub(super) const FEATURE_FLUSH: u64 = 1 << 9;
pub(super) const FEATURE_DISCARD: u64 = 1 << 13;
pub(super) const FEATURE_WRITE_ZEROES: u64 = 1 << 14;

/// virtio-blk 配置空间字段偏移，均相对于设备类型 config 起始地址。
pub(super) const CONFIG_CAPACITY_OFFSET: usize = 0x00;
pub(super) const CONFIG_BLK_SIZE_OFFSET: usize = 0x14;
pub(super) const CONFIG_MAX_DISCARD_SECTORS_OFFSET: usize = 0x28;
pub(super) const CONFIG_MAX_DISCARD_SEG_OFFSET: usize = 0x2c;
pub(super) const CONFIG_DISCARD_SECTOR_ALIGNMENT_OFFSET: usize = 0x30;
pub(super) const CONFIG_MAX_WRITE_ZEROES_SECTORS_OFFSET: usize = 0x34;
pub(super) const CONFIG_MAX_WRITE_ZEROES_SEG_OFFSET: usize = 0x38;
pub(super) const CONFIG_WRITE_ZEROES_MAY_UNMAP_OFFSET: usize = 0x3c;

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
        Self::new(4, 2 * 1024 * 1024)
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

/// 单段 discard/write-zeroes 类命令的设备限制。
#[derive(Clone, Copy, Debug)]
pub(super) struct VirtioBlkRangeOpLimits {
    pub max_sectors: u32,
    pub max_segments: u32,
}

impl VirtioBlkRangeOpLimits {
    pub const fn new(max_sectors: u32, max_segments: u32) -> Option<Self> {
        if max_sectors == 0 || max_segments == 0 {
            return None;
        }
        Some(Self {
            max_sectors,
            max_segments,
        })
    }

    pub const fn supports_single_segment(self, sectors: u32) -> bool {
        self.max_segments >= 1 && sectors <= self.max_sectors
    }
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

    pub fn block_features(self) -> BlockFeatures {
        let mut features = BlockFeatures(0);
        if self.features.read_only() {
            features |= BlockFeatures::READ_ONLY;
        }
        if self.features.has_flush() {
            features |= BlockFeatures::FLUSH;
        }
        if self.discard.is_some() {
            features |= BlockFeatures::DISCARD;
        }
        if self.write_zeroes.is_some() {
            features |= BlockFeatures::WRITE_ZEROES;
        }
        features
    }
}

/// 根据设备 DMA 能力和 virtio 描述符格式构造块层 I/O 限制。
///
/// 单个数据描述符的长度字段是 u32，同时 DMA 子系统可能给设备声明更小的单段
/// 上限；块层应看到二者的交集，避免提交后才因为 DMA 约束被驱动拒绝。
pub(super) fn block_limits(
    block_size: u32,
    dma_context: DmaContext,
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
    match BlockLimits::new(max_blocks, max_blocks, NonZeroU32::new(1)) {
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
        logical_block_size: u32,
        capabilities: VirtioBlkCapabilities,
    ) -> Result<Self, SubmitError> {
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
                data_direction: Some(DmaDirection::FromDevice),
                data_device_writable: true,
                data_payload: VirtioBlkDataPayload::BioBuffer,
            }),
            BioOp::Write => Ok(Self {
                req_type: REQ_TYPE_OUT,
                sector,
                descriptor_count: 3,
                data_len: checked_bio_payload_len(blocks, logical_block_size)?,
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
                    data_direction: None,
                    data_device_writable: false,
                    data_payload: VirtioBlkDataPayload::None,
                })
            }
            BioOp::Discard => {
                let num_sectors = u32::try_from(num_sectors)
                    .map_err(|_| SubmitError::InvalidRequest(BioReqError::TooLarge))?;
                let limits = capabilities.discard.ok_or(SubmitError::Unsupported)?;
                if !limits.supports_single_segment(num_sectors) {
                    return Err(SubmitError::InvalidRequest(BioReqError::TooLarge));
                }
                Ok(Self {
                    req_type: REQ_TYPE_DISCARD,
                    sector: 0,
                    descriptor_count: 3,
                    data_len: mem::size_of::<VirtioBlkRangeSegment>(),
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
                if !limits.supports_single_segment(num_sectors) {
                    return Err(SubmitError::InvalidRequest(BioReqError::TooLarge));
                }
                let flags = u32::from(capabilities.write_zeroes_may_unmap);
                Ok(Self {
                    req_type: REQ_TYPE_WRITE_ZEROES,
                    sector: 0,
                    descriptor_count: 3,
                    data_len: mem::size_of::<VirtioBlkRangeSegment>(),
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
        queue
            .write_desc(d0, header_dma, req_header_size(), 0, Some(d1))
            .and_then(|_| queue.write_desc(d1, data_dma, data_len, data_flags, Some(d2)))
            .and_then(|_| queue.write_desc(d2, status_dma, 1, VIRTQ_DESC_F_WRITE, None))
            .map_err(|_| SubmitError::QueueFull)
    } else {
        let d1 = chain
            .get(1)
            .ok_or(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch))?;
        queue
            .write_desc(d0, header_dma, req_header_size(), 0, Some(d1))
            .and_then(|_| queue.write_desc(d1, status_dma, 1, VIRTQ_DESC_F_WRITE, None))
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

    pub fn take(&mut self, len: usize, direction: DmaDirection) -> Option<DmaBuffer> {
        let pos = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, buffer)| buffer.direction() == direction && buffer.len() >= len)
            .min_by_key(|(_, buffer)| buffer.len())
            .map(|(pos, _)| pos)?;
        let buffer = self.buffers.remove(pos);
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
