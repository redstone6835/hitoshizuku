//! 块设备抽象接口。
//!
//! - 驱动只实现纯异步 `queue_bio`（类似 Linux `blk_mq_ops::queue_rq`）。
//! - 同步是上层薄包装 `submit_bio_wait` = submit + `Completion::wait()`。
//! - 异步通过 `submit_bio_async` 返回 `BioFuture`（`impl Future`）。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::future::Future;
use core::num::NonZeroU32;
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::{Context, Poll};

use spin::mutex::Mutex;

use crate::dev::bio::{Bio, BioBuffer, BioError, BioOp, BioReqError, BioResult, SubmitError};
use crate::dev::completion::Completion;

// ── 重导出 ──────────────────────────────────────────────────────────────────
pub use crate::dev::bio::{
    BioReqError as BlockRequestError, BlockRange, SubmitError as BlockSubmitError,
};

// ───────── 几何信息与限制 ─────────

#[derive(Clone, Copy, Debug)]
pub struct BlockGeometry {
    logical_block_size: NonZeroU32,
    physical_block_size: NonZeroU32,
    block_count: Option<u64>,
}

impl BlockGeometry {
    pub fn new(logical: NonZeroU32, physical: NonZeroU32, count: Option<u64>) -> Option<Self> {
        if physical < logical {
            return None;
        }
        if !logical.get().is_power_of_two() || !physical.get().is_power_of_two() {
            return None;
        }
        if let Some(c) = count {
            if c == 0 {
                return None;
            }
        }
        Some(Self {
            logical_block_size: logical,
            physical_block_size: physical,
            block_count: count,
        })
    }

    pub fn logical_block_size(&self) -> NonZeroU32 {
        self.logical_block_size
    }

    pub fn physical_block_size(&self) -> NonZeroU32 {
        self.physical_block_size
    }

    pub fn block_count(&self) -> Option<u64> {
        self.block_count
    }

    pub fn capacity_bytes(&self) -> Option<u64> {
        self.block_count
            .and_then(|count| count.checked_mul(self.logical_block_size.get() as u64))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BlockLimits {
    max_blocks_per_io: Option<NonZeroU32>,
    optimal_blocks_per_io: Option<NonZeroU32>,
    buffer_alignment: Option<NonZeroU32>,
}

impl BlockLimits {
    pub fn new(
        max_blocks_per_io: Option<NonZeroU32>,
        optimal_blocks_per_io: Option<NonZeroU32>,
        buffer_alignment: Option<NonZeroU32>,
    ) -> Option<Self> {
        if let (Some(max), Some(optimal)) = (max_blocks_per_io, optimal_blocks_per_io) {
            if optimal.get() > max.get() {
                return None;
            }
        }
        if let Some(align) = buffer_alignment {
            if !align.get().is_power_of_two() {
                return None;
            }
        }
        Some(Self {
            max_blocks_per_io,
            optimal_blocks_per_io,
            buffer_alignment,
        })
    }

    pub const fn unrestricted() -> Self {
        Self {
            max_blocks_per_io: None,
            optimal_blocks_per_io: None,
            buffer_alignment: None,
        }
    }

    pub const fn max_blocks_per_io(&self) -> Option<NonZeroU32> {
        self.max_blocks_per_io
    }

    pub const fn optimal_blocks_per_io(&self) -> Option<NonZeroU32> {
        self.optimal_blocks_per_io
    }

    pub const fn buffer_alignment(&self) -> Option<NonZeroU32> {
        self.buffer_alignment
    }
}

/// 块设备功能标志
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BlockFeatures(pub u32);

impl BlockFeatures {
    pub const READ_ONLY: Self = Self(1 << 0);
    pub const FLUSH: Self = Self(1 << 1);
    pub const DISCARD: Self = Self(1 << 2);
    pub const WRITE_ZEROES: Self = Self(1 << 3);
    pub const FUA: Self = Self(1 << 4);

    #[inline]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for BlockFeatures {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for BlockFeatures {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

// ───────── 块设备驱动 trait（纯异步，类似 Linux blk-mq） ─────────

/// 块设备底层驱动接口。
///
/// 驱动只需实现纯异步提交。完成通知通过 `bio.complete()` 触发，同步/异步
/// 语义由 `BlockDevice` 上层包装决定——驱动无感。
pub trait BlockDriver: Send + Sync {
    /// 接受一个 Bio 请求并排入硬件队列。
    ///
    /// 驱动完成请求时必须调用 `bio.complete(Ok(()))` 或 `bio.complete(Err(...))`。
    /// 返回 `Err` 表示立即拒绝（队列满等），Bio 原样归还。
    fn queue_bio(&self, bio: Bio) -> Result<(), (SubmitError, Bio)>;

    /// 推进完成队列（中断或主动轮询时调用）。
    ///
    /// 驱动在此方法中检查硬件完成状态，对已完成的 Bio 调用 `complete()`。
    /// 不持有任何外部锁时被调用。
    fn drain(&self) {}

    /// 用于向下转型，获取具体驱动类型。
    fn as_any(&self) -> &dyn Any;
}

// ───────── 块设备对象 ─────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockClass {
    Whole,
    Partition,
    Virtual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceState {
    Active = 0,
    Gone = 1,
}

pub struct BlockDevice {
    name: Box<str>,
    subsystem: &'static str,
    class: BlockClass,
    geometry: BlockGeometry,
    limits: BlockLimits,
    features: BlockFeatures,
    driver: Arc<dyn BlockDriver>,
    parent: Option<Arc<BlockDevice>>,
    state: AtomicU8,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockDeviceInit<'a> {
    pub name: &'a str,
    pub subsystem: &'static str,
    pub class: BlockClass,
    pub geometry: BlockGeometry,
    pub limits: BlockLimits,
    pub features: BlockFeatures,
}

impl BlockDevice {
    pub fn new(
        init: BlockDeviceInit<'_>,
        driver: Arc<dyn BlockDriver>,
        parent: Option<Arc<BlockDevice>>,
    ) -> Self {
        Self {
            name: init.name.into(),
            subsystem: init.subsystem,
            class: init.class,
            geometry: init.geometry,
            limits: init.limits,
            features: init.features,
            driver,
            parent,
            state: AtomicU8::new(DeviceState::Active as u8),
        }
    }

    // ── 访问器 ──────────────────────────────────
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn subsystem(&self) -> &'static str {
        self.subsystem
    }
    pub fn class(&self) -> BlockClass {
        self.class
    }
    pub fn geometry(&self) -> &BlockGeometry {
        &self.geometry
    }
    pub fn limits(&self) -> &BlockLimits {
        &self.limits
    }
    pub fn features(&self) -> BlockFeatures {
        self.features
    }
    pub fn parent(&self) -> Option<Arc<BlockDevice>> {
        self.parent.as_ref().map(Arc::clone)
    }

    pub fn state(&self) -> DeviceState {
        match self.state.load(Ordering::Acquire) {
            0 => DeviceState::Active,
            _ => DeviceState::Gone,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state() == DeviceState::Active
    }

    pub fn mark_gone(&self) {
        self.state.store(DeviceState::Gone as u8, Ordering::Release);
    }

    /// 推进驱动完成队列。
    pub fn drain(&self) {
        self.driver.drain();
    }

    /// 尝试将内部 `BlockDriver` 向下转型为具体类型。
    pub fn downcast_driver<T: 'static>(&self) -> Option<&T> {
        if !self.is_active() {
            return None;
        }
        self.driver.as_any().downcast_ref::<T>()
    }

    // ── Bio 提交 API ──────────────────────────────────────

    /// 同步提交并阻塞等待（类似 Linux `submit_bio_wait`）。
    ///
    /// 调度器就绪时通过 WaitQueue 让出 CPU；否则自旋。
    pub fn submit_bio_wait(
        self: &Arc<Self>,
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
    ) -> Result<Bio, BioError> {
        self.validate_bio(op, range, &buffer)?;
        let (bio, completion) = Bio::new(op, range, buffer, self.geometry.logical_block_size());
        self.driver
            .queue_bio(bio)
            .map_err(|(e, _)| BioError::Submit(e))?;

        if !sched::is_ready() {
            while !completion.is_done() {
                self.driver.drain();
                core::hint::spin_loop();
            }
        }
        completion.wait()
    }

    /// 异步提交，返回 `BioFuture`。
    pub fn submit_bio_async(
        self: &Arc<Self>,
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
    ) -> Result<BioFuture, BioError> {
        self.validate_bio(op, range, &buffer)?;
        let (bio, completion) = Bio::new(op, range, buffer, self.geometry.logical_block_size());
        self.driver
            .queue_bio(bio)
            .map_err(|(e, _)| BioError::Submit(e))?;
        Ok(BioFuture { completion })
    }

    // ── 参数校验 ──────────────────────────────────────

    fn validate_bio(&self, op: BioOp, range: BlockRange, buffer: &BioBuffer) -> Result<(), BioError> {
        if !self.is_active() {
            return Err(BioError::Submit(SubmitError::DeviceGone));
        }
        if op.is_write() && self.features.contains(BlockFeatures::READ_ONLY) {
            return Err(BioError::Submit(SubmitError::ReadOnly));
        }
        match op {
            BioOp::Flush => return Ok(()),
            BioOp::Discard | BioOp::WriteZeroes => {
                if op == BioOp::Discard && !self.features.contains(BlockFeatures::DISCARD) {
                    return Err(BioError::Submit(SubmitError::Unsupported));
                }
                if op == BioOp::WriteZeroes && !self.features.contains(BlockFeatures::WRITE_ZEROES) {
                    return Err(BioError::Submit(SubmitError::Unsupported));
                }
            }
            BioOp::Read | BioOp::Write => {}
        }

        if range.blocks == 0 {
            return Err(BioError::Submit(SubmitError::InvalidRequest(BioReqError::EmptyRange)));
        }
        if let Some(max) = self.limits.max_blocks_per_io() {
            if range.blocks > max.get() {
                return Err(BioError::Submit(SubmitError::InvalidRequest(BioReqError::TooLarge)));
            }
        }
        if let Some(count) = self.geometry.block_count() {
            let end = range.lba.checked_add(range.blocks as u64)
                .ok_or(BioError::Submit(SubmitError::InvalidRequest(BioReqError::OutOfBounds)))?;
            if end > count {
                return Err(BioError::Submit(SubmitError::InvalidRequest(BioReqError::OutOfBounds)));
            }
        }

        if op.needs_data() {
            let block_size = self.geometry.logical_block_size().get() as usize;
            let expected = (range.blocks as usize).checked_mul(block_size)
                .ok_or(BioError::Submit(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch)))?;
            if buffer.len() != expected {
                return Err(BioError::Submit(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch)));
            }
            if let Some(alignment) = self.limits.buffer_alignment() {
                let align = alignment.get() as usize;
                if let BioBuffer::Owned(b) = buffer {
                    if !(b.as_ptr() as usize).is_multiple_of(align) {
                        return Err(BioError::Submit(SubmitError::InvalidRequest(BioReqError::Misaligned)));
                    }
                }
            }
        }
        Ok(())
    }
}

// ───────── BioFuture ─────────

/// 块 I/O Future。poll 时检查底层 Completion 状态。
pub struct BioFuture {
    completion: Arc<Completion<BioResult>>,
}

impl Future for BioFuture {
    type Output = BioResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.completion.poll(cx)
    }
}

// ───────── 注册表错误 ─────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRegistryError {
    NameExists,
    DeviceGone,
    OutOfMemory,
}

// ───────── 块设备动态注册表 ─────────

pub struct BlockDeviceList {
    devices: Mutex<Vec<Arc<BlockDevice>>>,
}

impl BlockDeviceList {
    pub const fn new() -> Self {
        Self {
            devices: Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, dev: &Arc<BlockDevice>) -> Result<Arc<BlockDevice>, BlockRegistryError> {
        if !dev.is_active() {
            return Err(BlockRegistryError::DeviceGone);
        }
        let mut list = self.devices.lock();
        list.retain(|existing| existing.is_active());
        if !dev.is_active() {
            return Err(BlockRegistryError::DeviceGone);
        }
        if list.iter().any(|d| d.name() == dev.name()) {
            return Err(BlockRegistryError::NameExists);
        }
        list.push(Arc::clone(dev));
        Ok(Arc::clone(dev))
    }

    pub fn lookup(&self, name: &str) -> Option<Arc<BlockDevice>> {
        let list = self.devices.lock();
        list.iter()
            .find(|d| d.name() == name && d.is_active())
            .cloned()
    }

    pub fn remove(&self, name: &str) -> bool {
        let mut list = self.devices.lock();
        if let Some(pos) = list.iter().position(|d| d.name() == name) {
            list[pos].mark_gone();
            list.swap_remove(pos);
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<Arc<BlockDevice>> {
        self.devices
            .lock()
            .iter()
            .filter(|dev| dev.is_active())
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.devices
            .lock()
            .iter()
            .filter(|dev| dev.is_active())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for BlockDeviceList {
    fn default() -> Self {
        Self::new()
    }
}
