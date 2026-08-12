//! 块 I/O 请求结构体。
//!
//! `Bio` 自包含：携带操作类型、块范围、数据缓冲区及完成槽位。
//! 提交后所有权交给驱动；驱动调用 [`Bio::complete`] 时把自身（含数据缓冲区）通过
//! `Completion` 归还给等待者。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::marker::PhantomData;
use core::num::NonZeroU32;
use core::ptr::NonNull;

use crate::dev::completion::Completion;

/// 同步借用 BIO 最多内联保存的分段数。
///
/// 16 个数据段加 virtio-blk 的 header/status 共 18 个 descriptor，仍落在
/// split virtqueue 的 19 项内联链上，不会因为 fault-around 批量读额外分配。
pub const BIO_MAX_BORROWED_SEGMENTS: usize = 16;

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

/// BIO 借用缓冲区的访问方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BorrowedBioBufferKind {
    Read,
    Write,
}

/// 同步 BIO 使用的非拥有缓冲区。
///
/// 该类型只由块层同步提交接口构造，调用者必须在提交函数返回前保持原始 slice
/// 存活且不可并发访问。BIO 可以在驱动 pending 表中短暂跨函数移动，因此这里用
/// 原始指针表达“所有权仍在调用栈上”，由同步等待路径负责收束生命周期。
#[derive(Debug)]
pub struct BorrowedBioBuffer {
    ptr: NonNull<u8>,
    len: usize,
    kind: BorrowedBioBufferKind,
    _not_static_owner: PhantomData<*mut [u8]>,
}

impl BorrowedBioBuffer {
    fn from_read(buf: &mut [u8]) -> Self {
        Self {
            ptr: NonNull::new(buf.as_mut_ptr()).unwrap_or_else(NonNull::dangling),
            len: buf.len(),
            kind: BorrowedBioBufferKind::Read,
            _not_static_owner: PhantomData,
        }
    }

    fn from_write(buf: &[u8]) -> Self {
        Self {
            ptr: NonNull::new(buf.as_ptr() as *mut u8).unwrap_or_else(NonNull::dangling),
            len: buf.len(),
            kind: BorrowedBioBufferKind::Write,
            _not_static_owner: PhantomData,
        }
    }

    fn as_slice(&self) -> &[u8] {
        // Safety: 构造函数只接收有效 slice 指针和长度；同步 BIO 在完成前不会
        // 释放调用者缓冲区，读写方向只限制是否允许可变访问。
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        if self.kind != BorrowedBioBufferKind::Read {
            return &mut [];
        }
        // Safety: Read 借用缓冲来自 `&mut [u8]`，同步提交接口在 BIO 完成前
        // 独占该借用；驱动只通过归还的 BIO 访问这段内存。
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    const fn len(&self) -> usize {
        self.len
    }
}

// Safety: 非拥有 BIO 缓冲只允许通过块层同步提交接口构造；该接口等待 BIO
// 完成后才返回，因此 BIO 跨驱动队列移动时，调用者提供的 slice 仍然有效。
unsafe impl Send for BorrowedBioBuffer {}

/// BIO 中一个非拥有数据段的只读描述。
#[derive(Clone, Copy, Debug)]
pub struct BioBufferSegment {
    ptr: NonNull<u8>,
    len: usize,
}

impl BioBufferSegment {
    fn new(ptr: *mut u8, len: usize) -> Self {
        Self {
            ptr: NonNull::new(ptr).unwrap_or_else(NonNull::dangling),
            len,
        }
    }

    /// 段的内核虚拟地址。
    pub fn vaddr(self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// 段长度。
    pub const fn len(self) -> usize {
        self.len
    }

    unsafe fn as_slice<'a>(self) -> &'a [u8] {
        // Safety: 由调用方保证原始同步 BIO 借用仍然存活。
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    unsafe fn as_mut_slice<'a>(self) -> &'a mut [u8] {
        // Safety: 由调用方保证原始 Read 分段仍被同步 BIO 独占。
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

/// 同步 BIO 使用的内联 scatter/gather 借用缓冲区。
#[derive(Debug)]
pub struct BorrowedBioSegments {
    segments: [BioBufferSegment; BIO_MAX_BORROWED_SEGMENTS],
    segment_count: u8,
    total_len: usize,
    kind: BorrowedBioBufferKind,
    _not_static_owner: PhantomData<*mut [u8]>,
}

impl BorrowedBioSegments {
    fn from_read(bufs: &mut [&mut [u8]]) -> Result<Self, BioReqError> {
        if bufs.is_empty() {
            return Err(BioReqError::BufferSizeMismatch);
        }
        if bufs.len() > BIO_MAX_BORROWED_SEGMENTS {
            return Err(BioReqError::TooLarge);
        }

        let empty = BioBufferSegment::new(core::ptr::null_mut(), 0);
        let mut segments = [empty; BIO_MAX_BORROWED_SEGMENTS];
        let mut total_len = 0usize;
        for (slot, buf) in segments.iter_mut().zip(bufs.iter_mut()) {
            if buf.is_empty() {
                return Err(BioReqError::BufferSizeMismatch);
            }
            total_len = total_len
                .checked_add(buf.len())
                .ok_or(BioReqError::TooLarge)?;
            *slot = BioBufferSegment::new(buf.as_mut_ptr(), buf.len());
        }

        Ok(Self {
            segments,
            segment_count: bufs.len() as u8,
            total_len,
            kind: BorrowedBioBufferKind::Read,
            _not_static_owner: PhantomData,
        })
    }

    fn segment_count(&self) -> usize {
        usize::from(self.segment_count)
    }

    fn segment(&self, index: usize) -> Option<BioBufferSegment> {
        (index < self.segment_count()).then(|| self.segments[index])
    }
}

// Safety: 与单段 BorrowedBioBuffer 相同，所有原始 slice 都由同步提交入口保活到完成。
unsafe impl Send for BorrowedBioSegments {}

/// Bio 的数据缓冲区。
///
/// `Owned` 持有普通内核缓冲区；需要设备直接访问时，驱动应复制或映射到自己的
/// DMA 缓冲区，再把设备可见地址写入描述符。`Borrowed` 只用于同步提交路径，
/// 避免文件系统块后端每次 I/O 都分配临时缓冲并额外复制。`None` 用于
/// Flush / Discard / WriteZeroes 这类无数据操作。
#[derive(Debug)]
pub enum BioBuffer {
    Owned(Box<[u8]>),
    Borrowed(BorrowedBioBuffer),
    BorrowedSegments(BorrowedBioSegments),
    None,
}

#[kernel_symbols::export]
impl BioBuffer {
    pub(crate) fn borrowed_read(buf: &mut [u8]) -> Self {
        BioBuffer::Borrowed(BorrowedBioBuffer::from_read(buf))
    }

    pub(crate) fn borrowed_write(buf: &[u8]) -> Self {
        BioBuffer::Borrowed(BorrowedBioBuffer::from_write(buf))
    }

    pub(crate) fn borrowed_read_vectored(bufs: &mut [&mut [u8]]) -> Result<Self, BioReqError> {
        BorrowedBioSegments::from_read(bufs).map(BioBuffer::BorrowedSegments)
    }

    pub fn len(&self) -> usize {
        match self {
            BioBuffer::Owned(b) => b.len(),
            BioBuffer::Borrowed(b) => b.len(),
            BioBuffer::BorrowedSegments(b) => b.total_len,
            BioBuffer::None => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            BioBuffer::Owned(b) => b,
            BioBuffer::Borrowed(b) => b.as_slice(),
            BioBuffer::BorrowedSegments(_) => &[],
            BioBuffer::None => &[],
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            BioBuffer::Owned(b) => b,
            BioBuffer::Borrowed(b) => b.as_mut_slice(),
            BioBuffer::BorrowedSegments(_) => &mut [],
            BioBuffer::None => &mut [],
        }
    }

    /// 数据段数量。连续缓冲区返回 1，无数据 BIO 返回 0。
    #[kernel_symbols::export(
        name = "general.dev.bio.BioBuffer.segment_count",
        contract = "kernel.general.block-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER
    )]
    pub fn segment_count(&self) -> usize {
        match self {
            BioBuffer::Owned(_) | BioBuffer::Borrowed(_) => 1,
            BioBuffer::BorrowedSegments(b) => b.segment_count(),
            BioBuffer::None => 0,
        }
    }

    /// 返回指定数据段；调用方不得让该描述逃逸出 BIO 生命周期。
    #[kernel_symbols::export(
        name = "general.dev.bio.BioBuffer.segment",
        contract = "kernel.general.block-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER
    )]
    pub fn segment(&self, index: usize) -> Option<BioBufferSegment> {
        match self {
            BioBuffer::Owned(b) if index == 0 => {
                Some(BioBufferSegment::new(b.as_ptr() as *mut u8, b.len()))
            }
            BioBuffer::Borrowed(b) if index == 0 => {
                Some(BioBufferSegment::new(b.ptr.as_ptr(), b.len))
            }
            BioBuffer::BorrowedSegments(b) => b.segment(index),
            _ => None,
        }
    }

    /// 是否为同步入口构造的多段借用缓冲区。
    pub fn is_borrowed_vectored(&self) -> bool {
        matches!(self, BioBuffer::BorrowedSegments(_))
    }

    /// 在指定数据段的共享视图上执行闭包。
    pub fn with_segment<R>(&self, index: usize, visit: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let segment = self.segment(index)?;
        // Safety: 返回的 slice 只在闭包调用期间存活，不会逃逸出当前 BIO 借用。
        Some(visit(unsafe { segment.as_slice() }))
    }

    /// 在指定 Read 数据段的独占视图上执行闭包。
    pub fn with_segment_mut<R>(
        &mut self,
        index: usize,
        visit: impl FnOnce(&mut [u8]) -> R,
    ) -> Option<R> {
        if !matches!(
            self,
            BioBuffer::Owned(_)
                | BioBuffer::Borrowed(BorrowedBioBuffer {
                    kind: BorrowedBioBufferKind::Read,
                    ..
                })
                | BioBuffer::BorrowedSegments(BorrowedBioSegments {
                    kind: BorrowedBioBufferKind::Read,
                    ..
                })
        ) {
            return None;
        }
        let segment = self.segment(index)?;
        // Safety: Read BIO 独占该段，且 slice 只在闭包调用期间存活。
        Some(visit(unsafe { segment.as_mut_slice() }))
    }

    pub(crate) fn segments_aligned(&self, align: usize) -> bool {
        (0..self.segment_count()).all(|index| {
            self.segment(index)
                .is_some_and(|segment| segment.vaddr().is_multiple_of(align))
        })
    }

    /// 把一个连续源缓冲区 scatter 到 BIO 的所有数据段。
    #[kernel_symbols::export(
        name = "general.dev.bio.BioBuffer.copy_from_contiguous",
        contract = "kernel.general.block-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn copy_from_contiguous(&mut self, src: &[u8]) -> bool {
        if src.len() != self.len() {
            return false;
        }
        match self {
            BioBuffer::Owned(dst) => dst.copy_from_slice(src),
            BioBuffer::Borrowed(dst) if dst.kind == BorrowedBioBufferKind::Read => {
                dst.as_mut_slice().copy_from_slice(src)
            }
            BioBuffer::BorrowedSegments(dst) if dst.kind == BorrowedBioBufferKind::Read => {
                let mut copied = 0usize;
                for index in 0..dst.segment_count() {
                    let segment = dst.segments[index];
                    let end = copied + segment.len();
                    // Safety: Read 分段由同步 BIO 独占，且本循环依次访问互不重叠的段。
                    unsafe { segment.as_mut_slice() }.copy_from_slice(&src[copied..end]);
                    copied = end;
                }
            }
            _ => return false,
        }
        true
    }

    /// 把 BIO 的所有数据段 gather 到一个连续目标缓冲区。
    #[kernel_symbols::export(
        name = "general.dev.bio.BioBuffer.copy_to_contiguous",
        contract = "kernel.general.block-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER
    )]
    pub fn copy_to_contiguous(&self, dst: &mut [u8]) -> bool {
        if dst.len() != self.len() {
            return false;
        }
        match self {
            BioBuffer::Owned(src) => dst.copy_from_slice(src),
            BioBuffer::Borrowed(src) if src.kind == BorrowedBioBufferKind::Write => {
                dst.copy_from_slice(src.as_slice())
            }
            BioBuffer::BorrowedSegments(src) if src.kind == BorrowedBioBufferKind::Write => {
                let mut copied = 0usize;
                for index in 0..src.segment_count() {
                    let segment = src.segments[index];
                    let end = copied + segment.len();
                    // Safety: Write 分段在同步 BIO 完成前保持有效，本循环只读。
                    dst[copied..end].copy_from_slice(unsafe { segment.as_slice() });
                    copied = end;
                }
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn accepts_op(&self, op: BioOp) -> bool {
        match self {
            BioBuffer::Borrowed(b) => matches!(
                (b.kind, op),
                (BorrowedBioBufferKind::Read, BioOp::Read)
                    | (BorrowedBioBufferKind::Write, BioOp::Write)
            ),
            BioBuffer::BorrowedSegments(b) => matches!(
                (b.kind, op),
                (BorrowedBioBufferKind::Read, BioOp::Read)
                    | (BorrowedBioBufferKind::Write, BioOp::Write)
            ),
            _ => true,
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

type BioCompletionSlot = Arc<Completion<BioResult>>;

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
    completion: Option<BioCompletionSlot>,
    #[cfg(feature = "performance-profile")]
    profile_span_id: u64,
}

#[kernel_symbols::export]
impl Bio {
    /// 创建新 Bio 与配套 Completion。
    pub fn new(
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
        block_size: NonZeroU32,
    ) -> (Self, Arc<Completion<BioResult>>) {
        Self::new_shared_with_observer(op, range, buffer, block_size, 0, None)
    }

    /// 创建带共享完成槽位的新 Bio。
    pub fn new_shared_with_observer(
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
        block_size: NonZeroU32,
        submitted_ns: u64,
        observer: Option<Arc<dyn BioCompletionObserver>>,
    ) -> (Self, Arc<Completion<BioResult>>) {
        let completion = Completion::new_with_reason(sched::WaitReason::BlockIo);
        let bio = Self {
            op,
            range,
            buffer,
            block_size,
            fua: false,
            submitted_ns,
            observer,
            completion: Some(Arc::clone(&completion)),
            #[cfg(feature = "performance-profile")]
            profile_span_id: profiling::current_span_id(),
        };
        (bio, completion)
    }

    /// 创建带外部共享完成槽位的新 Bio。
    pub fn new_with_completion_observer(
        op: BioOp,
        range: BlockRange,
        buffer: BioBuffer,
        block_size: NonZeroU32,
        submitted_ns: u64,
        observer: Option<Arc<dyn BioCompletionObserver>>,
        completion: Arc<Completion<BioResult>>,
    ) -> Self {
        Self {
            op,
            range,
            buffer,
            block_size,
            fua: false,
            submitted_ns,
            observer,
            completion: Some(completion),
            #[cfg(feature = "performance-profile")]
            profile_span_id: profiling::current_span_id(),
        }
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
    #[kernel_symbols::export(
        name = "general.dev.bio.Bio.complete",
        contract = "kernel.general.block-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn complete(mut self, result: Result<(), BioIoError>) {
        #[cfg(feature = "performance-profile")]
        profiling::record_with_trace_args_and_span(
            profiling::Event::BlockComplete,
            0,
            0,
            0,
            self.profile_span_id,
            self.range.lba,
            ((self.op as u64) << 32) | u64::from(self.range.blocks),
        );
        if let Some(observer) = self.observer.as_ref() {
            observer.on_complete(
                self.op,
                self.range,
                self.block_size,
                self.submitted_ns,
                result,
            );
        }
        let Some(completion) = self.completion.take() else {
            return;
        };
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

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{BIO_MAX_BORROWED_SEGMENTS, BioBuffer, BioReqError};

    const TEST_PAGE_SIZE: usize = 4096;

    #[test]
    fn borrowed_segments_fill_sixteen_pages_without_spilling() {
        let mut pages = [[0u8; TEST_PAGE_SIZE]; BIO_MAX_BORROWED_SEGMENTS];
        let mut refs = pages
            .iter_mut()
            .map(|page| &mut page[..])
            .collect::<Vec<_>>();
        let mut buffer = BioBuffer::borrowed_read_vectored(refs.as_mut_slice()).unwrap();
        assert_eq!(buffer.segment_count(), BIO_MAX_BORROWED_SEGMENTS);
        assert_eq!(buffer.len(), BIO_MAX_BORROWED_SEGMENTS * TEST_PAGE_SIZE);

        let source = (0..buffer.len())
            .map(|index| index.wrapping_mul(17) as u8)
            .collect::<Vec<_>>();
        assert!(buffer.copy_from_contiguous(&source));
        drop(buffer);
        drop(refs);

        for (index, page) in pages.iter().enumerate() {
            let start = index * TEST_PAGE_SIZE;
            assert_eq!(page, &source[start..start + TEST_PAGE_SIZE]);
        }
    }

    #[test]
    fn borrowed_segments_reject_seventeen_pages() {
        let mut pages = [[0u8; TEST_PAGE_SIZE]; BIO_MAX_BORROWED_SEGMENTS + 1];
        let mut refs = pages
            .iter_mut()
            .map(|page| &mut page[..])
            .collect::<Vec<_>>();
        assert!(matches!(
            BioBuffer::borrowed_read_vectored(refs.as_mut_slice()),
            Err(BioReqError::TooLarge)
        ));
    }
}
