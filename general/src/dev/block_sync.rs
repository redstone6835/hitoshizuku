//! 块设备到文件系统后端的同步适配器。
//!
//! `FsBlockAdapter` 把 [`BlockDevice`] 的 Bio-based API 桥接为
//! `fatfs::BlockBackend` / `extfs::BlockBackend` 要求的同步 `read_sectors` /
//! `write_sectors` 接口。内部调用 [`BlockDevice::submit_bio_wait`]，自动根据
//! 调度器状态选择 WaitQueue 阻塞或自旋等待。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::dev::bio::{BioBuffer, BioError, BioOp, BlockRange};
use crate::dev::block::BlockDevice;

/// 把 [`BlockDevice`] 适配为文件系统同步 BlockBackend。
pub struct FsBlockAdapter {
    dev: Arc<BlockDevice>,
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
        let sector_count = dev
            .geometry()
            .block_count()
            .ok_or(SyncIoError::UnknownCapacity)?;
        // 文件系统 BlockBackend trait 只能返回 u64 容量，没有“不知道”的表达；
        // 构造期显式失败，避免把未知容量硬投影成 0 扇区空盘。
        Ok(Self { dev, sector_count })
    }

    pub fn sector_size_bytes(&self) -> u32 {
        self.dev.geometry().logical_block_size().get()
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
        let owned = zeroed_io_buffer(want)?;
        let range = BlockRange { lba, blocks: count };
        let bio = self
            .dev
            .submit_bio_wait(BioOp::Read, range, BioBuffer::Owned(owned))
            .map_err(SyncIoError::from)?;
        buf[..want].copy_from_slice(bio.buffer.as_slice());
        Ok(())
    }

    pub fn write(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), SyncIoError> {
        let bps = self.sector_size_bytes() as usize;
        let want = checked_io_len(count, bps).ok_or(SyncIoError::InvalidRange)?;
        if buf.len() < want {
            return Err(SyncIoError::BufferTooSmall);
        }
        let mut owned = zeroed_io_buffer(want)?;
        owned.copy_from_slice(&buf[..want]);
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait(BioOp::Write, range, BioBuffer::Owned(owned))
            .map_err(SyncIoError::from)?;
        Ok(())
    }
}

fn checked_io_len(count: u32, bytes_per_sector: usize) -> Option<usize> {
    // 用户态/文件系统传入的 sector count 不能直接乘 sector size；
    // 溢出时应视为越界 I/O，而不是在 debug 下 panic 或 release 下回绕。
    (count as usize).checked_mul(bytes_per_sector)
}

fn zeroed_io_buffer(len: usize) -> Result<Box<[u8]>, SyncIoError> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| SyncIoError::OutOfMemory)?;
    buf.resize(len, 0);
    Ok(buf.into_boxed_slice())
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
        let owned = zeroed_io_buffer(want).map_err(|_| fatfs::BlockBackendError::Io)?;
        let range = BlockRange { lba, blocks: count };
        let bio = self
            .dev
            .submit_bio_wait(BioOp::Read, range, BioBuffer::Owned(owned))
            .map_err(|_| fatfs::BlockBackendError::Io)?;
        buf[..want].copy_from_slice(bio.buffer.as_slice());
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
        let mut owned = zeroed_io_buffer(want).map_err(|_| fatfs::BlockBackendError::Io)?;
        owned.copy_from_slice(&buf[..want]);
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait(BioOp::Write, range, BioBuffer::Owned(owned))
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
        let owned = zeroed_io_buffer(want).map_err(|_| extfs::BlockBackendError::Io)?;
        let range = BlockRange { lba, blocks: count };
        let bio = self
            .dev
            .submit_bio_wait(BioOp::Read, range, BioBuffer::Owned(owned))
            .map_err(|_| extfs::BlockBackendError::Io)?;
        buf[..want].copy_from_slice(bio.buffer.as_slice());
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
        let mut owned = zeroed_io_buffer(want).map_err(|_| extfs::BlockBackendError::Io)?;
        owned.copy_from_slice(&buf[..want]);
        let range = BlockRange { lba, blocks: count };
        self.dev
            .submit_bio_wait(BioOp::Write, range, BioBuffer::Owned(owned))
            .map_err(|_| extfs::BlockBackendError::Io)?;
        Ok(())
    }
}
