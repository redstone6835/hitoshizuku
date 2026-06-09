//! 块 I/O 请求结构体。
//!
//! `Bio` 自包含：携带操作类型、块范围、数据缓冲区及完成槽位 `Arc<Completion<BioResult>>`。
//! 提交后所有权交给驱动；驱动调用 [`Bio::complete`] 时把自身（含数据缓冲区）通过
//! `Completion` 归还给等待者。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::num::NonZeroU32;

use crate::dev::completion::Completion;

// ── 块范围 ───────────────────────────────────────────────────────────────

/// 一段连续逻辑块。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRange {
    pub lba: u64,
    pub blocks: u32,
}

// ── 操作类型与缓冲区 ─────────────────────────────────────────────────────

/// Bio 携带的操作类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BioOp {
    Read,
    Write,
    Flush,
    Discard,
    WriteZeroes,
}

impl BioOp {
    /// 该操作是否需要随附数据缓冲区。
    pub const fn needs_data(self) -> bool {
        matches!(self, BioOp::Read | BioOp::Write)
    }

    /// 该操作是否会修改设备内容。
    pub const fn is_write(self) -> bool {
        matches!(
            self,
            BioOp::Write | BioOp::Discard | BioOp::WriteZeroes | BioOp::Flush
        )
    }
}

/// Bio 的数据缓冲区。
///
/// `Owned` 持有普通内核缓冲区；需要设备直接访问时，驱动应复制或映射到自己的
/// DMA 缓冲区，再把设备可见地址写入描述符。`None` 用于 Flush / Discard /
/// WriteZeroes 这类无数据操作。
#[derive(Debug)]
pub enum BioBuffer {
    Owned(Box<[u8]>),
    None,
}

impl BioBuffer {
    pub fn len(&self) -> usize {
        match self {
            BioBuffer::Owned(b) => b.len(),
            BioBuffer::None => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            BioBuffer::Owned(b) => b,
            BioBuffer::None => &[],
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            BioBuffer::Owned(b) => b,
            BioBuffer::None => &mut [],
        }
    }
}

// ── 错误类型 ─────────────────────────────────────────────────────────────

/// 提交失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    QueueFull,
    DeviceGone,
    OutOfMemory,
    Unsupported,
    ReadOnly,
    InvalidRequest(BioReqError),
}

/// 请求参数错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioReqError {
    EmptyRange,
    OutOfBounds,
    TooLarge,
    BufferSizeMismatch,
    Misaligned,
}

/// I/O 完成后可能的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioIoError {
    MediaError,
    Unavailable,
    Timeout,
    ReadOnly,
    Unsupported,
}

/// Bio 等待结果中的错误（聚合提交错误和 I/O 错误）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioError {
    Submit(SubmitError),
    Io(BioIoError),
}

impl From<SubmitError> for BioError {
    fn from(e: SubmitError) -> Self {
        BioError::Submit(e)
    }
}

impl From<BioIoError> for BioError {
    fn from(e: BioIoError) -> Self {
        BioError::Io(e)
    }
}

// ── Bio ──────────────────────────────────────────────────────────────────

pub type BioResult = Result<Bio, BioError>;

/// BIO 完成观察者。
///
/// 观察者只接收通用块 I/O 元数据，用于块设备通用层维护统计、inflight 等状态。
/// 具体驱动仍然只负责执行请求并调用 [`Bio::complete`]，不需要知道 sysfs 或
/// 兼容层如何展示这些信息。
pub trait BioCompletionObserver: Send + Sync {
    fn on_complete(
        &self,
        op: BioOp,
        range: BlockRange,
        block_size: NonZeroU32,
        submitted_ns: u64,
        result: Result<(), BioIoError>,
    );
}

/// 块 I/O 请求。
///
/// 提交后所有权进入驱动；驱动完成时调用 [`Bio::complete`] 把请求归还给
/// 持有 Completion 的等待者。
pub struct Bio {
    pub op: BioOp,
    pub range: BlockRange,
    pub buffer: BioBuffer,
    pub block_size: NonZeroU32,
    pub fua: bool,
    submitted_ns: u64,
    observer: Option<Arc<dyn BioCompletionObserver>>,
    completion: Arc<Completion<BioResult>>,
}

impl Bio {
    /// 创建新 Bio 与配套 Completion。
    pub fn new(
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
        block_size: NonZeroU32,
    ) -> (Self, Arc<Completion<BioResult>>) {
        Self::new_with_observer(op, range, buffer, block_size, 0, None)
    }

    /// 创建带完成观察者的新 Bio。
    pub fn new_with_observer(
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
        block_size: NonZeroU32,
        submitted_ns: u64,
        observer: Option<Arc<dyn BioCompletionObserver>>,
    ) -> (Self, Arc<Completion<BioResult>>) {
        let completion = Completion::new();
        let bio = Self {
            op,
            range,
            buffer,
            block_size,
            fua: false,
            submitted_ns,
            observer,
            completion: Arc::clone(&completion),
        };
        (bio, completion)
    }

    /// 标记 Force Unit Access（仅写请求有效）。
    pub fn with_fua(mut self, fua: bool) -> Self {
        self.fua = fua;
        self
    }

    /// 驱动完成请求时调用。
    ///
    /// `Ok(())` 表示成功——Bio 自身（包含数据缓冲区）通过 Completion 归还。
    /// `Err(e)` 表示失败——Bio 被消费，等待者收到 `Err(e)`。
    pub fn complete(self, result: Result<(), BioIoError>) {
        if let Some(observer) = self.observer.as_ref() {
            observer.on_complete(
                self.op,
                self.range,
                self.block_size,
                self.submitted_ns,
                result,
            );
        }
        let completion = Arc::clone(&self.completion);
        let value = match result {
            Ok(()) => Ok(self),
            Err(e) => Err(BioError::Io(e)),
        };
        completion.complete(value);
    }
}

impl core::fmt::Debug for Bio {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bio")
            .field("op", &self.op)
            .field("range", &self.range)
            .field("buffer_len", &self.buffer.len())
            .field("block_size", &self.block_size)
            .field("fua", &self.fua)
            .finish()
    }
}
