//! 把异步 [`BlockDevice`] 包成同步 [`fatfs::BlockBackend`] / [`extfs::BlockBackend`]。
//!
//! 文件系统驱动需要按扇区同步读写;块设备核心暴露的是 submit + 完成回调。
//! 本适配器在一次调用里:
//! 1. 分配 `Box<[u8]>` 作为 DMA 缓冲区;
//! 2. 构造 [`BlockIoRequest`] 提交;
//! 3. 自旋 `dev.poll()` 直到完成回调触发;
//! 4. 拷贝结果回用户 slice,或把用户数据搬进 Box 再提交写。
//!
//! 适用于早期启动 / bench 场景 —— 一次只有一条请求,不与其它线程竞争。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};

use vfs::sync::Spinlock;

use crate::dev::block::{
    BlockDevice, BlockIoCompletion, BlockIoError, BlockIoRequest, BlockRange, BlockSubmitError,
};

const STATE_PENDING: u8 = 0;
const STATE_OK: u8 = 1;
const STATE_ERR: u8 = 2;

struct SyncSlot {
    state: AtomicU8,
    /// 写路径时不使用;读路径在完成回调里回填 `Box<[u8]>` 数据。
    buffer: Spinlock<Option<Box<[u8]>>>,
}

impl SyncSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(STATE_PENDING),
            buffer: Spinlock::new(None),
        })
    }
}

/// 把 [`BlockDevice`] 的异步 `submit` 封装成一次同步操作。
///
/// 内部自旋 `dev.poll()` 直到完成回调运行。
pub struct SyncBlockBackend {
    dev: Arc<BlockDevice>,
}

impl SyncBlockBackend {
    pub fn new(dev: Arc<BlockDevice>) -> Self {
        Self { dev }
    }

    fn run<R>(
        &self,
        build_req: impl FnOnce() -> BlockIoRequest,
        on_done: impl FnOnce(&mut Option<Box<[u8]>>) -> R,
    ) -> Result<R, SyncIoError> {
        let slot = SyncSlot::new();
        let slot_for_cb = Arc::clone(&slot);
        let completion: Box<dyn FnOnce(BlockIoCompletion) + Send> =
            Box::new(move |done: BlockIoCompletion| {
                match done.result {
                    Ok(()) => {
                        // 读请求才会需要把 buffer 交回来
                        if let BlockIoRequest::Read { buffer, .. } = done.request {
                            *slot_for_cb.buffer.lock() = Some(buffer);
                        }
                        slot_for_cb.state.store(STATE_OK, Ordering::Release);
                    }
                    Err(_) => {
                        slot_for_cb.state.store(STATE_ERR, Ordering::Release);
                    }
                }
            });
        if let Err((err, _req, _cb)) = self.dev.submit(build_req(), completion) {
            return Err(SyncIoError::Submit(err));
        }
        // 自旋等完成。生产代码里会让出 CPU,这里 bench 初期 OK。
        loop {
            let s = slot.state.load(Ordering::Acquire);
            if s == STATE_OK {
                break;
            }
            if s == STATE_ERR {
                return Err(SyncIoError::Io(BlockIoError::MediaError));
            }
            self.dev.poll();
            core::hint::spin_loop();
        }
        let mut guard = slot.buffer.lock();
        Ok(on_done(&mut guard))
    }

    pub fn sector_size_bytes(&self) -> u32 {
        self.dev.geometry().logical_block_size().get()
    }

    pub fn sector_count_total(&self) -> u64 {
        self.dev.geometry().block_count().unwrap_or(0)
    }

    /// 同步读若干扇区到 `buf`。
    ///
    /// 优先使用 `read_sectors_sync` 零拷贝快速路径（无 Box 分配、无回调、无 spin-wait）。
    /// 仅当驱动不支持时回退到 submit/completion 慢路径。
    pub fn read(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), SyncIoError> {
        let bps = self.sector_size_bytes() as u64;
        let want = (count as u64 * bps) as usize;
        if buf.len() < want {
            return Err(SyncIoError::BufferTooSmall);
        }
        if count == 0 {
            return Err(SyncIoError::InvalidRange);
        }

        // 快速路径 1：直接传 buffer 给驱动，零分配零拷贝。
        match self.dev.read_sync(lba, count, buf) {
            Ok(()) => return Ok(()),
            Err(BlockSubmitError::Unsupported) => {}
            Err(_) => return Err(SyncIoError::Io(BlockIoError::MediaError)),
        }

        // 慢路径：Box 分配 + submit/completion
        let range = match self.build_range(lba, count) {
            Some(r) => r,
            None => return Err(SyncIoError::InvalidRange),
        };
        let backing: Box<[u8]> = alloc::vec![0u8; want].into_boxed_slice();
        let copied = self.run(
            || BlockIoRequest::Read {
                range,
                buffer: backing,
            },
            |done_buf| {
                if let Some(data) = done_buf.take() {
                    buf[..want].copy_from_slice(&data[..want]);
                    true
                } else {
                    false
                }
            },
        )?;
        if copied {
            Ok(())
        } else {
            Err(SyncIoError::Io(BlockIoError::MediaError))
        }
    }

    /// 同步写若干扇区。
    ///
    /// 优先使用 `write_sectors_sync` 零拷贝快速路径。
    pub fn write(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), SyncIoError> {
        let bps = self.sector_size_bytes() as u64;
        let want = (count as u64 * bps) as usize;
        if buf.len() < want {
            return Err(SyncIoError::BufferTooSmall);
        }
        if count == 0 {
            return Err(SyncIoError::InvalidRange);
        }

        // 快速路径 1
        match self.dev.write_sync(lba, count, buf) {
            Ok(()) => return Ok(()),
            Err(BlockSubmitError::Unsupported) => {}
            Err(_) => return Err(SyncIoError::Io(BlockIoError::MediaError)),
        }

        // 慢路径
        let range = match self.build_range(lba, count) {
            Some(r) => r,
            None => return Err(SyncIoError::InvalidRange),
        };
        let mut backing: Box<[u8]> = alloc::vec![0u8; want].into_boxed_slice();
        backing.copy_from_slice(&buf[..want]);
        self.run(
            || BlockIoRequest::Write {
                range,
                buffer: backing,
                fua: false,
            },
            |_| (),
        )?;
        Ok(())
    }

    fn build_range(&self, lba: u64, count: u32) -> Option<BlockRange> {
        if count == 0 {
            return None;
        }
        Some(BlockRange { lba, blocks: count })
    }
}

/// 适配器内部错误。
#[derive(Debug, Clone, Copy)]
pub enum SyncIoError {
    Submit(BlockSubmitError),
    Io(BlockIoError),
    BufferTooSmall,
    InvalidRange,
}

// ── FS-facing impls ──────────────────────────────────────────────────────

impl fatfs::BlockBackend for SyncBlockBackend {
    fn sector_size(&self) -> u32 {
        self.sector_size_bytes()
    }
    fn sector_count(&self) -> u64 {
        self.sector_count_total()
    }
    fn read_sectors(
        &self,
        lba: u64,
        count: u32,
        buf: &mut [u8],
    ) -> Result<(), fatfs::BlockBackendError> {
        self.read(lba, count, buf).map_err(map_err_fat)
    }
    fn write_sectors(
        &self,
        lba: u64,
        count: u32,
        buf: &[u8],
    ) -> Result<(), fatfs::BlockBackendError> {
        self.write(lba, count, buf).map_err(map_err_fat)
    }
}

impl extfs::BlockBackend for SyncBlockBackend {
    fn sector_size(&self) -> u32 {
        self.sector_size_bytes()
    }
    fn sector_count(&self) -> u64 {
        self.sector_count_total()
    }
    fn read_sectors(
        &self,
        lba: u64,
        count: u32,
        buf: &mut [u8],
    ) -> Result<(), extfs::BlockBackendError> {
        self.read(lba, count, buf).map_err(map_err_ext)
    }
    fn write_sectors(
        &self,
        lba: u64,
        count: u32,
        buf: &[u8],
    ) -> Result<(), extfs::BlockBackendError> {
        self.write(lba, count, buf).map_err(map_err_ext)
    }
}

fn map_err_fat(e: SyncIoError) -> fatfs::BlockBackendError {
    match e {
        SyncIoError::BufferTooSmall | SyncIoError::InvalidRange => {
            fatfs::BlockBackendError::OutOfRange
        }
        SyncIoError::Submit(_) | SyncIoError::Io(_) => fatfs::BlockBackendError::Io,
    }
}

fn map_err_ext(e: SyncIoError) -> extfs::BlockBackendError {
    match e {
        SyncIoError::BufferTooSmall | SyncIoError::InvalidRange => {
            extfs::BlockBackendError::OutOfRange
        }
        SyncIoError::Submit(_) | SyncIoError::Io(_) => extfs::BlockBackendError::Io,
    }
}
