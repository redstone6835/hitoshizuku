//! 块设备到文件系统后端的同步适配器。
//!
//! `FsBlockAdapter` 把 [`BlockDevice`] 的 Bio-based API 桥接为
//! `fatfs::BlockBackend` / `extfs::BlockBackend` 要求的同步 `read_sectors` /
//! `write_sectors` 接口。内部使用块层同步借用缓冲提交，避免每次文件系统 I/O
//! 都分配 staging buffer 并做额外 memcpy；等待策略仍由 [`BlockDevice`] 统一维护。

use alloc::sync::Arc;

use crate::dev::bio::{BioError, BlockRange};
use crate::dev::block::BlockDevice;
use crate::dev::control::{BlockControlRequest, BlockControlResponse, ControlError};

/// 把 [`BlockDevice`] 适配为文件系统同步 BlockBackend。
pub struct FsBlockAdapter {
    dev: Arc<BlockDevice>,
    sector_size: u32,
    sector_count: u64,
}

/// 兼容别名。
pub type SyncBlockBackend = FsBlockAdapter;

/// 兼容错误类型。
#[derive(Debug, Clone, Copy)]
pub enum SyncIoError {
    Submit(crate::dev::bio::SubmitError),
    Io(crate::dev::bio::BioIoError),
    BufferTooSmall,
    InvalidRange,
    UnknownCapacity,
    OutOfMemory,
}

impl From<BioError> for SyncIoError {
    fn from(e: BioError) -> Self {
        match e {
            BioError::Submit(s) => SyncIoError::Submit(s),
            BioError::Io(i) => SyncIoError::Io(i),
        }
    }
}

impl FsBlockAdapter {
    pub fn new(dev: Arc<BlockDevice>) -> Result<Self, SyncIoError> {
        let sector_size = block_sector_size(&dev)?;
        let capacity_bytes = block_capacity_bytes(&dev)?;
        let sector_count = capacity_bytes / sector_size as u64;
        if sector_count == 0 {
            return Err(SyncIoError::UnknownCapacity);
        }
        // 文件系统 BlockBackend trait 只能返回 u64 容量，没有“不知道”的表达；
        // 构造期显式失败，避免把未知容量硬投影成 0 扇区空盘。
        Ok(Self {
            dev,
            sector_size,
            sector_count,
        })
    }

    pub fn sector_size_bytes(&self) -> u32 {
        self.sector_size
    }

    pub fn sector_count_total(&self) -> u64 {
        self.sector_count
    }

    pub fn read(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), SyncIoError> {
        let bps = self.sector_size_bytes() as usize;
        let want = checked_io_len(count, bps).ok_or(SyncIoError::InvalidRange)?;
        if buf.len() < want {
            return Err(SyncIoError::BufferTooSmall);
        }
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait_borrowed_read(range, &mut buf[..want])
            .map_err(SyncIoError::from)?;
        Ok(())
    }

    pub fn write(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), SyncIoError> {
        let bps = self.sector_size_bytes() as usize;
        let want = checked_io_len(count, bps).ok_or(SyncIoError::InvalidRange)?;
        if buf.len() < want {
            return Err(SyncIoError::BufferTooSmall);
        }
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait_borrowed_write(range, &buf[..want])
            .map_err(SyncIoError::from)?;
        Ok(())
    }
}

fn block_sector_size(dev: &Arc<BlockDevice>) -> Result<u32, SyncIoError> {
    match dev
        .control(BlockControlRequest::GetLogicalBlockSize)
        .map_err(map_control_sync_error)?
    {
        BlockControlResponse::U32(size) if size != 0 && size.is_power_of_two() => Ok(size),
        _ => Err(SyncIoError::UnknownCapacity),
    }
}

fn block_capacity_bytes(dev: &Arc<BlockDevice>) -> Result<u64, SyncIoError> {
    match dev
        .control(BlockControlRequest::GetCapacityBytes)
        .map_err(map_control_sync_error)?
    {
        BlockControlResponse::U64(bytes) => Ok(bytes),
        _ => Err(SyncIoError::UnknownCapacity),
    }
}

fn map_control_sync_error(err: ControlError) -> SyncIoError {
    match err {
        ControlError::Unsupported | ControlError::Invalid | ControlError::NoDevice => {
            SyncIoError::UnknownCapacity
        }
        ControlError::Busy => SyncIoError::InvalidRange,
        ControlError::Io => SyncIoError::Io(crate::dev::bio::BioIoError::MediaError),
        ControlError::Permission => SyncIoError::Submit(crate::dev::bio::SubmitError::ReadOnly),
    }
}

fn checked_io_len(count: u32, bytes_per_sector: usize) -> Option<usize> {
    // 用户态/文件系统传入的 sector count 不能直接乘 sector size；
    // 溢出时应视为越界 I/O，而不是在 debug 下 panic 或 release 下回绕。
    (count as usize).checked_mul(bytes_per_sector)
}

impl fatfs::BlockBackend for FsBlockAdapter {
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
        let bps = self.sector_size_bytes() as usize;
        let want = checked_io_len(count, bps).ok_or(fatfs::BlockBackendError::OutOfRange)?;
        if buf.len() < want {
            return Err(fatfs::BlockBackendError::OutOfRange);
        }
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait_borrowed_read(range, &mut buf[..want])
            .map_err(|_| fatfs::BlockBackendError::Io)?;
        Ok(())
    }

    fn write_sectors(
        &self,
        lba: u64,
        count: u32,
        buf: &[u8],
    ) -> Result<(), fatfs::BlockBackendError> {
        let bps = self.sector_size_bytes() as usize;
        let want = checked_io_len(count, bps).ok_or(fatfs::BlockBackendError::OutOfRange)?;
        if buf.len() < want {
            return Err(fatfs::BlockBackendError::OutOfRange);
        }
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait_borrowed_write(range, &buf[..want])
            .map_err(|_| fatfs::BlockBackendError::Io)?;
        Ok(())
    }
}

impl extfs::BlockBackend for FsBlockAdapter {
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
        let bps = self.sector_size_bytes() as usize;
        let want = checked_io_len(count, bps).ok_or(extfs::BlockBackendError::OutOfRange)?;
        if buf.len() < want {
            return Err(extfs::BlockBackendError::OutOfRange);
        }
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait_borrowed_read(range, &mut buf[..want])
            .map_err(|_| extfs::BlockBackendError::Io)?;
        Ok(())
    }

    fn write_sectors(
        &self,
        lba: u64,
        count: u32,
        buf: &[u8],
    ) -> Result<(), extfs::BlockBackendError> {
        let bps = self.sector_size_bytes() as usize;
        let want = checked_io_len(count, bps).ok_or(extfs::BlockBackendError::OutOfRange)?;
        if buf.len() < want {
            return Err(extfs::BlockBackendError::OutOfRange);
        }
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait_borrowed_write(range, &buf[..want])
            .map_err(|_| extfs::BlockBackendError::Io)?;
        Ok(())
    }
}
