//! 块设备抽象接口。
//!
//! - 驱动只实现纯异步 `queue_bio`，把请求提交到自己的设备队列。
//! - 同步是上层薄包装 `submit_bio_wait` = submit + `Completion::wait()`。
//! - 异步通过 `submit_bio_async` 返回 `BioFuture`（`impl Future`）。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::future::Future;
use core::num::NonZeroU32;
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use core::task::{Context, Poll};

use crate::dev::bio::{
    Bio, BioBuffer, BioCompletionObserver, BioError, BioIoError, BioOp, BioReqError, BioResult,
    SubmitError,
};
use crate::dev::completion::Completion;
use crate::dev::control::{
    BlockControlRequest, BlockControlResponse, BlockIoHints, ControlError, DriverControl,
};

// ── 重导出 ──────────────────────────────────────────────────────────────────
pub use crate::dev::bio::{
    BioReqError as BlockRequestError, BlockRange, SubmitError as BlockSubmitError,
};

/// 块设备对象实例序列号分配器。
///
/// 序列号只描述本次内核生命周期内的块设备对象实例，不参与 PnP 身份匹配，也不作为
/// 底层驱动查找键；驱动若能提供更稳定的介质序列，可在 [`BlockAttributes`] 中覆盖。
static NEXT_BLOCK_DISKSEQ: AtomicU64 = AtomicU64::new(1);

fn allocate_block_diskseq() -> u64 {
    loop {
        let current = NEXT_BLOCK_DISKSEQ.load(Ordering::Acquire).max(1);
        let next = current.checked_add(1).unwrap_or(1);
        if NEXT_BLOCK_DISKSEQ
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return current;
        }
    }
}

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
    discard: Option<BlockRangeLimits>,
    write_zeroes: Option<BlockRangeLimits>,
}

/// 范围类块命令的限制。
///
/// Discard / WriteZeroes 等命令不携带用户数据缓冲，它们的设备限制通常来自介质
/// 擦除粒度或协议 range descriptor，而不是普通数据 DMA 段大小。用独立结构表达
/// 可以避免把读写 I/O 的 DMA 限制误套到范围命令上。
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockRangeLimits {
    max_blocks_per_io: Option<NonZeroU32>,
    alignment_blocks: Option<NonZeroU32>,
}

impl BlockRangeLimits {
    pub const fn new(
        max_blocks_per_io: Option<NonZeroU32>,
        alignment_blocks: Option<NonZeroU32>,
    ) -> Self {
        Self {
            max_blocks_per_io,
            alignment_blocks,
        }
    }

    pub const fn max_blocks_per_io(&self) -> Option<NonZeroU32> {
        self.max_blocks_per_io
    }

    pub const fn alignment_blocks(&self) -> Option<NonZeroU32> {
        self.alignment_blocks
    }
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
            discard: None,
            write_zeroes: None,
        })
    }

    pub const fn unrestricted() -> Self {
        Self {
            max_blocks_per_io: None,
            optimal_blocks_per_io: None,
            buffer_alignment: None,
            discard: None,
            write_zeroes: None,
        }
    }

    pub const fn with_discard_limits(mut self, limits: Option<BlockRangeLimits>) -> Self {
        self.discard = limits;
        self
    }

    pub const fn with_write_zeroes_limits(mut self, limits: Option<BlockRangeLimits>) -> Self {
        self.write_zeroes = limits;
        self
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

    pub const fn discard_limits(&self) -> Option<BlockRangeLimits> {
        self.discard
    }

    pub const fn write_zeroes_limits(&self) -> Option<BlockRangeLimits> {
        self.write_zeroes
    }

    pub const fn range_limits_for(&self, op: BioOp) -> Option<BlockRangeLimits> {
        match op {
            BioOp::Discard => self.discard,
            BioOp::WriteZeroes => self.write_zeroes,
            _ => None,
        }
    }
}

/// 块设备可观测属性。
///
/// 这些字段用于 devtmpfs/sysfs/procfs 等兼容视图展示设备能力，不参与底层设备
/// 身份判定；底层寻址仍由 PnP identity 和 typed device object 完成。
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockAttributes {
    removable: bool,
    rotational: bool,
    queue_depth: Option<NonZeroU32>,
    diskseq: Option<u64>,
}

impl BlockAttributes {
    pub const fn new(
        removable: bool,
        rotational: bool,
        queue_depth: Option<NonZeroU32>,
        diskseq: Option<u64>,
    ) -> Self {
        Self {
            removable,
            rotational,
            queue_depth,
            diskseq,
        }
    }

    pub const fn removable(&self) -> bool {
        self.removable
    }

    pub const fn rotational(&self) -> bool {
        self.rotational
    }

    pub const fn queue_depth(&self) -> Option<NonZeroU32> {
        self.queue_depth
    }

    pub const fn diskseq(&self) -> Option<u64> {
        self.diskseq
    }

    pub const fn with_diskseq(&self, diskseq: u64) -> Self {
        Self {
            removable: self.removable,
            rotational: self.rotational,
            queue_depth: self.queue_depth,
            diskseq: Some(diskseq),
        }
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
    /// Discard 后读取同一区间能够稳定返回零。
    ///
    /// 这和 WRITE_ZEROES 是两个能力：WRITE_ZEROES 表示设备支持显式写零命令，
    /// DISCARD_ZEROES 表示丢弃后的介质可观测内容为零。没有明确保证时不能声明。
    pub const DISCARD_ZEROES: Self = Self(1 << 5);

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

/// 块设备 I/O 统计快照。
///
/// 字段使用通用块层语义：完成数按成功完成的 BIO 计数，扇区数按 512 字节兼容
/// 扇区换算，耗时以纳秒累计。sysfs/procfs 只消费快照，不直接修改这些计数。
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockIoStatsSnapshot {
    pub read_ios: u64,
    pub read_sectors: u64,
    pub read_time_ns: u64,
    pub write_ios: u64,
    pub write_sectors: u64,
    pub write_time_ns: u64,
    pub discard_ios: u64,
    pub discard_sectors: u64,
    pub discard_time_ns: u64,
    pub flush_ios: u64,
    pub flush_time_ns: u64,
    pub read_inflight: u64,
    pub write_inflight: u64,
}

#[derive(Default)]
struct BlockIoStats {
    read_ios: AtomicU64,
    read_sectors: AtomicU64,
    read_time_ns: AtomicU64,
    write_ios: AtomicU64,
    write_sectors: AtomicU64,
    write_time_ns: AtomicU64,
    discard_ios: AtomicU64,
    discard_sectors: AtomicU64,
    discard_time_ns: AtomicU64,
    flush_ios: AtomicU64,
    flush_time_ns: AtomicU64,
    read_inflight: AtomicU64,
    write_inflight: AtomicU64,
}

impl BlockIoStats {
    fn begin(&self, op: BioOp) {
        if matches!(op, BioOp::Read) {
            self.read_inflight.fetch_add(1, Ordering::AcqRel);
        } else {
            self.write_inflight.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn cancel(&self, op: BioOp) {
        self.finish_inflight(op);
    }

    fn finish_inflight(&self, op: BioOp) {
        let counter = if matches!(op, BioOp::Read) {
            &self.read_inflight
        } else {
            &self.write_inflight
        };
        counter.fetch_sub(1, Ordering::AcqRel);
    }

    fn snapshot(&self) -> BlockIoStatsSnapshot {
        BlockIoStatsSnapshot {
            read_ios: self.read_ios.load(Ordering::Acquire),
            read_sectors: self.read_sectors.load(Ordering::Acquire),
            read_time_ns: self.read_time_ns.load(Ordering::Acquire),
            write_ios: self.write_ios.load(Ordering::Acquire),
            write_sectors: self.write_sectors.load(Ordering::Acquire),
            write_time_ns: self.write_time_ns.load(Ordering::Acquire),
            discard_ios: self.discard_ios.load(Ordering::Acquire),
            discard_sectors: self.discard_sectors.load(Ordering::Acquire),
            discard_time_ns: self.discard_time_ns.load(Ordering::Acquire),
            flush_ios: self.flush_ios.load(Ordering::Acquire),
            flush_time_ns: self.flush_time_ns.load(Ordering::Acquire),
            read_inflight: self.read_inflight.load(Ordering::Acquire),
            write_inflight: self.write_inflight.load(Ordering::Acquire),
        }
    }
}

impl BioCompletionObserver for BlockIoStats {
    fn on_complete(
        &self,
        op: BioOp,
        range: BlockRange,
        block_size: NonZeroU32,
        submitted_ns: u64,
        result: Result<(), BioIoError>,
    ) {
        self.finish_inflight(op);
        if result.is_err() {
            return;
        }

        let elapsed_ns = sched::now_ns_public().saturating_sub(submitted_ns);
        let sectors =
            (range.blocks as u64).saturating_mul((block_size.get() as u64).max(512) / 512);
        match op {
            BioOp::Read => {
                self.read_ios.fetch_add(1, Ordering::AcqRel);
                self.read_sectors.fetch_add(sectors, Ordering::AcqRel);
                self.read_time_ns.fetch_add(elapsed_ns, Ordering::AcqRel);
            }
            BioOp::Write => {
                self.write_ios.fetch_add(1, Ordering::AcqRel);
                self.write_sectors.fetch_add(sectors, Ordering::AcqRel);
                self.write_time_ns.fetch_add(elapsed_ns, Ordering::AcqRel);
            }
            BioOp::Discard | BioOp::WriteZeroes => {
                self.discard_ios.fetch_add(1, Ordering::AcqRel);
                self.discard_sectors.fetch_add(sectors, Ordering::AcqRel);
                self.discard_time_ns.fetch_add(elapsed_ns, Ordering::AcqRel);
            }
            BioOp::Flush => {
                self.flush_ios.fetch_add(1, Ordering::AcqRel);
                self.flush_time_ns.fetch_add(elapsed_ns, Ordering::AcqRel);
            }
        }
    }
}

// ───────── 块设备驱动 trait（纯异步队列模型） ─────────

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

    /// 一个 VFS 块设备文件被打开时调用。
    ///
    /// 这是设备对象层的生命周期通知，不携带 fd、路径或权限位等 POSIX 信息。
    /// 默认 no-op；需要按打开引用管理资源的虚拟设备可以覆盖。
    fn open_file(&self) -> Result<(), ControlError> {
        Ok(())
    }

    /// 一个 VFS 块设备文件的最后引用释放时调用。
    ///
    /// 与 [`open_file`](Self::open_file) 成对使用。默认 no-op，硬件块设备通常
    /// 不需要感知普通文件描述符生命周期。
    fn release_file(&self) {}

    /// 处理需要由具体驱动覆盖的块设备 typed control。
    ///
    /// 大多数硬件块设备的容量、只读状态和 I/O hint 都可以由 [`BlockDevice`]
    /// 的静态描述直接回答；loop 这类虚拟设备的 backing file 会在运行期变化，
    /// 因此允许驱动在这里覆盖特定请求。返回 `None` 表示继续使用块层默认实现。
    fn control(
        &self,
        _req: BlockControlRequest,
    ) -> Option<Result<BlockControlResponse, ControlError>> {
        None
    }

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
    attributes: BlockAttributes,
    features: BlockFeatures,
    driver: Arc<dyn BlockDriver>,
    parent: Option<Arc<BlockDevice>>,
    io_stats: Arc<BlockIoStats>,
    state: AtomicU8,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockDeviceInit<'a> {
    pub name: &'a str,
    pub subsystem: &'static str,
    pub class: BlockClass,
    pub geometry: BlockGeometry,
    pub limits: BlockLimits,
    pub attributes: BlockAttributes,
    pub features: BlockFeatures,
}

impl BlockDevice {
    pub fn new(
        init: BlockDeviceInit<'_>,
        driver: Arc<dyn BlockDriver>,
        parent: Option<Arc<BlockDevice>>,
    ) -> Self {
        let diskseq = init
            .attributes
            .diskseq()
            .filter(|seq| *seq != 0)
            .unwrap_or_else(allocate_block_diskseq);
        let attributes = init.attributes.with_diskseq(diskseq);
        Self {
            name: init.name.into(),
            subsystem: init.subsystem,
            class: init.class,
            geometry: init.geometry,
            limits: init.limits,
            attributes,
            features: init.features,
            driver,
            parent,
            io_stats: Arc::new(BlockIoStats::default()),
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
    pub fn attributes(&self) -> BlockAttributes {
        self.attributes
    }
    pub fn features(&self) -> BlockFeatures {
        self.features
    }
    pub fn parent(&self) -> Option<Arc<BlockDevice>> {
        self.parent.as_ref().map(Arc::clone)
    }
    pub fn io_stats(&self) -> BlockIoStatsSnapshot {
        self.io_stats.snapshot()
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

    /// 通知底层驱动该块设备节点被打开。
    ///
    /// VFS 兼容层只在成功创建文件对象前调用一次；失败时打开操作返回给上层。
    pub fn open_file(&self) -> Result<(), ControlError> {
        if !self.is_active() {
            return Err(ControlError::NoDevice);
        }
        self.driver.open_file()
    }

    /// 通知底层驱动该块设备文件对象已释放。
    pub fn release_file(&self) {
        self.driver.release_file();
    }

    /// 执行块设备类 typed control。
    pub fn control(
        self: &Arc<Self>,
        req: BlockControlRequest,
    ) -> Result<BlockControlResponse, ControlError> {
        if !self.is_active() {
            return Err(ControlError::NoDevice);
        }

        if let Some(response) = self.driver.control(req) {
            return response;
        }

        match req {
            BlockControlRequest::GetReadOnly => Ok(BlockControlResponse::Bool(
                self.features.contains(BlockFeatures::READ_ONLY),
            )),
            BlockControlRequest::GetCapacityBytes => self
                .geometry
                .capacity_bytes()
                .map(BlockControlResponse::U64)
                .ok_or(ControlError::Invalid),
            BlockControlRequest::GetLogicalBlockSize => Ok(BlockControlResponse::U32(
                self.geometry.logical_block_size().get(),
            )),
            BlockControlRequest::GetPhysicalBlockSize => Ok(BlockControlResponse::U32(
                self.geometry.physical_block_size().get(),
            )),
            BlockControlRequest::GetIoHints => {
                let logical = self.geometry.logical_block_size().get();
                let optimal = self
                    .limits
                    .optimal_blocks_per_io()
                    .map(|blocks| blocks.get().saturating_mul(logical))
                    .unwrap_or(0);
                Ok(BlockControlResponse::IoHints(BlockIoHints {
                    min_io_size: logical,
                    optimal_io_size: optimal,
                    alignment_offset: self.limits.buffer_alignment().map(|_| 0).unwrap_or(0),
                    discard_zeroes: self.features.contains(BlockFeatures::DISCARD_ZEROES),
                    rotational: self.attributes.rotational(),
                }))
            }
            BlockControlRequest::GetDiskSeq => self
                .attributes
                .diskseq()
                .map(BlockControlResponse::U64)
                .ok_or(ControlError::Unsupported),
            BlockControlRequest::Flush => {
                if !self.features.contains(BlockFeatures::FLUSH) {
                    return Ok(BlockControlResponse::Done);
                }
                self.submit_bio_wait(
                    BioOp::Flush,
                    BlockRange { lba: 0, blocks: 0 },
                    BioBuffer::None,
                )
                .map_err(map_bio_control_error)?;
                Ok(BlockControlResponse::Done)
            }
        }
    }

    /// 尝试将内部 `BlockDriver` 向下转型为具体类型。
    pub fn downcast_driver<T: 'static>(&self) -> Option<&T> {
        if !self.is_active() {
            return None;
        }
        self.driver.as_any().downcast_ref::<T>()
    }

    // ── Bio 提交 API ──────────────────────────────────────

    /// 同步提交并阻塞等待。
    ///
    /// 当前实现通过主动 drain 推进硬件完成，不依赖中断。
    /// 调度器就绪时在 drain 间让出 CPU（yield）；否则纯 spin。
    pub fn submit_bio_wait(
        self: &Arc<Self>,
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
    ) -> Result<Bio, BioError> {
        self.validate_bio(op, range, &buffer)?;
        let observer: Arc<dyn BioCompletionObserver> = self.io_stats.clone();
        let submitted_ns = sched::now_ns_public();
        let (bio, completion) = Bio::new_with_observer(
            op,
            range,
            buffer,
            self.geometry.logical_block_size(),
            submitted_ns,
            Some(observer),
        );
        self.io_stats.begin(op);
        if let Err((err, _bio)) = self.driver.queue_bio(bio) {
            self.io_stats.cancel(op);
            return Err(BioError::Submit(err));
        }

        while !completion.is_done() {
            self.driver.drain();
            if completion.is_done() {
                break;
            }
            if sched::is_ready() {
                sched::schedule_once(0);
            } else {
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
        let observer: Arc<dyn BioCompletionObserver> = self.io_stats.clone();
        let submitted_ns = sched::now_ns_public();
        let (bio, completion) = Bio::new_with_observer(
            op,
            range,
            buffer,
            self.geometry.logical_block_size(),
            submitted_ns,
            Some(observer),
        );
        self.io_stats.begin(op);
        if let Err((err, _bio)) = self.driver.queue_bio(bio) {
            self.io_stats.cancel(op);
            return Err(BioError::Submit(err));
        }
        Ok(BioFuture { completion })
    }

    // ── 参数校验 ──────────────────────────────────────

    fn validate_bio(
        &self,
        op: BioOp,
        range: BlockRange,
        buffer: &BioBuffer,
    ) -> Result<(), BioError> {
        if !self.is_active() {
            return Err(BioError::Submit(SubmitError::DeviceGone));
        }
        if op.is_write() && self.features.contains(BlockFeatures::READ_ONLY) {
            return Err(BioError::Submit(SubmitError::ReadOnly));
        }
        match op {
            BioOp::Flush => {
                if !self.features.contains(BlockFeatures::FLUSH) {
                    return Err(BioError::Submit(SubmitError::Unsupported));
                }
                // Flush 是设备级缓存同步命令，不携带 LBA 范围和数据缓冲区；
                // 在这里统一拒绝带范围/缓冲区的请求，避免具体驱动各自猜测语义。
                if range.blocks != 0 {
                    return Err(BioError::Submit(SubmitError::InvalidRequest(
                        BioReqError::BufferSizeMismatch,
                    )));
                }
                if !matches!(buffer, BioBuffer::None) {
                    return Err(BioError::Submit(SubmitError::InvalidRequest(
                        BioReqError::BufferSizeMismatch,
                    )));
                }
                return Ok(());
            }
            BioOp::Discard | BioOp::WriteZeroes => {
                if op == BioOp::Discard && !self.features.contains(BlockFeatures::DISCARD) {
                    return Err(BioError::Submit(SubmitError::Unsupported));
                }
                if op == BioOp::WriteZeroes && !self.features.contains(BlockFeatures::WRITE_ZEROES)
                {
                    return Err(BioError::Submit(SubmitError::Unsupported));
                }
            }
            BioOp::Read | BioOp::Write => {}
        }

        if range.blocks == 0 {
            return Err(BioError::Submit(SubmitError::InvalidRequest(
                BioReqError::EmptyRange,
            )));
        }
        match op {
            BioOp::Discard | BioOp::WriteZeroes => {
                if let Some(limits) = self.limits.range_limits_for(op) {
                    if let Some(max) = limits.max_blocks_per_io() {
                        if range.blocks > max.get() {
                            return Err(BioError::Submit(SubmitError::InvalidRequest(
                                BioReqError::TooLarge,
                            )));
                        }
                    }
                    if let Some(alignment) = limits.alignment_blocks() {
                        if !range.lba.is_multiple_of(u64::from(alignment.get())) {
                            return Err(BioError::Submit(SubmitError::InvalidRequest(
                                BioReqError::Misaligned,
                            )));
                        }
                    }
                }
            }
            BioOp::Read | BioOp::Write => {
                if let Some(max) = self.limits.max_blocks_per_io() {
                    if range.blocks > max.get() {
                        return Err(BioError::Submit(SubmitError::InvalidRequest(
                            BioReqError::TooLarge,
                        )));
                    }
                }
            }
            BioOp::Flush => {}
        }
        if let Some(count) = self.geometry.block_count() {
            let end = range
                .lba
                .checked_add(range.blocks as u64)
                .ok_or(BioError::Submit(SubmitError::InvalidRequest(
                    BioReqError::OutOfBounds,
                )))?;
            if end > count {
                return Err(BioError::Submit(SubmitError::InvalidRequest(
                    BioReqError::OutOfBounds,
                )));
            }
        }

        if op.needs_data() {
            let block_size = self.geometry.logical_block_size().get() as usize;
            let expected =
                (range.blocks as usize)
                    .checked_mul(block_size)
                    .ok_or(BioError::Submit(SubmitError::InvalidRequest(
                        BioReqError::BufferSizeMismatch,
                    )))?;
            if buffer.len() != expected {
                return Err(BioError::Submit(SubmitError::InvalidRequest(
                    BioReqError::BufferSizeMismatch,
                )));
            }
            if let Some(alignment) = self.limits.buffer_alignment() {
                let align = alignment.get() as usize;
                if let BioBuffer::Owned(b) = buffer {
                    if !(b.as_ptr() as usize).is_multiple_of(align) {
                        return Err(BioError::Submit(SubmitError::InvalidRequest(
                            BioReqError::Misaligned,
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

impl DriverControl for Arc<BlockDevice> {
    type Request = BlockControlRequest;
    type Response = BlockControlResponse;
    type Error = ControlError;

    fn control(&self, req: Self::Request) -> Result<Self::Response, Self::Error> {
        BlockDevice::control(self, req)
    }
}

fn map_bio_control_error(err: BioError) -> ControlError {
    match err {
        BioError::Submit(SubmitError::DeviceGone) => ControlError::NoDevice,
        BioError::Submit(SubmitError::QueueFull) => ControlError::Busy,
        BioError::Submit(SubmitError::ReadOnly) => ControlError::Permission,
        BioError::Submit(SubmitError::Unsupported) => ControlError::Unsupported,
        BioError::Submit(SubmitError::OutOfMemory | SubmitError::InvalidRequest(_)) => {
            ControlError::Invalid
        }
        BioError::Io(_) => ControlError::Io,
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
