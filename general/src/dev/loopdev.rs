//! 文件后端 loop 虚拟块设备。
//!
//! 本模块只描述 loop 设备的 typed 语义：一个可替换 backing object 被投影成
//! 通用块设备。用户态 fd、`ioctl` 命令号、`/dev/loop*` 名称发布等兼容层细节
//! 由 VFS 适配模块负责，不能进入这里。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;
use core::fmt::Write;
use core::num::NonZeroU32;

use vfs::sync::Spinlock;

use crate::dev::bio::{Bio, BioBuffer, BioIoError, BioOp, SubmitError};
use crate::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures,
    BlockGeometry, BlockLimits,
};
use crate::dev::control::{BlockControlRequest, BlockControlResponse, BlockIoHints, ControlError};

pub const LOOP_LOGICAL_BLOCK_SIZE: u32 = 512;
const LOOP_SUBSYSTEM: &str = "loop";
const LOOP_NAME_PREFIX: &str = "loop";

/// loop backing 的通用错误。
///
/// 这里不使用 VFS/POSIX errno；具体文件系统错误由 VFS 兼容层翻译成这些 typed
/// 结果，再交给 loop driver 转换为块 I/O 错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopBackingError {
    Io,
    NoDevice,
    ReadOnly,
    Invalid,
    Unsupported,
}

/// loop backing object 的最小能力集合。
///
/// VFS 适配层会把已经打开的普通文件包装成该 trait；未来也可以接入内核内存对象、
/// 远端块缓存等其他 backing，而不改变 loop 块设备驱动。
pub trait LoopBacking: Send + Sync {
    fn len(&self) -> Result<u64, LoopBackingError>;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, LoopBackingError>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, LoopBackingError>;
    fn sync(&self) -> Result<(), LoopBackingError>;
}

/// loop 运行期标志。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopFlags {
    pub autoclear: bool,
    pub partscan: bool,
    pub direct_io: bool,
}

/// attach backing 时的 typed 参数。
#[derive(Clone)]
pub struct LoopAttachOptions {
    pub backing: Arc<dyn LoopBacking>,
    pub file_name: Box<str>,
    pub read_only: bool,
    pub offset: u64,
    /// `None` 表示使用 backing 从 offset 起的完整可用空间。
    pub size_limit: Option<u64>,
    pub flags: LoopFlags,
}

/// loop 当前状态快照。
#[derive(Clone, Debug)]
pub struct LoopStatus {
    pub index: u32,
    pub attached: bool,
    pub read_only: bool,
    pub offset: u64,
    pub size_limit: Option<u64>,
    pub capacity_bytes: u64,
    pub file_name: Box<str>,
    pub flags: LoopFlags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopError {
    AlreadyAttached,
    NotAttached,
    Busy,
    Invalid,
    OutOfMemory,
    Io,
    NoDevice,
    ReadOnly,
    Unsupported,
}

struct LoopBackingState {
    backing: Arc<dyn LoopBacking>,
    file_name: Box<str>,
    read_only: bool,
    offset: u64,
    size_limit: Option<u64>,
    capacity_bytes: u64,
    flags: LoopFlags,
}

impl LoopBackingState {
    fn status(&self, index: u32) -> LoopStatus {
        LoopStatus {
            index,
            attached: true,
            read_only: self.read_only,
            offset: self.offset,
            size_limit: self.size_limit,
            capacity_bytes: self.capacity_bytes,
            file_name: self.file_name.clone(),
            flags: self.flags,
        }
    }
}

struct LoopDriverState {
    backing: Option<LoopBackingState>,
    active_ios: u32,
    open_files: u32,
}

impl LoopDriverState {
    const fn new() -> Self {
        Self {
            backing: None,
            active_ios: 0,
            open_files: 0,
        }
    }
}

struct LoopIoTarget {
    backing: Arc<dyn LoopBacking>,
    file_offset: u64,
    capacity_offset: u64,
    capacity_bytes: u64,
    read_only: bool,
}

/// loop 块设备驱动。
pub struct LoopDriver {
    index: u32,
    state: Spinlock<LoopDriverState>,
}

impl LoopDriver {
    fn new(index: u32) -> Self {
        Self {
            index,
            state: Spinlock::new(LoopDriverState::new()),
        }
    }

    pub const fn index(&self) -> u32 {
        self.index
    }

    pub fn is_attached(&self) -> bool {
        self.state.lock().backing.is_some()
    }

    pub fn status(&self) -> LoopStatus {
        self.state
            .lock()
            .backing
            .as_ref()
            .map(|state| state.status(self.index))
            .unwrap_or_else(|| LoopStatus {
                index: self.index,
                attached: false,
                read_only: false,
                offset: 0,
                size_limit: None,
                capacity_bytes: 0,
                file_name: Box::<str>::from(""),
                flags: LoopFlags::default(),
            })
    }

    pub fn attach(&self, options: LoopAttachOptions) -> Result<(), LoopError> {
        let capacity_bytes = compute_capacity(&options)?;
        let mut state = self.state.lock();
        if state.backing.is_some() {
            return Err(LoopError::AlreadyAttached);
        }
        state.backing = Some(LoopBackingState {
            backing: options.backing,
            file_name: options.file_name,
            read_only: options.read_only,
            offset: options.offset,
            size_limit: options.size_limit,
            capacity_bytes,
            flags: options.flags,
        });
        Ok(())
    }

    pub fn detach(&self) -> Result<(), LoopError> {
        let mut state = self.state.lock();
        if state.backing.is_none() {
            return Err(LoopError::NotAttached);
        }
        if state.active_ios != 0 {
            return Err(LoopError::Busy);
        }
        state.backing = None;
        Ok(())
    }

    pub fn set_status(
        &self,
        offset: u64,
        size_limit: Option<u64>,
        file_name: Option<Box<str>>,
        flags: LoopFlags,
    ) -> Result<(), LoopError> {
        let mut state = self.state.lock();
        let Some(backing) = state.backing.as_mut() else {
            return Err(LoopError::NotAttached);
        };
        let capacity_bytes = compute_capacity_for(backing.backing.as_ref(), offset, size_limit)?;
        backing.offset = offset;
        backing.size_limit = size_limit;
        backing.capacity_bytes = capacity_bytes;
        backing.flags = flags;
        if let Some(name) = file_name {
            backing.file_name = name;
        }
        Ok(())
    }

    pub fn resize_from_backing(&self) -> Result<(), LoopError> {
        let mut state = self.state.lock();
        let Some(backing) = state.backing.as_mut() else {
            return Err(LoopError::NotAttached);
        };
        backing.capacity_bytes =
            compute_capacity_for(backing.backing.as_ref(), backing.offset, backing.size_limit)?;
        Ok(())
    }

    fn begin_io(&self, op: BioOp, bio: &Bio) -> Result<LoopIoTarget, BioIoError> {
        let capacity_offset = bio
            .range
            .lba
            .checked_mul(LOOP_LOGICAL_BLOCK_SIZE as u64)
            .ok_or(BioIoError::MediaError)?;
        let io_len = (bio.range.blocks as u64)
            .checked_mul(LOOP_LOGICAL_BLOCK_SIZE as u64)
            .ok_or(BioIoError::MediaError)?;

        let mut state = self.state.lock();
        let Some(backing) = state.backing.as_ref() else {
            return Err(BioIoError::Unavailable);
        };
        if op.is_write() && backing.read_only {
            return Err(BioIoError::ReadOnly);
        }
        let end = capacity_offset
            .checked_add(io_len)
            .ok_or(BioIoError::MediaError)?;
        if end > backing.capacity_bytes {
            return Err(BioIoError::MediaError);
        }
        let file_offset = backing
            .offset
            .checked_add(capacity_offset)
            .ok_or(BioIoError::MediaError)?;
        let target = LoopIoTarget {
            backing: Arc::clone(&backing.backing),
            file_offset,
            capacity_offset,
            capacity_bytes: backing.capacity_bytes,
            read_only: backing.read_only,
        };
        state.active_ios = state.active_ios.saturating_add(1);
        Ok(target)
    }

    fn begin_flush(&self) -> Result<Arc<dyn LoopBacking>, BioIoError> {
        let mut state = self.state.lock();
        let Some(backing) = state.backing.as_ref() else {
            return Err(BioIoError::Unavailable);
        };
        let backing = Arc::clone(&backing.backing);
        state.active_ios = state.active_ios.saturating_add(1);
        Ok(backing)
    }

    fn finish_io(&self) {
        let mut state = self.state.lock();
        state.active_ios = state.active_ios.saturating_sub(1);
    }

    fn flush(&self) -> Result<(), LoopError> {
        let backing = self.begin_flush().map_err(map_bio_loop_error)?;
        let result = backing.sync().map_err(map_backing_loop_error);
        self.finish_io();
        result
    }
}

impl BlockDriver for LoopDriver {
    fn queue_bio(&self, mut bio: Bio) -> Result<(), (SubmitError, Bio)> {
        let result = match bio.op {
            BioOp::Read => match self.begin_io(BioOp::Read, &bio) {
                Ok(target) => {
                    let result = read_bio_from_backing(&target, &mut bio.buffer);
                    self.finish_io();
                    result
                }
                Err(err) => Err(err),
            },
            BioOp::Write => match self.begin_io(BioOp::Write, &bio) {
                Ok(target) => {
                    let result = write_bio_to_backing(&target, &bio.buffer);
                    self.finish_io();
                    result
                }
                Err(err) => Err(err),
            },
            BioOp::Flush => match self.begin_flush() {
                Ok(backing) => {
                    let result = backing.sync().map_err(map_backing_bio_error);
                    self.finish_io();
                    result
                }
                Err(err) => Err(err),
            },
            BioOp::Discard | BioOp::WriteZeroes => Err(BioIoError::Unsupported),
        };
        bio.complete(result);
        Ok(())
    }

    fn open_file(&self) -> Result<(), ControlError> {
        let mut state = self.state.lock();
        state.open_files = state.open_files.checked_add(1).ok_or(ControlError::Busy)?;
        Ok(())
    }

    fn release_file(&self) {
        let should_autoclear = {
            let mut state = self.state.lock();
            state.open_files = state.open_files.saturating_sub(1);
            state.open_files == 0
                && state.active_ios == 0
                && state
                    .backing
                    .as_ref()
                    .is_some_and(|backing| backing.flags.autoclear)
        };
        if should_autoclear {
            let _ = self.detach();
        }
    }

    fn control(
        &self,
        req: BlockControlRequest,
    ) -> Option<Result<BlockControlResponse, ControlError>> {
        match req {
            BlockControlRequest::GetReadOnly => {
                Some(Ok(BlockControlResponse::Bool(self.status().read_only)))
            }
            BlockControlRequest::GetCapacityBytes => {
                let status = self.status();
                if status.attached {
                    Some(Ok(BlockControlResponse::U64(status.capacity_bytes)))
                } else {
                    Some(Err(ControlError::NoDevice))
                }
            }
            BlockControlRequest::GetLogicalBlockSize
            | BlockControlRequest::GetPhysicalBlockSize => {
                Some(Ok(BlockControlResponse::U32(LOOP_LOGICAL_BLOCK_SIZE)))
            }
            BlockControlRequest::GetIoHints => {
                Some(Ok(BlockControlResponse::IoHints(BlockIoHints {
                    min_io_size: LOOP_LOGICAL_BLOCK_SIZE,
                    optimal_io_size: 0,
                    alignment_offset: 0,
                    discard_zeroes: false,
                    rotational: false,
                })))
            }
            BlockControlRequest::Flush => Some(
                self.flush()
                    .map(|()| BlockControlResponse::Done)
                    .map_err(map_loop_control_error),
            ),
            #[cfg(feature = "block-profile")]
            BlockControlRequest::GetDebugProfile => None,
            BlockControlRequest::GetDiskSeq => None,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 已构造但尚未发布到 VFS 兼容层的 loop 块设备对象。
pub struct LoopDeviceBundle {
    index: u32,
    name: Box<str>,
    driver: Arc<LoopDriver>,
    block: Arc<BlockDevice>,
}

impl LoopDeviceBundle {
    pub fn new(index: u32) -> Result<Self, LoopError> {
        let name = loop_name(index)?;
        let driver = Arc::new(LoopDriver::new(index));
        let block_driver: Arc<dyn BlockDriver> = driver.clone();
        let block_size = NonZeroU32::new(LOOP_LOGICAL_BLOCK_SIZE).ok_or(LoopError::Invalid)?;
        let geometry =
            BlockGeometry::new(block_size, block_size, None).ok_or(LoopError::Invalid)?;
        let block = Arc::new(BlockDevice::new(
            BlockDeviceInit {
                name: &name,
                subsystem: LOOP_SUBSYSTEM,
                class: BlockClass::Virtual,
                geometry,
                limits: BlockLimits::unrestricted(),
                attributes: BlockAttributes::new(false, false, None, None),
                features: BlockFeatures::FLUSH,
            },
            block_driver,
            None,
        ));
        Ok(Self {
            index,
            name,
            driver,
            block,
        })
    }

    pub const fn index(&self) -> u32 {
        self.index
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn driver(&self) -> Arc<LoopDriver> {
        Arc::clone(&self.driver)
    }

    pub fn block(&self) -> Arc<BlockDevice> {
        Arc::clone(&self.block)
    }
}

fn loop_name(index: u32) -> Result<Box<str>, LoopError> {
    let mut name = String::new();
    name.try_reserve(LOOP_NAME_PREFIX.len() + 10)
        .map_err(|_| LoopError::OutOfMemory)?;
    name.push_str(LOOP_NAME_PREFIX);
    write!(&mut name, "{}", index).map_err(|_| LoopError::OutOfMemory)?;
    Ok(name.into_boxed_str())
}

fn compute_capacity(options: &LoopAttachOptions) -> Result<u64, LoopError> {
    compute_capacity_for(options.backing.as_ref(), options.offset, options.size_limit)
}

fn compute_capacity_for(
    backing: &dyn LoopBacking,
    offset: u64,
    size_limit: Option<u64>,
) -> Result<u64, LoopError> {
    let len = backing.len().map_err(map_backing_loop_error)?;
    if offset >= len {
        return Err(LoopError::Invalid);
    }
    let available = len - offset;
    let limited = size_limit.unwrap_or(available).min(available);
    let capacity = limited / LOOP_LOGICAL_BLOCK_SIZE as u64 * LOOP_LOGICAL_BLOCK_SIZE as u64;
    if capacity == 0 {
        return Err(LoopError::Invalid);
    }
    Ok(capacity)
}

fn read_bio_from_backing(target: &LoopIoTarget, buffer: &mut BioBuffer) -> Result<(), BioIoError> {
    let mut relative = 0usize;
    for index in 0..buffer.segment_count() {
        let len = buffer
            .segment(index)
            .map(|segment| segment.len())
            .ok_or(BioIoError::MediaError)?;
        buffer
            .with_segment_mut(index, |segment| {
                read_exact_from_backing(target, relative, segment)
            })
            .ok_or(BioIoError::MediaError)??;
        relative = relative.checked_add(len).ok_or(BioIoError::MediaError)?;
    }
    Ok(())
}

fn write_bio_to_backing(target: &LoopIoTarget, buffer: &BioBuffer) -> Result<(), BioIoError> {
    let mut relative = 0usize;
    for index in 0..buffer.segment_count() {
        let len = buffer
            .segment(index)
            .map(|segment| segment.len())
            .ok_or(BioIoError::MediaError)?;
        buffer
            .with_segment(index, |segment| {
                write_exact_to_backing(target, relative, segment)
            })
            .ok_or(BioIoError::MediaError)??;
        relative = relative.checked_add(len).ok_or(BioIoError::MediaError)?;
    }
    Ok(())
}

fn read_exact_from_backing(
    target: &LoopIoTarget,
    relative: usize,
    buf: &mut [u8],
) -> Result<(), BioIoError> {
    let mut done = 0usize;
    while done < buf.len() {
        let offset = target
            .file_offset
            .checked_add(relative as u64)
            .and_then(|offset| offset.checked_add(done as u64))
            .ok_or(BioIoError::MediaError)?;
        let n = target
            .backing
            .read_at(offset, &mut buf[done..])
            .map_err(map_backing_bio_error)?;
        if n == 0 {
            for byte in &mut buf[done..] {
                *byte = 0;
            }
            return Ok(());
        }
        done = done.checked_add(n).ok_or(BioIoError::MediaError)?;
    }
    Ok(())
}

fn write_exact_to_backing(
    target: &LoopIoTarget,
    relative: usize,
    buf: &[u8],
) -> Result<(), BioIoError> {
    if target.read_only {
        return Err(BioIoError::ReadOnly);
    }
    let end = target
        .capacity_offset
        .checked_add(relative as u64)
        .and_then(|offset| offset.checked_add(buf.len() as u64))
        .ok_or(BioIoError::MediaError)?;
    if end > target.capacity_bytes {
        return Err(BioIoError::MediaError);
    }

    let mut done = 0usize;
    while done < buf.len() {
        let offset = target
            .file_offset
            .checked_add(relative as u64)
            .and_then(|offset| offset.checked_add(done as u64))
            .ok_or(BioIoError::MediaError)?;
        let n = target
            .backing
            .write_at(offset, &buf[done..])
            .map_err(map_backing_bio_error)?;
        if n == 0 {
            return Err(BioIoError::MediaError);
        }
        done = done.checked_add(n).ok_or(BioIoError::MediaError)?;
    }
    Ok(())
}

fn map_backing_bio_error(err: LoopBackingError) -> BioIoError {
    match err {
        LoopBackingError::Io | LoopBackingError::Invalid => BioIoError::MediaError,
        LoopBackingError::NoDevice => BioIoError::Unavailable,
        LoopBackingError::ReadOnly => BioIoError::ReadOnly,
        LoopBackingError::Unsupported => BioIoError::Unsupported,
    }
}

fn map_backing_loop_error(err: LoopBackingError) -> LoopError {
    match err {
        LoopBackingError::Io => LoopError::Io,
        LoopBackingError::NoDevice => LoopError::NoDevice,
        LoopBackingError::ReadOnly => LoopError::ReadOnly,
        LoopBackingError::Invalid => LoopError::Invalid,
        LoopBackingError::Unsupported => LoopError::Unsupported,
    }
}

fn map_bio_loop_error(err: BioIoError) -> LoopError {
    match err {
        BioIoError::MediaError => LoopError::Io,
        BioIoError::Unavailable => LoopError::NoDevice,
        BioIoError::Timeout => LoopError::Io,
        BioIoError::ReadOnly => LoopError::ReadOnly,
        BioIoError::Unsupported => LoopError::Unsupported,
    }
}

fn map_loop_control_error(err: LoopError) -> ControlError {
    match err {
        LoopError::AlreadyAttached | LoopError::Invalid => ControlError::Invalid,
        LoopError::NotAttached | LoopError::NoDevice => ControlError::NoDevice,
        LoopError::Busy => ControlError::Busy,
        LoopError::OutOfMemory => ControlError::Invalid,
        LoopError::Io => ControlError::Io,
        LoopError::ReadOnly => ControlError::Permission,
        LoopError::Unsupported => ControlError::Unsupported,
    }
}
