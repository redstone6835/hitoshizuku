//! 块设备抽象接口。
//!
//! 这个接口定义了块设备的基本操作，包括读写、提交、注册等。
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use spin::mutex::Mutex;

// ───────── 错误定义 ─────────

/// 硬件 / 协议层 I/O 错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockIoError {
    MediaError,
    Unavailable,
    Timeout,
    ReadOnly,
    Unsupported,
}

/// 调用者传入参数错误（不可恢复）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRequestError {
    EmptyRange,
    OutOfBounds,
    TooLarge,
    BufferSizeMismatch,
    Misaligned,
}

/// 提交层错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSubmitError {
    Unsupported,
    ReadOnly,
    QueueFull,
    DeviceGone,
    OutOfMemory,
    InvalidRequest(BlockRequestError),
}

/// 注册表操作错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRegistryError {
    NameExists,
    DeviceGone,
    OutOfMemory,
}

// ───────── 几何信息与限制 ─────────

#[derive(Clone, Copy, Debug)]
pub struct BlockGeometry {
    logical_block_size: NonZeroU32,
    physical_block_size: NonZeroU32,
    /// 设备容量（逻辑块数），`None` 表示未知或可变
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
        if let Some(c) = count
            && c == 0
        {
            return None;
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
        if let (Some(max), Some(optimal)) = (max_blocks_per_io, optimal_blocks_per_io)
            && optimal.get() > max.get()
        {
            return None;
        }

        if let Some(align) = buffer_alignment
            && !align.get().is_power_of_two()
        {
            return None;
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

/// 块设备功能标志（手动实现，不引入额外依赖）
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

// ───────── 请求与响应 ─────────

#[derive(Clone, Copy, Debug)]
pub struct BlockRange {
    pub lba: u64,
    pub blocks: u32,
}

/// 块设备 I/O 请求。
///
/// 请求对象携带缓冲区所有权。驱动完成请求时必须把同一个请求对象放入
/// [`BlockIoCompletion`] 返还给完成回调，保证成功和失败路径都不丢失缓冲区。
#[derive(Debug)]
pub enum BlockIoRequest {
    Read {
        range: BlockRange,
        buffer: Box<[u8]>,
    },
    Write {
        range: BlockRange,
        buffer: Box<[u8]>,
        fua: bool,
    },
    Discard {
        range: BlockRange,
    },
    WriteZeroes {
        range: BlockRange,
    },
    Flush,
}

/// I/O 完成结果。
pub struct BlockIoCompletion {
    pub request: BlockIoRequest,
    pub result: Result<(), BlockIoError>,
}

/// 异步完成回调。
pub type BlockCompletion = Box<dyn FnOnce(BlockIoCompletion) + Send>;

// ───────── 纯 I/O 接口 ─────────

/// 底层驱动必须实现的异步 I/O 接口
pub trait BlockIo: Send + Sync {
    /// 提交 I/O 请求和完成回调。
    ///
    /// - 成功返回 `Ok(())` 表示请求已被驱动接受，`completion` 将在未来某个时刻被调用恰好一次。
    /// - 失败返回 `Err((BlockSubmitError, BlockIoRequest, BlockCompletion))`，
    ///   此时 `completion` **不会被调用**，请求和回调的所有权交还给调用者。
    fn submit(
        &self,
        req: BlockIoRequest,
        completion: BlockCompletion,
    ) -> Result<(), (BlockSubmitError, BlockIoRequest, BlockCompletion)>;

    /// 推进设备完成队列。
    ///
    /// 中断驱动设备可以保留默认空实现；轮询或同步等待路径会调用此方法，确保已完成
    /// 请求可以在没有外部中断入口时被回收并触发 completion。
    fn poll(&self) {}

    /// 同步读若干扇区到虚拟地址缓冲区。
    ///
    /// 驱动内部负责地址转换（RAM 设备直接 memcpy，VirtIO 设备自行 virt_to_phys）。
    /// 跳过 `Box<[u8]>` 分配和完成回调的全部开销。
    ///
    /// 默认实现返回 `Unsupported`，调用方应回退到 `submit` 路径。
    fn read_sectors_sync(
        &self,
        _lba: u64,
        _count: u32,
        _buf: &mut [u8],
    ) -> Result<(), BlockIoError> {
        Err(BlockIoError::Unsupported)
    }

    /// 同步写若干扇区，对称于 `read_sectors_sync`。
    fn write_sectors_sync(
        &self,
        _lba: u64,
        _count: u32,
        _buf: &[u8],
    ) -> Result<(), BlockIoError> {
        Err(BlockIoError::Unsupported)
    }

    /// 同步读若干扇区到物理地址 `phys`(长度 `len`)的内存范围。
    ///
    /// 这是 FS 层调用 [`SyncBlockBackend`](super::block_sync::SyncBlockBackend) 的
    /// 快速路径 —— 用户缓冲区已是物理连续且内核直接映射,直接把它作为 VirtIO
    /// 描述符的数据指针,跳过 `Box<[u8]>` 分配 + 双向 memcpy。
    ///
    /// 默认实现返回 `Unsupported`,调用方应回退到 `submit` 路径。
    fn read_sectors_sync_phys(
        &self,
        _lba: u64,
        _count: u32,
        _phys: u64,
        _len: usize,
    ) -> Result<(), BlockIoError> {
        Err(BlockIoError::Unsupported)
    }

    /// 写路径的对称方法,语义同上。
    fn write_sectors_sync_phys(
        &self,
        _lba: u64,
        _count: u32,
        _phys: u64,
        _len: usize,
    ) -> Result<(), BlockIoError> {
        Err(BlockIoError::Unsupported)
    }

    /// 用于向下转型，获取具体驱动类型
    fn as_any(&self) -> &dyn Any;
}

/// 类型安全的块设备控制接口。
///
/// 与 [`BlockIo`] 正交：每种驱动自行定义控制请求、响应和错误类型。
pub use super::DriverControl;

// ───────── 块设备对象 ─────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockClass {
    Whole,
    Partition,
    Virtual,
}

/// 块设备的具体类型。
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockDeviceKind {
    /// VirtIO 块设备。
    VirtioBlk,
    /// NVMe 命名空间。
    NvmeNamespace,
    /// ATA / SATA 磁盘。
    AtaDisk,
    /// SCSI 磁盘。
    ScsiDisk,
    /// 内存盘。
    RamDisk,
    /// 回环块设备。
    Loop,
    /// 其他 MMIO 块设备。
    Mmio,
}

impl BlockDeviceKind {
    /// 返回用户空间可见名称前缀。
    pub fn name(&self) -> &'static str {
        match self {
            BlockDeviceKind::VirtioBlk => "vd",
            BlockDeviceKind::NvmeNamespace => "nvd",
            BlockDeviceKind::AtaDisk => "sd",
            BlockDeviceKind::ScsiDisk => "scd",
            BlockDeviceKind::RamDisk => "ramd",
            BlockDeviceKind::Loop => "loop",
            BlockDeviceKind::Mmio => "blk",
        }
    }
}

/// 设备状态，使用 `AtomicU8` 保证 `Sync`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceState {
    Active = 0,
    Gone = 1,
}

pub struct BlockDevice {
    name: Box<str>,
    kind: BlockDeviceKind,
    class: BlockClass,
    geometry: BlockGeometry,
    limits: BlockLimits,
    features: BlockFeatures,
    io: Arc<dyn BlockIo>,
    parent: Option<Arc<BlockDevice>>,
    state: AtomicU8,
    in_flight: AtomicUsize,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockDeviceInit<'a> {
    pub name: &'a str,
    pub kind: BlockDeviceKind,
    pub class: BlockClass,
    pub geometry: BlockGeometry,
    pub limits: BlockLimits,
    pub features: BlockFeatures,
}

impl BlockDevice {
    pub fn new(
        init: BlockDeviceInit<'_>,
        io: Arc<dyn BlockIo>,
        parent: Option<Arc<BlockDevice>>,
    ) -> Self {
        Self {
            name: init.name.into(),
            kind: init.kind,
            class: init.class,
            geometry: init.geometry,
            limits: init.limits,
            features: init.features,
            io,
            parent,
            state: AtomicU8::new(DeviceState::Active as u8),
            in_flight: AtomicUsize::new(0),
        }
    }

    // ── 访问器 ──────────────────────────────────
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> BlockDeviceKind {
        self.kind
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
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
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

    pub fn poll(&self) {
        self.io.poll();
    }

    /// 提交经过校验的 I/O 请求。
    ///
    /// 根据请求类型进行精确的参数检查，然后将请求传递给底层 `BlockIo`。
    /// 若提交失败，返回所有权。
    pub fn submit(
        self: &Arc<Self>,
        req: BlockIoRequest,
        completion: BlockCompletion,
    ) -> Result<(), (BlockSubmitError, BlockIoRequest, BlockCompletion)> {
        if let Err(err) = self.validate_request(&req) {
            return Err((err, req, completion));
        }

        let token = match self.try_begin_io() {
            Ok(token) => token,
            Err(err) => return Err((err, req, completion)),
        };

        // 写透设备（无 FLUSH 特性）：flush 是空操作，立即成功完成，不下发给驱动。
        if matches!(req, BlockIoRequest::Flush) && !self.features.contains(BlockFeatures::FLUSH) {
            token.finish();
            completion(BlockIoCompletion {
                request: req,
                result: Ok(()),
            });
            return Ok(());
        }

        let completion_token = Arc::clone(&token);
        let tracked_completion: BlockCompletion = Box::new(move |done| {
            completion_token.finish();
            completion(done);
        });

        match self.io.submit(req, tracked_completion) {
            Ok(()) => Ok(()),
            Err((err, req, completion)) => {
                token.finish();
                Err((err, req, completion))
            }
        }
    }

    /// 尝试将内部 `BlockIo` 向下转型为具体类型
    pub fn downcast_io<T: 'static>(&self) -> Option<&T> {
        if !self.is_active() {
            return None;
        }
        self.io.as_any().downcast_ref::<T>()
    }

    /// 同步读扇区到虚拟地址缓冲区（零拷贝快速路径）。
    ///
    /// 驱动内部负责地址转换。跳过 Box 分配和完成回调。
    /// 不支持时返回 `Unsupported`，调用方回退到 `submit` 路径。
    pub fn read_sync(
        self: &Arc<Self>,
        lba: u64,
        count: u32,
        buf: &mut [u8],
    ) -> Result<(), BlockSubmitError> {
        if !self.is_active() {
            return Err(BlockSubmitError::DeviceGone);
        }
        if count == 0 {
            return Err(BlockSubmitError::InvalidRequest(BlockRequestError::EmptyRange));
        }
        let bps = self.geometry.logical_block_size().get() as usize;
        let want = (count as usize).checked_mul(bps)
            .ok_or(BlockSubmitError::InvalidRequest(BlockRequestError::BufferSizeMismatch))?;
        if buf.len() < want {
            return Err(BlockSubmitError::InvalidRequest(BlockRequestError::BufferSizeMismatch));
        }
        if let Some(total) = self.geometry.block_count() {
            let end = lba.checked_add(count as u64)
                .ok_or(BlockSubmitError::InvalidRequest(BlockRequestError::OutOfBounds))?;
            if end > total {
                return Err(BlockSubmitError::InvalidRequest(BlockRequestError::OutOfBounds));
            }
        }
        let token = self.try_begin_io()?;
        let res = self.io.read_sectors_sync(lba, count, buf);
        token.finish();
        match res {
            Ok(()) => Ok(()),
            Err(BlockIoError::Unsupported) => Err(BlockSubmitError::Unsupported),
            Err(_) => Err(BlockSubmitError::InvalidRequest(BlockRequestError::OutOfBounds)),
        }
    }

    /// 同步写扇区（对称于 `read_sync`）。
    pub fn write_sync(
        self: &Arc<Self>,
        lba: u64,
        count: u32,
        buf: &[u8],
    ) -> Result<(), BlockSubmitError> {
        if !self.is_active() {
            return Err(BlockSubmitError::DeviceGone);
        }
        if count == 0 {
            return Err(BlockSubmitError::InvalidRequest(BlockRequestError::EmptyRange));
        }
        let bps = self.geometry.logical_block_size().get() as usize;
        let want = (count as usize).checked_mul(bps)
            .ok_or(BlockSubmitError::InvalidRequest(BlockRequestError::BufferSizeMismatch))?;
        if buf.len() < want {
            return Err(BlockSubmitError::InvalidRequest(BlockRequestError::BufferSizeMismatch));
        }
        if let Some(total) = self.geometry.block_count() {
            let end = lba.checked_add(count as u64)
                .ok_or(BlockSubmitError::InvalidRequest(BlockRequestError::OutOfBounds))?;
            if end > total {
                return Err(BlockSubmitError::InvalidRequest(BlockRequestError::OutOfBounds));
            }
        }
        let token = self.try_begin_io()?;
        let res = self.io.write_sectors_sync(lba, count, buf);
        token.finish();
        match res {
            Ok(()) => Ok(()),
            Err(BlockIoError::Unsupported) => Err(BlockSubmitError::Unsupported),
            Err(_) => Err(BlockSubmitError::InvalidRequest(BlockRequestError::OutOfBounds)),
        }
    }

    /// 同步读扇区到物理地址 `phys` 的内存范围。
    ///
    /// 这是 FS 层绕过 Box 分配 + memcpy 的快速路径。调用方负责:
    /// - `phys` 指向的内存物理连续、与 VirtIO DMA 兼容;
    /// - `len == count * logical_block_size`。
    ///
    /// 底层驱动若不支持 `read_sectors_sync_phys`,会返回
    /// `BlockIoError::Unsupported`;调用方应回退到 [`submit`](Self::submit) 路径。
    pub fn read_sync_phys(
        self: &Arc<Self>,
        lba: u64,
        count: u32,
        phys: u64,
        len: usize,
    ) -> Result<(), BlockSubmitError> {
        if !self.is_active() {
            return Err(BlockSubmitError::DeviceGone);
        }
        if count == 0 {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::EmptyRange,
            ));
        }
        let bps = self.geometry.logical_block_size().get() as usize;
        let want = (count as usize)
            .checked_mul(bps)
            .ok_or(BlockSubmitError::InvalidRequest(
                BlockRequestError::BufferSizeMismatch,
            ))?;
        if len < want {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::BufferSizeMismatch,
            ));
        }
        if let Some(max) = self.limits.max_blocks_per_io()
            && count > max.get()
        {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::TooLarge,
            ));
        }
        if let Some(total) = self.geometry.block_count() {
            let end = lba
                .checked_add(count as u64)
                .ok_or(BlockSubmitError::InvalidRequest(
                    BlockRequestError::OutOfBounds,
                ))?;
            if end > total {
                return Err(BlockSubmitError::InvalidRequest(
                    BlockRequestError::OutOfBounds,
                ));
            }
        }
        let token = self.try_begin_io()?;
        let res = self.io.read_sectors_sync_phys(lba, count, phys, want);
        token.finish();
        match res {
            Ok(()) => Ok(()),
            Err(BlockIoError::Unsupported) => Err(BlockSubmitError::Unsupported),
            Err(_) => Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::OutOfBounds,
            )),
        }
    }

    /// 写路径的同步快速路径(对称 [`Self::read_sync_phys`])。
    pub fn write_sync_phys(
        self: &Arc<Self>,
        lba: u64,
        count: u32,
        phys: u64,
        len: usize,
    ) -> Result<(), BlockSubmitError> {
        if !self.is_active() {
            return Err(BlockSubmitError::DeviceGone);
        }
        if self.features.contains(BlockFeatures::READ_ONLY) {
            return Err(BlockSubmitError::ReadOnly);
        }
        if count == 0 {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::EmptyRange,
            ));
        }
        let bps = self.geometry.logical_block_size().get() as usize;
        let want = (count as usize)
            .checked_mul(bps)
            .ok_or(BlockSubmitError::InvalidRequest(
                BlockRequestError::BufferSizeMismatch,
            ))?;
        if len < want {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::BufferSizeMismatch,
            ));
        }
        if let Some(max) = self.limits.max_blocks_per_io()
            && count > max.get()
        {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::TooLarge,
            ));
        }
        if let Some(total) = self.geometry.block_count() {
            let end = lba
                .checked_add(count as u64)
                .ok_or(BlockSubmitError::InvalidRequest(
                    BlockRequestError::OutOfBounds,
                ))?;
            if end > total {
                return Err(BlockSubmitError::InvalidRequest(
                    BlockRequestError::OutOfBounds,
                ));
            }
        }
        let token = self.try_begin_io()?;
        let res = self.io.write_sectors_sync_phys(lba, count, phys, want);
        token.finish();
        match res {
            Ok(()) => Ok(()),
            Err(BlockIoError::Unsupported) => Err(BlockSubmitError::Unsupported),
            Err(_) => Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::OutOfBounds,
            )),
        }
    }

    fn try_begin_io(self: &Arc<Self>) -> Result<Arc<InFlightToken>, BlockSubmitError> {
        if !self.is_active() {
            return Err(BlockSubmitError::DeviceGone);
        }

        self.in_flight.fetch_add(1, Ordering::AcqRel);
        if !self.is_active() {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(BlockSubmitError::DeviceGone);
        }

        Ok(Arc::new(InFlightToken::new(Arc::clone(self))))
    }

    fn validate_request(&self, req: &BlockIoRequest) -> Result<(), BlockSubmitError> {
        if !self.is_active() {
            return Err(BlockSubmitError::DeviceGone);
        }

        match req {
            BlockIoRequest::Read { range, buffer } => {
                self.validate_range(*range)?;
                self.validate_buffer(*range, buffer)
            }
            BlockIoRequest::Write { range, buffer, fua } => {
                self.ensure_writable()?;
                if *fua && !self.features.contains(BlockFeatures::FUA) {
                    return Err(BlockSubmitError::Unsupported);
                }
                self.validate_range(*range)?;
                self.validate_buffer(*range, buffer)
            }
            BlockIoRequest::Discard { range } => {
                self.ensure_writable()?;
                if !self.features.contains(BlockFeatures::DISCARD) {
                    return Err(BlockSubmitError::Unsupported);
                }
                self.validate_range(*range)
            }
            BlockIoRequest::WriteZeroes { range } => {
                self.ensure_writable()?;
                if !self.features.contains(BlockFeatures::WRITE_ZEROES) {
                    return Err(BlockSubmitError::Unsupported);
                }
                self.validate_range(*range)
            }
            BlockIoRequest::Flush => {
                // 写透设备（无 FLUSH 特性）的 flush 由 submit 层直接短路为成功；
                // 有 FLUSH 特性的设备才会进入此分支。
                Ok(())
            }
        }
    }

    fn ensure_writable(&self) -> Result<(), BlockSubmitError> {
        if self.features.contains(BlockFeatures::READ_ONLY) {
            Err(BlockSubmitError::ReadOnly)
        } else {
            Ok(())
        }
    }

    fn validate_range(&self, range: BlockRange) -> Result<(), BlockSubmitError> {
        if range.blocks == 0 {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::EmptyRange,
            ));
        }

        if let Some(max) = self.limits.max_blocks_per_io()
            && range.blocks > max.get()
        {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::TooLarge,
            ));
        }

        let end_lba =
            range
                .lba
                .checked_add(range.blocks as u64)
                .ok_or(BlockSubmitError::InvalidRequest(
                    BlockRequestError::OutOfBounds,
                ))?;

        if let Some(count) = self.geometry.block_count()
            && end_lba > count
        {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::OutOfBounds,
            ));
        }

        Ok(())
    }

    fn validate_buffer(&self, range: BlockRange, buffer: &[u8]) -> Result<(), BlockSubmitError> {
        let block_size = self.geometry.logical_block_size().get() as usize;
        let expected = (range.blocks as usize).checked_mul(block_size).ok_or(
            BlockSubmitError::InvalidRequest(BlockRequestError::BufferSizeMismatch),
        )?;

        if buffer.len() != expected {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::BufferSizeMismatch,
            ));
        }
        if expected > u32::MAX as usize {
            return Err(BlockSubmitError::InvalidRequest(
                BlockRequestError::TooLarge,
            ));
        }

        if let Some(alignment) = self.limits.buffer_alignment() {
            let alignment = alignment.get() as usize;
            if !(buffer.as_ptr() as usize).is_multiple_of(alignment) {
                return Err(BlockSubmitError::InvalidRequest(
                    BlockRequestError::Misaligned,
                ));
            }
        }

        Ok(())
    }
}

struct InFlightToken {
    dev: Arc<BlockDevice>,
    armed: AtomicBool,
}

impl InFlightToken {
    fn new(dev: Arc<BlockDevice>) -> Self {
        Self {
            dev,
            armed: AtomicBool::new(true),
        }
    }

    fn finish(&self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.dev.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for InFlightToken {
    fn drop(&mut self) {
        self.finish();
    }
}

// ───────── 块设备动态注册表 ─────────

/// 无容量上限的块设备列表（受自旋锁保护）
///
/// 使用 `spin::mutex::Mutex` 保护内部 `Vec`，IRQ 安全性由调用方保证
///（与 `CharDevList` 一样，此列表面向堆可用后的动态注册场景）。
/// 不要在中断上下文调用 `push` / `remove` / `list`。
pub struct BlockDeviceList {
    devices: Mutex<Vec<Arc<BlockDevice>>>,
}

impl BlockDeviceList {
    pub const fn new() -> Self {
        Self {
            devices: Mutex::new(Vec::new()),
        }
    }

    /// 注册设备，返回 `Arc<BlockDev>` 句柄。
    ///
    /// 会检查名称唯一性，若重复则返回 `Err(BlockRegistryError::NameExists)`。
    pub fn push(&self, dev: &Arc<BlockDevice>) -> Result<Arc<BlockDevice>, BlockRegistryError> {
        if !dev.is_active() {
            return Err(BlockRegistryError::DeviceGone);
        }

        {
            let mut list = self.devices.lock();
            list.retain(|existing| existing.is_active());
            if !dev.is_active() {
                return Err(BlockRegistryError::DeviceGone);
            }
            if list.iter().any(|d| d.name() == dev.name()) {
                return Err(BlockRegistryError::NameExists);
            }
            if list.len() < list.capacity() {
                list.push(Arc::clone(dev));
                return Ok(Arc::clone(dev));
            }
        }

        loop {
            let initial_len = self.devices.lock().len();
            let needed = initial_len
                .checked_add(1)
                .ok_or(BlockRegistryError::OutOfMemory)?;
            let mut replacement = Vec::new();
            replacement
                .try_reserve(needed)
                .map_err(|_| BlockRegistryError::OutOfMemory)?;

            let mut list = self.devices.lock();
            list.retain(|existing| existing.is_active());
            if !dev.is_active() {
                return Err(BlockRegistryError::DeviceGone);
            }
            if list.iter().any(|d| d.name() == dev.name()) {
                return Err(BlockRegistryError::NameExists);
            }

            if list.len() < list.capacity() {
                list.push(Arc::clone(dev));
                return Ok(Arc::clone(dev));
            }

            let needed = list
                .len()
                .checked_add(1)
                .ok_or(BlockRegistryError::OutOfMemory)?;
            if needed > replacement.capacity() {
                continue;
            }

            replacement.extend(list.iter().cloned());
            replacement.push(Arc::clone(dev));
            let old = core::mem::replace(&mut *list, replacement);
            drop(list);
            drop(old);
            return Ok(Arc::clone(dev));
        }
    }

    /// 根据名称查找设备
    pub fn lookup(&self, name: &str) -> Option<Arc<BlockDevice>> {
        let list = self.devices.lock();
        list.iter()
            .find(|d| d.name() == name && d.is_active())
            .cloned()
    }

    /// 移除指定名称的设备，返回 `true` 表示成功移除
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

    /// 获取当前设备列表的快照（`Arc` 副本）
    pub fn list(&self) -> Result<Vec<Arc<BlockDevice>>, BlockRegistryError> {
        loop {
            let len = self.devices.lock().len();
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve(len)
                .map_err(|_| BlockRegistryError::OutOfMemory)?;

            let list = self.devices.lock();
            if list.len() <= snapshot.capacity() {
                snapshot.extend(list.iter().filter(|dev| dev.is_active()).cloned());
                return Ok(snapshot);
            }
        }
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
